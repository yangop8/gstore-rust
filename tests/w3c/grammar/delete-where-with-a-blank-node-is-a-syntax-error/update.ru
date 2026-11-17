# spec: SPARQL 1.1 Update §3.1.3.3 DELETE WHERE and SPARQL 1.1 Query §19.8 Grammar
# (notes): blank node syntax is not allowed in DELETE WHERE
PREFIX : <http://example.org/>
DELETE WHERE { _:b :p ?o }
