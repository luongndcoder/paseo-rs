//! Ports of `soniox/stt.test.ts` (fake WebSocket, fake local fallback) plus timing/close cases
//! and one loopback test over the real `tokio-tungstenite` transport.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use paseo_speech::provider::{
    stt_event_channel, CommittedEvent, SttEvent, SttEventReceiver, SttEventSender, SttProvider,
    SttSession, SttSessionParams, TranscriptEvent,
};
use paseo_speech::providers::soniox::config::{
    default_soniox_language_hints, SonioxSpeechProviderConfig, DEFAULT_SONIOX_STT_MODEL,
    DEFAULT_SONIOX_WS_ENDPOINT,
};
use paseo_speech::providers::soniox::ws::{SonioxConnector, SonioxSocket, WsIncoming, WsOutgoing};
use paseo_speech::providers::soniox::{resolve_soniox_language_hints, SonioxStt, SonioxSttOptions};
use paseo_speech::{Result, SpeechError};
use tokio::sync::mpsc;

async fn settle() {
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
}

// ---- fake socket ----------------------------------------------------------------

struct FakeSocket {
    url: String,
    sent: Mutex<Vec<WsOutgoing>>,
    outgoing_rx: Mutex<mpsc::UnboundedReceiver<WsOutgoing>>,
    incoming_tx: mpsc::UnboundedSender<WsIncoming>,
}

impl FakeSocket {
    fn sent(&self) -> Vec<WsOutgoing> {
        let mut sent = self.sent.lock().unwrap();
        let mut rx = self.outgoing_rx.lock().unwrap();
        while let Ok(frame) = rx.try_recv() {
            sent.push(frame);
        }
        sent.clone()
    }
    async fn server_json(&self, value: serde_json::Value) {
        self.incoming_tx
            .send(WsIncoming::Text(value.to_string()))
            .unwrap();
        settle().await;
    }
    async fn server_close(&self, code: u16) {
        self.incoming_tx
            .send(WsIncoming::Closed {
                code,
                reason: String::new(),
            })
            .unwrap();
        settle().await;
    }
}

#[derive(Default)]
struct FakeConnector {
    sockets: Mutex<Vec<Arc<FakeSocket>>>,
    fail_with: Mutex<Option<String>>,
    hang: AtomicBool,
}

impl FakeConnector {
    fn socket(&self, index: usize) -> Arc<FakeSocket> {
        self.sockets.lock().unwrap()[index].clone()
    }
    fn count(&self) -> usize {
        self.sockets.lock().unwrap().len()
    }
}

#[async_trait]
impl SonioxConnector for FakeConnector {
    async fn connect(&self, url: &str) -> Result<SonioxSocket> {
        if self.hang.load(Ordering::SeqCst) {
            std::future::pending::<()>().await;
        }
        if let Some(message) = self.fail_with.lock().unwrap().clone() {
            return Err(SpeechError::msg(message));
        }
        let (out_tx, out_rx) = mpsc::unbounded_channel();
        let (in_tx, in_rx) = mpsc::unbounded_channel();
        self.sockets.lock().unwrap().push(Arc::new(FakeSocket {
            url: url.to_string(),
            sent: Mutex::new(Vec::new()),
            outgoing_rx: Mutex::new(out_rx),
            incoming_tx: in_tx,
        }));
        Ok(SonioxSocket {
            outgoing: out_tx,
            incoming: in_rx,
        })
    }
}

// ---- fake local fallback --------------------------------------------------------

struct FakeFallbackSession {
    events: SttEventSender,
    appended: Mutex<Vec<Vec<u8>>>,
    connected: AtomicBool,
    closed: AtomicBool,
}

