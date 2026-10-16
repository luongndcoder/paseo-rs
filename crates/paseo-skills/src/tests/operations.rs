use super::common::*;
use crate::operations::*;
use crate::selection::SkillSelection;
use crate::sync::MANAGED_FILES_MANIFEST;
use std::fs;

fn all() -> SkillSelection {
    SkillSelection::All
}

fn op(kind: OpKind, name: &str) -> SkillOp {
    SkillOp::new(kind, name)
}

fn status(
    state: SkillsState,
    ops: Vec<SkillOp>,
    available: &[&str],
    installed: &[&str],
) -> SkillsStatus {
    SkillsStatus {
        state,
        ops,
        available: strs(available),
        installed: strs(installed),
    }
}

// ---- getSkillsStatus -----------------------------------------------------------------

#[test]
fn not_installed_with_add_ops_when_nothing_is_on_disk() {
    let sb = sandbox();
    write_current_bundle(&sb.targets.source_dir);
    let s = get_skills_status(&sb.targets, &all()).unwrap();
    assert_eq!(s.state, SkillsState::NotInstalled);
    assert_eq!(
        s.ops,
        vec![op(OpKind::Add, "paseo"), op(OpKind::Add, "paseo-loop")]
    );
}

#[test]
fn reports_every_bundled_skill_as_available() {
    let sb = sandbox();
    write_current_bundle(&sb.targets.source_dir);
    write_bundle_skill(
        &sb.targets.source_dir,
        "paseo-advisor",
        &[("SKILL.md", "advisor-v1")],
    );
    let s = get_skills_status(&sb.targets, &only(&["paseo"])).unwrap();
    assert_eq!(s.available, strs(&["paseo", "paseo-advisor", "paseo-loop"]));
}

#[test]
fn a_skill_present_in_only_one_target_is_installed() {
    let sb = sandbox();
    let t = &sb.targets;
    write_current_bundle(&t.source_dir);
    write_on_disk_skill(&t.claude_dir, "paseo", &[("SKILL.md", "paseo-v1")]);
    let s = get_skills_status(t, &all()).unwrap();
    assert_eq!(s.installed, strs(&["paseo"]));
    assert_eq!(
        s.ops,
        vec![op(OpKind::Add, "paseo"), op(OpKind::Add, "paseo-loop")]
    );
}

#[test]
fn legacy_directories_left_on_disk_are_installed() {
    let sb = sandbox();
    let t = &sb.targets;
    write_current_bundle(&t.source_dir);
    write_on_disk_skill(&t.agents_dir, "paseo-chat", &[("SKILL.md", "chat-old")]);
    assert_eq!(
        get_skills_status(t, &all()).unwrap().installed,
        strs(&["paseo-chat"])
    );
}

#[test]
fn only_user_personal_skill_dirs_is_not_installed() {
    let sb = sandbox();
    let t = &sb.targets;
    write_current_bundle(&t.source_dir);
    for name in ["unslop", "tdd", "devbox"] {
        write_on_disk_skill(
            &t.agents_dir,
            name,
            &[("SKILL.md", &format!("user-{name}"))],
        );
    }
    let s = get_skills_status(t, &all()).unwrap();
    assert_eq!(s.state, SkillsState::NotInstalled);
    assert_eq!(
        s.ops,
        vec![op(OpKind::Add, "paseo"), op(OpKind::Add, "paseo-loop")]
    );
}

#[test]
fn up_to_date_when_every_bundled_skill_matches() {
    let sb = sandbox();
    let t = &sb.targets;
    write_current_bundle(&t.source_dir);
    write_to_all(t, "paseo", &[("SKILL.md", "paseo-v1")]);
    write_to_all(t, "paseo-loop", &[("SKILL.md", "loop-v1")]);
    assert_eq!(
        get_skills_status(t, &all()).unwrap(),
        status(
            SkillsState::UpToDate,
            vec![],
            &["paseo", "paseo-loop"],
            &["paseo", "paseo-loop"]
        )
    );
}

