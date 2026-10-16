//! `EventHub::close()` must deliver `ServerExited` to every subscriber, exactly once, and fail
//! a pending `ready()`.

use async_trait::async_trait;
use paseo_opencode::client::RawClient;
use paseo_opencode::error::{Error, Result};
use paseo_opencode::events::{EventHub, HubMessage, HubTiming};
use paseo_opencode::transport::{HttpRequest, HttpResponse, SseStream, Transport};
use std::sync::Arc;
use std::time::Duration;

/// Opens an SSE stream that never yields: the hub stays `Pending`.
struct SilentTransport;

#[async_trait]
impl Transport for SilentTransport {
    async fn send(&self, _req: HttpRequest) -> Result<HttpResponse> {
        Err(Error::ServerExited("unused".into()))
    }
    async fn open_sse(&self, _req: HttpRequest) -> Result<SseStream> {
        Ok(Box::pin(futures_util::stream::pending()))
    }
}

fn hub() -> Arc<EventHub> {
    EventHub::spawn(
        RawClient::new(Arc::new(SilentTransport), None),
        HubTiming::default(),
        None,
    )
}

async fn expect_exit(rx: &mut tokio::sync::broadcast::Receiver<HubMessage>) -> String {
    match tokio::time::timeout(Duration::from_secs(1), rx.recv()).await {
        Ok(Ok(HubMessage::ServerExited(why))) => why,
        other => panic!("expected ServerExited, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn close_delivers_server_exited_to_all_subscribers() {
    let hub = hub();
    let mut subs: Vec<_> = (0..8).map(|_| hub.subscribe()).collect();
    assert!(!hub.is_closed());
    hub.close();
    assert!(hub.is_closed());
    for rx in &mut subs {
        assert_eq!(expect_exit(rx).await, "OpenCode server stopped");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn close_is_idempotent_and_sends_one_message() {
    let hub = hub();
    let mut rx = hub.subscribe();
    hub.close();
    hub.close();
    expect_exit(&mut rx).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(100), rx.recv())
            .await
            .is_err(),
        "second close() must not send another ServerExited"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn close_fails_pending_ready() {
    let hub = hub();
    let waiter = {
        let hub = hub.clone();
        tokio::spawn(async move { hub.ready(Duration::from_secs(1)).await })
    };
    tokio::task::yield_now().await;
    hub.close();
    match waiter.await.unwrap() {
        Err(Error::ServerExited(_)) => {}
        other => panic!("expected ServerExited from ready(), got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_close_delivers_exactly_one_message() {
    for _ in 0..50 {
        let hub = hub();
        let mut rx = hub.subscribe();
        let barrier = Arc::new(tokio::sync::Barrier::new(4));
        let tasks: Vec<_> = (0..4)
            .map(|_| {
                let (hub, barrier) = (hub.clone(), barrier.clone());
                tokio::spawn(async move {
                    barrier.wait().await;
                    hub.close();
                })
            })
            .collect();
        for t in tasks {
            t.await.unwrap();
        }
        expect_exit(&mut rx).await;
        assert!(matches!(
            rx.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));
    }
}
