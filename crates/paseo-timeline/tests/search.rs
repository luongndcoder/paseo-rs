//! Port of `packages/protocol/src/search/text-match.test.ts`.

use paseo_timeline::search::*;

fn sc(tier: usize, offset: usize) -> Option<MatchScore> {
    Some(MatchScore {
        tier,
        offset,
        spread: None,
    })
}
fn sp(tier: usize, offset: usize, spread: usize) -> Option<MatchScore> {
    Some(MatchScore {
        tier,
        offset,
        spread: Some(spread),
    })
}
fn m(q: &str, t: &str) -> Option<MatchScore> {
    score_match(q, t, MatchOptions::default())
}
fn mf(q: &str, t: &str, max_edits: usize, transpositions_only: bool) -> Option<MatchScore> {
    score_match(
        q,
        t,
        MatchOptions {
            fuzzy: Some(FuzzyPolicy {
                max_edits,
                transpositions_only,
            }),
            subsequence: true,
        },
    )
}
fn mp(q: &str, t: &str) -> Option<MatchScore> {
    score_match(
        q,
        t,
        MatchOptions {
            fuzzy: fuzzy_policy_for_token(q),
            subsequence: true,
        },
    )
}

fn mark_up(q: &str, t: &str, options: MatchOptions) -> String {
    let Some(score) = score_match(q, t, options) else {
        return t.to_string();
    };
    let units: Vec<u16> = t.encode_utf16().collect();
    let mut out = String::new();
    let mut cursor = 0;
    for r in match_ranges(q, t, &score) {
        out.push_str(&String::from_utf16(&units[cursor..r.start]).unwrap());
        out.push('[');
        out.push_str(&String::from_utf16(&units[r.start..r.start + r.length]).unwrap());
        out.push(']');
        cursor = r.start + r.length;
    }
    out + &String::from_utf16(&units[cursor..]).unwrap()
}

#[test]
fn score_match_tiers() {
    assert_eq!(m("", "anything"), sc(0, 0));
    for (q, t) in [("pi", "pi"), ("PI", "pi"), ("Pi", "pI")] {
        assert_eq!(m(q, t), sc(0, 0));
    }
    assert_eq!(m("feat", "feat/pi-direct-sdk"), sc(1, 0));
    assert_eq!(m("pi", "feat/pi-direct-sdk"), sc(1, 5));
    assert_eq!(m("pi", "a b pi c d"), sc(1, 4));
    assert_eq!(m("par", "party"), sc(2, 0));
    assert_eq!(m("par", "a/party"), sc(3, 2));
    assert_eq!(m("art", "party"), sc(4, 1));
    assert_eq!(m("xyz", "feat/pi-direct-sdk"), None);
    assert_eq!(m("ab", "xabxab-yy"), sc(4, 1));
    assert_eq!(m("ab", "xxab x-ab-y"), sc(1, 7));
    assert_eq!(m("pi", "pi-a pi-b"), sc(1, 0));
    for t in ["x/pi", "x-pi", "x_pi", "x pi", "x.pi", "x:pi"] {
        assert_eq!(m("pi", t), sc(1, 2), "{t}");
    }
    assert_eq!(m("202", "#202 feat"), sc(1, 1));
    let pr = "#202 feat(server): replace Pi ACP with direct SDK provider";
    assert_eq!(m("pi", pr).unwrap().tier, 1);
    assert_eq!(m("202", pr), sc(1, 1));
    assert_eq!(m("replace", pr).unwrap().tier, 1);
}

#[test]
fn does_not_assemble_a_word_across_whitespace() {
    for sep in [" ", "\t", "\n", "\u{a0}"] {
        assert_eq!(m("terminal", &format!("term{sep}inal")), None);
    }
    assert_eq!(
        score_match(
            "terminal",
            "Let me diagnose this problem in a diagnose this problem and",
            MatchOptions {
                fuzzy: fuzzy_policy_for_token("terminal"),
                subsequence: true
            }
        ),
        None
    );
}

#[test]
fn subsequences_stay_within_a_word() {
    assert_eq!(m("trmnl", "Fix terminal resizing"), sp(5, 4, 8));
    assert_eq!(m("abc", "AgentBrowserConfig").unwrap().tier, 5);
    assert_eq!(m("pasbab", "paseo-babysit").unwrap().tier, 5);
    assert_eq!(m("confg", "check configuration").unwrap().offset, 6);
}

#[test]
fn typo_tolerance() {
    assert_eq!(m("bulling", "add stripe billing"), None);
    assert_eq!(
        mf("brnach", "brnach is not branch", 1, false).unwrap().tier,
        1
    );
    assert_eq!(mf("brnach", "switch the branch", 1, false).unwrap().tier, 6);
    assert_eq!(m("confg", "revamp configuration editor").unwrap().tier, 5);
    assert_eq!(m("confug", "revamp configuration editor"), None);
    assert_eq!(
        mf("confug", "revamp configuration editor", 2, false)
            .unwrap()
            .tier,
        6
    );
    assert_eq!(
        score_match(
            "confg",
            "revamp configuration editor",
            MatchOptions {
                fuzzy: None,
                subsequence: false
            }
        ),
        None
    );
    assert_eq!(
        score_match(
            "config",
            "revamp configuration editor",
            MatchOptions {
                fuzzy: None,
                subsequence: false
            }
        )
        .unwrap()
        .tier,
        3
    );
    assert_eq!(mf("bulling", "add stripe billing", 2, false), sp(6, 11, 1));
    let fuzzy = mf("bilingg", "billing", 2, false).unwrap();
    let worst_exact = m("iln", "billing").unwrap();
    assert!(compare_match_scores(&worst_exact, &fuzzy).is_lt());
    assert_eq!(mf("stpie", "add stripe billing", 2, false).unwrap().tier, 6);
    assert_eq!(mf("stpie", "add stripe billing", 1, false), None);
    assert_eq!(mf("zzzzzz", "add stripe billing", 2, false), None);
}

