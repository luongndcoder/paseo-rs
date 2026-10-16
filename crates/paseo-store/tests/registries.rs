//! Ports of `workspace-registry.test.ts` plus registry-level parity quirks.

use paseo_fs_atomic::{write_json_file_atomic, FsyncPolicy};
use paseo_store::project_registry::{
    GetOrCreateByRootInput, MutationKind, ProjectMutation, ProjectRegistryOptions,
};
use paseo_store::registry::BoxFuture;
use paseo_store::workspace_registry::{
    WorkspaceArchiveContext, WorkspaceMutationContext, WorkspaceRegistry,
};
use paseo_store::{
    ProjectKind, ProjectRecord, ProjectRegistry, StoreError, WorkspaceKind, WorkspaceRecord,
};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const T1: &str = "2026-03-01T00:00:00.000Z";
const T2: &str = "2026-03-02T00:00:00.000Z";
const T3: &str = "2026-03-03T00:00:00.000Z";

fn project(id: &str, root: &str, created: &str, updated: &str) -> ProjectRecord {
    ProjectRecord::new(id, root, ProjectKind::Git, id, created, updated)
}

fn workspace(id: &str) -> WorkspaceRecord {
    WorkspaceRecord::new(
        id,
        "project-one",
        "/tmp/repo",
        WorkspaceKind::LocalCheckout,
        "main",
        T1,
        T1,
    )
}

fn projects_path(dir: &Path) -> PathBuf {
    dir.join("projects").join("projects.json")
}

fn workspaces_path(dir: &Path) -> PathBuf {
    dir.join("projects").join("workspaces.json")
}

fn root_input(root: &str, kind: ProjectKind, name: &str, ts: &str) -> GetOrCreateByRootInput {
    GetOrCreateByRootInput {
        root_path: root.into(),
        kind,
        display_name: name.into(),
        project_key: None,
        timestamp: ts.into(),
    }
}

#[tokio::test]
async fn project_records_create_update_archive_remove() {
    let dir = tempfile::tempdir().unwrap();
    let registry = ProjectRegistry::new(projects_path(dir.path()));
    registry.initialize().await;
    let id = "remote:github.com/acme/repo";
    registry
        .upsert(project(id, "/tmp/repo", T1, T1))
        .await
        .unwrap();
    registry
        .upsert(project(id, "/tmp/repo", T1, T2))
        .await
        .unwrap();
    registry.archive(id, T3).await.unwrap();
    let archived = registry.get(id).await.unwrap();
    assert_eq!(archived.archived_at.as_deref(), Some(T3));
    assert_eq!(registry.list().await.len(), 1);
    registry.remove(id).await.unwrap();
    assert!(registry.get(id).await.is_none());
    assert!(registry.list().await.is_empty());
}

#[tokio::test]
async fn project_archive_preserves_a_concurrent_update() {
    let dir = tempfile::tempdir().unwrap();
    let pause = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
    let started_tx = Arc::new(Mutex::new(Some(started_tx)));
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let release_rx = Arc::new(Mutex::new(release_rx));
    let writer_pause = pause.clone();
    let writer: paseo_store::registry::WriteRecords<ProjectRecord> =
        Arc::new(move |path, records| {
            if writer_pause.swap(false, std::sync::atomic::Ordering::SeqCst) {
                if let Some(tx) = started_tx.lock().unwrap().take() {
                    let _ = tx.send(());
                }
                let _ = release_rx.lock().unwrap().recv();
            }
            write_json_file_atomic(path, &records, FsyncPolicy::Off)
        });
    let registry = Arc::new(ProjectRegistry::with_options(
        projects_path(dir.path()),
        ProjectRegistryOptions {
            write_records: Some(writer),
            ..Default::default()
        },
    ));
    registry
        .upsert(project("project-concurrent", "/tmp/c", T1, T1))
        .await
        .unwrap();
    pause.store(true, std::sync::atomic::Ordering::SeqCst);

    let r1 = registry.clone();
    let update = tokio::spawn(async move {
        r1.update("project-concurrent", |current| {
            let mut next = current.clone();
            next.custom_name = Some("Kept name".into());
            next.updated_at = T2.into();
            next
        })
        .await
        .unwrap();
    });
    started_rx.await.unwrap();
    let r2 = registry.clone();
    let archive = tokio::spawn(async move { r2.archive("project-concurrent", T3).await.unwrap() });
    tokio::task::yield_now().await;
    release_tx.send(()).unwrap();
    update.await.unwrap();
    archive.await.unwrap();
    let rec = registry.get("project-concurrent").await.unwrap();
    assert_eq!(rec.custom_name.as_deref(), Some("Kept name"));
    assert_eq!(rec.archived_at.as_deref(), Some(T3));
}

