//! Biller resilience policy. Timeout is a *biller* choice; payer latency is a
//! payer property; a "timeout fault" is only ever the emergent interaction of
//! the two. This is the seam a second resilience implementation would vary on.

use std::time::Duration;

#[derive(Clone, Copy, Debug)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    /// How long to wait for a remittance before treating silence as failure.
    pub timeout: Duration,
    /// Base of the exponential backoff between retries (fault 2.1).
    pub backoff_base: Duration,
}

impl RetryPolicy {
    /// Wait before submitting `attempt`: nothing for the first attempt, then
    /// exponential — base, 2×base, 4×base, …
    pub fn backoff(&self, attempt: u32) -> Duration {
        match attempt {
            0 | 1 => Duration::ZERO,
            n => self.backoff_base * 2u32.pow(n - 2),
        }
    }
}

impl Default for RetryPolicy {
    fn default() -> RetryPolicy {
        // Timeout comfortably above the slowest honest payer (anthem, 30s), so
        // the zero-fault path never times out.
        RetryPolicy {
            max_attempts: 3,
            timeout: Duration::from_secs(120),
            backoff_base: Duration::from_secs(15),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_is_zero_then_exponential() {
        let policy = RetryPolicy {
            backoff_base: Duration::from_secs(10),
            ..RetryPolicy::default()
        };
        assert_eq!(policy.backoff(1), Duration::ZERO);
        assert_eq!(policy.backoff(2), Duration::from_secs(10));
        assert_eq!(policy.backoff(3), Duration::from_secs(20));
        assert_eq!(policy.backoff(4), Duration::from_secs(40));
    }
}
