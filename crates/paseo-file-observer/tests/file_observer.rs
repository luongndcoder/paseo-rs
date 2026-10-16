//! Real-filesystem contracts ported from `file-observer/index.test.ts`. Every
//! scenario runs against both backends: the per-directory (Linux) backend and
//! the recursive-native backend, on whatever platform runs the suite.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use paseo_file_observer::{
    BackendKind, FileChange, FileChangeType, FileObserver, FileObserverConfig, FileObserverError,
    FileObserverOptions, FileObserverSubscription,
};

#[derive(Clone, Default)]
struct Collected {
    events: Arc<Mutex<Vec<FileChange>>>,
    errors: Arc<Mutex<Vec<String>>>,
}

impl Collected {
    fn paths(&self) -> Vec<PathBuf> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .map(|e| e.path.clone())
            .collect()
    }
    fn has(&self, path: &Path) -> bool {
        self.events.lock().unwrap().iter().any(|e| e.path == path)
    }
    fn has_type(&self, path: &Path, kind: FileChangeType) -> bool {
        self.events
            .lock()
            .unwrap()
            .iter()
            .any(|e| e.path == path && e.change_type == kind)
    }
    fn len(&self) -> usize {
        self.events.lock().unwrap().len()
    }
}

fn observer(kind: BackendKind) -> FileObserver {
    FileObserver::with_config(FileObserverConfig {
        backend: kind,
        ..FileObserverConfig::default()
    })
}

async fn subscribe(
    observer: &FileObserver,
    root: &Path,
    ignore: Vec<PathBuf>,
) -> (FileObserverSubscription, Collected) {
    let collected = Collected::default();
    let sink = collected.clone();
    let subscription = observer
        .subscribe(
            root,
            move |result| match result {
                Ok(batch) => sink.events.lock().unwrap().extend(batch),
                Err(error) => sink.errors.lock().unwrap().push(error.to_string()),
            },
            FileObserverOptions { ignore },
        )
        .await
        .expect("subscribe");
    (subscription, collected)
}