#[tokio::test]
async fn project_mutations_publish_only_lifecycle_changes() {
    let dir = tempfile::tempdir().unwrap();
    let registry = ProjectRegistry::new(projects_path(dir.path()));
    registry.initialize().await;
    let seen: Arc<Mutex<Vec<ProjectMutation>>> = Arc::default();
    let sink = seen.clone();
    let subscription = registry.subscribe(Arc::new(move |m| {
        sink.lock().unwrap().push(m);
        Box::pin(async { Ok(()) }) as BoxFuture<_>
    }));
    let active = project("project-one", "/tmp/project-one", T1, T1);
    registry.upsert(active.clone()).await.unwrap();
    registry.archive("project-one", T2).await.unwrap();
    registry.archive("project-one", T3).await.unwrap();
    registry.archive("project-unknown", T3).await.unwrap();
    registry.remove("project-one").await.unwrap();
    registry.remove("project-one").await.unwrap();
    registry.remove("project-unknown").await.unwrap();

    let mut archived = active.clone();
    archived.updated_at = T2.into();
    archived.archived_at = Some(T2.into());
    assert_eq!(
        *seen.lock().unwrap(),
        vec![
            ProjectMutation {
                kind: MutationKind::Upsert,
                project_id: "project-one".into(),
                project: Some(active)
            },
            ProjectMutation {
                kind: MutationKind::Archive,
                project_id: "project-one".into(),
                project: Some(archived)
            },
            ProjectMutation {
                kind: MutationKind::Remove,
                project_id: "project-one".into(),
                project: None
            },
        ]
    );
    subscription.unsubscribe();
    registry
        .upsert(project("later", "/tmp/later", T1, T1))
        .await
        .unwrap();
    assert_eq!(
        seen.lock().unwrap().len(),
        3,
        "unsubscribed listener stays silent"
    );
}

#[tokio::test]
async fn project_listener_error_rejects_after_the_write() {
    let dir = tempfile::tempdir().unwrap();
    let registry = ProjectRegistry::new(projects_path(dir.path()));
    registry.subscribe(Arc::new(|_| {
        Box::pin(async { Err(StoreError::Listener("observer failed".into())) }) as BoxFuture<_>
    }));
    let err = registry
        .upsert(project("p", "/tmp/p", T1, T1))
        .await
        .unwrap_err();
    assert_eq!(err.to_string(), "observer failed");
    assert!(registry.get("p").await.is_some(), "the write is durable");
    let reloaded = ProjectRegistry::new(projects_path(dir.path()));
    assert!(reloaded.get("p").await.is_some());
}

