//! `ToolCallDetail`, `ToolCallItem` and friends. Mirrors `ToolCallDetail` /
//! `ToolCallTimelineItem` in `packages/protocol/src/agent-types.ts`.

use crate::json::*;
use serde_json::Value;

pub const TOOL_CALL_ICON_NAMES: [&str; 9] = [
    "wrench",
    "square_terminal",
    "eye",
    "pencil",
    "search",
    "bot",
    "sparkles",
    "brain",
    "mic_vocal",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCallIcon {
    Wrench,
    SquareTerminal,
    Eye,
    Pencil,
    Search,
    Bot,
    Sparkles,
    Brain,
    MicVocal,
}

impl ToolCallIcon {
    pub fn as_str(self) -> &'static str {
        TOOL_CALL_ICON_NAMES[self as usize]
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "wrench" => Self::Wrench,
            "square_terminal" => Self::SquareTerminal,
            "eye" => Self::Eye,
            "pencil" => Self::Pencil,
            "search" => Self::Search,
            "bot" => Self::Bot,
            "sparkles" => Self::Sparkles,
            "brain" => Self::Brain,
            "mic_vocal" => Self::MicVocal,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchToolName {
    Search,
    Grep,
    Glob,
    WebSearch,
}

impl SearchToolName {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Search => "search",
            Self::Grep => "grep",
            Self::Glob => "glob",
            Self::WebSearch => "web_search",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "search" => Self::Search,
            "grep" => Self::Grep,
            "glob" => Self::Glob,
            "web_search" => Self::WebSearch,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchMode {
    Content,
    FilesWithMatches,
    Count,
}

impl SearchMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Content => "content",
            Self::FilesWithMatches => "files_with_matches",
            Self::Count => "count",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "content" => Self::Content,
            "files_with_matches" => Self::FilesWithMatches,
            "count" => Self::Count,
            _ => return None,
        })
    }
}

/// A web search hit. `extra` keeps keys beyond `title`/`url` (the TS parser is passthrough, so
/// they survive in the internal timeline; the wire schema strips them).
#[derive(Debug, Clone, PartialEq)]
pub struct WebResult {
    pub title: String,
    pub url: String,
    pub extra: Map,
}

impl WebResult {
    pub fn new(title: impl Into<String>, url: impl Into<String>) -> Self {
        WebResult {
            title: title.into(),
            url: url.into(),
            extra: Map::new(),
        }
    }

    pub fn to_value(&self) -> Value {
        let mut m = Map::new();
        put(&mut m, "title", self.title.clone());
        put(&mut m, "url", self.url.clone());
        for (k, v) in &self.extra {
            if k != "title" && k != "url" {
                m.insert(k.clone(), v.clone());
            }
        }
        Value::Object(m)
    }

