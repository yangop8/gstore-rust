//! Database statistics and construction helpers for the administrative
//! operations of the upstream-compatible API (`build`, `monitor`, `schema`
//! in [`crate::gapi`]): building from already-parsed triples, the distinct
//! subject count, the `rdf:type` class statistics (upstream's `subjectList`)
//! and the class / property schema graph (upstream `Database::updateSchema`,
//! `src/Database/Schema.cpp`).

use std::collections::{BTreeMap, BTreeSet, HashMap};

use super::Database;
use crate::error::Result;
use crate::model::id::{is_entity_id, is_literal_id, EntityLiteralId, PredId};
use crate::model::{IdTriple, Term, Triple};
use crate::parser::sparql::ast::RDF_TYPE;
use crate::store::{MutableStore, TripleSource};

/// One node of a [`SchemaGraph`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaNode {
    /// The node's id (classes first: `1..=n`, then their properties).
    pub id: u32,
    /// The class or property in N-Triples form (`<http://…>`).
    pub label: String,
    /// `false`: a class (upstream `type` 0); `true`: a literal-valued
    /// property of a class's instances (upstream `type` 1).
    pub property: bool,
}

/// One edge of a [`SchemaGraph`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaEdge {
    pub source: u32,
    pub target: u32,
    /// The predicate (N-Triples form) of a class → class relation; `None` for
    /// a class → property edge.
    pub label: Option<String>,
}

/// The schema graph of a database as upstream gStore derives it
/// (`Database::updateSchema`): every `rdf:type` class is a node; each class
/// links to the literal-valued properties its instances use, and to the
/// classes of the entities its instances point at (labelled with the
/// predicate).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SchemaGraph {
    /// Each class node followed by its property nodes, classes in label
    /// order.
    pub nodes: Vec<SchemaNode>,
    /// The class → property edges, then the class → class relations.
    pub edges: Vec<SchemaEdge>,
}

impl Database {
    /// Build an in-memory database from parsed triples (bulk-loaded, like
    /// [`build_from_files`](Self::build_from_files)). Every triple is taken
    /// as it is — callers drop the ones that are not valid RDF data first.
    pub fn build_from_triples(
        name: impl Into<String>,
        triples: impl IntoIterator<Item = Triple>,
    ) -> Result<Database> {
        let mut db = Database::new(name);
        let mut ids: Vec<IdTriple> = Vec::new();
        for t in triples {
            ids.push(db.encode_triple(&t)?);
        }
        db.store.bulk_load(ids);
        db.rebuild_index();
        Ok(db)
    }

    /// Write the default graph as N-Triples to `out` (upstream
    /// `Database::export_db`; what `gexport` writes). Returns the number of
    /// triples written. IRIs are written through
    /// [`export_key`](crate::model::term::export_key), so the export loads back.
    ///
    /// The same writer as `gexport` and `/api export`
    /// ([`write_export`](crate::zipio::write_export) in N-Triples), so it
    /// runs inside a read scope (contract C2): a read error a backend reports
    /// fails the export.
    pub fn export_ntriples(&self, out: &mut dyn std::io::Write) -> Result<u64> {
        crate::zipio::write_export(self, crate::zipio::ExportFormat::NTriples, out)
    }

    /// The number of distinct subjects of the default graph (upstream
    /// `monitor`'s `subjectNum`).
    pub fn subject_num(&self) -> usize {
        self.store.distinct_subjects()
    }

    /// The `rdf:type` id, if the database uses it.
    fn type_pred(&self) -> Option<PredId> {
        self.dict.predicate_id(&Term::iri(RDF_TYPE).dict_key())
    }

    /// How many subjects have each `rdf:type` class (upstream `monitor`'s
    /// `subjectList`, from `Database::getStatisticsInfo`): `(class in
    /// N-Triples form, count)`, sorted by class.
    pub fn type_statistics(&self) -> Vec<(String, u64)> {
        let Some(tp) = self.type_pred() else {
            return Vec::new();
        };
        let mut counts: HashMap<EntityLiteralId, u64> = HashMap::new();
        for (_s, o) in self.store.so_by_p(tp) {
            if is_entity_id(o) {
                *counts.entry(o).or_default() += 1;
            }
        }
        let mut out: Vec<(String, u64)> = counts
            .into_iter()
            .filter_map(|(o, n)| self.dict.id_to_string(o).map(|s| (s.to_string(), n)))
            .collect();
        out.sort();
        out
    }

