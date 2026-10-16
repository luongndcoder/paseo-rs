//! Wire-level behavior of the request handlers.

use paseo_files::binary_frames::*;
use paseo_files::handlers::*;
use paseo_files::token_store::DownloadTokenStore;
use paseo_files::wire::*;
use serde_json::{json, Value};
use std::fs;
use std::time::Duration;

fn explorer_request(
    cwd: &str,
    mode: &str,
    path: Option<&str>,
    extra: Value,
) -> FileExplorerRequest {
    let mut v = json!({"type":"file_explorer_request","cwd":cwd,"mode":mode,"requestId":"r1"});
    if let Some(p) = path {
        v["path"] = json!(p);
    }
    for (k, val) in extra.as_object().unwrap() {
        v[k] = val.clone();
    }
    serde_json::from_value(v).unwrap()
}

fn response_json(o: FileExplorerOutcome) -> Value {
    match o {
        FileExplorerOutcome::Response(r) => serde_json::to_value(*r).unwrap(),
        FileExplorerOutcome::Streamed => panic!("expected a JSON response"),
    }
}

#[test]
fn list_response_shape() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("a.txt"), "abc").unwrap();
    let req = explorer_request(root.path().to_str().unwrap(), "list", None, json!({}));
    let v = response_json(handle_file_explorer_request(&req, None));
    assert_eq!(v["type"], "file_explorer_response");
    let p = &v["payload"];
    assert_eq!(p["cwd"], root.path().to_str().unwrap());
    assert_eq!(p["path"], ".");
    assert_eq!(p["mode"], "list");
    assert_eq!(p["file"], Value::Null);
    assert_eq!(p["error"], Value::Null);
    assert_eq!(p["requestId"], "r1");
    assert_eq!(p["directory"]["entries"][0]["name"], "a.txt");
    assert_eq!(p["directory"]["entries"][0]["kind"], "file");
    assert_eq!(p["directory"]["entries"][0]["size"], 3);
}

#[test]
fn file_response_shape_and_errors() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("a.txt"), "abc").unwrap();
    let cwd = root.path().to_str().unwrap();
    let v = response_json(handle_file_explorer_request(
        &explorer_request(cwd, "file", Some("a.txt"), json!({})),
        None,
    ));
    assert_eq!(v["payload"]["file"]["content"], "abc");
    assert_eq!(v["payload"]["file"]["encoding"], "utf-8");
    assert_eq!(v["payload"]["directory"], Value::Null);

    let v = response_json(handle_file_explorer_request(
        &explorer_request(cwd, "file", Some("../x"), json!({})),
        None,
    ));
    assert_eq!(
        v["payload"]["error"],
        "Access outside of workspace is not allowed"
    );
    assert_eq!(v["payload"]["file"], Value::Null);
    assert_eq!(v["payload"]["path"], "../x");

    let v = response_json(handle_file_explorer_request(
        &explorer_request("  ", "list", None, json!({})),
        None,
    ));
    assert_eq!(v["payload"]["error"], "cwd is required");
    assert_eq!(v["payload"]["cwd"], "  ");
}

#[test]
fn max_bytes_rejects_large_files_before_reading() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("big.txt"), vec![b'a'; 100]).unwrap();
    let cwd = root.path().to_str().unwrap();
    let v = response_json(handle_file_explorer_request(
        &explorer_request(cwd, "file", Some("big.txt"), json!({"maxBytes": 99})),
        None,
    ));
    assert_eq!(v["payload"]["error"], "File is too large to display");
    let v = response_json(handle_file_explorer_request(
        &explorer_request(cwd, "file", Some("big.txt"), json!({"maxBytes": 100})),
        None,
    ));
    assert_eq!(v["payload"]["error"], Value::Null);
}