#[test]
fn ignores_user_added_files_inside_current_managed_dirs() {
    let sb = sandbox();
    let t = &sb.targets;
    write_current_bundle(&t.source_dir);
    write_to_all(t, "paseo", &[("SKILL.md", "paseo-v1")]);
    write_to_all(t, "paseo-loop", &[("SKILL.md", "loop-v1")]);
    write_on_disk_skill(&t.agents_dir, "paseo", &[("my-context.md", "user context")]);
    write_on_disk_skill(
        &t.claude_dir,
        "paseo",
        &[("commands/local.md", "user command")],
    );
    write_on_disk_skill(&t.codex_dir, "paseo", &[("hooks/guard.sh", "user guard")]);
    assert_eq!(
        get_skills_status(t, &all()).unwrap(),
        status(
            SkillsState::UpToDate,
            vec![],
            &["paseo", "paseo-loop"],
            &["paseo", "paseo-loop"]
        )
    );
}

#[test]
fn drift_with_a_single_update_op_when_one_file_diverges() {
    let sb = sandbox();
    let t = &sb.targets;
    write_current_bundle(&t.source_dir);
    write_on_disk_skill(&t.agents_dir, "paseo", &[("SKILL.md", "stale")]);
    write_on_disk_skill(&t.claude_dir, "paseo", &[("SKILL.md", "paseo-v1")]);
    write_on_disk_skill(&t.codex_dir, "paseo", &[("SKILL.md", "paseo-v1")]);
    write_to_all(t, "paseo-loop", &[("SKILL.md", "loop-v1")]);
    let s = get_skills_status(t, &all()).unwrap();
    assert_eq!(s.state, SkillsState::Drift);
    assert_eq!(s.ops, vec![op(OpKind::Update, "paseo")]);
}

#[test]
fn drift_when_a_secondary_agent_target_is_stale() {
    let sb = sandbox();
    let t = &sb.targets;
    write_current_bundle(&t.source_dir);
    write_on_disk_skill(&t.agents_dir, "paseo", &[("SKILL.md", "paseo-v1")]);
    write_on_disk_skill(&t.agents_dir, "paseo-loop", &[("SKILL.md", "loop-v1")]);
    write_on_disk_skill(&t.claude_dir, "paseo", &[("SKILL.md", "stale")]);
    write_on_disk_skill(&t.claude_dir, "paseo-loop", &[("SKILL.md", "loop-v1")]);
    write_on_disk_skill(&t.codex_dir, "paseo", &[("SKILL.md", "paseo-v1")]);
    write_on_disk_skill(&t.codex_dir, "paseo-loop", &[("SKILL.md", "loop-v1")]);
    let s = get_skills_status(t, &all()).unwrap();
    assert_eq!(s.state, SkillsState::Drift);
    assert_eq!(s.ops, vec![op(OpKind::Update, "paseo")]);
}

#[test]
fn drift_with_add_ops_for_bundled_skills_missing_from_disk() {
    let sb = sandbox();
    let t = &sb.targets;
    write_current_bundle(&t.source_dir);
    write_to_all(t, "paseo", &[("SKILL.md", "paseo-v1")]);
    let s = get_skills_status(t, &all()).unwrap();
    assert_eq!(s.state, SkillsState::Drift);
    assert_eq!(s.ops, vec![op(OpKind::Add, "paseo-loop")]);
}

#[test]
fn drift_with_a_delete_op_for_a_legacy_name_still_on_disk() {
    let sb = sandbox();
    let t = &sb.targets;
    write_current_bundle(&t.source_dir);
    write_to_all(t, "paseo", &[("SKILL.md", "paseo-v1")]);
    write_to_all(t, "paseo-loop", &[("SKILL.md", "loop-v1")]);
    write_on_disk_skill(&t.agents_dir, "paseo-chat", &[("SKILL.md", "chat-old")]);
    let s = get_skills_status(t, &all()).unwrap();
    assert_eq!(s.state, SkillsState::Drift);
    assert_eq!(s.ops, vec![op(OpKind::Delete, "paseo-chat")]);
}

#[test]
fn mixed_state_emits_ops_sorted_by_name() {
    let sb = sandbox();
    let t = &sb.targets;
    write_current_bundle(&t.source_dir);
    write_on_disk_skill(&t.agents_dir, "paseo", &[("SKILL.md", "stale")]);
    write_on_disk_skill(&t.claude_dir, "paseo", &[("SKILL.md", "paseo-v1")]);
    write_on_disk_skill(&t.codex_dir, "paseo", &[("SKILL.md", "paseo-v1")]);
    write_on_disk_skill(&t.agents_dir, "paseo-chat", &[("SKILL.md", "chat-old")]);
    let s = get_skills_status(t, &all()).unwrap();
    assert_eq!(s.state, SkillsState::Drift);
    assert_eq!(
        s.ops,
        vec![
            op(OpKind::Update, "paseo"),
            op(OpKind::Delete, "paseo-chat"),
            op(OpKind::Add, "paseo-loop"),
        ]
    );
}

