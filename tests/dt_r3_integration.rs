//! Data test: the three Phase 6 round-3 packages together — the servers'
//! shared-read / exclusive-write locking (R3-SRV), Stream-mode disk databases
//! and the `[storage]` open options (R3-STREAM, contract C7), and bulk builds
//! of the disk and RocksDB layouts (R2-BULK, R3-ROCKS, contract C6).
//!
//! - A server holds the directory of every database it serves (all layouts,
//!   RocksDB included). `/api build` refuses a loaded database and leaves it
//!   serving; a bulk build of the served directory in the same process is
//!   refused (`DirLock::held_here`) until the server releases the handle
//!   (`unload`), and the rebuilt directory is served again after `load`.
//!   REST `/build` replaces a loaded on-disk database in place.
//! - `refreshconf` re-installs the `[storage]` section as the process's
//!   default open options on a server that owns them (`gserver`), and leaves
//!   them alone on an embedded one.

#[path = "upstream_scripts/harness.rs"]
mod harness;

use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::thread;

use gstore::config::GStoreConfig;
use gstore::db::dirlock::DirLock;
use gstore::http_api::ApiServer;
use gstore::json::{self, Json};
use gstore::{default_open_options, set_default_open_options, Database, OpenOptions};
use harness::{code, int, ok, scratch, string, Server};

