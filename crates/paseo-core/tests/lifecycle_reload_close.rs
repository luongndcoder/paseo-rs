//! Ports of the lifecycle cases of `agent-manager.test.ts`: shutdown races, reload, close,
//! resume and restore. Real `AgentStore` in a temp dir, `MockClient` sessions, real time.

mod common;

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use paseo_agent_types::{AgentStreamEvent, ProviderSubagentInputEvent, ProviderSubagentStatus};
use paseo_core::mock::{boxed, Gate, MockClient, MockSession, ScriptedBehavior};
use paseo_core::{
    CoreError, CreateAgentOptions, LoadOptions, ManagerOptions, ReloadOptions, ResumeOptions,
};
use paseo_provider_api::{AgentSessionConfig, PromptInput, ProviderError};
use paseo_store::AgentLifecycle;
use serde_json::json;

/// Await `work`, failing the test instead of hanging when a gate never opens.
async fn within<T>(work: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(10), work)
        .await
        .expect("timed out")
}

fn session_for(config: &AgentSessionConfig) -> MockSession {
    MockSession::builder("codex")
        .config(config)
        .auto_completing()
        .build()
}

// ---- shutdown races ----

#[tokio::test]
async fn does_not_register_a_session_that_finishes_starting_after_shutdown_begins() {
    let client = MockClient::new("codex");
    let started = Gate::new();
    let allowed = Gate::new();
    let created: Arc<Mutex<Option<MockSession>>> = Arc::default();
    {
        let (started, allowed, created) = (started.clone(), allowed.clone(), created.clone());
        client.on_create(move |req| {
            let (started, allowed, created) = (started.clone(), allowed.clone(), created.clone());
            boxed(async move {
                let session = session_for(&req.config);
                *created.lock().unwrap() = Some(session.clone());
                started.open();
                allowed.wait().await;
                Ok(session)
            })
        });
    }
    let lc = Lc::with_client(client).await;
    let manager = lc.manager.clone();
    let config = lc.config();
    let creation = tokio::spawn(async move {
        manager
            .create_agent(config, None, CreateAgentOptions::default())
            .await
    });
    within(started.wait()).await;
    lc.manager.prepare_for_shutdown();
    allowed.open();

    let error = creation.await.unwrap().unwrap_err();
    assert_eq!(error.to_string(), "Agent manager is shutting down");
    assert!(lc.manager.list_agents(true).is_empty());
    assert!(created.lock().unwrap().as_ref().unwrap().is_closed());
}

#[tokio::test]
async fn flush_waits_for_rejected_session_cleanup_that_starts_after_shutdown() {
    let client = MockClient::new("codex");
    let creation_started = Gate::new();
    let creation_allowed = Gate::new();
    let close_started = Gate::new();
    let close_allowed = Gate::new();
    {
        let (cs, ca, ks, kl) = (
            creation_started.clone(),
            creation_allowed.clone(),
            close_started.clone(),
            close_allowed.clone(),
        );
        client.on_create(move |req| {
            let (cs, ca, ks, kl) = (cs.clone(), ca.clone(), ks.clone(), kl.clone());
            boxed(async move {
                cs.open();
                ca.wait().await;
                let session = held_close_session(&ks, &kl, &req.config);
                Ok(session)
            })
        });
    }
    let lc = Lc::with_client(client).await;
    let manager = lc.manager.clone();
    let config = lc.config();
    let creation = tokio::spawn(async move {
        manager
            .create_agent(config, None, CreateAgentOptions::default())
            .await
    });
    within(creation_started.wait()).await;
    lc.manager.prepare_for_shutdown();
    let flushed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flushing = {
        let (manager, flushed) = (lc.manager.clone(), flushed.clone());
        tokio::spawn(async move {
            manager.flush_for_shutdown().await;
            flushed.store(true, Ordering::SeqCst);
        })
    };
    creation_allowed.open();
    within(close_started.wait()).await;
    settle().await;
    assert!(
        !flushed.load(Ordering::SeqCst),
        "flush must wait for the cleanup close"
    );
    close_allowed.open();

    let error = creation.await.unwrap().unwrap_err();
    assert!(matches!(error, CoreError::ShuttingDown));
    within(flushing).await.unwrap();
    assert!(lc.manager.list_agents(true).is_empty());
}

#[tokio::test]
async fn does_not_persist_an_initializing_session_after_shutdown_closes_it() {
    let client = MockClient::new("codex");
    let info_started = Gate::new();
    let info_allowed = Gate::new();
    {
        let (started, allowed) = (info_started.clone(), info_allowed.clone());
        client.on_create(move |req| {
            let (started, allowed) = (started.clone(), allowed.clone());
            boxed(async move {
                Ok(MockSession::builder("codex")
                    .config(&req.config)
                    .auto_completing()
                    .behavior(ScriptedBehavior::new().on_runtime_info(move |session| {
                        let (started, allowed) = (started.clone(), allowed.clone());
                        boxed(async move {
                            started.open();
                            allowed.wait().await;
                            Ok(session.default_runtime_info())
                        })
                    }))
                    .build())
            })
        });
    }
    let lc = Lc::with_client(client).await;
    let manager = lc.manager.clone();
    let config = lc.config();
    let creation = tokio::spawn(async move {
        manager
            .create_agent(config, None, CreateAgentOptions::default())
            .await
    });
    within(info_started.wait()).await;

    lc.manager.prepare_for_shutdown();
    let agent_id = id_n(1);
    let closing = {
        let manager = lc.manager.clone();
        let id = agent_id.clone();
        tokio::spawn(async move { manager.close_agent(&id).await })
    };
    settle().await;
    info_allowed.open();

    let error = creation.await.unwrap().unwrap_err();
    assert!(matches!(error, CoreError::ShuttingDown));
    closing.await.unwrap().unwrap();
    lc.flush().await;
    assert!(lc.manager.list_agents(true).is_empty());
    assert_eq!(
        lc.record(&agent_id).await.unwrap().last_status,
        AgentLifecycle::Closed
    );
}

