//! An E2EE-decrypted relay data socket as a [`PhysicalSocket`], plus the writer task that
//! owns the sink. Port of `encrypted-relay-socket.ts`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use super::channel::{Action, DaemonChannel, PROTOCOL_ERROR_CODE};
use super::dial::WsStream;
use crate::error::{EdgeError, Result};
use crate::ws::{Message, PhysicalSocket};

/// Why the relay transport asked a data socket to close from the outside.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Shutdown {
    Running,
    /// `ws.close()` with no code (transport `stop`).
    Plain,
    /// `existing.close(1001, "Client disconnected")`.
    Coded(u16, String),
}

enum Outgoing {
    Frame(WsMessage, usize),
    Flush,
    Close(Option<CloseFrame>),
    Abort,
}

/// Handle to the task that owns the sink. `queued` counts bytes accepted but not yet written,
/// the stand-in for `ws.bufferedAmount`.
#[derive(Clone)]
pub(crate) struct Writer {
    tx: mpsc::UnboundedSender<Outgoing>,
    queued: Arc<AtomicUsize>,
}

impl Writer {
    pub(crate) fn spawn(mut sink: SplitSink<WsStream, WsMessage>) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<Outgoing>();
        let queued = Arc::new(AtomicUsize::new(0));
        let counter = queued.clone();
        tokio::spawn(async move {
            while let Some(item) = rx.recv().await {
                match item {
                    Outgoing::Frame(msg, len) => {
                        let result = sink.send(msg).await;
                        counter.fetch_sub(len, Ordering::Relaxed);
                        if result.is_err() {
                            return;
                        }
                    }
                    Outgoing::Flush => {
                        if sink.flush().await.is_err() {
                            return;
                        }
                    }
                    Outgoing::Close(frame) => {
                        let _ = sink.send(WsMessage::Close(frame)).await;
                        let _ = sink.close().await;
                        return;
                    }
                    Outgoing::Abort => return,
                }
            }
            let _ = sink.close().await;
        });
        Self { tx, queued }
    }

    pub(crate) fn queued_bytes(&self) -> usize {
        self.queued.load(Ordering::Relaxed)
    }

    pub(crate) fn send_data(&self, msg: Message) -> Result<()> {
        let (ws, len) = to_ws(msg);
        self.queued.fetch_add(len, Ordering::Relaxed);
        self.tx.send(Outgoing::Frame(ws, len)).map_err(|_| {
            self.queued.fetch_sub(len, Ordering::Relaxed);
            EdgeError::Relay("Encrypted relay socket is not open".into())
        })
    }

    pub(crate) fn flush(&self) {
        let _ = self.tx.send(Outgoing::Flush);
    }

    pub(crate) fn close(&self, code: Option<(u16, String)>) {
        let frame = code.and_then(|(code, reason)| close_frame(code, reason));
        let _ = self.tx.send(Outgoing::Close(frame));
    }

    /// `terminate()`: drop the connection without a close handshake.
    pub(crate) fn abort(&self) {
        let _ = self.tx.send(Outgoing::Abort);
    }
}

/// Codes that must never appear on the wire (RFC 6455 section 7.4.1) become a bare close.
fn close_frame(code: u16, reason: String) -> Option<CloseFrame> {
    if matches!(code, 1005 | 1006 | 1015) {
        return None;
    }
    Some(CloseFrame {
        code: CloseCode::from(code),
        reason: reason.into(),
    })
}

fn to_ws(msg: Message) -> (WsMessage, usize) {
    match msg {
        Message::Text(t) => {
            let len = t.len();
            (WsMessage::text(t), len)
        }
        Message::Binary(b) => {
            let len = b.len();
            (WsMessage::Binary(b), len)
        }
    }
}

/// Decrypted relay data socket handed to `WsProxy::run`.
pub struct RelaySocket {
    reader: SplitStream<WsStream>,
    writer: Writer,
    channel: DaemonChannel,
    shutdown: watch::Receiver<Shutdown>,
    max_buffered: usize,
    closed: bool,
}

impl RelaySocket {
    pub(crate) fn new(
        reader: SplitStream<WsStream>,
        writer: Writer,
        channel: DaemonChannel,
        shutdown: watch::Receiver<Shutdown>,
        max_buffered: usize,
    ) -> Self {
        Self {
            reader,
            writer,
            channel,
            shutdown,
            max_buffered,
            closed: false,
        }
    }

    fn finish_with_close(&mut self, code: u16, reason: String) {
        if !self.closed {
            self.closed = true;
            self.writer.close(Some((code, reason)));
        }
    }
}

impl PhysicalSocket for RelaySocket {
    async fn recv(&mut self) -> Option<Result<Message>> {
        loop {
            if self.closed {
                return None;
            }
            let frame = tokio::select! {
                changed = self.shutdown.changed() => {
                    if changed.is_err() {
                        self.closed = true;
                        self.writer.abort();
                        return None;
                    }
                    let signal = self.shutdown.borrow_and_update().clone();
                    match signal {
                        Shutdown::Running => continue,
                        Shutdown::Plain => {
                            self.closed = true;
                            self.writer.close(None);
                        }
                        Shutdown::Coded(code, reason) => self.finish_with_close(code, reason),
                    }
                    return None;
                }
                frame = self.reader.next() => frame,
            };
            let message = match frame {
                Some(Ok(WsMessage::Text(t))) => Message::Text(t.as_str().to_owned()),
                Some(Ok(WsMessage::Binary(b))) => Message::Binary(b),
                Some(Ok(WsMessage::Ping(_))) => {
                    self.writer.flush();
                    continue;
                }
                Some(Ok(WsMessage::Pong(_) | WsMessage::Frame(_))) => continue,
                Some(Ok(WsMessage::Close(_))) | None => {
                    self.closed = true;
                    return None;
                }
                Some(Err(e)) => {
                    self.closed = true;
                    self.writer.abort();
                    tracing::debug!(error = %e, "relay_data_read_error");
                    return None;
                }
            };
            match self.channel.on_frame(message) {
                Ok(Action::Deliver(m)) => return Some(Ok(m)),
                Ok(Action::Reply(m)) => {
                    let _ = self.writer.send_data(m);
                }
                Ok(Action::Ignore) => {}
                Ok(Action::Close { code, reason }) => {
                    self.finish_with_close(code, reason);
                    return None;
                }
                Err(e) => {
                    self.finish_with_close(PROTOCOL_ERROR_CODE, e.0);
                    return None;
                }
            }
        }
    }

    async fn send(&mut self, msg: Message) -> Result<()> {
        if self.closed {
            return Err(EdgeError::Relay(
                "Encrypted relay socket is not open".into(),
            ));
        }
        let wire_len = self.channel.outbound_wire_len(&msg);
        if self.writer.queued_bytes() + wire_len > self.max_buffered {
            self.closed = true;
            self.writer.abort();
            return Err(EdgeError::Relay(
                "Encrypted relay socket exceeded its outbound high-water mark".into(),
            ));
        }
        self.writer.send_data(self.channel.encode(&msg))
    }

    async fn close(&mut self, code: u16, reason: String) {
        self.finish_with_close(code, reason);
    }
}
