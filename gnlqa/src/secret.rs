//! A string wrapper that never reveals its contents through `Debug`/`Display`,
//! so API keys and passwords cannot leak into logs, panic messages, or test
//! output. Call [`Secret::expose`] only at the point of use.

use std::fmt;

/// A secret string with a redacted `Debug`/`Display`.
#[derive(Clone)]
pub struct Secret(String);

impl Secret {
    pub fn new(s: impl Into<String>) -> Secret {
        Secret(s.into())
    }
    /// The raw value — call only where the secret is actually consumed
    /// (e.g. building an HTTP auth header).
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret([REDACTED])")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_and_display_are_redacted() {
        let s = Secret::new("sk-very-secret");
        assert!(!format!("{s:?}").contains("secret"));
        assert!(!format!("{s}").contains("secret"));
        assert_eq!(s.expose(), "sk-very-secret");
    }
}
