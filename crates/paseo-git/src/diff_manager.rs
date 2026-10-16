//! Pure decision logic of `checkout-diff-manager.ts`: compare normalization,
//! target keys, snapshot payloads, change fingerprints and refresh coalescing.
//! The watcher/debounce/listener wiring belongs to the edge that owns the
//! workspace git service and timers.

use serde::Serialize;
use serde_json::{json, Value};

use crate::diff::{CheckoutDiffCompare, CheckoutDiffResult, DiffMode};
use crate::diff_parse::ParsedDiffFile;
use crate::error::{to_checkout_error, CheckoutErrorPayload, GitError, Result};
use crate::paths::js_str_cmp;

pub const CHECKOUT_DIFF_WATCH_DEBOUNCE_MS: u64 = 150;

/// Trim the base ref and keep only the fields that identify a subscription.
pub fn normalize_compare(compare: &CheckoutDiffCompare) -> CheckoutDiffCompare {
    let ignore_whitespace = Some(compare.ignore_whitespace == Some(true));
    if compare.mode == DiffMode::Uncommitted {
        return CheckoutDiffCompare {
            mode: DiffMode::Uncommitted,
            base_ref: None,
            ignore_whitespace,
            include_structured: None,
        };
    }
    let base_ref = compare
        .base_ref
        .as_deref()
        .map(str::trim)
        .filter(|b| !b.is_empty())
        .map(str::to_string);
    CheckoutDiffCompare {
        mode: DiffMode::Base,
        base_ref,
        ignore_whitespace,
        include_structured: None,
    }
}

pub fn build_target_key(cwd: &str, compare: &CheckoutDiffCompare) -> String {
    json!([
        cwd,
        compare.mode.as_str(),
        if compare.mode == DiffMode::Base {
            compare.base_ref.clone().unwrap_or_default()
        } else {
            String::new()
        },
        compare.ignore_whitespace == Some(true),
    ])
    .to_string()
}

/// `checkout_diff_update` payload minus `subscriptionId`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckoutDiffSnapshotPayload {
    pub cwd: String,
    pub files: Vec<ParsedDiffFile>,
    pub error: Option<CheckoutErrorPayload>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff_too_large: Option<bool>,
}

/// Turn a diff read into the subscription payload: structured files sorted by
/// path, `diffTooLarge` with a typed error, or the checkout error on failure.
pub fn build_diff_snapshot_payload<R: std::borrow::Borrow<CheckoutDiffResult>>(
    cwd: &str,
    result: Result<R>,
) -> CheckoutDiffSnapshotPayload {
    match result {
        Err(e) => CheckoutDiffSnapshotPayload {
            cwd: cwd.into(),
            files: vec![],
            error: Some(to_checkout_error(&e)),
            diff_too_large: None,
        },
        Ok(r) => {
            let r: &CheckoutDiffResult = r.borrow();
            if r.diff_too_large {
                return CheckoutDiffSnapshotPayload {
                    cwd: cwd.into(),
                    files: vec![],
                    error: Some(to_checkout_error(&GitError::other(
                        "Diff too large to display",
                    ))),
                    diff_too_large: Some(true),
                };
            }
            let mut files = r.structured.clone().unwrap_or_default();
            files.sort_by(|a, b| js_str_cmp(&a.path, &b.path));
            CheckoutDiffSnapshotPayload {
                cwd: cwd.into(),
                files,
                error: None,
                diff_too_large: None,
            }
        }
    }
}

pub fn snapshot_fingerprint(payload: &CheckoutDiffSnapshotPayload) -> String {
    serde_json::to_string(payload).unwrap_or_default()
}

impl CheckoutDiffSnapshotPayload {
    fn into_value(self, extra: Value) -> Value {
        let mut v = serde_json::to_value(self).unwrap_or(Value::Null);
        if let (Value::Object(base), Value::Object(extra)) = (&mut v, extra) {
            base.extend(extra);
        }
        v
    }

    pub fn into_subscribe_response(self, subscription_id: &str, request_id: &str) -> Value {
        json!({"type": "subscribe_checkout_diff_response", "payload": self.into_value(json!({"subscriptionId": subscription_id, "requestId": request_id}))})
    }

    pub fn into_diff_get_response(self, request_id: &str) -> Value {
        json!({"type": "checkout.diff.get.response", "payload": self.into_value(json!({"requestId": request_id}))})
    }

