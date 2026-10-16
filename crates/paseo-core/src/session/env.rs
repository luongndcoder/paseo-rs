//! What the session layer reads from the rest of the daemon: the agent manager, the stores,
//! the directory sequencer and a few hooks for domains that are still TS-resident.
//!
//! `AgentDirectory` (`agent/directory.ts`), `buildProjectPlacementForWorkspaceId` and
//! `buildActiveProjectPlacementsByWorkspaceId` live here because they only compose these
//! services.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use indexmap::IndexMap;
use paseo_directory::DirectorySync;
use paseo_store::workspace_registry::WorkspaceKind;
use paseo_store::{
    AgentStore, ProjectRegistry, StoredAgentRecord, WorkspaceRecord, WorkspaceRegistry,
};
use serde_json::{json, Map, Value};

use crate::agent_payload::{build_stored_agent_payload, enrich_agent_payload, to_agent_payload};
use crate::agent_updates::AgentUpdatesDeps;
use crate::manager::AgentManager;
use crate::time::Clock;

/// Git facts a workspace placement falls back to when the record lacks them
/// (COMPAT(workspacePlacementBackfill): added in v0.1.107, remove after 2027-01-15).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitFacts {
    pub current_branch: Option<String>,
    pub repo_root: Option<String>,
}

/// Seams to domains that are not Rust yet. Every method defaults to "nothing there".
#[async_trait]
pub trait SessionHooks: Send + Sync + 'static {
    /// `emitWorkspaceUpdateForWorkspaceId`: the workspace-updates machine lives in the workspace
    /// session; until it moves, the host forwards the request to it.
    async fn emit_workspace_update(&self, _workspace_id: &str) -> Result<(), String> {
        Ok(())
    }

    /// `workspaceGitService.peekSnapshot(cwd)`.
    fn peek_git_facts(&self, _cwd: &str) -> Option<GitFacts> {
        None
    }

    /// `buildProjectDescriptor` of a project registry record for `project.update`.
    async fn project_descriptor(&self, project: Value) -> Result<Value, String> {
        Ok(project)
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct NoSessionHooks;

#[async_trait]
impl SessionHooks for NoSessionHooks {}

/// Shared by every session of the daemon.
#[derive(Clone)]
pub struct SessionServices {
    pub manager: Arc<AgentManager>,
    pub store: Arc<AgentStore>,
    pub workspaces: Arc<WorkspaceRegistry>,
    pub projects: Arc<ProjectRegistry>,
    /// Daemon-global latest-state sequencer. One per boot: the generation is per boot.
    pub directory: Arc<DirectorySync>,
    pub hooks: Arc<dyn SessionHooks>,
    pub clock: Arc<dyn Clock>,
}

/// `AgentDirectory.list` options.
#[derive(Debug, Clone, Default)]
pub struct ListOptions {
    pub labels: Option<IndexMap<String, String>>,
    pub include_archived: bool,
    pub include_background: bool,
    pub include_unavailable_persisted: bool,
}

impl SessionServices {
    fn registered_provider_check(&self) -> impl Fn(&str) -> bool {
        let ids: HashSet<String> = self.manager.registered_provider_ids().into_iter().collect();
        move |provider: &str| ids.contains(provider)
    }

    pub async fn enrich(&self, mut payload: Value) -> Result<Value, String> {
        let id = payload
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let record = self.store.get(&id).await.map_err(|e| e.to_string())?;
        enrich_agent_payload(&mut payload, record.as_ref());
        Ok(payload)
    }

    pub fn stored_payload(&self, record: &StoredAgentRecord) -> Value {
        build_stored_agent_payload(record, &self.registered_provider_check())
    }

    /// `AgentDirectory.includes`: public discovery visibility of one payload.
    pub async fn directory_includes(&self, agent: &Value, include_background: bool) -> bool {
        if agent.get("internal").and_then(Value::as_bool) == Some(true) {
            return false;
        }
        let workspace_id = agent
            .get("workspaceId")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());
        if include_background {
            return true;
        }
        let Some(workspace_id) = workspace_id else {
            return true;
        };
        !self
            .workspaces
            .get(workspace_id)
            .await
            .is_some_and(|w| w.background)
    }

    /// `AgentDirectory.list`: live and stored agents, filtered by `options` and by provider
    /// visibility for the asking source.
    pub async fn list_agent_payloads(
        &self,
        options: &ListOptions,
        provider_visible: &(dyn Fn(&str) -> bool + Send + Sync),
    ) -> Result<Vec<Value>, String> {
        let live = self.manager.list_agents(false);
        let live_ids: HashSet<&str> = live.iter().map(|a| a.id.as_str()).collect();
        let background_ids: HashSet<String> = if options.include_background {
            HashSet::new()
        } else {
            self.workspaces
                .list()
                .await
                .into_iter()
                .filter(|w| w.background)
                .map(|w| w.workspace_id)
                .collect()
        };
        let matches =
            |archived: bool, workspace_id: Option<&str>, labels: &IndexMap<String, String>| {
                (options.include_archived || !archived)
                    && (options.include_background
                        || workspace_id.is_none_or(|w| w.is_empty() || !background_ids.contains(w)))
                    && options
                        .labels
                        .as_ref()
                        .is_none_or(|wanted| wanted.iter().all(|(k, v)| labels.get(k) == Some(v)))
            };
        let registered = self.registered_provider_check();
        let stored: Vec<Value> = self
            .store
            .list()
            .await
            .map_err(|e| e.to_string())?
            .iter()
            .filter(|r| r.internal != Some(true) && !live_ids.contains(r.id.as_str()))
            .filter(|r| {
                matches(
                    r.archived_at.value().is_some_and(|s| !s.is_empty()),
                    r.workspace_id.as_deref(),
                    &r.labels,
                )
            })
            .filter(|r| options.include_unavailable_persisted || registered(&r.provider))
            .map(|r| build_stored_agent_payload(r, &registered))
            .collect();
        let mut active = Vec::new();
        for agent in live
            .iter()
            .filter(|a| matches(false, a.workspace_id.as_deref(), &a.labels))
        {
            active.push(self.enrich(to_agent_payload(agent)).await?);
        }
        Ok(active
            .into_iter()
            .chain(stored)
            .filter(|agent| {
                provider_visible(
                    agent
                        .get("provider")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                )
            })
            .collect())
    }

    /// `buildProjectPlacementForWorkspace`.
    fn placement(
        &self,
        workspace: &WorkspaceRecord,
        project: &paseo_store::ProjectRecord,
    ) -> Value {
        let facts = self.hooks.peek_git_facts(&workspace.cwd);
        let checkout = checkout_from_persisted_placement(
            workspace,
            facts.as_ref().and_then(|f| f.current_branch.as_deref()),
            facts.as_ref().and_then(|f| f.repo_root.as_deref()),
        );
        json!({
            "projectKey": project.project_id,
            "projectName": project.resolved_display_name(),
            "workspaceName": workspace.resolved_display_name(),
            "checkout": checkout,
        })
    }

    /// `buildProjectPlacementForWorkspaceId`: `None` when the workspace or its project is gone.
    pub async fn project_placement(&self, workspace_id: &str) -> Option<Value> {
        let workspace = self.workspaces.get(workspace_id).await?;
        let project = self.projects.get(&workspace.project_id).await?;
        Some(self.placement(&workspace, &project))
    }

    /// `buildActiveProjectPlacementsByWorkspaceId`: placements of every active workspace of an
    /// active project.
    pub async fn active_placements(&self) -> HashMap<String, Value> {
        let workspaces = self.workspaces.list().await;
        let projects: HashMap<String, paseo_store::ProjectRecord> = self
            .projects
            .list()
            .await
            .into_iter()
            .filter(|p| p.archived_at.is_none())
            .map(|p| (p.project_id.clone(), p))
            .collect();
        workspaces
            .iter()
            .filter(|w| w.archived_at.is_none())
            .filter_map(|w| {
                projects
                    .get(&w.project_id)
                    .map(|p| (w.workspace_id.clone(), self.placement(w, p)))
            })
            .collect()
    }
}

