//! Diff shortstat and ref-driven derived state (`getCheckoutShortstat`,
//! `getCheckoutRefDerivedState`, `getCheckoutWorktreeState`).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use regex::Regex;
use serde::Serialize;
use tokio::sync::OnceCell;

use crate::args;
use crate::diff::PER_FILE_DIFF_MAX_BYTES;
use crate::error::Result;
use crate::paths::{branch_name_from_ref, node_resolve};
use crate::repo::{
    does_git_ref_exist, get_ahead_behind, get_ahead_behind_for_comparison_ref, get_current_branch,
    is_working_tree_dirty, require_git_repo, resolve_base_ref_for_cwd,
    resolve_best_comparison_base_ref, ro, AheadBehind, CheckoutContext, CheckoutSnapshotFacts,
    GitFacts, UpstreamStatus,
};
use crate::run::run_git;

pub const DEFAULT_SHORTSTAT_CACHE_TTL: Duration = Duration::from_secs(15);
const SHORTSTAT_CACHE_MAX: usize = 1000;
const UNTRACKED_SHORTSTAT_MAX_FILES: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CheckoutShortstat {
    pub additions: u64,
    pub deletions: u64,
}

fn parse_checkout_shortstat(text: &str) -> Option<CheckoutShortstat> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    let grab = |pat: &str| -> u64 {
        Regex::new(pat)
            .unwrap()
            .captures(trimmed)
            .and_then(|c| c[1].parse().ok())
            .unwrap_or(0)
    };
    let additions = grab(r"(\d+)\s+insertion");
    let deletions = grab(r"(\d+)\s+deletion");
    if additions == 0 && deletions == 0 {
        return None;
    }
    Some(CheckoutShortstat {
        additions,
        deletions,
    })
}

async fn is_likely_binary(path: &str) -> bool {
    use tokio::io::AsyncReadExt;
    let Ok(mut f) = tokio::fs::File::open(path).await else {
        return false;
    };
    let mut buf = vec![0u8; 16 * 1024];
    let Ok(n) = f.read(&mut buf).await else {
        return false;
    };
    if n == 0 {
        return false;
    }
    let mut suspicious = 0usize;
    for &b in &buf[..n] {
        if b == 0 {
            return true;
        }
        if b < 7 || (b > 14 && b < 32) || b == 127 {
            suspicious += 1;
        }
    }
    suspicious as f64 / n as f64 > 0.3
}

async fn count_untracked_additions(cwd: &str, throw_on_git_error: bool) -> Result<u64> {
    let out = match run_git(
        &["ls-files", "-z", "--others", "--exclude-standard"],
        &ro(cwd),
    )
    .await
    {
        Ok(o) => o,
        Err(e) if throw_on_git_error => return Err(e),
        Err(_) => return Ok(0),
    };
    let mut additions = 0u64;
    for file in out
        .stdout
        .split('\0')
        .filter(|f| !f.is_empty())
        .take(UNTRACKED_SHORTSTAT_MAX_FILES)
    {
        let absolute = node_resolve(cwd, file);
        let Ok(meta) = tokio::fs::metadata(&absolute).await else {
            continue;
        };
        if meta.len() as usize > PER_FILE_DIFF_MAX_BYTES || is_likely_binary(&absolute).await {
            continue;
        }
        // Unreadable (or non-UTF-8) files are skipped like in the TS reader.
        let Ok(content) = tokio::fs::read_to_string(&absolute).await else {
            continue;
        };
        if content.is_empty() {
            continue;
        }
        let normalized = content.replace("\r\n", "\n");
        let line_count = normalized.split('\n').count() as u64;
        additions += if normalized.ends_with('\n') {
            line_count - 1
        } else {
            line_count
        };
    }
    Ok(additions)
}

