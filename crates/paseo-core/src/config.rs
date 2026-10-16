//! Pure config helpers (`runtime-mcp-config.ts` and the config spreads of the manager).

use paseo_provider_api::{AgentSessionConfig, JsonObject};
use serde_json::{json, Value};

pub const PASEO_MCP_SERVER_NAME: &str = "paseo";
const PASEO_MCP_PATHNAME: &str = "/mcp/agents";

/// `new URL(url).pathname` for `http(s)` URLs; `None` when `url` does not parse as one.
fn url_pathname(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    if scheme.is_empty()
        || !scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c))
    {
        return None;
    }
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    if authority_end == 0 {
        return None;
    }
    let tail = &rest[authority_end..];
    let path_end = tail.find(['?', '#']).unwrap_or(tail.len());
    let path = &tail[..path_end];
    Some(if path.is_empty() {
        "/".to_owned()
    } else {
        path.to_owned()
    })
}

fn is_internal_paseo_mcp_server(server: &Value) -> bool {
    let kind = server.get("type").and_then(Value::as_str);
    if kind != Some("http") && kind != Some("sse") {
        return false;
    }
    server
        .get("url")
        .and_then(Value::as_str)
        .and_then(url_pathname)
        .is_some_and(|path| path == PASEO_MCP_PATHNAME)
}

/// `stripInternalPaseoMcpServer`: drop the injected `paseo` MCP entry before storing.
pub fn strip_internal_paseo_mcp_server(mut config: AgentSessionConfig) -> AgentSessionConfig {
    let Some(Value::Object(servers)) = &config.mcp_servers else {
        return config;
    };
    let Some(paseo) = servers.get(PASEO_MCP_SERVER_NAME) else {
        return config;
    };
    if !is_internal_paseo_mcp_server(paseo) {
        return config;
    }
    let mut rest = servers.clone();
    rest.shift_remove(PASEO_MCP_SERVER_NAME);
    config.mcp_servers = (!rest.is_empty()).then_some(Value::Object(rest));
    config
}

/// `withRuntimePaseoMcpServer`: add the `paseo` MCP entry for this launch only.
pub fn with_runtime_paseo_mcp_server(
    config: AgentSessionConfig,
    agent_id: &str,
    mcp_base_url: Option<&str>,
    mcp_auth_token: Option<&str>,
) -> AgentSessionConfig {
    let mut stored = strip_internal_paseo_mcp_server(config);
    let has_user_paseo = matches!(&stored.mcp_servers, Some(Value::Object(s)) if s.contains_key(PASEO_MCP_SERVER_NAME));
    let Some(base) = mcp_base_url.filter(|_| !has_user_paseo) else {
        return stored;
    };
    let mut entry = JsonObject::new();
    entry.insert("type".into(), json!("http"));
    entry.insert(
        "url".into(),
        json!(format!("{base}?callerAgentId={agent_id}")),
    );
    if let Some(token) = mcp_auth_token {
        entry.insert(
            "headers".into(),
            json!({"Authorization": format!("Bearer {token}")}),
        );
    }
    let mut servers = JsonObject::new();
    servers.insert(PASEO_MCP_SERVER_NAME.into(), Value::Object(entry));
    if let Some(Value::Object(existing)) = &stored.mcp_servers {
        for (name, server) in existing {
            servers.insert(name.clone(), server.clone());
        }
    }
    stored.mcp_servers = Some(Value::Object(servers));
    stored
}

/// `{...base, ...overrides, provider}` over a config. A `null` override is a JS `undefined`
/// own property: it removes the key from the result.
pub fn merge_config(
    base: &JsonObject,
    overrides: Option<&JsonObject>,
    provider: &str,
) -> AgentSessionConfig {
    let mut merged = base.clone();
    for (key, value) in overrides.into_iter().flatten() {
        if value.is_null() {
            merged.shift_remove(key);
        } else {
            merged.insert(key.clone(), value.clone());
        }
    }
    merged.insert("provider".into(), Value::String(provider.to_owned()));
    if !matches!(merged.get("cwd"), Some(Value::String(_))) {
        merged.insert("cwd".into(), Value::String(String::new()));
    }
    // Keys with a type the model cannot hold fall back to its defaults rather than failing.
    serde_json::from_value(Value::Object(merged.clone())).unwrap_or_else(|_| {
        let mut config = AgentSessionConfig::new(provider, "");
        config.extra = merged;
        config
    })
}

