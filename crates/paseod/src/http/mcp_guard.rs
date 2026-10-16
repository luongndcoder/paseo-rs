//! Host allowlist + CORS in front of the in-process `/mcp/agents` route.
//!
//! TS runs both before every route except the service proxy (`bootstrap.ts` Host allowlist then
//! CORS), so a DNS-rebinding page never reaches the MCP endpoint. Mounting the route in process
//! skips that chain, so the same two checks run here. Semantics mirror `hostnames.ts`
//! (`isHostnameAllowed`) and the CORS block in `bootstrap.ts`.

use std::sync::Arc;

use axum::extract::Request;
use axum::http::{header, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};

/// `hostnames` config (`HostnamesConfig` in `hostnames.ts`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Hostnames {
    /// `true`: any Host.
    Any,
    /// Extra patterns on top of the defaults (localhost, `*.localhost`, IPs). Empty = defaults.
    #[default]
    Defaults,
    List(Vec<String>),
}

#[derive(Debug, Clone, Default)]
pub struct McpHttpGuard {
    pub hostnames: Hostnames,
    /// `cors.allowedOrigins` plus the fixed origins (`paseo://app`, the listen address variants).
    pub allowed_origins: Vec<String>,
    /// TS skips the Host check on unix sockets; so does this.
    pub check_host: bool,
}

impl McpHttpGuard {
    /// The fixed origin set TS always adds, for a TCP listener on `host:port`.
    pub fn fixed_origins(host: &str, port: u16) -> Vec<String> {
        vec![
            "paseo://app".into(),
            format!("http://{host}:{port}"),
            format!("http://localhost:{port}"),
            format!("http://127.0.0.1:{port}"),
        ]
    }

    /// Wraps `router` in the Host check and CORS layer.
    pub fn wrap(self, router: Router) -> Router {
        let guard = Arc::new(self);
        router.layer(middleware::from_fn(move |req: Request, next: Next| {
            let guard = guard.clone();
            async move { guard.handle(req, next).await }
        }))
    }

    async fn handle(&self, req: Request, next: Next) -> Response {
        if self.check_host {
            let host = req
                .headers()
                .get(header::HOST)
                .and_then(|v| v.to_str().ok());
            if !hostname_allowed(host, &self.hostnames) {
                return (
                    StatusCode::FORBIDDEN,
                    Json(serde_json::json!({ "error": "Invalid Host header" })),
                )
                    .into_response();
            }
        }
        let origin = req
            .headers()
            .get(header::ORIGIN)
            .and_then(|v| v.to_str().ok())
            .filter(|o| self.allowed_origins.iter().any(|a| a == "*" || a == o))
            .and_then(|o| HeaderValue::from_str(o).ok());
        let preflight = req.method() == Method::OPTIONS;
        let mut response = if preflight {
            StatusCode::NO_CONTENT.into_response()
        } else {
            next.run(req).await
        };
        if let Some(origin) = origin {
            let h = response.headers_mut();
            h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
            h.insert(
                header::ACCESS_CONTROL_ALLOW_METHODS,
                HeaderValue::from_static("GET, POST, DELETE, OPTIONS"),
            );
            h.insert(
                header::ACCESS_CONTROL_ALLOW_HEADERS,
                HeaderValue::from_static("Content-Type, Authorization"),
            );
            h.insert(
                header::ACCESS_CONTROL_ALLOW_CREDENTIALS,
                HeaderValue::from_static("true"),
            );
        }
        response
    }
}

fn hostname_of(header: &str) -> Option<String> {
    let t = header.trim();
    if t.is_empty() {
        return None;
    }
    let name = if let Some(rest) = t.strip_prefix('[') {
        rest.split_once(']')?.0
    } else {
        t.split(':').next().unwrap_or(t)
    };
    Some(name.trim().to_lowercase())
}

/// `isHostnameAllowed` (`hostnames.ts`).
pub fn hostname_allowed(host_header: Option<&str>, hostnames: &Hostnames) -> bool {
    let Some(host) = host_header.and_then(hostname_of).filter(|h| !h.is_empty()) else {
        return false;
    };
    if matches!(hostnames, Hostnames::Any) {
        return true;
    }
    if host == "localhost"
        || host.ends_with(".localhost")
        || host.parse::<std::net::IpAddr>().is_ok()
    {
        return true;
    }
    let Hostnames::List(patterns) = hostnames else {
        return false;
    };
    patterns.iter().any(|p| {
        let p = p.trim().to_lowercase();
        match p.strip_prefix('.') {
            Some("") => false,
            Some(base) => host == base || host.ends_with(&format!(".{base}")),
            None => !p.is_empty() && host == p,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_allow_local_and_ips_only() {
        let d = Hostnames::Defaults;
        assert!(hostname_allowed(Some("localhost:6767"), &d));
        assert!(hostname_allowed(Some("a.localhost"), &d));
        assert!(hostname_allowed(Some("127.0.0.1:1"), &d));
        assert!(hostname_allowed(Some("[::1]:1"), &d));
        assert!(!hostname_allowed(Some("evil.example"), &d));
        assert!(!hostname_allowed(None, &d));
        assert!(!hostname_allowed(Some(""), &d));
    }

    #[test]
    fn patterns_and_any() {
        let l = Hostnames::List(vec![".example.com".into(), "myhost".into()]);
        assert!(hostname_allowed(Some("example.com"), &l));
        assert!(hostname_allowed(Some("a.example.com:80"), &l));
        assert!(hostname_allowed(Some("MyHost"), &l));
        assert!(!hostname_allowed(Some("notexample.com"), &l));
        assert!(hostname_allowed(Some("anything"), &Hostnames::Any));
        assert!(!hostname_allowed(None, &Hostnames::Any));
    }
}
