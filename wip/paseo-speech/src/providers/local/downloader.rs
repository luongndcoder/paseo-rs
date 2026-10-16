//! Catalog model installer (port of `sherpa/model-downloader.ts`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use tokio_util::sync::CancellationToken;

use crate::error::{Result, SpeechError};

use super::catalog::{model_spec, ModelSpec, NEMOTRON_MODEL_ID};
use super::download::{download_file_to_path, extract_tar_archive};
use super::vietnamese_model::{ensure_vietnamese_sherpa_model, resolve_installed_nemotron_model};

fn spec_for(model_id: &str) -> Result<&'static ModelSpec> {
    model_spec(model_id)
        .ok_or_else(|| SpeechError::msg(format!("Unknown local speech model id: {model_id}")))
}

/// Where `model_id` lives (or will live) under `models_dir`.
pub fn model_dir(models_dir: &Path, model_id: &str) -> Result<PathBuf> {
    if model_id == NEMOTRON_MODEL_ID {
        if let Some(installed) = resolve_installed_nemotron_model(Some(models_dir)) {
            return Ok(installed.model_dir);
        }
    }
    Ok(models_dir.join(spec_for(model_id)?.extracted_dir))
}

async fn has_required_files(model_dir: &Path, required: &[&str]) -> bool {
    for rel in required {
        let ok = match tokio::fs::metadata(model_dir.join(rel)).await {
            Ok(m) => m.is_dir() || (m.is_file() && m.len() > 0),
            Err(_) => false,
        };
        if !ok {
            return false;
        }
    }
    true
}

async fn is_non_empty_file(path: &Path) -> bool {
    tokio::fs::metadata(path)
        .await
        .map(|m| m.is_file() && m.len() > 0)
        .unwrap_or(false)
}

struct ModelPaths {
    model_dir: PathBuf,
    archive_path: PathBuf,
    extraction_dir: PathBuf,
}

fn archive_file_name(url: &str) -> String {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    path.rsplit('/').next().unwrap_or(path).to_string()
}

fn model_paths(models_dir: &Path, spec: &ModelSpec, archive_url: &str) -> ModelPaths {
    let downloads = models_dir.join(".downloads");
    ModelPaths {
        model_dir: models_dir.join(spec.extracted_dir),
        archive_path: downloads.join(archive_file_name(archive_url)),
        extraction_dir: downloads.join(format!("{}.extracting", spec.extracted_dir)),
    }
}

/// The archive is removed only as its model moves into place, so a model whose archive is still
/// in `.downloads` did not finish installing and may hold truncated files.
pub async fn is_model_installed(models_dir: &Path, model_id: &str) -> Result<bool> {
    if model_id == NEMOTRON_MODEL_ID {
        return Ok(resolve_installed_nemotron_model(Some(models_dir)).is_some());
    }
    let spec = spec_for(model_id)?;
    let paths = model_paths(models_dir, spec, spec.archive_url);
    Ok(!is_non_empty_file(&paths.archive_path).await
        && has_required_files(&paths.model_dir, spec.required_files).await)
}

#[derive(Default)]
pub struct EnsureModelOptions<'a> {
    /// Overrides the catalog archive URL (tests serve fixtures from a local server).
    pub archive_url: Option<&'a str>,
    pub cancel: Option<&'a CancellationToken>,
}

pub async fn ensure_model(
    models_dir: &Path,
    model_id: &str,
    options: EnsureModelOptions<'_>,
) -> Result<PathBuf> {
    if model_id == NEMOTRON_MODEL_ID {
        return Ok(ensure_vietnamese_sherpa_model(models_dir, options.cancel)
            .await?
            .model_dir);
    }
    let spec = spec_for(model_id)?;
    let url = options.archive_url.unwrap_or(spec.archive_url);
    let paths = model_paths(models_dir, spec, url);
    if is_model_installed(models_dir, model_id).await? {
        return Ok(paths.model_dir);
    }

    tracing::info!(model_id, models_dir = %models_dir.display(), "Starting model download");
    let result: Result<()> = async {
        if !is_non_empty_file(&paths.archive_path).await {
            download_file_to_path(url, &paths.archive_path, options.cancel).await?;
        }
        tracing::info!(model_id, archive = %paths.archive_path.display(), "Extracting model archive");
        // Extract beside the archive and move into place once complete, so an interrupted
        // extraction never leaves partial files at the model dir.
        let _ = tokio::fs::remove_dir_all(&paths.extraction_dir).await;
        let installed: Result<()> = async {
            extract_tar_archive(&paths.archive_path, &paths.extraction_dir).await?;
            let extracted = paths.extraction_dir.join(spec.extracted_dir);
            if !has_required_files(&extracted, spec.required_files).await {
                return Err(SpeechError::msg(format!(
                    "Downloaded and extracted {}, but required files are missing.",
                    archive_file_name(url)
                )));
            }
            // A retained archive marks the model as not installed, so remove it before the model
            // is moved into place. If removal fails, nothing at the model dir looks installed.
            let _ = tokio::fs::remove_dir_all(&paths.model_dir).await;
            remove_file_with_retries(&paths.archive_path).await?;
            tokio::fs::rename(&extracted, &paths.model_dir).await?;
            Ok(())
        }
        .await;
        let _ = tokio::fs::remove_dir_all(&paths.extraction_dir).await;
        installed
    }
    .await;

    match result {
        Ok(()) => {
            tracing::info!(model_dir = %paths.model_dir.display(), "Model download completed");
            Ok(paths.model_dir)
        }
        Err(err) => {
            tracing::error!(error = %err, "Model download failed");
            Err(err)
        }
    }
}

async fn remove_file_with_retries(path: &Path) -> Result<()> {
    let mut last = None;
    for _ in 0..4 {
        match tokio::fs::remove_file(path).await {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => {
                last = Some(e);
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }
    Err(last
        .map(SpeechError::from)
        .unwrap_or_else(|| SpeechError::msg("failed to remove archive")))
}

/// Installs every listed model (duplicates once), concurrently.
pub async fn ensure_models(
    models_dir: &Path,
    model_ids: &[&str],
    cancel: Option<&CancellationToken>,
) -> Result<HashMap<String, PathBuf>> {
    let mut unique: Vec<&str> = Vec::new();
    for id in model_ids {
        if !unique.contains(id) {
            unique.push(id);
        }
    }
    let installs = unique.iter().map(|id| async move {
        let dir = ensure_model(
            models_dir,
            id,
            EnsureModelOptions {
                archive_url: None,
                cancel,
            },
        )
        .await?;
        Ok::<_, SpeechError>((id.to_string(), dir))
    });
    Ok(futures_util::future::try_join_all(installs)
        .await?
        .into_iter()
        .collect())
}

/// The ids among `model_ids` that are not installed.
pub async fn list_missing_models(models_dir: &Path, model_ids: &[&str]) -> Result<Vec<String>> {
    let mut missing = Vec::new();
    for id in model_ids {
        if !is_model_installed(models_dir, id).await? {
            missing.push(id.to_string());
        }
    }
    Ok(missing)
}