#[async_trait]
impl SttSession for FakeFallbackSession {
    fn required_sample_rate(&self) -> u32 {
        16000
    }
    async fn connect(&self) -> Result<()> {
        self.connected.store(true, Ordering::SeqCst);
        Ok(())
    }
    fn append_pcm16(&self, pcm16le: &[u8]) {
        self.appended.lock().unwrap().push(pcm16le.to_vec());
    }
    fn commit(&self) -> Result<()> {
        let total: usize = self.appended.lock().unwrap().iter().map(Vec::len).sum();
        let _ = self.events.send(SttEvent::Committed(CommittedEvent {
            segment_id: "local-seg-1".into(),
            previous_segment_id: None,
        }));
        let _ = self
            .events
            .send(SttEvent::Transcript(TranscriptEvent::final_(
                "local-seg-1",
                format!("fallback-sherpa-transcript({total}b)"),
            )));
        Ok(())
    }
    fn clear(&self) {
        self.appended.lock().unwrap().clear();
    }
    fn close(&self) -> Result<()> {
        self.closed.store(true, Ordering::SeqCst);
        Ok(())
    }
}

#[derive(Default)]
struct FakeFallbackProvider {
    sessions: Mutex<Vec<Arc<FakeFallbackSession>>>,
}

impl SttProvider for FakeFallbackProvider {
    fn id(&self) -> &str {
        "local"
    }
    fn create_session(
        &self,
        _params: &SttSessionParams,
    ) -> Result<(Arc<dyn SttSession>, SttEventReceiver)> {
        let (tx, rx) = stt_event_channel();
        let session = Arc::new(FakeFallbackSession {
            events: tx,
            appended: Mutex::new(Vec::new()),
            connected: AtomicBool::new(false),
            closed: AtomicBool::new(false),
        });
        self.sessions.lock().unwrap().push(session.clone());
        Ok((session as Arc<dyn SttSession>, rx))
    }
}

// ---- helpers --------------------------------------------------------------------

fn config(api_key: &str) -> SonioxSpeechProviderConfig {
    SonioxSpeechProviderConfig {
        api_key: Some(api_key.into()),
        endpoint: Some(DEFAULT_SONIOX_WS_ENDPOINT.into()),
        model: Some(DEFAULT_SONIOX_STT_MODEL.into()),
        language_hints: Some(vec!["vi".into(), "en".into()]),
    }
}

fn provider(
    cfg: SonioxSpeechProviderConfig,
    connector: Arc<FakeConnector>,
    fallback: Option<Arc<FakeFallbackProvider>>,
) -> SonioxStt {
    let mut options = SonioxSttOptions::new(move || Some(cfg.clone())).with_connector(connector);
    options.fallback_provider = fallback.map(|f| f as Arc<dyn SttProvider>);
    options.connect_timeout = Duration::from_millis(200);
    options.finalize_wait = Duration::from_millis(500);
    SonioxStt::new(options)
}

fn drain(rx: &mut SttEventReceiver) -> Vec<SttEvent> {
    let mut out = Vec::new();
    while let Ok(e) = rx.try_recv() {
        out.push(e);
    }
    out
}

fn finals(events: &[SttEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            SttEvent::Transcript(t) if t.is_final => Some(t.transcript.clone()),
            _ => None,
        })
        .collect()
}

fn session_params(language: &str) -> SttSessionParams {
    SttSessionParams {
        language: Some(language.into()),
        prompt: None,
    }
}

// ---- tests ----------------------------------------------------------------------

#[test]
fn language_hints_put_the_session_language_first_and_dedupe() {
    let hints = vec!["vi".to_string(), "en".to_string()];
    assert_eq!(
        resolve_soniox_language_hints(Some("en-US"), Some(&hints)),
        vec!["en", "vi"]
    );
    assert_eq!(
        resolve_soniox_language_hints(Some("vi-VN"), None),
        default_soniox_language_hints()
    );
    assert_eq!(
        resolve_soniox_language_hints(None, Some(&[])),
        default_soniox_language_hints()
    );
    assert_eq!(
        resolve_soniox_language_hints(Some("pt_BR"), Some(&["JA".to_string(), " ".to_string()])),
        vec!["pt", "ja"]
    );
}

