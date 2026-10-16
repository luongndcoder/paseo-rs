//! Lexical path utilities (`utils/path.ts` plus `projectDirNameFromCwd`).
//!
//! Node's `path.posix` and `path.win32` are re-implemented here because the TS code runs the
//! win32 flavor on every platform. Comparison is string-only unless a function says
//! "realpath aware"; paths are never resolved through symlinks otherwise.

use regex::Regex;
use std::sync::LazyLock;

// ---------------------------------------------------------------------------------------------
// Node path flavors
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Flavor {
    Posix,
    Win32,
}

impl Flavor {
    fn is_sep(self, b: u8) -> bool {
        match self {
            Flavor::Posix => b == b'/',
            Flavor::Win32 => b == b'/' || b == b'\\',
        }
    }

    fn sep(self) -> char {
        match self {
            Flavor::Posix => '/',
            Flavor::Win32 => '\\',
        }
    }
}

fn is_windows_device_root(b: u8) -> bool {
    b.is_ascii_alphabetic()
}

/// Node's internal `normalizeString`. Separators are ASCII so byte slicing stays on UTF-8
/// boundaries.
fn normalize_string(path: &str, allow_above_root: bool, flavor: Flavor) -> String {
    let bytes = path.as_bytes();
    let separator = flavor.sep();
    let mut res = String::new();
    let mut last_segment_length: i64 = 0;
    let mut last_slash: i64 = -1;
    let mut dots: i64 = 0;
    let mut code: u8 = 0;
    let mut i: i64 = 0;
    let len = bytes.len() as i64;
    while i <= len {
        if i < len {
            code = bytes[i as usize];
        } else if flavor.is_sep(code) {
            break;
        } else {
            code = b'/';
        }
        if flavor.is_sep(code) {
            if last_slash == i - 1 || dots == 1 {
                // no-op
            } else if dots == 2 {
                if res.len() < 2 || last_segment_length != 2 || !res.ends_with("..") {
                    if res.len() > 2 {
                        match res.rfind(separator) {
                            None => {
                                res.clear();
                                last_segment_length = 0;
                            }
                            Some(idx) => {
                                res.truncate(idx);
                                last_segment_length = res.len() as i64
                                    - 1
                                    - res.rfind(separator).map(|p| p as i64).unwrap_or(-1);
                            }
                        }
                        last_slash = i;
                        dots = 0;
                        i += 1;
                        continue;
                    } else if !res.is_empty() {
                        res.clear();
                        last_segment_length = 0;
                        last_slash = i;
                        dots = 0;
                        i += 1;
                        continue;
                    }
                }
                if allow_above_root {
                    if res.is_empty() {
                        res.push_str("..");
                    } else {
                        res.push(separator);
                        res.push_str("..");
                    }
                    last_segment_length = 2;
                }
            } else {
                let segment = &path[(last_slash + 1) as usize..i as usize];
                if !res.is_empty() {
                    res.push(separator);
                    res.push_str(segment);
                } else {
                    res = segment.to_string();
                }
                last_segment_length = i - last_slash - 1;
            }
            last_slash = i;
            dots = 0;
        } else if code == b'.' && dots != -1 {
            dots += 1;
        } else {
            dots = -1;
        }
        i += 1;
    }
    res
}

/// `path.posix.normalize`.
pub fn posix_normalize(path: &str) -> String {
    if path.is_empty() {
        return ".".to_string();
    }
    let is_absolute = path.starts_with('/');
    let trailing_sep = path.ends_with('/');
    let mut out = normalize_string(path, !is_absolute, Flavor::Posix);
    if out.is_empty() {
        if is_absolute {
            return "/".to_string();
        }
        return if trailing_sep {
            "./".to_string()
        } else {
            ".".to_string()
        };
    }
    if trailing_sep {
        out.push('/');
    }
    if is_absolute {
        format!("/{out}")
    } else {
        out
    }
}

