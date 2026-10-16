//! `AgentUsage`.

use crate::json::*;
use serde_json::Value;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct AgentUsage {
    pub input_tokens: Option<Num>,
    pub cached_input_tokens: Option<Num>,
    pub output_tokens: Option<Num>,
    pub total_cost_usd: Option<Num>,
    pub context_window_max_tokens: Option<Num>,
    pub context_window_used_tokens: Option<Num>,
}

impl AgentUsage {
    pub fn to_value(&self) -> Value {
        let mut m = Map::new();
        put_opt_num(&mut m, "inputTokens", &self.input_tokens);
        put_opt_num(&mut m, "cachedInputTokens", &self.cached_input_tokens);
        put_opt_num(&mut m, "outputTokens", &self.output_tokens);
        put_opt_num(&mut m, "totalCostUsd", &self.total_cost_usd);
        put_opt_num(
            &mut m,
            "contextWindowMaxTokens",
            &self.context_window_max_tokens,
        );
        put_opt_num(
            &mut m,
            "contextWindowUsedTokens",
            &self.context_window_used_tokens,
        );
        Value::Object(m)
    }

    pub fn from_value(v: &Value) -> Result<Self> {
        let o = Obj::new(v, "usage")?;
        Ok(AgentUsage {
            input_tokens: o.opt_num("inputTokens")?,
            cached_input_tokens: o.opt_num("cachedInputTokens")?,
            output_tokens: o.opt_num("outputTokens")?,
            total_cost_usd: o.opt_num("totalCostUsd")?,
            context_window_max_tokens: o.opt_num("contextWindowMaxTokens")?,
            context_window_used_tokens: o.opt_num("contextWindowUsedTokens")?,
        })
    }
}

impl_serde_via_value!(AgentUsage);
