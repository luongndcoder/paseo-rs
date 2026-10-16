//! Codex-specific configuration: permission mode presets, version gates, and the translation of a
//! provider-agnostic [`SessionConfig`] into app-server request fields.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::{json, Map, Value};

use crate::agent::{AgentMode, McpServerConfig, SessionConfig, ToolPolicy};
use crate::protocol::params::CollaborationModeEntry;
use crate::transport::ProcessFactory;

pub const PROVIDER: &str = "codex";
pub const DEFAULT_MODE_ID: &str = "auto";
pub const AUTO_REVIEW_MODE_ID: &str = "auto-review";

/// How the app-server is launched. Defaults to `codex` on `PATH`.
#[derive(Debug, Clone, Default)]
pub struct CodexOptions {
    factory: ProcessFactory,
}

impl CodexOptions {
    pub fn new() -> Self {
        Self::default()
    }

    /// Binary to run instead of `codex` (a wrapper, a pinned install, ...).
    pub fn command(mut self, command: impl Into<PathBuf>) -> Self {
        self.factory.command = command.into();
        self
    }

    /// Arguments inserted before `app-server`.
    pub fn prefix_args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.factory.prefix_args = args.into_iter().map(Into::into).collect();
        self
    }

    /// Environment variable added to the inherited environment (e.g. `CODEX_HOME`).
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.factory.env.insert(key.into(), value.into());
        self
    }

    pub fn into_factory(self) -> ProcessFactory {
        self.factory
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ModePreset {
    pub id: &'static str,
    pub label: &'static str,
    pub description: &'static str,
    pub approval_policy: &'static str,
    /// `read-only` | `workspace-write` | `danger-full-access`
    pub sandbox: &'static str,
    /// `user` | `auto_review`
    pub approvals_reviewer: &'static str,
}

pub const MODE_PRESETS: [ModePreset; 4] = [
    ModePreset {
        id: "read-only",
        label: "Read Only",
        description: "Read files; ask before anything else.",
        approval_policy: "on-request",
        sandbox: "read-only",
        approvals_reviewer: "user",
    },
    ModePreset {
        id: "auto",
        label: "Default Permissions",
        description: "Edit files and run commands with Codex's default approval flow.",
        approval_policy: "on-request",
        sandbox: "workspace-write",
        approvals_reviewer: "user",
    },
    ModePreset {
        id: "auto-review",
        label: "Auto-review",
        description: "Same workspace-write permissions as Default, but eligible `on-request` approvals are routed through the auto-reviewer subagent.",
        approval_policy: "on-request",
        sandbox: "workspace-write",
        approvals_reviewer: "auto_review",
    },
    ModePreset {
        id: "full-access",
        label: "Full Access",
        description: "Edit files, run commands, and access the network without additional prompts.",
        approval_policy: "never",
        sandbox: "danger-full-access",
        approvals_reviewer: "user",
    },
];

pub fn mode_preset(id: &str) -> Option<&'static ModePreset> {
    MODE_PRESETS.iter().find(|p| p.id == id)
}

/// Modes offered to users. `read-only` is a valid preset but is not listed, as in the TS adapter.
pub fn catalog_modes(auto_review_enabled: bool) -> Vec<AgentMode> {
    MODE_PRESETS
        .iter()
        .filter(|p| p.id != "read-only")
        .filter(|p| auto_review_enabled || p.id != AUTO_REVIEW_MODE_ID)
        .map(|p| AgentMode {
            id: p.id.to_string(),
            label: p.label.to_string(),
            description: Some(p.description.to_string()),
        })
        .collect()
}

