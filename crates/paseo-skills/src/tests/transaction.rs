//! Format-level checks for `transaction.json` and recovery guards.

use super::common::*;
use crate::operations::{OpKind, SkillOp};
use crate::selection::SkillSelection;
use crate::transaction::{begin_skills_transaction, recover_interrupted_skill_transactions};
use serde_json::Value;
use std::fs;

fn custom(skills: &[&str]) -> SkillSelection {
    only(skills)
}

#[test]
fn manifest_is_written_in_the_ts_shape() {
    let h = harness();
    let previous = custom(&["paseo"]);
    let next = custom(&["paseo", "paseo-loop"]);
    h.controller
        .save(&crate::controller::SaveRequest::new(previous.clone(), &[]))
        .unwrap();

    let tx = begin_skills_transaction(
        &h.targets,
        &previous,
        &next,
        &[SkillOp::new(OpKind::Add, "paseo-loop")],
    )
    .unwrap();
    let parent = h.targets.agents_dir.parent().unwrap();
    let name = first_with_prefix(parent, ".paseo-skills-transaction-").unwrap();
    let text = read(parent.join(&name).join("transaction.json"));

    assert!(
        text.ends_with("}\n"),
        "trailing newline like TS writeManifest"
    );
    let value: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(value["owner"], "paseo-skills-transaction");
    assert_eq!(value["version"], 1);
    assert_eq!(value["phase"], "converging");
    let keys: Vec<&str> = value
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        [
            "owner",
            "version",
            "phase",
            "previousSelection",
            "nextSelection",
            "entries"
        ]
    );
    let entry_keys: Vec<&str> = value["entries"][0]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(entry_keys, ["livePath", "kind", "backupPath"]);
    tx.commit().unwrap();
    assert!(first_with_prefix(parent, ".paseo-skills-transaction-").is_none());
}

#[test]
fn a_manifest_with_an_entry_outside_the_managed_roots_is_never_acted_on() {
    let h = harness();
    let previous = custom(&["paseo"]);
    let victim = h.root.join("victim");
    write_files(&victim, &[("keep.md", "keep")]);
    let parent = h.targets.agents_dir.parent().unwrap();
    let tx_dir = parent.join(".paseo-skills-transaction-forged");
    fs::create_dir_all(&tx_dir).unwrap();
    let manifest = serde_json::json!({
        "owner": "paseo-skills-transaction",
        "version": 1,
        "phase": "converging",
        "previousSelection": {"mode": "custom", "skills": ["paseo"]},
        "nextSelection": {"mode": "all"},
        "entries": [{"livePath": victim.to_string_lossy(), "kind": "delete", "backupPath": null}],
    });
    fs::write(tx_dir.join("transaction.json"), manifest.to_string()).unwrap();

    recover_interrupted_skill_transactions(&h.targets, &previous).unwrap();

    assert_eq!(read(victim.join("keep.md")), "keep");
    assert!(tx_dir.exists(), "not validated, so left in place");
}

#[test]
fn recovery_with_nothing_to_recover_is_a_no_op() {
    let h = harness();
    recover_interrupted_skill_transactions(&h.targets, &SkillSelection::All).unwrap();
}
