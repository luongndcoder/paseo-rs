//! Pure-function ports of the TS unit tests (`acp-agent.test.ts`, `generic-acp-agent.*.test.ts`).

use std::collections::BTreeMap;

use paseo_acp::agent::{
    AgentMode, ClientCapabilitiesOverride, ConfigFeatureOption, McpServerConfig, PermissionResponse,
};
use paseo_acp::client::normalize_mcp_servers;
use paseo_acp::derive::*;
use paseo_acp::error::Error;
use paseo_acp::host::resolve_terminal_command;
use paseo_acp::launch::{build_version_probe_command, resolve_launch, CommandConfig};
use paseo_acp::permission::{is_chooser_request, select_permission_option};
use paseo_acp::profile::build_client_capabilities;
use paseo_acp::protocol::*;
use paseo_acp::providers::copilot;
use paseo_acp::replay::frames_match;
use serde_json::{json, Value};

fn select(category: &str, values: &[&str], current: &str) -> SessionConfigOption {
    serde_json::from_value(json!({
        "type": "select", "id": format!("{category}-option"), "name": category, "category": category,
        "currentValue": current,
        "options": values.iter().map(|v| json!({"value": v, "name": v})).collect::<Vec<_>>(),
    }))
    .unwrap()
}

fn argv(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| s.to_string()).collect()
}

#[test]
fn client_capabilities_default_to_terminal_only() {
    let caps = build_client_capabilities(None, None);
    assert_eq!(
        serde_json::to_value(caps).unwrap(),
        json!({"fs": {"readTextFile": false, "writeTextFile": false}, "terminal": true})
    );
}

#[test]
fn client_capability_overrides_keep_metadata_and_unset_fields() {
    let mut meta = Extra::new();
    meta.insert("source".into(), json!("provider"));
    let over = ClientCapabilitiesOverride {
        read_text_file: Some(true),
        write_text_file: None,
        terminal: Some(false),
    };
    let caps = build_client_capabilities(Some(&meta), Some(&over));
    assert_eq!(
        serde_json::to_value(caps).unwrap(),
        json!({"fs": {"readTextFile": true, "writeTextFile": false}, "terminal": false, "_meta": {"source": "provider"}})
    );
    assert!(
        serde_json::to_value(build_client_capabilities(Some(&Extra::new()), None))
            .unwrap()
            .get("_meta")
            .is_none()
    );
}

#[test]
fn version_probe_keeps_the_package_runner_prefix() {
    let probe = |a: &[&str]| build_version_probe_command(&argv(a)).unwrap();
    let p = probe(&["npx", "-y", "@google/gemini-cli@0.41.1", "--acp"]);
    assert_eq!(
        (p.command.as_str(), p.args),
        (
            "npx",
            argv(&["-y", "@google/gemini-cli@0.41.1", "--version"])
        )
    );
    let p = probe(&["pnpm", "dlx", "@agent/foo@1.2.3", "--acp"]);
    assert_eq!(
        (p.command.as_str(), p.args),
        ("pnpm", argv(&["dlx", "@agent/foo@1.2.3", "--version"]))
    );
    // Same rule as the TS port: the prefix ends at the first non-flag argument.
    let p = probe(&["uvx", "--from", "pkg==1", "tool", "acp"]);
    assert_eq!(p.args, argv(&["--from", "pkg==1", "--version"]));
    let p = probe(&["npx", "--package", "pkg@1", "tool", "acp"]);
    assert_eq!(p.args, argv(&["--package", "pkg@1", "tool", "--version"]));
    let p = probe(&["hermes", "acp"]);
    assert_eq!(
        (p.command.as_str(), p.args),
        ("hermes", argv(&["--version"]))
    );
}

#[test]
fn launch_resolution_keeps_default_args_after_a_replaced_launcher() {
    let default = argv(&["copilot", "--acp"]);
    assert_eq!(
        resolve_launch(&CommandConfig::Default, &default),
        Some(("copilot".into(), argv(&["--acp"])))
    );
    assert_eq!(
        resolve_launch(&CommandConfig::Append(argv(&["--debug"])), &default),
        Some(("copilot".into(), argv(&["--debug", "--acp"])))
    );
    assert_eq!(
        resolve_launch(&CommandConfig::Replace(argv(&["/opt/wrap", "x"])), &default),
        Some(("/opt/wrap".into(), argv(&["x", "--acp"])))
    );
    assert_eq!(
        resolve_launch(&CommandConfig::Replace(vec![]), &default),
        None
    );
}

