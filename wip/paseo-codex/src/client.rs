//! [`CodexClient`]: the provider entry point (create / resume sessions, discover models, modes,
//! commands and persisted threads).

use std::sync::Arc;

use serde_json::{json, Value};
use tokio::sync::OnceCell;

use crate::agent::*;
use crate::catalog;
use crate::error::{Error, Result};
use crate::options::{self, CodexOptions, PROVIDER};
use crate::protocol::params::{
    self, InitializeParams, SkillsListResponse, ThreadListParams, ThreadListResponse,
};
use crate::rpc::{RpcClient, RpcEvent};
use crate::session::{config_from_handle, CodexSession, LaunchFlags};
use crate::transport::TransportFactory;

pub struct CodexClient {
    factory: Arc<dyn TransportFactory>,
    version: OnceCell<Option<String>>,
}

impl CodexClient {
    pub fn new(options: CodexOptions) -> Self {
        Self::with_factory(Arc::new(options.into_factory()))
    }

    /// Use a custom transport (tests, replay, a remote host).
    pub fn with_factory(factory: Arc<dyn TransportFactory>) -> Self {
        CodexClient {
            factory,
            version: OnceCell::new(),
        }
    }

    /// `codex --version` output, probed once. `None` when the binary does not answer.
    pub async fn version(&self) -> Option<&str> {
        self.version
            .get_or_init(|| async { self.factory.version().await.ok() })
            .await
            .as_deref()
    }

    pub async fn flags(&self) -> LaunchFlags {
        match self.version().await {
            Some(v) => LaunchFlags {
                goals: options::version_at_least(v, options::GOALS_MIN_VERSION),
                auto_review: options::version_at_least(v, options::AUTO_REVIEW_MIN_VERSION),
            },
            None => LaunchFlags::default(),
        }
    }

    /// Short-lived app-server for discovery calls. Server requests are answered with `{}`.
    async fn open_initialized(&self) -> Result<RpcClient> {
        let transport = self.factory.open(&["app-server".to_string()])?;
        let (rpc, mut events) = RpcClient::start(transport)?;
        let responder = rpc.clone();
        tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                match event {
                    RpcEvent::ServerRequest(req) => responder.respond(&req.id, json!({})),
                    RpcEvent::Closed(_) => break,
                    _ => {}
                }
            }
        });
        let init = async {
            rpc.request(
                "initialize",
                params::to_value(&InitializeParams::default()),
                None,
            )
            .await?;
            rpc.notify("initialized", json!({}));
            Ok::<_, Error>(())
        }
        .await;
        match init {
            Ok(()) => Ok(rpc),
            Err(e) => {
                rpc.close().await;
                Err(e)
            }
        }
    }

    async fn with_app_server<T, F, Fut>(&self, f: F) -> Result<T>
    where
        F: FnOnce(RpcClient) -> Fut,
        Fut: std::future::Future<Output = Result<T>>,
    {
        let rpc = self.open_initialized().await?;
        let result = f(rpc.clone()).await;
        rpc.close().await;
        result
    }
}

/// `overlay` over `base`: a field only replaces the base when the caller actually set it.
fn overlay(mut base: SessionConfig, over: SessionConfig) -> SessionConfig {
    if !over.cwd.is_empty() {
        base.cwd = over.cwd;
    }
    macro_rules! take {
        ($($f:ident),*) => { $( if over.$f.is_some() { base.$f = over.$f; } )* };
    }
    take!(
        system_prompt,
        daemon_append_system_prompt,
        mode_id,
        model,
        thinking_option_id,
        title,
        provider_options,
        tool_policy
    );
    for (k, v) in over.feature_values {
        base.feature_values.insert(k, v);
    }
    if !over.mcp_servers.is_empty() {
        base.mcp_servers = over.mcp_servers;
    }
    base
}

impl AgentClient for CodexClient {
    type Session = CodexSession;

