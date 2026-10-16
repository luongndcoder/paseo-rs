use super::common::*;
use crate::error::SkillsError;
use crate::operations::SkillTargets;
use crate::sync::{remove_skill, sync_skills, SkillSyncOptions, SkillSyncResult};
use std::fs;

fn run(t: &SkillTargets, names: &[&str]) -> crate::error::Result<SkillSyncResult> {
    sync_skills(SkillSyncOptions {
        source_dir: &t.source_dir,
        agents_dir: &t.agents_dir,
        claude_dir: &t.claude_dir,
        codex_dir: &t.codex_dir,
        skill_names: &strs(names),
        on_skill_error: None,
    })
}

#[test]
fn overwrites_on_disk_skill_content_when_the_bundle_differs() {
    let sb = sandbox();
    let t = &sb.targets;
    write_bundle_skill(&t.source_dir, "paseo", &[("SKILL.md", "new paseo content")]);
    write_on_disk_skill(&t.agents_dir, "paseo", &[("SKILL.md", "old paseo content")]);

    let result = run(t, &["paseo"]).unwrap();

    assert_eq!(result.processed_skills, 1);
    assert!(result.changed_files > 0);
    assert_eq!(
        read(t.agents_dir.join("paseo/SKILL.md")),
        "new paseo content"
    );
    assert_eq!(
        read(t.claude_dir.join("paseo/SKILL.md")),
        "new paseo content"
    );
    assert!(!exists(t.codex_dir.join("paseo/SKILL.md")));
}

#[test]
fn installs_new_bundled_skills_including_references() {
    let sb = sandbox();
    let t = &sb.targets;
    write_bundle_skill(
        &t.source_dir,
        "paseo-committee",
        &[
            ("SKILL.md", "committee content"),
            ("references/roles.md", "roles content"),
        ],
    );

    run(t, &["paseo-committee"]).unwrap();

    assert_eq!(
        read(t.agents_dir.join("paseo-committee/SKILL.md")),
        "committee content"
    );
    assert_eq!(
        read(t.agents_dir.join("paseo-committee/references/roles.md")),
        "roles content"
    );
    assert!(!exists(t.codex_dir.join("paseo-committee")));
    let claude = t.claude_dir.join("paseo-committee");
    assert!(fs::symlink_metadata(&claude).unwrap().is_dir());
    assert_eq!(read(claude.join("SKILL.md")), "committee content");
    assert_eq!(read(claude.join("references/roles.md")), "roles content");
}

#[test]
fn leaves_on_disk_skills_not_in_the_bundle_untouched() {
    let sb = sandbox();
    let t = &sb.targets;
    write_bundle_skill(&t.source_dir, "paseo", &[("SKILL.md", "new")]);
    write_on_disk_skill(
        &t.agents_dir,
        "user-custom-skill",
        &[("SKILL.md", "user content")],
    );

    run(t, &["paseo"]).unwrap();

    assert_eq!(
        read(t.agents_dir.join("user-custom-skill/SKILL.md")),
        "user content"
    );
}

#[test]
fn removes_stale_files_previously_written_to_managed_skill_dirs() {
    let sb = sandbox();
    let t = &sb.targets;
    write_bundle_skill(
        &t.source_dir,
        "paseo",
        &[("SKILL.md", "old"), ("references/stale.md", "stale")],
    );
    run(t, &["paseo"]).unwrap();

    fs::remove_dir_all(t.source_dir.join("paseo")).unwrap();
    write_bundle_skill(&t.source_dir, "paseo", &[("SKILL.md", "new")]);
    run(t, &["paseo"]).unwrap();

    let on_disk = t.agents_dir.join("paseo");
    assert_eq!(read(on_disk.join("SKILL.md")), "new");
    assert!(!exists(on_disk.join("references/stale.md")));
    assert!(!exists(on_disk.join("references")));
}

#[test]
fn preserves_user_added_files_in_managed_skill_dirs() {
    let sb = sandbox();
    let t = &sb.targets;
    write_bundle_skill(&t.source_dir, "paseo", &[("SKILL.md", "new")]);
    write_on_disk_skill(
        &t.agents_dir,
        "paseo",
        &[
            ("SKILL.md", "old"),
            ("my-context.md", "user context"),
            ("references/notes.md", "user notes"),
        ],
    );

    run(t, &["paseo"]).unwrap();

    let on_disk = t.agents_dir.join("paseo");
    assert_eq!(read(on_disk.join("SKILL.md")), "new");
    assert_eq!(read(on_disk.join("my-context.md")), "user context");
    assert_eq!(read(on_disk.join("references/notes.md")), "user notes");
}

#[cfg(unix)]
#[test]
fn rejects_managed_paths_that_would_write_through_a_symlink() {
    let sb = sandbox();
    let t = &sb.targets;
    write_bundle_skill(
        &t.source_dir,
        "paseo",
        &[
            ("SKILL.md", "new skill"),
            ("references/guide.md", "new guide"),
        ],
    );
    let on_disk = t.agents_dir.join("paseo");
    let external = sb.root.path().join("external-references");
    fs::create_dir_all(&on_disk).unwrap();
    fs::create_dir_all(&external).unwrap();
    fs::write(on_disk.join("SKILL.md"), "old skill").unwrap();
    fs::write(external.join("guide.md"), "external guide").unwrap();
    std::os::unix::fs::symlink(&external, on_disk.join("references")).unwrap();

    let error = run(t, &["paseo"]).unwrap_err();

    assert!(error
        .to_string()
        .contains("Cannot sync through symbolic link"));
    assert_eq!(read(on_disk.join("SKILL.md")), "old skill");
    assert_eq!(read(external.join("guide.md")), "external guide");
    assert!(fs::symlink_metadata(on_disk.join("references"))
        .unwrap()
        .file_type()
        .is_symlink());
}