#[test]
fn jsonrpc_classification_and_id_normalization() {
    assert!(matches!(
        classify(json!({"jsonrpc": "2.0", "id": 0, "method": "m", "params": {}})),
        Ok(Frame::Request { .. })
    ));
    assert!(matches!(
        classify(json!({"jsonrpc": "2.0", "method": "m"})),
        Ok(Frame::Notification {
            params: Value::Null,
            ..
        })
    ));
    assert!(matches!(
        classify(json!({"jsonrpc": "2.0", "id": 3, "result": null})),
        Ok(Frame::Response {
            result: Ok(Value::Null),
            ..
        })
    ));
    assert!(matches!(
        classify(json!({"jsonrpc": "2.0", "id": 3, "error": {"code": 1, "message": "x"}})),
        Ok(Frame::Response {
            result: Err(RpcError { code: 1, .. }),
            ..
        })
    ));
    assert!(classify(json!([1, 2])).is_err());
    assert!(classify(json!({"jsonrpc": "2.0", "id": 1})).is_err());

    assert_eq!(normalize_response_id(json!("12")), json!(12));
    assert_eq!(normalize_response_id(json!("abc")), json!("abc"));
    assert_eq!(normalize_response_id(json!("")), json!(""));
    assert_eq!(
        normalize_response_id(json!("99999999999999999999")),
        json!("99999999999999999999")
    );
    // A string id the agent chose for its own request stays a string; only responses are rewritten.
    assert!(
        matches!(classify(json!({"jsonrpc": "2.0", "id": "7", "method": "m"})), Ok(Frame::Request { id, .. }) if id == json!("7"))
    );
}

#[test]
fn request_frames_match_the_sdk_shape() {
    assert_eq!(
        request_frame(0, "initialize", json!({})).to_string(),
        r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{}}"#
    );
    assert_eq!(
        response_ok_frame(json!(4), Value::Null).to_string(),
        r#"{"jsonrpc":"2.0","id":4,"result":null}"#
    );
    assert_eq!(
        response_err_frame(json!(4), &RpcError::method_not_found("x/y")).to_string(),
        r#"{"jsonrpc":"2.0","id":4,"error":{"code":-32601,"message":"\"Method not found\": x/y","data":{"method":"x/y"}}}"#
    );
}

#[test]
fn mcp_servers_use_the_acp_shapes() {
    let mut servers = BTreeMap::new();
    servers.insert(
        "a".to_string(),
        McpServerConfig::Stdio {
            command: "node".into(),
            args: vec!["s.js".into()],
            env: BTreeMap::from([("K".into(), "V".into())]),
        },
    );
    servers.insert(
        "b".to_string(),
        McpServerConfig::Http {
            url: "http://x".into(),
            headers: BTreeMap::from([("Authorization".into(), "t".into())]),
        },
    );
    servers.insert(
        "c".to_string(),
        McpServerConfig::Sse {
            url: "http://y".into(),
            headers: BTreeMap::new(),
        },
    );
    let wire = serde_json::to_value(normalize_mcp_servers(&servers)).unwrap();
    assert_eq!(
        wire,
        json!([
            {"name": "a", "command": "node", "args": ["s.js"], "env": [{"name": "K", "value": "V"}]},
            {"type": "http", "name": "b", "url": "http://x", "headers": [{"name": "Authorization", "value": "t"}]},
            {"type": "sse", "name": "c", "url": "http://y", "headers": []},
        ])
    );
}

