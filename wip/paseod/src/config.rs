//! Edge configuration. Resolves the same inputs the TS daemon does, with the same
//! precedence (`config.ts` `resolveListenAddress`, `resolveRelayConfig`):
//! CLI/env first, then `$PASEO_HOME/config.json` (read-only, TS is its only writer), then defaults.

use std::net::{SocketAddr, ToSocketAddrs};
use std::path::PathBuf;
use std::time::Duration;

use crate::error::{EdgeError, Result};

/// Default outbound high-water per physical socket. TS enforces 64 MiB
/// (`websocket/physical-socket.ts:4`); `docs/plan.md` says 8 MiB. 64 MiB keeps parity
/// until the open question in EDGE-SPEC.md is resolved. Override with `PASEO_EDGE_OUTBOUND_HIGH_WATER_BYTES`.
pub const DEFAULT_OUTBOUND_HIGH_WATER_BYTES: usize = 64 * 1024 * 1024;
/// Application-lease length (`physical-socket.ts:8`).
pub const APPLICATION_SOCKET_LEASE: Duration = Duration::from_millis(45_000);
/// Lease sweep interval (`physical-socket.ts:9`).
pub const APPLICATION_SOCKET_LEASE_CHECK: Duration = Duration::from_millis(10_000);
/// Largest accepted inbound message (`protocol/src/websocket-message-limit.ts:2`).
pub const MAX_WEBSOCKET_MESSAGE_BYTES: usize = 100 * 1024 * 1024;
/// Largest encrypted relay frame (`websocket-message-limit.ts:5`).
pub const MAX_RELAY_WEBSOCKET_MESSAGE_BYTES: usize = 32 * 1024 * 1024;

/// Where the edge listens (`PASEO_LISTEN` grammar: `host:port`, `/path`, `unix:///path`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListenAddr {
    Tcp(SocketAddr),
    Unix(PathBuf),
}

#[derive(Debug, Clone)]
pub struct Limits {
    pub outbound_high_water_bytes: usize,
    pub lease: Duration,
    pub lease_check_interval: Duration,
    pub max_message_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            outbound_high_water_bytes: DEFAULT_OUTBOUND_HIGH_WATER_BYTES,
            lease: APPLICATION_SOCKET_LEASE,
            lease_check_interval: APPLICATION_SOCKET_LEASE_CHECK,
            max_message_bytes: MAX_WEBSOCKET_MESSAGE_BYTES,
        }
    }
}

/// Worker crash policy (EDGE-SPEC open question 5): respawn with exponential backoff
/// (`base_backoff * 2^(n-1)`, capped at `max_backoff`), `Failed` after `max_crashes`
/// crashes inside `crash_window`.
#[derive(Debug, Clone)]
pub struct RestartPolicy {
    pub base_backoff: Duration,
    pub max_backoff: Duration,
    pub crash_window: Duration,
    pub max_crashes: usize,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self {
            base_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(30),
            crash_window: Duration::from_secs(60),
            max_crashes: 5,
        }
    }
}

/// How to launch the TS worker (`backend` module consumes this).
#[derive(Debug, Clone)]
pub struct BackendConfig {
    /// argv of the TS worker, e.g. `["node", "<dist>/server/server/daemon-worker.js"]`.
    /// From `PASEO_EDGE_BACKEND_CMD` (JSON array of strings).
    pub command: Vec<String>,
    /// Extra env for the worker; the edge always adds the `internal::ENV_*` vars on top.
    pub env: Vec<(String, String)>,
    /// Give up if the worker has not announced `paseo:ready` in this time.
    pub ready_timeout: Duration,
    /// Fixed internal port instead of a free one picked per spawn. For tests and debugging.
    pub internal_port: Option<u16>,
    pub restart: RestartPolicy,
    /// SIGTERM -> SIGKILL grace on shutdown.
    pub shutdown_grace: Duration,
}

impl BackendConfig {
    pub fn new(command: Vec<String>) -> Self {
        Self {
            command,
            env: Vec::new(),
            ready_timeout: Duration::from_secs(60),
            internal_port: None,
            restart: RestartPolicy::default(),
            shutdown_grace: Duration::from_secs(10),
        }
    }
}

