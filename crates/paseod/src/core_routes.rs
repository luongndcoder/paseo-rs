//! Which inbound message types the Rust core owns under `PASEO_CORE=rust`.
//!
//! The table is the module map of the session decomposition plan
//! (`docs/refactors/session-decomposition-plan.md`, "Module map and owned message types"),
//! cut into the migration slices of `docs/p4-core-spec.md` section 9:
//!
//! - P4a: registries, directory, labels, workspace messages
//! - P4b: agents, timeline, subscriptions, lifecycle, provider requests
//! - P4c: `agent.skills.*`, `hub.management.*`
//!
//! A slice flips when the core handles every type in it. Today only the subscription family and
//! the `fetch_agents` directory read are implemented in the core
//! ([`paseo_core::session::HANDLED_INBOUND`]), so [`implemented`] is what the edge claims; the
//! other slices are listed so the claim set for the next flip is a one-line change, and a test
//! pins every name against the protocol. Everything not claimed stays raw bytes to TS.
//!
//! Domains that stay outside the core (checkout, files, terminals, voice, schedule, daemon,
//! usage, plugins, push) are not listed. TS-resident controllers reach the Source registry over
//! `internal.delivery.*` instead ([`crate::internal_rpc`]).

use crate::routing::{Domain, OwnedTypes};

/// The replayed `hello`. The core only observes it (the edge keeps forwarding it to TS, which owns
/// admission) to learn the client id and capabilities.
pub const HELLO: &str = "hello";

/// Subscription ownership and the directory read: what the core implements.
pub const P4B_SUBSCRIPTIONS: &[&str] = &[
    "fetch_agents_request",
    "subscription.release.request",
    "agent.timeline.set_subscription.request",
];

/// `agent-lifecycle`, `agent-config`, the inline agent timeline/rewind/fork handlers and the
/// creation flows. Not implemented in the core yet.
pub const P4B_AGENTS: &[&str] = &[
    "fetch_agent_history_request",
    "fetch_recent_provider_sessions_request",
    "fetch_agent_request",
    "delete_agent_request",
    "archive_agent_request",
    "close_items_request",
    "update_agent_request",
    "send_agent_message_request",
    "wait_for_finish_request",
    "create_agent_request",
    "resume_agent_request",
    "import_agent_request",
    "refresh_agent_request",
    "cancel_agent_request",
    "agent_permission_response",
    "clear_agent_attention",
    "agent.detach.request",
    "set_agent_mode_request",
    "set_agent_model_request",
    "set_agent_feature_request",
    "set_agent_thinking_request",
    "agent.config.apply.request",
    "list_commands_request",
    "agent.rewind.request",
    "fetch_agent_timeline_request",
    "agent.timeline.append.request",
    "agent.timeline.search.request",
    "agent.timeline.list_prompts.request",
    "agent.provider_subagents.list.request",
    "agent.provider_subagents.timeline.get.request",
    "agent.fork_context.request",
    "agent.create.request",
    "creation.subscribe.request",
];

/// `provider/provider-catalog-session.ts`.
pub const P4B_PROVIDERS: &[&str] = &[
    "list_provider_models_request",
    "list_provider_modes_request",
    "list_provider_features_request",
    "list_available_providers_request",
    "get_providers_snapshot_request",
    "refresh_providers_snapshot_request",
    "provider_diagnostic_request",
];

/// Registries, directory, labels and workspace messages.
pub const P4A_WORKSPACES: &[&str] = &[
    "fetch_workspaces_request",
    "project.rename.request",
    "project.icon.set.request",
    "project.icon.get.request",
    "project.list.request",
    "project.remove.request",
    "project.add.request",
    "workspace.recovery.inspect.request",
    "workspace.recovery.restore.request",
    "workspace.label.list.request",
    "workspace.label.assignment.set.request",
    "workspace.label.update.request",
    "workspace.label.delete.request",
    "workspace.label.delete.inspect.request",
    "workspace.create.request",
    "archive_workspace_request",
    "workspace.title.set.request",
    "workspace.pin.set.request",
    "workspace.clear_attention.request",
    "workspace.mark_unread.request",
    "open_project_request",
];