// ---- custom selection ----------------------------------------------------------------

fn custom_sandbox() -> Sandbox {
    let sb = sandbox();
    write_current_bundle(&sb.targets.source_dir);
    write_bundle_skill(
        &sb.targets.source_dir,
        "paseo-advisor",
        &[("SKILL.md", "advisor-v1")],
    );
    sb
}

const THREE: [&str; 3] = ["paseo", "paseo-advisor", "paseo-loop"];

#[test]
fn installs_only_the_selected_skills() {
    let sb = custom_sandbox();
    let t = &sb.targets;
    let s = install_skills(t, &only(&["paseo", "paseo-loop"]), None).unwrap();
    assert_eq!(
        s,
        status(
            SkillsState::UpToDate,
            vec![],
            &THREE,
            &["paseo", "paseo-loop"]
        )
    );
    assert_eq!(installed_in(t, "paseo"), [true, true, false]);
    assert_eq!(installed_in(t, "paseo-loop"), [true, true, false]);
    assert_eq!(installed_in(t, "paseo-advisor"), [false, false, false]);
}

#[test]
fn up_to_date_while_an_unselected_bundled_skill_is_absent() {
    let sb = custom_sandbox();
    let t = &sb.targets;
    install_skills(t, &only(&["paseo"]), None).unwrap();
    assert_eq!(
        get_skills_status(t, &only(&["paseo"])).unwrap(),
        status(SkillsState::UpToDate, vec![], &THREE, &["paseo"])
    );
}

#[test]
fn removes_a_previously_installed_skill_once_it_leaves_the_selection() {
    let sb = custom_sandbox();
    let t = &sb.targets;
    install_skills(t, &all(), None).unwrap();
    let s = install_skills(t, &only(&["paseo"]), None).unwrap();
    assert_eq!(s.state, SkillsState::UpToDate);
    assert_eq!(installed_in(t, "paseo"), [true, true, false]);
    assert_eq!(installed_in(t, "paseo-loop"), [false, false, false]);
    assert_eq!(installed_in(t, "paseo-advisor"), [false, false, false]);
}

#[test]
fn reports_a_delete_op_for_a_deselected_skill_before_it_is_applied() {
    let sb = custom_sandbox();
    let t = &sb.targets;
    install_skills(t, &all(), None).unwrap();
    let s = get_skills_status(t, &only(&["paseo", "paseo-advisor"])).unwrap();
    assert_eq!(s.state, SkillsState::Drift);
    assert_eq!(s.ops, vec![op(OpKind::Delete, "paseo-loop")]);
}

#[test]
fn leaves_unrelated_user_skills_untouched() {
    let sb = custom_sandbox();
    let t = &sb.targets;
    write_on_disk_skill(&t.agents_dir, "unslop", &[("SKILL.md", "user-unslop")]);
    install_skills(t, &only(&["paseo"]), None).unwrap();
    assert_eq!(read(t.agents_dir.join("unslop/SKILL.md")), "user-unslop");
}

#[test]
fn an_empty_custom_selection_means_no_managed_skills_installed() {
    let sb = custom_sandbox();
    let t = &sb.targets;
    install_skills(t, &all(), None).unwrap();
    let s = install_skills(t, &only(&[]), None).unwrap();
    assert_eq!(s, status(SkillsState::NotInstalled, vec![], &THREE, &[]));
    assert_eq!(installed_in(t, "paseo"), [false, false, false]);
    assert_eq!(installed_in(t, "paseo-loop"), [false, false, false]);
}

#[test]
fn ignores_selected_names_the_bundle_does_not_ship() {
    let sb = custom_sandbox();
    let t = &sb.targets;
    let s = install_skills(t, &only(&["paseo", "not-a-skill"]), None).unwrap();
    assert_eq!(s, status(SkillsState::UpToDate, vec![], &THREE, &["paseo"]));
    assert_eq!(installed_in(t, "not-a-skill"), [false, false, false]);
}

