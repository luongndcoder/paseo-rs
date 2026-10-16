//! Trusted proxy set, the edge-side mirror of the daemon's `trustedProxies` setting
//! (Express `trust proxy`): `true`, or a list of keywords (`loopback`, `linklocal`,
//! `uniquelocal`), IPs and CIDR ranges. Default `["loopback"]`.
//!
//! The edge keeps a peer's `X-Forwarded-*` chain only when the peer is trusted; otherwise it
//! drops the chain so a direct client cannot forge it. The TS worker adds loopback (the edge
//! itself) to its own trusted set in edge mode, so Express still resolves the real client
//! through the chain the edge forwards.

use std::net::IpAddr;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Matcher {
    Loopback,
    LinkLocal,
    UniqueLocal,
    Cidr(IpAddr, u8),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrustedProxies {
    All,
    List(Vec<Matcher>),
}

impl Default for TrustedProxies {
    fn default() -> Self {
        TrustedProxies::List(vec![Matcher::Loopback])
    }
}

fn parse_matcher(token: &str) -> Option<Matcher> {
    let token = token.trim();
    match token.to_ascii_lowercase().as_str() {
        "loopback" => return Some(Matcher::Loopback),
        "linklocal" => return Some(Matcher::LinkLocal),
        "uniquelocal" => return Some(Matcher::UniqueLocal),
        _ => {}
    }
    let (addr, prefix) = match token.split_once('/') {
        Some((addr, prefix)) => (addr, Some(prefix.parse::<u8>().ok()?)),
        None => (token, None),
    };
    let ip: IpAddr = addr.parse().ok()?;
    let max = if ip.is_ipv4() { 32 } else { 128 };
    let prefix = prefix.unwrap_or(max);
    (prefix <= max).then_some(Matcher::Cidr(ip.to_canonical(), prefix))
}

impl TrustedProxies {
    /// Parses a list the way config does. Unknown tokens are ignored, like an unparsable
    /// entry would never match a peer.
    pub fn from_list<I: IntoIterator<Item = S>, S: AsRef<str>>(tokens: I) -> Self {
        TrustedProxies::List(
            tokens
                .into_iter()
                .filter_map(|t| parse_matcher(t.as_ref()))
                .collect(),
        )
    }

    /// `PASEO_TRUSTED_PROXIES` (`parseTrustedProxiesEnv`): truthy -> all, falsy -> none,
    /// otherwise a comma-separated list. Empty/unset -> `None`.
    pub fn from_env_value(value: &str) -> Option<Self> {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return None;
        }
        match trimmed.to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Some(TrustedProxies::All),
            "0" | "false" | "no" | "off" => Some(TrustedProxies::List(Vec::new())),
            _ => Some(Self::from_list(trimmed.split(','))),
        }
    }

    /// Persisted `daemon.trustedProxies`: `true` or an array of strings.
    pub fn from_json(value: &serde_json::Value) -> Option<Self> {
        match value {
            serde_json::Value::Bool(true) => Some(TrustedProxies::All),
            serde_json::Value::Array(items) => Some(Self::from_list(
                items.iter().filter_map(serde_json::Value::as_str),
            )),
            _ => None,
        }
    }

    pub fn trusts(&self, ip: IpAddr) -> bool {
        let ip = ip.to_canonical();
        match self {
            TrustedProxies::All => true,
            TrustedProxies::List(matchers) => matchers.iter().any(|m| match m {
                Matcher::Loopback => ip.is_loopback(),
                Matcher::LinkLocal => match ip {
                    IpAddr::V4(v4) => v4.is_link_local(),
                    IpAddr::V6(v6) => (v6.segments()[0] & 0xffc0) == 0xfe80,
                },
                Matcher::UniqueLocal => match ip {
                    IpAddr::V4(v4) => v4.is_private(),
                    IpAddr::V6(v6) => (v6.segments()[0] & 0xfe00) == 0xfc00,
                },
                Matcher::Cidr(net, prefix) => match (net, ip) {
                    (IpAddr::V4(net), IpAddr::V4(ip)) => {
                        let p = u32::from(*prefix);
                        p == 0 || (u32::from(*net) >> (32 - p)) == (u32::from(ip) >> (32 - p))
                    }
                    (IpAddr::V6(net), IpAddr::V6(ip)) => {
                        let p = u32::from(*prefix);
                        p == 0 || (u128::from(*net) >> (128 - p)) == (u128::from(ip) >> (128 - p))
                    }
                    _ => false,
                },
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn default_trusts_only_loopback() {
        let t = TrustedProxies::default();
        assert!(t.trusts(ip("127.0.0.1")));
        assert!(t.trusts(ip("::1")));
        assert!(t.trusts(ip("::ffff:127.0.0.1")));
        assert!(!t.trusts(ip("172.17.0.1")));
    }

    #[test]
    fn list_supports_ips_cidrs_and_keywords() {
        let t = TrustedProxies::from_list(["172.17.0.1", "10.0.0.0/8", "uniquelocal"]);
        assert!(t.trusts(ip("172.17.0.1")));
        assert!(t.trusts(ip("10.9.9.9")));
        assert!(t.trusts(ip("192.168.1.2")));
        assert!(!t.trusts(ip("8.8.8.8")));
        assert!(!t.trusts(ip("127.0.0.1")));
    }

    #[test]
    fn env_value_follows_the_ts_grammar() {
        assert_eq!(TrustedProxies::from_env_value(""), None);
        assert_eq!(
            TrustedProxies::from_env_value("true"),
            Some(TrustedProxies::All)
        );
        let none = TrustedProxies::from_env_value("off").unwrap();
        assert!(!none.trusts(ip("127.0.0.1")));
        let some = TrustedProxies::from_env_value("1.2.3.4, loopback").unwrap();
        assert!(some.trusts(ip("1.2.3.4")) && some.trusts(ip("::1")));
    }

    #[test]
    fn json_true_trusts_everyone() {
        let t = TrustedProxies::from_json(&serde_json::json!(true)).unwrap();
        assert!(t.trusts(ip("8.8.8.8")));
        assert_eq!(TrustedProxies::from_json(&serde_json::json!(false)), None);
    }
}
