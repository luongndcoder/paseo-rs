//! Static web UI, a port of `packages/server/src/server/web-ui.ts`.
//!
//! Same path resolution (SPA fallback to the dist `index.html`), `br`/`gz` sibling files,
//! content types, cache headers and `window.__PASEO_INITIAL_DAEMON_CONNECTION__` injection.
//! Differences from Express: no `ETag`/`304` on `index.html`, and no `X-Powered-By`.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use axum::body::Body;
use axum::response::Response;
use http::header::{
    HeaderValue, ACCEPT_ENCODING, CACHE_CONTROL, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE,
    EXPIRES, HOST, PRAGMA, VARY,
};
use http::{HeaderMap, Method, StatusCode};
use serde::Serialize;
use tokio_util::io::ReaderStream;

use crate::config::WebUiConfig;

const EXCLUDED_PREFIXES: [&str; 3] = ["/api/", "/mcp/", "/public/"];
const EXCLUDED_PATHS: [&str; 3] = ["/api", "/mcp", "/public"];

/// Requests the web UI middleware never handles; they continue to the backend.
pub fn is_excluded_path(path: &str) -> bool {
    EXCLUDED_PREFIXES.iter().any(|p| path.starts_with(p)) || EXCLUDED_PATHS.contains(&path)
}

/// Whether the edge answers this request itself.
pub fn handles(method: &Method, path: &str) -> bool {
    (method == Method::GET || method == Method::HEAD) && !is_excluded_path(path)
}

