//! Browser tools: thin wrappers over the browser automation broker (`browser-tools/tools.ts`).
//!
//! Each tool builds a broker command, runs it through [`CoreApi::browser_execute`] and renders
//! the payload into model-readable text plus structured content.

use std::sync::Arc;

use serde_json::{json, Map, Value};

use super::{handler, str_arg, ToolEnv, ToolOutcome};
use crate::catalog::Registrar;
use crate::types::ToolResult;

const WORKSPACE_CONTEXT_MESSAGE: &str = "This browser tool needs a workspace. Start the agent from a Paseo workspace before calling browser_new_tab or browser_list_tabs.";

type Args = fn(&Value) -> Map<String, Value>;

#[derive(Clone, Copy)]
struct Spec {
    name: &'static str,
    command: &'static str,
    args: Args,
    /// `browser_list_tabs` and `browser_new_tab` need a workspace before they run.
    needs_workspace: bool,
    /// `browser_wait` widens the broker timeout by one second over its own.
    widens_timeout: bool,
    /// Tab-scoped tools echo `browserId` in the result context.
    tab_scoped: bool,
}

fn copy(input: &Value, key: &str) -> Value {
    input.get(key).cloned().unwrap_or(Value::Null)
}

fn truthy_str<'a>(input: &'a Value, key: &str) -> Option<&'a str> {
    str_arg(input, key).filter(|s| !s.is_empty())
}

fn args_with_browser_id(input: &Value, rest: &[(&str, Value)]) -> Map<String, Value> {
    let mut map = Map::new();
    map.insert("browserId".into(), copy(input, "browserId"));
    for (key, value) in rest {
        map.insert((*key).into(), value.clone());
    }
    map
}

fn specs() -> Vec<Spec> {
    fn tab(name: &'static str, command: &'static str, args: Args) -> Spec {
        Spec {
            name,
            command,
            args,
            needs_workspace: false,
            widens_timeout: false,
            tab_scoped: true,
        }
    }
    vec![
        Spec {
            name: "browser_list_tabs",
            command: "list_tabs",
            args: |_| Map::new(),
            needs_workspace: true,
            widens_timeout: false,
            tab_scoped: false,
        },
        Spec {
            name: "browser_new_tab",
            command: "new_tab",
            args: |input| {
                let mut map = Map::new();
                if let Some(url) = truthy_str(input, "url") {
                    map.insert("url".into(), json!(url));
                }
                map
            },
            needs_workspace: true,
            widens_timeout: false,
            tab_scoped: false,
        },
        tab("browser_snapshot", "snapshot", |i| {
            args_with_browser_id(i, &[])
        }),
        tab("browser_click", "click", |i| {
            args_with_browser_id(
                i,
                &[
                    ("ref", copy(i, "ref")),
                    (
                        "button",
                        truthy_str(i, "button").map_or(json!("left"), |b| json!(b)),
                    ),
                    (
                        "doubleClick",
                        i.get("doubleClick").cloned().unwrap_or(json!(false)),
                    ),
                    (
                        "modifiers",
                        i.get("modifiers").cloned().unwrap_or(json!([])),
                    ),
                ],
            )
        }),
        tab("browser_fill", "fill", |i| {
            args_with_browser_id(i, &[("ref", copy(i, "ref")), ("value", copy(i, "value"))])
        }),
        Spec {
            name: "browser_wait",
            command: "wait",
            args: |i| {
                let mut map = args_with_browser_id(i, &[]);
                if let Some(text) = truthy_str(i, "text") {
                    map.insert("text".into(), json!(text));
                }
                if let Some(url) = truthy_str(i, "url") {
                    map.insert("url".into(), json!(url));
                }
                if let Some(timeout) = i.get("timeoutMs").filter(|t| t.as_f64() != Some(0.0)) {
                    map.insert("timeoutMs".into(), timeout.clone());
                }
                map
            },
            needs_workspace: false,
            widens_timeout: true,
            tab_scoped: true,
        },
        tab("browser_type", "type", |i| {
            let mut map = args_with_browser_id(i, &[]);
            if let Some(r) = truthy_str(i, "ref") {
                map.insert("ref".into(), json!(r));
            }
            map.insert("text".into(), copy(i, "text"));
            map
        }),
        tab("browser_keypress", "keypress", |i| {
            let mut map = args_with_browser_id(i, &[]);
            if let Some(r) = truthy_str(i, "ref") {
                map.insert("ref".into(), json!(r));
            }
            map.insert("key".into(), copy(i, "key"));
            map
        }),
        tab("browser_navigate", "navigate", |i| {
            args_with_browser_id(i, &[("url", copy(i, "url"))])
        }),
        tab("browser_back", "back", |i| args_with_browser_id(i, &[])),
        tab("browser_forward", "forward", |i| {
            args_with_browser_id(i, &[])
        }),
        tab("browser_reload", "reload", |i| args_with_browser_id(i, &[])),
        tab("browser_screenshot", "screenshot", |i| {
            args_with_browser_id(
                i,
                &[(
                    "fullPage",
                    i.get("fullPage").cloned().unwrap_or(json!(false)),
                )],
            )
        }),
        tab("browser_upload", "upload", |i| {
            args_with_browser_id(
                i,
                &[("ref", copy(i, "ref")), ("filePaths", copy(i, "filePaths"))],
            )
        }),
        tab("browser_hover", "hover", |i| {
            args_with_browser_id(i, &[("ref", copy(i, "ref"))])
        }),
        tab("browser_select", "select", |i| {
            args_with_browser_id(i, &[("ref", copy(i, "ref")), ("value", copy(i, "value"))])
        }),
        tab("browser_drag", "drag", |i| {
            args_with_browser_id(
                i,
                &[
                    ("sourceRef", copy(i, "sourceRef")),
                    ("targetRef", copy(i, "targetRef")),
                ],
            )
        }),
        tab("browser_logs", "logs", |i| {
            args_with_browser_id(
                i,
                &[(
                    "maxEntries",
                    i.get("maxEntries")
                        .filter(|m| m.as_f64() != Some(0.0))
                        .cloned()
                        .unwrap_or(json!(50)),
                )],
            )
        }),
        tab("browser_evaluate", "evaluate", |i| {
            let mut map = args_with_browser_id(i, &[("function", copy(i, "function"))]);
            if let Some(r) = truthy_str(i, "ref") {
                map.insert("ref".into(), json!(r));
            }
            map
        }),
        tab("browser_scroll", "scroll", |i| {
            let mut map = args_with_browser_id(i, &[]);
            if let Some(r) = truthy_str(i, "ref") {
                map.insert("ref".into(), json!(r));
            }
            map.insert("deltaX".into(), copy(i, "deltaX"));
            map.insert("deltaY".into(), copy(i, "deltaY"));
            map
        }),
        tab("browser_resize", "resize", |i| {
            args_with_browser_id(
                i,
                &[("width", copy(i, "width")), ("height", copy(i, "height"))],
            )
        }),
        tab("browser_close_tab", "close_tab", |i| {
            args_with_browser_id(i, &[])
        }),
    ]
}

