//! Sub-agent extensions: `pi-subagents`, `@tintinweb/pi-subagents`, `@gotgenes/pi-subagents`.

use std::collections::{HashMap, HashSet};

use paseo_agent_types::{Map, ToolCallDetail};
use paseo_tool_detail::detail::pi::{extract_text_from_tool_result, PiToolResult};
use serde_json::{json, Value};

use super::{
    fork_status, log_text, sub_agent_detail, upsert, ChildSession, ExtMapping, ExtSession,
    ExtToolCall,
};
use crate::pi::util::*;

pub(super) const GOTGENES_CHILD_SESSION_MARKER: &str = "PASEO_GOTGENES_CHILD_SESSION";

// ---- pi-subagents -------------------------------------------------------------------------

struct Row {
    index: Option<i64>,
    agent: String,
    exit_code: Option<f64>,
    success: Option<bool>,
    session_file: Option<String>,
}

fn parse_row(v: &Value) -> Option<Row> {
    let m = v.as_object()?;
    let index = match m.get("index") {
        None => None,
        Some(_) => {
            let i = req_int(m, "index")?;
            if i < 0 {
                return None;
            }
            Some(i)
        }
    };
    let exit_code = match m.get("exitCode") {
        None => None,
        Some(Value::Number(n)) => n.as_f64(),
        Some(_) => return None,
    };
    Some(Row {
        index,
        agent: req_trim(m, "agent")?.to_string(),
        exit_code,
        success: opt_bool(m, "success")?,
        session_file: opt_trim(m, "sessionFile")?.map(str::to_string),
    })
}

struct SubDetails {
    mode: String,
    run_id: Option<String>,
    async_id: Option<String>,
    async_dir: Option<String>,
    results: Vec<Row>,
    completions: Option<Vec<(String, Vec<Row>)>>,
}

fn parse_sub_details(v: Option<&Value>) -> Option<SubDetails> {
    let m = v?.as_object()?;
    let results = req_array(m, "results")?
        .iter()
        .map(parse_row)
        .collect::<Option<Vec<_>>>()?;
    let completions = match m.get("completions") {
        None => None,
        Some(Value::Array(a)) => Some(
            a.iter()
                .map(|c| {
                    let cm = c.as_object()?;
                    let rows = req_array(cm, "results")?
                        .iter()
                        .map(parse_row)
                        .collect::<Option<Vec<_>>>()?;
                    Some((req_trim(cm, "runId")?.to_string(), rows))
                })
                .collect::<Option<Vec<_>>>()?,
        ),
        Some(_) => return None,
    };
    Some(SubDetails {
        mode: req_str(m, "mode")?.to_string(),
        run_id: opt_str(m, "runId")?.map(str::to_string),
        async_id: opt_str(m, "asyncId")?.map(str::to_string),
        async_dir: opt_trim(m, "asyncDir")?.map(str::to_string),
        results,
        completions,
    })
}

/// The `subagent-notify` header; a grouped notice reports every run as completed.
fn notice_status(header: &str) -> Option<&'static str> {
    // /^Background tasks completed \(\d+\):/
    if let Some(rest) = header.strip_prefix("Background tasks completed (") {
        let digits = rest.chars().take_while(char::is_ascii_digit).count();
        if digits > 0 && rest[digits..].starts_with("):") {
            return Some("completed");
        }
    }
    // /^(?:Background task|Detached foreground task) (completed|failed|stopped): \*\*/
    for prefix in ["Background task ", "Detached foreground task "] {
        if let Some(rest) = header.strip_prefix(prefix) {
            for (word, status) in [
                ("completed", "completed"),
                ("failed", "failed"),
                ("stopped", "canceled"),
            ] {
                if let Some(after) = rest.strip_prefix(word) {
                    if after.starts_with(": **") {
                        return Some(status);
                    }
                }
            }
        }
    }
    None
}

/// JS `.+` anchored with `$`: a non-empty rest without line terminators.
///
/// PARITY-QUIRK(pi-notice-line-terminators): `.` does not match `\r`, so a notice line ending in
/// CR never matches its `Session file:` / `Retention-managed async directory:` pattern.
fn line_rest<'a>(line: &'a str, prefix: &str) -> Option<&'a str> {
    let rest = line.strip_prefix(prefix)?;
    (!rest.is_empty() && !rest.contains(['\n', '\r', '\u{2028}', '\u{2029}'])).then_some(rest)
}

#[derive(Default)]
pub(super) struct PiSubagents {
    calls_by_run: HashMap<String, String>,
    read_sessions: HashSet<String>,
    /// async directory -> owning call id
    async_runs: HashMap<String, String>,
}

