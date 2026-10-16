//! Approximation of `String.prototype.localeCompare` under ICU root collation.
//!
//! Order: whitespace, punctuation and symbols, digits, letters. Letters compare
//! case-insensitively first; at the first case difference lowercase sorts
//! before uppercase. Accents and non-Latin scripts fall back to code point
//! order, which differs from ICU.

use std::cmp::Ordering;

const PUNCTUATION_ORDER: &str = "_-,;:!?.'\"()[]{}@*/\\&#%`^+<=>|~$";

fn primary(c: char) -> (u8, u32) {
    if c.is_whitespace() {
        return (0, c as u32);
    }
    if let Some(pos) = PUNCTUATION_ORDER.find(c) {
        return (1, pos as u32);
    }
    if c.is_ascii_digit() {
        return (2, c as u32);
    }
    if c.is_alphabetic() {
        let lower = c.to_lowercase().next().unwrap_or(c);
        return (3, lower as u32);
    }
    (1, 1000 + c as u32)
}

pub fn locale_compare(a: &str, b: &str) -> Ordering {
    let mut ia = a.chars();
    let mut ib = b.chars();
    let mut case_diff = Ordering::Equal;
    loop {
        match (ia.next(), ib.next()) {
            (None, None) => break,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) => {
                let ord = primary(x).cmp(&primary(y));
                if ord != Ordering::Equal {
                    return ord;
                }
                if case_diff == Ordering::Equal && x != y {
                    // lowercase first
                    case_diff = match (x.is_lowercase(), y.is_lowercase()) {
                        (true, false) => Ordering::Less,
                        (false, true) => Ordering::Greater,
                        _ => x.cmp(&y),
                    };
                }
            }
        }
    }
    case_diff
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orders_like_icu_for_common_names() {
        assert_eq!(locale_compare("a", "B"), Ordering::Less);
        assert_eq!(locale_compare("a", "A"), Ordering::Less);
        assert_eq!(locale_compare("README.md", "src"), Ordering::Less);
        assert_eq!(locale_compare("a-noise", "z-projects"), Ordering::Less);
        assert_eq!(locale_compare("_x", "1"), Ordering::Less);
        assert_eq!(locale_compare("1", "a"), Ordering::Less);
        assert_eq!(locale_compare("abc", "abc"), Ordering::Equal);
        assert_eq!(locale_compare("ab", "abc"), Ordering::Less);
    }
}
