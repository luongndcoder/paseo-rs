//! Soniox real-time STT over WebSocket (port of `providers/soniox/stt.ts`), including the
//! automatic fallback to a local provider when Soniox is unconfigured or drops mid-stream.

use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::error::{Result, SpeechError};
use crate::provider::{
    stt_event_channel, CommittedEvent, SttEvent, SttEventReceiver, SttEventSender, SttProvider,
    SttSession, SttSessionParams, TranscriptEvent,
};

use super::config::{
    default_soniox_language_hints, SonioxSpeechProviderConfig, DEFAULT_SONIOX_STT_MODEL,
    DEFAULT_SONIOX_WS_ENDPOINT,
};
use super::ws::{
    connect_with_timeout, SonioxConnector, TungsteniteConnector, WsIncoming, WsOutgoing,
};

pub const SONIOX_SAMPLE_RATE: u32 = 16000;
const SONIOX_CONNECT_TIMEOUT_MS: u64 = 6000;
const SONIOX_FINALIZE_WAIT_MS: u64 = 500;
/// ~160s at 16kHz 16-bit mono.
const MAX_FALLBACK_PCM_BUFFER_BYTES: usize = 5 * 1024 * 1024;

#[derive(Debug, Deserialize)]
struct SonioxToken {
    text: Option<Value>,
    is_final: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct SonioxServerMessage {
    tokens: Option<Value>,
    finished: Option<Value>,
    error_code: Option<Value>,
    error_message: Option<Value>,
}

fn normalize_language_code(language: Option<&str>) -> Option<String> {
    let trimmed = language?.trim().to_lowercase();
    if trimmed.is_empty() {
        return None;
    }
    let primary = trimmed.split(['-', '_']).next().unwrap_or("");
    (!primary.is_empty()).then(|| primary.to_string())
}

/// Session language first, then configured hints, deduplicated (`resolveSonioxLanguageHints`).
pub fn resolve_soniox_language_hints(
    session_language: Option<&str>,
    configured_hints: Option<&[String]>,
) -> Vec<String> {
    let mut hints: Vec<String> = Vec::new();
    if let Some(primary) = normalize_language_code(session_language) {
        hints.push(primary);
    }
    let defaults = default_soniox_language_hints();
    for hint in configured_hints.unwrap_or(&defaults) {
        if let Some(normalized) = normalize_language_code(Some(hint)) {
            if !hints.contains(&normalized) {
                hints.push(normalized);
            }
        }
    }
    if hints.is_empty() {
        return defaults;
    }
    hints
}

fn is_control_token_text(text: &str) -> bool {
    let t = text.trim();
    t == "<end>" || t == "<fin>"
}

fn js_truthy(value: &Option<Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

fn display_value(value: &Option<Value>, fallback: &str) -> String {
    match value {
        Some(Value::String(s)) if !s.is_empty() => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => fallback.to_string(),
    }
}

pub type SonioxConfigGetter = Arc<dyn Fn() -> Option<SonioxSpeechProviderConfig> + Send + Sync>;

#[derive(Clone)]
pub struct SonioxSttOptions {
    pub get_config: SonioxConfigGetter,
    pub fallback_provider: Option<Arc<dyn SttProvider>>,
    pub connector: Arc<dyn SonioxConnector>,
    pub connect_timeout: Duration,
    pub finalize_wait: Duration,
}

impl SonioxSttOptions {
    pub fn new(
        get_config: impl Fn() -> Option<SonioxSpeechProviderConfig> + Send + Sync + 'static,
    ) -> Self {
        Self {
            get_config: Arc::new(get_config),
            fallback_provider: None,
            connector: Arc::new(TungsteniteConnector),
            connect_timeout: Duration::from_millis(SONIOX_CONNECT_TIMEOUT_MS),
            finalize_wait: Duration::from_millis(SONIOX_FINALIZE_WAIT_MS),
        }
    }
    pub fn with_fallback(mut self, provider: Option<Arc<dyn SttProvider>>) -> Self {
        self.fallback_provider = provider;
        self
    }
    pub fn with_connector(mut self, connector: Arc<dyn SonioxConnector>) -> Self {
        self.connector = connector;
        self
    }
}

pub struct SonioxStt {
    options: SonioxSttOptions,
}

impl SonioxStt {
    pub fn new(options: SonioxSttOptions) -> Self {
        Self { options }
    }
}

impl SttProvider for SonioxStt {
    fn id(&self) -> &str {
        "soniox"
    }

    fn create_session(
        &self,
        params: &SttSessionParams,
    ) -> Result<(Arc<dyn SttSession>, SttEventReceiver)> {
        let (tx, rx) = stt_event_channel();
        let inner = Arc::new_cyclic(|weak| Inner {
            options: self.options.clone(),
            language: params.language.clone(),
            prompt: params.prompt.clone(),
            events: tx,
            state: Mutex::new(State::new()),
            connect_lock: tokio::sync::Mutex::new(()),
            fallback_lock: tokio::sync::Mutex::new(()),
            weak_self: weak.clone(),
        });
        Ok((Arc::new(SonioxSession { inner }), rx))
    }
}

struct WsHandle {
    id: u64,
    tx: mpsc::UnboundedSender<WsOutgoing>,
}

struct State {
    closed: bool,
    ws: Option<WsHandle>,
    ws_ready: bool,
    background_connecting: bool,
    next_ws_id: u64,
    using_fallback: bool,
    fallback: Option<Arc<dyn SttSession>>,
    segment_sequence: u64,
    current_segment_id: String,
    previous_segment_id: Option<String>,
    finalized_text: String,
    non_final_text: String,
    last_partial: String,
    pending_send_chunks: Vec<Vec<u8>>,
    current_segment_pcm_chunks: Vec<Vec<u8>>,
    current_segment_pcm_bytes: usize,
    commit_pending: bool,
    commit_timer: Option<JoinHandle<()>>,
}

impl State {
    fn new() -> Self {
        Self {
            closed: false,
            ws: None,
            ws_ready: false,
            background_connecting: false,
            next_ws_id: 1,
            using_fallback: false,
            fallback: None,
            segment_sequence: 1,
            current_segment_id: "soniox-seg-1".to_string(),
            previous_segment_id: None,
            finalized_text: String::new(),
            non_final_text: String::new(),
            last_partial: String::new(),
            pending_send_chunks: Vec::new(),
            current_segment_pcm_chunks: Vec::new(),
            current_segment_pcm_bytes: 0,
            commit_pending: false,
            commit_timer: None,
        }
    }

    fn ws_open(&self) -> bool {
        self.ws_ready && self.ws.as_ref().is_some_and(|w| !w.tx.is_closed())
    }

    fn combined_text(&self) -> String {
        format!("{}{}", self.finalized_text, self.non_final_text)
            .trim()
            .to_string()
    }

    fn clear_commit_timer(&mut self) {
        if let Some(timer) = self.commit_timer.take() {
            timer.abort();
        }
    }

    fn reset_segment_state(&mut self) {
        self.finalized_text.clear();
        self.non_final_text.clear();
        self.last_partial.clear();
        self.pending_send_chunks.clear();
        self.current_segment_pcm_chunks.clear();
        self.current_segment_pcm_bytes = 0;
    }

    fn advance_segment(&mut self) {
        self.previous_segment_id = Some(self.current_segment_id.clone());
        self.segment_sequence += 1;
        self.current_segment_id = format!("soniox-seg-{}", self.segment_sequence);
        self.reset_segment_state();
    }

    fn teardown_ws(&mut self) {
        self.ws_ready = false;
        if let Some(ws) = self.ws.take() {
            let _ = ws.tx.send(WsOutgoing::Close(1000));
        }
    }

    fn flush_pending_to_socket(&mut self) -> std::result::Result<(), String> {
        if !self.ws_open() {
            return Ok(());
        }
        let chunks = std::mem::take(&mut self.pending_send_chunks);
        let tx = self.ws.as_ref().expect("open").tx.clone();
        for chunk in chunks {
            tx.send(WsOutgoing::Binary(chunk))
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}

struct Inner {
    options: SonioxSttOptions,
    language: Option<String>,
    prompt: Option<String>,
    events: SttEventSender,
    state: Mutex<State>,
    connect_lock: tokio::sync::Mutex<()>,
    fallback_lock: tokio::sync::Mutex<()>,
    weak_self: Weak<Inner>,
}

pub struct SonioxSession {
    inner: Arc<Inner>,
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn emit(&self, event: SttEvent) {
        let _ = self.events.send(event);
    }

    fn arc(&self) -> Arc<Inner> {
        self.weak_self
            .upgrade()
            .expect("session alive while in use")
    }

    fn spawn_failure(&self, message: String) {
        let this = self.arc();
        tokio::spawn(async move { this.handle_runtime_failure(message).await });
    }

    fn complete_commit(&self, st: &mut State) {
        st.clear_commit_timer();
        if !st.commit_pending
            && st.current_segment_pcm_bytes == 0
            && st.finalized_text.is_empty()
            && st.non_final_text.is_empty()
        {
            let segment_id = st.current_segment_id.clone();
            self.emit(SttEvent::Committed(CommittedEvent {
                segment_id: segment_id.clone(),
                previous_segment_id: st.previous_segment_id.clone(),
            }));
            self.emit(SttEvent::Transcript(TranscriptEvent::final_(
                segment_id, "",
            )));
            st.advance_segment();
            return;
        }
        st.commit_pending = false;
        let segment_id = st.current_segment_id.clone();
        let final_transcript = st.combined_text();
        self.emit(SttEvent::Committed(CommittedEvent {
            segment_id: segment_id.clone(),
            previous_segment_id: st.previous_segment_id.clone(),
        }));
        self.emit(SttEvent::Transcript(TranscriptEvent::final_(
            segment_id,
            final_transcript,
        )));
        st.advance_segment();
        // Soniox closes the stream after empty-frame finalization; close our side so the next
        // utterance in this session opens a fresh stream.
        st.teardown_ws();
    }

    async fn connect(&self) -> Result<()> {
        {
            let st = self.lock();
            if st.closed {
                return Err(SpeechError::msg("Soniox transcription session is closed"));
            }
            if st.using_fallback && st.fallback.is_some() {
                return Ok(());
            }
        }
        let api_key = (self.options.get_config)()
            .and_then(|c| c.api_key)
            .map(|k| k.trim().to_string())
            .unwrap_or_default();
        if api_key.is_empty() {
            if self
                .activate_fallback("Soniox API key is not configured".to_string())
                .await
            {
                return Ok(());
            }
            return Err(SpeechError::msg(
                "Soniox API key is not configured and local STT fallback is unavailable",
            ));
        }
        if let Err(err) = self.ensure_ws_connected().await {
            if self.activate_fallback(err.to_string()).await {
                return Ok(());
            }
            return Err(err);
        }
        Ok(())
    }

    async fn ensure_ws_connected(&self) -> Result<()> {
        if self.lock().ws_open() {
            return Ok(());
        }
        // Concurrent callers queue here; the second one finds the socket already open.
        let _guard = self.connect_lock.lock().await;
        if self.lock().ws_open() {
            return Ok(());
        }
        let config = (self.options.get_config)();
        let api_key = config
            .as_ref()
            .and_then(|c| c.api_key.as_deref())
            .map(str::trim)
            .unwrap_or("");
        if api_key.is_empty() {
            return Err(SpeechError::msg("Soniox API key is not configured"));
        }
        let endpoint = config
            .as_ref()
            .and_then(|c| c.endpoint.as_deref())
            .map(str::trim)
            .filter(|e| !e.is_empty())
            .unwrap_or(DEFAULT_SONIOX_WS_ENDPOINT)
            .to_string();
        let model = config
            .as_ref()
            .and_then(|c| c.model.as_deref())
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .unwrap_or(DEFAULT_SONIOX_STT_MODEL)
            .to_string();
        let hints = resolve_soniox_language_hints(
            self.language.as_deref(),
            config.as_ref().and_then(|c| c.language_hints.as_deref()),
        );
        let init = json!({
            "api_key": api_key,
            "model": model,
            "audio_format": "pcm_s16le",
            "sample_rate": SONIOX_SAMPLE_RATE,
            "num_channels": 1,
            "language_hints": hints,
            "enable_language_identification": true,
        });

        let socket = connect_with_timeout(
            self.options.connector.as_ref(),
            &endpoint,
            self.options.connect_timeout,
        )
        .await?;

        let ws_id = {
            let mut st = self.lock();
            if st.closed {
                let _ = socket.outgoing.send(WsOutgoing::Close(1000));
                return Ok(());
            }
            let id = st.next_ws_id;
            st.next_ws_id += 1;
            socket
                .outgoing
                .send(WsOutgoing::Text(init.to_string()))
                .map_err(|e| SpeechError::msg(e.to_string()))?;
            st.ws = Some(WsHandle {
                id,
                tx: socket.outgoing.clone(),
            });
            st.ws_ready = true;
            if let Err(err) = st.flush_pending_to_socket() {
                return Err(SpeechError::msg(err));
            }
            id
        };

        let this = self.arc();
        let mut incoming = socket.incoming;
        tokio::spawn(async move {
            while let Some(frame) = incoming.recv().await {
                if !this.on_incoming(ws_id, frame) {
                    break;
                }
            }
        });
        Ok(())
    }

    /// Returns false when this socket is stale or finished and its reader should stop.
    fn on_incoming(&self, ws_id: u64, frame: WsIncoming) -> bool {
        let mut st = self.lock();
        if st.closed || st.ws.as_ref().map(|w| w.id) != Some(ws_id) {
            return false;
        }
        match frame {
            WsIncoming::Text(text) => {
                if !st.using_fallback {
                    self.handle_server_message(&mut st, &text);
                }
                true
            }
            WsIncoming::Binary(bytes) => {
                if !st.using_fallback {
                    let text = String::from_utf8_lossy(&bytes).into_owned();
                    self.handle_server_message(&mut st, &text);
                }
                true
            }
            WsIncoming::Error(message) => {
                drop(st);
                self.spawn_failure(message);
                true
            }
            WsIncoming::Closed { code, reason } => {
                st.ws = None;
                st.ws_ready = false;
                if st.commit_pending {
                    // Text already received completes the commit cleanly; nothing and an
                    // abnormal close means Soniox failed, so try the fallback.
                    if !st.combined_text().is_empty() || code == 1000 {
                        self.complete_commit(&mut st);
                    } else {
                        drop(st);
                        self.spawn_failure(format!(
                            "Soniox WebSocket closed unexpectedly during commit (code={code}){}",
                            if reason.is_empty() {
                                String::new()
                            } else {
                                format!(", reason={reason}")
                            }
                        ));
                    }
                }
                false
            }
        }
    }

    fn handle_server_message(&self, st: &mut State, text: &str) {
        let parsed: SonioxServerMessage = match serde_json::from_str(text) {
            Ok(p) => p,
            Err(err) => {
                tracing::warn!(error = %err, %text, "Failed to parse Soniox WebSocket message");
                return;
            }
        };

        if js_truthy(&parsed.error_code) || js_truthy(&parsed.error_message) {
            let message = format!(
                "Soniox STT error ({}): {}",
                display_value(&parsed.error_code, "unknown"),
                display_value(&parsed.error_message, "Unknown error")
            );
            let this = self.arc();
            tokio::spawn(async move { this.handle_runtime_failure(message).await });
            return;
        }

        if let Some(Value::Array(tokens)) = parsed.tokens {
            let mut next_non_final = String::new();
            for raw in tokens {
                let Ok(token) = serde_json::from_value::<SonioxToken>(raw) else {
                    continue;
                };
                let text = match &token.text {
                    Some(Value::String(s)) => s.as_str(),
                    _ => "",
                };
                if text.is_empty() || is_control_token_text(text) {
                    continue;
                }
                if js_truthy(&token.is_final) {
                    st.finalized_text.push_str(text);
                } else {
                    next_non_final.push_str(text);
                }
            }
            st.non_final_text = next_non_final;
            let combined = st.combined_text();
            if !combined.is_empty() && combined != st.last_partial {
                st.last_partial = combined.clone();
                self.emit(SttEvent::Transcript(TranscriptEvent::partial(
                    st.current_segment_id.clone(),
                    combined,
                )));
            }
        }

        if st.commit_pending && matches!(parsed.finished, Some(Value::Bool(true))) {
            self.complete_commit(st);
        }
    }

    async fn handle_runtime_failure(&self, message: String) {
        {
            let st = self.lock();
            if st.closed || st.using_fallback {
                return;
            }
        }
        if self.activate_fallback(message.clone()).await {
            return;
        }
        let mut st = self.lock();
        // Keep whatever text we already have rather than losing it.
        if st.commit_pending && !st.combined_text().is_empty() {
            tracing::warn!(error = %message, "Soniox error during commit with no fallback; completing with partial transcript");
            self.complete_commit(&mut st);
            return;
        }
        drop(st);
        self.emit(SttEvent::Error(message));
    }

    async fn activate_fallback(&self, reason: String) -> bool {
        {
            let st = self.lock();
            if st.closed {
                return false;
            }
            if st.using_fallback && st.fallback.is_some() {
                return true;
            }
        }
        let Some(provider) = self.options.fallback_provider.clone() else {
            return false;
        };
        // One activation at a time; later callers see `using_fallback` and return.
        let _guard = self.fallback_lock.lock().await;
        {
            let st = self.lock();
            if st.closed {
                return false;
            }
            if st.using_fallback && st.fallback.is_some() {
                return true;
            }
        }

        tracing::warn!(error = %reason, "Activating local Sherpa STT fallback for Soniox session");
        {
            let mut st = self.lock();
            st.clear_commit_timer();
            st.teardown_ws();
        }

        let (session, mut rx) = match provider.create_session(&SttSessionParams {
            language: self.language.clone(),
            prompt: self.prompt.clone(),
        }) {
            Ok(created) => created,
            Err(err) => {
                tracing::error!(error = %err, "Failed to activate local Sherpa STT fallback session");
                return false;
            }
        };

        let this = self.arc();
        tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                if this.lock().closed {
                    break;
                }
                this.emit(event);
            }
        });

        if let Err(err) = session.connect().await {
            tracing::error!(error = %err, "Failed to activate local Sherpa STT fallback session");
            return false;
        }

        let (replay, commit_pending) = {
            let mut st = self.lock();
            if st.closed {
                drop(st);
                let _ = session.close();
                return false;
            }
            st.fallback = Some(session.clone());
            st.using_fallback = true;
            let replay = std::mem::take(&mut st.current_segment_pcm_chunks);
            st.current_segment_pcm_bytes = 0;
            st.pending_send_chunks.clear();
            let commit_pending = st.commit_pending;
            if commit_pending {
                st.commit_pending = false;
            }
            (replay, commit_pending)
        };
        for chunk in replay {
            session.append_pcm16(&chunk);
        }
        if commit_pending {
            if let Err(err) = session.commit() {
                self.emit(SttEvent::Error(err.to_string()));
            }
        }
        true
    }

    fn append(&self, pcm16: &[u8]) {
        let fallback = {
            let mut st = self.lock();
            if st.closed || pcm16.is_empty() {
                return;
            }
            if st.using_fallback {
                st.fallback.clone()
            } else {
                // Keep the segment's PCM so a mid-stream Soniox failure can replay it locally.
                if st.current_segment_pcm_bytes + pcm16.len() <= MAX_FALLBACK_PCM_BUFFER_BYTES {
                    st.current_segment_pcm_chunks.push(pcm16.to_vec());
                    st.current_segment_pcm_bytes += pcm16.len();
                }
                if st.ws_open() {
                    let tx = st.ws.as_ref().expect("open").tx.clone();
                    if let Err(err) = tx.send(WsOutgoing::Binary(pcm16.to_vec())) {
                        drop(st);
                        self.spawn_failure(err.to_string());
                    }
                    return;
                }
                st.pending_send_chunks.push(pcm16.to_vec());
                if !st.background_connecting {
                    st.background_connecting = true;
                    let this = self.arc();
                    tokio::spawn(async move {
                        let result = this.ensure_ws_connected().await;
                        this.lock().background_connecting = false;
                        if let Err(err) = result {
                            this.handle_runtime_failure(err.to_string()).await;
                        }
                    });
                }
                return;
            }
        };
        if let Some(session) = fallback {
            session.append_pcm16(pcm16);
        }
    }

    fn commit(&self) -> Result<()> {
        let mut st = self.lock();
        if st.closed {
            return Ok(());
        }
        if st.using_fallback {
            let fallback = st.fallback.clone();
            drop(st);
            if let Some(session) = fallback {
                return session.commit();
            }
            return Ok(());
        }

        if st.current_segment_pcm_bytes == 0
            && st.finalized_text.is_empty()
            && st.non_final_text.is_empty()
        {
            self.complete_commit(&mut st);
            return Ok(());
        }

        st.commit_pending = true;
        if let Err(err) = st.flush_pending_to_socket() {
            drop(st);
            self.spawn_failure(err);
            return Ok(());
        }
        if st.ws_open() {
            // An empty binary frame tells Soniox the utterance's audio ended and flushes the
            // remaining tokens as final.
            let tx = st.ws.as_ref().expect("open").tx.clone();
            if let Err(err) = tx.send(WsOutgoing::Binary(Vec::new())) {
                drop(st);
                self.spawn_failure(err.to_string());
                return Ok(());
            }
        }

        st.clear_commit_timer();
        let this = self.arc();
        let wait = self.options.finalize_wait;
        let deadline = tokio::time::Instant::now() + wait;
        st.commit_timer = Some(tokio::spawn(async move {
            tokio::time::sleep_until(deadline).await;
            let mut st = this.lock();
            st.commit_timer = None;
            if st.commit_pending && !st.using_fallback && !st.closed {
                this.complete_commit(&mut st);
            }
        }));
        Ok(())
    }

    fn clear(&self) {
        let mut st = self.lock();
        if st.closed {
            return;
        }
        if st.using_fallback {
            let fallback = st.fallback.clone();
            drop(st);
            if let Some(session) = fallback {
                session.clear();
            }
            return;
        }
        st.clear_commit_timer();
        st.commit_pending = false;
        st.reset_segment_state();
    }

    fn close(&self) {
        let fallback = {
            let mut st = self.lock();
            if st.closed {
                return;
            }
            st.closed = true;
            st.clear_commit_timer();
            st.commit_pending = false;
            st.pending_send_chunks.clear();
            st.current_segment_pcm_chunks.clear();
            st.current_segment_pcm_bytes = 0;
            st.teardown_ws();
            st.fallback.take()
        };
        if let Some(session) = fallback {
            let _ = session.close();
        }
    }
}

#[async_trait]
impl SttSession for SonioxSession {
    fn required_sample_rate(&self) -> u32 {
        SONIOX_SAMPLE_RATE
    }
    async fn connect(&self) -> Result<()> {
        self.inner.connect().await
    }
    fn append_pcm16(&self, pcm16le: &[u8]) {
        self.inner.append(pcm16le)
    }
    fn commit(&self) -> Result<()> {
        self.inner.commit()
    }
    fn clear(&self) {
        self.inner.clear()
    }
    fn close(&self) -> Result<()> {
        self.inner.close();
        Ok(())
    }
}
