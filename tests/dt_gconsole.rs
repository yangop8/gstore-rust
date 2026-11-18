//! `gconsole`, the upstream-compatible console: the binary is driven through
//! stdin scripts, `-e` and (for the prompts) `-i` against a server — an
//! in-process [`ApiServer`](gstore::http_api::ApiServer) on an ephemeral
//! port, a `gserver` binary started from a directory with a
//! `conf/conf.ini` (the README flow), or a scripted fake server (exact
//! request bodies, Ctrl-C) — and its output and the server's state are
//! checked for every command family.

#[path = "upstream_scripts/harness.rs"]
mod harness;

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use gstore::json::{self, Json};
use harness::{data, ok, path_str, scratch, Server, PASSWORD};

// ---- running gconsole ----------------------------------------------------------

/// What a finished gconsole run printed.
struct Run {
    code: i32,
    out: String,
    err: String,
}

impl std::fmt::Debug for Run {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "exit {}\n--- stdout ---\n{}\n--- stderr ---\n{}",
            self.code, self.out, self.err
        )
    }
}

/// A gconsole command for the server at `addr`, run in `cwd` with no
/// history file.
fn gconsole(addr: &SocketAddr, cwd: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_gconsole"));
    cmd.current_dir(cwd)
        .env("GSTORE_HISTFILE", "")
        .env_remove("GSTORE_PASSWORD")
        .args(["--host", "127.0.0.1", "--port", &addr.port().to_string()]);
    cmd
}

/// Run `cmd` with `stdin` as its input, to completion (bounded).
fn run(mut cmd: Command, stdin: &str) -> Run {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn gconsole");
    let mut input = child.stdin.take().unwrap();
    let text = stdin.to_string();
    let writer = thread::spawn(move || {
        let _ = input.write_all(text.as_bytes());
    });
    let (out, err) = collect(&mut child);
    let _ = writer.join();
    let code = wait(&mut child);
    Run {
        code,
        out: out.join().unwrap(),
        err: err.join().unwrap(),
    }
}

type Reader = thread::JoinHandle<String>;

/// Readers of the child's stdout and stderr.
fn collect(child: &mut Child) -> (Reader, Reader) {
    let mut out = child.stdout.take().unwrap();
    let mut err = child.stderr.take().unwrap();
    (
        thread::spawn(move || {
            let mut s = String::new();
            let _ = out.read_to_string(&mut s);
            s
        }),
        thread::spawn(move || {
            let mut s = String::new();
            let _ = err.read_to_string(&mut s);
            s
        }),
    )
}

