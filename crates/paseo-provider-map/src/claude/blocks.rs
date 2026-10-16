//! Content block mapping (`mapBlocksToTimeline`), the tool-use cache, streamed tool input and
//! tool results (`claude/agent.ts`).

use serde_json::{json, Map, Value};

use paseo_tool_detail::detail::claude::{
    map_claude_canceled_tool_call, map_claude_completed_tool_call, map_claude_failed_tool_call,
    map_claude_running_tool_call, ClaudeToolCallParams,
};

use super::images::render_image_as_assistant_markdown;
use super::partial_json::parse_partial_json_object;
use super::text::{
    coerce_tool_result_content_to_string, is_transcript_noise_str, normalize_js_numbers,
    split_tool_result_images, str_of, INTERRUPT_TOOL_USE_PLACEHOLDER,
};
use super::{ClaudeMapper, ToolEntry};

#[derive(Debug, Clone, Copy)]
pub(super) struct BlockOpts {
    user_message: bool,
    suppress_text: bool,
    suppress_reasoning: bool,
}

impl BlockOpts {
    /// Assistant blocks (`textMessageType` `assistant_message`).
    pub(super) fn assistant(suppress_text: bool, suppress_reasoning: bool) -> Self {
        Self {
            user_message: false,
            suppress_text,
            suppress_reasoning,
        }
    }

    /// User blocks: text coalesces into one `user_message`, never suppressed.
    pub(super) fn user() -> Self {
        Self {
            user_message: true,
            suppress_text: false,
            suppress_reasoning: false,
        }
    }
}

fn running(name: &str, call_id: Option<&str>, input: Option<&Value>) -> Option<Value> {
    map_claude_running_tool_call(&ClaudeToolCallParams {
        call_id,
        name,
        input,
        output: None,
        metadata: None,
    })
    .map(|i| i.to_value())
}

pub(super) fn canceled_tool_call(
    name: &str,
    call_id: &str,
    input: Option<&Value>,
) -> Option<Value> {
    map_claude_canceled_tool_call(&ClaudeToolCallParams {
        call_id: Some(call_id),
        name,
        input,
        output: None,
        metadata: None,
    })
    .map(|i| i.to_value())
}

pub(super) fn failed_tool_call(
    name: &str,
    call_id: Option<&str>,
    input: Option<&Value>,
    output: Option<&Value>,
    metadata: Option<&Map<String, Value>>,
    error: &Value,
) -> Option<Value> {
    map_claude_failed_tool_call(
        &ClaudeToolCallParams {
            call_id,
            name,
            input,
            output,
            metadata,
        },
        error,
    )
    .map(|i| i.to_value())
}

pub(super) fn completed_tool_call(
    name: &str,
    call_id: Option<&str>,
    input: Option<&Value>,
    output: Option<&Value>,
) -> Option<Value> {
    map_claude_completed_tool_call(&ClaudeToolCallParams {
        call_id,
        name,
        input,
        output,
        metadata: None,
    })
    .map(|i| i.to_value())
}

fn is_chunk(v: &Value) -> bool {
    v.as_object()
        .is_some_and(|o| o.get("type").is_some_and(Value::is_string))
}

fn nonempty_str<'a>(o: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    o.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn is_command_tool(name: &str, input: &Value) -> bool {
    let n = name.to_lowercase();
    if ["bash", "shell", "terminal", "command"]
        .iter()
        .any(|k| n.contains(k))
    {
        return true;
    }
    matches!(
        input.get("command"),
        Some(Value::String(_) | Value::Array(_))
    )
}

