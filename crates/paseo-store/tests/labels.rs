//! Ports of `workspace-labels/index.test.ts`: catalog semantics, change journal, and the
//! compound-commit recovery matrix (prepared/committed journal, lost acknowledgements,
//! uncertain outcomes).

use async_trait::async_trait;
use paseo_fs_atomic::{write_json_file_atomic, FsyncPolicy};
use paseo_store::label_sequence::{
    SequencedChange, SyncMode, WorkspaceLabelChange, WorkspaceLabelCursor, WorkspaceLabelSequence,
};
use paseo_store::label_service::{LabelUpdate, WorkspaceLabelService};
use paseo_store::labels::{
    LabelColor, LabelDefinition, LabelIo, LabelTransaction, TransactionPhase,
    WorkspaceLabelCatalogStore,
};
use paseo_store::registry::{BoxFuture, WriteRecords};
use paseo_store::workspace_registry::{WorkspaceMutation, WorkspaceRegistryOptions};
use paseo_store::{Result, StoreError, WorkspaceKind, WorkspaceRecord, WorkspaceRegistry};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

const T0: &str = "2026-08-14T00:00:00.000Z";

type CatalogFn = Arc<dyn Fn(&Path, &[LabelDefinition]) -> Result<()> + Send + Sync>;
type TransactionFn = Arc<dyn Fn(&Path, &LabelTransaction) -> Result<()> + Send + Sync>;
type RemoveFn = Arc<dyn Fn(&Path) -> Result<()> + Send + Sync>;

#[derive(Default, Clone)]
struct TestIo {
    catalog: Option<CatalogFn>,
    transaction: Option<TransactionFn>,
    remove: Option<RemoveFn>,
}

fn real_catalog(path: &Path, labels: &[LabelDefinition]) -> Result<()> {
    write_json_file_atomic(path, &labels, FsyncPolicy::Off)?;
    Ok(())
}

fn real_transaction(path: &Path, tx: &LabelTransaction) -> Result<()> {
    write_json_file_atomic(path, tx, FsyncPolicy::Off)?;
    Ok(())
}

fn real_remove(path: &Path) -> Result<()> {
    std::fs::remove_file(path)?;
    Ok(())
}

#[async_trait]
impl LabelIo for TestIo {
    async fn write_catalog(&self, path: &Path, labels: &[LabelDefinition]) -> Result<()> {
        match &self.catalog {
            Some(f) => f(path, labels),
            None => real_catalog(path, labels),
        }
    }
    async fn write_transaction(&self, path: &Path, tx: &LabelTransaction) -> Result<()> {
        match &self.transaction {
            Some(f) => f(path, tx),
            None => real_transaction(path, tx),
        }
    }
    async fn remove_transaction(&self, path: &Path) -> Result<()> {
        match &self.remove {
            Some(f) => f(path),
            None => real_remove(path),
        }
    }
}

fn ws(id: &str) -> WorkspaceRecord {
    WorkspaceRecord::new(
        id,
        "prj_one",
        "/repo",
        WorkspaceKind::LocalCheckout,
        "main",
        T0,
        T0,
    )
}

fn lbl(name: &str, color: LabelColor) -> LabelDefinition {
    LabelDefinition::new(name, color)
}

fn registry_at(
    home: &Path,
    file: &str,
    writer: Option<WriteRecords<WorkspaceRecord>>,
) -> Arc<WorkspaceRegistry> {
    Arc::new(WorkspaceRegistry::with_options(
        home.join("projects").join(file),
        WorkspaceRegistryOptions {
            fsync: None,
            write_records: writer,
        },
    ))
}

fn service(
    home: &Path,
    registry: &Arc<WorkspaceRegistry>,
    io: Option<TestIo>,
    journal_limit: Option<usize>,
) -> WorkspaceLabelService {
    let io: Option<Arc<dyn LabelIo>> = io.map(|i| Arc::new(i) as Arc<dyn LabelIo>);
    WorkspaceLabelService::new(
        WorkspaceLabelCatalogStore::new(home, registry.clone(), io),
        journal_limit.map(WorkspaceLabelSequence::new),
    )
}

struct Fx {
    home: tempfile::TempDir,
    registry: Arc<WorkspaceRegistry>,
    labels: WorkspaceLabelService,
}

async fn fx() -> Fx {
    let home = tempfile::tempdir().unwrap();
    let registry = registry_at(home.path(), "workspaces.json", None);
    registry.upsert(ws("wks_one"), None).await.unwrap();
    let labels = service(home.path(), &registry, None, None);
    Fx {
        home,
        registry,
        labels,
    }
}

type Changes = Arc<Mutex<Vec<SequencedChange>>>;

type Listener = Arc<dyn Fn(&SequencedChange) + Send + Sync>;

fn collector() -> (Changes, Listener) {
    let changes: Changes = Arc::default();
    let sink = changes.clone();
    (
        changes,
        Arc::new(move |c| sink.lock().unwrap().push(c.clone())),
    )
}

fn noop() -> Listener {
    Arc::new(|_| {})
}

fn publications(registry: &WorkspaceRegistry) -> Arc<Mutex<Vec<WorkspaceMutation>>> {
    let seen: Arc<Mutex<Vec<WorkspaceMutation>>> = Arc::default();
    let sink = seen.clone();
    registry.subscribe(Arc::new(move |m| {
        sink.lock().unwrap().push(m);
        Box::pin(async { Ok(()) }) as BoxFuture<_>
    }));
    seen
}

async fn assign(
    svc: &WorkspaceLabelService,
    ws: &str,
    name: &str,
    color: LabelColor,
) -> Result<paseo_store::label_service::AssignmentResult> {
    svc.set_assignment(ws, &lbl(name, color), true).await
}

async fn snapshot_labels(svc: &WorkspaceLabelService) -> Vec<LabelDefinition> {
    svc.list(None).await.unwrap().labels
}

