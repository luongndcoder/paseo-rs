use super::common::*;
use crate::controller::{SaveRequest, SkillsConfirmation, SkillsSaveResult, SkillsSnapshot};
use crate::operations::*;
use crate::selection::SkillSelection;
use crate::transaction::begin_skills_transaction;
use serde_json::{json, Value};
use std::fs;
use std::path::Path;
use std::sync::Arc;

fn save(h: &Harness, request: Value) -> crate::error::Result<SkillsSaveResult> {
    h.controller.save(&SaveRequest::from_value(&request))
}

fn snapshot(
    state: SkillsState,
    ops: Vec<SkillOp>,
    installed: &[&str],
    selection: SkillSelection,
) -> SkillsSnapshot {
    SkillsSnapshot {
        status: SkillsStatus {
            state,
            ops,
            available: strs(&BUNDLED_SKILLS),
            installed: strs(installed),
        },
        selection,
    }
}

fn add(name: &str) -> SkillOp {
    SkillOp::new(OpKind::Add, name)
}

fn del(name: &str) -> SkillOp {
    SkillOp::new(OpKind::Delete, name)
}

fn custom(skills: &[&str]) -> SkillSelection {
    only(skills)
}

fn custom_json(skills: &[&str]) -> Value {
    json!({"mode": "custom", "skills": skills})
}

fn save_confirmed(skills: &[&str], confirmed: &[&str]) -> Value {
    json!({"mode": "custom", "skills": skills, "confirmedRemovals": confirmed})
}

fn matrix(h: &Harness, skill: &str, rel: &str, expected: &str) {
    assert_eq!(
        h.read_user_file(skill, rel),
        vec![
            Some(expected.into()),
            Some(expected.into()),
            Some(expected.into())
        ]
    );
}

fn live_paths(h: &Harness, skill: &str) -> Vec<std::path::PathBuf> {
    h.targets.roots().iter().map(|r| r.join(skill)).collect()
}

/// Runs `save` on a thread, waits for the gated store write, lets `during` mutate the
/// disk, then releases the store and returns the save outcome.
fn gated_save(
    h: &Harness,
    control: &GateControl,
    request: Value,
    during: impl FnOnce(),
    succeed: bool,
) -> crate::error::Result<SkillsSaveResult> {
    let controller = Arc::clone(&h.controller);
    let request = SaveRequest::from_value(&request);
    let join = std::thread::spawn(move || controller.save(&request));
    control.started.recv().unwrap();
    during();
    control.release(succeed);
    join.join().unwrap()
}

#[test]
fn imports_a_legacy_selection_only_while_daemon_selection_is_absent() {
    let h = harness_with(TestStore::plain(None));
    let first = h
        .controller
        .import_legacy_selection_if_unset(&custom_json(&["paseo", "paseo-loop"]))
        .unwrap();
    assert!(first.imported);
    assert_eq!(first.selection, custom(&["paseo", "paseo-loop"]));
    let second = h
        .controller
        .import_legacy_selection_if_unset(&json!({"mode": "all"}))
        .unwrap();
    assert!(!second.imported);
    assert_eq!(second.selection, custom(&["paseo", "paseo-loop"]));
    assert_eq!(h.installed_everywhere(), vec![Vec::<String>::new(); 3]);
}

#[test]
fn reports_one_snapshot_with_catalog_selection_status_and_pending_work() {
    let h = harness();
    assert_eq!(
        h.controller.status().unwrap(),
        snapshot(
            SkillsState::NotInstalled,
            vec![add("paseo"), add("paseo-advisor"), add("paseo-loop")],
            &[],
            SkillSelection::All
        )
    );
}

#[test]
fn installs_every_bundled_skill_while_the_selection_is_all() {
    let h = harness();
    assert_eq!(
        h.controller.install().unwrap(),
        snapshot(
            SkillsState::UpToDate,
            vec![],
            &BUNDLED_SKILLS,
            SkillSelection::All
        )
    );
    assert!(h.is_installed("paseo-advisor"));
}

#[test]
fn does_not_remove_deselected_directories_during_install() {
    let h = harness();
    save(&h, custom_json(&["paseo"])).unwrap();
    h.user_file("paseo-loop", "notes/mine.md", "keep this");
    h.controller.install().unwrap();
    matrix(&h, "paseo-loop", "notes/mine.md", "keep this");
}

