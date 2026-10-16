//! Input validation schemas of every tool, written with the [`crate::zod`] DSL.
//!
//! These mirror the zod schemas in `agent/tools/paseo-tools.ts` and `browser-tools/tools.ts` and
//! decide validation and parsed values. What models see in `tools/list` comes from the golden
//! `data/tool-defs.json`, so a mismatch between the two shows up in `tests/validation_vectors.rs`
//! (inputs captured from the real zod schemas) and in `tests/golden.rs`.

use serde_json::{json, Map, Value};

use crate::util::resolve_required_provider_model;
use crate::zod::{
    array, array_min, boolean, default_to, nullable, number, obj, opt, pos_int, record, str_z,
    string, InputSchema, Obj, Z,
};

const BROWSER_ID_MESSAGE: &str =
    "browserId must be a real id returned by browser_new_tab or browser_list_tabs";
const PROVIDER_MODEL_MESSAGE: &str = "provider must be provider/model, for example codex/gpt-5.4";
const PROVIDER_OR_MODEL_MESSAGE: &str =
    "provider must be provider or provider/model, for example codex/gpt-5.4";

const AGENT_STATUSES: &[&str] = &["initializing", "idle", "running", "error", "closed"];

fn browser_id() -> Z {
    string()
        .type_message(BROWSER_ID_MESSAGE)
        .min_msg(1, BROWSER_ID_MESSAGE)
        .regex(
            "(?i)^(?:[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}|[0-9]{13,}-[0-9a-f]+)$",
            r"/^(?:[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}|\d{13,}-[0-9a-f]+)$/i",
            Some(BROWSER_ID_MESSAGE),
        )
        .z()
}

fn browser_ref() -> Z {
    string().regex("^@e[0-9]+$", r"/^@e\d+$/", None).z()
}

fn trimmed_min1() -> Z {
    string().trim().min(1).z()
}

fn str_min1() -> Z {
    string().min(1).z()
}

fn labels() -> Z {
    record(str_z())
}

fn features() -> Z {
    record(Z::Unknown)
}

/// `ProviderModelInputSchema`: `provider/model` with both parts present.
fn provider_model_input() -> Z {
    string()
        .trim()
        .refine(|v| v.contains('/'), PROVIDER_MODEL_MESSAGE)
        .refine(
            |v| resolve_required_provider_model(v).is_ok(),
            PROVIDER_MODEL_MESSAGE,
        )
        .z()
}

/// `ProviderOrProviderModelInputSchema`.
fn provider_or_provider_model_input() -> Z {
    string()
        .trim()
        .min_msg(1, "provider is required")
        .refine(
            |v| !v.contains('/') || resolve_required_provider_model(v).is_ok(),
            PROVIDER_OR_MODEL_MESSAGE,
        )
        .z()
}

fn agent_status() -> Z {
    Z::Enum(AGENT_STATUSES)
}

/// `CreateAgentSettingsInputSchema` (strict).
fn create_agent_settings() -> Z {
    obj(vec![
        ("modeId", opt(str_z())),
        ("thinkingOptionId", opt(str_z())),
        ("features", opt(features())),
    ])
    .strict()
    .z()
}

/// `UpdateAgentSettingsInputSchema` (strict).
fn update_agent_settings() -> Z {
    obj(vec![
        ("modeId", opt(str_z())),
        ("model", opt(nullable(str_z()))),
        ("thinkingOptionId", opt(nullable(str_z()))),
        ("features", opt(features())),
    ])
    .strict()
    .z()
}

/// `InspectProviderSettingsInputSchema` (strict).
fn inspect_provider_settings() -> Z {
    obj(vec![
        ("modeId", opt(str_z())),
        ("model", opt(str_z())),
        ("thinkingOptionId", opt(str_z())),
        ("features", opt(features())),
    ])
    .strict()
    .z()
}

fn relationship() -> Z {
    Z::DUnion {
        key: "kind",
        options: vec![
            obj(vec![("kind", Z::Literal("subagent"))]).strict(),
            obj(vec![("kind", Z::Literal("detached"))]).strict(),
        ],
    }
}

fn create_worktree_target() -> Z {
    Z::DUnion {
        key: "kind",
        options: vec![
            obj(vec![
                ("kind", Z::Literal("branch-off")),
                ("worktreeSlug", opt(str_min1())),
                ("branchName", opt(str_min1())),
                ("baseBranch", opt(str_min1())),
            ])
            .strict(),
            obj(vec![
                ("kind", Z::Literal("checkout-branch")),
                ("branch", str_min1()),
            ])
            .strict(),
            obj(vec![
                ("kind", Z::Literal("checkout-pr")),
                ("githubPrNumber", pos_int(None)),
            ])
            .strict(),
        ],
    }
}