fn rename(name: &str, new_name: &str) -> LabelUpdate {
    LabelUpdate {
        name: name.into(),
        new_name: Some(new_name.into()),
        color: None,
    }
}

fn flag() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}

fn on(f: &Arc<AtomicBool>) -> bool {
    f.load(Ordering::SeqCst)
}

fn set(f: &Arc<AtomicBool>, v: bool) {
    f.store(v, Ordering::SeqCst)
}

#[tokio::test]
async fn normalizes_identity_preserves_target_definition_and_stays_silent_on_noop() {
    let fx = fx().await;
    let (updates, listener) = collector();
    let (initial, sub) = fx.labels.subscribe(None, listener).await.unwrap();
    assert!(initial.labels.is_empty());
    assert_eq!(
        (initial.sync.mode, initial.sync.head_seq),
        (SyncMode::Snapshot, 0)
    );

    let assigned = assign(&fx.labels, "wks_one", "  Needs   review ", LabelColor::Sky)
        .await
        .unwrap();
    assert_eq!(assigned.label, lbl("Needs review", LabelColor::Sky));
    assert_eq!(assigned.workspace_labels, ["Needs review"]);
    assert_eq!(updates.lock().unwrap().len(), 1);

    let target_wins = assign(&fx.labels, "wks_one", "needs REVIEW", LabelColor::Red)
        .await
        .unwrap();
    assert_eq!(target_wins.label, lbl("Needs review", LabelColor::Sky));
    assert_eq!(updates.lock().unwrap().len(), 1);

    let reloaded = registry_at(fx.home.path(), "workspaces.json", None);
    assert_eq!(
        reloaded.get("wks_one").await.unwrap().labels,
        Some(vec!["Needs review".to_string()])
    );
    let reloaded_labels = service(fx.home.path(), &reloaded, None, None);
    assert_eq!(
        snapshot_labels(&reloaded_labels).await,
        vec![lbl("Needs review", LabelColor::Sky)]
    );
    sub.unsubscribe();
}

#[tokio::test]
async fn catches_up_one_change_and_falls_back_to_a_snapshot() {
    let fx = fx().await;
    let (initial, sub) = fx.labels.subscribe(None, noop()).await.unwrap();
    assign(&fx.labels, "wks_one", "Blocked", LabelColor::Red)
        .await
        .unwrap();
    sub.unsubscribe();

    let cursor = WorkspaceLabelCursor {
        generation: initial.sync.generation.clone(),
        after_seq: initial.sync.head_seq,
    };
    let (caught_up, s1) = fx.labels.subscribe(Some(&cursor), noop()).await.unwrap();
    assert_eq!(caught_up.labels, vec![lbl("Blocked", LabelColor::Red)]);
    assert_eq!(
        (caught_up.sync.mode, caught_up.sync.head_seq),
        (SyncMode::Changes, 1)
    );

    let cursor = WorkspaceLabelCursor {
        generation: caught_up.sync.generation.clone(),
        after_seq: caught_up.sync.head_seq,
    };
    let (current, s2) = fx.labels.subscribe(Some(&cursor), noop()).await.unwrap();
    assert!(current.labels.is_empty() && current.sync.removals.is_empty());
    assert_eq!(
        (current.sync.mode, current.sync.head_seq),
        (SyncMode::Changes, 1)
    );
    s1.unsubscribe();
    s2.unsubscribe();

    let expired = WorkspaceLabelCursor {
        generation: "expired".into(),
        after_seq: 1,
    };
    let (snapshot, s3) = fx.labels.subscribe(Some(&expired), noop()).await.unwrap();
    assert_eq!(snapshot.labels, vec![lbl("Blocked", LabelColor::Red)]);
    assert_eq!(
        (snapshot.sync.mode, snapshot.sync.head_seq),
        (SyncMode::Snapshot, 1)
    );
    s3.unsubscribe();
}

#[tokio::test]
async fn publishes_only_to_active_subscribers_and_never_for_noops() {
    let fx = fx().await;
    let (updates, listener) = collector();
    let (_, sub) = fx.labels.subscribe(None, listener).await.unwrap();
    tokio::task::yield_now().await;
    assert!(updates.lock().unwrap().is_empty());
    fx.labels
        .set_assignment("wks_one", &lbl("Quiet", LabelColor::Sky), false)
        .await
        .unwrap();
    assert!(updates.lock().unwrap().is_empty());
    sub.unsubscribe();
    assign(&fx.labels, "wks_one", "Quiet", LabelColor::Sky)
        .await
        .unwrap();
    assert!(updates.lock().unwrap().is_empty());
}

#[tokio::test]
async fn compacts_name_cycles_and_chained_edit_delete() {
    let fx = fx().await;
    assign(&fx.labels, "wks_one", "A", LabelColor::Red)
        .await
        .unwrap();
    let (checkpoint, c0) = fx.labels.subscribe(None, noop()).await.unwrap();
    fx.labels.update(rename("A", "B")).await.unwrap();
    fx.labels.update(rename("B", "A")).await.unwrap();
    let cursor = WorkspaceLabelCursor {
        generation: checkpoint.sync.generation.clone(),
        after_seq: checkpoint.sync.head_seq,
    };
    let (cycle, c1) = fx.labels.subscribe(Some(&cursor), noop()).await.unwrap();
    assert_eq!(cycle.labels, vec![lbl("A", LabelColor::Red)]);
    assert_eq!(cycle.sync.mode, SyncMode::Changes);
    assert!(cycle.sync.removals.is_empty());

    fx.labels.update(rename("A", "B")).await.unwrap();
    fx.labels.update(rename("B", "C")).await.unwrap();
    fx.labels.delete("C").await.unwrap();
    let cursor = WorkspaceLabelCursor {
        generation: cycle.sync.generation.clone(),
        after_seq: cycle.sync.head_seq,
    };
    let (deleted, c2) = fx.labels.subscribe(Some(&cursor), noop()).await.unwrap();
    assert!(deleted.labels.is_empty());
    assert_eq!(deleted.sync.mode, SyncMode::Changes);
    assert_eq!(deleted.sync.removals.len(), 1);
    assert_eq!(deleted.sync.removals[0].name, "A");
    for s in [c0, c1, c2] {
        s.unsubscribe();
    }
}

