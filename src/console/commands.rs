//! The console's commands (upstream `gconsole.cpp`'s `commands[]` table and
//! its handlers): each sends the `/api` operation upstream's handler sends,
//! with the same parameters, and prints what upstream prints.
//!
//! Differences from upstream, all deliberate:
//!
//! * requests carry the logged-in user's credentials (see
//!   [`super::client`]), so the **server** enforces privileges — upstream's
//!   console-side privilege cache (`check_priv`, `flushpriv`) has nothing to
//!   cache, and a refused operation prints the server's reason (preceded by
//!   upstream's `Permission denied. …` line);
//! * results are requested as JSON (`format=json`) and printed as upstream's
//!   `format=n-triple` table (an unbound cell stays an empty cell instead of
//!   shifting the row);
//! * `sparql <file>` runs each `;`-separated statement of the file, as
//!   upstream's documentation describes (its 1.4 code sends the whole file
//!   as one query — the same for a one-statement file);
//! * a local file named by `create`, `batchinsert`, `batchremove` or
//!   `restore` is sent as an absolute path when the server is on this
//!   machine (its working directory may differ from the console's);
//! * `backup` / `export` also print the path the server wrote;
//! * upstream's typos in messages are kept, its crashes are not (a missing
//!   or malformed JSON file, an unknown user).
//!
//! Extensions: `exit` (a synonym of `quit`) and `history`.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use super::client::{ApiClient, CallError};
use super::syntax;
use super::table;
use super::term::{EchoOff, LineReader, ReadOutcome};
use crate::json::{self, Json};

/// Upstream's product name, version and site (`GlobalTypedef`).
pub const PRODUCT_NAME: &str = "gStore";
pub const PRODUCT_VERSION: &str = "1.4";
pub const PRODUCT_WEBSITE: &str = "https://www.gstore.cn";
/// The system database (`GlobalTypedef::system_db`).
pub const SYSTEM_DB: &str = "system";
/// The built-in administrator (`conf.ini` `root_username`).
pub const ROOT_USER: &str = "root";
/// Upstream `MAX_WRONG_PSWD_TIMES`.
pub const MAX_WRONG_PSWD_TIMES: usize = 7;
/// The prompt of a statement's second and later lines (upstream
/// `CROSS_LINE_PROMPT`).
pub const CROSS_LINE_PROMPT: &str = "     -> ";
/// The privileges, by upstream's numbers 1..=7 (`priv_offset2name`).
const PRIVILEGE_NAMES: [&str; 8] = [
    "root", "query", "load", "unload", "update", "backup", "restore", "export",
];
/// `showuser`'s privilege fields, by privilege number.
const PRIVILEGE_FIELDS: [(usize, &str); 7] = [
    (1, "query_privilege"),
    (2, "load_privilege"),
    (3, "unload_privilege"),
    (4, "update_privilege"),
    (5, "backup_privilege"),
    (6, "restore_privilege"),
    (7, "export_privilege"),
];
/// The order upstream's `pusr` / `showusrs` list a user's privileges in.
const LISTING_ORDER: [(&str, &str); 7] = [
    ("query_privilege", "query"),
    ("load_privilege", "load"),
    ("update_privilege", "update"),
    ("unload_privilege", "unload"),
    ("backup_privilege", "backup"),
    ("restore_privilege", "restore"),
    ("export_privilege", "export"),
];

/// Why a command did not complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fail {
    /// It failed (and said why).
    Failed,
    /// Ctrl-C.
    Cancelled,
}

pub type CmdResult = Result<(), Fail>;
type Handler = fn(&mut Console, &[String]) -> CmdResult;

/// One entry of the command table (upstream `COMMAND`).
pub struct Command {
    pub name: &'static str,
    pub doc: &'static str,
    pub usage: &'static str,
    handler: Option<Handler>,
}

const fn cmd(name: &'static str, handler: Handler, doc: &'static str, usage: &'static str) -> Command {
    Command {
        name,
        doc,
        usage,
        handler: Some(handler),
    }
}

/// The commands, in upstream's order (plus `exit` and `history`); the last
/// entry stands for SPARQL typed at the prompt.
pub static COMMANDS: &[Command] = &[
    cmd("quit", h_quit, "Quit this console.", "quit;"),
    cmd("exit", h_quit, "Synonym for \"quit\".", "exit;"),
    cmd("refreshconf", h_refreshconf, "Refresh the configuration params.", "refreshconf;"),
    cmd("init", h_init, "Initializes the existing database to the system library.", "init <database_name>[,anothers];"),
    cmd("sparql", h_sparql, "Answer SPARQL query(s) in file.", "sparql <sparql_file_path>;"),
    cmd("create", h_create, "Build a database from a dataset or create an empty database.", "create <database_name> [<nt_file_path>];"),
    cmd("use", h_use, "Set current database.", "use <database_name>;"),
    cmd("drop", h_drop, "Drop a database.", "drop <database_name>;"),
    cmd("show", h_show, "Show info and specified number of triples of current database or other database.", "show [<database_name>];"),
    cmd("showdbs", h_showdbs, "Display all databases the current user has query privilege on.", "showdbs;"),
    cmd("backup", h_backup, "Backup current database.", "backup [<backup_path>];"),
    cmd("restore", h_restore, "Restore a database.", "restore <database_name> <backup_path>;"),
    cmd("export", h_export, "Export a database to .nt file.", "export [<file_path>];"),
    cmd("pdb", h_pdb, "Display current database name.", "pdb;"),
    cmd("unload", h_unload, "Unload the current database.", "unload;"),
    cmd("batchinsert", h_batchinsert, "Batch inserts data into the current database.", "batchinsert <nt_file_path>;"),
    cmd("batchremove", h_batchremove, "Batch deletes the current database data.", "batchremove <nt_file_path>;"),
    cmd("addreason", h_addreason, "add reason rule into current database.", "addreason <json_file_path>;"),
    cmd("listreason", h_listreason, "show all reason rules of current database.", "listreason;"),
    cmd("compilereason", h_compilereason, "compile reason rule of current database with rulename.", "compilereason <rule_name>;"),
    cmd("executereason", h_executereason, "execute reason rule of current database with rulename.", "executereason <rule_name>;"),
    cmd("disablereason", h_disablereason, "disable reason rule of current database with rulename.", "disablereason <rule_name>;"),
    cmd("showreason", h_showreason, "show detailed information of some rule with rulename in current database.", "showreason <rule_name>;"),
    cmd("deletereason", h_deletereason, "delete reason rule of current database with rulename.", "deletereason <rule_name>;"),
    cmd("flushpriv", h_flushpriv, "Flush priv for current user, updating the in-memory structure.", "flushpriv;"),
    cmd("pusr", h_pusr, "Display user's username and privilege.", "pusr; pusr <database_name>; pusr <database_name> <usr_name>;"),
    cmd("setpswd", h_setpswd, "Set your password. Be able to set other's password if you are root.", "setpswd; setpswd <usrname>;"),
    cmd("setpriv", h_setpriv, "Set user's privilege.", "setpriv <usrname> <database_name>;"),
    cmd("clearpriv", h_clearpriv, "clear users's privilege.", "clearpriv <usrname>"),
    cmd("addusr", h_addusr, "Add user.", "addusr <usrname>;"),
    cmd("delusr", h_delusr, "Del user.", "delusr <usrname>;"),
    cmd("showusrs", h_showusrs, "Show all users and privilege for each.", "showusrs;"),
    cmd("showpfn", h_showpfn, "query custom function.", "showpfn"),
    cmd("addpfn", h_addpfn, "create custom function.", "addpfn <json_file_path>"),
    cmd("mdfpfn", h_mdfpfn, "update custom function.", "mdfpfn <json_file_path>"),
    cmd("delpfn", h_delpfn, "delete custom function.", "delpfn <func_name>"),
    cmd("buildpfn", h_buildpfn, "build custom function.", "buildpfn <func_name>"),
    cmd("help", h_help, "Display help msg. Enter 'help;' see more about usage.", "help [edit/usage/<command>];"),
    cmd("?", h_help, "Synonym for \"help\".", "help [edit/usage/<command>];"),
    cmd("usage", h_usage, "Display all the commands and their usage", "usage;"),
    cmd("version", h_version, "Display  core version.", "version;"),
    cmd("pwd", h_pwd, "Print name of current/working directory.", "pwd;"),
    cmd("clear", h_clear, "Clear screen.", "clear;"),
    cmd("history", h_history, "Display the command history (gstore-rust).", "history [<number>];"),
    Command {
        name: "raw_sparql",
        doc: "Support enter sparql query directedly in gconsole.",
        usage: "Begin with SELECT, INSERT, DELETE, PREFIX or BASE. For more about SPARQL, see https://www.w3.org/TR/sparql11-query/ ",
        handler: None,
    },
];

