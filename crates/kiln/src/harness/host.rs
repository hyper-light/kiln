//! A machine's agent sessions (the server's own for `local`, a runner's for its machine): starts
//! them, routes commands to them, enforces limits, and keeps a small record on disk so that after
//! the process holding them restarts they continue where they were (each harness resumes its own
//! session), with kiln's queue of waiting messages intact.
//!
//! Its time is its clock's (kiln-seams' `Clock`, docs/seams.md): its watch of what the harnesses
//! run, and the pause that gathers changes into one write of its record, go by kiln's runtime in
//! production and by a simulated clock in tests.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use kiln_seams::Clock;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use super::kinds::{Kind, KindInfo};
use super::model::{Command, QueueItem, Status};
use super::session::{self, Info, Session, Sink, Spec};

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Agent sessions on this machine.
    pub per_machine: usize,
    /// Agent sessions in one shell.
    pub per_shell: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self { per_machine: 64, per_shell: 32 }
    }
}

/// What a machine runs and offers, sent to the server on every (re)connect.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Inventory {
    pub sessions: Vec<String>,
    pub kinds: Vec<KindInfo>,
}

/// A session as kept on disk, and as it goes to another runner process of this machine when its
/// harness process can't (a blue/green upgrade, `crate::bluegreen`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Saved {
    id: String,
    spec: Spec,
    #[serde(default)]
    queue: Vec<QueueItem>,
    /// A pause for a usage limit, which goes on (with its timer) after a restart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pause: Option<super::model::Pause>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pause_at: Option<u8>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    manual_resume: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    move_on_limit: bool,
    #[serde(default, skip_serializing_if = "super::model::MovePlan::is_empty")]
    move_plan: super::model::MovePlan,
    /// A turn of its was under way, and hadn't ended, when this runner last saved it: a restart
    /// (an upgrade, the system stopping the runner, a crash) cut it short, and started again it
    /// is told to go on with it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    cut: bool,
    /// Its harness's process, and what it started, as last seen (`survivors`): after a restart, a
    /// harness that outlived its runner is stopped before another resumes the conversation, and
    /// what it started that still runs is taken back over.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    harness: Option<super::survivors::Ran>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    ran: Vec<super::survivors::Ran>,
}

impl Saved {
    pub fn id(&self) -> &str {
        &self.id
    }
}

/// `s` as it is kept: its spec to resume it by, its queue and pause, and whether a turn of its is
/// under way. Takes the session's locks itself: never with one held.
fn record(s: &Session) -> Saved {
    // resume_spec takes the session's lock itself: before this one, never inside it.
    let spec = s.resume_spec();
    let ran = s.ran.lock().clone();
    let core = s.core.lock();
    let st = &core.state;
    Saved {
        id: s.id.clone(),
        spec,
        queue: st.queue.clone(),
        pause: st.pause.clone(),
        pause_at: st.pause_at,
        manual_resume: st.manual_resume,
        move_on_limit: st.move_on_limit,
        move_plan: st.move_plan.clone(),
        cut: s.turn_open.load(Ordering::Relaxed),
        harness: ran.0,
        ran: ran.1,
    }
}

/// What kiln says first to a session whose turn its restart cut short.
const CUT_BY_RESTART: &str = "kiln restarted on this machine part-way through your last turn, which stopped it. Pick up where you left off.";

pub struct Host {
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    sink: Sink,
    pub limits: Limits,
    file: Option<PathBuf>,
    dirty: AtomicBool,
    /// What the sessions' harnesses run is being looked at (`watch`).
    watching: AtomicBool,
    me: Weak<Host>,
    /// Where files attached to messages are kept, one directory per session.
    files_dir: PathBuf,
    /// Files received (whole), by file id.
    received: Mutex<HashMap<String, PathBuf>>,
    clock: Arc<dyn Clock>,
    /// Looks the watch has taken, and records written: told to `changed`'s listeners.
    looks: AtomicU64,
    saves: AtomicU64,
    /// Where its looks and record writes are counted for /metrics (kiln's, `metrics::agents()`).
    metrics: &'static crate::metrics::AgentMetrics,
    changed: event_listener::Event,
}

/// Largest file a message can carry.
pub const ATTACHMENT_MAX: u64 = 25 << 20;
/// Largest package of a session going on here from the server's store (its whole transcript).
pub const PACKAGE_MAX: u64 = 1 << 30;

impl Host {
    pub fn new(sink: Sink, file: Option<PathBuf>, limits: Limits) -> Arc<Self> {
        Self::with_clock(sink, file, limits, crate::rt::clock())
    }

    /// A host whose time is `clock`'s.
    pub fn with_clock(sink: Sink, file: Option<PathBuf>, limits: Limits, clock: Arc<dyn Clock>) -> Arc<Self> {
        Self::counting_into(sink, file, limits, clock, crate::metrics::agents())
    }

    /// A host whose time is `clock`'s, its looks and record writes counted into `metrics`.
    pub fn counting_into(sink: Sink, file: Option<PathBuf>, limits: Limits, clock: Arc<dyn Clock>, metrics: &'static crate::metrics::AgentMetrics) -> Arc<Self> {
        // The models its harnesses' sessions list are kept beside its record of them.
        if let Some(dir) = file.as_ref().and_then(|f| f.parent()) {
            super::kinds::keep_models_in(dir.join("harness-models.json"));
        }
        let files_dir = file.as_ref().and_then(|f| f.parent()).map_or_else(
            || std::env::temp_dir().join(format!("kiln-attachments-{}", crate::os::uid())),
            |d| d.join("attachments"),
        );
        Arc::new_cyclic(|me| Self {
            sessions: Mutex::new(HashMap::new()),
            sink,
            limits,
            file,
            dirty: AtomicBool::new(false),
            watching: AtomicBool::new(false),
            me: me.clone(),
            files_dir,
            received: Mutex::new(HashMap::new()),
            clock,
            looks: AtomicU64::new(0),
            saves: AtomicU64::new(0),
            metrics,
            changed: event_listener::Event::new(),
        })
    }

    /// Looks the watch has taken at what the harnesses run.
    pub fn looks(&self) -> u64 {
        self.looks.load(Ordering::Acquire)
    }

    /// Records of the sessions written (a write that failed isn't one).
    pub fn saves(&self) -> u64 {
        self.saves.load(Ordering::Acquire)
    }

    /// Completes once `holds` holds, looked at again after each look and each record written.
    pub async fn until(&self, holds: impl Fn(&Self) -> bool) {
        loop {
            let listener = self.changed.listen();
            if holds(self) {
                return;
            }
            listener.await;
        }
    }

    /// A piece of a file attached to a message for `session` (blocking; small writes).
    pub fn receive(&self, session: &str, file: &str, offset: u64, data: &[u8], done: bool) -> anyhow::Result<()> {
        use std::io::{Seek as _, Write as _};
        let safe = |s: &str| s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') && !s.is_empty() && s.len() <= 64;
        if !safe(session) || !safe(file) {
            anyhow::bail!("bad file or session id");
        }
        if offset.saturating_add(data.len() as u64) > ATTACHMENT_MAX {
            anyhow::bail!("attachment too large");
        }
        let dir = self.files_dir.join(session).join("incoming");
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(file);
        let mut f = std::fs::OpenOptions::new().create(true).write(true).truncate(offset == 0).open(&path)?;
        f.seek(std::io::SeekFrom::Start(offset))?;
        f.write_all(data)?;
        if done {
            self.received.lock().insert(file.to_owned(), path);
        }
        Ok(())
    }

