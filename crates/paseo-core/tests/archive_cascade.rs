//! Ports of the archive, cascade, detach, unarchive, label and internal-agent cases of
//! `agent-manager.test.ts`, plus the stored-agent loader cases of `agent-loading.test.ts`.

mod common;

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use common::*;
use indexmap::IndexMap;
use paseo_agent_types::AttentionReason;
use paseo_core::mock::{boxed, Gate, MockClient, MockSession};
use paseo_core::{
    CreateAgentOptions, LoadOptions, ManagedAgentView, MetadataUpdates, PersistOptions,
    SnapshotPersister, UnarchiveUpdates, PARENT_AGENT_ID_LABEL,
};
use paseo_provider_api::{AgentSessionConfig, PromptInput, ProviderError};
use paseo_store::{AgentLifecycle, Nullable, StoredAgentRecord};
use serde_json::json;

const DESKTOP_TAB: &str = "paseo.open-agent-tab.desktop-client";
const MOBILE_TAB: &str = "paseo.open-agent-tab.mobile-client";

async fn within<T>(work: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(10), work)
        .await
        .expect("timed out")
}

fn labels(pairs: &[(&str, &str)]) -> IndexMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

fn titled(lc: &Lc, title: &str) -> AgentSessionConfig {
    let mut config = lc.config();
    config.title = Some(title.into());
    config
}

fn child_options(
    parent: &str,
    extra: &[(&str, &str)],
    workspace: Option<&str>,
) -> CreateAgentOptions {
    let mut all = vec![(PARENT_AGENT_ID_LABEL, parent)];
    all.extend_from_slice(extra);
    CreateAgentOptions {
        labels: Some(labels(&all)),
        workspace_id: workspace.map(str::to_owned),
        ..CreateAgentOptions::default()
    }
}

async fn create_titled(lc: &Lc, title: &str, options: CreateAgentOptions) -> Arc<ManagedAgentView> {
    lc.create_with(titled(lc, title), options).await
}

/// `expectArchivedAgentRecord`.
fn assert_archived_record(record: Option<StoredAgentRecord>, expected: AgentLifecycle) {
    let record = record.expect("record exists");
    let archived_at = record.archived_at.value().expect("archivedAt").clone();
    assert_eq!(archived_at.len(), 24, "{archived_at}");
    assert!(archived_at.ends_with('Z'));
    assert_eq!(record.last_status, expected);
    assert_eq!(record.requires_attention, Some(false));
    assert_eq!(record.attention_reason, Nullable::Null);
    assert_eq!(record.attention_timestamp, Nullable::Null);
}

fn is_archived(record: &Option<StoredAgentRecord>) -> bool {
    record.as_ref().is_some_and(StoredAgentRecord::is_archived)
}

// ---- labels, metadata, detach ----

#[tokio::test]
async fn set_labels_merges_and_persists_labels() {
    let lc = Lc::new().await;
    let agent = lc.create_titled("Label test").await;
    lc.manager
        .set_labels(&agent.id, labels(&[("surface", "mobile")]))
        .await
        .unwrap();
    lc.manager
        .set_labels(&agent.id, labels(&[("phase", "1a")]))
        .await
        .unwrap();
    let persisted = lc.record(&agent.id).await.unwrap();
    assert_eq!(
        persisted.labels,
        labels(&[("surface", "mobile"), ("phase", "1a")])
    );
}

#[tokio::test]
async fn detach_agent_removes_relationship_lifecycle_labels_from_a_live_agent_and_emits_state() {
    let lc = Lc::new().await;
    let parent = lc.create_titled("Parent").await;
    let child = create_titled(
        &lc,
        "Child",
        child_options(
            &parent.id,
            &[
                (DESKTOP_TAB, "true"),
                (MOBILE_TAB, "false"),
                ("team", "infra"),
            ],
            None,
        ),
    )
    .await;
    lc.events.clear();

    let result = lc.manager.detach_agent(&child.id).await.unwrap();
    lc.manager.flush().await;

    assert_eq!(
        result.previous_parent_agent_id.as_deref(),
        Some(parent.id.as_str())
    );
    assert!(result.live);
    assert_eq!(result.record.labels, labels(&[("team", "infra")]));
    assert_eq!(
        lc.manager.get_agent(&child.id).unwrap().labels,
        labels(&[("team", "infra")])
    );
    assert_eq!(
        lc.record(&child.id).await.unwrap().labels,
        labels(&[("team", "infra")])
    );
    assert!(lc
        .events
        .states(&child.id)
        .iter()
        .any(|s| s.labels == labels(&[("team", "infra")])));
}

#[tokio::test]
async fn detach_agent_removes_relationship_lifecycle_labels_from_a_stored_only_agent() {
    let lc = Lc::new().await;
    let parent = lc.create_titled("Parent").await;
    let child = create_titled(
        &lc,
        "Stored child",
        child_options(
            &parent.id,
            &[
                (DESKTOP_TAB, "true"),
                (MOBILE_TAB, "false"),
                ("role", "reviewer"),
            ],
            None,
        ),
    )
    .await;
    lc.manager.close_agent(&child.id).await.unwrap();

    let result = lc.manager.detach_agent(&child.id).await.unwrap();
    assert_eq!(
        result.previous_parent_agent_id.as_deref(),
        Some(parent.id.as_str())
    );
    assert!(!result.live);
    assert_eq!(result.record.labels, labels(&[("role", "reviewer")]));
    assert_eq!(
        lc.record(&child.id).await.unwrap().labels,
        labels(&[("role", "reviewer")])
    );
}

#[tokio::test]
async fn detach_of_an_agent_without_a_parent_changes_nothing() {
    let lc = Lc::new().await;
    let agent = lc
        .create_with(
            lc.config(),
            CreateAgentOptions {
                labels: Some(labels(&[("role", "solo")])),
                ..CreateAgentOptions::default()
            },
        )
        .await;
    let live = lc.manager.detach_agent(&agent.id).await.unwrap();
    assert!(live.live);
    assert_eq!(live.previous_parent_agent_id, None);
    lc.manager.close_agent(&agent.id).await.unwrap();
    let stored = lc.manager.detach_agent(&agent.id).await.unwrap();
    assert!(!stored.live);
    assert_eq!(stored.previous_parent_agent_id, None);
    let missing = lc.manager.detach_agent(&id_n(99)).await.unwrap_err();
    assert_eq!(
        missing.to_string(),
        format!("Agent not found: {}", id_n(99))
    );
}

#[tokio::test]
async fn archive_agent_does_not_cascade_to_a_detached_former_child() {
    let lc = Lc::new().await;
    let parent = lc.create_titled("Parent").await;
    let child = create_titled(&lc, "Child", child_options(&parent.id, &[], None)).await;

    lc.manager.detach_agent(&child.id).await.unwrap();
    lc.manager.archive_agent(&parent.id).await.unwrap();

    assert!(is_archived(&lc.record(&parent.id).await));
    assert!(!is_archived(&lc.record(&child.id).await));
}

#[tokio::test]
async fn set_title_bumps_updated_at_and_persists_title_in_the_same_snapshot_write() {
    let lc = Lc::new().await;
    let agent = lc.create().await;
    let before = lc.record(&agent.id).await.unwrap();

    lc.manager
        .set_title(&agent.id, "Generated title")
        .await
        .unwrap();

    let after = lc.record(&agent.id).await.unwrap();
    assert_eq!(
        after.title.value().map(String::as_str),
        Some("Generated title")
    );
    let ms = |s: &str| paseo_store::time::parse_js_date(s).unwrap();
    assert!(ms(&after.updated_at) > ms(&before.updated_at));
    let live = lc.manager.get_agent(&agent.id).unwrap();
    assert!(live.updated_at_ms > ms(&before.updated_at));
}

