//! Permission request shaping (`claude/agent.ts`): kind resolution, AskUserQuestion
//! normalisation, plan actions.

use paseo_tool_detail::js::js_trim;
use serde_json::{json, Map, Value};

pub const DEFAULT_MODES: [(&str, &str, &str); 5] = [
    (
        "plan",
        "Plan Mode",
        "Analyze the codebase without executing tools or edits",
    ),
    (
        "default",
        "Always Ask",
        "Prompts for permission the first time a tool is used",
    ),
    (
        "acceptEdits",
        "Accept File Edits",
        "Automatically approves edit-focused tools without prompting",
    ),
    (
        "auto",
        "Auto mode",
        "Uses a model classifier to review permission prompts automatically",
    ),
    (
        "bypassPermissions",
        "Bypass",
        "Skip all permission prompts (use with caution)",
    ),
];

pub fn is_permission_mode(mode: &str) -> bool {
    DEFAULT_MODES.iter().any(|(id, _, _)| *id == mode)
}

pub fn default_modes_value() -> Value {
    Value::Array(
        DEFAULT_MODES
            .iter()
            .map(|(id, label, description)| {
                json!({"id": id, "label": label, "description": description})
            })
            .collect(),
    )
}

fn mode_label(mode: &str) -> String {
    DEFAULT_MODES
        .iter()
        .find(|(id, _, _)| *id == mode)
        .map(|(_, label, _)| (*label).to_owned())
        .unwrap_or_else(|| mode.to_owned())
}

/// `resolvePermissionKind`.
pub fn permission_kind(tool_name: &str, input: &Value) -> &'static str {
    if tool_name == "ExitPlanMode" {
        return "plan";
    }
    if tool_name == "AskUserQuestion" && input.get("questions").is_some_and(Value::is_array) {
        return "question";
    }
    "tool"
}

/// `normalizeClaudeAskUserQuestionRequestInput`.
pub fn normalize_question_request_input(tool_name: &str, input: &Value) -> Value {
    let Some(questions) = input.get("questions").and_then(Value::as_array) else {
        return input.clone();
    };
    if tool_name != "AskUserQuestion" {
        return input.clone();
    }
    let mut out = input.as_object().cloned().unwrap_or_default();
    let mapped: Vec<Value> = questions
        .iter()
        .map(|q| match q.as_object() {
            Some(o) => {
                let mut o = o.clone();
                o.insert("allowOther".into(), Value::Bool(true));
                Value::Object(o)
            }
            None => q.clone(),
        })
        .collect();
    out.insert("questions".into(), Value::Array(mapped));
    Value::Object(out)
}

/// `buildClaudeQuestionPermissionSummary`: optional `title` / `description`.
pub fn question_permission_summary(
    tool_name: &str,
    input: &Value,
) -> (Option<String>, Option<String>) {
    if tool_name != "AskUserQuestion" {
        return (None, None);
    }
    let Some(questions) = input.get("questions").and_then(Value::as_array) else {
        return (None, None);
    };
    let question = questions.iter().find(|q| q.is_object() || q.is_array());
    let title = question
        .and_then(|q| q.get("question"))
        .and_then(Value::as_str)
        .map(|s| js_trim(s).to_owned())
        .unwrap_or_default();
    if title.is_empty() {
        return (None, None);
    }
    let labels: Vec<String> = question
        .and_then(|q| q.get("options"))
        .and_then(Value::as_array)
        .map(|options| {
            options
                .iter()
                .map(|o| match o {
                    Value::String(s) => js_trim(s).to_owned(),
                    other => other
                        .get("label")
                        .and_then(Value::as_str)
                        .map(|s| js_trim(s).to_owned())
                        .unwrap_or_default(),
                })
                .filter(|l| !l.is_empty())
                .collect()
        })
        .unwrap_or_default();
    if labels.is_empty() {
        (Some(title), None)
    } else {
        (Some(title), Some(labels.join(" / ")))
    }
}

/// `buildClaudePlanPermissionActions`.
pub fn plan_permission_actions(resume_mode: Option<&str>) -> Value {
    let mut actions = vec![
        json!({"id": "reject", "label": "Reject", "behavior": "deny", "variant": "danger", "intent": "dismiss"}),
        json!({"id": "implement", "label": "Implement", "behavior": "allow", "variant": "primary", "intent": "implement"}),
    ];
    if resume_mode == Some("bypassPermissions") {
        actions.push(json!({
            "id": "implement_resume",
            "label": format!("Implement with {}", mode_label("bypassPermissions")),
            "behavior": "allow",
            "variant": "secondary",
            "intent": "implement_resume",
        }));
    }
    Value::Array(actions)
}

/// `resolveClaudeAskUserQuestionAnswers`: answers keyed by full question text, `None` without a
/// question list and an answers map.
pub fn resolve_question_answers(
    updated: Option<&Value>,
    fallback: Option<&Value>,
) -> Option<Map<String, Value>> {
    let base = updated.filter(|v| v.is_object());
    let fallback = fallback.filter(|v| v.is_object());
    let questions = base
        .and_then(|b| b.get("questions"))
        .and_then(Value::as_array)
        .or_else(|| {
            fallback
                .and_then(|f| f.get("questions"))
                .and_then(Value::as_array)
        })?;
    let answers = base
        .and_then(|b| b.get("answers"))
        .and_then(Value::as_object)?;
    let non_empty = |v: Option<&Value>| -> Option<String> {
        let s = v?.as_str()?;
        (!js_trim(s).is_empty()).then(|| s.to_owned())
    };
    let mut out = Map::new();
    for q in questions {
        if !q.is_object() {
            continue;
        }
        let Some(text) = non_empty(q.get("question")) else {
            continue;
        };
        let header = non_empty(q.get("header"));
        let answer = non_empty(answers.get(&text))
            .or_else(|| header.and_then(|h| non_empty(answers.get(&h))));
        if let Some(a) = answer {
            out.insert(text, Value::String(a));
        }
    }
    Some(out)
}
