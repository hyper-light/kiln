//! Terminal sessions owned by the server, so a shell outlives any one browser: on the server
//! (`local`) or on a machine's agent (`agent:<name>`). Any number of browsers, on any device, can
//! attach to a session at once; output fans out to all of them and input from any of them reaches
//! the shell. Attaching replays recent output, and while a full-screen program (htop, vim, k9s, an
//! AI coding agent) owns the screen, a snapshot of that screen rendered by a server-side terminal
//! emulator, so a new device picks up exactly where the session is. A session ends when its shell
//! exits, when it is closed, or after it has sat with nobody attached for the idle limit.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures::FutureExt as _;
use parking_lot::Mutex;
use serde::Serialize;

use crate::hub::{Hub, TermOut};
use crate::proto::{LANE_TERM, ServerMsg};
use crate::rt::{Broadcast, Shutdown};

/// Raw output kept per session for replay on attach.
const BACKLOG_BYTES: usize = 256 * 1024;
/// An exited session stays listed this long, so attached devices can show how it ended.
const EXITED_LINGER: Duration = Duration::from_secs(60);
/// Live frames buffered per attached client before a slow one starts losing the oldest.
const FANOUT: usize = 1024;

/// What attached clients receive.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum Frame {
    Data(Bytes),
    Exit { code: Option<i32>, error: Option<String> },
    /// The session's size changed. Every view draws at exactly this size, so a program's
    /// cursor-addressed redraws line up on every device.
    Size { cols: u16, rows: u16 },
}

/// What attached clients send.
#[derive(Debug)]
pub enum Input {
    Data(Vec<u8>),
    Resize { cols: u16, rows: u16 },
}

/// A session as the API lists it.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "TerminalSessionInfo"))]
pub struct Info {
    pub id: String,
    pub target: String,
    pub command: Option<String>,
    /// The window title the program set (OSC 0/2), if any.
    pub title: String,
    /// The name a person gave it, if any (shown over the title).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub name: Option<String>,
    pub created_at: i64,
    pub attached: usize,
    pub cols: u16,
    pub rows: u16,
    pub idle_seconds: u64,
    pub exited: bool,
    pub exit_code: Option<i32>,
    pub error: Option<String>,
}

/// Captures the window title programs set, for the session list.
#[derive(Default)]
struct Titles {
    title: String,
}

impl vt100::Callbacks for Titles {
    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        self.title = String::from_utf8_lossy(title).chars().filter(|c| !c.is_control()).take(120).collect();
    }
}

struct Exit {
    code: Option<i32>,
    error: Option<String>,
    at: Instant,
}

struct State {
    backlog: VecDeque<u8>,
    /// The backlog has dropped its oldest bytes at least once (replay then starts at a line).
    trimmed: bool,
    screen: vt100::Parser<Titles>,
    attached: usize,
    last_activity: Instant,
    exit: Option<Exit>,
    cols: u16,
    rows: u16,
}

pub struct Session {
    pub id: String,
    pub target: String,
    pub command: Option<String>,
    pub created_at: i64,
    state: Mutex<State>,
    out: Broadcast<Frame>,
    input: async_channel::Sender<Input>,
    close: Shutdown,
}

/// What a client gets when it attaches.
pub struct Attachment {
    /// Geometry at the same boundary as replay and the live cursor.
    pub cols: u16,
    pub rows: u16,
    /// Bytes that bring a fresh terminal to the session's current state.
    pub replay: Vec<u8>,
    /// Everything after the replay.
    pub live: crate::bus::Receiver<Frame>,
    /// Set when the session had already ended.
    pub ended: Option<(Option<i32>, Option<String>)>,
}

impl Session {
    /// Replay state only, for a terminal whose PTY someone else drives (an agent's sessions).
    pub fn detached(id: String, target: String, command: Option<String>, cols: u16, rows: u16) -> Arc<Self> {
        Self::with_id(id, target, command, cols, rows).0
    }

    fn new(target: String, command: Option<String>, cols: u16, rows: u16) -> (Arc<Self>, async_channel::Receiver<Input>) {
        Self::with_id(crate::util::new_id(), target, command, cols, rows)
    }

