//! Sequential dictionary encoding of the parsed quads.
//!
//! Every quad is encoded exactly as `BuildAccumulator::add` (the in-memory
//! build) does — subject, predicate, object, then the graph label — so the
//! ids equal the memory build's: entity and predicate ids from 0, literal
//! ids from `LITERAL_FIRST_ID`, each in first-seen order. Default-graph
//! triples go to the SPO sorter, named-graph quads to the GSPO sorter.
//!
//! A term that cannot be stored on disk fails the build with **exactly** the
//! error the incremental disk build (`DiskStore::insert_keys` /
//! `insert_quad_keys`) gives for it: the id-space bounds of
//! `checked_*_id`, the B+ tree key limits ([`MAX_KEY_LEN`]) and the IvArray
//! capacity, checked in the order and with the texts those writers use, for
//! the terms of a quad in the order the incremental path interns them (the
//! graph first).

use crate::dict::{checked_entity_id, checked_literal_id, checked_predicate_id};
use crate::error::{GStoreError, Result};
use crate::kvstore::bptree::MAX_KEY_LEN;
use crate::kvstore::PAGE_SIZE;
use crate::model::id::LITERAL_FIRST_ID;
use crate::model::Term;
use crate::parser::import::BNODE_GRAPH_PREFIX;
use crate::parser::nquads::Quad;

use super::extsort::ExtSorter;
use super::kvimage::{IVARRAY_CAPACITY, NODE_HEADER};
use super::termmap::{hash_key, TermMap};

/// A dictionary space.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Space {
    Entity,
    Literal,
    Predicate,
}

/// The three spaces' term maps.
#[derive(Debug, Default)]
pub(crate) struct Dict {
    pub(crate) entities: TermMap,
    pub(crate) literals: TermMap,
    pub(crate) predicates: TermMap,
}

impl Dict {
    pub(crate) fn map(&self, space: Space) -> &TermMap {
        match space {
            Space::Entity => &self.entities,
            Space::Literal => &self.literals,
            Space::Predicate => &self.predicates,
        }
    }

    fn map_mut(&mut self, space: Space) -> &mut TermMap {
        match space {
            Space::Entity => &mut self.entities,
            Space::Literal => &mut self.literals,
            Space::Predicate => &mut self.predicates,
        }
    }

    /// Heap bytes of the three maps.
    pub(crate) fn heap_bytes(&self) -> usize {
        self.entities.heap_bytes() + self.literals.heap_bytes() + self.predicates.heap_bytes()
    }
}

/// Render `t`'s dictionary key ([`Term::dict_key`]) into `out` without
/// allocating.
pub(crate) fn render_key(t: &Term, out: &mut Vec<u8>) {
    out.clear();
    match t {
        Term::Iri(s) => {
            out.reserve(s.len() + 2);
            out.push(b'<');
            out.extend_from_slice(s.as_bytes());
            out.push(b'>');
        }
        Term::Blank(s) => {
            out.extend_from_slice(b"_:");
            out.extend_from_slice(s.as_bytes());
        }
        Term::Literal {
            value,
            datatype,
            lang,
        } => {
            out.reserve(value.len() + 2);
            out.push(b'"');
            // `escape_literal`, byte by byte: the escaped characters are
            // ASCII, so they never occur inside a multi-byte UTF-8 sequence.
            for &b in value.as_bytes() {
                match b {
                    b'\\' => out.extend_from_slice(b"\\\\"),
                    b'"' => out.extend_from_slice(b"\\\""),
                    b'\n' => out.extend_from_slice(b"\\n"),
                    b'\r' => out.extend_from_slice(b"\\r"),
                    b'\t' => out.extend_from_slice(b"\\t"),
                    _ => out.push(b),
                }
            }
            out.push(b'"');
            if let Some(dt) = datatype {
                out.extend_from_slice(b"^^<");
                out.extend_from_slice(dt.as_bytes());
                out.push(b'>');
            } else if let Some(l) = lang {
                out.push(b'@');
                out.extend_from_slice(l.as_bytes());
            }
        }
    }
}

