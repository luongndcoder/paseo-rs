//! Generates `$OUT_DIR/tables.rs` from `tables/{inbound,outbound}.json`.
//!
//! The JSON files are extracted from the TS `operation-permissions.ts` by
//! `scripts/extract_tables.py`. Entries are emitted sorted by message type so lookups can
//! binary-search. Unknown permission names or duplicate keys fail the build.

use std::{env, fmt::Write as _, fs, path::PathBuf};

const PERMISSIONS: [(&str, &str); 9] = [
    ("daemon.read", "DaemonRead"),
    ("daemon.manage", "DaemonManage"),
    ("tunnel.manage", "TunnelManage"),
    ("access.manage", "AccessManage"),
    ("workspace.read", "WorkspaceRead"),
    ("workspace.write", "WorkspaceWrite"),
    ("workspace.manage", "WorkspaceManage"),
    ("automation.manage", "AutomationManage"),
    ("hub.execute", "HubExecute"),
];

fn perm(name: &str) -> String {
    let (_, v) = PERMISSIONS
        .iter()
        .find(|(n, _)| *n == name)
        .unwrap_or_else(|| panic!("unknown permission {name}"));
    format!("Permission::{v}")
}

fn requirement(v: &serde_json::Value) -> String {
    if v == "always" {
        return "Requirement::Always".into();
    }
    if let Some(p) = v.get("one").and_then(|p| p.as_str()) {
        return format!("Requirement::One({})", perm(p));
    }
    if let Some(a) = v.get("any").and_then(|a| a.as_array()) {
        let items: Vec<String> = a.iter().map(|p| perm(p.as_str().expect("str"))).collect();
        return format!("Requirement::Any(&[{}])", items.join(", "));
    }
    panic!("bad requirement {v}");
}

fn table(name: &str, file: &str) -> String {
    let path = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap())
        .join("tables")
        .join(file);
    println!("cargo:rerun-if-changed={}", path.display());
    let raw = fs::read_to_string(&path).unwrap();
    let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let mut rows: Vec<(String, String)> = v
        .as_array()
        .expect("array")
        .iter()
        .map(|e| (e[0].as_str().expect("key").to_owned(), requirement(&e[1])))
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    for w in rows.windows(2) {
        assert!(w[0].0 != w[1].0, "duplicate key {}", w[0].0);
    }
    let mut out = format!("pub(crate) static {name}: &[(&str, Requirement)] = &[\n");
    for (k, r) in rows {
        writeln!(out, "    ({k:?}, {r}),").unwrap();
    }
    out.push_str("];\n");
    out
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let mut code = String::new();
    code.push_str(&table("INBOUND", "inbound.json"));
    code.push_str(&table("OUTBOUND", "outbound.json"));
    let dest = PathBuf::from(env::var("OUT_DIR").unwrap()).join("tables.rs");
    fs::write(dest, code).unwrap();
}
