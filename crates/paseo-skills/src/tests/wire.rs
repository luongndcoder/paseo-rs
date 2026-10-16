//! `agent.skills.*` wire responses against the TS golden (`fixtures/p4/skills/wire-scenario.json`).

use super::common::*;
use crate::controller::{SaveRequest, SkillsController};
use crate::selection::{InMemoryConfigStore, SelectionStore, SkillSelection};
use crate::wire;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;

fn golden() -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/p4/skills/wire-scenario.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn controller_for(sb: &Sandbox, initial: Option<SkillSelection>) -> SkillsController {
    let config = Arc::new(match initial {
        Some(s) => InMemoryConfigStore::with_selection(&s),
        None => InMemoryConfigStore::new(),
    });
    let targets = sb.targets.clone();
    SkillsController::new(
        Arc::new(move || targets.clone()),
        SelectionStore::new(config),
    )
}

#[test]
fn wire_responses_are_byte_equal_to_the_ts_golden() {
    let sb = sandbox();
    for name in BUNDLED_SKILLS {
        write_bundle_skill(
            &sb.targets.source_dir,
            name,
            &[("SKILL.md", &format!("{name}-v1"))],
        );
    }
    write_bundle_skill(
        &sb.targets.source_dir,
        "paseo-loop",
        &[("references/guide.md", "guide-v1")],
    );
    let main = controller_for(&sb, Some(SkillSelection::All));
    let mut fresh: Option<SkillsController> = None;
    let steps = golden();
    let steps = steps["steps"].as_array().unwrap();
    // The first two `import` steps after the main-controller import use the unset store.
    let mut imports_seen = 0;
    for (i, step) in steps.iter().enumerate() {
        let call = step["call"].as_str().unwrap();
        let args = &step["args"];
        let actual: Value = match call {
            "status" => wire::status_response(
                "agent.skills.get_status.response",
                "req-1",
                &main.status().unwrap(),
            ),
            "uninstall" => wire::status_response(
                "agent.skills.uninstall.response",
                "req-1",
                &main.uninstall().unwrap(),
            ),
            "reconcile" => wire::status_response(
                "agent.skills.reconcile.response",
                "req-1",
                &main.update().unwrap(),
            ),
            "save" => {
                let mut request = args["selection"].clone();
                request["confirmedRemovals"] = args["confirmedRemovals"].clone();
                wire::save_selection_response(
                    "req-1",
                    &main.save(&SaveRequest::from_value(&request)).unwrap(),
                )
            }
            "import" => {
                imports_seen += 1;
                let controller = if imports_seen == 1 {
                    &main
                } else {
                    fresh.get_or_insert_with(|| controller_for(&sb, None))
                };
                wire::import_legacy_selection_response(
                    "req-1",
                    &controller
                        .import_legacy_selection_if_unset(&args["selection"])
                        .unwrap(),
                )
            }
            other => panic!("unknown step {other}"),
        };
        assert_eq!(
            serde_json::to_string(&actual).unwrap(),
            step["text"].as_str().unwrap(),
            "step {i} ({call})"
        );
    }
}
