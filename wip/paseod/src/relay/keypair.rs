//! `$PASEO_HOME/daemon-keypair.json` reader (`daemon-keypair.ts`). TS is the only writer.

use std::path::Path;

use serde::Deserialize;

use super::b64;
use crate::error::{EdgeError, Result};

pub const KEYPAIR_FILENAME: &str = "daemon-keypair.json";

/// Daemon X25519 keypair as stored by TS (`daemon-keypair.ts`).
#[derive(Clone)]
pub struct DaemonKeyPair {
    pub public_key: [u8; 32],
    pub secret_key: [u8; 32],
}

impl std::fmt::Debug for DaemonKeyPair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DaemonKeyPair")
            .field("secret_key", &"<redacted>")
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
struct StoredKeyPair {
    v: u8,
    #[serde(rename = "publicKeyB64")]
    public_key_b64: String,
    #[serde(rename = "secretKeyB64")]
    secret_key_b64: String,
}

/// Parses the file body. Error messages never include key material.
pub fn parse_keypair(contents: &str) -> Result<DaemonKeyPair> {
    let stored: StoredKeyPair = serde_json::from_str(contents).map_err(|e| {
        EdgeError::Relay(format!(
            "daemon keypair: invalid JSON shape ({})",
            e.classify_name()
        ))
    })?;
    if stored.v != 2 {
        return Err(EdgeError::Relay(format!(
            "daemon keypair: unsupported version {}",
            stored.v
        )));
    }
    let public_key = b64::decode_public_key(&stored.public_key_b64)
        .map_err(|e| EdgeError::Relay(format!("daemon keypair: public key: {e}")))?;
    let secret_key = b64::decode_secret_key(&stored.secret_key_b64)
        .map_err(|e| EdgeError::Relay(format!("daemon keypair: secret key: {e}")))?;
    Ok(DaemonKeyPair {
        public_key,
        secret_key,
    })
}

trait ClassifyName {
    fn classify_name(&self) -> &'static str;
}

impl ClassifyName for serde_json::Error {
    fn classify_name(&self) -> &'static str {
        use serde_json::error::Category;
        match self.classify() {
            Category::Io => "io",
            Category::Syntax => "syntax",
            Category::Data => "missing or mistyped field",
            Category::Eof => "truncated",
        }
    }
}

/// Loads `daemon-keypair.json` from `paseo_home`. Never creates or rewrites it.
pub fn load_daemon_keypair(paseo_home: &Path) -> Result<DaemonKeyPair> {
    let path = paseo_home.join(KEYPAIR_FILENAME);
    let contents = std::fs::read_to_string(&path).map_err(|e| {
        EdgeError::Relay(format!(
            "daemon keypair: cannot read {}: {e}",
            path.display()
        ))
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(&path) {
            if meta.permissions().mode() & 0o077 != 0 {
                tracing::warn!(path = %path.display(), "daemon keypair file is group/other accessible (expected 0600)");
            }
        }
    }
    parse_keypair(&contents)
}
