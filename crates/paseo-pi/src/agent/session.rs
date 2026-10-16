//! `AgentSession` over a running Pi / OMP process.

use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;

use serde_json::{json, Map};

use super::catalog::DEFAULT_THINKING_LEVEL;
use super::permission::{build_ui_response, ui_request_to_permission};
use super::turn::{TurnOutcome, TurnTracker};
use super::{
    AgentEvent, AgentSession, AgentSlashCommand, Capabilities, Mode, PermissionRequest,
    PermissionResponse, PersistenceHandle, PromptBlock, PromptInput, RuntimeInfo, SessionConfig,
    SlashCommandKind, TurnHandle,
};
use crate::error::{Error, Result};
use crate::flavor::Flavor;
use crate::protocol::{Event, ImageContent, Model, SessionState, SlashCommand, ThinkingLevel};
use crate::session::Session;

/// Commands Paseo handles itself on Pi (the TS adapter intercepts them).
const HANDLED_BUILTINS: [(&str, &str, &str); 2] = [
    (
        "compact",
        "Manually compact the session context",
        "[instructions]",
    ),
    (
        "autocompact",
        "Toggle automatic context compaction",
        "[on|off|toggle]",
    ),
];

struct Shared {
    flavor: Flavor,
    tracker: Mutex<TurnTracker>,
    pending_permissions: Mutex<HashMap<String, PermissionRequest>>,
    subscribers: Mutex<Vec<Sender<AgentEvent>>>,
    state: Mutex<SessionState>,
    config: Mutex<SessionConfig>,
    /// Request id of the prompt that started the active turn (OMP reports its fate later).
    active_prompt_id: Mutex<Option<String>>,
}

impl Shared {
    fn emit(&self, event: AgentEvent) {
        self.subscribers
            .lock()
            .unwrap()
            .retain(|tx| tx.send(event.clone()).is_ok());
    }

    fn emit_outcome(&self, turn_id: String, outcome: TurnOutcome) {
        self.emit(match outcome {
            TurnOutcome::Completed { final_text } => AgentEvent::TurnCompleted {
                turn_id,
                final_text,
            },
            TurnOutcome::Failed { error } => AgentEvent::TurnFailed { turn_id, error },
            TurnOutcome::Canceled { reason } => AgentEvent::TurnCanceled { turn_id, reason },
        });
    }

    fn handle_event(&self, event: Event) {
        let turn_id = self
            .tracker
            .lock()
            .unwrap()
            .active_turn_id()
            .map(str::to_owned);
        if let Event::ExtensionUiRequest(request) = &event {
            if request.is_dialog() {
                if let Some(permission) = ui_request_to_permission(self.flavor, request) {
                    self.pending_permissions
                        .lock()
                        .unwrap()
                        .insert(permission.id.clone(), permission.clone());
                    self.emit(AgentEvent::PermissionRequested {
                        turn_id: turn_id.clone(),
                        request: permission,
                    });
                }
            }
        }
        if let Event::ThinkingLevelChanged(change) = &event {
            self.state.lock().unwrap().thinking_level = Some(change.level.clone());
        }
        self.emit(AgentEvent::Provider {
            turn_id,
            event_type: event.tag().unwrap_or("unknown").to_string(),
            raw: event.to_value(),
        });
        if let Event::ProcessExit(exit) = &event {
            self.emit(AgentEvent::ProcessExited {
                error: exit.error.clone(),
            });
        }
        let finished = match &event {
            Event::PromptResult(result) => self.prompt_result_outcome(result),
            _ => self.tracker.lock().unwrap().on_event(&event),
        };
        if let Some((turn_id, outcome)) = finished {
            self.emit_outcome(turn_id, outcome);
        }
    }

    /// OMP can acknowledge a prompt and reject it afterwards, with no turn to follow.
    fn prompt_result_outcome(
        &self,
        result: &crate::protocol::events::PromptResult,
    ) -> Option<(String, TurnOutcome)> {
        if result.agent_invoked != Some(false) {
            return None;
        }
        let active = self.active_prompt_id.lock().unwrap().clone();
        if result.id.is_some() && active.is_some() && result.id != active {
            return None;
        }
        let mut tracker = self.tracker.lock().unwrap();
        match result.error_message() {
            Some(error) => tracker.fail_active(error),
            None => tracker.complete_without_agent(),
        }
    }
}

