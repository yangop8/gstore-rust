# spec: SPARQL 1.1 Update §3 (a request is a sequence of operations; when one fails,
# the request fails) and §3.2.2 DROP of a graph that does not exist: the INSERT
# after the failing DROP is not applied
PREFIX : <http://example.org/>
DROP GRAPH :none ;
INSERT DATA { :new :p :o }