#[test]
fn saves_a_custom_selection_converges_disk_and_returns_the_refreshed_snapshot() {
    let h = harness();
    let result = save(&h, custom_json(&["paseo-loop", "paseo"])).unwrap();
    assert_eq!(
        result,
        SkillsSaveResult {
            snapshot: snapshot(
                SkillsState::UpToDate,
                vec![],
                &["paseo", "paseo-loop"],
                custom(&["paseo", "paseo-loop"])
            ),
            confirmation_required: None,
        }
    );
    assert!(h.is_installed("paseo"));
    assert!(h.is_installed("paseo-loop"));
    assert!(!h.is_installed("paseo-advisor"));
}

#[test]
fn removes_a_skill_from_disk_when_it_is_dropped_from_the_selection() {
    let h = harness();
    h.controller.install().unwrap();
    save(
        &h,
        save_confirmed(&["paseo"], &["paseo-advisor", "paseo-loop"]),
    )
    .unwrap();
    assert!(h.is_installed("paseo"));
    assert!(!h.is_installed("paseo-advisor"));
    assert!(!h.is_installed("paseo-loop"));
}

#[test]
fn keeps_the_saved_selection_after_uninstall_so_a_later_install_restores_it() {
    let h = harness();
    save(&h, custom_json(&["paseo"])).unwrap();
    let after_uninstall = h.controller.uninstall().unwrap();
    let after_reinstall = h.controller.install().unwrap();
    assert_eq!(
        after_uninstall,
        snapshot(
            SkillsState::NotInstalled,
            vec![add("paseo")],
            &[],
            custom(&["paseo"])
        )
    );
    assert_eq!(
        after_reinstall,
        snapshot(
            SkillsState::UpToDate,
            vec![],
            &["paseo"],
            custom(&["paseo"])
        )
    );
    assert!(h.is_installed("paseo"));
    assert!(!h.is_installed("paseo-loop"));
}

#[test]
fn treats_an_empty_custom_selection_as_uninstall_while_keeping_the_preference() {
    let h = harness();
    h.controller.install().unwrap();
    let result = save(&h, save_confirmed(&[], &BUNDLED_SKILLS)).unwrap();
    assert_eq!(
        result.snapshot,
        snapshot(SkillsState::NotInstalled, vec![], &[], custom(&[]))
    );
    assert!(result.confirmation_required.is_none());
    assert!(!h.is_installed("paseo"));
}

#[test]
fn returns_to_every_bundled_skill_when_the_selection_goes_back_to_all() {
    let h = harness();
    save(&h, custom_json(&["paseo"])).unwrap();
    let result = save(&h, json!({"mode": "all"})).unwrap();
    assert_eq!(
        result.snapshot,
        snapshot(
            SkillsState::UpToDate,
            vec![],
            &BUNDLED_SKILLS,
            SkillSelection::All
        )
    );
    assert!(h.is_installed("paseo-advisor"));
}

#[test]
fn keeps_the_previous_selection_when_the_save_fails_to_reach_disk() {
    let h = harness();
    save(&h, custom_json(&["paseo"])).unwrap();
    h.block_agents_dir();
    assert!(save(&h, json!({"mode": "all"})).is_err());
    fs::remove_file(&h.targets.agents_dir).unwrap();
    assert_eq!(
        h.controller.status().unwrap(),
        snapshot(
            SkillsState::Drift,
            vec![add("paseo")],
            &["paseo"],
            custom(&["paseo"])
        )
    );
}

#[test]
fn saves_no_selection_at_all_when_the_very_first_save_fails() {
    let h = harness_with(TestStore::plain(None));
    h.block_agents_dir();
    assert!(save(&h, custom_json(&["paseo"])).is_err());
    fs::remove_file(&h.targets.agents_dir).unwrap();
    assert_eq!(
        h.controller.status().unwrap(),
        snapshot(
            SkillsState::NotInstalled,
            BUNDLED_SKILLS.iter().map(|n| add(n)).collect(),
            &[],
            SkillSelection::All
        )
    );
    assert_eq!(h.store.current(), None);
}

#[test]
fn restores_deleted_directories_byte_for_byte_when_the_selection_cannot_be_committed() {
    let h = harness_with(TestStore::unwritable(custom(&["paseo", "paseo-loop"])));
    h.controller.install().unwrap();
    h.user_file("paseo-loop", "notes/mine.md", "hand written");

    // Deselects paseo-loop and adds paseo-advisor, then fails to commit.
    let error = save(
        &h,
        save_confirmed(&["paseo", "paseo-advisor"], &["paseo-loop"]),
    )
    .unwrap_err();
    assert!(error.to_string().contains("selection store is read-only"));

    assert_eq!(
        h.controller.status().unwrap(),
        snapshot(
            SkillsState::UpToDate,
            vec![],
            &["paseo", "paseo-loop"],
            custom(&["paseo", "paseo-loop"])
        )
    );
    assert_eq!(
        h.installed_everywhere(),
        vec![
            strs(&["paseo", "paseo-loop"]),
            strs(&["paseo", "paseo-loop"]),
            strs(&["paseo-loop"]),
        ]
    );
    matrix(&h, "paseo-loop", "notes/mine.md", "hand written");
    assert_eq!(h.backup_artifacts(), vec![Vec::<String>::new(); 3]);
}