impl PiSubagents {
    fn collect_rows(
        &mut self,
        owner: &str,
        rows: &[Row],
        description: Option<&str>,
        failed: bool,
    ) -> (Vec<Value>, Vec<ChildSession>) {
        let mut subagents = Vec::new();
        let mut child_sessions = Vec::new();
        for (index, row) in rows.iter().enumerate() {
            let id = if rows.len() == 1 {
                owner.to_string()
            } else {
                format!("{owner}:{}", row.index.unwrap_or(index as i64))
            };
            let bad = failed
                || row.success == Some(false)
                || (row.success != Some(true) && row.exit_code != Some(0.0));
            let mut fields = vec![
                ("title", json!(row.agent)),
                ("toolCallId", json!(owner)),
                ("status", json!(if bad { "failed" } else { "completed" })),
            ];
            if let Some(d) = description {
                fields.push(("description", json!(d)));
            }
            subagents.push(upsert(&id, &fields));
            if let Some(file) = &row.session_file {
                if self.read_sessions.insert(file.clone()) {
                    child_sessions.push(ChildSession {
                        id,
                        file: file.clone(),
                    });
                }
            }
        }
        (subagents, child_sessions)
    }

    fn map_wait(&mut self, call: &ExtToolCall) -> Option<ExtMapping> {
        if call.status == "running" {
            return None;
        }
        let details = parse_sub_details(call.details())?;
        let completions = details.completions?;
        let mut subagents = Vec::new();
        let mut child_sessions = Vec::new();
        for (run_id, rows) in &completions {
            let Some(owner) = self.calls_by_run.get(run_id).cloned() else {
                continue;
            };
            let (s, c) = self.collect_rows(&owner, rows, None, false);
            subagents.extend(s);
            child_sessions.extend(c);
        }
        (!subagents.is_empty()).then(|| ExtMapping::subagents(subagents, child_sessions))
    }

    fn map_spawn(&mut self, call: &ExtToolCall) -> Option<ExtMapping> {
        let args = call.args.as_object()?;
        let agent = opt_trim(args, "agent")?;
        let task = opt_trim(args, "task")?;
        let is_async = opt_bool(args, "async")?;
        if args.contains_key("action") {
            return None;
        }
        let (agent, task) = (agent?, task?);
        let detail = sub_agent_detail(Some(agent), Some(task), log_text(&call.result));
        let base = |status: &str| {
            upsert(
                &call.call_id,
                &[
                    ("title", json!(agent)),
                    ("description", json!(task)),
                    ("toolCallId", json!(call.call_id)),
                    ("status", json!(status)),
                ],
            )
        };
        let with = |detail: ToolCallDetail, subagents: Vec<Value>| ExtMapping {
            detail: Some(detail),
            subagents,
            ..ExtMapping::default()
        };
        if call.status == "running" {
            return Some(with(detail, vec![base("running")]));
        }
        let failed = call.status == "failed";
        let details = parse_sub_details(call.details());
        let Some(details) = details.filter(|d| d.mode != "management") else {
            return Some(if failed {
                with(detail, vec![base("failed")])
            } else {
                ExtMapping::detail(detail)
            });
        };
        if let Some(run_id) = details.run_id.as_deref().filter(|r| !r.is_empty()) {
            self.calls_by_run
                .insert(run_id.to_string(), call.call_id.clone());
        }
        let has_async_id = details.async_id.as_deref().is_some_and(|a| !a.is_empty());
        if details.results.is_empty() && (has_async_id || is_async == Some(true)) {
            if let Some(dir) = &details.async_dir {
                if !failed {
                    self.async_runs.insert(dir.clone(), call.call_id.clone());
                }
            }
            return Some(with(
                detail,
                vec![base(if failed { "failed" } else { "running" })],
            ));
        }
        if details.results.is_empty() && failed {
            return Some(with(detail, vec![base("failed")]));
        }
        let (subagents, child_sessions) =
            self.collect_rows(&call.call_id, &details.results, Some(task), failed);
        Some(ExtMapping {
            detail: Some(detail),
            subagents,
            child_sessions,
            ..ExtMapping::default()
        })
    }
}

impl ExtSession for PiSubagents {
    fn map_tool_call(&mut self, call: &ExtToolCall) -> Option<ExtMapping> {
        match call.tool_name.as_str() {
            "bg_wait" => self.map_wait(call),
            "subagent" => self.map_spawn(call),
            _ => None,
        }
    }

