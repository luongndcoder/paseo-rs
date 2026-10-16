//! WebSocket RPC shapes for `schedule/*` (port of `protocol/src/schedule/rpc-schemas.ts`)
//! and the request handler (port of `session/schedule/schedule-session.ts`).
//!
//! The edge deserializes an inbound message into [`ScheduleRequest`] (via
//! [`ScheduleRequest::from_value`], which also applies the zod trim/min/positive
//! checks), calls [`ScheduleSession::handle`], and serializes the returned
//! [`ScheduleOutbound`] as-is: `{"type": "...", "payload": {...}}`.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::ScheduleError;
use crate::logger::ScheduleLogger;
use crate::service::ScheduleService;
use crate::types::{
    double_option, is_guid, CreateScheduleInput, NewAgentConfig, ScheduleCadence, ScheduleRun,
    ScheduleSummary, ScheduleTarget, StoredSchedule, UpdateNewAgentConfig, UpdateScheduleInput,
};

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type")]
pub enum CreateTarget {
    #[serde(rename = "self")]
    SelfAgent {
        #[serde(rename = "agentId")]
        agent_id: String,
    },
    #[serde(rename = "agent")]
    Agent {
        #[serde(rename = "agentId")]
        agent_id: String,
    },
    #[serde(rename = "new-agent")]
    NewAgent { config: NewAgentConfig },
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateRequest {
    pub request_id: String,
    pub prompt: String,
    #[serde(default)]
    pub name: Option<String>,
    pub cadence: ScheduleCadence,
    pub target: CreateTarget,
    #[serde(default)]
    pub max_runs: Option<i64>,
    #[serde(default)]
    pub expires_at: Option<String>,
    #[serde(default)]
    pub run_on_create: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListRequest {
    pub request_id: String,
}

/// Shared by inspect/logs/pause/resume/delete/run-once.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScheduleIdRequest {
    pub request_id: String,
    pub schedule_id: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateRequest {
    pub request_id: String,
    pub schedule_id: String,
    #[serde(default, deserialize_with = "double_option")]
    pub name: Option<Option<String>>,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub cadence: Option<ScheduleCadence>,
    #[serde(default)]
    pub new_agent_config: Option<UpdateNewAgentConfig>,
    #[serde(default, deserialize_with = "double_option")]
    pub max_runs: Option<Option<i64>>,
    #[serde(default, deserialize_with = "double_option")]
    pub expires_at: Option<Option<String>>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type")]
pub enum ScheduleRequest {
    #[serde(rename = "schedule/create")]
    Create(CreateRequest),
    #[serde(rename = "schedule/list")]
    List(ListRequest),
    #[serde(rename = "schedule/inspect")]
    Inspect(ScheduleIdRequest),
    #[serde(rename = "schedule/logs")]
    Logs(ScheduleIdRequest),
    #[serde(rename = "schedule/pause")]
    Pause(ScheduleIdRequest),
    #[serde(rename = "schedule/resume")]
    Resume(ScheduleIdRequest),
    #[serde(rename = "schedule/delete")]
    Delete(ScheduleIdRequest),
    #[serde(rename = "schedule/run-once")]
    RunOnce(ScheduleIdRequest),
    #[serde(rename = "schedule/update")]
    Update(UpdateRequest),
}

fn non_empty_trimmed(value: &mut String, what: &str) -> Result<(), String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(format!("{what} must not be empty"));
    }
    *value = trimmed.to_string();
    Ok(())
}

fn nullable_non_empty(value: &mut Option<Option<String>>, what: &str) -> Result<(), String> {
    if let Some(Some(v)) = value {
        non_empty_trimmed(v, what)?;
    }
    Ok(())
}

impl ScheduleRequest {
    pub fn request_type(&self) -> &'static str {
        match self {
            ScheduleRequest::Create(_) => "schedule/create",
            ScheduleRequest::List(_) => "schedule/list",
            ScheduleRequest::Inspect(_) => "schedule/inspect",
            ScheduleRequest::Logs(_) => "schedule/logs",
            ScheduleRequest::Pause(_) => "schedule/pause",
            ScheduleRequest::Resume(_) => "schedule/resume",
            ScheduleRequest::Delete(_) => "schedule/delete",
            ScheduleRequest::RunOnce(_) => "schedule/run-once",
            ScheduleRequest::Update(_) => "schedule/update",
        }
    }

