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
    pub fn dict_key(&self) -> String {
        self.to_string()
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
    /// Render a term in N-Triples surface syntax.
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

/// Escape a literal's lexical value for N-Triples output (`"`, `\`, newlines…).
fn escape_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(c),
        }
    }
    out
}

/// A character IRIREF excludes (`[^#x00-#x20<>"{}|^`\\]`).
fn excluded_from_iriref(c: char) -> bool {
    c <= ' ' || matches!(c, '<' | '>' | '"' | '{' | '}' | '|' | '^' | '`' | '\\')
}

/// A dictionary key (a term in N-Triples form, see [`Term::dict_key`]) as the
/// N-Triples / Turtle / N-Quads readers accept it back: in an IRI, or a
/// literal's datatype IRI, every character IRIREF excludes (a space, a
/// control character, `<>"{}|^`\`) becomes a `\uXXXX` escape, which the
/// readers decode. The lenient readers let such IRIs into a database, so the
/// exporters write keys through this to keep an export loadable. Any other
/// key is returned as is.
pub fn export_key(key: &str) -> std::borrow::Cow<'_, str> {
    use std::borrow::Cow;
    use std::fmt::Write as _;
    let (head, iri) = if let Some(inner) = key.strip_prefix('<').and_then(|k| k.strip_suffix('>')) {
        ("<", inner)
    } else if key.starts_with('"') && key.ends_with('>') {
        match key.rfind("\"^^<") {
            Some(i) => (&key[..i + 4], &key[i + 4..key.len() - 1]),
            None => return Cow::Borrowed(key),
        }
    } else {
        return Cow::Borrowed(key);
    };
    if !iri.chars().any(excluded_from_iriref) {
        return Cow::Borrowed(key);
    }
    let mut out = String::with_capacity(key.len() + 16);
    out.push_str(head);
    for c in iri.chars() {
        if excluded_from_iriref(c) {
            let _ = write!(out, "\\u{:04X}", u32::from(c));
        } else {
            out.push(c);
        }
    }
    out.push('>');
    Cow::Owned(out)
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
