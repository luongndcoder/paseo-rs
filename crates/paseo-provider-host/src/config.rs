use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;

/// `host.initialize` parameters.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeOptions {
    /// Enable dev-only providers such as `mock`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_dev: Option<bool>,
    /// Restrict the registry to these provider ids.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub providers: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_overrides: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime_settings: Option<Value>,
}

/// How to launch the TS host and how long to wait on it.
#[derive(Debug, Clone)]
pub struct HostConfig {
    /// Executable, normally `node`.
    pub command: PathBuf,
    /// Arguments, normally `--import tsx <entry>` or `<dist entry>`.
    pub args: Vec<OsString>,
    /// Working directory (the `packages/server` directory so `tsx` and deps resolve).
    pub cwd: Option<PathBuf>,
    pub env: Vec<(String, String)>,
    pub initialize: InitializeOptions,
    /// Applied to control-plane calls. Long calls (run, createSession, fetchCatalog, ...) are
    /// never timed out by the client.
    pub control_timeout: Option<Duration>,
    /// How long `shutdown` waits for the process to exit before killing it.
    pub shutdown_grace: Duration,
}

impl HostConfig {
    pub fn new(
        command: impl Into<PathBuf>,
        args: impl IntoIterator<Item = impl Into<OsString>>,
    ) -> Self {
        Self {
            command: command.into(),
            args: args.into_iter().map(Into::into).collect(),
            cwd: None,
            env: Vec::new(),
            initialize: InitializeOptions::default(),
            control_timeout: Some(Duration::from_secs(30)),
            shutdown_grace: Duration::from_secs(5),
        }
    }

    /// Run the TS entry through `tsx`: `node --import tsx <entry>` with `server_dir` as cwd.
    pub fn node_tsx(node: impl Into<PathBuf>, server_dir: &Path, entry: impl AsRef<Path>) -> Self {
        let mut config = Self::new(
            node,
            [
                OsString::from("--import"),
                OsString::from("tsx"),
                entry.as_ref().as_os_str().to_owned(),
            ],
        );
        config.cwd = Some(server_dir.to_path_buf());
        config
    }

    pub fn with_env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    pub fn with_initialize(mut self, initialize: InitializeOptions) -> Self {
        self.initialize = initialize;
        self
    }
}