    fn with_id(id: String, target: String, command: Option<String>, cols: u16, rows: u16) -> (Arc<Self>, async_channel::Receiver<Input>) {
        let (input, input_rx) = async_channel::bounded(1024);
        let cols = cols.clamp(2, 1000);
        let rows = rows.clamp(2, 1000);
        let session = Arc::new(Self {
            id,
            target,
            command,
            created_at: crate::util::now_unix(),
            state: Mutex::new(State {
                backlog: VecDeque::with_capacity(16 * 1024),
                trimmed: false,
                screen: vt100::Parser::new_with_callbacks(rows, cols, 0, Titles::default()),
                attached: 0,
                last_activity: Instant::now(),
                exit: None,
                cols,
                rows,
            }),
            out: Broadcast::new(FANOUT),
            input,
            close: Shutdown::new(),
        });
        (session, input_rx)
    }

    pub fn info(&self) -> Info {
        let st = self.state.lock();
        Info {
            id: self.id.clone(),
            target: self.target.clone(),
            command: self.command.clone(),
            title: st.screen.callbacks().title.clone(),
            name: None,
            created_at: self.created_at,
            attached: st.attached,
            cols: st.cols,
            rows: st.rows,
            idle_seconds: st.last_activity.elapsed().as_secs(),
            exited: st.exit.is_some(),
            exit_code: st.exit.as_ref().and_then(|e| e.code),
            error: st.exit.as_ref().and_then(|e| e.error.clone()),
        }
    }

    /// Nothing has been output yet.
    pub fn is_empty(&self) -> bool {
        self.state.lock().backlog.is_empty()
    }

    pub fn is_exited(&self) -> bool {
        self.state.lock().exit.is_some()
    }

    /// Program output: into the emulator and the backlog, then to every attached client, all under
    /// one lock so an attaching client sees each byte exactly once (in its replay or live).
    pub(crate) fn output(&self, data: &[u8]) {
        let mut st = self.state.lock();
        st.screen.process(data);
        st.backlog.extend(data.iter().copied());
        let excess = st.backlog.len().saturating_sub(BACKLOG_BYTES);
        if excess > 0 {
            st.backlog.drain(..excess);
            st.trimmed = true;
        }
        st.last_activity = Instant::now();
        let _ = self.out.send(Frame::Data(Bytes::copy_from_slice(data)));
    }

    fn finish(&self, code: Option<i32>, error: Option<String>) {
        let mut st = self.state.lock();
        if st.exit.is_some() {
            return;
        }
        st.exit = Some(Exit { code, error: error.clone(), at: Instant::now() });
        st.last_activity = Instant::now();
        let _ = self.out.send(Frame::Exit { code, error });
    }

    /// Join the session: the replay that reproduces its current state, then live output.
    pub fn attach(&self) -> Attachment {
        let mut st = self.state.lock();
        st.attached = st.attached.saturating_add(1);
        st.last_activity = Instant::now();
        let screen = st.screen.screen();
        let mut replay = b"\x1b[0m".to_vec();
        if screen.alternate_screen() {
            // A full-screen program owns the display: repaint it from the emulator.
            replay.extend_from_slice(b"\x1b[?1049h\x1b[H\x1b[2J");
            replay.extend(screen.state_formatted());
            replay.extend(screen.cursor_state_formatted());
        } else {
            // The shell's recent output ends in exactly the live state; once trimmed, start it
            // at a line boundary so the replay never opens mid escape sequence.
            let mut bytes: Vec<u8> = st.backlog.iter().copied().collect();
            if st.trimmed
                && let Some(nl) = bytes.iter().take(8192).position(|b| *b == b'\n')
            {
                bytes.drain(..=nl);
            }
            replay.extend(bytes);
            replay.extend(screen.input_mode_formatted());
        }
        let ended = st.exit.as_ref().map(|e| (e.code, e.error.clone()));
        let live = self.out.subscribe();
        Attachment { cols: st.cols, rows: st.rows, replay, live, ended }
    }

    pub fn detach(&self) {
        let mut st = self.state.lock();
        st.attached = st.attached.saturating_sub(1);
        st.last_activity = Instant::now();
    }

