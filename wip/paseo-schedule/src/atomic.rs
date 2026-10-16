//! Port of `atomic-file.ts`: write to a unique sibling temp file, then rename over
//! the target. Readers never observe a partially written file.

use serde::Serialize;
use std::fs;
use std::path::Path;

use crate::error::{Result, ScheduleError};

pub fn write_file_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(dir)?;
    let base = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let temp = dir.join(format!(
        ".{}.{}.{}.{}.tmp",
        base,
        std::process::id(),
        crate::time::now_ms(),
        uuid::Uuid::new_v4()
    ));
    let result = fs::write(&temp, data).and_then(|_| fs::rename(&temp, path));
    if let Err(e) = result {
        let _ = fs::remove_file(&temp);
        return Err(ScheduleError::from(e));
    }
    Ok(())
}

/// `JSON.stringify(value, null, 2)`: two-space indent, no trailing newline.
pub fn write_json_file_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let text = serde_json::to_string_pretty(value).map_err(|e| ScheduleError::Io(e.to_string()))?;
    write_file_atomic(path, text.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
