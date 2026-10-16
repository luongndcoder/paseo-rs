//! Small helpers for the hand-written serde of this crate.
//!
//! Every type exposes `to_value`/`from_value` and delegates `Serialize`/`Deserialize` to them.
//! Optional fields are *absent* in JSON (never `null`) unless the TS type says `T | null`;
//! `Option<Option<T>>` models `field?: T | null` (absent, null, or a value).

use serde_json::{Map as JsonMap, Number, Value};
use std::fmt;

pub type Map = JsonMap<String, Value>;
pub type Num = Number;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodeError(pub String);

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for DecodeError {}

pub type Result<T> = std::result::Result<T, DecodeError>;

pub(crate) fn err<T>(msg: impl Into<String>) -> Result<T> {
    Err(DecodeError(msg.into()))
}

/// Read-side view over a JSON object with a context label for error messages.
pub(crate) struct Obj<'a> {
    pub map: &'a Map,
    pub ctx: &'static str,
}

impl<'a> Obj<'a> {
    pub fn new(v: &'a Value, ctx: &'static str) -> Result<Self> {
        match v {
            Value::Object(map) => Ok(Obj { map, ctx }),
            _ => err(format!("{}: expected object", ctx)),
        }
    }

    fn bad<T>(&self, key: &str, want: &str) -> Result<T> {
        err(format!("{}.{}: expected {}", self.ctx, key, want))
    }

    pub fn get(&self, key: &str) -> Option<&'a Value> {
        self.map.get(key)
    }

    pub fn req_str(&self, key: &str) -> Result<String> {
        match self.map.get(key) {
            Some(Value::String(s)) => Ok(s.clone()),
            _ => self.bad(key, "string"),
        }
    }

    pub fn opt_str(&self, key: &str) -> Result<Option<String>> {
        match self.map.get(key) {
            None => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.clone())),
            _ => self.bad(key, "string"),
        }
    }

    /// `key?: string | null`
    pub fn opt_nullable_str(&self, key: &str) -> Result<Option<Option<String>>> {
        match self.map.get(key) {
            None => Ok(None),
            Some(Value::Null) => Ok(Some(None)),
            Some(Value::String(s)) => Ok(Some(Some(s.clone()))),
            _ => self.bad(key, "string | null"),
        }
    }

    /// `key: string | null` (required)
    pub fn req_nullable_str(&self, key: &str) -> Result<Option<String>> {
        match self.map.get(key) {
            Some(Value::Null) => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.clone())),
            _ => self.bad(key, "string | null"),
        }
    }

    pub fn req_bool(&self, key: &str) -> Result<bool> {
        match self.map.get(key) {
            Some(Value::Bool(b)) => Ok(*b),
            _ => self.bad(key, "boolean"),
        }
    }

    pub fn opt_bool(&self, key: &str) -> Result<Option<bool>> {
        match self.map.get(key) {
            None => Ok(None),
            Some(Value::Bool(b)) => Ok(Some(*b)),
            _ => self.bad(key, "boolean"),
        }
    }

    pub fn req_num(&self, key: &str) -> Result<Num> {
        match self.map.get(key) {
            Some(Value::Number(n)) => Ok(n.clone()),
            _ => self.bad(key, "number"),
        }
    }

    pub fn opt_num(&self, key: &str) -> Result<Option<Num>> {
        match self.map.get(key) {
            None => Ok(None),
            Some(Value::Number(n)) => Ok(Some(n.clone())),
            _ => self.bad(key, "number"),
        }
    }

    /// `key?: number | null`
    pub fn opt_nullable_num(&self, key: &str) -> Result<Option<Option<Num>>> {
        match self.map.get(key) {
            None => Ok(None),
            Some(Value::Null) => Ok(Some(None)),
            Some(Value::Number(n)) => Ok(Some(Some(n.clone()))),
            _ => self.bad(key, "number | null"),
        }
    }

    pub fn req_nullable_num(&self, key: &str) -> Result<Option<Num>> {
        match self.map.get(key) {
            Some(Value::Null) => Ok(None),
            Some(Value::Number(n)) => Ok(Some(n.clone())),
            _ => self.bad(key, "number | null"),
        }
    }

    pub fn opt_map(&self, key: &str) -> Result<Option<Map>> {
        match self.map.get(key) {
            None => Ok(None),
            Some(Value::Object(m)) => Ok(Some(m.clone())),
            _ => self.bad(key, "object"),
        }
    }

    pub fn req_array(&self, key: &str) -> Result<&'a Vec<Value>> {
        match self.map.get(key) {
            Some(Value::Array(a)) => Ok(a),
            _ => self.bad(key, "array"),
        }
    }

    pub fn opt_array(&self, key: &str) -> Result<Option<&'a Vec<Value>>> {
        match self.map.get(key) {
            None => Ok(None),
            Some(Value::Array(a)) => Ok(Some(a)),
            _ => self.bad(key, "array"),
        }
    }

    pub fn opt_str_array(&self, key: &str) -> Result<Option<Vec<String>>> {
        match self.opt_array(key)? {
            None => Ok(None),
            Some(a) => a
                .iter()
                .map(|v| match v {
                    Value::String(s) => Ok(s.clone()),
                    _ => self.bad(key, "string[]"),
                })
                .collect::<Result<Vec<_>>>()
                .map(Some),
        }
    }

    /// Keys of this object that are not in `known`, in input order.
    pub fn extras(&self, known: &[&str]) -> Map {
        self.map
            .iter()
            .filter(|(k, _)| !known.contains(&k.as_str()))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }
}