#[test]
fn summarize_error_matches_the_ts_adapter() {
    let e = Error::Rpc(RpcError {
        code: -32603,
        message: "Internal error".into(),
        data: Some(json!({"details": "Droid process exited unexpectedly (exit code 1)"})),
    });
    let s = e.summarize();
    assert_eq!(
        s.message,
        "Internal error: Droid process exited unexpectedly (exit code 1)"
    );
    assert_eq!(s.code.as_deref(), Some("-32603"));
    assert!(s
        .diagnostic
        .unwrap()
        .contains("Droid process exited unexpectedly"));

    // Nested `error` objects and message fields are searched in order.
    let nested = Error::Rpc(RpcError {
        code: 1,
        message: "Failed".into(),
        data: Some(json!({"error": {"errorMessage": " deep "}})),
    });
    assert_eq!(nested.summarize().message, "Failed: deep");
    let same = Error::Rpc(RpcError {
        code: 1,
        message: "Same".into(),
        data: Some(json!({"message": "Same"})),
    });
    assert_eq!(same.summarize().message, "Same");
    assert_eq!(Error::Closed.summarize().code, None);
}

fn opt(id: &str, kind: &str) -> PermissionOption {
    PermissionOption {
        option_id: id.into(),
        name: id.into(),
        kind: kind.into(),
        meta: None,
    }
}

#[test]
fn permission_options_follow_behavior_order() {
    let options = vec![
        opt("ra", "reject_always"),
        opt("aa", "allow_always"),
        opt("ao", "allow_once"),
        opt("ro", "reject_once"),
    ];
    let pick =
        |r: PermissionResponse| select_permission_option(&options, &r).map(|o| o.option_id.clone());
    assert_eq!(pick(PermissionResponse::allow()).as_deref(), Some("ao"));
    assert_eq!(pick(PermissionResponse::deny()).as_deref(), Some("ro"));
    assert_eq!(
        pick(PermissionResponse::Allow {
            selected_action_id: Some("aa".into())
        })
        .as_deref(),
        Some("aa")
    );
    assert_eq!(
        pick(PermissionResponse::Allow {
            selected_action_id: Some("ro".into())
        }),
        None
    );
    assert_eq!(
        pick(PermissionResponse::Deny {
            selected_action_id: Some("zzz".into()),
            interrupt: false
        }),
        None
    );
    let only_always = vec![opt("aa", "allow_always")];
    assert_eq!(
        select_permission_option(&only_always, &PermissionResponse::allow())
            .map(|o| o.option_id.as_str()),
        Some("aa")
    );
    assert_eq!(
        select_permission_option(&only_always, &PermissionResponse::deny()),
        None
    );

    assert!(!is_chooser_request(&options));
    assert!(is_chooser_request(&[
        opt("a", "allow_once"),
        opt("b", "allow_once")
    ]));
    assert!(!is_chooser_request(&[
        opt("a", "reject_once"),
        opt("b", "reject_once")
    ]));
}

#[test]
fn derives_modes_from_mode_state_then_config_option_then_fallback() {
    let fallback = vec![AgentMode {
        id: "fb".into(),
        label: "Fallback".into(),
        description: None,
    }];
    let state: SessionModeState = serde_json::from_value(json!({
        "availableModes": [{"id": "m1", "name": "One", "description": "d"}], "currentModeId": "m1"
    }))
    .unwrap();
    let (modes, current) = derive_modes(&fallback, Some(&state), &[]);
    assert_eq!(
        (
            modes[0].id.as_str(),
            modes[0].label.as_str(),
            modes[0].description.as_deref(),
            current.as_deref()
        ),
        ("m1", "One", Some("d"), Some("m1"))
    );

    let (modes, current) = derive_modes(
        &fallback,
        None,
        &[select("mode", &["default", "plan"], "plan")],
    );
    assert_eq!(
        modes.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
        ["default", "plan"]
    );
    assert_eq!(current.as_deref(), Some("plan"));

    let (modes, current) = derive_modes(&fallback, None, &[]);
    assert_eq!((modes, current), (fallback, None));
}

#[test]
fn derives_models_and_thinking_options_from_config_options() {
    let options = vec![
        select("model", &["sonnet", "opus"], "opus"),
        select("thought_level", &["low", "high"], "high"),
    ];
    let models = derive_model_definitions("claude-acp", None, &options);
    assert_eq!(
        models
            .iter()
            .map(|m| (m.id.as_str(), m.is_default))
            .collect::<Vec<_>>(),
        [("sonnet", false), ("opus", true)]
    );
    assert_eq!(models[0].thinking_options.len(), 2);
    assert_eq!(
        models[0].default_thinking_option_id.as_deref(),
        Some("high")
    );
    assert_eq!(
        derive_current_config_value(&options, "model").as_deref(),
        Some("opus")
    );
    assert_eq!(derive_current_config_value(&options, "nope"), None);
}

