//! Raw provider events -> `AgentStreamEvent` mappers (spec WP10).
//!
//! Mappers are stateful (live and history share one state machine), pure with respect to the
//! outside world: time and ids come from an injected [`MapEnv`] so goldens are deterministic.

use paseo_agent_types::AgentStreamEvent;
use serde_json::Value;

pub mod acp;
pub mod claude;
pub mod codex;
pub mod opencode;
pub mod pi;
pub mod testing;

/// Injectable clock and uuid source.
pub trait MapEnv {
    /// ISO-8601 UTC timestamp (what TS `new Date().toISOString()` yields).
    fn now_iso(&mut self) -> String;
    /// A fresh uuid string (what TS `crypto.randomUUID()` yields).
    fn uuid(&mut self) -> String;
}

/// Deterministic env matching the fixture capture: counter clock, counter uuid
/// (`00000000-0000-4000-8000-%012x`, starting at 1).
#[derive(Debug, Default)]
pub struct CounterEnv {
    clock: u64,
    uuids: u64,
}

impl CounterEnv {
    pub fn new() -> Self {
        Self::default()
    }
}

impl MapEnv for CounterEnv {
    fn now_iso(&mut self) -> String {
        let ms = self.clock;
        self.clock += 1;
        let (s, ms) = (ms / 1000, ms % 1000);
        format!("1970-01-01T00:{:02}:{:02}.{ms:03}Z", (s / 60) % 60, s % 60)
    }
    fn uuid(&mut self) -> String {
        self.uuids += 1;
        format!("00000000-0000-4000-8000-{:012x}", self.uuids)
    }
}

/// One provider's mapper: feed raw events (one JSON value per provider line/notification) in
/// arrival order, get the stream events they produce.
pub trait ProviderMapper {
    /// Provider id (`pi`, `acp`, `opencode`, `codex`, `claude`).
    fn provider(&self) -> &'static str;
    /// Map one raw event. May buffer state and return nothing.
    fn feed(&mut self, raw: &Value, env: &mut dyn MapEnv) -> Vec<AgentStreamEvent>;
    /// End of input (turn/process end): flush buffered state.
    fn finish(&mut self, _env: &mut dyn MapEnv) -> Vec<AgentStreamEvent> {
        Vec::new()
    }
}

/// Mapper for a provider id, or `None` when unknown.
pub fn mapper_for(provider: &str) -> Option<Box<dyn ProviderMapper>> {
    Some(match provider {
        "pi" => Box::new(pi::PiMapper),
        "acp" => Box::new(acp::AcpMapper::new()),
        "opencode" => Box::new(opencode::OpencodeMapper::new()),
        "codex" => Box::new(codex::CodexMapper::new()),
        "claude" => Box::new(claude::ClaudeMapper::new()),
        _ => return None,
    })
}
