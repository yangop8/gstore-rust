# spec: SPARQL 1.1 Update §3.1.3.2 INSERT (a template with GRAPH)
PREFIX : <http://example.org/>
INSERT { GRAPH :copy { ?s :q ?o } } WHERE { ?s :p ?o }
