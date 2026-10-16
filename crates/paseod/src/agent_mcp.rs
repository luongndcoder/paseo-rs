//! In-process `/mcp/agents` (WP11): the Paseo tool catalog over MCP, served by the edge instead of
//! reverse proxied to TS.
//!
//! The endpoint belongs to the core: it needs the [`CoreApi`] over the agent manager, so it moves
//! only together with the agent manager (docs/p4-core-spec.md section 7) and only under
//! `PASEO_CORE=rust`. Whoever builds the core calls [`mount`] with its `CoreApi`; until then
//! [`crate::http::AppState`] has no `agent_mcp` and the route keeps proxying to TS.

use std::sync::Arc;

use paseo_tools::CoreApi;

use crate::http::AppState;

pub use paseo_mcp::{mcp_router, mcp_router_with, mint_capability_token, McpOptions};

/// `PASEO_CORE=rust`: the Rust core owns the agent control plane.
pub fn core_rust_enabled(env: &dyn Fn(&str) -> Option<String>) -> bool {
    env("PASEO_CORE").is_some_and(|v| v.trim().eq_ignore_ascii_case("rust"))
}

/// Serves `/mcp/agents` in process over `core`. `options.capability_token` is the token the core
/// injects into the agents' `paseo` MCP entry; `options.password_hash` is the daemon password.
pub fn mount(state: AppState, core: Arc<dyn CoreApi>, options: McpOptions) -> AppState {
    state.with_agent_mcp(mcp_router_with(core, options))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_flag_is_exactly_rust() {
        let env =
            |value: &'static str| move |key: &str| (key == "PASEO_CORE").then(|| value.to_owned());
        assert!(core_rust_enabled(&env("rust")));
        assert!(core_rust_enabled(&env(" Rust ")));
        assert!(!core_rust_enabled(&env("ts")));
        assert!(!core_rust_enabled(&env("")));
        assert!(!core_rust_enabled(&|_: &str| None));
    }
}
