//! `gconsole` — gStore's interactive console, as in upstream gStore 1.4
//! (`src/Main/gconsole.cpp`): an HTTP client of a running `gserver`'s `/api`
//! (by default `http://127.0.0.1:<port>`, the port read from
//! `conf/conf.ini`). The binary `src/bin/gconsole.rs` only calls [`main`].
//!
//! ```text
//! gconsole [-u <user>] [-p <password>] [--host H] [--port N] [-c conf.ini]
//!          [-e "<commands>"] [-i]
//! gconsole --local <db>          # the embedded REPL (gstore-rust extension)
//! ```
//!
//! * **Login** (upstream): the user name is asked for unless `-u` gives it,
//!   the password (echo off) unless `-p` or the `GSTORE_PASSWORD`
//!   environment variable gives it; a person typing gets seven tries.
//! * **Statements** end with `;` and may span lines (`     -> ` prompts the
//!   continuation); `#` starts a comment; `<statement> > file` / `>> file`
//!   redirect its output. The prompt is `gStore[no database]> ` or
//!   `gStore[<db>]> `. See [`syntax`] for the details.
//! * **Commands**: upstream's table ([`commands::COMMANDS`]: `create`, `use`,
//!   `show`, `showdbs`, `drop`, `backup`, `restore`, `export`,
//!   `batchinsert`, user and privilege management, reasoning rules, custom
//!   functions, `help`, …), or SPARQL typed directly (it starts with
//!   `SELECT`, `ASK`, `CONSTRUCT`, `DESCRIBE`, `INSERT`, `DELETE`, `PREFIX`,
//!   `BASE`, `LOAD`, `CLEAR`, `WITH`, …), run on the current database.
//! * **Ctrl-C** abandons the statement being typed, or the request being
//!   served — for a SPARQL statement the server's task is cancelled too
//!   (`operationTaskList` + `operationTaskCancel`) —, without leaving the
//!   console; **Ctrl-D** (or `quit;` / `exit;`) leaves it.
//! * **Scripts**: with `-e "<commands>"`, or with commands on a stdin that is
//!   not a terminal, the console runs them without banner or prompts and
//!   exits `1` if any of them failed (a final statement may omit its `;`).
//!   A command that asks for input (`addusr`'s passwords, `setpriv`'s
//!   privilege numbers, …) reads the next lines of stdin.
//! * **History**: statements typed at a terminal are kept in
//!   `~/.gstore_history` (or `$GSTORE_HISTFILE`; at most 1024) and listed
//!   by `history;`. With the `readline` cargo feature the console also has
//!   line editing (arrow keys, Emacs keys), history recall and tab
//!   completion of command and database names.

pub mod client;
pub mod commands;
pub mod local;
pub mod syntax;
pub mod table;
pub mod term;

use std::io::{self, Write};
use std::path::PathBuf;

use client::{ApiClient, CallError};
use commands::{
    Console, History, CROSS_LINE_PROMPT, MAX_WRONG_PSWD_TIMES, PRODUCT_NAME, PRODUCT_VERSION,
};
use syntax::Next;
use term::ReadOutcome;

/// The default host (upstream `BASE_URL`).
pub const DEFAULT_HOST: &str = "127.0.0.1";
/// The configuration file read for the port (upstream `INIT_CONF_FILE`).
pub const DEFAULT_CONF: &str = "conf/conf.ini";

/// The command line.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Options {
    pub user: Option<String>,
    pub password: Option<String>,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub config: Option<PathBuf>,
    pub exec: Option<String>,
    pub interactive: bool,
    /// `--local <db>` (or the legacy `gconsole <db>`).
    pub local: Option<String>,
    /// The database was given the legacy way (a bare argument).
    pub legacy_local: bool,
}

/// What the command line asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Invocation {
    Run(Options),
    Help,
    Version,
}

