//! OpenAI-compatible STT/TTS request shapes (ports of `openai/stt.test.ts`, `tts.test.ts`,
//! `runtime.test.ts`) against a local HTTP server instead of a mocked SDK.

mod support;

use std::sync::Arc;
use std::time::Duration;

use paseo_speech::provider::{
    SttEvent, SttEventReceiver, SttProvider, SttSession, SttSessionParams, TtsProvider,
};
use paseo_speech::providers::openai::{OpenAiStt, OpenAiTts, SttConfig, TtsConfig};
use support::{Reply, TestServer};

async fn next_event(rx: &mut SttEventReceiver) -> SttEvent {
    tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("event in time")
        .expect("channel open")
}

async fn transcript_session(
    server: &TestServer,
    config: SttConfig,
    language: &str,
    prompt: Option<&str>,
) -> (Arc<dyn SttSession>, SttEventReceiver) {
    let _ = server;
    let provider = OpenAiStt::new(config);
    assert_eq!(provider.id(), "openai");
    provider
        .create_session(&SttSessionParams {
            language: Some(language.into()),
            prompt: prompt.map(String::from),
        })
        .unwrap()
}

#[tokio::test]
async fn stt_posts_multipart_to_the_configured_base_url_with_prompt_language_and_model() {
    let server = TestServer::start(|_| Reply::json(200, r#"{"text":"hello"}"#)).await;
    let (session, mut rx) = transcript_session(
        &server,
        SttConfig {
            api_key: "sk-test".into(),
            base_url: Some(format!("{}/", server.base_url)),
            model: Some("gpt-4o-transcribe".into()),
            confidence_threshold: None,
        },
        "en",
        Some("Only transcribe the speaker."),
    )
    .await;

    session.connect().await.unwrap();
    session.append_pcm16(&[0, 0, 0, 0]);
    session.commit().unwrap();

    assert!(matches!(next_event(&mut rx).await, SttEvent::Committed(_)));
    match next_event(&mut rx).await {
        SttEvent::Transcript(t) => {
            assert!(t.is_final);
            assert_eq!(t.transcript, "hello");
        }
        other => panic!("{other:?}"),
    }

    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    let req = &requests[0];
    assert_eq!(req.method, "POST");
    assert_eq!(req.path, "/v1/audio/transcriptions");
    assert_eq!(req.headers["authorization"], "Bearer sk-test");
    assert!(req.headers["content-type"].starts_with("multipart/form-data"));
    assert_eq!(
        req.multipart_field("model").as_deref(),
        Some("gpt-4o-transcribe")
    );
    assert_eq!(req.multipart_field("language").as_deref(), Some("en"));
    assert_eq!(
        req.multipart_field("prompt").as_deref(),
        Some("Only transcribe the speaker.")
    );
    assert_eq!(
        req.multipart_field("response_format").as_deref(),
        Some("json")
    );
    assert_eq!(
        req.multipart_field("include[]").as_deref(),
        Some("logprobs")
    );
    assert!(req.body_text().contains("filename=\"audio-"));
    assert!(req.body_text().contains("Content-Type: audio/wav"));
    // 24 kHz mono WAV header + 4 PCM bytes
    assert!(req.body.windows(4).any(|w| w == b"RIFF"));
}

#[tokio::test]
async fn stt_omits_include_for_whisper_and_prompt_when_absent() {
    let server =
        TestServer::start(|_| Reply::json(200, r#"{"text":"ok","language":"english"}"#)).await;
    let (session, mut rx) = transcript_session(
        &server,
        SttConfig {
            api_key: "k".into(),
            base_url: Some(server.base_url.clone()),
            ..Default::default()
        },
        "vi",
        None,
    )
    .await;
    session.connect().await.unwrap();
    session.append_pcm16(&[1, 0, 2, 0]);
    session.commit().unwrap();
    let _ = next_event(&mut rx).await;
    match next_event(&mut rx).await {
        SttEvent::Transcript(t) => assert_eq!(t.language.as_deref(), Some("english")),
        other => panic!("{other:?}"),
    }
    let req = &server.requests()[0];
    assert_eq!(req.multipart_field("model").as_deref(), Some("whisper-1"));
    assert!(!req.multipart_has_field("include[]"));
    assert!(!req.multipart_has_field("prompt"));
    assert_eq!(req.multipart_field("language").as_deref(), Some("vi"));
}

#[tokio::test]
async fn stt_computes_average_logprob_and_low_confidence() {
    let body = r#"{"text":"maybe","logprobs":[{"token":"a","logprob":-4.0,"bytes":[97]},{"token":"b","logprob":-2.0}]}"#;
    let server = TestServer::start(move |_| Reply::json(200, body)).await;
    let (session, mut rx) = transcript_session(
        &server,
        SttConfig {
            api_key: "k".into(),
            base_url: Some(server.base_url.clone()),
            model: Some("gpt-4o-mini-transcribe".into()),
            confidence_threshold: Some(-2.5),
        },
        "en",
        None,
    )
    .await;
    session.connect().await.unwrap();
    session.append_pcm16(&[1, 0]);
    session.commit().unwrap();
    let _ = next_event(&mut rx).await;
    match next_event(&mut rx).await {
        SttEvent::Transcript(t) => {
            assert_eq!(t.avg_logprob, Some(-3.0));
            assert_eq!(t.is_low_confidence, Some(true));
            let tokens = t.logprobs.unwrap();
            assert_eq!(tokens.len(), 2);
            assert_eq!(tokens[0].bytes.as_deref(), Some(&[97u8][..]));
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn stt_http_errors_surface_as_wrapped_error_events() {
    let server = TestServer::start(|_| {
        Reply::json(
            401,
            r#"{"error":{"message":"Incorrect API key provided","type":"invalid_request_error"}}"#,
        )
    })
    .await;
    let (session, mut rx) = transcript_session(
        &server,
        SttConfig {
            api_key: "bad".into(),
            base_url: Some(server.base_url.clone()),
            ..Default::default()
        },
        "en",
        None,
    )
    .await;
    session.connect().await.unwrap();
    session.append_pcm16(&[1, 0]);
    session.commit().unwrap();
    let _ = next_event(&mut rx).await;
    match next_event(&mut rx).await {
        SttEvent::Error(message) => {
            assert_eq!(
                message,
                "STT transcription failed: 401 Incorrect API key provided"
            );
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn stt_empty_commit_yields_empty_low_confidence_transcript_without_a_request() {
    let server = TestServer::start(|_| Reply::json(200, r#"{"text":"x"}"#)).await;
    let (session, mut rx) = transcript_session(
        &server,
        SttConfig {
            api_key: "k".into(),
            base_url: Some(server.base_url.clone()),
            ..Default::default()
        },
        "en",
        None,
    )
    .await;
    session.connect().await.unwrap();
    session.commit().unwrap();
    let _ = next_event(&mut rx).await;
    match next_event(&mut rx).await {
        SttEvent::Transcript(t) => {
            assert_eq!(t.transcript, "");
            assert!(t.is_final);
            assert_eq!(t.is_low_confidence, Some(true));
            assert_eq!(t.language.as_deref(), Some("en"));
        }
        other => panic!("{other:?}"),
    }
    assert!(server.requests().is_empty());
}

#[tokio::test]
async fn stt_unconnected_session_reports_errors() {
    let server = TestServer::start(|_| Reply::json(200, "{}")).await;
    let (session, mut rx) = transcript_session(
        &server,
        SttConfig {
            api_key: "k".into(),
            ..Default::default()
        },
        "en",
        None,
    )
    .await;
    session.append_pcm16(&[1, 0]);
    assert_eq!(
        next_event(&mut rx).await,
        SttEvent::Error("STT session not connected".into())
    );
    session.commit().unwrap();
    assert_eq!(
        next_event(&mut rx).await,
        SttEvent::Error("STT session not connected".into())
    );
}

/// Regression for a TypeScript bug: the session reset its buffer and segment id only after the
/// request finished, dropping audio appended during an in-flight transcription.
#[tokio::test]
async fn stt_audio_appended_while_a_commit_is_in_flight_goes_to_the_next_segment() {
    let server = TestServer::start(|_| Reply::json(200, r#"{"text":"t"}"#)).await;
    let (session, mut rx) = transcript_session(
        &server,
        SttConfig {
            api_key: "k".into(),
            base_url: Some(server.base_url.clone()),
            ..Default::default()
        },
        "en",
        None,
    )
    .await;
    session.connect().await.unwrap();

    session.append_pcm16(&[1, 0, 1, 0]);
    session.commit().unwrap();
    // Appended before the first request can possibly have finished (no await in between).
    session.append_pcm16(&[2, 0, 2, 0, 2, 0]);
    session.commit().unwrap();

    let mut committed = Vec::new();
    let mut finals = 0;
    while finals < 2 {
        match next_event(&mut rx).await {
            SttEvent::Committed(c) => committed.push(c),
            SttEvent::Transcript(t) => {
                assert!(t.is_final);
                finals += 1;
            }
            SttEvent::Error(e) => panic!("{e}"),
        }
    }
    assert_eq!(committed.len(), 2);
    assert_ne!(committed[0].segment_id, committed[1].segment_id);
    assert_eq!(committed[0].previous_segment_id, None);
    assert_eq!(
        committed[1].previous_segment_id.as_deref(),
        Some(committed[0].segment_id.as_str())
    );

    // Each request carried exactly its own audio: 44-byte header + 4 / 6 PCM bytes.
    let mut sizes: Vec<usize> = server
        .requests()
        .iter()
        .map(|r| {
            let at = r.body.windows(4).position(|w| w == b"RIFF").unwrap();
            let data_len_at = at + 40;
            u32::from_le_bytes(r.body[data_len_at..data_len_at + 4].try_into().unwrap()) as usize
        })
        .collect();
    sizes.sort();
    assert_eq!(sizes, vec![4, 6]);
}

#[tokio::test]
async fn tts_posts_json_and_streams_the_body_back() {
    let server = TestServer::start(|_| Reply::bytes(vec![1, 2, 3, 4, 5, 6])).await;
    let mut config = TtsConfig::new("sk-test");
    config.base_url = Some(server.base_url.clone());
    config.voice = "Aiden".into();
    config.model = "kokoro-v1:Q8_0".into();
    let tts = OpenAiTts::new(config);
    assert_eq!(
        tts.config().base_url.as_deref(),
        Some(server.base_url.as_str())
    );

    let result = tts.synthesize_speech("hello there").await.unwrap();
    assert_eq!(result.format, "pcm");
    let audio = result.collect().await.unwrap();
    assert_eq!(audio, vec![1, 2, 3, 4, 5, 6]);

    let req = &server.requests()[0];
    assert_eq!(req.path, "/v1/audio/speech");
    assert_eq!(req.headers["authorization"], "Bearer sk-test");
    let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
    assert_eq!(
        body,
        serde_json::json!({"model":"kokoro-v1:Q8_0","voice":"Aiden","input":"hello there","response_format":"pcm"})
    );
}

#[tokio::test]
async fn tts_defaults_and_error_wrapping() {
    let config = TtsConfig::new("k");
    assert_eq!(
        (
            config.model.as_str(),
            config.voice.as_str(),
            config.response_format.as_str()
        ),
        ("tts-1", "alloy", "pcm")
    );

    let server =
        TestServer::start(|_| Reply::json(429, r#"{"error":{"message":"Rate limit"}}"#)).await;
    let mut config = TtsConfig::new("k");
    config.base_url = Some(server.base_url.clone());
    let tts = OpenAiTts::new(config);
    let err = tts.synthesize_speech("hi").await.err().expect("error");
    assert_eq!(err.to_string(), "TTS synthesis failed: 429 Rate limit");
    let err = tts.synthesize_speech("   ").await.err().expect("error");
    assert_eq!(err.to_string(), "Cannot synthesize empty text");
}

#[tokio::test]
async fn openai_provider_is_usable_as_a_dictation_resolver_input() {
    use paseo_speech::provider::SttResolver;
    let provider: Arc<dyn SttProvider> = Arc::new(OpenAiStt::new(SttConfig {
        api_key: "k".into(),
        ..Default::default()
    }));
    let resolver: SttResolver = provider.into();
    assert_eq!(resolver.resolve().unwrap().id(), "openai");
}
