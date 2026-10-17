//! A machine's agent sessions (the server's own for `local`, a runner's for its machine): starts
//! them, routes commands to them, enforces limits, and keeps a small record on disk so that after
//! the process holding them restarts they continue where they were (each harness resumes its own
//! session), with kiln's queue of waiting messages intact.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

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

/// A session as kept on disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Saved {
    id: String,
    spec: Spec,
    #[serde(default)]
    queue: Vec<QueueItem>,
}

pub struct Host {
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    sink: Sink,
    pub limits: Limits,
    file: Option<PathBuf>,
    dirty: AtomicBool,
    me: Weak<Host>,
    /// Where files attached to messages are kept, one directory per session.
    files_dir: PathBuf,
    /// Files received (whole), by file id.
    received: Mutex<HashMap<String, PathBuf>>,
}

/// Largest file a message can carry.
pub const ATTACHMENT_MAX: u64 = 25 << 20;

impl Host {
    pub fn new(sink: Sink, file: Option<PathBuf>, limits: Limits) -> Arc<Self> {
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
            me: me.clone(),
            files_dir,
            received: Mutex::new(HashMap::new()),
        })
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

    /// A received file, once (its id is spent).
    fn take_file(&self, file: &str) -> Option<PathBuf> {
        self.received.lock().remove(file)
    }

    pub fn get(&self, id: &str) -> Option<Arc<Session>> {
        self.sessions.lock().get(id).cloned()
    }

    pub fn list(&self) -> Vec<Info> {
        let mut v: Vec<Info> = self.sessions.lock().values().map(|s| s.info()).collect();
        v.sort_by(|a, b| a.created_at.cmp(&b.created_at).then_with(|| a.id.cmp(&b.id)));
        v
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
        };
        let s = session::start(id.clone(), spec, self.sink.clone(), on_change, files)?;
        self.sessions.lock().insert(id, Arc::clone(&s));
        self.mark_dirty();
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
            // Its attachments go with it.
            let dir = self.files_dir.join(id);
            crate::rt::spawn(async move {
                let _ = crate::rt::blocking(move || std::fs::remove_dir_all(dir)).await;
            });
        }
        self.mark_dirty();
        found
    }

    /// Stop every harness process now (this process is about to re-execute itself; the sessions
    /// continue in the new image from the record saved here). Blocks up to a few seconds.
    pub fn stop_all(&self) {
        self.save_now();
        let pids: Vec<u32> = self.sessions.lock().values().filter_map(|s| *s.pid.lock()).collect();
        for pid in pids {
            stop_process(pid);
        }
    }

    /// Continue the sessions recorded before a restart.
    pub fn restore(&self) {
        let Some(file) = &self.file else { return };
        let Ok(raw) = std::fs::read(file) else { return };
        let saved: Vec<Saved> = match serde_json::from_slice(&raw) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, file = %file.display(), "unreadable agent session record; starting without it");
                return;
            }
        };
        for s in saved {
            let id = s.id.clone();
            match self.open(Some(s.id), s.spec) {
                Ok(session) => {
                    if !s.queue.is_empty() {
                        let mut core = session.core.lock();
                        core.state.queue = s.queue;
                        core.touch_state();
                    }
                    tracing::info!(session = %id, "agent session continued after a restart");
                }
                Err(e) => tracing::warn!(session = %id, error = %format!("{e:#}"), "could not continue an agent session"),
            }
        }
    }

    fn mark_dirty(&self) {
        if self.file.is_none() || self.dirty.swap(true, Ordering::AcqRel) {
            return;
        }
        let me = self.me.clone();
        crate::rt::spawn(async move {
            // Coalesce bursts (a session's id arriving, its queue changing) into one write.
            crate::rt::sleep(Duration::from_millis(500)).await;
            if let Some(h) = me.upgrade() {
                h.dirty.store(false, Ordering::Release);
                let _ = crate::rt::blocking(move || h.save_now()).await;
            }
        });
    }

    fn save_now(&self) {
        let Some(file) = &self.file else { return };
        let saved: Vec<Saved> = self
            .sessions
            .lock()
            .values()
            .map(|s| Saved { id: s.id.clone(), spec: s.resume_spec(), queue: s.core.lock().state.queue.clone() })
            .collect();
        let Ok(json) = serde_json::to_vec_pretty(&saved) else { return };
        let tmp = file.with_extension("tmp");
        let wrote = file.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| crate::util::write_secret(&tmp, &json, 0o600)).and_then(|()| std::fs::rename(&tmp, file));
        if let Err(e) = wrote {
            tracing::warn!(error = %e, file = %file.display(), "could not record agent sessions");
        }
    }
}

/// SIGTERM a harness's process group, then SIGKILL it if it is still there after 3 s.
#[cfg(unix)]
fn stop_process(pid: u32) {
    use nix::sys::signal::{Signal, killpg};
    use nix::unistd::Pid;
    let Ok(p) = i32::try_from(pid) else { return };
    if p <= 0 {
        return;
    }
    let _ = killpg(Pid::from_raw(p), Signal::SIGTERM);
    for _ in 0..30 {
        if !crate::os::pid_alive(pid) {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = killpg(Pid::from_raw(p), Signal::SIGKILL);
}

#[cfg(windows)]
fn stop_process(pid: u32) {
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
        tasks_running: 0,
        cost_usd: 0.0,
        activity: None,
        last_activity: now,
        exit: Some(exit.clone()),
        goal: spec.prompt.clone().map(|p| super::model::clip_middle(&p, 200).0).unwrap_or_default(),
        objective: None,
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
    let state = super::model::State { status: Status::Exited, exit: Some(exit), ..Default::default() };
    vec![super::core::Update::Open { info: Box::new(info) }, super::core::Update::State { state: Box::new(state) }]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_and_unknown_kinds_are_refused() -> anyhow::Result<()> {
        crate::rt::block_on(async move {
            let (tx, _rx) = async_channel::unbounded();
            let host = Host::new(tx, None, Limits { per_machine: 1, per_shell: 1 });
            assert!(host.open(None, Spec { kind: "nope".into(), ..Spec::default() }).is_err());
            let dir = tempfile::tempdir()?;
            // A program that does not exist: the session reports why it is down, nothing runs.
            let spec = Spec {
                kind: "claude".into(),
                cwd: Some(dir.path().to_string_lossy().into_owned()),
                program: Some(dir.path().join("no-such-claude").to_string_lossy().into_owned()),
                ..Spec::default()
            };
            let s = host.open(Some("a".into()), spec.clone())?;
            assert_eq!(s.id, "a");
            // The same id again is the same session; another one is over the limit.
            assert_eq!(host.open(Some("a".into()), spec.clone())?.id, "a");
            assert!(host.open(None, spec).is_err());
            assert_eq!(host.inventory().sessions, vec!["a".to_owned()]);
            assert!(host.close("a"));
            assert!(host.list().is_empty());
            Ok(())
        })
    }
}
