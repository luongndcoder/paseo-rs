//! Typed access to the OpenCode REST API (the provider-specific raw protocol layer).

use crate::error::{Error, Result};
use crate::protocol::*;
use crate::transport::{HttpRequest, Method, SseStream, Transport};
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

/// Same set as JavaScript's `encodeURIComponent`.
const COMPONENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'!')
    .remove(b'~')
    .remove(b'*')
    .remove(b'\'')
    .remove(b'(')
    .remove(b')');

pub fn encode_component(s: &str) -> String {
    utf8_percent_encode(s, COMPONENT).to_string()
}

/// Thin wrapper over a `Transport` bound to a project `directory`.
///
/// Like the official SDK, every request carries the directory twice: as the `directory` query
/// parameter and as the `x-opencode-directory` header (both URI-encoded).
#[derive(Clone)]
pub struct RawClient {
    transport: Arc<dyn Transport>,
    directory: Option<String>,
}

fn json<T: Serialize>(v: &T) -> Result<Value> {
    Ok(serde_json::to_value(v)?)
}

fn decode<T: DeserializeOwned>(v: Value) -> Result<T> {
    Ok(serde_json::from_value(v)?)
}

impl RawClient {
    pub fn new(transport: Arc<dyn Transport>, directory: Option<String>) -> Self {
        Self {
            transport,
            directory,
        }
    }

    pub fn with_directory(&self, directory: impl Into<String>) -> Self {
        Self {
            transport: self.transport.clone(),
            directory: Some(directory.into()),
        }
    }

    pub fn directory(&self) -> Option<&str> {
        self.directory.as_deref()
    }

    pub fn transport(&self) -> &Arc<dyn Transport> {
        &self.transport
    }

    fn request(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<Value>,
    ) -> HttpRequest {
        let mut params: Vec<String> = Vec::new();
        let mut headers = Vec::new();
        if let Some(dir) = &self.directory {
            params.push(format!("directory={}", encode_component(dir)));
            headers.push(("x-opencode-directory".to_owned(), encode_component(dir)));
        }
        for (k, v) in query {
            params.push(format!("{k}={}", encode_component(v)));
        }
        let path = if params.is_empty() {
            path.to_owned()
        } else {
            format!("{path}?{}", params.join("&"))
        };
        HttpRequest {
            method,
            path,
            headers,
            body,
        }
    }

