//! `AgentStreamEvent` (all 14 variants), `AgentMode`, `AgentRuntimeInfo`,
//! `ProviderSubagentInputEvent`.

use crate::json::*;
use crate::permission::{PermissionRequest, PermissionResponse};
use crate::timeline_item::AgentTimelineItem;
use crate::usage::AgentUsage;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub struct AgentMode {
    pub id: String,
    pub label: String,
    pub description: Option<String>,
    pub icon: Option<String>,
    pub color_tier: Option<String>,
    pub is_unattended: Option<bool>,
}

impl AgentMode {
    pub fn to_value(&self) -> Value {
        let mut m = Map::new();
        put(&mut m, "id", self.id.clone());
        put(&mut m, "label", self.label.clone());
        put_opt_str(&mut m, "description", &self.description);
        put_opt_str(&mut m, "icon", &self.icon);
        put_opt_str(&mut m, "colorTier", &self.color_tier);
        put_opt_bool(&mut m, "isUnattended", self.is_unattended);
        Value::Object(m)
    }

    pub fn from_value(v: &Value) -> Result<Self> {
        let o = Obj::new(v, "mode")?;
        Ok(AgentMode {
            id: o.req_str("id")?,
            label: o.req_str("label")?,
            description: o.opt_str("description")?,
            icon: o.opt_str("icon")?,
            color_tier: o.opt_str("colorTier")?,
            is_unattended: o.opt_bool("isUnattended")?,
        })
    }
}

impl_serde_via_value!(AgentMode);

#[derive(Debug, Clone, PartialEq)]
pub struct AgentRuntimeInfo {
    pub provider: String,
    pub session_id: Option<String>,
    pub model: Option<Option<String>>,
    pub thinking_option_id: Option<Option<String>>,
    pub mode_id: Option<Option<String>>,
    pub extra: Option<Map>,
}

impl AgentRuntimeInfo {
    pub fn to_value(&self) -> Value {
        let mut m = Map::new();
        put(&mut m, "provider", self.provider.clone());
        put(
            &mut m,
            "sessionId",
            self.session_id
                .clone()
                .map(Value::String)
                .unwrap_or(Value::Null),
        );
        put_opt_nullable_str(&mut m, "model", &self.model);
        put_opt_nullable_str(&mut m, "thinkingOptionId", &self.thinking_option_id);
        put_opt_nullable_str(&mut m, "modeId", &self.mode_id);
        put_opt_map(&mut m, "extra", &self.extra);
        Value::Object(m)
    }

    pub fn from_value(v: &Value) -> Result<Self> {
        let o = Obj::new(v, "runtime info")?;
        Ok(AgentRuntimeInfo {
            provider: o.req_str("provider")?,
            session_id: o.req_nullable_str("sessionId")?,
            model: o.opt_nullable_str("model")?,
            thinking_option_id: o.opt_nullable_str("thinkingOptionId")?,
            mode_id: o.opt_nullable_str("modeId")?,
            extra: o.opt_map("extra")?,
        })
    }
}

impl_serde_via_value!(AgentRuntimeInfo);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderSubagentStatus {
    Running,
    Completed,
    Failed,
    Canceled,
}

impl ProviderSubagentStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Canceled => "canceled",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "running" => Self::Running,
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            "canceled" => Self::Canceled,
            _ => return None,
        })
    }
}

/// `ProviderSubagentInputEvent` (`provider-subagents/store.ts`). `Option<Option<_>>` keeps the
/// TS distinction between an omitted field (keep stored value) and an explicit `null` (clear).
#[derive(Debug, Clone, PartialEq)]
pub enum ProviderSubagentInputEvent {
    Upsert {
        id: String,
        title: Option<Option<String>>,
        description: Option<Option<String>>,
        status: Option<ProviderSubagentStatus>,
        tool_call_id: Option<Option<String>>,
        cwd: Option<Option<String>>,
        subtitle: Option<Option<String>>,
        parent_subagent_id: Option<Option<String>>,
        timestamp: Option<String>,
    },
    Timeline {
        id: String,
        item: Box<AgentTimelineItem>,
        timestamp: Option<String>,
    },
    Remove {
        id: String,
    },
}

