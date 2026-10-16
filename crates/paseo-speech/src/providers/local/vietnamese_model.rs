//! Nemotron 3.5 ASR (Vietnamese + multilingual) model files (port of `sherpa/vietnamese-model.ts`).
//!
//! The model is fetched file by file from Hugging Face. A copy already installed by another tool
//! (`~/.voicescribe/models/...`) is reused before anything is downloaded.

use std::path::{Path, PathBuf};

use tokio_util::sync::CancellationToken;

use crate::error::{Result, SpeechError};

use super::catalog::NEMOTRON_MODEL_ID;
use super::download::download_file_to_path;

pub const VIETNAMESE_SHERPA_MODEL_DIR_NAME: &str = NEMOTRON_MODEL_ID;

pub const HF_NEMOTRON_REPO: &str =
    "csukuangfj2/sherpa-onnx-nemotron-3.5-asr-streaming-0.6b-560ms-int8-2026-06-11";
pub const HF_NEMOTRON_BASE_URL: &str =
    "https://huggingface.co/csukuangfj2/sherpa-onnx-nemotron-3.5-asr-streaming-0.6b-560ms-int8-2026-06-11/resolve/main";

const REQUIRED_NEMOTRON_FILES: [&str; 4] = [
    "tokens.txt",
    "decoder.int8.onnx",
    "joiner.int8.onnx",
    "encoder.int8.onnx",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VietnameseSherpaModelFiles {
    pub model_dir: PathBuf,
    pub encoder_path: PathBuf,
    pub decoder_path: PathBuf,
    pub joiner_path: PathBuf,
    pub tokens_path: PathBuf,
}

/// Where model lookups read environment and the home directory from (injectable for tests).
pub trait ModelEnv {
    fn var(&self, key: &str) -> Option<String>;
    fn home_dir(&self) -> Option<PathBuf>;
}

pub struct ProcessModelEnv;

impl ModelEnv for ProcessModelEnv {
    fn var(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }
    fn home_dir(&self) -> Option<PathBuf> {
        std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)
    }
}

fn absolutize(path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(&path))
            .unwrap_or(path)
    }
}

/// Candidate directories in lookup order, deduplicated.
pub fn candidate_nemotron_dirs(env: &dyn ModelEnv, models_dir: Option<&Path>) -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    for key in [
        "PASEO_NEMOTRON_MODEL_DIR",
        "VOICESCRIBE_NEMOTRON_DIR",
        "PASEO_VIETNAMESE_MODEL_DIR",
    ] {
        if let Some(value) = env.var(key) {
            let value = value.trim();
            if !value.is_empty() {
                candidates.push(PathBuf::from(value));
            }
        }
    }
    let home = env.home_dir();
    if let Some(home) = &home {
        candidates.push(
            home.join(".voicescribe")
                .join("models")
                .join(NEMOTRON_MODEL_ID),
        );
    }
    if let Some(models_dir) = models_dir {
        candidates.push(models_dir.join(NEMOTRON_MODEL_ID));
    }
    if let Some(home) = &home {
        candidates.push(
            home.join(".paseo")
                .join("models")
                .join("local-speech")
                .join(NEMOTRON_MODEL_ID),
        );
    }
    let mut out: Vec<PathBuf> = Vec::new();
    for candidate in candidates.into_iter().map(absolutize) {
        if !out.contains(&candidate) {
            out.push(candidate);
        }
    }
    out
}

fn is_non_empty_file(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.len() > 0)
        .unwrap_or(false)
}

fn resolve_from_dir(dir: &Path) -> Option<VietnameseSherpaModelFiles> {
    let encoder_path = dir.join("encoder.int8.onnx");
    let decoder_path = dir.join("decoder.int8.onnx");
    let joiner_path = dir.join("joiner.int8.onnx");
    let tokens_path = dir.join("tokens.txt");
    if [&encoder_path, &decoder_path, &joiner_path, &tokens_path]
        .iter()
        .all(|p| is_non_empty_file(p))
    {
        Some(VietnameseSherpaModelFiles {
            model_dir: dir.to_path_buf(),
            encoder_path,
            decoder_path,
            joiner_path,
            tokens_path,
        })
    } else {
        None
    }
}

