//! Query result sets.
//!
//! gStore's `ResultSet` holds the variable list and a string matrix. This is the
//! same idea: ordered column names plus rows of optional cell strings (`None`
//! renders as a blank / unbound cell).

/// The outcome of evaluating a query.
#[derive(Debug, Clone, PartialEq)]
pub enum QueryResult {
    /// SELECT results: a table.
    Select(ResultSet),
    /// ASK results: a single boolean.
    Ask(bool),
    /// CONSTRUCT results: a set of triples.
    Construct(Vec<crate::model::Triple>),
    /// An update (INSERT/DELETE DATA): the number of triples actually changed.
    Update { changed: usize },
}

/// A SELECT result table.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ResultSet {
    /// Projected variable names, in column order (without the `?`).
    pub vars: Vec<String>,
    /// Rows; each row has one cell per column. `None` = unbound.
    pub rows: Vec<Vec<Option<String>>>,
}

impl ResultSet {
    pub fn new(vars: Vec<String>) -> ResultSet {
        ResultSet {
            vars,
            rows: Vec::new(),
        }
    }

    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Render the table as gStore-style aligned text (header + rows).
    pub fn to_table_string(&self) -> String {
        let mut widths: Vec<usize> = self.vars.iter().map(|v| v.len() + 1).collect();
        for row in &self.rows {
            for (i, cell) in row.iter().enumerate() {
                let w = cell.as_deref().unwrap_or("").len();
                if w > widths[i] {
                    widths[i] = w;
                }
            }
        }
        let mut out = String::new();
        // header
        for (i, v) in self.vars.iter().enumerate() {
            out.push_str(&pad(&format!("?{v}"), widths[i] + 2));
        }
        out.push('\n');
        for row in &self.rows {
            for (i, cell) in row.iter().enumerate() {
                out.push_str(&pad(cell.as_deref().unwrap_or(""), widths[i] + 2));
            }
            out.push('\n');
        }
        out
    }
}

fn pad(s: &str, width: usize) -> String {
    let mut s = s.to_string();
    while s.len() < width {
        s.push(' ');
    }
    s
}