/// The child's exit code (bounded wait).
fn wait(child: &mut Child) -> i32 {
    let t0 = Instant::now();
    loop {
        if let Some(st) = child.try_wait().unwrap() {
            return st.code().unwrap_or(-1);
        }
        if t0.elapsed() > Duration::from_secs(120) {
            let _ = child.kill();
            panic!("gconsole did not finish");
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// Run a stdin script as root.
fn script(srv: &Server, cwd: &Path, stdin: &str) -> Run {
    script_at(&srv.addr, cwd, stdin)
}

/// Run a stdin script as root against the server at `addr`.
fn script_at(addr: &SocketAddr, cwd: &Path, stdin: &str) -> Run {
    let mut cmd = gconsole(addr, cwd);
    cmd.args(["-u", "root", "-p", PASSWORD]);
    run(cmd, stdin)
}

fn small_nt() -> String {
    path_str(&data("small/small.nt"))
}

/// The databases the server knows (`show` as root).
fn databases(srv: &Server) -> Vec<String> {
    let j = srv.op("show", &[]);
    ok(&j);
    j.get("ResponseBody")
        .and_then(Json::as_array)
        .unwrap_or_default()
        .iter()
        .filter_map(|d| d.get("database").and_then(Json::as_str).map(str::to_string))
        .collect()
}

fn wait_for(what: &str, cond: impl Fn() -> bool) {
    let t0 = Instant::now();
    while !cond() {
        assert!(
            t0.elapsed() < Duration::from_secs(30),
            "timed out waiting for {what}"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

/// Removes a directory when dropped.
struct DirGuard(PathBuf);

impl Drop for DirGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

// ---- against the server ----------------------------------------------------------

#[test]
fn database_lifecycle_commands() {
    let srv = Server::start("gc_lifecycle");
    let cwd = scratch("gc_lifecycle_cwd");
    let _cwd = DirGuard(cwd.clone());
    let r = script(
        &srv,
        &cwd,
        &format!(
            "create small {nt};\n\
             create sm;\n\
             create system;\n\
             create empty_db;\n\
             showdbs;\n\
             pdb;\n\
             use small;\n\
             pdb;\n\
             show;\n\
             show empty_db;\n\
             drop small;\n\
             backup;\n\
             export;\n\
             unload;\n\
             drop empty_db;\n\
             use missing_db;\n\
             create small {nt};\n",
            nt = small_nt()
        ),
    );
    let out = &r.out;
    // Some commands failed: exit 1, and every command still ran.
    assert_eq!(r.code, 1, "{r:?}");
    for line in [
        "Build RDF database small successfully! use ",
        "build param error, need param num at least 1 or database name need at least 3 character\n",
        "Your db name can NOT be \"system\".\n",
        "Build RDF database empty_db successfully! use ",
        "| database | creater |      builtTime      |    status     |\n",
        "Current database not selected. Please select it first, through \"USE <database_name>\".\n",
        "Current database switch to small successfully.\n\nsmall\n\n",
        "|     name     |        value        |\n",
        "| triple_num   | 25                  |\n",
        "| database     | empty_db            |\n",
        "You can NOT drop current database. Please UNLOAD current database through \"UNLOAD <database_name>;\" before you drop it.\n",
        "Database small backup successfully.\nBackup path: ",
        "Database small exported successfully.\nExport file: ",
        "Unload database small successfully.\n",
        "Drop database empty_db successfully! \n",
        "Load database missing_db failed: database[missing_db] does not exist.\n",
        "Build RDF database small failed: database already built.\n",
    ] {
        assert!(out.contains(line), "{line:?}: {r:?}");
    }
    // The server's state follows.
    let dbs = databases(&srv);
    assert!(dbs.contains(&"small".to_string()), "{dbs:?}");
    assert!(!dbs.contains(&"empty_db".to_string()), "{dbs:?}");
    assert_eq!(fs::read_dir(srv.root.join("backups")).unwrap().count(), 1);
    let exported: Vec<_> = fs::read_dir(srv.root.join("export"))
        .unwrap()
        .flatten()
        .collect();
    assert_eq!(exported.len(), 1);
    let nt = fs::read_to_string(exported[0].path()).unwrap();
    assert_eq!(nt.lines().count(), 25);

    // restore (from the backup path the console printed), batch insert /
    // remove.
    let backup_path = out
        .lines()
        .find_map(|l| l.strip_prefix("Backup path: "))
        .unwrap()
        .to_string();
    let add = path_str(&data("small/small_add.nt"));
    let r = script(
        &srv,
        &cwd,
        &format!(
            "use small;\n\
             batchinsert {add};\n\
             select (count(*) as ?n) where {{ ?s ?p ?o }};\n\
             restore small {backup_path};\n\
             unload;\n\
             restore small {backup_path}/;\n\
             use small;\n\
             batchremove {add};\n\
             batchinsert;\n\
             select (count(*) as ?n) where {{ ?s ?p ?o }};\n"
        ),
    );
    let out = &r.out;
    let int = |n: usize| format!("\"{n}\"^^<http://www.w3.org/2001/XMLSchema#integer>");
    for line in [
        "After inserted triples num 6,failed num 0,used ".to_string(),
        int(31),
        "Database small restore failed: Database alreay load, need unload it first through \"UNLOAD <database_name>;\" before you restore it\n".to_string(),
        "Database small restored successfully.\n".to_string(),
        // Restored: the added triples are gone, removing them removes none.
        "After removed triples num 0,failed num 0,used ".to_string(),
        "Wrong usage.\nUsage: batchinsert <nt_file_path>;\n".to_string(),
        int(25),
    ] {
        assert!(out.contains(&line), "{line:?}: {r:?}");
    }
    assert_eq!(r.code, 1, "{r:?}");
}

/// Upstream's console has no compress switch on `backup` / `export`, but
/// its `restore <db> <path>` hands the path to the server, which takes a
/// zipped backup (`backup compress=true`, `gbackup -z`) as well as a
/// directory (feature `zip`).
#[cfg(feature = "zip")]
#[test]
fn restore_takes_a_zipped_backup() {
    let srv = Server::start("gc_zip_restore");
    let cwd = scratch("gc_zip_restore_cwd");
    let _cwd = DirGuard(cwd.clone());
    ok(&srv.build("small", &data("small/small.nt")));
    let j = srv.op("backup", &[("db_name", "small"), ("compress", "true")]);
    ok(&j);
    let archive = harness::string(&j, "backupfilepath");
    assert!(archive.ends_with(".zip"), "{j:?}");
    let add = path_str(&data("small/small_add.nt"));
    let r = script(
        &srv,
        &cwd,
        &format!(
            "use small;\n\
             batchinsert {add};\n\
             unload;\n\
             restore small {archive};\n\
             use small;\n\
             select (count(*) as ?n) where {{ ?s ?p ?o }};\n"
        ),
    );
    for line in [
        "After inserted triples num 6,failed num 0,used ",
        "Database small restored successfully.\n",
        "\"25\"^^<http://www.w3.org/2001/XMLSchema#integer>",
    ] {
        assert!(r.out.contains(line), "{line:?}: {r:?}");
    }
    assert_eq!(r.code, 0, "{r:?}");
}

#[test]
fn sparql_statements_tables_and_multi_line_input() {
    let srv = Server::start("gc_sparql");
    ok(&srv.build("small", &data("small/small.nt")));
    let cwd = scratch("gc_sparql_cwd");
    let _cwd = DirGuard(cwd.clone());
    fs::write(
        cwd.join("q.sparql"),
        "# two statements\nselect ?o where { <root> <name> ?o };\n\
         ASK { <root> <contain> <node0> }\n",
    )
    .unwrap();
    let r = script(
        &srv,
        &cwd,
        "select * where { ?s ?p ?o };\n\
         use small;\n\
         # a comment line; not a statement\n\
         SELECT ?x ?n\n\
         WHERE {\n\
         \x20 ?x <name> ?n . # trailing comment ; with a semicolon\n\
         \x20 FILTER(?n != \"x;y\")\n\
         } ORDER BY ?x;\n\
         insert data { <new> <name> \"a;b\" . <new> <age> 3 };\n\
         select ?n ?a where { <new> <name> ?n . optional { <new> <nope> ?a } };\n\
         delete where { <new> ?p ?o };\n\
         select * where { <new> ?p ?o };\n\
         select where;\n\
         sparql q.sparql;\n\
         sparql missing.sparql;\n",
    );
    let out = &r.out;
    let table = "\
------------------------------------------
|    ?x    |             ?n              |
------------------------------------------
| <point1> | \"sbsb 12.879\"               |
| <point5> | \"tobe NO1.\"                 |
| <point7> | \"sadhjb wdhcbd sdcjhdsjkc1\" |
| <root>   | \"Bookug Lobert\"             |
------------------------------------------
Query ans num 4, use ";
    for line in [
        "Current database not selected. Please select it first",
        table,
        "Update ans num 2, use ",
        // An unbound cell is an empty cell.
        "|  ?n   | ?a |\n--------------\n| \"a;b\" |    |\n",
        "Query ans num 0, use ",
        "Query failed: ",
        // `sparql <file>`: both statements of the file.
        "| \"Bookug Lobert\" |\n",
        "?_askResult",
        "File open failed: missing.sparql\n",
    ] {
        assert!(out.contains(line), "{line:?}: {r:?}");
    }
    assert_eq!(out.matches("Update ans num 2, use ").count(), 2, "{r:?}");
    assert_eq!(r.code, 1, "{r:?}");
    // The update reached the server and was undone.
    let j = srv.query("small", "select * where { <new> ?p ?o }");
    ok(&j);
    assert_eq!(harness::int(&j, "AnsNum"), 0);
}

/// Phases 2 and 3 through the console: every SPARQL form the server
/// accepts is sent as SPARQL (Phase 3's graph-management and named-graph
/// updates, `LOAD … INTO GRAPH`, `CONSTRUCT WHERE`, a `PREFIX` / `BASE`
/// prologue before any of them), while upstream's console commands that
/// share a first word (`drop <db>`, `create <db> <file>`, `clear`) stay
/// commands; `sparql <file>` sends a run of the new update forms as one
/// request; a graph function's JSON is shown as upstream's console shows it.
#[test]
fn phase3_update_forms_graph_functions_and_console_commands() {
    let data_dir = scratch("gc_phase3_data");
    let _data = DirGuard(data_dir.clone());
    let base = data_dir.join("base.nt");
    fs::write(
        &base,
        "<http://ex/a> <http://ex/p> <http://ex/b> .\n<http://ex/b> <http://ex/p> <http://ex/c> .\n",
    )
    .unwrap();
    let extra = data_dir.join("extra.trig");
    fs::write(
        &extra,
        "<http://ex/x> <http://ex/p> <http://ex/y> .\n<http://ex/n> { <http://ex/q> <http://ex/p> <http://ex/r> . }\n",
    )
    .unwrap();
    let srv = Server::start_with_dir("gc_phase3", Some(&data_dir));
    ok(&srv.build("gcx", &base));
    let cwd = scratch("gc_phase3_cwd");
    let _cwd = DirGuard(cwd.clone());
    fs::write(
        cwd.join("updates.sparql"),
        "PREFIX ex: <http://ex/>\n\
         COPY ex:g TO ex:f ;\n\
         MOVE SILENT ex:f TO ex:moved ;\n\
         WITH ex:moved DELETE { ?s ?p ex:c } WHERE { ?s ?p ex:c } ;\n\
         CREATE GRAPH ex:e ;\n\
         DROP SILENT GRAPH ex:e ;\n\
         SELECT (COUNT(*) AS ?n) WHERE { GRAPH <http://ex/moved> { ?s ?p ?o } }\n",
    )
    .unwrap();
    let extra_s = path_str(&extra);
    let base_s = path_str(&base);
    let r = script(
        &srv,
        &cwd,
        &format!(
            "use gcx;\n\
             COPY DEFAULT TO <http://ex/g>;\n\
             ADD <http://ex/g> TO <http://ex/h>;\n\
             MOVE <http://ex/h> TO <http://ex/m>;\n\
             WITH <http://ex/m> DELETE {{ ?s ?p <http://ex/c> }} INSERT {{ ?s ?p <http://ex/z> }} WHERE {{ ?s ?p <http://ex/c> }};\n\
             LOAD <file://{extra_s}> INTO GRAPH <http://ex/l>;\n\
             CREATE GRAPH <http://ex/e>;\n\
             PREFIX ex: <http://ex/> ADD ex:l TO ex:k;\n\
             BASE <http://ex/> CLEAR GRAPH <k>;\n\
             DROP GRAPH <http://ex/m>;\n\
             CONSTRUCT WHERE {{ ?s <http://ex/p> <http://ex/b> }};\n\
             select (shortestPath(<http://ex/a>, <http://ex/c>, true, {{}}) as ?y) where {{}};\n\
             select ?x (shortestPathLen(<http://ex/a>, <http://ex/c>, true, {{}}) as ?y) where {{ values ?x {{ <http://ex/a> }} }};\n\
             sparql updates.sparql;\n\
             unload;\n\
             create gcy {base_s};\n\
             clear;\n\
             drop gcy;\n\
             showdbs;\n"
        ),
    );
    let out = &r.out;
    assert_eq!(r.code, 0, "{r:?}");
    assert!(!out.contains("failed"), "{r:?}");
    assert!(!out.contains("No such command"), "{r:?}");
    // Each update answered as an update.
    assert!(out.matches("Update ans num ").count() >= 9, "{r:?}");
    // CONSTRUCT WHERE.
    assert!(
        out.contains("| <http://ex/a> | <http://ex/p> | <http://ex/b> |"),
        "{r:?}"
    );
    // A lone graph function: its JSON, indented as nlohmann's dump(4).
    assert!(
        out.contains("{\n    \"paths\": [\n        {\n"),
        "the graph function's JSON is pretty-printed: {r:?}"
    );
    // Next to another column: the cell is upstream's, the JSON unescaped.
    assert!(
        out.contains("| <http://ex/a> | \"{\"paths\":[{\"src\":\"<http://ex/a>\",\"dst\":\"<http://ex/c>\",\"length\":2}]}\" |"),
        "{r:?}"
    );
    // `sparql <file>`: the five updates went as one request, then the query.
    assert!(
        out.contains("\"1\"^^<http://www.w3.org/2001/XMLSchema#integer>"),
        "{r:?}"
    );
    // The console commands.
    assert!(
        out.contains("Build RDF database gcy successfully!"),
        "{r:?}"
    );
    assert!(
        out.contains("\x1b[H\x1b[2J"),
        "clear clears the screen: {r:?}"
    );
    assert!(out.contains("Drop database gcy successfully!"), "{r:?}");
    let dbs = databases(&srv);
    assert!(
        dbs.contains(&"gcx".to_string()) && !dbs.contains(&"gcy".to_string()),
        "{dbs:?}"
    );

    // The server's graphs after it all.
    srv.load("gcx");
    let graph = |g: &str| {
        let j = srv.query(
            "gcx",
            &format!("SELECT ?s ?p ?o WHERE {{ GRAPH <http://ex/{g}> {{ ?s ?p ?o }} }}"),
        );
        ok(&j);
        harness::int(&j, "AnsNum")
    };
    assert_eq!(graph("g"), 2, "COPY DEFAULT TO g");
    assert_eq!(graph("h"), 0, "MOVE emptied h");
    assert_eq!(graph("m"), 0, "DROP GRAPH m");
    assert_eq!(graph("l"), 1, "LOAD INTO l: only the file's default graph");
    assert_eq!(graph("n"), 0, "LOAD INTO l loads no other graph");
    assert_eq!(graph("k"), 0, "CLEAR GRAPH k (BASE-resolved)");
    assert_eq!(
        graph("moved"),
        1,
        "COPY g, MOVE, WITH DELETE in one request"
    );
    assert_eq!(graph("f"), 0, "moved away");
}

#[test]
fn output_redirection() {
    let srv = Server::start("gc_redirect");
    ok(&srv.build("small", &data("small/small.nt")));
    let cwd = scratch("gc_redirect_cwd");
    let _cwd = DirGuard(cwd.clone());
    let r = script(
        &srv,
        &cwd,
        "use small;\n\
         select ?o where { <root> <contain> ?o } > out.txt;\n\
         pdb >> out.txt;\n\
         showdbs > dbs.txt;\n\
         select ?o where { <root> <name> ?o FILTER(strlen(?o) > 3) };\n\
         pdb > no_such_dir/x.txt;\n",
    );
    let out = &r.out;
    assert!(out.contains("Redirect output to file: out.txt\n"), "{r:?}");
    assert!(
        !out.contains("<node0>"),
        "the table went to the file: {r:?}"
    );
    let file = fs::read_to_string(cwd.join("out.txt")).unwrap();
    assert!(file.starts_with("-----------\n|   ?o    |\n"), "{file}");
    assert!(file.contains("| <node4> |"), "{file}");
    assert!(file.contains("Query ans num 5, use "), "{file}");
    // `>>` appended the current database's name (and upstream's blank line).
    assert!(file.ends_with(" ms.\n\nsmall\n\n"), "{file:?}");
    let dbs = fs::read_to_string(cwd.join("dbs.txt")).unwrap();
    assert!(dbs.contains("| small    |"), "{dbs}");
    // A `>` inside a FILTER is not a redirection.
    assert!(out.contains("| \"Bookug Lobert\" |"), "{r:?}");
    assert!(
        out.contains("Failed to open file: no_such_dir/x.txt"),
        "{r:?}"
    );
    assert_eq!(r.code, 1, "{r:?}");
}

#[test]
fn exec_mode_and_exit_codes() {
    let srv = Server::start("gc_exec");
    ok(&srv.build("small", &data("small/small.nt")));
    let cwd = scratch("gc_exec_cwd");
    let _cwd = DirGuard(cwd.clone());
    // Several statements; the last one without its `;`.
    let mut cmd = gconsole(&srv.addr, &cwd);
    cmd.args([
        "-u",
        "root",
        "-p",
        PASSWORD,
        "-e",
        "use small; select ?o where { <root> <name> ?o }; pdb",
    ]);
    let r = run(cmd, "");
    assert_eq!(r.code, 0, "{r:?}");
    assert!(
        r.out
            .starts_with("Current database switch to small successfully.\n\n"),
        "no banner, no prompt: {r:?}"
    );
    assert!(r.out.contains("| \"Bookug Lobert\" |"), "{r:?}");
    assert!(r.out.ends_with("small\n\n"), "{r:?}");
    assert!(!r.out.contains("gStore["), "{r:?}");
    // A failing command: exit 1.
    let mut cmd = gconsole(&srv.addr, &cwd);
    cmd.args(["-u", "root", "-p", PASSWORD, "-e", "use nothere;"]);
    let r = run(cmd, "");
    assert_eq!(r.code, 1, "{r:?}");
    assert!(r.out.contains("Load database nothere failed: "), "{r:?}");
    // Root cannot select the system database (nothing could query it).
    let mut cmd = gconsole(&srv.addr, &cwd);
    cmd.args(["-u", "root", "-p", PASSWORD, "-e", "use system;"]);
    let r = run(cmd, "");
    assert_eq!(r.code, 1, "{r:?}");
    assert!(
        r.out.contains("system database can NOT be queried"),
        "{r:?}"
    );
    // `clear <arg>` clears the screen, as upstream — never SPARQL `CLEAR`
    // of the current database.
    let mut cmd = gconsole(&srv.addr, &cwd);
    cmd.args([
        "-u",
        "root",
        "-p",
        PASSWORD,
        "-e",
        "use small; clear all; clear default; select * where { ?s ?p ?o };",
    ]);
    let r = run(cmd, "");
    assert_eq!(r.code, 0, "{r:?}");
    assert!(!r.out.contains("Update ans num"), "{r:?}");
    assert!(r.out.contains("Query ans num 25, use "), "{r:?}");
    // `quit;` ends a script: later statements do not run.
    let r = script(&srv, &cwd, "pwd;\nquit;\nuse small;\n");
    assert_eq!(r.code, 0, "{r:?}");
    let shown = fs::canonicalize(&cwd).unwrap();
    assert_eq!(r.out, format!("{}\n\n", shown.display()), "{r:?}");
    // The password from the environment; the user name and the password
    // from stdin when not given.
    let mut cmd = gconsole(&srv.addr, &cwd);
    cmd.args(["-u", "root", "-e", "showdbs;"])
        .env("GSTORE_PASSWORD", PASSWORD);
    let r = run(cmd, "");
    assert_eq!(r.code, 0, "{r:?}");
    let r = run(
        gconsole(&srv.addr, &cwd),
        &format!("root\n{PASSWORD}\nshowdbs;\n"),
    );
    assert_eq!(r.code, 0, "{r:?}");
    assert!(r.out.contains("| small    |"), "{r:?}");
    // Ctrl-D: the end of the input ends the console.
    let r = script(&srv, &cwd, "use small;\nselect * where { ?s ?p ?o }");
    assert_eq!(r.code, 0, "the final statement may lack its ';': {r:?}");
    assert!(r.out.contains("Query ans num 25, use "), "{r:?}");
}

#[test]
fn wrong_password_and_unreachable_server() {
    let srv = Server::start("gc_login");
    let cwd = scratch("gc_login_cwd");
    let _cwd = DirGuard(cwd.clone());
    let mut cmd = gconsole(&srv.addr, &cwd);
    cmd.args(["-u", "root", "-p", "wrong", "-e", "showdbs;"]);
    let r = run(cmd, "");
    assert_eq!(r.code, 1, "{r:?}");
    assert_eq!(
        r.out, "login failed: Username or password is wrong.\n",
        "{r:?}"
    );
    // Scripted credentials are not retried: the next line is not a user.
    let r = run(
        gconsole(&srv.addr, &cwd),
        "root\nwrong\nroot\n123456\nshowdbs;\n",
    );
    assert_eq!(r.code, 1, "{r:?}");
    assert_eq!(r.out.matches("login failed").count(), 1, "{r:?}");
    assert!(!r.out.contains("| database"), "{r:?}");
    // An unknown user.
    let mut cmd = gconsole(&srv.addr, &cwd);
    cmd.args(["-u", "nobody", "-p", "x", "-e", "showdbs;"]);
    let r = run(cmd, "");
    assert_eq!(r.code, 1, "{r:?}");
    assert!(r.out.starts_with("login failed: "), "{r:?}");
    // Nothing listening: port 0, which nothing can listen on (review
    // tests-a#7: a port bound and freed again could be taken meanwhile by
    // another test's server, which gconsole would then talk to).
    let nothing: SocketAddr = ([127, 0, 0, 1], 0).into();
    let mut cmd = gconsole(&nothing, &cwd);
    cmd.args(["-u", "root", "-p", PASSWORD]);
    let r = run(cmd, "showdbs;\n");
    assert_eq!(r.code, 1, "{r:?}");
    assert_eq!(
        r.out, "Could not connect to server. Please check server status\n",
        "{r:?}"
    );
    assert!(r.err.contains("cannot connect to 127.0.0.1:0"), "{r:?}");
    // Bad options.
    let mut cmd = gconsole(&srv.addr, &cwd);
    cmd.arg("--bogus");
    let r = run(cmd, "");
    assert_eq!(r.code, 2, "{r:?}");
    assert!(r.out.starts_with("Wrong usage.\n"), "{r:?}");
}

#[test]
fn users_and_privileges() {
    let srv = Server::start("gc_users");
    ok(&srv.build("small", &data("small/small.nt")));
    let cwd = scratch("gc_users_cwd");
    let _cwd = DirGuard(cwd.clone());
    // A prompt reads the next line of the script: root's password, the new
    // user's, the privilege numbers.
    let r = script(
        &srv,
        &cwd,
        &format!(
            "addusr bob;\n{PASSWORD}\nbobpw\n\
             addusr carl;\nnot-root-pw\n\
             setpriv bob small;\n{PASSWORD}\n1 2 3\n\
             setpriv root small;\n\
             pusr;\n\
             pusr small bob;\n\
             showusrs;\n"
        ),
    );
    let out = &r.out;
    for line in [
        "Add usr bob successfully.\n",
        "Fail to varify your id. User manage failed.\nAdd usr carl failed\n",
        "[will set priv:]00001110\nPrivilege set successfully.\n",
        "Root has all privilege on all databases. No need to set root's privilege.\nPrivilege set failed.\n",
        "usrname: root\n",
        "| bob  | small    | query,load,unload |\n",
        "| root | all      | all               |\n| bob  | small    | query,load,unload |\n",
    ] {
        assert!(out.contains(line), "{line:?}: {r:?}");
    }
    // The server agrees.
    let users = srv.op("showuser", &[]);
    let bob = users
        .get("ResponseBody")
        .and_then(Json::as_array)
        .unwrap()
        .iter()
        .find(|u| u.get("username").and_then(Json::as_str) == Some("bob"))
        .cloned()
        .unwrap();
    assert_eq!(
        bob.get("load_privilege").and_then(Json::as_str),
        Some("small,")
    );
    assert_eq!(bob.get("update_privilege").and_then(Json::as_str), Some(""));

    // As bob: reads work; writes and root-only commands are refused.
    let mut cmd = gconsole(&srv.addr, &cwd);
    cmd.args(["-u", "bob", "-p", "bobpw"]);
    let r = run(
        cmd,
        "use small;\n\
         select ?o where { <root> <name> ?o };\n\
         insert data { <x> <y> <z> };\n\
         showusrs;\n\
         addusr eve;\n\
         unload;\n\
         drop small;\n\
         setpswd;\nbobpw\nnewpw\nnewpw\n\
         pusr small root;\n",
    );
    let out = &r.out;
    for line in [
        "| \"Bookug Lobert\" |\n",
        "Permission denied. Check your privilege with database: small\nQuery failed: You have no update privilege\n",
        "Permission denied. Only root is allowed to view all usrs.\n",
        "Permission denied. Only root is allowed to manage user.\nAdd usr eve failed\n",
        "Unload database small successfully.\n",
        "Permission denied. Check your privilege with database: small\nDrop database small failed: You have no drop privilege\n",
        "Password set successfully.\n",
        "Permission denied. Only root is allowed to view other's privilege.\n",
    ] {
        assert!(out.contains(line), "{line:?}: {r:?}");
    }
    assert_eq!(r.code, 1, "{r:?}");
    // bob's new password works.
    let mut cmd = gconsole(&srv.addr, &cwd);
    cmd.args(["-u", "bob", "-p", "newpw", "-e", "pusr;"]);
    let r = run(cmd, "");
    assert_eq!(r.code, 0, "{r:?}");
    assert_eq!(r.out, "usrname: bob\n\n", "{r:?}");

    // root: reset bob's password, clear his privileges, delete him.
    let r = script(
        &srv,
        &cwd,
        &format!(
            "setpswd bob;\n{PASSWORD}\na\nb\nc\nc\n\
             clearpriv bob;\n\
             clearpriv root;\n\
             pusr small bob;\n\
             delusr bob;\n{PASSWORD}\n\
             delusr bob;\n{PASSWORD}\n"
        ),
    );
    let out = &r.out;
    for line in [
        "Not Matched.\nPassword set successfully.\n",
        "clear bob for all databases privilege successfully!\n",
        "Forbidden Behavior: clear root priv.\n",
        "| bob  | small    | no priv   |\n",
        "Del usr bob successfully.\n",
        "Username not exist, delete user failed.\nDel usr bob failed.\n",
    ] {
        assert!(out.contains(line), "{line:?}: {r:?}");
    }
    let mut cmd = gconsole(&srv.addr, &cwd);
    cmd.args(["-u", "bob", "-p", "c", "-e", "pusr;"]);
    assert_eq!(run(cmd, "").code, 1, "bob is gone");
}

/// `user`'s privileges on `db` by the server's `showuser`: the names of the
/// `*_privilege` fields listing `db`, sorted.
fn privileges_of(srv: &Server, user: &str, db: &str) -> Vec<String> {
    let users = srv.op("showuser", &[]);
    ok(&users);
    let entry = users
        .get("ResponseBody")
        .and_then(Json::as_array)
        .unwrap()
        .iter()
        .find(|u| u.get("username").and_then(Json::as_str) == Some(user))
        .cloned()
        .unwrap();
    let mut privs: Vec<String> = entry
        .as_object()
        .unwrap()
        .iter()
        .filter(|(k, v)| {
            k.ends_with("_privilege")
                && v.as_str()
                    .is_some_and(|l| l.split(',').any(|d| d.trim() == db))
        })
        .map(|(k, _)| k.trim_end_matches("_privilege").to_string())
        .collect();
    privs.sort();
    privs
}

/// review ext#11: `setpriv` refuses an answer that is not a list of
/// privilege numbers — a typo, an out-of-range number, an empty line —
/// and changes nothing; a valid one moves the user's privileges on the
/// database to exactly the given set (granting what is missing, then
/// revoking the rest), and a lone `0` sets none.
#[test]
fn setpriv_refuses_invalid_numbers_and_sets_exactly_the_given_ones() {
    let srv = Server::start("gc_setpriv");
    ok(&srv.build("small", &data("small/small.nt")));
    let cwd = scratch("gc_setpriv_cwd");
    let _cwd = DirGuard(cwd.clone());
    let setpriv = |answer: &str| {
        script(
            &srv,
            &cwd,
            &format!("setpriv bob small;\n{PASSWORD}\n{answer}\n"),
        )
    };
    let r = script(&srv, &cwd, &format!("addusr bob;\n{PASSWORD}\nbobpw\n"));
    assert_eq!(r.code, 0, "{r:?}");
    let r = setpriv("1 2 3");
    assert_eq!(r.code, 0, "{r:?}");
    let granted = ["load", "query", "unload"];
    assert_eq!(privileges_of(&srv, "bob", "small"), granted);
    for (answer, why) in [
        ("q", "not a privilege number (1-8, or 0 for none): q"),
        ("", "no privilege number given"),
        ("2 9", "not a privilege number (1-8, or 0 for none): 9"),
        ("1 x 3", "not a privilege number (1-8, or 0 for none): x"),
        (
            "0 1",
            "0 (no privilege) cannot be combined with other numbers",
        ),
    ] {
        let r = setpriv(answer);
        assert_eq!(r.code, 1, "{answer:?}: {r:?}");
        assert!(
            r.out.contains(&format!("{why}\nPrivilege set failed.\n")),
            "{answer:?}: {r:?}"
        );
        assert!(!r.out.contains("successfully"), "{answer:?}: {r:?}");
        assert_eq!(
            privileges_of(&srv, "bob", "small"),
            granted,
            "{answer:?} changed nothing"
        );
    }
    let r = setpriv("4 1");
    assert_eq!(r.code, 0, "{r:?}");
    assert!(
        r.out
            .contains("[will set priv:]00010010\nPrivilege set successfully.\n"),
        "{r:?}"
    );
    assert_eq!(privileges_of(&srv, "bob", "small"), ["query", "update"]);
    let r = setpriv("8");
    assert_eq!(r.code, 0, "{r:?}");
    assert_eq!(privileges_of(&srv, "bob", "small").len(), 7);
    let r = setpriv("0");
    assert_eq!(r.code, 0, "{r:?}");
    assert!(r.out.contains("[will set priv:]00000000\n"), "{r:?}");
    assert!(privileges_of(&srv, "bob", "small").is_empty());
}

#[test]
fn help_version_and_unknown_commands() {
    let srv = Server::start("gc_help");
    let cwd = scratch("gc_help_cwd");
    let _cwd = DirGuard(cwd.clone());
    let r = script(
        &srv,
        &cwd,
        "help;\nhelp use;\nHELP usage;\n?;\nhelp edit;\nhelp nope;\nversion;\nfoo bar;\n\
         create \"a\"b;\nquit extra;\nflushpriv;\nrefreshconf;\nclear;\nhelp select;\n",
    );
    let out = &r.out;
    for line in [
        "List of all console commands:\n",
        "| quit            | Quit this console.",
        "| raw_sparql      | Support enter sparql query directedly in gconsole.",
        "| Other help arg: |",
        "use\tSet current database.\n\t\tuse <database_name>;\n",
        "Note that all text commands must be end with ';' but need not be in one line.\n",
        "Frequently used GNU Readline shortcuts:\n",
        "nope: Not a supported console command. \nList of gconsole command:\nquit exit refreshconf ",
        "gStore version: 1.4 Source distribution\nCopyright (c) 2016-",
        "foo: No such command for gconsole.\n\n",
        "Invalid arguments!\n",
        "Wrong usage.\nUsage: quit;\n",
        "Privilige Flushed for current user successfully.\n",
        "Refresh config successfully.\n",
        "\x1b[H\x1b[2J",
        "raw_sparql\tSupport enter sparql query directedly in gconsole.\n",
    ] {
        assert!(out.contains(line), "{line:?}: {r:?}");
    }
    // `--help` / `--version`.
    let mut cmd = gconsole(&srv.addr, &cwd);
    cmd.arg("--help");
    let r = run(cmd, "");
    assert_eq!(r.code, 0);
    assert!(
        r.out.contains(
            "Usage: bin/gconsole [OPTIONS]\n  -?, --help          Display this help and exit.\n"
        ),
        "{r:?}"
    );
    let mut cmd = gconsole(&srv.addr, &cwd);
    cmd.arg("-v");
    let r = run(cmd, "");
    assert!(
        r.out
            .starts_with("gStore version: 1.4 Source distribution\n"),
        "{r:?}"
    );
}

/// Root's password on the `gserver` binaries started here. review sec#8 /
/// infra#3: gserver refuses to serve root's default password (123456, the
/// harness's [`PASSWORD`] for in-process servers), so they get their own.
const GSERVER_PASSWORD: &str = "gconsole-pw";

/// A `gserver` binary serving `dir` (its `conf/conf.ini` written from
/// `conf_extra` plus the port, `db_home` and root's password), killed when
/// dropped.
struct Gserver {
    child: Child,
    port: u16,
}

impl Drop for Gserver {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Gserver {
    /// review tests-a#7: gserver binds a port of its own choosing (`-p 0`)
    /// and the test learns it from gserver's own announcement — no port
    /// picked and freed beforehand (another test's server could take it
    /// meanwhile), and no readiness check that any listener would pass.
    fn start(dir: &Path, conf_extra: &str) -> Gserver {
        fs::create_dir_all(dir.join("conf")).unwrap();
        fs::write(
            dir.join("conf/conf.ini"),
            format!(
                "[server]\nthread_num=4\n[system]\ndb_home=./dbhome/\n\
                 root_password={GSERVER_PASSWORD}\n{conf_extra}"
            ),
        )
        .unwrap();
        let data_dir = path_str(&data("small"));
        let (child, port) =
            spawn_gserver(dir, &["--load-dir", &data_dir, "-p", "0"]).expect("gserver started");
        Gserver { child, port }
    }

    /// Run a gconsole stdin script as root, from `cwd`.
    fn script(&self, cwd: &Path, stdin: &str) -> Run {
        let addr: SocketAddr = ([127, 0, 0, 1], self.port).into();
        let mut cmd = gconsole(&addr, cwd);
        cmd.args(["-u", "root", "-p", GSERVER_PASSWORD]);
        run(cmd, stdin)
    }
}

/// Start `gserver` in `dir` with `args` and wait until it announces the
/// address it serves on (its own stdout, so whatever answers there is this
/// server): the child and its port. `None` when it exited first — e.g.
/// because its port was taken. Its output keeps being drained until it
/// exits.
fn spawn_gserver(dir: &Path, args: &[&str]) -> Option<(Child, u16)> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_gserver"))
        .current_dir(dir)
        .args(args)
        .env_remove("GSTORE_GSERVER_BACKGROUND")
        .env_remove("GSTORE_GSERVER_SUPERVISOR")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let out = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(out).lines() {
            let Ok(line) = line else { break };
            if let Some(port) = announced_port(&line) {
                let _ = tx.send(port);
            }
        }
    });
    // The timeout is only a hang guard; an exit ends the wait at once.
    match rx.recv_timeout(Duration::from_secs(120)) {
        Ok(port) => Some((child, port)),
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            None
        }
    }
}

/// review tests-a#7: the readiness check of the gserver binaries started
/// here is gserver's own announcement, not a connection to the port: a
/// port another listener holds is not taken for a ready gserver (gserver
/// exits, and the caller learns it at once).
#[test]
fn a_gserver_is_ready_when_it_says_so_not_when_its_port_answers() {
    let dir = scratch("gc_ready");
    let _dir = DirGuard(dir.clone());
    let taken = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = taken.local_addr().unwrap().port();
    fs::create_dir_all(dir.join("conf")).unwrap();
    fs::write(
        dir.join("conf/conf.ini"),
        format!(
            "[server]\nport={port}\nthread_num=4\n[system]\ndb_home=./dbhome/\n\
             root_password={GSERVER_PASSWORD}\n"
        ),
    )
    .unwrap();
    assert!(
        TcpStream::connect(("127.0.0.1", port)).is_ok(),
        "the old check would have taken this for gserver"
    );
    assert!(spawn_gserver(&dir, &[]).is_none());
    drop(taken);
}

/// The port of gserver's startup line `… on http://127.0.0.1:PORT …`.
fn announced_port(line: &str) -> Option<u16> {
    let (_, rest) = line.split_once(" on http://")?;
    let addr = rest.split_whitespace().next()?;
    addr.parse::<SocketAddr>().ok().map(|a| a.port())
}

/// Whether `hay` holds `lines` in this order (each found after the end of
/// the previous one): the order a script printed its answers in.
fn printed_in_order(hay: &str, lines: &[&str]) -> Result<(), String> {
    let mut at = 0;
    for line in lines {
        match hay[at..].find(line) {
            Some(i) => at += i + line.len(),
            None => return Err(format!("{line:?} not printed after offset {at}")),
        }
    }
    Ok(())
}

/// Phase 4 through the console: a reasoning rule's whole lifecycle —
/// `addreason` from a JSON file, `listreason`, `compilereason`,
/// `executereason`, `showreason`, a refused `deletereason` of an executed
/// rule, `disablereason`, `deletereason` — against a `gserver`, each
/// answer printed as upstream's gconsole prints it, and the inferred
/// `<Rule:…>` triples visible to SPARQL typed at the prompt in between.
#[test]
fn reasoning_commands_run_a_rules_whole_lifecycle() {
    let dir = scratch("gc_reason_life");
    let _dir = DirGuard(dir.clone());
    let srv = Gserver::start(&dir, "");
    fs::write(
        dir.join("rule.json"),
        r#"{"rulename":"holds","description":"a container holds what its nodes own",
            "isenable":1,"type":1,"logic":1,
            "conditions":[{"patterns":[
                {"subject":"?x","predicate":"<contain>","object":"?y"},
                {"subject":"?y","predicate":"<own>","object":"?z"}],"filters":[]}],
            "return":{"source":"?x","target":"?z","label":"holds"}}"#,
    )
    .unwrap();
    fs::write(dir.join("bad.json"), "{not json").unwrap();
    let small = path_str(&data("small/small.nt"));
    let count = "select (count(*) as ?n) where { ?x <Rule:holds> ?z };\n";
    let r = srv.script(
        &dir,
        &format!(
            "listreason;\n\
             create small {small};\n\
             use small;\n\
             addreason missing.json;\n\
             addreason bad.json;\n\
             addreason rule.json;\n\
             listreason;\n\
             compilereason holds;\n\
             executereason holds;\n\
             {count}\
             showreason holds;\n\
             deletereason holds;\n\
             disablereason holds;\n\
             {count}\
             deletereason holds;\n\
             listreason;\n\
             compilereason holds;\n"
        ),
    );
    let out = &r.out;
    let insert = "insert { ?x <Rule:holds> ?z. } where { ?x <contain> ?y. ?y <own> ?z.}";
    let eight = "\"8\"^^<http://www.w3.org/2001/XMLSchema#integer>";
    let zero = "\"0\"^^<http://www.w3.org/2001/XMLSchema#integer>";
    printed_in_order(
        out,
        &[
            "Current database not selected.",
            "Build RDF database small successfully!",
            "File missing.json does not exist.\n",
            "failed to parse file bad.json: ",
            // addreason: the server's message, naming the rule file.
            "Add Reason Rule done.Save Successfully! the file path is ",
            "holds.json\n",
            "| 1      | holds    | a container holds what its nodes own |\n",
            "list reasons successfully!\n",
            // compilereason: both statements in one row.
            "insert_sparql",
            "delete_sparql",
            insert,
            "compile reason holds successfully!\n",
            // executereason: the statement it ran.
            insert,
            "execute reason holds successfully!\n",
            eight,
            "Query ans num 1, use ",
            // showreason: the number of conditions, then one row per pattern.
            "1\n",
            "| holds    | a container holds what its nodes own | <?x,<contain>,?y> |",
            "| holds    | a container holds what its nodes own | <?y,<own>,?z>     |",
            "show reason holds successfully!\n",
            // An executed rule cannot be deleted: disable it first.
            "delete reason holds failed: ",
            "disable reason holds successfully!\n",
            zero,
            "delete reason holds successfully!\n",
            "list reasons successfully!\n",
            "compile reason holds failed: ",
        ],
    )
    .unwrap_or_else(|e| panic!("{e}: {r:?}"));
    // The disable printed the retraction statement.
    assert!(out.contains("delete_sparql"), "{r:?}");
    // The last listreason shows the empty table: no rule row left.
    let last_list = out.rfind("| ruleid | rulename | description").unwrap();
    assert!(!out[last_list..].contains("| 1 "), "{r:?}");
    assert!(dir.join("dbhome/small.db/reason_rule_files").is_dir());
    assert!(!dir
        .join("dbhome/small.db/reason_rule_files/holds.json")
        .exists());
    assert_eq!(r.code, 1, "the failing commands set the exit code: {r:?}");
}

/// A function file for `addpfn` / `mdfpfn`: upstream's `PFNInfo` with a
/// WebAssembly-text body that emits `{"<key>":<host(…)>}`.
fn pfn_file(name: &str, key: &str, host: &str) -> String {
    let open = format!("{{\\\"{key}\\\":");
    Json::obj()
        .with("funName", name)
        .with("funDesc", format!("{host} as JSON"))
        .with(
            "funSubs",
            format!("(data (i32.const 0) \"{open}\") (data (i32.const 32) \"}}\")",),
        )
        .with(
            "funBody",
            format!(
                "(call $emit (i32.const 0) (i32.const {})) (call $emit_i64 (call ${host})) \
                 (call $emit (i32.const 32) (i32.const 1)) (i32.const 1)",
                key.len() + 4
            ),
        )
        .to_string()
}

/// Phase 4 through the console: a PFN function's lifecycle — `showpfn`,
/// `addpfn` and `mdfpfn` from a JSON file holding WebAssembly text,
/// `buildpfn`, a call typed at the prompt, `delpfn` — against a `gserver`
/// whose functions live under `conf.ini`'s `pfn_base_path`. With the
/// `pfn-wasm` runtime the call prints the function's JSON and upstream's
/// `pfn query use … ms.`; without it, building and calling fail with an
/// error naming the feature, and the rest of the lifecycle still works.
#[test]
fn function_commands_run_a_pfn_lifecycle() {
    let dir = scratch("gc_pfn_life");
    let _dir = DirGuard(dir.clone());
    let srv = Gserver::start(&dir, "pfn_base_path=./pfnhome/\n");
    fs::write(
        dir.join("vcount.json"),
        pfn_file("vcount", "n", "vertex_count"),
    )
    .unwrap();
    fs::write(
        dir.join("ecount.json"),
        pfn_file("vcount", "e", "edge_count"),
    )
    .unwrap();
    let small = path_str(&data("small/small.nt"));
    let call = r#"select (PFN.vcount({}) AS ?r) where {};"#;
    let per_var = r#"SELECT ?x (PFN.vcount({"src":"?x"}) AS ?r) WHERE { <root> <contain> ?x };"#;
    let r = srv.script(
        &dir,
        &format!(
            "create small {small};\n\
             use small;\n\
             showpfn;\n\
             addpfn missing.json;\n\
             addpfn vcount.json;\n\
             addpfn vcount.json;\n\
             showpfn;\n\
             buildpfn vcount;\n\
             {call}\n\
             {per_var}\n\
             mdfpfn ecount.json;\n\
             {call}\n\
             buildpfn vcount;\n\
             {call}\n\
             delpfn vcount;\n\
             delpfn vcount;\n\
             showpfn;\n"
        ),
    );
    let out = &r.out;
    let header = "| name | desc | returnType | status | lastBuildTime |";
    let mut lines = vec![
        "Build RDF database small successfully!",
        header,
        "query custom function successfully!\n",
        "File missing.json does not exist.\n",
        "create custom function successfully!\n",
        "function operation failed: Function create fail: function name vcount already exists\n",
        "failed to create custom function\n",
        // The source column names the WebAssembly text the server keeps.
        "| vcount | vertex_count as JSON | src/root/vcount.wat | 1      |",
        "query custom function successfully!\n",
    ];
    if cfg!(feature = "pfn-wasm") {
        lines.extend([
            "custom function build successfully!\n",
            // small.nt has 14 vertices (entities on entity-to-entity edges).
            "{\"n\":14}\npfn query use ",
            // One run per distinct ?x (node0 … node4), an array of outputs.
            "[{\"n\":14},{\"n\":14},{\"n\":14},{\"n\":14},{\"n\":14}]\npfn query use ",
            "update custom function successfully!\n",
            // Updated, the function must be compiled again.
            "Query failed: ",
            "not compile yet",
            "custom function build successfully!\n",
            "{\"e\":",
            "pfn query use ",
        ]);
    } else {
        lines.extend([
            "function operation failed: ",
            "pfn-wasm",
            "failed to build custom function\n",
            "Query failed: ",
            "update custom function successfully!\n",
            "Query failed: ",
            "failed to build custom function\n",
            "Query failed: ",
        ]);
    }
    lines.extend([
        "delete custom function successfully!\n",
        "function operation failed: Function delete fail: function vcount not exists\n",
        "failed to delete custom function\n",
        header,
        "query custom function successfully!\n",
    ]);
    printed_in_order(out, &lines).unwrap_or_else(|e| panic!("{e}: {r:?}"));
    // The functions live under pfn_base_path, per user; delete removed the
    // source.
    assert!(dir.join("pfnhome/src/root/data.json").is_file(), "{r:?}");
    assert!(!dir.join("pfnhome/src/root/vcount.wat").exists(), "{r:?}");
    assert!(
        !out.contains("Update ans num"),
        "a PFN answer is not an update: {r:?}"
    );
    assert_eq!(r.code, 1, "the failing commands set the exit code: {r:?}");
}

#[test]
fn interactive_prompts_banner_and_history() {
    let srv = Server::start("gc_prompts");
    ok(&srv.build("small", &data("small/small.nt")));
    let cwd = scratch("gc_prompts_cwd");
    let _cwd = DirGuard(cwd.clone());
    let hist = cwd.join("hist");
    let mut cmd = gconsole(&srv.addr, &cwd);
    cmd.args(["-u", "root", "-p", PASSWORD, "-i"])
        .env("GSTORE_HISTFILE", &hist);
    let r = run(
        cmd,
        "use small;\nselect ?o\nwhere { <root> <name> ?o }\n;\nhistory;\nunload;\n",
    );
    let out = &r.out;
    assert_eq!(r.code, 0, "{r:?}");
    for line in [
        "\ngStore Console , an interactive shell based utility to communicate with gStore repositories.\n\
         gStore version: 1.4 Source distribution\n",
        "\n\nWelcome to the gStore Console.\n\
         Commands end with ;. Cross line input is allowed.\n\
         Comment start with #. Redirect (> and >>) is supported.\n\
         CTRL+C to quit current command. CTRL+D to exit this console.\n\
         Type 'help;' for help. \n\ngStore[no database]> Current database switch to small successfully.\n\n",
        "gStore[small]>      ->      -> -------------------\n",
        "    2  select ?o where { <root> <name> ?o };\n",
        "Unload database small successfully.\n\ngStore[no database]> \n\n",
    ] {
        assert!(out.contains(line), "{line:?}: {r:?}");
    }
    assert!(out.starts_with("\ngStore Console , "), "{r:?}");
    let saved = fs::read_to_string(&hist).unwrap();
    assert_eq!(
        saved,
        "use small;\nselect ?o where { <root> <name> ?o };\nhistory;\nunload;\n"
    );
    // The next session sees the first one's history.
    let mut cmd = gconsole(&srv.addr, &cwd);
    cmd.args(["-u", "root", "-p", PASSWORD, "-i"])
        .env("GSTORE_HISTFILE", &hist);
    let r = run(cmd, "history 2;\n");
    assert!(
        r.out.contains("    4  unload;\n    5  history 2;\n"),
        "{r:?}"
    );
    // Scripts leave no history.
    let mut cmd = gconsole(&srv.addr, &cwd);
    cmd.args(["-u", "root", "-p", PASSWORD])
        .env("GSTORE_HISTFILE", &hist);
    run(cmd, "pdb;\n");
    assert_eq!(fs::read_to_string(&hist).unwrap().lines().count(), 5);
}

#[test]
fn the_local_mode_still_opens_databases_directly() {
    let cwd = scratch("gc_local");
    let _cwd = DirGuard(cwd.clone());
    let db = cwd.join("loc.db");
    let mut built = gstore::Database::new("loc".to_string());
    gstore::parser::ntriples::for_each_triple_file(data("small/small.nt"), |t| {
        built.try_insert_triple(&t).map(|_| ())
    })
    .unwrap();
    built.save(&db).unwrap();
    drop(built);
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_gconsole"));
    cmd.current_dir(&cwd).args(["--local", &path_str(&db)]);
    let r = run(cmd, "select ?o where { <root> <name> ?o };\nstats\nquit;\n");
    assert_eq!(r.code, 0, "{r:?}");
    assert!(
        r.out.starts_with("Loaded database 'loc' (25 triples"),
        "{r:?}"
    );
    assert!(r.out.contains("Bookug Lobert"), "{r:?}");
    assert!(r.out.contains("triples=25"), "{r:?}");
    assert!(r.out.ends_with("bye.\n"), "{r:?}");
    // The legacy spelling: a bare name, `<name>.db` in the working directory.
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_gconsole"));
    cmd.current_dir(&cwd).arg("loc");
    let r = run(cmd, "exit;\n");
    assert_eq!(r.code, 0, "{r:?}");
    assert!(r.out.starts_with("Loaded database 'loc'"), "{r:?}");
    assert!(r.out.ends_with("bye.\n"), "{r:?}");
    assert!(r.err.contains("gconsole --local <db>"), "{r:?}");

    // `import` goes through the bulk-load pipeline: any format by extension,
    // N-Quads keep their named graph, a bad path is reported.
    fs::write(
        cwd.join("extra.ttl"),
        "@prefix ex: <http://ex.org/> .\nex:a ex:p ex:b , ex:c .\n",
    )
    .unwrap();
    fs::write(
        cwd.join("extra.nq"),
        "<http://ex.org/a> <http://ex.org/p> <http://ex.org/d> <http://ex.org/g> .\n",
    )
    .unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_gconsole"));
    cmd.current_dir(&cwd).args(["--local", &path_str(&db)]);
    let r = run(
        cmd,
        "import extra.ttl\n\
         import extra.nq;\n\
         import missing.ttl\n\
         select ?o where { graph <http://ex.org/g> { ?s ?p ?o } };\n\
         select (count(*) as ?n) where { <http://ex.org/a> ?p ?o };\n\
         quit;\n",
    );
    // `import missing.ttl` failed: a non-interactive run says so.
    assert_eq!(r.code, 1, "{r:?}");
    assert!(
        r.out.contains("imported 2 new triple(s) from extra.ttl"),
        "{r:?}"
    );
    assert!(
        r.out.contains("imported 1 new triple(s) from extra.nq"),
        "{r:?}"
    );
    assert!(r.out.contains("<http://ex.org/d>"), "{r:?}");
    assert!(r.out.contains("\"2\"^^"), "{r:?}");
    assert!(
        r.err.contains("import failed after 0 new triple(s)"),
        "{r:?}"
    );
}

/// `gconsole --local`: a SPARQL `LOAD` typed in lower case is SPARQL, not
/// the console's `load <name>`; a non-interactive run in which a statement
/// failed exits non-zero.
#[test]
fn the_local_mode_runs_a_lowercase_sparql_load_and_reports_failures() {
    let cwd = scratch("gc_local_load");
    let _cwd = DirGuard(cwd.clone());
    let file = cwd.join("d.ttl");
    fs::write(&file, "<http://ex/s> <http://ex/p> <http://ex/o> .\n").unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_gconsole"));
    cmd.current_dir(&cwd).args(["--local", "mem"]);
    let r = run(
        cmd,
        &format!(
            "load <file://{}> into graph <http://ex/z>;\n\
             select ?o where {{ graph <http://ex/z> {{ ?s ?p ?o }} }};\n\
             quit;\n",
            path_str(&file)
        ),
    );
    assert_eq!(r.code, 0, "{r:?}");
    assert!(r.out.contains("[updated 1 triple(s)]"), "{r:?}");
    assert!(r.out.contains("<http://ex/o>"), "{r:?}");
    for script in ["select garbage;\nquit;\n", "load missing\nquit;\n"] {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_gconsole"));
        cmd.current_dir(&cwd).args(["--local", "mem"]);
        let r = run(cmd, script);
        assert_eq!(r.code, 1, "{script}: {r:?}");
    }
}

/// A snapshot-layout database `<dir>/<name>.db` holding `triples` (N-Triples).
fn snapshot_db(dir: &Path, name: &str, triples: &str) -> PathBuf {
    let path = dir.join(format!("{name}.db"));
    let db = gstore::Database::build_from_str(name, triples).unwrap();
    db.save(&path).unwrap();
    assert_eq!(
        gstore::Database::storage_kind(&path),
        Some(gstore::StorageKind::Snapshot)
    );
    path
}

fn triples_in(dir: &Path) -> u64 {
    gstore::Database::open(dir).unwrap().triple_num()
}

/// review ext#5: `gconsole --local` keeps the updates it acknowledged
/// (`[updated N triple(s)]`) to a snapshot-layout database — persisted only
/// by a save — when it leaves the database: at `quit`, at the end of the
/// input and on `load <other>`. A new in-memory database is saved to
/// `<name>.db`, as `save` does.
#[test]
fn the_local_mode_saves_acknowledged_updates_when_it_leaves_a_database() {
    let cwd = scratch("gc_local_save");
    let _cwd = DirGuard(cwd.clone());
    let one = "<http://a> <http://p> <http://b> .\n";
    let insert = "insert data { <http://a> <http://p> <http://c> };\n";
    snapshot_db(&cwd, "other", one);
    for (how, tail) in [
        ("quit", "quit;\n"),
        ("the end of the input", ""),
        ("load <other>", "load other\nquit;\n"),
    ] {
        let kept = snapshot_db(&cwd, "kept", one);
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_gconsole"));
        cmd.current_dir(&cwd).args(["--local", "kept"]);
        let r = run(cmd, &format!("{insert}{tail}"));
        assert_eq!(r.code, 0, "{how}: {r:?}");
        assert!(r.out.contains("[updated 1 triple(s)]"), "{how}: {r:?}");
        assert_eq!(triples_in(&kept), 2, "{how}: the insert is kept: {r:?}");
        assert!(
            r.out.contains("saved to "),
            "{how}: the save is reported: {r:?}"
        );
        fs::remove_dir_all(&kept).unwrap();
    }
    // Nothing changed: nothing is written.
    snapshot_db(&cwd, "kept", one);
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_gconsole"));
    cmd.current_dir(&cwd).args(["--local", "kept"]);
    let r = run(cmd, "select * where { ?s ?p ?o };\nquit;\n");
    assert_eq!(r.code, 0, "{r:?}");
    assert!(!r.out.contains("saved to"), "{r:?}");
    // A new in-memory database: saved where `save` saves it.
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_gconsole"));
    cmd.current_dir(&cwd).args(["--local", "fresh"]);
    let r = run(cmd, insert);
    assert_eq!(r.code, 0, "{r:?}");
    assert_eq!(triples_in(&cwd.join("fresh.db")), 1, "{r:?}");
}

/// review ext#5, the rest of the rule: an `import` is kept like an update;
/// an on-disk database writes each update through and is never saved by
/// the console (in whichever `GSTORE_DISK_QUERY_MODE` it is opened); and a
/// save the console cannot make is never passed over: it fails the script,
/// and `load <other>` keeps the database whose updates it could not save.
#[test]
fn the_local_mode_keeps_imports_writes_disk_databases_through_and_reports_a_failed_save() {
    let cwd = scratch("gc_local_save_rest");
    let _cwd = DirGuard(cwd.clone());
    let one = "<http://a> <http://p> <http://b> .\n";
    let insert = "insert data { <http://a> <http://p> <http://c> };\n";
    let nt = cwd.join("more.nt");
    fs::write(&nt, "<http://x> <http://p> <http://y> .\n").unwrap();
    let local = |db: &str, stdin: &str| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_gconsole"));
        cmd.current_dir(&cwd).args(["--local", db]);
        run(cmd, stdin)
    };

    // An import into a snapshot database, then the end of the input.
    let kept = snapshot_db(&cwd, "kept", one);
    let r = local("kept", &format!("import {}\n", path_str(&nt)));
    assert_eq!(r.code, 0, "{r:?}");
    assert!(r.out.contains("imported 1 new triple(s)"), "{r:?}");
    assert!(r.out.contains("saved to "), "{r:?}");
    assert_eq!(triples_in(&kept), 2, "the import is kept: {r:?}");

    // An on-disk database: written through, not saved.
    let disk = cwd.join("ondisk.db");
    let base = cwd.join("one.nt");
    fs::write(&base, one).unwrap();
    gstore::Database::build_disk(&disk, &[&base]).unwrap();
    assert_eq!(
        gstore::Database::storage_kind(&disk),
        Some(gstore::StorageKind::Disk)
    );
    let r = local("ondisk", &format!("{insert}quit;\n"));
    assert_eq!(r.code, 0, "{r:?}");
    assert!(r.out.contains("[updated 1 triple(s)]"), "{r:?}");
    assert!(!r.out.contains("saved to"), "written through: {r:?}");
    assert_eq!(triples_in(&disk), 2, "the insert is kept: {r:?}");

    // A new in-memory database whose `<name>.db` cannot be written (a file
    // is in its way).
    fs::write(cwd.join("blocked.db"), "not a database").unwrap();
    for (how, tail) in [
        ("the end of the input", ""),
        ("quit", "quit;\n"),
        ("load <other>", "load kept\nstats\n"),
    ] {
        let r = local("blocked", &format!("{insert}{tail}"));
        assert_eq!(r.code, 1, "{how}: the failed save fails the script: {r:?}");
        assert!(r.err.contains("save failed"), "{how}: {r:?}");
        assert!(!r.out.contains("saved to"), "{how}: {r:?}");
        if how == "load <other>" {
            assert!(r.err.contains("'blocked' stays loaded"), "{how}: {r:?}");
            assert!(!r.out.contains("loaded 'kept'"), "{how}: {r:?}");
            assert!(r.out.contains("name=blocked triples=1"), "{how}: {r:?}");
        }
    }
    assert_eq!(
        fs::read_to_string(cwd.join("blocked.db")).unwrap(),
        "not a database"
    );
}

/// review ext#9: `gconsole --local` ends a statement by the remote
/// console's rule — at a `;` outside `{…}` groups, string literals and
/// `<IRI>`s — not at any line ending in `;`: a multi-line SELECT with a
/// predicate-object list, a long literal whose first line ends in `;`, two
/// statements on one line and a multi-operation update sharing its prologue
/// all run, and a script's final statement may lack its `;`.
#[test]
fn the_local_mode_ends_statements_like_the_remote_console() {
    let cwd = scratch("gc_local_statements");
    let _cwd = DirGuard(cwd.clone());
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_gconsole"));
    cmd.current_dir(&cwd).args(["--local", "memdb"]);
    let r = run(
        cmd,
        "insert data { <http://s> <http://p> \"a\" . <http://s> <http://q> \"b\" };\n\
         select ?s ?z where { ?s <http://p> ?o ;\n\
         <http://q> ?z };\n\
         insert data { <http://s> <http://r> \"\"\"line one;\n\
         line two\"\"\" };\n\
         ask { <http://s> <http://r> \"line one;\\nline two\" };\n\
         select (1 as ?first) {}; select (2 as ?second) {};\n\
         prefix ex: <http://ex/> insert data { ex:a ex:p ex:b } ; insert data { ex:c ex:p ex:d };\n\
         select (count(*) as ?n) where { ?x <http://ex/p> ?y }",
    );
    assert_eq!(r.code, 0, "{r:?}");
    assert!(r.err.is_empty(), "no statement failed: {r:?}");
    assert!(r.out.contains("\"b\""), "the multi-line SELECT ran: {r:?}");
    assert!(
        r.out.contains("true"),
        "the long literal kept its ';': {r:?}"
    );
    assert!(
        r.out.contains("?first") && r.out.contains("?second"),
        "{r:?}"
    );
    assert!(
        r.out.contains("\"2\"^^"),
        "both operations of the update, and the final statement without ';': {r:?}"
    );
}

/// `create` / `batchinsert` / `batchremove` against a real server whose
/// import is tolerant (upstream's): bad statements are skipped and counted,
/// the console prints the server's `success_num` / `failed_num`, and the
/// database's `parse_error.log` names each failure's line.
#[test]
fn tolerant_imports_print_the_servers_success_and_failed_counts() {
    let cwd = scratch("gc_tolerant_cwd");
    let _cwd = DirGuard(cwd.clone());
    let srv = Server::start_with_dir("gc_tolerant", Some(&cwd));
    fs::write(
        cwd.join("t.ttl"),
        "@prefix ex: <http://example.org/> .\n\
         ex:alice ex:knows ex:bob .\n\
         ex:alice ex:age \"30\"^^<http://www.w3.org/2001/XMLSchema#integer> .\n\
         ex:bob ex:age \"notanumber\"^^<http://www.w3.org/2001/XMLSchema#integer> .\n\
         ex:bob ex:name \"Bob\" .\n\
         ex:carol ex:knows ex:alice ; ex:name \"Carol\" .\n\
         ex:dave ex:broken <unterminated .\n\
         ex:dave ex:name \"Dave\" .\n",
    )
    .unwrap();
    fs::write(
        cwd.join("more.nt"),
        "<http://example.org/erin> <http://example.org/name> \"Erin\" .\n\
         <http://example.org/erin> <http://example.org/age> \"x\"^^<http://www.w3.org/2001/XMLSchema#int> .\n\
         <http://example.org/bob> <http://example.org/name> \"Bob\" .\n\
         this is not a triple\n",
    )
    .unwrap();
    fs::write(
        cwd.join("del.nt"),
        "<http://example.org/erin> <http://example.org/name> \"Erin\" .\n\
         <http://example.org/nobody> <http://example.org/name> \"Nobody\" .\n\
         broken line\n",
    )
    .unwrap();
    let r = script(
        &srv,
        &cwd,
        "create tolerant t.ttl;\n\
         use tolerant;\n\
         batchinsert more.nt;\n\
         batchremove del.nt;\n\
         select (count(*) as ?n) where { ?s ?p ?o };\n",
    );
    assert_eq!(r.code, 0, "{r:?}");
    // Build: 6 stored (the `<unterminated` line costs only itself), 2 failed.
    // Insert: Erin's name is new, Bob's is a duplicate (not counted), the
    // `xsd:int` literal and the garbage line fail. Remove: Erin's name
    // existed, nobody's did not (not counted), one line fails.
    for line in [
        "RDF parse error num 2\nSee log file for details \nBuild RDF database tolerant successfully! use ",
        "After inserted triples num 1,failed num 2,used ",
        "After removed triples num 1,failed num 1,used ",
        "\"6\"^^<http://www.w3.org/2001/XMLSchema#integer>",
    ] {
        assert!(r.out.contains(line), "{line:?}: {r:?}");
    }
    let log = fs::read_to_string(srv.root.join("tolerant.db").join("parse_error.log")).unwrap();
    let lines: Vec<&str> = log.lines().collect();
    assert_eq!(lines.len(), 8, "{log}");
    assert!(
        lines[0].ends_with(&format!(
            "build parser info, file path {}",
            path_str(&fs::canonicalize(cwd.join("t.ttl")).unwrap())
        )),
        "{log}"
    );
    assert!(
        lines[1].starts_with("Line 4 (") && lines[1].ends_with("Object integer value invalid"),
        "{log}"
    );
    assert!(
        lines[2].starts_with("Line 7 (") && lines[2].contains("invalid IRI"),
        "{log}"
    );
    assert!(
        lines[3].contains(" batch insert parser info, file path "),
        "{log}"
    );
    assert!(
        lines[4].starts_with("Line 2 (") && lines[4].ends_with("Object int value invalid"),
        "{log}"
    );
    assert!(lines[5].starts_with("Line 4 ("), "{log}");
    assert!(
        lines[6].contains(" batch remove parser info, file path "),
        "{log}"
    );
    assert!(lines[7].starts_with("Line 3 ("), "{log}");
}

// ---- a scripted fake server: exact requests, Ctrl-C -------------------------------

/// What the fake server answers a request with: `None` holds the request
/// until the client hangs up.
type Responder = dyn Fn(&Json) -> Option<Json> + Send + Sync;

/// A fake `/api` server recording every request body.
struct Fake {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<Json>>>,
    /// Requests held until the client hung up.
    dropped: Arc<Mutex<Vec<Json>>>,
}

impl Fake {
    fn start(respond: Arc<Responder>) -> Fake {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let dropped = Arc::new(Mutex::new(Vec::new()));
        let (reqs, drops) = (Arc::clone(&requests), Arc::clone(&dropped));
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let (reqs, drops, respond) =
                    (Arc::clone(&reqs), Arc::clone(&drops), Arc::clone(&respond));
                thread::spawn(move || {
                    let Some(req) = read_request(&mut stream) else {
                        return;
                    };
                    reqs.lock().unwrap().push(req.clone());
                    match respond(&req) {
                        Some(answer) => reply(&mut stream, &answer),
                        None => {
                            let _ = stream.set_read_timeout(Some(Duration::from_secs(60)));
                            let mut buf = [0u8; 16];
                            if stream.read(&mut buf).unwrap_or(0) == 0 {
                                drops.lock().unwrap().push(req);
                            }
                        }
                    }
                });
            }
        });
        Fake {
            addr,
            requests,
            dropped,
        }
    }

    /// The requests of operation `op`.
    fn ops(&self, op: &str) -> Vec<Json> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| field(r, "operation") == op)
            .cloned()
            .collect()
    }
}