#[tokio::test]
async fn concurrent_exact_root_adds_allocate_one_project() {
    let dir = tempfile::tempdir().unwrap();
    let registry = Arc::new(ProjectRegistry::new(projects_path(dir.path())));
    registry.initialize().await;
    let mut tasks = Vec::new();
    for i in 0..8 {
        let r = registry.clone();
        tasks.push(tokio::spawn(async move {
            r.get_or_create_active_by_root(root_input(
                "/work/shared",
                ProjectKind::NonGit,
                "shared",
                &format!("2026-03-0{}T00:00:00.000Z", i + 1),
            ))
            .await
            .unwrap()
        }));
    }
    let mut ids = std::collections::HashSet::new();
    for t in tasks {
        ids.insert(t.await.unwrap().project_id);
    }
    assert_eq!(ids.len(), 1);
    assert_eq!(registry.list().await.len(), 1);
    let id = ids.into_iter().next().unwrap();
    assert!(id.starts_with("prj_") && id.len() == 20);
    assert!(id[4..]
        .chars()
        .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
}

#[tokio::test]
async fn legacy_ids_coexist_with_new_opaque_ids() {
    let dir = tempfile::tempdir().unwrap();
    let registry = ProjectRegistry::new(projects_path(dir.path()));
    registry
        .upsert(project("remote:legacy", "/work/legacy", T1, T1))
        .await
        .unwrap();
    let reused = registry
        .get_or_create_active_by_root(root_input("/work/legacy", ProjectKind::Git, "x", T2))
        .await
        .unwrap();
    assert_eq!(reused.project_id, "remote:legacy");
    assert_eq!(reused.updated_at, T1, "nothing differs, nothing refreshed");
    let fresh = registry
        .get_or_create_active_by_root(root_input("/work/other", ProjectKind::Git, "o", T2))
        .await
        .unwrap();
    assert!(fresh.project_id.starts_with("prj_"));
    assert_eq!(registry.list().await.len(), 2);
}

#[tokio::test]
async fn archived_only_root_gets_a_fresh_project() {
    let dir = tempfile::tempdir().unwrap();
    let registry = ProjectRegistry::new(projects_path(dir.path()));
    let mut archived = project("prj_archived", "/work/archived-root", T1, T1);
    archived.archived_at = Some(T2.into());
    registry.upsert(archived.clone()).await.unwrap();
    let created = registry
        .get_or_create_active_by_root(root_input(
            "/work/archived-root",
            ProjectKind::NonGit,
            "archived-root",
            T3,
        ))
        .await
        .unwrap();
    assert_ne!(created.project_id, "prj_archived");
    assert!(created.archived_at.is_none());
    assert_eq!(registry.get("prj_archived").await.unwrap(), archived);
}

#[tokio::test]
async fn oldest_active_duplicate_is_refreshed_without_changing_identity() {
    let dir = tempfile::tempdir().unwrap();
    let registry = ProjectRegistry::new(projects_path(dir.path()));
    // Insert the younger one first: selection is by createdAt, not insertion order.
    registry
        .upsert(project("remote:duplicate", "/work/legacy-root", T2, T2))
        .await
        .unwrap();
    registry
        .upsert(project("remote:oldest", "/work/legacy-root", T1, T1))
        .await
        .unwrap();
    let mut input = root_input("/work/legacy-root", ProjectKind::NonGit, "x", T3);
    input.project_key = Some("remote:key".into());
    let refreshed = registry.get_or_create_active_by_root(input).await.unwrap();
    assert_eq!(refreshed.project_id, "remote:oldest");
    assert_eq!(refreshed.kind, ProjectKind::NonGit);
    assert_eq!(refreshed.project_key.as_deref(), Some("remote:key"));
    assert_eq!(refreshed.updated_at, T3);
    assert_eq!(refreshed.created_at, T1);
    assert_eq!(
        registry.get("remote:duplicate").await.unwrap().kind,
        ProjectKind::Git
    );
}

#[tokio::test]
async fn windows_lexical_equivalent_roots_reuse_the_project() {
    let dir = tempfile::tempdir().unwrap();
    let registry = ProjectRegistry::new(projects_path(dir.path()));
    let first = registry
        .get_or_create_active_by_root(root_input(
            "C:\\Users\\Dev\\Repo",
            ProjectKind::Git,
            "repo",
            T1,
        ))
        .await
        .unwrap();
    let second = registry
        .get_or_create_active_by_root(root_input(
            "c:/users/dev/repo/",
            ProjectKind::Git,
            "repo",
            T2,
        ))
        .await
        .unwrap();
    assert_eq!(first.project_id, second.project_id);
    assert_eq!(registry.list().await.len(), 1);
}

#[cfg(unix)]
#[tokio::test]
async fn lexical_and_symlink_spellings_stay_distinct() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("target");
    let link = dir.path().join("link");
    std::fs::create_dir(&target).unwrap();
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let registry = ProjectRegistry::new(projects_path(dir.path()));
    let a = registry
        .get_or_create_active_by_root(root_input(
            target.to_str().unwrap(),
            ProjectKind::NonGit,
            "target",
            T1,
        ))
        .await
        .unwrap();
    let b = registry
        .get_or_create_active_by_root(root_input(
            link.to_str().unwrap(),
            ProjectKind::NonGit,
            "link",
            T2,
        ))
        .await
        .unwrap();
    assert_ne!(a.project_id, b.project_id);
    assert_eq!(registry.list().await, vec![a, b]);
}