pub const P4C_SKILLS_HUB: &[&str] = &[
    "agent.skills.get_status.request",
    "agent.skills.reconcile.request",
    "agent.skills.uninstall.request",
    "agent.skills.save_selection.request",
    "agent.skills.import_legacy_selection.request",
    "hub.management.daemon.connect.request",
    "hub.management.daemon.get_status.request",
    "hub.management.daemon.disconnect.request",
    "hub.management.daemon.permissions.update.request",
];

/// Every slice, for the exhaustiveness tests.
pub const ALL_SLICES: &[&[&str]] = &[
    P4B_SUBSCRIPTIONS,
    P4B_AGENTS,
    P4B_PROVIDERS,
    P4A_WORKSPACES,
    P4C_SKILLS_HUB,
];

/// The types the core claims today: `hello` (observed, still forwarded) and the implemented
/// slice.
pub fn implemented() -> OwnedTypes {
    owned_types(&[P4B_SUBSCRIPTIONS])
}

/// A claim table for the given slices plus `hello`.
pub fn owned_types(slices: &[&[&'static str]]) -> OwnedTypes {
    let mut table = OwnedTypes::empty();
    table.claim_type(HELLO, Domain::Core);
    for slice in slices {
        for ty in *slice {
            table.claim_type(ty, Domain::Core);
        }
    }
    table
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use crate::routing::{peek_route, FrameRef, Route};

    #[test]
    fn every_listed_type_is_a_real_inbound_session_message() {
        let known: HashSet<&str> = paseo_authz::inbound_table()
            .iter()
            .map(|(t, _)| *t)
            .collect();
        for slice in ALL_SLICES {
            for ty in *slice {
                assert!(
                    known.contains(ty),
                    "{ty} is not an inbound protocol message"
                );
            }
        }
    }

    #[test]
    fn slices_are_disjoint() {
        let mut seen = HashSet::new();
        for slice in ALL_SLICES {
            for ty in *slice {
                assert!(seen.insert(*ty), "{ty} is claimed by two slices");
            }
        }
    }

    #[test]
    fn the_claimed_set_is_exactly_what_the_core_handles() {
        for ty in P4B_SUBSCRIPTIONS {
            assert!(
                paseo_core::session::HANDLED_INBOUND.contains(ty),
                "{ty} is claimed but the session actor has no handler"
            );
        }
        // `ping` is handled but not claimed (see `HANDLED_INBOUND`).
        let unclaimed: Vec<&&str> = paseo_core::session::HANDLED_INBOUND
            .iter()
            .filter(|t| !P4B_SUBSCRIPTIONS.contains(t))
            .collect();
        assert_eq!(unclaimed, vec![&"ping"]);
    }

    #[test]
    fn implemented_routes_session_messages_and_hello_to_the_core() {
        let table = implemented();
        let session = |ty: &str| {
            format!(r#"{{"type":"session","message":{{"type":"{ty}","requestId":"r"}}}}"#)
        };
        for ty in P4B_SUBSCRIPTIONS {
            assert_eq!(
                peek_route(&table, FrameRef::Text(&session(ty))),
                Route::Owned(Domain::Core),
                "{ty}"
            );
        }
        assert_eq!(
            peek_route(&table, FrameRef::Text(r#"{"type":"hello","clientId":"c"}"#)),
            Route::Owned(Domain::Core)
        );
        // Other slices and other domains stay with TS.
        assert_eq!(
            peek_route(&table, FrameRef::Text(&session("create_agent_request"))),
            Route::Forward
        );
        assert_eq!(
            peek_route(&table, FrameRef::Text(&session("checkout_status_request"))),
            Route::Forward
        );
        assert_eq!(
            peek_route(&table, FrameRef::Text(&session("ping"))),
            Route::Forward
        );
        assert_eq!(
            peek_route(&table, FrameRef::Binary(&[0x03, 1])),
            Route::Forward
        );
    }
}
