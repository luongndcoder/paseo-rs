//! Permission request/response types.

use crate::json::*;
use crate::tool_call::ToolCallDetail;
use serde_json::Value;

/// `AgentMetadata` / `AgentPermissionUpdate`: an open string-keyed object.
pub type Metadata = Map;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionKind {
    Tool,
    Plan,
    Question,
    Mode,
    Other,
}

impl PermissionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tool => "tool",
            Self::Plan => "plan",
            Self::Question => "question",
            Self::Mode => "mode",
            Self::Other => "other",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "tool" => Self::Tool,
            "plan" => Self::Plan,
            "question" => Self::Question,
            "mode" => Self::Mode,
            "other" => Self::Other,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionBehavior {
    Allow,
    Deny,
}

impl PermissionBehavior {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionVariant {
    Primary,
    Secondary,
    Danger,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionIntent {
    Implement,
    ImplementResume,
    Dismiss,
}

impl ActionVariant {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::Secondary => "secondary",
            Self::Danger => "danger",
        }
    }
}

impl ActionIntent {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Implement => "implement",
            Self::ImplementResume => "implement_resume",
            Self::Dismiss => "dismiss",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PermissionAction {
    pub id: String,
    pub label: String,
    pub behavior: PermissionBehavior,
    pub variant: Option<ActionVariant>,
    pub intent: Option<ActionIntent>,
}

impl PermissionAction {
    pub fn to_value(&self) -> Value {
        let mut m = Map::new();
        put(&mut m, "id", self.id.clone());
        put(&mut m, "label", self.label.clone());
        put(&mut m, "behavior", self.behavior.as_str());
        put_opt(&mut m, "variant", self.variant.map(|v| v.as_str()));
        put_opt(&mut m, "intent", self.intent.map(|v| v.as_str()));
        Value::Object(m)
    }

