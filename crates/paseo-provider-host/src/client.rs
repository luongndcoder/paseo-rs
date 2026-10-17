use std::collections::{HashMap, VecDeque};
use std::marker::PhantomData;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, oneshot};

use crate::config::HostConfig;
use crate::error::{Error, Result};
use crate::permission::{PermissionContext, PermissionHandler};
use crate::session::{EventStream, Session};
use crate::wire::{
    AgentPersistenceHandle, AgentSessionConfig, AgentStreamEvent, FetchCatalogOptions,
    PermissionResponse, ProviderCatalog, ProviderInfo, SessionInfo,
};
use crate::PROTOCOL_VERSION;
use paseo_provider_api::{ToolCallHandler, ToolCallRequest};

const STDERR_TAIL_LIMIT: usize = 8 * 1024;

/// One `session.event` notification.
#[derive(Debug, Clone)]
pub struct SessionEvent {
    pub handle: String,
    /// Per-session, strictly increasing from 1.
    pub seq: u64,
    pub event: AgentStreamEvent,
    /// The event JSON exactly as the host sent it.
    pub raw: Value,
    /// Produced by this client (host exit, sequence gap), not sent by the host. `seq` is the
    /// last host sequence number seen.
    pub synthetic: bool,
}

/// Optional reverse-call handlers for host -> client requests.
#[derive(Default, Clone)]
pub struct HostCallbacks {
    /// Answers `tool.call` (internal Paseo tools exposed to providers). Without one the host
    /// gets `-32601`.
    pub tool_handler: Option<Arc<dyn ToolCallHandler>>,
}

/// What a pending request does to client state when its response arrives.
#[derive(Clone)]
enum EntryKind {
    Plain,
    /// The response carries a session whose event channel must exist before any later event.
    RegistersSession,
    /// The response carries a `turnId` for this handle.
    StartTurn(String),
}

struct PendingTable {
    /// Set when the host is gone; later requests fail with it.
    closed: Option<(String, String)>,
    map: HashMap<u64, PendingEntry>,
}

struct PendingEntry {
    tx: oneshot::Sender<Result<Value>>,
    kind: EntryKind,
}

const FINISHED_TURNS_KEPT: usize = 64;

pub(crate) struct Channel {
    /// `None` once the session ended; the receiver still drains what was queued.
    tx: Option<mpsc::UnboundedSender<SessionEvent>>,
    rx: Option<mpsc::UnboundedReceiver<SessionEvent>>,
    handle: String,
    provider: String,
    last_seq: u64,
    /// Turns that started and have no terminal event yet (`None` = turn without an id).
    active: Vec<Option<String>>,
    finished: VecDeque<String>,
    /// A sequence gap ended this session; later events are ignored.
    dead: bool,
}

impl Channel {
    fn new(handle: String, provider: String) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            tx: Some(tx),
            rx: Some(rx),
            handle,
            provider,
            last_seq: 0,
            active: Vec::new(),
            finished: VecDeque::new(),
            dead: false,
        }
    }

    fn mark_active(&mut self, turn_id: Option<&str>) {
        let key = turn_id.map(str::to_owned);
        if let Some(id) = &key {
            if self.finished.contains(id) {
                return;
            }
        }
        if !self.active.contains(&key) {
            self.active.push(key);
        }
    }

    fn mark_finished(&mut self, turn_id: Option<&str>) {
        match turn_id {
            None => self.active.clear(),
            Some(id) => {
                let before = self.active.len();
                self.active.retain(|active| active.as_deref() != Some(id));
                if self.active.len() == before {
                    self.active.retain(|active| active.is_some());
                }
                self.finished.push_back(id.to_owned());
                if self.finished.len() > FINISHED_TURNS_KEPT {
                    self.finished.pop_front();
                }
            }
        }
    }

    /// One `turn_failed` per active turn (or one without an id when none is active), then the
    /// stream ends.
    fn fail(&mut self, error: &str, code: Option<&str>) {
        let turns: Vec<Option<String>> = if self.active.is_empty() {
            vec![None]
        } else {
            std::mem::take(&mut self.active)
        };
        for turn_id in turns {
            let event = AgentStreamEvent::TurnFailed {
                provider: self.provider.clone(),
                error: error.to_owned(),
                code: code.map(str::to_owned),
                diagnostic: None,
                turn_id,
            };
            let raw = serde_json::to_value(&event).unwrap_or(Value::Null);
            if let Some(tx) = &self.tx {
                let _ = tx.send(SessionEvent {
                    handle: self.handle.clone(),
                    seq: self.last_seq,
                    event,
                    raw,
                    synthetic: true,
                });
            }
        }
        self.end();
    }

    /// Ends the stream after the queued events.
    fn end(&mut self) {
        self.tx = None;
        self.dead = true;
    }
}