fn field(j: &Json, key: &str) -> String {
    j.get(key)
        .map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_string))
        .unwrap_or_default()
}

/// One HTTP request's JSON body.
fn read_request(stream: &mut TcpStream) -> Option<Json> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut len = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            if k.eq_ignore_ascii_case("content-length") {
                len = v.trim().parse().ok()?;
            }
        }
    }
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).ok()?;
    json::parse(&String::from_utf8_lossy(&body)).ok()
}

fn reply(stream: &mut TcpStream, body: &Json) {
    let text = body.to_string();
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
        text.len()
    );
}

fn success() -> Json {
    Json::obj()
        .with("StatusCode", 0)
        .with("StatusMsg", "success")
}

#[test]
fn requests_carry_upstreams_operations_and_parameters() {
    let respond: Arc<Responder> = Arc::new(|req: &Json| {
        let op = field(req, "operation");
        let kind = field(req, "type");
        Some(match (op.as_str(), kind.as_str()) {
            ("reasonManage", "2") => success().with(
                "list",
                Json::Arr(vec![Json::obj()
                    .with("rulename", "r1")
                    .with("description", "a rule")]),
            ),
            ("reasonManage", "3") => success()
                .with("insert_sparql", "INSERT DATA {}")
                .with("delete_sparql", "DELETE DATA {}"),
            ("reasonManage", "6") => success().with(
                "ruleinfo",
                Json::obj()
                    .with("rulename", "r1")
                    .with("description", "a rule")
                    .with(
                        "condition",
                        Json::Arr(vec![Json::obj()
                            .with(
                                "patterns",
                                Json::Arr(vec![Json::obj()
                                    .with("subject", "?x")
                                    .with("predicate", "<p>")
                                    .with("object", "?y")]),
                            )
                            .with("filters", Json::Arr(vec![Json::from("?x != ?y")]))]),
                    ),
            ),
            ("funquery", _) => success().with(
                "list",
                Json::Arr(vec![Json::obj()
                    .with("funName", "f1")
                    .with("funDesc", "d")
                    .with("funStatus", "2")
                    .with("lastTime", "2024-01-01 00:00:00")]),
            ),
            ("query", _) => success()
                .with(
                    "head",
                    Json::obj()
                        .with("link", Json::arr())
                        .with("vars", Json::Arr(vec![Json::from("r")])),
                )
                .with(
                    "results",
                    Json::obj().with(
                        "bindings",
                        Json::Arr(vec![Json::obj().with(
                            "r",
                            Json::obj()
                                .with("type", "literal")
                                .with("value", "{\"paths\":[{\"src\":\"<a>\",\"dst\":\"<b>\"}]}"),
                        )]),
                    ),
                )
                .with("AnsNum", 1)
                .with("QueryTime", "7"),
            ("build", _) => success().with("failed_num", 2).with("success_num", 3),
            ("backup", _) => success().with("backupfilepath", "./backups/db1.db_20240101000000"),
            _ => success(),
        })
    });
    let fake = Fake::start(respond);
    let cwd = scratch("gc_fake_requests");
    let _cwd = DirGuard(cwd.clone());
    let nt = cwd.join("d.nt");
    fs::write(&nt, "<a> <b> <c> .\n").unwrap();
    fs::create_dir_all(cwd.join("dir")).unwrap();
    fs::write(cwd.join("dir/2.nt"), "").unwrap();
    fs::write(cwd.join("dir/1.nt"), "").unwrap();
    fs::create_dir_all(cwd.join("dir/sub")).unwrap();
    fs::write(cwd.join("dir/sub/0.nt"), "").unwrap();
    fs::write(cwd.join("rule.json"), r#"{"rulename":"r1","condition":[]}"#).unwrap();
    fs::write(
        cwd.join("fun.json"),
        r#"{"funName":"f1","funDesc":"d","funBody":"int x;","funSubs":"","extra":1}"#,
    )
    .unwrap();
    let mut cmd = gconsole(&fake.addr, &cwd);
    cmd.args(["-u", "admin2", "-p", "pw"]);
    let r = run(
        cmd,
        "create db1 d.nt;\n\
         create db2 dir;\n\
         create db3 http://example.org/x.nt;\n\
         use db1;\n\
         select ?r where { ?r ?p ?o };\n\
         batchinsert d.nt;\n\
         batchremove d.nt;\n\
         backup bk;\n\
         export;\n\
         restore db2 ./backups/db1.db_20240101000000/;\n\
         drop db2;\n\
         init a1x,b2y;\n\
         refreshconf;\n\
         addreason rule.json;\n\
         listreason;\n\
         compilereason r1;\n\
         showreason r1;\n\
         deletereason r1;\n\
         showpfn;\n\
         addpfn fun.json;\n\
         delpfn f1;\n\
         buildpfn f1;\n\
         unload;\n",
    );
    let abs = |p: &str| path_str(&fs::canonicalize(cwd.join(p)).unwrap());
    // Every request carries the user's credentials.
    let all = fake.requests.lock().unwrap().clone();
    assert!(all.len() >= 22, "{all:?}");
    for req in &all {
        assert_eq!(field(req, "username"), "admin2", "{req}");
        assert_eq!(field(req, "password"), "pw", "{req}");
    }
    let builds = fake.ops("build");
    assert_eq!(
        builds[0].to_string(),
        format!(
            r#"{{"operation":"build","username":"admin2","password":"pw","db_name":"db1","remote":false,"db_path":["{}"]}}"#,
            abs("d.nt")
        )
    );
    // A directory: its files, recursively, sorted.
    assert_eq!(
        builds[1].get("db_path").unwrap().to_string(),
        format!(
            r#"["{}","{}","{}"]"#,
            abs("dir/1.nt"),
            abs("dir/2.nt"),
            abs("dir/sub/0.nt")
        )
    );
    assert_eq!(field(&builds[2], "remote"), "true");
    assert_eq!(
        builds[2].get("db_path").unwrap().to_string(),
        r#"["http://example.org/x.nt"]"#
    );
    let load = &fake.ops("load")[0];
    assert_eq!(
        (field(load, "db_name"), field(load, "csr")),
        ("db1".into(), "0".into())
    );
    let query = &fake.ops("query")[0];
    assert_eq!(field(query, "sparql"), "select ?r where { ?r ?p ?o }");
    assert_eq!(field(query, "db_name"), "db1");
    let insert = &fake.ops("batchInsert")[0];
    assert_eq!(
        insert.get("file").unwrap().to_string(),
        format!(r#"["{}"]"#, abs("d.nt"))
    );
    assert_eq!(fake.ops("batchRemove").len(), 1);
    assert_eq!(field(&fake.ops("backup")[0], "backup_path"), "bk");
    assert_eq!(field(&fake.ops("export")[0], "db_path"), "export/");
    let restore = &fake.ops("restore")[0];
    assert_eq!(field(restore, "db_name"), "db2");
    assert_eq!(
        field(restore, "backup_path"),
        "./backups/db1.db_20240101000000"
    );
    let drop = &fake.ops("drop")[0];
    assert_eq!(
        (field(drop, "db_name"), field(drop, "is_backup")),
        ("db2".into(), "false".into())
    );
    assert_eq!(field(&fake.ops("init")[0], "db_names"), "a1x,b2y");
    assert_eq!(fake.ops("refreshconf").len(), 1);
    let reasons = fake.ops("reasonManage");
    let kinds: Vec<String> = reasons.iter().map(|r| field(r, "type")).collect();
    assert_eq!(kinds, ["1", "2", "3", "6", "7"]);
    assert_eq!(
        reasons[0].get("ruleinfo").unwrap().to_string(),
        r#"{"rulename":"r1","condition":[]}"#
    );
    assert!(reasons.iter().all(|r| field(r, "db_name") == "db1"));
    assert_eq!(field(&reasons[2], "rulename"), "r1");
    assert_eq!(
        fake.ops("funquery")[0].get("funInfo").unwrap().to_string(),
        r#"{"funName":"","funDesc":"","funBody":"","funSubs":"","funStatus":"","lastTime":""}"#
    );
    let cudb = fake.ops("funcudb");
    let kinds: Vec<String> = cudb.iter().map(|r| field(r, "type")).collect();
    assert_eq!(kinds, ["1", "3", "4"]);
    assert_eq!(
        cudb[0].get("funInfo").unwrap().to_string(),
        r#"{"funName":"f1","funDesc":"d","funBody":"int x;","funSubs":"","funStatus":"","lastTime":""}"#
    );
    assert_eq!(field(cudb[1].get("funInfo").unwrap(), "funName"), "f1");
    assert_eq!(field(&fake.ops("unload")[0], "db_name"), "db1");

    // And the answers are printed as upstream prints them.
    let out = &r.out;
    for line in [
        "RDF parse error num 2\nSee log file for details \nBuild RDF database db1 successfully! use ",
        // A graph function's JSON cell, indented (keys sorted).
        "{\n    \"paths\": [\n        {\n            \"dst\": \"<b>\",\n            \"src\": \"<a>\"\n        }\n    ]\n}\nQuery ans num 1, use 7 ms.\n",
        "After inserted triples num 0,failed num 0,used ",
        "Database db1 backup successfully.\nBackup path: ./backups/db1.db_20240101000000\n",
        "Database db2 restored successfully.\n",
        "Drop database db2 successfully! \n",
        "Init database result: \n",
        "success\n",
        "| ruleid | rulename | description |\n-----------------------------------\n| 1      | r1       | a rule      |\n",
        "list reasons successfully!\n",
        "|  insert_sparql |  delete_sparql |\n",
        "| INSERT DATA {} | DELETE DATA {} |\n-----------------------------------\ncompile reason r1 successfully!\n",
        "1\n",
        "| r1       | a rule      | <?x,<p>,?y> | ?x != ?y; |\n",
        "delete reason r1 successfully!\n",
        // Upstream shows `./pfn/<name>.cpp`; here the WebAssembly source.
        "| f1   | d    | src/admin2/f1.wat | 2      | 2024-01-01 00:00:00 |\n",
        "query custom function successfully!\n",
        "create custom function successfully!\n",
        "delete custom function successfully!\n",
        "custom function build successfully!\n",
        "Unload database db1 successfully.\n",
    ] {
        assert!(out.contains(line), "{line:?}: {r:?}");
    }
    assert_eq!(r.code, 0, "{r:?}");
}

/// Whether `text` holds a character a terminal acts on rather than shows
/// (C0 controls but line breaks and tabs, DEL, C1 controls).
fn drives_the_terminal(text: &str) -> bool {
    text.chars().any(|c| {
        let u = c as u32;
        (u < 0x20 && c != '\n' && c != '\t') || (0x7f..0xa0).contains(&u)
    })
}

/// review ext#10: what a server answers is printed with its control
/// characters escaped — the paths `backup` and `export` print, the counts
/// and times of a query, an update and an import, the failure message of a
/// login and the text of an answer that is not an `/api` envelope — so a
/// spoofed or compromised server cannot drive the operator's terminal
/// (retitle it, clear it, write the clipboard).
#[test]
fn server_fields_cannot_drive_the_terminal() {
    const ESC: &str = "\u{1b}]0;PWNED\u{7}\u{1b}[2J";
    let respond: Arc<Responder> = Arc::new(|req: &Json| {
        Some(match field(req, "operation").as_str() {
            "backup" => success().with("backupfilepath", format!("{ESC}./backups/x").as_str()),
            "export" => success().with("filepath", "\u{1b}]52;c;SGVsbG8=\u{7}f.nt"),
            "query" => success()
                .with("AnsNum", "\u{1b}[2J5")
                .with("QueryTime", "\u{1b}]0;T\u{7}1"),
            "batchInsert" => success()
                .with("success_num", format!("{ESC}3").as_str())
                .with("failed_num", format!("{ESC}1").as_str()),
            "build" => success().with("failed_num", format!("{ESC}2").as_str()),
            _ => success(),
        })
    });
    let fake = Fake::start(respond);
    let cwd = scratch("gc_control_chars");
    let _cwd = DirGuard(cwd.clone());
    let nt = cwd.join("x.nt");
    fs::write(&nt, "<a> <b> <c> .\n").unwrap();
    let mut cmd = gconsole(&fake.addr, &cwd);
    cmd.args(["-u", "root", "-p", PASSWORD]);
    let r = run(
        cmd,
        &format!(
            "create db1 {nt};\nuse db1;\nbackup;\nexport;\ninsert data {{ <a> <b> <c> }};\n\
             batchinsert {nt};\n",
            nt = path_str(&nt)
        ),
    );
    assert!(!drives_the_terminal(&r.out), "{r:?}");
    for shown in [
        "Backup path: \\u001B]0;PWNED\\u0007\\u001B[2J./backups/x",
        "Export file: \\u001B]52;c;SGVsbG8=\\u0007f.nt",
        "Update ans num \\u001B[2J5, use \\u001B]0;T\\u00071 ms.",
        "RDF parse error num \\u001B]0;PWNED",
        "After inserted triples num \\u001B]0;PWNED",
    ] {
        assert!(r.out.contains(shown), "{shown:?}: {r:?}");
    }
    assert_eq!(r.code, 0, "{r:?}");

    // A login refused with such a message, and an answer that is not an
    // `/api` envelope.
    let respond: Arc<Responder> = Arc::new(|req: &Json| {
        Some(match field(req, "operation").as_str() {
            "login" => Json::obj()
                .with("StatusCode", 1001)
                .with("StatusMsg", format!("{ESC}no").as_str()),
            _ => success(),
        })
    });
    let fake = Fake::start(respond);
    let mut cmd = gconsole(&fake.addr, &cwd);
    cmd.args(["-u", "root", "-p", PASSWORD]);
    let r = run(cmd, "showdbs;\n");
    assert_eq!(r.code, 1, "{r:?}");
    assert!(r.out.contains("login failed: \\u001B]0;PWNED"), "{r:?}");
    assert!(!drives_the_terminal(&r.out), "{r:?}");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let not_json = thread::spawn(move || {
        // The login, then one more request: each answered with text.
        for _ in 0..2 {
            let Ok((mut s, _)) = listener.accept() else {
                return;
            };
            let _ = read_request(&mut s);
            let body = format!("{ESC}<html>");
            let _ = write!(
                s,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    let mut cmd = gconsole(&addr, &cwd);
    cmd.args(["-u", "root", "-p", PASSWORD]);
    let r = run(cmd, "showdbs;\n");
    // Unblock the fake's last accept, if the console made fewer requests.
    let _ = TcpStream::connect(addr);
    not_json.join().unwrap();
    assert_eq!(r.code, 1, "{r:?}");
    assert!(!drives_the_terminal(&r.out), "{r:?}");
    assert!(!drives_the_terminal(&r.err), "{r:?}");
    assert!(r.err.contains("\\u001B]0;PWNED"), "{r:?}");
}

/// review ext#11: `setpriv` grants what is missing first and then revokes
/// the rest; when the revoke fails it takes the grant back, so the user is
/// left with the privileges they had, and says so.
#[test]
fn setpriv_rolls_back_its_grant_when_the_revoke_fails() {
    let respond: Arc<Responder> = Arc::new(|req: &Json| {
        Some(match field(req, "operation").as_str() {
            "showuser" => success().with(
                "ResponseBody",
                Json::Arr(vec![Json::obj()
                    .with("username", "bob")
                    .with("query_privilege", "small,")
                    .with("load_privilege", "small,")
                    .with("unload_privilege", "small,")
                    .with("update_privilege", "")]),
            ),
            "userprivilegemanage"
                if field(req, "type") == "2" && field(req, "privileges") == "1,2,3" =>
            {
                Json::obj()
                    .with("StatusCode", 1005)
                    .with("StatusMsg", "delete privilege query,load,unload failed.")
            }
            _ => success(),
        })
    });
    let fake = Fake::start(respond);
    let cwd = scratch("gc_setpriv_rollback");
    let _cwd = DirGuard(cwd.clone());
    let mut cmd = gconsole(&fake.addr, &cwd);
    cmd.args(["-u", "root", "-p", PASSWORD]);
    let r = run(cmd, &format!("setpriv bob small;\n{PASSWORD}\n4\n"));
    assert_eq!(r.code, 1, "{r:?}");
    let steps: Vec<(String, String)> = fake
        .ops("userprivilegemanage")
        .iter()
        .map(|q| (field(q, "type"), field(q, "privileges")))
        .collect();
    let step = |t: &str, p: &str| (t.to_string(), p.to_string());
    assert_eq!(
        steps,
        [step("1", "4"), step("2", "1,2,3"), step("2", "4")],
        "grant, the failed revoke, the grant taken back: {r:?}"
    );
    assert!(
        r.out.contains(
            "Privilege set failed delete privilege query,load,unload failed.\n\
             The privileges of bob on small are unchanged.\n"
        ),
        "{r:?}"
    );
}

fn sigint(child: &Child) {
    let ok = Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .unwrap()
        .success();
    assert!(ok, "kill -INT");
}

#[cfg(unix)]
#[test]
fn ctrl_c_cancels_the_running_query_and_the_statement_being_typed() {
    // `query` is never answered; its task is listed as opt_id 42.
    let last_query: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let lq = Arc::clone(&last_query);
    let respond: Arc<Responder> =
        Arc::new(move |req: &Json| match field(req, "operation").as_str() {
            "query" => {
                *lq.lock().unwrap() = field(req, "sparql");
                None
            }
            "operationTaskList" => {
                let task = Json::obj()
                    .with("opt_id", "42")
                    .with("operation", "query")
                    .with("async", 0)
                    .with("database", "slow")
                    .with("sparql", lq.lock().unwrap().as_str())
                    .with("user", "root");
                let other = task.clone().with("opt_id", "7").with("user", "someone");
                Some(success().with("list", Json::Arr(vec![other, task])))
            }
            "operationTaskCancel" => Some(
                Json::obj()
                    .with("StatusCode", 0)
                    .with("StatusMsg", "query cancel successfully"),
            ),
            _ => Some(success()),
        });
    let fake = Fake::start(respond);
    let cwd = scratch("gc_ctrl_c");
    let _cwd = DirGuard(cwd.clone());
    // review ext#13 / tests-a#14: `-i` shows the prompts, and the test
    // sends each Ctrl-C only once the prompt it is meant for is visible —
    // the console reads with a Ctrl-C mark taken before showing its prompt
    // (see `term::LineReader::read_line_since`) — and goes on only once
    // the console has answered it, instead of ordering them with sleeps.
    let mut cmd = gconsole(&fake.addr, &cwd);
    cmd.args(["-u", "root", "-p", PASSWORD, "-i"]);
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let out = Live::start(child.stdout.take().unwrap());
    let mut stderr = child.stderr.take().unwrap();
    let err = thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr.read_to_string(&mut s);
        s
    });
    let at = out.wait_for("the first prompt", "gStore[no database]> ", 0);
    stdin.write_all(b"use slow;\n").unwrap();
    let at = out.wait_for("the database's prompt", "gStore[slow]> ", at);
    // Ctrl-C while a statement is being typed drops it.
    stdin.write_all(b"select * where {\n").unwrap();
    let at = out.wait_for("the continuation prompt", "     -> ", at);
    sigint(&child);
    let at = out.wait_for("a fresh prompt", "gStore[slow]> ", at);
    // Ctrl-C while the server works on a query: the connection is dropped
    // and the server's task cancelled. (The console takes its Ctrl-C mark
    // before it sends the request.)
    stdin.write_all(b"select ?x where { ?x ?p ?o };\n").unwrap();
    wait_for("the query", || !fake.ops("query").is_empty());
    sigint(&child);
    wait_for("the cancellation", || {
        !fake.ops("operationTaskCancel").is_empty()
    });
    let at = out.wait_for("the cancel's report", "Query cancelled.\n", at);
    // The console is still there.
    stdin.write_all(b"pdb;\n").unwrap();
    drop(stdin);
    let code = wait(&mut child);
    let (out, err) = (out.finish(), err.join().unwrap());
    let queries: Vec<String> = fake
        .ops("query")
        .iter()
        .map(|q| field(q, "sparql"))
        .collect();
    assert_eq!(
        queries,
        ["select ?x where { ?x ?p ?o }"],
        "the interrupted statement was discarded"
    );
    let cancels: Vec<String> = fake
        .ops("operationTaskCancel")
        .iter()
        .map(|c| field(c, "opt_id"))
        .collect();
    assert_eq!(cancels, ["42"], "only this user's task");
    wait_for("the dropped connection", || {
        !fake.dropped.lock().unwrap().is_empty()
    });
    assert!(
        out[at..].starts_with("\ngStore[slow]> slow\n\n"),
        "pdb ran after the cancel: {out}\n{err}"
    );
    // review ext#13: `-i` makes the run interactive, whose exit status is 0
    // whatever failed; that a cancelled command fails a script (exit 1) is
    // asserted by `ctrl_c_cancels_the_servers_query_task`.
    assert_eq!(code, 0, "{out}\n{err}");
}

/// A child's output as it arrives, to wait for what it printed.
struct Live {
    text: Arc<Mutex<Vec<u8>>>,
    reader: thread::JoinHandle<()>,
}

impl Live {
    fn start(mut pipe: impl Read + Send + 'static) -> Live {
        let text = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&text);
        let reader = thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n @ 1..) = pipe.read(&mut buf) {
                sink.lock().unwrap().extend_from_slice(&buf[..n]);
            }
        });
        Live { text, reader }
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.text.lock().unwrap()).into_owned()
    }

    /// Wait until `marker` has been printed at or after byte `from`; the
    /// offset just past it (the timeout is only a hang guard).
    fn wait_for(&self, what: &str, marker: &str, from: usize) -> usize {
        let t0 = Instant::now();
        loop {
            let text = self.text();
            if let Some(i) = text.get(from..).and_then(|t| t.find(marker)) {
                return from + i + marker.len();
            }
            assert!(
                t0.elapsed() < Duration::from_secs(60),
                "timed out waiting for {what}: {text}"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// Everything, once the child closed its end.
    fn finish(self) -> String {
        let Live { text, reader } = self;
        reader.join().unwrap();
        let text = text.lock().unwrap();
        String::from_utf8_lossy(&text).into_owned()
    }
}

/// A query that runs for a long time unless cancelled (a REGEX FILTER over
/// a cross product, checked for cancellation per row; as in
/// `dt_gapi_tasks`).
const SLOW: &str = "SELECT (COUNT(*) AS ?n) WHERE { ?a ?p1 ?b . ?c ?p2 ?d \
                    FILTER(REGEX(CONCAT(STR(?a), STR(?d), STR(?b), STR(?c)), \"(x+x+)+y\")) }";

#[cfg(unix)]
#[test]
fn ctrl_c_cancels_the_servers_query_task() {
    let dir = scratch("gc_ctrl_c_real_data");
    let _dir = DirGuard(dir.clone());
    let mut ttl = String::from("@prefix : <http://ex/> .\n");
    for i in 0..1200 {
        ttl.push_str(&format!(":s{i} :p :o{i} .\n"));
    }
    fs::write(dir.join("chain.ttl"), ttl).unwrap();
    let srv = Server::start_with_dir("gc_ctrl_c_real", Some(&dir));
    ok(&srv.build("slow", &dir.join("chain.ttl")));
    let mut cmd = gconsole(&srv.addr, &dir);
    cmd.args(["-u", "root", "-p", PASSWORD]);
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let (out, err) = collect(&mut child);
    stdin
        .write_all(format!("use slow;\n{SLOW};\n").as_bytes())
        .unwrap();
    let tasks = |status: &str| {
        let j = srv.op(
            "operationTaskList",
            &[("status", status), ("pageSize", "100")],
        );
        ok(&j);
        j.get("list").and_then(Json::as_array).unwrap().to_vec()
    };
    wait_for("the running query", || {
        tasks("0")
            .iter()
            .any(|t| field(t, "operation") == "query" && field(t, "sparql") == SLOW)
    });
    sigint(&child);
    // The task is cancelled (not merely abandoned by the client).
    wait_for("the cancelled task", || {
        tasks("-1")
            .iter()
            .any(|t| field(t, "operation") == "query" && field(t, "sparql") == SLOW)
    });
    stdin.write_all(b"pdb;\n").unwrap();
    drop(stdin);
    let code = wait(&mut child);
    let (out, err) = (out.join().unwrap(), err.join().unwrap());
    assert!(out.contains("Query cancelled.\n"), "{out}\n{err}");
    assert!(out.ends_with("slow\n\n"), "{out}\n{err}");
    assert_eq!(code, 1, "{out}\n{err}");
    assert!(tasks("0").is_empty(), "nothing left running");
}

// ---- what crosses the network: sessions and TLS (review ext#12) ------------------

/// A TCP relay to `upstream` that records, per connection, the bytes the
/// client sent (each recorded before it is forwarded, so a request the
/// server answered is recorded whole).
struct Relay {
    addr: SocketAddr,
    sent: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl Relay {
    fn start(upstream: SocketAddr) -> Relay {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let sent: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&sent);
        thread::spawn(move || {
            for client in listener.incoming() {
                let Ok(client) = client else { continue };
                let Ok(server) = TcpStream::connect(upstream) else {
                    continue;
                };
                let index = {
                    let mut log = log.lock().unwrap();
                    log.push(Vec::new());
                    log.len() - 1
                };
                let log = Arc::clone(&log);
                let (mut from_client, mut to_server) =
                    (client.try_clone().unwrap(), server.try_clone().unwrap());
                thread::spawn(move || {
                    let mut buf = [0u8; 8192];
                    while let Ok(n @ 1..) = from_client.read(&mut buf) {
                        log.lock().unwrap()[index].extend_from_slice(&buf[..n]);
                        if to_server.write_all(&buf[..n]).is_err() {
                            break;
                        }
                    }
                    let _ = to_server.shutdown(std::net::Shutdown::Write);
                });
                let (mut from_server, mut to_client) = (server, client);
                thread::spawn(move || {
                    let _ = std::io::copy(&mut from_server, &mut to_client);
                    let _ = to_client.shutdown(std::net::Shutdown::Write);
                });
            }
        });
        Relay { addr, sent }
    }

    /// What the client sent on each connection, as (lossy) text.
    fn requests(&self) -> Vec<String> {
        self.sent
            .lock()
            .unwrap()
            .iter()
            .map(|r| String::from_utf8_lossy(r).into_owned())
            .collect()
    }
}

/// review ext#12: gconsole logs in once (`POST /login`) and sends the
/// session, not the password, with every request; when the session ends —
/// here because the user changed their password — it logs in again and
/// repeats the request; at the end it logs out.
#[test]
fn the_password_crosses_the_network_once_per_login() {
    let srv = Server::start("gc_session");
    ok(&srv.build("small", &data("small/small.nt")));
    let relay = Relay::start(srv.addr);
    let cwd = scratch("gc_session_cwd");
    let _cwd = DirGuard(cwd.clone());
    let r = script_at(
        &relay.addr,
        &cwd,
        "showdbs;\nuse small;\nselect (count(*) as ?n) where { ?s ?p ?o };\n",
    );
    assert_eq!(r.code, 0, "{r:?}");
    assert!(r.out.contains("\"25\"^^"), "{r:?}");
    let requests = relay.requests();
    let with_password: Vec<&String> = requests.iter().filter(|r| r.contains(PASSWORD)).collect();
    assert_eq!(with_password.len(), 1, "{requests:#?}");
    assert!(
        with_password[0].starts_with("POST /login HTTP/1.1\r\n"),
        "{requests:#?}"
    );
    let api: Vec<&String> = requests
        .iter()
        .filter(|r| r.starts_with("POST /api "))
        .collect();
    // The session's check (`login`), then the three commands.
    assert_eq!(api.len(), 4, "{requests:#?}");
    assert!(api[0].contains("\"operation\":\"login\""), "{requests:#?}");
    for request in api {
        assert!(request.contains("\r\nAuthorization: Bearer "), "{request}");
        assert!(!request.contains("\"password\""), "{request}");
    }
    assert!(
        requests.last().unwrap().starts_with("POST /logout "),
        "{requests:#?}"
    );

    // The password changes: the server ends the session, the console logs
    // in again with the new password, and the next command still runs.
    let r = script_at(
        &relay.addr,
        &cwd,
        &format!("setpswd;\n{PASSWORD}\nnew-pw\nnew-pw\nshowdbs;\n"),
    );
    assert_eq!(r.code, 0, "{r:?}");
    assert!(
        printed_in_order(&r.out, &["Password set successfully.\n", "| small    |"]).is_ok(),
        "{r:?}"
    );
    let logins = relay
        .requests()
        .iter()
        .filter(|r| r.starts_with("POST /login ") && r.contains("new-pw"))
        .count();
    assert_eq!(logins, 1, "one login again, with the new password");
}

/// A gstore-rust gserver older than the `/api` sessions: `POST /login`
/// gives a session (its REST API), but `/api` wants the password (`1003`
/// without one). Every request is recorded as (path, body).
struct PreSessionServer {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<(String, String)>>>,
}

impl PreSessionServer {
    fn start() -> PreSessionServer {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let requests: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&requests);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let Some((path, body)) = read_raw_request(&mut stream) else {
                    continue;
                };
                log.lock().unwrap().push((path.clone(), body.clone()));
                let answer = match path.as_str() {
                    "/login" => success().with("session", "rest-only-token"),
                    "/api" => match json::parse(&body) {
                        Ok(j) if j.get("password").is_none() => Json::obj()
                            .with("StatusCode", 1003)
                            .with("StatusMsg", "username or password is empty"),
                        Ok(j) if field(&j, "password") != PASSWORD => Json::obj()
                            .with("StatusCode", 1001)
                            .with("StatusMsg", "Username or password is wrong."),
                        Ok(_) => success().with(
                            "ResponseBody",
                            Json::Arr(vec![Json::obj().with("database", "small")]),
                        ),
                        Err(_) => Json::obj().with("StatusCode", 1003),
                    },
                    _ => success(),
                };
                reply(&mut stream, &answer);
            }
        });
        PreSessionServer { addr, requests }
    }
}