#[tokio::test]
async fn edits_and_deletes_one_catalog_while_rewriting_assignments() {
    let fx = fx().await;
    assign(&fx.labels, "wks_one", "Blocked", LabelColor::Red)
        .await
        .unwrap();
    let renamed = fx
        .labels
        .update(rename("blocked", "Waiting"))
        .await
        .unwrap();
    assert_eq!(renamed.label, lbl("Waiting", LabelColor::Red));
    assert_eq!(renamed.affected_workspace_count, 1);
    let err = fx.labels.update(rename("waiting", "")).await.unwrap_err();
    assert_eq!(err.code(), Some("label_name_empty"));
    let recolored = fx
        .labels
        .update(LabelUpdate {
            name: "WAITING".into(),
            new_name: None,
            color: Some(LabelColor::Amber),
        })
        .await
        .unwrap();
    assert_eq!(
        (recolored.label, recolored.affected_workspace_count),
        (lbl("Waiting", LabelColor::Amber), 0)
    );
    assert_eq!(fx.labels.delete("waiting").await.unwrap(), 1);
    assert_eq!(fx.registry.get("wks_one").await.unwrap().labels, None);
}

#[tokio::test]
async fn name_and_colour_apply_in_one_commit_or_neither() {
    let fx = fx().await;
    let (changes, listener) = collector();
    let (_, sub) = fx.labels.subscribe(None, listener).await.unwrap();
    assign(&fx.labels, "wks_one", "Blocked", LabelColor::Red)
        .await
        .unwrap();
    assign(&fx.labels, "wks_one", "Waiting", LabelColor::Amber)
        .await
        .unwrap();
    changes.lock().unwrap().clear();

    let r = fx
        .labels
        .update(LabelUpdate {
            name: "blocked".into(),
            new_name: Some("Urgent".into()),
            color: Some(LabelColor::Sky),
        })
        .await
        .unwrap();
    assert_eq!(
        (r.label, r.affected_workspace_count),
        (lbl("Urgent", LabelColor::Sky), 1)
    );
    {
        let seen = changes.lock().unwrap();
        assert_eq!(seen.len(), 1, "one publication for one edit");
        assert_eq!(
            seen[0].change,
            WorkspaceLabelChange::Upsert {
                label: lbl("Urgent", LabelColor::Sky),
                previous_name: Some("Blocked".into())
            }
        );
    }
    assert_eq!(
        fx.registry.get("wks_one").await.unwrap().labels,
        Some(vec!["Urgent".to_string(), "Waiting".to_string()])
    );

    let err = fx
        .labels
        .update(LabelUpdate {
            name: "Urgent".into(),
            new_name: Some("waiting".into()),
            color: Some(LabelColor::Teal),
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), Some("label_name_taken"));
    assert_eq!(changes.lock().unwrap().len(), 1);
    assert_eq!(
        snapshot_labels(&fx.labels).await,
        vec![
            lbl("Urgent", LabelColor::Sky),
            lbl("Waiting", LabelColor::Amber)
        ]
    );

    let case_only = fx.labels.update(rename("Urgent", "URGENT")).await.unwrap();
    assert_eq!(
        (case_only.label, case_only.affected_workspace_count),
        (lbl("URGENT", LabelColor::Sky), 1)
    );
    assert_eq!(
        fx.registry.get("wks_one").await.unwrap().labels,
        Some(vec!["URGENT".to_string(), "Waiting".to_string()])
    );

    changes.lock().unwrap().clear();
    let nothing = fx
        .labels
        .update(LabelUpdate {
            name: "urgent".into(),
            new_name: Some("URGENT".into()),
            color: Some(LabelColor::Sky),
        })
        .await
        .unwrap();
    assert_eq!(
        (nothing.label, nothing.affected_workspace_count),
        (lbl("URGENT", LabelColor::Sky), 0)
    );
    assert!(changes.lock().unwrap().is_empty());
    sub.unsubscribe();
}

#[tokio::test]
async fn counts_the_same_active_and_archived_records_delete_rewrites() {
    let fx = fx().await;
    assign(&fx.labels, "wks_one", "Blocked", LabelColor::Red)
        .await
        .unwrap();
    fx.registry
        .archive("wks_one", "2026-08-14T01:00:00.000Z", None)
        .await
        .unwrap();
    assert_eq!(
        fx.labels
            .count_affected_workspaces("blocked")
            .await
            .unwrap(),
        1
    );
    assert_eq!(fx.labels.delete("Blocked").await.unwrap(), 1);
    assert_eq!(fx.registry.get("wks_one").await.unwrap().labels, None);
}

#[tokio::test]
async fn rejects_collisions_unknown_workspaces_and_archived_assignment() {
    let fx = fx().await;
    assign(&fx.labels, "wks_one", "Blocked", LabelColor::Red)
        .await
        .unwrap();
    assign(&fx.labels, "wks_one", "Waiting", LabelColor::Amber)
        .await
        .unwrap();
    let err = fx
        .labels
        .update(rename("Blocked", "waiting"))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Some("label_name_taken"));
    assert_eq!(
        fx.registry.get("wks_one").await.unwrap().labels,
        Some(vec!["Blocked".to_string(), "Waiting".to_string()])
    );
    assert_eq!(
        snapshot_labels(&fx.labels).await,
        vec![
            lbl("Blocked", LabelColor::Red),
            lbl("Waiting", LabelColor::Amber)
        ]
    );
    assert_eq!(
        fx.labels
            .update(rename("Nope", "X"))
            .await
            .unwrap_err()
            .code(),
        Some("label_not_found")
    );
    assert_eq!(
        assign(&fx.labels, "missing", "X", LabelColor::Red)
            .await
            .unwrap_err()
            .code(),
        Some("workspace_not_found")
    );
    fx.registry
        .archive("wks_one", "2026-08-14T02:00:00.000Z", None)
        .await
        .unwrap();
    assert_eq!(
        assign(&fx.labels, "wks_one", "X", LabelColor::Red)
            .await
            .unwrap_err()
            .code(),
        Some("workspace_not_found")
    );
    assert_eq!(
        assign(&fx.labels, "wks_one", "   ", LabelColor::Red)
            .await
            .unwrap_err()
            .code(),
        Some("label_name_empty")
    );
}

