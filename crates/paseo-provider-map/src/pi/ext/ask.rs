//! Question/dialog extensions: `pi-ask-user` (combined select + comment form) and
//! `rpiv-ask-user-question` (multi-question form answered through deferred dialogs).

use std::collections::HashSet;

use paseo_agent_types::{Map, ToolCallDetail};
use paseo_tool_detail::detail::pi::PiToolResult;
use serde_json::{json, Value};

use super::{answer_for, DialogMapping, ExtMapping, ExtSession, ExtToolCall, UiReply};
use crate::pi::util::*;

// ---- pi-ask-user --------------------------------------------------------------------------

const QUESTION_RESPONSE_HEADER: &str = "Response";
const QUESTION_COMMENT_HEADER: &str = "Comment";
const FREEFORM_SENTINEL: &str = "✏️ Type custom response...";
const COMBINED_METADATA: &str = "ask_user_select_optional_comment";

#[derive(Default)]
pub(super) struct PiAskUser {
    /// `(allowComment, allowFreeform, allowMultiple)` while an `ask_user` call is running.
    active: Option<(bool, bool, bool)>,
    pending: Option<(String, Option<String>)>,
}

fn combined_permission(
    dialog: &Value,
    provider: &str,
    options: &[String],
    allow_freeform: bool,
    question: &str,
) -> Value {
    let visible: Vec<&String> = options.iter().filter(|o| *o != FREEFORM_SENTINEL).collect();
    let allow_other = allow_freeform || visible.len() != options.len();
    let method = dialog.get("method").cloned().unwrap_or(Value::Null);
    let mut first = json!({
        "question": question,
        "header": QUESTION_RESPONSE_HEADER,
        "options": visible.iter().map(|l| json!({"label": l})).collect::<Vec<_>>(),
        "multiSelect": false,
    });
    if allow_other {
        first["allowOther"] = json!(true);
    }
    let mut metadata = json!({
        "extensionUiMethod": method,
        "answerHeader": QUESTION_RESPONSE_HEADER,
        "commentHeader": QUESTION_COMMENT_HEADER,
        "combinedAskUser": COMBINED_METADATA,
        "selectOptions": visible,
    });
    if allow_other {
        metadata["freeformSentinel"] = json!(FREEFORM_SENTINEL);
    }
    json!({
        "id": dialog.get("id").cloned().unwrap_or(Value::Null),
        "provider": provider,
        "name": "Pi ask_user",
        "kind": "question",
        "title": question,
        "input": {"questions": [
            first,
            {
                "question": "Optional comment",
                "header": QUESTION_COMMENT_HEADER,
                "options": [],
                "multiSelect": false,
                "placeholder": "Optional comment (press Enter to skip)...",
                "allowEmpty": true,
            },
        ]},
        "metadata": metadata,
    })
}

impl ExtSession for PiAskUser {
    fn on_tool_start(&mut self, call: &ExtToolCall, _provider: &str) -> Option<Value> {
        self.active = None;
        if call.tool_name == "ask_user" {
            if let Some(m) = call.args.as_object() {
                // `z.boolean().catch(default)`
                let flag = |k: &str, d: bool| m.get(k).and_then(Value::as_bool).unwrap_or(d);
                self.active = Some((
                    flag("allowComment", false),
                    flag("allowFreeform", true),
                    flag("allowMultiple", false),
                ));
            }
        }
        None
    }

    fn on_tool_end(&mut self, call: &ExtToolCall) {
        if call.tool_name == "ask_user" {
            self.active = None;
            self.pending = None;
        }
    }

