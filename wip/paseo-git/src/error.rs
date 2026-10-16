//! Error type and the checkout wire error payload.

use serde::Serialize;

#[derive(Debug, Clone, thiserror::Error)]
pub enum GitError {
    #[error("Not a git repository: {cwd}")]
    NotGitRepo { cwd: String },
    #[error("Merge conflict while merging {current_branch} into {base_ref}")]
    MergeConflict {
        base_ref: String,
        current_branch: String,
        conflict_files: Vec<String>,
    },
    #[error(
        "Merge conflict while merging {base_ref} into {current_branch}. Please merge manually."
    )]
    MergeFromBaseConflict {
        base_ref: String,
        current_branch: String,
        conflict_files: Vec<String>,
    },
    /// A failed `git` invocation. `message` matches the TS error text:
    /// `Git command failed: git <args> (exit code: N, signal: none)\n<stderr>`.
    #[error("{message}")]
    Command {
        message: String,
        stderr: String,
        stdout: String,
    },
    #[error("{0}")]
    Other(String),
}

impl GitError {
    pub fn other(message: impl Into<String>) -> Self {
        GitError::Other(message.into())
    }

    pub fn stderr(&self) -> &str {
        match self {
            GitError::Command { stderr, .. } => stderr,
            _ => "",
        }
    }

    pub fn stdout(&self) -> &str {
        match self {
            GitError::Command { stdout, .. } => stdout,
            _ => "",
        }
    }
}

impl From<std::io::Error> for GitError {
    fn from(error: std::io::Error) -> Self {
        GitError::Other(error.to_string())
    }
}

pub type Result<T> = std::result::Result<T, GitError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum CheckoutErrorCode {
    #[serde(rename = "NOT_GIT_REPO")]
    NotGitRepo,
    #[serde(rename = "NOT_ALLOWED")]
    NotAllowed,
    #[serde(rename = "MERGE_CONFLICT")]
    MergeConflict,
    #[serde(rename = "UNKNOWN")]
    Unknown,
}

/// `{ code, message }` as carried by every checkout_* response payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CheckoutErrorPayload {
    pub code: CheckoutErrorCode,
    pub message: String,
}

pub fn to_checkout_error(error: &GitError) -> CheckoutErrorPayload {
    let code = match error {
        GitError::NotGitRepo { .. } => CheckoutErrorCode::NotGitRepo,
        GitError::MergeConflict { .. } | GitError::MergeFromBaseConflict { .. } => {
            CheckoutErrorCode::MergeConflict
        }
        _ => CheckoutErrorCode::Unknown,
    };
    CheckoutErrorPayload {
        code,
        message: error.to_string(),
    }
}
