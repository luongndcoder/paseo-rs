//! Batched blob reads through `git cat-file --batch` (`git-file-contents.ts`).

use std::collections::HashMap;

use crate::error::{GitError, Result};
use crate::run::{run_git_bytes, GitOptions};

// Matches the git-show ceiling. Oversized batches use bounded per-file reads.
const MAX_CONTENT_BYTES: usize = 20 * 1024 * 1024;

/// `Ok(None)` means the batch cannot preserve per-file read semantics within its
/// budget (newline in a spec, truncated output, non-blob object); callers fall
/// back to per-file reads. A `None` map value is a missing object.
pub async fn read_git_file_contents(
    cwd: &str,
    specs: &[String],
) -> Result<Option<HashMap<String, Option<String>>>> {
    let mut unique: Vec<&String> = Vec::new();
    for s in specs {
        if !unique.contains(&s) {
            unique.push(s);
        }
    }
    if unique.is_empty() {
        return Ok(Some(HashMap::new()));
    }
    // The line-based batch protocol cannot represent embedded newlines.
    if unique.iter().any(|s| s.contains('\n')) {
        return Ok(None);
    }
    let input = format!(
        "{}\n",
        unique
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    );
    let result = run_git_bytes(
        &["cat-file", "--batch"],
        &GitOptions::new(cwd)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("LC_ALL", "C")
            .max_output(MAX_CONTENT_BYTES)
            .input(input.into_bytes()),
    )
    .await?;
    if result.truncated {
        return Ok(None);
    }
    let out = result.stdout;
    let mut contents = HashMap::new();
    let mut offset = 0usize;
    for spec in unique {
        let end = out[offset..]
            .iter()
            .position(|b| *b == b'\n')
            .map(|i| i + offset)
            .ok_or_else(|| GitError::other("Incomplete git object header"))?;
        let header = String::from_utf8_lossy(&out[offset..end]).into_owned();
        offset = end + 1;
        if header == format!("{spec} missing") {
            contents.insert(spec.clone(), None);
            continue;
        }
        // Submodules and other non-blob objects retain git-show behavior.
        let size = match parse_blob_header(&header) {
            Some(size) => size,
            None => return Ok(None),
        };
        if offset + size >= out.len() || out[offset + size] != b'\n' {
            return Err(GitError::other("Invalid git object length"));
        }
        contents.insert(
            spec.clone(),
            Some(String::from_utf8_lossy(&out[offset..offset + size]).into_owned()),
        );
        offset += size + 1;
    }
    if offset != out.len() {
        return Err(GitError::other("Unexpected git object output"));
    }
    Ok(Some(contents))
}

fn parse_blob_header(header: &str) -> Option<usize> {
    let mut parts = header.split(' ');
    let oid = parts.next()?;
    let kind = parts.next()?;
    let size = parts.next()?;
    if parts.next().is_some()
        || kind != "blob"
        || oid.is_empty()
        || !oid
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return None;
    }
    if size.is_empty() || !size.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    size.parse().ok()
}
