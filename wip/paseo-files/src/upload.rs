//! Chunked file upload store.
//!
//! Uploads are keyed by `(source, requestId)`, where `source` identifies the
//! connection that started them. Bytes land in
//! `<paseoHome>/uploads/upload_<uuid>/<sanitized name>`. The store is
//! synchronous and single-owner, so frames apply in arrival order; wrap it in a
//! mutex when several tasks share it. The stale timeout is driven by the caller
//! through [`UploadStore::expire_stale`] instead of a timer thread.

use crate::binary_frames::FileTransferFrame;
use crate::path_utils::{basename, extname};
use crate::wire::{
    FileUploadRequest, FileUploadResponse, FileUploadResponsePayload, UploadedFileAttachment,
};
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub type SourceId = u64;
pub const DEFAULT_SOURCE: SourceId = 0;
const DEFAULT_STALE_UPLOAD_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// Most file systems cap a single file name at 255 bytes.
const MAX_FILE_NAME_BYTES: usize = 255;

struct PendingUpload {
    request_id: String,
    id: String,
    file_name: String,
    mime_type: String,
    size: u64,
    path: PathBuf,
    received_bytes: u64,
    started: bool,
    last_activity: Instant,
}

pub struct UploadStore {
    paseo_home: PathBuf,
    stale_timeout: Duration,
    pending: HashMap<(SourceId, String), PendingUpload>,
}

impl UploadStore {
    pub fn new(paseo_home: impl Into<PathBuf>) -> Self {
        Self {
            paseo_home: paseo_home.into(),
            stale_timeout: DEFAULT_STALE_UPLOAD_TIMEOUT,
            pending: HashMap::new(),
        }
    }

    pub fn with_stale_timeout(mut self, timeout: Duration) -> Self {
        self.stale_timeout = timeout;
        self
    }

    pub fn begin_upload(&mut self, request: &FileUploadRequest, source: SourceId) {
        self.begin_upload_at(request, source, Instant::now());
    }

    /// Registers an upload. A pending upload with the same source and request id
    /// is cancelled and its partial data removed.
    pub fn begin_upload_at(&mut self, request: &FileUploadRequest, source: SourceId, now: Instant) {
        self.cancel(source, &request.request_id);
        let file_name = sanitize_file_name(&request.file_name);
        let id = format!("upload_{}", uuid::Uuid::new_v4());
        let path = self.upload_dir(&id).join(&file_name);
        self.pending.insert(
            (source, request.request_id.clone()),
            PendingUpload {
                request_id: request.request_id.clone(),
                id,
                file_name,
                mime_type: request.mime_type.clone(),
                size: request.size,
                path,
                received_bytes: 0,
                started: false,
                last_activity: now,
            },
        );
    }

    pub fn receive_frame(
        &mut self,
        frame: &FileTransferFrame,
        source: SourceId,
    ) -> Option<FileUploadResponse> {
        self.receive_frame_at(frame, source, Instant::now())
    }

    /// Applies a frame. Returns a response when the upload finished or failed;
    /// frames for unknown uploads are ignored.
    pub fn receive_frame_at(
        &mut self,
        frame: &FileTransferFrame,
        source: SourceId,
        now: Instant,
    ) -> Option<FileUploadResponse> {
        let key = (source, frame.request_id().to_string());
        let upload = self.pending.get_mut(&key)?;
        upload.last_activity = now;
        let outcome = match frame {
            FileTransferFrame::Begin { .. } => {
                start_writing(&self.paseo_home, upload).map(|()| None)
            }
            FileTransferFrame::Chunk { payload, .. } => write_chunk(upload, payload).map(|()| None),
            FileTransferFrame::End { .. } => Ok(Some(())),
        };
        match outcome {
            Ok(None) => None,
            Ok(Some(())) => {
                let upload = self.pending.remove(&key)?;
                if upload.received_bytes != upload.size {
                    self.remove_dir(&upload);
                    return Some(response(
                        &upload,
                        Some(format!(
                            "Upload size mismatch: expected {}, received {}.",
                            upload.size, upload.received_bytes
                        )),
                    ));
                }
                Some(response(&upload, None))
            }
            Err(message) => {
                let upload = self.pending.remove(&key)?;
                self.remove_dir(&upload);
                Some(response(&upload, Some(message)))
            }
        }
    }

    /// Cancels one upload and removes its partial data.
    pub fn cancel(&mut self, source: SourceId, request_id: &str) {
        if let Some(upload) = self.pending.remove(&(source, request_id.to_string())) {
            self.remove_dir(&upload);
        }
    }

    /// Cancels every pending upload of a connection, for example on disconnect.
    pub fn cancel_source(&mut self, source: SourceId) {
        let keys: Vec<_> = self
            .pending
            .keys()
            .filter(|(s, _)| *s == source)
            .cloned()
            .collect();
        for (s, request_id) in keys {
            self.cancel(s, &request_id);
        }
    }