    pub fn from_value(v: &Value) -> Result<Self> {
        let o = Obj::new(v, "permission action")?;
        let behavior = match o.req_str("behavior")?.as_str() {
            "allow" => PermissionBehavior::Allow,
            "deny" => PermissionBehavior::Deny,
            _ => return err("action.behavior"),
        };
        let variant = match o.opt_str("variant")?.as_deref() {
            None => None,
            Some("primary") => Some(ActionVariant::Primary),
            Some("secondary") => Some(ActionVariant::Secondary),
            Some("danger") => Some(ActionVariant::Danger),
            Some(_) => return err("action.variant"),
        };
        let intent = match o.opt_str("intent")?.as_deref() {
            None => None,
            Some("implement") => Some(ActionIntent::Implement),
            Some("implement_resume") => Some(ActionIntent::ImplementResume),
            Some("dismiss") => Some(ActionIntent::Dismiss),
            Some(_) => return err("action.intent"),
        };
        Ok(PermissionAction {
            id: o.req_str("id")?,
            label: o.req_str("label")?,
            behavior,
            variant,
            intent,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PermissionRequest {
    pub id: String,
    pub provider: String,
    pub name: String,
    pub kind: PermissionKind,
    pub title: Option<String>,
    pub description: Option<String>,
    pub input: Option<Metadata>,
    pub detail: Option<ToolCallDetail>,
    pub suggestions: Option<Vec<Metadata>>,
    pub actions: Option<Vec<PermissionAction>>,
    pub metadata: Option<Metadata>,
}

impl PermissionRequest {
    pub fn to_value(&self) -> Value {
        let mut m = Map::new();
        put(&mut m, "id", self.id.clone());
        put(&mut m, "provider", self.provider.clone());
        put(&mut m, "name", self.name.clone());
        put(&mut m, "kind", self.kind.as_str());
        put_opt_str(&mut m, "title", &self.title);
        put_opt_str(&mut m, "description", &self.description);
        put_opt_map(&mut m, "input", &self.input);
        if let Some(d) = &self.detail {
            put(&mut m, "detail", d.to_value());
        }
        if let Some(s) = &self.suggestions {
            put(
                &mut m,
                "suggestions",
                Value::Array(s.iter().cloned().map(Value::Object).collect()),
            );
        }
        if let Some(a) = &self.actions {
            put(
                &mut m,
                "actions",
                Value::Array(a.iter().map(PermissionAction::to_value).collect()),
            );
        }
        put_opt_map(&mut m, "metadata", &self.metadata);
        Value::Object(m)
    }

    pub fn from_value(v: &Value) -> Result<Self> {
        let o = Obj::new(v, "permission request")?;
        let kind = o.req_str("kind")?;
        let suggestions = match o.opt_array("suggestions")? {
            None => None,
            Some(a) => Some(
                a.iter()
                    .map(|x| match x {
                        Value::Object(m) => Ok(m.clone()),
                        _ => err("suggestions: expected object"),
                    })
                    .collect::<Result<Vec<_>>>()?,
            ),
        };
        let actions = match o.opt_array("actions")? {
            None => None,
            Some(a) => Some(
                a.iter()
                    .map(PermissionAction::from_value)
                    .collect::<Result<Vec<_>>>()?,
            ),
        };
        Ok(PermissionRequest {
            id: o.req_str("id")?,
            provider: o.req_str("provider")?,
            name: o.req_str("name")?,
            kind: PermissionKind::parse(&kind)
                .ok_or_else(|| DecodeError("permission.kind".into()))?,
            title: o.opt_str("title")?,
            description: o.opt_str("description")?,
            input: o.opt_map("input")?,
            detail: match o.get("detail") {
                None => None,
                Some(d) => Some(ToolCallDetail::from_value(d)?),
            },
            suggestions,
            actions,
            metadata: o.opt_map("metadata")?,
        })
    }
}

impl_serde_via_value!(PermissionRequest);

#[derive(Debug, Clone, PartialEq)]
pub enum PermissionResponse {
    Allow {
        selected_action_id: Option<String>,
        updated_input: Option<Metadata>,
        updated_permissions: Option<Vec<Metadata>>,
    },
    Deny {
        selected_action_id: Option<String>,
        message: Option<String>,
        interrupt: Option<bool>,
    },
}

impl PermissionResponse {
    pub fn to_value(&self) -> Value {
        let mut m = Map::new();
        match self {
            Self::Allow {
                selected_action_id,
                updated_input,
                updated_permissions,
            } => {
                put(&mut m, "behavior", "allow");
                put_opt_str(&mut m, "selectedActionId", selected_action_id);
                put_opt_map(&mut m, "updatedInput", updated_input);
                if let Some(p) = updated_permissions {
                    put(
                        &mut m,
                        "updatedPermissions",
                        Value::Array(p.iter().cloned().map(Value::Object).collect()),
                    );
                }
            }
            Self::Deny {
                selected_action_id,
                message,
                interrupt,
            } => {
                put(&mut m, "behavior", "deny");
                put_opt_str(&mut m, "selectedActionId", selected_action_id);
                put_opt_str(&mut m, "message", message);
                put_opt_bool(&mut m, "interrupt", *interrupt);
            }
        }
        Value::Object(m)
    }

    pub fn from_value(v: &Value) -> Result<Self> {
        let o = Obj::new(v, "permission response")?;
        Ok(match o.req_str("behavior")?.as_str() {
            "allow" => {
                let updated_permissions = match o.opt_array("updatedPermissions")? {
                    None => None,
                    Some(a) => Some(
                        a.iter()
                            .map(|x| match x {
                                Value::Object(m) => Ok(m.clone()),
                                _ => err("updatedPermissions: expected object"),
                            })
                            .collect::<Result<Vec<_>>>()?,
                    ),
                };
                Self::Allow {
                    selected_action_id: o.opt_str("selectedActionId")?,
                    updated_input: o.opt_map("updatedInput")?,
                    updated_permissions,
                }
            }
            "deny" => Self::Deny {
                selected_action_id: o.opt_str("selectedActionId")?,
                message: o.opt_str("message")?,
                interrupt: o.opt_bool("interrupt")?,
            },
            _ => return err("permission response.behavior"),
        })
    }
}

impl_serde_via_value!(PermissionResponse);
