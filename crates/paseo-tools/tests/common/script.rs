//! A scriptable `CoreApi` for behavior tests: every call is logged, every dependency is a knob.

#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use paseo_agent_types::AgentTimelineItem;
use paseo_core::PaseoToolPolicy;
use paseo_store::WorkspaceRecord;
use paseo_tools::core_api::*;
use paseo_tools::{RuntimeCtx, ToolCatalog, ToolCatalogFactory, ToolError};
use serde_json::{json, Map, Value};
use tokio_util::sync::CancellationToken;

pub const NOW_MS: i64 = 1_769_947_200_000; // 2026-02-01T12:00:00.000Z

#[derive(Default)]
pub struct State {
    pub caps: HashSet<Capability>,
    pub agents: HashMap<String, AgentInfo>,
    pub caller_context: Option<CallerContext>,
    pub policy: Option<PaseoToolPolicy>,
    pub speak_handler: bool,
    pub process_cwd: String,

    pub create_result: Option<CreateAgentResult>,
    pub create_error: Option<String>,
    pub minted_workspace: String,
    pub dispatch: Option<PromptDisposition>,
    pub send_error: Option<String>,
    pub wait_error: Option<String>,
    pub cancel_result: bool,
    pub set_mode_error: Option<String>,

    pub snapshots: Vec<Value>,
    pub status_snapshots: HashMap<String, AgentStatusSnapshot>,
    pub timeline: Vec<AgentTimelineItem>,

    pub providers: Vec<ProviderEntry>,
    pub workspaces: Vec<WorkspaceRecord>,
    pub active: Vec<ActiveWorkspaceRef>,
    pub created_workspace: Option<WorkspaceRecord>,
    pub source_cwd: String,
    pub schedules: HashMap<String, Value>,
    pub terminals: Vec<TerminalSummary>,
    pub browser_payload: Value,
}

pub struct Script {
    pub state: Mutex<State>,
    pub calls: Mutex<Vec<Value>>,
    pub stopped: Arc<Mutex<Vec<String>>>,
}

impl Script {
    pub fn new() -> Arc<Self> {
        let mut state = State {
            process_cwd: "/proc/cwd".into(),
            minted_workspace: "wks_minted".into(),
            source_cwd: "/projects/root".into(),
            browser_payload: json!({"ok": true, "result": {"command": "back"}}),
            ..State::default()
        };
        state.dispatch = Some(PromptDisposition::TurnStarted);
        Arc::new(Self {
            state: Mutex::new(state),
            calls: Mutex::new(Vec::new()),
            stopped: Arc::new(Mutex::new(Vec::new())),
        })
    }

    pub fn with(self: &Arc<Self>, f: impl FnOnce(&mut State)) -> Arc<Self> {
        f(&mut self.state.lock().unwrap());
        self.clone()
    }

    pub fn caps(self: &Arc<Self>, caps: &[Capability]) -> Arc<Self> {
        self.with(|s| s.caps.extend(caps.iter().copied()))
    }

    fn log(&self, method: &str, args: Value) {
        self.calls
            .lock()
            .unwrap()
            .push(json!({"method": method, "args": args}));
    }

    pub fn calls(&self) -> Vec<Value> {
        self.calls.lock().unwrap().clone()
    }

    /// Calls of one method, as their args.
    pub fn args_of(&self, method: &str) -> Vec<Value> {
        self.calls()
            .into_iter()
            .filter(|c| c["method"] == method)
            .map(|c| c["args"].clone())
            .collect()
    }

    pub fn methods(&self) -> Vec<String> {
        self.calls()
            .iter()
            .map(|c| c["method"].as_str().unwrap().to_owned())
            .collect()
    }

    pub async fn catalog(self: &Arc<Self>, runtime: RuntimeCtx) -> ToolCatalog {
        ToolCatalogFactory::new(self.clone()).build(runtime).await
    }
}