fn extract_command_text(input: &Value) -> Option<String> {
    match input.get("command") {
        Some(Value::String(s)) if !s.is_empty() => return Some(s.clone()),
        Some(Value::Array(a)) => {
            let tokens: Vec<&str> = a.iter().filter_map(Value::as_str).collect();
            if !tokens.is_empty() {
                return Some(tokens.join(" "));
            }
        }
        _ => {}
    }
    input
        .get("description")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn posix_relative(from: &str, to: &str) -> String {
    let split = |p: &str| -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for seg in p.split('/') {
            match seg {
                "" | "." => {}
                ".." => {
                    out.pop();
                }
                s => out.push(s.to_owned()),
            }
        }
        out
    };
    let (f, t) = (split(from), split(to));
    let common = f.iter().zip(&t).take_while(|(a, b)| a == b).count();
    let mut parts: Vec<String> = vec!["..".to_owned(); f.len() - common];
    parts.extend(t[common..].iter().cloned());
    parts.join("/")
}

fn basename(path: &str) -> String {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(path)
        .to_owned()
}

fn parse_patch_file_list(patch: &str) -> Vec<(String, String)> {
    let mut files: Vec<(String, String)> = Vec::new();
    for line in patch.split('\n') {
        let trimmed = paseo_tool_detail::js::js_trim(line.trim_end_matches('\r'));
        let (kind, rest) = if let Some(r) = trimmed.strip_prefix("*** Add File:") {
            ("add", r)
        } else if let Some(r) = trimmed.strip_prefix("*** Delete File:") {
            ("delete", r)
        } else if let Some(r) = trimmed.strip_prefix("*** Update File:") {
            ("update", r)
        } else {
            continue;
        };
        let path = paseo_tool_detail::js::js_trim(rest).to_owned();
        if path.is_empty() || files.iter().any(|(p, k)| p == &path && k == kind) {
            continue;
        }
        files.push((path, kind.to_owned()));
    }
    files
}

fn is_file_write_tool(t: &str) -> bool {
    t.contains("write") || t == "write_file" || t == "create_file"
}

fn is_file_edit_tool(t: &str) -> bool {
    t.contains("edit") || t.contains("patch") || t == "apply_patch" || t == "apply_diff"
}

fn is_file_read_tool(t: &str) -> bool {
    t.contains("read") || t == "read_file" || t == "view_file"
}

fn first_string_field(input: &Value, primary: &str, secondary: &str) -> Option<String> {
    input
        .get(primary)
        .and_then(Value::as_str)
        .or_else(|| input.get(secondary).and_then(Value::as_str))
        .map(str::to_owned)
}

fn put_opt(m: &mut Map<String, Value>, key: &str, v: Option<String>) {
    if let Some(v) = v {
        m.insert(key.into(), Value::String(v));
    }
}

impl ClaudeMapper {
    fn entry_index(&self, id: &str) -> Option<usize> {
        self.tool_use_cache.iter().position(|e| e.id == id)
    }

    fn relativize_path(&self, target: &str) -> Option<String> {
        if target.is_empty() {
            return None;
        }
        if let Some(cwd) = self.cwd.as_deref().filter(|c| !c.is_empty()) {
            if target.starts_with(cwd) {
                let relative = posix_relative(cwd, target);
                return Some(if relative.is_empty() {
                    basename(target)
                } else {
                    relative
                });
            }
        }
        Some(target.to_owned())
    }

