//! Scripted end-to-end run of the manager over a real [`AgentStore`]: create, prompt, second
//! prompt, cancel, close, resume, archive. The same script runs twice:
//!
//! - with `MockClient` (always), and
//! - with `HostAgentClient` over the real TS provider host and its `mock` load-test provider
//!   (skipped with a message when node/tsx or the TS checkout is unavailable; see
//!   `crates/paseo-provider-host/tests/integration.rs` for `PASEO_PROVIDER_HOST_NODE` and
//!   `PASEO_PROVIDER_HOST_SERVER_DIR`).
//!
//! Real time, no paused clock: the host is a child process.

mod common;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use common::*;
use paseo_agent_types::{AgentStreamEvent, AgentTimelineItem, AttentionReason};
use paseo_core::mock::{boxed, MockClient, MockSession, ScriptedBehavior};
use paseo_core::{
    AgentManager, CancelOutcome, CreateAgentOptions, Lifecycle, LoadOptions, ManagerOptions,
    ProviderRegistryUpdate, RunOpts, SubscribeOpts,
};
use paseo_provider_api::{AgentClient, AgentSessionConfig, PromptInput, TurnHandle};
use paseo_provider_host::{HostAgentClient, HostConfig, InitializeOptions, ProviderHostClient};
use paseo_store::AgentStore;
use serde_json::json;
use tokio::time::timeout;

const REPLY: &str = "scripted reply";

/// How a variant configures its two kinds of agent.
struct Variant {
    provider: &'static str,
    client: Arc<dyn AgentClient>,
    /// Answers every prompt with [`REPLY`] and completes.
    replying: fn(&mut AgentSessionConfig),
    /// Streams until interrupted.
    long: fn(&mut AgentSessionConfig),
}

fn replying_session(model: Option<String>) -> MockSession {
    let mut config = AgentSessionConfig::new("mock", "/unused");
    config.model = model;
    MockSession::builder("mock")
        .config(&config)
        .behavior(
            ScriptedBehavior::new().on_start(|session, prompt, options| {
                boxed(async move {
                    let n = session.record_start(prompt, options);
                    let turn = format!("turn-{n}");
                    let (soon, turn_id) = (session.clone(), turn.clone());
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::ZERO).await;
                        soon.push_event(provider_event_turn_started("mock", &turn_id));
                        soon.push_event(assistant(REPLY, "mock", Some(&turn_id)));
                        soon.push_event(AgentStreamEvent::TurnCompleted {
                            provider: "mock".into(),
                            usage: None,
                            turn_id: Some(turn_id),
                        });
                    });
                    Ok(TurnHandle { turn_id: turn })
                })
            }),
        )
        .build()
}

fn mock_variant() -> Variant {
    let client = MockClient::new("mock");
    client.on_create(|req| {
        boxed(async move {
            if req.config.model.as_deref() == Some("long") {
                let (session, _) = steering_session("mock");
                return Ok(session);
            }
            Ok(replying_session(req.config.model.clone()))
        })
    });
    client.on_resume(|req| {
        boxed(async move {
            let session = replying_session(None);
            session.set_persistence(req.handle.clone());
            Ok(session)
        })
    });
    Variant {
        provider: "mock",
        client: Arc::new(client),
        replying: |_| {},
        long: |config| config.model = Some("long".into()),
    }
}

