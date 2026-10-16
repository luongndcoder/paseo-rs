//! Permission option selection (`selectPermissionOption`, `isACPChooserRequest`).

use std::collections::HashSet;

use crate::agent::PermissionResponse;
use crate::protocol::PermissionOption;

/// A chooser (e.g. plan approval) offers several distinct allow options; auto-accept must
/// not pick one for the user.
pub fn is_chooser_request(options: &[PermissionOption]) -> bool {
    let mut seen = HashSet::new();
    for o in options.iter().filter(|o| o.kind.starts_with("allow")) {
        if !seen.insert(o.kind.as_str()) {
            return true;
        }
    }
    false
}

/// Pick the option answering `response`. With an explicit action id, the option must exist
/// and agree with the requested behavior.
pub fn select_permission_option<'a>(
    options: &'a [PermissionOption],
    response: &PermissionResponse,
) -> Option<&'a PermissionOption> {
    let (allow, selected) = match response {
        PermissionResponse::Allow { selected_action_id } => (true, selected_action_id),
        PermissionResponse::Deny {
            selected_action_id, ..
        } => (false, selected_action_id),
    };
    if let Some(id) = selected {
        let option = options.iter().find(|o| &o.option_id == id)?;
        return (option.is_allow() == allow).then_some(option);
    }
    let order: [&str; 2] = if allow {
        ["allow_once", "allow_always"]
    } else {
        ["reject_once", "reject_always"]
    };
    order
        .iter()
        .find_map(|kind| options.iter().find(|o| o.kind == *kind))
}
