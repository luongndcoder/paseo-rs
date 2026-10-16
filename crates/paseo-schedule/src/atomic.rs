//! Thin adapter over `paseo-fs-atomic` (parity fsync policy: off) that keeps this crate's
//! `Result` type. The implementation lives in `paseo-fs-atomic`.

use serde::Serialize;
use std::path::Path;

pub use paseo_fs_atomic::FsyncPolicy;

use crate::error::Result;

pub fn write_file_atomic(path: &Path, data: &[u8]) -> Result<()> {
    Ok(paseo_fs_atomic::write_file_atomic(
        path,
        data,
        FsyncPolicy::Off,
    )?)
}

/// `JSON.stringify(value, null, 2)`: two-space indent, no trailing newline.
pub fn write_json_file_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    Ok(paseo_fs_atomic::write_json_file_atomic(
        path,
        value,
        FsyncPolicy::Off,
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    #[test]
    fn writes_pretty_json_without_trailing_newline_and_leaves_no_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("nested").join("a.json");
        write_json_file_atomic(&target, &json!({"a": [], "b": {"c": 1}})).unwrap();
        let text = fs::read_to_string(&target).unwrap();
        assert_eq!(text, "{\n  \"a\": [],\n  \"b\": {\n    \"c\": 1\n  }\n}");
        write_json_file_atomic(&target, &json!({"a": 2})).unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "{\n  \"a\": 2\n}");
        let leftovers: Vec<_> = fs::read_dir(target.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(leftovers, vec!["a.json".to_string()]);
    }
}