#[tokio::test]
async fn generated_project_id_collision_is_retried() {
    let dir = tempfile::tempdir().unwrap();
    let ids = Mutex::new(vec!["prj_fresh".to_string(), "prj_collision".to_string()]);
    let registry = ProjectRegistry::with_options(
        projects_path(dir.path()),
        ProjectRegistryOptions {
            project_id_factory: Some(Box::new(move || {
                ids.lock()
                    .unwrap()
                    .pop()
                    .unwrap_or_else(|| "prj_unexpected".into())
            })),
            ..Default::default()
        },
    );
    registry
        .upsert(project("prj_collision", "/work/existing", T1, T1))
        .await
        .unwrap();
    let created = registry
        .get_or_create_active_by_root(root_input("/work/new", ProjectKind::NonGit, "new", T2))
        .await
        .unwrap();
    assert_eq!(created.project_id, "prj_fresh");
    assert_eq!(registry.list().await.len(), 2);
}

#[tokio::test]
async fn legacy_project_records_without_optional_fields_load_and_normalize_to_null() {
    let dir = tempfile::tempdir().unwrap();
    let path = projects_path(dir.path());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        &path,
        r#"[{"projectId":"legacy","rootPath":"/x","kind":"git","displayName":"x","createdAt":"a","updatedAt":"b","archivedAt":null}]"#,
    )
    .unwrap();
    let registry = ProjectRegistry::new(&path);
    let rec = registry.get("legacy").await.unwrap();
    assert_eq!(
        (
            rec.project_key.clone(),
            rec.custom_name.clone(),
            rec.custom_icon_revision.clone()
        ),
        (None, None, None)
    );
    registry
        .update("legacy", |r| {
            let mut n = r.clone();
            n.custom_name = Some("Mine".into());
            n
        })
        .await
        .unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("\"projectKey\": null") && text.contains("\"customIconRevision\": null"));
    assert_eq!(
        registry
            .get("legacy")
            .await
            .unwrap()
            .resolved_display_name(),
        "Mine"
    );
}

#[tokio::test]
async fn archivedat_is_required_in_registry_records() {
    let dir = tempfile::tempdir().unwrap();
    let path = projects_path(dir.path());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        &path,
        r#"[{"projectId":"p","rootPath":"/x","kind":"git","displayName":"x","createdAt":"a","updatedAt":"b"}]"#,
    )
    .unwrap();
    assert!(ProjectRegistry::new(&path).list().await.is_empty());
}

#[tokio::test]
async fn corrupt_registry_serves_empty_and_the_next_write_replaces_it() {
    // PARITY-QUIRK(corrupt-registry-truncate): same as TS, plus a warn log.
    let dir = tempfile::tempdir().unwrap();
    let path = projects_path(dir.path());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "{ definitely not json").unwrap();
    let registry = ProjectRegistry::new(&path);
    assert!(registry.list().await.is_empty());
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "{ definitely not json"
    );
    registry.upsert(project("p", "/x", T1, T1)).await.unwrap();
    assert_eq!(registry.list().await.len(), 1);
    let reloaded = ProjectRegistry::new(&path);
    assert_eq!(reloaded.list().await.len(), 1);
}

