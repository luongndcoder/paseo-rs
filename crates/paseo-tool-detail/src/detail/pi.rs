//! Pi tool-call parsing and detail mapping from `pi/tool-call-mapper.ts`
//! (`parseToolResult`, `extractTextFromToolResult`, `parseToolArgs`, `mapToolDetail`).

use super::primitives::{obj, opt_n, opt_s, req_s, P};
use crate::js::js_to_string;
use paseo_agent_types::{Map, Num, SearchDetail, SearchToolName, ToolCallDetail};
use serde_json::Value;

/// A parsed tool result. `Object` holds the zod output: known keys first (when present and
/// well-typed), then pass-through extras, text content blocks stripped to `{type, text}`.
#[derive(Debug, Clone, PartialEq)]
pub enum PiToolResult {
    Null,
    Text(String),
    Object(Map),
}

impl PiToolResult {
    pub fn to_value(&self) -> Value {
        match self {
            Self::Null => Value::Null,
            Self::Text(s) => Value::String(s.clone()),
            Self::Object(m) => Value::Object(m.clone()),
        }
    }
}

fn parse_content_block(v: &Value) -> P<Value> {
    let m = obj(v)?;
    if let (Some(Value::String(t)), Some(Value::String(text))) = (m.get("type"), m.get("text")) {
        if t == "text" {
            let mut out = Map::new();
            out.insert("type".into(), Value::String(t.clone()));
            out.insert("text".into(), Value::String(text.clone()));
            return Ok(Value::Object(out));
        }
    }
    // pass-through `{type: string}`: `type` first, then the rest in input order
    let ty = req_s(m, "type")?;
    let mut out = Map::new();
    out.insert("type".into(), Value::String(ty.to_string()));
    for (k, x) in m {
        if k != "type" {
            out.insert(k.clone(), x.clone());
        }
    }
    Ok(Value::Object(out))
}

fn parse_result_object(m: &Map) -> P<Map> {
    let mut out = Map::new();
    for k in ["output", "stdout", "text"] {
        if let Some(s) = opt_s(m, k)? {
            out.insert(k.into(), Value::String(s.to_string()));
        }
    }
    if let Some(c) = m.get("content") {
        let arr = c.as_array().ok_or(())?;
        let blocks: P<Vec<Value>> = arr.iter().map(parse_content_block).collect();
        out.insert("content".into(), Value::Array(blocks?));
    }
    for k in ["exitCode", "code"] {
        if let Some(n) = opt_n(m, k)? {
            out.insert(k.into(), Value::Number(n));
        }
    }
    if let Some(d) = m.get("details") {
        let dm = obj(d)?;
        let mut details = Map::new();
        if let Some(s) = opt_s(dm, "diff")? {
            details.insert("diff".into(), Value::String(s.to_string()));
        }
        for (k, x) in dm {
            if k != "diff" {
                details.insert(k.clone(), x.clone());
            }
        }
        out.insert("details".into(), Value::Object(details));
    }
    for (k, x) in m {
        if ![
            "output", "stdout", "text", "content", "exitCode", "code", "details",
        ]
        .contains(&k.as_str())
        {
            out.insert(k.clone(), x.clone());
        }
    }
    Ok(out)
}

/// `parseToolResult`: unparseable input becomes `Null`.
pub fn parse_tool_result(raw: Option<&Value>) -> PiToolResult {
    match raw {
        Some(Value::String(s)) => PiToolResult::Text(s.clone()),
        Some(Value::Object(m)) => match parse_result_object(m) {
            Ok(o) => PiToolResult::Object(o),
            Err(()) => PiToolResult::Null,
        },
        _ => PiToolResult::Null,
    }
}

