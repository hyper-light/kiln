//! One agent session on a machine: the harness process, its adapter and its state. The process
//! lives as long as the session (not as long as any browser); when it exits, the session stays and
//! can be continued where it left off (the harness's own resume), automatically when a person
//! sends it a message.

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::FutureExt as _;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use super::core::{Core, Update};
use super::kinds::Kind;
use super::model::{Command, ExitInfo, State, Status, ToolKind};

/// What to start.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Spec {
    /// Harness id ("claude", "codex", ...).
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// Continue this harness session (its own id) instead of starting a new one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume: Option<String>,
    /// The first message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The terminal session (shell) this agent belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell: Option<String>,
    /// Environment captured from that shell (`kiln spawn`), applied over the machine's own.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<(String, String)>,
    /// Offer the harness's skip-all-permissions mode (it is never the default).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub allow_bypass: bool,
    /// Run this executable instead of the harness's usual one (a custom install, tests).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program: Option<String>,
    /// The effort / reasoning level to start with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// With `resume`: continue only up to this point of that session (a message or turn id).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_at: Option<String>,
    /// With `resume`: a new session that branches off it, leaving it as it is.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub fork: bool,
    /// With `resume`: it takes over that session, found running as this process (which is being
    /// ended): the harness starts only once the process is gone, never beside it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub takeover_pid: Option<u32>,
}

/// A pending approval, as lists show it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ApprovalBrief {
    pub id: String,
    pub tool: ToolKind,
    pub tool_name: String,
    pub title: String,
    pub at: i64,
}

/// A session as lists show it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Info {
    pub id: String,
    pub kind: String,
    pub kind_name: String,
    /// "local" or "agent:<machine>" (set by the server).
    #[serde(default)]
    pub target: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell: Option<String>,
    pub cwd: String,
    pub created_at: i64,
    pub status: Status,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub mode: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_session: Option<String>,
    #[serde(default)]
    pub approvals: Vec<ApprovalBrief>,
    #[serde(default)]
    pub queued: usize,
    #[serde(default)]
    pub tasks_running: usize,
    #[serde(default)]
    pub cost_usd: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<String>,
    pub last_activity: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<ExitInfo>,
    /// What the person first asked for.
    #[serde(default)]
    pub goal: String,
    /// The agent's latest words, shortened.
    #[serde(default)]
    pub summary: String,
    /// What it is doing right now, in plain words.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub doing: Option<String>,
    #[serde(default)]
    pub todos_done: u32,
    #[serde(default)]
    pub todos_total: u32,
    #[serde(default)]
    pub files_changed: u32,
    #[serde(default)]
    pub lines_added: u32,
    #[serde(default)]
    pub lines_removed: u32,
    /// How full the context window is, 0–100, when the harness says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_pct: Option<u8>,
    /// Unix ms the current turn started (None when idle).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_started: Option<i64>,
    /// The plan limits of the account it runs on (as `State::limits`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub limits: Vec<super::model::Limit>,
    /// That account, as `State::account` (never an address).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    /// Tasks it left running with no turn under way (as `State::left_behind`).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub left_behind: u32,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

impl Info {
    /// Refresh the live fields from the session's state.
    pub fn apply(&mut self, s: &State) {
        self.status = s.status;
        self.title.clone_from(&s.title);
        self.model.clone_from(&s.model);
        self.mode.clone_from(&s.mode);
        if !s.cwd.is_empty() {
            self.cwd.clone_from(&s.cwd);
        }
        self.harness_session.clone_from(&s.harness_session);
        self.approvals = s
            .approvals
            .iter()
            .map(|a| ApprovalBrief { id: a.id.clone(), tool: a.tool, tool_name: a.tool_name.clone(), title: a.title.clone(), at: a.at })
            .collect();
        self.queued = s.queue.len();
        self.tasks_running = s.tasks.iter().filter(|t| t.status == "running").count();
        self.cost_usd = s.usage.cost_usd;
        self.activity.clone_from(&s.activity);
        self.exit.clone_from(&s.exit);
        self.goal = super::model::clip_middle(&s.goal, 200).0;
        self.summary = super::model::clip_middle(&s.summary, 300).0;
        self.doing.clone_from(&s.doing);
        let total = u32::try_from(s.todos.len()).unwrap_or(u32::MAX);
        self.todos_total = total;
        self.todos_done = u32::try_from(s.todos.iter().filter(|t| t.status == "completed").count()).unwrap_or(total);
        self.files_changed = s.files_changed;
        self.lines_added = s.lines_added;
        self.lines_removed = s.lines_removed;
        self.context_pct = match (s.usage.context_tokens, s.usage.context_window) {
            (Some(t), Some(w)) if w > 0 => u8::try_from((t.saturating_mul(100) / w).min(100)).ok(),
            _ => None,
        };
        self.turn_started = s.turn_started;
        self.limits.clone_from(&s.limits);
        self.account.clone_from(&s.account);
        self.plan.clone_from(&s.plan);
        self.left_behind = s.count_left_behind();
    }

    /// The parts lists care about changed (activity timestamps aside).
    pub fn differs(&self, o: &Self) -> bool {
        let strip = |i: &Self| Self { last_activity: 0, ..i.clone() };
        strip(self) != strip(o)
    }
}

/// Where a session's updates go (the server's registry, or the uplink to it).
pub type Sink = async_channel::Sender<(String, Update)>;

/// What a session needs from the machine that holds it: where it keeps the files attached to
/// its messages, how it collects ones that arrived separately (uploaded, or sent in pieces from
/// the server), and how it starts another session (a branch).
#[derive(Clone)]
pub struct Files {
    pub dir: std::path::PathBuf,
    pub take: Arc<dyn Fn(&str) -> Option<std::path::PathBuf> + Send + Sync>,
    pub spawn: Arc<dyn Fn(Spec) + Send + Sync>,
}