pub struct RpcAgentSession {
    session: Arc<Session>,
    shared: Arc<Shared>,
    capabilities: Capabilities,
}

impl RpcAgentSession {
    pub(crate) fn new(
        session: Session,
        config: SessionConfig,
        initial_state: SessionState,
        capabilities: Capabilities,
    ) -> Self {
        let flavor = session.flavor();
        let events = session.subscribe();
        let shared = Arc::new(Shared {
            flavor,
            tracker: Mutex::new(TurnTracker::new(flavor)),
            pending_permissions: Mutex::new(HashMap::new()),
            subscribers: Mutex::new(Vec::new()),
            state: Mutex::new(initial_state),
            config: Mutex::new(config),
            active_prompt_id: Mutex::new(None),
        });
        let pump = Arc::clone(&shared);
        thread::Builder::new()
            .name("paseo-pi-events".into())
            .spawn(move || {
                while let Ok(event) = events.recv() {
                    pump.handle_event(event);
                }
            })
            .expect("spawn event pump");
        Self {
            session: Arc::new(session),
            shared,
            capabilities,
        }
    }

    /// The raw protocol session underneath, for provider-specific calls.
    pub fn raw(&self) -> &Session {
        &self.session
    }

    fn refresh_state(&self) -> Result<SessionState> {
        let state = self.session.get_state()?;
        *self.shared.state.lock().unwrap() = state.clone();
        Ok(state)
    }

    fn flavor(&self) -> Flavor {
        self.shared.flavor
    }

    fn prompt_payload(&self, prompt: &PromptInput) -> (String, Vec<ImageContent>) {
        let blocks = match prompt {
            PromptInput::Text(text) => return (text.clone(), Vec::new()),
            PromptInput::Blocks(blocks) => blocks,
        };
        let supports_images = self
            .shared
            .state
            .lock()
            .unwrap()
            .model
            .as_ref()
            .is_none_or(model_accepts_images);
        let mut text = Vec::new();
        let mut images = Vec::new();
        for block in blocks {
            match block {
                PromptBlock::Text(t) => text.push(t.clone()),
                PromptBlock::Image { data, mime_type } if supports_images => {
                    images.push(ImageContent::new(data.clone(), mime_type.clone()));
                }
                PromptBlock::Image { mime_type, .. } => text.push(format!(
                    "[Image attachment omitted: the selected model does not accept {mime_type} input]"
                )),
            }
        }
        (text.join("\n\n"), images)
    }
}

fn model_accepts_images(model: &Model) -> bool {
    model
        .input
        .as_ref()
        .is_none_or(|input| input.iter().any(|i| i == "image"))
}

fn map_commands(flavor: Flavor, commands: Vec<SlashCommand>) -> Vec<AgentSlashCommand> {
    let mut mapped: Vec<AgentSlashCommand> = Vec::new();
    if flavor == Flavor::Pi {
        for (name, description, hint) in HANDLED_BUILTINS {
            mapped.push(AgentSlashCommand {
                name: name.into(),
                description: description.into(),
                argument_hint: hint.into(),
                kind: SlashCommandKind::Command,
            });
        }
    }
    for command in commands {
        let kind = if command.source.as_deref() == Some("skill") {
            SlashCommandKind::Skill
        } else {
            SlashCommandKind::Command
        };
        let known_hint = mapped
            .iter()
            .find(|m| m.name == command.name)
            .map(|m| m.argument_hint.clone());
        let entry = AgentSlashCommand {
            description: command
                .description
                .clone()
                .or_else(|| command.source.clone())
                .unwrap_or_default(),
            argument_hint: known_hint
                .or_else(|| command.input_hint().map(str::to_owned))
                .unwrap_or_default(),
            kind,
            name: command.name,
        };
        match mapped.iter_mut().find(|m| m.name == entry.name) {
            Some(slot) => *slot = entry,
            None => mapped.push(entry),
        }
    }
    mapped
}

