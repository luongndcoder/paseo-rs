//! Per-cwd git subscription ownership and live-update fan-out, ported from
//! `session/workspace-git-observer/workspace-git-observer-service.ts`.
//!
//! Filesystem subscriptions are keyed by cwd while descriptor and branch state
//! stay keyed by workspace id, so same-directory workspace records share one
//! watch without sharing identity or teardown lifetime. Branch changes reach
//! `on_branch_changed` from two paths sharing `last_branch`: the snapshot
//! listener and `record_descriptor_state`.
//!
//! No callback or unsubscribe runs while the internal lock is held, so
//! callbacks may re-enter the service.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex, Weak};

use serde::{Deserialize, Serialize};

use crate::paths::resolve_path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceKind {
    Directory,
    LocalCheckout,
    Worktree,
}

/// The slice of `WorkspaceDescriptorPayload` the observer reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceDescriptor {
    pub id: String,
    pub workspace_directory: String,
    pub workspace_kind: WorkspaceKind,
    /// `gitRuntime.currentBranch`: `None` = absent (undefined),
    /// `Some(None)` = explicit null, `Some(Some(b))` = branch `b`.
    pub current_branch: Option<Option<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitSnapshot {
    pub cwd: String,
    pub current_branch: Option<String>,
}

pub type GitListener = Arc<dyn Fn(&GitSnapshot) + Send + Sync>;
pub type Unsubscribe = Box<dyn FnOnce() + Send>;

/// `WorkspaceGitService.registerWorkspace`.
pub trait WorkspaceGitService: Send + Sync {
    fn register_workspace(&self, cwd: &str, listener: GitListener) -> Result<Unsubscribe, String>;
}

pub trait ObserverLogger: Send + Sync {
    fn warn(&self, message: &str, cwd: &str, error: &str);
}

pub type EmitWorkspaceUpdate = Arc<dyn Fn(&str) -> Result<(), String> + Send + Sync>;
pub type EmitStatusUpdate = Arc<dyn Fn(&str, &GitSnapshot) + Send + Sync>;
/// `(workspace_id, old_branch, new_branch)`.
pub type OnBranchChanged = Arc<dyn Fn(&str, Option<&str>, Option<&str>) + Send + Sync>;

pub struct WorkspaceGitObserverDeps {
    pub workspace_git_service: Arc<dyn WorkspaceGitService>,
    pub emit_workspace_update_for_cwd: EmitWorkspaceUpdate,
    pub emit_status_update: EmitStatusUpdate,
    pub on_branch_changed: Option<OnBranchChanged>,
    pub logger: Arc<dyn ObserverLogger>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceGitObserverMetrics {
    pub watched_directory_count: usize,
    pub workspace_record_count: usize,
    pub subscription_count: usize,
}

struct WatchState {
    cwd: String,
    last_branch: Option<String>,
}

#[derive(Default)]
struct State {
    watch_targets: HashMap<String, HashSet<String>>,
    workspace_states: HashMap<String, WatchState>,
    subscriptions: HashMap<String, Unsubscribe>,
}

struct Inner {
    deps: WorkspaceGitObserverDeps,
    state: Mutex<State>,
}

type BranchChange = (String, Option<String>, Option<String>);

#[derive(Clone)]
pub struct WorkspaceGitObserverService {
    inner: Arc<Inner>,
}

fn normalize(cwd: &str) -> String {
    resolve_path(Path::new(cwd)).to_string_lossy().into_owned()
}

impl Inner {
    fn notify(&self, changes: Vec<BranchChange>) {
        if let Some(callback) = &self.deps.on_branch_changed {
            for (id, old, new) in changes {
                callback(&id, old.as_deref(), new.as_deref());
            }
        }
    }

    fn remember_descriptor_state(
        state: &mut State,
        workspace_id: &str,
        workspace: Option<&WorkspaceDescriptor>,
    ) {
        let Some(entry) = state.workspace_states.get_mut(workspace_id) else {
            return;
        };
        if let Some(Some(branch)) = workspace.map(|w| w.current_branch.clone()) {
            entry.last_branch = branch;
        }
    }

    fn remove_for_cwd(state: &mut State, cwd: &str, out: &mut Vec<Unsubscribe>) {
        let normalized = normalize(cwd);
        if let Some(target) = state.watch_targets.remove(&normalized) {
            for id in target {
                state.workspace_states.remove(&id);
            }
        }
        if let Some(unsubscribe) = state.subscriptions.remove(&normalized) {
            out.push(unsubscribe);
        }
    }