/// `HeldReloadCloseClient`: the created session's close is held, a resumed session records its
/// close.
fn held_reload_client() -> (MockClient, Gate, Gate) {
    let client = MockClient::new("codex");
    let started = Gate::new();
    let allowed = Gate::new();
    {
        let (started, allowed) = (started.clone(), allowed.clone());
        client.on_create(move |req| {
            let (started, allowed) = (started.clone(), allowed.clone());
            boxed(async move { Ok(held_close_session(&started, &allowed, &req.config)) })
        });
    }
    (client, started, allowed)
}

#[tokio::test]
async fn reload_leaves_a_closed_durable_snapshot_when_shutdown_starts_during_the_swap() {
    let (client, close_started, close_allowed) = held_reload_client();
    let lc = Lc::with_client(client).await;
    let agent = lc.create().await;

    let reload = {
        let (manager, id) = (lc.manager.clone(), agent.id.clone());
        tokio::spawn(async move {
            manager
                .reload_agent_session(&id, None, ReloadOptions::default())
                .await
        })
    };
    within(close_started.wait()).await;

    lc.manager.prepare_for_shutdown();
    let closing = {
        let (manager, id) = (lc.manager.clone(), agent.id.clone());
        tokio::spawn(async move { manager.close_agent(&id).await })
    };
    close_allowed.open();

    closing.await.unwrap().unwrap();
    let error = reload.await.unwrap().unwrap_err();
    assert!(matches!(error, CoreError::ShuttingDown), "{error}");
    lc.flush().await;
    assert!(lc.manager.list_agents(true).is_empty());
    assert_eq!(
        lc.record(&agent.id).await.unwrap().last_status,
        AgentLifecycle::Closed
    );
    assert!(
        lc.client.resumes().is_empty(),
        "no replacement session was created"
    );
}

#[tokio::test]
async fn reload_does_not_create_a_replacement_when_the_closed_snapshot_cannot_be_persisted() {
    let (client, close_started, close_allowed) = held_reload_client();
    let lc = Lc::with_client(client).await;
    let agent = lc.create().await;
    lc.flush().await;
    let storage = lc.dir.path().join("agents");
    std::fs::remove_dir_all(&storage).unwrap();
    std::fs::write(&storage, "blocks the storage directory").unwrap();

    let reload = {
        let (manager, id) = (lc.manager.clone(), agent.id.clone());
        tokio::spawn(async move {
            manager
                .reload_agent_session(&id, None, ReloadOptions::default())
                .await
        })
    };
    within(close_started.wait()).await;
    close_allowed.open();

    assert!(reload.await.unwrap().is_err());
    lc.manager.flush_for_shutdown().await;
    assert!(lc.manager.list_agents(true).is_empty());
    assert!(lc.client.created_sessions()[0].is_closed());
    assert!(lc.client.resumes().is_empty());
}

// ---- reload ----

#[tokio::test]
async fn set_agent_mode_persists_the_selected_mode_across_session_reload() {
    let lc = Lc::new().await;
    let mut config = lc.config();
    config.mode_id = Some("auto".into());
    let agent = lc.create_with(config, CreateAgentOptions::default()).await;

    lc.manager
        .set_agent_mode(&agent.id, "full-access")
        .await
        .unwrap();
    let before = lc.manager.get_agent(&agent.id).unwrap();
    assert_eq!(before.config.mode_id.as_deref(), Some("full-access"));
    assert_eq!(before.current_mode_id.as_deref(), Some("full-access"));

    let reloaded = lc
        .manager
        .reload_agent_session(&agent.id, None, ReloadOptions::default())
        .await
        .unwrap();
    assert_eq!(reloaded.config.mode_id.as_deref(), Some("full-access"));
    assert_eq!(reloaded.current_mode_id.as_deref(), Some("full-access"));
}

