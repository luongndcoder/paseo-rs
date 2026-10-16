//! Options builder and argv construction. Mirrors `buildPiLaunch` / `buildOmpLaunch`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::flavor::Flavor;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolMode {
    Rpc,
    RpcUi,
}

impl ProtocolMode {
    pub fn as_arg(self) -> &'static str {
        match self {
            ProtocolMode::Rpc => "rpc",
            ProtocolMode::RpcUi => "rpc-ui",
        }
    }
}

/// Everything needed to start one provider process.
#[derive(Debug, Clone)]
pub struct Options {
    pub flavor: Flavor,
    pub command: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: BTreeMap<String, String>,
    pub protocol_mode: ProtocolMode,
    pub model: Option<String>,
    pub thinking: Option<String>,
    pub session: Option<String>,
    pub no_session: bool,
    /// Pi only.
    pub mcp_config_path: Option<PathBuf>,
    /// Pi only.
    pub extension_paths: Vec<PathBuf>,
    /// OMP only (`--append-system-prompt`).
    pub system_prompt: Option<String>,
    pub extra_args: Vec<String>,
    pub request_timeout: Duration,
    pub ready_timeout: Duration,
}

/// A fully resolved process invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    pub argv: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: BTreeMap<String, String>,
}

impl Options {
    pub fn new(flavor: Flavor) -> Self {
        Self {
            flavor,
            command: flavor.default_command(),
            cwd: None,
            env: BTreeMap::new(),
            protocol_mode: ProtocolMode::Rpc,
            model: None,
            thinking: None,
            session: None,
            no_session: false,
            mcp_config_path: None,
            extension_paths: Vec::new(),
            system_prompt: None,
            extra_args: Vec::new(),
            request_timeout: Duration::from_secs(30),
            ready_timeout: Duration::from_secs(20),
        }
    }

    pub fn pi() -> Self {
        Self::new(Flavor::Pi)
    }

    pub fn omp() -> Self {
        Self::new(Flavor::Omp)
    }

    /// Replace the binary (and any leading args), e.g. `["node", "tee.mjs"]`.
    pub fn command<I, S>(mut self, argv: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.command = argv.into_iter().map(Into::into).collect();
        self
    }

    pub fn cwd(mut self, cwd: impl AsRef<Path>) -> Self {
        self.cwd = Some(cwd.as_ref().to_path_buf());
        self
    }

    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }

    pub fn protocol_mode(mut self, mode: ProtocolMode) -> Self {
        self.protocol_mode = mode;
        self
    }

    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    pub fn thinking(mut self, level: impl Into<String>) -> Self {
        self.thinking = Some(level.into());
        self
    }

    pub fn session(mut self, session: impl Into<String>) -> Self {
        self.session = Some(session.into());
        self
    }

    pub fn no_session(mut self, no_session: bool) -> Self {
        self.no_session = no_session;
        self
    }

    pub fn mcp_config_path(mut self, path: impl AsRef<Path>) -> Self {
        self.mcp_config_path = Some(path.as_ref().to_path_buf());
        self
    }

    pub fn extension_path(mut self, path: impl AsRef<Path>) -> Self {
        self.extension_paths.push(path.as_ref().to_path_buf());
        self
    }

    pub fn system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = Some(prompt.into());
        self
    }

    pub fn extra_arg(mut self, arg: impl Into<String>) -> Self {
        self.extra_args.push(arg.into());
        self
    }

    pub fn request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    pub fn ready_timeout(mut self, timeout: Duration) -> Self {
        self.ready_timeout = timeout;
        self
    }

    pub fn build_launch(&self) -> Launch {
        let mut argv = if self.command.is_empty() {
            self.flavor.default_command()
        } else {
            self.command.clone()
        };
        if !has_mode_flag(&argv) {
            argv.push("--mode".into());
            argv.push(self.protocol_mode.as_arg().into());
        }
        argv.extend(self.extra_args.iter().cloned());
        if let Some(model) = &self.model {
            argv.push("--model".into());
            argv.push(model.clone());
        }
        if let Some(thinking) = &self.thinking {
            argv.push("--thinking".into());
            argv.push(thinking.clone());
        }
        if self.no_session {
            argv.push("--no-session".into());
        } else if let Some(session) = &self.session {
            argv.push("--session".into());
            argv.push(session.clone());
        }
        match self.flavor {
            Flavor::Pi => {
                if let Some(path) = &self.mcp_config_path {
                    argv.push("--mcp-config".into());
                    argv.push(path.to_string_lossy().into_owned());
                }
                for path in &self.extension_paths {
                    argv.push("--extension".into());
                    argv.push(path.to_string_lossy().into_owned());
                }
            }
            Flavor::Omp => {
                if let Some(prompt) = self.system_prompt.as_deref().map(str::trim) {
                    if !prompt.is_empty() {
                        argv.push("--append-system-prompt".into());
                        argv.push(prompt.to_string());
                    }
                }
            }
        }
        Launch {
            argv,
            cwd: self.cwd.clone(),
            env: self.env.clone(),
        }
    }
}

fn has_mode_flag(argv: &[String]) -> bool {
    argv.iter()
        .any(|a| a == "--mode" || a.starts_with("--mode="))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pi_argv_matches_ts_order() {
        let launch = Options::pi()
            .command(["pi"])
            .model("nvidia/openai/gpt-oss-20b")
            .thinking("high")
            .session("/s/file.jsonl")
            .mcp_config_path("/m.json")
            .extension_path("/e1.mjs")
            .extension_path("/e2.mjs")
            .extra_arg("--no-skills")
            .build_launch();
        assert_eq!(
            launch.argv,
            [
                "pi",
                "--mode",
                "rpc",
                "--no-skills",
                "--model",
                "nvidia/openai/gpt-oss-20b",
                "--thinking",
                "high",
                "--session",
                "/s/file.jsonl",
                "--mcp-config",
                "/m.json",
                "--extension",
                "/e1.mjs",
                "--extension",
                "/e2.mjs",
            ]
        );
    }

    #[test]
    fn no_session_beats_session_and_existing_mode_is_kept() {
        let launch = Options::pi()
            .command(["pi", "--mode=json"])
            .session("x")
            .no_session(true)
            .build_launch();
        assert_eq!(launch.argv, ["pi", "--mode=json", "--no-session"]);
    }

    #[test]
    fn omp_appends_trimmed_system_prompt() {
        let launch = Options::omp()
            .command(["omp"])
            .system_prompt("  be brief \n")
            .build_launch();
        assert_eq!(
            launch.argv,
            ["omp", "--mode", "rpc", "--append-system-prompt", "be brief"]
        );
        let blank = Options::omp()
            .command(["omp"])
            .system_prompt("  ")
            .build_launch();
        assert_eq!(blank.argv, ["omp", "--mode", "rpc"]);
    }
}