pub(crate) struct Inner {
    stdin: tokio::sync::Mutex<Option<ChildStdin>>,
    child: tokio::sync::Mutex<Option<Child>>,
    next_id: AtomicU64,
    pending: Mutex<PendingTable>,
    channels: Mutex<HashMap<String, Channel>>,
    stderr_tail: Arc<Mutex<String>>,
    handler: Option<Arc<dyn PermissionHandler>>,
    tool_handler: Option<Arc<dyn ToolCallHandler>>,
    config: HostConfig,
}

/// In-memory host calls that answer immediately. Only these get the control timeout; every
/// other method may spawn provider CLIs or wait on the user, so it runs unbounded.
fn is_short(method: &str) -> bool {
    matches!(
        method,
        "host.initialize"
            | "provider.list"
            | "session.getRuntimeInfo"
            | "session.getAvailableModes"
            | "session.getCurrentMode"
            | "session.getPendingPermissions"
            | "session.describePersistence"
            | "session.setMode"
            | "session.setModel"
            | "session.setThinkingOption"
            | "session.setFeature"
    )
}

/// An in-flight request that can be canceled. Dropping it does not cancel the request.
pub struct Pending<T> {
    id: u64,
    rx: oneshot::Receiver<Result<Value>>,
    inner: Arc<Inner>,
    timeout: Option<Duration>,
    _marker: PhantomData<fn() -> T>,
}

impl<T: DeserializeOwned> Pending<T> {
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Ask the host to cancel this request (`$/cancel`). For `session.run` the host interrupts
    /// the turn; the request then resolves with `canceled: true` or a `CANCELED` error.
    pub async fn cancel(&self) -> Result<()> {
        self.inner
            .write_line(&json!({"method": "$/cancel", "params": {"id": self.id}}))
            .await
    }

    pub async fn wait(self) -> Result<T> {
        let Pending {
            id,
            rx,
            inner,
            timeout,
            ..
        } = self;
        let received = match timeout {
            Some(limit) => match tokio::time::timeout(limit, rx).await {
                Ok(received) => received,
                Err(_) => {
                    inner.pending.lock().expect("pending lock").map.remove(&id);
                    let _ = inner
                        .write_line(&json!({"method": "$/cancel", "params": {"id": id}}))
                        .await;
                    return Err(Error::Timeout(limit));
                }
            },
            None => rx.await,
        };
        let value = received.map_err(|_| inner.exited_error())??;
        Ok(serde_json::from_value(value)?)
    }
}

impl Inner {
    fn exited_error(&self) -> Error {
        let reason = self
            .pending
            .lock()
            .expect("pending lock")
            .closed
            .as_ref()
            .map(|(reason, _)| reason.clone())
            .unwrap_or_else(|| "request dropped".to_owned());
        Error::HostExited {
            reason,
            stderr_tail: self.stderr_tail.lock().expect("stderr lock").clone(),
        }
    }

