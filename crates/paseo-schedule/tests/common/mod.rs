#![allow(dead_code)]

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use paseo_schedule::host::*;
use paseo_schedule::logger::{CollectingLogger, NoopLogger, ScheduleLogger};
use paseo_schedule::service::{Clock, ScheduleRunner};
use paseo_schedule::time::parse_js_date;
use paseo_schedule::types::*;
use paseo_schedule::{ScheduleError, ScheduleService, ScheduleServiceOptions};

pub fn ms(iso: &str) -> i64 {
    parse_js_date(iso).expect("valid date")
}

#[derive(Clone)]
pub struct TestClock(pub Arc<AtomicI64>);

impl TestClock {
    pub fn new(iso: &str) -> Self {
        TestClock(Arc::new(AtomicI64::new(ms(iso))))
    }
    pub fn set(&self, iso: &str) {
        self.0.store(ms(iso), Ordering::SeqCst);
    }
    pub fn clock(&self) -> Clock {
        let inner = self.0.clone();
        Arc::new(move || inner.load(Ordering::SeqCst))
    }
}

#[derive(Default)]
pub struct FakeHostState {
    pub agents: HashMap<String, AgentTargetState>,
    pub existing_outcome: AgentWaitOutcome,
    pub run_result: AgentRunResult,
    pub run_outcome: AgentWaitOutcome,
    pub initial_prompt_error: Option<String>,
    pub fail_archive: bool,
    pub fail_create_agent: Option<String>,
    pub delivered: Vec<(String, String)>,
    pub created: Vec<CreateScheduledAgent>,
    pub workspaces: Vec<(String, Isolation)>,
    pub archived: Vec<String>,
}

#[derive(Default)]
pub struct FakeHost {
    pub state: Mutex<FakeHostState>,
}

impl FakeHost {
    pub fn new() -> Arc<Self> {
        Arc::new(FakeHost::default())
    }
    pub fn set_agent(&self, id: &str, state: AgentTargetState) {
        self.state
            .lock()
            .unwrap()
            .agents
            .insert(id.to_string(), state);
    }
}

impl ScheduleAgentHost for FakeHost {
    fn agent_target_state(&self, agent_id: &str) -> Result<AgentTargetState, ScheduleError> {
        Ok(*self
            .state
            .lock()
            .unwrap()
            .agents
            .get(agent_id)
            .unwrap_or(&AgentTargetState::Missing))
    }

    fn run_existing_agent(
        &self,
        agent_id: &str,
        prompt: &str,
    ) -> Result<AgentWaitOutcome, ScheduleError> {
        let mut s = self.state.lock().unwrap();
        s.delivered.push((agent_id.to_string(), prompt.to_string()));
        Ok(s.existing_outcome.clone())
    }

    fn create_run_workspace(
        &self,
        cwd: &str,
        isolation: Isolation,
        _prompt: &str,
    ) -> Result<RunWorkspace, ScheduleError> {
        let mut s = self.state.lock().unwrap();
        s.workspaces.push((cwd.to_string(), isolation));
        let n = s.workspaces.len();
        Ok(RunWorkspace {
            workspace_id: format!("ws-{n}"),
            cwd: cwd.to_string(),
        })
    }

    fn create_agent(&self, request: &CreateScheduledAgent) -> Result<CreatedAgent, ScheduleError> {
        let mut s = self.state.lock().unwrap();
        if let Some(message) = &s.fail_create_agent {
            return Err(ScheduleError::msg(message.clone()));
        }
        s.created.push(request.clone());
        Ok(CreatedAgent {
            agent_id: "00000000-0000-0000-0000-0000000000aa".to_string(),
            initial_prompt_error: s.initial_prompt_error.clone(),
        })
    }

    fn run_agent(&self, _agent_id: &str, _prompt: &str) -> Result<AgentRunResult, ScheduleError> {
        Ok(self.state.lock().unwrap().run_result.clone())
    }

    fn wait_for_agent_event(&self, _agent_id: &str) -> Result<AgentWaitOutcome, ScheduleError> {
        Ok(self.state.lock().unwrap().run_outcome.clone())
    }

    fn archive_workspace(&self, workspace_id: &str) -> Result<(), ScheduleError> {
        let mut s = self.state.lock().unwrap();
        s.archived.push(workspace_id.to_string());
        if s.fail_archive {
            return Err(ScheduleError::msg("archive failed"));
        }
        Ok(())
    }
}

pub fn runner<F>(f: F) -> Option<ScheduleRunner>
where
    F: Fn(&StoredSchedule, &str) -> Result<ScheduleExecutionResult, ScheduleError>
        + Send
        + Sync
        + 'static,
{
    Some(Arc::new(f))
}

pub fn ok_runner() -> Option<ScheduleRunner> {
    runner(|_, _| {
        Ok(ScheduleExecutionResult {
            agent_id: None,
            output: Some("ok".into()),
        })
    })
}

pub fn service(
    home: &Path,
    clock: &TestClock,
    host: Arc<FakeHost>,
    runner: Option<ScheduleRunner>,
) -> Arc<ScheduleService> {
    service_with_logger(home, clock, host, runner, Arc::new(NoopLogger))
}

pub fn service_with_logger(
    home: &Path,
    clock: &TestClock,
    host: Arc<FakeHost>,
    runner: Option<ScheduleRunner>,
    logger: Arc<dyn ScheduleLogger>,
) -> Arc<ScheduleService> {
    Arc::new(ScheduleService::new(ScheduleServiceOptions {
        paseo_home: home.to_path_buf(),
        logger,
        host,
        now: Some(clock.clock()),
        runner,
    }))
}

pub fn collecting_logger() -> Arc<CollectingLogger> {
    Arc::new(CollectingLogger::default())
}

pub fn every(ms: u64) -> ScheduleCadence {
    ScheduleCadence::Every { every_ms: ms }
}

pub fn cron(expr: &str) -> ScheduleCadence {
    ScheduleCadence::Cron {
        expression: expr.into(),
        timezone: None,
    }
}

pub fn new_agent(cwd: &str) -> ScheduleTarget {
    ScheduleTarget::NewAgent {
        config: NewAgentConfig::new("claude", cwd),
    }
}

pub fn agent(id: &str) -> ScheduleTarget {
    ScheduleTarget::Agent {
        agent_id: id.to_string(),
    }
}

pub fn input(
    prompt: &str,
    cadence: ScheduleCadence,
    target: ScheduleTarget,
) -> CreateScheduleInput {
    CreateScheduleInput::new(prompt, cadence, target)
}

pub fn base_schedule(name: Option<&str>, target: ScheduleTarget) -> StoredSchedule {
    StoredSchedule {
        id: String::new(),
        name: name.map(str::to_string),
        prompt: "p".into(),
        cadence: every(60_000),
        target,
        status: ScheduleStatus::Active,
        created_at: "2026-01-01T00:00:00.000Z".into(),
        updated_at: "2026-01-01T00:00:00.000Z".into(),
        next_run_at: Some("2026-01-01T00:01:00.000Z".into()),
        last_run_at: None,
        paused_at: None,
        expires_at: None,
        max_runs: None,
        runs: vec![],
    }
}
