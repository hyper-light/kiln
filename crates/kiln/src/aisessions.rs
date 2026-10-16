//! Live AI coding-agent sessions on this machine: the Claude Code, Codex and Gemini CLI processes
//! running right now, summarized from the transcripts those tools write.
//!
//! - Claude Code keeps a `~/.claude/sessions/<pid>.json` record per running session (its id,
//!   directory, name, busy or idle); the transcript is `~/.claude/projects/<dir>/<id>.jsonl`.
//! - Codex writes `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`, matched to its process by
//!   directory and start time.
//! - Gemini CLI is found by its process alone.
//!
//! A session whose process runs inside one of kiln's terminals is linked to that terminal, so any
//! device can attach and drive it; the others can only be watched. Only transcript tails are read,
//! and only summaries leave the machine, plus recent messages when someone opens a transcript.

use std::collections::{HashMap, HashSet};
use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sysinfo::{Pid, Process, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

/// Bytes read from the end of a transcript for its summary.
const TAIL: u64 = 256 * 1024;
/// Bytes read from the end of a transcript someone opens, growing (×4) until it holds enough
/// messages: tool output can make up most of a transcript.
const OPEN_TAIL: u64 = 1024 * 1024;
const OPEN_TAIL_MAX: u64 = 16 * 1024 * 1024;
/// Messages returned for an opened transcript.
const OPEN_ENTRIES: usize = 150;
/// Characters kept of the last prompt and reply in a summary.
const SNIPPET: usize = 400;
/// Characters kept of each message in an opened transcript.
const ENTRY_CHARS: usize = 2000;
/// Most sessions reported per machine.
const MAX_SESSIONS: usize = 32;
/// How far up the process tree a kiln terminal is looked for.
const MAX_DEPTH: usize = 16;
/// A transcript untouched this long while mid-turn counts as waiting rather than working.
const STALLED: Duration = Duration::from_secs(120);
/// Codex subcommands that are not an interactive session.
const CODEX_BATCH: &[&str] = &[
    "exec", "e", "app-server", "mcp", "mcp-server", "login", "logout", "completion", "debug", "apply", "a", "proto", "cloud",
    "sandbox", "help", "features", "generate-ts", "responses-api-proxy", "stdio-to-uds", "-h", "--help", "-V", "--version",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Tool {
    Claude,
    Codex,
    Gemini,
}

impl Tool {
    fn name(self) -> &'static str {
        match self {
            Tool::Claude => "claude",
            Tool::Codex => "codex",
            Tool::Gemini => "gemini",
        }
    }
}

/// One live session, as runners report it and the API lists it. Every field has a default so
/// either side can add fields without breaking the other.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct AiSession {
    /// "claude", "codex" or "gemini".
    pub tool: String,
    /// The tool's own session id (empty when it has none).
    pub id: String,
    pub pid: u32,
    /// The session's name, where the tool gives it one (Claude Code).
    pub name: String,
    pub title: String,
    pub cwd: String,
    pub branch: String,
    pub model: String,
    /// Unix seconds the process started.
    pub started_at: i64,
    /// Unix seconds of the last transcript write (the start when there is none).
    pub last_activity: i64,
    /// "working" (mid-turn), "waiting" (the turn ended: waiting for the user), or empty.
    pub state: String,
    pub last_prompt: String,
    pub last_reply: String,
    /// The kiln terminal session it runs in: attach to drive it.
    pub terminal: Option<String>,
    /// A transcript can be opened.
    pub transcript: bool,
}

/// One message of an opened transcript.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Entry {
    /// "user", "assistant" or "tool".
    pub role: String,
    pub text: String,
    /// RFC 3339, when the record has one.
    pub at: String,
}

/// What a transcript says besides its messages.
#[derive(Debug, Clone, Default)]
struct Meta {
    id: String,
    title: String,
    cwd: String,
    branch: String,
    model: String,
    last_prompt: String,
}

#[derive(Debug, Clone, Default)]
struct Summary {
    meta: Meta,
    last_prompt: String,
    last_reply: String,
    /// The last message is the assistant's: its turn ended.
    turn_ended: bool,
}

/// Claude Code's record of a running session (`~/.claude/sessions/<pid>.json`).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaudeRecord {
    pid: u32,
    session_id: String,
    #[serde(default)]
    cwd: String,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    status: Option<String>,
}

/// A runner's side: scans on a blocking thread, one at a time, and reports only changes.
#[derive(Clone)]
pub struct Reporter {
    scanner: Arc<Mutex<Scanner>>,
    /// The last report sent (JSON), to skip unchanged ones.
    last: Arc<Mutex<Option<String>>>,
    scanning: Arc<AtomicBool>,
}