#[tokio::test]
async fn unassigning_the_last_label_removes_the_key_and_keeps_the_catalog_entry() {
    let fx = fx().await;
    assign(&fx.labels, "wks_one", "Solo", LabelColor::Teal)
        .await
        .unwrap();
    let r = fx
        .labels
        .set_assignment("wks_one", &lbl("solo", LabelColor::Red), false)
        .await
        .unwrap();
    assert!(r.workspace_labels.is_empty());
    assert_eq!(fx.registry.get("wks_one").await.unwrap().labels, None);
    assert_eq!(
        snapshot_labels(&fx.labels).await,
        vec![lbl("Solo", LabelColor::Teal)]
    );
}

#[tokio::test]
async fn blocks_when_assignment_persistence_and_rollback_both_fail() {
    let home = tempfile::tempdir().unwrap();
    let fail = flag();
    let f = fail.clone();
    let writer: WriteRecords<WorkspaceRecord> = Arc::new(move |path, records| {
        if on(&f) {
            return Err(std::io::Error::other("disk full"));
        }
        write_json_file_atomic(path, &records, FsyncPolicy::Off)
    });
    let registry = registry_at(home.path(), "failing-workspaces.json", Some(writer));
    registry.upsert(ws("wks_failing"), None).await.unwrap();
    let labels = service(&home.path().join("failing"), &registry, None, None);
    set(&fail, true);

    let err = assign(&labels, "wks_failing", "Urgent", LabelColor::Red)
        .await
        .unwrap_err();
    assert_eq!(err.code(), Some("workspace_label_storage_uncertain"));
    let (snapshot, sub) = labels.subscribe(None, noop()).await.unwrap();
    assert!(snapshot.labels.is_empty());
    assert_eq!(snapshot.sync.head_seq, 0);
    assert_eq!(registry.get("wks_failing").await.unwrap().labels, None);
    sub.unsubscribe();
}

#[tokio::test]
async fn restores_workspace_state_when_catalog_persistence_fails() {
    let fx = fx().await;
    let fail = Arc::new(AtomicBool::new(true));
    let f = fail.clone();
    let io = TestIo {
        catalog: Some(Arc::new(move |path, defs| {
            if on(&f) && !defs.is_empty() {
                return Err(StoreError::other("catalog disk full"));
            }
            real_catalog(path, defs)
        })),
        ..Default::default()
    };
    let failing = service(
        &fx.home.path().join("catalog-failure"),
        &fx.registry,
        Some(io),
        None,
    );
    let err = assign(&failing, "wks_one", "Urgent", LabelColor::Red)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("catalog disk full"));
    assert_eq!(fx.registry.get("wks_one").await.unwrap().labels, None);
    let (snap, sub) = failing.subscribe(None, noop()).await.unwrap();
    assert!(snap.labels.is_empty() && snap.sync.head_seq == 0);
    set(&fail, false);
    assign(&failing, "wks_one", "Urgent", LabelColor::Red)
        .await
        .unwrap();
    assert_eq!(
        fx.registry.get("wks_one").await.unwrap().labels,
        Some(vec!["Urgent".to_string()])
    );
    sub.unsubscribe();
}

#[tokio::test]
async fn restores_rename_and_delete_rewrites_when_catalog_persistence_fails() {
    let fx = fx().await;
    let mode: Arc<Mutex<&'static str>> = Arc::new(Mutex::new(""));
    let m = mode.clone();
    let io = TestIo {
        catalog: Some(Arc::new(move |path, defs| {
            let mode = *m.lock().unwrap();
            if (mode == "priority" && defs.iter().any(|l| l.name == "Priority"))
                || (mode == "empty" && defs.is_empty())
            {
                return Err(StoreError::other("catalog unavailable"));
            }
            real_catalog(path, defs)
        })),
        ..Default::default()
    };
    let svc = service(
        &fx.home.path().join("rewrite-catalog-failure"),
        &fx.registry,
        Some(io),
        None,
    );
    assign(&svc, "wks_one", "Urgent", LabelColor::Red)
        .await
        .unwrap();
    *mode.lock().unwrap() = "priority";
    assert!(svc
        .update(rename("Urgent", "Priority"))
        .await
        .unwrap_err()
        .to_string()
        .contains("catalog unavailable"));
    assert_eq!(
        fx.registry.get("wks_one").await.unwrap().labels,
        Some(vec!["Urgent".to_string()])
    );
    *mode.lock().unwrap() = "empty";
    assert!(svc
        .delete("Urgent")
        .await
        .unwrap_err()
        .to_string()
        .contains("catalog unavailable"));
    assert_eq!(
        fx.registry.get("wks_one").await.unwrap().labels,
        Some(vec!["Urgent".to_string()])
    );
    let (snap, sub) = svc.subscribe(None, noop()).await.unwrap();
    assert_eq!(snap.labels, vec![lbl("Urgent", LabelColor::Red)]);
    assert_eq!(snap.sync.head_seq, 1);
    sub.unsubscribe();
}