pub(crate) fn commands_for(flavor: Flavor, session: &Session) -> Result<Vec<AgentSlashCommand>> {
    Ok(map_commands(flavor, session.get_commands()?))
}

pub(crate) fn parse_model_reference(model_id: &str) -> Option<(String, String)> {
    for separator in ['/', ':'] {
        if let Some((provider, id)) = model_id.split_once(separator) {
            if !provider.is_empty() && !id.is_empty() {
                return Some((provider.to_string(), id.to_string()));
            }
        }
    }
    None
}

impl AgentSession for RpcAgentSession {
    fn provider(&self) -> &str {
        self.flavor().provider_id()
    }

    fn capabilities(&self) -> Capabilities {
        self.capabilities
    }

    fn id(&self) -> Option<String> {
        Some(self.shared.state.lock().unwrap().session_id.clone())
    }

    fn subscribe(&self) -> Receiver<AgentEvent> {
        let (tx, rx) = mpsc::channel();
        self.shared.subscribers.lock().unwrap().push(tx);
        rx
    }

    fn start_turn(&self, prompt: PromptInput) -> Result<TurnHandle> {
        let (text, images) = self.prompt_payload(&prompt);
        let (turn_id, superseded) = self.shared.tracker.lock().unwrap().begin_superseding();
        if let Some((old_id, outcome)) = superseded {
            self.shared.emit_outcome(old_id, outcome);
        }
        self.shared.emit(AgentEvent::TurnStarted {
            turn_id: turn_id.clone(),
        });
        *self.shared.active_prompt_id.lock().unwrap() = None;
        match self.session.prompt(&text, &images) {
            Ok(ack) => {
                *self.shared.active_prompt_id.lock().unwrap() = ack.request_id.clone();
                if ack.agent_invoked == Some(false) {
                    let finished = self.shared.tracker.lock().unwrap().complete_without_agent();
                    if let Some((id, outcome)) = finished {
                        self.shared.emit_outcome(id, outcome);
                    }
                }
                Ok(TurnHandle { turn_id })
            }
            Err(error) => {
                let finished = self
                    .shared
                    .tracker
                    .lock()
                    .unwrap()
                    .fail_active(&error.to_string());
                if let Some((id, outcome)) = finished {
                    self.shared.emit_outcome(id, outcome);
                }
                Err(error)
            }
        }
    }

    fn interrupt(&self) -> Result<()> {
        let interrupted_turn = {
            let mut tracker = self.shared.tracker.lock().unwrap();
            tracker.mark_interrupting();
            tracker.active_turn_id().map(str::to_owned)
        };
        // A dialog the agent waits on would keep the abort from landing.
        let pending: Vec<_> = self
            .shared
            .pending_permissions
            .lock()
            .unwrap()
            .drain()
            .map(|(id, _)| id)
            .collect();
        for id in pending {
            let _ = self.session.cancel_extension_ui_request(&id);
        }
        // COMPAT(clearQueueFallback): pi before 0.84.4 has no `clear_queue`.
        match self.session.clear_queue() {
            Ok(()) => {}
            Err(Error::Rpc { message, .. }) if message == "Unknown command: clear_queue" => {}
            Err(error) => {
                let finished = self
                    .shared
                    .tracker
                    .lock()
                    .unwrap()
                    .fail_active(&error.to_string());
                if let Some((id, outcome)) = finished {
                    self.shared.emit_outcome(id, outcome);
                }
                return Err(error);
            }
        }
        self.session.abort()?;
        // The provider may never send agent_end for an aborted turn; end it here as TS does.
        // If agent_end already finished it (or a new turn began), nothing is active under this id.
        let finished = {
            let mut tracker = self.shared.tracker.lock().unwrap();
            if tracker.active_turn_id().map(str::to_owned) == interrupted_turn {
                tracker.cancel_active("interrupted")
            } else {
                None
            }
        };
        if let Some((id, outcome)) = finished {
            self.shared.emit_outcome(id, outcome);
        }
        Ok(())
    }

