# spec: SPARQL 1.1 Update §3.2.3 COPY DEFAULT TO a graph that does not exist: it is
# created with every default-graph triple (language-tagged and typed literals and
# the blank node included, the same node in both graphs); the default graph keeps
# its triples
PREFIX : <http://example.org/>
COPY DEFAULT TO :g3
