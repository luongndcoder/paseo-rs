//! A `CoreApi` built from a JSON profile of `gen-behavior-vectors.mts`, recording calls with the
//! names and argument shapes of the TS stubs.

#![allow(dead_code)]

use std::sync::Mutex;

use async_trait::async_trait;
use paseo_agent_types::AgentTimelineItem;
use paseo_store::WorkspaceRecord;
use paseo_tools::core_api::*;
use paseo_tools::ToolError;
use serde_json::{json, Map, Value};

const T0: &str = "2026-01-15T10:00:00.000Z";
pub const FIXED_NOW_MS: i64 = 1_769_947_200_000;

pub struct ProfileCore {
    profile: Value,
    broker_payload: Value,
    pub calls: Mutex<Vec<Value>>,
}

impl ProfileCore {
    pub fn new(profile: &Value, broker_payload: Value) -> Self {
        Self {
            profile: profile.clone(),
            broker_payload,
            calls: Mutex::new(Vec::new()),
        }
    }

    fn record(&self, target: &str, method: &str, args: Vec<Value>) {
        self.calls
            .lock()
            .unwrap()
            .push(json!({"target": target, "method": method, "args": args}));
    }

    pub fn calls(&self) -> Vec<Value> {
        self.calls.lock().unwrap().clone()
    }

    pub fn caller_id(&self) -> Option<String> {
        self.profile["caller"]["id"].as_str().map(str::to_owned)
    }

    fn schedules(&self) -> &Map<String, Value> {
        self.profile["schedules"].as_object().unwrap()
    }

    fn workspace_json(w: &Value) -> Value {
        json!({
            "workspaceId": w["workspaceId"], "projectId": "prj_1", "cwd": w["cwd"], "kind": "directory",
            "displayName": w["workspaceId"], "title": w["title"], "createdAt": T0, "updatedAt": T0,
            "archivedAt": if w["archived"] == true { json!(T0) } else { Value::Null },
            "background": false,
        })
    }

    fn workspaces(&self) -> Vec<Value> {
        self.profile["workspaces"]
            .as_array()
            .unwrap()
            .iter()
            .map(Self::workspace_json)
            .collect()
    }

    fn provider_entries(&self) -> Vec<ProviderEntry> {
        self.profile["providers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| ProviderEntry {
                provider: p["provider"].as_str().unwrap().into(),
                label: p["label"].as_str().map(str::to_owned),
                description: p["description"].as_str().map(str::to_owned),
                enabled: p["enabled"].as_bool().unwrap(),
                modes: p["modes"].as_array().cloned(),
                status: p["status"].as_str().unwrap().into(),
                error: p["error"].as_str().map(str::to_owned),
            })
            .collect()
    }

    fn terminals(&self) -> Vec<TerminalSummary> {
        self.profile["terminals"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| TerminalSummary {
                id: t["id"].as_str().unwrap().into(),
                name: t["name"].as_str().unwrap().into(),
                cwd: t["cwd"].as_str().unwrap().into(),
            })
            .collect()
    }
}

fn nope<T>(what: &str) -> Result<T, ToolError> {
    Err(ToolError::new(format!("{what} is not scripted")))
}

#[async_trait]
impl CoreApi for ProfileCore {
    fn has(&self, capability: Capability) -> bool {
        let name = match capability {
            Capability::Schedules => "schedules",
            Capability::Terminals => "terminals",
            Capability::WorkspaceRegistry => "workspaceRegistry",
            Capability::WorkspaceUpdates => "workspaceUpdates",
            Capability::DirectoryWorkspaces => "directoryWorkspaces",
            Capability::WorkspaceMinting => "workspaceMinting",
            Capability::BrowserBroker => "browser",
            _ => return false,
        };
        self.profile["caps"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c == name)
    }

    fn now_ms(&self) -> i64 {
        FIXED_NOW_MS
    }

    fn process_cwd(&self) -> String {
        "/proc/cwd".into()
    }