/// `checkoutFromPersistedWorkspacePlacement`.
pub fn checkout_from_persisted_placement(
    workspace: &WorkspaceRecord,
    fallback_branch: Option<&str>,
    fallback_worktree_root: Option<&str>,
) -> Value {
    let opt = |v: Option<&str>| v.map_or(Value::Null, |s| Value::String(s.to_owned()));
    let mut out = Map::new();
    out.insert("cwd".into(), workspace.cwd.clone().into());
    if workspace.kind == WorkspaceKind::Directory {
        out.insert("isGit".into(), false.into());
        out.insert("currentBranch".into(), Value::Null);
        out.insert("remoteUrl".into(), Value::Null);
        out.insert("worktreeRoot".into(), Value::Null);
        out.insert("isPaseoOwnedWorktree".into(), false.into());
        out.insert("mainRepoRoot".into(), Value::Null);
        return Value::Object(out);
    }
    out.insert(
        "currentBranch".into(),
        opt(workspace.branch.as_deref().or(fallback_branch)),
    );
    out.insert("remoteUrl".into(), Value::Null);
    out.insert(
        "worktreeRoot".into(),
        opt(Some(
            workspace
                .worktree_root
                .as_deref()
                .or(fallback_worktree_root)
                .unwrap_or(&workspace.cwd),
        )),
    );
    out.insert("isGit".into(), true.into());
    match (&workspace.main_repo_root, workspace.is_paseo_owned_worktree) {
        (Some(root), true) => {
            out.insert("isPaseoOwnedWorktree".into(), true.into());
            out.insert("mainRepoRoot".into(), root.clone().into());
        }
        (root, _) => {
            out.insert("isPaseoOwnedWorktree".into(), false.into());
            out.insert("mainRepoRoot".into(), opt(root.as_deref()));
        }
    }
    Value::Object(out)
}

/// [`AgentUpdatesDeps`] over the session services.
pub struct SessionDeps {
    pub services: SessionServices,
}

#[async_trait]
impl AgentUpdatesDeps for SessionDeps {
    async fn includes_workspace(&self, agent: &Value, include_background: bool) -> bool {
        self.services
            .directory_includes(agent, include_background)
            .await
    }

    async fn enrich_agent_payload(&self, payload: Value) -> Result<Value, String> {
        self.services.enrich(payload).await
    }

    fn build_stored_agent_payload(&self, record: &StoredAgentRecord) -> Result<Value, String> {
        Ok(self.services.stored_payload(record))
    }

    async fn build_project_placement(&self, workspace_id: &str) -> Result<Option<Value>, String> {
        Ok(self.services.project_placement(workspace_id).await)
    }

    async fn emit_workspace_update(&self, workspace_id: &str) -> Result<(), String> {
        self.services
            .hooks
            .emit_workspace_update(workspace_id)
            .await
    }

    fn sequence_agent_update(
        &self,
        payload: Value,
        entry: Option<Value>,
        agent_id: &str,
        include_sequence: bool,
        include_background: bool,
    ) -> Value {
        self.services.directory.sequence_agent_update(
            payload,
            entry,
            agent_id,
            include_sequence,
            include_background,
        )
    }
}
