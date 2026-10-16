//! Replays recorded Pi / OMP traffic (see `fixtures/` and `tools/capture/`).
//!
//! `01`..`08` were captured from the real `pi` through the TS adapter. `omp-*` are hand-written
//! because no `omp` binary was available; they are built from the TS schemas and tests.

use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use paseo_pi::agent::{
    AgentClient, AgentEvent, LaunchContext, PermissionResponse, PromptInput, ResumeOverrides,
    SessionConfig,
};
use paseo_pi::protocol::{
    Command, Event, Message, Model, SessionState, SessionStats, SlashCommand,
};
use paseo_pi::testing::{Fixture, ReplayLauncher, ReplayTransport};
use paseo_pi::{Options, RpcAgentClient, Session};
use serde_json::{json, Value};
use std::sync::Arc;

const TIMEOUT: Duration = Duration::from_secs(20);

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(format!("{name}.jsonl"))
}

fn fixture(name: &str) -> Fixture {
    Fixture::load(fixture_path(name)).unwrap_or_else(|e| panic!("load {name}: {e}"))
}

/// The TS adapter polls usage; the Rust client does not send these.
fn ts_only(fixture: Fixture) -> Fixture {
    fixture.without_requests(&["get_session_stats"])
}

fn all_fixture_names() -> Vec<String> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures");
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.strip_suffix(".jsonl").map(str::to_owned)
        })
        .collect();
    names.sort();
    names
}

/// `null` and an absent key mean the same on this wire.
fn strip_nulls(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(_, v)| !v.is_null())
                .map(|(k, v)| (k.clone(), strip_nulls(v)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(strip_nulls).collect()),
        other => other.clone(),
    }
}

fn recv_until<T>(
    rx: &Receiver<AgentEvent>,
    mut pick: impl FnMut(&AgentEvent) -> Option<T>,
) -> (T, Vec<AgentEvent>) {
    let deadline = Instant::now() + TIMEOUT;
    let mut seen = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let event = rx.recv_timeout(left).unwrap_or_else(|e| {
            panic!(
                "timed out waiting for event ({e}); saw {} events",
                seen.len()
            )
        });
        let picked = pick(&event);
        seen.push(event);
        if let Some(found) = picked {
            return (found, seen);
        }
    }
}

fn client_for(launcher: &Arc<ReplayLauncher>, omp: bool) -> RpcAgentClient {
    let client = if omp {
        RpcAgentClient::omp()
    } else {
        RpcAgentClient::pi()
    };
    client
        .with_command([if omp { "omp" } else { "pi" }])
        .with_request_timeout(TIMEOUT)
        .with_launcher(launcher.clone())
}

fn assert_clean(launcher: &ReplayLauncher) {
    for (n, transport) in launcher.transports().iter().enumerate() {
        assert!(
            transport.mismatches().is_empty(),
            "launch {n}: {:?}",
            transport.mismatches()
        );
    }
}

fn provider_types(events: &[AgentEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Provider { event_type, .. } => Some(event_type.clone()),
            _ => None,
        })
        .collect()
}

/// Pi launches with a Paseo-owned extension file whose temp path changes per run.
fn extension_paths(fixture: &Fixture) -> Vec<PathBuf> {
    let argv = fixture.argv();
    argv.windows(2)
        .filter(|w| w[0] == "--extension")
        .map(|w| PathBuf::from(&w[1]))
        .collect()
}

fn pi_config(fixture: &Fixture) -> SessionConfig {
    SessionConfig {
        cwd: PathBuf::from("/tmp/paseo-pi-fixture"),
        model: Some("nvidia/openai/gpt-oss-20b".into()),
        internal: true,
        extension_paths: extension_paths(fixture),
        ..SessionConfig::default()
    }
}

// ---- wire-level golden checks ---------------------------------------------------------------