/// The command named `name` (case-insensitive; `raw_sparql` is not one).
pub fn find_command(name: &str) -> Option<&'static Command> {
    let lower = name.to_ascii_lowercase();
    COMMANDS
        .iter()
        .find(|c| c.handler.is_some() && c.name == lower)
}

/// The names [`find_command`] knows (for completion).
pub fn command_names() -> Vec<&'static str> {
    COMMANDS
        .iter()
        .filter(|c| c.handler.is_some())
        .map(|c| c.name)
        .collect()
}

/// Upstream's version lines (`PRINT_VERSION`), plus this implementation's.
pub fn version_text() -> String {
    format!(
        "{PRODUCT_NAME} version: {PRODUCT_VERSION} Source distribution\n\
         Copyright (c) 2016-{}, pkumod and topgraph and/or its affiliates.\n\
         gstore-rust {}\n",
        current_year(),
        env!("CARGO_PKG_VERSION")
    )
}

/// This year (local time).
pub fn current_year() -> i64 {
    crate::sysdb::clock::now().year
}

/// The console's command history: one line per statement, kept in a file
/// (at most [`History::MAX`] entries, the newest last).
#[derive(Debug, Default)]
pub struct History {
    path: Option<PathBuf>,
    entries: Vec<String>,
}

impl History {
    /// Upstream's `max_history`.
    pub const MAX: usize = 1024;

    /// The history kept in `path` (`None`: in memory only).
    pub fn open(path: Option<PathBuf>) -> History {
        let mut entries: Vec<String> = path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|t| {
                t.lines()
                    .filter(|l| !l.trim().is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        if entries.len() > History::MAX {
            entries.drain(..entries.len() - History::MAX);
            if let Some(p) = &path {
                let _ = std::fs::write(p, entries.join("\n") + "\n");
            }
        }
        History { path, entries }
    }

    pub fn entries(&self) -> &[String] {
        &self.entries
    }

    /// Record `entry` (a statement on one line) unless it repeats the last.
    pub fn add(&mut self, entry: &str) {
        let entry = entry.trim();
        if entry.is_empty() || self.entries.last().is_some_and(|l| l == entry) {
            return;
        }
        self.entries.push(entry.to_string());
        if let Some(p) = &self.path {
            if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(p) {
                let _ = writeln!(f, "{entry}");
            }
        }
        if self.entries.len() > History::MAX {
            self.entries.remove(0);
        }
    }
}

/// Where command output goes: stdout, or a redirection's file.
enum Out {
    Stdout,
    File(BufWriter<File>),
}

impl Write for Out {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Out::Stdout => io::stdout().write(buf),
            Out::File(f) => f.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Out::Stdout => io::stdout().flush(),
            Out::File(f) => f.flush(),
        }
    }
}

/// Print a line of command output.
macro_rules! say {
    ($c:expr) => {{
        let _ = writeln!($c.out);
    }};
    ($c:expr, $($t:tt)*) => {{
        let _ = writeln!($c.out, $($t)*);
    }};
}

/// Print a line of command output and fail the command.
macro_rules! bail {
    ($c:expr, $($t:tt)*) => {{
        let _ = writeln!($c.out, $($t)*);
        return Err(Fail::Failed);
    }};
}

/// An `/api` answer.
#[derive(Debug, Clone)]
pub struct Resp {
    /// `StatusCode` (`-1` when there was no answer).
    pub code: i64,
    /// `StatusMsg` (or why there was no answer).
    pub msg: String,
    pub json: Json,
}

impl Resp {
    fn from_json(json: Json) -> Resp {
        let code = json
            .get("StatusCode")
            .and_then(|v| v.as_i64().or_else(|| v.as_str()?.trim().parse().ok()))
            .unwrap_or(-1);
        let msg = json
            .get("StatusMsg")
            .map(text_of)
            .unwrap_or_default();
        Resp { code, msg, json }
    }

    fn no_answer(e: &CallError) -> Resp {
        Resp {
            code: -1,
            msg: e.to_string(),
            json: Json::obj(),
        }
    }

    pub fn ok(&self) -> bool {
        self.code == 0
    }

    /// Field `key` as text (`""` when absent).
    pub fn text(&self, key: &str) -> String {
        self.json.get(key).map(text_of).unwrap_or_default()
    }

    /// Field `key` as text, `"0"` when absent (upstream's numeric defaults).
    pub fn num(&self, key: &str) -> String {
        self.json
            .get(key)
            .map(text_of)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "0".to_string())
    }

    /// Whether the server refused for lack of a privilege.
    fn no_privilege(&self) -> bool {
        self.msg.starts_with("You have no ")
    }
}

/// A JSON value as text: a string's contents, a number's lexical form.
fn text_of(v: &Json) -> String {
    match v {
        Json::Str(s) => s.clone(),
        Json::Num(n) => n.clone(),
        Json::Bool(b) => b.to_string(),
        Json::Null => String::new(),
        other => other.to_string(),
    }
}

/// The console: the connection, the session's state, the command handlers'
/// I/O.
pub struct Console {
    pub client: ApiClient,
    /// The current database (`""`: none).
    pub current_db: String,
    /// Whether prompts (passwords, privilege numbers) are shown: a person is
    /// typing. Off, prompts read their lines from the script silently.
    pub show_prompts: bool,
    /// The input every prompt reads.
    pub reader: LineReader,
    pub history: History,
    /// Database names, for completion.
    pub db_names: Rc<RefCell<Vec<String>>>,
    /// Set by `quit`.
    pub quit: bool,
    /// Commands that failed or were cancelled.
    pub failures: usize,
    out: Out,
}

impl Console {
    pub fn new(client: ApiClient) -> Console {
        Console {
            client,
            current_db: String::new(),
            show_prompts: false,
            reader: LineReader::new(),
            history: History::default(),
            db_names: Rc::new(RefCell::new(Vec::new())),
            quit: false,
            failures: 0,
            out: Out::Stdout,
        }
    }

    /// The logged-in user.
    pub fn user(&self) -> &str {
        &self.client.username
    }

    fn is_root(&self) -> bool {
        self.client.username == ROOT_USER
    }

    /// The prompt: `gStore[no database]> ` or `gStore[<db>]> `.
    pub fn prompt(&self) -> String {
        if self.current_db.is_empty() {
            format!("{PRODUCT_NAME}[no database]> ")
        } else {
            format!("{PRODUCT_NAME}[{}]> ", self.current_db)
        }
    }

    // ---- input -----------------------------------------------------------

    /// Read one line for a prompt (shown only to a person), echo off when
    /// `hidden`. `None` at the end of input or on Ctrl-C.
    pub fn prompt_line(&mut self, prompt: &str, hidden: bool) -> Option<String> {
        if self.show_prompts {
            print!("{prompt}");
            let _ = io::stdout().flush();
        }
        let line = {
            let _echo = hidden.then(EchoOff::new);
            self.reader.read_line()
        };
        if hidden && self.show_prompts {
            println!();
        }
        match line {
            ReadOutcome::Line(l) => Some(l),
            ReadOutcome::Eof => {
                println!("End of stdin!");
                None
            }
            ReadOutcome::Interrupted => None,
        }
    }

    /// Ask for the logged-in user's password and compare it with the one
    /// they logged in with (upstream's identity check before user and
    /// privilege management).
    fn verify_password(&mut self, failure: &str) -> CmdResult {
        let Some(pw) = self.prompt_line("Enter your password: ", true) else {
            return Err(Fail::Failed);
        };
        if pw != self.client.password {
            bail!(self, "Fail to varify your id. {failure}");
        }
        Ok(())
    }

    // ---- requests --------------------------------------------------------

    /// Send `op` with `params`; Ctrl-C drops the connection
    /// ([`Fail::Cancelled`]). No answer at all is a [`Resp`] with code `-1`
    /// and the reason as its message.
    pub fn request(&mut self, op: &str, params: Vec<(&str, Json)>) -> Result<Resp, Fail> {
        self.request_with(op, params, None)
    }

    fn request_with(
        &mut self,
        op: &str,
        params: Vec<(&str, Json)>,
        query: Option<(&str, &str)>,
    ) -> Result<Resp, Fail> {
        let body = self.client.body(op, params);
        match self.client.call_interruptible(&body) {
            Ok(j) => Ok(Resp::from_json(j)),
            Err(CallError::Interrupted) => {
                let _ = self.out.flush();
                match query {
                    Some((db, sparql)) => {
                        let n = self.cancel_query(db, sparql);
                        if n > 0 {
                            println!("Query cancelled.");
                        } else {
                            println!("Query cancelled (the connection was dropped).");
                        }
                    }
                    None => println!(
                        "Cancelled: the connection was dropped; the server may still complete the operation."
                    ),
                }
                Err(Fail::Cancelled)
            }
            Err(e) => Ok(Resp::no_answer(&e)),
        }
    }