/// One HTTP request: its path and its body as text.
fn read_raw_request(stream: &mut TcpStream) -> Option<(String, String)> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut first = String::new();
    reader.read_line(&mut first).ok()?;
    let path = first.split_whitespace().nth(1)?.to_string();
    let mut len = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            if k.eq_ignore_ascii_case("content-length") {
                len = v.trim().parse().ok()?;
            }
        }
    }
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).ok()?;
    Some((path, String::from_utf8_lossy(&body).into_owned()))
}

/// review ext#12 (compatibility): a gserver whose `POST /login` gives a
/// session its `/api` does not take — a gstore-rust release older than the
/// `/api` sessions — is still served, with upstream's protocol (the
/// password in every body); the session it did not take is ended.
#[test]
fn a_server_whose_api_takes_no_session_gets_the_password() {
    let srv = PreSessionServer::start();
    let cwd = scratch("gc_pre_session");
    let _cwd = DirGuard(cwd.clone());
    let r = script_at(&srv.addr, &cwd, "showdbs;\n");
    assert_eq!(r.code, 0, "{r:?}");
    assert!(r.out.contains("| small "), "{r:?}");
    let requests = srv.requests.lock().unwrap().clone();
    let paths: Vec<&str> = requests.iter().map(|(p, _)| p.as_str()).collect();
    assert_eq!(
        paths,
        ["/login", "/api", "/logout", "/api", "/api"],
        "the session tried, refused and ended; upstream's login; showdbs: {requests:#?}"
    );
    for (_, body) in &requests[3..] {
        assert!(
            body.contains(&format!("\"password\":\"{PASSWORD}\"")),
            "{requests:#?}"
        );
    }
}