#[tokio::test]
async fn one_bad_record_empties_the_whole_registry() {
    // PARITY-QUIRK(corrupt-registry-truncate): z.array(schema).parse is all-or-nothing.
    let dir = tempfile::tempdir().unwrap();
    let path = projects_path(dir.path());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let good = serde_json::to_value(project("good", "/x", T1, T1)).unwrap();
    std::fs::write(
        &path,
        serde_json::to_string(&serde_json::json!([good, {"projectId": 5}])).unwrap(),
    )
    .unwrap();
    assert!(ProjectRegistry::new(&path).list().await.is_empty());
}

#[tokio::test]
async fn registry_records_keep_unknown_keys() {
    let dir = tempfile::tempdir().unwrap();
    let path = workspaces_path(dir.path());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut value = serde_json::to_value(workspace("ws-1")).unwrap();
    value["futureField"] = serde_json::json!({"nested": [1]});
    std::fs::write(
        &path,
        serde_json::to_string_pretty(&serde_json::json!([value])).unwrap(),
    )
    .unwrap();
    let registry = WorkspaceRegistry::new(&path);
    registry
        .update("ws-1", |r| {
            let mut n = r.clone();
            n.title = Some("Titled".into());
            n
        })
        .await
        .unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    let after: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(after[0]["futureField"], serde_json::json!({"nested": [1]}));
    assert_eq!(after[0]["title"], "Titled");
}

