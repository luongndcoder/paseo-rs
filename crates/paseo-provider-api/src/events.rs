use paseo_agent_types::AgentStreamEvent;
use tokio::sync::mpsc;

/// Single-consumer stream of one session's events, in provider emit order.
///
/// Bounded: producers `await` on a full channel and never drop events. The stream ends
/// (`next` returns `None`) when the session closes or the producer side is dropped.
#[derive(Debug)]
pub struct EventStream {
    rx: mpsc::Receiver<AgentStreamEvent>,
}

/// Producer half of an [`EventStream`].
#[derive(Debug, Clone)]
pub struct EventSender {
    tx: mpsc::Sender<AgentStreamEvent>,
}

/// Default capacity of an event channel.
pub const DEFAULT_EVENT_CAPACITY: usize = 1024;

pub fn event_channel(capacity: usize) -> (EventSender, EventStream) {
    let (tx, rx) = mpsc::channel(capacity.max(1));
    (EventSender { tx }, EventStream { rx })
}

impl EventSender {
    /// Wait for room and deliver. `Err` returns the event when the consumer is gone.
    #[allow(clippy::result_large_err)]
    pub async fn send(&self, event: AgentStreamEvent) -> Result<(), AgentStreamEvent> {
        self.tx.send(event).await.map_err(|error| error.0)
    }

    pub fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }
}

impl EventStream {
    pub async fn next(&mut self) -> Option<AgentStreamEvent> {
        self.rx.recv().await
    }

    pub fn try_next(&mut self) -> Option<AgentStreamEvent> {
        self.rx.try_recv().ok()
    }

    /// A stream that is already finished. Returned when a session is subscribed twice.
    pub fn closed() -> Self {
        let (_, stream) = event_channel(1);
        stream
    }
}