    /// A host that died before our write surfaces as EPIPE before the reader task has recorded
    /// the exit. Prefer the recorded exit (reason + stderr tail) once it lands.
    async fn prefer_exit(&self, error: Error) -> Error {
        if !matches!(&error, Error::Io(io) if io.kind() == std::io::ErrorKind::BrokenPipe) {
            return error;
        }
        for _ in 0..100 {
            if self.pending.lock().expect("pending lock").closed.is_some() {
                return self.exited_error();
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        error
    }

    async fn write_line(&self, message: &Value) -> Result<()> {
        let mut line = serde_json::to_string(message)?;
        line.push('\n');
        let mut guard = self.stdin.lock().await;
        let stdin = guard.as_mut().ok_or_else(|| self.exited_error())?;
        stdin.write_all(line.as_bytes()).await?;
        stdin.flush().await?;
        Ok(())
    }

    pub(crate) async fn request<T: DeserializeOwned>(
        self: &Arc<Self>,
        method: &str,
        params: Value,
    ) -> Result<Pending<T>> {
        let kind = match method {
            "provider.createSession" | "provider.resumeSession" | "provider.importSession" => {
                EntryKind::RegistersSession
            }
            "session.startTurn" => params
                .get("handle")
                .and_then(Value::as_str)
                .map_or(EntryKind::Plain, |handle| {
                    EntryKind::StartTurn(handle.to_owned())
                }),
            _ => EntryKind::Plain,
        };
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        {
            let mut table = self.pending.lock().expect("pending lock");
            if table.closed.is_some() {
                drop(table);
                return Err(self.exited_error());
            }
            table.map.insert(id, PendingEntry { tx, kind });
        }
        let frame = json!({"id": id, "method": method, "params": params});
        if let Err(error) = self.write_line(&frame).await {
            self.pending.lock().expect("pending lock").map.remove(&id);
            return Err(self.prefer_exit(error).await);
        }
        Ok(Pending {
            id,
            rx,
            inner: Arc::clone(self),
            timeout: if is_short(method) {
                self.config.control_timeout
            } else {
                None
            },
            _marker: PhantomData,
        })
    }

    pub(crate) async fn call<T: DeserializeOwned>(
        self: &Arc<Self>,
        method: &str,
        params: Value,
    ) -> Result<T> {
        self.request(method, params).await?.wait().await
    }

    pub(crate) fn take_events(&self, handle: &str) -> Option<EventStream> {
        let mut channels = self.channels.lock().expect("channels lock");
        channels
            .get_mut(handle)
            .and_then(|channel| channel.rx.take())
            .map(EventStream::new)
    }

    pub(crate) fn forget_session(&self, handle: &str) {
        self.channels.lock().expect("channels lock").remove(handle);
    }

    fn fail_all(&self, reason: &str) {
        let senders = {
            let mut table = self.pending.lock().expect("pending lock");
            table.closed = Some((reason.to_owned(), String::new()));
            std::mem::take(&mut table.map)
        };
        for (_, entry) in senders {
            let _ = entry.tx.send(Err(self.exited_error()));
        }
        // One terminal event per active turn, then dropping the senders ends every stream.
        let mut channels = self.channels.lock().expect("channels lock");
        for channel in channels.values_mut() {
            if !channel.dead && !channel.active.is_empty() {
                channel.fail("Provider host exited", None);
            }
            channel.end();
        }
    }

    /// Write the response to a host -> client request.
    async fn respond(&self, id: Value, outcome: std::result::Result<Value, (i64, String)>) {
        let frame = match outcome {
            Ok(result) => json!({"id": id, "result": result}),
            Err((code, message)) => json!({"id": id, "error": {"code": code, "message": message}}),
        };
        if let Err(error) = self.write_line(&frame).await {
            tracing::warn!(%error, "failed to answer a host request");
        }
    }
}

/// Handle to a running provider host process. Cheap to clone; the process is killed when the
/// last clone (and every [`Session`]) is dropped.
#[derive(Clone)]
pub struct ProviderHostClient {
    inner: Arc<Inner>,
    providers: Arc<Vec<ProviderInfo>>,
}

impl ProviderHostClient {
    /// Spawn the host and complete `host.initialize`.
    pub async fn spawn(
        config: HostConfig,
        handler: Option<Arc<dyn PermissionHandler>>,
    ) -> Result<Self> {
        Self::spawn_with(config, handler, HostCallbacks::default()).await
    }

    /// [`ProviderHostClient::spawn`] with reverse-call handlers (`tool.call`).
    pub async fn spawn_with(
        config: HostConfig,
        handler: Option<Arc<dyn PermissionHandler>>,
        callbacks: HostCallbacks,
    ) -> Result<Self> {
        let mut command = Command::new(&config.command);
        command
            .args(&config.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(cwd) = &config.cwd {
            command.current_dir(cwd);
        }
        for (key, value) in &config.env {
            command.env(key, value);
        }
        let mut child = command.spawn().map_err(Error::Spawn)?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::Protocol("no stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::Protocol("no stdout".into()))?;
        let mut stderr = child
            .stderr
            .take()
            .ok_or_else(|| Error::Protocol("no stderr".into()))?;

        let stderr_tail = Arc::new(Mutex::new(String::new()));
        let tail = Arc::clone(&stderr_tail);
        let stderr_task = tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            while let Ok(n) = stderr.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                let text = String::from_utf8_lossy(&buf[..n]).into_owned();
                tracing::debug!(target: "paseo_provider_host::stderr", "{}", text.trim_end());
                let mut tail = tail.lock().expect("stderr lock");
                tail.push_str(&text);
                if tail.len() > STDERR_TAIL_LIMIT {
                    let mut cut = tail.len() - STDERR_TAIL_LIMIT;
                    while !tail.is_char_boundary(cut) {
                        cut += 1;
                    }
                    tail.drain(..cut);
                }
            }
        });

        let initialize = serde_json::to_value(&config.initialize)?;
        let inner = Arc::new(Inner {
            stdin: tokio::sync::Mutex::new(Some(stdin)),
            child: tokio::sync::Mutex::new(Some(child)),
            next_id: AtomicU64::new(1),
            pending: Mutex::new(PendingTable {
                closed: None,
                map: HashMap::new(),
            }),
            channels: Mutex::new(HashMap::new()),
            stderr_tail,
            handler,
            tool_handler: callbacks.tool_handler,
            config,
        });
        tokio::spawn(read_loop(stdout, Arc::downgrade(&inner), stderr_task));

        let mut params = initialize;
        params["protocolVersion"] = json!(PROTOCOL_VERSION);
        #[derive(serde::Deserialize)]
        struct InitResult {
            #[serde(rename = "protocolVersion")]
            protocol_version: u32,
            providers: Vec<ProviderInfo>,
        }
        let init: InitResult = inner.call("host.initialize", params).await?;
        if init.protocol_version != PROTOCOL_VERSION {
            return Err(Error::Protocol(format!(
                "host speaks protocol {}, client speaks {PROTOCOL_VERSION}",
                init.protocol_version
            )));
        }
        Ok(Self {
            inner,
            providers: Arc::new(init.providers),
        })
    }

    /// Providers reported by `host.initialize`.
    pub fn providers(&self) -> &[ProviderInfo] {
        &self.providers
    }

    /// Escape hatch for methods without a typed wrapper.
    pub async fn call_raw(&self, method: &str, params: Value) -> Result<Value> {
        self.inner.call(method, params).await
    }

    pub async fn request_raw(&self, method: &str, params: Value) -> Result<Pending<Value>> {
        self.inner.request(method, params).await
    }

    pub async fn list_providers(&self) -> Result<Vec<ProviderInfo>> {
        #[derive(serde::Deserialize)]
        struct R {
            providers: Vec<ProviderInfo>,
        }
        Ok(self
            .inner
            .call::<R>("provider.list", json!({}))
            .await?
            .providers)
    }

    pub async fn is_available(
        &self,
        provider: &str,
        options: Option<&FetchCatalogOptions>,
    ) -> Result<bool> {
        #[derive(serde::Deserialize)]
        struct R {
            available: bool,
        }
        let r: R = self
            .inner
            .call(
                "provider.isAvailable",
                json!({"provider": provider, "options": options}),
            )
            .await?;
        Ok(r.available)
    }

    pub async fn fetch_catalog(
        &self,
        provider: &str,
        options: &FetchCatalogOptions,
    ) -> Result<ProviderCatalog> {
        self.inner
            .call(
                "provider.fetchCatalog",
                json!({"provider": provider, "options": options}),
            )
            .await
    }

    pub async fn get_diagnostic(&self, provider: &str) -> Result<Option<Value>> {
        self.inner
            .call("provider.getDiagnostic", json!({"provider": provider}))
            .await
    }

    pub async fn list_importable_sessions(
        &self,
        provider: &str,
        options: Option<Value>,
    ) -> Result<Vec<Value>> {
        #[derive(serde::Deserialize)]
        struct R {
            sessions: Vec<Value>,
        }
        let r: R = self
            .inner
            .call(
                "provider.listImportableSessions",
                json!({"provider": provider, "options": options}),
            )
            .await?;
        Ok(r.sessions)
    }

    pub async fn archive_native_session(&self, persistence: &AgentPersistenceHandle) -> Result<()> {
        self.persistence_call("provider.archiveNativeSession", persistence)
            .await
    }

    pub async fn unarchive_native_session(
        &self,
        persistence: &AgentPersistenceHandle,
    ) -> Result<()> {
        self.persistence_call("provider.unarchiveNativeSession", persistence)
            .await
    }

    async fn persistence_call(
        &self,
        method: &str,
        persistence: &AgentPersistenceHandle,
    ) -> Result<()> {
        let provider = persistence.provider.clone();
        let _: Value = self
            .inner
            .call(
                method,
                json!({"provider": provider, "persistence": persistence}),
            )
            .await?;
        Ok(())
    }

    pub async fn create_session(
        &self,
        config: &AgentSessionConfig,
        launch_context: Option<Value>,
        options: Option<Value>,
    ) -> Result<Session> {
        let info: SessionInfo = self
            .inner
            .call(
                "provider.createSession",
                json!({
                    "provider": config.provider,
                    "config": config,
                    "launchContext": launch_context,
                    "options": options,
                }),
            )
            .await?;
        Ok(Session::new(Arc::clone(&self.inner), info))
    }

    pub async fn resume_session(
        &self,
        persistence: &AgentPersistenceHandle,
        overrides: Option<&AgentSessionConfigOverrides>,
    ) -> Result<Session> {
        self.resume_session_with(persistence, overrides, None, None)
            .await
    }

    pub async fn resume_session_with(
        &self,
        persistence: &AgentPersistenceHandle,
        overrides: Option<&AgentSessionConfigOverrides>,
        launch_context: Option<Value>,
        options: Option<Value>,
    ) -> Result<Session> {
        let info: SessionInfo = self
            .inner
            .call(
                "provider.resumeSession",
                json!({
                    "provider": persistence.provider,
                    "persistence": persistence,
                    "overrides": overrides,
                    "launchContext": launch_context,
                    "options": options,
                }),
            )
            .await?;
        Ok(Session::new(Arc::clone(&self.inner), info))
    }

    /// `provider.getCapabilities`.
    pub async fn get_capabilities(&self, provider: &str) -> Result<crate::wire::JsonObject> {
        #[derive(serde::Deserialize)]
        struct R {
            capabilities: crate::wire::JsonObject,
        }
        let r: R = self
            .inner
            .call("provider.getCapabilities", json!({"provider": provider}))
            .await?;
        Ok(r.capabilities)
    }

    /// `provider.listCommands` for a session config.
    pub async fn list_commands(&self, provider: &str, config: &Value) -> Result<Vec<Value>> {
        #[derive(serde::Deserialize)]
        struct R {
            commands: Vec<Value>,
        }
        let r: R = self
            .inner
            .call(
                "provider.listCommands",
                json!({"provider": provider, "config": config}),
            )
            .await?;
        Ok(r.commands)
    }

    /// `provider.listFeatures` for a session config.
    pub async fn list_features(&self, provider: &str, config: &Value) -> Result<Vec<Value>> {
        #[derive(serde::Deserialize)]
        struct R {
            features: Vec<Value>,
        }
        let r: R = self
            .inner
            .call(
                "provider.listFeatures",
                json!({"provider": provider, "config": config}),
            )
            .await?;
        Ok(r.features)
    }

    /// `provider.importSession`. The returned [`ImportedHostSession`] keeps the open-ended
    /// parts as JSON.
    pub async fn import_session(
        &self,
        provider: &str,
        input: &Value,
        context: &Value,
    ) -> Result<ImportedHostSession> {
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct R {
            session: SessionInfo,
            config: Value,
            persistence: AgentPersistenceHandle,
            #[serde(default)]
            timeline: Vec<Value>,
            #[serde(default)]
            provider_subagent_events: Vec<Value>,
        }
        let r: R = self
            .inner
            .call(
                "provider.importSession",
                json!({"provider": provider, "input": input, "context": context}),
            )
            .await?;
        Ok(ImportedHostSession {
            session: Session::new(Arc::clone(&self.inner), r.session),
            config: r.config,
            persistence: r.persistence,
            timeline: r.timeline,
            provider_subagent_events: r.provider_subagent_events,
        })
    }

    /// Close sessions, shut providers down, and wait for the process (killing it after
    /// `shutdown_grace`).
    pub async fn shutdown(&self) -> Result<()> {
        let result: Result<Value> = self.inner.call("host.shutdown", json!({})).await;
        // A host that already exited is a successful shutdown.
        if let Err(error) = result {
            if !matches!(error, Error::HostExited { .. }) {
                return Err(error);
            }
        }
        self.inner.stdin.lock().await.take();
        let child = self.inner.child.lock().await.take();
        if let Some(mut child) = child {
            let grace = self.inner.config.shutdown_grace;
            if tokio::time::timeout(grace, child.wait()).await.is_err() {
                let _ = child.kill().await;
            }
        }
        Ok(())
    }
}

/// Result of [`ProviderHostClient::import_session`].
pub struct ImportedHostSession {
    pub session: Session,
    pub config: Value,
    pub persistence: AgentPersistenceHandle,
    /// `{item, timestamp?}` entries.
    pub timeline: Vec<Value>,
    pub provider_subagent_events: Vec<Value>,
}

/// `Partial<AgentSessionConfig>` for resume overrides: any JSON object of config fields.
pub type AgentSessionConfigOverrides = serde_json::Map<String, Value>;

async fn read_loop(
    stdout: ChildStdout,
    inner: Weak<Inner>,
    stderr_task: tokio::task::JoinHandle<()>,
) {
    let mut lines = BufReader::new(stdout).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                let Some(inner) = inner.upgrade() else { return };
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<Value>(&line) {
                    Ok(value) => dispatch(&inner, value),
                    Err(error) => tracing::warn!(%error, "non-JSON line from provider host"),
                }
            }
            Ok(None) => break,
            Err(error) => {
                tracing::warn!(%error, "provider host stdout read failed");
                break;
            }
        }
    }
    let Some(inner) = inner.upgrade() else { return };
    let reason = {
        let mut child = inner.child.lock().await;
        match child.as_mut() {
            Some(child) => {
                match tokio::time::timeout(Duration::from_millis(500), child.wait()).await {
                    Ok(Ok(status)) => format!("{status}"),
                    _ => "stdout closed".to_owned(),
                }
            }
            None => "stdout closed".to_owned(),
        }
    };
    let _ = tokio::time::timeout(Duration::from_millis(500), stderr_task).await;
    inner.fail_all(&reason);
}