/// A self-signed certificate for `localhost` / `127.0.0.1` (the one
/// `src/http_client.rs`'s HTTPS test uses) and its key.
#[cfg(feature = "tls")]
const CERT_PEM: &str = "-----BEGIN CERTIFICATE-----
MIIBvTCCAWOgAwIBAgIUKHclqAxNOvVOQ3kWyfFT+rCJUHwwCgYIKoZIzj0EAwIw
FDESMBAGA1UEAwwJbG9jYWxob3N0MB4XDTI2MDcwMjIwMjkyMFoXDTM2MDYyOTIw
MjkyMFowFDESMBAGA1UEAwwJbG9jYWxob3N0MFkwEwYHKoZIzj0CAQYIKoZIzj0D
AQcDQgAEYHEfrA2tZnM3sdpdua9aH0oSv8G2EmRrc10JlY7xgVO8+6VoyIJqtvH5
AxyPvu6UfMZMa+/UFLAmk+WCYEKCi6OBkjCBjzAdBgNVHQ4EFgQUJOZ8AYKC5S5i
QJhbdtMpHgmjlaowHwYDVR0jBBgwFoAUJOZ8AYKC5S5iQJhbdtMpHgmjlaowGgYD
VR0RBBMwEYIJbG9jYWxob3N0hwR/AAABMAwGA1UdEwEB/wQCMAAwDgYDVR0PAQH/
BAQDAgeAMBMGA1UdJQQMMAoGCCsGAQUFBwMBMAoGCCqGSM49BAMCA0gAMEUCIQCo
ndoh3KosHMemvMMbjxtaNbbXmxGjfsgme3rBkLuziQIgFeUx/cU+Fpw/Dl5McuKP
cCWfronEM/ASghi4/9ldMUI=
-----END CERTIFICATE-----
";