    /// Keystrokes or a resize from an attached client. A resize that changes the size is announced
    /// to every attached client (in order with the output, under the same lock).
    pub fn send(&self, input: Input) {
        let input = {
            let mut st = self.state.lock();
            st.last_activity = Instant::now();
            match input {
                Input::Resize { cols, rows } => {
                    let (cols, rows) = (cols.clamp(2, 1000), rows.clamp(2, 1000));
                    if (cols, rows) == (st.cols, st.rows) {
                        return;
                    }
                    st.cols = cols;
                    st.rows = rows;
                    st.screen.screen_mut().set_size(rows, cols);
                    let _ = self.out.send(Frame::Size { cols, rows });
                    Input::Resize { cols, rows }
                }
                data => data,
            }
        };
        if self.input.try_send(input).is_err() {
            tracing::debug!(session = %self.id, "terminal input dropped (session busy or ended)");
        }
    }

    /// End the session: the shell is killed.
    pub fn close(&self) {
        self.close.cancel();
    }
}

/// Every terminal session on this server. Physical PTYs stay in this process; accepted names
/// live in the injected host-qualified Records namespace.
pub struct Sessions {
    map: Mutex<HashMap<String, Arc<Session>>>,
    names: Mutex<HashMap<String, String>>,
    names_snapshot: crate::harness::host::snapshot::Snapshot,
    names_commit: Mutex<()>,
}

/// The longest name a terminal session takes.
const MAX_NAME: usize = 80;

/// U17: a missing physical session is distinct from unavailable/corrupt name persistence.
#[derive(Debug, thiserror::Error)]
#[error("terminal name storage: {0:#}")]
pub struct StorageError(anyhow::Error);

fn names_storage(error: impl Into<anyhow::Error>) -> anyhow::Error { StorageError(error.into()).into() }

impl Sessions {
    #[cfg(test)]
    pub fn new() -> anyhow::Result<Self> {
        Self::from_records(Arc::new(kiln_seams::records::SimRecords::default()), None)
    }

    /// U17: legacy terminal-names.json is a checked, read-only input consumed only when the
    /// canonical snapshot is absent. No sidecar remains a live writer after this constructor.
    pub fn from_records(records: Arc<dyn kiln_seams::Records>, legacy: Option<&std::path::Path>) -> anyhow::Result<Self> {
        let (snapshot, names) = crate::harness::host::snapshot::Snapshot::open(records, "terminal/names", legacy).map_err(names_storage)?;
        Ok(Self { map: Mutex::new(HashMap::new()), names: Mutex::new(names), names_snapshot: snapshot, names_commit: Mutex::new(()) })
    }

    /// False only when this process has no physical session with this identity. A name becomes
    /// visible and acknowledged only after its Records commit succeeds.
    pub fn rename(&self, id: &str, name: &str) -> anyhow::Result<bool> {
        let sessions = self.map.lock();
        if !sessions.contains_key(id) { return Ok(false); }
        let name: String = name.trim().chars().filter(|c| !c.is_control()).take(MAX_NAME).collect();
        self.edit_name(id, (!name.is_empty()).then_some(name), None)?;
        Ok(true)
    }

    /// A fresh read and its exact revision bind each delta: another process's unrelated name
    /// edits survive, and a reader cannot refresh the CAS revision of an older in-memory map.
    fn edit_name(&self, id: &str, name: Option<String>, expected: Option<&str>) -> anyhow::Result<()> {
        let _commit = self.names_commit.lock();
        for _ in 0..3 {
            let (revision, mut names) = self.names_snapshot.read_versioned::<HashMap<String, String>>().map_err(names_storage)?
                .ok_or_else(|| names_storage(anyhow::anyhow!("canonical terminal names disappeared")))?;
            // Lifecycle cleanup from an older process cannot erase a newer accepted rename.
            if expected.is_some_and(|previous| names.get(id).map(String::as_str) != Some(previous)) {
                *self.names.lock() = names;
                return Ok(());
            }
            if let Some(name) = &name { names.insert(id.to_owned(), name.clone()); } else { names.remove(id); }
            match self.names_snapshot.save_at(&names, revision) {
                Ok(()) => { *self.names.lock() = names; return Ok(()); }
                Err(error) if error.downcast_ref::<kiln_seams::records::Refusal>().is_some_and(|refusal| matches!(refusal, kiln_seams::records::Refusal::Conflict { .. })) => continue,
                Err(error) => return Err(names_storage(error)),
            }
        }
        Err(names_storage(anyhow::anyhow!("terminal names kept changing; rename was not committed")))
    }

