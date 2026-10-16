//! Ports of `workspace-registry-bootstrap.test.ts` and
//! `backfill-workspace-id.migration.test.ts`. Git/project-key derivation is host-owned, so a
//! fake `BootstrapHost` stands in for `WorkspaceGitService` + `classifyDirectoryForProjectMembership`.

use async_trait::async_trait;
use paseo_store::bootstrap::{
    backfill_workspace_id_for_legacy_agents, bootstrap_registries, BootstrapHost, BootstrapOptions,
    DirectoryMembership,
};
use paseo_store::{
    AgentStore, Nullable, ProjectKind, ProjectRecord, ProjectRegistry, Result, StoreError,
    StoredAgentRecord, WorkspaceKind, WorkspaceRecord, WorkspaceRegistry,
};
use std::path::PathBuf;
use std::sync::Mutex;

type Classify = Box<dyn Fn(&str) -> Result<DirectoryMembership> + Send + Sync>;

struct FakeHost {
    classify: Classify,
    calls: Mutex<Vec<String>>,
    pins: Mutex<Vec<(String, String)>>,
    pin_fails: bool,
}

fn non_git(cwd: &str) -> DirectoryMembership {
    let name = cwd.rsplit('/').next().unwrap().to_string();
    DirectoryMembership {
        workspace_directory_key: cwd.into(),
        workspace_cwd: cwd.into(),
        workspace_kind: WorkspaceKind::Directory,
        workspace_display_name: name.clone(),
        project_id: cwd.into(),
        project_key: format!("path:{cwd}"),
        project_name: name,
        project_root_path: cwd.into(),
        project_kind: ProjectKind::NonGit,
    }
}

impl FakeHost {
    fn new() -> Self {
        Self::with(Box::new(|cwd| Ok(non_git(cwd))))
    }

    fn with(classify: Classify) -> Self {
        FakeHost {
            classify,
            calls: Mutex::default(),
            pins: Mutex::default(),
            pin_fails: false,
        }
    }
}

#[async_trait]
impl BootstrapHost for FakeHost {
    async fn classify_directory(&self, cwd: &str) -> Result<DirectoryMembership> {
        self.calls.lock().unwrap().push(cwd.to_string());
        (self.classify)(cwd)
    }

    fn pin_worktree_branch_identity(&self, root: &str, branch: &str) -> Result<()> {
        self.pins.lock().unwrap().push((root.into(), branch.into()));
        if self.pin_fails {
            return Err(StoreError::other("pin failed"));
        }
        Ok(())
    }
}

struct Fx {
    tmp: tempfile::TempDir,
    home: PathBuf,
    agents: AgentStore,
    projects: ProjectRegistry,
    workspaces: WorkspaceRegistry,
}

fn fx() -> Fx {
    let tmp = tempfile::tempdir().unwrap();
    // Canonicalize so macOS /var -> /private/var cannot make cwd comparisons differ.
    let root = std::fs::canonicalize(tmp.path()).unwrap();
    let home = root.join(".paseo");
    Fx {
        agents: AgentStore::new(home.join("agents")),
        projects: ProjectRegistry::new(home.join("projects/projects.json")),
        workspaces: WorkspaceRegistry::new(home.join("projects/workspaces.json")),
        home,
        tmp,
    }
}

impl Fx {
    fn dir(&self, name: &str) -> String {
        let p = std::fs::canonicalize(self.tmp.path()).unwrap().join(name);
        std::fs::create_dir_all(&p).unwrap();
        p.to_string_lossy().into_owned()
    }

    async fn run(&self, host: &FakeHost) -> Result<()> {
        bootstrap_registries(BootstrapOptions {
            paseo_home: &self.home,
            agent_store: &self.agents,
            project_registry: &self.projects,
            workspace_registry: &self.workspaces,
            host,
            user_home: Some("/nonexistent-home"),
        })
        .await
    }
}

fn agent(
    id: &str,
    cwd: &str,
    created: &str,
    updated: &str,
    archived_at: Option<&str>,
) -> StoredAgentRecord {
    let mut r = StoredAgentRecord::new(id, "codex", cwd, created);
    r.updated_at = updated.into();
    r.last_activity_at = Some(updated.into());
    r.archived_at = archived_at.map_or(Nullable::Null, |a| Nullable::Value(a.into()));
    r
}

