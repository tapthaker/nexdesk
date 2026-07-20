use std::time::Duration;

/// Pure retry-delay policy used by reconnecting application loops.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetryPolicy {
    initial_delay: Duration,
    maximum_delay: Duration,
    multiplier: u32,
}

impl RetryPolicy {
    pub fn fixed(delay: Duration) -> Self {
        Self {
            initial_delay: delay,
            maximum_delay: delay,
            multiplier: 1,
        }
    }

    pub fn exponential(initial_delay: Duration, maximum_delay: Duration) -> Self {
        Self {
            initial_delay,
            maximum_delay: maximum_delay.max(initial_delay),
            multiplier: 2,
        }
    }

    /// Return the delay before retry number `attempt`, where zero is the first retry.
    pub fn delay_for_attempt(&self, attempt: u32) -> Duration {
        if self.multiplier == 1 || self.initial_delay.is_zero() {
            return self.initial_delay;
        }

        let mut delay = self.initial_delay;
        for _ in 0..attempt {
            delay = delay.saturating_mul(self.multiplier);
            if delay >= self.maximum_delay {
                return self.maximum_delay;
            }
        }
        delay.min(self.maximum_delay)
    }
}

impl Default for RetryPolicy {
    fn default() -> Self {
        // Preserve Nexdesk's existing reconnect behavior while moving the
        // decision out of the async connection loop.
        Self::fixed(Duration::from_secs(2))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_preserves_fixed_two_second_retries() {
        let policy = RetryPolicy::default();
        assert_eq!(policy.delay_for_attempt(0), Duration::from_secs(2));
        assert_eq!(policy.delay_for_attempt(1), Duration::from_secs(2));
        assert_eq!(policy.delay_for_attempt(u32::MAX), Duration::from_secs(2));
    }

    #[test]
    fn exponential_policy_grows_until_its_maximum() {
        let policy = RetryPolicy::exponential(Duration::from_secs(2), Duration::from_secs(10));
        assert_eq!(policy.delay_for_attempt(0), Duration::from_secs(2));
        assert_eq!(policy.delay_for_attempt(1), Duration::from_secs(4));
        assert_eq!(policy.delay_for_attempt(2), Duration::from_secs(8));
        assert_eq!(policy.delay_for_attempt(3), Duration::from_secs(10));
        assert_eq!(policy.delay_for_attempt(100), Duration::from_secs(10));
    }

    #[test]
    fn maximum_is_never_less_than_initial_delay() {
        let policy = RetryPolicy::exponential(Duration::from_secs(5), Duration::from_secs(1));
        assert_eq!(policy.delay_for_attempt(0), Duration::from_secs(5));
        assert_eq!(policy.delay_for_attempt(10), Duration::from_secs(5));
    }
}