#[tokio::test]
async fn rolls_back_an_interrupted_compound_rename_after_restart() {
    let home = tempfile::tempdir().unwrap();
    let writes = Arc::new(Mutex::new(0u32));
    let interrupt = flag();
    let (w, i) = (writes.clone(), interrupt.clone());
    let writer: WriteRecords<WorkspaceRecord> = Arc::new(move |path, records| {
        *w.lock().unwrap() += 1;
        if on(&i) && *w.lock().unwrap() > 2 {
            return Err(std::io::Error::other("process interrupted"));
        }
        write_json_file_atomic(path, &records, FsyncPolicy::Off)
    });
    let registry = registry_at(home.path(), "interrupted-workspaces.json", Some(writer));
    registry.upsert(ws("wks_interrupted"), None).await.unwrap();
    let svc = service(home.path(), &registry, None, None);
    assign(&svc, "wks_interrupted", "Urgent", LabelColor::Red)
        .await
        .unwrap();
    let workspace_pubs = publications(&registry);
    let (catalog_pubs, listener) = collector();
    let (_, sub) = svc.subscribe(None, listener).await.unwrap();
    set(&interrupt, true);

    assert!(svc.update(rename("Urgent", "Priority")).await.is_err());
    assert!(workspace_pubs.lock().unwrap().is_empty());
    assert!(catalog_pubs.lock().unwrap().is_empty());

    let recovered_registry = registry_at(home.path(), "interrupted-workspaces.json", None);
    let recovered = service(home.path(), &recovered_registry, None, None);
    recovered.initialize().await.unwrap();
    assert_eq!(
        recovered_registry
            .get("wks_interrupted")
            .await
            .unwrap()
            .labels,
        Some(vec!["Urgent".to_string()])
    );
    assert_eq!(
        snapshot_labels(&recovered).await,
        vec![lbl("Urgent", LabelColor::Red)]
    );
    sub.unsubscribe();
}

#[tokio::test]
async fn aborts_a_pre_commit_workspace_write_failure_without_publishing() {
    let home = tempfile::tempdir().unwrap();
    let fail_next = flag();
    let f = fail_next.clone();
    let writer: WriteRecords<WorkspaceRecord> = Arc::new(move |path, records| {
        if f.swap(false, Ordering::SeqCst) {
            return Err(std::io::Error::other("one interrupted write"));
        }
        write_json_file_atomic(path, &records, FsyncPolicy::Off)
    });
    let registry = registry_at(home.path(), "recovering-workspaces.json", Some(writer));
    registry.upsert(ws("wks_recovering"), None).await.unwrap();
    let svc = service(&home.path().join("recovering"), &registry, None, None);
    assign(&svc, "wks_recovering", "Urgent", LabelColor::Red)
        .await
        .unwrap();
    let workspace_pubs = publications(&registry);
    let (catalog_pubs, listener) = collector();
    let (_, sub) = svc.subscribe(None, listener).await.unwrap();

    set(&fail_next, true);
    let err = svc.update(rename("Urgent", "Priority")).await.unwrap_err();
    assert!(err.to_string().contains("one interrupted write"));
    assert_eq!(
        registry.get("wks_recovering").await.unwrap().labels,
        Some(vec!["Urgent".to_string()])
    );
    assert!(workspace_pubs.lock().unwrap().is_empty());
    assert!(catalog_pubs.lock().unwrap().is_empty());
    sub.unsubscribe();
}

fn phase_failing_io(phase: TransactionPhase, flag: Arc<AtomicBool>, write_first: bool) -> TestIo {
    TestIo {
        transaction: Some(Arc::new(move |path, tx| {
            if write_first {
                real_transaction(path, tx)?;
                if on(&flag) && tx.phase == phase {
                    return Err(StoreError::other("lost acknowledgement"));
                }
                return Ok(());
            }
            if on(&flag) && tx.phase == phase {
                return Err(StoreError::other("commit marker unavailable"));
            }
            real_transaction(path, tx)
        })),
        ..Default::default()
    }
}

#[tokio::test]
async fn journal_phase_rewrite_is_the_commit_point() {
    let fx = fx().await;
    let fail = flag();
    let svc = service(
        &fx.home.path().join("commit-marker"),
        &fx.registry,
        Some(phase_failing_io(
            TransactionPhase::Committed,
            fail.clone(),
            false,
        )),
        None,
    );
    assign(&svc, "wks_one", "Urgent", LabelColor::Red)
        .await
        .unwrap();
    let workspace_pubs = publications(&fx.registry);
    let (catalog_pubs, listener) = collector();
    let (_, sub) = svc.subscribe(None, listener).await.unwrap();

    set(&fail, true);
    let err = svc.update(rename("Urgent", "Priority")).await.unwrap_err();
    assert!(err.to_string().contains("commit marker unavailable"));
    assert_eq!(
        fx.registry.get("wks_one").await.unwrap().labels,
        Some(vec!["Urgent".to_string()])
    );
    assert!(workspace_pubs.lock().unwrap().is_empty() && catalog_pubs.lock().unwrap().is_empty());

    let restarted_registry = registry_at(fx.home.path(), "workspaces.json", None);
    let restarted = service(
        &fx.home.path().join("commit-marker"),
        &restarted_registry,
        None,
        None,
    );
    restarted.initialize().await.unwrap();
    assert_eq!(
        restarted_registry.get("wks_one").await.unwrap().labels,
        Some(vec!["Urgent".to_string()])
    );
    assert_eq!(
        snapshot_labels(&restarted).await,
        vec![lbl("Urgent", LabelColor::Red)]
    );
    sub.unsubscribe();
}

