//! The edge <-> TS backend contract. One place so the Rust side and the TS shim
//! (`packages/server/src/server/edge-backend.ts`, written in a later stage) agree.
//! See EDGE-SPEC.md section "Backend contract".

use serde::{Deserialize, Serialize};

/// Env var set on the TS worker: per-run random secret (base64url, 32 bytes).
/// Presence puts the worker in edge-backend mode.
pub const ENV_EDGE_TOKEN: &str = "PASEO_EDGE_TOKEN";
/// Env var set on the TS worker: the public address the edge listens on, same
/// grammar as `PASEO_LISTEN`. TS uses it for `/api/status.listen`, service-proxy
/// public URLs, CORS fixed origins and the `paseo.pid` `listen` value.
pub const ENV_EDGE_PUBLIC_LISTEN: &str = "PASEO_EDGE_PUBLIC_LISTEN";

/// Request header carrying [`ENV_EDGE_TOKEN`]. Edge sets it on every internal
/// request and WS upgrade; TS only honours the `x-paseo-edge-*` headers below when it matches.
pub const HEADER_TOKEN: &str = "x-paseo-edge-token";
/// `direct` or `relay`. Absent means `direct`.
pub const HEADER_TRANSPORT: &str = "x-paseo-edge-transport";
/// Remote socket address of the physical client (`ip:port`). Absent for relay.
pub const HEADER_REMOTE_ADDR: &str = "x-paseo-edge-remote-addr";
/// Relay `connectionId`, set when transport is `relay`.
pub const HEADER_CONNECTION_ID: &str = "x-paseo-edge-connection-id";

/// Backend route the edge asks whether a `Host` belongs to a registered script service
/// (`?host=<authority>`; answer `{"service":true|false}`). Requires [`HEADER_TOKEN`]. Such hosts
/// own their `/ws`, so the edge tunnels them raw instead of terminating the daemon protocol.
pub const HOST_CLASS_PATH: &str = "/__paseo_edge/host-class";

/// Headers the edge strips from every inbound client request before forwarding,
/// so a client can never forge edge identity.
pub const STRIPPED_CLIENT_HEADERS: [&str; 4] = [
    HEADER_TOKEN,
    HEADER_TRANSPORT,
    HEADER_REMOTE_ADDR,
    HEADER_CONNECTION_ID,
];

/// A TS worker stdout line starting with this prefix is a lifecycle event (JSON after the prefix).
pub const LIFECYCLE_PREFIX: &str = "@@paseo ";

/// Mirror of the supervisor IPC messages (`daemon-worker.ts` `SupervisorLifecycleMessage`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum LifecycleEvent {
    #[serde(rename = "paseo:ready")]
    Ready {
        listen: String,
        #[serde(rename = "serverId")]
        server_id: String,
    },
    #[serde(rename = "paseo:shutdown")]
    Shutdown { reason: Option<String> },
    #[serde(rename = "paseo:restart")]
    Restart { reason: Option<String> },
}

/// Parses one worker stdout line; `None` when it is ordinary log output.
pub fn parse_lifecycle_line(line: &str) -> Option<LifecycleEvent> {
    let json = line.strip_prefix(LIFECYCLE_PREFIX)?;
    serde_json::from_str(json.trim()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ready_line() {
        let line =
            r#"@@paseo {"type":"paseo:ready","listen":"127.0.0.1:41000","serverId":"srv_abc"}"#;
        assert_eq!(
            parse_lifecycle_line(line),
            Some(LifecycleEvent::Ready {
                listen: "127.0.0.1:41000".into(),
                server_id: "srv_abc".into()
            })
        );
    }

    #[test]
    fn ignores_plain_log_lines() {
        assert_eq!(parse_lifecycle_line(r#"{"level":30,"msg":"hi"}"#), None);
        assert_eq!(parse_lifecycle_line("@@paseo not-json"), None);
    }
}
