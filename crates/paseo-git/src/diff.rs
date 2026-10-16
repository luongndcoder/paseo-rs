//! Checkout diff (`getCheckoutDiff`): tracked + untracked changes against HEAD
//! or the base branch, as raw text and as structured `ParsedDiffFile`s.
//!
//! Behavior differences vs TS: structured files carry no syntax-highlight
//! `tokens` (the highlighter lives outside this crate); the wire field is optional.

use std::collections::HashMap;
use std::sync::OnceLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::diff_parse::{parse_diff, split_diff_sections, DiffFileStatus, ParsedDiffFile};
use crate::error::{GitError, Result};
use crate::paths::{js_str_cmp, unquote_git_path};
use crate::repo::{
    require_git_worktree_root, resolve_base_ref_for_cwd, resolve_best_comparison_base_ref,
    resolve_operation_base_ref, ro, try_resolve_merge_base, CheckoutContext,
};
use crate::run::{run_git, GitOptions};
use crate::{args, argv};

/// Parsed patches need a stable wire format regardless of terminal settings.
pub const GIT_PATCH_FORMAT_ARGS: [&str; 4] = [
    "--no-ext-diff",
    "--no-color",
    "--src-prefix=a/",
    "--dst-prefix=b/",
];

pub const PER_FILE_DIFF_MAX_BYTES: usize = 1024 * 1024;
pub const TOTAL_DIFF_MAX_BYTES: usize = 2 * 1024 * 1024;
const TRACKED_DIFF_NUMSTAT_MAX_BYTES: usize = 2 * 1024 * 1024;
const TRACKED_DIFF_BATCH_SIZE: usize = 64;
const EMPTY_TREE_OBJECT_ID: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
const UNTRACKED_BINARY_SNIFF_BYTES: usize = 16 * 1024;
const DISCARD_CHANGES_TIMEOUT_MS: u64 = 120_000;

// The relay's 32 MiB frame limit is binding: string frames are encrypted and
// base64-encoded (40 bytes overhead). Reserve 1 MiB for the JSON envelope.
const MAX_RELAY_WEBSOCKET_MESSAGE_BYTES: usize = 32 * 1024 * 1024;
const ENCRYPTED_PAYLOAD_OVERHEAD_BYTES: usize = 40;
const CHECKOUT_DIFF_FRAME_HEADROOM_BYTES: usize = 1024 * 1024;
pub const CHECKOUT_DIFF_MAX_STRUCTURED_BYTES: usize = (MAX_RELAY_WEBSOCKET_MESSAGE_BYTES / 4) * 3
    - ENCRYPTED_PAYLOAD_OVERHEAD_BYTES
    - CHECKOUT_DIFF_FRAME_HEADROOM_BYTES;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiffMode {
    Uncommitted,
    Base,
}

