//! The provider-specific raw protocol layer: one running Pi / OMP process.
//!
//! This is the Rust twin of `PiRuntimeSession` / `OmpRuntimeSession`. Events are exposed as
//! parsed [`Event`]s; nothing is mapped to Paseo timeline items here.

use std::collections::BTreeMap;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

use crate::error::{Error, Result};
use crate::flavor::Flavor;
use crate::frame::supports_protocol_v2;
use crate::launch::Options;
use crate::protocol::{
    Command, Event, ImageContent, Message, Model, PromptAck, SessionState, SessionStats,
    SlashCommand, UiResponse,
};
use crate::rpc::{RpcClient, Timeout};
use crate::transport::{ProcessTransport, Transport};

pub struct Session {
    flavor: Flavor,
    rpc: RpcClient,
    environment: BTreeMap<String, String>,
    request_timeout: Duration,
    /// OMP: entry id the conversation was last branched at.
    active_branch_entry_id: Mutex<Option<String>>,
}

impl Session {
    /// Spawns the provider and, for OMP, waits for `ready` and negotiates protocol v2.
    pub fn start(options: &Options) -> Result<Self> {
        let launch = options.build_launch();
        let transport = ProcessTransport::spawn(&launch)?;
        Self::from_transport(options, Arc::new(transport))
    }

    /// Same as [`Session::start`] over any transport (tests, replay).
    pub fn from_transport(options: &Options, transport: Arc<dyn Transport>) -> Result<Self> {
        let rpc = RpcClient::new(
            transport,
            options.flavor.diagnostic_name(),
            options.request_timeout,
        );
        let session = Self {
            flavor: options.flavor,
            rpc,
            environment: options.env.clone(),
            request_timeout: options.request_timeout,
            active_branch_entry_id: Mutex::new(None),
        };
        if options.flavor.waits_for_ready() {
            if let Err(error) = session.establish_protocol(options.ready_timeout) {
                session.rpc.close();
                return Err(error);
            }
        }
        Ok(session)
    }

    fn establish_protocol(&self, ready_timeout: Duration) -> Result<()> {
        let ready = self.rpc.wait_ready(ready_timeout)?;
        if !supports_protocol_v2(&ready) {
            return Ok(());
        }
        let response = self.request(&Command::NegotiateProtocol {
            protocol_version: 2,
        })?;
        if response.get("protocolVersion").and_then(Value::as_u64) != Some(2) {
            return Err(Error::Protocol(format!(
                "{} did not accept RPC protocol v2",
                self.flavor.label()
            )));
        }
        Ok(())
    }

    pub fn flavor(&self) -> Flavor {
        self.flavor
    }

    /// Environment the process was launched with (overlay only, not the inherited one).
    pub fn environment(&self) -> &BTreeMap<String, String> {
        &self.environment
    }

    pub fn rpc(&self) -> &RpcClient {
        &self.rpc
    }

    /// New event stream. Events sent before this call are not replayed.
    pub fn subscribe(&self) -> Receiver<Event> {
        self.rpc.subscribe()
    }

    pub fn request(&self, command: &Command) -> Result<Value> {
        self.rpc.request(command, Timeout::Default)
    }

    pub fn request_with(&self, command: &Command, timeout: Timeout) -> Result<Value> {
        self.rpc.request(command, timeout)
    }

    /// Send any command this crate has no typed variant for.
    pub fn request_raw(&self, command: Value, timeout: Timeout) -> Result<Value> {
        self.rpc.request_raw(command, timeout)
    }

    /// Write a frame without waiting for a response.
    pub fn send_raw_frame(&self, frame: &Value) -> Result<()> {
        self.rpc.send(frame)
    }

    pub fn default_request_timeout(&self) -> Duration {
        self.request_timeout
    }

    // ---- prompting ----------------------------------------------------------------------

    pub fn prompt(&self, message: &str, images: &[ImageContent]) -> Result<PromptAck> {
        let pending = self.rpc.start_request(
            &Command::Prompt {
                message: message.to_string(),
                images: non_empty(images),
            },
            Timeout::Default,
        )?;
        let request_id = pending.id().to_string();
        let data = pending.wait()?;
        Ok(PromptAck {
            request_id: Some(request_id),
            agent_invoked: data.get("agentInvoked").and_then(Value::as_bool),
        })
    }

