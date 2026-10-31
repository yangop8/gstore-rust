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
    let mut cmd = gconsole(&srv.addr, cwd);
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
    // Nothing listening.
    let free = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let mut cmd = gconsole(&free, &cwd);
    cmd.args(["-u", "root", "-p", PASSWORD]);
    let r = run(cmd, "showdbs;\n");
    assert_eq!(r.code, 1, "{r:?}");
    assert_eq!(
        r.out, "Could not connect to server. Please check server status\n",
        "{r:?}"
    );
    assert!(r.err.contains(&format!("{}", free.port())), "{r:?}");
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

#[test]
fn reasoning_and_function_commands_show_the_servers_answer() {
    let srv = Server::start("gc_reason");
    ok(&srv.build("small", &data("small/small.nt")));
    let cwd = scratch("gc_reason_cwd");
    let _cwd = DirGuard(cwd.clone());
    fs::write(
        cwd.join("rule.json"),
        r#"{"rulename":"r1","description":"d"}"#,
    )
    .unwrap();
    fs::write(cwd.join("bad.json"), "{not json").unwrap();
    fs::write(cwd.join("fun.json"), r#"{"funName":"f1","funBody":"{}"}"#).unwrap();
    let r = script(
        &srv,
        &cwd,
        "listreason;\n\
         use small;\n\
         addreason missing.json;\n\
         addreason bad.json;\n\
         addreason rule.json;\n\
         listreason;\n\
         compilereason r1;\n\
         executereason r1;\n\
         disablereason r1;\n\
         showreason r1;\n\
         deletereason r1;\n\
         showpfn;\n\
         addpfn fun.json;\n\
         addpfn missing.json;\n\
         mdfpfn fun.json;\n\
         delpfn f1;\n\
         buildpfn f1;\n",
    );
    let out = &r.out;
    assert!(out.starts_with("Current database not selected."), "{r:?}");
    assert!(out.contains("File missing.json does not exist.\n"), "{r:?}");
    assert!(out.contains("failed to parse file bad.json: "), "{r:?}");
    // Until the server implements these operations, the console shows its
    // answer (`1100`, "The operation is not defined") cleanly.
    let reason = srv.op("reasonManage", &[("db_name", "small"), ("type", "2")]);
    let fun = srv.op("funquery", &[]);
    if harness::code(&reason) == 1100 {
        let undefined = "The operation is not defined";
        for line in [
            format!("{undefined}\n"),
            format!("list reasons failed: {undefined}\n"),
            format!("compile reason r1 failed: {undefined}\n"),
            format!("execute reason r1 failed: {undefined}\n"),
            format!("disable reason r1 failed: {undefined}\n"),
            format!("show reason r1 failed: {undefined}\n"),
            format!("delete reason r1 failed: {undefined}\n"),
        ] {
            assert!(out.contains(&line), "{line:?}: {r:?}");
        }
    }
    if harness::code(&fun) == 1100 {
        let undefined = "The operation is not defined";
        for line in [
            format!("failed to query custom function: {undefined}\n"),
            format!("function operation failed: {undefined}\nfailed to create custom function\n"),
            format!("function operation failed: {undefined}\nfailed to update custom function\n"),
            format!("function operation failed: {undefined}\nfailed to delete custom function\n"),
            format!("function operation failed: {undefined}\nfailed to build custom function\n"),
        ] {
            assert!(out.contains(&line), "{line:?}: {r:?}");
        }
    }
    assert_eq!(r.code, 1, "{r:?}");
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
        "| f1   | d    | ./pfn/f1.cpp | 2      | 2024-01-01 00:00:00 |\n",
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
    let mut cmd = gconsole(&fake.addr, &cwd);
    cmd.args(["-u", "root", "-p", PASSWORD]);
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let (out, err) = collect(&mut child);
    stdin.write_all(b"use slow;\n").unwrap();
    wait_for("load", || !fake.ops("load").is_empty());
    // Ctrl-C while a statement is being typed drops it.
    stdin.write_all(b"select * where {\n").unwrap();
    thread::sleep(Duration::from_millis(400));
    sigint(&child);
    thread::sleep(Duration::from_millis(400));
    // Ctrl-C while the server works on a query: the connection is dropped
    // and the server's task cancelled.
    stdin.write_all(b"select ?x where { ?x ?p ?o };\n").unwrap();
    wait_for("the query", || !fake.ops("query").is_empty());
    thread::sleep(Duration::from_millis(200));
    sigint(&child);
    wait_for("the cancellation", || {
        !fake.ops("operationTaskCancel").is_empty()
    });
    // The console is still there.
    stdin.write_all(b"pdb;\n").unwrap();
    drop(stdin);
    let code = wait(&mut child);
    let (out, err) = (out.join().unwrap(), err.join().unwrap());
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
    assert!(out.contains("Query cancelled.\n"), "{out}\n{err}");
    assert!(
        out.ends_with("slow\n\n"),
        "pdb ran after the cancel: {out}\n{err}"
    );
    // A cancelled command counts as a failure of the script.
    assert_eq!(code, 1, "{out}\n{err}");
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

// ---- the README flow: gserver + conf.ini + gconsole -u root ---------------------

#[cfg(unix)]
#[test]
fn gconsole_finds_gserver_through_conf_ini() {
    let dir = scratch("gc_readme");
    let _dir = DirGuard(dir.clone());
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    fs::create_dir_all(dir.join("conf")).unwrap();
    fs::write(
        dir.join("conf/conf.ini"),
        format!("[server]\nport={port}\nthread_num=4\n[system]\ndb_home=./dbhome/\n"),
    )
    .unwrap();
    let data_dir = path_str(&data("small"));
    struct Kill(Child);
    impl Drop for Kill {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let server = Kill(
        Command::new(env!("CARGO_BIN_EXE_gserver"))
            .current_dir(&dir)
            .args(["--load-dir", &data_dir])
            .env_remove("GSTORE_GSERVER_BACKGROUND")
            .env_remove("GSTORE_GSERVER_SUPERVISOR")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    wait_for("gserver", || {
        TcpStream::connect(("127.0.0.1", port)).is_ok()
    });
    // No --port: gconsole reads it from ./conf/conf.ini, like upstream.
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_gconsole"));
    cmd.current_dir(&dir)
        .env("GSTORE_HISTFILE", "")
        .args(["-u", "root", "-p", PASSWORD]);
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
