//! Subscription manager for checkout diffs (`checkout-diff-manager.ts`):
//! shared watch targets, debounced refreshes, coalescing and change detection.
//! The workspace git service (snapshots, working-tree watches, forced diff
//! reads) is injected through [`CheckoutDiffWorkspace`]. Must run inside a Tokio
//! runtime: refreshes and the debounce timer are spawned tasks.

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Value};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::diff::{CheckoutDiffCompare, CheckoutDiffResult, DiffMode};
use crate::diff_manager::{
    build_diff_snapshot_payload, build_target_key, normalize_compare, CheckoutDiffSnapshotPayload,
    EmitGate, RefreshCoalescer, WorkspaceSnapshotWatch, CHECKOUT_DIFF_WATCH_DEBOUNCE_MS,
};
use crate::error::{GitError, Result};
use crate::paths::expand_tilde;
use crate::repo::AheadBehind;
use crate::shortstat::CheckoutShortstat;

pub type Unsubscribe = Box<dyn FnOnce() + Send>;
pub type ChangeListener = Arc<dyn Fn() + Send + Sync>;
pub type SnapshotListener = Arc<dyn Fn(WorkspaceSnapshot) + Send + Sync>;
pub type DiffListener = Arc<dyn Fn(CheckoutDiffSnapshotPayload) + Send + Sync>;

