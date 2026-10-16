//! Schedule and heartbeat tools.
//!
//! COMPAT(scheduleEveryInput): `update_schedule` still accepts the old hidden `every` field and
//! canonicalizes it to a five-field cron expression. Added in v0.2.0; remove after 2027-01-17.

use std::sync::Arc;

use paseo_schedule::cadence::every_ms_to_five_field_cron;
use serde_json::{json, Map, Value};
use tokio_util::sync::CancellationToken;

use super::{
    bool_arg, handler, ok_structured, required_str, str_arg, success, ToolEnv, ToolOutcome,
};
use crate::catalog::Registrar;
use crate::core_api::{AgentInfo, Capability};
use crate::error::ToolError;
use crate::util::{
    expand_user_path, iso_after, js_trim, parse_duration_string,
    resolve_schedule_provider_and_model,
};

pub(crate) fn register(r: &mut Registrar) {
    r.register("create_schedule", handler(create_schedule));
    r.register("create_heartbeat", handler(create_heartbeat));
    r.register("delete_heartbeat", handler(delete_heartbeat));
    r.register("list_schedules", handler(list_schedules));
    r.register("inspect_schedule", handler(inspect_schedule));
    r.register("pause_schedule", handler(pause_schedule));
    r.register("resume_schedule", handler(resume_schedule));
    r.register("delete_schedule", handler(delete_schedule));
    r.register("update_schedule", handler(update_schedule));
    r.register("schedule_logs", handler(schedule_logs));
    r.register("run_schedule_once", handler(run_schedule_once));
}

fn require_service(env: &ToolEnv) -> Result<(), ToolError> {
    if env.core.has(Capability::Schedules) {
        Ok(())
    } else {
        Err(ToolError::new("Schedule service is not configured"))
    }
}