async fn resolve_shortstat_comparison_ref(
    cwd: &str,
    current_branch: Option<&str>,
    local_base_ref: Option<&str>,
    facts: Option<&GitFacts>,
) -> Result<Option<String>> {
    let Some(current_branch) = current_branch else {
        return Ok(None);
    };
    if let Some(base) = local_base_ref.filter(|b| *b != current_branch) {
        if let Some(f) = facts.filter(|f| {
            f.resolved_base_ref.as_deref() == Some(base) && f.comparison_base_ref.is_some()
        }) {
            return Ok(f.comparison_base_ref.clone());
        }
        return Ok(resolve_best_comparison_base_ref(cwd, base).await.ok());
    }
    let has_origin =
        does_git_ref_exist(cwd, &format!("refs/remotes/origin/{current_branch}")).await?;
    Ok(has_origin.then(|| format!("origin/{current_branch}")))
}

/// Uncached shortstat against the merge base of HEAD and the comparison ref.
/// With `throw_on_git_error` git failures surface instead of reading as `None`.
pub async fn get_checkout_shortstat_uncached(
    cwd: &str,
    ctx: &CheckoutContext,
    throw_on_git_error: bool,
) -> Result<Option<CheckoutShortstat>> {
    if matches!(ctx.facts, Some(CheckoutSnapshotFacts::NotGit)) {
        return Ok(None);
    }
    let facts = match &ctx.facts {
        Some(CheckoutSnapshotFacts::Git(f)) => Some(f),
        _ => None,
    };
    if facts.is_none() && require_git_repo(cwd).await.is_err() {
        return Ok(None);
    }
    let (local_base_ref, current_branch) = match facts {
        Some(f) => (f.resolved_base_ref.clone(), f.current_branch.clone()),
        None => (
            resolve_base_ref_for_cwd(cwd, ctx).await?.resolved_base_ref,
            get_current_branch(cwd).await,
        ),
    };
    let Some(comparison) = resolve_shortstat_comparison_ref(
        cwd,
        current_branch.as_deref(),
        local_base_ref.as_deref(),
        facts,
    )
    .await?
    else {
        return Ok(None);
    };

    let attempt: Result<Option<CheckoutShortstat>> = async {
        let merge_base = run_git(&["merge-base", "HEAD", &comparison], &ro(cwd)).await?;
        let merge_base = merge_base.stdout.trim().to_string();
        if merge_base.is_empty() {
            return Ok(None);
        }
        let diff_args = args!["diff", "--shortstat", merge_base.as_str()];
        let diff_opts = ro(cwd);
        let (diff, untracked) = tokio::join!(
            run_git(&diff_args, &diff_opts),
            count_untracked_additions(cwd, throw_on_git_error),
        );
        let (diff, untracked) = (diff?, untracked?);
        Ok(match parse_checkout_shortstat(&diff.stdout) {
            Some(t) => Some(CheckoutShortstat {
                additions: t.additions + untracked,
                deletions: t.deletions,
            }),
            None if untracked > 0 => Some(CheckoutShortstat {
                additions: untracked,
                deletions: 0,
            }),
            None => None,
        })
    }
    .await;
    match attempt {
        Err(e) if throw_on_git_error => Err(e),
        Err(_) => Ok(None),
        Ok(v) => Ok(v),
    }
}

/// TTL cache + in-flight sharing for shortstat reads (15s, 1000 entries).
type ShortstatCell = Arc<OnceCell<Option<CheckoutShortstat>>>;

pub struct CheckoutShortstatCache {
    ttl: Duration,
    entries: Mutex<HashMap<String, (Instant, ShortstatCell)>>,
}

impl Default for CheckoutShortstatCache {
    fn default() -> Self {
        Self::new(DEFAULT_SHORTSTAT_CACHE_TTL)
    }
}

impl CheckoutShortstatCache {
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// Cached value when still fresh (`getCachedCheckoutShortstat`).
    pub fn peek(&self, cwd: &str) -> Option<Option<CheckoutShortstat>> {
        let entries = self.entries.lock().unwrap();
        let (at, cell) = entries.get(cwd)?;
        (at.elapsed() < self.ttl)
            .then(|| cell.get().copied())
            .flatten()
    }

