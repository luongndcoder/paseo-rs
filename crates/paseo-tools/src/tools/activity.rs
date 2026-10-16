//! Agent activity, mode and permission tools.

use std::sync::Arc;

use paseo_core::activity::{curate_agent_activity, ActivityOptions};
use paseo_timeline::{select_items_by_projected_limit, Direction};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use super::{handler, ok_structured, required_str, success, ToolEnv, ToolOutcome};
use crate::catalog::Registrar;

pub(crate) fn register(r: &mut Registrar) {
    r.register("get_agent_activity", handler(get_agent_activity));
    r.register("set_agent_mode", handler(set_agent_mode));
    r.register(
        "list_pending_permissions",
        handler(list_pending_permissions),
    );
    r.register("respond_to_permission", handler(respond_to_permission));
}

/// JS `String(number)` for the limits a model can pass.
pub(crate) fn js_number(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e21 {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
}

async fn get_agent_activity(env: Arc<ToolEnv>, input: Value, _c: CancellationToken) -> ToolOutcome {
    let agent_id = required_str(&input, "agentId");
    let limit = input.get("limit").and_then(Value::as_f64);
    env.core.ensure_agent_loaded(agent_id).await?;
    let timeline = env.core.agent_timeline(agent_id);
    let snapshot = env.core.get_agent(agent_id);

    // `Math.max(0, Math.floor(limit ?? 0))`
    let window = limit.map_or(0.0, |l| l.floor().max(0.0)) as usize;
    let selection = select_items_by_projected_limit(&timeline, Direction::Tail, window);
    let curated = curate_agent_activity(&selection.items, &ActivityOptions::default());
    let total = selection.total_projected;
    let shown = selection.shown_projected;

    let noun = if total == 1 { "activity" } else { "activities" };
    // `limit && shown < total`: a zero (or absent) limit is falsy.
    let count_header = match limit {
        Some(l) if l != 0.0 && shown < total => format!(
            "Showing {shown} of {total} {noun} (limited to {})",
            js_number(l)
        ),
        _ => format!("Showing all {total} {noun}"),
    };
    ok_structured(json!({
        "agentId": agent_id,
        "updateCount": timeline.len(),
        "currentModeId": snapshot.and_then(|s| s.current_mode_id),
        "content": format!("{count_header}\n\n{curated}"),
    }))
}

async fn set_agent_mode(env: Arc<ToolEnv>, input: Value, _c: CancellationToken) -> ToolOutcome {
    let agent_id = required_str(&input, "agentId");
    let mode_id = required_str(&input, "modeId");
    env.core.set_agent_mode(agent_id, mode_id).await?;
    ok_structured(json!({"success": true, "newMode": mode_id}))
}

async fn list_pending_permissions(
    env: Arc<ToolEnv>,
    _input: Value,
    _c: CancellationToken,
) -> ToolOutcome {
    let permissions: Vec<Value> = env
        .core
        .list_agent_permissions()
        .into_iter()
        .flat_map(|agent| {
            let agent_id = agent.agent_id;
            let status = agent.status;
            agent.requests.into_iter().map(
                move |request| json!({"agentId": agent_id, "status": status, "request": request}),
            )
        })
        .collect();
    ok_structured(json!({"permissions": permissions}))
}

async fn respond_to_permission(
    env: Arc<ToolEnv>,
    input: Value,
    _c: CancellationToken,
) -> ToolOutcome {
    let agent_id = required_str(&input, "agentId");
    let request_id = required_str(&input, "requestId");
    let response = input.get("response").cloned().unwrap_or(Value::Null);
    env.core
        .respond_to_permission(agent_id, request_id, &response)
        .await?;
    success()
}