#[tokio::test]
async fn streams_pcm16_emits_partial_and_final_and_flushes_with_an_empty_binary_frame() {
    let connector = Arc::new(FakeConnector::default());
    let stt = provider(config("soniox_secret_key"), connector.clone(), None);
    let (session, mut rx) = stt.create_session(&session_params("vi")).unwrap();
    assert_eq!(session.required_sample_rate(), 16000);

    session.connect().await.unwrap();
    assert_eq!(connector.count(), 1);
    let ws = connector.socket(0);
    assert_eq!(ws.url, DEFAULT_SONIOX_WS_ENDPOINT);

    let sent = ws.sent();
    assert_eq!(sent.len(), 1);
    let WsOutgoing::Text(init) = &sent[0] else {
        panic!("first frame must be the JSON handshake")
    };
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(init).unwrap(),
        serde_json::json!({
            "api_key": "soniox_secret_key",
            "model": "stt-rt-v5",
            "audio_format": "pcm_s16le",
            "sample_rate": 16000,
            "num_channels": 1,
            "language_hints": ["vi", "en"],
            "enable_language_identification": true
        })
    );

    session.append_pcm16(&[1, 2, 3, 4]);
    session.append_pcm16(&[5, 6, 7, 8]);
    let sent = ws.sent();
    assert_eq!(sent.len(), 3);
    assert_eq!(sent[1], WsOutgoing::Binary(vec![1, 2, 3, 4]));
    assert_eq!(sent[2], WsOutgoing::Binary(vec![5, 6, 7, 8]));

    ws.server_json(serde_json::json!({"tokens":[
        {"text":"Xin ","is_final":true},{"text":"chào ","is_final":false},{"text":"bạn","is_final":false}]}))
        .await;
    let events = drain(&mut rx);
    assert_eq!(
        events,
        vec![SttEvent::Transcript(TranscriptEvent::partial(
            "soniox-seg-1",
            "Xin chào bạn"
        ))]
    );

    session.commit().unwrap();
    let sent = ws.sent();
    assert_eq!(sent.len(), 4);
    assert_eq!(sent[3], WsOutgoing::Binary(Vec::new()));

    ws.server_json(serde_json::json!({"tokens":[
        {"text":"chào ","is_final":true},{"text":"bạn ","is_final":true},
        {"text":"Paseo","is_final":true},{"text":"<end>","is_final":true}],"finished":true}))
        .await;
    let events = drain(&mut rx);
    assert_eq!(
        events,
        vec![
            // the token batch itself updates the live partial first, as in the TS session
            SttEvent::Transcript(TranscriptEvent::partial(
                "soniox-seg-1",
                "Xin chào bạn Paseo"
            )),
            SttEvent::Committed(CommittedEvent {
                segment_id: "soniox-seg-1".into(),
                previous_segment_id: None
            }),
            SttEvent::Transcript(TranscriptEvent::final_(
                "soniox-seg-1",
                "Xin chào bạn Paseo"
            )),
        ]
    );
    // Soniox ends the stream after the empty frame; our side closes cleanly.
    assert!(ws.sent().contains(&WsOutgoing::Close(1000)));
    session.close().unwrap();
}

#[tokio::test]
async fn falls_back_to_local_when_the_api_key_is_missing() {
    let connector = Arc::new(FakeConnector::default());
    let fallback = Arc::new(FakeFallbackProvider::default());
    let stt = provider(config(""), connector.clone(), Some(fallback.clone()));
    let (session, mut rx) = stt.create_session(&session_params("vi")).unwrap();

    session.connect().await.unwrap();
    assert_eq!(connector.count(), 0);
    let sessions = fallback.sessions.lock().unwrap().clone();
    assert_eq!(sessions.len(), 1);
    assert!(sessions[0].connected.load(Ordering::SeqCst));

    session.append_pcm16(&[0u8; 320]);
    session.commit().unwrap();
    settle().await;
    assert_eq!(
        finals(&drain(&mut rx)),
        vec!["fallback-sherpa-transcript(320b)"]
    );
    session.close().unwrap();
    assert!(sessions[0].closed.load(Ordering::SeqCst));
}