/// Relay settings the edge needs. Source of truth is TS config; edge only reads it.
#[derive(Debug, Clone)]
pub struct RelayConfig {
    pub enabled: bool,
    /// `host:port`, default `relay.paseo.sh:443`.
    pub endpoint: String,
    /// Default: `endpoint == "relay.paseo.sh:443"`.
    pub use_tls: bool,
}

/// Static web UI served by the edge itself instead of through the reverse proxy.
///
/// Opt-in (`PASEO_EDGE_SERVE_WEB_UI`), because the TS chain puts the service-proxy host
/// classifier, Host allowlist and CORS in front of the web UI and the edge skips all three
/// for the files it serves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebUiConfig {
    pub dist_dir: PathBuf,
    /// Daemon label injected into `index.html` (`host-name.ts` `getHostName`).
    pub label: String,
}

#[derive(Debug, Clone)]
pub struct EdgeConfig {
    pub listen: ListenAddr,
    /// `PASEO_HOME`, default `~/.paseo`.
    pub paseo_home: PathBuf,
    /// `Some` only when the edge serves the web UI itself. `None`: TS serves it behind the proxy.
    pub web_ui: Option<WebUiConfig>,
    pub limits: Limits,
    pub backend: BackendConfig,
    pub relay: RelayConfig,
    /// `PASEO_EDGE_ATTACH` (`host:port`) + `PASEO_EDGE_TOKEN`: front an already-running daemon
    /// instead of spawning a worker. For in-process test daemons and debugging.
    pub attach: Option<AttachConfig>,
}

#[derive(Debug, Clone)]
pub struct AttachConfig {
    pub addr: std::net::SocketAddr,
    pub token: String,
}

const DEFAULT_PORT: &str = "6767";
const DEFAULT_RELAY_ENDPOINT: &str = "relay.paseo.sh:443";

fn parse_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn expand_home(input: &str, env: &dyn Fn(&str) -> Option<String>) -> PathBuf {
    let home = env("HOME").or_else(|| env("USERPROFILE"));
    match (input, home) {
        ("~", Some(h)) => PathBuf::from(h),
        (i, Some(h)) if i.starts_with("~/") => PathBuf::from(h).join(&i[2..]),
        _ => PathBuf::from(input),
    }
}

fn absolutize(path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(&path))
            .unwrap_or(path)
    }
}

fn read_persisted(paseo_home: &std::path::Path) -> Result<serde_json::Value> {
    let path = paseo_home.join("config.json");
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(serde_json::Value::Null)
        }
        Err(err) => {
            return Err(EdgeError::Config(format!(
                "cannot read {}: {err}",
                path.display()
            )))
        }
    };
    serde_json::from_str(raw.trim_start_matches('\u{feff}'))
        .map_err(|err| EdgeError::Config(format!("invalid {}: {err}", path.display())))
}

fn jstr<'a>(v: &'a serde_json::Value, path: &[&str]) -> Option<&'a str> {
    path.iter()
        .try_fold(v, |acc, key| acc.get(key))
        .and_then(|x| x.as_str())
}

fn jbool(v: &serde_json::Value, path: &[&str]) -> Option<bool> {
    path.iter()
        .try_fold(v, |acc, key| acc.get(key))
        .and_then(|x| x.as_bool())
}

fn env_u64(env: &dyn Fn(&str) -> Option<String>, key: &str) -> Result<Option<u64>> {
    match env(key) {
        None => Ok(None),
        Some(raw) => raw
            .trim()
            .parse::<u64>()
            .map(Some)
            .map_err(|_| EdgeError::Config(format!("{key} must be a non-negative integer"))),
    }
}

#[cfg(unix)]
fn system_hostname() -> Option<String> {
    nix::unistd::gethostname()
        .ok()
        .map(|h| h.to_string_lossy().into_owned())
}

#[cfg(not(unix))]
fn system_hostname() -> Option<String> {
    std::env::var("COMPUTERNAME").ok()
}

/// Mirrors `host-name.ts`: macOS uses `scutil --get ComputerName`, others the hostname.
fn daemon_label() -> String {
    if cfg!(target_os = "macos") {
        if let Ok(out) = std::process::Command::new("/usr/sbin/scutil")
            .args(["--get", "ComputerName"])
            .output()
        {
            let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if out.status.success() && !name.is_empty() {
                return name;
            }
        }
    }
    system_hostname().unwrap_or_else(|| "localhost".to_string())
}

