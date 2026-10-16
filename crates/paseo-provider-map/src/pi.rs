//! Pi mapper (placeholder: returns no events until WP10 implements it).

use paseo_agent_types::AgentStreamEvent;
use serde_json::Value;

use crate::{MapEnv, ProviderMapper};

#[derive(Debug, Default)]
pub struct PiMapper;

impl ProviderMapper for PiMapper {
    fn provider(&self) -> &'static str {
        "pi"
    }
    fn feed(&mut self, _raw: &Value, _env: &mut dyn MapEnv) -> Vec<AgentStreamEvent> {
        Vec::new()
    }
}
