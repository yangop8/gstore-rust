# spec: SPARQL 1.1 Update §3.2.2 DROP and §3.1.5 CLEAR of DEFAULT, NAMED and ALL: the
# default graph always exists and NAMED / ALL name no particular graph, so they
# succeed on an empty dataset
DROP DEFAULT ;
DROP NAMED ;
DROP ALL ;
CLEAR DEFAULT ;
CLEAR NAMED ;
CLEAR ALL
