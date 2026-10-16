//! The Paseo tool catalog: the tools agents call over MCP (and as native host tools), their
//! golden definitions, input validation, result text synthesis and the [`CoreApi`] seam they run
//! over. Behavior is the TS daemon's (`packages/server/src/server/agent/tools`,
//! `browser-tools/tools.ts`); see docs/p4-core-spec.md section 7.
//!
//! Modules:
//! - [`catalog`]: [`ToolCatalog`], [`ToolCatalogFactory`], policy filtering, registration order.
//! - [`core_api`]: [`CoreApi`], what the tools need from the daemon.
//! - [`defs`]: golden title/description/JSON schemas (`data/tool-defs.json`).
//! - [`zod`] and [`schemas`]: the zod 4 subset and the per-tool input schemas.
//! - [`serialization`]: structured-content text synthesis and the `tools/list` shape.

pub mod catalog;
pub mod core_api;
pub mod defs;
pub mod error;
pub mod schemas;
pub mod serialization;
mod tools;
pub mod types;
pub mod util;
pub mod zod;

pub use catalog::{PaseoTool, RuntimeCtx, ToolCatalog, ToolCatalogFactory};
pub use core_api::CoreApi;
pub use error::{ExecuteError, ToolError};
pub use serialization::{
    add_model_visible_structured_content, mcp_tool_listing, serialize_tool_input_parameters,
};
pub use types::{ToolDef, ToolResult};
