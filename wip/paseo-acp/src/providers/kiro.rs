//! Kiro CLI: publishes slash commands and skills through the `_kiro.dev/commands/available`
//! extension notification shortly after `session/new`.

use std::time::Duration;

use serde_json::Value;

use super::generic::{generic_client, GenericOptions};
use crate::agent::{SlashCommand, SlashCommandKind};
use crate::client::AcpClient;
use crate::profile::ProviderHooks;

const INITIAL_COMMANDS_WAIT_TIMEOUT: Duration = Duration::from_secs(10);
pub const COMMANDS_AVAILABLE_METHOD: &str = "_kiro.dev/commands/available";

pub struct KiroHooks;

impl ProviderHooks for KiroHooks {
    fn extension_commands(&self, method: &str, params: &Value) -> Option<Vec<SlashCommand>> {
        (method == COMMANDS_AVAILABLE_METHOD).then(|| map_available_commands(params))
    }
}

/// Built-in slash commands arrive under `commands` (names with a leading "/"), skills and
/// prompts under `prompts` (tagged with a `skill:` serverName). Paseo stores names without
/// the slash; the composer prepends it.
pub fn map_available_commands(params: &Value) -> Vec<SlashCommand> {
    let mut out: Vec<SlashCommand> = Vec::new();
    for key in ["commands", "prompts"] {
        let Some(entries) = params.get(key).and_then(Value::as_array) else {
            continue;
        };
        for entry in entries {
            let Some(obj) = entry.as_object() else {
                continue;
            };
            let raw = obj
                .get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or("");
            let name = raw.trim_start_matches('/');
            if name.is_empty() || out.iter().any(|c| c.name == name) {
                continue;
            }
            let description = obj
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let hint = obj
                .get("meta")
                .and_then(Value::as_object)
                .and_then(|m| m.get("hint"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let server = obj.get("serverName").and_then(Value::as_str).unwrap_or("");
            out.push(SlashCommand {
                name: name.to_string(),
                description,
                argument_hint: hint,
                kind: if server.starts_with("skill:") {
                    SlashCommandKind::Skill
                } else {
                    SlashCommandKind::Command
                },
            });
        }
    }
    out
}

pub fn options(command: Vec<String>) -> GenericOptions {
    let mut o = GenericOptions::new(command);
    o.wait_for_initial_commands = true;
    o.initial_commands_wait_timeout = INITIAL_COMMANDS_WAIT_TIMEOUT;
    o.hooks = Some(std::sync::Arc::new(KiroHooks));
    o
}

pub fn client(command: Vec<String>) -> AcpClient {
    generic_client(options(command))
}