    pub fn from_value(v: &Value) -> Result<Self> {
        let o = Obj::new(v, "webResult")?;
        Ok(WebResult {
            title: o.req_str("title")?,
            url: o.req_str("url")?,
            extra: o.extras(&["title", "url"]),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct SearchDetail {
    pub query: String,
    pub tool_name: Option<SearchToolName>,
    pub content: Option<String>,
    pub file_paths: Option<Vec<String>>,
    pub web_results: Option<Vec<WebResult>>,
    pub annotations: Option<Vec<String>>,
    pub num_files: Option<Num>,
    pub num_matches: Option<Num>,
    pub duration_ms: Option<Num>,
    pub duration_seconds: Option<Num>,
    pub truncated: Option<bool>,
    pub mode: Option<SearchMode>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WorktreeCommand {
    pub index: Num,
    pub command: String,
    pub cwd: String,
    pub log: String,
    pub status: WorktreeCommandStatus,
    pub exit_code: Option<Num>,
    pub duration_ms: Option<Num>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorktreeCommandStatus {
    Running,
    Completed,
    Failed,
}

impl WorktreeCommandStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "running" => Self::Running,
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SubAgentAction {
    pub index: Num,
    pub tool_name: String,
    pub summary: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ToolCallDetail {
    Shell {
        command: String,
        cwd: Option<String>,
        output: Option<String>,
        /// `exitCode?: number | null`
        exit_code: Option<Option<Num>>,
    },
    Read {
        file_path: String,
        content: Option<String>,
        offset: Option<Num>,
        limit: Option<Num>,
    },
    Edit {
        file_path: String,
        old_string: Option<String>,
        new_string: Option<String>,
        unified_diff: Option<String>,
    },
    Write {
        file_path: String,
        content: Option<String>,
    },
    Search(SearchDetail),
    Fetch {
        url: String,
        prompt: Option<String>,
        result: Option<String>,
        code: Option<Num>,
        code_text: Option<String>,
        bytes: Option<Num>,
        duration_ms: Option<Num>,
    },
    WorktreeSetup {
        worktree_path: String,
        branch_name: String,
        log: String,
        commands: Vec<WorktreeCommand>,
        truncated: Option<bool>,
    },
    SubAgent {
        sub_agent_type: Option<String>,
        description: Option<String>,
        child_session_id: Option<String>,
        log: String,
        actions: Option<Vec<SubAgentAction>>,
    },
    PlainText {
        label: Option<String>,
        text: Option<String>,
        icon: Option<ToolCallIcon>,
    },
    Plan {
        text: String,
    },
    /// `input`/`output` are always serialized, `null` when absent (the wire schema rejects a
    /// missing key).
    Unknown {
        input: Value,
        output: Value,
    },
}

impl ToolCallDetail {
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Shell { .. } => "shell",
            Self::Read { .. } => "read",
            Self::Edit { .. } => "edit",
            Self::Write { .. } => "write",
            Self::Search(_) => "search",
            Self::Fetch { .. } => "fetch",
            Self::WorktreeSetup { .. } => "worktree_setup",
            Self::SubAgent { .. } => "sub_agent",
            Self::PlainText { .. } => "plain_text",
            Self::Plan { .. } => "plan",
            Self::Unknown { .. } => "unknown",
        }
    }

    pub fn is_unknown(&self) -> bool {
        matches!(self, Self::Unknown { .. })
    }

    pub fn unknown(input: Value, output: Value) -> Self {
        Self::Unknown { input, output }
    }

    pub fn to_value(&self) -> Value {
        let mut m = Map::new();
        put(&mut m, "type", self.type_name());
        match self {
            Self::Shell {
                command,
                cwd,
                output,
                exit_code,
            } => {
                put(&mut m, "command", command.clone());
                put_opt_str(&mut m, "cwd", cwd);
                put_opt_str(&mut m, "output", output);
                put_opt_nullable_num(&mut m, "exitCode", exit_code);
            }
            Self::Read {
                file_path,
                content,
                offset,
                limit,
            } => {
                put(&mut m, "filePath", file_path.clone());
                put_opt_str(&mut m, "content", content);
                put_opt_num(&mut m, "offset", offset);
                put_opt_num(&mut m, "limit", limit);
            }
            Self::Edit {
                file_path,
                old_string,
                new_string,
                unified_diff,
            } => {
                put(&mut m, "filePath", file_path.clone());
                put_opt_str(&mut m, "oldString", old_string);
                put_opt_str(&mut m, "newString", new_string);
                put_opt_str(&mut m, "unifiedDiff", unified_diff);
            }
            Self::Write { file_path, content } => {
                put(&mut m, "filePath", file_path.clone());
                put_opt_str(&mut m, "content", content);
            }
            Self::Search(s) => {
                put(&mut m, "query", s.query.clone());
                put_opt(&mut m, "toolName", s.tool_name.map(|t| t.as_str()));
                put_opt_str(&mut m, "content", &s.content);
                if let Some(p) = &s.file_paths {
                    put(&mut m, "filePaths", str_array(p));
                }
                if let Some(w) = &s.web_results {
                    put(
                        &mut m,
                        "webResults",
                        Value::Array(w.iter().map(WebResult::to_value).collect()),
                    );
                }
                if let Some(a) = &s.annotations {
                    put(&mut m, "annotations", str_array(a));
                }
                put_opt_num(&mut m, "numFiles", &s.num_files);
                put_opt_num(&mut m, "numMatches", &s.num_matches);
                put_opt_num(&mut m, "durationMs", &s.duration_ms);
                put_opt_num(&mut m, "durationSeconds", &s.duration_seconds);
                put_opt_bool(&mut m, "truncated", s.truncated);
                put_opt(&mut m, "mode", s.mode.map(|x| x.as_str()));
            }
            Self::Fetch {
                url,
                prompt,
                result,
                code,
                code_text,
                bytes,
                duration_ms,
            } => {
                put(&mut m, "url", url.clone());
                put_opt_str(&mut m, "prompt", prompt);
                put_opt_str(&mut m, "result", result);
                put_opt_num(&mut m, "code", code);
                put_opt_str(&mut m, "codeText", code_text);
                put_opt_num(&mut m, "bytes", bytes);
                put_opt_num(&mut m, "durationMs", duration_ms);
            }
            Self::WorktreeSetup {
                worktree_path,
                branch_name,
                log,
                commands,
                truncated,
            } => {
                put(&mut m, "worktreePath", worktree_path.clone());
                put(&mut m, "branchName", branch_name.clone());
                put(&mut m, "log", log.clone());
                let cmds = commands
                    .iter()
                    .map(|c| {
                        let mut cm = Map::new();
                        put(&mut cm, "index", Value::Number(c.index.clone()));
                        put(&mut cm, "command", c.command.clone());
                        put(&mut cm, "cwd", c.cwd.clone());
                        put(&mut cm, "log", c.log.clone());
                        put(&mut cm, "status", c.status.as_str());
                        put(
                            &mut cm,
                            "exitCode",
                            c.exit_code
                                .clone()
                                .map(Value::Number)
                                .unwrap_or(Value::Null),
                        );
                        put_opt_num(&mut cm, "durationMs", &c.duration_ms);
                        Value::Object(cm)
                    })
                    .collect();
                put(&mut m, "commands", Value::Array(cmds));
                put_opt_bool(&mut m, "truncated", *truncated);
            }
            Self::SubAgent {
                sub_agent_type,
                description,
                child_session_id,
                log,
                actions,
            } => {
                put_opt_str(&mut m, "subAgentType", sub_agent_type);
                put_opt_str(&mut m, "description", description);
                put_opt_str(&mut m, "childSessionId", child_session_id);
                put(&mut m, "log", log.clone());
                if let Some(a) = actions {
                    let arr = a
                        .iter()
                        .map(|x| {
                            let mut am = Map::new();
                            put(&mut am, "index", Value::Number(x.index.clone()));
                            put(&mut am, "toolName", x.tool_name.clone());
                            put_opt_str(&mut am, "summary", &x.summary);
                            Value::Object(am)
                        })
                        .collect();
                    put(&mut m, "actions", Value::Array(arr));
                }
            }
            Self::PlainText { label, text, icon } => {
                put_opt_str(&mut m, "label", label);
                put_opt_str(&mut m, "text", text);
                put_opt(&mut m, "icon", icon.map(|i| i.as_str()));
            }
            Self::Plan { text } => {
                put(&mut m, "text", text.clone());
            }
            Self::Unknown { input, output } => {
                put(&mut m, "input", input.clone());
                put(&mut m, "output", output.clone());
            }
        }
        Value::Object(m)
    }

    pub fn from_value(v: &Value) -> Result<Self> {
        let o = Obj::new(v, "detail")?;
        let ty = o.req_str("type")?;
        Ok(match ty.as_str() {
            "shell" => Self::Shell {
                command: o.req_str("command")?,
                cwd: o.opt_str("cwd")?,
                output: o.opt_str("output")?,
                exit_code: o.opt_nullable_num("exitCode")?,
            },
            "read" => Self::Read {
                file_path: o.req_str("filePath")?,
                content: o.opt_str("content")?,
                offset: o.opt_num("offset")?,
                limit: o.opt_num("limit")?,
            },
            "edit" => Self::Edit {
                file_path: o.req_str("filePath")?,
                old_string: o.opt_str("oldString")?,
                new_string: o.opt_str("newString")?,
                unified_diff: o.opt_str("unifiedDiff")?,
            },
            "write" => Self::Write {
                file_path: o.req_str("filePath")?,
                content: o.opt_str("content")?,
            },
            "search" => {
                let tool_name = match o.opt_str("toolName")? {
                    None => None,
                    Some(s) => Some(
                        SearchToolName::parse(&s)
                            .ok_or_else(|| DecodeError("detail.toolName: invalid".into()))?,
                    ),
                };
                let mode = match o.opt_str("mode")? {
                    None => None,
                    Some(s) => Some(
                        SearchMode::parse(&s)
                            .ok_or_else(|| DecodeError("detail.mode: invalid".into()))?,
                    ),
                };
                let web_results = match o.opt_array("webResults")? {
                    None => None,
                    Some(a) => Some(
                        a.iter()
                            .map(WebResult::from_value)
                            .collect::<Result<Vec<_>>>()?,
                    ),
                };
                Self::Search(SearchDetail {
                    query: o.req_str("query")?,
                    tool_name,
                    content: o.opt_str("content")?,
                    file_paths: o.opt_str_array("filePaths")?,
                    web_results,
                    annotations: o.opt_str_array("annotations")?,
                    num_files: o.opt_num("numFiles")?,
                    num_matches: o.opt_num("numMatches")?,
                    duration_ms: o.opt_num("durationMs")?,
                    duration_seconds: o.opt_num("durationSeconds")?,
                    truncated: o.opt_bool("truncated")?,
                    mode,
                })
            }
            "fetch" => Self::Fetch {
                url: o.req_str("url")?,
                prompt: o.opt_str("prompt")?,
                result: o.opt_str("result")?,
                code: o.opt_num("code")?,
                code_text: o.opt_str("codeText")?,
                bytes: o.opt_num("bytes")?,
                duration_ms: o.opt_num("durationMs")?,
            },
            "worktree_setup" => {
                let mut commands = Vec::new();
                for c in o.req_array("commands")? {
                    let co = Obj::new(c, "worktree command")?;
                    let status = co.req_str("status")?;
                    commands.push(WorktreeCommand {
                        index: co.req_num("index")?,
                        command: co.req_str("command")?,
                        cwd: co.req_str("cwd")?,
                        // wire: `log: z.string().optional().default("")`
                        log: co.opt_str("log")?.unwrap_or_default(),
                        status: WorktreeCommandStatus::parse(&status)
                            .ok_or_else(|| DecodeError("worktree command status".into()))?,
                        exit_code: co.req_nullable_num("exitCode")?,
                        duration_ms: co.opt_num("durationMs")?,
                    });
                }
                Self::WorktreeSetup {
                    worktree_path: o.req_str("worktreePath")?,
                    branch_name: o.req_str("branchName")?,
                    log: o.req_str("log")?,
                    commands,
                    truncated: o.opt_bool("truncated")?,
                }
            }
            "sub_agent" => {
                let actions = match o.opt_array("actions")? {
                    None => None,
                    Some(a) => {
                        let mut out = Vec::new();
                        for x in a {
                            let ao = Obj::new(x, "sub_agent action")?;
                            out.push(SubAgentAction {
                                index: ao.req_num("index")?,
                                tool_name: ao.req_str("toolName")?,
                                summary: ao.opt_str("summary")?,
                            });
                        }
                        Some(out)
                    }
                };
                Self::SubAgent {
                    sub_agent_type: o.opt_str("subAgentType")?,
                    description: o.opt_str("description")?,
                    child_session_id: o.opt_str("childSessionId")?,
                    log: o.req_str("log")?,
                    actions,
                }
            }
            "plain_text" => {
                let icon = match o.opt_str("icon")? {
                    None => None,
                    Some(s) => Some(
                        ToolCallIcon::parse(&s)
                            .ok_or_else(|| DecodeError("detail.icon: invalid".into()))?,
                    ),
                };
                Self::PlainText {
                    label: o.opt_str("label")?,
                    text: o.opt_str("text")?,
                    icon,
                }
            }
            "plan" => Self::Plan {
                text: o.req_str("text")?,
            },
            "unknown" => Self::Unknown {
                input: o.get("input").cloned().unwrap_or(Value::Null),
                output: o.get("output").cloned().unwrap_or(Value::Null),
            },
            other => return err(format!("detail.type: unknown variant {other}")),
        })
    }
}

impl_serde_via_value!(ToolCallDetail);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCallStatus {
    Running,
    Completed,
    Failed,
    Canceled,
}

impl ToolCallStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Canceled => "canceled",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "running" => Self::Running,
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            "canceled" => Self::Canceled,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentMessageEvent {
    Message,
    Finished,
    Errored,
    PermissionRequired,
    Closed,
}

impl AgentMessageEvent {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Message => "message",
            Self::Finished => "finished",
            Self::Errored => "errored",
            Self::PermissionRequired => "permission-required",
            Self::Closed => "closed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "message" => Self::Message,
            "finished" => Self::Finished,
            "errored" => Self::Errored,
            "permission-required" => Self::PermissionRequired,
            "closed" => Self::Closed,
            _ => return None,
        })
    }
}

/// `AgentMessage` from `packages/protocol/src/agent-message.ts`.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentMessage {
    pub event: AgentMessageEvent,
    pub sender_id: String,
    pub sender_title: Option<String>,
    pub text: String,
}

impl AgentMessage {
    pub fn to_value(&self) -> Value {
        let mut sender = Map::new();
        put(&mut sender, "id", self.sender_id.clone());
        put_opt_str(&mut sender, "title", &self.sender_title);
        let mut m = Map::new();
        put(&mut m, "event", self.event.as_str());
        put(&mut m, "sender", Value::Object(sender));
        put(&mut m, "text", self.text.clone());
        Value::Object(m)
    }

