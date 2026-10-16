//! Window and page selection over projected entries (`selectTimelineWindowByProjectedLimit`,
//! `selectProjectedTimelinePage`, `selectItemsByProjectedLimit`). Ported verbatim, including the
//! backward extension loops and the contiguous `end_seq` advance.

use paseo_agent_types::AgentTimelineItem;

use crate::model::{Direction, ProjectedEntry, ProjectionMode, Row, Seq, SeqRange};
use crate::projection::{project_entries, project_rows};

/// Inclusive seq bounds of a timeline window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bounds {
    pub min_seq: Seq,
    pub max_seq: Seq,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WindowSelection {
    pub projected_entries: Vec<ProjectedEntry>,
    pub selected_rows: Vec<Row>,
    pub min_seq: Option<Seq>,
    pub max_seq: Option<Seq>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PageSelection {
    pub entries: Vec<ProjectedEntry>,
    pub start_seq: Option<Seq>,
    pub end_seq: Option<Seq>,
    pub has_older: bool,
    pub has_newer: bool,
}

impl PageSelection {
    fn empty(has_older: bool, has_newer: bool) -> Self {
        PageSelection {
            entries: Vec::new(),
            start_seq: None,
            end_seq: None,
            has_older,
            has_newer,
        }
    }
}

fn entry_bounds(entries: &[ProjectedEntry]) -> (Seq, Seq) {
    let mut min = Seq::MAX;
    let mut max = Seq::MIN;
    for e in entries {
        min = min.min(e.seq_start);
        max = max.max(e.seq_end);
    }
    (min, max)
}

fn empty_window() -> WindowSelection {
    WindowSelection {
        projected_entries: Vec::new(),
        selected_rows: Vec::new(),
        min_seq: None,
        max_seq: None,
    }
}

/// Select a window by projected-entry count, then map it back to contiguous canonical rows, so
/// canonical reads with a bounded limit never cut through a merged chunk. `limit` 0 = all.
pub fn select_window(rows: &[Row], direction: Direction, limit: usize) -> WindowSelection {
    let projected_all = project_rows(rows, ProjectionMode::Projected);
    if projected_all.is_empty() {
        return empty_window();
    }

    let n = projected_all.len();
    let projected_entries: Vec<ProjectedEntry> = if limit == 0 || limit >= n {
        projected_all.clone()
    } else if direction == Direction::After {
        projected_all[..limit].to_vec()
    } else {
        projected_all[n - limit..].to_vec()
    };
    if projected_entries.is_empty() {
        return empty_window();
    }

    let (mut min_seq, mut max_seq) = entry_bounds(&projected_entries);
    let mut expanded = projected_entries;

    // Expand to every projected entry overlapping the selected canonical range. Tool lifecycle
    // collapse makes `seq_end` non-monotonic, which would otherwise leave cursor gaps.
    for _ in 0..=n {
        let overlapping: Vec<ProjectedEntry> = projected_all
            .iter()
            .filter(|e| e.seq_start <= max_seq && e.seq_end >= min_seq)
            .cloned()
            .collect();
        let (next_min, next_max) = entry_bounds(&overlapping);
        let settled =
            overlapping.len() == expanded.len() && next_min == min_seq && next_max == max_seq;
        expanded = overlapping;
        if settled {
            break;
        }
        min_seq = next_min;
        max_seq = next_max;
    }

    let selected_rows = rows
        .iter()
        .filter(|r| r.seq >= min_seq && r.seq <= max_seq)
        .cloned()
        .collect();
    let finite = |v: Seq| (v != Seq::MAX && v != Seq::MIN).then_some(v);
    WindowSelection {
        projected_entries: expanded,
        selected_rows,
        min_seq: finite(min_seq),
        max_seq: finite(max_seq),
    }
}

fn first_source_seq_in_range(entry: &ProjectedEntry, start_seq: Seq, end_seq: Seq) -> Option<Seq> {
    entry.source_seq_ranges.iter().find_map(|range| {
        let first = range.start_seq.max(start_seq);
        (first <= range.end_seq.min(end_seq)).then_some(first)
    })
}

fn select_after(
    entries: &[ProjectedEntry],
    start_seq: Seq,
    max_seq: Seq,
    limit: usize,
) -> (Vec<ProjectedEntry>, Option<Seq>) {
    let mut eligible: Vec<(usize, Seq)> = entries
        .iter()
        .enumerate()
        .filter_map(|(i, e)| first_source_seq_in_range(e, start_seq, max_seq).map(|s| (i, s)))
        .collect();
    eligible.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(&b.0)));
    if limit != 0 {
        eligible.truncate(limit);
    }
    eligible.sort_by_key(|(i, _)| *i);
    let selected: Vec<ProjectedEntry> = eligible.iter().map(|(i, _)| entries[*i].clone()).collect();
    if selected.is_empty() {
        return (Vec::new(), None);
    }

    let mut ranges: Vec<SeqRange> = selected
        .iter()
        .flat_map(|e| e.source_seq_ranges.iter().copied())
        .collect();
    ranges.sort_by(|a, b| {
        a.start_seq
            .cmp(&b.start_seq)
            .then(a.end_seq.cmp(&b.end_seq))
    });
    // Wide entries can carry future, discontiguous ranges. The page cursor advances only through
    // source rows covered without a gap.
    let mut end_seq = start_seq - 1;
    for range in ranges {
        if range.end_seq <= end_seq {
            continue;
        }
        if range.start_seq > end_seq + 1 {
            break;
        }
        end_seq = range.end_seq.min(max_seq);
    }
    (selected, (end_seq >= start_seq).then_some(end_seq))
}

