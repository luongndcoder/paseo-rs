//! `isAgentMcpRequestAuthorized` (`server/auth.ts`).
//!
//! `/mcp/agents` is exempt from the daemon's bearer middleware, so it authenticates here: no
//! daemon password means open; otherwise the request must carry the per-process capability token
//! (minted by the core, never persisted) or the daemon password as a bearer token. The password
//! is stored as a bcrypt hash.

use subtle::ConstantTimeEq;

/// `extractHttpBearerToken`: `Bearer <token>` with exactly one token part.
pub fn extract_http_bearer_token(value: Option<&str>) -> Option<String> {
    let value = value?;
    let mut parts = value.split_whitespace();
    let scheme = parts.next()?;
    let token = parts.next()?;
    if scheme != "Bearer" || parts.next().is_some() {
        return None;
    }
    Some(token.to_owned())
}

/// Constant-time equality; differing lengths are rejected first because the compare needs equal
/// lengths (the TS `timingSafeEqual` throws on them).
pub fn tokens_match(provided: &str, expected: &str) -> bool {
    provided.len() == expected.len() && bool::from(provided.as_bytes().ct_eq(expected.as_bytes()))
}

/// `isBearerTokenValidAsync` for a configured bcrypt password hash.
async fn password_matches(password_hash: &str, token: Option<&str>) -> bool {
    let Some(token) = token else {
        return false;
    };
    let hash = password_hash.to_owned();
    let token = token.to_owned();
    // bcrypt at cost 12 takes a few hundred milliseconds.
    tokio::task::spawn_blocking(move || bcrypt::verify(token, &hash).unwrap_or(false))
        .await
        .unwrap_or(false)
}

/// The authorizer. `password_hash` is the daemon password hash, `None` when no password is set.
pub async fn is_agent_mcp_request_authorized(
    password_hash: Option<&str>,
    capability_token: Option<&str>,
    authorization_header: Option<&str>,
) -> bool {
    let Some(password_hash) = password_hash.filter(|p| !p.is_empty()) else {
        return true;
    };
    let token = extract_http_bearer_token(authorization_header);
    if let (Some(capability), Some(provided)) = (capability_token, token.as_deref()) {
        if tokens_match(provided, capability) {
            return true;
        }
    }
    password_matches(password_hash, token.as_deref()).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_extraction() {
        assert_eq!(
            extract_http_bearer_token(Some("Bearer secret")).as_deref(),
            Some("secret")
        );
        assert_eq!(
            extract_http_bearer_token(Some("  Bearer   secret  ")).as_deref(),
            Some("secret")
        );
        assert_eq!(extract_http_bearer_token(Some("Basic secret")), None);
        assert_eq!(extract_http_bearer_token(Some("Bearer a b")), None);
        assert_eq!(extract_http_bearer_token(Some("Bearer")), None);
        assert_eq!(extract_http_bearer_token(None), None);
    }

    #[test]
    fn token_comparison() {
        assert!(tokens_match("abc", "abc"));
        assert!(!tokens_match("abc", "abd"));
        assert!(!tokens_match("abc", "abcd"));
    }

    #[tokio::test]
    async fn authorizer_matrix() {
        const CAP: &str = "cap-token-abc123";
        let hash = bcrypt::hash("correct-password", 4).unwrap();
        // No password: open.
        assert!(is_agent_mcp_request_authorized(None, Some(CAP), None).await);
        // Capability token.
        assert!(
            is_agent_mcp_request_authorized(
                Some(&hash),
                Some(CAP),
                Some("Bearer cap-token-abc123")
            )
            .await
        );
        // Daemon password.
        assert!(
            is_agent_mcp_request_authorized(
                Some(&hash),
                Some(CAP),
                Some("Bearer correct-password")
            )
            .await
        );
        // Neither.
        assert!(!is_agent_mcp_request_authorized(Some(&hash), Some(CAP), None).await);
        assert!(
            !is_agent_mcp_request_authorized(Some(&hash), Some(CAP), Some("Bearer wrong-token"))
                .await
        );
        // Without a minted token only the password works.
        assert!(
            !is_agent_mcp_request_authorized(Some(&hash), None, Some("Bearer cap-token-abc123"))
                .await
        );
    }
}