pub fn agent(id: &str, provider: &str, cwd: &str, workspace: Option<&str>) -> AgentInfo {
    AgentInfo {
        id: id.into(),
        provider: provider.into(),
        cwd: cwd.into(),
        workspace_id: workspace.map(str::to_owned),
        lifecycle: "idle".into(),
        current_mode_id: Some("default".into()),
        available_modes: vec![json!({"id": "default"})],
        config: AgentConfigInfo::default(),
    }
}

pub fn workspace(id: &str, cwd: &str, background: bool, archived: bool) -> WorkspaceRecord {
    serde_json::from_value(json!({
        "workspaceId": id, "projectId": "prj_1", "cwd": cwd, "kind": "directory",
        "displayName": id, "title": null,
        "createdAt": "2026-01-15T10:00:00.000Z", "updatedAt": "2026-01-15T10:00:00.000Z",
        "archivedAt": if archived { json!("2026-01-15T10:00:00.000Z") } else { Value::Null },
        "background": background,
    }))
    .unwrap()
}

fn nope<T>(what: &str) -> Result<T, ToolError> {
    Err(ToolError::new(format!("{what} is not scripted")))
}

#[async_trait]
impl CoreApi for Script {
    fn has(&self, c: Capability) -> bool {
        self.state.lock().unwrap().caps.contains(&c)
    }
    fn now_ms(&self) -> i64 {
        NOW_MS
    }
    fn process_cwd(&self) -> String {
        self.state.lock().unwrap().process_cwd.clone()
    }
    fn paseo_tool_policy(&self, _id: &str) -> Option<PaseoToolPolicy> {
        self.state.lock().unwrap().policy.clone()
    }
    fn caller_context(&self, _id: &str) -> Option<CallerContext> {
        self.state.lock().unwrap().caller_context.clone()
    }
    fn get_agent(&self, id: &str) -> Option<AgentInfo> {
        self.state.lock().unwrap().agents.get(id).cloned()
    }
    fn list_agent_permissions(&self) -> Vec<AgentPermissions> {
        vec![]
    }
    async fn agent_status_snapshot(
        &self,
        id: &str,
    ) -> Result<Option<AgentStatusSnapshot>, ToolError> {
        Ok(self.state.lock().unwrap().status_snapshots.get(id).cloned())
    }
    async fn list_agent_snapshots(
        &self,
        bg: bool,
        archived: bool,
    ) -> Result<Vec<Value>, ToolError> {
        self.log("list_agent_snapshots", json!([bg, archived]));
        Ok(self.state.lock().unwrap().snapshots.clone())
    }
    async fn ensure_agent_loaded(&self, id: &str) -> Result<(), ToolError> {
        self.log("ensure_agent_loaded", json!([id]));
        Ok(())
    }
    fn agent_timeline(&self, _id: &str) -> Vec<AgentTimelineItem> {
        self.log("agent_timeline", json!([]));
        self.state.lock().unwrap().timeline.clone()
    }
    async fn create_agent(&self, r: CreateAgentRequest) -> Result<CreateAgentResult, ToolError> {
        self.log(
            "create_agent",
            json!({
                "provider": r.provider, "title": r.title, "initialPrompt": r.initial_prompt,
                "providerOptions": r.provider_options, "cwd": r.cwd, "workspaceId": r.workspace_id,
                "thinking": r.thinking, "features": r.features, "labels": r.labels, "mode": r.mode,
                "notifyOnFinish": r.notify_on_finish, "detached": r.detached,
                "callerAgentId": r.caller_agent_id,
                "callerContext": r.caller_context.map(|c| json!({"lockedCwd": c.locked_cwd, "allowCustomCwd": c.allow_custom_cwd})),
                "worktree": r.worktree.map(|w| json!({
                    "action": w.action, "worktreeName": w.worktree_name, "branchName": w.branch_name,
                    "baseBranch": w.base_branch, "refName": w.ref_name, "githubPrNumber": w.github_pr_number,
                })),
            }),
        );
        let state = self.state.lock().unwrap();
        if let Some(message) = &state.create_error {
            return Err(ToolError::new(message.clone()));
        }
        Ok(state.create_result.clone().unwrap_or(CreateAgentResult {
            snapshot: agent("new-agent", &r.provider, "/new", Some("wks_new")),
            initial_prompt_started: true,
        }))
    }
    async fn send_prompt_to_agent(
        &self,
        r: SendPromptRequest,
        on_dispatch: OnDispatch<'_>,
    ) -> Result<PromptDisposition, ToolError> {
        self.log(
            "send_prompt_to_agent",
            json!({"agentId": r.agent_id, "prompt": r.prompt, "source": r.source_agent_id, "sessionMode": r.session_mode}),
        );
        let (disposition, error) = {
            let state = self.state.lock().unwrap();
            (state.dispatch.unwrap(), state.send_error.clone())
        };
        on_dispatch(disposition);
        match error {
            Some(message) => Err(ToolError::new(message)),
            None => Ok(disposition),
        }
    }
    async fn wait_for_agent_run_start(&self, id: &str) -> Result<(), ToolError> {
        self.log("wait_for_agent_run_start", json!([id]));
        match self.state.lock().unwrap().wait_error.clone() {
            Some(message) => Err(ToolError::new(message)),
            None => Ok(()),
        }
    }
    fn setup_finish_notification(&self, r: FinishNotificationRequest) -> StopNotification {
        self.log(
            "setup_finish_notification",
            json!({"child": r.child_agent_id, "caller": r.caller_agent_id, "waitForTurnStart": r.wait_for_turn_start}),
        );
        let stopped = self.stopped.clone();
        let child = r.child_agent_id.clone();
        Box::new(move || stopped.lock().unwrap().push(child))
    }
    async fn cancel_agent_run(&self, id: &str) -> Result<bool, ToolError> {
        self.log("cancel_agent_run", json!([id]));
        Ok(self.state.lock().unwrap().cancel_result)
    }
    async fn archive_agent(&self, id: &str) -> Result<(), ToolError> {
        self.log("archive_agent", json!([id]));
        Ok(())
    }
    async fn close_agent(&self, id: &str) -> Result<(), ToolError> {
        self.log("close_agent", json!([id]));
        Ok(())
    }
    async fn update_agent_metadata(
        &self,
        id: &str,
        title: Option<String>,
        labels: Option<Map<String, Value>>,
    ) -> Result<(), ToolError> {
        self.log("update_agent_metadata", json!([id, title, labels]));
        Ok(())
    }
    async fn set_agent_mode(&self, id: &str, mode: &str) -> Result<(), ToolError> {
        self.log("set_agent_mode", json!([id, mode]));
        match self.state.lock().unwrap().set_mode_error.clone() {
            Some(message) => Err(ToolError::new(message)),
            None => Ok(()),
        }
    }
    async fn set_agent_model(&self, id: &str, model: Option<&str>) -> Result<(), ToolError> {
        self.log("set_agent_model", json!([id, model]));
        Ok(())
    }
    async fn set_agent_thinking_option(
        &self,
        id: &str,
        option: Option<&str>,
    ) -> Result<(), ToolError> {
        self.log("set_agent_thinking_option", json!([id, option]));
        Ok(())
    }
    async fn set_agent_feature(
        &self,
        id: &str,
        feature: &str,
        value: &Value,
    ) -> Result<(), ToolError> {
        self.log("set_agent_feature", json!([id, feature, value]));
        Ok(())
    }
    async fn respond_to_permission(
        &self,
        id: &str,
        request: &str,
        response: &Value,
    ) -> Result<(), ToolError> {
        self.log("respond_to_permission", json!([id, request, response]));
        Ok(())
    }
    async fn list_providers(&self) -> Result<Vec<ProviderEntry>, ToolError> {
        Ok(self.state.lock().unwrap().providers.clone())
    }
    async fn get_provider(&self, cwd: &str, provider: &str) -> Result<ProviderEntry, ToolError> {
        self.log("get_provider", json!([cwd, provider]));
        self.state
            .lock()
            .unwrap()
            .providers
            .iter()
            .find(|p| p.provider == provider)
            .cloned()
            .ok_or_else(|| ToolError::new(format!("Provider {provider} is not configured")))
    }
    async fn list_models(&self, _p: &str) -> Result<Vec<Value>, ToolError> {
        nope("list_models")
    }
    async fn list_draft_features(&self, request: Value) -> Result<Vec<Value>, ToolError> {
        self.log("list_draft_features", request);
        Ok(vec![])
    }

