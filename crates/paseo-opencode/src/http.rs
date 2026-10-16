//! reqwest-backed `Transport` for a local OpenCode server.

use crate::error::{Error, Result};
use crate::sse::SseParser;
use crate::transport::{HttpRequest, HttpResponse, Method, SseFrame, SseStream, Transport};
use async_trait::async_trait;
use futures_util::stream::{self, StreamExt};
use serde_json::Value;
use std::collections::VecDeque;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct HttpTransport {
    base_url: String,
    auth: Option<(String, String)>,
    client: reqwest::Client,
}

impl HttpTransport {
    /// `base_url` like `http://127.0.0.1:4096`; `auth` is the HTTP basic `(username, password)`.
    pub fn new(base_url: impl Into<String>, auth: Option<(String, String)>) -> Result<Self> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .no_proxy()
            .build()
            .map_err(|e| Error::Transport(e.to_string()))?;
        Ok(Self {
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            auth,
            client,
        })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    fn builder(&self, req: &HttpRequest) -> reqwest::RequestBuilder {
        let method = match req.method {
            Method::Get => reqwest::Method::GET,
            Method::Post => reqwest::Method::POST,
            Method::Put => reqwest::Method::PUT,
            Method::Patch => reqwest::Method::PATCH,
            Method::Delete => reqwest::Method::DELETE,
        };
        let mut b = self
            .client
            .request(method, format!("{}{}", self.base_url, req.path));
        if let Some((user, pass)) = &self.auth {
            b = b.basic_auth(user, Some(pass));
        }
        for (k, v) in &req.headers {
            b = b.header(k, v);
        }
        if let Some(body) = &req.body {
            b = b
                .header("content-type", "application/json")
                .body(body.to_string());
        }
        b
    }
}

fn parse_body(bytes: &[u8]) -> Value {
    if bytes.is_empty() {
        return Value::Null;
    }
    serde_json::from_slice(bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(bytes).into_owned()))
}

#[async_trait]
impl Transport for HttpTransport {
    async fn send(&self, req: HttpRequest) -> Result<HttpResponse> {
        let resp = self
            .builder(&req)
            .send()
            .await
            .map_err(|e| Error::Transport(e.to_string()))?;
        let status = resp.status().as_u16();
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| Error::Transport(e.to_string()))?;
        Ok(HttpResponse {
            status,
            body: parse_body(&bytes),
        })
    }

    async fn open_sse(&self, req: HttpRequest) -> Result<SseStream> {
        let resp = self
            .builder(&req)
            .header("accept", "text/event-stream")
            .send()
            .await
            .map_err(|e| Error::Transport(e.to_string()))?;
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Http {
                status,
                path: req.path,
                body,
            });
        }
        let state = (
            resp.bytes_stream().boxed(),
            SseParser::new(),
            VecDeque::<SseFrame>::new(),
        );
        let out = stream::unfold(state, |(mut body, mut parser, mut queue)| async move {
            loop {
                if let Some(frame) = queue.pop_front() {
                    return Some((Ok(frame), (body, parser, queue)));
                }
                match body.next().await {
                    Some(Ok(chunk)) => queue.extend(parser.push(&chunk)),
                    Some(Err(e)) => {
                        return Some((
                            Err(Error::Transport(e.to_string())),
                            (body, parser, VecDeque::new()),
                        ))
                    }
                    None => return None,
                }
            }
        });
        Ok(Box::pin(out))
    }
}
