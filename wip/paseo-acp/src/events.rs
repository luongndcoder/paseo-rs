//! Raw session events. The TS adapter maps these to timeline items and `ToolCallDetail`
//! (P4); this crate hands out the ACP payloads untouched.

use serde_json::Value;

use crate::agent::{AgentMode, PermissionResponse, RuntimeInfo, SlashCommand, TurnUsage};
use crate::protocol::{RequestPermissionRequest, SessionNotification};

#[derive(Debug, Clone, PartialEq)]
pub enum SessionEvent {
    ThreadStarted {
        session_id: String,
    },
    TurnStarted {
        turn_id: String,
        message_id: String,
    },
    /// A `session/update` for this session, as received.
    Update(SessionNotification),
    /// A notification that was not a well-formed `session/update` (vendor extension etc.).
    ExtNotification {
        method: String,
        params: Value,
    },
    PermissionRequested {
        request_id: String,
        turn_id: Option<String>,
        request: RequestPermissionRequest,
    },
    PermissionResolved {
        request_id: String,
        turn_id: Option<String>,
        response: PermissionResponse,
    },
    ModeChanged {
        current_mode_id: Option<String>,
        available_modes: Vec<AgentMode>,
    },
    ModelChanged {
        runtime: RuntimeInfo,
    },
    ThinkingOptionChanged {
        thinking_option_id: Option<String>,
    },
    CommandsUpdated {
        commands: Vec<SlashCommand>,
    },
    UsageUpdated {
        usage: TurnUsage,
        turn_id: Option<String>,
    },
    TurnCompleted {
        turn_id: String,
        usage: Option<TurnUsage>,
    },
    TurnFailed {
        turn_id: String,
        error: String,
        code: Option<String>,
        diagnostic: Option<String>,
    },
    TurnCanceled {
        turn_id: String,
        reason: String,
    },
    /// A selection the session ignored (invalid mode/model id, missing config value).
    Warning {
        message: String,
    },
}

impl SessionEvent {
    pub fn is_turn_end(&self) -> bool {
        matches!(
            self,
            SessionEvent::TurnCompleted { .. }
                | SessionEvent::TurnFailed { .. }
                | SessionEvent::TurnCanceled { .. }
        )
    }
}
