//! Every real `$PASEO_HOME/agents/**` and `projects/*` file must round-trip through the typed
//! records to the identical JSON text. Reads `PASEO_HOME` (default `~/.paseo`), read-only; the
//! test passes vacuously when the directory does not exist.

use paseo_store::project_registry::ProjectRecord;
use paseo_store::workspace_registry::WorkspaceRecord;
use paseo_store::StoredAgentRecord;
use std::path::{Path, PathBuf};

fn home() -> Option<PathBuf> {
    let dir = std::env::var_os("PASEO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".paseo")))?;
    dir.is_dir().then_some(dir)
}

fn json_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            json_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "json") {
            out.push(path);
        }
    }
}

#[test]
fn agent_records_round_trip_byte_for_byte() {
    let Some(home) = home() else { return };
    let mut files = Vec::new();
    json_files(&home.join("agents"), &mut files);
    let (mut checked, mut skipped) = (0, 0);
    let mut mismatches = Vec::new();
    for file in files {
        let text = std::fs::read_to_string(&file).unwrap();
        let Ok(record) = serde_json::from_str::<StoredAgentRecord>(&text) else {
            skipped += 1; // TS skips invalid records the same way
            continue;
        };
        let out = serde_json::to_string_pretty(&record).unwrap();
        let before: serde_json::Value = serde_json::from_str(&text).unwrap();
        let after: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(before, after, "value changed: {}", file.display());
        if out != text {
            mismatches.push(file.display().to_string());
        }
        checked += 1;
    }
    eprintln!("agent records: checked={checked} skipped_invalid={skipped}");
    assert!(
        mismatches.is_empty(),
        "text differs (key order?) for {} files, first: {:?}",
        mismatches.len(),
        mismatches.first()
    );
}

#[test]
fn registry_files_round_trip_byte_for_byte() {
    let Some(home) = home() else { return };
    let projects = home.join("projects").join("projects.json");
    if let Ok(text) = std::fs::read_to_string(&projects) {
        let records: Vec<ProjectRecord> = serde_json::from_str(&text).unwrap();
        assert_eq!(serde_json::to_string_pretty(&records).unwrap(), text);
    }
    let workspaces = home.join("projects").join("workspaces.json");
    if let Ok(text) = std::fs::read_to_string(&workspaces) {
        let records: Vec<WorkspaceRecord> = serde_json::from_str(&text).unwrap();
        assert_eq!(serde_json::to_string_pretty(&records).unwrap(), text);
    }
}
