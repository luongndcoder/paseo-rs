//! SSE events from `GET /global/event`.
//!
//! Wire shape: `{"directory": "...", "project": "...", "payload": {"id": "evt_..", "type": "..", "properties": {..}}}`
//! (`server.connected` / `server.heartbeat` omit `directory` and `project`).

use super::types::*;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Number, Value};

wire! {
    pub struct SessionRef {
        #[serde(rename = "sessionID")]
        pub session_id: String,
    }
}

wire! {
    /// `session.created`, `session.updated`, `session.deleted`.
    pub struct SessionInfoEvent {
        #[serde(rename = "sessionID")]
        pub session_id: String,
        pub info: Option<Session>,
    }
}

wire! {
    pub struct SessionStatusEvent {
        #[serde(rename = "sessionID")]
        pub session_id: String,
        pub status: Option<SessionStatusInfo>,
    }
}

wire! {
    pub struct SessionErrorEvent {
        #[serde(rename = "sessionID", default, skip_serializing_if = "Option::is_none")]
        pub session_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub error: Option<Value>,
    }
}

wire! {
    pub struct MessageUpdatedEvent {
        #[serde(rename = "sessionID")]
        pub session_id: String,
        pub info: Option<MessageInfo>,
    }
}

wire! {
    pub struct PartUpdatedEvent {
        #[serde(rename = "sessionID")]
        pub session_id: String,
        pub part: Option<Part>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub time: Option<Number>,
    }
}

wire! {
    pub struct PartDeltaEvent {
        #[serde(rename = "sessionID")]
        pub session_id: String,
        #[serde(rename = "messageID")]
        pub message_id: String,
        #[serde(rename = "partID")]
        pub part_id: String,
        pub field: String,
        pub delta: String,
    }
}

wire! {
    /// `message.removed` / `message.part.removed`.
    pub struct RemovedEvent {
        #[serde(rename = "sessionID")]
        pub session_id: String,
        #[serde(rename = "messageID", default, skip_serializing_if = "Option::is_none")]
        pub message_id: Option<String>,
        #[serde(rename = "partID", default, skip_serializing_if = "Option::is_none")]
        pub part_id: Option<String>,
    }
}

wire! {
    pub struct PermissionRepliedEvent {
        #[serde(rename = "sessionID")]
        pub session_id: String,
        #[serde(rename = "requestID")]
        pub request_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub reply: Option<String>,
    }
}

wire! {
    /// `question.replied` / `question.rejected`.
    pub struct QuestionResolvedEvent {
        #[serde(rename = "sessionID")]
        pub session_id: String,
        #[serde(rename = "requestID")]
        pub request_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub answers: Option<Value>,
    }
}

wire! {
    pub struct TodoUpdatedEvent {
        #[serde(rename = "sessionID")]
        pub session_id: String,
        #[serde(default)]
        pub todos: Vec<Value>,
    }
}

wire! {
    pub struct SessionDiffEvent {
        #[serde(rename = "sessionID")]
        pub session_id: String,
        #[serde(default)]
        pub diff: Vec<Value>,
    }
}

macro_rules! events {
    (
        data { $( $name:literal => $variant:ident($ty:ty) ),* $(,)? }
        unit { $( $uname:literal => $uvariant:ident ),* $(,)? }
    ) => {
        /// A typed event. Anything unrecognised or not parseable is `Unknown`, never an error.
        #[derive(Debug, Clone, PartialEq)]
        pub enum Event {
            $( $variant($ty), )*
            $( $uvariant, )*
            /// `type` we have no struct for (or whose `properties` failed to parse).
            Unknown { ty: String, properties: Option<Value> },
            /// Payload that is not an object with a string `type`.
            Malformed(Value),
        }

        impl Event {
            pub fn type_name(&self) -> &str {
                match self {
                    $( Event::$variant(_) => $name, )*
                    $( Event::$uvariant => $uname, )*
                    Event::Unknown { ty, .. } => ty,
                    Event::Malformed(_) => "",
                }
            }

            fn parse(ty: &str, properties: Option<&Value>) -> Event {
                let unknown = || Event::Unknown { ty: ty.to_owned(), properties: properties.cloned() };
                match ty {
                    $( $name => match properties.cloned().map(serde_json::from_value::<$ty>) {
                        Some(Ok(p)) => Event::$variant(p),
                        _ => unknown(),
                    }, )*
                    $( $uname => Event::$uvariant, )*
                    _ => unknown(),
                }
            }

            fn properties(&self) -> Option<Value> {
                match self {
                    $( Event::$variant(p) => serde_json::to_value(p).ok(), )*
                    $( Event::$uvariant => Some(Value::Object(Map::new())), )*
                    Event::Unknown { properties, .. } => properties.clone(),
                    Event::Malformed(_) => None,
                }
            }
        }
    };
}

