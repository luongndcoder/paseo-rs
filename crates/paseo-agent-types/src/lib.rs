//! Agent timeline, stream-event, permission and tool-call types for the Paseo core.
//!
//! Types are hand-written (no derive) so optional-vs-null, key presence and unknown-key
//! preservation follow the TS daemon exactly. `wire::to_wire` is the allow-list/strip/validate
//! step that mirrors `serializeAgentStreamEvent` (zod `AgentStreamEventPayloadSchema`).

pub mod js_str;
pub mod json;
pub mod permission;
pub mod persistence;
pub mod stream_event;
pub mod timeline_item;
pub mod tool_call;
pub mod usage;
pub mod wire;

pub use json::{DecodeError, Map, Num};
pub use permission::{
    ActionIntent, ActionVariant, Metadata, PermissionAction, PermissionBehavior, PermissionKind,
    PermissionRequest, PermissionResponse,
};
pub use persistence::PersistenceHandle;
pub use stream_event::{
    AgentMode, AgentRuntimeInfo, AgentStreamEvent, AttentionReason, ProviderSubagentInputEvent,
    ProviderSubagentStatus,
};
pub use timeline_item::{
    AgentTaskItem, AgentTimelineItem, CompactionStatus, CompactionTrigger, NotificationLevel,
    TaskStatus,
};
pub use tool_call::{
    AgentMessage, AgentMessageEvent, SearchDetail, SearchMode, SearchToolName, SubAgentAction,
    ToolCallDetail, ToolCallIcon, ToolCallItem, ToolCallStatus, WebResult, WorktreeCommand,
    WorktreeCommandStatus, TOOL_CALL_ICON_NAMES,
};
pub use usage::AgentUsage;
pub use wire::{to_wire, WireEvent};
