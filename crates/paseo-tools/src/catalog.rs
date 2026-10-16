//! `ToolCatalog` and its factory (`createPaseoToolCatalog`).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use indexmap::IndexMap;
use paseo_core::{is_paseo_tool_policy_enabled, PaseoToolPolicy};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::core_api::{CallerContext, Capability, CoreApi};
use crate::defs;
use crate::error::{ExecuteError, ToolError};
use crate::schemas;
use crate::serialization::add_model_visible_structured_content;
use crate::tools::{self, ToolEnv};
use crate::types::{ToolDef, ToolResult};
use crate::zod::{issues_message, InputSchema, ParsePass};

/// `PaseoToolRuntimeContext`: what a catalog is built for. The factory is called once per MCP
/// request and once per agent launch.
#[derive(Debug, Clone, Default)]
pub struct RuntimeCtx {
    pub caller_agent_id: Option<String>,
    pub policy: Option<PaseoToolPolicy>,
    pub enable_voice: bool,
    pub voice_only: bool,
}

/// One registered tool.
#[async_trait]
pub trait PaseoTool: Send + Sync {
    fn def(&self) -> &ToolDef;
    fn input_schema(&self) -> &InputSchema;
    /// Runs the handler on already-validated input.
    async fn call(&self, input: Value, cancel: CancellationToken) -> Result<ToolResult, ToolError>;
}

pub(crate) type HandlerFuture = Pin<Box<dyn Future<Output = Result<ToolResult, ToolError>> + Send>>;
pub(crate) type Handler =
    Arc<dyn Fn(Arc<ToolEnv>, Value, CancellationToken) -> HandlerFuture + Send + Sync>;

struct RegisteredTool {
    def: ToolDef,
    input: InputSchema,
    env: Arc<ToolEnv>,
    handler: Handler,
}

#[async_trait]
impl PaseoTool for RegisteredTool {
    fn def(&self) -> &ToolDef {
        &self.def
    }

    fn input_schema(&self) -> &InputSchema {
        &self.input
    }

    async fn call(&self, input: Value, cancel: CancellationToken) -> Result<ToolResult, ToolError> {
        (self.handler)(self.env.clone(), input, cancel).await
    }
}

/// Tools in registration order (which is the `tools/list` order).
#[derive(Clone)]
pub struct ToolCatalog {
    tools: IndexMap<String, Arc<dyn PaseoTool>>,
}

impl ToolCatalog {
    pub fn tools(&self) -> impl Iterator<Item = &Arc<dyn PaseoTool>> {
        self.tools.values()
    }

