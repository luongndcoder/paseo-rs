use serde_json::{json, Value};

use super::*;

fn project(id: &str, name: &str) -> Value {
    json!({
        "projectId": id,
        "projectDisplayName": name,
        "projectRootPath": format!("/{id}"),
        "projectKind": "git"
    })
}

fn cursor(generation: &str, after: i64) -> DirectorySyncCursor {
    DirectorySyncCursor {
        generation: Some(generation.into()),
        after_seq: Some(after),
    }
}

fn to_json<T: serde::Serialize>(v: &T) -> Value {
    serde_json::to_value(v).unwrap()
}

// TS: owns sequencing and returns wire-ready latest project changes
#[test]
fn latest_project_changes() {
    let s = DirectorySync::new("generation");
    let first = s.synchronize_projects([project("one", "One")], &DirectorySyncCursor::default());
    assert_eq!(first.sync.mode, "snapshot");
    assert_eq!(first.sync.head_seq, 1);

    s.sequence_project_update(
        &ProjectUpdate::Upsert {
            project: project("one", "Renamed"),
        },
        true,
    );
    s.sequence_project_update(
        &ProjectUpdate::Upsert {
            project: project("one", "Latest"),
        },
        true,
    );
    let read = s.synchronize_projects([project("one", "Latest")], &cursor("generation", 1));
    let mut expected_project = project("one", "Latest");
    expected_project["syncSeq"] = json!(3);
    assert_eq!(
        to_json(&read),
        json!({
            "projects": [expected_project],
            "sync": {"generation":"generation","headSeq":3,"mode":"changes","removals":[]}
        })
    );
}

// TS: sequences a hard project removal once
#[test]
fn hard_removal_once() {
    let s = DirectorySync::new("generation");
    s.synchronize_projects([project("one", "One")], &DirectorySyncCursor::default());
    let update = ProjectUpdate::Remove {
        project_id: "one".into(),
    };
    let expected = json!({"kind":"remove","projectId":"one","generation":"generation","seq":2});
    assert_eq!(s.sequence_project_update(&update, true), expected);
    assert_eq!(s.sequence_project_update(&update, true), expected);
    let read = s.synchronize_projects(Vec::new(), &cursor("generation", 1));
    assert_eq!(to_json(&read.sync.removals), json!([{"id":"one","seq":2}]));
}

// TS: keeps one daemon-wide version for repeated observations
#[test]
fn repeated_observations_same_seq() {
    let s = DirectorySync::new("generation");
    let update = ProjectUpdate::Upsert {
        project: project("one", "One"),
    };
    let mut expected = update.to_payload();
    expected["generation"] = json!("generation");
    expected["seq"] = json!(1);
    assert_eq!(s.sequence_project_update(&update, true), expected);
    assert_eq!(s.sequence_project_update(&update, true), expected);
}

// TS: sequences only the project whose effective icon changed
#[test]
fn only_changed_project_icon() {
    let s = DirectorySync::new("generation");
    let mut one = project("one", "One");
    one["projectIconRevision"] = json!("icon-a");
    let mut two = project("two", "Two");
    two["projectIconRevision"] = json!("icon-a");
    s.synchronize_projects([one.clone(), two.clone()], &DirectorySyncCursor::default());
    let mut changed = one.clone();
    changed["projectIconRevision"] = json!("icon-b");
    s.sequence_project_update(
        &ProjectUpdate::Upsert {
            project: changed.clone(),
        },
        true,
    );

    let read = s.synchronize_projects([changed.clone(), two], &cursor("generation", 2));
    let mut expected = changed;
    expected["syncSeq"] = json!(3);
    assert_eq!(
        to_json(&read),
        json!({
            "projects": [expected],
            "sync": {"generation":"generation","headSeq":3,"mode":"changes","removals":[]}
        })
    );
}

