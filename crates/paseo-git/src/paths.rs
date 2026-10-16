//! Pure path/ref helpers: git quoted paths, rev-parse path output, worktree
//! list parsing and branch-name derivation.

use std::path::{Component, Path, PathBuf};

use serde::Serialize;

/// Lexical `path.resolve(base, p)` (no symlink resolution).
pub fn node_resolve(base: &str, p: &str) -> String {
    let joined = if Path::new(p).is_absolute() {
        PathBuf::from(p)
    } else {
        Path::new(base).join(p)
    };
    normalize_lexical(&joined)
}

pub fn normalize_lexical(path: &Path) -> String {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::ParentDir => {
                if !out.pop() {
                    // Past the root: stay at the root for absolute paths.
                    if !path.is_absolute() {
                        out.push("..");
                    }
                }
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    let s = out.to_string_lossy().into_owned();
    if s.is_empty() {
        ".".to_string()
    } else {
        s
    }
}

/// `core.quotepath=false` still quotes control characters, double quotes and
/// backslashes. Octal escapes are raw bytes, decoded together as UTF-8.
pub fn unquote_git_path(value: &str) -> String {
    if !value.starts_with('"') || !value.ends_with('"') {
        return value.to_string();
    }
    if value.len() < 2 {
        return String::new();
    }
    let encoded = &value.as_bytes()[1..value.len() - 1];
    let mut out: Vec<u8> = Vec::with_capacity(encoded.len());
    let mut i = 0;
    while i < encoded.len() {
        let b = encoded[i];
        if b == b'\\' && i + 1 < encoded.len() {
            let next = encoded[i + 1];
            if (b'0'..=b'7').contains(&next) {
                let mut j = i + 1;
                let mut v: u32 = 0;
                while j < encoded.len() && j < i + 4 && (b'0'..=b'7').contains(&encoded[j]) {
                    v = v * 8 + u32::from(encoded[j] - b'0');
                    j += 1;
                }
                out.push((v & 0xff) as u8);
                i = j;
                continue;
            }
            let mapped = match next {
                b'a' => Some(7),
                b'b' => Some(8),
                b't' => Some(9),
                b'n' => Some(10),
                b'v' => Some(11),
                b'f' => Some(12),
                b'r' => Some(13),
                b'"' => Some(34),
                b'\\' => Some(92),
                _ => None,
            };
            if let Some(m) = mapped {
                out.push(m);
                i += 2;
                continue;
            }
        }
        out.push(b);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Remove git's record delimiter (not whitespace belonging to the path).
pub fn parse_git_rev_parse_path(stdout: &str) -> Option<String> {
    let path = stdout
        .strip_suffix("\r\n")
        .or_else(|| stdout.strip_suffix('\n'))
        .unwrap_or(stdout);
    if path.is_empty() || path.contains('\r') || path.contains('\n') || path.starts_with("--") {
        return None;
    }
    Some(path.to_string())
}

pub fn resolve_git_rev_parse_path(cwd: &str, stdout: &str) -> Option<String> {
    parse_git_rev_parse_path(stdout).map(|p| node_resolve(cwd, &p))
}

/// True when `child` is strictly inside `parent` (handles `/` and `\`).
pub fn is_descendant_path(child: &str, parent: &str) -> bool {
    let mut c = child.replace('\\', "/");
    let mut p = parent.replace('\\', "/");
    c = c.trim_end_matches('/').to_string();
    p = p.trim_end_matches('/').to_string();
    let is_drive = |s: &str| {
        let b = s.as_bytes();
        b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':'
    };
    if is_drive(&c) || is_drive(&p) {
        c = c.to_lowercase();
        p = p.to_lowercase();
    }
    if !c.starts_with(&p) || c.len() == p.len() {
        return false;
    }
    c.as_bytes()[p.len()] == b'/'
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitWorktreeEntry {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_bare: Option<bool>,
}

/// Parse `git worktree list --porcelain`.
pub fn parse_worktree_list(output: &str) -> Vec<GitWorktreeEntry> {
    let mut entries = Vec::new();
    let mut current: Option<GitWorktreeEntry> = None;
    for line in output.split('\n') {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("worktree ") {
            if let Some(c) = current.take() {
                entries.push(c);
            }
            current = Some(GitWorktreeEntry {
                path: rest.trim().to_string(),
                branch_ref: None,
                is_bare: None,
            });
            continue;
        }
        if let Some(c) = current.as_mut() {
            if let Some(rest) = trimmed.strip_prefix("branch ") {
                c.branch_ref = Some(rest.trim().to_string());
            }
            if trimmed == "bare" {
                c.is_bare = Some(true);
            }
        }
    }
    if let Some(c) = current {
        entries.push(c);
    }
    entries
}

/// The human-readable branch name behind a ref (display and legacy identity
/// only). `refs/remotes/<remote>/<branch>` reads the first segment as the remote.
pub fn branch_name_from_ref(r: &str) -> String {
    let trimmed = r.trim();
    if let Some(rest) = trimmed.strip_prefix("refs/heads/") {
        return rest.to_string();
    }
    if let Some(rest) = trimmed.strip_prefix("refs/remotes/") {
        return match rest.find('/') {
            Some(i) => rest[i + 1..].to_string(),
            None => rest.to_string(),
        };
    }
    if let Some(rest) = trimmed.strip_prefix("origin/") {
        return rest.to_string();
    }
    trimmed.to_string()
}

pub fn is_qualified_ref(r: &str) -> bool {
    r.trim().starts_with("refs/")
}

pub fn is_qualified_branch_ref(r: &str) -> bool {
    r.starts_with("refs/heads/") || r.starts_with("refs/remotes/")
}

/// Ordering of JS string `<` (UTF-16 code units).
pub fn js_str_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    a.encode_utf16().cmp(b.encode_utf16())
}

pub fn expand_tilde(p: &str) -> String {
    if p == "~" || p.starts_with("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return format!("{}{}", home.to_string_lossy(), &p[1..]);
        }
    }
    p.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rev_parse_path_preserves_whitespace_with_each_delimiter() {
        for delimiter in ["\n", "\r\n", ""] {
            assert_eq!(
                parse_git_rev_parse_path(&format!("/repo {delimiter}")).as_deref(),
                Some("/repo ")
            );
            assert_eq!(
                parse_git_rev_parse_path(&format!(" relative path {delimiter}")).as_deref(),
                Some(" relative path ")
            );
        }
    }

    #[test]
    fn rev_parse_path_rejects_invalid_output() {
        for out in [
            "",
            "\n",
            "\r\n",
            "--path-format=absolute\n",
            "--path-format=absolute\n/repo\n",
            "/repo\n/another\n",
            "/repo\n\n",
        ] {
            assert_eq!(parse_git_rev_parse_path(out), None, "{out:?}");
        }
    }

    #[test]
    fn resolves_relative_path_without_stripping_trailing_space() {
        assert_eq!(
            resolve_git_rev_parse_path("/base", "repo \n").as_deref(),
            Some("/base/repo ")
        );
    }

    #[test]
    fn unquotes_octal_and_named_escapes() {
        assert_eq!(unquote_git_path("plain.txt"), "plain.txt");
        assert_eq!(
            unquote_git_path(r#""new \"draft\".txt""#),
            "new \"draft\".txt"
        );
        assert_eq!(unquote_git_path(r#""a\tb\\c""#), "a\tb\\c");
        assert_eq!(unquote_git_path(r#""\346\265\213.txt""#), "测.txt");
    }

    #[test]
    fn parses_worktree_list() {
        let out = "worktree /repo\nHEAD abc\nbranch refs/heads/main\n\nworktree /repo.git\nbare\n\nworktree /wt\nHEAD def\ndetached\n";
        let entries = parse_worktree_list(out);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].branch_ref.as_deref(), Some("refs/heads/main"));
        assert_eq!(entries[1].is_bare, Some(true));
        assert_eq!(entries[2].branch_ref, None);
    }

    #[test]
    fn descendant_path_rules() {
        assert!(is_descendant_path("/a/b/c", "/a/b"));
        assert!(is_descendant_path("/a/b/c/", "/a/b/"));
        assert!(!is_descendant_path("/a/b", "/a/b"));
        assert!(!is_descendant_path("/a/bc", "/a/b"));
        assert!(is_descendant_path("C:\\Repo\\x", "c:/repo"));
    }

    #[test]
    fn branch_names_from_refs() {
        assert_eq!(branch_name_from_ref("refs/heads/feature/x"), "feature/x");
        assert_eq!(branch_name_from_ref("refs/remotes/upstream/main"), "main");
        assert_eq!(branch_name_from_ref("refs/remotes/a/b/c"), "b/c");
        assert_eq!(branch_name_from_ref("origin/main"), "main");
        assert_eq!(branch_name_from_ref(" main "), "main");
    }
}
