//! `TimelineState`: one agent's timeline (`InMemoryAgentTimelineStore` for a single agent id).
//!
//! Single-threaded `&mut self` API, no interior locking. The epoch is minted on initialize and is
//! never persisted; callers re-initialize (new epoch) on resume, forced hydrate, reload, rewind.
//! Seq is assigned here, after coalescing; the caller records the row BEFORE dispatching the
//! `agent_stream` event so a gap-recovering client can always fetch it.
//!
//! Memory is unbounded and `fetch` is O(N) (it re-projects, like TS). A copy-on-write snapshot is
//! a later optimization.

use paseo_agent_types::AgentTimelineItem;

use crate::model::{Direction, ProjectedEntry, ProjectionMode, Row, Seq};
use crate::paging::{select_page, Bounds};
use crate::projection::{project_entries, Projection};

pub const DEFAULT_TIMELINE_FETCH_LIMIT: usize = 200;

/// Seed for [`TimelineState::initialize`] (`SeedAgentTimelineOptions`).
#[derive(Debug, Clone, Default)]
pub struct SeedOptions {
    pub items: Vec<AgentTimelineItem>,
    /// When non-empty, used instead of `items`.
    pub rows: Vec<Row>,
    /// Already-projected rows (seeding from another timeline's `rows()`); used when `rows` is
    /// empty and non-empty itself. Their `seq` is `seq_end`.
    pub projected_rows: Vec<ProjectedEntry>,
    pub epoch: Option<String>,
    pub next_seq: Option<Seq>,
    pub timestamp: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor {
    pub epoch: String,
    pub seq: Seq,
}

#[derive(Debug, Clone, Default)]
pub struct FetchOptions {
    pub direction: Option<Direction>,
    pub cursor: Option<Cursor>,
    /// `None`: default 200. `Some(0)`: all rows in the selected window.
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    pub min_seq: Seq,
    pub max_seq: Seq,
    pub next_seq: Seq,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FetchResult {
    pub epoch: String,
    /// The requested direction, echoed even when `reset` forced a tail read.
    pub direction: Direction,
    pub reset: bool,
    pub stale_cursor: bool,
    pub gap: bool,
    pub window: Window,
    pub has_older: bool,
    pub has_newer: bool,
    pub start_seq: Option<Seq>,
    pub end_seq: Option<Seq>,
    pub rows: Vec<ProjectedEntry>,
}

impl FetchResult {
    pub fn to_value(&self) -> serde_json::Value {
        use serde_json::{json, Value};
        let opt = |v: Option<Seq>| v.map_or(Value::Null, Value::from);
        json!({
            "epoch": self.epoch,
            "direction": self.direction.as_str(),
            "reset": self.reset,
            "staleCursor": self.stale_cursor,
            "gap": self.gap,
            "window": {
                "minSeq": self.window.min_seq,
                "maxSeq": self.window.max_seq,
                "nextSeq": self.window.next_seq,
            },
            "hasOlder": self.has_older,
            "hasNewer": self.has_newer,
            "startSeq": opt(self.start_seq),
            "endSeq": opt(self.end_seq),
            "rows": self.rows.iter().map(ProjectedEntry::to_row_value).collect::<Vec<_>>(),
        })
    }
}

#[derive(Debug, Clone, Default)]
pub struct AppendOptions {
    pub timestamp: Option<String>,
    pub provider_message_id: Option<String>,
    pub turn_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TimelineState {
    epoch: String,
    projection: Projection,
    min_seq: Seq,
    next_seq: Seq,
}

impl TimelineState {
    /// Build state from a seed. Mints a new epoch unless `seed.epoch` is set.
    pub fn initialize(seed: SeedOptions) -> Self {
        let entries: Vec<ProjectedEntry> = if !seed.rows.is_empty() {
            seed.rows.iter().map(Row::to_entry).collect()
        } else if !seed.projected_rows.is_empty() {
            seed.projected_rows.clone()
        } else {
            let timestamp = seed.timestamp.clone().unwrap_or_else(iso8601_now);
            let mut next = seed.next_seq.unwrap_or(1);
            seed.items
                .iter()
                .map(|item| {
                    let row = Row::new(next, timestamp.clone(), item.clone());
                    next += 1;
                    row.to_entry()
                })
                .collect()
        };
        let next_seq = entries.iter().fold(seed.next_seq.unwrap_or(1), |next, e| {
            next.max(e.seq_end + 1)
        });
        let mut projection = Projection::new();
        for entry in entries {
            projection.append_entry(entry);
        }
        let min_seq = projection.rows().first().map_or(0, |r| r.seq_start);
        TimelineState {
            epoch: seed
                .epoch
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            projection,
            min_seq,
            next_seq,
        }
    }

