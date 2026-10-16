//! Mode / model / thinking-option / feature switching: the `set*` half of `ACPAgentSession`.

use serde_json::Value;

use crate::agent::ConfigFeatureOption;
use crate::connection::Connection;
use crate::derive::{
    derive_modes, find_select_config_choice, find_select_config_feature_option,
    find_select_config_option, find_select_config_option_by_id, flatten_select_options,
    resolve_mode_selection, resolve_model_selection,
};
use crate::error::{Error, Result};
use crate::events::SessionEvent;
use crate::profile::ModeWriterContext;
use crate::protocol::{
    ModelInfo, SelectConfigOption, SessionConfigOption, SetConfigOptionParams,
    SetConfigOptionResponse, SetModeParams, SetModelParams,
};
use crate::session::{AcpSession, AUTO_ACCEPT_FEATURE_ID};

impl AcpSession {
    fn require(&self) -> Result<(Connection, String)> {
        match (self.inner.connection(), self.id()) {
            (Some(c), Some(s)) => Ok((c, s)),
            _ => Err(Error::Unsupported("ACP session not initialized".into())),
        }
    }

    fn warn(&self, message: String) {
        self.inner.emit(SessionEvent::Warning { message });
    }

    fn unavailable_model_message(&self) -> String {
        format!("{} does not expose ACP model selection", self.provider())
    }

    fn unavailable_feature_message(&self, feature_id: &str) -> String {
        format!(
            "{} does not expose ACP feature '{}'",
            self.provider(),
            feature_id
        )
    }

    pub async fn set_mode(&self, mode_id: &str) -> Result<()> {
        self.require()?;
        self.set_mode_inner(mode_id).await
    }

    async fn set_mode_inner(&self, mode_id: &str) -> Result<()> {
        let (conn, session_id) = self.require()?;
        let provider = self.provider().to_string();
        let hooks = self.inner.profile.hooks.clone();
        let (options, current_mode, available_modes) = {
            let s = self.inner.state.lock().expect("state lock");
            (
                s.config_options.clone(),
                s.current_mode.clone(),
                s.available_modes.clone(),
            )
        };

        let ctx = ModeWriterContext {
            connection: &conn,
            session_id: &session_id,
            requested_mode_id: mode_id,
            current_mode_id: current_mode.as_deref(),
            config_options: &options,
        };
        let provided = hooks.provider_mode_write(&ctx).await?;
        if provided.handled {
            let mut s = self.inner.state.lock().expect("state lock");
            s.current_mode = Some(
                provided
                    .current_mode_id
                    .unwrap_or_else(|| mode_id.to_string()),
            );
            if let Some(opts) = provided.config_options {
                s.config_options = self.inner.transform_config_options(opts);
            }
            s.available_modes =
                derive_modes(&self.inner.profile.default_modes, None, &s.config_options).0;
            let event = SessionEvent::ModeChanged {
                current_mode_id: s.current_mode.clone(),
                available_modes: s.available_modes.clone(),
            };
            drop(s);
            self.inner.emit(event);
            return Ok(());
        }

        let (has_modes, mode_valid, config_option_id, config_option_values) = {
            let sel = resolve_mode_selection(mode_id, &available_modes, &options);
            (
                sel.has_available_modes,
                sel.available_mode.is_some(),
                sel.config_option.map(|o| o.id.clone()),
                sel.config_option.map(|o| {
                    flatten_select_options(o)
                        .into_iter()
                        .map(|c| c.choice.value)
                        .collect::<Vec<_>>()
                }),
            )
        };
        let choice_valid =
            find_select_config_choice(find_select_config_option(&options, "mode", None), mode_id)
                .is_some();

        if has_modes {
            if !mode_valid {
                let ids: Vec<_> = available_modes.iter().map(|m| m.id.clone()).collect();
                self.warn(format!(
                    "{mode_id} is not valid {provider} mode. Available options: {}",
                    ids.join(", ")
                ));
                return Ok(());
            }
        } else {
            if config_option_id.is_none() {
                return Err(Error::Unsupported(format!(
                    "{provider} does not expose ACP mode switching"
                )));
            }
            if !choice_valid {
                self.warn(format!(
                    "{mode_id} is not valid {provider} mode config option. Available options: {}",
                    config_option_values.unwrap_or_default().join(", ")
                ));
                return Ok(());
            }
        }

        let before = hooks.before_mode_write(&ctx).await?;
        if let Some(opts) = before.config_options {
            self.inner.state.lock().expect("state lock").config_options =
                self.inner.transform_config_options(opts);
        }

        if has_modes {
            conn.set_mode(&SetModeParams {
                session_id: session_id.clone(),
                mode_id: mode_id.to_string(),
            })
            .await?;
            let event = {
                let mut s = self.inner.state.lock().expect("state lock");
                s.current_mode = Some(mode_id.to_string());
                SessionEvent::ModeChanged {
                    current_mode_id: s.current_mode.clone(),
                    available_modes: s.available_modes.clone(),
                }
            };
            self.inner.emit(event);
            return Ok(());
        }

        let config_id = config_option_id.expect("checked above");
        let response = conn
            .set_config_option(&SetConfigOptionParams {
                session_id,
                config_id: config_id.clone(),
                value: Value::String(mode_id.to_string()),
            })
            .await?;
        let current =
            self.apply_config_option_response(response, &config_id, Some("mode"), mode_id, "mode");
        let event = {
            let mut s = self.inner.state.lock().expect("state lock");
            s.current_mode = Some(current);
            s.available_modes =
                derive_modes(&self.inner.profile.default_modes, None, &s.config_options).0;
            SessionEvent::ModeChanged {
                current_mode_id: s.current_mode.clone(),
                available_modes: s.available_modes.clone(),
            }
        };
        self.inner.emit(event);
        Ok(())
    }

