//! The engine seam (G2 step 4): every engine the competition corpus runs on, opened by name, each a
//! fresh database per file and run at steady state as the Amphora one is (its log settled into the
//! database before the first query after any statement). The transport is each engine's own, with
//! the fewest dependencies: its command-line shell kept open for the whole file (one process, one
//! connection, so a query's cost is the engine's, not a start-up's), or, for ClickHouse, its HTTP
//! interface. An engine this machine lacks is refused with [`Unavailable`]: it never silently passes.
//!
//! Where a server's address is needed it comes from `CORPUS_DSN_<ENGINE>`, by default the address of
//! the bench's own container for it (bench/corpus/run.sh, bench/servers/<engine>.sh).

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

use crate::answer::Value;
use crate::run::{Engine, Stop};

/// Every engine G2 step 4 names (bench/corpus/run.sh's list), in its order.
pub const ENGINES: [&str; 9] = [
    "sqlite",
    "duckdb",
    "amphora",
    "turso",
    "postgres",
    "orioledb",
    "cedardb",
    "mysql",
    "clickhouse",
];

/// Why an engine can't run here.
#[derive(Clone, Debug, PartialEq)]
pub enum Unavailable {
    /// Not one of [`ENGINES`].
    Unknown(String),
    /// Its program isn't on this machine.
    Missing {
        engine: &'static str,
        program: String,
        why: String,
    },
    /// Its program runs but its database didn't open (a server not running, refused credentials).
    Unreachable { engine: &'static str, why: String },
}

impl std::fmt::Display for Unavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unknown(e) => write!(f, "no engine {e} (engines: {})", ENGINES.join(", ")),
            Self::Missing {
                engine,
                program,
                why,
            } => write!(f, "{engine}: its program {program} isn't here ({why})"),
            Self::Unreachable { engine, why } => {
                write!(f, "{engine}: its database didn't open: {why}")
            }
        }
    }
}

/// Names each file's fresh database (or schema) apart on a shared server.
static OPENED: AtomicU64 = AtomicU64::new(0);

fn fresh_name() -> String {
    let n = OPENED.fetch_add(1, Ordering::Relaxed);
    format!("corpus_{}_{n}", std::process::id())
}

fn dsn(engine: &str, default: &str) -> String {
    std::env::var(format!("CORPUS_DSN_{}", engine.to_ascii_uppercase()))
        .unwrap_or_else(|_| default.to_owned())
}

/// Opens `name` on a fresh database. The program each shell engine runs can be pointed elsewhere
/// with `CORPUS_BIN_<ENGINE>` (a build of its own, a path off PATH), a command line split at its
/// spaces, so a client inside a server's container runs as `docker exec -i <container> psql`.
/// `bound` is how long each statement that opens it (its fresh database), settles it or drops it
/// may take, the run's budget (G2 step 7): a server that doesn't answer within it is Unreachable,
/// and None waits on it as long as it takes.
pub fn open(name: &str, bound: Option<Duration>) -> Result<Box<dyn Engine>, Unavailable> {
    let bin = |engine: &str, default: &str| {
        std::env::var(format!("CORPUS_BIN_{}", engine.to_ascii_uppercase()))
            .unwrap_or_else(|_| default.to_owned())
    };
    let shell = |spec: ShellSpec| Shell::open(spec, bound).map(boxed);
    match name {
        "amphora" => crate::amphora::Amphora::in_memory()
            .map(|a| Box::new(a) as Box<dyn Engine>)
            .map_err(|why| Unavailable::Unreachable {
                engine: "amphora",
                why,
            }),
        "sqlite" => shell(sqlite_shell("sqlite", bin("sqlite", "sqlite3"))),
        "duckdb" => shell(duckdb(bin("duckdb", "duckdb"))),
        "turso" => shell(turso(bin("turso", "tursodb"))),
        // Run.sh's addresses for the bench's containers (bench/servers/<engine>.sh).
        "postgres" => shell(postgres(
            "postgres",
            bin("postgres", "psql"),
            dsn("postgres", "postgres://postgres@127.0.0.1:55432/postgres"),
        )),
        "orioledb" => shell(postgres(
            "orioledb",
            bin("orioledb", "psql"),
            dsn("orioledb", "postgres://postgres@127.0.0.1:55433/postgres"),
        )),
        "cedardb" => shell(postgres(
            "cedardb",
            bin("cedardb", "psql"),
            dsn(
                "cedardb",
                "postgres://postgres:amphorabenchlocalonly2026@127.0.0.1:55434/postgres",
            ),
        )),
        "mysql" => shell(mysql(
            bin("mysql", "mysql"),
            &dsn("mysql", "mysql://root@127.0.0.1:33306/convo"),
        )?),
        "clickhouse" => {
            ClickHouse::open(&dsn("clickhouse", "http://127.0.0.1:58123"), bound).map(boxed)
        }
        other => Err(Unavailable::Unknown(other.to_owned())),
    }
}

