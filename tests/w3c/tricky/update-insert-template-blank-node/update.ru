# spec: SPARQL 1.1 Update §3.1.3 DELETE/INSERT (blank nodes in an INSERT template are
# instantiated as fresh blank nodes for each solution) and SPARQL 1.1 Query §16.2.1
PREFIX : <http://example.com/>
INSERT { ?s :p _:n . _:n :v ?o } WHERE { ?s :q ?o }
