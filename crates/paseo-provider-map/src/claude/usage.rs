//! `ClaudeContextUsageState` (`claude/agent.ts`): stream usage and result usage conversion.

use paseo_tool_detail::num_from_f64;
use serde_json::{Map, Value};

use super::text::{read_trimmed, Obj};

#[derive(Debug, Clone, Copy)]
struct StreamInput {
    input: f64,
    cache_creation: f64,
    cache_read: f64,
}

fn read_count(usage: &Obj, key: &str) -> Option<f64> {
    let f = usage.get(key)?.as_f64()?;
    (f.is_finite() && f >= 0.0).then_some(f)
}

fn finite(usage: &Obj, key: &str) -> f64 {
    usage
        .get(key)
        .and_then(Value::as_f64)
        .filter(|f| f.is_finite())
        .unwrap_or(0.0)
}

fn usage_token_total(usage: &Obj) -> Option<f64> {
    let total = finite(usage, "input_tokens")
        + finite(usage, "cache_creation_input_tokens")
        + finite(usage, "cache_read_input_tokens")
        + finite(usage, "output_tokens");
    (total > 0.0).then_some(total)
}

fn active_usage_tokens(usage: &Value) -> Option<f64> {
    let iterations = usage.get("iterations")?.as_array()?;
    let last = iterations.iter().rev().find_map(Value::as_object)?;
    usage_token_total(last)
}

fn num(f: f64) -> Value {
    Value::Number(num_from_f64(f))
}

#[derive(Debug, Default)]
pub struct ContextUsageState {
    context_window_max: Option<f64>,
    stream_input: Option<StreamInput>,
    stream_output: Option<f64>,
    compacted_used: Option<f64>,
    completed_result_turns: u32,
}

impl ContextUsageState {
    pub fn begin_turn(&mut self) {
        self.stream_input = None;
        self.stream_output = None;
        self.compacted_used = None;
    }

    pub fn set_initial_context_window_max(&mut self, max: Option<u64>) {
        self.context_window_max = max.map(|m| m as f64);
    }

    fn record_model_usage(&mut self, model_usage: Option<&Value>, model_id: Option<&str>) {
        let window = model_id
            .and_then(|id| model_usage?.get(id)?.get("contextWindow")?.as_f64())
            .filter(|w| w.is_finite() && *w > 0.0);
        if let Some(w) = window {
            self.context_window_max = Some(w);
        }
    }

    fn stream_used_tokens(&self) -> Option<f64> {
        let input = self.stream_input?;
        let out = self.stream_output?;
        let used = input.input + input.cache_creation + input.cache_read + out;
        (used > 0.0).then_some(used)
    }

    fn usage_updated(&self, used: f64) -> Value {
        let mut usage = Map::new();
        usage.insert("contextWindowUsedTokens".into(), num(used));
        if let Some(max) = self.context_window_max {
            usage.insert("contextWindowMaxTokens".into(), num(max));
        }
        serde_json::json!({"type": "usage_updated", "provider": "claude", "usage": Value::Object(usage)})
    }

    /// `buildStreamUsageEvent`.
    pub fn build_stream_usage_event(&mut self, event: &Value) -> Option<Value> {
        let o = event.as_object()?;
        match read_trimmed(o.get("type")).as_deref() {
            Some("message_start") => {
                let usage = o.get("message")?.get("usage")?.as_object()?;
                let input = read_count(usage, "input_tokens")?;
                self.stream_input = Some(StreamInput {
                    input,
                    cache_creation: read_count(usage, "cache_creation_input_tokens").unwrap_or(0.0),
                    cache_read: read_count(usage, "cache_read_input_tokens").unwrap_or(0.0),
                });
                self.stream_output = Some(0.0);
            }
            Some("message_delta") => {
                let usage = o.get("usage").and_then(Value::as_object);
                let out = usage.and_then(|u| read_count(u, "output_tokens"))?;
                self.stream_output = Some(out);
                if let (Some(current), Some(usage)) = (self.stream_input, usage) {
                    self.stream_input = Some(StreamInput {
                        input: read_count(usage, "input_tokens").unwrap_or(current.input),
                        cache_creation: read_count(usage, "cache_creation_input_tokens")
                            .unwrap_or(current.cache_creation),
                        cache_read: read_count(usage, "cache_read_input_tokens")
                            .unwrap_or(current.cache_read),
                    });
                }
            }
            _ => return None,
        }
        let used = self.stream_used_tokens()?;
        Some(self.usage_updated(used))
    }

    /// `buildResultUsage`; `None` is `undefined`.
    pub fn build_result_usage(&mut self, message: &Value, model_id: Option<&str>) -> Option<Value> {
        let result = (|| {
            let usage_v = message.get("usage").filter(|u| super::text::js_truthy(u))?;
            let usage = usage_v.as_object();
            let mut out = Map::new();
            let copy = |key: &str, from: Option<&Value>, out: &mut Map<String, Value>| {
                if let Some(v) = from.filter(|v| v.is_number()) {
                    out.insert(key.into(), v.clone());
                }
            };
            copy(
                "inputTokens",
                usage.and_then(|u| u.get("input_tokens")),
                &mut out,
            );
            copy(
                "cachedInputTokens",
                usage.and_then(|u| u.get("cache_read_input_tokens")),
                &mut out,
            );
            copy(
                "outputTokens",
                usage.and_then(|u| u.get("output_tokens")),
                &mut out,
            );
            copy("totalCostUsd", message.get("total_cost_usd"), &mut out);

            self.record_model_usage(message.get("modelUsage"), model_id);
            if let Some(max) = self.context_window_max {
                out.insert("contextWindowMaxTokens".into(), num(max));
            }
            let active = active_usage_tokens(usage_v).or_else(|| {
                if self.completed_result_turns == 0 {
                    usage.and_then(usage_token_total)
                } else {
                    None
                }
            });
            let used = self.stream_used_tokens().or(active).or(self.compacted_used);
            if let Some(used) = used {
                out.insert("contextWindowUsedTokens".into(), num(used));
            }
            Some(Value::Object(out))
        })();
        self.compacted_used = None;
        self.completed_result_turns += 1;
        result
    }

    /// `buildCompactionUsageEvent`.
    pub fn build_compaction_usage_event(&mut self, post_tokens: Option<f64>) -> Value {
        self.stream_input = None;
        self.stream_output = None;
        self.compacted_used = post_tokens;
        let mut usage = Map::new();
        if let Some(max) = self.context_window_max {
            usage.insert("contextWindowMaxTokens".into(), num(max));
        }
        if let Some(p) = post_tokens {
            usage.insert("contextWindowUsedTokens".into(), num(p));
        }
        serde_json::json!({"type": "usage_updated", "provider": "claude", "usage": Value::Object(usage)})
    }
}
