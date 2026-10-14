//! Exponential backoff with full jitter.

use rand::RngExt;
use std::time::Duration;
use tokio::time::Instant;

/// Uniform delay in `[0, min(cap, base * 2^attempt)]`; jitter keeps restarted agents from retrying in lockstep.
pub fn full_jitter(base: Duration, cap: Duration, attempt: u32) -> Duration {
    let ceiling = base.saturating_mul(2u32.saturating_pow(attempt)).min(cap);
    ceiling.mul_f64(rand::rng().random::<f64>())
}

/// Paces retries across a run of consecutive failures and gives up once the run outlasts `give_up_after`.
#[derive(Debug)]
pub struct Backoff {
    base: Duration,
    cap: Duration,
    give_up_after: Duration,
    first_failure: Option<Instant>,
    attempt: u32,
}

impl Backoff {
    pub const fn new(base: Duration, cap: Duration, give_up_after: Duration) -> Self {
        Self {
            base,
            cap,
            give_up_after,
            first_failure: None,
            attempt: 0,
        }
    }

    /// Records a failure and returns how long to wait, or `None` once failures have lasted too long.
    pub fn next_delay(&mut self) -> Option<Duration> {
        let first_failure = *self.first_failure.get_or_insert_with(Instant::now);
        if first_failure.elapsed() > self.give_up_after {
            return None;
        }
        let delay = full_jitter(self.base, self.cap, self.attempt);
        self.attempt = self.attempt.saturating_add(1);
        Some(delay)
    }

    /// Number of failures recorded since the last success.
    pub const fn attempt(&self) -> u32 {
        self.attempt
    }

    pub fn reset(&mut self) {
        self.first_failure = None;
        self.attempt = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_jitter_stays_within_bounds() {
        let base = Duration::from_millis(500);
        let cap = Duration::from_secs(10);
        for attempt in 0..200 {
            let ceiling = if attempt < 5 {
                base * 2u32.pow(attempt)
            } else {
                cap
            };
            for _ in 0..50 {
                assert!(
                    full_jitter(base, cap, attempt) <= ceiling,
                    "attempt {attempt}"
                );
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn backoff_gives_up_after_limit_and_resets() {
        let mut backoff = Backoff::new(
            Duration::from_millis(100),
            Duration::from_secs(1),
            Duration::from_secs(5),
        );
        while let Some(delay) = backoff.next_delay() {
            assert!(delay <= Duration::from_secs(1));
            tokio::time::advance(Duration::from_secs(1)).await;
        }
        assert!(backoff.attempt() >= 5);

        backoff.reset();
        assert_eq!(backoff.attempt(), 0);
        assert!(backoff.next_delay().is_some());
    }
}
