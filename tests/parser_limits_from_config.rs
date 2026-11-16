//! The parser limits of `[upload]` (review parser#5, parser#8):
//! `rdf_max_token_size` bounds one token of the streaming RDF readers and
//! `sparql_max_size` one SPARQL request. `gserver` and `gbuild` install them
//! as process defaults through `GStoreConfig::install_storage_defaults`
//! (and `/api refreshconf` again).
//!
//! One test: the limits are process-wide, so nothing else runs in this
//! binary while it changes them.

use gstore::config::GStoreConfig;
use gstore::parser::sparql::{self, lexer};
use gstore::parser::{stream, turtle};

#[test]
fn the_upload_section_sets_the_token_and_request_limits() {
    // The built-in defaults, and the shipped configuration's.
    assert_eq!(stream::default_max_token_bytes(), 64 << 20);
    assert_eq!(lexer::max_request_bytes(), 16 << 20);
    let shipped = GStoreConfig::default();
    assert_eq!(
        (shipped.rdf_max_token_size, shipped.sparql_max_size),
        (64 << 20, 16 << 20)
    );

    let cfg = GStoreConfig::from_ini_str("[upload]\nrdf_max_token_size=100\nsparql_max_size=200\n");
    assert!(cfg.warnings().is_empty(), "{:?}", cfg.warnings());
    cfg.install_storage_defaults();
    assert_eq!(stream::default_max_token_bytes(), 100);
    assert_eq!(lexer::max_request_bytes(), 200);

    // A request over the limit is refused before it is parsed; one at the
    // limit is parsed.
    let at_limit = format!("ASK {{}}{}", " ".repeat(200 - 6));
    assert_eq!(at_limit.len(), 200);
    sparql::parse(&at_limit).unwrap();
    let over = format!("{at_limit} ");
    let e = sparql::parse(&over).unwrap_err().to_string();
    assert!(
        e.contains("201 bytes") && e.contains("sparql_max_size"),
        "{e}"
    );

    // The plain readers use the process default.
    let doc = |n: usize| format!("<http://ex/s> <http://ex/p> \"{}\" .\n", "a".repeat(n));
    turtle::parse_str(&doc(100)).unwrap();
    let e = turtle::parse_str(&doc(101)).unwrap_err().to_string();
    assert!(e.contains("rdf_max_token_size"), "{e}");
    // ImportOptions created now take it as their default.
    assert_eq!(
        gstore::parser::import::ImportOptions::strict().max_token_bytes,
        100
    );

    // 0 = no limit.
    let off = GStoreConfig::from_ini_str("[upload]\nrdf_max_token_size=0\nsparql_max_size=0\n");
    off.install_storage_defaults();
    assert_eq!(stream::default_max_token_bytes(), usize::MAX);
    assert_eq!(lexer::max_request_bytes(), usize::MAX);
    turtle::parse_str(&doc(1000)).unwrap();
    sparql::parse(&format!("ASK {{}}{}", " ".repeat(1000))).unwrap();

    // Back to the shipped defaults.
    shipped.install_storage_defaults();
    assert_eq!(stream::default_max_token_bytes(), 64 << 20);
    assert_eq!(lexer::max_request_bytes(), 16 << 20);
}
