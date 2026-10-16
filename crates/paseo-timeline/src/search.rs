//! Ranked text matching (`packages/protocol/src/search/text-match.ts`) and the agent history
//! query predicate (`agent-history-search.ts`).
//!
//! A score is a tier plus the offset it was found at; lower is better on both. All offsets and
//! lengths are UTF-16 code units, as in TS, so scores compare equal across implementations.

use paseo_tool_detail::js::{is_js_whitespace, js_trim};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MatchScore {
    pub tier: usize,
    pub offset: usize,
    pub spread: Option<usize>,
}

impl MatchScore {
    const fn new(tier: usize, offset: usize) -> Self {
        MatchScore {
            tier,
            offset,
            spread: None,
        }
    }
}

/// How much a typo in one query token is forgiven. Short tokens get transpositions and nothing
/// else; null (None) means the token is matched exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FuzzyPolicy {
    pub max_edits: usize,
    pub transpositions_only: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct MatchOptions {
    pub fuzzy: Option<FuzzyPolicy>,
    /// Match characters in order within one whitespace-delimited word. Default on.
    pub subsequence: bool,
}

impl Default for MatchOptions {
    fn default() -> Self {
        MatchOptions {
            fuzzy: None,
            subsequence: true,
        }
    }
}

const TIER_EXACT: usize = 0;
const TIER_WHOLE_WORD: usize = 1;
const TIER_PREFIX: usize = 2;
const TIER_WORD_START: usize = 3;
const TIER_SUBSTRING: usize = 4;
const TIER_SUBSEQUENCE: usize = 5;
const TIER_FUZZY: usize = 6;

type Units = Vec<u16>;

fn lower(s: &str) -> Units {
    s.to_lowercase().encode_utf16().collect()
}

fn is_alnum(u: u16) -> bool {
    matches!(u, 0x30..=0x39 | 0x61..=0x7a)
}

fn is_ws_unit(u: u16) -> bool {
    char::from_u32(u32::from(u)).is_some_and(is_js_whitespace)
}

fn is_word_boundary(ch: Option<u16>) -> bool {
    ch.is_none_or(|c| !is_alnum(c))
}

fn index_of(text: &[u16], query: &[u16], from: usize) -> Option<usize> {
    if query.len() > text.len() || from > text.len() - query.len() {
        return None;
    }
    (from..=text.len() - query.len()).find(|&i| &text[i..i + query.len()] == query)
}

fn score_substring_match(query: &[u16], text: &[u16]) -> Option<MatchScore> {
    let mut best: Option<MatchScore> = None;
    let mut pos = 0usize;
    while text.len() >= query.len() && pos <= text.len() - query.len() {
        let Some(found) = index_of(text, query, pos) else {
            break;
        };
        let before = (found > 0).then(|| text[found - 1]);
        let after = text.get(found + query.len()).copied();
        let starts_at_boundary = found == 0 || is_word_boundary(before);
        let ends_at_boundary = after.is_none() || is_word_boundary(after);
        let tier = if starts_at_boundary && ends_at_boundary {
            TIER_WHOLE_WORD
        } else if found == 0 {
            TIER_PREFIX
        } else if starts_at_boundary {
            TIER_WORD_START
        } else {
            TIER_SUBSTRING
        };
        if best.is_none_or(|b| tier < b.tier || (tier == b.tier && found < b.offset)) {
            best = Some(MatchScore::new(tier, found));
        }
        pos = found + 1;
    }
    best
}

fn score_subsequence_match(query: &[u16], text: &[u16]) -> Option<MatchScore> {
    let mut query_index = 0usize;
    let mut first: Option<usize> = None;
    let mut last = 0usize;
    let mut text_index = 0usize;
    while text_index < text.len() && query_index < query.len() {
        if is_ws_unit(text[text_index]) {
            query_index = 0;
            first = None;
            text_index += 1;
            continue;
        }
        if text[text_index] == query[query_index] {
            if first.is_none() {
                first = Some(text_index);
            }
            last = text_index;
            query_index += 1;
        }
        text_index += 1;
    }
    let first = first?;
    (query_index == query.len()).then(|| MatchScore {
        tier: TIER_SUBSEQUENCE,
        offset: first,
        spread: Some(last - first + 1),
    })
}