/// The graph key the memory build files a quad under (`graph_iri` + an IRI
/// dictionary key): `None` = the default graph.
fn render_graph(g: &Option<Term>, out: &mut Vec<u8>) -> bool {
    out.clear();
    match g {
        Some(Term::Iri(s)) => {
            out.push(b'<');
            out.extend_from_slice(s.as_bytes());
            out.push(b'>');
            true
        }
        Some(Term::Blank(l)) => {
            out.push(b'<');
            out.extend_from_slice(BNODE_GRAPH_PREFIX.as_bytes());
            out.extend_from_slice(l.as_bytes());
            out.push(b'>');
            true
        }
        Some(Term::Literal { .. }) | None => false,
    }
}

/// The checks the incremental disk writers apply to a **new** term of
/// `space` with key length `key_len` that would get dense index `index`
/// (the literal index, not the public id) — same order, same texts:
/// `intern_*_in` (`checked_*_id`), `BTree::insert` (entry size, then
/// [`MAX_KEY_LEN`]), `IvArray::insert` (capacity).
pub(crate) fn check_new_term(space: Space, key_len: usize, index: usize) -> Result<()> {
    let next = index as u64;
    match space {
        Space::Entity => checked_entity_id(next).map(|_| ())?,
        Space::Literal => checked_literal_id(next).map(|_| ())?,
        Space::Predicate => checked_predicate_id(next).map(|_| ())?,
    }
    // The `*2id` value is a 4-byte id.
    if NODE_HEADER + 2 + key_len + 2 + 4 > PAGE_SIZE {
        return Err(GStoreError::Serialize(format!(
            "B+ tree entry ({key_len}-byte key + 4-byte value) too large for a {PAGE_SIZE}-byte page"
        )));
    }
    if key_len > MAX_KEY_LEN {
        return Err(GStoreError::Serialize(format!(
            "B+ tree key of {key_len} bytes exceeds the {MAX_KEY_LEN}-byte key limit"
        )));
    }
    if index >= IVARRAY_CAPACITY {
        return Err(GStoreError::Database(format!(
            "IvArray key {index} exceeds addressable capacity"
        )));
    }
    Ok(())
}

/// Encodes quads into ids and sorter records (see the module docs).
pub(crate) struct Encoder<'a> {
    pub(crate) dict: Dict,
    spo: ExtSorter<'a, [u32; 3]>,
    gspo: ExtSorter<'a, [u32; 4]>,
    /// Reused key buffers: subject, predicate, object, graph.
    keys: [Vec<u8>; 4],
}

impl<'a> Encoder<'a> {
    pub(crate) fn new(spo: ExtSorter<'a, [u32; 3]>, gspo: ExtSorter<'a, [u32; 4]>) -> Encoder<'a> {
        Encoder {
            dict: Dict::default(),
            spo,
            gspo,
            keys: Default::default(),
        }
    }

    /// Encode a batch of quads. The batch is left as it is: its owner clears
    /// it (the parallel parser drops the quads on its own thread).
    pub(crate) fn add(&mut self, batch: &[Quad]) -> Result<()> {
        for q in batch.iter() {
            self.add_quad(q)?;
        }
        Ok(())
    }

    fn add_quad(&mut self, q: &Quad) -> Result<()> {
        let [ks, kp, ko, kg] = &mut self.keys;
        render_key(&q.subject, ks);
        render_key(&q.predicate, kp);
        render_key(&q.object, ko);
        let named = render_graph(&q.graph, kg);
        let o_space = if q.object.is_literal() {
            Space::Literal
        } else {
            Space::Entity
        };
        let terms: [(Space, &[u8]); 4] = [
            (Space::Entity, ks.as_slice()),
            (Space::Predicate, kp.as_slice()),
            (o_space, ko.as_slice()),
            (Space::Entity, kg.as_slice()),
        ];
        let n = if named { 4 } else { 3 };
        let mut hashes = [0u64; 4];
        let mut found = [None; 4];
        let mut any_new = false;
        for i in 0..n {
            let (space, key) = terms[i];
            hashes[i] = hash_key(key);
            found[i] = self.dict.map(space).find(key, hashes[i]);
            any_new |= found[i].is_none();
        }
        if any_new {
            check_quad(&self.dict, &terms[..n], &found[..n])?;
        }
        let mut ids = [0u32; 4];
        for i in 0..n {
            let (space, key) = terms[i];
            let local = match found[i] {
                Some(id) => id,
                None => self.dict.map_mut(space).intern(key, hashes[i]).0,
            };
            ids[i] = if space == Space::Literal {
                LITERAL_FIRST_ID + local
            } else {
                local
            };
        }
        if named {
            self.gspo.push([ids[3], ids[0], ids[1], ids[2]])
        } else {
            self.spo.push([ids[0], ids[1], ids[2]])
        }
    }

    /// The sorters and the dictionary, once every quad is encoded.
    pub(crate) fn into_parts(self) -> (Dict, ExtSorter<'a, [u32; 3]>, ExtSorter<'a, [u32; 4]>) {
        (self.dict, self.spo, self.gspo)
    }
}

