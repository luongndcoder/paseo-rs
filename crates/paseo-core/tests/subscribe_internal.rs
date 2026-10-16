//! Port of the internal-agent and subscribe cases of `agent-manager.test.ts`. The archive-based
//! cases (`archiveAgent closes an internal agent...`, internal parent cascade) belong to the
//! archive step; `close_agent` stands in where a closed state is needed.

mod common;

use common::*;
use paseo_agent_types::{AgentStreamEvent, ProviderSubagentInputEvent, ProviderSubagentStatus};
use paseo_core::mock::MockSession;
use paseo_core::{ManagerEvent, RegisterOptions, SubscribeOpts};

fn internal_config(c: &mut paseo_provider_api::AgentSessionConfig) {
    c.internal = Some(true);
    c.title = Some("Internal Agent".into());
}

async fn internal_agent(h: &Harness, id: &str) -> MockSession {
    let session = MockSession::auto_completing("codex");
    h.agent_with(
        id,
        &session,
        "codex",
        RegisterOptions::default(),
        internal_config,
    )
    .await;
    session
}

fn state_ids(log: &EventLog) -> Vec<String> {
    log.all()
        .iter()
        .filter_map(|e| match &**e {
            ManagerEvent::AgentState { agent } => Some(agent.id.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test(start_paused = true)]
async fn subscribe_error_isolation_a_dropped_subscriber_does_not_break_event_flow() {
    // A TS subscriber that throws must not stop delivery to the others. Subscribers here are
    // channels, so the equivalent failure is a consumer that disappears mid-stream.
    let h = Harness::new().await;
    let session = MockSession::new("codex");
    h.agent(AGENT_IDS[0], &session).await;
    let doomed = h
        .manager
        .subscribe(SubscribeOpts::for_agent(AGENT_IDS[0]).without_replay())
        .await
        .unwrap();
    let survivor = collect(
        &h.manager,
        SubscribeOpts::for_agent(AGENT_IDS[0]).without_replay(),
    )
    .await;
    drop(doomed);

    let turn = "autonomous-isolation-1";
    session.push_event(turn_started(turn));
    session.push_event(timeline_event(
        assistant_item("EVENT_AFTER_ERROR"),
        "codex",
        Some(turn),
    ));
    session.push_event(turn_completed(turn));
    settle().await;

    let kinds: Vec<&str> = survivor
        .streams(AGENT_IDS[0])
        .iter()
        .map(|e| stream_event(e).type_name())
        .collect();
    assert!(kinds.contains(&"turn_started"));
    assert!(kinds.contains(&"timeline"));
    assert!(kinds.contains(&"turn_completed"));
    assert!(h
        .items(AGENT_IDS[0])
        .await
        .contains(&assistant_item("EVENT_AFTER_ERROR")));
}

#[tokio::test(start_paused = true)]
async fn list_agents_excludes_internal_agents() {
    let h = Harness::new().await;
    h.agent_with(
        AGENT_IDS[0],
        &MockSession::new("codex"),
        "codex",
        RegisterOptions::default(),
        |c| c.title = Some("Normal Agent".into()),
    )
    .await;
    internal_agent(&h, AGENT_IDS[1]).await;

    let agents = h.manager.list_agents(false);
    assert_eq!(agents.len(), 1);
    assert_eq!(agents[0].config.title.as_deref(), Some("Normal Agent"));

    // Opting in lists both, and the internal one is marked.
    let every: Vec<(Option<String>, bool)> = h
        .manager
        .list_agents(true)
        .iter()
        .map(|a| (a.config.title.clone(), a.internal))
        .collect();
    assert_eq!(
        every,
        vec![
            (Some("Normal Agent".to_owned()), false),
            (Some("Internal Agent".to_owned()), true),
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn global_subscribers_receive_internal_agent_events_only_when_they_opt_in() {
    let h = Harness::new().await;
    internal_agent(&h, AGENT_IDS[2]).await;

    let hidden = collect(&h.manager, SubscribeOpts::default()).await;
    let opted_in = collect(&h.manager, SubscribeOpts::default().including_internal()).await;
    settle().await;
    assert!(state_ids(&hidden).is_empty());
    assert_eq!(
        state_ids(&opted_in),
        vec![AGENT_IDS[2].to_owned()],
        "replayed on subscribe"
    );

    h.manager.close_agent(AGENT_IDS[2]).await.unwrap();
    settle().await;
    assert!(state_ids(&hidden).is_empty());
    assert!(
        state_ids(&opted_in).len() > 1,
        "the closed state reaches the opted-in subscriber"
    );
}

#[tokio::test(start_paused = true)]
async fn get_agent_returns_internal_agents_by_id() {
    let h = Harness::new().await;
    internal_agent(&h, AGENT_IDS[2]).await;
    let agent = h
        .manager
        .get_agent(AGENT_IDS[2])
        .expect("internal agents are readable by id");
    assert!(agent.internal);
}

#[tokio::test(start_paused = true)]
async fn subscribe_does_not_emit_state_events_for_internal_agents_to_global_subscribers() {
    let h = Harness::new().await;
    let global = collect(&h.manager, SubscribeOpts::default()).await;
    h.agent_with(
        AGENT_IDS[0],
        &MockSession::new("codex"),
        "codex",
        RegisterOptions::default(),
        |c| c.title = Some("Normal Agent".into()),
    )
    .await;
    internal_agent(&h, AGENT_IDS[1]).await;
    settle().await;

    let ids = state_ids(&global);
    assert!(ids.iter().filter(|id| *id == AGENT_IDS[0]).count() > 0);
    assert_eq!(ids.iter().filter(|id| *id == AGENT_IDS[1]).count(), 0);
}

#[tokio::test(start_paused = true)]
async fn subscribe_hides_provider_subagents_of_internal_parents_from_global_subscribers() {
    let h = Harness::new().await;
    let global = collect(&h.manager, SubscribeOpts::default().without_replay()).await;
    let session = internal_agent(&h, AGENT_IDS[2]).await;
    let scoped = collect(
        &h.manager,
        SubscribeOpts::for_agent(AGENT_IDS[2]).without_replay(),
    )
    .await;

    session.push_event(AgentStreamEvent::ProviderSubagent {
        provider: "codex".into(),
        event: Box::new(ProviderSubagentInputEvent::Upsert {
            id: "hidden-child".into(),
            title: Some(Some("Hidden child".into())),
            description: None,
            status: Some(ProviderSubagentStatus::Running),
            tool_call_id: None,
            cwd: None,
            subtitle: None,
            parent_subagent_id: None,
            timestamp: None,
        }),
    });
    h.manager.flush().await;
    settle().await;

    let is_subagent =
        |e: &paseo_core::SharedEvent| matches!(&**e, ManagerEvent::ProviderSubagent { .. });
    assert!(global.all().iter().filter(|e| is_subagent(e)).count() == 0);
    let seen: Vec<_> = scoped
        .all()
        .into_iter()
        .filter(|e| is_subagent(e))
        .collect();
    assert_eq!(seen.len(), 1);
    match &*seen[0] {
        ManagerEvent::ProviderSubagent { event, .. } => {
            assert_eq!(event.parent_agent_id(), AGENT_IDS[2]);
            assert!(matches!(
                event,
                paseo_core::provider_subagents::SubagentStoreEvent::Upsert { subagent } if subagent.id == "hidden-child"
            ));
        }
        other => panic!("unexpected {other:?}"),
    }
    let unknown = format!("Unknown agent '{}'", AGENT_IDS[2]);
    assert_eq!(
        h.manager
            .list_provider_subagents(AGENT_IDS[2])
            .unwrap_err()
            .to_string(),
        unknown
    );
    assert_eq!(
        h.manager
            .get_provider_subagent(AGENT_IDS[2], "hidden-child")
            .unwrap_err()
            .to_string(),
        unknown
    );
    assert_eq!(
        h.manager
            .fetch_provider_subagent_timeline(AGENT_IDS[2], "hidden-child", &Default::default())
            .unwrap_err()
            .to_string(),
        unknown
    );
    assert!(h.manager.list_provider_subagent_activity().is_empty());
}

#[tokio::test(start_paused = true)]
async fn closing_a_parent_cancels_its_running_provider_subagents() {
    let h = Harness::new().await;
    let session = MockSession::new("codex");
    h.agent(AGENT_IDS[0], &session).await;
    let scoped = collect(
        &h.manager,
        SubscribeOpts::for_agent(AGENT_IDS[0]).without_replay(),
    )
    .await;
    session.push_event(AgentStreamEvent::ProviderSubagent {
        provider: "codex".into(),
        event: Box::new(ProviderSubagentInputEvent::Upsert {
            id: "child".into(),
            title: Some(Some("Child".into())),
            description: None,
            status: Some(ProviderSubagentStatus::Running),
            tool_call_id: None,
            cwd: None,
            subtitle: None,
            parent_subagent_id: None,
            timestamp: None,
        }),
    });
    settle().await;
    assert_eq!(h.manager.list_provider_subagent_activity().len(), 1);
    let child = h
        .manager
        .get_provider_subagent(AGENT_IDS[0], "child")
        .unwrap()
        .unwrap();
    assert_eq!(child.status, ProviderSubagentStatus::Running);

    h.manager.close_agent(AGENT_IDS[0]).await.unwrap();
    settle().await;

    let canceled = scoped.all().into_iter().any(|e| {
        matches!(
            &*e,
            ManagerEvent::ProviderSubagent {
                event: paseo_core::provider_subagents::SubagentStoreEvent::Upsert { subagent },
                ..
            } if subagent.status == ProviderSubagentStatus::Canceled
        )
    });
    assert!(
        canceled,
        "the running child is canceled before the parent closes"
    );
    // The parent is gone: public reads fail and the activity list hides its children.
    assert!(h
        .manager
        .get_provider_subagent(AGENT_IDS[0], "child")
        .is_err());
    assert!(h.manager.list_provider_subagent_activity().is_empty());
}

#[tokio::test(start_paused = true)]
async fn subscribe_emits_state_events_for_internal_agents_when_subscribed_by_agent_id() {
    let h = Harness::new().await;
    let scoped = collect(
        &h.manager,
        SubscribeOpts::for_agent(AGENT_IDS[2]).without_replay(),
    )
    .await;
    internal_agent(&h, AGENT_IDS[2]).await;
    settle().await;
    assert!(
        state_ids(&scoped)
            .iter()
            .filter(|id| *id == AGENT_IDS[2])
            .count()
            > 0
    );
}

#[tokio::test(start_paused = true)]
async fn subscribe_fails_when_filter_agent_id_is_not_a_uuid() {
    let h = Harness::new().await;
    let error = h
        .manager
        .subscribe(SubscribeOpts::for_agent("invalid-agent-id"))
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "subscribe: agentId must be a UUID");
}
