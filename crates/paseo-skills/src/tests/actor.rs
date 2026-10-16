use super::common::*;
use crate::actor::SkillsHandle;
use crate::operations::SkillsState;
use crate::selection::{InMemoryConfigStore, SkillSelection};
use std::sync::Arc;

fn handle() -> (Sandbox, SkillsHandle) {
    let sb = sandbox();
    for name in BUNDLED_SKILLS {
        write_bundle_skill(
            &sb.targets.source_dir,
            name,
            &[("SKILL.md", &format!("{name}-v1"))],
        );
    }
    let h = SkillsHandle::spawn_with_targets(
        sb.targets.clone(),
        Arc::new(InMemoryConfigStore::with_selection(&SkillSelection::All)),
    );
    (sb, h)
}

#[tokio::test]
async fn status_save_uninstall_round_trip_through_the_actor() {
    let (sb, h) = handle();
    assert_eq!(
        h.status().await.unwrap().status.state,
        SkillsState::NotInstalled
    );

    let saved = h.save_selection(only(&["paseo"]), vec![]).await.unwrap();
    assert!(saved.confirmation_required.is_none());
    assert_eq!(installed_in(&sb.targets, "paseo"), [true, true, false]);

    let asked = h.save_selection(only(&[]), vec![]).await.unwrap();
    assert_eq!(
        asked.confirmation_required.unwrap().removals,
        strs(&["paseo"])
    );
    assert_eq!(installed_in(&sb.targets, "paseo"), [true, true, false]);

    h.uninstall().await.unwrap();
    assert_eq!(installed_in(&sb.targets, "paseo"), [false, false, false]);
    assert_eq!(h.reconcile().await.unwrap().selection, only(&["paseo"]));
}

#[tokio::test]
async fn concurrent_startup_convergence_and_save_leave_disk_matching_the_saved_selection() {
    let (sb, h) = handle();
    h.install().await.unwrap();
    std::fs::remove_dir_all(sb.targets.claude_dir.join("paseo-loop")).unwrap();

    let (_, saved) = tokio::join!(
        h.auto_update(),
        h.save_selection(only(&["paseo"]), strs(&["paseo-advisor", "paseo-loop"]))
    );

    assert_eq!(saved.unwrap().snapshot.selection, only(&["paseo"]));
    assert_eq!(
        installed_in(&sb.targets, "paseo-advisor"),
        [false, false, false]
    );
    let status = h.status().await.unwrap();
    assert_eq!(status.status.state, SkillsState::UpToDate);
    assert_eq!(status.status.installed, strs(&["paseo"]));
}

#[tokio::test]
async fn an_error_does_not_wedge_the_queue() {
    let (sb, h) = handle();
    h.install().await.unwrap();
    // A missing skill plus an agents root that is a file: the save needs to add it and
    // cannot, so it fails mid-way.
    std::fs::remove_dir_all(sb.targets.claude_dir.join("paseo-loop")).unwrap();
    std::fs::remove_dir_all(&sb.targets.agents_dir).unwrap();
    std::fs::write(&sb.targets.agents_dir, "blocker").unwrap();
    assert!(h.save_selection(SkillSelection::All, vec![]).await.is_err());
    std::fs::remove_file(&sb.targets.agents_dir).unwrap();
    assert!(h.status().await.is_ok());
}

#[tokio::test]
async fn import_legacy_selection_goes_through_the_actor() {
    let sb = sandbox();
    let h =
        SkillsHandle::spawn_with_targets(sb.targets.clone(), Arc::new(InMemoryConfigStore::new()));
    let first = h
        .import_legacy_selection(serde_json::json!({"mode": "custom", "skills": ["b", "a"]}))
        .await
        .unwrap();
    assert!(first.imported);
    assert_eq!(first.selection, only(&["a", "b"]));
    let second = h
        .import_legacy_selection(serde_json::json!({"mode": "all"}))
        .await
        .unwrap();
    assert!(!second.imported);
}