#[test]
fn fuzzy_policy_per_token() {
    assert_eq!(fuzzy_policy_for_token("pi"), None);
    assert_eq!(
        fuzzy_policy_for_token("main"),
        Some(FuzzyPolicy {
            max_edits: 1,
            transpositions_only: true
        })
    );
    assert_eq!(
        fuzzy_policy_for_token("stipe"),
        Some(FuzzyPolicy {
            max_edits: 1,
            transpositions_only: false
        })
    );
    assert_eq!(
        fuzzy_policy_for_token("configration"),
        Some(FuzzyPolicy {
            max_edits: 2,
            transpositions_only: false
        })
    );
    assert_eq!(mp("mian", "switch to main").unwrap().tier, 6);
    for near in ["mial", "rain", "maid"] {
        assert_eq!(mp(near, "switch to main"), None, "{near}");
    }
    assert_eq!(mp("mian", "feat/mainline-fix").unwrap().tier, 6);
}

#[test]
fn path_match() {
    assert!(score_path_match(
        "skills/",
        "something/something-else/skills/paseo-advisor/SKILL.md"
    )
    .is_some());
    assert_eq!(
        score_path_match("blank page editor", "blankpage/editor"),
        sc(0, 0)
    );
    assert_eq!(
        score_path_match("blank page editor", "projects/blankpage/editor"),
        sc(4, 9)
    );
}

#[test]
fn text_fields() {
    let o = TextFieldsOptions::default();
    assert!(score_text_fields("stripe billing", &["add stripe billing system"], o).is_some());
    assert!(score_text_fields("stripe rosetta", &["add stripe billing system"], o).is_none());
    assert!(score_text_fields("stripe main", &["add stripe billing", "main"], o).is_some());
    let typo = TextFieldsOptions {
        typo_tolerant: true,
        subsequence: true,
    };
    assert!(score_text_fields("stipe bulling", &["add stripe billing"], typo).is_some());
    assert!(score_text_fields("stipe bulling", &["add stripe billing"], o).is_none());
    let no_sub = TextFieldsOptions {
        typo_tolerant: false,
        subsequence: false,
    };
    let fields = ["Label as Design"];
    assert!(score_text_fields("lab des", &fields, no_sub).is_some());
    assert!(score_text_fields("labdes", &fields, no_sub).is_none());
    assert!(score_text_fields("labdes", &fields, o).is_none());
    assert!(score_text_fields("lbl dsgn", &fields, o).is_some());
    assert_eq!(
        score_text_fields("   ", &["x"], o),
        Some(MatchScore {
            tier: 0,
            offset: 0,
            spread: Some(0)
        })
    );
}

#[test]
fn compare_scores() {
    let s = |tier, offset| MatchScore {
        tier,
        offset,
        spread: None,
    };
    assert!(compare_match_scores(&s(1, 10), &s(2, 0)).is_lt());
    assert!(compare_match_scores(&s(3, 0), &s(1, 99)).is_gt());
    assert!(compare_match_scores(&s(1, 0), &s(1, 5)).is_lt());
    assert!(compare_match_scores(&s(1, 5), &s(1, 5)).is_eq());
}

#[test]
fn ranges_markup() {
    let d = MatchOptions::default;
    assert_eq!(
        mark_up("stripe", "add stripe billing", d()),
        "add [stripe] billing"
    );
    assert_eq!(mark_up("bill", "unbilled usage", d()), "un[bill]ed usage");
    assert_eq!(mark_up("main", "main", d()), "[main]");
    assert_eq!(mark_up("confg", "configuration", d()), "[conf]i[g]uration");
    assert_eq!(
        mark_up("confg", "check configuration", d()),
        "check [conf]i[g]uration"
    );
    assert_eq!(
        mark_up(
            "bulling",
            "add stripe billing",
            MatchOptions {
                fuzzy: fuzzy_policy_for_token("bulling"),
                subsequence: true
            }
        ),
        "add stripe [billing]"
    );
    assert_eq!(
        mark_up(
            "mian",
            "switch to main",
            MatchOptions {
                fuzzy: fuzzy_policy_for_token("mian"),
                subsequence: true
            }
        ),
        "switch to [main]"
    );
    assert!(match_ranges(
        "",
        "anything",
        &MatchScore {
            tier: 0,
            offset: 0,
            spread: None
        }
    )
    .is_empty());
}

#[test]
fn history_query() {
    assert!(matches_agent_history_query(
        "brnach",
        Some("ws"),
        Some("Fix the branch picker"),
        None,
        "paseo"
    ));
    assert!(matches_agent_history_query(
        "paseo",
        None,
        None,
        Some("main"),
        "paseo"
    ));
    assert!(!matches_agent_history_query(
        "zebra",
        Some("ws"),
        Some("title"),
        Some("main"),
        "paseo"
    ));
}