    /// The notice is the only completion Pi persists, so replay settles async runs from it.
    fn map_custom_message(&mut self, message: &Value) -> Option<ExtMapping> {
        if message.get("customType").and_then(Value::as_str) != Some("subagent-notify") {
            return None;
        }
        let content = message.get("content")?;
        let text = match content {
            Value::String(s) => Some(s.clone()),
            other => {
                let wrapped = json!({"content": other});
                let result = paseo_tool_detail::detail::pi::parse_tool_result(Some(&wrapped));
                extract_text_from_tool_result(&result)
            }
        }?;
        let lines: Vec<&str> = text.split('\n').collect();
        let status = notice_status(lines.first().copied().unwrap_or(""))?;
        let mut subagents = Vec::new();
        let mut child_sessions = Vec::new();
        let mut owner: Option<String> = None;
        for line in lines {
            if let Some(dir) = line_rest(line, "Retention-managed async directory: ") {
                owner = self.async_runs.get(dir).cloned();
                if let Some(o) = &owner {
                    subagents.push(upsert(o, &[("status", json!(status))]));
                }
                continue;
            }
            let Some(owner_id) = owner.clone() else {
                continue;
            };
            let Some(file) = line_rest(line, "Session file: ") else {
                continue;
            };
            if self.read_sessions.insert(file.to_string()) {
                child_sessions.push(ChildSession {
                    id: owner_id,
                    file: file.to_string(),
                });
            }
            owner = None;
        }
        (!subagents.is_empty()).then(|| ExtMapping::subagents(subagents, child_sessions))
    }
}

// ---- @tintinweb/pi-subagents --------------------------------------------------------------

struct ForkDetails {
    agent_id: String,
    status: String,
    description: Option<String>,
    transcript_path: Option<String>,
}

/// `gotgenes` also validates `transcriptPath`; for tintinweb it is an unchecked pass-through key.
fn parse_fork_details(v: Option<&Value>, gotgenes: bool) -> Option<ForkDetails> {
    let m = v?.as_object()?;
    opt_str(m, "displayName")?;
    Some(ForkDetails {
        agent_id: req_trim(m, "agentId")?.to_string(),
        status: req_str(m, "status")?.to_string(),
        description: opt_str(m, "description")?.map(str::to_string),
        transcript_path: if gotgenes {
            opt_trim(m, "transcriptPath")?.map(str::to_string)
        } else {
            None
        },
    })
}

struct SpawnArgs {
    subagent_type: String,
    prompt: String,
    description: Option<String>,
}

fn parse_spawn_args(args: &Value) -> Option<SpawnArgs> {
    let m = args.as_object()?;
    Some(SpawnArgs {
        subagent_type: req_trim(m, "subagent_type")?.to_string(),
        prompt: req_trim(m, "prompt")?.to_string(),
        description: opt_str(m, "description")?.map(str::to_string),
    })
}

/// `outputFileFromToolResult`: `/^Output file:\s*(\S+)$/m` over the result text.
fn output_file_from_tool_result(result: &PiToolResult) -> Option<String> {
    let text = extract_text_from_tool_result(result)?;
    for line in text.split('\n') {
        // `$` in multiline mode also matches before `\r`, ` ` and ` `.
        let line = line
            .split(['\r', '\u{2028}', '\u{2029}'])
            .next()
            .unwrap_or("");
        let Some(rest) = line.strip_prefix("Output file:") else {
            continue;
        };
        let rest = rest.trim_start_matches(paseo_tool_detail::js::is_js_whitespace);
        if !rest.is_empty() && !rest.chars().any(paseo_tool_detail::js::is_js_whitespace) {
            return Some(rest.to_string());
        }
    }
    None
}

#[derive(Default)]
pub(super) struct Tintinweb {
    calls_by_agent: HashMap<String, String>,
    read_sessions: HashSet<String>,
}

impl Tintinweb {
    fn map_spawn(&mut self, call: &ExtToolCall) -> Option<ExtMapping> {
        let args = parse_spawn_args(&call.args)?;
        let details = parse_fork_details(call.details(), false);
        let title = args.subagent_type.as_str();
        let description = args.description.as_deref().unwrap_or(&args.prompt);
        let detail = sub_agent_detail(Some(title), Some(description), log_text(&call.result));
        let upsert_full = |status: &str| {
            upsert(
                &call.call_id,
                &[
                    ("title", json!(title)),
                    ("description", json!(description)),
                    ("toolCallId", json!(call.call_id)),
                    ("status", json!(status)),
                ],
            )
        };
        if call.status == "running" {
            return Some(ExtMapping {
                detail: Some(detail),
                subagents: vec![upsert_full("running")],
                ..ExtMapping::default()
            });
        }
        let Some(details) = details else {
            return Some(if call.status == "failed" {
                ExtMapping {
                    detail: Some(detail),
                    subagents: vec![upsert(&call.call_id, &[("status", json!("failed"))])],
                    ..ExtMapping::default()
                }
            } else {
                ExtMapping::detail(detail)
            });
        };
        self.calls_by_agent
            .insert(details.agent_id.clone(), call.call_id.clone());
        let file = output_file_from_tool_result(&call.result);
        let mut child_sessions = Vec::new();
        if let Some(file) = file {
            if self.read_sessions.insert(file.clone()) {
                child_sessions.push(ChildSession {
                    id: call.call_id.clone(),
                    file,
                });
            }
        }
        Some(ExtMapping {
            detail: Some(detail),
            subagents: vec![upsert_full(fork_status(&details.status, true))],
            child_sessions,
            ..ExtMapping::default()
        })
    }