    fn map_dialog(&mut self, dialog: &Value, provider: &str) -> Option<DialogMapping> {
        let method = dialog.get("method").and_then(Value::as_str);
        if self.pending.is_some() && method == Some("input") {
            // `Input.safeParse(dialog)`: a non-string placeholder fails the parse.
            let placeholder = match dialog.get("placeholder") {
                None => None,
                Some(Value::String(s)) => Some(s.as_str()),
                Some(_) => None,
            };
            let optional = is_optional_input_placeholder(placeholder);
            let pending = self.pending.as_mut()?;
            if pending.1.is_some() && !optional {
                let value = pending.1.take()?;
                return Some(DialogMapping::Response(json!({"value": value})));
            }
            if optional {
                let value = pending.0.clone();
                self.pending = None;
                return Some(DialogMapping::Response(json!({"value": value})));
            }
        }
        let (allow_comment, allow_freeform, allow_multiple) = self.active?;
        if method != Some("select") || !allow_comment || allow_multiple {
            return None;
        }
        let parsed = (|| {
            let m = dialog.as_object()?;
            let title = opt_str(m, "title")?;
            let options = match m.get("options") {
                None => None,
                Some(Value::Array(a)) => Some(
                    a.iter()
                        .map(|o| o.as_str().map(str::to_string))
                        .collect::<Option<Vec<_>>>()?,
                ),
                Some(_) => return None,
            };
            Some((title.map(str::to_string), options))
        })();
        let (question, options) = match parsed {
            Some((title, options)) => (
                title.unwrap_or_else(|| "Select an option".into()),
                options.unwrap_or_default(),
            ),
            None => ("Select an option".into(), Vec::new()),
        };
        Some(DialogMapping::Permission(combined_permission(
            dialog,
            provider,
            &options,
            allow_freeform,
            &question,
        )))
    }

    fn respond_to_permission(&mut self, request: &Value, response: &Value) -> Option<UiReply> {
        let meta = request.get("metadata")?;
        if meta.get("combinedAskUser").and_then(Value::as_str) != Some(COMBINED_METADATA) {
            return None;
        }
        let id = request.get("id")?.as_str()?.to_string();
        let cancelled = |this: &mut Self| {
            this.pending = None;
            Some(vec![(id.clone(), json!({"cancelled": true}))])
        };
        if response.get("behavior").and_then(Value::as_str) == Some("deny") {
            return cancelled(self);
        }
        let updated = response.get("updatedInput");
        let Some(selected) = answer_for(updated, QUESTION_RESPONSE_HEADER) else {
            return cancelled(self);
        };
        let select_options: Vec<&str> = meta
            .get("selectOptions")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let sentinel = meta
            .get("freeformSentinel")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());
        let is_freeform = sentinel.is_some() && !select_options.contains(&selected.as_str());
        self.pending = Some((
            answer_for(updated, QUESTION_COMMENT_HEADER).unwrap_or_default(),
            is_freeform.then(|| selected.clone()),
        ));
        let value = if is_freeform {
            sentinel.map(str::to_string).unwrap_or(selected)
        } else {
            selected
        };
        Some(vec![(id, json!({"value": value}))])
    }
}

// ---- rpiv-ask-user-question ---------------------------------------------------------------

const RPIV_PERMISSION_PREFIX: &str = "rpiv-question:";

#[derive(Clone)]
struct RpivOption {
    label: String,
    description: String,
    preview: Option<String>,
}

#[derive(Clone)]
struct RpivQuestion {
    question: String,
    header: String,
    options: Vec<RpivOption>,
    multi_select: Option<bool>,
}

struct RpivActive {
    call_id: String,
    questions: Vec<RpivQuestion>,
    answers: Option<Map>,
    cancelled: bool,
    index: usize,
    custom_input: bool,
    deferred: Option<Value>,
}

#[derive(Default)]
pub(super) struct RpivAskUserQuestion {
    active: Option<RpivActive>,
}

