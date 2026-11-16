# spec: SPARQL 1.1 Update §3.1.3.2 INSERT with SPARQL 1.1 Query §12 and §19.8 Grammar rule [53] GroupGraphPattern ::= '{' ( SubSelect | GroupGraphPatternSub ) '}': … WHERE { SELECT … }
PREFIX : <http://example.org/>
INSERT { :stats :people ?n }
WHERE { SELECT (COUNT(DISTINCT ?p) AS ?n) WHERE { ?p :age ?a } }
