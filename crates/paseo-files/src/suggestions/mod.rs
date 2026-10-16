//! Directory and file suggestion engine (port of `searchDirectoryEntries`).
//!
//! Discovery and retrieval filter differently, on purpose. Discovery, which
//! ranks or browses candidates the caller has not named, drops gitignored and
//! hidden entries so pickers do not offer build output. Retrieval of a path the
//! caller named exactly applies no ignore or hidden filtering; the only
//! question is whether it stays inside the root.
//!
//! Not ported: the cross-request directory listing cache and the libuv
//! threadpool limiter. Both are performance details of the Node runtime.

mod fs_read;
mod query;
mod rank;
mod walk;

pub use crate::wire::{DirectorySuggestionEntry, MatchMode, SuggestionKind};

use crate::path_utils::{is_path_inside_root, normalize_relative_path, resolve, resolve_cwd};
use fs_read::{kind_of, load_git_ignored_paths};
use query::{parse_query, QueryPlan};
use rank::{compare_rank, has_confident_result, suggest_rank, RankedEntry};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use walk::{
    child_branches, is_git_ignored_path, is_inside_excluded_path, should_discover, RoundRobin,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathFormat {
    Absolute,
    Relative,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathQueryPolicy {
    Rooted,
    Slashes,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlankQueryBehavior {
    None,
    Children,
}

pub const WORKSPACE_SEARCH_HIDDEN_DIRECTORIES: [&str; 8] = [
    ".agents",
    ".claude",
    ".codex",
    ".cursor",
    ".github",
    ".opencode",
    ".paseo",
    ".vscode",
];

const DEFAULT_LIMIT: i64 = 30;
const MAX_LIMIT: i64 = 100;
const DEFAULT_MAX_DEPTH: usize = 12;
const DEFAULT_MAX_ENTRIES_SCANNED: u64 = 20_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Kind {
    File,
    Directory,
}

impl From<Kind> for SuggestionKind {
    fn from(k: Kind) -> Self {
        match k {
            Kind::File => SuggestionKind::File,
            Kind::Directory => SuggestionKind::Directory,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ChildEntry {
    pub name: String,
    pub resolved: PathBuf,
    pub kind: Kind,
    pub via_symlink: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct Traversed {
    pub name: String,
    pub resolved: PathBuf,
    pub kind: Kind,
    pub visible: PathBuf,
    pub depth: usize,
}

#[derive(Debug, Clone)]
pub struct SearchOptions {
    pub root: String,
    pub query: String,
    pub path_format: PathFormat,
    /// Default false.
    pub include_files: Option<bool>,
    /// Default true.
    pub include_directories: Option<bool>,
    /// Default fuzzy.
    pub match_mode: Option<MatchMode>,
    /// Default slashes.
    pub path_query_policy: Option<PathQueryPolicy>,
    pub root_aliases: Vec<String>,
    /// Default none.
    pub blank_query_behavior: Option<BlankQueryBehavior>,
    pub traversable_hidden_directory_names: Vec<String>,
    /// Truncated and clamped to 1..=100; default 30.
    pub limit: Option<i64>,
    pub max_depth: Option<usize>,
    pub max_entries_scanned: Option<u64>,
    /// 0 disables early exit.
    pub confident_result_scan_threshold: Option<u64>,
    pub respect_git_ignore: bool,
    /// Absolute paths discovery never walks into. A path typed explicitly still
    /// resolves, and browsing inside one lists a single level.
    pub excluded_discovery_paths: Vec<String>,
}

impl SearchOptions {
    pub fn new(root: impl Into<String>, query: impl Into<String>, path_format: PathFormat) -> Self {
        Self {
            root: root.into(),
            query: query.into(),
            path_format,
            include_files: None,
            include_directories: None,
            match_mode: None,
            path_query_policy: None,
            root_aliases: Vec::new(),
            blank_query_behavior: None,
            traversable_hidden_directory_names: Vec::new(),
            limit: None,
            max_depth: None,
            max_entries_scanned: None,
            confident_result_scan_threshold: None,
            respect_git_ignore: false,
            excluded_discovery_paths: Vec::new(),
        }
    }
}

pub(crate) struct SearchInput {
    pub root: PathBuf,
    pub plan: QueryPlan,
    pub include_directories: bool,
    pub include_files: bool,
    pub match_mode: MatchMode,
    pub path_query_policy: PathQueryPolicy,
    pub path_format: PathFormat,
    pub hidden_directory_names: HashSet<String>,
    pub limit: usize,
    pub max_depth: usize,
    pub max_entries_scanned: u64,
    pub confident_result_scan_threshold: Option<u64>,
    pub git_ignored_paths: std::sync::Arc<HashSet<PathBuf>>,
    pub excluded_discovery_paths: HashSet<PathBuf>,
}

struct WalkStart {
    visible: PathBuf,
    resolved: PathBuf,
}

pub fn search_directory_entries(options: &SearchOptions) -> Vec<DirectorySuggestionEntry> {
    let Some(root) = resolve_directory(&options.root) else {
        return Vec::new();
    };
    let git_ignored = if options.respect_git_ignore {
        load_git_ignored_paths(&root)
    } else {
        Default::default()
    };
    let excluded = resolve_existing_paths(&options.excluded_discovery_paths);
    let Some(input) = build_search_input(options, root, git_ignored, excluded) else {
        return Vec::new();
    };

    let exact = if input.plan.browse_exact_path
        || (input.plan.is_path_query
            && (input.match_mode == MatchMode::Suffix || resolves_typed_path(&input)))
    {
        find_exact_entry(&input)
    } else {
        None
    };
    if let (Some(exact), 1) = (&exact, input.limit) {
        return vec![exact.clone()];
    }

    let ranked = search_candidates(&input);
    let results: Vec<_> = sort_and_format(ranked, &input)
        .into_iter()
        .take(input.limit)
        .collect();
    match exact {
        Some(exact) => std::iter::once(exact.clone())
            .chain(results.into_iter().filter(|e| *e != exact))
            .take(input.limit)
            .collect(),
        None => results,
    }
}

fn resolve_directory(root: &str) -> Option<PathBuf> {
    let resolved = fs::canonicalize(resolve_cwd(root)).ok()?;
    fs::metadata(&resolved).ok()?.is_dir().then_some(resolved)
}

fn resolve_existing_paths(paths: &[String]) -> HashSet<PathBuf> {
    paths
        .iter()
        .filter_map(|p| fs::canonicalize(resolve_cwd(p)).ok())
        .collect()
}

fn build_search_input(
    options: &SearchOptions,
    root: PathBuf,
    git_ignored_paths: std::sync::Arc<HashSet<PathBuf>>,
    excluded_discovery_paths: HashSet<PathBuf>,
) -> Option<SearchInput> {
    let include_directories = options.include_directories.unwrap_or(true);
    let include_files = options.include_files.unwrap_or(false);
    if !include_directories && !include_files {
        return None;
    }
    let policy = options
        .path_query_policy
        .unwrap_or(PathQueryPolicy::Slashes);
    let plan = parse_query(
        &options.query,
        &root,
        &resolve_cwd(&options.root),
        policy,
        &options.root_aliases,
        options
            .blank_query_behavior
            .unwrap_or(BlankQueryBehavior::None),
    )?;
    Some(SearchInput {
        root,
        plan,
        include_directories,
        include_files,
        match_mode: options.match_mode.unwrap_or(MatchMode::Fuzzy),
        path_query_policy: policy,
        path_format: options.path_format,
        hidden_directory_names: options
            .traversable_hidden_directory_names
            .iter()
            .cloned()
            .collect(),
        limit: options.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT) as usize,
        max_depth: options.max_depth.unwrap_or(DEFAULT_MAX_DEPTH),
        max_entries_scanned: options
            .max_entries_scanned
            .unwrap_or(DEFAULT_MAX_ENTRIES_SCANNED),
        confident_result_scan_threshold: options.confident_result_scan_threshold.filter(|t| *t > 0),
        git_ignored_paths,
        excluded_discovery_paths,
    })
}

/// Only a rooted query under the rooted policy names a location. Under the
/// slashes policy a query with a slash is a fragment that may match anywhere.
fn resolves_typed_path(input: &SearchInput) -> bool {
    input.path_query_policy == PathQueryPolicy::Rooted && input.plan.is_path_query
}

fn find_exact_entry(input: &SearchInput) -> Option<DirectorySuggestionEntry> {
    if input.plan.normalized_query.is_empty() {
        return None;
    }
    let visible = resolve(&input.root, &input.plan.normalized_query);
    let resolved = fs::canonicalize(&visible).ok()?;
    if !is_path_inside_root(&input.root, &resolved) {
        return None;
    }
    // No ignore filtering: the caller named this path, so containment is the only question.
    let kind = kind_of(&fs::metadata(&resolved).ok()?)?;
    if (kind == Kind::Directory && !input.include_directories)
        || (kind == Kind::File && !input.include_files)
    {
        return None;
    }
    Some(format_entry(&visible, kind, input))
}

fn format_entry(visible: &Path, kind: Kind, input: &SearchInput) -> DirectorySuggestionEntry {
    let path = match input.path_format {
        PathFormat::Absolute => visible.to_string_lossy().into_owned(),
        PathFormat::Relative => normalize_relative_path(&input.root, visible),
    };
    DirectorySuggestionEntry {
        path,
        kind: kind.into(),
    }
}

fn search_candidates(input: &SearchInput) -> Vec<RankedEntry> {
    let browses_root = input.plan.is_path_query && input.plan.normalized_query.is_empty();
    if browses_root || input.plan.browse_exact_path {
        return search_children(input);
    }
    if !resolves_typed_path(input) || input.plan.parent_part.is_empty() {
        return search_tree(input, None);
    }
    // A typed parent such as `~/Developer/pas` anchors the walk under `~/Developer`.
    // A parent that does not exist falls back to matching against the whole tree.
    let Some(parent) = resolve_typed_parent(input) else {
        return search_tree(input, None);
    };
    if is_inside_excluded_path(&parent.resolved, input) {
        return search_children(input);
    }
    search_tree(input, Some(parent))
}

fn resolve_typed_parent(input: &SearchInput) -> Option<WalkStart> {
    let visible = resolve(&input.root, &input.plan.parent_part);
    let resolved = fs::canonicalize(&visible).ok()?;
    if !is_path_inside_root(&input.root, &resolved) {
        return None;
    }
    fs::metadata(&resolved)
        .ok()?
        .is_dir()
        .then_some(WalkStart { visible, resolved })
}

fn search_children(input: &SearchInput) -> Vec<RankedEntry> {
    let part = if input.plan.parent_part.is_empty() {
        "."
    } else {
        &input.plan.parent_part
    };
    let visible_parent = resolve(&input.root, part);
    let Ok(parent) = fs::canonicalize(&visible_parent) else {
        return Vec::new();
    };
    if !is_path_inside_root(&input.root, &parent) || is_git_ignored_path(&parent, input) {
        return Vec::new();
    }
    // Browsing inside an excluded path is explicit: list what is there.
    let cleared;
    let discovery_input: &SearchInput = if is_inside_excluded_path(&parent, input) {
        cleared = input.with_excluded(HashSet::new());
        &cleared
    } else {
        input
    };
    fs_read::read_children(&parent)
        .into_iter()
        .filter(|entry| {
            is_path_inside_root(&input.root, &entry.resolved)
                && should_discover(entry, discovery_input)
        })
        .filter_map(|entry| {
            let candidate = Traversed {
                visible: visible_parent.join(&entry.name),
                depth: 1,
                name: entry.name,
                resolved: entry.resolved,
                kind: entry.kind,
            };
            suggest_rank(&candidate, input)
        })
        .collect()
}

impl SearchInput {
    fn with_excluded(&self, excluded: HashSet<PathBuf>) -> SearchInput {
        SearchInput {
            root: self.root.clone(),
            plan: self.plan.clone(),
            include_directories: self.include_directories,
            include_files: self.include_files,
            match_mode: self.match_mode,
            path_query_policy: self.path_query_policy,
            path_format: self.path_format,
            hidden_directory_names: self.hidden_directory_names.clone(),
            limit: self.limit,
            max_depth: self.max_depth,
            max_entries_scanned: self.max_entries_scanned,
            confident_result_scan_threshold: self.confident_result_scan_threshold,
            git_ignored_paths: self.git_ignored_paths.clone(),
            excluded_discovery_paths: excluded,
        }
    }
}

fn search_tree(input: &SearchInput, start: Option<WalkStart>) -> Vec<RankedEntry> {
    if input.max_entries_scanned == 0 {
        return Vec::new();
    }
    let start = start.unwrap_or_else(|| WalkStart {
        visible: input.root.clone(),
        resolved: input.root.clone(),
    });
    let mut visited: HashSet<PathBuf> = HashSet::new();
    visited.insert(start.resolved.clone());
    let branches = child_branches(&start.resolved, &start.visible, 1, input);
    let mut walker = RoundRobin::new(branches);
    let confidence_query = if input.plan.normalized_query.is_empty() {
        &input.plan.search_term
    } else {
        &input.plan.normalized_query
    };
    let mut ranked = Vec::new();
    let mut scanned: u64 = 0;
    while let Some(entry) = walker.next(input, &mut visited) {
        scanned += 1;
        if let Some(r) = suggest_rank(&entry, input) {
            ranked.push(r);
        }
        if scanned >= input.max_entries_scanned {
            break;
        }
        if let Some(threshold) = input.confident_result_scan_threshold {
            if scanned >= threshold && has_confident_result(&ranked, confidence_query) {
                break;
            }
        }
    }
    ranked
}

fn sort_and_format(
    entries: Vec<RankedEntry>,
    input: &SearchInput,
) -> Vec<DirectorySuggestionEntry> {
    let mut unique: HashMap<(Kind, String), RankedEntry> = HashMap::new();
    for entry in entries {
        let key = (entry.kind, entry.path.clone());
        match unique.get(&key) {
            Some(existing) if compare_rank(&entry, existing).is_ge() => {}
            _ => {
                unique.insert(key, entry);
            }
        }
    }
    let mut sorted: Vec<_> = unique.into_values().collect();
    sorted.sort_by(compare_rank);
    sorted
        .iter()
        .map(|e| format_entry(&e.visible, e.kind, input))
        .collect()
}
