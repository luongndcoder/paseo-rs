//! The tools, grouped like `paseo-tools.ts` registers them.

use std::future::Future;
use std::sync::Arc;

use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

use crate::catalog::Handler;
use crate::core_api::{AgentInfo, CallerContext, CoreApi};
use crate::error::ToolError;
use crate::types::ToolResult;
use crate::util::{expand_user_path, js_trim, resolve_path_from_base};

pub(crate) mod activity;
pub(crate) mod agents;
pub(crate) mod browser;
pub(crate) mod providers;
pub(crate) mod schedules;
pub(crate) mod speak;
pub(crate) mod terminals;
pub(crate) mod workspaces;

pub(crate) type ToolOutcome = Result<ToolResult, ToolError>;

/// Per-catalog state the handlers close over: the host, the caller agent (agent-scoped
/// catalogs) and its voice caller context.
pub(crate) struct ToolEnv {
    pub core: Arc<dyn CoreApi>,
    pub caller_agent_id: Option<String>,
    pub caller_context: Option<CallerContext>,
}

impl ToolEnv {
    /// `resolveCallerAgent`: `None` outside an agent-scoped session, an error when the caller is
    /// gone.
    pub fn resolve_caller_agent(&self) -> Result<Option<AgentInfo>, ToolError> {
        let Some(id) = &self.caller_agent_id else {
            return Ok(None);
        };
        match self.core.get_agent(id) {
            Some(agent) => Ok(Some(agent)),
            None => Err(ToolError::new(format!("Parent agent {id} not found"))),
        }
    }

    /// `resolveScopedCwd`.
    pub fn resolve_scoped_cwd(
        &self,
        requested_cwd: Option<&str>,
        required: bool,
    ) -> Result<String, ToolError> {
        if let Some(caller) = self.resolve_caller_agent()? {
            return Ok(resolve_child_agent_cwd(
                &caller.cwd,
                requested_cwd,
                self.caller_context
                    .as_ref()
                    .and_then(|c| c.locked_cwd.as_deref()),
                self.caller_context
                    .as_ref()
                    .and_then(|c| c.allow_custom_cwd)
                    .unwrap_or(true),
                &self.core.process_cwd(),
            ));
        }
        let trimmed = requested_cwd.map(js_trim).filter(|c| !c.is_empty());
        match trimmed {
            Some(cwd) => Ok(expand_user_path(cwd, &self.core.process_cwd())),
            None if required => Err(ToolError::new("cwd is required")),
            None => Err(ToolError::new(
                "cwd is required outside an agent-scoped session",
            )),
        }
    }
}

/// `resolveChildAgentCwd`: locked cwd, else the requested cwd resolved against the parent's,
/// else the parent's.
pub(crate) fn resolve_child_agent_cwd(
    parent_cwd: &str,
    requested_cwd: Option<&str>,
    locked_cwd: Option<&str>,
    allow_custom_cwd: bool,
    process_cwd: &str,
) -> String {
    if let Some(locked) = locked_cwd.map(js_trim).filter(|c| !c.is_empty()) {
        return expand_user_path(locked, process_cwd);
    }
    match requested_cwd.map(js_trim).filter(|c| !c.is_empty()) {
        Some(requested) if allow_custom_cwd => {
            resolve_path_from_base(parent_cwd, requested, process_cwd)
        }
        _ => parent_cwd.to_owned(),
    }
}

/// Wraps an async handler function into the catalog's handler type.
pub(crate) fn handler<F, Fut>(f: F) -> Handler
where
    F: Fn(Arc<ToolEnv>, Value, CancellationToken) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ToolOutcome> + Send + 'static,
{
    Arc::new(move |env, input, cancel| Box::pin(f(env, input, cancel)))
}

// ---- input accessors (parsed input is an object with defaults applied) ---------------------

pub(crate) fn str_arg<'a>(input: &'a Value, key: &str) -> Option<&'a str> {
    input.get(key).and_then(Value::as_str)
}

pub(crate) fn bool_arg(input: &Value, key: &str) -> Option<bool> {
    input.get(key).and_then(Value::as_bool)
}

pub(crate) fn obj_arg<'a>(input: &'a Value, key: &str) -> Option<&'a Map<String, Value>> {
    input.get(key).and_then(Value::as_object)
}

pub(crate) fn required_str<'a>(input: &'a Value, key: &str) -> &'a str {
    str_arg(input, key).unwrap_or_default()
}

pub(crate) fn ok_structured(value: Value) -> ToolOutcome {
    Ok(ToolResult::structured(value))
}

pub(crate) fn success() -> ToolOutcome {
    ok_structured(serde_json::json!({"success": true}))
}