impl DiffMode {
    pub fn as_str(self) -> &'static str {
        match self {
            DiffMode::Uncommitted => "uncommitted",
            DiffMode::Base => "base",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckoutDiffCompare {
    pub mode: DiffMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignore_whitespace: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_structured: Option<bool>,
}

impl CheckoutDiffCompare {
    pub fn uncommitted() -> Self {
        Self {
            mode: DiffMode::Uncommitted,
            base_ref: None,
            ignore_whitespace: None,
            include_structured: None,
        }
    }
    pub fn base(base_ref: Option<&str>) -> Self {
        Self {
            mode: DiffMode::Base,
            base_ref: base_ref.map(str::to_string),
            ignore_whitespace: None,
            include_structured: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckoutDiffResult {
    pub diff: String,
    pub structured: Option<Vec<ParsedDiffFile>>,
    pub diff_too_large: bool,
}

impl CheckoutDiffResult {
    fn plain(diff: String) -> Self {
        Self {
            diff,
            structured: None,
            diff_too_large: false,
        }
    }
    fn too_large() -> Self {
        Self {
            diff: String::new(),
            structured: Some(vec![]),
            diff_too_large: true,
        }
    }
}

#[derive(Debug, Clone)]
struct CheckoutFileChange {
    path: String,
    old_path: Option<String>,
    status: String,
    is_new: bool,
    is_deleted: bool,
    is_untracked: bool,
}

#[derive(Debug, Clone)]
struct CheckoutDiffRefs {
    base_ref: String,
    target_ref: Option<String>,
    include_untracked: bool,
}

impl CheckoutDiffRefs {
    fn ref_args(&self) -> Vec<String> {
        let mut v = vec![self.base_ref.clone()];
        v.extend(self.target_ref.clone());
        v
    }
}

#[derive(Debug, Clone, Copy)]
struct FileStat {
    additions: u64,
    deletions: u64,
    is_binary: bool,
}

pub(crate) fn build_git_diff_args(
    ignore_whitespace: bool,
    extra: impl IntoIterator<Item = String>,
) -> Vec<String> {
    let mut a = vec!["diff".to_string()];
    a.extend(GIT_PATCH_FORMAT_ARGS.iter().map(|s| s.to_string()));
    if ignore_whitespace {
        a.push("-w".into());
    }
    a.extend(extra);
    a
}

pub(crate) fn normalize_numstat_path(path_field: &str) -> String {
    static QUOTED: OnceLock<Regex> = OnceLock::new();
    static BRACE: OnceLock<Regex> = OnceLock::new();
    static INLINE: OnceLock<Regex> = OnceLock::new();
    let quoted = QUOTED.get_or_init(|| Regex::new(r#"^"(?:\\.|[^"\\])*"$"#).unwrap());
    if quoted.is_match(path_field) {
        return unquote_git_path(path_field);
    }
    let brace = BRACE.get_or_init(|| Regex::new(r"^(.*)\{(.*) => (.*)\}(.*)$").unwrap());
    if let Some(c) = brace.captures(path_field) {
        return unquote_git_path(&format!("{}{}{}", &c[1], &c[3], &c[4]));
    }
    let inline = INLINE.get_or_init(|| Regex::new(r"^(.*) => (.*)$").unwrap());
    if let Some(c) = inline.captures(path_field) {
        return unquote_git_path(&c[2]);
    }
    unquote_git_path(path_field)
}

fn is_unborn_head_diff_error(error: &GitError) -> bool {
    let m = error.to_string();
    m.contains("--name-status HEAD") && m.contains("ambiguous argument 'HEAD'")
}

async fn list_checkout_file_changes(
    cwd: &str,
    refs: &CheckoutDiffRefs,
    ignore_whitespace: bool,
) -> Result<Vec<CheckoutFileChange>> {
    let mut extra = argv!["-z", "--name-status"];
    extra.extend(refs.ref_args());
    let out = run_git(&build_git_diff_args(ignore_whitespace, extra), &ro(cwd)).await?;
    let fields: Vec<&str> = out.stdout.split('\0').collect();
    let mut changes: Vec<CheckoutFileChange> = Vec::new();
    let mut i = 0;
    while i < fields.len() {
        let raw_status = fields[i];
        i += 1;
        if raw_status.is_empty() {
            continue;
        }
        if raw_status.starts_with('R') || raw_status.starts_with('C') {
            let old_path = fields.get(i).copied().unwrap_or("");
            let new_path = fields.get(i + 1).copied().unwrap_or("");
            i += 2;
            if !new_path.is_empty() {
                changes.push(CheckoutFileChange {
                    path: new_path.to_string(),
                    old_path: (!old_path.is_empty()).then(|| old_path.to_string()),
                    status: raw_status.to_string(),
                    is_new: false,
                    is_deleted: false,
                    is_untracked: false,
                });
            }
            continue;
        }
        let path = fields.get(i).copied().unwrap_or("");
        i += 1;
        if path.is_empty() {
            continue;
        }
        let code = raw_status.chars().next();
        changes.push(CheckoutFileChange {
            path: path.to_string(),
            old_path: None,
            status: raw_status.to_string(),
            is_new: code == Some('A'),
            is_deleted: code == Some('D'),
            is_untracked: false,
        });
    }
    if refs.include_untracked {
        let out = run_git(
            &["ls-files", "-z", "--others", "--exclude-standard"],
            &ro(cwd),
        )
        .await?;
        for file in out.stdout.split('\0').filter(|f| !f.is_empty()) {
            changes.push(CheckoutFileChange {
                path: file.to_string(),
                old_path: None,
                status: "U".into(),
                is_new: true,
                is_deleted: false,
                is_untracked: true,
            });
        }
    }
    // Deduplicate by path (prefer tracked over the untracked marker), keeping first position.
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut deduped: Vec<CheckoutFileChange> = Vec::new();
    for change in changes {
        match index.get(&change.path) {
            None => {
                index.insert(change.path.clone(), deduped.len());
                deduped.push(change);
            }
            Some(&pos) => {
                if deduped[pos].is_untracked && !change.is_untracked {
                    deduped[pos] = change;
                }
            }
        }
    }
    Ok(deduped)
}

async fn get_tracked_numstat_by_path(
    cwd: &str,
    refs: &CheckoutDiffRefs,
    ignore_whitespace: bool,
) -> Result<HashMap<String, Option<FileStat>>> {
    let mut extra = argv!["-z", "--numstat"];
    extra.extend(refs.ref_args());
    let out = run_git(
        &build_git_diff_args(ignore_whitespace, extra),
        &ro(cwd)
            .max_output(TRACKED_DIFF_NUMSTAT_MAX_BYTES)
            .accept(&[0]),
    )
    .await?;
    let records: Vec<&str> = out.stdout.split('\0').collect();
    let mut stats = HashMap::new();
    let mut index = 0;
    while index < records.len() {
        let parts: Vec<&str> = records[index].split('\t').collect();
        if parts.len() < 3 {
            index += 1;
            continue;
        }
        let additions_field = parts[0];
        let deletions_field = parts[1];
        let mut path = parts[2..].join("\t");
        if path.is_empty() {
            // A rename has an empty path here, followed by separate old/new NUL fields.
            path = records.get(index + 2).copied().unwrap_or("").to_string();
            index += 2;
        }
        index += 1;
        if path.is_empty() {
            continue;
        }
        if additions_field == "-" || deletions_field == "-" {
            stats.insert(
                path,
                Some(FileStat {
                    additions: 0,
                    deletions: 0,
                    is_binary: true,
                }),
            );
            continue;
        }
        match (
            additions_field.parse::<u64>(),
            deletions_field.parse::<u64>(),
        ) {
            (Ok(a), Ok(d)) => stats.insert(
                path,
                Some(FileStat {
                    additions: a,
                    deletions: d,
                    is_binary: false,
                }),
            ),
            _ => stats.insert(path, None),
        };
    }
    Ok(stats)
}

#[derive(Debug, Clone)]
struct TrackedDiffText {
    path: String,
    text: String,
    truncated: bool,
}

fn header_of(section: &str) -> &str {
    match section.find("\n@@ ") {
        Some(i) => &section[..i],
        None => section,
    }
}

async fn get_tracked_diff_text_for_path(
    cwd: &str,
    refs: &CheckoutDiffRefs,
    path: &str,
    old_path: Option<&str>,
    ignore_whitespace: bool,
) -> Result<TrackedDiffText> {
    let paths: Vec<&str> = match old_path {
        Some(o) => vec![o, path],
        None => vec![path],
    };
    let mut extra = vec![];
    if old_path.is_some() {
        extra.push("--diff-filter=RC".to_string());
    }
    extra.extend(refs.ref_args());
    extra.push("--".into());
    extra.extend(paths.iter().map(|p| format!(":(literal){p}")));
    let out = run_git(
        &build_git_diff_args(ignore_whitespace, extra),
        &ro(cwd).max_output(PER_FILE_DIFF_MAX_BYTES),
    )
    .await?;
    // A source path can take part in another rename/copy in the same request;
    // keep only the requested destination.
    let mut text = out.stdout.clone();
    if old_path.is_some() && !out.truncated {
        text = split_diff_sections(&out.stdout)
            .into_iter()
            .filter(|s| {
                parse_diff(header_of(s))
                    .first()
                    .is_some_and(|f| f.path == path)
            })
            .collect::<String>();
    }
    Ok(TrackedDiffText {
        path: path.to_string(),
        text,
        truncated: out.truncated,
    })
}

fn get_tracked_diff_texts<'a>(
    cwd: &'a str,
    refs: &'a CheckoutDiffRefs,
    paths: &'a [String],
    ignore_whitespace: bool,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<TrackedDiffText>>> + Send + 'a>>
{
    Box::pin(async move {
        if paths.is_empty() {
            return Ok(vec![]);
        }
        if paths.len() == 1 {
            return Ok(vec![
                get_tracked_diff_text_for_path(cwd, refs, &paths[0], None, ignore_whitespace)
                    .await?,
            ]);
        }
        let mut extra = refs.ref_args();
        extra.push("--".into());
        extra.extend(paths.iter().map(|p| format!(":(literal){p}")));
        let out = run_git(
            &build_git_diff_args(ignore_whitespace, extra),
            &ro(cwd).max_output(8 * PER_FILE_DIFF_MAX_BYTES),
        )
        .await?;
        let mut files: HashMap<String, TrackedDiffText> = HashMap::new();
        if !out.truncated {
            for text in split_diff_sections(&out.stdout) {
                // Only identify the file here. Oversized bodies must not reach the parser.
                let parsed = parse_diff(header_of(text));
                let Some(file) = parsed.first() else { break };
                if !paths.contains(&file.path) {
                    break;
                }
                files.insert(
                    file.path.clone(),
                    TrackedDiffText {
                        path: file.path.clone(),
                        text: text.to_string(),
                        truncated: text.len() > PER_FILE_DIFF_MAX_BYTES,
                    },
                );
            }
            // Empty patches are valid when whitespace is ignored. Every emitted
            // section must be accounted for before accepting a combined command.
            if files.values().map(|f| f.text.len()).sum::<usize>() == out.stdout.len() {
                return Ok(paths
                    .iter()
                    .map(|p| {
                        files.get(p).cloned().unwrap_or(TrackedDiffText {
                            path: p.clone(),
                            text: String::new(),
                            truncated: false,
                        })
                    })
                    .collect());
            }
        }
        // Bound oversized output without letting one large file hide its neighbors.
        let middle = paths.len().div_ceil(2);
        let mut left =
            get_tracked_diff_texts(cwd, refs, &paths[..middle], ignore_whitespace).await?;
        left.extend(get_tracked_diff_texts(cwd, refs, &paths[middle..], ignore_whitespace).await?);
        Ok(left)
    })
}

// ---- untracked files ---------------------------------------------------------

async fn is_likely_binary_file(path: &str) -> std::io::Result<bool> {
    use tokio::io::AsyncReadExt;
    let mut file = tokio::fs::File::open(path).await?;
    let mut buf = vec![0u8; UNTRACKED_BINARY_SNIFF_BYTES];
    let n = file.read(&mut buf).await?;
    if n == 0 {
        return Ok(false);
    }
    let mut suspicious = 0usize;
    for &b in &buf[..n] {
        if b == 0 {
            return Ok(true);
        }
        if b < 7 || (b > 14 && b < 32) || b == 127 {
            suspicious += 1;
        }
    }
    Ok(suspicious as f64 / n as f64 > 0.3)
}

async fn inspect_untracked_file(
    cwd: &str,
    relative: &str,
) -> std::io::Result<(Option<FileStat>, bool)> {
    let absolute = crate::paths::node_resolve(cwd, relative);
    let metadata = tokio::fs::metadata(&absolute).await?;
    if !metadata.is_file() {
        return Ok((None, false));
    }
    if is_likely_binary_file(&absolute).await? {
        return Ok((
            Some(FileStat {
                additions: 0,
                deletions: 0,
                is_binary: true,
            }),
            false,
        ));
    }
    let stat = Some(FileStat {
        additions: 0,
        deletions: 0,
        is_binary: false,
    });
    Ok((stat, metadata.len() as usize > PER_FILE_DIFF_MAX_BYTES))
}

async fn get_untracked_diff_text(
    cwd: &str,
    change: &CheckoutFileChange,
    ignore_whitespace: bool,
) -> Result<(String, bool, Option<FileStat>)> {
    // If metadata probing fails fall through to the git diff path.
    if let Ok((stat, truncated)) = inspect_untracked_file(cwd, &change.path).await {
        if stat.is_some_and(|s| s.is_binary) || truncated {
            return Ok((String::new(), truncated, stat));
        }
    }
    let out = run_git(
        &build_git_diff_args(
            ignore_whitespace,
            args!["--no-index", "/dev/null", "--", change.path.as_str()],
        ),
        &ro(cwd).max_output(PER_FILE_DIFF_MAX_BYTES).accept(&[0, 1]),
    )
    .await?;
    Ok((
        out.stdout,
        out.truncated,
        Some(FileStat {
            additions: 0,
            deletions: 0,
            is_binary: false,
        }),
    ))
}

// ---- accumulation --------------------------------------------------------------

struct StructuredAccumulator {
    files: Vec<ParsedDiffFile>,
    serialized_bytes: usize,
}

impl StructuredAccumulator {
    fn new() -> Self {
        Self {
            files: vec![],
            serialized_bytes: 2,
        }
    }

    fn append(&mut self, file: ParsedDiffFile) -> bool {
        let separator = usize::from(!self.files.is_empty());
        let file_bytes = serde_json::to_vec(&file).map_or(0, |v| v.len());
        let next = self.serialized_bytes + separator + file_bytes;
        if next > CHECKOUT_DIFF_MAX_STRUCTURED_BYTES {
            return false;
        }
        self.files.push(file);
        self.serialized_bytes = next;
        true
    }
}

struct DiffBuffer {
    text: String,
    bytes: usize,
}

impl DiffBuffer {
    fn append(&mut self, text: &str) {
        if text.is_empty() || self.bytes >= TOTAL_DIFF_MAX_BYTES {
            return;
        }
        if self.bytes + text.len() <= TOTAL_DIFF_MAX_BYTES {
            self.text.push_str(text);
            self.bytes += text.len();
            return;
        }
        let remaining = TOTAL_DIFF_MAX_BYTES - self.bytes;
        if remaining > 0 {
            // Node decodes the cut buffer, replacing a split multibyte tail.
            self.text
                .push_str(&String::from_utf8_lossy(&text.as_bytes()[..remaining]));
            self.bytes = TOTAL_DIFF_MAX_BYTES;
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Placeholder {
    Binary,
    TooLarge,
}

fn placeholder_file(
    change: &CheckoutFileChange,
    status: Placeholder,
    stat: Option<FileStat>,
) -> ParsedDiffFile {
    ParsedDiffFile {
        path: change.path.clone(),
        old_path: change.old_path.clone(),
        is_new: change.is_new,
        is_deleted: change.is_deleted,
        additions: stat.map_or(0, |s| s.additions),
        deletions: stat.map_or(0, |s| s.deletions),
        staged: None,
        unstaged: None,
        hunks: vec![],
        status: Some(match status {
            Placeholder::Binary => DiffFileStatus::Binary,
            Placeholder::TooLarge => DiffFileStatus::TooLarge,
        }),
    }
}

fn placeholder_comment(change: &CheckoutFileChange, status: Placeholder) -> String {
    match status {
        Placeholder::Binary => format!("# {}: binary diff omitted\n", change.path),
        Placeholder::TooLarge => format!("# {}: diff too large omitted\n", change.path),
    }
}

struct TrackedOutcome {
    numstat: HashMap<String, Option<FileStat>>,
    placeholders: HashMap<String, (Placeholder, Option<FileStat>)>,
    diff_text: String,
}

async fn process_tracked_changes(
    cwd: &str,
    refs: &CheckoutDiffRefs,
    tracked: &[CheckoutFileChange],
    ignore_whitespace: bool,
    buffer: &mut DiffBuffer,
) -> Result<TrackedOutcome> {
    let numstat = if tracked.is_empty() {
        HashMap::new()
    } else {
        get_tracked_numstat_by_path(cwd, refs, ignore_whitespace).await?
    };
    let mut diff_paths: Vec<String> = vec![];
    let mut placeholders: HashMap<String, (Placeholder, Option<FileStat>)> = HashMap::new();
    for change in tracked {
        let stat = numstat.get(&change.path).copied().flatten();
        if stat.is_some_and(|s| s.is_binary) {
            placeholders.insert(change.path.clone(), (Placeholder::Binary, stat));
            continue;
        }
        diff_paths.push(change.path.clone());
    }
    let mut diff_text = String::new();
    let mut diff_bytes = 0usize;
    let old_path_by_path: HashMap<&str, &str> = tracked
        .iter()
        .filter_map(|c| c.old_path.as_deref().map(|o| (c.path.as_str(), o)))
        .collect();
    for batch in diff_paths.chunks(TRACKED_DIFF_BATCH_SIZE) {
        // Rename/copy detection needs both paths.
        let ordinary_paths: Vec<String> = batch
            .iter()
            .filter(|p| !old_path_by_path.contains_key(p.as_str()))
            .cloned()
            .collect();
        let ordinary =
            get_tracked_diff_texts(cwd, refs, &ordinary_paths, ignore_whitespace).await?;
        let mut by_path: HashMap<String, TrackedDiffText> =
            ordinary.into_iter().map(|f| (f.path.clone(), f)).collect();
        for path in batch
            .iter()
            .filter(|p| old_path_by_path.contains_key(p.as_str()))
        {
            let f = get_tracked_diff_text_for_path(
                cwd,
                refs,
                path,
                old_path_by_path.get(path.as_str()).copied(),
                ignore_whitespace,
            )
            .await?;
            by_path.insert(f.path.clone(), f);
        }
        for path in batch {
            let file = &by_path[path];
            let stat = numstat.get(&file.path).copied().flatten();
            if file.truncated || diff_bytes + file.text.len() > TOTAL_DIFF_MAX_BYTES {
                placeholders.insert(file.path.clone(), (Placeholder::TooLarge, stat));
                continue;
            }
            diff_bytes += file.text.len();
            diff_text.push_str(&file.text);
        }
    }
    buffer.append(&diff_text);
    Ok(TrackedOutcome {
        numstat,
        placeholders,
        diff_text,
    })
}

fn is_whitespace_only_tracked_change(
    change: &CheckoutFileChange,
    stat: Option<FileStat>,
    ignore_whitespace: bool,
) -> bool {
    ignore_whitespace
        && change.status.starts_with('M')
        && stat.is_none_or(|s| !s.is_binary && s.additions == 0 && s.deletions == 0)
}

async fn append_structured_tracked_diffs(
    tracked: &[CheckoutFileChange],
    outcome: &TrackedOutcome,
    ignore_whitespace: bool,
    structured: &mut StructuredAccumulator,
    buffer: &mut DiffBuffer,
) -> bool {
    let parsed: HashMap<String, ParsedDiffFile> = if outcome.diff_text.is_empty() {
        HashMap::new()
    } else {
        parse_diff(&outcome.diff_text)
            .into_iter()
            .map(|f| (f.path.clone(), f))
            .collect()
    };
    for batch in tracked.chunks(TRACKED_DIFF_BATCH_SIZE) {
        for change in batch {
            if let Some((status, stat)) = outcome.placeholders.get(&change.path) {
                if !structured.append(placeholder_file(change, *status, *stat)) {
                    return false;
                }
                buffer.append(&placeholder_comment(change, *status));
                continue;
            }
            let stat = outcome.numstat.get(&change.path).copied().flatten();
            if let Some(parsed_file) = parsed.get(&change.path) {
                let mut file = parsed_file.clone();
                file.path = change.path.clone();
                file.old_path = change.old_path.clone();
                file.is_new = change.is_new;
                file.is_deleted = change.is_deleted;
                file.status = Some(DiffFileStatus::Ok);
                if !structured.append(file) {
                    return false;
                }
                // Keep serving other daemon work between large batches.
                tokio::task::yield_now().await;
                continue;
            }
            // `git diff -w --name-status` can report a modified path even when the
            // whitespace-filtered patch and numstat are both empty; skip it so
            // whitespace-only edits disappear.
            if is_whitespace_only_tracked_change(change, stat, ignore_whitespace) {
                continue;
            }
            let file = ParsedDiffFile {
                path: change.path.clone(),
                old_path: change.old_path.clone(),
                is_new: change.is_new,
                is_deleted: change.is_deleted,
                additions: stat.map_or(0, |s| s.additions),
                deletions: stat.map_or(0, |s| s.deletions),
                staged: None,
                unstaged: None,
                hunks: vec![],
                status: Some(DiffFileStatus::Ok),
            };
            if !structured.append(file) {
                return false;
            }
        }
    }
    true
}

async fn process_untracked_change(
    cwd: &str,
    change: &CheckoutFileChange,
    ignore_whitespace: bool,
    include_structured: bool,
    structured: &mut StructuredAccumulator,
    buffer: &mut DiffBuffer,
) -> Result<bool> {
    let (text, truncated, stat) = get_untracked_diff_text(cwd, change, ignore_whitespace).await?;
    let binary = stat.is_some_and(|s| s.is_binary);
    if !include_structured {
        if binary {
            buffer.append(&placeholder_comment(change, Placeholder::Binary));
        } else if truncated {
            buffer.append(&placeholder_comment(change, Placeholder::TooLarge));
        } else {
            buffer.append(&text);
        }
        return Ok(true);
    }
    if binary {
        if !structured.append(placeholder_file(change, Placeholder::Binary, stat)) {
            return Ok(false);
        }
        buffer.append(&placeholder_comment(change, Placeholder::Binary));
        return Ok(true);
    }
    if truncated {
        if !structured.append(placeholder_file(change, Placeholder::TooLarge, stat)) {
            return Ok(false);
        }
        buffer.append(&placeholder_comment(change, Placeholder::TooLarge));
        return Ok(true);
    }
    buffer.append(&text);
    let mut file = parse_diff(&text)
        .into_iter()
        .next()
        .unwrap_or(ParsedDiffFile {
            path: change.path.clone(),
            old_path: None,
            is_new: change.is_new,
            is_deleted: change.is_deleted,
            additions: stat.map_or(0, |s| s.additions),
            deletions: stat.map_or(0, |s| s.deletions),
            staged: None,
            unstaged: None,
            hunks: vec![],
            status: None,
        });
    file.path = change.path.clone();
    file.is_new = change.is_new;
    file.is_deleted = change.is_deleted;
    file.status = Some(DiffFileStatus::Ok);
    Ok(structured.append(file))
}

async fn resolve_checkout_diff_refs(
    cwd: &str,
    compare: &CheckoutDiffCompare,
    ctx: &CheckoutContext,
) -> Result<Option<CheckoutDiffRefs>> {
    if compare.mode == DiffMode::Uncommitted {
        return Ok(Some(CheckoutDiffRefs {
            base_ref: "HEAD".into(),
            target_ref: None,
            include_untracked: true,
        }));
    }
    let r = resolve_base_ref_for_cwd(cwd, ctx).await?;
    let base_ref = resolve_operation_base_ref(
        r.stored_base_ref.as_deref(),
        r.resolved_base_ref.as_deref(),
        compare.base_ref.as_deref(),
    )?;
    let Some(base_ref) = base_ref else {
        return Ok(None);
    };
    let best = resolve_best_comparison_base_ref(cwd, &base_ref).await?;
    Ok(Some(CheckoutDiffRefs {
        base_ref: try_resolve_merge_base(cwd, &best).await.unwrap_or(best),
        target_ref: Some("HEAD".into()),
        include_untracked: false,
    }))
}

pub async fn get_checkout_diff(
    cwd: &str,
    compare: &CheckoutDiffCompare,
    ctx: &CheckoutContext,
) -> Result<CheckoutDiffResult> {
    let cwd = require_git_worktree_root(cwd).await?;
    let cwd = cwd.as_str();
    let Some(refs) = resolve_checkout_diff_refs(cwd, compare, ctx).await? else {
        return Ok(CheckoutDiffResult::plain(String::new()));
    };
    let ignore_whitespace = compare.ignore_whitespace == Some(true);
    let include_structured = compare.include_structured == Some(true);
    let mut effective = refs.clone();
    let mut changes = match list_checkout_file_changes(cwd, &effective, ignore_whitespace).await {
        Ok(c) => c,
        Err(e) if is_unborn_head_diff_error(&e) => {
            effective = CheckoutDiffRefs {
                base_ref: EMPTY_TREE_OBJECT_ID.into(),
                ..refs
            };
            list_checkout_file_changes(cwd, &effective, ignore_whitespace).await?
        }
        Err(e) => return Err(e),
    };
    changes.sort_by(|a, b| js_str_cmp(&a.path, &b.path));

    let mut structured = StructuredAccumulator::new();
    let mut buffer = DiffBuffer {
        text: String::new(),
        bytes: 0,
    };
    let tracked: Vec<CheckoutFileChange> = changes
        .iter()
        .filter(|c| !c.is_untracked)
        .cloned()
        .collect();
    let untracked: Vec<CheckoutFileChange> =
        changes.iter().filter(|c| c.is_untracked).cloned().collect();
    let outcome =
        process_tracked_changes(cwd, &effective, &tracked, ignore_whitespace, &mut buffer).await?;

    if include_structured {
        if !append_structured_tracked_diffs(
            &tracked,
            &outcome,
            ignore_whitespace,
            &mut structured,
            &mut buffer,
        )
        .await
        {
            return Ok(CheckoutDiffResult::too_large());
        }
    } else {
        for change in &tracked {
            if let Some((status, _)) = outcome.placeholders.get(&change.path) {
                buffer.append(&placeholder_comment(change, *status));
            }
        }
    }

    for change in &untracked {
        if buffer.bytes >= TOTAL_DIFF_MAX_BYTES {
            break;
        }
        if !process_untracked_change(
            cwd,
            change,
            ignore_whitespace,
            include_structured,
            &mut structured,
            &mut buffer,
        )
        .await?
        {
            return Ok(CheckoutDiffResult::too_large());
        }
    }

    if include_structured {
        if compare.mode == DiffMode::Uncommitted {
            annotate_uncommitted_stage_status(cwd, &mut structured.files).await;
        }
        return Ok(CheckoutDiffResult {
            diff: buffer.text,
            structured: Some(structured.files),
            diff_too_large: false,
        });
    }
    Ok(CheckoutDiffResult::plain(buffer.text))
}

async fn annotate_uncommitted_stage_status(cwd: &str, files: &mut [ParsedDiffFile]) {
    if files.is_empty() {
        return;
    }
    // Non-fatal: staged/unstaged stay unset if the status check fails.
    let Ok(out) = run_git(
        &["status", "--porcelain=v1", "-z", "-uall"],
        &GitOptions::new(cwd)
            .read_only()
            .timeout_ms(DISCARD_CHANGES_TIMEOUT_MS),
    )
    .await
    else {
        return;
    };
    let mut stage_map: HashMap<String, (bool, bool)> = HashMap::new();
    let tokens: Vec<&str> = out.stdout.split('\0').collect();
    let mut i = 0;
    while i < tokens.len() {
        let token = tokens[i];
        i += 1;
        if token.len() < 4 {
            continue;
        }
        let x = token.as_bytes()[0] as char;
        let y = token.as_bytes()[1] as char;
        let Some(file_path) = token.get(3..) else {
            continue;
        };
        if x == 'R' || x == 'C' {
            i += 1;
        }
        let staged = x != ' ' && x != '?' && x != '!';
        let unstaged = y != ' ' && y != '!';
        stage_map.insert(file_path.to_string(), (staged, unstaged));
    }
    for file in files {
        match stage_map.get(&file.path) {
            Some(&(staged, unstaged)) => {
                file.staged = Some(staged);
                file.unstaged = Some(unstaged);
            }
            None => {
                file.staged = Some(false);
                file.unstaged = Some(true);
            }
        }
    }
}
