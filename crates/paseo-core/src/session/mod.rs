//! The session layer of the core (WP9): per-client session actors, subscriptions, directory
//! sync responses and the registry the edge talks to.

pub mod actor;
pub mod caps;
pub mod env;
pub mod fetch_agents;
pub mod registry;
pub mod sink;
pub mod state;

pub use actor::{
    ClientHello, SessionActor, SessionCmd, SessionHandle, SessionKey, RECONNECT_GRACE,
};
pub use env::{GitFacts, ListOptions, NoSessionHooks, SessionDeps, SessionHooks, SessionServices};
pub use registry::{AttachRequest, Attached, SessionRegistry};

/// Inbound session message types [`SessionActor`] handles. The edge claims a type only when it is
/// listed here (see `paseod::core_routes`), so a type routed to the core always has a handler.
///
/// `ping` is handled but left unclaimed while the top-level application ping and the session-level
/// one share the routing key `ping`; TS keeps answering the session-level one.
///
/// `session.events.set_subscription.request` is handled by the actor but left unclaimed until the
/// TS shim exists: TS reconciles its git observers (`checkout_status_update`) on that request.
pub const HANDLED_INBOUND: &[&str] = &[
    "fetch_agents_request",
    "subscription.release.request",
    "agent.timeline.set_subscription.request",
    "ping",
];