impl ProviderSubagentInputEvent {
    pub fn to_value(&self) -> Value {
        let mut m = Map::new();
        match self {
            Self::Upsert {
                id,
                title,
                description,
                status,
                tool_call_id,
                cwd,
                subtitle,
                parent_subagent_id,
                timestamp,
            } => {
                put(&mut m, "type", "upsert");
                put(&mut m, "id", id.clone());
                put_opt_nullable_str(&mut m, "title", title);
                put_opt_nullable_str(&mut m, "description", description);
                put_opt(&mut m, "status", status.map(|s| s.as_str()));
                put_opt_nullable_str(&mut m, "toolCallId", tool_call_id);
                put_opt_nullable_str(&mut m, "cwd", cwd);
                put_opt_nullable_str(&mut m, "subtitle", subtitle);
                put_opt_nullable_str(&mut m, "parentSubagentId", parent_subagent_id);
                put_opt_str(&mut m, "timestamp", timestamp);
            }
            Self::Timeline {
                id,
                item,
                timestamp,
            } => {
                put(&mut m, "type", "timeline");
                put(&mut m, "id", id.clone());
                put(&mut m, "item", item.to_value());
                put_opt_str(&mut m, "timestamp", timestamp);
            }
            Self::Remove { id } => {
                put(&mut m, "type", "remove");
                put(&mut m, "id", id.clone());
            }
        }
        Value::Object(m)
    }

    pub fn from_value(v: &Value) -> Result<Self> {
        let o = Obj::new(v, "provider subagent event")?;
        Ok(match o.req_str("type")?.as_str() {
            "upsert" => {
                let status = match o.opt_str("status")? {
                    None => None,
                    Some(s) => Some(
                        ProviderSubagentStatus::parse(&s)
                            .ok_or_else(|| DecodeError("subagent status".into()))?,
                    ),
                };
                Self::Upsert {
                    id: o.req_str("id")?,
                    title: o.opt_nullable_str("title")?,
                    description: o.opt_nullable_str("description")?,
                    status,
                    tool_call_id: o.opt_nullable_str("toolCallId")?,
                    cwd: o.opt_nullable_str("cwd")?,
                    subtitle: o.opt_nullable_str("subtitle")?,
                    parent_subagent_id: o.opt_nullable_str("parentSubagentId")?,
                    timestamp: o.opt_str("timestamp")?,
                }
            }
            "timeline" => Self::Timeline {
                id: o.req_str("id")?,
                item: Box::new(AgentTimelineItem::from_value(o.get("item").ok_or_else(
                    || DecodeError("subagent timeline.item missing".into()),
                )?)?),
                timestamp: o.opt_str("timestamp")?,
            },
            "remove" => Self::Remove {
                id: o.req_str("id")?,
            },
            _ => return err("provider subagent event.type"),
        })
    }
}

impl_serde_via_value!(ProviderSubagentInputEvent);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttentionReason {
    Finished,
    Error,
    Permission,
}

impl AttentionReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Finished => "finished",
            Self::Error => "error",
            Self::Permission => "permission",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "finished" => Self::Finished,
            "error" => Self::Error,
            "permission" => Self::Permission,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum AgentStreamEvent {
    ThreadStarted {
        session_id: String,
        provider: String,
    },
    TurnStarted {
        provider: String,
        turn_id: Option<String>,
    },
    TurnCompleted {
        provider: String,
        usage: Option<AgentUsage>,
        turn_id: Option<String>,
    },
    UsageUpdated {
        provider: String,
        usage: AgentUsage,
        turn_id: Option<String>,
    },
    ModeChanged {
        provider: String,
        current_mode_id: Option<String>,
        available_modes: Vec<AgentMode>,
    },
    ModelChanged {
        provider: String,
        runtime_info: AgentRuntimeInfo,
    },
    ThinkingOptionChanged {
        provider: String,
        thinking_option_id: Option<String>,
    },
    TurnFailed {
        provider: String,
        error: String,
        code: Option<String>,
        diagnostic: Option<String>,
        turn_id: Option<String>,
    },
    TurnCanceled {
        provider: String,
        reason: String,
        turn_id: Option<String>,
    },
    Timeline {
        item: Box<AgentTimelineItem>,
        provider: String,
        turn_id: Option<String>,
        timestamp: Option<String>,
    },
    PermissionRequested {
        provider: String,
        request: Box<PermissionRequest>,
        turn_id: Option<String>,
    },
    PermissionResolved {
        provider: String,
        request_id: String,
        resolution: PermissionResponse,
        turn_id: Option<String>,
    },
    AttentionRequired {
        provider: String,
        reason: AttentionReason,
        timestamp: String,
    },
    ProviderSubagent {
        provider: String,
        event: Box<ProviderSubagentInputEvent>,
    },
}

