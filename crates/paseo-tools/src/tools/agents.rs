//! Agent tools: create_agent, send_agent_prompt, get_agent_status, list_agents, cancel_agent,
//! archive_agent, kill_agent, update_agent.
//!
//! COMPAT(nestedCreateAgentPlacement): `create_agent` still accepts the old `relationship` /
//! `workspace` shape (and the flat legacy top-level fields) without advertising it to models.
//! Added in v0.2.0; remove after 2027-01-17.

use std::cmp::Ordering;
use std::sync::{Arc, Mutex};

use paseo_store::time::parse_js_date;
use serde_json::{json, Map, Value};
use tokio_util::sync::CancellationToken;

use super::{
    bool_arg, handler, obj_arg, ok_structured, required_str, str_arg, success, ToolEnv, ToolOutcome,
};
use crate::catalog::Registrar;
use crate::core_api::{
    Capability, CreateAgentRequest, FinishNotificationRequest, PromptDisposition,
    SendPromptRequest, StopNotification, WorktreeSpec,
};
use crate::error::ToolError;
use crate::schemas;
use crate::util::{
    expand_user_path, is_same_or_descendant_path, js_trim, resolve_required_provider_model,
};
use crate::zod::{issues_message, Obj, Z};

const PROMPTED_AGENT_NOTIFICATION_GUIDANCE: &str = "You will get notified when the prompted agent finishes, errors, or needs permission. Do not poll for status; continue with other work until the notification arrives.";
const CREATED_AGENT_NOTIFICATION_GUIDANCE: &str = "You will get notified when the created agent finishes, errors, or needs permission. Do not poll for status; continue with other work until the notification arrives.";
const NO_NOTIFICATION_GUIDANCE: &str = "Dispatch acknowledged. Observe completion using get_agent_status/get_agent_activity or SDK/RPC subscriptions and wait APIs.";

pub(crate) fn register(r: &mut Registrar) {
    r.register("create_agent", handler(create_agent));
    r.register("send_agent_prompt", handler(send_agent_prompt));
    r.register("get_agent_status", handler(get_agent_status));
    r.register("list_agents", handler(list_agents));
    r.register("cancel_agent", handler(cancel_agent));
    r.register("archive_agent", handler(archive_agent));
    r.register("kill_agent", handler(kill_agent));
    r.register("update_agent", handler(update_agent));
}

// ---- create_agent --------------------------------------------------------------------------

fn parse_strict(schema: &Obj, input: &Value) -> Result<Value, ToolError> {
    let (value, issues) = Z::Obj(schema.clone()).parse(Some(input), &[]);
    if issues.is_empty() {
        Ok(value.unwrap_or(Value::Null))
    } else {
        Err(ToolError::new(issues_message(&issues)))
    }
}

/// `hasLegacyCreateAgentPlacement`.
fn has_legacy_placement(args: &Value) -> bool {
    let Some(map) = args.as_object() else {
        return false;
    };
    [
        "relationship",
        "workspace",
        "cwd",
        "worktreeName",
        "branchName",
        "baseBranch",
        "refName",
        "githubPrNumber",
    ]
    .iter()
    .any(|k| map.contains_key(*k))
}

struct ResolvedWorkspace {
    cwd: Option<String>,
    workspace_id: Option<String>,
    worktree: Option<WorktreeSpec>,
}

struct ResolvedCreateArgs {
    parsed: Value,
    detached: bool,
    workspace: ResolvedWorkspace,
}

fn first_agent_context(prompt: &str) -> Value {
    json!({"prompt": prompt})
}

fn truthy(value: Option<&str>) -> Option<&str> {
    value.filter(|v| !v.is_empty())
}

/// `resolveCreateAgentWorktree`.
fn resolve_create_agent_worktree(target: &Value) -> WorktreeSpec {
    match str_arg(target, "kind") {
        Some("branch-off") => WorktreeSpec {
            action: Some("branch-off".into()),
            worktree_name: str_arg(target, "worktreeSlug").map(str::to_owned),
            branch_name: str_arg(target, "branchName").map(str::to_owned),
            base_branch: str_arg(target, "baseBranch").map(str::to_owned),
            ..WorktreeSpec::default()
        },
        Some("checkout-branch") => WorktreeSpec {
            action: Some("checkout".into()),
            ref_name: str_arg(target, "branch").map(str::to_owned),
            ..WorktreeSpec::default()
        },
        _ => WorktreeSpec {
            action: Some("checkout".into()),
            github_pr_number: target.get("githubPrNumber").and_then(Value::as_u64),
            ..WorktreeSpec::default()
        },
    }
}

