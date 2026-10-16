//! Ports of the live-config mutator cases of `agent-manager.test.ts`: mode, model, thinking
//! option and feature changes, and provider-side configuration drift.

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use common::*;
use paseo_agent_types::{AgentMode, AgentRuntimeInfo, AgentStreamEvent, PersistenceHandle};
use paseo_core::mock::{boxed, MockClient, MockSession, ScriptedBehavior};
use paseo_core::CreateAgentOptions;
use paseo_provider_api::{AgentSessionConfig, ProviderError};
use paseo_store::Nullable;
use serde_json::json;

fn full_config(lc: &Lc) -> AgentSessionConfig {
    let mut config = lc.config();
    config.mode_id = Some("plan".into());
    config.model = Some("gpt-5.2-codex".into());
    config.thinking_option_id = Some("low".into());
    config
}

fn mode(id: &str, label: &str) -> AgentMode {
    AgentMode {
        id: id.into(),
        label: label.into(),
        description: None,
        icon: None,
        color_tier: None,
        is_unattended: None,
    }
}

#[tokio::test]
async fn persists_live_mode_model_and_thinking_changes_without_an_external_snapshot_subscriber() {
    let lc = Lc::new().await;
    let snapshot = lc
        .create_with(full_config(&lc), CreateAgentOptions::default())
        .await;

    lc.manager
        .set_agent_mode(&snapshot.id, "build")
        .await
        .unwrap();
    lc.manager
        .set_agent_model(&snapshot.id, Some("gpt-5.4"))
        .await
        .unwrap();
    lc.manager
        .set_agent_thinking_option(&snapshot.id, Some("high"))
        .await
        .unwrap();
    lc.flush().await;

    let persisted = lc.record(&snapshot.id).await.unwrap();
    assert_eq!(persisted.last_mode_id, Nullable::Value("build".into()));
    let config = persisted.config.value().unwrap();
    assert_eq!(config.model.value().map(String::as_str), Some("gpt-5.4"));
    assert_eq!(
        config.thinking_option_id.value().map(String::as_str),
        Some("high")
    );
    let info = persisted.runtime_info.unwrap();
    assert_eq!(info.mode_id, Nullable::Value("build".into()));
    assert_eq!(info.model, Nullable::Value("gpt-5.4".into()));
}

#[tokio::test]
async fn model_changes_persist_the_resolved_thinking_and_fresh_provider_handle_before_another_turn()
{
    let client = MockClient::new("codex");
    client.on_create(|req| {
        boxed(async move {
            let session = MockSession::builder("codex")
                .config(&req.config)
                .auto_completing()
                .behavior(ScriptedBehavior::new().on_set_model(|session, model| {
                    boxed(async move {
                        let model = model.unwrap_or_else(|| "default".into());
                        session.set_persistence(PersistenceHandle {
                            provider: "codex".into(),
                            session_id: session.session_id().to_owned(),
                            native_handle: None,
                            metadata: Some(obj(json!({"model": model}))),
                        });
                        session.push_event(AgentStreamEvent::ThinkingOptionChanged {
                            provider: "codex".into(),
                            thinking_option_id: None,
                        });
                        Ok(())
                    })
                }))
                .build();
            session.set_persistence(PersistenceHandle {
                provider: "codex".into(),
                session_id: session.session_id().to_owned(),
                native_handle: None,
                metadata: Some(obj(
                    json!({"model": "source", "thinkingOptionId": "medium"}),
                )),
            });
            Ok(session)
        })
    });
    let lc = Lc::with_client(client).await;
    let mut config = lc.config();
    config.model = Some("source".into());
    config.thinking_option_id = Some("medium".into());
    let agent = lc.create_with(config, CreateAgentOptions::default()).await;

    lc.manager
        .set_agent_model(&agent.id, Some("target"))
        .await
        .unwrap();
    lc.flush().await;

    let stored = lc.record(&agent.id).await.unwrap();
    let config = stored.config.value().unwrap();
    assert_eq!(config.model.value().map(String::as_str), Some("target"));
    assert!(config.thinking_option_id.value().is_none());
    let metadata = stored
        .persistence
        .value()
        .unwrap()
        .metadata
        .clone()
        .unwrap();
    assert_eq!(metadata["model"], json!("target"));
    assert!(metadata.get("thinkingOptionId").is_none());
}