impl Reporter {
    pub fn new(home: PathBuf) -> Self {
        Self { scanner: Arc::new(Mutex::new(Scanner::new(home))), last: Arc::new(Mutex::new(None)), scanning: Arc::new(AtomicBool::new(false)) }
    }

    /// Report on the next scan even when nothing changed (a new server connection).
    pub fn reset(&self) {
        *self.last.lock() = None;
    }

    /// Scan in the background and send the sessions when they changed. `terminals` maps each
    /// kiln terminal's shell pid to its session id.
    pub fn scan(&self, terminals: HashMap<u32, String>, up: async_channel::Sender<crate::exec::Up>) {
        if self.scanning.swap(true, Ordering::AcqRel) {
            return;
        }
        let me = self.clone();
        crate::rt::spawn(async move {
            let scanner = Arc::clone(&me.scanner);
            let sessions = crate::rt::blocking(move || scanner.lock().scan(&terminals)).await;
            me.scanning.store(false, Ordering::Release);
            let Some(json) = sessions.ok().and_then(|s| serde_json::to_string(&s).ok()) else { return };
            {
                let mut last = me.last.lock();
                if last.as_deref() == Some(json.as_str()) {
                    return;
                }
                *last = Some(json.clone());
            }
            // Never queued across a disconnect: a reconnect sends a fresh report anyway.
            let _ = up.send(crate::exec::Up::Term(crate::proto::AgentMsg::AiSessions { json })).await;
        });
    }

    /// Answer the server's request for a session's recent messages.
    pub fn transcript(&self, req: u64, pid: u32, up: async_channel::Sender<crate::exec::Up>) {
        let scanner = Arc::clone(&self.scanner);
        crate::rt::spawn(async move {
            let entries = crate::rt::blocking(move || scanner.lock().transcript(pid)).await.ok().flatten();
            let json = entries.and_then(|e| serde_json::to_string(&e).ok());
            let _ = up.send(crate::exec::Up::Term(crate::proto::AgentMsg::AiTranscript { req, json })).await;
        });
    }
}

/// What a scan needs from a process (copied out of the process table).
struct Proc {
    pid: u32,
    /// Unix seconds.
    start: u64,
    cwd: String,
    args: Vec<String>,
}

impl Proc {
    fn of(p: &Process) -> Self {
        Self {
            pid: p.pid().as_u32(),
            start: p.start_time(),
            cwd: p.cwd().map(|c| c.to_string_lossy().into_owned()).unwrap_or_default(),
            args: args_of(p),
        }
    }
}

/// Finds the sessions. Keeps the process table and parsed transcripts between scans, so a scan
/// only rereads transcripts that changed.
pub struct Scanner {
    home: PathBuf,
    sys: System,
    /// Summaries by transcript, with the length and mtime they were read at.
    summaries: HashMap<PathBuf, (u64, SystemTime, Summary)>,
    /// Claude Code transcripts by session id, once found.
    claude_files: HashMap<String, PathBuf>,
    /// Codex transcripts by process (pid, start time), once found.
    codex_files: HashMap<(u32, u64), PathBuf>,
    /// The `session_meta` (id, cwd) of Codex transcripts, read once per file.
    codex_meta: HashMap<PathBuf, (String, String)>,
    /// Each reported session's transcript by pid, for [`Scanner::transcript`].
    open: HashMap<u32, (Tool, PathBuf)>,
}

impl Scanner {
    pub fn new(home: PathBuf) -> Self {
        Self {
            home,
            sys: System::new(),
            summaries: HashMap::new(),
            claude_files: HashMap::new(),
            codex_files: HashMap::new(),
            codex_meta: HashMap::new(),
            open: HashMap::new(),
        }
    }

    /// The sessions running now, most recently active first. `terminals` maps the shell pid of
    /// each kiln terminal on this machine to its session id.
    pub fn scan(&mut self, terminals: &HashMap<u32, String>) -> Vec<AiSession> {
        // Names, parents and command lines of everything; working directories only of the tools.
        self.sys.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing().with_cmd(UpdateKind::OnlyIfNotSet).with_exe(UpdateKind::OnlyIfNotSet).without_tasks(),
        );
        let tools: Vec<Pid> = self.sys.processes().iter().filter(|(_, p)| tool_of(p).is_some()).map(|(pid, _)| *pid).collect();
        if !tools.is_empty() {
            self.sys.refresh_processes_specifics(ProcessesToUpdate::Some(&tools), false, ProcessRefreshKind::nothing().with_cwd(UpdateKind::Always).without_tasks());
        }
        let mut out = Vec::new();
        let mut open = HashMap::new();
        let mut seen: HashSet<u32> = HashSet::new();