#[tokio::test]
async fn missing_key_without_a_fallback_is_an_error() {
    let connector = Arc::new(FakeConnector::default());
    let stt = provider(config("  "), connector, None);
    let (session, _rx) = stt.create_session(&session_params("vi")).unwrap();
    let err = session.connect().await.unwrap_err();
    assert_eq!(
        err.to_string(),
        "Soniox API key is not configured and local STT fallback is unavailable"
    );
}

#[tokio::test]
async fn replays_buffered_pcm16_into_the_fallback_when_soniox_fails_mid_stream() {
    let connector = Arc::new(FakeConnector::default());
    let fallback = Arc::new(FakeFallbackProvider::default());
    let stt = provider(
        config("soniox_valid_key"),
        connector.clone(),
        Some(fallback.clone()),
    );
    let (session, mut rx) = stt.create_session(&session_params("vi")).unwrap();

    session.connect().await.unwrap();
    let ws = connector.socket(0);
    session.append_pcm16(&[1u8; 300]);
    session.append_pcm16(&[2u8; 340]);

    ws.server_json(
        serde_json::json!({"error_code":503,"error_message":"Service temporarily unavailable"}),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(10)).await;

    let sessions = fallback.sessions.lock().unwrap().clone();
    assert_eq!(sessions.len(), 1);
    {
        let appended = sessions[0].appended.lock().unwrap();
        assert_eq!(appended.len(), 2);
        assert_eq!(appended[0].len(), 300);
        assert_eq!(appended[1].len(), 340);
    }

    session.append_pcm16(&[3u8; 160]);
    session.commit().unwrap();
    settle().await;
    assert_eq!(
        finals(&drain(&mut rx)),
        vec!["fallback-sherpa-transcript(800b)"]
    );
    // the failed Soniox socket was closed
    assert!(ws.sent().contains(&WsOutgoing::Close(1000)));
    session.close().unwrap();
}

#[tokio::test]
async fn connection_failure_falls_back_or_errors() {
    let connector = Arc::new(FakeConnector::default());
    *connector.fail_with.lock().unwrap() = Some("dns failure".into());
    let fallback = Arc::new(FakeFallbackProvider::default());

    let stt = provider(config("k"), connector.clone(), Some(fallback.clone()));
    let (session, _rx) = stt.create_session(&session_params("en")).unwrap();
    session.connect().await.unwrap();
    assert_eq!(fallback.sessions.lock().unwrap().len(), 1);

    let stt = provider(config("k"), connector, None);
    let (session, _rx) = stt.create_session(&session_params("en")).unwrap();
    assert_eq!(
        session.connect().await.unwrap_err().to_string(),
        "dns failure"
    );
}

#[tokio::test(start_paused = true)]
async fn connection_timeout_reports_the_configured_budget() {
    let connector = Arc::new(FakeConnector::default());
    connector.hang.store(true, Ordering::SeqCst);
    let stt = provider(config("k"), connector, None);
    let (session, _rx) = stt.create_session(&session_params("en")).unwrap();
    let err = session.connect().await.unwrap_err();
    assert_eq!(
        err.to_string(),
        "Soniox WebSocket connection timed out after 200ms"
    );
}

#[tokio::test(start_paused = true)]
async fn commit_completes_after_the_finalize_wait_when_soniox_never_sends_finished() {
    let connector = Arc::new(FakeConnector::default());
    let stt = provider(config("k"), connector.clone(), None);
    let (session, mut rx) = stt.create_session(&session_params("vi")).unwrap();
    session.connect().await.unwrap();
    let ws = connector.socket(0);
    session.append_pcm16(&[1, 2]);
    ws.server_json(serde_json::json!({"tokens":[{"text":"xin chao","is_final":false}]}))
        .await;
    drain(&mut rx);

    session.commit().unwrap();
    settle().await;
    assert!(finals(&drain(&mut rx)).is_empty());
    tokio::time::advance(Duration::from_millis(600)).await;
    settle().await;
    assert_eq!(finals(&drain(&mut rx)), vec!["xin chao"]);
}

