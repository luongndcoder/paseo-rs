//! Pure timeline core for the Paseo daemon: row projection, paging, per-agent
//! [`TimelineState`], the stream [`Coalescer`] state machine, prompt index, agent-message
//! envelope projection and history text search. No tokio, no IO: callers own clocks and timers.
//!
//! Source of truth is the TS daemon (`timeline-projection.ts`, `agent-timeline-store.ts`,
//! `agent-stream-coalescer.ts`, `timeline-prompt-index.ts`, `agent-messages`, `text-match.ts`).
//! Quirks are reproduced and marked `PARITY-QUIRK(<name>)`; see the spec's post-cutover list.

pub mod agent_message;
pub mod coalescer;
pub mod limits;
pub mod model;
pub mod paging;
pub mod projection;
pub mod prompt_index;
pub mod search;
pub mod store;

pub use agent_message::project_agent_message;
pub use coalescer::{is_coalescable, Coalescer, Flush};
pub use model::{
    CollapseKind, Direction, ProjectedEntry, ProjectedRow, ProjectionMode, Row, RowView, Seq,
    SeqRange,
};
pub use paging::{
    select_items_by_projected_limit, select_page, select_page_from_rows, select_window, Bounds,
    ItemSelection, PageSelection, WindowSelection,
};
pub use projection::{project_entries, project_rows, Projection};
pub use prompt_index::{build_timeline_prompt_index, PromptIndex, PromptIndexEntry};
pub use store::{
    AppendOptions, Cursor, FetchOptions, FetchResult, SeedOptions, TimelineState, Window,
    DEFAULT_TIMELINE_FETCH_LIMIT,
};