pub(crate) fn register(r: &mut Registrar) {
    for spec in specs() {
        r.register(
            spec.name,
            handler(move |env, input, _cancel| run(env, spec, input)),
        );
    }
}

/// `resolveBrowserToolContext`: agent id, cwd and workspace of the caller.
fn browser_context(env: &ToolEnv) -> Result<Map<String, Value>, crate::error::ToolError> {
    let caller = env.resolve_caller_agent()?;
    let mut map = Map::new();
    if let Some(id) = &env.caller_agent_id {
        map.insert("agentId".into(), json!(id));
    }
    if let Some(agent) = &caller {
        if !agent.cwd.is_empty() {
            map.insert("cwd".into(), json!(agent.cwd));
        }
        if let Some(workspace_id) = agent.workspace_id.as_ref().filter(|w| !w.is_empty()) {
            map.insert("workspaceId".into(), json!(workspace_id));
        }
    }
    Ok(map)
}

async fn run(env: Arc<ToolEnv>, spec: Spec, input: Value) -> ToolOutcome {
    let context = browser_context(&env)?;
    if spec.needs_workspace && !context.contains_key("workspaceId") {
        // `requireWorkspaceContext`: a denial, returned as a normal result.
        return Ok(browser_tool_result(
            &json!({
                "requestId": "browser-tools-workspace-context",
                "ok": false,
                "error": {
                    "code": "browser_denied",
                    "message": WORKSPACE_CONTEXT_MESSAGE,
                    "retryable": false,
                },
            }),
            context,
        ));
    }
    let args = (spec.args)(&input);
    let mut request = Map::new();
    for key in ["agentId", "cwd", "workspaceId"] {
        if let Some(value) = context.get(key) {
            request.insert(key.into(), value.clone());
        }
    }
    if spec.widens_timeout {
        if let Some(timeout) = args.get("timeoutMs").and_then(Value::as_f64) {
            request.insert("timeoutMs".into(), json!(timeout as i64 + 1000));
        }
    }
    request.insert(
        "command".into(),
        json!({"command": spec.command, "args": Value::Object(args)}),
    );
    let payload = env.core.browser_execute(Value::Object(request)).await?;
    let mut result_context = context;
    if spec.tab_scoped {
        result_context.insert("browserId".into(), copy(&input, "browserId"));
    }
    Ok(browser_tool_result(&payload, result_context))
}

