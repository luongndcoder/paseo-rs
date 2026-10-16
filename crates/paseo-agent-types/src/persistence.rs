//! `AgentPersistenceHandle`.

use crate::json::*;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub struct PersistenceHandle {
    pub provider: String,
    pub session_id: String,
    /// Provider specific handle (Codex thread id, Claude resume token, etc).
    pub native_handle: Option<String>,
    pub metadata: Option<Map>,
}

impl PersistenceHandle {
    pub fn to_value(&self) -> Value {
        let mut m = Map::new();
        put(&mut m, "provider", self.provider.clone());
        put(&mut m, "sessionId", self.session_id.clone());
        put_opt_str(&mut m, "nativeHandle", &self.native_handle);
        put_opt_map(&mut m, "metadata", &self.metadata);
        Value::Object(m)
    }

    pub fn from_value(v: &Value) -> Result<Self> {
        let o = Obj::new(v, "persistence handle")?;
        Ok(PersistenceHandle {
            provider: o.req_str("provider")?,
            session_id: o.req_str("sessionId")?,
            native_handle: o.opt_str("nativeHandle")?,
            metadata: o.opt_map("metadata")?,
        })
    }
}

impl_serde_via_value!(PersistenceHandle);