/// `extractTextFromToolResult`.
pub fn extract_text_from_tool_result(result: &PiToolResult) -> Option<String> {
    match result {
        PiToolResult::Text(s) => Some(s.clone()),
        PiToolResult::Null => None,
        PiToolResult::Object(m) => {
            // `result.output ?? result.stdout ?? result.text`, then truthiness
            let direct = ["output", "stdout", "text"]
                .iter()
                .find_map(|k| m.get(*k).and_then(Value::as_str));
            if let Some(d) = direct.filter(|d| !d.is_empty()) {
                return Some(d.to_string());
            }
            let content = m.get("content")?.as_array()?;
            let parts: Vec<String> = content
                .iter()
                .filter_map(|b| {
                    let bm = b.as_object()?;
                    if bm.get("type").and_then(Value::as_str) == Some("text") {
                        bm.get("text").map(|t| match t {
                            Value::Null => String::new(),
                            other => js_to_string(other),
                        })
                    } else {
                        None
                    }
                })
                .collect();
            (!parts.is_empty()).then(|| parts.join("\n"))
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct EditPair {
    pub old_text: String,
    pub new_text: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PiTrackedToolCall {
    Bash {
        command: String,
        timeout: Option<Num>,
    },
    Read {
        path: String,
        offset: Option<Num>,
        limit: Option<Num>,
    },
    Edit {
        path: String,
        edits: Vec<EditPair>,
    },
    Write {
        path: String,
        content: String,
    },
    Find {
        pattern: String,
        path: Option<String>,
        limit: Option<Num>,
    },
    Grep {
        pattern: String,
        path: Option<String>,
    },
    Ls {
        path: Option<String>,
    },
    Unknown {
        tool_name: String,
        args: Value,
    },
}

fn parse_edit_args(m: &Map) -> Option<PiTrackedToolCall> {
    // current shape: {path, edits: [{oldText, newText}]}
    let current = (|| -> P<PiTrackedToolCall> {
        let path = req_s(m, "path")?.to_string();
        let arr = m.get("edits").and_then(Value::as_array).ok_or(())?;
        let edits: P<Vec<EditPair>> = arr
            .iter()
            .map(|e| {
                let em = obj(e)?;
                Ok(EditPair {
                    old_text: req_s(em, "oldText")?.to_string(),
                    new_text: req_s(em, "newText")?.to_string(),
                })
            })
            .collect();
        Ok(PiTrackedToolCall::Edit {
            path,
            edits: edits?,
        })
    })();
    if let Ok(c) = current {
        return Some(c);
    }
    // legacy: {path, old_string?|oldString?, new_string?|newString?}
    let legacy = (|| -> P<Option<PiTrackedToolCall>> {
        let path = req_s(m, "path")?.to_string();
        let old_a = opt_s(m, "old_string")?;
        let old_b = opt_s(m, "oldString")?;
        let new_a = opt_s(m, "new_string")?;
        let new_b = opt_s(m, "newString")?;
        let old_text = old_a.or(old_b);
        let new_text = new_a.or(new_b);
        match (old_text, new_text) {
            (Some(o), Some(n)) if !o.is_empty() => Ok(Some(PiTrackedToolCall::Edit {
                path,
                edits: vec![EditPair {
                    old_text: o.to_string(),
                    new_text: n.to_string(),
                }],
            })),
            _ => Ok(None),
        }
    })();
    legacy.ok().flatten()
}

/// `parseToolArgs`. `raw_args = None` is `undefined`.
pub fn parse_tool_args(tool_name: &str, raw_args: Option<&Value>) -> PiTrackedToolCall {
    let unknown = || PiTrackedToolCall::Unknown {
        tool_name: tool_name.to_string(),
        args: raw_args.cloned().unwrap_or(Value::Null),
    };
    let Some(Value::Object(m)) = raw_args else {
        return unknown();
    };
    let parsed: Option<PiTrackedToolCall> = match tool_name {
        "edit" => parse_edit_args(m),
        "bash" => (|| -> P<PiTrackedToolCall> {
            Ok(PiTrackedToolCall::Bash {
                command: req_s(m, "command")?.to_string(),
                timeout: opt_n(m, "timeout")?,
            })
        })()
        .ok(),
        "read" => (|| -> P<PiTrackedToolCall> {
            Ok(PiTrackedToolCall::Read {
                path: req_s(m, "path")?.to_string(),
                offset: opt_n(m, "offset")?,
                limit: opt_n(m, "limit")?,
            })
        })()
        .ok(),
        "write" => (|| -> P<PiTrackedToolCall> {
            Ok(PiTrackedToolCall::Write {
                path: req_s(m, "path")?.to_string(),
                content: req_s(m, "content")?.to_string(),
            })
        })()
        .ok(),
        "find" => (|| -> P<PiTrackedToolCall> {
            Ok(PiTrackedToolCall::Find {
                pattern: req_s(m, "pattern")?.to_string(),
                path: opt_s(m, "path")?.map(String::from),
                limit: opt_n(m, "limit")?,
            })
        })()
        .ok(),
        "grep" => (|| -> P<PiTrackedToolCall> {
            let pattern = req_s(m, "pattern")?.to_string();
            let path = opt_s(m, "path")?.map(String::from);
            opt_s(m, "glob")?;
            for k in ["ignoreCase", "literal"] {
                match m.get(k) {
                    None | Some(Value::Bool(_)) => {}
                    _ => return Err(()),
                }
            }
            opt_n(m, "context")?;
            opt_n(m, "limit")?;
            Ok(PiTrackedToolCall::Grep { pattern, path })
        })()
        .ok(),
        "ls" => (|| -> P<PiTrackedToolCall> {
            let path = opt_s(m, "path")?.map(String::from);
            opt_n(m, "limit")?;
            Ok(PiTrackedToolCall::Ls { path })
        })()
        .ok(),
        _ => None,
    };
    parsed.unwrap_or_else(unknown)
}

/// `mapToolDetail`.
pub fn map_tool_detail(call: &PiTrackedToolCall, result: Option<&PiToolResult>) -> ToolCallDetail {
    let null = PiToolResult::Null;
    let result = result.unwrap_or(&null);
    let string_result = match result {
        PiToolResult::Text(s) => Some(s.clone()),
        _ => None,
    };
    match call {
        PiTrackedToolCall::Bash { command, .. } => {
            let (output, exit_code) = resolve_output(result);
            ToolCallDetail::Shell {
                command: command.clone(),
                cwd: None,
                output,
                exit_code,
            }
        }
        PiTrackedToolCall::Read {
            path,
            offset,
            limit,
        } => ToolCallDetail::Read {
            file_path: path.clone(),
            content: extract_text_from_tool_result(result),
            offset: offset.clone(),
            limit: limit.clone(),
        },
        PiTrackedToolCall::Edit { path, edits } => {
            let first = edits.first();
            let unified_diff = match result {
                PiToolResult::Object(m) => m
                    .get("details")
                    .and_then(Value::as_object)
                    .and_then(|d| d.get("diff"))
                    .and_then(Value::as_str)
                    .map(String::from),
                _ => None,
            };
            ToolCallDetail::Edit {
                file_path: path.clone(),
                old_string: first.map(|e| e.old_text.clone()),
                new_string: first.map(|e| e.new_text.clone()),
                unified_diff,
            }
        }
        PiTrackedToolCall::Write { path, content } => ToolCallDetail::Write {
            file_path: path.clone(),
            content: Some(content.clone()),
        },
        PiTrackedToolCall::Find { pattern, .. } => ToolCallDetail::Search(SearchDetail {
            query: pattern.clone(),
            tool_name: Some(SearchToolName::Search),
            content: string_result,
            ..Default::default()
        }),
        PiTrackedToolCall::Grep { pattern, .. } => ToolCallDetail::Search(SearchDetail {
            query: pattern.clone(),
            tool_name: Some(SearchToolName::Grep),
            content: string_result,
            ..Default::default()
        }),
        PiTrackedToolCall::Ls { path } => ToolCallDetail::Search(SearchDetail {
            query: path.clone().unwrap_or_else(|| "ls".into()),
            content: string_result,
            ..Default::default()
        }),
        PiTrackedToolCall::Unknown { args, .. } => ToolCallDetail::Unknown {
            input: args.clone(),
            output: result.to_value(),
        },
    }
}

/// `resolveToolCallOutput`: `(output, exitCode)` where `exitCode: Some(None)` is JS `null`.
fn resolve_output(result: &PiToolResult) -> (Option<String>, Option<Option<Num>>) {
    match result {
        PiToolResult::Text(s) => (Some(s.clone()), None),
        PiToolResult::Null => (None, None),
        PiToolResult::Object(m) => {
            let output = extract_text_from_tool_result(result);
            let exit = ["exitCode", "code"]
                .iter()
                .find_map(|k| m.get(*k).and_then(Value::as_number).cloned());
            (output, Some(exit))
        }
    }
}