#[tokio::test]
async fn reload_releases_the_original_writer_before_resuming_the_same_session() {
    let client = MockClient::new("codex");
    let current: Arc<Mutex<Option<MockSession>>> = Arc::default();
    {
        let current = current.clone();
        client.on_create(move |req| {
            let current = current.clone();
            boxed(async move {
                let session = session_for(&req.config);
                *current.lock().unwrap() = Some(session.clone());
                Ok(session)
            })
        });
    }
    {
        let current = current.clone();
        client.on_resume(move |req| {
            let current = current.clone();
            boxed(async move {
                if current
                    .lock()
                    .unwrap()
                    .as_ref()
                    .is_some_and(|s| !s.is_closed())
                {
                    return Err(ProviderError::provider(
                        "thread already has an active writer",
                    ));
                }
                let cwd = req
                    .overrides
                    .as_ref()
                    .and_then(|o| o.get("cwd"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("/tmp")
                    .to_owned();
                let session = session_for(&AgentSessionConfig::new("codex", cwd));
                session.set_persistence(req.handle.clone());
                *current.lock().unwrap() = Some(session.clone());
                Ok(session)
            })
        });
    }
    let lc = Lc::with_client(client).await;
    let created = lc.create().await;
    for _ in 0..3 {
        let reloaded = lc
            .manager
            .reload_agent_session(
                &created.id,
                None,
                ReloadOptions {
                    rehydrate_from_disk: true,
                },
            )
            .await
            .unwrap();
        assert_eq!(reloaded.id, created.id);
        assert_eq!(
            reloaded.persistence.as_ref().map(|p| &p.session_id),
            created.persistence.as_ref().map(|p| &p.session_id)
        );
    }
    lc.manager.close_agent(&created.id).await.unwrap();
}

#[tokio::test]
async fn opening_during_reload_waits_for_the_replacement() {
    let (client, close_started, close_allowed) = held_reload_client();
    let lc = Lc::with_client(client).await;
    let created = lc.create().await;

    let reloading = {
        let (manager, id) = (lc.manager.clone(), created.id.clone());
        tokio::spawn(async move {
            manager
                .reload_agent_session(&id, None, ReloadOptions::default())
                .await
        })
    };
    within(close_started.wait()).await;
    let opened = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let opening = {
        let (manager, id, opened) = (lc.manager.clone(), created.id.clone(), opened.clone());
        tokio::spawn(async move {
            let agent = manager
                .ensure_agent_loaded(&id, LoadOptions::default())
                .await
                .unwrap();
            opened.store(true, Ordering::SeqCst);
            agent
        })
    };
    settle().await;
    assert!(!opened.load(Ordering::SeqCst));
    close_allowed.open();
    let replacement = reloading.await.unwrap().unwrap();
    let loaded = opening.await.unwrap();
    assert_eq!(loaded.persistence, replacement.persistence);
    assert_eq!(
        lc.client.resumes().len(),
        1,
        "the load reused the replacement"
    );
    lc.manager.close_agent(&created.id).await.unwrap();
}

#[tokio::test]
async fn closing_during_reload_leaves_the_replacement_closed() {
    let (client, close_started, close_allowed) = held_reload_client();
    let lc = Lc::with_client(client).await;
    let created = lc.create().await;

    let reloading = {
        let (manager, id) = (lc.manager.clone(), created.id.clone());
        tokio::spawn(async move {
            manager
                .reload_agent_session(&id, None, ReloadOptions::default())
                .await
        })
    };
    within(close_started.wait()).await;
    let closing = {
        let (manager, id) = (lc.manager.clone(), created.id.clone());
        tokio::spawn(async move { manager.close_agent(&id).await })
    };
    close_allowed.open();
    reloading.await.unwrap().unwrap();
    closing.await.unwrap().unwrap();

    assert!(lc.manager.get_agent(&created.id).is_none());
    assert!(
        lc.client.created_sessions()[1].is_closed(),
        "the replacement was closed"
    );
    assert_eq!(
        lc.record(&created.id).await.unwrap().last_status,
        AgentLifecycle::Closed
    );
}

#[tokio::test]
async fn retrying_a_timed_out_reload_waits_for_the_original_close_to_finish() {
    let (client, _close_started, close_allowed) = held_reload_client();
    let lc = Lc::with(
        client,
        LcOptions {
            tweak: Box::new(|o: &mut ManagerOptions| {
                o.reload_session_close_timeout = Duration::from_millis(10);
            }),
            ..LcOptions::default()
        },
    )
    .await;
    let created = lc.create().await;

    let error = lc
        .manager
        .reload_agent_session(&created.id, None, ReloadOptions::default())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("Timed out closing"));
    assert_eq!(
        lc.manager.get_agent(&created.id).unwrap().lifecycle,
        AgentLifecycle::Error
    );

    let retry = {
        let (manager, id) = (lc.manager.clone(), created.id.clone());
        tokio::spawn(async move {
            manager
                .reload_agent_session(&id, None, ReloadOptions::default())
                .await
        })
    };
    close_allowed.open();
    let reloaded = within(retry).await.unwrap().unwrap();
    assert_eq!(reloaded.id, created.id);
    assert!(lc.client.created_sessions()[0].is_closed());
    lc.manager.close_agent(&created.id).await.unwrap();
}

#[tokio::test]
async fn failed_reload_retains_the_closed_agent_for_a_later_resume() {
    let client = MockClient::new("codex");
    let fail = Arc::new(std::sync::atomic::AtomicBool::new(true));
    {
        let fail = fail.clone();
        client.on_resume(move |req| {
            let fail = fail.clone();
            boxed(async move {
                if fail.load(Ordering::SeqCst) {
                    return Err(ProviderError::provider("resume unavailable"));
                }
                let session = session_for(&AgentSessionConfig::new("codex", "/tmp"));
                session.set_persistence(req.handle.clone());
                Ok(session)
            })
        });
    }
    let lc = Lc::with_client(client).await;
    let created = lc.create_titled("Keep me").await;

    let error = lc
        .manager
        .reload_agent_session(&created.id, None, ReloadOptions::default())
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "resume unavailable");
    assert!(lc.manager.get_agent(&created.id).is_none());
    let record = lc.record(&created.id).await.unwrap();
    assert_eq!(record.last_status, AgentLifecycle::Closed);
    assert_eq!(record.title.value().map(String::as_str), Some("Keep me"));
    assert_eq!(
        record.persistence.value().map(|p| p.session_id.clone()),
        created.persistence.as_ref().map(|p| p.session_id.clone())
    );

    fail.store(false, Ordering::SeqCst);
    let recovered = lc.load(&created.id).await.unwrap();
    assert_eq!(recovered.id, created.id);
    assert_eq!(
        lc.record(&created.id)
            .await
            .unwrap()
            .title
            .value()
            .map(String::as_str),
        Some("Keep me")
    );
    lc.manager.close_agent(&created.id).await.unwrap();
}