        // Claude Code: one record per running session.
        let records_dir = self.home.join(".claude").join("sessions");
        let have_records = records_dir.is_dir();
        for r in claude_records(&records_dir) {
            // The pid may since belong to something else.
            let Some(p) = self.sys.process(Pid::from_u32(r.pid)).filter(|p| tool_of(p) == Some(Tool::Claude)).map(Proc::of) else { continue };
            if r.kind.as_deref().is_some_and(|k| k != "interactive") {
                continue;
            }
            seen.insert(r.pid);
            let file = self.claude_file(&r.session_id, &r.cwd);
            let summary = file.as_deref().and_then(|f| self.summary(Tool::Claude, f));
            let mut s = self.session(Tool::Claude, &p, file.as_deref(), summary, terminals);
            s.id = r.session_id.clone();
            s.name = r.name.unwrap_or_default();
            if s.cwd.is_empty() {
                s.cwd = r.cwd;
            }
            match r.status.as_deref() {
                Some("busy") => s.state = "working".into(),
                Some("idle") => s.state = "waiting".into(),
                _ => {}
            }
            if let Some(f) = file {
                open.insert(r.pid, (Tool::Claude, f));
            }
            out.push(s);
        }

        // Everything else by its process: Codex, Gemini, and Claude Code versions without records.
        let mut found: Vec<(Tool, Proc)> = Vec::new();
        for (pid, p) in self.sys.processes() {
            let Some(tool) = tool_of(p) else { continue };
            if seen.contains(&pid.as_u32()) || (tool == Tool::Claude && have_records) || !interactive(tool, &args_of(p)) {
                continue;
            }
            // A tool's own helpers and relaunches run under it: report the outermost one.
            if p.parent().and_then(|pp| self.sys.process(pp)).and_then(tool_of) == Some(tool) {
                continue;
            }
            found.push((tool, Proc::of(p)));
        }
        // Newest first, so two sessions in one directory each claim their own transcript.
        found.sort_by_key(|(_, p)| std::cmp::Reverse(p.start));
        let mut taken: HashSet<PathBuf> = HashSet::new();
        for (tool, p) in found {
            let file = match tool {
                Tool::Codex => self.codex_file(&p, &taken),
                Tool::Claude => self.claude_file_by_cwd(&p, &taken),
                Tool::Gemini => None,
            };
            if let Some(f) = &file {
                taken.insert(f.clone());
            }
            let summary = file.as_deref().and_then(|f| self.summary(tool, f));
            let s = self.session(tool, &p, file.as_deref(), summary, terminals);
            if let Some(f) = file {
                open.insert(p.pid, (tool, f));
            }
            out.push(s);
        }