#[tokio::test]
async fn prepared_journal_failure_leaves_no_replayable_intent() {
    let fx = fx().await;
    let workspace_pubs = publications(&fx.registry);
    let io = TestIo {
        transaction: Some(Arc::new(|_, _| {
            Err(StoreError::other("journal unavailable"))
        })),
        ..Default::default()
    };
    let svc = service(
        &fx.home.path().join("prepared-write"),
        &fx.registry,
        Some(io),
        None,
    );
    let (catalog_pubs, listener) = collector();
    let (_, sub) = svc.subscribe(None, listener).await.unwrap();
    let err = assign(&svc, "wks_one", "Urgent", LabelColor::Red)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("journal unavailable"));
    assert_eq!(fx.registry.get("wks_one").await.unwrap().labels, None);
    assert!(workspace_pubs.lock().unwrap().is_empty() && catalog_pubs.lock().unwrap().is_empty());

    let restarted_registry = registry_at(fx.home.path(), "workspaces.json", None);
    let restarted = service(
        &fx.home.path().join("prepared-write"),
        &restarted_registry,
        None,
        None,
    );
    restarted.initialize().await.unwrap();
    assert_eq!(
        restarted_registry.get("wks_one").await.unwrap().labels,
        None
    );
    assert!(snapshot_labels(&restarted).await.is_empty());
    sub.unsubscribe();
}

#[tokio::test]
async fn rolls_back_a_durable_prepared_marker_whose_acknowledgement_is_lost() {
    let fx = fx().await;
    let lose = flag();
    let svc = service(
        &fx.home.path().join("prepared-lost-ack"),
        &fx.registry,
        Some(phase_failing_io(
            TransactionPhase::Prepared,
            lose.clone(),
            true,
        )),
        None,
    );
    assign(&svc, "wks_one", "Urgent", LabelColor::Red)
        .await
        .unwrap();
    let workspace_pubs = publications(&fx.registry);
    let (catalog_pubs, listener) = collector();
    let (_, sub) = svc.subscribe(None, listener).await.unwrap();

    set(&lose, true);
    let err = svc.update(rename("Urgent", "Priority")).await.unwrap_err();
    assert!(err.to_string().contains("lost acknowledgement"));
    assert_eq!(
        fx.registry.get("wks_one").await.unwrap().labels,
        Some(vec!["Urgent".to_string()])
    );
    assert!(workspace_pubs.lock().unwrap().is_empty() && catalog_pubs.lock().unwrap().is_empty());

    set(&lose, false);
    let archived_at = "2099-01-01T00:00:00.000Z";
    fx.registry
        .archive("wks_one", archived_at, None)
        .await
        .unwrap();
    assert_eq!(workspace_pubs.lock().unwrap().len(), 1);

    let restarted_registry = registry_at(fx.home.path(), "workspaces.json", None);
    let restarted = service(
        &fx.home.path().join("prepared-lost-ack"),
        &restarted_registry,
        None,
        None,
    );
    restarted.initialize().await.unwrap();
    let rec = restarted_registry.get("wks_one").await.unwrap();
    assert_eq!(rec.labels, Some(vec!["Urgent".to_string()]));
    assert_eq!(
        (rec.archived_at.as_deref(), rec.updated_at.as_str()),
        (Some(archived_at), archived_at)
    );
    assert_eq!(
        snapshot_labels(&restarted).await,
        vec![lbl("Urgent", LabelColor::Red)]
    );
    sub.unsubscribe();
}

#[tokio::test]
async fn rolls_back_a_durable_catalog_write_whose_acknowledgement_is_lost() {
    let fx = fx().await;
    let lose = flag();
    let l = lose.clone();
    let io = TestIo {
        catalog: Some(Arc::new(move |path, defs| {
            real_catalog(path, defs)?;
            if on(&l) && defs.iter().any(|d| d.name == "Priority") {
                return Err(StoreError::other("lost catalog acknowledgement"));
            }
            Ok(())
        })),
        ..Default::default()
    };
    let svc = service(
        &fx.home.path().join("catalog-lost-ack"),
        &fx.registry,
        Some(io),
        None,
    );
    assign(&svc, "wks_one", "Urgent", LabelColor::Red)
        .await
        .unwrap();
    set(&lose, true);
    let err = svc.update(rename("Urgent", "Priority")).await.unwrap_err();
    assert!(err.to_string().contains("lost catalog acknowledgement"));
    assert_eq!(
        fx.registry.get("wks_one").await.unwrap().labels,
        Some(vec!["Urgent".to_string()])
    );
    // The rolled-back catalog is what a restart sees too.
    set(&lose, false);
    let restarted_registry = registry_at(fx.home.path(), "workspaces.json", None);
    let restarted = service(
        &fx.home.path().join("catalog-lost-ack"),
        &restarted_registry,
        None,
        None,
    );
    assert_eq!(
        snapshot_labels(&restarted).await,
        vec![lbl("Urgent", LabelColor::Red)]
    );
    // And later registry writes are allowed (the rollback succeeded).
    fx.registry
        .archive("wks_one", "2099-01-02T00:00:00.000Z", None)
        .await
        .unwrap();
}

#[tokio::test]
async fn rolls_back_a_durable_workspace_write_whose_acknowledgement_is_lost() {
    let home = tempfile::tempdir().unwrap();
    let lose = flag();
    let l = lose.clone();
    let writer: WriteRecords<WorkspaceRecord> = Arc::new(move |path, records| {
        write_json_file_atomic(path, &records, FsyncPolicy::Off)?;
        if l.swap(false, Ordering::SeqCst) {
            return Err(std::io::Error::other("lost workspace acknowledgement"));
        }
        Ok(())
    });
    let registry = registry_at(home.path(), "lost-ack-workspaces.json", Some(writer));
    registry.upsert(ws("wks_lost_ack"), None).await.unwrap();
    let svc = service(
        &home.path().join("workspace-lost-ack"),
        &registry,
        None,
        None,
    );
    assign(&svc, "wks_lost_ack", "Urgent", LabelColor::Red)
        .await
        .unwrap();
    let workspace_pubs = publications(&registry);
    let (catalog_pubs, listener) = collector();
    let (_, sub) = svc.subscribe(None, listener).await.unwrap();

    set(&lose, true);
    let err = svc.update(rename("Urgent", "Priority")).await.unwrap_err();
    assert!(err.to_string().contains("lost workspace acknowledgement"));
    assert_eq!(
        registry.get("wks_lost_ack").await.unwrap().labels,
        Some(vec!["Urgent".to_string()])
    );
    assert!(workspace_pubs.lock().unwrap().is_empty() && catalog_pubs.lock().unwrap().is_empty());

    let archived_at = "2099-01-03T00:00:00.000Z";
    registry
        .archive("wks_lost_ack", archived_at, None)
        .await
        .unwrap();
    let restarted_registry = registry_at(home.path(), "lost-ack-workspaces.json", None);
    let restarted = service(
        &home.path().join("workspace-lost-ack"),
        &restarted_registry,
        None,
        None,
    );
    restarted.initialize().await.unwrap();
    let rec = restarted_registry.get("wks_lost_ack").await.unwrap();
    assert_eq!(
        (rec.labels, rec.archived_at.as_deref()),
        (Some(vec!["Urgent".to_string()]), Some(archived_at))
    );
    assert_eq!(
        snapshot_labels(&restarted).await,
        vec![lbl("Urgent", LabelColor::Red)]
    );
    sub.unsubscribe();
}

