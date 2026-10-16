//! `hub-relationship.json`: the persisted relationship record (TS `relationship-controller.ts`
//! schemas), its validation, v1 migration, atomic private persistence and quarantine.
//!
//! The record is validated by hand over `serde_json::Value` instead of derived `Deserialize`
//! because the TS zod schemas strip unknown keys, reject `null` for optional keys and run URL
//! refinements; a derive would accept some of those files. Serialization writes keys in the
//! order TS writes them (`version`, `state`, ...).

use std::path::{Path, PathBuf};

use paseo_authz::{legacy_hub_scopes, Permission};
use serde_json::{Map, Value};
use url::Url;

pub const FILE_NAME: &str = "hub-relationship.json";

/// `relationship` of every state except `revoked`.
#[derive(Debug, Clone, PartialEq)]
pub struct Relationship {
    pub daemon_id: String,
    pub idempotency_key: String,
    pub hub_origin: String,
    pub created_at: String,
    pub permissions: Vec<Permission>,
}

/// `relationship` of a revoked record: the idempotency key is dropped.
#[derive(Debug, Clone, PartialEq)]
pub struct SanitizedRelationship {
    pub daemon_id: String,
    pub hub_origin: String,
    pub created_at: String,
    pub permissions: Vec<Permission>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Credential {
    pub secret: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Transport {
    pub web_socket_url: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Identity {
    pub server_id: String,
    pub daemon_public_key: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PendingRecord {
    pub relationship: Relationship,
    pub credential: Credential,
    pub enrollment_token: String,
    pub identity: Identity,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ActiveRecord {
    pub relationship: Relationship,
    pub credential: Credential,
    pub transport: Transport,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DisconnectingRecord {
    pub relationship: Relationship,
    pub credential: Credential,
    pub transport: Option<Transport>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RevokedRecord {
    pub relationship: SanitizedRelationship,
    pub transport: Option<Transport>,
    pub reason: Option<String>,
}

/// `HubRelationshipRecord` (version 2).
#[derive(Debug, Clone, PartialEq)]
pub enum Record {
    Pending(PendingRecord),
    Active(ActiveRecord),
    Disconnecting(DisconnectingRecord),
    Revoked(RevokedRecord),
}

impl Record {
    pub fn state_name(&self) -> &'static str {
        match self {
            Self::Pending(_) => "pending",
            Self::Active(_) => "active",
            Self::Disconnecting(_) => "disconnecting",
            Self::Revoked(_) => "revoked",
        }
    }

    pub fn daemon_id(&self) -> &str {
        match self {
            Self::Pending(r) => &r.relationship.daemon_id,
            Self::Active(r) => &r.relationship.daemon_id,
            Self::Disconnecting(r) => &r.relationship.daemon_id,
            Self::Revoked(r) => &r.relationship.daemon_id,
        }
    }

    pub fn hub_origin(&self) -> &str {
        match self {
            Self::Pending(r) => &r.relationship.hub_origin,
            Self::Active(r) => &r.relationship.hub_origin,
            Self::Disconnecting(r) => &r.relationship.hub_origin,
            Self::Revoked(r) => &r.relationship.hub_origin,
        }
    }

    pub fn permissions(&self) -> &[Permission] {
        match self {
            Self::Pending(r) => &r.relationship.permissions,
            Self::Active(r) => &r.relationship.permissions,
            Self::Disconnecting(r) => &r.relationship.permissions,
            Self::Revoked(r) => &r.relationship.permissions,
        }
    }

    pub fn permissions_mut(&mut self) -> &mut Vec<Permission> {
        match self {
            Self::Pending(r) => &mut r.relationship.permissions,
            Self::Active(r) => &mut r.relationship.permissions,
            Self::Disconnecting(r) => &mut r.relationship.permissions,
            Self::Revoked(r) => &mut r.relationship.permissions,
        }
    }

    pub fn created_at(&self) -> &str {
        match self {
            Self::Pending(r) => &r.relationship.created_at,
            Self::Active(r) => &r.relationship.created_at,
            Self::Disconnecting(r) => &r.relationship.created_at,
            Self::Revoked(r) => &r.relationship.created_at,
        }
    }

    pub fn credential(&self) -> Option<&Credential> {
        match self {
            Self::Pending(r) => Some(&r.credential),
            Self::Active(r) => Some(&r.credential),
            Self::Disconnecting(r) => Some(&r.credential),
            Self::Revoked(_) => None,
        }
    }

    pub fn transport(&self) -> Option<&Transport> {
        match self {
            Self::Pending(_) => None,
            Self::Active(r) => Some(&r.transport),
            Self::Disconnecting(r) => r.transport.as_ref(),
            Self::Revoked(r) => r.transport.as_ref(),
        }
    }

    /// The JSON written to disk (`JSON.stringify` key order).
    pub fn to_value(&self) -> Value {
        let mut map = Map::new();
        map.insert("version".into(), Value::from(2));
        map.insert("state".into(), self.state_name().into());
        match self {
            Self::Pending(r) => {
                map.insert("relationship".into(), relationship_value(&r.relationship));
                map.insert("credential".into(), credential_value(&r.credential));
                map.insert(
                    "enrollment".into(),
                    json_obj([("token", r.enrollment_token.clone().into())]),
                );
                map.insert(
                    "identity".into(),
                    json_obj([
                        ("serverId", r.identity.server_id.clone().into()),
                        (
                            "daemonPublicKey",
                            r.identity.daemon_public_key.clone().into(),
                        ),
                    ]),
                );
            }
            Self::Active(r) => {
                map.insert("relationship".into(), relationship_value(&r.relationship));
                map.insert("credential".into(), credential_value(&r.credential));
                map.insert("transport".into(), transport_value(&r.transport));
            }
            Self::Disconnecting(r) => {
                map.insert("relationship".into(), relationship_value(&r.relationship));
                map.insert("credential".into(), credential_value(&r.credential));
                if let Some(t) = &r.transport {
                    map.insert("transport".into(), transport_value(t));
                }
            }
            Self::Revoked(r) => {
                map.insert(
                    "relationship".into(),
                    json_obj([
                        ("daemonId", r.relationship.daemon_id.clone().into()),
                        ("hubOrigin", r.relationship.hub_origin.clone().into()),
                        ("createdAt", r.relationship.created_at.clone().into()),
                        (
                            "permissions",
                            permissions_value(&r.relationship.permissions),
                        ),
                    ]),
                );
                if let Some(t) = &r.transport {
                    map.insert("transport".into(), transport_value(t));
                }
                if let Some(reason) = &r.reason {
                    map.insert("reason".into(), reason.clone().into());
                }
            }
        }
        Value::Object(map)
    }

    /// `JSON.stringify(record, null, 2)` plus the trailing newline TS appends.
    pub fn to_file_text(&self) -> String {
        let mut text = serde_json::to_string_pretty(&self.to_value()).unwrap_or_default();
        text.push('\n');
        text
    }

    /// Validates `raw` against the current schema, migrating a v1 record first. The flag is
    /// true when the input was a v1 record (TS rewrites those as v2 immediately).
    pub fn parse(raw: &Value) -> Result<(Record, bool), String> {
        let legacy = is_legacy_record(raw);
        let migrated = migrate_legacy_record(raw);
        parse_current(&migrated).map(|record| (record, legacy))
    }
}

pub fn permissions_value(permissions: &[Permission]) -> Value {
    Value::Array(permissions.iter().map(|p| p.as_str().into()).collect())
}

fn json_obj<const N: usize>(entries: [(&str, Value); N]) -> Value {
    Value::Object(
        entries
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v))
            .collect(),
    )
}

fn relationship_value(r: &Relationship) -> Value {
    json_obj([
        ("daemonId", r.daemon_id.clone().into()),
        ("idempotencyKey", r.idempotency_key.clone().into()),
        ("hubOrigin", r.hub_origin.clone().into()),
        ("createdAt", r.created_at.clone().into()),
        ("permissions", permissions_value(&r.permissions)),
    ])
}

fn credential_value(c: &Credential) -> Value {
    json_obj([("secret", c.secret.clone().into())])
}

fn transport_value(t: &Transport) -> Value {
    json_obj([
        ("kind", "direct_websocket".into()),
        ("webSocketUrl", t.web_socket_url.clone().into()),
    ])
}

// ---------------------------------------------------------------------------------------
// URLs

/// `normalizeHubUrl`. Error messages are the TS ones (`new URL` failure is a `TypeError`
/// whose message is `Invalid URL`).
pub fn normalize_hub_url(value: &str) -> Result<String, String> {
    let mut url = Url::parse(value).map_err(|_| "Invalid URL".to_owned())?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err("Hub URL must use HTTP or HTTPS".into());
    }
    if !url.username().is_empty() || url.password().is_some_and(|p| !p.is_empty()) {
        return Err("Hub URL cannot include credentials".into());
    }
    if url.query().is_some_and(|q| !q.is_empty()) || url.fragment().is_some_and(|f| !f.is_empty()) {
        return Err("Hub URL cannot include a query or fragment".into());
    }
    // `url.pathname = url.pathname.replace(/\/$/, "")` re-adds a lone `/` for special schemes;
    // `toString().replace(/\/$/, "")` then strips one more.
    let path = url
        .path()
        .strip_suffix('/')
        .unwrap_or(url.path())
        .to_owned();
    url.set_path(&path);
    let text = url.to_string();
    Ok(text.strip_suffix('/').unwrap_or(&text).to_owned())
}

fn host_with_port(url: &Url) -> String {
    let host = url.host_str().unwrap_or_default();
    match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    }
}

/// The Hub WebSocket must live on the Hub's own origin (`https` pairs with `wss`).
pub fn web_socket_matches_hub_origin(hub_origin: &str, web_socket_url: &str) -> bool {
    let (Ok(hub), Ok(socket)) = (Url::parse(hub_origin), Url::parse(web_socket_url)) else {
        return false;
    };
    let expected = if hub.scheme() == "https" { "wss" } else { "ws" };
    socket.scheme() == expected && host_with_port(&socket) == host_with_port(&hub)
}

/// `TransportSchema` URL rules: ws/wss and no non-empty fragment.
pub fn is_valid_web_socket_url(value: &str) -> bool {
    Url::parse(value)
        .is_ok_and(|u| matches!(u.scheme(), "ws" | "wss") && u.fragment().is_none_or(str::is_empty))
}

// ---------------------------------------------------------------------------------------
// Validation

fn is_legacy_record(value: &Value) -> bool {
    value
        .as_object()
        .and_then(|o| o.get("version"))
        .is_some_and(|v| v.as_f64() == Some(1.0))
}

/// `migrateLegacyRecord`: only `hub.execution.*` scopes map (COMPAT(semanticHubPermissions)).
fn migrate_legacy_record(value: &Value) -> Value {
    if !is_legacy_record(value) {
        return value.clone();
    }
    let Some(relationship) = value.get("relationship").and_then(Value::as_object) else {
        return value.clone();
    };
    let Some(scopes) = relationship.get("scopes").and_then(Value::as_array) else {
        return value.clone();
    };
    let Some(scopes) = scopes
        .iter()
        .map(Value::as_str)
        .collect::<Option<Vec<&str>>>()
    else {
        return value.clone();
    };
    let mut migrated_relationship = relationship.clone();
    migrated_relationship.shift_remove("scopes");
    migrated_relationship.insert(
        "permissions".into(),
        permissions_value(&legacy_hub_scopes(&scopes)),
    );
    let mut migrated = value.as_object().cloned().unwrap_or_default();
    migrated.insert("version".into(), Value::from(2));
    migrated.insert("relationship".into(), Value::Object(migrated_relationship));
    Value::Object(migrated)
}

type Obj = Map<String, Value>;

fn object<'a>(value: &'a Value, what: &str) -> Result<&'a Obj, String> {
    value
        .as_object()
        .ok_or_else(|| format!("{what}: expected object"))
}

