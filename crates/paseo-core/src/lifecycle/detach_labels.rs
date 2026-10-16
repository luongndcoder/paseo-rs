//! Titles, labels, metadata and detach (`agent-manager.ts:2118-2250`, `:2376-2425`).
//!
//! Parentage is the label `paseo.parent-agent-id`; detach clears it plus every open-tab label.

use indexmap::IndexMap;
use paseo_store::time::parse_js_date;
use paseo_store::{Nullable, StoredAgentRecord};

use crate::error::{CoreError, Result};
use crate::hooks::PersistOptions;
use crate::lifecycle::archive::is_open_agent_tab_label;
use crate::manager::AgentManager;
use crate::model::{
    apply_label_patch, label_patch_of, DetachResult, LabelPatch, PARENT_AGENT_ID_LABEL,
};

/// `getParentAgentIdFromLabels`.
fn parent_from_labels(labels: &IndexMap<String, String>) -> Option<String> {
    labels
        .get(PARENT_AGENT_ID_LABEL)
        .map(|v| v.trim())
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
}

/// `detachedAgentLabelPatch`: the parent label and every open-tab label are removed.
pub fn detached_agent_label_patch(labels: &IndexMap<String, String>) -> LabelPatch {
    let mut patch = LabelPatch::new();
    patch.insert(PARENT_AGENT_ID_LABEL.to_owned(), None);
    for label in labels.keys() {
        if is_open_agent_tab_label(label) {
            patch.insert(label.clone(), None);
        }
    }
    patch
}

/// `AgentMetadataPatch` of `updateAgentMetadata`.
#[derive(Debug, Clone, Default)]
pub struct MetadataUpdates {
    pub title: Option<String>,
    pub labels: Option<IndexMap<String, String>>,
}

struct WrittenLabels {
    record: Option<StoredAgentRecord>,
    #[allow(dead_code)]
    live: bool,
}

fn store_error(error: paseo_store::StoreError) -> CoreError {
    CoreError::msg(error.to_string())
}

impl AgentManager {
    /// `nextStoredUpdatedAt`: `max(now, previous + 1)` for a record that has no live agent.
    pub(crate) fn next_stored_updated_at(&self, record: &StoredAgentRecord) -> String {
        let previous = parse_js_date(&record.updated_at).unwrap_or(0);
        let next = crate::time::next_updated_at(previous, self.clock().now_ms());
        crate::time::iso8601_from_millis(next)
    }

    /// `setTitle`: blank titles are ignored; the title lands in the same snapshot write as the
    /// `updatedAt` bump.
    pub async fn set_title(&self, agent_id: &str, title: &str) -> Result<()> {
        let handle = self.require_agent(agent_id)?;
        let normalized = title.trim().to_owned();
        if normalized.is_empty() {
            return Ok(());
        }
        handle
            .ask(|a| {
                a.touch_updated_at();
            })
            .await?;
        self.persist_snapshot(
            &handle,
            PersistOptions {
                title: Some(Some(normalized)),
                internal: None,
            },
        )
        .await?;
        handle.ask(|a| a.emit_state(false)).await
    }

    /// `setLabels`: merges and persists, on the lifecycle lane.
    pub async fn set_labels(&self, agent_id: &str, labels: IndexMap<String, String>) -> Result<()> {
        let key = agent_id.to_owned();
        self.inner
            .lifecycle_lanes
            .run(&key, || async {
                let handle = self.require_agent(agent_id)?;
                self.write_labels(&handle.id, &label_patch_of(&labels))
                    .await?;
                Ok(())
            })
            .await
    }

    /// `writeLabels`: a live agent is patched, touched, persisted and re-emitted; otherwise the
    /// stored record is rewritten.
    async fn write_labels(&self, agent_id: &str, patch: &LabelPatch) -> Result<WrittenLabels> {
        if let Some(handle) = self.inner.registry.get(agent_id) {
            let patch = patch.clone();
            handle
                .ask(move |a| {
                    a.view.labels = apply_label_patch(&a.view.labels, &patch);
                    a.touch_updated_at();
                })
                .await?;
            self.persist_snapshot(&handle, PersistOptions::default())
                .await?;
            handle.ask(|a| a.emit_state(false)).await?;
            let record = match self.store() {
                Some(store) => store.get(agent_id).await.map_err(store_error)?,
                None => None,
            };
            return Ok(WrittenLabels { record, live: true });
        }
        let record = self
            .write_stored_metadata(agent_id, None, Some(patch))
            .await?;
        Ok(WrittenLabels {
            record: Some(record),
            live: false,
        })
    }

