//! Ports of the create, resume, import, config-preparation and provider-registry cases of
//! `agent-manager.test.ts`.

mod common;

use std::collections::HashSet;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use common::*;
use indexmap::IndexMap;
use paseo_agent_types::{AgentStreamEvent, AgentTimelineItem, PersistenceHandle};
use paseo_core::mock::{boxed, Gate, MockClient, MockSession, ScriptedBehavior};
use paseo_core::{
    AgentManager, CreateAgentOptions, ImportInput, ImportableQuery, ManagedAgentView, ManagerHooks,
    ManagerOptions, PaseoToolPolicy, PaseoToolsOptions, ProviderDefinition, ProviderRegistryUpdate,
    ReloadOptions, ResumeOptions, StartRunOptions, WaitForAgentOptions,
};
use paseo_provider_api::{
    AgentClient, AgentSessionConfig, ImportedSession, ImportedTimelineEntry, ProviderCatalog,
    ProviderError, ToolSpec,
};
use paseo_store::AgentLifecycle;
use serde_json::{json, Value};

async fn within<T>(work: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(10), work)
        .await
        .expect("timed out")
}

fn with_config(lc: &Lc, edit: impl FnOnce(&mut AgentSessionConfig)) -> AgentSessionConfig {
    let mut config = lc.config();
    edit(&mut config);
    config
}

fn mcp(value: Value) -> Option<Value> {
    Some(value)
}

/// A manager over a store with several providers (the TS `clients: {codex, claude}` shape).
async fn multi_provider(
    clients: &[(&str, Arc<MockClient>)],
    tweak: impl FnOnce(&mut ManagerOptions),
) -> (
    tempfile::TempDir,
    Arc<AgentManager>,
    Arc<paseo_store::AgentStore>,
) {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(paseo_store::AgentStore::new(dir.path().join("agents")));
    let mut update = ProviderRegistryUpdate::default();
    for (provider, client) in clients {
        update.clients.insert(
            (*provider).to_owned(),
            client.clone() as Arc<dyn AgentClient>,
        );
    }
    let counter = AtomicU32::new(0);
    let mut options = ManagerOptions {
        store: Some(store.clone()),
        providers: update,
        id_factory: Arc::new(move || id_n(counter.fetch_add(1, Ordering::SeqCst) + 1)),
        ..ManagerOptions::default()
    };
    tweak(&mut options);
    (dir, AgentManager::new(options), store)
}

// ---- normalization ----

#[tokio::test]
async fn normalize_config_injects_the_provider_default_model_while_leaving_mode_omitted() {
    let lc = Lc::new().await;
    let snapshot = lc.create().await;
    assert_eq!(snapshot.config.model.as_deref(), Some("gpt-5.4"));
    assert_eq!(snapshot.config.mode_id, None);
}

#[tokio::test]
async fn normalize_config_leaves_claude_mode_omitted() {
    let lc = Lc::with(
        MockClient::new("claude"),
        LcOptions {
            provider: "claude".into(),
            ..LcOptions::default()
        },
    )
    .await;
    let snapshot = lc
        .create_with(
            AgentSessionConfig::new("claude", lc.cwd()),
            CreateAgentOptions::default(),
        )
        .await;
    assert_eq!(snapshot.config.mode_id, None);
}

#[tokio::test]
async fn normalize_config_does_not_ask_the_provider_to_synthesize_an_omitted_mode() {
    let lc = Lc::new().await;
    let mut env = std::collections::BTreeMap::new();
    env.insert("CLAUDE_CODE_USE_BEDROCK".to_owned(), "1".to_owned());
    let snapshot = lc
        .create_with(
            lc.config(),
            CreateAgentOptions {
                env: Some(env),
                ..CreateAgentOptions::default()
            },
        )
        .await;
    assert_eq!(snapshot.config.mode_id, None);
    assert_eq!(lc.client.creates()[0].config.mode_id, None);
}

#[tokio::test]
async fn create_agent_forwards_request_env_into_the_provider_launch_context() {
    let lc = Lc::new().await;
    let mut env = std::collections::BTreeMap::new();
    env.insert("CHUNK14_PROBE".to_owned(), "expected".to_owned());
    let agent = lc
        .create_with(
            lc.config(),
            CreateAgentOptions {
                env: Some(env),
                ..CreateAgentOptions::default()
            },
        )
        .await;
    let context = lc.client.creates()[0].launch_context.clone().unwrap();
    let env = context.env.unwrap();
    assert_eq!(env["CHUNK14_PROBE"], "expected");
    assert_eq!(env["PASEO_AGENT_ID"], agent.id);
}

#[tokio::test]
async fn normalize_config_strips_legacy_default_model_id() {
    let lc = Lc::new().await;
    let snapshot = lc
        .create_with(
            with_config(&lc, |c| c.model = Some("default".into())),
            CreateAgentOptions::default(),
        )
        .await;
    assert_eq!(snapshot.config.model.as_deref(), Some("gpt-5.4"));
    assert_eq!(snapshot.config.mode_id, None);
}

#[tokio::test]
async fn normalize_config_keeps_the_model_unset_when_the_catalog_cannot_be_read() {
    let lc = Lc::new().await;
    lc.client.set_catalog_error("catalog unavailable");
    let snapshot = lc.create().await;
    assert_eq!(snapshot.config.model, None);
    lc.client.set_catalog(ProviderCatalog {
        models: vec![
            json!({"id": "first"}),
            json!({"id": "second", "isDefault": true}),
        ],
        ..ProviderCatalog::default()
    });
    let second = lc.create().await;
    assert_eq!(
        second.config.model.as_deref(),
        Some("second"),
        "default wins over first"
    );
    lc.client.set_catalog(ProviderCatalog {
        models: vec![json!({"id": "only-first"}), json!({"id": "other"})],
        ..ProviderCatalog::default()
    });
    assert_eq!(
        lc.create().await.config.model.as_deref(),
        Some("only-first")
    );
}

// ---- draft listings ----

#[tokio::test]
async fn list_draft_commands_returns_no_commands_without_guessing_a_missing_model() {
    let lc = Lc::new().await;
    let commands = lc.manager.list_draft_commands(lc.config()).await.unwrap();
    assert!(commands.is_empty());
    assert!(lc.client.catalog_calls().is_empty());
    assert!(lc.client.creates().is_empty());
    assert_eq!(lc.client.availability_calls(), 0);
}

#[tokio::test]
async fn list_draft_commands_uses_explicit_model_config_without_default_model_fetching() {
    let lc = Lc::new().await;
    let command = json!({"name": "review", "description": "Review changes", "argumentHint": "", "kind": "command"});
    lc.client.set_commands(vec![command.clone()]);
    let config = with_config(&lc, |c| c.model = Some("gpt-5.4".into()));

    let commands = lc.manager.list_draft_commands(config).await.unwrap();

    assert_eq!(commands, vec![command]);
    assert!(lc.client.catalog_calls().is_empty());
    // DEVIATION: the client lists commands itself; the TS fallback that opens a throwaway
    // session when the client has no `listCommands` belongs to the host adapter.
    let configs = lc.client.command_configs();
    assert_eq!(configs.len(), 1);
    assert_eq!(configs[0].provider, "codex");
    assert_eq!(configs[0].cwd, lc.cwd());
    assert_eq!(configs[0].model.as_deref(), Some("gpt-5.4"));
}

#[tokio::test]
async fn list_draft_features_does_not_start_a_fallback_session_without_a_model() {
    let lc = Lc::new().await;
    let features = lc.manager.list_draft_features(lc.config()).await.unwrap();
    assert!(features.is_empty());
    assert!(lc.client.catalog_calls().is_empty());
    assert!(lc.client.creates().is_empty());
}

#[tokio::test]
async fn list_draft_features_uses_client_feature_listing_without_a_model() {
    let lc = Lc::new().await;
    let feature =
        json!({"type": "toggle", "id": "auto_accept", "label": "Auto Accept", "value": false});
    lc.client.set_features(vec![feature.clone()]);

    let features = lc.manager.list_draft_features(lc.config()).await.unwrap();

    assert_eq!(features, vec![feature]);
    assert!(lc.client.catalog_calls().is_empty());
    assert!(lc.client.creates().is_empty());
    assert_eq!(lc.client.availability_calls(), 1);
    let configs = lc.client.feature_configs();
    assert_eq!(configs.len(), 1);
    assert_eq!(configs[0].model, None);
}

#[tokio::test]
async fn list_draft_features_uses_explicit_model_config_without_default_model_fetching() {
    let lc = Lc::new().await;
    let feature =
        json!({"type": "toggle", "id": "fast_mode", "label": "Fast mode", "value": false});
    lc.client.set_features(vec![feature.clone()]);
    let config = with_config(&lc, |c| c.model = Some("gpt-5.4".into()));

    let features = lc.manager.list_draft_features(config).await.unwrap();

    assert_eq!(features, vec![feature]);
    assert!(lc.client.catalog_calls().is_empty());
    assert!(lc.client.creates().is_empty());
    assert_eq!(
        lc.client.feature_configs()[0].model.as_deref(),
        Some("gpt-5.4")
    );
}

// ---- daemon prompt ----