async fn reload_does_not_resume_when_the_previous_session_close_fails(hang: bool) {
    let client = MockClient::new("codex");
    client.on_create(move |req| {
        boxed(async move {
            Ok(MockSession::builder("codex")
                .config(&req.config)
                .auto_completing()
                .behavior(ScriptedBehavior::new().on_close(move |session| {
                    boxed(async move {
                        session.with_state(|s| s.closed = true);
                        if hang {
                            std::future::pending::<()>().await;
                        }
                        Err(ProviderError::provider("close rejected"))
                    })
                }))
                .build())
        })
    });
    let lc = Lc::with(
        client,
        LcOptions {
            tweak: Box::new(|o: &mut ManagerOptions| {
                o.reload_session_close_timeout = Duration::from_millis(10);
            }),
            ..LcOptions::default()
        },
    )
    .await;
    let agent = lc.create().await;

    for _ in 0..2 {
        let error = lc
            .manager
            .reload_agent_session(&agent.id, None, ReloadOptions::default())
            .await
            .unwrap_err();
        let expected = if hang {
            "Timed out closing previous session"
        } else {
            "close rejected"
        };
        assert!(error.to_string().contains(expected), "{error}");
    }
    assert!(lc.client.created_sessions()[0].is_closed());
    assert!(lc.client.resumes().is_empty());
    assert!(
        lc.manager.get_agent(&agent.id).is_some(),
        "the live agent keeps its session"
    );
    lc.manager.flush().await;
}

#[tokio::test]
async fn reload_does_not_resume_when_the_previous_session_close_fails_hang() {
    reload_does_not_resume_when_the_previous_session_close_fails(true).await;
}

#[tokio::test]
async fn reload_does_not_resume_when_the_previous_session_close_fails_reject() {
    reload_does_not_resume_when_the_previous_session_close_fails(false).await;
}

#[tokio::test]
async fn reload_passes_daemon_launch_env_through_the_provider_launch_context() {
    let lc = Lc::new().await;
    let agent = lc.create().await;
    let expected = |id: &str, cwd: &str| {
        let launch = lc.client.creates();
        let _ = launch;
        json!({"agentId": id, "env": {"PASEO_AGENT_ID": id, "PASEO_AGENT_CWD": cwd}})
    };
    let create_context = lc.client.creates()[0].launch_context.clone().unwrap();
    assert_eq!(
        serde_json::to_value(&create_context).unwrap(),
        expected(&agent.id, &lc.cwd())
    );

    let mut overrides = paseo_provider_api::JsonObject::new();
    overrides.insert("systemPrompt".into(), json!("reloaded prompt"));
    lc.manager
        .reload_agent_session(&agent.id, Some(overrides), ReloadOptions::default())
        .await
        .unwrap();
    let resume_context = lc.client.resumes()[0].launch_context.clone().unwrap();
    assert_eq!(
        serde_json::to_value(&resume_context).unwrap(),
        expected(&agent.id, &lc.cwd())
    );
}

#[tokio::test]
async fn reload_preserves_timeline_and_does_not_force_history_replay() {
    let client = MockClient::new("codex");
    client.on_resume(|req| {
        boxed(async move {
            let session = MockSession::builder("codex")
                .auto_completing()
                .behavior(ScriptedBehavior::new().on_history(|session| {
                    boxed(async move {
                        let _ = session;
                        Ok(vec![assistant(
                            "history replay from provider",
                            "codex",
                            None,
                        )])
                    })
                }))
                .build();
            session.set_persistence(req.handle.clone());
            Ok(session)
        })
    });
    let lc = Lc::with_client(client).await;
    let agent = lc.create().await;
    lc.manager
        .append_timeline_item(&agent.id, assistant_item("keep this timeline in memory"))
        .await
        .unwrap();
    lc.manager
        .hydrate_timeline_from_provider(&agent.id, Default::default())
        .await
        .unwrap();
    let before = lc.manager.get_timeline(&agent.id).await.unwrap();
    assert_eq!(before.len(), 1);

    let mut overrides = paseo_provider_api::JsonObject::new();
    overrides.insert("systemPrompt".into(), json!("reloaded prompt"));
    lc.manager
        .reload_agent_session(&agent.id, Some(overrides), ReloadOptions::default())
        .await
        .unwrap();
    assert_eq!(lc.manager.get_timeline(&agent.id).await.unwrap(), before);

    // A reload that reset `historyPrimed` would replay provider history and append an item.
    lc.manager
        .hydrate_timeline_from_provider(&agent.id, Default::default())
        .await
        .unwrap();
    assert_eq!(lc.manager.get_timeline(&agent.id).await.unwrap(), before);
}

fn running_child(id: &str) -> AgentStreamEvent {
    AgentStreamEvent::ProviderSubagent {
        provider: "codex".into(),
        event: Box::new(ProviderSubagentInputEvent::Upsert {
            id: id.into(),
            title: Some(Some("Child".into())),
            description: None,
            status: Some(ProviderSubagentStatus::Running),
            tool_call_id: None,
            cwd: None,
            subtitle: None,
            parent_subagent_id: None,
            timestamp: None,
        }),
    }
}

