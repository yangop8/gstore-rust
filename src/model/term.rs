//! RDF terms and triples.
//!
//! gStore's `Triple.h` models a triple as three raw strings plus an
//! `ObjectType {None, Entity, Literal}` flag. The Rust rewrite uses a proper
//! [`Term`] enum so that "is this an IRI or a literal" is a type-level fact
//! rather than a convention scattered across the codebase.

use std::fmt;

use serde::{Deserialize, Serialize};

/// An RDF term: the value at a subject/predicate/object position.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Term {
    /// An IRI reference, stored *without* the angle brackets, e.g. `http://ex/a`.
    Iri(String),
    /// A literal: lexical value plus optional datatype IRI or language tag.
    /// `datatype` and `lang` are mutually exclusive in RDF; we don't enforce it
    /// structurally but the parser never sets both.
    Literal {
        value: String,
        datatype: Option<String>,
        lang: Option<String>,
    },
    /// A blank node, stored without the `_:` prefix.
    Blank(String),
}

impl Term {
    /// Construct an IRI term.
    pub fn iri(s: impl Into<String>) -> Term {
        Term::Iri(s.into())
    }

    /// Construct a plain (untyped, no-lang) literal.
    pub fn plain_literal(s: impl Into<String>) -> Term {
        Term::Literal {
            value: s.into(),
            datatype: None,
            lang: None,
        }
    }

    /// Construct a typed literal.
    pub fn typed_literal(value: impl Into<String>, datatype: impl Into<String>) -> Term {
        Term::Literal {
            value: value.into(),
            datatype: Some(datatype.into()),
            lang: None,
        }
    }

    /// A blank node, e.g. label `b0` for `_:b0`.
    pub fn blank(s: impl Into<String>) -> Term {
        Term::Blank(s.into())
    }

    /// Is this term a literal? (Entities = IRI or blank node.)
    pub fn is_literal(&self) -> bool {
        matches!(self, Term::Literal { .. })
    }

    /// Is this term an entity (IRI or blank node)?
    pub fn is_entity(&self) -> bool {
        matches!(self, Term::Iri(_) | Term::Blank(_))
    }

    /// The canonical string key used by the dictionary. This is the *exact*
    /// surface syntax gStore stores: `<iri>`, `_:blank`, or the literal with its
    /// quotes and `^^<dt>` / `@lang` suffix. Keying on this round-trips losslessly.
    ///
    /// A key is not an N-Triples serialization: an IRI is written verbatim
    /// (one a lenient reader let in may hold `>` or a newline), so output
    /// meant for another parser goes through [`to_ntriples`](Self::to_ntriples).
    pub fn dict_key(&self) -> String {
        self.to_string()
    }

    /// This term in N-Triples syntax, safe to embed in an N-Triples line
    /// whatever the term holds: the one serializer of every N-Triples and
    /// Turtle-term output (CONSTRUCT / DESCRIBE responses, result tables,
    /// exports). Unlike [`dict_key`](Self::dict_key) (the stored form) it
    /// escapes what IRIREF excludes in an IRI or a datatype IRI as `\uXXXX`,
    /// and a literal's control characters (`\n`, `\r`, `\t`, `\"`, `\\`,
    /// the rest as `\uXXXX`), so no term can end its line or forge a
    /// statement; a blank-node label outside BLANK_NODE_LABEL (a database
    /// written before the readers validated labels) is replaced by
    /// [`safe_blank_label`], and such a language tag by
    /// [`output_lang_tag`]'s reading of it. For every term the readers
    /// produce, [`crate::parser::ntriples::parse_term`] reads the result back
    /// as this very term.
    pub fn to_ntriples(&self) -> String {
        let mut out = String::with_capacity(self.heap_bytes() + 8);
        self.write_ntriples(&mut out);
        out
    }

    /// Append [`to_ntriples`](Self::to_ntriples) to `out`.
    pub fn write_ntriples(&self, out: &mut String) {
        match self {
            Term::Iri(iri) => {
                out.push('<');
                escape_iriref(iri, out);
                out.push('>');
            }
            Term::Blank(label) => {
                out.push_str("_:");
                out.push_str(&safe_blank_label(label));
            }
            Term::Literal {
                value,
                datatype,
                lang,
            } => {
                out.push('"');
                escape_literal_into(value, out, true);
                out.push('"');
                if let Some(dt) = datatype {
                    out.push_str("^^<");
                    escape_iriref(dt, out);
                    out.push('>');
                } else if let Some(tag) = lang.as_deref().and_then(output_lang_tag) {
                    out.push('@');
                    out.push_str(&tag);
                }
            }
        }
    }

