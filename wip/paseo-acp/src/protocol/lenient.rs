use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Typed value with a raw fallback. A payload that does not match `T` (new variant, extra
/// shape from a vendor agent) deserializes to `Unknown(raw)` and serializes back unchanged,
/// so unknown traffic is never a hard error.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Lenient<T> {
    Known(T),
    Unknown(Value),
}

impl<T> Lenient<T> {
    pub fn known(&self) -> Option<&T> {
        match self {
            Lenient::Known(t) => Some(t),
            Lenient::Unknown(_) => None,
        }
    }

    pub fn unknown(&self) -> Option<&Value> {
        match self {
            Lenient::Known(_) => None,
            Lenient::Unknown(v) => Some(v),
        }
    }
}

impl<T> From<T> for Lenient<T> {
    fn from(t: T) -> Self {
        Lenient::Known(t)
    }
}