    fn get_agent(&self, id: &str) -> Option<AgentInfo> {
        let caller = &self.profile["caller"];
        if caller.is_null() || caller["id"] != id {
            return None;
        }
        let config = &caller["config"];
        let text = |key: &str| config[key].as_str().map(str::to_owned);
        let value = |key: &str| (!config[key].is_null()).then(|| config[key].clone());
        Some(AgentInfo {
            id: id.into(),
            provider: caller["provider"].as_str().unwrap().into(),
            cwd: caller["cwd"].as_str().unwrap().into(),
            workspace_id: caller["workspaceId"].as_str().map(str::to_owned),
            lifecycle: "idle".into(),
            current_mode_id: caller["currentModeId"].as_str().map(str::to_owned),
            available_modes: vec![],
            config: AgentConfigInfo {
                model: text("model"),
                thinking_option_id: text("thinkingOptionId"),
                provider_options: value("providerOptions"),
                feature_values: value("featureValues"),
                system_prompt: text("systemPrompt"),
                mcp_servers: value("mcpServers"),
            },
        })
    }

    fn list_agent_permissions(&self) -> Vec<AgentPermissions> {
        vec![]
    }
    async fn agent_status_snapshot(
        &self,
        _: &str,
    ) -> Result<Option<AgentStatusSnapshot>, ToolError> {
        nope("agent_status_snapshot")
    }
    async fn list_agent_snapshots(&self, _: bool, _: bool) -> Result<Vec<Value>, ToolError> {
        nope("list_agent_snapshots")
    }
    async fn ensure_agent_loaded(&self, _: &str) -> Result<(), ToolError> {
        nope("ensure_agent_loaded")
    }
    fn agent_timeline(&self, _: &str) -> Vec<AgentTimelineItem> {
        vec![]
    }
    async fn create_agent(&self, _: CreateAgentRequest) -> Result<CreateAgentResult, ToolError> {
        nope("create_agent")
    }
    async fn send_prompt_to_agent(
        &self,
        _: SendPromptRequest,
        _: OnDispatch<'_>,
    ) -> Result<PromptDisposition, ToolError> {
        nope("send_prompt_to_agent")
    }
    async fn wait_for_agent_run_start(&self, _: &str) -> Result<(), ToolError> {
        nope("wait_for_agent_run_start")
    }
    fn setup_finish_notification(&self, _: FinishNotificationRequest) -> StopNotification {
        Box::new(|| {})
    }
    async fn cancel_agent_run(&self, _: &str) -> Result<bool, ToolError> {
        nope("cancel_agent_run")
    }
    async fn archive_agent(&self, _: &str) -> Result<(), ToolError> {
        nope("archive_agent")
    }
    async fn close_agent(&self, _: &str) -> Result<(), ToolError> {
        nope("close_agent")
    }
    async fn update_agent_metadata(
        &self,
        id: &str,
        title: Option<String>,
        labels: Option<Map<String, Value>>,
    ) -> Result<(), ToolError> {
        let mut updates = Map::new();
        if let Some(title) = title {
            updates.insert("title".into(), json!(title));
        }
        if let Some(labels) = labels {
            updates.insert("labels".into(), Value::Object(labels));
        }
        self.record(
            "agentManager",
            "updateAgentMetadata",
            vec![json!(id), Value::Object(updates)],
        );
        Ok(())
    }
    async fn set_agent_mode(&self, id: &str, mode: &str) -> Result<(), ToolError> {
        self.record("agentManager", "setAgentMode", vec![json!(id), json!(mode)]);
        Ok(())
    }
    async fn set_agent_model(&self, id: &str, model: Option<&str>) -> Result<(), ToolError> {
        self.record(
            "agentManager",
            "setAgentModel",
            vec![json!(id), json!(model)],
        );
        Ok(())
    }
    async fn set_agent_thinking_option(
        &self,
        id: &str,
        option: Option<&str>,
    ) -> Result<(), ToolError> {
        self.record(
            "agentManager",
            "setAgentThinkingOption",
            vec![json!(id), json!(option)],
        );
        Ok(())
    }
    async fn set_agent_feature(
        &self,
        id: &str,
        feature: &str,
        value: &Value,
    ) -> Result<(), ToolError> {
        self.record(
            "agentManager",
            "setAgentFeature",
            vec![json!(id), json!(feature), value.clone()],
        );
        Ok(())
    }
    async fn respond_to_permission(&self, _: &str, _: &str, _: &Value) -> Result<(), ToolError> {
        nope("respond_to_permission")
    }