fn dispatch(inner: &Arc<Inner>, value: Value) {
    let has_id = value.get("id").is_some_and(|id| !id.is_null());
    let method = value
        .get("method")
        .and_then(Value::as_str)
        .map(str::to_owned);
    match (has_id, method) {
        (true, None) => dispatch_response(inner, value),
        (true, Some(method)) => dispatch_host_request(inner, &method, value),
        (false, Some(method)) => dispatch_notification(inner, &method, value),
        _ => {
            if value.get("error").is_some() {
                tracing::warn!(frame = %value, "host reported an error without a request id");
            } else {
                tracing::warn!(frame = %value, "unrecognized frame from provider host");
            }
        }
    }
}

fn dispatch_response(inner: &Arc<Inner>, value: Value) {
    let Some(id) = value.get("id").and_then(Value::as_u64) else {
        tracing::warn!(frame = %value, "response with non-numeric id");
        return;
    };
    let entry = inner.pending.lock().expect("pending lock").map.remove(&id);
    let Some(entry) = entry else {
        tracing::debug!(id, "response for unknown or abandoned request");
        return;
    };
    let outcome = if let Some(error) = value.get("error") {
        Err(Error::Rpc {
            code: error
                .get("code")
                .and_then(Value::as_i64)
                .unwrap_or(crate::code::INTERNAL),
            message: error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown error")
                .to_owned(),
            data: error.get("data").cloned(),
        })
    } else {
        Ok(value.get("result").cloned().unwrap_or(Value::Null))
    };
    match (&entry.kind, outcome.as_ref().ok()) {
        (EntryKind::RegistersSession, Some(result)) => {
            // `importSession` nests the session info under `session`.
            let info = result.get("session").unwrap_or(result);
            if let Some(handle) = info.get("handle").and_then(Value::as_str) {
                let provider = info
                    .get("provider")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                // Registered before the awaiting task wakes, so no later event can miss it.
                inner
                    .channels
                    .lock()
                    .expect("channels lock")
                    .entry(handle.to_owned())
                    .or_insert_with(|| Channel::new(handle.to_owned(), provider.to_owned()));
            }
        }
        (EntryKind::StartTurn(handle), Some(result)) => {
            if let Some(turn_id) = result.get("turnId").and_then(Value::as_str) {
                // Counts as active from the response on, so a host that dies before the
                // `turn_started` event still ends the turn.
                if let Some(channel) = inner
                    .channels
                    .lock()
                    .expect("channels lock")
                    .get_mut(handle)
                {
                    channel.mark_active(Some(turn_id));
                }
            }
        }
        _ => {}
    }
    let _ = entry.tx.send(outcome);
}