    /// Cancel the server's task running `sparql` on `db` for this user
    /// (found with `operationTaskList`, cancelled with
    /// `operationTaskCancel`): how many were cancelled.
    fn cancel_query(&mut self, db: &str, sparql: &str) -> usize {
        let timeout = Some(Duration::from_secs(10));
        for attempt in 0..3 {
            if attempt > 0 {
                std::thread::sleep(Duration::from_millis(150));
            }
            let body = self.client.body(
                "operationTaskList",
                vec![
                    ("status", Json::from("0")),
                    ("pageNo", Json::from("1")),
                    ("pageSize", Json::from("1000")),
                ],
            );
            let Ok(list) = self.client.call(&body, timeout) else {
                return 0;
            };
            let ids: Vec<String> = list
                .get("list")
                .and_then(Json::as_array)
                .unwrap_or_default()
                .iter()
                .filter(|t| {
                    let field = |k: &str| t.get(k).map(text_of).unwrap_or_default();
                    field("operation") == "query"
                        && field("database") == db
                        && field("sparql") == sparql
                        && field("async") != "1"
                        && t.get("user")
                            .is_none_or(|u| text_of(u) == self.client.username)
                })
                .filter_map(|t| t.get("opt_id").map(text_of))
                .collect();
            if ids.is_empty() {
                continue;
            }
            let mut cancelled = 0;
            for id in ids {
                let body = self
                    .client
                    .body("operationTaskCancel", vec![("opt_id", Json::from(id))]);
                if self
                    .client
                    .call(&body, timeout)
                    .is_ok_and(|r| Resp::from_json(r).ok())
                {
                    cancelled += 1;
                }
            }
            return cancelled;
        }
        0
    }

    /// `path` as the server should get it: an absolute path when the server
    /// is on this machine and the file exists here (the server's working
    /// directory may not be ours), else as given.
    fn server_path(&self, path: &str) -> String {
        if self.client.is_local() && Path::new(path).exists() {
            if let Ok(abs) = std::fs::canonicalize(path) {
                return abs.to_string_lossy().into_owned();
            }
        }
        path.to_string()
    }

    // ---- checks ----------------------------------------------------------

    /// Upstream `CHECK_ARGC`.
    fn argc(&mut self, args: &[String], allowed: &[usize], name: &str) -> CmdResult {
        if allowed.contains(&args.len()) {
            return Ok(());
        }
        let usage = find_command(name).map(|c| c.usage).unwrap_or_default();
        say!(self, "Wrong usage.");
        bail!(self, "Usage: {usage}");
    }

    /// Upstream `CHECK_CURRENT_DB_LOADED`.
    fn need_db(&mut self) -> Result<String, Fail> {
        if self.current_db.is_empty() {
            bail!(
                self,
                "Current database not selected. Please select it first, through \"USE <database_name>\"."
            );
        }
        Ok(self.current_db.clone())
    }

    /// Upstream `CHECK_CURRENT_DB_NOT_SYSDB`.
    fn not_sysdb(&mut self) -> CmdResult {
        if self.current_db == SYSTEM_DB {
            bail!(self, "You can NOT do this for system database.");
        }
        Ok(())
    }

    /// Upstream's `check_priv` message, when the server refused `r` for
    /// lack of a privilege on `db`.
    fn privilege_hint(&mut self, r: &Resp, db: &str) {
        if r.no_privilege() {
            say!(
                self,
                "Permission denied. Check your privilege with database: {db}"
            );
        }
    }

    fn remember_db(&self, db: &str) {
        let mut names = self.db_names.borrow_mut();
        if !names.iter().any(|n| n == db) {
            names.push(db.to_string());
            names.sort();
        }
    }

    fn forget_db(&self, db: &str) {
        self.db_names.borrow_mut().retain(|n| n != db);
    }

    /// Refresh [`db_names`](Self::db_names) from `show` (quietly).
    pub fn refresh_db_names(&mut self) {
        let body = self.client.body("show", vec![]);
        if let Ok(j) = self.client.call(&body, Some(Duration::from_secs(10))) {
            let mut names: Vec<String> = j
                .get("ResponseBody")
                .and_then(Json::as_array)
                .unwrap_or_default()
                .iter()
                .filter_map(|d| d.get("database").map(text_of))
                .collect();
            names.sort();
            *self.db_names.borrow_mut() = names;
        }
    }

    // ---- statements ------------------------------------------------------

    /// Run one statement (without its `;`): output redirection, then the
    /// command or SPARQL, then upstream's blank line.
    pub fn execute(&mut self, stmt: &str) {
        let (body, redirect) = syntax::split_redirect(stmt);
        if let Some(r) = &redirect {
            println!("Redirect output to file: {}", r.path);
            let file = if r.append {
                OpenOptions::new().create(true).append(true).open(&r.path)
            } else {
                File::create(&r.path)
            };
            match file {
                Ok(f) => self.out = Out::File(BufWriter::new(f)),
                Err(_) => {
                    println!("Failed to open file: {}", r.path);
                    self.failures += 1;
                    return;
                }
            }
        }
        let result = self.dispatch(&body);
        if !self.quit {
            say!(self);
        }
        let _ = self.out.flush();
        self.out = Out::Stdout;
        if result.is_err() {
            self.failures += 1;
        }
    }

    fn dispatch(&mut self, body: &str) -> CmdResult {
        if syntax::is_sparql(body) {
            return raw_sparql(self, body);
        }
        let Ok((name, args)) = syntax::parse_command(body) else {
            bail!(self, "Invalid arguments!");
        };
        let Some(command) = find_command(&name) else {
            let word = syntax::first_word(body);
            bail!(self, "{word}: No such command for gconsole.");
        };
        let handler = command.handler.expect("commands have handlers");
        handler(self, &args)
    }
}

// ---------------------------------------------------------------------------
// SPARQL
// ---------------------------------------------------------------------------

/// SPARQL typed at the prompt (upstream `raw_sparql_handler`): `query` on
/// the current database; a result table and `Query ans num N, use T ms.`,
/// or `Update ans num N, use T ms.`. Ctrl-C cancels the server's task too.
fn raw_sparql(c: &mut Console, sparql: &str) -> CmdResult {
    let db = c.need_db()?;
    let r = c.request_with(
        "query",
        vec![
            ("db_name", Json::from(db.as_str())),
            ("sparql", Json::from(sparql)),
            ("format", Json::from("json")),
        ],
        Some((&db, sparql)),
    )?;
    if !r.ok() {
        c.privilege_hint(&r, &db);
        bail!(c, "Query failed: {}", r.msg);
    }
    let ans = r.num("AnsNum");
    let time = r.num("QueryTime");
    let Some((vars, rows)) = result_table(&r.json) else {
        say!(c, "Update ans num {ans}, use {time} ms.");
        return Ok(());
    };
    if let Some(value) = single_literal(&r.json) {
        // A graph function's JSON answer, shown indented.
        if value.starts_with('{') && value.ends_with('}') {
            if let Ok(doc) = json::parse(&value) {
                say!(c, "{}", table::pretty_json(&doc.into_sorted()));
                say!(c, "Query ans num {ans}, use {time} ms.");
                return Ok(());
            }
        }
        if let Some(rest) = value.strip_prefix("pfn_type") {
            say!(c, "{rest}");
            say!(c, "pfn query use {time} ms.");
            return Ok(());
        }
    }
    let _ = write!(c.out, "{}", table::render_owned(&vars, &rows));
    say!(c, "Query ans num {ans}, use {time} ms.");
    Ok(())
}

/// The lexical form of an answer's only cell, when it has one row of one
/// literal (upstream checks such a cell for a JSON document — a graph
/// function's result — and for a `pfn_type` answer).
fn single_literal(j: &Json) -> Option<String> {
    let results = j.get("results")?;
    if let Some(bindings) = results.get("bindings").and_then(Json::as_array) {
        let [row] = bindings else { return None };
        let [(_, b)] = row.as_object()? else {
            return None;
        };
        let kind = b.get("type").and_then(Json::as_str)?;
        if kind != "literal" && kind != "typed-literal" {
            return None;
        }
        return b.get("value").and_then(Json::as_str).map(str::to_string);
    }
    // Upstream's `n-triple` rows: `"lexical form"` with `\"` escapes.
    let [row] = results.as_array()? else {
        return None;
    };
    let [cell] = row.as_array()? else {
        return None;
    };
    let inner = cell.as_str()?.strip_prefix('"')?.strip_suffix('"')?;
    Some(inner.replace("\\\"", "\""))
}

