//! ACP wire types. Hand-rolled against `@agentclientprotocol/sdk` 0.17 (protocol version 1).
//!
//! Every union that agents extend (content blocks, session updates, config options, stop
//! reasons) is `Lenient<T>`: unknown shapes land in `Unknown(Value)` and round-trip intact.

pub mod content;
pub mod jsonrpc;
pub mod lenient;
pub mod requests;
pub mod session;

pub use content::*;
pub use jsonrpc::*;
pub use lenient::Lenient;
pub use requests::*;
pub use session::*;

/// Agent-side method names (client -> agent).
pub mod agent_methods {
    pub const AUTHENTICATE: &str = "authenticate";
    pub const INITIALIZE: &str = "initialize";
    pub const LOGOUT: &str = "logout";
    pub const SESSION_CANCEL: &str = "session/cancel";
    pub const SESSION_CLOSE: &str = "session/close";
    pub const SESSION_FORK: &str = "session/fork";
    pub const SESSION_LIST: &str = "session/list";
    pub const SESSION_LOAD: &str = "session/load";
    pub const SESSION_NEW: &str = "session/new";
    pub const SESSION_PROMPT: &str = "session/prompt";
    pub const SESSION_RESUME: &str = "session/resume";
    pub const SESSION_SET_CONFIG_OPTION: &str = "session/set_config_option";
    pub const SESSION_SET_MODE: &str = "session/set_mode";
    pub const SESSION_SET_MODEL: &str = "session/set_model";
}

/// Client-side method names (agent -> client).
pub mod client_methods {
    pub const FS_READ_TEXT_FILE: &str = "fs/read_text_file";
    pub const FS_WRITE_TEXT_FILE: &str = "fs/write_text_file";
    pub const SESSION_ELICITATION: &str = "session/elicitation";
    pub const SESSION_ELICITATION_COMPLETE: &str = "session/elicitation/complete";
    pub const SESSION_REQUEST_PERMISSION: &str = "session/request_permission";
    pub const SESSION_UPDATE: &str = "session/update";
    pub const TERMINAL_CREATE: &str = "terminal/create";
    pub const TERMINAL_KILL: &str = "terminal/kill";
    pub const TERMINAL_OUTPUT: &str = "terminal/output";
    pub const TERMINAL_RELEASE: &str = "terminal/release";
    pub const TERMINAL_WAIT_FOR_EXIT: &str = "terminal/wait_for_exit";
}
