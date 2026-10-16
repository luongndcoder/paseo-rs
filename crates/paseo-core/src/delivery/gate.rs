//! The egress gate: what a physical socket accepts. Shared between the delivery registry
//! (which flips `active`) and the edge's socket writer (which calls [`SourceGate::permits`]).

use std::sync::atomic::{AtomicBool, Ordering};

use super::types::{Outbound, SourceId};

#[derive(Debug)]
pub struct SourceGate {
    id: SourceId,
    modern: bool,
    active: AtomicBool,
    handshake_pending: AtomicBool,
}

impl SourceGate {
    pub(crate) fn new(id: SourceId, modern: bool) -> Self {
        Self {
            id,
            modern,
            active: AtomicBool::new(true),
            handshake_pending: AtomicBool::new(true),
        }
    }

    pub fn id(&self) -> SourceId {
        self.id
    }

    pub fn is_modern(&self) -> bool {
        self.modern
    }

    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::SeqCst)
    }

    pub(crate) fn deactivate(&self) {
        self.active.store(false, Ordering::SeqCst);
    }

    /// `SessionDelivery.permits` / `permitsBinary`.
    ///
    /// An inactive source accepts nothing. A legacy source accepts everything
    /// (COMPAT(ownedSubscriptions): added in v0.8.0, remove after 2027-03-09). A modern source
    /// accepts one `server_info` while its handshake is pending, and otherwise only frames proven
    /// by an active owner of this same source.
    pub fn permits(&self, out: &Outbound) -> bool {
        if out.to != self.id || !self.is_active() {
            return false;
        }
        if !self.modern {
            return true;
        }
        if out.server_info && self.handshake_pending.swap(false, Ordering::SeqCst) {
            return true;
        }
        out.owner
            .as_ref()
            .is_some_and(|o| o.is_active() && o.source() == self.id)
    }
}