/// Put a message's attachments on disk, so the harness can read them: each gets a `path`;
/// images keep (or get) their bytes inline for harnesses that take them that way. Blocking.
fn store_attachments(files: &Files, msg: &str, list: Vec<super::model::Attachment>) -> Vec<super::model::Attachment> {
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD;
    let mut out = Vec::new();
    if std::fs::create_dir_all(&files.dir).is_err() {
        return out;
    }
    let tag: String = msg.chars().filter(char::is_ascii_alphanumeric).take(12).collect();
    for (i, mut a) in list.into_iter().enumerate() {
        let name: String = a.name.chars().map(|c| if c.is_alphanumeric() || matches!(c, '.' | '-' | '_') { c } else { '_' }).take(80).collect();
        let name = if name.trim_matches('.').is_empty() { format!("file-{i}") } else { name };
        let path = files.dir.join(format!("{tag}-{i}-{name}"));
        let wrote = match a.file_id.as_deref().and_then(|f| (files.take)(f)) {
            Some(src) => std::fs::rename(&src, &path).or_else(|_| std::fs::copy(&src, &path).map(|_| ())).is_ok(),
            None => match b64.decode(a.data_base64.as_bytes()) {
                Ok(bytes) if !bytes.is_empty() && bytes.len() as u64 <= super::host::ATTACHMENT_MAX => std::fs::write(&path, &bytes).is_ok(),
                _ => false,
            },
        };
        if !wrote {
            tracing::warn!(attachment = %a.name, "could not store an attachment; it is left out");
            continue;
        }
        if a.is_image() {
            if a.data_base64.is_empty()
                && let Ok(bytes) = std::fs::read(&path)
            {
                a.data_base64 = b64.encode(bytes);
            }
        } else {
            a.data_base64.clear();
        }
        a.file_id = None;
        a.path = Some(path.to_string_lossy().into_owned());
        out.push(a);
    }
    out
}

/// A command as the adapter should see it (attachments stored first).
async fn prepare(files: &Files, c: Command) -> Command {
    match c {
        Command::Send { id, text, mode, attachments, overrides } if !attachments.is_empty() => {
            let files = files.clone();
            let msg = id.clone();
            let stored = crate::rt::blocking(move || store_attachments(&files, &msg, attachments)).await.unwrap_or_default();
            Command::Send { id, text, mode, attachments: stored, overrides }
        }
        other => other,
    }
}

enum Ctl {
    Cmd(Box<Command>),
    Close,
}

/// A running session, as its machine holds it.
pub struct Session {
    pub id: String,
    pub spec: Mutex<Spec>,
    pub created_at: i64,
    pub core: Mutex<Core>,
    info: Mutex<Info>,
    ctl: async_channel::Sender<Ctl>,
    /// The process's pid while it runs.
    pub pid: Mutex<Option<u32>>,
}

impl Session {
    pub fn info(&self) -> Info {
        let mut i = self.info.lock().clone();
        i.apply(&self.core.lock().state);
        i
    }

    pub fn command(&self, c: Command) -> bool {
        self.ctl.try_send(Ctl::Cmd(Box::new(c))).is_ok()
    }

    pub fn close(&self) {
        let _ = self.ctl.try_send(Ctl::Close);
    }

    /// Everything a new viewer (or a restarted server) needs, starting with the announcement.
    pub fn snapshot(&self) -> Vec<Update> {
        let mut out = vec![Update::Open { info: Box::new(self.info()) }];
        out.extend(self.core.lock().snapshot(256 * 1024));
        out
    }

    /// A task's output after byte `since` (its last `TASK_TAIL` bytes without one), read where the
    /// harness keeps it: `{data, offset, live, truncated}` (ask from `offset` next), or `{error}`.
    /// Blocking (reads a file).
    pub fn task_output(&self, task: &str, since: Option<u64>) -> serde_json::Value {
        use serde_json::json;
        let (t, kept, session, cwd, env) = {
            let core = self.core.lock();
            (core.state.tasks.iter().find(|x| x.id == task).cloned(), core.task_output.get(task).cloned(), core.state.harness_session.clone(), core.state.cwd.clone(), self.spec.lock().env.clone())
        };
        let Some(t) = t else { return json!({"error": "no such task in this session"}) };
        // Kept by kiln (a harness that streams it).
        if let Some(k) = kept {
            let (bytes, offset, truncated) = k.since(since.unwrap_or_else(|| k.total.saturating_sub(TASK_TAIL)));
            return json!({"data": String::from_utf8_lossy(&bytes), "offset": offset, "live": t.live(), "truncated": truncated});
        }
        // A file the harness writes (Claude Code: while the task lives).
        let Some(path) = t.output.clone().or_else(|| claude_task_file(&env, &cwd, session.as_deref(), task)) else {
            return json!({"error": "this task's output is not kept anywhere kiln can read", "live": t.live()});
        };
        let p = std::path::Path::new(&path);
        let named = p.extension().and_then(|e| e.to_str()) == Some("output") && p.parent().and_then(|d| d.file_name()).and_then(|n| n.to_str()) == Some("tasks");
        if !named || !p.is_absolute() {
            return json!({"error": "not a task output file", "live": t.live()});
        }
        read_tail(p, since).map_or_else(
            || json!({"data": "", "offset": since.unwrap_or(0), "live": t.live(), "gone": true}),
            |(data, offset, truncated)| json!({"data": data, "offset": offset, "live": t.live(), "truncated": truncated, "path": path}),
        )
    }

    /// What resuming this session takes: its spec with the harness's session id to continue.
    pub fn resume_spec(&self) -> Spec {
        let mut s = self.spec.lock().clone();
        let core = self.core.lock();
        if let Some(h) = core.state.harness_session.clone() {
            s.resume = Some(h);
        }
        // Where it is now, not where it started: its own session, settings as last chosen.
        s.prompt = None;
        s.resume_at = None;
        s.fork = false;
        if !core.state.effort.is_empty() {
            s.effort = Some(core.state.effort.clone());
        }
        if !core.state.mode.is_empty() {
            s.mode = Some(core.state.mode.clone());
        }
        if !core.state.model.is_empty() {
            s.model = Some(core.state.model.clone());
        }
        s
    }
}

