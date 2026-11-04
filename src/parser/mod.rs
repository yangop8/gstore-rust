//! Parsing: RDF input and SPARQL queries.
//!
//! Corresponds to gStore's `src/Parser`. RDF readers: N-Triples, N-Quads,
//! Turtle and TriG stream from a [`std::io::BufRead`] with bounded memory
//! (see [`stream`]); RDF/XML, JSON-LD and RDFa parse a whole document. Bulk
//! loads go through [`import`] (formats by extension, directories, URLs,
//! tolerant parsing with upstream's `parse_error.log`, per-document
//! blank-node scopes, named graphs).

pub mod import;
pub mod jsonld;
pub mod nquads;
pub mod ntriples;
pub mod rdfa;
pub mod rdfxml;
pub mod sparql;
pub mod stream;
pub mod trig;
pub mod turtle;

#[cfg(test)]
mod stream_tests;

use std::path::Path;

use crate::error::Result;
use crate::model::Triple;

/// Parse an RDF file into triples, dispatching on the file extension (see
/// [`import::RdfFormat::from_extension`]; unknown → Turtle, a superset of
/// N-Triples). Quad formats (N-Quads / TriG) are flattened to their triples —
/// the bulk loaders keep the graphs instead ([`import`]). Strict: the first
/// statement that does not parse is an error. Blank-node labels are kept as
/// written. The streaming formats are read with bounded memory, but the
/// result is one vector; prefer [`import::import_source`] for large files.
pub fn parse_file_by_ext<P: AsRef<Path>>(path: P) -> Result<Vec<Triple>> {
    let opts = import::ImportOptions {
        blank_nodes: import::BlankNodes::Keep,
        ..import::ImportOptions::strict()
    };
    let mut out = Vec::new();
    import::import_source(
        &import::Source::File(path.as_ref().to_path_buf()),
        "read",
        &opts,
        &mut (),
        &mut |batch: &mut Vec<nquads::Quad>| {
            out.extend(batch.drain(..).map(nquads::Quad::into_triple));
            Ok(())
        },
    )?;
    Ok(out)
}