    /// A piece of the package of a session the server sends to go on here (it went on from the
    /// server's conversation store: its machine is gone). Blocking (small writes). The package,
    /// once its last piece is here.
    pub fn receive_package(&self, session: &str, offset: u64, data: &[u8], done: bool) -> anyhow::Result<Option<PathBuf>> {
        use std::io::{Seek as _, Write as _};
        if session.is_empty() || session.len() > 64 || !session.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            anyhow::bail!("bad session id");
        }
        if offset.saturating_add(data.len() as u64) > PACKAGE_MAX {
            anyhow::bail!("a session package too large");
        }
        let dir = self.files_dir.join(session);
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("package.json");
        let mut f = std::fs::OpenOptions::new().create(true).write(true).truncate(offset == 0).open(&path)?;
        f.seek(std::io::SeekFrom::Start(offset))?;
        f.write_all(data)?;
        Ok(done.then_some(path))
    }

    /// Take over the session in the package the server sent (`receive_package`). Blocking.
    pub fn adopt_package(&self, path: &std::path::Path) -> Result<(), String> {
        let bytes = std::fs::read(path).map_err(|e| format!("the session's package: {e}"));
        let _ = std::fs::remove_file(path);
        self.adopt("the kiln server", &bytes?)
    }

    /// Session `id` went on on another machine since: its harness here stops, and this machine
    /// lets go of it (it isn't closed).
    pub fn let_go(&self, id: &str) -> bool {
        let s = self.get(id);
        if let Some(s) = &s {
            s.let_go();
        }
        s.is_some()
    }

    /// A received file, once (its id is spent).
    fn take_file(&self, file: &str) -> Option<PathBuf> {
        self.received.lock().remove(file)
    }

    pub fn get(&self, id: &str) -> Option<Arc<Session>> {
        self.sessions.lock().get(id).cloned()
    }

    /// Each session and its harness's process, where one runs (which agent a process is: a
    /// `kiln` command run by an agent's tool, `super::socket`). The sessions' own locks are taken
    /// after this host's is let go.
    pub fn harness_pids(&self) -> Vec<(String, Option<u32>)> {
        let sessions: Vec<Arc<Session>> = self.sessions.lock().values().cloned().collect();
        sessions.iter().map(|s| (s.id.clone(), *s.pid.lock())).collect()
    }

    pub fn list(&self) -> Vec<Info> {
        let mut v: Vec<Info> = self.sessions.lock().values().map(|s| s.info()).collect();
        v.sort_by(|a, b| a.created_at.cmp(&b.created_at).then_with(|| a.id.cmp(&b.id)));
        v
    }

    /// What each session's harness keeps as its record, for the conversation store to follow.
    /// The sessions' locks are taken after this host's is let go.
    pub fn record_sources(&self) -> Vec<crate::convo::record::Source> {
        let sessions: Vec<Arc<Session>> = self.sessions.lock().values().cloned().collect();
        sessions.iter().filter_map(|s| s.record_source()).collect()
    }

    pub fn inventory(&self) -> Inventory {
        Inventory { sessions: self.sessions.lock().keys().cloned().collect(), kinds: super::kinds::offered() }
    }

    /// Sessions still working on something (an upgrade waits for these).
    pub fn busy(&self) -> usize {
        self.sessions.lock().values().filter(|s| matches!(s.core.lock().state.status, Status::Running | Status::Waiting)).count()
    }

    /// Start a session (`id` keeps a known one's identity, e.g. one the server named).
    pub fn open(&self, id: Option<String>, spec: Spec) -> anyhow::Result<Arc<Session>> {
        self.open_seeded(id, spec, None, None)
    }

    /// Take over a session another runner handed this one (`super::moving`): its record where its
    /// harness here looks, then the session, under its own id, its timeline as it was. Blocking
    /// (writes the record).
    pub fn adopt(&self, from: &str, data: &[u8]) -> Result<(), String> {
        let pkg: super::moving::Package = serde_json::from_slice(data).map_err(|e| format!("the session from {from} is unreadable: {e}"))?;
        if !super::moving::takes(pkg.version) {
            return Err(format!("a session packed as version {} (this runner takes up to {})", pkg.version, super::moving::VERSION));
        }
        let kind = Kind::get(&pkg.spec.kind).ok_or_else(|| format!("this runner doesn't know {}", pkg.spec.kind))?;
        if !kind.available() {
            return Err(format!("{} is not installed on this machine", kind.name));
        }
        if self.get(&pkg.id).is_some() {
            return Err("that session runs here already".into());
        }
        let mut pkg = pkg;
        let mut cwd = session::resolve_cwd(pkg.spec.cwd.as_deref());
        // Its folder isn't here (it wasn't a repository kiln keeps in step): it works in home. A
        // worktree it brings is made once its repository's files are here.
        let mut note = None;
        if !std::path::Path::new(&cwd).is_dir() && pkg.worktree.is_none() {
            note = Some(format!("{cwd} isn't on this machine, so it works in the home folder"));
            cwd = session::resolve_cwd(None);
            pkg.spec.cwd = Some(cwd.clone());
        }
        let items = super::moving::install(&pkg, &cwd)?;
        // Its repository's state, the files git ignores and its worktree, kept on disk until its
        // files are here (a restart keeps them too), readable by this user alone: they may hold
        // secrets.
        if let Some(g) = pkg.git.take() {
            let dir = self.files_dir.join(&pkg.id);
            let kept = super::moving::MovedGit { from: pkg.from.clone(), state: g, ignored: pkg.ignored.take(), worktree: pkg.worktree.take() };
            std::fs::create_dir_all(&dir).and_then(|()| super::moving::keep_private(&super::moving::moved_git(&dir), &serde_json::to_vec(&kept).unwrap_or_default())).map_err(|e| format!("keeping its repository's state: {e}"))?;
        }
        let seed = session::Seed { events: pkg.events, state: pkg.state, items, from: pkg.from, from_cwd: pkg.from_cwd, picks_up: pkg.picks_up, note, restored: false, synced: pkg.synced };
        self.open_seeded(Some(pkg.id), pkg.spec, Some(seed), None).map(|_| ()).map_err(|e| format!("{e:#}"))
    }

    /// Let go of a session that went on on another machine: it is no longer this machine's, and it
    /// isn't closed (its viewers follow it there).
    pub fn release(&self, id: &str) {
        if self.sessions.lock().remove(id).is_some() {
            self.mark_dirty();
        }
    }

    fn open_seeded(&self, id: Option<String>, spec: Spec, seed: Option<session::Seed>, handed: Option<Box<session::Handed>>) -> anyhow::Result<Arc<Session>> {
        if Kind::get(&spec.kind).is_none() {
            anyhow::bail!("unknown harness {:?}", spec.kind);
        }
        {
            let g = self.sessions.lock();
            if let Some(id) = &id
                && let Some(s) = g.get(id)
            {
                return Ok(Arc::clone(s));
            }
            if g.len() >= self.limits.per_machine {
                anyhow::bail!("this machine already runs {} agent sessions (the limit)", g.len());
            }
            if let Some(shell) = &spec.shell {
                let n = g.values().filter(|s| s.spec.lock().shell.as_deref() == Some(shell.as_str())).count();
                if n >= self.limits.per_shell {
                    anyhow::bail!("that shell already has {n} agent sessions (the limit)");
                }
            }
        }
        let id = id.unwrap_or_else(crate::util::new_id);
        let me = self.me.clone();
        let on_change: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            if let Some(h) = me.upgrade() {
                h.mark_dirty();
            }
        });
        let me = self.me.clone();
        let me2 = self.me.clone();
        let me3 = self.me.clone();
        let files = session::Files {
            dir: self.files_dir.join(&id),
            take: Arc::new(move |f: &str| me.upgrade().and_then(|h| h.take_file(f))),
            // A branch: another session on this machine.
            spawn: Arc::new(move |spec: Spec| {
                if let Some(h) = me2.upgrade()
                    && let Err(e) = h.open(None, spec)
                {
                    tracing::warn!(error = %format!("{e:#}"), "could not branch an agent session");
                }
            }),
            release: Arc::new(move |id: &str| {
                if let Some(h) = me3.upgrade() {
                    h.release(id);
                }
            }),
        };
        let s = match handed {
            Some(h) => session::start_handed(id.clone(), spec, self.sink.clone(), on_change, files, h)?,
            None => session::start_seeded(id.clone(), spec, self.sink.clone(), on_change, files, seed)?,
        };
        self.sessions.lock().insert(id, Arc::clone(&s));
        self.mark_dirty();
        self.watch();
        Ok(s)
    }

    pub fn command(&self, id: &str, c: Command) -> bool {
        self.get(id).is_some_and(|s| s.command(c))
    }

    pub fn close(&self, id: &str) -> bool {
        let s = self.sessions.lock().remove(id);
        let found = s.is_some();
        if let Some(s) = s {
            s.close();
            // A workspace of its own ends with it.
            let spec = s.spec.lock().clone();
            crate::workspace::release(&spec);
            // Its attachments go with it.
            let dir = self.files_dir.join(id);
            crate::rt::spawn(async move {
                let _ = crate::rt::blocking(move || std::fs::remove_dir_all(dir)).await;
            });
        }
        self.mark_dirty();
        found
    }

    /// Stop every harness process now (this process is about to end, and a fresh one continues
    /// the sessions from the record saved here: an upgrade by a successor on macOS). Blocks up to
    /// a few seconds.
    pub fn stop_all(&self) {
        self.save_now();
        let pids: Vec<u32> = self.sessions.lock().values().filter_map(|s| *s.pid.lock()).collect();
        for pid in pids {
            stop_process(pid);
            // The process that continues them won't wait for it: reaped now.
            #[cfg(unix)]
            let _ = crate::os::try_reap(pid);
        }
    }

    /// Hand every session's harness process, still running, to this process's next image (it
    /// re-executes itself next: an in-place upgrade), where each goes on as it was, mid-turn
    /// included ([`Host::restore`]). One that can't be handed over is stopped, and starts again
    /// there from its record (its conversation goes on). Returns how many were handed over.
    pub async fn hand_over_all(&self) -> usize {
        let sessions: Vec<Arc<Session>> = self.sessions.lock().values().cloned().collect();
        let handed = futures::future::join_all(sessions.iter().map(|s| async move { (Arc::clone(s), crate::rt::timeout(Duration::from_secs(15), s.hand_over()).await.ok().flatten()) })).await;
        let mut out: HashMap<String, Box<session::Handed>> = HashMap::new();
        for (s, h) in handed {
            match h {
                Some(h) => {
                    out.insert(s.id.clone(), h);
                }
                None => {
                    if let Some(pid) = s.pid.lock().take() {
                        stop_process(pid);
                        #[cfg(unix)]
                        let _ = crate::os::try_reap(pid);
                    }
                }
            }
        }
        self.save_now();
        let n = out.len();
        if let Some(path) = self.handover_path() {
            let wrote = serde_json::to_vec(&out).map_err(std::io::Error::other).and_then(|json| crate::util::write_secret(&path, &json, 0o600));
            if let Err(e) = wrote {
                // The next image can't take them: stopped, and they start again from their records.
                tracing::warn!(error = %e, file = %path.display(), "couldn't write the agent sessions' handover: they start again after the upgrade");
                for h in out.values() {
                    stop_process(h.process.pid);
                    #[cfg(unix)]
                    let _ = crate::os::try_reap(h.process.pid);
                }
                return 0;
            }
            // Their pipes kept open across the exec that comes next, only now: a process started
            // while they were handed over would hold them for good.
            for (id, h) in &out {
                if let Err(e) = h.process.keep_across_exec() {
                    tracing::warn!(session = %id, error = %e, "an agent session's pipes can't be kept across the upgrade: it starts again after it");
                }
            }
        }
        n
    }

    /// Every session, handed to another runner process of this machine (a blue/green upgrade,
    /// `crate::bluegreen`): each with its spec and, still running, its harness process (`None`:
    /// it couldn't be handed over, was stopped, and starts again there from its spec). They are no
    /// longer this runner's.
    pub async fn move_out(&self) -> Vec<(Saved, Option<Box<session::Handed>>)> {
        let sessions: Vec<Arc<Session>> = self.sessions.lock().values().cloned().collect();
        let handed = futures::future::join_all(sessions.iter().map(|s| async move { (Arc::clone(s), crate::rt::timeout(Duration::from_secs(15), s.hand_over()).await.ok().flatten()) })).await;
        let mut out = Vec::new();
        for (s, h) in handed {
            if h.is_none()
                && let Some(pid) = s.pid.lock().take()
            {
                stop_process(pid);
                #[cfg(unix)]
                let _ = crate::os::try_reap(pid);
            }
            let rec = record(&s);
            self.sessions.lock().remove(&s.id);
            out.push((rec, h));
        }
        self.mark_dirty();
        out
    }

    /// Take over a session another runner process of this machine handed over (blue/green): with
    /// its process (its descriptors are this process's now), or started again from its record (a
    /// turn its stop cut short is picked up first).
    pub fn move_in(&self, rec: Saved, handed: Option<Box<session::Handed>>) -> anyhow::Result<()> {
        match handed {
            Some(h) => {
                let (harness, ran) = (rec.harness.clone(), rec.ran.clone());
                let session = self.open_seeded(Some(rec.id), rec.spec, None, Some(h))?;
                // What its harness runs stays under it; the watch finds the rest again.
                *session.ran.lock() = (harness, ran);
                Ok(())
            }
            None => {
                let outlived = super::survivors::alive(&rec.harness.iter().cloned().collect::<Vec<_>>());
                if !outlived.is_empty() {
                    super::survivors::stop(&outlived.iter().map(|r| r.pid).collect::<Vec<_>>());
                }
                let found = super::survivors::scan(&[(rec.id.clone(), None)]);
                let apart = found.get(&rec.id).map(|f| f.apart.clone()).unwrap_or_default();
                let now_s = u64::try_from(self.clock.unix_s()).unwrap_or(0);
                self.open_record(rec, None, apart, now_s).map(|_| ())
            }
        }
    }

    /// The image the sessions were handed to never came (the exec failed): this one goes on with
    /// them, their processes included.
    pub fn take_back(&self) {
        self.sessions.lock().clear();
        self.restore();
    }

    /// Where the sessions handed to the next image are written: beside their record.
    fn handover_path(&self) -> Option<PathBuf> {
        self.file.as_ref().map(|f| f.with_file_name("agent-handover.json"))
    }

    /// The sessions the previous image of this process handed over ([`Host::hand_over_all`]),
    /// once: the file goes as it's read.
    fn take_handover(&self) -> HashMap<String, Box<session::Handed>> {
        let Some(path) = self.handover_path() else { return HashMap::new() };
        let Ok(raw) = std::fs::read(&path) else { return HashMap::new() };
        let _ = std::fs::remove_file(&path);
        serde_json::from_slice(&raw).unwrap_or_else(|e| {
            tracing::warn!(error = %e, file = %path.display(), "unreadable agent session handover: they start again");
            HashMap::new()
        })
    }

    /// Continue the sessions recorded before a restart (`restore_from`, without the conversation
    /// store).
    pub fn restore(&self) {
        self.restore_from(None);
    }

    /// Continue the sessions recorded before a restart. Those the previous image of this process
    /// handed over go on with their processes, still running. The rest start again, each with its
    /// timeline and state as the conversation store kept them (`crate::convo`), when it kept any;
    /// otherwise as before (a Claude Code session's history from its transcript).
    pub fn restore_from(&self, store: Option<&crate::convo::store::Store>) {
        tracing::info!(how = %crate::os::contain::how(), "agents' processes are held together by");
        let mut handed = self.take_handover();
        let saved: Vec<Saved> = match self.file.as_ref().map(|f| (f, std::fs::read(f))) {
            Some((file, Ok(raw))) => serde_json::from_slice(&raw).unwrap_or_else(|e| {
                tracing::warn!(error = %e, file = %file.display(), "unreadable agent session record; starting without it");
                Vec::new()
            }),
            _ => Vec::new(),
        };
        // A harness that outlived the runner it ran under (a crash, the system stopping only the
        // runner) is stopped first, so no second one resumes its conversation beside it; what it
        // started is kept, and taken back over with the rest.
        let outlived: Vec<super::survivors::Ran> =
            super::survivors::alive(&saved.iter().filter(|s| !handed.contains_key(&s.id)).filter_map(|s| s.harness.clone()).collect::<Vec<_>>());
        if !outlived.is_empty() {
            tracing::info!(processes = ?outlived.iter().map(|r| r.pid).collect::<Vec<_>>(), "agent harnesses outlived their runner: stopped before their sessions resume");
            super::survivors::stop(&outlived.iter().map(|r| r.pid).collect::<Vec<_>>());
        }
        let now_s = u64::try_from(self.clock.unix_s()).unwrap_or(0);
        // What the sessions started that still runs: found by their mark, and as recorded.
        let found = super::survivors::scan(&saved.iter().map(|s| (s.id.clone(), None)).collect::<Vec<_>>());
        for s in saved {
            let id = s.id.clone();
            if let Some(h) = handed.remove(&id) {
                let pid = h.process.pid;
                match self.open_seeded(Some(s.id.clone()), s.spec.clone(), None, Some(h)) {
                    Ok(session) => {
                        // What its harness runs stays under it; the watch finds the rest again.
                        *session.ran.lock() = (s.harness.clone(), s.ran.clone());
                        continue;
                    }
                    Err(e) => {
                        tracing::warn!(session = %id, error = %format!("{e:#}"), "could not go on with an agent session's process: it starts again");
                        stop_process(pid);
                    }
                }
            }
            let seed = store.and_then(|st| kept_seed(st, &id));
            let from_store = seed.is_some();
            let cut = s.cut;
            let apart = found.get(&id).map(|f| f.apart.clone()).unwrap_or_default();
            match self.open_record(s, seed, apart, now_s) {
                Ok(left) => tracing::info!(session = %id, from_store, cut, left_running = left, "agent session continued after a restart"),
                Err(e) => tracing::warn!(session = %id, error = %format!("{e:#}"), "could not continue an agent session"),
            }
        }
        // Handed over without a record to go on from: nothing goes on with them.
        for h in handed.into_values() {
            stop_process(h.process.pid);
        }
    }

    /// Start a session again from its record: its queue and pause as they were and, when a turn of
    /// its was cut short, that turn picked up first.
    /// `apart`: what it started that still runs on its own, found by its mark (`survivors::scan`).
    /// What it started that still runs (that, and what its record says) is taken back over as
    /// left running. How many such were found.
    fn open_record(&self, s: Saved, seed: Option<session::Seed>, apart: Vec<super::survivors::Ran>, now_s: u64) -> anyhow::Result<usize> {
        let cut = s.cut;
        // What it started that still runs, taken back over (its stopped harness not among it).
        let mut alive = apart;
        for r in super::survivors::alive(&s.ran).into_iter().filter(|r| !super::survivors::login_agent(&r.command)) {
            if !alive.iter().any(|a| a.pid == r.pid) {
                alive.push(r);
            }
        }
        if let Some(h) = &s.harness {
            alive.retain(|r| r.pid != h.pid);
        }
        let trees = super::survivors::trees(&alive);
        let session = self.open_seeded(Some(s.id), s.spec, seed, None)?;
        let paused = s.pause.is_some() || s.pause_at.is_some() || s.manual_resume || s.move_on_limit;
        if !trees.is_empty() {
            session.ran.lock().1.clone_from(&alive);
            *session.left.lock() = trees.iter().map(|(root, _)| root.clone()).collect();
        }
        // Shown once each, and what it showed as left running that has ended since, ended.
        let apart = alive.iter().map(|r| r.pid).collect();
        let mut core = session.core.lock();
        if settle_left(&mut core.state.tasks, trees.iter().map(|(root, _)| super::survivors::task(root)).collect(), &apart, now_s) {
            core.touch_state();
        }
        drop(core);
        if !s.queue.is_empty() || paused || cut {
            let mut core = session.core.lock();
            core.state.queue = s.queue;
            // A check under way when it stopped is asked again.
            core.state.pause = s.pause.map(|p| super::model::Pause { checking: None, ..p });
            core.state.pause_at = s.pause_at;
            core.state.manual_resume = s.manual_resume;
            core.state.move_on_limit = s.move_on_limit;
            core.state.move_plan = s.move_plan;
            // The turn the restart cut short goes on first, in kiln's words, with what it started
            // that still runs.
            if cut {
                picks_up(&mut core.state.queue, &super::survivors::said(&trees, now_s));
            }
            core.touch_state();
        }
        Ok(trees.len())
    }

    /// Every [`WATCH_EVERY`] by its clock, what each session's harness runs: noted (and kept in the
    /// record when it changes), and what it started that runs on once no longer under it (a command
    /// that daemonized, a harness started again) taken over as left running, until it ends.
    fn watch(&self) {
        if self.watching.swap(true, Ordering::AcqRel) {
            return;
        }
        let me = self.me.clone();
        let clock = Arc::clone(&self.clock);
        crate::rt::spawn(async move {
            loop {
                clock.sleep(WATCH_EVERY).await;
                let Some(h) = me.upgrade() else { return };
                let sessions: Vec<Arc<Session>> = h.sessions.lock().values().cloned().collect();
                let which: Vec<(String, Option<u32>)> = sessions.iter().map(|s| (s.id.clone(), *s.pid.lock())).collect();
                let known: Vec<super::survivors::Ran> = sessions.iter().flat_map(|s| s.ran.lock().1.clone()).collect();
                let now_s = u64::try_from(clock.unix_s()).unwrap_or(0);
                let look = move || {
                    let seen = super::survivors::scan(&which);
                    // Shells' ssh-agents are no session's work: stopped once old enough.
                    let agents = super::survivors::agents_to_stop(&seen, now_s);
                    if !agents.is_empty() {
                        tracing::info!(count = agents.len(), "stopping the ssh-agents shells' startup files left in agents' sessions");
                        super::survivors::stop(&agents);
                    }
                    (seen, super::survivors::alive(&known))
                };
                let Ok((seen, alive)) = crate::rt::blocking(look).await else { continue };
                let alive: HashMap<u32, u64> = alive.into_iter().map(|r| (r.pid, r.started)).collect();
                let mut changed = false;
                for s in &sessions {
                    if h.note(s, seen.get(&s.id).cloned().unwrap_or_default(), &alive, now_s) {
                        changed = true;
                    }
                }
                if changed {
                    h.mark_dirty();
                }
                h.looks.fetch_add(1, Ordering::AcqRel);
                h.metrics.harness_polls.inc();
                h.changed.notify(usize::MAX);
            }
        });
    }

    /// One session's look (`watch`): whether what's kept of it changed. What it started that runs
    /// apart from its harness (marked as its, or as recorded and still running) is left running.
    fn note(&self, s: &Session, seen: super::survivors::Seen, alive: &HashMap<u32, u64>, now_s: u64) -> bool {
        let running = s.pid.lock().is_some();
        let mut ran = s.ran.lock();
        let still = |r: &super::survivors::Ran| alive.get(&r.pid) == Some(&r.started);
        let under: std::collections::HashSet<u32> = seen.under.iter().map(|r| r.pid).collect();
        let mut apart = seen.apart;
        // As recorded, still running, neither under it nor marked (it cleared its environment).
        for r in ran.1.iter().filter(|r| !under.contains(&r.pid) && still(r) && !super::survivors::login_agent(&r.command)) {
            if !apart.iter().any(|a| a.pid == r.pid) {
                apart.push(r.clone());
            }
        }
        let now_apart: std::collections::HashSet<u32> = apart.iter().map(|r| r.pid).collect();
        let mut left = s.left.lock();
        let mut new_left = Vec::new();
        for (root, _) in super::survivors::trees(&apart) {
            if !left.iter().any(|l| l.pid == root.pid) {
                new_left.push(root.clone());
                left.push(root);
            }
        }
        // Left running that ended.
        left.retain(|l| now_apart.contains(&l.pid));
        let mut next = (if running { seen.harness } else { ran.0.clone() }, seen.under);
        next.1.extend(apart);
        let changed = *ran != next;
        *ran = next;
        drop((ran, left));
        let taken = new_left.iter().map(|root| super::model::Task { summary: Some(format!("Left running, apart from the agent (process {})", root.pid)), ..super::survivors::task(root) }).collect();
        let mut core = s.core.lock();
        if settle_left(&mut core.state.tasks, taken, &now_apart, now_s) {
            core.touch_state();
        }
        changed
    }

    fn mark_dirty(&self) {
        if self.file.is_none() || self.dirty.swap(true, Ordering::AcqRel) {
            return;
        }
        let me = self.me.clone();
        let clock = Arc::clone(&self.clock);
        crate::rt::spawn(async move {
            // Coalesce bursts (a session's id arriving, its queue changing) into one write.
            clock.sleep(SAVE_AFTER).await;
            if let Some(h) = me.upgrade() {
                h.dirty.store(false, Ordering::Release);
                let _ = crate::rt::blocking(move || h.save_now()).await;
            }
        });
    }

    fn save_now(&self) {
        let Some(file) = &self.file else { return };
        let sessions: Vec<Arc<Session>> = self.sessions.lock().values().cloned().collect();
        let saved: Vec<Saved> = sessions.iter().map(|s| record(s)).collect();
        let Ok(json) = serde_json::to_vec_pretty(&saved) else { return };
        let tmp = file.with_extension("tmp");
        let wrote = file.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| crate::util::write_secret(&tmp, &json, 0o600)).and_then(|()| std::fs::rename(&tmp, file));
        match wrote {
            Ok(()) => {
                self.saves.fetch_add(1, Ordering::AcqRel);
                self.metrics.harness_records_written.inc();
            }
            Err(e) => tracing::warn!(error = %e, file = %file.display(), "could not record agent sessions"),
        }
        self.changed.notify(usize::MAX);
    }
}

