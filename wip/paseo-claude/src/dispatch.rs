//! stdout line loop and the handlers for CLI-initiated control requests.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::callbacks::PermissionDecision;
use crate::error::{Error, Result};
use crate::message::SdkMessage;
use crate::protocol::mcp::{is_jsonrpc_request, mcp_notification_ack, mcp_response_payload};
use crate::protocol::responses::SlashCommand;
use crate::protocol::{
    ControlCancelRequest, ControlRequest, ControlRequestBody, ControlResponse, ControlResponseBody,
    HookJsonOutput, Line, PermissionResult,
};
use crate::session::{lock, Shared};
use crate::transport::{ExitInfo, TransportEvent};

enum Parsed {
    Request(ControlRequest),
    Response(ControlResponse),
    Cancel(ControlCancelRequest),
    Message(Line, Option<Vec<SlashCommand>>),
    Ignore,
}

/// Control envelopes are deserialized directly (they are never re-serialized, so the
/// lossless fallback of [`Line::parse`] would only hide them inside `Line::Unknown`).
fn classify(text: &str) -> Parsed {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return Parsed::Ignore;
    };
    match value.get("type").and_then(Value::as_str) {
        Some("control_request") => serde_json::from_value(value)
            .map(Parsed::Request)
            .unwrap_or(Parsed::Ignore),
        Some("control_response") => serde_json::from_value(value)
            .map(Parsed::Response)
            .unwrap_or(Parsed::Ignore),
        Some("control_cancel_request") => serde_json::from_value(value)
            .map(Parsed::Cancel)
            .unwrap_or(Parsed::Ignore),
        _ => {
            // sdk.mjs keeps the latest `commands_changed` list for supportedCommands().
            let commands = (value.get("type").and_then(Value::as_str) == Some("system")
                && value.get("subtype").and_then(Value::as_str) == Some("commands_changed"))
            .then(|| value.get("commands").cloned())
            .flatten()
            .and_then(|c| serde_json::from_value::<Vec<SlashCommand>>(c).ok());
            Parsed::Message(Line::from_value(value), commands)
        }
    }
}

pub(crate) async fn run_reader(
    shared: Arc<Shared>,
    mut events: mpsc::UnboundedReceiver<TransportEvent>,
    msg_tx: mpsc::UnboundedSender<Result<SdkMessage>>,
) {
    let mut last_error_result: Option<String> = None;
    let mut exit = ExitInfo {
        code: Some(0),
        ..ExitInfo::default()
    };
    while let Some(event) = events.recv().await {
        let text = match event {
            TransportEvent::Line(t) => t,
            TransportEvent::Exit(info) => {
                exit = info;
                break;
            }
        };
        if text.trim().is_empty() {
            continue;
        }
        match classify(&text) {
            Parsed::Ignore => {}
            Parsed::Request(r) => handle_cli_request(&shared, r.request_id, r.request),
            Parsed::Cancel(c) => {
                if let Some(token) = lock(&shared.inflight).remove(&c.request_id) {
                    token.cancel();
                }
            }
            Parsed::Response(r) => handle_response(&shared, r),
            Parsed::Message(line, commands) => {
                if let Some(c) = commands {
                    *lock(&shared.latest_commands) = Some(c);
                }
                if let Line::Result(res) = &line {
                    shared.on_result();
                    let failed = res.is_error == Some(true) || res.subtype != "success";
                    last_error_result = failed.then(|| result_text(res));
                }
                if let Some(msg) = SdkMessage::from_line(line) {
                    // The consumer may have dropped the stream; keep serving control traffic.
                    let _ = msg_tx.send(Ok(msg));
                }
            }
        }
    }
    shared.on_exit();
    if !shared.closing.load(Ordering::SeqCst) && !exit.success() {
        let err = match last_error_result {
            Some(text) => Error::ErrorResult(text),
            None => Error::ProcessExited {
                status: exit.describe(),
                stderr: exit.stderr_tail,
            },
        };
        let _ = msg_tx.send(Err(err));
    }
}

fn result_text(res: &crate::protocol::ResultMessage) -> String {
    match (&res.result, &res.errors) {
        (Some(r), _) if !r.is_empty() => r.clone(),
        (_, Some(errors)) if !errors.is_empty() => errors.join("; "),
        _ => res.subtype.clone(),
    }
}

fn handle_response(shared: &Arc<Shared>, resp: ControlResponse) {
    match resp.response {
        ControlResponseBody::Success(s) => {
            let id = s.request_id.clone();
            let is_init = lock(&shared.init_request_id).as_deref() == Some(id.as_str());
            if is_init {
                // Requests the CLI still awaits are redelivered here.
                let redelivered = s
                    .pending_permission_requests
                    .iter()
                    .chain(s.pending_user_dialog_requests.iter())
                    .flatten()
                    .cloned()
                    .collect::<Vec<_>>();
                for v in redelivered {
                    if let Ok(req) = serde_json::from_value::<ControlRequest>(v) {
                        handle_cli_request(shared, req.request_id, req.request);
                    }
                }
            }
            shared.resolve(&id, Ok(s));
        }
        ControlResponseBody::Error(e) => {
            let subtype = shared.subtype_of(&e.request_id);
            shared.resolve(
                &e.request_id,
                Err(Error::Control {
                    subtype,
                    message: e.error,
                }),
            );
        }
        ControlResponseBody::Unknown(v) => {
            if let Some(id) = v.get("request_id").and_then(Value::as_str) {
                shared.resolve(
                    id,
                    Err(Error::Protocol(format!(
                        "unrecognized control response: {v}"
                    ))),
                );
            }
        }
    }
}

