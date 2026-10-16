//! Row and projected-entry types (`agent-timeline-store-types.ts`, `timeline-projection.ts`).
//!
//! JSON helpers (`to_value`/`from_value`) exist so goldens and the wire layer can compare against
//! TS output. Optional keys are absent, never `null`, as in TS.

use paseo_agent_types::{AgentTimelineItem, Map};
use serde_json::Value;

/// Source and display sequence numbers. Signed because paging arithmetic goes below zero
/// (`minSeq - 1` on an empty window).
pub type Seq = i64;

/// PARITY-QUIRK(falsy-id-dropped): `row.turnId ?` / `row.providerMessageId ?` (and the store's
/// `options?.turnId ?`) in TS use truthiness, so an empty string is treated as absent.
pub(crate) fn non_empty(s: &Option<String>) -> Option<String> {
    s.as_ref().filter(|s| !s.is_empty()).cloned()
}

/// A canonical, recorded timeline row (`AgentTimelineRow`).
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub seq: Seq,
    pub timestamp: String,
    pub item: AgentTimelineItem,
    pub turn_id: Option<String>,
    pub provider_message_id: Option<String>,
}

impl Row {
    pub fn new(seq: Seq, timestamp: impl Into<String>, item: AgentTimelineItem) -> Self {
        Row {
            seq,
            timestamp: timestamp.into(),
            item,
            turn_id: None,
            provider_message_id: None,
        }
    }

    /// `makeCanonicalEntries` for one row.
    pub fn to_entry(&self) -> ProjectedEntry {
        ProjectedEntry {
            item: self.item.clone(),
            turn_id: non_empty(&self.turn_id),
            provider_message_id: non_empty(&self.provider_message_id),
            timestamp: self.timestamp.clone(),
            seq_start: self.seq,
            seq_end: self.seq,
            source_seq_ranges: vec![SeqRange {
                start_seq: self.seq,
                end_seq: self.seq,
            }],
            collapsed: Vec::new(),
        }
    }

    pub fn to_value(&self) -> Value {
        let mut m = Map::new();
        m.insert("seq".into(), Value::from(self.seq));
        m.insert("timestamp".into(), Value::String(self.timestamp.clone()));
        m.insert("item".into(), self.item.to_value());
        if let Some(t) = &self.turn_id {
            m.insert("turnId".into(), Value::String(t.clone()));
        }
        if let Some(p) = &self.provider_message_id {
            m.insert("providerMessageId".into(), Value::String(p.clone()));
        }
        Value::Object(m)
    }

    pub fn from_value(v: &Value) -> Result<Self, String> {
        let o = v.as_object().ok_or("row: expected object")?;
        Ok(Row {
            seq: o
                .get("seq")
                .and_then(Value::as_i64)
                .ok_or("row.seq: expected integer")?,
            timestamp: o
                .get("timestamp")
                .and_then(Value::as_str)
                .ok_or("row.timestamp: expected string")?
                .to_string(),
            item: AgentTimelineItem::from_value(o.get("item").ok_or("row.item missing")?)
                .map_err(|e| e.to_string())?,
            turn_id: o.get("turnId").and_then(Value::as_str).map(String::from),
            provider_message_id: o
                .get("providerMessageId")
                .and_then(Value::as_str)
                .map(String::from),
        })
    }
}

/// Inclusive source-seq interval an entry was built from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SeqRange {
    pub start_seq: Seq,
    pub end_seq: Seq,
}

/// Why an entry absorbed other source rows (`TimelineProjectionKind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollapseKind {
    AssistantMerge,
    ReasoningMerge,
    ToolLifecycle,
    Identity,
}

impl CollapseKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AssistantMerge => "assistant_merge",
            Self::ReasoningMerge => "reasoning_merge",
            Self::ToolLifecycle => "tool_lifecycle",
            Self::Identity => "identity",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "assistant_merge" => Self::AssistantMerge,
            "reasoning_merge" => Self::ReasoningMerge,
            "tool_lifecycle" => Self::ToolLifecycle,
            "identity" => Self::Identity,
            _ => return None,
        })
    }
}

/// A projected display entry (`TimelineProjectionEntry`).
///
/// The stored/fetched row type `ProjectedTimelineRow` is the same data plus `seq`, which is always
/// `seq_end`; see [`ProjectedRow`].
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectedEntry {
    pub item: AgentTimelineItem,
    pub turn_id: Option<String>,
    pub provider_message_id: Option<String>,
    pub timestamp: String,
    pub seq_start: Seq,
    pub seq_end: Seq,
    pub source_seq_ranges: Vec<SeqRange>,
    pub collapsed: Vec<CollapseKind>,
}

/// A retained item. `seq()` is its latest source event, not its display anchor.
pub type ProjectedRow = ProjectedEntry;