    fn remove_for_workspace_id(state: &mut State, workspace_id: &str, out: &mut Vec<Unsubscribe>) {
        let Some(entry) = state.workspace_states.remove(workspace_id) else {
            return;
        };
        let emptied = match state.watch_targets.get_mut(&entry.cwd) {
            Some(target) => {
                target.remove(workspace_id);
                target.is_empty()
            }
            None => false,
        };
        if emptied {
            Self::remove_for_cwd(state, &entry.cwd, out);
        }
    }

    fn handle_branch_snapshot(&self, cwd: &str, branch: Option<&str>) {
        let changes = {
            let mut state = self.state.lock().unwrap();
            let State {
                watch_targets,
                workspace_states,
                ..
            } = &mut *state;
            let Some(target) = watch_targets.get(&normalize(cwd)) else {
                return;
            };
            let mut changes = Vec::new();
            for id in target {
                let Some(entry) = workspace_states.get_mut(id) else {
                    continue;
                };
                if entry.last_branch.as_deref() == branch {
                    continue;
                }
                let previous = entry.last_branch.take();
                entry.last_branch = branch.map(str::to_string);
                changes.push((id.clone(), previous, branch.map(str::to_string)));
            }
            changes
        };
        self.notify(changes);
    }
}

fn run_all(unsubscribes: Vec<Unsubscribe>) {
    for unsubscribe in unsubscribes {
        unsubscribe();
    }
}

impl WorkspaceGitObserverService {
    pub fn new(deps: WorkspaceGitObserverDeps) -> Self {
        Self {
            inner: Arc::new(Inner {
                deps,
                state: Mutex::new(State::default()),
            }),
        }
    }

    fn sync_observer(&self, cwd: &str, is_git: bool, workspace_id: &str) -> Result<(), String> {
        let normalized = normalize(cwd);
        let mut stale = Vec::new();
        {
            let mut state = self.inner.state.lock().unwrap();
            if state
                .workspace_states
                .get(workspace_id)
                .is_some_and(|s| s.cwd != normalized)
            {
                Inner::remove_for_workspace_id(&mut state, workspace_id, &mut stale);
            }
            if !is_git {
                Inner::remove_for_workspace_id(&mut state, workspace_id, &mut stale);
                drop(state);
                run_all(stale);
                return Ok(());
            }
            state
                .watch_targets
                .entry(normalized.clone())
                .or_default()
                .insert(workspace_id.to_string());
            state
                .workspace_states
                .entry(workspace_id.to_string())
                .or_insert_with(|| WatchState {
                    cwd: normalized.clone(),
                    last_branch: None,
                });
            if state.subscriptions.contains_key(&normalized) {
                drop(state);
                run_all(stale);
                return Ok(());
            }
        }
        run_all(stale);

        let weak: Weak<Inner> = Arc::downgrade(&self.inner);
        let listener_cwd = normalized.clone();
        let listener: GitListener = Arc::new(move |snapshot| {
            let Some(inner) = weak.upgrade() else { return };
            inner.handle_branch_snapshot(&listener_cwd, snapshot.current_branch.as_deref());
            if let Err(error) = (inner.deps.emit_workspace_update_for_cwd)(&listener_cwd) {
                inner.deps.logger.warn(
                    "Failed to emit workspace update after git branch snapshot",
                    &listener_cwd,
                    &error,
                );
            }
            (inner.deps.emit_status_update)(&listener_cwd, snapshot);
        });
        match self
            .inner
            .deps
            .workspace_git_service
            .register_workspace(&normalized, listener)
        {
            Ok(unsubscribe) => {
                let mut state = self.inner.state.lock().unwrap();
                match state.subscriptions.entry(normalized) {
                    std::collections::hash_map::Entry::Vacant(slot) => {
                        slot.insert(unsubscribe);
                    }
                    std::collections::hash_map::Entry::Occupied(_) => {
                        drop(state);
                        unsubscribe();
                    }
                }
                Ok(())
            }
            Err(error) => {
                let mut stale = Vec::new();
                {
                    let mut state = self.inner.state.lock().unwrap();
                    Inner::remove_for_workspace_id(&mut state, workspace_id, &mut stale);
                }
                run_all(stale);
                Err(error)
            }
        }
    }

