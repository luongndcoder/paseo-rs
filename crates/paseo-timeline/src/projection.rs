//! Projection of canonical rows into display entries (`timeline-projection.ts`).
//!
//! Invariants: `seq == seq_end`; tool-call updates with the same callId and turnId merge into the
//! first row; plugin items replace the item but keep `seq_start`; text merges only into the last
//! row when seq-adjacent. Re-projecting projected entries is idempotent.

use std::collections::HashMap;

use paseo_agent_types::AgentTimelineItem;
use paseo_tool_detail::merge_tool_call_items;

use crate::model::{CollapseKind, ProjectedEntry, ProjectionMode, Row, SeqRange};

/// Merge `incoming` ranges into `existing`. Arithmetic form of the TS per-seq loop: each seq is
/// compared against the LAST range only, so an incoming range starting at or before
/// `last.end + 1` extends it (start kept), otherwise it is appended.
pub fn merge_seq_ranges(existing: &[SeqRange], incoming: &[SeqRange]) -> Vec<SeqRange> {
    let mut merged = existing.to_vec();
    for range in incoming {
        match merged.last_mut() {
            Some(last) if range.start_seq <= last.end_seq + 1 => {
                last.end_seq = last.end_seq.max(range.end_seq);
            }
            _ => merged.push(*range),
        }
    }
    merged
}

fn with_collapse(
    existing: &ProjectedEntry,
    entry: &ProjectedEntry,
    kind: CollapseKind,
) -> (Vec<SeqRange>, Vec<CollapseKind>) {
    let mut collapsed = existing.collapsed.clone();
    if !collapsed.contains(&kind) {
        collapsed.push(kind);
    }
    (
        merge_seq_ranges(&existing.source_seq_ranges, &entry.source_seq_ranges),
        collapsed,
    )
}

/// `mergeIdentityEntries`: fold `entry` into `existing` when both share an identity.
pub(crate) fn merge_identity_entries(
    existing: &ProjectedEntry,
    entry: &ProjectedEntry,
) -> Option<ProjectedEntry> {
    match &entry.item {
        AgentTimelineItem::ToolCall(incoming) => {
            let AgentTimelineItem::ToolCall(prior) = &existing.item else {
                return None;
            };
            if existing.turn_id != entry.turn_id {
                return None;
            }
            let (ranges, collapsed) = with_collapse(existing, entry, CollapseKind::ToolLifecycle);
            Some(ProjectedEntry {
                item: AgentTimelineItem::tool_call(merge_tool_call_items(prior, incoming)),
                timestamp: entry.timestamp.clone(),
                seq_end: existing.seq_end.max(entry.seq_end),
                source_seq_ranges: ranges,
                collapsed,
                ..existing.clone()
            })
        }
        AgentTimelineItem::Plugin { .. } => {
            if !matches!(existing.item, AgentTimelineItem::Plugin { .. }) {
                return None;
            }
            let (ranges, collapsed) = with_collapse(existing, entry, CollapseKind::Identity);
            Some(ProjectedEntry {
                seq_start: existing.seq_start,
                source_seq_ranges: ranges,
                collapsed,
                ..entry.clone()
            })
        }
        _ => None,
    }
}

fn union_collapsed(
    a: &[CollapseKind],
    b: &[CollapseKind],
    extra: CollapseKind,
) -> Vec<CollapseKind> {
    let mut out: Vec<CollapseKind> = Vec::new();
    for k in a.iter().chain(b.iter()).copied().chain([extra]) {
        if !out.contains(&k) {
            out.push(k);
        }
    }
    out
}

/// One step of `mergeReasoningChunks`: `Some(merged)` when `entry` folds into `previous`.
pub(crate) fn try_merge_reasoning(
    previous: &ProjectedEntry,
    entry: &ProjectedEntry,
) -> Option<ProjectedEntry> {
    let (AgentTimelineItem::Reasoning { text: prev }, AgentTimelineItem::Reasoning { text: next }) =
        (&previous.item, &entry.item)
    else {
        return None;
    };
    if previous.seq_end + 1 != entry.seq_start || previous.turn_id != entry.turn_id {
        return None;
    }
    Some(ProjectedEntry {
        item: AgentTimelineItem::Reasoning {
            text: format!("{prev}{next}"),
        },
        timestamp: entry.timestamp.clone(),
        seq_end: entry.seq_end,
        source_seq_ranges: merge_seq_ranges(&previous.source_seq_ranges, &entry.source_seq_ranges),
        collapsed: union_collapsed(
            &previous.collapsed,
            &entry.collapsed,
            CollapseKind::ReasoningMerge,
        ),
        ..previous.clone()
    })
}