/// `None` (with a SKIP message) when the TS host cannot run here.
async fn host_variant() -> Option<(Variant, ProviderHostClient)> {
    let node = std::env::var("PASEO_PROVIDER_HOST_NODE").unwrap_or_else(|_| "node".into());
    // PASEO_PROVIDER_HOST_SERVER_DIR points at packages/server; PASEO_REPO at the TS checkout root.
    let server_dir = match std::env::var("PASEO_PROVIDER_HOST_SERVER_DIR") {
        Ok(dir) => PathBuf::from(dir),
        Err(_) => match std::env::var("PASEO_REPO") {
            Ok(repo) => PathBuf::from(repo).join("packages/server"),
            Err(_) => {
                eprintln!(
                    "SKIP: set PASEO_REPO (TS paseo checkout) or PASEO_PROVIDER_HOST_SERVER_DIR"
                );
                return None;
            }
        },
    };
    if !server_dir.join("src/provider-host/main.ts").exists() {
        eprintln!(
            "SKIP: provider-host entry not found in {}",
            server_dir.display()
        );
        return None;
    }
    let tsx_ok = std::process::Command::new(&node)
        .args(["--import", "tsx", "-e", "0"])
        .current_dir(&server_dir)
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    if !tsx_ok {
        eprintln!(
            "SKIP: `{node} --import tsx` is unavailable in {}",
            server_dir.display()
        );
        return None;
    }
    let config = HostConfig::node_tsx(node, &server_dir, "src/provider-host/main.ts")
        .with_initialize(InitializeOptions {
            is_dev: Some(true),
            providers: Some(vec!["mock".into()]),
            ..InitializeOptions::default()
        });
    let host = ProviderHostClient::spawn(config, None)
        .await
        .expect("spawn host");
    let client = HostAgentClient::new(host.clone(), "mock");
    Some((
        Variant {
            provider: "mock",
            client: Arc::new(client),
            replying: |config| {
                config.model = Some("ten-second-stream".into());
                config.feature_values =
                    json!({"mockAssistantResponse": REPLY}).as_object().cloned();
            },
            long: |config| config.model = Some("ten-second-stream".into()),
        },
        host,
    ))
}

async fn within<T>(what: &str, future: impl std::future::Future<Output = T>) -> T {
    timeout(Duration::from_secs(30), future)
        .await
        .unwrap_or_else(|_| panic!("timed out: {what}"))
}

