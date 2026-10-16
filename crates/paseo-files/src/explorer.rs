//! File explorer: scoped listing, reading, streaming, versioning and atomic write.

use crate::binary_frames::{
    encode_begin, encode_chunk, encode_end, FileBeginMetadata, FileEncoding,
    FILE_EXPLORER_STREAM_CHUNK_BYTES,
};
use crate::collation::locale_compare;
use crate::error::{ExplorerError, Result};
use crate::path_utils::{
    expand_user_path, extname, is_outside, normalize_relative_path, relative,
    resolve_path_from_base,
};
use crate::time::iso_from_ms;
use crate::wire::{
    ExplorerEncoding, ExplorerEntryKind, ExplorerFileKind, ExplorerFileVersion,
    ExplorerFileWriteResult, FileExplorerDirectory, FileExplorerEntry, FileExplorerFile,
};
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

pub const MAX_EDITABLE_FILE_BYTES: u64 = 1024 * 1024;
const FILE_TYPE_SAMPLE_BYTES: usize = 8192;

#[derive(Debug, Clone)]
pub struct ReadFileParams<'a> {
    pub root: &'a str,
    pub relative_path: &'a str,
}

#[derive(Debug, Clone)]
pub struct WriteFileParams<'a> {
    pub root: &'a str,
    pub relative_path: &'a str,
    pub content: &'a str,
    pub expected_modified_at: &'a str,
    pub expected_revision: Option<&'a str>,
}

pub(crate) struct ScopedPath {
    pub requested: PathBuf,
    pub resolved: PathBuf,
}

/// Resolve `relative_path` under `root`, following symlinks, and refuse
/// anything that leaves the canonical root. A path that does not exist yet
/// resolves to itself.
pub(crate) fn resolve_scoped_path(root: &str, relative_path: &str) -> Result<ScopedPath> {
    let workspace = expand_user_path(root);
    let requested = resolve_path_from_base(&workspace, relative_path);
    assert_within_workspace(&workspace, &requested)?;
    let canonical_root = fs::canonicalize(&workspace)?;
    match fs::canonicalize(&requested) {
        Ok(canonical) => {
            assert_within_workspace(&canonical_root, &canonical)?;
            Ok(ScopedPath {
                requested,
                resolved: canonical,
            })
        }
        Err(e) if crate::error::io_is_missing(&e) => {
            // The target (or a dangling final symlink) does not resolve. Resolve
            // the deepest existing ancestor so a symlinked parent that escapes
            // the workspace is still refused.
            let mut tail: Vec<std::ffi::OsString> = Vec::new();
            let mut cursor = requested.as_path();
            let mut base = loop {
                match fs::canonicalize(cursor) {
                    Ok(c) => break c,
                    Err(err) if crate::error::io_is_missing(&err) => {
                        match (cursor.file_name(), cursor.parent()) {
                            (Some(name), Some(parent)) => {
                                tail.push(name.to_os_string());
                                cursor = parent;
                            }
                            _ => return Err(err.into()),
                        }
                    }
                    Err(err) => return Err(err.into()),
                }
            };
            for name in tail.iter().rev() {
                base.push(name);
            }
            assert_within_workspace(&canonical_root, &base)?;
            Ok(ScopedPath {
                resolved: requested.clone(),
                requested,
            })
        }
        Err(e) => Err(e.into()),
    }
}

fn assert_within_workspace(root: &Path, candidate: &Path) -> Result<()> {
    if is_outside(&relative(root, candidate)) {
        return Err(ExplorerError::OutsideWorkspace);
    }
    Ok(())
}

pub(crate) fn open_for_read(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

pub(crate) fn file_revision(m: &Metadata) -> String {
    format!("{}:{}:{}:{}", m.dev(), m.ino(), m.size(), mtime_ns(m))
}

fn mtime_ns(m: &Metadata) -> i128 {
    m.mtime() as i128 * 1_000_000_000 + m.mtime_nsec() as i128
}

pub(crate) fn modified_at(m: &Metadata) -> String {
    iso_from_ms((mtime_ns(m) / 1_000_000) as i64)
}

fn image_mime(ext: &str) -> Option<&'static str> {
    match ext {
        ".png" => Some("image/png"),
        ".jpg" | ".jpeg" => Some("image/jpeg"),
        ".gif" => Some("image/gif"),
        ".webp" => Some("image/webp"),
        ".svg" => Some("image/svg+xml"),
        _ => None,
    }
}

fn text_mime(ext: &str) -> &'static str {
    if ext == ".json" {
        "application/json"
    } else {
        "text/plain"
    }
}