    /// The term a dictionary key names — the exact inverse of
    /// [`dict_key`](Self::dict_key), for every term a reader may have let
    /// in. Unlike the N-Triples reader it takes an IRI's text verbatim (one
    /// holding `>`, `\` or a space, which the lenient readers accept, reads
    /// back as itself; `\u` is not an escape there) and an empty language
    /// tag. `None` for a string that is not a key.
    pub fn from_dict_key(key: &str) -> Option<Term> {
        if let Some(iri) = key.strip_prefix('<').and_then(|k| k.strip_suffix('>')) {
            return Some(Term::Iri(iri.to_string()));
        }
        if let Some(label) = key.strip_prefix("_:") {
            return Some(Term::Blank(label.to_string()));
        }
        let body = key.strip_prefix('"')?;
        // The lexical form, escaped as `escape_literal` escapes it.
        let mut value = String::with_capacity(body.len());
        let mut chars = body.char_indices();
        let rest = loop {
            match chars.next()? {
                (i, '"') => break &body[i + 1..],
                (_, '\\') => value.push(match chars.next()?.1 {
                    '\\' => '\\',
                    '"' => '"',
                    'n' => '\n',
                    'r' => '\r',
                    't' => '\t',
                    _ => return None,
                }),
                (_, c) => value.push(c),
            }
        };
        let (datatype, lang) = if rest.is_empty() {
            (None, None)
        } else if let Some(dt) = rest.strip_prefix("^^<").and_then(|r| r.strip_suffix('>')) {
            (Some(dt.to_string()), None)
        } else {
            (None, Some(rest.strip_prefix('@')?.to_string()))
        };
        Some(Term::Literal {
            value,
            datatype,
            lang,
        })
    }

    /// The term a redo / incremental-backup record names
    /// ([`record_key`] form): [`from_dict_key`](Self::from_dict_key), with
    /// the `\uXXXX` / `\UXXXXXXXX` escapes of an IRI, a datatype IRI, and
    /// a blank-node label or language tag holding a `\` decoded. Exact for
    /// every stored term, a label or tag the strict readers refuse
    /// included. `None` for a string that is not such a key.
    pub fn from_record_key(key: &str) -> Option<Term> {
        match Term::from_dict_key(key)? {
            Term::Iri(iri) => Some(Term::Iri(unescape_iriref(&iri)?)),
            Term::Blank(label) => Some(Term::Blank(unescape_name(label))),
            Term::Literal {
                value,
                datatype: Some(dt),
                lang,
            } => Some(Term::Literal {
                value,
                datatype: Some(unescape_iriref(&dt)?),
                lang,
            }),
            Term::Literal {
                value,
                datatype: None,
                lang,
            } => Some(Term::Literal {
                value,
                datatype: None,
                lang: lang.map(unescape_name),
            }),
        }
    }

    /// The bytes this term's strings hold (what a parsed statement costs in
    /// memory, short of the fixed size of the term itself).
    pub(crate) fn heap_bytes(&self) -> usize {
        match self {
            Term::Iri(s) | Term::Blank(s) => s.len(),
            Term::Literal {
                value,
                datatype,
                lang,
            } => {
                value.len()
                    + datatype.as_ref().map_or(0, String::len)
                    + lang.as_ref().map_or(0, String::len)
            }
        }
    }
}

impl fmt::Display for Term {
    /// Render a term in its dictionary-key form ([`Term::dict_key`]): the
    /// stored surface syntax, which is N-Triples for well-formed terms but
    /// writes an IRI verbatim. Output for other parsers uses
    /// [`Term::to_ntriples`].
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Term::Iri(s) => write!(f, "<{s}>"),
            Term::Blank(s) => write!(f, "_:{s}"),
            Term::Literal {
                value,
                datatype,
                lang,
            } => {
                write!(f, "\"{}\"", escape_literal(value))?;
                if let Some(dt) = datatype {
                    write!(f, "^^<{dt}>")?;
                } else if let Some(lang) = lang {
                    write!(f, "@{lang}")?;
                }
                Ok(())
            }
        }
    }
}