#[test]
fn preserves_files_added_by_another_writer_before_rollback() {
    let selection = custom(&["paseo"]);
    let (store, control) = TestStore::gated(selection.clone());
    let h = harness_with(store);
    // Gate is only for the save below: install goes through update, which never sets.
    h.controller.install().unwrap();
    fs::write(h.targets.source_dir.join("paseo/SKILL.md"), "paseo-v2").unwrap();

    let result = gated_save(
        &h,
        &control,
        custom_json(&["paseo"]),
        || h.user_file("paseo", "notes/concurrent.md", "keep this"),
        false,
    );

    assert!(result
        .unwrap_err()
        .to_string()
        .contains("selection store is read-only"));
    matrix(&h, "paseo", "notes/concurrent.md", "keep this");
}

#[test]
fn does_not_automatically_delete_files_preserved_from_a_rolled_back_add() {
    let (store, control) = TestStore::gated(custom(&["paseo"]));
    let h = harness_with(store);
    h.controller.install().unwrap();

    let result = gated_save(
        &h,
        &control,
        custom_json(&["paseo", "paseo-loop"]),
        || h.user_file("paseo-loop", "notes/concurrent.md", "keep this"),
        false,
    );
    assert!(result.is_err());

    h.controller.auto_update().unwrap();
    matrix(&h, "paseo-loop", "notes/concurrent.md", "keep this");
}

#[test]
fn merges_a_deleted_directory_backup_into_files_another_writer_recreated() {
    let (store, control) = TestStore::gated(custom(&["paseo", "paseo-loop"]));
    let h = harness_with(store);
    h.controller.install().unwrap();
    h.user_file("paseo-loop", "notes/before.md", "restore this");

    let result = gated_save(
        &h,
        &control,
        save_confirmed(&["paseo"], &["paseo-loop"]),
        || h.user_file("paseo-loop", "notes/concurrent.md", "keep this"),
        false,
    );
    assert!(result.is_err());

    matrix(&h, "paseo-loop", "notes/before.md", "restore this");
    matrix(&h, "paseo-loop", "notes/concurrent.md", "keep this");
}

#[test]
fn atomically_stages_a_deletion_before_another_writer_can_recreate_its_path() {
    let h = harness();
    let previous = custom(&["paseo", "paseo-loop"]);
    let next = custom(&["paseo"]);
    save(&h, custom_json(&["paseo", "paseo-loop"])).unwrap();

    let tx = begin_skills_transaction(&h.targets, &previous, &next, &[del("paseo-loop")]).unwrap();

    assert!(!h.is_installed("paseo-loop"));
    h.user_file("paseo-loop", "notes/concurrent.md", "keep this");
    tx.rollback().unwrap();
    matrix(&h, "paseo-loop", "notes/concurrent.md", "keep this");
    assert!(h.is_installed("paseo-loop"));
}

