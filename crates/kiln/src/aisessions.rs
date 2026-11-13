//! Live AI coding-agent sessions on this machine: the Claude Code, Codex, Devin and Gemini CLI processes
//! running right now, summarized from the transcripts those tools write.
//!
//! - Claude Code keeps a `~/.claude/sessions/<pid>.json` record per running session (its id,
//!   directory, name, busy or idle); the transcript is `~/.claude/projects/<dir>/<id>.jsonl`.
//! - Codex writes `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`, matched to its process by
//!   directory and start time.
//! - Gemini CLI is found by its process alone.
//! - Devin keeps sessions and their main message chains in `devin/cli/sessions.db`; a held
//!   `session_locks/<id>.lock` identifies the process that owns a conversation.
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

use crate::harness::isolated::{Refusal, home};
use crate::harness::session::Isolation;

mod devin;
#[cfg(all(test, unix))]
mod live_tests;

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
/// Most past sessions one listing returns.
const HISTORY_MAX: usize = 200;
/// Bytes read from the start and the end of a past session's transcript.
const HISTORY_HEAD: u64 = 32 * 1024;
const HISTORY_TAIL: u64 = 64 * 1024;
/// Past sessions remembered between listings (by transcript, with its length and mtime).
const HISTORY_CACHE: usize = 4096;
/// How far back from the end the last reply (and so the model) is looked for, when the tail
/// holds none (attachments can be large).
const HISTORY_MODEL_TAIL: u64 = 2 * 1024 * 1024;
/// Codex subcommands that are not an interactive session.
const CODEX_BATCH: &[&str] = &[
    "exec", "e", "app-server", "mcp", "mcp-server", "login", "logout", "completion", "debug", "apply", "a", "proto", "cloud",
    "sandbox", "help", "features", "generate-ts", "responses-api-proxy", "stdio-to-uds", "-h", "--help", "-V", "--version",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Tool {
    Claude,
    Codex,
    Devin,
    Gemini,
}

impl Tool {
    fn name(self) -> &'static str {
        match self {
            Tool::Claude => "claude",
            Tool::Codex => "codex",
            Tool::Devin => "devin",
            Tool::Gemini => "gemini",
        }
    }
}

/// A conversation on a machine that is not running: one to continue (`Spec::resume = id`; the
/// harness keeps its model). Every field has a default, as for [`AiSession`].
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct PastSession {
    /// The harness (`"claude"`, `"codex"`, `"devin"`).
    pub kind: String,
    /// The harness's own conversation id.
    pub id: String,
    /// Its name (Claude Code's custom or generated title, Codex's thread name), else its last
    /// prompt's first line.
    pub title: String,
    pub cwd: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub branch: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub model: String,
    /// Unix seconds of the last write to its transcript.
    pub last_activity: i64,
    /// The last prompt (clipped).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub last_prompt: String,
    /// How it ran when not as a process on this machine: in a microVM of its person's (`microvm`,
    /// its `person`), its conversation kept in their volume here (kiln plan
    /// 01a1d2c16eb074f19cf7ae7f1025c0be step 5). It goes on in one again (`Spec::isolation`), never as
    /// a process. None: one of the machine's own.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub isolation: Option<Isolation>,
}

/// A machine's past conversations as the server asks for them (`ServerMsg::HarnessHistoryFor`, its
/// JSON): one harness's or all, at most `limit`, with those in `person`'s microVM volumes there (kiln
/// plan 01a1d2c16eb074f19cf7ae7f1025c0be step 5). Every field has a default, so either side can add one.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Asked {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    pub limit: u32,
    /// Whom it's asked for (`owner`, `user:<id>`): kiln's server says, from who asked it, never from
    /// what was asked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub person: Option<String>,
}

/// One live session, as runners report it and the API lists it. Every field has a default so
/// either side can add fields without breaking the other.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct AiSession {
    /// "claude", "codex", "devin" or "gemini".
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
    /// "working" (mid-turn), "waiting" (the turn ended: waiting for the user), "starting" (not a
    /// session yet, e.g. at a first-run prompt), or empty (unknown).
    pub state: String,
    pub last_prompt: String,
    pub last_reply: String,
    /// The kiln terminal session it runs in: attach to drive it.
    pub terminal: Option<String>,
    /// A transcript can be opened.
    pub transcript: bool,
}

/// Whether a found session can be taken over: continued as a kiln agent session (its kiln
/// terminal is ended, or its process when it runs outside kiln, and the harness resumes it in the
/// same folder).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TakeOver {
    Ready,
    /// It runs outside kiln: taking it over ends it there first (kiln quits its process).
    Stop,
    /// It runs outside kiln, as a harness kiln can't be sure to resume (an ACP agent may only start
    /// anew): quit it there first (an ended session is resumed with `spec.resume`).
    Outside,
    /// It has no session id to continue yet (still starting).
    NoId,
    /// kiln cannot run that harness as an agent session.
    Unsupported,
    /// The machine's runner predates this take-over (it would not wait for the session to end, or
    /// could not end one running outside kiln): upgrade it first.
    Upgrade,
}

impl AiSession {
    /// Whether it can be taken over; `held`: kiln holds the terminal it runs in.
    pub fn takeover(&self, held: bool) -> TakeOver {
        let Some(kind) = crate::harness::kinds::Kind::get(&self.tool).filter(|k| k.structured()) else {
            return TakeOver::Unsupported;
        };
        if !self.started() {
            TakeOver::NoId
        } else if held && self.terminal.is_some() {
            TakeOver::Ready
        } else if native(kind) || kind.id == "devin" {
            TakeOver::Stop
        } else {
            TakeOver::Outside
        }
    }

    /// The agent session that continues it: the same harness, folder, conversation and model,
    /// started once its process (ended with its terminal, or by kiln when it runs outside kiln) is
    /// gone. `None` unless it is `Ready` or `Stop`.
    pub fn takeover_spec(&self, held: bool) -> Option<crate::harness::session::Spec> {
        let t = self.takeover(held);
        matches!(t, TakeOver::Ready | TakeOver::Stop).then(|| crate::harness::session::Spec {
            takeover_pid: Some(self.pid),
            takeover_stop: t == TakeOver::Stop,
            ..self.continued()
        })
    }

    /// It has a conversation to continue: an id, a folder, and a transcript with a first message.
    /// A brand-new session reports its id before its transcript holds anything to resume (taking it
    /// over or branching it then would wait on a transcript that isn't there).
    pub fn started(&self) -> bool {
        !self.id.trim().is_empty() && !self.cwd.trim().is_empty() && self.transcript && !(self.last_prompt.trim().is_empty() && self.last_reply.trim().is_empty())
    }

    /// Whether a copy of it can be continued as an agent session, leaving it running as it is.
    pub fn forkable(&self) -> bool {
        crate::harness::kinds::Kind::get(&self.tool).is_some_and(native) && self.started()
    }

    /// The agent session that continues a copy of it (a branch of its conversation), leaving it
    /// running as it is. `None` unless it is `forkable`.
    pub fn fork_spec(&self) -> Option<crate::harness::session::Spec> {
        self.forkable().then(|| {
            let s = self.continued();
            let name = format!("{} (branch)", s.name.as_deref().unwrap_or(&self.tool));
            crate::harness::session::Spec { fork: true, name: Some(name), ..s }
        })
    }

    /// Its harness, folder, conversation, model and name, as an agent session's spec.
    fn continued(&self) -> crate::harness::session::Spec {
        crate::harness::session::Spec {
            kind: self.tool.clone(),
            cwd: Some(self.cwd.clone()),
            resume: Some(self.id.clone()),
            model: Some(self.model.clone()).filter(|m| !m.is_empty()),
            name: [&self.name, &self.title].into_iter().find(|n| !n.trim().is_empty()).map(|n| n.chars().take(80).collect()),
            ..crate::harness::session::Spec::default()
        }
    }
}