/// Parse the arguments (without the program name).
pub fn parse_args(args: &[String]) -> Result<Invocation, String> {
    let mut o = Options::default();
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        // `--opt=value` too.
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f, Some(v.to_string())),
            _ => (arg, None),
        };
        let mut value = |name: &str| -> Result<String, String> {
            if let Some(v) = inline.clone() {
                return Ok(v);
            }
            i += 1;
            args.get(i)
                .cloned()
                .ok_or_else(|| format!("option '{name}' needs a value"))
        };
        match flag {
            "-?" | "-h" | "--help" => return Ok(Invocation::Help),
            "-v" | "--version" => return Ok(Invocation::Version),
            "-u" | "--user" => o.user = Some(value(flag)?),
            "-p" | "--password" => o.password = Some(value(flag)?),
            "--host" | "-H" => o.host = Some(value(flag)?),
            "--port" | "-P" => {
                let v = value(flag)?;
                o.port = Some(
                    v.parse()
                        .map_err(|_| format!("invalid port '{v}'"))?,
                );
            }
            "-c" | "--config" => o.config = Some(PathBuf::from(value(flag)?)),
            "-e" | "--exec" => o.exec = Some(value(flag)?),
            "-i" | "--interactive" => o.interactive = true,
            "--local" => o.local = Some(value(flag)?),
            _ if !arg.starts_with('-') && o.local.is_none() => {
                o.local = Some(arg.to_string());
                o.legacy_local = true;
            }
            _ => return Err(format!("unknown option '{arg}'")),
        }
        i += 1;
    }
    Ok(Invocation::Run(o))
}

/// Upstream's `--help` text (`PRINT_ENTER_HELP_MSG`), with this console's
/// options.
pub fn help_text() -> String {
    let os = match std::env::consts::OS {
        "linux" => "Linux",
        "macos" => "macOS",
        other => other,
    };
    format!(
        "{PRODUCT_NAME} Ver {PRODUCT_VERSION} for {os} on {arch} (Source distribution)\n\
         {PRODUCT_NAME} Console(gconsole), an interactive shell based utility to communicate with {PRODUCT_NAME} repositories.\n\
         Copyright (c) 2016-{year}, pkumod and topgraph and/or its affiliates.\n\
         \n\
         Usage: bin/gconsole [OPTIONS]\n\
         \x20 -?, --help          Display this help and exit.\n\
         \x20 -v, --version       Display version information and exit.\n\
         \x20 -u, --user          username. \n\
         \x20 -p, --password      password (else asked for, or $GSTORE_PASSWORD).\n\
         \x20 --host HOST         gserver's address (default {DEFAULT_HOST}).\n\
         \x20 --port PORT         gserver's port (default: conf.ini's port, 9000).\n\
         \x20 -c, --config FILE   the conf.ini to read the port from (default ./{DEFAULT_CONF}).\n\
         \x20 -e, --exec CMDS     run the ;-separated commands, then exit.\n\
         \x20 -i, --interactive   banner, prompts and history even when stdin is not a terminal.\n\
         \x20 --local DB          gstore-rust extension: the embedded console on database DB\n\
         \x20                     (a directory or <name>.db), without a server.\n\
         \x20 \n\
         Supported command in gconsole: Type \"?\" or \"help\" in the console to see info of all commands.\n\
         \x20 \n",
        arch = std::env::consts::ARCH,
        year = commands::current_year(),
    )
}

/// The welcome text after a successful login (upstream `main`).
pub fn banner() -> String {
    format!(
        "\n{PRODUCT_NAME} Console , an interactive shell based utility to communicate with {PRODUCT_NAME} repositories.\n\
         {version}\n\
         Welcome to the {PRODUCT_NAME} Console.\n\
         Commands end with ;. Cross line input is allowed.\n\
         Comment start with #. Redirect (> and >>) is supported.\n\
         CTRL+C to quit current command. CTRL+D to exit this console.\n\
         Type 'help;' for help. \n",
        version = commands::version_text(),
    )
}

/// The port to use: `--port`, else the configuration's `port` (`-c`, else
/// `conf/conf.ini` when present), else 9000.
pub fn resolve_port(o: &Options) -> Result<u16, String> {
    if let Some(p) = o.port {
        return Ok(p);
    }
    let path = o
        .config
        .clone()
        .unwrap_or_else(|| PathBuf::from(DEFAULT_CONF));
    if o.config.is_none() && !path.is_file() {
        return Ok(crate::config::GStoreConfig::default().port);
    }
    crate::config::GStoreConfig::load(&path)
        .map(|c| c.port)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))
}

/// The history file: `$GSTORE_HISTFILE`, else `~/.gstore_history`.
pub fn history_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("GSTORE_HISTFILE") {
        return (!p.is_empty()).then(|| PathBuf::from(p));
    }
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(|h| PathBuf::from(h).join(".gstore_history"))
}

