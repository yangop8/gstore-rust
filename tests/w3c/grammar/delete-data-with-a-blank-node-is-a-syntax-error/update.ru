# spec: SPARQL 1.1 Update §3.1.2 DELETE DATA and SPARQL 1.1 Query §19.8 Grammar (notes):
# blank node syntax is not allowed in DELETE DATA, DELETE WHERE or a DELETE clause
PREFIX : <http://example.org/>
DELETE DATA { _:b :p "x" }