fn dispatch_notification(inner: &Arc<Inner>, method: &str, value: Value) {
    let params = value.get("params").cloned().unwrap_or(Value::Null);
    match method {
        "session.event" => {
            let (Some(handle), Some(seq), Some(raw)) = (
                params.get("handle").and_then(Value::as_str),
                params.get("seq").and_then(Value::as_u64),
                params.get("event"),
            ) else {
                tracing::warn!(frame = %value, "malformed session.event");
                return;
            };
            let raw_type = raw.get("type").and_then(Value::as_str).unwrap_or("");
            let raw_turn_id = raw.get("turnId").and_then(Value::as_str);
            let event = match serde_json::from_value::<AgentStreamEvent>(raw.clone()) {
                Ok(event) => event,
                Err(error) => {
                    tracing::warn!(%handle, %error, event = %raw, "undecodable host event; typed view is Unknown");
                    AgentStreamEvent::Unknown
                }
            };
            let session_event = SessionEvent {
                handle: handle.to_owned(),
                seq,
                event: event.clone(),
                raw: raw.clone(),
                synthetic: false,
            };
            {
                // Events for sessions without a live channel (released or dropped) are
                // discarded.
                let mut channels = inner.channels.lock().expect("channels lock");
                let Some(channel) = channels.get_mut(handle) else {
                    return;
                };
                if channel.dead {
                    return;
                }
                let expected = channel.last_seq + 1;
                if seq != expected {
                    tracing::error!(%handle, expected, got = seq, "provider host event sequence gap");
                    channel.fail(
                        &format!(
                            "Provider host event sequence gap: expected {expected}, got {seq}"
                        ),
                        Some("provider_host_seq_gap"),
                    );
                    return;
                }
                channel.last_seq = seq;
                // Tracking keys off the raw JSON so a typed decode failure cannot leave a
                // finished turn marked active.
                match raw_type {
                    "turn_started" => channel.mark_active(raw_turn_id),
                    "turn_completed" | "turn_failed" | "turn_canceled" => {
                        channel.mark_finished(raw_turn_id)
                    }
                    _ => {}
                }
                if let Some(tx) = &channel.tx {
                    let _ = tx.send(session_event);
                }
            }
            if raw_type == "permission_requested" {
                if let Some(handler) = inner.handler.clone() {
                    match event {
                        AgentStreamEvent::PermissionRequested {
                            request, turn_id, ..
                        } => spawn_permission_task(
                            Arc::clone(inner),
                            handler,
                            handle.to_owned(),
                            turn_id,
                            *request,
                        ),
                        _ => deny_undecodable_permission(inner, handle, raw),
                    }
                }
            }
        }
        "host.log" => log_host_message(&params),
        "session.closed" => {
            if let Some(handle) = params.get("handle").and_then(Value::as_str) {
                inner.forget_session(handle);
            }
        }
        other => tracing::debug!(method = other, "ignoring host notification"),
    }
}

