//! Kimi: thinking options exist only for the selected model, so catalog discovery switches
//! through each model and reads the options back.

use super::generic::{generic_client, GenericOptions};
use crate::agent::ModelDefinition;
use crate::client::AcpClient;
use crate::derive::{derive_selector_options, find_select_config_option};
use crate::error::Result;
use crate::profile::{CatalogContext, ProviderHooks};
use crate::protocol::SetConfigOptionParams;
use crate::util::BoxFuture;

pub struct KimiHooks;

impl ProviderHooks for KimiHooks {
    fn resolve_catalog_models<'a>(
        &'a self,
        ctx: CatalogContext<'a>,
    ) -> BoxFuture<'a, Result<Option<Vec<ModelDefinition>>>> {
        Box::pin(async move { resolve_catalog_models(ctx).await.map(Some) })
    }
}

pub async fn resolve_catalog_models(ctx: CatalogContext<'_>) -> Result<Vec<ModelDefinition>> {
    if ctx.models.len() <= 1 {
        return Ok(ctx.models);
    }
    let Some(model_option_id) = ctx
        .config_options
        .and_then(|o| find_select_config_option(o, "model", None))
        .map(|o| o.id.clone())
    else {
        return Ok(ctx.models);
    };

    let mut resolved = Vec::with_capacity(ctx.models.len());
    for model in ctx.models.iter().cloned() {
        let response = ctx
            .connection
            .set_config_option(&SetConfigOptionParams {
                session_id: ctx.session_id.to_string(),
                config_id: model_option_id.clone(),
                value: serde_json::Value::String(model.id.clone()),
            })
            .await;
        match response {
            Ok(r) => {
                let options = ctx.hooks.transform_config_options(r.config_options);
                let thinking = derive_selector_options(&options, "thought_level");
                let default_thinking = thinking.iter().find(|o| o.is_default).map(|o| o.id.clone());
                resolved.push(ModelDefinition {
                    thinking_options: thinking,
                    default_thinking_option_id: default_thinking,
                    ..model
                });
            }
            // The current model keeps the options the session already reported.
            Err(_) if model.is_default => resolved.push(model),
            Err(_) => resolved.push(ModelDefinition {
                thinking_options: Vec::new(),
                default_thinking_option_id: None,
                ..model
            }),
        }
    }
    Ok(resolved)
}

pub fn options(command: Vec<String>) -> GenericOptions {
    let mut o = GenericOptions::new(command);
    o.hooks = Some(std::sync::Arc::new(KimiHooks));
    o
}

pub fn client(command: Vec<String>) -> AcpClient {
    generic_client(options(command))
}