    fn current_names(&self) -> HashMap<String, String> {
        let _commit = self.names_commit.lock();
        match self.names_snapshot.read_versioned::<HashMap<String, String>>() {
            Ok(Some((_, names))) => { *self.names.lock() = names.clone(); names }
            Ok(None) => { tracing::error!("U17 canonical terminal names disappeared; retaining last committed view"); self.names.lock().clone() }
            Err(error) => { tracing::error!(%error, "U17 terminal names unavailable; retaining last committed view"); self.names.lock().clone() }
        }
    }

    pub fn get(&self, id: &str) -> Option<Arc<Session>> {
        self.map.lock().get(id).cloned()
    }

    pub fn list(&self) -> Vec<Info> {
        let names = self.current_names();
        let mut v: Vec<Info> = self
            .map
            .lock()
            .values()
            .map(|s| {
                let mut i = s.info();
                i.name = names.get(&i.id).cloned();
                i
            })
            .collect();
        v.sort_by(|a, b| a.created_at.cmp(&b.created_at).then_with(|| a.id.cmp(&b.id)));
        v
    }

    /// The sessions people see: not those kiln opens for itself ([`hidden`]).
    pub fn visible(&self) -> Vec<Info> {
        self.list().into_iter().filter(|i| !hidden(i.command.as_deref())).collect()
    }

    /// Sessions whose shell is still running.
    pub fn live(&self) -> usize {
        self.map.lock().values().filter(|s| !s.is_exited()).count()
    }

    pub fn remove(&self, id: &str) {
        self.map.lock().remove(id);
        self.forget_name(id);
    }

    fn forget_name(&self, id: &str) {
        let previous = self.names.lock().get(id).cloned();
        if let Some(previous) = previous
            && let Err(error) = self.edit_name(id, None, Some(&previous))
        {
            tracing::error!(%error, session = id, "U17 terminal name cleanup not committed; durable name retained");
        }
    }

    fn insert(&self, s: &Arc<Session>) {
        self.map.lock().insert(s.id.clone(), Arc::clone(s));
    }

    /// Close sessions nobody has attached to for `idle`, and forget exited ones nobody watches.
    /// Returns whether anything changed.
    pub fn sweep(&self, idle: Duration) -> bool {
        let mut changed = false;
        let sessions: Vec<Arc<Session>> = self.map.lock().values().cloned().collect();
        for s in sessions {
            let (attached, idle_for, exited_for) = {
                let st = s.state.lock();
                (st.attached, st.last_activity.elapsed(), st.exit.as_ref().map(|e| e.at.elapsed()))
            };
            match exited_for {
                Some(gone) if attached == 0 || gone > EXITED_LINGER * 10 => {
                    if gone > EXITED_LINGER || attached == 0 {
                        self.map.lock().remove(&s.id);
                        self.forget_name(&s.id);
                        changed = true;
                    }
                }
                Some(_) => {}
                None if attached == 0 && idle_for > idle => {
                    tracing::info!(session = %s.id, target = %s.target, "closing terminal session left idle");
                    s.close();
                    changed = true;
                }
                None => {}
            }
        }
        changed
    }
}

