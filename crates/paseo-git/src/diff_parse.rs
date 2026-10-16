//! Unified diff parsing into the wire `ParsedDiffFile` shape
//! (`server/utils/diff-highlighter.ts` parseDiff, without syntax highlighting).

use std::sync::OnceLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::paths::unquote_git_path;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HighlightToken {
    pub text: String,
    pub style: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiffLineType {
    Add,
    Remove,
    Context,
    Header,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffLine {
    #[serde(rename = "type")]
    pub kind: DiffLineType,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<Vec<HighlightToken>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffHunk {
    pub old_start: u64,
    pub old_count: u64,
    pub new_start: u64,
    pub new_count: u64,
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffFileStatus {
    Ok,
    TooLarge,
    Binary,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParsedDiffFile {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
    pub is_new: bool,
    pub is_deleted: bool,
    pub additions: u64,
    pub deletions: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub staged: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unstaged: Option<bool>,
    pub hunks: Vec<DiffHunk>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<DiffFileStatus>,
}

// JS `.` excludes all line terminators, not just `\n`.
const D: &str = r"[^\r\n\u{2028}\u{2029}]";

fn re(cell: &'static OnceLock<Regex>, pattern: impl FnOnce() -> String) -> &'static Regex {
    cell.get_or_init(|| Regex::new(&pattern()).expect("valid regex"))
}

fn uses_diff_path_prefixes(old_path: &str, new_path: &str) -> bool {
    old_path.starts_with("a/") && new_path.starts_with("b/")
}

fn extract_path_from_metadata(lines: &[&str], prefix: &str) -> Option<String> {
    let line = lines.iter().find(|l| l.starts_with(prefix))?;
    let rest = &line[prefix.len()..];
    // `.replace(/\t.*$/, "")`
    let rest = match rest.find('\t') {
        Some(i) => &rest[..i],
        None => rest,
    };
    let path = unquote_git_path(rest);
    if path == "/dev/null" {
        None
    } else {
        Some(path)
    }
}

fn extract_path_from_diff_header(lines: &[&str]) -> String {
    static PREFIXED: OnceLock<Regex> = OnceLock::new();
    static QUOTED_FIRST: OnceLock<Regex> = OnceLock::new();
    static QUOTED_SECOND: OnceLock<Regex> = OnceLock::new();
    static PLAIN: OnceLock<Regex> = OnceLock::new();

    let first = lines.first().copied().unwrap_or("");
    let prefixed = re(&PREFIXED, || format!(r"^a/({D}+) b/({D}+)$"));
    if let Some(c) = prefixed.captures(first) {
        return c[2].to_string();
    }

    let quoted = r#""(?:\\[^\r\n\u{2028}\u{2029}]|[^"\\])*""#;
    let q1 = re(&QUOTED_FIRST, || format!(r"^({quoted}) ({D}+)$"));
    let q2 = re(&QUOTED_SECOND, || format!(r"^({D}+) ({quoted})$"));
    if let Some(c) = q1.captures(first).or_else(|| q2.captures(first)) {
        let old_path = unquote_git_path(&c[1]);
        let new_path = unquote_git_path(&c[2]);
        if uses_diff_path_prefixes(&old_path, &new_path) {
            return new_path[2..].to_string();
        }
        return new_path;
    }

    if let Some(p) = extract_path_from_metadata(lines, "+++ ")
        .or_else(|| extract_path_from_metadata(lines, "--- "))
    {
        return p;
    }

    let plain = re(&PLAIN, || r"^(\S+)\s+(\S+)$".to_string());
    if let Some(c) = plain.captures(first) {
        let (old_path, new_path) = (&c[1], &c[2]);
        let path = if new_path == "/dev/null" {
            old_path
        } else {
            new_path
        };
        return if uses_diff_path_prefixes(old_path, new_path) {
            path[2..].to_string()
        } else {
            path.to_string()
        };
    }
    "unknown".to_string()
}

fn parse_hunk_header(line: &str) -> Option<DiffHunk> {
    static HUNK: OnceLock<Regex> = OnceLock::new();
    static HEADER: OnceLock<Regex> = OnceLock::new();
    let hunk = re(&HUNK, || {
        r"^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@".to_string()
    });
    let c = hunk.captures(line)?;
    let num = |i: usize, default: u64| {
        c.get(i)
            .and_then(|m| m.as_str().parse::<u64>().ok())
            .unwrap_or(default)
    };
    let header = re(&HEADER, || format!(r"^(@@ {D}+? @@)"));
    let content = header
        .captures(line)
        .map(|h| h[1].to_string())
        .unwrap_or_else(|| line.to_string());
    Some(DiffHunk {
        old_start: num(1, 0),
        old_count: num(2, 1),
        new_start: num(3, 0),
        new_count: num(4, 1),
        lines: vec![DiffLine {
            kind: DiffLineType::Header,
            content,
            tokens: None,
        }],
    })
}

fn parse_section_body(lines: &[&str]) -> (Vec<DiffHunk>, u64, u64) {
    let mut hunks = Vec::new();
    let mut current: Option<DiffHunk> = None;
    let (mut additions, mut deletions) = (0u64, 0u64);
    for line in lines.iter().skip(1) {
        if let Some(new_hunk) = parse_hunk_header(line) {
            if let Some(h) = current.take() {
                hunks.push(h);
            }
            current = Some(new_hunk);
            continue;
        }
        let Some(h) = current.as_mut() else { continue };
        let push = |h: &mut DiffHunk, kind, content: &str| {
            h.lines.push(DiffLine {
                kind,
                content: content.to_string(),
                tokens: None,
            })
        };
        if let Some(rest) = line.strip_prefix('+') {
            push(h, DiffLineType::Add, rest);
            additions += 1;
        } else if let Some(rest) = line.strip_prefix('-') {
            push(h, DiffLineType::Remove, rest);
            deletions += 1;
        } else if let Some(rest) = line.strip_prefix(' ') {
            push(h, DiffLineType::Context, rest);
        } else if !line.is_empty() && !line.starts_with('\\') {
            push(h, DiffLineType::Context, line);
        }
    }
    if let Some(h) = current {
        hunks.push(h);
    }
    (hunks, additions, deletions)
}

/// Split at every line that starts with `diff --git `, dropping the marker
/// (JS `text.split(/^diff --git /m).filter(Boolean)`).
fn split_on_diff_marker(text: &str) -> Vec<&str> {
    const MARKER: &str = "diff --git ";
    let mut sections = Vec::new();
    let mut start = 0usize;
    let mut idx = 0usize;
    while idx <= text.len() {
        let at_line_start = idx == 0 || text.as_bytes()[idx - 1] == b'\n';
        if at_line_start && text[idx..].starts_with(MARKER) {
            sections.push(&text[start..idx]);
            idx += MARKER.len();
            start = idx;
            continue;
        }
        match text[idx..].find('\n') {
            Some(i) => idx += i + 1,
            None => break,
        }
    }
    sections.push(&text[start..]);
    sections.into_iter().filter(|s| !s.is_empty()).collect()
}

/// Split a multi-file patch into per-file sections, keeping the
/// `diff --git ` marker (JS `split(/(?=^diff --git )/m)`).
pub fn split_diff_sections(text: &str) -> Vec<&str> {
    const MARKER: &str = "diff --git ";
    let mut cuts = vec![0usize];
    let mut idx = 0usize;
    loop {
        if idx > 0 && text[idx..].starts_with(MARKER) {
            cuts.push(idx);
        }
        match text[idx..].find('\n') {
            Some(i) => idx += i + 1,
            None => break,
        }
        if idx >= text.len() {
            break;
        }
    }
    cuts.push(text.len());
    cuts.windows(2)
        .map(|w| &text[w[0]..w[1]])
        .filter(|s| !s.is_empty())
        .collect()
}

pub fn parse_diff(diff_text: &str) -> Vec<ParsedDiffFile> {
    if diff_text.trim().is_empty() {
        return Vec::new();
    }
    split_on_diff_marker(diff_text)
        .into_iter()
        .map(|section| {
            let lines: Vec<&str> = section.split('\n').collect();
            let is_new = section.contains("new file mode") || section.contains("--- /dev/null");
            let is_deleted =
                section.contains("deleted file mode") || section.contains("+++ /dev/null");
            let path = extract_path_from_diff_header(&lines);
            let (hunks, additions, deletions) = parse_section_body(&lines);
            ParsedDiffFile {
                path,
                old_path: None,
                is_new,
                is_deleted,
                additions,
                deletions,
                staged: None,
                unstaged: None,
                hunks,
                status: None,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hunks_counts_and_paths() {
        let text = "diff --git a/foo.txt b/foo.txt\nindex 1..2 100644\n--- a/foo.txt\n+++ b/foo.txt\n@@ -1,3 +1,4 @@ fn x\n a\n-b\n+B\n c\n+d\n";
        let files = parse_diff(text);
        assert_eq!(files.len(), 1);
        let f = &files[0];
        assert_eq!(f.path, "foo.txt");
        assert_eq!((f.additions, f.deletions), (2, 1));
        assert!(!f.is_new && !f.is_deleted);
        assert_eq!(f.hunks[0].lines[0].content, "@@ -1,3 +1,4 @@");
        assert_eq!(f.hunks[0].old_count, 3);
    }

    #[test]
    fn parses_new_and_deleted_and_quoted_paths() {
        let text = "diff --git a/n.txt b/n.txt\nnew file mode 100644\n--- /dev/null\n+++ b/n.txt\n@@ -0,0 +1 @@\n+x\ndiff --git \"a/q \\\"d\\\".txt\" \"b/q \\\"d\\\".txt\"\ndeleted file mode 100644\n--- \"a/q \\\"d\\\".txt\"\n+++ /dev/null\n@@ -1 +0,0 @@\n-y\n";
        let files = parse_diff(text);
        assert_eq!(files.len(), 2);
        assert!(files[0].is_new);
        assert_eq!(files[1].path, "q \"d\".txt");
        assert!(files[1].is_deleted);
    }

    #[test]
    fn rename_only_has_no_hunks_and_uses_destination() {
        let text = "diff --git a/old.txt b/with spaces.txt\nsimilarity index 100%\nrename from old.txt\nrename to with spaces.txt\n";
        let files = parse_diff(text);
        assert_eq!(files[0].path, "with spaces.txt");
        assert!(files[0].hunks.is_empty());
    }

    #[test]
    fn serializes_wire_shape() {
        let f = &parse_diff("diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -1 +1 @@\n-x\n+y\n")[0];
        let v = serde_json::to_value(f).unwrap();
        assert_eq!(v["isNew"], false);
        assert_eq!(v["hunks"][0]["oldStart"], 1);
        assert_eq!(v["hunks"][0]["lines"][1]["type"], "remove");
        assert!(v.get("oldPath").is_none() && v.get("status").is_none());
    }

    #[test]
    fn splits_sections_like_the_js_lookahead() {
        let t = "diff --git a/a b/a\n+x\ndiff --git a/b b/b\n+y\n";
        let s = split_diff_sections(t);
        assert_eq!(
            s,
            vec!["diff --git a/a b/a\n+x\n", "diff --git a/b b/b\n+y\n"]
        );
        assert_eq!(split_diff_sections("").len(), 0);
    }
}