fn workspace_input() -> Z {
    Z::DUnion {
        key: "kind",
        options: vec![
            obj(vec![("kind", Z::Literal("current")), ("cwd", opt(str_z()))]).strict(),
            obj(vec![
                ("kind", Z::Literal("existing")),
                ("workspaceId", str_min1()),
                ("cwd", opt(str_z())),
            ])
            .strict(),
            obj(vec![
                ("kind", Z::Literal("create")),
                (
                    "source",
                    Z::DUnion {
                        key: "kind",
                        options: vec![
                            obj(vec![
                                ("kind", Z::Literal("directory")),
                                ("path", opt(str_z())),
                            ])
                            .strict(),
                            obj(vec![
                                ("kind", Z::Literal("worktree")),
                                ("cwd", opt(str_z())),
                                ("target", create_worktree_target()),
                            ])
                            .strict(),
                        ],
                    },
                ),
            ])
            .strict(),
        ],
    }
}

fn common_create_agent_fields() -> Vec<(&'static str, Z)> {
    vec![
        (
            "title",
            string()
                .trim()
                .min_msg(1, "Title is required")
                .max_msg(60, "Title must be 60 characters or fewer")
                .z(),
        ),
        ("provider", provider_model_input()),
        ("labels", opt(labels())),
        ("settings", opt(create_agent_settings())),
        (
            "initialPrompt",
            string().trim().min_msg(1, "initialPrompt is required").z(),
        ),
    ]
}

fn notify_on_finish(default: bool) -> Z {
    default_to(boolean(), json!(default))
}

fn canonical_create_agent_fields() -> Vec<(&'static str, Z)> {
    let mut fields = common_create_agent_fields();
    fields.push(("workspaceId", opt(str_min1())));
    fields
}

/// The `create_agent` tool input: a passthrough object, so legacy placement keys reach the
/// strict re-parse in the handler.
pub fn create_agent_input(caller: bool) -> InputSchema {
    let mut fields = canonical_create_agent_fields();
    fields.push(("notifyOnFinish", notify_on_finish(caller)));
    InputSchema::schema(obj(fields).passthrough())
}

/// `agentToAgentCreateAgentArgsSchema` / `canonicalTopLevelCreateAgentArgsSchema` (strict).
pub fn create_agent_args_strict(caller: bool) -> Obj {
    let mut fields = canonical_create_agent_fields();
    fields.push(("notifyOnFinish", notify_on_finish(caller)));
    obj(fields).strict()
}

/// `legacyAgentToAgentCreateAgentArgsSchema` (strict).
pub fn legacy_agent_to_agent_args() -> Obj {
    let mut fields = common_create_agent_fields();
    fields.push(("relationship", relationship()));
    fields.push(("workspace", workspace_input()));
    fields.push(("notifyOnFinish", notify_on_finish(true)));
    obj(fields).strict()
}

/// `legacyTopLevelCreateAgentArgsSchema` (strict).
pub fn legacy_top_level_args() -> Obj {
    let mut fields = common_create_agent_fields();
    fields.push(("relationship", opt(relationship())));
    fields.push(("workspace", opt(workspace_input())));
    fields.push(("notifyOnFinish", notify_on_finish(false)));
    fields.push(("cwd", opt(str_z())));
    fields.push(("mode", opt(str_z())));
    fields.push(("thinking", opt(str_z())));
    fields.push(("features", opt(features())));
    fields.push(("worktreeName", opt(str_min1())));
    fields.push(("branchName", opt(str_min1())));
    fields.push(("baseBranch", opt(str_min1())));
    fields.push(("refName", opt(str_min1())));
    fields.push(("githubPrNumber", opt(pos_int(None))));
    obj(fields).strict()
}

fn permission_response() -> Z {
    Z::DUnion {
        key: "behavior",
        options: vec![
            obj(vec![
                ("behavior", Z::Literal("allow")),
                ("selectedActionId", opt(str_z())),
                ("updatedInput", opt(record(Z::Unknown))),
                ("updatedPermissions", opt(array(record(Z::Unknown)))),
            ]),
            obj(vec![
                ("behavior", Z::Literal("deny")),
                ("selectedActionId", opt(str_z())),
                ("message", opt(str_z())),
                ("interrupt", opt(boolean())),
            ]),
        ],
    }
}

fn exactly_one_of_text_or_url(input: &Map<String, Value>) -> bool {
    let truthy = |key: &str| {
        input
            .get(key)
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty())
    };
    usize::from(truthy("text")) + usize::from(truthy("url")) == 1
}