fn boxed<E: Engine + 'static>(e: E) -> Box<dyn Engine> {
    Box::new(e)
}

/// How a shell writes a row.
#[derive(Clone, Copy, Debug)]
pub enum Rows {
    /// SQLite's shell `.mode quote`: each value an SQL literal, commas between (typed: NULL,
    /// numbers bare, text quoted, blobs X'..').
    Quote,
    /// Values as text between `field`s, NULL written as `null`, with MySQL's batch escapes when
    /// `escapes` (\t \n \\ \0).
    Delimited {
        field: char,
        null: &'static str,
        escapes: bool,
    },
}

/// One engine's shell: what runs, how it's driven and how its answers read.
pub struct ShellSpec {
    pub engine: &'static str,
    pub program: String,
    pub args: Vec<String>,
    /// Statements run when it opens (its fresh database), each refused one making it unreachable.
    pub setup: Vec<String>,
    /// Statements run when it closes (the fresh database dropped).
    pub teardown: Vec<String>,
    /// The shell's command that prints the line it's given, alone (after each statement: the end
    /// of that statement's output).
    pub echo: fn(&str) -> String,
    /// Whether a statement is ended by an added `;` line (shells that run an empty statement as
    /// nothing; MySQL's answers one with an error, so it gets `;` on the statement's own line).
    pub terminator_line: bool,
    /// The statement that brings it to its steady state, if it has one.
    pub settle: Option<&'static str>,
    pub rows: Rows,
    /// Whether a line of its output says the statement failed.
    pub error: fn(&str) -> bool,
    /// For a shell whose rows can't carry every text whole, the query that counts a query's rows
    /// (Engine::recount).
    pub recount: Option<fn(&str) -> String>,
}

/// SQLite's own shell (`sqlite3`), on an in-memory database as Amphora's is.
pub fn sqlite_shell(engine: &'static str, program: String) -> ShellSpec {
    ShellSpec {
        engine,
        program,
        args: ["-batch", "-quote", ":memory:"].map(String::from).to_vec(),
        setup: Vec::new(),
        teardown: Vec::new(),
        echo: |m| format!(".print {m}"),
        terminator_line: true,
        settle: Some("PRAGMA wal_checkpoint(TRUNCATE)"),
        rows: Rows::Quote,
        recount: None,
        error: |l| {
            !l.starts_with('\'')
                && (l.starts_with("Parse error")
                    || l.starts_with("Runtime error")
                    || l.starts_with("Error"))
        },
    }
}

/// DuckDB's shell (SQLite's shell's descendant: the same `.mode quote` and `.print`). Its quote mode
/// writes a header line and puts its column separator (`|` by default) between values, so both are
/// set as SQLite's shell has them (checked on DuckDB 2.0.1's release CLI).
pub fn duckdb(program: String) -> ShellSpec {
    ShellSpec {
        engine: "duckdb",
        program,
        args: [
            "-batch",
            "-noheader",
            "-quote",
            "-separator",
            ",",
            ":memory:",
        ]
        .map(String::from)
        .to_vec(),
        settle: Some("CHECKPOINT"),
        // Its errors are "<Kind> Error: ..." (Parser, Catalog, Binder, Conversion, ...).
        error: |l| !l.starts_with('\'') && (l.contains("Error: ") || l.starts_with("Error")),
        ..sqlite_shell("duckdb", String::new())
    }
}