#[test]
fn still_deletes_legacy_names_that_are_not_selectable() {
    let sb = custom_sandbox();
    let t = &sb.targets;
    write_to_all(t, "paseo-orchestrator", &[("SKILL.md", "orchestrator-old")]);
    install_skills(t, &only(&["paseo"]), None).unwrap();
    assert_eq!(installed_in(t, "paseo-orchestrator"), [false, false, false]);
}

// ---- install / update ----------------------------------------------------------------

#[test]
fn installs_into_shared_and_claude_roots_without_duplicating_codex() {
    let sb = sandbox();
    let t = &sb.targets;
    write_current_bundle(&t.source_dir);
    write_on_disk_skill(&t.agents_dir, "unslop", &[("SKILL.md", "user-unslop")]);
    let s = install_skills(t, &all(), None).unwrap();
    assert_eq!(
        s,
        status(
            SkillsState::UpToDate,
            vec![],
            &["paseo", "paseo-loop"],
            &["paseo", "paseo-loop"]
        )
    );
    for name in ["paseo", "paseo-loop"] {
        let expected = if name == "paseo" {
            "paseo-v1"
        } else {
            "loop-v1"
        };
        assert_eq!(read(t.agents_dir.join(name).join("SKILL.md")), expected);
        assert!(!exists(t.codex_dir.join(name).join("SKILL.md")));
        assert!(exists(t.claude_dir.join(name)));
    }
    assert_eq!(read(t.agents_dir.join("unslop/SKILL.md")), "user-unslop");
}

fn sha(content: &str) -> String {
    crate::fsx::sha256_hex(content.as_bytes())
}

#[test]
fn retires_unchanged_managed_codex_copies_while_preserving_personal_files() {
    let sb = sandbox();
    let t = &sb.targets;
    write_current_bundle(&t.source_dir);
    install_skills(t, &all(), None).unwrap();
    let legacy = t.codex_dir.join("paseo");
    let manifest = format!(
        r#"{{"version":1,"files":{{"SKILL.md":"{}","references/old.md":"{}"}}}}"#,
        sha("paseo-v1"),
        sha("bundled reference")
    );
    write_files(
        &legacy,
        &[
            ("SKILL.md", "paseo-v1"),
            ("references/old.md", "bundled reference"),
            ("notes/mine.md", "personal notes"),
            (MANAGED_FILES_MANIFEST, &manifest),
        ],
    );

    let s = auto_update_installed_skills(t, &all()).unwrap();

    assert_eq!(s.state, SkillsState::UpToDate);
    assert!(s.ops.is_empty());
    assert!(!exists(legacy.join("SKILL.md")));
    assert!(!exists(legacy.join("references/old.md")));
    assert_eq!(read(legacy.join("notes/mine.md")), "personal notes");
    assert_eq!(auto_update_installed_skills(t, &all()).unwrap(), s);
}

#[test]
fn keeps_edited_or_untracked_codex_skill_definitions() {
    let sb = sandbox();
    let t = &sb.targets;
    write_current_bundle(&t.source_dir);
    install_skills(t, &all(), None).unwrap();
    let manifest = format!(
        r#"{{"version":1,"files":{{"SKILL.md":"{}"}}}}"#,
        sha("paseo-v1")
    );
    write_on_disk_skill(
        &t.codex_dir,
        "paseo",
        &[
            ("SKILL.md", "personalized skill"),
            (MANAGED_FILES_MANIFEST, &manifest),
        ],
    );
    write_on_disk_skill(
        &t.codex_dir,
        "paseo-loop",
        &[("SKILL.md", "untracked skill")],
    );

    assert_eq!(
        auto_update_installed_skills(t, &all()).unwrap().state,
        SkillsState::UpToDate
    );
    assert_eq!(
        read(t.codex_dir.join("paseo/SKILL.md")),
        "personalized skill"
    );
    assert_eq!(
        read(t.codex_dir.join("paseo-loop/SKILL.md")),
        "untracked skill"
    );
}