/// A scratch directory removed when dropped (also when the test fails).
struct Scratch(std::path::PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Three triples, then five other ones.
const ONE: &str = "<http://ex/a> <http://ex/p> <http://ex/b> .\n\
                   <http://ex/a> <http://ex/p> <http://ex/c> .\n\
                   <http://ex/b> <http://ex/q> \"one\" .\n";
const TWO: &str = "<http://ex/x> <http://ex/p> <http://ex/y> .\n\
                   <http://ex/x> <http://ex/p> <http://ex/z> .\n\
                   <http://ex/y> <http://ex/q> \"two\" .\n\
                   <http://ex/z> <http://ex/q> \"two\" .\n\
                   <http://ex/x> <http://ex/r> <http://ex/a> .\n";

/// The number of answers of `SELECT * { ?s ?p ?o }` on loaded `db`.
fn answers(srv: &Server, db: &str) -> i64 {
    let j = srv.query(db, "SELECT * WHERE { ?s ?p ?o }");
    ok(&j);
    int(&j, "AnsNum")
}

/// What every layout's lifecycle shares: `dir` (`<root>/dsk.db`) holds a
/// database built from `one` by `bulk_build`; the server serves it, refuses
/// `/api build` over it, and a bulk build of its directory waits for
/// `unload`.
fn served_database_is_rebuilt_only_after_unload(
    srv: &Server,
    dir: &Path,
    two: &Path,
    bulk_build: &dyn Fn(&Path, &Path) -> gstore::Result<()>,
) {
    srv.load("dsk");
    assert_eq!(answers(srv, "dsk"), 3);
    assert!(
        DirLock::held_here(dir),
        "the served database holds its directory"
    );

    // `/api build` of a loaded database: refused, the database untouched.
    let j = srv.build("dsk", two);
    assert_eq!(code(&j), 1004, "{j}");
    assert_eq!(string(&j, "StatusMsg"), "database already built.");
    assert!(!dir.with_extension("db.build-tmp").exists());
    assert_eq!(answers(srv, "dsk"), 3);

    // A bulk build of the served directory: refused before anything is
    // staged, while the server holds it.
    let e = bulk_build(dir, two).unwrap_err().to_string();
    assert!(e.contains("open in this process"), "{e}");
    assert!(!dir.join(gstore::bulk::STAGING_DIR).exists());
    assert_eq!(answers(srv, "dsk"), 3);

    // Released by `unload`: the build replaces the directory, and `load`
    // serves the new database.
    srv.unload("dsk");
    assert!(!DirLock::held_here(dir), "unload releases the directory");
    bulk_build(dir, two).unwrap();
    srv.load("dsk");
    assert_eq!(answers(srv, "dsk"), 5);
    assert!(DirLock::held_here(dir));
}

#[test]
fn a_served_disk_database_is_rebuilt_only_after_unload() {
    let data = scratch("r3_build_data");
    let _data = Scratch(data.clone());
    let (one, two) = (data.join("one.nt"), data.join("two.nt"));
    fs::write(&one, ONE).unwrap();
    fs::write(&two, TWO).unwrap();
    let root = scratch("r3_build_disk");
    let dir = root.join("dsk.db");
    Database::build_disk(&dir, &[&one]).unwrap();
    let srv = Server::start_at(root, Some(&data));
    served_database_is_rebuilt_only_after_unload(&srv, &dir, &two, &|dir, file| {
        Database::build_disk(dir, &[file])
    });
    assert!(Database::is_disk(&dir));

    // REST `/build` rebuilds a loaded database in place: the loaded copy is
    // retired first (its directory handle released), the new one is served.
    let (status, body) = srv.rest(
        "POST",
        "/build?db_name=dsk",
        "<http://ex/n> <http://ex/p> <http://ex/m> .",
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(answers(&srv, "dsk"), 1);
    assert!(!Database::is_disk(&dir), "replaced by the rebuilt snapshot");
    assert!(DirLock::held_here(&dir));
    srv.unload("dsk");
    assert!(!DirLock::held_here(&dir));
    drop(srv);
}

#[cfg(feature = "rocksdb")]
#[test]
fn a_served_rocksdb_database_is_rebuilt_only_after_unload() {
    use gstore::bulk::{self, BulkInput, BulkOptions, BulkTarget};
    use gstore::parser::import::{self, ImportOptions};

    let bulk_rocks = |dir: &Path, file: &Path| -> gstore::Result<()> {
        bulk::build(
            dir,
            "dsk",
            BulkInput::Sources(&import::sources_from_paths(&[file])?),
            &ImportOptions::strict(),
            &mut (),
            BulkTarget::RocksDb,
            &BulkOptions::default(),
        )
        .map(|_| ())
    };
    let data = scratch("r3_build_rocks_data");
    let _data = Scratch(data.clone());
    let (one, two) = (data.join("one.nt"), data.join("two.nt"));
    fs::write(&one, ONE).unwrap();
    fs::write(&two, TWO).unwrap();
    let root = scratch("r3_build_rocks");
    let dir = root.join("dsk.db");
    bulk_rocks(&dir, &one).unwrap();
    let srv = Server::start_at(root, Some(&data));
    served_database_is_rebuilt_only_after_unload(&srv, &dir, &two, &bulk_rocks);
    assert!(Database::is_rocksdb(&dir));
    srv.unload("dsk");
    drop(srv);
}

/// The open options `Database::open` uses after `[storage]` of `cfg` is
/// installed: the section with the environment on top.
fn installed(cfg: &GStoreConfig) -> OpenOptions {
    let mut options = cfg.open_options();
    let _ = options.apply_env();
    options
}

/// Start an `ApiServer` from configuration file `conf`; `owns` makes it
/// re-install `[storage]` on `refreshconf` (as `gserver` does).
fn start_from(conf: &Path, owns: bool) -> (Arc<ApiServer>, std::net::SocketAddr) {
    let mut cfg = GStoreConfig::load(conf).unwrap();
    assert!(cfg.warnings().is_empty(), "{:?}", cfg.warnings());
    cfg.listen_addr = "127.0.0.1".into();
    cfg.root_password = harness::PASSWORD.into();
    let server = ApiServer::bind_with_config(cfg).unwrap();
    let server = Arc::new(if owns {
        server.with_storage_refresh()
    } else {
        server
    });
    let addr = server.local_addr().unwrap();
    let s2 = Arc::clone(&server);
    thread::spawn(move || s2.serve_forever());
    (server, addr)
}

fn refreshconf(addr: &std::net::SocketAddr) {
    let body = Json::obj()
        .with("operation", "refreshconf")
        .with("username", harness::USERNAME)
        .with("password", harness::PASSWORD);
    let (status, text) = harness::http(addr, "POST", "/api", body.to_string().as_bytes());
    assert_eq!(status, 200, "{text}");
    ok(&json::parse(&text).unwrap());
}

#[test]
fn refreshconf_reinstalls_the_storage_section_of_a_server_that_owns_it() {
    let dir = scratch("r3_refresh_storage");
    let _dir = Scratch(dir.clone());
    // The process's defaults go back to the built-in ones however the test
    // ends.
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            set_default_open_options(OpenOptions::default());
        }
    }
    let _reset = Reset;
    let conf = dir.join("conf.ini");
    let write = |home: &str, storage: &str| {
        fs::write(
            &conf,
            format!(
                "[server]\nport=0\nip_deny_path=off\nthread_num=2\n\
                 [system]\ndb_home={}\n\
                 [log]\nlog_mode=none\nlog_stderr=off\n\
                 [storage]\n{storage}",
                dir.join(home).display()
            ),
        )
        .unwrap();
    };
    // Only cache sizes change here: the process's other tests open disk
    // databases meanwhile, and their answers do not depend on them.
    write("home_a", "disk_cache_mb=3\ndict_cache_mb=6\n");
    // What gserver does at startup.
    let first = GStoreConfig::load(&conf).unwrap();
    set_default_open_options(first.open_options());
    assert_eq!(default_open_options(), installed(&first));
    let (owner, owner_addr) = start_from(&conf, true);

    write("home_a", "disk_cache_mb=5\ndict_cache_mb=7\n");
    refreshconf(&owner_addr);
    let second = GStoreConfig::load(&conf).unwrap();
    assert_eq!(default_open_options(), installed(&second));
    assert_eq!(default_open_options().dict_cache_bytes, 7 << 20);

    // An embedded server (not built `with_storage_refresh`) re-reads the
    // file but leaves the process's open options alone.
    write("home_b", "disk_cache_mb=9\ndict_cache_mb=11\n");
    let (embedded, embedded_addr) = start_from(&conf, false);
    refreshconf(&embedded_addr);
    assert_eq!(default_open_options(), installed(&second));

    owner.shutdown();
    embedded.shutdown();
}