/// One step of `mergeAssistantChunks`. An explicit differing `messageId` on the incoming chunk
/// breaks the merge; an undefined one merges.
pub(crate) fn try_merge_assistant(
    previous: &ProjectedEntry,
    entry: &ProjectedEntry,
) -> Option<ProjectedEntry> {
    let (
        AgentTimelineItem::AssistantMessage {
            text: prev,
            message_id: prev_id,
        },
        AgentTimelineItem::AssistantMessage {
            text: next,
            message_id: next_id,
        },
    ) = (&previous.item, &entry.item)
    else {
        return None;
    };
    if previous.seq_end + 1 != entry.seq_start || previous.turn_id != entry.turn_id {
        return None;
    }
    if next_id.is_some() && prev_id != next_id {
        return None;
    }
    Some(ProjectedEntry {
        item: AgentTimelineItem::AssistantMessage {
            text: format!("{prev}{next}"),
            // PARITY-QUIRK(falsy-id-dropped): `previousAssistant.messageId ? {messageId} : {}`
            // drops an empty-string messageId on merge.
            message_id: prev_id.clone().filter(|s| !s.is_empty()),
        },
        timestamp: entry.timestamp.clone(),
        seq_end: entry.seq_end,
        source_seq_ranges: merge_seq_ranges(&previous.source_seq_ranges, &entry.source_seq_ranges),
        collapsed: union_collapsed(
            &previous.collapsed,
            &entry.collapsed,
            CollapseKind::AssistantMerge,
        ),
        ..previous.clone()
    })
}

fn collapse_by_identity(entries: &[ProjectedEntry]) -> Vec<ProjectedEntry> {
    let mut output: Vec<ProjectedEntry> = Vec::new();
    let mut index_by_identity: HashMap<String, usize> = HashMap::new();
    for entry in entries {
        let Some(identity) = entry.item.identity() else {
            output.push(entry.clone());
            continue;
        };
        let Some(&existing_index) = index_by_identity.get(&identity) else {
            index_by_identity.insert(identity, output.len());
            output.push(entry.clone());
            continue;
        };
        match merge_identity_entries(&output[existing_index], entry) {
            Some(merged) => output[existing_index] = merged,
            None => {
                index_by_identity.insert(identity, output.len());
                output.push(entry.clone());
            }
        }
    }
    output
}

fn merge_text_chunks(
    entries: Vec<ProjectedEntry>,
    merge: fn(&ProjectedEntry, &ProjectedEntry) -> Option<ProjectedEntry>,
) -> Vec<ProjectedEntry> {
    let mut output: Vec<ProjectedEntry> = Vec::with_capacity(entries.len());
    for entry in entries {
        if let Some(previous) = output.last() {
            if let Some(merged) = merge(previous, &entry) {
                *output.last_mut().expect("non-empty") = merged;
                continue;
            }
        }
        output.push(entry);
    }
    output
}

/// `projectTimelineRows` over already-built entries. Canonical mode returns them unchanged.
/// Projected-row inputs (re-projection) go through here too.
pub fn project_entries(entries: &[ProjectedEntry], mode: ProjectionMode) -> Vec<ProjectedEntry> {
    if mode == ProjectionMode::Canonical {
        return entries.to_vec();
    }
    let tool_collapsed = collapse_by_identity(entries);
    let assistant_merged = merge_text_chunks(tool_collapsed, try_merge_assistant);
    merge_text_chunks(assistant_merged, try_merge_reasoning)
}

/// `projectTimelineRows` over canonical rows.
pub fn project_rows(rows: &[Row], mode: ProjectionMode) -> Vec<ProjectedEntry> {
    let canonical: Vec<ProjectedEntry> = rows.iter().map(Row::to_entry).collect();
    project_entries(&canonical, mode)
}

/// Owns projected state; source events are consumed, never retained.
#[derive(Debug, Clone, Default)]
pub struct Projection {
    rows: Vec<ProjectedEntry>,
    identities: HashMap<String, usize>,
}

impl Projection {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn append(&mut self, row: &Row) {
        self.append_entry(row.to_entry());
    }

    /// Append an entry that may already be projected (seeding from another timeline's rows keeps
    /// its source ranges and `seq_start`).
    pub fn append_entry(&mut self, entry: ProjectedEntry) {
        let identity = entry.item.identity();
        let index = identity
            .as_ref()
            .and_then(|i| self.identities.get(i))
            .copied();
        if let Some(index) = index {
            if let Some(merged) = merge_identity_entries(&self.rows[index], &entry) {
                self.rows[index] = merged;
                return;
            }
        }
        if let Some(previous) = self.rows.last() {
            let merged = try_merge_assistant(previous, &entry)
                .or_else(|| try_merge_reasoning(previous, &entry));
            if let Some(merged) = merged {
                *self.rows.last_mut().expect("non-empty") = merged;
                return;
            }
        }
        if let Some(identity) = identity {
            self.identities.insert(identity, self.rows.len());
        }
        self.rows.push(entry);
    }

    pub fn rows(&self) -> &[ProjectedEntry] {
        &self.rows
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Stamp `provider_message_id` on the first user row with this `clientMessageId`.
    pub fn enrich_submitted_user_message(
        &mut self,
        client_message_id: &str,
        provider_message_id: &str,
    ) -> Option<ProjectedEntry> {
        let row = self.rows.iter_mut().find(|row| {
            matches!(&row.item, AgentTimelineItem::UserMessage { client_message_id: Some(c), .. } if c == client_message_id)
        })?;
        row.provider_message_id = Some(provider_message_id.to_string());
        Some(row.clone())
    }
}
