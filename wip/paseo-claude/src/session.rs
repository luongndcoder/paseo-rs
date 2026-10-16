//! Session driver: owns the transport, correlates control requests, runs callbacks, and
//! exposes conversation messages as a `Stream`.

use std::collections::HashMap;
use std::ops::Deref;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll};
use std::time::Duration;

use futures_core::Stream;
use serde_json::{json, Map, Value};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;

use crate::error::{Error, Result};
use crate::message::{SdkMessage, UserInput};
use crate::options::{Callbacks, ClaudeOptions};
use crate::process::ProcessTransport;
use crate::protocol::mcp::McpStatusResponse;
use crate::protocol::responses::SetPermissionModeResponse;
use crate::protocol::responses::{ModelInfo, SlashCommand};
use crate::protocol::{
    parse_response, ControlRequestBody, InitializeRequest, InitializeResponse, InterruptResponse,
    Line, McpServerStatus, PermissionMode, RewindFilesResult, SuccessResponse,
};
use crate::transport::{Transport, TransportIo};

const GRACEFUL_EXIT: Duration = Duration::from_secs(2);
const KILLED_EXIT: Duration = Duration::from_secs(6);

pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    match m.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}

#[derive(Clone)]
pub(crate) enum InitState {
    Pending,
    Ready(Arc<InitializeResponse>, Value),
    Failed(String),
}

type PendingMap = HashMap<String, (String, oneshot::Sender<Result<SuccessResponse>>)>;

pub(crate) struct Shared {
    stdin: Mutex<Option<mpsc::UnboundedSender<String>>>,
    pending: Mutex<PendingMap>,
    next_id: AtomicU64,
    pub(crate) inflight: Mutex<HashMap<String, CancellationToken>>,
    pub(crate) callbacks: Callbacks,
    init_tx: watch::Sender<InitState>,
    pub(crate) init_request_id: Mutex<Option<String>>,
    pub(crate) closing: AtomicBool,
    /// Latest `commands_changed` list; wins over the initialize-time list.
    pub(crate) latest_commands: Mutex<Option<Vec<SlashCommand>>>,
    /// `close_input` was called but stdin stays open until the first result, because callbacks
    /// still need to answer CLI requests (the SDK's `waitForFirstResult`).
    input_close_pending: AtomicBool,
    result_seen: AtomicBool,
    exited_tx: watch::Sender<bool>,
    pub(crate) shutdown: CancellationToken,
    control_timeout: Duration,
    kill: Box<dyn Fn() + Send + Sync>,
}

impl Shared {
    fn new_request_id(&self) -> String {
        let n = self.next_id.fetch_add(1, Ordering::Relaxed);
        let rnd = uuid::Uuid::new_v4().simple().to_string();
        format!("req_{n}_{}", &rnd[..8])
    }

    pub(crate) fn write_line(&self, line: &Line) -> Result<()> {
        let text = serde_json::to_string(line)?;
        let guard = lock(&self.stdin);
        match guard.as_ref() {
            Some(tx) => tx.send(text).map_err(|_| Error::Closed),
            None => Err(Error::Closed),
        }
    }

    pub(crate) fn close_stdin(&self) {
        lock(&self.stdin).take();
    }

    fn has_callbacks(&self) -> bool {
        let c = &self.callbacks;
        c.can_use_tool.is_some()
            || !c.hooks.is_empty()
            || !c.mcp_servers.is_empty()
            || c.elicitation.is_some()
    }

    /// End of input. With callbacks registered and no result yet, closing is deferred so
    /// responses to `can_use_tool` / hook / MCP requests can still be written.
    fn end_input(&self) {
        if self.has_callbacks() && !self.result_seen.load(Ordering::SeqCst) {
            self.input_close_pending.store(true, Ordering::SeqCst);
            return;
        }
        self.close_stdin();
    }

    pub(crate) fn on_result(&self) {
        self.result_seen.store(true, Ordering::SeqCst);
        if self.input_close_pending.swap(false, Ordering::SeqCst) {
            self.close_stdin();
        }
    }

    /// Register a pending request and write it. The receiver resolves with the response.
    fn send_control(
        &self,
        body: ControlRequestBody,
    ) -> Result<(String, oneshot::Receiver<Result<SuccessResponse>>)> {
        if self.shutdown.is_cancelled() {
            return Err(Error::Closed);
        }
        let id = self.new_request_id();
        let subtype = body.subtype().unwrap_or("unknown").to_string();
        let (tx, rx) = oneshot::channel();
        lock(&self.pending).insert(id.clone(), (subtype, tx));
        if let Err(e) = self.write_line(&Line::control_request(id.clone(), body)) {
            lock(&self.pending).remove(&id);
            return Err(e);
        }
        Ok((id, rx))
    }