#[cfg(unix)]
#[test]
fn renames_an_untouched_staged_deletion_back_with_its_inode_and_mode() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let h = harness();
    let previous = custom(&["paseo", "paseo-loop"]);
    let next = custom(&["paseo"]);
    save(&h, custom_json(&["paseo", "paseo-loop"])).unwrap();
    h.user_file("paseo-loop", "SKILL.md", "paseo-loop-v1");
    let lives = live_paths(&h, "paseo-loop");
    for live in &lives {
        fs::set_permissions(live, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let before: Vec<u64> = lives
        .iter()
        .map(|p| fs::symlink_metadata(p).unwrap().ino())
        .collect();

    let tx = begin_skills_transaction(&h.targets, &previous, &next, &[del("paseo-loop")]).unwrap();
    tx.rollback().unwrap();

    let after: Vec<fs::Metadata> = lives
        .iter()
        .map(|p| fs::symlink_metadata(p).unwrap())
        .collect();
    assert_eq!(after.iter().map(|m| m.ino()).collect::<Vec<_>>(), before);
    assert!(after.iter().all(|m| m.mode() & 0o777 == 0o700));
}

#[test]
fn finishes_rollback_when_an_external_deletion_leaves_no_live_or_staged_path() {
    let h = harness();
    let previous = custom(&["paseo", "paseo-loop"]);
    let next = custom(&["paseo"]);
    save(&h, custom_json(&["paseo", "paseo-loop"])).unwrap();
    h.user_file("paseo-loop", "SKILL.md", "paseo-loop-v1");

    let tx = begin_skills_transaction(&h.targets, &previous, &next, &[del("paseo-loop")]).unwrap();
    let stage = first_with_prefix(&h.targets.codex_dir, ".paseo-skills-transaction-").unwrap();
    fs::remove_dir_all(h.targets.codex_dir.join(stage).join("paseo-loop")).unwrap();

    tx.rollback().unwrap();

    assert_eq!(
        h.read_user_file("paseo-loop", "SKILL.md"),
        vec![
            Some("paseo-loop-v1".into()),
            Some("paseo-loop-v1".into()),
            None
        ]
    );
    assert_eq!(h.backup_artifacts(), vec![Vec::<String>::new(); 3]);
    assert_eq!(h.controller.status().unwrap().selection, previous);
}

#[test]
fn quarantines_a_staged_directory_when_an_external_file_takes_its_live_path() {
    let h = harness();
    let previous = custom(&["paseo", "paseo-loop"]);
    let next = custom(&["paseo"]);
    save(&h, custom_json(&["paseo", "paseo-loop"])).unwrap();
    h.user_file("paseo-loop", "notes/mine.md", "keep this");

    let tx = begin_skills_transaction(&h.targets, &previous, &next, &[del("paseo-loop")]).unwrap();
    let live = h.targets.codex_dir.join("paseo-loop");
    fs::write(&live, "external replacement").unwrap();

    tx.rollback().unwrap();

    assert_eq!(read(&live), "external replacement");
    let recovered = first_with_prefix(&h.targets.codex_dir, ".paseo-skills-recovered-").unwrap();
    assert_eq!(
        read(h.targets.codex_dir.join(recovered).join("notes/mine.md")),
        "keep this"
    );
    assert_eq!(
        h.read_user_file("paseo-loop", "notes/mine.md"),
        vec![Some("keep this".into()), Some("keep this".into()), None]
    );
    assert_eq!(h.backup_artifacts(), vec![Vec::<String>::new(); 3]);
    assert_eq!(h.controller.status().unwrap().selection, previous);
}

#[test]
fn quarantines_staged_files_that_collide_with_a_recreated_directory() {
    let h = harness();
    let previous = custom(&["paseo", "paseo-loop"]);
    let next = custom(&["paseo"]);
    save(&h, custom_json(&["paseo", "paseo-loop"])).unwrap();
    h.user_file("paseo-loop", "SKILL.md", "paseo-loop-v1");
    h.user_file("paseo-loop", "notes/mine.md", "staged notes");

    let tx = begin_skills_transaction(&h.targets, &previous, &next, &[del("paseo-loop")]).unwrap();
    let live = h.targets.codex_dir.join("paseo-loop");
    fs::create_dir_all(live.join("notes")).unwrap();
    fs::write(live.join("SKILL.md"), "external skill").unwrap();
    fs::write(live.join("notes/mine.md"), "external notes").unwrap();

    tx.rollback().unwrap();

    assert_eq!(read(live.join("SKILL.md")), "external skill");
    assert_eq!(read(live.join("notes/mine.md")), "external notes");
    let recovered =
        first_with_prefix(&h.targets.codex_dir, ".paseo-skills-recovered-paseo-loop-").unwrap();
    let recovered = h.targets.codex_dir.join(recovered);
    assert_eq!(read(recovered.join("SKILL.md")), "paseo-loop-v1");
    assert_eq!(read(recovered.join("notes/mine.md")), "staged notes");
    assert_eq!(h.backup_artifacts(), vec![Vec::<String>::new(); 3]);
    assert_eq!(h.controller.status().unwrap().selection, previous);
}

#[test]
fn preserves_incompatible_live_paths_while_rolling_back_adds_and_updates() {
    let h = harness();
    let previous = custom(&["paseo"]);
    let next = custom(&["paseo", "paseo-advisor"]);
    save(&h, custom_json(&["paseo"])).unwrap();
    h.user_file("paseo", "notes/mine.md", "keep this");

    let tx = begin_skills_transaction(
        &h.targets,
        &previous,
        &next,
        &[SkillOp::new(OpKind::Update, "paseo"), add("paseo-advisor")],
    )
    .unwrap();
    let replaced_update = h.targets.agents_dir.join("paseo");
    fs::remove_dir_all(&replaced_update).unwrap();
    fs::write(&replaced_update, "external update replacement").unwrap();
    let replaced_add = h.targets.codex_dir.join("paseo-advisor");
    fs::write(&replaced_add, "external add replacement").unwrap();

    tx.rollback().unwrap();

    assert_eq!(read(&replaced_update), "external update replacement");
    assert_eq!(read(&replaced_add), "external add replacement");
    let parent = h.targets.agents_dir.parent().unwrap();
    let recovered = first_with_prefix(parent, ".paseo-skills-recovered-paseo-").unwrap();
    assert_eq!(
        read(parent.join(recovered).join("notes/mine.md")),
        "keep this"
    );
    assert_eq!(
        h.read_user_file("paseo", "notes/mine.md"),
        vec![None, Some("keep this".into()), Some("keep this".into())]
    );
    assert_eq!(h.backup_artifacts(), vec![Vec::<String>::new(); 3]);
    assert_eq!(h.controller.status().unwrap().selection, previous);
}

#[test]
fn recovers_after_an_update_backup_was_quarantined_before_transaction_cleanup() {
    let h = harness();
    let previous = custom(&["paseo"]);
    let next = custom(&["paseo", "paseo-advisor"]);
    save(&h, custom_json(&["paseo"])).unwrap();
    h.user_file("paseo", "notes/mine.md", "captured notes");

    let tx = begin_skills_transaction(
        &h.targets,
        &previous,
        &next,
        &[SkillOp::new(OpKind::Update, "paseo")],
    )
    .unwrap();
    std::mem::forget(tx);
    let parent = h.targets.agents_dir.parent().unwrap();
    let tx_name = first_with_prefix(parent, ".paseo-skills-transaction-").unwrap();
    let tx_dir = parent.join(&tx_name);
    let manifest: Value = serde_json::from_str(&read(tx_dir.join("transaction.json"))).unwrap();
    let live = h.targets.agents_dir.join("paseo");
    let entry = manifest["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["livePath"] == live.to_string_lossy().as_ref())
        .unwrap();
    let backup = tx_dir.join(entry["backupPath"].as_str().unwrap());
    let recovered = parent.join(format!(
        ".paseo-skills-recovered-paseo-{}",
        tx_name.replace(".paseo-skills-transaction-", "")
    ));
    fs::remove_dir_all(&live).unwrap();
    fs::write(&live, "external replacement").unwrap();
    fs::rename(backup, &recovered).unwrap();

    assert_eq!(h.controller.status().unwrap().selection, previous);

    assert_eq!(read(&live), "external replacement");
    assert_eq!(read(recovered.join("notes/mine.md")), "captured notes");
    assert_eq!(h.backup_artifacts(), vec![Vec::<String>::new(); 3]);
}

#[test]
fn preserves_a_directory_that_replaces_a_captured_file_before_recovery() {
    let h = harness();
    let previous = custom(&["paseo"]);
    let next = custom(&["paseo", "paseo-advisor"]);
    save(&h, custom_json(&["paseo"])).unwrap();
    let live = h.targets.agents_dir.join("paseo");
    fs::remove_dir_all(&live).unwrap();
    fs::write(&live, "captured file").unwrap();

    let tx = begin_skills_transaction(
        &h.targets,
        &previous,
        &next,
        &[SkillOp::new(OpKind::Update, "paseo")],
    )
    .unwrap();
    std::mem::forget(tx);
    fs::remove_file(&live).unwrap();
    fs::create_dir_all(&live).unwrap();
    fs::write(live.join("external.md"), "external directory").unwrap();

    assert_eq!(h.controller.status().unwrap().selection, previous);

    assert_eq!(read(live.join("external.md")), "external directory");
    let parent = h.targets.agents_dir.parent().unwrap();
    let recovered = first_with_prefix(parent, ".paseo-skills-recovered-paseo-").unwrap();
    assert_eq!(read(parent.join(recovered)), "captured file");
    assert_eq!(h.backup_artifacts(), vec![Vec::<String>::new(); 3]);
}

#[cfg(target_os = "linux")]
#[test]
fn stages_deletions_when_an_agent_skills_root_is_on_another_filesystem() {
    use std::os::unix::fs::MetadataExt;
    let cross = tempfile::Builder::new()
        .prefix("paseo-skills-controller-")
        .tempdir_in("/dev/shm")
        .unwrap();
    let mut h = harness();
    h.targets.claude_dir = cross.path().join("skills");
    let targets = h.targets.clone();
    let store = h.store.clone();
    h.controller = Arc::new(crate::controller::SkillsController::new(
        Arc::new(move || targets.clone()),
        crate::selection::SelectionStore::new(store),
    ));
    let previous = custom(&["paseo", "paseo-loop"]);
    let next = custom(&["paseo"]);
    save(&h, custom_json(&["paseo", "paseo-loop"])).unwrap();

    assert_ne!(
        fs::symlink_metadata(&h.targets.agents_dir).unwrap().dev(),
        fs::symlink_metadata(&h.targets.claude_dir).unwrap().dev()
    );
    let tx = begin_skills_transaction(&h.targets, &previous, &next, &[del("paseo-loop")]).unwrap();
    assert!(!h.is_installed("paseo-loop"));
    tx.rollback().unwrap();
    assert!(h.is_installed("paseo-loop"));
}

#[cfg(unix)]
#[test]
fn restores_a_relative_symlink_used_as_the_skill_directory() {
    let h = harness();
    let previous = custom(&["paseo", "paseo-loop"]);
    let next = custom(&["paseo"]);
    save(&h, custom_json(&["paseo", "paseo-loop"])).unwrap();
    let shared = h.root.join("home/shared/paseo-loop");
    write_files(&shared, &[("SKILL.md", "shared target")]);
    let live = h.targets.claude_dir.join("paseo-loop");
    fs::remove_dir_all(&live).unwrap();
    std::os::unix::fs::symlink(Path::new("../../shared/paseo-loop"), &live).unwrap();

    let tx = begin_skills_transaction(&h.targets, &previous, &next, &[del("paseo-loop")]).unwrap();
    tx.rollback().unwrap();

    assert!(fs::symlink_metadata(&live)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(read(live.join("SKILL.md")), "shared target");
}

#[cfg(unix)]
#[test]
fn restores_updates_made_through_a_relative_skill_directory_symlink() {
    let selection = custom(&["paseo"]);
    let (store, control) = TestStore::gated(selection);
    let h = harness_with(store);
    h.controller.install().unwrap();
    let shared = h.root.join("home/shared/paseo");
    write_files(&shared, &[("SKILL.md", "paseo-v1")]);
    let live = h.targets.claude_dir.join("paseo");
    fs::remove_dir_all(&live).unwrap();
    std::os::unix::fs::symlink(Path::new("../../shared/paseo"), &live).unwrap();
    fs::write(h.targets.source_dir.join("paseo/SKILL.md"), "paseo-v2").unwrap();

    let result = gated_save(&h, &control, custom_json(&["paseo"]), || {}, false);
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("selection store is read-only"));

    assert!(fs::symlink_metadata(&live)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(read(shared.join("SKILL.md")), "paseo-v1");
}

#[cfg(unix)]
#[test]
fn stages_one_deletion_when_two_agent_roots_alias_the_same_directory() {
    let h = harness();
    h.controller.install().unwrap();
    fs::remove_dir_all(&h.targets.claude_dir).unwrap();
    std::os::unix::fs::symlink(Path::new("../.agents/skills"), &h.targets.claude_dir).unwrap();

    let result = save(
        &h,
        save_confirmed(&["paseo", "paseo-advisor"], &["paseo-loop"]),
    )
    .unwrap();

    assert!(result.confirmation_required.is_none());
    assert!(!h.is_installed("paseo-loop"));
}

#[cfg(unix)]
#[test]
fn restores_a_user_added_symlink_when_deletion_rolls_back() {
    let previous = custom(&["paseo", "paseo-loop"]);
    let (store, control) = TestStore::gated(previous);
    let h = harness_with(store);
    h.controller.install().unwrap();
    for root in h.targets.roots() {
        let notes = root.join("paseo-loop/notes");
        fs::create_dir_all(&notes).unwrap();
        fs::write(notes.join("before.md"), "target").unwrap();
        std::os::unix::fs::symlink("before.md", notes.join("latest.md")).unwrap();
    }

    let result = gated_save(
        &h,
        &control,
        save_confirmed(&["paseo"], &["paseo-loop"]),
        || {},
        false,
    );
    assert!(result.is_err());

    for root in h.targets.roots() {
        let restored = root.join("paseo-loop/notes/latest.md");
        assert!(fs::symlink_metadata(&restored)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::read_link(&restored).unwrap(), Path::new("before.md"));
    }
}

#[cfg(unix)]
#[test]
fn restores_executable_permissions_when_deletion_rolls_back() {
    use std::os::unix::fs::PermissionsExt;
    let previous = custom(&["paseo", "paseo-loop"]);
    let (store, control) = TestStore::gated(previous);
    let h = harness_with(store);
    h.controller.install().unwrap();
    h.user_file("paseo-loop", "hooks/run.sh", "#!/bin/sh\n");
    for root in h.targets.roots() {
        fs::set_permissions(
            root.join("paseo-loop/hooks/run.sh"),
            fs::Permissions::from_mode(0o751),
        )
        .unwrap();
    }

    let result = gated_save(
        &h,
        &control,
        save_confirmed(&["paseo"], &["paseo-loop"]),
        || {},
        false,
    );
    assert!(result.is_err());

    for root in h.targets.roots() {
        let mode = fs::symlink_metadata(root.join("paseo-loop/hooks/run.sh"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o751);
    }
}

#[test]
fn leaves_no_backup_artifacts_behind_after_a_successful_save() {
    let h = harness();
    save(&h, custom_json(&["paseo"])).unwrap();
    assert_eq!(h.backup_artifacts(), vec![Vec::<String>::new(); 3]);
}

#[cfg(unix)]
#[test]
fn reports_a_committed_save_as_successful_when_cleanup_is_deferred() {
    let (store, control) = TestStore::gated(SkillSelection::All);
    let h = harness_with(store.clone());
    let next = custom(&["paseo"]);
    let parent = h.targets.agents_dir.parent().unwrap().to_path_buf();
    let moved = parent.with_file_name(format!(
        "{}-moved",
        parent.file_name().unwrap().to_string_lossy()
    ));

    let result = gated_save(
        &h,
        &control,
        custom_json(&["paseo"]),
        || {
            fs::rename(&parent, &moved).unwrap();
            fs::write(&parent, "block transaction cleanup with ENOTDIR").unwrap();
        },
        true,
    );
    fs::remove_file(&parent).unwrap();
    fs::rename(&moved, &parent).unwrap();

    assert!(result.is_ok(), "{result:?}");
    assert_eq!(store.current(), Some(next));
    h.controller.status().unwrap();
    assert_eq!(h.backup_artifacts(), vec![Vec::<String>::new(); 3]);
}

#[test]
fn does_not_delete_an_unrelated_directory_that_resembles_transaction_staging() {
    let h = harness();
    let unrelated = h
        .targets
        .agents_dir
        .parent()
        .unwrap()
        .join(".paseo-skills-transaction-my-notes");
    write_files(&unrelated, &[("mine.md", "keep me")]);
    save(&h, custom_json(&["paseo"])).unwrap();
    assert_eq!(read(unrelated.join("mine.md")), "keep me");
}

#[test]
fn recovers_an_interrupted_save_before_the_next_controller_operation() {
    let h = harness();
    let previous = custom(&["paseo", "paseo-loop"]);
    let next = custom(&["paseo", "paseo-advisor"]);
    save(&h, custom_json(&["paseo", "paseo-loop"])).unwrap();
    h.user_file("paseo-loop", "notes/mine.md", "hand written");
    let tx = begin_skills_transaction(&h.targets, &previous, &next, &[del("paseo-loop")]).unwrap();
    std::mem::forget(tx);
    for root in h.targets.roots() {
        // Staged deletes are renamed away already; the live path must be gone.
        let _ = fs::remove_dir_all(root.join("paseo-loop"));
    }

    h.controller.status().unwrap();

    matrix(&h, "paseo-loop", "notes/mine.md", "hand written");
    assert_eq!(h.backup_artifacts(), vec![Vec::<String>::new(); 3]);
}

#[test]
fn does_not_roll_back_an_interrupted_transaction_after_the_selection_committed() {
    let h = harness();
    let previous = custom(&["paseo", "paseo-loop"]);
    let next = custom(&["paseo"]);
    save(&h, custom_json(&["paseo", "paseo-loop"])).unwrap();
    let tx = begin_skills_transaction(&h.targets, &previous, &next, &[del("paseo-loop")]).unwrap();
    std::mem::forget(tx);
    crate::selection::ConfigStore::set_selection(&*h.store, &next).unwrap();

    let snapshot = h.controller.status().unwrap();

    assert_eq!(snapshot.selection, next);
    assert!(!h.is_installed("paseo-loop"));
    assert_eq!(h.backup_artifacts(), vec![Vec::<String>::new(); 3]);
}

#[test]
fn asks_for_confirmation_naming_the_directories_a_save_would_delete() {
    let h = harness();
    save(&h, custom_json(&["paseo", "paseo-loop"])).unwrap();
    // Something puts a managed directory back after the UI took its snapshot.
    h.user_file("paseo-advisor", "SKILL.md", "external");

    let result = save(&h, custom_json(&["paseo", "paseo-loop"])).unwrap();

    assert_eq!(
        result.confirmation_required,
        Some(SkillsConfirmation {
            removals: strs(&["paseo-advisor"])
        })
    );
    assert_eq!(
        h.installed_everywhere(),
        vec![
            strs(&["paseo", "paseo-advisor", "paseo-loop"]),
            strs(&["paseo", "paseo-advisor", "paseo-loop"]),
            strs(&["paseo-advisor"]),
        ]
    );
    assert_eq!(result.snapshot.selection, custom(&["paseo", "paseo-loop"]));
}

#[test]
fn applies_the_save_once_the_removals_are_confirmed() {
    let h = harness();
    h.controller.install().unwrap();
    let result = save(
        &h,
        save_confirmed(&["paseo"], &["paseo-advisor", "paseo-loop"]),
    )
    .unwrap();
    assert!(result.confirmation_required.is_none());
    assert_eq!(result.snapshot.selection, custom(&["paseo"]));
    assert_eq!(
        h.installed_everywhere(),
        vec![strs(&["paseo"]), strs(&["paseo"]), vec![]]
    );
}

#[test]
fn asks_again_when_another_directory_appears_before_the_retry() {
    let h = harness();
    h.controller.install().unwrap();
    h.user_file("paseo-chat", "SKILL.md", "retired but present");

    let result = save(
        &h,
        save_confirmed(&["paseo"], &["paseo-advisor", "paseo-loop"]),
    )
    .unwrap();

    assert_eq!(
        result.confirmation_required,
        Some(SkillsConfirmation {
            removals: strs(&["paseo-advisor", "paseo-chat", "paseo-loop"])
        })
    );
    assert_eq!(
        h.installed_everywhere(),
        vec![
            strs(&["paseo", "paseo-advisor", "paseo-chat", "paseo-loop"]),
            strs(&["paseo", "paseo-advisor", "paseo-chat", "paseo-loop"]),
            strs(&["paseo-chat"]),
        ]
    );
}

#[test]
fn saves_without_asking_when_nothing_would_be_deleted() {
    let h = harness();
    let result = save(&h, custom_json(&["paseo"])).unwrap();
    assert!(result.confirmation_required.is_none());
    assert_eq!(
        h.installed_everywhere(),
        vec![strs(&["paseo"]), strs(&["paseo"]), vec![]]
    );
}

#[test]
fn preserves_a_regular_file_at_a_skill_path_when_save_convergence_fails() {
    let h = harness();
    fs::create_dir_all(&h.targets.agents_dir).unwrap();
    let collision = h.targets.agents_dir.join("paseo");
    fs::write(&collision, "keep this file").unwrap();

    assert!(save(&h, custom_json(&["paseo"])).is_err());

    assert_eq!(read(&collision), "keep this file");
}

#[test]
fn updates_a_drifted_install_without_touching_the_saved_selection() {
    let h = harness();
    save(&h, custom_json(&["paseo"])).unwrap();
    fs::write(h.targets.agents_dir.join("paseo/SKILL.md"), "stale").unwrap();
    assert_eq!(
        h.controller.update().unwrap(),
        snapshot(
            SkillsState::UpToDate,
            vec![],
            &["paseo"],
            custom(&["paseo"])
        )
    );
}

#[test]
fn does_not_remove_deselected_directories_during_a_manual_update() {
    let h = harness();
    save(&h, custom_json(&["paseo"])).unwrap();
    h.user_file("paseo-loop", "notes/mine.md", "keep this");
    h.controller.update().unwrap();
    matrix(&h, "paseo-loop", "notes/mine.md", "keep this");
}

#[test]
fn keeps_confirmation_removals_when_the_store_is_unreadable_rather_than_deleting() {
    // A failing config read surfaces as an error before anything is touched.
    struct Broken;
    impl crate::selection::ConfigStore for Broken {
        fn selection(&self) -> crate::error::Result<Option<Value>> {
            Err(crate::error::SkillsError::Store("config offline".into()))
        }
        fn set_selection(&self, _: &SkillSelection) -> crate::error::Result<()> {
            Ok(())
        }
    }
    let sb = sandbox();
    write_current_bundle(&sb.targets.source_dir);
    write_to_all(&sb.targets, "paseo", &[("SKILL.md", "paseo-v1")]);
    let targets = sb.targets.clone();
    let controller = crate::controller::SkillsController::new(
        Arc::new(move || targets.clone()),
        crate::selection::SelectionStore::new(Arc::new(Broken)),
    );
    assert!(controller
        .status()
        .unwrap_err()
        .to_string()
        .contains("config offline"));
    assert_eq!(installed_in(&sb.targets, "paseo"), [true, true, true]);
}
