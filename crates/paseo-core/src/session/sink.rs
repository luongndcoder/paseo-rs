//! Per-session output multiplexer: one [`Delivery`](crate::delivery::Delivery) serves every
//! socket of a logical session, so its sink routes each [`Outbound`] to the socket it names.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::delivery::{Outbound, OutboundSink, SourceId};

#[derive(Default)]
pub struct SessionSink {
    outs: Mutex<HashMap<SourceId, Arc<dyn OutboundSink>>>,
}

impl SessionSink {
    pub fn add(&self, source: SourceId, out: Arc<dyn OutboundSink>) {
        self.outs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(source, out);
    }

    pub fn remove(&self, source: SourceId) {
        self.outs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&source);
    }
}

impl OutboundSink for SessionSink {
    fn send(&self, out: Outbound) {
        let target = self
            .outs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&out.to)
            .cloned();
        if let Some(target) = target {
            target.send(out);
        }
    }
}