#[tokio::test]
async fn set_title_ignores_blank_titles() {
    let lc = Lc::new().await;
    let agent = lc.create_titled("Keep").await;
    lc.manager.set_title(&agent.id, "   ").await.unwrap();
    assert_eq!(
        lc.record(&agent.id)
            .await
            .unwrap()
            .title
            .value()
            .map(String::as_str),
        Some("Keep")
    );
}

#[tokio::test]
async fn update_agent_metadata_bumps_updated_at_for_stored_agents() {
    let lc = Lc::new().await;
    let snapshot = lc.create().await;
    lc.manager.close_agent(&snapshot.id).await.unwrap();

    let mut before = lc.record(&snapshot.id).await.unwrap();
    before.labels = labels(&[("surface", "mobile")]);
    lc.store.upsert(before.clone()).await.unwrap();
    assert!(lc.manager.get_agent(&snapshot.id).is_none());

    lc.manager
        .update_agent_metadata(
            &snapshot.id,
            MetadataUpdates {
                title: Some("Stored title".into()),
                labels: Some(labels(&[("role", "worker")])),
            },
        )
        .await
        .unwrap();

    let after = lc.record(&snapshot.id).await.unwrap();
    assert_eq!(
        after.title.value().map(String::as_str),
        Some("Stored title")
    );
    assert_eq!(
        after.labels,
        labels(&[("surface", "mobile"), ("role", "worker")])
    );
    let ms = |s: &str| paseo_store::time::parse_js_date(s).unwrap();
    assert!(ms(&after.updated_at) > ms(&before.updated_at));
}

#[tokio::test]
async fn run_agent_persists_finished_attention_and_idle_status_without_an_external_subscriber() {
    let lc = Lc::new().await;
    let agent = lc.create_titled("Finished attention test").await;

    lc.manager
        .run_agent(
            &agent.id,
            PromptInput::from("say hello"),
            Default::default(),
        )
        .await
        .unwrap();
    lc.flush().await;

    let persisted = lc.record(&agent.id).await.unwrap();
    assert_eq!(persisted.last_status, AgentLifecycle::Idle);
    assert_eq!(persisted.requires_attention, Some(true));
    assert_eq!(
        persisted.attention_reason,
        Nullable::Value(paseo_store::agent_record::AttentionReason::Finished)
    );
    assert!(persisted.attention_timestamp.value().is_some());
}

// ---- archive snapshot ----

#[tokio::test]
async fn archive_snapshot_clears_persisted_attention_and_normalizes_running_status() {
    let lc = Lc::new().await;
    let agent = lc.create_titled("Archive attention test").await;

    // TS mutates a COPY (`getAgent` returns `{...agent}`), so the live agent stays idle here.
    let archived_at = "2025-01-03T00:00:00.000Z";
    let archived = lc
        .manager
        .archive_snapshot(&agent.id, archived_at)
        .await
        .unwrap();

    assert_eq!(
        archived.archived_at.value().map(String::as_str),
        Some(archived_at)
    );
    assert_eq!(archived.last_status, AgentLifecycle::Closed);
    assert_eq!(archived.requires_attention, Some(false));
    assert_eq!(archived.attention_reason, Nullable::Null);
    assert_eq!(archived.attention_timestamp, Nullable::Null);
    let persisted = lc.record(&agent.id).await.unwrap();
    assert_eq!(
        persisted.archived_at.value().map(String::as_str),
        Some(archived_at)
    );
    assert_eq!(persisted.last_status, AgentLifecycle::Closed);
    assert_eq!(persisted.requires_attention, Some(false));
}

/// PARITY-QUIRK(archive-closed-snapshot-overwrite): the closed snapshot written while closing the
/// runtime overwrites `lastStatus` and the attention of the archived record with the live
/// agent's, preserving only `archivedAt`.
#[tokio::test]
async fn archive_closed_snapshot_overwrites_attention_of_the_archived_record() {
    let lc = Lc::new().await;
    let agent = lc.create_titled("Quirk").await;
    lc.manager
        .run_agent(&agent.id, PromptInput::from("work"), Default::default())
        .await
        .unwrap();
    assert!(lc
        .manager
        .get_agent(&agent.id)
        .unwrap()
        .attention
        .requires_attention());

    lc.manager.archive_agent(&agent.id).await.unwrap();
    lc.flush().await;

    let record = lc.record(&agent.id).await.unwrap();
    assert!(record.is_archived());
    assert_eq!(record.last_status, AgentLifecycle::Closed);
    assert_eq!(
        record.requires_attention,
        Some(true),
        "attention comes back from the live agent"
    );
    assert_eq!(
        record.attention_reason,
        Nullable::Value(paseo_store::agent_record::AttentionReason::Finished)
    );
}

#[tokio::test]
async fn archive_snapshot_dispatches_archived_state_for_stored_only_agents() {
    let lc = Lc::new().await;
    let created = lc.create_titled("Stored archive dispatch").await;
    lc.manager.close_agent(&created.id).await.unwrap();
    lc.events.clear();

    lc.manager
        .archive_snapshot(
            &created.id,
            &paseo_core::time::iso8601_from_millis(1_700_000_000_000),
        )
        .await
        .unwrap();

    let states = lc.events.states(&created.id);
    assert!(!states.is_empty());
    let last = states.last().unwrap();
    assert_eq!(last.id, created.id);
    assert_eq!(last.lifecycle, AgentLifecycle::Closed);
}

#[tokio::test]
async fn mark_agent_unread_dispatches_stored_attention_to_every_subscriber() {
    let lc = Lc::new().await;
    let created = lc
        .create_with(
            titled(&lc, "Stored unread dispatch"),
            CreateAgentOptions {
                workspace_id: Some("workspace-1".into()),
                ..CreateAgentOptions::default()
            },
        )
        .await;
    lc.manager.close_agent(&created.id).await.unwrap();
    let first = collect(
        &lc.manager,
        paseo_core::SubscribeOpts::default().without_replay(),
    )
    .await;
    let second = collect(
        &lc.manager,
        paseo_core::SubscribeOpts::default().without_replay(),
    )
    .await;

    lc.manager.mark_agent_unread(&created.id).await.unwrap();
    lc.manager.bus_barrier().await;
    settle().await;

    for log in [&first, &second] {
        let states = log.states(&created.id);
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].lifecycle, AgentLifecycle::Closed);
        assert_eq!(states[0].workspace_id.as_deref(), Some("workspace-1"));
        assert!(matches!(
            states[0].attention,
            paseo_core::AttentionState::Required {
                reason: AttentionReason::Finished,
                ..
            }
        ));
    }
    let record = lc.record(&created.id).await.unwrap();
    assert_eq!(record.requires_attention, Some(true));
    assert!(record.attention_timestamp.value().is_some());
}

#[tokio::test]
async fn mark_agent_unread_rejects_agents_that_are_not_finished_and_read() {
    let lc = Lc::new().await;
    let created = lc.create().await;
    lc.manager
        .run_agent(&created.id, PromptInput::from("work"), Default::default())
        .await
        .unwrap();
    let error = lc.manager.mark_agent_unread(&created.id).await.unwrap_err();
    assert_eq!(
        error.to_string(),
        format!("Agent is no longer finished and read: {}", created.id)
    );
    lc.manager.clear_agent_attention(&created.id).await.unwrap();
    lc.manager.mark_agent_unread(&created.id).await.unwrap();
}

// ---- archive agent ----

