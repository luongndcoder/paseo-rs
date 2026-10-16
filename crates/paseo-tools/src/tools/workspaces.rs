//! Workspace tools: create/list/archive/rename and workspace scripts.

use std::sync::Arc;

use paseo_store::{WorkspaceKind, WorkspaceRecord};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use super::{bool_arg, handler, ok_structured, required_str, str_arg, ToolEnv, ToolOutcome};
use crate::catalog::Registrar;
use crate::core_api::{Capability, CreateWorktreeRequest, DirectoryWorkspaceContext};
use crate::error::ToolError;
use paseo_core::time::iso8601_from_millis;

pub(crate) fn register_first(r: &mut Registrar) {
    r.register("create_workspace", handler(create_workspace));
    r.register("list_workspaces", handler(list_workspaces));
    r.register("archive_workspace", handler(archive_workspace));
}

pub(crate) fn register_second(r: &mut Registrar) {
    r.register("rename_workspace", handler(rename_workspace));
    r.register("list_workspace_scripts", handler(list_workspace_scripts));
    r.register("start_workspace_script", handler(start_workspace_script));
    r.register("stop_workspace_script", handler(stop_workspace_script));
}

fn kind_name(kind: WorkspaceKind) -> &'static str {
    match kind {
        WorkspaceKind::LocalCheckout => "local_checkout",
        WorkspaceKind::Worktree => "worktree",
        WorkspaceKind::Directory => "directory",
    }
}

/// `toWorkspaceAutomationSummary`.
fn workspace_summary(workspace: &WorkspaceRecord) -> Value {
    json!({
        "workspaceId": workspace.workspace_id,
        "projectId": workspace.project_id,
        "cwd": workspace.cwd,
        "isolation": if workspace.kind == WorkspaceKind::Worktree { "worktree" } else { "local" },
        "kind": kind_name(workspace.kind),
        "title": workspace.title,
        "background": workspace.background,
    })
}

/// `assertOptionsAbsent`.
fn assert_options_absent(input: &Value, keys: &[&str], message: &str) -> Result<(), ToolError> {
    if keys.iter().any(|k| input.get(k).is_some()) {
        return Err(ToolError::new(message));
    }
    Ok(())
}

/// `isExistingDirectory`: missing paths are not directories, other stat errors surface.
async fn is_existing_directory(path: &str) -> Result<bool, ToolError> {
    match tokio::fs::metadata(path).await {
        Ok(meta) => Ok(meta.is_dir()),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            Ok(false)
        }
        Err(error) => Err(ToolError::new(error.to_string())),
    }
}

fn truthy_str(value: Option<&str>) -> Option<String> {
    value.filter(|v| !v.is_empty()).map(str::to_owned)
}

/// `resolveWorkspaceWorktreeTarget` merged into the request being built.
fn apply_worktree_target(
    input: &Value,
    request: &mut CreateWorktreeRequest,
) -> Result<(), ToolError> {
    match str_arg(input, "mode").unwrap_or("branch-off") {
        "branch-off" => {
            assert_options_absent(
                input,
                &["branch", "prNumber", "forge"],
                "branch, prNumber, and forge require a checkout mode",
            )?;
            request.action = "branch-off".into();
            request.branch_name = truthy_str(str_arg(input, "branchName"));
            request.ref_name = truthy_str(str_arg(input, "baseBranch"));
        }
        "checkout-branch" => {
            let Some(branch) = truthy_str(str_arg(input, "branch")) else {
                return Err(ToolError::new(
                    "branch is required for checkout-branch mode",
                ));
            };
            assert_options_absent(
                input,
                &["branchName", "baseBranch", "prNumber", "forge"],
                "branchName, baseBranch, prNumber, and forge are not valid for checkout-branch mode",
            )?;
            request.action = "checkout".into();
            request.ref_name = Some(branch);
        }
        _ => {
            let Some(pr_number) = input.get("prNumber").cloned() else {
                return Err(ToolError::new("prNumber is required for checkout-pr mode"));
            };
            assert_options_absent(
                input,
                &["branchName", "baseBranch", "branch"],
                "branchName, baseBranch, and branch are not valid for checkout-pr mode",
            )?;
            request.action = "checkout".into();
            let mut source = serde_json::Map::new();
            source.insert("kind".into(), json!("change_request"));
            if let Some(forge) = truthy_str(str_arg(input, "forge")) {
                source.insert("forge".into(), json!(forge));
            }
            source.insert("number".into(), pr_number);
            request.checkout_source = Some(Value::Object(source));
        }
    }
    Ok(())
}