#[tokio::test]
async fn later_explicit_config_mutations_win_over_events_emitted_by_earlier_mutations() {
    let client = MockClient::new("codex");
    client.on_create(|req| {
        boxed(async move {
            Ok(MockSession::builder("codex")
                .config(&req.config)
                .auto_completing()
                .behavior(ScriptedBehavior::new().on_set_model(|session, _| {
                    boxed(async move {
                        session.push_event(assistant("model changed", "codex", None));
                        session.push_event(AgentStreamEvent::ThinkingOptionChanged {
                            provider: "codex".into(),
                            thinking_option_id: Some("low".into()),
                        });
                        Ok(())
                    })
                }))
                .build())
        })
    });
    let lc = Lc::with_client(client).await;
    let mut config = lc.config();
    config.model = Some("gpt-5.2-codex".into());
    config.thinking_option_id = Some("off".into());
    let snapshot = lc.create_with(config, CreateAgentOptions::default()).await;

    lc.manager
        .set_agent_model(&snapshot.id, Some("gpt-5.4"))
        .await
        .unwrap();
    lc.manager
        .set_agent_thinking_option(&snapshot.id, Some("high"))
        .await
        .unwrap();
    lc.manager.flush().await;

    assert_eq!(
        lc.manager
            .get_agent(&snapshot.id)
            .unwrap()
            .config
            .thinking_option_id
            .as_deref(),
        Some("high")
    );
}

#[tokio::test]
async fn set_agent_thinking_option_adopts_the_sessions_effective_clamped_level() {
    let client = MockClient::new("codex");
    let clamp = Arc::new(AtomicBool::new(false));
    {
        let clamp = clamp.clone();
        client.on_create(move |req| {
            let clamp = clamp.clone();
            boxed(async move {
                Ok(MockSession::builder("codex")
                    .config(&req.config)
                    .auto_completing()
                    .behavior(ScriptedBehavior::new().on_runtime_info(move |session| {
                        let clamp = clamp.clone();
                        boxed(async move {
                            let mut info = session.default_runtime_info();
                            if clamp.load(Ordering::SeqCst) {
                                info.thinking_option_id = Some(Some("high".into()));
                            }
                            Ok(info)
                        })
                    }))
                    .build())
            })
        });
    }
    let lc = Lc::with_client(client).await;
    let mut config = lc.config();
    config.thinking_option_id = Some("low".into());
    let snapshot = lc.create_with(config, CreateAgentOptions::default()).await;
    clamp.store(true, Ordering::SeqCst);

    lc.manager
        .set_agent_thinking_option(&snapshot.id, Some("medium"))
        .await
        .unwrap();
    lc.flush().await;

    let agent = lc.manager.get_agent(&snapshot.id).unwrap();
    assert_eq!(agent.config.thinking_option_id.as_deref(), Some("high"));
    assert_eq!(
        agent.runtime_info.clone().unwrap().thinking_option_id,
        Some(Some("high".into()))
    );
    let persisted = lc.record(&snapshot.id).await.unwrap();
    assert_eq!(
        persisted
            .config
            .value()
            .unwrap()
            .thinking_option_id
            .value()
            .map(String::as_str),
        Some("high")
    );
}

#[tokio::test]
async fn set_agent_thinking_option_surfaces_a_failed_state_read_and_keeps_the_previous_level() {
    let client = MockClient::new("codex");
    let unreadable = Arc::new(AtomicBool::new(false));
    {
        let unreadable = unreadable.clone();
        client.on_create(move |req| {
            let unreadable = unreadable.clone();
            boxed(async move {
                Ok(MockSession::builder("codex")
                    .config(&req.config)
                    .auto_completing()
                    .behavior(ScriptedBehavior::new().on_runtime_info(move |session| {
                        let unreadable = unreadable.clone();
                        boxed(async move {
                            if unreadable.load(Ordering::SeqCst) {
                                return Err(ProviderError::provider("state unavailable"));
                            }
                            Ok(session.default_runtime_info())
                        })
                    }))
                    .build())
            })
        });
    }
    let lc = Lc::with_client(client).await;
    let mut config = lc.config();
    config.thinking_option_id = Some("low".into());
    let snapshot = lc.create_with(config, CreateAgentOptions::default()).await;
    unreadable.store(true, Ordering::SeqCst);

    let error = lc
        .manager
        .set_agent_thinking_option(&snapshot.id, Some("medium"))
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "state unavailable");
    lc.flush().await;

    assert_eq!(
        lc.manager
            .get_agent(&snapshot.id)
            .unwrap()
            .config
            .thinking_option_id
            .as_deref(),
        Some("low")
    );
    let persisted = lc.record(&snapshot.id).await.unwrap();
    assert_eq!(
        persisted
            .config
            .value()
            .unwrap()
            .thinking_option_id
            .value()
            .map(String::as_str),
        Some("low")
    );
}