/// `resolveCreateAgentWorkspace`.
async fn resolve_create_agent_workspace(
    env: &ToolEnv,
    workspace: &Value,
    context: Option<Value>,
) -> Result<ResolvedWorkspace, ToolError> {
    match str_arg(workspace, "kind") {
        Some("current") => {
            let Some(caller_id) = &env.caller_agent_id else {
                return Err(ToolError::new(
                    "workspace current requires an agent-scoped tool session",
                ));
            };
            let caller = env.resolve_caller_agent()?;
            let Some(workspace_id) = caller.and_then(|c| c.workspace_id) else {
                return Err(ToolError::new(format!(
                    "Caller agent {caller_id} has no current workspace"
                )));
            };
            Ok(ResolvedWorkspace {
                cwd: str_arg(workspace, "cwd").map(str::to_owned),
                workspace_id: Some(workspace_id),
                worktree: None,
            })
        }
        Some("existing") => {
            if !env.core.has(Capability::ActiveWorkspaces) {
                return Err(ToolError::new("Workspace lookup is not configured"));
            }
            let id = required_str(workspace, "workspaceId");
            let active = env.core.list_active_workspaces().await?;
            let Some(existing) = active.iter().find(|w| w.workspace_id == id) else {
                return Err(ToolError::new(format!("Workspace {id} not found")));
            };
            let cwd = match truthy(str_arg(workspace, "cwd")) {
                Some(requested) => env.resolve_scoped_cwd(Some(requested), true)?,
                None => existing.cwd.clone(),
            };
            let locked = env
                .caller_context
                .as_ref()
                .and_then(|c| c.locked_cwd.as_deref())
                .map(js_trim)
                .filter(|l| !l.is_empty());
            if let Some(locked) = locked {
                if !is_same_or_descendant_path(
                    &expand_user_path(locked, &env.core.process_cwd()),
                    &cwd,
                ) {
                    return Err(ToolError::new(format!(
                        "Workspace {id} is outside the allowed cwd"
                    )));
                }
            }
            Ok(ResolvedWorkspace {
                cwd: Some(cwd),
                workspace_id: Some(id.to_owned()),
                worktree: None,
            })
        }
        _ => {
            let source = workspace.get("source").cloned().unwrap_or(Value::Null);
            if str_arg(&source, "kind") == Some("directory") {
                let cwd = env.resolve_scoped_cwd(str_arg(&source, "path"), true)?;
                if !env.core.has(Capability::WorkspaceMinting) {
                    return Err(ToolError::new("Workspace creation is not configured"));
                }
                let caller_workspace = env.resolve_caller_agent()?.and_then(|a| a.workspace_id);
                let workspace_id = env
                    .core
                    .ensure_workspace_for_create(&cwd, context, caller_workspace.as_deref())
                    .await?;
                return Ok(ResolvedWorkspace {
                    cwd: Some(cwd),
                    workspace_id: Some(workspace_id),
                    worktree: None,
                });
            }
            let cwd = env.resolve_scoped_cwd(str_arg(&source, "cwd"), true)?;
            let target = source.get("target").cloned().unwrap_or(Value::Null);
            Ok(ResolvedWorkspace {
                cwd: Some(cwd),
                workspace_id: None,
                worktree: Some(resolve_create_agent_worktree(&target)),
            })
        }
    }
}