/// `toScheduleSummary`: a stored schedule without its runs.
fn schedule_summary(schedule: &Value) -> Value {
    match schedule {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(k, _)| k.as_str() != "runs")
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn target_type(schedule: &Value) -> Option<&str> {
    schedule.get("target")?.get("type")?.as_str()
}

/// `requireScheduleTarget`.
async fn require_schedule_target(env: &ToolEnv, id: &str, kind: &str) -> Result<Value, ToolError> {
    require_service(env)?;
    let schedule = env.core.schedule_inspect(id).await?;
    if target_type(&schedule) != Some(kind) {
        return Err(ToolError::new(if kind == "agent" {
            format!("Heartbeat not found: {id}")
        } else {
            format!("Schedule not found: {id}")
        }));
    }
    Ok(schedule)
}

/// `requireCallerHeartbeat`.
async fn require_caller_heartbeat(env: &ToolEnv, id: &str) -> Result<(), ToolError> {
    let Some(caller) = &env.caller_agent_id else {
        return Err(ToolError::new(
            "Heartbeat operations require an agent-scoped session",
        ));
    };
    let schedule = require_schedule_target(env, id, "agent").await?;
    let owner = schedule
        .get("target")
        .and_then(|t| t.get("agentId"))
        .and_then(Value::as_str);
    if owner != Some(caller.as_str()) {
        return Err(ToolError::new(format!(
            "Heartbeat {id} does not belong to caller {caller}"
        )));
    }
    Ok(())
}

fn normalize_cadence_arg(value: Option<&str>) -> Option<String> {
    value
        .map(js_trim)
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
}

/// `buildCronScheduleCadence`.
fn build_cron_cadence(cron: Option<&str>, timezone: Option<&str>) -> Result<Value, ToolError> {
    let expression = cron.map(js_trim).unwrap_or("");
    if expression.is_empty() {
        return Err(ToolError::new("cron is required"));
    }
    let mut map = Map::new();
    map.insert("type".into(), json!("cron"));
    map.insert("expression".into(), json!(expression));
    if let Some(tz) = normalize_cadence_arg(timezone) {
        map.insert("timezone".into(), json!(tz));
    }
    Ok(Value::Object(map))
}

/// `buildScheduleExpiry`.
fn build_expiry(env: &ToolEnv, expires_in: Option<&str>) -> Result<Option<String>, ToolError> {
    expires_in
        .map(|d| iso_after(env.core.now_ms(), parse_duration_string(d)?))
        .transpose()
}

fn truthy(value: &Option<String>) -> Option<&str> {
    value.as_deref().filter(|v| !v.is_empty())
}

/// `buildCallerAgentScheduleConfigExtras`.
fn caller_config_extras(
    caller: &AgentInfo,
    resolved_provider: &str,
    config: &mut Map<String, Value>,
) {
    if let Some(thinking) = truthy(&caller.config.thinking_option_id) {
        config.insert("thinkingOptionId".into(), json!(thinking));
    }
    if caller.provider == resolved_provider {
        if let Some(options) = caller
            .config
            .provider_options
            .as_ref()
            .filter(|v| !v.is_null())
        {
            config.insert("providerOptions".into(), options.clone());
        }
    }
    if let Some(values) = caller
        .config
        .feature_values
        .as_ref()
        .filter(|v| !v.is_null())
    {
        config.insert("featureValues".into(), values.clone());
    }
    if let Some(prompt) = truthy(&caller.config.system_prompt) {
        config.insert("systemPrompt".into(), json!(prompt));
    }
    if let Some(servers) = caller.config.mcp_servers.as_ref().filter(|v| !v.is_null()) {
        config.insert("mcpServers".into(), servers.clone());
    }
}

/// `buildCallerAgentScheduleConfig`.
fn caller_schedule_config(
    caller: &AgentInfo,
    provider: Option<&str>,
    cwd: Option<&str>,
    process_cwd: &str,
) -> Result<Map<String, Value>, ToolError> {
    let has_override = provider.is_some();
    let resolved = if has_override {
        Some(resolve_schedule_provider_and_model(
            provider,
            &caller.provider,
        )?)
    } else {
        None
    };
    let resolved_provider = resolved
        .as_ref()
        .map_or_else(|| caller.provider.clone(), |(p, _)| p.clone());
    let resolved_model: Option<String> = match &resolved {
        Some((_, Some(model))) if !model.is_empty() => Some(model.clone()),
        _ if !has_override => truthy(&caller.config.model).map(str::to_owned),
        _ => None,
    };
    let mut config = Map::new();
    config.insert("provider".into(), json!(resolved_provider));
    let resolved_cwd = match cwd.map(js_trim).filter(|c| !c.is_empty()) {
        Some(_) => expand_user_path(cwd.unwrap_or_default(), process_cwd),
        None => caller.cwd.clone(),
    };
    config.insert("cwd".into(), json!(resolved_cwd));
    if let Some(mode) = truthy(&caller.current_mode_id) {
        if caller.provider == resolved_provider {
            config.insert("modeId".into(), json!(mode));
        }
    }
    if let Some(model) = resolved_model {
        config.insert("model".into(), json!(model));
    }
    caller_config_extras(caller, &resolved_provider, &mut config);
    Ok(config)
}

/// `resolveNewAgentScheduleTarget`.
fn new_agent_target(
    env: &ToolEnv,
    provider: Option<&str>,
    cwd: Option<&str>,
    isolation: Option<&str>,
) -> Result<Value, ToolError> {
    if let Some(caller) = env.resolve_caller_agent()? {
        let mut config = caller_schedule_config(&caller, provider, cwd, &env.core.process_cwd())?;
        if let Some(isolation) = isolation {
            config.insert("isolation".into(), json!(isolation));
        }
        return Ok(json!({"type": "new-agent", "config": config}));
    }
    let Some(provider_input) = provider.map(js_trim).filter(|p| !p.is_empty()) else {
        return Err(ToolError::new(
            "provider is required when target is new-agent",
        ));
    };
    let (resolved_provider, model) = resolve_schedule_provider_and_model(provider, provider_input)?;
    let mut config = Map::new();
    config.insert("provider".into(), json!(resolved_provider));
    let resolved_cwd = match cwd.map(js_trim).filter(|c| !c.is_empty()) {
        Some(_) => expand_user_path(cwd.unwrap_or_default(), &env.core.process_cwd()),
        None => env.core.process_cwd(),
    };
    config.insert("cwd".into(), json!(resolved_cwd));
    if let Some(model) = model.filter(|m| !m.is_empty()) {
        config.insert("model".into(), json!(model));
    }
    if let Some(isolation) = isolation {
        config.insert("isolation".into(), json!(isolation));
    }
    Ok(json!({"type": "new-agent", "config": config}))
}

fn insert_optional_tail(map: &mut Map<String, Value>, input: &Value, expires_at: Option<String>) {
    if let Some(name) = str_arg(input, "name")
        .map(js_trim)
        .filter(|n| !n.is_empty())
    {
        map.insert("name".into(), json!(name));
    }
    if let Some(max_runs) = input.get("maxRuns") {
        map.insert("maxRuns".into(), max_runs.clone());
    }
    if let Some(expires_at) = expires_at {
        map.insert("expiresAt".into(), json!(expires_at));
    }
}

async fn create_schedule(env: Arc<ToolEnv>, input: Value, _c: CancellationToken) -> ToolOutcome {
    require_service(&env)?;
    let expires_at = build_expiry(&env, str_arg(&input, "expiresIn"))?;
    let cadence = build_cron_cadence(str_arg(&input, "cron"), str_arg(&input, "timezone"))?;
    let target = new_agent_target(
        &env,
        str_arg(&input, "provider"),
        str_arg(&input, "cwd"),
        str_arg(&input, "isolation"),
    )?;
    let mut request = Map::new();
    request.insert(
        "prompt".into(),
        json!(js_trim(required_str(&input, "prompt"))),
    );
    request.insert("cadence".into(), cadence);
    request.insert("target".into(), target);
    insert_optional_tail(&mut request, &input, expires_at);
    let schedule = env
        .core
        .schedule_create_or_replace(Value::Object(request))
        .await?;
    ok_structured(schedule_summary(&schedule))
}

async fn create_heartbeat(env: Arc<ToolEnv>, input: Value, _c: CancellationToken) -> ToolOutcome {
    require_service(&env)?;
    let Some(caller_id) = env.caller_agent_id.clone() else {
        return Err(ToolError::new(
            "create_heartbeat requires an agent-scoped session",
        ));
    };
    env.resolve_caller_agent()?;
    let expires_at = build_expiry(&env, str_arg(&input, "expiresIn"))?;
    let cadence = build_cron_cadence(str_arg(&input, "cron"), str_arg(&input, "timezone"))?;
    let mut request = Map::new();
    request.insert(
        "prompt".into(),
        json!(js_trim(required_str(&input, "prompt"))),
    );
    request.insert("cadence".into(), cadence);
    request.insert(
        "target".into(),
        json!({"type": "agent", "agentId": caller_id}),
    );
    insert_optional_tail(&mut request, &input, expires_at);
    let schedule = env
        .core
        .schedule_create_or_replace(Value::Object(request))
        .await?;
    ok_structured(schedule_summary(&schedule))
}

async fn delete_heartbeat(env: Arc<ToolEnv>, input: Value, _c: CancellationToken) -> ToolOutcome {
    require_service(&env)?;
    let id = required_str(&input, "id");
    require_caller_heartbeat(&env, id).await?;
    env.core.schedule_delete(id).await?;
    success()
}

async fn list_schedules(env: Arc<ToolEnv>, _input: Value, _c: CancellationToken) -> ToolOutcome {
    require_service(&env)?;
    let schedules: Vec<Value> = env
        .core
        .schedule_list()
        .await?
        .iter()
        .filter(|s| target_type(s) == Some("new-agent"))
        .map(schedule_summary)
        .collect();
    ok_structured(json!({"schedules": schedules}))
}

async fn inspect_schedule(env: Arc<ToolEnv>, input: Value, _c: CancellationToken) -> ToolOutcome {
    require_service(&env)?;
    let schedule = require_schedule_target(&env, required_str(&input, "id"), "new-agent").await?;
    ok_structured(schedule)
}

async fn pause_schedule(env: Arc<ToolEnv>, input: Value, _c: CancellationToken) -> ToolOutcome {
    require_service(&env)?;
    let id = required_str(&input, "id");
    require_schedule_target(&env, id, "new-agent").await?;
    env.core.schedule_pause(id).await?;
    success()
}

async fn resume_schedule(env: Arc<ToolEnv>, input: Value, _c: CancellationToken) -> ToolOutcome {
    require_service(&env)?;
    let id = required_str(&input, "id");
    require_schedule_target(&env, id, "new-agent").await?;
    env.core.schedule_resume(id).await?;
    success()
}

async fn delete_schedule(env: Arc<ToolEnv>, input: Value, _c: CancellationToken) -> ToolOutcome {
    require_service(&env)?;
    let id = required_str(&input, "id");
    require_schedule_target(&env, id, "new-agent").await?;
    env.core.schedule_delete(id).await?;
    success()
}

async fn schedule_logs(env: Arc<ToolEnv>, input: Value, _c: CancellationToken) -> ToolOutcome {
    require_service(&env)?;
    let id = required_str(&input, "id");
    require_schedule_target(&env, id, "new-agent").await?;
    let runs = env.core.schedule_logs(id).await?;
    ok_structured(json!({"runs": runs}))
}

async fn run_schedule_once(env: Arc<ToolEnv>, input: Value, _c: CancellationToken) -> ToolOutcome {
    require_service(&env)?;
    let id = required_str(&input, "id");
    require_schedule_target(&env, id, "new-agent").await?;
    let schedule = env.core.schedule_run_once(id).await?;
    ok_structured(schedule)
}

async fn update_schedule(env: Arc<ToolEnv>, input: Value, _c: CancellationToken) -> ToolOutcome {
    require_service(&env)?;
    let id = required_str(&input, "id");
    require_schedule_target(&env, id, "new-agent").await?;
    let update = build_schedule_update_input(&env, &input)?;
    let schedule = env.core.schedule_update(update).await?;
    ok_structured(schedule)
}

/// `normalizeScheduleCadenceArg` on a passthrough value (`every` is not in the schema, so it can
/// be any JSON type; `value.trim` on a non-string throws).
fn normalize_loose_cadence(value: Option<&Value>) -> Result<Option<String>, ToolError> {
    match value {
        None => Ok(None),
        Some(Value::String(s)) => Ok(normalize_cadence_arg(Some(s))),
        Some(_) => Err(ToolError::new("value.trim is not a function")),
    }
}

/// `resolveScheduleUpdateCadence`.
fn resolve_update_cadence(input: &Value) -> Result<Option<Value>, ToolError> {
    let every = normalize_loose_cadence(input.get("every"))?;
    let cron = normalize_loose_cadence(input.get("cron"))?;
    let time_zone = normalize_loose_cadence(input.get("timezone"))?;
    if every.is_some() && cron.is_some() {
        return Err(ToolError::new("Specify at most one of every or cron"));
    }
    if time_zone.is_some() && cron.is_none() {
        return Err(ToolError::new("timezone can only be used with cron"));
    }
    if let Some(every) = every {
        // COMPAT(scheduleEveryInput): see the module doc.
        let every_ms = parse_duration_string(&every)?;
        let expression = if every_ms.fract() == 0.0 && every_ms > 0.0 && every_ms < u64::MAX as f64
        {
            every_ms_to_five_field_cron(every_ms as u64)
        } else {
            None
        };
        return match expression {
            Some(expression) => Ok(Some(json!({"type": "cron", "expression": expression}))),
            None => Err(ToolError::new(format!(
                "{every} cannot be represented faithfully by five-field cron"
            ))),
        };
    }
    if let Some(cron) = cron {
        let mut map = Map::new();
        map.insert("type".into(), json!("cron"));
        map.insert("expression".into(), json!(cron));
        if let Some(tz) = time_zone {
            map.insert("timezone".into(), json!(tz));
        }
        return Ok(Some(Value::Object(map)));
    }
    Ok(None)
}

/// `resolveScheduleUpdateExpiresAt`: `Some(None)` is `null` (clear), `None` is `undefined`.
fn resolve_update_expires_at(
    env: &ToolEnv,
    input: &Value,
) -> Result<Option<Option<String>>, ToolError> {
    let expires_in = str_arg(input, "expiresIn");
    let clear = bool_arg(input, "clearExpires").unwrap_or(false);
    if expires_in.is_some() && clear {
        return Err(ToolError::new(
            "Specify at most one of expiresIn or clearExpires",
        ));
    }
    if let Some(duration) = expires_in {
        return Ok(Some(Some(iso_after(
            env.core.now_ms(),
            parse_duration_string(duration)?,
        )?)));
    }
    Ok(clear.then_some(None))
}

/// `resolveScheduleUpdateProviderAndModel`: `(provider, model)` where the model is `None`
/// (absent), `Some(Null)` (clear) or a string.
fn resolve_update_provider_and_model(
    input: &Value,
) -> Result<(Option<String>, Option<Value>), ToolError> {
    let provider_input = str_arg(input, "provider").map(js_trim);
    let model_value = input.get("model");
    let model_input: Option<Value> = model_value.map(|m| match m {
        Value::String(s) => json!(js_trim(s)),
        other => other.clone(),
    });
    if model_input.as_ref().and_then(Value::as_str) == Some("") {
        return Err(ToolError::new("model cannot be empty"));
    }
    let Some(provider_input) = provider_input.filter(|p| !p.is_empty()) else {
        return Ok((None, model_input));
    };
    let Some(slash) = provider_input.find('/') else {
        return Ok((Some(provider_input.to_owned()), model_input));
    };
    let provider = js_trim(&provider_input[..slash]);
    let model_from_provider = js_trim(&provider_input[slash + 1..]);
    if provider.is_empty() || model_from_provider.is_empty() {
        return Err(ToolError::new(
            "provider must be <provider> or <provider>/<model>",
        ));
    }
    if model_input.as_ref().is_some_and(Value::is_null) {
        return Err(ToolError::new(
            "provider specifies a model but model is null",
        ));
    }
    if let Some(model) = model_input.as_ref().and_then(Value::as_str) {
        if model != model_from_provider {
            return Err(ToolError::new("Conflicting model values provided"));
        }
    }
    Ok((
        Some(provider.to_owned()),
        Some(model_input.unwrap_or_else(|| json!(model_from_provider))),
    ))
}

/// `buildScheduleUpdateInput`.
fn build_schedule_update_input(env: &ToolEnv, input: &Value) -> Result<Value, ToolError> {
    let cadence = resolve_update_cadence(input)?;
    let expires_at = resolve_update_expires_at(env, input)?;
    let (provider, model) = resolve_update_provider_and_model(input)?;

    let mut new_agent_config = Map::new();
    if let Some(provider) = provider {
        new_agent_config.insert("provider".into(), json!(provider));
    }
    if let Some(model) = model {
        new_agent_config.insert("model".into(), model);
    }
    if let Some(mode) = input.get("mode") {
        new_agent_config.insert("modeId".into(), mode.clone());
    }
    if let Some(cwd) = input.get("cwd") {
        new_agent_config.insert("cwd".into(), cwd.clone());
    }

    let mut out = Map::new();
    out.insert("id".into(), json!(required_str(input, "id")));
    for key in ["name", "prompt"] {
        if let Some(value) = input.get(key) {
            out.insert(key.into(), value.clone());
        }
    }
    if let Some(cadence) = cadence {
        out.insert("cadence".into(), cadence);
    }
    if let Some(max_runs) = input.get("maxRuns") {
        out.insert("maxRuns".into(), max_runs.clone());
    }
    if let Some(expires_at) = expires_at {
        out.insert("expiresAt".into(), json!(expires_at));
    }
    if !new_agent_config.is_empty() {
        out.insert("newAgentConfig".into(), Value::Object(new_agent_config));
    }
    Ok(Value::Object(out))
}