impl EdgeConfig {
    /// Builds config from an env lookup (injectable for tests).
    ///
    /// Contract: pure; no filesystem writes; reads `$PASEO_HOME/config.json` only to
    /// fill `daemon.listen`, `daemon.relay.*` and `features.webUi.*` when env is unset.
    /// Errors are [`EdgeError::Config`] with a message safe to print.
    pub fn from_env(env: &dyn Fn(&str) -> Option<String>) -> Result<Self> {
        let home_raw = env("PASEO_HOME").unwrap_or_else(|| "~/.paseo".to_string());
        let paseo_home = absolutize(expand_home(&home_raw, env));
        let persisted = read_persisted(&paseo_home)?;

        let listen_raw = env("PASEO_LISTEN")
            .or_else(|| jstr(&persisted, &["daemon", "listen"]).map(str::to_owned))
            .unwrap_or_else(|| {
                format!(
                    "127.0.0.1:{}",
                    env("PORT").unwrap_or_else(|| DEFAULT_PORT.into())
                )
            });
        let listen = match parse_listen(&listen_raw)? {
            ListenAddr::Unix(path) => ListenAddr::Unix(expand_home(&path.to_string_lossy(), env)),
            tcp => tcp,
        };

        let mut limits = Limits::default();
        if let Some(bytes) = env_u64(env, "PASEO_EDGE_OUTBOUND_HIGH_WATER_BYTES")? {
            limits.outbound_high_water_bytes = bytes as usize;
        }

        let attach = match env("PASEO_EDGE_ATTACH") {
            Some(raw) => {
                let addr = raw
                    .trim()
                    .parse()
                    .map_err(|_| EdgeError::Config("PASEO_EDGE_ATTACH must be ip:port".into()))?;
                let token = env("PASEO_EDGE_TOKEN")
                    .filter(|t| !t.is_empty())
                    .ok_or_else(|| {
                        EdgeError::Config("PASEO_EDGE_ATTACH requires PASEO_EDGE_TOKEN".into())
                    })?;
                Some(AttachConfig { addr, token })
            }
            None => None,
        };
        let command: Vec<String> = if attach.is_some() {
            Vec::new()
        } else {
            let command_raw = env("PASEO_EDGE_BACKEND_CMD").ok_or_else(|| {
                EdgeError::Config(
                    "PASEO_EDGE_BACKEND_CMD (JSON argv of the TS worker) is required".into(),
                )
            })?;
            let command: Vec<String> = serde_json::from_str(&command_raw).map_err(|_| {
                EdgeError::Config("PASEO_EDGE_BACKEND_CMD must be a JSON array of strings".into())
            })?;
            if command.is_empty() || command[0].is_empty() {
                return Err(EdgeError::Config("PASEO_EDGE_BACKEND_CMD is empty".into()));
            }
            command
        };
        let mut backend = BackendConfig::new(command);
        if let Some(ms) = env_u64(env, "PASEO_EDGE_READY_TIMEOUT_MS")? {
            backend.ready_timeout = Duration::from_millis(ms);
        }
        if let Some(ms) = env_u64(env, "PASEO_EDGE_SHUTDOWN_GRACE_MS")? {
            backend.shutdown_grace = Duration::from_millis(ms);
        }

        // Relay: same precedence as `resolveRelayConfig`. Absent everywhere, relay is on
        // (COMPAT relayOptInDefault in TS).
        let persisted_enabled = jbool(&persisted, &["daemon", "relay", "enabled"]);
        let enabled = env("PASEO_RELAY_ENABLED")
            .and_then(|v| parse_bool(&v))
            .or(persisted_enabled)
            .unwrap_or(true);
        let endpoint = env("PASEO_RELAY_ENDPOINT")
            .or_else(|| jstr(&persisted, &["daemon", "relay", "endpoint"]).map(str::to_owned))
            .unwrap_or_else(|| DEFAULT_RELAY_ENDPOINT.to_string());
        let use_tls = match env("PASEO_RELAY_USE_TLS") {
            Some(v) => parse_bool(&v).unwrap_or(false),
            None => jbool(&persisted, &["daemon", "relay", "useTls"])
                .unwrap_or(endpoint == DEFAULT_RELAY_ENDPOINT),
        };

        // Web UI: enabled/dist resolved like `resolveWebUiConfig`; the edge serves it only on opt-in.
        let ui_enabled = env("PASEO_WEB_UI_ENABLED")
            .and_then(|v| parse_bool(&v))
            .or_else(|| jbool(&persisted, &["features", "webUi", "enabled"]))
            .unwrap_or(false);
        let edge_serves = env("PASEO_EDGE_SERVE_WEB_UI")
            .and_then(|v| parse_bool(&v))
            .unwrap_or(false);
        let dist_raw = env("PASEO_WEB_UI_DIST_DIR")
            .or_else(|| jstr(&persisted, &["features", "webUi", "distDir"]).map(str::to_owned));
        let web_ui = match (
            ui_enabled && edge_serves,
            dist_raw.as_deref().map(str::trim),
        ) {
            (true, Some(dist)) if !dist.is_empty() => {
                let expanded = expand_home(dist, env);
                let dist_dir = if expanded.is_absolute() {
                    expanded
                } else {
                    paseo_home.join(expanded)
                };
                Some(WebUiConfig {
                    dist_dir,
                    label: env("PASEO_EDGE_WEB_UI_LABEL").unwrap_or_else(daemon_label),
                })
            }
            _ => None,
        };

        Ok(Self {
            listen,
            paseo_home,
            web_ui,
            limits,
            backend,
            relay: RelayConfig {
                enabled,
                endpoint,
                use_tls,
            },
            attach,
        })
    }
}