/// `path.win32.normalize` (the legacy `:` special case for drive-relative names is omitted).
pub fn win32_normalize(path: &str) -> String {
    let bytes = path.as_bytes();
    let len = bytes.len();
    if len == 0 {
        return ".".to_string();
    }
    let is_sep = |b: u8| b == b'/' || b == b'\\';
    if len == 1 {
        return if bytes[0] == b'/' {
            "\\".to_string()
        } else {
            path.to_string()
        };
    }
    let mut root_end = 0usize;
    let mut device: Option<String> = None;
    let mut is_absolute = false;
    let code = bytes[0];
    if is_sep(code) {
        is_absolute = true;
        if is_sep(bytes[1]) {
            let mut j = 2usize;
            let mut last = j;
            while j < len && !is_sep(bytes[j]) {
                j += 1;
            }
            if j < len && j != last {
                let first_part = &path[last..j];
                last = j;
                while j < len && is_sep(bytes[j]) {
                    j += 1;
                }
                if j < len && j != last {
                    last = j;
                    while j < len && !is_sep(bytes[j]) {
                        j += 1;
                    }
                    if j == len {
                        return format!("\\\\{}\\{}\\", first_part, &path[last..]);
                    }
                    if j != last {
                        device = Some(format!("\\\\{}\\{}", first_part, &path[last..j]));
                        root_end = j;
                    }
                }
            }
        } else {
            root_end = 1;
        }
    } else if is_windows_device_root(code) && bytes[1] == b':' {
        device = Some(path[..2].to_string());
        root_end = 2;
        if len > 2 && is_sep(bytes[2]) {
            is_absolute = true;
            root_end = 3;
        }
    }
    let mut tail = if root_end < len {
        normalize_string(&path[root_end..], !is_absolute, Flavor::Win32)
    } else {
        String::new()
    };
    if tail.is_empty() && !is_absolute {
        tail = ".".to_string();
    }
    if !tail.is_empty() && is_sep(bytes[len - 1]) {
        tail.push('\\');
    }
    match device {
        None => {
            if is_absolute {
                format!("\\{tail}")
            } else {
                tail
            }
        }
        Some(device) => {
            if is_absolute {
                format!("{device}\\{tail}")
            } else {
                format!("{device}{tail}")
            }
        }
    }
}

/// `path.win32.parse(path).root`.
pub fn win32_root(path: &str) -> String {
    let bytes = path.as_bytes();
    let len = bytes.len();
    if len == 0 {
        return String::new();
    }
    let is_sep = |b: u8| b == b'/' || b == b'\\';
    let code = bytes[0];
    if len == 1 {
        return if is_sep(code) {
            path.to_string()
        } else {
            String::new()
        };
    }
    let mut root_end = 0usize;
    if is_sep(code) {
        root_end = 1;
        if is_sep(bytes[1]) {
            let mut j = 2usize;
            let mut last = j;
            while j < len && !is_sep(bytes[j]) {
                j += 1;
            }
            if j < len && j != last {
                last = j;
                while j < len && is_sep(bytes[j]) {
                    j += 1;
                }
                if j < len && j != last {
                    last = j;
                    while j < len && !is_sep(bytes[j]) {
                        j += 1;
                    }
                    if j == len {
                        return path.to_string();
                    }
                    if j != last {
                        root_end = j + 1;
                    }
                }
            }
        }
    } else if is_windows_device_root(code) && bytes[1] == b':' {
        if len <= 2 {
            return path.to_string();
        }
        root_end = 2;
        if is_sep(bytes[2]) {
            if len == 3 {
                return path.to_string();
            }
            root_end = 3;
        }
    }
    path[..root_end].to_string()
}

fn posix_root(path: &str) -> String {
    if path.starts_with('/') {
        "/".to_string()
    } else {
        String::new()
    }
}

fn root_of(flavor: Flavor, path: &str) -> String {
    match flavor {
        Flavor::Posix => posix_root(path),
        Flavor::Win32 => win32_root(path),
    }
}

fn normalize(flavor: Flavor, path: &str) -> String {
    match flavor {
        Flavor::Posix => posix_normalize(path),
        Flavor::Win32 => win32_normalize(path),
    }
}

fn is_absolute(flavor: Flavor, path: &str) -> bool {
    let b = path.as_bytes();
    match flavor {
        Flavor::Posix => b.first() == Some(&b'/'),
        Flavor::Win32 => {
            if b.is_empty() {
                return false;
            }
            if b[0] == b'/' || b[0] == b'\\' {
                return true;
            }
            b.len() > 2
                && is_windows_device_root(b[0])
                && b[1] == b':'
                && (b[2] == b'/' || b[2] == b'\\')
        }
    }
}

