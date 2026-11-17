# spec: SPARQL 1.1 Update §3.1.3 DELETE/INSERT and SPARQL 1.1 Query §19.8 Grammar (notes):
# blank node syntax is not allowed in the DeleteClause of DELETE
PREFIX : <http://example.org/>
DELETE { ?s :p _:b } WHERE { ?s :p ?o }