fn ext_of(path: &Path) -> String {
    extname(&path.to_string_lossy()).to_lowercase()
}

pub(crate) fn path_str(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

pub fn is_likely_binary(buffer: &[u8]) -> bool {
    if buffer.is_empty() {
        return false;
    }
    let mut suspicious = 0usize;
    for &b in buffer {
        if b == 0 {
            return true;
        }
        if (b < 32 && b != 9 && b != 10 && b != 13) || b == 127 {
            suspicious += 1;
        }
    }
    suspicious as f64 / buffer.len() as f64 > 0.3
}

fn is_valid_utf8(buffer: &[u8]) -> bool {
    std::str::from_utf8(buffer).is_ok()
}

#[derive(Debug, Clone, PartialEq)]
pub struct ListDirectoryParams<'a> {
    pub root: &'a str,
    pub relative_path: &'a str,
}

pub fn list_directory_entries(params: ListDirectoryParams) -> Result<FileExplorerDirectory> {
    let root = params.root;
    let dir = resolve_scoped_path(root, params.relative_path)?;
    let stats = fs::metadata(&dir.resolved)?;
    if !stats.is_dir() {
        return Err(ExplorerError::NotADirectory);
    }
    let workspace = expand_user_path(root);
    let mut entries = Vec::new();
    for dirent in fs::read_dir(&dir.resolved)? {
        let dirent = dirent?;
        let name = dirent.file_name().to_string_lossy().into_owned();
        let target = dir.requested.join(&name);
        // Dirent kind does not follow symlinks, so a link to a directory is a "file".
        let kind = if dirent.file_type()?.is_dir() {
            ExplorerEntryKind::Directory
        } else {
            ExplorerEntryKind::File
        };
        match build_entry_payload(root, &workspace, &target, name, kind) {
            Ok(entry) => entries.push(entry),
            // Dangling links and links leaving the workspace are skipped.
            Err(e) if e.is_missing() || e.is_outside_workspace() => {}
            Err(e) => return Err(e),
        }
    }
    entries.sort_by(|a, b| {
        parse_iso_ms_cmp(&b.modified_at, &a.modified_at)
            .then_with(|| locale_compare(&a.name, &b.name))
    });
    Ok(FileExplorerDirectory {
        path: normalize_relative_path(&workspace, &dir.requested),
        entries,
    })
}

// ISO strings from `iso_from_ms` sort lexicographically for years 1000-9999.
fn parse_iso_ms_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    a.cmp(b)
}

fn build_entry_payload(
    root: &str,
    workspace: &Path,
    target: &Path,
    name: String,
    kind: ExplorerEntryKind,
) -> Result<FileExplorerEntry> {
    let rel = normalize_relative_path(workspace, target);
    let scoped = resolve_scoped_path(root, &rel)?;
    let stats = fs::metadata(&scoped.resolved)?;
    Ok(FileExplorerEntry {
        name,
        path: rel,
        kind,
        size: stats.size(),
        modified_at: modified_at(&stats),
    })
}

#[derive(Debug, Clone)]
pub struct FileExplorerFileBytes {
    pub path: String,
    pub kind: ExplorerFileKind,
    pub encoding: FileEncoding,
    pub bytes: Vec<u8>,
    pub mime_type: String,
    pub size: u64,
    pub modified_at: String,
    pub revision: String,
}

pub fn read_explorer_file_bytes(params: ReadFileParams) -> Result<FileExplorerFileBytes> {
    let scoped = resolve_scoped_path(params.root, params.relative_path)?;
    let mut handle = open_for_read(&scoped.resolved)?;
    let stats = handle.metadata()?;
    if !stats.is_file() {
        return Err(ExplorerError::NotAFile);
    }
    let ext = ext_of(&scoped.resolved);
    let workspace = expand_user_path(params.root);
    let path = normalize_relative_path(&workspace, &scoped.requested);
    let size = stats.size();
    let modified = modified_at(&stats);
    let revision = file_revision(&stats);
    let mut buffer = Vec::new();
    handle.read_to_end(&mut buffer)?;

    let (kind, encoding, mime) = if let Some(mime) = image_mime(&ext) {
        (ExplorerFileKind::Image, FileEncoding::Binary, mime)
    } else if is_likely_binary(&buffer) || !is_valid_utf8(&buffer) {
        (
            ExplorerFileKind::Binary,
            FileEncoding::Binary,
            "application/octet-stream",
        )
    } else {
        (ExplorerFileKind::Text, FileEncoding::Utf8, text_mime(&ext))
    };
    Ok(FileExplorerFileBytes {
        path,
        kind,
        encoding,
        bytes: buffer,
        mime_type: mime.to_string(),
        size,
        modified_at: modified,
        revision,
    })
}

