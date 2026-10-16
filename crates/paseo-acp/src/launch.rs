//! Launch description for an agent process plus the helpers the TS `GenericACPAgentClient`
//! uses around it (version probe command, availability check).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Env vars of a parent Paseo/Claude session that must not leak into a provider process
/// (`PARENT_SESSION_ENV_VARS` in provider-launch-config.ts).
pub const PARENT_SESSION_ENV_VARS: &[&str] = &[
    "CLAUDECODE",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_SSE_PORT",
    "CLAUDE_AGENT_SDK_VERSION",
];

/// Env overlay applied while probing models/modes. Gemini CLI honors it to avoid opening a
/// browser sign-in; other agents ignore it.
pub fn probe_env() -> BTreeMap<String, String> {
    BTreeMap::from([("NO_BROWSER".to_string(), "true".to_string())])
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct LaunchSpec {
    pub command: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    /// Overlay on top of the inherited environment.
    pub env: BTreeMap<String, String>,
    pub env_remove: Vec<String>,
}

impl LaunchSpec {
    pub fn new(
        command: impl Into<String>,
        args: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        LaunchSpec {
            command: command.into(),
            args: args.into_iter().map(Into::into).collect(),
            cwd: None,
            env: BTreeMap::new(),
            env_remove: PARENT_SESSION_ENV_VARS
                .iter()
                .map(|s| s.to_string())
                .collect(),
        }
    }

    pub fn from_argv(argv: &[String]) -> Option<Self> {
        let (first, rest) = argv.split_first()?;
        Some(Self::new(first.clone(), rest.iter().cloned()))
    }

    pub fn with_cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    pub fn with_env(mut self, env: &BTreeMap<String, String>) -> Self {
        self.env
            .extend(env.iter().map(|(k, v)| (k.clone(), v.clone())));
        self
    }
}

/// How the user configured the agent command (`ProviderCommand` in the TS settings).
#[derive(Debug, Clone, PartialEq, Default)]
pub enum CommandConfig {
    /// Run the profile's default binary.
    #[default]
    Default,
    /// Run the default binary with extra leading args.
    Append(Vec<String>),
    /// Replace the launcher; the profile's default args (everything after argv[0]) still follow.
    Replace(Vec<String>),
}

/// Resolve the launch command: config prefix plus `default_command[1..]`
/// (`resolveLaunchCommand` in the TS client).
pub fn resolve_launch(
    config: &CommandConfig,
    default_command: &[String],
) -> Option<(String, Vec<String>)> {
    let default_rest = default_command.get(1..).unwrap_or(&[]);
    match config {
        CommandConfig::Replace(argv) => {
            let (cmd, rest) = argv.split_first()?;
            Some((
                cmd.clone(),
                rest.iter().chain(default_rest).cloned().collect(),
            ))
        }
        CommandConfig::Append(args) => {
            let cmd = default_command.first()?.clone();
            Some((cmd, args.iter().chain(default_rest).cloned().collect()))
        }
        CommandConfig::Default => {
            let cmd = default_command.first()?.clone();
            Some((cmd, default_rest.to_vec()))
        }
    }
}

/// Find an executable on `PATH` (or accept an explicit path that exists).
pub fn find_executable(command: &str, env: &BTreeMap<String, String>) -> Option<PathBuf> {
    let candidate = Path::new(command);
    if candidate.components().count() > 1 {
        return candidate.is_file().then(|| candidate.to_path_buf());
    }
    let path_var = env
        .get("PATH")
        .cloned()
        .or_else(|| std::env::var("PATH").ok())?;
    for dir in std::env::split_paths(&path_var) {
        let full = dir.join(command);
        if is_executable(&full) {
            return Some(full);
        }
        if cfg!(windows) {
            for ext in ["exe", "cmd", "bat"] {
                let with_ext = dir.join(format!("{command}.{ext}"));
                if with_ext.is_file() {
                    return Some(with_ext);
                }
            }
        }
    }
    None
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.metadata()
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(p: &Path) -> bool {
    p.is_file()
}

#[derive(Debug, Clone, PartialEq)]
pub struct CommandInvocation {
    pub command: String,
    pub args: Vec<String>,
}

/// `buildVersionProbeCommand`: `<launcher> --version`, keeping a package-runner prefix
/// (`npx -y pkg`, `uvx --from x pkg`, `pnpm dlx pkg`) so the version of the package is probed.
pub fn build_version_probe_command(command: &[String]) -> Option<CommandInvocation> {
    let (launcher, args) = command.split_first()?;
    let mut out = if is_package_runner(launcher) {
        take_package_runner_prefix(args)
    } else {
        Vec::new()
    };
    out.push("--version".to_string());
    Some(CommandInvocation {
        command: launcher.clone(),
        args: out,
    })
}

fn is_package_runner(command: &str) -> bool {
    matches!(command, "npx" | "bunx" | "pnpm" | "uvx")
}

fn take_package_runner_prefix(args: &[String]) -> Vec<String> {
    match args.first() {
        None => Vec::new(),
        Some(first) if first == "dlx" => {
            let mut out = vec!["dlx".to_string()];
            out.extend(take_package_spec_prefix(&args[1..]));
            out
        }
        Some(_) => take_package_spec_prefix(args),
    }
}

fn take_package_spec_prefix(args: &[String]) -> Vec<String> {
    let mut prefix = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        prefix.push(arg.clone());
        if arg == "--package" || arg == "-p" {
            if let Some(next) = args.get(i + 1).filter(|s| !s.is_empty()) {
                prefix.push(next.clone());
                i += 1;
            }
            i += 1;
            continue;
        }
        if !arg.starts_with('-') {
            break;
        }
        i += 1;
    }
    prefix
}