/// Open a shell on this server.
pub fn open_local(hub: &Arc<Hub>, command: Option<String>, cols: u16, rows: u16) -> anyhow::Result<Arc<Session>> {
    let id = crate::util::new_id();
    // `kiln spawn` in this terminal starts agents on this server, under this terminal.
    let env = hub.harness.terminal_env(&id);
    let (pty, pty_out) = crate::terminal::spawn_with(hub.cfg.terminal.shell.as_deref(), command.as_deref(), cols, rows, &env)?;
    hub.harness.note_shell(&id, pty.pid());
    let (session, input) = Session::with_id(id, "local".to_owned(), command, cols, rows);
    hub.terminals.insert(&session);
    let s = Arc::clone(&session);
    let h = Arc::clone(hub);
    crate::rt::spawn(async move {
        let mut pty = Some(pty);
        enum Ev {
            Out(Option<crate::terminal::PtyOut>),
            In(Option<Input>),
            Close,
        }
        loop {
            let ev = futures::select_biased! {
                () = s.close.cancelled().fuse() => Ev::Close,
                o = pty_out.recv().fuse() => Ev::Out(o.ok()),
                i = input.recv().fuse() => Ev::In(i.ok()),
            };
            match ev {
                Ev::Out(Some(crate::terminal::PtyOut::Data(d))) => s.output(&d),
                Ev::Out(Some(crate::terminal::PtyOut::Exit(code))) => {
                    s.finish(code, None);
                    break;
                }
                Ev::Out(None) => {
                    s.finish(None, None);
                    break;
                }
                Ev::In(Some(Input::Data(d))) => {
                    if let Some(p) = &pty {
                        p.write(d);
                    }
                }
                Ev::In(Some(Input::Resize { cols, rows })) => {
                    if let Some(p) = &pty {
                        p.resize(cols, rows);
                    }
                }
                Ev::In(None) | Ev::Close => {
                    // Dropping the PTY session kills the shell.
                    drop(pty.take());
                    s.finish(None, Some("session closed".to_owned()));
                    break;
                }
            }
        }
        h.emit_terminals();
    });
    hub.emit_terminals();
    Ok(session)
}

/// A terminal kiln opened for itself, upgrading an older runner ([`crate::updates::HIDDEN_MARK`])
/// or signing a harness in or out ([`crate::accounts::MARK`]), or one beside an agent, shown on
/// that agent's page ([`crate::harness::screen::MARK`]): never listed as anyone's terminal.
pub fn hidden(command: Option<&str>) -> bool {
    command.is_some_and(|c| c.contains(crate::updates::HIDDEN_MARK) || c.contains(crate::accounts::MARK) || c.contains(crate::harness::screen::MARK))
}

/// Open a shell on a machine through its agent.
pub fn open_agent(hub: &Arc<Hub>, machine: String, command: Option<String>, cols: u16, rows: u16) -> Arc<Session> {
    let (session, input) = Session::new(format!("agent:{machine}"), command.clone(), cols, rows);
    run_agent(hub, machine, session.clone(), input, Some(ServerMsg::TermOpen { session: session.id.clone(), cols, rows, command }));
    session
}

/// A terminal inside a CI leg on `machine` (docs/ci.md W17-R01): the runner opens it in the leg's
/// workspace and environment, or its container, while the leg runs or is kept.
pub fn open_leg(hub: &Arc<Hub>, machine: String, job: &str, cols: u16, rows: u16) -> Arc<Session> {
    let (session, input) = Session::new(format!("leg:{job}"), None, cols, rows);
    run_agent(hub, machine, session.clone(), input, Some(ServerMsg::JobTerminalOpen { job: job.to_owned(), session: session.id.clone(), cols, rows }));
    session
}

/// A command in a leg's shell, its output on a session of its own (W17-R05): attach to it before
/// anything can come.
pub fn exec_leg(hub: &Arc<Hub>, machine: String, job: &str, command: &str) -> (Arc<Session>, Attachment) {
    let (session, input) = Session::new(format!("leg:{job}"), Some(command.to_owned()), 200, 50);
    let attached = session.attach();
    run_agent(hub, machine, session.clone(), input, Some(ServerMsg::JobExec { job: job.to_owned(), session: session.id.clone(), command: command.to_owned() }));
    (session, attached)
}

/// Take back a session an agent reports after a reconnect (it outlived the connection or this
/// server): register it and fetch its replay. A no-op when the session is already known.
pub fn adopt_agent(hub: &Arc<Hub>, machine: &str, info: &crate::proto::TermInfo) {
    if hub.terminals.get(&info.session).is_some() {
        return;
    }
    let (session, input) = Session::with_id(info.session.clone(), format!("agent:{machine}"), info.command.clone(), info.cols, info.rows);
    run_agent(hub, machine.to_owned(), session, input, Some(ServerMsg::TermAttach { session: info.session.clone() }));
}

