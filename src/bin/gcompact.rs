//! `gcompact` — maintain a database's id spaces (Phase 6 id reclamation).
//!
//! ```text
//! gcompact <db_name>                    # show the id spaces
//! gcompact <db_name> --reclaim          # free every unreferenced term id, save
//! gcompact <db_name> --enable-reclaim   # upgrade a pre-Phase-6 kvstore.kv
//! gcompact <db_name> --check            # verify the integrity of the id spaces
//! ```
//!
//! * `--reclaim` runs [`Database::reclaim_unused`] — the ids of terms that no
//!   triple or quad uses any more (orphans a crash, a rolled-back RocksDB
//!   transaction or `id_reclaim=off` left) become free for reuse — and makes
//!   it durable (a snapshot database is saved again).
//! * `--enable-reclaim` turns id reclamation on for a `kvstore.kv` written
//!   before Phase 6 (which keeps it off): the file gets the `FREE_IDS`
//!   feature and becomes version 2, which **binaries older than Phase 6
//!   refuse**. Downgrade: `gexport` with this binary, `gbuild` with the old
//!   one. Every other layout frees ids already.
//! * `--check` runs [`Database::verify_integrity`].
//!
//! Options combine (`--enable-reclaim --reclaim`). The database must not be
//! open in another process (its directory lock is taken). Exit status: 0 done,
//! 1 failed, 2 invalid arguments; `-h`/`--help`.

#[path = "common/cli.rs"]
mod cli;

use std::process::ExitCode;

use gstore::{db_dir_for, Database};

const TOOL: cli::Tool = cli::Tool {
    name: "gcompact",
    usage: "gcompact <db_name> [--reclaim] [--enable-reclaim] [--check]\n\
            gcompact -db <db_name> [--reclaim] [--enable-reclaim] [--check]",
    help: "show and maintain a database's id spaces (id reclamation)\n\
           \n\
           Options:\n\
           \t-h,--help\t\tDisplay this message.\n\
           \t-db,--database NAME\tThe database name (or give it as the argument).\n\
           \t--reclaim\t\tFree the id of every term no triple or quad uses, and save.\n\
           \t--enable-reclaim\tTurn id reclamation on for a kvstore.kv written before\n\
           \t\t\t\tPhase 6 (it becomes version 2: older binaries refuse it).\n\
           \t--check\t\t\tVerify the integrity of the id spaces.",
};

fn main() -> ExitCode {
    let mut argv = cli::argv();
    let mut reclaim = false;
    let mut enable = false;
    let mut check = false;
    argv.retain(|a| match a.as_str() {
        "--reclaim" => {
            reclaim = true;
            false
        }
        "--enable-reclaim" => {
            enable = true;
            false
        }
        "--check" => {
            check = true;
            false
        }
        _ => true,
    });
    let args = match cli::parse(&argv, &[cli::DB]) {
        Ok(cli::Parsed::Help) => {
            TOOL.print_help();
            return ExitCode::SUCCESS;
        }
        Ok(cli::Parsed::Args(a)) => a,
        Err(e) => return TOOL.usage_error(&e),
    };
    if let Some(o) = args.unknown_option() {
        return TOOL.usage_error(&format!("unknown option '{o}'"));
    }
    let name = match (args.value(0), args.rest.as_slice()) {
        (Some(db), []) => db.to_string(),
        (None, [db]) => db.clone(),
        (None, []) => return TOOL.usage_error("missing the database name"),
        _ => return TOOL.usage_error("too many arguments"),
    };
    let dir = db_dir_for(&name);
    let mut db = match Database::open(&dir) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("cannot load database '{dir}': {e}");
            return ExitCode::FAILURE;
        }
    };
    if enable {
        match db.enable_id_reclaim() {
            Ok(true) => gstore::outln!(
                "{name}: id reclamation enabled (kvstore.kv is now version 2; binaries older \
                 than Phase 6 refuse it)"
            ),
            Ok(false) => gstore::outln!("{name}: id reclamation was already possible"),
            Err(e) => {
                eprintln!("{name}: cannot enable id reclamation: {e}");
                return ExitCode::FAILURE;
            }
        }
    }
    if reclaim {
        let freed = match db.reclaim_unused() {
            Ok(n) => n,
            Err(e) => {
                eprintln!("{name}: reclaim failed: {e}");
                return ExitCode::FAILURE;
            }
        };
        // A snapshot database persists the freed ids at its save.
        if let Err(e) = db.checkpoint() {
            eprintln!("{name}: freed {freed} id(s) but could not save: {e}");
            return ExitCode::FAILURE;
        }
        gstore::outln!("{name}: freed {freed} unreferenced id(s)");
    }
    if check {
        if let Err(e) = db.verify_integrity() {
            eprintln!("{name}: {e}");
            return ExitCode::FAILURE;
        }
        gstore::outln!("{name}: integrity ok");
    }
    let s = db.id_space_stats();
    let spaces = ["entity", "literal", "predicate"];
    gstore::outln!(
        "id_reclaim : {}{}",
        s.policy,
        if s.enabled {
            ""
        } else {
            " (off for this kvstore.kv: gcompact --enable-reclaim)"
        }
    );
    for (i, space) in spaces.iter().enumerate() {
        gstore::outln!(
            "{space:<9} : live {} free {} id_limit {}",
            s.live[i],
            s.free[i],
            s.limits[i]
        );
    }
    ExitCode::SUCCESS
}
