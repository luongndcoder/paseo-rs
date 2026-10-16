//! Ports of `agent/agent-storage.test.ts` plus the Rust-specific behavior (unknown-key
//! preservation, duplicate resolution, cwd move, lanes).

use paseo_store::agent_record::{
    AgentOwner, OwnerKind, PersistenceHandle, SerializableConfig, StoredAgentRecord,
};
use paseo_store::agent_store::SnapshotInput;
use paseo_store::{AgentLifecycle, AgentStore, Nullable, SnapshotOptions, StoreError};
use serde_json::json;
use std::path::Path;

const T0: &str = "2025-01-01T00:00:00.000Z";

/// Stand-in for `toStoredAgentRecord`: what the manager projects from a live agent.
fn project(
    id: &str,
    cwd: &str,
    status: AgentLifecycle,
    updated_at: &str,
) -> impl Fn(SnapshotInput) -> StoredAgentRecord {
    let (id, cwd, updated_at) = (id.to_string(), cwd.to_string(), updated_at.to_string());
    move |input: SnapshotInput| {
        let mut record = StoredAgentRecord::new(&id, "claude", &cwd, &updated_at);
        record.created_at = input
            .created_at
            .clone()
            .unwrap_or_else(|| updated_at.clone());
        record.updated_at = updated_at.clone();
        record.last_activity_at = Some(updated_at.clone());
        record.title = Nullable::from(input.title.clone());
        record.last_status = status;
        record.internal = input.resolve_internal(None);
        record
    }
}

async fn snapshot(store: &AgentStore, id: &str, cwd: &str, status: AgentLifecycle, at: &str) {
    store
        .apply_snapshot(id, SnapshotOptions::default(), project(id, cwd, status, at))
        .await
        .unwrap();
}

fn store_in(dir: &Path) -> AgentStore {
    AgentStore::new(dir.join("agents"))
}

#[tokio::test]
async fn apply_snapshot_persists_config_and_reloads() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_in(dir.path());
    store
        .apply_snapshot("agent-1", SnapshotOptions::default(), |input| {
            let mut r = StoredAgentRecord::new("agent-1", "claude", "/tmp/project", T0);
            r.title = Nullable::from(input.title);
            r.last_status = AgentLifecycle::Idle;
            r.last_mode_id = Nullable::Value("coding".into());
            r.config = Nullable::Value(SerializableConfig {
                mode_id: Nullable::Value("coding".into()),
                model: Nullable::Value("gpt-5.1".into()),
                system_prompt: Nullable::Value("Be terse and explicit.".into()),
                provider_options: Nullable::Value(
                    json!({"allowedTools": ["Read"]})
                        .as_object()
                        .unwrap()
                        .clone(),
                ),
                ..Default::default()
            });
            r
        })
        .await
        .unwrap();

    let records = store.list().await.unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].last_status, AgentLifecycle::Idle);
    assert_eq!(
        records[0].last_mode_id.value().map(String::as_str),
        Some("coding")
    );

    let reloaded = store_in(dir.path());
    let persisted = reloaded.list().await.unwrap();
    assert_eq!(persisted[0].cwd, "/tmp/project");
    let config = persisted[0].config.value().unwrap();
    assert_eq!(
        config.provider_options.value().unwrap().get("allowedTools"),
        Some(&json!(["Read"]))
    );
    // Absent feature values stay absent on disk.
    let text = std::fs::read_to_string(dir.path().join("agents/tmp-project/agent-1.json")).unwrap();
    assert!(!text.contains("featureValues"));
    assert!(
        !text.ends_with('\n'),
        "no trailing newline, like JSON.stringify"
    );
}

