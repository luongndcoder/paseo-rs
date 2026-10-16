//! Checkout status and its wire projection (`getCheckoutStatus`,
//! `checkout/status-projection.ts`).

use serde_json::{json, Map, Value};

use crate::error::{to_checkout_error, GitError, Result};
use crate::paths::{branch_name_from_ref, node_resolve};
use crate::repo::{
    get_ahead_behind, get_checkout_snapshot_facts, is_working_tree_dirty, AheadBehind,
    CheckoutContext, CheckoutSnapshotFacts,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckoutStatusGit {
    pub repo_root: String,
    pub main_repo_root: Option<String>,
    pub current_branch: Option<String>,
    pub is_dirty: bool,
    pub base_ref: Option<String>,
    pub ahead_behind: Option<AheadBehind>,
    /// Remote-tracking ref for the branch (`refs/remotes/origin/main`); ahead/behind of
    /// origin are measured against exactly this ref.
    pub upstream_ref: Option<String>,
    pub ahead_of_origin: Option<u64>,
    pub behind_of_origin: Option<u64>,
    pub has_remote: bool,
    pub remote_url: Option<String>,
    pub is_paseo_owned_worktree: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckoutStatusResult {
    NotGit,
    Git(Box<CheckoutStatusGit>),
}

pub async fn get_checkout_status(cwd: &str, ctx: &CheckoutContext) -> Result<CheckoutStatusResult> {
    let facts = get_checkout_snapshot_facts(cwd, ctx).await?;
    let CheckoutSnapshotFacts::Git(facts) = facts else {
        return Ok(CheckoutStatusResult::NotGit);
    };
    let is_dirty = is_working_tree_dirty(cwd).await?;
    let has_remote = facts.remote_url.is_some();
    let base_ref = facts.resolved_base_ref.clone();
    let facts_ctx = CheckoutContext {
        facts: Some(CheckoutSnapshotFacts::Git(facts.clone())),
        ..ctx.clone()
    };
    let ahead_behind = match (&base_ref, &facts.current_branch) {
        (Some(base), Some(branch)) => get_ahead_behind(cwd, base, branch, &facts_ctx).await?,
        _ => None,
    };
    // The wire carries the display name; the exact ref stays in worktree.json and facts.
    let display_base_ref = base_ref.as_deref().map(branch_name_from_ref);
    let upstream = facts.upstream_status.as_ref();
    let common = CheckoutStatusGit {
        repo_root: facts.worktree_root.clone(),
        main_repo_root: None,
        current_branch: facts.current_branch.clone(),
        is_dirty,
        base_ref: None,
        ahead_behind,
        upstream_ref: upstream.map(|u| u.reference.clone()),
        ahead_of_origin: upstream.map(|u| u.ahead_behind.ahead),
        behind_of_origin: upstream.map(|u| u.ahead_behind.behind),
        has_remote,
        remote_url: facts.remote_url.clone(),
        is_paseo_owned_worktree: false,
    };

    if facts.paseo_worktree.is_owned() && base_ref.is_some() {
        return Ok(CheckoutStatusResult::Git(Box::new(CheckoutStatusGit {
            main_repo_root: Some(
                facts
                    .main_repo_root
                    .clone()
                    .unwrap_or_else(|| facts.worktree_root.clone()),
            ),
            base_ref: display_base_ref.or(base_ref),
            is_paseo_owned_worktree: true,
            ..common
        })));
    }

    let main_repo_root = facts
        .main_repo_root
        .clone()
        .filter(|m| node_resolve("/", m) != node_resolve("/", &facts.worktree_root));
    Ok(CheckoutStatusResult::Git(Box::new(CheckoutStatusGit {
        main_repo_root,
        base_ref: display_base_ref,
        ..common
    })))
}

fn ahead_behind_json(v: &Option<AheadBehind>) -> Value {
    match v {
        Some(ab) => json!({"ahead": ab.ahead, "behind": ab.behind}),
        None => Value::Null,
    }
}

/// `checkout_status_response` payload (`buildCheckoutStatusPayloadFromSnapshot`).
pub fn build_checkout_status_payload(
    cwd: &str,
    request_id: &str,
    status: &CheckoutStatusResult,
) -> Result<Value> {
    let CheckoutStatusResult::Git(git) = status else {
        return Ok(json!({
            "cwd": cwd, "isGit": false, "repoRoot": null, "currentBranch": null,
            "isDirty": null, "baseRef": null, "aheadBehind": null, "upstreamRef": null,
            "aheadOfOrigin": null, "behindOfOrigin": null, "hasRemote": false,
            "remoteUrl": null, "isPaseoOwnedWorktree": false, "error": null,
            "requestId": request_id,
        }));
    };
    if git.is_paseo_owned_worktree && (git.main_repo_root.is_none() || git.base_ref.is_none()) {
        return Err(GitError::other(
            "Workspace git snapshot is missing required worktree status fields",
        ));
    }
    Ok(json!({
        "cwd": cwd, "isGit": true, "repoRoot": git.repo_root,
        "mainRepoRoot": git.main_repo_root, "currentBranch": git.current_branch,
        "isDirty": git.is_dirty, "baseRef": git.base_ref,
        "aheadBehind": ahead_behind_json(&git.ahead_behind),
        "upstreamRef": git.upstream_ref, "aheadOfOrigin": git.ahead_of_origin,
        "behindOfOrigin": git.behind_of_origin, "hasRemote": git.has_remote,
        "remoteUrl": git.remote_url, "isPaseoOwnedWorktree": git.is_paseo_owned_worktree,
        "error": null, "requestId": request_id,
    }))
}

/// Failure variant: the handler reports a not-git payload carrying the error.
pub fn build_checkout_status_error_payload(cwd: &str, request_id: &str, error: &GitError) -> Value {
    json!({
        "cwd": cwd, "isGit": false, "repoRoot": null, "currentBranch": null,
        "isDirty": null, "baseRef": null, "aheadBehind": null, "aheadOfOrigin": null,
        "behindOfOrigin": null, "hasRemote": false, "remoteUrl": null,
        "isPaseoOwnedWorktree": false, "error": to_checkout_error(error),
        "requestId": request_id,
    })
}

fn str_or_null<'a>(status: &'a Value, key: &str) -> &'a Value {
    status.get(key).unwrap_or(&Value::Null)
}

fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

/// Project a forge pull-request status onto the `checkout_pr_status_response`
/// wire shape (`normalizeCheckoutPrStatusPayload`). The input is the forge
/// service's JSON status; `forgeSpecific` GitHub facts are mirrored onto
/// `github` for clients that predate the envelope.
pub fn normalize_checkout_pr_status_payload(status: Option<&Value>, forge: Option<&str>) -> Value {
    let Some(status) = status.filter(|s| !s.is_null()) else {
        return Value::Null;
    };
    let mut p = Map::new();
    if let Some(f) = forge.filter(|f| !f.is_empty()) {
        p.insert("forge".into(), json!(f));
    }
    for key in [
        "number",
        "url",
        "title",
        "state",
        "repoOwner",
        "repoName",
        "baseRefName",
        "headRefName",
        "isMerged",
    ] {
        p.insert(key.into(), str_or_null(status, key).clone());
    }
    p.insert(
        "isDraft".into(),
        json!(status
            .get("isDraft")
            .and_then(Value::as_bool)
            .unwrap_or(false)),
    );
    p.insert(
        "mergeable".into(),
        match status.get("mergeable") {
            Some(v) if !v.is_null() => v.clone(),
            _ => json!("UNKNOWN"),
        },
    );
    p.insert(
        "checks".into(),
        match status.get("checks") {
            Some(v) if !v.is_null() => v.clone(),
            _ => json!([]),
        },
    );
    p.insert(
        "checksStatus".into(),
        str_or_null(status, "checksStatus").clone(),
    );
    p.insert(
        "reviewDecision".into(),
        str_or_null(status, "reviewDecision").clone(),
    );
    if truthy(status.get("projectPath")) {
        p.insert("projectPath".into(), status["projectPath"].clone());
    } else if truthy(status.get("repoOwner")) && truthy(status.get("repoName")) {
        let owner = status["repoOwner"].as_str().unwrap_or_default();
        let name = status["repoName"].as_str().unwrap_or_default();
        p.insert("projectPath".into(), json!(format!("{owner}/{name}")));
    }
    if truthy(status.get("forgeSpecific")) {
        let facts = status["forgeSpecific"].clone();
        // COMPAT(forgeSpecific): mirror GitHub facts onto `github` until the client floor is >= v0.2.0.
        if facts.get("forge").and_then(Value::as_str) == Some("github") {
            let mut github = facts.as_object().cloned().unwrap_or_default();
            github.remove("forge");
            p.insert("forgeSpecific".into(), facts);
            p.insert("github".into(), Value::Object(github));
        } else {
            p.insert("forgeSpecific".into(), facts);
        }
    }
    Value::Object(p)
}

