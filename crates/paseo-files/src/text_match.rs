//! Ranked text matching (port of `protocol/src/search/text-match.ts`, exact
//! tiers only: the directory search never enables typo tolerance).
//!
//! Offsets are `char` indices; the TS code uses UTF-16 indices, which only
//! differ for characters outside the BMP.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MatchScore {
    pub tier: i64,
    pub offset: i64,
    pub spread: Option<i64>,
}

const TIER_EXACT: i64 = 0;
const TIER_WHOLE_WORD: i64 = 1;
const TIER_PREFIX: i64 = 2;
const TIER_WORD_START: i64 = 3;
const TIER_SUBSTRING: i64 = 4;
const TIER_SUBSEQUENCE: i64 = 5;

fn is_word_boundary_char(ch: Option<char>) -> bool {
    match ch {
        None => true,
        Some(c) => !(c.is_ascii_lowercase() || c.is_ascii_digit()),
    }
}

fn find_from(text: &[char], query: &[char], from: usize) -> Option<usize> {
    if query.len() > text.len() {
        return None;
    }
    (from..=text.len() - query.len()).find(|&i| text[i..i + query.len()] == *query)
}

fn score_substring_match(query: &[char], text: &[char]) -> Option<MatchScore> {
    let mut best: Option<MatchScore> = None;
    let mut pos = 0usize;
    while pos + query.len() <= text.len() {
        let Some(found) = find_from(text, query, pos) else {
            break;
        };
        let before = if found > 0 {
            Some(text[found - 1])
        } else {
            None
        };
        let after = text.get(found + query.len()).copied();
        let starts_at_boundary = found == 0 || is_word_boundary_char(before);
        let ends_at_boundary = after.is_none() || is_word_boundary_char(after);
        let tier = if starts_at_boundary && ends_at_boundary {
            TIER_WHOLE_WORD
        } else if found == 0 {
            TIER_PREFIX
        } else if starts_at_boundary {
            TIER_WORD_START
        } else {
            TIER_SUBSTRING
        };
        let candidate = MatchScore {
            tier,
            offset: found as i64,
            spread: None,
        };
        match best {
            Some(b) if !(tier < b.tier || (tier == b.tier && (found as i64) < b.offset)) => {}
            _ => best = Some(candidate),
        }
        pos = found + 1;
    }
    best
}

fn score_subsequence_match(query: &[char], text: &[char]) -> Option<MatchScore> {
    let mut query_index = 0usize;
    let mut first: i64 = -1;
    let mut last: i64 = -1;
    let mut text_index = 0usize;
    while text_index < text.len() && query_index < query.len() {
        let c = text[text_index];
        if c.is_whitespace() {
            query_index = 0;
            first = -1;
            last = -1;
            text_index += 1;
            continue;
        }
        if c == query[query_index] {
            if first == -1 {
                first = text_index as i64;
            }
            last = text_index as i64;
            query_index += 1;
        }
        text_index += 1;
    }
    if query_index != query.len() || first == -1 {
        return None;
    }
    Some(MatchScore {
        tier: TIER_SUBSEQUENCE,
        offset: first,
        spread: Some(last - first + 1),
    })
}

fn lower_chars(s: &str) -> Vec<char> {
    s.to_lowercase().chars().collect()
}

pub fn score_match(query: &str, text: &str) -> Option<MatchScore> {
    if query.is_empty() {
        return Some(MatchScore {
            tier: TIER_EXACT,
            offset: 0,
            spread: None,
        });
    }
    let q = lower_chars(query);
    let t = lower_chars(text);
    if t == q {
        return Some(MatchScore {
            tier: TIER_EXACT,
            offset: 0,
            spread: None,
        });
    }
    score_substring_match(&q, &t).or_else(|| score_subsequence_match(&q, &t))
}

struct CompactText {
    value: String,
    offsets: Vec<usize>,
}

fn compact_text(value: &str) -> CompactText {
    let mut compact = String::new();
    let mut offsets = Vec::new();
    for (index, c) in value.chars().enumerate() {
        if !c.is_ascii_alphanumeric() {
            continue;
        }
        compact.push(c.to_ascii_lowercase());
        offsets.push(index);
    }
    CompactText {
        value: compact,
        offsets,
    }
}

/// Match a query against a complete displayed path, including its separators.
pub fn score_path_match(query: &str, path: &str) -> Option<MatchScore> {
    if let Some(direct) = score_match(query, path) {
        return Some(direct);
    }
    let cq = compact_text(query);
    let cp = compact_text(path);
    if cq.value.is_empty() || cp.value.is_empty() {
        return None;
    }
    let score = score_match(&cq.value, &cp.value)?;
    let offset = cp
        .offsets
        .get(score.offset as usize)
        .map(|&o| o as i64)
        .unwrap_or(score.offset);
    Some(MatchScore { offset, ..score })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_and_substring_tiers() {
        assert_eq!(score_match("abc", "ABC").unwrap().tier, 0);
        assert_eq!(
            score_match("src", "src/components").unwrap().tier,
            TIER_WHOLE_WORD
        );
        assert_eq!(
            score_match("sr", "src/components").unwrap().tier,
            TIER_PREFIX
        );
        assert_eq!(
            score_match("comp", "src/components").unwrap().tier,
            TIER_WORD_START
        );
        assert_eq!(
            score_match("pon", "src/components").unwrap().tier,
            TIER_SUBSTRING
        );
    }

    #[test]
    fn subsequence_stays_within_a_word() {
        let s = score_match("pso", "projects/paseo-desktop").unwrap();
        assert_eq!(s.tier, TIER_SUBSEQUENCE);
        assert!(score_match("labdes", "label as design").is_none());
    }

    #[test]
    fn path_match_falls_back_to_compact_text() {
        let s = score_path_match("blank page editor", "blankpage/editor").unwrap();
        assert!(s.tier <= TIER_SUBSEQUENCE);
        assert!(score_path_match("zzz", "blankpage/editor").is_none());
    }
}
