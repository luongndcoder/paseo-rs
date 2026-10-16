//! `AgentClient` for OpenCode: acquires servers, creates and resumes sessions, discovers catalogues.

use super::mapping::*;
use super::session::{capabilities, OpenCodeAgentSession, SessionParams};
use super::types::*;
use super::{AgentClient, AgentSession};
use crate::client::RawClient;
use crate::error::{Error, Result};
use crate::manager::{
    Acquire, ManagedServers, OpenCodeServerManager, ServerHandle, ServerProvider,
};
use crate::options::{OpenCodeOptions, SERVER_STARTUP_TIMEOUT};
use crate::protocol::*;
use crate::server::find_executable;
use async_trait::async_trait;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

pub struct OpenCodeAgentClient {
    servers: Arc<dyn ServerProvider>,
    opts: OpenCodeOptions,
}

impl OpenCodeAgentClient {
    /// Spawns and manages `opencode serve` processes itself.
    pub fn new(opts: OpenCodeOptions) -> Self {
        let servers = Arc::new(ManagedServers(OpenCodeServerManager::new(opts.clone())));
        Self { servers, opts }
    }

    /// Uses an existing server provider (tests, or a server owned elsewhere).
    pub fn with_servers(servers: Arc<dyn ServerProvider>, opts: OpenCodeOptions) -> Self {
        Self { servers, opts }
    }

    async fn acquire(&self, launch: Option<&AgentLaunchContext>) -> Result<ServerHandle> {
        match launch.and_then(|l| l.env.clone()) {
            Some(env) => self.servers.acquire(Acquire::Dedicated(env)).await,
            None => self.servers.acquire(Acquire::Current).await,
        }
    }

    async fn context_windows(client: &RawClient) -> HashMap<String, u64> {
        let Ok(list) = client.provider_list().await else {
            return HashMap::new();
        };
        providers_to_models(&list)
            .into_iter()
            .filter_map(|m| m.context_window_max_tokens.map(|c| (m.id, c)))
            .collect()
    }
}

#[async_trait]
impl AgentClient for OpenCodeAgentClient {
    fn provider(&self) -> &str {
        "opencode"
    }

    fn capabilities(&self) -> AgentCapabilities {
        capabilities()
    }

    async fn create_session(
        &self,
        config: AgentSessionConfig,
        launch: Option<AgentLaunchContext>,
        options: AgentCreateSessionOptions,
    ) -> Result<Arc<dyn AgentSession>> {
        let handle = self.acquire(launch.as_ref()).await?;
        let client = RawClient::new(handle.transport.clone(), Some(config.cwd.clone()));
        // Permission rules live on the session (prompts do not carry them), so set them here.
        let body = SessionCreateBody {
            permission: session_permission_rules(&config),
            ..Default::default()
        };
        // The first session in a directory is part of OpenCode coming up: share the startup budget.
        let created =
            tokio::time::timeout(SERVER_STARTUP_TIMEOUT, client.session_create(&body)).await;
        let session = match created {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => {
                handle.release().await;
                return Err(Error::Other(format!(
                    "Failed to create OpenCode session: {e}"
                )));
            }
            Err(_) => {
                handle.release().await;
                return Err(Error::Timeout("OpenCode session.create timed out".into()));
            }
        };
        let context_windows = Self::context_windows(&client).await;
        Ok(Arc::new(OpenCodeAgentSession::new(SessionParams {
            config,
            session_id: session.id,
            client,
            server: handle,
            persist: options.persist_session.unwrap_or(true),
            context_windows,
        })))
    }

    async fn resume_session(
        &self,
        handle: &AgentPersistenceHandle,
        overrides: Option<AgentSessionConfig>,
        launch: Option<AgentLaunchContext>,
    ) -> Result<Arc<dyn AgentSession>> {
        let meta_str = |k: &str| {
            handle
                .metadata
                .get(k)
                .and_then(|v| v.as_str())
                .map(str::to_owned)
        };
        let mut config = overrides.unwrap_or_default();
        if config.cwd.is_empty() {
            config.cwd = meta_str("cwd").ok_or_else(|| {
                Error::Other("OpenCode resume requires the original working directory".into())
            })?;
        }
        config.mode_id = config.mode_id.or_else(|| meta_str("modeId"));
        config.model = config.model.or_else(|| meta_str("model"));
        let server = self.acquire(launch.as_ref()).await?;
        let client = RawClient::new(server.transport.clone(), Some(config.cwd.clone()));
        if let Some(rules) = session_permission_rules(&config) {
            let update = SessionUpdateBody {
                permission: Some(rules),
                ..Default::default()
            };
            if let Err(e) = client.session_update(&handle.session_id, &update).await {
                server.release().await;
                return Err(Error::Other(format!(
                    "Failed to apply OpenCode session permission rules: {e}"
                )));
            }
        }
        let context_windows = Self::context_windows(&client).await;
        Ok(Arc::new(OpenCodeAgentSession::new(SessionParams {
            config,
            session_id: handle.session_id.clone(),
            client,
            server,
            persist: true,
            context_windows,
        })))
    }

    async fn fetch_catalog(&self, options: FetchCatalogOptions) -> Result<ProviderCatalog> {
        let kind = if options.force {
            Acquire::New
        } else {
            Acquire::Current
        };
        let handle = self.servers.acquire(kind).await?;
        let directory: PathBuf = match &options.scope {
            FetchCatalogScope::Workspace { cwd } => PathBuf::from(cwd),
            FetchCatalogScope::Global => {
                let dir = self.opts.home_dir_ref().clone();
                std::fs::create_dir_all(&dir)?;
                dir
            }
        };
        let client = RawClient::new(
            handle.transport.clone(),
            Some(directory.to_string_lossy().into_owned()),
        );
        let (providers, agents) = tokio::join!(client.provider_list(), client.agents());
        handle.release().await;
        let providers = providers
            .map_err(|e| Error::Other(format!("Failed to fetch OpenCode providers: {e}")))?;
        let accessible = providers
            .all
            .iter()
            .any(|p| providers.connected.contains(&p.id) || p.source.as_deref() == Some("api"));
        if !accessible {
            return Err(Error::Other(
                "OpenCode has no connected providers. Authenticate with at least one provider, set its API key environment variable, or log in to OpenCode Go.".into(),
            ));
        }
        Ok(ProviderCatalog {
            models: providers_to_models(&providers),
            modes: agents.map(|a| agents_to_modes(&a)).unwrap_or_default(),
        })
    }

    async fn list_commands(&self, config: &AgentSessionConfig) -> Result<Vec<AgentSlashCommand>> {
        let handle = self.servers.acquire(Acquire::Current).await?;
        let client = RawClient::new(handle.transport.clone(), Some(config.cwd.clone()));
        let result = client.commands().await;
        handle.release().await;
        Ok(commands_to_slash(&result.unwrap_or_default()))
    }

    async fn is_available(&self) -> bool {
        let wanted = self
            .opts
            .binary_override()
            .cloned()
            .unwrap_or_else(|| PathBuf::from("opencode"));
        find_executable(&wanted).is_some()
    }

    async fn shutdown(&self) {
        self.servers.shutdown().await;
    }
}