    pub fn into_update(self, subscription_id: &str) -> Value {
        json!({"type": "checkout_diff_update", "payload": self.into_value(json!({"subscriptionId": subscription_id}))})
    }
}

/// Emits a snapshot only when it differs from the last one sent.
#[derive(Debug, Default)]
pub struct EmitGate {
    latest: Option<String>,
}

impl EmitGate {
    pub fn remember_initial(&mut self, payload: &CheckoutDiffSnapshotPayload) {
        self.latest = Some(snapshot_fingerprint(payload));
    }

    /// True when `payload` changed since the last remembered/emitted snapshot.
    pub fn should_emit(&mut self, payload: &CheckoutDiffSnapshotPayload) -> bool {
        let fingerprint = snapshot_fingerprint(payload);
        if self.latest.as_deref() == Some(fingerprint.as_str()) {
            return false;
        }
        self.latest = Some(fingerprint);
        true
    }
}

/// Coalesces refreshes: one runs at a time and an edit during a run produces
/// exactly one follow-up, forced if any queued request was forced.
#[derive(Debug, Default)]
pub struct RefreshCoalescer {
    running: bool,
    queued: bool,
    queued_force: bool,
}

impl RefreshCoalescer {
    /// `Some(force)` when the caller should start a refresh now; `None` when it
    /// was queued behind the active one.
    pub fn request(&mut self, force: bool) -> Option<bool> {
        if self.running {
            self.queued = true;
            self.queued_force |= force;
            return None;
        }
        self.running = true;
        Some(force)
    }

    /// Call when a refresh finished. `Some(force)` starts the queued follow-up.
    pub fn finish(&mut self) -> Option<bool> {
        if self.queued {
            let force = self.queued_force;
            self.queued = false;
            self.queued_force = false;
            return Some(force);
        }
        self.running = false;
        None
    }
}

/// Decides when a base-compare subscription must refresh in response to
/// workspace snapshot updates: structural changes always do; worktree-only or
/// forge-only changes do not.
#[derive(Debug, Default)]
pub struct WorkspaceSnapshotWatch {
    structure: Option<String>,
    worktree: Option<String>,
    forge: Option<String>,
}