/// `path.<flavor>.relative(from, to)` for the normalized absolute inputs this module feeds
/// it. A relative input is resolved against the process cwd first.
fn relative(flavor: Flavor, from: &str, to: &str) -> String {
    let resolve_one = |p: &str| -> String {
        let joined = if is_absolute(flavor, p) {
            p.to_string()
        } else {
            let cwd = std::env::current_dir()
                .map(|c| c.to_string_lossy().into_owned())
                .unwrap_or_else(|_| "/".to_string());
            format!("{}{}{}", cwd, flavor.sep(), p)
        };
        let normalized = normalize(flavor, &joined);
        let root = root_of(flavor, &normalized);
        let mut s = normalized;
        while s.len() > root.len() && s.as_bytes().last().is_some_and(|b| flavor.is_sep(*b)) {
            s.pop();
        }
        s
    };
    let from = resolve_one(from);
    let to = resolve_one(to);
    let fold = |s: &str| -> String {
        if flavor == Flavor::Win32 {
            s.to_lowercase()
        } else {
            s.to_string()
        }
    };
    if fold(&from) == fold(&to) {
        return String::new();
    }
    let from_root = root_of(flavor, &from);
    let to_root = root_of(flavor, &to);
    if fold(&from_root) != fold(&to_root) {
        return to;
    }
    let parts = |s: &str, root: &str| -> Vec<String> {
        s[root.len()..]
            .split(|c| c == '/' || (flavor == Flavor::Win32 && c == '\\'))
            .filter(|p| !p.is_empty())
            .map(str::to_string)
            .collect()
    };
    let from_parts = parts(&from, &from_root);
    let to_parts = parts(&to, &to_root);
    let mut common = 0;
    while common < from_parts.len()
        && common < to_parts.len()
        && fold(&from_parts[common]) == fold(&to_parts[common])
    {
        common += 1;
    }
    let mut out: Vec<String> = Vec::new();
    for _ in common..from_parts.len() {
        out.push("..".to_string());
    }
    out.extend(to_parts[common..].iter().cloned());
    out.join(&flavor.sep().to_string())
}

// ---------------------------------------------------------------------------------------------
// utils/path.ts
// ---------------------------------------------------------------------------------------------

static DRIVE_NAMESPACE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[/\\]{2}\?[/\\]([a-zA-Z]:)[/\\](.*)$").expect("regex"));
static UNC_NAMESPACE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^[/\\]{2}\?[/\\]UNC[/\\]([^/\\]+)[/\\]([^/\\]+)(?:[/\\](.*))?$")
        .expect("regex")
});

/// True when the path's shape identifies it as a Windows path (drive letter, `\\?\` device
/// namespace, or UNC), independent of the host platform.
pub fn looks_like_definite_windows_path(value: &str) -> bool {
    let b = value.as_bytes();
    if b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'/' || b[2] == b'\\')
    {
        return true;
    }
    let is_sep = |x: u8| x == b'/' || x == b'\\';
    if b.len() >= 4 && is_sep(b[0]) && is_sep(b[1]) && b[2] == b'?' && is_sep(b[3]) {
        return true;
    }
    // ^\\{2}[^/\\]+[/\\][^/\\]+
    if b.len() >= 2 && b[0] == b'\\' && b[1] == b'\\' {
        let mut i = 2;
        let start = i;
        while i < b.len() && !is_sep(b[i]) {
            i += 1;
        }
        if i > start && i < b.len() {
            i += 1;
            let s2 = i;
            while i < b.len() && !is_sep(b[i]) {
                i += 1;
            }
            return i > s2;
        }
    }
    false
}

fn should_compare_as_windows(left: &str, right: &str) -> bool {
    looks_like_definite_windows_path(left) || looks_like_definite_windows_path(right)
}

fn strip_windows_namespace_prefix(value: &str) -> String {
    if let Some(caps) = DRIVE_NAMESPACE.captures(value) {
        return format!("{}\\{}", &caps[1], caps.get(2).map_or("", |m| m.as_str()));
    }
    if let Some(caps) = UNC_NAMESPACE.captures(value) {
        let rest = caps
            .get(3)
            .map(|m| format!("\\{}", m.as_str()))
            .unwrap_or_default();
        return format!("\\\\{}\\{}{}", &caps[1], &caps[2], rest);
    }
    value.to_string()
}

fn strip_trailing_separators(value: &str, root: &str, compare_as_windows: bool) -> String {
    let flavor = if compare_as_windows {
        Flavor::Win32
    } else {
        Flavor::Posix
    };
    let mut result = value.to_string();
    while result.len() > root.len()
        && result.as_bytes().last().is_some_and(|b| {
            if compare_as_windows {
                flavor.is_sep(*b)
            } else {
                *b == b'/'
            }
        })
    {
        result.pop();
    }
    result
}

