//! Commit history for the checkout: list (`checkout.commits.list`) and the
//! per-file diff of one commit (`checkout.commits.file_diff`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use regex::Regex;
use serde::Serialize;
use serde_json::{json, Value};
use tokio::sync::OnceCell;

use crate::diff::{normalize_numstat_path, GIT_PATCH_FORMAT_ARGS};
use crate::diff_parse::{parse_diff, ParsedDiffFile};
use crate::error::{to_checkout_error, GitError, Result};
use crate::paths::{branch_name_from_ref, is_qualified_branch_ref, unquote_git_path};
use crate::repo::{
    get_current_branch, require_git_worktree_root, resolve_base_ref, resolve_base_ref_for_cwd,
    resolve_most_ahead_base_ref, ro, try_resolve_merge_base, CheckoutContext,
};
use crate::run::run_git;
use crate::{args, argv};

const CHECKOUT_BASE_COMMIT_LIMIT: usize = 10;
pub(crate) const COMMIT_FIELD_SEPARATOR: char = '\0';
pub(crate) const COMMIT_RECORD_SEPARATOR: char = '\x1e';
// `%x1e`/`%x00` are git placeholders: literal text in the arg, real bytes in the output.
const COMMIT_LOG_FORMAT: &str = "%x1e%H%x00%h%x00%an%x00%aI%x00%s";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CommitFileStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CheckoutCommitFile {
    pub path: String,
    pub additions: u64,
    pub deletions: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<CommitFileStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckoutCommit {
    pub sha: String,
    pub short_sha: String,
    pub subject: String,
    pub author_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author_email: Option<String>,
    /// ISO 8601.
    pub author_date: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parents: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refs: Option<Vec<String>>,
    /// false = local-only (unpushed).
    pub is_on_remote: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_on_base: Option<bool>,
    pub files: Vec<CheckoutCommitFile>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckoutCommitsResult {
    pub base_ref: Option<String>,
    pub commits: Vec<CheckoutCommit>,
}

/// Insertion-ordered map (JS `Map` semantics: overwriting keeps the position).
pub(crate) struct OrderedMap<V> {
    keys: Vec<String>,
    index: HashMap<String, V>,
}

impl<V> OrderedMap<V> {
    pub(crate) fn new() -> Self {
        Self {
            keys: Vec::new(),
            index: HashMap::new(),
        }
    }
    pub(crate) fn insert(&mut self, key: String, value: V) {
        if self.index.insert(key.clone(), value).is_none() {
            self.keys.push(key);
        }
    }
    pub(crate) fn get(&self, key: &str) -> Option<&V> {
        self.index.get(key)
    }
    pub(crate) fn iter(&self) -> impl Iterator<Item = (&String, &V)> {
        self.keys.iter().map(|k| (k, &self.index[k]))
    }
}

fn map_name_status_letter(letter: char) -> Option<CommitFileStatus> {
    match letter {
        'A' | 'C' => Some(CommitFileStatus::Added),
        'M' | 'T' => Some(CommitFileStatus::Modified),
        'D' => Some(CommitFileStatus::Deleted),
        'R' => Some(CommitFileStatus::Renamed),
        _ => None,
    }
}

/// A `--raw` line: `:<srcmode> <dstmode> <srcsha> <dstsha> <STATUS>\t<path>`;
/// rename/copy add a second path. Keyed on the destination path.
pub(crate) fn parse_raw_status_line(line: &str, statuses: &mut OrderedMap<CommitFileStatus>) {
    let tab_parts: Vec<&str> = line.split('\t').collect();
    let meta = tab_parts.first().copied().unwrap_or("");
    let status_token = &meta[meta.rfind(' ').map_or(0, |i| i + 1)..];
    let letter = status_token.chars().next().unwrap_or('\0');
    let Some(status) = map_name_status_letter(letter) else {
        return;
    };
    let path = if letter == 'R' || letter == 'C' {
        tab_parts.last().copied().unwrap_or("")
    } else {
        tab_parts.get(1).copied().unwrap_or("")
    };
    if path.is_empty() {
        return;
    }
    statuses.insert(unquote_git_path(path), status);
}

/// A `--numstat` line: `<adds>\t<dels>\t<path>`; binary files report `-`.
pub(crate) fn parse_numstat_line(line: &str, stats: &mut OrderedMap<(u64, u64)>) {
    let parts: Vec<&str> = line.split('\t').collect();
    if parts.len() < 3 {
        return;
    }
    let path = normalize_numstat_path(&parts[2..].join("\t"));
    if path.is_empty() {
        return;
    }
    if parts[0] == "-" || parts[1] == "-" {
        stats.insert(path, (0, 0));
        return;
    }
    // JS parseInt accepts a numeric prefix; git only emits digits here.
    let (Ok(a), Ok(d)) = (parts[0].parse::<u64>(), parts[1].parse::<u64>()) else {
        return;
    };
    stats.insert(path, (a, d));
}

pub(crate) fn files_from(
    stats: &OrderedMap<(u64, u64)>,
    statuses: &OrderedMap<CommitFileStatus>,
) -> Vec<CheckoutCommitFile> {
    stats
        .iter()
        .map(|(path, (additions, deletions))| CheckoutCommitFile {
            path: path.clone(),
            additions: *additions,
            deletions: *deletions,
            status: statuses.get(path).copied(),
        })
        .collect()
}

struct ParsedCommit {
    sha: String,
    short_sha: String,
    author_name: String,
    author_date: String,
    subject: String,
    files: Vec<CheckoutCommitFile>,
}

/// Parses the combined `git log ... --raw --numstat -M` stream; `--raw`
/// (status) and `--numstat` (counts) are merged by destination path.
fn parse_checkout_commit_records(stdout: &str) -> Vec<ParsedCommit> {
    let mut commits = Vec::new();
    for record in stdout
        .split(COMMIT_RECORD_SEPARATOR)
        .filter(|r| !r.is_empty())
    {
        let lines: Vec<&str> = record.split('\n').collect();
        let fields: Vec<&str> = lines[0].split(COMMIT_FIELD_SEPARATOR).collect();
        if fields.len() < 5 {
            continue;
        }
        let sha = fields[0].trim();
        if sha.is_empty() {
            continue;
        }
        let mut stats = OrderedMap::new();
        let mut statuses = OrderedMap::new();
        for line in lines.iter().skip(1).filter(|l| !l.is_empty()) {
            if line.starts_with(':') {
                parse_raw_status_line(line, &mut statuses);
            } else {
                parse_numstat_line(line, &mut stats);
            }
        }
        commits.push(ParsedCommit {
            sha: sha.to_string(),
            short_sha: fields[1].trim().to_string(),
            author_name: fields[2].to_string(),
            author_date: fields[3].trim().to_string(),
            subject: fields[4].to_string(),
            files: files_from(&stats, &statuses),
        });
    }
    commits
}

/// Commits reachable from HEAD that are not reachable from any remote ref.
pub(crate) async fn get_unpushed_commit_shas(
    cwd: &str,
) -> Result<std::collections::HashSet<String>> {
    let out = run_git(&["rev-list", "HEAD", "--not", "--remotes"], &ro(cwd)).await?;
    Ok(out
        .stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect())
}

async fn get_checkout_commit_records(
    cwd: &str,
    revision: &str,
    max_count: Option<usize>,
) -> Result<Vec<ParsedCommit>> {
    let mut a = argv!["log", revision];
    if let Some(n) = max_count {
        a.push(format!("--max-count={n}"));
    }
    a.extend(args![
        "--diff-merges=first-parent",
        format!("--format={COMMIT_LOG_FORMAT}"),
        "--raw",
        "--numstat",
        "-M"
    ]);
    let out = run_git(&a, &ro(cwd)).await?;
    if out.truncated {
        return Err(GitError::other(
            "Commit history exceeded the git output limit",
        ));
    }
    Ok(parse_checkout_commit_records(&out.stdout))
}

async fn try_resolve_checkout_commits_base_ref(
    cwd: &str,
    base_ref: Option<&str>,
    current_branch: &str,
) -> Result<Option<String>> {
    let Some(base_ref) = base_ref else {
        return Ok(None);
    };
    let normalized = branch_name_from_ref(base_ref);
    if normalized.is_empty() || normalized == current_branch {
        return Ok(None);
    }
    match resolve_most_ahead_base_ref(cwd, base_ref).await {
        Ok(r) => Ok(Some(r)),
        Err(e) if is_qualified_branch_ref(base_ref) => Err(e),
        Err(_) => Ok(None),
    }
}

/// Every workspace commit, followed by at most 10 base commits.
pub async fn list_checkout_commits(
    cwd: &str,
    ctx: &CheckoutContext,
) -> Result<CheckoutCommitsResult> {
    let Some(current_branch) = get_current_branch(cwd).await else {
        return Ok(CheckoutCommitsResult {
            base_ref: None,
            commits: vec![],
        });
    };
    let resolution = resolve_base_ref_for_cwd(cwd, ctx).await?;
    let resolved = resolution.resolved_base_ref;
    let normalized_base = resolved.as_deref().map(branch_name_from_ref);
    let mut comparison =
        try_resolve_checkout_commits_base_ref(cwd, resolved.as_deref(), &current_branch).await?;
    if comparison.is_none()
        && normalized_base
            .as_deref()
            .is_some_and(|n| !n.is_empty() && n != current_branch)
    {
        // Saved worktree metadata can outlive a renamed or deleted base branch.
        let fallback = resolve_base_ref(cwd).await?;
        comparison =
            try_resolve_checkout_commits_base_ref(cwd, fallback.as_deref(), &current_branch)
                .await?;
    }

    let mut workspace_records = Vec::new();
    let mut base_revision = "HEAD".to_string();
    if let Some(cmp) = &comparison {
        let range = format!("{cmp}..HEAD");
        let (records, merge_base) = tokio::join!(
            get_checkout_commit_records(cwd, &range, None),
            try_resolve_merge_base(cwd, cmp),
        );
        workspace_records = records?;
        base_revision = merge_base.unwrap_or_default();
    }
    let base_records = if base_revision.is_empty() {
        vec![]
    } else {
        get_checkout_commit_records(cwd, &base_revision, Some(CHECKOUT_BASE_COMMIT_LIMIT)).await?
    };
    let workspace_shas: std::collections::HashSet<String> =
        workspace_records.iter().map(|r| r.sha.clone()).collect();
    let records: Vec<ParsedCommit> = workspace_records.into_iter().chain(base_records).collect();
    if records.is_empty() {
        return Ok(CheckoutCommitsResult {
            base_ref: comparison,
            commits: vec![],
        });
    }
    let unpushed = get_unpushed_commit_shas(cwd).await?;
    let commits = records
        .into_iter()
        .map(|r| CheckoutCommit {
            is_on_remote: !unpushed.contains(&r.sha),
            is_on_base: Some(!workspace_shas.contains(&r.sha)),
            sha: r.sha,
            short_sha: r.short_sha,
            subject: r.subject,
            author_name: r.author_name,
            author_email: None,
            author_date: r.author_date,
            body: None,
            parents: None,
            refs: None,
            files: r.files,
        })
        .collect();
    Ok(CheckoutCommitsResult {
        base_ref: comparison,
        commits,
    })
}

pub(crate) const GRAPH_COMMIT_LOG_FORMAT: &str =
    "%x1e%H%x00%h%x00%P%x00%D%x00%an%x00%ae%x00%aI%x00%s%x00%b%x00";

/// Parses the graph log stream (`GRAPH_COMMIT_LOG_FORMAT` + `--raw --numstat`).
pub(crate) fn parse_graph_commit_records(
    stdout: &str,
    unpushed: &std::collections::HashSet<String>,
) -> Vec<CheckoutCommit> {
    let mut commits = Vec::new();
    for record in stdout
        .split(COMMIT_RECORD_SEPARATOR)
        .filter(|r| !r.is_empty())
    {
        let fields: Vec<&str> = record.split(COMMIT_FIELD_SEPARATOR).collect();
        if fields.len() < 9 {
            continue;
        }
        let sha = fields[0].trim();
        if sha.is_empty() {
            continue;
        }
        let parents_raw = fields[2].trim();
        let parents: Vec<String> = parents_raw.split_whitespace().map(str::to_string).collect();
        let refs: Vec<String> = fields[3]
            .trim()
            .split(',')
            .map(|r| r.trim().to_string())
            .filter(|r| !r.is_empty())
            .collect();
        let author_email = fields[5].trim();
        let body = fields[8].trim();
        let tail = fields[9..].join(&COMMIT_FIELD_SEPARATOR.to_string());
        let mut stats = OrderedMap::new();
        let mut statuses = OrderedMap::new();
        for raw in tail.split('\n') {
            let line = raw.trim();
            if line.is_empty() {
                continue;
            }
            if line.starts_with(':') {
                parse_raw_status_line(line, &mut statuses);
            } else {
                parse_numstat_line(line, &mut stats);
            }
        }
        commits.push(CheckoutCommit {
            sha: sha.to_string(),
            short_sha: fields[1].trim().to_string(),
            subject: fields[7].to_string(),
            author_name: fields[4].to_string(),
            author_email: (!author_email.is_empty()).then(|| author_email.to_string()),
            author_date: fields[6].trim().to_string(),
            body: (!body.is_empty()).then(|| body.to_string()),
            parents: Some(parents),
            refs: Some(refs),
            is_on_remote: !unpushed.contains(sha),
            is_on_base: None,
            files: files_from(&stats, &statuses),
        });
    }
    commits
}

// ---- per-file diff of one commit -------------------------------------------

type Renames = Arc<HashMap<String, String>>;

const RENAMES_CACHE_MAX: usize = 100;
const RENAMES_CACHE_TTL: Duration = Duration::from_secs(60);

type RenamesCache = Mutex<HashMap<String, (Instant, Arc<OnceCell<Renames>>)>>;

fn renames_cache() -> &'static RenamesCache {
    static CACHE: OnceLock<RenamesCache> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Discover renames across the commit before narrowing the patch to a file.
/// (`--follow` also detects copies, which would disagree with history's -M-only list.)
async fn read_commit_renames(cwd: &str, sha: &str) -> Result<Renames> {
    let out = run_git(
        &[
            "show",
            sha,
            "--format=",
            "--diff-merges=first-parent",
            "--name-status",
            "-z",
            "-M",
            "--diff-filter=R",
        ],
        &ro(cwd),
    )
    .await?;
    if out.truncated {
        return Err(GitError::other(
            "Commit renames exceeded the git output limit",
        ));
    }
    let fields: Vec<&str> = out.stdout.split('\0').collect();
    let mut renames = HashMap::new();
    let mut index = 0;
    while index + 2 < fields.len() {
        renames.insert(fields[index + 2].to_string(), fields[index + 1].to_string());
        index += 3;
    }
    Ok(Arc::new(renames))
}

async fn get_commit_renames(cwd: &str, sha: &str) -> Result<Renames> {
    static FULL_SHA: OnceLock<Regex> = OnceLock::new();
    // Symbolic refs and abbreviated ids can resolve to a different commit later.
    let full = FULL_SHA.get_or_init(|| Regex::new(r"(?i)^(?:[0-9a-f]{40}|[0-9a-f]{64})$").unwrap());
    if !full.is_match(sha) {
        return read_commit_renames(cwd, sha).await;
    }
    let key = json!([cwd, sha]).to_string();
    let cell = {
        let mut cache = renames_cache().lock().unwrap();
        let now = Instant::now();
        cache.retain(|_, (at, _)| now.duration_since(*at) < RENAMES_CACHE_TTL);
        if !cache.contains_key(&key) {
            if cache.len() >= RENAMES_CACHE_MAX {
                if let Some(oldest) = cache
                    .iter()
                    .min_by_key(|(_, (at, _))| *at)
                    .map(|(k, _)| k.clone())
                {
                    cache.remove(&oldest);
                }
            }
            cache.insert(key.clone(), (now, Arc::new(OnceCell::new())));
        }
        cache[&key].1.clone()
    };
    // Concurrent file requests share one scan; a failed scan can be retried.
    cell.get_or_try_init(|| read_commit_renames(cwd, sha))
        .await
        .cloned()
}

/// The unified diff of one file as introduced by `sha`, in the same shape the
/// diff subscription emits. Merge commits compare to their first parent.
/// `None` when the file is absent from the commit or the change is binary-only.
pub async fn get_commit_file_diff(
    cwd: &str,
    sha: &str,
    path: &str,
) -> Result<Option<ParsedDiffFile>> {
    static BINARY: OnceLock<Regex> = OnceLock::new();
    let root = require_git_worktree_root(cwd).await?;
    let renames = get_commit_renames(&root, sha).await?;
    let paths: Vec<String> = match renames.get(path) {
        Some(old) => vec![old.clone(), path.to_string()],
        None => vec![path.to_string()],
    };
    let mut a = argv!["show", sha, "--format=", "--diff-merges=first-parent", "-M"];
    a.extend(GIT_PATCH_FORMAT_ARGS.iter().map(|s| s.to_string()));
    a.push("--".into());
    a.extend(paths.iter().map(|p| format!(":(literal){p}")));
    let out = run_git(&a, &ro(&root)).await?;
    if out.stdout.trim().is_empty() {
        return Ok(None);
    }
    let files = parse_diff(&out.stdout);
    // Including the rename source lets Git pair it with the destination.
    let Some(file) = files.into_iter().find(|f| f.path == path) else {
        return Ok(None);
    };
    // Binary changes carry a "Binary files ... differ" marker and no hunks.
    let binary = BINARY.get_or_init(|| Regex::new(r"(?m)^Binary files .* differ$").unwrap());
    if file.hunks.is_empty() && binary.is_match(&out.stdout) {
        return Ok(None);
    }
    Ok(Some(file))
}

// ---- wire payloads -----------------------------------------------------------

pub fn build_commits_list_payload(
    cwd: &str,
    request_id: &str,
    result: &Result<CheckoutCommitsResult>,
) -> Value {
    match result {
        Ok(r) => {
            json!({"cwd": cwd, "baseRef": r.base_ref, "commits": r.commits, "error": null, "requestId": request_id})
        }
        Err(e) => {
            json!({"cwd": cwd, "baseRef": null, "commits": [], "error": to_checkout_error(e), "requestId": request_id})
        }
    }
}

/// Request validation done by the TS handler before reading the diff.
pub fn validate_commit_file_diff_request(sha: &str, path: &str) -> Result<()> {
    crate::branches::assert_safe_git_ref(sha, "commit")?;
    if path.is_empty()
        || path.starts_with('/')
        || path.split(['/', '\\']).any(|p| p == "..")
        || std::path::Path::new(path).is_absolute()
    {
        return Err(GitError::other(format!("Invalid path: {path}")));
    }
    Ok(())
}

pub fn build_commit_file_diff_payload(
    cwd: &str,
    sha: &str,
    path: &str,
    request_id: &str,
    result: &Result<Option<ParsedDiffFile>>,
) -> Value {
    match result {
        Ok(file) => {
            json!({"cwd": cwd, "sha": sha, "path": path, "file": file, "error": null, "requestId": request_id})
        }
        Err(e) => {
            json!({"cwd": cwd, "sha": sha, "path": path, "file": null, "error": to_checkout_error(e), "requestId": request_id})
        }
    }
}
