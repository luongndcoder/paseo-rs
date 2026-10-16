use paseo_provider_api::{
    AgentSessionConfig, FetchCatalogOptions, LaunchContext, PromptInput, SteerOptions,
    ToolCallResult, ToolSpec,
};
use serde_json::json;

#[test]
fn session_config_round_trips_unknown_keys() {
    let wire = json!({"provider": "mock", "cwd": "/w", "modeId": "plan", "futureKey": {"a": 1}});
    let config: AgentSessionConfig = serde_json::from_value(wire.clone()).unwrap();
    assert_eq!(config.mode_id.as_deref(), Some("plan"));
    assert_eq!(config.extra["futureKey"], json!({"a": 1}));
    assert_eq!(serde_json::to_value(&config).unwrap(), wire);
}

#[test]
fn launch_context_serializes_tools_in_camel_case() {
    let context = LaunchContext {
        agent_id: Some("a1".into()),
        env: None,
        paseo_tools: Some(vec![ToolSpec {
            name: "list_agents".into(),
            description: "List".into(),
            title: None,
            input_schema: Some(json!({"type": "object", "properties": {}})),
        }]),
    };
    assert_eq!(
        serde_json::to_value(&context).unwrap(),
        json!({"agentId": "a1", "paseoTools": [{"name": "list_agents", "description": "List",
            "inputSchema": {"type": "object", "properties": {}}}]})
    );
}

#[test]
fn steer_options_flatten_run_options() {
    let options: SteerOptions = serde_json::from_value(
        json!({"expectedTurnId": "t1", "clearPendingPermissions": true, "clientMessageId": "c1"}),
    )
    .unwrap();
    assert_eq!(options.expected_turn_id, "t1");
    assert_eq!(options.run["clientMessageId"], "c1");
}

#[test]
fn prompt_catalog_options_and_tool_result_shapes() {
    assert_eq!(
        serde_json::to_value(PromptInput::from("hi")).unwrap(),
        json!("hi")
    );
    assert_eq!(
        serde_json::to_value(FetchCatalogOptions::Workspace {
            cwd: "/w".into(),
            force: true
        })
        .unwrap(),
        json!({"scope": "workspace", "cwd": "/w", "force": true})
    );
    let result: ToolCallResult = serde_json::from_value(
        json!({"content": [{"type": "text", "text": "ok"}], "isError": false}),
    )
    .unwrap();
    assert_eq!(result.is_error, Some(false));
}
