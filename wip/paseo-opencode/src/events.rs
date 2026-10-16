//! Event hub: one reconnecting consumer of `GET /global/event` fanned out to subscribers.
//!
//! Mirrors the TS `OpenCodeEventConsumer`: ready on the first `server.connected`, a watchdog
//! (5 s until the first record, 30 s afterwards; the server heartbeats about every 10 s),
//! exponential reconnect backoff capped at 5 s, and a terminal `ServerExited` message.

use crate::client::RawClient;
use crate::error::{Error, Result};
use crate::protocol::{Event, GlobalEvent};
use futures_util::StreamExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;

pub const FIRST_RECORD_WATCHDOG: Duration = Duration::from_secs(5);
pub const WATCHDOG: Duration = Duration::from_secs(30);
pub const MAX_BACKOFF: Duration = Duration::from_secs(5);
const CHANNEL_CAPACITY: usize = 4096;

#[derive(Debug, Clone)]
pub enum HubMessage {
    Event(Arc<GlobalEvent>),
    /// The server process went away; the hub is closed.
    ServerExited(String),
}

#[derive(Debug, Clone, Copy)]
pub struct HubTiming {
    pub first_record_watchdog: Duration,
    pub watchdog: Duration,
    pub max_backoff: Duration,
}

impl Default for HubTiming {
    fn default() -> Self {
        Self {
            first_record_watchdog: FIRST_RECORD_WATCHDOG,
            watchdog: WATCHDOG,
            max_backoff: MAX_BACKOFF,
        }
    }
}

pub struct EventHub {
    tx: broadcast::Sender<HubMessage>,
    ready: watch::Receiver<ReadyState>,
    ready_tx: Arc<watch::Sender<ReadyState>>,
    closed: Arc<AtomicBool>,
    task: JoinHandle<()>,
}

#[derive(Debug, Clone, PartialEq)]
enum ReadyState {
    Pending,
    Ready,
    Failed(String),
}

impl EventHub {
    /// Starts consuming `client.global_event()`. Pass `exit` to close the hub when the server dies.
    pub fn spawn(
        client: RawClient,
        timing: HubTiming,
        exit: Option<watch::Receiver<Option<String>>>,
    ) -> Arc<EventHub> {
        let (tx, _) = broadcast::channel(CHANNEL_CAPACITY);
        let (ready_tx, ready) = watch::channel(ReadyState::Pending);
        let ready_tx = Arc::new(ready_tx);
        let closed = Arc::new(AtomicBool::new(false));
        let task = tokio::spawn(run(
            client,
            timing,
            tx.clone(),
            ready_tx.clone(),
            closed.clone(),
            exit,
        ));
        Arc::new(EventHub {
            tx,
            ready,
            ready_tx,
            closed,
            task,
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<HubMessage> {
        self.tx.subscribe()
    }

    /// Resolves after the first `server.connected`, or fails on timeout / server exit.
    pub async fn ready(&self, timeout: Duration) -> Result<()> {
        let mut rx = self.ready.clone();
        let wait = async {
            loop {
                match rx.borrow_and_update().clone() {
                    ReadyState::Ready => return Ok(()),
                    ReadyState::Failed(why) => return Err(Error::ServerExited(why)),
                    ReadyState::Pending => {}
                }
                if rx.changed().await.is_err() {
                    return Err(Error::ServerExited("event hub stopped".into()));
                }
            }
        };
        tokio::time::timeout(timeout, wait)
            .await
            .map_err(|_| Error::Timeout("OpenCode event stream did not become ready".into()))?
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    pub fn close(&self) {
        if self.closed.swap(true, Ordering::SeqCst) {
            self.task.abort();
            return;
        }
        self.task.abort();
        let why = "OpenCode server stopped".to_string();
        self.ready_tx.send_if_modified(|s| {
            if *s == ReadyState::Pending {
                *s = ReadyState::Failed(why.clone());
                true
            } else {
                false
            }
        });
        let _ = self.tx.send(HubMessage::ServerExited(why));
    }
}

impl Drop for EventHub {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn run(
    client: RawClient,
    timing: HubTiming,
    tx: broadcast::Sender<HubMessage>,
    ready_tx: Arc<watch::Sender<ReadyState>>,
    closed: Arc<AtomicBool>,
    exit: Option<watch::Receiver<Option<String>>>,
) {
    let consume = consume(client, timing, tx.clone(), ready_tx.clone());
    tokio::pin!(consume);
    match exit {
        Some(mut exit) => {
            tokio::select! {
                _ = &mut consume => {}
                res = exit.wait_for(|v| v.is_some()) => {
                    let why = res.ok().and_then(|v| v.clone()).unwrap_or_default();
                    closed.store(true, Ordering::SeqCst);
                    let _ = ready_tx.send_if_modified(|s| {
                        if *s == ReadyState::Pending { *s = ReadyState::Failed(why.clone()); true } else { false }
                    });
                    let _ = tx.send(HubMessage::ServerExited(why));
                }
            }
        }
        None => consume.await,
    }
}

async fn consume(
    client: RawClient,
    timing: HubTiming,
    tx: broadcast::Sender<HubMessage>,
    ready_tx: Arc<watch::Sender<ReadyState>>,
) {
    let mut connected = false;
    let mut failures = 0u32;
    loop {
        let mut delivered = false;
        if let Ok(mut stream) = client.global_event().await {
            loop {
                let watchdog = if connected {
                    timing.watchdog
                } else {
                    timing.first_record_watchdog
                };
                match tokio::time::timeout(watchdog, stream.next()).await {
                    Ok(Some(Ok(frame))) => {
                        delivered = true;
                        let value = match serde_json::from_str(&frame.data) {
                            Ok(v) => v,
                            Err(_) => continue,
                        };
                        let event = GlobalEvent::from_value(value);
                        if !connected && matches!(event.payload.event, Event::ServerConnected) {
                            connected = true;
                            let _ = ready_tx.send(ReadyState::Ready);
                            continue;
                        }
                        // No subscribers is fine: events are only interesting while a session listens.
                        let _ = tx.send(HubMessage::Event(Arc::new(event)));
                    }
                    Ok(Some(Err(_))) | Ok(None) | Err(_) => break,
                }
            }
        }
        failures = if delivered { 0 } else { failures + 1 };
        let backoff =
            Duration::from_millis(100u64.saturating_mul(1 << failures.saturating_sub(1).min(10)));
        tokio::time::sleep(backoff.min(timing.max_backoff)).await;
    }
}