#[test]
fn accept_binary_streams_frames_only_when_a_binary_channel_exists() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("a.txt"), "hello").unwrap();
    let cwd = root.path().to_str().unwrap();
    let req = explorer_request(cwd, "file", Some("a.txt"), json!({"acceptBinary": true}));

    let mut frames: Vec<Vec<u8>> = Vec::new();
    let mut emit = |f: Vec<u8>| -> Result<(), String> {
        frames.push(f);
        Ok(())
    };
    assert!(matches!(
        handle_file_explorer_request(&req, Some(&mut emit)),
        FileExplorerOutcome::Streamed
    ));
    assert_eq!(frames.len(), 3);
    match decode_file_transfer_frame(&frames[0]).unwrap() {
        FileTransferFrame::Begin {
            request_id,
            metadata,
        } => {
            assert_eq!(request_id, "r1");
            assert_eq!(metadata.size, 5);
            assert_eq!(metadata.encoding, FileEncoding::Utf8);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        decode_file_transfer_frame(&frames[1]),
        Some(FileTransferFrame::Chunk {
            request_id: "r1".into(),
            payload: b"hello".to_vec()
        })
    );

    // Without a binary channel the same request falls back to a JSON response.
    let v = response_json(handle_file_explorer_request(&req, None));
    assert_eq!(v["payload"]["file"]["content"], "hello");
}

#[test]
fn a_stream_failure_becomes_an_error_response() {
    let root = tempfile::tempdir().unwrap();
    let cwd = root.path().to_str().unwrap();
    let req = explorer_request(
        cwd,
        "file",
        Some("missing.txt"),
        json!({"acceptBinary": true}),
    );
    let mut emit = |_f: Vec<u8>| -> Result<(), String> { Ok(()) };
    let v = response_json(handle_file_explorer_request(&req, Some(&mut emit)));
    assert!(v["payload"]["error"]
        .as_str()
        .unwrap()
        .starts_with("ENOENT"));
}

#[test]
fn download_token_roundtrip() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("report.txt"), "abc").unwrap();
    let mut store = DownloadTokenStore::new(Duration::from_secs(60));
    let req: FileDownloadTokenRequest = serde_json::from_value(json!({
        "type":"file_download_token_request","cwd":root.path().to_str().unwrap(),"path":"report.txt","requestId":"r9"
    }))
    .unwrap();
    let v = serde_json::to_value(handle_file_download_token_request(&req, &mut store)).unwrap();
    assert_eq!(v["type"], "file_download_token_response");
    let p = &v["payload"];
    assert_eq!(p["path"], "report.txt");
    assert_eq!(p["fileName"], "report.txt");
    assert_eq!(p["mimeType"], "text/plain");
    assert_eq!(p["size"], 3);
    assert_eq!(p["error"], Value::Null);
    let token = p["token"].as_str().unwrap();
    let entry = store.consume_token(token).unwrap();
    assert_eq!(
        entry.absolute_path,
        fs::canonicalize(root.path().join("report.txt"))
            .unwrap()
            .to_string_lossy()
    );

    let bad: FileDownloadTokenRequest = serde_json::from_value(json!({
        "type":"file_download_token_request","cwd":root.path().to_str().unwrap(),"path":"nope.txt","requestId":"r10"
    }))
    .unwrap();
    let v = serde_json::to_value(handle_file_download_token_request(&bad, &mut store)).unwrap();
    assert_eq!(v["payload"]["token"], Value::Null);
    assert_eq!(v["payload"]["fileName"], Value::Null);
    assert!(v["payload"]["error"]
        .as_str()
        .unwrap()
        .starts_with("ENOENT"));
}

#[test]
fn directory_suggestions_workspace_response() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir_all(root.path().join("src/components")).unwrap();
    fs::write(root.path().join("src/components/chat.tsx"), "").unwrap();
    let req: DirectorySuggestionsRequest = serde_json::from_value(json!({
        "type":"directory_suggestions_request","query":"src/co","cwd":root.path().to_str().unwrap(),
        "includeFiles":true,"includeDirectories":true,"limit":20,"requestId":"s1"
    }))
    .unwrap();
    let v = serde_json::to_value(handle_directory_suggestions_request(&req)).unwrap();
    assert_eq!(
        v,
        json!({"type":"directory_suggestions_response","payload":{
            "directories":["src/components"],
            "entries":[{"path":"src/components","kind":"directory"},{"path":"src/components/chat.tsx","kind":"file"}],
            "error":null,"requestId":"s1"}})
    );
}
