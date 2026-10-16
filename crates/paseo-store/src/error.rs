//! Error type shared by every store in this crate.

use std::io;

/// Stable codes of the workspace label service errors (`WorkspaceLabelError.code` in TS).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LabelErrorCode {
    LabelNameEmpty,
    LabelNotFound,
    LabelNameTaken,
    WorkspaceNotFound,
}

impl LabelErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            LabelErrorCode::LabelNameEmpty => "label_name_empty",
            LabelErrorCode::LabelNotFound => "label_not_found",
            LabelErrorCode::LabelNameTaken => "label_name_taken",
            LabelErrorCode::WorkspaceNotFound => "workspace_not_found",
        }
    }
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum StoreError {
    #[error("{message}")]
    Io {
        kind: io::ErrorKind,
        message: String,
    },
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    NotFound(String),
    #[error("Workspace registry mutations are blocked until daemon restart")]
    RegistryBlocked,
    #[error("Workspace label storage outcome is uncertain; restart the daemon before retrying")]
    LabelStorageUncertain,
    #[error("{message}")]
    Label {
        code: LabelErrorCode,
        message: String,
    },
    /// A post-commit listener rejected (project registry only; the write already happened).
    #[error("{0}")]
    Listener(String),
    #[error("{0}")]
    Other(String),
}

impl StoreError {
    pub fn code(&self) -> Option<&'static str> {
        match self {
            StoreError::LabelStorageUncertain => Some("workspace_label_storage_uncertain"),
            StoreError::Label { code, .. } => Some(code.as_str()),
            _ => None,
        }
    }

    pub fn other(message: impl Into<String>) -> Self {
        StoreError::Other(message.into())
    }
}

impl From<io::Error> for StoreError {
    fn from(error: io::Error) -> Self {
        StoreError::Io {
            kind: error.kind(),
            message: error.to_string(),
        }
    }
}

impl From<serde_json::Error> for StoreError {
    fn from(error: serde_json::Error) -> Self {
        StoreError::Invalid(error.to_string())
    }
}

pub type Result<T> = std::result::Result<T, StoreError>;