fn content_type(path: &Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{}", e.to_ascii_lowercase()))
        .unwrap_or_default();
    match ext.as_str() {
        ".html" => "text/html; charset=utf-8",
        ".js" | ".mjs" => "application/javascript; charset=utf-8",
        ".css" => "text/css; charset=utf-8",
        ".json" => "application/json; charset=utf-8",
        ".png" => "image/png",
        ".jpg" | ".jpeg" => "image/jpeg",
        ".gif" => "image/gif",
        ".svg" => "image/svg+xml",
        ".ico" => "image/x-icon",
        ".woff" => "font/woff",
        ".woff2" => "font/woff2",
        ".ttf" => "font/ttf",
        ".otf" => "font/otf",
        ".eot" => "application/vnd.ms-fontobject",
        ".map" => "application/json",
        _ => "application/octet-stream",
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Encoding {
    Br,
    Gzip,
}

/// `selectEncoding`: substring match on the lowercased header, `br` before `gzip`.
fn select_encoding(accept: Option<&str>) -> Option<Encoding> {
    let accept = accept?.to_ascii_lowercase();
    if accept.contains("br") {
        Some(Encoding::Br)
    } else if accept.contains("gzip") {
        Some(Encoding::Gzip)
    } else {
        None
    }
}

/// `isHashedAsset`: `/[-.][0-9a-f]{16,}[-.]/i` on the file name.
fn is_hashed_asset(file_name: &str) -> bool {
    let b = file_name.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'-' || b[i] == b'.' {
            let mut j = i + 1;
            while j < b.len() && b[j].is_ascii_hexdigit() {
                j += 1;
            }
            if j - (i + 1) >= 16 && j < b.len() && (b[j] == b'-' || b[j] == b'.') {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// POSIX `path.normalize` of an absolute request path, clamped at the root, as the relative
/// components below the dist dir.
fn relative_components(request_path: &str) -> PathBuf {
    let mut stack: Vec<&str> = Vec::new();
    for part in request_path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                stack.pop();
            }
            other => stack.push(other),
        }
    }
    stack.iter().collect()
}

async fn is_file(path: &Path) -> bool {
    tokio::fs::metadata(path)
        .await
        .map(|m| m.is_file())
        .unwrap_or(false)
}

async fn is_dir(path: &Path) -> bool {
    tokio::fs::metadata(path)
        .await
        .map(|m| m.is_dir())
        .unwrap_or(false)
}

async fn resolve_target(dist: &Path, request_path: &str) -> Option<PathBuf> {
    let mut file = dist.join(relative_components(request_path));
    if is_dir(&file).await {
        file.push("index.html");
    }
    if !is_file(&file).await {
        file = dist.join("index.html");
        if !is_file(&file).await {
            return None;
        }
    }
    file.starts_with(dist).then_some(file)
}

fn empty(status: StatusCode) -> Response {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = status;
    response
}

fn set(headers: &mut HeaderMap, name: http::header::HeaderName, value: &'static str) {
    headers.insert(name, HeaderValue::from_static(value));
}

fn set_cache_headers(headers: &mut HeaderMap, is_index: bool, file_name: &str) {
    if is_index {
        set(
            headers,
            CACHE_CONTROL,
            "no-store, no-cache, must-revalidate, proxy-revalidate",
        );
        set(headers, PRAGMA, "no-cache");
        set(headers, EXPIRES, "0");
    } else if is_hashed_asset(file_name) {
        set(
            headers,
            CACHE_CONTROL,
            "public, max-age=31536000, immutable",
        );
    } else {
        set(headers, CACHE_CONTROL, "no-cache");
    }
}

/// Serves `request_path` from the web UI dist dir. Caller has checked [`handles`].
pub async fn serve(
    ui: &WebUiConfig,
    method: &Method,
    request_path: &str,
    headers: &HeaderMap,
    peer: Option<SocketAddr>,
) -> Response {
    let Some(resolved) = resolve_target(&ui.dist_dir, request_path).await else {
        return empty(StatusCode::NOT_FOUND);
    };
    let file_name = resolved
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_string();
    let is_index = file_name.eq_ignore_ascii_case("index.html");

    let (final_file, encoding) = if is_index {
        (resolved.clone(), None)
    } else {
        let accept = headers.get(ACCEPT_ENCODING).and_then(|v| v.to_str().ok());
        match select_encoding(accept) {
            Some(enc) => {
                let mut name = resolved.clone().into_os_string();
                name.push(if enc == Encoding::Br { ".br" } else { ".gz" });
                let compressed = PathBuf::from(name);
                if is_file(&compressed).await {
                    (compressed, Some(enc))
                } else {
                    (resolved.clone(), None)
                }
            }
            None => (resolved.clone(), None),
        }
    };

    let mut response = Response::new(Body::empty());
    let out = response.headers_mut();
    set(out, CONTENT_TYPE, content_type(&resolved));
    if let Some(enc) = encoding {
        set(
            out,
            CONTENT_ENCODING,
            if enc == Encoding::Br { "br" } else { "gzip" },
        );
        set(out, VARY, "Accept-Encoding");
    }
    set_cache_headers(out, is_index, &file_name);

    if method == Method::HEAD {
        return response;
    }

    if is_index {
        return match tokio::fs::read(&final_file).await {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(html) => {
                    let injected = inject_connection_hint(&html, headers, peer, &ui.label);
                    *response.body_mut() = Body::from(injected);
                    response
                }
                Err(_) => empty(StatusCode::INTERNAL_SERVER_ERROR),
            },
            Err(_) => empty(StatusCode::INTERNAL_SERVER_ERROR),
        };
    }

    match tokio::fs::File::open(&final_file).await {
        Ok(file) => {
            if let Ok(meta) = file.metadata().await {
                response
                    .headers_mut()
                    .insert(CONTENT_LENGTH, HeaderValue::from(meta.len()));
            }
            *response.body_mut() = Body::from_stream(ReaderStream::new(file));
            response
        }
        Err(_) => empty(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

#[derive(Serialize)]
struct ConnectionHint<'a> {
    listen: &'a str,
    #[serde(rename = "useTls")]
    use_tls: bool,
    label: &'a str,
}

/// `hasExplicitPort`: the app's `parseHostPort` needs an explicit port.
fn has_explicit_port(host: &str) -> bool {
    let is_port = |s: &str| !s.is_empty() && s.len() <= 5 && s.bytes().all(|b| b.is_ascii_digit());
    if host.starts_with('[') {
        return host
            .find(']')
            .is_some_and(|end| host[end + 1..].strip_prefix(':').is_some_and(is_port));
    }
    match host.find(':') {
        Some(colon) if colon == host.rfind(':').unwrap_or(usize::MAX) => {
            is_port(&host[colon + 1..])
        }
        _ => false,
    }
}

/// `req.protocol === "https"` as Express computes it behind a loopback proxy (its default
/// `trust proxy`): the edge does not terminate TLS, so only `X-Forwarded-Proto` from a
/// loopback peer counts.
fn request_uses_tls(headers: &HeaderMap, peer: Option<SocketAddr>) -> bool {
    let trusted = peer.is_some_and(|p| p.ip().to_canonical().is_loopback());
    trusted
        && headers
            .get("x-forwarded-proto")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(',').next())
            .is_some_and(|v| v.trim() == "https")
}

fn inject_connection_hint(
    html: &str,
    headers: &HeaderMap,
    peer: Option<SocketAddr>,
    label: &str,
) -> String {
    let host = headers
        .get(HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let use_tls = request_uses_tls(headers, peer);
    let listen = if !host.is_empty() && !has_explicit_port(host) {
        format!("{host}:{}", if use_tls { 443 } else { 80 })
    } else {
        host.to_string()
    };
    let json = serde_json::to_string(&ConnectionHint {
        listen: &listen,
        use_tls,
        label,
    })
    .unwrap_or_else(|_| "null".into())
    .replace('<', "\\u003C")
    .replace('>', "\\u003E")
    .replace('&', "\\u0026");
    let script = format!("<script>window.__PASEO_INITIAL_DAEMON_CONNECTION__={json}</script>");
    match html.to_ascii_lowercase().find("</head>") {
        Some(idx) => format!("{}{script}{}", &html[..idx], &html[idx..]),
        None => format!("{script}{html}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashed_asset_detection() {
        assert!(is_hashed_asset("index-0123456789abcdef.js"));
        assert!(is_hashed_asset("main.0123456789ABCDEF0.css"));
        assert!(!is_hashed_asset("index-0123456789abcde.js"));
        assert!(!is_hashed_asset("favicon.ico"));
        assert!(!is_hashed_asset("index-0123456789abcdef"));
    }

    #[test]
    fn normalize_clamps_at_root() {
        assert_eq!(relative_components("/"), PathBuf::new());
        assert_eq!(relative_components("/a/./b//c/"), PathBuf::from("a/b/c"));
        assert_eq!(
            relative_components("/../../etc/passwd"),
            PathBuf::from("etc/passwd")
        );
        assert_eq!(relative_components("/a/../b"), PathBuf::from("b"));
    }

    #[test]
    fn explicit_port_detection() {
        assert!(has_explicit_port("localhost:6767"));
        assert!(!has_explicit_port("example.com"));
        assert!(has_explicit_port("[::1]:80"));
        assert!(!has_explicit_port("[::1]"));
        assert!(!has_explicit_port("::1"));
    }

    #[test]
    fn encoding_selection() {
        assert_eq!(select_encoding(Some("gzip, br")), Some(Encoding::Br));
        assert_eq!(select_encoding(Some("GZIP")), Some(Encoding::Gzip));
        assert_eq!(select_encoding(Some("identity")), None);
        assert_eq!(select_encoding(None), None);
    }
}