    fn map_followup(&mut self, call: &ExtToolCall) -> Option<ExtMapping> {
        let m = call.args.as_object()?;
        let agent_id = req_trim(m, "agent_id")?;
        let id = self.calls_by_agent.get(agent_id)?.clone();
        let details = parse_fork_details(call.details(), false);
        let detail = sub_agent_detail(
            None,
            details.as_ref().and_then(|d| d.description.as_deref()),
            log_text(&call.result),
        );
        let Some(details) = details.filter(|_| call.status != "running") else {
            return Some(ExtMapping::detail(detail));
        };
        Some(ExtMapping {
            detail: Some(detail),
            subagents: vec![upsert(
                &id,
                &[("status", json!(fork_status(&details.status, true)))],
            )],
            ..ExtMapping::default()
        })
    }
}

fn parse_notification_entry(m: &Map) -> Option<(String, String, Option<String>, Option<String>)> {
    Some((
        req_trim(m, "id")?.to_string(),
        req_str(m, "status")?.to_string(),
        opt_str(m, "description")?.map(str::to_string),
        opt_trim(m, "outputFile")?.map(str::to_string),
    ))
}

impl ExtSession for Tintinweb {
    fn map_tool_call(&mut self, call: &ExtToolCall) -> Option<ExtMapping> {
        match call.tool_name.as_str() {
            "Agent" => self.map_spawn(call),
            "get_subagent_result" | "steer_subagent" => self.map_followup(call),
            _ => None,
        }
    }

    fn map_custom_message(&mut self, message: &Value) -> Option<ExtMapping> {
        let ty = message.get("customType").and_then(Value::as_str);
        if ty != Some("subagent-notification") && ty != Some("subagent-update") {
            return None;
        }
        let top = message.get("details")?.as_object()?;
        let mut entries = vec![parse_notification_entry(top)?];
        match top.get("others") {
            None => {}
            Some(Value::Array(a)) => {
                for o in a {
                    entries.push(parse_notification_entry(o.as_object()?)?);
                }
            }
            Some(_) => return None,
        }
        let mut subagents = Vec::new();
        let mut child_sessions = Vec::new();
        for (entry_id, status, description, output_file) in entries {
            let Some(id) = self.calls_by_agent.get(&entry_id).cloned() else {
                continue;
            };
            if let Some(file) = output_file {
                if self.read_sessions.insert(file.clone()) {
                    child_sessions.push(ChildSession {
                        id: id.clone(),
                        file,
                    });
                }
            }
            let mut fields = vec![("status", json!(fork_status(&status, true)))];
            if let Some(d) = description {
                fields.push(("description", json!(d)));
            }
            subagents.push(upsert(&id, &fields));
        }
        (!subagents.is_empty()).then(|| ExtMapping::subagents(subagents, child_sessions))
    }
}

// ---- @gotgenes/pi-subagents ---------------------------------------------------------------

#[derive(Default)]
pub(super) struct Gotgenes {
    calls_by_agent: HashMap<String, String>,
    read_sessions: HashSet<String>,
    pending_files_by_agent: HashMap<String, String>,
}

impl Gotgenes {
    fn child_session(&mut self, id: &str, file: Option<&str>) -> Vec<ChildSession> {
        match file {
            Some(f) if self.read_sessions.insert(f.to_string()) => vec![ChildSession {
                id: id.to_string(),
                file: f.to_string(),
            }],
            _ => Vec::new(),
        }
    }

