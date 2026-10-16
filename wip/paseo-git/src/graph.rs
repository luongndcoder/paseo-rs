//! Git graph and ref listings plus the detail views behind
//! `checkout.commits.list` (graph query, conflict file, stash/compare detail, blame).

use std::path::Path;
use std::sync::OnceLock;

use regex::Regex;
use serde::Serialize;

use crate::commits::{
    get_unpushed_commit_shas, parse_graph_commit_records, parse_numstat_line, CheckoutCommit,
    CheckoutCommitFile, CommitFileStatus, OrderedMap, GRAPH_COMMIT_LOG_FORMAT,
};
use crate::diff_parse::{parse_diff, ParsedDiffFile};
use crate::error::{GitError, Result};
use crate::paths::node_resolve;
use crate::repo::{
    get_current_branch, require_git_repo, resolve_base_ref_for_cwd, ro, CheckoutContext,
};
use crate::run::run_git;
use crate::{args, argv};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitBranchNode {
    pub name: String,
    pub sha: String,
    pub is_current: bool,
    pub upstream: Option<String>,
    pub ahead: u64,
    pub behind: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GitRemoteBranchNode {
    pub name: String,
    pub sha: String,
    pub remote: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GitTagNode {
    pub name: String,
    pub sha: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub date: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GitStashNode {
    pub index: usize,
    /// e.g. `stash@{0}`.
    pub r#ref: String,
    pub branch: Option<String>,
    pub message: String,
    pub date: String,
    pub sha: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum GitActiveOperation {
    #[serde(rename = "merge")]
    Merge,
    #[serde(rename = "rebase")]
    Rebase,
    #[serde(rename = "cherry-pick")]
    CherryPick,
    #[serde(rename = "conflict")]
    Conflict,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitConflictState {
    pub in_progress: Option<GitActiveOperation>,
    pub conflict_files: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitConflictFilePayload {
    pub path: String,
    pub base_content: String,
    pub ours_content: String,
    pub theirs_content: String,
    pub working_content: String,
    pub ours_label: String,
    pub theirs_label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitBlameLine {
    pub line_number: u64,
    pub sha: String,
    pub short_sha: String,
    pub author: String,
    pub author_time: u64,
    pub summary: String,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GitDiffDetail {
    pub label: String,
    pub files: Vec<CheckoutCommitFile>,
    pub diffs: Vec<ParsedDiffFile>,
}

async fn check_git_path_exists(cwd: &str, sub_path: &str) -> bool {
    let Ok(res) = run_git(&["rev-parse", "--git-path", sub_path], &ro(cwd)).await else {
        return false;
    };
    let raw = res.stdout.trim().to_string();
    !raw.is_empty() && Path::new(&node_resolve(cwd, &raw)).exists()
}

/// Which merge/rebase/cherry-pick is in progress, if any.
pub async fn detect_git_active_operation(cwd: &str) -> Option<GitActiveOperation> {
    let (merge_head, rebase_head, rebase_merge, rebase_apply, cherry_pick_head) = tokio::join!(
        check_git_path_exists(cwd, "MERGE_HEAD"),
        check_git_path_exists(cwd, "REBASE_HEAD"),
        check_git_path_exists(cwd, "rebase-merge"),
        check_git_path_exists(cwd, "rebase-apply"),
        check_git_path_exists(cwd, "CHERRY_PICK_HEAD"),
    );
    if merge_head {
        Some(GitActiveOperation::Merge)
    } else if rebase_head || rebase_merge || rebase_apply {
        Some(GitActiveOperation::Rebase)
    } else if cherry_pick_head {
        Some(GitActiveOperation::CherryPick)
    } else {
        None
    }
}

pub async fn detect_git_conflict_state(cwd: &str) -> GitConflictState {
    let opts = ro(cwd);
    let (diff, active) = tokio::join!(
        run_git(&["diff", "--name-only", "--diff-filter=U"], &opts),
        detect_git_active_operation(cwd),
    );
    let conflict_files: Vec<String> = diff
        .map(|d| {
            d.stdout
                .split('\n')
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let in_progress = active.or(if conflict_files.is_empty() {
        None
    } else {
        Some(GitActiveOperation::Conflict)
    });
    GitConflictState {
        in_progress,
        conflict_files,
    }
}

pub struct RefsAndBranches {
    pub branches: Vec<GitBranchNode>,
    pub remote_branches: Vec<GitRemoteBranchNode>,
    pub tags: Vec<GitTagNode>,
    pub stashes: Vec<GitStashNode>,
    pub conflict_state: GitConflictState,
}

async fn stdout_or_empty(cwd: &str, args: &[&str]) -> String {
    run_git(args, &ro(cwd))
        .await
        .map(|o| o.stdout)
        .unwrap_or_default()
}

pub async fn list_refs_and_branches(cwd: &str) -> RefsAndBranches {
    static AHEAD: OnceLock<Regex> = OnceLock::new();
    static BEHIND: OnceLock<Regex> = OnceLock::new();
    static STASH_IDX: OnceLock<Regex> = OnceLock::new();
    static STASH_MSG: OnceLock<Regex> = OnceLock::new();
    let (local, remote, tags_out, stash_out, conflict_state) = tokio::join!(
        stdout_or_empty(cwd, &["for-each-ref", "--sort=-committerdate", "--format=%(refname:short)%00%(objectname)%00%(HEAD)%00%(upstream:short)%00%(upstream:track)", "refs/heads"]),
        stdout_or_empty(cwd, &["for-each-ref", "--sort=-committerdate", "--format=%(refname:short)%00%(objectname)", "refs/remotes"]),
        stdout_or_empty(cwd, &["for-each-ref", "--sort=-creatordate", "--format=%(refname:short)%00%(*objectname)%00%(objectname)%00%(creatordate:iso-strict)", "refs/tags"]),
        stdout_or_empty(cwd, &["stash", "list", "--format=%gd%x09%H%x09%aI%x09%gs"]),
        detect_git_conflict_state(cwd),
    );

    let mut branches = Vec::new();
    for line in local.split('\n') {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let mut p = trimmed.split('\0');
        let (name, sha) = (p.next().unwrap_or(""), p.next().unwrap_or(""));
        let (head_marker, upstream, track) = (
            p.next().unwrap_or(""),
            p.next().unwrap_or(""),
            p.next().unwrap_or(""),
        );
        if name.is_empty() || sha.is_empty() {
            continue;
        }
        let grab = |cell: &'static OnceLock<Regex>, pat: &str| {
            cell.get_or_init(|| Regex::new(pat).unwrap())
                .captures(track)
                .and_then(|c| c[1].parse::<u64>().ok())
                .unwrap_or(0)
        };
        branches.push(GitBranchNode {
            name: name.to_string(),
            sha: sha.to_string(),
            is_current: head_marker == "*",
            upstream: (!upstream.is_empty()).then(|| upstream.to_string()),
            ahead: grab(&AHEAD, r"ahead\s+(\d+)"),
            behind: grab(&BEHIND, r"behind\s+(\d+)"),
        });
    }

    let mut remote_branches = Vec::new();
    for line in remote.split('\n') {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let mut p = trimmed.split('\0');
        let (name, sha) = (p.next().unwrap_or(""), p.next().unwrap_or(""));
        if name.is_empty() || sha.is_empty() || name.ends_with("/HEAD") || !name.contains('/') {
            continue;
        }
        let remote_name = &name[..name.find('/').unwrap()];
        remote_branches.push(GitRemoteBranchNode {
            name: name.to_string(),
            sha: sha.to_string(),
            remote: remote_name.to_string(),
        });
    }

    let mut tags = Vec::new();
    for line in tags_out.split('\n') {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let mut p = trimmed.split('\0');
        let (name, peeled, obj, date) = (
            p.next().unwrap_or(""),
            p.next().unwrap_or(""),
            p.next().unwrap_or(""),
            p.next().unwrap_or(""),
        );
        let target = if peeled.is_empty() { obj } else { peeled }.trim();
        if name.is_empty() || target.is_empty() {
            continue;
        }
        tags.push(GitTagNode {
            name: name.to_string(),
            sha: target.to_string(),
            date: (!date.is_empty()).then(|| date.to_string()),
        });
    }

    let stash_idx = STASH_IDX.get_or_init(|| Regex::new(r"stash@\{(\d+)\}").unwrap());
    let stash_msg = STASH_MSG.get_or_init(|| Regex::new(r"^(?:WIP on|On) ([^:]+): (.*)$").unwrap());
    let mut stashes: Vec<GitStashNode> = Vec::new();
    for line in stash_out.split('\n') {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let parts: Vec<&str> = trimmed.split('\t').collect();
        let reference = parts.first().map_or("", |s| s.trim());
        let sha = parts.get(1).map_or("", |s| s.trim());
        let date = parts.get(2).map_or("", |s| s.trim());
        let gs = if parts.len() > 3 {
            parts[3..].join("\t").trim().to_string()
        } else {
            String::new()
        };
        if reference.is_empty() || sha.is_empty() {
            continue;
        }
        let index = stash_idx
            .captures(reference)
            .and_then(|c| c[1].parse::<usize>().ok())
            .unwrap_or(stashes.len());
        let caps = stash_msg.captures(&gs);
        let branch = caps
            .as_ref()
            .map(|c| c[1].trim().to_string())
            .filter(|b| !b.is_empty());
        let message = caps
            .as_ref()
            .map(|c| c[2].trim().to_string())
            .filter(|m| !m.is_empty())
            .or_else(|| (!gs.is_empty()).then(|| gs.clone()))
            .unwrap_or_else(|| reference.to_string());
        stashes.push(GitStashNode {
            index,
            r#ref: reference.to_string(),
            branch,
            message,
            date: date.to_string(),
            sha: sha.to_string(),
        });
    }

    RefsAndBranches {
        branches,
        remote_branches,
        tags,
        stashes,
        conflict_state,
    }
}

#[derive(Debug, Clone, Default)]
pub struct GraphQuery {
    pub all_branches: Option<bool>,
    pub r#ref: Option<String>,
    pub search: Option<String>,
    pub author: Option<String>,
    pub file_path: Option<String>,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphResult {
    pub base_ref: Option<String>,
    pub commits: Vec<CheckoutCommit>,
    pub head_sha: Option<String>,
    pub current_branch: Option<String>,
    pub last_commit_message: Option<String>,
    pub branches: Vec<GitBranchNode>,
    pub remote_branches: Vec<GitRemoteBranchNode>,
    pub tags: Vec<GitTagNode>,
    pub stashes: Vec<GitStashNode>,
    pub conflict_state: GitConflictState,
}

fn non_blank(v: &Option<String>) -> Option<&str> {
    v.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

pub async fn query_git_graph_and_refs(
    cwd: &str,
    query: &GraphQuery,
    ctx: &CheckoutContext,
) -> Result<GraphResult> {
    require_git_repo(cwd).await?;
    let limit = query.limit.unwrap_or(150).clamp(1, 400);
    let (current_branch, head_out, last_msg, refs, unpushed, base) = tokio::join!(
        get_current_branch(cwd),
        stdout_or_empty(cwd, &["rev-parse", "--verify", "HEAD"]),
        stdout_or_empty(cwd, &["log", "-1", "--format=%B", "HEAD"]),
        list_refs_and_branches(cwd),
        async { get_unpushed_commit_shas(cwd).await.unwrap_or_default() },
        async {
            resolve_base_ref_for_cwd(cwd, ctx)
                .await
                .map(|b| b.resolved_base_ref)
                .unwrap_or(None)
        },
    );
    let head_sha = Some(head_out.trim().to_string()).filter(|s| !s.is_empty());
    let last_commit_message = Some(last_msg.trim().to_string()).filter(|s| !s.is_empty());

    let mut log_args = argv!["log", "--topo-order", format!("--max-count={limit}")];
    let trimmed_ref = non_blank(&query.r#ref);
    let trimmed_file = non_blank(&query.file_path);
    match trimmed_ref {
        Some(r) if r != "ALL" => log_args.push(r.to_string()),
        _ if query.all_branches.unwrap_or(true) && trimmed_file.is_none() => {
            log_args.extend(args!["--branches", "--remotes", "--tags", "HEAD"]);
        }
        _ => log_args.push("HEAD".into()),
    }
    if let Some(s) = non_blank(&query.search) {
        log_args.extend([format!("--grep={s}"), "-i".to_string()]);
    }
    if let Some(a) = non_blank(&query.author) {
        log_args.extend([format!("--author={a}"), "-i".to_string()]);
    }
    log_args.extend(args![
        "--diff-merges=first-parent",
        format!("--format={GRAPH_COMMIT_LOG_FORMAT}"),
        "--raw",
        "--numstat",
        "-M"
    ]);
    if let Some(f) = trimmed_file {
        log_args.extend(args!["--follow", "--", f]);
    }
    let commits = if head_sha.is_some() {
        let out = run_git(&log_args, &ro(cwd).max_output(16 * 1024 * 1024)).await?;
        parse_graph_commit_records(&out.stdout, &unpushed)
    } else {
        vec![]
    };
    Ok(GraphResult {
        base_ref: base,
        commits,
        head_sha,
        current_branch,
        last_commit_message,
        branches: refs.branches,
        remote_branches: refs.remote_branches,
        tags: refs.tags,
        stashes: refs.stashes,
        conflict_state: refs.conflict_state,
    })
}

pub async fn get_git_conflict_file(cwd: &str, file_path: &str) -> Result<GitConflictFilePayload> {
    require_git_repo(cwd).await?;
    let path = file_path.trim();
    if path.is_empty() || path.starts_with('-') {
        return Err(GitError::other(format!(
            "Invalid conflict file path: {file_path}"
        )));
    }
    let root = node_resolve("/", cwd);
    let target = node_resolve(&root, path);
    if target != root && !target.starts_with(&format!("{root}/")) {
        return Err(GitError::other(format!(
            "Invalid conflict file path outside repository: {file_path}"
        )));
    }
    let stage = |n: u8| {
        let spec = format!(":{n}:{path}");
        async move {
            run_git(&args!["show", spec], &ro(cwd))
                .await
                .map(|o| o.stdout)
                .unwrap_or_default()
        }
    };
    let (base, ours, theirs, working, branch, active) = tokio::join!(
        stage(1),
        stage(2),
        stage(3),
        async { tokio::fs::read_to_string(&target).await.unwrap_or_default() },
        get_current_branch(cwd),
        detect_git_active_operation(cwd),
    );
    let ours_label = match branch {
        Some(b) => format!("HEAD ({b})"),
        None => "HEAD (Local)".to_string(),
    };
    let theirs_label = match active {
        Some(GitActiveOperation::Rebase) => "REBASE_HEAD / Incoming",
        Some(GitActiveOperation::CherryPick) => "CHERRY_PICK_HEAD / Incoming",
        _ => "MERGE_HEAD / Incoming",
    };
    Ok(GitConflictFilePayload {
        path: path.to_string(),
        base_content: base,
        ours_content: ours,
        theirs_content: theirs,
        working_content: working,
        ours_label,
        theirs_label: theirs_label.to_string(),
    })
}

fn detail_status(d: &ParsedDiffFile) -> CommitFileStatus {
    if d.is_new {
        CommitFileStatus::Added
    } else if d.is_deleted {
        CommitFileStatus::Deleted
    } else if d.old_path.is_some() {
        CommitFileStatus::Renamed
    } else {
        CommitFileStatus::Modified
    }
}

pub async fn get_git_stash_detail(cwd: &str, stash_index: usize) -> Result<GitDiffDetail> {
    require_git_repo(cwd).await?;
    let stash_ref = format!("stash@{{{stash_index}}}");
    let patch_args = args!["stash", "show", "-p", "--no-color", stash_ref.as_str()];
    let stat_args = args!["stash", "show", "--numstat", stash_ref.as_str()];
    let patch_opts = ro(cwd).max_output(16 * 1024 * 1024);
    let stat_opts = ro(cwd);
    let (patch, numstat) = tokio::join!(
        run_git(&patch_args, &patch_opts),
        run_git(&stat_args, &stat_opts)
    );
    let diffs = parse_diff(&patch?.stdout);
    let mut stats: OrderedMap<(u64, u64)> = OrderedMap::new();
    for raw in numstat.map(|n| n.stdout).unwrap_or_default().split('\n') {
        let line = raw.trim();
        if !line.is_empty() {
            parse_numstat_line(line, &mut stats);
        }
    }
    let mut by_path: OrderedMap<CheckoutCommitFile> = OrderedMap::new();
    for d in &diffs {
        let stat = stats.get(&d.path);
        by_path.insert(
            d.path.clone(),
            CheckoutCommitFile {
                path: d.path.clone(),
                additions: stat.map_or(d.additions, |s| s.0),
                deletions: stat.map_or(d.deletions, |s| s.1),
                status: Some(detail_status(d)),
            },
        );
    }
    for (path, (a, del)) in stats.iter() {
        if by_path.get(path).is_none() {
            by_path.insert(
                path.clone(),
                CheckoutCommitFile {
                    path: path.clone(),
                    additions: *a,
                    deletions: *del,
                    status: Some(CommitFileStatus::Modified),
                },
            );
        }
    }
    let files = by_path.iter().map(|(_, f)| f.clone()).collect();
    Ok(GitDiffDetail {
        label: stash_ref,
        files,
        diffs,
    })
}

pub async fn get_git_compare_detail(cwd: &str, compare_ref: &str) -> Result<GitDiffDetail> {
    require_git_repo(cwd).await?;
    let r = compare_ref.trim();
    if r.is_empty() || r.starts_with('-') {
        return Err(GitError::other(format!(
            "Invalid compare ref: {compare_ref}"
        )));
    }
    let opts = ro(cwd).max_output(16 * 1024 * 1024);
    let stdout = match run_git(&args!["diff", "--no-color", format!("{r}...HEAD")], &opts).await {
        Ok(o) => o.stdout,
        Err(_) => {
            run_git(&args!["diff", "--no-color", format!("{r}..HEAD")], &opts)
                .await?
                .stdout
        }
    };
    let diffs = parse_diff(&stdout);
    let files = diffs
        .iter()
        .map(|d| CheckoutCommitFile {
            path: d.path.clone(),
            additions: d.additions,
            deletions: d.deletions,
            status: Some(detail_status(d)),
        })
        .collect();
    Ok(GitDiffDetail {
        label: format!("{r} \u{2194} HEAD"),
        files,
        diffs,
    })
}

pub async fn get_git_file_blame(cwd: &str, file_path: &str) -> Result<Vec<GitBlameLine>> {
    static HEADER: OnceLock<Regex> = OnceLock::new();
    require_git_repo(cwd).await?;
    let path = file_path.trim();
    if path.is_empty() || path.starts_with('-') {
        return Err(GitError::other(format!(
            "Invalid file path for git blame: {file_path}"
        )));
    }
    let out = run_git(
        &["blame", "--line-porcelain", "HEAD", "--", path],
        &ro(cwd).max_output(16 * 1024 * 1024),
    )
    .await?;
    let header = HEADER.get_or_init(|| Regex::new(r"^([0-9a-f]{40})\s+\d+\s+(\d+)").unwrap());
    let mut result = Vec::new();
    let (mut sha, mut line_number, mut author, mut author_time, mut summary) =
        (String::new(), 0u64, String::new(), 0u64, String::new());
    for line in out.stdout.split('\n') {
        if sha.is_empty() {
            if let Some(c) = header.captures(line) {
                sha = c[1].to_string();
                line_number = c[2].parse().unwrap_or(0);
                author.clear();
                author_time = 0;
                summary.clear();
            }
            continue;
        }
        if let Some(content) = line.strip_prefix('\t') {
            result.push(GitBlameLine {
                line_number,
                sha: sha.clone(),
                short_sha: sha[..7].to_string(),
                author: author.clone(),
                author_time,
                summary: summary.clone(),
                content: content.to_string(),
            });
            sha.clear();
        } else if let Some(v) = line.strip_prefix("author ") {
            author = v.trim().to_string();
        } else if let Some(v) = line.strip_prefix("author-time ") {
            author_time = v.trim().parse().unwrap_or(0);
        } else if let Some(v) = line.strip_prefix("summary ") {
            summary = v.trim().to_string();
        }
    }
    Ok(result)
}