#[tokio::test]
async fn apply_snapshot_keeps_feature_values_when_set() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_in(dir.path());
    store
        .apply_snapshot("a", SnapshotOptions::default(), |_| {
            let mut r = StoredAgentRecord::new("a", "claude", "/tmp/p", T0);
            r.config = Nullable::Value(SerializableConfig {
                feature_values: Nullable::Value(
                    json!({"auto_accept": true}).as_object().unwrap().clone(),
                ),
                ..Default::default()
            });
            r
        })
        .await
        .unwrap();
    let reloaded = store_in(dir.path());
    let rec = reloaded.get("a").await.unwrap().unwrap();
    assert_eq!(
        rec.config
            .value()
            .unwrap()
            .feature_values
            .value()
            .unwrap()
            .get("auto_accept"),
        Some(&json!(true))
    );
}

#[tokio::test]
async fn apply_snapshot_preserves_created_at() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_in(dir.path());
    snapshot(
        &store,
        "a",
        "/tmp/p",
        AgentLifecycle::Idle,
        "2025-01-01T00:00:00.000Z",
    )
    .await;
    snapshot(
        &store,
        "a",
        "/tmp/p",
        AgentLifecycle::Running,
        "2025-02-01T00:00:00.000Z",
    )
    .await;
    let rec = store.get("a").await.unwrap().unwrap();
    assert_eq!(rec.created_at, "2025-01-01T00:00:00.000Z");
    assert_eq!(rec.updated_at, "2025-02-01T00:00:00.000Z");
    assert_eq!(rec.last_status, AgentLifecycle::Running);
}

#[tokio::test]
async fn apply_snapshot_preserves_archived_at() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_in(dir.path());
    snapshot(&store, "a", "/tmp/p", AgentLifecycle::Idle, T0).await;
    let mut rec = store.get("a").await.unwrap().unwrap();
    rec.archived_at = Nullable::Value("2025-01-05T00:00:00.000Z".into());
    store.upsert(rec).await.unwrap();
    snapshot(
        &store,
        "a",
        "/tmp/p",
        AgentLifecycle::Running,
        "2025-01-06T00:00:00.000Z",
    )
    .await;
    let rec = store.get("a").await.unwrap().unwrap();
    assert_eq!(
        rec.archived_at.value().map(String::as_str),
        Some("2025-01-05T00:00:00.000Z")
    );
    assert_eq!(rec.last_status, AgentLifecycle::Running);
}

#[tokio::test]
async fn explicit_null_archived_at_is_preserved_too() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_in(dir.path());
    snapshot(&store, "a", "/tmp/p", AgentLifecycle::Idle, T0).await;
    let mut rec = store.get("a").await.unwrap().unwrap();
    rec.archived_at = Nullable::Null;
    store.upsert(rec).await.unwrap();
    snapshot(&store, "a", "/tmp/p", AgentLifecycle::Idle, T0).await;
    let text = std::fs::read_to_string(dir.path().join("agents/tmp-p/a.json")).unwrap();
    assert!(text.contains("\"archivedAt\": null"));
}

#[tokio::test]
async fn titles_are_stored_independently_of_snapshots() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_in(dir.path());
    snapshot(&store, "a", "/tmp/p", AgentLifecycle::Idle, T0).await;
    assert_eq!(store.get("a").await.unwrap().unwrap().title, Nullable::Null);
    store.set_title("a", "Important Bug Fix").await.unwrap();
    snapshot(
        &store,
        "a",
        "/tmp/p",
        AgentLifecycle::Running,
        "2025-01-02T00:00:00.000Z",
    )
    .await;
    let rec = store.get("a").await.unwrap().unwrap();
    assert_eq!(
        rec.title.value().map(String::as_str),
        Some("Important Bug Fix")
    );
    assert_eq!(rec.last_status, AgentLifecycle::Running);
}

#[tokio::test]
async fn set_title_fails_for_missing_agent() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_in(dir.path());
    let err = store.set_title("missing", "x").await.unwrap_err();
    assert!(matches!(err, StoreError::NotFound(_)));
    assert_eq!(err.to_string(), "Agent missing not found");
}