/// What a session shows as left running, against what runs apart from it now (`apart`, by pid):
/// the tasks of the roots taken over now (`taken`) shown, once each (one that came back with its
/// timeline after a restart or an upgrade is shown already), and every one shown that no longer
/// runs ended, those it came back with included, which may have ended while no runner looked.
/// Whether any changed.
fn settle_left(tasks: &mut Vec<super::model::Task>, taken: Vec<super::model::Task>, apart: &std::collections::HashSet<u32>, now_s: u64) -> bool {
    use super::survivors::LEFT;
    // A second copy of one shown goes (older images showed it again on each restart).
    let mut shown = std::collections::HashSet::new();
    let before = tasks.len();
    tasks.retain(|t| !(t.live() && t.id.starts_with(LEFT)) || shown.insert(t.id.clone()));
    let mut changed = tasks.len() != before;
    for t in taken {
        if !shown.contains(&t.id) {
            tasks.push(t);
            changed = true;
        }
    }
    let ended = i64::try_from(now_s).unwrap_or(0).saturating_mul(1000);
    for t in tasks.iter_mut().filter(|t| t.live() && t.id.strip_prefix(LEFT).and_then(|p| p.parse::<u32>().ok()).is_some_and(|p| !apart.contains(&p))) {
        t.status = "completed".to_owned();
        t.ended = Some(ended);
        changed = true;
    }
    changed
}