    /// Wire schema: `sender.id` and `sender.title` have `min(1)`.
    pub fn is_wire_valid(&self) -> bool {
        !self.sender_id.is_empty() && self.sender_title.as_deref() != Some("")
    }

    pub fn from_value(v: &Value) -> Result<Self> {
        let o = Obj::new(v, "agentMessage")?;
        let event = o.req_str("event")?;
        let sender_v = o
            .get("sender")
            .ok_or_else(|| DecodeError("agentMessage.sender missing".into()))?;
        let s = Obj::new(sender_v, "agentMessage.sender")?;
        Ok(AgentMessage {
            event: AgentMessageEvent::parse(&event)
                .ok_or_else(|| DecodeError("agentMessage.event invalid".into()))?,
            sender_id: s.req_str("id")?,
            sender_title: s.opt_str("title")?,
            text: o.req_str("text")?,
        })
    }
}

impl_serde_via_value!(AgentMessage);

/// A `tool_call` timeline item.
///
/// `error`: `None` means the key is absent (TS `undefined`); `Some(Value::Null)` is an explicit
/// `null`. The wire schema needs `null` for non-failed statuses and a non-null value for
/// `failed`; see `wire`.
///
/// `extra` mirrors the TS index signature on `ToolCallBase` (`[key: string]: unknown`): unknown
/// keys survive merges and round trips, but the wire schema strips them.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCallItem {
    pub call_id: String,
    pub name: String,
    pub status: ToolCallStatus,
    pub detail: ToolCallDetail,
    pub error: Option<Value>,
    pub metadata: Option<Map>,
    pub agent_message: Option<AgentMessage>,
    pub extra: Map,
}

