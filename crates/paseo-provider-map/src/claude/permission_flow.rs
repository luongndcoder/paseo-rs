//! Permission requests: `handlePermissionRequest`, `respondToPermission` and the deny/cancel
//! paths of `ClaudeAgentSession` (`claude/agent.ts`).

use serde_json::{json, Map, Value};

use paseo_tool_detail::detail::claude::{map_claude_running_tool_call, ClaudeToolCallParams};

use super::blocks::{completed_tool_call, failed_tool_call};
use super::permissions::{
    normalize_question_request_input, permission_kind, plan_permission_actions,
    question_permission_summary, resolve_question_answers,
};
use super::text::str_of;
use super::{ClaudeMapper, PendingPermission, PermissionAnswer};
use crate::MapEnv;

impl ClaudeMapper {
    /// `handlePermissionRequest` (the `canUseTool` callback). The request stays pending until
    /// answered, canceled or the session closes.
    pub(super) fn handle_permission_request(
        &mut self,
        tool_name: &str,
        input: &Value,
        tool_use_id: Option<String>,
        env: &mut dyn MapEnv,
    ) {
        let request_id = format!("permission-{}", env.uuid());
        let kind = permission_kind(tool_name, input);
        let request_input = normalize_question_request_input(tool_name, input);
        let mut metadata = Map::new();
        if let Some(id) = &tool_use_id {
            metadata.insert("toolUseId".into(), json!(id));
        }
        if tool_name == "ExitPlanMode" {
            if let Some(plan) = input.get("plan").and_then(Value::as_str) {
                metadata.insert("planText".into(), json!(plan));
            }
        }
        let detail = (kind == "tool")
            .then(|| {
                map_claude_running_tool_call(&ClaudeToolCallParams {
                    call_id: Some(tool_use_id.as_deref().unwrap_or(&request_id)),
                    name: tool_name,
                    input: Some(input),
                    output: None,
                    metadata: None,
                })
            })
            .flatten()
            .map(|i| i.detail.to_value());

        let mut request = Map::new();
        request.insert("id".into(), json!(request_id));
        request.insert("provider".into(), json!("claude"));
        request.insert("name".into(), json!(tool_name));
        request.insert("kind".into(), json!(kind));
        let (title, description) = question_permission_summary(tool_name, input);
        if let Some(t) = title {
            request.insert("title".into(), json!(t));
        }
        if let Some(d) = description {
            request.insert("description".into(), json!(d));
        }
        request.insert("input".into(), request_input);
        if let Some(d) = detail {
            request.insert("detail".into(), d);
        }
        if kind == "plan" {
            request.insert(
                "actions".into(),
                plan_permission_actions(self.plan_resume_mode.as_deref()),
            );
        }
        if !metadata.is_empty() {
            request.insert("metadata".into(), Value::Object(metadata));
        }
        let request = Value::Object(request);
        self.notify(
            json!({"type": "permission_requested", "provider": "claude", "request": request}),
        );
        self.pending_permissions.push(PendingPermission { request });
    }

    /// The golden driver answered `getPendingPermissions()[0]`.
    pub(super) fn respond_to_first_permission(
        &mut self,
        answer: PermissionAnswer,
        _env: &mut dyn MapEnv,
    ) {
        let Some(first) = self.pending_permissions.first() else {
            return;
        };
        let request = first.request.clone();
        self.respond_to_permission(&request, answer);
    }

    /// `respondToPermission`.
    fn respond_to_permission(&mut self, request: &Value, answer: PermissionAnswer) {
        let id = str_of(request.get("id")).unwrap_or("").to_owned();
        let kind = str_of(request.get("kind")).unwrap_or("tool");
        // assertClaudeQuestionAnswerDeliverable: an allow without answers throws and leaves the
        // request pending.
        if kind == "question" && matches!(answer, PermissionAnswer::Allow) {
            let answers = resolve_question_answers(None, request.get("input"));
            if answers.is_none_or(|a| a.is_empty()) {
                return;
            }
        }
        self.pending_permissions
            .retain(|p| p.request.get("id").and_then(Value::as_str) != Some(id.as_str()));
        match answer {
            PermissionAnswer::Allow => {
                if kind == "plan" {
                    // `setMode("acceptEdits")`: the mode change surfaces through Claude's status
                    // message, not an event here.
                    self.current_mode = "acceptEdits".to_owned();
                    self.plan_resume_mode = Some("acceptEdits".to_owned());
                    let call_id = plan_tool_call_id(request);
                    let output = json!({"approved": true, "actionId": "implement"});
                    if let Some(item) = completed_tool_call(
                        "ExitPlanMode",
                        Some(&call_id),
                        request.get("input"),
                        Some(&output),
                    ) {
                        self.notify_timeline(item);
                    }
                }
                self.notify(json!({
                    "type": "permission_resolved",
                    "provider": "claude",
                    "requestId": id,
                    "resolution": {"behavior": "allow"},
                }));
            }
            PermissionAnswer::Deny { message } => self.resolve_denied_permission(request, &message),
        }
    }

    /// `resolveDeniedPermission`: the denied timeline row first, then `permission_resolved`.
    fn resolve_denied_permission(&mut self, request: &Value, message: &str) {
        self.record_denied_permission_timeline(request, message);
        self.notify(json!({
            "type": "permission_resolved",
            "provider": "claude",
            "requestId": request.get("id").cloned().unwrap_or(Value::Null),
            "resolution": {"behavior": "deny", "message": message},
        }));
    }

    /// `recordDeniedPermissionTimeline`.
    fn record_denied_permission_timeline(&mut self, request: &Value, message: &str) {
        let name = str_of(request.get("name")).unwrap_or("");
        let item = match str_of(request.get("kind")) {
            Some("tool") => {
                let call_id = request
                    .get("metadata")
                    .and_then(|m| m.get("toolUseId"))
                    .and_then(Value::as_str)
                    .or_else(|| str_of(request.get("id")));
                failed_tool_call(
                    name,
                    call_id,
                    request.get("input"),
                    None,
                    None,
                    &json!({"message": message}),
                )
            }
            Some("plan") => {
                let call_id = plan_tool_call_id(request);
                let mut meta = Map::new();
                meta.insert("actionId".into(), json!("reject"));
                failed_tool_call(
                    "ExitPlanMode",
                    Some(&call_id),
                    request.get("input"),
                    None,
                    Some(&meta),
                    &json!(message),
                )
            }
            _ => None,
        };
        if let Some(item) = item {
            self.notify_timeline(item);
        }
    }

    /// `rejectAllPendingPermissions`.
    pub(super) fn reject_all_pending_permissions(&mut self, message: &str) {
        for pending in std::mem::take(&mut self.pending_permissions) {
            self.notify(json!({
                "type": "permission_resolved",
                "provider": "claude",
                "requestId": pending.request.get("id").cloned().unwrap_or(Value::Null),
                "resolution": {"behavior": "deny", "message": message},
            }));
        }
    }
}

/// `planToolCallId`.
fn plan_tool_call_id(request: &Value) -> String {
    request
        .get("metadata")
        .and_then(|m| m.get("toolUseId"))
        .and_then(Value::as_str)
        .or_else(|| str_of(request.get("id")))
        .unwrap_or("")
        .to_owned()
}