        self.open = open;
        // Forget what no longer runs.
        let live: HashSet<&PathBuf> = self.open.values().map(|(_, f)| f).collect();
        self.summaries.retain(|f, _| live.contains(f));
        let sys = &self.sys;
        self.codex_files.retain(|(pid, start), _| sys.process(Pid::from_u32(*pid)).is_some_and(|p| p.start_time() == *start));
        if self.codex_meta.len() > 4096 {
            self.codex_meta.clear();
        }
        out.sort_by_key(|s| std::cmp::Reverse(s.last_activity));
        out.truncate(MAX_SESSIONS);
        out
    }

    /// Recent messages of the transcript of the session running as `pid` (as of the last scan).
    pub fn transcript(&self, pid: u32) -> Option<Vec<Entry>> {
        let (tool, file) = self.open.get(&pid)?;
        let len = std::fs::metadata(file).ok()?.len();
        let mut tail = OPEN_TAIL;
        let mut entries = loop {
            let (_, entries) = parse(*tool, &tail_lines(file, tail)?);
            if entries.len() >= OPEN_ENTRIES || tail >= len || tail >= OPEN_TAIL_MAX {
                break entries;
            }
            tail = tail.saturating_mul(4);
        };
        let skip = entries.len().saturating_sub(OPEN_ENTRIES);
        entries.drain(..skip);
        Some(entries)
    }

    fn session(&self, tool: Tool, p: &Proc, file: Option<&Path>, summary: Option<Summary>, terminals: &HashMap<u32, String>) -> AiSession {
        let started_at = i64::try_from(p.start).unwrap_or(0);
        let modified = file.and_then(|f| std::fs::metadata(f).ok()?.modified().ok());
        let summary = summary.unwrap_or_default();
        let cwd = if summary.meta.cwd.is_empty() { p.cwd.clone() } else { summary.meta.cwd.clone() };
        let state = match (file, summary.turn_ended) {
            (None, _) => "",
            (Some(_), true) => "waiting",
            // Mid-turn but silent for a while: most likely asking the user something.
            (Some(_), false) if modified.and_then(|m| m.elapsed().ok()).is_some_and(|e| e > STALLED) => "waiting",
            (Some(_), false) => "working",
        };
        let title = if summary.meta.title.is_empty() { first_line(&summary.last_prompt, 80) } else { summary.meta.title.clone() };
        AiSession {
            tool: tool.name().to_owned(),
            id: summary.meta.id,
            pid: p.pid,
            name: String::new(),
            title,
            cwd,
            branch: summary.meta.branch,
            model: summary.meta.model,
            started_at,
            last_activity: modified.map_or(started_at, unix_secs).max(started_at),
            state: state.to_owned(),
            last_prompt: clip(&summary.last_prompt, SNIPPET),
            last_reply: clip(&summary.last_reply, SNIPPET),
            terminal: terminal_of(&self.sys, Pid::from_u32(p.pid), terminals),
            transcript: file.is_some(),
        }
    }

    /// The summary of a transcript, reread only when it changed.
    fn summary(&mut self, tool: Tool, file: &Path) -> Option<Summary> {
        let meta = std::fs::metadata(file).ok()?;
        let (len, modified) = (meta.len(), meta.modified().ok()?);
        if let Some((l, m, s)) = self.summaries.get(file)
            && *l == len
            && *m == modified
        {
            return Some(s.clone());
        }
        let lines = tail_lines(file, TAIL)?;
        let (meta, entries) = parse(tool, &lines);
        let s = summarize(meta, &entries);
        self.summaries.insert(file.to_owned(), (len, modified, s.clone()));
        Some(s)
    }

    /// A Claude Code session's transcript: `projects/<dir>/<id>.jsonl`, where `<dir>` is the
    /// working directory with every other character than letters and digits turned into `-`
    /// (looked for in every project when the directory was shortened or moved).
    fn claude_file(&mut self, id: &str, cwd: &str) -> Option<PathBuf> {
        if let Some(f) = self.claude_files.get(id)
            && f.is_file()
        {
            return Some(f.clone());
        }
        let projects = self.home.join(".claude").join("projects");
        let name = format!("{id}.jsonl");
        let direct = projects.join(claude_dir_name(cwd)).join(&name);
        let found = if direct.is_file() {
            Some(direct)
        } else {
            std::fs::read_dir(&projects).ok()?.flatten().map(|e| e.path().join(&name)).find(|p| p.is_file())
        };
        if let Some(f) = &found {
            self.claude_files.insert(id.to_owned(), f.clone());
        }
        found
    }

    /// Without a session record: the newest transcript in the process's project directory
    /// written since it started.
    fn claude_file_by_cwd(&self, p: &Proc, taken: &HashSet<PathBuf>) -> Option<PathBuf> {
        if p.cwd.is_empty() {
            return None;
        }
        let dir = self.home.join(".claude").join("projects").join(claude_dir_name(&p.cwd));
        newest_since(&dir, p.start.saturating_sub(60), taken)
    }

    /// A Codex process's rollout: `codex resume <id>` names it; otherwise the newest rollout of
    /// the same directory written since the process started.
    fn codex_file(&mut self, p: &Proc, taken: &HashSet<PathBuf>) -> Option<PathBuf> {
        let key = (p.pid, p.start);
        if let Some(f) = self.codex_files.get(&key) {
            return Some(f.clone());
        }
        let root = self.home.join(".codex").join("sessions");
        let resume = p.args.iter().position(|a| a == "resume").and_then(|i| p.args.get(i + 1)).filter(|a| !a.starts_with('-'));
        let mut files = Vec::new();
        match resume {
            // Resumed sessions keep writing their original rollout, from whatever day it began.
            Some(id) => walk_jsonl(&root, 3, &mut |f| f.file_name().is_some_and(|n| n.to_string_lossy().ends_with(&format!("{id}.jsonl"))), &mut files),
            None => {
                let since = p.start.saturating_sub(60);
                for day in recent_days(&root, since) {
                    walk_jsonl(&day, 0, &mut |f| modified_since(f, since), &mut files);
                }
            }
        }
        let mut best: Option<(SystemTime, PathBuf)> = None;
        for f in files {
            if taken.contains(&f) {
                continue;
            }
            if resume.is_none() {
                let (_, file_cwd) = self.codex_session_meta(&f);
                if file_cwd != p.cwd {
                    continue;
                }
            }
            let m = std::fs::metadata(&f).and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH);
            if best.as_ref().is_none_or(|(bm, _)| m > *bm) {
                best = Some((m, f));
            }
        }
        let f = best.map(|(_, f)| f)?;
        self.codex_files.insert(key, f.clone());
        Some(f)
    }

    fn codex_session_meta(&mut self, f: &Path) -> (String, String) {
        if let Some(m) = self.codex_meta.get(f) {
            return m.clone();
        }
        let first = head_line(f).unwrap_or_default();
        let (meta, _) = parse_codex(std::slice::from_ref(&first));
        let m = (meta.id, meta.cwd);
        // Only remember a complete header: a brand-new file may not have one yet.
        if !m.1.is_empty() {
            self.codex_meta.insert(f.to_owned(), m.clone());
        }
        m
    }
}