    pub async fn set_model(&self, model_id: Option<&str>) -> Result<()> {
        self.require()?;
        let Some(model_id) = model_id.filter(|m| !m.is_empty()) else {
            self.inner.state.lock().expect("state lock").current_model = None;
            return Ok(());
        };
        self.set_model_inner(model_id).await
    }

    async fn set_model_inner(&self, model_id: &str) -> Result<()> {
        let (conn, session_id) = self.require()?;
        let provider = self.provider().to_string();
        let (options, available) = {
            let s = self.inner.state.lock().expect("state lock");
            (s.config_options.clone(), s.available_models.clone())
        };
        let selection = resolve_model_selection(model_id, available.as_deref(), &options);
        let mut available_model = selection.available_model.clone();
        // A model the user added in provider config is applied even when the agent does not advertise it.
        if available_model.is_none() && self.inner.configured_model_ids.contains(model_id) {
            available_model = Some(ModelInfo {
                model_id: model_id.to_string(),
                name: model_id.to_string(),
                description: None,
                meta: None,
            });
        }
        let config_option_id = selection.config_option.map(|o| o.id.clone());
        let config_option_values = selection.config_option.map(|o| {
            flatten_select_options(o)
                .into_iter()
                .map(|c| c.choice.value)
                .collect::<Vec<_>>()
        });
        let choice_valid = selection.config_choice.is_some();

        if selection.has_available_models && available_model.is_none() {
            let ids: Vec<_> = available
                .iter()
                .flatten()
                .map(|m| m.model_id.clone())
                .collect();
            self.warn(format!(
                "{model_id} is not a valid {provider} model. Available options: {}",
                ids.join(", ")
            ));
            return Ok(());
        }

        if available_model.is_some() {
            let result = conn
                .set_model(&SetModelParams {
                    session_id: session_id.clone(),
                    model_id: model_id.to_string(),
                })
                .await;
            if result.is_ok() {
                self.inner.state.lock().expect("state lock").current_model =
                    Some(model_id.to_string());
                self.inner.emit(SessionEvent::ModelChanged {
                    runtime: self.inner.runtime_info(),
                });
                return Ok(());
            }
            // Fall through to the config option path.
        }

        let Some(config_id) = config_option_id else {
            return Err(Error::Unsupported(self.unavailable_model_message()));
        };
        if !choice_valid {
            self.warn(format!(
                "{model_id} is not a valid {provider} model config option. Available options: {}",
                config_option_values.unwrap_or_default().join(", ")
            ));
            return Ok(());
        }
        let response = conn
            .set_config_option(&SetConfigOptionParams {
                session_id,
                config_id: config_id.clone(),
                value: Value::String(model_id.to_string()),
            })
            .await?;
        let current = self.apply_config_option_response(
            response,
            &config_id,
            Some("model"),
            model_id,
            "model",
        );
        self.inner.state.lock().expect("state lock").current_model = Some(current);
        self.inner.emit(SessionEvent::ModelChanged {
            runtime: self.inner.runtime_info(),
        });
        Ok(())
    }