    async fn list_providers(&self) -> Result<Vec<ProviderEntry>, ToolError> {
        Ok(self.provider_entries())
    }
    async fn get_provider(&self, cwd: &str, provider: &str) -> Result<ProviderEntry, ToolError> {
        self.record(
            "providerSnapshotManager",
            "getProvider",
            vec![json!({"cwd": cwd, "provider": provider, "wait": true})],
        );
        self.provider_entries()
            .into_iter()
            .find(|e| e.provider == provider)
            .ok_or_else(|| ToolError::new(format!("Provider {provider} is not configured")))
    }
    async fn list_models(&self, _: &str) -> Result<Vec<Value>, ToolError> {
        nope("list_models")
    }
    async fn list_draft_features(&self, request: Value) -> Result<Vec<Value>, ToolError> {
        self.record("agentManager", "listDraftFeatures", vec![request]);
        Ok(vec![])
    }

    async fn list_workspaces(&self) -> Result<Vec<WorkspaceRecord>, ToolError> {
        Ok(self
            .workspaces()
            .into_iter()
            .map(|w| serde_json::from_value(w).unwrap())
            .collect())
    }
    async fn get_workspace(&self, id: &str) -> Result<Option<WorkspaceRecord>, ToolError> {
        Ok(self
            .workspaces()
            .into_iter()
            .find(|w| w["workspaceId"] == id)
            .map(|w| serde_json::from_value(w).unwrap()))
    }
    async fn upsert_workspace(&self, record: WorkspaceRecord) -> Result<(), ToolError> {
        self.record(
            "workspaceRegistry",
            "upsert",
            vec![json!({"workspaceId": record.workspace_id, "title": record.title, "updatedAt": record.updated_at})],
        );
        Ok(())
    }
    async fn emit_workspace_updates(&self, ids: &[String]) -> Result<(), ToolError> {
        self.record("daemon", "emitWorkspaceUpdates", vec![json!(ids)]);
        Ok(())
    }
    async fn create_directory_workspace(
        &self,
        cwd: &str,
        title: Option<&str>,
        project: Option<&str>,
        ctx: DirectoryWorkspaceContext,
    ) -> Result<WorkspaceRecord, ToolError> {
        let mut context = Map::new();
        if let Some(background) = ctx.background {
            context.insert("background".into(), json!(background));
        }
        if let Some(caller) = &ctx.caller_workspace_id {
            context.insert("callerWorkspaceId".into(), json!(caller));
        }
        self.record(
            "daemon",
            "createDirectoryWorkspace",
            vec![
                json!(cwd),
                json!(title),
                json!(project),
                Value::Object(context),
            ],
        );
        Ok(serde_json::from_value(Self::workspace_json(
            &json!({"workspaceId": "wks_created", "cwd": cwd, "title": title}),
        ))
        .unwrap())
    }
    async fn ensure_workspace_for_create(
        &self,
        cwd: &str,
        context: Option<Value>,
        caller: Option<&str>,
    ) -> Result<String, ToolError> {
        let mut args = vec![json!(cwd)];
        if context.is_some() || caller.is_some() {
            args.push(json!(context));
        }
        self.record("daemon", "ensureWorkspaceForCreate", args);
        Ok("wks_minted".into())
    }

