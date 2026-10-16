//! GitHub Copilot CLI (`copilot --acp`). Copilot's native `autopilot` mode is replaced by an
//! `allow-all` mode backed by the `allow_all` config option, and a custom `agent` selector
//! is exposed as a feature.

use serde_json::Value;

use crate::agent::{AgentMode, ConfigFeatureOption};
use crate::client::AcpClient;
use crate::connection::Connection;
use crate::error::Result;
use crate::profile::{
    AgentProfile, BeforeModeWriteResult, ModeWriteResult, ModeWriterContext, ProviderHooks,
};
use crate::protocol::{
    as_select, KnownConfigOption, Lenient, SelectChoice, SelectEntry, SessionConfigOption,
    SessionMode, SessionState, SetConfigOptionParams,
};
use crate::util::BoxFuture;

pub const PROVIDER: &str = "copilot";
pub const AGENT_MODE_ID: &str = "https://agentclientprotocol.com/protocol/session-modes#agent";
pub const PLAN_MODE_ID: &str = "https://agentclientprotocol.com/protocol/session-modes#plan";
pub const AUTOPILOT_MODE_ID: &str =
    "https://agentclientprotocol.com/protocol/session-modes#autopilot";
pub const ALLOW_ALL_MODE_ID: &str = "allow-all";
const ALLOW_ALL_CONFIG_ID: &str = "allow_all";
const ALLOW_ALL_DESCRIPTION: &str =
    "Automatically approves all Copilot tool, path, and URL requests.";

pub fn agent_feature_option() -> ConfigFeatureOption {
    ConfigFeatureOption {
        id: "agent".into(),
        config_id: "agent".into(),
        category: Some("_agent".into()),
        label: "Agent".into(),
        description: Some("Use a Copilot custom agent profile".into()),
        tooltip: Some("Select Copilot agent".into()),
        icon: None,
        empty_option_label: Some("Default".into()),
    }
}

pub fn modes() -> Vec<AgentMode> {
    vec![
        AgentMode {
            id: AGENT_MODE_ID.into(),
            label: "Agent".into(),
            description: Some("Default agent mode for conversational interactions".into()),
        },
        AgentMode {
            id: PLAN_MODE_ID.into(),
            label: "Plan".into(),
            description: Some("Plan mode for creating and executing multi-step plans".into()),
        },
        AgentMode {
            id: ALLOW_ALL_MODE_ID.into(),
            label: "Allow All".into(),
            description: Some(ALLOW_ALL_DESCRIPTION.into()),
        },
    ]
}

pub fn profile() -> AgentProfile {
    let mut p = AgentProfile::new(PROVIDER, vec!["copilot".into(), "--acp".into()]);
    p.default_modes = modes();
    p.config_feature_options = vec![agent_feature_option()];
    p.with_hooks(CopilotHooks)
}

pub fn client() -> AcpClient {
    AcpClient::new(profile())
}

pub struct CopilotHooks;

impl ProviderHooks for CopilotHooks {
    fn transform_session_response(&self, response: SessionState) -> SessionState {
        transform_session_response(response)
    }

    fn transform_config_options(
        &self,
        options: Vec<SessionConfigOption>,
    ) -> Vec<SessionConfigOption> {
        transform_config_options(options)
    }

    fn transform_mode_id(&self, mode_id: &str) -> Option<String> {
        Some(transform_mode_id(mode_id))
    }

    fn provider_mode_write<'a>(
        &'a self,
        ctx: &'a ModeWriterContext<'a>,
    ) -> BoxFuture<'a, Result<ModeWriteResult>> {
        Box::pin(write_provider_mode(ctx))
    }

    fn before_mode_write<'a>(
        &'a self,
        ctx: &'a ModeWriterContext<'a>,
    ) -> BoxFuture<'a, Result<BeforeModeWriteResult>> {
        Box::pin(before_mode_write(ctx))
    }
}

fn is_allow_all_enabled(options: &[SessionConfigOption]) -> bool {
    options
        .iter()
        .filter_map(as_select)
        .any(|o| o.id == ALLOW_ALL_CONFIG_ID && o.current_value == "on")
}

pub fn transform_mode_id(mode_id: &str) -> String {
    if mode_id == AUTOPILOT_MODE_ID {
        AGENT_MODE_ID.to_string()
    } else {
        mode_id.to_string()
    }
}

