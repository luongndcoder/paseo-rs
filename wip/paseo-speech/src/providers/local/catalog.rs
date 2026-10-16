//! Local model catalog (port of `sherpa/model-catalog.ts`). URLs and file lists are the
//! same as the TypeScript daemon so both share one models directory.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelKind {
    /// STT model (the TS catalog calls every STT entry `stt-offline`, Nemotron included).
    Stt,
    Tts,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefaultRole {
    Stt,
    Tts,
}

#[derive(Debug, Clone, Copy)]
pub struct ModelSpec {
    pub id: &'static str,
    pub kind: ModelKind,
    pub archive_url: &'static str,
    pub extracted_dir: &'static str,
    pub required_files: &'static [&'static str],
    pub description: &'static str,
    pub default_for: Option<DefaultRole>,
}

pub const NEMOTRON_MODEL_ID: &str = "nemotron-3.5-560ms-int8";

const TRANSDUCER_FILES: &[&str] = &[
    "encoder.int8.onnx",
    "decoder.int8.onnx",
    "joiner.int8.onnx",
    "tokens.txt",
];

pub static CATALOG: &[ModelSpec] = &[
    ModelSpec {
        id: "parakeet-tdt-0.6b-v2-int8",
        kind: ModelKind::Stt,
        archive_url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-nemo-parakeet-tdt-0.6b-v2-int8.tar.bz2",
        extracted_dir: "sherpa-onnx-nemo-parakeet-tdt-0.6b-v2-int8",
        required_files: TRANSDUCER_FILES,
        description: "NVIDIA Parakeet TDT v2 (offline NeMo transducer, English).",
        default_for: Some(DefaultRole::Stt),
    },
    ModelSpec {
        id: "parakeet-tdt-0.6b-v3-int8",
        kind: ModelKind::Stt,
        archive_url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8.tar.bz2",
        extracted_dir: "sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8",
        required_files: TRANSDUCER_FILES,
        description: "NVIDIA Parakeet TDT v3 (offline NeMo transducer, 25 European languages, auto-detected).",
        default_for: None,
    },
    ModelSpec {
        id: NEMOTRON_MODEL_ID,
        kind: ModelKind::Stt,
        archive_url: "https://huggingface.co/csukuangfj2/sherpa-onnx-nemotron-3.5-asr-streaming-0.6b-560ms-int8-2026-06-11/resolve/main/tokens.txt",
        extracted_dir: NEMOTRON_MODEL_ID,
        required_files: TRANSDUCER_FILES,
        description: "NVIDIA Nemotron 3.5 ASR Streaming 0.6B INT8 (560ms cache-aware transducer, Vietnamese & multilingual).",
        default_for: None,
    },
    ModelSpec {
        id: "kokoro-en-v0_19",
        kind: ModelKind::Tts,
        archive_url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/tts-models/kokoro-en-v0_19.tar.bz2",
        extracted_dir: "kokoro-en-v0_19",
        required_files: &["model.onnx", "voices.bin", "tokens.txt", "espeak-ng-data"],
        description: "Kokoro TTS (higher quality; larger).",
        default_for: Some(DefaultRole::Tts),
    },
];

pub fn list_models() -> &'static [ModelSpec] {
    CATALOG
}

pub fn model_spec(id: &str) -> Option<&'static ModelSpec> {
    CATALOG.iter().find(|m| m.id == id)
}

pub fn default_stt_model() -> &'static str {
    CATALOG
        .iter()
        .find(|m| m.default_for == Some(DefaultRole::Stt))
        .expect("default stt")
        .id
}

pub fn default_tts_model() -> &'static str {
    CATALOG
        .iter()
        .find(|m| m.default_for == Some(DefaultRole::Tts))
        .expect("default tts")
        .id
}

fn parse_of_kind(value: &str, kind: ModelKind) -> Option<&'static str> {
    let normalized = value.trim().to_ascii_lowercase();
    CATALOG
        .iter()
        .find(|m| m.kind == kind && m.id == normalized)
        .map(|m| m.id)
}

/// `LocalSttModelIdSchema`: trimmed, lower-cased, must be an STT catalog id.
pub fn parse_stt_model_id(value: &str) -> Option<&'static str> {
    parse_of_kind(value, ModelKind::Stt)
}

pub fn parse_tts_model_id(value: &str) -> Option<&'static str> {
    parse_of_kind(value, ModelKind::Tts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_parsing() {
        assert_eq!(default_stt_model(), "parakeet-tdt-0.6b-v2-int8");
        assert_eq!(default_tts_model(), "kokoro-en-v0_19");
        assert_eq!(
            parse_stt_model_id("  Parakeet-TDT-0.6B-V3-INT8 "),
            Some("parakeet-tdt-0.6b-v3-int8")
        );
        assert_eq!(
            parse_stt_model_id("nemotron-3.5-560ms-int8"),
            Some(NEMOTRON_MODEL_ID)
        );
        assert_eq!(parse_stt_model_id("kokoro-en-v0_19"), None);
        assert_eq!(
            parse_tts_model_id("kokoro-en-v0_19"),
            Some("kokoro-en-v0_19")
        );
        assert_eq!(parse_tts_model_id("nope"), None);
    }

    #[test]
    fn urls_match_the_typescript_catalog() {
        let v2 = model_spec("parakeet-tdt-0.6b-v2-int8").unwrap();
        assert!(v2
            .archive_url
            .ends_with("/asr-models/sherpa-onnx-nemo-parakeet-tdt-0.6b-v2-int8.tar.bz2"));
        let tts = model_spec("kokoro-en-v0_19").unwrap();
        assert_eq!(
            tts.archive_url,
            "https://github.com/k2-fsa/sherpa-onnx/releases/download/tts-models/kokoro-en-v0_19.tar.bz2"
        );
        assert!(tts.required_files.contains(&"espeak-ng-data"));
    }
}
