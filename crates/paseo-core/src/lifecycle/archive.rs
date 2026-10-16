//! Archive, restore and the parent cascade (`agent-manager.ts:1837-2033`, `:2285-2374`,
//! `agent-archive.ts`).
//!
//! All of it runs on the lifecycle lane of the agent it touches; the cascade enters the lane of
//! each child while it holds the parent's, which cannot cycle because the parent is already
//! archived before the scan.

use std::future::Future;
use std::pin::Pin;

use paseo_agent_types::PersistenceHandle;
use paseo_store::agent_record::PersistenceHandle as StoredHandle;
use paseo_store::time::parse_js_date;
use paseo_store::{Nullable, StoredAgentRecord};
use serde_json::json;
use tracing::warn;

use crate::error::{CoreError, Result};
use crate::events::ManagerEvent;
use crate::hooks::{PersistOptions, SnapshotPersister};
use crate::lifecycle::close::describe_hook_agent;
use crate::manager::retired::RetiredInternalAgent;
use crate::manager::AgentManager;
use crate::model::{apply_label_patch, UnarchiveUpdates, PARENT_AGENT_ID_LABEL};
use crate::persistence_sub::StorePersister;
use crate::projection::{
    build_archived_agent_record, from_stored_persistence, to_stored_agent_record,
    view_from_stored_record,
};
use crate::time::iso8601_from_millis;

type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

const OPEN_AGENT_TAB_LABEL_PREFIX: &str = "paseo.open-agent-tab.";

/// `isOpenAgentTabLabel`.
pub fn is_open_agent_tab_label(label: &str) -> bool {
    label.starts_with(OPEN_AGENT_TAB_LABEL_PREFIX)
}

/// `hasOpenAgentTab`.
pub fn has_open_agent_tab(labels: &indexmap::IndexMap<String, String>) -> bool {
    labels
        .iter()
        .any(|(label, value)| is_open_agent_tab_label(label) && value == "true")
}