    pub fn steer(&self, message: &str, images: &[ImageContent]) -> Result<()> {
        self.request(&Command::Steer {
            message: message.to_string(),
            images: non_empty(images),
        })
        .map(drop)
    }

    /// OMP only: queue a message for after the current run. No response is sent.
    pub fn follow_up(&self, message: &str, images: &[ImageContent]) -> Result<()> {
        self.require_omp("follow_up")?;
        let mut frame = json!({"type": "follow_up", "message": message});
        if !images.is_empty() {
            frame["images"] = serde_json::to_value(images)?;
        }
        self.rpc.send(&frame)
    }

    pub fn clear_queue(&self) -> Result<()> {
        self.rpc
            .request_stop_work(&Command::ClearQueue, Timeout::Default)
    }

    pub fn abort(&self) -> Result<()> {
        self.rpc
            .request_stop_work(&Command::Abort, Timeout::Default)
    }

    /// Blocks until compaction is durable; no elapsed-time failure.
    pub fn compact(&self, custom_instructions: Option<&str>) -> Result<()> {
        self.request_with(
            &Command::Compact {
                custom_instructions: custom_instructions.map(str::to_owned),
            },
            Timeout::Never,
        )
        .map(drop)
    }

    pub fn set_auto_compaction(&self, enabled: bool) -> Result<()> {
        self.request(&Command::SetAutoCompaction { enabled })
            .map(drop)
    }

    // ---- state --------------------------------------------------------------------------

    pub fn get_state(&self) -> Result<SessionState> {
        Ok(serde_json::from_value(self.request(&Command::GetState)?)?)
    }

    pub fn get_messages(&self) -> Result<Vec<Message>> {
        let data = self.request(&Command::GetMessages)?;
        match data.get("messages") {
            Some(messages) => Ok(serde_json::from_value(messages.clone())?),
            None => Ok(Vec::new()),
        }
    }

    pub fn get_available_models(&self, timeout: Timeout) -> Result<Vec<Model>> {
        let data = self.request_with(&Command::GetAvailableModels, timeout)?;
        match data.get("models") {
            Some(models) => Ok(serde_json::from_value(models.clone())?),
            None => Ok(Vec::new()),
        }
    }

    pub fn set_model(&self, provider: &str, model_id: &str) -> Result<Model> {
        let data = self.request(&Command::SetModel {
            provider: provider.to_string(),
            model_id: model_id.to_string(),
        })?;
        Ok(serde_json::from_value(data)?)
    }

    pub fn set_thinking_level(&self, level: &str) -> Result<()> {
        self.request(&Command::SetThinkingLevel {
            level: level.to_string(),
        })
        .map(drop)
    }

    /// COMPAT(getStateFallback): older binaries lack `get_session_stats`; fall back to the
    /// context usage `get_state` reports. Mirrors the TS runtime.
    pub fn get_session_stats(&self) -> Result<SessionStats> {
        let stats: Option<SessionStats> = self
            .request(&Command::GetSessionStats)
            .ok()
            .and_then(|v| serde_json::from_value(v).ok());
        let empty = stats
            .as_ref()
            .is_none_or(|s| s.tokens.is_none() && s.cost.is_none() && s.context_usage.is_none());
        if empty {
            if let Ok(state) = self.get_state() {
                if let Some(ctx) = state.context_usage {
                    return Ok(SessionStats {
                        context_usage: Some(crate::protocol::ContextUsage {
                            tokens: ctx.tokens,
                            context_window: ctx.context_window,
                            percent: None,
                            extra: Default::default(),
                        }),
                        ..SessionStats::default()
                    });
                }
            }
        }
        Ok(stats.unwrap_or_default())
    }

    pub fn get_commands(&self) -> Result<Vec<SlashCommand>> {
        let command = match self.flavor {
            Flavor::Pi => Command::GetCommands,
            Flavor::Omp => Command::GetAvailableCommands,
        };
        let data = self.request(&command)?;
        match data.get("commands") {
            Some(commands) => Ok(serde_json::from_value(commands.clone())?),
            None => Ok(Vec::new()),
        }
    }

    // ---- extension UI -------------------------------------------------------------------

    pub fn respond_to_extension_ui_request(&self, id: &str, response: &UiResponse) -> Result<()> {
        let mut frame = serde_json::to_value(response)?;
        let obj = frame.as_object_mut().expect("UiResponse is an object");
        obj.insert("type".into(), "extension_ui_response".into());
        obj.insert("id".into(), id.into());
        self.rpc.send(&frame)
    }