/// zod `Params` (including the uniqueness refinement). Unknown keys are stripped, as in zod.
fn parse_rpiv_params(args: &Value) -> Option<Vec<RpivQuestion>> {
    let questions = req_array(args.as_object()?, "questions")?;
    if questions.is_empty() || questions.len() > 4 {
        return None;
    }
    let mut out = Vec::new();
    for q in questions {
        let qm = q.as_object()?;
        let options = req_array(qm, "options")?;
        if options.len() < 2 || options.len() > 4 {
            return None;
        }
        let mut parsed_options = Vec::new();
        for o in options {
            let om = o.as_object()?;
            parsed_options.push(RpivOption {
                label: req_str(om, "label")?.to_string(),
                description: req_str(om, "description")?.to_string(),
                preview: opt_str(om, "preview")?.map(str::to_string),
            });
        }
        out.push(RpivQuestion {
            question: req_str(qm, "question")?.to_string(),
            header: req_str(qm, "header")?.to_string(),
            options: parsed_options,
            multi_select: opt_bool(qm, "multiSelect")?,
        });
    }
    let texts: HashSet<&str> = out.iter().map(|q| q.question.as_str()).collect();
    if texts.len() != out.len() {
        return None;
    }
    for q in &out {
        let labels: HashSet<&str> = q.options.iter().map(|o| o.label.as_str()).collect();
        if labels.len() != q.options.len()
            || labels
                .iter()
                .any(|l| matches!(*l, "Other" | "Type something." | "Next"))
        {
            return None;
        }
    }
    Some(out)
}

fn reply_to_dialog(
    dialog: &Value,
    question: &RpivQuestion,
    answer: &str,
    custom_input: bool,
) -> (Value, bool) {
    // `z.array(z.string()).safeParse(dialog.options).data`
    let options: Option<Vec<&str>> = dialog
        .get("options")
        .and_then(Value::as_array)
        .and_then(|a| a.iter().map(Value::as_str).collect::<Option<Vec<_>>>());
    if custom_input {
        return (json!({"value": answer}), false);
    }
    if question.multi_select == Some(true) {
        let indices: Vec<Option<usize>> = answer
            .split(", ")
            .map(|label| question.options.iter().position(|o| o.label == label))
            .collect();
        if indices.iter().all(Option::is_some) {
            let joined = indices
                .iter()
                .map(|i| (i.unwrap_or(0) + 1).to_string())
                .collect::<Vec<_>>()
                .join(",");
            return (json!({"value": joined}), false);
        }
        return (json!({"value": answer}), false);
    }
    if let Some(index) = question.options.iter().position(|o| o.label == answer) {
        let value = options
            .as_ref()
            .and_then(|o| o.get(index).map(|s| s.to_string()))
            .unwrap_or_else(|| {
                format!(
                    "{}. {} — {}",
                    index + 1,
                    answer,
                    question.options[index].description
                )
            });
        return (json!({"value": value}), false);
    }
    let value = options
        .as_ref()
        .and_then(|o| o.get(question.options.len()).map(|s| s.to_string()))
        .unwrap_or_else(|| format!("{}. Type something.", question.options.len() + 1));
    (json!({"value": value}), true)
}

impl RpivActive {
    fn answer_dialog(&mut self, dialog: &Value) -> Value {
        let cancelled = || json!({"cancelled": true});
        if self.cancelled {
            return cancelled();
        }
        let (Some(question), Some(answers)) =
            (self.questions.get(self.index), self.answers.as_ref())
        else {
            return cancelled();
        };
        let Some(answer) = answers.get(&question.header).and_then(Value::as_str) else {
            return cancelled();
        };
        let (response, custom) = reply_to_dialog(dialog, question, answer, self.custom_input);
        self.custom_input = custom;
        if !self.custom_input {
            self.index += 1;
        }
        response
    }
}

/// zod `Result` of the tool's `details`.
fn rpiv_result_text(details: &Value) -> Option<String> {
    let m = details.as_object()?;
    let cancelled = req_bool(m, "cancelled")?;
    let error = opt_str(m, "error")?;
    let mut lines = Vec::new();
    for a in req_array(m, "answers")? {
        let am = a.as_object()?;
        let question = req_str(am, "question")?;
        let kind = req_str(am, "kind")?;
        if !matches!(kind, "option" | "custom" | "multi") {
            return None;
        }
        let answer = match am.get("answer") {
            Some(Value::Null) => None,
            Some(Value::String(s)) => Some(s.as_str()),
            _ => return None,
        };
        let selected: Option<Vec<&str>> = match am.get("selected") {
            None => None,
            Some(Value::Array(a)) => Some(a.iter().map(Value::as_str).collect::<Option<Vec<_>>>()?),
            Some(_) => return None,
        };
        let shown = if kind == "multi" {
            selected.unwrap_or_default().join(", ")
        } else {
            answer.unwrap_or("").to_string()
        };
        lines.push(format!("{question}: {shown}"));
    }
    Some(match error {
        Some(e) => e.to_string(),
        None if cancelled => "Cancelled".to_string(),
        None => lines.join("\n"),
    })
}