#[cfg(unix)]
#[test]
fn keeps_installed_skills_when_a_root_links_to_another_root() {
    for (alias, target) in [
        ("codex", "agents"),
        ("codex", "claude"),
        ("claude", "codex"),
    ] {
        let sb = sandbox();
        let t = &sb.targets;
        write_current_bundle(&t.source_dir);
        let pick = |n: &str| match n {
            "agents" => t.agents_dir.clone(),
            "claude" => t.claude_dir.clone(),
            _ => t.codex_dir.clone(),
        };
        let (alias_root, target_root) = (pick(alias), pick(target));
        fs::create_dir_all(&target_root).unwrap();
        fs::create_dir_all(alias_root.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&target_root, &alias_root).unwrap();

        let installed = install_skills(t, &all(), None).unwrap();
        let updated = auto_update_installed_skills(t, &all()).unwrap();

        assert_eq!(installed.state, SkillsState::UpToDate, "{alias}->{target}");
        assert_eq!(updated.state, SkillsState::UpToDate, "{alias}->{target}");
        for name in ["paseo", "paseo-loop"] {
            assert_eq!(
                installed_in(t, name),
                [true, true, true],
                "{alias}->{target}"
            );
        }
    }
}

#[cfg(unix)]
#[test]
fn keeps_an_installed_skill_when_its_codex_directory_links_to_the_shared_copy() {
    let sb = sandbox();
    let t = &sb.targets;
    write_current_bundle(&t.source_dir);
    fs::create_dir_all(t.agents_dir.join("paseo")).unwrap();
    fs::create_dir_all(&t.codex_dir).unwrap();
    std::os::unix::fs::symlink(t.agents_dir.join("paseo"), t.codex_dir.join("paseo")).unwrap();

    let installed = install_skills(t, &all(), None).unwrap();
    let updated = auto_update_installed_skills(t, &all()).unwrap();

    assert_eq!(installed.state, SkillsState::UpToDate);
    assert_eq!(updated.state, SkillsState::UpToDate);
    assert_eq!(read(t.agents_dir.join("paseo/SKILL.md")), "paseo-v1");
}

#[test]
fn repairs_missing_and_edited_skills_without_deleting_a_legacy_directory() {
    let sb = sandbox();
    let t = &sb.targets;
    write_current_bundle(&t.source_dir);
    write_on_disk_skill(&t.agents_dir, "paseo", &[("SKILL.md", "stale")]);
    for dir in t.roots() {
        write_on_disk_skill(dir, "paseo-chat", &[("SKILL.md", "chat-old")]);
    }

    let s = update_skills(t, &all()).unwrap();

    assert_eq!(
        s,
        status(
            SkillsState::Drift,
            vec![op(OpKind::Delete, "paseo-chat")],
            &["paseo", "paseo-loop"],
            &["paseo", "paseo-chat", "paseo-loop"]
        )
    );
    assert_eq!(read(t.agents_dir.join("paseo/SKILL.md")), "paseo-v1");
    assert_eq!(read(t.agents_dir.join("paseo-loop/SKILL.md")), "loop-v1");
    for dir in t.roots() {
        assert!(exists(dir.join("paseo-chat")));
    }
}

#[test]
fn updated_is_the_state_reached_after_preserving_user_files() {
    let sb = sandbox();
    let t = &sb.targets;
    write_current_bundle(&t.source_dir);
    write_to_all(t, "paseo", &[("SKILL.md", "paseo-v1")]);
    write_to_all(t, "paseo-loop", &[("SKILL.md", "loop-v1")]);
    write_on_disk_skill(
        &t.agents_dir,
        "paseo",
        &[("SKILL.md", "stale"), ("hooks/guard.sh", "user guard")],
    );
    write_on_disk_skill(
        &t.claude_dir,
        "paseo",
        &[("notes/local.md", "claude notes")],
    );
    write_on_disk_skill(
        &t.codex_dir,
        "paseo",
        &[("prompts/local.md", "codex prompt")],
    );

    let s = update_skills(t, &all()).unwrap();

    assert_eq!(s.state, SkillsState::UpToDate);
    assert_eq!(
        get_skills_status(t, &all()).unwrap(),
        status(
            SkillsState::UpToDate,
            vec![],
            &["paseo", "paseo-loop"],
            &["paseo", "paseo-loop"]
        )
    );
    assert_eq!(
        read(t.agents_dir.join("paseo/hooks/guard.sh")),
        "user guard"
    );
    assert_eq!(
        read(t.claude_dir.join("paseo/notes/local.md")),
        "claude notes"
    );
    assert_eq!(
        read(t.codex_dir.join("paseo/prompts/local.md")),
        "codex prompt"
    );
}