    pub fn request_id(&self) -> &str {
        match self {
            ScheduleRequest::Create(r) => &r.request_id,
            ScheduleRequest::List(r) => &r.request_id,
            ScheduleRequest::Inspect(r)
            | ScheduleRequest::Logs(r)
            | ScheduleRequest::Pause(r)
            | ScheduleRequest::Resume(r)
            | ScheduleRequest::Delete(r)
            | ScheduleRequest::RunOnce(r) => &r.request_id,
            ScheduleRequest::Update(r) => &r.request_id,
        }
    }

    /// Deserialize and apply the zod-level checks (trim, non-empty, positive, guid).
    pub fn from_value(value: Value) -> Result<ScheduleRequest, String> {
        let mut request: ScheduleRequest =
            serde_json::from_value(value).map_err(|e| e.to_string())?;
        request.validate()?;
        Ok(request)
    }

    pub fn validate(&mut self) -> Result<(), String> {
        match self {
            ScheduleRequest::Create(r) => {
                if r.prompt.is_empty() {
                    return Err("prompt must not be empty".into());
                }
                if r.max_runs.is_some_and(|m| m <= 0) {
                    return Err("maxRuns must be positive".into());
                }
                r.cadence.normalize()?;
                match &mut r.target {
                    CreateTarget::SelfAgent { agent_id } | CreateTarget::Agent { agent_id } => {
                        if !is_guid(agent_id) {
                            return Err("Invalid GUID".into());
                        }
                    }
                    CreateTarget::NewAgent { config } => config.normalize()?,
                }
                Ok(())
            }
            ScheduleRequest::Update(r) => {
                if r.prompt.as_deref() == Some("") {
                    return Err("prompt must not be empty".into());
                }
                if r.max_runs.flatten().is_some_and(|m| m <= 0) {
                    return Err("maxRuns must be positive".into());
                }
                if let Some(cadence) = &mut r.cadence {
                    cadence.normalize()?;
                }
                if let Some(cfg) = &mut r.new_agent_config {
                    if let Some(p) = &mut cfg.provider {
                        non_empty_trimmed(p, "provider")?;
                    }
                    if let Some(c) = &mut cfg.cwd {
                        non_empty_trimmed(c, "cwd")?;
                    }
                    nullable_non_empty(&mut cfg.model, "model")?;
                    nullable_non_empty(&mut cfg.mode_id, "modeId")?;
                    nullable_non_empty(&mut cfg.thinking_option_id, "thinkingOptionId")?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

// ----- responses -------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SummaryPayload {
    pub request_id: String,
    pub schedule: Option<ScheduleSummary>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListPayload {
    pub request_id: String,
    pub schedules: Vec<ScheduleSummary>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredPayload {
    pub request_id: String,
    pub schedule: Option<StoredSchedule>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LogsPayload {
    pub request_id: String,
    pub runs: Vec<ScheduleRun>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeletePayload {
    pub request_id: String,
    pub schedule_id: String,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcErrorPayload {
    pub request_id: String,
    pub request_type: String,
    pub error: String,
    pub code: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", content = "payload")]
pub enum ScheduleOutbound {
    #[serde(rename = "schedule/create/response")]
    Create(SummaryPayload),
    #[serde(rename = "schedule/list/response")]
    List(ListPayload),
    #[serde(rename = "schedule/inspect/response")]
    Inspect(StoredPayload),
    #[serde(rename = "schedule/logs/response")]
    Logs(LogsPayload),
    #[serde(rename = "schedule/pause/response")]
    Pause(SummaryPayload),
    #[serde(rename = "schedule/resume/response")]
    Resume(SummaryPayload),
    #[serde(rename = "schedule/delete/response")]
    Delete(DeletePayload),
    #[serde(rename = "schedule/run-once/response")]
    RunOnce(StoredPayload),
    #[serde(rename = "schedule/update/response")]
    Update(StoredPayload),
    #[serde(rename = "rpc_error")]
    RpcError(RpcErrorPayload),
}

// ----- session ---------------------------------------------------------------

pub struct ScheduleSession {
    service: Arc<ScheduleService>,
    logger: Arc<dyn ScheduleLogger>,
}

impl ScheduleSession {
    pub fn new(service: Arc<ScheduleService>, logger: Arc<dyn ScheduleLogger>) -> Self {
        ScheduleSession { service, logger }
    }

    fn rpc_error(&self, request: &ScheduleRequest, error: ScheduleError) -> ScheduleOutbound {
        self.logger.error(&format!(
            "Schedule request failed (requestType={}): {error}",
            request.request_type()
        ));
        ScheduleOutbound::RpcError(RpcErrorPayload {
            request_id: request.request_id().to_string(),
            request_type: request.request_type().to_string(),
            error: error.to_string(),
            code: "schedule_request_failed".to_string(),
        })
    }

    pub fn handle(&self, request: &ScheduleRequest) -> ScheduleOutbound {
        match self.dispatch(request) {
            Ok(outbound) => outbound,
            Err(error) => self.rpc_error(request, error),
        }
    }

    fn dispatch(&self, request: &ScheduleRequest) -> Result<ScheduleOutbound, ScheduleError> {
        let service = &self.service;
        Ok(match request {
            ScheduleRequest::Create(r) => {
                let target = match &r.target {
                    CreateTarget::SelfAgent { agent_id } | CreateTarget::Agent { agent_id } => {
                        ScheduleTarget::Agent {
                            agent_id: agent_id.clone(),
                        }
                    }
                    CreateTarget::NewAgent { config } => ScheduleTarget::NewAgent {
                        config: config.clone(),
                    },
                };
                let schedule = service.create(CreateScheduleInput {
                    name: r.name.clone(),
                    prompt: r.prompt.clone(),
                    cadence: r.cadence.clone(),
                    target,
                    max_runs: r.max_runs,
                    expires_at: r.expires_at.clone(),
                    run_on_create: r.run_on_create,
                })?;
                ScheduleOutbound::Create(SummaryPayload {
                    request_id: r.request_id.clone(),
                    schedule: Some((&schedule).into()),
                    error: None,
                })
            }
            ScheduleRequest::List(r) => {
                let schedules = service.list()?;
                ScheduleOutbound::List(ListPayload {
                    request_id: r.request_id.clone(),
                    schedules: schedules.iter().map(ScheduleSummary::from).collect(),
                    error: None,
                })
            }
            ScheduleRequest::Inspect(r) => ScheduleOutbound::Inspect(StoredPayload {
                request_id: r.request_id.clone(),
                schedule: Some(service.inspect(&r.schedule_id)?),
                error: None,
            }),
            ScheduleRequest::Logs(r) => ScheduleOutbound::Logs(LogsPayload {
                request_id: r.request_id.clone(),
                runs: service.logs(&r.schedule_id)?,
                error: None,
            }),
            ScheduleRequest::Pause(r) => {
                let schedule = service.pause(&r.schedule_id)?;
                ScheduleOutbound::Pause(SummaryPayload {
                    request_id: r.request_id.clone(),
                    schedule: Some((&schedule).into()),
                    error: None,
                })
            }
            ScheduleRequest::Resume(r) => {
                let schedule = service.resume(&r.schedule_id)?;
                ScheduleOutbound::Resume(SummaryPayload {
                    request_id: r.request_id.clone(),
                    schedule: Some((&schedule).into()),
                    error: None,
                })
            }
            ScheduleRequest::Delete(r) => {
                service.delete(&r.schedule_id)?;
                ScheduleOutbound::Delete(DeletePayload {
                    request_id: r.request_id.clone(),
                    schedule_id: r.schedule_id.clone(),
                    error: None,
                })
            }
            ScheduleRequest::RunOnce(r) => ScheduleOutbound::RunOnce(StoredPayload {
                request_id: r.request_id.clone(),
                schedule: Some(service.run_once(&r.schedule_id)?),
                error: None,
            }),
            ScheduleRequest::Update(r) => {
                let schedule = service.update(UpdateScheduleInput {
                    id: r.schedule_id.clone(),
                    name: r.name.clone(),
                    prompt: r.prompt.clone(),
                    cadence: r.cadence.clone(),
                    new_agent_config: r.new_agent_config.clone(),
                    max_runs: r.max_runs,
                    expires_at: r.expires_at.clone(),
                })?;
                ScheduleOutbound::Update(StoredPayload {
                    request_id: r.request_id.clone(),
                    schedule: Some(schedule),
                    error: None,
                })
            }
        })
    }
}