#[tokio::test]
async fn reload_clears_provider_children_before_rehydrating_from_disk() {
    let lc = Lc::new().await;
    let agent = lc.create().await;
    lc.client.created_sessions()[0].push_event(running_child("stale-child"));
    settle().await;
    assert_eq!(
        lc.manager.list_provider_subagents(&agent.id).unwrap().len(),
        1
    );

    lc.manager
        .reload_agent_session(
            &agent.id,
            None,
            ReloadOptions {
                rehydrate_from_disk: true,
            },
        )
        .await
        .unwrap();
    assert!(lc
        .manager
        .list_provider_subagents(&agent.id)
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn reload_terminalizes_running_provider_children_when_preserving_history() {
    let lc = Lc::new().await;
    let agent = lc.create().await;
    lc.client.created_sessions()[0].push_event(running_child("running-child"));
    settle().await;
    assert_eq!(
        lc.manager.list_provider_subagents(&agent.id).unwrap().len(),
        1
    );

    lc.manager
        .reload_agent_session(&agent.id, None, ReloadOptions::default())
        .await
        .unwrap();
    let children = lc.manager.list_provider_subagents(&agent.id).unwrap();
    assert_eq!(children.len(), 1);
    assert_eq!(children[0].id, "running-child");
    assert_eq!(children[0].status, ProviderSubagentStatus::Canceled);
}

#[tokio::test]
async fn reload_preserves_current_title_when_config_title_is_unset() {
    let lc = Lc::new().await;
    let agent = lc.create().await;
    lc.manager
        .set_title(&agent.id, "Generated title")
        .await
        .unwrap();

    let before = lc.record(&agent.id).await.unwrap();
    assert_eq!(
        before.title.value().map(String::as_str),
        Some("Generated title")
    );
    assert!(before
        .config
        .value()
        .is_none_or(|c| c.extra.get("title").is_none()));

    lc.manager
        .reload_agent_session(&agent.id, None, ReloadOptions::default())
        .await
        .unwrap();
    let after = lc.record(&agent.id).await.unwrap();
    assert_eq!(
        after.title.value().map(String::as_str),
        Some("Generated title")
    );
}

#[tokio::test]
async fn reload_cancels_active_run_and_resumes_existing_session_once_thread_started_is_observed() {
    let client = MockClient::new("codex");
    let counter = Arc::new(AtomicU32::new(0));
    {
        let counter = counter.clone();
        client.on_create(move |req| {
            let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
            boxed(async move { Ok(delayed_persistence_session(&req.config, n, false)) })
        });
    }
    client.on_resume(|req| {
        boxed(async move {
            let session =
                delayed_persistence_session(&AgentSessionConfig::new("codex", "/tmp"), 0, true);
            session.set_persistence(req.handle.clone());
            Ok(session)
        })
    });
    let lc = Lc::with_client(client).await;
    let snapshot = lc.create().await;
    assert!(snapshot.persistence.is_none());

    let mut stream = lc
        .manager
        .stream_agent(&snapshot.id, PromptInput::from("hello"), Default::default())
        .await
        .unwrap();
    let first = stream.next().await.unwrap().unwrap();
    assert!(matches!(first, AgentStreamEvent::TurnStarted { .. }));
    // `thread_started` is a session-level event, not forwarded to the foreground stream.
    within(async {
        loop {
            let active = lc.manager.get_agent(&snapshot.id).unwrap();
            if active.persistence.as_ref().map(|p| p.session_id.as_str())
                == Some("delayed-session-1")
            {
                break;
            }
            settle().await;
        }
    })
    .await;
    assert_eq!(
        lc.manager.get_agent(&snapshot.id).unwrap().lifecycle,
        AgentLifecycle::Running
    );

    let mut overrides = paseo_provider_api::JsonObject::new();
    overrides.insert("systemPrompt".into(), json!("voice mode on"));
    let reloaded = lc
        .manager
        .reload_agent_session(&snapshot.id, Some(overrides), ReloadOptions::default())
        .await
        .unwrap();
    assert_eq!(lc.client.creates().len(), 1);
    assert_eq!(lc.client.resumes().len(), 1);
    assert_eq!(
        reloaded.persistence.as_ref().map(|p| p.session_id.as_str()),
        Some("delayed-session-1")
    );
    while stream.next().await.unwrap().is_some() {}
}

/// `DelayedPersistenceSession`: persistence appears once the turn reports `thread_started`;
/// `interrupt` and `close` cancel the turn.
fn delayed_persistence_session(
    config: &AgentSessionConfig,
    number: u32,
    ready: bool,
) -> MockSession {
    let session_id = format!("delayed-session-{number}");
    let gate = Gate::new();
    let session = MockSession::builder("codex")
        .config(config)
        .behavior(
            ScriptedBehavior::new()
                .on_start({
                    let (gate, session_id) = (gate.clone(), session_id.clone());
                    move |session, prompt, options| {
                        let (gate, session_id) = (gate.clone(), session_id.clone());
                        boxed(async move {
                            let n = session.record_start(prompt, options);
                            let turn_id = format!("delayed-turn-{n}");
                            let soon = session.clone();
                            let turn = turn_id.clone();
                            tokio::spawn(async move {
                                tokio::time::sleep(Duration::ZERO).await;
                                soon.push_event(AgentStreamEvent::TurnStarted {
                                    provider: "codex".into(),
                                    turn_id: Some(turn.clone()),
                                });
                                soon.set_persistence(paseo_agent_types::PersistenceHandle {
                                    provider: "codex".into(),
                                    session_id: session_id.clone(),
                                    native_handle: None,
                                    metadata: None,
                                });
                                soon.push_event(AgentStreamEvent::ThreadStarted {
                                    session_id: session_id.clone(),
                                    provider: "codex".into(),
                                });
                                gate.wait().await;
                                let interrupted =
                                    soon.with_state(|s| s.closed || s.interrupt_count > 0);
                                soon.push_event(if interrupted {
                                    AgentStreamEvent::TurnCanceled {
                                        provider: "codex".into(),
                                        reason: "Interrupted".into(),
                                        turn_id: Some(turn),
                                    }
                                } else {
                                    AgentStreamEvent::TurnCompleted {
                                        provider: "codex".into(),
                                        usage: None,
                                        turn_id: Some(turn),
                                    }
                                });
                            });
                            Ok(paseo_provider_api::TurnHandle { turn_id })
                        })
                    }
                })
                .on_interrupt({
                    let gate = gate.clone();
                    move |session| {
                        let gate = gate.clone();
                        boxed(async move {
                            session.with_state(|s| s.interrupt_count += 1);
                            gate.open();
                            Ok(())
                        })
                    }
                })
                .on_close({
                    let gate = gate.clone();
                    move |session| {
                        let gate = gate.clone();
                        boxed(async move {
                            session.with_state(|s| s.closed = true);
                            gate.open();
                            Ok(())
                        })
                    }
                })
                .on_runtime_info({
                    let session_id = session_id.clone();
                    move |session| {
                        let session_id = session_id.clone();
                        boxed(async move {
                            let ready = session.with_state(|s| s.persistence.is_some());
                            Ok(paseo_agent_types::AgentRuntimeInfo {
                                provider: "codex".into(),
                                session_id: ready.then_some(session_id),
                                model: Some(None),
                                thinking_option_id: None,
                                mode_id: Some(None),
                                extra: None,
                            })
                        })
                    }
                }),
        )
        .build();
    if ready {
        session.set_persistence(paseo_agent_types::PersistenceHandle {
            provider: "codex".into(),
            session_id,
            native_handle: None,
            metadata: None,
        });
    } else {
        session.with_state(|s| s.persistence_hidden = true);
    }
    session
}

// ---- close ----

#[tokio::test]
async fn close_during_in_flight_stream_does_not_clear_persistence_session_id() {
    let client = MockClient::new("codex");
    client.on_create(|req| {
        boxed(async move {
            Ok(MockSession::builder("codex")
                .config(&req.config)
                .behavior(
                    ScriptedBehavior::new()
                        .on_start(|session, prompt, options| {
                            boxed(async move {
                                let n = session.record_start(prompt, options);
                                let turn_id = format!("turn-{n}");
                                session.push_event_soon(provider_event_turn_started(
                                    "codex", &turn_id,
                                ));
                                Ok(paseo_provider_api::TurnHandle { turn_id })
                            })
                        })
                        .on_close(|session| {
                            boxed(async move {
                                session.with_state(|s| s.closed = true);
                                // A closed session no longer describes its thread.
                                session.with_state(|s| s.persistence_hidden = true);
                                session.push_event(turn_canceled(Some("turn-1"), "closed"));
                                Ok(())
                            })
                        }),
                )
                .build())
        })
    });
    let lc = Lc::with_client(client).await;
    let snapshot = lc.create().await;

    let mut stream = lc
        .manager
        .stream_agent(&snapshot.id, PromptInput::from("hello"), Default::default())
        .await
        .unwrap();
    stream.next().await.unwrap();
    lc.manager.close_agent(&snapshot.id).await.unwrap();
    while stream.next().await.unwrap().is_some() {}
    lc.flush().await;

    let persisted = lc.record(&snapshot.id).await.unwrap();
    assert_eq!(
        persisted.persistence.value().map(|p| p.session_id.clone()),
        snapshot.persistence.as_ref().map(|p| p.session_id.clone())
    );
}

#[tokio::test]
async fn close_agent_persists_one_final_closed_snapshot() {
    let counter = Arc::new(CountingPersister::default());
    let lc = {
        let counter = counter.clone();
        Lc::with(
            MockClient::new("codex"),
            LcOptions {
                tweak: Box::new(move |o: &mut ManagerOptions| {
                    o.persister = Some(counter.clone());
                }),
                ..LcOptions::default()
            },
        )
        .await
    };
    counter.attach(lc.store.clone());
    let agent = lc.create().await;
    lc.manager.flush().await;
    let before = counter.count();

    lc.manager.close_agent(&agent.id).await.unwrap();
    lc.manager.flush().await;
    assert_eq!(counter.count(), before + 1);
}

#[tokio::test]
async fn idle_agents_remain_resident_until_an_explicit_lifecycle_action_closes_them() {
    let lc = Lc::new().await;
    let agent = lc.create().await;
    tokio::time::sleep(Duration::from_millis(25)).await;
    assert_eq!(
        lc.manager.get_agent(&agent.id).unwrap().lifecycle,
        AgentLifecycle::Idle
    );
    assert!(!lc.client.created_sessions()[0].is_closed());

    lc.manager
        .run_agent(
            &agent.id,
            PromptInput::from("Continue on the resident runtime"),
            Default::default(),
        )
        .await
        .unwrap();
    assert_eq!(
        lc.manager.get_agent(&agent.id).unwrap().lifecycle,
        AgentLifecycle::Idle
    );
    assert!(lc.client.resumes().is_empty());
}

#[tokio::test]
async fn explicit_close_cancels_running_provider_subagents_before_resume() {
    let lc = Lc::new().await;
    let parent = lc.create().await;
    let session = lc.client.created_sessions()[0].clone();
    session.push_event(running_child("provider-child-running"));
    session.push_event(running_child("provider-child-finishing"));
    lc.manager.flush().await;
    session.push_event(AgentStreamEvent::ProviderSubagent {
        provider: "codex".into(),
        event: Box::new(ProviderSubagentInputEvent::Upsert {
            id: "provider-child-finishing".into(),
            title: None,
            description: None,
            status: Some(ProviderSubagentStatus::Completed),
            tool_call_id: None,
            cwd: None,
            subtitle: None,
            parent_subagent_id: None,
            timestamp: None,
        }),
    });
    lc.manager.close_agent(&parent.id).await.unwrap();
    lc.load(&parent.id).await.unwrap();

    let status = |id: &str| {
        lc.manager
            .get_provider_subagent(&parent.id, id)
            .unwrap()
            .unwrap()
            .status
    };
    assert_eq!(
        status("provider-child-running"),
        ProviderSubagentStatus::Canceled
    );
    assert_eq!(
        status("provider-child-finishing"),
        ProviderSubagentStatus::Completed
    );
}

#[tokio::test]
async fn load_waits_for_an_in_flight_explicit_close_and_creates_one_resumed_runtime() {
    let client = MockClient::new("codex");
    let close_started = Gate::new();
    let close_allowed = Gate::new();
    {
        let (started, allowed) = (close_started.clone(), close_allowed.clone());
        client.on_create(move |req| {
            let (started, allowed) = (started.clone(), allowed.clone());
            boxed(async move { Ok(held_close_session(&started, &allowed, &req.config)) })
        });
    }
    let lc = Lc::with_client(client).await;
    let created = lc.create().await;
    let close = {
        let (manager, id) = (lc.manager.clone(), created.id.clone());
        tokio::spawn(async move { manager.close_agent(&id).await })
    };
    within(close_started.wait()).await;
    let loads = {
        let (m1, m2, id) = (lc.manager.clone(), lc.manager.clone(), created.id.clone());
        let id2 = id.clone();
        tokio::spawn(async move {
            tokio::join!(
                m1.ensure_agent_loaded(&id, LoadOptions::default()),
                m2.ensure_agent_loaded(&id2, LoadOptions::default())
            )
        })
    };
    settle().await;
    assert!(lc.client.resumes().is_empty());
    close_allowed.open();
    let (first, second) = loads.await.unwrap();
    close.await.unwrap().unwrap();

    assert_eq!(first.unwrap().id, created.id);
    assert_eq!(second.unwrap().id, created.id);
    assert_eq!(lc.client.resumes().len(), 1);
}

#[tokio::test]
async fn concurrent_explicit_closes_tear_down_the_runtime_once() {
    let client = MockClient::new("codex");
    let close_started = Gate::new();
    let close_allowed = Gate::new();
    let close_count = Arc::new(AtomicU32::new(0));
    {
        let (started, allowed, count) = (
            close_started.clone(),
            close_allowed.clone(),
            close_count.clone(),
        );
        client.on_create(move |req| {
            let (started, allowed, count) = (started.clone(), allowed.clone(), count.clone());
            boxed(async move {
                Ok(MockSession::builder("codex")
                    .config(&req.config)
                    .auto_completing()
                    .behavior(ScriptedBehavior::new().on_close(move |_| {
                        let (started, allowed, count) =
                            (started.clone(), allowed.clone(), count.clone());
                        boxed(async move {
                            count.fetch_add(1, Ordering::SeqCst);
                            started.open();
                            allowed.wait().await;
                            Ok(())
                        })
                    }))
                    .build())
            })
        });
    }
    let lc = Lc::with_client(client).await;
    let agent = lc.create().await;
    let first = {
        let (manager, id) = (lc.manager.clone(), agent.id.clone());
        tokio::spawn(async move { manager.close_agent(&id).await })
    };
    within(close_started.wait()).await;
    let second = {
        let (manager, id) = (lc.manager.clone(), agent.id.clone());
        tokio::spawn(async move { manager.close_agent(&id).await })
    };
    settle().await;
    close_allowed.open();
    first.await.unwrap().unwrap();
    second.await.unwrap().unwrap();
    assert_eq!(close_count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn provider_close_failure_retains_the_runtime_for_cleanup_instead_of_allowing_another_writer()
{
    let client = MockClient::new("codex");
    client.on_create(|req| {
        boxed(async move {
            Ok(failing_close_session(
                "provider cleanup failed",
                &req.config,
            ))
        })
    });
    let lc = Lc::with_client(client).await;
    let created = lc.create().await;

    let error = lc.manager.close_agent(&created.id).await.unwrap_err();
    assert_eq!(error.to_string(), "provider cleanup failed");
    let live = lc.manager.get_agent(&created.id).unwrap();
    assert_eq!(live.lifecycle, AgentLifecycle::Idle);
    let stored = lc.record(&created.id).await.unwrap();
    assert_eq!(stored.last_status, AgentLifecycle::Idle);
    assert!(!stored.is_archived());

    let loaded = lc.load(&created.id).await.unwrap();
    assert_eq!(loaded.lifecycle, AgentLifecycle::Idle);
}

// ---- restore and startup ----

#[tokio::test]
async fn concurrent_native_restores_run_once_before_resuming_the_same_agent() {
    let client = MockClient::new("codex");
    let restore_started = Gate::new();
    let restore_allowed = Gate::new();
    {
        let (started, allowed) = (restore_started.clone(), restore_allowed.clone());
        client.on_unarchive(move || {
            let (started, allowed) = (started.clone(), allowed.clone());
            boxed(async move {
                started.open();
                allowed.wait().await;
                Ok(())
            })
        });
    }
    let lc = Lc::with_client(client).await;
    let created = lc.create().await;
    lc.manager.archive_agent(&created.id).await.unwrap();

    let first = {
        let (manager, id) = (lc.manager.clone(), created.id.clone());
        tokio::spawn(async move { manager.unarchive_snapshot(&id, Default::default()).await })
    };
    within(restore_started.wait()).await;
    let second = {
        let (manager, id) = (lc.manager.clone(), created.id.clone());
        tokio::spawn(async move { manager.unarchive_snapshot(&id, Default::default()).await })
    };
    let load = {
        let (manager, id) = (lc.manager.clone(), created.id.clone());
        tokio::spawn(async move {
            manager
                .ensure_agent_loaded(&id, LoadOptions::default())
                .await
        })
    };
    settle().await;
    restore_allowed.open();
    first.await.unwrap().unwrap();
    second.await.unwrap().unwrap();
    load.await.unwrap().unwrap();

    assert_eq!(lc.client.unarchived_handles().len(), 1);
    let resumes = lc.client.resumes();
    assert_eq!(resumes.len(), 1);
    assert_eq!(
        resumes[0]
            .options
            .as_ref()
            .and_then(|o| o.purpose.as_deref()),
        Some("interactive")
    );
    assert!(!lc.record(&created.id).await.unwrap().is_archived());
    assert_eq!(
        lc.manager
            .get_agent(&created.id)
            .unwrap()
            .persistence
            .as_ref()
            .map(|p| p.session_id.clone()),
        created.persistence.as_ref().map(|p| p.session_id.clone())
    );
}

#[tokio::test]
async fn failed_startup_history_closes_the_session_without_registering_an_agent() {
    let client = MockClient::new("codex");
    let session: Arc<Mutex<Option<MockSession>>> = Arc::default();
    {
        let session = session.clone();
        client.on_resume(move |_| {
            let session = session.clone();
            boxed(async move {
                let created = MockSession::builder("codex")
                    .initial_timeline(vec![paseo_agent_types::AgentTimelineItem::Notification {
                        level: paseo_agent_types::NotificationLevel::Info,
                        message: "Runtime v2".into(),
                    }])
                    .behavior(ScriptedBehavior::new().on_history(|_| {
                        boxed(async move { Err(ProviderError::provider("History unavailable")) })
                    }))
                    .build();
                *session.lock().unwrap() = Some(created.clone());
                Ok(created)
            })
        });
    }
    let lc = Lc::with_client(client).await;
    let error = lc
        .manager
        .resume_agent_from_persistence(
            handle("codex", "failed-history", &lc.cwd()),
            Some(obj(json!({"cwd": lc.cwd()}))),
            None,
            ResumeOptions::default(),
            None,
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("History unavailable"));
    assert!(lc.manager.list_agents(true).is_empty());
    assert!(session.lock().unwrap().as_ref().unwrap().is_closed());
}

#[tokio::test]
async fn commits_startup_notices_once_on_create_and_after_restored_history() {
    let notice = paseo_agent_types::AgentTimelineItem::Notification {
        level: paseo_agent_types::NotificationLevel::Info,
        message: "Runtime v2".into(),
    };
    let history = assistant_item("Existing conversation");
    let big_output = tool_call(
        "startup-output",
        paseo_agent_types::ToolCallStatus::Completed,
        &"x".repeat(1024 * 1024),
        None,
    );
    let limited_output = tool_call(
        "startup-output",
        paseo_agent_types::ToolCallStatus::Completed,
        &"x".repeat(64 * 1024),
        None,
    );
    let make_session = {
        let (notice, history, big_output) = (notice.clone(), history.clone(), big_output.clone());
        move |config: &AgentSessionConfig| {
            let events = vec![
                timeline_event(history.clone(), "codex", None),
                timeline_event(big_output.clone(), "codex", None),
                timeline_event(notice.clone(), "codex", None),
            ];
            MockSession::builder("codex")
                .config(config)
                .auto_completing()
                .initial_timeline(vec![notice.clone()])
                .behavior(ScriptedBehavior::new().on_history(move |_| {
                    let events = events.clone();
                    boxed(async move { Ok(events) })
                }))
                .build()
        }
    };
    let client = MockClient::new("codex");
    {
        let make = make_session.clone();
        client.on_create(move |req| {
            let make = make.clone();
            boxed(async move { Ok(make(&req.config)) })
        });
    }
    {
        let make = make_session.clone();
        client.on_resume(move |_| {
            let make = make.clone();
            boxed(async move { Ok(make(&AgentSessionConfig::new("codex", "/tmp"))) })
        });
    }
    let lc = Lc::with_client(client).await;
    let created = lc.create().await;
    assert_eq!(lc.items(&created.id).await, vec![notice.clone()]);

    let resumed = lc
        .manager
        .resume_agent_from_persistence(
            handle("codex", "existing", &lc.cwd()),
            Some(obj(json!({"cwd": lc.cwd()}))),
            None,
            ResumeOptions::default(),
            None,
        )
        .await
        .unwrap();
    lc.manager
        .hydrate_timeline_from_provider(&resumed.id, Default::default())
        .await
        .unwrap();
    let expected = vec![history.clone(), limited_output.clone(), notice.clone()];
    assert_eq!(lc.items(&resumed.id).await, expected);
    lc.manager
        .hydrate_timeline_from_provider(
            &resumed.id,
            paseo_core::HydrateOpts {
                force: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(lc.items(&resumed.id).await, expected);
    lc.manager.flush().await;
}

// ---- harness extras ----

/// `vi.spyOn(storage, "applySnapshot")`: counts snapshot writes and delegates to the store.
#[derive(Default)]
struct CountingPersister {
    count: AtomicU32,
    inner: Mutex<Option<Arc<paseo_core::StorePersister>>>,
}

impl CountingPersister {
    fn attach(&self, store: Arc<paseo_store::AgentStore>) {
        *self.inner.lock().unwrap() = Some(Arc::new(paseo_core::StorePersister::new(store)));
    }

    fn count(&self) -> u32 {
        self.count.load(Ordering::SeqCst)
    }

    fn inner(&self) -> Arc<paseo_core::StorePersister> {
        self.inner.lock().unwrap().clone().expect("attached")
    }
}

#[async_trait::async_trait]
impl paseo_core::SnapshotPersister for CountingPersister {
    async fn persist(
        &self,
        agent: &paseo_core::ManagedAgentView,
        options: paseo_core::PersistOptions,
    ) -> Result<(), String> {
        self.count.fetch_add(1, Ordering::SeqCst);
        self.inner().persist(agent, options).await
    }

    async fn stored_title(&self, agent_id: &str) -> Result<Option<Option<String>>, String> {
        self.inner().stored_title(agent_id).await
    }
}
