# spec: SPARQL 1.1 Update §3.2.1 CREATE of a graph that does not exist succeeds, and
# §3.1.1 INSERT DATA then adds to it
PREFIX : <http://example.org/>
CREATE GRAPH :g2 ;
INSERT DATA { GRAPH :g2 { :c :p :d } }
