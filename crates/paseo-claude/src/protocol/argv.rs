//! CLI argument construction, mirroring `ProcessTransport.initialize()` of
//! `@anthropic-ai/claude-agent-sdk` 0.3.246 (sdk.mjs). Order matters only for readability;
//! the CLI parses flags in any order, but keeping the SDK's order makes golden diffs trivial.

use serde_json::Value;
use std::collections::BTreeMap;

use super::mcp::{McpConfigFile, McpServerConfig};

#[derive(Debug, Clone, PartialEq)]
pub enum ThinkingConfig {
    /// `--thinking adaptive` (+ `--thinking-display`).
    Adaptive { display: Option<String> },
    /// `budget_tokens: None` → `--thinking adaptive`; `Some(n)` → `--max-thinking-tokens n`.
    Enabled {
        budget_tokens: Option<u64>,
        display: Option<String>,
    },
    /// `--thinking disabled`.
    Disabled,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ToolsOption {
    /// `--tools default`.
    Default,
    /// `--tools a,b` (empty list → `--tools ""`).
    List(Vec<String>),
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ArgvOptions {
    pub thinking: Option<ThinkingConfig>,
    pub effort: Option<String>,
    pub max_turns: Option<u64>,
    pub max_budget_usd: Option<f64>,
    pub model: Option<String>,
    pub agent: Option<String>,
    pub betas: Vec<String>,
    /// Output JSON schema (`outputFormat.type == "json_schema"`).
    pub json_schema: Option<Value>,
    pub debug: bool,
    pub debug_file: Option<String>,
    /// `canUseTool` callback present → `--permission-prompt-tool stdio`.
    pub can_use_tool: bool,
    /// Only valid when `can_use_tool` is false.
    pub permission_prompt_tool_name: Option<String>,
    pub continue_conversation: bool,
    pub resume: Option<String>,
    pub allowed_tools: Vec<String>,
    pub disallowed_tools: Vec<String>,
    pub tools: Option<ToolsOption>,
    /// Out-of-process MCP servers (in-process `sdk` servers with an instance are NOT passed on
    /// the command line; they go in `initialize.sdkMcpServers`).
    pub mcp_servers: BTreeMap<String, McpServerConfig>,
    /// `Some(vec![])` still emits `--setting-sources=` (load nothing).
    pub setting_sources: Option<Vec<String>>,
    pub strict_mcp_config: bool,
    pub permission_mode: Option<String>,
    pub allow_dangerously_skip_permissions: bool,
    pub fallback_model: Option<String>,
    pub include_hook_events: bool,
    pub include_partial_messages: bool,
    pub additional_directories: Vec<String>,
    /// `(path, skip_mcp_discovery)`.
    pub plugin_dirs: Vec<(String, bool)>,
    pub fork_session: bool,
    pub resume_session_at: Option<String>,
    pub resume_drops_turn: Option<bool>,
    pub session_id: Option<String>,
    /// `Some(false)` → `--no-session-persistence`.
    pub persist_session: Option<bool>,
    /// Settings JSON string or file path (`--settings`).
    pub settings: Option<String>,
    /// Generic `--key value`; `None` value → bare `--key`.
    pub extra_args: BTreeMap<String, Option<String>>,
}

fn push_flag(args: &mut Vec<String>, key: &str, value: &str) {
    // Values that start with '-' must use the `=` form or the CLI reads them as flags.
    if value.len() > 1 && value.starts_with('-') {
        args.push(format!("--{key}={value}"));
    } else {
        args.push(format!("--{key}"));
        args.push(value.to_string());
    }
}

fn thinking_args(args: &mut Vec<String>, t: &ThinkingConfig) {
    let display = match t {
        ThinkingConfig::Adaptive { display } => {
            args.extend(["--thinking".into(), "adaptive".into()]);
            display
        }
        ThinkingConfig::Enabled {
            budget_tokens,
            display,
        } => {
            match budget_tokens {
                None => args.extend(["--thinking".into(), "adaptive".into()]),
                Some(n) => args.extend(["--max-thinking-tokens".into(), n.to_string()]),
            }
            display
        }
        ThinkingConfig::Disabled => {
            args.extend(["--thinking".into(), "disabled".into()]);
            return;
        }
    };
    if let Some(d) = display {
        args.extend(["--thinking-display".into(), d.clone()]);
    }
}

/// Arguments to pass to the `claude` executable (not including the executable itself).
pub fn build_argv(o: &ArgvOptions) -> Vec<String> {
    let mut a: Vec<String> = [
        "--output-format",
        "stream-json",
        "--verbose",
        "--input-format",
        "stream-json",
    ]
    .map(String::from)
    .into();
    if let Some(t) = &o.thinking {
        thinking_args(&mut a, t);
    }
    if let Some(e) = &o.effort {
        a.extend(["--effort".into(), e.clone()]);
    }
    if let Some(n) = o.max_turns.filter(|n| *n > 0) {
        a.extend(["--max-turns".into(), n.to_string()]);
    }
    if let Some(b) = o.max_budget_usd {
        a.extend(["--max-budget-usd".into(), b.to_string()]);
    }
    if let Some(m) = &o.model {
        a.extend(["--model".into(), m.clone()]);
    }
    if let Some(m) = &o.agent {
        a.extend(["--agent".into(), m.clone()]);
    }
    if !o.betas.is_empty() {
        a.extend(["--betas".into(), o.betas.join(",")]);
    }
    if let Some(s) = &o.json_schema {
        a.extend(["--json-schema".into(), s.to_string()]);
    }
    if let Some(f) = &o.debug_file {
        a.extend(["--debug-file".into(), f.clone()]);
    } else if o.debug {
        a.push("--debug".into());
    }
    if o.can_use_tool {
        a.extend(["--permission-prompt-tool".into(), "stdio".into()]);
    } else if let Some(name) = &o.permission_prompt_tool_name {
        a.extend(["--permission-prompt-tool".into(), name.clone()]);
    }
    if o.continue_conversation {
        a.push("--continue".into());
    }
    if let Some(r) = &o.resume {
        a.push(format!("--resume={r}"));
    }
    if !o.allowed_tools.is_empty() {
        a.extend(["--allowedTools".into(), o.allowed_tools.join(",")]);
    }
    if !o.disallowed_tools.is_empty() {
        a.extend(["--disallowedTools".into(), o.disallowed_tools.join(",")]);
    }
    match &o.tools {
        Some(ToolsOption::Default) => a.extend(["--tools".into(), "default".into()]),
        Some(ToolsOption::List(l)) => a.extend(["--tools".into(), l.join(",")]),
        None => {}
    }
    if !o.mcp_servers.is_empty() {
        let file = McpConfigFile {
            mcp_servers: o.mcp_servers.clone(),
            extra: Default::default(),
        };
        a.extend([
            "--mcp-config".into(),
            serde_json::to_string(&file).expect("mcp config serializes"),
        ]);
    }
    if let Some(s) = &o.setting_sources {
        a.push(format!("--setting-sources={}", s.join(",")));
    }
    if o.strict_mcp_config {
        a.push("--strict-mcp-config".into());
    }
    if let Some(m) = &o.permission_mode {
        a.extend(["--permission-mode".into(), m.clone()]);
    }
    if o.allow_dangerously_skip_permissions {
        a.push("--allow-dangerously-skip-permissions".into());
    }
    if let Some(m) = &o.fallback_model {
        a.extend(["--fallback-model".into(), m.clone()]);
    }
    if o.include_hook_events {
        a.push("--include-hook-events".into());
    }
    if o.include_partial_messages {
        a.push("--include-partial-messages".into());
    }
    for d in &o.additional_directories {
        a.extend(["--add-dir".into(), d.clone()]);
    }
    for (path, skip_mcp) in &o.plugin_dirs {
        let flag = if *skip_mcp {
            "--plugin-dir-no-mcp"
        } else {
            "--plugin-dir"
        };
        a.extend([flag.into(), path.clone()]);
    }
    if o.fork_session {
        a.push("--fork-session".into());
    }
    if let Some(v) = &o.resume_session_at {
        a.push(format!("--resume-session-at={v}"));
    }
    if let Some(v) = o.resume_drops_turn {
        a.push(format!("--resume-drops-turn={v}"));
    }
    if let Some(v) = &o.session_id {
        a.push(format!("--session-id={v}"));
    }
    if o.persist_session == Some(false) {
        a.push("--no-session-persistence".into());
    }
    let mut extra = o.extra_args.clone();
    if let Some(s) = &o.settings {
        extra.insert("settings".into(), Some(s.clone()));
    }
    for (k, v) in &extra {
        match v {
            None => a.push(format!("--{k}")),
            Some(v) => push_flag(&mut a, k, v),
        }
    }
    a
}