#[tokio::test]
async fn archive_agent_persists_archived_at_and_updated_at_before_emitting_closed_state() {
    let lc = Lc::new().await;
    let agent = lc.create_titled("Archive target").await;
    lc.events.clear();

    let archived_at = lc.manager.archive_agent(&agent.id).await.unwrap();
    lc.sync().await;
    let stored = lc.record(&agent.id).await.unwrap();

    assert_eq!(stored.archived_at.value(), Some(&archived_at));
    assert_eq!(stored.last_status, AgentLifecycle::Closed);
    assert_eq!(stored.requires_attention, Some(false));
    assert_eq!(stored.attention_reason, Nullable::Null);
    assert_eq!(stored.attention_timestamp, Nullable::Null);
    let ms = |s: &str| paseo_store::time::parse_js_date(s).unwrap();
    assert!((ms(&stored.updated_at) - ms(&archived_at)).abs() <= 5);
    let lifecycles: Vec<_> = lc
        .events
        .states(&agent.id)
        .iter()
        .map(|s| s.lifecycle)
        .collect();
    assert_eq!(
        lifecycles[lifecycles.len() - 2..],
        [AgentLifecycle::Idle, AgentLifecycle::Closed]
    );
}

#[tokio::test]
async fn archive_agent_of_an_unknown_agent_fails() {
    let lc = Lc::new().await;
    let error = lc.manager.archive_agent(&id_n(77)).await.unwrap_err();
    assert_eq!(error.to_string(), format!("Unknown agent '{}'", id_n(77)));
}

#[tokio::test]
async fn archive_agent_without_a_store_fails() {
    let lc = Lc::with(
        MockClient::new("codex"),
        LcOptions {
            store: false,
            ..LcOptions::default()
        },
    )
    .await;
    let agent = lc.create().await;
    let error = lc.manager.archive_agent(&agent.id).await.unwrap_err();
    assert_eq!(error.to_string(), "Agent storage is not configured");
}

#[tokio::test]
async fn fires_on_agent_archived_for_archived_parent_and_cascaded_children() {
    let lc = Lc::new().await;
    let parent = lc.create_titled("Parent").await;
    let child = create_titled(&lc, "Child", child_options(&parent.id, &[], None)).await;

    lc.manager.archive_agent(&parent.id).await.unwrap();

    let mut archived = lc.hooks.archived.lock().unwrap().clone();
    archived.sort();
    let mut expected = vec![child.id.clone(), parent.id.clone()];
    expected.sort();
    assert_eq!(archived, expected);
}

#[tokio::test]
async fn fires_on_agent_archived_for_stored_only_snapshot_archives() {
    let lc = Lc::new().await;
    let stored_only = lc.create_titled("Stored only").await;
    lc.manager.close_agent(&stored_only.id).await.unwrap();

    lc.manager
        .archive_snapshot(
            &stored_only.id,
            &paseo_core::time::iso8601_from_millis(1_700_000_000_000),
        )
        .await
        .unwrap();
    assert_eq!(
        *lc.hooks.archived.lock().unwrap(),
        vec![stored_only.id.clone()]
    );
}

#[tokio::test]
async fn archive_emits_the_agent_archived_plugin_event_once() {
    let lc = Lc::new().await;
    let agent = lc.create_titled("Plugin event").await;
    lc.manager.archive_agent(&agent.id).await.unwrap();
    let archived: Vec<_> = lc
        .hooks
        .plugin_emits
        .lock()
        .unwrap()
        .iter()
        .filter(|(hook, _)| hook == "agent.archived")
        .cloned()
        .collect();
    assert_eq!(archived.len(), 1);
    assert_eq!(archived[0].1["agent"]["id"], json!(agent.id));
    assert_eq!(archived[0].1["agent"]["title"], json!("Plugin event"));
    assert!(archived[0].1["archivedAt"].is_string());
}

// ---- native archive and restore ----

/// `WriterClient`: native archive and restore need the previous writer closed.
fn writer_client() -> (MockClient, Arc<Mutex<Option<MockSession>>>) {
    let client = MockClient::new("codex");
    let current: Arc<Mutex<Option<MockSession>>> = Arc::default();
    {
        let current = current.clone();
        client.on_create(move |req| {
            let current = current.clone();
            boxed(async move {
                let session = MockSession::builder("codex")
                    .config(&req.config)
                    .auto_completing()
                    .build();
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
                let session = MockSession::builder("codex").auto_completing().build();
                session.set_persistence(req.handle.clone());
                *current.lock().unwrap() = Some(session.clone());
                Ok(session)
            })
        });
    }
    for archive in [true, false] {
        let current = current.clone();
        let guard = move || {
            let current = current.clone();
            boxed(async move {
                if current
                    .lock()
                    .unwrap()
                    .as_ref()
                    .is_some_and(|s| !s.is_closed())
                {
                    return Err(ProviderError::provider(
                        "native thread has an active writer",
                    ));
                }
                Ok(())
            })
        };
        if archive {
            client.on_archive(guard);
        } else {
            client.on_unarchive(guard);
        }
    }
    (client, current)
}

#[tokio::test]
async fn native_archive_and_restore_release_the_loaded_session_writer() {
    let (client, current) = writer_client();
    let lc = Lc::with_client(client).await;
    let agent = lc.create().await;
    lc.manager.archive_agent(&agent.id).await.unwrap();
    assert_eq!(lc.client.archived_handles().len(), 1);

    // Opening archived history can retain a writer, including records archived by older daemons.
    lc.load(&agent.id).await.unwrap();
    assert!(!current.lock().unwrap().as_ref().unwrap().is_closed());
    let history = lc.client.resumes();
    assert_eq!(
        history
            .last()
            .unwrap()
            .options
            .as_ref()
            .and_then(|o| o.purpose.as_deref()),
        Some("history")
    );
    assert!(lc
        .manager
        .unarchive_snapshot(&agent.id, UnarchiveUpdates::default())
        .await
        .unwrap());
    assert_eq!(lc.client.unarchived_handles().len(), 1);
    assert!(!lc.record(&agent.id).await.unwrap().is_archived());
    assert!(current.lock().unwrap().as_ref().unwrap().is_closed());
    lc.load(&agent.id).await.unwrap();
    assert!(!current.lock().unwrap().as_ref().unwrap().is_closed());
}

#[tokio::test]
async fn unarchive_snapshot_skips_native_provider_unarchive_for_active_records() {
    let lc = Lc::new().await;
    let agent = lc.create_titled("Active unarchive target").await;
    let unarchived = lc
        .manager
        .unarchive_snapshot(&agent.id, UnarchiveUpdates::default())
        .await
        .unwrap();
    assert!(!unarchived);
    assert!(lc.client.unarchived_handles().is_empty());
}