// TS: falls back to a snapshot when the daemon generation changes
#[test]
fn generation_changed_snapshot() {
    let s = DirectorySync::new("new-generation");
    let read = s.synchronize_projects([project("one", "One")], &cursor("old-generation", 10));
    assert_eq!(read.sync.mode, "snapshot");
    assert_eq!(read.sync.reason, Some("generation_changed"));
    assert_eq!(read.sync.head_seq, 1);
}

#[test]
fn missing_afterseq_with_matching_generation_is_no_cursor() {
    let s = DirectorySync::new("g");
    let c = DirectorySyncCursor {
        generation: Some("g".into()),
        after_seq: None,
    };
    assert_eq!(
        s.synchronize_projects([project("a", "A")], &c).sync.reason,
        Some("no_cursor")
    );
}

#[test]
fn background_generation_stamps_and_cursor() {
    let s = DirectorySync::new("g");
    let ws = json!({"id":"w1","name":"W"});
    // bare generation does not match a background reader
    let r = s.synchronize_workspaces([ws.clone()], &cursor("g", 0), true);
    assert_eq!(r.sync.reason, Some("generation_changed"));
    assert_eq!(r.sync.generation, "g:background");
    assert_eq!(
        to_json(&r.entries),
        json!([{"id":"w1","name":"W","syncSeq":1}])
    );
    assert_eq!(
        to_json(&r.page_info),
        json!({"nextCursor":null,"prevCursor":null,"hasMore":false})
    );
    assert!(r.empty_projects.is_empty());
    // matching background cursor reads changes
    let r = s.synchronize_workspaces([ws.clone()], &cursor("g:background", 1), true);
    assert_eq!(r.sync.mode, "changes");
    // update payload carries the background generation; seq version
    let out =
        s.sequence_workspace_update(json!({"kind":"upsert"}), Some(ws.clone()), "w1", true, true);
    assert_eq!(
        out,
        json!({"kind":"upsert","generation":"g:background","seq":1})
    );
    let out = s.sequence_workspace_update(json!({"kind":"upsert"}), Some(ws), "w1", true, false);
    assert_eq!(out, json!({"kind":"upsert","generation":"g","seq":1}));
}

#[test]
fn workspace_snapshot_expired_cursor_and_removal() {
    let s = DirectorySync::new("g");
    s.synchronize_workspaces([json!({"id":"a"})], &DirectorySyncCursor::default(), false);
    let r = s.synchronize_workspaces([json!({"id":"a"})], &cursor("g", 99), false);
    assert_eq!(r.sync.reason, Some("cursor_expired"));
    assert_eq!(r.sync.mode, "snapshot");
    // removal through snapshot replace_all
    let r = s.synchronize_workspaces(Vec::new(), &cursor("g", 1), false);
    assert_eq!(to_json(&r.sync.removals), json!([{"id":"a","seq":2}]));
    assert!(r.entries.is_empty());
}

#[test]
fn agent_updates_use_nested_id_and_pass_through_without_sequence() {
    let s = DirectorySync::new("g");
    let entry = json!({"agent":{"id":"a1","title":"t"},"project":{"projectId":"p"}});
    let payload = json!({"kind":"upsert"});
    // includeSequence=false still records the value but leaves the payload untouched
    assert_eq!(
        s.sequence_agent_update(payload.clone(), Some(entry.clone()), "a1", false, false),
        payload
    );
    // unchanged observation keeps seq 1
    assert_eq!(
        s.sequence_agent_update(payload.clone(), Some(entry), "a1", true, false),
        json!({"kind":"upsert","generation":"g","seq":1})
    );
    // removal bumps; remove of unknown id leaves payload unchanged
    assert_eq!(
        s.sequence_agent_update(payload.clone(), None, "a1", true, false),
        json!({"kind":"upsert","generation":"g","seq":2})
    );
    assert_eq!(
        s.sequence_agent_update(payload.clone(), None, "zzz", true, false),
        payload
    );
    let r = s.synchronize_agents(Vec::new(), &cursor("g", 1), false);
    assert_eq!(to_json(&r.sync.removals), json!([{"id":"a1","seq":2}]));
}

