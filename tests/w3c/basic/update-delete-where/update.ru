# spec: SPARQL 1.1 Update §3.1.3.3 DELETE WHERE (Example 11, on the default graph)
PREFIX foaf: <http://xmlns.com/foaf/0.1/>
DELETE WHERE { ?person foaf:givenName 'Fred' ;
                       ?property ?value }