pub fn transform_session_response(mut response: SessionState) -> SessionState {
    let allow_all = is_allow_all_enabled(response.config_options.as_deref().unwrap_or_default());
    let Some(modes) = response.modes.as_mut() else {
        return response;
    };
    if let Some(available) = modes.available_modes.take() {
        let mut kept: Vec<SessionMode> = available
            .into_iter()
            .filter(|m| m.id != AUTOPILOT_MODE_ID && m.id != ALLOW_ALL_MODE_ID)
            .collect();
        kept.push(SessionMode {
            id: ALLOW_ALL_MODE_ID.into(),
            name: "Allow All".into(),
            description: Some(ALLOW_ALL_DESCRIPTION.into()),
        });
        modes.available_modes = Some(kept);
    }
    modes.current_mode_id = Some(if allow_all {
        ALLOW_ALL_MODE_ID.to_string()
    } else {
        transform_mode_id(modes.current_mode_id.as_deref().unwrap_or(AGENT_MODE_ID))
    });
    response
}

pub fn transform_config_options(options: Vec<SessionConfigOption>) -> Vec<SessionConfigOption> {
    let allow_all = is_allow_all_enabled(&options);
    options
        .into_iter()
        .map(|option| {
            let Lenient::Known(KnownConfigOption::Select(mut select)) = option else {
                return option;
            };
            if select.category.as_deref() != Some("mode") {
                return Lenient::Known(KnownConfigOption::Select(select));
            }
            // Trust Copilot's allow_all config value as the source of truth when it changes in-process.
            let mut flat: Vec<SelectChoice> = Vec::new();
            for entry in select.options {
                match entry {
                    SelectEntry::Choice(c) => flat.push(c),
                    SelectEntry::Group(g) => flat.extend(g.options),
                }
            }
            flat.retain(|c| c.value != AUTOPILOT_MODE_ID && c.value != ALLOW_ALL_MODE_ID);
            flat.push(SelectChoice {
                value: ALLOW_ALL_MODE_ID.into(),
                name: "Allow All".into(),
                description: Some(ALLOW_ALL_DESCRIPTION.into()),
                extra: Default::default(),
            });
            select.current_value = if allow_all {
                ALLOW_ALL_MODE_ID.to_string()
            } else {
                transform_mode_id(&select.current_value)
            };
            select.options = flat.into_iter().map(SelectEntry::Choice).collect();
            Lenient::Known(KnownConfigOption::Select(select))
        })
        .collect()
}

async fn set_allow_all(
    conn: &Connection,
    session_id: &str,
    on: bool,
) -> Result<Vec<SessionConfigOption>> {
    let response = conn
        .set_config_option(&SetConfigOptionParams {
            session_id: session_id.to_string(),
            config_id: ALLOW_ALL_CONFIG_ID.into(),
            value: Value::String(if on { "on" } else { "off" }.into()),
        })
        .await?;
    Ok(response.config_options)
}

pub async fn write_provider_mode(ctx: &ModeWriterContext<'_>) -> Result<ModeWriteResult> {
    // COMPAT(copilotAutopilotMode): added in v0.1.75, remove after 2026-11-12 once old clients
    // no longer send Copilot's old ACP autopilot mode ID.
    let wants_allow_all =
        ctx.requested_mode_id == ALLOW_ALL_MODE_ID || ctx.requested_mode_id == AUTOPILOT_MODE_ID;
    if !wants_allow_all {
        return Ok(ModeWriteResult::default());
    }
    let config_options = set_allow_all(ctx.connection, ctx.session_id, true).await?;
    Ok(ModeWriteResult {
        handled: true,
        current_mode_id: Some(ALLOW_ALL_MODE_ID.into()),
        config_options: Some(config_options),
    })
}

pub async fn before_mode_write(ctx: &ModeWriterContext<'_>) -> Result<BeforeModeWriteResult> {
    if ctx.current_mode_id != Some(ALLOW_ALL_MODE_ID) || ctx.requested_mode_id == ALLOW_ALL_MODE_ID
    {
        return Ok(BeforeModeWriteResult::default());
    }
    let config_options = set_allow_all(ctx.connection, ctx.session_id, false).await?;
    Ok(BeforeModeWriteResult {
        config_options: Some(config_options),
    })
}