/// The binary's entry point: the process exit code.
pub fn main(args: &[String]) -> i32 {
    let opts = match parse_args(args) {
        Ok(Invocation::Run(o)) => o,
        Ok(Invocation::Help) => {
            print!("{}", help_text());
            return 0;
        }
        Ok(Invocation::Version) => {
            print!("{}", commands::version_text());
            return 0;
        }
        Err(e) => {
            println!("Wrong usage.");
            println!("gconsole: {e}");
            print!("{}", help_text());
            return 2;
        }
    };
    if let Some(db) = &opts.local {
        if opts.legacy_local {
            eprintln!(
                "gconsole: note: `gconsole <db>` is the embedded console, now spelled `gconsole --local <db>`; \
                 without it gconsole connects to gserver"
            );
        }
        return local::run(db);
    }
    let port = match resolve_port(&opts) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("gconsole: {e}");
            return 2;
        }
    };
    let host = opts.host.clone().unwrap_or_else(|| DEFAULT_HOST.to_string());
    run_client(&opts, ApiClient::new(&host, port))
}

/// Log in (upstream `login`), retrying while a person types the password.
fn login(console: &mut Console, opts: &Options) -> Result<(), i32> {
    let mut user = opts.user.clone();
    let mut password = opts
        .password
        .clone()
        .or_else(|| std::env::var("GSTORE_PASSWORD").ok());
    let mut wrong = 0;
    loop {
        let name = match user.take() {
            Some(u) => u,
            None => match console.prompt_line("Enter user name: ", false) {
                Some(u) => u.trim().to_string(),
                None => return Err(1),
            },
        };
        let typed = password.is_none();
        let pw = match password.take() {
            Some(p) => p,
            None => match console.prompt_line("Enter password: ", true) {
                Some(p) => p,
                None => return Err(1),
            },
        };
        console.client.username = name;
        console.client.password = pw;
        let body = console.client.body("login", vec![]);
        match console.client.call_interruptible(&body) {
            Ok(j) => {
                let code = j.get("StatusCode").and_then(crate::json::Json::as_i64);
                if code == Some(0) {
                    return Ok(());
                }
                let msg = j
                    .get("StatusMsg")
                    .and_then(crate::json::Json::as_str)
                    .unwrap_or_default();
                println!("login failed: {msg}");
            }
            Err(CallError::Interrupted) => return Err(1),
            Err(e) => {
                println!("Could not connect to server. Please check server status");
                eprintln!("gconsole: {e} ({})", console.client.url());
                return Err(1);
            }
        }
        // Only a person at a terminal gets another try.
        if !(typed && term::stdin_is_tty()) {
            return Err(1);
        }
        wrong += 1;
        if wrong >= MAX_WRONG_PSWD_TIMES {
            println!("Please check your remember list for your usrname and password.");
            return Err(1);
        }
        println!("Username or password wrong! Please try again.");
    }
}

/// Where the statements come from.
enum Source {
    /// stdin, read plainly (prompts printed when a person types).
    Plain { prompts: bool },
    /// `-e`: the given text, then the end.
    Exec(Option<String>),
    /// stdin through the line editor.
    #[cfg(feature = "readline")]
    Editor(Box<editor::Editor>),
}

impl Source {
    fn read(&mut self, console: &mut Console, prompt: &str) -> ReadOutcome {
        match self {
            Source::Plain { prompts } => {
                if *prompts {
                    print!("{prompt}");
                    let _ = io::stdout().flush();
                }
                let line = console.reader.read_line();
                if *prompts && line == ReadOutcome::Interrupted {
                    println!();
                }
                line
            }
            Source::Exec(text) => match text.take() {
                Some(t) => ReadOutcome::Line(t),
                None => ReadOutcome::Eof,
            },
            #[cfg(feature = "readline")]
            Source::Editor(ed) => ed.read(prompt),
        }
    }

    fn add_history(&mut self, _entry: &str) {
        #[cfg(feature = "readline")]
        if let Source::Editor(ed) = self {
            ed.add_history(_entry);
        }
    }
}

