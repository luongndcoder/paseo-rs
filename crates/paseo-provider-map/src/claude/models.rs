//! Claude model manifest subset: runtime model id normalisation and context window sizes
//! (`claude/model-manifest.ts`, `claude/models.ts`).

use regex::Regex;
use std::sync::OnceLock;

/// `CLAUDE_MODEL_MANIFEST` ids with their `contextWindowMaxTokens`.
const MANIFEST: &[(&str, u64)] = &[
    ("claude-opus-5-5", 1_000_000),
    ("claude-opus-5", 1_000_000),
    ("claude-mythos-5-1", 1_000_000),
    ("claude-fable-5-1", 1_000_000),
    ("claude-fable-5", 1_000_000),
    ("claude-opus-4-8[1m]", 1_000_000),
    ("claude-opus-4-8", 200_000),
    ("claude-sonnet-5-5", 1_000_000),
    ("claude-sonnet-5", 200_000),
    ("claude-sonnet-5[1m]", 1_000_000),
    ("claude-opus-4-7[1m]", 1_000_000),
    ("claude-opus-4-7", 200_000),
    ("claude-opus-4-6[1m]", 1_000_000),
    ("claude-opus-4-6", 200_000),
    ("claude-sonnet-4-6[1m]", 1_000_000),
    ("claude-sonnet-4-6", 200_000),
    ("claude-haiku-5-5", 1_000_000),
    ("claude-haiku-4-5", 200_000),
];

fn is_manifest_id(id: &str) -> bool {
    MANIFEST.iter().any(|(m, _)| *m == id)
}

fn regex(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).expect("static regex"))
}

fn first_manifest(candidates: [String; 2]) -> Option<String> {
    candidates.into_iter().find(|c| is_manifest_id(c))
}

fn single_segment(family: &str, major: &str, one_m: bool) -> Option<String> {
    let family = family.to_lowercase();
    let suffix = if one_m { "[1m]" } else { "" };
    first_manifest([
        format!("claude-{family}-{major}{suffix}"),
        format!("claude-{family}-{major}"),
    ])
}

fn major_minor(family: &str, major: &str, minor: &str, one_m: bool) -> Option<String> {
    let family = family.to_lowercase();
    let suffix = if one_m { "[1m]" } else { "" };
    first_manifest([
        format!("claude-{family}-{major}-{minor}{suffix}"),
        format!("claude-{family}-{major}-{minor}"),
    ])
}

/// `normalizeClaudeManifestModelId`.
pub fn normalize_manifest_model_id(value: Option<&str>) -> Option<String> {
    static SINGLE: OnceLock<Regex> = OnceLock::new();
    static MINOR: OnceLock<Regex> = OnceLock::new();
    let trimmed = paseo_tool_detail::js::js_trim(value?);
    if trimmed.is_empty() {
        return None;
    }
    if is_manifest_id(trimmed) {
        return Some(trimmed.to_owned());
    }
    let one_m = trimmed.to_lowercase().contains("[1m]");
    let single = regex(
        &SINGLE,
        r"(?i)^(?:claude[-_ ])?(mythos|fable|opus|sonnet|haiku)[-_ ]+([0-9]+)(?:\[1m\])?(?:[-_ ]+[0-9]{8})?(?:\[1m\])?$",
    );
    if let Some(c) = single.captures(trimmed) {
        return single_segment(&c[1], &c[2], one_m);
    }
    let minor = regex(
        &MINOR,
        r"(?i)^(?:claude[-_ ])?(mythos|fable|opus|sonnet|haiku)[-_ ]+([0-9]+)[-.]([0-9]+)(?:\[1m\])?(?:[-_ ]+[0-9]{8})?(?:\[1m\])?$",
    );
    let c = minor.captures(trimmed)?;
    major_minor(&c[1], &c[2], &c[3], one_m)
}

/// `normalizeClaudeRuntimeModelId`.
pub fn normalize_runtime_model_id(value: Option<&str>) -> Option<String> {
    static MINOR: OnceLock<Regex> = OnceLock::new();
    static SINGLE: OnceLock<Regex> = OnceLock::new();
    if let Some(id) = normalize_manifest_model_id(value) {
        return Some(id);
    }
    let trimmed = paseo_tool_detail::js::js_trim(value?);
    if trimmed.is_empty() {
        return None;
    }
    let one_m = trimmed.to_lowercase().contains("[1m]");
    let minor = regex(
        &MINOR,
        r"(?i)claude[-_ ](mythos|fable|opus|sonnet|haiku)[-_ ]+([0-9]+)[-.]([0-9]+)(\[1m\])?",
    );
    if let Some(c) = minor.captures(trimmed) {
        if let Some(id) = major_minor(&c[1], &c[2], &c[3], one_m) {
            return Some(id);
        }
    }
    let single = regex(
        &SINGLE,
        r"(?i)claude[-_ ](mythos|fable|opus|sonnet|haiku)[-_ ]+([0-9]+)(\[1m\])?",
    );
    let c = single.captures(trimmed)?;
    single_segment(&c[1], &c[2], one_m)
}

/// `getClaudeContextWindowMaxTokens`.
pub fn context_window_max_tokens(model_id: Option<&str>) -> Option<u64> {
    let id = model_id?;
    if id.trim().to_lowercase().ends_with("[1m]") {
        return Some(1_000_000);
    }
    let normalized = normalize_runtime_model_id(Some(id))?;
    MANIFEST
        .iter()
        .find(|(m, _)| *m == normalized)
        .map(|(_, w)| *w)
}

/// `CLAUDE_MODEL_MANIFEST` labels, parallel to `MANIFEST`.
const LABELS: &[&str] = &[
    "Opus 5.5",
    "Opus 5",
    "Mythos 5.1",
    "Fable 5.1",
    "Fable 5",
    "Opus 4.8 1M",
    "Opus 4.8",
    "Sonnet 5.5",
    "Sonnet 5",
    "Sonnet 5 1M",
    "Opus 4.7 1M",
    "Opus 4.7",
    "Opus 4.6 1M",
    "Opus 4.6",
    "Sonnet 4.6 1M",
    "Sonnet 4.6",
    "Haiku 5.5",
    "Haiku 4.5",
];

/// `findClaudeModel(modelId)?.label`.
pub fn find_model_label(model_id: &str) -> Option<&'static str> {
    let normalized = normalize_runtime_model_id(Some(model_id))?;
    MANIFEST
        .iter()
        .position(|(m, _)| *m == normalized)
        .map(|i| LABELS[i])
}

/// `resolveObservedClaudeModelId`: the manifest id when known, else the raw trimmed string;
/// `None` for placeholders and empty values.
pub fn resolve_observed_model_id(value: Option<&str>) -> Option<String> {
    let trimmed = paseo_tool_detail::js::js_trim(value?);
    if trimmed.is_empty() || trimmed == "<synthetic>" {
        return None;
    }
    Some(normalize_runtime_model_id(Some(trimmed)).unwrap_or_else(|| trimmed.to_owned()))
}