/// JS template-literal rendering of a JSON value (`${value}`).
fn js_text(value: Option<&Value>) -> String {
    match value {
        None => "undefined".to_owned(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) => "null".to_owned(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Number(n)) => match n.as_f64() {
            Some(f) if f.fract() == 0.0 && f.abs() < 1e21 => format!("{}", f as i64),
            _ => n.to_string(),
        },
        Some(other) => other.to_string(),
    }
}

fn plural(count: usize, one: &'static str, many: &'static str) -> &'static str {
    if count == 1 {
        one
    } else {
        many
    }
}

fn browser_tool_result(payload: &Value, context: Map<String, Value>) -> ToolResult {
    let dialogs = payload.get("dialogs").filter(|d| !d.is_null());
    if payload.get("ok").and_then(Value::as_bool) == Some(true) {
        let result = payload.get("result").cloned().unwrap_or(Value::Null);
        let mut structured = Map::new();
        structured.insert("ok".into(), json!(true));
        structured.insert("result".into(), structured_result(&result));
        if let Some(dialogs) = dialogs {
            structured.insert("dialogs".into(), dialogs.clone());
        }
        structured.insert("context".into(), Value::Object(context));
        return ToolResult {
            content: success_content(payload, &result),
            structured_content: Some(Value::Object(structured)),
            is_error: None,
        };
    }
    let error = payload.get("error").cloned().unwrap_or(Value::Null);
    let mut structured = Map::new();
    structured.insert("ok".into(), json!(false));
    structured.insert("error".into(), error.clone());
    if let Some(dialogs) = dialogs {
        structured.insert("dialogs".into(), dialogs.clone());
    }
    structured.insert("context".into(), Value::Object(context));
    ToolResult {
        content: vec![json!({
            "type": "text",
            "text": append_dialog_summary(summarize_error(&error), dialogs),
        })],
        structured_content: Some(Value::Object(structured)),
        is_error: None,
    }
}