    /// The schema graph (see [`SchemaGraph`]).
    pub fn schema_graph(&self) -> SchemaGraph {
        let mut graph = SchemaGraph::default();
        let Some(tp) = self.type_pred() else {
            return graph;
        };
        // Class → its instances.
        let mut instances: HashMap<EntityLiteralId, Vec<EntityLiteralId>> = HashMap::new();
        for (s, o) in self.store.so_by_p(tp) {
            if is_entity_id(o) {
                instances.entry(o).or_default().push(s);
            }
        }
        let mut classes: Vec<(String, EntityLiteralId)> = instances
            .keys()
            .filter_map(|&c| self.dict.id_to_string(c).map(|s| (s.to_string(), c)))
            .collect();
        classes.sort();
        let class_ids: HashMap<EntityLiteralId, u32> = classes
            .iter()
            .enumerate()
            .map(|(i, (_, c))| (*c, i as u32 + 1))
            .collect();
        let mut next_id = classes.len() as u32 + 1;
        let mut relations: Vec<SchemaEdge> = Vec::new();
        for (label, class) in &classes {
            let id = class_ids[class];
            graph.nodes.push(SchemaNode {
                id,
                label: label.clone(),
                property: false,
            });
            let mut props: BTreeSet<String> = BTreeSet::new();
            let mut rels: BTreeMap<(String, String), u32> = BTreeMap::new();
            for &s in &instances[class] {
                for (p, o) in self.store.po_by_s(s) {
                    let Some(pname) = self.dict.predicate_to_string(p) else {
                        continue;
                    };
                    if is_literal_id(o) {
                        props.insert(pname.to_string());
                    } else if p != tp {
                        for c2 in self.store.o_by_sp(o, tp) {
                            if let (Some(&target), Some(cname)) =
                                (class_ids.get(&c2), self.dict.id_to_string(c2))
                            {
                                rels.insert((pname.to_string(), cname.to_string()), target);
                            }
                        }
                    }
                }
            }
            for p in props {
                graph.nodes.push(SchemaNode {
                    id: next_id,
                    label: p,
                    property: true,
                });
                graph.edges.push(SchemaEdge {
                    source: id,
                    target: next_id,
                    label: None,
                });
                next_id += 1;
            }
            for ((p, _), target) in rels {
                relations.push(SchemaEdge {
                    source: id,
                    target,
                    label: Some(p),
                });
            }
        }
        graph.edges.extend(relations);
        graph
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DATA: &str = r#"
<http://ex/a> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://ex/Person> .
<http://ex/b> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://ex/Person> .
<http://ex/u> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://ex/Univ> .
<http://ex/a> <http://ex/name> "Alice" .
<http://ex/a> <http://ex/worksAt> <http://ex/u> .
<http://ex/u> <http://ex/label> "U" .
"#;

    #[test]
    fn statistics_and_schema_graph() {
        let triples = crate::parser::ntriples::parse_str(DATA).unwrap();
        let db = Database::build_from_triples("s", triples).unwrap();
        assert_eq!(db.triple_num(), 6);
        assert_eq!(db.subject_num(), 3);
        assert_eq!(
            db.type_statistics(),
            vec![
                ("<http://ex/Person>".to_string(), 2),
                ("<http://ex/Univ>".to_string(), 1)
            ]
        );
        let g = db.schema_graph();
        let labels: Vec<(u32, &str, bool)> = g
            .nodes
            .iter()
            .map(|n| (n.id, n.label.as_str(), n.property))
            .collect();
        assert_eq!(
            labels,
            vec![
                (1, "<http://ex/Person>", false),
                (3, "<http://ex/name>", true),
                (2, "<http://ex/Univ>", false),
                (4, "<http://ex/label>", true),
            ]
        );
        assert_eq!(
            g.edges,
            vec![
                SchemaEdge {
                    source: 1,
                    target: 3,
                    label: None
                },
                SchemaEdge {
                    source: 2,
                    target: 4,
                    label: None
                },
                SchemaEdge {
                    source: 1,
                    target: 2,
                    label: Some("<http://ex/worksAt>".to_string())
                },
            ]
        );
        let mut out = Vec::new();
        assert_eq!(db.export_ntriples(&mut out).unwrap(), 6);
        let back = crate::parser::ntriples::parse_str(&String::from_utf8(out).unwrap()).unwrap();
        assert_eq!(back.len(), 6);
        // No rdf:type at all: empty statistics and graph.
        let plain = Database::build_from_str("p", "<http://x> <http://y> <http://z> .").unwrap();
        assert!(plain.type_statistics().is_empty());
        assert_eq!(plain.schema_graph(), SchemaGraph::default());
    }
}
