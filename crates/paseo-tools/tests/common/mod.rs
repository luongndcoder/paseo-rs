//! A hand-built `CoreApi` mirroring the stubs of `capture-tool-catalog-fixtures.ts`.

#![allow(dead_code)]

pub mod profile;
pub mod script;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use paseo_agent_types::AgentTimelineItem;
use paseo_store::WorkspaceRecord;
use paseo_tools::core_api::*;
use paseo_tools::{ToolCatalog, ToolCatalogFactory, ToolError};
use serde_json::{json, Map, Value};
use tokio_util::sync::CancellationToken;

pub const LIVE_ID: &str = "11111111-1111-4111-8111-111111111111";
pub const RUNNING_ID: &str = "33333333-3333-4333-8333-333333333333";
pub const STORED_ID: &str = "22222222-2222-4222-8222-222222222222";
const T0: &str = "2026-01-15T10:00:00.000Z";
/// `FIXED_NOW` of the capture: 2026-02-01T12:00:00.000Z.
pub const FIXED_NOW_MS: i64 = 1_769_947_200_000;

pub fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/p4")
}

pub fn read_fixture(path: &str) -> Value {
    let full = fixtures_dir().join(path);
    serde_json::from_str(
        &std::fs::read_to_string(&full).unwrap_or_else(|e| panic!("{full:?}: {e}")),
    )
    .expect("fixture json")
}

#[derive(Clone, Default)]
pub struct Options {
    pub voice: bool,
    pub browser: bool,
    pub caller: bool,
    pub minting: bool,
}

pub struct StubCore {
    pub options: Options,
    pub calls: Mutex<Vec<Value>>,
    pub speak_calls: Mutex<Vec<Value>>,
    pub status_snapshots: Map<String, Value>,
    pub timeline: Vec<AgentTimelineItem>,
}

impl StubCore {
    pub fn new(options: Options) -> Arc<Self> {
        let transcripts = read_fixture("tool-catalog/tools-call-transcripts.json");
        let mut status_snapshots = Map::new();
        for t in transcripts["transcripts"].as_array().unwrap() {
            if t["tool"] == "get_agent_status" && t["response"]["isError"].is_null() {
                let id = t["args"]["agentId"].as_str().unwrap().to_owned();
                status_snapshots.insert(id, t["response"]["structuredContent"].clone());
            }
        }
        let timeline = transcripts["stubs"]["timeline"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| AgentTimelineItem::from_value(v).expect("timeline item"))
            .collect();
        Arc::new(Self {
            options,
            calls: Mutex::new(Vec::new()),
            speak_calls: Mutex::new(Vec::new()),
            status_snapshots,
            timeline,
        })
    }

    fn record(&self, target: &str, method: &str, args: Vec<Value>) {
        self.calls
            .lock()
            .unwrap()
            .push(json!({"target": target, "method": method, "args": args}));
    }

    pub fn take_calls(&self) -> Vec<Value> {
        std::mem::take(&mut *self.calls.lock().unwrap())
    }

    fn live(&self, id: &str) -> Option<AgentInfo> {
        let (lifecycle, status) = match id {
            LIVE_ID => ("idle", "idle"),
            RUNNING_ID => ("running", "running"),
            _ => return None,
        };
        let _ = status;
        Some(AgentInfo {
            id: id.to_owned(),
            provider: "claude".into(),
            cwd: "/work/repo".into(),
            workspace_id: Some("wks_0000000000000001".into()),
            lifecycle: lifecycle.into(),
            current_mode_id: Some("default".into()),
            available_modes: vec![],
            config: AgentConfigInfo::default(),
        })
    }

    fn live_payload(&self, id: &str, status: &str) -> Value {
        let mut snapshot = self.status_snapshots[LIVE_ID]["snapshot"].clone();
        snapshot["id"] = json!(id);
        snapshot["status"] = json!(status);
        snapshot
    }

    fn stored_payload(&self) -> Value {
        self.status_snapshots[STORED_ID]["snapshot"].clone()
    }
}