#[test]
fn every_recorded_out_frame_round_trips_through_the_typed_events() {
    let mut checked = 0;
    let mut other_types: Vec<String> = Vec::new();
    for name in all_fixture_names() {
        for frame in fixture(&name).out_frames() {
            let event = Event::from_value(frame.clone());
            if event.is_other() {
                other_types.push(format!(
                    "{name}: {}",
                    frame.get("type").unwrap_or(&Value::Null)
                ));
            }
            assert_eq!(
                strip_nulls(&event.to_value()),
                strip_nulls(frame),
                "{name}: frame did not round-trip: {frame}"
            );
            checked += 1;
        }
    }
    assert!(
        checked > 500,
        "expected a real corpus, got {checked} frames"
    );
    assert!(
        other_types.is_empty(),
        "frames fell back to Other: {other_types:?}"
    );
}

#[test]
fn recorded_responses_parse_into_typed_payloads() {
    let mut seen = std::collections::BTreeSet::new();
    for name in all_fixture_names() {
        for frame in fixture(&name).out_frames() {
            if frame["type"] != "response" || frame["success"] != true {
                continue;
            }
            let command = frame["command"].as_str().unwrap_or_default();
            let data = frame.get("data").cloned().unwrap_or(Value::Null);
            fn check<T: serde::Serialize + serde::de::DeserializeOwned>(
                name: &str,
                command: &str,
                value: &Value,
            ) {
                let typed: T = serde_json::from_value(value.clone()).unwrap_or_else(|e| {
                    panic!("{name}: {command} data did not parse: {e}\n{value}")
                });
                assert_eq!(
                    strip_nulls(&serde_json::to_value(typed).unwrap()),
                    strip_nulls(value),
                    "{name}: {command} data did not round-trip"
                );
            }
            match command {
                "get_state" => check::<SessionState>(&name, command, &data),
                "set_model" => check::<Model>(&name, command, &data),
                "get_session_stats" => check::<SessionStats>(&name, command, &data),
                "get_available_models" => {
                    check::<Vec<Model>>(&name, command, &data["models"]);
                }
                "get_commands" | "get_available_commands" => {
                    check::<Vec<SlashCommand>>(&name, command, &data["commands"]);
                }
                "get_messages" => check::<Vec<Message>>(&name, command, &data["messages"]),
                _ => continue,
            }
            seen.insert(command.to_string());
        }
    }
    for expected in [
        "get_state",
        "set_model",
        "get_session_stats",
        "get_available_models",
        "get_commands",
    ] {
        assert!(
            seen.contains(expected),
            "no recorded {expected} response was checked"
        );
    }
}

/// Rebuilds a typed command from a recorded client frame; `None` for frames this client does not
/// originate as commands.
fn command_for(frame: &Value) -> Option<Command> {
    let s = |k: &str| frame[k].as_str().map(str::to_owned);
    Some(match frame["type"].as_str()? {
        "prompt" => Command::Prompt {
            message: s("message")?,
            images: None,
        },
        "steer" => Command::Steer {
            message: s("message")?,
            images: None,
        },
        "clear_queue" => Command::ClearQueue,
        "abort" => Command::Abort,
        "get_state" => Command::GetState,
        "get_messages" => Command::GetMessages,
        "get_available_models" => Command::GetAvailableModels,
        "get_session_stats" => Command::GetSessionStats,
        "get_commands" => Command::GetCommands,
        "get_available_commands" => Command::GetAvailableCommands,
        "set_model" => Command::SetModel {
            provider: s("provider")?,
            model_id: s("modelId")?,
        },
        "set_thinking_level" => Command::SetThinkingLevel { level: s("level")? },
        "negotiate_protocol" => Command::NegotiateProtocol {
            protocol_version: frame["protocolVersion"].as_u64()?,
        },
        "set_subagent_subscription" => Command::SetSubagentSubscription { level: s("level")? },
        "branch" => Command::Branch {
            entry_id: s("entryId")?,
        },
        "get_branch_messages" => Command::GetBranchMessages,
        _ => return None,
    })
}

#[test]
fn every_recorded_client_command_is_what_our_commands_serialize_to() {
    let mut checked = 0;
    for name in all_fixture_names() {
        for frame in fixture(&name).in_frames() {
            let Some(command) = command_for(frame) else {
                continue;
            };
            let mut expected = command.to_value();
            expected["id"] = frame["id"].clone();
            assert_eq!(&expected, frame, "{name}: command serialization differs");
            checked += 1;
        }
    }
    assert!(checked > 30, "only {checked} commands checked");
}