#[test]
fn grouped_select_options_flatten_with_group_metadata() {
    let option: SessionConfigOption = serde_json::from_value(json!({
        "type": "select", "id": "model", "name": "Model", "category": "model", "currentValue": "b",
        "options": [
            {"value": "a", "name": "A"},
            {"group": "G", "name": "Group", "options": [{"value": "b", "name": "B", "description": "bee"}]}
        ]
    }))
    .unwrap();
    let selectors = derive_selector_options(&[option], "model");
    assert_eq!(selectors[0].metadata, None);
    assert_eq!(selectors[1].metadata, Some(json!({"group": "G"})));
    assert_eq!(
        (selectors[1].is_default, selectors[1].description.as_deref()),
        (true, Some("bee"))
    );
}

#[test]
fn feature_options_use_the_empty_option_label() {
    let option: SessionConfigOption = serde_json::from_value(json!({
        "type": "select", "id": "agent", "name": "Agent", "category": "_agent", "currentValue": "",
        "options": [{"value": "", "name": ""}, {"value": "reviewer", "name": "Reviewer"}]
    }))
    .unwrap();
    let feature = ConfigFeatureOption {
        id: "agent".into(),
        config_id: "agent".into(),
        category: Some("_agent".into()),
        label: "Agent".into(),
        empty_option_label: Some("Default".into()),
        ..Default::default()
    };
    let features = derive_features(
        std::slice::from_ref(&option),
        std::slice::from_ref(&feature),
    );
    match &features[0] {
        paseo_acp::agent::Feature::Select { value, options, .. } => {
            assert_eq!(value.as_deref(), Some(""));
            assert_eq!(
                options.iter().map(|o| o.label.as_str()).collect::<Vec<_>>(),
                ["Default", "Reviewer"]
            );
        }
        other => panic!("{other:?}"),
    }
    let wrong_category = ConfigFeatureOption {
        category: Some("other".into()),
        ..feature
    };
    assert!(derive_features(&[option], &[wrong_category]).is_empty());
}

#[test]
fn mode_and_model_selection_resolution() {
    let modes = vec![AgentMode {
        id: "plan".into(),
        label: "Plan".into(),
        description: None,
    }];
    let options = vec![select("mode", &["plan", "act"], "plan")];
    let sel = resolve_mode_selection("act", &modes, &options);
    assert!(sel.has_available_modes && sel.available_mode.is_none() && sel.config_choice.is_some());
    let models: Vec<ModelInfo> =
        serde_json::from_value(json!([{"modelId": "m", "name": "M"}])).unwrap();
    let sel = resolve_model_selection("m", Some(&models), &[]);
    assert!(
        sel.has_available_models && sel.available_model.is_some() && sel.config_option.is_none()
    );
    assert!(!resolve_model_selection("m", None, &[]).has_available_models);
}

