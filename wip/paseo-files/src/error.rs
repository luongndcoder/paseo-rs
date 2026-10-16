use std::fmt;
use std::io;

pub const ACCESS_OUTSIDE_WORKSPACE_MESSAGE: &str = "Access outside of workspace is not allowed";

/// Error raised by file explorer operations. `Display` yields the message the
/// TS daemon puts on the wire.
#[derive(Debug)]
pub enum ExplorerError {
    OutsideWorkspace,
    NotADirectory,
    NotAFile,
    FileChanged,
    Io(io::Error),
    Message(String),
}

impl ExplorerError {
    pub fn message(msg: impl Into<String>) -> Self {
        ExplorerError::Message(msg.into())
    }

    /// ENOENT, ENOTDIR or ELOOP, the codes the TS service treats as "missing".
    pub fn is_missing(&self) -> bool {
        match self {
            ExplorerError::Io(e) => io_is_missing(e),
            _ => false,
        }
    }

    pub fn is_outside_workspace(&self) -> bool {
        matches!(self, ExplorerError::OutsideWorkspace)
    }

    pub fn is_exists(&self) -> bool {
        matches!(self, ExplorerError::Io(e) if e.raw_os_error() == Some(libc::EEXIST))
    }
}

pub fn io_is_missing(e: &io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(code) if code == libc::ENOENT || code == libc::ENOTDIR || code == libc::ELOOP
    )
}

/// Node-style `CODE: description` prefix for common errno values.
pub fn describe_io(e: &io::Error) -> String {
    let (code, text) = match e.raw_os_error() {
        Some(c) if c == libc::ENOENT => ("ENOENT", "no such file or directory"),
        Some(c) if c == libc::EACCES => ("EACCES", "permission denied"),
        Some(c) if c == libc::ENOTDIR => ("ENOTDIR", "not a directory"),
        Some(c) if c == libc::EEXIST => ("EEXIST", "file already exists"),
        Some(c) if c == libc::EISDIR => ("EISDIR", "illegal operation on a directory"),
        Some(c) if c == libc::ELOOP => ("ELOOP", "too many symbolic links encountered"),
        Some(c) if c == libc::EPERM => ("EPERM", "operation not permitted"),
        Some(c) if c == libc::ENOTEMPTY => ("ENOTEMPTY", "directory not empty"),
        _ => return e.to_string(),
    };
    format!("{code}: {text}")
}

impl fmt::Display for ExplorerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExplorerError::OutsideWorkspace => f.write_str(ACCESS_OUTSIDE_WORKSPACE_MESSAGE),
            ExplorerError::NotADirectory => f.write_str("Requested path is not a directory"),
            ExplorerError::NotAFile => f.write_str("Requested path is not a file"),
            ExplorerError::FileChanged => f.write_str("File changed during transfer"),
            ExplorerError::Io(e) => f.write_str(&describe_io(e)),
            ExplorerError::Message(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for ExplorerError {}

impl From<io::Error> for ExplorerError {
    fn from(e: io::Error) -> Self {
        ExplorerError::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, ExplorerError>;