/// Claude Code's project directory name for a working directory.
fn claude_dir_name(cwd: &str) -> String {
    cwd.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect()
}

fn claude_records(dir: &Path) -> Vec<ClaudeRecord> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    rd.flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| serde_json::from_slice::<ClaudeRecord>(&std::fs::read(e.path()).ok()?).ok())
        .collect()
}

fn args_of(p: &Process) -> Vec<String> {
    p.cmd().iter().map(|a| a.to_string_lossy().into_owned()).collect()
}

/// Which tool a process is, if any: by its name, or by the script a Node or Bun process runs.
fn tool_of(p: &Process) -> Option<Tool> {
    let by_name = |n: &str| match n {
        "claude" => Some(Tool::Claude),
        "codex" => Some(Tool::Codex),
        "gemini" => Some(Tool::Gemini),
        _ => None,
    };
    let name = crate::os::program_stem(&p.name().to_string_lossy());
    if let Some(t) = by_name(&name) {
        return Some(t);
    }
    if let Some(t) = p.exe().and_then(|e| by_name(&crate::os::program_stem(&e.to_string_lossy()))) {
        return Some(t);
    }
    if name == "node" || name == "bun" {
        let script = p.cmd().get(1).map(|a| a.to_string_lossy().into_owned()).unwrap_or_default();
        if script.contains("claude-code") {
            return Some(Tool::Claude);
        }
        if script.contains("@openai/codex") {
            return Some(Tool::Codex);
        }
        if script.contains("gemini-cli") {
            return Some(Tool::Gemini);
        }
        return by_name(&crate::os::program_stem(&script));
    }
    None
}

/// An interactive session rather than a one-shot or helper run of the tool.
fn interactive(tool: Tool, args: &[String]) -> bool {
    // A Node-hosted CLI's own arguments follow its script.
    let skip = if matches!(crate::os::program_stem(args.first().map_or("", String::as_str)).as_str(), "node" | "bun") { 2 } else { 1 };
    let rest: Vec<&str> = args.iter().skip(skip).map(String::as_str).collect();
    match tool {
        Tool::Claude => !rest.iter().any(|a| matches!(*a, "-p" | "--print")),
        Tool::Codex => rest.first().is_none_or(|a| !CODEX_BATCH.contains(a)),
        Tool::Gemini => !rest.iter().any(|a| matches!(*a, "-p" | "--prompt")),
    }
}

/// The kiln terminal a process runs in: the nearest ancestor that is a terminal's shell.
fn terminal_of(sys: &System, pid: Pid, terminals: &HashMap<u32, String>) -> Option<String> {
    if terminals.is_empty() {
        return None;
    }
    let mut cur = pid;
    for _ in 0..MAX_DEPTH {
        if let Some(id) = terminals.get(&cur.as_u32()) {
            return Some(id.clone());
        }
        cur = sys.process(cur)?.parent()?;
    }
    None
}

/// The newest `*.jsonl` directly in `dir` modified at or after `since` (unix seconds).
fn newest_since(dir: &Path, since: u64, taken: &HashSet<PathBuf>) -> Option<PathBuf> {
    let mut files = Vec::new();
    walk_jsonl(dir, 0, &mut |f| modified_since(f, since), &mut files);
    files
        .into_iter()
        .filter(|f| !taken.contains(f))
        .filter_map(|f| Some((std::fs::metadata(&f).ok()?.modified().ok()?, f)))
        .max_by_key(|(m, _)| *m)
        .map(|(_, f)| f)
}

fn modified_since(f: &Path, since: u64) -> bool {
    std::fs::metadata(f).and_then(|m| m.modified()).is_ok_and(|m| u64::try_from(unix_secs(m)).unwrap_or(0) >= since)
}

/// `*.jsonl` files under `dir`, `depth` directories down, that pass `keep`.
fn walk_jsonl(dir: &Path, depth: u32, keep: &mut dyn FnMut(&Path) -> bool, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_dir() && depth > 0 {
            walk_jsonl(&p, depth - 1, keep, out);
        } else if ft.is_file() && p.extension().is_some_and(|x| x == "jsonl") && keep(&p) {
            out.push(p);
        }
    }
}

