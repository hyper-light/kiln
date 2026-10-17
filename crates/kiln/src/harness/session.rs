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
    }

    /// The parts lists care about changed (activity timestamps aside).
    pub fn differs(&self, o: &Self) -> bool {
        let strip = |i: &Self| Self { last_activity: 0, ..i.clone() };
        strip(self) != strip(o)
    }
}

/// Where a session's updates go (the server's registry, or the uplink to it).
pub type Sink = async_channel::Sender<(String, Update)>;

enum Ctl {
    Cmd(Command),
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
        self.ctl.try_send(Ctl::Cmd(c)).is_ok()
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

    /// What resuming this session takes: its spec with the harness's session id to continue.
    pub fn resume_spec(&self) -> Spec {
        let mut s = self.spec.lock().clone();
        if let Some(h) = self.core.lock().state.harness_session.clone() {
            s.resume = Some(h);
        }
        s.prompt = None;
        s
    }
}

/// Start a session; its updates go to `sink`, tagged with its id.
pub fn start(id: String, spec: Spec, sink: Sink, on_change: Arc<dyn Fn() + Send + Sync>) -> anyhow::Result<Arc<Session>> {
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
        if let Err(p) = std::panic::AssertUnwindSafe(drive(Arc::clone(&run), kind, ctl_rx, sink.clone(), on_change)).catch_unwind().await {
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
}

/// The session's life: start the process, pump it, and after it exits wait to be restarted.
async fn drive(s: Arc<Session>, kind: &'static Kind, ctl_rx: async_channel::Receiver<Ctl>, sink: Sink, on_change: Arc<dyn Fn() + Send + Sync>) {
    let (pv_tx, pv_rx) = async_channel::unbounded::<(String, Vec<super::model::FileDiff>)>();
    let (ch_tx, ch_rx) = async_channel::unbounded::<(String, (Option<super::model::FileChange>, Option<super::core::Baseline>))>();
    // Announce it before anything else, so whoever mirrors it knows what it is.
    let _ = sink.send((s.id.clone(), Update::Open { info: Box::new(s.info()) })).await;
    let mut first = true;
    let mut crashes: Vec<Instant> = Vec::new();
    // A message that arrived while the process was down, sent once it is back.
    let mut carry: Vec<Command> = Vec::new();
    loop {
        let spec = if first { s.spec.lock().clone() } else { s.resume_spec() };
        let mut adapter = kind.adapter(&spec);
        let started = Instant::now();
        let spawned = kind.launch(&spec).and_then(crate::proc::spawn_duplex);
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
                        adapter.command(&mut core, Command::Send { id: crate::util::new_id(), text: prompt, mode: Default::default() });
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
                        x = (&mut p.exit).fuse() => Ev::Exit(x.ok().flatten()),
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
                        Ev::Ctl(Some(Ctl::Cmd(Command::Restart))) => {}
                        Ev::Ctl(Some(Ctl::Cmd(Command::Snapshot))) => {
                            for u in s.snapshot() {
                                let _ = sink.send((s.id.clone(), u)).await;
                            }
                        }
                        Ev::Ctl(Some(Ctl::Cmd(c))) => {
                            let mut core = s.core.lock();
                            adapter.command(&mut core, c);
                        }
                        Ev::Ctl(Some(Ctl::Close)) | Ev::Ctl(None) => {
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
                    jobs = flush(&s, &sink, Some(&p.stdin)).await;
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
        loop {
            match ctl_rx.recv().await {
                Ok(Ctl::Cmd(Command::Restart)) => break,
                Ok(Ctl::Cmd(Command::Snapshot)) => {
                    for u in s.snapshot() {
                        let _ = sink.send((s.id.clone(), u)).await;
                    }
                }
                Ok(Ctl::Cmd(c @ Command::Send { .. })) => {
                    carry.push(c);
                    break;
                }
                Ok(Ctl::Cmd(c)) => {
                    {
                        let mut core = s.core.lock();
                        adapter.command(&mut core, c);
                    }
                    flush(&s, &sink, None).await;
                }
                Ok(Ctl::Close) | Err(_) => {
                    let _ = sink.send((s.id.clone(), Update::Closed)).await;
                    on_change();
                    return;
                }
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
}
