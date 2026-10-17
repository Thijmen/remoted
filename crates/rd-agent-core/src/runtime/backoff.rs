//! Exponential reconnect backoff with ±20 % jitter.

use std::time::Duration;

use rand::Rng as _;

const JITTER: f64 = 0.2;

#[derive(Debug, Clone)]
pub(crate) struct Backoff {
    min: Duration,
    max: Duration,
    next: Duration,
}

impl Backoff {
    pub(crate) fn new(min: Duration, max: Duration) -> Self {
        let max = max.max(min);
        Backoff {
            min,
            max,
            next: min,
        }
    }

    /// The delay before the next attempt; doubles (up to `max`) on every call.
    pub(crate) fn next_delay(&mut self) -> Duration {
        let base = self.next;
        self.next = (self.next * 2).min(self.max);
        jitter(base)
    }

    /// The shortest delay, without advancing the backoff (the gateway is restarting behind its
    /// proxy and will be back in seconds).
    pub(crate) fn min_delay(&self) -> Duration {
        jitter(self.min)
    }

    /// Jump straight to the longest delay (the gateway refused us; retrying fast won't help).
    pub(crate) fn max_delay(&mut self) -> Duration {
        self.next = self.max;
        jitter(self.max)
    }

    /// After a successful connection the next failure starts again at `min`.
    pub(crate) fn reset(&mut self) {
        self.next = self.min;
    }
}

fn jitter(d: Duration) -> Duration {
    d.mul_f64(rand::rng().random_range(1.0 - JITTER..=1.0 + JITTER))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn within(d: Duration, base_ms: u64) -> bool {
        let ms = d.as_secs_f64() * 1000.0;
        let base = base_ms as f64;
        ms >= base * (1.0 - JITTER) - 1e-6 && ms <= base * (1.0 + JITTER) + 1e-6
    }

    #[test]
    fn doubles_caps_and_resets() {
        let mut b = Backoff::new(Duration::from_secs(1), Duration::from_secs(30));
        for base in [1000, 2000, 4000, 8000, 16000, 30000, 30000] {
            let d = b.next_delay();
            assert!(within(d, base), "{d:?} vs {base} ms");
        }
        b.reset();
        assert!(within(b.next_delay(), 1000));
        assert!(within(b.min_delay(), 1000));
        assert!(within(b.next_delay(), 2000), "min_delay does not advance");
        assert!(within(b.max_delay(), 30000));
        assert!(within(b.next_delay(), 30000));
    }
}