const TOOL_CALL_KNOWN: [&str; 8] = [
    "type",
    "callId",
    "name",
    "status",
    "detail",
    "error",
    "metadata",
    "agentMessage",
];

impl ToolCallItem {
    /// A non-failed item with an explicit `error: null`.
    pub fn new(
        call_id: impl Into<String>,
        name: impl Into<String>,
        status: ToolCallStatus,
        detail: ToolCallDetail,
    ) -> Self {
        ToolCallItem {
            call_id: call_id.into(),
            name: name.into(),
            status,
            detail,
            error: Some(Value::Null),
            metadata: None,
            agent_message: None,
            extra: Map::new(),
        }
    }

    pub fn failed(
        call_id: impl Into<String>,
        name: impl Into<String>,
        detail: ToolCallDetail,
        error: Value,
    ) -> Self {
        let mut item = Self::new(call_id, name, ToolCallStatus::Failed, detail);
        item.error = Some(error);
        item
    }

    pub fn to_value(&self) -> Value {
        let mut m = Map::new();
        put(&mut m, "type", "tool_call");
        put(&mut m, "callId", self.call_id.clone());
        put(&mut m, "name", self.name.clone());
        put(&mut m, "status", self.status.as_str());
        put(&mut m, "detail", self.detail.to_value());
        if let Some(e) = &self.error {
            m.insert("error".into(), e.clone());
        }
        put_opt_map(&mut m, "metadata", &self.metadata);
        if let Some(a) = &self.agent_message {
            put(&mut m, "agentMessage", a.to_value());
        }
        for (k, v) in &self.extra {
            if !TOOL_CALL_KNOWN.contains(&k.as_str()) {
                m.insert(k.clone(), v.clone());
            }
        }
        Value::Object(m)
    }

    pub fn from_value(v: &Value) -> Result<Self> {
        let o = Obj::new(v, "tool_call")?;
        let status = o.req_str("status")?;
        let detail_v = o
            .get("detail")
            .ok_or_else(|| DecodeError("tool_call.detail missing".into()))?;
        let metadata = match o.get("metadata") {
            None | Some(Value::Null) => None,
            Some(Value::Object(m)) => Some(m.clone()),
            _ => return err("tool_call.metadata: expected object"),
        };
        let agent_message = match o.get("agentMessage") {
            None => None,
            Some(a) => Some(AgentMessage::from_value(a)?),
        };
        Ok(ToolCallItem {
            call_id: o.req_str("callId")?,
            name: o.req_str("name")?,
            status: ToolCallStatus::parse(&status)
                .ok_or_else(|| DecodeError("tool_call.status invalid".into()))?,
            detail: ToolCallDetail::from_value(detail_v)?,
            error: o.get("error").cloned(),
            metadata,
            agent_message,
            extra: o.extras(&TOOL_CALL_KNOWN),
        })
    }
}

impl_serde_via_value!(ToolCallItem);