    fn map_spawn(&mut self, call: &ExtToolCall) -> Option<ExtMapping> {
        let args = parse_spawn_args(&call.args)?;
        let details = parse_fork_details(call.details(), true);
        let title = args.subagent_type.as_str();
        let description = args.description.as_deref().unwrap_or(&args.prompt);
        let detail = sub_agent_detail(Some(title), Some(description), log_text(&call.result));
        let upsert_full = |status: &str| {
            upsert(
                &call.call_id,
                &[
                    ("title", json!(title)),
                    ("description", json!(description)),
                    ("toolCallId", json!(call.call_id)),
                    ("status", json!(status)),
                ],
            )
        };
        if call.status == "running" {
            return Some(ExtMapping {
                detail: Some(detail),
                subagents: vec![upsert_full("running")],
                ..ExtMapping::default()
            });
        }
        let Some(details) = details else {
            return Some(if call.status == "failed" {
                ExtMapping {
                    detail: Some(detail),
                    subagents: vec![upsert(&call.call_id, &[("status", json!("failed"))])],
                    ..ExtMapping::default()
                }
            } else {
                ExtMapping::detail(detail)
            });
        };
        self.calls_by_agent
            .insert(details.agent_id.clone(), call.call_id.clone());
        let file = output_file_from_tool_result(&call.result)
            .or_else(|| self.pending_files_by_agent.get(&details.agent_id).cloned());
        self.pending_files_by_agent.remove(&details.agent_id);
        let child_sessions = self.child_session(&call.call_id, file.as_deref());
        Some(ExtMapping {
            detail: Some(detail),
            subagents: vec![upsert_full(fork_status(&details.status, false))],
            child_sessions,
            ..ExtMapping::default()
        })
    }

    fn map_followup(&mut self, call: &ExtToolCall) -> Option<ExtMapping> {
        let m = call.args.as_object()?;
        let agent_id = req_trim(m, "agent_id")?;
        let id = self.calls_by_agent.get(agent_id)?.clone();
        let details = parse_fork_details(call.details(), true);
        let detail = sub_agent_detail(
            None,
            details.as_ref().and_then(|d| d.description.as_deref()),
            log_text(&call.result),
        );
        let Some(details) = details.filter(|_| call.status != "running") else {
            return Some(ExtMapping::detail(detail));
        };
        let child_sessions = self.child_session(&id, details.transcript_path.as_deref());
        Some(ExtMapping {
            detail: Some(detail),
            subagents: vec![upsert(
                &id,
                &[("status", json!(fork_status(&details.status, false)))],
            )],
            child_sessions,
            ..ExtMapping::default()
        })
    }
}

impl ExtSession for Gotgenes {
    fn map_tool_call(&mut self, call: &ExtToolCall) -> Option<ExtMapping> {
        // This fork's spawn tool is `subagent`; its follow-up result shape differs from tintinweb's.
        match call.tool_name.as_str() {
            "subagent" => self.map_spawn(call),
            "get_subagent_result" | "steer_subagent" => self.map_followup(call),
            _ => None,
        }
    }

    fn map_custom_message(&mut self, message: &Value) -> Option<ExtMapping> {
        let ty = message.get("customType").and_then(Value::as_str);
        if ty != Some("subagent-notification") && ty != Some("subagent-update") {
            return None;
        }
        let m = message.get("details")?.as_object()?;
        let entry_id = req_trim(m, "id")?;
        let status = opt_str(m, "status")?;
        let description = opt_str(m, "description")?;
        let output_file = opt_trim(m, "outputFile")?;
        let id = self.calls_by_agent.get(entry_id)?.clone();
        let child_sessions = self.child_session(&id, output_file);
        let mut fields = Vec::new();
        if let Some(d) = description {
            fields.push(("description", json!(d)));
        }
        fields.push((
            "status",
            json!(status.map_or("running", |s| fork_status(s, false))),
        ));
        Some(ExtMapping::subagents(
            vec![upsert(&id, &fields)],
            child_sessions,
        ))
    }

    fn map_runtime_notification(&mut self, message: &str) -> Option<ExtMapping> {
        let prefix = format!("{GOTGENES_CHILD_SESSION_MARKER} ");
        let rest = message.strip_prefix(&prefix)?;
        let empty = || Some(ExtMapping::default());
        let Ok(raw) = serde_json::from_str::<Value>(rest) else {
            return empty();
        };
        let parsed = raw.as_object().and_then(|m| {
            Some((
                req_trim(m, "agentId")?.to_string(),
                req_trim(m, "file")?.to_string(),
            ))
        });
        let Some((agent_id, file)) = parsed else {
            return empty();
        };
        let Some(id) = self.calls_by_agent.get(&agent_id).cloned() else {
            self.pending_files_by_agent.insert(agent_id, file);
            return empty();
        };
        let child_sessions = self.child_session(&id, Some(&file));
        Some(ExtMapping::subagents(
            vec![upsert(&id, &[("status", json!("running"))])],
            child_sessions,
        ))
    }
}