fn ws(id: &str, cwd: &str, created: &str, archived_at: Option<&str>) -> WorkspaceRecord {
    let mut w = WorkspaceRecord::new(
        id,
        "proj",
        cwd,
        WorkspaceKind::Directory,
        id,
        created,
        created,
    );
    w.archived_at = archived_at.map(str::to_string);
    w
}

const T1: &str = "2026-03-01T00:00:00.000Z";
const T2: &str = "2026-03-02T00:00:00.000Z";
const T3: &str = "2026-03-03T00:00:00.000Z";

#[tokio::test]
async fn skips_a_legacy_agent_whose_directory_no_longer_exists() {
    let fx = fx();
    let missing = format!("{}/missing-project", fx.dir("."));
    fx.agents
        .upsert(agent("a", &missing, T1, T1, None))
        .await
        .unwrap();
    let host = FakeHost::with(Box::new(|_| {
        panic!("Git must not inspect a missing directory")
    }));
    fx.run(&host).await.unwrap();
    assert!(fx.projects.list().await.is_empty());
    assert!(fx.workspaces.list().await.is_empty());
}

#[tokio::test]
async fn skips_a_legacy_agent_whose_cwd_is_a_file() {
    let fx = fx();
    let file = format!("{}/not-a-directory", fx.dir("."));
    std::fs::write(&file, "x").unwrap();
    fx.agents
        .upsert(agent("a", &file, T1, T1, None))
        .await
        .unwrap();
    let host = FakeHost::with(Box::new(|_| panic!("Git must not inspect a file")));
    fx.run(&host).await.unwrap();
    assert!(fx.workspaces.list().await.is_empty());
}

#[tokio::test]
async fn propagates_a_git_failure_for_an_existing_legacy_directory() {
    let fx = fx();
    let cwd = fx.dir("project");
    fx.agents
        .upsert(agent("a", &cwd, T1, T1, None))
        .await
        .unwrap();
    let host = FakeHost::with(Box::new(|_| Err(StoreError::other("Git is unavailable"))));
    let err = fx.run(&host).await.unwrap_err();
    assert_eq!(err.to_string(), "Git is unavailable");
    assert!(fx.workspaces.list().await.is_empty());
}

#[tokio::test]
async fn materializes_registries_from_non_archived_agent_records() {
    let fx = fx();
    let project = fx.dir("non-git-project");
    let archived = fx.dir("archived-project");
    fx.agents
        .upsert(agent("agent-1", &project, T1, T2, None))
        .await
        .unwrap();
    fx.agents
        .upsert(agent(
            "agent-2",
            &project,
            "2026-03-01T01:00:00.000Z",
            T3,
            None,
        ))
        .await
        .unwrap();
    fx.agents
        .upsert(agent("agent-archived", &archived, T1, T1, Some(T2)))
        .await
        .unwrap();

    let host = FakeHost::new();
    fx.run(&host).await.unwrap();

    let workspaces = fx.workspaces.list().await;
    assert_eq!(workspaces.len(), 1);
    let w = &workspaces[0];
    assert!(w.workspace_id.starts_with("wks_") && w.workspace_id.len() == 20);
    assert_eq!(
        (w.cwd.as_str(), w.created_at.as_str(), w.updated_at.as_str()),
        (project.as_str(), T1, T3)
    );
    assert_eq!(w.kind, WorkspaceKind::Directory);
    assert_eq!(w.title, None);

    let projects = fx.projects.list().await;
    assert_eq!(projects.len(), 1);
    assert_eq!(projects[0].project_id, project);
    assert_eq!(
        projects[0].project_key.as_deref(),
        Some(format!("path:{project}").as_str())
    );
    assert_eq!(
        (
            projects[0].created_at.as_str(),
            projects[0].updated_at.as_str()
        ),
        (T1, T3)
    );
    assert_eq!(
        host.calls.lock().unwrap().len(),
        2,
        "only the two active agents are classified"
    );
    // Agents got stamped with the new workspace by the backfill.
    assert_eq!(
        fx.agents
            .get("agent-1")
            .await
            .unwrap()
            .unwrap()
            .workspace_id
            .as_deref(),
        Some(w.workspace_id.as_str())
    );
    assert!(fx
        .agents
        .get("agent-archived")
        .await
        .unwrap()
        .unwrap()
        .workspace_id
        .is_none());
}

