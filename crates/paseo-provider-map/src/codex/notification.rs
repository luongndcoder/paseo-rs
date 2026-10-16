//! `CodexNotificationSchema`: raw app-server notifications -> `ParsedCodexNotification`.
//!
//! The TS union has an `invalid_payload` and an `unknown_method` arm; both only log, so they
//! collapse into [`Parsed::Ignored`] here.

use serde_json::{Map as JsonMap, Value};

type Obj = JsonMap<String, Value>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Item,
    CodexEvent,
}

#[derive(Debug, Clone)]
pub enum Parsed {
    ThreadStarted {
        thread_id: String,
    },
    TurnStarted {
        thread_id: Option<String>,
    },
    TurnCompleted {
        status: String,
        error_message: Option<String>,
        thread_id: Option<String>,
    },
    PlanUpdated {
        plan: Vec<(Option<String>, Option<String>)>,
        thread_id: Option<String>,
    },
    DiffUpdated {
        thread_id: Option<String>,
    },
    TokenUsageUpdated {
        token_usage: Value,
        thread_id: Option<String>,
    },
    AgentMessageDelta {
        item_id: String,
        delta: String,
        thread_id: Option<String>,
    },
    ReasoningDelta {
        item_id: String,
        delta: String,
        thread_id: Option<String>,
    },
    ItemCompleted {
        source: Source,
        thread_id: Option<String>,
        turn_id: Option<String>,
        item: Value,
    },
    ItemStarted {
        source: Source,
        thread_id: Option<String>,
        turn_id: Option<String>,
        item: Value,
    },
    ExecCommandStarted {
        call_id: Option<String>,
        command: Value,
        cwd: Option<String>,
        thread_id: Option<String>,
    },
    ExecCommandCompleted {
        call_id: Option<String>,
        command: Value,
        cwd: Option<String>,
        output: Option<String>,
        exit_code: Option<Value>,
        success: Option<bool>,
        stderr: Option<String>,
        thread_id: Option<String>,
    },
    ExecCommandOutputDelta {
        call_id: Option<String>,
        chunk: Option<String>,
        thread_id: Option<String>,
    },
    TerminalInteraction {
        call_id: Option<String>,
        process_id: Option<String>,
        stdin: Option<String>,
        thread_id: Option<String>,
    },
    PatchApplyStarted {
        call_id: Option<String>,
        changes: Value,
        thread_id: Option<String>,
    },
    PatchApplyCompleted {
        call_id: Option<String>,
        changes: Value,
        stdout: Option<String>,
        stderr: Option<String>,
        success: Option<bool>,
        thread_id: Option<String>,
    },
    FileChangeOutputDelta {
        item_id: String,
        delta: Option<String>,
        thread_id: Option<String>,
    },
    ThreadRolledBack {
        num_turns: i64,
        thread_id: Option<String>,
    },
    ContextCompacted {
        thread_id: String,
        turn_id: Option<String>,
    },
    Ignored,
}

impl Parsed {
    /// `getCodexNotificationThreadId`.
    pub fn thread_id(&self) -> Option<&str> {
        match self {
            Self::ThreadStarted { thread_id } => Some(thread_id),
            Self::ContextCompacted { thread_id, .. } => Some(thread_id),
            Self::TurnStarted { thread_id, .. }
            | Self::TurnCompleted { thread_id, .. }
            | Self::PlanUpdated { thread_id, .. }
            | Self::DiffUpdated { thread_id }
            | Self::TokenUsageUpdated { thread_id, .. }
            | Self::AgentMessageDelta { thread_id, .. }
            | Self::ReasoningDelta { thread_id, .. }
            | Self::ItemCompleted { thread_id, .. }
            | Self::ItemStarted { thread_id, .. }
            | Self::ExecCommandStarted { thread_id, .. }
            | Self::ExecCommandCompleted { thread_id, .. }
            | Self::ExecCommandOutputDelta { thread_id, .. }
            | Self::TerminalInteraction { thread_id, .. }
            | Self::PatchApplyStarted { thread_id, .. }
            | Self::PatchApplyCompleted { thread_id, .. }
            | Self::FileChangeOutputDelta { thread_id, .. }
            | Self::ThreadRolledBack { thread_id, .. } => thread_id.as_deref(),
            Self::Ignored => None,
        }
    }
}

