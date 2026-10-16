use crate::selection::{
    coerce_skill_names, coerce_skill_selection, InMemoryConfigStore, SelectionStore, SkillSelection,
};
use serde_json::json;
use std::sync::Arc;

fn store() -> (Arc<InMemoryConfigStore>, SelectionStore) {
    let config = Arc::new(InMemoryConfigStore::new());
    (config.clone(), SelectionStore::new(config))
}

#[test]
fn defaults_missing_selection_to_all_without_persisting_it() {
    let (config, store) = store();
    assert_eq!(store.get().unwrap(), SkillSelection::All);
    assert!(!store.is_set().unwrap());
    assert!(crate::selection::ConfigStore::selection(&*config)
        .unwrap()
        .is_none());
}

#[test]
fn persists_a_normalized_custom_selection() {
    let (config, store) = store();
    store
        .set(&json!({"mode": "custom", "skills": ["paseo-loop", "paseo", "paseo"]}))
        .unwrap();
    assert_eq!(
        crate::selection::ConfigStore::selection(&*config).unwrap(),
        Some(json!({"mode": "custom", "skills": ["paseo", "paseo-loop"]}))
    );
}

#[test]
fn replaces_a_custom_selection_with_all_without_retaining_names() {
    let (config, store) = store();
    store
        .set(&json!({"mode": "custom", "skills": ["paseo"]}))
        .unwrap();
    store.set(&json!({"mode": "all"})).unwrap();
    assert_eq!(
        crate::selection::ConfigStore::selection(&*config).unwrap(),
        Some(json!({"mode": "all"}))
    );
}

#[test]
fn coercion_follows_ts_rules() {
    assert_eq!(coerce_skill_selection(&json!(null)), SkillSelection::All);
    assert_eq!(coerce_skill_selection(&json!([1])), SkillSelection::All);
    assert_eq!(
        coerce_skill_selection(&json!({"mode": "weird"})),
        SkillSelection::All
    );
    assert_eq!(
        coerce_skill_selection(&json!({"mode": "custom"})),
        SkillSelection::Custom { skills: vec![] }
    );
    assert_eq!(
        coerce_skill_names(&json!([" b ", "a", 3, "", "   ", "a", "\u{feff}c\u{a0}"])),
        vec!["a", "b", "c"]
    );
    assert!(coerce_skill_names(&json!("a")).is_empty());
}

#[test]
fn selection_serializes_like_the_zod_schema() {
    assert_eq!(
        serde_json::to_string(&SkillSelection::All).unwrap(),
        r#"{"mode":"all"}"#
    );
    assert_eq!(
        serde_json::to_string(&SkillSelection::Custom {
            skills: vec!["a".into()]
        })
        .unwrap(),
        r#"{"mode":"custom","skills":["a"]}"#
    );
}
