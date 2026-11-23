use std::time::Duration;

use crate::error::LlmError;

pub const DEFAULT_MAX_RETRIES: u32 = 3;
pub const DEFAULT_BASE_DELAY: Duration = Duration::from_millis(500);
pub const DEFAULT_MAX_DELAY: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FailureKind {
    /// An HTTP response with a non-2xx status.
    Api,
    /// Socket/connection failure before or outside a stream body.
    Transport,
    /// Connection attempt failed before the request was accepted.
    Connect,
    /// Read or total timeout.
    Timeout,
    /// A stream that broke after output was already observed.
    StreamInterrupted,
    Cancelled,
    TruncatedToolArguments,
    InvalidResponse,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RetryDecision {
    /// `attempt` failed; wait `delay` and try attempt `attempt + 1`.
    Retry {
        attempt: u32,
        delay: Duration,
    },
    Fail,
}

impl RetryDecision {
    pub fn is_retry(&self) -> bool {
        matches!(self, Self::Retry { .. })
    }

    pub fn delay(&self) -> Option<Duration> {
        match self {
            Self::Retry { delay, .. } => Some(*delay),
            Self::Fail => None,
        }
    }
}

/// Deterministic retry policy. `attempt` is the 1-based number of the attempt
/// that just failed; the policy allows at most `max_retries` extra attempts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RetryPolicy {
    max_retries: u32,
    base_delay: Duration,
    max_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_RETRIES, DEFAULT_BASE_DELAY, DEFAULT_MAX_DELAY)
    }
}

impl RetryPolicy {
    pub const fn new(max_retries: u32, base_delay: Duration, max_delay: Duration) -> Self {
        Self {
            max_retries,
            base_delay,
            max_delay,
        }
    }

    /// Policy with no sleeping, intended for tests and deterministic callers.
    pub const fn immediate(max_retries: u32) -> Self {
        Self::new(max_retries, Duration::ZERO, Duration::ZERO)
    }

    pub const fn max_retries(&self) -> u32 {
        self.max_retries
    }

    pub const fn max_attempts(&self) -> u32 {
        self.max_retries.saturating_add(1)
    }

    pub const fn base_delay(&self) -> Duration {
        self.base_delay
    }

    pub const fn max_delay(&self) -> Duration {
        self.max_delay
    }

    /// Exponential backoff: `base * 2^(attempt - 1)`, capped at `max_delay`.
    pub fn delay_for_attempt(&self, attempt: u32) -> Duration {
        let exponent = attempt.saturating_sub(1);
        let factor = 1u32.checked_shl(exponent).unwrap_or(u32::MAX);
        let millis = self
            .base_delay
            .as_millis()
            .saturating_mul(u128::from(factor));
        let millis = u64::try_from(millis).unwrap_or(u64::MAX);
        Duration::from_millis(millis).min(self.max_delay)
    }

    pub fn classify(&self, attempt: u32, status: Option<u16>, kind: FailureKind) -> RetryDecision {
        let attempt = attempt.max(1);
        if attempt >= self.max_attempts() {
            return RetryDecision::Fail;
        }
        let retryable = match kind {
            FailureKind::Api => matches!(status, Some(408 | 429) | Some(500..=599)),
            FailureKind::Transport | FailureKind::Connect | FailureKind::Timeout => true,
            FailureKind::StreamInterrupted
            | FailureKind::Cancelled
            | FailureKind::TruncatedToolArguments
            | FailureKind::InvalidResponse => false,
        };
        if retryable {
            RetryDecision::Retry {
                attempt,
                delay: self.delay_for_attempt(attempt),
            }
        } else {
            RetryDecision::Fail
        }
    }

    pub fn classify_error(&self, attempt: u32, error: &LlmError) -> RetryDecision {
        self.classify(attempt, error.status(), error.failure_kind())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limit_retries_three_times_then_gives_up() {
        let policy = RetryPolicy::default();
        assert_eq!(
            policy.classify(1, Some(429), FailureKind::Api),
            RetryDecision::Retry {
                attempt: 1,
                delay: Duration::from_millis(500),
            }
        );
        assert!(policy.classify(2, Some(429), FailureKind::Api).is_retry());
        assert!(policy.classify(3, Some(429), FailureKind::Api).is_retry());
        assert_eq!(
            policy.classify(4, Some(429), FailureKind::Api),
            RetryDecision::Fail
        );
        assert_eq!(policy.max_attempts(), 4);
    }

    #[test]
    fn client_errors_never_retry() {
        let policy = RetryPolicy::default();
        for status in [400, 401, 403, 404, 422] {
            assert_eq!(
                policy.classify(1, Some(status), FailureKind::Api),
                RetryDecision::Fail,
                "status {status} must not retry"
            );
        }
    }

    #[test]
    fn server_errors_and_transport_failures_retry() {
        let policy = RetryPolicy::default();
        assert!(policy.classify(1, Some(500), FailureKind::Api).is_retry());
        assert!(policy.classify(1, Some(503), FailureKind::Api).is_retry());
        assert!(policy.classify(1, None, FailureKind::Transport).is_retry());
        assert!(policy.classify(1, None, FailureKind::Timeout).is_retry());
        assert!(policy.classify(1, None, FailureKind::Connect).is_retry());
    }

    #[test]
    fn backoff_is_exponential_and_capped() {
        let policy = RetryPolicy::new(10, Duration::from_millis(500), Duration::from_secs(10));
        assert_eq!(policy.delay_for_attempt(1), Duration::from_millis(500));
        assert_eq!(policy.delay_for_attempt(2), Duration::from_secs(1));
        assert_eq!(policy.delay_for_attempt(3), Duration::from_secs(2));
        assert_eq!(policy.delay_for_attempt(4), Duration::from_secs(4));
        assert_eq!(policy.delay_for_attempt(5), Duration::from_secs(8));
        assert_eq!(policy.delay_for_attempt(6), Duration::from_secs(10));
        assert_eq!(policy.delay_for_attempt(40), Duration::from_secs(10));
    }

    #[test]
    fn cancellation_and_truncation_do_not_retry() {
        let policy = RetryPolicy::default();
        assert_eq!(
            policy.classify(1, None, FailureKind::Cancelled),
            RetryDecision::Fail
        );
        assert_eq!(
            policy.classify(1, None, FailureKind::TruncatedToolArguments),
            RetryDecision::Fail
        );
        assert_eq!(
            policy.classify(1, None, FailureKind::StreamInterrupted),
            RetryDecision::Fail
        );
    }
}
