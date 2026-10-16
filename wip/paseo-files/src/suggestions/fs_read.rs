//! Filesystem reads for the suggestion engine: child listing and the
//! git-ignored path set.

use super::{ChildEntry, Kind};
use crate::collation::locale_compare;
use crate::git::{run_git, GitOptions};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

const GIT_IGNORED_PATHS_CACHE_TTL: Duration = Duration::from_secs(8);
const GIT_IGNORED_PATHS_CACHE_MAX_ENTRIES: usize = 256;

enum RawKind {
    Entry(Kind),
    Symlink,
}

pub(super) fn kind_of(meta: &fs::Metadata) -> Option<Kind> {
    if meta.is_dir() {
        Some(Kind::Directory)
    } else if meta.is_file() {
        Some(Kind::File)
    } else {
        None
    }
}

/// Children of `directory`, sorted by name. Symlinks resolve to their target's
/// kind; broken links and special files are dropped.
pub(super) fn read_children(directory: &Path) -> Vec<ChildEntry> {
    match fs::metadata(directory) {
        Ok(m) if m.is_dir() => {}
        _ => return Vec::new(),
    }
    let Ok(read) = fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut raw: Vec<(String, RawKind)> = Vec::new();
    for dirent in read.flatten() {
        let Ok(ft) = dirent.file_type() else { continue };
        let name = dirent.file_name().to_string_lossy().into_owned();
        if ft.is_dir() {
            raw.push((name, RawKind::Entry(Kind::Directory)));
        } else if ft.is_file() {
            raw.push((name, RawKind::Entry(Kind::File)));
        } else if ft.is_symlink() {
            raw.push((name, RawKind::Symlink));
        }
    }
    raw.sort_by(|a, b| locale_compare(&a.0, &b.0));
    let mut children: Vec<ChildEntry> = raw
        .into_iter()
        .filter_map(|(name, kind)| resolve_child(directory, name, kind))
        .collect();
    children.sort_by(|a, b| locale_compare(&a.name, &b.name));
    children
}

fn resolve_child(directory: &Path, name: String, kind: RawKind) -> Option<ChildEntry> {
    let visible = directory.join(&name);
    match kind {
        RawKind::Entry(kind) => Some(ChildEntry {
            name,
            resolved: visible,
            kind,
            via_symlink: false,
        }),
        RawKind::Symlink => {
            let resolved = fs::canonicalize(&visible).ok()?;
            let kind = kind_of(&fs::metadata(&resolved).ok()?)?;
            Some(ChildEntry {
                name,
                resolved,
                kind,
                via_symlink: true,
            })
        }
    }
}

type IgnoreCache = Mutex<HashMap<PathBuf, (Instant, Arc<HashSet<PathBuf>>)>>;

fn ignore_cache() -> &'static IgnoreCache {
    static CACHE: OnceLock<IgnoreCache> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Gitignored files and directories under `root`, cached for 8 seconds per root.
/// Any git failure yields an empty set.
pub(super) fn load_git_ignored_paths(root: &Path) -> Arc<HashSet<PathBuf>> {
    let now = Instant::now();
    if let Ok(cache) = ignore_cache().lock() {
        if let Some((expires, paths)) = cache.get(root) {
            if *expires > now {
                return paths.clone();
            }
        }
    }
    let paths = Arc::new(run_ignored_listing(root));
    if let Ok(mut cache) = ignore_cache().lock() {
        cache.insert(
            root.to_path_buf(),
            (now + GIT_IGNORED_PATHS_CACHE_TTL, paths.clone()),
        );
        if cache.len() > GIT_IGNORED_PATHS_CACHE_MAX_ENTRIES {
            cache.retain(|_, (expires, _)| *expires > now);
            while cache.len() > GIT_IGNORED_PATHS_CACHE_MAX_ENTRIES {
                let Some(key) = cache.keys().next().cloned() else {
                    break;
                };
                cache.remove(&key);
            }
        }
    }
    paths
}

fn run_ignored_listing(root: &Path) -> HashSet<PathBuf> {
    let output = run_git(
        &[
            "ls-files",
            "-o",
            "-i",
            "--directory",
            "--exclude-standard",
            "-z",
        ],
        root,
        &GitOptions {
            env_set: &[("GIT_OPTIONAL_LOCKS", "0")],
            timeout: Some(Duration::from_secs(10)),
            ..Default::default()
        },
    );
    match output {
        Ok(out) => out
            .stdout
            .split('\0')
            .filter(|s| !s.is_empty())
            .map(|rel| crate::path_utils::resolve(root, rel.strip_suffix('/').unwrap_or(rel)))
            .collect(),
        Err(_) => HashSet::new(),
    }
}