pub fn resolve_installed_nemotron_model_with(
    env: &dyn ModelEnv,
    models_dir: Option<&Path>,
) -> Option<VietnameseSherpaModelFiles> {
    candidate_nemotron_dirs(env, models_dir)
        .iter()
        .find_map(|dir| resolve_from_dir(dir))
}

pub fn resolve_installed_nemotron_model(
    models_dir: Option<&Path>,
) -> Option<VietnameseSherpaModelFiles> {
    resolve_installed_nemotron_model_with(&ProcessModelEnv, models_dir)
}

pub fn is_nemotron_model_installed(models_dir: Option<&Path>) -> bool {
    resolve_installed_nemotron_model(models_dir).is_some()
}

pub fn vietnamese_dictation_model_directory(models_dir: &Path) -> PathBuf {
    resolve_installed_nemotron_model(Some(models_dir))
        .map(|m| m.model_dir)
        .unwrap_or_else(|| models_dir.join(NEMOTRON_MODEL_ID))
}

/// Downloads any missing Nemotron file into `models_dir/nemotron-3.5-560ms-int8`.
/// `base_url` is `HF_NEMOTRON_BASE_URL` in production; `env` supplies lookup overrides and `$HOME`.
pub async fn ensure_vietnamese_sherpa_model_with(
    env: &dyn ModelEnv,
    base_url: &str,
    models_dir: &Path,
    cancel: Option<&CancellationToken>,
) -> Result<VietnameseSherpaModelFiles> {
    if let Some(existing) = resolve_installed_nemotron_model_with(env, Some(models_dir)) {
        tracing::info!(model_dir = %existing.model_dir.display(), "Resolved installed NVIDIA Nemotron 3.5 ASR model on disk");
        return Ok(existing);
    }

    let target_dir = absolutize(models_dir.to_path_buf()).join(NEMOTRON_MODEL_ID);
    tokio::fs::create_dir_all(&target_dir).await?;
    tracing::info!(target_dir = %target_dir.display(), repo = HF_NEMOTRON_REPO, "Downloading NVIDIA Nemotron 3.5 ASR model files from HuggingFace");

    for file_name in REQUIRED_NEMOTRON_FILES {
        let dest = target_dir.join(file_name);
        if is_non_empty_file(&dest) {
            continue;
        }
        let url = format!("{}/{}", base_url.trim_end_matches('/'), file_name);
        tracing::info!(file_name, %url, "Downloading Nemotron 3.5 model file");
        download_file_to_path(&url, &dest, cancel).await?;
    }

    let resolved = resolve_from_dir(&target_dir).ok_or_else(|| {
        SpeechError::msg(format!(
            "Nemotron 3.5 model download completed at {}, but required ONNX files were not found",
            target_dir.display()
        ))
    })?;
    tracing::info!(model_dir = %resolved.model_dir.display(), "Nemotron 3.5 ASR model ready");
    Ok(resolved)
}

pub async fn ensure_vietnamese_sherpa_model(
    models_dir: &Path,
    cancel: Option<&CancellationToken>,
) -> Result<VietnameseSherpaModelFiles> {
    ensure_vietnamese_sherpa_model_with(&ProcessModelEnv, HF_NEMOTRON_BASE_URL, models_dir, cancel)
        .await
}

pub async fn ensure_vietnamese_dictation_model(
    models_dir: &Path,
    cancel: Option<&CancellationToken>,
) -> Result<PathBuf> {
    Ok(ensure_vietnamese_sherpa_model(models_dir, cancel)
        .await?
        .model_dir)
}