/// A query answer's header (`?`-prefixed variables) and rows of N-Triples
/// terms; `None` for an update's answer (no `head`). Reads the JSON format
/// (`head.vars` + `results.bindings`) and upstream's `n-triple` format
/// (`head` + `results` as arrays).
pub fn result_table(j: &Json) -> Option<(Vec<String>, Vec<Vec<String>>)> {
    let head = j.get("head")?;
    let vars: Vec<String> = head
        .get("vars")
        .unwrap_or(head)
        .as_array()?
        .iter()
        .map(text_of)
        .collect();
    if vars.is_empty() {
        return None;
    }
    let names: Vec<&str> = vars.iter().map(|v| v.trim_start_matches('?')).collect();
    let results = j.get("results");
    let rows: Vec<Vec<String>> = match results.and_then(|r| r.get("bindings")).and_then(Json::as_array) {
        Some(bindings) => bindings
            .iter()
            .map(|b| {
                names
                    .iter()
                    .map(|v| b.get(v).map(table::binding_term).unwrap_or_default())
                    .collect()
            })
            .collect(),
        None => results
            .and_then(Json::as_array)
            .unwrap_or_default()
            .iter()
            .map(|row| row.as_array().unwrap_or_default().iter().map(text_of).collect())
            .collect(),
    };
    let header = names.iter().map(|v| format!("?{v}")).collect();
    Some((header, rows))
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

fn h_quit(c: &mut Console, args: &[String]) -> CmdResult {
    c.argc(args, &[0], "quit")?;
    c.current_db.clear();
    c.quit = true;
    Ok(())
}

fn h_refreshconf(c: &mut Console, _args: &[String]) -> CmdResult {
    let r = c.request("refreshconf", vec![])?;
    if !r.ok() {
        bail!(c, "Refresh config failed: {}", r.msg);
    }
    say!(c, "Refresh config successfully.");
    Ok(())
}

fn h_init(c: &mut Console, args: &[String]) -> CmdResult {
    c.argc(args, &[1], "init")?;
    let names = &args[0];
    if names.contains(SYSTEM_DB) {
        bail!(c, "You can NOT init system database. ");
    }
    let r = c.request("init", vec![("db_names", Json::from(names.as_str()))])?;
    if !r.ok() {
        bail!(c, "Init database {names} failed: {}", r.msg);
    }
    say!(c, "Init database result: ");
    let rows: Vec<Vec<String>> = r
        .json
        .get("data")
        .and_then(Json::as_array)
        .unwrap_or_default()
        .iter()
        .map(|d| {
            ["db_name", "status", "msg"]
                .iter()
                .map(|k| d.get(k).map(text_of).unwrap_or_default())
                .collect()
        })
        .collect();
    let _ = write!(c.out, "{}", table::render(&["db_name", "status", "msg"], &rows));
    for row in &rows {
        if row[1] == "0" {
            c.remember_db(&row[0]);
        }
    }
    Ok(())
}

fn h_sparql(c: &mut Console, args: &[String]) -> CmdResult {
    c.argc(args, &[1], "sparql")?;
    c.need_db()?;
    let Ok(text) = std::fs::read_to_string(&args[0]) else {
        bail!(c, "File open failed: {}", args[0]);
    };
    let mut result = Ok(());
    for stmt in syntax::split_statements(&text) {
        match raw_sparql(c, &stmt) {
            Err(Fail::Cancelled) => return Err(Fail::Cancelled),
            Err(f) => result = Err(f),
            Ok(()) => {}
        }
        say!(c);
    }
    result
}

/// The files a `create` from directory `dir` reads (upstream
/// `FileUtil::dir_filepaths`: the regular files in it and, recursively, in
/// its subdirectories), sorted.
fn dir_files(dir: &Path) -> Vec<String> {
    fn walk(dir: &Path, depth: usize, out: &mut Vec<String>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in rd.flatten() {
            let path = entry.path();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() && depth < 32 {
                walk(&path, depth + 1, out);
            } else if path.is_file() {
                out.push(path.to_string_lossy().into_owned());
            }
        }
    }
    let mut files = Vec::new();
    walk(dir, 0, &mut files);
    files.sort();
    files
}

fn is_remote(path: &str) -> bool {
    path.starts_with("http://") || path.starts_with("https://")
}

fn h_create(c: &mut Console, args: &[String]) -> CmdResult {
    c.argc(args, &[1, 2], "create")?;
    let db = args[0].clone();
    if db.chars().count() < 3 {
        bail!(
            c,
            "build param error, need param num at least 1 or database name need at least 3 character"
        );
    }
    if db == SYSTEM_DB {
        bail!(c, "Your db name can NOT be \"system\".");
    }
    let mut files = Vec::new();
    let mut remote = false;
    if let Some(path) = args.get(1) {
        if is_remote(path) {
            remote = true;
            files.push(path.clone());
        } else if Path::new(path).is_dir() {
            files = dir_files(Path::new(path))
                .iter()
                .map(|f| c.server_path(f))
                .collect();
        } else {
            files.push(c.server_path(path));
        }
    }
    let started = Instant::now();
    let r = c.request(
        "build",
        vec![
            ("db_name", Json::from(db.as_str())),
            ("remote", Json::from(remote)),
            (
                "db_path",
                Json::Arr(files.into_iter().map(Json::from).collect()),
            ),
        ],
    )?;
    let ms = started.elapsed().as_millis();
    if !r.ok() {
        bail!(c, "Build RDF database {db} failed: {}", r.msg);
    }
    let failed = r.num("failed_num");
    if failed != "0" {
        say!(c, "RDF parse error num {failed}");
        say!(c, "See log file for details ");
    }
    c.remember_db(&db);
    say!(c, "Build RDF database {db} successfully! use {ms} ms.");
    Ok(())
}

fn h_use(c: &mut Console, args: &[String]) -> CmdResult {
    c.argc(args, &[1], "use")?;
    let db = args[0].clone();
    if db == SYSTEM_DB {
        if c.is_root() {
            c.current_db = db;
            return Ok(());
        }
        bail!(c, "You can NOT use system database. ");
    }
    let r = c.request(
        "load",
        vec![("db_name", Json::from(db.as_str())), ("csr", Json::from("0"))],
    )?;
    if !r.ok() {
        c.privilege_hint(&r, &db);
        bail!(c, "Load database {db} failed: {}", r.msg);
    }
    c.current_db = db.clone();
    say!(c, "Current database switch to {db} successfully.");
    Ok(())
}

fn h_drop(c: &mut Console, args: &[String]) -> CmdResult {
    c.argc(args, &[1], "drop")?;
    let db = args[0].clone();
    if db == SYSTEM_DB {
        bail!(c, "You can NOT drop system database. ");
    }
    if db == c.current_db {
        bail!(
            c,
            "You can NOT drop current database. Please UNLOAD current database through \"UNLOAD <database_name>;\" before you drop it."
        );
    }
    let r = c.request(
        "drop",
        vec![
            ("db_name", Json::from(db.as_str())),
            ("is_backup", Json::from(false)),
        ],
    )?;
    if !r.ok() {
        c.privilege_hint(&r, &db);
        bail!(c, "Drop database {db} failed: {}", r.msg);
    }
    c.forget_db(&db);
    say!(c, "Drop database {db} successfully! ");
    Ok(())
}

fn h_show(c: &mut Console, args: &[String]) -> CmdResult {
    c.argc(args, &[0, 1], "show")?;
    let db = match args.first() {
        Some(db) => db.clone(),
        None => c.need_db()?,
    };
    let r = c.request("monitor", vec![("db_name", Json::from(db.as_str()))])?;
    if !r.ok() {
        c.privilege_hint(&r, &db);
        bail!(c, "Failed to monitor database: {}", r.msg);
    }
    let rows = vec![
        vec!["database".to_string(), r.text("database")],
        vec!["creator".to_string(), r.text("creator")],
        vec!["builtTime".to_string(), r.text("builtTime")],
        vec!["triple_num".to_string(), r.num("tripleNum")],
        vec!["entityNum".to_string(), r.num("entityNum")],
        vec!["literalNum".to_string(), r.num("literalNum")],
        vec!["subjectNum".to_string(), r.num("subjectNum")],
        vec!["predicateNum".to_string(), r.num("predicateNum")],
        vec!["size".to_string(), format!("{}M", r.num("diskUsed"))],
        vec!["builtCost".to_string(), format!("{}ms", r.num("costTime"))],
    ];
    let _ = write!(c.out, "{}", table::render(&["name", "value"], &rows));
    Ok(())
}

fn h_showdbs(c: &mut Console, args: &[String]) -> CmdResult {
    c.argc(args, &[0], "showdbs")?;
    let r = c.request("show", vec![])?;
    if !r.ok() {
        bail!(c, "show databases failed: {}", r.msg);
    }
    let rows: Vec<Vec<String>> = r
        .json
        .get("ResponseBody")
        .and_then(Json::as_array)
        .unwrap_or_default()
        .iter()
        .map(|d| {
            ["database", "creator", "built_time", "status"]
                .iter()
                .map(|k| d.get(k).map(text_of).unwrap_or_default())
                .collect()
        })
        .collect();
    let mut names: Vec<String> = rows.iter().map(|r| r[0].clone()).collect();
    names.sort();
    *c.db_names.borrow_mut() = names;
    let _ = write!(
        c.out,
        "{}",
        table::render(&["database", "creater", "builtTime", "status"], &rows)
    );
    Ok(())
}

fn h_backup(c: &mut Console, args: &[String]) -> CmdResult {
    c.argc(args, &[0, 1], "backup")?;
    let db = c.need_db()?;
    c.not_sysdb()?;
    let mut params = vec![("db_name", Json::from(db.as_str()))];
    if let Some(path) = args.first() {
        params.push(("backup_path", Json::from(path.as_str())));
    }
    let r = c.request("backup", params)?;
    if !r.ok() {
        c.privilege_hint(&r, &db);
        bail!(c, "Database {db} backup failed: {}", r.msg);
    }
    say!(c, "Database {db} backup successfully.");
    let path = r.text("backupfilepath");
    if !path.is_empty() {
        say!(c, "Backup path: {path}");
    }
    Ok(())
}

fn h_restore(c: &mut Console, args: &[String]) -> CmdResult {
    c.argc(args, &[2], "restore")?;
    let db = args[0].clone();
    if db == c.current_db {
        bail!(
            c,
            "Database {db} restore failed: Database alreay load, need unload it first through \"UNLOAD <database_name>;\" before you restore it"
        );
    }
    let mut path = args[1].clone();
    while path.len() > 1 && path.ends_with('/') {
        path.pop();
    }
    let path = c.server_path(&path);
    let r = c.request(
        "restore",
        vec![
            ("db_name", Json::from(db.as_str())),
            ("backup_path", Json::from(path)),
        ],
    )?;
    if !r.ok() {
        c.privilege_hint(&r, &db);
        bail!(c, "Database {db} restore failed: {}", r.msg);
    }
    c.remember_db(&db);
    say!(c, "Database {db} restored successfully.");
    Ok(())
}

fn h_export(c: &mut Console, args: &[String]) -> CmdResult {
    c.argc(args, &[0, 1], "export")?;
    let db = c.need_db()?;
    c.not_sysdb()?;
    let path = args.first().map_or("export/", String::as_str);
    let r = c.request(
        "export",
        vec![
            ("db_name", Json::from(db.as_str())),
            ("db_path", Json::from(path)),
        ],
    )?;
    if !r.ok() {
        c.privilege_hint(&r, &db);
        bail!(c, "Database {db} exported failed: {}", r.msg);
    }
    say!(c, "Database {db} exported successfully.");
    let file = r.text("filepath");
    if !file.is_empty() {
        say!(c, "Export file: {file}");
    }
    Ok(())
}

fn h_pdb(c: &mut Console, _args: &[String]) -> CmdResult {
    let db = c.need_db()?;
    say!(c, "{db}");
    Ok(())
}

fn h_unload(c: &mut Console, _args: &[String]) -> CmdResult {
    let db = c.need_db()?;
    c.not_sysdb()?;
    let r = c.request("unload", vec![("db_name", Json::from(db.as_str()))])?;
    if !r.ok() {
        c.privilege_hint(&r, &db);
        bail!(c, "Unload database {db} failed: {}", r.msg);
    }
    say!(c, "Unload database {db} successfully.");
    c.current_db.clear();
    Ok(())
}

fn batch(c: &mut Console, args: &[String], insert: bool) -> CmdResult {
    let db = c.need_db()?;
    c.not_sysdb()?;
    c.argc(args, &[1], if insert { "batchinsert" } else { "batchremove" })?;
    let file = &args[0];
    let remote = is_remote(file);
    let file = if remote {
        file.clone()
    } else {
        c.server_path(file)
    };
    let mut params = vec![
        ("db_name", Json::from(db.as_str())),
        ("file", Json::Arr(vec![Json::from(file)])),
    ];
    if remote {
        params.push(("remote", Json::from(true)));
    }
    let started = Instant::now();
    let op = if insert { "batchInsert" } else { "batchRemove" };
    let r = c.request(op, params)?;
    let ms = started.elapsed().as_millis();
    if !r.ok() {
        c.privilege_hint(&r, &db);
        if insert {
            bail!(c, "Insert data into {db} failed: {}", r.msg);
        }
        bail!(c, "Delete the {db} data failed: {}", r.msg);
    }
    let (ok, failed) = (r.num("success_num"), r.num("failed_num"));
    if insert {
        say!(c, "After inserted triples num {ok},failed num {failed},used {ms} ms");
    } else {
        say!(c, "After removed triples num {ok},failed num {failed},used {ms} ms");
    }
    Ok(())
}

fn h_batchinsert(c: &mut Console, args: &[String]) -> CmdResult {
    batch(c, args, true)
}

fn h_batchremove(c: &mut Console, args: &[String]) -> CmdResult {
    batch(c, args, false)
}

// ---- reasoning rules (upstream `reasonManage`, type codes 1-7) ----------

/// `reasonManage` of `kind` on the current database (upstream
/// `reason_manage_handler`).
fn reason(c: &mut Console, kind: &str, extra: Vec<(&str, Json)>) -> Result<Resp, Fail> {
    let db = c.current_db.clone();
    let mut params = vec![
        ("db_name", Json::from(db.as_str())),
        ("type", Json::from(kind)),
    ];
    params.extend(extra);
    c.request("reasonManage", params)
}

/// The checks every reasoning command starts with.
fn reason_prelude(c: &mut Console, args: &[String], argc: usize, name: &str) -> CmdResult {
    c.need_db()?;
    c.not_sysdb()?;
    c.argc(args, &[argc], name)
}

/// The JSON document in file `path`, or the reason it is not one (printed).
fn read_json_file(c: &mut Console, path: &str) -> Result<Json, Fail> {
    if !Path::new(path).is_file() {
        bail!(c, "File {path} does not exist.");
    }
    let Ok(text) = std::fs::read_to_string(path) else {
        bail!(c, "failed to open file: {path}");
    };
    match json::parse(&text) {
        Ok(j) => Ok(j),
        Err(e) => bail!(c, "failed to parse file {path}: {e}"),
    }
}

fn h_addreason(c: &mut Console, args: &[String]) -> CmdResult {
    reason_prelude(c, args, 1, "addreason")?;
    let rule = read_json_file(c, &args[0])?;
    let r = reason(c, "1", vec![("ruleinfo", rule)])?;
    say!(c, "{}", r.msg);
    if r.ok() {
        Ok(())
    } else {
        Err(Fail::Failed)
    }
}

fn h_listreason(c: &mut Console, args: &[String]) -> CmdResult {
    reason_prelude(c, args, 0, "listreason")?;
    let r = reason(c, "2", vec![])?;
    if !r.ok() {
        bail!(c, "list reasons failed: {}", r.msg);
    }
    let rows: Vec<Vec<String>> = r
        .json
        .get("list")
        .and_then(Json::as_array)
        .unwrap_or_default()
        .iter()
        .enumerate()
        .map(|(i, rule)| {
            let f = |k: &str| rule.get(k).map(text_of).unwrap_or_default();
            vec![(i + 1).to_string(), f("rulename"), f("description")]
        })
        .collect();
    let _ = write!(
        c.out,
        "{}",
        table::render(&["ruleid", "rulename", "description"], &rows)
    );
    say!(c, "list reasons successfully!");
    Ok(())
}

/// `compilereason` / `executereason` / `disablereason`: the rule's
/// statements as a table.
fn reason_rule_op(
    c: &mut Console,
    args: &[String],
    name: &str,
    kind: &str,
    verb: &str,
    fields: &[&str],
) -> CmdResult {
    reason_prelude(c, args, 1, name)?;
    let rule = args[0].clone();
    let r = reason(c, kind, vec![("rulename", Json::from(rule.as_str()))])?;
    if !r.ok() {
        bail!(c, "{verb} reason {rule} failed: {}", r.msg);
    }
    let row: Vec<String> = fields.iter().map(|f| r.text(f)).collect();
    let _ = write!(c.out, "{}", table::render(fields, &[row]));
    say!(c, "{verb} reason {rule} successfully!");
    Ok(())
}

fn h_compilereason(c: &mut Console, args: &[String]) -> CmdResult {
    reason_rule_op(c, args, "compilereason", "3", "compile", &["insert_sparql", "delete_sparql"])
}

fn h_executereason(c: &mut Console, args: &[String]) -> CmdResult {
    reason_rule_op(c, args, "executereason", "4", "execute", &["insert_sparql"])
}

fn h_disablereason(c: &mut Console, args: &[String]) -> CmdResult {
    reason_rule_op(c, args, "disablereason", "5", "disable", &["delete_sparql"])
}

/// A JSON value that may hold its JSON as a string (upstream nests rule
/// parts as JSON text).
fn json_value(v: &Json) -> Json {
    match v {
        Json::Str(s) => json::parse(s).unwrap_or_else(|_| v.clone()),
        other => other.clone(),
    }
}

fn h_showreason(c: &mut Console, args: &[String]) -> CmdResult {
    reason_prelude(c, args, 1, "showreason")?;
    let rule = args[0].clone();
    let r = reason(c, "6", vec![("rulename", Json::from(rule.as_str()))])?;
    if !r.ok() {
        bail!(c, "show reason {rule} failed: {}", r.msg);
    }
    let info = r.json.get("ruleinfo").map(json_value).unwrap_or_else(Json::obj);
    let field = |k: &str| info.get(k).map(text_of).unwrap_or_default();
    let (name, desc) = (field("rulename"), field("description"));
    let conditions: Vec<Json> = info
        .get("condition")
        .or_else(|| info.get("conditions"))
        .and_then(Json::as_array)
        .unwrap_or_default()
        .iter()
        .map(json_value)
        .collect();
    say!(c, "{}", conditions.len());
    let mut rows = Vec::new();
    for cond in &conditions {
        let filters: String = cond
            .get("filters")
            .and_then(Json::as_array)
            .unwrap_or_default()
            .iter()
            .map(|f| format!("{};", text_of(f)))
            .collect();
        for p in cond.get("patterns").and_then(Json::as_array).unwrap_or_default() {
            let p = json_value(p);
            let part = |k: &str| p.get(k).map(text_of).unwrap_or_default();
            let pattern = format!("<{},{},{}>", part("subject"), part("predicate"), part("object"));
            rows.push(vec![name.clone(), desc.clone(), pattern, filters.clone()]);
        }
    }
    let _ = write!(
        c.out,
        "{}",
        table::render(&["rulename", "description", "patterns", "filters"], &rows)
    );
    say!(c, "show reason {rule} successfully!");
    Ok(())
}

fn h_deletereason(c: &mut Console, args: &[String]) -> CmdResult {
    reason_prelude(c, args, 1, "deletereason")?;
    let rule = args[0].clone();
    let r = reason(c, "7", vec![("rulename", Json::from(rule.as_str()))])?;
    if !r.ok() {
        bail!(c, "delete reason {rule} failed: {}", r.msg);
    }
    say!(c, "delete reason {rule} successfully!");
    Ok(())
}

// ---- users and privileges -----------------------------------------------

fn h_flushpriv(c: &mut Console, args: &[String]) -> CmdResult {
    c.argc(args, &[0], "flushpriv")?;
    // The server checks privileges on every request: nothing is cached here.
    say!(c, "Privilige Flushed for current user successfully.");
    Ok(())
}

/// The users of `showuser` (`ResponseBody`).
fn show_users(c: &mut Console) -> Result<Vec<Json>, Fail> {
    let r = c.request("showuser", vec![])?;
    if !r.ok() {
        bail!(c, "Users Query failed: {}", r.msg);
    }
    Ok(r
        .json
        .get("ResponseBody")
        .and_then(Json::as_array)
        .unwrap_or_default()
        .to_vec())
}

/// A `showuser` entry's privileges per database (upstream's `parse_priv`):
/// database → `"query,load,…"` (`"all"` with all seven); `None` for a user
/// with every privilege everywhere (`"all"` lists).
fn privileges_by_db(user: &Json) -> Option<BTreeMap<String, String>> {
    let mut by_db: BTreeMap<String, Vec<&str>> = BTreeMap::new();
    for (field, name) in LISTING_ORDER {
        let list = user.get(field).map(text_of).unwrap_or_default();
        if list.trim() == "all" {
            return None;
        }
        for db in list.split(',').map(str::trim).filter(|d| !d.is_empty()) {
            by_db.entry(db.to_string()).or_default().push(name);
        }
    }
    Some(
        by_db
            .into_iter()
            .map(|(db, names)| {
                let list = if names.len() == LISTING_ORDER.len() {
                    "all".to_string()
                } else {
                    names.join(",")
                };
                (db, list)
            })
            .collect(),
    )
}

fn h_pusr(c: &mut Console, args: &[String]) -> CmdResult {
    c.argc(args, &[0, 1, 2], "pusr")?;
    let me = c.user().to_string();
    let (target, db) = match args {
        [] => {
            say!(c, "usrname: {me}");
            return Ok(());
        }
        [db] => (me.clone(), db.clone()),
        [db, user, ..] => {
            if !c.is_root() {
                say!(
                    c,
                    "Permission denied. Only root is allowed to view other's privilege."
                );
                if *user == me {
                    say!(
                        c,
                        "If you want to view your privilege, just enter 'pusr;' or 'pusr <database_name>;'."
                    );
                }
                return Err(Fail::Failed);
            }
            (user.clone(), db.clone())
        }
    };
    let headers = ["user", "database", "privilege"];
    if target == ROOT_USER {
        let row = vec![ROOT_USER.to_string(), db, "all".to_string()];
        let _ = write!(c.out, "{}", table::render(&headers, &[row]));
        return Ok(());
    }
    let users = show_users(c)?;
    let Some(entry) = users
        .iter()
        .find(|u| u.get("username").map(text_of).as_deref() == Some(target.as_str()))
    else {
        bail!(c, "User {target} does not exist.");
    };
    let privs = match privileges_by_db(entry) {
        None => "all".to_string(),
        Some(by_db) => by_db
            .get(&db)
            .or_else(|| by_db.get("*"))
            .cloned()
            .unwrap_or_else(|| "no priv".to_string()),
    };
    let _ = write!(c.out, "{}", table::render(&headers, &[vec![target, db, privs]]));
    Ok(())
}

fn h_showusrs(c: &mut Console, args: &[String]) -> CmdResult {
    c.argc(args, &[0], "showusrs")?;
    if !c.is_root() {
        bail!(c, "Permission denied. Only root is allowed to view all usrs.");
    }
    let users = show_users(c)?;
    let mut rows = vec![vec![
        ROOT_USER.to_string(),
        "all".to_string(),
        "all".to_string(),
    ]];
    for u in &users {
        let name = u.get("username").map(text_of).unwrap_or_default();
        if name == ROOT_USER {
            continue;
        }
        match privileges_by_db(u) {
            None => rows.push(vec![name, "all".to_string(), "all".to_string()]),
            Some(by_db) if by_db.is_empty() => {
                rows.push(vec![name, "all".to_string(), "no priv".to_string()])
            }
            Some(by_db) => {
                for (db, list) in by_db {
                    rows.push(vec![name.clone(), db, list]);
                }
            }
        }
    }
    let _ = write!(
        c.out,
        "{}",
        table::render(&["user", "database", "privilege"], &rows)
    );
    Ok(())
}

fn h_setpswd(c: &mut Console, args: &[String]) -> CmdResult {
    c.argc(args, &[0, 1], "setpswd")?;
    let me = c.user().to_string();
    let (prompt, target) = match args.first() {
        Some(user) => {
            if !c.is_root() {
                say!(
                    c,
                    "Permission denied. Only root is allowed to set other's pswd."
                );
                if *user == me {
                    say!(c, "If you want to set your pswd, just enter 'setpswd;'.");
                }
                return Err(Fail::Failed);
            }
            ("Enter your password: ", user.clone())
        }
        None => ("Enter old password: ", me.clone()),
    };
    let Some(old) = c.prompt_line(prompt, true) else {
        return Err(Fail::Failed);
    };
    if old != c.client.password {
        bail!(c, "Fail to varify your id. Password set failed.");
    }
    let mut new = None;
    for attempt in 0..MAX_WRONG_PSWD_TIMES {
        if attempt > 0 {
            say!(c, "Not Matched.");
        }
        let Some(first) = c.prompt_line("Enter new password: ", true) else {
            return Err(Fail::Failed);
        };
        let Some(again) = c.prompt_line("Enter new password again: ", true) else {
            return Err(Fail::Failed);
        };
        if first == again {
            new = Some(first);
            break;
        }
    }
    let Some(new) = new else {
        bail!(c, "Too much not match. Password set failed.");
    };
    let r = if args.is_empty() {
        c.request("userpassword", vec![("op_password", Json::from(new.as_str()))])?
    } else {
        c.request(
            "usermanage",
            vec![
                ("type", Json::from("3")),
                ("op_username", Json::from(target.as_str())),
                ("op_password", Json::from(new.as_str())),
            ],
        )?
    };
    if !r.ok() {
        bail!(
            c,
            "System db update failed : {}. Password set failed.",
            r.msg
        );
    }
    if target == me {
        c.client.password = new;
    }
    say!(c, "Password set successfully.");
    Ok(())
}

/// Upstream `print_lowbits(priv, 8)`.
fn low_bits(bits: u32) -> String {
    (0..8)
        .rev()
        .map(|i| if bits & (1 << i) != 0 { '1' } else { '0' })
        .collect()
}

/// The privilege numbers in `bits` as upstream's comma list (`"1,2,3"`).
fn privilege_list(bits: u32) -> String {
    (1..=7)
        .filter(|i| bits & (1 << i) != 0)
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

/// Parse upstream's privilege numbers (`1 2 3`, `8` = all) into bits.
pub fn parse_privilege_numbers(line: &str) -> u32 {
    let mut bits = 0u32;
    for word in line.split_whitespace() {
        match word.parse::<u32>() {
            Ok(n @ 1..=7) => bits |= 1 << n,
            Ok(8) => bits |= 0xFE,
            _ => {}
        }
    }
    bits
}

fn h_setpriv(c: &mut Console, args: &[String]) -> CmdResult {
    c.argc(args, &[2], "setpriv")?;
    if !c.is_root() {
        bail!(
            c,
            "Permission denied. Only root is allowed to set other's privilege."
        );
    }
    let (user, db) = (args[0].clone(), args[1].clone());
    if user == ROOT_USER {
        say!(
            c,
            "Root has all privilege on all databases. No need to set root's privilege."
        );
        bail!(c, "Privilege set failed.");
    }
    c.verify_password("Privilege set failed.")?;
    let menu: String = (1..PRIVILEGE_NAMES.len())
        .map(|i| format!("[{i}]{} ", PRIVILEGE_NAMES[i]))
        .collect();
    let prompt = format!(
        "{menu}[{}]all\nEnter privilege number to assign separated by whitespace: \n",
        PRIVILEGE_NAMES.len()
    );
    let Some(line) = c.prompt_line(&prompt, false) else {
        return Err(Fail::Failed);
    };
    let bits = parse_privilege_numbers(&line);
    say!(c, "[will set priv:]{}", low_bits(bits));
    // The user's current privileges on the database (upstream `read_priv`).
    let users = show_users(c)?;
    let mut current = 0u32;
    if let Some(u) = users
        .iter()
        .find(|u| u.get("username").map(text_of).as_deref() == Some(user.as_str()))
    {
        for (n, field) in PRIVILEGE_FIELDS {
            let list = u.get(field).map(text_of).unwrap_or_default();
            if list.split(',').any(|d| d.trim() == db) {
                current |= 1 << n;
            }
        }
    }
    for (kind, set) in [("2", current), ("1", bits)] {
        if set == 0 {
            continue;
        }
        let r = c.request(
            "userprivilegemanage",
            vec![
                ("type", Json::from(kind)),
                ("op_username", Json::from(user.as_str())),
                ("privileges", Json::from(privilege_list(set))),
                ("db_name", Json::from(db.as_str())),
            ],
        )?;
        if !r.ok() {
            bail!(c, "Privilege set failed {}", r.msg);
        }
    }
    say!(c, "Privilege set successfully.");
    Ok(())
}

fn h_clearpriv(c: &mut Console, args: &[String]) -> CmdResult {
    c.argc(args, &[1], "clearpriv")?;
    if !c.is_root() {
        bail!(
            c,
            "Permission denied. Only root is allowed to set other's privilege."
        );
    }
    let user = args[0].clone();
    if user == ROOT_USER {
        bail!(c, "Forbidden Behavior: clear root priv.");
    }
    let r = c.request(
        "userprivilegemanage",
        vec![
            ("type", Json::from("3")),
            ("op_username", Json::from(user.as_str())),
            ("privileges", Json::from("")),
            ("db_name", Json::from("")),
        ],
    )?;
    if !r.ok() {
        bail!(c, "Privilege clear failed {}", r.msg);
    }
    say!(c, "clear {user} for all databases privilege successfully!");
    Ok(())
}

/// `addusr` / `delusr` (upstream `adddelusr_handler`).
fn add_del_user(c: &mut Console, add: bool, user: &str) -> CmdResult {
    if !c.is_root() {
        bail!(c, "Permission denied. Only root is allowed to manage user.");
    }
    c.verify_password("User manage failed.")?;
    let password = if add {
        match c.prompt_line("set password for new usr: ", true) {
            Some(p) => p,
            None => return Err(Fail::Failed),
        }
    } else {
        String::new()
    };
    let r = c.request(
        "usermanage",
        vec![
            ("type", Json::from(if add { "1" } else { "2" })),
            ("op_username", Json::from(user)),
            ("op_password", Json::from(password)),
        ],
    )?;
    if !r.ok() {
        bail!(c, "{}", r.msg);
    }
    Ok(())
}

fn h_addusr(c: &mut Console, args: &[String]) -> CmdResult {
    c.argc(args, &[1], "addusr")?;
    let user = args[0].clone();
    match add_del_user(c, true, &user) {
        Ok(()) => {
            say!(c, "Add usr {user} successfully.");
            Ok(())
        }
        Err(Fail::Cancelled) => Err(Fail::Cancelled),
        Err(f) => {
            say!(c, "Add usr {user} failed");
            Err(f)
        }
    }
}

fn h_delusr(c: &mut Console, args: &[String]) -> CmdResult {
    c.argc(args, &[1], "delusr")?;
    let user = args[0].clone();
    match add_del_user(c, false, &user) {
        Ok(()) => {
            say!(c, "Del usr {user} successfully.");
            Ok(())
        }
        Err(Fail::Cancelled) => Err(Fail::Cancelled),
        Err(f) => {
            say!(c, "Del usr {user} failed.");
            Err(f)
        }
    }
}

// ---- custom functions (upstream `funquery` / `funcudb`) ---------------

/// Upstream's `PFNInfo` as JSON: its six string fields, from `src`.
fn fun_info(src: &Json) -> Json {
    let mut info = Json::obj();
    for k in ["funName", "funDesc", "funBody", "funSubs", "funStatus", "lastTime"] {
        info.set(k, src.get(k).map(text_of).unwrap_or_default());
    }
    info
}

fn h_showpfn(c: &mut Console, args: &[String]) -> CmdResult {
    c.argc(args, &[0], "showpfn")?;
    let r = c.request("funquery", vec![("funInfo", fun_info(&Json::obj()))])?;
    if !r.ok() {
        bail!(c, "failed to query custom function: {}", r.msg);
    }
    let rows: Vec<Vec<String>> = r
        .json
        .get("list")
        .and_then(Json::as_array)
        .unwrap_or_default()
        .iter()
        .map(|f| {
            let g = |k: &str| f.get(k).map(text_of).unwrap_or_default();
            vec![
                g("funName"),
                g("funDesc"),
                format!("./pfn/{}.cpp", g("funName")),
                g("funStatus"),
                g("lastTime"),
            ]
        })
        .collect();
    let _ = write!(
        c.out,
        "{}",
        table::render(
            &["name", "desc", "returnType", "status", "lastBuildTime"],
            &rows
        )
    );
    say!(c, "query custom function successfully!");
    Ok(())
}

/// `funcudb` of `kind` (1 create, 2 update from a JSON file; 3 delete, 4
/// build by name); `what` names it in the messages.
fn fun_cudb(c: &mut Console, args: &[String], name: &str, kind: &str) -> CmdResult {
    c.argc(args, &[1], name)?;
    let info = if kind == "1" || kind == "2" {
        fun_info(&read_json_file(c, &args[0])?)
    } else {
        fun_info(&Json::obj().with("funName", args[0].as_str()))
    };
    let (ok, failed) = match kind {
        "1" => ("create custom function successfully!", "failed to create custom function"),
        "2" => ("update custom function successfully!", "failed to update custom function"),
        "3" => ("delete custom function successfully!", "failed to delete custom function"),
        _ => ("custom function build successfully!", "failed to build custom function"),
    };
    let r = c.request(
        "funcudb",
        vec![("type", Json::from(kind)), ("funInfo", info)],
    )?;
    if !r.ok() {
        say!(c, "function operation failed: {}", r.msg);
        bail!(c, "{failed}");
    }
    say!(c, "{ok}");
    Ok(())
}

fn h_addpfn(c: &mut Console, args: &[String]) -> CmdResult {
    fun_cudb(c, args, "addpfn", "1")
}

fn h_mdfpfn(c: &mut Console, args: &[String]) -> CmdResult {
    fun_cudb(c, args, "mdfpfn", "2")
}

fn h_delpfn(c: &mut Console, args: &[String]) -> CmdResult {
    fun_cudb(c, args, "delpfn", "3")
}

fn h_buildpfn(c: &mut Console, args: &[String]) -> CmdResult {
    fun_cudb(c, args, "buildpfn", "4")
}

// ---- help and miscellany ------------------------------------------------

fn help_usage(c: &mut Console) {
    say!(c, "List of all gconsole commands:");
    say!(
        c,
        "Note that all text commands must be end with ';' but need not be in one line."
    );
    let rows: Vec<Vec<String>> = COMMANDS
        .iter()
        .map(|cmd| vec![cmd.name.to_string(), cmd.doc.to_string(), cmd.usage.to_string()])
        .collect();
    let _ = write!(
        c.out,
        "{}",
        table::render(&["name", "description", "usage"], &rows)
    );
}

fn h_help(c: &mut Console, args: &[String]) -> CmdResult {
    c.argc(args, &[0, 1], "help")?;
    let Some(name) = args.first() else {
        say!(
            c,
            "{PRODUCT_NAME} Console , an interactive shell based utility to communicate with {PRODUCT_NAME} repositories."
        );
        say!(c);
        say!(c, "For information about {PRODUCT_NAME} products and services, visit:");
        say!(c, "   {PRODUCT_WEBSITE}");
        say!(
            c,
            "For developer information, including the {PRODUCT_NAME} Reference Manual, visit:"
        );
        say!(c, "   {PRODUCT_WEBSITE}/pcsite/index.html#/documentation");
        say!(c);
        say!(c, "Commands end with ;. Cross line input is allowed.");
        say!(c, "Comment start with #.");
        say!(c, "CTRL+C to quit current command. CTRL+D to exit this console.");
        say!(c, "List of all console commands:");
        let mut rows: Vec<Vec<String>> = COMMANDS
            .iter()
            .map(|cmd| vec![cmd.name.to_string(), cmd.doc.to_string()])
            .collect();
        rows.push(vec!["Other help arg:".to_string(), String::new()]);
        rows.push(vec![
            "edit".to_string(),
            "Display line editing shortcut keys supported by console.".to_string(),
        ]);
        rows.push(vec![
            "usage".to_string(),
            "Display all commands as well as their usage.".to_string(),
        ]);
        let _ = write!(c.out, "{}", table::render(&["name", "description"], &rows));
        return Ok(());
    };
    match name.as_str() {
        "usage" => help_usage(c),
        "edit" => {
            say!(c, "Frequently used GNU Readline shortcuts:");
            let rows: Vec<Vec<String>> = [
                ("CTRL-a", "move cursor to the beginning of line"),
                ("CTRL-e", "move cursor to the end of line"),
                ("CTRL-d", "delete a character"),
                ("CTRL-f", "move cursor forward (right arrow)"),
                ("CTRL-b", "move cursor backward (left arrow)"),
                ("CTRL-p", "previous line, previous command in history (up arrow)"),
                ("CTRL-n", "next line, next command in history (down arrow)"),
                ("CTRL-k", "kill the line after the cursor, add to clipboard"),
                ("CTRL-u", "kill the line before the cursor, add to clipboard"),
                ("CTRL-y", "paste from the clipboard"),
                ("ALT-b", "move cursor back one word"),
                ("ALT-f", "move cursor forward one word"),
            ]
            .iter()
            .map(|(k, d)| vec![k.to_string(), d.to_string()])
            .collect();
            let _ = write!(c.out, "{}", table::render(&["name", "description"], &rows));
            say!(
                c,
                "For more about GNU Readline shortcuts, see https://en.wikipedia.org/wiki/GNU_Readline#Emacs_keyboard_shortcuts"
            );
            if !cfg!(feature = "readline") {
                say!(
                    c,
                    "(This gconsole was built without the `readline` feature: it reads plain lines; see 'history;'.)"
                );
            }
        }
        other => match find_command(other) {
            Some(cmd) => say!(c, "{}\t{}\n\t\t{}", cmd.name, cmd.doc, cmd.usage),
            None if syntax::is_sparql(other) || other.eq_ignore_ascii_case("raw_sparql") => {
                let cmd = COMMANDS.last().expect("raw_sparql entry");
                say!(c, "{}\t{}\n\t\t{}", cmd.name, cmd.doc, cmd.usage);
            }
            None => {
                say!(
                    c,
                    "{other}: Not a supported console command. \nList of gconsole command:"
                );
                let names: String = COMMANDS.iter().map(|cmd| format!("{} ", cmd.name)).collect();
                say!(c, "{names}");
            }
        },
    }
    Ok(())
}

fn h_usage(c: &mut Console, args: &[String]) -> CmdResult {
    c.argc(args, &[0], "usage")?;
    help_usage(c);
    Ok(())
}

fn h_version(c: &mut Console, _args: &[String]) -> CmdResult {
    let _ = write!(c.out, "{}", version_text());
    Ok(())
}

fn h_pwd(c: &mut Console, _args: &[String]) -> CmdResult {
    match std::env::current_dir() {
        Ok(d) => {
            say!(c, "{}", d.display());
            Ok(())
        }
        Err(e) => bail!(c, "pwd failed: {e}"),
    }
}

fn h_clear(c: &mut Console, _args: &[String]) -> CmdResult {
    let _ = write!(c.out, "\x1b[H\x1b[2J\x1b[3J");
    let _ = c.out.flush();
    Ok(())
}

fn h_history(c: &mut Console, args: &[String]) -> CmdResult {
    c.argc(args, &[0, 1], "history")?;
    let entries = c.history.entries().to_vec();
    let n = match args.first() {
        Some(a) => match a.parse::<usize>() {
            Ok(n) => n,
            Err(_) => bail!(c, "Illegal Argument: {a} must be number"),
        },
        None => entries.len(),
    };
    let skip = entries.len().saturating_sub(n);
    for (i, e) in entries.iter().enumerate().skip(skip) {
        say!(c, "{:5}  {e}", i + 1);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_command_table_matches_upstream() {
        // Upstream's 43 entries (its `TOTAL_COMMAND_NUM`) plus `exit` and
        // `history`; `raw_sparql` is last.
        assert_eq!(COMMANDS.len(), 45);
        assert_eq!(COMMANDS.last().unwrap().name, "raw_sparql");
        assert!(find_command("raw_sparql").is_none());
        for name in [
            "quit", "refreshconf", "init", "sparql", "create", "use", "drop", "show", "showdbs",
            "backup", "restore", "export", "pdb", "unload", "batchinsert", "batchremove",
            "addreason", "listreason", "compilereason", "executereason", "disablereason",
            "showreason", "deletereason", "flushpriv", "pusr", "setpswd", "setpriv", "clearpriv",
            "addusr", "delusr", "showusrs", "showpfn", "addpfn", "mdfpfn", "delpfn", "buildpfn",
            "help", "?", "usage", "version", "pwd", "clear",
        ] {
            assert!(find_command(name).is_some(), "{name}");
        }
        assert_eq!(find_command("SHOWDBS").unwrap().name, "showdbs");
        assert!(find_command("select").is_none());
    }

    #[test]
    fn privilege_numbers_and_bits() {
        assert_eq!(parse_privilege_numbers("1 2 3"), 0b1110);
        assert_eq!(low_bits(parse_privilege_numbers("1 2 3")), "00001110");
        assert_eq!(parse_privilege_numbers("8"), 0xFE);
        assert_eq!(parse_privilege_numbers("9 x 0 7"), 1 << 7);
        assert_eq!(privilege_list(0xFE), "1,2,3,4,5,6,7");
        assert_eq!(privilege_list(0b1010), "1,3");
    }

    #[test]
    fn showuser_entries_become_per_database_lists() {
        let u = json::parse(
            r#"{"username":"bob","query_privilege":"lubm,small,","load_privilege":"lubm,",
                "unload_privilege":"","update_privilege":"lubm,","backup_privilege":"lubm,",
                "restore_privilege":"lubm,","export_privilege":"lubm,"}"#,
        )
        .unwrap();
        let by_db = privileges_by_db(&u).unwrap();
        assert_eq!(by_db["lubm"], "query,load,update,backup,restore,export");
        assert_eq!(by_db["small"], "query");
        let all = json::parse(r#"{"username":"a","query_privilege":"all"}"#).unwrap();
        assert!(privileges_by_db(&all).is_none());
    }

    #[test]
    fn answers_become_tables() {
        let j = json::parse(
            r#"{"head":{"link":[],"vars":["x","y"]},"results":{"bindings":[
                {"x":{"type":"uri","value":"a"},"y":{"type":"literal","value":"1"}},
                {"x":{"type":"uri","value":"b"}}]},"AnsNum":2}"#,
        )
        .unwrap();
        let (h, rows) = result_table(&j).unwrap();
        assert_eq!(h, vec!["?x", "?y"]);
        assert_eq!(
            rows,
            vec![
                vec!["<a>".to_string(), "\"1\"".to_string()],
                vec!["<b>".to_string(), String::new()]
            ]
        );
        let nt = json::parse(r#"{"head":["?s"],"results":[["<a>"]]}"#).unwrap();
        assert_eq!(result_table(&nt).unwrap().1, vec![vec!["<a>".to_string()]]);
        let upd = json::parse(r#"{"StatusCode":0,"AnsNum":1}"#).unwrap();
        assert!(result_table(&upd).is_none());
    }

    #[test]
    fn history_is_kept_and_capped() {
        let dir = std::env::temp_dir().join(format!("gconsole_hist_{}", std::process::id()));
        let _ = std::fs::remove_file(&dir);
        let mut h = History::open(Some(dir.clone()));
        h.add("use lubm;");
        h.add("use lubm;");
        h.add("  ");
        h.add("pdb;");
        assert_eq!(h.entries(), ["use lubm;", "pdb;"]);
        let lines: Vec<String> = (0..History::MAX + 10).map(|i| format!("s{i};")).collect();
        std::fs::write(&dir, lines.join("\n")).unwrap();
        let h = History::open(Some(dir.clone()));
        assert_eq!(h.entries().len(), History::MAX);
        assert_eq!(h.entries()[0], "s10;");
        assert_eq!(
            std::fs::read_to_string(&dir).unwrap().lines().count(),
            History::MAX
        );
        let _ = std::fs::remove_file(&dir);
    }
}
