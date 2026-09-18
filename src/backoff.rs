//! Exponential backoff with jitter for polling the API.
//!
//! Polls start near the latency the API usually needs (a receive's invoice is typically ready
//! within about 100 ms and a send settles within about 250 ms), then double up to a ceiling so
//! a slow payment is not hammered. Equal jitter spreads concurrent clients apart. A server
//! retry hint replaces one pause without resetting the schedule.

use std::time::Duration;

/// Pauses no poll loop exceeds without a server hint.
pub const POLL_CEILING: Duration = Duration::from_secs(2);

pub struct Backoff {
    step: Duration,
    ceiling: Duration,
    rng: fastrand::Rng,
}

impl Backoff {
    pub fn new(initial: Duration, ceiling: Duration) -> Self {
        Self::seeded(initial, ceiling, fastrand::Rng::new())
    }

    fn seeded(initial: Duration, ceiling: Duration, rng: fastrand::Rng) -> Self {
        Self {
            step: initial.min(ceiling),
            ceiling,
            rng,
        }
    }

    /// The next pause: between half the current step and the full step, after which the step
    /// doubles until it reaches the ceiling.
    pub fn pause(&mut self) -> Duration {
        let step = self.step;
        let half = step / 2;
        let spread = u64::try_from(half.as_nanos()).unwrap_or(u64::MAX);
        let jitter = Duration::from_nanos(self.rng.u64(0..=spread));
        self.step = step.saturating_mul(2).min(self.ceiling);
        half + jitter
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backoff(initial_ms: u64, ceiling_ms: u64) -> Backoff {
        Backoff::seeded(
            Duration::from_millis(initial_ms),
            Duration::from_millis(ceiling_ms),
            fastrand::Rng::with_seed(7),
        )
    }

    #[test]
    fn pauses_start_small_and_double_toward_the_ceiling() {
        let mut backoff = backoff(100, 2000);
        let expected = [100, 200, 400, 800, 1600, 2000, 2000];
        for step in expected {
            let pause = backoff.pause();
            assert!(
                pause >= Duration::from_millis(step / 2) && pause <= Duration::from_millis(step),
                "pause {pause:?} is outside the jitter window for a {step} ms step"
            );
        }
    }

    #[test]
    fn jitter_varies_between_draws() {
        let mut backoff = backoff(1000, 1000);
        let draws: Vec<_> = (0..8).map(|_| backoff.pause()).collect();
        assert!(draws.iter().any(|pause| *pause != draws[0]));
        assert!(
            draws
                .iter()
                .all(|pause| *pause >= Duration::from_millis(500))
        );
    }

    #[test]
    fn an_initial_pause_above_the_ceiling_is_clamped() {
        let mut backoff = backoff(5000, 250);
        assert!(backoff.pause() <= Duration::from_millis(250));
    }
}