    fn cell_for(&self, cwd: &str) -> ShortstatCell {
        let mut entries = self.entries.lock().unwrap();
        let now = Instant::now();
        if let Some((at, cell)) = entries.get(cwd) {
            if now.duration_since(*at) < self.ttl || !cell.initialized() {
                return cell.clone();
            }
        }
        if entries.len() >= SHORTSTAT_CACHE_MAX {
            if let Some(oldest) = entries
                .iter()
                .min_by_key(|(_, (at, _))| *at)
                .map(|(k, _)| k.clone())
            {
                entries.remove(&oldest);
            }
        }
        let cell = Arc::new(OnceCell::new());
        entries.insert(cwd.to_string(), (now, cell.clone()));
        cell
    }

    fn has_fresh_entry(&self, cwd: &str) -> bool {
        let entries = self.entries.lock().unwrap();
        entries
            .get(cwd)
            .is_some_and(|(at, cell)| !cell.initialized() || at.elapsed() < self.ttl)
    }

    /// Populate the cache without blocking the caller (`warmCheckoutShortstatInBackground`).
    /// Does nothing when a fresh value or an in-flight read already exists; read
    /// failures are ignored.
    pub fn warm_in_background(
        self: &Arc<Self>,
        cwd: &str,
        ctx: &CheckoutContext,
        on_complete: Option<Box<dyn FnOnce() + Send>>,
    ) {
        if self.has_fresh_entry(cwd) {
            return;
        }
        let (cache, cwd, ctx) = (self.clone(), cwd.to_string(), ctx.clone());
        tokio::spawn(async move {
            if cache.get(&cwd, &ctx, false).await.is_ok() {
                if let Some(done) = on_complete {
                    done();
                }
            }
        });
    }

    pub async fn get(
        &self,
        cwd: &str,
        ctx: &CheckoutContext,
        force: bool,
    ) -> Result<Option<CheckoutShortstat>> {
        if force {
            let value = get_checkout_shortstat_uncached(cwd, ctx, false).await?;
            let cell = Arc::new(OnceCell::new());
            let _ = cell.set(value);
            self.entries
                .lock()
                .unwrap()
                .insert(cwd.to_string(), (Instant::now(), cell));
            return Ok(value);
        }
        let cell = self.cell_for(cwd);
        cell.get_or_try_init(|| get_checkout_shortstat_uncached(cwd, ctx, false))
            .await
            .copied()
    }
}

// ---- ref-derived state ---------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckoutRefDerivedState {
    pub ahead_behind: Option<AheadBehind>,
    pub diff_stat: Option<CheckoutShortstat>,
    pub upstream_status: Option<UpstreamStatus>,
}

fn normalize_remote_tracking_ref(r: &str) -> String {
    r.strip_prefix("refs/remotes/").unwrap_or(r).to_string()
}

fn facts_configured_remote_ref(facts: &GitFacts) -> Option<String> {
    let tracked = facts
        .branch_merge_ref
        .as_deref()?
        .strip_prefix("refs/heads/")?;
    let remote = facts.branch_remote_name.as_deref().filter(|r| *r != ".")?;
    Some(format!("{remote}/{tracked}"))
}

struct RefMovement {
    base_moved: bool,
    comparison_ref: Option<String>,
    current_branch: Option<String>,
    normalized_resolved_base: Option<String>,
    upstream_moved: bool,
    upstream_ref: Option<String>,
}

