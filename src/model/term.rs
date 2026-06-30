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