#[tokio::test]
async fn create_agent_injects_daemon_append_system_prompt_at_runtime_only() {
    let lc = Lc::with(
        MockClient::new("codex"),
        LcOptions {
            tweak: Box::new(|o: &mut ManagerOptions| {
                o.paseo_tools.append_system_prompt = "  Daemon instructions.  ".into();
            }),
            ..LcOptions::default()
        },
    )
    .await;
    let snapshot = lc
        .create_with(
            with_config(&lc, |c| {
                c.system_prompt = Some("Agent instructions.".into())
            }),
            CreateAgentOptions::default(),
        )
        .await;
    let record = lc.record(&snapshot.id).await.unwrap();

    let created = &lc.client.creates()[0].config;
    assert_eq!(
        created.system_prompt.as_deref(),
        Some("Agent instructions.")
    );
    assert_eq!(
        created.daemon_append_system_prompt.as_deref(),
        Some("Daemon instructions.")
    );
    assert_eq!(snapshot.config.daemon_append_system_prompt, None);
    let stored = record.config.value().expect("stored config");
    assert_eq!(
        stored.system_prompt.value().map(String::as_str),
        Some("Agent instructions.")
    );
    let stored_json = serde_json::to_value(stored).unwrap();
    assert!(stored_json.get("daemonAppendSystemPrompt").is_none());
}

#[tokio::test]
async fn daemon_append_system_prompt_is_injected_into_pi_configs() {
    let lc = Lc::with(
        MockClient::new("pi"),
        LcOptions {
            provider: "pi".into(),
            definitions: true,
            tweak: Box::new(|o: &mut ManagerOptions| {
                o.paseo_tools.append_system_prompt = "Daemon instructions.".into();
            }),
            ..LcOptions::default()
        },
    )
    .await;
    let mut config = AgentSessionConfig::new("pi", lc.cwd());
    config.system_prompt = Some("Agent instructions.".into());
    lc.create_with(config, CreateAgentOptions::default()).await;
    assert_eq!(
        lc.client.creates()[0]
            .config
            .daemon_append_system_prompt
            .as_deref(),
        Some("Daemon instructions.")
    );
}

// ---- provider availability and registry ----

#[tokio::test]
async fn list_provider_availability_uses_registered_client_keys_including_custom_providers() {
    let lc = Lc::with(
        MockClient::new("zai"),
        LcOptions {
            provider: "zai".into(),
            ..LcOptions::default()
        },
    )
    .await;
    let availability = lc.manager.list_provider_availability().await;
    assert_eq!(
        availability,
        vec![paseo_core::ProviderAvailability {
            provider: "zai".into(),
            available: true,
            error: None
        }]
    );
}

#[tokio::test]
async fn create_agent_passes_daemon_launch_env_through_the_provider_launch_context() {
    let lc = Lc::new().await;
    let snapshot = lc.create().await;
    let call = &lc.client.creates()[0];
    assert_eq!(call.config.provider, "codex");
    assert_eq!(call.config.cwd, lc.cwd());
    assert_eq!(call.config.model.as_deref(), Some("gpt-5.4"));
    assert_eq!(
        serde_json::to_value(call.launch_context.as_ref().unwrap()).unwrap(),
        json!({"agentId": snapshot.id, "env": {"PASEO_AGENT_ID": snapshot.id, "PASEO_AGENT_CWD": lc.cwd()}})
    );
}

#[tokio::test]
async fn create_agent_passes_persist_session_to_provider_create_options() {
    let lc = Lc::new().await;
    lc.create_with(
        lc.config(),
        CreateAgentOptions {
            persist_session: Some(false),
            ..CreateAgentOptions::default()
        },
    )
    .await;
    let options = lc.client.creates()[0].options.clone().unwrap();
    assert_eq!(options.persist_session, Some(false));
    assert_eq!(options.purpose, None);
    let plain = Lc::new().await;
    plain.create().await;
    assert!(plain.client.creates()[0].options.is_none());
}

#[tokio::test]
async fn create_agent_persists_workspace_id_on_the_stored_record_and_emits_it_in_the_snapshot() {
    let lc = Lc::new().await;
    let agent = lc
        .create_with(
            lc.config(),
            CreateAgentOptions {
                workspace_id: Some("wks_owner".into()),
                ..CreateAgentOptions::default()
            },
        )
        .await;
    assert_eq!(agent.workspace_id.as_deref(), Some("wks_owner"));
    lc.sync().await;
    assert!(lc
        .events
        .states(&agent.id)
        .iter()
        .all(|s| s.workspace_id.as_deref() == Some("wks_owner")));
    assert_eq!(
        lc.record(&agent.id).await.unwrap().workspace_id.as_deref(),
        Some("wks_owner")
    );
}

// ---- paseo MCP injection ----

fn runtime_tools(url: Option<&str>) -> PaseoToolsOptions {
    PaseoToolsOptions {
        mcp_base_url: url.map(str::to_owned),
        ..PaseoToolsOptions::default()
    }
}

fn mcp_client(provider: &str) -> MockClient {
    MockClient::new(provider).with_capability("supportsMcpServers", true)
}

fn mcp_session(config: &AgentSessionConfig) -> MockSession {
    MockSession::builder("codex")
        .config(config)
        .auto_completing()
        .capability("supportsMcpServers", true)
        .build()
}

#[tokio::test]
async fn create_agent_injects_paseo_mcp_server_only_into_provider_launch_config() {
    let client = mcp_client("codex");
    client.on_create(|req| boxed(async move { Ok(mcp_session(&req.config)) }));
    let lc = Lc::with(
        client,
        LcOptions {
            tweak: Box::new(|o: &mut ManagerOptions| {
                o.paseo_tools = runtime_tools(Some("http://127.0.0.1:6767/mcp/agents"));
            }),
            ..LcOptions::default()
        },
    )
    .await;
    let custom = json!({"custom": {"type": "stdio", "command": "custom-mcp"}});
    let snapshot = lc
        .create_with(
            with_config(&lc, |c| c.mcp_servers = mcp(custom.clone())),
            CreateAgentOptions::default(),
        )
        .await;

    assert_eq!(snapshot.config.mcp_servers, Some(custom.clone()));
    let launched = lc.client.creates()[0].config.mcp_servers.clone().unwrap();
    assert_eq!(
        launched,
        json!({
            "paseo": {"type": "http", "url": format!("http://127.0.0.1:6767/mcp/agents?callerAgentId={}", snapshot.id)},
            "custom": {"type": "stdio", "command": "custom-mcp"}
        })
    );
    assert_eq!(
        launched.as_object().unwrap().keys().next().unwrap(),
        "paseo"
    );
    let stored = lc.record(&snapshot.id).await.unwrap();
    assert_eq!(
        stored
            .config
            .value()
            .unwrap()
            .mcp_servers
            .value()
            .cloned()
            .map(Value::Object),
        Some(custom)
    );
}

#[tokio::test]
async fn create_agent_closes_and_rejects_a_provider_session_that_cannot_honor_mcp_servers() {
    let client = MockClient::new("codex");
    let created: Arc<Mutex<Option<MockSession>>> = Arc::default();
    {
        let created = created.clone();
        client.on_create(move |req| {
            let created = created.clone();
            boxed(async move {
                let session = MockSession::builder("codex")
                    .config(&req.config)
                    .auto_completing()
                    .build();
                *created.lock().unwrap() = Some(session.clone());
                Ok(session)
            })
        });
    }
    let lc = Lc::with_client(client).await;
    let error = lc
        .manager
        .create_agent(
            with_config(&lc, |c| {
                c.mcp_servers = mcp(json!({"hub": {"type": "http", "url": "http://127.0.0.1:3000/api/executions/test/mcp"}}));
            }),
            None,
            CreateAgentOptions::default(),
        )
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "Provider 'codex' does not support MCP servers"
    );

    let session = created.lock().unwrap().clone().unwrap();
    assert!(session.is_closed());
    assert_eq!(session.start_count(), 0);
    assert!(lc.manager.get_agent(&id_n(1)).is_none());
    assert!(lc.record(&id_n(1)).await.is_none());
}

#[tokio::test]
async fn resume_agent_from_persistence_closes_and_rejects_a_session_that_cannot_honor_external_mcp()
{
    let client = MockClient::new("codex");
    let replacement: Arc<Mutex<Option<MockSession>>> = Arc::default();
    {
        let replacement = replacement.clone();
        client.on_resume(move |_| {
            let replacement = replacement.clone();
            boxed(async move {
                let session = MockSession::builder("codex").auto_completing().build();
                *replacement.lock().unwrap() = Some(session.clone());
                Ok(session)
            })
        });
    }
    let lc = Lc::with_client(client).await;
    let agent_id = id_n(0x108);
    let error = lc
        .manager
        .resume_agent_from_persistence(
            handle("codex", "persist-unsupported-mcp", &lc.cwd()),
            Some(obj(json!({
                "cwd": lc.cwd(),
                "mcpServers": {"hub": {"type": "http", "url": "https://hub.test/mcp/executions/resume"}}
            }))),
            Some(&agent_id),
            ResumeOptions::default(),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "Provider 'codex' does not support MCP servers"
    );
    assert!(replacement.lock().unwrap().as_ref().unwrap().is_closed());
    assert!(lc.manager.get_agent(&agent_id).is_none());
    assert!(lc.record(&agent_id).await.is_none());
}

#[tokio::test]
async fn reload_agent_session_preserves_the_live_session_when_its_replacement_cannot_honor_external_mcp(
) {
    let policy: Arc<Mutex<PaseoToolPolicy>> = Arc::new(Mutex::new(PaseoToolPolicy {
        enabled: None,
        disabled_tools: vec!["list_agents".into()],
    }));
    let lc = {
        let policy = policy.clone();
        Lc::with(
            MockClient::new("codex"),
            LcOptions {
                tweak: Box::new(move |o: &mut ManagerOptions| {
                    o.paseo_tools.resolve_policy =
                        Some(Arc::new(move |_| Some(policy.lock().unwrap().clone())));
                }),
                ..LcOptions::default()
            },
        )
        .await
    };
    let created = lc.create().await;
    *policy.lock().unwrap() = PaseoToolPolicy {
        enabled: None,
        disabled_tools: vec!["create_agent".into()],
    };

    let error = lc
        .manager
        .reload_agent_session(
            &created.id,
            Some(obj(json!({"mcpServers": {"hub": {"type": "http", "url": "https://hub.test/mcp/executions/reload"}}}))),
            ReloadOptions::default(),
        )
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "Provider 'codex' does not support MCP servers"
    );

    assert!(lc.client.resumes().is_empty());
    assert!(!lc.client.created_sessions()[0].is_closed());
    let live = lc.manager.get_agent(&created.id).unwrap();
    assert_eq!(live.lifecycle, AgentLifecycle::Idle);
    assert_eq!(
        lc.manager
            .paseo_tool_policy(&created.id)
            .unwrap()
            .disabled_tools,
        vec!["list_agents".to_owned()]
    );
}

