//! `SkillsActor`: one thread owns the [`SkillsController`], every request is a message, and
//! even `status` runs recovery first. Ordering between concurrent callers is not promised;
//! consistency after both finish is (TS `serialize`).

use crate::controller::{
    ImportLegacyResult, ResolveTargets, SaveRequest, SkillsController, SkillsSaveResult,
    SkillsSnapshot,
};
use crate::error::{failed, Result, SkillsError};
use crate::operations::SkillTargets;
use crate::selection::{ConfigStore, SelectionStore, SkillSelection};
use serde_json::Value;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::mpsc;
use std::sync::Arc;
use tokio::sync::oneshot;

type Job = Box<dyn FnOnce(&SkillsController) + Send>;

/// Cloneable front door to the actor thread. The thread stops when the last handle drops.
#[derive(Clone)]
pub struct SkillsHandle {
    tx: mpsc::Sender<Job>,
}

pub struct SkillsActor;

impl SkillsActor {
    pub fn spawn(controller: SkillsController) -> SkillsHandle {
        let (tx, rx) = mpsc::channel::<Job>();
        std::thread::Builder::new()
            .name("paseo-skills".into())
            .spawn(move || {
                while let Ok(job) = rx.recv() {
                    job(&controller);
                }
            })
            .expect("spawn paseo-skills thread");
        SkillsHandle { tx }
    }
}

impl SkillsHandle {
    pub fn spawn(resolve_targets: ResolveTargets, config: Arc<dyn ConfigStore>) -> Self {
        SkillsActor::spawn(SkillsController::new(
            resolve_targets,
            SelectionStore::new(config),
        ))
    }

    /// Convenience for fixed targets.
    pub fn spawn_with_targets(targets: SkillTargets, config: Arc<dyn ConfigStore>) -> Self {
        Self::spawn(Arc::new(move || targets.clone()), config)
    }

    async fn call<T: Send + 'static>(
        &self,
        run: impl FnOnce(&SkillsController) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let (reply, rx) = oneshot::channel();
        let job: Job = Box::new(move |controller| {
            let outcome = catch_unwind(AssertUnwindSafe(|| run(controller)))
                .unwrap_or_else(|_| Err(failed("skills operation panicked")));
            let _ = reply.send(outcome);
        });
        self.tx.send(job).map_err(|_| SkillsError::ActorStopped)?;
        rx.await.map_err(|_| SkillsError::ActorStopped)?
    }

    pub async fn status(&self) -> Result<SkillsSnapshot> {
        self.call(|c| c.status()).await
    }

    /// Non-destructive install of the saved selection.
    pub async fn install(&self) -> Result<SkillsSnapshot> {
        self.call(|c| c.install()).await
    }

    pub async fn reconcile(&self) -> Result<SkillsSnapshot> {
        self.call(|c| c.update()).await
    }

    pub async fn uninstall(&self) -> Result<SkillsSnapshot> {
        self.call(|c| c.uninstall()).await
    }

    pub async fn auto_update(&self) -> Result<SkillsSnapshot> {
        self.call(|c| c.auto_update()).await
    }

    pub async fn save_selection(
        &self,
        selection: SkillSelection,
        confirmed_removals: Vec<String>,
    ) -> Result<SkillsSaveResult> {
        let request = SaveRequest::new(selection, &confirmed_removals);
        self.call(move |c| c.save(&request)).await
    }

    /// Raw-request variant: the selection and `confirmedRemovals` are coerced from JSON.
    pub async fn save_request(&self, request: Value) -> Result<SkillsSaveResult> {
        let request = SaveRequest::from_value(&request);
        self.call(move |c| c.save(&request)).await
    }

    pub async fn import_legacy_selection(&self, selection: Value) -> Result<ImportLegacyResult> {
        self.call(move |c| c.import_legacy_selection_if_unset(&selection))
            .await
    }
}
