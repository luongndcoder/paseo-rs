//! `ProviderSubagentStore` (`provider-subagents/store.ts`): volatile descriptors and timelines
//! of provider-managed children, keyed `parent\0sub`. Shared by every actor of one manager.

use std::collections::HashMap;

use paseo_agent_types::{AgentTimelineItem, ProviderSubagentInputEvent, ProviderSubagentStatus};
use paseo_timeline::limits::limit_item_content;
use paseo_timeline::store::{AppendOptions, FetchOptions, FetchResult, SeedOptions};
use paseo_timeline::{Row, TimelineState};

#[derive(Debug, Clone, PartialEq)]
pub struct SubagentDescriptor {
    pub id: String,
    pub parent_agent_id: String,
    /// Direct provider-subagent parent. `None` is a child of the managed agent.
    pub parent_subagent_id: Option<String>,
    pub provider: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub status: ProviderSubagentStatus,
    pub created_at: String,
    pub updated_at: String,
    pub tool_call_id: Option<String>,
    pub cwd: Option<String>,
    pub subtitle: Option<String>,
}

/// `ProviderSubagentStoreEvent`.
#[derive(Debug, Clone, PartialEq)]
pub enum SubagentStoreEvent {
    Upsert {
        subagent: Box<SubagentDescriptor>,
    },
    Timeline {
        parent_agent_id: String,
        subagent_id: String,
        provider: String,
        row: Box<Row>,
        epoch: String,
    },
    Remove {
        parent_agent_id: String,
        subagent_id: String,
    },
}

impl SubagentStoreEvent {
    pub fn parent_agent_id(&self) -> &str {
        match self {
            Self::Upsert { subagent } => &subagent.parent_agent_id,
            Self::Timeline {
                parent_agent_id, ..
            }
            | Self::Remove {
                parent_agent_id, ..
            } => parent_agent_id,
        }
    }
}

fn key(parent: &str, sub: &str) -> String {
    format!("{parent}\0{sub}")
}

/// Sticky upsert field: omitted keeps the stored value, an explicit null clears it.
fn sticky(next: &Option<Option<String>>, previous: Option<&Option<String>>) -> Option<String> {
    match next {
        None => previous.cloned().flatten(),
        Some(value) => value.clone(),
    }
}

#[derive(Debug, Default)]
pub struct ProviderSubagentStore {
    descriptors: HashMap<String, SubagentDescriptor>,
    timelines: HashMap<String, TimelineState>,
    /// Insertion order of descriptors, for stable `list_all`.
    order: Vec<String>,
}

impl ProviderSubagentStore {
    pub fn apply(
        &mut self,
        parent: &str,
        provider: &str,
        event: &ProviderSubagentInputEvent,
        now_iso: &str,
    ) -> SubagentStoreEvent {
        match event {
            ProviderSubagentInputEvent::Remove { id } => {
                let k = key(parent, id);
                self.descriptors.remove(&k);
                self.timelines.remove(&k);
                self.order.retain(|o| o != &k);
                SubagentStoreEvent::Remove {
                    parent_agent_id: parent.to_owned(),
                    subagent_id: id.clone(),
                }
            }
            ProviderSubagentInputEvent::Timeline {
                id,
                item,
                timestamp,
            } => {
                let k = key(parent, id);
                let timeline = self
                    .timelines
                    .entry(k)
                    .or_insert_with(|| TimelineState::initialize(SeedOptions::default()));
                let row = timeline.append(
                    limit_item_content((**item).clone()),
                    AppendOptions {
                        timestamp: Some(timestamp.clone().unwrap_or_else(|| now_iso.to_owned())),
                        ..AppendOptions::default()
                    },
                );
                SubagentStoreEvent::Timeline {
                    parent_agent_id: parent.to_owned(),
                    subagent_id: id.clone(),
                    provider: provider.to_owned(),
                    row: Box::new(row),
                    epoch: timeline.epoch().to_owned(),
                }
            }
            ProviderSubagentInputEvent::Upsert {
                id,
                title,
                description,
                status,
                tool_call_id,
                cwd,
                subtitle,
                parent_subagent_id,
                timestamp,
            } => {
                let k = key(parent, id);
                self.timelines
                    .entry(k.clone())
                    .or_insert_with(|| TimelineState::initialize(SeedOptions::default()));
                let previous = self.descriptors.get(&k);
                let timestamp = timestamp.clone().unwrap_or_else(|| now_iso.to_owned());
                let prev = |f: fn(&SubagentDescriptor) -> &Option<String>| previous.map(f);
                let subagent = SubagentDescriptor {
                    id: id.clone(),
                    parent_agent_id: parent.to_owned(),
                    provider: provider.to_owned(),
                    title: sticky(title, prev(|d| &d.title)),
                    description: sticky(description, prev(|d| &d.description)),
                    status: status
                        .or_else(|| previous.map(|p| p.status))
                        .unwrap_or(ProviderSubagentStatus::Running),
                    created_at: previous
                        .map_or_else(|| timestamp.clone(), |p| p.created_at.clone()),
                    updated_at: timestamp,
                    tool_call_id: sticky(tool_call_id, prev(|d| &d.tool_call_id)),
                    cwd: sticky(cwd, prev(|d| &d.cwd)),
                    subtitle: sticky(subtitle, prev(|d| &d.subtitle)),
                    parent_subagent_id: sticky(parent_subagent_id, prev(|d| &d.parent_subagent_id)),
                };
                if !self.descriptors.contains_key(&k) {
                    self.order.push(k.clone());
                }
                self.descriptors.insert(k, subagent.clone());
                SubagentStoreEvent::Upsert {
                    subagent: Box::new(subagent),
                }
            }
        }
    }

