use jiff::Timestamp;
use std::str::FromStr;

/// `Date.prototype.toISOString()` for a millisecond epoch.
pub fn to_iso(ms: i64) -> String {
    match Timestamp::from_millisecond(ms) {
        Ok(ts) => ts.strftime("%Y-%m-%dT%H:%M:%S%.3fZ").to_string(),
        Err(_) => String::from("Invalid Date"),
    }
}

/// Lenient `new Date(str).getTime()`. Returns `None` for what JS would parse as
/// an Invalid Date (NaN). Strings without a UTC offset are read as UTC (JS reads
/// date-time forms without offset as local time).
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

pub fn now_ms() -> i64 {
    Timestamp::now().as_millisecond()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_round_trip() {
        let ms = parse_js_date("2026-01-01T00:01:00.000Z").unwrap();
        assert_eq!(to_iso(ms), "2026-01-01T00:01:00.000Z");
        assert_eq!(to_iso(ms + 5), "2026-01-01T00:01:00.005Z");
    }

    #[test]
    fn lenient_forms() {
        assert_eq!(
            parse_js_date("2026-01-01"),
            parse_js_date("2026-01-01T00:00:00Z")
        );
        assert_eq!(parse_js_date("garbage"), None);
    }
}
