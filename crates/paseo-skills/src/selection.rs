//! Skill selection model, coercion and the `skills.selection` config port.
//!
//! Ports `selection-store.ts`. `skills.selection` lives in the daemon config, which TS
//! still owns during the strangler (spec Q10), so the engine reaches it through the
//! [`ConfigStore`] trait. The production adapter calls TS over the internal RPC until
//! ownership flips; tests use [`InMemoryConfigStore`].

use crate::error::Result;
use serde::Serialize;
use serde_json::Value;
use std::sync::{Arc, Mutex};

/// What the user asked to have installed. `All` follows the bundle as it grows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "mode", rename_all = "lowercase")]
pub enum SkillSelection {
    All,
    Custom { skills: Vec<String> },
}

/// JS `String.prototype.trim` whitespace set (differs from Rust's `char::is_whitespace`).
fn is_js_whitespace(c: char) -> bool {
    matches!(
        c,
        '\u{0009}'..='\u{000D}'
            | '\u{0020}'
            | '\u{00A0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200A}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202F}'
            | '\u{205F}'
            | '\u{3000}'
            | '\u{FEFF}'
    )
}

/// Trim, drop empties, dedupe, sort by UTF-16 code units. Non-arrays and non-strings are
/// dropped silently, as in TS.
pub fn coerce_skill_names(value: &Value) -> Vec<String> {
    let Some(items) = value.as_array() else {
        return Vec::new();
    };
    let mut names: Vec<String> = items
        .iter()
        .filter_map(Value::as_str)
        .map(|s| s.trim_matches(is_js_whitespace).to_string())
        .filter(|s| !s.is_empty())
        .collect();
    names.sort_by(|a, b| crate::fsx::js_cmp(a, b));
    names.dedup();
    names
}

/// Anything that is not a recognizable selection becomes `All`.
pub fn coerce_skill_selection(value: &Value) -> SkillSelection {
    let Some(map) = value.as_object() else {
        return SkillSelection::All;
    };
    match map.get("mode").and_then(Value::as_str) {
        Some("custom") => SkillSelection::Custom {
            skills: coerce_skill_names(map.get("skills").unwrap_or(&Value::Null)),
        },
        _ => SkillSelection::All,
    }
}

/// Port to the daemon config. Called from the actor thread, so implementations may block
/// (an internal-RPC adapter waits for the TS reply).
pub trait ConfigStore: Send + Sync {
    /// Raw stored `skills.selection`, `None` when never set.
    fn selection(&self) -> Result<Option<Value>>;
    fn set_selection(&self, selection: &SkillSelection) -> Result<()>;
}

#[derive(Debug, Default)]
pub struct InMemoryConfigStore {
    value: Mutex<Option<Value>>,
}

impl InMemoryConfigStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_selection(selection: &SkillSelection) -> Self {
        Self {
            value: Mutex::new(serde_json::to_value(selection).ok()),
        }
    }
}

impl ConfigStore for InMemoryConfigStore {
    fn selection(&self) -> Result<Option<Value>> {
        Ok(self.value.lock().unwrap_or_else(|e| e.into_inner()).clone())
    }

    fn set_selection(&self, selection: &SkillSelection) -> Result<()> {
        *self.value.lock().unwrap_or_else(|e| e.into_inner()) =
            serde_json::to_value(selection).ok();
        Ok(())
    }
}

/// Coercing wrapper over a [`ConfigStore`] (TS `createSkillSelectionStore`).
#[derive(Clone)]
pub struct SelectionStore {
    config: Arc<dyn ConfigStore>,
}

impl SelectionStore {
    pub fn new(config: Arc<dyn ConfigStore>) -> Self {
        Self { config }
    }

    pub fn get(&self) -> Result<SkillSelection> {
        Ok(match self.config.selection()? {
            Some(raw) => coerce_skill_selection(&raw),
            None => SkillSelection::All,
        })
    }

    /// Coerces `selection`, persists it and returns what was persisted.
    pub fn set(&self, selection: &Value) -> Result<SkillSelection> {
        let parsed = coerce_skill_selection(selection);
        self.config.set_selection(&parsed)?;
        Ok(parsed)
    }

    pub fn set_parsed(&self, selection: &SkillSelection) -> Result<()> {
        self.config.set_selection(selection)
    }

    pub fn is_set(&self) -> Result<bool> {
        Ok(self.config.selection()?.is_some())
    }
}