async fn script(variant: Variant) {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().to_string_lossy().into_owned();
    let store = Arc::new(AgentStore::new(dir.path().join("agents")));
    let mut providers = ProviderRegistryUpdate::default();
    providers
        .clients
        .insert(variant.provider.into(), variant.client.clone());
    let (hooks, hook_log) = RecordingHooks::new();
    let manager = AgentManager::new(ManagerOptions {
        hooks: Arc::new(hooks),
        store: Some(store.clone()),
        providers,
        ..ManagerOptions::default()
    });
    let events = collect(&manager, SubscribeOpts::default().without_replay()).await;
    let config = |tweak: fn(&mut AgentSessionConfig), title: &str| {
        let mut config = AgentSessionConfig::new(variant.provider, cwd.clone());
        config.title = Some(title.into());
        tweak(&mut config);
        config
    };

    // 1. create
    let agent = within(
        "create",
        manager.create_agent(
            config(variant.replying, "Scripted"),
            None,
            CreateAgentOptions::default(),
        ),
    )
    .await
    .unwrap();
    assert_eq!(agent.lifecycle, Lifecycle::Idle);
    assert!(
        agent.persistence.is_some(),
        "the provider described a persistence handle"
    );
    let id = agent.id.clone();

    // 2. first prompt
    let outcome = within(
        "first run",
        manager.run_agent(
            &id,
            PromptInput::from("first prompt"),
            RunOpts::with_client_message_id("msg-1"),
        ),
    )
    .await
    .unwrap();
    assert_eq!(outcome.final_text, REPLY);
    let view = manager.get_agent(&id).unwrap();
    assert_eq!(view.lifecycle, Lifecycle::Idle);
    assert!(matches!(
        view.attention,
        paseo_core::AttentionState::Required {
            reason: AttentionReason::Finished,
            ..
        }
    ));
    let items = manager.get_timeline_rows(&id).await.unwrap();
    assert!(
        matches!(&items[0].item, AgentTimelineItem::UserMessage { text, .. } if text == "first prompt")
    );
    assert!(items.iter().any(|row| matches!(&row.item, AgentTimelineItem::AssistantMessage { text, .. } if text.contains(REPLY))));
    let seqs: Vec<i64> = items.iter().map(|row| row.seq()).collect();
    assert!(
        seqs.windows(2).all(|w| w[0] < w[1]),
        "monotonic seqs: {seqs:?}"
    );

    // 3. record on disk
    manager.flush().await;
    store.flush().await.unwrap();
    let record = store.get(&id).await.unwrap().expect("record");
    assert_eq!(record.last_status, paseo_store::AgentLifecycle::Idle);
    assert_eq!(record.title.value().map(String::as_str), Some("Scripted"));
    assert_eq!(
        record.persistence.value().map(|p| p.provider.as_str()),
        Some(variant.provider)
    );

    // 4. second prompt through the stream API
    let mut stream = manager
        .stream_agent(
            &id,
            PromptInput::from("second prompt"),
            RunOpts::with_client_message_id("msg-2"),
        )
        .await
        .unwrap();
    let mut last = None;
    while let Some(event) = within("second stream", stream.next()).await.unwrap() {
        last = Some(event);
    }
    assert!(
        matches!(last, Some(AgentStreamEvent::TurnCompleted { .. })),
        "{last:?}"
    );
    let after_second = manager.get_timeline_rows(&id).await.unwrap();
    assert!(after_second.len() > items.len());
    assert_eq!(
        user_message_texts(
            &after_second
                .iter()
                .map(|r| r.item.clone())
                .collect::<Vec<_>>()
        ),
        ["first prompt", "second prompt"]
    );

    // 5. cancel a long run on a second agent
    let long = within(
        "create long",
        manager.create_agent(
            config(variant.long, "Long"),
            None,
            CreateAgentOptions::default(),
        ),
    )
    .await
    .unwrap();
    let mut long_stream = manager
        .stream_agent(
            &long.id,
            PromptInput::from("stream a lot"),
            RunOpts::default(),
        )
        .await
        .unwrap();
    let drain = tokio::spawn(async move { while let Ok(Some(_)) = long_stream.next().await {} });
    within(
        "run start",
        manager.wait_for_agent_run_start(&long.id, None),
    )
    .await
    .unwrap();
    // Accepting the prompt already reports `running`; the provider's own `turn_started` follows
    // `start_turn` one tick later. Interrupting before it would let it restart the agent.
    within("turn started", async {
        while manager.get_agent(&long.id).map(|v| v.lifecycle) != Some(Lifecycle::Running) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let cancel = within("cancel", manager.cancel_agent_run(&long.id))
        .await
        .unwrap();
    assert_eq!(cancel, CancelOutcome::Settled);
    within("drain", drain).await.unwrap();
    let long_view = manager.get_agent(&long.id).unwrap();
    manager.bus_barrier().await;
    settle().await;
    let seen: Vec<_> = events
        .streams(&long.id)
        .iter()
        .map(|e| format!("{:?}", stream_event(e)))
        .collect();
    assert_eq!(long_view.lifecycle, Lifecycle::Idle, "events: {seen:#?}");
    assert!(!manager.has_in_flight_run(&long.id));

    // 6. close, then resume through the loader
    within("close", manager.close_agent(&id)).await.unwrap();
    assert!(manager.get_agent(&id).is_none());
    manager.flush().await;
    store.flush().await.unwrap();
    assert_eq!(
        store.get(&id).await.unwrap().unwrap().last_status,
        paseo_store::AgentLifecycle::Closed
    );
    let resumed = within(
        "resume",
        manager.ensure_agent_loaded(&id, LoadOptions::default()),
    )
    .await
    .unwrap();
    assert_eq!(resumed.lifecycle, Lifecycle::Idle);
    let third = within(
        "run after resume",
        manager.run_agent(
            &id,
            PromptInput::from("third prompt"),
            RunOpts::with_client_message_id("msg-3"),
        ),
    )
    .await
    .unwrap();
    assert_eq!(third.final_text, REPLY);

    // 7. archive cascades nothing here, closes the runtime and marks the record
    let archived_at = within("archive", manager.archive_agent(&id)).await.unwrap();
    assert!(manager.get_agent(&id).is_none());
    let record = store.get(&id).await.unwrap().unwrap();
    assert_eq!(
        record.archived_at.value().map(String::as_str),
        Some(archived_at.as_str())
    );
    assert_eq!(
        hook_log.archived.lock().unwrap().as_slice(),
        std::slice::from_ref(&id)
    );

    // The bus delivered state for both agents, in order, ending with the closed archive state.
    manager.bus_barrier().await;
    settle().await;
    let states = events.states(&id);
    assert!(states.len() >= 6);
    assert_eq!(states.last().unwrap().lifecycle, Lifecycle::Closed);
    manager.prepare_for_shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scripted_run_with_the_mock_client() {
    script(mock_variant()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scripted_run_over_the_real_provider_host() {
    let Some((variant, host)) = host_variant().await else {
        return;
    };
    script(variant).await;
    host.shutdown().await.unwrap();
}