/// Damerau-Levenshtein distance, abandoned as soon as every cell in a row is over budget.
fn bounded_edit_distance(query: &[u16], word: &[u16], budget: usize) -> Option<usize> {
    if query.len().abs_diff(word.len()) > budget {
        return None;
    }
    let mut two_rows_back: Vec<usize> = Vec::new();
    let mut previous_row: Vec<usize> = (0..=word.len()).collect();
    for qi in 1..=query.len() {
        let mut current_row = vec![qi];
        let mut row_best = qi;
        for wi in 1..=word.len() {
            let substitution = usize::from(query[qi - 1] != word[wi - 1]);
            let mut cost = (current_row[wi - 1] + 1)
                .min(previous_row[wi] + 1)
                .min(previous_row[wi - 1] + substitution);
            let transposition =
                qi > 1 && wi > 1 && query[qi - 1] == word[wi - 2] && query[qi - 2] == word[wi - 1];
            if transposition {
                cost = cost.min(two_rows_back[wi - 2] + 1);
            }
            current_row.push(cost);
            row_best = row_best.min(cost);
        }
        if row_best > budget {
            return None;
        }
        two_rows_back = previous_row;
        previous_row = current_row;
    }
    let distance = previous_row[word.len()];
    (distance <= budget).then_some(distance)
}

/// True when the two differ only by one swap of neighbouring characters.
fn is_adjacent_transposition(query: &[u16], word: &[u16]) -> bool {
    if query.len() != word.len() {
        return false;
    }
    let mut index = 0;
    while index < query.len() && query[index] == word[index] {
        index += 1;
    }
    if index + 1 >= query.len() {
        return false;
    }
    if query[index] != word[index + 1] || query[index + 1] != word[index] {
        return false;
    }
    query[index + 2..] == word[index + 2..]
}

pub fn fuzzy_policy_for_token(token: &str) -> Option<FuzzyPolicy> {
    let len = token.encode_utf16().count();
    if len <= 3 {
        None
    } else if len == 4 {
        Some(FuzzyPolicy {
            max_edits: 1,
            transpositions_only: true,
        })
    } else if len <= 7 {
        Some(FuzzyPolicy {
            max_edits: 1,
            transpositions_only: false,
        })
    } else {
        Some(FuzzyPolicy {
            max_edits: 2,
            transpositions_only: false,
        })
    }
}

