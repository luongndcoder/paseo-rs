use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::error::{Error, Result};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Random RFC 4122 v4-shaped id (no external crate). Not cryptographic.
pub fn random_uuid() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut h1 = RandomState::new().build_hasher();
    h1.write_u128(nanos);
    h1.write_u64(seq);
    let mut h2 = RandomState::new().build_hasher();
    h2.write_u64(seq);
    h2.write_u128(nanos);
    let mut b = [0u8; 16];
    b[..8].copy_from_slice(&h1.finish().to_le_bytes());
    b[8..].copy_from_slice(&h2.finish().to_le_bytes());
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let hex: Vec<String> = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        hex[0..4].concat(),
        hex[4..6].concat(),
        hex[6..8].concat(),
        hex[8..10].concat(),
        hex[10..16].concat()
    )
}

pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// `withTimeout` from the TS adapter.
pub async fn with_timeout<T>(
    duration: Duration,
    message: impl Into<String>,
    fut: impl Future<Output = Result<T>>,
) -> Result<T> {
    match tokio::time::timeout(duration, fut).await {
        Ok(r) => r,
        Err(_) => Err(Error::Timeout(message.into())),
    }
}