#[test]
fn repairs_secondary_agent_targets_even_when_agents_skills_are_current() {
    let sb = sandbox();
    let t = &sb.targets;
    write_current_bundle(&t.source_dir);
    write_on_disk_skill(&t.agents_dir, "paseo", &[("SKILL.md", "paseo-v1")]);
    write_on_disk_skill(&t.agents_dir, "paseo-loop", &[("SKILL.md", "loop-v1")]);
    write_on_disk_skill(&t.claude_dir, "paseo", &[("SKILL.md", "paseo-v1")]);
    write_on_disk_skill(&t.codex_dir, "paseo", &[("SKILL.md", "paseo-v1")]);
    write_on_disk_skill(&t.codex_dir, "paseo-loop", &[("SKILL.md", "loop-v1")]);

    assert_eq!(
        update_skills(t, &all()).unwrap().state,
        SkillsState::UpToDate
    );
    assert_eq!(read(t.claude_dir.join("paseo-loop/SKILL.md")), "loop-v1");
}

#[test]
fn auto_updates_drifted_installed_skills() {
    let sb = sandbox();
    let t = &sb.targets;
    write_current_bundle(&t.source_dir);
    write_on_disk_skill(
        &t.agents_dir,
        "paseo",
        &[("SKILL.md", "stale"), ("hooks/guard.sh", "user guard")],
    );

    assert_eq!(
        auto_update_installed_skills(t, &all()).unwrap().state,
        SkillsState::UpToDate
    );
    assert_eq!(
        get_skills_status(t, &all()).unwrap().state,
        SkillsState::UpToDate
    );
    assert_eq!(read(t.agents_dir.join("paseo/SKILL.md")), "paseo-v1");
    assert_eq!(
        read(t.agents_dir.join("paseo/hooks/guard.sh")),
        "user guard"
    );
}

#[test]
fn does_not_auto_install_skills_on_a_clean_machine() {
    let sb = sandbox();
    let t = &sb.targets;
    write_current_bundle(&t.source_dir);
    assert_eq!(
        auto_update_installed_skills(t, &all()).unwrap(),
        status(
            SkillsState::NotInstalled,
            vec![op(OpKind::Add, "paseo"), op(OpKind::Add, "paseo-loop")],
            &["paseo", "paseo-loop"],
            &[]
        )
    );
    assert_eq!(installed_in(t, "paseo"), [false, false, false]);
}

#[test]
fn install_is_idempotent() {
    let sb = sandbox();
    let t = &sb.targets;
    write_current_bundle(&t.source_dir);
    assert_eq!(
        install_skills(t, &all(), None).unwrap().state,
        SkillsState::UpToDate
    );
    assert_eq!(
        install_skills(t, &all(), None).unwrap().state,
        SkillsState::UpToDate
    );
}

// ---- uninstall -----------------------------------------------------------------------

#[test]
fn uninstall_removes_every_paseo_skill_and_preserves_user_dirs() {
    let sb = sandbox();
    let t = &sb.targets;
    write_current_bundle(&t.source_dir);
    install_skills(t, &all(), None).unwrap();
    for name in ["unslop", "tdd", "devbox"] {
        write_on_disk_skill(
            &t.agents_dir,
            name,
            &[("SKILL.md", &format!("user-{name}"))],
        );
    }

    let s = uninstall_skills(t, &all()).unwrap();

    assert_eq!(s.state, SkillsState::NotInstalled);
    for name in ["paseo", "paseo-loop"]
        .into_iter()
        .chain(LEGACY_SKILL_NAMES)
    {
        assert_eq!(installed_in(t, name), [false, false, false]);
    }
    for name in ["unslop", "tdd", "devbox"] {
        assert_eq!(
            read(t.agents_dir.join(name).join("SKILL.md")),
            format!("user-{name}")
        );
    }
}

#[test]
fn uninstall_is_a_no_op_when_nothing_is_installed() {
    let sb = sandbox();
    write_current_bundle(&sb.targets.source_dir);
    assert_eq!(
        uninstall_skills(&sb.targets, &all()).unwrap().state,
        SkillsState::NotInstalled
    );
}

#[test]
fn uninstall_cleans_up_legacy_names_in_all_three_roots() {
    let sb = sandbox();
    let t = &sb.targets;
    write_current_bundle(&t.source_dir);
    for dir in t.roots() {
        write_on_disk_skill(dir, "paseo-chat", &[("SKILL.md", "chat-old")]);
    }
    assert_eq!(
        uninstall_skills(t, &all()).unwrap().state,
        SkillsState::NotInstalled
    );
    assert_eq!(installed_in(t, "paseo-chat"), [false, false, false]);
}