    pub fn names(&self) -> Vec<String> {
        self.tools.keys().cloned().collect()
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    pub fn get_tool(&self, name: &str) -> Option<&Arc<dyn PaseoTool>> {
        self.tools.get(name)
    }

    /// `executeTool`: parse with the catalog's own pass (passthrough for raw shapes), then run.
    /// `input` is `None` for JS `undefined`.
    pub async fn execute_tool(
        &self,
        name: &str,
        input: Option<&Value>,
        cancel: CancellationToken,
    ) -> Result<ToolResult, ExecuteError> {
        let tool = self
            .tools
            .get(name)
            .ok_or_else(|| ExecuteError::NotFound(name.to_owned()))?;
        let parsed = tool
            .input_schema()
            .parse(ParsePass::Catalog, input)
            .map_err(|issues| ExecuteError::InvalidInput(issues_message(&issues)))?;
        Ok(tool.call(parsed, cancel).await?)
    }
}

impl ToolCatalog {
    /// What `McpServer` does for a `tools/call`, as the `CallToolResult` JSON: SDK validation
    /// (strip pass), then `executeTool` (passthrough pass), then the handler. Failures become
    /// `isError` results carrying the message, with the `MCP error <code>:` prefix the SDK adds to
    /// its own `McpError`s.
    pub async fn call_tool_for_mcp(
        &self,
        name: &str,
        arguments: Option<&Value>,
        cancel: CancellationToken,
    ) -> Value {
        let Some(tool) = self.tools.get(name) else {
            return ToolResult::error_text(format!("MCP error -32602: Tool {name} not found"))
                .to_json();
        };
        let args = match tool.input_schema().parse(ParsePass::Mcp, arguments) {
            Ok(args) => args,
            Err(issues) => {
                return ToolResult::error_text(format!(
                    "MCP error -32602: Input validation error: Invalid arguments for tool {name}: {}",
                    issues_message(&issues)
                ))
                .to_json();
            }
        };
        match self.execute_tool(name, Some(&args), cancel).await {
            Ok(result) => add_model_visible_structured_content(result).to_json(),
            Err(error) => ToolResult::error_text(error.to_string()).to_json(),
        }
    }
}

/// Builds catalogs over a [`CoreApi`].
#[derive(Clone)]
pub struct ToolCatalogFactory {
    core: Arc<dyn CoreApi>,
}

/// `isPaseoToolEnabled`.
pub fn is_paseo_tool_enabled(policy: Option<&PaseoToolPolicy>, tool_name: &str) -> bool {
    // PARITY-QUIRK(speak-ignores-tool-policy): `speak` is never removed by the policy.
    if tool_name == "speak" {
        return true;
    }
    if !is_paseo_tool_policy_enabled(policy) {
        return false;
    }
    !policy.is_some_and(|p| p.disabled_tools.iter().any(|t| t == tool_name))
}

/// Registration helper handed to the tool modules.
pub(crate) struct Registrar {
    tools: IndexMap<String, Arc<dyn PaseoTool>>,
    policy: Option<PaseoToolPolicy>,
    caller: bool,
    env: Arc<ToolEnv>,
}

impl Registrar {
    /// `registerTool`: skipped when the policy disables the tool.
    pub(crate) fn register(&mut self, name: &str, handler: Handler) {
        if !is_paseo_tool_enabled(self.policy.as_ref(), name) {
            return;
        }
        let def = defs::tool_def(name, self.caller)
            .unwrap_or_else(|| panic!("no golden definition for tool {name}"));
        let input = schemas::input_schema(name, self.caller)
            .unwrap_or_else(|| panic!("no input schema for tool {name}"));
        self.tools.insert(
            name.to_owned(),
            Arc::new(RegisteredTool {
                def,
                input,
                env: self.env.clone(),
                handler,
            }),
        );
    }
}

impl ToolCatalogFactory {
    pub fn new(core: Arc<dyn CoreApi>) -> Self {
        Self { core }
    }

    /// `createPaseoToolCatalog` for `runtime`.
    pub async fn build(&self, runtime: RuntimeCtx) -> ToolCatalog {
        let caller_context: Option<CallerContext> = runtime
            .caller_agent_id
            .as_deref()
            .and_then(|id| self.core.caller_context(id));
        let env = Arc::new(ToolEnv {
            core: self.core.clone(),
            caller_agent_id: runtime.caller_agent_id.clone(),
            caller_context: caller_context.clone(),
        });
        let mut registrar = Registrar {
            tools: IndexMap::new(),
            policy: runtime.policy.clone(),
            caller: runtime.caller_agent_id.is_some(),
            env,
        };

        if runtime.voice_only
            || runtime.enable_voice
            || caller_context
                .as_ref()
                .is_some_and(|c| c.enable_voice_tools)
        {
            tools::speak::register(&mut registrar);
        }
        if runtime.voice_only {
            return ToolCatalog {
                tools: registrar.tools,
            };
        }
        if self.core.has(Capability::BrowserBroker) {
            tools::browser::register(&mut registrar);
        }
        tools::workspaces::register_first(&mut registrar);
        tools::agents::register(&mut registrar);
        tools::workspaces::register_second(&mut registrar);
        tools::terminals::register(&mut registrar);
        tools::schedules::register(&mut registrar);
        tools::providers::register(&mut registrar);
        tools::activity::register(&mut registrar);
        ToolCatalog {
            tools: registrar.tools,
        }
    }
}
