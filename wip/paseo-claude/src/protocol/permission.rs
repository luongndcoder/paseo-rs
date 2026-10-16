//! Permission modes, rule updates and the `can_use_tool` answer.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::lossless::Extra;

macro_rules! string_enum {
    ($(#[$m:meta])* $name:ident { $($variant:ident = $wire:literal),+ $(,)? }) => {
        $(#[$m])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub enum $name {
            $(#[serde(rename = $wire)] $variant,)+
            /// A value this crate does not know yet.
            #[serde(untagged)]
            Other(String),
        }
        impl $name {
            pub fn as_str(&self) -> &str {
                match self {
                    $(Self::$variant => $wire,)+
                    Self::Other(s) => s,
                }
            }
        }
    };
}
pub(crate) use string_enum;

string_enum!(
    /// `--permission-mode` / `set_permission_mode.mode`.
    PermissionMode {
        Default = "default",
        AcceptEdits = "acceptEdits",
        BypassPermissions = "bypassPermissions",
        Plan = "plan",
        DontAsk = "dontAsk",
        Auto = "auto",
    }
);

string_enum!(PermissionBehavior {
    Allow = "allow",
    Deny = "deny",
    Ask = "ask",
});

string_enum!(PermissionDestination {
    UserSettings = "userSettings",
    ProjectSettings = "projectSettings",
    LocalSettings = "localSettings",
    Session = "session",
    CliArg = "cliArg",
});

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PermissionRuleValue {
    #[serde(rename = "toolName")]
    pub tool_name: String,
    #[serde(
        rename = "ruleContent",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub rule_content: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// One entry of `permission_suggestions` / `updatedPermissions`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum PermissionUpdate {
    #[serde(rename = "addRules")]
    AddRules {
        rules: Vec<PermissionRuleValue>,
        behavior: PermissionBehavior,
        destination: PermissionDestination,
        #[serde(flatten)]
        extra: Extra,
    },
    #[serde(rename = "replaceRules")]
    ReplaceRules {
        rules: Vec<PermissionRuleValue>,
        behavior: PermissionBehavior,
        destination: PermissionDestination,
        #[serde(flatten)]
        extra: Extra,
    },
    #[serde(rename = "removeRules")]
    RemoveRules {
        rules: Vec<PermissionRuleValue>,
        behavior: PermissionBehavior,
        destination: PermissionDestination,
        #[serde(flatten)]
        extra: Extra,
    },
    #[serde(rename = "setMode")]
    SetMode {
        mode: PermissionMode,
        destination: PermissionDestination,
        #[serde(flatten)]
        extra: Extra,
    },
    #[serde(rename = "addDirectories")]
    AddDirectories {
        directories: Vec<String>,
        destination: PermissionDestination,
        #[serde(flatten)]
        extra: Extra,
    },
    #[serde(rename = "removeDirectories")]
    RemoveDirectories {
        directories: Vec<String>,
        destination: PermissionDestination,
        #[serde(flatten)]
        extra: Extra,
    },
    #[serde(untagged)]
    Unknown(Value),
}

/// Response payload for a `can_use_tool` control request.
///
/// `toolUseID` must echo the request's `tool_use_id` (the TS SDK always adds it).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "behavior")]
pub enum PermissionResult {
    #[serde(rename = "allow")]
    Allow {
        /// Always send it (echo the original input when unchanged); UNVERIFIED whether the
        /// CLI accepts an allow without it.
        #[serde(
            rename = "updatedInput",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        updated_input: Option<Value>,
        #[serde(
            rename = "updatedPermissions",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        updated_permissions: Option<Vec<PermissionUpdate>>,
        #[serde(rename = "toolUseID", default, skip_serializing_if = "Option::is_none")]
        tool_use_id: Option<String>,
        #[serde(
            rename = "decisionClassification",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        decision_classification: Option<Value>,
        #[serde(flatten)]
        extra: Extra,
    },
    #[serde(rename = "deny")]
    Deny {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        interrupt: Option<bool>,
        #[serde(rename = "toolUseID", default, skip_serializing_if = "Option::is_none")]
        tool_use_id: Option<String>,
        #[serde(
            rename = "decisionClassification",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        decision_classification: Option<Value>,
        #[serde(flatten)]
        extra: Extra,
    },
    #[serde(untagged)]
    Unknown(Value),
}

impl PermissionResult {
    pub fn allow(updated_input: Value, tool_use_id: impl Into<String>) -> Self {
        Self::Allow {
            updated_input: Some(updated_input),
            updated_permissions: None,
            tool_use_id: Some(tool_use_id.into()),
            decision_classification: None,
            extra: Extra::new(),
        }
    }

    pub fn deny(
        message: impl Into<String>,
        interrupt: bool,
        tool_use_id: impl Into<String>,
    ) -> Self {
        Self::Deny {
            message: message.into(),
            // The TS SDK forwards the host's explicit `interrupt: false`; capture 03 carries it.
            interrupt: Some(interrupt),
            tool_use_id: Some(tool_use_id.into()),
            decision_classification: None,
            extra: Extra::new(),
        }
    }
}