    async fn call(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<Value>,
    ) -> Result<Value> {
        let req = self.request(method, path, query, body);
        let full = req.path.clone();
        let resp = self.transport.send(req).await?;
        if resp.status >= 400 {
            let body = match &resp.body {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            return Err(Error::Http {
                status: resp.status,
                path: full,
                body,
            });
        }
        Ok(resp.body)
    }

    // ---- server ----

    pub async fn global_health(&self) -> Result<Value> {
        self.call(Method::Get, "/global/health", &[], None).await
    }

    /// `GET /global/event`. Project-less on purpose (no directory).
    pub async fn global_event(&self) -> Result<SseStream> {
        let req = HttpRequest {
            method: Method::Get,
            path: "/global/event".into(),
            headers: vec![],
            body: None,
        };
        self.transport.open_sse(req).await
    }

    // ---- sessions ----

    pub async fn session_create(&self, body: &SessionCreateBody) -> Result<Session> {
        let body = (*body != SessionCreateBody::default())
            .then(|| json(body))
            .transpose()?;
        decode(self.call(Method::Post, "/session", &[], body).await?)
    }

    pub async fn session_get(&self, id: &str) -> Result<Session> {
        decode(
            self.call(
                Method::Get,
                &format!("/session/{}", encode_component(id)),
                &[],
                None,
            )
            .await?,
        )
    }

    /// `GET /session`; `roots` limits to top-level sessions.
    pub async fn session_list(
        &self,
        roots: bool,
        limit: Option<u32>,
        search: Option<&str>,
    ) -> Result<Vec<Session>> {
        let mut q = Vec::new();
        if roots {
            q.push(("roots", "true".to_owned()));
        }
        if let Some(l) = limit {
            q.push(("limit", l.to_string()));
        }
        if let Some(s) = search {
            q.push(("search", s.to_owned()));
        }
        decode(self.call(Method::Get, "/session", &q, None).await?)
    }

    pub async fn session_children(&self, id: &str) -> Result<Vec<Session>> {
        decode(
            self.call(
                Method::Get,
                &format!("/session/{}/children", encode_component(id)),
                &[],
                None,
            )
            .await?,
        )
    }

    pub async fn session_update(&self, id: &str, body: &SessionUpdateBody) -> Result<Session> {
        decode(
            self.call(
                Method::Patch,
                &format!("/session/{}", encode_component(id)),
                &[],
                Some(json(body)?),
            )
            .await?,
        )
    }

    pub async fn session_delete(&self, id: &str) -> Result<bool> {
        let v = self
            .call(
                Method::Delete,
                &format!("/session/{}", encode_component(id)),
                &[],
                None,
            )
            .await?;
        Ok(v.as_bool().unwrap_or(true))
    }

    pub async fn session_messages(
        &self,
        id: &str,
        limit: Option<u32>,
    ) -> Result<Vec<MessageWithParts>> {
        let q: Vec<_> = limit
            .map(|l| ("limit", l.to_string()))
            .into_iter()
            .collect();
        decode(
            self.call(
                Method::Get,
                &format!("/session/{}/message", encode_component(id)),
                &q,
                None,
            )
            .await?,
        )
    }

    /// Same data as `session_messages`, uninterpreted (for provider-agnostic history replay).
    pub async fn session_messages_raw(&self, id: &str) -> Result<Vec<Value>> {
        decode(
            self.call(
                Method::Get,
                &format!("/session/{}/message", encode_component(id)),
                &[],
                None,
            )
            .await?,
        )
    }

    /// `GET /session/status`: busy/retry sessions only (idle sessions are absent).
    pub async fn session_status(&self) -> Result<HashMap<String, SessionStatusInfo>> {
        decode(self.call(Method::Get, "/session/status", &[], None).await?)
    }

    /// `POST /session/{id}/prompt_async`: returns once the prompt is accepted (204).
    pub async fn prompt_async(&self, id: &str, body: &PromptBody) -> Result<()> {
        self.call(
            Method::Post,
            &format!("/session/{}/prompt_async", encode_component(id)),
            &[],
            Some(json(body)?),
        )
        .await
        .map(|_| ())
    }

    pub async fn command(&self, id: &str, body: &CommandBody) -> Result<Value> {
        self.call(
            Method::Post,
            &format!("/session/{}/command", encode_component(id)),
            &[],
            Some(json(body)?),
        )
        .await
    }

    pub async fn summarize(&self, id: &str, model: Option<&ModelRef>) -> Result<Value> {
        let body = match model {
            Some(m) => json(m)?,
            None => Value::Object(Default::default()),
        };
        self.call(
            Method::Post,
            &format!("/session/{}/summarize", encode_component(id)),
            &[],
            Some(body),
        )
        .await
    }

    pub async fn abort(&self, id: &str) -> Result<bool> {
        let v = self
            .call(
                Method::Post,
                &format!("/session/{}/abort", encode_component(id)),
                &[],
                None,
            )
            .await?;
        Ok(v.as_bool().unwrap_or(true))
    }

    pub async fn revert(&self, id: &str, body: &RevertBody) -> Result<Session> {
        decode(
            self.call(
                Method::Post,
                &format!("/session/{}/revert", encode_component(id)),
                &[],
                Some(json(body)?),
            )
            .await?,
        )
    }

    pub async fn unrevert(&self, id: &str) -> Result<Session> {
        decode(
            self.call(
                Method::Post,
                &format!("/session/{}/unrevert", encode_component(id)),
                &[],
                None,
            )
            .await?,
        )
    }

    // ---- permissions and questions ----

    pub async fn permission_list(&self) -> Result<Vec<PermissionRequest>> {
        decode(self.call(Method::Get, "/permission", &[], None).await?)
    }

    pub async fn permission_reply(
        &self,
        request_id: &str,
        body: &PermissionReplyBody,
    ) -> Result<bool> {
        let v = self
            .call(
                Method::Post,
                &format!("/permission/{}/reply", encode_component(request_id)),
                &[],
                Some(json(body)?),
            )
            .await?;
        Ok(v.as_bool().unwrap_or(true))
    }

    pub async fn question_list(&self) -> Result<Vec<QuestionRequest>> {
        decode(self.call(Method::Get, "/question", &[], None).await?)
    }

    pub async fn question_reply(&self, request_id: &str, body: &QuestionReplyBody) -> Result<bool> {
        let v = self
            .call(
                Method::Post,
                &format!("/question/{}/reply", encode_component(request_id)),
                &[],
                Some(json(body)?),
            )
            .await?;
        Ok(v.as_bool().unwrap_or(true))
    }

    pub async fn question_reject(&self, request_id: &str) -> Result<bool> {
        let v = self
            .call(
                Method::Post,
                &format!("/question/{}/reject", encode_component(request_id)),
                &[],
                None,
            )
            .await?;
        Ok(v.as_bool().unwrap_or(true))
    }

    // ---- catalogue ----

    pub async fn provider_list(&self) -> Result<ProviderList> {
        decode(self.call(Method::Get, "/provider", &[], None).await?)
    }

    pub async fn agents(&self) -> Result<Vec<AgentInfo>> {
        decode(self.call(Method::Get, "/agent", &[], None).await?)
    }

    pub async fn commands(&self) -> Result<Vec<CommandInfo>> {
        decode(self.call(Method::Get, "/command", &[], None).await?)
    }

    // ---- MCP ----

    pub async fn mcp_add(&self, body: &McpAddBody) -> Result<Value> {
        self.call(Method::Post, "/mcp", &[], Some(json(body)?))
            .await
    }

    pub async fn mcp_status(&self) -> Result<Value> {
        self.call(Method::Get, "/mcp", &[], None).await
    }
}