impl ProjectedEntry {
    pub fn seq(&self) -> Seq {
        self.seq_end
    }

    /// Entry JSON (no `seq`), as `projectTimelineRows` returns it.
    pub fn to_value(&self) -> Value {
        self.build_value(false)
    }

    /// Row JSON with `seq` first, as `fetch` returns it (`Object.assign({seq}, entry)`).
    pub fn to_row_value(&self) -> Value {
        self.build_value(true)
    }

    fn build_value(&self, with_seq: bool) -> Value {
        let mut m = Map::new();
        if with_seq {
            m.insert("seq".into(), Value::from(self.seq_end));
        }
        m.insert("item".into(), self.item.to_value());
        if let Some(t) = &self.turn_id {
            m.insert("turnId".into(), Value::String(t.clone()));
        }
        if let Some(p) = &self.provider_message_id {
            m.insert("providerMessageId".into(), Value::String(p.clone()));
        }
        m.insert("timestamp".into(), Value::String(self.timestamp.clone()));
        m.insert("seqStart".into(), Value::from(self.seq_start));
        m.insert("seqEnd".into(), Value::from(self.seq_end));
        m.insert(
            "sourceSeqRanges".into(),
            Value::Array(
                self.source_seq_ranges
                    .iter()
                    .map(|r| {
                        let mut o = Map::new();
                        o.insert("startSeq".into(), Value::from(r.start_seq));
                        o.insert("endSeq".into(), Value::from(r.end_seq));
                        Value::Object(o)
                    })
                    .collect(),
            ),
        );
        m.insert(
            "collapsed".into(),
            Value::Array(
                self.collapsed
                    .iter()
                    .map(|c| Value::String(c.as_str().into()))
                    .collect(),
            ),
        );
        Value::Object(m)
    }

    pub fn from_value(v: &Value) -> Result<Self, String> {
        let o = v.as_object().ok_or("entry: expected object")?;
        let int = |k: &str| {
            o.get(k)
                .and_then(Value::as_i64)
                .ok_or_else(|| format!("entry.{k}: expected integer"))
        };
        let ranges = o
            .get("sourceSeqRanges")
            .and_then(Value::as_array)
            .ok_or("entry.sourceSeqRanges: expected array")?
            .iter()
            .map(|r| {
                Ok(SeqRange {
                    start_seq: r
                        .get("startSeq")
                        .and_then(Value::as_i64)
                        .ok_or("range.startSeq")?,
                    end_seq: r
                        .get("endSeq")
                        .and_then(Value::as_i64)
                        .ok_or("range.endSeq")?,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        let collapsed = o
            .get("collapsed")
            .and_then(Value::as_array)
            .ok_or("entry.collapsed: expected array")?
            .iter()
            .map(|c| {
                c.as_str()
                    .and_then(CollapseKind::parse)
                    .ok_or_else(|| "entry.collapsed: unknown kind".to_string())
            })
            .collect::<Result<Vec<_>, String>>()?;
        Ok(ProjectedEntry {
            item: AgentTimelineItem::from_value(o.get("item").ok_or("entry.item missing")?)
                .map_err(|e| e.to_string())?,
            turn_id: o.get("turnId").and_then(Value::as_str).map(String::from),
            provider_message_id: o
                .get("providerMessageId")
                .and_then(Value::as_str)
                .map(String::from),
            timestamp: o
                .get("timestamp")
                .and_then(Value::as_str)
                .ok_or("entry.timestamp: expected string")?
                .to_string(),
            seq_start: int("seqStart")?,
            seq_end: int("seqEnd")?,
            source_seq_ranges: ranges,
            collapsed,
        })
    }
}

/// Anything with a recorded seq, timestamp and item (canonical or projected rows).
pub trait RowView {
    fn seq(&self) -> Seq;
    fn timestamp(&self) -> &str;
    fn item(&self) -> &AgentTimelineItem;
}

impl RowView for Row {
    fn seq(&self) -> Seq {
        self.seq
    }
    fn timestamp(&self) -> &str {
        &self.timestamp
    }
    fn item(&self) -> &AgentTimelineItem {
        &self.item
    }
}

impl RowView for ProjectedEntry {
    fn seq(&self) -> Seq {
        self.seq_end
    }
    fn timestamp(&self) -> &str {
        &self.timestamp
    }
    fn item(&self) -> &AgentTimelineItem {
        &self.item
    }
}

/// Which view of the rows `project_rows` returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectionMode {
    Canonical,
    Projected,
}

/// `TimelineLimitDirection` / `AgentTimelineFetchDirection`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Tail,
    Before,
    After,
}

impl Direction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tail => "tail",
            Self::Before => "before",
            Self::After => "after",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "tail" => Self::Tail,
            "before" => Self::Before,
            "after" => Self::After,
            _ => return None,
        })
    }
}
