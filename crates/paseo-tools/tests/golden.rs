//! Golden fixtures from the TS daemon (`fixtures/p4/tool-catalog`): `tools/list` per catalog
//! configuration (byte for byte), `tools/call` transcripts, text synthesis vectors.

mod common;

use common::*;
use paseo_tools::{
    add_model_visible_structured_content, mcp_tool_listing, serialize_tool_input_parameters,
    ToolResult,
};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

fn compact(value: &Value) -> String {
    serde_json::to_string(value).unwrap()
}

#[tokio::test]
async fn tools_list_is_byte_equal_per_config() {
    let fixture = read_fixture("tool-catalog/tools-list.json");
    let mut tools_checked = 0;
    for list in fixture["lists"].as_array().unwrap() {
        let name = list["name"].as_str().unwrap();
        let (options, runtime) = config_options(name);
        let core = StubCore::new(options);
        let catalog = build_catalog(core, runtime).await;

        let want_names: Vec<&str> = list["toolNames"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n.as_str().unwrap())
            .collect();
        assert_eq!(catalog.names(), want_names, "{name}: tool names and order");

        let defs: Vec<Value> = catalog.tools().map(|t| mcp_tool_listing(t.def())).collect();
        let got = compact(&serde_json::json!({"tools": defs}));
        let want = compact(&list["mcpToolsList"]);
        assert_eq!(got, want, "{name}: tools/list");

        for (tool, golden) in catalog
            .tools()
            .zip(list["catalogTools"].as_array().unwrap())
        {
            let def = tool.def();
            assert_eq!(def.name, golden["name"], "{name}");
            assert_eq!(
                def.title.as_deref(),
                golden["title"].as_str(),
                "{name}/{}: title",
                def.name
            );
            assert_eq!(def.description, golden["description"].as_str().unwrap());
            assert_eq!(
                compact(&serialize_tool_input_parameters(def)),
                compact(&golden["inputJsonSchema"]),
                "{name}/{}: input schema",
                def.name
            );
            assert_eq!(
                def.output_schema.as_ref().map(compact),
                (!golden["outputJsonSchema"].is_null())
                    .then(|| compact(&golden["outputJsonSchema"])),
                "{name}/{}: output schema",
                def.name
            );
            tools_checked += 1;
        }
    }
    assert_eq!(tools_checked, 62 + 39 + 1 + 1 + 36 + 40);
}

#[tokio::test]
async fn tools_call_transcripts_match() {
    let fixture = read_fixture("tool-catalog/tools-call-transcripts.json");
    let mut cores: std::collections::HashMap<
        String,
        (std::sync::Arc<StubCore>, paseo_tools::ToolCatalog),
    > = Default::default();
    let mut passed = 0;
    for (index, transcript) in fixture["transcripts"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
    {
        let config = transcript["config"].as_str().unwrap();
        if !cores.contains_key(config) {
            let (options, runtime) = config_options(config);
            let core = StubCore::new(options);
            let catalog = build_catalog(core.clone(), runtime).await;
            cores.insert(config.to_owned(), (core, catalog));
        }
        let (core, catalog) = &cores[config];
        core.take_calls();
        let response = catalog
            .call_tool_for_mcp(
                transcript["tool"].as_str().unwrap(),
                Some(&transcript["args"]),
                CancellationToken::new(),
            )
            .await;
        assert_eq!(
            compact(&response),
            compact(&transcript["response"]),
            "#{index} {config}/{} {}",
            transcript["tool"],
            transcript["args"]
        );
        let calls = core.take_calls();
        assert_eq!(
            compact(&Value::Array(calls)),
            compact(&transcript["sideEffects"]),
            "#{index} {config}/{}: side effects",
            transcript["tool"]
        );
        passed += 1;
    }
    assert_eq!(passed, 46);

    // The speak handler saw the caller's text.
    let (core, _) = &cores["caller_agent"];
    let speak = core.speak_calls.lock().unwrap().clone();
    assert_eq!(speak.len(), fixture["speakCalls"].as_array().unwrap().len());
    assert_eq!(speak[0]["text"], fixture["speakCalls"][0]["text"]);
    assert_eq!(
        speak[0]["callerAgentId"],
        fixture["speakCalls"][0]["callerAgentId"]
    );
}

#[tokio::test]
async fn direct_catalog_execution_matches() {
    let fixture = read_fixture("tool-catalog/tools-call-transcripts.json");
    let (options, runtime) = config_options("default");
    let catalog = build_catalog(StubCore::new(options), runtime).await;
    for case in fixture["directCatalogExecution"].as_array().unwrap() {
        let outcome = catalog
            .execute_tool(
                case["tool"].as_str().unwrap(),
                Some(&case["args"]),
                CancellationToken::new(),
            )
            .await;
        match (outcome, case.get("threw")) {
            (Ok(result), None) => {
                assert_eq!(
                    compact(&result.to_json()),
                    compact(&case["result"]),
                    "{}",
                    case["tool"]
                );
            }
            (Err(error), Some(threw)) => {
                assert_eq!(
                    error.to_string(),
                    threw["message"].as_str().unwrap(),
                    "{}",
                    case["tool"]
                );
            }
            (outcome, threw) => panic!("{}: got {outcome:?}, want {threw:?}", case["tool"]),
        }
    }
}

#[test]
fn structured_content_text_synthesis_matches() {
    let fixture = read_fixture("tool-catalog/structured-content-text.json");
    let cases = fixture["cases"].as_array().unwrap();
    for case in cases {
        let input = &case["input"];
        let result = ToolResult {
            content: input["content"].as_array().unwrap().clone(),
            structured_content: input.get("structuredContent").cloned(),
            is_error: input.get("isError").and_then(Value::as_bool),
        };
        let got = add_model_visible_structured_content(result).to_json();
        assert_eq!(compact(&got), compact(&case["output"]), "{}", case["name"]);
    }
    assert_eq!(cases.len(), 18);
}

/// `data/tool-defs.json` is derived from the fixture; this catches a stale copy.
#[test]
fn embedded_tool_defs_match_the_fixture() {
    let fixture = read_fixture("tool-catalog/tools-list.json");
    for list in fixture["lists"].as_array().unwrap() {
        let caller = list["name"] == "caller_agent";
        for golden in list["catalogTools"].as_array().unwrap() {
            let def =
                paseo_tools::defs::tool_def(golden["name"].as_str().unwrap(), caller).unwrap();
            assert_eq!(
                compact(&def.input_schema),
                compact(&golden["inputJsonSchema"])
            );
            assert_eq!(def.description, golden["description"].as_str().unwrap());
        }
    }
    assert_eq!(paseo_tools::defs::tool_names().len(), 62);
}
