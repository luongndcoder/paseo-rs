//! Recursive file observation for the Paseo daemon, ported from
//! `packages/server/src/server/file-observer` (see `docs/file-observation.md`),
//! plus the git-root observers.
//!
//! - [`FileObserver`]: service owning subscriptions; one dedicated thread per
//!   observation, `notify` for native handles.
//! - [`ProjectRootObserver`]: non-recursive watch on each project root,
//!   reacting only to the root `.git` entry, with a slow rescan fallback.
//! - [`WorkspaceGitObserverService`]: per-cwd git subscription ownership and
//!   branch-change fan-out.
//!
//! Serde shapes (`FileChange`, `FileObserverDiagnostics`, `WorkspaceGitObserverMetrics`)
//! match the TS wire/diagnostic JSON exactly.

pub mod backend;
mod error;
mod host;
mod linux;
mod native_recursive;
mod observation;
mod paths;
pub mod project_root;
mod service;
mod types;
mod watcher;
pub mod workspace_git_observer;

pub use backend::Timings;
pub use error::FileObserverError;
pub use observation::{BackendKind, FileObserverCallback};
pub use paths::{ObserverPaths, PathPlatform};
pub use project_root::{ProjectRootObserver, ProjectRootSignal};
pub use service::{FileObserver, FileObserverConfig, FileObserverSubscription};
pub use types::{FileChange, FileChangeType, FileObserverDiagnostics, FileObserverOptions};
pub use workspace_git_observer::{
    GitSnapshot, WorkspaceDescriptor, WorkspaceGitObserverMetrics, WorkspaceGitObserverService,
    WorkspaceGitService, WorkspaceKind,
};
