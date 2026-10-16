//! Opencode mapper (placeholder: returns no events until WP10 implements it).

use paseo_agent_types::AgentStreamEvent;
use serde_json::Value;

use crate::{MapEnv, ProviderMapper};

#[derive(Debug, Default)]
pub struct OpencodeMapper;

impl ProviderMapper for OpencodeMapper {
    fn provider(&self) -> &'static str {
        "opencode"
    }
    fn feed(&mut self, _raw: &Value, _env: &mut dyn MapEnv) -> Vec<AgentStreamEvent> {
        Vec::new()
    }
}