/// Hooks that offer a native tool catalog and record who asked.
struct ToolHooks {
    asked: Mutex<Vec<String>>,
}

#[async_trait]
impl ManagerHooks for ToolHooks {
    async fn paseo_tool_catalog(&self, agent_id: &str) -> Option<Vec<ToolSpec>> {
        self.asked.lock().unwrap().push(agent_id.to_owned());
        Some(vec![ToolSpec {
            name: "list_agents".into(),
            description: "List agents".into(),
            title: None,
            input_schema: None,
        }])
    }
}

#[tokio::test]
async fn create_agent_passes_native_paseo_tools_through_launch_context_without_internal_mcp() {
    let client = mcp_client("codex").with_capability("supportsNativePaseoTools", true);
    client.on_create(|req| boxed(async move { Ok(mcp_session(&req.config)) }));
    let hooks = Arc::new(ToolHooks {
        asked: Mutex::default(),
    });
    let lc = {
        let hooks = hooks.clone();
        Lc::with(
            client,
            LcOptions {
                tweak: Box::new(move |o: &mut ManagerOptions| {
                    o.hooks = hooks;
                    o.paseo_tools = runtime_tools(Some("http://127.0.0.1:6767/mcp/agents"));
                }),
                ..LcOptions::default()
            },
        )
        .await
    };
    let custom = json!({"custom": {"type": "stdio", "command": "custom-mcp"}});
    let snapshot = lc
        .create_with(
            with_config(&lc, |c| c.mcp_servers = mcp(custom.clone())),
            CreateAgentOptions::default(),
        )
        .await;

    let call = &lc.client.creates()[0];
    assert_eq!(
        call.launch_context
            .as_ref()
            .unwrap()
            .paseo_tools
            .as_ref()
            .unwrap()[0]
            .name,
        "list_agents"
    );
    assert_eq!(call.config.mcp_servers, Some(custom.clone()));
    assert_eq!(snapshot.config.mcp_servers, Some(custom));
    assert_eq!(*hooks.asked.lock().unwrap(), vec![snapshot.id.clone()]);
}

#[tokio::test]
async fn create_agent_allows_best_effort_internal_mcp_when_the_provider_session_reports_no_support()
{
    let lc = Lc::with(
        MockClient::new("codex"),
        LcOptions {
            tweak: Box::new(|o: &mut ManagerOptions| {
                o.paseo_tools = PaseoToolsOptions {
                    mcp_base_url: Some("http://127.0.0.1:6767/mcp/agents".into()),
                    mcp_auth_token: Some("cap-token".into()),
                    ..PaseoToolsOptions::default()
                };
            }),
            ..LcOptions::default()
        },
    )
    .await;
    let snapshot = lc.create().await;
    let servers = lc.client.creates()[0].config.mcp_servers.clone().unwrap();
    assert_eq!(
        servers["paseo"],
        json!({
            "type": "http",
            "url": format!("http://127.0.0.1:6767/mcp/agents?callerAgentId={}", snapshot.id),
            "headers": {"Authorization": "Bearer cap-token"}
        })
    );
    assert_eq!(
        snapshot.config.mcp_servers, None,
        "the injected entry is never stored"
    );
}

#[tokio::test]
async fn uses_each_providers_current_policy_for_new_sessions_and_snapshots_it_by_agent() {
    let codex = Arc::new(mcp_client("codex").with_capability("supportsNativePaseoTools", true));
    let claude = Arc::new(mcp_client("claude").with_capability("supportsNativePaseoTools", true));
    let policies: Arc<Mutex<IndexMap<String, PaseoToolPolicy>>> =
        Arc::new(Mutex::new(IndexMap::from([
            (
                "codex".to_owned(),
                PaseoToolPolicy {
                    enabled: None,
                    disabled_tools: vec!["list_agents".into()],
                },
            ),
            (
                "claude".to_owned(),
                PaseoToolPolicy {
                    enabled: Some(false),
                    disabled_tools: vec![],
                },
            ),
        ])));
    let hooks = Arc::new(ToolHooks {
        asked: Mutex::default(),
    });
    let (dir, manager, _store) = {
        let (policies, hooks) = (policies.clone(), hooks.clone());
        multi_provider(
            &[("codex", codex.clone()), ("claude", claude.clone())],
            move |o| {
                o.hooks = hooks;
                o.paseo_tools = PaseoToolsOptions {
                    mcp_base_url: Some("http://127.0.0.1:6767/mcp/agents".into()),
                    resolve_policy: Some(Arc::new(move |provider| {
                        policies.lock().unwrap().get(provider).cloned()
                    })),
                    ..PaseoToolsOptions::default()
                };
            },
        )
        .await
    };
    let cwd = dir.path().to_string_lossy().into_owned();
    let codex_agent = manager
        .create_agent(
            AgentSessionConfig::new("codex", &cwd),
            Some(&id_n(0x107)),
            CreateAgentOptions::default(),
        )
        .await
        .unwrap();
    let claude_agent = manager
        .create_agent(
            AgentSessionConfig::new("claude", &cwd),
            Some(&id_n(0x108)),
            CreateAgentOptions::default(),
        )
        .await
        .unwrap();

    assert_eq!(*hooks.asked.lock().unwrap(), vec![codex_agent.id.clone()]);
    assert!(codex.creates()[0]
        .launch_context
        .as_ref()
        .unwrap()
        .paseo_tools
        .is_some());
    assert!(claude.creates()[0]
        .launch_context
        .as_ref()
        .unwrap()
        .paseo_tools
        .is_none());
    assert!(
        codex.creates()[0].config.mcp_servers.is_none(),
        "native tools replace the MCP entry"
    );
    assert!(claude.creates()[0].config.mcp_servers.is_none());
    assert_eq!(
        manager
            .paseo_tool_policy(&codex_agent.id)
            .unwrap()
            .disabled_tools,
        vec!["list_agents".to_owned()]
    );
    assert_eq!(
        manager.paseo_tool_policy(&claude_agent.id).unwrap().enabled,
        Some(false)
    );

    policies.lock().unwrap().insert(
        "codex".into(),
        PaseoToolPolicy {
            enabled: None,
            disabled_tools: vec!["create_agent".into()],
        },
    );
    let next = manager
        .create_agent(
            AgentSessionConfig::new("codex", &cwd),
            Some(&id_n(0x111)),
            CreateAgentOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(
        manager
            .paseo_tool_policy(&codex_agent.id)
            .unwrap()
            .disabled_tools,
        vec!["list_agents".to_owned()]
    );
    assert_eq!(
        manager.paseo_tool_policy(&next.id).unwrap().disabled_tools,
        vec!["create_agent".to_owned()]
    );
    assert_eq!(
        *hooks.asked.lock().unwrap(),
        vec![codex_agent.id.clone(), next.id.clone()]
    );

    manager.archive_agent(&claude_agent.id).await.unwrap();
    assert!(manager.paseo_tool_policy(&claude_agent.id).is_none());
}

#[tokio::test]
async fn keeps_the_global_paseo_tools_gate_outside_provider_policy_and_mcp_injection() {
    let enabled = Lc::with(
        mcp_client("codex"),
        LcOptions {
            tweak: Box::new(|o: &mut ManagerOptions| {
                o.paseo_tools = PaseoToolsOptions {
                    mcp_base_url: Some("http://127.0.0.1:6767/mcp/agents".into()),
                    resolve_policy: Some(Arc::new(|_| {
                        Some(PaseoToolPolicy {
                            enabled: None,
                            disabled_tools: vec!["list_agents".into()],
                        })
                    })),
                    ..PaseoToolsOptions::default()
                };
            }),
            ..LcOptions::default()
        },
    )
    .await;
    let enabled_agent = enabled.create().await;
    assert_eq!(
        enabled.client.creates()[0]
            .config
            .mcp_servers
            .as_ref()
            .unwrap()["paseo"],
        json!({"type": "http", "url": format!("http://127.0.0.1:6767/mcp/agents?callerAgentId={}", enabled_agent.id)})
    );

    let hooks = Arc::new(ToolHooks {
        asked: Mutex::default(),
    });
    let disabled = {
        let hooks = hooks.clone();
        Lc::with(
            mcp_client("codex"),
            LcOptions {
                tweak: Box::new(move |o: &mut ManagerOptions| {
                    o.hooks = hooks;
                    o.paseo_tools = PaseoToolsOptions {
                        enabled: false,
                        mcp_base_url: Some("http://127.0.0.1:6767/mcp/agents".into()),
                        resolve_policy: Some(Arc::new(|_| {
                            Some(PaseoToolPolicy {
                                enabled: Some(true),
                                disabled_tools: vec![],
                            })
                        })),
                        ..PaseoToolsOptions::default()
                    };
                }),
                ..LcOptions::default()
            },
        )
        .await
    };
    let disabled_agent = disabled.create().await;
    assert!(disabled.client.creates()[0].config.mcp_servers.is_none());
    assert!(hooks.asked.lock().unwrap().is_empty());
    assert_eq!(
        disabled
            .manager
            .paseo_tool_policy(&disabled_agent.id)
            .unwrap()
            .enabled,
        Some(false)
    );
}

#[tokio::test]
async fn resume_agent_from_persistence_replaces_stored_internal_paseo_mcp_with_current_runtime_url()
{
    let lc = Lc::with(
        mcp_client("codex"),
        LcOptions {
            tweak: Box::new(|o: &mut ManagerOptions| {
                o.paseo_tools = runtime_tools(Some("http://127.0.0.1:6768/mcp/agents"));
            }),
            ..LcOptions::default()
        },
    )
    .await;
    let snapshot = lc
        .manager
        .resume_agent_from_persistence(
            handle("codex", "session-123", &lc.cwd()),
            Some(obj(json!({
                "cwd": lc.cwd(),
                "mcpServers": {
                    "paseo": {"type": "http", "url": "http://127.0.0.1:6767/mcp/agents?callerAgentId=stale-agent"},
                    "custom": {"type": "stdio", "command": "custom-mcp"}
                }
            }))),
            None,
            ResumeOptions::default(),
            None,
        )
        .await
        .unwrap();

    let overrides = lc.client.resumes()[0].overrides.clone().unwrap();
    assert_eq!(
        overrides["mcpServers"],
        json!({
            "paseo": {"type": "http", "url": format!("http://127.0.0.1:6768/mcp/agents?callerAgentId={}", snapshot.id)},
            "custom": {"type": "stdio", "command": "custom-mcp"}
        })
    );
    assert_eq!(
        snapshot.config.mcp_servers,
        Some(json!({"custom": {"type": "stdio", "command": "custom-mcp"}}))
    );
}

#[tokio::test]
async fn resume_agent_from_persistence_drops_stored_internal_paseo_mcp_when_runtime_injection_is_disabled(
) {
    let lc = Lc::new().await;
    let snapshot = lc
        .manager
        .resume_agent_from_persistence(
            handle("codex", "session-123", &lc.cwd()),
            Some(obj(json!({
                "cwd": lc.cwd(),
                "mcpServers": {"paseo": {"type": "http", "url": "http://127.0.0.1:6767/mcp/agents?callerAgentId=stale-agent"}}
            }))),
            None,
            ResumeOptions::default(),
            None,
        )
        .await
        .unwrap();
    assert!(lc.client.resumes()[0]
        .overrides
        .as_ref()
        .unwrap()
        .get("mcpServers")
        .is_none());
    assert_eq!(snapshot.config.mcp_servers, None);
}

#[tokio::test]
async fn create_agent_preserves_a_user_provided_paseo_mcp_config() {
    let client = mcp_client("codex");
    client.on_create(|req| boxed(async move { Ok(mcp_session(&req.config)) }));
    let lc = Lc::with(
        client,
        LcOptions {
            tweak: Box::new(|o: &mut ManagerOptions| {
                o.paseo_tools = runtime_tools(Some("http://127.0.0.1:6767/mcp/agents"));
            }),
            ..LcOptions::default()
        },
    )
    .await;
    let user = json!({"paseo": {"type": "http", "url": "https://example.com/custom-paseo"}});
    let snapshot = lc
        .create_with(
            with_config(&lc, |c| c.mcp_servers = mcp(user.clone())),
            CreateAgentOptions::default(),
        )
        .await;
    assert_eq!(snapshot.config.mcp_servers, Some(user.clone()));
    assert_eq!(lc.client.creates()[0].config.mcp_servers, Some(user));
}

#[tokio::test]
async fn tool_policy_preapprovals_must_reference_a_server_of_the_same_request() {
    let lc = Lc::new().await;
    let error = lc
        .manager
        .create_agent(
            with_config(&lc, |c| {
                c.tool_policy =
                    Some(json!({"preapproved": [{"kind": "mcp", "server": "hub", "tool": "run"}]}));
            }),
            None,
            CreateAgentOptions::default(),
        )
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "toolPolicy preapproval 'hub.run' requires MCP server 'hub' in the same agent request"
    );
    let error = lc
        .manager
        .create_agent(
            with_config(&lc, |c| {
                c.tool_policy =
                    Some(json!({"preapproved": [{"kind": "mcp", "server": "hub", "tool": "run"}]}));
                c.mcp_servers = mcp(json!({"hub": {"type": "stdio", "command": "x"}}));
            }),
            None,
            CreateAgentOptions::default(),
        )
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "Provider 'codex' cannot preapprove exact MCP tools for unattended execution"
    );
}