fn schedule(id: &str, new_agent: bool) -> Value {
    let target = if new_agent {
        json!({"type": "new-agent", "config": {"provider": "claude", "cwd": "/work/repo"}})
    } else {
        json!({"type": "agent", "agentId": LIVE_ID})
    };
    json!({
        "id": id,
        "name": "nightly",
        "prompt": "run tests",
        "cadence": {"type": "cron", "expression": "0 2 * * *", "timezone": "UTC"},
        "target": target,
        "status": "active",
        "createdAt": T0,
        "updatedAt": T0,
        "nextRunAt": T0,
        "lastRunAt": null,
        "pausedAt": null,
        "expiresAt": null,
        "maxRuns": null,
        "runs": [{
            "id": "run-1", "scheduledFor": T0, "startedAt": T0, "endedAt": T0,
            "status": "succeeded", "agentId": LIVE_ID, "workspaceId": null,
            "output": "ok", "error": null
        }],
    })
}

/// A persisted workspace record from `[id, project, cwd, kind, displayName]` plus the optional
/// `title`, `background` and `archivedAt` overrides.
fn workspace(base: [&str; 5], extra: Value) -> WorkspaceRecord {
    let [id, project, cwd, kind, name] = base;
    let mut record = json!({
        "workspaceId": id, "projectId": project, "cwd": cwd, "kind": kind,
        "displayName": name, "title": null, "createdAt": T0, "updatedAt": T0,
        "archivedAt": null, "background": false,
    });
    for (key, value) in extra.as_object().unwrap() {
        record[key] = value.clone();
    }
    serde_json::from_value(record).expect("workspace record")
}

fn provider_entries() -> Vec<ProviderEntry> {
    vec![
        ProviderEntry {
            provider: "claude".into(),
            label: Some("Claude".into()),
            description: Some("Anthropic Claude".into()),
            enabled: true,
            modes: Some(vec![
                json!({"id": "default", "label": "Default", "description": "Ask first"}),
                json!({"id": "bypassPermissions", "label": "Bypass", "description": "No prompts", "isUnattended": true}),
            ]),
            status: "ready".into(),
            error: None,
        },
        ProviderEntry {
            provider: "codex".into(),
            label: Some("Codex".into()),
            description: Some("OpenAI Codex".into()),
            enabled: false,
            modes: Some(vec![]),
            status: "unavailable".into(),
            error: None,
        },
    ]
}

fn terminals() -> Vec<TerminalSummary> {
    vec![
        TerminalSummary {
            id: "term-1".into(),
            name: "main".into(),
            cwd: "/work/repo".into(),
        },
        TerminalSummary {
            id: "term-2".into(),
            name: "logs".into(),
            cwd: "/work/other".into(),
        },
    ]
}

#[async_trait]
impl CoreApi for StubCore {
    fn has(&self, capability: Capability) -> bool {
        match capability {
            Capability::Terminals | Capability::Schedules | Capability::WorkspaceRegistry => true,
            Capability::BrowserBroker => self.options.browser,
            Capability::WorkspaceMinting => self.options.minting,
            _ => false,
        }
    }

    fn now_ms(&self) -> i64 {
        FIXED_NOW_MS
    }

    fn caller_context(&self, _id: &str) -> Option<CallerContext> {
        self.options.caller.then(|| CallerContext {
            enable_voice_tools: true,
            allow_custom_cwd: Some(true),
            ..CallerContext::default()
        })
    }

    fn get_agent(&self, id: &str) -> Option<AgentInfo> {
        self.live(id)
    }

    fn list_agent_permissions(&self) -> Vec<AgentPermissions> {
        vec![
            AgentPermissions {
                agent_id: LIVE_ID.into(),
                status: "idle".into(),
                requests: vec![],
            },
            AgentPermissions {
                agent_id: RUNNING_ID.into(),
                status: "running".into(),
                requests: vec![json!({
                    "id": "perm-1", "provider": "claude", "name": "Bash", "kind": "tool",
                    "title": "Run command", "input": {"command": "ls"}
                })],
            },
        ]
    }

    async fn agent_status_snapshot(
        &self,
        id: &str,
    ) -> Result<Option<AgentStatusSnapshot>, ToolError> {
        Ok(self.status_snapshots.get(id).map(|v| AgentStatusSnapshot {
            status: v["status"].as_str().unwrap().to_owned(),
            snapshot: v["snapshot"].clone(),
        }))
    }

    async fn list_agent_snapshots(
        &self,
        _bg: bool,
        _archived: bool,
    ) -> Result<Vec<Value>, ToolError> {
        Ok(vec![
            self.live_payload(LIVE_ID, "idle"),
            self.live_payload(RUNNING_ID, "running"),
            self.stored_payload(),
        ])
    }

