//! Turn identity rules (`attachManagedTurnIdentity`, `applyActiveTurnTerminal`).

use paseo_agent_types::AgentStreamEvent;
use uuid::Uuid;

use crate::model::ManagedAgentView;

pub(crate) struct Identified {
    pub event: AgentStreamEvent,
    pub turn_id: Option<String>,
}

/// Set the turn id of the variants that carry one.
pub(crate) fn with_turn_id(event: AgentStreamEvent, id: String) -> AgentStreamEvent {
    use AgentStreamEvent as E;
    match event {
        E::TurnStarted { provider, .. } => E::TurnStarted {
            provider,
            turn_id: Some(id),
        },
        E::TurnCompleted {
            provider, usage, ..
        } => E::TurnCompleted {
            provider,
            usage,
            turn_id: Some(id),
        },
        E::TurnFailed {
            provider,
            error,
            code,
            diagnostic,
            ..
        } => E::TurnFailed {
            provider,
            error,
            code,
            diagnostic,
            turn_id: Some(id),
        },
        E::TurnCanceled {
            provider, reason, ..
        } => E::TurnCanceled {
            provider,
            reason,
            turn_id: Some(id),
        },
        E::Timeline {
            item,
            provider,
            timestamp,
            ..
        } => E::Timeline {
            item,
            provider,
            turn_id: Some(id),
            timestamp,
        },
        other => other,
    }
}

