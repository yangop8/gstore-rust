//! Data test: the three Phase 6 round-3 packages together — the servers'
//! shared-read / exclusive-write locking (R3-SRV), Stream-mode disk databases
//! and the `[storage]` open options (R3-STREAM, contract C7), and bulk builds
//! of the disk and RocksDB layouts (R2-BULK, R3-ROCKS, contract C6).
//!
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
use gstore::http_api::ApiServer;
use gstore::json::{self, Json};
use gstore::{default_open_options, set_default_open_options, OpenOptions};
use harness::{ok, scratch};

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
    set_default_open_options(OpenOptions::default());
    let _ = fs::remove_dir_all(&dir);
}
