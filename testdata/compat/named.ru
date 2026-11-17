INSERT DATA {
  GRAPH <http://example.org/graph/contacts> {
    <http://example.org/alice> <http://example.org/email> <mailto:alice@example.org> .
    <http://example.org/bob> <http://example.org/email> <mailto:bob@example.org> .
  }
  GRAPH <http://example.org/graph/hr> {
    <http://example.org/alice> <http://example.org/salary> "5000"^^<http://www.w3.org/2001/XMLSchema#integer> .
    <http://example.org/bob> <http://example.org/salary> "4200"^^<http://www.w3.org/2001/XMLSchema#integer> .
    <http://example.org/alice> <http://example.org/knows> <http://example.org/bob> .
  }
}