#[tokio::test]
async fn empty_commit_emits_committed_and_empty_final_without_touching_the_socket() {
    let connector = Arc::new(FakeConnector::default());
    let stt = provider(config("k"), connector.clone(), None);
    let (session, mut rx) = stt.create_session(&session_params("vi")).unwrap();
    session.connect().await.unwrap();
    session.commit().unwrap();
    session.commit().unwrap();
    let events = drain(&mut rx);
    assert_eq!(
        events,
        vec![
            SttEvent::Committed(CommittedEvent {
                segment_id: "soniox-seg-1".into(),
                previous_segment_id: None
            }),
            SttEvent::Transcript(TranscriptEvent::final_("soniox-seg-1", "")),
            SttEvent::Committed(CommittedEvent {
                segment_id: "soniox-seg-2".into(),
                previous_segment_id: Some("soniox-seg-1".into())
            }),
            SttEvent::Transcript(TranscriptEvent::final_("soniox-seg-2", "")),
        ]
    );
    assert_eq!(connector.socket(0).sent().len(), 1); // init only
}

#[tokio::test]
async fn next_utterance_after_a_commit_opens_a_fresh_stream_and_flushes_buffered_audio() {
    let connector = Arc::new(FakeConnector::default());
    let stt = provider(config("k"), connector.clone(), None);
    let (session, mut rx) = stt.create_session(&session_params("vi")).unwrap();
    session.connect().await.unwrap();
    session.append_pcm16(&[1, 2]);
    session.commit().unwrap();
    connector
        .socket(0)
        .server_json(serde_json::json!({"tokens":[{"text":"a","is_final":true}],"finished":true}))
        .await;
    assert_eq!(finals(&drain(&mut rx)), vec!["a"]);

    // socket 0 is gone: new audio buffers, reconnects, and is sent after the handshake
    session.append_pcm16(&[9, 9]);
    tokio::time::sleep(Duration::from_millis(5)).await;
    assert_eq!(connector.count(), 2);
    let sent = connector.socket(1).sent();
    assert!(matches!(sent[0], WsOutgoing::Text(_)));
    assert_eq!(sent[1], WsOutgoing::Binary(vec![9, 9]));
}

#[tokio::test]
async fn clear_discards_the_current_segment_text() {
    let connector = Arc::new(FakeConnector::default());
    let stt = provider(config("k"), connector.clone(), None);
    let (session, mut rx) = stt.create_session(&session_params("vi")).unwrap();
    session.connect().await.unwrap();
    session.append_pcm16(&[1, 2]);
    connector
        .socket(0)
        .server_json(serde_json::json!({"tokens":[{"text":"noise","is_final":false}]}))
        .await;
    drain(&mut rx);
    session.clear();
    session.commit().unwrap();
    let events = drain(&mut rx);
    assert_eq!(finals(&events), vec![""]);
}

#[tokio::test]
async fn abnormal_close_during_commit_without_text_surfaces_an_error() {
    let connector = Arc::new(FakeConnector::default());
    let stt = provider(config("k"), connector.clone(), None);
    let (session, mut rx) = stt.create_session(&session_params("vi")).unwrap();
    session.connect().await.unwrap();
    session.append_pcm16(&[1, 2]);
    session.commit().unwrap();
    connector.socket(0).server_close(1006).await;
    let events = drain(&mut rx);
    assert_eq!(
        events,
        vec![SttEvent::Error(
            "Soniox WebSocket closed unexpectedly during commit (code=1006)".into()
        )]
    );
}

#[tokio::test]
async fn clean_close_during_commit_completes_with_what_we_have() {
    let connector = Arc::new(FakeConnector::default());
    let stt = provider(config("k"), connector.clone(), None);
    let (session, mut rx) = stt.create_session(&session_params("vi")).unwrap();
    session.connect().await.unwrap();
    session.append_pcm16(&[1, 2]);
    connector
        .socket(0)
        .server_json(serde_json::json!({"tokens":[{"text":"partial","is_final":false}]}))
        .await;
    drain(&mut rx);
    session.commit().unwrap();
    connector.socket(0).server_close(1006).await;
    assert_eq!(finals(&drain(&mut rx)), vec!["partial"]);
}