#[tokio::test]
async fn session_config_drift_events_update_state_through_the_stream_channel() {
    let lc = Lc::new().await;
    let snapshot = lc
        .create_with(full_config(&lc), CreateAgentOptions::default())
        .await;
    let session = lc.client.created_sessions()[0].clone();
    lc.events.clear();

    session.push_event(AgentStreamEvent::ModeChanged {
        provider: "codex".into(),
        current_mode_id: Some("build".into()),
        available_modes: vec![mode("plan", "Plan"), mode("build", "Build")],
    });
    session.push_event(AgentStreamEvent::ModelChanged {
        provider: "codex".into(),
        runtime_info: AgentRuntimeInfo {
            provider: "codex".into(),
            session_id: Some(session.session_id().to_owned()),
            model: Some(Some("gpt-5.4".into())),
            thinking_option_id: Some(Some("low".into())),
            mode_id: Some(Some("build".into())),
            extra: None,
        },
    });
    session.push_event(AgentStreamEvent::ThinkingOptionChanged {
        provider: "codex".into(),
        thinking_option_id: Some("high".into()),
    });
    lc.flush().await;
    lc.sync().await;

    let agent = lc.manager.get_agent(&snapshot.id).unwrap();
    assert_eq!(agent.current_mode_id.as_deref(), Some("build"));
    assert_eq!(agent.config.mode_id.as_deref(), Some("build"));
    assert_eq!(agent.config.thinking_option_id.as_deref(), Some("high"));
    assert_eq!(
        agent
            .available_modes
            .iter()
            .map(|m| (m.id.as_str(), m.label.as_str()))
            .collect::<Vec<_>>(),
        [("plan", "Plan"), ("build", "Build")]
    );
    let info = agent.runtime_info.clone().unwrap();
    assert_eq!(info.model, Some(Some("gpt-5.4".into())));
    assert_eq!(info.mode_id, Some(Some("build".into())));
    assert_eq!(info.thinking_option_id, Some(Some("high".into())));
    assert!(
        lc.events.streams(&snapshot.id).is_empty(),
        "config events are state, not stream events"
    );

    let persisted = lc.record(&snapshot.id).await.unwrap();
    assert_eq!(persisted.last_mode_id, Nullable::Value("build".into()));
    assert_eq!(
        persisted
            .config
            .value()
            .unwrap()
            .mode_id
            .value()
            .map(String::as_str),
        Some("build")
    );
}

#[tokio::test]
async fn blank_model_and_thinking_values_clear_the_setting() {
    let lc = Lc::new().await;
    let snapshot = lc
        .create_with(full_config(&lc), CreateAgentOptions::default())
        .await;
    lc.manager
        .set_agent_model(&snapshot.id, Some("   "))
        .await
        .unwrap();
    lc.manager
        .set_agent_thinking_option(&snapshot.id, None)
        .await
        .unwrap();
    let agent = lc.manager.get_agent(&snapshot.id).unwrap();
    assert_eq!(agent.config.model, None);
    assert_eq!(agent.config.thinking_option_id, None);
    assert_eq!(agent.runtime_info.clone().unwrap().model, Some(None));
}