/// The git section of a workspace snapshot, as far as diff refreshes care.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkspaceGitSnapshot {
    pub is_git: bool,
    pub repo_root: Option<String>,
    pub main_repo_root: Option<String>,
    pub current_branch: Option<String>,
    pub remote_url: Option<String>,
    pub is_paseo_owned_worktree: bool,
    pub is_dirty: Option<bool>,
    pub base_ref: Option<String>,
    pub ahead_behind: Option<AheadBehind>,
    pub ahead_of_origin: Option<u64>,
    pub behind_of_origin: Option<u64>,
    pub has_remote: bool,
    pub diff_stat: Option<CheckoutShortstat>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WorkspaceSnapshot {
    pub cwd: String,
    pub git: WorkspaceGitSnapshot,
    /// Forge section, treated as an opaque JSON value.
    pub forge: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForceRead {
    pub reason: String,
}

pub struct WorkingTreeWatch {
    pub repo_root: Option<String>,
    pub unsubscribe: Unsubscribe,
}

pub trait CheckoutDiffWorkspace: Send + Sync + 'static {
    fn get_checkout_diff(
        &self,
        cwd: &str,
        compare: &CheckoutDiffCompare,
        force: Option<ForceRead>,
    ) -> impl Future<Output = Result<Arc<CheckoutDiffResult>>> + Send;
    fn get_snapshot(
        &self,
        cwd: &str,
        include_forge: bool,
    ) -> impl Future<Output = Result<WorkspaceSnapshot>> + Send;
    fn peek_snapshot(&self, cwd: &str) -> Option<WorkspaceSnapshot>;
    fn register_workspace(&self, cwd: &str, listener: SnapshotListener) -> Unsubscribe;
    fn request_working_tree_watch(
        &self,
        cwd: &str,
        on_change: ChangeListener,
    ) -> impl Future<Output = Result<WorkingTreeWatch>> + Send;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckoutDiffMetrics {
    pub checkout_diff_target_count: usize,
    pub checkout_diff_subscription_count: usize,
    pub checkout_diff_watcher_count: usize,
    pub checkout_diff_fallback_refresh_target_count: usize,
}

/// Cancels a pending or active subscription (`AbortSignal`).
#[derive(Clone, Default)]
pub struct AbortSignal {
    inner: Arc<AbortInner>,
}

#[derive(Default)]
struct AbortInner {
    aborted: AtomicBool,
    callbacks: Mutex<Vec<Box<dyn FnOnce() + Send>>>,
}

impl AbortSignal {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn abort(&self) {
        if self.inner.aborted.swap(true, Ordering::SeqCst) {
            return;
        }
        let callbacks: Vec<_> = self.inner.callbacks.lock().unwrap().drain(..).collect();
        for cb in callbacks {
            cb();
        }
    }

    pub fn is_aborted(&self) -> bool {
        self.inner.aborted.load(Ordering::SeqCst)
    }

    fn on_abort(&self, cb: Box<dyn FnOnce() + Send>) {
        let mut cbs = self.inner.callbacks.lock().unwrap();
        if self.is_aborted() {
            drop(cbs);
            cb();
        } else {
            cbs.push(cb);
        }
    }
}

type OpenOutcome = Option<std::result::Result<(), String>>;

struct TargetState {
    diff_cwd: String,
    listeners: HashMap<u64, DiffListener>,
    working_tree_unsubscribe: Option<Unsubscribe>,
    workspace_unsubscribe: Option<Unsubscribe>,
    snapshot_watch: WorkspaceSnapshotWatch,
    debounce: Option<JoinHandle<()>>,
    pending_debounce_force: bool,
    coalescer: RefreshCoalescer,
    latest_payload: Option<CheckoutDiffSnapshotPayload>,
    gate: EmitGate,
    open: Option<watch::Receiver<OpenOutcome>>,
}

struct Target {
    id: u64,
    key: String,
    cwd: String,
    compare: CheckoutDiffCompare,
    state: Mutex<TargetState>,
}

struct Shared<W> {
    workspace: Arc<W>,
    targets: Mutex<HashMap<String, Arc<Target>>>,
    next_id: AtomicU64,
}

pub struct CheckoutDiffManager<W: CheckoutDiffWorkspace> {
    shared: Arc<Shared<W>>,
}

pub struct CheckoutDiffSubscription {
    pub initial: CheckoutDiffSnapshotPayload,
    unsubscribe: Arc<dyn Fn() + Send + Sync>,
}

impl CheckoutDiffSubscription {
    pub fn unsubscribe(&self) {
        (self.unsubscribe)();
    }
}

fn snapshot_fingerprints(s: &WorkspaceSnapshot) -> (String, String, String) {
    let g = &s.git;
    let ab = |v: &Option<AheadBehind>| {
        v.map_or(
            Value::Null,
            |a| json!({"ahead": a.ahead, "behind": a.behind}),
        )
    };
    let structure = json!({
        "isGit": g.is_git, "repoRoot": g.repo_root, "mainRepoRoot": g.main_repo_root,
        "currentBranch": g.current_branch, "remoteUrl": g.remote_url,
        "isPaseoOwnedWorktree": g.is_paseo_owned_worktree, "baseRef": g.base_ref,
        "aheadBehind": ab(&g.ahead_behind), "aheadOfOrigin": g.ahead_of_origin,
        "behindOfOrigin": g.behind_of_origin, "hasRemote": g.has_remote,
    });
    let worktree = json!({
        "isDirty": g.is_dirty,
        "diffStat": g.diff_stat.map_or(Value::Null, |d| json!({"additions": d.additions, "deletions": d.deletions})),
    });
    (
        structure.to_string(),
        worktree.to_string(),
        s.forge.to_string(),
    )
}

impl<W: CheckoutDiffWorkspace> CheckoutDiffManager<W> {
    pub fn new(workspace: Arc<W>) -> Self {
        Self {
            shared: Arc::new(Shared {
                workspace,
                targets: Mutex::new(HashMap::new()),
                next_id: AtomicU64::new(1),
            }),
        }
    }

    /// One-shot read without subscribing.
    pub async fn read(
        &self,
        cwd: &str,
        compare: &CheckoutDiffCompare,
    ) -> CheckoutDiffSnapshotPayload {
        compute_snapshot(&self.shared, cwd, &normalize_compare(compare), None, None).await
    }

    pub async fn subscribe(
        &self,
        cwd: &str,
        compare: &CheckoutDiffCompare,
        listener: DiffListener,
        signal: Option<AbortSignal>,
    ) -> Result<CheckoutDiffSubscription> {
        let compare = normalize_compare(compare);
        let (target, listener_id, open_rx) = {
            let target = ensure_target(&self.shared, cwd, &compare);
            let mut st = target.state.lock().unwrap();
            let listener_id = self.shared.next_id.fetch_add(1, Ordering::SeqCst);
            st.listeners.insert(listener_id, listener);
            if st.open.is_none() {
                let (tx, rx) = watch::channel(None);
                st.open = Some(rx);
                let (shared, t) = (self.shared.clone(), target.clone());
                tokio::spawn(async move {
                    let out = open_target(&shared, &t).await.map_err(|e| e.to_string());
                    let _ = tx.send(Some(out));
                });
            }
            let rx = st.open.clone().expect("open set above");
            drop(st);
            (target, listener_id, rx)
        };

        let subscribed = Arc::new(AtomicBool::new(true));
        let unsubscribe: Arc<dyn Fn() + Send + Sync> = {
            let (shared, target, subscribed) =
                (self.shared.clone(), target.clone(), subscribed.clone());
            Arc::new(move || {
                if !subscribed.swap(false, Ordering::SeqCst) {
                    return;
                }
                remove_listener(&shared, &target, listener_id);
            })
        };
        if let Some(signal) = &signal {
            let u = unsubscribe.clone();
            signal.on_abort(Box::new(move || u()));
        }

        let mut rx = open_rx;
        let opened = match rx.wait_for(|v| v.is_some()).await {
            Ok(v) => v.clone().expect("checked"),
            Err(_) => Err("diff target open was cancelled".to_string()),
        };
        if let Err(message) = opened {
            unsubscribe();
            return Err(GitError::other(message));
        }
        let (existing, diff_cwd) = {
            let st = target.state.lock().unwrap();
            (st.latest_payload.clone(), st.diff_cwd.clone())
        };
        let initial = match existing {
            Some(p) => p,
            None => {
                compute_snapshot(
                    &self.shared,
                    &target.cwd,
                    &target.compare,
                    Some(&diff_cwd),
                    None,
                )
                .await
            }
        };
        {
            let mut st = target.state.lock().unwrap();
            st.latest_payload = Some(initial.clone());
            st.gate.remember_initial(&initial);
        }
        Ok(CheckoutDiffSubscription {
            initial,
            unsubscribe,
        })
    }

    /// Debounced, forced refresh of every target whose cwd matches.
    pub fn schedule_refresh_for_cwd(&self, cwd: &str) {
        let resolved = expand_tilde(cwd);
        let targets: Vec<Arc<Target>> = self
            .shared
            .targets
            .lock()
            .unwrap()
            .values()
            .cloned()
            .collect();
        for t in targets {
            let diff_cwd = t.state.lock().unwrap().diff_cwd.clone();
            if t.cwd == resolved || diff_cwd == resolved {
                schedule_target_refresh(&self.shared, &t, true);
            }
        }
    }

    pub fn get_metrics(&self) -> CheckoutDiffMetrics {
        let targets = self.shared.targets.lock().unwrap();
        let subscriptions = targets
            .values()
            .map(|t| t.state.lock().unwrap().listeners.len())
            .sum();
        CheckoutDiffMetrics {
            checkout_diff_target_count: targets.len(),
            checkout_diff_subscription_count: subscriptions,
            checkout_diff_watcher_count: 0,
            checkout_diff_fallback_refresh_target_count: 0,
        }
    }

    pub fn dispose(&self) {
        let targets: Vec<Arc<Target>> = self
            .shared
            .targets
            .lock()
            .unwrap()
            .drain()
            .map(|(_, t)| t)
            .collect();
        for t in targets {
            close_target(&t);
        }
    }
}

fn ensure_target<W: CheckoutDiffWorkspace>(
    shared: &Arc<Shared<W>>,
    cwd: &str,
    compare: &CheckoutDiffCompare,
) -> Arc<Target> {
    let key = build_target_key(cwd, compare);
    let mut targets = shared.targets.lock().unwrap();
    if let Some(t) = targets.get(&key) {
        return t.clone();
    }
    let target = Arc::new(Target {
        id: shared.next_id.fetch_add(1, Ordering::SeqCst),
        key: key.clone(),
        cwd: cwd.to_string(),
        compare: compare.clone(),
        state: Mutex::new(TargetState {
            diff_cwd: cwd.to_string(),
            listeners: HashMap::new(),
            working_tree_unsubscribe: None,
            workspace_unsubscribe: None,
            snapshot_watch: WorkspaceSnapshotWatch::default(),
            debounce: None,
            pending_debounce_force: false,
            coalescer: RefreshCoalescer::default(),
            latest_payload: None,
            gate: EmitGate::default(),
            open: None,
        }),
    });
    targets.insert(key, target.clone());
    target
}

fn is_registered<W: CheckoutDiffWorkspace>(shared: &Shared<W>, target: &Arc<Target>) -> bool {
    shared
        .targets
        .lock()
        .unwrap()
        .get(&target.key)
        .is_some_and(|t| t.id == target.id)
        && !target.state.lock().unwrap().listeners.is_empty()
}

fn close_target(target: &Target) {
    let (debounce, wt, ws) = {
        let mut st = target.state.lock().unwrap();
        st.listeners.clear();
        (
            st.debounce.take(),
            st.working_tree_unsubscribe.take(),
            st.workspace_unsubscribe.take(),
        )
    };
    if let Some(d) = debounce {
        d.abort();
    }
    if let Some(u) = wt {
        u();
    }
    if let Some(u) = ws {
        u();
    }
}

fn remove_listener<W: CheckoutDiffWorkspace>(
    shared: &Arc<Shared<W>>,
    target: &Arc<Target>,
    listener_id: u64,
) {
    let empty = {
        let mut st = target.state.lock().unwrap();
        st.listeners.remove(&listener_id);
        st.listeners.is_empty()
    };
    if !empty {
        return;
    }
    close_target(target);
    let mut targets = shared.targets.lock().unwrap();
    if targets.get(&target.key).is_some_and(|t| t.id == target.id) {
        targets.remove(&target.key);
    }
}

async fn compute_snapshot<W: CheckoutDiffWorkspace>(
    shared: &Arc<Shared<W>>,
    cwd: &str,
    compare: &CheckoutDiffCompare,
    diff_cwd: Option<&str>,
    force: Option<ForceRead>,
) -> CheckoutDiffSnapshotPayload {
    let diff_cwd = diff_cwd.unwrap_or(cwd);
    let request = CheckoutDiffCompare {
        mode: compare.mode,
        base_ref: compare.base_ref.clone(),
        ignore_whitespace: compare.ignore_whitespace,
        include_structured: Some(true),
    };
    let result = shared
        .workspace
        .get_checkout_diff(diff_cwd, &request, force)
        .await;
    build_diff_snapshot_payload(cwd, result)
}

async fn open_target<W: CheckoutDiffWorkspace>(
    shared: &Arc<Shared<W>>,
    target: &Arc<Target>,
) -> Result<()> {
    if target.compare.mode == DiffMode::Base {
        let snapshot = match shared.workspace.peek_snapshot(&target.cwd) {
            Some(s) => s,
            None => shared.workspace.get_snapshot(&target.cwd, false).await?,
        };
        target.state.lock().unwrap().diff_cwd = snapshot
            .git
            .repo_root
            .clone()
            .unwrap_or_else(|| target.cwd.clone());
        if !is_registered(shared, target) {
            return Ok(());
        }
        remember_workspace_snapshot(shared, target, &snapshot);
        let (s, t) = (shared.clone(), target.clone());
        let unsubscribe = shared.workspace.register_workspace(
            &target.cwd,
            Arc::new(move |next| remember_workspace_snapshot(&s, &t, &next)),
        );
        if !is_registered(shared, target) {
            unsubscribe();
            return Ok(());
        }
        target.state.lock().unwrap().workspace_unsubscribe = Some(unsubscribe);
        return Ok(());
    }
    let (s, t) = (shared.clone(), target.clone());
    let watch = shared
        .workspace
        .request_working_tree_watch(
            &target.cwd,
            Arc::new(move || schedule_target_refresh(&s, &t, true)),
        )
        .await?;
    target.state.lock().unwrap().diff_cwd = watch
        .repo_root
        .clone()
        .unwrap_or_else(|| target.cwd.clone());
    if !is_registered(shared, target) {
        (watch.unsubscribe)();
        return Ok(());
    }
    target.state.lock().unwrap().working_tree_unsubscribe = Some(watch.unsubscribe);
    Ok(())
}

fn remember_workspace_snapshot<W: CheckoutDiffWorkspace>(
    shared: &Arc<Shared<W>>,
    target: &Arc<Target>,
    snapshot: &WorkspaceSnapshot,
) {
    let (structure, worktree, forge) = snapshot_fingerprints(snapshot);
    let refresh = target
        .state
        .lock()
        .unwrap()
        .snapshot_watch
        .remember(structure, worktree, forge);
    if refresh {
        schedule_target_refresh(shared, target, false);
    }
}

fn schedule_target_refresh<W: CheckoutDiffWorkspace>(
    shared: &Arc<Shared<W>>,
    target: &Arc<Target>,
    force: bool,
) {
    let mut st = target.state.lock().unwrap();
    st.pending_debounce_force |= force;
    if let Some(previous) = st.debounce.take() {
        previous.abort();
    }
    let (shared, t) = (shared.clone(), target.clone());
    st.debounce = Some(tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(CHECKOUT_DIFF_WATCH_DEBOUNCE_MS)).await;
        let pending_force = {
            let mut st = t.state.lock().unwrap();
            st.debounce = None;
            std::mem::take(&mut st.pending_debounce_force)
        };
        refresh_target(&shared, &t, pending_force).await;
    }));
}

