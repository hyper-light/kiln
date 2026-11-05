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
use crate::rcu::Rcu;
use crate::proto::{LANE_TERM, ServerMsg};
use crate::rt::Shutdown;

/// Raw output kept per session for replay on attach; and the most of its live frames' bytes kept for
/// a slow viewer (goal 11, plan 01a1a87b2e56 step 3: `crate::memory::owners::TERMINAL_FEEDS`): one that
/// falls further behind is repainted from this backlog, which costs it less than catching up would.
const BACKLOG_BYTES: usize = 256 * 1024;
/// An exited session stays listed this long, so attached devices can show how it ended.
const EXITED_LINGER: Duration = Duration::from_secs(60);
/// Live frames buffered per attached client before a slow one starts losing the oldest (or past
/// [`BACKLOG_BYTES`] of them).
const FANOUT: usize = 1024;

/// What attached clients receive.
#[derive(Clone, Debug)]
pub enum Frame {
    Data(Bytes),
    Exit { code: Option<i32>, error: Option<String> },
    /// The session's size changed. Every view draws at exactly this size, so a program's
    /// cursor-addressed redraws line up on every device.
    Size { cols: u16, rows: u16 },
}

/// A frame's place and its bytes, as its session's ring keeps it for its viewers (goal 11, plan
/// 01a1a87b2e56 step 3).
impl crate::memory::queue::Weigh for Frame {
    fn weight(&self) -> usize {
        size_of::<Self>().saturating_add(match self {
            Frame::Data(d) => d.len(),
            Frame::Exit { error, .. } => error.as_ref().map_or(0, String::capacity),
            Frame::Size { .. } => 0,
        })
    }
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

#[derive(Clone)]
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
    cols: u16,
    rows: u16,
}

pub struct Session {
    pub id: String,
    pub target: String,
    pub command: Option<String>,
    pub created_at: i64,
    /// Its screen and backlog: taken by its relay for its output (in order) and by its viewers as they
    /// attach or resize it, so a replay and the live output meet with no byte twice or lost. What its list
    /// shows of it, a keystroke and a detach take no lock (goal 5, plan 01a1a59c2aae step 2): the
    /// figures below are atomics and copies kept beside it.
    state: Mutex<State>,
    /// How many are attached.
    attached: std::sync::atomic::AtomicUsize,
    /// When it was made; and when it last had output, input or a viewer come or go, in nanoseconds after
    /// that (monotonic, as the `Instant` it kept before).
    born: Instant,
    active: std::sync::atomic::AtomicU64,
    /// Its size, columns in the high half ([`pack`]).
    size: std::sync::atomic::AtomicU32,
    /// The window title its program set, a copy swapped in as it sets another.
    title: Rcu<String>,
    /// How it ended, once it has (set under its lock, in order with its output).
    exit: Rcu<Option<Exit>>,
    /// It has had output.
    said: std::sync::atomic::AtomicBool,
    out: crate::memory::ring::Ring<Frame>,
    input: async_channel::Sender<Input>,
    close: Shutdown,
    /// Output the hub had no room for in this session's relay ([`forward`]): gaps its relay repairs
    /// from the machine's replay of the session. Only rises.
    gaps: std::sync::atomic::AtomicU64,
    /// Rung as `gaps` rises, so a relay with nothing else to do repairs at once.
    gap: crate::rt::Notify,
}

/// A size as one word: columns in the high half, rows in the low ([`Session::size`]).
fn pack(cols: u16, rows: u16) -> u32 {
    (u32::from(cols) << u16::BITS) | u32::from(rows)
}

/// [`pack`]'s columns and rows.
fn unpack(size: u32) -> (u16, u16) {
    (u16::try_from(size >> u16::BITS).unwrap_or(u16::MAX), u16::try_from(size & u32::from(u16::MAX)).unwrap_or(u16::MAX))
}