#[tokio::test]
async fn reports_an_uncertain_outcome_when_the_committed_marker_is_durable_before_its_write_rejects(
) {
    let fx = fx().await;
    let reject = flag();
    let svc = service(
        &fx.home.path().join("uncertain-commit-marker"),
        &fx.registry,
        Some(phase_failing_io(
            TransactionPhase::Committed,
            reject.clone(),
            true,
        )),
        None,
    );
    assign(&svc, "wks_one", "Urgent", LabelColor::Red)
        .await
        .unwrap();
    let workspace_pubs = publications(&fx.registry);
    let (catalog_pubs, listener) = collector();
    let (_, sub) = svc.subscribe(None, listener).await.unwrap();

    set(&reject, true);
    let err = svc.update(rename("Urgent", "Priority")).await.unwrap_err();
    assert_eq!(err.code(), Some("workspace_label_storage_uncertain"));
    assert_eq!(
        fx.registry.get("wks_one").await.unwrap().labels,
        Some(vec!["Urgent".to_string()])
    );
    assert!(workspace_pubs.lock().unwrap().is_empty() && catalog_pubs.lock().unwrap().is_empty());
    set(&reject, false);
    let blocked = fx
        .registry
        .archive("wks_one", "2099-01-04T00:00:00.000Z", None)
        .await
        .unwrap_err();
    assert!(blocked.to_string().contains("blocked until daemon restart"));
    let again = svc
        .update(LabelUpdate {
            name: "Priority".into(),
            new_name: None,
            color: Some(LabelColor::Sky),
        })
        .await
        .unwrap_err();
    assert_eq!(again.code(), Some("workspace_label_storage_uncertain"));
    assert!(workspace_pubs.lock().unwrap().is_empty() && catalog_pubs.lock().unwrap().is_empty());

    let restarted_registry = registry_at(fx.home.path(), "workspaces.json", None);
    let restarted = service(
        &fx.home.path().join("uncertain-commit-marker"),
        &restarted_registry,
        None,
        None,
    );
    restarted.initialize().await.unwrap();
    let rec = restarted_registry.get("wks_one").await.unwrap();
    assert_eq!(rec.labels, Some(vec!["Priority".to_string()]));
    assert_eq!(rec.archived_at, None);
    assert_eq!(
        snapshot_labels(&restarted).await,
        vec![lbl("Priority", LabelColor::Red)]
    );
    sub.unsubscribe();
}

#[tokio::test]
async fn keeps_a_rejected_mutation_aborted_when_prepared_journal_cleanup_also_fails() {
    let fx = fx().await;
    let fail_after_catalog = flag();
    let f = fail_after_catalog.clone();
    let io = TestIo {
        catalog: Some(Arc::new(move |path, defs| {
            if on(&f) && defs.iter().any(|l| l.name == "Priority") {
                return Err(StoreError::other("catalog unavailable"));
            }
            real_catalog(path, defs)
        })),
        remove: Some(Arc::new(|_| {
            Err(StoreError::other("journal cleanup unavailable"))
        })),
        ..Default::default()
    };
    let svc = service(
        &fx.home.path().join("prepared-cleanup"),
        &fx.registry,
        Some(io),
        None,
    );
    assign(&svc, "wks_one", "Urgent", LabelColor::Red)
        .await
        .unwrap();
    set(&fail_after_catalog, true);
    let err = svc.update(rename("Urgent", "Priority")).await.unwrap_err();
    assert_eq!(err.code(), Some("workspace_label_storage_uncertain"));
    assert_eq!(
        fx.registry.get("wks_one").await.unwrap().labels,
        Some(vec!["Urgent".to_string()])
    );

    let restarted_registry = registry_at(fx.home.path(), "workspaces.json", None);
    let restarted = service(
        &fx.home.path().join("prepared-cleanup"),
        &restarted_registry,
        None,
        None,
    );
    restarted.initialize().await.unwrap();
    assert_eq!(
        restarted_registry.get("wks_one").await.unwrap().labels,
        Some(vec!["Urgent".to_string()])
    );
    assert_eq!(
        snapshot_labels(&restarted).await,
        vec![lbl("Urgent", LabelColor::Red)]
    );
}