// ---- create failures ----

#[tokio::test]
async fn create_agent_fails_when_cwd_does_not_exist() {
    let lc = Lc::new().await;
    let error = lc
        .manager
        .create_agent(
            AgentSessionConfig::new("codex", format!("{}/does-not-exist", lc.cwd())),
            None,
            CreateAgentOptions::default(),
        )
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("Working directory does not exist"),
        "{error}"
    );

    let file = lc.dir.path().join("a-file");
    std::fs::write(&file, "x").unwrap();
    let error = lc
        .manager
        .create_agent(
            AgentSessionConfig::new("codex", file.to_string_lossy()),
            None,
            CreateAgentOptions::default(),
        )
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("Working directory is not a directory"),
        "{error}"
    );
}

#[tokio::test]
async fn create_agent_reports_configured_providers_when_provider_is_unknown() {
    let lc = Lc::new().await;
    let error = lc
        .manager
        .create_agent(
            AgentSessionConfig::new("missing-provider", lc.cwd()),
            None,
            CreateAgentOptions::default(),
        )
        .await
        .unwrap_err();
    assert!(error
        .to_string()
        .contains("Unknown provider 'missing-provider'. Configured providers: codex."));
}

#[tokio::test]
async fn create_agent_reports_available_providers_when_selected_provider_is_unavailable() {
    let codex = Arc::new(MockClient::new("codex"));
    codex.set_available(false);
    let claude = Arc::new(MockClient::new("claude"));
    let (dir, manager, _) = multi_provider(&[("codex", codex), ("claude", claude)], |_| {}).await;
    let error = manager
        .create_agent(
            AgentSessionConfig::new("codex", dir.path().to_string_lossy()),
            None,
            CreateAgentOptions::default(),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains(
        "Provider 'codex' is not available. Available providers: claude. Use one of those providers, or install/configure 'codex'."
    ), "{error}");

    let failing = Arc::new(MockClient::new("flaky"));
    failing.set_availability_error("probe exploded");
    let (dir, manager, _) = multi_provider(&[("flaky", failing)], |_| {}).await;
    let error = manager
        .create_agent(
            AgentSessionConfig::new("flaky", dir.path().to_string_lossy()),
            None,
            CreateAgentOptions::default(),
        )
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("Reason: probe exploded."),
        "{error}"
    );
    assert!(
        error.to_string().contains("Available providers: none."),
        "{error}"
    );
}