fn handle_cli_request(shared: &Arc<Shared>, request_id: String, body: ControlRequestBody) {
    let token = shared.shutdown.child_token();
    {
        let mut inflight = lock(&shared.inflight);
        if inflight.contains_key(&request_id) {
            return; // repeated delivery of an in-flight request
        }
        inflight.insert(request_id.clone(), token.clone());
    }
    let shared = shared.clone();
    tokio::spawn(async move {
        // The user callback runs in its own task so a panic becomes an error response
        // instead of leaving the CLI waiting forever.
        let mut work = tokio::spawn(answer(shared.clone(), body, token.clone()));
        let outcome = tokio::select! {
            r = &mut work => Some(match r {
                Ok(v) => v,
                Err(e) => Err(format!("handler failed: {e}")),
            }),
            () = token.cancelled() => {
                work.abort();
                None
            }
        };
        let still_wanted = lock(&shared.inflight).remove(&request_id).is_some();
        let Some(outcome) = outcome else { return };
        if !still_wanted {
            return;
        }
        let line = match outcome {
            Ok(payload) => Line::control_success(request_id, payload),
            Err(message) => Line::control_error(request_id, message),
        };
        // Writing fails only when the session is closing.
        let _ = shared.write_line(&line);
    });
}

/// `Ok(payload)` becomes a success response, `Err(message)` an error response.
async fn answer(
    shared: Arc<Shared>,
    body: ControlRequestBody,
    cancel: CancellationToken,
) -> std::result::Result<Option<Value>, String> {
    let callbacks = &shared.callbacks;
    match body {
        ControlRequestBody::CanUseTool(req) => {
            let cb = callbacks
                .can_use_tool
                .clone()
                .ok_or("canUseTool callback is not provided.")?;
            let tool_use_id = req.tool_use_id.clone();
            let input = req.input.clone();
            let decision = cb
                .can_use_tool(req, cancel)
                .await
                .map_err(|e| e.to_string())?;
            let result = match decision {
                PermissionDecision::Allow {
                    updated_input,
                    updated_permissions,
                } => {
                    let mut r =
                        PermissionResult::allow(updated_input.unwrap_or(input), tool_use_id);
                    if let PermissionResult::Allow {
                        updated_permissions: slot,
                        ..
                    } = &mut r
                    {
                        *slot = updated_permissions;
                    }
                    r
                }
                PermissionDecision::Deny { message, interrupt } => {
                    PermissionResult::deny(message, interrupt, tool_use_id)
                }
            };
            serde_json::to_value(result)
                .map(Some)
                .map_err(|e| e.to_string())
        }
        ControlRequestBody::HookCallback {
            callback_id,
            input,
            tool_use_id,
            ..
        } => {
            let cb = callbacks
                .hooks
                .get(&callback_id)
                .cloned()
                .ok_or_else(|| format!("No hook callback found for ID: {callback_id}"))?;
            let out: HookJsonOutput = cb
                .call(input, tool_use_id, cancel)
                .await
                .map_err(|e| e.to_string())?;
            serde_json::to_value(out)
                .map(Some)
                .map_err(|e| e.to_string())
        }
        ControlRequestBody::McpMessage {
            server_name,
            message,
            ..
        } => {
            let server = callbacks
                .mcp_servers
                .get(&server_name)
                .ok_or_else(|| format!("SDK MCP server not found: {server_name}"))?;
            if !is_jsonrpc_request(&message) {
                return Ok(Some(mcp_notification_ack()));
            }
            let resp = crate::mcp_bridge::handle_request(server, &message, cancel).await;
            Ok(Some(mcp_response_payload(resp)))
        }
        ControlRequestBody::Elicitation(req) => match callbacks.elicitation.clone() {
            Some(h) => h
                .elicit(req, cancel)
                .await
                .map(Some)
                .map_err(|e| e.to_string()),
            None => Ok(Some(json!({"action": "decline"}))),
        },
        ControlRequestBody::RequestUserDialog { dialog_kind, .. } => {
            Err(format!("No handler for user dialog: {dialog_kind}"))
        }
        ControlRequestBody::OauthTokenRefresh { .. } => {
            Err("No OAuth token refresh callback provided".into())
        }
        ControlRequestBody::HostAuthTokenRefresh { .. } => {
            Err("No host auth token refresh callback provided".into())
        }
        other => Err(format!(
            "Unsupported control request subtype: {}",
            other.subtype().unwrap_or("unknown")
        )),
    }
}