fn score_fuzzy_match(query: &[u16], text: &[u16], policy: FuzzyPolicy) -> Option<MatchScore> {
    if policy.max_edits == 0 || query.len() <= policy.max_edits {
        return None;
    }
    let mut best: Option<MatchScore> = None;
    let mut i = 0;
    while i < text.len() {
        if !is_alnum(text[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < text.len() && is_alnum(text[i]) {
            i += 1;
        }
        let word = &text[start..i];
        // The whole word and its leading slices, so a typo in a prefix still lands.
        let mut candidates: Vec<&[u16]> = Vec::new();
        for c in [
            word,
            &word[..query.len().min(word.len())],
            &word[..(query.len() + policy.max_edits).min(word.len())],
        ] {
            if !candidates.contains(&c) {
                candidates.push(c);
            }
        }
        for candidate in candidates {
            let distance = if policy.transpositions_only {
                is_adjacent_transposition(query, candidate).then_some(1)
            } else {
                bounded_edit_distance(query, candidate, policy.max_edits)
            };
            let Some(distance) = distance else { continue };
            let score = MatchScore {
                tier: TIER_FUZZY,
                offset: start,
                spread: Some(distance),
            };
            if best.is_none_or(|b| compare_match_scores(&score, &b).is_lt()) {
                best = Some(score);
            }
        }
    }
    best
}

fn score_match_units(query: &str, text: &str, options: MatchOptions) -> Option<MatchScore> {
    if query.is_empty() {
        return Some(MatchScore::new(TIER_EXACT, 0));
    }
    let q = lower(query);
    let t = lower(text);
    if t == q {
        return Some(MatchScore::new(TIER_EXACT, 0));
    }
    let exact = score_substring_match(&q, &t).or_else(|| {
        if options.subsequence {
            score_subsequence_match(&q, &t)
        } else {
            None
        }
    });
    if exact.is_some() {
        return exact;
    }
    options
        .fuzzy
        .and_then(|policy| score_fuzzy_match(&q, &t, policy))
}

/// `scoreMatch`.
pub fn score_match(query: &str, text: &str, options: MatchOptions) -> Option<MatchScore> {
    score_match_units(query, text, options)
}

/// `compareMatchScores`: tier, then offset, then spread (absent = 0).
pub fn compare_match_scores(a: &MatchScore, b: &MatchScore) -> std::cmp::Ordering {
    a.tier
        .cmp(&b.tier)
        .then(a.offset.cmp(&b.offset))
        .then(a.spread.unwrap_or(0).cmp(&b.spread.unwrap_or(0)))
}

struct Compact {
    value: String,
    offsets: Vec<usize>,
}

fn compact_text(value: &str) -> Compact {
    let mut compact = String::new();
    let mut offsets = Vec::new();
    for (index, unit) in value.encode_utf16().enumerate() {
        let is_alnum_ci = matches!(unit, 0x30..=0x39 | 0x41..=0x5a | 0x61..=0x7a);
        if !is_alnum_ci {
            continue;
        }
        compact.push((unit as u8 as char).to_ascii_lowercase());
        offsets.push(index);
    }
    Compact {
        value: compact,
        offsets,
    }
}

/// `scorePathMatch`: match against a displayed path including separators, then against the
/// compacted alphanumeric form (offset mapped back to the original path).
pub fn score_path_match(query: &str, path: &str) -> Option<MatchScore> {
    if let Some(direct) = score_match(query, path, MatchOptions::default()) {
        return Some(direct);
    }
    let cq = compact_text(query);
    let cp = compact_text(path);
    if cq.value.is_empty() || cp.value.is_empty() {
        return None;
    }
    let mut score = score_match(&cq.value, &cp.value, MatchOptions::default())?;
    score.offset = cp
        .offsets
        .get(score.offset)
        .copied()
        .unwrap_or(score.offset);
    Some(score)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MatchRange {
    pub start: usize,
    pub length: usize,
}

/// `matchRanges`: where a score's match landed, in UTF-16 units of `text`.
pub fn match_ranges(query: &str, text: &str, score: &MatchScore) -> Vec<MatchRange> {
    if query.is_empty() {
        return Vec::new();
    }
    let q = lower(query);
    let t = lower(text);
    match score.tier {
        TIER_EXACT => vec![MatchRange {
            start: 0,
            length: text.encode_utf16().count(),
        }],
        TIER_FUZZY => {
            let mut end = score.offset;
            while end < t.len() && is_alnum(t[end]) {
                end += 1;
            }
            vec![MatchRange {
                start: score.offset,
                length: (end - score.offset).max(1),
            }]
        }
        TIER_SUBSEQUENCE => {
            let mut indices = Vec::new();
            let mut qi = 0;
            let mut ti = score.offset;
            while ti < t.len() && qi < q.len() {
                if t[ti] == q[qi] {
                    indices.push(ti);
                    qi += 1;
                }
                ti += 1;
            }
            let mut ranges: Vec<MatchRange> = Vec::new();
            for index in indices {
                match ranges.last_mut() {
                    Some(last) if last.start + last.length == index => last.length += 1,
                    _ => ranges.push(MatchRange {
                        start: index,
                        length: 1,
                    }),
                }
            }
            ranges
        }
        _ => vec![MatchRange {
            start: score.offset,
            length: q.len(),
        }],
    }
}

/// `tokenizeQuery`: trim, lowercase, split on whitespace runs.
pub fn tokenize_query(query: &str) -> Vec<String> {
    js_trim(query)
        .to_lowercase()
        .split(is_js_whitespace)
        .filter(|t| !t.is_empty())
        .map(String::from)
        .collect()
}

#[derive(Debug, Clone, Copy)]
pub struct TextFieldsOptions {
    /// Forgive typos, per token.
    pub typo_tolerant: bool,
    /// See [`MatchOptions::subsequence`]; applied per token.
    pub subsequence: bool,
}

impl Default for TextFieldsOptions {
    fn default() -> Self {
        TextFieldsOptions {
            typo_tolerant: false,
            subsequence: true,
        }
    }
}

/// `scoreTextFields`: every token must match some field; scores add up.
pub fn score_text_fields(
    query: &str,
    fields: &[&str],
    options: TextFieldsOptions,
) -> Option<MatchScore> {
    let tokens = tokenize_query(query);
    if tokens.is_empty() {
        return Some(MatchScore {
            tier: TIER_EXACT,
            offset: 0,
            spread: Some(0),
        });
    }
    let mut aggregate = MatchScore {
        tier: 0,
        offset: 0,
        spread: Some(0),
    };
    for token in &tokens {
        let fuzzy = if options.typo_tolerant {
            fuzzy_policy_for_token(token)
        } else {
            None
        };
        let mut best: Option<MatchScore> = None;
        for field in fields {
            let score = score_match(
                token,
                field,
                MatchOptions {
                    fuzzy,
                    subsequence: options.subsequence,
                },
            );
            if let Some(score) = score {
                if best.is_none_or(|b| compare_match_scores(&score, &b).is_lt()) {
                    best = Some(score);
                }
            }
        }
        let best = best?;
        aggregate.tier += best.tier;
        aggregate.offset += best.offset;
        aggregate.spread = Some(
            aggregate.spread.unwrap_or(0)
                + best.spread.unwrap_or_else(|| token.encode_utf16().count()),
        );
    }
    Some(aggregate)
}

/// `matchesAgentHistoryQuery`: search the names people recall (workspace name, title, branch,
/// project name), typo tolerant.
pub fn matches_agent_history_query(
    query: &str,
    workspace_name: Option<&str>,
    title: Option<&str>,
    current_branch: Option<&str>,
    project_name: &str,
) -> bool {
    score_text_fields(
        query,
        &[
            workspace_name.unwrap_or(""),
            title.unwrap_or(""),
            current_branch.unwrap_or(""),
            project_name,
        ],
        TextFieldsOptions {
            typo_tolerant: true,
            subsequence: true,
        },
    )
    .is_some()
}
