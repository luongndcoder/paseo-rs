use std::sync::Arc;

use tokio::sync::OnceCell;

use crate::error::{Result, SpeechError};

type Factory<E> = Arc<dyn Fn() -> Result<E> + Send + Sync>;

/// An engine built on first use, off the async threads (model loading takes seconds).
/// A failed build is not cached, so a later session can retry (e.g. after a download finished).
pub struct LazyEngine<E: Send + Sync + 'static> {
    cell: OnceCell<Arc<E>>,
    factory: Factory<E>,
}

impl<E: Send + Sync + 'static> LazyEngine<E> {
    pub fn new(factory: impl Fn() -> Result<E> + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self {
            cell: OnceCell::new(),
            factory: Arc::new(factory),
        })
    }

    pub fn ready(engine: E) -> Arc<Self> {
        let cell = OnceCell::new();
        let _ = cell.set(Arc::new(engine));
        Arc::new(Self {
            cell,
            factory: Arc::new(|| Err(SpeechError::msg("engine already initialised"))),
        })
    }

    pub async fn get(&self) -> Result<Arc<E>> {
        self.cell
            .get_or_try_init(|| async {
                let factory = self.factory.clone();
                let engine = tokio::task::spawn_blocking(move || factory())
                    .await
                    .map_err(|e| {
                        SpeechError::msg(format!("engine initialisation panicked: {e}"))
                    })??;
                Ok::<_, SpeechError>(Arc::new(engine))
            })
            .await
            .cloned()
    }

    pub fn is_initialised(&self) -> bool {
        self.cell.initialized()
    }
}