/// What a client gets when it attaches.
pub struct Attachment {
    /// Bytes that bring a fresh terminal to the session's current state.
    pub replay: Vec<u8>,
    /// Everything after the replay.
    pub live: crate::memory::ring::Subscription<Frame>,
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
                cols,
                rows,
            }),
            attached: std::sync::atomic::AtomicUsize::new(0),
            born: Instant::now(),
            active: std::sync::atomic::AtomicU64::new(0),
            size: std::sync::atomic::AtomicU32::new(pack(cols, rows)),
            title: Rcu::default(),
            exit: Rcu::default(),
            said: std::sync::atomic::AtomicBool::new(false),
            out: crate::memory::ring::Ring::new(&crate::memory::owners::TERMINAL_FEEDS, FANOUT, BACKLOG_BYTES),
            input,
            close: Shutdown::new(),
            gaps: std::sync::atomic::AtomicU64::new(0),
            gap: crate::rt::Notify::new(),
        });
        (session, input_rx)
    }

    /// Output its relay had no room for (goal 3, plan 01a1a59c2a8e step 5): a gap the relay repairs.
    fn lost(&self) {
        self.gaps.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        self.gap.notify();
    }

    /// The gaps in its output so far ([`forward`]).
    pub fn gaps(&self) -> u64 {
        self.gaps.load(std::sync::atomic::Ordering::Acquire)
    }

    /// As the list shows it, read without its lock.
    pub fn info(&self) -> Info {
        let (cols, rows) = unpack(self.size.load(std::sync::atomic::Ordering::Acquire));
        let exit = self.ended();
        Info {
            id: self.id.clone(),
            target: self.target.clone(),
            command: self.command.clone(),
            title: String::clone(&self.title.load()),
            name: None,
            created_at: self.created_at,
            attached: self.attached.load(std::sync::atomic::Ordering::Acquire),
            cols,
            rows,
            idle_seconds: self.idle().as_secs(),
            exited: exit.is_some(),
            exit_code: exit.as_ref().and_then(|e| e.code),
            error: exit.and_then(|e| e.error),
        }
    }

    /// How it ended, if it has.
    fn ended(&self) -> Option<Exit> {
        Option::<Exit>::clone(&self.exit.load())
    }

    /// How long since it last had output, input or a viewer come or go.
    fn idle(&self) -> Duration {
        self.born.elapsed().saturating_sub(Duration::from_nanos(self.active.load(std::sync::atomic::Ordering::Acquire)))
    }

    /// It's active now.
    fn touch(&self) {
        let now = u64::try_from(self.born.elapsed().as_nanos()).unwrap_or(u64::MAX);
        self.active.fetch_max(now, std::sync::atomic::Ordering::AcqRel);
    }

    /// Nothing has been output yet.
    pub fn is_empty(&self) -> bool {
        !self.said.load(std::sync::atomic::Ordering::Acquire)
    }

    pub fn is_exited(&self) -> bool {
        self.exit.load().is_some()
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
        // A title the program set anew goes into the list's copy.
        let title = &st.screen.callbacks().title;
        if *title != **self.title.load() {
            self.title.set(title.clone());
        }
        self.touch();
        if !data.is_empty() {
            self.said.store(true, std::sync::atomic::Ordering::Release);
        }
        self.out.send(Frame::Data(Bytes::copy_from_slice(data)));
    }

    fn finish(&self, code: Option<i32>, error: Option<String>) {
        // Under its lock: in order with its output, and with a viewer's attach (its end, or its Exit).
        let _st = self.state.lock();
        if self.exit.load().is_some() {
            return;
        }
        self.exit.set(Some(Exit { code, error: error.clone(), at: Instant::now() }));
        self.touch();
        self.out.send(Frame::Exit { code, error });
    }

    /// Join the session: the replay that reproduces its current state, then live output.
    pub fn attach(&self) -> Attachment {
        let st = self.state.lock();
        self.attached.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        self.touch();
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
        let ended = self.ended().map(|e| (e.code, e.error));
        let live = self.out.subscribe();
        Attachment { replay, live, ended }
    }

    pub fn detach(&self) {
        // One fewer, never below none.
        let mut n = self.attached.load(std::sync::atomic::Ordering::Acquire);
        while n > 0 {
            match self.attached.compare_exchange_weak(n, n.saturating_sub(1), std::sync::atomic::Ordering::AcqRel, std::sync::atomic::Ordering::Acquire) {
                Ok(_) => break,
                Err(now) => n = now,
            }
        }
        self.touch();
    }

    /// Keystrokes or a resize from an attached client. A resize that changes the size is announced
    /// to every attached client (in order with the output, under the same lock); keystrokes take none.
    pub fn send(&self, input: Input) {
        self.touch();
        let input = match input {
            Input::Resize { cols, rows } => {
                let (cols, rows) = (cols.clamp(2, 1000), rows.clamp(2, 1000));
                let mut st = self.state.lock();
                if (cols, rows) == (st.cols, st.rows) {
                    return;
                }
                st.cols = cols;
                st.rows = rows;
                st.screen.screen_mut().set_size(rows, cols);
                self.size.store(pack(cols, rows), std::sync::atomic::Ordering::Release);
                self.out.send(Frame::Size { cols, rows });
                Input::Resize { cols, rows }
            }
            data => data,
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

/// Every terminal session on this server.
pub struct Sessions {
    /// Read on every terminal's frame and list without a lock, a copy swapped in whole as one opens
    /// or goes (goal 5, plan 01a1a59c2aae step 2: `crate::rcu`), as are the two below.
    map: Rcu<HashMap<String, Arc<Session>>>,
    /// Names people gave sessions, by session id: kept in `names_file`, so they outlive this
    /// server's restarts and a machine's reconnects (its sessions come back under their ids).
    names: Rcu<HashMap<String, String>>,
    names_file: Rcu<Option<std::path::PathBuf>>,
}

/// The longest name a terminal session takes.
const MAX_NAME: usize = 80;

impl Default for Sessions {
    fn default() -> Self {
        Self::new()
    }
}

impl Sessions {
    pub fn new() -> Self {
        Self { map: Rcu::default(), names: Rcu::default(), names_file: Rcu::default() }
    }

    /// Keeps sessions' names in `file` (read now, written as they change).
    pub fn keep_names_in(&self, file: std::path::PathBuf) {
        if let Ok(raw) = std::fs::read(&file) {
            match serde_json::from_slice::<HashMap<String, String>>(&raw) {
                Ok(names) => self.names.set(names),
                Err(e) => tracing::warn!(file = %file.display(), error = %e, "unreadable terminal names; starting without them"),
            }
        }
        self.names_file.set(Some(file));
    }

    /// Names a session ("" takes its name away, back to its window title). False when there is
    /// no such session.
    pub fn rename(&self, id: &str, name: &str) -> bool {
        if self.get(id).is_none() {
            return false;
        }
        let name: String = name.trim().chars().filter(|c| !c.is_control()).take(MAX_NAME).collect();
        self.names.update(|names| {
            if name.is_empty() {
                names.remove(id);
            } else {
                names.insert(id.to_owned(), name.clone());
            }
        });
        self.save_names();
        true
    }

    /// Kept off the calling thread (goal 3, plan 01a1a59c2a8e step 5: `crate::keep`): written here, a
    /// rename waited on the disk, on the shard of the request that made it.
    fn save_names(&self) {
        let Some(file) = Option::<std::path::PathBuf>::clone(&self.names_file.load()) else { return };
        let Ok(json) = serde_json::to_vec_pretty(&*self.names.get()) else { return };
        crate::keep::later(file, json);
    }

    pub fn get(&self, id: &str) -> Option<Arc<Session>> {
        self.map.load().get(id).cloned()
    }

    pub fn list(&self) -> Vec<Info> {
        let names = self.names.load();
        let mut v: Vec<Info> = self
            .map
            .load()
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
        self.map.load().values().filter(|s| !s.is_exited()).count()
    }

    pub fn remove(&self, id: &str) {
        self.map.update(|m| m.remove(id));
        self.forget_name(id);
    }

    fn forget_name(&self, id: &str) {
        let had = self.names.load().contains_key(id) && self.names.update(|m| m.remove(id)).is_some();
        if had {
            self.save_names();
        }
    }

    pub(crate) fn insert(&self, s: &Arc<Session>) {
        self.map.update(|m| m.insert(s.id.clone(), Arc::clone(s)));
    }

    /// Close sessions nobody has attached to for `idle`, and forget exited ones nobody watches.
    /// Returns whether anything changed.
    pub fn sweep(&self, idle: Duration) -> bool {
        let mut changed = false;
        let sessions: Vec<Arc<Session>> = self.map.load().values().cloned().collect();
        for s in sessions {
            let (attached, idle_for, exited_for) = (s.attached.load(std::sync::atomic::Ordering::Acquire), s.idle(), s.ended().map(|e| e.at.elapsed()));
            match exited_for {
                Some(gone) if attached == 0 || gone > EXITED_LINGER * 10 => {
                    if gone > EXITED_LINGER || attached == 0 {
                        self.map.update(|m| m.remove(&s.id));
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
/// or signing a harness in or out ([`crate::accounts::MARK`]), one beside an agent, shown on that
/// agent's page ([`crate::harness::screen::MARK`]), or reading a deployment's log on its host
/// ([`crate::cd::logs::MARK`]): never listed as anyone's terminal.
pub fn hidden(command: Option<&str>) -> bool {
    command.is_some_and(|c| c.contains(crate::updates::HIDDEN_MARK) || c.contains(crate::accounts::MARK) || c.contains(crate::harness::screen::MARK) || c.contains(crate::cd::logs::MARK))
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

/// Hands `out` to a terminal's relay without waiting (goal 3, plan 01a1a59c2a8e step 5): the hub's
/// loop takes every runner's messages, and it waited for room in one terminal's relay, every runner's
/// messages behind it. Output with no room is a gap in `session`, which its relay repairs from the
/// machine's replay of the session (the relay asks for it, and sets aside what comes until it does);
/// an end (`Exit`, `Gone`) with no room goes on its own once there is, after what came before it, as
/// its relay's sender is taken from the hub's map with it and nothing follows it.
pub fn forward(tx: &async_channel::Sender<TermOut>, session: Option<&Session>, out: TermOut) {
    match tx.try_send(out) {
        Ok(()) | Err(async_channel::TrySendError::Closed(_)) => {}
        Err(async_channel::TrySendError::Full(TermOut::Data(_) | TermOut::Replay(_))) => {
            if let Some(s) = session {
                s.lost();
            }
        }
        Err(async_channel::TrySendError::Full(end)) => {
            let tx = tx.clone();
            crate::rt::spawn(async move {
                let _ = tx.send(end).await;
            });
        }
    }
}

/// A relay's repair of the gaps the hub left in its output ([`forward`]): the gaps it has asked a
/// replay for, and whether one is on its way (the output before it set aside: the replay holds it).
#[derive(Debug, Default)]
struct Repair {
    seen: u64,
    waiting: bool,
}

impl Repair {
    /// The session's gaps now: whether one is new, to ask the machine for its replay.
    fn gap(&mut self, gaps: u64) -> bool {
        if gaps <= self.seen {
            return false;
        }
        self.seen = gaps;
        self.waiting = true;
        true
    }
}

/// Asks `machine` for `session`'s replay: its picture of the session now, live output after it.
async fn ask_replay(hub: &Arc<Hub>, machine: &str, session: &str) {
    if !hub.send_to_agent(machine, LANE_TERM, &ServerMsg::TermAttach { session: session.to_owned() }).await {
        tracing::debug!(%machine, session, "a terminal's replay couldn't be asked for: its machine is away");
    }
}

fn run_agent(hub: &Arc<Hub>, machine: String, s: Arc<Session>, input: async_channel::Receiver<Input>, first: Option<ServerMsg>) {
    hub.terminals.insert(&s);
    let h = Arc::clone(hub);
    crate::rt::spawn(async move {
        let key = format!("{machine}/{}", s.id);
        let (out_tx, out_rx) = async_channel::bounded::<TermOut>(4096);
        h.terms.update(|m| m.insert(key.clone(), out_tx.clone()));
        // A terminal to open is a live command (`crate::wire::commands`): refused at once while its
        // machine is away, and answered once it opened it. Unanswered, it goes on: its output says.
        let failed = match first {
            Some(msg) if matches!(crate::wire::server_class(&msg), crate::wire::Class::Command(_)) => {
                match h.command_start(&machine, &msg).await.answer(crate::hub::COMMAND_WITHIN).await {
                    a @ (crate::wire::commands::Answer::Refused { .. } | crate::wire::commands::Answer::Unreachable { .. }) => Some(a.words()),
                    _ => None,
                }
            }
            Some(msg) => (!h.send_to_agent(&machine, LANE_TERM, &msg).await).then(|| format!("machine {} is not connected", h.shown(&machine))),
            None => None,
        };
        if let Some(why) = failed {
            h.terms.update(|m| m.remove(&key));
            s.finish(None, Some(why));
            h.emit_terminals();
            return;
        }
        crate::metrics::global().terminal_sessions.add(1);
        enum Ev {
            Out(Option<TermOut>),
            In(Option<Input>),
            Gap,
            Close,
        }
        let mut repair = Repair::default();
        // Its turns on its shard (goal 3, plan 01a1a59c2a8e step 3): a burst of output, up to a full queue of
        // it through the terminal's emulator, held the shard for as long as it all took; it gives the shard
        // up by its step quantum now.
        let mut turns = crate::rt::Turns::new();
        loop {
            turns.after().await;
            let ev = futures::select_biased! {
                () = s.close.cancelled().fuse() => Ev::Close,
                () = s.gap.notified().fuse() => Ev::Gap,
                o = out_rx.recv().fuse() => Ev::Out(o.ok()),
                i = input.recv().fuse() => Ev::In(i.ok()),
            };
            match ev {
                Ev::Gap => {
                    if repair.gap(s.gaps()) {
                        ask_replay(&h, &machine, &s.id).await;
                    }
                }
                Ev::Out(Some(TermOut::Data(d))) => {
                    if repair.gap(s.gaps()) {
                        ask_replay(&h, &machine, &s.id).await;
                    }
                    if !repair.waiting {
                        s.output(&d);
                    }
                }
                Ev::Out(Some(TermOut::Replay(d))) => {
                    if repair.gap(s.gaps()) {
                        // A gap since this replay came: it's older than what was lost, so another
                        // is asked for.
                        ask_replay(&h, &machine, &s.id).await;
                    } else {
                        repair.waiting = false;
                        // An adopted session starts from the agent's picture of it. One that has a
                        // picture already (its machine's output stream started over, docs/wire.md,
                        // or a gap the hub left) is repainted from the new one.
                        if !s.is_empty() {
                            s.output(b"\x1bc");
                        }
                        s.output(&d);
                    }
                }
                Ev::Out(Some(TermOut::Exit { code, error })) => {
                    s.finish(code, error);
                    break;
                }
                Ev::Out(Some(TermOut::Gone) | None) => {
                    // The machine went away; the shell keeps running there and is adopted again
                    // when it reconnects. Attached browsers reconnect by session id.
                    h.terminals.remove(&s.id);
                    s.out.send(Frame::Exit { code: None, error: Some("__reconnect__".to_owned()) });
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
        h.terms.update(|m| m.remove(&key));
        crate::metrics::global().terminal_sessions.add(-1);
        h.emit_terminals();
    });
    hub.emit_terminals();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_outlives_restarts_and_goes_with_its_session() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let file = dir.path().join("terminal-names.json");
        let reg = Sessions::new();
        reg.keep_names_in(file.clone());
        reg.insert(&Session::detached("t1".into(), "local".into(), None, 80, 24));
        assert!(!reg.rename("nope", "x"), "no such session");
        assert!(reg.rename("t1", "  builds\u{7}  "));
        assert_eq!(reg.list().first().and_then(|i| i.name.clone()).as_deref(), Some("builds"));
        // Kept off the calling thread, soon after (goal 3, plan 01a1a59c2a8e step 5): read once it's written.
        let settled = |done: &dyn Fn(&str) -> bool| {
            let deadline = Instant::now() + Duration::from_secs(20);
            while !std::fs::read_to_string(&file).is_ok_and(|t| done(&t)) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        settled(&|t| t.contains("builds"));
        // Another server reads it back when the session comes back under its id.
        let again = Sessions::new();
        again.keep_names_in(file.clone());
        again.insert(&Session::detached("t1".into(), "local".into(), None, 80, 24));
        assert_eq!(again.list().first().and_then(|i| i.name.clone()).as_deref(), Some("builds"));
        // Emptied, it shows its title again; the session gone, so is its name.
        assert!(again.rename("t1", ""));
        assert_eq!(again.list().first().and_then(|i| i.name.clone()), None);
        assert!(again.rename("t1", "deploys"));
        settled(&|t| t.contains("deploys"));
        again.remove("t1");
        settled(&|t| !t.contains("deploys"));
        let third = Sessions::new();
        third.keep_names_in(file);
        assert!(third.names.load().is_empty());
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

    /// Goal 3, plan 01a1a59c2a8e step 5: a relay asks for one replay a gap, sets the output aside until
    /// it comes, and asks again for a gap after the one it asked for.
    #[test]
    fn g3_s5_a_relay_asks_for_a_replay_once_a_gap() {
        let mut r = Repair::default();
        assert!(!r.gap(0) && !r.waiting, "no gap, nothing asked");
        assert!(r.gap(1) && r.waiting, "a gap: a replay asked for, the output set aside");
        assert!(!r.gap(1), "the same gap: asked once");
        r.waiting = false;
        assert!(r.gap(3) && r.waiting, "gaps since: asked again");
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

    /// Goal 11, plan 01a1a87b2e56 step 3: a slow viewer's frames are kept up to the session's backlog in
    /// bytes (its count of frames too), counted as the terminals' feeds' on `/metrics`; past it the viewer
    /// is told it fell behind, and the repaint it's sent then holds the session's state.
    #[test]
    fn g11_s3_a_slow_viewers_frames_are_kept_up_to_the_backlog_in_bytes() {
        let (s, _input) = Session::new("local".into(), None, 80, 24);
        let mut slow = s.attach();
        let bound = FANOUT * crate::memory::ring::place::<Frame>() + BACKLOG_BYTES;
        assert_eq!(s.out.bound(), bound);
        assert!(crate::memory::owners::TERMINAL_FEEDS.bound().is_some_and(|b| b >= bound));
        let chunk = [b'x'; 32 * 1024];
        for _ in 0..(2 * BACKLOG_BYTES / chunk.len()) {
            feed(&s, &chunk);
            assert!(s.out.held() <= bound, "{} past {bound}", s.out.held());
        }
        assert!(matches!(slow.live.try_recv(), Err(async_broadcast::TryRecvError::Overflowed(n)) if n > 0), "told it fell behind");
        let mut kept = 0;
        while let Ok(Frame::Data(d)) = slow.live.try_recv() {
            kept += d.len();
        }
        assert!(kept <= BACKLOG_BYTES && kept >= BACKLOG_BYTES - chunk.len(), "the newest backlog's worth: {kept}");
        assert!(s.attach().replay.len() >= BACKLOG_BYTES - 8192, "the repaint holds the backlog");
    }

    #[test]
    fn resizes_are_announced_once_and_clamped() {
        let (s, input) = Session::new("local".into(), None, 80, 24);
        let mut a = s.attach();
        s.send(Input::Resize { cols: 120, rows: 40 });
        s.send(Input::Resize { cols: 120, rows: 40 }); // unchanged: no frame, nothing to the PTY
        s.send(Input::Resize { cols: 1, rows: 5000 });
        let mut sizes = Vec::new();
        while let Ok(f) = a.live.try_recv() {
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
    }

    #[test]
    fn titles_exits_and_sweeping() {
        let sessions = Sessions::new();
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
    }
}