#[tokio::test]
async fn set_agent_feature_merges_values_and_reports_unsupported_sessions() {
    let client = MockClient::new("codex");
    client.on_create(|req| {
        boxed(async move {
            Ok(MockSession::builder("codex")
                .config(&req.config)
                .auto_completing()
                .behavior(ScriptedBehavior::new().on_set_feature(|_, id, _| {
                    boxed(async move {
                        if id == "fast_mode" {
                            Ok(())
                        } else {
                            Err(ProviderError::Unsupported("set_feature".into()))
                        }
                    })
                }))
                .build())
        })
    });
    let lc = Lc::with_client(client).await;
    let agent = lc.create().await;

    lc.manager
        .set_agent_feature(&agent.id, "fast_mode", json!(true))
        .await
        .unwrap();
    lc.manager
        .set_agent_feature(&agent.id, "fast_mode", json!(false))
        .await
        .unwrap();
    let values = lc
        .manager
        .get_agent(&agent.id)
        .unwrap()
        .config
        .feature_values
        .clone()
        .unwrap();
    assert_eq!(values["fast_mode"], json!(false));

    let error = lc
        .manager
        .set_agent_feature(&agent.id, "other", json!(1))
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "Agent session does not support setting features"
    );
}

#[tokio::test]
async fn mutators_fail_for_an_unknown_agent() {
    let lc = Lc::new().await;
    let error = lc
        .manager
        .set_agent_mode(&id_n(404), "x")
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), format!("Unknown agent '{}'", id_n(404)));
    let error = lc.manager.set_agent_mode("nope", "x").await.unwrap_err();
    assert_eq!(error.to_string(), "requireAgent: agentId must be a UUID");
}

#[tokio::test]
async fn respond_to_permission_plan_approval_survives_a_reload_with_the_new_mode() {
    use paseo_agent_types::{PermissionKind, PermissionRequest, PermissionResponse};
    let client = MockClient::new("codex");
    client.on_create(|req| {
        boxed(async move {
            Ok(MockSession::builder("codex")
                .config(&req.config)
                .auto_completing()
                .behavior(ScriptedBehavior::new().on_respond(|session, _, _| {
                    boxed(async move {
                        // The provider approves the plan and switches mode by itself.
                        session.set_mode_id(Some("acceptEdits"));
                        Ok(None)
                    })
                }))
                .build())
        })
    });
    let lc = Lc::with_client(client).await;
    let mut config = lc.config();
    config.mode_id = Some("plan".into());
    let snapshot = lc.create_with(config, CreateAgentOptions::default()).await;
    assert_eq!(snapshot.current_mode_id.as_deref(), Some("plan"));

    lc.manager
        .testing_mutate_agent(&snapshot.id, |view| {
            view.pending_permissions.insert(
                "perm-123".into(),
                PermissionRequest {
                    id: "perm-123".into(),
                    provider: "codex".into(),
                    name: "ExitPlanMode".into(),
                    kind: PermissionKind::Plan,
                    title: None,
                    description: None,
                    input: None,
                    detail: None,
                    suggestions: None,
                    actions: None,
                    metadata: None,
                },
            );
        })
        .await
        .unwrap();
    lc.manager
        .respond_to_permission(
            &snapshot.id,
            "perm-123",
            PermissionResponse::Allow {
                selected_action_id: None,
                updated_input: None,
                updated_permissions: None,
            },
        )
        .await
        .unwrap();

    assert_eq!(
        lc.manager
            .get_agent(&snapshot.id)
            .unwrap()
            .current_mode_id
            .as_deref(),
        Some("acceptEdits")
    );
    lc.flush().await;
    let persisted = lc.record(&snapshot.id).await.unwrap();
    assert_eq!(
        persisted.last_mode_id,
        Nullable::Value("acceptEdits".into())
    );
    assert_eq!(
        persisted
            .config
            .value()
            .unwrap()
            .mode_id
            .value()
            .map(String::as_str),
        Some("acceptEdits")
    );

    // Without the provider-managed switch reaching `config.modeId` a reload would resume in
    // the stale creation-time mode ("plan").
    let reloaded = lc
        .manager
        .reload_agent_session(&snapshot.id, None, paseo_core::ReloadOptions::default())
        .await
        .unwrap();
    assert_eq!(reloaded.config.mode_id.as_deref(), Some("acceptEdits"));
    let resumes = lc.client.resumes();
    assert_eq!(
        resumes[0].overrides.as_ref().unwrap()["modeId"],
        json!("acceptEdits")
    );
}