/// Turso's shell (`tursodb`), its list mode with `|` between values. Its NULL is nothing there, as
/// empty text is, so `.nullvalue` makes it [`NULL_MARK`]. Its shell has no quote, JSON or escaped
/// mode and no `.separator` (tursodb 0.8.4, and its main at 0.9.0-pre.6: `OutputMode` is list,
/// pretty and line, cli/input.rs, none escaping), so a text value holding `|` or a line end reads
/// as more values or rows than it is: such an answer is counted again by Turso itself
/// (`SELECT count(*) FROM (...)`) and, when what was read doesn't match, is unreadable, never a
/// disagreement (Engine::recount).
pub fn turso(program: String) -> ShellSpec {
    ShellSpec {
        engine: "turso",
        program,
        args: ["--quiet", "--output-mode", "list", ":memory:"]
            .map(String::from)
            .to_vec(),
        setup: vec![format!(".nullvalue {NULL_MARK}")],
        teardown: Vec::new(),
        // A SELECT of its own, ended: the line after a statement isn't read until its `;`.
        echo: |m| format!("SELECT '{m}';"),
        terminator_line: true,
        settle: Some("PRAGMA wal_checkpoint(TRUNCATE)"),
        rows: Rows::Delimited {
            field: '|',
            null: NULL_MARK,
            escapes: false,
        },
        recount: Some(|q| format!("SELECT count(*) FROM ({q})")),
        // Its errors are miette reports, "  × Parse error: no such table: nope", or "Error: ...".
        error: |l| {
            let t = l.trim_start();
            t.starts_with('×') || t.starts_with("Error") || t.starts_with("Parse error")
        },
    }
}

/// The NULL a text shell writes: no text a corpus answer holds.
const NULL_MARK: &str = "\u{1}NULL\u{1}";

/// `psql` on one PostgreSQL-protocol server (PostgreSQL, OrioleDB, CedarDB), a schema of its own per
/// file. CHECKPOINT is its steady state: every dirty page written, its WAL recycled.
pub fn postgres(engine: &'static str, program: String, dsn: String) -> ShellSpec {
    let schema = fresh_name();
    ShellSpec {
        engine,
        program,
        args: vec![
            "-X".into(),
            "-q".into(),
            "-A".into(),
            "-t".into(),
            "-F".into(),
            "\u{1f}".into(),
            "-P".into(),
            format!("null={NULL_MARK}"),
            "-v".into(),
            "ON_ERROR_STOP=0".into(),
            // Its statements from `-f -` (standard input) rather than from standard input as such:
            // only a file's errors are written `psql:<file>:<line>: ERROR:`, which is how a refusal
            // is told from a row (psql's documentation of -f; its log_locus_callback).
            "-f".into(),
            "-".into(),
            "-d".into(),
            dsn,
        ],
        setup: vec![
            "SET client_min_messages = error".into(),
            format!("CREATE SCHEMA {schema}"),
            format!("SET search_path = {schema}"),
        ],
        teardown: vec![format!("DROP SCHEMA {schema} CASCADE")],
        echo: |m| format!("\\echo {m}"),
        terminator_line: true,
        // CedarDB (v2026-10-08) answers CHECKPOINT "ERROR: CHECKPOINT not implemented yet" (run on
        // its image, 2026-11-21) and documents no other statement that settles it, so its settle
        // runs none, rather than every query after a statement failing its settle.
        settle: if engine == "cedardb" {
            None
        } else {
            Some("CHECKPOINT")
        },
        rows: Rows::Delimited {
            field: '\u{1f}',
            null: NULL_MARK,
            escapes: false,
        },
        error: |l| l.starts_with("psql:") && (l.contains("ERROR:") || l.contains("FATAL:")),
        recount: None,
    }
}

