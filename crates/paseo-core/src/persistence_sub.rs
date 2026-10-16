//! The persistence subscriber: a [`SnapshotPersister`] over [`AgentStore`] that projects the
//! live view into a stored record inside the agent's write lane (`toStoredAgentRecord`).
//!
//! TS projects when the store lane runs, not at enqueue; here the view is cloned at enqueue and
//! the per-agent FIFO of the store keeps the final file identical (only intermediate writes
//! differ), as spec section 3 describes.

use std::sync::Arc;

use async_trait::async_trait;
use paseo_store::{AgentStore, SnapshotOptions};

use crate::hooks::{PersistOptions, SnapshotPersister};
use crate::model::ManagedAgentView;
use crate::projection::to_stored_agent_record;

pub struct StorePersister {
    store: Arc<AgentStore>,
}

impl StorePersister {
    pub fn new(store: Arc<AgentStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl SnapshotPersister for StorePersister {
    async fn persist(
        &self,
        agent: &ManagedAgentView,
        options: PersistOptions,
    ) -> Result<(), String> {
        let view = agent.clone();
        self.store
            .apply_snapshot(
                &agent.id,
                SnapshotOptions {
                    title: options.title,
                    internal: options.internal,
                },
                move |input| {
                    let internal = input.resolve_internal(Some(view.internal));
                    to_stored_agent_record(&view, input.title, input.created_at, internal)
                },
            )
            .await
            .map_err(|e| e.to_string())
    }

    async fn stored_title(&self, agent_id: &str) -> Result<Option<Option<String>>, String> {
        let record = self.store.get(agent_id).await.map_err(|e| e.to_string())?;
        Ok(record.map(|r| r.title.value().cloned()))
    }
}
