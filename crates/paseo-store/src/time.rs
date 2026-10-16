//! `Date` helpers with JS semantics. Timestamps are `Date.toISOString()` strings.

use jiff::Timestamp;
use std::str::FromStr;

/// `Date.prototype.toISOString()` for a millisecond epoch.
pub fn to_iso(ms: i64) -> String {
    match Timestamp::from_millisecond(ms) {
        Ok(ts) => ts.strftime("%Y-%m-%dT%H:%M:%S%.3fZ").to_string(),
        Err(_) => String::from("Invalid Date"),
    }
}

pub fn now_iso() -> String {
    to_iso(Timestamp::now().as_millisecond())
}

/// Lenient `Date.parse(str)`. `None` is JS `NaN`. Date-time forms without an offset are
/// read as UTC (JS reads them as local time).
pub fn parse_js_date(value: &str) -> Option<i64> {
    let value = value.trim();
    if let Ok(ts) = Timestamp::from_str(value) {
        return Some(ts.as_millisecond());
    }
    if let Ok(date) = jiff::civil::Date::from_str(value) {
        return date
            .at(0, 0, 0, 0)
            .to_zoned(jiff::tz::TimeZone::UTC)
            .ok()
            .map(|z| z.timestamp().as_millisecond());
    }
    if let Ok(dt) = jiff::civil::DateTime::from_str(value) {
        return dt
            .to_zoned(jiff::tz::TimeZone::UTC)
            .ok()
            .map(|z| z.timestamp().as_millisecond());
    }
    None
}
