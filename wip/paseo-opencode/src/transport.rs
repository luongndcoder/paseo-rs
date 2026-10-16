//! The seam between the OpenCode client and the network.
//!
//! `Transport` carries JSON requests and one SSE stream. `HttpTransport` (reqwest) is the real
//! implementation; `replay::ReplayTransport` serves recorded traffic in tests.

use crate::error::Result;
use async_trait::async_trait;
use futures_util::Stream;
use serde_json::Value;
use std::pin::Pin;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

impl Method {
    pub fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
            Method::Put => "PUT",
            Method::Patch => "PATCH",
            Method::Delete => "DELETE",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct HttpRequest {
    pub method: Method,
    /// Path including the query string, e.g. `/session?directory=%2Ftmp`.
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Value>,
}

impl HttpRequest {
    /// Path without the query string.
    pub fn path_only(&self) -> &str {
        self.path.split('?').next().unwrap_or(&self.path)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct HttpResponse {
    pub status: u16,
    /// Parsed JSON; `Null` for an empty body, `String` for a body that is not JSON.
    pub body: Value,
}

/// One server-sent-events frame.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SseFrame {
    pub event: Option<String>,
    pub id: Option<String>,
    pub data: String,
}

pub type SseStream = Pin<Box<dyn Stream<Item = Result<SseFrame>> + Send>>;

#[async_trait]
pub trait Transport: Send + Sync + 'static {
    /// Performs a request. Any HTTP status is returned as `Ok`; the caller maps errors.
    async fn send(&self, req: HttpRequest) -> Result<HttpResponse>;
    /// Opens an SSE stream (a `GET` that returns `text/event-stream`).
    async fn open_sse(&self, req: HttpRequest) -> Result<SseStream>;
}
