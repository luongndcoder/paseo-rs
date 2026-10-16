//! Cursor (`cursor-agent acp`): model catalog from the `cursor/list_available_models`
//! extension (switching models in a probe session would persist CLI preferences), a `fast`
//! feature, and the `parameterizedModelPicker` client capability.

use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

use super::generic::{generic_client, GenericOptions};
use crate::agent::{ConfigFeatureOption, ModelDefinition};
use crate::client::AcpClient;
use crate::derive::derive_selector_options;
use crate::error::{Error, Result};
use crate::profile::{CatalogContext, ProviderHooks};
use crate::protocol::{Extra, SessionConfigOption};
use crate::util::BoxFuture;

const INITIAL_COMMANDS_WAIT_TIMEOUT: Duration = Duration::from_secs(10);
pub const LIST_MODELS_METHOD: &str = "cursor/list_available_models";

pub fn fast_feature_option() -> ConfigFeatureOption {
    ConfigFeatureOption {
        id: "fast".into(),
        config_id: "fast".into(),
        category: None,
        label: "Fast".into(),
        description: Some("Cursor fast mode".into()),
        tooltip: Some("Select Cursor fast mode".into()),
        icon: Some("zap".into()),
        empty_option_label: None,
    }
}

#[derive(Deserialize)]
struct Catalog {
    models: Vec<CatalogModel>,
}

#[derive(Deserialize)]
struct CatalogModel {
    value: String,
    name: String,
    #[serde(rename = "configOptions")]
    config_options: Vec<SessionConfigOption>,
}

pub struct CursorHooks;

impl ProviderHooks for CursorHooks {
    fn resolve_catalog_models<'a>(
        &'a self,
        ctx: CatalogContext<'a>,
    ) -> BoxFuture<'a, Result<Option<Vec<ModelDefinition>>>> {
        Box::pin(async move { resolve_catalog_models(ctx).await.map(Some) })
    }
}

pub async fn resolve_catalog_models(ctx: CatalogContext<'_>) -> Result<Vec<ModelDefinition>> {
    let raw =
        match ctx
            .connection
            .ext_method(LIST_MODELS_METHOD, serde_json::json!({}))
            .await
        {
            Ok(v) => v,
            Err(e) if e.rpc_code() == Some(-32601) => return Err(Error::Unsupported(
                "Update Cursor CLI: this version does not support cursor/list_available_models."
                    .into(),
            )),
            Err(e) => return Err(e),
        };
    let catalog: Catalog = serde_json::from_value(raw)
        .map_err(|e| Error::Protocol(format!("{LIST_MODELS_METHOD}: {e}")))?;
    let current = ctx
        .models
        .iter()
        .find(|m| m.is_default)
        .map(|m| m.id.clone());
    Ok(catalog
        .models
        .into_iter()
        .map(|m| {
            let thinking = derive_selector_options(&m.config_options, "thought_level");
            let default_thinking = thinking.iter().find(|o| o.is_default).map(|o| o.id.clone());
            ModelDefinition {
                provider: ctx.provider.to_string(),
                is_default: current.as_deref() == Some(m.value.as_str()),
                id: m.value,
                label: m.name,
                description: None,
                metadata: None,
                thinking_options: thinking,
                default_thinking_option_id: default_thinking,
            }
        })
        .collect())
}

pub fn options(command: Vec<String>) -> GenericOptions {
    let mut o = GenericOptions::new(command);
    o.wait_for_initial_commands = true;
    o.initial_commands_wait_timeout = INITIAL_COMMANDS_WAIT_TIMEOUT;
    let mut meta = Extra::new();
    meta.insert("parameterizedModelPicker".into(), Value::Bool(true));
    o.client_capability_meta = Some(meta);
    o.config_feature_options = vec![fast_feature_option()];
    o.hooks = Some(std::sync::Arc::new(CursorHooks));
    o
}

pub fn client(command: Vec<String>) -> AcpClient {
    generic_client(options(command))
}
