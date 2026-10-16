use std::io;
use std::path::PathBuf;

/// Errors surfaced by the observer. `Clone` so a failure can be both delivered
/// to the subscription callback and returned from the barrier that caused it.
#[derive(Debug, Clone, thiserror::Error)]
pub enum FileObserverError {
    #[error("File observer is closed")]
    Closed,
    #[error("File observation requires a directory: {}", .0.display())]
    NotADirectory(PathBuf),
    #[error("Recursive file observation exceeded {limit} directories under {}", .root.display())]
    DirectoryLimit { limit: usize, root: PathBuf },
    #[error("Recursive file observation exceeded {limit} entries under {}", .root.display())]
    EntryLimit { limit: usize, root: PathBuf },
    #[error("Recursive file observation exceeded {limit} queued classifications under {}", .root.display())]
    ClassificationLimit { limit: usize, root: PathBuf },
    #[error("{message}")]
    Io {
        kind: io::ErrorKind,
        message: String,
    },
    #[error("watcher error: {message}")]
    Watch {
        message: String,
        paths: Vec<PathBuf>,
        missing: bool,
        permission_denied: bool,
    },
}

impl FileObserverError {
    /// ENOENT / ENOTDIR: the path is gone (rename or archive churn).
    pub fn is_missing_path(&self) -> bool {
        match self {
            Self::Io { kind, .. } => {
                matches!(kind, io::ErrorKind::NotFound | io::ErrorKind::NotADirectory)
            }
            Self::Watch { missing, .. } => *missing,
            _ => false,
        }
    }

    /// Paths a watcher error is attributed to (empty when unknown).
    pub fn watch_paths(&self) -> &[PathBuf] {
        match self {
            Self::Watch { paths, .. } => paths,
            _ => &[],
        }
    }

    pub(crate) fn is_permission_denied(&self) -> bool {
        match self {
            Self::Io { kind, .. } => *kind == io::ErrorKind::PermissionDenied,
            Self::Watch {
                permission_denied, ..
            } => *permission_denied,
            _ => false,
        }
    }
}

impl From<io::Error> for FileObserverError {
    fn from(error: io::Error) -> Self {
        Self::Io {
            kind: error.kind(),
            message: error.to_string(),
        }
    }
}

impl From<notify::Error> for FileObserverError {
    fn from(error: notify::Error) -> Self {
        let io_kind = match &error.kind {
            notify::ErrorKind::Io(io) => Some(io.kind()),
            _ => None,
        };
        let missing = matches!(error.kind, notify::ErrorKind::PathNotFound)
            || matches!(
                io_kind,
                Some(io::ErrorKind::NotFound | io::ErrorKind::NotADirectory)
            );
        Self::Watch {
            message: error.to_string(),
            paths: error.paths,
            missing,
            permission_denied: io_kind == Some(io::ErrorKind::PermissionDenied),
        }
    }
}
