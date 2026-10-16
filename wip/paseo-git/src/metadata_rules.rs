//! Git metadata event rules (`git-metadata-event-rules.ts`): which paths under
//! a git dir matter to workspace snapshots, and how a change is routed.

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitMetadataScope {
    Worktree,
    Common,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuleScope {
    Both,
    Worktree,
    Common,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Match {
    Exact,
    Prefix,
    Suffix,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Route {
    Ignore,
    Owner { refresh_base: bool },
    All { refresh_base: bool },
    LocalRef,
    RemoteRef,
}

#[derive(Debug, Clone, Copy)]
pub struct GitMetadataEventRule {
    pub id: &'static str,
    scope: RuleScope,
    matcher: Match,
    pub path: &'static str,
    route: Route,
    prune: bool,
}

const fn rule(
    id: &'static str,
    scope: RuleScope,
    matcher: Match,
    path: &'static str,
    route: Route,
    prune: bool,
) -> GitMetadataEventRule {
    GitMetadataEventRule {
        id,
        scope,
        matcher,
        path,
        route,
        prune,
    }
}

pub static GIT_METADATA_EVENT_RULES: &[GitMetadataEventRule] = &[
    rule(
        "lock",
        RuleScope::Both,
        Match::Suffix,
        ".lock",
        Route::Ignore,
        false,
    ),
    rule(
        "worktree-index",
        RuleScope::Worktree,
        Match::Exact,
        "index",
        Route::Owner {
            refresh_base: false,
        },
        false,
    ),
    rule(
        "worktree-head",
        RuleScope::Worktree,
        Match::Exact,
        "HEAD",
        Route::Owner { refresh_base: true },
        false,
    ),
    rule(
        "worktree-config",
        RuleScope::Worktree,
        Match::Exact,
        "config.worktree",
        Route::Owner { refresh_base: true },
        false,
    ),
    rule(
        "paseo-worktree-metadata",
        RuleScope::Worktree,
        Match::Exact,
        "paseo/worktree.json",
        Route::Owner { refresh_base: true },
        false,
    ),
    rule(
        "main-index",
        RuleScope::Common,
        Match::Exact,
        "index",
        Route::Owner {
            refresh_base: false,
        },
        false,
    ),
    rule(
        "main-head",
        RuleScope::Common,
        Match::Exact,
        "HEAD",
        Route::Owner { refresh_base: true },
        false,
    ),
    rule(
        "local-branch",
        RuleScope::Common,
        Match::Prefix,
        "refs/heads/",
        Route::LocalRef,
        false,
    ),
    rule(
        "remote-branch",
        RuleScope::Common,
        Match::Prefix,
        "refs/remotes/",
        Route::RemoteRef,
        false,
    ),
    rule(
        "packed-refs",
        RuleScope::Common,
        Match::Exact,
        "packed-refs",
        Route::All { refresh_base: true },
        false,
    ),
    rule(
        "reftable",
        RuleScope::Common,
        Match::Prefix,
        "reftable/",
        Route::All { refresh_base: true },
        false,
    ),
    rule(
        "shared-config",
        RuleScope::Common,
        Match::Exact,
        "config",
        Route::All { refresh_base: true },
        false,
    ),
    rule(
        "worktree-administration",
        RuleScope::Common,
        Match::Prefix,
        "worktrees/",
        Route::Ignore,
        false,
    ),
    rule(
        "hooks",
        RuleScope::Common,
        Match::Prefix,
        "hooks/",
        Route::Ignore,
        true,
    ),
    rule(
        "ref-logs",
        RuleScope::Common,
        Match::Prefix,
        "logs/",
        Route::Ignore,
        true,
    ),
    rule(
        "object-database",
        RuleScope::Common,
        Match::Prefix,
        "objects/",
        Route::Ignore,
        true,
    ),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum RefNamespace {
    Local,
    Remote,
}

/// Serializes to the TS shape: `{kind:"ignore"}`, `{kind:"owner"|"all",refreshBase}`
/// or `{kind:"ref",namespace,ref}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum GitMetadataEffect {
    Ignore,
    #[serde(rename_all = "camelCase")]
    Owner {
        refresh_base: bool,
    },
    #[serde(rename_all = "camelCase")]
    All {
        refresh_base: bool,
    },
    Ref {
        namespace: RefNamespace,
        #[serde(rename = "ref")]
        reference: String,
    },
}

impl GitMetadataEventRule {
    fn applies_to(&self, scope: GitMetadataScope) -> bool {
        matches!(
            (self.scope, scope),
            (RuleScope::Both, _)
                | (RuleScope::Worktree, GitMetadataScope::Worktree)
                | (RuleScope::Common, GitMetadataScope::Common)
        )
    }

    fn matches(&self, path: &str) -> bool {
        match self.matcher {
            Match::Exact => path == self.path,
            Match::Suffix => path.ends_with(self.path),
            Match::Prefix => {
                path == &self.path[..self.path.len() - 1] || path.starts_with(self.path)
            }
        }
    }
}

/// Directory names the watcher can prune, derived from the same rule inventory.
pub fn get_pruned_git_metadata_paths(scope: GitMetadataScope) -> Vec<String> {
    GIT_METADATA_EVENT_RULES
        .iter()
        .filter(|r| r.applies_to(scope) && r.prune && r.matcher == Match::Prefix)
        .map(|r| r.path[..r.path.len() - 1].to_string())
        .collect()
}

pub fn classify_git_metadata_path(scope: GitMetadataScope, input_path: &str) -> GitMetadataEffect {
    let path = input_path.replace('\\', "/");
    let Some(rule) = GIT_METADATA_EVENT_RULES
        .iter()
        .find(|r| r.applies_to(scope) && r.matches(&path))
    else {
        return GitMetadataEffect::Ignore;
    };
    match rule.route {
        Route::Ignore => GitMetadataEffect::Ignore,
        Route::Owner { refresh_base } => GitMetadataEffect::Owner { refresh_base },
        Route::All { refresh_base } => GitMetadataEffect::All { refresh_base },
        Route::LocalRef | Route::RemoteRef => GitMetadataEffect::Ref {
            namespace: if rule.route == Route::LocalRef {
                RefNamespace::Local
            } else {
                RefNamespace::Remote
            },
            reference: path.get(rule.path.len()..).unwrap_or("").to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use GitMetadataScope::{Common, Worktree};

    #[test]
    fn has_unique_rule_names() {
        let mut ids: Vec<_> = GIT_METADATA_EVENT_RULES.iter().map(|r| r.id).collect();
        let len = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), len);
    }

    #[test]
    fn derives_watcher_pruning_from_the_same_inventory() {
        assert_eq!(
            get_pruned_git_metadata_paths(Common),
            vec!["hooks", "logs", "objects"]
        );
        assert!(get_pruned_git_metadata_paths(Worktree).is_empty());
    }

    #[test]
    fn classifies_paths_to_the_wire_shape() {
        let cases = [
            (
                Worktree,
                "index",
                json!({"kind":"owner","refreshBase":false}),
            ),
            (Worktree, "HEAD", json!({"kind":"owner","refreshBase":true})),
            (
                Worktree,
                "config.worktree",
                json!({"kind":"owner","refreshBase":true}),
            ),
            (
                Worktree,
                "paseo/worktree.json",
                json!({"kind":"owner","refreshBase":true}),
            ),
            (Worktree, "COMMIT_EDITMSG", json!({"kind":"ignore"})),
            (Worktree, "ORIG_HEAD", json!({"kind":"ignore"})),
            (Common, "index", json!({"kind":"owner","refreshBase":false})),
            (Common, "HEAD", json!({"kind":"owner","refreshBase":true})),
            (
                Common,
                "refs/heads/feature",
                json!({"kind":"ref","namespace":"local","ref":"feature"}),
            ),
            (
                Common,
                "refs/remotes/origin/main",
                json!({"kind":"ref","namespace":"remote","ref":"origin/main"}),
            ),
            (
                Common,
                "packed-refs",
                json!({"kind":"all","refreshBase":true}),
            ),
            (
                Common,
                "reftable/tables.list",
                json!({"kind":"all","refreshBase":true}),
            ),
            (Common, "config", json!({"kind":"all","refreshBase":true})),
            (Common, "worktrees/new/index", json!({"kind":"ignore"})),
            (Common, "hooks/pre-commit", json!({"kind":"ignore"})),
            (Common, "logs/refs/heads/main", json!({"kind":"ignore"})),
            (Common, "objects/ab/cdef", json!({"kind":"ignore"})),
            (Common, "refs/tags/v1", json!({"kind":"ignore"})),
            (Common, "FETCH_HEAD", json!({"kind":"ignore"})),
            (Common, "packed-refs.lock", json!({"kind":"ignore"})),
        ];
        assert_eq!(cases.len(), 20);
        for (scope, path, expected) in cases {
            let got = serde_json::to_value(classify_git_metadata_path(scope, path)).unwrap();
            assert_eq!(got, expected, "{path}");
        }
    }
}
