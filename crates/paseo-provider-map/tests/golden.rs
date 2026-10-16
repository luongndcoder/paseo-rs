use paseo_provider_map::testing::{
    assert_provider_goldens, load_mapper_vectors, load_provider_events, replay, PROVIDERS,
};
use paseo_provider_map::{mapper_for, CounterEnv, MapEnv};

#[test]
fn fixtures_load_for_every_provider() {
    for p in PROVIDERS {
        let goldens = load_provider_events(p);
        assert!(!goldens.is_empty(), "{p}: no provider-event goldens");
        for g in &goldens {
            assert!(
                matches!(g.status.as_str(), "ok" | "partial" | "failed"),
                "{p}/{}",
                g.fixture
            );
        }
        let _ = load_mapper_vectors(p);
    }
}

#[test]
fn placeholder_mappers_emit_nothing() {
    for p in PROVIDERS {
        let mut m = mapper_for(p).unwrap();
        assert_eq!(m.provider(), p);
        assert!(replay(m.as_mut(), &[serde_json::json!({})]).is_empty());
    }
}

#[test]
fn counter_env_is_deterministic() {
    let mut e = CounterEnv::new();
    assert_eq!(e.uuid(), "00000000-0000-4000-8000-000000000001");
    assert_eq!(e.now_iso(), "1970-01-01T00:00:00.000Z");
}

macro_rules! golden {
    ($name:ident, $p:literal) => {
        #[test]
        #[ignore = "WP10: mapper not implemented"]
        fn $name() {
            assert_provider_goldens($p, false);
        }
    };
}
golden!(pi_goldens, "pi");
golden!(acp_goldens, "acp");
golden!(opencode_goldens, "opencode");
golden!(codex_goldens, "codex");
golden!(claude_goldens, "claude");