/// Connect, log in and run the console; the process exit code.
fn run_client(opts: &Options, client: ApiClient) -> i32 {
    let stdin_tty = term::stdin_is_tty();
    let interactive = opts.exec.is_none() && (stdin_tty || opts.interactive);
    term::install_sigint();
    let mut console = Console::new(client);
    console.show_prompts = stdin_tty || opts.interactive;
    if let Err(code) = login(&mut console, opts) {
        return code;
    }
    let mut source = match &opts.exec {
        Some(cmds) => Source::Exec(Some(cmds.clone())),
        None => plain_or_editor(&mut console, interactive),
    };
    if interactive {
        print!("{}", banner());
        console.history = History::open(history_path());
        for e in console.history.entries().to_vec() {
            source.add_history(&e);
        }
    }
    repl(&mut console, &mut source, interactive);
    if !interactive && console.failures > 0 {
        1
    } else {
        0
    }
}

#[cfg(feature = "readline")]
fn plain_or_editor(console: &mut Console, interactive: bool) -> Source {
    if interactive && term::stdin_is_tty() && term::stdout_is_tty() {
        console.refresh_db_names();
        if let Some(ed) = editor::Editor::new(std::rc::Rc::clone(&console.db_names)) {
            return Source::Editor(Box::new(ed));
        }
    }
    Source::Plain {
        prompts: interactive,
    }
}

#[cfg(not(feature = "readline"))]
fn plain_or_editor(_console: &mut Console, interactive: bool) -> Source {
    Source::Plain {
        prompts: interactive,
    }
}

/// Read statements and run them until `quit`, the end of input or (with
/// `-e`) the end of the commands.
fn repl(console: &mut Console, source: &mut Source, interactive: bool) {
    let mut buf = String::new();
    loop {
        loop {
            match syntax::next_statement(&buf, false) {
                Next::Statement { text, consumed } => {
                    buf.drain(..consumed);
                    if interactive {
                        let entry = format!("{};", text.replace(['\n', '\r'], " "));
                        console.history.add(&entry);
                        source.add_history(&entry);
                    }
                    console.execute(&text);
                    if console.quit {
                        return;
                    }
                }
                Next::Blank { consumed } => {
                    buf.drain(..consumed);
                    if buf.is_empty() {
                        break;
                    }
                }
                Next::Incomplete => break,
            }
        }
        let prompt = if buf.trim().is_empty() {
            buf.clear();
            console.prompt()
        } else {
            CROSS_LINE_PROMPT.to_string()
        };
        match source.read(console, &prompt) {
            ReadOutcome::Line(line) => {
                buf.push_str(&line);
                buf.push('\n');
            }
            ReadOutcome::Interrupted => buf.clear(),
            ReadOutcome::Eof => {
                // A script's last statement may lack its `;`; a person's
                // unfinished one is dropped (upstream).
                if !interactive {
                    if let Next::Statement { text, .. } = syntax::next_statement(&buf, true) {
                        console.execute(&text);
                    }
                } else {
                    println!();
                    println!();
                }
                console.current_db.clear();
                return;
            }
        }
    }
}

/// Line editing, history recall and completion (the `readline` feature).
#[cfg(feature = "readline")]
mod editor {
    use std::cell::RefCell;
    use std::rc::Rc;

    use rustyline::completion::{Completer, Pair};
    use rustyline::error::ReadlineError;
    use rustyline::highlight::Highlighter;
    use rustyline::hint::Hinter;
    use rustyline::history::DefaultHistory;
    use rustyline::validate::Validator;
    use rustyline::{Context, Helper};

    use super::commands;
    use super::term::{self, ReadOutcome};

    /// Completes the first word of a statement with command names, later
    /// words with database names.
    pub struct ConsoleHelper {
        db_names: Rc<RefCell<Vec<String>>>,
    }

    impl Completer for ConsoleHelper {
        type Candidate = Pair;

        fn complete(
            &self,
            line: &str,
            pos: usize,
            _ctx: &Context<'_>,
        ) -> rustyline::Result<(usize, Vec<Pair>)> {
            let before = &line[..pos];
            let start = before
                .rfind(|c: char| c.is_whitespace() || c == ';')
                .map_or(0, |i| i + 1);
            let word = &before[start..];
            let first = before[..start]
                .rsplit(';')
                .next()
                .unwrap_or_default()
                .trim()
                .is_empty();
            let candidates: Vec<String> = if first {
                commands::command_names()
                    .into_iter()
                    .filter(|n| n.starts_with(&word.to_ascii_lowercase()))
                    .map(str::to_string)
                    .collect()
            } else {
                self.db_names
                    .borrow()
                    .iter()
                    .filter(|n| n.starts_with(word))
                    .cloned()
                    .collect()
            };
            Ok((
                start,
                candidates
                    .into_iter()
                    .map(|c| Pair {
                        display: c.clone(),
                        replacement: c,
                    })
                    .collect(),
            ))
        }
    }

