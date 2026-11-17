# spec: SPARQL 1.1 Update §3.2.3 COPY :g1 TO :g2: :g2 first loses its own triples, then
# holds a copy of :g1's (the blank node the same node in both)
PREFIX : <http://example.org/>
COPY :g1 TO :g2