fn normalize_path_preserving_case(value: &str, compare_as_windows: bool) -> String {
    let flavor = if compare_as_windows {
        Flavor::Win32
    } else {
        Flavor::Posix
    };
    let comparable = if compare_as_windows {
        strip_windows_namespace_prefix(value)
    } else {
        value.to_string()
    };
    let normalized = normalize(flavor, &comparable);
    let root = root_of(flavor, &normalized);
    strip_trailing_separators(&normalized, &root, compare_as_windows)
}

fn normalize_path_for_comparison(value: &str, compare_as_windows: bool) -> String {
    let normalized = normalize_path_preserving_case(value, compare_as_windows);
    if compare_as_windows {
        normalized.to_lowercase()
    } else {
        normalized
    }
}

/// Filesystem-equivalence of two cwd strings: separators, dot segments, trailing separators,
/// Windows namespace prefixes and (for Windows paths only) case. Never touches the disk.
pub fn are_equivalent_paths(left: &str, right: &str) -> bool {
    let as_windows = should_compare_as_windows(left, right);
    normalize_path_for_comparison(left, as_windows)
        == normalize_path_for_comparison(right, as_windows)
}

pub fn create_path_equivalence_matcher(target: &str) -> impl Fn(&str) -> bool {
    let target = target.to_string();
    let compare_as_windows = looks_like_definite_windows_path(&target);
    let normalized_target = normalize_path_for_comparison(&target, compare_as_windows);
    move |candidate: &str| {
        let candidate_as_windows =
            compare_as_windows || looks_like_definite_windows_path(candidate);
        let comparable_target = if candidate_as_windows == compare_as_windows {
            normalized_target.clone()
        } else {
            normalize_path_for_comparison(&target, candidate_as_windows)
        };
        normalize_path_for_comparison(candidate, candidate_as_windows) == comparable_target
    }
}

/// `value` plus its native and JS-flavor realpaths (both resolve through symlinks here), when
/// the path exists.
fn collect_path_variants(value: &str) -> Vec<String> {
    let mut variants = vec![value.to_string()];
    if let Ok(real) = std::fs::canonicalize(value) {
        let real = real.to_string_lossy().into_owned();
        if !variants.contains(&real) {
            variants.push(real);
        }
    }
    variants
}

pub fn create_realpath_aware_path_matcher(target: &str) -> impl Fn(&str) -> bool {
    let matchers: Vec<_> = collect_path_variants(target)
        .iter()
        .map(|variant| {
            let m = create_path_equivalence_matcher(variant);
            Box::new(m) as Box<dyn Fn(&str) -> bool>
        })
        .collect();
    move |candidate: &str| {
        collect_path_variants(candidate)
            .iter()
            .any(|variant| matchers.iter().any(|m| m(variant)))
    }
}

/// `normalizePathForIdentity`: realpath when it exists, then lexical normalization.
pub fn normalize_path_for_identity(value: &str) -> String {
    let canonical = collect_path_variants(value)
        .into_iter()
        .nth(1)
        .unwrap_or_else(|| value.to_string());
    let as_windows = looks_like_definite_windows_path(&canonical);
    normalize_path_for_comparison(&canonical, as_windows)
}

fn get_relative_path_inside_root(root: &str, candidate: &str) -> Option<String> {
    let as_windows = should_compare_as_windows(root, candidate);
    let flavor = if as_windows {
        Flavor::Win32
    } else {
        Flavor::Posix
    };
    let normalized_root = normalize_path_for_comparison(root, as_windows);
    let normalized_candidate = normalize_path_for_comparison(candidate, as_windows);
    let comparable = relative(flavor, &normalized_root, &normalized_candidate);
    let sep = flavor.sep();
    let outside = comparable == ".."
        || comparable.starts_with(&format!("..{sep}"))
        || is_absolute(flavor, &comparable);
    if outside {
        return None;
    }
    let preserving_root = normalize_path_preserving_case(root, as_windows);
    let preserving_candidate = normalize_path_preserving_case(candidate, as_windows);
    Some(relative(flavor, &preserving_root, &preserving_candidate))
}

pub fn is_path_inside_root(root: &str, candidate: &str) -> bool {
    get_relative_path_inside_root(root, candidate).is_some()
}

