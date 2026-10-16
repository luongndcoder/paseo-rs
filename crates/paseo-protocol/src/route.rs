use serde::Deserialize;

/// Routing keys of a text frame: the top-level `type` and, for `session` envelopes,
/// the inner `message.type`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    pub type_: String,
    pub session_type: Option<String>,
}

#[derive(Deserialize)]
struct Peek {
    #[serde(rename = "type", default, deserialize_with = "string_or_none")]
    type_: Option<String>,
    #[serde(default, deserialize_with = "message_type")]
    message: Option<String>,
}

#[derive(Deserialize)]
struct Inner {
    #[serde(rename = "type", default, deserialize_with = "string_or_none")]
    type_: Option<String>,
}

fn string_or_none<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum V {
        S(String),
        Other(serde::de::IgnoredAny),
    }
    Ok(match V::deserialize(d)? {
        V::S(s) => Some(s),
        V::Other(_) => None,
    })
}

fn message_type<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum V {
        Obj(Inner),
        Other(serde::de::IgnoredAny),
    }
    Ok(match V::deserialize(d)? {
        V::Obj(i) => i.type_,
        V::Other(_) => None,
    })
}

/// Extracts the routing keys without building a `serde_json::Value`; every other field is
/// skipped. Returns `None` for non-JSON input (e.g. binary frames) or a missing string `type`.
pub fn peek_route(bytes: &[u8]) -> Option<Route> {
    let peek: Peek = serde_json::from_slice(bytes).ok()?;
    let type_ = peek.type_?;
    let session_type = if type_ == "session" {
        peek.message
    } else {
        None
    };
    Some(Route {
        type_,
        session_type,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_type() {
        let r = peek_route(br#"{"type":"ping"}"#).unwrap();
        assert_eq!(
            r,
            Route {
                type_: "ping".into(),
                session_type: None
            }
        );
    }

    #[test]
    fn session_envelope() {
        let r = peek_route(
            br#"{"big":[1,2,{"x":null}],"type":"session","message":{"type":"agent_stream","payload":{"a":1}}}"#,
        )
        .unwrap();
        assert_eq!(r.type_, "session");
        assert_eq!(r.session_type.as_deref(), Some("agent_stream"));
    }

    #[test]
    fn session_type_ignored_for_non_session() {
        let r = peek_route(br#"{"type":"hello","message":{"type":"x"}}"#).unwrap();
        assert_eq!(r.session_type, None);
    }

    #[test]
    fn missing_or_invalid() {
        assert!(peek_route(b"{}").is_none());
        assert!(peek_route(br#"{"type":5}"#).is_none());
        assert!(peek_route(b"not json").is_none());
        assert!(peek_route(&[0, 1, 2, 3]).is_none());
        assert!(peek_route(b"").is_none());
        assert!(peek_route(b"[1,2]").is_none());
    }

    #[test]
    fn session_without_message() {
        let r = peek_route(br#"{"type":"session"}"#).unwrap();
        assert_eq!(r.session_type, None);
        let r = peek_route(br#"{"type":"session","message":"str"}"#).unwrap();
        assert_eq!(r.session_type, None);
    }
}
