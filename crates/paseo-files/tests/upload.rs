//! Port of `file-upload/index.test.ts`. Timer-driven cases use an explicit clock.

use paseo_files::binary_frames::*;
use paseo_files::upload::*;
use paseo_files::wire::*;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn request(name: &str, size: u64, id: &str) -> FileUploadRequest {
    FileUploadRequest {
        file_name: name.into(),
        mime_type: "text/plain".into(),
        size,
        modified_at: "2026-05-02T00:00:00.000Z".into(),
        request_id: id.into(),
    }
}

fn begins(id: &str) -> FileTransferFrame {
    FileTransferFrame::Begin {
        request_id: id.into(),
        metadata: FileBeginMetadata {
            mime: "text/plain".into(),
            size: 11,
            encoding: FileEncoding::Binary,
            modified_at: "2026-05-02T00:00:00.000Z".into(),
            revision: None,
            file_name: Some("notes.txt".into()),
        },
    }
}

fn chunk(id: &str, text: &str) -> FileTransferFrame {
    FileTransferFrame::Chunk {
        request_id: id.into(),
        payload: text.as_bytes().to_vec(),
    }
}

fn ends(id: &str) -> FileTransferFrame {
    FileTransferFrame::End {
        request_id: id.into(),
    }
}

fn uploaded_path(home: &Path, name: &str) -> PathBuf {
    let root = home.join("uploads");
    fs::read_dir(root)
        .unwrap()
        .flatten()
        .map(|e| e.path().join(name))
        .find(|p| p.exists())
        .unwrap_or_else(|| panic!("upload file {name} missing"))
}

fn upload_named(store: &mut UploadStore, name: &str, id: &str) -> UploadedFileAttachment {
    store.begin_upload(&request(name, 11, id), DEFAULT_SOURCE);
    store.receive_frame(&begins(id), DEFAULT_SOURCE);
    store.receive_frame(&chunk(id, "hello world"), DEFAULT_SOURCE);
    let response = store.receive_frame(&ends(id), DEFAULT_SOURCE).unwrap();
    assert_eq!(response.payload.error, None);
    response.payload.file.unwrap()
}

#[test]
fn stores_chunked_upload_bytes_and_returns_an_uploaded_file_attachment() {
    let home = tempfile::tempdir().unwrap();
    let mut store = UploadStore::new(home.path());
    store.begin_upload(&request("notes.txt", 11, "req-upload"), DEFAULT_SOURCE);
    assert_eq!(
        store.receive_frame(&begins("req-upload"), DEFAULT_SOURCE),
        None
    );
    assert_eq!(
        store.receive_frame(&chunk("req-upload", "hello"), DEFAULT_SOURCE),
        None
    );
    assert_eq!(
        store.receive_frame(&chunk("req-upload", " world"), DEFAULT_SOURCE),
        None
    );
    let path = uploaded_path(home.path(), "notes.txt");
    let response = store
        .receive_frame(&ends("req-upload"), DEFAULT_SOURCE)
        .unwrap();
    let file = response.payload.file.clone().unwrap();
    assert!(file.id.starts_with("upload_"));
    assert_eq!(
        serde_json::to_value(&response).unwrap(),
        serde_json::json!({
            "type": "file.upload.response",
            "payload": {
                "requestId": "req-upload",
                "file": {"type": "uploaded_file", "id": file.id, "fileName": "notes.txt", "mimeType": "text/plain", "size": 11, "path": path.to_string_lossy()},
                "error": null
            }
        })
    );
    assert_eq!(fs::read_to_string(path).unwrap(), "hello world");
}

#[test]
fn keeps_the_original_file_name_for_non_ascii_and_punctuated_names() {
    let home = tempfile::tempdir().unwrap();
    let mut store = UploadStore::new(home.path());
    for (i, name) in [
        "2026年9月绩效计划表.xlsx",
        "테스트 파일 (1).xlsx",
        "résumé [final] & notes, v2.pdf",
        "cafe\u{301} हिंदी.txt",
    ]
    .iter()
    .enumerate()
    {
        let file = upload_named(&mut store, name, &format!("req-named-{i}"));
        assert_eq!(file.file_name, *name);
        assert_eq!(
            Path::new(&file.path).file_name().unwrap().to_string_lossy(),
            *name
        );
        assert_eq!(fs::read_to_string(&file.path).unwrap(), "hello world");
    }
}