#[test]
fn recorded_pi_1_3_wire_facts() {
    // Facts that differ from older Pi and shaped the types.
    let f = fixture("01-prompt-reply");
    let out: Vec<&Value> = f.out_frames().collect();
    let ack = out.iter().find(|v| v["command"] == "prompt").unwrap();
    assert_eq!(ack["data"]["disposition"], "started");
    let update = out.iter().find(|v| v["type"] == "message_update").unwrap();
    assert!(
        update.get("message").is_none(),
        "pi >= 0.84 omits the cumulative message"
    );
    assert!(out.iter().any(|v| v["type"] == "turn_end"));
    assert!(out
        .iter()
        .any(|v| v["type"] == "message_start" && v["message"]["role"] == "system"));
    let settled = out.iter().find(|v| v["type"] == "agent_settled").unwrap();
    assert_eq!(settled["aborted"], false);
}

// ---- raw session replay -----------------------------------------------------------------------

#[test]
fn raw_session_prompt_streams_typed_events() {
    let fx = ts_only(fixture("01-prompt-reply"));
    let transport = Arc::new(ReplayTransport::new(&fx));
    let options = Options::pi()
        .command(["pi"])
        .model("nvidia/openai/gpt-oss-20b")
        .no_session(true);
    let session = Session::from_transport(&options, transport.clone()).unwrap();
    let events = session.subscribe();

    let state = session.get_state().unwrap();
    assert_eq!(
        state.model.as_ref().unwrap().qualified_id(),
        "nvidia/openai/gpt-oss-20b"
    );
    assert_eq!(state.thinking_level.as_ref().unwrap().as_str(), "medium");

    let ack = session.prompt("Reply with exactly: pong", &[]).unwrap();
    assert_eq!(ack.agent_invoked, None);
    assert_eq!(ack.request_id.as_deref(), Some("req_2"));

    let mut text = String::new();
    let mut tags = Vec::new();
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let event = events
            .recv_timeout(deadline - Instant::now())
            .expect("event");
        tags.push(event.tag().unwrap_or("?").to_string());
        if let Event::MessageUpdate(update) = &event {
            if let paseo_pi::protocol::AssistantMessageEvent::TextDelta(d) =
                &update.assistant_message_event
            {
                text.push_str(d.delta.as_deref().unwrap_or_default());
            }
        }
        if matches!(event, Event::AgentSettled(_)) {
            break;
        }
    }
    assert_eq!(text, "pong");
    for expected in [
        "agent_start",
        "turn_start",
        "message_start",
        "message_end",
        "turn_end",
        "agent_end",
    ] {
        assert!(
            tags.iter().any(|t| t == expected),
            "missing {expected} in {tags:?}"
        );
    }
    assert!(
        transport.mismatches().is_empty(),
        "{:?}",
        transport.mismatches()
    );
    session.close();
}

#[test]
fn raw_session_surfaces_rpc_errors_and_times_out() {
    let fx = Fixture::parse(
        r#"{"argv":["--mode","rpc"]}
{"dir":"in","line":{"type":"set_model","provider":"x","modelId":"y","id":"req_1"}}
{"dir":"out","line":{"id":"req_1","type":"response","command":"set_model","success":false,"error":"Model not found: x/y"}}
{"dir":"in","line":{"type":"get_state","id":"req_2"}}
"#,
    )
    .unwrap();
    let options = Options::pi()
        .command(["pi"])
        .request_timeout(Duration::from_millis(300));
    let session = Session::from_transport(&options, Arc::new(ReplayTransport::new(&fx))).unwrap();
    let err = session.set_model("x", "y").unwrap_err();
    assert_eq!(err.to_string(), "Model not found: x/y");
    // The recording never answers req_2.
    let err = session.get_state().unwrap_err();
    assert!(
        err.to_string()
            .starts_with("request timed out phase=get_state"),
        "{err}"
    );
}