    fn pending_permissions(&self) -> Vec<PermissionRequest> {
        self.shared
            .pending_permissions
            .lock()
            .unwrap()
            .values()
            .cloned()
            .collect()
    }

    fn respond_to_permission(&self, request_id: &str, response: PermissionResponse) -> Result<()> {
        let request = self
            .shared
            .pending_permissions
            .lock()
            .unwrap()
            .remove(request_id)
            .ok_or_else(|| {
                Error::Protocol(format!(
                    "No pending permission request with id '{request_id}'"
                ))
            })?;
        let body = build_ui_response(&request, &response);
        self.session
            .respond_to_extension_ui_request(&request.id, &body)?;
        let turn_id = self
            .shared
            .tracker
            .lock()
            .unwrap()
            .active_turn_id()
            .map(str::to_owned);
        self.shared.emit(AgentEvent::PermissionResolved {
            turn_id,
            request_id: request_id.to_string(),
            response,
        });
        Ok(())
    }

    fn list_commands(&self) -> Result<Vec<AgentSlashCommand>> {
        commands_for(self.flavor(), &self.session)
    }

    fn available_modes(&self) -> Result<Vec<Mode>> {
        Ok(Vec::new())
    }

    fn current_mode(&self) -> Option<String> {
        self.shared.config.lock().unwrap().mode_id.clone()
    }

    fn set_mode(&self, _mode_id: &str) -> Result<()> {
        Err(Error::Unsupported("Pi does not expose selectable modes"))
    }

    fn set_model(&self, model_id: Option<&str>) -> Result<()> {
        let Some(model_id) = model_id.filter(|m| !m.is_empty()) else {
            return Ok(());
        };
        let Some((provider, id)) = parse_model_reference(model_id) else {
            return Err(Error::Protocol(format!(
                "{} model id must include a provider: {model_id}",
                self.flavor().label()
            )));
        };
        let model = self.session.set_model(&provider, &id)?;
        self.refresh_state()?;
        self.shared.config.lock().unwrap().model = Some(model.qualified_id());
        Ok(())
    }

    fn set_thinking_option(&self, thinking_option_id: Option<&str>) -> Result<()> {
        let level = thinking_option_id
            .and_then(ThinkingLevel::normalize)
            .unwrap_or_else(|| ThinkingLevel(DEFAULT_THINKING_LEVEL.to_string()));
        self.session.set_thinking_level(level.as_str())?;
        self.refresh_state()?;
        Ok(())
    }

    fn runtime_info(&self) -> Result<RuntimeInfo> {
        let state = self.refresh_state()?;
        Ok(RuntimeInfo {
            provider: self.provider().to_string(),
            session_id: Some(state.session_id.clone()),
            model: state.model.as_ref().map(Model::qualified_id),
            thinking_option_id: state.thinking_level.map(|l| l.0),
            mode_id: self.current_mode(),
        })
    }

    fn describe_persistence(&self) -> Option<PersistenceHandle> {
        let state = self.shared.state.lock().unwrap().clone();
        let config = self.shared.config.lock().unwrap().clone();
        let mut metadata = Map::new();
        metadata.insert("cwd".into(), json!(config.cwd.to_string_lossy()));
        if let Some(model) = &config.model {
            metadata.insert("model".into(), json!(model));
        }
        if let Some(level) = state
            .thinking_level
            .as_ref()
            .map(|l| l.0.clone())
            .or(config.thinking_option_id)
        {
            metadata.insert("thinkingOptionId".into(), json!(level));
        }
        if let Some(mode) = &config.mode_id {
            metadata.insert("modeId".into(), json!(mode));
        }
        Some(PersistenceHandle {
            provider: self.provider().to_string(),
            session_id: state.session_id,
            native_handle: state.session_file,
            metadata,
        })
    }

    fn close(&self) -> Result<()> {
        self.session.close();
        Ok(())
    }
}
