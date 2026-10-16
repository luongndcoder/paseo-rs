//! Wire/storage types. Field order matches the zod schema key order so serialized
//! JSON is byte-compatible with what `StoredScheduleSchema.parse` produces in TS.

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

pub type JsonMap = Map<String, Value>;

/// Required key whose value may be `null` (zod `.nullable()`): a missing key is an error.
fn required_nullable<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(d)
}

/// Optional key whose value may be `null` (zod `.nullable().optional()`):
/// absent -> `None`, `null` -> `Some(None)`, value -> `Some(Some(v))`.
pub fn double_option<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Some(Option::<T>::deserialize(d)?))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScheduleStatus {
    Active,
    Paused,
    Completed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ScheduleCadence {
    #[serde(rename = "every")]
    Every {
        #[serde(rename = "everyMs")]
        every_ms: u64,
    },
    #[serde(rename = "cron")]
    Cron {
        expression: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timezone: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Isolation {
    Local,
    Worktree,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewAgentConfig {
    pub provider: String,
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_option_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archive_on_finish: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isolation: Option<Isolation>,
    /// zod `.nullable().optional()`.
    #[serde(
        default,
        deserialize_with = "double_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub title: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_options: Option<JsonMap>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feature_values: Option<JsonMap>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_servers: Option<JsonMap>,
}

impl NewAgentConfig {
    pub fn new(provider: impl Into<String>, cwd: impl Into<String>) -> Self {
        NewAgentConfig {
            provider: provider.into(),
            cwd: cwd.into(),
            mode_id: None,
            model: None,
            thinking_option_id: None,
            archive_on_finish: None,
            isolation: None,
            title: None,
            provider_options: None,
            feature_values: None,
            system_prompt: None,
            mcp_servers: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ScheduleTarget {
    #[serde(rename = "agent")]
    Agent {
        #[serde(rename = "agentId")]
        agent_id: String,
    },
    #[serde(rename = "new-agent")]
    NewAgent { config: NewAgentConfig },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunStatus {
    Running,
    Succeeded,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScheduleRun {
    pub id: String,
    pub scheduled_for: String,
    pub started_at: String,
    #[serde(deserialize_with = "required_nullable")]
    pub ended_at: Option<String>,
    pub status: RunStatus,
    #[serde(deserialize_with = "required_nullable")]
    pub agent_id: Option<String>,
    /// zod `.nullable().optional()`.
    #[serde(
        default,
        deserialize_with = "double_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub workspace_id: Option<Option<String>>,
    #[serde(deserialize_with = "required_nullable")]
    pub output: Option<String>,
    #[serde(deserialize_with = "required_nullable")]
    pub error: Option<String>,
}

impl ScheduleRun {
    pub fn workspace(&self) -> Option<&str> {
        self.workspace_id.as_ref().and_then(|w| w.as_deref())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredSchedule {
    pub id: String,
    #[serde(deserialize_with = "required_nullable")]
    pub name: Option<String>,
    pub prompt: String,
    pub cadence: ScheduleCadence,
    pub target: ScheduleTarget,
    pub status: ScheduleStatus,
    pub created_at: String,
    pub updated_at: String,
    #[serde(deserialize_with = "required_nullable")]
    pub next_run_at: Option<String>,
    #[serde(deserialize_with = "required_nullable")]
    pub last_run_at: Option<String>,
    #[serde(deserialize_with = "required_nullable")]
    pub paused_at: Option<String>,
    #[serde(deserialize_with = "required_nullable")]
    pub expires_at: Option<String>,
    #[serde(deserialize_with = "required_nullable")]
    pub max_runs: Option<u64>,
    pub runs: Vec<ScheduleRun>,
}

/// `StoredSchedule` minus `runs` (the list/create/pause/resume wire shape).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScheduleSummary {
    pub id: String,
    #[serde(deserialize_with = "required_nullable")]
    pub name: Option<String>,
    pub prompt: String,
    pub cadence: ScheduleCadence,
    pub target: ScheduleTarget,
    pub status: ScheduleStatus,
    pub created_at: String,
    pub updated_at: String,
    #[serde(deserialize_with = "required_nullable")]
    pub next_run_at: Option<String>,
    #[serde(deserialize_with = "required_nullable")]
    pub last_run_at: Option<String>,
    #[serde(deserialize_with = "required_nullable")]
    pub paused_at: Option<String>,
    #[serde(deserialize_with = "required_nullable")]
    pub expires_at: Option<String>,
    #[serde(deserialize_with = "required_nullable")]
    pub max_runs: Option<u64>,
}

impl From<&StoredSchedule> for ScheduleSummary {
    fn from(s: &StoredSchedule) -> Self {
        ScheduleSummary {
            id: s.id.clone(),
            name: s.name.clone(),
            prompt: s.prompt.clone(),
            cadence: s.cadence.clone(),
            target: s.target.clone(),
            status: s.status,
            created_at: s.created_at.clone(),
            updated_at: s.updated_at.clone(),
            next_run_at: s.next_run_at.clone(),
            last_run_at: s.last_run_at.clone(),
            paused_at: s.paused_at.clone(),
            expires_at: s.expires_at.clone(),
            max_runs: s.max_runs,
        }
    }
}

// ---------------------------------------------------------------------------
// Service inputs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct CreateScheduleInput {
    pub name: Option<String>,
    pub prompt: String,
    pub cadence: ScheduleCadence,
    pub target: ScheduleTarget,
    /// Signed so that `0`/negative reach the service check ("maxRuns must be a positive integer").
    pub max_runs: Option<i64>,
    pub expires_at: Option<String>,
    pub run_on_create: Option<bool>,
}

impl CreateScheduleInput {
    pub fn new(
        prompt: impl Into<String>,
        cadence: ScheduleCadence,
        target: ScheduleTarget,
    ) -> Self {
        CreateScheduleInput {
            name: None,
            prompt: prompt.into(),
            cadence,
            target,
            max_runs: None,
            expires_at: None,
            run_on_create: None,
        }
    }
}

/// Patch for a `new-agent` target. For nullable fields `Some(None)` clears the value.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateNewAgentConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(
        default,
        deserialize_with = "double_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub model: Option<Option<String>>,
    #[serde(
        default,
        deserialize_with = "double_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub mode_id: Option<Option<String>>,
    #[serde(
        default,
        deserialize_with = "double_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub thinking_option_id: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archive_on_finish: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isolation: Option<Isolation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct UpdateScheduleInput {
    pub id: String,
    pub name: Option<Option<String>>,
    pub prompt: Option<String>,
    pub cadence: Option<ScheduleCadence>,
    pub new_agent_config: Option<UpdateNewAgentConfig>,
    pub max_runs: Option<Option<i64>>,
    pub expires_at: Option<Option<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduleExecutionResult {
    pub agent_id: Option<String>,
    pub output: Option<String>,
}

// ---------------------------------------------------------------------------
// zod-equivalent normalization (trim + min(1) + guid), applied wherever TS
// would call `StoredScheduleSchema.parse` or an RPC request schema.
// ---------------------------------------------------------------------------

pub fn is_guid(value: &str) -> bool {
    let groups: Vec<&str> = value.split('-').collect();
    let lens = [8usize, 4, 4, 4, 12];
    groups.len() == 5
        && groups
            .iter()
            .zip(lens)
            .all(|(g, n)| g.len() == n && g.bytes().all(|b| b.is_ascii_hexdigit()))
}

fn trim_min1(value: &mut String, what: &str) -> Result<(), String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(format!("{what} must not be empty"));
    }
    if trimmed.len() != value.len() {
        *value = trimmed.to_string();
    }
    Ok(())
}

fn trim_min1_opt(value: &mut Option<String>, what: &str) -> Result<(), String> {
    match value {
        Some(v) => trim_min1(v, what),
        None => Ok(()),
    }
}

impl ScheduleCadence {
    pub fn normalize(&mut self) -> Result<(), String> {
        match self {
            ScheduleCadence::Every { every_ms } => {
                if *every_ms == 0 {
                    return Err("everyMs must be positive".into());
                }
                Ok(())
            }
            ScheduleCadence::Cron {
                expression,
                timezone,
            } => {
                trim_min1(expression, "expression")?;
                trim_min1_opt(timezone, "timezone")
            }
        }
    }
}

impl NewAgentConfig {
    pub fn normalize(&mut self) -> Result<(), String> {
        trim_min1(&mut self.cwd, "cwd")?;
        trim_min1_opt(&mut self.mode_id, "modeId")?;
        trim_min1_opt(&mut self.model, "model")?;
        trim_min1_opt(&mut self.thinking_option_id, "thinkingOptionId")?;
        if let Some(Some(title)) = &mut self.title {
            trim_min1(title, "title")?;
        }
        Ok(())
    }
}

impl ScheduleTarget {
    pub fn normalize(&mut self) -> Result<(), String> {
        match self {
            ScheduleTarget::Agent { agent_id } => {
                if !is_guid(agent_id) {
                    return Err("Invalid GUID".into());
                }
                Ok(())
            }
            ScheduleTarget::NewAgent { config } => config.normalize(),
        }
    }
}

impl StoredSchedule {
    /// Validate and normalize like `StoredScheduleSchema.parse`.
    pub fn normalize(&mut self) -> Result<(), String> {
        if self.prompt.is_empty() {
            return Err("prompt must not be empty".into());
        }
        if self.max_runs == Some(0) {
            return Err("maxRuns must be positive".into());
        }
        self.cadence.normalize()?;
        self.target.normalize()?;
        for run in &self.runs {
            if let Some(agent_id) = &run.agent_id {
                if !is_guid(agent_id) {
                    return Err("Invalid GUID".into());
                }
            }
        }
        Ok(())
    }

    /// Parse JSON text and validate it like the TS store does on read.
    pub fn parse(content: &str) -> Result<StoredSchedule, String> {
        let mut schedule: StoredSchedule =
            serde_json::from_str(content).map_err(|e| e.to_string())?;
        schedule.normalize()?;
        Ok(schedule)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn accepts_utc_cron_cadence_without_time_zone() {
        let cadence: ScheduleCadence =
            serde_json::from_value(json!({"type": "cron", "expression": "0 9 * * *"})).unwrap();
        assert_eq!(
            serde_json::to_value(&cadence).unwrap(),
            json!({"type": "cron", "expression": "0 9 * * *"})
        );
    }

    #[test]
    fn accepts_timezone_aware_cron_cadence() {
        let value =
            json!({"type": "cron", "expression": "0 9 * * *", "timezone": "America/New_York"});
        let cadence: ScheduleCadence = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(&cadence).unwrap(), value);
    }

    #[test]
    fn nullable_keys_are_required_on_stored_schedules() {
        let full = json!({
            "id": "deadbeef", "name": null, "prompt": "p",
            "cadence": {"type": "every", "everyMs": 1000},
            "target": {"type": "agent", "agentId": "00000000-0000-0000-0000-000000000001"},
            "status": "active", "createdAt": "x", "updatedAt": "x",
            "nextRunAt": null, "lastRunAt": null, "pausedAt": null,
            "expiresAt": null, "maxRuns": null, "runs": []
        });
        assert!(StoredSchedule::parse(&full.to_string()).is_ok());
        let mut missing = full.clone();
        missing.as_object_mut().unwrap().remove("lastRunAt");
        assert!(StoredSchedule::parse(&missing.to_string()).is_err());
    }

    #[test]
    fn config_key_order_follows_the_schema() {
        let mut config = NewAgentConfig::new("claude", "/tmp");
        config.isolation = Some(Isolation::Worktree);
        config.mode_id = Some("m".into());
        config.title = Some(None);
        let text = serde_json::to_string(&config).unwrap();
        assert_eq!(
            text,
            r#"{"provider":"claude","cwd":"/tmp","modeId":"m","isolation":"worktree","title":null}"#
        );
    }

    #[test]
    fn guid_check() {
        assert!(is_guid("00000000-0000-0000-0000-000000000001"));
        assert!(!is_guid("a"));
    }
}