#[tokio::test]
async fn unarchive_snapshot_unarchives_native_provider_storage_before_clearing_archived_at() {
    let client = MockClient::new("codex");
    let store_slot: Arc<Mutex<Option<Arc<paseo_store::AgentStore>>>> = Arc::default();
    let agent_slot: Arc<Mutex<Option<String>>> = Arc::default();
    let seen_archived_at: Arc<Mutex<Option<Option<String>>>> = Arc::default();
    {
        let (store_slot, agent_slot, seen) = (
            store_slot.clone(),
            agent_slot.clone(),
            seen_archived_at.clone(),
        );
        client.on_unarchive(move || {
            let (store_slot, agent_slot, seen) =
                (store_slot.clone(), agent_slot.clone(), seen.clone());
            boxed(async move {
                let store = store_slot.lock().unwrap().clone().unwrap();
                let id = agent_slot.lock().unwrap().clone().unwrap();
                let record = store.get(&id).await.unwrap().unwrap();
                *seen.lock().unwrap() = Some(record.archived_at.value().cloned());
                Ok(())
            })
        });
    }
    let lc = Lc::with_client(client).await;
    *store_slot.lock().unwrap() = Some(lc.store.clone());
    let agent = lc
        .create_with(
            titled(&lc, "Native unarchive target"),
            CreateAgentOptions {
                labels: Some(labels(&[
                    (PARENT_AGENT_ID_LABEL, "archived-parent"),
                    ("retained", "yes"),
                ])),
                ..CreateAgentOptions::default()
            },
        )
        .await;
    *agent_slot.lock().unwrap() = Some(agent.id.clone());
    lc.manager.archive_agent(&agent.id).await.unwrap();

    let mut patch = IndexMap::new();
    patch.insert(PARENT_AGENT_ID_LABEL.to_owned(), None);
    patch.insert("source".to_owned(), Some("reimport".to_owned()));
    let unarchived = lc
        .manager
        .unarchive_snapshot(
            &agent.id,
            UnarchiveUpdates {
                workspace_id: Some("ws-restored".into()),
                labels: Some(patch),
            },
        )
        .await
        .unwrap();
    let stored = lc.record(&agent.id).await.unwrap();

    assert!(unarchived);
    assert_eq!(lc.client.archived_handles().len(), 1);
    assert_eq!(lc.client.unarchived_handles(), lc.client.archived_handles());
    assert!(seen_archived_at.lock().unwrap().clone().unwrap().is_some());
    assert_eq!(stored.archived_at, Nullable::Null);
    assert_eq!(stored.workspace_id.as_deref(), Some("ws-restored"));
    assert_eq!(
        stored.labels,
        labels(&[("retained", "yes"), ("source", "reimport")])
    );
}

#[tokio::test]
async fn unarchive_snapshot_by_handle_unarchives_native_provider_storage_for_the_matched_snapshot()
{
    let lc = Lc::new().await;
    let agent = lc.create_titled("Native unarchive by handle target").await;
    lc.manager.archive_agent(&agent.id).await.unwrap();
    let archived = lc.record(&agent.id).await.unwrap();
    let stored_handle = archived.persistence.value().expect("persistence");
    let handle = paseo_agent_types::PersistenceHandle {
        provider: stored_handle.provider.clone(),
        session_id: stored_handle.session_id.clone(),
        native_handle: None,
        metadata: None,
    };

    lc.manager
        .unarchive_snapshot_by_handle(&handle)
        .await
        .unwrap();

    assert_eq!(lc.client.unarchived_handles(), lc.client.archived_handles());
    assert_eq!(
        lc.record(&agent.id).await.unwrap().archived_at,
        Nullable::Null
    );
}

#[tokio::test]
async fn unarchive_snapshot_keeps_the_stored_record_archived_when_native_unarchive_fails() {
    let lc = Lc::new().await;
    let agent = lc.create_titled("Native unarchive failure target").await;
    lc.manager.archive_agent(&agent.id).await.unwrap();
    lc.client.fail_unarchive("provider still archived");

    let error = lc
        .manager
        .unarchive_snapshot(&agent.id, UnarchiveUpdates::default())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("provider still archived"));

    assert!(lc.record(&agent.id).await.unwrap().is_archived());
    assert_eq!(lc.client.unarchived_handles().len(), 1);
}

// ---- cascade ----

#[tokio::test]
async fn archive_agent_cascade_archives_in_memory_children_with_the_full_archive_contract() {
    let lc = Lc::new().await;
    let parent = create_titled(&lc, "Parent", CreateAgentOptions::default()).await;
    let child = create_titled(&lc, "Child", child_options(&parent.id, &[], None)).await;
    let unrelated = create_titled(&lc, "Unrelated", CreateAgentOptions::default()).await;

    lc.manager.archive_agent(&parent.id).await.unwrap();

    assert_archived_record(lc.record(&parent.id).await, AgentLifecycle::Closed);
    assert_archived_record(lc.record(&child.id).await, AgentLifecycle::Closed);
    assert!(!is_archived(&lc.record(&unrelated.id).await));
    assert!(lc.manager.get_agent(&child.id).is_none());
}

#[tokio::test]
async fn archive_agent_detaches_an_open_same_workspace_child_instead_of_cascading() {
    let lc = Lc::new().await;
    let parent = lc
        .create_with(
            titled(&lc, "Parent"),
            CreateAgentOptions {
                workspace_id: Some("workspace-a".into()),
                ..CreateAgentOptions::default()
            },
        )
        .await;
    let child = create_titled(
        &lc,
        "Open child",
        child_options(
            &parent.id,
            &[(DESKTOP_TAB, "false"), (MOBILE_TAB, "true")],
            Some("workspace-a"),
        ),
    )
    .await;

    lc.manager.archive_agent(&parent.id).await.unwrap();

    let stored = lc.record(&child.id).await.unwrap();
    assert!(!stored.is_archived());
    for label in [PARENT_AGENT_ID_LABEL, DESKTOP_TAB, MOBILE_TAB] {
        assert!(!stored.labels.contains_key(label), "{label} removed");
    }
    assert_eq!(lc.manager.get_agent(&child.id).unwrap().id, child.id);
}

#[tokio::test]
async fn archive_agent_detaches_a_cross_workspace_child_even_when_its_tab_is_closed() {
    let lc = Lc::new().await;
    let parent = lc
        .create_with(
            titled(&lc, "Parent"),
            CreateAgentOptions {
                workspace_id: Some("workspace-a".into()),
                ..CreateAgentOptions::default()
            },
        )
        .await;
    let child = create_titled(
        &lc,
        "Cross-workspace child",
        child_options(&parent.id, &[(DESKTOP_TAB, "false")], Some("workspace-b")),
    )
    .await;

    lc.manager.archive_agent(&parent.id).await.unwrap();

    let stored = lc.record(&child.id).await.unwrap();
    assert!(!stored.is_archived());
    assert_eq!(stored.workspace_id.as_deref(), Some("workspace-b"));
    assert!(!stored.labels.contains_key(PARENT_AGENT_ID_LABEL));
}

/// The cascade scan sees a child before its tab opens; the re-read inside the child's lane sees
/// the open tab. A held close keeps the child's lane busy between the two.
#[tokio::test]
async fn archive_agent_re_reads_a_child_before_deciding_whether_to_cascade() {
    let client = MockClient::new("codex");
    let close_started = Gate::new();
    let close_allowed = Gate::new();
    let created = Arc::new(std::sync::atomic::AtomicU32::new(0));
    {
        let (started, allowed, created) = (
            close_started.clone(),
            close_allowed.clone(),
            created.clone(),
        );
        client.on_create(move |req| {
            let (started, allowed) = (started.clone(), allowed.clone());
            let nth = created.fetch_add(1, Ordering::SeqCst);
            boxed(async move {
                // Only the child's session (the second one) holds its close.
                Ok(if nth == 1 {
                    held_close_session(&started, &allowed, &req.config)
                } else {
                    MockSession::builder("codex")
                        .config(&req.config)
                        .auto_completing()
                        .build()
                })
            })
        });
    }
    let lc = Lc::with_client(client).await;
    let parent = lc
        .create_with(
            titled(&lc, "Parent"),
            CreateAgentOptions {
                workspace_id: Some("workspace-a".into()),
                ..CreateAgentOptions::default()
            },
        )
        .await;
    let child = create_titled(
        &lc,
        "Late-open child",
        child_options(&parent.id, &[(MOBILE_TAB, "false")], Some("workspace-a")),
    )
    .await;

    let closing = {
        let (manager, id) = (lc.manager.clone(), child.id.clone());
        tokio::spawn(async move { manager.close_agent(&id).await })
    };
    within(close_started.wait()).await;
    let archive = {
        let (manager, id) = (lc.manager.clone(), parent.id.clone());
        tokio::spawn(async move { manager.archive_agent(&id).await })
    };
    // The cascade is now waiting for the child's lane.
    within(async {
        while !is_archived(&lc.record(&parent.id).await) {
            settle().await;
        }
    })
    .await;
    // The tab opens while the close still holds the lane; the closed snapshot carries the label.
    lc.manager
        .testing_mutate_agent(&child.id, |view| {
            view.labels.insert(MOBILE_TAB.to_owned(), "true".to_owned());
        })
        .await
        .unwrap();
    close_allowed.open();
    closing.await.unwrap().unwrap();
    archive.await.unwrap().unwrap();

    let stored = lc.record(&child.id).await.unwrap();
    assert!(!stored.is_archived());
    assert!(!stored.labels.contains_key(PARENT_AGENT_ID_LABEL));
}