/// Start a session; its updates go to `sink`, tagged with its id.
pub fn start(id: String, spec: Spec, sink: Sink, on_change: Arc<dyn Fn() + Send + Sync>, files: Files) -> anyhow::Result<Arc<Session>> {
    let kind = Kind::get(&spec.kind).ok_or_else(|| anyhow::anyhow!("unknown harness {:?}", spec.kind))?;
    let now = crate::util::now_ms();
    let cwd = spec.cwd.clone().unwrap_or_else(|| crate::os::home_dir().to_string_lossy().into_owned());
    let mut core = Core::new();
    core.state.cwd.clone_from(&cwd);
    core.state.model = spec.model.clone().unwrap_or_default();
    core.state.mode = spec.mode.clone().unwrap_or_default();
    core.state.harness_session.clone_from(&spec.resume);
    let info = Info {
        id: id.clone(),
        kind: kind.id.to_owned(),
        kind_name: kind.name.to_owned(),
        target: String::new(),
        name: spec.name.clone().filter(|n| !n.trim().is_empty()).unwrap_or_else(|| kind.name.to_owned()),
        shell: spec.shell.clone(),
        cwd,
        created_at: now,
        status: Status::Starting,
        title: String::new(),
        model: String::new(),
        mode: String::new(),
        harness_session: spec.resume.clone(),
        approvals: Vec::new(),
        queued: 0,
        tasks_running: 0,
        cost_usd: 0.0,
        activity: None,
        last_activity: now,
        exit: None,
        goal: String::new(),
        summary: String::new(),
        doing: None,
        todos_done: 0,
        todos_total: 0,
        files_changed: 0,
        lines_added: 0,
        lines_removed: 0,
        context_pct: None,
        turn_started: None,
        limits: Vec::new(),
        account: None,
        plan: None,
        left_behind: 0,
    };
    let (ctl, ctl_rx) = async_channel::unbounded();
    let s = Arc::new(Session {
        id,
        spec: Mutex::new(spec),
        created_at: now,
        core: Mutex::new(core),
        info: Mutex::new(info),
        ctl,
        pid: Mutex::new(None),
    });
    let run = Arc::clone(&s);
    crate::rt::spawn(async move {
        let id = run.id.clone();
        if let Err(p) = std::panic::AssertUnwindSafe(drive(Arc::clone(&run), kind, ctl_rx, sink.clone(), on_change, files)).catch_unwind().await {
            tracing::error!(session = %id, panic = %crate::sup::panic_message(p.as_ref()), "agent session task panicked");
        }
    });
    Ok(s)
}

/// Background work a step asked for.
struct Jobs {
    previews: Vec<super::core::PreviewJob>,
    changes: Vec<super::core::ChangeJob>,
}

/// Hand out what the core produced: updates to the sink, lines to the process.
async fn flush(s: &Session, sink: &Sink, stdin: Option<&async_channel::Sender<Vec<u8>>>) -> Jobs {
    let (updates, writes, previews, changes) = {
        let mut core = s.core.lock();
        let u = core.take_out();
        (u, core.take_writes(), std::mem::take(&mut core.previews), std::mem::take(&mut core.change_jobs))
    };
    if let Some(stdin) = stdin {
        for w in writes {
            let mut line = w.into_bytes();
            line.push(b'\n');
            let _ = stdin.send(line).await;
        }
    }
    if !updates.is_empty() {
        s.info.lock().last_activity = crate::util::now_ms();
    }
    for u in updates {
        if sink.send((s.id.clone(), u)).await.is_err() {
            break;
        }
    }
    Jobs { previews, changes }
}

enum Ev {
    Change(Option<(String, (Option<super::model::FileChange>, Option<super::core::Baseline>))>),
    Out(Option<Vec<u8>>),
    Err(Option<Vec<u8>>),
    Ctl(Option<Ctl>),
    Preview(Option<(String, Vec<super::model::FileDiff>)>),
    Exit(Option<i32>),
    Fx(Option<Done>),
    Tick,
}

/// What an effect carried out off the event loop came back with.
enum Done {
    /// A run (or a fetch) ended.
    Run { job: String, code: Option<i32>, output: String },
    /// The answer for one client.
    Reply { client: String, value: serde_json::Value },
}

/// Longest a run of the harness's CLI may take.
const RUN_TIMEOUT: Duration = Duration::from_secs(120);
/// Most of a run's output handed back.
const RUN_OUTPUT_MAX: usize = 32 * 1024;
/// Largest file a read hands back.
const READ_MAX: u64 = 512 * 1024;
/// How often the adapter gets a tick while the process runs.
const TICK: Duration = Duration::from_secs(5);
/// How long a take-over waits for the session it takes over to end.
const TAKEOVER_WAIT: Duration = Duration::from_secs(15);

/// Most of a task's output returned at once, and the tail returned without an offset.
const TASK_READ_MAX: u64 = 256 * 1024;
const TASK_TAIL: u64 = 64 * 1024;

/// Where Claude Code streams a task's output while it lives: `<tmp>/<cwd as a name>/<session>/
/// tasks/<task>.output`, `<tmp>` being `CLAUDE_CODE_TMPDIR` or the system's `claude-<uid>`.
fn claude_task_file(env: &[(String, String)], cwd: &str, session: Option<&str>, task: &str) -> Option<String> {
    let session = session.filter(|s| !s.is_empty() && !s.contains(['/', '\\']))?;
    if task.is_empty() || task.contains(['/', '\\', '.']) {
        return None;
    }
    let root = env
        .iter()
        .rev()
        .find(|(k, _)| k == "CLAUDE_CODE_TMPDIR")
        .map(|(_, v)| std::path::PathBuf::from(v))
        .or_else(|| std::env::var_os("CLAUDE_CODE_TMPDIR").map(std::path::PathBuf::from))
        .unwrap_or_else(|| std::env::temp_dir().join(format!("claude-{}", crate::os::uid())));
    let dir: String = cwd.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    Some(root.join(dir).join(session).join("tasks").join(format!("{task}.output")).to_string_lossy().into_owned())
}

/// A file's bytes after `since` (its tail without one, the start again when it shrank below
/// `since`), at most `TASK_READ_MAX`: (text, the offset after them, whether older bytes were
/// skipped). None when it cannot be read. Blocking.
fn read_tail(p: &std::path::Path, since: Option<u64>) -> Option<(String, u64, bool)> {
    use std::io::{Read as _, Seek as _, SeekFrom};
    let mut f = std::fs::File::open(p).ok()?;
    let len = f.metadata().ok()?.len();
    let want = match since {
        Some(s) if s <= len => s,
        Some(_) => 0,
        None => len.saturating_sub(TASK_TAIL),
    };
    let start = want.max(len.saturating_sub(TASK_READ_MAX));
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::new();
    f.take(len - start).read_to_end(&mut buf).ok()?;
    Some((String::from_utf8_lossy(&buf).into_owned(), start + buf.len() as u64, start > want || (since.is_none() && start > 0)))
}