#[tokio::test]
async fn does_not_rematerialize_when_registry_files_exist() {
    let fx = fx();
    fx.projects.initialize().await;
    fx.workspaces.initialize().await;
    fx.projects
        .upsert(ProjectRecord::new(
            "proj-existing",
            "/tmp/existing",
            ProjectKind::NonGit,
            "existing",
            T1,
            T1,
        ))
        .await
        .unwrap();
    fx.workspaces
        .upsert(ws("ws-existing", "/tmp/existing", T1, None), None)
        .await
        .unwrap();
    fx.agents
        .upsert(agent("agent-1", "/tmp/another-project", T2, T2, None))
        .await
        .unwrap();
    let host = FakeHost::with(Box::new(|_| panic!("must not classify")));
    fx.run(&host).await.unwrap();
    assert_eq!(fx.projects.list().await.len(), 1);
    assert_eq!(fx.workspaces.list().await.len(), 1);
    assert_eq!(fx.workspaces.list().await[0].workspace_id, "ws-existing");
}

#[tokio::test]
async fn materializes_legacy_remote_worktrees_into_one_project() {
    let fx = fx();
    let main = fx.dir("legacy-git-project");
    let tree = fx.dir("legacy-git-project-feature");
    let main_for_host = main.clone();
    let host = FakeHost::with(Box::new(move |cwd| {
        let is_main = cwd == main_for_host;
        Ok(DirectoryMembership {
            workspace_directory_key: cwd.into(),
            workspace_cwd: cwd.into(),
            workspace_kind: if is_main {
                WorkspaceKind::LocalCheckout
            } else {
                WorkspaceKind::Worktree
            },
            workspace_display_name: if is_main {
                "main".into()
            } else {
                "feature/plain".into()
            },
            project_id: "remote:github.com/acme/legacy-project".into(),
            project_key: "remote:github.com/acme/legacy-project".into(),
            project_name: "acme/legacy-project".into(),
            project_root_path: main_for_host.clone(),
            project_kind: ProjectKind::Git,
        })
    }));
    fx.agents
        .upsert(agent("main-agent", &main, T1, T1, None))
        .await
        .unwrap();
    fx.agents
        .upsert(agent("worktree-agent", &tree, T2, T3, None))
        .await
        .unwrap();
    fx.run(&host).await.unwrap();

    let projects = fx.projects.list().await;
    assert_eq!(projects.len(), 1);
    assert_eq!(
        projects[0].project_id,
        "remote:github.com/acme/legacy-project"
    );
    assert_eq!(projects[0].kind, ProjectKind::Git);
    assert_eq!(
        (
            projects[0].created_at.as_str(),
            projects[0].updated_at.as_str()
        ),
        (T1, T3)
    );
    let workspaces = fx.workspaces.list().await;
    assert_eq!(workspaces.len(), 2);
    assert!(workspaces
        .iter()
        .all(|w| w.project_id == "remote:github.com/acme/legacy-project"));
}

#[tokio::test]
async fn migrates_cwd_only_agents_to_the_oldest_same_cwd_workspace() {
    let fx = fx();
    let cwd = fx.dir("non-git-project");
    fx.projects.initialize().await;
    fx.workspaces.initialize().await;
    fx.projects
        .upsert(ProjectRecord::new(
            &cwd,
            &cwd,
            ProjectKind::NonGit,
            "p",
            T1,
            T1,
        ))
        .await
        .unwrap();
    fx.workspaces
        .upsert(ws("ws-newer", &cwd, T2, None), None)
        .await
        .unwrap();
    fx.workspaces
        .upsert(ws("ws-older", &cwd, T1, None), None)
        .await
        .unwrap();
    fx.agents
        .upsert(agent(
            "legacy-agent",
            &cwd,
            "2026-03-01T12:00:00.000Z",
            "2026-03-01T12:00:00.000Z",
            None,
        ))
        .await
        .unwrap();
    fx.run(&FakeHost::new()).await.unwrap();
    assert_eq!(
        fx.agents
            .get("legacy-agent")
            .await
            .unwrap()
            .unwrap()
            .workspace_id
            .as_deref(),
        Some("ws-older")
    );
    assert_eq!(fx.workspaces.list().await.len(), 2);
}