    pub fn expire_stale(&mut self) -> Vec<(SourceId, String)> {
        self.expire_stale_at(Instant::now())
    }

    /// Cancels uploads idle for at least the stale timeout; returns their keys.
    pub fn expire_stale_at(&mut self, now: Instant) -> Vec<(SourceId, String)> {
        let timeout = self.stale_timeout;
        let stale: Vec<_> = self
            .pending
            .iter()
            .filter(|(_, u)| now.saturating_duration_since(u.last_activity) >= timeout)
            .map(|(k, _)| k.clone())
            .collect();
        for (source, request_id) in &stale {
            self.cancel(*source, request_id);
        }
        stale
    }

    pub fn is_pending(&self, source: SourceId, request_id: &str) -> bool {
        self.pending.contains_key(&(source, request_id.to_string()))
    }

    fn upload_dir(&self, id: &str) -> PathBuf {
        self.paseo_home.join("uploads").join(id)
    }

    fn remove_dir(&self, upload: &PendingUpload) {
        let _ = fs::remove_dir_all(self.upload_dir(&upload.id));
    }
}

fn start_writing(paseo_home: &Path, upload: &mut PendingUpload) -> Result<(), String> {
    fs::create_dir_all(paseo_home.join("uploads").join(&upload.id)).map_err(|e| e.to_string())?;
    fs::write(&upload.path, []).map_err(|e| e.to_string())?;
    upload.started = true;
    Ok(())
}

fn write_chunk(upload: &mut PendingUpload, bytes: &[u8]) -> Result<(), String> {
    if !upload.started {
        return Err("Upload chunks arrived before file begin.".to_string());
    }
    let next = upload.received_bytes + bytes.len() as u64;
    if next > upload.size {
        return Err(format!(
            "Upload exceeded declared size: expected {}, received {}.",
            upload.size, next
        ));
    }
    let mut file = OpenOptions::new()
        .append(true)
        .open(&upload.path)
        .map_err(|e| e.to_string())?;
    file.write_all(bytes).map_err(|e| e.to_string())?;
    upload.received_bytes = next;
    Ok(())
}

fn response(upload: &PendingUpload, error: Option<String>) -> FileUploadResponse {
    FileUploadResponse::new(FileUploadResponsePayload {
        request_id: upload.request_id.clone(),
        file: error.is_none().then(|| UploadedFileAttachment {
            kind: "uploaded_file",
            id: upload.id.clone(),
            file_name: upload.file_name.clone(),
            mime_type: upload.mime_type.clone(),
            size: upload.size,
            path: upload.path.to_string_lossy().into_owned(),
        }),
        error,
    })
}

/// Keeps the client's file name, replacing only what cannot appear in a single
/// file name on Linux, macOS or Windows.
pub fn sanitize_file_name(value: &str) -> String {
    let replaced: String = basename(value)
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                '_'
            } else {
                c
            }
        })
        .collect();
    let name = replaced.trim();
    let name = if name.is_empty() || name == "." || name == ".." {
        "upload"
    } else {
        name
    };
    fit_file_name_length(name)
}

fn fit_file_name_length(name: &str) -> String {
    if name.len() <= MAX_FILE_NAME_BYTES {
        return name.to_string();
    }
    let extension = extname(name);
    let kept_extension = if extension.len() < MAX_FILE_NAME_BYTES {
        extension
    } else {
        String::new()
    };
    let stem_source = &name[..name.len() - kept_extension.len()];
    let mut stem = String::new();
    for c in stem_source.chars() {
        if stem.len() + c.len_utf8() + kept_extension.len() > MAX_FILE_NAME_BYTES {
            break;
        }
        stem.push(c);
    }
    stem + &kept_extension
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_cases() {
        assert_eq!(sanitize_file_name("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_file_name("a<b>:\"c|?*.txt"), "a_b___c___.txt");
        assert_eq!(sanitize_file_name("line\nbreak.txt"), "line_break.txt");
        assert_eq!(sanitize_file_name("dir\\name.txt"), "dir_name.txt");
        assert_eq!(sanitize_file_name("  "), "upload");
        assert_eq!(sanitize_file_name(".."), "upload");
        assert_eq!(
            sanitize_file_name("2026年9月绩效计划表.xlsx"),
            "2026年9月绩效计划表.xlsx"
        );
    }

    #[test]
    fn long_names_keep_extension_within_255_bytes() {
        let name = format!("{}.xlsx", "绩".repeat(100));
        let fitted = sanitize_file_name(&name);
        assert_eq!(fitted, format!("{}.xlsx", "绩".repeat(83)));
        assert!(fitted.len() <= 255);
    }
}
