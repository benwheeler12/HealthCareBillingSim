//! Biller resilience policy. Timeout is a *biller* choice; payer latency is a
//! payer property; a "timeout fault" is only ever the emergent interaction of
//! the two. This is the seam a second resilience implementation would vary on.

use std::time::Duration;

#[derive(Clone, Copy, Debug)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    /// How long to wait for a remittance before treating silence as failure.
    pub timeout: Duration,
}

impl Default for RetryPolicy {
    fn default() -> RetryPolicy {
        // Timeout comfortably above the slowest honest payer (anthem, 30s), so
        // the zero-fault path never times out; backoff arrives with fault 2.1.
        RetryPolicy {
            max_attempts: 3,
            timeout: Duration::from_secs(120),
        }
    }
}