pub fn config_to_object(config: &AgentSessionConfig) -> JsonObject {
    match serde_json::to_value(config) {
        Ok(Value::Object(object)) => object,
        _ => JsonObject::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_servers(servers: Value) -> AgentSessionConfig {
        let mut c = AgentSessionConfig::new("codex", "/x");
        c.mcp_servers = Some(servers);
        c
    }

    #[test]
    fn pathname_parsing() {
        assert_eq!(
            url_pathname("http://127.0.0.1:1/mcp/agents?callerAgentId=a").as_deref(),
            Some("/mcp/agents")
        );
        assert_eq!(url_pathname("https://h").as_deref(), Some("/"));
        assert_eq!(url_pathname("not a url"), None);
        assert_eq!(url_pathname("http://"), None);
    }

    #[test]
    fn strips_only_the_internal_paseo_entry() {
        let internal = with_servers(json!({
            "paseo": {"type": "http", "url": "http://h/mcp/agents?callerAgentId=x"},
            "other": {"type": "stdio", "command": "c"}
        }));
        let stripped = strip_internal_paseo_mcp_server(internal);
        assert_eq!(
            stripped.mcp_servers,
            Some(json!({"other": {"type": "stdio", "command": "c"}}))
        );

        let only = with_servers(json!({"paseo": {"type": "sse", "url": "http://h/mcp/agents"}}));
        assert_eq!(strip_internal_paseo_mcp_server(only).mcp_servers, None);

        let user = with_servers(json!({"paseo": {"type": "stdio", "command": "node"}}));
        assert!(strip_internal_paseo_mcp_server(user.clone()).mcp_servers == user.mcp_servers);
    }

    #[test]
    fn runtime_injection_puts_paseo_first_and_keeps_a_user_entry() {
        let base = with_servers(json!({"other": {"type": "stdio", "command": "c"}}));
        let injected = with_runtime_paseo_mcp_server(
            base,
            "agent-1",
            Some("http://h/mcp/agents"),
            Some("tok"),
        );
        let servers = injected.mcp_servers.unwrap();
        let keys: Vec<_> = servers.as_object().unwrap().keys().cloned().collect();
        assert_eq!(keys, ["paseo", "other"]);
        assert_eq!(
            servers["paseo"]["url"],
            json!("http://h/mcp/agents?callerAgentId=agent-1")
        );
        assert_eq!(
            servers["paseo"]["headers"]["Authorization"],
            json!("Bearer tok")
        );

        let user = with_servers(json!({"paseo": {"type": "stdio", "command": "node"}}));
        let kept =
            with_runtime_paseo_mcp_server(user.clone(), "a", Some("http://h/mcp/agents"), None);
        assert_eq!(kept.mcp_servers, user.mcp_servers);
        assert_eq!(
            with_runtime_paseo_mcp_server(AgentSessionConfig::new("c", "/x"), "a", None, None)
                .mcp_servers,
            None
        );
    }

    #[test]
    fn merge_applies_overrides_and_null_removes_keys() {
        let base = json!({"provider": "x", "cwd": "/a", "model": "old", "systemPrompt": "p"});
        let overrides = json!({"model": null, "cwd": "/b"});
        let merged = merge_config(base.as_object().unwrap(), overrides.as_object(), "codex");
        assert_eq!(merged.provider, "codex");
        assert_eq!(merged.cwd, "/b");
        assert_eq!(merged.model, None);
        assert_eq!(merged.system_prompt.as_deref(), Some("p"));
    }
}