impl AgentStreamEvent {
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::ThreadStarted { .. } => "thread_started",
            Self::TurnStarted { .. } => "turn_started",
            Self::TurnCompleted { .. } => "turn_completed",
            Self::UsageUpdated { .. } => "usage_updated",
            Self::ModeChanged { .. } => "mode_changed",
            Self::ModelChanged { .. } => "model_changed",
            Self::ThinkingOptionChanged { .. } => "thinking_option_changed",
            Self::TurnFailed { .. } => "turn_failed",
            Self::TurnCanceled { .. } => "turn_canceled",
            Self::Timeline { .. } => "timeline",
            Self::PermissionRequested { .. } => "permission_requested",
            Self::PermissionResolved { .. } => "permission_resolved",
            Self::AttentionRequired { .. } => "attention_required",
            Self::ProviderSubagent { .. } => "provider_subagent",
        }
    }

    pub fn provider(&self) -> &str {
        match self {
            Self::ThreadStarted { provider, .. }
            | Self::TurnStarted { provider, .. }
            | Self::TurnCompleted { provider, .. }
            | Self::UsageUpdated { provider, .. }
            | Self::ModeChanged { provider, .. }
            | Self::ModelChanged { provider, .. }
            | Self::ThinkingOptionChanged { provider, .. }
            | Self::TurnFailed { provider, .. }
            | Self::TurnCanceled { provider, .. }
            | Self::Timeline { provider, .. }
            | Self::PermissionRequested { provider, .. }
            | Self::PermissionResolved { provider, .. }
            | Self::AttentionRequired { provider, .. }
            | Self::ProviderSubagent { provider, .. } => provider,
        }
    }

    /// `getAgentStreamEventTurnId`: `"turnId" in event ? event.turnId : undefined`.
    pub fn turn_id(&self) -> Option<&str> {
        match self {
            Self::TurnStarted { turn_id, .. }
            | Self::TurnCompleted { turn_id, .. }
            | Self::UsageUpdated { turn_id, .. }
            | Self::TurnFailed { turn_id, .. }
            | Self::TurnCanceled { turn_id, .. }
            | Self::Timeline { turn_id, .. }
            | Self::PermissionRequested { turn_id, .. }
            | Self::PermissionResolved { turn_id, .. } => turn_id.as_deref(),
            _ => None,
        }
    }

    pub fn to_value(&self) -> Value {
        let mut m = Map::new();
        put(&mut m, "type", self.type_name());
        match self {
            Self::ThreadStarted {
                session_id,
                provider,
            } => {
                put(&mut m, "sessionId", session_id.clone());
                put(&mut m, "provider", provider.clone());
            }
            Self::TurnStarted { provider, turn_id } => {
                put(&mut m, "provider", provider.clone());
                put_opt_str(&mut m, "turnId", turn_id);
            }
            Self::TurnCompleted {
                provider,
                usage,
                turn_id,
            } => {
                put(&mut m, "provider", provider.clone());
                if let Some(u) = usage {
                    put(&mut m, "usage", u.to_value());
                }
                put_opt_str(&mut m, "turnId", turn_id);
            }
            Self::UsageUpdated {
                provider,
                usage,
                turn_id,
            } => {
                put(&mut m, "provider", provider.clone());
                put(&mut m, "usage", usage.to_value());
                put_opt_str(&mut m, "turnId", turn_id);
            }
            Self::ModeChanged {
                provider,
                current_mode_id,
                available_modes,
            } => {
                put(&mut m, "provider", provider.clone());
                put(
                    &mut m,
                    "currentModeId",
                    current_mode_id
                        .clone()
                        .map(Value::String)
                        .unwrap_or(Value::Null),
                );
                put(
                    &mut m,
                    "availableModes",
                    Value::Array(available_modes.iter().map(AgentMode::to_value).collect()),
                );
            }
            Self::ModelChanged {
                provider,
                runtime_info,
            } => {
                put(&mut m, "provider", provider.clone());
                put(&mut m, "runtimeInfo", runtime_info.to_value());
            }
            Self::ThinkingOptionChanged {
                provider,
                thinking_option_id,
            } => {
                put(&mut m, "provider", provider.clone());
                put(
                    &mut m,
                    "thinkingOptionId",
                    thinking_option_id
                        .clone()
                        .map(Value::String)
                        .unwrap_or(Value::Null),
                );
            }
            Self::TurnFailed {
                provider,
                error,
                code,
                diagnostic,
                turn_id,
            } => {
                put(&mut m, "provider", provider.clone());
                put(&mut m, "error", error.clone());
                put_opt_str(&mut m, "code", code);
                put_opt_str(&mut m, "diagnostic", diagnostic);
                put_opt_str(&mut m, "turnId", turn_id);
            }
            Self::TurnCanceled {
                provider,
                reason,
                turn_id,
            } => {
                put(&mut m, "provider", provider.clone());
                put(&mut m, "reason", reason.clone());
                put_opt_str(&mut m, "turnId", turn_id);
            }
            Self::Timeline {
                item,
                provider,
                turn_id,
                timestamp,
            } => {
                put(&mut m, "item", item.to_value());
                put(&mut m, "provider", provider.clone());
                put_opt_str(&mut m, "turnId", turn_id);
                put_opt_str(&mut m, "timestamp", timestamp);
            }
            Self::PermissionRequested {
                provider,
                request,
                turn_id,
            } => {
                put(&mut m, "provider", provider.clone());
                put(&mut m, "request", request.to_value());
                put_opt_str(&mut m, "turnId", turn_id);
            }
            Self::PermissionResolved {
                provider,
                request_id,
                resolution,
                turn_id,
            } => {
                put(&mut m, "provider", provider.clone());
                put(&mut m, "requestId", request_id.clone());
                put(&mut m, "resolution", resolution.to_value());
                put_opt_str(&mut m, "turnId", turn_id);
            }
            Self::AttentionRequired {
                provider,
                reason,
                timestamp,
            } => {
                put(&mut m, "provider", provider.clone());
                put(&mut m, "reason", reason.as_str());
                put(&mut m, "timestamp", timestamp.clone());
            }
            Self::ProviderSubagent { provider, event } => {
                put(&mut m, "provider", provider.clone());
                put(&mut m, "event", event.to_value());
            }
        }
        Value::Object(m)
    }

    pub fn from_value(v: &Value) -> Result<Self> {
        let o = Obj::new(v, "stream event")?;
        let provider = || o.req_str("provider");
        let turn_id = || o.opt_str("turnId");
        let sub = |key: &str| -> Result<&Value> {
            o.get(key)
                .ok_or_else(|| DecodeError(format!("stream event.{key} missing")))
        };
        Ok(match o.req_str("type")?.as_str() {
            "thread_started" => Self::ThreadStarted {
                session_id: o.req_str("sessionId")?,
                provider: provider()?,
            },
            "turn_started" => Self::TurnStarted {
                provider: provider()?,
                turn_id: turn_id()?,
            },
            "turn_completed" => Self::TurnCompleted {
                provider: provider()?,
                usage: match o.get("usage") {
                    None => None,
                    Some(u) => Some(AgentUsage::from_value(u)?),
                },
                turn_id: turn_id()?,
            },
            "usage_updated" => Self::UsageUpdated {
                provider: provider()?,
                usage: AgentUsage::from_value(sub("usage")?)?,
                turn_id: turn_id()?,
            },
            "mode_changed" => Self::ModeChanged {
                provider: provider()?,
                current_mode_id: o.req_nullable_str("currentModeId")?,
                available_modes: o
                    .req_array("availableModes")?
                    .iter()
                    .map(AgentMode::from_value)
                    .collect::<Result<Vec<_>>>()?,
            },
            "model_changed" => Self::ModelChanged {
                provider: provider()?,
                runtime_info: AgentRuntimeInfo::from_value(sub("runtimeInfo")?)?,
            },
            "thinking_option_changed" => Self::ThinkingOptionChanged {
                provider: provider()?,
                thinking_option_id: o.req_nullable_str("thinkingOptionId")?,
            },
            "turn_failed" => Self::TurnFailed {
                provider: provider()?,
                error: o.req_str("error")?,
                code: o.opt_str("code")?,
                diagnostic: o.opt_str("diagnostic")?,
                turn_id: turn_id()?,
            },
            "turn_canceled" => Self::TurnCanceled {
                provider: provider()?,
                reason: o.req_str("reason")?,
                turn_id: turn_id()?,
            },
            "timeline" => Self::Timeline {
                item: Box::new(AgentTimelineItem::from_value(sub("item")?)?),
                provider: provider()?,
                turn_id: turn_id()?,
                timestamp: o.opt_str("timestamp")?,
            },
            "permission_requested" => Self::PermissionRequested {
                provider: provider()?,
                request: Box::new(PermissionRequest::from_value(sub("request")?)?),
                turn_id: turn_id()?,
            },
            "permission_resolved" => Self::PermissionResolved {
                provider: provider()?,
                request_id: o.req_str("requestId")?,
                resolution: PermissionResponse::from_value(sub("resolution")?)?,
                turn_id: turn_id()?,
            },
            "attention_required" => {
                let reason = o.req_str("reason")?;
                Self::AttentionRequired {
                    provider: provider()?,
                    reason: AttentionReason::parse(&reason)
                        .ok_or_else(|| DecodeError("attention reason".into()))?,
                    timestamp: o.req_str("timestamp")?,
                }
            }
            "provider_subagent" => Self::ProviderSubagent {
                provider: provider()?,
                event: Box::new(ProviderSubagentInputEvent::from_value(sub("event")?)?),
            },
            other => return err(format!("stream event: unknown type {other}")),
        })
    }
}

impl_serde_via_value!(AgentStreamEvent);