    pub fn cancel_extension_ui_request(&self, id: &str) -> Result<()> {
        self.respond_to_extension_ui_request(id, &UiResponse::cancelled())
    }

    // ---- OMP only -----------------------------------------------------------------------

    fn require_omp(&self, what: &'static str) -> Result<()> {
        match self.flavor {
            Flavor::Omp => Ok(()),
            Flavor::Pi => Err(Error::Unsupported(what)),
        }
    }

    pub fn set_fast_mode(&self, enabled: bool) -> Result<(bool, bool)> {
        self.require_omp("set_fast_mode")?;
        let data = self.request(&Command::SetFastMode { enabled })?;
        let flag = |k: &str| data.get(k).and_then(Value::as_bool);
        match (flag("enabled"), flag("active")) {
            (Some(e), Some(a)) => Ok((e, a)),
            _ => Err(Error::Protocol(
                "set_fast_mode result lacks enabled/active".into(),
            )),
        }
    }

    /// `level` is `off | progress | events`.
    pub fn set_subagent_subscription(&self, level: &str) -> Result<()> {
        self.require_omp("set_subagent_subscription")?;
        self.request(&Command::SetSubagentSubscription {
            level: level.to_string(),
        })
        .map(drop)
    }

    /// Registers host-implemented tools; returns the tool names OMP accepted.
    pub fn set_host_tools(&self, tools: Vec<Value>) -> Result<Vec<String>> {
        self.require_omp("set_host_tools")?;
        let data = self.request(&Command::SetHostTools { tools })?;
        Ok(data
            .get("toolNames")
            .and_then(Value::as_array)
            .map(|names| {
                names
                    .iter()
                    .filter_map(|n| n.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Answer a `host_tool_call`. `result` is `{content:[...], details?, isError?}`.
    pub fn send_host_tool_result(
        &self,
        id: &str,
        result: Value,
        is_error: Option<bool>,
    ) -> Result<()> {
        self.require_omp("host_tool_result")?;
        let mut frame = json!({"type": "host_tool_result", "id": id, "result": result});
        if let Some(is_error) = is_error {
            frame["isError"] = is_error.into();
        }
        self.rpc.send(&frame)
    }

    pub fn send_host_tool_update(&self, id: &str, partial_result: Value) -> Result<()> {
        self.require_omp("host_tool_update")?;
        self.rpc
            .send(&json!({"type": "host_tool_update", "id": id, "partialResult": partial_result}))
    }

    /// Rewinds to `entry_id`; returns the prompt text to restore.
    pub fn branch(&self, entry_id: &str) -> Result<String> {
        self.require_omp("branch")?;
        let data = self.request(&Command::Branch {
            entry_id: entry_id.to_string(),
        })?;
        if data.get("cancelled").and_then(Value::as_bool) == Some(true) {
            return Err(Error::Rpc {
                command: "branch".into(),
                message: "OMP branch was cancelled".into(),
            });
        }
        let text = data
            .get("text")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                Error::Protocol("OMP branch response did not include restored prompt text".into())
            })?
            .to_string();
        *self.active_branch_entry_id.lock().unwrap() = Some(entry_id.to_string());
        Ok(text)
    }

    pub fn active_branch_entry_id(&self) -> Option<String> {
        self.active_branch_entry_id.lock().unwrap().clone()
    }

    /// `(entryId, text)` for every user message that can be branched from.
    pub fn get_branch_messages(&self) -> Result<Vec<(String, String)>> {
        self.require_omp("get_branch_messages")?;
        let data = self.request(&Command::GetBranchMessages)?;
        Ok(data
            .get("messages")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|m| {
                        Some((
                            m.get("entryId")?.as_str()?.to_string(),
                            m.get("text")?.as_str()?.to_string(),
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    pub fn handoff(&self, custom_instructions: Option<&str>) -> Result<()> {
        self.require_omp("handoff")?;
        self.request(&Command::Handoff {
            custom_instructions: custom_instructions.map(str::to_owned),
        })
        .map(drop)
    }

    pub fn close(&self) {
        self.rpc.close();
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.rpc.close();
    }
}

fn non_empty(images: &[ImageContent]) -> Option<Vec<ImageContent>> {
    (!images.is_empty()).then(|| images.to_vec())
}