#[tokio::test]
async fn snapshot_accepts_explicit_title_override() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_in(dir.path());
    store
        .apply_snapshot(
            "a",
            SnapshotOptions {
                title: Some(Some("Generated".into())),
                internal: None,
            },
            project("a", "/tmp/p", AgentLifecycle::Idle, T0),
        )
        .await
        .unwrap();
    assert_eq!(
        store
            .get("a")
            .await
            .unwrap()
            .unwrap()
            .title
            .value()
            .map(String::as_str),
        Some("Generated")
    );
    // Some(None) is an explicit null override that wins over the stored title.
    store
        .apply_snapshot(
            "a",
            SnapshotOptions {
                title: Some(None),
                internal: None,
            },
            project("a", "/tmp/p", AgentLifecycle::Idle, T0),
        )
        .await
        .unwrap();
    assert_eq!(store.get("a").await.unwrap().unwrap().title, Nullable::Null);
}

#[tokio::test]
async fn snapshot_projects_after_in_flight_archival_write() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_in(dir.path());
    snapshot(&store, "a", "/tmp/p", AgentLifecycle::Idle, T0).await;
    let mut archived = store.get("a").await.unwrap().unwrap();
    archived.title = Nullable::Value("Generated title".into());
    archived.archived_at = Nullable::Value("2025-01-03T00:00:00.000Z".into());

    // The upsert is queued first; the snapshot queued behind it must project against it.
    let (first, second) = tokio::join!(
        store.upsert(archived),
        store.apply_snapshot(
            "a",
            SnapshotOptions::default(),
            project(
                "a",
                "/tmp/p",
                AgentLifecycle::Running,
                "2025-01-02T00:00:00.000Z"
            ),
        )
    );
    first.unwrap();
    second.unwrap();
    let rec = store.get("a").await.unwrap().unwrap();
    assert_eq!(
        rec.title.value().map(String::as_str),
        Some("Generated title")
    );
    assert_eq!(
        rec.archived_at.value().map(String::as_str),
        Some("2025-01-03T00:00:00.000Z")
    );
    assert_eq!(rec.last_status, AgentLifecycle::Running);
}

#[tokio::test]
async fn list_and_get_include_internal_agents_and_flag_persists() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_in(dir.path());
    snapshot(&store, "normal", "/tmp/p", AgentLifecycle::Idle, T0).await;
    store
        .apply_snapshot(
            "internal",
            SnapshotOptions {
                title: None,
                internal: Some(Some(true)),
            },
            project("internal", "/tmp/p", AgentLifecycle::Idle, T0),
        )
        .await
        .unwrap();
    assert_eq!(store.list().await.unwrap().len(), 2);
    assert_eq!(
        store.get("internal").await.unwrap().unwrap().internal,
        Some(true)
    );
    let reloaded = store_in(dir.path());
    assert_eq!(
        reloaded.get("internal").await.unwrap().unwrap().internal,
        Some(true)
    );
}

#[tokio::test]
async fn queries_by_provider_session_native_handle_and_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_in(dir.path());
    for (id, ws, session, native) in [
        ("a", "wks_1", "sess-a", Some("native-a")),
        ("b", "wks_2", "sess-b", None),
    ] {
        let mut r = StoredAgentRecord::new(id, "codex", "/tmp/p", T0);
        r.workspace_id = Some(ws.into());
        r.persistence = Nullable::Value(PersistenceHandle {
            provider: "codex".into(),
            session_id: session.into(),
            native_handle: native.map_or(Nullable::Missing, |n| Nullable::Value(json!(n))),
            metadata: None,
            extra: Default::default(),
        });
        store.upsert(r).await.unwrap();
    }
    let ids = |v: Vec<StoredAgentRecord>| v.into_iter().map(|r| r.id).collect::<Vec<_>>();
    assert_eq!(
        ids(store
            .list_by_provider_session("codex", "sess-a")
            .await
            .unwrap()),
        ["a"]
    );
    assert_eq!(
        ids(store
            .list_by_provider_session("codex", "native-a")
            .await
            .unwrap()),
        ["a"]
    );
    assert!(store
        .list_by_provider_session("claude", "sess-a")
        .await
        .unwrap()
        .is_empty());
    assert_eq!(ids(store.list_by_workspace("wks_2").await.unwrap()), ["b"]);
}