fn required<'a>(obj: &'a Obj, key: &str, what: &str) -> Result<&'a Value, String> {
    obj.get(key)
        .ok_or_else(|| format!("{what}.{key}: required"))
}

fn string(obj: &Obj, key: &str, what: &str) -> Result<String, String> {
    required(obj, key, what)?
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| format!("{what}.{key}: expected string"))
}

fn non_empty(obj: &Obj, key: &str, what: &str) -> Result<String, String> {
    let value = string(obj, key, what)?;
    if value.is_empty() {
        return Err(format!("{what}.{key}: must not be empty"));
    }
    Ok(value)
}

fn permissions_of(obj: &Obj, what: &str) -> Result<Vec<Permission>, String> {
    let array = required(obj, "permissions", what)?
        .as_array()
        .ok_or_else(|| format!("{what}.permissions: expected array"))?;
    let mut out = Vec::new();
    for item in array {
        let name = item
            .as_str()
            .ok_or_else(|| format!("{what}.permissions: expected string"))?;
        let permission =
            Permission::parse(name).ok_or_else(|| format!("Invalid daemon permission: {name}"))?;
        if !out.contains(&permission) {
            out.push(permission);
        }
    }
    Ok(out)
}

fn hub_origin_of(obj: &Obj, what: &str) -> Result<String, String> {
    let origin = string(obj, "hubOrigin", what)?;
    Url::parse(&origin).map_err(|_| format!("{what}.hubOrigin: invalid url"))?;
    normalize_hub_url(&origin)?;
    Ok(origin)
}

