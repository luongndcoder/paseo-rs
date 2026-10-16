//! Golden replay of the `fetch_agents` sequenced sync flow:
//! snapshot -> changes -> removal -> `cursor_expired` -> `generation_changed` (and the edge cases
//! around the cursor). `fixtures/p4/session/fetch-agents-sync.jsonl` is captured from the TS
//! `DirectorySyncService` and stored-agent projection by
//! `fixtures/p4/session/capture-fetch-agents-sync.mts`; the same inputs go through the Rust
//! `SessionActor` here.

mod session_support;

use std::path::PathBuf;

use paseo_store::{Nullable, ProjectRecord, StoredAgentRecord, WorkspaceRecord};
use serde_json::{json, Value};
use session_support::*;

fn fixture(name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/p4/session")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn record(value: &Value) -> StoredAgentRecord {
    serde_json::from_value(value.clone()).expect("stored agent record")
}

#[tokio::test]
async fn fetch_agents_sync_replays_the_ts_golden() {
    let inputs: Value = serde_json::from_str(&fixture("fetch-agents-sync.inputs.json")).unwrap();
    let golden: Vec<Value> = fixture("fetch-agents-sync.jsonl")
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let steps = inputs["steps"].as_array().unwrap();
    assert_eq!(steps.len(), golden.len());

    let env = Env::new().await;
    let project: ProjectRecord = serde_json::from_value(inputs["project"].clone()).unwrap();
    let workspace: WorkspaceRecord = serde_json::from_value(inputs["workspace"].clone()).unwrap();
    env.projects.upsert(project).await.unwrap();
    env.workspaces.upsert(workspace, None).await.unwrap();
    for initial in inputs["initialRecords"].as_array().unwrap() {
        env.put_agent(record(initial)).await;
    }
    assert_eq!(inputs["generation"], GENERATION);
    let socket = env.connect("client-1", &modern()).await;

    for (step, expected) in steps.iter().zip(&golden) {
        let name = step["name"].as_str().unwrap();
        assert_eq!(expected["step"], name);
        match step["op"].as_str().unwrap() {
            "set_agent" => env.put_agent(record(&step["record"])).await,
            "archive_agent" => {
                let id = step["id"].as_str().unwrap();
                let mut archived = env.store.get(id).await.unwrap().unwrap();
                archived.archived_at = Nullable::Value(step["archivedAt"].as_str().unwrap().into());
                env.put_agent(archived).await;
            }
            "live_update" => {
                let id = step["id"].as_str().unwrap();
                let stored = env.store.get(id).await.unwrap().unwrap();
                let agent = env.services.stored_payload(&stored);
                let project = env
                    .services
                    .project_placement(agent["workspaceId"].as_str().unwrap())
                    .await
                    .unwrap();
                let entry = json!({ "agent": agent, "project": project });
                let payload = env.directory.sequence_agent_update(
                    json!({ "kind": "upsert", "agent": agent, "project": project }),
                    Some(entry),
                    id,
                    true,
                    false,
                );
                assert_eq!(
                    json!({ "type": "agent_update", "payload": payload }),
                    expected["message"],
                    "step {name}"
                );
            }
            "fetch" => {
                let mut request = step["request"].clone();
                let request_id = request["requestId"].as_str().unwrap().to_owned();
                socket.send(request.take()).await;
                let expected_message = &expected["message"];
                let ty = expected_message["type"].as_str().unwrap();
                let got = socket.response(ty, &request_id).await;
                assert_eq!(&got, expected_message, "step {name}");
            }
            other => panic!("unknown op {other}"),
        }
    }
    // Every frame the client got was a reply to one of its requests.
    assert_eq!(socket.dropped(), 0);
}
