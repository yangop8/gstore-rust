# spec: SPARQL 1.1 Update §3.2.1 CREATE: creating a graph that already exists fails
# unless SILENT is given — :g1 holds a triple, so it exists
PREFIX : <http://example.org/>
CREATE GRAPH :g1