#[test]
fn sync_metadata_key_order() {
    let s = DirectorySync::new("g");
    let r = s.synchronize_projects([project("a", "A")], &DirectorySyncCursor::default());
    assert_eq!(
        serde_json::to_string(&r.sync).unwrap(),
        r#"{"generation":"g","headSeq":1,"mode":"snapshot","reason":"no_cursor","removals":[]}"#
    );
    let r = s.synchronize_projects([project("a", "A")], &cursor("g", 1));
    assert_eq!(
        serde_json::to_string(&r.sync).unwrap(),
        r#"{"generation":"g","headSeq":1,"mode":"changes","removals":[]}"#
    );
}

#[test]
fn project_update_payload_roundtrip() {
    let p = json!({"kind":"upsert","project":project("a","A")});
    assert_eq!(ProjectUpdate::from_payload(&p).unwrap().to_payload(), p);
    assert!(ProjectUpdate::from_payload(&json!({"kind":"x"})).is_none());
}

// Golden vectors from WP0 (fixtures/p4/directory/*.json). Skips with a note when absent.
#[test]
fn golden_vectors() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/p4/directory");
    let Ok(rd) = std::fs::read_dir(&dir) else {
        eprintln!(
            "SKIP golden_vectors: {} missing (WP0 not landed)",
            dir.display()
        );
        return;
    };
    let mut files: Vec<_> = rd
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    files.sort();
    if files.is_empty() {
        eprintln!("SKIP golden_vectors: no *.json in {}", dir.display());
        return;
    }
    for path in files {
        let doc: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        run_vector(&path, &doc);
    }
}

/// Vector shape (documented for WP0): `{generation, steps:[{op, ...args, expect}]}` where `op` is
/// one of synchronize_projects|synchronize_workspaces|synchronize_agents|sequence_project_update|
/// sequence_workspace_update|sequence_agent_update, `cursor` is `{generation?,afterSeq?}`,
/// `snapshot` is an array, and `expect` is the exact JSON result.
fn run_vector(path: &std::path::Path, doc: &Value) {
    let (Some(generation), Some(steps)) = (doc["generation"].as_str(), doc["steps"].as_array())
    else {
        eprintln!(
            "SKIP {}: unrecognized vector shape (no generation/steps)",
            path.display()
        );
        return;
    };
    let s = DirectorySync::new(generation);
    for (i, step) in steps.iter().enumerate() {
        let op = step["op"].as_str().unwrap_or_default();
        let cur = DirectorySyncCursor {
            generation: step["cursor"]["generation"].as_str().map(String::from),
            after_seq: step["cursor"]["afterSeq"].as_i64(),
        };
        let bg = step["includeBackground"].as_bool().unwrap_or(false);
        let snap = || step["snapshot"].as_array().cloned().unwrap_or_default();
        let inc = step["includeSequence"].as_bool().unwrap_or(true);
        let got = match op {
            "synchronize_projects" => to_json(&s.synchronize_projects(snap(), &cur)),
            "synchronize_workspaces" => to_json(&s.synchronize_workspaces(snap(), &cur, bg)),
            "synchronize_agents" => to_json(&s.synchronize_agents(snap(), &cur, bg)),
            "sequence_project_update" => {
                let u = ProjectUpdate::from_payload(&step["update"]).expect("project update");
                s.sequence_project_update(&u, inc)
            }
            "sequence_workspace_update" | "sequence_agent_update" => {
                let value = (!step["value"].is_null()).then(|| step["value"].clone());
                let id = step["id"].as_str().unwrap_or_default();
                let payload = step["payload"].clone();
                if op == "sequence_workspace_update" {
                    s.sequence_workspace_update(payload, value, id, inc, bg)
                } else {
                    s.sequence_agent_update(payload, value, id, inc, bg)
                }
            }
            other => panic!("{}: step {i}: unknown op {other}", path.display()),
        };
        assert_eq!(got, step["expect"], "{} step {i} ({op})", path.display());
    }
}