    async fn await_control(
        &self,
        id: String,
        subtype: &str,
        rx: oneshot::Receiver<Result<SuccessResponse>>,
        timeout: Duration,
    ) -> Result<SuccessResponse> {
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err(Error::Closed),
            Err(_) => {
                lock(&self.pending).remove(&id);
                // Tell the CLI to stop working on it; failure just means we are closing.
                let _ = self.write_line(&Line::cancel_request(id));
                Err(Error::Timeout {
                    what: format!("control response to `{subtype}`"),
                    after: timeout,
                })
            }
        }
    }

    pub(crate) async fn request(
        &self,
        body: ControlRequestBody,
        timeout: Duration,
    ) -> Result<SuccessResponse> {
        let subtype = body.subtype().unwrap_or("unknown").to_string();
        let (id, rx) = self.send_control(body)?;
        self.await_control(id, &subtype, rx, timeout).await
    }

    pub(crate) fn resolve(&self, id: &str, result: Result<SuccessResponse>) {
        let entry = lock(&self.pending).remove(id);
        if let Some((_, tx)) = entry {
            let _ = tx.send(result);
        }
    }

    pub(crate) fn subtype_of(&self, id: &str) -> String {
        lock(&self.pending)
            .get(id)
            .map(|(s, _)| s.clone())
            .unwrap_or_else(|| "unknown".into())
    }

    /// Process ended: fail every waiter and stop callbacks.
    pub(crate) fn on_exit(&self) {
        self.shutdown.cancel();
        self.close_stdin();
        let drained: Vec<_> = lock(&self.pending).drain().collect();
        for (_, (_, tx)) in drained {
            let _ = tx.send(Err(Error::Closed));
        }
        // The initialize awaiter observes the drained pending map and records the failure.
        self.exited_tx.send_replace(true);
    }

    pub(crate) fn set_init(&self, state: InitState) {
        self.init_tx.send_replace(state);
    }

    async fn wait_init(&self) -> Result<(Arc<InitializeResponse>, Value)> {
        let mut rx = self.init_tx.subscribe();
        let wait = async {
            loop {
                match &*rx.borrow_and_update() {
                    InitState::Ready(t, raw) => return Ok((t.clone(), raw.clone())),
                    InitState::Failed(m) => return Err(Error::Protocol(m.clone())),
                    InitState::Pending => {}
                }
                if rx.changed().await.is_err() {
                    return Err(Error::Closed);
                }
            }
        };
        tokio::time::timeout(self.control_timeout, wait)
            .await
            .map_err(|_| Error::Timeout {
                what: "initialize response".into(),
                after: self.control_timeout,
            })?
    }
}

/// Last handle dropped: close stdin and terminate the process.
struct Guard(Arc<Shared>);

impl Drop for Guard {
    fn drop(&mut self) {
        self.0.closing.store(true, Ordering::SeqCst);
        self.0.close_stdin();
        (self.0.kill)();
    }
}

/// Cloneable control surface of a session. The process is terminated when the last handle
/// (including the one inside [`Session`]) is dropped.
#[derive(Clone)]
pub struct SessionHandle {
    shared: Arc<Shared>,
    _guard: Arc<Guard>,
}

/// Conversation message stream; ends when the process exits. An `Err` item is yielded last
/// when the process died abnormally (and was not closed on purpose).
pub struct MessageStream {
    rx: mpsc::UnboundedReceiver<Result<SdkMessage>>,
}

impl MessageStream {
    pub async fn next_message(&mut self) -> Option<Result<SdkMessage>> {
        self.rx.recv().await
    }
}

impl Stream for MessageStream {
    type Item = Result<SdkMessage>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.rx.poll_recv(cx)
    }
}

/// A running `claude` session: a [`MessageStream`] plus a [`SessionHandle`] (via `Deref`).
pub struct Session {
    handle: SessionHandle,
    stream: MessageStream,
}

impl Deref for Session {
    type Target = SessionHandle;

    fn deref(&self) -> &SessionHandle {
        &self.handle
    }
}

impl Stream for Session {
    type Item = Result<SdkMessage>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.stream).poll_next(cx)
    }
}

impl Session {
    /// Spawn the real `claude` CLI. Must be called inside a tokio runtime.
    pub fn start(options: ClaudeOptions) -> Result<Session> {
        let prepared = options.prepare()?;
        let transport = ProcessTransport::new(prepared.spawn.clone());
        Self::launch(
            prepared.init,
            prepared.callbacks,
            prepared.control_timeout,
            Box::new(transport),
        )
    }