/// zod `z.string().optional()`: absent is fine, any other type fails.
fn opt_str(m: &Obj, k: &str) -> Result<Option<String>, ()> {
    match m.get(k) {
        None => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(()),
    }
}

fn req_str(m: &Obj, k: &str) -> Result<String, ()> {
    match m.get(k) {
        Some(Value::String(s)) => Ok(s.clone()),
        _ => Err(()),
    }
}

fn opt_bool(m: &Obj, k: &str) -> Result<Option<bool>, ()> {
    match m.get(k) {
        None => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(()),
    }
}

/// `z.string().nullable().optional()`, folded to `Option` (null == undefined for every caller).
fn opt_nullable_str(m: &Obj, k: &str) -> Result<Option<String>, ()> {
    match m.get(k) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(()),
    }
}

fn opt_nullable_num(m: &Obj, k: &str) -> Result<Option<Value>, ()> {
    match m.get(k) {
        None | Some(Value::Null) => Ok(None),
        Some(v @ Value::Number(_)) => Ok(Some(v.clone())),
        Some(_) => Err(()),
    }
}

fn thread_id_of(m: &Obj) -> Result<Option<String>, ()> {
    opt_str(m, "threadId")
}

/// Shared prefix of every `codex/event/*` payload: `threadId`/`thread_id` plus `msg`.
struct CodexEvent<'a> {
    msg: &'a Obj,
    thread_id: Option<String>,
    turn_id: Option<String>,
}

fn codex_event<'a>(params: &'a Obj, ty: &[&str], with_turn: bool) -> Result<CodexEvent<'a>, ()> {
    let top_thread = opt_str(params, "threadId")?;
    let top_thread_snake = opt_str(params, "thread_id")?;
    let msg = match params.get("msg") {
        Some(Value::Object(m)) => m,
        _ => return Err(()),
    };
    let msg_ty = req_str(msg, "type")?;
    if !ty.contains(&msg_ty.as_str()) {
        return Err(());
    }
    let msg_thread = opt_str(msg, "threadId")?;
    let msg_thread_snake = opt_str(msg, "thread_id")?;
    let (mut top_turn, mut top_turn_snake, mut msg_turn, mut msg_turn_snake) =
        (None, None, None, None);
    if with_turn {
        top_turn = opt_str(params, "turnId")?;
        top_turn_snake = opt_str(params, "turn_id")?;
        msg_turn = opt_str(msg, "turnId")?;
        msg_turn_snake = opt_str(msg, "turn_id")?;
    }
    Ok(CodexEvent {
        msg,
        thread_id: top_thread
            .or(top_thread_snake)
            .or(msg_thread)
            .or(msg_thread_snake),
        turn_id: top_turn.or(top_turn_snake).or(msg_turn).or(msg_turn_snake),
    })
}

fn item_object(m: &Obj) -> Result<Value, ()> {
    match m.get("item") {
        Some(Value::Object(i)) => {
            opt_str(i, "id")?;
            opt_str(i, "type")?;
            Ok(Value::Object(i.clone()))
        }
        _ => Err(()),
    }
}

fn process_id(m: &Obj, k: &str) -> Result<Option<String>, ()> {
    match m.get(k) {
        None => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(Value::Number(n)) => Ok(Some(paseo_tool_detail::js::js_to_string(&Value::Number(
            n.clone(),
        )))),
        Some(_) => Err(()),
    }
}

