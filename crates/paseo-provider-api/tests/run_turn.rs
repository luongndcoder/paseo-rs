use paseo_agent_types::{AgentStreamEvent, AgentTimelineItem, AgentUsage};
use paseo_provider_api::{
    append_or_replace_growing, event_channel, replace_with_assistant_message, run_provider_turn,
    EventStream, ProviderError, TurnHandle,
};

fn text(turn: Option<&str>, text: &str) -> AgentStreamEvent {
    AgentStreamEvent::Timeline {
        item: Box::new(AgentTimelineItem::AssistantMessage {
            text: text.into(),
            message_id: None,
        }),
        provider: "mock".into(),
        turn_id: turn.map(str::to_owned),
        timestamp: None,
    }
}

fn completed(turn: &str, output_tokens: u64) -> AgentStreamEvent {
    AgentStreamEvent::TurnCompleted {
        provider: "mock".into(),
        usage: Some(AgentUsage {
            output_tokens: Some(output_tokens.into()),
            ..AgentUsage::default()
        }),
        turn_id: Some(turn.into()),
    }
}

async fn start(turn: &str) -> Result<TurnHandle, ProviderError> {
    Ok(TurnHandle {
        turn_id: turn.into(),
    })
}

async fn id() -> Result<String, ProviderError> {
    Ok("session-1".into())
}

#[tokio::test]
async fn events_before_start_turn_resolves_are_buffered_and_filtered_by_turn() {
    let (tx, mut events) = event_channel(16);
    // Delivered before the turn id is known; one belongs to another turn.
    tx.send(text(Some("other"), "stale")).await.unwrap();
    tx.send(text(Some("t1"), "hello")).await.unwrap();
    tx.send(text(None, " world")).await.unwrap();
    tx.send(completed("other", 1)).await.unwrap();
    tx.send(completed("t1", 7)).await.unwrap();
    let result = run_provider_turn(&mut events, start("t1"), id(), append_or_replace_growing)
        .await
        .unwrap();
    assert_eq!(result.final_text, "hello world");
    assert_eq!(result.timeline.len(), 2);
    assert_eq!(result.session_id, "session-1");
    assert_eq!(
        result.usage.unwrap().output_tokens.unwrap().as_u64(),
        Some(7)
    );
}

#[tokio::test]
async fn events_after_start_are_processed_live_and_replace_reducer_keeps_last_message() {
    let (tx, mut events) = event_channel(16);
    let producer = tokio::spawn(async move {
        tokio::task::yield_now().await;
        tx.send(text(Some("t1"), "first")).await.unwrap();
        tx.send(text(Some("t1"), "second")).await.unwrap();
        tx.send(completed("t1", 2)).await.unwrap();
    });
    let result = run_provider_turn(
        &mut events,
        start("t1"),
        id(),
        replace_with_assistant_message,
    )
    .await
    .unwrap();
    producer.await.unwrap();
    assert_eq!(result.final_text, "second");
}

#[tokio::test]
async fn growing_reducer_replaces_when_text_extends_and_appends_otherwise() {
    let (tx, mut events) = event_channel(16);
    for chunk in ["Hel", "Hello", "!"] {
        tx.send(text(Some("t1"), chunk)).await.unwrap();
    }
    tx.send(completed("t1", 1)).await.unwrap();
    let result = run_provider_turn(&mut events, start("t1"), id(), append_or_replace_growing)
        .await
        .unwrap();
    assert_eq!(result.final_text, "Hello!");
}

#[tokio::test]
async fn turn_failed_rejects_with_the_error_text() {
    let (tx, mut events) = event_channel(16);
    tx.send(AgentStreamEvent::TurnFailed {
        provider: "mock".into(),
        error: "model exploded".into(),
        code: None,
        diagnostic: None,
        turn_id: Some("t1".into()),
    })
    .await
    .unwrap();
    let error = run_provider_turn(
        &mut events,
        start("t1"),
        id(),
        replace_with_assistant_message,
    )
    .await
    .unwrap_err();
    assert!(matches!(error, ProviderError::TurnFailed(ref m) if m == "model exploded"));
}

#[tokio::test]
async fn turn_canceled_resolves_with_what_was_collected() {
    let (tx, mut events) = event_channel(16);
    tx.send(text(Some("t1"), "partial")).await.unwrap();
    tx.send(AgentStreamEvent::TurnCanceled {
        provider: "mock".into(),
        reason: "interrupted".into(),
        turn_id: Some("t1".into()),
    })
    .await
    .unwrap();
    let result = run_provider_turn(
        &mut events,
        start("t1"),
        id(),
        replace_with_assistant_message,
    )
    .await
    .unwrap();
    assert_eq!(result.final_text, "partial");
    assert!(result.usage.is_none());
}

#[tokio::test]
async fn start_turn_failure_propagates() {
    let (_tx, mut events) = event_channel(16);
    let error = run_provider_turn(
        &mut events,
        async { Err(ProviderError::provider("cannot start")) },
        id(),
        replace_with_assistant_message,
    )
    .await
    .unwrap_err();
    assert!(matches!(error, ProviderError::Provider { ref message } if message == "cannot start"));
}

#[tokio::test]
async fn closed_stream_before_terminal_event_is_an_error() {
    let (tx, mut events) = event_channel(16);
    tx.send(text(Some("t1"), "partial")).await.unwrap();
    drop(tx);
    let error = run_provider_turn(
        &mut events,
        start("t1"),
        id(),
        replace_with_assistant_message,
    )
    .await
    .unwrap_err();
    assert!(matches!(error, ProviderError::StreamClosed));
}

#[tokio::test]
async fn closed_stream_while_start_is_pending_is_an_error() {
    let (tx, mut events) = event_channel(16);
    drop(tx);
    let error = run_provider_turn(
        &mut events,
        std::future::pending::<Result<TurnHandle, ProviderError>>(),
        id(),
        replace_with_assistant_message,
    )
    .await
    .unwrap_err();
    assert!(matches!(error, ProviderError::StreamClosed));
}

#[tokio::test]
async fn full_channel_applies_backpressure_and_never_drops() {
    let (tx, mut events) = event_channel(2);
    let producer = tokio::spawn(async move {
        for n in 0..50 {
            tx.send(text(Some("t1"), &format!("m{n}"))).await.unwrap();
        }
    });
    let mut seen = 0;
    // Let the producer fill the channel and block before consuming.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    while seen < 50 {
        let event = events.next().await.expect("producer still has events");
        match event {
            AgentStreamEvent::Timeline { item, .. } => match *item {
                AgentTimelineItem::AssistantMessage { text, .. } => {
                    assert_eq!(text, format!("m{seen}"));
                    seen += 1;
                }
                other => panic!("unexpected item {other:?}"),
            },
            other => panic!("unexpected event {other:?}"),
        }
    }
    producer.await.unwrap();
    assert!(events.next().await.is_none());
}

#[tokio::test]
async fn closed_stream_is_finished_and_sender_reports_closed_consumer() {
    let mut closed = EventStream::closed();
    assert!(closed.next().await.is_none());
    let (tx, events) = event_channel(1);
    drop(events);
    assert!(tx.is_closed());
    assert!(tx.send(text(None, "x")).await.is_err());
}
