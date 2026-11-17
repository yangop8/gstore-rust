# spec: SPARQL 1.1 Update §3.2.2 DROP removes :g1, so §3.2.1 CREATE GRAPH :g1 creates a
# graph that does not exist (and empty: the dataset holds no triple of it)
PREFIX : <http://example.org/>
DROP GRAPH :g1 ;
CREATE GRAPH :g1