impl WorkspaceSnapshotWatch {
    /// Fingerprints are JSON strings of the relevant snapshot sections. Returns
    /// whether to schedule a (non-forced) refresh.
    pub fn remember(&mut self, structure: String, worktree: String, forge: String) -> bool {
        let first = self.structure.is_none();
        let structure_changed = self.structure.as_deref() != Some(structure.as_str());
        let worktree_changed = self.worktree.as_deref() != Some(worktree.as_str());
        let forge_changed = self.forge.as_deref() != Some(forge.as_str());
        self.structure = Some(structure);
        self.worktree = Some(worktree);
        self.forge = Some(forge);
        if first {
            return false;
        }
        structure_changed || (!worktree_changed && !forge_changed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff_parse::DiffFileStatus;

    fn file(path: &str) -> ParsedDiffFile {
        ParsedDiffFile {
            path: path.into(),
            old_path: None,
            is_new: false,
            is_deleted: false,
            additions: 0,
            deletions: 0,
            staged: None,
            unstaged: None,
            hunks: vec![],
            status: Some(DiffFileStatus::Ok),
        }
    }

    #[test]
    fn normalizes_compare_inputs() {
        let c = normalize_compare(&CheckoutDiffCompare {
            mode: DiffMode::Base,
            base_ref: Some("  main ".into()),
            ignore_whitespace: None,
            include_structured: Some(true),
        });
        assert_eq!(
            c,
            CheckoutDiffCompare {
                mode: DiffMode::Base,
                base_ref: Some("main".into()),
                ignore_whitespace: Some(false),
                include_structured: None
            }
        );
        let c = normalize_compare(&CheckoutDiffCompare {
            mode: DiffMode::Base,
            base_ref: Some("  ".into()),
            ignore_whitespace: Some(true),
            include_structured: None,
        });
        assert_eq!(c.base_ref, None);
        let c = normalize_compare(&CheckoutDiffCompare {
            mode: DiffMode::Uncommitted,
            base_ref: Some("x".into()),
            ignore_whitespace: None,
            include_structured: None,
        });
        assert_eq!(c.base_ref, None);
        assert_eq!(c.ignore_whitespace, Some(false));
    }

    #[test]
    fn target_key_ignores_base_ref_for_uncommitted() {
        let a = CheckoutDiffCompare {
            mode: DiffMode::Uncommitted,
            base_ref: Some("x".into()),
            ignore_whitespace: Some(false),
            include_structured: None,
        };
        let b = CheckoutDiffCompare {
            base_ref: None,
            ..a.clone()
        };
        assert_eq!(build_target_key("/r", &a), build_target_key("/r", &b));
        assert_eq!(
            build_target_key("/r", &b),
            r#"["/r","uncommitted","",false]"#
        );
    }

    #[test]
    fn snapshot_payload_sorts_files_and_flags_too_large() {
        let r = CheckoutDiffResult {
            diff: String::new(),
            structured: Some(vec![file("b.txt"), file("a.txt"), file("B.txt")]),
            diff_too_large: false,
        };
        let p = build_diff_snapshot_payload("/r", Ok(std::sync::Arc::new(r)));
        assert_eq!(
            p.files.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(),
            ["B.txt", "a.txt", "b.txt"]
        );
        let v = serde_json::to_value(&p).unwrap();
        assert!(v.get("diffTooLarge").is_none());
        assert_eq!(v["error"], Value::Null);

        let big = CheckoutDiffResult {
            diff: String::new(),
            structured: Some(vec![]),
            diff_too_large: true,
        };
        let v = serde_json::to_value(build_diff_snapshot_payload(
            "/r",
            Ok(std::sync::Arc::new(big)),
        ))
        .unwrap();
        assert_eq!(v["diffTooLarge"], true);
        assert_eq!(
            v["error"],
            json!({"code": "UNKNOWN", "message": "Diff too large to display"})
        );

        let failed = build_diff_snapshot_payload::<std::sync::Arc<CheckoutDiffResult>>(
            "/r",
            Err(GitError::NotGitRepo { cwd: "/r".into() }),
        );
        assert_eq!(
            serde_json::to_value(failed).unwrap()["error"]["code"],
            "NOT_GIT_REPO"
        );
    }

    #[test]
    fn response_envelopes_carry_ids() {
        let p = build_diff_snapshot_payload(
            "/r",
            Ok(std::sync::Arc::new(CheckoutDiffResult {
                diff: String::new(),
                structured: Some(vec![]),
                diff_too_large: false,
            })),
        );
        let v = p.clone().into_subscribe_response("sub", "req");
        assert_eq!(v["type"], "subscribe_checkout_diff_response");
        assert_eq!(v["payload"]["subscriptionId"], "sub");
        assert_eq!(v["payload"]["requestId"], "req");
        let g = p.clone().into_diff_get_response("req");
        assert!(g["payload"].get("subscriptionId").is_none());
        assert_eq!(p.into_update("sub")["type"], "checkout_diff_update");
    }

    #[test]
    fn emit_gate_suppresses_unchanged_snapshots() {
        let mk = |files: Vec<ParsedDiffFile>| CheckoutDiffSnapshotPayload {
            cwd: "/r".into(),
            files,
            error: None,
            diff_too_large: None,
        };
        let mut gate = EmitGate::default();
        gate.remember_initial(&mk(vec![]));
        assert!(!gate.should_emit(&mk(vec![])));
        assert!(gate.should_emit(&mk(vec![file("a")])));
        assert!(!gate.should_emit(&mk(vec![file("a")])));
    }

    #[test]
    fn an_edit_during_a_refresh_produces_one_final_follow_up() {
        let mut c = RefreshCoalescer::default();
        assert_eq!(c.request(true), Some(true));
        assert_eq!(c.request(false), None);
        assert_eq!(c.request(true), None);
        assert_eq!(c.finish(), Some(true));
        assert_eq!(c.finish(), None);
        assert_eq!(c.request(false), Some(false));
    }

    #[test]
    fn base_subscriptions_refresh_only_for_structural_changes() {
        let mut w = WorkspaceSnapshotWatch::default();
        assert!(!w.remember("s1".into(), "w1".into(), "f1".into()));
        // worktree-only update: ignore
        assert!(!w.remember("s1".into(), "w2".into(), "f1".into()));
        // forge-only update: ignore
        assert!(!w.remember("s1".into(), "w2".into(), "f2".into()));
        // structural change: refresh
        assert!(w.remember("s2".into(), "w2".into(), "f2".into()));
        // identical re-delivery: refresh (nothing else explains the notification)
        assert!(w.remember("s2".into(), "w2".into(), "f2".into()));
    }
}