pub fn default_mode_id(auto_review_enabled: bool) -> &'static str {
    if auto_review_enabled {
        AUTO_REVIEW_MODE_ID
    } else {
        DEFAULT_MODE_ID
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct CodexVersion(pub u32, pub u32, pub u32);

/// `--enable goals` is rejected by older binaries.
pub const GOALS_MIN_VERSION: CodexVersion = CodexVersion(0, 128, 0);
pub const AUTO_REVIEW_MIN_VERSION: CodexVersion = CodexVersion(0, 115, 0);
/// Codex removed `thread/rollback` in 0.156.0; rewinding there must fork before the target turn.
pub const THREAD_ROLLBACK_REMOVED_VERSION: CodexVersion = CodexVersion(0, 156, 0);

impl CodexVersion {
    /// First `x.y.z` found in the text (`codex-cli 0.160.0`, `codex_cli_rs/0.160.0 (...)`).
    pub fn parse(text: &str) -> Option<CodexVersion> {
        let bytes = text.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i].is_ascii_digit() {
                let start = i;
                while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
                    i += 1;
                }
                let parts: Vec<&str> = text[start..i].split('.').collect();
                if parts.len() >= 3 {
                    let n = |s: &str| s.parse::<u32>().ok();
                    if let (Some(a), Some(b), Some(c)) = (n(parts[0]), n(parts[1]), n(parts[2])) {
                        return Some(CodexVersion(a, b, c));
                    }
                }
            } else {
                i += 1;
            }
        }
        None
    }
}

/// Unparseable output counts as "too old".
pub fn version_at_least(text: &str, min: CodexVersion) -> bool {
    CodexVersion::parse(text).is_some_and(|v| v >= min)
}

pub fn normalize_thinking_option(id: Option<&str>) -> Option<String> {
    let id = id?.trim();
    if id.is_empty() || id == "default" {
        None
    } else {
        Some(id.to_string())
    }
}

pub fn normalize_model_label(display_name: &str) -> String {
    // `\bgpt\b` case-insensitively -> `GPT`
    let mut out = String::with_capacity(display_name.len());
    let chars: Vec<char> = display_name.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let is_word = |c: char| c.is_alphanumeric() || c == '_';
        if i + 3 <= chars.len()
            && chars[i..i + 3]
                .iter()
                .collect::<String>()
                .eq_ignore_ascii_case("gpt")
            && (i == 0 || !is_word(chars[i - 1]))
            && (i + 3 == chars.len() || !is_word(chars[i + 3]))
        {
            out.push_str("GPT");
            i += 3;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

pub fn compose_system_prompt<'a>(
    parts: impl IntoIterator<Item = Option<&'a str>>,
) -> Option<String> {
    let joined = parts
        .into_iter()
        .flatten()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    if joined.is_empty() {
        None
    } else {
        Some(joined)
    }
}

/// `workspace-write` knobs, from `provider_options.sandbox_workspace_write` or the resolved config.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WorkspaceWrite {
    pub writable_roots: Option<Vec<String>>,
    pub network_access: Option<bool>,
    pub exclude_slash_tmp: Option<bool>,
    pub exclude_tmpdir_env_var: Option<bool>,
}

impl WorkspaceWrite {
    /// Reads both snake_case (config.toml) and camelCase (sandbox policy) spellings.
    pub fn from_value(v: &Value) -> Option<WorkspaceWrite> {
        let o = v.as_object()?;
        let pick = |snake: &str, camel: &str| o.get(snake).or_else(|| o.get(camel));
        Some(WorkspaceWrite {
            writable_roots: pick("writable_roots", "writableRoots")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect()
                }),
            network_access: pick("network_access", "networkAccess").and_then(Value::as_bool),
            exclude_slash_tmp: pick("exclude_slash_tmp", "excludeSlashTmp")
                .and_then(Value::as_bool),
            exclude_tmpdir_env_var: pick("exclude_tmpdir_env_var", "excludeTmpdirEnvVar")
                .and_then(Value::as_bool),
        })
    }

    /// `self` over `base`, field by field.
    pub fn over(&self, base: &WorkspaceWrite) -> WorkspaceWrite {
        WorkspaceWrite {
            writable_roots: self
                .writable_roots
                .clone()
                .or_else(|| base.writable_roots.clone()),
            network_access: self.network_access.or(base.network_access),
            exclude_slash_tmp: self.exclude_slash_tmp.or(base.exclude_slash_tmp),
            exclude_tmpdir_env_var: self.exclude_tmpdir_env_var.or(base.exclude_tmpdir_env_var),
        }
    }
}

