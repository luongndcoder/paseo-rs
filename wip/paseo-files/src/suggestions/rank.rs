//! Ranking of suggestion candidates.

use super::{Kind, MatchMode, PathFormat, SearchInput, Traversed};
use crate::collation::locale_compare;
use crate::path_utils::normalize_relative_path;
use crate::text_match::{score_path_match, MatchScore};
use std::cmp::Ordering;

const NO_SEGMENT_INDEX: i64 = i64::MAX;
const NO_MATCH_OFFSET: i64 = i64::MAX;
const NO_FUZZY_SCORE: i64 = i64::MAX;
pub(super) const NO_MATCH_TIER: i64 = 5;
const MAX_CONFIDENT_FUZZY_SKIPS_PER_CHARACTER: i64 = 2;

#[derive(Debug, Clone)]
pub(super) struct RankedEntry {
    pub path: String,
    pub visible: std::path::PathBuf,
    pub kind: Kind,
    pub match_tier: i64,
    pub segment_index: i64,
    pub match_offset: i64,
    pub fuzzy_score: i64,
    pub depth: i64,
}

struct RankFields {
    match_tier: i64,
    segment_index: i64,
    match_offset: i64,
    fuzzy_score: i64,
}

/// `shouldSuggest` plus `rank` in one pass: `None` when the entry is not a suggestion.
pub(super) fn suggest_rank(entry: &Traversed, input: &SearchInput) -> Option<RankedEntry> {
    if entry.name.starts_with('.') {
        return None;
    }
    if entry.kind == Kind::Directory && !input.include_directories {
        return None;
    }
    if entry.kind == Kind::File && !input.include_files {
        return None;
    }
    let ranked = rank(entry, input);
    if input.plan.normalized_query.is_empty() {
        return Some(ranked);
    }
    if input.match_mode == MatchMode::Suffix {
        return suffix_matches(&entry.visible, input, &input.plan.normalized_query)
            .then_some(ranked);
    }
    (ranked.match_tier != NO_MATCH_TIER).then_some(ranked)
}

pub(super) fn rank(entry: &Traversed, input: &SearchInput) -> RankedEntry {
    let relative = normalize_relative_path(&input.root, &entry.visible);
    let lower_path = relative.to_lowercase();
    let query = rank_query(input).to_lowercase();
    let segments: Vec<&str> = if lower_path == "." {
        Vec::new()
    } else {
        lower_path.split('/').collect()
    };
    let path_score = if query.is_empty() {
        None
    } else {
        score_path_match(&query, &relative)
    };
    let fields = if input.plan.is_path_query {
        rank_path_match(path_score)
    } else {
        rank_text_match(
            &query,
            &lower_path,
            input.path_format,
            path_score,
            &segments,
        )
    };
    RankedEntry {
        path: entry.visible.to_string_lossy().into_owned(),
        visible: entry.visible.clone(),
        kind: entry.kind,
        match_tier: fields.match_tier,
        segment_index: fields.segment_index,
        match_offset: fields.match_offset,
        fuzzy_score: fields.fuzzy_score,
        depth: if relative == "." {
            0
        } else {
            segments.len() as i64
        },
    }
}

fn rank_query(input: &SearchInput) -> &str {
    if input.plan.is_path_query {
        &input.plan.normalized_query
    } else {
        &input.plan.search_term
    }
}

fn rank_path_match(score: Option<MatchScore>) -> RankFields {
    RankFields {
        match_tier: score.map_or(NO_MATCH_TIER, |s| s.tier.min(4)),
        segment_index: NO_SEGMENT_INDEX,
        match_offset: score.map_or(NO_MATCH_OFFSET, |s| s.offset),
        fuzzy_score: score.and_then(|s| s.spread).unwrap_or(NO_FUZZY_SCORE),
    }
}

fn char_index_of(haystack: &str, needle: &str) -> i64 {
    let h: Vec<char> = haystack.chars().collect();
    let n: Vec<char> = needle.chars().collect();
    if n.len() > h.len() {
        return -1;
    }
    (0..=h.len() - n.len())
        .find(|&i| h[i..i + n.len()] == n[..])
        .map_or(-1, |i| i as i64)
}