#[tokio::test]
async fn server_error_without_a_fallback_after_text_completes_the_commit_otherwise_errors() {
    let connector = Arc::new(FakeConnector::default());
    let stt = provider(config("k"), connector.clone(), None);
    let (session, mut rx) = stt.create_session(&session_params("vi")).unwrap();
    session.connect().await.unwrap();
    session.append_pcm16(&[1, 2]);
    connector
        .socket(0)
        .server_json(serde_json::json!({"error_code":401,"error_message":"bad key"}))
        .await;
    assert_eq!(
        drain(&mut rx),
        vec![SttEvent::Error("Soniox STT error (401): bad key".into())]
    );
}

#[tokio::test]
async fn closed_session_rejects_connect_and_ignores_audio() {
    let connector = Arc::new(FakeConnector::default());
    let stt = provider(config("k"), connector.clone(), None);
    let (session, _rx) = stt.create_session(&session_params("vi")).unwrap();
    session.close().unwrap();
    assert_eq!(
        session.connect().await.unwrap_err().to_string(),
        "Soniox transcription session is closed"
    );
    session.append_pcm16(&[1, 2]);
    session.commit().unwrap();
    assert_eq!(connector.count(), 0);
}

/// Real transport: a local WebSocket server speaking the Soniox protocol.
#[tokio::test]
async fn tungstenite_transport_round_trip() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let received = Arc::new(Mutex::new(Vec::<String>::new()));
    let server_log = received.clone();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        while let Some(Ok(msg)) = ws.next().await {
            match msg {
                tokio_tungstenite::tungstenite::Message::Text(t) => {
                    server_log.lock().unwrap().push(format!("text:{t}"));
                }
                tokio_tungstenite::tungstenite::Message::Binary(b) if b.is_empty() => {
                    server_log.lock().unwrap().push("flush".into());
                    let reply = r#"{"tokens":[{"text":"xin chao","is_final":true},{"text":"<fin>","is_final":true}],"finished":true}"#;
                    ws.send(tokio_tungstenite::tungstenite::Message::text(reply))
                        .await
                        .unwrap();
                }
                tokio_tungstenite::tungstenite::Message::Binary(b) => {
                    server_log
                        .lock()
                        .unwrap()
                        .push(format!("audio:{}", b.len()));
                }
                tokio_tungstenite::tungstenite::Message::Close(_) => {
                    server_log.lock().unwrap().push("close".into());
                    break;
                }
                _ => {}
            }
        }
    });

    let cfg = SonioxSpeechProviderConfig {
        api_key: Some("key".into()),
        endpoint: Some(format!("ws://{addr}/transcribe-websocket")),
        model: Some("stt-rt-v5".into()),
        language_hints: None,
    };
    let stt = SonioxStt::new(SonioxSttOptions::new(move || Some(cfg.clone())));
    let (session, mut rx) = stt.create_session(&session_params("vi")).unwrap();
    session.connect().await.unwrap();
    session.append_pcm16(&[0u8; 640]);
    session.commit().unwrap();

    let mut final_text = None;
    for _ in 0..200 {
        for event in drain(&mut rx) {
            if let SttEvent::Transcript(t) = event {
                if t.is_final {
                    final_text = Some(t.transcript);
                }
            }
        }
        if final_text.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(final_text.as_deref(), Some("xin chao"));
    session.close().unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    let log = received.lock().unwrap().clone();
    assert!(log[0].starts_with("text:{"), "{log:?}");
    assert!(
        log[0].contains("\"language_hints\":[\"vi\",\"en\"]"),
        "{log:?}"
    );
    assert_eq!(&log[1..3], ["audio:640", "flush"]);
}