/// The `mysql` client in batch mode on a database of its own per file. FLUSH TABLES is its steady
/// state: every table's changes written and its caches closed.
pub fn mysql(program: String, dsn: &str) -> Result<ShellSpec, Unavailable> {
    let bad = |why: &str| Unavailable::Unreachable {
        engine: "mysql",
        why: format!("{dsn}: {why}"),
    };
    let rest = dsn
        .strip_prefix("mysql://")
        .ok_or_else(|| bad("not mysql://user[:password]@host:port/db"))?;
    let (auth, addr) = rest.split_once('@').ok_or_else(|| bad("no user@"))?;
    let (user, password) = auth
        .split_once(':')
        .map_or((auth, None), |(u, p)| (u, Some(p)));
    let addr = addr.split_once('/').map_or(addr, |(a, _)| a);
    let (host, port) = addr.split_once(':').unwrap_or((addr, "3306"));
    let db = fresh_name();
    let mut args = vec![
        "--batch".into(),
        "--skip-column-names".into(),
        "--force".into(),
        "--unbuffered".into(),
        "--default-character-set=utf8mb4".into(),
        format!("--host={host}"),
        format!("--port={port}"),
        format!("--user={user}"),
    ];
    if let Some(p) = password {
        args.push(format!("--password={p}"));
    }
    Ok(ShellSpec {
        engine: "mysql",
        program,
        args,
        setup: vec![format!("CREATE DATABASE {db}"), format!("USE {db}")],
        teardown: vec![format!("DROP DATABASE {db}")],
        echo: |m| format!("SELECT '{m}';"),
        terminator_line: false,
        settle: Some("FLUSH TABLES"),
        rows: Rows::Delimited {
            field: '\t',
            null: "NULL",
            escapes: true,
        },
        error: |l| l.starts_with("ERROR "),
        recount: None,
    })
}

/// A shell kept open for one file: statements written to its input, its output and errors read
/// from one pipe in the order it wrote them, each statement's end found by the line it echoes.
pub struct Shell {
    spec: ShellSpec,
    child: Child,
    input: ChildStdin,
    lines: Receiver<String>,
    marks: u64,
    /// Set when the process was stopped (a bound) or ended: no statement runs on it again.
    ended: Option<String>,
    /// How long each opening, settling and closing statement may take (crate::engines::open).
    bound: Option<Duration>,
}

impl Shell {
    /// Its program started and its opening statements run, each within `bound`: one that isn't
    /// answered within it makes the engine Unreachable (a server that never answers can't hold
    /// the run).
    pub fn open(spec: ShellSpec, bound: Option<Duration>) -> Result<Self, Unavailable> {
        let missing = |why: String| Unavailable::Missing {
            engine: spec.engine,
            program: spec.program.clone(),
            why,
        };
        let (reader, writer) = std::io::pipe().map_err(|e| missing(e.to_string()))?;
        let err = writer.try_clone().map_err(|e| missing(e.to_string()))?;
        // CORPUS_BIN_<ENGINE> may be a command line: its first word runs, the rest come first
        // (a program whose own path holds spaces is run whole).
        let mut words = spec.program.split_whitespace();
        let mut cmd = if std::path::Path::new(&spec.program).exists() {
            words = "".split_whitespace();
            Command::new(&spec.program)
        } else {
            Command::new(words.next().unwrap_or_default())
        };
        cmd.args(words)
            .args(&spec.args)
            .stdin(Stdio::piped())
            .stdout(writer)
            .stderr(err);
        let mut child = cmd.spawn().map_err(|e| missing(e.to_string()))?;
        // The pipe's writing ends now live only in the child: its exit ends the reader.
        drop(cmd);
        let input = child
            .stdin
            .take()
            .ok_or_else(|| missing("no input pipe".into()))?;
        let (tx, lines) = channel();
        std::thread::spawn(move || {
            let mut r = BufReader::new(reader);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                match r.read_until(b'\n', &mut buf) {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {
                        let line = String::from_utf8_lossy(&buf);
                        let line = line.strip_suffix('\n').unwrap_or(&line);
                        let line = line.strip_suffix('\r').unwrap_or(line);
                        if tx.send(line.to_owned()).is_err() {
                            return;
                        }
                    }
                }
            }
        });
        let mut shell = Self {
            spec,
            child,
            input,
            lines,
            marks: 0,
            ended: None,
            bound,
        };
        let setup = std::mem::take(&mut shell.spec.setup);
        for s in &setup {
            let until = shell.until();
            shell
                .run(s, 0, until)
                .map_err(|e| Unavailable::Unreachable {
                    engine: shell.spec.engine,
                    why: match e {
                        Stop::PastBudget => format!("{s}: no answer within {bound:?}"),
                        Stop::Refused(e) => format!("{s}: {e}"),
                    },
                })?;
        }
        Ok(shell)
    }

    /// The bound of an opening or closing statement begun now.
    fn until(&self) -> Option<Instant> {
        self.bound.and_then(|b| Instant::now().checked_add(b))
    }

    fn stop(&mut self, why: String) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.ended = Some(why);
    }

    /// The statement's output: every line before its mark.
    fn exchange(&mut self, sql: &str, until: Option<Instant>) -> Result<Vec<String>, Stop> {
        if let Some(why) = &self.ended {
            return Err(Stop::Refused(why.clone()));
        }
        self.marks = self.marks.saturating_add(1);
        let mark = format!("kiln-corpus-mark-{}", self.marks);
        let end = if self.spec.terminator_line {
            "\n;"
        } else {
            ";"
        };
        let text = format!("{sql}{end}\n{}\n", (self.spec.echo)(&mark));
        if let Err(e) = self
            .input
            .write_all(text.as_bytes())
            .and_then(|()| self.input.flush())
        {
            let why = format!("its input closed: {e}");
            self.stop(why.clone());
            return Err(Stop::Refused(why));
        }
        let mut out = Vec::new();
        loop {
            let got = match until {
                None => self
                    .lines
                    .recv()
                    .map_err(|_| RecvTimeoutError::Disconnected),
                Some(u) => self
                    .lines
                    .recv_timeout(u.saturating_duration_since(Instant::now())),
            };
            match got {
                Ok(line) if line == mark => return Ok(out),
                Ok(line) => out.push(line),
                Err(RecvTimeoutError::Timeout) => {
                    // The shell's own interrupt would be a signal it handles only between rows on
                    // some platforms; its process ended is the stop every OS has.
                    self.stop("stopped at an earlier record's bound".into());
                    return Err(Stop::PastBudget);
                }
                Err(RecvTimeoutError::Disconnected) => {
                    let why = format!("its process ended: {}", out.join("\n"));
                    self.stop(why.clone());
                    return Err(Stop::Refused(why));
                }
            }
        }
    }
}