/// Persister that blocks the snapshot write of one agent while its open-tab label is `true`.
struct BlockingOpenMarkerPersister {
    inner: paseo_core::StorePersister,
    child_id: Mutex<Option<String>>,
    started: Gate,
    release: Gate,
}

#[async_trait]
impl SnapshotPersister for BlockingOpenMarkerPersister {
    async fn persist(
        &self,
        agent: &ManagedAgentView,
        options: PersistOptions,
    ) -> Result<(), String> {
        let is_child = self.child_id.lock().unwrap().as_deref() == Some(agent.id.as_str());
        if is_child && agent.labels.get(MOBILE_TAB).map(String::as_str) == Some("true") {
            self.started.open();
            self.release.wait().await;
        }
        self.inner.persist(agent, options).await
    }

    async fn stored_title(&self, agent_id: &str) -> Result<Option<Option<String>>, String> {
        self.inner.stored_title(agent_id).await
    }
}

#[tokio::test]
async fn archive_agent_cannot_overtake_a_received_child_open_tab_update() {
    let blocker: Arc<Mutex<Option<Arc<BlockingOpenMarkerPersister>>>> = Arc::default();
    let started = Gate::new();
    let release = Gate::new();
    let lc = {
        let (blocker, started, release) = (blocker.clone(), started.clone(), release.clone());
        Lc::with(
            MockClient::new("codex"),
            LcOptions {
                tweak: Box::new(move |options: &mut paseo_core::ManagerOptions| {
                    let store = options.store.clone().unwrap();
                    let persister = Arc::new(BlockingOpenMarkerPersister {
                        inner: paseo_core::StorePersister::new(store),
                        child_id: Mutex::new(None),
                        started,
                        release,
                    });
                    *blocker.lock().unwrap() = Some(persister.clone());
                    options.persister = Some(persister);
                }),
                ..LcOptions::default()
            },
        )
        .await
    };
    let parent = lc
        .create_with(
            titled(&lc, "Parent"),
            CreateAgentOptions {
                workspace_id: Some("workspace-a".into()),
                ..CreateAgentOptions::default()
            },
        )
        .await;
    let child = create_titled(
        &lc,
        "Opening child",
        child_options(&parent.id, &[], Some("workspace-a")),
    )
    .await;
    *blocker
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .child_id
        .lock()
        .unwrap() = Some(child.id.clone());

    let mark_open = {
        let (manager, id) = (lc.manager.clone(), child.id.clone());
        tokio::spawn(async move {
            manager
                .update_agent_metadata(
                    &id,
                    MetadataUpdates {
                        title: None,
                        labels: Some(labels(&[(MOBILE_TAB, "true")])),
                    },
                )
                .await
        })
    };
    within(started.wait()).await;
    let archive_parent = {
        let (manager, id) = (lc.manager.clone(), parent.id.clone());
        tokio::spawn(async move { manager.archive_agent(&id).await })
    };
    release.open();
    mark_open.await.unwrap().unwrap();
    archive_parent.await.unwrap().unwrap();

    let stored = lc.record(&child.id).await.unwrap();
    assert!(!stored.is_archived());
    assert!(!stored.labels.contains_key(PARENT_AGENT_ID_LABEL));
}

#[tokio::test]
async fn archive_agent_cascade_closes_a_running_child_runtime() {
    let client = MockClient::new("codex");
    let finish = Gate::new();
    let created = Arc::new(std::sync::atomic::AtomicU32::new(0));
    {
        let (finish, created) = (finish.clone(), created.clone());
        client.on_create(move |req| {
            let (finish, nth) = (finish.clone(), created.fetch_add(1, Ordering::SeqCst));
            boxed(async move {
                use paseo_core::mock::ScriptedBehavior;
                Ok(MockSession::builder("codex")
                    .config(&req.config)
                    .behavior(
                        ScriptedBehavior::new().on_start(move |session, prompt, options| {
                            let finish = finish.clone();
                            boxed(async move {
                                let n = session.record_start(prompt, options);
                                let turn_id = format!("running-child-turn-{n}-{nth}");
                                let (soon, turn) = (session.clone(), turn_id.clone());
                                tokio::spawn(async move {
                                    soon.push_event(turn_started_for("codex", &turn));
                                    finish.wait().await;
                                    soon.push_event(turn_completed_for("codex", &turn));
                                });
                                Ok(paseo_provider_api::TurnHandle { turn_id })
                            })
                        }),
                    )
                    .build())
            })
        });
    }
    let lc = Lc::with_client(client).await;
    let parent = create_titled(&lc, "Parent", CreateAgentOptions::default()).await;
    let child = create_titled(&lc, "Running Child", child_options(&parent.id, &[], None)).await;
    let child_session = lc.client.created_sessions()[1].clone();
    lc.events.clear();
    let mut stream = lc
        .manager
        .stream_agent(
            &child.id,
            PromptInput::from("keep running"),
            Default::default(),
        )
        .await
        .unwrap();
    let drain = tokio::spawn(async move { while let Ok(Some(_)) = stream.next().await {} });
    lc.manager
        .wait_for_agent_run_start(&child.id, None)
        .await
        .unwrap();

    lc.manager.archive_agent(&parent.id).await.unwrap();
    finish.open();
    within(drain).await.unwrap();
    lc.sync().await;

    assert!(child_session.is_closed());
    assert!(lc.manager.get_agent(&child.id).is_none());
    assert!(lc
        .events
        .states(&child.id)
        .iter()
        .any(|s| s.lifecycle == AgentLifecycle::Closed));
}

fn turn_started_for(provider: &str, turn: &str) -> paseo_agent_types::AgentStreamEvent {
    provider_event_turn_started(provider, turn)
}

fn turn_completed_for(provider: &str, turn: &str) -> paseo_agent_types::AgentStreamEvent {
    paseo_agent_types::AgentStreamEvent::TurnCompleted {
        provider: provider.into(),
        usage: None,
        turn_id: Some(turn.into()),
    }
}

#[tokio::test]
async fn archive_agent_cascade_archives_off_memory_children_with_the_full_archive_contract() {
    let lc = Lc::new().await;
    let parent = create_titled(&lc, "Parent", CreateAgentOptions::default()).await;
    let child = create_titled(
        &lc,
        "Off-memory Child",
        child_options(&parent.id, &[], None),
    )
    .await;
    lc.manager.testing_forget_agent(&child.id);

    lc.manager.archive_agent(&parent.id).await.unwrap();

    assert_archived_record(lc.record(&child.id).await, AgentLifecycle::Idle);
}

#[tokio::test]
async fn archive_agent_cascade_notifies_subscribers_for_in_memory_and_off_memory_children() {
    let lc = Lc::new().await;
    let parent = create_titled(&lc, "Parent", CreateAgentOptions::default()).await;
    let in_memory =
        create_titled(&lc, "In-memory Child", child_options(&parent.id, &[], None)).await;
    let off_memory = create_titled(
        &lc,
        "Off-memory Child",
        child_options(&parent.id, &[], None),
    )
    .await;
    lc.manager.testing_forget_agent(&off_memory.id);
    lc.events.clear();

    lc.manager.archive_agent(&parent.id).await.unwrap();
    lc.sync().await;

    assert!(!lc.events.states(&in_memory.id).is_empty());
    assert!(!lc.events.states(&off_memory.id).is_empty());
}