#[test]
fn raw_session_reports_process_exit_to_pending_requests_and_subscribers() {
    let fx = Fixture::parse(
        r#"{"argv":["--mode","rpc"]}
{"dir":"in","line":{"type":"get_state","id":"req_1"}}
{"exit":{"code":3,"signal":null}}
"#,
    )
    .unwrap();
    let options = Options::pi().command(["pi"]);
    let session = Session::from_transport(&options, Arc::new(ReplayTransport::new(&fx))).unwrap();
    let events = session.subscribe();
    let err = session.get_state().unwrap_err();
    assert!(err.to_string().contains("exited with code 3"), "{err}");
    let exit = events.recv_timeout(TIMEOUT).unwrap();
    assert!(matches!(exit, Event::ProcessExit(_)), "{exit:?}");
    // Stopping work on a dead process is already done.
    session.abort().unwrap();
}

// ---- agent-level replay: Pi ---------------------------------------------------------------------

#[test]
fn pi_prompt_reply() {
    let fx = ts_only(fixture("01-prompt-reply"));
    let config = pi_config(&fx);
    let launcher = ReplayLauncher::new([fx.clone()]);
    let client = client_for(&launcher, false);

    let session = client
        .create_session(&config, &LaunchContext::default())
        .unwrap();
    assert_eq!(
        launcher.launches()[0][1..],
        fx.argv()[..],
        "launch argv differs from the TS adapter's"
    );

    let result = session.run("Reply with exactly: pong".into()).unwrap();
    assert_eq!(result.final_text, "pong");
    let types = provider_types(&result.events);
    assert!(types.contains(&"message_update".to_string()));
    assert!(matches!(
        result.events.first(),
        Some(AgentEvent::TurnStarted { .. })
    ));
    assert_eq!(
        session.runtime_info().unwrap().model.as_deref(),
        Some("nvidia/openai/gpt-oss-20b")
    );
    session.close().unwrap();
    assert_clean(&launcher);
}

#[test]
fn pi_tool_call_is_visible_as_raw_events() {
    let fx = ts_only(fixture("02-tool-bash"));
    let launcher = ReplayLauncher::new([fx.clone()]);
    let session = client_for(&launcher, false)
        .create_session(&pi_config(&fx), &LaunchContext::default())
        .unwrap();
    let result = session
        .run("Use the bash tool to run `echo hello-from-pi` and tell me the output.".into())
        .unwrap();
    assert!(
        result.final_text.contains("hello-from-pi"),
        "{}",
        result.final_text
    );
    let start = result
        .events
        .iter()
        .find_map(|e| match e {
            AgentEvent::Provider {
                event_type, raw, ..
            } if event_type == "tool_execution_start" => Some(raw),
            _ => None,
        })
        .expect("tool_execution_start");
    assert_eq!(start["toolName"], "bash");
    assert!(start["args"]["command"]
        .as_str()
        .unwrap()
        .contains("echo hello-from-pi"));
    assert!(provider_types(&result.events).contains(&"tool_execution_end".to_string()));
    assert_clean(&launcher);
}

fn run_permission_scenario(
    name: &str,
    ending: &str,
    response: impl Fn(&paseo_pi::agent::PermissionRequest) -> PermissionResponse,
) -> (String, Vec<AgentEvent>) {
    let fx = ts_only(fixture(name));
    let launcher = ReplayLauncher::new([fx.clone()]);
    let session = client_for(&launcher, false)
        .create_session(&pi_config(&fx), &LaunchContext::default())
        .unwrap();
    let rx = session.subscribe();
    session
        .start_turn(format!("Call the ask_user_question tool exactly once with one question 'Which snack?' and options Apple and Cookie. Then tell me {ending}.").into())
        .unwrap();
    let (request, mut seen) = recv_until(&rx, |e| match e {
        AgentEvent::PermissionRequested { request, .. } => Some(request.clone()),
        _ => None,
    });
    assert_eq!(session.pending_permissions().len(), 1);
    assert_eq!(request.kind, paseo_pi::agent::PermissionKind::Question);
    session
        .respond_to_permission(&request.id, response(&request))
        .unwrap();
    assert!(session.pending_permissions().is_empty());
    let (text, rest) = recv_until(&rx, |e| match e {
        AgentEvent::TurnCompleted { final_text, .. } => Some(final_text.clone()),
        AgentEvent::TurnFailed { error, .. } => panic!("turn failed: {error}"),
        _ => None,
    });
    seen.extend(rest);
    assert!(seen
        .iter()
        .any(|e| matches!(e, AgentEvent::PermissionResolved { .. })));
    assert_clean(&launcher);
    (text, seen)
}

