//! Ports of `local/models.test.ts` and `sherpa/model-downloader.test.ts`, plus real HTTP
//! download/extract coverage against a local server (no large models are fetched).

mod support;

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use paseo_speech::providers::local::catalog::{model_spec, ModelSpec, NEMOTRON_MODEL_ID};
use paseo_speech::providers::local::downloader::{
    ensure_model, ensure_models, is_model_installed, list_missing_models, model_dir,
    EnsureModelOptions,
};
use paseo_speech::providers::local::vietnamese_model::{
    candidate_nemotron_dirs, ensure_vietnamese_sherpa_model_with,
    resolve_installed_nemotron_model_with, ModelEnv,
};
use support::{Reply, TestServer};
use tokio_util::sync::CancellationToken;

const PARAKEET: &str = "parakeet-tdt-0.6b-v2-int8";
const KOKORO: &str = "kokoro-en-v0_19";

fn spec(id: &str) -> &'static ModelSpec {
    model_spec(id).unwrap()
}

fn archive_name(spec: &ModelSpec) -> String {
    spec.archive_url.rsplit('/').next().unwrap().to_string()
}

fn write_complete_model(dir: &Path, spec: &ModelSpec, filler: usize) {
    fs::create_dir_all(dir).unwrap();
    for file in spec.required_files {
        if *file == "espeak-ng-data" {
            fs::create_dir_all(dir.join(file)).unwrap();
        } else {
            let mut content = format!("complete {file}");
            while content.len() < filler {
                content.push('.');
            }
            fs::write(dir.join(file), content).unwrap();
        }
    }
}

fn build_tar(source_root: &Path, entry: &str) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    builder
        .append_dir_all(entry, source_root.join(entry))
        .unwrap();
    builder.into_inner().unwrap()
}

fn bzip2(data: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut enc = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::fast());
    enc.write_all(data).unwrap();
    enc.finish().unwrap()
}

// ---- models.test.ts -------------------------------------------------------------

#[tokio::test]
async fn reinstalls_a_model_whose_extraction_was_interrupted() {
    // An interrupted extraction leaves the archive in .downloads and a truncated file in place.
    let root = tempfile::tempdir().unwrap();
    let spec = spec(PARAKEET);
    let source_root = root.path().join("source");
    write_complete_model(&source_root.join(spec.extracted_dir), spec, 64_000);
    let models_dir = root.path().join("models");
    let archive = models_dir.join(".downloads").join(archive_name(spec));
    fs::create_dir_all(archive.parent().unwrap()).unwrap();
    fs::write(&archive, build_tar(&source_root, spec.extracted_dir)).unwrap(); // plain tar, .tar.bz2 name

    let model_dir = model_dir(&models_dir, PARAKEET).unwrap();
    write_complete_model(&model_dir, spec, 64_000);
    let encoder = model_dir.join(spec.required_files[0]);
    fs::OpenOptions::new()
        .write(true)
        .open(&encoder)
        .unwrap()
        .set_len(1_000)
        .unwrap();

    assert_eq!(
        list_missing_models(&models_dir, &[PARAKEET]).await.unwrap(),
        vec![PARAKEET.to_string()]
    );

    ensure_models(&models_dir, &[PARAKEET], None).await.unwrap();

    let expected = {
        let mut c = format!("complete {}", spec.required_files[0]);
        while c.len() < 64_000 {
            c.push('.');
        }
        c
    };
    assert_eq!(fs::read_to_string(&encoder).unwrap(), expected);
    assert!(!archive.exists());
    assert!(list_missing_models(&models_dir, &[PARAKEET])
        .await
        .unwrap()
        .is_empty());
}

// ---- model-downloader.test.ts ---------------------------------------------------

#[tokio::test]
async fn does_not_expose_a_partial_model_when_tar_extraction_fails() {
    let root = tempfile::tempdir().unwrap();
    let models_dir = root.path().join("models");
    let spec = spec(KOKORO);
    let source = root.path().join("source").join(spec.extracted_dir);
    fs::create_dir_all(&source).unwrap();
    fs::write(source.join("model.onnx"), vec![1u8; 32_768]).unwrap();
    let archive = models_dir.join(".downloads").join(archive_name(spec));
    fs::create_dir_all(archive.parent().unwrap()).unwrap();
    let tar = build_tar(&root.path().join("source"), spec.extracted_dir);
    fs::write(&archive, &tar[..1_024]).unwrap(); // truncated archive

    let err = ensure_model(&models_dir, KOKORO, EnsureModelOptions::default())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("tar exited"), "{err}");
    assert!(!model_dir(&models_dir, KOKORO).unwrap().exists());
    assert!(archive.exists());
    let entries: Vec<String> = fs::read_dir(&models_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(entries, vec![".downloads".to_string()]);
}

