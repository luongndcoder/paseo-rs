/// Convert an exact rolling interval to an equivalent five-field cron cadence.
/// Returns `None` when cron's calendar boundaries would change the interval.
pub fn every_ms_to_five_field_cron(every_ms: u64) -> Option<String> {
    if every_ms == 0 || !every_ms.is_multiple_of(60_000) {
        return None;
    }
    let minutes = every_ms / 60_000;
    if minutes < 60 && 60 % minutes == 0 {
        return Some(format!("*/{minutes} * * * *"));
    }
    if minutes == 60 {
        return Some("0 * * * *".to_string());
    }
    if !minutes.is_multiple_of(60) {
        return None;
    }
    let hours = minutes / 60;
    if hours < 24 && 24 % hours == 0 {
        return Some(format!("0 */{hours} * * *"));
    }
    if hours == 24 {
        return Some("0 0 * * *".to_string());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_representable_intervals() {
        for (ms, cron) in [
            (60_000, "*/1 * * * *"),
            (15 * 60_000, "*/15 * * * *"),
            (60 * 60_000, "0 * * * *"),
            (6 * 60 * 60_000, "0 */6 * * *"),
            (24 * 60 * 60_000, "0 0 * * *"),
        ] {
            assert_eq!(every_ms_to_five_field_cron(ms).as_deref(), Some(cron));
        }
    }

    #[test]
    fn rejects_unrepresentable_intervals() {
        for ms in [30_000, 7 * 60_000, 5 * 60 * 60_000, 48 * 60 * 60_000] {
            assert_eq!(every_ms_to_five_field_cron(ms), None);
        }
    }
}