fn run_agent(hub: &Arc<Hub>, machine: String, s: Arc<Session>, input: async_channel::Receiver<Input>, first: Option<ServerMsg>) {
    hub.terminals.insert(&s);
    let h = Arc::clone(hub);
    crate::rt::spawn(async move {
        let key = format!("{machine}/{}", s.id);
        let (out_tx, out_rx) = async_channel::bounded::<TermOut>(4096);
        h.terms.lock().insert(key.clone(), out_tx);
        // A terminal to open is a live command (`crate::wire::commands`): refused at once while its
        // machine is away, and answered once it opened it. Unanswered, it goes on: its output says.
        let failed = match first {
            Some(msg) if matches!(crate::wire::server_class(&msg), crate::wire::Class::Command(_)) => {
                match h.command_start(&machine, &msg).await.answer(crate::hub::COMMAND_WITHIN).await {
                    a @ (crate::wire::commands::Answer::Refused { .. } | crate::wire::commands::Answer::Unreachable { .. }) => Some(a.words()),
                    _ => None,
                }
            }
            Some(msg) => (!h.send_to_agent(&machine, LANE_TERM, &msg).await).then(|| format!("machine {machine} is not connected")),
            None => None,
        };
        if let Some(why) = failed {
            h.terms.lock().remove(&key);
            s.finish(None, Some(why));
            h.emit_terminals();
            return;
        }
        crate::metrics::global().terminal_sessions.add(1);
        enum Ev {
            Out(Option<TermOut>),
            In(Option<Input>),
            Close,
        }
        loop {
            let ev = futures::select_biased! {
                () = s.close.cancelled().fuse() => Ev::Close,
                o = out_rx.recv().fuse() => Ev::Out(o.ok()),
                i = input.recv().fuse() => Ev::In(i.ok()),
            };
            match ev {
                Ev::Out(Some(TermOut::Data(d))) => s.output(&d),
                Ev::Out(Some(TermOut::Replay(d))) => {
                    // An adopted session starts from the agent's picture of it. One that has a
                    // picture already (its machine's output stream started over, docs/wire.md) is
                    // repainted from the new one.
                    if !s.is_empty() {
                        s.output(b"\x1bc");
                    }
                    s.output(&d);
                }
                Ev::Out(Some(TermOut::Exit { code, error })) => {
                    s.finish(code, error);
                    break;
                }
                Ev::Out(Some(TermOut::Gone) | None) => {
                    // The machine went away; the shell keeps running there and is adopted again
                    // when it reconnects. Attached browsers reconnect by session id.
                    h.terminals.remove(&s.id);
                    let _ = s.out.send(Frame::Exit { code: None, error: Some("__reconnect__".to_owned()) });
                    break;
                }
                // Commands (`crate::wire::commands`), in order, each answered, none waited for:
                // keystrokes follow one another closely. Keystrokes are live (refused at once while
                // the machine is away, never typed later); a size is its newest, and a close waits
                // for the machine to come back.
                Ev::In(Some(Input::Data(data))) => {
                    h.command_start(&machine, &ServerMsg::TermInput { session: s.id.clone(), data }).await.log_when_answered(crate::hub::LOG_WITHIN);
                }
                Ev::In(Some(Input::Resize { cols, rows })) => {
                    h.command_start(&machine, &ServerMsg::TermResize { session: s.id.clone(), cols, rows }).await.log_when_answered(crate::hub::LOG_WITHIN);
                }
                Ev::In(None) | Ev::Close => {
                    h.command_start(&machine, &ServerMsg::TermClose { session: s.id.clone() }).await.log_when_answered(crate::hub::LOG_WITHIN);
                    s.finish(None, Some("session closed".to_owned()));
                    break;
                }
            }
        }
        h.terms.lock().remove(&key);
        crate::metrics::global().terminal_sessions.add(-1);
        h.emit_terminals();
    });
    hub.emit_terminals();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn u17_terminal_names_survive_reopen_merge_and_refused_writes() -> anyhow::Result<()> {
        use kiln_seams::records::{Records, Refusal, SimLimits, SimRecords, VALUE_MAX};
        let dir = tempfile::tempdir()?;
        let file = dir.path().join("terminal-names.json");
        std::fs::write(&file, br#"{"t1":"legacy"}"#)?;
        let records: Arc<dyn Records> = Arc::new(SimRecords::default());
        let reg = Sessions::from_records(records.clone(), Some(&file))?;
        reg.insert(&Session::detached("t1".into(), "local".into(), None, 80, 24));
        assert!(!reg.rename("nope", "x")?, "no such physical session");
        assert!(reg.rename("t1", "  builds\u{7}  ")?);
        assert_eq!(reg.list().first().and_then(|i| i.name.clone()).as_deref(), Some("builds"));
        // The committed import ignores an obsolete source, including one that is now corrupt.
        std::fs::write(&file, b"obsolete corrupt sidecar")?;
        let again = Sessions::from_records(records.clone(), Some(&file))?;
        again.insert(&Session::detached("t1".into(), "local".into(), None, 80, 24));
        again.insert(&Session::detached("t2".into(), "local".into(), None, 80, 24));
        assert_eq!(again.list().iter().find(|i| i.id == "t1").and_then(|i| i.name.as_deref()), Some("builds"));
        assert!(reg.rename("t1", "first edit")?);
        assert!(again.rename("t2", "second edit")?);
        assert_eq!(again.list().iter().find(|i| i.id == "t1").and_then(|i| i.name.as_deref()), Some("first edit"), "a stale instance's unrelated delta must not dump its old map over this name");
        assert!(again.rename("t1", "")?);
        assert_eq!(reg.list().first().and_then(|i| i.name.clone()), None, "reads observe another process's committed clear");
        assert!(again.rename("t1", "deploys")?);
        reg.remove("t1");
        assert_eq!(again.list().iter().find(|i| i.id == "t1").and_then(|i| i.name.as_deref()), Some("deploys"), "stale cleanup cannot erase a newer rename");
        again.remove("t1");
        let third = Sessions::from_records(records, Some(&file))?;
        assert!(!third.names.lock().contains_key("t1"));
        assert_eq!(third.names.lock().get("t2").map(String::as_str), Some("second edit"));
        assert_eq!(std::fs::read(&file)?.as_slice(), b"obsolete corrupt sidecar", "canonical changes never rewrite the legacy input");
        // A first import is checked and can resume after the actual corrupt input is repaired.
        let fresh: Arc<dyn Records> = Arc::new(SimRecords::default());
        assert!(Sessions::from_records(fresh.clone(), Some(&file)).is_err());
        std::fs::write(&file, br#"{"t1":"repaired"}"#)?;
        assert_eq!(Sessions::from_records(fresh, Some(&file))?.names.lock().get("t1").map(String::as_str), Some("repaired"));
        // A rename that crosses the segment boundary needs more rows. The simulator refuses
        // that write atomically; neither memory nor the acknowledged canonical value changes.
        let mut large = HashMap::from([("t1".to_owned(), "old".to_owned()), ("padding".to_owned(), String::new())]);
        let base = serde_json::to_vec(&serde_json::json!({"version":1,"state":&large}))?.len();
        large.insert("padding".to_owned(), "x".repeat(VALUE_MAX.saturating_sub(base + 16)));
        let legacy = serde_json::to_vec(&large)?;
        std::fs::write(&file, &legacy)?;
        let bounded: Arc<dyn Records> = Arc::new(SimRecords::with_limits(SimLimits { rows: 1, ..Default::default() }));
        let limited = Sessions::from_records(bounded, Some(&file))?;
        limited.insert(&Session::detached("t1".into(), "local".into(), None, 80, 24));
        let refused = limited.rename("t1", &"n".repeat(MAX_NAME)).err().ok_or_else(|| anyhow::anyhow!("rename committed beyond the store's capacity"))?;
        let storage = refused.downcast_ref::<StorageError>().ok_or_else(|| anyhow::anyhow!("storage refusal lost its type"))?;
        assert!(matches!(storage.0.downcast_ref::<Refusal>(), Some(Refusal::Full { .. })));
        assert_eq!(limited.names.lock().get("t1").map(String::as_str), Some("old"), "a refused write must not publish its name in memory");
        assert_eq!(limited.list().first().and_then(|i| i.name.clone()).as_deref(), Some("old"));
        assert_eq!(std::fs::read(&file)?, legacy);
        Ok(())
    }

    fn feed(s: &Session, b: &[u8]) {
        s.output(b);
    }

    #[test]
    fn replay_is_exact_for_shells_and_snapshots_full_screen_programs() {
        let (s, _input) = Session::new("local".into(), None, 80, 24);
        feed(&s, b"$ echo hi\r\nhi\r\n$ ");
        let a = s.attach();
        assert!(a.replay.ends_with(b"$ echo hi\r\nhi\r\n$ ") || a.replay.windows(5).any(|w| w == b"hi\r\n$"));
        assert!(a.ended.is_none());

        // A full-screen program: the replay repaints its screen instead of replaying history.
        feed(&s, b"\x1b[?1049h\x1b[H\x1b[2JTOP 1\r\nload 0.5");
        let b = s.attach();
        assert!(b.replay.starts_with(b"\x1b[0m\x1b[?1049h"));
        let text = String::from_utf8_lossy(&b.replay);
        assert!(text.contains("TOP 1") && text.contains("load 0.5"), "{text}");
        assert!(!text.contains("echo hi"), "{text}");
        assert_eq!(s.info().attached, 2);
        s.detach();
        assert_eq!(s.info().attached, 1);
    }

    #[test]
    fn backlog_is_bounded_and_replay_starts_at_a_line() {
        let (s, _input) = Session::new("local".into(), None, 80, 24);
        let line = b"0123456789abcdefghijklmnopqrstuvwxyz\r\n";
        for _ in 0..(BACKLOG_BYTES / line.len() + 500) {
            feed(&s, line);
        }
        let a = s.attach();
        assert!(a.replay.len() <= BACKLOG_BYTES + 64);
        // After the reset prefix the replay begins with a whole line.
        assert!(a.replay.get(4..).is_some_and(|r| r.starts_with(b"0123")));
    }

    #[test]
    fn resizes_are_announced_once_and_clamped() {
        let (s, input) = Session::new("local".into(), None, 80, 24);
        let mut a = s.attach();
        s.send(Input::Resize { cols: 120, rows: 40 });
        s.send(Input::Resize { cols: 120, rows: 40 }); // unchanged: no frame, nothing to the PTY
        s.send(Input::Resize { cols: 1, rows: 5000 });
        let mut sizes = Vec::new();
        while let Some(Ok(f)) = a.live.recv().now_or_never() {
            if let Frame::Size { cols, rows } = f {
                sizes.push((cols, rows));
            }
        }
        assert_eq!(sizes, vec![(120, 40), (2, 1000)]);
        assert_eq!((s.info().cols, s.info().rows), (2, 1000));
        let mut sent = Vec::new();
        while let Ok(Input::Resize { cols, rows }) = input.try_recv() {
            sent.push((cols, rows));
        }
        assert_eq!(sent, vec![(120, 40), (2, 1000)]);
        let snapshot = s.attach();
        assert_eq!((snapshot.cols, snapshot.rows), (2, 1000));
        s.detach();
    }

    #[test]
    fn titles_exits_and_sweeping() -> anyhow::Result<()> {
        let sessions = Sessions::new()?;
        let (s, _input) = Session::new("local".into(), None, 80, 24);
        sessions.insert(&s);
        feed(&s, b"\x1b]0;claude: fixing tests\x07ready");
        assert_eq!(s.info().title, "claude: fixing tests");
        // Attached and live: never swept.
        let _a = s.attach();
        assert!(!sessions.sweep(Duration::ZERO));
        // Detached for longer than the idle limit: closed.
        s.detach();
        assert!(sessions.sweep(Duration::ZERO));
        assert!(s.close.is_cancelled());
        // Exited with nobody attached: forgotten.
        s.finish(Some(0), None);
        assert!(sessions.sweep(Duration::from_secs(3600)));
        assert!(sessions.get(&s.id).is_none());
        let late = s.attach();
        assert_eq!(late.ended, Some((Some(0), None)));
        Ok(())
    }
}