    async fn list_workspaces(&self) -> Result<Vec<WorkspaceRecord>, ToolError> {
        Ok(self.state.lock().unwrap().workspaces.clone())
    }
    async fn get_workspace(&self, id: &str) -> Result<Option<WorkspaceRecord>, ToolError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .workspaces
            .iter()
            .find(|w| w.workspace_id == id)
            .cloned())
    }
    async fn upsert_workspace(&self, record: WorkspaceRecord) -> Result<(), ToolError> {
        self.log(
            "upsert_workspace",
            json!([record.workspace_id, record.title, record.updated_at]),
        );
        Ok(())
    }
    async fn emit_workspace_updates(&self, ids: &[String]) -> Result<(), ToolError> {
        self.log("emit_workspace_updates", json!(ids));
        Ok(())
    }
    async fn list_active_workspaces(&self) -> Result<Vec<ActiveWorkspaceRef>, ToolError> {
        Ok(self.state.lock().unwrap().active.clone())
    }
    async fn create_directory_workspace(
        &self,
        cwd: &str,
        title: Option<&str>,
        project: Option<&str>,
        ctx: DirectoryWorkspaceContext,
    ) -> Result<WorkspaceRecord, ToolError> {
        self.log(
            "create_directory_workspace",
            json!([cwd, title, project, {"background": ctx.background, "callerWorkspaceId": ctx.caller_workspace_id}]),
        );
        Ok(self
            .state
            .lock()
            .unwrap()
            .created_workspace
            .clone()
            .unwrap_or_else(|| {
                workspace("wks_created", cwd, ctx.background.unwrap_or(false), false)
            }))
    }
    async fn resolve_worktree_source_cwd(&self, project: &str) -> Result<String, ToolError> {
        self.log("resolve_worktree_source_cwd", json!([project]));
        Ok(self.state.lock().unwrap().source_cwd.clone())
    }
    async fn create_paseo_worktree(
        &self,
        r: CreateWorktreeRequest,
    ) -> Result<WorkspaceRecord, ToolError> {
        self.log(
            "create_paseo_worktree",
            json!({
                "cwd": r.cwd, "projectId": r.project_id, "worktreeSlug": r.worktree_slug, "action": r.action,
                "branchName": r.branch_name, "refName": r.ref_name, "checkoutSource": r.checkout_source,
                "title": r.title, "background": r.background, "callerWorkspaceId": r.caller_workspace_id,
            }),
        );
        Ok(self
            .state
            .lock()
            .unwrap()
            .created_workspace
            .clone()
            .unwrap_or_else(|| workspace("wks_wt", &r.cwd, false, false)))
    }
    async fn archive_workspace(&self, id: &str) -> Result<ArchiveWorkspaceResult, ToolError> {
        self.log("archive_workspace", json!([id]));
        Ok(ArchiveWorkspaceResult {
            archived_agent_ids: vec!["a1".into()],
            removed_directory: true,
        })
    }
    async fn ensure_workspace_for_create(
        &self,
        cwd: &str,
        context: Option<Value>,
        caller: Option<&str>,
    ) -> Result<String, ToolError> {
        self.log("ensure_workspace_for_create", json!([cwd, context, caller]));
        Ok(self.state.lock().unwrap().minted_workspace.clone())
    }
    async fn workspace_scripts_list(&self, id: &str) -> Result<Vec<Value>, ToolError> {
        self.log("workspace_scripts_list", json!([id]));
        Ok(vec![json!({"scriptName": "dev"})])
    }
    async fn workspace_script_launch(&self, w: &str, s: &str) -> Result<Value, ToolError> {
        self.log("workspace_script_launch", json!([w, s]));
        Ok(json!({"scriptName": s, "lifecycle": "running"}))
    }
    async fn workspace_script_stop(&self, w: &str, s: &str) -> Result<Value, ToolError> {
        self.log("workspace_script_stop", json!([w, s]));
        Ok(json!({"scriptName": s, "lifecycle": "stopped"}))
    }

    fn terminal_directories(&self) -> Vec<String> {
        let mut dirs: Vec<String> = self
            .state
            .lock()
            .unwrap()
            .terminals
            .iter()
            .map(|t| t.cwd.clone())
            .collect();
        dirs.dedup();
        dirs
    }
    async fn get_terminals(&self, cwd: &str) -> Result<Vec<TerminalSummary>, ToolError> {
        self.log("get_terminals", json!([cwd]));
        Ok(self
            .state
            .lock()
            .unwrap()
            .terminals
            .iter()
            .filter(|t| t.cwd == cwd)
            .cloned()
            .collect())
    }
    fn get_terminal(&self, id: &str) -> Option<TerminalSummary> {
        self.state
            .lock()
            .unwrap()
            .terminals
            .iter()
            .find(|t| t.id == id)
            .cloned()
    }
    async fn create_terminal(
        &self,
        cwd: &str,
        workspace_id: &str,
        name: Option<&str>,
    ) -> Result<TerminalSummary, ToolError> {
        self.log("create_terminal", json!([cwd, workspace_id, name]));
        Ok(TerminalSummary {
            id: "term-new".into(),
            name: name.unwrap_or("Terminal").into(),
            cwd: cwd.into(),
        })
    }

    async fn schedule_list(&self) -> Result<Vec<Value>, ToolError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .schedules
            .values()
            .cloned()
            .collect())
    }
    async fn schedule_inspect(&self, id: &str) -> Result<Value, ToolError> {
        self.state
            .lock()
            .unwrap()
            .schedules
            .get(id)
            .cloned()
            .ok_or_else(|| ToolError::new(format!("Schedule not found: {id}")))
    }
    async fn schedule_create_or_replace(&self, input: Value) -> Result<Value, ToolError> {
        self.log("schedule_create_or_replace", input.clone());
        Ok(json!({"id": "sch-new", "runs": [], "echo": input}))
    }
    async fn schedule_pause(&self, id: &str) -> Result<(), ToolError> {
        self.log("schedule_pause", json!([id]));
        Ok(())
    }
    async fn schedule_resume(&self, id: &str) -> Result<(), ToolError> {
        self.log("schedule_resume", json!([id]));
        Ok(())
    }
    async fn schedule_delete(&self, id: &str) -> Result<(), ToolError> {
        self.log("schedule_delete", json!([id]));
        Ok(())
    }
    async fn schedule_run_once(&self, id: &str) -> Result<Value, ToolError> {
        self.log("schedule_run_once", json!([id]));
        Ok(json!({"id": id}))
    }
    async fn schedule_update(&self, input: Value) -> Result<Value, ToolError> {
        self.log("schedule_update", input.clone());
        Ok(json!({"id": input["id"], "updated": true}))
    }
    async fn schedule_logs(&self, id: &str) -> Result<Vec<Value>, ToolError> {
        self.log("schedule_logs", json!([id]));
        Ok(vec![json!({"id": "run-1"})])
    }

    fn has_speak_handler(&self, _id: &str) -> bool {
        self.state.lock().unwrap().speak_handler
    }
    async fn speak(&self, id: &str, text: &str, _c: CancellationToken) -> Result<(), ToolError> {
        self.log("speak", json!([id, text]));
        Ok(())
    }
    async fn browser_execute(&self, request: Value) -> Result<Value, ToolError> {
        self.log("browser_execute", request);
        Ok(self.state.lock().unwrap().browser_payload.clone())
    }
}