fn rank_text_match(
    query: &str,
    lower_path: &str,
    path_format: PathFormat,
    path_score: Option<MatchScore>,
    segments: &[&str],
) -> RankFields {
    let offset = char_index_of(lower_path, query);
    let fuzzy = score_fuzzy_subsequence(query, segments.last().copied().unwrap_or(""));
    if query.is_empty() {
        return RankFields {
            match_tier: 3,
            segment_index: NO_SEGMENT_INDEX,
            match_offset: NO_MATCH_OFFSET,
            fuzzy_score: NO_FUZZY_SCORE,
        };
    }
    if let Some(r) = rank_segment_match(query, segments, offset, fuzzy) {
        return r;
    }
    let path_hit = if path_format == PathFormat::Relative {
        lower_path.starts_with(query)
    } else {
        offset >= 0
    };
    if path_hit {
        return RankFields {
            match_tier: 3,
            segment_index: NO_SEGMENT_INDEX,
            match_offset: offset,
            fuzzy_score: fuzzy.unwrap_or(NO_FUZZY_SCORE),
        };
    }
    if let Some(score) = fuzzy {
        return RankFields {
            match_tier: 4,
            segment_index: NO_SEGMENT_INDEX,
            match_offset: if offset >= 0 { offset } else { NO_MATCH_OFFSET },
            fuzzy_score: score,
        };
    }
    rank_path_match(path_score)
}

fn rank_segment_match(
    query: &str,
    segments: &[&str],
    offset: i64,
    fuzzy: Option<i64>,
) -> Option<RankFields> {
    let match_offset = if offset >= 0 { offset } else { NO_MATCH_OFFSET };
    let fuzzy_score = fuzzy.unwrap_or(NO_FUZZY_SCORE);
    type Predicate<'a> = &'a dyn Fn(&str) -> bool;
    let tiers: [(i64, Predicate); 3] = [
        (0, &|s| s == query),
        (1, &|s| s.starts_with(query)),
        (2, &|s| s.contains(query)),
    ];
    for (tier, predicate) in tiers {
        if let Some(index) = segments.iter().position(|s| predicate(s)) {
            return Some(RankFields {
                match_tier: tier,
                segment_index: index as i64,
                match_offset,
                fuzzy_score,
            });
        }
    }
    None
}

fn score_fuzzy_subsequence(query: &str, candidate: &str) -> Option<i64> {
    let q: Vec<char> = query.chars().collect();
    let mut query_index = 0usize;
    let mut first: i64 = -1;
    let mut previous: i64 = -1;
    let mut gaps: i64 = 0;
    for (index, c) in candidate.chars().enumerate() {
        if query_index >= q.len() {
            break;
        }
        if c != q[query_index] {
            continue;
        }
        let index = index as i64;
        if first < 0 {
            first = index;
        }
        if previous >= 0 {
            gaps += index - previous - 1;
        }
        previous = index;
        query_index += 1;
    }
    (query_index == q.len() && first >= 0).then_some(first + gaps)
}

fn suffix_matches(visible: &std::path::Path, input: &SearchInput, query: &str) -> bool {
    let query_segments: Vec<String> = query
        .to_lowercase()
        .split('/')
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();
    if query_segments.is_empty() {
        return false;
    }
    let relative = normalize_relative_path(&input.root, visible).to_lowercase();
    let path_segments: Vec<&str> = relative.split('/').filter(|s| !s.is_empty()).collect();
    if path_segments.len() < query_segments.len() {
        return false;
    }
    let offset = path_segments.len() - query_segments.len();
    query_segments
        .iter()
        .enumerate()
        .all(|(i, seg)| path_segments[offset + i] == seg)
}

pub(super) fn compare_rank(l: &RankedEntry, r: &RankedEntry) -> Ordering {
    l.match_tier
        .cmp(&r.match_tier)
        .then(l.segment_index.cmp(&r.segment_index))
        .then(l.match_offset.cmp(&r.match_offset))
        .then(l.fuzzy_score.cmp(&r.fuzzy_score))
        .then(l.depth.cmp(&r.depth))
        .then_with(|| compare_kinds(l.kind, r.kind))
        .then_with(|| locale_compare(&l.path, &r.path))
}

fn compare_kinds(l: Kind, r: Kind) -> Ordering {
    match (l, r) {
        (a, b) if a == b => Ordering::Equal,
        (Kind::Directory, _) => Ordering::Less,
        _ => Ordering::Greater,
    }
}

pub(super) fn has_confident_result(entries: &[RankedEntry], query: &str) -> bool {
    let max_fuzzy = query.chars().count() as i64 * MAX_CONFIDENT_FUZZY_SKIPS_PER_CHARACTER;
    entries
        .iter()
        .any(|e| e.match_tier < 4 || (e.match_tier == 4 && e.fuzzy_score <= max_fuzzy))
}