#[tokio::test]
async fn create_agent_rejects_a_disabled_provider_without_creating_a_session() {
    let client = Arc::new(MockClient::new("codex"));
    let (dir, manager, store) = {
        let client = client.clone();
        multi_provider(&[("codex", client)], |o| {
            o.providers
                .definitions
                .insert("codex".into(), ProviderDefinition::disabled());
        })
        .await
    };
    let error = manager
        .create_agent(
            AgentSessionConfig::new("codex", dir.path().to_string_lossy()),
            None,
            CreateAgentOptions::default(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "Provider 'codex' is disabled");
    assert!(client.creates().is_empty());
    assert!(store.list().await.unwrap().is_empty());
}

fn registry(
    clients: &[(&str, &Arc<MockClient>)],
    enabled: &[(&str, bool)],
) -> ProviderRegistryUpdate {
    let mut update = ProviderRegistryUpdate::default();
    for (provider, client) in clients {
        update.clients.insert(
            (*provider).to_owned(),
            (*client).clone() as Arc<dyn AgentClient>,
        );
    }
    for (provider, on) in enabled {
        update.definitions.insert(
            (*provider).to_owned(),
            if *on {
                ProviderDefinition::enabled()
            } else {
                ProviderDefinition::disabled()
            },
        );
    }
    update
}

#[tokio::test]
async fn update_provider_registry_re_enables_a_previously_disabled_provider() {
    let client = Arc::new(MockClient::new("codex"));
    let (dir, manager, _) = {
        let client = client.clone();
        multi_provider(&[("codex", client)], |o| {
            o.providers
                .definitions
                .insert("codex".into(), ProviderDefinition::disabled());
        })
        .await
    };
    let cwd = dir.path().to_string_lossy().into_owned();
    let error = manager
        .create_agent(
            AgentSessionConfig::new("codex", &cwd),
            None,
            CreateAgentOptions::default(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "Provider 'codex' is disabled");

    manager.update_provider_registry(registry(&[("codex", &client)], &[("codex", true)]));
    let snapshot = manager
        .create_agent(
            AgentSessionConfig::new("codex", &cwd),
            None,
            CreateAgentOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(snapshot.config.provider, "codex");
}

#[tokio::test]
async fn update_provider_registry_disables_a_previously_enabled_provider() {
    let client = Arc::new(MockClient::new("codex"));
    let (dir, manager, _) = multi_provider(&[("codex", client.clone())], |o| {
        o.providers
            .definitions
            .insert("codex".into(), ProviderDefinition::enabled());
    })
    .await;
    manager.update_provider_registry(registry(&[("codex", &client)], &[("codex", false)]));
    let error = manager
        .create_agent(
            AgentSessionConfig::new("codex", dir.path().to_string_lossy()),
            None,
            CreateAgentOptions::default(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "Provider 'codex' is disabled");
}

#[tokio::test]
async fn update_provider_registry_registers_a_previously_unknown_provider() {
    let (dir, manager, _) = multi_provider(&[], |_| {}).await;
    assert!(!manager
        .registered_provider_ids()
        .contains(&"codex".to_owned()));
    let client = Arc::new(MockClient::new("codex"));
    manager.update_provider_registry(registry(&[("codex", &client)], &[("codex", true)]));
    assert!(manager
        .registered_provider_ids()
        .contains(&"codex".to_owned()));
    let snapshot = manager
        .create_agent(
            AgentSessionConfig::new("codex", dir.path().to_string_lossy()),
            None,
            CreateAgentOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(snapshot.config.provider, "codex");
}

#[tokio::test]
async fn update_provider_registry_removes_providers_omitted_from_the_next_registry() {
    let codex = Arc::new(MockClient::new("codex"));
    let removed = Arc::new(MockClient::new("zai-claude"));
    let (dir, manager, _) = multi_provider(
        &[("codex", codex.clone()), ("zai-claude", removed.clone())],
        |o| {
            o.providers
                .definitions
                .insert("codex".into(), ProviderDefinition::enabled());
            o.providers
                .definitions
                .insert("zai-claude".into(), ProviderDefinition::enabled());
        },
    )
    .await;
    assert!(manager
        .registered_provider_ids()
        .contains(&"zai-claude".to_owned()));

    manager.update_provider_registry(registry(&[("codex", &codex)], &[("codex", true)]));

    assert!(!manager
        .registered_provider_ids()
        .contains(&"zai-claude".to_owned()));
    let error = manager
        .create_agent(
            AgentSessionConfig::new("zai-claude", dir.path().to_string_lossy()),
            None,
            CreateAgentOptions::default(),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("Unknown provider 'zai-claude'"));
    assert!(removed.creates().is_empty());
}

// ---- plugin provider replacement and stale sessions ----

fn plugin_provider() -> &'static str {
    "plugin-provider"
}

/// The replacement client of `ResumeTrackingTestAgentClient`: each resumed session opens `gate`
/// when a turn starts.
fn resume_tracking_client(provider: &str, gate: &Gate) -> MockClient {
    let client = MockClient::new(provider);
    let (gate, provider) = (gate.clone(), provider.to_owned());
    client.on_resume(move |req| {
        let (gate, provider) = (gate.clone(), provider.clone());
        boxed(async move {
            let session = MockSession::builder(&provider)
                .behavior(
                    ScriptedBehavior::new().on_start(move |session, prompt, options| {
                        let gate = gate.clone();
                        boxed(async move {
                            gate.open();
                            Ok(session.auto_start(prompt, options))
                        })
                    }),
                )
                .build();
            session.set_persistence(req.handle.clone());
            Ok(session)
        })
    });
    client
}

fn stale_error() -> ProviderError {
    ProviderError::provider("Provider session stale-bridge is stale after its plugin reloaded")
}

async fn plugin_lc(client: MockClient) -> Lc {
    Lc::with(
        client,
        LcOptions {
            provider: plugin_provider().into(),
            definitions: true,
            ..LcOptions::default()
        },
    )
    .await
}

fn swap_client(lc: &Lc, replacement: &Arc<MockClient>, retired: &[&str]) {
    let mut update = registry(
        &[(plugin_provider(), replacement)],
        &[(plugin_provider(), true)],
    );
    update.retired_providers = retired.iter().map(|p| (*p).to_owned()).collect();
    lc.manager.update_provider_registry(update);
}

#[tokio::test]
async fn retires_loaded_agents_when_their_plugin_provider_is_replaced() {
    let original = MockClient::new(plugin_provider());
    original.on_create(|req| {
        boxed(async move {
            let session = MockSession::builder(plugin_provider())
                .config(&req.config)
                .auto_completing()
                .build();
            session.set_persistence(PersistenceHandle {
                provider: plugin_provider().into(),
                session_id: "plugin-session".into(),
                native_handle: None,
                metadata: None,
            });
            Ok(session)
        })
    });
    let lc = plugin_lc(original).await;
    let created = lc
        .create_with(
            AgentSessionConfig::new(plugin_provider(), lc.cwd()),
            CreateAgentOptions::default(),
        )
        .await;
    let replacement = Arc::new(MockClient::new(plugin_provider()));

    swap_client(&lc, &replacement, &[plugin_provider()]);
    lc.manager.wait_for_agent_close(&created.id).await;
    settle().await;
    lc.manager.wait_for_agent_close(&created.id).await;

    assert!(lc.client.created_sessions()[0].is_closed());
    assert!(lc.manager.get_agent(&created.id).is_none());
    let resumed = lc.load(&created.id).await.unwrap();
    assert_eq!(resumed.id, created.id);
    let resumes = replacement.resumes();
    assert_eq!(resumes.len(), 1);
    let overrides = resumes[0].overrides.as_ref().unwrap();
    assert_eq!(overrides["cwd"], json!(lc.cwd()));
    assert_eq!(overrides["provider"], json!(plugin_provider()));
}

#[tokio::test]
async fn a_prompt_after_provider_replacement_reopens_the_stale_session() {
    let stale = MockClient::new(plugin_provider());
    stale.on_create(|req| {
        boxed(async move {
            let session = MockSession::builder(plugin_provider())
                .config(&req.config)
                .behavior(
                    ScriptedBehavior::new().on_start(|_, _, _| boxed(async { Err(stale_error()) })),
                )
                .build();
            session.set_persistence(PersistenceHandle {
                provider: plugin_provider().into(),
                session_id: "plugin-session".into(),
                native_handle: None,
                metadata: None,
            });
            Ok(session)
        })
    });
    let lc = plugin_lc(stale).await;
    let created = lc
        .create_with(
            AgentSessionConfig::new(plugin_provider(), lc.cwd()),
            CreateAgentOptions::default(),
        )
        .await;
    let retry_started = Gate::new();
    let replacement = Arc::new(resume_tracking_client(plugin_provider(), &retry_started));
    swap_client(&lc, &replacement, &[]);

    let disposition = lc
        .manager
        .start_agent_run(
            &created.id,
            "continue after reload".into(),
            StartRunOptions {
                run: paseo_core::RunOpts::with_client_message_id("stale-session-retry"),
                ..StartRunOptions::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(disposition, paseo_core::PromptDisposition::TurnStarted);
    within(retry_started.wait()).await;
    let result = lc
        .manager
        .wait_for_agent_event(&created.id, WaitForAgentOptions::default())
        .await
        .unwrap();
    assert_eq!(result.status, AgentLifecycle::Idle);
    assert_eq!(replacement.resumes().len(), 1);
    let timeline = lc.items(&created.id).await;
    assert_eq!(user_message_texts(&timeline), vec!["continue after reload"]);
    assert!(has_user_message(&timeline, "stale-session-retry"));
    assert!(!timeline.iter().any(|item| matches!(
        item,
        AgentTimelineItem::AssistantMessage { text, .. } if text.contains("[System Error]")
    )));
}

#[tokio::test]
async fn a_prompt_after_a_timed_out_reload_close_automatically_reloads_and_recovers() {
    let release_close = Gate::new();
    let client = MockClient::new("claude");
    {
        let release = release_close.clone();
        client.on_create(move |req| {
            let release = release.clone();
            boxed(async move {
                let session = MockSession::builder("claude")
                    .config(&req.config)
                    .behavior(
                        ScriptedBehavior::new()
                            .on_start(|session, prompt, options| {
                                boxed(async move {
                                    if session.is_closed() {
                                        return Err(ProviderError::provider(
                                            "Claude session is closed",
                                        ));
                                    }
                                    Ok(session.auto_start(prompt, options))
                                })
                            })
                            .on_close(move |session| {
                                let release = release.clone();
                                boxed(async move {
                                    session.with_state(|s| s.closed = true);
                                    release.wait().await;
                                    Ok(())
                                })
                            }),
                    )
                    .build();
                session.set_persistence(PersistenceHandle {
                    provider: "claude".into(),
                    session_id: "claude-persisted-session".into(),
                    native_handle: None,
                    metadata: None,
                });
                Ok(session)
            })
        });
    }
    let lc = Lc::with(
        client,
        LcOptions {
            provider: "claude".into(),
            definitions: true,
            tweak: Box::new(|o: &mut ManagerOptions| {
                o.reload_session_close_timeout = Duration::from_millis(10);
            }),
            ..LcOptions::default()
        },
    )
    .await;
    let created = lc
        .create_with(
            AgentSessionConfig::new("claude", lc.cwd()),
            CreateAgentOptions::default(),
        )
        .await;
    let retry_started = Gate::new();
    let replacement = Arc::new(resume_tracking_client("claude", &retry_started));
    // The retired runtime's resume goes to the replacement client.
    let mut update = ProviderRegistryUpdate::default();
    update
        .clients
        .insert("claude".into(), replacement.clone() as Arc<dyn AgentClient>);
    update
        .definitions
        .insert("claude".into(), ProviderDefinition::enabled());

    let error = lc
        .manager
        .reload_agent_session(&created.id, None, ReloadOptions::default())
        .await
        .unwrap_err();
    assert!(error
        .to_string()
        .contains("Timed out closing previous session during refresh"));
    release_close.open();
    lc.manager.update_provider_registry(update);

    let disposition = lc
        .manager
        .start_agent_run(
            &created.id,
            "continue after account switch".into(),
            StartRunOptions {
                run: paseo_core::RunOpts::with_client_message_id("closed-session-auto-reload"),
                ..StartRunOptions::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(disposition, paseo_core::PromptDisposition::TurnStarted);
    within(retry_started.wait()).await;
    let result = lc
        .manager
        .wait_for_agent_event(&created.id, WaitForAgentOptions::default())
        .await
        .unwrap();
    assert_eq!(result.status, AgentLifecycle::Idle);
    assert_eq!(lc.manager.get_agent(&created.id).unwrap().last_error, None);
    assert_eq!(replacement.resumes().len(), 1);
    assert!(!lc.items(&created.id).await.iter().any(|item| matches!(
        item,
        AgentTimelineItem::AssistantMessage { text, .. } if text.contains("Claude session is closed")
    )));
}

#[tokio::test]
async fn a_replacement_prompt_recovers_when_the_retired_session_fails_to_start() {
    let client = MockClient::new(plugin_provider());
    client.on_create(|req| {
        boxed(async move {
            let session = MockSession::builder(plugin_provider())
                .config(&req.config)
                .behavior(
                    ScriptedBehavior::new()
                        .on_start(|session, prompt, options| {
                            boxed(async move {
                                let n = session.record_start(prompt, options);
                                if n > 1 {
                                    return Err(stale_error());
                                }
                                Ok(paseo_provider_api::TurnHandle {
                                    turn_id: "initial-turn".into(),
                                })
                            })
                        })
                        .on_interrupt(|session| {
                            boxed(async move {
                                if session.start_count() > 1 {
                                    return Err(stale_error());
                                }
                                session.push_event(turn_canceled_for(
                                    plugin_provider(),
                                    "initial-turn",
                                ));
                                Ok(())
                            })
                        }),
                )
                .build();
            session.set_persistence(PersistenceHandle {
                provider: plugin_provider().into(),
                session_id: "plugin-session".into(),
                native_handle: None,
                metadata: None,
            });
            Ok(session)
        })
    });
    let lc = Lc::with(
        client,
        LcOptions {
            provider: plugin_provider().into(),
            definitions: true,
            tweak: Box::new(|o: &mut ManagerOptions| {
                o.interrupt_session_timeout = Duration::from_millis(20);
            }),
            ..LcOptions::default()
        },
    )
    .await;
    let created = lc
        .create_with(
            AgentSessionConfig::new(plugin_provider(), lc.cwd()),
            CreateAgentOptions::default(),
        )
        .await;
    let retry_started = Gate::new();
    let replacement = Arc::new(resume_tracking_client(plugin_provider(), &retry_started));

    let mut initial = lc
        .manager
        .stream_agent(&created.id, "initial".into(), Default::default())
        .await
        .unwrap();
    tokio::spawn(async move { while let Ok(Some(_)) = initial.next().await {} });
    lc.manager
        .wait_for_agent_run_start(&created.id, None)
        .await
        .unwrap();
    swap_client(&lc, &replacement, &[]);

    let disposition = lc
        .manager
        .start_agent_run(
            &created.id,
            "replacement".into(),
            StartRunOptions {
                replace_running: true,
                ..StartRunOptions::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(disposition, paseo_core::PromptDisposition::TurnStarted);
    within(retry_started.wait()).await;
    let result = lc
        .manager
        .wait_for_agent_event(&created.id, WaitForAgentOptions::default())
        .await
        .unwrap();
    assert_eq!(result.status, AgentLifecycle::Idle);
    assert_eq!(replacement.resumes().len(), 1);
}

fn turn_canceled_for(provider: &str, turn: &str) -> AgentStreamEvent {
    AgentStreamEvent::TurnCanceled {
        provider: provider.into(),
        reason: "interrupted".into(),
        turn_id: Some(turn.into()),
    }
}

// ---- create, resume, import ----

#[tokio::test]
async fn create_agent_passes_explicit_model_strings_through_to_the_provider() {
    let lc = Lc::new().await;
    lc.create_with(
        with_config(&lc, |c| c.model = Some("not-a-real-model".into())),
        CreateAgentOptions::default(),
    )
    .await;
    assert_eq!(
        lc.client.creates()[0].config.model.as_deref(),
        Some("not-a-real-model")
    );
}

#[tokio::test]
async fn resume_agent_from_persistence_keeps_metadata_config_applies_overrides_and_passes_launch_env(
) {
    let lc = Lc::with(mcp_client("codex"), LcOptions::default()).await;
    let mut handle = handle("codex", "resume-session-1", &lc.cwd());
    handle.metadata = Some(obj(json!({
        "provider": "codex",
        "cwd": lc.cwd(),
        "systemPrompt": "old prompt",
        "mcpServers": {"legacy": {"type": "stdio", "command": "legacy-bridge", "args": ["/tmp/legacy.sock"]}}
    })));
    let paseo_server = json!({"paseo": {"type": "stdio", "command": "node", "args": ["/tmp/mcp-bridge.mjs", "--socket", "/tmp/paseo.sock"]}});

    let resumed = lc
        .manager
        .resume_agent_from_persistence(
            handle,
            Some(obj(
                json!({"cwd": lc.cwd(), "systemPrompt": "new prompt", "mcpServers": paseo_server}),
            )),
            None,
            ResumeOptions::default(),
            None,
        )
        .await
        .unwrap();

    assert_eq!(resumed.config.system_prompt.as_deref(), Some("new prompt"));
    assert_eq!(resumed.config.mcp_servers, Some(paseo_server.clone()));
    let call = &lc.client.resumes()[0];
    let overrides = call.overrides.as_ref().unwrap();
    assert_eq!(overrides["model"], json!("gpt-5.4"));
    assert_eq!(overrides["systemPrompt"], json!("new prompt"));
    assert_eq!(overrides["mcpServers"], paseo_server);
    assert!(!overrides.contains_key("modeId"));
    assert_eq!(
        serde_json::to_value(call.launch_context.as_ref().unwrap()).unwrap(),
        json!({"agentId": resumed.id, "env": {"PASEO_AGENT_ID": resumed.id, "PASEO_AGENT_CWD": lc.cwd()}})
    );
}

#[tokio::test]
async fn resume_applies_an_explicit_undefined_override_over_the_handle_metadata() {
    let lc = Lc::new().await;
    let mut handle = handle("codex", "shadow", &lc.cwd());
    handle.metadata = Some(obj(
        json!({"cwd": lc.cwd(), "model": "from-metadata", "systemPrompt": "from-metadata"}),
    ));
    lc.manager
        .resume_agent_from_persistence(
            handle,
            Some(obj(json!({"model": null}))),
            None,
            ResumeOptions::default(),
            None,
        )
        .await
        .unwrap();
    let overrides = lc.client.resumes()[0].overrides.clone().unwrap();
    assert_eq!(
        overrides["model"],
        json!("gpt-5.4"),
        "undefined shadows metadata, then the default model fills in"
    );
    assert_eq!(overrides["systemPrompt"], json!("from-metadata"));
}

#[tokio::test]
async fn resume_agent_from_persistence_rejects_an_unavailable_or_unknown_provider() {
    let lc = Lc::new().await;
    lc.client.set_available(false);
    let error = lc
        .manager
        .resume_agent_from_persistence(
            handle("codex", "s", &lc.cwd()),
            Some(obj(json!({"cwd": lc.cwd()}))),
            None,
            ResumeOptions::default(),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "Provider 'codex' is not available. Please ensure the CLI is installed."
    );
    let error = lc
        .manager
        .resume_agent_from_persistence(
            handle("ghost", "s", &lc.cwd()),
            Some(obj(json!({"cwd": lc.cwd()}))),
            None,
            ResumeOptions::default(),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "No client registered for provider 'ghost'"
    );
}

#[tokio::test]
async fn retries_provider_history_hydration_after_a_stream_failure() {
    let client = MockClient::new("codex");
    let attempts = Arc::new(AtomicU32::new(0));
    {
        let attempts = attempts.clone();
        client.on_resume(move |req| {
            let attempts = attempts.clone();
            boxed(async move {
                let session = MockSession::builder("codex")
                    .auto_completing()
                    .behavior(ScriptedBehavior::new().on_history(move |_| {
                        let attempts = attempts.clone();
                        boxed(async move {
                            if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                                return Err(ProviderError::provider("history interrupted"));
                            }
                            Ok(vec![assistant("provider history", "codex", None)])
                        })
                    }))
                    .build();
                session.set_persistence(req.handle.clone());
                Ok(session)
            })
        });
    }
    let lc = Lc::with_client(client).await;
    let agent = lc
        .manager
        .resume_agent_from_persistence(
            handle("codex", "retry-history", &lc.cwd()),
            Some(obj(json!({"cwd": lc.cwd()}))),
            None,
            ResumeOptions::default(),
            None,
        )
        .await
        .unwrap();
    let error = lc
        .manager
        .hydrate_timeline_from_provider(&agent.id, Default::default())
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "history interrupted");
    assert!(!lc.manager.get_agent(&agent.id).unwrap().history_primed);

    lc.manager
        .hydrate_timeline_from_provider(&agent.id, Default::default())
        .await
        .unwrap();
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert!(lc
        .items(&agent.id)
        .await
        .contains(&assistant_item("provider history")));
}

#[tokio::test]
async fn create_agent_fails_when_generated_agent_id_is_not_a_uuid() {
    let lc = Lc::with(
        MockClient::new("codex"),
        LcOptions {
            tweak: Box::new(|o: &mut ManagerOptions| {
                o.id_factory = Arc::new(|| "not-a-uuid".into())
            }),
            ..LcOptions::default()
        },
    )
    .await;
    let error = lc
        .manager
        .create_agent(lc.config(), None, CreateAgentOptions::default())
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "createAgent: agentId must be a UUID");
}

#[tokio::test]
async fn create_agent_fails_when_explicit_agent_id_is_not_a_uuid() {
    let lc = Lc::new().await;
    let error = lc
        .manager
        .create_agent(
            lc.config(),
            Some("not-a-uuid"),
            CreateAgentOptions::default(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "createAgent: agentId must be a UUID");
}

#[tokio::test]
async fn create_agent_persists_provided_title_before_returning() {
    let lc = Lc::new().await;
    let snapshot = lc
        .create_with(
            with_config(&lc, |c| c.title = Some("Fix Login Bug".into())),
            CreateAgentOptions::default(),
        )
        .await;
    assert_eq!(snapshot.id, id_n(1));
    assert_eq!(snapshot.lifecycle, AgentLifecycle::Idle);
    let persisted = lc.record(&snapshot.id).await.unwrap();
    assert_eq!(
        persisted.title.value().map(String::as_str),
        Some("Fix Login Bug")
    );
    assert_eq!(persisted.id, snapshot.id);
}

#[tokio::test]
async fn create_agent_populates_runtime_info_after_session_creation() {
    let lc = Lc::new().await;
    let snapshot = lc
        .create_with(
            with_config(&lc, |c| {
                c.model = Some("gpt-5.2-codex".into());
                c.mode_id = Some("full-access".into());
            }),
            CreateAgentOptions::default(),
        )
        .await;
    let info = snapshot.runtime_info.clone().expect("runtime info");
    assert_eq!(info.model, Some(Some("gpt-5.2-codex".into())));
    assert_eq!(
        info.session_id,
        snapshot.persistence.as_ref().map(|p| p.session_id.clone())
    );
}

#[tokio::test]
async fn run_agent_refreshes_runtime_info_after_completion() {
    let lc = Lc::new().await;
    let snapshot = lc.create().await;
    assert_eq!(
        snapshot.runtime_info.as_ref().unwrap().model,
        Some(Some("gpt-5.4".into()))
    );
    lc.manager
        .run_agent(&snapshot.id, "hello".into(), Default::default())
        .await
        .unwrap();
    let refreshed = lc.manager.get_agent(&snapshot.id).unwrap();
    assert_eq!(
        refreshed.runtime_info.as_ref().unwrap().model,
        Some(Some("gpt-5.2-codex".into()))
    );
}

#[tokio::test]
async fn create_agent_emits_the_agent_created_plugin_event_for_public_agents_only() {
    let lc = Lc::new().await;
    let agent = lc.create_titled("Visible").await;
    let mut helper = lc.config();
    helper.internal = Some(true);
    lc.create_with(helper, CreateAgentOptions::default()).await;
    let created: Vec<_> = lc
        .hooks
        .plugin_emits
        .lock()
        .unwrap()
        .iter()
        .filter(|(hook, _)| hook == "agent.created")
        .cloned()
        .collect();
    assert_eq!(created.len(), 1);
    assert_eq!(created[0].1["agent"]["id"], json!(agent.id));
    assert_eq!(created[0].1["agent"]["title"], json!("Visible"));
    assert_eq!(created[0].1["agent"]["parentAgentId"], Value::Null);
}

#[tokio::test]
async fn import_provider_session_imports_the_selected_session_without_listing_and_publishes_ready_state(
) {
    let lc = Lc::new().await;
    let session = MockSession::builder("codex").auto_completing().build();
    let cwd = lc.cwd();
    {
        let (session, cwd) = (session.clone(), cwd.clone());
        lc.client.on_import(move |input, _context| {
            let (session, cwd) = (session.clone(), cwd.clone());
            boxed(async move {
                let handle_id = input["providerHandleId"].as_str().unwrap().to_owned();
                Ok(ImportedSession {
                    session: Box::new(session),
                    config: AgentSessionConfig::new("codex", cwd.clone()),
                    persistence: PersistenceHandle {
                        provider: "codex".into(),
                        session_id: handle_id.clone(),
                        native_handle: Some(handle_id),
                        metadata: Some(obj(json!({"provider": "codex", "cwd": cwd}))),
                    },
                    timeline: vec![
                        ImportedTimelineEntry {
                            item: AgentTimelineItem::UserMessage {
                                text: "Trace provider imports".into(),
                                message_id: None,
                                client_message_id: None,
                            },
                            timestamp: Some("2026-01-02T00:00:00.000Z".into()),
                        },
                        ImportedTimelineEntry {
                            item: assistant_item("Done"),
                            timestamp: Some("2026-01-02T00:00:01.000Z".into()),
                        },
                        ImportedTimelineEntry {
                            item: tool_call(
                                "large-shell-result",
                                paseo_agent_types::ToolCallStatus::Completed,
                                &"x".repeat(1024 * 1024),
                                None,
                            ),
                            timestamp: Some("2026-01-02T00:00:02.000Z".into()),
                        },
                    ],
                    provider_subagent_events: vec![
                        AgentStreamEvent::ProviderSubagent {
                            provider: "codex".into(),
                            event: Box::new(
                                paseo_agent_types::ProviderSubagentInputEvent::Upsert {
                                    id: "thread-child".into(),
                                    title: Some(Some("Imported child".into())),
                                    description: None,
                                    status: Some(
                                        paseo_agent_types::ProviderSubagentStatus::Completed,
                                    ),
                                    tool_call_id: None,
                                    cwd: None,
                                    subtitle: None,
                                    parent_subagent_id: None,
                                    timestamp: None,
                                },
                            ),
                        },
                        AgentStreamEvent::ProviderSubagent {
                            provider: "codex".into(),
                            event: Box::new(
                                paseo_agent_types::ProviderSubagentInputEvent::Timeline {
                                    id: "thread-child".into(),
                                    item: Box::new(assistant_item("Child result")),
                                    timestamp: None,
                                },
                            ),
                        },
                    ],
                })
            })
        });
    }

    let imported = lc
        .manager
        .import_provider_session(ImportInput {
            provider: "codex".into(),
            provider_handle_id: "thread-selected".into(),
            cwd: cwd.clone(),
            workspace_id: "ws-imported".into(),
            labels: None,
        })
        .await
        .unwrap();

    assert_eq!(lc.client.list_calls(), 0);
    assert_eq!(imported.lifecycle, AgentLifecycle::Idle);
    assert!(imported.history_primed);
    let items = lc.items(&imported.id).await;
    assert_eq!(items.len(), 3);
    assert_eq!(items[1], assistant_item("Done"));
    assert_eq!(
        items[2],
        tool_call(
            "large-shell-result",
            paseo_agent_types::ToolCallStatus::Completed,
            &"x".repeat(64 * 1024),
            None
        )
    );
    let children = lc.manager.list_provider_subagents(&imported.id).unwrap();
    assert_eq!(children.len(), 1);
    assert_eq!(children[0].id, "thread-child");
    assert_eq!(children[0].title.as_deref(), Some("Imported child"));
    assert_eq!(
        children[0].status,
        paseo_agent_types::ProviderSubagentStatus::Completed
    );
    let child_rows = lc
        .manager
        .fetch_provider_subagent_timeline(&imported.id, "thread-child", &Default::default())
        .unwrap()
        .unwrap();
    assert_eq!(child_rows.rows.len(), 1);
    assert_eq!(child_rows.rows[0].item, assistant_item("Child result"));
    lc.sync().await;
    let all = lc.events.all();
    assert_eq!(
        all.len(),
        3,
        "state, then the two provider-child updates: {all:?}"
    );
    match &*all[0] {
        paseo_core::ManagerEvent::AgentState { agent } => {
            assert_eq!(agent.id, imported.id);
            assert_eq!(agent.lifecycle, AgentLifecycle::Idle);
            assert_eq!(
                agent
                    .persistence
                    .as_ref()
                    .and_then(|p| p.native_handle.clone())
                    .as_deref(),
                Some("thread-selected")
            );
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        lc.record(&imported.id)
            .await
            .unwrap()
            .title
            .value()
            .map(String::as_str),
        Some("Trace provider imports")
    );
    let _ = HashSet::<u8>::new();
}

#[tokio::test]
async fn import_provider_session_rejects_a_provider_that_cannot_import() {
    let lc = Lc::new().await;
    let error = lc
        .manager
        .import_provider_session(ImportInput {
            provider: "codex".into(),
            provider_handle_id: "x".into(),
            cwd: lc.cwd(),
            workspace_id: "ws".into(),
            labels: None,
        })
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "Provider 'codex' does not support importing sessions"
    );
}

// ---- importable sessions ----

fn listing_client(provider: &str) -> (MockClient, Arc<AtomicU32>) {
    let client = MockClient::new(provider);
    let calls = Arc::new(AtomicU32::new(0));
    let (provider, counted) = (provider.to_owned(), calls.clone());
    client.on_list_importable(move || {
        counted.fetch_add(1, Ordering::SeqCst);
        let provider = provider.clone();
        boxed(async move {
            Ok(vec![json!({
                "providerHandleId": format!("{provider}-session"),
                "cwd": "/tmp/recent",
                "title": null,
                "lastActivityAt": "2026-01-01T00:00:00.000Z",
                "firstPromptPreview": null,
                "lastPromptPreview": null,
            })])
        })
    });
    (client, calls)
}

async fn importable_manager(
    clients: Vec<(&str, MockClient)>,
) -> (tempfile::TempDir, Arc<AgentManager>) {
    let arcs: Vec<(String, Arc<MockClient>)> = clients
        .into_iter()
        .map(|(p, c)| (p.to_owned(), Arc::new(c)))
        .collect();
    let refs: Vec<(&str, Arc<MockClient>)> =
        arcs.iter().map(|(p, c)| (p.as_str(), c.clone())).collect();
    let (dir, manager, _) = multi_provider(&refs, |o| {
        for (provider, _) in &arcs {
            o.providers
                .definitions
                .insert(provider.clone(), ProviderDefinition::enabled());
        }
    })
    .await;
    (dir, manager)
}

#[tokio::test]
async fn list_importable_sessions_includes_derived_providers_that_list_persisted_agents() {
    let (claude, claude_calls) = listing_client("claude");
    let (omp, omp_calls) = listing_client("omp");
    let (_dir, manager) = importable_manager(vec![("claude", claude), ("omp", omp)]).await;

    let result = manager
        .list_importable_sessions(ImportableQuery::default())
        .await;

    assert_eq!(claude_calls.load(Ordering::SeqCst), 1);
    assert_eq!(omp_calls.load(Ordering::SeqCst), 1);
    let mut providers: Vec<_> = result
        .sessions
        .iter()
        .map(|s| s["provider"].as_str().unwrap().to_owned())
        .collect();
    providers.sort();
    assert_eq!(providers, ["claude", "omp"]);
}

#[tokio::test]
async fn list_importable_sessions_narrows_to_the_provider_filter_when_supplied() {
    let (claude, claude_calls) = listing_client("claude");
    let (codex, codex_calls) = listing_client("codex");
    let (_dir, manager) = importable_manager(vec![("claude", claude), ("codex", codex)]).await;

    let result = manager
        .list_importable_sessions(ImportableQuery {
            provider_filter: Some(HashSet::from(["claude".to_owned()])),
            ..ImportableQuery::default()
        })
        .await;

    assert_eq!(claude_calls.load(Ordering::SeqCst), 1);
    assert_eq!(codex_calls.load(Ordering::SeqCst), 0);
    assert_eq!(result.sessions.len(), 1);
    assert_eq!(result.sessions[0]["provider"], json!("claude"));
}

#[tokio::test]
async fn list_importable_sessions_skips_providers_that_lack_supports_session_listing() {
    let (listable, listable_calls) = listing_client("claude");
    let (mut non_listable, non_listable_calls) = listing_client("acp");
    non_listable = non_listable.with_capability("supportsSessionListing", false);
    let (_dir, manager) =
        importable_manager(vec![("claude", listable), ("acp", non_listable)]).await;

    let result = manager
        .list_importable_sessions(ImportableQuery::default())
        .await;

    assert_eq!(listable_calls.load(Ordering::SeqCst), 1);
    assert_eq!(non_listable_calls.load(Ordering::SeqCst), 0);
    assert_eq!(result.sessions.len(), 1);
    assert_eq!(result.sessions[0]["provider"], json!("claude"));
}

#[tokio::test(start_paused = true)]
async fn list_importable_sessions_returns_healthy_rows_alongside_thrown_and_timed_out_provider_errors(
) {
    let (healthy, _) = listing_client("claude");
    let failing = MockClient::new("codex");
    failing.on_list_importable(|| {
        boxed(async { Err(ProviderError::provider("codex listing failed")) })
    });
    let hanging = MockClient::new("pi");
    hanging.on_list_importable(|| boxed(async { std::future::pending().await }));
    let (_dir, manager) = importable_manager(vec![
        ("claude", healthy),
        ("codex", failing),
        ("pi", hanging),
    ])
    .await;

    let result = manager
        .list_importable_sessions(ImportableQuery::default())
        .await;

    assert_eq!(result.sessions.len(), 1);
    assert_eq!(
        result.sessions[0]["providerHandleId"],
        json!("claude-session")
    );
    assert_eq!(result.sessions[0]["provider"], json!("claude"));
    assert_eq!(
        result.provider_errors,
        vec![
            paseo_core::ImportableSessionProviderError {
                provider: "codex".into(),
                message: "codex listing failed".into()
            },
            paseo_core::ImportableSessionProviderError {
                provider: "pi".into(),
                message: "Timed out listing importable sessions for provider 'pi' after 90000ms"
                    .into()
            },
        ]
    );
}

#[tokio::test]
async fn list_importable_sessions_searches_every_provider_result_before_global_ranking() {
    let client = MockClient::new("claude");
    client.on_list_importable(|| {
        boxed(async {
            let mut rows: Vec<Value> = (0..25)
                .map(|i| {
                    json!({
                        "providerHandleId": format!("non-match-{i}"),
                        "cwd": "/tmp/other",
                        "title": "Other work",
                        "firstPromptPreview": null,
                        "lastPromptPreview": null,
                        "lastActivityAt": format!("2026-04-{:02}T00:00:00.000Z", i + 2),
                    })
                })
                .collect();
            rows.push(json!({"providerHandleId": "title-match", "cwd": "/tmp/archive", "title": "Invoice cleanup", "firstPromptPreview": null, "lastPromptPreview": null, "lastActivityAt": "2026-04-01T04:00:00.000Z"}));
            rows.push(json!({"providerHandleId": "first-prompt-match", "cwd": "/tmp/archive", "title": "Unrelated", "firstPromptPreview": "Investigate invoice totals", "lastPromptPreview": null, "lastActivityAt": "2026-04-01T03:00:00.000Z"}));
            rows.push(json!({"providerHandleId": "last-prompt-match", "cwd": "/tmp/archive", "title": "Unrelated", "firstPromptPreview": null, "lastPromptPreview": "Finish invoice export", "lastActivityAt": "2026-04-01T02:00:00.000Z"}));
            rows.push(json!({"providerHandleId": "cwd-match", "cwd": "/tmp/invoice-service", "title": "Unrelated", "firstPromptPreview": null, "lastPromptPreview": null, "lastActivityAt": "2026-04-01T01:00:00.000Z"}));
            Ok(rows)
        })
    });
    let (_dir, manager) = importable_manager(vec![("claude", client)]).await;

    let result = manager
        .list_importable_sessions(ImportableQuery {
            query: Some("INVOICE".into()),
            limit: Some(10),
            scan_limit: Some(500),
            ..ImportableQuery::default()
        })
        .await;

    let ids: Vec<_> = result
        .sessions
        .iter()
        .map(|s| s["providerHandleId"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        ids,
        [
            "title-match",
            "first-prompt-match",
            "last-prompt-match",
            "cwd-match"
        ]
    );
}

// ---- workspace hook ----

#[tokio::test]
async fn command_may_have_changed_external_state_matches_remote_state_commands() {
    for command in [
        "gh pr merge 123",
        "gh pr close 123",
        "gh pr create",
        "gh pr edit 123",
        "gh pr comment 123 -b \"lgtm\"",
        "gh pr review 123 -a",
        "git push origin main",
        "git fetch origin",
    ] {
        assert!(
            paseo_core::command_may_have_changed_external_state(command),
            "{command}"
        );
    }
}

#[tokio::test]
async fn command_may_have_changed_external_state_ignores_local_or_read_only_commands() {
    for command in [
        "git commit -m 'hello'",
        "git checkout main",
        "git merge feature",
        "git rebase main",
        "git reset --hard HEAD~1",
        "git pull origin main",
        "gh pr view 123",
        "gh pr list",
        "gh auth status",
        "gh repo view",
        "git status",
        "ls -la",
        "cat file.txt",
        "npm install",
        "npm publish",
    ] {
        assert!(
            !paseo_core::command_may_have_changed_external_state(command),
            "{command}"
        );
    }
}

async fn run_with_turn_items(items: Vec<AgentTimelineItem>) -> Arc<HookLog> {
    let client = MockClient::new("codex");
    client.on_create(move |req| {
        let items = items.clone();
        boxed(async move {
            Ok(MockSession::builder("codex")
                .config(&req.config)
                .behavior(
                    ScriptedBehavior::new().on_start(move |session, prompt, options| {
                        let items = items.clone();
                        boxed(async move {
                            let n = session.record_start(prompt, options);
                            let turn_id = format!("turn-fake-codex-{n}");
                            let (soon, turn) = (session.clone(), turn_id.clone());
                            tokio::spawn(async move {
                                tokio::time::sleep(Duration::ZERO).await;
                                soon.push_event(provider_event_turn_started("codex", &turn));
                                for item in items {
                                    soon.push_event(timeline_event(item, "codex", Some(&turn)));
                                }
                                soon.push_event(turn_completed(&turn));
                            });
                            Ok(paseo_provider_api::TurnHandle { turn_id })
                        })
                    }),
                )
                .build())
        })
    });
    let lc = Lc::with_client(client).await;
    let snapshot = lc.create().await;
    lc.manager
        .run_agent(&snapshot.id, "go".into(), Default::default())
        .await
        .unwrap();
    let cwds = lc.hooks.workspace_cwds.lock().unwrap().clone();
    assert!(cwds.iter().all(|c| *c == lc.cwd()));
    lc.hooks.clone()
}

fn shell_call(command: &str, status: paseo_agent_types::ToolCallStatus) -> AgentTimelineItem {
    let mut item = paseo_agent_types::ToolCallItem::new(
        "call-1",
        "bash",
        status,
        paseo_agent_types::ToolCallDetail::Shell {
            command: command.into(),
            cwd: None,
            output: None,
            exit_code: None,
        },
    );
    item.error = None;
    AgentTimelineItem::tool_call(item)
}

#[tokio::test]
async fn on_workspace_state_may_have_changed_is_called_when_a_completed_shell_tool_call_may_have_changed_external_state(
) {
    let log = run_with_turn_items(vec![shell_call(
        "gh pr merge 123 --squash",
        paseo_agent_types::ToolCallStatus::Completed,
    )])
    .await;
    assert_eq!(log.workspace_cwds.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn on_workspace_state_may_have_changed_is_not_called_for_non_shell_tool_calls() {
    let read = AgentTimelineItem::tool_call(paseo_agent_types::ToolCallItem::new(
        "call-1",
        "read",
        paseo_agent_types::ToolCallStatus::Completed,
        paseo_agent_types::ToolCallDetail::Read {
            file_path: "/tmp/foo.txt".into(),
            content: None,
            offset: None,
            limit: None,
        },
    ));
    let log = run_with_turn_items(vec![read]).await;
    assert!(log.workspace_cwds.lock().unwrap().is_empty());
}

#[tokio::test]
async fn on_workspace_state_may_have_changed_is_not_called_for_running_shell_tool_calls() {
    let log = run_with_turn_items(vec![shell_call(
        "gh pr merge 123 --squash",
        paseo_agent_types::ToolCallStatus::Running,
    )])
    .await;
    assert!(log.workspace_cwds.lock().unwrap().is_empty());
}

#[allow(dead_code)]
fn _unused(_: &ManagedAgentView) {}