    pub fn sync_observers(&self, workspaces: &[WorkspaceDescriptor]) -> Result<(), String> {
        for workspace in workspaces {
            self.sync_observer(
                &workspace.workspace_directory,
                workspace.workspace_kind != WorkspaceKind::Directory,
                &workspace.id,
            )?;
            let mut state = self.inner.state.lock().unwrap();
            Inner::remember_descriptor_state(&mut state, &workspace.id, Some(workspace));
        }
        Ok(())
    }

    /// Drops observers for workspaces not in `workspaces`, then syncs the rest.
    pub fn reconcile_observers(&self, workspaces: &[WorkspaceDescriptor]) -> Result<(), String> {
        let retained: HashSet<&str> = workspaces.iter().map(|w| w.id.as_str()).collect();
        let mut stale = Vec::new();
        {
            let mut state = self.inner.state.lock().unwrap();
            let gone: Vec<String> = state
                .workspace_states
                .keys()
                .filter(|id| !retained.contains(id.as_str()))
                .cloned()
                .collect();
            for id in gone {
                Inner::remove_for_workspace_id(&mut state, &id, &mut stale);
            }
        }
        run_all(stale);
        for workspace in workspaces {
            self.sync_observer(
                &workspace.workspace_directory,
                workspace.workspace_kind != WorkspaceKind::Directory,
                &workspace.id,
            )?;
        }
        Ok(())
    }

    pub fn record_descriptor_state(
        &self,
        workspace_id: &str,
        next_workspace: Option<&WorkspaceDescriptor>,
    ) {
        let change = {
            let mut state = self.inner.state.lock().unwrap();
            let change = match (
                state.workspace_states.get(workspace_id),
                next_workspace.and_then(|w| w.current_branch.as_ref()),
            ) {
                (Some(entry), Some(new_branch)) if *new_branch != entry.last_branch => Some((
                    workspace_id.to_string(),
                    entry.last_branch.clone(),
                    new_branch.clone(),
                )),
                _ => None,
            };
            Inner::remember_descriptor_state(&mut state, workspace_id, next_workspace);
            change
        };
        if let Some(change) = change {
            self.inner.notify(vec![change]);
        }
    }

    pub fn handle_branch_snapshot(&self, cwd: &str, branch: Option<&str>) {
        self.inner.handle_branch_snapshot(cwd, branch);
    }

    pub fn get_metrics(&self) -> WorkspaceGitObserverMetrics {
        let state = self.inner.state.lock().unwrap();
        WorkspaceGitObserverMetrics {
            watched_directory_count: state.watch_targets.len(),
            workspace_record_count: state.workspace_states.len(),
            subscription_count: state.subscriptions.len(),
        }
    }

    pub fn remove_for_workspace_id(&self, workspace_id: &str) {
        let mut stale = Vec::new();
        {
            let mut state = self.inner.state.lock().unwrap();
            Inner::remove_for_workspace_id(&mut state, workspace_id, &mut stale);
        }
        run_all(stale);
    }