pub fn read_explorer_file(params: ReadFileParams) -> Result<FileExplorerFile> {
    let file = read_explorer_file_bytes(params)?;
    let (encoding, content) = match file.kind {
        ExplorerFileKind::Image => (ExplorerEncoding::Base64, Some(base64_encode(&file.bytes))),
        ExplorerFileKind::Binary => (ExplorerEncoding::None, None),
        ExplorerFileKind::Text => (
            ExplorerEncoding::Utf8,
            Some(String::from_utf8_lossy(&file.bytes).into_owned()),
        ),
    };
    Ok(FileExplorerFile {
        path: file.path,
        kind: file.kind,
        encoding,
        content,
        mime_type: Some(file.mime_type),
        size: file.size,
        modified_at: file.modified_at,
        revision: Some(file.revision),
    })
}

pub fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// Metadata for a streamed download, announced before the first chunk.
#[derive(Debug, Clone, PartialEq)]
pub struct FileStreamMeta {
    pub path: String,
    pub kind: ExplorerFileKind,
    pub encoding: FileEncoding,
    pub mime_type: String,
    pub size: u64,
    pub modified_at: String,
    pub revision: String,
}

/// Reads the advertised size in `FILE_EXPLORER_STREAM_CHUNK_BYTES` chunks and
/// fails with "File changed during transfer" when the file shrinks, or when the
/// revision differs after the last chunk.
pub struct ChunkReader {
    file: File,
    size: u64,
    revision: String,
    position: u64,
    finished: bool,
}

impl Iterator for ChunkReader {
    type Item = Result<Vec<u8>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }
        if self.position < self.size {
            let want =
                (self.size - self.position).min(FILE_EXPLORER_STREAM_CHUNK_BYTES as u64) as usize;
            let mut buf = vec![0u8; want];
            return match self.file.read_at(&mut buf, self.position) {
                Ok(0) => {
                    self.finished = true;
                    Some(Err(ExplorerError::FileChanged))
                }
                Ok(n) => {
                    self.position += n as u64;
                    buf.truncate(n);
                    Some(Ok(buf))
                }
                Err(e) => {
                    self.finished = true;
                    Some(Err(e.into()))
                }
            };
        }
        self.finished = true;
        match self.file.metadata() {
            Ok(m) if file_revision(&m) == self.revision => None,
            Ok(_) => Some(Err(ExplorerError::FileChanged)),
            Err(e) => Some(Err(e.into())),
        }
    }
}

pub struct FileStream {
    pub meta: FileStreamMeta,
    pub chunks: ChunkReader,
}

pub fn open_explorer_file_stream(params: ReadFileParams) -> Result<FileStream> {
    let scoped = resolve_scoped_path(params.root, params.relative_path)?;
    let file = open_for_read(&scoped.resolved)?;
    let stats = file.metadata()?;
    if !stats.is_file() {
        return Err(ExplorerError::NotAFile);
    }
    let size = stats.size();
    let revision = file_revision(&stats);
    let ext = ext_of(&scoped.resolved);
    let image = image_mime(&ext);
    let is_binary = image.is_some() || is_file_binary(&file, size)?;
    let (kind, mime) = if let Some(m) = image {
        (ExplorerFileKind::Image, m)
    } else if is_binary {
        (ExplorerFileKind::Binary, "application/octet-stream")
    } else {
        (ExplorerFileKind::Text, text_mime(&ext))
    };
    let workspace = expand_user_path(params.root);
    let meta = FileStreamMeta {
        path: normalize_relative_path(&workspace, &scoped.requested),
        kind,
        encoding: if is_binary {
            FileEncoding::Binary
        } else {
            FileEncoding::Utf8
        },
        mime_type: mime.to_string(),
        size,
        modified_at: modified_at(&stats),
        revision: revision.clone(),
    };
    Ok(FileStream {
        meta,
        chunks: ChunkReader {
            file,
            size,
            revision,
            position: 0,
            finished: false,
        },
    })
}