    pub async fn set_thinking_option(&self, thinking_option_id: Option<&str>) -> Result<()> {
        let (conn, session_id) = self.require()?;
        let Some(id) = thinking_option_id.filter(|i| !i.is_empty()) else {
            self.inner
                .state
                .lock()
                .expect("state lock")
                .thinking_option_id = None;
            return Ok(());
        };
        if self
            .inner
            .profile
            .hooks
            .write_thinking_option(&conn, &session_id, id)
            .await?
        {
            self.inner
                .state
                .lock()
                .expect("state lock")
                .thinking_option_id = Some(id.to_string());
            self.inner.emit(SessionEvent::ThinkingOptionChanged {
                thinking_option_id: Some(id.to_string()),
            });
            return Ok(());
        }
        let config_id = {
            let s = self.inner.state.lock().expect("state lock");
            find_select_config_option(&s.config_options, "thought_level", None)
                .map(|o| o.id.clone())
        };
        let Some(config_id) = config_id else {
            return Err(Error::Unsupported(format!(
                "{} does not expose ACP thought-level selection",
                self.provider()
            )));
        };
        let response = conn
            .set_config_option(&SetConfigOptionParams {
                session_id,
                config_id: config_id.clone(),
                value: Value::String(id.to_string()),
            })
            .await?;
        let current = self.apply_config_option_response(
            response,
            &config_id,
            Some("thought_level"),
            id,
            "thought-level",
        );
        self.inner
            .state
            .lock()
            .expect("state lock")
            .thinking_option_id = Some(current.clone());
        self.inner.emit(SessionEvent::ThinkingOptionChanged {
            thinking_option_id: Some(current),
        });
        Ok(())
    }

    pub async fn set_feature(&self, feature_id: &str, value: Value) -> Result<()> {
        let (conn, session_id) = self.require()?;
        if feature_id == AUTO_ACCEPT_FEATURE_ID {
            self.inner
                .config
                .lock()
                .expect("config lock")
                .feature_values
                .insert(
                    AUTO_ACCEPT_FEATURE_ID.into(),
                    Value::Bool(value == Value::Bool(true)),
                );
            return Ok(());
        }
        let feature: ConfigFeatureOption = self
            .inner
            .profile
            .config_feature_options
            .iter()
            .find(|f| f.id == feature_id)
            .cloned()
            .ok_or_else(|| {
                Error::Unsupported(format!("Unknown {} feature: {feature_id}", self.provider()))
            })?;

        let config_id = {
            let s = self.inner.state.lock().expect("state lock");
            find_select_config_feature_option(&s.config_options, &feature).map(|o| o.id.clone())
        };
        let Some(config_id) = config_id else {
            return Err(Error::Unsupported(
                self.unavailable_feature_message(feature_id),
            ));
        };
        let requested = match &value {
            Value::String(s) => s.clone(),
            Value::Null => String::new(),
            _ => {
                return Err(Error::Unsupported(
                    "ACP feature value must be a string".into(),
                ))
            }
        };
        let choice_exists = {
            let s = self.inner.state.lock().expect("state lock");
            let option = find_select_config_feature_option(&s.config_options, &feature);
            find_select_config_choice(option, &requested).is_some()
        };
        if !choice_exists {
            return Err(Error::Unsupported(format!(
                "{} feature '{feature_id}' does not include option '{requested}'",
                self.provider()
            )));
        }
        let response = conn
            .set_config_option(&SetConfigOptionParams {
                session_id,
                config_id: config_id.clone(),
                value: Value::String(requested.clone()),
            })
            .await?;
        let current = self.apply_config_option_response(
            response,
            &config_id,
            feature.category.as_deref(),
            &requested,
            &feature.label,
        );
        self.inner
            .config
            .lock()
            .expect("config lock")
            .feature_values
            .insert(feature_id.to_string(), Value::String(current));
        Ok(())
    }