/// A `permission_requested` event the typed model rejects still needs an answer, or the
/// provider waits on the user forever. Deny it so the provider can proceed.
fn deny_undecodable_permission(inner: &Arc<Inner>, handle: &str, raw: &Value) {
    let Some(request_id) = raw
        .get("request")
        .and_then(|request| request.get("id"))
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        tracing::error!(%handle, event = %raw, "permission_requested without request id");
        return;
    };
    let inner = Arc::clone(inner);
    let handle = handle.to_owned();
    tokio::spawn(async move {
        let params = json!({
            "handle": handle,
            "requestId": request_id,
            "response": serialize_response(&crate::wire::PermissionResponse::deny(
                "Permission request could not be decoded",
            )),
        });
        if let Err(error) = inner
            .call::<Value>("session.respondToPermission", params)
            .await
        {
            tracing::warn!(%error, %handle, %request_id, "failed to deny undecodable permission");
        }
    });
}

fn spawn_permission_task(
    inner: Arc<Inner>,
    handler: Arc<dyn PermissionHandler>,
    handle: String,
    turn_id: Option<String>,
    request: crate::wire::PermissionRequest,
) {
    tokio::spawn(async move {
        let request_id = request.id.clone();
        let ctx = PermissionContext {
            handle: handle.clone(),
            turn_id,
        };
        let response: PermissionResponse = handler.decide(ctx, request).await;
        let params = json!({
            "handle": handle,
            "requestId": request_id,
            "response": serialize_response(&response),
        });
        if let Err(error) = inner
            .call::<Value>("session.respondToPermission", params)
            .await
        {
            tracing::warn!(%error, %handle, %request_id, "failed to deliver permission response");
        }
    });
}