impl ExtSession for RpivAskUserQuestion {
    fn on_tool_start(&mut self, call: &ExtToolCall, provider: &str) -> Option<Value> {
        if call.tool_name != "ask_user_question" {
            return None;
        }
        let questions = parse_rpiv_params(&call.args)?;
        let request = json!({
            "id": format!("{RPIV_PERMISSION_PREFIX}{}", call.call_id),
            "provider": provider,
            "name": "Pi ask_user_question",
            "kind": "question",
            "title": questions[0].question,
            "input": {"questions": questions.iter().map(|q| {
                json!({
                    "question": q.question,
                    "header": q.header,
                    "options": q.options.iter().map(|o| {
                        let mut v = json!({"label": o.label, "description": o.description});
                        if let Some(p) = &o.preview {
                            v["preview"] = json!(p);
                        }
                        v
                    }).collect::<Vec<_>>(),
                    "multiSelect": q.multi_select.unwrap_or(false),
                    "allowOther": true,
                })
            }).collect::<Vec<_>>()},
        });
        self.active = Some(RpivActive {
            call_id: call.call_id.clone(),
            questions,
            answers: None,
            cancelled: false,
            index: 0,
            custom_input: false,
            deferred: None,
        });
        Some(request)
    }

    fn on_tool_end(&mut self, call: &ExtToolCall) {
        if self
            .active
            .as_ref()
            .is_some_and(|a| a.call_id == call.call_id)
        {
            self.active = None;
        }
    }

    fn map_dialog(&mut self, dialog: &Value, _provider: &str) -> Option<DialogMapping> {
        let active = self.active.as_mut()?;
        let method = dialog.get("method").and_then(Value::as_str);
        if method != Some("select") && method != Some("input") {
            return None;
        }
        if active.answers.is_none() && !active.cancelled {
            active.deferred = Some(dialog.clone());
            return Some(DialogMapping::Deferred);
        }
        Some(DialogMapping::Response(active.answer_dialog(dialog)))
    }

    fn respond_to_permission(&mut self, request: &Value, response: &Value) -> Option<UiReply> {
        let active = self.active.as_mut()?;
        let expected = format!("{RPIV_PERMISSION_PREFIX}{}", active.call_id);
        if request.get("id").and_then(Value::as_str) != Some(expected.as_str()) {
            return None;
        }
        let allow = response.get("behavior").and_then(Value::as_str) == Some("allow");
        let answers = if allow {
            response
                .get("updatedInput")
                .and_then(|u| u.get("answers"))
                .and_then(Value::as_object)
                .cloned()
        } else {
            None
        };
        active.answers = answers;
        active.cancelled = response.get("behavior").and_then(Value::as_str) == Some("deny")
            || active.answers.is_none();
        let deferred = active.deferred.take();
        Some(match deferred {
            Some(d) => {
                let id = d
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                vec![(id, active.answer_dialog(&d))]
            }
            None => Vec::new(),
        })
    }

    fn map_tool_call(&mut self, call: &ExtToolCall) -> Option<ExtMapping> {
        if call.tool_name != "ask_user_question" {
            return None;
        }
        let text = rpiv_result_text(call.details()?)?;
        if !matches!(call.result, PiToolResult::Object(_)) {
            return None;
        }
        Some(ExtMapping::detail(ToolCallDetail::PlainText {
            label: Some("Questions".into()),
            text: Some(text),
            icon: None,
        }))
    }
}
