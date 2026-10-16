//! Git log actions (`checkout.commits.list` with `gitAction`): cherry-pick,
//! revert, reset, checkout, merge/rebase with auto-stash, tags, history
//! rewriting, stash operations, hunk patches and conflict resolution.
//!
//! Behavior difference vs TS: the interactive-rebase editors are POSIX `sh`/`awk`
//! scripts instead of Node scripts, so reword/squash need a POSIX host.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::error::{GitError, Result};
use crate::graph::{detect_git_active_operation, detect_git_conflict_state, GitActiveOperation};
use crate::paths::node_resolve;
use crate::repo::{require_git_repo, ro, LONG_TIMEOUT_MS};
use crate::run::{run_git, GitOptions};
use crate::{args, argv};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GitLogActionKind {
    CherryPick,
    Revert,
    Reset,
    CheckoutRef,
    CreateBranch,
    DeleteBranch,
    MergeRef,
    RebaseOnto,
    CreateTag,
    DeleteTag,
    RewordCommit,
    SquashCommit,
    DropCommit,
    StashCreate,
    StashApply,
    StashPop,
    StashDrop,
    StashBranch,
    ApplyHunkPatch,
    ResolveConflict,
    AbortOperation,
    ContinueOperation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResetMode {
    Soft,
    Mixed,
    Hard,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PatchMode {
    Rollback,
    Stage,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitLogAction {
    pub kind: GitLogActionKind,
    pub target: String,
    #[serde(default)]
    pub second_target: Option<String>,
    #[serde(default)]
    pub mode: Option<ResetMode>,
    #[serde(default)]
    pub force: Option<bool>,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub patch: Option<String>,
    #[serde(default)]
    pub patch_mode: Option<PatchMode>,
    #[serde(default)]
    pub content: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ActionResult {
    pub success: bool,
    pub message: String,
}

fn ok(message: impl Into<String>) -> Result<ActionResult> {
    Ok(ActionResult {
        success: true,
        message: message.into(),
    })
}

fn long(cwd: &str) -> GitOptions {
    GitOptions::new(cwd).timeout_ms(LONG_TIMEOUT_MS)
}

fn short(sha: &str) -> &str {
    sha.get(..7).unwrap_or(sha)
}

fn is_dirty_tree_git_error(error: &GitError) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    let details = format!("{}\n{}\n{}", error, error.stderr(), error.stdout());
    RE.get_or_init(|| {
        Regex::new(r"(?i)overwritten by merge|overwritten by checkout|Please commit your changes or stash them|cannot rebase: You have unstaged changes|cannot rebase: Your index contains uncommitted changes|local changes to the following files would be overwritten|local changes would be overwritten").unwrap()
    })
    .is_match(&details)
}

async fn latest_stash_sha(cwd: &str) -> String {
    run_git(&["stash", "list", "-n", "1", "--format=%H"], &ro(cwd))
        .await
        .map(|o| o.stdout.trim().to_string())
        .unwrap_or_default()
}

/// Run a working-tree-touching operation; on a dirty-tree failure stash, retry and pop.
async fn execute_smart_git_operation(
    cwd: &str,
    label: &str,
    target: &str,
    git_args: &[String],
    success_message: String,
) -> Result<ActionResult> {
    let initial = match run_git(git_args, &long(cwd)).await {
        Ok(_) => return ok(success_message),
        Err(e) => e,
    };
    if is_dirty_tree_git_error(&initial) {
        let before = latest_stash_sha(cwd).await;
        let _ = run_git(
            &args![
                "stash",
                "push",
                "-u",
                "-m",
                format!("Paseo Smart {label} auto-stash ({target})")
            ],
            &long(cwd),
        )
        .await;
        let after = latest_stash_sha(cwd).await;
        let did_auto_stash = !after.is_empty() && after != before;
        if did_auto_stash {
            match run_git(git_args, &long(cwd)).await {
                Ok(_) => {
                    if run_git(&["stash", "pop"], &long(cwd)).await.is_ok() {
                        return ok(format!(
                            "Smart {label}: auto-stashed local changes, completed {} of {target}, and restored local changes",
                            label.to_lowercase()
                        ));
                    }
                    let pop_conflict = detect_git_conflict_state(cwd).await;
                    if !pop_conflict.conflict_files.is_empty() {
                        return ok(format!(
                            "CONFLICT: Smart {label} of {target} succeeded, but restoring stashed local changes produced {} conflict(s) ({}).",
                            pop_conflict.conflict_files.len(),
                            pop_conflict.conflict_files.join(", ")
                        ));
                    }
                    return ok(format!("Smart {label}: completed {} of {target} (local changes remain in stash@{{0}})", label.to_lowercase()));
                }
                Err(retry_error) => {
                    let conflict = detect_git_conflict_state(cwd).await;
                    if !conflict.conflict_files.is_empty() {
                        return ok(format!(
                            "CONFLICT: Smart {label} of {target} resulted in {} conflict(s) ({}). Local changes are safely stashed in stash@{{0}}.",
                            conflict.conflict_files.len(),
                            conflict.conflict_files.join(", ")
                        ));
                    }
                    let _ = run_git(&["stash", "pop"], &long(cwd)).await;
                    return Err(retry_error);
                }
            }
        }
    }
    let conflict = detect_git_conflict_state(cwd).await;
    if !conflict.conflict_files.is_empty() {
        return ok(format!(
            "CONFLICT: {label} of {target} resulted in {} conflict(s) ({}).",
            conflict.conflict_files.len(),
            conflict.conflict_files.join(", ")
        ));
    }
    Err(initial)
}

static SCRIPT_COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_suffix() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!(
        "{}-{}-{}",
        std::process::id(),
        nanos,
        SCRIPT_COUNTER.fetch_add(1, Ordering::SeqCst)
    )
}

/// Temp files removed on drop.
struct TempFiles(Vec<PathBuf>);

impl TempFiles {
    fn write(&mut self, name: &str, contents: &str, executable: bool) -> Result<String> {
        let path = std::env::temp_dir().join(format!("{name}-{}", unique_suffix()));
        std::fs::write(&path, contents)?;
        #[cfg(unix)]
        if executable {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
        }
        let p = path.to_string_lossy().into_owned();
        self.0.push(path);
        Ok(p)
    }
}

impl Drop for TempFiles {
    fn drop(&mut self) {
        for p in &self.0 {
            let _ = std::fs::remove_file(p);
        }
    }
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Sequence editor that rewrites the first `pick` line matching `target_sha` to `word`.
fn sequence_editor_script(target_sha: &str, word: &str) -> String {
    format!(
        "#!/bin/sh\nawk -v target={} -v word={} 'BEGIN {{ done = 0 }}\n{{\n  if (!done && match($0, /^pick[ \\t]+[0-9a-f]+/)) {{\n    split($0, f, /[ \\t]+/)\n    sha = f[2]\n    if (index(target, sha) == 1 || index(sha, target) == 1) {{ sub(/^pick/, word); done = 1 }}\n  }}\n  print\n}}' \"$1\" > \"$1.paseo-tmp\" && mv \"$1.paseo-tmp\" \"$1\"\n",
        shell_quote(target_sha),
        shell_quote(word)
    )
}

fn message_editor_script(message_file: &str) -> String {
    format!("#!/bin/sh\ncat {} > \"$1\"\n", shell_quote(message_file))
}

async fn rev_parse_ok(cwd: &str, spec: &str) -> Option<String> {
    run_git(&["rev-parse", "--verify", spec], &ro(cwd))
        .await
        .ok()
        .map(|o| o.stdout.trim().to_string())
}

fn stash_ref(raw: &str) -> String {
    if raw.starts_with("stash@{") {
        raw.to_string()
    } else {
        format!("stash@{{{raw}}}")
    }
}

fn inside_root(root: &str, target: &str) -> bool {
    target == root || target.starts_with(&format!("{root}/"))
}

pub async fn execute_git_log_action(cwd: &str, action: &GitLogAction) -> Result<ActionResult> {
    use GitLogActionKind::*;
    require_git_repo(cwd).await?;
    let target = action.target.trim();
    let target_optional = matches!(
        action.kind,
        StashCreate | ApplyHunkPatch | AbortOperation | ContinueOperation
    );
    if (target.is_empty() && !target_optional) || target.starts_with('-') {
        return Err(GitError::other(format!("Invalid git target: {target}")));
    }
    let second = action
        .second_target
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if second.is_some_and(|s| s.starts_with('-')) {
        return Err(GitError::other(format!(
            "Invalid secondary git target: {}",
            second.unwrap_or_default()
        )));
    }

    match action.kind {
        CherryPick => {
            execute_smart_git_operation(
                cwd,
                "Cherry-pick",
                target,
                &args!["cherry-pick", target],
                format!("Cherry-picked {}", short(target)),
            )
            .await
        }
        Revert => {
            execute_smart_git_operation(
                cwd,
                "Revert",
                target,
                &args!["revert", "--no-edit", target],
                format!("Reverted {}", short(target)),
            )
            .await
        }
        Reset => {
            let (flag, label) = match action.mode {
                Some(ResetMode::Soft) => ("--soft", "soft"),
                Some(ResetMode::Hard) => ("--hard", "hard"),
                _ => ("--mixed", "mixed"),
            };
            run_git(&["reset", flag, target], &long(cwd)).await?;
            let shown = match action.mode {
                None => "mixed",
                Some(_) => label,
            };
            ok(format!("Reset ({shown}) to {}", short(target)))
        }
        CheckoutRef => {
            let mut checkout_args = argv!["checkout", target];
            let mut label = target.to_string();
            let is_remote = run_git(
                &[
                    "show-ref",
                    "--verify",
                    "--quiet",
                    &format!("refs/remotes/{target}"),
                ],
                &ro(cwd).accept(&[0, 1]),
            )
            .await
            .map(|o| o.exit_code)
            .unwrap_or(Some(1));
            if is_remote == Some(0) && target.contains('/') {
                let local = &target[target.find('/').unwrap() + 1..];
                let has_local = run_git(
                    &[
                        "show-ref",
                        "--verify",
                        "--quiet",
                        &format!("refs/heads/{local}"),
                    ],
                    &ro(cwd).accept(&[0, 1]),
                )
                .await
                .map(|o| o.exit_code)
                .unwrap_or(Some(1));
                if has_local == Some(0) {
                    checkout_args = argv!["checkout", local];
                    label = local.to_string();
                } else {
                    checkout_args = argv!["checkout", "--track", target];
                    label = format!("{local} (tracking {target})");
                }
            }
            execute_smart_git_operation(
                cwd,
                "Checkout",
                &label,
                &checkout_args,
                format!("Checked out {label}"),
            )
            .await
        }
        CreateBranch => {
            let mut a = argv!["checkout", "-b", target];
            a.extend(second.map(str::to_string));
            run_git(&a, &long(cwd)).await?;
            ok(format!("Created and checked out branch {target}"))
        }
        DeleteBranch => {
            run_git(
                &[
                    "branch",
                    if action.force == Some(true) {
                        "-D"
                    } else {
                        "-d"
                    },
                    target,
                ],
                &long(cwd),
            )
            .await?;
            ok(format!("Deleted branch {target}"))
        }
        MergeRef => {
            execute_smart_git_operation(
                cwd,
                "Merge",
                target,
                &args!["merge", "--no-edit", target],
                format!("Merged {target} into current branch"),
            )
            .await
        }
        RebaseOnto => {
            execute_smart_git_operation(
                cwd,
                "Rebase",
                target,
                &args!["rebase", target],
                format!("Rebased current branch onto {target}"),
            )
            .await
        }
        CreateTag => {
            let mut a = argv!["tag", target];
            a.extend(second.map(str::to_string));
            run_git(&a, &long(cwd)).await?;
            ok(format!("Created tag {target}"))
        }
        DeleteTag => {
            run_git(&["tag", "-d", target], &long(cwd)).await?;
            ok(format!("Deleted tag {target}"))
        }
        RewordCommit => reword_commit(cwd, target, action).await,
        SquashCommit => squash_commit(cwd, target, action).await,
        DropCommit => {
            run_git(
                &args!["rebase", "--onto", format!("{target}^"), target],
                &long(cwd),
            )
            .await?;
            ok(format!("Dropped commit {}", short(target)))
        }
        StashCreate => {
            let message = action
                .message
                .as_deref()
                .map(str::trim)
                .filter(|m| !m.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| {
                    if target.is_empty() {
                        "WIP".into()
                    } else {
                        target.to_string()
                    }
                });
            run_git(&["stash", "push", "-u", "-m", &message], &long(cwd)).await?;
            ok(format!("Created stash: {message}"))
        }
        StashApply => {
            let r = stash_ref(target);
            run_git(&["stash", "apply", &r], &long(cwd)).await?;
            ok(format!("Applied {r}"))
        }
        StashPop => {
            let r = stash_ref(target);
            run_git(&["stash", "pop", &r], &long(cwd)).await?;
            ok(format!("Popped {r}"))
        }
        StashDrop => {
            let r = stash_ref(target);
            run_git(&["stash", "drop", &r], &long(cwd)).await?;
            ok(format!("Dropped {r}"))
        }
        StashBranch => {
            let r = stash_ref(second.unwrap_or("0"));
            run_git(&["stash", "branch", target, &r], &long(cwd)).await?;
            ok(format!("Created branch {target} from {r}"))
        }
        ApplyHunkPatch => {
            let raw = action
                .patch
                .as_deref()
                .filter(|p| !p.trim().is_empty())
                .ok_or_else(|| GitError::other("Patch content is required for apply-hunk-patch"))?;
            let normalized = if raw.ends_with('\n') {
                raw.to_string()
            } else {
                format!("{raw}\n")
            };
            let mut temp = TempFiles(vec![]);
            let path = temp.write("paseo-hunk-patch", &normalized, false)?;
            let stage = action.patch_mode == Some(PatchMode::Stage);
            let apply_args = if stage {
                args![
                    "apply",
                    "--cached",
                    "--unidiff-zero",
                    "--whitespace=nowarn",
                    path.as_str()
                ]
            } else {
                args![
                    "apply",
                    "--reverse",
                    "--unidiff-zero",
                    "--whitespace=nowarn",
                    path.as_str()
                ]
            };
            run_git(&apply_args, &GitOptions::new(cwd).timeout_ms(60_000)).await?;
            ok(if stage {
                "Staged hunk"
            } else {
                "Rolled back hunk"
            })
        }
        ResolveConflict => {
            let root = node_resolve("/", cwd);
            let resolved = node_resolve(&root, target);
            if !inside_root(&root, &resolved) {
                return Err(GitError::other(format!(
                    "Invalid conflict file path outside repository: {target}"
                )));
            }
            tokio::fs::write(&resolved, action.content.clone().unwrap_or_default()).await?;
            run_git(
                &["add", "--", target],
                &GitOptions::new(cwd).timeout_ms(60_000),
            )
            .await?;
            ok(format!("Resolved conflict in {target}"))
        }
        AbortOperation => match detect_git_active_operation(cwd).await {
            Some(GitActiveOperation::Merge) => {
                run_git(&["merge", "--abort"], &long(cwd)).await?;
                ok("Aborted merge")
            }
            Some(GitActiveOperation::Rebase) => {
                run_git(&["rebase", "--abort"], &long(cwd)).await?;
                ok("Aborted rebase")
            }
            Some(GitActiveOperation::CherryPick) => {
                run_git(&["cherry-pick", "--abort"], &long(cwd)).await?;
                ok("Aborted cherry-pick")
            }
            _ => Err(GitError::other(
                "No active merge, rebase, or cherry-pick operation to abort",
            )),
        },
        ContinueOperation => match detect_git_active_operation(cwd).await {
            Some(GitActiveOperation::Merge) => {
                run_git(&["commit", "--no-edit"], &long(cwd)).await?;
                ok("Continued merge")
            }
            Some(GitActiveOperation::Rebase) => {
                run_git(
                    &["-c", "core.editor=true", "rebase", "--continue"],
                    &long(cwd),
                )
                .await?;
                ok("Continued rebase")
            }
            Some(GitActiveOperation::CherryPick) => {
                run_git(
                    &["-c", "core.editor=true", "cherry-pick", "--continue"],
                    &long(cwd),
                )
                .await?;
                ok("Continued cherry-pick")
            }
            _ => Err(GitError::other(
                "No active merge, rebase, or cherry-pick operation to continue",
            )),
        },
    }
}

async fn reword_commit(cwd: &str, target: &str, action: &GitLogAction) -> Result<ActionResult> {
    let new_message = action
        .message
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .ok_or_else(|| GitError::other("Commit message is required for reword-commit"))?;
    let opts = ro(cwd);
    let target_args = ["rev-parse", "--verify", target];
    let (head, target_sha) = tokio::join!(
        run_git(&["rev-parse", "--verify", "HEAD"], &opts),
        run_git(&target_args, &opts),
    );
    let (head_sha, target_sha) = (
        head?.stdout.trim().to_string(),
        target_sha?.stdout.trim().to_string(),
    );
    if head_sha == target_sha {
        run_git(&["commit", "--amend", "-m", new_message], &long(cwd)).await?;
        return ok(format!("Reworded commit {}", short(&target_sha)));
    }
    let has_parent = rev_parse_ok(cwd, &format!("{target_sha}^")).await.is_some();
    let mut temp = TempFiles(vec![]);
    let seq = temp.write(
        "paseo-git-seq",
        &sequence_editor_script(&target_sha, "reword"),
        true,
    )?;
    let msg_file = temp.write("paseo-git-msgtext", &format!("{new_message}\n"), false)?;
    let msg = temp.write("paseo-git-msg", &message_editor_script(&msg_file), true)?;
    let rebase_args = if has_parent {
        args!["rebase", "-i", format!("{target_sha}^")]
    } else {
        args!["rebase", "-i", "--root"]
    };
    run_git(
        &rebase_args,
        &long(cwd)
            .env("GIT_SEQUENCE_EDITOR", &seq)
            .env("GIT_EDITOR", &msg),
    )
    .await?;
    ok(format!("Reworded commit {}", short(&target_sha)))
}

async fn squash_commit(cwd: &str, target: &str, action: &GitLogAction) -> Result<ActionResult> {
    let target_sha = run_git(&["rev-parse", "--verify", target], &ro(cwd))
        .await?
        .stdout
        .trim()
        .to_string();
    if rev_parse_ok(cwd, &format!("{target_sha}^")).await.is_none() {
        return Err(GitError::other(
            "Cannot squash root commit without a parent",
        ));
    }
    let has_grandparent = rev_parse_ok(cwd, &format!("{target_sha}~2"))
        .await
        .is_some();
    let custom = action
        .message
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty());
    let mut temp = TempFiles(vec![]);
    let seq = temp.write(
        "paseo-git-squash-seq",
        &sequence_editor_script(
            &target_sha,
            if custom.is_some() { "squash" } else { "fixup" },
        ),
        true,
    )?;
    let editor = match custom {
        Some(m) => {
            let msg_file = temp.write("paseo-git-squash-msgtext", &format!("{m}\n"), false)?;
            temp.write(
                "paseo-git-squash-msg",
                &message_editor_script(&msg_file),
                true,
            )?
        }
        None => "true".to_string(),
    };
    let rebase_args = if has_grandparent {
        args!["rebase", "-i", format!("{target_sha}~2")]
    } else {
        args!["rebase", "-i", "--root"]
    };
    run_git(
        &rebase_args,
        &long(cwd)
            .env("GIT_SEQUENCE_EDITOR", &seq)
            .env("GIT_EDITOR", &editor),
    )
    .await?;
    ok(format!(
        "Squashed commit {} into parent",
        short(&target_sha)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_deserializes_from_the_wire_shape() {
        let a: GitLogAction = serde_json::from_str(r#"{"kind":"stash-branch","target":"feat","secondTarget":"2","force":true,"patchMode":"stage"}"#).unwrap();
        assert_eq!(
            (
                a.kind,
                a.target.as_str(),
                a.second_target.as_deref(),
                a.force,
                a.patch_mode
            ),
            (
                GitLogActionKind::StashBranch,
                "feat",
                Some("2"),
                Some(true),
                Some(PatchMode::Stage)
            )
        );
        assert!(serde_json::from_str::<GitLogAction>(r#"{"kind":"nope","target":"x"}"#).is_err());
    }

    #[test]
    fn dirty_tree_errors_are_recognized() {
        let e = GitError::Command {
            message: "x".into(),
            stderr:
                "error: Your local changes to the following files would be overwritten by checkout"
                    .into(),
            stdout: String::new(),
        };
        assert!(is_dirty_tree_git_error(&e));
        assert!(!is_dirty_tree_git_error(&GitError::other(
            "fatal: bad revision"
        )));
    }

    #[test]
    fn stash_refs_are_normalized() {
        assert_eq!(stash_ref("3"), "stash@{3}");
        assert_eq!(stash_ref("stash@{1}"), "stash@{1}");
    }
}
