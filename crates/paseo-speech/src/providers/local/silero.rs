//! The Silero VAD model asset (port of `ensureSileroVadModel` in `silero-vad-provider.ts`).
//!
//! The asset is the same `silero_vad.onnx` the TypeScript daemon ships under
//! `packages/server/src/server/speech/providers/local/sherpa/assets/`. Native code needs a real
//! file path, so the embedded copy is written to `<modelsDir>/silero-vad/silero_vad.onnx` once.

use std::path::{Path, PathBuf};

use crate::error::Result;

pub const SILERO_VAD_DIR: &str = "silero-vad";
pub const SILERO_VAD_FILE: &str = "silero_vad.onnx";

pub static SILERO_VAD_MODEL_BYTES: &[u8] = include_bytes!("../../../assets/silero_vad.onnx");

pub fn silero_vad_model_path(models_dir: &Path) -> PathBuf {
    models_dir.join(SILERO_VAD_DIR).join(SILERO_VAD_FILE)
}

/// Returns the model path, writing the embedded copy if it is missing or empty.
pub async fn ensure_silero_vad_model(models_dir: &Path) -> Result<PathBuf> {
    let dest = silero_vad_model_path(models_dir);
    if let Ok(meta) = tokio::fs::metadata(&dest).await {
        if meta.is_file() && meta.len() > 0 {
            return Ok(dest);
        }
    }
    tokio::fs::create_dir_all(dest.parent().expect("has parent")).await?;
    // Write beside the target and rename, so a crash never leaves a truncated model in place.
    let tmp = dest.with_extension(format!("onnx.tmp-{}", std::process::id()));
    tokio::fs::write(&tmp, SILERO_VAD_MODEL_BYTES).await?;
    tokio::fs::rename(&tmp, &dest).await?;
    tracing::info!(dest = %dest.display(), "Copied Silero VAD model to models directory");
    Ok(dest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_asset_is_an_onnx_model() {
        assert!(SILERO_VAD_MODEL_BYTES.len() > 100_000);
        // ONNX protobuf starts with the `ir_version` field (tag 0x08).
        assert_eq!(SILERO_VAD_MODEL_BYTES[0], 0x08);
    }

    /// The embedded copy is byte-identical to the asset the TS daemon ships (when that checkout is
    /// next to this repo).
    #[test]
    fn embedded_asset_matches_the_typescript_repo_copy() {
        let ts = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(
            "../../../paseo-vietnamese-voice/packages/server/src/server/speech/providers/local/sherpa/assets/silero_vad.onnx",
        );
        match std::fs::read(&ts) {
            Ok(bytes) => assert!(
                bytes == SILERO_VAD_MODEL_BYTES,
                "assets/silero_vad.onnx drifted from {ts:?}"
            ),
            Err(_) => eprintln!("skipping: TS checkout not found at {ts:?}"),
        }
    }

    #[tokio::test]
    async fn ensure_copies_once_and_repairs_empty_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = ensure_silero_vad_model(dir.path()).await.unwrap();
        assert_eq!(path, dir.path().join("silero-vad").join("silero_vad.onnx"));
        assert_eq!(std::fs::read(&path).unwrap(), SILERO_VAD_MODEL_BYTES);

        // an existing non-empty file is left alone
        std::fs::write(&path, b"custom").unwrap();
        ensure_silero_vad_model(dir.path()).await.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"custom");

        // an empty file is replaced
        std::fs::write(&path, b"").unwrap();
        ensure_silero_vad_model(dir.path()).await.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), SILERO_VAD_MODEL_BYTES);
    }
}
