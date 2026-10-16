//! Ports of `dictation/dictation-stream-manager.test.ts` plus protocol-shape and edge cases.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use base64::Engine;
use paseo_speech::dictation::{
    DictationInbound, DictationOutbound, DictationStreamManager, DictationStreamManagerParams,
};
use paseo_speech::dictation_debug::DictationDebug;
use paseo_speech::provider::{
    stt_event_channel, CommittedEvent, SttEvent, SttEventReceiver, SttEventSender, SttProvider,
    SttResolver, SttSession, SttSessionParams, TranscriptEvent,
};
use paseo_speech::{Result, SpeechError};
use tokio::sync::{mpsc, Notify};

const FMT_24K: &str = "audio/pcm;rate=24000;bits=16";

struct FakeSession {
    events: SttEventSender,
    rate: u32,
    gate: Option<Arc<Notify>>,
    close_fails: bool,
    connected: AtomicBool,
    closed: AtomicBool,
    commit_calls: AtomicUsize,
    clear_calls: AtomicUsize,
    appended: Mutex<Vec<Vec<u8>>>,
}

impl FakeSession {
    fn new(rate: u32) -> (Arc<Self>, SttEventReceiver) {
        Self::build(rate, None, false)
    }
    fn build(
        rate: u32,
        gate: Option<Arc<Notify>>,
        close_fails: bool,
    ) -> (Arc<Self>, SttEventReceiver) {
        let (tx, rx) = stt_event_channel();
        (
            Arc::new(Self {
                events: tx,
                rate,
                gate,
                close_fails,
                connected: AtomicBool::new(false),
                closed: AtomicBool::new(false),
                commit_calls: AtomicUsize::new(0),
                clear_calls: AtomicUsize::new(0),
                appended: Mutex::new(Vec::new()),
            }),
            rx,
        )
    }
    fn commits(&self) -> usize {
        self.commit_calls.load(Ordering::SeqCst)
    }
    fn clears(&self) -> usize {
        self.clear_calls.load(Ordering::SeqCst)
    }
    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }
    async fn committed(&self, id: &str) {
        let _ = self.events.send(SttEvent::Committed(CommittedEvent {
            segment_id: id.into(),
            previous_segment_id: None,
        }));
        settle().await;
    }
    async fn transcript(&self, id: &str, text: &str, is_final: bool) {
        let event = if is_final {
            TranscriptEvent::final_(id, text)
        } else {
            TranscriptEvent::partial(id, text)
        };
        let _ = self.events.send(SttEvent::Transcript(event));
        settle().await;
    }
    async fn error(&self, message: &str) {
        let _ = self.events.send(SttEvent::Error(message.into()));
        settle().await;
    }
}

#[async_trait]
impl SttSession for FakeSession {
    fn required_sample_rate(&self) -> u32 {
        self.rate
    }
    async fn connect(&self) -> Result<()> {
        if let Some(gate) = &self.gate {
            gate.notified().await;
        }
        self.connected.store(true, Ordering::SeqCst);
        Ok(())
    }
    fn append_pcm16(&self, pcm16le: &[u8]) {
        self.appended.lock().unwrap().push(pcm16le.to_vec());
    }
    fn commit(&self) -> Result<()> {
        self.commit_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn clear(&self) {
        self.clear_calls.fetch_add(1, Ordering::SeqCst);
    }
    fn close(&self) -> Result<()> {
        self.closed.store(true, Ordering::SeqCst);
        if self.close_fails {
            return Err(SpeechError::msg("provider cleanup failed"));
        }
        Ok(())
    }
}

/// Hands out pre-built sessions in order and records the params it was called with.
struct FakeProvider {
    queue: Mutex<Vec<(Arc<FakeSession>, SttEventReceiver)>>,
    last_params: Mutex<Option<SttSessionParams>>,
}

impl FakeProvider {
    fn single(session: (Arc<FakeSession>, SttEventReceiver)) -> Arc<Self> {
        Self::many(vec![session])
    }
    fn many(sessions: Vec<(Arc<FakeSession>, SttEventReceiver)>) -> Arc<Self> {
        Arc::new(Self {
            queue: Mutex::new(sessions),
            last_params: Mutex::new(None),
        })
    }
    fn last_language(&self) -> Option<String> {
        self.last_params
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|p| p.language.clone())
    }
}

