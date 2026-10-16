//! Lexical path helpers mirroring Node's `path.resolve` / `path.relative` and the
//! daemon's `expandUserPath`, `resolvePathFromBase`, `isPathInsideRoot`.

use std::path::{Component, Path, PathBuf};

pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// Collapse `.`, `..` and repeated separators without touching the filesystem.
pub fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::RootDir => out.push("/"),
            Component::CurDir | Component::Prefix(_) => {}
            Component::ParentDir => {
                if out.as_os_str() == "/" {
                    // `/..` stays at the root
                } else if !out.pop() {
                    out.push("..");
                }
            }
            Component::Normal(n) => out.push(n),
        }
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

/// Node's `path.resolve(base, requested)` for a single `requested` segment.
pub fn resolve(base: &Path, requested: &str) -> PathBuf {
    let req = Path::new(requested);
    let joined = if req.is_absolute() {
        req.to_path_buf()
    } else if requested.is_empty() {
        base.to_path_buf()
    } else {
        base.join(req)
    };
    let absolute = if joined.is_absolute() {
        joined
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("/"))
            .join(joined)
    };
    normalize(&absolute)
}

pub fn resolve_cwd(requested: &str) -> PathBuf {
    resolve(
        &std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
        requested,
    )
}

fn has_home_prefix(value: &str) -> bool {
    value == "~" || value.starts_with("~/")
}

pub fn expand_user_path(value: &str) -> PathBuf {
    let trimmed = value.trim();
    if has_home_prefix(trimmed) {
        return resolve(&home_dir(), trimmed.get(2..).unwrap_or(""));
    }
    resolve_cwd(trimmed)
}

pub fn resolve_path_from_base(base: &Path, requested: &str) -> PathBuf {
    let trimmed = requested.trim();
    if has_home_prefix(trimmed) || Path::new(trimmed).is_absolute() {
        return expand_user_path(trimmed);
    }
    resolve(base, trimmed)
}

/// Node's `path.relative(from, to)` on already-resolved absolute paths. Returns
/// an empty string when both are the same path.
pub fn relative(from: &Path, to: &Path) -> String {
    let from = normalize(from);
    let to = normalize(to);
    let f: Vec<_> = from.components().collect();
    let t: Vec<_> = to.components().collect();
    let common = f.iter().zip(t.iter()).take_while(|(a, b)| a == b).count();
    let mut parts: Vec<String> = Vec::new();
    for _ in common..f.len() {
        parts.push("..".to_string());
    }
    for c in &t[common..] {
        if let Component::Normal(n) = c {
            parts.push(n.to_string_lossy().into_owned());
        }
    }
    parts.join("/")
}

/// Relative path with `/` separators, `.` for the same path.
pub fn normalize_relative_path(root: &Path, target: &Path) -> String {
    let rel = relative(root, target);
    if rel.is_empty() {
        ".".to_string()
    } else {
        rel
    }
}

pub fn is_outside(relative_path: &str) -> bool {
    relative_path == ".." || relative_path.starts_with("../") || relative_path.starts_with('/')
}

/// Lexical containment: true when `candidate` equals or sits below `root`.
pub fn is_path_inside_root(root: &Path, candidate: &Path) -> bool {
    !is_outside(&relative(root, candidate))
}

/// Node's `path.extname`.
pub fn extname(path: &str) -> String {
    let base = basename(path);
    if base == ".." {
        return String::new();
    }
    match base.rfind('.') {
        Some(idx) if idx > 0 => base[idx..].to_string(),
        _ => String::new(),
    }
}

/// Node's `path.basename` (posix).
pub fn basename(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(idx) => &trimmed[idx + 1..],
        None => trimmed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_matches_node() {
        assert_eq!(relative(Path::new("/a/b"), Path::new("/a/b/c/d")), "c/d");
        assert_eq!(relative(Path::new("/a/b"), Path::new("/a/x")), "../x");
        assert_eq!(relative(Path::new("/a/b"), Path::new("/a/b")), "");
        assert_eq!(relative(Path::new("/"), Path::new("/usr")), "usr");
    }

    #[test]
    fn extname_matches_node() {
        assert_eq!(extname("a/b.txt"), ".txt");
        assert_eq!(extname(".bashrc"), "");
        assert_eq!(extname("file."), ".");
        assert_eq!(extname("a.tar.gz"), ".gz");
        assert_eq!(extname(".."), "");
        assert_eq!(extname("noext"), "");
    }

    #[test]
    fn basename_matches_node() {
        assert_eq!(basename("../../etc/passwd"), "passwd");
        assert_eq!(basename("a/b/"), "b");
        assert_eq!(basename("/"), "");
    }

    #[test]
    fn resolve_collapses_dots() {
        assert_eq!(
            resolve(Path::new("/w"), "a/../b/./c"),
            PathBuf::from("/w/b/c")
        );
        assert_eq!(resolve(Path::new("/w"), "/x//y/"), PathBuf::from("/x/y"));
        assert_eq!(resolve(Path::new("/w"), ".."), PathBuf::from("/"));
    }

    #[test]
    fn containment_is_lexical() {
        assert!(is_path_inside_root(Path::new("/r"), Path::new("/r/a")));
        assert!(is_path_inside_root(Path::new("/r"), Path::new("/r")));
        assert!(!is_path_inside_root(Path::new("/r"), Path::new("/rx")));
        assert!(!is_path_inside_root(Path::new("/r/a"), Path::new("/r")));
    }
}