#[tokio::test]
async fn archive_agent_cascade_surfaces_partial_child_archive_failures() {
    let lc = Lc::new().await;
    let parent = create_titled(&lc, "Parent", CreateAgentOptions::default()).await;
    let child = create_titled(&lc, "Failing Child", child_options(&parent.id, &[], None)).await;
    lc.flush().await;
    // A directory where the child's record file belongs makes its next write fail.
    let record_path = lc
        .dir
        .path()
        .join("agents")
        .join(paseo_store::project_dir_name_from_cwd(&lc.cwd()))
        .join(format!("{}.json", child.id));
    std::fs::remove_file(&record_path).unwrap();
    std::fs::create_dir(&record_path).unwrap();

    let error = lc.manager.archive_agent(&parent.id).await.unwrap_err();
    assert!(!error.to_string().is_empty());
    assert!(
        is_archived(&lc.record(&parent.id).await),
        "the parent was archived first"
    );
}

#[tokio::test]
async fn archiving_a_closed_parent_still_cascades_to_its_managed_children() {
    let lc = Lc::new().await;
    let parent = create_titled(&lc, "Closed parent", CreateAgentOptions::default()).await;
    let child = create_titled(&lc, "Managed child", child_options(&parent.id, &[], None)).await;

    lc.manager.close_agent(&parent.id).await.unwrap();
    lc.manager
        .archive_snapshot(
            &parent.id,
            &paseo_core::time::iso8601_from_millis(1_700_000_000_000),
        )
        .await
        .unwrap();

    assert!(is_archived(&lc.record(&parent.id).await));
    assert!(is_archived(&lc.record(&child.id).await));
    assert!(lc.manager.get_agent(&child.id).is_none());
}

// ---- internal agents ----

fn internal(lc: &Lc, title: &str) -> AgentSessionConfig {
    let mut config = titled(lc, title);
    config.internal = Some(true);
    config
}

#[tokio::test]
async fn archive_agent_closes_an_internal_agent_without_writing_a_record() {
    let lc = Lc::new().await;
    let agent = lc
        .create_with(
            internal(&lc, "Internal Agent"),
            CreateAgentOptions::default(),
        )
        .await;

    let archived_at = lc.manager.archive_agent(&agent.id).await.unwrap();

    assert!(paseo_store::time::parse_js_date(&archived_at).is_some());
    assert!(lc.manager.get_agent(&agent.id).is_none());
    assert!(lc.record(&agent.id).await.is_none());
    assert!(lc.store.list().await.unwrap().is_empty());
    let retired = lc
        .manager
        .get_retired_internal_agent(&agent.id)
        .expect("retired");
    assert_eq!(retired.record.id, agent.id);
    assert_eq!(
        retired.record.title.value().map(String::as_str),
        Some("Internal Agent")
    );
    assert_eq!(retired.record.internal, Some(true));
    assert_eq!(retired.record.archived_at.value(), Some(&archived_at));
    assert_eq!(retired.record.last_status, AgentLifecycle::Idle);
    assert_eq!(retired.last_message, None);
    assert!(lc.manager.get_retired_internal_agent(&id_n(999)).is_none());
}

#[tokio::test]
async fn retired_internal_agents_expire_after_ten_minutes() {
    let clock = Arc::new(paseo_core::time::ManualClock::new(1_700_000_000_000));
    let lc = {
        let clock = clock.clone();
        Lc::with(
            MockClient::new("codex"),
            LcOptions {
                tweak: Box::new(move |o: &mut paseo_core::ManagerOptions| o.clock = clock),
                ..LcOptions::default()
            },
        )
        .await
    };
    let agent = lc
        .create_with(internal(&lc, "Helper"), CreateAgentOptions::default())
        .await;
    lc.manager.archive_agent(&agent.id).await.unwrap();
    assert!(lc.manager.get_retired_internal_agent(&agent.id).is_some());
    assert_eq!(lc.manager.list_retired_internal_agents().len(), 1);
    clock.advance(10 * 60 * 1000);
    assert!(lc.manager.get_retired_internal_agent(&agent.id).is_none());
    assert!(lc.manager.list_retired_internal_agents().is_empty());
}

#[tokio::test]
async fn archive_agent_on_an_internal_parent_still_archives_its_attached_children() {
    let lc = Lc::new().await;
    let parent = lc
        .create_with(
            internal(&lc, "Internal Parent"),
            CreateAgentOptions::default(),
        )
        .await;
    let child = create_titled(&lc, "Child", child_options(&parent.id, &[], None)).await;

    lc.manager.archive_agent(&parent.id).await.unwrap();

    assert!(lc.manager.get_agent(&parent.id).is_none());
    assert!(lc.manager.get_agent(&child.id).is_none());
    assert!(is_archived(&lc.record(&child.id).await));
    assert!(lc.record(&parent.id).await.is_none());
}

#[tokio::test]
async fn archive_agent_on_a_parent_archives_its_live_internal_children() {
    let lc = Lc::new().await;
    let parent = lc.create_titled("Parent").await;
    // An internal child is never in storage, so only the live scan can reach it.
    let mut config = internal(&lc, "Internal Child");
    config.internal = Some(true);
    let child = lc
        .create_with(config, child_options(&parent.id, &[], None))
        .await;
    assert!(lc.manager.get_agent(&child.id).is_some());

    lc.manager.archive_agent(&parent.id).await.unwrap();

    assert!(lc.manager.get_agent(&parent.id).is_none());
    assert!(lc.manager.get_agent(&child.id).is_none());
    assert!(lc
        .manager
        .get_retired_internal_agent(&child.id)
        .unwrap()
        .record
        .archived_at
        .value()
        .is_some());
    assert!(lc.record(&child.id).await.is_none());
}

#[tokio::test]
async fn internal_agent_archive_events_reach_only_opted_in_global_subscribers() {
    let lc = Lc::new().await;
    let agent = lc
        .create_with(
            internal(&lc, "Internal Agent"),
            CreateAgentOptions::default(),
        )
        .await;
    let hidden = collect(&lc.manager, paseo_core::SubscribeOpts::default()).await;
    let opted_in = collect(
        &lc.manager,
        paseo_core::SubscribeOpts::default().including_internal(),
    )
    .await;
    lc.manager.bus_barrier().await;
    settle().await;
    assert!(hidden.states(&agent.id).is_empty());
    assert_eq!(opted_in.states(&agent.id).len(), 1);

    lc.manager.archive_agent(&agent.id).await.unwrap();
    lc.manager.bus_barrier().await;
    settle().await;
    assert!(hidden.states(&agent.id).is_empty());
    assert!(opted_in.states(&agent.id).len() > 1);
}

#[tokio::test]
async fn workspace_ownership_does_not_turn_an_ordinary_agent_into_a_disposable_helper() {
    let (mut hooks, log) = RecordingHooks::new();
    hooks.plugin_lifecycle = true;
    let lc = Lc::with(
        MockClient::new("codex"),
        LcOptions {
            hooks: Some(hooks),
            ..LcOptions::default()
        },
    )
    .await;
    let agent = lc
        .create_with(
            titled(&lc, "Helper"),
            CreateAgentOptions {
                workspace_id: Some("wks_internal".into()),
                ..CreateAgentOptions::default()
            },
        )
        .await;

    assert!(!lc.manager.get_agent(&agent.id).unwrap().internal);
    assert_eq!(
        lc.manager
            .list_agents(false)
            .iter()
            .map(|a| a.id.clone())
            .collect::<Vec<_>>(),
        vec![agent.id.clone()]
    );
    assert!(lc.record(&agent.id).await.is_some());
    // Workspace ownership preserves ordinary agent lifecycle hooks.
    let names: Vec<_> = log
        .before_hooks
        .lock()
        .unwrap()
        .iter()
        .map(|(n, _)| n.clone())
        .collect();
    assert!(names.contains(&"agent.create".to_owned()), "{names:?}");
    assert!(
        names.contains(&"agent.session_open".to_owned()),
        "{names:?}"
    );
}

