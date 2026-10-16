//! Pure conversions between the provider-agnostic types and OpenCode wire types.

use super::types::*;
use crate::protocol::*;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};

pub const ACTION_ALLOW_ONCE: &str = "allow_once";
pub const ACTION_ALLOW_ALWAYS: &str = "allow_always";
pub const DEFAULT_VARIANT_ID: &str = "default";
pub const BUILD_MODE_ID: &str = "build";

/// `None` means "no explicit mode": the `agent` field is then omitted and OpenCode uses its own default.
pub fn normalize_mode_id(mode: Option<&str>) -> Option<String> {
    let trimmed = mode.map(str::trim).unwrap_or("");
    (!trimmed.is_empty() && trimmed != "default").then(|| trimmed.to_owned())
}

pub fn normalize_variant_id(variant: Option<&str>) -> Option<String> {
    let trimmed = variant.map(str::trim).unwrap_or("");
    (!trimmed.is_empty() && trimmed != DEFAULT_VARIANT_ID).then(|| trimmed.to_owned())
}

fn attachment_extension(mime: &str) -> &'static str {
    match mime {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "image/svg+xml" => "svg",
        _ => "bin",
    }
}

/// Returns `(mime, data URL)`; accepts base64 or an existing `data:<mime>;base64,<data>` URL.
fn to_data_url(mime: &str, data: &str) -> (String, String) {
    if let Some(rest) = data.strip_prefix("data:") {
        if let Some((m, payload)) = rest.split_once(";base64,") {
            if !m.is_empty() && !payload.is_empty() && !m.contains(',') {
                return (m.to_owned(), data.to_owned());
            }
        }
    }
    (mime.to_owned(), format!("data:{mime};base64,{data}"))
}

pub fn build_prompt_parts(prompt: &AgentPromptInput) -> Vec<PromptPartInput> {
    let mut ordinal = 0;
    prompt
        .iter()
        .map(|part| match part {
            PromptPart::Text(text) => PromptPartInput::Text { text: text.clone() },
            PromptPart::Image { mime_type, data } => {
                ordinal += 1;
                let (mime, url) = to_data_url(mime_type, data);
                PromptPartInput::File {
                    filename: format!("attachment-{ordinal}.{}", attachment_extension(&mime)),
                    mime,
                    url,
                }
            }
        })
        .collect()
}