impl Engine for Shell {
    fn run(
        &mut self,
        sql: &str,
        keep: usize,
        until: Option<Instant>,
    ) -> Result<Vec<Vec<Value>>, Stop> {
        let out = self.exchange(sql, until)?;
        if out.iter().any(|l| (self.spec.error)(l)) {
            return Err(Stop::Refused(out.join("\n")));
        }
        let mut rows = match self.spec.rows {
            Rows::Quote => quoted_rows(&out.join("\n")).map_err(Stop::Refused)?,
            Rows::Delimited {
                field,
                null,
                escapes,
            } => out
                .iter()
                .map(|l| {
                    l.split(field)
                        .map(|v| match v {
                            v if v == null => Value::Null,
                            v if escapes => Value::Text(unescape(v)),
                            v => Value::Text(v.as_bytes().to_vec()),
                        })
                        .collect()
                })
                .collect(),
        };
        rows.truncate(keep);
        Ok(rows)
    }

    fn settle(&mut self, until: Option<Instant>) -> Result<(), Stop> {
        match self.spec.settle {
            Some(s) => self.run(s, 0, until).map(drop),
            None => Ok(()),
        }
    }

    fn recount(&mut self, sql: &str, until: Option<Instant>) -> Option<Result<u64, Stop>> {
        let count = (self.spec.recount?)(sql.trim().trim_end_matches(';'));
        Some(self.run(&count, 2, until).and_then(|rows| {
            match rows.as_slice() {
                [row] => match row.as_slice() {
                    [Value::Integer(n)] => u64::try_from(*n).ok(),
                    [Value::Text(t)] => std::str::from_utf8(t).ok().and_then(|t| t.parse().ok()),
                    _ => None,
                },
                _ => None,
            }
            .ok_or_else(|| Stop::Refused(format!("{count}: not one count: {rows:?}")))
        }))
    }

    fn ended(&self) -> bool {
        self.ended.is_some()
    }
}

