# spec: SPARQL 1.1 Query §19.8 Grammar, rules [50] Quads and [52] TriplesTemplate ::=
# TriplesSameSubject ( '.' TriplesTemplate? )? (the QuadData of INSERT DATA)
PREFIX : <http://example.org/>
INSERT DATA { :a :p :b :c :p :d }