/// The processes under `root`, newest first: (pid, command line). Blocking.
fn descendants(root: u32) -> Vec<(u32, Vec<String>)> {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
    let mut sys = System::new();
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing().with_cmd(UpdateKind::Always).without_tasks());
    let mut kids: std::collections::HashMap<Pid, Vec<Pid>> = std::collections::HashMap::new();
    for (pid, p) in sys.processes() {
        if let Some(parent) = p.parent() {
            kids.entry(parent).or_default().push(*pid);
        }
    }
    let mut out: Vec<(u64, u32, Vec<String>)> = Vec::new();
    let mut todo = vec![Pid::from_u32(root)];
    let mut seen = std::collections::HashSet::new();
    while let Some(p) = todo.pop() {
        for c in kids.get(&p).into_iter().flatten() {
            if seen.insert(*c) {
                todo.push(*c);
                if let Some(proc_) = sys.process(*c) {
                    out.push((proc_.start_time(), c.as_u32(), proc_.cmd().iter().map(|a| a.to_string_lossy().into_owned()).collect()));
                }
            }
        }
    }
    out.sort_by_key(|(start, _, _)| std::cmp::Reverse(*start));
    out.into_iter().map(|(_, pid, cmd)| (pid, cmd)).collect()
}

/// Stop the newest process under `root` that runs `command` (its command line, or the last
/// argument of a `shell -c` one), with what it started: whether one was found. Blocking.
fn kill_command(root: u32, command: &str) -> bool {
    let want = command.trim();
    if want.is_empty() {
        return false;
    }
    let all = descendants(root);
    let found = all.iter().find(|(_, cmd)| cmd.join(" ") == want || cmd.last().is_some_and(|a| a.trim() == want));
    let Some((pid, _)) = found else { return false };
    let mut pids: Vec<u32> = vec![*pid];
    pids.extend(descendants(*pid).into_iter().map(|(p, _)| p));
    crate::os::stop_processes(&pids, Some(root), Duration::from_secs(2));
    true
}

/// Wait (up to `wait`) for `pid` to be gone: whether it is.
async fn gone(pid: u32, wait: Duration) -> bool {
    let end = Instant::now() + wait;
    while crate::os::pid_alive(pid) {
        if Instant::now() >= end {
            return false;
        }
        crate::rt::sleep(Duration::from_millis(200)).await;
    }
    true
}

/// A take-over waits for the process it takes over to end (its terminal is being closed): an
/// error to show when it has not, `None` once it is gone (and the spec forgets it).
async fn takeover_ready(s: &Session, sink: &Sink, wait: Duration) -> Option<String> {
    let pid = s.spec.lock().takeover_pid?;
    // Said in the session's state, not its timeline (which its history fills once it is free).
    let waiting = crate::os::pid_alive(pid);
    if waiting {
        {
            let mut core = s.core.lock();
            core.state.doing = Some(format!("Waiting for the session it takes over (process {pid}) to end"));
            core.touch_state();
        }
        flush(s, sink, None).await;
    }
    let free = gone(pid, wait).await;
    if waiting {
        let mut core = s.core.lock();
        core.state.doing = None;
        core.touch_state();
    }
    if free {
        s.spec.lock().takeover_pid = None;
        return None;
    }
    Some(format!("The session found running as process {pid} has not ended, so kiln did not start a second one beside it. Quit it, then restart this agent."))
}

impl Done {
    fn apply(self, adapter: &mut super::kinds::Adapter, core: &mut Core) {
        match self {
            Self::Run { job, code, output } => adapter.run_done(core, &job, code, &output),
            Self::Reply { client, value } => core.reply(client, value),
        }
    }
}

/// Carry out what the adapter asked for. A branch starts another session; runs, reads and fetches
/// go off the event loop and come back on `done`. A restart is the caller's: its point (the
/// harness's own resume point, if any) is returned.
fn effects(s: &Arc<Session>, kind: &'static Kind, files: &Files, done: &async_channel::Sender<Done>) -> Option<Option<String>> {
    let effects = std::mem::take(&mut s.core.lock().effects);
    let mut restart = None;
    for e in effects {
        match e {
            super::core::Effect::Branch { resume, resume_at } => {
                let mut b = s.resume_spec();
                b.resume = Some(resume);
                b.resume_at = resume_at;
                b.fork = true;
                b.name = Some(format!("{} (branch)", s.info().name));
                (files.spawn)(b);
            }
            super::core::Effect::Restart { resume_at } => restart = Some(resume_at),
            super::core::Effect::Run { job, args } => {
                let mut spec = s.spec.lock().clone();
                let cwd = s.core.lock().state.cwd.clone();
                if !cwd.is_empty() {
                    spec.cwd = Some(cwd);
                }
                let tx = done.clone();
                crate::rt::spawn(async move {
                    let (code, output) = match kind.cli(&spec, &args) {
                        Err(e) => (None, format!("{e:#}")),
                        Ok(cmd) => match crate::proc::output(cmd, RUN_TIMEOUT).await {
                            Ok(o) => {
                                let mut text = String::from_utf8_lossy(&o.stdout).into_owned();
                                let err = String::from_utf8_lossy(&o.stderr);
                                if !err.trim().is_empty() {
                                    if !text.is_empty() && !text.ends_with('\n') {
                                        text.push('\n');
                                    }
                                    text.push_str(&err);
                                }
                                (o.status.code(), super::model::clip_middle(text.trim(), RUN_OUTPUT_MAX).0)
                            }
                            Err(e) => (None, format!("{e:#}")),
                        },
                    };
                    let _ = tx.send(Done::Run { job, code, output }).await;
                });
            }
            super::core::Effect::Read { client, paths } => {
                let tx = done.clone();
                crate::rt::spawn(async move {
                    let value = crate::rt::blocking(move || read_first(&paths)).await.unwrap_or_else(|e| serde_json::json!({"error": format!("{e:#}")}));
                    let _ = tx.send(Done::Reply { client, value }).await;
                });
            }
            super::core::Effect::Fetch { job, url } => {
                let tx = done.clone();
                crate::rt::spawn(async move {
                    let (code, output) = crate::rt::blocking(move || fetch_loopback(&url)).await.unwrap_or_else(|e| (None, format!("{e:#}")));
                    let _ = tx.send(Done::Run { job, code, output }).await;
                });
            }
            super::core::Effect::Kill { job, command } => {
                let tx = done.clone();
                let root = *s.pid.lock();
                crate::rt::spawn(async move {
                    let killed = match root {
                        Some(root) => crate::rt::blocking(move || kill_command(root, &command)).await.unwrap_or(false),
                        None => false,
                    };
                    let (code, output) = if killed { (Some(0), String::new()) } else { (None, "it is not running any more".to_owned()) };
                    let _ = tx.send(Done::Run { job, code, output }).await;
                });
            }
        }
    }
    restart
}