/// The candidate's relative suffix when it is inside `root`, over every realpath variant of
/// both. The suffix comes from the same lexical pair that proved containment.
pub fn get_realpath_aware_relative_path(root: &str, candidate: &str) -> Option<String> {
    let root_variants = collect_path_variants(root);
    let candidate_variants = collect_path_variants(candidate);
    for root_variant in &root_variants {
        for candidate_variant in &candidate_variants {
            if let Some(rel) = get_relative_path_inside_root(root_variant, candidate_variant) {
                return Some(rel);
            }
        }
    }
    None
}

pub fn is_realpath_inside_root(root: &str, candidate: &str) -> bool {
    get_realpath_aware_relative_path(root, candidate).is_some()
}

// ---------------------------------------------------------------------------------------------
// projectDirNameFromCwd
// ---------------------------------------------------------------------------------------------

static ROOT_SANITIZE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[:\\/]+").expect("regex"));
static SEP_RUN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[\\/]+").expect("regex"));

/// Directory name that holds one project's agent records under `agents/`.
///
/// win32 root parsing runs on every platform so drive letters and UNC roots map to the same
/// directory regardless of the host: `C:\Users\me` -> `C-Users-me`, `\\srv\share\x` ->
/// `srv-share-x`, `/` -> `root`.
pub fn project_dir_name_from_cwd(cwd: &str) -> String {
    let root = win32_root(cwd);
    let rest = &cwd[root.len()..];
    let trimmed = rest.trim_end_matches(['\\', '/']);
    let replaced = ROOT_SANITIZE.replace_all(&root, "-");
    let sanitized_root = replaced.trim_matches('-').to_string();
    let prefix = if sanitized_root.is_empty() {
        String::new()
    } else {
        format!("{sanitized_root}-")
    };
    if trimmed.is_empty() {
        return if sanitized_root.is_empty() {
            "root".to_string()
        } else {
            sanitized_root
        };
    }
    format!("{prefix}{}", SEP_RUN.replace_all(trimmed, "-"))
}