async fn create_workspace(env: Arc<ToolEnv>, input: Value, _c: CancellationToken) -> ToolOutcome {
    let isolation = required_str(&input, "isolation");
    let path = str_arg(&input, "path");
    let project_id = str_arg(&input, "projectId");
    let title = str_arg(&input, "title");
    let background = bool_arg(&input, "background");
    let workspace = if isolation == "local" {
        let cwd = env.resolve_scoped_cwd(path, true)?;
        if !is_existing_directory(&cwd).await? {
            return Err(ToolError::new(format!("Directory not found: {cwd}")));
        }
        assert_options_absent(
            &input,
            &[
                "mode",
                "worktreeSlug",
                "branchName",
                "baseBranch",
                "branch",
                "prNumber",
                "forge",
            ],
            "Worktree options require isolation worktree",
        )?;
        if !env.core.has(Capability::DirectoryWorkspaces) {
            return Err(ToolError::new("Workspace provisioning is not configured"));
        }
        env.core
            .create_directory_workspace(
                &cwd,
                title,
                project_id,
                DirectoryWorkspaceContext {
                    background,
                    caller_workspace_id: env.resolve_caller_agent()?.and_then(|a| a.workspace_id),
                },
            )
            .await?
    } else {
        let cwd = if path.is_some() || project_id.is_none() {
            Some(env.resolve_scoped_cwd(path, true)?)
        } else {
            None
        };
        let cwd = match cwd {
            Some(cwd) => cwd,
            None => {
                if !env.core.has(Capability::ProjectRegistry) {
                    return Err(ToolError::new("Project registry is not configured"));
                }
                env.core
                    .resolve_worktree_source_cwd(project_id.unwrap_or_default())
                    .await?
            }
        };
        let mut request = CreateWorktreeRequest {
            cwd,
            project_id: project_id.map(str::to_owned),
            worktree_slug: truthy_str(str_arg(&input, "worktreeSlug")),
            title: title.filter(|t| !t.is_empty()).map(str::to_owned),
            background,
            ..CreateWorktreeRequest::default()
        };
        // `resolveWorkspaceWorktreeTarget` runs before the worktree command, after the cwd.
        apply_worktree_target(&input, &mut request)?;
        request.caller_workspace_id = env.resolve_caller_agent()?.and_then(|a| a.workspace_id);
        env.core.create_paseo_worktree(request).await?
    };
    ok_structured(workspace_summary(&workspace))
}

async fn list_workspaces(env: Arc<ToolEnv>, input: Value, _c: CancellationToken) -> ToolOutcome {
    if !env.core.has(Capability::WorkspaceRegistry) {
        return Err(ToolError::new("Workspace registry is not configured"));
    }
    let include_background = bool_arg(&input, "includeBackground").unwrap_or(false);
    let workspaces: Vec<Value> = env
        .core
        .list_workspaces()
        .await?
        .iter()
        .filter(|w| w.archived_at.is_none() && (include_background || !w.background))
        .map(workspace_summary)
        .collect();
    ok_structured(json!({"workspaces": workspaces}))
}