async fn refresh_target<W: CheckoutDiffWorkspace>(
    shared: &Arc<Shared<W>>,
    target: &Arc<Target>,
    force: bool,
) {
    let Some(mut current_force) = target.state.lock().unwrap().coalescer.request(force) else {
        return;
    };
    loop {
        let diff_cwd = target.state.lock().unwrap().diff_cwd.clone();
        let force_read = current_force.then(|| ForceRead {
            reason: "working-tree-watch".to_string(),
        });
        let snapshot = compute_snapshot(
            shared,
            &target.cwd,
            &target.compare,
            Some(&diff_cwd),
            force_read,
        )
        .await;
        let listeners: Vec<DiffListener> = {
            let mut st = target.state.lock().unwrap();
            st.latest_payload = Some(snapshot.clone());
            if st.gate.should_emit(&snapshot) {
                st.listeners.values().cloned().collect()
            } else {
                vec![]
            }
        };
        for l in listeners {
            l(snapshot.clone());
        }
        match target.state.lock().unwrap().coalescer.finish() {
            Some(next_force) => current_force = next_force,
            None => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::atomic::AtomicUsize;
    use tokio::sync::oneshot;

    use crate::diff_parse::{DiffFileStatus, ParsedDiffFile};

    fn file(path: &str, additions: u64, deletions: u64) -> ParsedDiffFile {
        ParsedDiffFile {
            path: path.into(),
            old_path: None,
            is_new: false,
            is_deleted: false,
            additions,
            deletions,
            staged: None,
            unstaged: None,
            hunks: vec![],
            status: Some(DiffFileStatus::Ok),
        }
    }

    fn diff_of(files: Vec<ParsedDiffFile>) -> CheckoutDiffResult {
        CheckoutDiffResult {
            diff: String::new(),
            structured: Some(files),
            diff_too_large: false,
        }
    }

    fn snapshot(f: impl FnOnce(&mut WorkspaceGitSnapshot)) -> WorkspaceSnapshot {
        let mut git = WorkspaceGitSnapshot {
            is_git: true,
            repo_root: Some("/tmp/repo".into()),
            main_repo_root: None,
            current_branch: Some("feature".into()),
            remote_url: Some("https://github.com/acme/repo.git".into()),
            is_paseo_owned_worktree: false,
            is_dirty: Some(false),
            base_ref: Some("main".into()),
            ahead_behind: Some(AheadBehind {
                ahead: 1,
                behind: 0,
            }),
            ahead_of_origin: Some(1),
            behind_of_origin: Some(0),
            has_remote: true,
            diff_stat: Some(CheckoutShortstat {
                additions: 1,
                deletions: 0,
            }),
        };
        f(&mut git);
        WorkspaceSnapshot {
            cwd: "/tmp/repo".into(),
            git,
            forge: json!({"featuresEnabled": false, "authState": "no_remote", "pullRequest": null, "error": null}),
        }
    }

    enum Step {
        Ready(CheckoutDiffResult),
        Pending(oneshot::Receiver<CheckoutDiffResult>),
    }

    struct Mock {
        diff_calls: Mutex<Vec<(String, CheckoutDiffCompare, Option<ForceRead>)>>,
        script: Mutex<VecDeque<Step>>,
        fallback: CheckoutDiffResult,
        watch_calls: Mutex<Vec<String>>,
        on_change: Mutex<Option<ChangeListener>>,
        ws_listener: Mutex<Option<SnapshotListener>>,
        repo_root: Option<String>,
        watch_unsubscribes: Arc<AtomicUsize>,
        ws_unsubscribes: Arc<AtomicUsize>,
        register_calls: AtomicUsize,
        watch_gate: Mutex<Option<oneshot::Receiver<()>>>,
    }

    impl Mock {
        fn new(repo_root: Option<&str>, script: Vec<Step>) -> Arc<Self> {
            Arc::new(Mock {
                diff_calls: Mutex::new(vec![]),
                script: Mutex::new(script.into()),
                fallback: diff_of(vec![]),
                watch_calls: Mutex::new(vec![]),
                on_change: Mutex::new(None),
                ws_listener: Mutex::new(None),
                repo_root: repo_root.map(str::to_string),
                watch_unsubscribes: Arc::new(AtomicUsize::new(0)),
                ws_unsubscribes: Arc::new(AtomicUsize::new(0)),
                register_calls: AtomicUsize::new(0),
                watch_gate: Mutex::new(None),
            })
        }
        fn diff_count(&self) -> usize {
            self.diff_calls.lock().unwrap().len()
        }
        fn fire_change(&self) {
            let l = self
                .on_change
                .lock()
                .unwrap()
                .clone()
                .expect("watch registered");
            l();
        }
        fn fire_snapshot(&self, s: WorkspaceSnapshot) {
            let l = self
                .ws_listener
                .lock()
                .unwrap()
                .clone()
                .expect("workspace registered");
            l(s);
        }
    }

    impl CheckoutDiffWorkspace for Mock {
        async fn get_checkout_diff(
            &self,
            cwd: &str,
            compare: &CheckoutDiffCompare,
            force: Option<ForceRead>,
        ) -> Result<Arc<CheckoutDiffResult>> {
            self.diff_calls
                .lock()
                .unwrap()
                .push((cwd.to_string(), compare.clone(), force));
            let step = self.script.lock().unwrap().pop_front();
            Ok(Arc::new(match step {
                Some(Step::Ready(r)) => r,
                Some(Step::Pending(rx)) => rx.await.unwrap(),
                None => self.fallback.clone(),
            }))
        }
        async fn get_snapshot(
            &self,
            _cwd: &str,
            _include_forge: bool,
        ) -> Result<WorkspaceSnapshot> {
            Ok(snapshot(|_| {}))
        }
        fn peek_snapshot(&self, _cwd: &str) -> Option<WorkspaceSnapshot> {
            None
        }
        fn register_workspace(&self, _cwd: &str, listener: SnapshotListener) -> Unsubscribe {
            self.register_calls.fetch_add(1, Ordering::SeqCst);
            *self.ws_listener.lock().unwrap() = Some(listener);
            let c = self.ws_unsubscribes.clone();
            Box::new(move || {
                c.fetch_add(1, Ordering::SeqCst);
            })
        }
        async fn request_working_tree_watch(
            &self,
            cwd: &str,
            on_change: ChangeListener,
        ) -> Result<WorkingTreeWatch> {
            self.watch_calls.lock().unwrap().push(cwd.to_string());
            *self.on_change.lock().unwrap() = Some(on_change);
            let gate = self.watch_gate.lock().unwrap().take();
            if let Some(rx) = gate {
                let _ = rx.await;
            }
            let c = self.watch_unsubscribes.clone();
            Ok(WorkingTreeWatch {
                repo_root: self.repo_root.clone(),
                unsubscribe: Box::new(move || {
                    c.fetch_add(1, Ordering::SeqCst);
                }),
            })
        }
    }

    fn collector() -> (DiffListener, Arc<Mutex<Vec<CheckoutDiffSnapshotPayload>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s = seen.clone();
        (Arc::new(move |p| s.lock().unwrap().push(p)), seen)
    }

    fn noop() -> DiffListener {
        Arc::new(|_| {})
    }

    async fn advance(ms: u64) {
        // Let freshly spawned timers register before the paused clock jumps.
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        tokio::time::advance(Duration::from_millis(ms)).await;
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
    }

    fn uncommitted() -> CheckoutDiffCompare {
        CheckoutDiffCompare::uncommitted()
    }

    fn base_main() -> CheckoutDiffCompare {
        CheckoutDiffCompare::base(Some("main"))
    }

    #[tokio::test(start_paused = true)]
    async fn subscribe_requests_a_working_tree_watch_with_the_correct_cwd() {
        let mock = Mock::new(Some("/tmp/repo"), vec![]);
        let manager = CheckoutDiffManager::new(mock.clone());
        manager
            .subscribe("/tmp/repo/packages/server", &uncommitted(), noop(), None)
            .await
            .unwrap();
        assert_eq!(
            *mock.watch_calls.lock().unwrap(),
            ["/tmp/repo/packages/server"]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn unsubscribe_calls_the_working_tree_watch_unsubscribe() {
        let mock = Mock::new(Some("/tmp/repo"), vec![]);
        let manager = CheckoutDiffManager::new(mock.clone());
        let sub = manager
            .subscribe("/tmp/repo/packages/server", &uncommitted(), noop(), None)
            .await
            .unwrap();
        sub.unsubscribe();
        sub.unsubscribe();
        assert_eq!(mock.watch_unsubscribes.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn cancels_a_subscription_while_its_working_tree_watch_is_still_opening() {
        let mock = Mock::new(Some("/tmp/repo"), vec![]);
        let (gate_tx, gate_rx) = oneshot::channel();
        *mock.watch_gate.lock().unwrap() = Some(gate_rx);
        let manager = Arc::new(CheckoutDiffManager::new(mock.clone()));
        let signal = AbortSignal::new();
        let m = manager.clone();
        let s = signal.clone();
        let pending = tokio::spawn(async move {
            m.subscribe("/tmp/repo/packages/server", &uncommitted(), noop(), Some(s))
                .await
        });
        tokio::task::yield_now().await;
        signal.abort();
        gate_tx.send(()).unwrap();
        pending.await.unwrap().unwrap();
        assert_eq!(mock.watch_unsubscribes.load(Ordering::SeqCst), 1);
        assert_eq!(
            manager.get_metrics(),
            CheckoutDiffMetrics {
                checkout_diff_target_count: 0,
                checkout_diff_subscription_count: 0,
                checkout_diff_watcher_count: 0,
                checkout_diff_fallback_refresh_target_count: 0
            }
        );
    }

    #[tokio::test(start_paused = true)]
    async fn shares_one_opening_target_between_concurrent_subscriptions() {
        let mock = Mock::new(Some("/tmp/repo"), vec![]);
        let (gate_tx, gate_rx) = oneshot::channel();
        *mock.watch_gate.lock().unwrap() = Some(gate_rx);
        let manager = Arc::new(CheckoutDiffManager::new(mock.clone()));
        let spawn_sub = || {
            let m = manager.clone();
            tokio::spawn(async move {
                m.subscribe("/tmp/repo/packages/server", &uncommitted(), noop(), None)
                    .await
                    .unwrap()
            })
        };
        let (a, b) = (spawn_sub(), spawn_sub());
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        assert_eq!(mock.watch_calls.lock().unwrap().len(), 1);
        gate_tx.send(()).unwrap();
        let (first, second) = (a.await.unwrap(), b.await.unwrap());
        assert_eq!(manager.get_metrics().checkout_diff_subscription_count, 2);
        first.unsubscribe();
        assert_eq!(mock.watch_unsubscribes.load(Ordering::SeqCst), 0);
        second.unsubscribe();
        assert_eq!(mock.watch_unsubscribes.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn diff_cwd_uses_repo_root_from_the_working_tree_watch_result() {
        let mock = Mock::new(Some("/tmp/repo"), vec![]);
        let manager = CheckoutDiffManager::new(mock.clone());
        manager
            .subscribe("/tmp/repo/packages/server", &uncommitted(), noop(), None)
            .await
            .unwrap();
        let calls = mock.diff_calls.lock().unwrap();
        assert_eq!(calls[0].0, "/tmp/repo");
        assert_eq!(
            (calls[0].1.mode, calls[0].1.include_structured),
            (DiffMode::Uncommitted, Some(true))
        );
        assert_eq!(calls[0].2, None);
    }

    #[tokio::test(start_paused = true)]
    async fn falls_back_to_cwd_when_the_working_tree_watch_returns_no_repo_root() {
        let mock = Mock::new(None, vec![]);
        let manager = CheckoutDiffManager::new(mock.clone());
        manager
            .subscribe("/tmp/plain", &uncommitted(), noop(), None)
            .await
            .unwrap();
        assert_eq!(mock.diff_calls.lock().unwrap()[0].0, "/tmp/plain");
    }

    #[tokio::test(start_paused = true)]
    async fn diff_refresh_is_triggered_when_the_working_tree_watch_callback_fires_and_forces_a_cache_bypass(
    ) {
        let mock = Mock::new(
            Some("/tmp/repo"),
            vec![
                Step::Ready(diff_of(vec![file("a.ts", 1, 0)])),
                Step::Ready(diff_of(vec![file("b.ts", 2, 0)])),
            ],
        );
        let manager = CheckoutDiffManager::new(mock.clone());
        let (listener, seen) = collector();
        let sub = manager
            .subscribe("/tmp/repo/packages/server", &uncommitted(), listener, None)
            .await
            .unwrap();
        assert_eq!(sub.initial.files[0].path, "a.ts");
        mock.fire_change();
        advance(150).await;
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(
            serde_json::to_value(&seen[0]).unwrap(),
            json!({"cwd": "/tmp/repo/packages/server", "files": [serde_json::to_value(file("b.ts", 2, 0)).unwrap()], "error": null})
        );
        let calls = mock.diff_calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].2, None);
        assert!(calls[1]
            .2
            .as_ref()
            .is_some_and(|f| f.reason.contains("working-tree")));
    }

    #[tokio::test(start_paused = true)]
    async fn an_edit_during_an_in_flight_diff_refresh_produces_one_final_follow_up() {
        let (tx, rx) = oneshot::channel();
        let mock = Mock::new(
            Some("/tmp/repo"),
            vec![
                Step::Ready(diff_of(vec![file("tracked.ts", 1, 0)])),
                Step::Pending(rx),
                Step::Ready(diff_of(vec![file("tracked.ts", 100, 25)])),
            ],
        );
        let manager = CheckoutDiffManager::new(mock.clone());
        let (listener, seen) = collector();
        manager
            .subscribe("/tmp/repo", &uncommitted(), listener, None)
            .await
            .unwrap();
        mock.fire_change();
        advance(150).await;
        assert_eq!(mock.diff_count(), 2);
        for _ in 0..100 {
            mock.fire_change();
        }
        advance(150).await;
        assert_eq!(
            mock.diff_count(),
            2,
            "the in-flight refresh blocks new reads"
        );
        tx.send(diff_of(vec![file("tracked.ts", 2, 1)])).unwrap();
        advance(0).await;
        assert_eq!(mock.diff_count(), 3);
        let seen = seen.lock().unwrap();
        let last = seen.last().unwrap();
        assert_eq!(
            (last.files[0].additions, last.files[0].deletions),
            (100, 25)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn base_diff_subscriptions_ignore_ordinary_working_tree_edits() {
        let mock = Mock::new(
            Some("/tmp/repo"),
            vec![Step::Ready(diff_of(vec![file("committed.ts", 1, 0)]))],
        );
        let manager = CheckoutDiffManager::new(mock.clone());
        manager
            .subscribe("/tmp/repo", &base_main(), noop(), None)
            .await
            .unwrap();
        assert!(
            mock.on_change.lock().unwrap().is_none(),
            "no working tree watch for base compares"
        );
        advance(150).await;
        assert_eq!(mock.diff_count(), 1);
        assert!(mock.watch_calls.lock().unwrap().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn base_diff_subscriptions_ignore_worktree_only_workspace_snapshot_updates() {
        let mock = Mock::new(Some("/tmp/repo"), vec![]);
        let manager = CheckoutDiffManager::new(mock.clone());
        manager
            .subscribe("/tmp/repo", &base_main(), noop(), None)
            .await
            .unwrap();
        mock.fire_snapshot(snapshot(|g| {
            g.is_dirty = Some(true);
            g.diff_stat = Some(CheckoutShortstat {
                additions: 5,
                deletions: 2,
            });
        }));
        advance(150).await;
        assert_eq!(mock.diff_count(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn base_diff_subscriptions_refresh_for_structural_workspace_changes() {
        let mock = Mock::new(Some("/tmp/repo"), vec![]);
        let manager = CheckoutDiffManager::new(mock.clone());
        manager
            .subscribe("/tmp/repo", &base_main(), noop(), None)
            .await
            .unwrap();
        assert_eq!(mock.register_calls.load(Ordering::SeqCst), 1);
        mock.fire_snapshot(snapshot(|g| {
            g.current_branch = Some("feature-2".into());
            g.is_dirty = Some(true);
            g.diff_stat = Some(CheckoutShortstat {
                additions: 5,
                deletions: 2,
            });
        }));
        advance(150).await;
        assert_eq!(mock.diff_count(), 2);
        assert_eq!(
            mock.diff_calls.lock().unwrap()[1].2,
            None,
            "structural refreshes are not forced"
        );
        mock.fire_snapshot(snapshot(|g| {
            g.ahead_behind = Some(AheadBehind {
                ahead: 2,
                behind: 0,
            })
        }));
        advance(150).await;
        assert_eq!(mock.diff_count(), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn schedule_refresh_for_cwd_forces_matching_targets_only() {
        let mock = Mock::new(Some("/tmp/repo"), vec![]);
        let manager = CheckoutDiffManager::new(mock.clone());
        manager
            .subscribe("/tmp/repo/pkg", &uncommitted(), noop(), None)
            .await
            .unwrap();
        manager.schedule_refresh_for_cwd("/tmp/elsewhere");
        advance(150).await;
        assert_eq!(mock.diff_count(), 1);
        manager.schedule_refresh_for_cwd("/tmp/repo");
        advance(150).await;
        assert_eq!(mock.diff_count(), 2);
        assert!(mock.diff_calls.lock().unwrap()[1].2.is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn unchanged_refresh_results_are_not_re_emitted_and_dispose_releases_watches() {
        let mock = Mock::new(Some("/tmp/repo"), vec![]);
        let manager = CheckoutDiffManager::new(mock.clone());
        let (listener, seen) = collector();
        manager
            .subscribe("/tmp/repo", &uncommitted(), listener, None)
            .await
            .unwrap();
        mock.fire_change();
        advance(150).await;
        assert_eq!(mock.diff_count(), 2);
        assert!(
            seen.lock().unwrap().is_empty(),
            "identical payloads are suppressed"
        );
        manager.dispose();
        assert_eq!(mock.watch_unsubscribes.load(Ordering::SeqCst), 1);
        assert_eq!(manager.get_metrics().checkout_diff_target_count, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn read_returns_a_payload_without_subscribing() {
        let mock = Mock::new(
            None,
            vec![Step::Ready(diff_of(vec![
                file("b.ts", 1, 1),
                file("a.ts", 1, 1),
            ]))],
        );
        let manager = CheckoutDiffManager::new(mock.clone());
        let payload = manager
            .read(
                "/tmp/repo",
                &CheckoutDiffCompare {
                    ignore_whitespace: Some(true),
                    ..uncommitted()
                },
            )
            .await;
        assert_eq!(
            payload
                .files
                .iter()
                .map(|f| f.path.as_str())
                .collect::<Vec<_>>(),
            ["a.ts", "b.ts"]
        );
        assert_eq!(
            mock.diff_calls.lock().unwrap()[0].1.ignore_whitespace,
            Some(true)
        );
        assert!(mock.watch_calls.lock().unwrap().is_empty());
    }
}