    fn provider(&self) -> &'static str {
        PROVIDER
    }

    async fn create_session(&self, config: SessionConfig) -> Result<CodexSession> {
        let flags = self.flags().await;
        CodexSession::open(self.factory.clone(), config, None, flags).await
    }

    async fn resume_session(
        &self,
        handle: &PersistenceHandle,
        config: Option<SessionConfig>,
    ) -> Result<CodexSession> {
        let stored = config_from_handle(handle);
        let mut merged = match config {
            Some(over) => overlay(stored, over),
            None => stored,
        };
        if merged.cwd.is_empty() {
            merged.cwd = std::env::current_dir()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
        }
        let thread_id = handle
            .native_handle
            .clone()
            .unwrap_or_else(|| handle.session_id.clone());
        let flags = self.flags().await;
        CodexSession::open(self.factory.clone(), merged, Some(thread_id), flags).await
    }

    async fn fetch_catalog(&self) -> Result<ProviderCatalog> {
        let flags = self.flags().await;
        let models = self
            .with_app_server(|rpc| async move {
                let models = catalog::list_models(&rpc).await?;
                let defaults = catalog::read_configured_defaults(&rpc).await;
                Ok(catalog::build_catalog_models(&models, &defaults))
            })
            .await?;
        Ok(ProviderCatalog {
            models,
            modes: options::catalog_modes(flags.auto_review),
            default_mode_id: Some(options::default_mode_id(flags.auto_review).to_string()),
        })
    }

    async fn list_commands(&self, config: &SessionConfig) -> Result<Vec<SlashCommand>> {
        let cwd = config.cwd.clone();
        let skills = self
            .with_app_server(|rpc| async move {
                let v = rpc
                    .request(
                        "skills/list",
                        params::to_value(&params::SkillsListParams { cwds: vec![cwd] }),
                        None,
                    )
                    .await?;
                Ok(serde_json::from_value::<SkillsListResponse>(v)
                    .map(|r| r.data)
                    .unwrap_or_default())
            })
            .await?;
        let mut commands = vec![SlashCommand {
            name: "compact".into(),
            description: "Summarize conversation to prevent hitting the context limit".into(),
            argument_hint: String::new(),
            kind: SlashCommandKind::Command,
        }];
        let mut seen = std::collections::BTreeSet::new();
        for skill in skills.into_iter().flat_map(|e| e.skills) {
            if skill.enabled && !skill.name.is_empty() && seen.insert(skill.name.clone()) {
                commands.push(SlashCommand {
                    name: skill.name,
                    description: if skill.description.is_empty() {
                        "Skill".into()
                    } else {
                        skill.description
                    },
                    argument_hint: String::new(),
                    kind: SlashCommandKind::Skill,
                });
            }
        }
        commands.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(commands)
    }

    async fn list_importable_sessions(
        &self,
        cwd: Option<&str>,
        limit: u32,
    ) -> Result<Vec<ImportableSession>> {
        // Fetch a wider window when filtering: most threads belong to other cwds.
        let window = if cwd.is_some() { limit.max(50) } else { limit } as usize;
        let cwd_owned = cwd.map(str::to_string);
        let threads = self
            .with_app_server(|rpc| async move {
                let mut threads: Vec<crate::protocol::notifications::Thread> = Vec::new();
                let mut cursor: Option<String> = None;
                while threads.len() < window {
                    let request = ThreadListParams {
                        limit: Some((window - threads.len()) as u32),
                        // rank by last use, not creation time
                        sort_key: Some("updated_at".into()),
                        cwd: cwd_owned.clone(),
                        model_providers: None,
                        cursor: cursor.clone(),
                    };
                    let v: Value = rpc
                        .request("thread/list", params::to_value(&request), None)
                        .await?;
                    let page = serde_json::from_value::<ThreadListResponse>(v)
                        .map_err(|e| Error::Protocol(format!("thread/list: {e}")))?;
                    let before = threads.len();
                    for t in page.data {
                        if !threads.iter().any(|x| x.id == t.id) {
                            threads.push(t);
                        }
                    }
                    let Some(next) = page.next_cursor else { break };
                    // a page that adds nothing would loop forever on a stuck cursor
                    if threads.len() == before {
                        break;
                    }
                    cursor = Some(next);
                }
                Ok(threads)
            })
            .await?;
        Ok(threads
            .into_iter()
            .filter(|t| match cwd {
                Some(c) => t
                    .cwd
                    .as_deref()
                    .is_some_and(|tc| tc.trim_end_matches('/') == c.trim_end_matches('/')),
                None => true,
            })
            .take(limit as usize)
            .map(|t| {
                let title = t
                    .name
                    .clone()
                    .filter(|n| !n.trim().is_empty())
                    .or_else(|| t.preview.clone());
                ImportableSession {
                    session_id: t.id,
                    cwd: t.cwd,
                    title,
                    preview: t.preview,
                    updated_at: t.updated_at.or(t.created_at),
                }
            })
            .collect())
    }
}
