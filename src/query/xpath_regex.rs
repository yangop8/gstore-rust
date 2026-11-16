//! XPath regular expressions for SPARQL `REGEX` and `REPLACE`.
//!
//! SPARQL 1.1 §17.4.3.14 / §17.4.3.15 define `REGEX` and `REPLACE` by XPath
//! `fn:matches` / `fn:replace` (XPath and XQuery Functions and Operators
//! §5.6). This module carries the parts of those definitions the `regex`
//! crate does not (review query#11, sparqlconf#8):
//!
//! * the **flags** string (§5.6.1.1): `s` (dot matches newline), `m`
//!   (multi-line `^` / `$`), `i` (case-insensitive), `x` (whitespace in the
//!   pattern is removed, except inside a character class) and `q` (the
//!   pattern is a literal string; `m`, `s` and `x` then have no effect).
//!   Any other character is an error (FORX0001), so the call is an
//!   evaluation error;
//! * the **replacement string** of `fn:replace` (§5.6.3): `$N` is the text
//!   of capture group N (digits are taken greedily and given back one at a
//!   time while N names no group and is above 9; a group above the pattern's
//!   count up to 9 is the empty string), `\$` a literal `$`, `\\` a
//!   literal `\`; any other `$` or `\` is an error (FORX0004). With the `q`
//!   flag the replacement is used as it is;
//! * `fn:replace`'s rule that a pattern matching the empty string is an
//!   error (FORX0003), so `REPLACE("abc", "", "-")` is an error rather than
//!   `"-a-b-c-"`;
//! * a **per-query cache** of compiled patterns ([`RegexCache`]), so a
//!   `FILTER(REGEX(?o, "…"))` compiles its pattern once per query rather
//!   than once per solution.
//!
//! The pattern syntax itself is handed to the `regex` crate, which accepts
//! the common core of XPath regular expressions. What XPath defines and the
//! crate does not — back-references (`\1`), character-class subtraction
//! (`[a-z-[aeiou]]`) and the `\p{IsBlock}` names — is an invalid pattern
//! (an evaluation error), as before.

use std::collections::HashMap;
use std::rc::Rc;

use regex::{Regex, RegexBuilder};

/// The XPath flags of one `REGEX` / `REPLACE` call (see the module docs).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Flags {
    dot_all: bool,
    multi_line: bool,
    case_insensitive: bool,
    strip_whitespace: bool,
    literal: bool,
}

impl Flags {
    /// Parse an XPath flags string; `None` for a character other than
    /// `s`, `m`, `i`, `x`, `q` (XPath error FORX0001). A flag may repeat.
    pub(crate) fn parse(flags: &str) -> Option<Flags> {
        let mut f = Flags::default();
        for c in flags.chars() {
            match c {
                's' => f.dot_all = true,
                'm' => f.multi_line = true,
                'i' => f.case_insensitive = true,
                'x' => f.strip_whitespace = true,
                'q' => f.literal = true,
                _ => return None,
            }
        }
        Some(f)
    }
}

/// A compiled `REGEX` / `REPLACE` pattern with what the callers need to
/// know about it.
#[derive(Debug)]
pub(crate) struct Compiled {
    pub(crate) re: Regex,
    /// The `q` flag was given: a `REPLACE` replacement is literal too.
    pub(crate) literal: bool,
    /// The pattern matches the empty string, which `REPLACE` rejects
    /// (FORX0003).
    pub(crate) matches_empty: bool,
}

/// Compile `pattern` under the XPath `flags` string; `None` if either is
/// invalid.
pub(crate) fn compile(pattern: &str, flags: &str) -> Option<Compiled> {
    let f = Flags::parse(flags)?;
    let source = if f.literal {
        regex::escape(pattern)
    } else if f.strip_whitespace {
        strip_pattern_whitespace(pattern)
    } else {
        pattern.to_string()
    };
    let re = RegexBuilder::new(&source)
        .case_insensitive(f.case_insensitive)
        // `q` turns `m`, `s` and `x` off (§5.6.1.1).
        .dot_matches_new_line(f.dot_all && !f.literal)
        .multi_line(f.multi_line && !f.literal)
        .build()
        .ok()?;
    let matches_empty = re.is_match("");
    Some(Compiled {
        re,
        literal: f.literal,
        matches_empty,
    })
}

/// The `x` flag: remove the whitespace characters (#x9, #xA, #xD, #x20) of
/// `pattern`, except inside a character class. An escaped character is kept
/// with its backslash.
fn strip_pattern_whitespace(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len());
    let mut class_depth = 0usize;
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                out.push(c);
                if let Some(next) = chars.next() {
                    out.push(next);
                }
            }
            '[' => {
                class_depth += 1;
                out.push(c);
            }
            ']' if class_depth > 0 => {
                class_depth -= 1;
                out.push(c);
            }
            '\t' | '\n' | '\r' | ' ' if class_depth == 0 => {}
            _ => out.push(c),
        }
    }
    out
}