/// The turn a restart cut short goes on first: kiln's word (shown as kiln's), ahead of what was
/// already queued, with what it started that still runs (`also`), unless it's waiting there
/// already (a restart before it went out).
fn picks_up(queue: &mut Vec<QueueItem>, also: &str) {
    if queue.first().is_some_and(|q| q.text.starts_with(CUT_BY_RESTART)) {
        return;
    }
    let now = super::core::Core::now();
    queue.insert(0, QueueItem { id: format!("{}restart-{now}", super::core::KEEP_ID), text: format!("{CUT_BY_RESTART}{also}"), at: now, attachments: Vec::new(), overrides: None });
}

/// How often what the harnesses run is looked at.
pub const WATCH_EVERY: Duration = Duration::from_secs(10);
/// How long changes are gathered before the record is written.
pub const SAVE_AFTER: Duration = Duration::from_millis(500);

/// A session's timeline and state as the conversation store kept them, for its restart: None when
/// the store kept nothing of it (or can't say in time), and the session starts as it always did.
fn kept_seed(store: &crate::convo::store::Store, id: &str) -> Option<session::Seed> {
    let kept = match store.kept(id, super::core::MAX_EVENTS) {
        Ok(Some(k)) => k,
        Ok(None) => return None,
        Err(e) => {
            tracing::warn!(session = %id, error = %format!("{e:#}"), "the conversation store couldn't say what it kept of an agent session");
            return None;
        }
    };
    let state: super::model::State = kept.state.as_deref().and_then(|s| serde_json::from_str(s).ok())?;
    let events: Vec<super::model::Event> = kept.events.iter().filter_map(|e| serde_json::from_str(&e.body).ok()).collect();
    if events.is_empty() {
        return None;
    }
    Some(session::Seed { events, state, items: None, from: String::new(), from_cwd: String::new(), picks_up: false, note: None, restored: true, synced: None })
}

