# spec: SPARQL 1.1 Update §3.1.5 CLEAR: with SILENT the clear of a graph that does not
# exist succeeds and changes nothing
PREFIX : <http://example.org/>
CLEAR SILENT GRAPH :none