/// One piece of a parsed `fn:replace` replacement string.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Piece {
    Text(String),
    /// The text of capture group N (`$0` is the whole match).
    Group(usize),
}

/// A parsed `fn:replace` replacement string (see the module docs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Replacement(Vec<Piece>);

impl Replacement {
    /// Parse `rep` for a pattern with `groups` capture groups (not counting
    /// the whole match); `literal` (the `q` flag) takes it as it is. `None`
    /// for a `$` not followed by a digit or a `\` not followed by `\` or `$`
    /// (XPath error FORX0004).
    pub(crate) fn parse(rep: &str, groups: usize, literal: bool) -> Option<Replacement> {
        if literal {
            return Some(Replacement(vec![Piece::Text(rep.to_string())]));
        }
        let mut pieces = Vec::new();
        let mut text = String::new();
        let mut chars = rep.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\\' => match chars.next() {
                    Some(e @ ('\\' | '$')) => text.push(e),
                    _ => return None,
                },
                '$' => {
                    let mut digits = String::new();
                    while let Some(&d) = chars.peek().filter(|d| d.is_ascii_digit()) {
                        digits.push(d);
                        chars.next();
                    }
                    if digits.is_empty() {
                        return None;
                    }
                    // §5.6.3: while N names no group and is above 9, its
                    // last digit is a literal character after the reference.
                    let mut tail = String::new();
                    let group = loop {
                        // More digits than fit a u64 are certainly above 9
                        // and above the group count.
                        let n = digits.parse::<u64>().unwrap_or(u64::MAX);
                        if n <= groups as u64 {
                            break Some(n as usize);
                        }
                        if n <= 9 {
                            break None; // an absent group 1..=9: empty text
                        }
                        let last = digits.pop().expect("more than one digit");
                        tail.insert(0, last);
                    };
                    if let Some(g) = group {
                        if !text.is_empty() {
                            pieces.push(Piece::Text(std::mem::take(&mut text)));
                        }
                        pieces.push(Piece::Group(g));
                    }
                    text.push_str(&tail);
                }
                _ => text.push(c),
            }
        }
        if !text.is_empty() {
            pieces.push(Piece::Text(text));
        }
        Some(Replacement(pieces))
    }

    /// Replace every non-overlapping match of `re` in `text` (leftmost
    /// first) by this replacement.
    pub(crate) fn replace_all(&self, re: &Regex, text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut last = 0;
        for caps in re.captures_iter(text) {
            let m = caps.get(0).expect("group 0 is the match");
            out.push_str(&text[last..m.start()]);
            for piece in &self.0 {
                match piece {
                    Piece::Text(t) => out.push_str(t),
                    Piece::Group(g) => {
                        if let Some(c) = caps.get(*g) {
                            out.push_str(c.as_str());
                        }
                    }
                }
            }
            last = m.end();
        }
        out.push_str(&text[last..]);
        out
    }
}

/// Compiled patterns of one query, keyed by (flags, pattern); a pattern or
/// flags string that does not compile is remembered as `None`. Patterns
/// are nearly always constants, so a query compiles each once; one built
/// from the data per solution (`REGEX(?a, ?b)`) is bounded by
/// [`RegexCache::MAX_ENTRIES`]: past it, new patterns are compiled without
/// being kept.
#[derive(Debug, Default)]
pub(crate) struct RegexCache {
    by_flags: HashMap<String, HashMap<String, Option<Rc<Compiled>>>>,
    entries: usize,
    /// Patterns compiled so far (cache misses), for tests and statistics.
    compiled: u64,
}

impl RegexCache {
    /// The most patterns one query keeps compiled.
    pub(crate) const MAX_ENTRIES: usize = 1024;

    /// `pattern` compiled under `flags`, from the cache or compiled now;
    /// `None` if the pattern or the flags are invalid.
    pub(crate) fn get(&mut self, pattern: &str, flags: &str) -> Option<Rc<Compiled>> {
        if let Some(hit) = self.by_flags.get(flags).and_then(|m| m.get(pattern)) {
            return hit.clone();
        }
        self.compiled += 1;
        let compiled = compile(pattern, flags).map(Rc::new);
        if self.entries < Self::MAX_ENTRIES {
            self.entries += 1;
            self.by_flags
                .entry(flags.to_string())
                .or_default()
                .insert(pattern.to_string(), compiled.clone());
        }
        compiled
    }

