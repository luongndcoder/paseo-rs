//! Small filesystem helpers that reproduce the Node semantics the TS module relies on
//! (`fs.rm({recursive, force})`, `fs.cp` with `verbatimSymlinks`, `.catch(() => null)`).

use sha2::{Digest, Sha256};
use std::cmp::Ordering;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// `fs.lstat(p).catch(() => null)`.
pub(crate) fn lstat(path: &Path) -> Option<fs::Metadata> {
    fs::symlink_metadata(path).ok()
}

/// `stat(target).catch(() => null)?.isDirectory() === true` (follows symlinks).
pub(crate) fn is_directory(path: &Path) -> bool {
    fs::metadata(path).map(|m| m.is_dir()).unwrap_or(false)
}

/// `fs.rm(p, {recursive: true, force: true})`: a missing path is fine, a symlink is removed
/// as a link, every other error propagates.
pub(crate) fn rm_rf(path: &Path) -> io::Result<()> {
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    let result = if meta.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    };
    match result {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// `fs.rm(p, {force: true})` for one file: missing is fine.
pub(crate) fn rm_file_force(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// `readdir(p).catch(() => [])`, names sorted for determinism (Node order is arbitrary).
pub(crate) fn read_dir_names(path: &Path) -> Vec<String> {
    let mut names: Vec<String> = match fs::read_dir(path) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect(),
        Err(_) => Vec::new(),
    };
    names.sort_by(|a, b| js_cmp(a, b));
    names
}

/// JS string ordering: UTF-16 code units.
pub(crate) fn js_cmp(a: &str, b: &str) -> Ordering {
    a.encode_utf16().cmp(b.encode_utf16())
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

#[cfg(unix)]
pub(crate) fn mode_bits(meta: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o777
}

#[cfg(not(unix))]
pub(crate) fn mode_bits(_meta: &fs::Metadata) -> u32 {
    0
}

#[cfg(unix)]
fn set_mode(path: &Path, meta: &fs::Metadata) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(meta.permissions().mode()));
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _meta: &fs::Metadata) {}

fn set_times(path: &Path, meta: &fs::Metadata) {
    let (Ok(modified), Ok(accessed)) = (meta.modified(), meta.accessed()) else {
        return;
    };
    let times = fs::FileTimes::new()
        .set_modified(modified)
        .set_accessed(accessed);
    if let Ok(file) = fs::File::open(path) {
        let _ = file.set_times(times);
    }
}

#[cfg(unix)]
fn symlink(target: &Path, link: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn symlink(target: &Path, link: &Path) -> io::Result<()> {
    std::os::windows::fs::symlink_dir(target, link)
}

/// `fs.cp(src, dst, {recursive, preserveTimestamps: true, verbatimSymlinks: true})`: copies
/// files with their mode, recreates symlinks with the same link text, preserves
/// timestamps, creates missing parents and overwrites existing files.
pub(crate) fn copy_entry(src: &Path, dst: &Path) -> io::Result<()> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }
    let meta = fs::symlink_metadata(src)?;
    if meta.file_type().is_symlink() {
        let target = fs::read_link(src)?;
        if lstat(dst).is_some() {
            rm_rf(dst)?;
        }
        return symlink(&target, dst);
    }
    if meta.is_dir() {
        match fs::create_dir(dst) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists && is_directory(dst) => {}
            Err(e) => return Err(e),
        }
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            copy_entry(&entry.path(), &dst.join(entry.file_name()))?;
        }
        set_mode(dst, &meta);
        set_times(dst, &meta);
        return Ok(());
    }
    if !meta.is_file() {
        // Node fs.cp rejects FIFOs/sockets/devices; fs::copy would block on a FIFO.
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("cannot copy special file: {}", src.display()),
        ));
    }
    fs::copy(src, dst)?;
    set_mode(dst, &meta);
    set_times(dst, &meta);
    Ok(())
}

pub(crate) fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

pub(crate) fn base_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

pub(crate) fn dir_name(path: &Path) -> PathBuf {
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

/// `path.join(root, rel)` where `rel` may be `"."`.
pub(crate) fn join_rel(root: &Path, rel: &str) -> PathBuf {
    if rel == "." {
        root.to_path_buf()
    } else {
        root.join(rel)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn copy_entry_rejects_fifo_without_blocking() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        fs::create_dir(&src).unwrap();
        let fifo = src.join("pipe");
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(status.success());
        let err = copy_entry(&src, &dir.path().join("dst")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }
}