    fn detect_file_kind(path: &str) -> &'static str {
        if std::path::Path::new(path).exists() {
            "update"
        } else {
            "add"
        }
    }

    fn extract_file_changes(&self, input: &Value) -> Vec<(String, String)> {
        if let Some(fp) = input
            .get("file_path")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            if let Some(rel) = self.relativize_path(fp) {
                return vec![(rel, Self::detect_file_kind(fp).to_owned())];
            }
        }
        if let Some(patch) = input
            .get("patch")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            let files = parse_patch_file_list(patch);
            if !files.is_empty() {
                return files
                    .into_iter()
                    .map(|(p, k)| (self.relativize_path(&p).unwrap_or(p), k))
                    .collect();
            }
        }
        if let Some(Value::Array(list)) = input.get("files") {
            let files: Vec<(String, String)> = list
                .iter()
                .filter_map(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(|v| {
                    (
                        self.relativize_path(v).unwrap_or_else(|| v.to_owned()),
                        Self::detect_file_kind(v).to_owned(),
                    )
                })
                .collect();
            if !files.is_empty() {
                return files;
            }
        }
        Vec::new()
    }

    fn apply_tool_input(&self, entry: &mut ToolEntry, input: &Value) {
        entry.input = Some(input.clone());
        if !is_command_tool(&entry.name, input) {
            let files = self.extract_file_changes(input);
            if !files.is_empty() {
                entry.files = files;
            }
        }
    }

    /// `upsertToolUseEntry`: returns the entry's index in the cache.
    fn upsert_tool_use_entry(&mut self, block: &Map<String, Value>) -> Option<usize> {
        let id = nonempty_str(block, "id")?.to_owned();
        let idx = match self.entry_index(&id) {
            Some(i) => i,
            None => {
                let name = nonempty_str(block, "name").unwrap_or("tool").to_owned();
                let server = nonempty_str(block, "server")
                    .or_else(|| nonempty_str(block, "name"))
                    .unwrap_or("tool")
                    .to_owned();
                self.tool_use_cache.push(ToolEntry {
                    id: id.clone(),
                    name,
                    server,
                    started: false,
                    files: Vec::new(),
                    input: None,
                });
                self.tool_use_cache.len() - 1
            }
        };
        if let Some(name) = nonempty_str(block, "name") {
            self.tool_use_cache[idx].name = name.to_owned();
        }
        if let Some(server) = nonempty_str(block, "server") {
            self.tool_use_cache[idx].server = server.to_owned();
        }
        if matches!(
            str_of(block.get("type")),
            Some("tool_use" | "mcp_tool_use" | "server_tool_use")
        ) {
            if let Some(input) = block.get("input").filter(|i| i.is_object() || i.is_array()) {
                let mut entry = self.tool_use_cache[idx].clone();
                self.apply_tool_input(&mut entry, input);
                self.tool_use_cache[idx] = entry;
            }
        }
        Some(idx)
    }

    /// `mapBlocksToTimeline`.
    pub(super) fn map_blocks_to_timeline(
        &mut self,
        content: &Value,
        opts: BlockOpts,
    ) -> Vec<Value> {
        let text_type = if opts.user_message {
            "user_message"
        } else {
            "assistant_message"
        };
        let suppress_text = !opts.user_message && opts.suppress_text;
        if let Value::String(s) = content {
            if s.is_empty()
                || s == INTERRUPT_TOOL_USE_PLACEHOLDER
                || is_transcript_noise_str(s)
                || suppress_text
            {
                return Vec::new();
            }
            return vec![json!({"type": text_type, "text": s})];
        }
        let Some(blocks) = content.as_array() else {
            return Vec::new();
        };
        let mut items = Vec::new();
        let mut user_text_parts: Vec<String> = Vec::new();
        for block in blocks {
            if !is_chunk(block) || self.map_subagent_handback_block(block, &mut items) {
                continue;
            }
            self.map_block_to_timeline(
                block,
                &mut items,
                &mut user_text_parts,
                opts,
                suppress_text,
            );
        }
        if opts.user_message && !user_text_parts.is_empty() {
            items.insert(
                0,
                json!({"type": "user_message", "text": user_text_parts.join("\n\n")}),
            );
        }
        items
    }

    fn map_block_to_timeline(
        &mut self,
        block: &Value,
        items: &mut Vec<Value>,
        user_text_parts: &mut Vec<String>,
        opts: BlockOpts,
        suppress_text: bool,
    ) {
        let Some(o) = block.as_object() else {
            return;
        };
        match str_of(o.get("type")).unwrap_or("") {
            "text" | "text_delta" => {
                let text = o.get("text").and_then(Value::as_str).unwrap_or("");
                if text.is_empty()
                    || text == INTERRUPT_TOOL_USE_PLACEHOLDER
                    || is_transcript_noise_str(text)
                {
                    return;
                }
                if opts.user_message {
                    let trimmed = paseo_tool_detail::js::js_trim(text);
                    if !trimmed.is_empty() {
                        user_text_parts.push(trimmed.to_owned());
                    }
                    return;
                }
                if !suppress_text {
                    items.push(json!({"type": "assistant_message", "text": text}));
                }
            }
            "thinking" | "thinking_delta" => {
                if let Some(t) = o
                    .get("thinking")
                    .and_then(Value::as_str)
                    .filter(|t| !t.is_empty())
                {
                    if !opts.suppress_reasoning {
                        items.push(json!({"type": "reasoning", "text": t}));
                    }
                }
            }
            "tool_use" | "server_tool_use" | "mcp_tool_use" => self.handle_tool_use_start(o, items),
            "tool_result"
            | "mcp_tool_result"
            | "web_fetch_tool_result"
            | "web_search_tool_result"
            | "code_execution_tool_result"
            | "bash_code_execution_tool_result"
            | "text_editor_code_execution_tool_result" => self.handle_tool_result(o, items),
            _ => {}
        }
    }

    /// `mapSubagentHandbackBlock`.
    fn map_subagent_handback_block(&mut self, block: &Value, items: &mut Vec<Value>) -> bool {
        let Some(o) = block.as_object() else {
            return false;
        };
        if let Some(id) = o.get("tool_use_id").and_then(Value::as_str) {
            if self.subagent_handback_ids.remove(id) {
                return true;
            }
        }
        if str_of(o.get("type")) != Some("tool_use")
            || str_of(o.get("name")) != Some("SubagentHandback")
        {
            return false;
        }
        let Some(id) = nonempty_str(o, "id") else {
            return false;
        };
        self.subagent_handback_ids.insert(id.to_owned());
        let report = o
            .get("input")
            .and_then(|i| i.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("");
        if !paseo_tool_detail::js::js_trim(report).is_empty() {
            items.push(json!({"type": "assistant_message", "text": report}));
        }
        true
    }

    /// `handleToolUseStart`.
    fn handle_tool_use_start(&mut self, block: &Map<String, Value>, items: &mut Vec<Value>) {
        let Some(idx) = self.upsert_tool_use_entry(block) else {
            return;
        };
        if self.tool_use_cache[idx].started {
            return;
        }
        self.tool_use_cache[idx].started = true;
        let entry = self.tool_use_cache[idx].clone();
        let input = entry
            .input
            .clone()
            .or_else(|| {
                block
                    .get("input")
                    .filter(|i| i.is_object() || i.is_array())
                    .cloned()
            })
            .unwrap_or(Value::Null);
        if let Some(item) = running(&entry.name, Some(&entry.id), Some(&input)) {
            items.push(item);
        }
    }

    /// `handleToolResult`.
    fn handle_tool_result(&mut self, block: &Map<String, Value>, items: &mut Vec<Value>) {
        let tool_use_id = block.get("tool_use_id").and_then(Value::as_str);
        let entry = tool_use_id
            .and_then(|id| self.entry_index(id))
            .map(|i| self.tool_use_cache[i].clone());
        let block_tool_name = block.get("tool_name").and_then(Value::as_str);
        let tool_name = entry
            .as_ref()
            .map(|e| e.name.as_str())
            .or(block_tool_name)
            .unwrap_or("tool")
            .to_owned();
        let call_id = tool_use_id
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .or_else(|| entry.as_ref().map(|e| e.id.clone()));

        let (images, text) = split_tool_result_images(block.get("content"));
        let is_error = block.get("is_error").is_some_and(super::text::js_truthy);
        let output = self.build_tool_output(text.as_ref(), block, entry.as_ref());
        let input = entry.as_ref().and_then(|e| e.input.as_ref());

        if is_error {
            let mut error = block.clone();
            match &text {
                Some(t) => {
                    error.insert("content".into(), t.clone());
                }
                None => {
                    error.shift_remove("content");
                }
            }
            if let Some(item) = failed_tool_call(
                &tool_name,
                call_id.as_deref(),
                input,
                output.as_ref(),
                None,
                &Value::Object(error),
            ) {
                items.push(item);
            }
        } else if let Some(item) =
            completed_tool_call(&tool_name, call_id.as_deref(), input, output.as_ref())
        {
            items.push(item);
        }

        for image in &images {
            if let Some(item) = render_image_as_assistant_markdown(image) {
                items.push(item);
            }
        }
        if let Some(id) = tool_use_id {
            self.tool_use_cache.retain(|e| e.id != id);
        }
    }

    /// `buildToolOutput`; `None` is `undefined`.
    fn build_tool_output(
        &self,
        content: Option<&Value>,
        block: &Map<String, Value>,
        entry: Option<&ToolEntry>,
    ) -> Option<Value> {
        if block.get("is_error").is_some_and(super::text::js_truthy) {
            return None;
        }
        let server = entry
            .map(|e| e.server.as_str())
            .or_else(|| block.get("server").and_then(Value::as_str))
            .unwrap_or("tool");
        let tool = entry
            .map(|e| e.name.as_str())
            .or_else(|| block.get("tool_name").and_then(Value::as_str))
            .unwrap_or("tool");
        let coerced = coerce_tool_result_content_to_string(content);
        let input = entry.and_then(|e| e.input.as_ref());
        if let Some(structured) = self.build_structured_tool_result(server, tool, &coerced, input) {
            return Some(structured);
        }
        let mut result = Map::new();
        if !coerced.is_empty() {
            let parsed = serde_json::from_str::<Value>(&coerced)
                .map(normalize_js_numbers)
                .unwrap_or_else(|_| Value::String(coerced.clone()));
            result.insert("output".into(), parsed);
        }
        if let Some(e) = entry.filter(|e| !e.files.is_empty()) {
            result.insert(
                "files".into(),
                Value::Array(
                    e.files
                        .iter()
                        .map(|(path, kind)| json!({"path": path, "kind": kind}))
                        .collect(),
                ),
            );
        }
        (!result.is_empty()).then_some(Value::Object(result))
    }

    fn build_structured_tool_result(
        &self,
        server: &str,
        tool: &str,
        output: &str,
        input: Option<&Value>,
    ) -> Option<Value> {
        let ns = server.to_lowercase();
        let nt = tool.to_lowercase();
        let is_command = ["bash", "shell", "command"]
            .iter()
            .any(|k| ns.contains(k) || nt.contains(k))
            || input.is_some_and(|i| {
                matches!(i.get("command"), Some(Value::String(_) | Value::Array(_)))
            });
        if is_command {
            let empty = json!({});
            let command = extract_command_text(input.unwrap_or(&empty))
                .unwrap_or_else(|| "command".to_owned());
            let mut m = Map::new();
            m.insert("type".into(), json!("command"));
            m.insert("command".into(), json!(command));
            m.insert("output".into(), json!(output));
            put_opt(
                &mut m,
                "cwd",
                input
                    .and_then(|i| i.get("cwd"))
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            );
            return Some(Value::Object(m));
        }
        let file_path = input
            .and_then(|i| i.get("file_path"))
            .and_then(Value::as_str)?;
        let input = input?;
        if is_file_write_tool(&nt) {
            let new_content = input
                .get("content")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| output.to_owned());
            return Some(json!({
                "type": "file_write",
                "filePath": file_path,
                "oldContent": "",
                "newContent": new_content,
            }));
        }
        if is_file_edit_tool(&nt) {
            let mut m = Map::new();
            m.insert("type".into(), json!("file_edit"));
            m.insert("filePath".into(), json!(file_path));
            put_opt(&mut m, "diff", first_string_field(input, "patch", "diff"));
            put_opt(
                &mut m,
                "oldContent",
                first_string_field(input, "old_str", "old_string"),
            );
            put_opt(
                &mut m,
                "newContent",
                first_string_field(input, "new_str", "new_string"),
            );
            return Some(Value::Object(m));
        }
        if is_file_read_tool(&nt) {
            return Some(json!({"type": "file_read", "filePath": file_path, "content": output}));
        }
        None
    }

    // ---- partial (streamed) events --------------------------------------------------------------

    /// `updatePartialEventToolState`: true when the event was a tool input delta.
    fn update_partial_event_tool_state(&mut self, event: &Value) -> bool {
        let index = event.get("index").and_then(Value::as_i64);
        match str_of(event.get("type")) {
            Some("content_block_start") => {
                if let Some(block) = event.get("content_block").filter(|b| is_chunk(b)) {
                    if str_of(block.get("type")) == Some("tool_use") {
                        if let (Some(index), Some(id)) =
                            (index, block.get("id").and_then(Value::as_str))
                        {
                            self.tool_use_index_to_id.insert(index, id.to_owned());
                            self.tool_use_input_buffers.remove(id);
                        }
                    }
                }
                false
            }
            Some("content_block_delta") => {
                let Some(delta) = event.get("delta").filter(|d| is_chunk(d)) else {
                    return false;
                };
                if str_of(delta.get("type")) == Some("input_json_delta") {
                    let partial = delta.get("partial_json").and_then(Value::as_str);
                    self.handle_tool_input_delta(index, partial);
                    return true;
                }
                false
            }
            Some("content_block_stop") => {
                if let Some(index) = index {
                    if let Some(id) = self.tool_use_index_to_id.remove(&index) {
                        self.tool_use_input_buffers.remove(&id);
                    }
                }
                false
            }
            _ => false,
        }
    }

    /// `mapPartialEvent`.
    pub(super) fn map_partial_event(&mut self, event: &Value, opts: BlockOpts) -> Vec<Value> {
        if self.update_partial_event_tool_state(event) {
            return Vec::new();
        }
        let key = match str_of(event.get("type")) {
            Some("content_block_start") => "content_block",
            Some("content_block_delta") => "delta",
            _ => return Vec::new(),
        };
        match event.get(key).filter(|b| is_chunk(b)) {
            Some(block) => self.map_blocks_to_timeline(&Value::Array(vec![block.clone()]), opts),
            None => Vec::new(),
        }
    }

    /// `handleToolInputDelta`.
    fn handle_tool_input_delta(&mut self, index: Option<i64>, partial_json: Option<&str>) {
        let (Some(index), Some(partial)) = (index, partial_json) else {
            return;
        };
        let Some(tool_id) = self.tool_use_index_to_id.get(&index).cloned() else {
            return;
        };
        let buffer = {
            let b = self
                .tool_use_input_buffers
                .entry(tool_id.clone())
                .or_default();
            b.push_str(partial);
            b.clone()
        };
        let Some(idx) = self.entry_index(&tool_id) else {
            return;
        };
        let Some((parsed, complete)) = parse_partial_json_object(&buffer) else {
            return;
        };
        let normalized = Value::Object(parsed);
        if !complete && normalized.as_object().is_some_and(Map::is_empty) {
            return;
        }
        if inputs_equal(self.tool_use_cache[idx].input.as_ref(), &normalized) {
            return;
        }
        let mut entry = self.tool_use_cache[idx].clone();
        self.apply_tool_input(&mut entry, &normalized);
        self.tool_use_cache[idx] = entry.clone();
        if let Some(item) = running(&entry.name, Some(&tool_id), Some(&normalized)) {
            self.notify_timeline(item);
        }
    }
}

/// `areToolInputsEqual`: shallow `===` per key; containers are never identical.
fn inputs_equal(left: Option<&Value>, right: &Value) -> bool {
    let Some(left) = left.and_then(Value::as_object) else {
        return false;
    };
    let Some(right) = right.as_object() else {
        return false;
    };
    if left.len() != right.len() {
        return false;
    }
    right
        .iter()
        .all(|(k, rv)| !matches!(rv, Value::Object(_) | Value::Array(_)) && left.get(k) == Some(rv))
}
