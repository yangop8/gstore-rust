//! Base64 decoding (RFC 4648): one decoder, with the rules each caller
//! needs (review gapi#15, robust#16, design#13: the HTTP Basic-auth and the
//! PFN module decoders were two copies with different rules).
//!
//! * [`BASIC_AUTH`] — the `Authorization: Basic` token
//!   ([`crate::server`], [`crate::preauth`]): the standard alphabet, ASCII
//!   whitespace skipped, decoding stops at the first `=` (what follows is
//!   ignored), a final partial group is ignored;
//! * [`LENIENT`] — a `wasm-base64:` PFN module ([`crate::pfn::base64_decode`]):
//!   the standard and the URL-safe alphabets, padding optional, a space read
//!   as `+` (what URL-decoding a `+` gives), `\n`, `\r` and `\t` skipped,
//!   nothing but padding and whitespace after the padding, and a truncated
//!   final group refused.

use std::fmt;

/// How [`decode`] reads its input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rules {
    /// `-` and `_` are the digits 62 and 63 too (RFC 4648's URL-safe
    /// alphabet).
    pub url_safe: bool,
    /// A space is the digit 62 (`+`); otherwise it is skipped as
    /// whitespace.
    pub space_is_plus: bool,
    /// The first `=` ends the input (the rest is ignored); otherwise only
    /// `=` and whitespace may follow it.
    pub stop_at_padding: bool,
    /// A final group of a single digit (6 bits: no whole byte) is an
    /// error; otherwise it is ignored.
    pub refuse_truncated: bool,
}

/// The rules of an HTTP Basic-auth token (see the module docs).
pub const BASIC_AUTH: Rules = Rules {
    url_safe: false,
    space_is_plus: false,
    stop_at_padding: true,
    refuse_truncated: false,
};

/// The rules of a PFN WebAssembly module (see the module docs).
pub const LENIENT: Rules = Rules {
    url_safe: true,
    space_is_plus: true,
    stop_at_padding: false,
    refuse_truncated: true,
};

/// Why [`decode`] refused its input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Base64Error {
    /// A byte that is no digit of the alphabet, at this offset.
    InvalidCharacter(usize),
    /// A digit after the padding, at this offset.
    DataAfterPadding(usize),
    /// The input ends inside a byte.
    Truncated,
}

impl fmt::Display for Base64Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Base64Error::InvalidCharacter(i) => write!(f, "invalid base64 character at offset {i}"),
            Base64Error::DataAfterPadding(i) => {
                write!(f, "base64 data after padding at offset {i}")
            }
            Base64Error::Truncated => f.write_str("truncated base64 data"),
        }
    }
}

impl std::error::Error for Base64Error {}

/// Decode `s` under `rules`.
pub fn decode(s: &str, rules: Rules) -> Result<Vec<u8>, Base64Error> {
    let mut out = Vec::with_capacity(s.len() / 4 * 3 + 3);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    let mut padding = false;
    for (i, c) in s.bytes().enumerate() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'-' if rules.url_safe => 62,
            b'_' if rules.url_safe => 63,
            b' ' if rules.space_is_plus => 62,
            b'=' if rules.stop_at_padding => break,
            b'=' => {
                padding = true;
                continue;
            }
            b'\r' | b'\n' | b'\t' | b' ' => continue,
            _ => return Err(Base64Error::InvalidCharacter(i)),
        };
        if padding {
            return Err(Base64Error::DataAfterPadding(i));
        }
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    if rules.refuse_truncated && bits >= 6 {
        return Err(Base64Error::Truncated);
    }
    Ok(out)
}
