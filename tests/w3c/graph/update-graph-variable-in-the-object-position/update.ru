# spec: SPARQL 1.1 Update §3.1.3 DELETE/INSERT with SPARQL 1.1 Query §18.5 Graph(var, P):
# only (?g = :g2, ?s = :s2) is a solution of the WHERE clause
PREFIX : <http://example.org/>
INSERT { ?s :inGraph ?g } WHERE { GRAPH ?g { ?s :p ?g } }
