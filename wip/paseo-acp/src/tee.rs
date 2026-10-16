//! JSONL traffic recorder, same format as `tools/capture/tee-acp.mjs`:
//! header `{argv, env_keys}`, then `{dir:"in"|"out", line}` per frame, then `{exit}`.
//! `in` is client -> agent, `out` is agent -> client. Env values are never recorded.

use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

#[derive(Clone)]
pub struct Tee {
    file: Arc<Mutex<File>>,
}

impl Tee {
    pub fn create(
        path: impl AsRef<Path>,
        argv: &[String],
        env_keys: &[String],
    ) -> std::io::Result<Self> {
        let file = File::create(path)?;
        let tee = Tee {
            file: Arc::new(Mutex::new(file)),
        };
        tee.write(&json!({ "argv": argv, "env_keys": env_keys }));
        Ok(tee)
    }

    pub fn log_in(&self, frame: &Value) {
        self.write(&json!({ "dir": "in", "line": frame }));
    }

    pub fn log_out(&self, frame: &Value) {
        self.write(&json!({ "dir": "out", "line": frame }));
    }

    pub fn log_exit(&self, code: Option<i32>, signal: Option<i32>) {
        self.write(&json!({ "exit": { "code": code, "signal": signal } }));
    }

    fn write(&self, entry: &Value) {
        if let Ok(mut f) = self.file.lock() {
            let _ = writeln!(f, "{entry}");
        }
    }
}