fn relationship_of(value: &Value, sanitized: bool) -> Result<RelationshipParts, String> {
    let obj = object(value, "relationship")?;
    Ok(RelationshipParts {
        daemon_id: non_empty(obj, "daemonId", "relationship")?,
        idempotency_key: if sanitized {
            String::new()
        } else {
            non_empty(obj, "idempotencyKey", "relationship")?
        },
        hub_origin: hub_origin_of(obj, "relationship")?,
        created_at: string(obj, "createdAt", "relationship")?,
        permissions: permissions_of(obj, "relationship")?,
    })
}

struct RelationshipParts {
    daemon_id: String,
    idempotency_key: String,
    hub_origin: String,
    created_at: String,
    permissions: Vec<Permission>,
}

impl RelationshipParts {
    fn full(self) -> Relationship {
        Relationship {
            daemon_id: self.daemon_id,
            idempotency_key: self.idempotency_key,
            hub_origin: self.hub_origin,
            created_at: self.created_at,
            permissions: self.permissions,
        }
    }

    fn sanitized(self) -> SanitizedRelationship {
        SanitizedRelationship {
            daemon_id: self.daemon_id,
            hub_origin: self.hub_origin,
            created_at: self.created_at,
            permissions: self.permissions,
        }
    }
}

fn credential_of(obj: &Obj) -> Result<Credential, String> {
    let value = required(obj, "credential", "record")?;
    let credential = object(value, "credential")?;
    Ok(Credential {
        secret: non_empty(credential, "secret", "credential")?,
    })
}