/// Codex's `YYYY/MM/DD` directories from two days before `since` (unix seconds) on, which covers
/// local dates on either side of UTC.
fn recent_days(root: &Path, since: u64) -> Vec<PathBuf> {
    let from = time::OffsetDateTime::from_unix_timestamp(i64::try_from(since).unwrap_or(0) - 2 * 86_400)
        .map(|t| t.date())
        .unwrap_or(time::Date::MIN);
    let mut days = Vec::new();
    let numbered = |dir: &Path| -> Vec<(u32, PathBuf)> {
        std::fs::read_dir(dir)
            .map(|rd| rd.flatten().filter_map(|e| Some((e.file_name().to_str()?.parse::<u32>().ok()?, e.path()))).collect())
            .unwrap_or_default()
    };
    for (y, ydir) in numbered(root) {
        for (m, mdir) in numbered(&ydir) {
            for (d, ddir) in numbered(&mdir) {
                let Ok(month) = time::Month::try_from(u8::try_from(m).unwrap_or(0)) else { continue };
                let Ok(date) = time::Date::from_calendar_date(i32::try_from(y).unwrap_or(0), month, u8::try_from(d).unwrap_or(0)) else {
                    continue;
                };
                if date >= from {
                    days.push(ddir);
                }
            }
        }
    }
    days
}

fn unix_secs(t: SystemTime) -> i64 {
    t.duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0))
}

fn clip(s: &str, max: usize) -> String {
    let s = s.trim();
    match s.char_indices().nth(max) {
        None => s.to_owned(),
        Some((i, _)) => format!("{}…", &s[..i]),
    }
}

fn first_line(s: &str, max: usize) -> String {
    clip(s.lines().find(|l| !l.trim().is_empty()).unwrap_or(""), max)
}

/// The last `max` bytes of a file as whole lines (a line cut at the start is dropped).
fn tail_lines(path: &Path, max: u64) -> Option<Vec<String>> {
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let start = len.saturating_sub(max);
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::new();
    f.take(max).read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf);
    let mut lines = text.lines();
    if start > 0 {
        lines.next();
    }
    Some(lines.filter(|l| !l.trim().is_empty()).map(str::to_owned).collect())
}

/// The first line of a file (at most 64 KiB of it).
fn head_line(path: &Path) -> Option<String> {
    let f = std::fs::File::open(path).ok()?;
    let mut buf = Vec::new();
    f.take(64 * 1024).read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf);
    text.lines().next().map(str::to_owned)
}

fn str_at<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

/// The text parts of a message's content: a plain string, or typed parts.
fn text_of(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter(|p| matches!(str_at(p, "type"), Some("text" | "input_text" | "output_text")))
            .filter_map(|p| str_at(p, "text"))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// One line for a tool call: its name and the argument that says what it does.
fn tool_line(name: &str, input: &Value) -> String {
    const KEYS: [&str; 9] = ["command", "cmd", "file_path", "path", "pattern", "url", "query", "description", "prompt"];
    let parsed;
    // Codex passes arguments as a JSON string.
    let input = match input {
        Value::String(s) => {
            parsed = serde_json::from_str::<Value>(s).unwrap_or(Value::Null);
            &parsed
        }
        v => v,
    };
    let what = KEYS
        .iter()
        .find_map(|k| match input.get(k)? {
            Value::String(s) => Some(s.clone()),
            Value::Array(a) => Some(a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" ")),
            _ => None,
        })
        .unwrap_or_default();
    let what = first_line(&what, 200);
    if what.is_empty() { name.to_owned() } else { format!("{name}: {what}") }
}

fn parse(tool: Tool, lines: &[String]) -> (Meta, Vec<Entry>) {
    match tool {
        Tool::Claude => parse_claude(lines),
        Tool::Codex => parse_codex(lines),
        Tool::Gemini => (Meta::default(), Vec::new()),
    }
}

fn entry(role: &str, text: &str, at: &str) -> Entry {
    Entry { role: role.to_owned(), text: clip(text, ENTRY_CHARS), at: at.to_owned() }
}

