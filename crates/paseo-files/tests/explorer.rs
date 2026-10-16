//! Ports of `file-explorer/service.test.ts` and `service.posix.test.ts` (read, stream, write).

use paseo_files::explorer::*;
use paseo_files::wire::*;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::Path;

fn params<'a>(root: &'a Path, rel: &'a str) -> ReadFileParams<'a> {
    ReadFileParams {
        root: root.to_str().unwrap(),
        relative_path: rel,
    }
}

fn ready(v: ExplorerFileVersion) -> (String, String) {
    match v {
        ExplorerFileVersion::Ready {
            modified_at,
            revision,
            ..
        } => (modified_at, revision),
        other => panic!("expected ready, got {other:?}"),
    }
}

fn write_params<'a>(
    root: &'a Path,
    rel: &'a str,
    content: &'a str,
    modified_at: &'a str,
    revision: Option<&'a str>,
) -> WriteFileParams<'a> {
    WriteFileParams {
        root: root.to_str().unwrap(),
        relative_path: rel,
        content,
        expected_modified_at: modified_at,
        expected_revision: revision,
    }
}

#[test]
fn atomically_writes_an_existing_text_file_at_the_expected_revision() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("notes.txt"), "before").unwrap();
    let (m, r) = ready(get_explorer_file_version(params(root.path(), "notes.txt")));
    let result = write_explorer_file(write_params(
        root.path(),
        "notes.txt",
        "after",
        &m,
        Some(&r),
    ));
    assert!(
        matches!(result, ExplorerFileWriteResult::Written { .. }),
        "{result:?}"
    );
    assert_eq!(
        read_explorer_file(params(root.path(), "notes.txt"))
            .unwrap()
            .content
            .as_deref(),
        Some("after")
    );
    let leftovers: Vec<_> = fs::read_dir(root.path())
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty());
}

#[test]
fn preserves_original_file_permissions_across_atomic_replacement() {
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("script.sh");
    fs::write(&file, "before").unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o764)).unwrap();
    let (m, r) = ready(get_explorer_file_version(params(root.path(), "script.sh")));
    let result = write_explorer_file(write_params(
        root.path(),
        "script.sh",
        "after",
        &m,
        Some(&r),
    ));
    assert!(matches!(result, ExplorerFileWriteResult::Written { .. }));
    assert_eq!(
        fs::metadata(&file).unwrap().permissions().mode() & 0o7777,
        0o764
    );
}

#[test]
fn preserves_a_newer_disk_revision_instead_of_overwriting_it() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("notes.txt"), "newer on disk").unwrap();
    let result = write_explorer_file(write_params(
        root.path(),
        "notes.txt",
        "stale local edit",
        "2020-01-01T00:00:00.000Z",
        None,
    ));
    assert!(
        matches!(
            result,
            ExplorerFileWriteResult::Conflict {
                version: ExplorerFileVersion::Ready { .. }
            }
        ),
        "{result:?}"
    );
    assert_eq!(
        read_explorer_file(params(root.path(), "notes.txt"))
            .unwrap()
            .content
            .as_deref(),
        Some("newer on disk")
    );
}

#[test]
fn prefers_the_high_precision_revision_token_over_the_display_timestamp() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("notes.txt"), "on disk").unwrap();
    let (m, r) = ready(get_explorer_file_version(params(root.path(), "notes.txt")));
    let stale = format!("{r}-stale");
    let result = write_explorer_file(write_params(
        root.path(),
        "notes.txt",
        "stale local edit",
        &m,
        Some(&stale),
    ));
    assert!(matches!(result, ExplorerFileWriteResult::Conflict { .. }));
    assert_eq!(
        read_explorer_file(params(root.path(), "notes.txt"))
            .unwrap()
            .content
            .as_deref(),
        Some("on disk")
    );
}