#[test]
fn reports_zero_changed_files_on_a_no_op_resync() {
    let sb = sandbox();
    let t = &sb.targets;
    write_bundle_skill(
        &t.source_dir,
        "paseo",
        &[("SKILL.md", "content"), ("references/extra.md", "ref")],
    );
    assert!(run(t, &["paseo"]).unwrap().changed_files > 0);
    assert_eq!(run(t, &["paseo"]).unwrap().changed_files, 0);
}

#[test]
fn skips_names_missing_from_the_bundle_without_raising() {
    let sb = sandbox();
    let t = &sb.targets;
    write_bundle_skill(&t.source_dir, "paseo", &[("SKILL.md", "content")]);
    let mut errors: Vec<String> = Vec::new();
    let mut on_error = |name: &str, _: &SkillsError| errors.push(name.to_string());

    let result = sync_skills(SkillSyncOptions {
        source_dir: &t.source_dir,
        agents_dir: &t.agents_dir,
        claude_dir: &t.claude_dir,
        codex_dir: &t.codex_dir,
        skill_names: &strs(&["paseo", "paseo-removed"]),
        on_skill_error: Some(&mut on_error),
    })
    .unwrap();

    assert!(errors.is_empty());
    assert_eq!(result.processed_skills, 1);
    assert_eq!(read(t.agents_dir.join("paseo/SKILL.md")), "content");
    assert!(!exists(t.agents_dir.join("paseo-removed")));
}

#[test]
fn leaves_on_disk_content_alone_when_the_skill_left_the_bundle() {
    let sb = sandbox();
    let t = &sb.targets;
    write_bundle_skill(&t.source_dir, "paseo", &[("SKILL.md", "current")]);
    write_on_disk_skill(
        &t.agents_dir,
        "paseo-deprecated",
        &[("SKILL.md", "old content")],
    );

    run(t, &["paseo", "paseo-deprecated"]).unwrap();

    assert_eq!(
        read(t.agents_dir.join("paseo-deprecated/SKILL.md")),
        "old content"
    );
}

#[test]
fn does_not_crash_when_the_source_bundle_directory_is_missing() {
    let sb = sandbox();
    let mut t = sb.targets.clone();
    t.source_dir = sb.root.path().join("no-bundle-here");
    let mut errors: Vec<String> = Vec::new();
    let mut on_error = |name: &str, _: &SkillsError| errors.push(name.to_string());

    let result = sync_skills(SkillSyncOptions {
        source_dir: &t.source_dir,
        agents_dir: &t.agents_dir,
        claude_dir: &t.claude_dir,
        codex_dir: &t.codex_dir,
        skill_names: &strs(&["paseo"]),
        on_skill_error: Some(&mut on_error),
    })
    .unwrap();

    assert!(errors.is_empty());
    assert_eq!(result, SkillSyncResult::default());
}

#[test]
fn reports_per_skill_errors_via_callback_without_throwing() {
    let sb = sandbox();
    let t = &sb.targets;
    write_bundle_skill(&t.source_dir, "paseo", &[("SKILL.md", "content")]);
    // The agents skill path is a file, not a directory, to force a write error.
    fs::create_dir_all(&t.agents_dir).unwrap();
    fs::write(t.agents_dir.join("paseo"), "blocking file").unwrap();
    let mut errors: Vec<String> = Vec::new();
    let mut on_error = |name: &str, _: &SkillsError| errors.push(name.to_string());

    let result = sync_skills(SkillSyncOptions {
        source_dir: &t.source_dir,
        agents_dir: &t.agents_dir,
        claude_dir: &t.claude_dir,
        codex_dir: &t.codex_dir,
        skill_names: &strs(&["paseo"]),
        on_skill_error: Some(&mut on_error),
    })
    .unwrap();

    assert!(errors.contains(&"paseo".to_string()));
    assert_eq!(result.processed_skills, 0);
}

#[test]
fn rejects_manifest_paths_that_escape_the_skill_directory() {
    let sb = sandbox();
    let t = &sb.targets;
    write_bundle_skill(&t.source_dir, "paseo", &[("SKILL.md", "new")]);
    let on_disk = t.agents_dir.join("paseo");
    write_files(
        &on_disk,
        &[(
            ".paseo-managed-files.json",
            r#"{"version":1,"files":{"../../victim.txt":"abc"}}"#,
        )],
    );
    let victim = t.agents_dir.parent().unwrap().join("victim.txt");
    fs::write(&victim, "keep").unwrap();

    let error = run(t, &["paseo"]).unwrap_err();

    assert!(error.to_string().contains("outside skill directory"));
    assert_eq!(read(&victim), "keep");
}

#[test]
fn remove_skill_removes_from_all_three_targets() {
    let sb = sandbox();
    let t = &sb.targets;
    write_bundle_skill(&t.source_dir, "paseo", &[("SKILL.md", "content")]);
    run(t, &["paseo"]).unwrap();
    write_on_disk_skill(&t.codex_dir, "paseo", &[("SKILL.md", "codex")]);

    remove_skill("paseo", &t.remove_targets()).unwrap();

    assert_eq!(installed_in(t, "paseo"), [false, false, false]);
}

#[test]
fn remove_skill_does_not_throw_when_targets_are_missing() {
    let sb = sandbox();
    remove_skill("does-not-exist", &sb.targets.remove_targets()).unwrap();
}