/// Escape a literal's lexical value as a dictionary key holds it (`"`, `\`,
/// newline, carriage return and tab; see [`Term::dict_key`]).
pub(crate) fn escape_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    escape_literal_into(s, &mut out, false);
    out
}

/// Append `s` escaped as [`escape_literal`] does; with `controls`, also
/// every other control character (U+0000-U+001F, U+007F) as `\uXXXX`, as
/// N-Triples output writes them ([`Term::to_ntriples`]).
fn escape_literal_into(s: &str, out: &mut String, controls: bool) {
    use std::fmt::Write as _;
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if controls && (c < ' ' || c == '\u{7f}') => {
                let _ = write!(out, "\\u{:04X}", u32::from(c));
            }
            _ => out.push(c),
        }
    }
}

/// A character IRIREF excludes (`[^#x00-#x20<>"{}|^`\\]`).
fn excluded_from_iriref(c: char) -> bool {
    c <= ' ' || matches!(c, '<' | '>' | '"' | '{' | '}' | '|' | '^' | '`' | '\\')
}

// ---- the shared term validator ---------------------------------------------
//
// One set of rules for what a term may hold, used by every reader (RDF/XML,
// JSON-LD, RDFa, N-Triples, N-Quads, Turtle), the SPARQL lexer and the term
// built-ins (`IRI()`, `STRLANG()`, `STRDT()`), so every stored term reads
// back through the strict N-Triples reader (review parser#1, sparqlconf#11).

/// Whether `tag` is an RDF language tag: the `LANGTAG` production of
/// N-Triples / Turtle / SPARQL without its `@`, `[a-zA-Z]+(-[a-zA-Z0-9]+)*`
/// (BCP 47's shape; an empty tag is not one).
pub fn is_valid_lang_tag(tag: &str) -> bool {
    let mut parts = tag.split('-');
    let first = parts.next().unwrap_or("");
    !first.is_empty()
        && first.bytes().all(|b| b.is_ascii_alphabetic())
        && parts.all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_alphanumeric()))
}

/// A language tag written as an attribute or string value (RDF/XML
/// `xml:lang`, JSON-LD `@language`, RDFa `lang` / `xml:lang`), as the
/// readers store it: surrounding whitespace is dropped, `_` reads as `-`
/// (`zh_CN` is `zh-CN`, as POSIX locales write it), and an empty value means
/// no language (`Ok(None)`, as those formats define it). Anything else that
/// is not a [valid tag](is_valid_lang_tag) is refused with a message naming
/// it, which the reader reports as a syntax error.
pub fn normalize_lang_tag(raw: &str) -> std::result::Result<Option<String>, String> {
    let tag = raw.trim();
    if tag.is_empty() {
        return Ok(None);
    }
    let tag = tag.replace('_', "-");
    if is_valid_lang_tag(&tag) {
        Ok(Some(tag))
    } else {
        Err(format!(
            "invalid language tag {raw:?} (expected letters, then '-'-separated letters or \
             digits, e.g. en-GB)"
        ))
    }
}

/// The language tag N-Triples output writes for a stored `tag`: the tag
/// itself when [valid](is_valid_lang_tag) (every tag the readers store
/// today); for one an older version stored unchecked, its letters and
/// digits with every other run of characters read as one `-` (`zh_CN` →
/// `zh-CN`, `en US` → `en-US`), or no tag at all (`None`) when that is
/// still not a tag — never text that could end the term.
pub fn output_lang_tag(tag: &str) -> Option<std::borrow::Cow<'_, str>> {
    use std::borrow::Cow;
    if is_valid_lang_tag(tag) {
        return Some(Cow::Borrowed(tag));
    }
    let fixed = tag
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    is_valid_lang_tag(&fixed).then_some(Cow::Owned(fixed))
}

/// Whether `c` may appear in a blank-node label, as every reader accepts it
/// (BLANK_NODE_LABEL's name characters: letters and digits, `_` and `-`).
/// A `.` may also appear inside a label, never at its end; see
/// [`is_valid_blank_label`].
pub fn is_blank_label_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '-'
}

