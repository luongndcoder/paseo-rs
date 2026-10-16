//! `<paseo-system>` envelope handling (`agent/agent-messages/index.ts`): format, parse and the
//! single projection that owns provider echoes, acceptance, import and replay.

use std::sync::OnceLock;

use paseo_agent_types::{
    AgentMessage, AgentMessageEvent, AgentTimelineItem, ToolCallDetail, ToolCallIcon, ToolCallItem,
    ToolCallStatus,
};
use regex::Regex;

#[derive(Debug, Clone, PartialEq)]
pub enum PromptSource {
    AgentMessage {
        agent_id: String,
        title: Option<String>,
    },
    AgentNotification {
        agent_id: String,
        title: Option<String>,
        event: AgentMessageEvent,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParsedAgentMessage {
    pub id: String,
    pub source: Option<PromptSource>,
    pub text: String,
}

fn escape_xml(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

const ENTITIES: [(&str, char); 5] = [
    ("&amp;", '&'),
    ("&lt;", '<'),
    ("&gt;", '>'),
    ("&quot;", '"'),
    ("&apos;", '\''),
];

/// `None` when the value holds a raw `<`/`>` or an `&` that starts no known entity.
fn unescape_xml(value: &str) -> Option<String> {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(c) = rest.chars().next() {
        match c {
            '<' | '>' => return None,
            '&' => {
                let (entity, ch) = ENTITIES.iter().find(|(e, _)| rest.starts_with(e))?;
                out.push(*ch);
                rest = &rest[entity.len()..];
            }
            c => {
                out.push(c);
                rest = &rest[c.len_utf8()..];
            }
        }
    }
    Some(out)
}

/// JS `\s`: WhiteSpace + LineTerminator.
const JS_WS: &str = r"[\t\n\x0B\x0C\r \x{a0}\x{1680}\x{2000}-\x{200a}\x{2028}\x{2029}\x{202f}\x{205f}\x{3000}\x{feff}]";

fn envelope_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(&format!(
            r#"^<paseo-system((?:{JS_WS}+[a-z-]+="[^"<>]*")+)>\n((?s:.*))\n</paseo-system>$"#
        ))
        .expect("envelope regex")
    })
}

fn attribute_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(&format!(r#"{JS_WS}+([a-z-]+)="([^"<>]*)""#)).expect("attribute regex")
    })
}

fn system_envelope_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^<paseo-system>\n(?s:.*)\n</paseo-system>$").expect("system envelope regex")
    })
}