/// Events that already carry a turn id, and history events, are untouched. `turn_started`
/// without an id gets `activeForegroundTurnId ?? activeTurnId ?? autonomous-<uuid>`; terminal
/// and timeline events get `activeForegroundTurnId ?? activeTurnId ?? absent`.
pub(crate) fn attach_turn_identity(
    view: &ManagedAgentView,
    event: AgentStreamEvent,
    from_history: bool,
) -> Identified {
    use AgentStreamEvent as E;
    let existing = event.turn_id().map(str::to_owned);
    if from_history || existing.is_some() {
        return Identified {
            event,
            turn_id: existing,
        };
    }
    let owner = || {
        view.active_foreground_turn_id
            .clone()
            .or_else(|| view.active_turn_id.clone())
    };
    let turn_id = match &event {
        E::TurnStarted { .. } => {
            Some(owner().unwrap_or_else(|| format!("autonomous-{}", Uuid::new_v4())))
        }
        E::TurnCompleted { .. }
        | E::TurnFailed { .. }
        | E::TurnCanceled { .. }
        | E::Timeline { .. } => owner(),
        _ => None,
    };
    match turn_id {
        Some(id) => Identified {
            event: with_turn_id(event, id.clone()),
            turn_id: Some(id),
        },
        None => Identified {
            event,
            turn_id: None,
        },
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TerminalDisposition {
    ClosedCurrent,
    Stale,
    Untracked,
}

/// `isForegroundEvent`: TS compares `activeForegroundTurnId === eventTurnId` where
/// `null !== undefined`, so an absent event turn id never matches.
pub(crate) fn is_foreground_event(view: &ManagedAgentView, event_turn: Option<&str>) -> bool {
    matches!((&view.active_foreground_turn_id, event_turn), (Some(fg), Some(t)) if fg == t)
}

/// `applyActiveTurnTerminal`. Empty turn ids are falsy in TS and treated as absent.
pub(crate) fn apply_active_turn_terminal(
    view: &mut ManagedAgentView,
    turn_id: Option<&str>,
    from_history: bool,
) -> TerminalDisposition {
    if from_history {
        return TerminalDisposition::Stale;
    }
    let Some(active) = &view.active_turn_id else {
        return TerminalDisposition::Untracked;
    };
    if let Some(turn) = turn_id.filter(|t| !t.is_empty()) {
        if active != turn {
            return TerminalDisposition::Stale;
        }
    }
    view.active_turn_id = None;
    view.active_turn_started_at_ms = None;
    TerminalDisposition::ClosedCurrent
}

#[cfg(test)]
mod tests {
    use super::*;
    use paseo_agent_types::AgentTimelineItem;

    fn timeline() -> AgentStreamEvent {
        AgentStreamEvent::Timeline {
            item: Box::new(AgentTimelineItem::Reasoning { text: "r".into() }),
            provider: "codex".into(),
            turn_id: None,
            timestamp: None,
        }
    }

    fn started() -> AgentStreamEvent {
        AgentStreamEvent::TurnStarted {
            provider: "codex".into(),
            turn_id: None,
        }
    }

    fn view(fg: Option<&str>, active: Option<&str>) -> ManagedAgentView {
        let mut v = ManagedAgentView::for_test("00000000-0000-4000-8000-000000000001");
        v.active_foreground_turn_id = fg.map(str::to_owned);
        v.active_turn_id = active.map(str::to_owned);
        v
    }

    #[test]
    fn events_with_an_id_and_history_events_are_untouched() {
        let v = view(Some("fg"), Some("active"));
        let with_id = AgentStreamEvent::TurnCompleted {
            provider: "codex".into(),
            usage: None,
            turn_id: Some("own".into()),
        };
        let out = attach_turn_identity(&v, with_id.clone(), false);
        assert_eq!((out.event, out.turn_id.as_deref()), (with_id, Some("own")));
        let out = attach_turn_identity(&v, timeline(), true);
        assert_eq!(out.turn_id, None, "history keeps absent ids");
    }

    #[test]
    fn turn_started_prefers_foreground_then_active_then_mints_an_autonomous_id() {
        let out = attach_turn_identity(&view(Some("fg"), Some("active")), started(), false);
        assert_eq!(out.turn_id.as_deref(), Some("fg"));
        let out = attach_turn_identity(&view(None, Some("active")), started(), false);
        assert_eq!(out.turn_id.as_deref(), Some("active"));
        let out = attach_turn_identity(&view(None, None), started(), false);
        let minted = out.turn_id.expect("minted");
        assert!(minted.starts_with("autonomous-"), "{minted}");
        assert_eq!(out.event.turn_id(), Some(minted.as_str()));
    }

    #[test]
    fn terminal_and_timeline_events_stay_absent_without_an_owner() {
        let none = view(None, None);
        for event in [
            timeline(),
            AgentStreamEvent::TurnCanceled {
                provider: "codex".into(),
                reason: "x".into(),
                turn_id: None,
            },
        ] {
            assert_eq!(attach_turn_identity(&none, event, false).turn_id, None);
        }
        let owned = attach_turn_identity(&view(None, Some("active")), timeline(), false);
        assert_eq!(owned.turn_id.as_deref(), Some("active"));
        // Other event kinds never get an id.
        let usage = AgentStreamEvent::ThreadStarted {
            session_id: "s".into(),
            provider: "codex".into(),
        };
        assert_eq!(
            attach_turn_identity(&view(Some("fg"), None), usage, false).turn_id,
            None
        );
    }

    #[test]
    fn foreground_match_never_treats_two_absent_ids_as_equal() {
        assert!(is_foreground_event(&view(Some("t"), None), Some("t")));
        assert!(!is_foreground_event(&view(Some("t"), None), Some("u")));
        assert!(!is_foreground_event(&view(Some("t"), None), None));
        // TS `null === undefined` is false: no foreground turn and no event turn do not match.
        assert!(!is_foreground_event(&view(None, None), None));
    }

    #[test]
    fn terminal_disposition() {
        let mut v = view(None, Some("a"));
        assert_eq!(
            apply_active_turn_terminal(&mut v, Some("a"), true),
            TerminalDisposition::Stale
        );
        assert_eq!(
            v.active_turn_id.as_deref(),
            Some("a"),
            "history never closes the turn"
        );
        assert_eq!(
            apply_active_turn_terminal(&mut v, Some("b"), false),
            TerminalDisposition::Stale
        );
        assert_eq!(v.active_turn_id.as_deref(), Some("a"));
        assert_eq!(
            apply_active_turn_terminal(&mut v, Some("a"), false),
            TerminalDisposition::ClosedCurrent
        );
        assert_eq!(v.active_turn_id, None);
        assert_eq!(
            apply_active_turn_terminal(&mut v, Some("a"), false),
            TerminalDisposition::Untracked
        );

        // An id-less terminal closes whatever is active; an empty id is falsy in TS.
        let mut v = view(None, Some("a"));
        assert_eq!(
            apply_active_turn_terminal(&mut v, Some(""), false),
            TerminalDisposition::ClosedCurrent
        );
        let mut v = view(None, Some("a"));
        assert_eq!(
            apply_active_turn_terminal(&mut v, None, false),
            TerminalDisposition::ClosedCurrent
        );
    }
}