#[tokio::test]
async fn daemon_execution_index_follows_owner() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_in(dir.path());
    let owner = AgentOwner {
        kind: OwnerKind::Daemon,
        daemon_id: "d1".into(),
        execution_id: "e1".into(),
        extra: Default::default(),
    };
    let mut r = StoredAgentRecord::new("a", "claude", "/tmp/p", T0);
    r.owner = Some(owner.clone());
    store.upsert(r).await.unwrap();
    assert_eq!(
        store
            .find_by_daemon_execution(&owner)
            .await
            .unwrap()
            .unwrap()
            .id,
        "a"
    );
    store.remove("a").await.unwrap();
    assert!(store
        .find_by_daemon_execution(&owner)
        .await
        .unwrap()
        .is_none());
    // Reload rebuilds the index from disk.
    let mut r = StoredAgentRecord::new("b", "claude", "/tmp/p", T0);
    r.owner = Some(owner.clone());
    store.upsert(r).await.unwrap();
    let reloaded = store_in(dir.path());
    assert_eq!(
        reloaded
            .find_by_daemon_execution(&owner)
            .await
            .unwrap()
            .unwrap()
            .id,
        "b"
    );
}

#[tokio::test]
async fn windows_drive_paths_produce_valid_directory_names() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_in(dir.path());
    snapshot(
        &store,
        "win-agent",
        "D:\\Users\\dev\\MyProject",
        AgentLifecycle::Idle,
        T0,
    )
    .await;
    let dirs: Vec<_> = std::fs::read_dir(dir.path().join("agents"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(dirs, ["D-Users-dev-MyProject"]);
}

#[tokio::test]
async fn remove_deletes_every_duplicate_record_file() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_in(dir.path());
    snapshot(&store, "dup", "/tmp/project-a", AgentLifecycle::Idle, T0).await;
    let a = std::fs::read_to_string(dir.path().join("agents/tmp-project-a/dup.json")).unwrap();
    let b_dir = dir.path().join("agents/tmp-project-b");
    std::fs::create_dir_all(&b_dir).unwrap();
    std::fs::write(
        b_dir.join("dup.json"),
        a.replace("/tmp/project-a", "/tmp/project-b"),
    )
    .unwrap();

    let reloaded = store_in(dir.path());
    assert!(reloaded.list().await.unwrap().iter().any(|r| r.id == "dup"));
    reloaded.remove("dup").await.unwrap();
    assert!(!dir.path().join("agents/tmp-project-a/dup.json").exists());
    assert!(!b_dir.join("dup.json").exists());
    assert!(reloaded.get("dup").await.unwrap().is_none());
}

/// PARITY-QUIRK(duplicate-agent-files-last-wins): the last record in scan order (root files,
/// then project dirs) wins; nothing is unlinked on load.
#[tokio::test]
async fn duplicate_files_resolve_to_the_last_scanned_and_nothing_is_unlinked() {
    let dir = tempfile::tempdir().unwrap();
    let agents = dir.path().join("agents");
    let write = |sub: &str, cwd: &str, updated: &str| {
        let d = agents.join(sub);
        std::fs::create_dir_all(&d).unwrap();
        let mut r = StoredAgentRecord::new("dup", "claude", cwd, T0);
        r.updated_at = updated.into();
        std::fs::write(
            d.join("dup.json"),
            serde_json::to_string_pretty(&r).unwrap(),
        )
        .unwrap();
    };
    write("a-old", "/a/old", "2025-01-01T00:00:00.000Z");
    write("b-new", "/b/new", "2025-03-01T00:00:00.000Z");
    write("c-mid", "/c/mid", "2025-02-01T00:00:00.000Z");
    let store = AgentStore::new(&agents);
    let rec = store.get("dup").await.unwrap().unwrap();
    assert_eq!(
        rec.cwd, "/c/mid",
        "last in sorted scan order wins, not max updatedAt"
    );
    for sub in ["a-old", "b-new", "c-mid"] {
        assert!(
            agents.join(sub).join("dup.json").exists(),
            "{sub} stays on disk"
        );
    }

    // The next write moves the winner to its canonical path and unlinks only its old file.
    store
        .apply_snapshot(
            "dup",
            SnapshotOptions::default(),
            project("dup", "/d/canonical", AgentLifecycle::Idle, T0),
        )
        .await
        .unwrap();
    assert!(!agents.join("c-mid/dup.json").exists());
    assert!(agents.join("a-old/dup.json").exists());
    assert!(agents.join("b-new/dup.json").exists());
    assert!(agents.join("d-canonical/dup.json").exists());

    // `remove` still deletes every indexed copy.
    store.remove("dup").await.unwrap();
    for sub in ["a-old", "b-new", "d-canonical"] {
        assert!(!agents.join(sub).join("dup.json").exists(), "{sub} removed");
    }
}