#[test]
fn pi_permission_allow_answers_the_dialog() {
    // The recording answered with the first option's label.
    let (text, _) =
        run_permission_scenario("03-permission-allow", "which one I picked", |request| {
            let option = request.input["questions"][0]["options"][0]["label"]
                .as_str()
                .unwrap()
                .to_string();
            PermissionResponse::Allow {
                updated_input: Some(json!({"answers": {"Response": option}})),
            }
        });
    assert!(text.contains("Apple"), "{text}");
}

#[test]
fn pi_permission_deny_cancels_the_dialog() {
    let (text, _) = run_permission_scenario("04-permission-deny", "what happened", |_| {
        PermissionResponse::Deny { message: None }
    });
    assert!(text.to_lowercase().contains("declined"), "{text}");
}

#[test]
fn pi_interrupt_cancels_the_turn() {
    let fx = ts_only(fixture("05-interrupt"));
    let launcher = ReplayLauncher::new([fx.clone()]);
    let session = client_for(&launcher, false)
        .create_session(&pi_config(&fx), &LaunchContext::default())
        .unwrap();
    let rx = session.subscribe();
    let turn = session
        .start_turn("Count from 1 to 400, one number per line, with no other text.".into())
        .unwrap();
    session.interrupt().unwrap();
    let (reason, _) = recv_until(&rx, |e| match e {
        AgentEvent::TurnCanceled { turn_id, reason } if *turn_id == turn.turn_id => {
            Some(reason.clone())
        }
        AgentEvent::TurnCompleted { .. } => panic!("an interrupted turn must not complete"),
        _ => None,
    });
    assert_eq!(reason, "interrupted");
    let writes = launcher.transports()[0].writes();
    let order: Vec<&str> = writes.iter().filter_map(|w| w["type"].as_str()).collect();
    assert_eq!(order, ["get_state", "prompt", "clear_queue", "abort"]);
    assert_clean(&launcher);
}

#[test]
fn pi_resume_launches_on_the_session_file_and_keeps_the_conversation() {
    let first = ts_only(fixture("06-resume-first"));
    let second = ts_only(fixture("06-resume"));
    let launcher = ReplayLauncher::new([first.clone(), second.clone()]);
    let client = client_for(&launcher, false);

    let mut config = pi_config(&first);
    config.internal = false;
    let s1 = client
        .create_session(&config, &LaunchContext::default())
        .unwrap();
    let r1 = s1
        .run("Remember the secret word: tangerine. Reply with exactly: ok".into())
        .unwrap();
    assert_eq!(r1.final_text.trim(), "ok");
    let handle = s1.describe_persistence().unwrap();
    assert!(handle.native_handle.as_deref().unwrap().ends_with(".jsonl"));
    assert_eq!(handle.metadata["model"], "nvidia/openai/gpt-oss-20b");
    s1.close().unwrap();

    // Resume with the recorded launch's extension; everything else comes from the handle.
    let overrides = ResumeOverrides {
        thinking_option_id: Some("medium".into()),
        extension_paths: extension_paths(&second),
        ..ResumeOverrides::default()
    };
    // The handle's model is the one the session is already on, so no set_model is sent.
    let mut handle = handle.clone();
    handle
        .metadata
        .insert("cwd".into(), json!("/tmp/paseo-pi-fixture"));
    let s2 = client
        .resume_session(&handle, &overrides, &LaunchContext::default())
        .unwrap();
    let r2 = s2
        .run("What was the secret word? Reply with just the word.".into())
        .unwrap();
    assert!(
        r2.final_text.to_lowercase().contains("tangerine"),
        "{}",
        r2.final_text
    );
    let launches = launcher.launches();
    let resumed = &launches[1];
    assert!(resumed.contains(&"--session".to_string()));
    assert!(
        !resumed.contains(&"--model".to_string()),
        "model is applied after launch, not on argv"
    );
    assert_eq!(resumed[1..], second.argv()[..]);
    assert_clean(&launcher);
}

