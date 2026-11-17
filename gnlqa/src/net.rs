//! Shared plumbing for gnlqa's outbound HTTP clients (gStore SPARQL endpoint,
//! LLM providers, embedding service).
//!
//! * **Credentials stay with the configured host** (review cluster-nlqa#15).
//!   ureq follows redirects by default and keeps every header except
//!   `Authorization`/`Cookie` — so Anthropic's `x-api-key` would follow a
//!   `302` to any host the endpoint names. Every credentialed client here
//!   uses an agent that follows no redirects, and a `3xx` reply is an error.

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
