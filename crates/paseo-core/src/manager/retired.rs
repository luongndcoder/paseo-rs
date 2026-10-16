//! Retired internal agents: the archived snapshot of an internal agent, readable by id for a
//! while after archive (storage plays this role for public agents; internal agents never reach
//! it). FIFO by insertion, not LRU; 500 entries; 10 minute TTL; expired entries are pruned on
//! read and on insert (`agent-manager.ts:739-760`, `:1729-1759`).

use indexmap::IndexMap;
use paseo_store::StoredAgentRecord;

use super::AgentManager;

pub const RETIRED_INTERNAL_AGENT_TTL_MS: i64 = 10 * 60 * 1000;
pub const RETIRED_INTERNAL_AGENT_LIMIT: usize = 500;

/// `RetiredInternalAgent`.
#[derive(Debug, Clone, PartialEq)]
pub struct RetiredInternalAgent {
    pub record: StoredAgentRecord,
    pub last_message: Option<String>,
}

#[derive(Debug, Default)]
pub(crate) struct RetiredInternal {
    entries: IndexMap<String, (RetiredInternalAgent, i64)>,
}

impl RetiredInternal {
    fn prune(&mut self, now_ms: i64) {
        self.entries.retain(|_, (_, expires)| *expires > now_ms);
    }

    pub fn insert(&mut self, entry: RetiredInternalAgent, now_ms: i64) {
        self.prune(now_ms);
        while self.entries.len() >= RETIRED_INTERNAL_AGENT_LIMIT {
            if self.entries.shift_remove_index(0).is_none() {
                break;
            }
        }
        self.entries.insert(
            entry.record.id.clone(),
            (entry, now_ms + RETIRED_INTERNAL_AGENT_TTL_MS),
        );
    }

    pub fn get(&mut self, id: &str, now_ms: i64) -> Option<RetiredInternalAgent> {
        let (entry, expires) = self.entries.get(id)?;
        if *expires <= now_ms {
            self.entries.shift_remove(id);
            return None;
        }
        Some(entry.clone())
    }

    pub fn list(&mut self, now_ms: i64) -> Vec<RetiredInternalAgent> {
        self.prune(now_ms);
        self.entries
            .values()
            .map(|(entry, _)| entry.clone())
            .collect()
    }
}

impl AgentManager {
    fn retired(&self) -> std::sync::MutexGuard<'_, RetiredInternal> {
        self.inner.retired.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `getRetiredInternalAgent`: `None` once expired or for an agent that was never internal.
    pub fn get_retired_internal_agent(&self, id: &str) -> Option<RetiredInternalAgent> {
        let now = self.clock().now_ms();
        self.retired().get(id, now)
    }

    /// `listRetiredInternalAgents`.
    pub fn list_retired_internal_agents(&self) -> Vec<RetiredInternalAgent> {
        let now = self.clock().now_ms();
        self.retired().list(now)
    }

    /// `retireInternalAgent`.
    pub(crate) fn retire_internal_agent(&self, entry: RetiredInternalAgent) {
        let now = self.clock().now_ms();
        self.retired().insert(entry, now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str) -> RetiredInternalAgent {
        RetiredInternalAgent {
            record: StoredAgentRecord::new(id, "codex", "/x", "2026-01-01T00:00:00.000Z"),
            last_message: None,
        }
    }

    #[test]
    fn expired_entries_are_pruned_on_read_and_insert() {
        let mut retired = RetiredInternal::default();
        retired.insert(entry("a"), 0);
        assert!(retired
            .get("a", RETIRED_INTERNAL_AGENT_TTL_MS - 1)
            .is_some());
        assert!(retired.get("a", RETIRED_INTERNAL_AGENT_TTL_MS).is_none());
        retired.insert(entry("b"), 0);
        retired.insert(entry("c"), RETIRED_INTERNAL_AGENT_TTL_MS);
        assert_eq!(retired.entries.len(), 1, "insert pruned the expired entry");
    }

    #[test]
    fn eviction_is_fifo_not_lru() {
        let mut retired = RetiredInternal::default();
        for i in 0..RETIRED_INTERNAL_AGENT_LIMIT {
            retired.insert(entry(&format!("a{i}")), 0);
        }
        // Reading the oldest does not refresh it.
        assert!(retired.get("a0", 1).is_some());
        retired.insert(entry("new"), 1);
        assert!(retired.get("a0", 1).is_none());
        assert!(retired.get("a1", 1).is_some());
        assert!(retired.get("new", 1).is_some());
        assert_eq!(retired.entries.len(), RETIRED_INTERNAL_AGENT_LIMIT);
    }

    #[test]
    fn reinserting_an_id_keeps_its_queue_position() {
        let mut retired = RetiredInternal::default();
        retired.insert(entry("a"), 0);
        retired.insert(entry("b"), 0);
        retired.insert(entry("a"), 5);
        let ids: Vec<_> = retired.list(5).into_iter().map(|e| e.record.id).collect();
        assert_eq!(ids, ["a", "b"]);
    }
}
