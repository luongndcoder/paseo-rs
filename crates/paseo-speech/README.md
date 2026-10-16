# paseo-speech

Speech for the Paseo daemon, ported from `packages/server/src/server/{speech,dictation}` of the TS
daemon (`../paseo-vietnamese-voice`). Standalone crate; wire shapes follow `packages/protocol`.

## What is here

| Module | Port of | Notes |
| --- | --- | --- |
| `dictation` | `dictation/dictation-stream-manager.ts` | `begin/chunk/finish/cancel` (`handle_start/chunk/finish/cancel`, or `handle_inbound` for `dictation_stream_*` JSON). Outbound `DictationOutbound` serialises to the exact protocol messages (`ack`, `finish_accepted`, `partial`, `final`, `error`, `activity_log`). |
| `provider` | `speech-provider.ts`, `turn-detection-provider.ts` | Traits. Sessions are `Arc<dyn SttSession>` plus an event channel instead of an `EventEmitter`. |
| `config` | `speech-config-resolver.ts` and the three `*/config.ts` | Pure: explicit `env` map and the persisted-config subset. |
| `providers::openai` | `providers/openai/*` | `reqwest`; multipart `audio/transcriptions` (language, prompt, `include[]=logprobs`, `response_format=json`) and JSON `audio/speech`. |
| `providers::soniox` | `providers/soniox/*` | `tokio-tungstenite` (rustls), same init handshake, empty-frame flush, 500 ms finalize wait, local fallback with PCM replay. |
| `providers::local` | `providers/local/**` | Catalog + downloader (same URLs, `.downloads` layout, tar / tar.bz2), Nemotron streaming STT, Parakeet offline STT (batch and partial-decoding sessions), Kokoro TTS, Silero turn detection (`vad` state machine + embedded `silero_vad.onnx`). |
| `readiness` | readiness builders in `speech-runtime.ts` | The reconcile/download monitor loop stays in the daemon. |
| `audio`, `resampler` | `speech/audio.ts`, `agent/pcm16-resampler.ts` | Sample-identical resampler. |

Not ported: the Node child-process worker (`worker-client/process/protocol`). It isolated native
crashes from the Node event loop; here engines run in-process and each Nemotron session decodes on
its own thread.

## Local provider and the `sherpa` feature

`providers::local::{nemotron, parakeet, tts, vad}` are written against small engine traits
(`StreamingEngine`, `OfflineEngine`, `TtsEngine`, `SpeechDetector`) so their logic is tested with
fakes. `--features sherpa` adds `providers::local::sherpa`, the adapters over the official
[`sherpa-onnx`](https://crates.io/crates/sherpa-onnx) crate (chosen over `sherpa-rs`: maintained by
k2-fsa and current, safe RAII wrappers over the same C API the Node addon uses, per-stream
`set_option` for Nemotron's language). Its build script downloads a prebuilt static library unless
`SHERPA_ONNX_LIB_DIR` points at a local copy, which is why the feature is off by default.

The Silero asset is the TS repo's `speech/providers/local/sherpa/assets/silero_vad.onnx`
(`assets/silero_vad.onnx`, embedded); `ensure_silero_vad_model` writes it to
`<modelsDir>/silero-vad/silero_vad.onnx`, the same place the TS daemon uses.

## Tests

```text
cargo test                                   # fakes, local HTTP/WebSocket servers, no models, no network
cargo test --features sherpa                 # + real Silero VAD
cargo test --features sherpa --test sherpa_live -- --ignored --test-threads=1
                                             # real Nemotron / Parakeet / Kokoro if installed (never downloads)
```

Model downloads are only exercised against a local HTTP server with tiny fixture archives.