    impl Hinter for ConsoleHelper {
        type Hint = String;
    }
    impl Highlighter for ConsoleHelper {}
    impl Validator for ConsoleHelper {}
    impl Helper for ConsoleHelper {}

    pub struct Editor {
        ed: rustyline::Editor<ConsoleHelper, DefaultHistory>,
    }

    impl Editor {
        pub fn new(db_names: Rc<RefCell<Vec<String>>>) -> Option<Editor> {
            let config = rustyline::Config::builder()
                .max_history_size(commands::History::MAX)
                .ok()?
                .auto_add_history(false)
                .build();
            let mut ed = rustyline::Editor::with_config(config).ok()?;
            ed.set_helper(Some(ConsoleHelper { db_names }));
            Some(Editor { ed })
        }

        pub fn read(&mut self, prompt: &str) -> ReadOutcome {
            match self.ed.readline(prompt) {
                Ok(line) => ReadOutcome::Line(line),
                Err(ReadlineError::Interrupted) => {
                    term::note_interrupt();
                    ReadOutcome::Interrupted
                }
                Err(_) => ReadOutcome::Eof,
            }
        }

        pub fn add_history(&mut self, entry: &str) {
            let _ = self.ed.add_history_entry(entry);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn command_lines() {
        assert_eq!(
            parse_args(&args(&["-u", "root", "-p", "123456", "--port", "9001", "-e", "showdbs;"])),
            Ok(Invocation::Run(Options {
                user: Some("root".into()),
                password: Some("123456".into()),
                port: Some(9001),
                exec: Some("showdbs;".into()),
                ..Options::default()
            }))
        );
        assert_eq!(
            parse_args(&args(&["--user=bob", "--host=10.0.0.2", "-i"])),
            Ok(Invocation::Run(Options {
                user: Some("bob".into()),
                host: Some("10.0.0.2".into()),
                interactive: true,
                ..Options::default()
            }))
        );
        assert_eq!(parse_args(&args(&["-?"])), Ok(Invocation::Help));
        assert_eq!(parse_args(&args(&["--help"])), Ok(Invocation::Help));
        assert_eq!(parse_args(&args(&["-v"])), Ok(Invocation::Version));
        assert!(parse_args(&args(&["-u"])).is_err());
        assert!(parse_args(&args(&["--port", "x"])).is_err());
        assert!(parse_args(&args(&["--bogus"])).is_err());
        assert_eq!(
            parse_args(&args(&["--local", "lubm"])),
            Ok(Invocation::Run(Options {
                local: Some("lubm".into()),
                ..Options::default()
            }))
        );
        assert_eq!(
            parse_args(&args(&["lubm"])),
            Ok(Invocation::Run(Options {
                local: Some("lubm".into()),
                legacy_local: true,
                ..Options::default()
            }))
        );
    }

    #[test]
    fn ports_come_from_the_configuration() {
        let dir = std::env::temp_dir().join(format!("gconsole_conf_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let conf = dir.join("conf.ini");
        std::fs::write(&conf, "[server]\nport=9123\n").unwrap();
        let o = Options {
            config: Some(conf.clone()),
            ..Options::default()
        };
        assert_eq!(resolve_port(&o), Ok(9123));
        let o = Options {
            config: Some(conf),
            port: Some(7),
            ..Options::default()
        };
        assert_eq!(resolve_port(&o), Ok(7));
        let o = Options {
            config: Some(dir.join("missing.ini")),
            ..Options::default()
        };
        assert!(resolve_port(&o).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn texts() {
        assert!(help_text().contains("Usage: bin/gconsole [OPTIONS]"));
        assert!(banner().contains("Welcome to the gStore Console."));
        assert!(commands::version_text().starts_with("gStore version: 1.4 Source distribution\n"));
    }
}