/// SIGTERM a harness's process group, then SIGKILL it if it is still there after 3 s.
#[cfg(unix)]
pub(crate) fn stop_process(pid: u32) {
    use nix::sys::signal::{Signal, killpg};
    use nix::unistd::Pid;
    let Ok(p) = i32::try_from(pid) else { return };
    if p <= 0 {
        return;
    }
    let _ = killpg(Pid::from_raw(p), Signal::SIGTERM);
    let end = std::time::Instant::now() + Duration::from_secs(3);
    let mut pause = Duration::from_millis(1);
    while std::time::Instant::now() < end {
        if !crate::os::pid_alive(pid) {
            return;
        }
        std::thread::sleep(pause);
        pause = super::survivors::next_look(pause);
    }
    let _ = killpg(Pid::from_raw(p), Signal::SIGKILL);
}

#[cfg(windows)]
pub(crate) fn stop_process(pid: u32) {
    crate::os::proc::taskkill_tree(pid);
}

/// The working directory and environment of a shell (by pid), to start an agent in.
pub fn shell_context(pid: u32) -> (Option<String>, Vec<(String, String)>) {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
    let mut sys = System::new();
    let pid = Pid::from_u32(pid);
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        false,
        ProcessRefreshKind::nothing().with_cwd(UpdateKind::Always).with_environ(UpdateKind::Always).without_tasks(),
    );
    let Some(p) = sys.process(pid) else { return (None, Vec::new()) };
    let cwd = p.cwd().map(|c| c.to_string_lossy().into_owned()).filter(|c| !c.is_empty());
    let env = p
        .environ()
        .iter()
        .filter_map(|kv| {
            let kv = kv.to_string_lossy();
            let (k, v) = kv.split_once('=')?;
            (!k.is_empty()).then(|| (k.to_owned(), v.to_owned()))
        })
        .collect();
    (cwd, env)
}

