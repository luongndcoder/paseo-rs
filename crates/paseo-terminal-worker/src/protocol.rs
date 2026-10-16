//! JSONL envelope: the Node IPC `TerminalWorkerRequest/Response/Event` objects, one per line.
//! See PROTOCOL.md. Buffer-typed fields use `{"$b64": "..."}` ([`Bytes`]).

use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{json, Map, Value};
use std::collections::HashMap;

/// Byte payload; serialized as `{"$b64":"<base64>"}`. Reserved: no current message carries raw bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub struct Bytes(pub Vec<u8>);

#[derive(Serialize, Deserialize)]
#[allow(dead_code)]
struct B64Repr {
    #[serde(rename = "$b64")]
    b64: String,
}

impl Serialize for Bytes {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        B64Repr {
            b64: STANDARD.encode(&self.0),
        }
        .serialize(s)
    }
}

impl<'de> Deserialize<'de> for Bytes {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let repr = B64Repr::deserialize(d)?;
        STANDARD
            .decode(repr.b64.as_bytes())
            .map(Bytes)
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ActivityState {
    Idle,
    Working,
    Attention,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CreateOptions {
    pub id: Option<String>,
    pub cwd: String,
    pub workspace_id: Option<String>,
    pub name: Option<String>,
    pub title: Option<String>,
    pub env: Option<HashMap<String, String>>,
    pub command: Option<String>,
    pub args: Option<Vec<String>>,
    pub rows: Option<u16>,
    pub cols: Option<u16>,
    pub activity_token: Option<String>,
    pub activity_url: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct KillAndWaitOptions {
    pub graceful_timeout_ms: Option<u64>,
    pub force_timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ClientMessage {
    Input {
        data: String,
    },
    Resize {
        rows: u16,
        cols: u16,
    },
    Mouse {
        row: i64,
        col: i64,
        button: i64,
        action: String,
    },
}

#[derive(Debug, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum Request {
    CreateTerminal {
        request_id: String,
        options: CreateOptions,
    },
    RegisterCwdEnv {
        request_id: String,
        cwd: String,
        env: HashMap<String, String>,
    },
    SetActivity {
        request_id: String,
        terminal_id: String,
        state: ActivityState,
    },
    ClearAttention {
        request_id: String,
        terminal_id: String,
    },
    KillTerminal {
        request_id: String,
        terminal_id: String,
    },
    KillTerminalAndWait {
        request_id: String,
        terminal_id: String,
        options: Option<KillAndWaitOptions>,
    },
    GetTerminalState {
        request_id: String,
        terminal_id: String,
        options: Option<Value>,
    },
    CaptureTerminal {
        request_id: String,
        terminal_id: String,
        start: Option<i64>,
        end: Option<i64>,
        strip_ansi: Option<bool>,
    },
    KillAll {
        request_id: String,
    },
    Send {
        request_id: String,
        terminal_id: String,
        message: ClientMessage,
    },
}

pub fn response_ok(request_id: &str, result: Option<Value>) -> Value {
    let mut m = Map::new();
    m.insert("type".into(), json!("response"));
    m.insert("requestId".into(), json!(request_id));
    m.insert("ok".into(), json!(true));
    if let Some(r) = result {
        m.insert("result".into(), r);
    }
    Value::Object(m)
}

pub fn response_err(request_id: &str, error: &str) -> Value {
    json!({"type":"response","requestId":request_id,"ok":false,"error":error})
}

pub fn terminal_message(terminal_id: &str, message: Value) -> Value {
    json!({"type":"terminalMessage","terminalId":terminal_id,"message":message})
}

pub fn output_message(terminal_id: &str, data: &str, revision: Option<u64>) -> Value {
    let mut m = json!({"type":"output","data":data});
    if let Some(r) = revision {
        m["revision"] = json!(r);
    }
    terminal_message(terminal_id, m)
}

pub fn title_message(terminal_id: &str, title: Option<&str>) -> Value {
    let mut m = json!({"type":"titleChange"});
    if let Some(t) = title {
        m["title"] = json!(t);
    }
    terminal_message(terminal_id, m)
}

pub fn title_event(terminal_id: &str, title: Option<&str>) -> Value {
    let mut m = json!({"type":"terminalTitleChange","terminalId":terminal_id});
    if let Some(t) = title {
        m["title"] = json!(t);
    }
    m
}

/// Parses a request line. On failure returns the recovered `requestId` (if any) and a message.
pub fn parse_request(line: &str) -> Result<Request, (Option<String>, String)> {
    let value: Value = serde_json::from_str(line)
        .map_err(|e| (scan_request_id(line), format!("invalid JSON: {e}")))?;
    let request_id = value
        .get("requestId")
        .and_then(Value::as_str)
        .map(str::to_owned);
    serde_json::from_value::<Request>(value)
        .map_err(|e| (request_id, format!("invalid request: {e}")))
}

/// Best-effort `requestId` recovery from a line serde_json rejected (for example a lone UTF-16
/// surrogate escape), so the caller still gets an error response instead of waiting out a timeout.
fn scan_request_id(line: &str) -> Option<String> {
    let rest = line[line.find("\"requestId\"")? + "\"requestId\"".len()..].trim_start();
    let rest = rest.strip_prefix(':')?.trim_start().strip_prefix('"')?;
    let mut id = String::new();
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Some(id),
            '\\' => id.push(chars.next()?),
            c => id.push(c),
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_roundtrip_uses_b64_envelope() {
        let v = serde_json::to_value(Bytes(vec![0, 1, 2, 255])).unwrap();
        assert_eq!(v, json!({"$b64":"AAEC/w=="}));
        let back: Bytes = serde_json::from_value(v).unwrap();
        assert_eq!(back.0, vec![0, 1, 2, 255]);
        assert!(serde_json::from_value::<Bytes>(json!({"$b64":"!!"})).is_err());
    }

    #[test]
    fn parses_create_terminal() {
        let line = r#"{"type":"createTerminal","requestId":"r1","options":{"id":"t1","cwd":"/tmp","workspaceId":"w","env":{"A":"b"},"command":"sh","args":["-c","x"],"rows":10,"cols":20,"activityToken":"tok","activityUrl":null}}"#;
        match parse_request(line).unwrap() {
            Request::CreateTerminal {
                request_id,
                options,
            } => {
                assert_eq!(request_id, "r1");
                assert_eq!(options.id.as_deref(), Some("t1"));
                assert_eq!(options.workspace_id.as_deref(), Some("w"));
                assert_eq!(options.rows, Some(10));
                assert_eq!(options.activity_url, None);
                assert_eq!(options.env.unwrap()["A"], "b");
                assert_eq!(options.args.unwrap(), vec!["-c", "x"]);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn parses_every_request_type() {
        let lines = [
            r#"{"type":"registerCwdEnv","requestId":"1","cwd":"/a","env":{"X":"1"}}"#,
            r#"{"type":"setActivity","requestId":"2","terminalId":"t","state":"working"}"#,
            r#"{"type":"clearAttention","requestId":"3","terminalId":"t"}"#,
            r#"{"type":"killTerminal","requestId":"4","terminalId":"t"}"#,
            r#"{"type":"killTerminalAndWait","requestId":"5","terminalId":"t","options":{"gracefulTimeoutMs":5,"forceTimeoutMs":6}}"#,
            r#"{"type":"killTerminalAndWait","requestId":"5","terminalId":"t"}"#,
            r#"{"type":"getTerminalState","requestId":"6","terminalId":"t","options":{"scrollbackLines":3,"includeWrapFlags":true}}"#,
            r#"{"type":"captureTerminal","requestId":"7","terminalId":"t","start":-5,"end":-1,"stripAnsi":false}"#,
            r#"{"type":"killAll","requestId":"8"}"#,
            r#"{"type":"send","requestId":"9","terminalId":"t","message":{"type":"input","data":"ls\r"}}"#,
            r#"{"type":"send","requestId":"9","terminalId":"t","message":{"type":"resize","rows":5,"cols":6}}"#,
            r#"{"type":"send","requestId":"9","terminalId":"t","message":{"type":"mouse","row":1,"col":2,"button":0,"action":"down"}}"#,
        ];
        for line in lines {
            parse_request(line).unwrap_or_else(|e| panic!("{line}: {e:?}"));
        }
    }

    #[test]
    fn parse_failure_recovers_request_id() {
        let (id, _) = parse_request(r#"{"type":"nope","requestId":"zz"}"#).unwrap_err();
        assert_eq!(id.as_deref(), Some("zz"));
        let (id, _) = parse_request("not json").unwrap_err();
        assert_eq!(id, None);
        let lone = r#"{"type":"send","requestId":"r1","terminalId":"x","message":{"type":"input","data":"\ud83d"}}"#;
        let (id, _) = parse_request(lone).unwrap_err();
        assert_eq!(id.as_deref(), Some("r1"));
    }

    #[test]
    fn serializes_responses_and_events() {
        assert_eq!(
            response_ok("a", None),
            json!({"type":"response","requestId":"a","ok":true})
        );
        assert_eq!(response_ok("a", Some(json!(null)))["result"], Value::Null);
        assert_eq!(
            response_err("a", "boom"),
            json!({"type":"response","requestId":"a","ok":false,"error":"boom"})
        );
        assert_eq!(
            output_message("t", "hi", Some(3)),
            json!({"type":"terminalMessage","terminalId":"t","message":{"type":"output","data":"hi","revision":3}})
        );
        assert_eq!(
            title_event("t", None),
            json!({"type":"terminalTitleChange","terminalId":"t"})
        );
        assert_eq!(
            title_message("t", Some("x"))["message"],
            json!({"type":"titleChange","title":"x"})
        );
    }
}
