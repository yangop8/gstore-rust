//! Shared plumbing for gnlqa's outbound HTTP clients (gStore SPARQL endpoint,
//! LLM providers, embedding service).
//!
//! * **Credentials stay with the configured host** (review cluster-nlqa#15).
//!   ureq follows redirects by default and keeps every header except
//!   `Authorization`/`Cookie` — so Anthropic's `x-api-key` would follow a
//!   `302` to any host the endpoint names. Every credentialed client here
//!   uses an agent that follows no redirects, and a `3xx` reply is an error.
//! * **Transient provider errors are retried** (review cluster-nlqa#16):
//!   `429` and `5xx` answers get a bounded number of retries with exponential
//!   back-off that honours `Retry-After` and never sleeps past the current
//!   request's deadline ([`crate::budget`]).

use std::time::Duration;

use crate::error::{Error, Result};

/// A ureq agent for credentialed requests: redirects are never followed.
pub(crate) fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new().redirects(0).build()
}

/// With redirects disabled ureq hands a `3xx` back as a successful response;
/// turn it into an error so a redirecting endpoint never receives a retry
/// with the credential and the caller sees why the call failed.
pub(crate) fn refuse_redirect(resp: ureq::Response, what: &str) -> Result<ureq::Response> {
    let code = resp.status();
    if (300..400).contains(&code) {
        return Err(Error::Http(format!(
            "{what} answered with a redirect (HTTP {code}); redirects are not followed, so \
             credentials are only ever sent to the configured host — configure the final URL"
        )));
    }
    Ok(resp)
}

/// How a client retries transient HTTP failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Retries after the first attempt (0 = never retry).
    pub max_retries: u32,
    /// Back-off before the first retry; doubled for each further one.
    pub base_backoff: Duration,
    /// Upper bound on any single wait (also caps `Retry-After`).
    pub max_backoff: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        RetryPolicy {
            max_retries: 3,
            base_backoff: Duration::from_millis(500),
            max_backoff: Duration::from_secs(30),
        }
    }
}

impl RetryPolicy {
    /// Never retry.
    pub fn none() -> RetryPolicy {
        RetryPolicy {
            max_retries: 0,
            ..RetryPolicy::default()
        }
    }

    fn backoff(&self, attempt: u32) -> Duration {
        let factor = 1u32.checked_shl(attempt.min(16)).unwrap_or(u32::MAX);
        self.base_backoff
            .saturating_mul(factor)
            .min(self.max_backoff)
    }
}

/// `Retry-After` in its delta-seconds form (the HTTP-date form is ignored and
/// falls back to the computed back-off).
fn retry_after(resp: &ureq::Response) -> Option<Duration> {
    resp.header("retry-after")?
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
}

/// Send with `send`, retrying an HTTP status in `retryable` per `policy`.
/// Transport errors and other statuses are returned at once. Each wait is
/// logged; a wait that would outlast the current request's time budget is
/// not taken (the last error is returned instead).
pub(crate) fn send_with_retry(
    policy: &RetryPolicy,
    what: &str,
    retryable: &[u16],
    mut send: impl FnMut() -> std::result::Result<ureq::Response, ureq::Error>,
) -> std::result::Result<ureq::Response, ureq::Error> {
    let mut attempt = 0u32;
    loop {
        match send() {
            Err(ureq::Error::Status(code, resp))
                if retryable.contains(&code) && attempt < policy.max_retries =>
            {
                let wait = retry_after(&resp)
                    .unwrap_or_else(|| policy.backoff(attempt))
                    .min(policy.max_backoff);
                if crate::budget::remaining_time().is_some_and(|left| wait >= left) {
                    return Err(ureq::Error::Status(code, resp));
                }
                attempt += 1;
                eprintln!(
                    "gnlqa: {what} answered HTTP {code}; retry {attempt}/{} in {} ms",
                    policy.max_retries,
                    wait.as_millis()
                );
                std::thread::sleep(wait);
            }
            other => return other,
        }
    }
}

/// Statuses an LLM / embedding provider uses for transient overload or
/// outage (`529` is Anthropic's "overloaded").
pub(crate) const PROVIDER_RETRYABLE: &[u16] = &[429, 500, 502, 503, 504, 529];

/// Statuses a gStore endpoint uses for transient conditions. `500` is left
/// out: gStore reports query-evaluation failures with it, and retrying a
/// deterministic failure only wastes time.
pub(crate) const KB_RETRYABLE: &[u16] = &[429, 502, 503, 504];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_and_is_capped() {
        let p = RetryPolicy {
            max_retries: 10,
            base_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_secs(1),
        };
        assert_eq!(p.backoff(0), Duration::from_millis(100));
        assert_eq!(p.backoff(1), Duration::from_millis(200));
        assert_eq!(p.backoff(3), Duration::from_millis(800));
        assert_eq!(p.backoff(4), Duration::from_secs(1));
        assert_eq!(p.backoff(40), Duration::from_secs(1));
    }
}
