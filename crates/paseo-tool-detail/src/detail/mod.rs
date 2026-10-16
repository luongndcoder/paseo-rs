//! Per-provider tool-call detail parsers (`deriveClaudeToolDetail`, `deriveCodexToolDetail`,
//! `deriveOpencodeToolDetail`, Pi `mapToolDetail`) and the shared primitives.

pub mod claude;
pub mod codex;
pub mod opencode;
pub mod pi;
pub mod primitives;

use paseo_agent_types::ToolCallDetail;
use primitives::{nullable, P};
use serde_json::Value;

/// One `toolDetailBranchBy*` branch: parse nullable input and output, then map. `None` means a
/// parse failure (the branch does not match); `Some(None)` means it matched but produced no
/// detail (TS `undefined`), which still ends the union.
pub(crate) fn branch<I, O>(
    input: Option<&Value>,
    output: Option<&Value>,
    parse_in: impl FnOnce(&Value) -> P<I>,
    parse_out: impl FnOnce(&Value) -> P<O>,
    map: impl FnOnce(Option<&I>, Option<&O>) -> Option<ToolCallDetail>,
) -> Option<Option<ToolCallDetail>> {
    let i = nullable(input, parse_in).ok()?;
    let o = nullable(output, parse_out).ok()?;
    Some(map(i.as_ref(), o.as_ref()))
}

/// Branch whose output schema is `z.unknown()`.
pub(crate) fn branch_in<I>(
    input: Option<&Value>,
    parse_in: impl FnOnce(&Value) -> P<I>,
    map: impl FnOnce(Option<&I>) -> Option<ToolCallDetail>,
) -> Option<Option<ToolCallDetail>> {
    let i = nullable(input, parse_in).ok()?;
    Some(map(i.as_ref()))
}

pub(crate) fn unknown_detail(input: Option<&Value>, output: Option<&Value>) -> ToolCallDetail {
    ToolCallDetail::Unknown {
        input: input.cloned().unwrap_or(Value::Null),
        output: output.cloned().unwrap_or(Value::Null),
    }
}

/// Speak tool input: `string | {text: string}` (nullable). Returns the trimmed text.
pub(crate) fn speak_detail(input: Option<&Value>) -> Option<Option<ToolCallDetail>> {
    let text = nullable(input, |v| match v {
        Value::String(s) => Ok(s.clone()),
        Value::Object(m) => match m.get("text") {
            Some(Value::String(s)) => Ok(s.clone()),
            _ => Err(()),
        },
        _ => Err(()),
    })
    .ok()?;
    let trimmed = text
        .as_deref()
        .map(crate::js::js_trim)
        .unwrap_or("")
        .to_string();
    if trimmed.is_empty() {
        return Some(None);
    }
    Some(Some(ToolCallDetail::Unknown {
        input: Value::String(trimmed),
        output: Value::Null,
    }))
}

/// Skill output: `{output: string} | string`, nullable.
pub(crate) fn skill_output(v: &Value) -> P<String> {
    match v {
        Value::String(s) => Ok(s.clone()),
        Value::Object(m) => match m.get("output") {
            Some(Value::String(s)) => Ok(s.clone()),
            _ => Err(()),
        },
        _ => Err(()),
    }
}