#[tokio::test]
async fn migrated_agents_keep_their_owner_when_a_same_cwd_workspace_appears_later() {
    let fx = fx();
    let cwd = fx.dir("non-git-project");
    fx.projects.initialize().await;
    fx.workspaces.initialize().await;
    fx.projects
        .upsert(ProjectRecord::new(
            &cwd,
            &cwd,
            ProjectKind::NonGit,
            "p",
            T1,
            T1,
        ))
        .await
        .unwrap();
    fx.workspaces
        .upsert(ws("ws-original-owner", &cwd, T1, None), None)
        .await
        .unwrap();
    fx.agents
        .upsert(agent("legacy-agent", &cwd, T1, T1, None))
        .await
        .unwrap();
    fx.run(&FakeHost::new()).await.unwrap();
    fx.workspaces
        .upsert(
            ws("ws-created-later", &cwd, "2026-03-04T00:00:00.000Z", None),
            None,
        )
        .await
        .unwrap();
    fx.run(&FakeHost::new()).await.unwrap();
    assert_eq!(
        fx.agents
            .get("legacy-agent")
            .await
            .unwrap()
            .unwrap()
            .workspace_id
            .as_deref(),
        Some("ws-original-owner")
    );
    assert!(fx.workspaces.get("ws-created-later").await.is_some());
}

#[tokio::test]
async fn preserves_existing_workspace_ids_when_only_projects_file_is_missing() {
    let fx = fx();
    let cwd = fx.dir("non-git-project");
    fx.workspaces.initialize().await;
    fx.workspaces
        .upsert(ws("ws-existing", &cwd, T1, None), None)
        .await
        .unwrap();
    fx.agents
        .upsert(agent("agent-1", &cwd, T2, T2, None))
        .await
        .unwrap();
    fx.run(&FakeHost::new()).await.unwrap();
    let workspaces = fx.workspaces.list().await;
    assert_eq!(workspaces.len(), 1);
    assert_eq!(
        (
            workspaces[0].workspace_id.as_str(),
            workspaces[0].cwd.as_str()
        ),
        ("ws-existing", cwd.as_str())
    );
    let projects = fx.projects.list().await;
    assert_eq!(projects.len(), 1);
    assert_eq!(projects[0].project_id, cwd);
}

#[tokio::test]
async fn pins_branch_identity_of_active_paseo_worktrees_and_survives_pin_failures() {
    let fx = fx();
    fx.projects.initialize().await;
    fx.workspaces.initialize().await;
    let mut owned = ws("ws-owned", "/wt/a", T1, None);
    owned.is_paseo_owned_worktree = true;
    owned.worktree_root = Some("/wt/a".into());
    owned.branch = Some("feat/a".into());
    let mut archived = owned.clone();
    archived.workspace_id = "ws-archived".into();
    archived.archived_at = Some(T2.into());
    let mut no_branch = owned.clone();
    no_branch.workspace_id = "ws-no-branch".into();
    no_branch.branch = None;
    let mut not_owned = owned.clone();
    not_owned.workspace_id = "ws-not-owned".into();
    not_owned.is_paseo_owned_worktree = false;
    for w in [owned, archived, no_branch, not_owned] {
        fx.workspaces.upsert(w, None).await.unwrap();
    }
    let mut host = FakeHost::new();
    host.pin_fails = true;
    fx.run(&host).await.unwrap();
    assert_eq!(
        *host.pins.lock().unwrap(),
        vec![("/wt/a".to_string(), "feat/a".to_string())]
    );
}

// ---- backfill -------------------------------------------------------------------------

async fn backfill_fx() -> (tempfile::TempDir, AgentStore, WorkspaceRegistry) {
    let tmp = tempfile::tempdir().unwrap();
    let agents = AgentStore::new(tmp.path().join("agents"));
    agents.initialize().await.unwrap();
    let workspaces = WorkspaceRegistry::new(tmp.path().join("workspaces.json"));
    workspaces.initialize().await;
    (tmp, agents, workspaces)
}

fn legacy(id: &str, cwd: &str, archived_at: Option<&str>) -> StoredAgentRecord {
    agent(
        id,
        cwd,
        "2026-03-01T12:00:00.000Z",
        "2026-03-01T12:00:00.000Z",
        archived_at,
    )
}

async fn run_backfill(agents: &AgentStore, workspaces: &WorkspaceRegistry, home: &str) -> usize {
    backfill_workspace_id_for_legacy_agents(agents, workspaces, Some(home))
        .await
        .unwrap()
}