#[test]
fn never_creates_a_missing_file_through_the_write_api() {
    let root = tempfile::tempdir().unwrap();
    let result = write_explorer_file(write_params(
        root.path(),
        "missing.txt",
        "new file",
        "2020-01-01T00:00:00.000Z",
        None,
    ));
    assert!(matches!(
        result,
        ExplorerFileWriteResult::Conflict {
            version: ExplorerFileVersion::Missing { .. }
        }
    ));
    assert!(!root.path().join("missing.txt").exists());
}

#[test]
fn write_refuses_binary_and_oversized_content() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("blob.bin"), [0u8, 1, 2]).unwrap();
    let (m, r) = ready(get_explorer_file_version(params(root.path(), "blob.bin")));
    let result = write_explorer_file(write_params(root.path(), "blob.bin", "x", &m, Some(&r)));
    assert_eq!(
        result,
        ExplorerFileWriteResult::Error {
            error: "Binary files cannot be edited".into()
        }
    );
    let big = "a".repeat(MAX_EDITABLE_FILE_BYTES as usize + 1);
    let result = write_explorer_file(write_params(root.path(), "blob.bin", &big, &m, Some(&r)));
    assert_eq!(
        result,
        ExplorerFileWriteResult::Error {
            error: "File is too large to edit".into()
        }
    );
}

#[test]
fn reads_ex_and_unknown_extension_files_as_text() {
    let root = tempfile::tempdir().unwrap();
    for (name, content) in [
        ("sample.ex", "defmodule Sample do\nend\n"),
        ("notes.customext", "hello from a custom text file\n"),
    ] {
        fs::write(root.path().join(name), content).unwrap();
        let result = read_explorer_file(params(root.path(), name)).unwrap();
        assert_eq!(result.kind, ExplorerFileKind::Text);
        assert_eq!(result.encoding, ExplorerEncoding::Utf8);
        assert_eq!(result.mime_type.as_deref(), Some("text/plain"));
        assert_eq!(result.content.as_deref(), Some(content));
    }
}

#[test]
fn json_files_get_json_mime_and_images_are_base64() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("a.json"), "{}").unwrap();
    assert_eq!(
        read_explorer_file(params(root.path(), "a.json"))
            .unwrap()
            .mime_type
            .as_deref(),
        Some("application/json")
    );
    fs::write(root.path().join("p.PNG"), [0x89, b'P', b'N', b'G']).unwrap();
    let img = read_explorer_file(params(root.path(), "p.PNG")).unwrap();
    assert_eq!(img.kind, ExplorerFileKind::Image);
    assert_eq!(img.encoding, ExplorerEncoding::Base64);
    assert_eq!(img.mime_type.as_deref(), Some("image/png"));
    assert_eq!(img.content.as_deref(), Some("iVBORw=="));
}

#[test]
fn classifies_files_with_null_bytes_as_binary() {
    let root = tempfile::tempdir().unwrap();
    fs::write(
        root.path().join("blob.weird"),
        [0x48, 0x65, 0x00, 0x6c, 0x6f],
    )
    .unwrap();
    let result = read_explorer_file(params(root.path(), "blob.weird")).unwrap();
    assert_eq!(result.kind, ExplorerFileKind::Binary);
    assert_eq!(result.encoding, ExplorerEncoding::None);
    assert_eq!(result.content, None);
    assert_eq!(
        result.mime_type.as_deref(),
        Some("application/octet-stream")
    );
}

fn drain(stream: FileStream) -> Result<usize, paseo_files::ExplorerError> {
    let mut total = 0;
    for chunk in stream.chunks {
        total += chunk?.len();
    }
    Ok(total)
}

#[test]
fn stream_yields_256kib_chunks_and_matches_advertised_metadata() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("big.txt"), vec![b'a'; 600 * 1024]).unwrap();
    let stream = open_explorer_file_stream(params(root.path(), "big.txt")).unwrap();
    assert_eq!(stream.meta.size, 600 * 1024);
    assert_eq!(stream.meta.kind, ExplorerFileKind::Text);
    let sizes: Vec<usize> = stream.chunks.map(|c| c.unwrap().len()).collect();
    assert_eq!(sizes, vec![256 * 1024, 256 * 1024, 88 * 1024]);
}