/// Updates that show a session this machine could not start (and why), so it can be dismissed.
pub fn refusal(id: &str, spec_json: &str, error: &str) -> Vec<super::core::Update> {
    let spec: Spec = serde_json::from_str(spec_json).unwrap_or_default();
    let now = crate::util::now_ms();
    let kind = Kind::get(&spec.kind);
    let exit = super::model::ExitInfo { code: None, error: Some(error.to_owned()) };
    let info = Info {
        id: id.to_owned(),
        kind: spec.kind.clone(),
        kind_name: kind.map_or_else(|| spec.kind.clone(), |k| k.name.to_owned()),
        target: String::new(),
        name: spec.name.clone().unwrap_or_else(|| kind.map_or_else(|| spec.kind.clone(), |k| k.name.to_owned())),
        shell: spec.shell.clone(),
        cwd: spec.cwd.clone().unwrap_or_default(),
        created_at: now,
        status: Status::Exited,
        title: String::new(),
        model: String::new(),
        mode: String::new(),
        harness_session: None,
        approvals: Vec::new(),
        queued: 0,
        queue: Vec::new(),
        tasks_running: 0,
        subagents: Vec::new(),
        cost_usd: 0.0,
        activity: None,
        last_activity: now,
        exit: Some(exit.clone()),
        goal: spec.prompt.clone().map(|p| super::model::clip_middle(&p, 200).0).unwrap_or_default(),
        objective: None,
        summary: String::new(),
        doing: None,
        epoch: 0,
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
        reach: spec.reach.clone(),
        pause: None,
        last_turn: None,
        keep_off: false,
        move_on_limit: false,
        move_plan: Default::default(),
        pause_at: None,
        parent: spec.parent.clone(),
        plan_run: spec.plan_run.clone(),
        no_bypass: !spec.allow_bypass,
        workspace: spec.workspace.clone(),
    };
    let state = super::model::State { status: Status::Exited, exit: Some(exit), reach: spec.reach.clone(), ..Default::default() };
    vec![super::core::Update::Open { info: Box::new(info) }, super::core::Update::State { state: Box::new(state) }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiln_seams::clock::SimClock;

    /// A host on a simulated clock of its own, never moved: its watch never looks and its record
    /// is written only when a test says (`save_now`).
    fn sim(tx: Sink, file: Option<PathBuf>, limits: Limits) -> Arc<Host> {
        Host::with_clock(tx, file, limits, Arc::new(SimClock::new()))
    }

    /// `Host::until`, failing the test rather than waiting forever when what it waits for never
    /// comes (a write that failed): 30 s, which no passing run comes near.
    async fn until(host: &Host, holds: impl Fn(&Host) -> bool) -> anyhow::Result<()> {
        crate::rt::timeout(Duration::from_secs(30), host.until(holds)).await.map_err(|_| anyhow::anyhow!("what the test waited for didn't come in 30 s"))
    }

    fn no_harness(dir: &std::path::Path) -> Spec {
        Spec {
            kind: "claude".into(),
            cwd: Some(dir.to_string_lossy().into_owned()),
            program: Some(dir.join("no-such-claude").to_string_lossy().into_owned()),
            ..Spec::default()
        }
    }

    /// The watch looks at what the harnesses run every 10 s by the host's clock: not before
    /// (CP-M2's AC-M2.1: the harness host's watch on the simulated clock in tests). Each look is
    /// counted for /metrics, once (`kiln_harness_polls_total`, a non-vacuity check).
    #[test]
    fn the_watch_looks_every_10_s_by_the_host_s_clock() -> anyhow::Result<()> {
        crate::rt::block_on(async move {
            let dir = tempfile::tempdir()?;
            let clock = SimClock::new();
            let start = clock.now();
            let (tx, _rx) = async_channel::unbounded();
            let metrics: &'static crate::metrics::AgentMetrics = Box::leak(Box::default());
            let host = Host::counting_into(tx, None, Limits::default(), Arc::new(clock.clone()), metrics);
            // A session starts the watch; closed again, a look has nothing to scan.
            host.open(Some("watched".into()), no_harness(dir.path()))?;
            assert!(host.close("watched"));
            let mut slept = 0;
            for n in 1..=2u32 {
                // The watch sleeps till its next look: 10 s on from the last, and it's the only
                // way to a look, so one that hasn't woken is one that hasn't looked.
                clock.sleeping_since(slept, 1).await;
                slept = clock.registrations();
                assert_eq!((clock.sleepers(), clock.next_deadline()), (1, Some(start + WATCH_EVERY * n)), "the watch's sleep");
                clock.advance(WATCH_EVERY.saturating_sub(Duration::from_millis(1)));
                assert_eq!((clock.sleepers(), host.looks()), (1, u64::from(n - 1)), "awake, or a look, before 10 s");
                clock.advance(Duration::from_millis(1));
                until(&host, |h| h.looks() == u64::from(n)).await?;
                assert_eq!(metrics.harness_polls.get(), u64::from(n), "each look counted");
            }
            Ok(())
        })
    }

    /// Changes in a burst are written once, half a second by the host's clock after the first
    /// (CP-M2, AC-M2.1), and the write counted for /metrics, once
    /// (`kiln_harness_records_written_total`, a non-vacuity check).
    #[test]
    fn changes_are_written_once_half_a_second_after_them_by_the_host_s_clock() -> anyhow::Result<()> {
        crate::rt::block_on(async move {
            let dir = tempfile::tempdir()?;
            let file = dir.path().join("harness.json");
            let clock = SimClock::new();
            let start = clock.now();
            let (tx, _rx) = async_channel::unbounded();
            let metrics: &'static crate::metrics::AgentMetrics = Box::leak(Box::default());
            let host = Host::counting_into(tx, Some(file.clone()), Limits::default(), Arc::new(clock.clone()), metrics);
            host.open(Some("first".into()), no_harness(dir.path()))?;
            // The watch's sleep, and the write's: the write sleeps half a second, and it's the
            // only way to the write, so one that hasn't woken is one that hasn't written.
            clock.sleeping_since(0, 2).await;
            assert_eq!((clock.sleepers(), clock.next_deadline()), (2, Some(start + SAVE_AFTER)), "the write's sleep");
            host.open(Some("second".into()), no_harness(dir.path()))?;
            clock.advance(SAVE_AFTER.saturating_sub(Duration::from_millis(1)));
            assert_eq!((clock.sleepers(), clock.next_deadline()), (2, Some(start + SAVE_AFTER)), "awake before half a second");
            assert!(host.saves() == 0 && !file.exists());
            clock.advance(Duration::from_millis(1));
            until(&host, |h| h.saves() == 1).await?;
            let saved: Vec<Saved> = serde_json::from_slice(&std::fs::read(&file)?)?;
            let mut ids: Vec<&str> = saved.iter().map(Saved::id).collect();
            ids.sort_unstable();
            assert_eq!(ids, ["first", "second"], "both changes in the one write");
            // (What the sessions do next, their harnesses failing to start, is the next burst's.)
            assert_eq!(host.saves(), 1);
            assert_eq!(metrics.harness_records_written.get(), 1, "the write counted");
            Ok(())
        })
    }

    /// On a simulated clock of its own, as every host in these tests is (CP-M2, AC-M2.1).
    #[test]
    fn limits_and_unknown_kinds_are_refused() -> anyhow::Result<()> {
        crate::rt::block_on(async move {
            let (tx, _rx) = async_channel::unbounded();
            let host = sim(tx, None, Limits { per_machine: 1, per_shell: 1 });
            assert!(host.open(None, Spec { kind: "nope".into(), ..Spec::default() }).is_err());
            let dir = tempfile::tempdir()?;
            // A program that does not exist: the session reports why it is down, nothing runs.
            let spec = Spec {
                kind: "claude".into(),
                cwd: Some(dir.path().to_string_lossy().into_owned()),
                program: Some(dir.path().join("no-such-claude").to_string_lossy().into_owned()),
                ..Spec::default()
            };
            let s = host.open(Some("limits".into()), spec.clone())?;
            assert_eq!(s.id, "limits");
            // The same id again is the same session; another one is over the limit.
            assert_eq!(host.open(Some("limits".into()), spec.clone())?.id, "limits");
            assert!(host.open(None, spec).is_err());
            assert_eq!(host.inventory().sessions, vec!["limits".to_owned()]);
            assert!(host.close("limits"));
            assert!(host.list().is_empty());
            Ok(())
        })
    }

    /// On a simulated clock of its own, as every host in these tests is (CP-M2, AC-M2.1).
    #[test]
    fn a_restarted_session_takes_its_timeline_from_the_conversation_store() -> anyhow::Result<()> {
        use super::super::core::Update;
        use super::super::model::{Body, Event, State};
        crate::rt::block_on(async move {
            let dir = tempfile::tempdir()?;
            let file = dir.path().join("harness.json");
            let spec = Spec {
                kind: "claude".into(),
                cwd: Some(dir.path().to_string_lossy().into_owned()),
                program: Some(dir.path().join("no-such-claude").to_string_lossy().into_owned()),
                ..Spec::default()
            };
            // Before the restart: the session, saved; its conversation, in the store.
            {
                let (tx, _rx) = async_channel::unbounded();
                let host = sim(tx, Some(file.clone()), Limits::default());
                host.open(Some("timeline".into()), spec)?;
                host.save_now();
            }
            let (_dir, store) = crate::convo::store::scratch()?;
            let said = |id: &str, seq: u64, text: &str| Update::Events { events: vec![Event { id: id.into(), seq, at: 1_000 + seq as i64, parent: None, by: None, body: Body::Assistant { text: text.into(), streaming: false } }] };
            store.send("timeline", &Update::State { state: Box::new(State { title: "the parser".into(), ..State::default() }) });
            store.send("timeline", &said("m1", 1, "first"));
            store.send("timeline", &said("n1", 2, "a notice only kiln has"));
            store.send("timeline", &said("m2", 3, "second"));
            store.sync()?;
            // After it: the session comes back with that timeline, in order, by id.
            let (tx, _rx) = async_channel::unbounded();
            let again = sim(tx, Some(file.clone()), Limits::default());
            again.restore_from(Some(&store));
            let back = again.sessions.lock().get("timeline").cloned().ok_or_else(|| anyhow::anyhow!("not restored"))?;
            let core = back.core.lock();
            assert_eq!(core.events().map(|e| e.id.as_str()).collect::<Vec<_>>(), vec!["m1", "n1", "m2"]);
            assert_eq!(core.state.title, "the parser");
            Ok(())
        })
    }

    /// On a simulated clock of its own, as every host in these tests is (CP-M2, AC-M2.1).
    #[test]
    fn a_turn_a_restart_cut_short_goes_on_first_thing() -> anyhow::Result<()> {
        crate::rt::block_on(async move {
            let dir = tempfile::tempdir()?;
            let file = dir.path().join("harness.json");
            let spec = Spec {
                kind: "claude".into(),
                cwd: Some(dir.path().to_string_lossy().into_owned()),
                program: Some(dir.path().join("no-such-claude").to_string_lossy().into_owned()),
                ..Spec::default()
            };
            let cut = |file: &std::path::Path| -> anyhow::Result<Vec<String>> {
                let saved: Vec<serde_json::Value> = serde_json::from_slice(&std::fs::read(file)?)?;
                Ok(saved.iter().filter(|s| s.get("cut") == Some(&serde_json::json!(true))).filter_map(|s| s.get("id").and_then(serde_json::Value::as_str).map(str::to_owned)).collect())
            };
            {
                let (tx, _rx) = async_channel::unbounded();
                let host = sim(tx, Some(file.clone()), Limits::default());
                // A turn under way in one, none in the other, as the runner is stopped.
                host.open(Some("busy".into()), spec.clone())?.turn_open.store(true, Ordering::Relaxed);
                host.open(Some("idle".into()), spec.clone())?;
                host.save_now();
            }
            assert_eq!(cut(&file)?, ["busy"]);
            let restore = || -> anyhow::Result<(Arc<Host>, Vec<QueueItem>, Vec<QueueItem>)> {
                let (tx, _rx) = async_channel::unbounded();
                let again = sim(tx, Some(file.clone()), Limits::default());
                again.restore();
                let queue = |id: &str| again.sessions.lock().get(id).map(|s| s.core.lock().state.queue.clone()).unwrap_or_default();
                let (busy, idle) = (queue("busy"), queue("idle"));
                Ok((again, busy, idle))
            };
            let (again, busy, idle) = restore()?;
            assert_eq!(busy.len(), 1);
            assert!(busy.first().is_some_and(|q| q.id.starts_with(super::super::core::KEEP_ID) && q.text.ends_with("Pick up where you left off.")), "{busy:?}");
            assert!(idle.is_empty());
            // Restarted again before it went out: it's there once, not twice.
            if let Some(s) = again.sessions.lock().get("busy") {
                s.turn_open.store(true, Ordering::Relaxed);
            }
            again.save_now();
            drop(again);
            let (again, busy, _) = restore()?;
            assert_eq!(busy.len(), 1, "{busy:?}");
            // Once its turn has ended, a restart says nothing.
            if let Some(s) = again.sessions.lock().get("busy") {
                s.turn_open.store(false, Ordering::Relaxed);
            }
            again.save_now();
            assert!(cut(&file)?.is_empty());
            Ok(())
        })
    }

    /// What a harness left running is taken back over. The test waits for its stop on the
    /// session's updates, each a look; it doesn't sleep (CP-M2, AC-M2.3; the stop it asks for
    /// polls the process, as stopping one does).
    #[cfg(unix)]
    #[test]
    fn what_a_session_left_running_is_taken_back_over_after_a_restart() -> anyhow::Result<()> {
        use super::super::survivors::{self, Ran};
        crate::rt::block_on(async move {
            let dir = tempfile::tempdir()?;
            let file = dir.path().join("harness.json");
            let spec = Spec {
                kind: "claude".into(),
                cwd: Some(dir.path().to_string_lossy().into_owned()),
                program: Some(dir.path().join("no-such-claude").to_string_lossy().into_owned()),
                ..Spec::default()
            };
            // A harness that outlived its runner (a sleep stands in for it), what it left running
            // that the record has, and one only its mark says is its.
            let mut harness = std::process::Command::new("sleep").arg("60").spawn()?;
            let mut recorded = std::process::Command::new("sleep").arg("61").spawn()?;
            let Some(sleep_62) = survivors::sleeper(62) else {
                eprintln!("skipped: nothing here sleeps as a process whose environment can be read");
                let _ = harness.kill();
                let _ = recorded.kill();
                return Ok(());
            };
            // Started as an agent's processes start: in the session's group of processes where
            // this machine has them (found by it), by its mark elsewhere.
            let group = crate::os::contain::Group::of("left-running");
            let mut cmd = std::process::Command::new("sh");
            cmd.arg("-c").arg(format!("{sleep_62} >/dev/null 2>&1 & echo $!")).env(survivors::MARK, "left-running");
            if let Some(g) = &group {
                g.enter(&mut cmd);
            }
            let out = cmd.output()?;
            let marked: u32 = String::from_utf8_lossy(&out.stdout).trim().parse()?;
            let mine = survivors::scan(&[("me".to_owned(), Some(std::process::id()))]).remove("me").unwrap_or_default().under;
            let ran = |pid: u32| -> anyhow::Result<Ran> { mine.iter().find(|r| r.pid == pid).cloned().ok_or_else(|| anyhow::anyhow!("{pid} not seen")) };
            let (h, r) = (ran(harness.id())?, ran(recorded.id())?);
            {
                let (tx, _rx) = async_channel::unbounded();
                let host = sim(tx, Some(file.clone()), Limits::default());
                let s = host.open(Some("left-running".into()), spec.clone())?;
                s.turn_open.store(true, Ordering::Relaxed);
                *s.ran.lock() = (Some(h.clone()), vec![r.clone()]);
                host.save_now();
            }
            let (tx, rx) = async_channel::unbounded();
            let again = sim(tx, Some(file.clone()), Limits::default());
            again.restore();
            // The harness was stopped; what it started wasn't.
            let _ = harness.wait();
            assert!(survivors::alive(std::slice::from_ref(&h)).is_empty(), "the harness that outlived its runner is stopped");
            assert_eq!(survivors::alive(std::slice::from_ref(&r)).len(), 1);
            let s = again.sessions.lock().get("left-running").cloned().ok_or_else(|| anyhow::anyhow!("not restored"))?;
            let (tasks, queue) = {
                let core = s.core.lock();
                (core.state.tasks.clone(), core.state.queue.clone())
            };
            for pid in [recorded.id(), marked] {
                assert!(tasks.iter().any(|t| t.id == format!("left-{pid}") && t.status == "running" && t.left_running()), "{pid} taken back over: {tasks:?}");
            }
            let said = queue.first().map(|q| q.text.clone()).unwrap_or_default();
            assert!(said.contains("Pick up where you left off") && said.contains("sleep 61") && said.contains("62"), "{said}");
            // Stopped from kiln, it ends (waited for without holding up the session doing it): each
            // update the session sends is a look at whether its task says so yet.
            let task = format!("left-{}", recorded.id());
            s.command(Command::StopTask { task: task.clone() });
            let stopped = || s.core.lock().state.tasks.iter().any(|t| t.id == task && t.status == "stopped");
            let waited = crate::rt::timeout(Duration::from_secs(30), async {
                while !stopped() {
                    if rx.recv().await.is_err() {
                        return;
                    }
                }
            })
            .await;
            assert!(waited.is_ok() && stopped(), "its task says it stopped");
            assert!(survivors::alive(std::slice::from_ref(&r)).is_empty(), "and it ended");
            let _ = recorded.wait();
            survivors::stop(&[marked]);
            if let Some(g) = group {
                g.kill();
                g.remove();
            }
            Ok(())
        })
    }

    /// What a session shows as left running is set right: one that has ended is ended though no look
    /// took it over, one shown twice is shown once, and one taken over again isn't shown again.
    #[test]
    fn left_running_is_shown_once_and_ended_once_it_no_longer_runs() {
        use super::super::survivors::{self, Ran};
        let shown = |pid: u32| survivors::task(&Ran { pid, ppid: 1, started: 1_700_000_000, command: format!("sleep {pid}") });
        // 11 ended while no runner looked; 22 runs, shown twice by an older image; 33 is new.
        let mut tasks = vec![shown(11), shown(22), shown(22)];
        let apart = [22, 33].into_iter().collect();
        assert!(settle_left(&mut tasks, vec![shown(22), shown(33)], &apart, 1_800_000_000));
        let live: Vec<&str> = tasks.iter().filter(|t| t.live()).map(|t| t.id.as_str()).collect();
        assert_eq!(live, ["left-22", "left-33"]);
        let ended = tasks.iter().find(|t| t.id == "left-11").map(|t| (t.status.as_str(), t.ended));
        assert_eq!(ended, Some(("completed", Some(1_800_000_000_000))), "kept, as ended");
        // Nothing to set right, nothing changes.
        assert!(!settle_left(&mut tasks, vec![shown(22)], &apart, 1_800_000_010));
    }

    /// A session an upgrade handed over comes with its timeline, whose left running the watch never
    /// took over: its next look ends those that have ended since and shows the rest once, taken
    /// back over.
    #[test]
    fn a_look_sets_right_the_left_running_a_handed_over_session_came_with() -> anyhow::Result<()> {
        use super::super::survivors::{self, Ran, Seen};
        crate::rt::block_on(async move {
            let dir = tempfile::tempdir()?;
            let (tx, _rx) = async_channel::unbounded();
            let host = sim(tx, None, Limits::default());
            let s = host.open(Some("handed".into()), no_harness(dir.path()))?;
            let ran = |pid: u32| Ran { pid, ppid: 1, started: 1_700_000_000, command: format!("sleep {pid}") };
            // Its timeline shows 11 (ended during the upgrade) and 22 (twice); its record has 22.
            s.core.lock().state.tasks.extend([survivors::task(&ran(11)), survivors::task(&ran(22)), survivors::task(&ran(22))]);
            *s.ran.lock() = (None, vec![ran(22)]);
            host.note(&s, Seen::default(), &HashMap::from([(22, 1_700_000_000)]), 1_800_000_000);
            let tasks = s.core.lock().state.tasks.clone();
            let live: Vec<&str> = tasks.iter().filter(|t| t.live()).map(|t| t.id.as_str()).collect();
            assert_eq!(live, ["left-22"], "{tasks:?}");
            assert!(tasks.iter().any(|t| t.id == "left-11" && t.status == "completed"), "{tasks:?}");
            assert_eq!(s.left.lock().iter().map(|r| r.pid).collect::<Vec<_>>(), [22], "taken back over");
            // Once 22 ends, the next look ends it.
            host.note(&s, Seen::default(), &HashMap::new(), 1_800_000_010);
            assert!(s.core.lock().state.tasks.iter().all(|t| !t.live()), "nothing left running");
            Ok(())
        })
    }

    /// On a simulated clock of its own, as every host in these tests is (CP-M2, AC-M2.1).
    #[test]
    fn a_session_is_saved_and_restored_with_its_pause_without_deadlocking() -> anyhow::Result<()> {
        crate::rt::block_on(async move {
            let (tx, _rx) = async_channel::unbounded();
            let dir = tempfile::tempdir()?;
            let file = dir.path().join("harness.json");
            let host = sim(tx, Some(file.clone()), Limits::default());
            let spec = Spec {
                kind: "claude".into(),
                cwd: Some(dir.path().to_string_lossy().into_owned()),
                program: Some(dir.path().join("no-such-claude").to_string_lossy().into_owned()),
                ..Spec::default()
            };
            let s = host.open(Some("paused".into()), spec)?;
            {
                let mut core = s.core.lock();
                core.state.harness_session = Some("h1".into());
                core.state.pause = Some(super::super::model::Pause { reason: "limit".into(), since: 7, auto: true, by: "harness".into(), checking: Some(9), ..Default::default() });
                core.state.pause_at = Some(90);
            }
            // Saving takes each session's lock: it must never take one twice.
            let (done_tx, done_rx) = std::sync::mpsc::channel();
            let saver = host.clone();
            std::thread::spawn(move || {
                saver.save_now();
                let _ = done_tx.send(());
            });
            anyhow::ensure!(done_rx.recv_timeout(Duration::from_secs(5)).is_ok(), "saving the sessions deadlocked");
            let saved: Vec<serde_json::Value> = serde_json::from_slice(&std::fs::read(&file)?)?;
            let a = saved.first().ok_or_else(|| anyhow::anyhow!("nothing saved"))?;
            assert_eq!(a.pointer("/spec/resume").and_then(serde_json::Value::as_str), Some("h1"));
            assert_eq!(a.pointer("/pause/since").and_then(serde_json::Value::as_i64), Some(7));
            assert_eq!(a.get("pause_at").and_then(serde_json::Value::as_u64), Some(90));
            // Another start of the machine picks the pause up, its check under way asked again.
            let (tx2, _rx2) = async_channel::unbounded();
            let again = sim(tx2, Some(file.clone()), Limits::default());
            again.restore();
            let back = again.sessions.lock().get("paused").cloned().ok_or_else(|| anyhow::anyhow!("not restored"))?;
            let p = back.core.lock().state.pause.clone().ok_or_else(|| anyhow::anyhow!("pause not restored"))?;
            assert_eq!((p.since, p.checking), (7, None));
            assert_eq!(back.core.lock().state.pause_at, Some(90));
            Ok(())
        })
    }

    /// Ending an agent while its harness runs (and while it's down): the last state it sends
    /// before it closes, what the ended agent shows, has nothing still running.
    #[cfg(unix)]
    #[test]
    fn an_ended_agents_last_state_has_its_tasks_stopped() -> anyhow::Result<()> {
        use super::super::core::Update;
        use super::super::model::Task;
        use std::os::unix::fs::PermissionsExt;
        crate::rt::block_on(async move {
            let dir = tempfile::tempdir()?;
            // A harness that runs and says nothing.
            let quiet = dir.path().join("quiet-claude");
            std::fs::write(&quiet, "#!/bin/sh\nexec sleep 30\n")?;
            std::fs::set_permissions(&quiet, std::fs::Permissions::from_mode(0o755))?;
            for (id, program) in [("running", quiet.clone()), ("down", dir.path().join("no-such-claude"))] {
                let (tx, rx) = async_channel::unbounded();
                let host = Host::new(tx, None, Limits::default());
                let spec = Spec { kind: "claude".into(), cwd: Some(dir.path().to_string_lossy().into_owned()), program: Some(program.to_string_lossy().into_owned()), ..Spec::default() };
                let s = host.open(Some(id.into()), spec)?;
                // Its harness running (and its adapter started), or down (it could not start). One
                // lock at a time.
                let ready = |s: &Session| {
                    if id == "down" {
                        return s.core.lock().state.status == Status::Exited;
                    }
                    let spawned = s.pid.lock().is_some();
                    spawned && !s.core.lock().meta.features.is_empty()
                };
                let started = crate::rt::timeout(Duration::from_secs(10), async {
                    while !ready(&s) {
                        crate::rt::sleep(Duration::from_millis(50)).await;
                    }
                })
                .await;
                anyhow::ensure!(started.is_ok(), "{id}: the session never got going");
                {
                    let mut core = s.core.lock();
                    core.state.tasks.push(Task { id: "b7".into(), class: Some("shell".into()), status: "running".into(), background: true, stoppable: true, ..Task::default() });
                    core.touch_state();
                }
                assert!(host.close(id));
                let last = crate::rt::timeout(Duration::from_secs(20), async {
                    let mut last = None;
                    while let Ok((_, u)) = rx.recv().await {
                        match u {
                            Update::State { state } => last = Some(state),
                            Update::Closed => return Some(last),
                            _ => {}
                        }
                    }
                    None
                })
                .await;
                let state = last.ok().flatten().flatten().ok_or_else(|| anyhow::anyhow!("{id}: never closed, or no state before it"))?;
                assert!(state.tasks.iter().any(|t| t.id == "b7") && state.tasks.iter().all(|t| !t.live()), "{id}: {:?}", state.tasks);
            }
            Ok(())
        })
    }
}