/// Parses a `PASEO_LISTEN` value (`bootstrap.ts` `parseListenString`).
///
/// `127.0.0.1:6767` -> Tcp; `6767` -> Tcp on 127.0.0.1; `/tmp/p.sock` or `unix:///tmp/p.sock`
/// -> Unix. Windows named pipes (`\\.\pipe\...`, `pipe://`) return [`EdgeError::Config`] in P2.
pub fn parse_listen(value: &str) -> Result<ListenAddr> {
    if value.starts_with("\\\\.\\pipe\\") || value.starts_with("pipe://") {
        return Err(EdgeError::Config(
            "named pipe listen targets are not supported by the edge yet".into(),
        ));
    }
    if let Some(path) = value.strip_prefix("unix://") {
        return Ok(ListenAddr::Unix(PathBuf::from(path)));
    }
    let bytes = value.as_bytes();
    if bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'\\' {
        return Err(EdgeError::Config(format!(
            "invalid listen string (Windows path is not a valid listen target): {value}"
        )));
    }
    if value.starts_with('/') || value.starts_with('~') {
        return Ok(ListenAddr::Unix(PathBuf::from(value)));
    }
    let trimmed = value.trim();
    if !trimmed.is_empty() && trimmed.bytes().all(|b| b.is_ascii_digit()) {
        let port = parse_port(trimmed, value)?;
        return Ok(ListenAddr::Tcp(SocketAddr::from(([127, 0, 0, 1], port))));
    }
    if let Some(idx) = value.rfind(':') {
        let (host, port_str) = (&value[..idx], &value[idx + 1..]);
        let port = parse_port(port_str, value)?;
        let host = host
            .strip_prefix('[')
            .and_then(|h| h.strip_suffix(']'))
            .unwrap_or(host);
        let host = if host.is_empty() { "127.0.0.1" } else { host };
        let addr = (host, port)
            .to_socket_addrs()
            .map_err(|err| EdgeError::Config(format!("cannot resolve listen host {host}: {err}")))?
            .next()
            .ok_or_else(|| EdgeError::Config(format!("listen host {host} resolved to nothing")))?;
        return Ok(ListenAddr::Tcp(addr));
    }
    Err(EdgeError::Config(format!("invalid listen string: {value}")))
}