/// A harness that resumes and branches a session itself (Claude Code, Codex); an ACP agent may only
/// be able to start a new one.
fn native(kind: &crate::harness::kinds::Kind) -> bool {
    use crate::harness::kinds::Protocol;
    matches!(kind.protocol, Protocol::ClaudeStreamJson | Protocol::CodexAppServer)
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

/// A runner's side: scans on a blocking thread, one at a time, and reports only changes. Once
/// told its terminals ([`Reporter::terminals`]) it scans as what it reads changes, never on a
/// clock (goal 9; it scanned at every sample).
#[derive(Clone)]
pub struct Reporter {
    scanner: Arc<Mutex<Scanner>>,
    /// The last report sent (JSON), to skip unchanged ones.
    last: Arc<Mutex<Option<String>>>,
    scanning: Arc<AtomicBool>,
    /// What the scans run with: the shells of kiln's terminals, as the runner last said them.
    shells: Arc<Mutex<HashMap<u32, String>>>,
    /// Told when a scan is wanted besides the folders' changes: the terminals changed, or a new
    /// connection wants a report.
    asked: Arc<crate::rt::Notify>,
    /// The scans its events made (tests count an idle one's).
    scans: Arc<std::sync::atomic::AtomicU64>,
    watching: Arc<AtomicBool>,
    /// Whether its sessions' processes' ends scan again (a test that counts its scans over a
    /// quiet span leaves out the machine's own tools ending).
    exits: bool,
}

impl Reporter {
    pub fn new(home: PathBuf) -> Self {
        Self {
            scanner: Arc::new(Mutex::new(Scanner::new(home))),
            last: Arc::new(Mutex::new(None)),
            scanning: Arc::new(AtomicBool::new(false)),
            shells: Arc::new(Mutex::new(HashMap::new())),
            asked: Arc::new(crate::rt::Notify::new()),
            scans: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            watching: Arc::new(AtomicBool::new(false)),
            exits: true,
        }
    }

    /// Report on the next scan even when nothing changed (a new server connection), and scan now.
    pub fn reset(&self) {
        *self.last.lock() = None;
        self.asked.notify();
    }

    /// The shells of this machine's kiln terminals now (each shell's pid, its session): scanned
    /// with from now on, and a scan now when they changed. The first call starts the scans on
    /// events (`up` gets their reports; [`Reporter::watch`]).
    pub fn terminals(&self, shells: HashMap<u32, String>, up: crate::memory::queue::Sender<crate::exec::Up>) {
        let changed = {
            let mut g = self.shells.lock();
            let changed = *g != shells;
            *g = shells;
            changed
        };
        if changed {
            self.asked.notify();
        }
        self.watch(up);
    }

    /// Scans as what it reads changes, once (goal 9): its tools' own folders (Claude Code's
    /// session records and transcripts, Codex's sessions, Devin's database and locks, Gemini CLI's
    /// folder), the terminals the runner tells it, a new connection ([`Reporter::reset`]), and a
    /// session's process ending (its pid, as the OS tells it); and once as it starts. A tool's
    /// process that has written nothing yet (Gemini CLI before its first prompt, any tool before
    /// its first file) is seen at the next scan one of these brings, not before.
    fn watch(&self, up: crate::memory::queue::Sender<crate::exec::Up>) {
        if self.watching.swap(true, Ordering::AcqRel) {
            return;
        }
        let me = self.clone();
        drop(crate::rt::spawn(async move {
            use futures::FutureExt as _;
            let folders = me.scanner.lock().folders();
            let files = crate::os::watch::Watched::new(&folders, true, &[]);
            loop {
                let Some(found) = me.scan_now(&up).await else { return };
                let pids = if me.exits { found } else { Vec::new() };
                futures::select_biased! {
                    () = me.asked.notified().fuse() => {}
                    () = files.changed().fuse() => {}
                    () = crate::os::watch::exited(&pids).fuse() => {}
                }
            }
        }));
    }

    /// One scan, its report sent when it changed: the sessions' processes, to watch for their end.
    /// None once the uplink is gone.
    async fn scan_now(&self, up: &crate::memory::queue::Sender<crate::exec::Up>) -> Option<Vec<u32>> {
        self.scans.fetch_add(1, Ordering::Relaxed);
        let (scanner, terminals) = (Arc::clone(&self.scanner), self.shells.lock().clone());
        // The processes read before the scanner is taken: a history or a view meanwhile doesn't wait on that read.
        let sessions = crate::rt::blocking(move || { let sys = Scanner::processes(); scanner.lock().scan_with(sys, &terminals) }).await.ok().unwrap_or_default();
        let pids: Vec<u32> = sessions.iter().map(|s| s.pid).filter(|p| *p != 0).collect();
        let Ok(json) = serde_json::to_string(&sessions) else { return Some(pids) };
        {
            let mut last = self.last.lock();
            if last.as_deref() == Some(json.as_str()) {
                return Some(pids);
            }
            *last = Some(json.clone());
        }
        // Never queued across a disconnect: a reconnect sends a fresh report anyway.
        up.send(crate::exec::Up::Term(crate::proto::AgentMsg::AiSessions { json })).await.ok().map(|()| pids)
    }

    /// The scans its events made so far.
    pub fn scans(&self) -> u64 {
        self.scans.load(Ordering::Relaxed)
    }

    /// Scan in the background and send the sessions when they changed. `terminals` maps each
    /// kiln terminal's shell pid to its session id.
    pub fn scan(&self, terminals: HashMap<u32, String>, up: crate::memory::queue::Sender<crate::exec::Up>) {
        if self.scanning.swap(true, Ordering::AcqRel) {
            return;
        }
        let me = self.clone();
        crate::rt::spawn(async move {
            let scanner = Arc::clone(&me.scanner);
            let sessions = crate::rt::blocking(move || { let sys = Scanner::processes(); scanner.lock().scan_with(sys, &terminals) }).await;
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

    /// Answer the server's request for a session's whole view (timeline, state, changeset).
    pub fn view(&self, req: u64, pid: u32, up: crate::memory::queue::Sender<crate::exec::Up>) {
        let scanner = Arc::clone(&self.scanner);
        crate::rt::spawn(async move {
            let json = crate::rt::blocking(move || scanner.lock().view(pid)).await.ok().flatten().map(|v| v.to_string());
            let _ = up.send(crate::exec::Up::Term(crate::proto::AgentMsg::AiView { req, json })).await;
        });
    }

    /// Reads this machine's people's microVM volumes under `root` too, from now on
    /// ([`Scanner::with_homes`]): where its harness host keeps them (`harness::host::Host::homes`).
    pub fn read_homes(&self, root: PathBuf) {
        self.scanner.lock().homes = Some(root);
    }

    /// Answer the server's request for this machine's past sessions (`running`: the harness ids
    /// of kiln's own sessions here), with those in `person`'s microVM volumes where it names one.
    pub fn history(&self, req: u64, kind: Option<String>, limit: usize, running: HashSet<String>, person: Option<String>, up: crate::memory::queue::Sender<crate::exec::Up>) {
        let scanner = Arc::clone(&self.scanner);
        crate::rt::spawn(async move {
            let past = crate::rt::blocking(move || scanner.lock().history(kind.as_deref(), limit, &running, person.as_deref())).await.ok();
            let json = past.and_then(|p| serde_json::to_string(&p).ok());
            // On the control lane: an answer the server waits on with a deadline, not behind what the sessions' streams
            // carry on the terminal lane (2026-11-12: their snapshots, a megabyte a second, went ahead of it).
            let _ = up.send(crate::exec::Up::Msg(crate::proto::AgentMsg::HarnessHistory { req, json })).await;
        });
    }

    /// Answer the server's request for a session's recent messages.
    pub fn transcript(&self, req: u64, pid: u32, up: crate::memory::queue::Sender<crate::exec::Up>) {
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

/// A Codex rollout's `session_meta`: its folder, when its thread began (unix seconds) and what
/// started it ("codex-tui", "kiln", ...).
#[derive(Clone, Default)]
struct CodexHead {
    cwd: String,
    began: i64,
    originator: String,
}

/// Finds the sessions. Keeps parsed transcripts between scans, so a scan only rereads transcripts
/// that changed.
pub struct Scanner {
    /// Claude Code's configuration directory (`CLAUDE_CONFIG_DIR`, else `~/.claude`) and Codex's
    /// (`CODEX_HOME`, else `~/.codex`), as the sessions this machine starts use them.
    claude: PathBuf,
    codex: PathBuf,
    devin: PathBuf,
    /// Gemini CLI's own folder (`~/.gemini`): it's found by its process, and this is where it
    /// writes as it works.
    gemini: PathBuf,
    sys: System,
    /// Summaries by transcript, with the length and mtime they were read at.
    summaries: HashMap<PathBuf, (u64, SystemTime, Summary)>,
    /// Claude Code transcripts by session id, once found.
    claude_files: HashMap<String, PathBuf>,
    /// Codex transcripts by process (pid, start time), once found.
    codex_files: HashMap<(u32, u64), PathBuf>,
    /// The `session_meta` of Codex transcripts, read once per file.
    codex_meta: HashMap<PathBuf, CodexHead>,
    /// Each reported session's transcript by pid, for [`Scanner::transcript`].
    open: HashMap<u32, (Tool, PathBuf)>,
    /// Devin's database-backed conversations, by the interactive process that owns them.
    devin_open: HashMap<u32, String>,
    devin_running: HashSet<String>,
    /// Past sessions by transcript, with the length and mtime they were read at (`None`: not a
    /// conversation, e.g. nothing was ever asked).
    past: HashMap<PathBuf, (u64, SystemTime, Option<PastSession>)>,
    /// The ids of the sessions the latest scan found running (None before the first): what a
    /// history leaves out, read from the scans the tools' own changes make, not a scan of its own.
    running: Option<HashSet<String>>,
    /// Where this machine keeps its people's microVM volumes (`harness::host::Host::homes`), whose
    /// conversations a history asked for a person lists too; none where it keeps none.
    homes: Option<PathBuf>,
}

impl Scanner {
    pub fn new(home: PathBuf) -> Self {
        let dir = |var: &str, default: &str| std::env::var_os(var).filter(|v| !v.is_empty()).map_or_else(|| home.join(default), PathBuf::from);
        Self {
            claude: dir("CLAUDE_CONFIG_DIR", ".claude"),
            codex: dir("CODEX_HOME", ".codex"),
            devin: if cfg!(windows) { dir("LOCALAPPDATA", "AppData/Local") } else { dir("XDG_DATA_HOME", ".local/share") }.join("devin/cli"),
            gemini: home.join(".gemini"),
            sys: System::new(),
            summaries: HashMap::new(),
            claude_files: HashMap::new(),
            codex_files: HashMap::new(),
            codex_meta: HashMap::new(),
            open: HashMap::new(),
            devin_open: HashMap::new(),
            devin_running: HashSet::new(),
            past: HashMap::new(),
            running: None,
            homes: None,
        }
    }

    /// Reads the conversations in this machine's people's microVM volumes under `root`
    /// (`harness::host::Host::homes`) too, each for its own person.
    #[must_use]
    pub fn with_homes(mut self, root: PathBuf) -> Self {
        self.homes = Some(root);
        self
    }

    /// The folders the scans read, as the tools write them: Claude Code's session records and
    /// transcripts, Codex's sessions, Devin's database and session locks, and Gemini CLI's own
    /// folder (its chats and logs, `~/.gemini`).
    pub fn folders(&self) -> Vec<PathBuf> {
        vec![self.claude.join("sessions"), self.claude.join("projects"), self.codex.join("sessions"), self.devin.clone(), self.gemini.clone()]
    }

    /// The machine's processes as a scan reads them: names, parents and command lines of
    /// everything; working directories and environments of the tools alone. Read without the
    /// scanner (`Reporter::scan_now`): on a machine of thousands of processes that read is the
    /// scan's time (2026-11-12, adacachy: 6,474 processes, ~80,000 reads, 0.8 s warm, 17.9 s
    /// under load), and what else takes the scanner (a history, a view) didn't wait on it.
    pub fn processes() -> System {
        // sysinfo keeps names across exec even when the PID and start time stay the same: new.
        let mut sys = System::new();
        sys.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing().with_cmd(UpdateKind::OnlyIfNotSet).with_exe(UpdateKind::OnlyIfNotSet).without_tasks(),
        );
        let tools: Vec<Pid> = sys.processes().iter().filter(|(_, p)| tool_of(p).is_some()).map(|(pid, _)| *pid).collect();
        if !tools.is_empty() {
            sys.refresh_processes_specifics(
                ProcessesToUpdate::Some(&tools),
                false,
                ProcessRefreshKind::nothing().with_cwd(UpdateKind::Always).with_environ(UpdateKind::OnlyIfNotSet).without_tasks(),
            );
        }
        sys
    }

    /// The sessions running now, most recently active first. `terminals` maps the shell pid of
    /// each kiln terminal on this machine to its session id.
    pub fn scan(&mut self, terminals: &HashMap<u32, String>) -> Vec<AiSession> {
        self.scan_with(Self::processes(), terminals)
    }

    /// [`Scanner::scan`] of the processes `sys` read ([`Scanner::processes`]).
    pub fn scan_with(&mut self, sys: System, terminals: &HashMap<u32, String>) -> Vec<AiSession> {
        self.sys = sys;
        let mut out = Vec::new();
        let mut open = HashMap::new();
        let mut seen: HashSet<u32> = HashSet::new();
        let locks = devin::locks(&self.devin);
        self.devin_running = locks.iter().filter(|(pid, _)| self.sys.process(Pid::from_u32(*pid)).and_then(tool_of) == Some(Tool::Devin)).map(|(_, id)| id.clone()).collect();
        let owners = devin_owners(&self.sys, &locks);
        let ids: Vec<String> = owners.values().cloned().collect();
        let mut devin_records: HashMap<String, devin::Record> = devin::records(&self.devin, &ids).into_iter().map(|r| (r.id.clone(), r)).collect();
        self.devin_open.clear();

        // Claude Code: one record per running session.
        let records_dir = self.claude.join("sessions");
        let have_records = records_dir.is_dir();
        for r in claude_records(&records_dir) {
            // The pid may since belong to something else.
            let Some(sp) = self.sys.process(Pid::from_u32(r.pid)).filter(|p| tool_of(p) == Some(Tool::Claude)) else { continue };
            // A run a program drives (`claude -p`: kiln's own agents, the Agent SDK's) is that
            // program's to show, though its record says "interactive" all the same.
            if r.kind.as_deref().is_some_and(|k| k != "interactive") || !interactive(Tool::Claude, &args_of(sp)) {
                continue;
            }
            let p = Proc::of(sp);
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
            if seen.contains(&pid.as_u32()) || !interactive(tool, &args_of(p)) {
                continue;
            }
            // Claude Code writes its record once the session starts (after the folder-trust
            // prompt). Before that, only one inside a kiln terminal is shown: it is the one
            // someone just launched from here.
            if tool == Tool::Claude && have_records && terminal_of(&self.sys, *pid, terminals).is_none() {
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
        // Transcripts already accounted for are nobody else's.
        let mut taken: HashSet<PathBuf> = open.values().map(|(_, f): &(Tool, PathBuf)| f.clone()).collect();
        for (tool, p) in found {
            let file = match tool {
                Tool::Codex => self.codex_file(&p, &taken),
                Tool::Claude => self.claude_file_by_cwd(&p, &taken),
                Tool::Gemini => None,
                Tool::Devin => None,
            };
            if let Some(f) = &file {
                taken.insert(f.clone());
            }
            let summary = file.as_deref().and_then(|f| self.summary(tool, f));
            let mut s = self.session(tool, &p, file.as_deref(), summary, terminals);
            if tool == Tool::Devin && let Some(record) = owners.get(&p.pid).and_then(|id| devin_records.remove(id)) {
                let meta = Meta { id: record.id.clone(), title: record.title.clone(), cwd: record.cwd, model: record.model, ..Meta::default() };
                let summary = summarize(meta, &record.entries);
                s.id = record.id.clone();
                s.name = record.title;
                s.title = if summary.meta.title.is_empty() { first_line(&summary.last_prompt, 80) } else { summary.meta.title };
                s.cwd = summary.meta.cwd;
                s.model = summary.meta.model;
                s.last_activity = record.last_activity.max(record.created_at).max(s.started_at);
                s.last_prompt = clip(&summary.last_prompt, SNIPPET);
                s.last_reply = clip(&summary.last_reply, SNIPPET);
                s.transcript = !record.entries.is_empty();
                let stalled = time::OffsetDateTime::now_utc().unix_timestamp().saturating_sub(s.last_activity) > STALLED.as_secs() as i64;
                s.state = if !s.transcript { "starting" } else if summary.turn_ended || stalled { "waiting" } else { "working" }.into();
                self.devin_open.insert(p.pid, record.id);
            }
            // Claude Code without its record has not started a session yet.
            if tool == Tool::Claude && have_records {
                s.state = "starting".into();
            }
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
        self.running = Some(out.iter().map(|s| s.id.clone()).filter(|id| !id.is_empty()).collect());
        out
    }

    /// The normalized view of the session running as `pid` (Claude Code; as of the last scan).
    pub fn view(&self, pid: u32) -> Option<Value> {
        let (tool, file) = self.open.get(&pid)?;
        if *tool != Tool::Claude {
            return None;
        }
        let p = self.sys.process(Pid::from_u32(pid));
        let cwd = p.and_then(|p| p.cwd()).map(|c| c.to_string_lossy().into_owned()).unwrap_or_default();
        let started = p.and_then(|p| i64::try_from(p.start_time()).ok());
        crate::harness::transcript::view(file, &cwd, started)
    }

    /// Recent messages of the transcript of the session running as `pid` (as of the last scan).
    pub fn transcript(&self, pid: u32) -> Option<Vec<Entry>> {
        if let Some(id) = self.devin_open.get(&pid) {
            return devin::transcript(&self.devin, id, OPEN_ENTRIES);
        }
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

    /// This machine's conversations that are not running, most recently active first, at most
    /// `limit`: Claude Code's (`projects/*/*.jsonl`) and Codex's (`sessions/**/rollout-*.jsonl`),
    /// or one `kind`'s. Running: those found in terminals (a scan), Claude Code's with a live
    /// session record (of any kind), and `running` (the harness ids of kiln's own sessions).
    /// Only the head and the tail of each transcript are read, once per change. Asked for a
    /// `person`, those kept in their microVM volumes here too, each saying so (`isolation`; kiln plan
    /// 01a1d2c16eb074f19cf7ae7f1025c0be step 5), never another person's.
    pub fn history(&mut self, kind: Option<&str>, limit: usize, running: &HashSet<String>, person: Option<&str>) -> Vec<PastSession> {
        // What runs, as the latest scan found it: the scans the tools' changes make keep it, so a
        // history reads no process of its own (2026-11-12, adacachy: each read every one of 6,474
        // processes, a 10 s ask's answer 17.9 s late under load). Before any scan, one.
        if self.running.is_none() {
            self.scan(&HashMap::new());
        }
        let open: HashSet<PathBuf> = self.open.values().map(|(_, f)| f.clone()).collect();
        let mut skip: HashSet<String> = running.clone();
        skip.extend(self.devin_running.iter().cloned());
        skip.extend(self.running.iter().flatten().cloned());
        for r in claude_records(&self.claude.join("sessions")) {
            if self.sys.process(Pid::from_u32(r.pid)).is_some_and(|p| tool_of(p) == Some(Tool::Claude)) {
                skip.insert(r.session_id);
            }
        }
        let want = |k: &str| kind.is_none_or(|w| w == k);
        // This machine's own folders, then the asking person's microVM volumes. What lives in a
        // volume is a microVM's: Claude Code's session records there name a guest's pids, and no
        // process here is its harness, so only `running` (kiln's own sessions) holds one back.
        let mut places = vec![Folders { claude: Some(self.claude.clone()), codex: Some(self.codex.clone()), devin: Some(self.devin.clone()), isolation: None }];
        if let (Some(homes), Some(person)) = (&self.homes, person) {
            match volumes_of(homes, person) {
                Ok(theirs) => places.push(theirs),
                Err(refused) => tracing::warn!(error = %refused, "a history reads no microVM volume of this person's"),
            }
        }
        let mut files: Vec<(SystemTime, Tool, PathBuf, usize)> = Vec::new();
        for (at, place) in places.iter().enumerate() {
            let mut stat = |tool: Tool, paths: Vec<PathBuf>| {
                for f in paths {
                    if let Ok(m) = std::fs::metadata(&f).and_then(|m| m.modified()) {
                        files.push((m, tool, f, at));
                    }
                }
            };
            if want("claude")
                && let Some(claude) = &place.claude
            {
                let mut paths = Vec::new();
                for d in std::fs::read_dir(claude.join("projects")).into_iter().flatten().flatten() {
                    // Sessions only: subagents' transcripts live below them, or as agent-*.jsonl.
                    walk_jsonl(&d.path(), 0, &mut |f| !f.file_name().is_some_and(|n| n.to_string_lossy().starts_with("agent-")), &mut paths);
                }
                stat(Tool::Claude, paths);
            }
            if want("codex")
                && let Some(codex) = &place.codex
            {
                let mut paths = Vec::new();
                walk_jsonl(&codex.join("sessions"), 3, &mut |_| true, &mut paths);
                stat(Tool::Codex, paths);
            }
        }
        files.sort_by_key(|(m, _, _, _)| std::cmp::Reverse(*m));
        let names: Vec<HashMap<String, String>> = places.iter().map(|p| p.codex.as_deref().filter(|_| want("codex")).map(codex_names).unwrap_or_default()).collect();
        let limit = limit.clamp(1, HISTORY_MAX);
        let mut out: Vec<PastSession> = Vec::new();
        let mut seen = HashSet::new();
        for (modified, tool, f, at) in files {
            if out.len() >= limit {
                break;
            }
            if open.contains(&f) {
                continue;
            }
            let Some(mut p) = self.past_session(tool, &f, modified) else { continue };
            let Some(place) = places.get(at) else { continue };
            // A thread's forks and later parts share its id: the newest stands for it.
            if place.held(&skip, running).contains(&p.id) || !seen.insert((at, p.id.clone())) {
                continue;
            }
            if let Some(n) = names.get(at).and_then(|n| n.get(&p.id)) {
                p.title.clone_from(n);
            }
            p.isolation.clone_from(&place.isolation);
            ran_in(&mut p, self.homes.as_deref());
            out.push(p);
        }
        if self.past.len() > HISTORY_CACHE {
            self.past.clear();
        }
        if want("devin") {
            for place in &places {
                if let Some(devin) = &place.devin {
                    out.extend(devin::history(devin, limit, place.held(&skip, running)).into_iter().map(|p| {
                        let mut p = PastSession { isolation: place.isolation.clone(), ..p };
                        ran_in(&mut p, self.homes.as_deref());
                        p
                    }));
                }
            }
            out.sort_by_key(|p| std::cmp::Reverse(p.last_activity));
            out.truncate(limit);
        }
        out
    }

    /// A transcript as a past session, reread only when it changed.
    fn past_session(&mut self, tool: Tool, f: &Path, modified: SystemTime) -> Option<PastSession> {
        let len = std::fs::metadata(f).ok()?.len();
        if let Some((l, m, p)) = self.past.get(f)
            && *l == len
            && *m == modified
        {
            return p.clone();
        }
        let p = read_past(tool, f, len, modified);
        self.past.insert(f.to_owned(), (len, modified, p.clone()));
        p
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
        let projects = self.claude.join("projects");
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

    /// Without a session record: the newest transcript in the process's project directory that
    /// was created after it started (other sessions in the same directory write there too).
    fn claude_file_by_cwd(&self, p: &Proc, taken: &HashSet<PathBuf>) -> Option<PathBuf> {
        if p.cwd.is_empty() {
            return None;
        }
        let dir = self.claude.join("projects").join(claude_dir_name(&p.cwd));
        let mut files = Vec::new();
        walk_jsonl(&dir, 0, &mut |f| born_since(f, p.start), &mut files);
        files
            .into_iter()
            .filter(|f| !taken.contains(f))
            .filter_map(|f| Some((std::fs::metadata(&f).ok()?.modified().ok()?, f)))
            .max_by_key(|(m, _)| *m)
            .map(|(_, f)| f)
    }

    /// A Codex process's rollout: the one `codex resume <id>` names, or the newest of its
    /// directory whose thread began after the process started (a new session's own; Codex writes
    /// it from the first turn on). A session resumed from Codex's menu writes to an older rollout
    /// that could as well be another session's: none is claimed for it.
    fn codex_file(&mut self, p: &Proc, taken: &HashSet<PathBuf>) -> Option<PathBuf> {
        let key = (p.pid, p.start);
        if let Some(f) = self.codex_files.get(&key) {
            return Some(f.clone());
        }
        let root = self.codex.join("sessions");
        let resume = p.args.iter().position(|a| a == "resume").and_then(|i| p.args.get(i + 1)).filter(|a| !a.starts_with('-'));
        // (A second of slack for the start time's rounding.)
        let since = p.start.saturating_sub(1);
        let mut files = Vec::new();
        match resume {
            // Resumed sessions keep writing their original rollout, from whatever day it began.
            Some(id) => walk_jsonl(&root, 3, &mut |f| f.file_name().is_some_and(|n| n.to_string_lossy().ends_with(&format!("{id}.jsonl"))), &mut files),
            None => {
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
                let head = self.codex_session_meta(&f);
                // A terminal's thread: not one kiln's agent sessions (or an editor) started.
                let tui = head.originator.is_empty() || head.originator.contains("tui") || head.originator.contains("cli");
                if head.cwd != p.cwd || u64::try_from(head.began).unwrap_or(0) < since || !tui {
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

    fn codex_session_meta(&mut self, f: &Path) -> CodexHead {
        if let Some(m) = self.codex_meta.get(f) {
            return m.clone();
        }
        let first = head_line(f).unwrap_or_default();
        let (meta, _) = parse_codex(std::slice::from_ref(&first));
        let v = serde_json::from_str::<Value>(&first).unwrap_or(Value::Null);
        let began = v
            .pointer("/payload/timestamp")
            .and_then(Value::as_str)
            .and_then(|t| time::OffsetDateTime::parse(t, &time::format_description::well_known::Rfc3339).ok())
            .map_or(0, time::OffsetDateTime::unix_timestamp);
        let originator = v.pointer("/payload/originator").and_then(Value::as_str).unwrap_or("").to_owned();
        let m = CodexHead { cwd: meta.cwd, began, originator };
        // Only remember a complete header: a brand-new file may not have one yet.
        if !m.cwd.is_empty() {
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

/// Where a history reads one place's conversations, as the harnesses keep them there: Claude Code's
/// configuration folder, Codex's home and Devin's `devin/cli`; and how what's there ran (none: this
/// machine's own, as processes here).
struct Folders {
    claude: Option<PathBuf>,
    codex: Option<PathBuf>,
    devin: Option<PathBuf>,
    isolation: Option<Isolation>,
}

impl Folders {
    /// What holds a conversation here back from a history: on this machine, all it finds running
    /// (`machine`); in a microVM volume, kiln's own sessions alone (`kilns`), as no process of this
    /// machine's is a microVM's harness and no record a microVM's harness writes names one of them.
    fn held<'a>(&self, machine: &'a HashSet<String>, kilns: &'a HashSet<String>) -> &'a HashSet<String> {
        if self.isolation.is_some() { kilns } else { machine }
    }
}

/// `person`'s microVM volumes under `homes` (`<state>/homes/<person>/<harness>/`, kiln plan
/// 01a1d2c16eb074f19cf7ae7f1025c0be step 5), as kiln's Agentfile for each harness lays them out
/// (`home::of_kilns`): each harness's folder at its path on this machine, its conversations saying they
/// ran in a microVM of that person's. Refused, typed, for a person kiln keeps no volume for.
fn volumes_of(homes: &Path, person: &str) -> Result<Folders, Refusal> {
    let var = |harness: &str, key: &str| -> Result<Option<PathBuf>, Refusal> {
        Ok(home::of_kilns(homes, person, harness)?.and_then(|l| l.host_env().into_iter().find(|(k, _)| k == key)).map(|(_, v)| PathBuf::from(v)))
    };
    Ok(Folders {
        claude: var("claude", "CLAUDE_CONFIG_DIR")?,
        codex: var("codex", "CODEX_HOME")?,
        // In its microVM Devin keeps its sessions in XDG's data folder, as on any Linux (`Scanner::new`).
        devin: var("devin", "XDG_DATA_HOME")?.map(|d| d.join("devin").join("cli")),
        isolation: Some(Isolation { person: Some(person.to_owned()), ..Isolation::microvm() }),
    })
}

/// `p`, from a person's microVM volume under `homes`, goes on in the Agentfile it ran in, as kiln kept it
/// beside the volume (`home::RanIn`, kiln plan 01a1cd8e862e7169a074db3d5fa33f3d step 17, goal 65's
/// finding), not the one a start would take today; one kiln kept nothing of stays as it is.
fn ran_in(p: &mut PastSession, homes: Option<&Path>) {
    let (Some(homes), Some(isolation)) = (homes, p.isolation.as_mut()) else {
        return;
    };
    let Some(person) = isolation.person.as_deref() else {
        return;
    };
    if let Some(ran) = home::ran_in_dir(homes, person, &p.kind).ok().and_then(|d| home::ran_in(&d, &p.id)) {
        isolation.image = Some(ran.agentfile);
    }
}

/// A transcript's head and tail (the whole of a small one) as one past session.
fn read_past(tool: Tool, f: &Path, len: u64, modified: SystemTime) -> Option<PastSession> {
    let mut lines = Vec::new();
    if len > HISTORY_HEAD + HISTORY_TAIL {
        let file = std::fs::File::open(f).ok()?;
        let mut buf = Vec::new();
        file.take(HISTORY_HEAD).read_to_end(&mut buf).ok()?;
        let head = String::from_utf8_lossy(&buf);
        // (Its last line is cut short.)
        let mut head_lines: Vec<&str> = head.lines().collect();
        head_lines.pop();
        lines.extend(head_lines.into_iter().filter(|l| !l.trim().is_empty()).map(str::to_owned));
        lines.extend(tail_lines(f, HISTORY_TAIL)?);
    } else {
        lines = tail_lines(f, len.max(1))?;
    }
    let (mut meta, entries) = parse(tool, &lines);
    if meta.model.is_empty() && len > HISTORY_HEAD + HISTORY_TAIL {
        let (more, _) = parse(tool, &tail_lines(f, HISTORY_MODEL_TAIL.min(len)).unwrap_or_default());
        meta.model = more.model;
    }
    let summary = summarize(meta, &entries);
    let meta = &summary.meta;
    let id = match tool {
        // The file's name is the session's (the thread's, for Codex: a fork's or later part's
        // first id).
        Tool::Claude => f.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
        _ => codex_thread_of(f).unwrap_or_else(|| meta.id.clone()),
    };
    if id.is_empty() || (summary.last_prompt.trim().is_empty() && meta.title.is_empty()) {
        return None;
    }
    // Codex's header (the first line) can outgrow the head read: its folder and branch.
    let (cwd, branch) = if tool == Tool::Codex && (meta.cwd.is_empty() || meta.branch.is_empty()) {
        let (head, _) = parse_codex(&head_line(f).into_iter().collect::<Vec<_>>());
        (if meta.cwd.is_empty() { head.cwd } else { meta.cwd.clone() }, if meta.branch.is_empty() { head.branch } else { meta.branch.clone() })
    } else {
        (meta.cwd.clone(), meta.branch.clone())
    };
    Some(PastSession {
        kind: tool.name().to_owned(),
        id,
        title: if meta.title.is_empty() { first_line(&summary.last_prompt, 80) } else { meta.title.clone() },
        cwd,
        branch,
        model: meta.model.clone(),
        last_activity: unix_secs(modified),
        last_prompt: clip(&summary.last_prompt, SNIPPET),
        isolation: None,
    })
}

/// The thread a Codex rollout belongs to, from its name: `rollout-<time>-<thread>[_<part>].jsonl`.
fn codex_thread_of(f: &Path) -> Option<String> {
    let name = f.file_name()?.to_str()?.strip_prefix("rollout-")?.strip_suffix(".jsonl")?;
    // The time is "YYYY-MM-DDThh-mm-ss", then "-".
    let rest = name.get(20..)?;
    let thread = rest.split('_').next()?;
    (thread.len() >= 32).then(|| thread.to_owned())
}

/// Codex's thread names (`session_index.jsonl`; the latest per thread).
fn codex_names(home: &Path) -> HashMap<String, String> {
    let mut names = HashMap::new();
    let Some(lines) = tail_lines(&home.join("session_index.jsonl"), 1024 * 1024) else { return names };
    for l in lines {
        let Ok(v) = serde_json::from_str::<Value>(&l) else { continue };
        if let (Some(id), Some(n)) = (str_at(&v, "id"), str_at(&v, "thread_name"))
            && !n.trim().is_empty()
        {
            names.insert(id.to_owned(), n.to_owned());
        }
    }
    names
}

fn args_of(p: &Process) -> Vec<String> {
    p.cmd().iter().map(|a| a.to_string_lossy().into_owned()).collect()
}

/// Which tool a process is, if any: by its name, or by the script a Node or Bun process runs.
fn tool_of(p: &Process) -> Option<Tool> {
    let by_name = |n: &str| match n {
        "claude" => Some(Tool::Claude),
        "codex" => Some(Tool::Codex),
        "devin" => Some(Tool::Devin),
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

/// A native Devin REPL owns its ACP child's lock. A nested agent's lock and a process with
/// several conversations do not identify which conversation a terminal is showing.
fn devin_owners(sys: &System, locks: &[(u32, String)]) -> HashMap<u32, String> {
    let mut owners: HashMap<u32, Vec<String>> = HashMap::new();
    for (owner, id) in locks {
        let mut pid = Pid::from_u32(*owner);
        if sys.process(pid).and_then(tool_of) != Some(Tool::Devin) {
            continue;
        }
        for _ in 0..MAX_DEPTH {
            let Some(p) = sys.process(pid) else { break };
            if tool_of(p) == Some(Tool::Devin) {
                if interactive(Tool::Devin, &args_of(p)) {
                    owners.entry(pid.as_u32()).or_default().push(id.clone());
                    break;
                }
                if pid.as_u32() != *owner {
                    break;
                }
            }
            let Some(parent) = p.parent() else { break };
            pid = parent;
        }
    }
    owners.into_iter().filter_map(|(pid, ids)| {
        if ids.len() != 1 {
            return None;
        }
        ids.into_iter().next().map(|id| (pid, id))
    }).collect()
}

/// Whether `pid` runs the harness `kind`, as a scan would
/// find it.
pub fn runs(pid: u32, kind: &str) -> bool {
    let p = Pid::from_u32(pid);
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[p]),
        true,
        ProcessRefreshKind::nothing().with_cmd(UpdateKind::Always).with_exe(UpdateKind::Always).without_tasks(),
    );
    sys.process(p).and_then(tool_of).is_some_and(|t| t.name() == kind)
}

/// Ends a session running outside kiln, for a take-over: asks its process to quit (SIGTERM;
/// Windows has no such ask, so there it is `false`), or with `force` kills it and everything it
/// started, never kiln itself. Whether that was done.
pub fn end(pid: u32, force: bool) -> bool {
    let root = Pid::from_u32(pid);
    let ours = Pid::from_u32(std::process::id());
    if pid <= 1 || root == ours {
        return false;
    }
    let mut sys = System::new();
    if !force {
        sys.refresh_processes_specifics(ProcessesToUpdate::Some(&[root]), true, ProcessRefreshKind::nothing().without_tasks());
        return sys.process(root).and_then(|p| p.kill_with(sysinfo::Signal::Term)).unwrap_or(false);
    }
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing().without_tasks());
    // Its tree, parents first, leaving out kiln's own (were kiln started from inside it).
    let mut tree = vec![root];
    let mut i = 0;
    while let Some(&parent) = tree.get(i) {
        let kids: Vec<Pid> = sys.processes().iter().filter(|(c, p)| p.parent() == Some(parent) && **c != ours && !tree.contains(*c)).map(|(c, _)| *c).collect();
        tree.extend(kids);
        i += 1;
    }
    let ended = sys.process(root).is_some_and(Process::kill);
    for c in tree.iter().skip(1) {
        if let Some(p) = sys.process(*c) {
            p.kill();
        }
    }
    ended
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
        Tool::Devin => {
            let mut args = rest.into_iter().peekable();
            while let Some(a) = args.next() {
                match a {
                    "--" => return true,
                    "-p" | "--print" | "-h" | "--help" | "-V" | "--version" => return false,
                    "--config" | "--model" | "--permission-mode" | "--prompt-file" | "--respect-workspace-trust" => { args.next(); }
                    "--export" | "-r" | "--resume" => {
                        if args.peek().is_some_and(|a| !a.starts_with('-')) { args.next(); }
                    }
                    a if a.starts_with("--print=") || a.starts_with("-p") => return false,
                    a if a.starts_with('-') => {}
                    a => return !["acp", "auth", "mcp", "models", "doctor", "rules", "skills", "plugins", "cloud", "desktop", "list", "ls", "rm", "ssh", "forward", "update", "version", "migrate", "sandbox", "setup", "uninstall", "help"].contains(&a),
                }
            }
            true
        }
    }
}

/// The kiln terminal a process runs in: the nearest ancestor that is one of this runner's
/// terminal shells, else the `KILN_TERM_SESSION` every kiln terminal's shell exports. The
/// variable finds terminals another runner process of this machine holds (an older one keeps its
/// shells across an upgrade, and only the newest reports); the server drops ids it no longer has.
fn terminal_of(sys: &System, pid: Pid, terminals: &HashMap<u32, String>) -> Option<String> {
    let mut cur = pid;
    for _ in 0..MAX_DEPTH {
        if let Some(id) = terminals.get(&cur.as_u32()) {
            return Some(id.clone());
        }
        let Some(parent) = sys.process(cur).and_then(Process::parent) else { break };
        cur = parent;
    }
    sys.process(pid).and_then(|p| term_session_in(p.environ()))
}

/// The kiln terminal session an environment names.
fn term_session_in(env: &[std::ffi::OsString]) -> Option<String> {
    env.iter().find_map(|kv| kv.to_str()?.strip_prefix("KILN_TERM_SESSION=").filter(|v| !v.is_empty()).map(str::to_owned))
}

/// Created at or after `since` (unix seconds). Where the filesystem keeps no creation time,
/// nothing qualifies: an older file could be any session's.
fn born_since(f: &Path, since: u64) -> bool {
    std::fs::metadata(f).and_then(|m| m.created()).is_ok_and(|c| u64::try_from(unix_secs(c)).unwrap_or(0) >= since)
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
        Tool::Gemini | Tool::Devin => (Meta::default(), Vec::new()),
    }
}

fn entry(role: &str, text: &str, at: &str) -> Entry {
    Entry { role: role.to_owned(), text: clip(text, ENTRY_CHARS), at: at.to_owned() }
}

/// Prompts, replies and tool calls of the main conversation; subagents' records are skipped.
fn parse_claude(lines: &[String]) -> (Meta, Vec<Entry>) {
    let mut meta = Meta::default();
    let mut entries = Vec::new();
    let mut custom = false;
    for line in lines {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        let kind = str_at(&v, "type").unwrap_or("");
        match kind {
            "ai-title" => {
                if let Some(t) = str_at(&v, "aiTitle")
                    && !custom
                {
                    meta.title = t.to_owned();
                }
                continue;
            }
            // Named by the user (/rename): over any generated title.
            "custom-title" => {
                if let Some(t) = str_at(&v, "customTitle").filter(|t| !t.trim().is_empty()) {
                    meta.title = t.to_owned();
                    custom = true;
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
    fn kiln_s_own_agents_are_not_sessions_found_elsewhere() {
        // Claude Code records the sessions kiln drives as "interactive" too: their `-p` tells.
        let spec = crate::harness::session::Spec { kind: "claude".into(), mode: Some("bypassPermissions".into()), ..Default::default() };
        let mut bin = vec!["/usr/local/bin/claude".to_owned()];
        bin.extend(crate::harness::claude::args(&spec, None));
        let mut node = vec!["node".to_owned(), "/usr/lib/node_modules/@anthropic-ai/claude-code/cli.js".to_owned()];
        node.extend(crate::harness::claude::args(&spec, Some("01a14a73-8779-77b9-ba23-49a19e001122")));
        assert!(!interactive(Tool::Claude, &bin) && !interactive(Tool::Claude, &node));
        // One you started in a terminal is yours to take over.
        assert!(interactive(Tool::Claude, &["claude".to_owned(), "--resume".to_owned(), "01a14a73".to_owned()]));
    }

    #[test]
    fn devin_native_sessions_exclude_helpers_and_print_runs() {
        let is = |args: &[&str]| interactive(Tool::Devin, &args.iter().map(|a| (*a).to_owned()).collect::<Vec<_>>());
        for args in [vec!["devin"], vec!["devin", "-r", "auth"], vec!["devin", "--model", "codex", "--", "fix the build"], vec!["devin", "--", "auth"]] {
            assert!(is(&args), "{args:?}");
        }
        for args in [vec!["devin", "acp"], vec!["devin", "auth", "status"], vec!["devin", "--config", "/tmp/config", "list"], vec!["devin", "--model", "opus", "-p", "hello"], vec!["devin", "--export", "-p", "hello"], vec!["devin", "--resume", "--print=hello"], vec!["devin", "--print=hello"], vec!["devin", "--help"]] {
            assert!(!is(&args), "{args:?}");
        }
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

    #[test]
    fn terminal_from_the_environment() {
        let env = |vars: &[&str]| vars.iter().map(std::ffi::OsString::from).collect::<Vec<_>>();
        assert_eq!(term_session_in(&env(&["HOME=/home/ada", "KILN_TERM_SESSION=t-41", "TERM=xterm"])), Some("t-41".into()));
        assert_eq!(term_session_in(&env(&["KILN_TERM_SESSION="])), None);
        assert_eq!(term_session_in(&env(&["XKILN_TERM_SESSION=t-1", "KILN_TERM_SESSIONS=t-2"])), None);
        assert_eq!(term_session_in(&[]), None);
    }

    /// A harness started in a terminal another runner process holds (so it isn't in `terminals`)
    /// is still linked to that terminal, by the variable its shell exported.
    #[cfg(target_os = "linux")]
    #[test]
    fn harness_in_another_runners_terminal() {
        use std::os::unix::fs::PermissionsExt as _;
        let (Ok(home), Ok(bin)) = (tempfile::tempdir(), tempfile::tempdir()) else { return };
        // A script is named for itself in the process table; it waits on sleep rather than exec it.
        let claude = bin.path().join("claude");
        if std::fs::write(&claude, "#!/bin/sh\nsleep 30\n").is_err() || std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).is_err() {
            return;
        }
        let Ok(mut child) = std::process::Command::new(&claude).env("KILN_TERM_SESSION", "t-elsewhere").spawn() else { return };
        let pid = child.id();
        let mut s = Scanner::new(home.path().to_owned());
        let mut found = None;
        for _ in 0..50 {
            found = s.scan(&HashMap::new()).into_iter().find(|x| x.pid == pid);
            if found.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = child.kill();
        let _ = child.wait();
        let found = found.map(|f| (f.tool, f.terminal));
        assert_eq!(found, Some(("claude".to_owned(), Some("t-elsewhere".to_owned()))));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_process_scanned_before_exec_is_discovered_as_its_new_harness() -> anyhow::Result<()> {
        use std::io::{Read as _, Write as _};
        use std::os::unix::fs::PermissionsExt as _;
        use std::process::Stdio;

        struct Running(std::process::Child);
        impl Drop for Running {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }

        let home = tempfile::tempdir()?;
        let program = home.path().join("devin");
        std::fs::write(&program, "#!/bin/sh\nprintf ready\nread -r stop\n")?;
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755))?;
        let mut child = Running(std::process::Command::new("/bin/sh")
            .args(["-c", "printf ready; read -r release; exec \"$1\"", "sh"])
            .arg(&program)
            .env("KILN_TERM_SESSION", "exec-terminal")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?);
        let pid = child.0.id();
        let mut input = child.0.stdin.take().ok_or_else(|| anyhow::anyhow!("no child stdin"))?;
        let mut output = child.0.stdout.take().ok_or_else(|| anyhow::anyhow!("no child stdout"))?;
        let mut ready = [0; 5];
        output.read_exact(&mut ready)?;
        assert_eq!(&ready, b"ready");
        let mut scanner = Scanner::new(home.path().to_owned());
        scanner.claude = home.path().join("claude");
        scanner.codex = home.path().join("codex");
        scanner.devin = home.path().join("native");
        assert!(!scanner.scan(&HashMap::new()).iter().any(|s| s.pid == pid));
        assert!(scanner.sys.process(Pid::from_u32(pid)).is_some_and(|p| tool_of(p).is_none()));
        input.write_all(b"go\n")?;
        output.read_exact(&mut ready)?;
        assert_eq!(&ready, b"ready");
        let found = scanner.scan(&HashMap::new()).into_iter().find(|s| s.pid == pid);
        assert_eq!(found.map(|s| (s.tool, s.terminal)), Some(("devin".to_owned(), Some("exec-terminal".to_owned()))));
        Ok(())
    }


    /// 2026-11-12, adacachy: each history read every one of the machine's 6,474 processes, its
    /// answer 17.9 s late against a 10 s ask under load. A history reads no process once a scan
    /// has said what runs (the scans the tools' changes make keep that); before any, it scans once.
    #[test]
    fn a_history_reads_no_process_once_a_scan_said_what_runs() {
        let home = std::env::temp_dir().join(format!("kiln-history-scan-{}", std::process::id()));
        let mut sc = Scanner::new(home);
        assert!(sc.running.is_none());
        let _ = sc.history(Some("claude"), 10, &HashSet::new(), None);
        assert!(sc.running.is_some(), "before any scan, the history scans once");
        assert!(!sc.sys.processes().is_empty(), "that scan read the processes");
        sc.sys = System::new();
        sc.running = Some(HashSet::from(["running-now".to_owned()]));
        let _ = sc.history(Some("claude"), 10, &HashSet::new(), None);
        assert!(sc.sys.processes().is_empty(), "a history after a scan reads no process");
        assert_eq!(sc.running, Some(HashSet::from(["running-now".to_owned()])));
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn a_native_devin_session_uses_its_held_lock_and_enters_history_once_ended() -> anyhow::Result<()> {
        use std::io::Write as _;
        use std::os::unix::fs::PermissionsExt as _;
        use rusqlite::params;
        let home = tempfile::tempdir()?;
        let root = home.path().join("data");
        let locks = root.join("session_locks");
        std::fs::create_dir_all(&locks)?;
        let program = home.path().join("devin");
        std::fs::write(&program, "#!/bin/sh\nsleep 30\n")?;
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755))?;
        let db = rusqlite::Connection::open(root.join("sessions.db"))?;
        db.execute_batch("CREATE TABLE sessions (id TEXT PRIMARY KEY, working_directory TEXT, model TEXT, title TEXT, created_at INTEGER, last_activity_at INTEGER, main_chain_id INTEGER, hidden INTEGER); CREATE TABLE message_nodes (session_id TEXT, node_id INTEGER, parent_node_id INTEGER, chat_message TEXT, UNIQUE(session_id, node_id));")?;
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        db.execute("INSERT INTO sessions VALUES ('native', '/work', 'model', 'Native conversation', ?1, ?1, 2, 0)", params![now])?;
        for (id, parent, role, text) in [(1, None, "user", "native question"), (2, Some(1), "assistant", "native answer")] {
            db.execute("INSERT INTO message_nodes VALUES ('native', ?1, ?2, ?3)", params![id, parent, serde_json::json!({"role": role, "content": text}).to_string()])?;
        }
        let mut child = std::process::Command::new(&program).env("KILN_TERM_SESSION", "native-terminal").spawn()?;
        let mut lock = std::fs::File::create(locks.join("native.lock"))?;
        writeln!(lock, "{}", child.id())?;
        lock.lock()?;
        let mut sc = Scanner::new(home.path().to_owned());
        sc.devin = root;
        let mut found = None;
        for _ in 0..50 {
            found = sc.scan(&HashMap::new()).into_iter().find(|s| s.pid == child.id() && s.id == "native");
            if found.is_some() { break }
            std::thread::sleep(Duration::from_millis(20));
        }
        let entries = sc.transcript(child.id());
        let active_history = sc.history(Some("devin"), 10, &HashSet::new(), None);
        let ambiguous = devin_owners(&sc.sys, &[(child.id(), "native".into()), (child.id(), "another".into())]);
        let _ = end(child.id(), true);
        let _ = child.wait();
        // Its process's end is a scan (`Reporter::watch`, `os::watch::exited`): a history reads what runs off it.
        sc.scan(&HashMap::new());
        let past = sc.history(Some("devin"), 10, &HashSet::new(), None);
        let found = found.ok_or_else(|| anyhow::anyhow!("native session not found"))?;
        assert_eq!((found.tool.as_str(), found.title.as_str(), found.model.as_str(), found.last_prompt.as_str(), found.last_reply.as_str()), ("devin", "Native conversation", "model", "native question", "native answer"));
        assert_eq!(found.terminal.as_deref(), Some("native-terminal"));
        assert_eq!(found.takeover(true), TakeOver::Ready);
        assert_eq!(entries.unwrap_or_default().iter().map(|e| e.text.as_str()).collect::<Vec<_>>(), ["native question", "native answer"]);
        assert!(active_history.is_empty() && ambiguous.is_empty());
        assert_eq!(past.iter().map(|p| (p.id.as_str(), p.last_prompt.as_str())).collect::<Vec<_>>(), [("native", "native question")]);
        Ok(())
    }

    #[test]
    fn a_codex_process_gets_its_own_rollout() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let day = dir.path().join("sessions").join(time::OffsetDateTime::now_utc().format(&time::macros::format_description!("[year]/[month]/[day]"))?);
        std::fs::create_dir_all(&day)?;
        let now = time::OffsetDateTime::now_utc();
        let rfc = |t: time::OffsetDateTime| t.format(&time::format_description::well_known::Rfc3339).unwrap_or_default();
        let rollout = |id: &str, cwd: &str, began: time::OffsetDateTime| -> anyhow::Result<PathBuf> {
            let f = day.join(format!("rollout-x-{id}.jsonl"));
            let originator = if id.starts_with("kiln") { "kiln" } else { "codex-tui" };
            let head = serde_json::json!({"timestamp": rfc(now), "type": "session_meta", "payload": {"id": id, "cwd": cwd, "timestamp": rfc(began), "originator": originator}});
            std::fs::write(&f, format!("{head}\n"))?;
            Ok(f)
        };
        let mut sc = Scanner::new(dir.path().to_owned());
        sc.codex = dir.path().to_owned();
        let start = u64::try_from(now.unix_timestamp()).unwrap_or(0).saturating_sub(5);
        let p = Proc { pid: 4242, start, cwd: "/work".into(), args: vec!["codex".into()] };
        // A thread that began before the process did, still written to (another session in the
        // same folder, ending): not its.
        rollout("other-1", "/work", now - time::Duration::minutes(3))?;
        assert_eq!(sc.codex_file(&p, &HashSet::new()), None);
        // Another folder's, or a thread one of kiln's agent sessions began meanwhile: not its
        // either. Its own, once written: its for good.
        rollout("elsewhere-1", "/other", now)?;
        rollout("kiln-1", "/work", now)?;
        assert_eq!(sc.codex_file(&p, &HashSet::new()), None);
        let own = rollout("own-1", "/work", now - time::Duration::seconds(2))?;
        assert_eq!(sc.codex_file(&p, &HashSet::new()).as_ref(), Some(&own));
        assert_eq!(sc.codex_files.get(&(4242, start)), Some(&own));
        // Resumed from Codex's menu: an older thread, nobody's for sure.
        let q = Proc { pid: 4343, start, cwd: "/work".into(), args: vec!["codex".into(), "resume".into()] };
        assert_eq!(sc.codex_file(&q, &HashSet::from([own.clone()])), None);
        // Resumed by id: that one.
        let r = Proc { pid: 4444, start, cwd: "/work".into(), args: vec!["codex".into(), "resume".into(), "other-1".into()] };
        assert!(sc.codex_file(&r, &HashSet::new()).is_some_and(|f| f.to_string_lossy().ends_with("other-1.jsonl")));
        Ok(())
    }

    #[test]
    fn past_sessions_newest_first_without_the_running_ones() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let (claude, codex) = (dir.path().join("claude"), dir.path().join("codex"));
        let write = |f: &Path, lines: &[serde_json::Value], age: u64| -> anyhow::Result<()> {
            if let Some(p) = f.parent() {
                std::fs::create_dir_all(p)?;
            }
            std::fs::write(f, lines.iter().map(|l| format!("{l}\n")).collect::<String>())?;
            std::fs::File::options().write(true).open(f)?.set_modified(SystemTime::now() - Duration::from_secs(age))?;
            Ok(())
        };
        let user = |id: &str, said: &str| serde_json::json!({"type": "user", "sessionId": id, "cwd": "/work/a", "gitBranch": "main", "timestamp": "t", "message": {"role": "user", "content": said}});
        let reply = |id: &str| serde_json::json!({"type": "assistant", "sessionId": id, "cwd": "/work/a", "timestamp": "t", "message": {"role": "assistant", "model": "claude-opus-5-5", "content": [{"type": "text", "text": "done"}]}});
        let proj = claude.join("projects").join("-work-a");
        // Named with /rename; then one with no name; then one kiln runs now; an empty one; a
        // subagent's.
        write(&proj.join("c-named.jsonl"), &[user("c-named", "fix the build"), reply("c-named"), serde_json::json!({"type": "custom-title", "customTitle": "Build fix", "sessionId": "c-named"})], 300)?;
        write(&proj.join("c-plain.jsonl"), &[user("c-plain", "write the docs\nall of them"), reply("c-plain")], 200)?;
        write(&proj.join("c-running.jsonl"), &[user("c-running", "busy"), reply("c-running")], 10)?;
        write(&proj.join("c-empty.jsonl"), &[serde_json::json!({"type": "queue-operation", "operation": "enqueue"})], 5)?;
        write(&proj.join("agent-sub.jsonl"), &[user("agent-sub", "sub task")], 1)?;
        // Codex: a thread (its session_meta, a turn's context, a prompt) and a later part of it;
        // its name from the index.
        let day = codex.join("sessions/2026/10/16");
        let thread = "01a14829-db2b-7e00-a770-67aa7bf6d977";
        let meta = serde_json::json!({"timestamp": "t", "type": "session_meta", "payload": {"id": thread, "cwd": "/work/b", "originator": "codex-tui", "git": {"branch": "dev"}}});
        let ctx = serde_json::json!({"timestamp": "t", "type": "turn_context", "payload": {"cwd": "/work/b", "model": "gpt-5.5"}});
        let said = serde_json::json!({"timestamp": "t", "type": "response_item", "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "ship it"}]}});
        write(&day.join(format!("rollout-2026-10-16T21-41-05-{thread}.jsonl")), &[meta.clone(), ctx.clone(), said.clone()], 100)?;
        write(&day.join(format!("rollout-2026-10-16T21-41-07-{thread}_01a14829-e32a-79c2-8581-3ab5a4b9d221.jsonl")), &[meta, ctx, said], 50)?;
        std::fs::write(codex.join("session_index.jsonl"), format!("{}\n", serde_json::json!({"id": thread, "thread_name": "Ship", "updated_at": "t"})))?;
        let mut sc = Scanner::new(dir.path().to_owned());
        sc.claude = claude;
        sc.codex = codex;
        let running: HashSet<String> = HashSet::from(["c-running".to_owned()]);
        let past = sc.history(None, 50, &running, None);
        let got: Vec<(&str, &str, &str, &str, &str, &str)> = past.iter().map(|p| (p.kind.as_str(), p.id.as_str(), p.title.as_str(), p.cwd.as_str(), p.branch.as_str(), p.model.as_str())).collect();
        assert_eq!(
            got,
            vec![
                ("codex", thread, "Ship", "/work/b", "dev", "gpt-5.5"),
                ("claude", "c-plain", "write the docs", "/work/a", "main", "claude-opus-5-5"),
                ("claude", "c-named", "Build fix", "/work/a", "main", "claude-opus-5-5"),
            ]
        );
        assert!(past.windows(2).all(|w| w.first().map(|p| p.last_activity) >= w.get(1).map(|p| p.last_activity)));
        assert_eq!(past.get(1).map(|p| p.last_prompt.as_str()), Some("write the docs\nall of them"));
        // One kind, a limit.
        assert_eq!(sc.history(Some("claude"), 1, &running, None).iter().map(|p| p.id.as_str()).collect::<Vec<_>>(), vec!["c-plain"]);
        assert!(sc.history(Some("codex"), 50, &running, None).iter().all(|p| p.kind == "codex"));
        // Written to since: read again.
        write(&proj.join("c-plain.jsonl"), &[user("c-plain", "write the docs"), reply("c-plain"), user("c-plain", "and the changelog")], 1)?;
        assert_eq!(sc.history(Some("claude"), 1, &running, None).first().map(|p| p.last_prompt.as_str()), Some("and the changelog"));
        Ok(())
    }

    /// Kiln plan 01a1d2c16eb074f19cf7ae7f1025c0be step 5: asked for a person, a history lists the
    /// Claude Code, Codex and Devin conversations kept in their microVM volumes here (laid out as
    /// kiln's Agentfile for each harness lays them out), each saying it ran in a microVM of theirs,
    /// beside the machine's own, newest first; never another person's, nor one kiln's own sessions
    /// hold. Asked for nobody, or for a person kiln keeps no volume for, the machine's own alone.
    /// Mutations: reading every person's volumes lists u2's; leaving out `isolation` marks none;
    /// reading Claude Code's folder from `HOME` finds none of the volume's.
    #[test]
    fn s35_history_lists_a_persons_volume_conversations_marked_isolated() -> anyhow::Result<()> {
        use rusqlite::params;
        let dir = tempfile::tempdir()?;
        let homes = home::root(dir.path());
        let write = |f: &Path, lines: &[serde_json::Value], age: u64| -> anyhow::Result<()> {
            if let Some(p) = f.parent() {
                std::fs::create_dir_all(p)?;
            }
            std::fs::write(f, lines.iter().map(|l| format!("{l}\n")).collect::<String>())?;
            std::fs::File::options().write(true).open(f)?.set_modified(SystemTime::now() - Duration::from_secs(age))?;
            Ok(())
        };
        let user = |id: &str, said: &str| serde_json::json!({"type": "user", "sessionId": id, "cwd": "/work/a", "timestamp": "t", "message": {"role": "user", "content": said}});
        // Where kiln's Agentfile for `harness` keeps `key` in `person`'s volume, on this machine.
        let at = |person: &str, harness: &str, key: &str| -> anyhow::Result<PathBuf> {
            let layout = home::of_kilns(&homes, person, harness)?.ok_or_else(|| anyhow::anyhow!("kiln ships no Agentfile for {harness}"))?;
            let v = layout.host_env().into_iter().find(|(k, _)| k == key).map(|(_, v)| v).ok_or_else(|| anyhow::anyhow!("{harness}'s layout names no {key}"))?;
            Ok(PathBuf::from(v))
        };
        // u1's: a Claude Code conversation, one kiln's own isolated session holds now, a Codex thread
        // (named in its index) and a Devin session; u2's Claude Code conversation; the machine's own.
        let claude = at("user:u1", "claude", "CLAUDE_CONFIG_DIR")?;
        assert!(claude.starts_with(homes.join("user-u1").join("claude")), "{}", claude.display());
        write(&claude.join("projects/-work-a/vm-claude.jsonl"), &[user("vm-claude", "port the parser")], 300)?;
        write(&claude.join("projects/-work-a/vm-held.jsonl"), &[user("vm-held", "busy in its microVM")], 10)?;
        let codex = at("user:u1", "codex", "CODEX_HOME")?;
        let thread = "01a1d2c9-c2a9-7212-911e-ce0f904323ab";
        let meta = serde_json::json!({"timestamp": "t", "type": "session_meta", "payload": {"id": thread, "cwd": "/work/b", "originator": "kiln"}});
        let said = serde_json::json!({"timestamp": "t", "type": "response_item", "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "ship it"}]}});
        write(&codex.join(format!("sessions/2026/11/13/rollout-2026-11-13T10-00-00-{thread}.jsonl")), &[meta, said], 200)?;
        std::fs::write(codex.join("session_index.jsonl"), format!("{}\n", serde_json::json!({"id": thread, "thread_name": "Ship", "updated_at": "t"})))?;
        let devin = at("user:u1", "devin", "XDG_DATA_HOME")?.join("devin").join("cli");
        std::fs::create_dir_all(&devin)?;
        let db = rusqlite::Connection::open(devin.join("sessions.db"))?;
        db.execute_batch("CREATE TABLE sessions (id TEXT PRIMARY KEY, working_directory TEXT, model TEXT, title TEXT, created_at INTEGER, last_activity_at INTEGER, main_chain_id INTEGER, hidden INTEGER); CREATE TABLE message_nodes (session_id TEXT, node_id INTEGER, parent_node_id INTEGER, chat_message TEXT, UNIQUE(session_id, node_id));")?;
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        db.execute("INSERT INTO sessions VALUES ('vm-devin', '/work/c', 'model', 'Devin in its microVM', ?1, ?1, 1, 0)", params![now - 100])?;
        db.execute("INSERT INTO message_nodes VALUES ('vm-devin', 1, NULL, ?1)", params![serde_json::json!({"role": "user", "content": "review it"}).to_string()])?;
        drop(db);
        write(&at("user:u2", "claude", "CLAUDE_CONFIG_DIR")?.join("projects/-work-a/theirs.jsonl"), &[user("theirs", "another person's")], 50)?;
        let machine = dir.path().join("machine");
        write(&machine.join("claude/projects/-work-a/c-machine.jsonl"), &[user("c-machine", "the machine's own")], 400)?;
        let mut sc = Scanner::new(dir.path().to_owned()).with_homes(homes.clone());
        (sc.claude, sc.codex, sc.devin) = (machine.join("claude"), machine.join("codex"), machine.join("devin"));
        // What runs, as a scan said: nothing of the machine's.
        sc.running = Some(HashSet::new());
        let running = HashSet::from(["vm-held".to_owned()]);
        let theirs = Isolation { person: Some("user:u1".into()), ..Isolation::microvm() };
        let past = sc.history(None, 50, &running, Some("user:u1"));
        let got: Vec<(&str, &str, &str, Option<&Isolation>)> = past.iter().map(|p| (p.kind.as_str(), p.id.as_str(), p.title.as_str(), p.isolation.as_ref())).collect();
        assert_eq!(
            got,
            vec![
                ("devin", "vm-devin", "Devin in its microVM", Some(&theirs)),
                ("codex", thread, "Ship", Some(&theirs)),
                ("claude", "vm-claude", "port the parser", Some(&theirs)),
                ("claude", "c-machine", "the machine's own", None),
            ]
        );
        // As the server is answered: the volume's say how they ran, the machine's say nothing.
        let json = serde_json::to_value(&past)?;
        assert_eq!(json.pointer("/0/isolation"), Some(&serde_json::json!({"kind": "microvm", "person": "user:u1"})));
        assert_eq!(json.pointer("/3/isolation"), None);
        // One harness's, from the volume too.
        assert_eq!(sc.history(Some("codex"), 50, &running, Some("user:u1")).iter().map(|p| (p.id.as_str(), p.isolation.is_some())).collect::<Vec<_>>(), [(thread, true)]);
        // Another person's own: theirs alone.
        assert_eq!(sc.history(Some("claude"), 50, &running, Some("user:u2")).iter().map(|p| (p.id.as_str(), p.isolation.as_ref().and_then(|i| i.person.as_deref()))).collect::<Vec<_>>(), [("theirs", Some("user:u2")), ("c-machine", None)]);
        // Asked for nobody, or for one kiln keeps no volume for: the machine's own alone.
        for person in [None, Some("user:../user-u1")] {
            assert_eq!(sc.history(None, 50, &running, person).iter().map(|p| p.id.as_str()).collect::<Vec<_>>(), ["c-machine"], "{person:?}");
        }
        Ok(())
    }

    /// Kiln plan 01a1cd8e862e7169a074db3d5fa33f3d step 17 (goal 65's finding): a conversation of a
    /// person's volume that kiln kept where it ran goes on in that Agentfile; one it kept nothing of
    /// stays as a start would take it.
    #[test]
    fn s17_a_volumes_conversation_goes_on_in_the_agentfile_it_ran_in() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let homes = home::root(dir.path());
        let layout = home::of_kilns(&homes, "user:u1", "claude")?.ok_or_else(|| anyhow::anyhow!("kiln ships no Agentfile for claude"))?;
        let claude = layout.host_env().into_iter().find(|(k, _)| k == "CLAUDE_CONFIG_DIR").map(|(_, v)| PathBuf::from(v)).ok_or_else(|| anyhow::anyhow!("no CLAUDE_CONFIG_DIR"))?;
        let projects = claude.join("projects").join("-work-a");
        std::fs::create_dir_all(&projects)?;
        for id in ["ran-own", "ran-unkept"] {
            let said = serde_json::json!({"type": "user", "sessionId": id, "cwd": "/work/a", "timestamp": "t", "message": {"role": "user", "content": "go on"}});
            std::fs::write(projects.join(format!("{id}.jsonl")), format!("{said}\n"))?;
        }
        let kept = home::RanIn { conversation: "ran-own".into(), agentfile: "agentfile:0123".into(), digest: "ab".repeat(32), image: "agentfile-0123@sha256:00".into(), kept_in: None };
        home::keep_ran_in(&home::ran_in_dir(&homes, "user:u1", "claude")?, &kept)?;
        let mut sc = Scanner::new(dir.path().to_owned()).with_homes(homes.clone());
        let machine = dir.path().join("machine");
        (sc.claude, sc.codex, sc.devin) = (machine.join("claude"), machine.join("codex"), machine.join("devin"));
        sc.running = Some(HashSet::new());
        let past = sc.history(Some("claude"), 50, &HashSet::new(), Some("user:u1"));
        let mut images: Vec<(&str, Option<&str>)> = past.iter().map(|p| (p.id.as_str(), p.isolation.as_ref().and_then(|i| i.image.as_deref()))).collect();
        images.sort_unstable();
        assert_eq!(images, [("ran-own", Some("agentfile:0123")), ("ran-unkept", None)]);
        Ok(())
    }

    /// Kiln plan 01a1cd8e862e7169a074db3d5fa33f3d step 17 part 7: a conversation a person's own
    /// Agentfile kept elsewhere than kiln's does (Claude Code's folder at `/data/claude` in its microVM,
    /// a volume kiln's Agentfile has none of) is found where its record beside their volume says its
    /// harness keeps them (`home::RanIn::kept_in`), and goes on in that Agentfile; never for another
    /// person, nor at a folder a record names outside its person's volume.
    #[test]
    fn s17_a_history_finds_a_conversation_kept_where_a_persons_own_agentfile_keeps_them() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let homes = home::root(dir.path());
        let said = |id: &str| serde_json::json!({"type": "user", "sessionId": id, "cwd": "/work/a", "timestamp": "t", "message": {"role": "user", "content": "go on"}});
        let projects = homes.join("user-u1").join("claude").join("data").join("claude").join("projects").join("-work-a");
        std::fs::create_dir_all(&projects)?;
        std::fs::write(projects.join("own-layout.jsonl"), format!("{}\n", said("own-layout")))?;
        let kept = home::RanIn { conversation: "own-layout".into(), agentfile: "agentfile:0123".into(), digest: "ab".repeat(32), image: "agentfile-0123@sha256:00".into(), kept_in: Some("/data/claude".into()) };
        home::keep_ran_in(&home::ran_in_dir(&homes, "user:u1", "claude")?, &kept)?;
        // A record that names a folder out of its person's volume leads nowhere: u2's folder holds a
        // conversation that a record of u1's would reach by climbing out of u1's.
        let theirs = homes.join("user-u2").join("claude").join("c").join("projects").join("-work-a");
        std::fs::create_dir_all(&theirs)?;
        std::fs::write(theirs.join("theirs.jsonl"), format!("{}\n", said("theirs")))?;
        let climbing = home::RanIn { conversation: "climbing".into(), kept_in: Some("/../../user-u2/claude/c".into()), ..kept.clone() };
        home::keep_ran_in(&home::ran_in_dir(&homes, "user:u1", "claude")?, &climbing)?;
        // The machine's own folders hold nothing.
        let machine = dir.path().join("machine");
        let mut sc = Scanner::new(dir.path().to_owned()).with_homes(homes.clone());
        (sc.claude, sc.codex, sc.devin, sc.running) = (machine.join("claude"), machine.join("codex"), machine.join("devin"), Some(HashSet::new()));
        let mut found = |person: Option<&str>| -> Vec<(String, Option<String>, Option<String>)> {
            let past = sc.history(Some("claude"), 50, &HashSet::new(), person);
            past.into_iter().map(|p| (p.id, p.isolation.as_ref().and_then(|i| i.person.clone()), p.isolation.and_then(|i| i.image))).collect()
        };
        let own = [("own-layout".to_owned(), Some("user:u1".to_owned()), Some("agentfile:0123".to_owned()))];
        assert_eq!(found(Some("user:u1")), own);
        assert_eq!(found(Some("user:u1")), own, "the same, asked again");
        assert!(found(Some("user:u2")).is_empty(), "u1's records are read for u1 alone, and u2 keeps none");
        assert!(found(None).is_empty(), "asked for nobody: the machine's own alone");
        Ok(())
    }

    #[test]
    fn found_sessions_are_taken_over_or_branched() {
        let s = AiSession {
            tool: "claude".into(),
            id: "8b2c".into(),
            pid: 4242,
            name: "fix the build".into(),
            cwd: "/work".into(),
            model: "claude-opus-5-5".into(),
            terminal: Some("term1".into()),
            transcript: true,
            last_prompt: "fix the build".into(),
            ..AiSession::default()
        };
        assert_eq!(s.takeover(true), TakeOver::Ready);
        let spec = s.takeover_spec(true).unwrap_or_default();
        assert_eq!((spec.kind.as_str(), spec.cwd.as_deref(), spec.resume.as_deref(), spec.takeover_pid), ("claude", Some("/work"), Some("8b2c"), Some(4242)));
        assert_eq!((spec.model.as_deref(), spec.name.as_deref()), (Some("claude-opus-5-5"), Some("fix the build")));
        assert!(!spec.takeover_stop && !spec.fork);
        // kiln does not hold its terminal (any more), or it runs outside kiln: kiln ends it first.
        assert_eq!(s.takeover(false), TakeOver::Stop);
        assert_eq!(AiSession { terminal: None, ..s.clone() }.takeover(true), TakeOver::Stop);
        let stop = s.takeover_spec(false).unwrap_or_default();
        assert_eq!((stop.takeover_pid, stop.takeover_stop, stop.resume.as_deref()), (Some(4242), true, Some("8b2c")));
        // An ACP agent might start anew rather than resume it: never ended to find out.
        let acp = AiSession { tool: "gemini".into(), terminal: None, ..s.clone() };
        assert_eq!(acp.takeover(true), TakeOver::Outside);
        assert!(acp.takeover_spec(true).is_none());
        let devin = AiSession { tool: "devin".into(), terminal: None, ..s.clone() };
        assert_eq!(devin.takeover(true), TakeOver::Stop);
        assert!(devin.takeover_spec(true).is_some_and(|s| s.takeover_stop && s.kind == "devin"));
        assert!(!devin.forkable() && devin.fork_spec().is_none());
        // A copy leaves it running: a branch of its conversation, with no process to wait for.
        let fork = s.fork_spec().unwrap_or_default();
        assert_eq!((fork.fork, fork.takeover_pid, fork.resume.as_deref(), fork.name.as_deref()), (true, None, Some("8b2c"), Some("fix the build (branch)")));
        assert!(acp.fork_spec().is_none() && AiSession { id: String::new(), ..s.clone() }.fork_spec().is_none());
        // Brand new: its id is known before its transcript holds a first message.
        for fresh in [AiSession { transcript: false, ..s.clone() }, AiSession { last_prompt: String::new(), ..s.clone() }] {
            assert_eq!((fresh.takeover(true), fresh.takeover(false), fresh.forkable()), (TakeOver::NoId, TakeOver::NoId, false));
        }
        // Nothing to continue yet; a tool kiln cannot run as an agent session. Antigravity is one
        // it can, since its headless mode.
        assert_eq!(AiSession { id: String::new(), ..s.clone() }.takeover(true), TakeOver::NoId);
        assert_eq!(AiSession { tool: "aider".into(), ..s.clone() }.takeover(true), TakeOver::Unsupported);
        assert_eq!(AiSession { tool: "antigravity".into(), ..s.clone() }.takeover(true), TakeOver::Ready);
        assert_eq!(serde_json::to_value(TakeOver::NoId).ok(), Some(serde_json::json!("no_id")));
        assert_eq!(serde_json::to_value(TakeOver::Stop).ok(), Some(serde_json::json!("stop")));
    }

    #[cfg(unix)]
    #[test]
    fn a_session_taken_over_from_outside_is_asked_to_quit_then_killed() -> anyhow::Result<()> {
        use std::os::unix::process::ExitStatusExt as _;
        let mut asked = std::process::Command::new("sleep").arg("30").spawn()?;
        // Not a harness, whatever it was found as: a take-over leaves it alone.
        assert!(!runs(asked.id(), "claude"));
        assert!(end(asked.id(), false));
        assert_eq!(asked.wait()?.signal(), Some(15));
        // Killed, with what it started.
        let mut killed = std::process::Command::new("sh").args(["-c", "sleep 30; true"]).spawn()?;
        std::thread::sleep(Duration::from_millis(200));
        assert!(end(killed.id(), true));
        assert_eq!(killed.wait()?.signal(), Some(9));
        // Never kiln itself.
        assert!(!end(std::process::id(), true) && !end(1, false));
        Ok(())
    }

    /// Goal 9: the runner's reports of its AI sessions wait on what they read, never on a clock:
    /// a scan as the watch starts, one for a change in a tool's folder (Gemini CLI's, as it writes a
    /// chat), one for its terminals changing and none for them said again the same, and none over a
    /// quiet span of seconds (the machine's own tools' ends left out: `os::watch`'s test has a
    /// process's end wake). The planted mistake it catches: the folders' watch left
    /// off (`files` never told) fails at the folder's change.
    #[test]
    fn g9_ai_sessions_scan_on_their_folders_and_terminals_not_a_clock() -> anyhow::Result<()> {
        use std::time::{Duration, Instant};
        crate::rt::block_on(async {
            let dir = tempfile::tempdir()?;
            let home = dir.path().canonicalize()?;
            std::fs::create_dir_all(home.join(".gemini"))?;
            // Its tools' folders under this home whatever the environment says (`CLAUDE_CONFIG_DIR`).
            let mut sc = Scanner::new(home.clone());
            (sc.claude, sc.codex, sc.devin) = (home.join(".claude"), home.join(".codex"), home.join("devin"));
            let r = Reporter { scanner: Arc::new(Mutex::new(sc)), exits: false, ..Reporter::new(home.clone()) };
            let (tx, _rx) = crate::memory::queue::unbounded::<crate::exec::Up>(&crate::memory::owners::CHANNELS);
            let after = |r: &Reporter, n: u64| {
                let r = r.clone();
                async move {
                    let until = Instant::now() + Duration::from_secs(20);
                    while r.scans() <= n && Instant::now() < until {
                        crate::rt::sleep(Duration::from_millis(50)).await;
                    }
                    r.scans() > n
                }
            };
            r.terminals(HashMap::new(), tx.clone());
            anyhow::ensure!(after(&r, 0).await, "no scan as it started");
            crate::rt::sleep(Duration::from_secs(1)).await;
            let first = r.scans();
            crate::rt::sleep(Duration::from_secs(2)).await;
            anyhow::ensure!(r.scans() == first, "{} scans over a quiet span", r.scans() - first);
            std::fs::write(home.join(".gemini").join("chat.json"), b"{}")?;
            anyhow::ensure!(after(&r, first).await, "a change in a tool's folder scanned nothing");
            crate::rt::sleep(Duration::from_secs(1)).await;
            let second = r.scans();
            r.terminals(HashMap::from([(1, "s".to_owned())]), tx.clone());
            anyhow::ensure!(after(&r, second).await, "its terminals changing scanned nothing");
            crate::rt::sleep(Duration::from_secs(1)).await;
            let third = r.scans();
            r.terminals(HashMap::from([(1, "s".to_owned())]), tx.clone());
            crate::rt::sleep(Duration::from_secs(2)).await;
            anyhow::ensure!(r.scans() == third, "its terminals said again the same scanned again");
            Ok(())
        })
    }
}