fn get_checkout_ref_movement(facts: &GitFacts, moved_remote_refs: &HashSet<String>) -> RefMovement {
    let current_branch = facts.current_branch.clone();
    let comparison_ref = facts.comparison_base_ref.clone();
    let moves: HashSet<String> = moved_remote_refs
        .iter()
        .map(|r| normalize_remote_tracking_ref(r))
        .collect();
    let normalized_resolved_base = facts.resolved_base_ref.as_deref().map(branch_name_from_ref);
    let shortstat_remote_ref = current_branch.as_ref().and_then(|b| {
        (normalized_resolved_base.is_none()
            || normalized_resolved_base.as_deref() == Some(b.as_str()))
        .then(|| format!("origin/{b}"))
    });
    let base_moved = [
        facts.stored_base_ref.clone(),
        facts.resolved_base_ref.clone(),
        comparison_ref.clone(),
        shortstat_remote_ref,
    ]
    .into_iter()
    .flatten()
    .map(|r| normalize_remote_tracking_ref(&r))
    .any(|r| moves.contains(&r));
    let upstream_ref = facts
        .upstream_status
        .as_ref()
        .map(|u| u.reference.clone())
        .or_else(|| facts_configured_remote_ref(facts));
    let upstream_moved = upstream_ref
        .as_ref()
        .is_some_and(|r| moves.contains(&normalize_remote_tracking_ref(r)));
    RefMovement {
        base_moved,
        comparison_ref,
        current_branch,
        normalized_resolved_base,
        upstream_moved,
        upstream_ref,
    }
}

/// Recompute only what the moved remote refs can have changed.
pub async fn get_checkout_ref_derived_state(
    cwd: &str,
    facts: &GitFacts,
    current: &CheckoutRefDerivedState,
    moved_remote_refs: &HashSet<String>,
    ctx: &CheckoutContext,
) -> Result<CheckoutRefDerivedState> {
    let m = get_checkout_ref_movement(facts, moved_remote_refs);
    let facts_ctx = CheckoutContext {
        facts: Some(CheckoutSnapshotFacts::Git(facts.clone())),
        ..ctx.clone()
    };
    let mut ahead_behind = current.ahead_behind;
    let mut diff_stat = current.diff_stat;
    if let (true, Some(branch), Some(base)) =
        (m.base_moved, &m.current_branch, &facts.resolved_base_ref)
    {
        ahead_behind = get_ahead_behind(cwd, base, branch, &facts_ctx).await?;
    }
    if m.base_moved || (m.upstream_moved && m.current_branch == m.normalized_resolved_base) {
        diff_stat = get_checkout_shortstat_uncached(cwd, &facts_ctx, true).await?;
    }
    let mut upstream_status = facts.upstream_status.clone();
    if let (true, Some(branch), Some(upstream_ref)) =
        (m.upstream_moved, &m.current_branch, &m.upstream_ref)
    {
        let normalized_upstream = normalize_remote_tracking_ref(upstream_ref);
        let normalized_comparison = m
            .comparison_ref
            .as_deref()
            .map(normalize_remote_tracking_ref);
        let upstream_ab = if m.base_moved
            && normalized_comparison.as_deref() == Some(normalized_upstream.as_str())
        {
            ahead_behind
        } else {
            get_ahead_behind_for_comparison_ref(cwd, upstream_ref, branch).await?
        };
        upstream_status = upstream_ab.map(|ab| UpstreamStatus {
            reference: facts
                .upstream_status
                .as_ref()
                .map(|u| u.reference.clone())
                .unwrap_or_else(|| format!("refs/remotes/{normalized_upstream}")),
            ahead_behind: ab,
        });
    }
    Ok(CheckoutRefDerivedState {
        ahead_behind,
        diff_stat,
        upstream_status,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckoutWorktreeState {
    pub is_dirty: bool,
    pub diff_stat: Option<CheckoutShortstat>,
}

pub async fn get_checkout_worktree_state(
    cwd: &str,
    ctx: &CheckoutContext,
    cache: &CheckoutShortstatCache,
) -> Result<CheckoutWorktreeState> {
    let (is_dirty, diff_stat) = tokio::join!(is_working_tree_dirty(cwd), cache.get(cwd, ctx, true));
    Ok(CheckoutWorktreeState {
        is_dirty: is_dirty?,
        diff_stat: diff_stat?,
    })
}
