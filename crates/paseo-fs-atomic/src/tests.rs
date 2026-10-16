use super::*;
use serde_json::{json, Value};

const ALL: [FsyncPolicy; 3] = [FsyncPolicy::Off, FsyncPolicy::File, FsyncPolicy::FileAndDir];

fn names(dir: &Path) -> Vec<String> {
    let mut v: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

#[test]
fn writes_pretty_json_without_trailing_newline_and_leaves_no_temp_files() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("nested").join("a.json");
    for policy in ALL {
        write_json_file_atomic(&target, &json!({"a": [], "b": {"c": 1}}), policy).unwrap();
        assert_eq!(
            fs::read_to_string(&target).unwrap(),
            "{\n  \"a\": [],\n  \"b\": {\n    \"c\": 1\n  }\n}"
        );
        write_json_file_atomic(&target, &json!({"a": 2}), policy).unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "{\n  \"a\": 2\n}");
        assert_eq!(names(target.parent().unwrap()), vec!["a.json"]);
    }
}

#[test]
fn preserves_key_insertion_order() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("o.json");
    let value: Value = serde_json::from_str(r#"{"z":1,"a":2,"m":{"b":1,"a":2}}"#).unwrap();
    write_json_file_atomic(&target, &value, FsyncPolicy::Off).unwrap();
    assert_eq!(
        fs::read_to_string(&target).unwrap(),
        "{\n  \"z\": 1,\n  \"a\": 2,\n  \"m\": {\n    \"b\": 1,\n    \"a\": 2\n  }\n}"
    );
}

#[test]
fn raw_bytes_round_trip_and_overwrite() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("b.bin");
    write_file_atomic(&target, &[0, 255, 1], FsyncPolicy::File).unwrap();
    write_file_atomic(&target, b"x", FsyncPolicy::File).unwrap();
    assert_eq!(fs::read(&target).unwrap(), b"x");
}

#[test]
fn bare_relative_filename_uses_current_dir_parent() {
    assert_eq!(parent_dir(Path::new("a.json")), Path::new("."));
}

#[test]
fn temp_name_matches_ts_shape() {
    let t = temp_path(Path::new("/d/a.json"));
    let name = t.file_name().unwrap().to_string_lossy().into_owned();
    let re_parts: Vec<_> = name.split('.').collect();
    // "", "a", "json", pid, ms, uuid-with-dashes (no dots), "tmp"
    assert_eq!(re_parts[0], "");
    assert_eq!(&re_parts[1..3], ["a", "json"]);
    assert_eq!(re_parts[3], std::process::id().to_string());
    assert!(re_parts[4].parse::<u128>().is_ok());
    assert_eq!(re_parts[5].len(), 36);
    assert_eq!(re_parts[6], "tmp");
    assert_eq!(t.parent().unwrap(), Path::new("/d"));
}

#[test]
fn failed_rename_cleans_temp_and_keeps_target() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("t");
    fs::create_dir(&target).unwrap();
    fs::write(target.join("child"), b"keep").unwrap();
    assert!(write_file_atomic(&target, b"data", FsyncPolicy::Off).is_err());
    assert_eq!(names(dir.path()), vec!["t"]);
    assert_eq!(fs::read(target.join("child")).unwrap(), b"keep");
}

#[test]
fn quarantine_renames_and_avoids_collisions() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("projects.json");
    fs::write(&p, b"{bad").unwrap();
    let q1 = quarantine_corrupt(&p).unwrap();
    assert!(!p.exists());
    assert!(q1
        .file_name()
        .unwrap()
        .to_string_lossy()
        .starts_with("projects.json.corrupt-"));
    assert_eq!(fs::read(&q1).unwrap(), b"{bad");
    fs::write(&p, b"{bad2").unwrap();
    let q2 = quarantine_corrupt(&p).unwrap();
    assert_ne!(q1, q2);
    assert_eq!(fs::read(&q1).unwrap(), b"{bad");
    assert!(quarantine_corrupt(&p).is_err());
}

#[cfg(unix)]
mod unix {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};

    fn mode(p: &Path) -> u32 {
        fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn private_write_sets_modes_and_creates_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("sub").join("hub.json");
        write_private_atomic(&target, b"one").unwrap();
        write_private_atomic(&target, b"two").unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"two");
        assert_eq!(mode(&target), 0o600);
        assert_eq!(mode(target.parent().unwrap()), 0o700);
        assert_eq!(names(target.parent().unwrap()), vec!["hub.json"]);
    }

    #[test]
    fn private_write_tightens_existing_loose_file() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("f");
        fs::write(&target, b"old").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).unwrap();
        write_private_atomic(&target, b"new").unwrap();
        assert_eq!(mode(&target), 0o600);
    }

    #[test]
    fn private_write_follows_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.json");
        let link = dir.path().join("link.json");
        fs::write(&real, b"old").unwrap();
        symlink(&real, &link).unwrap();
        write_private_atomic(&link, b"new").unwrap();
        assert!(fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::read(&real).unwrap(), b"new");
    }

    #[test]
    fn private_write_dangling_symlink_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("link.json");
        symlink(dir.path().join("missing"), &link).unwrap();
        write_private_atomic(&link, b"new").unwrap();
        assert!(!fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::read(&link).unwrap(), b"new");
    }

    /// Child half of the crash test: stage the temp file, then die before the rename.
    #[test]
    fn crash_child() {
        let Ok(target) = std::env::var("FS_ATOMIC_CRASH_TARGET") else {
            return;
        };
        let target = PathBuf::from(target);
        let temp = temp_path(&target);
        write_temp(&temp, &vec![b'x'; 1 << 20], FsyncPolicy::Off).unwrap();
        std::process::abort();
    }

    #[test]
    fn kill_before_rename_leaves_target_intact() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("t.json");
        write_file_atomic(&target, b"{\"ok\":true}", FsyncPolicy::Off).unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "tests::unix::crash_child", "--nocapture"])
            .env("FS_ATOMIC_CRASH_TARGET", &target)
            .status()
            .unwrap();
        assert!(!status.success(), "child must abort");
        assert_eq!(fs::read(&target).unwrap(), b"{\"ok\":true}");
        let leftovers: Vec<_> = names(dir.path())
            .into_iter()
            .filter(|n| n != "t.json")
            .collect();
        assert_eq!(leftovers.len(), 1);
        assert!(leftovers[0].starts_with(".t.json.") && leftovers[0].ends_with(".tmp"));
    }
}

fn run_vectors(path: &Path) -> usize {
    let cases: Vec<Value> = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    for case in &cases {
        let name = case["name"].as_str().unwrap();
        let target = dir.path().join(format!("{name}.json"));
        write_json_file_atomic(&target, &case["value"], FsyncPolicy::Off).unwrap();
        assert_eq!(
            fs::read_to_string(&target).unwrap(),
            case["expected"].as_str().unwrap(),
            "vector {name}"
        );
    }
    cases.len()
}

/// Vectors captured from TS `JSON.stringify(v, null, 2)` (checked in with this crate).
#[test]
fn ts_json_stringify_vectors() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/json-vectors.json");
    assert_eq!(run_vectors(&path), 7);
}

/// WP0 golden, same format as `json-vectors.json`; skipped while the fixture is absent.
#[test]
fn wp0_golden_vectors() {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/p4/fs-atomic/json-vectors.json");
    if !path.exists() {
        eprintln!("SKIP wp0_golden_vectors: {} missing", path.display());
        return;
    }
    assert!(run_vectors(&path) > 0);
}