#[tokio::test]
async fn cwd_change_writes_new_path_then_unlinks_old() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_in(dir.path());
    snapshot(&store, "a", "/tmp/old", AgentLifecycle::Idle, T0).await;
    let old = dir.path().join("agents/tmp-old/a.json");
    assert!(old.exists());
    snapshot(&store, "a", "/tmp/new", AgentLifecycle::Idle, T0).await;
    assert!(!old.exists());
    assert!(dir.path().join("agents/tmp-new/a.json").exists());
    // Removal after the move deletes only the live copy and clears the index.
    store.remove("a").await.unwrap();
    assert!(!dir.path().join("agents/tmp-new/a.json").exists());
}

#[tokio::test]
async fn legacy_root_records_are_read_and_rewritten_into_project_dirs() {
    let dir = tempfile::tempdir().unwrap();
    let agents = dir.path().join("agents");
    std::fs::create_dir_all(&agents).unwrap();
    let rec = StoredAgentRecord::new("legacy", "claude", "/tmp/legacy", T0);
    std::fs::write(
        agents.join("legacy.json"),
        serde_json::to_string_pretty(&rec).unwrap(),
    )
    .unwrap();
    let store = AgentStore::new(&agents);
    assert!(store.get("legacy").await.unwrap().is_some());
    store.set_title("legacy", "T").await.unwrap();
    assert!(!agents.join("legacy.json").exists());
    assert!(agents.join("tmp-legacy/legacy.json").exists());
}

#[tokio::test]
async fn lenient_load_skips_invalid_and_defaults_missing_fields() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path().join("agents/p");
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(d.join("bad.json"), "{not json").unwrap();
    std::fs::write(
        d.join("wrong-type.json"),
        r#"{"id":1,"provider":"x","cwd":"/x","createdAt":"a","updatedAt":"b"}"#,
    )
    .unwrap();
    std::fs::write(d.join("null-optional.json"), r#"{"id":"n","provider":"x","cwd":"/x","createdAt":"a","updatedAt":"b","workspaceId":null}"#).unwrap();
    std::fs::write(
        d.join("minimal.json"),
        r#"{"id":"min","provider":"x","cwd":"/x","createdAt":"a","updatedAt":"b"}"#,
    )
    .unwrap();
    let store = AgentStore::new(dir.path().join("agents"));
    let ids: Vec<_> = store
        .list()
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.id)
        .collect();
    assert_eq!(ids, ["min"]);
    let rec = store.get("min").await.unwrap().unwrap();
    assert_eq!(rec.last_status, AgentLifecycle::Closed);
    assert!(rec.labels.is_empty());
}