/// Whether `label` (without its `_:`) is a blank-node label every reader
/// reads back: non-empty, [label characters](is_blank_label_char) and inner
/// dots, not ending in `.`.
pub fn is_valid_blank_label(label: &str) -> bool {
    !label.is_empty()
        && !label.ends_with('.')
        && label.chars().all(|c| c == '.' || is_blank_label_char(c))
}

/// `label` as a [valid label](is_valid_blank_label): itself when valid,
/// else (a JSON-LD `"_:a b"`, an RDF/XML `rdf:nodeID="g."`, an RDFa
/// `_:` CURIE holding a control character) `h<16 hex digits>-<label>` — the
/// hex digits a stable hash of the whole label, then the label with every
/// character outside the label set as `_` — so equal labels stay one node
/// and different ones (`a b`, `a_b`) stay apart.
pub fn safe_blank_label(label: &str) -> std::borrow::Cow<'_, str> {
    use std::borrow::Cow;
    if is_valid_blank_label(label) {
        return Cow::Borrowed(label);
    }
    let mut out = format!("h{:016x}-", fnv1a64(label.as_bytes()));
    out.extend(
        label
            .chars()
            .map(|c| if is_blank_label_char(c) { c } else { '_' }),
    );
    Cow::Owned(out)
}

/// Whether `iri` (an IRI's text, without brackets) holds no character
/// IRIREF excludes (a space, a control character, `<>"{}|^`\`): what
/// `IRI()` / `URI()` and `STRDT()` accept from a string.
pub fn is_valid_iriref(iri: &str) -> bool {
    !iri.chars().any(excluded_from_iriref)
}

/// 64-bit FNV-1a: a hash that is the same in every process and release.
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// A dictionary key (a term in its stored form, see [`Term::dict_key`]) as
/// N-Triples / Turtle / N-Quads output writes it: the key's term through
/// [`Term::to_ntriples`], so an export loads back. A key that needs no
/// change (every key of a term the readers produce today, short of an IRI
/// holding a character IRIREF excludes, which the lenient readers let in)
/// is returned as is, without parsing it; so is a string that is not a key.
pub fn export_key(key: &str) -> std::borrow::Cow<'_, str> {
    use std::borrow::Cow;
    if key_is_ntriples(key) {
        return Cow::Borrowed(key);
    }
    match Term::from_dict_key(key) {
        Some(t) => Cow::Owned(t.to_ntriples()),
        None => Cow::Borrowed(key),
    }
}

/// Whether the dictionary key `key` already is its term's
/// [`Term::to_ntriples`] form (or is no key at all): one pass, no
/// allocation.
fn key_is_ntriples(key: &str) -> bool {
    if let Some(inner) = key.strip_prefix('<').and_then(|k| k.strip_suffix('>')) {
        return is_valid_iriref(inner);
    }
    if let Some(label) = key.strip_prefix("_:") {
        return is_valid_blank_label(label);
    }
    let Some(body) = key.strip_prefix('"') else {
        return true;
    };
    let mut chars = body.char_indices();
    let rest = loop {
        match chars.next() {
            None => return true,
            Some((i, '"')) => break &body[i + 1..],
            Some((_, '\\')) => {
                chars.next();
            }
            Some((_, c)) if c < ' ' || c == '\u{7f}' => return false,
            Some(_) => {}
        }
    };
    if rest.is_empty() {
        true
    } else if let Some(dt) = rest.strip_prefix("^^<").and_then(|r| r.strip_suffix('>')) {
        is_valid_iriref(dt)
    } else if let Some(tag) = rest.strip_prefix('@') {
        is_valid_lang_tag(tag)
    } else {
        true
    }
}