/// `resolveCanonicalCreateAgentWorkspace`.
async fn resolve_canonical_create_agent_workspace(
    env: &ToolEnv,
    workspace_id: Option<&str>,
    context: Option<Value>,
) -> Result<ResolvedWorkspace, ToolError> {
    if let Some(id) = truthy(workspace_id) {
        let resolved = resolve_create_agent_workspace(
            env,
            &json!({"kind": "existing", "workspaceId": id}),
            None,
        )
        .await?;
        return Ok(ResolvedWorkspace {
            cwd: resolved.cwd,
            workspace_id: Some(id.to_owned()),
            worktree: None,
        });
    }
    let Some(caller_id) = &env.caller_agent_id else {
        if !env.core.has(Capability::WorkspaceMinting) {
            return Err(ToolError::new("Workspace creation is not configured"));
        }
        let cwd = env.core.process_cwd();
        let id = env
            .core
            .ensure_workspace_for_create(&cwd, context, None)
            .await?;
        return Ok(ResolvedWorkspace {
            cwd: Some(cwd),
            workspace_id: Some(id),
            worktree: None,
        });
    };
    let caller = env.resolve_caller_agent()?;
    match caller.and_then(|c| c.workspace_id) {
        Some(id) => Ok(ResolvedWorkspace {
            cwd: None,
            workspace_id: Some(id),
            worktree: None,
        }),
        None => Err(ToolError::new(format!(
            "Caller agent {caller_id} has no current workspace"
        ))),
    }
}

/// `resolveLegacyCreateAgentWorktreeTarget`.
fn legacy_worktree_target(args: &Value) -> Option<Value> {
    if let Some(number) = args.get("githubPrNumber") {
        return Some(json!({"kind": "checkout-pr", "githubPrNumber": number}));
    }
    if let Some(ref_name) = truthy(str_arg(args, "refName")) {
        return Some(json!({"kind": "checkout-branch", "branch": ref_name}));
    }
    let worktree_name = truthy(str_arg(args, "worktreeName"));
    let branch_name = truthy(str_arg(args, "branchName"));
    let base_branch = truthy(str_arg(args, "baseBranch"));
    if worktree_name.is_some() || branch_name.is_some() || base_branch.is_some() {
        let mut target = Map::new();
        target.insert("kind".into(), json!("branch-off"));
        for (key, value) in [
            ("worktreeSlug", worktree_name),
            ("branchName", branch_name),
            ("baseBranch", base_branch),
        ] {
            if let Some(value) = value {
                target.insert(key.into(), json!(value));
            }
        }
        return Some(Value::Object(target));
    }
    None
}

/// `normalizeTopLevelCreateAgentArgs`.
fn normalize_top_level_args(args: &Value) -> Result<Value, ToolError> {
    let schema = schemas::legacy_top_level_args();
    let Some(map) = args.as_object() else {
        return Ok(args.clone());
    };
    const LEGACY: [&str; 8] = [
        "cwd",
        "mode",
        "thinking",
        "features",
        "worktreeName",
        "branchName",
        "baseBranch",
        "refName",
    ];
    let mut candidate: Map<String, Value> = map
        .iter()
        .filter(|(k, _)| !LEGACY.contains(&k.as_str()) && k.as_str() != "githubPrNumber")
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let mut settings: Map<String, Value> = candidate
        .get("settings")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    if let Some(mode) = truthy(str_arg(args, "mode")) {
        settings.insert("modeId".into(), json!(mode));
    }
    if let Some(thinking) = truthy(str_arg(args, "thinking")) {
        settings.insert("thinkingOptionId".into(), json!(thinking));
    }
    if let Some(features) = args.get("features") {
        settings.insert("features".into(), features.clone());
    }
    let has_relationship = candidate.contains_key("relationship");
    let has_workspace = candidate.contains_key("workspace");

    if has_relationship && has_workspace {
        if !settings.is_empty() {
            candidate.insert("settings".into(), Value::Object(settings));
        }
        return parse_strict(&schema, &Value::Object(candidate));
    }
    if has_relationship || has_workspace {
        return Err(ToolError::new(
            "relationship and workspace must be provided together",
        ));
    }
    let cwd = str_arg(args, "cwd").map(js_trim).filter(|c| !c.is_empty());
    let Some(_) = cwd else {
        return Err(ToolError::new(
            "cwd is required for legacy top-level create_agent calls",
        ));
    };
    let cwd = str_arg(args, "cwd").unwrap_or_default();
    let workspace = match legacy_worktree_target(args) {
        Some(target) => json!({
            "kind": "create",
            "source": {"kind": "worktree", "cwd": cwd, "target": target},
        }),
        None => json!({"kind": "create", "source": {"kind": "directory", "path": cwd}}),
    };
    candidate.insert("relationship".into(), json!({"kind": "detached"}));
    candidate.insert("workspace".into(), workspace);
    if !settings.is_empty() {
        candidate.insert("settings".into(), Value::Object(settings));
    }
    parse_strict(&schema, &Value::Object(candidate))
}

