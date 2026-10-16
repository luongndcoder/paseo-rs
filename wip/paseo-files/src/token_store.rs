use std::collections::HashMap;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq)]
pub struct DownloadTokenEntry {
    pub token: String,
    pub path: String,
    pub absolute_path: String,
    pub file_name: String,
    pub mime_type: String,
    pub size: u64,
    expires_at: Instant,
}

impl DownloadTokenEntry {
    pub fn expires_at(&self) -> Instant {
        self.expires_at
    }
}

#[derive(Debug, Clone)]
pub struct DownloadTokenInput {
    pub path: String,
    pub absolute_path: String,
    pub file_name: String,
    pub mime_type: String,
    pub size: u64,
}

/// Single-use download tokens with a TTL. Callers pass the clock so tests do
/// not sleep; the `*` convenience methods use `Instant::now()`.
pub struct DownloadTokenStore {
    ttl: Duration,
    tokens: HashMap<String, DownloadTokenEntry>,
}

impl DownloadTokenStore {
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            tokens: HashMap::new(),
        }
    }

    pub fn issue_token(&mut self, input: DownloadTokenInput) -> DownloadTokenEntry {
        self.issue_token_at(input, Instant::now())
    }

    pub fn issue_token_at(
        &mut self,
        input: DownloadTokenInput,
        now: Instant,
    ) -> DownloadTokenEntry {
        self.tokens.retain(|_, e| e.expires_at > now);
        let token = uuid::Uuid::new_v4().to_string();
        let entry = DownloadTokenEntry {
            token: token.clone(),
            path: input.path,
            absolute_path: input.absolute_path,
            file_name: input.file_name,
            mime_type: input.mime_type,
            size: input.size,
            expires_at: now + self.ttl,
        };
        self.tokens.insert(token, entry.clone());
        entry
    }

    pub fn consume_token(&mut self, token: &str) -> Option<DownloadTokenEntry> {
        self.consume_token_at(token, Instant::now())
    }

    /// Removes the token whether or not it is still valid.
    pub fn consume_token_at(&mut self, token: &str, now: Instant) -> Option<DownloadTokenEntry> {
        let entry = self.tokens.remove(token)?;
        if entry.expires_at <= now {
            return None;
        }
        Some(entry)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input() -> DownloadTokenInput {
        DownloadTokenInput {
            path: "a.txt".into(),
            absolute_path: "/w/a.txt".into(),
            file_name: "a.txt".into(),
            mime_type: "text/plain".into(),
            size: 3,
        }
    }

    #[test]
    fn token_is_single_use() {
        let mut store = DownloadTokenStore::new(Duration::from_secs(60));
        let entry = store.issue_token(input());
        assert!(store.consume_token(&entry.token).is_some());
        assert!(store.consume_token(&entry.token).is_none());
    }

    #[test]
    fn expired_token_is_rejected_and_removed() {
        let mut store = DownloadTokenStore::new(Duration::from_secs(10));
        let t0 = Instant::now();
        let entry = store.issue_token_at(input(), t0);
        assert!(store
            .consume_token_at(&entry.token, t0 + Duration::from_secs(10))
            .is_none());
        assert!(store.consume_token_at(&entry.token, t0).is_none());
    }

    #[test]
    fn issuing_prunes_expired_tokens() {
        let mut store = DownloadTokenStore::new(Duration::from_secs(10));
        let t0 = Instant::now();
        let old = store.issue_token_at(input(), t0);
        store.issue_token_at(input(), t0 + Duration::from_secs(11));
        assert!(store.consume_token_at(&old.token, t0).is_none());
    }
}
