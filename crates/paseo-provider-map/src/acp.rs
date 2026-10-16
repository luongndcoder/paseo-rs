//! Acp mapper (placeholder: returns no events until WP10 implements it).

use paseo_agent_types::AgentStreamEvent;
use serde_json::Value;

use crate::{MapEnv, ProviderMapper};

#[derive(Debug, Default)]
pub struct AcpMapper;

impl ProviderMapper for AcpMapper {
    fn provider(&self) -> &'static str {
        "acp"
    }
    fn feed(&mut self, _raw: &Value, _env: &mut dyn MapEnv) -> Vec<AgentStreamEvent> {
        Vec::new()
    }
}