fn transport_of(value: &Value) -> Result<Transport, String> {
    let obj = object(value, "transport")?;
    if string(obj, "kind", "transport")? != "direct_websocket" {
        return Err("transport.kind: expected direct_websocket".into());
    }
    let url = string(obj, "webSocketUrl", "transport")?;
    if !is_valid_web_socket_url(&url) {
        return Err("transport.webSocketUrl: invalid WebSocket URL".into());
    }
    Ok(Transport {
        web_socket_url: url,
    })
}

/// An optional key: absent is fine, `null` is not (zod `.optional()`).
fn optional_transport(obj: &Obj) -> Result<Option<Transport>, String> {
    obj.get("transport").map(transport_of).transpose()
}

fn parse_current(raw: &Value) -> Result<Record, String> {
    let obj = object(raw, "record")?;
    if required(obj, "version", "record")?.as_f64() != Some(2.0) {
        return Err("record.version: expected 2".into());
    }
    let state = string(obj, "state", "record")?;
    let relationship_raw = required(obj, "relationship", "record")?;
    let record = match state.as_str() {
        "pending" => {
            let relationship = relationship_of(relationship_raw, false)?.full();
            let enrollment = object(required(obj, "enrollment", "record")?, "enrollment")?;
            let identity = object(required(obj, "identity", "record")?, "identity")?;
            Record::Pending(PendingRecord {
                relationship,
                credential: credential_of(obj)?,
                enrollment_token: non_empty(enrollment, "token", "enrollment")?,
                identity: Identity {
                    server_id: non_empty(identity, "serverId", "identity")?,
                    daemon_public_key: non_empty(identity, "daemonPublicKey", "identity")?,
                },
            })
        }
        "active" => Record::Active(ActiveRecord {
            relationship: relationship_of(relationship_raw, false)?.full(),
            credential: credential_of(obj)?,
            transport: transport_of(required(obj, "transport", "record")?)?,
        }),
        "disconnecting" => Record::Disconnecting(DisconnectingRecord {
            relationship: relationship_of(relationship_raw, false)?.full(),
            credential: credential_of(obj)?,
            transport: optional_transport(obj)?,
        }),
        "revoked" => Record::Revoked(RevokedRecord {
            relationship: relationship_of(relationship_raw, true)?.sanitized(),
            transport: optional_transport(obj)?,
            reason: obj
                .get("reason")
                .map(|v| {
                    v.as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| "record.reason: expected string".to_owned())
                })
                .transpose()?,
        }),
        other => return Err(format!("record.state: unknown state {other}")),
    };
    if let Some(transport) = record.transport() {
        if !web_socket_matches_hub_origin(record.hub_origin(), &transport.web_socket_url) {
            return Err("Hub WebSocket URL must match the Hub origin".into());
        }
    }
    Ok(record)
}

