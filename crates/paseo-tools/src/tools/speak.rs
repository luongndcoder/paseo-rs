//! `speak`: narrate through the daemon's voice output.

use std::sync::Arc;

use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use super::{handler, ok_structured, required_str, ToolEnv, ToolOutcome};
use crate::catalog::Registrar;
use crate::error::ToolError;

pub(crate) fn register(r: &mut Registrar) {
    r.register("speak", handler(speak));
}

async fn speak(env: Arc<ToolEnv>, input: Value, cancel: CancellationToken) -> ToolOutcome {
    let Some(caller) = &env.caller_agent_id else {
        return Err(ToolError::new(
            "speak is only available to agent-scoped tool sessions",
        ));
    };
    if !env.core.has_speak_handler(caller) {
        return Err(ToolError::new(format!(
            "No speak handler registered for your session '{caller}'"
        )));
    }
    env.core
        .speak(caller, required_str(&input, "text"), cancel)
        .await?;
    ok_structured(json!({"ok": true}))
}