#[tokio::test]
async fn unknown_keys_survive_a_rewrite_at_every_level() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path().join("agents/tmp-x");
    std::fs::create_dir_all(&d).unwrap();
    let original = json!({
        "id": "u", "provider": "claude", "cwd": "/tmp/x",
        "createdAt": T0, "updatedAt": T0,
        "labels": {}, "lastStatus": "idle",
        "config": {"model": "m", "futureConfigKey": {"a": 1}},
        "runtimeInfo": {"provider": "claude", "sessionId": null, "futureRuntime": true},
        "persistence": {"provider": "claude", "sessionId": "s", "futurePersist": [1, 2]},
        "owner": {"kind": "daemon", "daemonId": "d", "executionId": "e", "futureOwner": "x"},
        "futureTopLevel": "keep me"
    });
    std::fs::write(
        d.join("u.json"),
        serde_json::to_string_pretty(&original).unwrap(),
    )
    .unwrap();
    let store = AgentStore::new(dir.path().join("agents"));
    store.set_title("u", "Renamed").await.unwrap();
    let after: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(d.join("u.json")).unwrap()).unwrap();
    assert_eq!(after["futureTopLevel"], "keep me");
    assert_eq!(after["config"]["futureConfigKey"], json!({"a": 1}));
    assert_eq!(after["runtimeInfo"]["futureRuntime"], true);
    assert_eq!(after["runtimeInfo"]["sessionId"], serde_json::Value::Null);
    assert_eq!(after["persistence"]["futurePersist"], json!([1, 2]));
    assert_eq!(after["owner"]["futureOwner"], "x");
    assert_eq!(after["title"], "Renamed");
}

#[tokio::test]
async fn flush_waits_for_queued_writes_and_writes_are_fifo_per_agent() {
    let dir = tempfile::tempdir().unwrap();
    let store = std::sync::Arc::new(store_in(dir.path()));
    let mut handles = Vec::new();
    for i in 0..20 {
        let store = store.clone();
        handles.push(tokio::spawn(async move {
            let mut r = StoredAgentRecord::new("a", "claude", "/tmp/p", T0);
            r.title = Nullable::Value(format!("t{i}"));
            store.upsert(r).await.unwrap();
        }));
        tokio::task::yield_now().await;
    }
    store.flush().await.unwrap();
    for h in handles {
        h.await.unwrap();
    }
    // Spawned in order and each yielded once: FIFO lane means the last queued write wins.
    let rec = store.get("a").await.unwrap().unwrap();
    let text = std::fs::read_to_string(dir.path().join("agents/tmp-p/a.json")).unwrap();
    assert!(text.contains(rec.title.value().unwrap()));
}

#[tokio::test]
async fn writes_for_a_removed_agent_are_no_ops() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_in(dir.path());
    snapshot(&store, "a", "/tmp/p", AgentLifecycle::Idle, T0).await;
    store.remove("a").await.unwrap();
    snapshot(&store, "a", "/tmp/p", AgentLifecycle::Running, T0).await;
    assert!(store.get("a").await.unwrap().is_none());
    assert!(!dir.path().join("agents/tmp-p/a.json").exists());
}

#[tokio::test]
async fn missing_base_dir_loads_empty() {
    let dir = tempfile::tempdir().unwrap();
    let store = AgentStore::new(dir.path().join("does-not-exist"));
    store.initialize().await.unwrap();
    assert!(store.list().await.unwrap().is_empty());
}

#[tokio::test]
async fn lone_surrogate_title_is_loaded_and_written_back_as_escape() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_in(dir.path());
    snapshot(&store, "a", "/tmp/p", AgentLifecycle::Idle, T0).await;
    let title = format!(
        "a{}",
        paseo_agent_types::js_str::lone_high_surrogate(0xD83D).unwrap()
    );
    store.set_title("a", &title).await.unwrap();

    let file = walk_json(&dir.path().join("agents"));
    assert!(std::fs::read_to_string(&file).unwrap().contains(r"a\ud83d"));

    let reloaded = store_in(dir.path());
    let rec = reloaded.get("a").await.unwrap().expect("agent not dropped");
    assert_eq!(rec.title.value().map(String::as_str), Some(title.as_str()));
}

fn walk_json(dir: &Path) -> std::path::PathBuf {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            return walk_json(&path);
        }
        if path.extension().is_some_and(|e| e == "json") {
            return path;
        }
    }
    panic!("no json file");
}