fn parse_port(port: &str, whole: &str) -> Result<u16> {
    port.trim()
        .parse::<u16>()
        .map_err(|_| EdgeError::Config(format!("invalid port in listen string: {whole}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| map.get(k).cloned()
    }

    #[test]
    fn listen_grammar() {
        assert_eq!(
            parse_listen("127.0.0.1:6767").unwrap(),
            ListenAddr::Tcp("127.0.0.1:6767".parse().unwrap())
        );
        assert_eq!(
            parse_listen("7000").unwrap(),
            ListenAddr::Tcp("127.0.0.1:7000".parse().unwrap())
        );
        assert_eq!(
            parse_listen("[::1]:7000").unwrap(),
            ListenAddr::Tcp("[::1]:7000".parse().unwrap())
        );
        assert_eq!(
            parse_listen("0.0.0.0:80").unwrap(),
            ListenAddr::Tcp("0.0.0.0:80".parse().unwrap())
        );
        assert_eq!(
            parse_listen("/tmp/p.sock").unwrap(),
            ListenAddr::Unix("/tmp/p.sock".into())
        );
        assert_eq!(
            parse_listen("unix:///tmp/p.sock").unwrap(),
            ListenAddr::Unix("/tmp/p.sock".into())
        );
        assert!(parse_listen(r"\\.\pipe\paseo").is_err());
        assert!(parse_listen(r"C:\x").is_err());
        assert!(parse_listen("host:notaport").is_err());
        assert!(parse_listen("nonsense").is_err());
    }

    #[test]
    fn precedence_env_then_persisted_then_default() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.json"),
            r#"{"daemon":{"listen":"127.0.0.1:7001","relay":{"enabled":false,"endpoint":"r.example:1"}}}"#,
        )
        .unwrap();
        let home = dir.path().to_str().unwrap();
        let cmd = r#"["node","w.js"]"#;

        let cfg = EdgeConfig::from_env(&env_of(&[
            ("PASEO_HOME", home),
            ("PASEO_EDGE_BACKEND_CMD", cmd),
        ]))
        .unwrap();
        assert_eq!(
            cfg.listen,
            ListenAddr::Tcp("127.0.0.1:7001".parse().unwrap())
        );
        assert!(!cfg.relay.enabled);
        assert_eq!(cfg.relay.endpoint, "r.example:1");
        assert!(!cfg.relay.use_tls);

        let cfg = EdgeConfig::from_env(&env_of(&[
            ("PASEO_HOME", home),
            ("PASEO_EDGE_BACKEND_CMD", cmd),
            ("PASEO_LISTEN", "127.0.0.1:7002"),
            ("PASEO_RELAY_ENABLED", "yes"),
        ]))
        .unwrap();
        assert_eq!(
            cfg.listen,
            ListenAddr::Tcp("127.0.0.1:7002".parse().unwrap())
        );
        assert!(cfg.relay.enabled);

        let empty = tempfile::tempdir().unwrap();
        let cfg = EdgeConfig::from_env(&env_of(&[
            ("PASEO_HOME", empty.path().to_str().unwrap()),
            ("PASEO_EDGE_BACKEND_CMD", cmd),
            ("PORT", "9000"),
        ]))
        .unwrap();
        assert_eq!(
            cfg.listen,
            ListenAddr::Tcp("127.0.0.1:9000".parse().unwrap())
        );
        assert!(cfg.relay.enabled);
        assert!(cfg.relay.use_tls);
        assert!(cfg.web_ui.is_none());
    }

    #[test]
    fn backend_command_is_required_and_validated() {
        let empty = tempfile::tempdir().unwrap();
        let home = empty.path().to_str().unwrap();
        assert!(EdgeConfig::from_env(&env_of(&[("PASEO_HOME", home)])).is_err());
        assert!(EdgeConfig::from_env(&env_of(&[
            ("PASEO_HOME", home),
            ("PASEO_EDGE_BACKEND_CMD", "node w.js")
        ]))
        .is_err());
        assert!(EdgeConfig::from_env(&env_of(&[
            ("PASEO_HOME", home),
            ("PASEO_EDGE_BACKEND_CMD", "[]")
        ]))
        .is_err());
    }

    #[test]
    fn web_ui_needs_opt_in_and_dist_dir() {
        let empty = tempfile::tempdir().unwrap();
        let home = empty.path().to_str().unwrap();
        let base = [
            ("PASEO_HOME", home),
            ("PASEO_EDGE_BACKEND_CMD", r#"["x"]"#),
            ("PASEO_WEB_UI_ENABLED", "1"),
            ("PASEO_WEB_UI_DIST_DIR", "dist"),
            ("PASEO_EDGE_WEB_UI_LABEL", "box"),
        ];
        let cfg = EdgeConfig::from_env(&env_of(&base)).unwrap();
        assert!(cfg.web_ui.is_none());

        let mut opted = base.to_vec();
        opted.push(("PASEO_EDGE_SERVE_WEB_UI", "true"));
        let cfg = EdgeConfig::from_env(&env_of(&opted)).unwrap();
        let ui = cfg.web_ui.unwrap();
        assert_eq!(ui.dist_dir, cfg.paseo_home.join("dist"));
        assert_eq!(ui.label, "box");
    }
}
