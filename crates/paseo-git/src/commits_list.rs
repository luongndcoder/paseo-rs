//! Request dispatch for `checkout.commits.list.request`: one RPC, several
//! views (git action, conflict file, stash detail, compare detail, blame,
//! graph, plain commit list).

use serde::Deserialize;
use serde_json::{json, Value};

use crate::commits::list_checkout_commits;
use crate::error::{to_checkout_error, GitError};
use crate::graph::{
    get_git_compare_detail, get_git_conflict_file, get_git_file_blame, get_git_stash_detail,
    query_git_graph_and_refs, GraphQuery,
};
use crate::log_actions::{execute_git_log_action, GitLogAction};
use crate::paths::expand_tilde;
use crate::repo::CheckoutContext;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommitsListRequest {
    pub cwd: String,
    #[serde(default)]
    pub include_graph: Option<bool>,
    #[serde(default)]
    pub all_branches: Option<bool>,
    #[serde(default, rename = "ref")]
    pub r#ref: Option<String>,
    #[serde(default)]
    pub search: Option<String>,
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub file_path: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub blame_path: Option<String>,
    #[serde(default)]
    pub git_action: Option<GitLogAction>,
    #[serde(default)]
    pub conflict_file_path: Option<String>,
    #[serde(default)]
    pub stash_index: Option<usize>,
    #[serde(default)]
    pub compare_ref: Option<String>,
    pub request_id: String,
}

pub struct CommitsListOutcome {
    /// The `checkout.commits.list.response` payload.
    pub payload: Value,
    /// True when a git action ran; the caller refreshes snapshots and diffs.
    pub git_mutated: bool,
}

fn truthy(v: &Option<String>) -> bool {
    v.as_deref().is_some_and(|s| !s.is_empty())
}

fn empty_with(cwd: &str, request_id: &str, extra: Value) -> Value {
    let mut base =
        json!({"cwd": cwd, "baseRef": null, "commits": [], "error": null, "requestId": request_id});
    if let (Value::Object(b), Value::Object(e)) = (&mut base, extra) {
        b.extend(e);
    }
    base
}

pub async fn handle_commits_list_request(
    req: &CommitsListRequest,
    ctx: &CheckoutContext,
) -> CommitsListOutcome {
    let cwd = req.cwd.as_str();
    let expanded = expand_tilde(cwd);
    let rid = req.request_id.as_str();
    let mut git_mutated = false;

    let result: Result<Value, GitError> = async {
        if let Some(action) = &req.git_action {
            let action_result = execute_git_log_action(&expanded, action).await?;
            git_mutated = true;
            return Ok(empty_with(cwd, rid, json!({"actionResult": action_result})));
        }
        if truthy(&req.conflict_file_path) {
            let file = get_git_conflict_file(&expanded, req.conflict_file_path.as_deref().unwrap_or_default()).await?;
            return Ok(empty_with(cwd, rid, json!({"conflictFile": file})));
        }
        if let Some(index) = req.stash_index {
            let detail = get_git_stash_detail(&expanded, index).await?;
            return Ok(empty_with(cwd, rid, json!({"stashDetail": detail})));
        }
        if truthy(&req.compare_ref) {
            let detail = get_git_compare_detail(&expanded, req.compare_ref.as_deref().unwrap_or_default()).await?;
            return Ok(empty_with(cwd, rid, json!({"compareDetail": detail})));
        }
        if truthy(&req.blame_path) {
            let lines = get_git_file_blame(&expanded, req.blame_path.as_deref().unwrap_or_default()).await?;
            return Ok(empty_with(cwd, rid, json!({"blameLines": lines})));
        }
        if req.include_graph == Some(true)
            || req.all_branches.is_some()
            || truthy(&req.r#ref)
            || truthy(&req.search)
            || truthy(&req.author)
            || truthy(&req.file_path)
        {
            let graph = query_git_graph_and_refs(
                &expanded,
                &GraphQuery {
                    all_branches: req.all_branches,
                    r#ref: req.r#ref.clone(),
                    search: req.search.clone(),
                    author: req.author.clone(),
                    file_path: req.file_path.clone(),
                    limit: req.limit,
                },
                ctx,
            )
            .await?;
            let mut payload = serde_json::to_value(&graph).map_err(|e| GitError::other(e.to_string()))?;
            if let Value::Object(map) = &mut payload {
                map.insert("cwd".into(), json!(cwd));
                map.insert("error".into(), Value::Null);
                map.insert("requestId".into(), json!(rid));
            }
            return Ok(payload);
        }
        let result = list_checkout_commits(&expanded, ctx).await?;
        Ok(json!({"cwd": cwd, "baseRef": result.base_ref, "commits": result.commits, "error": null, "requestId": rid}))
    }
    .await;

    let payload = match result {
        Ok(p) => p,
        Err(e) => {
            json!({"cwd": cwd, "baseRef": null, "commits": [], "error": to_checkout_error(&e), "requestId": rid})
        }
    };
    CommitsListOutcome {
        payload,
        git_mutated,
    }
}