#[test]
fn fails_a_stream_when_the_file_grows_after_its_revision_is_advertised() {
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("growing.log");
    fs::write(&file, vec![0x61u8; 300 * 1024]).unwrap();
    let stream = open_explorer_file_stream(params(root.path(), "growing.log")).unwrap();
    fs::OpenOptions::new()
        .append(true)
        .open(&file)
        .unwrap()
        .write_all(&vec![0x62u8; 300 * 1024])
        .unwrap();
    assert_eq!(
        drain(stream).unwrap_err().to_string(),
        "File changed during transfer"
    );
}

#[test]
fn fails_a_stream_when_the_file_shrinks_below_its_advertised_size() {
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("shrinking.log");
    fs::write(&file, vec![0x61u8; 300 * 1024]).unwrap();
    let stream = open_explorer_file_stream(params(root.path(), "shrinking.log")).unwrap();
    fs::OpenOptions::new()
        .write(true)
        .open(&file)
        .unwrap()
        .set_len(100 * 1024)
        .unwrap();
    assert_eq!(
        drain(stream).unwrap_err().to_string(),
        "File changed during transfer"
    );
}

#[test]
fn fails_a_stream_when_the_file_is_overwritten_in_place() {
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("changing.log");
    fs::write(&file, vec![0x61u8; 600 * 1024]).unwrap();
    let mut stream = open_explorer_file_stream(params(root.path(), "changing.log")).unwrap();
    let mut error = None;
    let mut index = 0;
    for chunk in stream.chunks.by_ref() {
        match chunk {
            Ok(_) => {
                index += 1;
                if index == 1 {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                    fs::write(&file, vec![0x62u8; 600 * 1024]).unwrap();
                }
            }
            Err(e) => error = Some(e.to_string()),
        }
    }
    assert_eq!(error.as_deref(), Some("File changed during transfer"));
}

fn stream_kind(
    root: &Path,
    name: &str,
    content: &[u8],
) -> (ExplorerFileKind, paseo_files::binary_frames::FileEncoding) {
    fs::write(root.join(name), content).unwrap();
    let s = open_explorer_file_stream(params(root, name)).unwrap();
    (s.meta.kind, s.meta.encoding)
}

#[test]
fn classifies_sampled_text_when_utf8_crosses_a_read_boundary() {
    use paseo_files::binary_frames::FileEncoding;
    let root = tempfile::tempdir().unwrap();
    let mut content = vec![0x61u8; 8191];
    content.extend_from_slice("€z".as_bytes());
    assert_eq!(
        stream_kind(root.path(), "sample.txt", &content),
        (ExplorerFileKind::Text, FileEncoding::Utf8)
    );
    // Same property across the real 256 KiB block boundary.
    let mut content = vec![0x61u8; 256 * 1024 - 1];
    content.extend_from_slice("€z".as_bytes());
    assert_eq!(
        stream_kind(root.path(), "edge.txt", &content),
        (ExplorerFileKind::Text, FileEncoding::Utf8)
    );
}

#[test]
fn rejects_incomplete_utf8_when_the_whole_file_was_sampled() {
    let root = tempfile::tempdir().unwrap();
    assert_eq!(
        stream_kind(root.path(), "invalid.txt", &[0x61, 0xe2, 0x82]).0,
        ExplorerFileKind::Binary
    );
}

#[test]
fn detects_binary_bytes_beyond_the_initial_classification_block() {
    let root = tempfile::tempdir().unwrap();
    let mut content = vec![0x61u8; 8192];
    content.push(0xff);
    assert_eq!(
        stream_kind(root.path(), "late-binary.unknown", &content).0,
        ExplorerFileKind::Binary
    );
}