    fn terminal_directories(&self) -> Vec<String> {
        let mut dirs: Vec<String> = Vec::new();
        for t in self.terminals() {
            if !dirs.contains(&t.cwd) {
                dirs.push(t.cwd);
            }
        }
        dirs
    }
    async fn get_terminals(&self, cwd: &str) -> Result<Vec<TerminalSummary>, ToolError> {
        Ok(self
            .terminals()
            .into_iter()
            .filter(|t| t.cwd == cwd)
            .collect())
    }
    fn get_terminal(&self, id: &str) -> Option<TerminalSummary> {
        self.terminals().into_iter().find(|t| t.id == id)
    }
    async fn create_terminal(
        &self,
        cwd: &str,
        workspace_id: &str,
        name: Option<&str>,
    ) -> Result<TerminalSummary, ToolError> {
        let mut input = Map::new();
        input.insert("cwd".into(), json!(cwd));
        input.insert("workspaceId".into(), json!(workspace_id));
        if let Some(name) = name {
            input.insert("name".into(), json!(name));
        }
        self.record(
            "terminalManager",
            "createTerminal",
            vec![Value::Object(input)],
        );
        Ok(TerminalSummary {
            id: "term-new".into(),
            name: name.unwrap_or("Terminal").into(),
            cwd: cwd.into(),
        })
    }
    async fn capture_terminal(
        &self,
        id: &str,
        options: CaptureOptions,
    ) -> Result<TerminalCapture, ToolError> {
        let mut opts = Map::new();
        if let Some(start) = options.start {
            opts.insert("start".into(), paseo_tools::util::json_number(start));
        }
        if let Some(end) = options.end {
            opts.insert("end".into(), paseo_tools::util::json_number(end));
        }
        opts.insert("stripAnsi".into(), json!(options.strip_ansi));
        self.record(
            "terminalManager",
            "captureTerminal",
            vec![json!(id), Value::Object(opts)],
        );
        Ok(TerminalCapture {
            lines: vec!["a".into()],
            total_lines: 1,
        })
    }
    fn kill_terminal(&self, _id: &str) {
        self.record("terminal", "kill", vec![]);
    }
    fn send_terminal_input(&self, _id: &str, data: &str) {
        self.record(
            "terminal",
            "send",
            vec![json!({"type": "input", "data": data})],
        );
    }

    async fn schedule_list(&self) -> Result<Vec<Value>, ToolError> {
        Ok(self.schedules().values().cloned().collect())
    }
    async fn schedule_inspect(&self, id: &str) -> Result<Value, ToolError> {
        self.schedules()
            .get(id)
            .cloned()
            .ok_or_else(|| ToolError::new(format!("Schedule not found: {id}")))
    }
    async fn schedule_create_or_replace(&self, input: Value) -> Result<Value, ToolError> {
        self.record("scheduleService", "createOrReplace", vec![input.clone()]);
        Ok(json!({"id": "sch-new", "runs": [], "echo": input}))
    }
    async fn schedule_pause(&self, id: &str) -> Result<(), ToolError> {
        self.record("scheduleService", "pause", vec![json!(id)]);
        Ok(())
    }
    async fn schedule_resume(&self, id: &str) -> Result<(), ToolError> {
        self.record("scheduleService", "resume", vec![json!(id)]);
        Ok(())
    }
    async fn schedule_delete(&self, id: &str) -> Result<(), ToolError> {
        self.record("scheduleService", "delete", vec![json!(id)]);
        Ok(())
    }
    async fn schedule_run_once(&self, id: &str) -> Result<Value, ToolError> {
        self.record("scheduleService", "runOnce", vec![json!(id)]);
        Ok(json!({"id": id}))
    }
    async fn schedule_update(&self, input: Value) -> Result<Value, ToolError> {
        self.record("scheduleService", "update", vec![input.clone()]);
        Ok(json!({"id": input["id"], "updated": true}))
    }
    async fn schedule_logs(&self, id: &str) -> Result<Vec<Value>, ToolError> {
        self.record("scheduleService", "logs", vec![json!(id)]);
        Ok(vec![json!({"id": "run-1"})])
    }

    async fn browser_execute(&self, request: Value) -> Result<Value, ToolError> {
        self.record("browserBroker", "execute", vec![request]);
        Ok(self.broker_payload.clone())
    }
}