    async fn ensure_agent_loaded(&self, _id: &str) -> Result<(), ToolError> {
        Ok(())
    }

    fn agent_timeline(&self, _id: &str) -> Vec<AgentTimelineItem> {
        self.timeline.clone()
    }

    async fn create_agent(&self, _r: CreateAgentRequest) -> Result<CreateAgentResult, ToolError> {
        Err(ToolError::new("create_agent is not stubbed"))
    }

    async fn send_prompt_to_agent(
        &self,
        _r: SendPromptRequest,
        _d: OnDispatch<'_>,
    ) -> Result<PromptDisposition, ToolError> {
        Err(ToolError::new("send_prompt is not stubbed"))
    }

    async fn wait_for_agent_run_start(&self, _id: &str) -> Result<(), ToolError> {
        Ok(())
    }

    fn setup_finish_notification(&self, _r: FinishNotificationRequest) -> StopNotification {
        Box::new(|| {})
    }

    async fn cancel_agent_run(&self, _id: &str) -> Result<bool, ToolError> {
        Ok(false)
    }

    async fn archive_agent(&self, _id: &str) -> Result<(), ToolError> {
        Ok(())
    }

    async fn close_agent(&self, _id: &str) -> Result<(), ToolError> {
        Ok(())
    }

    async fn update_agent_metadata(
        &self,
        _id: &str,
        _t: Option<String>,
        _l: Option<Map<String, Value>>,
    ) -> Result<(), ToolError> {
        Ok(())
    }

    async fn set_agent_mode(&self, id: &str, mode: &str) -> Result<(), ToolError> {
        self.record("agentManager", "setAgentMode", vec![json!(id), json!(mode)]);
        Ok(())
    }

    async fn set_agent_model(&self, _id: &str, _m: Option<&str>) -> Result<(), ToolError> {
        Ok(())
    }

    async fn set_agent_thinking_option(
        &self,
        _id: &str,
        _o: Option<&str>,
    ) -> Result<(), ToolError> {
        Ok(())
    }

    async fn set_agent_feature(&self, _id: &str, _f: &str, _v: &Value) -> Result<(), ToolError> {
        Ok(())
    }

    async fn respond_to_permission(
        &self,
        id: &str,
        request_id: &str,
        response: &Value,
    ) -> Result<(), ToolError> {
        self.record(
            "agentManager",
            "respondToPermission",
            vec![json!(id), json!(request_id), response.clone()],
        );
        Ok(())
    }

    async fn list_providers(&self) -> Result<Vec<ProviderEntry>, ToolError> {
        Ok(provider_entries())
    }

    async fn get_provider(&self, _cwd: &str, provider: &str) -> Result<ProviderEntry, ToolError> {
        provider_entries()
            .into_iter()
            .find(|e| e.provider == provider)
            .ok_or_else(|| ToolError::new(format!("Provider {provider} is not configured")))
    }

    async fn list_models(&self, provider: &str) -> Result<Vec<Value>, ToolError> {
        Ok(vec![
            json!({"provider": provider, "id": "model-a", "label": "Model A", "isDefault": true,
                   "thinkingOptions": [{"id": "high", "label": "High"}], "defaultThinkingOptionId": "high"}),
            json!({"provider": provider, "id": "model-b", "label": "Model B"}),
        ])
    }

    async fn list_draft_features(&self, request: Value) -> Result<Vec<Value>, ToolError> {
        self.record("agentManager", "listDraftFeatures", vec![request]);
        Ok(vec![
            json!({"type": "toggle", "id": "fast", "label": "Fast", "value": false}),
        ])
    }

    fn agent_profiles(&self) -> Vec<Value> {
        vec![
            json!({"id": "reviewer", "label": "Reviewer", "provider": "claude", "model": "model-a",
                    "modeId": "default", "notes": "Careful code review."}),
        ]
    }

    async fn list_workspaces(&self) -> Result<Vec<WorkspaceRecord>, ToolError> {
        Ok(vec![
            workspace(
                [
                    "wks_0000000000000001",
                    "prj_0000000000000001",
                    "/work/repo",
                    "local_checkout",
                    "repo",
                ],
                json!({}),
            ),
            workspace(
                [
                    "wks_0000000000000002",
                    "prj_0000000000000001",
                    "/work/repo-wt",
                    "worktree",
                    "repo-wt",
                ],
                json!({"title": "Feature"}),
            ),
            workspace(
                [
                    "wks_0000000000000003",
                    "prj_0000000000000002",
                    "/work/bg",
                    "directory",
                    "bg",
                ],
                json!({"background": true}),
            ),
            workspace(
                [
                    "wks_0000000000000004",
                    "prj_0000000000000002",
                    "/work/archived",
                    "directory",
                    "archived",
                ],
                json!({"archivedAt": T0}),
            ),
        ])
    }

