//! Ports of `packages/server/src/server/authorization/index.test.ts`, plus table checks against
//! the protocol schemas and (when present) the WP0 golden dump.

use paseo_authz::*;
use serde_json::Value;
use std::{collections::BTreeSet, path::PathBuf};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn schema_types(file: &str) -> BTreeSet<String> {
    let p = root().join("crates/paseo-protocol/schema").join(file);
    let v: Value = serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
    v["oneOf"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| {
            o["properties"]["type"]["const"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect()
}

fn outbound(ty: &str) -> OutboundRef<'_> {
    // TS test helper: `status` carries agent_create_failed.
    OutboundRef {
        ty,
        status: (ty == "status").then_some("agent_create_failed"),
    }
}

fn hub() -> SessionAuthz {
    SessionAuthz::new([Permission::HubExecute].into_iter().collect())
}

// --- ported from index.test.ts ---------------------------------------------------------

#[test]
fn owner_authority_covers_every_session_operation() {
    let a = SessionAuthz::owner();
    for t in schema_types("session-inbound.json") {
        assert!(a.allows_inbound(&t), "inbound {t}");
    }
    for t in schema_types("session-outbound.json") {
        assert!(a.allows_outbound(&outbound(&t)), "outbound {t}");
    }
}

#[test]
fn semantic_permissions_authorize_operations_instead_of_namespaces() {
    let a = hub();
    for t in [
        "hub.execution.agent.create.request",
        "get_providers_snapshot_request",
        "refresh_providers_snapshot_request",
    ] {
        assert!(a.allows_inbound(t), "{t}");
    }
    for t in [
        "hub.execution.agent.update",
        "get_providers_snapshot_response",
        "providers_snapshot_update",
        "refresh_providers_snapshot_response",
    ] {
        assert!(a.allows_outbound(&outbound(t)), "{t}");
    }
    for t in [
        "get_daemon_config_request",
        "provider_diagnostic_request",
        "ping",
        "hub.management.daemon.get_status.request",
    ] {
        assert!(!a.allows_inbound(t), "{t}");
    }
}

#[test]
fn hub_can_operate_agents_and_recover_workspaces_without_daemon_administration() {
    let a = hub();
    for t in [
        "create_agent_request",
        "workspace.title.set.request",
        "fetch_agents_request",
        "fetch_agent_request",
        "agent.timeline.set_subscription.request",
        "send_agent_message_request",
        "workspace.recovery.inspect.request",
        "workspace.recovery.restore.request",
        "archive_workspace_request",
        "cancel_agent_request",
    ] {
        assert!(a.allows_inbound(t), "{t}");
    }
    for t in [
        "status",
        "agent_update",
        "agent_stream",
        "workspace_update",
        "rpc_error",
        "workspace.title.set.response",
        "fetch_agents_response",
        "fetch_agent_response",
        "agent.timeline.set_subscription.response",
        "send_agent_message_response",
        "workspace.recovery.inspect.response",
        "workspace.recovery.restore.response",
        "archive_workspace_response",
        "cancel_agent_response",
    ] {
        assert!(a.allows_outbound(&outbound(t)), "{t}");
    }
    for t in [
        "restart_server_request",
        "terminal_input",
        "hub.management.daemon.permissions.update.request",
    ] {
        assert!(!a.allows_inbound(t), "{t}");
    }
    assert!(!a.allows_outbound(&OutboundRef {
        ty: "status",
        status: Some("shutdown_requested")
    }));
    a.replace(PermSet::EMPTY);
    assert!(!a.allows_inbound("send_agent_message_request"));
    assert!(!a.allows_outbound(&outbound("agent_update")));
}

#[test]
fn correlated_authorization_errors_can_always_be_emitted() {
    let a = SessionAuthz::new(PermSet::EMPTY);
    assert!(a.allows_outbound(&outbound("rpc_error")));
}

#[test]
fn legacy_hub_authority_is_translated_at_one_boundary() {
    assert_eq!(
        legacy_hub_scopes(&["hub.execution.*"]),
        vec![Permission::HubExecute]
    );
    assert_eq!(legacy_hub_scopes(&["*"]), Vec::<Permission>::new());
}

#[test]
fn permission_names_are_semantic() {
    for p in DAEMON_PERMISSIONS {
        assert!(!p.as_str().contains('*') && !p.as_str().contains("request"));
    }
}

#[test]
fn permission_parsing_validates_and_deduplicates() {
    assert_eq!(
        parse_daemon_permissions(&["hub.execute", "hub.execute"]),
        Ok(vec![Permission::HubExecute])
    );
    assert_eq!(
        parse_daemon_permissions(&["hub.execution.*"]),
        Err(InvalidPermission)
    );
    assert_eq!(InvalidPermission.to_string(), "Invalid daemon permission");
}

// --- Rust-specific ---------------------------------------------------------------------

#[test]
fn tables_cover_exactly_the_protocol_message_types() {
    let inb: BTreeSet<String> = inbound_table()
        .iter()
        .map(|(k, _)| (*k).to_owned())
        .collect();
    let out: BTreeSet<String> = outbound_table()
        .iter()
        .map(|(k, _)| (*k).to_owned())
        .collect();
    assert_eq!(
        inb,
        schema_types("session-inbound.json"),
        "inbound table vs schema"
    );
    assert_eq!(
        out,
        schema_types("session-outbound.json"),
        "outbound table vs schema"
    );
}

#[test]
fn unmapped_types_deny() {
    assert!(required_inbound("no_such_type").is_none());
    assert!(required_outbound(&OutboundRef::new("no_such_type")).is_none());
    assert!(!SessionAuthz::owner().allows_inbound("no_such_type"));
    assert!(!SessionAuthz::owner().allows_outbound(&OutboundRef::new("no_such_type")));
}

#[test]
fn outbound_status_special_case() {
    let none = SessionAuthz::new(PermSet::EMPTY);
    let write = SessionAuthz::new([Permission::WorkspaceWrite].into_iter().collect());
    let read = SessionAuthz::new([Permission::DaemonRead].into_iter().collect());
    for s in ["agent_created", "agent_create_failed"] {
        let m = OutboundRef {
            ty: "status",
            status: Some(s),
        };
        assert_eq!(
            required_outbound(&m),
            Some(Requirement::Any(&[
                Permission::WorkspaceWrite,
                Permission::HubExecute
            ]))
        );
        assert!(write.allows_outbound(&m));
        assert!(hub().allows_outbound(&m));
        assert!(!read.allows_outbound(&m));
        assert!(!none.allows_outbound(&m));
    }
    for status in [Some("other"), None] {
        let m = OutboundRef {
            ty: "status",
            status,
        };
        assert_eq!(
            required_outbound(&m),
            Some(Requirement::One(Permission::DaemonRead))
        );
        assert!(read.allows_outbound(&m));
        assert!(!write.allows_outbound(&m));
    }
    let v: Value = serde_json::json!({"type":"status","payload":{"status":"agent_created"}});
    assert_eq!(
        OutboundRef::from_value(&v).unwrap().status,
        Some("agent_created")
    );
}

#[test]
fn replace_is_atomic_and_reports_previous() {
    let a = SessionAuthz::owner();
    let prev = a.replace([Permission::DaemonRead].into_iter().collect());
    assert_eq!(prev, PermSet::OWNER);
    assert!(lost_workspace_write(prev, a.permissions()));
    assert!(!lost_workspace_write(a.permissions(), PermSet::EMPTY));
    assert_eq!(
        a.permissions().iter().collect::<Vec<_>>(),
        vec![Permission::DaemonRead]
    );
    assert_eq!(PermSet::OWNER.iter().count(), 9);
}

#[test]
fn requirement_any_needs_one_match() {
    let r = Requirement::Any(&[Permission::WorkspaceRead, Permission::HubExecute]);
    assert!(r.satisfied_by([Permission::HubExecute].into_iter().collect()));
    assert!(!r.satisfied_by([Permission::DaemonRead].into_iter().collect()));
    assert!(!r.satisfied_by(PermSet::EMPTY));
    assert!(Requirement::Always.satisfied_by(PermSet::EMPTY));
}

#[test]
fn denial_error_text() {
    assert_eq!(
        access_denied_error("ping"),
        "Session is not authorized for ping"
    );
}

// --- WP0 golden ------------------------------------------------------------------------

fn golden_path() -> Option<PathBuf> {
    // Deterministic: read_dir order differs per filesystem.
    let p = root().join("fixtures/p4/authz/permissions.json");
    p.is_file().then_some(p)
}

/// Accepts `[{type, requirement: {kind, permissions}}]` (WP0 dump), `{type: req}` or
/// `[[type, req], ...]`, req being the TS form (null | "p" | ["p", ..]).
fn golden_rows(v: &Value) -> Vec<(String, Requirement)> {
    fn req(v: &Value) -> Requirement {
        if let Some(kind) = v.get("kind").and_then(Value::as_str) {
            let ps: Vec<Permission> = v["permissions"]
                .as_array()
                .expect("permissions")
                .iter()
                .map(|p| Permission::parse(p.as_str().unwrap()).expect("perm"))
                .collect();
            return match kind {
                "always" => Requirement::Always,
                "one" => Requirement::One(ps[0]),
                "any" => Requirement::Any(Box::leak(ps.into_boxed_slice())),
                other => panic!("bad requirement kind {other}"),
            };
        }
        match v {
            Value::Null => Requirement::Always,
            Value::String(s) => Requirement::One(Permission::parse(s).expect("perm")),
            Value::Array(a) => {
                let ps: Vec<Permission> = a
                    .iter()
                    .map(|p| Permission::parse(p.as_str().unwrap()).unwrap())
                    .collect();
                Requirement::Any(Box::leak(ps.into_boxed_slice()))
            }
            other => panic!("bad golden requirement {other}"),
        }
    }
    match v {
        Value::Object(m) => m.iter().map(|(k, r)| (k.clone(), req(r))).collect(),
        Value::Array(a) => a
            .iter()
            .map(|e| match e.get("type") {
                Some(t) => (t.as_str().unwrap().to_owned(), req(&e["requirement"])),
                None => (e[0].as_str().unwrap().to_owned(), req(&e[1])),
            })
            .collect(),
        _ => panic!("bad golden table"),
    }
}

#[test]
fn golden_tables_match_ts_dump() {
    let Some(path) = golden_path() else {
        eprintln!(
            "SKIP golden_tables_match_ts_dump: no authz fixture under fixtures/p4 (WP0 pending)"
        );
        return;
    };
    let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    for (key, table) in [("inbound", inbound_table()), ("outbound", outbound_table())] {
        let g = v
            .get(key)
            .or_else(|| v.get(key.to_uppercase() + "_PERMISSION"));
        let Some(g) = g else {
            eprintln!("SKIP {key}: key missing in {}", path.display());
            continue;
        };
        let mut rows = golden_rows(g);
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        let ours: Vec<(String, Requirement)> =
            table.iter().map(|(k, r)| ((*k).to_owned(), *r)).collect();
        assert_eq!(ours, rows, "{key} table differs from {}", path.display());
    }
}