/// `checkout_pr_status_response` payload from a resolved forge snapshot.
pub fn build_checkout_pr_status_payload(
    cwd: &str,
    request_id: &str,
    forge: Option<&str>,
    pull_request: Option<&Value>,
    features_enabled: bool,
    auth_state: &Value,
    error_message: Option<&str>,
) -> Value {
    let mut payload = Map::new();
    payload.insert("cwd".into(), json!(cwd));
    payload.insert(
        "status".into(),
        normalize_checkout_pr_status_payload(pull_request, forge),
    );
    payload.insert("githubFeaturesEnabled".into(), json!(features_enabled));
    payload.insert("authState".into(), auth_state.clone());
    if let Some(f) = forge.filter(|f| !f.is_empty()) {
        payload.insert("forge".into(), json!(f));
    }
    payload.insert(
        "error".into(),
        match error_message {
            Some(m) => json!({"code": "UNKNOWN", "message": m}),
            None => Value::Null,
        },
    );
    payload.insert("requestId".into(), json!(request_id));
    Value::Object(payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pr_status_includes_identity_and_defaults() {
        let status = json!({
            "number": 123, "repoOwner": "internal-owner", "repoName": "internal-repo",
            "url": "https://github.com/getpaseo/paseo/pull/123", "title": "Ship PR pane",
            "state": "open", "baseRefName": "main", "headRefName": "feature/pr-pane",
            "isMerged": false, "isDraft": true, "mergeable": "MERGEABLE",
            "checks": [], "checksStatus": "success", "reviewDecision": "approved",
        });
        let p = normalize_checkout_pr_status_payload(Some(&status), Some("github"));
        assert_eq!(p["repoOwner"], "internal-owner");
        assert_eq!(p["forge"], "github");
        assert_eq!(p["projectPath"], "internal-owner/internal-repo");
        assert_eq!(p["mergeable"], "MERGEABLE");

        let bare = json!({"number": 1, "state": "open", "repoOwner": null});
        let p = normalize_checkout_pr_status_payload(Some(&bare), None);
        assert_eq!(p["isDraft"], false);
        assert_eq!(p["mergeable"], "UNKNOWN");
        assert_eq!(p["checks"], json!([]));
        assert!(p.get("forge").is_none() && p.get("projectPath").is_none());
        assert!(normalize_checkout_pr_status_payload(None, Some("github")).is_null());
    }

    #[test]
    fn github_facts_are_mirrored_and_other_forges_are_not() {
        let gh = json!({"number": 993, "forgeSpecific": {"forge": "github", "mergeStateStatus": "BLOCKED", "viewerCanEnableAutoMerge": true}});
        let p = normalize_checkout_pr_status_payload(Some(&gh), Some("github"));
        assert_eq!(p["forgeSpecific"]["forge"], "github");
        assert_eq!(p["github"]["mergeStateStatus"], "BLOCKED");
        assert!(p["github"].get("forge").is_none());

        let gl = json!({"number": 7, "projectPath": "group/proj", "forgeSpecific": {"forge": "gitlab", "x": 1}});
        let p = normalize_checkout_pr_status_payload(Some(&gl), Some("gitlab"));
        assert_eq!(p["projectPath"], "group/proj");
        assert_eq!(p["forgeSpecific"]["forge"], "gitlab");
        assert!(p.get("github").is_none());

        // Gitea family emits a `gitea` facts tag while the top-level forge keeps the brand.
        let fj = json!({"number": 9, "forgeSpecific": {"forge": "gitea"}});
        let p = normalize_checkout_pr_status_payload(Some(&fj), Some("forgejo"));
        assert_eq!(p["forge"], "forgejo");
        assert_eq!(p["forgeSpecific"]["forge"], "gitea");
    }

    #[test]
    fn unresolved_forge_stays_unlabeled() {
        let status = json!({"number": 1, "forgeSpecific": {"forge": "gitea"}});
        let payload = build_checkout_pr_status_payload(
            "/repo",
            "r1",
            None,
            Some(&status),
            true,
            &json!({"state": "unauthenticated"}),
            None,
        );
        assert!(payload.get("forge").is_none());
        assert!(payload["status"].get("forge").is_none());
        assert_eq!(payload["error"], Value::Null);
        let errored = build_checkout_pr_status_payload(
            "/repo",
            "r1",
            Some("gitlab"),
            None,
            false,
            &json!({}),
            Some("boom"),
        );
        assert_eq!(errored["forge"], "gitlab");
        assert_eq!(
            errored["error"],
            json!({"code": "UNKNOWN", "message": "boom"})
        );
        assert_eq!(errored["status"], Value::Null);
    }

    #[test]
    fn status_payload_shapes() {
        let not_git =
            build_checkout_status_payload("/x", "r", &CheckoutStatusResult::NotGit).unwrap();
        assert_eq!(not_git["isGit"], false);
        assert!(not_git.get("mainRepoRoot").is_none());
        assert!(not_git.get("aheadBehind").is_some_and(Value::is_null));

        let git = CheckoutStatusGit {
            repo_root: "/r".into(),
            main_repo_root: None,
            current_branch: Some("f".into()),
            is_dirty: false,
            base_ref: Some("main".into()),
            ahead_behind: Some(AheadBehind {
                ahead: 1,
                behind: 2,
            }),
            upstream_ref: None,
            ahead_of_origin: None,
            behind_of_origin: None,
            has_remote: false,
            remote_url: None,
            is_paseo_owned_worktree: false,
        };
        let v = build_checkout_status_payload(
            "/r",
            "r",
            &CheckoutStatusResult::Git(Box::new(git.clone())),
        )
        .unwrap();
        assert_eq!(v["aheadBehind"], json!({"ahead": 1, "behind": 2}));
        assert_eq!(v["mainRepoRoot"], Value::Null);
        let mut paseo = git;
        paseo.is_paseo_owned_worktree = true;
        assert!(build_checkout_status_payload(
            "/r",
            "r",
            &CheckoutStatusResult::Git(Box::new(paseo))
        )
        .is_err());

        let err = build_checkout_status_error_payload(
            "/x",
            "r",
            &GitError::NotGitRepo { cwd: "/x".into() },
        );
        assert_eq!(
            err["error"],
            json!({"code": "NOT_GIT_REPO", "message": "Not a git repository: /x"})
        );
    }
}
