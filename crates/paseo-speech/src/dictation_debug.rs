//! Dictation audio capture for debugging (port of `agent/dictation-debug.ts` and
//! `agent/recordings-debug.ts`). Writes are synchronous: they only happen with
//! debug capture on, and the chunks are small.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::audio::{infer_audio_extension, sanitize_for_filename};

/// Where debug captures go.
#[derive(Debug, Clone, Default)]
pub enum DictationDebug {
    /// `DICTATION_DEBUG_AUDIO_DIR`, or `./.debug/recordings` when
    /// `PASEO_DICTATION_DEBUG` is truthy; otherwise off. Read at use time.
    #[default]
    FromEnv,
    Disabled,
    Dir(PathBuf),
}

fn is_truthy(value: Option<String>) -> bool {
    matches!(
        value
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "1" | "true" | "yes" | "on"
    )
}

impl DictationDebug {
    pub fn resolve_dir(&self) -> Option<PathBuf> {
        match self {
            Self::Disabled => None,
            Self::Dir(dir) => Some(dir.clone()),
            Self::FromEnv => {
                if let Ok(explicit) = std::env::var("DICTATION_DEBUG_AUDIO_DIR") {
                    let explicit = explicit.trim();
                    if !explicit.is_empty() {
                        return Some(absolute(Path::new(explicit)));
                    }
                }
                if !is_truthy(std::env::var("PASEO_DICTATION_DEBUG").ok()) {
                    return None;
                }
                Some(absolute(Path::new(".debug/recordings")))
            }
        }
    }

    pub fn is_enabled(&self) -> bool {
        match self {
            Self::FromEnv => is_truthy(std::env::var("PASEO_DICTATION_DEBUG").ok()),
            Self::Disabled => false,
            Self::Dir(_) => true,
        }
    }
}

fn absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    }
}

fn timestamp_for_path() -> String {
    jiff::Timestamp::now().to_string().replace([':', '.'], "-")
}

pub struct DebugChunkWriter {
    pub folder: PathBuf,
    created: bool,
}

impl DebugChunkWriter {
    pub fn create(debug: &DictationDebug, session_id: &str, dictation_id: &str) -> Option<Self> {
        let dir = debug.resolve_dir()?;
        let folder = dir
            .join(sanitize_for_filename(Some(session_id), "session"))
            .join(format!(
                "{}_{}",
                timestamp_for_path(),
                sanitize_for_filename(Some(dictation_id), "dictation")
            ));
        Some(Self {
            folder,
            created: false,
        })
    }

    pub fn write_chunk(&mut self, seq: i64, pcm16: &[u8]) -> std::io::Result<()> {
        if !self.created {
            fs::create_dir_all(&self.folder)?;
            self.created = true;
        }
        let mut file = fs::File::create(self.folder.join(format!("chunk_{seq:06}.pcm")))?;
        file.write_all(pcm16)
    }
}

/// Writes the combined recording and returns its path, or `None` when capture is off.
pub fn persist_debug_audio(
    debug: &DictationDebug,
    audio: &[u8],
    session_id: &str,
    dictation_id: &str,
    format: &str,
    chunk_writer_folder: Option<&Path>,
) -> std::io::Result<Option<PathBuf>> {
    let Some(dir) = debug.resolve_dir() else {
        return Ok(None);
    };
    let ext = infer_audio_extension(Some(format));
    let (folder, filename) = match chunk_writer_folder {
        Some(folder) => (folder.to_path_buf(), format!("combined.{ext}")),
        None => (
            dir.join(sanitize_for_filename(Some(session_id), "session")),
            format!(
                "{}_{}.{ext}",
                timestamp_for_path(),
                sanitize_for_filename(Some(dictation_id), "dictation")
            ),
        ),
    };
    fs::create_dir_all(&folder)?;
    let path = folder.join(filename);
    fs::write(&path, audio)?;
    Ok(Some(path))
}