/// A dictionary key as a redo-log / incremental-backup record writes it:
/// exact, so the record names the stored term whatever it holds, and free
/// of raw spaces outside a literal's quotes, so a record splits into its
/// terms unambiguously. In an IRI or a literal's datatype IRI every
/// character IRIREF excludes becomes a `\uXXXX` escape (as N-Triples writes
/// it); a blank-node label or language tag the strict readers would refuse
/// (one an older version stored unchecked: `zh_CN`, `a b`) has every
/// character outside the label / tag set — `\` included — as a `\uXXXX` /
/// `\UXXXXXXXX` escape. A key that is valid N-Triples — what every reader
/// stores today — is written unchanged, as before. Read back with
/// [`Term::from_record_key`] / [`parse_record_triple`].
pub fn record_key(key: &str) -> std::borrow::Cow<'_, str> {
    use std::borrow::Cow;
    let needs_change = if let Some(inner) = key.strip_prefix('<').and_then(|k| k.strip_suffix('>'))
    {
        !is_valid_iriref(inner)
    } else if let Some(label) = key.strip_prefix("_:") {
        !is_valid_blank_label(label)
    } else {
        key.starts_with('"') && !literal_suffix_is_ntriples(key)
    };
    if !needs_change {
        return Cow::Borrowed(key);
    }
    let Some(term) = Term::from_dict_key(key) else {
        return Cow::Borrowed(key);
    };
    let mut out = String::with_capacity(key.len() + 16);
    match &term {
        Term::Iri(iri) => {
            out.push('<');
            escape_iriref(iri, &mut out);
            out.push('>');
        }
        Term::Blank(label) => {
            out.push_str("_:");
            escape_name(label, is_blank_label_char, &mut out);
        }
        Term::Literal {
            value,
            datatype,
            lang,
        } => {
            out.push('"');
            escape_literal_into(value, &mut out, false);
            out.push('"');
            if let Some(dt) = datatype {
                out.push_str("^^<");
                escape_iriref(dt, &mut out);
                out.push('>');
            } else if let Some(tag) = lang {
                out.push('@');
                if is_valid_lang_tag(tag) {
                    out.push_str(tag);
                } else {
                    escape_name(tag, |c| c.is_ascii_alphanumeric() || c == '-', &mut out);
                }
            }
        }
    }
    Cow::Owned(out)
}

/// Whether the datatype IRI or language tag after a literal key's closing
/// quote is as N-Triples writes it (no suffix: yes).
fn literal_suffix_is_ntriples(key: &str) -> bool {
    let Some(body) = key.strip_prefix('"') else {
        return true;
    };
    let mut chars = body.char_indices();
    let rest = loop {
        match chars.next() {
            None => return true,
            Some((i, '"')) => break &body[i + 1..],
            Some((_, '\\')) => {
                chars.next();
            }
            Some(_) => {}
        }
    };
    if let Some(dt) = rest.strip_prefix("^^<").and_then(|r| r.strip_suffix('>')) {
        is_valid_iriref(dt)
    } else if let Some(tag) = rest.strip_prefix('@') {
        is_valid_lang_tag(tag)
    } else {
        true
    }
}

/// Append `name` (a blank-node label or language tag) with every character
/// `keep` refuses as a `\uXXXX` / `\UXXXXXXXX` escape (a `\` is never kept).
fn escape_name(name: &str, keep: impl Fn(char) -> bool, out: &mut String) {
    use std::fmt::Write as _;
    for c in name.chars() {
        if keep(c) && c != '\\' {
            out.push(c);
        } else if u32::from(c) <= 0xFFFF {
            let _ = write!(out, "\\u{:04X}", u32::from(c));
        } else {
            let _ = write!(out, "\\U{:08X}", u32::from(c));
        }
    }
}

/// A blank-node label or language tag of a record: escapes decoded when it
/// holds a `\` (no valid label or tag does); kept as written when that
/// fails (a record an older version wrote with the name raw).
fn unescape_name(name: String) -> String {
    if !name.contains('\\') {
        return name;
    }
    unescape_iriref(&name).unwrap_or(name)
}

/// One redo / incremental-backup record, `S P O .` with each term in its
/// [`record_key`] form, as the triple it names: exactly the stored terms.
/// The subject and predicate hold no raw space, so the object runs from
/// the second space to the record's final ` .`. A record an older version
/// wrote with a raw space in a blank subject's label is split at the first
/// ` <` that leaves a predicate IRI and an object. `None` when the record
/// does not have this shape.
pub fn parse_record_triple(record: &str) -> Option<Triple> {
    let body = record.strip_suffix(" .")?;
    // The predicate and object after the subject: `<p> O`.
    let rest_of = |rest: &str| -> Option<(Term, Term)> {
        let end = rest.strip_prefix('<')?.find('>')? + 1;
        let p = Term::from_record_key(&rest[..=end])?;
        let o = Term::from_record_key(rest[end + 1..].strip_prefix(' ')?)?;
        matches!(p, Term::Iri(_)).then_some((p, o))
    };
    let subject_end = if body.starts_with('<') {
        body.find('>')? + 1
    } else if body.starts_with("_:") {
        body.find(' ')?
    } else {
        return None;
    };
    let s = Term::from_record_key(&body[..subject_end])?;
    if let Some((p, o)) = body[subject_end..].strip_prefix(' ').and_then(rest_of) {
        return Some(Triple::new(s, p, o));
    }
    // A raw space in an old record's blank subject label.
    let label_and_rest = body.strip_prefix("_:")?;
    label_and_rest.match_indices(" <").find_map(|(i, _)| {
        let (p, o) = rest_of(&label_and_rest[i + 1..])?;
        let s = Term::Blank(unescape_name(label_and_rest[..i].to_string()));
        Some(Triple::new(s, p, o))
    })
}

