//! Session authorization for the Paseo daemon.
//!
//! Mirrors `packages/server/src/server/authorization`. Requirement tables are generated at build
//! time from `tables/*.json` (extracted from the TS source of truth). A message type missing from a
//! table is denied: `required_*` return `None`, never a default-allow requirement.

use std::sync::atomic::{AtomicU16, Ordering};

mod tables {
    use super::{Permission, Requirement};
    include!(concat!(env!("OUT_DIR"), "/tables.rs"));
}

/// Daemon permissions, in `DAEMON_PERMISSIONS` order. The discriminant is the bit index.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Permission {
    DaemonRead = 0,
    DaemonManage = 1,
    TunnelManage = 2,
    AccessManage = 3,
    WorkspaceRead = 4,
    WorkspaceWrite = 5,
    WorkspaceManage = 6,
    AutomationManage = 7,
    HubExecute = 8,
}

pub const DAEMON_PERMISSIONS: [Permission; 9] = [
    Permission::DaemonRead,
    Permission::DaemonManage,
    Permission::TunnelManage,
    Permission::AccessManage,
    Permission::WorkspaceRead,
    Permission::WorkspaceWrite,
    Permission::WorkspaceManage,
    Permission::AutomationManage,
    Permission::HubExecute,
];

impl Permission {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DaemonRead => "daemon.read",
            Self::DaemonManage => "daemon.manage",
            Self::TunnelManage => "tunnel.manage",
            Self::AccessManage => "access.manage",
            Self::WorkspaceRead => "workspace.read",
            Self::WorkspaceWrite => "workspace.write",
            Self::WorkspaceManage => "workspace.manage",
            Self::AutomationManage => "automation.manage",
            Self::HubExecute => "hub.execute",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        DAEMON_PERMISSIONS
            .iter()
            .copied()
            .find(|p| p.as_str() == name)
    }

    const fn bit(self) -> u16 {
        1 << (self as u8)
    }
}

/// Error for [`parse_daemon_permissions`]. Message matches TS.
#[derive(Debug, PartialEq, Eq)]
pub struct InvalidPermission;

impl std::fmt::Display for InvalidPermission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Invalid daemon permission")
    }
}

impl std::error::Error for InvalidPermission {}

/// Validates names and removes duplicates, keeping first-seen order (TS `parseDaemonPermissions`).
pub fn parse_daemon_permissions<S: AsRef<str>>(
    values: &[S],
) -> Result<Vec<Permission>, InvalidPermission> {
    let mut out = Vec::new();
    for v in values {
        let p = Permission::parse(v.as_ref()).ok_or(InvalidPermission)?;
        if !out.contains(&p) {
            out.push(p);
        }
    }
    Ok(out)
}

/// Set of permissions as a u16 bitset.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PermSet(u16);

impl PermSet {
    pub const EMPTY: PermSet = PermSet(0);
    pub const OWNER: PermSet = {
        let mut bits = 0u16;
        let mut i = 0;
        while i < DAEMON_PERMISSIONS.len() {
            bits |= DAEMON_PERMISSIONS[i].bit();
            i += 1;
        }
        PermSet(bits)
    };

    pub fn contains(self, p: Permission) -> bool {
        self.0 & p.bit() != 0
    }

    /// Permissions in `DAEMON_PERMISSIONS` order.
    pub fn iter(self) -> impl Iterator<Item = Permission> {
        DAEMON_PERMISSIONS
            .into_iter()
            .filter(move |p| self.contains(*p))
    }
}

impl FromIterator<Permission> for PermSet {
    fn from_iter<I: IntoIterator<Item = Permission>>(iter: I) -> Self {
        PermSet(iter.into_iter().fold(0, |a, p| a | p.bit()))
    }
}

/// What a message needs. TS: `null` is `Always`, a string is `One`, an array is `Any`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Requirement {
    Always,
    One(Permission),
    Any(&'static [Permission]),
}

impl Requirement {
    pub fn satisfied_by(self, set: PermSet) -> bool {
        match self {
            Self::Always => true,
            Self::One(p) => set.contains(p),
            Self::Any(ps) => ps.iter().any(|p| set.contains(*p)),
        }
    }
}

fn lookup(table: &'static [(&'static str, Requirement)], ty: &str) -> Option<Requirement> {
    table
        .binary_search_by(|(k, _)| (*k).cmp(ty))
        .ok()
        .map(|i| table[i].1)
}