#[test]
fn replaces_path_separators_control_characters_and_characters_windows_rejects() {
    let home = tempfile::tempdir().unwrap();
    let mut store = UploadStore::new(home.path());
    assert_eq!(
        upload_named(&mut store, "../../etc/passwd", "a").file_name,
        "passwd"
    );
    let backslashed = upload_named(&mut store, "dir\\name.txt", "b").file_name;
    assert!(!backslashed.contains('\\') && backslashed.ends_with("name.txt"));
    assert_eq!(
        upload_named(&mut store, "a<b>:\"c|?*.txt", "c").file_name,
        "a_b___c___.txt"
    );
    assert_eq!(
        upload_named(&mut store, "line\nbreak.txt", "d").file_name,
        "line_break.txt"
    );
}

#[test]
fn shortens_a_long_non_ascii_name_to_the_file_system_limit_and_keeps_its_extension() {
    let home = tempfile::tempdir().unwrap();
    let mut store = UploadStore::new(home.path());
    let file = upload_named(&mut store, &format!("{}.xlsx", "绩".repeat(100)), "long");
    assert_eq!(file.file_name, format!("{}.xlsx", "绩".repeat(83)));
    assert!(file.file_name.len() <= 255);
    assert_eq!(fs::read_to_string(&file.path).unwrap(), "hello world");
}

#[test]
fn rejects_chunks_beyond_the_declared_size_and_removes_the_partial_file() {
    let home = tempfile::tempdir().unwrap();
    let mut store = UploadStore::new(home.path());
    store.begin_upload(&request("notes.txt", 5, "req-overflow"), DEFAULT_SOURCE);
    assert_eq!(
        store.receive_frame(&begins("req-overflow"), DEFAULT_SOURCE),
        None
    );
    let path = uploaded_path(home.path(), "notes.txt");
    let upload_dir = path.parent().unwrap().to_path_buf();
    let response = store
        .receive_frame(&chunk("req-overflow", "hello!"), DEFAULT_SOURCE)
        .unwrap();
    assert_eq!(
        serde_json::to_value(&response).unwrap(),
        serde_json::json!({"type":"file.upload.response","payload":{"requestId":"req-overflow","file":null,"error":"Upload exceeded declared size: expected 5, received 6."}})
    );
    assert!(!path.exists() && !upload_dir.exists());
    assert!(!store.is_pending(DEFAULT_SOURCE, "req-overflow"));
}

#[test]
fn size_mismatch_at_end_is_an_error_and_cleans_up() {
    let home = tempfile::tempdir().unwrap();
    let mut store = UploadStore::new(home.path());
    store.begin_upload(&request("notes.txt", 11, "short"), DEFAULT_SOURCE);
    store.receive_frame(&begins("short"), DEFAULT_SOURCE);
    store.receive_frame(&chunk("short", "hello"), DEFAULT_SOURCE);
    let response = store.receive_frame(&ends("short"), DEFAULT_SOURCE).unwrap();
    assert_eq!(
        response.payload.error.as_deref(),
        Some("Upload size mismatch: expected 11, received 5.")
    );
    assert!(response.payload.file.is_none());
    assert_eq!(
        fs::read_dir(home.path().join("uploads")).unwrap().count(),
        0
    );
}

#[test]
fn chunks_before_begin_fail_and_unknown_requests_are_ignored() {
    let home = tempfile::tempdir().unwrap();
    let mut store = UploadStore::new(home.path());
    assert_eq!(
        store.receive_frame(&chunk("nobody", "x"), DEFAULT_SOURCE),
        None
    );
    store.begin_upload(&request("notes.txt", 5, "early"), DEFAULT_SOURCE);
    let response = store
        .receive_frame(&chunk("early", "x"), DEFAULT_SOURCE)
        .unwrap();
    assert_eq!(
        response.payload.error.as_deref(),
        Some("Upload chunks arrived before file begin.")
    );
}

