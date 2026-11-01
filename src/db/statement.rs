//! A parsed SPARQL request: the text (which the query cache and the update
//! log key on) together with its syntax tree, so a request is parsed once and
//! can be routed — to the shared read path ([`Database::read_as`]) or the
//! exclusive write path ([`Database::execute_as`]) — before it runs.
//!
//! [`Database::read_as`]: super::Database::read_as
//! [`Database::execute_as`]: super::Database::execute_as

use std::sync::Arc;

use crate::error::Result;
use crate::parser::sparql;
use crate::parser::sparql::ast::Query;

/// One parsed SPARQL request (a query or an UPDATE).
#[derive(Debug, Clone)]
pub struct Statement {
    text: Arc<str>,
    query: Query,
}

impl Statement {
    /// Parse `text` as a SPARQL query or update.
    pub fn parse(text: &str) -> Result<Statement> {
        let query = sparql::parse(text)?;
        Ok(Statement {
            text: Arc::from(text),
            query,
        })
    }

    /// Whether this is an UPDATE request (it needs exclusive access to the
    /// database); `false` for SELECT, ASK, CONSTRUCT and DESCRIBE.
    pub fn is_update(&self) -> bool {
        matches!(self.query, Query::Update(_))
    }

    /// The request text, as given.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The parsed request.
    pub fn query(&self) -> &Query {
        &self.query
    }

    /// The text and the parsed request.
    pub(crate) fn into_parts(self) -> (Arc<str>, Query) {
        (self.text, self.query)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_reads_and_updates() {
        let q = Statement::parse("SELECT ?s WHERE { ?s ?p ?o }").unwrap();
        assert!(!q.is_update());
        assert_eq!(q.text(), "SELECT ?s WHERE { ?s ?p ?o }");
        assert!(matches!(q.query(), Query::Select(_)));
        let u = Statement::parse("INSERT DATA { <a> <b> <c> }").unwrap();
        assert!(u.is_update());
        assert!(Statement::parse("SELEKT").is_err());
    }
}
