//! The crate-wide error type.

use std::fmt;

/// Convenience alias.
pub type Result<T> = std::result::Result<T, Error>;

/// Everything that can go wrong in gNLQA.
#[derive(Debug)]
pub enum Error {
    /// Missing or invalid configuration (e.g. no API key).
    Config(String),
    /// The LLM call failed or returned an unexpected shape.
    Llm(String),
    /// Talking to the gStore backend failed.
    GStore(String),
    /// A generated/!supplied SPARQL query is invalid.
    Sparql(String),
    /// Transport-level HTTP failure.
    Http(String),
    /// (De)serialization failure.
    Json(String),
    /// A per-request cost limit (LLM calls, KB rows, time) was exhausted; see
    /// [`crate::budget`]. Like [`Error::Http`], it ends the request early
    /// instead of being retried, degraded around or repaired.
    Limit(String),
}

impl Error {
    /// A stable machine-readable code for the error kind (for HTTP / MCP
    /// clients; review cluster-nlqa#18).
    pub fn code(&self) -> &'static str {
        match self {
            Error::Config(_) => "config_error",
            Error::Llm(_) | Error::Json(_) => "llm_error",
            Error::GStore(_) => "kb_error",
            Error::Sparql(_) => "no_answer",
            Error::Http(_) => "upstream_unavailable",
            Error::Limit(_) => "limit_exceeded",
        }
    }

    /// A message that is safe to show a remote client (review
    /// cluster-nlqa#18): internal detail — endpoint URLs, provider error
    /// bodies, gStore error text, configuration — stays in the server log.
    /// Only a [`Error::Limit`] keeps its text: it names the server's own
    /// limits and nothing else.
    pub fn client_message(&self) -> String {
        match self {
            Error::Config(_) => "the server is misconfigured".into(),
            Error::Llm(_) | Error::Json(_) => "the language-model call failed".into(),
            Error::GStore(_) => "the knowledge-base query failed".into(),
            Error::Sparql(_) => "no query could be produced or executed for this question".into(),
            Error::Http(_) => {
                "an upstream service (language model or knowledge base) is unavailable".into()
            }
            Error::Limit(_) => self.to_string(),
        }
    }

    /// Whether this error must end the whole request rather than being
    /// degraded around: a transport outage ([`Error::Http`]) or an exhausted
    /// cost budget ([`Error::Limit`]). Trying the next candidate or another
    /// repair round cannot help with either.
    pub fn is_fatal(&self) -> bool {
        matches!(self, Error::Http(_) | Error::Limit(_))
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Config(m) => write!(f, "config error: {m}"),
            Error::Llm(m) => write!(f, "LLM error: {m}"),
            Error::GStore(m) => write!(f, "gStore error: {m}"),
            Error::Sparql(m) => write!(f, "SPARQL error: {m}"),
            Error::Http(m) => write!(f, "HTTP error: {m}"),
            Error::Json(m) => write!(f, "JSON error: {m}"),
            Error::Limit(m) => write!(f, "limit exceeded: {m}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Json(e.to_string())
    }
}

/// Replace the userinfo (`user:password@`) of every URL in `text` with `***@`,
/// so an endpoint configured with credentials in its URL never reaches an
/// error message or a log line (review cluster-nlqa#18). ureq's transport
/// errors print the full request URL.
pub fn redact_userinfo(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find("://") {
        let (before, after) = rest.split_at(i + 3);
        out.push_str(before);
        let end = after
            .find(|c: char| c == '/' || c == '?' || c == '#' || c.is_whitespace())
            .unwrap_or(after.len());
        let authority = &after[..end];
        match authority.rfind('@') {
            Some(at) => {
                out.push_str("***");
                out.push_str(&authority[at..]);
            }
            None => out.push_str(authority),
        }
        rest = &after[end..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn userinfo_is_redacted_from_every_url() {
        assert_eq!(
            redact_userinfo(
                "gStore endpoint unreachable: http://admin:s3cret@127.0.0.1:9/sparql: refused"
            ),
            "gStore endpoint unreachable: http://***@127.0.0.1:9/sparql: refused"
        );
        assert_eq!(
            redact_userinfo("a https://u@h/x and http://p:q@[::1]:8/ end"),
            "a https://***@h/x and http://***@[::1]:8/ end"
        );
        assert_eq!(
            redact_userinfo("http://host/path?q=a@b"),
            "http://host/path?q=a@b"
        );
        assert_eq!(redact_userinfo("no url here"), "no url here");
        assert_eq!(redact_userinfo("http://u:p@h"), "http://***@h");
    }

    #[test]
    fn client_messages_carry_no_internal_detail() {
        let internal = "http://admin:s3cret@kb:9000/sparql provider body sk-...abcd";
        for e in [
            Error::Config(internal.into()),
            Error::Llm(internal.into()),
            Error::GStore(internal.into()),
            Error::Sparql(internal.into()),
            Error::Http(internal.into()),
            Error::Json(internal.into()),
        ] {
            let m = e.client_message();
            assert!(!m.contains("s3cret") && !m.contains("kb:9000") && !m.contains("abcd"));
            assert!(!e.code().is_empty());
        }
        let limit = Error::Limit("the request needed more than its budget of 3 LLM call(s)".into());
        assert!(limit.client_message().contains("3 LLM call"));
    }
}