#[test]
fn pi_model_and_thinking_switch() {
    let fx = ts_only(fixture("07-model-thinking-switch"));
    let launcher = ReplayLauncher::new([fx.clone()]);
    let session = client_for(&launcher, false)
        .create_session(&pi_config(&fx), &LaunchContext::default())
        .unwrap();
    let info = session.runtime_info().unwrap();
    assert_eq!(info.model.as_deref(), Some("nvidia/openai/gpt-oss-20b"));
    assert_eq!(info.thinking_option_id.as_deref(), Some("medium"));

    session
        .set_model(Some("nvidia/z-ai/glm-5.3-flash"))
        .unwrap();
    session.set_thinking_option(Some("high")).unwrap();
    let info = session.runtime_info().unwrap();
    assert_eq!(info.model.as_deref(), Some("nvidia/z-ai/glm-5.3-flash"));
    assert_eq!(info.thinking_option_id.as_deref(), Some("high"));
    assert_eq!(
        session.describe_persistence().unwrap().metadata["model"],
        "nvidia/z-ai/glm-5.3-flash"
    );

    let commands = session.list_commands().unwrap();
    assert!(
        commands.iter().any(|c| c.name == "compact"),
        "Paseo-handled builtins are listed"
    );
    assert!(commands.iter().any(|c| c.name == "paseo_capture_entries"));

    assert!(session.available_modes().unwrap().is_empty());
    assert!(session.set_mode("plan").is_err());

    let result = session.run("Reply with exactly: pong".into()).unwrap();
    assert_eq!(result.final_text.trim(), "pong");
    assert_clean(&launcher);
}

#[test]
fn pi_catalog_maps_models_and_flags_the_configured_default() {
    let fx = fixture("08-catalog");
    let recorded: usize = fx
        .out_frames()
        .find(|v| v["command"] == "get_available_models")
        .map(|v| v["data"]["models"].as_array().unwrap().len())
        .unwrap();
    let configured = fx
        .out_frames()
        .find(|v| v["command"] == "get_state")
        .map(|v| {
            format!(
                "{}/{}",
                v["data"]["model"]["provider"].as_str().unwrap(),
                v["data"]["model"]["id"].as_str().unwrap()
            )
        })
        .unwrap();
    let launcher = ReplayLauncher::new([fx]);
    let client = client_for(&launcher, false);
    let catalog = client
        .fetch_catalog(Some(std::path::Path::new("/tmp/paseo-pi-fixture")))
        .unwrap();
    assert_eq!(catalog.models.len(), recorded);
    assert!(catalog.modes.is_empty());
    let defaults: Vec<_> = catalog.models.iter().filter(|m| m.is_default).collect();
    assert_eq!(defaults.len(), 1);
    assert_eq!(defaults[0].id, configured);
    let reasoning = catalog
        .models
        .iter()
        .find(|m| m.id == "nvidia/deepseek-ai/deepseek-v4.1-flash")
        .unwrap();
    let ids: Vec<_> = reasoning
        .thinking_options
        .iter()
        .map(|o| o.id.as_str())
        .collect();
    assert_eq!(ids, ["off", "high", "max"]);
    assert_eq!(
        reasoning.default_thinking_option_id.as_deref(),
        Some("high")
    );
    let plain = catalog
        .models
        .iter()
        .find(|m| m.id == "nvidia/google/gemma-3-4b-it")
        .unwrap();
    assert!(plain.thinking_options.is_empty());
    assert_clean(&launcher);
}

