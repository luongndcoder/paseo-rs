//! `relationship-retry.test.ts`.

use paseo_hub::{BoundedExponentialRetryPolicy, RetryPolicy};

#[test]
fn keeps_jitter_within_the_lower_and_upper_bounds() {
    let lower = BoundedExponentialRetryPolicy::new(|| 0.0);
    let upper = BoundedExponentialRetryPolicy::new(|| 1.0);
    assert_eq!(lower.delay(0), 375);
    assert_eq!(upper.delay(0), 625);
}

#[test]
fn grows_exponentially_and_caps_the_base_delay() {
    let retry = BoundedExponentialRetryPolicy::new(|| 0.5);
    let delays: Vec<u64> = [0, 1, 2, 3, 4, 5, 6, 20]
        .into_iter()
        .map(|attempt| retry.delay(attempt))
        .collect();
    assert_eq!(
        delays,
        [500, 1_000, 2_000, 4_000, 8_000, 16_000, 30_000, 30_000]
    );
}

#[test]
fn never_schedules_a_zero_delay_tight_loop() {
    let retry = BoundedExponentialRetryPolicy::new(|| 0.0);
    let delays: Vec<u64> = [0, 1, 2].into_iter().map(|a| retry.delay(a)).collect();
    assert_eq!(delays, [375, 750, 1_500]);
}

#[test]
fn a_huge_attempt_counter_stays_at_the_cap() {
    let retry = BoundedExponentialRetryPolicy::new(|| 0.5);
    assert_eq!(retry.delay(u32::MAX), 30_000);
    assert_eq!(retry.delay(2000), 30_000);
}

#[test]
fn the_default_policy_stays_inside_its_jitter_band() {
    let retry = BoundedExponentialRetryPolicy::default();
    for _ in 0..200 {
        let delay = retry.delay(0);
        assert!((375..=625).contains(&delay), "{delay}");
    }
}