/// `resolveCreateAgentToolArgs`.
async fn resolve_create_agent_args(
    env: &ToolEnv,
    args: &Value,
) -> Result<ResolvedCreateArgs, ToolError> {
    let caller = env.caller_agent_id.is_some();
    let legacy = has_legacy_placement(args);
    if caller {
        if legacy {
            let parsed = parse_strict(&schemas::legacy_agent_to_agent_args(), args)?;
            let context = first_agent_context(required_str(&parsed, "initialPrompt"));
            let workspace = resolve_create_agent_workspace(
                env,
                parsed.get("workspace").unwrap_or(&Value::Null),
                Some(context),
            )
            .await?;
            let detached =
                parsed.get("relationship").and_then(|r| str_arg(r, "kind")) == Some("detached");
            return Ok(ResolvedCreateArgs {
                parsed,
                detached,
                workspace,
            });
        }
        let parsed = parse_strict(&schemas::create_agent_args_strict(true), args)?;
        let context = first_agent_context(required_str(&parsed, "initialPrompt"));
        let workspace = resolve_canonical_create_agent_workspace(
            env,
            str_arg(&parsed, "workspaceId"),
            Some(context),
        )
        .await?;
        return Ok(ResolvedCreateArgs {
            parsed,
            detached: false,
            workspace,
        });
    }
    if legacy {
        let parsed =
            normalize_top_level_args(&parse_strict(&schemas::legacy_top_level_args(), args)?)?;
        if parsed.get("relationship").and_then(|r| str_arg(r, "kind")) == Some("subagent") {
            return Err(ToolError::new(
                "relationship subagent requires an agent-scoped tool session",
            ));
        }
        let Some(workspace) = parsed.get("workspace") else {
            return Err(ToolError::new(
                "Legacy create_agent placement could not be resolved",
            ));
        };
        let context = first_agent_context(required_str(&parsed, "initialPrompt"));
        let workspace = resolve_create_agent_workspace(env, workspace, Some(context)).await?;
        return Ok(ResolvedCreateArgs {
            parsed,
            detached: true,
            workspace,
        });
    }
    let parsed = parse_strict(&schemas::create_agent_args_strict(false), args)?;
    let context = first_agent_context(required_str(&parsed, "initialPrompt"));
    let workspace = resolve_canonical_create_agent_workspace(
        env,
        str_arg(&parsed, "workspaceId"),
        Some(context),
    )
    .await?;
    Ok(ResolvedCreateArgs {
        parsed,
        detached: false,
        workspace,
    })
}