/// `read-only` | `workspace-write` | `danger-full-access` -> the `sandboxPolicy.type` tag.
pub fn sandbox_policy_type(sandbox: &str) -> &'static str {
    match sandbox {
        "workspace-write" => "workspaceWrite",
        "read-only" => "readOnly",
        _ => "dangerFullAccess",
    }
}

pub fn sandbox_policy(sandbox: &str, ww: &WorkspaceWrite) -> Value {
    match sandbox {
        "read-only" => json!({ "type": "readOnly" }),
        "workspace-write" => json!({
            "type": "workspaceWrite",
            "networkAccess": ww.network_access.unwrap_or(false),
            "writableRoots": ww.writable_roots.clone().unwrap_or_default(),
            "excludeSlashTmp": ww.exclude_slash_tmp.unwrap_or(false),
            "excludeTmpdirEnvVar": ww.exclude_tmpdir_env_var.unwrap_or(false),
        }),
        "danger-full-access" => json!({ "type": "dangerFullAccess" }),
        _ => json!({ "type": "workspaceWrite", "networkAccess": false, "writableRoots": [] }),
    }
}

pub fn mcp_config_to_codex(config: &McpServerConfig) -> Value {
    let mut o = Map::new();
    match config {
        McpServerConfig::Stdio { command, args, env } => {
            o.insert("command".into(), json!(command));
            if let Some(a) = args {
                o.insert("args".into(), json!(a));
            }
            if let Some(e) = env {
                o.insert("env".into(), json!(e));
            }
        }
        McpServerConfig::Http { url, headers } | McpServerConfig::Sse { url, headers } => {
            o.insert("url".into(), json!(url));
            if let Some(h) = headers {
                o.insert("http_headers".into(), json!(h));
            }
        }
    }
    Value::Object(o)
}

/// Pre-approved MCP tools skip the approval prompt; everything else on that server still asks.
pub fn apply_tool_policy(
    mut config: Map<String, Value>,
    policy: Option<&ToolPolicy>,
) -> Map<String, Value> {
    let Some(policy) = policy else { return config };
    let mut mcp_servers = match config.remove("mcp_servers") {
        Some(Value::Object(o)) => o,
        _ => Map::new(),
    };
    let mut grants: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for g in &policy.preapproved {
        grants.entry(&g.server).or_default().push(&g.tool);
    }
    for (server, tools) in grants {
        let mut server_config = match mcp_servers.remove(server) {
            Some(Value::Object(o)) => o,
            _ => Map::new(),
        };
        let approvals: Map<String, Value> = tools
            .iter()
            .map(|t| ((*t).to_string(), json!({ "approval_mode": "approve" })))
            .collect();
        server_config.insert("enabled_tools".into(), json!(tools));
        server_config.insert("default_tools_approval_mode".into(), json!("prompt"));
        server_config.insert("tools".into(), Value::Object(approvals));
        mcp_servers.insert(server.to_string(), Value::Object(server_config));
    }
    config.insert("mcp_servers".into(), Value::Object(mcp_servers));
    config
}

/// The `config` object sent with `thread/start`, `thread/resume` and `turn/start`. `None` when
/// there is nothing to override.
pub fn build_inner_config(config: &SessionConfig) -> Option<Map<String, Value>> {
    let mut inner: Map<String, Value> = config.provider_options.clone().unwrap_or_default();
    if !config.mcp_servers.is_empty() {
        let servers: Map<String, Value> = config
            .mcp_servers
            .iter()
            .map(|(name, c)| (name.clone(), mcp_config_to_codex(c)))
            .collect();
        inner.insert("mcp_servers".into(), Value::Object(servers));
    }
    let configured = apply_tool_policy(inner, config.tool_policy.as_ref());
    if configured.is_empty() {
        None
    } else {
        Some(configured)
    }
}

