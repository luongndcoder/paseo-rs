//! `extension_ui_request` dialogs <-> permission requests (`mapExtensionUiRequestToPermission`,
//! `buildExtensionUiResponse`).

use serde_json::{json, Value};

use super::{PermissionKind, PermissionRequest, PermissionResponse};
use crate::flavor::Flavor;
use crate::protocol::{UiRequest, UiResponse};

const QUESTION_RESPONSE_HEADER: &str = "Response";

/// Dialog -> permission request. `None` for fire-and-forget methods (`notify`, `setWidget`, ...).
pub fn ui_request_to_permission(flavor: Flavor, request: &UiRequest) -> Option<PermissionRequest> {
    let options = request.options.clone().unwrap_or_default();
    let (question, options, placeholder, allow_empty) = match request.method.as_str() {
        "select" => (
            request
                .title
                .clone()
                .unwrap_or_else(|| "Select an option".into()),
            options,
            None,
            false,
        ),
        "input" => {
            let placeholder = request.placeholder.clone();
            let allow_empty = placeholder
                .as_deref()
                .is_some_and(|p| p.trim().to_ascii_lowercase().starts_with("optional"));
            (
                input_question_title(request.title.as_deref(), placeholder.as_deref()),
                Vec::new(),
                placeholder,
                allow_empty,
            )
        }
        "editor" => (
            request.title.clone().unwrap_or_else(|| "Edit text".into()),
            Vec::new(),
            None,
            false,
        ),
        "confirm" => (
            [request.title.as_deref(), request.message.as_deref()]
                .into_iter()
                .flatten()
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join("\n\n"),
            vec!["Yes".to_string(), "No".to_string()],
            None,
            false,
        ),
        _ => return None,
    };
    let mut q = json!({
        "question": question,
        "header": QUESTION_RESPONSE_HEADER,
        "options": options.iter().map(|label| json!({"label": label})).collect::<Vec<_>>(),
        "multiSelect": false,
    });
    if let Some(placeholder) = placeholder {
        q["placeholder"] = placeholder.into();
    }
    if allow_empty {
        q["allowEmpty"] = true.into();
        q["dismissLabel"] = "Skip".into();
    }
    Some(PermissionRequest {
        id: request.id.clone(),
        provider: flavor.provider_id().to_string(),
        name: format!("{} {}", flavor.label(), request.method),
        kind: PermissionKind::Question,
        title: question,
        input: json!({ "questions": [q] }),
        metadata: json!({
            "extensionUiMethod": request.method,
            "answerHeader": QUESTION_RESPONSE_HEADER,
        }),
    })
}

fn input_question_title(title: Option<&str>, placeholder: Option<&str>) -> String {
    match (
        title.filter(|t| !t.is_empty()),
        placeholder.filter(|p| !p.is_empty()),
    ) {
        (Some(title), _) => title.to_string(),
        (None, Some(placeholder)) => placeholder.to_string(),
        (None, None) => "Optional response".to_string(),
    }
}

/// Permission response -> `extension_ui_response` body. Deny, or an allow without an answer,
/// cancels the dialog.
pub fn build_ui_response(request: &PermissionRequest, response: &PermissionResponse) -> UiResponse {
    let PermissionResponse::Allow { updated_input } = response else {
        return UiResponse::cancelled();
    };
    let Some(answer) = first_answer(updated_input.as_ref()) else {
        return UiResponse::cancelled();
    };
    let method = request
        .metadata
        .get("extensionUiMethod")
        .and_then(Value::as_str);
    if method == Some("confirm") {
        return UiResponse::confirmed(answer.trim().eq_ignore_ascii_case("yes"));
    }
    UiResponse::value(answer)
}

fn first_answer(input: Option<&Value>) -> Option<String> {
    input?
        .get("answers")?
        .as_object()?
        .values()
        .find_map(|v| v.as_str().map(str::to_owned))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ui(value: Value) -> UiRequest {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn select_becomes_a_question_with_options() {
        let req = ui(
            json!({"id":"u1","method":"select","title":"[Snack] Which snack?",
            "options":["1. Apple","2. Cookie"]}),
        );
        let perm = ui_request_to_permission(Flavor::Pi, &req).unwrap();
        assert_eq!(perm.name, "Pi select");
        assert_eq!(perm.title, "[Snack] Which snack?");
        assert_eq!(
            perm.input["questions"][0]["options"][1]["label"],
            "2. Cookie"
        );
        assert_eq!(perm.metadata["extensionUiMethod"], "select");
    }

    #[test]
    fn confirm_joins_title_and_message_and_maps_yes_no() {
        let req = ui(json!({"id":"u2","method":"confirm","title":"Run it?","message":"rm -rf"}));
        let perm = ui_request_to_permission(Flavor::Omp, &req).unwrap();
        assert_eq!(perm.title, "Run it?\n\nrm -rf");
        let yes = PermissionResponse::Allow {
            updated_input: Some(json!({"answers":{"Response":"yes"}})),
        };
        assert_eq!(build_ui_response(&perm, &yes), UiResponse::confirmed(true));
        let no = PermissionResponse::Allow {
            updated_input: Some(json!({"answers":{"Response":"No"}})),
        };
        assert_eq!(build_ui_response(&perm, &no), UiResponse::confirmed(false));
    }

    #[test]
    fn optional_input_allows_skip_and_deny_cancels() {
        let req = ui(json!({"id":"u3","method":"input","placeholder":"Optional: notes"}));
        let perm = ui_request_to_permission(Flavor::Pi, &req).unwrap();
        assert_eq!(perm.input["questions"][0]["allowEmpty"], true);
        assert_eq!(perm.title, "Optional: notes");
        assert_eq!(
            build_ui_response(&perm, &PermissionResponse::Deny { message: None }),
            UiResponse::cancelled()
        );
        assert_eq!(
            build_ui_response(
                &perm,
                &PermissionResponse::Allow {
                    updated_input: None
                }
            ),
            UiResponse::cancelled()
        );
    }

    #[test]
    fn notifications_are_not_permissions() {
        for method in ["notify", "setWidget", "setStatus", "set_editor_text"] {
            let req = ui(json!({"id":"n","method":method}));
            assert!(ui_request_to_permission(Flavor::Pi, &req).is_none());
        }
    }
}