/// Requirement for an inbound session message type. `None` means unmapped, which denies.
pub fn required_inbound(ty: &str) -> Option<Requirement> {
    lookup(tables::INBOUND, ty)
}

/// Outbound message identity: its `type` plus, for the legacy `status` envelope, `payload.status`.
#[derive(Clone, Copy, Debug)]
pub struct OutboundRef<'a> {
    pub ty: &'a str,
    pub status: Option<&'a str>,
}

impl<'a> OutboundRef<'a> {
    pub fn new(ty: &'a str) -> Self {
        Self { ty, status: None }
    }

    /// Reads `type` and `payload.status` from a JSON frame.
    pub fn from_value(v: &'a serde_json::Value) -> Option<Self> {
        let ty = v.get("type")?.as_str()?;
        let status = v
            .get("payload")
            .and_then(|p| p.get("status"))
            .and_then(|s| s.as_str());
        Some(Self { ty, status })
    }
}

const AGENT_WRITE_OR_HUB: Requirement =
    Requirement::Any(&[Permission::WorkspaceWrite, Permission::HubExecute]);

/// Requirement for an outbound session message. `None` means unmapped, which denies.
pub fn required_outbound(m: &OutboundRef<'_>) -> Option<Requirement> {
    // The legacy status envelope carries both agent RPC results and daemon configuration.
    if m.ty == "status" && matches!(m.status, Some("agent_created" | "agent_create_failed")) {
        return Some(AGENT_WRITE_OR_HUB);
    }
    lookup(tables::OUTBOUND, m.ty)
}

/// All inbound table rows (sorted by type). For tests and fixture comparison.
pub fn inbound_table() -> &'static [(&'static str, Requirement)] {
    tables::INBOUND
}

/// All outbound table rows (sorted by type), without the `status` special case.
pub fn outbound_table() -> &'static [(&'static str, Requirement)] {
    tables::OUTBOUND
}

/// Atomically replaceable permission set for one session. One load per frame on the hot path.
///
/// `ArcSwap<u16>` from the spec reduces to an atomic word for a 9-bit set.
#[derive(Debug)]
pub struct SessionAuthz(AtomicU16);

impl SessionAuthz {
    pub fn new(set: PermSet) -> Self {
        Self(AtomicU16::new(set.0))
    }

    pub fn owner() -> Self {
        Self::new(PermSet::OWNER)
    }

    pub fn permissions(&self) -> PermSet {
        PermSet(self.0.load(Ordering::Acquire))
    }

    pub fn allows_permission(&self, p: Permission) -> bool {
        self.permissions().contains(p)
    }

    pub fn allows_inbound(&self, ty: &str) -> bool {
        required_inbound(ty).is_some_and(|r| r.satisfied_by(self.permissions()))
    }

    pub fn allows_outbound(&self, m: &OutboundRef<'_>) -> bool {
        required_outbound(m).is_some_and(|r| r.satisfied_by(self.permissions()))
    }

    /// Replaces the set and returns the previous one. Callers compare the two: losing
    /// `workspace.write` also releases the browser-host delivery family.
    pub fn replace(&self, set: PermSet) -> PermSet {
        PermSet(self.0.swap(set.0, Ordering::AcqRel))
    }
}

/// True when `workspace.write` was in `prev` but not in `next`.
pub fn lost_workspace_write(prev: PermSet, next: PermSet) -> bool {
    prev.contains(Permission::WorkspaceWrite) && !next.contains(Permission::WorkspaceWrite)
}

const LEGACY_HUB_EXECUTION_SCOPE: &str = "hub.execution.*";

/// Translates legacy Hub enrollment scopes to permissions. Only `hub.execution.*` maps.
// COMPAT(semanticHubPermissions): added in v0.7, remove after Hub enrollment uses permissions.
pub fn legacy_hub_scopes<S: AsRef<str>>(scopes: &[S]) -> Vec<Permission> {
    if scopes
        .iter()
        .any(|s| s.as_ref() == LEGACY_HUB_EXECUTION_SCOPE)
    {
        vec![Permission::HubExecute]
    } else {
        Vec::new()
    }
}

/// `error` text of the `rpc_error {code:"access_denied"}` sent for a denied inbound message.
/// The caller sends it only when the message carries a `requestId`; the handler never runs.
pub fn access_denied_error(ty: &str) -> String {
    format!("Session is not authorized for {ty}")
}