/// Write-side helpers. Insertion order = serialization order.
pub(crate) fn put<V: Into<Value>>(m: &mut Map, key: &str, v: V) {
    m.insert(key.to_string(), v.into());
}

pub(crate) fn put_opt<V: Into<Value>>(m: &mut Map, key: &str, v: Option<V>) {
    if let Some(v) = v {
        m.insert(key.to_string(), v.into());
    }
}

pub(crate) fn put_opt_str(m: &mut Map, key: &str, v: &Option<String>) {
    if let Some(s) = v {
        m.insert(key.to_string(), Value::String(s.clone()));
    }
}

pub(crate) fn put_opt_nullable_str(m: &mut Map, key: &str, v: &Option<Option<String>>) {
    match v {
        None => {}
        Some(None) => {
            m.insert(key.to_string(), Value::Null);
        }
        Some(Some(s)) => {
            m.insert(key.to_string(), Value::String(s.clone()));
        }
    }
}

pub(crate) fn put_opt_num(m: &mut Map, key: &str, v: &Option<Num>) {
    if let Some(n) = v {
        m.insert(key.to_string(), Value::Number(n.clone()));
    }
}

pub(crate) fn put_opt_nullable_num(m: &mut Map, key: &str, v: &Option<Option<Num>>) {
    match v {
        None => {}
        Some(None) => {
            m.insert(key.to_string(), Value::Null);
        }
        Some(Some(n)) => {
            m.insert(key.to_string(), Value::Number(n.clone()));
        }
    }
}

pub(crate) fn put_opt_bool(m: &mut Map, key: &str, v: Option<bool>) {
    if let Some(b) = v {
        m.insert(key.to_string(), Value::Bool(b));
    }
}

pub(crate) fn put_opt_map(m: &mut Map, key: &str, v: &Option<Map>) {
    if let Some(x) = v {
        m.insert(key.to_string(), Value::Object(x.clone()));
    }
}

pub(crate) fn str_array(v: &[String]) -> Value {
    Value::Array(v.iter().cloned().map(Value::String).collect())
}

/// Implements `Serialize`/`Deserialize` for a type with `to_value`/`from_value`.
macro_rules! impl_serde_via_value {
    ($t:ty) => {
        impl serde::Serialize for $t {
            fn serialize<S: serde::Serializer>(
                &self,
                s: S,
            ) -> std::result::Result<S::Ok, S::Error> {
                self.to_value().serialize(s)
            }
        }
        impl<'de> serde::Deserialize<'de> for $t {
            fn deserialize<D: serde::Deserializer<'de>>(
                d: D,
            ) -> std::result::Result<Self, D::Error> {
                let v = <serde_json::Value as serde::Deserialize>::deserialize(d)?;
                <$t>::from_value(&v).map_err(serde::de::Error::custom)
            }
        }
    };
}
pub(crate) use impl_serde_via_value;