events! {
    data {
        "session.created" => SessionCreated(SessionInfoEvent),
        "session.updated" => SessionUpdated(SessionInfoEvent),
        "session.deleted" => SessionDeleted(SessionInfoEvent),
        "session.status" => SessionStatus(SessionStatusEvent),
        "session.idle" => SessionIdle(SessionRef),
        "session.compacted" => SessionCompacted(SessionRef),
        "session.error" => SessionError(SessionErrorEvent),
        "session.diff" => SessionDiff(SessionDiffEvent),
        "message.updated" => MessageUpdated(MessageUpdatedEvent),
        "message.removed" => MessageRemoved(RemovedEvent),
        "message.part.updated" => MessagePartUpdated(PartUpdatedEvent),
        "message.part.delta" => MessagePartDelta(PartDeltaEvent),
        "message.part.removed" => MessagePartRemoved(RemovedEvent),
        "permission.asked" => PermissionAsked(PermissionRequest),
        "permission.replied" => PermissionReplied(PermissionRepliedEvent),
        "question.asked" => QuestionAsked(QuestionRequest),
        "question.replied" => QuestionReplied(QuestionResolvedEvent),
        "question.rejected" => QuestionRejected(QuestionResolvedEvent),
        "todo.updated" => TodoUpdated(TodoUpdatedEvent),
    }
    unit {
        "server.connected" => ServerConnected,
        "server.heartbeat" => ServerHeartbeat,
    }
}

impl Event {
    /// Session this event belongs to, when it carries one.
    pub fn session_id(&self) -> Option<&str> {
        match self {
            Event::SessionCreated(e) | Event::SessionUpdated(e) | Event::SessionDeleted(e) => {
                Some(&e.session_id)
            }
            Event::SessionStatus(e) => Some(&e.session_id),
            Event::SessionIdle(e) | Event::SessionCompacted(e) => Some(&e.session_id),
            Event::SessionError(e) => e.session_id.as_deref(),
            Event::SessionDiff(e) => Some(&e.session_id),
            Event::MessageUpdated(e) => Some(&e.session_id),
            Event::MessageRemoved(e) | Event::MessagePartRemoved(e) => Some(&e.session_id),
            Event::MessagePartUpdated(e) => Some(&e.session_id),
            Event::MessagePartDelta(e) => Some(&e.session_id),
            Event::PermissionAsked(e) => Some(&e.session_id),
            Event::PermissionReplied(e) => Some(&e.session_id),
            Event::QuestionAsked(e) => Some(&e.session_id),
            Event::QuestionReplied(e) | Event::QuestionRejected(e) => Some(&e.session_id),
            Event::TodoUpdated(e) => Some(&e.session_id),
            Event::Unknown { properties, .. } => properties
                .as_ref()
                .and_then(|p| p.get("sessionID"))
                .and_then(Value::as_str),
            _ => None,
        }
    }
}

/// `payload` of a global event: the typed event plus the sibling `id` and any other top-level keys
/// (`sync` events carry `syncEvent` here).
#[derive(Debug, Clone, PartialEq)]
pub struct EventPayload {
    pub id: Option<String>,
    pub event: Event,
    pub extra: Map<String, Value>,
}

impl EventPayload {
    pub fn from_value(v: Value) -> EventPayload {
        let Value::Object(mut map) = v else {
            return EventPayload {
                id: None,
                event: Event::Malformed(v),
                extra: Map::new(),
            };
        };
        let Some(ty) = map.get("type").and_then(Value::as_str).map(str::to_owned) else {
            return EventPayload {
                id: None,
                event: Event::Malformed(Value::Object(map)),
                extra: Map::new(),
            };
        };
        let id = match map.get("id") {
            Some(Value::String(s)) => {
                let s = s.clone();
                map.remove("id");
                Some(s)
            }
            _ => None,
        };
        let properties = map.remove("properties");
        map.remove("type");
        let event = Event::parse(&ty, properties.as_ref());
        EventPayload {
            id,
            event,
            extra: map,
        }
    }

    pub fn to_value(&self) -> Value {
        if let Event::Malformed(v) = &self.event {
            return v.clone();
        }
        let mut m = Map::new();
        if let Some(id) = &self.id {
            m.insert("id".into(), Value::String(id.clone()));
        }
        m.insert(
            "type".into(),
            Value::String(self.event.type_name().to_owned()),
        );
        if let Some(p) = self.event.properties() {
            m.insert("properties".into(), p);
        }
        for (k, v) in &self.extra {
            m.insert(k.clone(), v.clone());
        }
        Value::Object(m)
    }
}

impl Serialize for EventPayload {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.to_value().serialize(s)
    }
}

impl<'de> Deserialize<'de> for EventPayload {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(EventPayload::from_value(Value::deserialize(d)?))
    }
}

wire! {
    /// One SSE frame of `GET /global/event`.
    pub struct GlobalEvent {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub directory: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub project: Option<String>,
        pub payload: EventPayload,
    }
}

impl Default for EventPayload {
    fn default() -> Self {
        EventPayload {
            id: None,
            event: Event::Malformed(Value::Null),
            extra: Map::new(),
        }
    }
}

impl GlobalEvent {
    /// Parses a frame; a frame that is not an object with a `payload` becomes a malformed event.
    pub fn from_value(v: Value) -> GlobalEvent {
        match serde_json::from_value::<GlobalEvent>(v.clone()) {
            Ok(e) => e,
            Err(_) => GlobalEvent {
                directory: None,
                project: None,
                payload: EventPayload {
                    id: None,
                    event: Event::Malformed(v),
                    extra: Map::new(),
                },
                extra: Map::new(),
            },
        }
    }

    pub fn to_value(&self) -> Value {
        match &self.payload.event {
            // A frame that was not an object with a `payload` is returned verbatim.
            Event::Malformed(v)
                if self.directory.is_none() && self.project.is_none() && self.extra.is_empty() =>
            {
                v.clone()
            }
            _ => serde_json::to_value(self).unwrap_or(Value::Null),
        }
    }
}
