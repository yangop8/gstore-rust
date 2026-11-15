# spec: SPARQL 1.1 Update §3.1.3 DELETE/INSERT (Example 5, on the default graph)
PREFIX foaf: <http://xmlns.com/foaf/0.1/>
DELETE { ?person foaf:givenName 'Bill' }
INSERT { ?person foaf:givenName 'William' }
WHERE  { ?person foaf:givenName 'Bill' }