#[cfg(feature = "tls")]
const KEY_PEM: &str = "-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgUBOdoa9/azwl851s
NPlNYkOXVnBaUKf3fk8J38AOaYehRANCAARgcR+sDa1mczex2l25r1ofShK/wbYS
ZGtzXQmVjvGBU7z7pWjIgmq28fkDHI++7pR8xkxr79QUsCaT5YJgQoKL
-----END PRIVATE KEY-----
";

/// review ext#12: gconsole talks to a gserver serving TLS (`--tls`),
/// verifying its certificate (`--ca-file`, else `$SSL_CERT_FILE`): nothing
/// of the session — not the password, not a statement — crosses the
/// network in clear text, and a server whose certificate the CA bundle
/// does not vouch for gets nothing.
#[cfg(feature = "tls")]
#[test]
fn the_console_talks_to_a_tls_gserver() {
    let root = scratch("gc_tls");
    let _root = DirGuard(root.clone());
    let certs = scratch("gc_tls_certs");
    let _certs = DirGuard(certs.clone());
    let (cert, key) = (certs.join("cert.pem"), certs.join("key.pem"));
    fs::write(&cert, CERT_PEM).unwrap();
    fs::write(&key, KEY_PEM).unwrap();
    let empty = certs.join("empty.pem");
    fs::write(&empty, "").unwrap();
    let mut config = gstore::config::GStoreConfig::default();
    config.ip_deny_path = None;
    config.thread_num = 4;
    let policy = gstore::LoadPolicy::restricted().with_allowed_dir(data("small"));
    let server = Arc::new(
        gstore::http_api::ApiServer::bind(&root, PASSWORD, "127.0.0.1:0")
            .unwrap()
            .with_config(config)
            .with_load_policy(policy),
    );
    // Stops the server and waits for its serving thread (its shutdown
    // writes below `root`, which `_root` removes afterwards).
    struct Stop(
        Arc<gstore::http_api::ApiServer>,
        Option<thread::JoinHandle<()>>,
    );
    impl Drop for Stop {
        fn drop(&mut self) {
            self.0.shutdown();
            if let Some(h) = self.1.take() {
                let _ = h.join();
            }
        }
    }
    let tls = gstore::server::load_tls_config(&cert, &key).unwrap();
    let serving = Arc::clone(&server);
    let _stop = Stop(
        Arc::clone(&server),
        Some(thread::spawn(move || {
            serving.serve_forever_tls(tls);
        })),
    );
    let relay = Relay::start(server.local_addr().unwrap());
    let cwd = scratch("gc_tls_cwd");
    let _cwd = DirGuard(cwd.clone());
    let console = |extra: &[&str]| {
        let mut cmd = gconsole(&relay.addr, &cwd);
        cmd.args(["-u", "root", "-p", PASSWORD]).args(extra);
        cmd
    };
    let r = run(
        console(&["--tls", "--ca-file", &path_str(&cert)]),
        &format!(
            "create small {nt};\nuse small;\nselect (count(*) as ?n) where {{ ?s ?p ?o }};\n",
            nt = small_nt()
        ),
    );
    assert_eq!(r.code, 0, "{r:?}");
    assert!(
        r.out.contains("Build RDF database small successfully!"),
        "{r:?}"
    );
    assert!(r.out.contains("\"25\"^^"), "{r:?}");
    let sent = relay.requests().concat();
    assert!(!sent.is_empty());
    for clear in [PASSWORD, "POST ", "select", "Bearer"] {
        assert!(
            !sent.contains(clear),
            "{clear:?} crossed the network in clear text"
        );
    }
    // `$SSL_CERT_FILE` names the bundle when --ca-file does not.
    let mut cmd = console(&["--tls", "-e", "showdbs;"]);
    cmd.env("SSL_CERT_FILE", &cert);
    let r = run(cmd, "");
    assert_eq!(r.code, 0, "{r:?}");
    assert!(r.out.contains("| small    |"), "{r:?}");
    // A bundle that does not vouch for the server: refused before anything
    // is sent.
    let before = relay.requests().len();
    let r = run(
        console(&["--tls", "--ca-file", &path_str(&empty), "-e", "showdbs;"]),
        "",
    );
    assert_eq!(r.code, 1, "{r:?}");
    assert_eq!(
        r.out, "Could not connect to server. Please check server status\n",
        "{r:?}"
    );
    assert!(r.err.contains("TLS with 127.0.0.1"), "{r:?}");
    assert!(
        relay.requests()[before..]
            .iter()
            .all(|c| !c.contains(PASSWORD)),
        "nothing is sent to an unverified server"
    );
}