/// Screenshots keep their metadata in the structured content and the pixels in an image block.
fn structured_result(result: &Value) -> Value {
    if result.get("command").and_then(Value::as_str) != Some("screenshot") {
        return result.clone();
    }
    match result {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(k, _)| k.as_str() != "dataBase64")
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn success_content(payload: &Value, result: &Value) -> Vec<Value> {
    let text = json!({"type": "text", "text": summarize_success(payload, result)});
    if result.get("command").and_then(Value::as_str) == Some("screenshot") {
        let image = json!({
            "type": "image",
            "data": result.get("dataBase64").cloned().unwrap_or(Value::Null),
            "mimeType": result.get("mimeType").cloned().unwrap_or(Value::Null),
        });
        return vec![text, image];
    }
    vec![text]
}

fn append_dialog_summary(summary: String, dialogs: Option<&Value>) -> String {
    let Some(Value::Array(dialogs)) = dialogs else {
        return summary;
    };
    if dialogs.is_empty() {
        return summary;
    }
    let described: Vec<String> = dialogs
        .iter()
        .map(|d| {
            format!(
                "{} {} {}",
                js_text(d.get("action")),
                js_text(d.get("type")),
                serde_json::to_string(d.get("message").unwrap_or(&Value::Null)).unwrap_or_default()
            )
        })
        .collect();
    format!(
        "{summary}\nHandled browser dialog{}: {}.",
        plural(dialogs.len(), "", "s"),
        described.join("; ")
    )
}

fn summarize_success(payload: &Value, result: &Value) -> String {
    let dialogs = payload.get("dialogs");
    let with_dialogs = |summary: String| append_dialog_summary(summary, dialogs);
    let field = |key: &str| result.get(key);
    let command = result.get("command").and_then(Value::as_str).unwrap_or("");
    let summary = match command {
        "select" => format!(
            "Selected {} in browser element {}.",
            js_text(field("value")),
            js_text(field("ref"))
        ),
        "hover" => format!("Hovered browser element {}.", js_text(field("ref"))),
        "drag" => format!(
            "Dragged browser element {} to {}.",
            js_text(field("sourceRef")),
            js_text(field("targetRef"))
        ),
        "scroll" => match truthy_str(result, "ref") {
            Some(r) => format!(
                "Scrolled browser element {r} by {}, {}.",
                js_text(field("deltaX")),
                js_text(field("deltaY"))
            ),
            None => format!(
                "Scrolled browser by {}, {}.",
                js_text(field("deltaX")),
                js_text(field("deltaY"))
            ),
        },
        "resize" => format!(
            "Resized browser viewport to {}x{}.",
            js_text(field("width")),
            js_text(field("height"))
        ),
        "close_tab" => format!("Closed browser tab {}.", js_text(field("browserId"))),
        "click" => format!("Clicked browser element {}.", js_text(field("ref"))),
        "fill" => format!("Filled browser element {}.", js_text(field("ref"))),
        "evaluate" => {
            let mut lines = vec![
                "Browser evaluate returned:".to_owned(),
                js_text(field("resultJson")),
            ];
            if field("truncated").and_then(Value::as_bool) == Some(true) {
                lines.push("Result was truncated.".to_owned());
            }
            lines.join("\n")
        }
        "logs" => {
            let console = field("console").and_then(Value::as_array).map_or(0, Vec::len);
            let network = field("network").and_then(Value::as_array).map_or(0, Vec::len);
            format!(
                "Read {console} console log{} and {network} network entr{}.",
                plural(console, "", "s"),
                plural(network, "y", "ies")
            )
        }
        "type" => match truthy_str(result, "ref") {
            Some(r) => format!("Typed into browser element {r}."),
            None => "Typed into the focused browser element.".to_owned(),
        },
        "keypress" => match truthy_str(result, "ref") {
            Some(r) => format!("Pressed {} on browser element {r}.", js_text(field("key"))),
            None => format!("Pressed {} in the browser.", js_text(field("key"))),
        },
        "navigate" => format!("Navigated browser to {}.", js_text(field("url"))),
        "back" | "forward" | "reload" => format!("Browser {command} complete."),
        "screenshot" => format!(
            "Captured browser screenshot ({}x{}).",
            js_text(field("width")),
            js_text(field("height"))
        ),
        "upload" => {
            let count = field("filePaths").and_then(Value::as_array).map_or(0, Vec::len);
            format!(
                "Uploaded {count} file{} to browser element {}.",
                plural(count, "", "s"),
                js_text(field("ref"))
            )
        }
        "list_tabs" => {
            let tabs = field("tabs").and_then(Value::as_array).cloned().unwrap_or_default();
            if tabs.is_empty() {
                // PARITY-QUIRK(browser-empty-tabs-no-dialogs): the empty case returns before the
                // dialog summary is appended.
                return "No Paseo browser tabs are open. Call browser_new_tab to create one."
                    .to_owned();
            }
            let mut lines = vec![format!(
                "Found {} Paseo browser tab{}. Use these browserId values for tab-scoped browser tools.",
                tabs.len(),
                plural(tabs.len(), "", "s")
            )];
            for tab in &tabs {
                let active = if tab.get("isActive").and_then(Value::as_bool) == Some(true) {
                    " active"
                } else {
                    ""
                };
                let title = truthy_str(tab, "title").unwrap_or("Untitled");
                lines.push(format!(
                    "- browserId={}{active} title={} url={}",
                    js_text(tab.get("browserId")),
                    serde_json::to_string(title).unwrap_or_default(),
                    js_text(tab.get("url"))
                ));
            }
            lines.join("\n")
        }
        "new_tab" => format!(
            "Created browser tab browserId={} url={}. Use this browserId for tab-scoped browser tools.",
            js_text(field("browserId")),
            js_text(field("url"))
        ),
        "snapshot" => {
            let stats = field("stats");
            let node_count = stats.and_then(|s| s.get("nodeCount")).and_then(Value::as_u64).unwrap_or(0);
            let ref_count = stats.and_then(|s| s.get("refCount")).and_then(Value::as_u64).unwrap_or(0);
            [
                format!(
                    "Snapshot captured {node_count} node{} with {ref_count} ref{}.",
                    plural(node_count as usize, "", "s"),
                    plural(ref_count as usize, "", "s")
                ),
                format!("Title: {}", truthy_str(result, "title").unwrap_or("Untitled")),
                format!("URL: {}", js_text(field("url"))),
                String::new(),
                js_text(field("snapshot")),
            ]
            .join("\n")
        }
        "wait" => format!("Browser wait matched {}.", js_text(field("matched"))),
        other => format!("Browser {other} complete."),
    };
    with_dialogs(summary)
}

/// `summarizeBrowserError`.
fn summarize_error(error: &Value) -> String {
    let message = || js_text(error.get("message"));
    match error.get("code").and_then(Value::as_str).unwrap_or("") {
        "browser_disabled" => {
            "Browser tools are disabled. Enable browser tools on the host, then try again."
                .to_owned()
        }
        "browser_timeout" => {
            "The browser did not respond before the timeout. Try again or check the browser host."
                .to_owned()
        }
        "browser_stale_ref" => {
            "That browser element reference is stale. Take a new browser snapshot and try again."
                .to_owned()
        }
        _ => message(),
    }
}
