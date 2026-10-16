//! HTTP download and archive extraction helpers shared by the model installers.

use std::path::{Path, PathBuf};

use futures_util::StreamExt;
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;

use crate::error::{Result, SpeechError};

fn cancelled() -> SpeechError {
    SpeechError::msg("The operation was aborted")
}

/// Streams `url` into `target_path` through a `.part` file that is renamed on success and removed
/// on failure, so an interrupted download never leaves a plausible-looking file.
pub async fn download_file_to_path(
    url: &str,
    target_path: &Path,
    cancel: Option<&CancellationToken>,
) -> Result<()> {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let mut tmp_name = target_path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    tmp_name.push(format!(".part-{}-{millis}", std::process::id()));
    let tmp_path: PathBuf = target_path.with_file_name(tmp_name);

    let result = download_inner(url, target_path, &tmp_path, cancel).await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&tmp_path).await;
    }
    result
}

async fn download_inner(
    url: &str,
    target_path: &Path,
    tmp_path: &Path,
    cancel: Option<&CancellationToken>,
) -> Result<()> {
    let send = reqwest::Client::new().get(url).send();
    let response = match cancel {
        Some(token) => tokio::select! {
            _ = token.cancelled() => return Err(cancelled()),
            r = send => r?,
        },
        None => send.await?,
    };
    let status = response.status();
    if !status.is_success() {
        return Err(SpeechError::msg(format!(
            "HTTP {} {} while downloading {url}",
            status.as_u16(),
            status.canonical_reason().unwrap_or("")
        )));
    }
    if let Some(parent) = target_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let mut file = tokio::fs::File::create(tmp_path).await?;
    let mut stream = response.bytes_stream();
    loop {
        let next = match cancel {
            Some(token) => tokio::select! {
                _ = token.cancelled() => return Err(cancelled()),
                n = stream.next() => n,
            },
            None => stream.next().await,
        };
        match next {
            Some(chunk) => file.write_all(&chunk?).await?,
            None => break,
        }
    }
    file.flush().await?;
    drop(file);
    tokio::fs::rename(tmp_path, target_path).await?;
    Ok(())
}

/// Extracts a `.tar`, or a `.tar.bz2` (detected by magic bytes, so a plain tar with a
/// `.tar.bz2` name works too, like `tar xf`).
pub async fn extract_tar_archive(archive_path: &Path, dest_dir: &Path) -> Result<()> {
    tokio::fs::create_dir_all(dest_dir).await?;
    let archive = archive_path.to_path_buf();
    let dest = dest_dir.to_path_buf();
    tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        use std::io::Read;
        let mut file = std::fs::File::open(&archive)?;
        let mut magic = [0u8; 3];
        let read = file.read(&mut magic)?;
        drop(file);
        let file = std::fs::File::open(&archive)?;
        if read == 3 && &magic == b"BZh" {
            tar::Archive::new(bzip2::read::BzDecoder::new(file)).unpack(&dest)
        } else {
            tar::Archive::new(file).unpack(&dest)
        }
    })
    .await
    .map_err(|e| SpeechError::msg(format!("tar exited: {e}")))?
    .map_err(|e| SpeechError::msg(format!("tar exited with an error: {e}")))
}
