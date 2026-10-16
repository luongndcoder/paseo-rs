//! Cross-implementation recovery: TS-written crashed transactions are recovered by Rust, and
//! a Rust-written crashed transaction has the same on-disk shape TS wrote (so TS recovers it).

use super::common::*;
use crate::controller::SaveRequest;
use crate::operations::{get_skills_status, install_skills, OpKind, SkillOp, SkillTargets};
use crate::selection::SkillSelection;
use crate::transaction::{begin_skills_transaction, recover_interrupted_skill_transactions};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

const TXID: &str = "00000000-0000-4000-8000-000000000001";

fn golden() -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/p4/skills/transactions.json");
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

fn targets_for(root: &Path) -> SkillTargets {
    SkillTargets {
        source_dir: root.join("bundle"),
        agents_dir: root.join("home/.agents/skills"),
        claude_dir: root.join("home/.claude/skills"),
        codex_dir: root.join("home/.codex/skills"),
    }
}

fn selection(value: &Value) -> SkillSelection {
    crate::selection::coerce_skill_selection(value)
}

/// Same layout as the TS script's `snapshot`: sorted DFS, files with utf8 content and mode.
fn snapshot(root: &Path, txid: &str) -> Vec<Value> {
    fn walk(root: &Path, dir: &Path, txid: &str, out: &mut Vec<Value>) {
        let mut names = names(dir);
        names.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
        for name in names {
            let full = dir.join(&name);
            let rel = hide(
                &full.strip_prefix(root).unwrap().to_string_lossy(),
                root,
                txid,
            );
            let meta = fs::symlink_metadata(&full).unwrap();
            if meta.file_type().is_symlink() {
                out.push(json!({
                    "path": rel,
                    "type": "symlink",
                    "target": hide(&fs::read_link(&full).unwrap().to_string_lossy(), root, txid),
                }));
            } else if meta.is_dir() {
                out.push(json!({"path": rel, "type": "dir"}));
                walk(root, &full, txid, out);
            } else {
                out.push(json!({
                    "path": rel,
                    "type": "file",
                    "content": hide(&fs::read_to_string(&full).unwrap(), root, txid),
                    "mode": crate::fsx::mode_bits(&meta),
                }));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, txid, &mut out);
    out
}

fn hide(text: &str, root: &Path, txid: &str) -> String {
    text.replace(&*root.to_string_lossy(), "{{ROOT}}")
        .replace(txid, "{{TXID}}")
}

fn materialize(root: &Path, entries: &Value) {
    for e in entries.as_array().unwrap() {
        let rel = e["path"].as_str().unwrap().replace("{{TXID}}", TXID);
        let full = root.join(rel);
        match e["type"].as_str().unwrap() {
            "dir" => fs::create_dir_all(&full).unwrap(),
            "symlink" => {
                #[cfg(unix)]
                std::os::unix::fs::symlink(e["target"].as_str().unwrap(), &full).unwrap();
            }
            _ => {
                fs::create_dir_all(full.parent().unwrap()).unwrap();
                let content = e["content"]
                    .as_str()
                    .unwrap()
                    .replace("{{ROOT}}", &root.to_string_lossy())
                    .replace("{{TXID}}", TXID);
                fs::write(&full, content).unwrap();
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    fs::set_permissions(
                        &full,
                        fs::Permissions::from_mode(e["mode"].as_u64().unwrap() as u32),
                    )
                    .unwrap();
                }
            }
        }
    }
}

/// Managed-files manifests list files in readdir order under TS and sorted order here;
/// both mean the same thing, so compare them as JSON.
fn assert_trees_equal(actual: &[Value], expected: &Value, label: &str) {
    let expected = expected.as_array().unwrap();
    let paths = |v: &[Value]| -> Vec<String> {
        v.iter()
            .map(|e| e["path"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(paths(actual), paths(expected), "{label}: paths");
    for (a, e) in actual.iter().zip(expected) {
        let path = a["path"].as_str().unwrap();
        if path.ends_with(".paseo-managed-files.json") {
            let pa: Value = serde_json::from_str(a["content"].as_str().unwrap()).unwrap();
            let pe: Value = serde_json::from_str(e["content"].as_str().unwrap()).unwrap();
            assert_eq!(pa, pe, "{label}: {path}");
            assert_eq!(a["mode"], e["mode"], "{label}: {path} mode");
        } else {
            assert_eq!(a, e, "{label}: {path}");
        }
    }
}

fn fresh_root() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::Builder::new()
        .prefix("paseo-skills-interop-")
        .tempdir()
        .unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    (dir, root)
}

#[test]
fn rust_recovers_a_ts_written_converging_transaction_to_the_previous_selection() {
    let g = golden();
    let (_dir, root) = fresh_root();
    materialize(&root, &g["crashed"]);

    recover_interrupted_skill_transactions(&targets_for(&root), &selection(&g["previous"]))
        .unwrap();

    assert_trees_equal(
        &snapshot(&root, TXID),
        &g["recoveredToPrevious"],
        "recoveredToPrevious",
    );
    assert_trees_equal(&snapshot(&root, TXID), &g["before"], "before");
}

#[test]
fn rust_discards_a_ts_written_transaction_once_the_next_selection_committed() {
    let g = golden();
    let (_dir, root) = fresh_root();
    materialize(&root, &g["crashed"]);

    recover_interrupted_skill_transactions(&targets_for(&root), &selection(&g["next"])).unwrap();

    assert_trees_equal(
        &snapshot(&root, TXID),
        &g["recoveredToNext"],
        "recoveredToNext",
    );
}

#[test]
fn rust_recovers_a_ts_written_capturing_transaction() {
    let g = golden();
    let (_dir, root) = fresh_root();
    materialize(&root, &g["capturing"]);

    recover_interrupted_skill_transactions(&targets_for(&root), &selection(&g["previous"]))
        .unwrap();

    assert_trees_equal(
        &snapshot(&root, TXID),
        &g["recoveredCapturing"],
        "recoveredCapturing",
    );
}

#[test]
fn rust_refuses_to_guess_when_the_committed_selection_matches_neither_side() {
    let g = golden();
    let (_dir, root) = fresh_root();
    materialize(&root, &g["crashed"]);
    let before = snapshot(&root, TXID);

    let error = recover_interrupted_skill_transactions(&targets_for(&root), &SkillSelection::All)
        .unwrap_err();

    assert!(error
        .to_string()
        .contains("Cannot safely recover interrupted skills transaction"));
    assert_eq!(snapshot(&root, TXID), before);
}

#[test]
fn rust_ignores_a_transaction_directory_whose_manifest_is_not_ours() {
    let g = golden();
    let (_dir, root) = fresh_root();
    materialize(&root, &g["crashed"]);
    let tx = root
        .join("home/.agents")
        .join(format!(".paseo-skills-transaction-{TXID}"));
    // Not our owner, so the whole directory (and everything staged) stays put.
    let text = fs::read_to_string(tx.join("transaction.json")).unwrap();
    fs::write(
        tx.join("transaction.json"),
        text.replace("paseo-skills-transaction\"", "someone-else\""),
    )
    .unwrap();
    let before = snapshot(&root, TXID);

    recover_interrupted_skill_transactions(&targets_for(&root), &selection(&g["previous"]))
        .unwrap();

    assert_eq!(snapshot(&root, TXID), before);
}

/// Builds the exact crashed state the TS script captured: save previous, add user files,
/// bump the bundle, then begin + converge the frozen plan and "crash" before commit.
fn rust_crashed_state(root: &Path, g: &Value) -> String {
    let targets = targets_for(root);
    for name in ["paseo", "paseo-advisor", "paseo-loop"] {
        write_bundle_skill(
            &targets.source_dir,
            name,
            &[("SKILL.md", &format!("{name}-v1"))],
        );
    }
    write_bundle_skill(
        &targets.source_dir,
        "paseo-loop",
        &[("references/guide.md", "guide-v1")],
    );
    let previous = selection(&g["previous"]);
    let next = selection(&g["next"]);
    let store = TestStore::plain(Some(previous.clone()));
    let controller = crate::controller::SkillsController::new(
        {
            let t = targets.clone();
            std::sync::Arc::new(move || t.clone())
        },
        crate::selection::SelectionStore::new(store),
    );
    controller
        .save(&SaveRequest::from_value(&json!({
            "mode": "custom", "skills": ["paseo", "paseo-loop"], "confirmedRemovals": []
        })))
        .unwrap();
    for dir in targets.roots() {
        write_on_disk_skill(dir, "paseo-loop", &[("notes/mine.md", "hand written")]);
    }
    write_on_disk_skill(
        &targets.agents_dir,
        "paseo",
        &[("my-context.md", "user context")],
    );
    for name in ["paseo", "paseo-advisor", "paseo-loop"] {
        write_bundle_skill(
            &targets.source_dir,
            name,
            &[("SKILL.md", &format!("{name}-v2"))],
        );
    }
    write_bundle_skill(
        &targets.source_dir,
        "paseo-loop",
        &[("references/guide.md", "guide-v2")],
    );

    let plan = get_skills_status(&targets, &next).unwrap();
    let kinds: Vec<String> = plan
        .ops
        .iter()
        .map(|op| format!("{}:{}", format!("{:?}", op.kind).to_lowercase(), op.name))
        .collect();
    assert_eq!(serde_json::to_value(&kinds).unwrap(), g["planOps"]);
    let tx = begin_skills_transaction(&targets, &previous, &next, &plan.ops).unwrap();
    std::mem::forget(tx);
    let frozen = crate::operations::SkillsStatus {
        ops: plan
            .ops
            .iter()
            .filter(|op| op.kind != OpKind::Delete)
            .cloned()
            .collect::<Vec<SkillOp>>(),
        ..plan.clone()
    };
    install_skills(&targets, &next, Some(frozen)).unwrap();
    first_with_prefix(
        targets.agents_dir.parent().unwrap(),
        ".paseo-skills-transaction-",
    )
    .unwrap()
    .replace(".paseo-skills-transaction-", "")
}

#[test]
fn rust_crashed_state_has_the_same_on_disk_shape_ts_wrote() {
    let g = golden();
    let (_dir, root) = fresh_root();
    let txid = rust_crashed_state(&root, &g);
    assert_trees_equal(&snapshot(&root, &txid), &g["crashed"], "crashed");
}

#[test]
fn rust_recovers_its_own_crashed_state_to_the_ts_before_tree() {
    let g = golden();
    let (_dir, root) = fresh_root();
    let txid = rust_crashed_state(&root, &g);
    recover_interrupted_skill_transactions(&targets_for(&root), &selection(&g["previous"]))
        .unwrap();
    assert_trees_equal(&snapshot(&root, &txid), &g["before"], "before");
}

/// Real reverse interop: the TS module recovers a transaction Rust left behind. Needs the
/// TS checkout (`P4_TS_SERVER_DIR=<ts>/packages/server`), so it is opt-in:
/// `cargo test -p paseo-skills -- --ignored ts_recovers`.
#[test]
#[ignore = "needs the TS checkout: set P4_TS_SERVER_DIR"]
fn ts_recovers_a_transaction_rust_left_behind() {
    let server_dir = PathBuf::from(std::env::var("P4_TS_SERVER_DIR").expect("P4_TS_SERVER_DIR"));
    let g = golden();
    let (_dir, root) = fresh_root();
    let txid = rust_crashed_state(&root, &g);
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/p4/skills/capture-skills.mts");

    let output = std::process::Command::new(server_dir.join("../../node_modules/.bin/tsx"))
        .current_dir(&server_dir)
        .env(
            "TS_SKILLS_DIR",
            server_dir.join("src/server/orchestration-skills"),
        )
        .arg(script)
        .arg("recover")
        .arg(&root)
        .arg(serde_json::to_string(&g["previous"]).unwrap())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json_line = stdout.lines().rev().find(|l| l.starts_with('[')).unwrap();
    let raw: Vec<Value> = serde_json::from_str(json_line).unwrap();
    // The TS script hides nothing; normalize like the Rust snapshot does.
    let normalized: Vec<Value> = raw
        .iter()
        .map(|e| {
            let mut e = e.clone();
            for key in ["path", "content", "target"] {
                if let Some(s) = e[key].as_str() {
                    e[key] = Value::String(hide(s, &root, &txid));
                }
            }
            e
        })
        .collect();
    assert_trees_equal(&normalized, &g["before"], "ts recovered rust state");
}