async fn archive_workspace(env: Arc<ToolEnv>, input: Value, _c: CancellationToken) -> ToolOutcome {
    if !env.core.has(Capability::ActiveWorkspaces) {
        return Err(ToolError::new(
            "Active workspace lister is required to archive workspaces",
        ));
    }
    let workspace_id = required_str(&input, "workspaceId");
    let active = env.core.list_active_workspaces().await?;
    let Some(workspace) = active.iter().find(|w| w.workspace_id == workspace_id) else {
        return Err(ToolError::new(format!(
            "Workspace not found: {workspace_id}"
        )));
    };
    let result = env.core.archive_workspace(&workspace.workspace_id).await?;
    ok_structured(json!({
        "workspaceId": workspace_id,
        "archivedAgentIds": result.archived_agent_ids,
        "removedDirectory": result.removed_directory,
    }))
}

async fn rename_workspace(env: Arc<ToolEnv>, input: Value, _c: CancellationToken) -> ToolOutcome {
    if !env.core.has(Capability::WorkspaceRegistry) {
        return Err(ToolError::new(
            "Workspace registry is required to rename workspaces",
        ));
    }
    if !env.core.has(Capability::WorkspaceUpdates) {
        return Err(ToolError::new(
            "Workspace update emitter is required to rename workspaces",
        ));
    }
    let title = required_str(&input, "title");
    // `resolveWorkspaceIdForRename`
    let workspace_id = match str_arg(&input, "workspaceId").map(crate::util::js_trim) {
        Some(id) if !id.is_empty() => id.to_owned(),
        _ => match &env.caller_agent_id {
            Some(caller_id) => {
                let caller = env.resolve_caller_agent()?;
                match caller.and_then(|c| c.workspace_id) {
                    Some(id) => id,
                    None => {
                        return Err(ToolError::new(format!(
                            "Caller agent {caller_id} has no current workspace"
                        )))
                    }
                }
            }
            None => {
                return Err(ToolError::new(
                    "workspaceId is required outside an agent-scoped session",
                ))
            }
        },
    };
    let Some(mut existing) = env.core.get_workspace(&workspace_id).await? else {
        return Err(ToolError::new(format!(
            "Workspace {workspace_id} not found"
        )));
    };
    if existing.archived_at.is_some() {
        return Err(ToolError::new(format!(
            "Workspace {workspace_id} is archived"
        )));
    }
    existing.title = Some(title.to_owned());
    existing.updated_at = iso8601_from_millis(env.core.now_ms());
    env.core.upsert_workspace(existing).await?;
    env.core
        .emit_workspace_updates(std::slice::from_ref(&workspace_id))
        .await?;
    ok_structured(json!({"success": true, "workspaceId": workspace_id, "title": title}))
}

fn require_scripts(env: &ToolEnv) -> Result<(), ToolError> {
    if env.core.has(Capability::WorkspaceScripts) {
        Ok(())
    } else {
        Err(ToolError::new(
            "Workspace script management is not configured",
        ))
    }
}

async fn list_workspace_scripts(
    env: Arc<ToolEnv>,
    input: Value,
    _c: CancellationToken,
) -> ToolOutcome {
    require_scripts(&env)?;
    let scripts = env
        .core
        .workspace_scripts_list(required_str(&input, "workspaceId"))
        .await?;
    ok_structured(json!({"scripts": scripts}))
}

async fn start_workspace_script(
    env: Arc<ToolEnv>,
    input: Value,
    _c: CancellationToken,
) -> ToolOutcome {
    require_scripts(&env)?;
    let script = env
        .core
        .workspace_script_launch(
            required_str(&input, "workspaceId"),
            required_str(&input, "scriptName"),
        )
        .await?;
    ok_structured(json!({"script": script}))
}

async fn stop_workspace_script(
    env: Arc<ToolEnv>,
    input: Value,
    _c: CancellationToken,
) -> ToolOutcome {
    require_scripts(&env)?;
    let script = env
        .core
        .workspace_script_stop(
            required_str(&input, "workspaceId"),
            required_str(&input, "scriptName"),
        )
        .await?;
    ok_structured(json!({"script": script}))
}