/// Append `iri` (an IRI's text, without its brackets) to `out` as IRIREF
/// holds it: every character IRIREF excludes (a space, a control character,
/// `<>"{}|^`\`) as a `\uXXXX` escape, which the readers decode — so what
/// is appended holds no ASCII space or control character, and no `\` but
/// those escapes.
pub(crate) fn escape_iriref(iri: &str, out: &mut String) {
    use std::fmt::Write as _;
    for c in iri.chars() {
        if excluded_from_iriref(c) {
            let _ = write!(out, "\\u{:04X}", u32::from(c));
        } else {
            out.push(c);
        }
    }
}

/// Decode the `\uXXXX` / `\UXXXXXXXX` escapes of an IRI's text (the inverse
/// of [`escape_iriref`]); `None` for a `\` that starts no such escape.
fn unescape_iriref(s: &str) -> Option<String> {
    if !s.contains('\\') {
        return Some(s.to_string());
    }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let n = match chars.next()? {
            'u' => 4,
            'U' => 8,
            _ => return None,
        };
        let mut code = 0u32;
        for _ in 0..n {
            code = code * 16 + chars.next()?.to_digit(16)?;
        }
        out.push(char::from_u32(code)?);
    }
    Some(out)
}

/// Whether an object position holds an entity or a literal (gStore:
/// `TripleWithObjType::ObjectType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ObjectType {
    Entity,
    Literal,
}

/// A parsed RDF triple. `object_type` is derived from `object` and cached so the
/// loader can route the object into the entity vs literal id space without
/// re-inspecting the term.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Triple {
    pub subject: Term,
    pub predicate: Term,
    pub object: Term,
}

impl Triple {
    pub fn new(subject: Term, predicate: Term, object: Term) -> Triple {
        Triple {
            subject,
            predicate,
            object,
        }
    }

    /// This triple as an N-Triples statement (`S P O .`, no newline), each
    /// term through [`Term::to_ntriples`]: the line every N-Triples output
    /// writes.
    pub fn to_ntriples(&self) -> String {
        let mut out = String::new();
        for t in [&self.subject, &self.predicate, &self.object] {
            t.write_ntriples(&mut out);
            out.push(' ');
        }
        out.push('.');
        out
    }

    /// Classify the object position.
    pub fn object_type(&self) -> ObjectType {
        if self.object.is_literal() {
            ObjectType::Literal
        } else {
            ObjectType::Entity
        }
    }
}