#[test]
fn pi_prompt_with_images_sends_them_to_models_that_accept_them() {
    // Synthetic exchange: one image-capable model, one text-only.
    let build = |input: &str| {
        Fixture::parse(&format!(
            r#"{{"argv":["--mode","rpc"]}}
{{"dir":"in","line":{{"type":"get_state","id":"req_1"}}}}
{{"dir":"out","line":{{"id":"req_1","type":"response","command":"get_state","success":true,"data":{{"model":{{"provider":"p","id":"m","input":{input}}},"thinkingLevel":"off","isStreaming":false,"isCompacting":false,"sessionId":"s","messageCount":0}}}}}}
{{"dir":"in","line":{{"type":"prompt","message":"look\n\nsee","id":"req_2"{}}}}}
{{"dir":"out","line":{{"id":"req_2","type":"response","command":"prompt","success":true,"data":{{"agentInvoked":false}}}}}}
"#,
            if input.contains("image") {
                r#","images":[{"type":"image","data":"AAAA","mimeType":"image/png"}]"#
            } else {
                ""
            }
        ))
        .unwrap()
    };
    let prompt = PromptInput::Blocks(vec![
        paseo_pi::agent::PromptBlock::Text("look".into()),
        paseo_pi::agent::PromptBlock::Image {
            data: "AAAA".into(),
            mime_type: "image/png".into(),
        },
        paseo_pi::agent::PromptBlock::Text("see".into()),
    ]);
    let config = SessionConfig {
        cwd: "/tmp".into(),
        ..SessionConfig::default()
    };

    // Image-capable model: image is forwarded; prompt acked with agentInvoked=false completes at once.
    let launcher = ReplayLauncher::new([build(r#"["text","image"]"#)]);
    let session = client_for(&launcher, false)
        .create_session(&config, &LaunchContext::default())
        .unwrap();
    let rx = session.subscribe();
    session.start_turn(prompt.clone()).unwrap();
    let (text, _) = recv_until(&rx, |e| match e {
        AgentEvent::TurnCompleted { final_text, .. } => Some(final_text.clone()),
        _ => None,
    });
    assert_eq!(text, "");
    assert_clean(&launcher);

    // Text-only model: the image becomes a text hint, nothing is attached.
    let text_only = Fixture::parse(
        r#"{"argv":["--mode","rpc"]}
{"dir":"in","line":{"type":"get_state","id":"req_1"}}
{"dir":"out","line":{"id":"req_1","type":"response","command":"get_state","success":true,"data":{"model":{"provider":"p","id":"m","input":["text"]},"thinkingLevel":"off","isStreaming":false,"isCompacting":false,"sessionId":"s","messageCount":0}}}
"#,
    )
    .unwrap();
    let launcher = ReplayLauncher::new([text_only]);
    let session = client_for(&launcher, false)
        .create_session(&config, &LaunchContext::default())
        .unwrap();
    // The recording has no prompt step, so the write is a mismatch; inspect what was sent.
    let _ = session.start_turn(prompt);
    let writes = launcher.transports()[0].writes();
    let sent = writes.iter().find(|w| w["type"] == "prompt").unwrap();
    assert!(sent.get("images").is_none());
    assert!(sent["message"]
        .as_str()
        .unwrap()
        .contains("Image attachment omitted"));
}

// ---- agent-level replay: OMP --------------------------------------------------------------------

#[test]
fn omp_negotiates_v2_then_prompts() {
    let fx = fixture("omp-01-prompt-reply");
    let launcher = ReplayLauncher::new([fx.clone()]);
    let config = SessionConfig {
        cwd: "/tmp/paseo-pi-fixture".into(),
        model: Some("anthropic/claude-haiku-4-5".into()),
        internal: true,
        ..SessionConfig::default()
    };
    let session = client_for(&launcher, true)
        .create_session(&config, &LaunchContext::default())
        .unwrap();
    assert_eq!(launcher.launches()[0][1..], fx.argv()[..]);
    let order: Vec<String> = launcher.transports()[0]
        .writes()
        .iter()
        .filter_map(|w| w["type"].as_str().map(str::to_owned))
        .collect();
    assert_eq!(order, ["negotiate_protocol", "get_state"]);

    let result = session.run("Reply with exactly: pong".into()).unwrap();
    assert_eq!(result.final_text, "pong");
    assert_eq!(session.id().as_deref(), Some("omp-session-1"));
    assert_clean(&launcher);
}

#[test]
fn omp_prompt_rejected_after_ack_fails_the_turn_with_the_reason() {
    let fx = fixture("omp-02-prompt-rejected");
    let launcher = ReplayLauncher::new([fx]);
    let config = SessionConfig {
        cwd: "/tmp".into(),
        internal: true,
        ..SessionConfig::default()
    };
    let session = client_for(&launcher, true)
        .create_session(&config, &LaunchContext::default())
        .unwrap();
    let err = session.run("hello".into()).unwrap_err();
    assert!(
        err.to_string().contains("No API key found for anthropic."),
        "{err}"
    );
    assert_clean(&launcher);
}

#[test]
fn omp_raw_session_host_tools_dialogs_and_branching() {
    let fx = fixture("omp-03-host-tools-and-ui");
    let transport = Arc::new(ReplayTransport::new(&fx));
    let options = Options::omp()
        .command(["omp"])
        .protocol_mode(paseo_pi::ProtocolMode::RpcUi)
        .no_session(true)
        .request_timeout(TIMEOUT);
    let session = Session::from_transport(&options, transport.clone()).unwrap();
    assert_eq!(options.build_launch().argv[1..], fx.argv()[..]);
    let events = session.subscribe();

    assert_eq!(session.get_state().unwrap().queued_message_count, Some(0));
    let commands = session.get_commands().unwrap();
    assert_eq!(commands[0].input_hint(), Some("[instructions]"));
    assert_eq!(commands[1].source.as_deref(), Some("skill"));
    session.set_subagent_subscription("events").unwrap();
    let names = session
        .set_host_tools(vec![json!({"name":"paseo_echo","description":"Echo text",
            "parameters":{"type":"object","properties":{"text":{"type":"string"}}}})])
        .unwrap();
    assert_eq!(names, ["paseo_echo"]);
    assert!(
        session
            .prompt("use the echo tool", &[])
            .unwrap()
            .agent_invoked
            == Some(true)
    );

    let mut tags = Vec::new();
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let event = events
            .recv_timeout(deadline - Instant::now())
            .expect("event");
        tags.push(event.tag().unwrap_or("?").to_string());
        match &event {
            Event::ExtensionUiRequest(ui) => {
                assert!(ui.is_dialog());
                session
                    .respond_to_extension_ui_request(
                        &ui.id,
                        &paseo_pi::protocol::UiResponse::confirmed(true),
                    )
                    .unwrap();
            }
            Event::HostToolCall(call) => {
                assert_eq!(call.tool_name, "paseo_echo");
                assert_eq!(call.arguments["text"], "hi");
                session
                    .send_host_tool_update(
                        &call.id,
                        json!({"content":[{"type":"text","text":"h"}]}),
                    )
                    .unwrap();
                session
                    .send_host_tool_result(
                        &call.id,
                        json!({"content":[{"type":"text","text":"hi"}]}),
                        Some(false),
                    )
                    .unwrap();
            }
            Event::AgentEnd(_) => break,
            _ => {}
        }
    }
    for expected in [
        "subagent_lifecycle",
        "subagent_progress",
        "todo_reminder",
        "available_commands_update",
    ] {
        assert!(
            tags.iter().any(|t| t == expected),
            "missing {expected} in {tags:?}"
        );
    }
    let branchable = session.get_branch_messages().unwrap();
    assert_eq!(
        branchable,
        [("e1".to_string(), "use the echo tool".to_string())]
    );
    assert_eq!(session.branch("e1").unwrap(), "use the echo tool");
    assert_eq!(session.active_branch_entry_id().as_deref(), Some("e1"));
    assert!(
        transport.mismatches().is_empty(),
        "{:?}",
        transport.mismatches()
    );
    assert!(
        transport.unsent().is_empty(),
        "unsent: {:?}",
        transport.unsent()
    );
}

#[test]
fn omp_only_calls_are_rejected_on_pi() {
    let fx = Fixture::parse("{\"argv\":[]}\n").unwrap();
    let session = Session::from_transport(
        &Options::pi().command(["pi"]),
        Arc::new(ReplayTransport::new(&fx)),
    )
    .unwrap();
    assert!(matches!(
        session.set_host_tools(vec![]),
        Err(paseo_pi::Error::Unsupported(_))
    ));
    assert!(matches!(
        session.branch("x"),
        Err(paseo_pi::Error::Unsupported(_))
    ));
}