#[tokio::test]
async fn workspace_records_create_update_archive_remove() {
    let dir = tempfile::tempdir().unwrap();
    let registry = WorkspaceRegistry::new(workspaces_path(dir.path()));
    registry.initialize().await;
    registry.upsert(workspace("ws-1"), None).await.unwrap();
    let updated = registry
        .update("ws-1", |r| {
            let mut n = r.clone();
            n.title = Some("Payments".into());
            n.updated_at = T2.into();
            n
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(updated.resolved_display_name(), "Payments");
    assert!(registry
        .update("missing", |r| r.clone())
        .await
        .unwrap()
        .is_none());
    registry.archive("ws-1", T3, None).await.unwrap();
    assert_eq!(
        registry.get("ws-1").await.unwrap().archived_at.as_deref(),
        Some(T3)
    );
    registry.remove("ws-1").await.unwrap();
    assert!(registry.list().await.is_empty());
}

#[tokio::test]
async fn repeated_workspace_archive_refreshes_timestamps() {
    let dir = tempfile::tempdir().unwrap();
    let registry = WorkspaceRegistry::new(workspaces_path(dir.path()));
    registry.upsert(workspace("ws-1"), None).await.unwrap();
    registry.archive("ws-1", T2, None).await.unwrap();
    registry.archive("ws-1", T3, None).await.unwrap();
    let rec = registry.get("ws-1").await.unwrap();
    assert_eq!(
        (rec.archived_at.as_deref(), rec.updated_at.as_str()),
        (Some(T3), T3)
    );
    // Archiving an unknown workspace is silent.
    registry.archive("nope", T3, None).await.unwrap();
}

#[tokio::test]
async fn archive_persists_the_consumed_change_request() {
    let dir = tempfile::tempdir().unwrap();
    let registry = WorkspaceRegistry::new(workspaces_path(dir.path()));
    registry.upsert(workspace("ws-auto"), None).await.unwrap();
    registry
        .archive(
            "ws-auto",
            T2,
            Some(WorkspaceArchiveContext {
                auto_archived_change_request_url: Some(
                    "https://github.com/acme/repo/pull/123".into(),
                ),
            }),
        )
        .await
        .unwrap();
    let reloaded = WorkspaceRegistry::new(workspaces_path(dir.path()));
    let rec = reloaded.get("ws-auto").await.unwrap();
    assert_eq!(rec.archived_at.as_deref(), Some(T2));
    assert_eq!(
        rec.auto_archived_change_request_url.as_deref(),
        Some("https://github.com/acme/repo/pull/123")
    );
}

#[tokio::test]
async fn concurrent_workspace_updates_compose() {
    let dir = tempfile::tempdir().unwrap();
    let registry = Arc::new(WorkspaceRegistry::new(workspaces_path(dir.path())));
    registry.upsert(workspace("ws-1"), None).await.unwrap();
    let (a, b) = (registry.clone(), registry.clone());
    let (ra, rb) = tokio::join!(
        a.update("ws-1", |r| {
            let mut n = r.clone();
            n.title = Some("Payments work".into());
            n.updated_at = T2.into();
            n
        }),
        b.update("ws-1", |r| {
            let mut n = r.clone();
            n.pinned_at = Some(T3.into());
            n.updated_at = T3.into();
            n
        })
    );
    ra.unwrap();
    rb.unwrap();
    let reloaded = WorkspaceRegistry::new(workspaces_path(dir.path()));
    let rec = reloaded.get("ws-1").await.unwrap();
    assert_eq!(rec.title.as_deref(), Some("Payments work"));
    assert_eq!(rec.pinned_at.as_deref(), Some(T3));
}

#[tokio::test]
async fn workspace_defaults_and_optional_label_key() {
    let dir = tempfile::tempdir().unwrap();
    let registry = WorkspaceRegistry::new(workspaces_path(dir.path()));
    registry.upsert(workspace("ws-1"), None).await.unwrap();
    let text = std::fs::read_to_string(workspaces_path(dir.path())).unwrap();
    assert!(text.contains("\"background\": false"));
    assert!(!text.contains("\"labels\""), "labels omitted when missing");
    assert!(text.contains("\"archivedAt\": null"));
    let mut with_labels = workspace("ws-2");
    with_labels.labels = Some(vec!["a".into()]);
    with_labels.background = true;
    registry.upsert(with_labels, None).await.unwrap();
    let rec = WorkspaceRegistry::new(workspaces_path(dir.path()))
        .get("ws-2")
        .await
        .unwrap();
    assert_eq!(rec.labels, Some(vec!["a".to_string()]));
    assert!(rec.background);
}

#[tokio::test]
async fn workspace_listener_failure_does_not_fail_the_mutation_and_flag_is_forwarded() {
    let dir = tempfile::tempdir().unwrap();
    let registry = WorkspaceRegistry::new(workspaces_path(dir.path()));
    let (_sub, mut rx) = registry.subscribe_channel();
    registry.subscribe(Arc::new(|_| {
        Box::pin(async { Err(StoreError::other("listener boom")) }) as BoxFuture<_>
    }));
    registry
        .upsert(
            workspace("ws-1"),
            Some(WorkspaceMutationContext {
                expects_initial_agent: true,
            }),
        )
        .await
        .unwrap();
    let mutation = rx.recv().await.unwrap();
    assert_eq!(mutation.kind, MutationKind::Upsert);
    assert!(mutation.expects_initial_agent);
    registry.remove("ws-1").await.unwrap();
    let removed = rx.recv().await.unwrap();
    assert_eq!(
        (removed.kind, removed.workspace),
        (MutationKind::Remove, None)
    );
}

#[tokio::test]
async fn upsert_of_an_identical_record_still_writes_and_notifies() {
    let dir = tempfile::tempdir().unwrap();
    let registry = WorkspaceRegistry::new(workspaces_path(dir.path()));
    let (_sub, mut rx) = registry.subscribe_channel();
    registry.upsert(workspace("ws-1"), None).await.unwrap();
    std::fs::remove_file(workspaces_path(dir.path())).unwrap();
    registry.upsert(workspace("ws-1"), None).await.unwrap();
    assert!(
        workspaces_path(dir.path()).exists(),
        "parity: every upsert writes"
    );
    rx.recv().await.unwrap();
    rx.recv().await.unwrap();
}

#[tokio::test]
async fn failed_write_leaves_the_cache_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let fail = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = fail.clone();
    let writer: paseo_store::registry::WriteRecords<WorkspaceRecord> =
        Arc::new(move |path, records| {
            if flag.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(std::io::Error::other("disk full"));
            }
            write_json_file_atomic(path, &records, FsyncPolicy::Off)
        });
    let registry = WorkspaceRegistry::with_options(
        workspaces_path(dir.path()),
        paseo_store::workspace_registry::WorkspaceRegistryOptions {
            fsync: None,
            write_records: Some(writer),
        },
    );
    registry.upsert(workspace("ws-1"), None).await.unwrap();
    fail.store(true, std::sync::atomic::Ordering::SeqCst);
    let err = registry.upsert(workspace("ws-2"), None).await.unwrap_err();
    assert!(err.to_string().contains("disk full"));
    assert!(registry.get("ws-2").await.is_none());
    assert_eq!(registry.list().await.len(), 1);
}