impl Drop for Shell {
    fn drop(&mut self) {
        if self.ended.is_none() {
            let teardown = std::mem::take(&mut self.spec.teardown);
            for s in &teardown {
                let until = self.until();
                let _ = self.run(s, 0, until);
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// MySQL's batch escapes undone: \t \n \\ \0.
fn unescape(v: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len());
    let mut it = v.bytes();
    while let Some(b) = it.next() {
        if b != b'\\' {
            out.push(b);
            continue;
        }
        match it.next() {
            Some(b't') => out.push(b'\t'),
            Some(b'n') => out.push(b'\n'),
            Some(b'0') => out.push(0),
            Some(c) => out.push(c),
            None => out.push(b'\\'),
        }
    }
    out
}

/// Rows in SQLite's shell's quote mode: values separated by `,`, rows by line ends outside quotes.
/// A bare word that isn't NULL or a number (DuckDB's true, a date) is text.
pub fn quoted_rows(text: &str) -> Result<Vec<Vec<Value>>, String> {
    let mut rows = Vec::new();
    if text.is_empty() {
        return Ok(rows);
    }
    let mut row = Vec::new();
    let mut chars = text.chars().peekable();
    loop {
        let value = match chars.peek() {
            Some('\'') => {
                chars.next();
                let mut s = String::new();
                loop {
                    match chars.next() {
                        Some('\'') if chars.peek() == Some(&'\'') => {
                            chars.next();
                            s.push('\'');
                        }
                        Some('\'') => break,
                        Some(c) => s.push(c),
                        None => return Err(format!("an unclosed quote in {text}")),
                    }
                }
                Value::Text(s.into_bytes())
            }
            _ => {
                let mut s = String::new();
                while let Some(&c) = chars.peek() {
                    if c == ',' || c == '\n' {
                        break;
                    }
                    s.push(c);
                    chars.next();
                }
                bare(&s)?
            }
        };
        row.push(value);
        match chars.next() {
            Some(',') => {}
            Some('\n') => rows.push(std::mem::take(&mut row)),
            None => {
                rows.push(row);
                return Ok(rows);
            }
            Some(c) => return Err(format!("{c:?} after a value in {text}")),
        }
    }
}

fn bare(s: &str) -> Result<Value, String> {
    if s == "NULL" {
        return Ok(Value::Null);
    }
    if let Some(hex) = s.strip_prefix("X'").and_then(|h| h.strip_suffix('\'')) {
        let digits = hex.as_bytes();
        return digits
            .chunks(2)
            .map(|p| {
                std::str::from_utf8(p)
                    .ok()
                    .and_then(|p| u8::from_str_radix(p, 16).ok())
                    .ok_or_else(|| format!("a blob that isn't hex: {s}"))
            })
            .collect::<Result<Vec<u8>, String>>()
            .map(Value::Blob);
    }
    if let Ok(i) = s.parse::<i64>() {
        return Ok(Value::Integer(i));
    }
    if let Ok(r) = s.parse::<f64>() {
        return Ok(Value::Real(r));
    }
    Ok(Value::Text(s.as_bytes().to_vec()))
}

/// ClickHouse through its HTTP interface (its own shell reads all of its input before it runs any
/// of it, so it can't be driven a statement at a time): one request a statement, a database of its
/// own per file, answers as TabSeparated (NULL as \N, its escapes as MySQL's). The bound is its own
/// `max_execution_time`, so the server interrupts the query, and the socket's read timeout.
pub struct ClickHouse {
    addr: String,
    host: String,
    db: String,
    ended: Option<String>,
    /// How long each opening, settling and closing request may take (crate::engines::open).
    bound: Option<Duration>,
}

impl ClickHouse {
    /// Its fresh database made, within `bound`: a server that doesn't answer within it is
    /// Unreachable.
    pub fn open(url: &str, bound: Option<Duration>) -> Result<Self, Unavailable> {
        let addr = url
            .strip_prefix("http://")
            .ok_or_else(|| Unavailable::Unreachable {
                engine: "clickhouse",
                why: format!("{url}: not http://host:port"),
            })?
            .trim_end_matches('/')
            .to_owned();
        let mut ch = Self {
            host: addr.split(':').next().unwrap_or(&addr).to_owned(),
            addr,
            db: "default".into(),
            ended: None,
            bound,
        };
        let db = fresh_name();
        let until = ch.until();
        ch.request(&format!("CREATE DATABASE {db}"), until)
            .map_err(|e| Unavailable::Unreachable {
                engine: "clickhouse",
                why: match e {
                    Stop::PastBudget => format!("no answer within {bound:?}"),
                    Stop::Refused(e) => e,
                },
            })?;
        ch.db = db;
        Ok(ch)
    }

    /// The bound of an opening or closing request begun now.
    fn until(&self) -> Option<Instant> {
        self.bound.and_then(|b| Instant::now().checked_add(b))
    }

    fn request(&mut self, sql: &str, until: Option<Instant>) -> Result<String, Stop> {
        if let Some(why) = &self.ended {
            return Err(Stop::Refused(why.clone()));
        }
        let refused = |e: std::io::Error| Stop::Refused(e.to_string());
        let mut path = format!("/?database={}&default_format=TabSeparated", self.db);
        let left = until.map(|u| u.saturating_duration_since(Instant::now()));
        if left.is_some_and(|l| l.is_zero()) {
            return Err(Stop::PastBudget);
        }
        let mut stream = match left {
            None => TcpStream::connect(&self.addr).map_err(refused)?,
            // A server that takes the connection no sooner than the bound is past it.
            Some(left) => {
                let at = self
                    .addr
                    .to_socket_addrs()
                    .map_err(refused)?
                    .next()
                    .ok_or_else(|| Stop::Refused(format!("{}: no address", self.addr)))?;
                TcpStream::connect_timeout(&at, left).map_err(|e| match e.kind() {
                    std::io::ErrorKind::TimedOut => Stop::PastBudget,
                    _ => refused(e),
                })?
            }
        };
        if let Some(left) = left {
            path.push_str(&format!(
                "&max_execution_time={}&timeout_overflow_mode=throw",
                left.as_secs_f64()
            ));
            stream.set_read_timeout(Some(left)).map_err(refused)?;
            stream.set_write_timeout(Some(left)).map_err(refused)?;
        }
        let head = format!(
            "POST {path} HTTP/1.0\r\nHost: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            self.host,
            sql.len()
        );
        stream
            .write_all(head.as_bytes())
            .and_then(|()| stream.write_all(sql.as_bytes()))
            .map_err(refused)?;
        let mut body = Vec::new();
        match stream.read_to_end(&mut body) {
            Ok(_) => {}
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return Err(Stop::PastBudget);
            }
            Err(e) => return Err(refused(e)),
        }
        let text = String::from_utf8_lossy(&body).into_owned();
        let (status, rest) = text.split_once("\r\n").unwrap_or((&text, ""));
        let body = rest.split_once("\r\n\r\n").map_or("", |(_, b)| b);
        if status.split(' ').nth(1) == Some("200") {
            Ok(body.to_owned())
        } else if body.contains("TIMEOUT_EXCEEDED") {
            // Code 159: its max_execution_time came first.
            Err(Stop::PastBudget)
        } else {
            Err(Stop::Refused(body.trim().to_owned()))
        }
    }
}

impl Engine for ClickHouse {
    fn run(
        &mut self,
        sql: &str,
        keep: usize,
        until: Option<Instant>,
    ) -> Result<Vec<Vec<Value>>, Stop> {
        let body = self.request(sql, until)?;
        Ok(body
            .lines()
            .take(keep)
            .map(|l| {
                l.split('\t')
                    .map(|v| {
                        if v == "\\N" {
                            Value::Null
                        } else {
                            Value::Text(unescape(v))
                        }
                    })
                    .collect()
            })
            .collect())
    }

    /// Its steady state: every MergeTree table's parts merged into one (OPTIMIZE ... FINAL).
    fn settle(&mut self, until: Option<Instant>) -> Result<(), Stop> {
        let tables = self.request(
            "SELECT name FROM system.tables WHERE database = currentDatabase() AND engine LIKE '%MergeTree'",
            until,
        )?;
        for t in tables.lines() {
            self.request(&format!("OPTIMIZE TABLE `{t}` FINAL"), until)?;
        }
        Ok(())
    }
}

impl Drop for ClickHouse {
    fn drop(&mut self) {
        if self.db != "default" {
            let db = std::mem::take(&mut self.db);
            self.db = "default".into();
            self.ended = None;
            let until = self.until();
            let _ = self.request(&format!("DROP DATABASE {db}"), until);
        }
    }
}
