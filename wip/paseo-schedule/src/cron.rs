//! Five-field cron parsing and next-run computation, ported 1:1 from
//! `protocol/src/schedule/cron-expression.ts` and `server/schedule/cron.ts`.
//!
//! Semantics worth knowing (all inherited from TS): day-of-month AND day-of-week
//! must both match (not the Vixie OR rule), day-of-week is 0-6 only, names like
//! `mon` and the `L`/`#` extensions are rejected, and the search is a
//! minute-by-minute walk limited to 366 days.

use crate::error::{Result, ScheduleError};
use crate::types::ScheduleCadence;
use jiff::tz::{Offset, TimeZone};
use jiff::Timestamp;

struct FieldBounds {
    min: u32,
    max: u32,
    name: &'static str,
}

const BOUNDS: [FieldBounds; 5] = [
    FieldBounds {
        min: 0,
        max: 59,
        name: "minute",
    },
    FieldBounds {
        min: 0,
        max: 23,
        name: "hour",
    },
    FieldBounds {
        min: 1,
        max: 31,
        name: "day-of-month",
    },
    FieldBounds {
        min: 1,
        max: 12,
        name: "month",
    },
    FieldBounds {
        min: 0,
        max: 6,
        name: "day-of-week",
    },
];

/// Set of allowed values for one field (all bounds fit in a u64 bitset).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CronFieldMatcher(u64);