fn select_before(
    entries: &[ProjectedEntry],
    end_seq: Seq,
    limit: usize,
) -> (Vec<ProjectedEntry>, Option<Seq>, bool) {
    // Older history follows display order; `seq_start` keeps a full projected item on exactly one
    // backward page even when lifecycle updates moved its `seq_end`.
    let eligible: Vec<&ProjectedEntry> =
        entries.iter().filter(|e| e.seq_start <= end_seq).collect();
    let skip = if limit == 0 || limit >= eligible.len() {
        0
    } else {
        eligible.len() - limit
    };
    let selected: Vec<ProjectedEntry> = eligible[skip..].iter().map(|e| (*e).clone()).collect();
    let start_seq = selected.first().map(|e| e.seq_start);
    let has_older = selected.len() < eligible.len();
    (selected, start_seq, has_older)
}

fn select_tail(projected_all: &[ProjectedEntry], limit: usize, bounds: Bounds) -> PageSelection {
    let mut start = if limit == 0 {
        0
    } else {
        projected_all.len().saturating_sub(limit)
    };
    // Include earlier display anchors with updates in the selected source window, so a tail
    // certifies contiguous coverage even with interleaved tool updates.
    for i in (0..start).rev() {
        if projected_all[i].seq_end >= projected_all[start].seq_start {
            start = i;
        }
    }
    let selected = projected_all[start..].to_vec();
    PageSelection {
        start_seq: selected.first().map(|e| e.seq_start),
        end_seq: Some(bounds.max_seq),
        has_older: start > 0,
        has_newer: false,
        entries: selected,
    }
}

/// `bounds` from `getTimelineBounds`: min `seq_start`, max `seq_end` (floor 0) over the input.
fn timeline_bounds(entries: &[ProjectedEntry]) -> Option<Bounds> {
    if entries.is_empty() {
        return None;
    }
    let (min_seq, _) = entry_bounds(entries);
    let max_seq = entries.iter().map(|e| e.seq_end).fold(0, Seq::max);
    Some(Bounds { min_seq, max_seq })
}

/// `selectProjectedTimelinePage` over entries (canonical or already projected; they are projected
/// again, which is idempotent). `limit` `None` and `Some(0)` both mean unbounded.
pub fn select_page(
    entries: &[ProjectedEntry],
    bounds: Option<Bounds>,
    direction: Direction,
    cursor_seq: Option<Seq>,
    limit: Option<usize>,
) -> PageSelection {
    let limit = limit.unwrap_or(0);
    let Some(bounds) = bounds.or_else(|| timeline_bounds(entries)) else {
        return PageSelection::empty(false, false);
    };
    let projected_all = project_entries(entries, ProjectionMode::Projected);

    if projected_all.is_empty() {
        return match direction {
            Direction::After => {
                let cursor = cursor_seq.unwrap_or(bounds.min_seq - 1);
                PageSelection::empty(cursor >= bounds.min_seq, cursor < bounds.max_seq)
            }
            Direction::Before => {
                let cursor = cursor_seq.unwrap_or(bounds.max_seq + 1);
                PageSelection::empty(cursor > bounds.min_seq, cursor <= bounds.max_seq)
            }
            Direction::Tail => PageSelection::empty(false, false),
        };
    }

    match direction {
        Direction::Tail => select_tail(&projected_all, limit, bounds),
        Direction::After => {
            let cursor = cursor_seq.unwrap_or(bounds.min_seq - 1);
            let start_seq = bounds.min_seq.max(cursor + 1);
            let (selected, end_seq) =
                select_after(&projected_all, start_seq, bounds.max_seq, limit);
            PageSelection {
                entries: selected,
                start_seq: end_seq.map(|_| start_seq),
                end_seq,
                has_older: start_seq > bounds.min_seq,
                has_newer: end_seq.is_some_and(|e| e < bounds.max_seq),
            }
        }
        Direction::Before => {
            let cursor = cursor_seq.unwrap_or(bounds.max_seq + 1);
            let end_seq = bounds.max_seq.min(cursor - 1);
            if end_seq < bounds.min_seq {
                return PageSelection::empty(false, end_seq < bounds.max_seq);
            }
            let (selected, start_seq, has_older) = select_before(&projected_all, end_seq, limit);
            PageSelection {
                entries: selected,
                start_seq,
                end_seq: Some(end_seq),
                has_older,
                has_newer: end_seq < bounds.max_seq,
            }
        }
    }
}

/// `select_page` over canonical rows.
pub fn select_page_from_rows(
    rows: &[Row],
    bounds: Option<Bounds>,
    direction: Direction,
    cursor_seq: Option<Seq>,
    limit: Option<usize>,
) -> PageSelection {
    let entries: Vec<ProjectedEntry> = rows.iter().map(Row::to_entry).collect();
    select_page(&entries, bounds, direction, cursor_seq, limit)
}

#[derive(Debug, Clone, PartialEq)]
pub struct ItemSelection {
    pub items: Vec<AgentTimelineItem>,
    pub total_projected: usize,
    pub shown_projected: usize,
}

/// `selectItemsByProjectedLimit`: index position (1-based) is the canonical seq.
pub fn select_items_by_projected_limit(
    items: &[AgentTimelineItem],
    direction: Direction,
    limit: usize,
) -> ItemSelection {
    let rows: Vec<Row> = items
        .iter()
        .enumerate()
        .map(|(i, item)| Row::new(i as Seq + 1, "", item.clone()))
        .collect();
    let total = project_rows(&rows, ProjectionMode::Projected).len();
    let window = select_window(&rows, direction, limit);
    ItemSelection {
        items: window.selected_rows.into_iter().map(|r| r.item).collect(),
        total_projected: total,
        shown_projected: window.projected_entries.len(),
    }
}