#[test]
fn emits_begin_chunk_end_frames_in_order() {
    use paseo_files::binary_frames::*;
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("f.txt"), vec![b'x'; 300 * 1024]).unwrap();
    let mut frames = Vec::new();
    emit_file_transfer_frames(
        "req-1",
        params(root.path(), "f.txt"),
        |f| -> Result<(), String> {
            frames.push(f);
            Ok(())
        },
    )
    .unwrap();
    let decoded: Vec<_> = frames
        .iter()
        .map(|f| decode_file_transfer_frame(f).unwrap())
        .collect();
    assert_eq!(decoded.len(), 4);
    match &decoded[0] {
        FileTransferFrame::Begin { metadata, .. } => {
            assert_eq!(metadata.size, 300 * 1024);
            assert_eq!(metadata.mime, "text/plain");
            assert!(metadata.revision.is_some());
        }
        other => panic!("{other:?}"),
    }
    assert!(
        matches!(&decoded[1], FileTransferFrame::Chunk { payload, .. } if payload.len() == 256 * 1024)
    );
    assert!(
        matches!(&decoded[2], FileTransferFrame::Chunk { payload, .. } if payload.len() == 44 * 1024)
    );
    assert!(matches!(&decoded[3], FileTransferFrame::End { .. }));
}

#[test]
fn tilde_prefix_expands_against_the_home_directory() {
    let home = std::env::var("HOME").unwrap();
    let root = tempfile::Builder::new()
        .prefix(".paseo-file-explorer-home-")
        .tempdir_in(&home)
        .unwrap();
    fs::write(root.path().join("sample.txt"), "hello from home\n").unwrap();
    let rel = root.path().strip_prefix(&home).unwrap().join("sample.txt");
    let tilde = format!("~/{}", rel.display());
    let result = read_explorer_file(params(root.path(), &tilde)).unwrap();
    assert_eq!(result.content.as_deref(), Some("hello from home\n"));

    let result = read_explorer_file(ReadFileParams {
        root: "~",
        relative_path: &tilde,
    })
    .unwrap();
    assert_eq!(result.path, rel.to_string_lossy());
    assert_eq!(result.content.as_deref(), Some("hello from home\n"));
}

#[test]
fn rejects_tilde_paths_that_resolve_outside_the_workspace() {
    let root = tempfile::tempdir().unwrap();
    let err = read_explorer_file(params(root.path(), "~/some/file.txt")).unwrap_err();
    assert_eq!(
        err.to_string(),
        "Access outside of workspace is not allowed"
    );
}

#[test]
fn rejects_dot_dot_traversal_and_absolute_outside_paths() {
    let root = tempfile::tempdir().unwrap();
    for p in ["../x", "/etc/hosts"] {
        let err = read_explorer_file(params(root.path(), p)).unwrap_err();
        assert!(err.is_outside_workspace(), "{p}");
    }
}

#[test]
fn lists_entries_newest_first_with_dir_and_file_kinds() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("old.txt"), "a").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(30));
    fs::create_dir(root.path().join("newdir")).unwrap();
    let t0 = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    fs::File::open(root.path().join("old.txt"))
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(t0))
        .unwrap();
    let dir = list_directory_entries(ListDirectoryParams {
        root: root.path().to_str().unwrap(),
        relative_path: ".",
    })
    .unwrap();
    assert_eq!(dir.path, ".");
    let names: Vec<_> = dir
        .entries
        .iter()
        .map(|e| (e.name.as_str(), e.kind))
        .collect();
    assert_eq!(
        names,
        vec![
            ("newdir", ExplorerEntryKind::Directory),
            ("old.txt", ExplorerEntryKind::File)
        ]
    );
    assert_eq!(dir.entries[1].size, 1);
    assert_eq!(dir.entries[1].path, "old.txt");
}

#[test]
fn listing_a_file_is_an_error() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("a.txt"), "").unwrap();
    let err = list_directory_entries(ListDirectoryParams {
        root: root.path().to_str().unwrap(),
        relative_path: "a.txt",
    })
    .unwrap_err();
    assert_eq!(err.to_string(), "Requested path is not a directory");
}