fn parse_inner(method: &str, params: &Value) -> Result<Parsed, ()> {
    let Some(p) = params.as_object() else {
        return Ok(Parsed::Ignored);
    };
    Ok(match method {
        "thread/started" => {
            let thread = p.get("thread").and_then(Value::as_object).ok_or(())?;
            Parsed::ThreadStarted {
                thread_id: req_str(thread, "id")?,
            }
        }
        "turn/started" => {
            let thread_id = thread_id_of(p)?;
            let turn = p.get("turn").and_then(Value::as_object).ok_or(())?;
            req_str(turn, "id")?;
            Parsed::TurnStarted { thread_id }
        }
        "turn/completed" => {
            let thread_id = thread_id_of(p)?;
            let turn = p.get("turn").and_then(Value::as_object).ok_or(())?;
            opt_str(turn, "id")?;
            let status = req_str(turn, "status")?;
            let error_message = match turn.get("error") {
                None | Some(Value::Null) => None,
                Some(Value::Object(e)) => opt_str(e, "message")?,
                Some(_) => return Err(()),
            };
            Parsed::TurnCompleted {
                status,
                error_message,
                thread_id,
            }
        }
        "turn/plan/updated" => {
            let thread_id = thread_id_of(p)?;
            let plan = p.get("plan").and_then(Value::as_array).ok_or(())?;
            let mut out = Vec::new();
            for e in plan {
                let r = e.as_object().ok_or(())?;
                out.push((opt_str(r, "step")?, opt_str(r, "status")?));
            }
            Parsed::PlanUpdated {
                plan: out,
                thread_id,
            }
        }
        "turn/diff/updated" => {
            let thread_id = thread_id_of(p)?;
            req_str(p, "diff")?;
            Parsed::DiffUpdated { thread_id }
        }
        "thread/tokenUsage/updated" => Parsed::TokenUsageUpdated {
            thread_id: thread_id_of(p)?,
            // `z.unknown()`: a missing key parses as undefined.
            token_usage: p.get("tokenUsage").cloned().unwrap_or(Value::Null),
        },
        "thread/compacted" => Parsed::ContextCompacted {
            thread_id: req_str(p, "threadId")?,
            turn_id: opt_str(p, "turnId")?,
        },
        "item/agentMessage/delta" | "item/reasoning/summaryTextDelta" => {
            let thread_id = thread_id_of(p)?;
            let item_id = req_str(p, "itemId")?;
            let delta = req_str(p, "delta")?;
            if method == "item/agentMessage/delta" {
                Parsed::AgentMessageDelta {
                    item_id,
                    delta,
                    thread_id,
                }
            } else {
                Parsed::ReasoningDelta {
                    item_id,
                    delta,
                    thread_id,
                }
            }
        }
        "item/completed" | "item/started" => {
            let thread_id = thread_id_of(p)?;
            let turn_id = opt_str(p, "turnId")?;
            let item = item_object(p)?;
            if method == "item/completed" {
                Parsed::ItemCompleted {
                    source: Source::Item,
                    thread_id,
                    turn_id,
                    item,
                }
            } else {
                Parsed::ItemStarted {
                    source: Source::Item,
                    thread_id,
                    turn_id,
                    item,
                }
            }
        }
        "codex/event/item_started" | "codex/event/item_completed" => {
            let ev = codex_event(p, &["item_started", "item_completed"], true)?;
            let item = item_object(ev.msg)?;
            if method == "codex/event/item_completed" {
                Parsed::ItemCompleted {
                    source: Source::CodexEvent,
                    thread_id: ev.thread_id,
                    turn_id: ev.turn_id,
                    item,
                }
            } else {
                Parsed::ItemStarted {
                    source: Source::CodexEvent,
                    thread_id: ev.thread_id,
                    turn_id: ev.turn_id,
                    item,
                }
            }
        }
        "codex/event/exec_command_begin" => {
            let ev = codex_event(p, &["exec_command_begin"], false)?;
            Parsed::ExecCommandStarted {
                call_id: opt_str(ev.msg, "call_id")?,
                command: ev.msg.get("command").cloned().unwrap_or(Value::Null),
                cwd: opt_str(ev.msg, "cwd")?,
                thread_id: ev.thread_id,
            }
        }
        "codex/event/exec_command_end" => {
            let ev = codex_event(p, &["exec_command_end"], false)?;
            let m = ev.msg;
            opt_str(m, "formatted_output")?;
            let output = opt_nullable_str(m, "aggregated_output")?
                .or(opt_nullable_str(m, "aggregatedOutput")?)
                .or(opt_str(m, "formatted_output")?)
                .or(opt_str(m, "stdout")?);
            Parsed::ExecCommandCompleted {
                call_id: opt_str(m, "call_id")?,
                command: m.get("command").cloned().unwrap_or(Value::Null),
                cwd: opt_str(m, "cwd")?,
                output,
                exit_code: opt_nullable_num(m, "exit_code")?.or(opt_nullable_num(m, "exitCode")?),
                success: opt_bool(m, "success")?,
                stderr: opt_str(m, "stderr")?,
                thread_id: ev.thread_id,
            }
        }
        "codex/event/exec_command_output_delta" => {
            let ev = codex_event(p, &["exec_command_output_delta"], false)?;
            opt_str(ev.msg, "stream")?;
            let chunk = opt_str(ev.msg, "chunk")?;
            let delta = opt_str(ev.msg, "delta")?;
            Parsed::ExecCommandOutputDelta {
                call_id: opt_str(ev.msg, "call_id")?,
                chunk: chunk.or(delta),
                thread_id: ev.thread_id,
            }
        }
        "codex/event/terminal_interaction" => {
            let ev = codex_event(p, &["terminal_interaction"], false)?;
            Parsed::TerminalInteraction {
                call_id: opt_str(ev.msg, "call_id")?,
                process_id: process_id(ev.msg, "process_id")?,
                stdin: opt_str(ev.msg, "stdin")?,
                thread_id: ev.thread_id,
            }
        }
        "item/commandExecution/terminalInteraction" => Parsed::TerminalInteraction {
            thread_id: thread_id_of(p)?,
            call_id: opt_str(p, "itemId")?,
            process_id: process_id(p, "processId")?,
            stdin: opt_str(p, "stdin")?,
        },
        "codex/event/patch_apply_begin" => {
            let ev = codex_event(p, &["patch_apply_begin"], false)?;
            Parsed::PatchApplyStarted {
                call_id: opt_str(ev.msg, "call_id")?,
                changes: ev.msg.get("changes").cloned().unwrap_or(Value::Null),
                thread_id: ev.thread_id,
            }
        }
        "codex/event/patch_apply_end" => {
            let ev = codex_event(p, &["patch_apply_end"], false)?;
            Parsed::PatchApplyCompleted {
                call_id: opt_str(ev.msg, "call_id")?,
                changes: ev.msg.get("changes").cloned().unwrap_or(Value::Null),
                stdout: opt_str(ev.msg, "stdout")?,
                stderr: opt_str(ev.msg, "stderr")?,
                success: opt_bool(ev.msg, "success")?,
                thread_id: ev.thread_id,
            }
        }
        "item/fileChange/outputDelta" => {
            let thread_id = thread_id_of(p)?;
            let item_id = req_str(p, "itemId")?;
            let delta = opt_str(p, "delta")?;
            let chunk = opt_str(p, "chunk")?;
            Parsed::FileChangeOutputDelta {
                item_id,
                delta: delta.or(chunk),
                thread_id,
            }
        }
        "codex/event/turn_diff" => {
            let ev = codex_event(p, &["turn_diff"], false)?;
            opt_str(ev.msg, "unified_diff")?;
            opt_str(ev.msg, "diff")?;
            Parsed::DiffUpdated {
                thread_id: ev.thread_id,
            }
        }
        "codex/event/turn_aborted" => {
            let ev = codex_event(p, &["turn_aborted"], false)?;
            opt_str(ev.msg, "reason")?;
            Parsed::TurnCompleted {
                status: "interrupted".into(),
                error_message: None,
                thread_id: ev.thread_id,
            }
        }
        "codex/event/task_complete" => {
            let ev = codex_event(p, &["task_complete"], false)?;
            Parsed::TurnCompleted {
                status: "completed".into(),
                error_message: None,
                thread_id: ev.thread_id,
            }
        }
        "codex/event/thread_rolled_back" => {
            let ev = codex_event(p, &["thread_rolled_back"], false)?;
            let nonneg_int = |k: &str| -> Result<Option<i64>, ()> {
                match ev.msg.get(k) {
                    None => Ok(None),
                    Some(Value::Number(n)) => match n.as_i64() {
                        Some(i) if i >= 0 => Ok(Some(i)),
                        _ => match n.as_f64() {
                            Some(f) if f >= 0.0 && f.fract() == 0.0 => Ok(Some(f as i64)),
                            _ => Err(()),
                        },
                    },
                    Some(_) => Err(()),
                }
            };
            let num_turns = nonneg_int("num_turns")?
                .or(nonneg_int("numTurns")?)
                .unwrap_or(0);
            Parsed::ThreadRolledBack {
                num_turns,
                thread_id: ev.thread_id,
            }
        }
        _ => Parsed::Ignored,
    })
}

/// `CodexNotificationSchema.parse({method, params})`.
pub fn parse_notification(method: &str, params: &Value) -> Parsed {
    // A non-object `params` fails every typed arm; `z.unknown()` arms then log only.
    parse_inner(method, params).unwrap_or(Parsed::Ignored)
}