#[tokio::test]
async fn backfill_stamps_the_oldest_exact_cwd_workspace() {
    let (_t, agents, workspaces) = backfill_fx().await;
    workspaces
        .upsert(ws("ws-newer", "/tmp/repo", T2, None), None)
        .await
        .unwrap();
    workspaces
        .upsert(ws("ws-older", "/tmp/repo", T1, None), None)
        .await
        .unwrap();
    agents
        .upsert(legacy("legacy-agent", "/tmp/repo", None))
        .await
        .unwrap();
    assert_eq!(run_backfill(&agents, &workspaces, "/home/u").await, 1);
    assert_eq!(
        agents
            .get("legacy-agent")
            .await
            .unwrap()
            .unwrap()
            .workspace_id
            .as_deref(),
        Some("ws-older")
    );
}

#[tokio::test]
async fn backfill_attributes_to_the_deepest_enclosing_workspace() {
    let (_t, agents, workspaces) = backfill_fx().await;
    workspaces
        .upsert(ws("ws-root", "/tmp/repo", T1, None), None)
        .await
        .unwrap();
    workspaces
        .upsert(ws("ws-app", "/tmp/repo/packages/app", T1, None), None)
        .await
        .unwrap();
    agents
        .upsert(legacy("legacy-agent", "/tmp/repo/packages/app/src", None))
        .await
        .unwrap();
    // A sibling prefix must not match: /tmp/repo-other is not inside /tmp/repo.
    agents
        .upsert(legacy("sibling", "/tmp/repo-other/x", None))
        .await
        .unwrap();
    run_backfill(&agents, &workspaces, "/home/u").await;
    assert_eq!(
        agents
            .get("legacy-agent")
            .await
            .unwrap()
            .unwrap()
            .workspace_id
            .as_deref(),
        Some("ws-app")
    );
    assert!(agents
        .get("sibling")
        .await
        .unwrap()
        .unwrap()
        .workspace_id
        .is_none());
}

#[tokio::test]
async fn backfill_leaves_stamped_records_untouched() {
    let (_t, agents, workspaces) = backfill_fx().await;
    workspaces
        .upsert(ws("ws-cwd", "/tmp/repo", T1, None), None)
        .await
        .unwrap();
    let mut stamped = legacy("stamped-agent", "/tmp/repo", None);
    stamped.workspace_id = Some("ws-explicit".into());
    agents.upsert(stamped).await.unwrap();
    assert_eq!(run_backfill(&agents, &workspaces, "/home/u").await, 0);
    assert_eq!(
        agents
            .get("stamped-agent")
            .await
            .unwrap()
            .unwrap()
            .workspace_id
            .as_deref(),
        Some("ws-explicit")
    );
}

#[tokio::test]
async fn backfill_stamps_archived_agents_from_archived_owners_only() {
    let (_t, agents, workspaces) = backfill_fx().await;
    workspaces
        .upsert(ws("ws-archived", "/tmp/repo", T1, Some(T2)), None)
        .await
        .unwrap();
    agents
        .upsert(legacy(
            "archived-agent",
            "/tmp/repo",
            Some("2026-03-02T12:00:00.000Z"),
        ))
        .await
        .unwrap();
    agents
        .upsert(legacy("active-agent", "/tmp/repo", None))
        .await
        .unwrap();
    assert_eq!(run_backfill(&agents, &workspaces, "/home/u").await, 1);
    assert_eq!(
        agents
            .get("archived-agent")
            .await
            .unwrap()
            .unwrap()
            .workspace_id
            .as_deref(),
        Some("ws-archived")
    );
    assert!(agents
        .get("active-agent")
        .await
        .unwrap()
        .unwrap()
        .workspace_id
        .is_none());
}

#[tokio::test]
async fn backfill_never_lets_the_home_directory_own_descendants() {
    let (_t, agents, workspaces) = backfill_fx().await;
    workspaces
        .upsert(ws("ws-home", "/home/u", T1, None), None)
        .await
        .unwrap();
    agents
        .upsert(legacy("descendant", "/home/u/repo", None))
        .await
        .unwrap();
    agents
        .upsert(legacy("exact", "/home/u", None))
        .await
        .unwrap();
    assert_eq!(run_backfill(&agents, &workspaces, "/home/u").await, 1);
    assert!(agents
        .get("descendant")
        .await
        .unwrap()
        .unwrap()
        .workspace_id
        .is_none());
    assert_eq!(
        agents
            .get("exact")
            .await
            .unwrap()
            .unwrap()
            .workspace_id
            .as_deref(),
        Some("ws-home")
    );
}