/// A path with `*` components, matched against the directories there (the most recently changed
/// first); a path without them, as it is.
fn expand(path: &str) -> Vec<String> {
    let p = std::path::Path::new(path);
    let mut out: Vec<std::path::PathBuf> = vec![std::path::PathBuf::new()];
    for part in p.components() {
        if part.as_os_str() == "*" {
            let mut next = Vec::new();
            for base in &out {
                let Ok(entries) = std::fs::read_dir(base) else { continue };
                let mut dirs: Vec<(std::time::SystemTime, std::path::PathBuf)> = entries
                    .filter_map(Result::ok)
                    .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                    .map(|e| (e.metadata().and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH), e.path()))
                    .collect();
                dirs.sort_by_key(|d| std::cmp::Reverse(d.0));
                next.extend(dirs.into_iter().take(64).map(|(_, d)| d));
            }
            out = next;
        } else {
            for o in &mut out {
                o.push(part);
            }
        }
    }
    out.into_iter().map(|p| p.to_string_lossy().into_owned()).collect()
}

/// The first of these Markdown files (skills, commands) that can be read: `{"content", "path"}`,
/// else `{"error"}`. `*` stands for any directory (see `expand`). Blocking.
fn read_first(paths: &[String]) -> serde_json::Value {
    for p in paths.iter().flat_map(|p| expand(p)) {
        let p = &p;
        let path = std::path::Path::new(p);
        if !path.is_absolute() || path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref() != Some("md") {
            continue;
        }
        let Ok(meta) = std::fs::metadata(path) else { continue };
        if !meta.is_file() {
            continue;
        }
        if meta.len() > READ_MAX {
            return serde_json::json!({"error": format!("{p} is too large to show here"), "path": p});
        }
        if let Ok(bytes) = std::fs::read(path) {
            return serde_json::json!({"content": String::from_utf8_lossy(&bytes), "path": p});
        }
    }
    serde_json::json!({"error": "The file is not on this machine (any more)."})
}

/// GET a URL on this machine's loopback (a sign-in callback a harness waits for): the HTTP status
/// and the start of the answer, or None and why it failed. Nothing but loopback is fetched.
/// Blocking.
fn fetch_loopback(url: &str) -> (Option<i32>, String) {
    use std::io::{Read as _, Write as _};
    let Some((host, port, target)) = loopback_target(url) else {
        return (None, "Only an http://localhost or http://127.0.0.1 address can be completed on the agent's machine.".to_owned());
    };
    let addrs: Vec<std::net::SocketAddr> = match host.as_str() {
        "::1" => vec![std::net::SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, port))],
        "127.0.0.1" => vec![std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, port))],
        _ => vec![std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, port)), std::net::SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, port))],
    };
    let mut last = String::from("nothing is listening there");
    for addr in addrs {
        let mut conn = match std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(5)) {
            Ok(c) => c,
            Err(e) => {
                last = e.to_string();
                continue;
            }
        };
        let _ = conn.set_read_timeout(Some(Duration::from_secs(15)));
        let _ = conn.set_write_timeout(Some(Duration::from_secs(5)));
        let host_header = if host == "::1" { format!("[::1]:{port}") } else { format!("{host}:{port}") };
        let req = format!("GET {target} HTTP/1.1\r\nHost: {host_header}\r\nUser-Agent: kiln\r\nAccept: */*\r\nConnection: close\r\n\r\n");
        if let Err(e) = conn.write_all(req.as_bytes()) {
            last = e.to_string();
            continue;
        }
        let mut answer = Vec::new();
        let _ = conn.take(64 * 1024).read_to_end(&mut answer);
        let text = String::from_utf8_lossy(&answer);
        let code = text.lines().next().and_then(|l| l.split_whitespace().nth(1)).and_then(|c| c.parse::<i32>().ok());
        let body = text.split_once("\r\n\r\n").map_or("", |(_, b)| b);
        return (code, super::model::clip_middle(body.trim(), 2000).0);
    }
    (None, format!("Could not reach {host}:{port} on the agent's machine: {last}"))
}

/// (host, port, request target) of an http URL on loopback; None for anything else.
fn loopback_target(url: &str) -> Option<(String, u16, String)> {
    let rest = url.trim().strip_prefix("http://")?;
    let split = rest.find(['/', '?']).unwrap_or(rest.len());
    let (authority, target) = rest.split_at(split);
    let target = target.split('#').next().unwrap_or("");
    let target = match target {
        "" => "/".to_owned(),
        t if t.starts_with('?') => format!("/{t}"),
        t => t.to_owned(),
    };
    if !target.bytes().all(|b| b.is_ascii_graphic()) || authority.contains('@') {
        return None;
    }
    let (host, port) = match authority.strip_prefix('[') {
        Some(v6) => {
            let (h, p) = v6.split_once(']')?;
            (h.to_owned(), p.strip_prefix(':').map_or(Some(80), |p| p.parse().ok())?)
        }
        None => match authority.split_once(':') {
            Some((h, p)) => (h.to_owned(), p.parse().ok()?),
            None => (authority.to_owned(), 80),
        },
    };
    let host = host.to_ascii_lowercase();
    matches!(host.as_str(), "localhost" | "127.0.0.1" | "::1").then_some((host, port, target))
}

