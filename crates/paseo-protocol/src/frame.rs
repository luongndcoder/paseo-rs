use crate::generated::{session_inbound, session_outbound, ws_inbound, ws_outbound};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Client to daemon.
    Inbound,
    /// Daemon to client.
    Outbound,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Top-level WebSocket envelope message.
    Ws,
    /// Message carried inside a `session` envelope.
    Session,
}

/// Classification of a frame that a generated struct accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Typed {
    pub direction: Direction,
    pub scope: Scope,
    pub type_: String,
}

/// Any wire frame. The original JSON is kept as-is, so reserializing is lossless even when the
/// frame carries fields or variants the generated types do not know. `typed()` reports whether a
/// generated struct accepts it; decode the concrete struct from `value()` when you need it.
#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    value: Value,
    typed: Option<Typed>,
}

impl Frame {
    pub fn from_value(value: Value) -> Self {
        let typed = classify(&value);
        Frame { value, typed }
    }

    pub fn from_slice(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        Ok(Self::from_value(serde_json::from_slice(bytes)?))
    }

    pub fn value(&self) -> &Value {
        &self.value
    }

    pub fn into_value(self) -> Value {
        self.value
    }

    pub fn typed(&self) -> Option<&Typed> {
        self.typed.as_ref()
    }

    pub fn is_typed(&self) -> bool {
        self.typed.is_some()
    }

    pub fn to_vec(&self) -> Vec<u8> {
        serde_json::to_vec(&self.value).expect("Value serializes")
    }
}

fn type_of(v: &Value) -> Option<&str> {
    v.get("type")?.as_str()
}

fn classify(value: &Value) -> Option<Typed> {
    let ty = type_of(value)?;
    if ty == "session" {
        let msg = value.get("message")?;
        let inner = type_of(msg)?;
        // The envelope's own fields are not generated (the union is owned by session_*).
        if !msg.is_object() {
            return None;
        }
        let (direction, _) = pick(
            session_inbound::validate(inner, msg),
            session_outbound::validate(inner, msg),
        )?;
        return Some(Typed {
            direction,
            scope: Scope::Session,
            type_: inner.to_string(),
        });
    }
    let (direction, _) = pick(
        ws_inbound::validate(ty, value),
        ws_outbound::validate(ty, value),
    )?;
    Some(Typed {
        direction,
        scope: Scope::Ws,
        type_: ty.to_string(),
    })
}

fn pick(inb: Option<bool>, out: Option<bool>) -> Option<(Direction, ())> {
    if inb == Some(true) {
        Some((Direction::Inbound, ()))
    } else if out == Some(true) {
        Some((Direction::Outbound, ()))
    } else {
        None
    }
}

impl<'de> Deserialize<'de> for Frame {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(Frame::from_value(Value::deserialize(d)?))
    }
}

impl Serialize for Frame {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.value.serialize(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_frames_roundtrip() {
        let raw = r#"{"type":"from_the_future","x":[1,2.5,null],"nested":{"a":"b"}}"#;
        let f: Frame = serde_json::from_str(raw).unwrap();
        assert!(!f.is_typed());
        let back: Value = serde_json::from_str(&serde_json::to_string(&f).unwrap()).unwrap();
        assert_eq!(back, serde_json::from_str::<Value>(raw).unwrap());
    }

    #[test]
    fn ping_is_typed() {
        let f = Frame::from_slice(br#"{"type":"ping","extra":true}"#).unwrap();
        let t = f.typed().expect("typed");
        assert_eq!((t.scope, t.type_.as_str()), (Scope::Ws, "ping"));
        assert_eq!(f.value()["extra"], true);
    }

    #[test]
    fn non_object_frames_do_not_error() {
        for raw in ["1", "\"s\"", "null", "[1]"] {
            assert!(!serde_json::from_str::<Frame>(raw).unwrap().is_typed());
        }
    }
}
