//! Silero-style turn detection (port of `sherpa/silero-vad-session.ts`).
//!
//! The speech/non-speech decision per 32 ms window comes from a [`SpeechDetector`] (Silero via
//! sherpa-onnx in production); the confirm/silence state machine on top is plain Rust so it is
//! testable without a model.

use std::sync::{Arc, Mutex};

use crate::audio::pcm16le_to_float32;
use crate::error::Result;
use crate::provider::{TurnDetectionSession, TurnEvent, TurnEventReceiver, TurnEventSender};

pub const DEFAULT_SAMPLE_RATE: u32 = 16000;
pub const DEFAULT_BUFFER_SIZE_SECONDS: f32 = 60.0;
pub const DEFAULT_SILERO_THRESHOLD: f32 = 0.5;
pub const DEFAULT_WINDOW_SIZE: usize = 512;
/// Silero internal durations: kept low so `is_detected()` tracks actual sound. The state machine
/// below owns speech confirmation and end-of-speech detection.
pub const SILERO_MIN_SILENCE_DURATION: f32 = 0.2;
pub const SILERO_MIN_SPEECH_DURATION: f32 = 0.1;
pub const DEFAULT_CONFIRM_MS: f64 = 800.0;
pub const DEFAULT_SILENCE_MS: f64 = 1000.0;

/// Per-window speech detector.
pub trait SpeechDetector: Send {
    fn accept_waveform(&mut self, window: &[f32]);
    fn is_detected(&self) -> bool;
    fn flush(&mut self);
    fn reset(&mut self);
}

#[derive(Debug, Clone)]
pub struct VadSessionConfig {
    pub sample_rate: u32,
    pub window_size: usize,
    pub confirm_ms: f64,
    pub silence_ms: f64,
}