#[test]
fn lists_directory_entries_even_when_a_dangling_symlink_exists() {
    let root = tempfile::tempdir().unwrap();
    let server = root.path().join("packages/server");
    fs::create_dir_all(&server).unwrap();
    fs::write(server.join("README.md"), "# server\n").unwrap();
    symlink("CLAUDE.md", server.join("AGENTS.md")).unwrap();
    let result = list_directory_entries(ListDirectoryParams {
        root: root.path().to_str().unwrap(),
        relative_path: "packages/server",
    })
    .unwrap();
    assert_eq!(result.path, "packages/server");
    let names: Vec<_> = result.entries.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"README.md"));
    assert!(!names.contains(&"AGENTS.md"));
}

#[test]
fn rejects_symlinked_files_that_resolve_outside_the_workspace() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("secret.txt"), "top secret\n").unwrap();
    symlink(
        outside.path().join("secret.txt"),
        root.path().join("secret-link.txt"),
    )
    .unwrap();
    let err = read_explorer_file(params(root.path(), "secret-link.txt")).unwrap_err();
    assert_eq!(
        err.to_string(),
        "Access outside of workspace is not allowed"
    );
}

#[test]
fn skips_listed_symlink_entries_that_resolve_outside_the_workspace() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::write(root.path().join("visible.txt"), "visible\n").unwrap();
    fs::write(outside.path().join("secret.txt"), "top secret\n").unwrap();
    symlink(
        outside.path().join("secret.txt"),
        root.path().join("secret-link.txt"),
    )
    .unwrap();
    let result = list_directory_entries(ListDirectoryParams {
        root: root.path().to_str().unwrap(),
        relative_path: ".",
    })
    .unwrap();
    let names: Vec<_> = result.entries.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"visible.txt"));
    assert!(!names.contains(&"secret-link.txt"));
}

#[test]
fn uses_canonical_paths_for_downloadable_symlink_targets_inside_the_workspace() {
    let root = tempfile::tempdir().unwrap();
    let target = root.path().join("safe.txt");
    fs::write(&target, "safe\n").unwrap();
    symlink("safe.txt", root.path().join("safe-link.txt")).unwrap();
    let file = read_explorer_file(params(root.path(), "safe-link.txt")).unwrap();
    let info = get_downloadable_file_info(params(root.path(), "safe-link.txt")).unwrap();
    assert_eq!(file.path, "safe-link.txt");
    assert_eq!(file.content.as_deref(), Some("safe\n"));
    assert_eq!(info.path, "safe-link.txt");
    assert_eq!(info.file_name, "safe-link.txt");
    assert_eq!(
        info.absolute_path,
        fs::canonicalize(target).unwrap().to_string_lossy()
    );
}

#[test]
fn downloadable_info_sniffs_mime_from_content_and_extension() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("a.json"), "{}").unwrap();
    fs::write(root.path().join("b.dat"), [0u8, 1, 2, 3]).unwrap();
    fs::write(root.path().join("c.svg"), "<svg/>").unwrap();
    assert_eq!(
        get_downloadable_file_info(params(root.path(), "a.json"))
            .unwrap()
            .mime_type,
        "application/json"
    );
    assert_eq!(
        get_downloadable_file_info(params(root.path(), "b.dat"))
            .unwrap()
            .mime_type,
        "application/octet-stream"
    );
    assert_eq!(
        get_downloadable_file_info(params(root.path(), "c.svg"))
            .unwrap()
            .mime_type,
        "image/svg+xml"
    );
}

#[test]
fn version_reports_missing_and_non_file() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("d")).unwrap();
    assert!(matches!(
        get_explorer_file_version(params(root.path(), "nope.txt")),
        ExplorerFileVersion::Missing { .. }
    ));
    match get_explorer_file_version(params(root.path(), "d")) {
        ExplorerFileVersion::Error { error, path, .. } => {
            assert_eq!(error, "Requested path is not a file");
            assert_eq!(path, "d");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn revision_has_dev_ino_size_mtime_ns_shape() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("a.txt"), "abc").unwrap();
    let (_, revision) = ready(get_explorer_file_version(params(root.path(), "a.txt")));
    let parts: Vec<_> = revision.split(':').collect();
    assert_eq!(parts.len(), 4);
    assert_eq!(parts[2], "3");
    assert!(parts.iter().all(|p| p.chars().all(|c| c.is_ascii_digit())));
}