impl fmt::Display for Triple {
    /// `S P O .` with each term in its dictionary-key form (see
    /// [`Term`]'s `Display`); N-Triples output uses [`Triple::to_ntriples`].
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {} {} .", self.subject, self.predicate, self.object)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dictionary_keys_read_back_as_the_terms_they_name() {
        for t in [
            Term::iri("http://ex/a"),
            Term::iri("http://ex/a>b c\\u0041\\d"),
            Term::iri(""),
            Term::blank("b1_x-y"),
            Term::plain_literal("a \"q\"\nb\\c\t\r\\u0041"),
            Term::typed_literal("1", "http://www.w3.org/2001/XMLSchema#integer"),
            Term::typed_literal("x", "http://ex/dt>odd"),
            Term::Literal {
                value: "x".into(),
                datatype: None,
                lang: Some("en-GB".into()),
            },
            Term::Literal {
                value: "x".into(),
                datatype: None,
                lang: Some(String::new()),
            },
            Term::plain_literal(""),
        ] {
            assert_eq!(Term::from_dict_key(&t.dict_key()), Some(t.clone()), "{t}");
        }
        for not_a_key in ["", "x", "\"open", "\"x\"junk", "\"\\q\""] {
            assert_eq!(Term::from_dict_key(not_a_key), None, "{not_a_key}");
        }
    }

    #[test]
    fn language_tags_follow_langtag() {
        for ok in ["en", "en-GB", "zh-Hant-TW", "x-1", "EN", "de-1996"] {
            assert!(is_valid_lang_tag(ok), "{ok}");
        }
        for bad in [
            "", "-", "-x", "en-", "1en", "en--GB", "en_GB", "en GB", "en>",
        ] {
            assert!(!is_valid_lang_tag(bad), "{bad}");
        }
        assert_eq!(normalize_lang_tag("zh_CN"), Ok(Some("zh-CN".into())));
        assert_eq!(normalize_lang_tag(" fr "), Ok(Some("fr".into())));
        assert_eq!(normalize_lang_tag(""), Ok(None));
        assert_eq!(normalize_lang_tag("  "), Ok(None));
        assert!(normalize_lang_tag("en US").is_err());
        assert!(normalize_lang_tag("en>bad").is_err());
        assert_eq!(output_lang_tag("en-GB").as_deref(), Some("en-GB"));
        assert_eq!(output_lang_tag("zh_CN").as_deref(), Some("zh-CN"));
        assert_eq!(output_lang_tag("en US").as_deref(), Some("en-US"));
        assert_eq!(output_lang_tag("en>\n<x").as_deref(), Some("en-x"));
        assert_eq!(output_lang_tag("").as_deref(), None);
        assert_eq!(output_lang_tag("1x").as_deref(), None);
    }

    #[test]
    fn blank_labels_are_made_valid_stably() {
        for ok in ["b0", "a.b", "a..b", "é中", "_x", "-x", "genid-0"] {
            assert!(is_valid_blank_label(ok), "{ok}");
            assert_eq!(safe_blank_label(ok), ok);
        }
        for bad in ["", "g.", "a b", "a\u{0}b", "x>", "a\\b"] {
            assert!(!is_valid_blank_label(bad), "{bad}");
            let safe = safe_blank_label(bad);
            assert!(is_valid_blank_label(&safe), "{bad:?} -> {safe}");
            assert_eq!(safe, safe_blank_label(bad), "deterministic");
        }
        assert_ne!(safe_blank_label("a b"), safe_blank_label("a_b"));
        assert_ne!(safe_blank_label("a b"), safe_blank_label("a\tb"));
        assert!(safe_blank_label("a b").ends_with("-a_b"));
    }

    #[test]
    fn ntriples_output_escapes_every_term() {
        let t = Triple::new(
            Term::iri("http://a> <http://evil> <http://c> .\n<http://d"),
            Term::iri("http://ex/p"),
            Term::Literal {
                value: "a\u{1}\u{7f}b\n\"q\"\\".into(),
                datatype: None,
                lang: Some("en> x".into()),
            },
        );
        let line = t.to_ntriples();
        assert!(!line.contains('\n') && !line.contains('\u{1}'), "{line}");
        assert_eq!(
            line,
            "<http://a\\u003E\\u0020\\u003Chttp://evil\\u003E\\u0020\\u003Chttp://c\\u003E\\u0020.\
             \\u000A\\u003Chttp://d> <http://ex/p> \"a\\u0001\\u007Fb\\n\\\"q\\\"\\\\\"@en-x ."
        );
        let back = crate::parser::ntriples::parse_str(&line).unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].subject, t.subject);
        assert_eq!(
            Term::blank("a b").to_ntriples(),
            format!("_:{}", safe_blank_label("a b"))
        );
        // A well-formed term is its dictionary key.
        for t in [
            Term::iri("http://ex/a"),
            Term::blank("b1"),
            Term::plain_literal("x y\tz"),
            Term::typed_literal("1", "http://www.w3.org/2001/XMLSchema#integer"),
        ] {
            assert_eq!(t.to_ntriples(), t.dict_key());
            assert!(matches!(
                export_key(&t.dict_key()),
                std::borrow::Cow::Borrowed(_)
            ));
        }
    }

    #[test]
    fn records_name_every_stored_term_exactly() {
        let odd = [
            Term::iri("http://ex/a>b c\\u0041\n"),
            Term::blank("a b"),
            Term::blank("x <http://ex/q> _:y"),
            Term::blank("g."),
            Term::blank("a\\u0041"),
            Term::Literal {
                value: "v \"x\" \u{1}".into(),
                datatype: None,
                lang: Some("en US\\u0041".into()),
            },
            Term::Literal {
                value: "w".into(),
                datatype: None,
                lang: Some(String::new()),
            },
            Term::typed_literal("1", "http://ex/dt> x"),
            Term::plain_literal("😀 a"),
            Term::blank("😀"),
        ];
        for t in &odd {
            let rec = record_key(&t.dict_key()).into_owned();
            assert!(!rec.contains('\n'), "{rec}");
            assert_eq!(Term::from_record_key(&rec).as_ref(), Some(t), "{rec}");
        }
        let p = Term::iri("http://ex/p");
        for s in odd.iter().filter(|t| t.is_entity()) {
            for o in &odd {
                let t = Triple::new(s.clone(), p.clone(), o.clone());
                let rec = format!(
                    "{} {} {} .",
                    record_key(&s.dict_key()),
                    record_key(&p.dict_key()),
                    record_key(&o.dict_key())
                );
                assert_eq!(parse_record_triple(&rec), Some(t), "{rec}");
            }
        }
        // A valid key is written as before.
        for key in ["<http://ex/a>", "_:b1", "\"x\"@en-GB", "\"a b\""] {
            assert!(
                matches!(record_key(key), std::borrow::Cow::Borrowed(_)),
                "{key}"
            );
        }
        // A record an older version wrote with a raw space in a label.
        assert_eq!(
            parse_record_triple("_:a b <http://ex/p> \"x\"@en US ."),
            Some(Triple::new(
                Term::blank("a b"),
                p.clone(),
                Term::Literal {
                    value: "x".into(),
                    datatype: None,
                    lang: Some("en US".into())
                }
            ))
        );
        assert_eq!(parse_record_triple("not a record"), None);
    }

    #[test]
    fn iri_function_text_is_checked_as_iriref() {
        assert!(is_valid_iriref("http://ex/a?b=c#d"));
        assert!(is_valid_iriref(""));
        for bad in ["http://a> <b", "a b", "a\nb", "a\"b", "a{b", "a\\b"] {
            assert!(!is_valid_iriref(bad), "{bad}");
        }
    }

    #[test]
    fn iri_roundtrips_through_display() {
        let t = Term::iri("http://example/a");
        assert_eq!(t.to_string(), "<http://example/a>");
        assert!(t.is_entity());
        assert!(!t.is_literal());
    }

    #[test]
    fn blank_node_display() {
        assert_eq!(Term::blank("b0").to_string(), "_:b0");
        assert!(Term::blank("b0").is_entity());
    }

    #[test]
    fn plain_literal_display() {
        assert_eq!(Term::plain_literal("hello").to_string(), "\"hello\"");
    }

    #[test]
    fn typed_literal_display() {
        let t = Term::typed_literal("2500", "http://www.w3.org/2001/XMLSchema#integer");
        assert_eq!(
            t.to_string(),
            "\"2500\"^^<http://www.w3.org/2001/XMLSchema#integer>"
        );
        assert!(t.is_literal());
    }

    #[test]
    fn lang_literal_display() {
        let t = Term::Literal {
            value: "chat".into(),
            datatype: None,
            lang: Some("fr".into()),
        };
        assert_eq!(t.to_string(), "\"chat\"@fr");
    }

    #[test]
    fn literal_with_quotes_is_escaped() {
        let t = Term::plain_literal("say \"hi\"\nbye");
        assert_eq!(t.to_string(), "\"say \\\"hi\\\"\\nbye\"");
    }

    #[test]
    fn triple_object_type_is_derived() {
        let t = Triple::new(Term::iri("a"), Term::iri("p"), Term::plain_literal("v"));
        assert_eq!(t.object_type(), ObjectType::Literal);
        let t2 = Triple::new(Term::iri("a"), Term::iri("p"), Term::iri("b"));
        assert_eq!(t2.object_type(), ObjectType::Entity);
    }

    #[test]
    fn dict_key_distinguishes_iri_from_same_named_literal() {
        // <foo> and "foo" must not collide in the dictionary.
        assert_ne!(
            Term::iri("foo").dict_key(),
            Term::plain_literal("foo").dict_key()
        );
    }
}