#[tokio::test]
async fn blocked_registry_rejects_mutations_but_still_reads() {
    let dir = tempfile::tempdir().unwrap();
    let registry = WorkspaceRegistry::new(workspaces_path(dir.path()));
    registry.upsert(workspace("ws-1"), None).await.unwrap();
    registry.block_all_mutations_until_restart();
    let err = registry.upsert(workspace("ws-2"), None).await.unwrap_err();
    assert!(matches!(err, StoreError::RegistryBlocked));
    assert_eq!(registry.list().await.len(), 1);
}

#[tokio::test]
async fn mutate_many_writes_once_and_returns_changed() {
    let dir = tempfile::tempdir().unwrap();
    let registry = WorkspaceRegistry::new(workspaces_path(dir.path()));
    registry.upsert(workspace("ws-1"), None).await.unwrap();
    registry.upsert(workspace("ws-2"), None).await.unwrap();
    let changed = registry
        .mutate_many(|records| {
            records
                .values()
                .map(|r| {
                    let mut n = r.clone();
                    n.pinned_at = Some(T2.into());
                    n
                })
                .collect()
        })
        .await
        .unwrap();
    assert_eq!(changed.len(), 2);
    let none = registry.mutate_many(|_| Vec::new()).await.unwrap();
    assert!(none.is_empty());
    assert!(registry.list().await.iter().all(|r| r.pinned_at.is_some()));
}

#[test]
fn resolve_workspace_name_prefers_title() {
    use paseo_store::workspace_registry::resolve_workspace_name;
    assert_eq!(resolve_workspace_name(Some("T"), "derived"), "T");
    assert_eq!(resolve_workspace_name(None, "derived"), "derived");
    assert_eq!(workspace("w").resolved_display_name(), "main");
}

fn carrier() -> char {
    paseo_agent_types::js_str::lone_high_surrogate(0xD83D).unwrap()
}

fn read_all(dir: &Path) -> String {
    std::fs::read_to_string(dir).unwrap()
}

#[tokio::test]
async fn lone_surrogate_display_names_survive_load_and_rewrite() {
    let dir = tempfile::tempdir().unwrap();
    let title = format!("a{}", carrier());

    let mut p = project("p1", "/tmp/a", T1, T1);
    p.display_name = title.clone();
    let registry = ProjectRegistry::new(projects_path(dir.path()));
    registry.initialize().await;
    registry.upsert(p).await.unwrap();
    assert!(read_all(&projects_path(dir.path())).contains(r"a\ud83d"));
    let reloaded = ProjectRegistry::new(projects_path(dir.path()));
    reloaded.initialize().await;
    assert_eq!(reloaded.get("p1").await.unwrap().display_name, title);
    reloaded
        .upsert(project("p2", "/tmp/b", T1, T1))
        .await
        .unwrap();
    assert!(read_all(&projects_path(dir.path())).contains(r"a\ud83d"));
    assert_eq!(reloaded.list().await.len(), 2);

    let mut w = workspace("w1");
    w.display_name = title.clone();
    let registry = WorkspaceRegistry::new(workspaces_path(dir.path()));
    registry.initialize().await;
    registry.upsert(w, None).await.unwrap();
    assert!(read_all(&workspaces_path(dir.path())).contains(r"a\ud83d"));
    let reloaded = WorkspaceRegistry::new(workspaces_path(dir.path()));
    reloaded.initialize().await;
    assert_eq!(reloaded.get("w1").await.unwrap().display_name, title);
}