/// Plain text of a prompt (used to detect slash commands).
pub fn prompt_text(prompt: &AgentPromptInput) -> String {
    prompt
        .iter()
        .filter_map(|p| match p {
            PromptPart::Text(t) => Some(t.as_str()),
            PromptPart::Image { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `system` field: the session prompt followed by the daemon-level append, blank-line separated.
pub fn compose_system_prompt(config: &AgentSessionConfig) -> Option<String> {
    let parts: Vec<&str> = [
        config.system_prompt.as_deref(),
        config.daemon_append_system_prompt.as_deref(),
    ]
    .into_iter()
    .flatten()
    .map(str::trim)
    .filter(|s| !s.is_empty())
    .collect();
    (!parts.is_empty()).then(|| parts.join("\n\n"))
}

/// Rules stored on the session: pre-approved tool grants first, then the configured rules.
pub fn session_permission_rules(config: &AgentSessionConfig) -> Option<Vec<PermissionRule>> {
    let mut rules: Vec<PermissionRule> = config
        .preapproved_tools
        .iter()
        .map(|g| PermissionRule {
            permission: format!("{}_{}", g.server, g.tool),
            pattern: "*".into(),
            action: Some(PermissionAction::Allow),
            ..Default::default()
        })
        .collect();
    rules.extend(config.permission_rules.iter().cloned());
    (!rules.is_empty()).then_some(rules)
}

static LAST_TS: AtomicU64 = AtomicU64::new(0);
static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Ascending message id (`msg_` + 12 hex of `now_ms * 0x1000 + counter` + 14 random base62), so
/// prompts sort in submission order on the server.
pub fn create_message_id() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    message_id_from(now)
}

pub(crate) fn message_id_from(now: u64) -> String {
    if LAST_TS.swap(now, Ordering::SeqCst) != now {
        COUNTER.store(0, Ordering::SeqCst);
    }
    let counter = COUNTER.fetch_add(1, Ordering::SeqCst) + 1;
    let ascending = ((now as u128) * 0x1000 + counter as u128) & 0xffff_ffff_ffff;
    const ALPHABET: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    let mut random = [0u8; 14];
    getrandom::getrandom(&mut random).expect("OS randomness is available");
    let suffix: String = random
        .iter()
        .map(|b| ALPHABET[(*b as usize) % 62] as char)
        .collect();
    format!("msg_{ascending:012x}{suffix}")
}

pub fn permission_actions() -> Vec<AgentPermissionAction> {
    vec![
        AgentPermissionAction {
            id: "deny".into(),
            label: "Deny".into(),
            behavior: PermissionBehavior::Deny,
        },
        AgentPermissionAction {
            id: ACTION_ALLOW_ALWAYS.into(),
            label: "Allow always".into(),
            behavior: PermissionBehavior::Allow,
        },
        AgentPermissionAction {
            id: ACTION_ALLOW_ONCE.into(),
            label: "Allow once".into(),
            behavior: PermissionBehavior::Allow,
        },
    ]
}

pub fn map_permission_request(req: &PermissionRequest) -> AgentPermissionRequest {
    AgentPermissionRequest {
        id: req.id.clone(),
        provider: "opencode".into(),
        name: req.permission.clone(),
        kind: PermissionKind::Tool,
        input: json!({
            "permission": req.permission,
            "patterns": req.patterns,
            "metadata": req.metadata,
            "always": req.always,
            "tool": req.tool,
        }),
        actions: permission_actions(),
        raw: serde_json::to_value(req).unwrap_or(Value::Null),
    }
}

pub fn map_question_request(req: &QuestionRequest) -> AgentPermissionRequest {
    AgentPermissionRequest {
        id: req.id.clone(),
        provider: "opencode".into(),
        name: "question".into(),
        kind: PermissionKind::Question,
        input: json!({ "questions": req.questions }),
        actions: vec![
            AgentPermissionAction {
                id: "deny".into(),
                label: "Dismiss".into(),
                behavior: PermissionBehavior::Deny,
            },
            AgentPermissionAction {
                id: "answer".into(),
                label: "Answer".into(),
                behavior: PermissionBehavior::Allow,
            },
        ],
        raw: serde_json::to_value(req).unwrap_or(Value::Null),
    }
}

pub fn resolve_permission_reply(response: &AgentPermissionResponse) -> PermissionReply {
    if response.behavior == PermissionBehavior::Deny {
        PermissionReply::Reject
    } else if response.selected_action_id.as_deref() == Some(ACTION_ALLOW_ALWAYS) {
        PermissionReply::Always
    } else {
        PermissionReply::Once
    }
}

/// Answers for a question reply: one list per question, looked up by the question's `header`
/// in `updated_input.answers`, comma-separated into several choices.
pub fn question_answers(
    request: &AgentPermissionRequest,
    response: &AgentPermissionResponse,
) -> Vec<Vec<String>> {
    let answers = response
        .updated_input
        .as_ref()
        .and_then(|v| v.get("answers"))
        .and_then(Value::as_object);
    let questions = request
        .input
        .get("questions")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    questions
        .iter()
        .map(|q| {
            let header = q.get("header").and_then(Value::as_str).unwrap_or("");
            let raw = answers
                .and_then(|a| a.get(header))
                .and_then(Value::as_str)
                .unwrap_or("");
            raw.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .collect()
}

fn capitalize(name: &str) -> String {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

pub fn is_selectable_agent(agent: &AgentInfo) -> bool {
    matches!(agent.mode.as_deref(), Some("primary") | Some("all")) && agent.hidden != Some(true)
}

fn is_hex_color(s: &str) -> bool {
    s.len() == 7 && s.starts_with('#') && s[1..].chars().all(|c| c.is_ascii_hexdigit())
}

fn default_mode_description(id: &str) -> Option<&'static str> {
    match id {
        "build" => Some("Allows edits and tool execution for implementation work"),
        "plan" => Some("Read-only planning mode that avoids file edits"),
        _ => None,
    }
}

/// Selectable agents as modes, `build` and `plan` first, the rest alphabetical. Discovery results
/// are trusted exactly: no default modes are injected.
pub fn agents_to_modes(agents: &[AgentInfo]) -> Vec<AgentMode> {
    let mut modes: Vec<AgentMode> = agents
        .iter()
        .filter(|a| is_selectable_agent(a))
        .map(|a| AgentMode {
            id: a.name.clone(),
            label: capitalize(&a.name),
            description: a
                .description
                .as_deref()
                .map(str::trim)
                .filter(|d| !d.is_empty())
                .map(str::to_owned)
                .or_else(|| default_mode_description(&a.name).map(str::to_owned)),
            color: a.color.clone().filter(|c| is_hex_color(c)),
        })
        .collect();
    let rank = |id: &str| match id {
        "build" => 0,
        "plan" => 1,
        _ => 2,
    };
    modes.sort_by(|a, b| {
        rank(&a.id)
            .cmp(&rank(&b.id))
            .then_with(|| a.label.to_lowercase().cmp(&b.label.to_lowercase()))
    });
    modes
}

pub fn context_window(model: &ModelInfo) -> Option<u64> {
    model
        .limit
        .as_ref()?
        .context
        .as_ref()?
        .as_u64()
        .filter(|n| *n > 0)
}

/// Accessible providers are connected ones or `source == "api"`.
pub fn providers_to_models(list: &ProviderList) -> Vec<AgentModelDefinition> {
    let mut out = Vec::new();
    for provider in &list.all {
        let accessible =
            list.connected.contains(&provider.id) || provider.source.as_deref() == Some("api");
        if !accessible {
            continue;
        }
        let provider_name = provider.name.clone().unwrap_or_else(|| provider.id.clone());
        for (model_id, raw) in &provider.models {
            let Ok(model) = serde_json::from_value::<ModelInfo>(raw.clone()) else {
                continue;
            };
            let variants: Vec<String> = model
                .variants
                .as_ref()
                .map(|v| v.keys().cloned().collect())
                .unwrap_or_default();
            let thinking_options: Vec<ThinkingOption> = if variants.is_empty() {
                Vec::new()
            } else {
                std::iter::once(ThinkingOption {
                    id: DEFAULT_VARIANT_ID.into(),
                    label: "Default".into(),
                    is_default: true,
                })
                .chain(
                    variants
                        .iter()
                        .filter(|v| v.as_str() != DEFAULT_VARIANT_ID)
                        .map(|v| ThinkingOption {
                            id: v.clone(),
                            label: v.clone(),
                            is_default: false,
                        }),
                )
                .collect()
            };
            out.push(AgentModelDefinition {
                id: format!("{}/{}", provider.id, model_id),
                label: model.name.clone().unwrap_or_else(|| model_id.clone()),
                description: Some(
                    format!(
                        "{provider_name} - {}",
                        model.family.as_deref().unwrap_or("")
                    )
                    .trim()
                    .to_owned(),
                ),
                provider_id: provider.id.clone(),
                model_id: model_id.clone(),
                default_thinking_option_id: thinking_options.first().map(|t| t.id.clone()),
                thinking_options,
                context_window_max_tokens: context_window(&model),
                raw: raw.clone(),
            });
        }
    }
    out
}

pub fn commands_to_slash(commands: &[CommandInfo]) -> Vec<AgentSlashCommand> {
    // `compact`/`summarize` are handled by the adapter itself and always offered.
    let mut by_name: Vec<AgentSlashCommand> = [
        ("compact", "Summarize the session to free context"),
        ("summarize", "Summarize the session to free context"),
    ]
    .into_iter()
    .map(|(name, description)| AgentSlashCommand {
        name: name.into(),
        description: description.into(),
        argument_hint: String::new(),
        kind: "command".into(),
    })
    .collect();
    for c in commands {
        let entry = AgentSlashCommand {
            name: c.name.clone(),
            description: c.description.clone().unwrap_or_default(),
            argument_hint: c.hints.as_ref().map(|h| h.join(" ")).unwrap_or_default(),
            kind: if c.source.as_deref() == Some("skill") {
                "skill".into()
            } else {
                "command".into()
            },
        };
        match by_name.iter_mut().find(|e| e.name == c.name) {
            Some(slot) => *slot = entry,
            None => by_name.push(entry),
        }
    }
    by_name
}

/// `(name, args)` for `/name args`.
pub fn parse_slash_command(text: &str) -> Option<(String, String)> {
    let trimmed = text.trim();
    let rest = trimmed.strip_prefix('/')?;
    if rest.is_empty() {
        return None;
    }
    let (name, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    (!name.is_empty()).then(|| (name.to_owned(), args.trim().to_owned()))
}

fn num_u64(n: &Option<serde_json::Number>) -> u64 {
    n.as_ref()
        .and_then(|n| n.as_u64().or_else(|| n.as_f64().map(|f| f as u64)))
        .unwrap_or(0)
}

/// Usage contributed by one assistant message.
pub fn message_usage(info: &MessageInfo) -> AgentUsage {
    let mut usage = AgentUsage::default();
    if let Some(t) = &info.tokens {
        let (input, output, reasoning) =
            (num_u64(&t.input), num_u64(&t.output), num_u64(&t.reasoning));
        let (read, write) = t
            .cache
            .as_ref()
            .map(|c| (num_u64(&c.read), num_u64(&c.write)))
            .unwrap_or((0, 0));
        if input > 0 {
            usage.input_tokens = Some(input);
        }
        if read > 0 {
            usage.cached_input_tokens = Some(read);
        }
        if output > 0 {
            usage.output_tokens = Some(output);
        }
        let total = input + output + reasoning + read + write;
        if total > 0 {
            usage.context_window_used_tokens = Some(total);
        }
    }
    if let Some(cost) = info
        .cost
        .as_ref()
        .and_then(|c| c.as_f64())
        .filter(|c| *c > 0.0)
    {
        usage.total_cost_usd = Some(cost);
    }
    usage
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_ids_ascend_and_have_expected_shape() {
        let a = message_id_from(1_792_172_446_360);
        let b = message_id_from(1_792_172_446_360);
        let c = message_id_from(1_792_172_446_361);
        assert!(a.starts_with("msg_") && a.len() == 4 + 12 + 14);
        assert!(a < b && b < c, "{a} {b} {c}");
    }

    #[test]
    fn model_strings() {
        assert_eq!(
            ModelRef::parse("opencode/glm-5.3-flash-free").model_id,
            "glm-5.3-flash-free"
        );
        let m = ModelRef::parse("openrouter/anthropic/claude");
        assert_eq!(
            (m.provider_id.as_str(), m.model_id.as_str()),
            ("openrouter", "anthropic/claude")
        );
        assert_eq!(ModelRef::parse("big-pickle").provider_id, "opencode");
    }

    #[test]
    fn image_parts_become_file_parts() {
        let parts = build_prompt_parts(&vec![
            PromptPart::Text("hi".into()),
            PromptPart::Image {
                mime_type: "image/png".into(),
                data: "AAAA".into(),
            },
            PromptPart::Image {
                mime_type: "image/jpeg".into(),
                data: "data:image/webp;base64,BBBB".into(),
            },
        ]);
        assert_eq!(
            parts[1],
            PromptPartInput::File {
                mime: "image/png".into(),
                filename: "attachment-1.png".into(),
                url: "data:image/png;base64,AAAA".into()
            }
        );
        assert_eq!(
            parts[2],
            PromptPartInput::File {
                mime: "image/webp".into(),
                filename: "attachment-2.webp".into(),
                url: "data:image/webp;base64,BBBB".into()
            }
        );
    }

    #[test]
    fn slash_commands() {
        assert_eq!(
            parse_slash_command("/review src/lib.rs"),
            Some(("review".into(), "src/lib.rs".into()))
        );
        assert_eq!(
            parse_slash_command("/compact"),
            Some(("compact".into(), String::new()))
        );
        assert_eq!(parse_slash_command("hello /x"), None);
        assert_eq!(parse_slash_command("/"), None);
    }

    #[test]
    fn mode_normalization() {
        assert_eq!(normalize_mode_id(Some(" plan ")), Some("plan".into()));
        assert_eq!(normalize_mode_id(Some("default")), None);
        assert_eq!(normalize_variant_id(Some("high")), Some("high".into()));
        assert_eq!(normalize_variant_id(Some("default")), None);
    }

    #[test]
    fn reply_mapping() {
        assert_eq!(
            resolve_permission_reply(&AgentPermissionResponse::deny("no")),
            PermissionReply::Reject
        );
        assert_eq!(
            resolve_permission_reply(&AgentPermissionResponse::allow()),
            PermissionReply::Once
        );
        assert_eq!(
            resolve_permission_reply(&AgentPermissionResponse::allow_always()),
            PermissionReply::Always
        );
    }
}