/// The session's life: start the process, pump it, and after it exits wait to be restarted.
async fn drive(s: Arc<Session>, kind: &'static Kind, ctl_rx: async_channel::Receiver<Ctl>, sink: Sink, on_change: Arc<dyn Fn() + Send + Sync>, files: Files) {
    let (pv_tx, pv_rx) = async_channel::unbounded::<(String, Vec<super::model::FileDiff>)>();
    let (ch_tx, ch_rx) = async_channel::unbounded::<(String, (Option<super::model::FileChange>, Option<super::core::Baseline>))>();
    let (fx_tx, fx_rx) = async_channel::unbounded::<Done>();
    // Taking over a session: its transcript is read once it has ended (checked again before each
    // start while it has not).
    let _ = takeover_ready(&s, &sink, TAKEOVER_WAIT).await;
    // A resumed Claude Code session starts from its transcript (its history), not empty.
    if kind.protocol == super::kinds::Protocol::ClaudeStreamJson {
        let (resume, env, upto) = {
            let sp = s.spec.lock();
            (sp.resume.clone(), sp.env.clone(), sp.resume_at.clone())
        };
        let empty = s.core.lock().events().next().is_none();
        if let Some(r) = resume.filter(|_| empty) {
            let session = r.clone();
            match crate::rt::blocking(move || super::transcript::history(&env, &r)).await {
                Ok(Some(lines)) => {
                    let mut core = s.core.lock();
                    super::transcript::load_upto(&mut core, &lines, upto.as_deref());
                    tracing::info!(id = %s.id, %session, events = core.events().count(), "agent session history loaded from its transcript");
                }
                _ => tracing::info!(id = %s.id, %session, "no transcript found for the resumed agent session"),
            }
        }
    }
    // Announce it, whole, before anything else: whoever mirrors it starts from all of it.
    for u in s.snapshot() {
        let _ = sink.send((s.id.clone(), u)).await;
    }
    let mut first = true;
    let mut crashes: Vec<Instant> = Vec::new();
    // A message that arrived while the process was down, sent once it is back.
    let mut carry: Vec<Command> = Vec::new();
    // A rewind: the harness starts again, continuing its session from this point.
    let mut rewind_to: Option<Option<String>> = None;
    loop {
        let mut spec = if first { s.spec.lock().clone() } else { s.resume_spec() };
        if let Some(at) = rewind_to.take() {
            spec.resume_at = at;
        }
        let mut adapter = kind.adapter(&spec);
        let started = Instant::now();
        let blocked = if spec.takeover_pid.is_some() { takeover_ready(&s, &sink, Duration::from_secs(3)).await } else { None };
        let spawned = match blocked {
            Some(why) => Err(anyhow::anyhow!(why)),
            None => kind.launch(&spec).and_then(crate::proc::spawn_duplex),
        };
        let mut code = None;
        let mut error = None;
        match spawned {
            Err(e) => error = Some(format!("{e:#}")),
            Ok(mut p) => {
                *s.pid.lock() = Some(p.pid);
                {
                    let mut core = s.core.lock();
                    core.state.exit = None;
                    core.set_status(Status::Starting);
                    core.touch_state();
                    adapter.start(&mut core);
                    if first && let Some(prompt) = spec.prompt.clone().filter(|p| !p.trim().is_empty()) {
                        adapter.command(&mut core, Command::Send { id: crate::util::new_id(), text: prompt, mode: Default::default(), attachments: Vec::new(), overrides: None });
                    }
                    for c in carry.drain(..) {
                        adapter.command(&mut core, c);
                    }
                }
                on_change();
                let mut jobs = flush(&s, &sink, Some(&p.stdin)).await;
                let mut buf: Vec<u8> = Vec::new();
                let mut stderr_tail: Vec<u8> = Vec::new();
                let mut out_open = true;
                let mut err_open = true;
                let mut last_session = s.core.lock().state.harness_session.clone();
                let mut last_queue = 0usize;
                let mut next_tick = Instant::now() + TICK;
                loop {
                    for job in jobs.previews.drain(..) {
                        let tx = pv_tx.clone();
                        crate::rt::spawn(async move {
                            let id = job.approval.clone();
                            let diffs = crate::rt::blocking(move || super::claude::preview(&job.tool, &job.input, &job.cwd)).await.unwrap_or_default();
                            let _ = tx.send((id, diffs)).await;
                        });
                    }
                    for job in jobs.changes.drain(..) {
                        let tx = ch_tx.clone();
                        crate::rt::spawn(async move {
                            let abs = job.abs.clone();
                            let change = crate::rt::blocking(move || super::diff::file_change(&job)).await.unwrap_or((None, None));
                            let _ = tx.send((abs, change)).await;
                        });
                    }
                    let ev = futures::select_biased! {
                        c = ctl_rx.recv().fuse() => Ev::Ctl(c.ok()),
                        o = p.stdout.recv().fuse() => if out_open { Ev::Out(o.ok()) } else { Ev::Out(None) },
                        e = p.stderr.recv().fuse() => if err_open { Ev::Err(e.ok()) } else { Ev::Err(None) },
                        d = pv_rx.recv().fuse() => Ev::Preview(d.ok()),
                        c = ch_rx.recv().fuse() => Ev::Change(c.ok()),
                        f = fx_rx.recv().fuse() => Ev::Fx(f.ok()),
                        x = (&mut p.exit).fuse() => Ev::Exit(x.ok().flatten()),
                        _ = crate::rt::sleep_until(next_tick).fuse() => Ev::Tick,
                    };
                    match ev {
                        Ev::Out(Some(chunk)) => {
                            buf.extend_from_slice(&chunk);
                            let mut core = s.core.lock();
                            while let Some(nl) = buf.iter().position(|b| *b == b'\n') {
                                let line: Vec<u8> = buf.drain(..=nl).collect();
                                adapter.line(&mut core, &String::from_utf8_lossy(&line));
                            }
                            if buf.len() > 32 << 20 {
                                tracing::warn!(session = %s.id, "dropping an oversized line from the harness");
                                buf.clear();
                            }
                        }
                        Ev::Out(None) => {
                            out_open = false;
                            if !err_open {
                                // Both pipes closed: the exit status follows.
                                code = (&mut p.exit).await.ok().flatten();
                                break;
                            }
                        }
                        Ev::Err(Some(chunk)) => {
                            stderr_tail.extend_from_slice(&chunk);
                            let excess = stderr_tail.len().saturating_sub(8192);
                            stderr_tail.drain(..excess);
                        }
                        Ev::Err(None) => {
                            err_open = false;
                            if !out_open {
                                code = (&mut p.exit).await.ok().flatten();
                                break;
                            }
                        }
                        Ev::Ctl(Some(Ctl::Cmd(c))) => match *c {
                            Command::Restart => {}
                            Command::Snapshot => {
                                for u in s.snapshot() {
                                    let _ = sink.send((s.id.clone(), u)).await;
                                }
                            }
                            c => {
                                let c = prepare(&files, c).await;
                                let mut core = s.core.lock();
                                adapter.command(&mut core, c);
                            }
                        },
                        Ev::Ctl(Some(Ctl::Close)) | Ev::Ctl(None) => {
                            // Ending the agent ends what it left running: stopped through the
                            // harness first, then whatever is still under it (shells and
                            // terminals that left its process group included), then the harness.
                            if s.core.lock().state.tasks.iter().any(super::model::Task::live) {
                                adapter.command(&mut s.core.lock(), Command::StopTasks);
                                let _ = flush(&s, &sink, Some(&p.stdin)).await;
                                crate::rt::sleep(Duration::from_millis(1500)).await;
                            }
                            let root = p.pid;
                            let _ = crate::rt::blocking(move || {
                                let pids: Vec<u32> = descendants(root).into_iter().map(|(pid, _)| pid).collect();
                                crate::os::stop_processes(&pids, Some(root), Duration::from_secs(2));
                            })
                            .await;
                            p.terminate(Duration::from_secs(3));
                            *s.pid.lock() = None;
                            let _ = sink.send((s.id.clone(), Update::Closed)).await;
                            on_change();
                            return;
                        }
                        Ev::Preview(Some((id, diffs))) => {
                            let mut core = s.core.lock();
                            if let Some(a) = core.state.approvals.iter_mut().find(|a| a.id == id) {
                                a.diffs = diffs;
                                core.touch_state();
                            }
                        }
                        Ev::Preview(None) | Ev::Change(None) => {}
                        Ev::Change(Some((abs, (change, resolved)))) => {
                            s.core.lock().set_change(&abs, change, resolved);
                        }
                        Ev::Fx(Some(d)) => d.apply(&mut adapter, &mut s.core.lock()),
                        Ev::Fx(None) => {}
                        Ev::Tick => {}
                        Ev::Exit(c) => {
                            code = c;
                            // Read what is left on stdout (the last lines often explain the exit).
                            while let Ok(Some(chunk)) = crate::rt::timeout(Duration::from_millis(300), p.stdout.recv()).await.map(Result::ok) {
                                buf.extend_from_slice(&chunk);
                            }
                            {
                                let mut core = s.core.lock();
                                while let Some(nl) = buf.iter().position(|b| *b == b'\n') {
                                    let line: Vec<u8> = buf.drain(..=nl).collect();
                                    adapter.line(&mut core, &String::from_utf8_lossy(&line));
                                }
                            }
                            break;
                        }
                    }
                    // Polls the harness only answers when asked: every few seconds, however busy.
                    if Instant::now() >= next_tick {
                        next_tick = Instant::now() + TICK;
                        adapter.tick(&mut s.core.lock());
                    }
                    jobs = flush(&s, &sink, Some(&p.stdin)).await;
                    if let Some(at) = effects(&s, kind, &files, &fx_tx) {
                        rewind_to = Some(at);
                    }
                    if rewind_to.is_some() {
                        p.terminate(Duration::from_secs(2));
                        let _ = crate::rt::timeout(Duration::from_secs(5), &mut p.exit).await;
                        break;
                    }
                    let (session, queue) = {
                        let core = s.core.lock();
                        (core.state.harness_session.clone(), core.state.queue.len())
                    };
                    if session != last_session || queue != last_queue {
                        last_session = session;
                        last_queue = queue;
                        on_change();
                    }
                }
                *s.pid.lock() = None;
                if rewind_to.is_some() {
                    first = false;
                    continue;
                }
                let tail = String::from_utf8_lossy(&stderr_tail).trim().to_owned();
                if !tail.is_empty() && code != Some(0) {
                    error = Some(super::model::clip_middle(&tail, 2000).0);
                }
            }
        }
        {
            let mut core = s.core.lock();
            adapter.exited(&mut core, code, error.clone());
        }
        flush(&s, &sink, None).await;
        on_change();
        first = false;
        // A crash after a good run comes back by itself (a few times); a harness that will not
        // start waits for a person.
        let ran = started.elapsed();
        crashes.retain(|t| t.elapsed() < Duration::from_secs(600));
        let auto = code != Some(0) && error.is_none() && ran > Duration::from_secs(20) && crashes.len() < 3;
        if auto {
            crashes.push(Instant::now());
            crate::rt::sleep(Duration::from_secs(2)).await;
            continue;
        }
        // Down: a restart, or a message (which restarts it and is then delivered), brings it back.
        // Other commands still get answers (a skill's file, an MCP server added with the CLI).
        loop {
            let ev = futures::select_biased! {
                c = ctl_rx.recv().fuse() => Ev::Ctl(c.ok()),
                f = fx_rx.recv().fuse() => Ev::Fx(f.ok()),
            };
            match ev {
                Ev::Ctl(Some(Ctl::Cmd(c))) => match *c {
                    Command::Restart => break,
                    Command::Snapshot => {
                        for u in s.snapshot() {
                            let _ = sink.send((s.id.clone(), u)).await;
                        }
                    }
                    c @ (Command::Send { .. } | Command::SkillRun { .. }) => {
                        carry.push(prepare(&files, c).await);
                        break;
                    }
                    c => adapter.command(&mut s.core.lock(), c),
                },
                Ev::Ctl(Some(Ctl::Close) | None) => {
                    let _ = sink.send((s.id.clone(), Update::Closed)).await;
                    on_change();
                    return;
                }
                Ev::Fx(Some(d)) => d.apply(&mut adapter, &mut s.core.lock()),
                _ => {}
            }
            flush(&s, &sink, None).await;
            if let Some(at) = effects(&s, kind, &files, &fx_tx) {
                rewind_to = Some(at);
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn info_follows_state() {
        let mut i = Info {
            id: "a".into(),
            kind: "claude".into(),
            kind_name: "Claude Code".into(),
            target: "local".into(),
            name: "x".into(),
            shell: None,
            cwd: "/w".into(),
            created_at: 1,
            status: Status::Starting,
            title: String::new(),
            model: String::new(),
            mode: String::new(),
            harness_session: None,
            approvals: vec![],
            queued: 0,
            tasks_running: 0,
            cost_usd: 0.0,
            activity: None,
            last_activity: 1,
            exit: None,
            goal: String::new(),
            summary: String::new(),
            doing: None,
            todos_done: 0,
            todos_total: 0,
            files_changed: 0,
            lines_added: 0,
            lines_removed: 0,
            context_pct: None,
            turn_started: None,
            limits: vec![],
            account: None,
            plan: None,
            left_behind: 0,
        };
        let before = i.clone();
        let mut st = State { status: Status::Running, mode: "plan".into(), ..State::default() };
        st.tasks.push(super::super::model::Task { id: "t".into(), status: "running".into(), ..Default::default() });
        i.apply(&st);
        assert_eq!((i.status, i.mode.as_str(), i.tasks_running, i.cwd.as_str()), (Status::Running, "plan", 1, "/w"));
        assert!(i.differs(&before));
        let mut later = i.clone();
        later.last_activity = 99;
        assert!(!later.differs(&i));
    }

    #[test]
    fn only_loopback_is_fetched() {
        assert_eq!(loopback_target("http://localhost:41234/callback?code=a&state=b"), Some(("localhost".into(), 41234, "/callback?code=a&state=b".into())));
        assert_eq!(loopback_target("http://127.0.0.1:9/?x=1#frag"), Some(("127.0.0.1".into(), 9, "/?x=1".into())));
        assert_eq!(loopback_target("http://[::1]:7/cb"), Some(("::1".into(), 7, "/cb".into())));
        assert_eq!(loopback_target("http://LOCALHOST?code=1"), Some(("localhost".into(), 80, "/?code=1".into())));
        for bad in ["https://localhost/cb", "http://example.com/cb", "http://localhost.example.com/cb", "http://user@localhost/cb", "http://localhost:1/a b", "http://localhost:99999/", "file:///etc/passwd", "http://10.0.0.1/"] {
            assert_eq!(loopback_target(bad), None, "{bad}");
        }
        assert_eq!(fetch_loopback("http://example.com/").0, None);
    }

    #[test]
    fn a_callback_on_loopback_is_fetched() -> anyhow::Result<()> {
        use std::io::{BufRead as _, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        let server = std::thread::spawn(move || -> Option<String> {
            let (conn, _) = listener.accept().ok()?;
            let mut reader = std::io::BufReader::new(conn.try_clone().ok()?);
            let mut first = String::new();
            reader.read_line(&mut first).ok()?;
            let mut conn = conn;
            conn.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\nsigned in").ok()?;
            Some(first)
        });
        let (code, body) = fetch_loopback(&format!("http://127.0.0.1:{port}/callback?code=abc&state=s"));
        assert_eq!((code, body.as_str()), (Some(200), "signed in"));
        let first = server.join().map_err(|_| anyhow::anyhow!("server thread"))?.unwrap_or_default();
        assert_eq!(first.trim_end(), "GET /callback?code=abc&state=s HTTP/1.1");
        Ok(())
    }

    #[test]
    fn reads_are_markdown_files_only() -> anyhow::Result<()> {
        let dir = std::env::temp_dir().join(format!("kiln-read-{}", crate::util::new_id()));
        std::fs::create_dir_all(dir.join("skill"))?;
        let md = dir.join("skill").join("SKILL.md");
        std::fs::write(&md, "# hi")?;
        let secret = dir.join("secret.txt");
        std::fs::write(&secret, "no")?;
        let p = |x: &std::path::Path| x.to_string_lossy().into_owned();
        let missing = dir.join("missing").join("SKILL.md");
        assert_eq!(read_first(&[p(&missing), p(&md)]).get("content").and_then(serde_json::Value::as_str), Some("# hi"));
        assert!(read_first(&[p(&secret)]).get("error").is_some());
        assert!(read_first(&["relative/SKILL.md".to_owned()]).get("error").is_some());
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    #[test]
    fn a_star_matches_the_newest_directory_first() -> anyhow::Result<()> {
        let dir = std::env::temp_dir().join(format!("kiln-expand-{}", crate::util::new_id()));
        for (bucket, text) in [("old", "old"), ("new", "new")] {
            std::fs::create_dir_all(dir.join("synced").join(bucket).join("docs"))?;
            std::fs::write(dir.join("synced").join(bucket).join("docs").join("SKILL.md"), text)?;
            std::thread::sleep(Duration::from_millis(20));
        }
        let pattern = dir.join("synced").join("*").join("docs").join("SKILL.md").to_string_lossy().into_owned();
        assert_eq!(read_first(&[pattern]).get("content").and_then(serde_json::Value::as_str), Some("new"));
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn a_takeover_waits_for_the_process_to_end() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let mut short = std::process::Command::new("sleep").arg("0.3").spawn()?;
            let pid = short.id();
            // Reaped as soon as it exits (as the taken-over session's parent would).
            let reaper = std::thread::spawn(move || short.wait());
            assert!(gone(pid, Duration::from_secs(5)).await);
            let _ = reaper.join();
            let mut long = std::process::Command::new("sleep").arg("30").spawn()?;
            let t = Instant::now();
            assert!(!gone(long.id(), Duration::from_millis(400)).await);
            assert!(t.elapsed() < Duration::from_secs(3));
            let _ = long.kill();
            let _ = long.wait();
            anyhow::Ok(())
        })
    }

    #[test]
    fn task_output_files_and_their_tails() -> anyhow::Result<()> {
        let env = vec![("CLAUDE_CODE_TMPDIR".to_owned(), "/tmp/cc".to_owned())];
        assert_eq!(claude_task_file(&env, "/home/u/my app", Some("s1"), "b7").as_deref(), Some("/tmp/cc/-home-u-my-app/s1/tasks/b7.output"));
        assert!(claude_task_file(&env, "/w", Some("s1"), "../x").is_none());
        assert!(claude_task_file(&env, "/w", None, "b7").is_none());
        let dir = std::env::temp_dir().join(format!("kiln-tail-{}", crate::util::new_id())).join("tasks");
        std::fs::create_dir_all(&dir)?;
        let f = dir.join("b7.output");
        std::fs::write(&f, "one\ntwo\n")?;
        assert_eq!(read_tail(&f, None), Some(("one\ntwo\n".to_owned(), 8, false)));
        assert_eq!(read_tail(&f, Some(4)), Some(("two\n".to_owned(), 8, false)));
        // Shorter than asked for (written again from the start): from the start.
        assert_eq!(read_tail(&f, Some(100)), Some(("one\ntwo\n".to_owned(), 8, false)));
        std::fs::remove_dir_all(dir.parent().unwrap_or(&dir))?;
        Ok(())
    }
}
