//! Launch options for the `opencode serve` child process.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

/// Budget for the server to print `listening on` after spawn (matches the TS adapter).
pub const SERVER_STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
/// One stalled SSE attempt plus enough time for a retry.
pub const EVENT_STREAM_READY_TIMEOUT: Duration = Duration::from_secs(45);

/// How to launch OpenCode, built with chained setters.
///
/// ```
/// let opts = paseo_opencode::OpenCodeOptions::new().binary("/usr/local/bin/opencode").env("FOO", "bar");
/// assert_eq!(opts.binary_override().unwrap().to_str(), Some("/usr/local/bin/opencode"));
/// ```
#[derive(Debug, Clone)]
pub struct OpenCodeOptions {
    binary: Option<PathBuf>,
    prefix_args: Vec<String>,
    env: HashMap<String, String>,
    home_dir: PathBuf,
    startup_timeout: Duration,
    password: PasswordMode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PasswordMode {
    /// Generate a random password per server (default).
    Random,
    /// Use this password.
    Fixed(String),
    /// Run unsecured (what the 1.x TS adapter does).
    None,
}

impl Default for OpenCodeOptions {
    fn default() -> Self {
        Self {
            binary: None,
            prefix_args: Vec::new(),
            env: HashMap::new(),
            home_dir: default_home_dir(),
            startup_timeout: SERVER_STARTUP_TIMEOUT,
            password: PasswordMode::Random,
        }
    }
}

/// `$PASEO_HOME/opencode-home` (neutral server cwd; launching from `$HOME` makes OpenCode index it).
fn default_home_dir() -> PathBuf {
    let paseo_home = std::env::var_os("PASEO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".paseo")))
        .unwrap_or_else(|| PathBuf::from(".paseo"));
    paseo_home.join("opencode-home")
}

impl OpenCodeOptions {
    pub fn new() -> Self {
        Self::default()
    }

    /// Path or command name of the `opencode` executable (default: looked up on `PATH`).
    pub fn binary(mut self, path: impl Into<PathBuf>) -> Self {
        self.binary = Some(path.into());
        self
    }

    /// Arguments placed before `serve` (for wrapper commands).
    pub fn prefix_args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.prefix_args = args.into_iter().map(Into::into).collect();
        self
    }

    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }

    /// Working directory of the server process.
    pub fn home_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.home_dir = dir.into();
        self
    }

    pub fn startup_timeout(mut self, timeout: Duration) -> Self {
        self.startup_timeout = timeout;
        self
    }

    pub fn password(mut self, password: impl Into<String>) -> Self {
        self.password = PasswordMode::Fixed(password.into());
        self
    }

    /// Run the server without HTTP basic auth.
    pub fn unsecured(mut self) -> Self {
        self.password = PasswordMode::None;
        self
    }

    pub fn binary_override(&self) -> Option<&PathBuf> {
        self.binary.as_ref()
    }

    pub fn prefix_args_ref(&self) -> &[String] {
        &self.prefix_args
    }

    pub fn env_ref(&self) -> &HashMap<String, String> {
        &self.env
    }

    pub fn home_dir_ref(&self) -> &PathBuf {
        &self.home_dir
    }

    pub fn startup_timeout_value(&self) -> Duration {
        self.startup_timeout
    }

    /// Resolves the password for one server: `None` when unsecured.
    pub(crate) fn resolve_password(&self) -> Option<String> {
        match &self.password {
            PasswordMode::Random => Some(random_token()),
            PasswordMode::Fixed(p) => Some(p.clone()),
            PasswordMode::None => None,
        }
    }
}

/// 32 random bytes as URL-safe hex.
pub(crate) fn random_token() -> String {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).expect("OS randomness is available");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