async fn eventually(timeout: Duration, what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

const WAIT: Duration = Duration::from_secs(15);

macro_rules! each_backend {
    ($name:ident, |$kind:ident| $body:block) => {
        mod $name {
            use super::*;
            #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
            async fn native_recursive() {
                let $kind = BackendKind::NativeRecursive;
                $body
            }
            #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
            async fn per_directory() {
                let $kind = BackendKind::PerDirectory;
                $body
            }
        }
    };
}

each_backend!(
    observes_nested_files_while_pruning_excluded_directories,
    |kind| {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let (ignored, observed) = (root.join("ignored"), root.join("nested"));
        fs::create_dir(&ignored).unwrap();
        fs::create_dir(&observed).unwrap();
        let service = observer(kind);
        let (sub, events) = subscribe(&service, root, vec![ignored.clone()]).await;

        fs::write(ignored.join("ignored.txt"), "ignored").unwrap();
        let observed_path = observed.join("observed.txt");
        fs::write(&observed_path, "observed").unwrap();

        eventually(WAIT, "observed file", || events.has(&observed_path)).await;
        assert!(!events.has(&ignored.join("ignored.txt")));
        sub.unsubscribe().await;
        service.close().await;
    }
);

each_backend!(
    covers_files_populated_immediately_inside_a_new_directory,
    |kind| {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let service = observer(kind);
        let (sub, events) = subscribe(&service, root, vec![]).await;

        let created = root.join("new/nested");
        fs::create_dir_all(&created).unwrap();
        let first = created.join("first.txt");
        fs::write(&first, "first").unwrap();
        eventually(WAIT, "first file", || events.has(&first)).await;

        let second = created.join("second.txt");
        fs::write(&second, "second").unwrap();
        eventually(WAIT, "second file", || events.has(&second)).await;
        sub.unsubscribe().await;
        service.close().await;
    }
);

each_backend!(
    re_admits_an_excluded_directory_without_replacing_the_subscription,
    |kind| {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let ignored = root.join("ignored");
        fs::create_dir(&ignored).unwrap();
        let service = observer(kind);
        let (sub, events) = subscribe(&service, root, vec![ignored.clone()]).await;

        sub.update_ignore(vec![]).await.unwrap();
        let path = ignored.join("observed.txt");
        fs::write(&path, "observed").unwrap();
        eventually(WAIT, "re-admitted file", || events.has(&path)).await;
        sub.unsubscribe().await;
        service.close().await;
    }
);

each_backend!(an_ignore_update_is_a_barrier_for_later_delivery, |kind| {
    // notify's FSEvents backend restarts its stream on every unwatch(), which
    // drops events written meanwhile. inotify (the real per-directory target)
    // has no such gap, so only run this concurrent-write shape where it holds.
    if kind == BackendKind::PerDirectory && !cfg!(target_os = "linux") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let ignored = root.join("generated");
    fs::create_dir(&ignored).unwrap();
    let service = observer(kind);
    let (sub, events) = subscribe(&service, &root, vec![]).await;

    for index in 0..100 {
        fs::write(ignored.join(format!("before-{index}.txt")), "x").unwrap();
    }
    let outside: Vec<PathBuf> = (0..1_000)
        .map(|i| root.join(format!("outside-{i}.txt")))
        .collect();
    let writer = {
        let outside = outside.clone();
        tokio::task::spawn_blocking(move || {
            for (index, path) in outside.iter().enumerate() {
                fs::write(path, index.to_string()).unwrap();
            }
        })
    };
    sub.update_ignore(vec![ignored.clone()]).await.unwrap();
    writer.await.unwrap();
    eventually(Duration::from_secs(30), "all outside writes", || {
        let seen: std::collections::HashSet<PathBuf> = events.paths().into_iter().collect();
        outside.iter().all(|p| seen.contains(p))
    })
    .await;

    // Let in-flight native notices drain, then mark the barrier position.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let at_barrier = events.len();
    for index in 0..100 {
        fs::write(ignored.join(format!("after-{index}.txt")), "x").unwrap();
    }
    let sentinel = root.join("still-observed.txt");
    fs::write(&sentinel, "observed").unwrap();
    eventually(WAIT, "sentinel", || events.has(&sentinel)).await;

    let after: Vec<FileChange> = events.events.lock().unwrap()[at_barrier..].to_vec();
    assert!(
        after.iter().all(|e| !e.path.starts_with(&ignored)),
        "delivered below the excluded root after the barrier"
    );
    sub.unsubscribe().await;
    service.close().await;
});

each_backend!(survives_atomic_replacement_and_remains_observable, |kind| {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let target = root.join("target.txt");
    fs::write(&target, "before").unwrap();
    let service = observer(kind);
    let (sub, events) = subscribe(&service, root, vec![]).await;

    let replacement = root.join("replacement.txt");
    fs::write(&replacement, "after").unwrap();
    fs::rename(&replacement, &target).unwrap();
    eventually(WAIT, "replaced target", || events.has(&target)).await;

    events.events.lock().unwrap().clear();
    fs::write(&target, "again").unwrap();
    eventually(WAIT, "target after replacement", || events.has(&target)).await;
    sub.unsubscribe().await;
    service.close().await;
});

each_backend!(classifies_a_removed_file_as_deleted, |kind| {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let target = root.join("removed.txt");
    fs::write(&target, "before").unwrap();
    let service = observer(kind);
    let (sub, events) = subscribe(&service, root, vec![]).await;

    // Give the native stream a moment so the delete is a live event.
    tokio::time::sleep(Duration::from_millis(200)).await;
    fs::remove_file(&target).unwrap();
    eventually(WAIT, "delete", || {
        events.has_type(&target, FileChangeType::Delete)
    })
    .await;
    sub.unsubscribe().await;
    service.close().await;
});

each_backend!(
    observes_files_moved_into_the_tree_with_their_directory,
    |kind| {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let root = dir.path();
        let moved_from = outside.path().join("prepared");
        fs::create_dir(&moved_from).unwrap();
        fs::write(moved_from.join("already-written.txt"), "ready").unwrap();
        let service = observer(kind);
        let (sub, events) = subscribe(&service, root, vec![]).await;

        let moved_to = root.join("prepared");
        fs::rename(&moved_from, &moved_to).unwrap();
        let expected = moved_to.join("already-written.txt");
        eventually(WAIT, "moved file", || events.has(&expected)).await;
        sub.unsubscribe().await;
        service.close().await;
    }
);

each_backend!(delivers_no_callbacks_after_unsubscribe_resolves, |kind| {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let service = observer(kind);
    let (sub, events) = subscribe(&service, root, vec![]).await;
    sub.unsubscribe().await;
    let at_close = events.len();
    fs::write(root.join("after-close.txt"), "closed").unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(events.len(), at_close);
    assert!(!events.has(&root.join("after-close.txt")));
    service.close().await;
});

each_backend!(
    deleting_a_watched_root_cannot_wedge_idempotent_unsubscription,
    |kind| {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        fs::create_dir(&root).unwrap();
        let service = observer(kind);
        let (sub, _events) = subscribe(&service, &root, vec![]).await;
        fs::remove_dir_all(&root).unwrap();
        tokio::time::timeout(WAIT, sub.unsubscribe())
            .await
            .expect("first unsubscribe");
        tokio::time::timeout(WAIT, sub.unsubscribe())
            .await
            .expect("second unsubscribe");
        service.close().await;
    }
);

each_backend!(
    observes_a_thousand_writes_and_stays_healthy_after_delete_and_rename_churn,
    |kind| {
        // Directory removal unwatches, which restarts the FSEvents stream (see above).
        if kind == BackendKind::PerDirectory && !cfg!(target_os = "linux") {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let directories: Vec<PathBuf> = (0..20).map(|i| root.join(format!("dir-{i}"))).collect();
        for directory in &directories {
            fs::create_dir(directory).unwrap();
        }
        let paths: Vec<PathBuf> = (0..1_000)
            .map(|i| directories[i % directories.len()].join(format!("file-{i}.txt")))
            .collect();
        let populated = root.join("populated/nested");
        let populated_paths: Vec<PathBuf> = (0..200)
            .map(|i| populated.join(format!("nested-{i}.txt")))
            .collect();
        let removed = &paths[..100];
        let moves: Vec<(PathBuf, PathBuf)> = directories[..10]
            .iter()
            .enumerate()
            .map(|(i, from)| (from.clone(), root.join(format!("renamed-{i}"))))
            .collect();
        let moved_paths: Vec<PathBuf> = moves
            .iter()
            .flat_map(|(from, to)| {
                paths[100..]
                    .iter()
                    .filter(|p| p.parent() == Some(from.as_path()))
                    .map(|p| to.join(p.file_name().unwrap()))
                    .collect::<Vec<_>>()
            })
            .collect();
        let sentinel = directories[15].join("still-observed.txt");

        let service = observer(kind);
        let (sub, events) = subscribe(&service, &root, vec![]).await;
        let all_seen = |wanted: &[PathBuf], kind: Option<FileChangeType>| {
            let log = events.events.lock().unwrap();
            let seen: std::collections::HashSet<&PathBuf> = log
                .iter()
                .filter(|e| kind.is_none_or(|k| e.change_type == k))
                .map(|e| &e.path)
                .collect();
            wanted.iter().all(|p| seen.contains(p))
        };
        let long = Duration::from_secs(60);

        fs::create_dir_all(&populated).unwrap();
        for (i, path) in populated_paths.iter().enumerate() {
            fs::write(path, i.to_string()).unwrap();
        }
        eventually(long, "populated", || all_seen(&populated_paths, None)).await;

        for (i, path) in paths.iter().enumerate() {
            fs::write(path, i.to_string()).unwrap();
        }
        eventually(long, "writes", || all_seen(&paths, None)).await;

        // Drain late creates so one cannot coalesce with the expected delete.
        eventually(long, "drain", || {
            let d = service.get_diagnostics();
            d.pending_event_count == 0 && d.pending_classification_count == 0
        })
        .await;
        tokio::time::sleep(Duration::from_millis(500)).await;

        for path in removed {
            fs::remove_file(path).unwrap();
        }
        eventually(long, "deletions", || {
            all_seen(removed, Some(FileChangeType::Delete))
        })
        .await;

        for (from, to) in &moves {
            fs::rename(from, to).unwrap();
        }
        eventually(long, "moved files", || all_seen(&moved_paths, None)).await;
        for (_, to) in &moves {
            fs::remove_dir_all(to).unwrap();
        }
        fs::write(&sentinel, "alive").unwrap();
        eventually(long, "sentinel", || {
            all_seen(std::slice::from_ref(&sentinel), None)
        })
        .await;
        assert!(events.errors.lock().unwrap().is_empty());
        sub.unsubscribe().await;
        service.close().await;
    }
);

each_backend!(unsubscribe_cancels_work_queued_by_a_write_burst, |kind| {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let service = observer(kind);
    let (sub, events) = subscribe(&service, root, vec![]).await;
    for index in 0..1_000 {
        fs::write(root.join(format!("closing-{index}.txt")), index.to_string()).unwrap();
    }
    tokio::time::timeout(WAIT, sub.unsubscribe())
        .await
        .expect("unsubscribe");
    let at_close = events.len();
    fs::write(root.join("after-burst-close.txt"), "closed").unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(events.len(), at_close);
    service.close().await;
});

each_backend!(
    aggregate_diagnostics_return_to_their_lifecycle_baseline,
    |kind| {
        let dir = tempfile::tempdir().unwrap();
        let service = observer(kind);
        let (sub, _events) = subscribe(&service, dir.path(), vec![]).await;
        let live = service.get_diagnostics();
        assert_eq!(live.active_observation_count, 1);
        assert!(live.native_handle_count > 0);

        sub.unsubscribe().await;
        let after = service.get_diagnostics();
        assert_eq!(after.active_observation_count, 0);
        assert_eq!(after.native_handle_count, 0);
        assert_eq!(after.native_tracked_file_count, 0);
        assert_eq!(after.pending_event_count, 0);
        assert_eq!(after.reconciliation_in_flight_count, 0);
        service.close().await;
    }
);

each_backend!(
    closing_the_service_releases_every_subscription_and_rejects_reuse,
    |kind| {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let service = observer(kind);
        let (_a, _) = subscribe(&service, first.path(), vec![]).await;
        let (_b, _) = subscribe(&service, second.path(), vec![]).await;
        assert_eq!(service.get_diagnostics().active_observation_count, 2);

        service.close().await;

        assert_eq!(service.get_diagnostics(), Default::default());
        let rejected = service
            .subscribe(first.path(), |_| {}, FileObserverOptions::default())
            .await;
        assert!(matches!(rejected, Err(FileObserverError::Closed)));
        assert_eq!(
            rejected.err().unwrap().to_string(),
            "File observer is closed"
        );
    }
);

each_backend!(rejects_a_missing_root_and_a_file_root, |kind| {
    let dir = tempfile::tempdir().unwrap();
    let service = observer(kind);
    let missing = service
        .subscribe(
            dir.path().join("missing"),
            |_| {},
            FileObserverOptions::default(),
        )
        .await;
    assert!(missing.err().unwrap().is_missing_path());
    let file = dir.path().join("file.txt");
    fs::write(&file, "x").unwrap();
    let not_dir = service
        .subscribe(&file, |_| {}, FileObserverOptions::default())
        .await;
    assert!(matches!(not_dir, Err(FileObserverError::NotADirectory(_))));
    assert_eq!(service.get_diagnostics().active_observation_count, 0);
    service.close().await;
});

each_backend!(
    the_recursive_watcher_remembers_announced_files_before_reconciliation,
    |kind| {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let service = observer(kind);
        let (sub, events) = subscribe(&service, root, vec![]).await;
        let path = root.join("new.txt");
        fs::write(&path, "created").unwrap();
        eventually(WAIT, "create", || {
            events.has_type(&path, FileChangeType::Create)
        })
        .await;
        if kind == BackendKind::NativeRecursive {
            assert_eq!(service.get_diagnostics().native_tracked_file_count, 1);
        }
        fs::remove_file(&path).unwrap();
        sub.update_ignore(vec![root.join("unrelated")])
            .await
            .unwrap();
        eventually(WAIT, "delete", || {
            events.has_type(&path, FileChangeType::Delete)
        })
        .await;
        sub.unsubscribe().await;
        service.close().await;
    }
);

#[test]
fn wire_shapes_match_the_ts_json() {
    let change = FileChange::new(FileChangeType::Create, "/r/a.txt");
    assert_eq!(
        serde_json::to_value(&change).unwrap(),
        serde_json::json!({"path": "/r/a.txt", "type": "create"})
    );
    let diagnostics =
        serde_json::to_value(paseo_file_observer::FileObserverDiagnostics::default()).unwrap();
    let mut keys: Vec<&str> = diagnostics
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    let mut expected = vec![
        "activeObservationCount",
        "nativeHandleCount",
        "nativeTrackedFileCount",
        "pendingEventCount",
        "pendingReconciliationWorkCount",
        "pendingClassificationCount",
        "reconciliationInFlightCount",
        "reconciliationCount",
        "scopedReconciliationCount",
        "fullReconciliationCount",
        "reconciliationFailureCount",
        "observerFailureCount",
        "directoryLimitFailureCount",
        "nativeEventCount",
        "nativeChangeEventCount",
        "nativeRenameEventCount",
        "nativePathlessEventCount",
        "nativeClassificationCount",
        "nativeShallowScanCount",
        "lastReconciliationDurationMs",
        "maxReconciliationDurationMs",
    ];
    expected.sort_unstable();
    assert_eq!(keys, expected);
}

each_backend!(cancelled_subscribe_leaves_no_observation, |kind| {
    let dir = tempfile::tempdir().unwrap();
    let service = observer(kind);
    let cancelled = tokio::time::timeout(
        Duration::ZERO,
        service.subscribe(dir.path(), |_| {}, FileObserverOptions::default()),
    )
    .await;
    drop(cancelled);
    let deadline = Instant::now() + Duration::from_secs(5);
    while service.get_diagnostics().active_observation_count != 0 {
        assert!(Instant::now() < deadline, "observation leaked after cancel");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    service.close().await;
});