// ---------------------------------------------------------------------------------------
// Files

/// `ensurePrivateFile`: chmod 0600, failures ignored.
pub fn ensure_private_file(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// `writePrivateFileAtomicSync` of the record (0600, symlink-following, atomic rename).
pub fn persist(path: &Path, record: &Record) -> std::io::Result<()> {
    paseo_fs_atomic::write_private_atomic(path, record.to_file_text().as_bytes())
}

/// Outcome of [`load`].
pub enum Loaded {
    Missing,
    Record(Record),
    /// The file was invalid and moved to `quarantine_path`.
    Quarantined {
        quarantine_path: PathBuf,
        error: String,
    },
}

/// `HubRelationshipController.load`: parse and validate, rewrite a v1 record as v2, and
/// quarantine an invalid file as `hub-relationship.invalid-<ms>-<uuid>.json` (0600).
///
/// A failure to rewrite the migrated v1 record quarantines the file too, as in TS (the
/// rewrite sits inside the same `try`).
pub fn load(path: &Path, now_ms: i64) -> std::io::Result<Loaded> {
    if !path.exists() {
        return Ok(Loaded::Missing);
    }
    let attempt = (|| -> Result<Record, String> {
        let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
        let raw: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
        let (record, legacy) = Record::parse(&raw)?;
        if legacy {
            persist(path, &record).map_err(|e| e.to_string())?;
        }
        Ok(record)
    })();
    match attempt {
        Ok(record) => {
            ensure_private_file(path);
            Ok(Loaded::Record(record))
        }
        Err(error) => {
            let quarantine_path = path.with_file_name(format!(
                "hub-relationship.invalid-{now_ms}-{}.json",
                uuid::Uuid::new_v4()
            ));
            std::fs::rename(path, &quarantine_path)?;
            ensure_private_file(&quarantine_path);
            Ok(Loaded::Quarantined {
                quarantine_path,
                error,
            })
        }
    }
}

/// `samePermissions`: equal length and every expected one present.
pub fn same_permissions(actual: &[String], expected: &[Permission]) -> bool {
    actual.len() == expected.len()
        && expected
            .iter()
            .all(|p| actual.iter().any(|a| a == p.as_str()))
}