    /// Store the options from a `session/set_config_option` response and return the option's
    /// new current value, or the requested one when the response does not say.
    fn apply_config_option_response(
        &self,
        response: SetConfigOptionResponse,
        config_id: &str,
        category: Option<&str>,
        requested: &str,
        label: &str,
    ) -> String {
        let options: Vec<SessionConfigOption> =
            self.inner.transform_config_options(response.config_options);
        let found: Option<String> = {
            let option: Option<&SelectConfigOption> = match category {
                None => find_select_config_option_by_id(&options, config_id),
                Some(c) => find_select_config_option(&options, c, Some(config_id)),
            };
            option.map(|o| o.current_value.clone())
        };
        self.inner.state.lock().expect("state lock").config_options = options;
        found.unwrap_or_else(|| {
            self.warn(format!(
                "ACP setSessionConfigOption response did not include the requested {label} option currentValue; using requested value"
            ));
            requested.to_string()
        })
    }

    /// Apply the model / mode / thinking / feature values from the session config
    /// (`applyConfiguredOverrides`).
    pub(crate) async fn apply_configured_overrides(&self) -> Result<()> {
        let config = self.inner.config.lock().expect("config lock").clone();
        let (current_mode, current_model, current_thinking) = {
            let s = self.inner.state.lock().expect("state lock");
            (
                s.current_mode.clone(),
                s.current_model.clone(),
                s.thinking_option_id.clone(),
            )
        };
        if let Some(mode) = config
            .mode_id
            .as_deref()
            .filter(|m| Some(*m) != current_mode.as_deref())
        {
            self.set_mode_inner(mode).await?;
        }
        let mut switched_model = false;
        if let Some(model) = config
            .model
            .as_deref()
            .filter(|m| Some(*m) != current_model.as_deref())
        {
            match self.set_model_inner(model).await {
                Ok(()) => switched_model = true,
                Err(Error::Unsupported(m)) if m == self.unavailable_model_message() => {
                    self.warn(format!(
                        "{} does not expose ACP model selection; using provider default model",
                        self.provider()
                    ));
                }
                Err(e) => return Err(e),
            }
        }
        if let Some(t) = config
            .thinking_option_id
            .as_deref()
            .filter(|t| Some(*t) != current_thinking.as_deref())
        {
            self.set_thinking_option(Some(t)).await?;
        }
        for feature in self.inner.profile.config_feature_options.clone() {
            let Some(value) = config.feature_values.get(&feature.id).cloned() else {
                continue;
            };
            if let Err(e) = self.set_feature(&feature.id, value).await {
                let unavailable = matches!(&e, Error::Unsupported(m) if *m == self.unavailable_feature_message(&feature.id));
                // A stored value is a preference from whichever model the user last configured;
                // the model switch answers with an empty response, so the agent itself may reject it.
                if unavailable || (switched_model && e.is_invalid_params()) {
                    self.warn(format!(
                        "{} cannot apply ACP feature '{}' to the current model; using the provider default",
                        self.provider(),
                        feature.id
                    ));
                    continue;
                }
                return Err(e);
            }
        }
        Ok(())
    }
}
