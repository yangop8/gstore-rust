# spec: SPARQL 1.1 Update §3.1.1 INSERT DATA (Example 1)
PREFIX dc: <http://purl.org/dc/elements/1.1/>
INSERT DATA
{
  <http://example/book1> dc:title "A new book" ;
                         dc:creator "A.N.Other" .
}