impl SttProvider for FakeProvider {
    fn id(&self) -> &str {
        "fake"
    }
    fn create_session(
        &self,
        params: &SttSessionParams,
    ) -> Result<(Arc<dyn SttSession>, SttEventReceiver)> {
        *self.last_params.lock().unwrap() = Some(params.clone());
        let mut queue = self.queue.lock().unwrap();
        if queue.is_empty() {
            return Err(SpeechError::msg("no more fake sessions"));
        }
        let (session, rx) = queue.remove(0);
        Ok((session as Arc<dyn SttSession>, rx))
    }
}

async fn settle() {
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
}

fn pcm_b64(value: i16, samples: usize) -> String {
    let bytes: Vec<u8> = std::iter::repeat_n(value.to_le_bytes(), samples)
        .flatten()
        .collect();
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

struct Harness {
    manager: DictationStreamManager,
    out: mpsc::UnboundedReceiver<DictationOutbound>,
}

impl Harness {
    fn new(
        provider: Arc<FakeProvider>,
        configure: impl FnOnce(&mut DictationStreamManagerParams),
    ) -> Self {
        let (tx, out) = mpsc::unbounded_channel();
        let mut params = DictationStreamManagerParams::new(tx, "s1", provider);
        params.debug = DictationDebug::Disabled;
        configure(&mut params);
        Self {
            manager: DictationStreamManager::new(params),
            out,
        }
    }
    fn drain(&mut self) -> Vec<DictationOutbound> {
        let mut all = Vec::new();
        while let Ok(m) = self.out.try_recv() {
            all.push(m);
        }
        all
    }
    fn chunk(&self, id: &str, seq: i64, value: i16, samples: usize) {
        self.manager
            .handle_chunk(id, seq, &pcm_b64(value, samples), FMT_24K);
    }
}

fn find<'a>(messages: &'a [DictationOutbound], kind: &str) -> Option<&'a DictationOutbound> {
    messages.iter().find(|m| m.type_name() == kind)
}

fn final_text(messages: &[DictationOutbound]) -> Option<String> {
    match find(messages, "dictation_stream_final") {
        Some(DictationOutbound::DictationStreamFinal(p)) => Some(p.text.clone()),
        _ => None,
    }
}

fn timeout_ms(messages: &[DictationOutbound]) -> Option<u64> {
    match find(messages, "dictation_stream_finish_accepted") {
        Some(DictationOutbound::DictationStreamFinishAccepted(p)) => Some(p.timeout_ms),
        _ => None,
    }
}

#[tokio::test]
async fn buffer_too_small_is_benign_and_finalizes_with_existing_transcripts() {
    let session = FakeSession::new(24000);
    let s = session.0.clone();
    let mut h = Harness::new(FakeProvider::single(session), |p| {
        p.final_timeout_ms = Some(5000)
    });

    h.manager.handle_start("d1", FMT_24K).await;
    h.chunk("d1", 0, 2000, 2400);
    s.transcript("seg-1", "hello world", true).await;
    h.manager.handle_finish("d1", 0);
    settle().await;
    s.error("Error committing input audio buffer: buffer too small. Expected at least 100ms of audio, but buffer only has 0.00ms of audio.").await;

    let messages = h.drain();
    assert!(
        find(&messages, "dictation_stream_error").is_none(),
        "{messages:?}"
    );
    assert_eq!(final_text(&messages).as_deref(), Some("hello world"));
    assert!(s.is_closed());
}

#[tokio::test]
async fn language_defaults_to_en_and_is_passed_through() {
    let session = FakeSession::new(24000);
    let provider = FakeProvider::single(session);
    let h = Harness::new(provider.clone(), |_| {});
    h.manager.handle_start("d-lang", FMT_24K).await;
    assert_eq!(provider.last_language().as_deref(), Some("en"));

    let session = FakeSession::new(24000);
    let provider = FakeProvider::single(session);
    let h = Harness::new(provider.clone(), |p| p.language = Some("pt".into()));
    h.manager.handle_start("d-lang", FMT_24K).await;
    assert_eq!(provider.last_language().as_deref(), Some("pt"));
}