    fn terminal_directories(&self) -> Vec<String> {
        vec!["/work/repo".into(), "/work/other".into()]
    }

    async fn get_terminals(&self, cwd: &str) -> Result<Vec<TerminalSummary>, ToolError> {
        Ok(terminals().into_iter().filter(|t| t.cwd == cwd).collect())
    }

    fn get_terminal(&self, id: &str) -> Option<TerminalSummary> {
        terminals().into_iter().find(|t| t.id == id)
    }

    async fn create_terminal(
        &self,
        cwd: &str,
        workspace_id: &str,
        name: Option<&str>,
    ) -> Result<TerminalSummary, ToolError> {
        self.record(
            "terminalManager",
            "createTerminal",
            vec![json!({"cwd": cwd, "workspaceId": workspace_id, "name": name})],
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
            lines: vec!["$ ls".into(), "a b c".into()],
            total_lines: 2,
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
        Ok(vec![schedule("sch-1", true), schedule("sch-hb", false)])
    }

    async fn schedule_inspect(&self, id: &str) -> Result<Value, ToolError> {
        match id {
            "sch-1" => Ok(schedule("sch-1", true)),
            "sch-hb" => Ok(schedule("sch-hb", false)),
            _ => Err(ToolError::new(format!("Schedule not found: {id}"))),
        }
    }

    async fn schedule_create_or_replace(&self, input: Value) -> Result<Value, ToolError> {
        self.record("scheduleService", "createOrReplace", vec![input.clone()]);
        let mut created = schedule("sch-new", true);
        created["name"] = input.get("name").cloned().unwrap_or(Value::Null);
        created["runs"] = json!([]);
        Ok(created)
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
        Ok(schedule("sch-1", true))
    }

    async fn schedule_update(&self, input: Value) -> Result<Value, ToolError> {
        self.record("scheduleService", "update", vec![input]);
        Ok(schedule("sch-1", true))
    }

    async fn schedule_logs(&self, _id: &str) -> Result<Vec<Value>, ToolError> {
        Ok(vec![])
    }

    fn has_speak_handler(&self, _id: &str) -> bool {
        self.options.caller
    }

    async fn speak(
        &self,
        caller: &str,
        text: &str,
        _cancel: CancellationToken,
    ) -> Result<(), ToolError> {
        self.speak_calls
            .lock()
            .unwrap()
            .push(json!({"text": text, "callerAgentId": caller}));
        Ok(())
    }

    async fn browser_execute(&self, request: Value) -> Result<Value, ToolError> {
        self.record("browserBroker", "execute", vec![request]);
        Ok(json!({"ok": true, "result": {}}))
    }
}

/// The six catalog configurations of the capture.
pub fn config_options(name: &str) -> (Options, paseo_tools::RuntimeCtx) {
    use paseo_core::PaseoToolPolicy;
    use paseo_tools::RuntimeCtx;
    let mut options = Options::default();
    let mut runtime = RuntimeCtx::default();
    match name {
        "full" => {
            options.browser = true;
            runtime.enable_voice = true;
        }
        "default" => {}
        "voice_only" => runtime.voice_only = true,
        "policy_disabled" => {
            runtime.policy = Some(PaseoToolPolicy::disabled());
            runtime.enable_voice = true;
        }
        "policy_disabled_tools" => {
            runtime.policy = Some(PaseoToolPolicy {
                enabled: None,
                disabled_tools: vec![
                    "create_agent".into(),
                    "kill_agent".into(),
                    "list_providers".into(),
                ],
            });
        }
        "caller_agent" => {
            options.caller = true;
            runtime.caller_agent_id = Some(LIVE_ID.into());
        }
        other => panic!("unknown config {other}"),
    }
    (options, runtime)
}

pub async fn build_catalog(core: Arc<StubCore>, runtime: paseo_tools::RuntimeCtx) -> ToolCatalog {
    ToolCatalogFactory::new(core).build(runtime).await
}