/// `shouldDetachFromArchivedParent`: cross-workspace children and children with an open tab
/// are detached instead of archived.
fn should_detach_from_archived_parent(
    parent_workspace: Option<&str>,
    child: &StoredAgentRecord,
) -> bool {
    let cross_workspace = matches!(
        (parent_workspace, child.workspace_id.as_deref()),
        (Some(p), Some(c)) if p != c
    );
    cross_workspace || has_open_agent_tab(&child.labels)
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum NativeArchiveState {
    Archive,
    Restore,
}

impl AgentManager {
    pub(crate) fn now_iso(&self) -> String {
        iso8601_from_millis(self.clock().now_ms())
    }

    /// `archiveAgent`: returns `archivedAt`.
    pub async fn archive_agent(&self, agent_id: &str) -> Result<String> {
        let key = agent_id.to_owned();
        self.inner
            .lifecycle_lanes
            .run(&key, || {
                self.archive_agent_unlocked(agent_id.to_owned(), None)
            })
            .await
    }

    pub(crate) fn archive_agent_unlocked(
        &self,
        agent_id: String,
        requested_archived_at: Option<String>,
    ) -> BoxFut<'_, Result<String>> {
        Box::pin(async move {
            let handle = self.require_agent(&agent_id)?;
            let view = handle.view();
            if view.internal {
                // Nothing about an internal agent is on disk and archiving must not put it
                // there: the final snapshot stays in memory for a while (retired agents).
                let archived_at = requested_archived_at.unwrap_or_else(|| self.now_iso());
                let last_message = self.get_last_assistant_message(&agent_id).await;
                let record = build_archived_agent_record(
                    &to_stored_agent_record(&view, view.config.title.clone(), None, Some(true)),
                    &archived_at,
                    Some(&archived_at),
                );
                self.close_agent_runtime(&agent_id).await?;
                self.delete_agent_state(&agent_id);
                self.retire_internal_agent(RetiredInternalAgent {
                    record: record.clone(),
                    last_message,
                });
                self.cascade_archive_children(&record.id, record.workspace_id.as_deref())
                    .await?;
                return Ok(archived_at);
            }
            let store = self
                .store()
                .ok_or_else(|| CoreError::msg("Agent storage is not configured"))?
                .clone();
            StorePersister::new(store.clone())
                .persist(
                    &view,
                    PersistOptions {
                        title: None,
                        internal: Some(Some(view.internal)),
                    },
                )
                .await
                .map_err(CoreError::msg)?;
            let stored = store
                .get(&agent_id)
                .await
                .map_err(|e| CoreError::msg(e.to_string()))?
                .ok_or_else(|| {
                    CoreError::msg(format!(
                        "Agent {agent_id} not found in storage after snapshot"
                    ))
                })?;
            let archived_at = self
                .mark_record_archived(&stored, requested_archived_at)
                .await?;
            let updated_ms = parse_js_date(&archived_at).unwrap_or(0);
            handle
                .ask(move |a| a.view.updated_at_ms = updated_ms)
                .await?;
            // PARITY-QUIRK(archive-closed-snapshot-overwrite): the closed snapshot persisted here
            // overwrites `lastStatus` and attention of the archived record with the live agent's.
            self.close_agent_runtime(&agent_id).await?;
            self.sync_native_archive_state(
                &stored.provider,
                stored.persistence.value(),
                NativeArchiveState::Archive,
            )
            .await?;
            self.discard_retained_agent_state(&agent_id);
            self.cascade_archive_children(&stored.id, stored.workspace_id.as_deref())
                .await?;
            Ok(archived_at)
        })
    }

    /// `cascadeArchiveChildren`: children created through MCP `create_agent` carry the
    /// parent-agent-id label. The first child failure propagates and later children are skipped
    /// (PARITY-QUIRK(cascade-abort-on-first-failure)).
    fn cascade_archive_children<'a>(
        &'a self,
        parent_id: &'a str,
        parent_workspace: Option<&'a str>,
    ) -> BoxFut<'a, Result<()>> {
        Box::pin(async move {
            let Some(store) = self.store().cloned() else {
                return Ok(());
            };
            let map = |e: paseo_store::StoreError| CoreError::msg(e.to_string());
            let is_child = |record: &StoredAgentRecord| {
                !record.is_archived()
                    && record.labels.get(PARENT_AGENT_ID_LABEL).map(String::as_str)
                        == Some(parent_id)
            };
            for listed in store.list().await.map_err(map)? {
                if !is_child(&listed) {
                    continue;
                }
                let Some(child) = store.get(&listed.id).await.map_err(map)? else {
                    continue;
                };
                if !is_child(&child) {
                    continue;
                }
                let child_id = child.id.clone();
                self.inner
                    .lifecycle_lanes
                    .run(&child_id, || async {
                        // Re-read inside the child's lane: an open-tab update or a concurrent
                        // archive may have landed since the scan.
                        let Some(current) = store.get(&child.id).await.map_err(map)? else {
                            return Ok::<(), CoreError>(());
                        };
                        if !is_child(&current) {
                            return Ok(());
                        }
                        if should_detach_from_archived_parent(parent_workspace, &current) {
                            self.detach_agent_unlocked(&current.id).await?;
                        } else if self.inner.registry.get(&current.id).is_some() {
                            self.archive_agent_unlocked(current.id.clone(), None)
                                .await?;
                        } else {
                            self.archive_snapshot_unlocked(&current.id, &self.now_iso())
                                .await?;
                        }
                        Ok(())
                    })
                    .await?;
            }
            // Internal children never reach storage, so the scan above cannot see them. They
            // have no tab to keep open and no listing to detach into: they always archive.
            for handle in self.inner.registry.all() {
                let view = handle.view();
                if !view.internal || view.parent_agent_id() != Some(parent_id) {
                    continue;
                }
                let child_id = view.id.clone();
                self.inner
                    .lifecycle_lanes
                    .run(&child_id, || async {
                        let Some(current) = self.inner.registry.get(&child_id) else {
                            return Ok(());
                        };
                        if current.view().parent_agent_id() != Some(parent_id) {
                            return Ok(());
                        }
                        self.archive_agent_unlocked(child_id.clone(), None).await?;
                        Ok::<(), CoreError>(())
                    })
                    .await?;
            }
            Ok(())
        })
    }

    /// `markRecordArchived`: persist the archived record, announce it, run `on_archived`.
    async fn mark_record_archived(
        &self,
        record: &StoredAgentRecord,
        requested: Option<String>,
    ) -> Result<String> {
        let archived_at = requested.unwrap_or_else(|| self.now_iso());
        let archived = self
            .persist_archived_record(record, &archived_at, Some(&archived_at))
            .await?;
        if self.inner.registry.get(&record.id).is_some() {
            self.notify_agent_state(&record.id).await;
        } else if archived.internal != Some(true) {
            self.dispatch_stored_agent_state(&archived);
        }
        self.fire_agent_archived(&record.id).await;
        Ok(archived_at)
    }

    /// `persistArchivedRecord`.
    async fn persist_archived_record(
        &self,
        record: &StoredAgentRecord,
        archived_at: &str,
        updated_at: Option<&str>,
    ) -> Result<StoredAgentRecord> {
        let archived = build_archived_agent_record(record, archived_at, updated_at);
        self.require_store()?
            .upsert(archived.clone())
            .await
            .map_err(|e| CoreError::msg(e.to_string()))?;
        if !record.is_archived() && record.internal != Some(true) {
            self.hooks().plugin_emit(
                "agent.archived",
                json!({
                    "agent": describe_hook_agent(
                        &archived.id,
                        archived.workspace_id.as_deref(),
                        &archived.labels,
                        &archived.provider,
                        &archived.cwd,
                        archived.title.value().map(String::as_str),
                    ),
                    "archivedAt": archived_at,
                }),
            );
        }
        Ok(archived)
    }

    /// `fireAgentArchived`: awaited, errors logged and swallowed (decision Q19).
    async fn fire_agent_archived(&self, agent_id: &str) {
        if let Err(err) = self.hooks().on_archived(agent_id).await {
            warn!(%err, %agent_id, "onAgentArchived callback failed");
        }
    }

    /// `dispatchStoredAgentState`: a closed `agent_state` built from a stored record.
    pub(crate) fn dispatch_stored_agent_state(&self, record: &StoredAgentRecord) {
        self.bus().emit(ManagerEvent::AgentState {
            agent: std::sync::Arc::new(view_from_stored_record(record)),
        });
    }

    /// `syncNativeArchiveState`: archive is best effort, restore propagates.
    pub(crate) async fn sync_native_archive_state(
        &self,
        provider: &str,
        persistence: Option<&StoredHandle>,
        state: NativeArchiveState,
    ) -> Result<()> {
        let Some(persistence) = persistence else {
            return Ok(());
        };
        let Some(client) = self.client_for(provider) else {
            return Ok(());
        };
        let handle: PersistenceHandle = from_stored_persistence(persistence);
        match state {
            NativeArchiveState::Restore => Ok(client.unarchive_native_session(&handle).await?),
            NativeArchiveState::Archive => {
                if let Err(error) = client.archive_native_session(&handle).await {
                    warn!(%error, %provider, session_id = %handle.session_id, "Failed to archive native session (best-effort)");
                }
                Ok(())
            }
        }
    }

    /// `archiveSnapshot`.
    pub async fn archive_snapshot(
        &self,
        agent_id: &str,
        archived_at: &str,
    ) -> Result<StoredAgentRecord> {
        let key = agent_id.to_owned();
        self.inner
            .lifecycle_lanes
            .run(&key, || {
                self.archive_snapshot_unlocked(agent_id, archived_at)
            })
            .await
    }

    pub(crate) fn archive_snapshot_unlocked<'a>(
        &'a self,
        agent_id: &'a str,
        archived_at: &'a str,
    ) -> BoxFut<'a, Result<StoredAgentRecord>> {
        Box::pin(async move {
            let store = self.require_store()?.clone();
            let map = |e: paseo_store::StoreError| CoreError::msg(e.to_string());
            // A stored-only archive can have waited behind a persisted resume: reuse the live
            // archive transition so the newly acquired runtime is closed as well.
            if self.inner.registry.get(agent_id).is_some() {
                self.archive_agent_unlocked(agent_id.to_owned(), Some(archived_at.to_owned()))
                    .await?;
                return store
                    .get(agent_id)
                    .await
                    .map_err(map)?
                    .ok_or_else(|| CoreError::msg(format!("Agent not found: {agent_id}")));
            }
            let record = store
                .get(agent_id)
                .await
                .map_err(map)?
                .ok_or_else(|| CoreError::msg(format!("Agent not found: {agent_id}")))?;
            let next = self
                .persist_archived_record(&record, archived_at, None)
                .await?;
            self.sync_native_archive_state(
                &record.provider,
                record.persistence.value(),
                NativeArchiveState::Archive,
            )
            .await?;
            self.discard_retained_agent_state(agent_id);
            if next.internal != Some(true) {
                self.dispatch_stored_agent_state(&next);
            }
            self.fire_agent_archived(agent_id).await;
            self.cascade_archive_children(&record.id, record.workspace_id.as_deref())
                .await?;
            Ok(next)
        })
    }

    /// `unarchiveSnapshot`.
    pub async fn unarchive_snapshot(
        &self,
        agent_id: &str,
        updates: UnarchiveUpdates,
    ) -> Result<bool> {
        let key = agent_id.to_owned();
        self.inner
            .lifecycle_lanes
            .run(&key, || self.unarchive_snapshot_unlocked(agent_id, updates))
            .await
    }

    async fn unarchive_snapshot_unlocked(
        &self,
        agent_id: &str,
        updates: UnarchiveUpdates,
    ) -> Result<bool> {
        let store = self.require_store()?.clone();
        let map = |e: paseo_store::StoreError| CoreError::msg(e.to_string());
        let Some(record) = store.get(agent_id).await.map_err(map)? else {
            return Ok(false);
        };
        if !record.is_archived() {
            return Ok(false);
        }
        // Close and native restore share the lane with persisted resume: no new history or
        // interactive runtime can acquire the writer between them.
        if self.inner.registry.get(agent_id).is_some() {
            self.close_agent_runtime(agent_id).await?;
        }
        self.sync_native_archive_state(
            &record.provider,
            record.persistence.value(),
            NativeArchiveState::Restore,
        )
        .await?;
        let mut next = record.clone();
        if let Some(workspace_id) = updates.workspace_id.filter(|w| !w.is_empty()) {
            next.workspace_id = Some(workspace_id);
        }
        if let Some(patch) = &updates.labels {
            next.labels = apply_label_patch(&record.labels, patch);
        }
        next.archived_at = Nullable::Null;
        next.updated_at = self.now_iso();
        store.upsert(next).await.map_err(map)?;
        if self.get_agent(agent_id).is_some() {
            self.notify_agent_state(agent_id).await;
        }
        Ok(true)
    }

    /// `unarchiveSnapshotByHandle`.
    pub async fn unarchive_snapshot_by_handle(&self, handle: &PersistenceHandle) -> Result<()> {
        let store = self.require_store()?.clone();
        let records = store
            .list()
            .await
            .map_err(|e| CoreError::msg(e.to_string()))?;
        let matched = records.iter().find(|record| {
            record
                .persistence
                .value()
                .is_some_and(|p| p.provider == handle.provider && p.session_id == handle.session_id)
        });
        if let Some(matched) = matched {
            self.unarchive_snapshot(&matched.id, UnarchiveUpdates::default())
                .await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use indexmap::IndexMap;

    fn child(workspace: Option<&str>, labels: &[(&str, &str)]) -> StoredAgentRecord {
        let mut record = StoredAgentRecord::new("c", "codex", "/x", "2026-01-01T00:00:00.000Z");
        record.workspace_id = workspace.map(str::to_owned);
        record.labels = labels
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect::<IndexMap<_, _>>();
        record
    }

    #[test]
    fn detach_rules() {
        let open = [("paseo.open-agent-tab.client-1", "true")];
        assert!(should_detach_from_archived_parent(
            Some("w1"),
            &child(Some("w2"), &[])
        ));
        assert!(!should_detach_from_archived_parent(
            Some("w1"),
            &child(Some("w1"), &[])
        ));
        assert!(!should_detach_from_archived_parent(
            None,
            &child(Some("w2"), &[])
        ));
        assert!(!should_detach_from_archived_parent(
            Some("w1"),
            &child(None, &[])
        ));
        assert!(should_detach_from_archived_parent(
            Some("w1"),
            &child(Some("w1"), &open)
        ));
        assert!(!should_detach_from_archived_parent(
            Some("w1"),
            &child(Some("w1"), &[("paseo.open-agent-tab.client-1", "false")])
        ));
    }
}