    /// Children of one parent, sorted by `created_at` (stable on ties).
    pub fn list(&self, parent: &str) -> Vec<SubagentDescriptor> {
        let mut list: Vec<_> = self
            .order
            .iter()
            .filter_map(|k| self.descriptors.get(k))
            .filter(|d| d.parent_agent_id == parent)
            .cloned()
            .collect();
        list.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        list
    }

    pub fn list_all(&self) -> Vec<SubagentDescriptor> {
        self.order
            .iter()
            .filter_map(|k| self.descriptors.get(k))
            .cloned()
            .collect()
    }

    pub fn get(&self, parent: &str, sub: &str) -> Option<SubagentDescriptor> {
        self.descriptors.get(&key(parent, sub)).cloned()
    }

    pub fn fetch_timeline(
        &self,
        parent: &str,
        sub: &str,
        options: &FetchOptions,
    ) -> Option<FetchResult> {
        self.timelines
            .get(&key(parent, sub))
            .map(|t| t.fetch(options))
    }

    pub fn timeline_items(&self, parent: &str, sub: &str) -> Vec<AgentTimelineItem> {
        self.timelines
            .get(&key(parent, sub))
            .map(TimelineState::items)
            .unwrap_or_default()
    }

    /// `deleteParent`: remove events for every child of `parent`.
    pub fn delete_parent(&mut self, parent: &str) -> Vec<SubagentStoreEvent> {
        let children = self.list(parent);
        children
            .into_iter()
            .map(|child| {
                let k = key(parent, &child.id);
                self.descriptors.remove(&k);
                self.timelines.remove(&k);
                self.order.retain(|o| o != &k);
                SubagentStoreEvent::Remove {
                    parent_agent_id: parent.to_owned(),
                    subagent_id: child.id,
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn upsert(
        id: &str,
        title: Option<Option<&str>>,
        status: Option<ProviderSubagentStatus>,
        timestamp: Option<&str>,
    ) -> ProviderSubagentInputEvent {
        ProviderSubagentInputEvent::Upsert {
            id: id.into(),
            title: title.map(|t| t.map(str::to_owned)),
            description: None,
            status,
            tool_call_id: None,
            cwd: None,
            subtitle: None,
            parent_subagent_id: None,
            timestamp: timestamp.map(str::to_owned),
        }
    }

    #[test]
    fn upsert_is_sticky_and_status_defaults_to_running() {
        let mut store = ProviderSubagentStore::default();
        let first = upsert(
            "a",
            Some(Some("T")),
            Some(ProviderSubagentStatus::Completed),
            Some("t1"),
        );
        store.apply("p", "codex", &first, "now");
        // A presentation-only upsert must not revert a finished child or blank its title.
        let SubagentStoreEvent::Upsert { subagent } =
            store.apply("p", "codex", &upsert("a", None, None, None), "now2")
        else {
            panic!("expected upsert")
        };
        assert_eq!(subagent.status, ProviderSubagentStatus::Completed);
        assert_eq!(subagent.title.as_deref(), Some("T"));
        assert_eq!(subagent.created_at, "t1");
        assert_eq!(subagent.updated_at, "now2");
        // An explicit null clears the title.
        let SubagentStoreEvent::Upsert { subagent } =
            store.apply("p", "codex", &upsert("a", Some(None), None, None), "now3")
        else {
            panic!("expected upsert")
        };
        assert_eq!(subagent.title, None);
        let SubagentStoreEvent::Upsert { subagent } =
            store.apply("p", "codex", &upsert("fresh", None, None, None), "now4")
        else {
            panic!("expected upsert")
        };
        assert_eq!(subagent.status, ProviderSubagentStatus::Running);
    }

    #[test]
    fn delete_parent_removes_only_that_parents_children() {
        let mut store = ProviderSubagentStore::default();
        store.apply("p1", "codex", &upsert("a", None, None, None), "t");
        store.apply("p2", "codex", &upsert("b", None, None, None), "t");
        let removed = store.delete_parent("p1");
        assert_eq!(removed.len(), 1);
        assert!(store.list("p1").is_empty());
        assert_eq!(store.list("p2").len(), 1);
    }

    fn timeline_input(
        id: &str,
        item: AgentTimelineItem,
        timestamp: Option<&str>,
    ) -> ProviderSubagentInputEvent {
        ProviderSubagentInputEvent::Timeline {
            id: id.into(),
            item: Box::new(item),
            timestamp: timestamp.map(str::to_owned),
        }
    }

    fn assistant(text: &str) -> AgentTimelineItem {
        AgentTimelineItem::AssistantMessage {
            text: text.into(),
            message_id: None,
        }
    }

    #[test]
    fn keeps_provider_children_and_their_timelines_scoped_to_the_parent_agent() {
        let mut store = ProviderSubagentStore::default();
        let mut first = upsert(
            "child-1",
            Some(Some("Explore")),
            Some(ProviderSubagentStatus::Running),
            Some("2026-07-12T10:00:00.000Z"),
        );
        if let ProviderSubagentInputEvent::Upsert { cwd, .. } = &mut first {
            *cwd = Some(Some("/workspace/child".into()));
        }
        store.apply("parent-a", "codex", &first, "now");
        store.apply(
            "parent-a",
            "codex",
            &timeline_input(
                "child-1",
                assistant("Found it."),
                Some("2026-07-12T10:00:01.000Z"),
            ),
            "now",
        );
        store.apply(
            "parent-a",
            "codex",
            &upsert(
                "child-1",
                None,
                Some(ProviderSubagentStatus::Completed),
                Some("2026-07-12T10:00:02.000Z"),
            ),
            "now",
        );
        store.apply(
            "parent-b",
            "claude",
            &upsert(
                "child-1",
                Some(Some("Review")),
                Some(ProviderSubagentStatus::Running),
                Some("2026-07-12T10:00:03.000Z"),
            ),
            "now",
        );

        let listed = store.list("parent-a");
        assert_eq!(listed.len(), 1);
        let child = &listed[0];
        assert_eq!(child.parent_agent_id, "parent-a");
        assert_eq!(child.provider, "codex");
        assert_eq!(child.title.as_deref(), Some("Explore"));
        assert_eq!(child.cwd.as_deref(), Some("/workspace/child"));
        assert_eq!(child.status, ProviderSubagentStatus::Completed);
        assert_eq!(child.created_at, "2026-07-12T10:00:00.000Z");
        assert_eq!(child.updated_at, "2026-07-12T10:00:02.000Z");
        let rows = store
            .fetch_timeline("parent-a", "child-1", &FetchOptions::default())
            .unwrap()
            .rows;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].seq(), 1);
        assert_eq!(rows[0].timestamp, "2026-07-12T10:00:01.000Z");
        assert_eq!(rows[0].item, assistant("Found it."));
        assert_eq!(store.list("parent-b")[0].provider, "claude");
        assert_eq!(store.list("parent-b")[0].title.as_deref(), Some("Review"));
        assert_eq!(
            store.delete_parent("parent-a"),
            vec![SubagentStoreEvent::Remove {
                parent_agent_id: "parent-a".into(),
                subagent_id: "child-1".into()
            }]
        );
        assert!(store.list("parent-a").is_empty());
        assert_eq!(store.list("parent-b").len(), 1);
    }

    #[test]
    fn limits_oversized_provider_child_tool_output_before_storage() {
        use paseo_agent_types::{ToolCallDetail, ToolCallItem, ToolCallStatus};
        let mut store = ProviderSubagentStore::default();
        let call = ToolCallItem::new(
            "call-1",
            "shell",
            ToolCallStatus::Completed,
            ToolCallDetail::Shell {
                command: "print".into(),
                cwd: None,
                output: Some("x".repeat(70 * 1024)),
                exit_code: None,
            },
        );
        let update = store.apply(
            "parent-a",
            "opencode",
            &timeline_input("child-1", AgentTimelineItem::tool_call(call), None),
            "now",
        );
        assert!(matches!(update, SubagentStoreEvent::Timeline { .. }));
        let rows = store
            .fetch_timeline("parent-a", "child-1", &FetchOptions::default())
            .unwrap()
            .rows;
        let AgentTimelineItem::ToolCall(call) = &rows[0].item else {
            panic!("tool call row")
        };
        let ToolCallDetail::Shell { output, .. } = &call.detail else {
            panic!("shell detail")
        };
        assert_eq!(output.as_deref(), Some("x".repeat(64 * 1024).as_str()));
    }

    #[test]
    fn pages_provider_history_on_projected_item_boundaries() {
        let mut store = ProviderSubagentStore::default();
        for index in 0..101 {
            store.apply(
                "parent-a",
                "opencode",
                &timeline_input("child-1", assistant(&index.to_string()), None),
                "now",
            );
        }
        let page = store
            .fetch_timeline(
                "parent-a",
                "child-1",
                &FetchOptions {
                    limit: Some(1),
                    ..FetchOptions::default()
                },
            )
            .unwrap();
        assert_eq!(page.rows.len(), 1);
        assert_eq!(page.rows[0].seq_start, 1);
        let expected: String = (0..101).map(|i| i.to_string()).collect();
        assert_eq!(page.rows[0].item, assistant(&expected));
        assert_eq!(page.rows.last().unwrap().seq(), 101);
        assert!(!page.has_older);
    }
}