#[tokio::test]
async fn plugin_before_hooks_are_skipped_for_internal_agents_and_failures_abort_creation() {
    struct RejectingHooks;
    #[async_trait]
    impl paseo_core::ManagerHooks for RejectingHooks {
        fn has_plugin_lifecycle(&self) -> bool {
            true
        }
        async fn plugin_before_create(
            &self,
            _input: serde_json::Value,
        ) -> Result<serde_json::Value, String> {
            Err("Plugin p before agent.create failed: no".into())
        }
    }
    let lc = Lc::with(
        MockClient::new("codex"),
        LcOptions {
            tweak: Box::new(|o: &mut paseo_core::ManagerOptions| {
                o.hooks = Arc::new(RejectingHooks)
            }),
            ..LcOptions::default()
        },
    )
    .await;
    // Internal agents never run the create hook.
    let helper = lc
        .create_with(internal(&lc, "Helper"), CreateAgentOptions::default())
        .await;
    assert!(helper.internal);
    let error = lc
        .manager
        .create_agent(titled(&lc, "Visible"), None, CreateAgentOptions::default())
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "Plugin p before agent.create failed: no");
    assert_eq!(lc.manager.list_agents(true).len(), 1);
}

// ---- stored agent loading ----

#[tokio::test]
async fn ensure_unarchived_agent_loaded_does_not_resume_an_archived_agent() {
    let lc = Lc::new().await;
    let agent = lc.create().await;
    lc.manager.close_agent(&agent.id).await.unwrap();
    lc.manager
        .archive_snapshot(
            &agent.id,
            &paseo_core::time::iso8601_from_millis(1_700_000_000_000),
        )
        .await
        .unwrap();

    let error = lc
        .manager
        .ensure_unarchived_agent_loaded(&agent.id, LoadOptions::default())
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        format!("Agent is archived: {}", agent.id)
    );
    assert!(lc.manager.get_agent(&agent.id).is_none());
}

/// A resume that waits on a gate (TS `resumeStarted` / `resumeAllowed` deferreds).
fn held_resume_client() -> (MockClient, Gate, Gate) {
    let client = MockClient::new("codex");
    let started = Gate::new();
    let allowed = Gate::new();
    {
        let (started, allowed) = (started.clone(), allowed.clone());
        client.on_resume(move |req| {
            let (started, allowed) = (started.clone(), allowed.clone());
            boxed(async move {
                started.open();
                allowed.wait().await;
                let session = MockSession::builder("codex").auto_completing().build();
                session.set_persistence(req.handle.clone());
                Ok(session)
            })
        });
    }
    (client, started, allowed)
}

#[tokio::test]
async fn a_stored_only_archive_waits_for_an_in_flight_resume_and_then_closes_it() {
    let (client, resume_started, resume_allowed) = held_resume_client();
    let lc = Lc::with_client(client).await;
    let agent = lc.create().await;
    lc.manager.close_agent(&agent.id).await.unwrap();

    let load = {
        let (manager, id) = (lc.manager.clone(), agent.id.clone());
        tokio::spawn(async move {
            manager
                .ensure_agent_loaded(&id, LoadOptions::default())
                .await
        })
    };
    within(resume_started.wait()).await;
    let archive = {
        let (manager, id) = (lc.manager.clone(), agent.id.clone());
        tokio::spawn(async move {
            manager
                .archive_snapshot(
                    &id,
                    &paseo_core::time::iso8601_from_millis(1_700_000_000_000),
                )
                .await
        })
    };
    resume_allowed.open();
    archive.await.unwrap().unwrap();

    assert_eq!(load.await.unwrap().unwrap().id, agent.id);
    assert!(lc.manager.get_agent(&agent.id).is_none());
    assert!(is_archived(&lc.record(&agent.id).await));
}

#[tokio::test]
async fn a_stored_only_archive_closes_a_shared_resume_before_a_later_protected_load_returns() {
    let (client, resume_started, resume_allowed) = held_resume_client();
    let lc = Lc::with_client(client).await;
    let agent = lc.create().await;
    lc.manager.close_agent(&agent.id).await.unwrap();

    let shared = {
        let (manager, id) = (lc.manager.clone(), agent.id.clone());
        tokio::spawn(async move {
            manager
                .ensure_agent_loaded(&id, LoadOptions::default())
                .await
        })
    };
    within(resume_started.wait()).await;
    let protected = {
        let (manager, id) = (lc.manager.clone(), agent.id.clone());
        tokio::spawn(async move {
            manager
                .ensure_unarchived_agent_loaded(&id, LoadOptions::default())
                .await
        })
    };
    let archive = {
        let (manager, id) = (lc.manager.clone(), agent.id.clone());
        tokio::spawn(async move {
            manager
                .archive_snapshot(
                    &id,
                    &paseo_core::time::iso8601_from_millis(1_700_000_000_000),
                )
                .await
        })
    };
    resume_allowed.open();
    archive.await.unwrap().unwrap();

    shared.await.unwrap().unwrap();
    let error = protected.await.unwrap().unwrap_err();
    assert_eq!(
        error.to_string(),
        format!("Agent is archived: {}", agent.id)
    );
    assert!(lc.manager.get_agent(&agent.id).is_none());
    assert!(is_archived(&lc.record(&agent.id).await));
}

#[tokio::test]
async fn a_shared_agent_load_upgrades_provider_history_hydration_to_broadcast() {
    use paseo_core::mock::ScriptedBehavior;
    let client = MockClient::new("codex");
    let history_started = Gate::new();
    let history_allowed = Gate::new();
    {
        let (started, allowed) = (history_started.clone(), history_allowed.clone());
        client.on_resume(move |req| {
            let (started, allowed) = (started.clone(), allowed.clone());
            boxed(async move {
                let session = MockSession::builder("codex")
                    .auto_completing()
                    .behavior(ScriptedBehavior::new().on_history(move |_| {
                        let (started, allowed) = (started.clone(), allowed.clone());
                        boxed(async move {
                            started.open();
                            allowed.wait().await;
                            Ok(vec![assistant("Recovered history", "codex", None)])
                        })
                    }))
                    .build();
                session.set_persistence(req.handle.clone());
                Ok(session)
            })
        });
    }
    let lc = Lc::with_client(client).await;
    let agent = lc.create().await;
    lc.manager.close_agent(&agent.id).await.unwrap();
    lc.manager.delete_agent_state(&agent.id);
    let events = collect(
        &lc.manager,
        paseo_core::SubscribeOpts::for_agent(agent.id.clone()).without_replay(),
    )
    .await;

    let quiet = {
        let (manager, id) = (lc.manager.clone(), agent.id.clone());
        tokio::spawn(async move {
            manager
                .ensure_agent_loaded(&id, LoadOptions::default())
                .await
        })
    };
    within(history_started.wait()).await;
    let broadcasting = {
        let (manager, id) = (lc.manager.clone(), agent.id.clone());
        tokio::spawn(async move {
            manager
                .ensure_agent_loaded(
                    &id,
                    LoadOptions {
                        broadcast_timeline: true,
                        ..LoadOptions::default()
                    },
                )
                .await
        })
    };
    history_allowed.open();
    quiet.await.unwrap().unwrap();
    broadcasting.await.unwrap().unwrap();
    lc.manager.bus_barrier().await;
    settle().await;

    assert!(events.timeline_streams(&agent.id).iter().any(|e| matches!(
        stream_event(e),
        paseo_agent_types::AgentStreamEvent::Timeline { item, .. }
            if **item == assistant_item("Recovered history")
    )));
}