/// Whole-file classification: NUL byte, invalid UTF-8 (checked across block
/// boundaries) or more than 30% control bytes.
fn is_file_binary(file: &File, size: u64) -> Result<bool> {
    if size == 0 {
        return Ok(false);
    }
    let mut position = 0u64;
    let mut suspicious = 0u64;
    let mut carry: Vec<u8> = Vec::new();
    while position < size {
        let want = (size - position).min(FILE_EXPLORER_STREAM_CHUNK_BYTES as u64) as usize;
        let mut block = vec![0u8; want];
        let n = file.read_at(&mut block, position)?;
        if n == 0 {
            return Err(ExplorerError::FileChanged);
        }
        block.truncate(n);
        for &b in &block {
            if b == 0 {
                return Ok(true);
            }
            if (b < 32 && b != 9 && b != 10 && b != 13) || b == 127 {
                suspicious += 1;
            }
        }
        carry.extend_from_slice(&block);
        match std::str::from_utf8(&carry) {
            Ok(_) => carry.clear(),
            Err(e) if e.error_len().is_none() => {
                carry.drain(..e.valid_up_to());
            }
            Err(_) => return Ok(true),
        }
        position += n as u64;
    }
    if !carry.is_empty() {
        return Ok(true);
    }
    Ok(suspicious as f64 / size as f64 > 0.3)
}

/// Send a file as `FileBegin`, `FileChunk`..., `FileEnd` frames through `emit`.
/// Errors from the file (including "File changed during transfer") surface
/// after the frames already emitted, as in the TS session.
pub fn emit_file_transfer_frames<E: std::fmt::Display>(
    request_id: &str,
    params: ReadFileParams,
    mut emit: impl FnMut(Vec<u8>) -> std::result::Result<(), E>,
) -> Result<()> {
    let stream = open_explorer_file_stream(params)?;
    let frame_err = |e: crate::binary_frames::FrameError| ExplorerError::message(e.to_string());
    let emit_err = |e: E| ExplorerError::message(e.to_string());
    let meta = stream.meta;
    emit(
        encode_begin(
            request_id,
            FileBeginMetadata {
                mime: meta.mime_type,
                size: meta.size,
                encoding: meta.encoding,
                modified_at: meta.modified_at,
                revision: Some(meta.revision),
                file_name: None,
            },
        )
        .map_err(frame_err)?,
    )
    .map_err(emit_err)?;
    for chunk in stream.chunks {
        emit(encode_chunk(request_id, &chunk?).map_err(frame_err)?).map_err(emit_err)?;
    }
    emit(encode_end(request_id).map_err(frame_err)?).map_err(emit_err)?;
    Ok(())
}

pub fn get_explorer_file_version(params: ReadFileParams) -> ExplorerFileVersion {
    let cwd = path_str(&expand_user_path(params.root));
    let attempt = (|| -> Result<ExplorerFileVersion> {
        let scoped = resolve_scoped_path(params.root, params.relative_path)?;
        let stats = fs::metadata(&scoped.resolved)?;
        if !stats.is_file() {
            return Ok(ExplorerFileVersion::Error {
                cwd: cwd.clone(),
                path: params.relative_path.to_string(),
                error: ExplorerError::NotAFile.to_string(),
            });
        }
        let workspace = expand_user_path(params.root);
        Ok(ExplorerFileVersion::Ready {
            cwd: cwd.clone(),
            path: normalize_relative_path(&workspace, &scoped.requested),
            size: stats.size(),
            modified_at: modified_at(&stats),
            revision: file_revision(&stats),
        })
    })();
    match attempt {
        Ok(v) => v,
        Err(e) if e.is_missing() => ExplorerFileVersion::Missing {
            cwd,
            path: params.relative_path.to_string(),
        },
        Err(e) => ExplorerFileVersion::Error {
            cwd,
            path: params.relative_path.to_string(),
            error: e.to_string(),
        },
    }
}

pub fn resolve_explorer_file_path(params: ReadFileParams) -> Result<PathBuf> {
    Ok(resolve_scoped_path(params.root, params.relative_path)?.resolved)
}

fn matches_expected_revision(
    m: &Metadata,
    expected_modified_at: &str,
    expected_revision: Option<&str>,
) -> bool {
    match expected_revision.filter(|r| !r.is_empty()) {
        Some(rev) => file_revision(m) == rev,
        None => modified_at(m) == expected_modified_at,
    }
}

fn ready_version(root: &str, path: String, m: &Metadata) -> ExplorerFileVersion {
    ExplorerFileVersion::Ready {
        cwd: path_str(&expand_user_path(root)),
        path,
        size: m.size(),
        modified_at: modified_at(m),
        revision: file_revision(m),
    }
}

fn write_error(e: impl std::fmt::Display) -> ExplorerFileWriteResult {
    ExplorerFileWriteResult::Error {
        error: e.to_string(),
    }
}