#[tokio::test]
async fn repairs_a_partial_legacy_extraction_when_its_cached_archive_remains() {
    let root = tempfile::tempdir().unwrap();
    let models_dir = root.path().join("models");
    let spec = spec(KOKORO);
    let source = root.path().join("source").join(spec.extracted_dir);
    let model_dir = model_dir(&models_dir, KOKORO).unwrap();
    for dir in [&source, &model_dir] {
        fs::create_dir_all(dir.join("espeak-ng-data")).unwrap();
        for f in ["model.onnx", "voices.bin", "tokens.txt"] {
            fs::write(dir.join(f), "complete model data").unwrap();
        }
    }
    let archive = models_dir.join(".downloads").join(archive_name(spec));
    fs::create_dir_all(archive.parent().unwrap()).unwrap();
    fs::write(
        &archive,
        build_tar(&root.path().join("source"), spec.extracted_dir),
    )
    .unwrap();
    fs::write(model_dir.join("model.onnx"), "truncated").unwrap();

    ensure_model(&models_dir, KOKORO, EnsureModelOptions::default())
        .await
        .unwrap();
    assert_eq!(
        fs::read_to_string(model_dir.join("model.onnx")).unwrap(),
        "complete model data"
    );
}

#[test]
fn model_dir_maps_model_id_to_extracted_dir() {
    let models_dir = Path::new("/tmp/models");
    assert!(model_dir(models_dir, PARAKEET)
        .unwrap()
        .to_string_lossy()
        .contains("sherpa-onnx-nemo-parakeet-tdt-0.6b-v2-int8"));
    assert!(model_dir(models_dir, KOKORO)
        .unwrap()
        .to_string_lossy()
        .contains("kokoro-en-v0_19"));
    assert!(model_dir(models_dir, "nope").is_err());
}

#[tokio::test]
async fn ensure_succeeds_without_downloading_when_files_exist() {
    let models_dir = tempfile::tempdir().unwrap();
    let dir = model_dir(models_dir.path(), KOKORO).unwrap();
    fs::create_dir_all(dir.join("espeak-ng-data")).unwrap();
    for f in ["model.onnx", "voices.bin", "tokens.txt"] {
        fs::write(dir.join(f), "x").unwrap();
    }
    let out = ensure_model(models_dir.path(), KOKORO, EnsureModelOptions::default())
        .await
        .unwrap();
    assert_eq!(out, dir);
    assert!(is_model_installed(models_dir.path(), KOKORO).await.unwrap());
}

// ---- HTTP download -------------------------------------------------------------