/// Without the `tls` feature, `--tls` is refused up front.
#[cfg(not(feature = "tls"))]
#[test]
fn tls_needs_the_tls_feature() {
    let cwd = scratch("gc_no_tls");
    let _cwd = DirGuard(cwd.clone());
    let addr: SocketAddr = ([127, 0, 0, 1], 0).into();
    let mut cmd = gconsole(&addr, &cwd);
    cmd.args(["-u", "root", "-p", PASSWORD, "--tls", "-e", "showdbs;"]);
    let r = run(cmd, "");
    assert_eq!(r.code, 2, "{r:?}");
    assert!(r.err.contains("`tls` cargo feature"), "{r:?}");
    let mut cmd = gconsole(&addr, &cwd);
    cmd.args(["--ca-file", "x.pem", "-e", "showdbs;"]);
    let r = run(cmd, "");
    assert_eq!(r.code, 2, "{r:?}");
    assert!(r.err.contains("--ca-file needs --tls"), "{r:?}");
}

// ---- the README flow: gserver + conf.ini + gconsole -u root ---------------------

#[cfg(unix)]
#[test]
fn gconsole_finds_gserver_through_conf_ini() {
    let dir = scratch("gc_readme");
    let _dir = DirGuard(dir.clone());
    let data_dir = path_str(&data("small"));
    struct Kill(Child);
    impl Drop for Kill {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    // review tests-a#7: both read the port from conf.ini, so it is chosen
    // beforehand; gserver must then say it serves on it (its own stdout —
    // not merely some listener on that port), and when the port was taken
    // meanwhile gserver exits and another one is tried.
    let mut server = None;
    for _ in 0..10 {
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        fs::create_dir_all(dir.join("conf")).unwrap();
        fs::write(
            dir.join("conf/conf.ini"),
            // review sec#8 / infra#3: an explicit root password (gserver
            // refuses the default one).
            format!(
                "[server]\nport={port}\nthread_num=4\n[system]\ndb_home=./dbhome/\n\
                 root_password={GSERVER_PASSWORD}\n"
            ),
        )
        .unwrap();
        if let Some((child, announced)) = spawn_gserver(&dir, &["--load-dir", &data_dir]) {
            let child = Kill(child);
            assert_eq!(announced, port, "gserver serves conf.ini's port");
            server = Some(child);
            break;
        }
    }
    let server = server.expect("gserver started on a free port");
    // No --port: gconsole reads it from ./conf/conf.ini, like upstream.
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_gconsole"));
    cmd.current_dir(&dir)
        .env("GSTORE_HISTFILE", "")
        .args(["-u", "root", "-p", GSERVER_PASSWORD]);
    let r = run(
        cmd,
        &format!(
            "create small {data_dir}/small.nt;\nuse small;\nselect (count(*) as ?n) where {{ ?s ?p ?o }};\n"
        ),
    );
    drop(server);
    assert_eq!(r.code, 0, "{r:?}");
    assert!(
        r.out.contains("Build RDF database small successfully!"),
        "{r:?}"
    );
    assert!(
        r.out
            .contains("\"25\"^^<http://www.w3.org/2001/XMLSchema#integer>"),
        "{r:?}"
    );
    assert!(dir.join("dbhome/small.db").is_dir());
}

/// Round-5 review follow-up (ext#5): Ctrl-C in `gconsole --local` left
/// `SIGINT` at its default action, so it killed the process — and with it
/// every update the console had acknowledged (`[updated N triple(s)]`) to
/// a snapshot-layout database but not yet saved. As in the remote console,
/// it now discards the statement being typed; the acknowledged update is
/// saved when the console is left.
#[cfg(unix)]
#[test]
fn ctrl_c_in_the_local_mode_keeps_the_acknowledged_updates() {
    let cwd = scratch("gc_local_ctrl_c");
    let _cwd = DirGuard(cwd.clone());
    let kept = snapshot_db(&cwd, "kept", "<http://a> <http://p> <http://b> .\n");
    let mut child = Command::new(env!("CARGO_BIN_EXE_gconsole"))
        .current_dir(&cwd)
        .args(["--local", "kept"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let out = Live::start(child.stdout.take().unwrap());
    let err = Live::start(child.stderr.take().unwrap());
    // Wait for `marker`, failing at once if the console is gone (the
    // timeout is only a hang guard).
    let shown = |child: &mut Child, what: &str, marker: &str, from: usize| {
        let t0 = Instant::now();
        loop {
            let text = out.text();
            if let Some(i) = text.get(from..).and_then(|t| t.find(marker)) {
                return from + i + marker.len();
            }
            if let Some(status) = child.try_wait().unwrap() {
                panic!("gconsole ended ({status}) before {what}: {text}");
            }
            assert!(
                t0.elapsed() < Duration::from_secs(60),
                "timed out waiting for {what}: {text}"
            );
            thread::sleep(Duration::from_millis(10));
        }
    };
    stdin
        .write_all(b"insert data { <http://a> <http://p> <http://c> };\n")
        .unwrap();
    let at = shown(
        &mut child,
        "the acknowledgement",
        "[updated 1 triple(s)]",
        0,
    );
    let at = shown(&mut child, "the next prompt", "gsql> ", at);
    // A statement being typed; Ctrl-C once its continuation prompt shows.
    stdin
        .write_all(b"insert data { <http://a> <http://p> <http://d> }\n")
        .unwrap();
    let at = shown(&mut child, "the continuation prompt", "  ..> ", at);
    sigint(&child);
    let at = shown(&mut child, "a fresh prompt after Ctrl-C", "gsql> ", at);
    stdin.write_all(b"stats;\n").unwrap();
    drop(stdin);
    let code = wait(&mut child);
    let (out, err) = (out.finish(), err.finish());
    assert_eq!(code, 0, "{out}\n{err}");
    assert!(
        out[at..].contains("name=kept triples=2"),
        "the interrupted statement was discarded: {out}\n{err}"
    );
    assert!(out[at..].contains("saved to "), "{out}\n{err}");
    assert_eq!(
        triples_in(&kept),
        2,
        "the acknowledged insert survived the Ctrl-C: {out}\n{err}"
    );
}