async fn create_agent(env: Arc<ToolEnv>, input: Value, _c: CancellationToken) -> ToolOutcome {
    let resolved = resolve_create_agent_args(&env, &input).await?;
    let parsed = &resolved.parsed;
    let notify_on_finish = bool_arg(parsed, "notifyOnFinish").unwrap_or(false);
    let provider = required_str(parsed, "provider");
    let (selected_provider, _) = resolve_required_provider_model(provider)?;
    // `resolveInheritedProviderConfig`: provider options only pass between agents of one provider.
    let inherited_options = env.resolve_caller_agent()?.and_then(|caller| {
        (caller.provider == selected_provider)
            .then_some(caller.config.provider_options)
            .flatten()
            .filter(|o| !o.is_null())
    });
    let settings = obj_arg(parsed, "settings");
    let setting_str = |key: &str| {
        settings
            .and_then(|s| s.get(key))
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    let request = CreateAgentRequest {
        provider: provider.to_owned(),
        title: required_str(parsed, "title").to_owned(),
        initial_prompt: required_str(parsed, "initialPrompt").to_owned(),
        provider_options: inherited_options,
        cwd: resolved.workspace.cwd.clone(),
        workspace_id: resolved.workspace.workspace_id.clone(),
        thinking: setting_str("thinkingOptionId"),
        features: settings
            .and_then(|s| s.get("features"))
            .and_then(Value::as_object)
            .cloned(),
        labels: obj_arg(parsed, "labels").cloned(),
        mode: setting_str("modeId"),
        notify_on_finish,
        detached: resolved.detached,
        caller_agent_id: env.caller_agent_id.clone(),
        caller_context: env.caller_context.clone(),
        worktree: resolved.workspace.worktree.clone(),
    };
    let created = env.core.create_agent(request).await?;
    let snapshot = created.snapshot;
    // Return immediately for async creation.
    let current = env
        .core
        .get_agent(&snapshot.id)
        .unwrap_or_else(|| snapshot.clone());
    let guidance = if env.caller_agent_id.is_some()
        && notify_on_finish
        && created.initial_prompt_started
    {
        CREATED_AGENT_NOTIFICATION_GUIDANCE
    } else {
        "Creation acknowledged. Observe completion using get_agent_status/get_agent_activity or SDK/RPC subscriptions and wait APIs."
    };
    let mut out = Map::new();
    out.insert("agentId".into(), json!(current.id));
    out.insert("type".into(), json!(snapshot.provider));
    out.insert("status".into(), json!(current.lifecycle));
    out.insert("cwd".into(), json!(current.cwd));
    if let Some(workspace_id) = &current.workspace_id {
        out.insert("workspaceId".into(), json!(workspace_id));
    }
    out.insert("currentModeId".into(), json!(current.current_mode_id));
    out.insert(
        "availableModes".into(),
        Value::Array(current.available_modes),
    );
    out.insert("guidance".into(), json!(guidance));
    ok_structured(Value::Object(out))
}

// ---- send_agent_prompt -----------------------------------------------------------------------

async fn send_agent_prompt(env: Arc<ToolEnv>, input: Value, _c: CancellationToken) -> ToolOutcome {
    let agent_id = required_str(&input, "agentId").to_owned();
    let prompt = required_str(&input, "prompt").to_owned();
    let caller = env.caller_agent_id.clone();
    let notify_on_finish = bool_arg(&input, "notifyOnFinish").unwrap_or(caller.is_some());
    let notifying = caller.is_some() && notify_on_finish;

    let stop_slot: Arc<Mutex<Option<StopNotification>>> = Arc::new(Mutex::new(None));
    let stop = |slot: &Arc<Mutex<Option<StopNotification>>>| {
        if let Some(stop) = slot.lock().ok().and_then(|mut s| s.take()) {
            stop();
        }
    };

    // `armFinishNotification`: runs inside the dispatch, before the run-start wait.
    let on_dispatch = {
        let core = env.core.clone();
        let slot = stop_slot.clone();
        let agent_id = agent_id.clone();
        let caller = caller.clone();
        Box::new(move |disposition: PromptDisposition| {
            let Some(caller_id) = caller else { return };
            if !notify_on_finish {
                return;
            }
            let stop = core.setup_finish_notification(FinishNotificationRequest {
                child_agent_id: agent_id,
                caller_agent_id: caller_id,
                wait_for_turn_start: disposition == PromptDisposition::TurnStarted,
            });
            if let Ok(mut slot) = slot.lock() {
                *slot = Some(stop);
            }
        })
    };
    let disposition = match env
        .core
        .send_prompt_to_agent(
            SendPromptRequest {
                agent_id: agent_id.clone(),
                prompt,
                source_agent_id: caller.clone(),
                session_mode: str_arg(&input, "sessionMode").map(str::to_owned),
            },
            on_dispatch,
        )
        .await
    {
        Ok(disposition) => disposition,
        Err(error) => {
            stop(&stop_slot);
            return Err(error);
        }
    };

    // Return once the provider has accepted the turn, so the status reports it running.
    if disposition == PromptDisposition::TurnStarted {
        if let Err(error) = env.core.wait_for_agent_run_start(&agent_id).await {
            stop(&stop_slot);
            return Err(error);
        }
    }
    let status = env
        .core
        .get_agent(&agent_id)
        .map_or_else(|| "idle".to_owned(), |a| a.lifecycle);
    ok_structured(json!({
        "success": true,
        "disposition": disposition.as_str(),
        "status": status,
        "guidance": if notifying { PROMPTED_AGENT_NOTIFICATION_GUIDANCE } else { NO_NOTIFICATION_GUIDANCE },
    }))
}

// ---- status and listing ----------------------------------------------------------------------

async fn get_agent_status(env: Arc<ToolEnv>, input: Value, _c: CancellationToken) -> ToolOutcome {
    let agent_id = required_str(&input, "agentId");
    match env.core.agent_status_snapshot(agent_id).await? {
        Some(found) => ok_structured(json!({"status": found.status, "snapshot": found.snapshot})),
        None => Err(ToolError::new(format!("Agent {agent_id} not found"))),
    }
}

fn parse_timestamp(value: Option<&Value>) -> i64 {
    value
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .and_then(parse_js_date)
        .unwrap_or(0)
}

/// `resolveAgentListActivityTime`.
fn activity_time(agent: &Value) -> i64 {
    [
        "updatedAt",
        "lastUserMessageAt",
        "attentionTimestamp",
        "archivedAt",
        "createdAt",
    ]
    .iter()
    .map(|k| parse_timestamp(agent.get(*k)))
    .max()
    .unwrap_or(0)
}

/// `toAgentListItemPayload`.
fn to_list_item(agent: &Value) -> Value {
    let field = |key: &str| agent.get(key).cloned().unwrap_or(Value::Null);
    let non_null = |key: &str, fallback: Value| match agent.get(key) {
        Some(Value::Null) | None => fallback,
        Some(v) => v.clone(),
    };
    let id = agent.get("id").and_then(Value::as_str).unwrap_or_default();
    let model = match agent.get("runtimeInfo").and_then(|r| r.get("model")) {
        Some(Value::Null) | None => field("model"),
        Some(m) => m.clone(),
    };
    let mut map = Map::new();
    map.insert("id".into(), json!(id));
    map.insert(
        "shortId".into(),
        json!(id.chars().take(7).collect::<String>()),
    );
    map.insert("title".into(), field("title"));
    map.insert("provider".into(), field("provider"));
    map.insert("model".into(), model);
    map.insert("thinkingOptionId".into(), field("thinkingOptionId"));
    map.insert(
        "effectiveThinkingOptionId".into(),
        field("effectiveThinkingOptionId"),
    );
    map.insert("status".into(), field("status"));
    map.insert("cwd".into(), field("cwd"));
    map.insert("createdAt".into(), field("createdAt"));
    map.insert("updatedAt".into(), field("updatedAt"));
    map.insert("lastUserMessageAt".into(), field("lastUserMessageAt"));
    map.insert("archivedAt".into(), non_null("archivedAt", Value::Null));
    map.insert(
        "requiresAttention".into(),
        non_null("requiresAttention", json!(false)),
    );
    map.insert(
        "attentionReason".into(),
        non_null("attentionReason", Value::Null),
    );
    map.insert(
        "attentionTimestamp".into(),
        non_null("attentionTimestamp", Value::Null),
    );
    map.insert("labels".into(), field("labels"));
    if agent.get("providerUnavailable").and_then(Value::as_bool) == Some(true) {
        map.insert("providerUnavailable".into(), json!(true));
    }
    Value::Object(map)
}

fn status_rank(status: &str) -> i32 {
    match status {
        "running" => 0,
        "initializing" => 1,
        "idle" => 2,
        "error" => 3,
        "closed" => 4,
        _ => 999,
    }
}

/// `compareAgentListItems`.
fn compare_list_items(a: &Value, b: &Value) -> Ordering {
    let attention =
        |v: &Value| i32::from(v.get("requiresAttention").and_then(Value::as_bool) == Some(true));
    let by_attention = attention(b).cmp(&attention(a));
    if by_attention != Ordering::Equal {
        return by_attention;
    }
    let rank = |v: &Value| status_rank(v.get("status").and_then(Value::as_str).unwrap_or_default());
    let by_status = rank(a).cmp(&rank(b));
    if by_status != Ordering::Equal {
        return by_status;
    }
    activity_time(b).cmp(&activity_time(a))
}

async fn list_agents(env: Arc<ToolEnv>, input: Value, _c: CancellationToken) -> ToolOutcome {
    let include_background = bool_arg(&input, "includeBackground").unwrap_or(false);
    let include_archived = bool_arg(&input, "includeArchived").unwrap_or(false);
    let since_hours = input
        .get("sinceHours")
        .and_then(Value::as_f64)
        .unwrap_or(48.0);
    let limit = input.get("limit").and_then(Value::as_f64).unwrap_or(50.0) as usize;

    let caller_cwd = if env.caller_agent_id.is_some() {
        env.resolve_caller_agent()?.map(|a| a.cwd)
    } else {
        None
    };
    let requested_cwd = match str_arg(&input, "cwd")
        .map(js_trim)
        .filter(|c| !c.is_empty())
    {
        Some(_) => Some(expand_user_path(
            str_arg(&input, "cwd").unwrap_or_default(),
            &env.core.process_cwd(),
        )),
        None => caller_cwd,
    };
    let statuses: Option<Vec<&str>> = input
        .get("statuses")
        .and_then(Value::as_array)
        .filter(|s| !s.is_empty())
        .map(|s| s.iter().filter_map(Value::as_str).collect());
    let since_ms = env.core.now_ms() - (since_hours * 3_600_000.0) as i64;
    if !env.core.has(Capability::WorkspaceRegistry) {
        return Err(ToolError::new("Workspace registry is not configured"));
    }
    let snapshots = env
        .core
        .list_agent_snapshots(include_background, include_archived)
        .await?;
    let mut agents: Vec<Value> = snapshots
        .iter()
        .map(to_list_item)
        .filter(|agent| {
            requested_cwd.as_deref().is_none_or(|cwd| {
                is_same_or_descendant_path(cwd, agent["cwd"].as_str().unwrap_or_default())
            })
        })
        .filter(|agent| {
            statuses
                .as_ref()
                .is_none_or(|s| s.contains(&agent["status"].as_str().unwrap_or_default()))
        })
        .filter(|agent| agent["archivedAt"].is_null() || activity_time(agent) >= since_ms)
        .collect();
    agents.sort_by(compare_list_items);
    agents.truncate(limit);
    ok_structured(json!({"agents": agents}))
}

// ---- lifecycle ---------------------------------------------------------------------------------

async fn cancel_agent(env: Arc<ToolEnv>, input: Value, _c: CancellationToken) -> ToolOutcome {
    let cancelled = env
        .core
        .cancel_agent_run(required_str(&input, "agentId"))
        .await?;
    ok_structured(json!({"success": cancelled}))
}

async fn archive_agent(env: Arc<ToolEnv>, input: Value, _c: CancellationToken) -> ToolOutcome {
    env.core
        .archive_agent(required_str(&input, "agentId"))
        .await?;
    success()
}

async fn kill_agent(env: Arc<ToolEnv>, input: Value, _c: CancellationToken) -> ToolOutcome {
    env.core
        .close_agent(required_str(&input, "agentId"))
        .await?;
    success()
}

async fn update_agent(env: Arc<ToolEnv>, input: Value, _c: CancellationToken) -> ToolOutcome {
    let agent_id = required_str(&input, "agentId");
    let settings = obj_arg(&input, "settings");
    if let Some(mode) = settings
        .and_then(|s| s.get("modeId"))
        .and_then(Value::as_str)
    {
        env.core.set_agent_mode(agent_id, mode).await?;
    }
    if let Some(model) = settings.and_then(|s| s.get("model")) {
        env.core.set_agent_model(agent_id, model.as_str()).await?;
    }
    if let Some(option) = settings.and_then(|s| s.get("thinkingOptionId")) {
        env.core
            .set_agent_thinking_option(agent_id, option.as_str())
            .await?;
    }
    if let Some(features) = settings
        .and_then(|s| s.get("features"))
        .and_then(Value::as_object)
    {
        for (feature_id, value) in features {
            env.core
                .set_agent_feature(agent_id, feature_id, value)
                .await?;
        }
    }

    // PARITY-QUIRK(update-agent-noop-success): `updateAgentCommand` reports "Nothing to update"
    // and the tool ignores it, so an empty update succeeds.
    let title = str_arg(&input, "name")
        .map(js_trim)
        .filter(|t| !t.is_empty())
        .map(str::to_owned);
    let labels = obj_arg(&input, "labels").filter(|l| !l.is_empty()).cloned();
    if title.is_some() || labels.is_some() {
        env.core
            .update_agent_metadata(agent_id, title, labels)
            .await?;
    }
    success()
}