    /// Drive any [`Transport`]; process-related options (executable, argv, env) are ignored.
    pub fn start_with_transport(
        options: ClaudeOptions,
        transport: Box<dyn Transport>,
    ) -> Result<Session> {
        let prepared = options.prepare()?;
        Self::launch(
            prepared.init,
            prepared.callbacks,
            prepared.control_timeout,
            transport,
        )
    }

    fn launch(
        init: InitializeRequest,
        callbacks: Callbacks,
        control_timeout: Duration,
        transport: Box<dyn Transport>,
    ) -> Result<Session> {
        let TransportIo {
            stdin,
            events,
            kill,
        } = transport.start()?;
        let (init_tx, _) = watch::channel(InitState::Pending);
        let (exited_tx, _) = watch::channel(false);
        let shared = Arc::new(Shared {
            stdin: Mutex::new(Some(stdin)),
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(0),
            inflight: Mutex::new(HashMap::new()),
            callbacks,
            init_tx,
            init_request_id: Mutex::new(None),
            closing: AtomicBool::new(false),
            latest_commands: Mutex::new(None),
            input_close_pending: AtomicBool::new(false),
            result_seen: AtomicBool::new(false),
            exited_tx,
            shutdown: CancellationToken::new(),
            control_timeout,
            kill,
        });
        let handle = SessionHandle {
            shared: shared.clone(),
            _guard: Arc::new(Guard(shared.clone())),
        };

        // `initialize` goes out before anything else can be written.
        let (init_id, init_rx) = shared.send_control(ControlRequestBody::Initialize(init))?;
        *lock(&shared.init_request_id) = Some(init_id.clone());

        let (msg_tx, msg_rx) = mpsc::unbounded_channel();
        tokio::spawn(crate::dispatch::run_reader(shared.clone(), events, msg_tx));

        let s = shared.clone();
        tokio::spawn(async move {
            let state = match s
                .await_control(init_id, "initialize", init_rx, s.control_timeout)
                .await
            {
                Ok(success) => {
                    let raw = success.response.clone().unwrap_or_else(|| json!({}));
                    match parse_response::<InitializeResponse>(&success)
                        .or_else(|| serde_json::from_value(json!({})).ok())
                    {
                        Some(t) => InitState::Ready(Arc::new(t), raw),
                        None => InitState::Failed("malformed initialize response".into()),
                    }
                }
                Err(e) => InitState::Failed(e.to_string()),
            };
            s.set_init(state);
        });

        Ok(Session {
            handle,
            stream: MessageStream { rx: msg_rx },
        })
    }

    pub async fn next_message(&mut self) -> Option<Result<SdkMessage>> {
        self.stream.next_message().await
    }

    pub fn handle(&self) -> SessionHandle {
        self.handle.clone()
    }

    /// Separate the control handle from the message stream.
    pub fn split(self) -> (SessionHandle, MessageStream) {
        (self.handle, self.stream)
    }
}

fn expect_payload<T: serde::de::DeserializeOwned>(
    subtype: &str,
    success: SuccessResponse,
) -> Result<T> {
    let payload = success
        .response
        .ok_or_else(|| Error::Protocol(format!("`{subtype}` response had no payload")))?;
    serde_json::from_value(payload)
        .map_err(|e| Error::Protocol(format!("`{subtype}` response did not parse: {e}")))
}

impl SessionHandle {
    /// Send a user turn. Returns the message uuid (generated when the input has none); keep it
    /// to `rewind_files` to this message later.
    pub fn send_user_message(&self, input: impl Into<UserInput>) -> Result<String> {
        let (msg, uuid) = input
            .into()
            .into_message(|| uuid::Uuid::new_v4().to_string());
        self.shared.write_line(&Line::User(msg))?;
        Ok(uuid)
    }

    /// End of input: the CLI finishes queued turns and exits. When callbacks are registered,
    /// stdin is closed after the first result instead of immediately, so permission, hook and
    /// MCP replies for that turn can still be written.
    pub fn close_input(&self) {
        self.shared.end_input();
    }

    /// Raw control request.
    pub async fn control(&self, body: ControlRequestBody) -> Result<Option<Value>> {
        Ok(self
            .shared
            .request(body, self.shared.control_timeout)
            .await?
            .response)
    }

    pub async fn interrupt(&self) -> Result<InterruptResponse> {
        self.interrupt_with(false).await
    }

    /// `cancel_queued`: also drop queued user messages.
    pub async fn interrupt_with(&self, cancel_queued: bool) -> Result<InterruptResponse> {
        let s = self
            .shared
            .request(
                ControlRequestBody::Interrupt {
                    cancel_queued: cancel_queued.then_some(true),
                    extra: Default::default(),
                },
                self.shared.control_timeout,
            )
            .await?;
        // Older CLIs reply without a payload.
        Ok(parse_response(&s).unwrap_or_else(|| InterruptResponse {
            still_queued: Vec::new(),
            cancelled: None,
            extra: Default::default(),
        }))
    }

