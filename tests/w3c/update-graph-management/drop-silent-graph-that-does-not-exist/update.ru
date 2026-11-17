# spec: SPARQL 1.1 Update §3.2.2 DROP: with SILENT the drop of a graph that does not
# exist succeeds and changes nothing
PREFIX : <http://example.org/>
DROP SILENT GRAPH :none