#[tokio::test]
async fn local_session_does_not_need_any_api_key() {
    let session = FakeSession::new(24000);
    let s = session.0.clone();
    let mut h = Harness::new(FakeProvider::single(session), |_| {});
    h.manager
        .handle_start("d-local", "audio/pcm;rate=16000;bits=16")
        .await;
    assert!(s.connected.load(Ordering::SeqCst));
    let messages = h.drain();
    assert!(find(&messages, "dictation_stream_error").is_none());
    assert!(find(&messages, "dictation_stream_ack").is_some());
}

#[tokio::test]
async fn auto_commits_while_streaming_and_assembles_final_in_segment_order() {
    let session = FakeSession::new(24000);
    let s = session.0.clone();
    let mut h = Harness::new(FakeProvider::single(session), |p| {
        p.auto_commit_seconds = Some(1.0)
    });

    h.manager.handle_start("d-segmented", FMT_24K).await;
    h.chunk("d-segmented", 0, 2000, 24000);
    assert_eq!(s.commits(), 1);

    s.committed("seg-1").await;
    s.transcript("seg-1", "hello", true).await;

    h.chunk("d-segmented", 1, 2000, 12000);
    h.manager.handle_finish("d-segmented", 1);
    assert_eq!(s.commits(), 2);

    s.committed("seg-2").await;
    s.transcript("seg-2", "world", true).await;

    let messages = h.drain();
    assert_eq!(final_text(&messages).as_deref(), Some("hello world"));
    // Partials carry the running, ordered transcript.
    let partials: Vec<String> = messages
        .iter()
        .filter_map(|m| match m {
            DictationOutbound::DictationStreamPartial(p) => Some(p.text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        partials,
        vec!["hello".to_string(), "hello world".to_string()]
    );
}

#[tokio::test]
async fn waits_for_an_in_flight_auto_commit_before_finalizing() {
    let session = FakeSession::new(24000);
    let s = session.0.clone();
    let mut h = Harness::new(FakeProvider::single(session), |p| {
        p.auto_commit_seconds = Some(1.0)
    });

    h.manager.handle_start("d", FMT_24K).await;
    h.chunk("d", 0, 2000, 24000);
    assert_eq!(s.commits(), 1);

    h.manager.handle_finish("d", 0);
    settle().await;
    assert!(final_text(&h.drain()).is_none());
    assert!(!s.is_closed());

    s.committed("seg-tail").await;
    s.transcript("seg-tail", "the final words", true).await;
    assert_eq!(final_text(&h.drain()).as_deref(), Some("the final words"));
    assert!(s.is_closed());
}

#[tokio::test]
async fn commits_tail_audio_appended_while_an_auto_commit_is_in_flight() {
    let session = FakeSession::new(24000);
    let s = session.0.clone();
    let mut h = Harness::new(FakeProvider::single(session), |p| {
        p.auto_commit_seconds = Some(1.0)
    });

    h.manager.handle_start("d", FMT_24K).await;
    h.chunk("d", 0, 2000, 24000);
    h.chunk("d", 1, 2000, 2400);
    s.committed("seg-first").await;
    s.transcript("seg-first", "the beginning", true).await;
    h.manager.handle_finish("d", 1);
    assert_eq!(s.commits(), 2);

    s.committed("seg-tail").await;
    s.transcript("seg-tail", "the final words", true).await;
    assert_eq!(
        final_text(&h.drain()).as_deref(),
        Some("the beginning the final words")
    );
}

#[tokio::test]
async fn does_not_wait_for_an_abandoned_partial_after_clearing_mid_stream_silence() {
    let session = FakeSession::new(24000);
    let s = session.0.clone();
    let mut h = Harness::new(FakeProvider::single(session), |p| {
        p.auto_commit_seconds = Some(1.0)
    });

    h.manager.handle_start("d", FMT_24K).await;
    h.chunk("d", 0, 2000, 24000);
    s.committed("seg-first").await;
    s.transcript("seg-first", "the beginning", true).await;

    s.transcript("seg-abandoned", "quiet partial", false).await;
    h.chunk("d", 1, 0, 24000);
    assert_eq!(s.clears(), 1);

    h.chunk("d", 2, 2000, 2400);
    h.manager.handle_finish("d", 2);
    s.committed("seg-final").await;
    s.transcript("seg-final", "the final words", true).await;

    assert_eq!(
        final_text(&h.drain()).as_deref(),
        Some("the beginning the final words")
    );
    assert!(s.is_closed());
}

#[tokio::test]
async fn adapts_finish_timeout_to_pending_committed_segments() {
    let session = FakeSession::new(24000);
    let s = session.0.clone();
    let mut h = Harness::new(FakeProvider::single(session), |p| {
        p.final_timeout_ms = Some(5000)
    });

    h.manager.handle_start("d", FMT_24K).await;
    h.chunk("d", 0, 2000, 2400);
    s.committed("seg-pending").await;
    h.manager.handle_finish("d", 0);

    let timeout = timeout_ms(&h.drain()).expect("finish accepted");
    assert!(timeout > 5000, "{timeout}");
}

#[tokio::test]
async fn does_not_extend_the_finish_timeout_for_abandoned_non_final_transcripts() {
    let session = FakeSession::new(24000);
    let s = session.0.clone();
    let mut h = Harness::new(FakeProvider::single(session), |p| {
        p.final_timeout_ms = Some(5000)
    });

    h.manager.handle_start("d", FMT_24K).await;
    h.chunk("d", 0, 2000, 2400);
    s.committed("seg-1").await;
    s.transcript("seg-1", "hello", true).await;
    s.transcript("seg-dangling", "hel", false).await;
    h.manager.handle_finish("d", 0);

    assert_eq!(timeout_ms(&h.drain()), Some(20_000));
}

#[tokio::test(start_paused = true)]
async fn drops_dangling_uncommitted_transcripts_when_finishing_after_silence_tail_clear() {
    let session = FakeSession::new(24000);
    let s = session.0.clone();
    let mut h = Harness::new(FakeProvider::single(session), |p| {
        p.final_timeout_ms = Some(5000);
        p.auto_commit_seconds = Some(1.0);
    });

    h.manager.handle_start("d", FMT_24K).await;
    h.chunk("d", 0, 2000, 24000);
    s.committed("seg-1").await;
    s.transcript("seg-1", "hello", true).await;
    h.chunk("d", 1, 0, 2400);
    s.transcript("seg-dangling", "", false).await;

    h.manager.handle_finish("d", 1);
    settle().await;
    tokio::time::advance(std::time::Duration::from_millis(5_100)).await;
    settle().await;

    let messages = h.drain();
    assert!(s.clears() > 0);
    assert!(
        find(&messages, "dictation_stream_error").is_none(),
        "{messages:?}"
    );
    assert_eq!(final_text(&messages).as_deref(), Some("hello"));
}

#[tokio::test]
async fn cancellation_during_stt_bootstrap_closes_producer_and_never_acknowledges_late_connection()
{
    let gate = Arc::new(Notify::new());
    let session = FakeSession::build(24000, Some(gate.clone()), false);
    let s = session.0.clone();
    let mut h = Harness::new(FakeProvider::single(session), |_| {});

    let manager = h.manager.clone();
    let starting = tokio::spawn(async move { manager.handle_start("dictation", FMT_24K).await });
    settle().await;
    h.manager.handle_cancel("dictation").unwrap();
    assert!(s.is_closed());
    gate.notify_one();
    starting.await.unwrap();

    assert_eq!(h.drain(), vec![]);
    h.manager.cleanup_all().unwrap();
}

#[tokio::test]
async fn closes_every_stream_when_one_provider_cleanup_fails() {
    let first = FakeSession::build(24000, None, true);
    let second = FakeSession::new(24000);
    let (f, sec) = (first.0.clone(), second.0.clone());
    let h = Harness::new(FakeProvider::many(vec![first, second]), |_| {});

    h.manager.handle_start("first", FMT_24K).await;
    h.manager.handle_start("second", FMT_24K).await;
    assert!(h.manager.has_demand());
    assert!(h.manager.cleanup_all().is_err());
    assert!(f.is_closed());
    assert!(sec.is_closed());
    assert!(!h.manager.has_demand());
}

#[tokio::test]
async fn chunks_are_forwarded_in_order_and_duplicates_are_reacked() {
    let session = FakeSession::new(24000);
    let s = session.0.clone();
    let mut h = Harness::new(FakeProvider::single(session), |p| {
        p.auto_commit_seconds = Some(0.0)
    });

    h.manager.handle_start("d", FMT_24K).await;
    assert_eq!(h.drain().len(), 1); // ack -1

    h.chunk("d", 1, 7, 10); // out of order: held back
    h.chunk("d", 0, 5, 10);
    let appended = s.appended.lock().unwrap().clone();
    assert_eq!(appended.len(), 2);
    assert_eq!(&appended[0][..2], &5i16.to_le_bytes());
    assert_eq!(&appended[1][..2], &7i16.to_le_bytes());

    h.chunk("d", 0, 5, 10); // duplicate
    assert_eq!(s.appended.lock().unwrap().len(), 2);

    let acks: Vec<i64> = h
        .drain()
        .into_iter()
        .filter_map(|m| match m {
            DictationOutbound::DictationStreamAck(a) => Some(a.ack_seq),
            _ => None,
        })
        .collect();
    assert_eq!(acks, vec![-1, 1, 1]);
}

#[tokio::test]
async fn resamples_input_to_the_provider_rate() {
    let session = FakeSession::new(24000);
    let s = session.0.clone();
    let h = Harness::new(FakeProvider::single(session), |p| {
        p.auto_commit_seconds = Some(0.0)
    });

    h.manager
        .handle_start("d", "audio/pcm;rate=48000;bits=16")
        .await;
    h.manager
        .handle_chunk("d", 0, &pcm_b64(1000, 480), "audio/pcm;rate=48000;bits=16");
    let appended: usize = s.appended.lock().unwrap().iter().map(Vec::len).sum();
    // 480 samples @48k -> ~240 samples @24k (carry sample may hold one back).
    assert!((236..=240).contains(&(appended / 2)), "{appended}");
}

#[tokio::test]
async fn mismatched_chunk_format_fails_non_retryably_and_cleans_up() {
    let session = FakeSession::new(24000);
    let s = session.0.clone();
    let mut h = Harness::new(FakeProvider::single(session), |_| {});
    h.manager.handle_start("d", FMT_24K).await;
    h.drain();
    h.manager
        .handle_chunk("d", 0, &pcm_b64(1, 4), "audio/pcm;rate=16000;bits=16");
    let messages = h.drain();
    match find(&messages, "dictation_stream_error") {
        Some(DictationOutbound::DictationStreamError(e)) => {
            assert!(!e.retryable);
            assert_eq!(
                e.error,
                "Mismatched dictation stream format: audio/pcm;rate=16000;bits=16"
            );
        }
        other => panic!("expected error, got {other:?}"),
    }
    assert!(s.is_closed());
    assert!(!h.manager.has_demand());
}

#[tokio::test]
async fn chunk_and_finish_before_start_report_retryable_errors() {
    let mut h = Harness::new(FakeProvider::many(vec![]), |_| {});
    h.manager.handle_chunk("nope", 0, "AAAA", FMT_24K);
    h.manager.handle_finish("nope", 0);
    let messages = h.drain();
    assert_eq!(messages.len(), 2);
    for m in messages {
        match m {
            DictationOutbound::DictationStreamError(e) => {
                assert_eq!(e.error, "Dictation stream not started");
                assert!(e.retryable);
            }
            other => panic!("{other:?}"),
        }
    }
}

#[tokio::test]
async fn finish_without_any_chunk_fails_fast() {
    let session = FakeSession::new(24000);
    let s = session.0.clone();
    let mut h = Harness::new(FakeProvider::single(session), |_| {});
    h.manager.handle_start("d", FMT_24K).await;
    h.drain();
    h.manager.handle_finish("d", 3);
    let messages = h.drain();
    match find(&messages, "dictation_stream_error") {
        Some(DictationOutbound::DictationStreamError(e)) => {
            assert_eq!(
                e.error,
                "Dictation finished (finalSeq=3) but no audio chunks were received"
            );
            assert!(e.retryable);
        }
        other => panic!("expected error, got {other:?}"),
    }
    assert!(s.is_closed());
}

#[tokio::test]
async fn unconfigured_provider_fails_non_retryably() {
    let (tx, mut out) = mpsc::unbounded_channel();
    let mut params = DictationStreamManagerParams::new(tx, "s1", SttResolver::none());
    params.debug = DictationDebug::Disabled;
    let manager = DictationStreamManager::new(params);
    manager.handle_start("d", FMT_24K).await;
    match out.try_recv().unwrap() {
        DictationOutbound::DictationStreamError(e) => {
            assert_eq!(e.error, "Dictation STT not configured");
            assert!(!e.retryable);
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn provider_error_fails_retryably_and_closes_the_session() {
    let session = FakeSession::new(24000);
    let s = session.0.clone();
    let mut h = Harness::new(FakeProvider::single(session), |_| {});
    h.manager.handle_start("d", FMT_24K).await;
    h.drain();
    s.error("upstream exploded").await;
    let messages = h.drain();
    match find(&messages, "dictation_stream_error") {
        Some(DictationOutbound::DictationStreamError(e)) => {
            assert_eq!(e.error, "upstream exploded");
            assert!(e.retryable);
        }
        other => panic!("{other:?}"),
    }
    assert!(s.is_closed());
}

#[tokio::test(start_paused = true)]
async fn final_timeout_fires_when_the_provider_never_answers() {
    let session = FakeSession::new(24000);
    let s = session.0.clone();
    let mut h = Harness::new(FakeProvider::single(session), |p| {
        p.final_timeout_ms = Some(1000)
    });
    h.manager.handle_start("d", FMT_24K).await;
    h.chunk("d", 0, 2000, 2400);
    h.manager.handle_finish("d", 0); // commits; provider stays silent
    h.drain();
    tokio::time::advance(std::time::Duration::from_millis(60_000)).await;
    settle().await;
    let messages = h.drain();
    match find(&messages, "dictation_stream_error") {
        Some(DictationOutbound::DictationStreamError(e)) => {
            assert_eq!(e.error, "Timed out waiting for final transcription");
            assert!(e.retryable);
        }
        other => panic!("{other:?}"),
    }
    assert!(s.is_closed());
}

#[tokio::test]
async fn silence_only_stream_finalizes_with_empty_text() {
    let session = FakeSession::new(24000);
    let s = session.0.clone();
    let mut h = Harness::new(FakeProvider::single(session), |_| {});
    h.manager.handle_start("d", FMT_24K).await;
    h.chunk("d", 0, 0, 2400);
    h.manager.handle_finish("d", 0);
    assert_eq!(s.commits(), 0);
    assert_eq!(s.clears(), 1);
    let messages = h.drain();
    assert_eq!(final_text(&messages).as_deref(), Some(""));
    assert!(s.is_closed());
}

#[tokio::test]
async fn restarting_the_same_dictation_id_closes_the_previous_session() {
    let first = FakeSession::new(24000);
    let second = FakeSession::new(24000);
    let (f, sec) = (first.0.clone(), second.0.clone());
    let h = Harness::new(FakeProvider::many(vec![first, second]), |_| {});
    h.manager.handle_start("d", FMT_24K).await;
    h.manager.handle_start("d", FMT_24K).await;
    assert!(f.is_closed());
    assert!(!sec.is_closed());
}

#[tokio::test]
async fn on_idle_runs_after_the_last_stream_goes_away() {
    let session = FakeSession::new(24000);
    let idle = Arc::new(AtomicUsize::new(0));
    let idle_hook = idle.clone();
    let h = Harness::new(FakeProvider::single(session), move |p| {
        p.on_idle = Some(Arc::new(move || {
            idle_hook.fetch_add(1, Ordering::SeqCst);
        }));
    });
    h.manager.handle_start("d", FMT_24K).await;
    assert_eq!(idle.load(Ordering::SeqCst), 0);
    h.manager.handle_cancel("d").unwrap();
    assert_eq!(idle.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn debug_capture_writes_combined_wav_and_reports_path_in_final_and_activity_log() {
    let dir = tempfile::tempdir().unwrap();
    let session = FakeSession::new(24000);
    let s = session.0.clone();
    let debug_dir = dir.path().to_path_buf();
    let mut h = Harness::new(FakeProvider::single(session), move |p| {
        p.debug = DictationDebug::Dir(debug_dir);
        p.auto_commit_seconds = Some(0.0);
    });
    h.manager.handle_start("d/1", FMT_24K).await;
    h.chunk("d/1", 0, 2000, 2400);
    h.manager.handle_finish("d/1", 0);
    s.committed("seg").await;
    s.transcript("seg", "hi", true).await;

    let messages = h.drain();
    let path = match find(&messages, "dictation_stream_final") {
        Some(DictationOutbound::DictationStreamFinal(p)) => {
            p.debug_recording_path.clone().expect("path")
        }
        other => panic!("{other:?}"),
    };
    assert!(path.ends_with("combined.wav"), "{path}");
    let wav = std::fs::read(&path).unwrap();
    assert_eq!(&wav[0..4], b"RIFF");
    assert_eq!(wav.len(), 44 + 2400 * 2);
    // chunk file sits next to it
    assert!(std::path::Path::new(&path)
        .parent()
        .unwrap()
        .join("chunk_000000.pcm")
        .exists());
    match find(&messages, "activity_log") {
        Some(DictationOutbound::ActivityLog(a)) => {
            assert_eq!(a.content, format!("Saved dictation audio: {path}"));
            assert_eq!(a.metadata["dictationId"], "d/1");
            assert_eq!(a.kind, "system");
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn inbound_and_outbound_json_match_the_protocol_package() {
    let start: DictationInbound = serde_json::from_str(
        r#"{"type":"dictation_stream_start","dictationId":"d","format":"audio/pcm;rate=16000;bits=16"}"#,
    )
    .unwrap();
    assert!(matches!(
        start,
        DictationInbound::DictationStreamStart { .. }
    ));
    let chunk: DictationInbound = serde_json::from_str(
        r#"{"type":"dictation_stream_chunk","dictationId":"d","seq":2,"audio":"AAAA","format":"f"}"#,
    )
    .unwrap();
    assert!(matches!(
        chunk,
        DictationInbound::DictationStreamChunk { seq: 2, .. }
    ));
    let finish: DictationInbound = serde_json::from_str(
        r#"{"type":"dictation_stream_finish","dictationId":"d","finalSeq":9}"#,
    )
    .unwrap();
    assert!(matches!(
        finish,
        DictationInbound::DictationStreamFinish { final_seq: 9, .. }
    ));
    let cancel: DictationInbound =
        serde_json::from_str(r#"{"type":"dictation_stream_cancel","dictationId":"d"}"#).unwrap();
    assert!(matches!(
        cancel,
        DictationInbound::DictationStreamCancel { .. }
    ));

    let ack = serde_json::to_value(DictationOutbound::DictationStreamAck(
        paseo_speech::dictation::AckPayload {
            dictation_id: "d".into(),
            ack_seq: -1,
        },
    ))
    .unwrap();
    assert_eq!(
        ack,
        serde_json::json!({"type":"dictation_stream_ack","payload":{"dictationId":"d","ackSeq":-1}})
    );
    let accepted = serde_json::to_value(DictationOutbound::DictationStreamFinishAccepted(
        paseo_speech::dictation::FinishAcceptedPayload {
            dictation_id: "d".into(),
            timeout_ms: 10000,
        },
    ))
    .unwrap();
    assert_eq!(
        accepted,
        serde_json::json!({"type":"dictation_stream_finish_accepted","payload":{"dictationId":"d","timeoutMs":10000}})
    );
    let fin = serde_json::to_value(DictationOutbound::DictationStreamFinal(
        paseo_speech::dictation::FinalPayload {
            dictation_id: "d".into(),
            text: "t".into(),
            debug_recording_path: None,
        },
    ))
    .unwrap();
    assert_eq!(
        fin,
        serde_json::json!({"type":"dictation_stream_final","payload":{"dictationId":"d","text":"t"}})
    );
}

#[tokio::test]
async fn handle_inbound_drives_a_full_dictation() {
    let session = FakeSession::new(16000);
    let s = session.0.clone();
    let mut h = Harness::new(FakeProvider::single(session), |p| {
        p.auto_commit_seconds = Some(0.0)
    });
    let start: DictationInbound = serde_json::from_str(
        r#"{"type":"dictation_stream_start","dictationId":"d","format":"audio/pcm;rate=16000;bits=16"}"#,
    )
    .unwrap();
    h.manager.handle_inbound(start).await.unwrap();
    let chunk = DictationInbound::DictationStreamChunk {
        dictation_id: "d".into(),
        seq: 0,
        audio: pcm_b64(3000, 1600),
        format: "audio/pcm;rate=16000;bits=16".into(),
    };
    h.manager.handle_inbound(chunk).await.unwrap();
    h.manager
        .handle_inbound(DictationInbound::DictationStreamFinish {
            dictation_id: "d".into(),
            final_seq: 0,
        })
        .await
        .unwrap();
    s.committed("a").await;
    s.transcript("a", "xin chao", true).await;
    assert_eq!(final_text(&h.drain()).as_deref(), Some("xin chao"));
}