    pub async fn set_permission_mode(&self, mode: PermissionMode) -> Result<PermissionMode> {
        let requested = mode.clone();
        let s = self
            .shared
            .request(
                ControlRequestBody::SetPermissionMode {
                    mode,
                    extra: Default::default(),
                },
                self.shared.control_timeout,
            )
            .await?;
        Ok(parse_response::<SetPermissionModeResponse>(&s)
            .map(|r| r.mode)
            .unwrap_or(requested))
    }

    /// `None` resets to the default model.
    pub async fn set_model(&self, model: Option<&str>) -> Result<()> {
        self.shared
            .request(
                ControlRequestBody::SetModel {
                    model: model.map(|m| Some(m.to_string())),
                    extra: Default::default(),
                },
                self.shared.control_timeout,
            )
            .await?;
        Ok(())
    }

    /// Needs `enable_file_checkpointing(true)` and a user message sent with a uuid.
    pub async fn rewind_files(
        &self,
        user_message_id: &str,
        dry_run: bool,
    ) -> Result<RewindFilesResult> {
        let s = self
            .shared
            .request(
                ControlRequestBody::RewindFiles {
                    user_message_id: user_message_id.to_string(),
                    dry_run: dry_run.then_some(true),
                    extra: Default::default(),
                },
                self.shared.control_timeout,
            )
            .await?;
        expect_payload("rewind_files", s)
    }

    pub async fn mcp_server_status(&self) -> Result<Vec<McpServerStatus>> {
        let s = self
            .shared
            .request(
                ControlRequestBody::McpStatus {},
                self.shared.control_timeout,
            )
            .await?;
        Ok(expect_payload::<McpStatusResponse>("mcp_status", s)?.mcp_servers)
    }

    /// Merge into the flag settings layer (e.g. `{"fastMode": true}`).
    pub async fn apply_flag_settings(&self, settings: Map<String, Value>) -> Result<()> {
        self.shared
            .request(
                ControlRequestBody::ApplyFlagSettings {
                    settings,
                    extra: Default::default(),
                },
                self.shared.control_timeout,
            )
            .await?;
        Ok(())
    }

    /// The `initialize` response (waits for it).
    pub async fn initialize_response(&self) -> Result<Arc<InitializeResponse>> {
        Ok(self.shared.wait_init().await?.0)
    }

    /// The `initialize` response payload as received.
    pub async fn initialize_response_raw(&self) -> Result<Value> {
        Ok(self.shared.wait_init().await?.1)
    }

    pub async fn supported_commands(&self) -> Result<Vec<SlashCommand>> {
        let init = self.initialize_response().await?;
        Ok(lock(&self.shared.latest_commands)
            .clone()
            .unwrap_or_else(|| init.commands.clone()))
    }

    pub async fn supported_models(&self) -> Result<Vec<ModelInfo>> {
        Ok(self.initialize_response().await?.models.clone())
    }

    /// Tell the CLI an in-process MCP server's tool list changed. Fire and forget: the
    /// CLI's reply to a host-sent `mcp_message` is not awaited.
    pub fn notify_mcp_tools_changed(&self, server_name: &str) -> Result<()> {
        let id = self.shared.new_request_id();
        self.shared.write_line(&Line::control_request(
            id,
            ControlRequestBody::McpMessage {
                server_name: server_name.to_string(),
                message: json!({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"}),
                extra: Default::default(),
            },
        ))
    }

    /// Wait for the process to exit.
    pub async fn wait_exit(&self) {
        let mut rx = self.shared.exited_tx.subscribe();
        while !*rx.borrow_and_update() {
            if rx.changed().await.is_err() {
                return;
            }
        }
    }

    /// Graceful stop: close stdin, wait ~2 s, then terminate. No error item is produced for
    /// the resulting exit status.
    pub async fn close(&self) {
        self.shared.closing.store(true, Ordering::SeqCst);
        self.shared.close_stdin();
        if tokio::time::timeout(GRACEFUL_EXIT, self.wait_exit())
            .await
            .is_err()
        {
            (self.shared.kill)();
            let _ = tokio::time::timeout(KILLED_EXIT, self.wait_exit()).await;
        }
    }

    /// Terminate immediately (SIGTERM to the process group, SIGKILL after 5 s).
    pub fn kill(&self) {
        self.shared.closing.store(true, Ordering::SeqCst);
        (self.shared.kill)();
    }

    pub fn is_closed(&self) -> bool {
        self.shared.shutdown.is_cancelled()
    }
}