#[test]
fn uploads_are_scoped_to_their_source() {
    let home = tempfile::tempdir().unwrap();
    let mut store = UploadStore::new(home.path());
    store.begin_upload(&request("notes.txt", 5, "r"), 1);
    assert_eq!(store.receive_frame(&begins("r"), 2), None);
    assert!(store.is_pending(1, "r"));
    store.cancel_source(1);
    assert!(!store.is_pending(1, "r"));
}

#[test]
fn replaces_duplicate_upload_starts_without_letting_the_old_stale_timeout_evict_the_replacement() {
    let home = tempfile::tempdir().unwrap();
    let mut store = UploadStore::new(home.path()).with_stale_timeout(Duration::from_millis(50));
    let t0 = Instant::now();
    store.begin_upload_at(&request("old.txt", 3, "dup"), DEFAULT_SOURCE, t0);
    store.receive_frame_at(&begins("dup"), DEFAULT_SOURCE, t0);
    store.receive_frame_at(&chunk("dup", "old"), DEFAULT_SOURCE, t0);

    let t25 = t0 + Duration::from_millis(25);
    store.begin_upload_at(&request("new.txt", 3, "dup"), DEFAULT_SOURCE, t25);
    assert!(!uploaded_dir_has(home.path(), "old.txt"));
    let t55 = t0 + Duration::from_millis(55);
    assert!(store.expire_stale_at(t55).is_empty());

    store.receive_frame_at(&begins("dup"), DEFAULT_SOURCE, t55);
    store.receive_frame_at(&chunk("dup", "new"), DEFAULT_SOURCE, t55);
    let path = uploaded_path(home.path(), "new.txt");
    let response = store
        .receive_frame_at(&ends("dup"), DEFAULT_SOURCE, t55)
        .unwrap();
    assert_eq!(response.payload.file.unwrap().path, path.to_string_lossy());
    assert_eq!(fs::read_to_string(path).unwrap(), "new");
}

fn uploaded_dir_has(home: &Path, name: &str) -> bool {
    fs::read_dir(home.join("uploads"))
        .map(|d| d.flatten().any(|e| e.path().join(name).exists()))
        .unwrap_or(false)
}

#[test]
fn keeps_an_active_upload_alive_beyond_the_initial_stale_timeout() {
    let home = tempfile::tempdir().unwrap();
    let mut store = UploadStore::new(home.path()).with_stale_timeout(Duration::from_millis(50));
    let t0 = Instant::now();
    store.begin_upload_at(&request("notes.txt", 11, "slow"), DEFAULT_SOURCE, t0);
    let ms = |n| t0 + Duration::from_millis(n);
    assert!(store.expire_stale_at(ms(25)).is_empty());
    store.receive_frame_at(&begins("slow"), DEFAULT_SOURCE, ms(25));
    assert!(store.expire_stale_at(ms(55)).is_empty());
    store.receive_frame_at(&chunk("slow", "hello"), DEFAULT_SOURCE, ms(55));
    assert!(store.expire_stale_at(ms(85)).is_empty());
    store.receive_frame_at(&chunk("slow", " world"), DEFAULT_SOURCE, ms(85));
    let path = uploaded_path(home.path(), "notes.txt");
    let response = store
        .receive_frame_at(&ends("slow"), DEFAULT_SOURCE, ms(85))
        .unwrap();
    assert_eq!(response.payload.error, None);
    assert_eq!(fs::read_to_string(path).unwrap(), "hello world");
}

#[test]
fn idle_uploads_expire_and_their_data_is_removed() {
    let home = tempfile::tempdir().unwrap();
    let mut store = UploadStore::new(home.path()).with_stale_timeout(Duration::from_millis(50));
    let t0 = Instant::now();
    store.begin_upload_at(&request("notes.txt", 11, "idle"), DEFAULT_SOURCE, t0);
    store.receive_frame_at(&begins("idle"), DEFAULT_SOURCE, t0);
    let path = uploaded_path(home.path(), "notes.txt");
    assert_eq!(
        store.expire_stale_at(t0 + Duration::from_millis(50)),
        vec![(DEFAULT_SOURCE, "idle".to_string())]
    );
    assert!(!path.exists());
    assert_eq!(
        store.receive_frame_at(&chunk("idle", "x"), DEFAULT_SOURCE, t0),
        None
    );
}