impl Default for VadSessionConfig {
    fn default() -> Self {
        Self {
            sample_rate: DEFAULT_SAMPLE_RATE,
            window_size: DEFAULT_WINDOW_SIZE,
            confirm_ms: DEFAULT_CONFIRM_MS,
            silence_ms: DEFAULT_SILENCE_MS,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Phase {
    Idle,
    Confirming { started_at: f64 },
    Speaking,
    Ending { started_at: f64 },
}

struct State {
    detector: Box<dyn SpeechDetector>,
    input: Vec<f32>,
    connected: bool,
    phase: Phase,
    window_timestamp: f64,
}

pub struct VadTurnSession {
    config: VadSessionConfig,
    ms_per_window: f64,
    events: TurnEventSender,
    state: Mutex<State>,
}

impl VadTurnSession {
    pub fn new(
        detector: Box<dyn SpeechDetector>,
        config: VadSessionConfig,
    ) -> (Arc<Self>, TurnEventReceiver) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let ms_per_window = config.window_size as f64 / config.sample_rate as f64 * 1000.0;
        let session = Arc::new(Self {
            config,
            ms_per_window,
            events: tx,
            state: Mutex::new(State {
                detector,
                input: Vec::new(),
                connected: false,
                phase: Phase::Idle,
                window_timestamp: 0.0,
            }),
        });
        (session, rx)
    }

    fn emit(&self, event: TurnEvent) {
        let _ = self.events.send(Ok(event));
    }

    fn emit_error(&self, message: String) {
        let _ = self.events.send(Err(message));
    }

    fn step(&self, st: &mut State) {
        let detected = st.detector.is_detected();
        let now = st.window_timestamp;
        match st.phase {
            Phase::Idle => {
                if detected {
                    st.phase = Phase::Confirming { started_at: now };
                }
            }
            Phase::Confirming { started_at } => {
                if !detected {
                    st.phase = Phase::Idle;
                } else if now - started_at >= self.config.confirm_ms {
                    st.phase = Phase::Speaking;
                    self.emit(TurnEvent::SpeechStarted);
                }
            }
            Phase::Speaking => {
                if !detected {
                    st.phase = Phase::Ending { started_at: now };
                }
            }
            Phase::Ending { started_at } => {
                if detected {
                    st.phase = Phase::Speaking;
                } else if now - started_at >= self.config.silence_ms {
                    st.phase = Phase::Idle;
                    self.emit(TurnEvent::SpeechStopped);
                }
            }
        }
    }
}

impl TurnDetectionSession for VadTurnSession {
    fn required_sample_rate(&self) -> u32 {
        self.config.sample_rate
    }

    fn connect(&self) {
        self.state.lock().unwrap().connected = true;
    }

    fn append_pcm16(&self, pcm16le: &[u8]) {
        let mut st = self.state.lock().unwrap();
        if !st.connected {
            drop(st);
            self.emit_error("Turn detection session not connected".into());
            return;
        }
        if pcm16le.is_empty() {
            return;
        }
        let samples = match pcm16le_to_float32(pcm16le, 1.0) {
            Ok(s) => s,
            Err(err) => {
                drop(st);
                self.emit_error(err.to_string());
                return;
            }
        };
        st.input.extend_from_slice(&samples);
        // Strictly greater, like the circular-buffer loop in TS: up to one window stays queued.
        while st.input.len() > self.config.window_size {
            let window: Vec<f32> = st.input.drain(..self.config.window_size).collect();
            st.detector.accept_waveform(&window);
            st.window_timestamp += self.ms_per_window;
            self.step(&mut st);
        }
    }

    fn flush(&self) {
        let mut st = self.state.lock().unwrap();
        if !st.connected {
            return;
        }
        st.detector.flush();
        self.step(&mut st);
        match st.phase {
            Phase::Speaking | Phase::Ending { .. } => {
                st.phase = Phase::Idle;
                self.emit(TurnEvent::SpeechStopped);
            }
            Phase::Confirming { .. } => st.phase = Phase::Idle,
            Phase::Idle => {}
        }
    }

    fn reset(&self) {
        let mut st = self.state.lock().unwrap();
        st.detector.reset();
        st.input.clear();
        st.phase = Phase::Idle;
    }

    fn close(&self) {
        self.reset();
        let mut st = self.state.lock().unwrap();
        st.connected = false;
        st.window_timestamp = 0.0;
    }
}

/// Builds sessions around detectors produced by `make_detector`
/// (`SherpaSileroTurnDetectionProvider` in TS).
pub struct VadTurnDetectionProvider {
    make_detector: Box<dyn Fn() -> Result<Box<dyn SpeechDetector>> + Send + Sync>,
    config: VadSessionConfig,
}

impl VadTurnDetectionProvider {
    pub fn new(
        config: VadSessionConfig,
        make_detector: impl Fn() -> Result<Box<dyn SpeechDetector>> + Send + Sync + 'static,
    ) -> Self {
        Self {
            make_detector: Box::new(make_detector),
            config,
        }
    }
}

impl crate::provider::TurnDetectionProvider for VadTurnDetectionProvider {
    fn id(&self) -> &str {
        "local"
    }
    fn create_session(&self) -> Result<(Arc<dyn TurnDetectionSession>, TurnEventReceiver)> {
        let detector = (self.make_detector)()?;
        let (session, rx) = VadTurnSession::new(detector, self.config.clone());
        Ok((session, rx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Detector scripted by window index.
    struct Scripted {
        windows_seen: usize,
        script: Arc<dyn Fn(usize) -> bool + Send + Sync>,
        flushes: Arc<Mutex<usize>>,
        resets: Arc<Mutex<usize>>,
    }

    impl SpeechDetector for Scripted {
        fn accept_waveform(&mut self, window: &[f32]) {
            assert_eq!(window.len(), DEFAULT_WINDOW_SIZE);
            self.windows_seen += 1;
        }
        fn is_detected(&self) -> bool {
            (self.script)(self.windows_seen)
        }
        fn flush(&mut self) {
            *self.flushes.lock().unwrap() += 1;
        }
        fn reset(&mut self) {
            *self.resets.lock().unwrap() += 1;
            self.windows_seen = 0;
        }
    }

    type Counter = Arc<Mutex<usize>>;

    fn session(
        script: impl Fn(usize) -> bool + Send + Sync + 'static,
    ) -> (Arc<VadTurnSession>, TurnEventReceiver, Counter, Counter) {
        let flushes = Arc::new(Mutex::new(0));
        let resets = Arc::new(Mutex::new(0));
        let detector = Scripted {
            windows_seen: 0,
            script: Arc::new(script),
            flushes: flushes.clone(),
            resets: resets.clone(),
        };
        let (s, rx) = VadTurnSession::new(Box::new(detector), VadSessionConfig::default());
        s.connect();
        (s, rx, flushes, resets)
    }

    /// `n` windows of PCM (+1 sample, since one window stays queued until more audio arrives).
    fn feed_windows(s: &VadTurnSession, n: usize) {
        let samples = n * DEFAULT_WINDOW_SIZE + 1;
        s.append_pcm16(&vec![0u8; samples * 2]);
    }

    fn drain(rx: &mut TurnEventReceiver) -> Vec<Result<TurnEvent, String>> {
        let mut out = Vec::new();
        while let Ok(e) = rx.try_recv() {
            out.push(e);
        }
        out
    }

    #[test]
    fn required_rate_and_window_timing() {
        let (s, _rx, _, _) = session(|_| false);
        assert_eq!(s.required_sample_rate(), 16000);
        assert_eq!(s.ms_per_window, 32.0);
    }

    #[test]
    fn speech_is_confirmed_only_after_confirm_ms_of_continuous_detection() {
        // detected from window 1 onward; confirm needs now - startedAt >= 800ms = 25 windows.
        let (s, mut rx, _, _) = session(|n| n >= 1);
        feed_windows(&s, 25); // startedAt at window 1 (t=32); window 25 -> 768-32=736 < 800
        assert!(drain(&mut rx).is_empty());
        feed_windows(&s, 1); // window 26 -> 832-32=800
        assert_eq!(drain(&mut rx), vec![Ok(TurnEvent::SpeechStarted)]);
    }

    #[test]
    fn detection_that_drops_before_confirmation_is_discarded() {
        let (s, mut rx, _, _) = session(|n| (1..=10).contains(&n));
        feed_windows(&s, 60);
        assert!(drain(&mut rx).is_empty());
    }

    #[test]
    fn speech_stops_after_silence_ms_and_resumed_speech_cancels_the_ending() {
        // speech windows 1..=40, short gap (10 windows = 320ms) 41..=50, speech 51..=70, silence after.
        let (s, mut rx, _, _) = session(|n| (1..=40).contains(&n) || (51..=70).contains(&n));
        feed_windows(&s, 70);
        assert_eq!(drain(&mut rx), vec![Ok(TurnEvent::SpeechStarted)]);
        // silence from window 71; ending starts at window 71 (t=2272ms). Window 102 is at 992ms,
        // window 103 at 1024ms >= 1000ms, so the stop fires on window 103.
        feed_windows(&s, 32);
        assert!(drain(&mut rx).is_empty());
        feed_windows(&s, 1);
        assert_eq!(drain(&mut rx), vec![Ok(TurnEvent::SpeechStopped)]);
        // idle again: more silence emits nothing
        feed_windows(&s, 100);
        assert!(drain(&mut rx).is_empty());
    }

    #[test]
    fn flush_forces_speech_stopped_while_speaking_and_drops_unconfirmed_speech() {
        let (s, mut rx, flushes, _) = session(|n| n >= 1);
        feed_windows(&s, 40);
        assert_eq!(drain(&mut rx), vec![Ok(TurnEvent::SpeechStarted)]);
        s.flush();
        assert_eq!(drain(&mut rx), vec![Ok(TurnEvent::SpeechStopped)]);
        assert_eq!(*flushes.lock().unwrap(), 1);

        // unconfirmed: only 5 windows of detection, then flush
        let (s, mut rx, _, _) = session(|n| n >= 1);
        feed_windows(&s, 5);
        s.flush();
        assert!(drain(&mut rx).is_empty());
        // and it did return to idle: fresh detection must be re-confirmed from scratch
        feed_windows(&s, 10);
        assert!(drain(&mut rx).is_empty());
    }

    #[test]
    fn flush_while_idle_or_unconnected_is_silent() {
        let (s, mut rx, flushes, _) = session(|_| false);
        s.flush();
        assert!(drain(&mut rx).is_empty());
        assert_eq!(*flushes.lock().unwrap(), 1);
        s.close();
        s.flush();
        assert_eq!(
            *flushes.lock().unwrap(),
            1,
            "flush on a closed session never reaches the detector"
        );
    }

    #[test]
    fn reset_clears_state_and_close_disconnects() {
        let (s, mut rx, _, resets) = session(|n| n >= 1);
        feed_windows(&s, 30);
        assert_eq!(drain(&mut rx), vec![Ok(TurnEvent::SpeechStarted)]);
        s.reset();
        assert_eq!(*resets.lock().unwrap(), 1);
        feed_windows(&s, 5); // detection restarts, not confirmed yet
        assert!(drain(&mut rx).is_empty());
        s.close();
        s.append_pcm16(&[0, 0]);
        assert_eq!(
            drain(&mut rx),
            vec![Err("Turn detection session not connected".to_string())]
        );
    }

    #[test]
    fn unconnected_append_and_odd_pcm_report_errors() {
        let flushes = Arc::new(Mutex::new(0));
        let resets = Arc::new(Mutex::new(0));
        let detector = Scripted {
            windows_seen: 0,
            script: Arc::new(|_| false),
            flushes,
            resets,
        };
        let (s, mut rx) = VadTurnSession::new(Box::new(detector), VadSessionConfig::default());
        s.append_pcm16(&[0, 0]);
        assert_eq!(
            drain(&mut rx),
            vec![Err("Turn detection session not connected".to_string())]
        );
        s.connect();
        s.append_pcm16(&[]);
        assert!(drain(&mut rx).is_empty());
        s.append_pcm16(&[1, 2, 3]);
        assert_eq!(
            drain(&mut rx),
            vec![Err("PCM16 chunk byteLength must be even, got 3".to_string())]
        );
    }

    #[test]
    fn audio_is_windowed_across_arbitrary_chunk_boundaries() {
        // Feed in odd-sized slices; the detector asserts every window is exactly 512 samples.
        let (s, mut rx, _, _) = session(|n| n >= 1);
        let total_samples = 40 * DEFAULT_WINDOW_SIZE + 1;
        let bytes = vec![0u8; total_samples * 2];
        for chunk in bytes.chunks(1234 * 2) {
            s.append_pcm16(chunk);
        }
        assert_eq!(drain(&mut rx), vec![Ok(TurnEvent::SpeechStarted)]);
    }
}
