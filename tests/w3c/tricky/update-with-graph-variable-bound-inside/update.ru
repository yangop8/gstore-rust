# spec: SPARQL 1.1 Update §3.1.3 DELETE/INSERT with SPARQL 1.1 Query §18.5 (Graph(var, P)):
# only (?g = :g1, ?t = "2020") is a solution of the WHERE clause
PREFIX : <http://example.com/>
INSERT { ?g :meta ?t } WHERE { GRAPH ?g { ?g :created ?t } }
