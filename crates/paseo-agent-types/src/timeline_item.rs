//! `AgentTimelineItem` and `timelineItemIdentity`.

use crate::json::*;
use crate::tool_call::ToolCallItem;
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    Pending,
    InProgress,
    Completed,
}

impl TaskStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "pending" => Self::Pending,
            "in_progress" => Self::InProgress,
            "completed" => Self::Completed,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AgentTaskItem {
    pub text: String,
    pub completed: bool,
    pub id: Option<String>,
    pub status: Option<TaskStatus>,
    pub active_form: Option<String>,
}

impl AgentTaskItem {
    pub fn to_value(&self) -> Value {
        let mut m = Map::new();
        put(&mut m, "text", self.text.clone());
        put(&mut m, "completed", self.completed);
        put_opt_str(&mut m, "id", &self.id);
        put_opt(&mut m, "status", self.status.map(|s| s.as_str()));
        put_opt_str(&mut m, "activeForm", &self.active_form);
        Value::Object(m)
    }

    pub fn from_value(v: &Value) -> Result<Self> {
        let o = Obj::new(v, "todo item")?;
        let status = match o.opt_str("status")? {
            None => None,
            Some(s) => {
                Some(TaskStatus::parse(&s).ok_or_else(|| DecodeError("todo status".into()))?)
            }
        };
        Ok(AgentTaskItem {
            text: o.req_str("text")?,
            completed: o.req_bool("completed")?,
            id: o.opt_str("id")?,
            status,
            active_form: o.opt_str("activeForm")?,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationLevel {
    Info,
    Warning,
    Error,
}

impl NotificationLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "info" => Self::Info,
            "warning" => Self::Warning,
            "error" => Self::Error,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionStatus {
    Loading,
    Completed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionTrigger {
    Auto,
    Manual,
}

const COMPACTION_KNOWN: [&str; 4] = ["type", "status", "trigger", "preTokens"];

#[derive(Debug, Clone, PartialEq)]
pub enum AgentTimelineItem {
    UserMessage {
        text: String,
        message_id: Option<String>,
        client_message_id: Option<String>,
    },
    AssistantMessage {
        text: String,
        message_id: Option<String>,
    },
    Reasoning {
        text: String,
    },
    ToolCall(Box<ToolCallItem>),
    Todo {
        items: Vec<AgentTaskItem>,
    },
    Error {
        message: String,
    },
    Notification {
        level: NotificationLevel,
        message: String,
    },
    /// Index-signature item in TS (`[key: string]: unknown`); `extra` keeps unknown keys.
    Compaction {
        status: CompactionStatus,
        trigger: Option<CompactionTrigger>,
        pre_tokens: Option<Num>,
        extra: Map,
    },
    Plugin {
        id: String,
        plugin_id: String,
        kind: String,
        version: Num,
        data: Value,
    },
}

impl AgentTimelineItem {
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::UserMessage { .. } => "user_message",
            Self::AssistantMessage { .. } => "assistant_message",
            Self::Reasoning { .. } => "reasoning",
            Self::ToolCall(_) => "tool_call",
            Self::Todo { .. } => "todo",
            Self::Error { .. } => "error",
            Self::Notification { .. } => "notification",
            Self::Compaction { .. } => "compaction",
            Self::Plugin { .. } => "plugin",
        }
    }

    pub fn tool_call(item: ToolCallItem) -> Self {
        Self::ToolCall(Box::new(item))
    }

    pub fn as_tool_call(&self) -> Option<&ToolCallItem> {
        match self {
            Self::ToolCall(t) => Some(t),
            _ => None,
        }
    }

    /// `timelineItemIdentity` from `packages/protocol/src/timeline-identity.ts`.
    pub fn identity(&self) -> Option<String> {
        match self {
            Self::ToolCall(t) => Some(t.call_id.clone()),
            Self::Plugin { plugin_id, id, .. } => Some(format!("{plugin_id}/{id}")),
            _ => None,
        }
    }

    pub fn to_value(&self) -> Value {
        match self {
            Self::ToolCall(t) => t.to_value(),
            Self::UserMessage {
                text,
                message_id,
                client_message_id,
            } => {
                let mut m = Map::new();
                put(&mut m, "type", "user_message");
                put(&mut m, "text", text.clone());
                put_opt_str(&mut m, "messageId", message_id);
                put_opt_str(&mut m, "clientMessageId", client_message_id);
                Value::Object(m)
            }
            Self::AssistantMessage { text, message_id } => {
                let mut m = Map::new();
                put(&mut m, "type", "assistant_message");
                put(&mut m, "text", text.clone());
                put_opt_str(&mut m, "messageId", message_id);
                Value::Object(m)
            }
            Self::Reasoning { text } => {
                let mut m = Map::new();
                put(&mut m, "type", "reasoning");
                put(&mut m, "text", text.clone());
                Value::Object(m)
            }
            Self::Todo { items } => {
                let mut m = Map::new();
                put(&mut m, "type", "todo");
                put(
                    &mut m,
                    "items",
                    Value::Array(items.iter().map(AgentTaskItem::to_value).collect()),
                );
                Value::Object(m)
            }
            Self::Error { message } => {
                let mut m = Map::new();
                put(&mut m, "type", "error");
                put(&mut m, "message", message.clone());
                Value::Object(m)
            }
            Self::Notification { level, message } => {
                let mut m = Map::new();
                put(&mut m, "type", "notification");
                put(&mut m, "level", level.as_str());
                put(&mut m, "message", message.clone());
                Value::Object(m)
            }
            Self::Compaction {
                status,
                trigger,
                pre_tokens,
                extra,
            } => {
                let mut m = Map::new();
                put(&mut m, "type", "compaction");
                put(
                    &mut m,
                    "status",
                    match status {
                        CompactionStatus::Loading => "loading",
                        CompactionStatus::Completed => "completed",
                    },
                );
                put_opt(
                    &mut m,
                    "trigger",
                    trigger.map(|t| match t {
                        CompactionTrigger::Auto => "auto",
                        CompactionTrigger::Manual => "manual",
                    }),
                );
                put_opt_num(&mut m, "preTokens", pre_tokens);
                for (k, v) in extra {
                    if !COMPACTION_KNOWN.contains(&k.as_str()) {
                        m.insert(k.clone(), v.clone());
                    }
                }
                Value::Object(m)
            }
            Self::Plugin {
                id,
                plugin_id,
                kind,
                version,
                data,
            } => {
                let mut m = Map::new();
                put(&mut m, "type", "plugin");
                put(&mut m, "id", id.clone());
                put(&mut m, "pluginId", plugin_id.clone());
                put(&mut m, "kind", kind.clone());
                put(&mut m, "version", Value::Number(version.clone()));
                put(&mut m, "data", data.clone());
                Value::Object(m)
            }
        }
    }

    pub fn from_value(v: &Value) -> Result<Self> {
        let o = Obj::new(v, "timeline item")?;
        let ty = o.req_str("type")?;
        Ok(match ty.as_str() {
            "tool_call" => Self::ToolCall(Box::new(ToolCallItem::from_value(v)?)),
            "user_message" => Self::UserMessage {
                text: o.req_str("text")?,
                message_id: o.opt_str("messageId")?,
                client_message_id: o.opt_str("clientMessageId")?,
            },
            "assistant_message" => Self::AssistantMessage {
                text: o.req_str("text")?,
                message_id: o.opt_str("messageId")?,
            },
            "reasoning" => Self::Reasoning {
                text: o.req_str("text")?,
            },
            "todo" => Self::Todo {
                items: o
                    .req_array("items")?
                    .iter()
                    .map(AgentTaskItem::from_value)
                    .collect::<Result<Vec<_>>>()?,
            },
            "error" => Self::Error {
                message: o.req_str("message")?,
            },
            "notification" => {
                let level = o.req_str("level")?;
                Self::Notification {
                    level: NotificationLevel::parse(&level)
                        .ok_or_else(|| DecodeError("notification.level".into()))?,
                    message: o.req_str("message")?,
                }
            }
            "compaction" => {
                let status = match o.req_str("status")?.as_str() {
                    "loading" => CompactionStatus::Loading,
                    "completed" => CompactionStatus::Completed,
                    _ => return err("compaction.status"),
                };
                let trigger = match o.opt_str("trigger")? {
                    None => None,
                    Some(s) if s == "auto" => Some(CompactionTrigger::Auto),
                    Some(s) if s == "manual" => Some(CompactionTrigger::Manual),
                    Some(_) => return err("compaction.trigger"),
                };
                Self::Compaction {
                    status,
                    trigger,
                    pre_tokens: o.opt_num("preTokens")?,
                    extra: o.extras(&COMPACTION_KNOWN),
                }
            }
            "plugin" => Self::Plugin {
                id: o.req_str("id")?,
                plugin_id: o.req_str("pluginId")?,
                kind: o.req_str("kind")?,
                version: o.req_num("version")?,
                data: o.get("data").cloned().unwrap_or(Value::Null),
            },
            other => return err(format!("timeline item: unknown type {other}")),
        })
    }
}

impl_serde_via_value!(AgentTimelineItem);