#[tokio::test]
async fn reports_success_after_the_commit_point_even_when_committed_cleanup_fails() {
    let fx = fx().await;
    let fail_cleanup = flag();
    let f = fail_cleanup.clone();
    let io = TestIo {
        remove: Some(Arc::new(move |path| {
            if on(&f) {
                return Err(StoreError::other("journal cleanup unavailable"));
            }
            real_remove(path)
        })),
        ..Default::default()
    };
    let svc = service(
        &fx.home.path().join("committed-cleanup"),
        &fx.registry,
        Some(io),
        None,
    );
    assign(&svc, "wks_one", "Urgent", LabelColor::Red)
        .await
        .unwrap();
    set(&fail_cleanup, true);
    let result = svc.update(rename("Urgent", "Priority")).await.unwrap();
    assert_eq!(
        (result.label, result.affected_workspace_count),
        (lbl("Priority", LabelColor::Red), 1)
    );
    let archived_at = "2099-01-01T00:00:00.000Z";
    fx.registry
        .archive("wks_one", archived_at, None)
        .await
        .unwrap();

    for _ in 0..2 {
        let always_failing = TestIo {
            remove: Some(Arc::new(|_| {
                Err(StoreError::other("journal cleanup still unavailable"))
            })),
            ..Default::default()
        };
        let restarted_registry = registry_at(fx.home.path(), "workspaces.json", None);
        let restarted = service(
            &fx.home.path().join("committed-cleanup"),
            &restarted_registry,
            Some(always_failing),
            None,
        );
        restarted.initialize().await.unwrap();
        let rec = restarted_registry.get("wks_one").await.unwrap();
        assert_eq!(rec.labels, Some(vec!["Priority".to_string()]));
        assert_eq!(
            (rec.archived_at.as_deref(), rec.updated_at.as_str()),
            (Some(archived_at), archived_at)
        );
        assert_eq!(
            snapshot_labels(&restarted).await,
            vec![lbl("Priority", LabelColor::Red)]
        );
    }
}

#[tokio::test]
async fn listener_and_subscriber_failures_do_not_reject_durable_mutations() {
    let fx = fx().await;
    fx.registry.subscribe(Arc::new(|_| {
        Box::pin(async { Err(StoreError::other("subscriber failed")) }) as BoxFuture<_>
    }));
    let (_, sub) = fx
        .labels
        .subscribe(None, Arc::new(|_| panic!("subscriber failed")))
        .await
        .unwrap();
    let r = assign(&fx.labels, "wks_one", "Urgent", LabelColor::Red)
        .await
        .unwrap();
    assert_eq!(r.workspace_labels, ["Urgent"]);
    assert_eq!(
        fx.registry.get("wks_one").await.unwrap().labels,
        Some(vec!["Urgent".to_string()])
    );
    sub.unsubscribe();
}

#[tokio::test]
async fn preserves_concurrent_workspace_fields_while_assigning() {
    let fx = fx().await;
    let (a, b) = tokio::join!(
        assign(&fx.labels, "wks_one", "Urgent", LabelColor::Red),
        fx.registry.update("wks_one", |w| {
            let mut n = w.clone();
            n.title = Some("Concurrent title".into());
            n
        })
    );
    a.unwrap();
    b.unwrap();
    let rec = fx.registry.get("wks_one").await.unwrap();
    assert_eq!(rec.title.as_deref(), Some("Concurrent title"));
    assert_eq!(rec.labels, Some(vec!["Urgent".to_string()]));
}

#[tokio::test]
async fn bounded_journal_expires_to_a_coherent_snapshot() {
    let fx = fx().await;
    let bounded = service(&fx.home.path().join("bounded"), &fx.registry, None, Some(1));
    let (checkpoint, c) = bounded.subscribe(None, noop()).await.unwrap();
    assign(&bounded, "wks_one", "One", LabelColor::Red)
        .await
        .unwrap();
    assign(&bounded, "wks_one", "Two", LabelColor::Sky)
        .await
        .unwrap();
    let cursor = WorkspaceLabelCursor {
        generation: checkpoint.sync.generation.clone(),
        after_seq: checkpoint.sync.head_seq,
    };
    let (expired, s) = bounded.subscribe(Some(&cursor), noop()).await.unwrap();
    assert_eq!(
        expired.labels,
        vec![lbl("One", LabelColor::Red), lbl("Two", LabelColor::Sky)]
    );
    assert_eq!(
        (expired.sync.mode, expired.sync.head_seq),
        (SyncMode::Snapshot, 2)
    );
    s.unsubscribe();
    c.unsubscribe();
}

#[tokio::test]
async fn catalog_file_is_json_array_and_label_extras_survive() {
    let fx = fx().await;
    assign(&fx.labels, "wks_one", "Keep", LabelColor::Pink)
        .await
        .unwrap();
    let path = fx.home.path().join("projects/workspace-labels.json");
    let mut value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
        value,
        serde_json::json!([{"name": "Keep", "color": "pink"}])
    );
    value[0]["futureField"] = serde_json::json!(1);
    std::fs::write(&path, serde_json::to_string_pretty(&value).unwrap()).unwrap();

    let registry = registry_at(fx.home.path(), "workspaces.json", None);
    let svc = service(fx.home.path(), &registry, None, None);
    svc.update(LabelUpdate {
        name: "keep".into(),
        new_name: None,
        color: Some(LabelColor::Teal),
    })
    .await
    .unwrap();
    let after: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
        after,
        serde_json::json!([{"name": "Keep", "color": "teal", "futureField": 1}])
    );
    assert!(!fx
        .home
        .path()
        .join("projects/workspace-labels.transaction.json")
        .exists());
}

#[tokio::test]
async fn corrupt_journal_blocks_initialization() {
    let fx = fx().await;
    let tx = fx
        .home
        .path()
        .join("projects/workspace-labels.transaction.json");
    std::fs::write(&tx, "{broken").unwrap();
    let svc = service(fx.home.path(), &fx.registry, None, None);
    assert!(svc.initialize().await.is_err());
}

#[test]
fn name_normalization_matches_js_whitespace_semantics() {
    use paseo_store::{normalize_workspace_label_name as n, workspace_label_key as k};
    assert_eq!(n("  a \t b\u{00A0}\u{FEFF}c  "), "a b c");
    assert_eq!(n("\u{0085}x"), "\u{0085}x", "NEL is not JS whitespace");
    assert_eq!(k("  Needs   REVIEW "), "needs review");
    assert_eq!(n("   "), "");
}