#[tokio::test]
async fn downloads_extracts_and_installs_a_bz2_archive_over_http() {
    let root = tempfile::tempdir().unwrap();
    let spec = spec(KOKORO);
    let source_root = root.path().join("source");
    write_complete_model(&source_root.join(spec.extracted_dir), spec, 32);
    let archive_bytes = bzip2(&build_tar(&source_root, spec.extracted_dir));
    let name = archive_name(spec);
    let expected_path = format!("/models/{name}");
    let served = archive_bytes.clone();
    let server = TestServer::start(move |req| {
        if req.path == expected_path {
            Reply::bytes(served.clone())
        } else {
            Reply::json(404, "{}")
        }
    })
    .await;

    let models_dir = root.path().join("models");
    let url = format!("{}/models/{name}", server.base_url.trim_end_matches("/v1"));
    let dir = ensure_model(
        &models_dir,
        KOKORO,
        EnsureModelOptions {
            archive_url: Some(&url),
            cancel: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(dir, models_dir.join("kokoro-en-v0_19"));
    assert!(fs::read_to_string(dir.join("model.onnx"))
        .unwrap()
        .starts_with("complete model.onnx"));
    assert!(dir.join("espeak-ng-data").is_dir());
    assert!(
        !models_dir.join(".downloads").join(&name).exists(),
        "archive removed once installed"
    );
    assert!(!models_dir
        .join(".downloads")
        .join("kokoro-en-v0_19.extracting")
        .exists());
    assert!(is_model_installed(&models_dir, KOKORO).await.unwrap());
}

#[tokio::test]
async fn http_failure_leaves_no_archive_and_reports_status() {
    let server = TestServer::start(|_| Reply::json(404, "{}")).await;
    let models_dir = tempfile::tempdir().unwrap();
    let url = format!(
        "{}/missing.tar.bz2",
        server.base_url.trim_end_matches("/v1")
    );
    let err = ensure_model(
        models_dir.path(),
        KOKORO,
        EnsureModelOptions {
            archive_url: Some(&url),
            cancel: None,
        },
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("HTTP 404"), "{err}");
    let downloads = models_dir.path().join(".downloads");
    let leftovers = fs::read_dir(&downloads).map(|d| d.count()).unwrap_or(0);
    assert_eq!(leftovers, 0);
    assert!(!is_model_installed(models_dir.path(), KOKORO).await.unwrap());
}

#[tokio::test]
async fn a_cancelled_download_aborts_and_cleans_up() {
    let server = TestServer::start(|_| Reply::bytes(vec![0u8; 16])).await;
    let models_dir = tempfile::tempdir().unwrap();
    let token = CancellationToken::new();
    token.cancel();
    let url = format!("{}/x.tar.bz2", server.base_url.trim_end_matches("/v1"));
    let err = ensure_model(
        models_dir.path(),
        KOKORO,
        EnsureModelOptions {
            archive_url: Some(&url),
            cancel: Some(&token),
        },
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("aborted"), "{err}");
    assert!(!model_dir(models_dir.path(), KOKORO).unwrap().exists());
}

#[tokio::test]
async fn ensure_models_installs_each_model_once_and_returns_their_dirs() {
    let models_dir = tempfile::tempdir().unwrap();
    for id in [KOKORO, PARAKEET] {
        write_complete_model(&model_dir(models_dir.path(), id).unwrap(), spec(id), 8);
    }
    let dirs: HashMap<String, PathBuf> =
        ensure_models(models_dir.path(), &[KOKORO, PARAKEET, KOKORO], None)
            .await
            .unwrap();
    assert_eq!(dirs.len(), 2);
    assert_eq!(dirs[KOKORO], model_dir(models_dir.path(), KOKORO).unwrap());
}

// ---- Nemotron -------------------------------------------------------------------

struct FakeEnv {
    vars: HashMap<String, String>,
    home: Option<PathBuf>,
}

impl ModelEnv for FakeEnv {
    fn var(&self, key: &str) -> Option<String> {
        self.vars.get(key).cloned()
    }
    fn home_dir(&self) -> Option<PathBuf> {
        self.home.clone()
    }
}

fn write_nemotron(dir: &Path) {
    fs::create_dir_all(dir).unwrap();
    for f in [
        "encoder.int8.onnx",
        "decoder.int8.onnx",
        "joiner.int8.onnx",
        "tokens.txt",
    ] {
        fs::write(dir.join(f), "x").unwrap();
    }
}

#[test]
fn nemotron_lookup_order_and_completeness() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let models_dir = root.path().join("models");
    let env = FakeEnv {
        vars: HashMap::from([
            ("PASEO_NEMOTRON_MODEL_DIR".to_string(), "  ".to_string()),
            (
                "VOICESCRIBE_NEMOTRON_DIR".to_string(),
                root.path().join("scribe").to_string_lossy().into_owned(),
            ),
        ]),
        home: Some(home.clone()),
    };
    let candidates = candidate_nemotron_dirs(&env, Some(&models_dir));
    assert_eq!(
        candidates,
        vec![
            root.path().join("scribe"),
            home.join(".voicescribe/models").join(NEMOTRON_MODEL_ID),
            models_dir.join(NEMOTRON_MODEL_ID),
            home.join(".paseo/models/local-speech")
                .join(NEMOTRON_MODEL_ID),
        ]
    );

    assert!(resolve_installed_nemotron_model_with(&env, Some(&models_dir)).is_none());

    // an incomplete directory (missing joiner) is skipped
    let incomplete = models_dir.join(NEMOTRON_MODEL_ID);
    write_nemotron(&incomplete);
    fs::remove_file(incomplete.join("joiner.int8.onnx")).unwrap();
    assert!(resolve_installed_nemotron_model_with(&env, Some(&models_dir)).is_none());

    // an empty file does not count
    write_nemotron(&incomplete);
    fs::write(incomplete.join("tokens.txt"), "").unwrap();
    assert!(resolve_installed_nemotron_model_with(&env, Some(&models_dir)).is_none());

    // the legacy voicescribe copy wins over the models dir
    write_nemotron(&incomplete);
    write_nemotron(&home.join(".voicescribe/models").join(NEMOTRON_MODEL_ID));
    let found = resolve_installed_nemotron_model_with(&env, Some(&models_dir)).unwrap();
    assert_eq!(
        found.model_dir,
        home.join(".voicescribe/models").join(NEMOTRON_MODEL_ID)
    );
    assert_eq!(found.tokens_path, found.model_dir.join("tokens.txt"));
}

fn isolated_env(root: &Path) -> FakeEnv {
    FakeEnv {
        vars: HashMap::new(),
        home: Some(root.join("empty-home")),
    }
}

#[tokio::test]
async fn nemotron_downloads_only_missing_files_from_the_hf_layout() {
    let server = TestServer::start(|req| match req.path.rsplit('/').next().unwrap_or("") {
        "tokens.txt" => Reply::bytes(b"<blk> 0\n".to_vec()),
        "encoder.int8.onnx" | "decoder.int8.onnx" | "joiner.int8.onnx" => {
            Reply::bytes(vec![7u8; 64])
        }
        _ => Reply::json(404, "{}"),
    })
    .await;
    let base = format!("{}/resolve/main", server.base_url.trim_end_matches("/v1"));
    let root = tempfile::tempdir().unwrap();
    let models_dir = root.path().join("models");
    // Pre-seed one valid file and one zero-byte (interrupted) file.
    let target = models_dir.join(NEMOTRON_MODEL_ID);
    fs::create_dir_all(&target).unwrap();
    fs::write(target.join("tokens.txt"), "existing").unwrap();
    fs::write(target.join("encoder.int8.onnx"), "").unwrap();

    let resolved =
        ensure_vietnamese_sherpa_model_with(&isolated_env(root.path()), &base, &models_dir, None)
            .await
            .unwrap();
    assert_eq!(resolved.model_dir, target);
    assert_eq!(
        fs::read_to_string(target.join("tokens.txt")).unwrap(),
        "existing"
    );
    assert_eq!(
        fs::read(target.join("encoder.int8.onnx")).unwrap(),
        vec![7u8; 64]
    );
    let paths: Vec<String> = server.requests().iter().map(|r| r.path.clone()).collect();
    assert!(
        paths.iter().all(|p| p.starts_with("/resolve/main/")),
        "{paths:?}"
    );
    assert!(
        !paths.iter().any(|p| p.ends_with("tokens.txt")),
        "existing file must not be re-fetched: {paths:?}"
    );
    assert_eq!(paths.len(), 3);

    // Second call finds everything on disk and fetches nothing.
    ensure_vietnamese_sherpa_model_with(&isolated_env(root.path()), &base, &models_dir, None)
        .await
        .unwrap();
    assert_eq!(server.requests().len(), 3);
}

#[tokio::test]
async fn nemotron_download_failure_is_reported() {
    let server = TestServer::start(|_| Reply::json(500, "{}")).await;
    let base = format!("{}/resolve/main", server.base_url.trim_end_matches("/v1"));
    let root = tempfile::tempdir().unwrap();
    let err = ensure_vietnamese_sherpa_model_with(
        &isolated_env(root.path()),
        &base,
        &root.path().join("models"),
        None,
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("HTTP 500"), "{err}");
}

#[tokio::test]
async fn nemotron_reuses_a_model_installed_elsewhere_without_downloading() {
    let server = TestServer::start(|_| Reply::json(500, "{}")).await;
    let root = tempfile::tempdir().unwrap();
    let elsewhere = root.path().join("elsewhere");
    write_nemotron(&elsewhere);
    let env = FakeEnv {
        vars: HashMap::from([(
            "PASEO_NEMOTRON_MODEL_DIR".to_string(),
            elsewhere.to_string_lossy().into_owned(),
        )]),
        home: Some(root.path().join("empty-home")),
    };
    let found = ensure_vietnamese_sherpa_model_with(
        &env,
        &server.base_url,
        &root.path().join("models"),
        None,
    )
    .await
    .unwrap();
    assert_eq!(found.model_dir, elsewhere);
    assert!(server.requests().is_empty());
}
