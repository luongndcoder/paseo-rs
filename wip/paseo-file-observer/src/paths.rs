use std::io;
use std::path::{Component, Path, PathBuf};

use crate::error::FileObserverError;

/// Which path rules the observer compares with. Selectable so Windows
/// behavior is testable on any host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathPlatform {
    Posix,
    Windows,
}

impl PathPlatform {
    pub fn host() -> Self {
        if cfg!(windows) {
            Self::Windows
        } else {
            Self::Posix
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ObserverPaths {
    platform: PathPlatform,
}

impl ObserverPaths {
    pub fn new(platform: PathPlatform) -> Self {
        Self { platform }
    }

    pub fn host() -> Self {
        Self::new(PathPlatform::host())
    }

    fn sep(self) -> char {
        match self.platform {
            PathPlatform::Posix => '/',
            PathPlatform::Windows => '\\',
        }
    }

    fn comparable(self, path: &Path) -> String {
        let text = path.to_string_lossy();
        match self.platform {
            PathPlatform::Posix => text.into_owned(),
            PathPlatform::Windows => text.to_lowercase(),
        }
    }

    /// True when `path` equals `root` or lies below it.
    pub fn is_inside(self, root: &Path, path: &Path) -> bool {
        let root = self.comparable(root);
        let path = self.comparable(path);
        if path == root {
            return true;
        }
        let mut prefix = root;
        prefix.push(self.sep());
        path.starts_with(&prefix)
    }

    /// Drops duplicates and descendants of another path in the set.
    pub fn collapse(self, paths: &[PathBuf]) -> Vec<PathBuf> {
        // Plain lexicographic order does not put a parent right before its
        // descendants: a sibling such as "app-web" sorts between "app" and
        // "app/src". Sort on a key with the separator swapped for NUL so one
        // backward look is enough.
        let sep = self.sep();
        let mut seen = std::collections::HashSet::new();
        let mut decorated: Vec<(String, &PathBuf)> = paths
            .iter()
            .filter(|path| seen.insert((*path).clone()))
            .map(|path| (self.comparable(path).replace(sep, "\0"), path))
            .collect();
        decorated.sort_by(|a, b| a.0.cmp(&b.0));
        let mut kept: Vec<PathBuf> = Vec::new();
        for (_, path) in decorated {
            if let Some(previous) = kept.last() {
                if self.is_inside(previous, path) {
                    continue;
                }
            }
            kept.push(path.clone());
        }
        kept
    }

    /// Resolves, drops the root itself and anything outside it, then collapses.
    pub fn normalize_ignored_roots(self, root: &Path, paths: &[PathBuf]) -> Vec<PathBuf> {
        let inside: Vec<PathBuf> = paths
            .iter()
            .map(|path| resolve_path(path))
            .filter(|path| path != root && self.is_inside(root, path))
            .collect();
        self.collapse(&inside)
    }

    /// A watcher error that means rename/archive churn, not a real failure.
    pub fn is_expected_watch_disappearance(self, error: &FileObserverError) -> bool {
        error.is_missing_path()
            || (self.platform == PathPlatform::Windows && error.is_permission_denied())
    }
}

pub fn is_missing_io_error(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
    )
}

/// Lexical absolute path (like Node `path.resolve`): no symlink resolution,
/// no filesystem access besides the current directory for relative input.
pub fn resolve_path(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("/"))
            .join(path)
    };
    let mut out = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !matches!(
                    out.components().next_back(),
                    Some(Component::RootDir | Component::Prefix(_)) | None
                ) {
                    out.pop();
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sorted(mut v: Vec<PathBuf>) -> Vec<PathBuf> {
        v.sort();
        v
    }

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    // m3 regression: a sibling whose name extends the parent's ("app-web")
    // must not hide the parent from the single backward look in collapse().
    #[test]
    fn collapse_drops_descendant_but_keeps_sibling_extending_parent_name() {
        let paths = ObserverPaths::new(PathPlatform::Posix);
        let out = paths.collapse(&[
            p("/repo/packages/app"),
            p("/repo/packages/app-web"),
            p("/repo/packages/app/src"),
        ]);
        assert_eq!(
            sorted(out),
            sorted(vec![p("/repo/packages/app"), p("/repo/packages/app-web")])
        );
    }

    #[test]
    fn collapse_keeps_unrelated_roots_regardless_of_input_order() {
        let paths = ObserverPaths::new(PathPlatform::Posix);
        let out = paths.collapse(&[p("/repo/a/child"), p("/repo/b"), p("/repo/a")]);
        assert_eq!(sorted(out), sorted(vec![p("/repo/a"), p("/repo/b")]));
    }

    #[test]
    fn collapse_win32_is_case_insensitive_with_mismatched_casing() {
        let paths = ObserverPaths::new(PathPlatform::Windows);
        let parent = p("C:\\repo\\packages\\app");
        let sibling = p("C:\\repo\\packages\\APP-web");
        let descendant = p("c:\\repo\\packages\\app\\src");
        let out = paths.collapse(&[parent.clone(), sibling.clone(), descendant]);
        assert_eq!(sorted(out), sorted(vec![parent, sibling]));
    }

    #[test]
    fn normalize_ignored_roots_drops_root_and_outsiders() {
        let paths = ObserverPaths::new(PathPlatform::Posix);
        let root = p("/repo");
        let out = paths.normalize_ignored_roots(
            &root,
            &[
                p("/repo"),
                p("/elsewhere/x"),
                p("/repo/a/../b"),
                p("/repo/b/c"),
            ],
        );
        assert_eq!(out, vec![p("/repo/b")]);
    }

    #[test]
    fn resolve_path_is_lexical() {
        assert_eq!(resolve_path(Path::new("/a/./b/../c")), p("/a/c"));
        assert_eq!(resolve_path(Path::new("/../a")), p("/a"));
    }
}
