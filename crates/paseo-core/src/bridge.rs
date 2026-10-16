//! The seam between the core and a provider session.
//!
//! `ProviderBridge` is the object-safe session trait the actor drives. It is the provider API's
//! `AgentSession` (implemented by the host adapter, native adapters and `MockSession`); the core
//! never names a concrete provider.

use std::sync::Arc;

pub use paseo_provider_api::AgentSession as ProviderBridge;

/// A shared handle to a session. Blocking provider calls clone this into sub-tasks so the
/// actor loop never awaits them.
pub type SessionRef = Arc<dyn ProviderBridge>;

/// Poll a future once without a waker.
///
/// `AgentSession::describe_persistence` is async in the provider API but a synchronous read in
/// TS, and the actor needs it inside an atomic block. Adapters answer from cached state, so the
/// future is ready on the first poll; a provider that really suspends yields `None` and the
/// caller falls back to a sub-task.
pub(crate) fn sync_probe<T>(future: impl std::future::Future<Output = T>) -> Option<T> {
    use std::task::{Context, Poll, Waker};
    let mut future = std::pin::pin!(future);
    let mut cx = Context::from_waker(Waker::noop());
    match future.as_mut().poll(&mut cx) {
        Poll::Ready(value) => Some(value),
        Poll::Pending => None,
    }
}