/// Lexical `path.resolve(p)` on POSIX: absolute against the process cwd, normalized, no
/// trailing separator. Paths that look like Windows paths are returned normalized as win32.
pub fn resolve_path(path: &str) -> String {
    if looks_like_definite_windows_path(path) {
        return normalize_path_preserving_case(path, true);
    }
    let joined = if path.starts_with('/') {
        path.to_string()
    } else {
        let cwd = std::env::current_dir()
            .map(|c| c.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "/".to_string());
        if path.is_empty() {
            cwd
        } else {
            format!("{cwd}/{path}")
        }
    };
    let normalized = posix_normalize(&joined);
    if normalized.len() > 1 && normalized.ends_with('/') {
        normalized[..normalized.len() - 1].to_string()
    } else {
        normalized
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_equivalent_forms() {
        let cases = [
            (
                "C:/Users/Administrator/GhostFactory",
                "C:\\Users\\Administrator\\GhostFactory",
            ),
            ("d:\\Projects\\paseo", "D:\\Projects\\paseo"),
            (
                "C:\\Users\\Administrator\\GhostFactory\\",
                "C:\\Users\\Administrator\\GhostFactory",
            ),
            (
                r"\\?\C:\Users\Administrator\GhostFactory",
                "C:\\Users\\Administrator\\GhostFactory",
            ),
            (
                r"\\?\UNC\server\share\GhostFactory",
                r"\\server\share\GhostFactory",
            ),
        ];
        for (l, r) in cases {
            assert!(are_equivalent_paths(l, r), "{l} vs {r}");
            assert!(create_path_equivalence_matcher(l)(r), "{l} matcher {r}");
        }
    }

    #[test]
    fn posix_casing_is_significant() {
        assert!(!are_equivalent_paths(
            "/Users/Administrator/GhostFactory",
            "/users/administrator/ghostfactory"
        ));
    }

    #[test]
    fn posix_root_containment_has_no_prefix_false_positives() {
        assert!(is_path_inside_root(
            "/opt/paseo",
            "/opt/paseo/node_modules/@getpaseo/server"
        ));
        assert!(!is_path_inside_root("/opt/paseo", "/opt/paseo-other"));
    }

    #[test]
    fn windows_root_containment_is_case_insensitive() {
        assert!(is_path_inside_root(
            "C:\\Paseo\\node_modules",
            "c:/paseo/node_modules/@getpaseo/server"
        ));
        assert!(!is_path_inside_root(
            "C:\\Paseo\\node_modules",
            "C:\\Paseo\\node_modules-other"
        ));
    }

    #[test]
    fn named_containment_cases() {
        let cases: Vec<(&str, &str, bool)> = vec![
            ("/repo", "/repo/..notes.txt", true),
            ("/repo", "/repo/..drafts/notes.md", true),
            ("/repo", "/repo/...drafts/notes.md", true),
            ("/repo", "/repo/docs/..notes.txt", true),
            ("/repo", "/repo", true),
            ("/repo", "/repo/../sibling/notes.md", false),
            ("/repo", "/repo/..drafts/../../outside.txt", false),
            ("/repo", "/repo-other/notes.md", false),
            ("C:\\Repo", "c:/repo/..notes.txt", true),
            ("C:\\Repo", "C:\\Repo\\..drafts\\notes.md", true),
            ("C:\\Repo", "C:\\Repo\\...drafts\\notes.md", true),
            ("C:\\Repo", "C:\\Repo\\..\\sibling\\notes.md", false),
            ("C:\\Repo", "C:\\Repo-other\\notes.md", false),
            ("C:\\Repo", "D:\\Repo\\..notes.txt", false),
            (
                r"\\server\share\repo",
                r"\\server\share\repo\..drafts\notes.md",
                true,
            ),
            (r"\\?\C:\Repo", "C:\\Repo\\..notes.txt", true),
        ];
        for (root, candidate, inside) in cases {
            assert_eq!(
                is_path_inside_root(root, candidate),
                inside,
                "{root} {candidate}"
            );
        }
    }

    #[test]
    fn windows_relative_suffix_keeps_case() {
        assert_eq!(
            get_realpath_aware_relative_path("C:\\Repo\\.git", "c:\\repo\\.git\\HEAD").as_deref(),
            Some("HEAD")
        );
        assert_eq!(
            get_realpath_aware_relative_path(
                "C:\\Repo\\.git",
                "c:\\repo\\.git\\refs\\heads\\FeatureCase"
            )
            .as_deref(),
            Some("refs\\heads\\FeatureCase")
        );
    }

    #[cfg(unix)]
    #[test]
    fn realpath_equivalent_root_suffix() {
        let dir = tempfile::tempdir().unwrap();
        let real_root = dir.path().join("real-root");
        let nested = real_root.join("packages").join("app");
        std::fs::create_dir_all(&nested).unwrap();
        let alias = dir.path().join("root-alias");
        std::os::unix::fs::symlink(&real_root, &alias).unwrap();
        let rel =
            get_realpath_aware_relative_path(alias.to_str().unwrap(), nested.to_str().unwrap());
        assert_eq!(rel.as_deref(), Some("packages/app"));
        assert_eq!(
            get_realpath_aware_relative_path(alias.to_str().unwrap(), dir.path().to_str().unwrap()),
            None
        );
    }

    #[test]
    fn project_dir_name_vectors() {
        let cases = [
            ("/Users/me/code/app", "Users-me-code-app"),
            ("/Users/me/code/app/", "Users-me-code-app"),
            ("/", "root"),
            ("C:\\", "C"),
            ("C:\\Users\\me\\app", "C-Users-me-app"),
            ("C:/Users/me/app/", "C-Users-me-app"),
            (r"\\server\share\", "server-share"),
            (r"\\server\share\proj\sub", "server-share-proj-sub"),
            ("relative/dir", "relative-dir"),
            ("", "root"),
            ("/a//b", "a-b"),
        ];
        for (cwd, expected) in cases {
            assert_eq!(project_dir_name_from_cwd(cwd), expected, "cwd {cwd:?}");
        }
    }

    #[test]
    fn node_normalize_matches_known_outputs() {
        assert_eq!(posix_normalize("/a/b/../c/./d//"), "/a/c/d/");
        assert_eq!(posix_normalize("a/../.."), "..");
        assert_eq!(posix_normalize(""), ".");
        assert_eq!(win32_normalize("C:/a/../b"), "C:\\b");
        assert_eq!(win32_normalize(r"\\server\share"), "\\\\server\\share\\");
        assert_eq!(win32_normalize("C:"), "C:.");
    }

    #[test]
    fn resolve_path_is_lexical() {
        assert_eq!(resolve_path("/a/b/../c/"), "/a/c");
        assert_eq!(resolve_path("/"), "/");
    }
}