#[tokio::test]
async fn ensure_agent_loaded_reports_missing_and_unavailable_records() {
    let lc = Lc::new().await;
    let missing = lc.load(&id_n(500)).await.unwrap_err();
    assert_eq!(
        missing.to_string(),
        format!("Agent not found: {}", id_n(500))
    );

    let agent = lc.create().await;
    lc.manager.close_agent(&agent.id).await.unwrap();
    let error = lc
        .manager
        .ensure_agent_loaded(
            &agent.id,
            LoadOptions {
                valid_providers: Some(vec!["claude".into()]),
                ..LoadOptions::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        format!("Agent {} references unavailable provider 'codex'", agent.id)
    );
}

#[tokio::test]
async fn ensure_agent_loaded_creates_a_session_for_a_record_without_a_provider_handle() {
    let lc = Lc::new().await;
    let mut record =
        StoredAgentRecord::new(&id_n(600), "codex", &lc.cwd(), "2026-01-01T00:00:00.000Z");
    record.labels = labels(&[("kept", "yes")]);
    record.workspace_id = Some("wks_x".into());
    lc.store.upsert(record).await.unwrap();

    let loaded = lc.load(&id_n(600)).await.unwrap();
    assert_eq!(loaded.labels, labels(&[("kept", "yes")]));
    assert_eq!(loaded.workspace_id.as_deref(), Some("wks_x"));
    assert_eq!(
        lc.client.creates().len(),
        1,
        "no handle to resume: a first session is created"
    );
    assert!(lc.client.resumes().is_empty());
    assert!(loaded.history_primed, "create primes history");
}

#[tokio::test]
async fn ensure_agent_loaded_resumes_with_the_stored_timestamps_and_attention() {
    let lc = Lc::new().await;
    let agent = lc.create_titled("Resume me").await;
    lc.manager
        .run_agent(&agent.id, PromptInput::from("work"), Default::default())
        .await
        .unwrap();
    lc.flush().await;
    let stored = lc.record(&agent.id).await.unwrap();
    lc.manager.close_agent(&agent.id).await.unwrap();

    let loaded = lc.load(&agent.id).await.unwrap();
    let ms = |s: &str| paseo_store::time::parse_js_date(s).unwrap();
    assert_eq!(loaded.created_at_ms, ms(&stored.created_at));
    assert!(
        loaded.attention.requires_attention(),
        "unread state survives a resume"
    );
    assert!(
        loaded.updated_at_ms >= ms(&stored.updated_at),
        "restoring is not activity"
    );
}

// ---- agent-loading.test.ts ----

#[tokio::test]
async fn loads_archived_records_for_history_and_active_records_with_the_interactive_default() {
    let lc = Lc::new().await;
    let archived = lc.create().await;
    lc.manager.archive_agent(&archived.id).await.unwrap();
    let active = lc.create().await;
    lc.manager.close_agent(&active.id).await.unwrap();

    lc.load(&archived.id).await.unwrap();
    lc.load(&active.id).await.unwrap();

    let purposes: Vec<_> = lc
        .client
        .resumes()
        .iter()
        .map(|r| r.options.as_ref().and_then(|o| o.purpose.clone()))
        .collect();
    assert_eq!(
        purposes,
        vec![Some("history".to_owned()), Some("interactive".to_owned())]
    );
}

#[tokio::test]
async fn resuming_a_stored_agent_keeps_its_unread_flag_and_its_last_activity_time() {
    let lc = Lc::new().await;
    let agent = lc.create().await;
    lc.manager.close_agent(&agent.id).await.unwrap();
    lc.flush().await;
    let last_active = "2026-01-02T03:04:05.000Z";
    let marked_unread = "2026-01-09T03:04:05.000Z";

    // The agent finished days ago and was marked unread later without being opened, which moves
    // `updatedAt` on its own. Clients already hold that newer time.
    let mut stored = lc.record(&agent.id).await.unwrap();
    stored.updated_at = marked_unread.into();
    stored.last_activity_at = Some(last_active.into());
    stored.requires_attention = Some(true);
    stored.attention_reason = Nullable::Value(paseo_store::agent_record::AttentionReason::Finished);
    stored.attention_timestamp = Nullable::Value(last_active.into());
    lc.store.upsert(stored).await.unwrap();

    lc.load(&agent.id).await.unwrap();
    lc.flush().await;

    // Loading the runtime is neither the agent working nor the user reading the chat.
    let resumed = lc.record(&agent.id).await.unwrap();
    assert_eq!(resumed.requires_attention, Some(true));
    assert_eq!(
        resumed.attention_reason,
        Nullable::Value(paseo_store::agent_record::AttentionReason::Finished)
    );
    assert_eq!(resumed.updated_at, marked_unread);
    assert_eq!(resumed.last_activity_at.as_deref(), Some(marked_unread));
    let live = lc.manager.get_agent(&agent.id).unwrap();
    assert!(live.attention.requires_attention());
    assert_eq!(
        live.updated_at_ms,
        paseo_store::time::parse_js_date(marked_unread).unwrap()
    );
}

#[tokio::test]
async fn loads_an_archived_agents_history_after_its_working_directory_is_removed() {
    let client = MockClient::new("codex");
    client.on_resume(|req| {
        boxed(async move {
            let session = MockSession::builder("codex")
                .behavior(paseo_core::mock::ScriptedBehavior::new().on_history(|_| {
                    boxed(async { Ok(vec![assistant("I changed the file", "codex", None)]) })
                }))
                .build();
            session.set_persistence(req.handle.clone());
            Ok(session)
        })
    });
    let lc = Lc::with_client(client).await;
    let worktree = lc.dir.path().join("managed-worktree");
    std::fs::create_dir_all(&worktree).unwrap();
    let agent = lc
        .create_with(
            AgentSessionConfig::new("codex", worktree.to_string_lossy()),
            CreateAgentOptions {
                workspace_id: Some("workspace-worktree".into()),
                ..CreateAgentOptions::default()
            },
        )
        .await;
    lc.manager
        .start_agent_run(&agent.id, "what did you change".into(), Default::default())
        .await
        .unwrap();
    let finished = lc
        .manager
        .wait_for_agent_event(&agent.id, Default::default())
        .await
        .unwrap();
    assert_eq!(finished.status, AgentLifecycle::Idle);
    lc.manager.archive_agent(&agent.id).await.unwrap();
    lc.flush().await;

    // Archiving the workspace removes the worktree it owned: reading the persisted history must
    // not depend on that directory.
    std::fs::remove_dir_all(&worktree).unwrap();
    let loaded = lc.load(&agent.id).await.unwrap();

    assert_eq!(loaded.id, agent.id);
    let replies: Vec<_> = lc
        .items(&agent.id)
        .await
        .into_iter()
        .filter(|i| {
            matches!(
                i,
                paseo_agent_types::AgentTimelineItem::AssistantMessage { .. }
            )
        })
        .collect();
    assert!(!replies.is_empty());
    // An interactive session in the removed directory is refused, history is not.
    lc.manager.close_agent(&agent.id).await.unwrap();
    lc.manager
        .unarchive_snapshot(&agent.id, UnarchiveUpdates::default())
        .await
        .unwrap();
    let error = lc.load(&agent.id).await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("Working directory does not exist"),
        "{error}"
    );
}
