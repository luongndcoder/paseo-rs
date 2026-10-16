use std::collections::HashMap;
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
}

struct PendingTable {
    /// Set when the host is gone; later requests fail with it.
    closed: Option<(String, String)>,
    map: HashMap<u64, PendingEntry>,
}

struct PendingEntry {
    tx: oneshot::Sender<Result<Value>>,
    /// The response carries a `handle` whose event channel must exist before any later event.
    registers_session: bool,
}

pub(crate) struct Channel {
    tx: mpsc::UnboundedSender<SessionEvent>,
    rx: Option<mpsc::UnboundedReceiver<SessionEvent>>,
}

pub(crate) struct Inner {
    stdin: tokio::sync::Mutex<Option<ChildStdin>>,
    child: tokio::sync::Mutex<Option<Child>>,
    next_id: AtomicU64,
    pending: Mutex<PendingTable>,
    channels: Mutex<HashMap<String, Channel>>,
    stderr_tail: Arc<Mutex<String>>,
    handler: Option<Arc<dyn PermissionHandler>>,
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
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        {
            let mut table = self.pending.lock().expect("pending lock");
            if table.closed.is_some() {
                drop(table);
                return Err(self.exited_error());
            }
            table.map.insert(
                id,
                PendingEntry {
                    tx,
                    registers_session: matches!(
                        method,
                        "provider.createSession"
                            | "provider.resumeSession"
                            | "provider.importSession"
                    ),
                },
            );
        }
        let frame = json!({"id": id, "method": method, "params": params});
        if let Err(error) = self.write_line(&frame).await {
            self.pending.lock().expect("pending lock").map.remove(&id);
            return Err(error);
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
        // Dropping the senders ends every event stream.
        self.channels.lock().expect("channels lock").clear();
    }
}

fn new_channel() -> Channel {
    let (tx, rx) = mpsc::unbounded_channel();
    Channel { tx, rx: Some(rx) }
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
        let info: SessionInfo = self
            .inner
            .call(
                "provider.resumeSession",
                json!({
                    "provider": persistence.provider,
                    "persistence": persistence,
                    "overrides": overrides,
                }),
            )
            .await?;
        Ok(Session::new(Arc::clone(&self.inner), info))
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
    if entry.registers_session {
        if let Some(handle) = outcome
            .as_ref()
            .ok()
            .and_then(|result| result.get("handle"))
            .and_then(Value::as_str)
        {
            // Registered before the awaiting task wakes, so no later event can miss it.
            inner
                .channels
                .lock()
                .expect("channels lock")
                .entry(handle.to_owned())
                .or_insert_with(new_channel);
        }
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
            let event = serde_json::from_value::<AgentStreamEvent>(raw.clone())
                .unwrap_or(AgentStreamEvent::Unknown);
            let session_event = SessionEvent {
                handle: handle.to_owned(),
                seq,
                event: event.clone(),
                raw: raw.clone(),
            };
            if let (
                Some(handler),
                AgentStreamEvent::PermissionRequested {
                    request, turn_id, ..
                },
            ) = (inner.handler.clone(), event)
            {
                spawn_permission_task(
                    Arc::clone(inner),
                    handler,
                    handle.to_owned(),
                    turn_id,
                    *request,
                );
            }
            // Events for sessions without a live channel (released or dropped) are discarded.
            let channels = inner.channels.lock().expect("channels lock");
            if let Some(channel) = channels.get(handle) {
                let _ = channel.tx.send(session_event);
            }
        }
        "session.closed" => {
            if let Some(handle) = params.get("handle").and_then(Value::as_str) {
                inner.forget_session(handle);
            }
        }
        other => tracing::debug!(method = other, "ignoring host notification"),
    }
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