/// Check the new terms of one quad in the incremental path's order (graph,
/// subject, predicate, object), each with the dense index it would get there.
#[cold]
fn check_quad(dict: &Dict, terms: &[(Space, &[u8])], found: &[Option<u32>]) -> Result<()> {
    // Incremental order: the graph (index 3) first, then s, p, o.
    let order: &[usize] = if terms.len() == 4 {
        &[3, 0, 1, 2]
    } else {
        &[0, 1, 2]
    };
    let mut seen: Vec<(Space, &[u8])> = Vec::with_capacity(4);
    let mut pending = [0usize; 3];
    for &i in order {
        if found[i].is_some() {
            continue;
        }
        let (space, key) = terms[i];
        if seen.contains(&(space, key)) {
            continue; // interned by an earlier term of this quad
        }
        seen.push((space, key));
        let slot = space as usize;
        let index = dict.map(space).len() + pending[slot];
        pending[slot] += 1;
        check_new_term(space, key.len(), index)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rendered_keys_equal_dict_keys() {
        let terms = [
            Term::iri("http://ex/a"),
            Term::iri(""),
            Term::blank("b0"),
            Term::plain_literal("plain"),
            Term::plain_literal("quote \" back \\ nl \n cr \r tab \t é ü 中"),
            Term::typed_literal("42", "http://www.w3.org/2001/XMLSchema#integer"),
            Term::Literal {
                value: "chat".into(),
                datatype: None,
                lang: Some("fr".into()),
            },
            Term::plain_literal(""),
        ];
        let mut buf = Vec::new();
        for t in &terms {
            render_key(t, &mut buf);
            assert_eq!(buf, t.dict_key().as_bytes(), "{t:?}");
        }
    }

    #[test]
    fn graph_keys_follow_the_memory_build() {
        let mut buf = Vec::new();
        assert!(render_graph(&Some(Term::iri("http://g")), &mut buf));
        assert_eq!(buf, b"<http://g>");
        assert!(render_graph(&Some(Term::blank("x")), &mut buf));
        assert_eq!(buf, format!("<{BNODE_GRAPH_PREFIX}x>").as_bytes());
        assert!(!render_graph(&None, &mut buf));
        assert!(!render_graph(&Some(Term::plain_literal("l")), &mut buf));
    }

    #[test]
    fn term_checks_match_the_incremental_writers() {
        assert!(check_new_term(Space::Entity, MAX_KEY_LEN, 0).is_ok());
        let e = check_new_term(Space::Entity, MAX_KEY_LEN + 1, 0).unwrap_err();
        assert_eq!(
            e.to_string(),
            format!(
                "serialization error: B+ tree key of {} bytes exceeds the {MAX_KEY_LEN}-byte key limit",
                MAX_KEY_LEN + 1
            )
        );
        let e = check_new_term(Space::Literal, 5000, 0).unwrap_err();
        assert!(
            e.to_string().contains("too large for a 4096-byte page"),
            "{e}"
        );
        let e = check_new_term(Space::Predicate, 10, IVARRAY_CAPACITY).unwrap_err();
        assert_eq!(
            e.to_string(),
            format!("database error: IvArray key {IVARRAY_CAPACITY} exceeds addressable capacity")
        );
        let e = check_new_term(Space::Entity, 10, LITERAL_FIRST_ID as usize).unwrap_err();
        assert!(e.to_string().contains("entity id space exhausted"), "{e}");
    }
}
