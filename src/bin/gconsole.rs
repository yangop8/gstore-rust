//! `gconsole` — gStore's interactive console: a client of a running
//! `gserver`'s upstream `/api` (as in upstream gStore 1.4), or, with
//! `--local <db>`, the embedded console on a database directory (a
//! gstore-rust extension). See [`gstore::console`] for the commands and
//! options; `gconsole --help` lists the options.
//!
//! ```text
//! gconsole -u root                      # log in to 127.0.0.1:<conf.ini port>
//! gconsole -u root -p 123456 -e "use lubm; select * where {?s ?p ?o} limit 5;"
//! gconsole --local lubm                 # no server: open lubm.db directly
//! ```

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(gstore::console::main(&args));
}