/// Which collaboration-mode entry applies. `plan` -> a "plan"/"read" entry; otherwise the
/// "auto"/"code" entry, falling back to the first non-plan entry.
pub fn find_collaboration_mode(
    entries: &[CollaborationModeEntry],
    plan: bool,
) -> Option<&CollaborationModeEntry> {
    if entries.is_empty() {
        return None;
    }
    let lower = |e: &CollaborationModeEntry| e.name.to_lowercase();
    if plan {
        return entries.iter().find(|e| {
            let n = lower(e);
            n.contains("plan") || n.contains("read")
        });
    }
    entries
        .iter()
        .find(|e| {
            let n = lower(e);
            n.contains("auto") || n.contains("code")
        })
        .or_else(|| {
            entries.iter().find(|e| {
                let n = lower(e);
                !n.contains("plan") && !n.contains("read")
            })
        })
        .or_else(|| entries.first())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::ToolGrant;

    #[test]
    fn parses_versions() {
        assert_eq!(
            CodexVersion::parse("codex-cli 0.160.0"),
            Some(CodexVersion(0, 160, 0))
        );
        assert_eq!(
            CodexVersion::parse("codex_cli_rs/0.160.0 (Mac OS 26.6.2; arm64) iTerm.app/3.7.5"),
            Some(CodexVersion(0, 160, 0))
        );
        assert!(version_at_least(
            "codex-cli 0.115.0",
            AUTO_REVIEW_MIN_VERSION
        ));
        assert!(!version_at_least(
            "codex-cli 0.114.9",
            AUTO_REVIEW_MIN_VERSION
        ));
        assert!(!version_at_least("garbage", AUTO_REVIEW_MIN_VERSION));
    }

    #[test]
    fn labels() {
        assert_eq!(normalize_model_label("gpt-6.1-Sol"), "GPT-6.1-Sol");
        assert_eq!(normalize_model_label("Pgpt gpt5 GPT"), "Pgpt gpt5 GPT");
    }

    #[test]
    fn sandbox_policy_matches_fixture_shape() {
        assert_eq!(
            sandbox_policy("read-only", &WorkspaceWrite::default()),
            json!({"type":"readOnly"})
        );
        assert_eq!(
            sandbox_policy("workspace-write", &WorkspaceWrite::default()),
            json!({"type":"workspaceWrite","networkAccess":false,"writableRoots":[],"excludeSlashTmp":false,"excludeTmpdirEnvVar":false})
        );
    }

    #[test]
    fn tool_policy_prompts_unlisted_tools() {
        let policy = ToolPolicy {
            preapproved: vec![ToolGrant {
                server: "s".into(),
                tool: "t".into(),
            }],
        };
        let cfg = apply_tool_policy(Map::new(), Some(&policy));
        assert_eq!(
            cfg["mcp_servers"]["s"],
            json!({"enabled_tools":["t"],"default_tools_approval_mode":"prompt","tools":{"t":{"approval_mode":"approve"}}})
        );
    }

    #[test]
    fn collaboration_mode_lookup() {
        let e = |name: &str, mode: &str| CollaborationModeEntry {
            name: name.into(),
            mode: Some(mode.into()),
            model: None,
            reasoning_effort: None,
            developer_instructions: None,
        };
        let entries = vec![e("Plan", "plan"), e("Default", "default")];
        assert_eq!(
            find_collaboration_mode(&entries, false)
                .unwrap()
                .mode
                .as_deref(),
            Some("default")
        );
        assert_eq!(
            find_collaboration_mode(&entries, true)
                .unwrap()
                .mode
                .as_deref(),
            Some("plan")
        );
    }

    #[test]
    fn modes_hide_read_only_and_gate_auto_review() {
        let ids = |m: Vec<AgentMode>| m.into_iter().map(|m| m.id).collect::<Vec<_>>();
        assert_eq!(ids(catalog_modes(false)), vec!["auto", "full-access"]);
        assert_eq!(
            ids(catalog_modes(true)),
            vec!["auto", "auto-review", "full-access"]
        );
    }
}
