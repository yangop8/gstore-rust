# spec: SPARQL 1.1 Update §3.1.1 INSERT DATA (Example 2: into a named graph)
PREFIX ns: <http://example.org/ns#>
INSERT DATA
{ GRAPH <http://example/bookStore> { <http://example/book1>  ns:price  42 } }
