//! WebSocket transport for Soniox, behind a trait so the session logic can be driven by an
//! in-memory socket in tests (the TS tests inject a `createWebSocket` factory the same way).

use std::time::Duration;

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::Message;

use crate::error::{Result, SpeechError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WsOutgoing {
    Text(String),
    Binary(Vec<u8>),
    Close(u16),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WsIncoming {
    Text(String),
    Binary(Vec<u8>),
    Error(String),
    Closed { code: u16, reason: String },
}

/// An open socket: frames go out through `outgoing`, everything the server does arrives on `incoming`.
pub struct SonioxSocket {
    pub outgoing: mpsc::UnboundedSender<WsOutgoing>,
    pub incoming: mpsc::UnboundedReceiver<WsIncoming>,
}

#[async_trait]
pub trait SonioxConnector: Send + Sync {
    /// Resolves once the connection is open.
    async fn connect(&self, url: &str) -> Result<SonioxSocket>;
}

/// `tokio-tungstenite` over rustls.
pub struct TungsteniteConnector;

#[async_trait]
impl SonioxConnector for TungsteniteConnector {
    async fn connect(&self, url: &str) -> Result<SonioxSocket> {
        let (stream, _response) = tokio_tungstenite::connect_async(url)
            .await
            .map_err(|e| SpeechError::msg(e.to_string()))?;
        let (mut sink, mut source) = stream.split();
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<WsOutgoing>();
        let (in_tx, in_rx) = mpsc::unbounded_channel::<WsIncoming>();

        let writer_in = in_tx.clone();
        tokio::spawn(async move {
            while let Some(frame) = out_rx.recv().await {
                let result = match frame {
                    WsOutgoing::Text(text) => sink.send(Message::text(text)).await,
                    WsOutgoing::Binary(bytes) => sink.send(Message::binary(bytes)).await,
                    WsOutgoing::Close(code) => {
                        let close = Message::Close(Some(CloseFrame {
                            code: CloseCode::from(code),
                            reason: "".into(),
                        }));
                        let r = sink.send(close).await;
                        let _ = sink.close().await;
                        if let Err(err) = r {
                            let _ = writer_in.send(WsIncoming::Error(err.to_string()));
                        }
                        break;
                    }
                };
                if let Err(err) = result {
                    let _ = writer_in.send(WsIncoming::Error(err.to_string()));
                    break;
                }
            }
        });

        tokio::spawn(async move {
            loop {
                match source.next().await {
                    Some(Ok(Message::Text(text))) => {
                        if in_tx.send(WsIncoming::Text(text.to_string())).is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Binary(bytes))) => {
                        if in_tx.send(WsIncoming::Binary(bytes.to_vec())).is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Close(frame))) => {
                        let (code, reason) = frame
                            .map(|f| (u16::from(f.code), f.reason.to_string()))
                            .unwrap_or((1005, String::new()));
                        let _ = in_tx.send(WsIncoming::Closed { code, reason });
                        break;
                    }
                    Some(Ok(_)) => {}
                    Some(Err(err)) => {
                        let _ = in_tx.send(WsIncoming::Error(err.to_string()));
                        let _ = in_tx.send(WsIncoming::Closed {
                            code: 1006,
                            reason: String::new(),
                        });
                        break;
                    }
                    None => {
                        let _ = in_tx.send(WsIncoming::Closed {
                            code: 1006,
                            reason: String::new(),
                        });
                        break;
                    }
                }
            }
        });

        Ok(SonioxSocket {
            outgoing: out_tx,
            incoming: in_rx,
        })
    }
}

pub(crate) async fn connect_with_timeout(
    connector: &dyn SonioxConnector,
    url: &str,
    timeout: Duration,
) -> Result<SonioxSocket> {
    match tokio::time::timeout(timeout, connector.connect(url)).await {
        Ok(result) => result,
        Err(_) => Err(SpeechError::msg(format!(
            "Soniox WebSocket connection timed out after {}ms",
            timeout.as_millis()
        ))),
    }
}
