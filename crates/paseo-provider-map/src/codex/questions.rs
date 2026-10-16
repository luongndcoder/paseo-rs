//! Question prompts: `request_user_input` server requests (`normalizeCodexQuestionPrompts`,
//! `mapCodexQuestionRequestToToolCall`) and Codex's async questions (`codex/async-questions.ts`).

use paseo_agent_types::{
    Map, PermissionKind, PermissionRequest, PermissionResponse, ToolCallDetail, ToolCallIcon,
    ToolCallItem, ToolCallStatus,
};
use paseo_tool_detail::js::js_trim;
use serde_json::{json, Value};

use super::tool_call::as_record;

fn non_empty_string(v: Option<&Value>) -> Option<String> {
    match v {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

/// `normalizeCodexQuestionPrompts`: validated prompts as JSON objects.
pub fn normalize_question_prompts(raw: &Value) -> Vec<Value> {
    let Value::Array(items) = raw else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for item in items {
        let Some(r) = as_record(item) else { continue };
        let (Some(id), Some(header), Some(question)) = (
            non_empty_string(r.get("id")),
            non_empty_string(r.get("header")),
            non_empty_string(r.get("question")),
        ) else {
            continue;
        };
        let options: Vec<Value> = match r.get("options") {
            Some(Value::Array(opts)) => opts
                .iter()
                .filter_map(|o| {
                    let or = as_record(o)?;
                    let label = non_empty_string(or.get("label"))?;
                    let mut m = Map::new();
                    m.insert("label".into(), Value::String(label));
                    if let Some(Value::String(d)) = or.get("description") {
                        if !js_trim(d).is_empty() {
                            m.insert("description".into(), Value::String(d.clone()));
                        }
                    }
                    Some(Value::Object(m))
                })
                .collect(),
            _ => Vec::new(),
        };
        let mut q = Map::new();
        q.insert("id".into(), Value::String(id));
        q.insert("header".into(), Value::String(header));
        q.insert("question".into(), Value::String(question));
        q.insert("options".into(), Value::Array(options));
        for (k, out_k) in [
            ("multiSelect", "multiSelect"),
            ("isOther", "isOther"),
            ("isSecret", "isSecret"),
        ] {
            if r.get(k) == Some(&Value::Bool(true)) {
                q.insert(out_k.into(), Value::Bool(true));
            }
        }
        out.push(Value::Object(q));
    }
    out
}

/// `formatCodexQuestionPrompts`.
pub fn format_question_prompts(questions: &[Value]) -> String {
    let text = questions
        .iter()
        .map(|q| {
            let header = q.get("header").and_then(Value::as_str).unwrap_or("");
            let question = q.get("question").and_then(Value::as_str).unwrap_or("");
            let mut lines = vec![format!("{header}: {question}")];
            let labels: Vec<&str> = q
                .get("options")
                .and_then(Value::as_array)
                .map(|o| o.iter().filter_map(|x| x.get("label")?.as_str()).collect())
                .unwrap_or_default();
            if !labels.is_empty() {
                lines.push(format!("Options: {}", labels.join(", ")));
            }
            lines.join("\n")
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    js_trim(&text).to_string()
}

/// `mapCodexQuestionRequestToToolCall`. `answers`: ordered `(question id, values)`.
pub fn map_question_request_to_tool_call(
    call_id: &str,
    questions: &[Value],
    status: ToolCallStatus,
    answers: Option<&[(String, Vec<String>)]>,
    error: Option<Value>,
) -> ToolCallItem {
    let formatted_questions = format_question_prompts(questions);
    let formatted_answers = answers.filter(|a| !a.is_empty()).map(|a| {
        a.iter()
            .map(|(id, values)| format!("{id}: {}", values.join(", ")))
            .collect::<Vec<_>>()
            .join("\n")
    });
    let detail_text = match (&status, &formatted_answers) {
        (ToolCallStatus::Completed, Some(a)) => {
            [formatted_questions.as_str(), "Answers:", a.as_str()]
                .iter()
                .filter(|s| !s.is_empty())
                .copied()
                .collect::<Vec<_>>()
                .join("\n\n")
        }
        _ => formatted_questions,
    };
    let detail = ToolCallDetail::PlainText {
        label: None,
        text: Some(detail_text),
        icon: Some(ToolCallIcon::Brain),
    };
    let mut metadata = Map::new();
    metadata.insert("questions".into(), Value::Array(questions.to_vec()));
    if let Some(a) = answers {
        let mut m = Map::new();
        for (id, values) in a {
            m.insert(
                id.clone(),
                Value::Array(values.iter().cloned().map(Value::String).collect()),
            );
        }
        metadata.insert("answers".into(), Value::Object(m));
    }
    let mut item = match status {
        ToolCallStatus::Failed => ToolCallItem::failed(
            call_id,
            "request_user_input",
            detail,
            error.unwrap_or_else(|| json!({"message": "Question dismissed"})),
        ),
        s => ToolCallItem::new(call_id, "request_user_input", s, detail),
    };
    item.metadata = Some(metadata);
    item
}

// ---------------------------------------------------------------------------
// async questions
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct AsyncQuestionItem {
    id: String,
    /// `(trimmed title, options)`.
    questions: Vec<(String, Vec<String>)>,
}

/// Outcome of an async question. Answers come from the client, never from the wire, so only the
/// interrupt path (`Dismissed`) is reachable here.
#[derive(Debug, Clone)]
enum Resolution {
    Dismissed,
}

fn parse_async_item(item: &Value) -> Option<AsyncQuestionItem> {
    let r = as_record(item)?;
    if r.get("type")?.as_str()? != "agentMessage" || r.get("delivery")?.as_str()? != "async" {
        return None;
    }
    let id = non_empty_string(r.get("id"))?;
    let qs = r.get("questions")?.as_array()?;
    if qs.is_empty() {
        return None;
    }
    let mut questions = Vec::new();
    for q in qs {
        let qr = as_record(q)?;
        let title = js_trim(qr.get("title")?.as_str()?).to_string();
        if title.is_empty() {
            return None;
        }
        let options = match qr.get("options") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(o)) => {
                let mut v = Vec::new();
                for x in o {
                    match x {
                        Value::String(s) if !s.is_empty() => v.push(s.clone()),
                        _ => return None,
                    }
                }
                v
            }
            Some(_) => return None,
        };
        questions.push((title, options));
    }
    Some(AsyncQuestionItem { id, questions })
}

fn request_id(item_id: &str) -> String {
    format!("permission-{item_id}")
}

fn record_timeline(item: &AsyncQuestionItem, resolution: Option<&Resolution>) -> ToolCallItem {
    let body = item
        .questions
        .iter()
        .map(|(title, options)| {
            let second = options.join(", ");
            [title.as_str(), second.as_str()]
                .iter()
                .filter(|s| !s.is_empty())
                .copied()
                .collect::<Vec<_>>()
                .join("\n")
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    let suffix = if matches!(resolution, Some(Resolution::Dismissed)) {
        "\n\nDismissed"
    } else {
        ""
    };
    ToolCallItem::new(
        item.id.clone(),
        "request_user_input_async",
        ToolCallStatus::Completed,
        ToolCallDetail::PlainText {
            label: None,
            text: Some(format!("{body}{suffix}")),
            icon: Some(ToolCallIcon::Brain),
        },
    )
}

/// `codexAsyncQuestionToTimeline`.
pub fn async_question_to_timeline(item: &Value) -> Option<ToolCallItem> {
    parse_async_item(item).map(|i| record_timeline(&i, None))
}

fn to_permission(item: &AsyncQuestionItem) -> PermissionRequest {
    let questions: Vec<Value> = item
        .questions
        .iter()
        .enumerate()
        .map(|(i, (title, options))| {
            json!({
                "id": i.to_string(),
                "header": format!("Question {}", i + 1),
                "question": title,
                "options": options.iter().map(|l| json!({"label": l})).collect::<Vec<_>>(),
                "isOther": true,
            })
        })
        .collect();
    let mut input = Map::new();
    input.insert("questions".into(), Value::Array(questions));
    PermissionRequest {
        id: request_id(&item.id),
        provider: "codex".into(),
        name: "request_user_input_async".into(),
        kind: PermissionKind::Question,
        title: Some("Question".into()),
        description: None,
        input: Some(input),
        detail: None,
        suggestions: None,
        actions: None,
        metadata: None,
    }
}

/// `CodexAsyncQuestions` (receive / pending / dismiss; the answer path is client driven).
#[derive(Debug, Default)]
pub struct AsyncQuestions {
    records: Vec<(String, AsyncQuestionItem, Option<Resolution>)>,
}

impl AsyncQuestions {
    pub fn receive(&mut self, item: &Value) -> Option<PermissionRequest> {
        let parsed = parse_async_item(item)?;
        let id = request_id(&parsed.id);
        if self.records.iter().any(|(rid, _, _)| *rid == id) {
            return None;
        }
        let perm = to_permission(&parsed);
        self.records.push((id, parsed, None));
        Some(perm)
    }

    pub fn pending(&self) -> Vec<PermissionRequest> {
        self.records
            .iter()
            .filter(|(_, _, r)| r.is_none())
            .map(|(_, i, _)| to_permission(i))
            .collect()
    }

    /// `prepareResponse(id, deny).complete()`: records the dismissal and returns the timeline row.
    pub fn dismiss(&mut self, id: &str) -> Option<ToolCallItem> {
        let rec = self.records.iter_mut().find(|(rid, _, _)| rid == id)?;
        if rec.2.is_some() {
            return None;
        }
        rec.2 = Some(Resolution::Dismissed);
        Some(record_timeline(&rec.1, rec.2.as_ref()))
    }

    pub fn clear(&mut self) {
        self.records.clear();
    }

    /// The `deny` response used when a turn is interrupted.
    pub fn interrupted_resolution() -> PermissionResponse {
        PermissionResponse::Deny {
            selected_action_id: None,
            message: Some("Interrupted".into()),
            interrupt: None,
        }
    }
}
