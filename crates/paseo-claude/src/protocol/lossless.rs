//! Helpers that keep the wire types lossless.
//!
//! Every typed struct carries a flattened `extra` map so unknown fields survive a
//! deserialize/serialize round trip. Every tagged enum ends with an untagged
//! `Unknown(Value)` variant so unknown tags (or known tags whose fields do not fit) never
//! produce an error. [`parse_lossless`] closes the remaining gap (null vs absent, int vs
//! float): if the typed value does not serialize back to the exact input, the input is kept
//! as `Unknown(Value)`.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};

/// Unknown fields captured by `#[serde(flatten)]`.
pub type Extra = Map<String, Value>;

/// Enums with an `Unknown(Value)` escape hatch.
pub trait OpenEnum: Sized {
    fn unknown(value: Value) -> Self;
}

/// Deserialize `value` into `T`, falling back to `T::unknown(value)` when the typed form
/// is missing information (it does not reserialize to exactly `value`).
pub fn parse_lossless<T>(value: Value) -> T
where
    T: DeserializeOwned + Serialize + OpenEnum,
{
    match serde_json::from_value::<T>(value.clone()) {
        Ok(typed) => match serde_json::to_value(&typed) {
            Ok(back) if back == value => typed,
            _ => T::unknown(value),
        },
        Err(_) => T::unknown(value),
    }
}

/// Parse one JSON text. Errors only when `line` is not valid JSON.
pub fn parse_line_lossless<T>(line: &str) -> Result<T, serde_json::Error>
where
    T: DeserializeOwned + Serialize + OpenEnum,
{
    Ok(parse_lossless(serde_json::from_str::<Value>(line)?))
}

/// Serde adapter for `Option<Option<T>>`: outer `None` = key absent, `Some(None)` = `null`.
/// Use with `#[serde(default, with = "double_option", skip_serializing_if = "Option::is_none")]`.
pub mod double_option {
    use super::*;

    pub fn deserialize<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
    where
        D: Deserializer<'de>,
        T: Deserialize<'de>,
    {
        Option::<T>::deserialize(d).map(Some)
    }

    pub fn serialize<S, T>(value: &Option<Option<T>>, s: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
        T: Serialize,
    {
        match value {
            Some(inner) => inner.serialize(s),
            None => s.serialize_none(),
        }
    }
}

/// Flatten `Option<Option<T>>` to `Option<&T>`.
pub fn opt_ref<T>(v: &Option<Option<T>>) -> Option<&T> {
    v.as_ref().and_then(|inner| inner.as_ref())
}
