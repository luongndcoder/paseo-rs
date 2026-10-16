//! Atomic file writes: write a unique sibling temp file, then rename over the target so
//! readers never observe a partial file.
//!
//! Ports `atomic-file.ts` ([`write_file_atomic`], [`write_json_file_atomic`]) and
//! `private-files.ts` ([`write_private_atomic`]). Naming and output bytes match TS so both
//! runtimes can share `$PASEO_HOME` during the strangler phase.

use serde::Serialize;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// How hard a write tries to survive power loss.
///
/// TS never fsyncs, so `Off` is the parity default (spec Q5). `File` syncs the temp file
/// before the rename; `FileAndDir` also syncs the parent directory after it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FsyncPolicy {
    #[default]
    Off,
    File,
    FileAndDir,
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn parent_dir(path: &Path) -> &Path {
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    }
}

fn base_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// `.<base>.<pid>.<ms>.<uuid>.tmp` in the target's directory (TS `writeFileAtomic`).
fn temp_path(path: &Path) -> PathBuf {
    parent_dir(path).join(format!(
        ".{}.{}.{}.{}.tmp",
        base_name(path),
        std::process::id(),
        now_ms(),
        uuid::Uuid::new_v4()
    ))
}

fn sync_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        fs::File::open(dir)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

fn write_temp(temp: &Path, bytes: &[u8], policy: FsyncPolicy) -> io::Result<()> {
    let mut file = fs::File::create(temp)?;
    file.write_all(bytes)?;
    if policy != FsyncPolicy::Off {
        file.sync_all()?;
    }
    Ok(())
}

fn commit(temp: &Path, dest: &Path, policy: FsyncPolicy) -> io::Result<()> {
    fs::rename(temp, dest)?;
    if policy == FsyncPolicy::FileAndDir {
        sync_dir(parent_dir(dest))?;
    }
    Ok(())
}

/// Writes `bytes` to `path` atomically, creating parent directories. The temp file is
/// removed on any error.
pub fn write_file_atomic(path: &Path, bytes: &[u8], policy: FsyncPolicy) -> io::Result<()> {
    fs::create_dir_all(parent_dir(path))?;
    let temp = temp_path(path);
    let result = write_temp(&temp, bytes, policy).and_then(|_| commit(&temp, path, policy));
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

/// `JSON.stringify(value, null, 2)`: two-space indent, no trailing newline.
pub fn write_json_file_atomic<T: Serialize>(
    path: &Path,
    value: &T,
    policy: FsyncPolicy,
) -> io::Result<()> {
    let text = serde_json::to_string_pretty(value).map_err(io::Error::other)?;
    write_file_atomic(path, text.as_bytes(), policy)
}

/// Private variant (TS `writePrivateFileAtomicSync`): directory 0700, file 0600, and a
/// symlinked destination is written through (rename onto a link would replace the link).
/// Permission failures on chmod are ignored, as in TS; no fsync, as in TS.
pub fn write_private_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    ensure_private_directory(parent_dir(path))?;
    let dest = match fs::canonicalize(path) {
        Ok(real) => real,
        Err(e) if e.kind() == io::ErrorKind::NotFound => path.to_path_buf(),
        Err(e) => return Err(e),
    };
    let temp = parent_dir(&dest).join(format!(
        ".{}.{}.{}",
        base_name(&dest),
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    let result = write_private_temp(&temp, bytes).and_then(|_| {
        fs::rename(&temp, &dest)?;
        apply_mode(&dest, 0o600);
        Ok(())
    });
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn write_private_temp(temp: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(temp)?.write_all(bytes)
}

fn ensure_private_directory(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
    }
    #[cfg(not(unix))]
    fs::create_dir_all(dir)?;
    apply_mode(dir, 0o700);
    Ok(())
}

fn apply_mode(path: &Path, mode: u32) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
}

/// Renames a corrupt file to `<name>.corrupt-<unix ms>` (suffixed `-<n>` on collision) and
/// returns the new path. Callers decide whether to use it (spec Q1: parity phase does not).
pub fn quarantine_corrupt(path: &Path) -> io::Result<PathBuf> {
    let base = base_name(path);
    let ts = now_ms();
    let mut n = 0u32;
    loop {
        let name = if n == 0 {
            format!("{base}.corrupt-{ts}")
        } else {
            format!("{base}.corrupt-{ts}-{n}")
        };
        let target = parent_dir(path).join(name);
        if !target.exists() {
            fs::rename(path, &target)?;
            return Ok(target);
        }
        n += 1;
    }
}

#[cfg(test)]
mod tests;
