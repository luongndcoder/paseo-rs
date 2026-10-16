//! Golden harness helpers (used by `tests/golden.rs` and later WP10 tests).
//!
//! - raw input: `crates/paseo-<provider>/fixtures/<fixture>.jsonl` (one raw event per line)
//! - expected events: `fixtures/p4/provider-events/<provider>/<fixture>.json` (`events`)
//! - function vectors: `fixtures/p4/mapper-vectors/<provider>.jsonl` (`{fn,name,input,output,threw?}`)

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::{CounterEnv, ProviderMapper};

pub const PROVIDERS: [&str; 5] = ["pi", "acp", "opencode", "codex", "claude"];

pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

pub fn p4_fixtures() -> PathBuf {
    repo_root().join("fixtures/p4")
}

/// Golden status from the capture: `ok`, `partial` or `failed`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderEventGolden {
    pub provider: String,
    pub fixture: String,
    pub status: String,
    pub expected: Vec<Value>,
    pub raw: Vec<Value>,
}

fn read_json(path: &Path) -> Value {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn read_jsonl(path: &Path) -> Vec<Value> {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("{}: {e}", path.display())))
        .collect()
}

/// Every provider-event golden for `provider` (sorted by fixture name), including `failed` ones.
/// A missing raw transcript yields an empty `raw`.
pub fn load_provider_events(provider: &str) -> Vec<ProviderEventGolden> {
    let dir = p4_fixtures().join("provider-events").join(provider);
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter_map(|n| n.strip_suffix(".json").map(str::to_owned))
        .collect();
    names.sort();
    names
        .into_iter()
        .map(|fixture| {
            let doc = read_json(&dir.join(format!("{fixture}.json")));
            let raw_path = repo_root()
                .join("crates")
                .join(format!("paseo-{provider}"))
                .join("fixtures")
                .join(format!("{fixture}.jsonl"));
            ProviderEventGolden {
                provider: provider.to_owned(),
                status: doc["status"].as_str().unwrap_or("").to_owned(),
                expected: doc["events"].as_array().cloned().unwrap_or_default(),
                raw: if raw_path.exists() {
                    read_jsonl(&raw_path)
                } else {
                    Vec::new()
                },
                fixture,
            }
        })
        .collect()
}

/// One `mapper-vectors/<provider>.jsonl` line.
#[derive(Debug, Clone)]
pub struct MapperVector {
    pub func: String,
    pub name: String,
    pub input: Value,
    pub output: Value,
    pub threw: Option<Value>,
}

pub fn load_mapper_vectors(provider: &str) -> Vec<MapperVector> {
    let path = p4_fixtures()
        .join("mapper-vectors")
        .join(format!("{provider}.jsonl"));
    if !path.exists() {
        return Vec::new();
    }
    read_jsonl(&path)
        .into_iter()
        .map(|v| MapperVector {
            func: v["fn"].as_str().unwrap_or("").to_owned(),
            name: v["name"].as_str().unwrap_or("").to_owned(),
            input: v["input"].clone(),
            output: v["output"].clone(),
            threw: v.get("threw").cloned(),
        })
        .collect()
}

/// Replay `raw` through `mapper` with a fresh [`CounterEnv`]; output serialized to JSON values.
pub fn replay(mapper: &mut dyn ProviderMapper, raw: &[Value]) -> Vec<Value> {
    let mut env = CounterEnv::new();
    let mut out = Vec::new();
    for r in raw {
        out.extend(mapper.feed(r, &mut env));
    }
    out.extend(mapper.finish(&mut env));
    out.iter()
        .map(|e| serde_json::to_value(e).expect("serialize AgentStreamEvent"))
        .collect()
}

/// Assert every replayable (`ok`) golden for the provider matches; `partial` goldens are
/// compared too only when `include_partial`.
pub fn assert_provider_goldens(provider: &str, include_partial: bool) {
    for g in load_provider_events(provider) {
        let wanted = g.status == "ok" || (include_partial && g.status == "partial");
        if !wanted {
            continue;
        }
        let mut mapper = crate::mapper_for(provider).expect("known provider");
        let actual = replay(mapper.as_mut(), &g.raw);
        assert_eq!(actual, g.expected, "{provider}/{} events differ", g.fixture);
    }
}