#[test]
fn copilot_replaces_autopilot_with_allow_all() {
    assert_eq!(
        copilot::transform_mode_id(copilot::AUTOPILOT_MODE_ID),
        copilot::AGENT_MODE_ID
    );
    assert_eq!(
        copilot::transform_mode_id(copilot::PLAN_MODE_ID),
        copilot::PLAN_MODE_ID
    );

    let mode_opt = |current: &str| -> SessionConfigOption {
        serde_json::from_value(json!({
            "type": "select", "id": "mode", "name": "Mode", "category": "mode", "currentValue": current,
            "options": [
                {"value": copilot::AGENT_MODE_ID, "name": "Agent"},
                {"value": copilot::AUTOPILOT_MODE_ID, "name": "Autopilot"}
            ]
        }))
        .unwrap()
    };
    let allow_all = |on: &str| -> SessionConfigOption {
        serde_json::from_value(json!({
            "type": "select", "id": "allow_all", "name": "Allow all", "currentValue": on,
            "options": [{"value": "off", "name": "Off"}, {"value": "on", "name": "On"}]
        }))
        .unwrap()
    };
    let transformed = copilot::transform_config_options(vec![
        mode_opt(copilot::AUTOPILOT_MODE_ID),
        allow_all("off"),
    ]);
    let m = as_select(&transformed[0]).unwrap();
    assert_eq!(m.current_value, copilot::AGENT_MODE_ID);
    let values: Vec<_> = flatten_select_options(m)
        .into_iter()
        .map(|c| c.choice.value)
        .collect();
    assert_eq!(values, [copilot::AGENT_MODE_ID, "allow-all"]);
    // The allow_all option itself is untouched.
    assert_eq!(as_select(&transformed[1]).unwrap().current_value, "off");

    let on =
        copilot::transform_config_options(vec![mode_opt(copilot::AGENT_MODE_ID), allow_all("on")]);
    assert_eq!(as_select(&on[0]).unwrap().current_value, "allow-all");

    let response: SessionState = serde_json::from_value(json!({
        "sessionId": "s",
        "modes": {"availableModes": [
            {"id": copilot::AGENT_MODE_ID, "name": "Agent"}, {"id": copilot::AUTOPILOT_MODE_ID, "name": "Autopilot"}
        ], "currentModeId": copilot::AUTOPILOT_MODE_ID},
        "configOptions": [allow_all("off")]
    }))
    .unwrap();
    let out = copilot::transform_session_response(response);
    let modes = out.modes.unwrap();
    assert_eq!(
        modes
            .available_modes
            .unwrap()
            .iter()
            .map(|m| m.id.as_str())
            .collect::<Vec<_>>(),
        [copilot::AGENT_MODE_ID, "allow-all"]
    );
    assert_eq!(
        modes.current_mode_id.as_deref(),
        Some(copilot::AGENT_MODE_ID)
    );

    let no_modes = SessionState {
        session_id: Some("s".into()),
        ..Default::default()
    };
    assert_eq!(
        copilot::transform_session_response(no_modes.clone()),
        no_modes
    );
}

#[test]
fn terminal_commands_with_spaces_run_through_a_shell() {
    assert_eq!(
        resolve_terminal_command("ls", None),
        ("ls".to_string(), vec![])
    );
    assert_eq!(
        resolve_terminal_command("ls -la", Some(&argv(&["x"]))),
        ("ls -la".to_string(), argv(&["x"]))
    );
    let (cmd, args) = resolve_terminal_command("echo a && echo b", None);
    if cfg!(windows) {
        assert_eq!(
            (cmd.as_str(), args),
            ("cmd", argv(&["/C", "echo a && echo b"]))
        );
    } else {
        assert_eq!(
            (cmd.as_str(), args),
            ("sh", argv(&["-c", "echo a && echo b"]))
        );
    }
}

#[test]
fn replay_matching_masks_only_the_named_keys() {
    let masked = vec!["messageId".to_string()];
    let a = json!({"params": {"messageId": "x", "prompt": [{"type": "text", "text": "hi"}]}});
    assert!(frames_match(
        &a,
        &json!({"params": {"messageId": "y", "prompt": [{"type": "text", "text": "hi"}]}}),
        &masked
    ));
    assert!(!frames_match(
        &a,
        &json!({"params": {"messageId": "y", "prompt": [{"type": "text", "text": "ho"}]}}),
        &masked
    ));
    assert!(
        !frames_match(
            &a,
            &json!({"params": {"prompt": [{"type": "text", "text": "hi"}]}}),
            &masked
        ),
        "masked keys must still be present"
    );
    assert!(!frames_match(
        &json!({"a": 1}),
        &json!({"a": 1, "b": 2}),
        &masked
    ));
}

#[test]
fn random_uuids_look_like_v4_and_differ() {
    let a = paseo_acp::util::random_uuid();
    let b = paseo_acp::util::random_uuid();
    assert_ne!(a, b);
    let parts: Vec<_> = a.split('-').map(str::len).collect();
    assert_eq!(parts, [8, 4, 4, 4, 12]);
    assert_eq!(a.as_bytes()[14], b'4');
    assert!(matches!(a.as_bytes()[19], b'8' | b'9' | b'a' | b'b'));
}