    /// `writeStoredMetadata`.
    async fn write_stored_metadata(
        &self,
        agent_id: &str,
        title: Option<&str>,
        labels: Option<&LabelPatch>,
    ) -> Result<StoredAgentRecord> {
        let store = self.require_store()?.clone();
        let record = store
            .get(agent_id)
            .await
            .map_err(store_error)?
            .ok_or_else(|| CoreError::msg(format!("Agent not found: {agent_id}")))?;
        let mut next = record.clone();
        if let Some(title) = title.filter(|t| !t.is_empty()) {
            next.title = Nullable::Value(title.to_owned());
        }
        if let Some(patch) = labels {
            next.labels = apply_label_patch(&record.labels, patch);
        }
        next.updated_at = self.next_stored_updated_at(&record);
        store.upsert(next.clone()).await.map_err(store_error)?;
        Ok(next)
    }

    /// `updateAgentMetadata`: on the lifecycle lane.
    pub async fn update_agent_metadata(
        &self,
        agent_id: &str,
        updates: MetadataUpdates,
    ) -> Result<()> {
        let key = agent_id.to_owned();
        self.inner
            .lifecycle_lanes
            .run(&key, || async {
                if self.get_agent(agent_id).is_some() {
                    if let Some(title) = updates.title.as_deref().filter(|t| !t.is_empty()) {
                        self.set_title(agent_id, title).await?;
                    }
                    if let Some(labels) = &updates.labels {
                        self.write_labels(agent_id, &label_patch_of(labels)).await?;
                    }
                    return Ok(());
                }
                let patch = updates.labels.as_ref().map(label_patch_of);
                self.write_stored_metadata(agent_id, updates.title.as_deref(), patch.as_ref())
                    .await?;
                Ok(())
            })
            .await
    }

    /// `detachAgent`: on the lifecycle lane.
    pub async fn detach_agent(&self, agent_id: &str) -> Result<DetachResult> {
        let key = agent_id.to_owned();
        self.inner
            .lifecycle_lanes
            .run(&key, || self.detach_agent_unlocked(agent_id))
            .await
    }

    pub(crate) async fn detach_agent_unlocked(&self, agent_id: &str) -> Result<DetachResult> {
        let store = self.require_store()?.clone();
        let missing_after = || {
            CoreError::msg(format!(
                "Agent not found in storage after detach: {agent_id}"
            ))
        };
        if let Some(handle) = self.inner.registry.get(agent_id) {
            let labels = handle.view().labels.clone();
            let Some(previous) = parent_from_labels(&labels) else {
                self.persist_snapshot(&handle, PersistOptions::default())
                    .await?;
                let record = store
                    .get(agent_id)
                    .await
                    .map_err(store_error)?
                    .ok_or_else(missing_after)?;
                return Ok(DetachResult {
                    record,
                    live: true,
                    previous_parent_agent_id: None,
                });
            };
            let written = self
                .write_labels(agent_id, &detached_agent_label_patch(&labels))
                .await?;
            let record = written.record.ok_or_else(missing_after)?;
            return Ok(DetachResult {
                record,
                live: true,
                previous_parent_agent_id: Some(previous),
            });
        }
        let record = store
            .get(agent_id)
            .await
            .map_err(store_error)?
            .ok_or_else(|| CoreError::msg(format!("Agent not found: {agent_id}")))?;
        let Some(previous) = parent_from_labels(&record.labels) else {
            return Ok(DetachResult {
                record,
                live: false,
                previous_parent_agent_id: None,
            });
        };
        let written = self
            .write_labels(agent_id, &detached_agent_label_patch(&record.labels))
            .await?;
        Ok(DetachResult {
            record: written.record.ok_or_else(missing_after)?,
            live: false,
            previous_parent_agent_id: Some(previous),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detach_patch_clears_parent_and_open_tab_labels_only() {
        let labels: IndexMap<String, String> = [
            (PARENT_AGENT_ID_LABEL, "p"),
            ("paseo.open-agent-tab.c1", "true"),
            ("keep", "me"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
        let patch = detached_agent_label_patch(&labels);
        assert_eq!(
            patch.keys().map(String::as_str).collect::<Vec<_>>(),
            [PARENT_AGENT_ID_LABEL, "paseo.open-agent-tab.c1"]
        );
        assert!(patch.values().all(Option::is_none));
        let next = apply_label_patch(&labels, &patch);
        assert_eq!(next.len(), 1);
        assert_eq!(next["keep"], "me");
    }

    #[test]
    fn parent_label_is_trimmed_and_blank_is_none() {
        let mut labels = IndexMap::new();
        labels.insert(PARENT_AGENT_ID_LABEL.to_owned(), "  p1 ".to_owned());
        assert_eq!(parent_from_labels(&labels).as_deref(), Some("p1"));
        labels.insert(PARENT_AGENT_ID_LABEL.to_owned(), "   ".to_owned());
        assert_eq!(parent_from_labels(&labels), None);
    }
}
