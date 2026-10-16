//! Claude mapper (placeholder: returns no events until WP10 implements it).

use paseo_agent_types::AgentStreamEvent;
use serde_json::Value;

use crate::{MapEnv, ProviderMapper};

#[derive(Debug, Default)]
pub struct ClaudeMapper;

impl ProviderMapper for ClaudeMapper {
    fn provider(&self) -> &'static str {
        "claude"
    }
    fn feed(&mut self, _raw: &Value, _env: &mut dyn MapEnv) -> Vec<AgentStreamEvent> {
        Vec::new()
    }
}
