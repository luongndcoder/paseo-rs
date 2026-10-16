//! In-memory scripted agent for flows a recorded fixture cannot express (random ids,
//! process exit, host requests).
#![allow(dead_code)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use paseo_acp::error::Result;
use paseo_acp::launch::LaunchSpec;
use paseo_acp::transport::{NoProcess, Transport, TransportFactory, TransportHandle, WireEvent};
use paseo_acp::util::BoxFuture;
use serde_json::{json, Value};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

pub struct AgentIo {
    pub from_client: UnboundedReceiver<Value>,
    pub to_client: UnboundedSender<WireEvent>,
}

impl AgentIo {
    pub async fn recv(&mut self) -> Value {
        tokio::time::timeout(Duration::from_secs(10), self.from_client.recv())
            .await
            .expect("agent timed out waiting for the client")
            .expect("client closed")
    }

    pub fn send(&self, v: Value) {
        let _ = self.to_client.send(WireEvent::Frame(v));
    }

    pub fn reply(&self, request: &Value, result: Value) {
        self.send(json!({"jsonrpc": "2.0", "id": request["id"], "result": result}));
    }

    pub fn reply_err(&self, request: &Value, code: i64, message: &str, data: Value) {
        self.send(json!({"jsonrpc": "2.0", "id": request["id"], "error": {"code": code, "message": message, "data": data}}));
    }

    pub fn notify(&self, method: &str, params: Value) {
        self.send(json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    pub fn request(&self, id: i64, method: &str, params: Value) {
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
    }

    /// The agent process ends: the client sees EOF.
    pub fn exit(&self) {
        let _ = self.to_client.send(WireEvent::Closed);
    }

    /// Receive frames until one with `method` arrives.
    pub async fn expect(&mut self, method: &str) -> Value {
        let f = self.recv().await;
        assert_eq!(f["method"], method, "unexpected frame {f}");
        f
    }

    /// Answer `initialize` and `session/new` like a minimal agent.
    pub async fn handshake(&mut self, session_id: &str, new_session_result: Value) {
        let init = self.expect("initialize").await;
        self.reply(
            &init,
            json!({"protocolVersion": 1, "agentCapabilities": {}, "authMethods": []}),
        );
        let new = self.expect("session/new").await;
        let mut result = new_session_result;
        result["sessionId"] = json!(session_id);
        self.reply(&new, result);
    }
}

type Script = Box<dyn FnOnce(AgentIo) -> BoxFuture<'static, ()> + Send>;

pub struct MemoryTransport(Script);

impl Transport for MemoryTransport {
    fn start(self: Box<Self>) -> Result<TransportHandle> {
        let (out_tx, out_rx) = unbounded_channel();
        let (in_tx, in_rx) = unbounded_channel();
        let fut = (self.0)(AgentIo {
            from_client: out_rx,
            to_client: in_tx,
        });
        tokio::spawn(fut);
        Ok(TransportHandle {
            outgoing: out_tx,
            incoming: in_rx,
            process: Arc::new(NoProcess),
        })
    }
}

pub struct MemoryFactory(Mutex<Option<Script>>);

impl MemoryFactory {
    pub fn new<F, Fut>(script: F) -> Arc<Self>
    where
        F: FnOnce(AgentIo) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        Arc::new(MemoryFactory(Mutex::new(Some(Box::new(move |io| {
            Box::pin(script(io))
        })))))
    }
}

impl TransportFactory for MemoryFactory {
    fn open(&self, _spec: &LaunchSpec) -> Result<Box<dyn Transport>> {
        let script = self
            .0
            .lock()
            .unwrap()
            .take()
            .expect("MemoryFactory opens one process");
        Ok(Box::new(MemoryTransport(script)))
    }

    fn requires_binary(&self) -> bool {
        false
    }
}
