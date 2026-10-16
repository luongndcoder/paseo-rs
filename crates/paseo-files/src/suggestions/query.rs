//! Query parsing for the directory suggestion engine.

use super::{BlankQueryBehavior, PathQueryPolicy};
use crate::path_utils::{is_path_inside_root, normalize_relative_path, resolve_cwd};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct QueryPlan {
    pub is_path_query: bool,
    pub parent_part: String,
    pub search_term: String,
    pub normalized_query: String,
    pub browse_exact_path: bool,
}

struct NormalizedInput {
    typed: String,
    normalized: String,
    rooted: bool,
    is_absolute: bool,
}

pub(super) fn parse_query(
    query: &str,
    root: &Path,
    configured_root: &Path,
    policy: PathQueryPolicy,
    aliases: &[String],
    blank: BlankQueryBehavior,
) -> Option<QueryPlan> {
    let input = normalize_query_input(query, root, configured_root, aliases)?;
    let normalized = input.normalized;

    if normalized.is_empty() {
        let explicitly_browse_root = input.rooted || input.typed == ".";
        if !explicitly_browse_root && blank != BlankQueryBehavior::Children {
            return None;
        }
        return Some(QueryPlan {
            is_path_query: true,
            parent_part: String::new(),
            search_term: String::new(),
            normalized_query: String::new(),
            browse_exact_path: false,
        });
    }
    if input.is_absolute && is_filesystem_root(root) && !normalized.contains('/') {
        return Some(QueryPlan {
            is_path_query: true,
            parent_part: normalized.clone(),
            search_term: String::new(),
            normalized_query: normalized,
            browse_exact_path: true,
        });
    }
    let is_path_query =
        input.rooted || (policy == PathQueryPolicy::Slashes && normalized.contains('/'));
    let slash = normalized.rfind('/');
    let (parent_part, search_term) = match (is_path_query, slash) {
        (true, Some(i)) => (normalized[..i].to_string(), normalized[i + 1..].to_string()),
        _ => (String::new(), normalized.clone()),
    };
    Some(QueryPlan {
        is_path_query,
        parent_part,
        search_term,
        normalized_query: normalized,
        browse_exact_path: false,
    })
}

fn normalize_query_input(
    query: &str,
    root: &Path,
    configured_root: &Path,
    aliases: &[String],
) -> Option<NormalizedInput> {
    let typed = query.trim().replace('\\', "/");
    let mut normalized = typed.clone();
    let mut rooted = false;
    let mut is_absolute = false;
    for alias in aliases {
        if normalized == *alias || normalized.starts_with(&format!("{alias}/")) {
            rooted = true;
            normalized = normalized[alias.len()..]
                .trim_start_matches('/')
                .to_string();
            break;
        }
    }
    if normalized.starts_with('/') {
        is_absolute = true;
        let browse_absolute_directory = normalized.ends_with('/');
        let absolute = resolve_cwd(&normalized);
        let query_root = if is_path_inside_root(root, &absolute) {
            root
        } else if is_path_inside_root(configured_root, &absolute) {
            configured_root
        } else {
            return None;
        };
        rooted = true;
        normalized = normalize_relative_path(query_root, &absolute);
        if browse_absolute_directory && normalized != "." {
            normalized.push('/');
        }
    }
    if normalized.starts_with("./") {
        rooted = true;
        normalized = normalized[2..].trim_start_matches('/').to_string();
    }
    normalized = collapse_slashes(&normalized);
    if normalized == "." && (rooted || typed == ".") {
        normalized.clear();
    }
    Some(NormalizedInput {
        typed,
        normalized,
        rooted,
        is_absolute,
    })
}

fn collapse_slashes(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut prev_slash = false;
    for c in value.chars() {
        if c == '/' {
            if prev_slash {
                continue;
            }
            prev_slash = true;
        } else {
            prev_slash = false;
        }
        out.push(c);
    }
    out
}

fn is_filesystem_root(path: &Path) -> bool {
    path == Path::new("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(
        q: &str,
        policy: PathQueryPolicy,
        aliases: &[&str],
        blank: BlankQueryBehavior,
    ) -> Option<QueryPlan> {
        let aliases: Vec<String> = aliases.iter().map(|s| s.to_string()).collect();
        parse_query(q, Path::new("/r"), Path::new("/r"), policy, &aliases, blank)
    }

    #[test]
    fn blank_query_follows_behavior_and_explicit_roots() {
        use BlankQueryBehavior::*;
        use PathQueryPolicy::*;
        assert_eq!(plan("", Rooted, &["~"], None), Option::None);
        assert!(plan("~", Rooted, &["~"], None)
            .unwrap()
            .normalized_query
            .is_empty());
        assert!(plan("", Slashes, &[], Children).is_some());
        assert!(plan(".", Slashes, &[], None).is_some());
    }

    #[test]
    fn slashes_policy_only_treats_slash_queries_as_paths() {
        use BlankQueryBehavior::None as NoBlank;
        let p = plan("src/co", PathQueryPolicy::Slashes, &[], NoBlank).unwrap();
        assert!(p.is_path_query);
        assert_eq!(
            (p.parent_part.as_str(), p.search_term.as_str()),
            ("src", "co")
        );
        let p = plan("src", PathQueryPolicy::Slashes, &[], NoBlank).unwrap();
        assert!(!p.is_path_query);
        let p = plan("src/co", PathQueryPolicy::Rooted, &[], NoBlank).unwrap();
        assert!(!p.is_path_query);
        let p = plan("./src/co", PathQueryPolicy::Rooted, &[], NoBlank).unwrap();
        assert!(p.is_path_query);
        assert_eq!(p.normalized_query, "src/co");
    }

    #[test]
    fn absolute_queries_map_into_the_root_or_are_refused() {
        use BlankQueryBehavior::None as NoBlank;
        let p = plan("/r/projects/", PathQueryPolicy::Rooted, &[], NoBlank).unwrap();
        assert_eq!(p.normalized_query, "projects/");
        assert!(plan("/elsewhere", PathQueryPolicy::Rooted, &[], NoBlank).is_none());
        let p = plan("/r", PathQueryPolicy::Rooted, &[], NoBlank).unwrap();
        assert!(p.normalized_query.is_empty());
    }

    #[test]
    fn alias_strips_prefix_and_marks_rooted() {
        use BlankQueryBehavior::None as NoBlank;
        let p = plan("~/Developer/pas", PathQueryPolicy::Rooted, &["~"], NoBlank).unwrap();
        assert_eq!(p.parent_part, "Developer");
        assert_eq!(p.search_term, "pas");
        assert!(p.is_path_query);
    }
}
