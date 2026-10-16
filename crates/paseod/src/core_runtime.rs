//! Builds the Rust core's services from the edge configuration (`PASEO_CORE=rust`).
//!
//! Layout of `$PASEO_HOME` (the TS daemon's, byte-compatible with `paseo-store`):
//! `agents/` (agent records), `projects/projects.json`, `projects/workspaces.json`.
//!
//! The runtime is the Rust side of the strangler: one [`DirectorySync`] per boot, one
//! [`AgentManager`], one [`SessionRegistry`]. Provider adapters are injected
//! ([`ProviderRegistryUpdate`]): the provider host / native providers arrive with the later work
//! packages, and until a provider is registered its stored agents are not listed (the same rule
//! as TS for unregistered providers).

use std::path::Path;
use std::sync::Arc;

use paseo_core::session::{NoSessionHooks, SessionHooks, SessionRegistry, SessionServices};
use paseo_core::time::SystemClock;
use paseo_core::{AgentManager, ManagerOptions, ProviderRegistryUpdate, StorePersister};
use paseo_directory::DirectorySync;
use paseo_store::{AgentStore, ProjectRegistry, WorkspaceRegistry};

use crate::client_registry::{ClientRegistry, CoreDispatcher};
use crate::core_routes;
use crate::routing::OwnedTypes;

/// Everything the edge needs to route owned message types to the core.
pub struct CoreRuntime {
    pub services: SessionServices,
    pub sessions: SessionRegistry,
    pub clients: Arc<ClientRegistry>,
    pub dispatcher: Arc<CoreDispatcher>,
}

pub struct CoreRuntimeOptions {
    pub providers: ProviderRegistryUpdate,
    pub hooks: Arc<dyn SessionHooks>,
}

impl Default for CoreRuntimeOptions {
    fn default() -> Self {
        Self {
            providers: ProviderRegistryUpdate::default(),
            hooks: Arc::new(NoSessionHooks),
        }
    }
}

impl CoreRuntime {
    /// Opens the stores under `paseo_home` and starts the manager. Must run inside a tokio
    /// runtime.
    pub async fn build(
        paseo_home: &Path,
        options: CoreRuntimeOptions,
    ) -> Result<Self, paseo_store::StoreError> {
        let store = Arc::new(AgentStore::new(paseo_home.join("agents")));
        store.initialize().await?;
        let projects_dir = paseo_home.join("projects");
        let projects = Arc::new(ProjectRegistry::new(projects_dir.join("projects.json")));
        projects.initialize().await;
        let workspaces = Arc::new(WorkspaceRegistry::new(projects_dir.join("workspaces.json")));
        workspaces.initialize().await;
        let manager = AgentManager::new(ManagerOptions {
            store: Some(store.clone()),
            persister: Some(Arc::new(StorePersister::new(store.clone()))),
            providers: options.providers,
            ..ManagerOptions::default()
        });
        let services = SessionServices {
            manager,
            store,
            workspaces,
            projects,
            directory: Arc::new(DirectorySync::with_random_generation()),
            hooks: options.hooks,
            clock: Arc::new(SystemClock),
        };
        Ok(Self::from_services(services))
    }

    pub fn from_services(services: SessionServices) -> Self {
        let sessions = SessionRegistry::new(services.clone());
        let clients = Arc::new(ClientRegistry::new(sessions.clone()));
        let dispatcher = CoreDispatcher::new(clients.clone());
        Self {
            services,
            sessions,
            clients,
            dispatcher,
        }
    }

    /// Ends every session, then lets the manager finish what it has in flight.
    pub async fn shutdown(&self) {
        self.sessions.shutdown_all().await;
        self.services.manager.prepare_for_shutdown();
        self.services.manager.flush_for_shutdown().await;
    }

    /// The claim table for the implemented slices.
    pub fn owned_types(&self) -> OwnedTypes {
        core_routes::implemented()
    }
}