/// `formatAgentMessage`.
pub fn format_agent_message(id: &str, source: Option<&PromptSource>, text: &str) -> String {
    let attributes = match source {
        Some(PromptSource::AgentMessage { agent_id, .. }) => format!(
            r#"kind="agent-message" source-agent-id="{}""#,
            escape_xml(agent_id)
        ),
        Some(PromptSource::AgentNotification { agent_id, .. }) => format!(
            r#"kind="agent-notification" source-agent-id="{}""#,
            escape_xml(agent_id)
        ),
        None => r#"kind="user-message""#.to_string(),
    };
    let title = match source {
        Some(
            PromptSource::AgentMessage { title: Some(t), .. }
            | PromptSource::AgentNotification { title: Some(t), .. },
        ) if !t.is_empty() => format!(r#" source-agent-title="{}""#, escape_xml(t)),
        _ => String::new(),
    };
    let event = match source {
        Some(PromptSource::AgentNotification { event, .. }) => {
            format!(r#" event="{}""#, event.as_str())
        }
        _ => String::new(),
    };
    format!(
        "<paseo-system version=\"1\" {attributes} message-id=\"{}\"{title}{event}>\n{}\n</paseo-system>",
        escape_xml(id),
        escape_xml(text)
    )
}

/// `parseAgentMessage`.
pub fn parse_agent_message(text: &str) -> Option<ParsedAgentMessage> {
    let envelope = envelope_re().captures(text)?;
    let mut attributes: Vec<(String, String)> = Vec::new();
    for m in attribute_re().captures_iter(&envelope[1]) {
        let value = unescape_xml(&m[2])?;
        if attributes.iter().any(|(k, _)| k == &m[1]) {
            return None;
        }
        attributes.push((m[1].to_string(), value));
    }
    let get = |k: &str| {
        attributes
            .iter()
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.as_str())
    };
    if get("version") != Some("1") {
        return None;
    }
    let id = get("message-id").filter(|s| !s.is_empty())?;
    let body = unescape_xml(&envelope[2])?;
    if get("kind") == Some("user-message") {
        if get("source-agent-id").is_some() || get("event").is_some() {
            return None;
        }
        return Some(ParsedAgentMessage {
            id: id.to_string(),
            source: None,
            text: body,
        });
    }
    let agent_id = get("source-agent-id")
        .filter(|s| !s.is_empty())?
        .to_string();
    // zod `title: z.string().min(1).optional()`: an empty title fails the parse.
    let title = match get("source-agent-title") {
        Some("") => return None,
        Some(t) => Some(t.to_string()),
        None => None,
    };
    let source = match get("kind")? {
        "agent-message" => PromptSource::AgentMessage { agent_id, title },
        "agent-notification" => {
            let event = match get("event")? {
                "finished" => AgentMessageEvent::Finished,
                "errored" => AgentMessageEvent::Errored,
                "permission-required" => AgentMessageEvent::PermissionRequired,
                "closed" => AgentMessageEvent::Closed,
                _ => return None,
            };
            PromptSource::AgentNotification {
                agent_id,
                title,
                event,
            }
        }
        _ => return None,
    };
    Some(ParsedAgentMessage {
        id: id.to_string(),
        source: Some(source),
        text: body,
    })
}

/// `formatSystemNotificationPrompt`.
pub fn format_system_notification_prompt(reason: &str) -> String {
    format!("<paseo-system>\n{reason}\n</paseo-system>")
}

/// `isSystemInjectedEnvelope`.
pub fn is_system_injected_envelope(text: &str) -> bool {
    system_envelope_re().is_match(text)
}

/// `projectAgentMessage`: user messages carrying a versioned envelope become the plain user row
/// (human) or a completed `agent_message` tool call (agent); legacy system envelopes are dropped.
pub fn project_agent_message(item: &AgentTimelineItem) -> Option<AgentTimelineItem> {
    let AgentTimelineItem::UserMessage {
        text, message_id, ..
    } = item
    else {
        return Some(item.clone());
    };
    let Some(message) = parse_agent_message(text) else {
        // COMPAT(legacySystemEnvelope): added in v0.11.1; remove after 2027-04-08 once legacy
        // notification history is retired. Schedules still use this envelope.
        return if is_system_injected_envelope(text) {
            None
        } else {
            Some(item.clone())
        };
    };
    let Some(source) = message.source else {
        return Some(AgentTimelineItem::UserMessage {
            text: message.text,
            message_id: message_id.clone(),
            client_message_id: Some(message.id),
        });
    };
    let (event, sender_id, sender_title) = match source {
        PromptSource::AgentMessage { agent_id, title } => {
            (AgentMessageEvent::Message, agent_id, title)
        }
        PromptSource::AgentNotification {
            agent_id,
            title,
            event,
        } => (event, agent_id, title),
    };
    let mut call = ToolCallItem::new(
        format!("paseo-agent-message:{}", message.id),
        // COMPAT(agentMessageToolEnvelope): added in v0.11.1; remove after 2027-04-08 once
        // clients can accept a dedicated timeline variant. Older clients still parse this
        // existing shape and can expand the delivered body.
        "agent_message",
        ToolCallStatus::Completed,
        ToolCallDetail::PlainText {
            label: None,
            text: Some(message.text.clone()),
            icon: Some(ToolCallIcon::Bot),
        },
    );
    call.agent_message = Some(AgentMessage {
        event,
        sender_id,
        sender_title: sender_title.filter(|t| !t.is_empty()),
        text: message.text,
    });
    Some(AgentTimelineItem::tool_call(call))
}
