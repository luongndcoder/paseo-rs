//! `paseod` binary: edge entrypoint.
//!
//! Startup order (EDGE-SPEC.md "Startup"):
//! 1. [`paseod::config::EdgeConfig::from_env`]
//! 2. bind the public listener (requests get 503 until the backend is `Ready`)
//! 3. [`paseod::backend::Backend::spawn`] (TS worker on an internal loopback port)
//! 4. announce `paseo:ready` on stdout once the backend is ready (the supervisor's readiness
//!    signal); worker `paseo:shutdown`/`paseo:restart` events are forwarded the same way
//!
//! stdout carries only `@@paseo ` lifecycle lines and the worker's passed-through output; the
//! edge's own logs go to stderr.

use std::sync::Arc;
use std::time::Duration;

use paseod::backend::{Backend, BackendState};
use paseod::config::EdgeConfig;
use paseod::http::{self, AppState};
use paseod::internal::{LifecycleEvent, LIFECYCLE_PREFIX};
use paseod::relay::RelaySwitch;
use paseod::routing::OwnedTypes;
use paseod::ws::WsProxy;
use tokio::sync::watch;

/// Grace for in-flight HTTP responses (SSE/MCP streams never finish on their own).
const HTTP_DRAIN: Duration = Duration::from_secs(5);

fn emit(event: &LifecycleEvent) {
    if let Ok(json) = serde_json::to_string(event) {
        println!("{LIFECYCLE_PREFIX}{json}");
    }
}

#[cfg(unix)]
async fn termination_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let (Ok(mut term), Ok(mut int)) = (
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
    ) else {
        std::future::pending::<()>().await;
        return;
    };
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
}

#[cfg(not(unix))]
async fn termination_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    let env = |key: &str| std::env::var(key).ok();
    let config = Arc::new(EdgeConfig::from_env(&env)?);

    let listener = http::bind(&config.listen).await?;
    let public = listener.public_address();
    tracing::info!(listen = %public, "paseod listening");

    let backend = match &config.attach {
        Some(attach) => Backend::fixed(attach.addr, attach.token.clone()),
        None => Backend::spawn(&config.backend, &public).await?,
    };
    let ws = Arc::new(WsProxy::new(
        backend.clone(),
        Arc::new(OwnedTypes::empty()),
        config.limits.clone(),
    ));
    let relay = Arc::new(RelaySwitch::new(
        config.relay.clone(),
        config.paseo_home.clone(),
        ws.clone(),
    ));
    if config.relay.enabled {
        let relay = relay.clone();
        let backend = backend.clone();
        tokio::spawn(async move {
            if let Err(err) = relay.set_enabled(true, &backend).await {
                tracing::warn!(%err, "relay failed to start");
            }
        });
    }
    let state = AppState::new(config.clone(), backend.clone(), ws);

    // First writer wins: the exit code the process ends with.
    let (stop_tx, stop_rx) = watch::channel::<Option<i32>>(None);
    let request_stop = {
        let stop_tx = stop_tx.clone();
        move |code: i32| {
            stop_tx.send_if_modified(|current| {
                if current.is_none() {
                    *current = Some(code);
                    true
                } else {
                    false
                }
            });
        }
    };

    tokio::spawn({
        let request_stop = request_stop.clone();
        async move {
            termination_signal().await;
            tracing::info!("termination signal received");
            request_stop(0);
        }
    });

    // Backend state -> supervisor readiness line, or exit on failure.
    tokio::spawn({
        let request_stop = request_stop.clone();
        let mut state_rx = backend.state();
        let public = public.clone();
        async move {
            let mut last_ready = None;
            loop {
                let current = state_rx.borrow_and_update().clone();
                match current {
                    BackendState::Ready(info) if last_ready.as_ref() != Some(&info) => {
                        emit(&LifecycleEvent::Ready {
                            listen: public.clone(),
                            server_id: info.server_id.clone(),
                        });
                        last_ready = Some(info);
                    }
                    BackendState::Failed { reason } => {
                        tracing::error!(%reason, "backend failed; exiting");
                        request_stop(1);
                        return;
                    }
                    _ => {}
                }
                if state_rx.changed().await.is_err() {
                    return;
                }
            }
        }
    });

    // Worker shutdown/restart requests go to our supervisor, then we stop.
    tokio::spawn({
        let request_stop = request_stop.clone();
        let mut events = backend.subscribe_lifecycle();
        async move {
            while let Ok(event) = events.recv().await {
                emit(&event);
                request_stop(0);
            }
        }
    });

    let shutdown = {
        let mut rx = stop_rx.clone();
        async move {
            let _ = rx.wait_for(|v| v.is_some()).await;
        }
    };
    let mut server = tokio::spawn(http::serve_on(state, listener, shutdown));

    let mut wait_rx = stop_rx.clone();
    let code = tokio::select! {
        _ = wait_rx.wait_for(|v| v.is_some()) => {
            let code = (*stop_rx.borrow()).unwrap_or(0);
            if let Err(err) = backend.shutdown().await {
                tracing::warn!(%err, "backend shutdown");
            }
            if tokio::time::timeout(HTTP_DRAIN, &mut server).await.is_err() {
                tracing::warn!("HTTP drain timed out; dropping open connections");
                server.abort();
            }
            code
        }
        result = &mut server => {
            let _ = backend.shutdown().await;
            match result {
                Ok(Ok(())) => 0,
                Ok(Err(err)) => {
                    tracing::error!(%err, "http server stopped");
                    1
                }
                Err(err) => {
                    tracing::error!(%err, "http server task failed");
                    1
                }
            }
        }
    };
    std::process::exit(code);
}
