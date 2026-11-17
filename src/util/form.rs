//! `application/x-www-form-urlencoded` decoding (also what a URL query
//! string holds): `&`-separated `name=value` pairs, `+` a space, `%XX` an
//! escaped byte. The one implementation behind the REST endpoints
//! ([`crate::server`]), the `/api` layer ([`crate::gapi::params`]) and PFN
//! ([`crate::pfn::url_decode`]) — review gapi#15, robust#16, design#13.
//!
//! The rules, the same everywhere:
//!
//! * `+` decodes to a space; `%` followed by two hex digits (either case)
//!   to that byte; a `%` not followed by two hex digits is kept as is (as is
//!   whatever follows it) — never an error;
//! * the decoded bytes are read as UTF-8, invalid sequences replaced by
//!   U+FFFD;
//! * names are decoded like values.
//!
//! The two ways of reading a form differ only where a caller needs them to:
//! [`parse_form`] lists every pair (a name without `=` has an empty value,
//! an empty name is dropped) — the `/api` layer merges them, the last
//! occurrence winning; [`form_lookup`] finds the first pair with a given
//! name and its `=` (a bare name is not a value) — the REST endpoints ask
//! for one parameter at a time, without decoding the rest of the form.

/// Percent-decode `s` (see the module docs).
pub fn url_decode(s: &str) -> String {
    url_decode_bytes(s.as_bytes())
}

/// Percent-decode the bytes `b` (see the module docs).
pub fn url_decode_bytes(b: &[u8]) -> String {
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let (c, step) = decode_at(b, i);
        out.push(c);
        i += step;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Every `name=value` pair of the form `s`, decoded, in order. A pair
/// without `=` is a name with an empty value; empty pairs and pairs whose
/// name decodes to nothing are skipped.
pub fn parse_form(s: &str) -> Vec<(String, String)> {
    s.split('&')
        .filter(|pair| !pair.is_empty())
        .filter_map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            let k = url_decode(k);
            (!k.is_empty()).then(|| (k, url_decode(v)))
        })
        .collect()
}

/// The decoded value of the first pair of `form` whose decoded name is
/// `key`; pairs without `=` are skipped. One pass, allocating only the
/// value found: names are compared while they are decoded.
pub fn form_lookup(form: &[u8], key: &str) -> Option<String> {
    form.split(|&b| b == b'&').find_map(|pair| {
        let eq = pair.iter().position(|&b| b == b'=')?;
        decoded_eq(&pair[..eq], key.as_bytes()).then(|| url_decode_bytes(&pair[eq + 1..]))
    })
}

/// Whether the decoding of `enc` is exactly `want`.
fn decoded_eq(enc: &[u8], want: &[u8]) -> bool {
    let (mut i, mut j) = (0, 0);
    while i < enc.len() {
        let (b, step) = decode_at(enc, i);
        if want.get(j) != Some(&b) {
            return false;
        }
        i += step;
        j += 1;
    }
    j == want.len()
}

/// The byte the encoding `b` holds at `i` (`+` a space, `%XX` an escape, a
/// malformed `%` itself) and how many input bytes it took.
fn decode_at(b: &[u8], i: usize) -> (u8, usize) {
    match b[i] {
        b'+' => (b' ', 1),
        b'%' if i + 2 < b.len() => match (hex(b[i + 1]), hex(b[i + 2])) {
            (Some(h), Some(l)) => ((h << 4) | l, 3),
            _ => (b'%', 1),
        },
        c => (c, 1),
    }
}

/// The value of one hex digit (either case).
pub(crate) fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}