    /// How many patterns this cache compiled (each distinct (pattern,
    /// flags) once, unless the cache was full).
    pub(crate) fn compiled(&self) -> u64 {
        self.compiled
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matches(text: &str, pattern: &str, flags: &str) -> Option<bool> {
        compile(pattern, flags).map(|c| c.re.is_match(text))
    }

    fn replace(text: &str, pattern: &str, rep: &str, flags: &str) -> Option<String> {
        let c = compile(pattern, flags)?;
        if c.matches_empty {
            return None;
        }
        let r = Replacement::parse(rep, c.re.captures_len() - 1, c.literal)?;
        Some(r.replace_all(&c.re, text))
    }

    #[test]
    fn flags_follow_xpath() {
        assert_eq!(matches("a\nb", "a.b", ""), Some(false));
        assert_eq!(matches("a\nb", "a.b", "s"), Some(true));
        assert_eq!(matches("a\nb", "^b", ""), Some(false));
        assert_eq!(matches("a\nb", "^b", "m"), Some(true));
        assert_eq!(matches("AB", "ab", "i"), Some(true));
        assert_eq!(matches("ab", "a b", ""), Some(false));
        assert_eq!(matches("ab", "a b", "x"), Some(true));
        assert_eq!(
            matches("a b", "a[ ]b", "x"),
            Some(true),
            "class keeps its space"
        );
        assert_eq!(matches("ab", "a[ ]b", "x"), Some(false));
        assert_eq!(matches("abc", ".", "q"), Some(false));
        assert_eq!(matches("a.c", ".", "q"), Some(true));
        assert_eq!(matches("A.C", "a.c", "qi"), Some(true));
        // q turns m / s / x off.
        assert_eq!(matches("a b", "a b", "qx"), Some(true));
        assert_eq!(matches("ab", "a", "z"), None, "FORX0001");
        assert_eq!(matches("ab", "a", "I"), None, "flags are case-sensitive");
        assert_eq!(matches("ab", "a", "ii"), Some(true), "a flag may repeat");
        assert_eq!(matches("ab", "(", ""), None, "invalid pattern");
    }

    #[test]
    fn replacement_strings_follow_xpath() {
        assert_eq!(replace("abc", "(b)", "$1x", "").as_deref(), Some("abxc"));
        assert_eq!(replace("abc", "(b)", "[$1]", "").as_deref(), Some("a[b]c"));
        assert_eq!(replace("abc", "b", "[$0]", "").as_deref(), Some("a[b]c"));
        // An absent group up to 9 is the empty string.
        assert_eq!(replace("abc", "(b)", "$2", "").as_deref(), Some("ac"));
        // Above 9 and above the count: the last digit is literal.
        assert_eq!(replace("abc", "(b)", "$12", "").as_deref(), Some("ab2c"));
        assert_eq!(replace("abc", "(b)", "$123", "").as_deref(), Some("ab23c"));
        assert_eq!(
            replace(
                "abcdefghijkl",
                "(a)(b)(c)(d)(e)(f)(g)(h)(i)(j)(k)",
                "$11$10-",
                ""
            )
            .as_deref(),
            Some("kj-l")
        );
        assert_eq!(replace("abc", "b", "\\$", "").as_deref(), Some("a$c"));
        assert_eq!(replace("abc", "b", "\\\\", "").as_deref(), Some("a\\c"));
        assert_eq!(replace("abc", "b", "$", ""), None, "FORX0004: lone $");
        assert_eq!(
            replace("abc", "b", "$x", ""),
            None,
            "FORX0004: $ not before a digit"
        );
        assert_eq!(replace("abc", "b", "\\n", ""), None, "FORX0004: lone \\");
        // q: the replacement is used as it is.
        assert_eq!(replace("a.c", ".", "$1\\", "q").as_deref(), Some("a$1\\c"));
        // FORX0003: a pattern matching the empty string.
        assert_eq!(replace("abc", "", "-", ""), None);
        assert_eq!(replace("abc", "x*", "-", ""), None);
        assert_eq!(replace("aaa", "a", "b", "").as_deref(), Some("bbb"));
    }

    #[test]
    fn the_cache_compiles_each_pattern_once() {
        let mut cache = RegexCache::default();
        for _ in 0..100 {
            assert!(cache.get("a.c", "").is_some());
            assert!(cache.get("a.c", "i").is_some());
            assert!(cache.get("(", "").is_none());
        }
        assert_eq!(cache.compiled(), 3);
        for i in 0..RegexCache::MAX_ENTRIES + 10 {
            assert!(cache.get(&format!("p{i}"), "").is_some());
        }
        assert_eq!(cache.entries, RegexCache::MAX_ENTRIES);
    }
}
