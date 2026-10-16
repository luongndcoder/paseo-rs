//! Events the manager fans out to subscribers (`AgentManagerEvent`) and the subscription handle.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use paseo_agent_types::AgentStreamEvent;
use tokio::sync::mpsc;

use crate::manager::bus::BusTx;
use crate::model::ManagedAgentView;
use crate::provider_subagents::SubagentStoreEvent;

/// `AgentManagerEvent`. `internal` is the emitting agent's internal flag at dispatch time, so
/// the bus can filter without reading actor state.
#[derive(Debug, Clone, PartialEq)]
pub enum ManagerEvent {
    AgentState {
        agent: Arc<ManagedAgentView>,
    },
    ProviderSubagent {
        event: SubagentStoreEvent,
        parent_internal: bool,
    },
    TimelineReplacement {
        agent_id: String,
        epoch: String,
    },
    AgentStream {
        agent_id: String,
        event: AgentStreamEvent,
        seq: Option<i64>,
        epoch: Option<String>,
        timestamp: Option<String>,
        internal: bool,
    },
}

impl ManagerEvent {
    pub fn agent_id(&self) -> &str {
        match self {
            Self::AgentState { agent } => &agent.id,
            Self::ProviderSubagent { event, .. } => event.parent_agent_id(),
            Self::TimelineReplacement { agent_id, .. } | Self::AgentStream { agent_id, .. } => {
                agent_id
            }
        }
    }

    pub(crate) fn belongs_to_internal_agent(&self) -> bool {
        match self {
            Self::AgentState { agent } => agent.internal,
            Self::AgentStream { internal, .. } => *internal,
            Self::ProviderSubagent {
                parent_internal, ..
            } => *parent_internal,
            Self::TimelineReplacement { .. } => false,
        }
    }
}

pub type SharedEvent = Arc<ManagerEvent>;

/// `SubscribeOptions`.
#[derive(Debug, Clone)]
pub struct SubscribeOpts {
    pub agent_id: Option<String>,
    /// Global subscribers skip internal agents unless this is set.
    pub include_internal: bool,
    /// Replay the latest `agent_state` of matching live agents before live events.
    pub replay_state: bool,
    /// Per-subscriber queue bound; overflow closes the subscription (see
    /// [`Subscription::overflowed`]) instead of dropping events.
    pub capacity: usize,
}

pub const DEFAULT_SUBSCRIBER_CAPACITY: usize = 4096;

impl Default for SubscribeOpts {
    fn default() -> Self {
        Self {
            agent_id: None,
            include_internal: false,
            replay_state: true,
            capacity: DEFAULT_SUBSCRIBER_CAPACITY,
        }
    }
}

impl SubscribeOpts {
    pub fn for_agent(agent_id: impl Into<String>) -> Self {
        Self {
            agent_id: Some(agent_id.into()),
            ..Self::default()
        }
    }

    pub fn without_replay(mut self) -> Self {
        self.replay_state = false;
        self
    }

    pub fn including_internal(mut self) -> Self {
        self.include_internal = true;
        self
    }
}

/// A live subscription. Dropping it unsubscribes.
#[derive(Debug)]
pub struct Subscription {
    pub(crate) rx: mpsc::Receiver<SharedEvent>,
    pub(crate) id: u64,
    pub(crate) overflowed: Arc<AtomicBool>,
    pub(crate) bus: BusTx,
}

impl Subscription {
    pub async fn next(&mut self) -> Option<SharedEvent> {
        self.rx.recv().await
    }

    pub fn try_next(&mut self) -> Option<SharedEvent> {
        self.rx.try_recv().ok()
    }

    /// The bus closed this subscription because its queue was full. The consumer must resync
    /// (resubscribe with replay); events are never silently dropped.
    pub fn overflowed(&self) -> bool {
        self.overflowed.load(Ordering::SeqCst)
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.bus.unsubscribe(self.id);
    }
}