/// Prompts, replies and tool calls of the main conversation; subagents' records are skipped.
fn parse_claude(lines: &[String]) -> (Meta, Vec<Entry>) {
    let mut meta = Meta::default();
    let mut entries = Vec::new();
    for line in lines {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        let kind = str_at(&v, "type").unwrap_or("");
        match kind {
            "ai-title" => {
                if let Some(t) = str_at(&v, "aiTitle") {
                    meta.title = t.to_owned();
                }
                continue;
            }
            "last-prompt" => {
                if let Some(t) = str_at(&v, "lastPrompt") {
                    meta.last_prompt = t.to_owned();
                }
                continue;
            }
            "user" | "assistant" => {}
            _ => continue,
        }
        if v.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        if let Some(c) = str_at(&v, "cwd") {
            meta.cwd = c.to_owned();
        }
        // "HEAD" outside a repository or on a detached checkout: no branch to show.
        if let Some(b) = str_at(&v, "gitBranch") {
            meta.branch = if b == "HEAD" { String::new() } else { b.to_owned() };
        }
        if let Some(id) = str_at(&v, "sessionId") {
            meta.id = id.to_owned();
        }
        let at = str_at(&v, "timestamp").unwrap_or("");
        let Some(msg) = v.get("message") else { continue };
        let content = msg.get("content").unwrap_or(&Value::Null);
        if kind == "user" {
            let flag = |k: &str| v.get(k).and_then(Value::as_bool) == Some(true);
            if flag("isMeta") || flag("isCompactSummary") || v.get("toolUseResult").is_some() {
                continue;
            }
            let t = text_of(content);
            // Tool results are user records too, and commands and reminders come wrapped in tags.
            if !t.trim().is_empty() && !t.trim_start().starts_with('<') {
                entries.push(entry("user", &t, at));
            }
            continue;
        }
        if let Some(m) = str_at(msg, "model")
            && !m.starts_with('<')
        {
            meta.model = m.to_owned();
        }
        match content {
            Value::String(t) if !t.trim().is_empty() => entries.push(entry("assistant", t, at)),
            Value::Array(parts) => {
                for part in parts {
                    match str_at(part, "type") {
                        Some("text") => {
                            let t = str_at(part, "text").unwrap_or("");
                            if !t.trim().is_empty() {
                                entries.push(entry("assistant", t, at));
                            }
                        }
                        Some("tool_use") => {
                            let line = tool_line(str_at(part, "name").unwrap_or("tool"), part.get("input").unwrap_or(&Value::Null));
                            entries.push(entry("tool", &line, at));
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    (meta, entries)
}

/// Codex rollouts: `{"type": ..., "payload": ...}` records (older ones are bare items).
fn parse_codex(lines: &[String]) -> (Meta, Vec<Entry>) {
    let mut meta = Meta::default();
    let mut entries = Vec::new();
    for line in lines {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        let at = str_at(&v, "timestamp").unwrap_or("");
        let kind = str_at(&v, "type").unwrap_or("");
        let p = v.get("payload").unwrap_or(&v);
        if kind == "session_meta" || (kind.is_empty() && v.get("instructions").is_some()) {
            if let Some(id) = str_at(p, "id") {
                meta.id = id.to_owned();
            }
            if let Some(c) = str_at(p, "cwd") {
                meta.cwd = c.to_owned();
            }
            if let Some(b) = p.get("git").and_then(|g| str_at(g, "branch")) {
                meta.branch = b.to_owned();
            }
            continue;
        }
        if kind == "turn_context" {
            if let Some(c) = str_at(p, "cwd") {
                meta.cwd = c.to_owned();
            }
            if let Some(m) = str_at(p, "model") {
                meta.model = m.to_owned();
            }
            continue;
        }
        match str_at(p, "type") {
            Some("message") => {
                let t = text_of(p.get("content").unwrap_or(&Value::Null));
                match str_at(p, "role") {
                    // The harness's own context (environment, instructions) comes as tagged user text.
                    Some("user") if !t.trim().is_empty() && !t.trim_start().starts_with('<') => entries.push(entry("user", &t, at)),
                    Some("assistant") if !t.trim().is_empty() => entries.push(entry("assistant", &t, at)),
                    _ => {}
                }
            }
            Some("function_call" | "custom_tool_call") => {
                let input = p.get("arguments").or_else(|| p.get("input")).unwrap_or(&Value::Null);
                entries.push(entry("tool", &tool_line(str_at(p, "name").unwrap_or("tool"), input), at));
            }
            Some("local_shell_call") => {
                let input = p.get("action").unwrap_or(&Value::Null);
                entries.push(entry("tool", &tool_line("shell", input), at));
            }
            _ => {}
        }
    }
    (meta, entries)
}

fn summarize(meta: Meta, entries: &[Entry]) -> Summary {
    let last = |role: &str| entries.iter().rev().find(|e| e.role == role).map(|e| e.text.clone()).unwrap_or_default();
    let last_prompt = if meta.last_prompt.is_empty() { last("user") } else { meta.last_prompt.clone() };
    Summary {
        last_prompt,
        last_reply: last("assistant"),
        turn_ended: entries.last().is_some_and(|e| e.role == "assistant"),
        meta,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(s: &str) -> Vec<String> {
        s.lines().map(str::to_owned).collect()
    }

    #[test]
    fn claude_transcript() {
        let (meta, entries) = parse_claude(&lines(concat!(
            r#"{"type":"ai-title","aiTitle":"Fix flaky test","sessionId":"abc"}"#, "\n",
            r#"{"type":"user","cwd":"/work/repo","gitBranch":"main","sessionId":"abc","timestamp":"2026-10-16T10:00:00Z","message":{"role":"user","content":"make the fleet test stable"}}"#, "\n",
            r#"{"type":"assistant","message":{"model":"claude-opus-5-5","content":[{"type":"thinking","thinking":"hmm"},{"type":"text","text":"Looking at it."},{"type":"tool_use","name":"Bash","input":{"command":"cargo test fleet\nmore"}}]}}"#, "\n",
            r#"{"type":"user","toolUseResult":{"stdout":"ok"},"message":{"role":"user","content":[{"type":"tool_result","content":"ok"}]}}"#, "\n",
            r#"{"type":"user","isSidechain":true,"message":{"role":"user","content":"subagent prompt"}}"#, "\n",
            r#"{"type":"user","message":{"role":"user","content":"<command-name>/clear</command-name>"}}"#, "\n",
            r#"{"type":"assistant","message":{"model":"<synthetic>","content":[{"type":"text","text":"Done: raised the budget."}]}}"#, "\n",
            r#"{"type":"last-prompt","lastPrompt":"make the fleet test stable","sessionId":"abc"}"#, "\n",
            "not json\n",
        )));
        assert_eq!((meta.title.as_str(), meta.cwd.as_str(), meta.branch.as_str(), meta.model.as_str()), ("Fix flaky test", "/work/repo", "main", "claude-opus-5-5"));
        let roles: Vec<&str> = entries.iter().map(|e| e.role.as_str()).collect();
        assert_eq!(roles, ["user", "assistant", "tool", "assistant"]);
        assert_eq!(entries.get(2).map(|e| e.text.as_str()), Some("Bash: cargo test fleet"));
        let s = summarize(meta, &entries);
        assert_eq!(s.last_prompt, "make the fleet test stable");
        assert_eq!(s.last_reply, "Done: raised the budget.");
        assert!(s.turn_ended);
    }

    #[test]
    fn codex_rollout() {
        let (meta, entries) = parse_codex(&lines(concat!(
            r#"{"timestamp":"t0","type":"session_meta","payload":{"id":"cx-9","cwd":"/work/other","git":{"branch":"dev"}}}"#, "\n",
            r#"{"timestamp":"t1","type":"turn_context","payload":{"cwd":"/work/other","model":"gpt-5-codex"}}"#, "\n",
            r#"{"timestamp":"t2","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context>x</environment_context>"}]}}"#, "\n",
            r#"{"timestamp":"t3","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"add a test"}]}}"#, "\n",
            r#"{"timestamp":"t4","type":"response_item","payload":{"type":"function_call","name":"shell","arguments":"{\"command\":[\"bash\",\"-lc\",\"cargo test\"]}"}}"#, "\n",
        )));
        assert_eq!((meta.id.as_str(), meta.cwd.as_str(), meta.branch.as_str(), meta.model.as_str()), ("cx-9", "/work/other", "dev", "gpt-5-codex"));
        assert_eq!(entries.len(), 2);
        assert_eq!(entries.get(1).map(|e| e.text.as_str()), Some("shell: bash -lc cargo test"));
        let s = summarize(meta, &entries);
        assert_eq!(s.last_prompt, "add a test");
        assert!(!s.turn_ended);
    }

    #[test]
    fn helpers() {
        assert_eq!(claude_dir_name("/home/ada/My Projects/kiln.rs"), "-home-ada-My-Projects-kiln-rs");
        assert_eq!(clip("  héllo wörld ", 5), "héllo…");
        assert_eq!(first_line("\n\nfirst\nsecond", 80), "first");
        let dir = tempfile::tempdir().ok();
        let Some(dir) = dir else { return };
        let day = dir.path().join("2026").join("10").join("16");
        std::fs::create_dir_all(&day).ok();
        std::fs::create_dir_all(dir.path().join("2020").join("01").join("01")).ok();
        let since = u64::try_from(time::macros::datetime!(2026-10-17 01:00 UTC).unix_timestamp()).unwrap_or(0);
        assert_eq!(recent_days(dir.path(), since), vec![day]);
    }

    #[test]
    fn live_scan_runs() {
        // The scan itself must never fail, whatever this machine is running.
        let dir = tempfile::tempdir().ok();
        let Some(dir) = dir else { return };
        let mut s = Scanner::new(dir.path().to_owned());
        let found = s.scan(&HashMap::new());
        assert!(found.len() <= MAX_SESSIONS);
        assert!(s.transcript(u32::MAX).is_none());
    }
}