    pub fn epoch(&self) -> &str {
        &self.epoch
    }

    pub fn window(&self) -> Window {
        Window {
            min_seq: self.min_seq,
            max_seq: self.next_seq - 1,
            next_seq: self.next_seq,
        }
    }

    /// Projected rows (`getRows`).
    pub fn rows(&self) -> &[ProjectedEntry] {
        self.projection.rows()
    }

    pub fn items(&self) -> Vec<AgentTimelineItem> {
        self.rows().iter().map(|r| r.item.clone()).collect()
    }

    pub fn submitted_user_message(&self, client_message_id: &str) -> Option<ProjectedEntry> {
        self.rows()
            .iter()
            .find(|r| {
                matches!(&r.item, AgentTimelineItem::UserMessage { client_message_id: Some(c), .. } if c == client_message_id)
            })
            .cloned()
    }

    pub fn enrich_submitted_user_message(
        &mut self,
        client_message_id: &str,
        provider_message_id: &str,
    ) -> Option<ProjectedEntry> {
        self.projection
            .enrich_submitted_user_message(client_message_id, provider_message_id)
    }

    /// Record an item at the next seq and return the canonical row.
    pub fn append(&mut self, item: AgentTimelineItem, options: AppendOptions) -> Row {
        let row = Row {
            seq: self.next_seq,
            timestamp: options.timestamp.unwrap_or_else(iso8601_now),
            item,
            turn_id: options.turn_id.filter(|s| !s.is_empty()),
            provider_message_id: options.provider_message_id.filter(|s| !s.is_empty()),
        };
        self.next_seq += 1;
        if self.min_seq == 0 {
            self.min_seq = row.seq;
        }
        self.projection.append(&row);
        row
    }

    pub fn fetch(&self, options: &FetchOptions) -> FetchResult {
        let direction = options.direction.unwrap_or(Direction::Tail);
        let cursor = options.cursor.as_ref();
        let rows = self.projection.rows();
        let window = self.window();
        let stale_cursor = cursor.is_some_and(|c| c.epoch != self.epoch);
        let gap = !stale_cursor
            && direction == Direction::After
            && !rows.is_empty()
            && cursor.is_some_and(|c| c.seq < self.min_seq - 1);
        let reset = stale_cursor || gap;
        let page = select_page(
            rows,
            Some(Bounds {
                min_seq: window.min_seq,
                max_seq: window.max_seq,
            }),
            if reset { Direction::Tail } else { direction },
            cursor.map(|c| c.seq),
            Some(options.limit.unwrap_or(DEFAULT_TIMELINE_FETCH_LIMIT)),
        );
        FetchResult {
            epoch: self.epoch.clone(),
            direction,
            reset,
            stale_cursor,
            gap,
            window,
            has_older: page.has_older,
            has_newer: page.has_newer,
            start_seq: page.start_seq,
            end_seq: page.end_seq,
            rows: page.entries,
        }
    }

    pub fn last_item(&self) -> Option<&AgentTimelineItem> {
        let last = self.next_seq - 1;
        self.rows()
            .iter()
            .find(|r| r.seq_end == last)
            .map(|r| &r.item)
    }

    pub fn last_assistant_message(&self) -> Option<&str> {
        self.rows().iter().rev().find_map(|r| match &r.item {
            AgentTimelineItem::AssistantMessage { text, .. } => Some(text.as_str()),
            _ => None,
        })
    }

    /// Re-project the current rows (idempotent; used by tests and hydration checks).
    pub fn reproject(&self) -> Vec<ProjectedEntry> {
        project_entries(self.rows(), ProjectionMode::Projected)
    }
}

/// `new Date().toISOString()`: `YYYY-MM-DDTHH:MM:SS.mmmZ` in UTC.
pub fn iso8601_now() -> String {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64);
    iso8601_from_millis(millis)
}

pub fn iso8601_from_millis(millis: i64) -> String {
    let days = millis.div_euclid(86_400_000);
    let rem = millis.rem_euclid(86_400_000);
    // Howard Hinnant's civil-from-days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3_600_000,
        rem / 60_000 % 60,
        rem / 1000 % 60,
        rem % 1000
    )
}