impl CronFieldMatcher {
    pub fn matches(&self, value: u32) -> bool {
        value < 64 && (self.0 >> value) & 1 == 1
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParsedCronExpression {
    pub minute: CronFieldMatcher,
    pub hour: CronFieldMatcher,
    pub day_of_month: CronFieldMatcher,
    pub month: CronFieldMatcher,
    pub day_of_week: CronFieldMatcher,
}

fn err(message: String) -> ScheduleError {
    ScheduleError::Message(message)
}

fn parse_step(part: &str, bounds: &FieldBounds) -> Result<(String, u64)> {
    let pieces: Vec<&str> = part.split('/').collect();
    if pieces.len() > 2 {
        return Err(err(format!("Invalid cron {} step", bounds.name)));
    }
    let base = pieces[0].to_string();
    let step = match pieces.get(1) {
        None => 1,
        Some(source) => {
            let normalized = source.trim();
            // parseInt + String(step) === source: rejects "05", "+5", "1e3", "", "5x".
            match normalized.parse::<u64>() {
                Ok(n) if n > 0 && n.to_string() == normalized => n,
                _ => return Err(err(format!("Invalid cron {} step", bounds.name))),
            }
        }
    };
    Ok((base, step))
}

fn is_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

fn parse_u64_saturating(s: &str) -> u64 {
    s.parse::<u64>().unwrap_or(u64::MAX)
}

fn add_range(allowed: &mut u64, start: u32, end: u32, step: u64) {
    let mut value = start as u64;
    while value <= end as u64 {
        *allowed |= 1u64 << value;
        value += step;
    }
}

fn parse_field(source: &str, bounds: &FieldBounds) -> Result<CronFieldMatcher> {
    let trimmed = source.trim();
    if trimmed.is_empty() {
        return Err(err(format!("Invalid cron {} field", bounds.name)));
    }
    let mut allowed: u64 = 0;
    for raw_part in trimmed.split(',') {
        let part = raw_part.trim();
        if part.is_empty() {
            return Err(err(format!("Invalid cron {} field", bounds.name)));
        }
        let (base, step) = parse_step(part, bounds)?;
        if base == "*" {
            add_range(&mut allowed, bounds.min, bounds.max, step);
            continue;
        }
        if let Some((a, b)) = base.split_once('-') {
            if is_digits(a) && is_digits(b) {
                let start = parse_u64_saturating(a);
                let end = parse_u64_saturating(b);
                if start > end || start < bounds.min as u64 || end > bounds.max as u64 {
                    return Err(err(format!("Invalid cron {} range", bounds.name)));
                }
                add_range(&mut allowed, start as u32, end as u32, step);
                continue;
            }
        }
        if !is_digits(&base) {
            return Err(err(format!("Invalid cron {} value", bounds.name)));
        }
        let value = parse_u64_saturating(&base);
        if value < bounds.min as u64 || value > bounds.max as u64 {
            return Err(err(format!("Invalid cron {} value", bounds.name)));
        }
        allowed |= 1u64 << value;
    }
    Ok(CronFieldMatcher(allowed))
}

pub fn parse_cron_expression(expression: &str) -> Result<ParsedCronExpression> {
    let parts: Vec<&str> = expression.split_whitespace().collect();
    if parts.len() != 5 {
        return Err(err("Cron expressions must have 5 fields".to_string()));
    }
    Ok(ParsedCronExpression {
        minute: parse_field(parts[0], &BOUNDS[0])?,
        hour: parse_field(parts[1], &BOUNDS[1])?,
        day_of_month: parse_field(parts[2], &BOUNDS[2])?,
        month: parse_field(parts[3], &BOUNDS[3])?,
        day_of_week: parse_field(parts[4], &BOUNDS[4])?,
    })
}

/// `None` when valid, otherwise the TS-identical error text.
pub fn validate_cron_expression(expression: &str) -> Option<String> {
    parse_cron_expression(expression)
        .err()
        .map(|e| e.to_string())
}

fn resolve_time_zone(name: &str) -> Result<TimeZone> {
    let invalid = || err(format!("Invalid cron time zone: {name}"));
    if name.is_empty() {
        return Err(invalid());
    }
    // Intl accepts fixed offsets such as "+05:30"; mirror that.
    if let Some(offset) = parse_fixed_offset(name) {
        return Ok(TimeZone::fixed(offset));
    }
    TimeZone::get(name).map_err(|_| invalid())
}

fn parse_fixed_offset(name: &str) -> Option<Offset> {
    let bytes = name.as_bytes();
    let sign = match bytes.first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let digits: String = name[1..].chars().filter(|c| *c != ':').collect();
    if !digits.bytes().all(|b| b.is_ascii_digit()) || !(digits.len() == 2 || digits.len() == 4) {
        return None;
    }
    let hours: i32 = digits[..2].parse().ok()?;
    let minutes: i32 = if digits.len() == 4 {
        digits[2..].parse().ok()?
    } else {
        0
    };
    if hours > 23 || minutes > 59 {
        return None;
    }
    Offset::from_seconds(sign * (hours * 3600 + minutes * 60)).ok()
}

pub fn validate_schedule_cadence(cadence: &ScheduleCadence) -> Result<()> {
    if let ScheduleCadence::Cron {
        expression,
        timezone,
    } = cadence
    {
        parse_cron_expression(expression)?;
        if let Some(tz) = timezone {
            resolve_time_zone(tz)?;
        }
    }
    Ok(())
}

/// Next run strictly after `after_ms` (epoch milliseconds).
pub fn compute_next_run_at(cadence: &ScheduleCadence, after_ms: i64) -> Result<i64> {
    let (expression, timezone) = match cadence {
        // COMPAT(scheduleEveryMs) in TS: legacy persisted rolling intervals.
        ScheduleCadence::Every { every_ms } => {
            return Ok(after_ms.saturating_add(*every_ms as i64));
        }
        ScheduleCadence::Cron {
            expression,
            timezone,
        } => (expression, timezone),
    };
    let cron = parse_cron_expression(expression)?;
    let tz = match timezone {
        Some(name) => resolve_time_zone(name)?,
        None => TimeZone::UTC,
    };
    let limit: i64 = 366 * 24 * 60;
    let mut cursor = (after_ms.div_euclid(60_000) + 1) * 60_000;
    for _ in 0..limit {
        let ts = Timestamp::from_millisecond(cursor).map_err(|e| err(e.to_string()))?;
        let dt = tz.to_datetime(ts);
        if cron.minute.matches(dt.minute() as u32)
            && cron.hour.matches(dt.hour() as u32)
            && cron.day_of_month.matches(dt.day() as u32)
            && cron.month.matches(dt.month() as u32)
            && cron
                .day_of_week
                .matches(dt.weekday().to_sunday_zero_offset() as u32)
        {
            return Ok(cursor);
        }
        cursor += 60_000;
    }
    Err(err(format!(
        "Unable to compute next run time for cron expression: {expression}"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::{parse_js_date, to_iso};

    fn ms(s: &str) -> i64 {
        parse_js_date(s).unwrap()
    }

    fn cron(expr: &str) -> ScheduleCadence {
        ScheduleCadence::Cron {
            expression: expr.into(),
            timezone: None,
        }
    }

    fn cron_tz(expr: &str, tz: &str) -> ScheduleCadence {
        ScheduleCadence::Cron {
            expression: expr.into(),
            timezone: Some(tz.into()),
        }
    }

    // --- protocol/cron-expression.test.ts ---

    #[test]
    fn matches_structurally_valid_expressions() {
        let c = parse_cron_expression("*/5 9-17 * 1,6 1-5").unwrap();
        assert!(c.minute.matches(10));
        assert!(!c.minute.matches(11));
        assert!(c.hour.matches(12));
        assert!(!c.hour.matches(18));
        assert!(c.month.matches(6));
        assert!(!c.day_of_week.matches(0));
    }

    #[test]
    fn reports_invalid_expressions_with_server_facing_copy() {
        let cases = [
            ("* * *", "Cron expressions must have 5 fields"),
            ("*/5/2 * * * *", "Invalid cron minute step"),
            ("60 * * * *", "Invalid cron minute value"),
            ("* 24 * * *", "Invalid cron hour value"),
            ("* * 31-1 * *", "Invalid cron day-of-month range"),
            ("* * * */0 *", "Invalid cron month step"),
            ("* * * * mon", "Invalid cron day-of-week value"),
        ];
        for (expr, message) in cases {
            assert_eq!(
                validate_cron_expression(expr).as_deref(),
                Some(message),
                "{expr}"
            );
        }
        assert_eq!(validate_cron_expression("*/5 * * * *"), None);
    }

    #[test]
    fn step_and_field_edge_cases_match_ts() {
        assert_eq!(
            validate_cron_expression("*/05 * * * *").as_deref(),
            Some("Invalid cron minute step")
        );
        assert_eq!(
            validate_cron_expression("*/ * * * *").as_deref(),
            Some("Invalid cron minute step")
        );
        assert_eq!(
            validate_cron_expression("1,,2 * * * *").as_deref(),
            Some("Invalid cron minute field")
        );
        assert_eq!(
            validate_cron_expression("* * * * 7").as_deref(),
            Some("Invalid cron day-of-week value")
        );
        assert_eq!(
            validate_cron_expression("99999999999999999999 * * * *").as_deref(),
            Some("Invalid cron minute value")
        );
        // A step on a single value is accepted and ignored.
        let c = parse_cron_expression("5/2 * * * *").unwrap();
        assert!(c.minute.matches(5));
        assert!(!c.minute.matches(7));
    }

    // --- server/schedule/cron.test.ts ---

    #[test]
    fn computes_the_next_every_cadence_from_the_provided_timestamp() {
        let next = compute_next_run_at(
            &ScheduleCadence::Every {
                every_ms: 5 * 60_000,
            },
            ms("2026-01-01T00:00:00.000Z"),
        )
        .unwrap();
        assert_eq!(to_iso(next), "2026-01-01T00:05:00.000Z");
    }

    #[test]
    fn computes_the_next_cron_minute_match_in_utc() {
        let next =
            compute_next_run_at(&cron("15 9 * * 1-5"), ms("2026-01-05T09:14:30.000Z")).unwrap();
        assert_eq!(to_iso(next), "2026-01-05T09:15:00.000Z");
    }

    #[test]
    fn computes_timezone_cron_matches_at_the_requested_wall_clock_time() {
        let c = cron_tz("0 9 * * 1-5", "America/New_York");
        let winter = compute_next_run_at(&c, ms("2026-01-05T13:59:30.000Z")).unwrap();
        let summer = compute_next_run_at(&c, ms("2026-07-06T12:59:30.000Z")).unwrap();
        assert_eq!(to_iso(winter), "2026-01-05T14:00:00.000Z");
        assert_eq!(to_iso(summer), "2026-07-06T13:00:00.000Z");
    }

    #[test]
    fn keeps_repeated_fall_back_wall_clock_matches_distinct() {
        let c = cron_tz("30 1 1 11 *", "America/New_York");
        let first = compute_next_run_at(&c, ms("2026-11-01T05:29:30.000Z")).unwrap();
        let second = compute_next_run_at(&c, first).unwrap();
        assert_eq!(to_iso(first), "2026-11-01T05:30:00.000Z");
        assert_eq!(to_iso(second), "2026-11-01T06:30:00.000Z");
    }

    #[test]
    fn rejects_invalid_cron_expressions() {
        let e = validate_schedule_cadence(&cron("not-a-valid-cron")).unwrap_err();
        assert_eq!(e.to_string(), "Cron expressions must have 5 fields");
    }

    #[test]
    fn rejects_step_fields_with_extra_slash_tokens() {
        let e = validate_schedule_cadence(&cron("*/5/2 * * * *")).unwrap_err();
        assert_eq!(e.to_string(), "Invalid cron minute step");
    }

    #[test]
    fn rejects_invalid_cron_time_zones() {
        let e = validate_schedule_cadence(&cron_tz("0 9 * * *", "Not/AZone")).unwrap_err();
        assert_eq!(e.to_string(), "Invalid cron time zone: Not/AZone");
    }

    // --- extra coverage for Rust-specific code paths ---

    #[test]
    fn day_of_month_and_day_of_week_must_both_match() {
        // Jan 5 2026 is a Monday: "0 6 5 1 1" fires then, not on other Mondays.
        let next = compute_next_run_at(&cron("0 6 5 1 1"), ms("2026-01-01T00:00:00.000Z")).unwrap();
        assert_eq!(to_iso(next), "2026-01-05T06:00:00.000Z");
    }

    #[test]
    fn fixed_offset_time_zone_is_accepted() {
        let c = cron_tz("0 9 * * *", "+05:30");
        let next = compute_next_run_at(&c, ms("2026-01-01T00:00:00.000Z")).unwrap();
        assert_eq!(to_iso(next), "2026-01-01T03:30:00.000Z");
    }

    #[test]
    fn impossible_dates_error_after_the_search_window() {
        let e =
            compute_next_run_at(&cron("0 0 31 2 *"), ms("2026-01-01T00:00:00.000Z")).unwrap_err();
        assert_eq!(
            e.to_string(),
            "Unable to compute next run time for cron expression: 0 0 31 2 *"
        );
    }
}
