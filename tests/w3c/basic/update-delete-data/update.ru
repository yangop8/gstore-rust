# spec: SPARQL 1.1 Update §3.1.2 DELETE DATA (Example 3)
PREFIX dc: <http://purl.org/dc/elements/1.1/>
DELETE DATA
{
  <http://example/book2> dc:title "David Copperfield" ;
                         dc:creator "Edmund Wells" .
}