fn raw(fields: Vec<(&'static str, Z)>) -> InputSchema {
    InputSchema::raw(fields)
}

fn agent_id_only() -> InputSchema {
    raw(vec![("agentId", str_z())])
}

fn id_only() -> InputSchema {
    raw(vec![("id", str_z())])
}

fn tab_only() -> InputSchema {
    raw(vec![("browserId", browser_id())])
}

/// The input schema of `name`; `caller` is whether the catalog is agent-scoped.
pub fn input_schema(name: &str, caller: bool) -> Option<InputSchema> {
    let schema = match name {
        "speak" => raw(vec![(
            "text",
            string()
                .trim()
                .min_msg(1, "text is required")
                .max_msg(4000, "text must be 4000 characters or fewer")
                .z(),
        )]),
        "browser_list_tabs" => raw(vec![]),
        "browser_new_tab" => raw(vec![("url", opt(Z::HttpUrl))]),
        "browser_snapshot" | "browser_back" | "browser_forward" | "browser_reload"
        | "browser_close_tab" => tab_only(),
        "browser_click" => raw(vec![
            ("ref", browser_ref()),
            ("browserId", browser_id()),
            ("button", opt(Z::Enum(&["left", "right", "middle"]))),
            ("doubleClick", opt(boolean())),
            (
                "modifiers",
                opt(array(Z::Enum(&["Alt", "Control", "Meta", "Shift"]))),
            ),
        ]),
        "browser_fill" | "browser_select" => raw(vec![
            ("ref", browser_ref()),
            ("value", str_z()),
            ("browserId", browser_id()),
        ]),
        "browser_wait" => InputSchema::schema(
            obj(vec![
                ("text", opt(str_min1())),
                ("url", opt(str_min1())),
                ("timeoutMs", opt(pos_int(Some(30_000.0)))),
                ("browserId", browser_id()),
            ])
            .refine(
                exactly_one_of_text_or_url,
                "browser_wait requires exactly one of text or url",
            ),
        ),
        "browser_type" => raw(vec![
            ("text", str_z()),
            ("ref", opt(browser_ref())),
            ("browserId", browser_id()),
        ]),
        "browser_keypress" => raw(vec![
            ("key", str_min1()),
            ("ref", opt(browser_ref())),
            ("browserId", browser_id()),
        ]),
        "browser_navigate" => raw(vec![("url", Z::HttpUrl), ("browserId", browser_id())]),
        "browser_screenshot" => raw(vec![
            ("browserId", browser_id()),
            ("fullPage", default_to(boolean(), json!(false))),
        ]),
        "browser_upload" => raw(vec![
            ("ref", browser_ref()),
            ("filePaths", array_min(str_min1(), 1)),
            ("browserId", browser_id()),
        ]),
        "browser_hover" => raw(vec![("ref", browser_ref()), ("browserId", browser_id())]),
        "browser_drag" => raw(vec![
            ("sourceRef", browser_ref()),
            ("targetRef", browser_ref()),
            ("browserId", browser_id()),
        ]),
        "browser_logs" => raw(vec![
            ("maxEntries", opt(pos_int(Some(200.0)))),
            ("browserId", browser_id()),
        ]),
        "browser_evaluate" => raw(vec![
            ("function", str_min1()),
            ("ref", opt(browser_ref())),
            ("browserId", browser_id()),
        ]),
        "browser_scroll" => raw(vec![
            ("browserId", browser_id()),
            ("ref", opt(browser_ref())),
            ("deltaX", number()),
            ("deltaY", number()),
        ]),
        "browser_resize" => raw(vec![
            ("browserId", browser_id()),
            ("width", pos_int(None)),
            ("height", pos_int(None)),
        ]),
        "create_workspace" => raw(vec![
            ("isolation", Z::Enum(&["local", "worktree"])),
            ("path", opt(str_z())),
            ("projectId", opt(str_z())),
            ("title", opt(trimmed_min1())),
            ("background", opt(boolean())),
            (
                "mode",
                opt(Z::Enum(&["branch-off", "checkout-branch", "checkout-pr"])),
            ),
            ("worktreeSlug", opt(trimmed_min1())),
            ("branchName", opt(trimmed_min1())),
            ("baseBranch", opt(trimmed_min1())),
            ("branch", opt(trimmed_min1())),
            ("prNumber", opt(pos_int(None))),
            ("forge", opt(trimmed_min1())),
        ]),
        "list_workspaces" => raw(vec![(
            "includeBackground",
            default_to(boolean(), json!(false)),
        )]),
        "archive_workspace" => raw(vec![("workspaceId", str_min1())]),
        "create_agent" => create_agent_input(caller),
        "send_agent_prompt" => raw(vec![
            ("agentId", str_z()),
            ("prompt", str_z()),
            ("sessionMode", opt(str_z())),
            ("notifyOnFinish", notify_on_finish(caller)),
        ]),
        "get_agent_status" | "cancel_agent" | "archive_agent" | "kill_agent" => agent_id_only(),
        "list_agents" => raw(vec![
            ("includeBackground", default_to(boolean(), json!(false))),
            ("includeArchived", default_to(boolean(), json!(false))),
            ("cwd", opt(str_z())),
            ("sinceHours", default_to(pos_int(Some(720.0)), json!(48))),
            ("statuses", opt(array(agent_status()))),
            ("limit", default_to(pos_int(Some(200.0)), json!(50))),
        ]),
        "update_agent" => raw(vec![
            ("agentId", str_z()),
            ("name", opt(str_z())),
            ("labels", opt(labels())),
            ("settings", opt(update_agent_settings())),
        ]),
        "rename_workspace" => raw(vec![
            ("workspaceId", opt(trimmed_min1())),
            ("title", string().trim().min_msg(1, "title is required").z()),
        ]),
        "list_workspace_scripts" => raw(vec![("workspaceId", str_z())]),
        "start_workspace_script" | "stop_workspace_script" => {
            raw(vec![("workspaceId", str_z()), ("scriptName", str_min1())])
        }
        "list_terminals" => raw(vec![("cwd", opt(str_z())), ("all", opt(boolean()))]),
        "create_terminal" => raw(vec![("cwd", opt(str_z())), ("name", opt(str_z()))]),
        "kill_terminal" => raw(vec![("terminalId", str_z())]),
        "capture_terminal" => raw(vec![
            ("terminalId", str_z()),
            ("start", opt(number())),
            ("end", opt(number())),
            ("scrollback", opt(boolean())),
            ("stripAnsi", default_to(boolean(), json!(true))),
        ]),
        "send_terminal_keys" => raw(vec![
            ("terminalId", str_z()),
            ("keys", str_z()),
            ("literal", opt(boolean())),
        ]),
        "create_schedule" => raw(vec![
            ("prompt", schedule_text("prompt is required")),
            ("cron", schedule_text("cron is required")),
            ("timezone", opt(trimmed_min1())),
            ("name", opt(str_z())),
            ("provider", if caller { opt(str_z()) } else { str_z() }),
            ("cwd", opt(str_z())),
            ("isolation", opt(Z::Enum(&["local", "worktree"]))),
            ("maxRuns", opt(pos_int(None))),
            ("expiresIn", opt(str_z())),
        ]),
        "create_heartbeat" => raw(vec![
            ("prompt", schedule_text("prompt is required")),
            ("cron", schedule_text("cron is required")),
            ("timezone", opt(trimmed_min1())),
            ("name", opt(str_z())),
            ("maxRuns", opt(pos_int(None))),
            ("expiresIn", opt(str_z())),
        ]),
        "delete_heartbeat" | "run_schedule_once" => raw(vec![("id", str_min1())]),
        "list_schedules" | "list_providers" | "list_profiles" | "list_pending_permissions" => {
            raw(vec![])
        }
        "inspect_schedule" | "pause_schedule" | "resume_schedule" | "delete_schedule"
        | "schedule_logs" => id_only(),
        "update_schedule" => InputSchema::schema(
            obj(vec![
                ("id", str_z()),
                ("cron", opt(str_z())),
                ("timezone", opt(trimmed_min1())),
                ("name", opt(nullable(str_z()))),
                ("prompt", opt(trimmed_min1())),
                ("maxRuns", opt(nullable(pos_int(None)))),
                ("provider", opt(trimmed_min1())),
                ("model", opt(nullable(trimmed_min1()))),
                ("mode", opt(nullable(trimmed_min1()))),
                ("cwd", opt(trimmed_min1())),
                ("expiresIn", opt(str_z())),
                ("clearExpires", opt(boolean())),
            ])
            .passthrough(),
        ),
        "list_models" => raw(vec![("provider", str_z())]),
        "inspect_provider" => raw(vec![
            ("provider", provider_or_provider_model_input()),
            ("cwd", opt(str_z())),
            ("settings", opt(inspect_provider_settings())),
        ]),
        "get_agent_activity" => raw(vec![("agentId", str_z()), ("limit", opt(number()))]),
        "set_agent_mode" => raw(vec![("agentId", str_z()), ("modeId", str_z())]),
        "respond_to_permission" => raw(vec![
            ("agentId", str_z()),
            ("requestId", str_z()),
            ("response", permission_response()),
        ]),
        _ => return None,
    };
    Some(schema)
}

fn schedule_text(message: &str) -> Z {
    string().trim().min_msg(1, message).z()
}