/// Atomic write guarded by revision (or timestamp when no revision is given).
/// Never creates a missing file. The temp file keeps the original mode.
pub fn write_explorer_file(params: WriteFileParams) -> ExplorerFileWriteResult {
    let encoded = params.content.as_bytes();
    if encoded.len() as u64 > MAX_EDITABLE_FILE_BYTES {
        return write_error("File is too large to edit");
    }
    let root = params.root;
    let workspace = expand_user_path(root);

    let (scoped, current_mode) =
        match (|| -> Result<std::result::Result<(ScopedPath, u32), ExplorerFileWriteResult>> {
            let scoped = resolve_scoped_path(root, params.relative_path)?;
            let mut handle = open_for_read(&scoped.resolved)?;
            let stats = handle.metadata()?;
            if !stats.is_file() {
                return Ok(Err(write_error(ExplorerError::NotAFile)));
            }
            if stats.size() > MAX_EDITABLE_FILE_BYTES {
                return Ok(Err(write_error("File is too large to edit")));
            }
            let mut current = Vec::new();
            handle.read_to_end(&mut current)?;
            if is_likely_binary(&current) || !is_valid_utf8(&current) {
                return Ok(Err(write_error("Binary files cannot be edited")));
            }
            if !matches_expected_revision(
                &stats,
                params.expected_modified_at,
                params.expected_revision,
            ) {
                let path = normalize_relative_path(&workspace, &scoped.requested);
                return Ok(Err(ExplorerFileWriteResult::Conflict {
                    version: ready_version(root, path, &stats),
                }));
            }
            Ok(Ok((scoped, stats.mode())))
        })() {
            Ok(Ok(ok)) => ok,
            Ok(Err(result)) => return result,
            Err(e) if e.is_missing() => {
                return ExplorerFileWriteResult::Conflict {
                    version: ExplorerFileVersion::Missing {
                        cwd: path_str(&workspace),
                        path: params.relative_path.to_string(),
                    },
                }
            }
            Err(e) => return write_error(e),
        };

    let dir = scoped.resolved.parent().unwrap_or(Path::new("/"));
    let temporary = dir.join(format!(".paseo-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> Result<ExplorerFileWriteResult> {
        let mut tmp = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(current_mode & 0o7777)
            .open(&temporary)?;
        tmp.set_permissions(fs::Permissions::from_mode(current_mode & 0o7777))?;
        tmp.write_all(encoded)?;
        tmp.sync_all()?;
        drop(tmp);
        let latest = fs::metadata(&scoped.resolved)?;
        if !matches_expected_revision(
            &latest,
            params.expected_modified_at,
            params.expected_revision,
        ) {
            let path = normalize_relative_path(&workspace, &scoped.requested);
            return Ok(ExplorerFileWriteResult::Conflict {
                version: ready_version(root, path, &latest),
            });
        }
        fs::rename(&temporary, &scoped.resolved)?;
        let stats = fs::metadata(&scoped.resolved)?;
        Ok(ExplorerFileWriteResult::Written {
            modified_at: modified_at(&stats),
            size: stats.size(),
            revision: file_revision(&stats),
        })
    })();
    let _ = fs::remove_file(&temporary);
    result.unwrap_or_else(write_error)
}

#[derive(Debug, Clone, PartialEq)]
pub struct DownloadableFileInfo {
    pub path: String,
    pub absolute_path: String,
    pub file_name: String,
    pub mime_type: String,
    pub size: u64,
}

pub fn get_downloadable_file_info(params: ReadFileParams) -> Result<DownloadableFileInfo> {
    let scoped = resolve_scoped_path(params.root, params.relative_path)?;
    let file = open_for_read(&scoped.resolved)?;
    let stats = file.metadata()?;
    if !stats.is_file() {
        return Err(ExplorerError::NotAFile);
    }
    let ext = ext_of(&scoped.resolved);
    let mime = if let Some(m) = image_mime(&ext) {
        m
    } else {
        let mut sample = vec![0u8; FILE_TYPE_SAMPLE_BYTES];
        let n = file.read_at(&mut sample, 0)?;
        sample.truncate(n);
        if is_likely_binary(&sample) {
            "application/octet-stream"
        } else {
            text_mime(&ext)
        }
    };
    let workspace = expand_user_path(params.root);
    Ok(DownloadableFileInfo {
        path: normalize_relative_path(&workspace, &scoped.requested),
        absolute_path: path_str(&scoped.resolved),
        file_name: crate::path_utils::basename(&path_str(&scoped.requested)).to_string(),
        mime_type: mime.to_string(),
        size: stats.size(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_reference() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn binary_heuristic() {
        assert!(!is_likely_binary(b""));
        assert!(is_likely_binary(&[0x48, 0x00]));
        assert!(!is_likely_binary(b"hello\n\t"));
        assert!(is_likely_binary(&[1, 2, 3, b'a']));
    }
}
