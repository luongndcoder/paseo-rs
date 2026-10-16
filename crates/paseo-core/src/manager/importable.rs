//! `listImportableSessions`: each provider is asked in parallel with a 90 s timeout, results are
//! filtered by the query, merged, then ranked by last activity (`agent-manager.ts:1042-1107`).

use std::collections::HashSet;
use std::time::Duration;

use paseo_provider_api::JsonObject;
use paseo_store::time::parse_js_date;
use serde_json::Value;
use tracing::warn;

use super::providers::futures_join_all;
use super::AgentManager;

/// `IMPORTABLE_SESSION_LIST_TIMEOUT_MS`.
pub const IMPORTABLE_SESSION_LIST_TIMEOUT: Duration = Duration::from_millis(90_000);

/// `ImportablePersistedAgentQueryOptions`.
#[derive(Debug, Clone, Default)]
pub struct ImportableQuery {
    pub limit: Option<usize>,
    pub query: Option<String>,
    pub scan_limit: Option<usize>,
    pub cwd: Option<String>,
    /// Only these providers are scanned (on top of the enabled and listing rules).
    pub provider_filter: Option<HashSet<String>>,
}

/// `ImportableSessionProviderError`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportableSessionProviderError {
    pub provider: String,
    pub message: String,
}

/// `ManagedImportableSessionsResult`. Sessions are the provider's rows with `provider` added.
#[derive(Debug, Clone, PartialEq)]
pub struct ImportableSessionsResult {
    pub sessions: Vec<Value>,
    pub provider_errors: Vec<ImportableSessionProviderError>,
}

/// `matchesImportableSessionQuery`.
fn matches_query(session: &Value, raw_query: Option<&str>) -> bool {
    let query = raw_query.unwrap_or_default().trim().to_lowercase();
    if query.is_empty() {
        return true;
    }
    let text = |key: &str| session.get(key).and_then(Value::as_str).map(str::to_owned);
    let cwd = text("cwd").unwrap_or_default().replace('\\', "/");
    let basename = cwd
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .to_owned();
    [
        text("title"),
        text("firstPromptPreview"),
        text("lastPromptPreview"),
        Some(basename),
    ]
    .iter()
    .flatten()
    .any(|value| value.to_lowercase().contains(&query))
}

fn last_activity_ms(session: &Value) -> i64 {
    match session.get("lastActivityAt") {
        Some(Value::String(s)) => parse_js_date(s).unwrap_or(0),
        Some(Value::Number(n)) => n.as_i64().unwrap_or(0),
        _ => 0,
    }
}

impl AgentManager {
    pub async fn list_importable_sessions(
        &self,
        options: ImportableQuery,
    ) -> ImportableSessionsResult {
        let mut entries = Vec::new();
        for provider in self.registered_provider_ids() {
            let Some(client) = self.client_for(&provider) else {
                continue;
            };
            let lists = client
                .capabilities()
                .await
                .map(|c| c.get("supportsSessionListing") == Some(&Value::Bool(true)))
                .unwrap_or(false);
            if lists && self.is_provider_importable(&provider, options.provider_filter.as_ref()) {
                entries.push((provider, client));
            }
        }
        let mut request = JsonObject::new();
        for (key, value) in [
            ("limit", options.limit.map(Value::from)),
            ("query", options.query.clone().map(Value::from)),
            ("scanLimit", options.scan_limit.map(Value::from)),
            ("cwd", options.cwd.clone().map(Value::from)),
        ] {
            if let Some(value) = value {
                request.insert(key.into(), value);
            }
        }
        let results = futures_join_all(entries.iter().map(|(provider, client)| {
            let request = &request;
            let query = options.query.as_deref();
            async move {
                let listed = tokio::time::timeout(
                    IMPORTABLE_SESSION_LIST_TIMEOUT,
                    client.list_importable_sessions(Some(request)),
                )
                .await;
                match listed {
                    Ok(Ok(sessions)) => (
                        sessions
                            .into_iter()
                            .filter(|s| matches_query(s, query))
                            .map(|mut s| {
                                if let Value::Object(map) = &mut s {
                                    map.insert("provider".into(), Value::String(provider.clone()));
                                }
                                s
                            })
                            .collect::<Vec<_>>(),
                        None,
                    ),
                    Ok(Err(error)) => {
                        warn!(%error, %provider, "Failed to list importable sessions for provider");
                        (Vec::new(), Some(error.to_string()))
                    }
                    Err(_) => {
                        let message = format!(
                            "Timed out listing importable sessions for provider '{provider}' after {}ms",
                            IMPORTABLE_SESSION_LIST_TIMEOUT.as_millis()
                        );
                        warn!(%message, "Failed to list importable sessions for provider");
                        (Vec::new(), Some(message))
                    }
                }
            }
        }))
        .await;
        let mut sessions: Vec<Value> = Vec::new();
        let mut provider_errors = Vec::new();
        for ((provider, _), (rows, error)) in entries.iter().zip(results) {
            sessions.extend(rows);
            if let Some(message) = error {
                provider_errors.push(ImportableSessionProviderError {
                    provider: provider.clone(),
                    message,
                });
            }
        }
        sessions.sort_by_key(|s| std::cmp::Reverse(last_activity_ms(s)));
        sessions.truncate(options.limit.unwrap_or(20));
        ImportableSessionsResult {
            sessions,
            provider_errors,
        }
    }

    /// `isProviderImportable`.
    fn is_provider_importable(&self, provider: &str, filter: Option<&HashSet<String>>) -> bool {
        if self
            .inner
            .providers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .enabled
            .get(provider)
            == Some(&false)
        {
            return false;
        }
        filter.is_none_or(|f| f.contains(provider))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn query_matches_title_prompts_or_cwd_basename_case_insensitively() {
        let s = json!({"title": "Invoice cleanup", "cwd": "/tmp/archive"});
        assert!(matches_query(&s, Some("INVOICE")));
        assert!(matches_query(
            &json!({"cwd": "/tmp/invoice-service/"}),
            Some("invoice")
        ));
        assert!(matches_query(
            &json!({"cwd": "C:\\work\\Invoice"}),
            Some("invoice")
        ));
        assert!(!matches_query(
            &json!({"cwd": "/tmp/invoice/other"}),
            Some("invoice")
        ));
        assert!(matches_query(&s, Some("   ")));
        assert!(matches_query(&s, None));
    }
}