fn serialize_response<T: Serialize>(value: &T) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

/// Host -> client request (`id` and `method`). Only `tool.call` exists (see RPC.md).
fn dispatch_host_request(inner: &Arc<Inner>, method: &str, value: Value) {
    let id = value.get("id").cloned().unwrap_or(Value::Null);
    let params = value.get("params").cloned().unwrap_or(Value::Null);
    let inner = Arc::clone(inner);
    if method != "tool.call" {
        let message = format!("unknown client method {method}");
        tokio::spawn(async move {
            inner
                .respond(id, Err((crate::code::METHOD_NOT_FOUND, message)))
                .await;
        });
        return;
    }
    tokio::spawn(async move {
        let Some(handler) = inner.tool_handler.clone() else {
            let message = "no tool.call handler registered".to_owned();
            inner
                .respond(id, Err((crate::code::METHOD_NOT_FOUND, message)))
                .await;
            return;
        };
        let request: ToolCallRequest = match serde_json::from_value(params) {
            Ok(request) => request,
            Err(error) => {
                inner
                    .respond(id, Err((crate::code::INVALID_PARAMS, error.to_string())))
                    .await;
                return;
            }
        };
        let outcome = match handler.call(request).await {
            Ok(result) => serde_json::to_value(result)
                .map_err(|error| (crate::code::INTERNAL, error.to_string())),
            Err(paseo_provider_api::ProviderError::Canceled) => {
                Err((crate::code::CANCELED, "request canceled".to_owned()))
            }
            Err(error) => Err((crate::code::PROVIDER, error.to_string())),
        };
        inner.respond(id, outcome).await;
    });
}

/// `host.log`: `{level, message, data?}` forwarded to `tracing`.
fn log_host_message(params: &Value) {
    let message = params
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let data = params.get("data").map(Value::to_string).unwrap_or_default();
    match params.get("level").and_then(Value::as_str) {
        Some("error") => tracing::error!(target: "paseo_provider_host::host", %data, "{message}"),
        Some("warn") => tracing::warn!(target: "paseo_provider_host::host", %data, "{message}"),
        Some("info") => tracing::info!(target: "paseo_provider_host::host", %data, "{message}"),
        _ => tracing::debug!(target: "paseo_provider_host::host", %data, "{message}"),
    }
}