    pub fn dispose(&self) {
        let subscriptions: Vec<Unsubscribe> = {
            let mut state = self.inner.state.lock().unwrap();
            let subs = state.subscriptions.drain().map(|(_, u)| u).collect();
            state.watch_targets.clear();
            state.workspace_states.clear();
            subs
        };
        run_all(subscriptions);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws1() -> String {
        normalize("/repo/ws1")
    }
    fn ws2() -> String {
        normalize("/repo/ws2")
    }

    #[derive(Default)]
    struct Recorded {
        listeners: HashMap<String, GitListener>,
        register_calls: Vec<String>,
        unsubscribe_calls: Vec<String>,
        emit_cwd_calls: Vec<String>,
        status_calls: Vec<(String, Option<String>)>,
        branch_changes: Vec<(String, Option<String>, Option<String>)>,
        warn_calls: usize,
    }

    type Shared = Arc<Mutex<Recorded>>;

    struct FakeGit(Shared);
    impl WorkspaceGitService for FakeGit {
        fn register_workspace(
            &self,
            cwd: &str,
            listener: GitListener,
        ) -> Result<Unsubscribe, String> {
            let mut rec = self.0.lock().unwrap();
            rec.register_calls.push(cwd.to_string());
            rec.listeners.insert(cwd.to_string(), listener);
            let shared = self.0.clone();
            let cwd = cwd.to_string();
            Ok(Box::new(move || {
                let mut rec = shared.lock().unwrap();
                rec.unsubscribe_calls.push(cwd.clone());
                rec.listeners.remove(&cwd);
            }))
        }
    }

    struct FakeLogger(Shared);
    impl ObserverLogger for FakeLogger {
        fn warn(&self, _: &str, _: &str, _: &str) {
            self.0.lock().unwrap().warn_calls += 1;
        }
    }

    struct Harness {
        service: WorkspaceGitObserverService,
        rec: Shared,
    }

    impl Harness {
        fn new(emit_cwd_rejects: bool) -> Self {
            let rec: Shared = Arc::default();
            let service = WorkspaceGitObserverService::new(WorkspaceGitObserverDeps {
                workspace_git_service: Arc::new(FakeGit(rec.clone())),
                emit_workspace_update_for_cwd: {
                    let rec = rec.clone();
                    Arc::new(move |cwd| {
                        rec.lock().unwrap().emit_cwd_calls.push(cwd.to_string());
                        if emit_cwd_rejects {
                            Err("emit boom".into())
                        } else {
                            Ok(())
                        }
                    })
                },
                emit_status_update: {
                    let rec = rec.clone();
                    Arc::new(move |cwd, snapshot| {
                        rec.lock()
                            .unwrap()
                            .status_calls
                            .push((cwd.to_string(), snapshot.current_branch.clone()));
                    })
                },
                on_branch_changed: Some({
                    let rec = rec.clone();
                    Arc::new(move |id, old, new| {
                        rec.lock().unwrap().branch_changes.push((
                            id.to_string(),
                            old.map(str::to_string),
                            new.map(str::to_string),
                        ));
                    })
                }),
                logger: Arc::new(FakeLogger(rec.clone())),
            });
            Self { service, rec }
        }

        fn emit_snapshot(&self, cwd: &str, branch: Option<&str>) -> Result<(), String> {
            let listener = self
                .rec
                .lock()
                .unwrap()
                .listeners
                .get(cwd)
                .cloned()
                .ok_or_else(|| format!("no listener registered for {cwd}"))?;
            listener(&GitSnapshot {
                cwd: cwd.to_string(),
                current_branch: branch.map(str::to_string),
            });
            Ok(())
        }

        fn rec<R>(&self, f: impl FnOnce(&Recorded) -> R) -> R {
            f(&self.rec.lock().unwrap())
        }
    }

    fn descriptor(id: &str, dir: &str) -> WorkspaceDescriptor {
        WorkspaceDescriptor {
            id: id.into(),
            workspace_directory: dir.into(),
            workspace_kind: WorkspaceKind::LocalCheckout,
            current_branch: None,
        }
    }

    fn directory(id: &str, dir: &str) -> WorkspaceDescriptor {
        WorkspaceDescriptor {
            workspace_kind: WorkspaceKind::Directory,
            ..descriptor(id, dir)
        }
    }

    fn on_branch(id: &str, dir: &str, branch: &str) -> WorkspaceDescriptor {
        WorkspaceDescriptor {
            current_branch: Some(Some(branch.into())),
            ..descriptor(id, dir)
        }
    }

    fn change(
        id: &str,
        old: Option<&str>,
        new: Option<&str>,
    ) -> (String, Option<String>, Option<String>) {
        (id.into(), old.map(Into::into), new.map(Into::into))
    }

    #[test]
    fn registers_a_subscription_for_a_git_workspace() {
        let h = Harness::new(false);
        h.service
            .sync_observers(&[descriptor("ws1", &ws1())])
            .unwrap();
        assert_eq!(h.rec(|r| r.register_calls.clone()), vec![ws1()]);
    }

    #[test]
    fn does_not_register_a_non_git_workspace() {
        let h = Harness::new(false);
        h.service
            .sync_observers(&[directory("ws1", &ws1())])
            .unwrap();
        assert!(h.rec(|r| r.register_calls.is_empty()));
    }

    #[test]
    fn is_idempotent_for_resync_of_same_workspace() {
        let h = Harness::new(false);
        let d = descriptor("ws1", &ws1());
        h.service.sync_observers(std::slice::from_ref(&d)).unwrap();
        h.service.sync_observers(std::slice::from_ref(&d)).unwrap();
        assert_eq!(h.rec(|r| r.register_calls.len()), 1);
    }

    #[test]
    fn shares_one_cwd_subscription_across_workspace_identities() {
        let h = Harness::new(false);
        h.service
            .sync_observers(&[descriptor("ws1", &ws1()), descriptor("ws2", &ws1())])
            .unwrap();
        assert_eq!(h.rec(|r| r.register_calls.clone()), vec![ws1()]);
    }

    #[test]
    fn tears_down_when_a_git_workspace_becomes_non_git() {
        let h = Harness::new(false);
        h.service
            .sync_observers(&[descriptor("ws1", &ws1())])
            .unwrap();
        h.service
            .sync_observers(&[directory("ws1", &ws1())])
            .unwrap();
        assert_eq!(h.rec(|r| r.unsubscribe_calls.clone()), vec![ws1()]);
    }

    #[test]
    fn keeps_shared_subscription_when_one_sibling_becomes_non_git() {
        let h = Harness::new(false);
        h.service
            .sync_observers(&[descriptor("ws1", &ws1()), descriptor("ws2", &ws1())])
            .unwrap();
        h.service
            .sync_observers(&[directory("ws1", &ws1())])
            .unwrap();
        assert!(h.rec(|r| r.unsubscribe_calls.is_empty()));
        h.emit_snapshot(&ws1(), Some("feature")).unwrap();
        assert_eq!(
            h.rec(|r| r.branch_changes.clone()),
            vec![change("ws2", None, Some("feature"))]
        );
    }

    #[test]
    fn reports_observer_ownership_as_workspaces_are_added_and_removed() {
        let h = Harness::new(false);
        h.service
            .sync_observers(&[
                descriptor("ws1", &ws1()),
                descriptor("ws2", &ws1()),
                descriptor("ws3", &ws2()),
            ])
            .unwrap();
        let metrics = |a, b, c| WorkspaceGitObserverMetrics {
            watched_directory_count: a,
            workspace_record_count: b,
            subscription_count: c,
        };
        assert_eq!(h.service.get_metrics(), metrics(2, 3, 2));
        h.service.remove_for_workspace_id("ws1");
        assert_eq!(h.service.get_metrics(), metrics(2, 2, 2));
        h.service.dispose();
        assert_eq!(h.service.get_metrics(), metrics(0, 0, 0));
        // Wire shape matches the TS metrics object.
        assert_eq!(
            serde_json::to_value(h.service.get_metrics()).unwrap(),
            serde_json::json!({"watchedDirectoryCount":0,"workspaceRecordCount":0,"subscriptionCount":0})
        );
    }

    #[test]
    fn snapshot_fans_out_to_branch_change_workspace_update_and_status() {
        let h = Harness::new(false);
        h.service
            .sync_observers(&[descriptor("ws1", &ws1())])
            .unwrap();
        h.emit_snapshot(&ws1(), Some("feature")).unwrap();
        assert_eq!(
            h.rec(|r| r.branch_changes.clone()),
            vec![change("ws1", None, Some("feature"))]
        );
        assert_eq!(h.rec(|r| r.emit_cwd_calls.clone()), vec![ws1()]);
        assert_eq!(
            h.rec(|r| r.status_calls.clone()),
            vec![(ws1(), Some("feature".to_string()))]
        );
    }

    #[test]
    fn does_not_refire_branch_changed_when_unchanged() {
        let h = Harness::new(false);
        h.service
            .sync_observers(&[descriptor("ws1", &ws1())])
            .unwrap();
        h.emit_snapshot(&ws1(), Some("feature")).unwrap();
        h.emit_snapshot(&ws1(), Some("feature")).unwrap();
        assert_eq!(h.rec(|r| r.branch_changes.len()), 1);
    }

    #[test]
    fn logs_and_swallows_an_emit_failure_without_skipping_status_update() {
        let h = Harness::new(true);
        h.service
            .sync_observers(&[descriptor("ws1", &ws1())])
            .unwrap();
        h.emit_snapshot(&ws1(), Some("feature")).unwrap();
        assert_eq!(h.rec(|r| r.status_calls.len()), 1);
        assert_eq!(h.rec(|r| r.warn_calls), 1);
    }

    #[test]
    fn a_workspace_title_is_not_a_branch_change() {
        let h = Harness::new(false);
        let titled = on_branch("ws1", &ws1(), "main");
        h.service
            .sync_observers(std::slice::from_ref(&titled))
            .unwrap();
        h.emit_snapshot(&ws1(), Some("main")).unwrap();
        h.service.record_descriptor_state("ws1", Some(&titled));
        assert!(h.rec(|r| r.branch_changes.is_empty()));
    }

    #[test]
    fn fires_branch_changed_once_per_transition() {
        let h = Harness::new(false);
        h.service
            .sync_observers(&[descriptor("ws1", &ws1())])
            .unwrap();
        let feature = on_branch("ws1", &ws1(), "feature");
        h.service.record_descriptor_state("ws1", Some(&feature));
        h.service.record_descriptor_state("ws1", Some(&feature));
        assert_eq!(
            h.rec(|r| r.branch_changes.clone()),
            vec![change("ws1", None, Some("feature"))]
        );
    }

    #[test]
    fn record_descriptor_state_ignores_unknown_workspace() {
        let h = Harness::new(false);
        h.service
            .record_descriptor_state("unknown", Some(&on_branch("x", "/x", "main")));
        assert!(h.rec(|r| r.branch_changes.is_empty()));
    }

    #[test]
    fn remove_for_workspace_id_unsubscribes_and_ignores_unknown() {
        let h = Harness::new(false);
        h.service
            .sync_observers(&[descriptor("ws1", &ws1())])
            .unwrap();
        h.service.remove_for_workspace_id("nope");
        assert!(h.rec(|r| r.unsubscribe_calls.is_empty()));
        h.service.remove_for_workspace_id("ws1");
        assert_eq!(h.rec(|r| r.unsubscribe_calls.clone()), vec![ws1()]);
    }

    #[test]
    fn keeps_shared_cwd_subscription_until_last_workspace_removed() {
        let h = Harness::new(false);
        h.service
            .sync_observers(&[
                on_branch("ws1", &ws1(), "main"),
                on_branch("ws2", &ws1(), "main"),
            ])
            .unwrap();
        h.emit_snapshot(&ws1(), Some("feature")).unwrap();
        h.service.remove_for_workspace_id("ws1");
        assert!(h.rec(|r| r.unsubscribe_calls.is_empty()));
        h.emit_snapshot(&ws1(), Some("next")).unwrap();
        let mut got = h.rec(|r| r.branch_changes.clone());
        // Iteration order across sibling workspaces is unspecified.
        let tail = got.split_off(got.len() - 1);
        got.sort();
        assert_eq!(
            got,
            vec![
                change("ws1", Some("main"), Some("feature")),
                change("ws2", Some("main"), Some("feature")),
            ]
        );
        assert_eq!(tail, vec![change("ws2", Some("feature"), Some("next"))]);
        h.service.remove_for_workspace_id("ws2");
        assert_eq!(h.rec(|r| r.unsubscribe_calls.clone()), vec![ws1()]);
        assert!(h.emit_snapshot(&ws1(), Some("x")).is_err());
    }

    #[test]
    fn dispose_releases_every_subscription_and_clears_targets() {
        let h = Harness::new(false);
        h.service
            .sync_observers(&[descriptor("ws1", &ws1()), descriptor("ws2", &ws2())])
            .unwrap();
        h.service.dispose();
        let mut calls = h.rec(|r| r.unsubscribe_calls.clone());
        calls.sort();
        let mut expected = vec![ws1(), ws2()];
        expected.sort();
        assert_eq!(calls, expected);
        h.service
            .record_descriptor_state("ws1", Some(&on_branch("ws1", &ws1(), "main")));
        assert!(h.rec(|r| r.branch_changes.is_empty()));
    }

    #[test]
    fn reconcile_observers_drops_missing_workspaces_and_syncs_the_rest() {
        let h = Harness::new(false);
        h.service
            .sync_observers(&[descriptor("ws1", &ws1()), descriptor("ws2", &ws2())])
            .unwrap();
        h.service
            .reconcile_observers(&[descriptor("ws2", &ws2())])
            .unwrap();
        assert_eq!(h.rec(|r| r.unsubscribe_calls.clone()), vec![ws1()]);
        assert_eq!(h.service.get_metrics().workspace_record_count, 1);
    }
}
