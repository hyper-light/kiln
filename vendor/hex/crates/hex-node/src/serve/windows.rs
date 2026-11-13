//! Windows' event sources and local socket for the node's socket core (native plan step 8; the decision
//! "the Windows node, on one completion port" in docs/design/runtime.md), in safe calls only: the Win32
//! calls are `hex_afunix`'s (CLAUDE.md §12).
//! - the socket is AF_UNIX `SOCK_STREAM` (afunix.sys has no other kind), so each message is framed
//!   ([`hex_core::stream`]) and the core gets them whole, as on macOS;
//! - one completion port carries every source: the listener's one overlapped `AcceptEx` at a time, each
//!   connection's one overlapped receive and one overlapped send, a program's end (a wait completion packet
//!   on its process handle, which the kernel queues to the port itself), and [`Os::wake`]. The core's event set is level-triggered readiness, so
//!   a connection watched for reads is reported while a message read whole waits or its client has gone,
//!   and one watched for writes while no send of it is under way. A program's end, whose completion is its
//!   only report, is kept here until a wait has room for it, and reported before the connections;
//! - a connection is read one buffer at a time, and only while the core reads it and no message read from
//!   it waits, so a client the core has stopped reading holds one buffer and what it decoded here, no more;
//! - a send hands the kernel the whole framed message and owns nothing after, so to the core every send is
//!   whole or nothing; while one is under way, the next is `WouldBlock`;
//! - a connection the core ends is closed in two phases: once its last send has completed it is held,
//!   reading, until its peer closes (a graceful close), and reset if the peer hasn't closed within a learned
//!   wait. It is never given an end-of-stream: afunix.sys now and then fails to wake a peer's read with one
//!   (2026-11-09: ~2% under load), and a read that missed it isn't woken by a later reset either, only by
//!   its own next call; a reset alone wakes every read, and the wait gives the peer the time to read the
//!   last message first (what a reset would discard);
//! - only this user or LocalSystem is served: each peer's pid (`SIO_AF_UNIX_GETPEERPID`) names a process
//!   whose user is read and refused by name otherwise, as Linux's `SO_PEERCRED` is;
//! - an exec's asked streams reach the caller as handles of its own (the stdio decision's pick, dup):
//!   Windows' AF_UNIX passes no handles, so the node gives the caller's process a handle to each end
//!   (`DuplicateHandle` into the process it opened at the connection's admission, `PROCESS_DUP_HANDLE`,
//!   held so its pid can't name another meanwhile) and names their values in the message's envelope
//!   (`hex_afunix::envelope`); an exec that gives a stream (the caller's own pipe) is refused by name, as
//!   the Windows client lends no handle;
//! - a host's own connection (its guest's, [`super::Source`]) is watched by its socket's network events
//!   (`hex_afunix::SourceWatch`: bytes or its end to read, and room to send while its writes wait), queued
//!   to the port by a wait completion packet as a program's end is. Each watch reads afresh what holds, and
//!   one reported is reported at each wait until the core watches it again, as the core's event set is
//!   level-triggered. Its socket never joins the port: it stays its owner's.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io;
use std::os::windows::fs::MetadataExt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use hex_afunix::{self as socket, Done, Port, ProgramWatch, Socket};
use hex_api::{Refusal, Stdio};
use hex_core::stream::{self, Deframer};
use hex_core::wire::{self, Streams};

use super::{Accepted, EVENTS, Event, Interest, Os, Peer, Recv, Sent, Unwatched};

/// Windows' startup admission ceilings (native plan step 8): handles are bounded by its documented
/// 2^24 per-process kernel handle table; each program watch retains a process and wait-packet handle.
/// Threads are bounded by the process's commit room divided by a default worker's measured stack extent.
/// https://learn.microsoft.com/en-us/windows/win32/sysinfo/kernel-objects
/// https://learn.microsoft.com/en-us/windows/win32/procthread/thread-stack-size
/// Allocation failures remain typed refusals, as on Linux and macOS; these are ceilings, not reservations.
pub fn platform() -> Result<super::Platform, Refusal> {
    use hex_runtime::windows::sys;
    let stack = std::thread::Builder::new()
        .name("hex-worker-stack".to_owned())
        .spawn(sys::thread_stack_bytes)
        .map_err(|e| Refusal::os("measuring a worker's stack", e))?
        .join()
        .map_err(|_| Refusal::Broken("the worker stack probe ended unexpectedly".to_owned()))?
        .map_err(|e| Refusal::os("reading a worker's stack extent", e))?;
    let commit =
        sys::commit_available().map_err(|e| Refusal::os("reading the process's commit room", e))?;
    let processes = sys::enclosing_process_limit()
        .map_err(|e| Refusal::os("reading the enclosing job's process limit", e))?;
    let handles = 1usize
        .checked_shl(24)
        .ok_or_else(|| Refusal::Broken("the kernel handle ceiling is past usize".to_owned()))?;
    let threads = usize::try_from(commit)
        .unwrap_or(usize::MAX)
        .checked_div(stack)
        .ok_or_else(|| Refusal::Broken("the worker's stack extent was zero".to_owned()))?;
    let watched = handles
        .checked_div(2)
        .ok_or_else(|| Refusal::Broken("a program watch has no handle cost".to_owned()))?;
    Ok(super::Platform {
        descriptors: handles,
        threads,
        // ProgramWatch owns exactly these two handles for each watched program.
        programs: processes.unwrap_or(watched).min(watched),
    })
}

/// A frame's prefix, its body's length (`hex_core::stream`'s `u32`).
const PREFIX: usize = size_of::<u32>();
/// The longest frame the node sends its caller: the longest message and the longest envelope of its ends.
const FRAME_BOUND: usize = wire::MAX_MESSAGE + socket::ENVELOPE_MAX;
/// Where a [`Stream`]'s token starts before the core watches it: each its own, from a range the core's
/// tokens (counted up from its first) never reach, so a connection the core never watched (one refused as
/// it came) still has a tag of its own on the port while it is ended.
const UNWATCHED_FIRST: u64 = 1 << 63;
/// The next unwatched token.
static NEXT_UNWATCHED: AtomicU64 = AtomicU64::new(UNWATCHED_FIRST);
/// The slowest a peer that reads to its end closed after the node's last message, on its record
/// (stdio_probe peerclose at dac212ce, bench/native/records/peerclose-jupiter-dac212ce-20261109T1834Z.txt:
/// 2,883 us beside a workspace build, of 2,990 rounds): the seed of the node's wait before it resets an
/// ended connection.
const PEER_CLOSE_RECORD: Duration = Duration::from_micros(2_883);
/// The listener's accepts' tag on the port.
const LISTENER: u64 = 0;
/// `FILE_ATTRIBUTE_REPARSE_POINT`: afunix.sys's socket file is one (`IO_REPARSE_TAG_AF_UNIX`).
const REPARSE_POINT: u32 = 0x400;

/// What a request carries besides its bytes: nothing on Windows, whose AF_UNIX passes no handles.
pub enum NoHandle {}

/// One connection: its socket, its token once watched, and the framing between its bytes and the core's
/// messages.
pub struct Stream {
    socket: Socket,
    token: AtomicU64,
    state: Mutex<State>,
    /// Its caller's process, opened at its admission to give it the ends of the streams its execs ask for.
    peer: OnceLock<socket::Peer>,
}

struct State {
    deframer: Deframer,
    /// Whole messages read and not yet taken by the core.
    ready: VecDeque<Message>,
    /// The read buffer, when no receive holds it.
    buf: Option<Vec<u8>>,
    /// A receive is under way.
    receiving: bool,
    /// A send is under way.
    sending: bool,
    /// The core watches it for its requests.
    reading: bool,
    /// Its client ended it, or it failed: nothing more is read.
    ended: bool,
    /// Its client reset it: reported once, as [`Recv::Reset`].
    reset: bool,
    /// The stream can't be read further (an oversized or bad frame): the next read is its end.
    broken: bool,
    /// The core has ended it: its two-phase close is under way ([`Windows::hold`]).
    ending: bool,
}

struct Message {
    bytes: Vec<u8>,
    /// Its declared size, when it was past the bound and not kept.
    size: usize,
}

fn poisoned<T>(_: std::sync::PoisonError<T>) -> Refusal {
    Refusal::Broken("a socket lock was poisoned".to_owned())
}

/// Closes in the caller's process the handles given it for a message that didn't go.
fn take_back(peer: Option<&socket::Peer>, given: &[u64]) {
    if let Some(peer) = peer {
        for value in given {
            peer.take_back(*value);
        }
    }
}

impl Stream {
    fn new(socket: Socket) -> Stream {
        Stream {
            socket,
            token: AtomicU64::new(NEXT_UNWATCHED.fetch_add(1, Ordering::Relaxed)),
            peer: OnceLock::new(),
            state: Mutex::new(State {
                deframer: Deframer::new(wire::MAX_MESSAGE),
                ready: VecDeque::new(),
                buf: Some(vec![0u8; stream::MAX_FRAME]),
                receiving: false,
                sending: false,
                reading: false,
                ended: false,
                reset: false,
                broken: false,
                ending: false,
            }),
        }
    }

    /// Starts the next receive if the core reads it, nothing read waits, and none is under way.
    fn read_on(&self, s: &mut State) {
        if !s.reading || s.receiving || s.ended || s.broken || !s.ready.is_empty() {
            return;
        }
        let Some(buf) = s.buf.take() else {
            return;
        };
        match socket::receive(&self.socket, self.token.load(Ordering::SeqCst), buf) {
            Ok(()) => s.receiving = true,
            Err((e, buf)) => {
                s.buf = Some(buf);
                s.reset = socket::is_reset(&e);
                s.ended = true;
            }
        }
    }
}

/// The connections the core watches, by token, and what for.
type Conns = BTreeMap<u64, (Arc<Stream>, Interest)>;

/// The connection a completion is for, and what the core watches it for (`None`: the core has let it go,
/// and this side holds it through its close).
type Completed = Option<(Arc<Stream>, Option<Interest>)>;

/// The listener's state: whether the core accepts, an accept under way, and what one took.
#[derive(Default)]
struct Listening {
    on: bool,
    accepting: bool,
    taken: VecDeque<io::Result<Socket>>,
}

/// A host's own connection the core watches ([`Os::watch_source`]): its watch on the port, whether it is
/// watched for room too, and whether its watch has reported since the core last watched it, so it is
/// reported at each wait until then.
struct Sourced {
    watch: socket::SourceWatch,
    write: bool,
    fired: bool,
}

/// A connection the core has ended: held until its peer closes, or reset once the wait for that passes.
struct Ending {
    conn: Arc<Stream>,
    /// When its last send completed and the wait for its peer's close began; `None` while that send is
    /// still under way.
    since: Option<Instant>,
}

/// The listener and the completion port that watches it, the connections and the programs.
pub struct Windows {
    listener: Socket,
    port: Port,
    listening: Mutex<Listening>,
    /// The connections the core watches, by token, and what for.
    watching: Mutex<Conns>,
    /// The connections the core has ended, by tag, held through their two-phase close.
    ending: Mutex<BTreeMap<u64, Ending>>,
    /// The slowest close a peer made after its end-of-stream went, in nanoseconds: it only rises.
    slowest_close: AtomicU64,
    /// The host's own connections the core watches, by token ([`super::Source`]).
    sources: Mutex<BTreeMap<u64, Sourced>>,
    /// Watched programs whose end was taken from the port and not yet reported: an end's completion is its
    /// only report, so one a wait had no room for is kept for the next (CLAUDE.md §3).
    ends: Mutex<BTreeSet<u64>>,
}

impl Windows {
    /// A socket at `path`, and its completion port. A socket left at `path` by an agent that has ended is
    /// replaced; one an agent still serves, or a file that isn't a socket, is refused.
    pub fn bind(path: &Path) -> Result<Self, Refusal> {
        clear(path)?;
        let listener = Socket::listen(path).map_err(|e| Refusal::Os {
            op: "binding the node agent's socket",
            why: format!("{}: {e}", path.display()),
        })?;
        let port =
            Port::new().map_err(|e| Refusal::os("making the socket's completion port", e))?;
        port.add(&listener)
            .map_err(|e| Refusal::os("watching the node agent's socket", e))?;
        Ok(Self {
            listener,
            port,
            listening: Mutex::new(Listening::default()),
            watching: Mutex::new(BTreeMap::new()),
            ending: Mutex::new(BTreeMap::new()),
            slowest_close: AtomicU64::new(0),
            sources: Mutex::new(BTreeMap::new()),
            ends: Mutex::new(BTreeSet::new()),
        })
    }

    fn watching(&self) -> Result<MutexGuard<'_, Conns>, Refusal> {
        self.watching.lock().map_err(poisoned)
    }

    fn listening(&self) -> Result<MutexGuard<'_, Listening>, Refusal> {
        self.listening.lock().map_err(poisoned)
    }

    fn ending(&self) -> Result<MutexGuard<'_, BTreeMap<u64, Ending>>, Refusal> {
        self.ending.lock().map_err(poisoned)
    }

    fn sources(&self) -> Result<MutexGuard<'_, BTreeMap<u64, Sourced>>, Refusal> {
        self.sources.lock().map_err(poisoned)
    }

    /// Reports the kept program ends `events` has room for; the rest wait for the next wait.
    fn report_ends(&self, events: &mut Vec<Event>) -> Result<(), Refusal> {
        let mut ends = self.ends.lock().map_err(poisoned)?;
        while events.len() < EVENTS {
            let Some(token) = ends.pop_first() else {
                break;
            };
            push(events, token, true, false, false);
        }
        Ok(())
    }

    /// How long an ended connection's peer has to close after its last message before the node resets it:
    /// twice the slowest of its record and what this node has seen (Hex's rule for learned waits), so it only
    /// rises.
    fn close_wait(&self) -> Duration {
        let seen = Duration::from_nanos(self.slowest_close.load(Ordering::Relaxed));
        PEER_CLOSE_RECORD.max(seen).saturating_mul(2)
    }

    /// The second phase of an ended connection's close, once its last send has completed: the time noted,
    /// and a receive kept under way for its peer's close. No end-of-stream is sent (see the module's note).
    /// Done once: a second call finds the time noted.
    fn hold(&self, tag: u64, conn: &Arc<Stream>) -> Result<(), Refusal> {
        {
            let mut ending = self.ending()?;
            match ending.get_mut(&tag) {
                Some(e) if e.since.is_none() => e.since = Some(Instant::now()),
                _ => return Ok(()),
            }
        }
        let mut st = conn.state.lock().map_err(poisoned)?;
        if st.ended {
            drop(st);
            self.closed(tag);
            return Ok(());
        }
        if !st.receiving
            && let Some(buf) = st.buf.take()
        {
            match socket::receive(&conn.socket, tag, buf) {
                Ok(()) => st.receiving = true,
                Err((_, buf)) => {
                    st.buf = Some(buf);
                    st.ended = true;
                    drop(st);
                    self.closed(tag);
                }
            }
        }
        Ok(())
    }

    /// An ended connection's peer has closed (or its read failed): it is let go, so its close is a graceful
    /// one, and how long the peer took after the last message teaches the wait.
    fn closed(&self, tag: u64) {
        let held = self.ending.lock().ok().and_then(|mut e| e.remove(&tag));
        if let Some(Ending {
            since: Some(since), ..
        }) = held
        {
            let took = u64::try_from(since.elapsed().as_nanos()).unwrap_or(u64::MAX);
            self.slowest_close.fetch_max(took, Ordering::Relaxed);
        }
    }

    /// Resets each ended connection whose peer hasn't closed within the wait (a peer that reads to the end it
    /// is given has its last message, and waits): its close becomes a reset, which ends the peer's read; a
    /// core still watching it sees it hung up. How long until the next held one is due, if any.
    fn reset_overdue(&self) -> Result<Option<Duration>, Refusal> {
        let wait = self.close_wait();
        let now = Instant::now();
        let mut next: Option<Duration> = None;
        let mut overdue = Vec::new();
        self.ending()?.retain(|_, e| {
            let Some(since) = e.since else {
                return true;
            };
            let left = wait.saturating_sub(now.saturating_duration_since(since));
            if left.is_zero() {
                overdue.push(Arc::clone(&e.conn));
                false
            } else {
                next = Some(next.map_or(left, |n| n.min(left)));
                true
            }
        });
        for conn in overdue {
            let _ = conn.socket.reset_on_close();
            if let Ok(mut st) = conn.state.lock() {
                st.ended = true;
            }
        }
        Ok(next)
    }

    /// The connection a completion under `tag` is for: one the core watches (and what for), or one it has
    /// ended and this side holds through its close.
    fn completed(&self, tag: u64) -> Result<Completed, Refusal> {
        if let Some((conn, interest)) = self.watched(tag)? {
            return Ok(Some((conn, Some(interest))));
        }
        Ok(self
            .ending()?
            .get(&tag)
            .map(|e| (Arc::clone(&e.conn), None)))
    }

    /// Starts an accept if the core accepts and none is under way or waiting to be taken.
    fn accept_on(&self, l: &mut Listening) {
        if !l.on || l.accepting || !l.taken.is_empty() {
            return;
        }
        match socket::accept(&self.port, &self.listener, LISTENER) {
            Ok(()) => l.accepting = true,
            // Reported to the core by its next accept, which pauses on a spent one.
            Err(e) => l.taken.push_back(Err(e)),
        }
    }

    /// Puts in `events` what is ready without a wait: first what only this side holds (connections taken,
    /// kept program ends, sources reported), so the connections, which stay ready on their own until read,
    /// can't crowd it out. Whether there was any.
    fn ready_now(&self, conns: &Conns, events: &mut Vec<Event>) -> Result<bool, Refusal> {
        let before = events.len();
        {
            let l = self.listening()?;
            if l.on && !l.taken.is_empty() {
                events.push(Event::Listener);
            }
        }
        self.report_ends(events)?;
        for (token, s) in self.sources()?.iter() {
            if events.len() >= EVENTS {
                break;
            }
            if s.fired {
                push(events, *token, true, s.write, false);
            }
        }
        for (token, (s, interest)) in conns {
            if events.len() >= EVENTS {
                break;
            }
            let st = s.state.lock().map_err(poisoned)?;
            let (readable, writable) = match interest {
                Interest::Read => (!st.ready.is_empty() || st.ended || st.broken, false),
                Interest::Write => (false, !st.sending || st.ended),
            };
            if readable || writable {
                push(events, *token, readable, writable, st.ended);
            }
        }
        Ok(events.len() > before)
    }

    /// The connection watched under `tag` and what for, as the core watches it now: a worker may have
    /// changed it while the wait was under way.
    fn watched(&self, tag: u64) -> Result<Option<(Arc<Stream>, Interest)>, Refusal> {
        Ok(self.watching()?.get(&tag).map(|(s, i)| (Arc::clone(s), *i)))
    }

    /// Takes one completion into the core's events.
    fn take(&self, done: Done, events: &mut Vec<Event>) -> Result<(), Refusal> {
        match done {
            Done::Wake => {
                if !events.contains(&Event::Wake) {
                    events.push(Event::Wake);
                }
            }
            // Kept until a wait has room for it ([`Windows::report_ends`]): this completion is its only report.
            Done::Program(token) => {
                self.ends.lock().map_err(poisoned)?.insert(token);
            }
            Done::Source(token) => {
                // Kept until the core watches it again, so a report the events had no room for comes at the
                // next wait.
                if let Some(s) = self.sources()?.get_mut(&token) {
                    s.fired = true;
                    push(events, token, true, s.write, false);
                }
            }
            Done::Accepted { conn, .. } => {
                let mut l = self.listening()?;
                l.accepting = false;
                l.taken.push_back(conn);
                if l.on && !events.contains(&Event::Listener) {
                    events.push(Event::Listener);
                }
            }
            Done::Received { tag, buf, n } => {
                let Some((conn, interest)) = self.completed(tag)? else {
                    return Ok(());
                };
                let mut st = conn.state.lock().map_err(poisoned)?;
                st.receiving = false;
                if st.ending {
                    // An ended connection reads only for its peer's close; what else comes is let go.
                    st.buf = Some(buf);
                    if matches!(n, Ok(k) if k > 0)
                        && let Some(again) = st.buf.take()
                    {
                        match socket::receive(&conn.socket, tag, again) {
                            Ok(()) => {
                                st.receiving = true;
                                return Ok(());
                            }
                            Err((_, again)) => st.buf = Some(again),
                        }
                    }
                    st.ended = true;
                    drop(st);
                    self.closed(tag);
                    if matches!(interest, Some(Interest::Read)) {
                        push(events, tag, true, false, true);
                    }
                    return Ok(());
                }
                let Some(interest) = interest else {
                    return Ok(());
                };
                match n {
                    Ok(0) => st.ended = true,
                    Ok(n) => frame(&mut st, buf.get(..n).unwrap_or_default()),
                    Err(e) => {
                        st.reset = socket::is_reset(&e);
                        st.ended = true;
                    }
                }
                st.buf = Some(buf);
                conn.read_on(&mut st);
                if matches!(interest, Interest::Read)
                    && (!st.ready.is_empty() || st.ended || st.broken)
                {
                    push(events, tag, true, false, st.ended);
                }
            }
            Done::Sent { tag, buf, n } => {
                let Some((conn, interest)) = self.completed(tag)? else {
                    return Ok(());
                };
                let mut st = conn.state.lock().map_err(poisoned)?;
                st.sending = false;
                match n {
                    Ok(n) if n >= buf.len() => {}
                    Ok(n) => {
                        // Taken in part: the rest goes now, and the core's next send waits for it.
                        let rest = buf.get(n..).unwrap_or_default().to_vec();
                        match socket::send_all(&conn.socket, tag, rest) {
                            Ok(()) => st.sending = true,
                            Err(_) => st.ended = true,
                        }
                    }
                    Err(_) => st.ended = true,
                }
                if matches!(interest, Some(Interest::Write)) && (!st.sending || st.ended) {
                    push(events, tag, false, true, st.ended);
                }
                // An ended connection's last send is out: the second phase of its close.
                let end_now = st.ending && !st.sending;
                drop(st);
                if end_now {
                    self.hold(tag, &conn)?;
                }
            }
        }
        Ok(())
    }
}

/// Adds to `token`'s event in `events`, or appends one.
fn push(events: &mut Vec<Event>, token: u64, readable: bool, writable: bool, hung_up: bool) {
    let room = events.len() < EVENTS;
    match events
        .iter_mut()
        .find(|ev| matches!(ev, Event::Ready { token: t, .. } if *t == token))
    {
        Some(Event::Ready {
            readable: r,
            writable: w,
            hung_up: h,
            ..
        }) => {
            *r |= readable;
            *w |= writable;
            *h |= hung_up;
        }
        _ if room => events.push(Event::Ready {
            token,
            readable,
            writable,
            hung_up,
        }),
        _ => {}
    }
}

/// Frames what a receive read into whole messages.
fn frame(st: &mut State, mut input: &[u8]) {
    while !input.is_empty() {
        let at_start = !st.deframer.in_frame();
        match st.deframer.take(input) {
            Ok((took, done)) => {
                input = input.get(took..).unwrap_or_default();
                if let Some(message) = done {
                    st.ready.push_back(Message {
                        size: message.len(),
                        bytes: message,
                    });
                }
            }
            Err(wire::Fault::Oversized { bytes, .. }) => {
                // Its size is said, with its header where this read holds it, so the core refuses it by
                // name to the request it is; the stream can't be read past it.
                let header = if at_start {
                    input
                        .get(PREFIX..)
                        .unwrap_or_default()
                        .iter()
                        .take(wire::HEADER_BYTES)
                        .copied()
                        .collect()
                } else {
                    Vec::new()
                };
                st.ready.push_back(Message {
                    bytes: header,
                    size: bytes,
                });
                st.broken = true;
                return;
            }
            Err(_) => {
                st.broken = true;
                return;
            }
        }
    }
}

/// Clears `path` for a new socket: nothing there, or a socket no agent serves (removed). A socket an agent
/// still serves, or a file that isn't a socket, is refused.
fn clear(path: &Path) -> Result<(), Refusal> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => {
            return Err(Refusal::Os {
                op: "checking the node agent's socket path",
                why: format!("{}: {e}", path.display()),
            });
        }
    };
    if meta.file_attributes() & REPARSE_POINT == 0 {
        return Err(Refusal::Invalid {
            what: "socket path",
            why: format!("{}: a file that isn't a socket is there", path.display()),
        });
    }
    if Socket::connect(path).is_ok() {
        return Err(Refusal::Invalid {
            what: "socket path",
            why: format!("{}: a node agent serves it already", path.display()),
        });
    }
    std::fs::remove_file(path).map_err(|e| Refusal::Os {
        op: "removing a socket no agent serves",
        why: format!("{}: {e}", path.display()),
    })
}

/// A watched program: the watch on its process handle, which its end is seen through and it is killed
/// through.
pub struct Watch {
    _program: ProgramWatch,
}

impl Os for Windows {
    type Conn = Arc<Stream>;
    type Watch = Watch;
    type Passed = NoHandle;

    fn wait(&self, events: &mut Vec<Event>, timeout: Option<Duration>) -> Result<(), Refusal> {
        // Ended connections whose peer hasn't closed in time are reset, and the wait ends no later than the
        // next is due.
        let timeout = match (timeout, self.reset_overdue()?) {
            (Some(t), Some(due)) => Some(t.min(due)),
            (None, due) => due,
            (t, None) => t,
        };
        let conns: Conns = self
            .watching()?
            .iter()
            .map(|(t, (s, i))| (*t, (Arc::clone(s), *i)))
            .collect();
        let now = self.ready_now(&conns, events)?;
        let mut done = Vec::new();
        let room = EVENTS.saturating_sub(events.len()).max(1);
        self.port
            .wait(
                &mut done,
                if now { Some(Duration::ZERO) } else { timeout },
                room,
            )
            .map_err(|e| Refusal::os("waiting on the socket's completion port", e))?;
        for d in done {
            self.take(d, events)?;
        }
        self.report_ends(events)
    }

    fn wake(&self) -> Result<(), Refusal> {
        self.port
            .wake()
            .map_err(|e| Refusal::os("waking the socket's thread", e))
    }

    fn accept(&self) -> Accepted<Arc<Stream>> {
        let Ok(mut l) = self.listening.lock() else {
            return Accepted::Failed(Refusal::Broken("a socket lock was poisoned".to_owned()));
        };
        let taken = l.taken.pop_front();
        self.accept_on(&mut l);
        match taken {
            None => Accepted::None,
            // On the port already: an accept's socket joins it before the accept starts.
            Some(Ok(s)) => Accepted::Conn(Arc::new(Stream::new(s))),
            Some(Err(e)) if socket::is_spent(&e) => Accepted::Spent,
            Some(Err(e)) => {
                Accepted::Failed(Refusal::os("accepting on the node agent's socket", e))
            }
        }
    }

    fn accepting(&self, on: bool) -> Result<(), Refusal> {
        let mut l = self.listening()?;
        l.on = on;
        self.accept_on(&mut l);
        Ok(())
    }

    fn stop_listening(&self) {
        if let Ok(mut l) = self.listening.lock() {
            l.on = false;
        }
    }

    fn peer(&self, conn: &Arc<Stream>) -> Peer {
        let Ok(pid) = conn.socket.peer_pid() else {
            return Peer::Gone;
        };
        match socket::same_user_or_system(pid) {
            Ok(true) => {
                // Held for the connection, to give its execs' asked streams; a caller this process may not
                // open so is still served, and an exec's ends to it end the connection instead.
                if let Ok(peer) = socket::Peer::open(pid) {
                    let _ = conn.peer.set(peer);
                }
                Peer::Allowed
            }
            Ok(false) => Peer::Refused(format!(
                "pid {pid} runs as another user than the node agent's"
            )),
            Err(_) => Peer::Gone,
        }
    }

    fn watch(&self, conn: &Arc<Stream>, token: u64, interest: Interest) -> Result<(), Refusal> {
        conn.token.store(token, Ordering::SeqCst);
        self.watching()?.insert(token, (Arc::clone(conn), interest));
        let mut st = conn.state.lock().map_err(poisoned)?;
        st.reading = matches!(interest, Interest::Read);
        conn.read_on(&mut st);
        let ready = match interest {
            Interest::Read => !st.ready.is_empty() || st.ended || st.broken,
            Interest::Write => !st.sending || st.ended,
        };
        drop(st);
        // What is ready already (a message read, room to send) brings no completion of its own, and a
        // worker may change a watch while the I/O thread waits: a wake makes that wait look again.
        if ready { self.wake() } else { Ok(()) }
    }

    fn rewatch(&self, conn: &Arc<Stream>, token: u64, interest: Interest) -> Result<(), Refusal> {
        self.watch(conn, token, interest)
    }

    fn unwatch(&self, conn: &Arc<Stream>) {
        if let Ok(mut w) = self.watching.lock() {
            w.retain(|_, (s, _)| !Arc::ptr_eq(s, conn));
        }
        if let Ok(mut st) = conn.state.lock() {
            st.reading = false;
        }
    }

    fn recv(&self, conn: &Arc<Stream>, buf: &mut [u8]) -> Recv<NoHandle> {
        let Ok(mut st) = conn.state.lock() else {
            return Recv::Gone;
        };
        if let Some(m) = st.ready.pop_front() {
            let n = m.bytes.len().min(buf.len());
            if let (Some(into), Some(from)) = (buf.get_mut(..n), m.bytes.get(..n)) {
                into.copy_from_slice(from);
            }
            conn.read_on(&mut st);
            return Recv::Message {
                size: m.size,
                got: n,
                passed: Vec::new(),
                cut: false,
            };
        }
        if std::mem::take(&mut st.reset) {
            return Recv::Reset;
        }
        if st.ended || st.broken {
            return Recv::Gone;
        }
        conn.read_on(&mut st);
        Recv::WouldBlock
    }

    /// Sends `bytes` whole, the ends in `fds` with it (an exec's asked streams, `Answer::Given`): each is
    /// given to the caller's process as a handle of its own, and its value named in the message's envelope
    /// ([`socket::envelope`]), so the caller reads it with the message. A message whose ends can't be given
    /// (a caller not opened at its admission, or a give refused) is never sent without them: what was given
    /// is taken back and the connection ended, which its client sees.
    fn send(&self, conn: &Arc<Stream>, bytes: &[u8], fds: &[hex_api::End]) -> Sent {
        let Ok(mut st) = conn.state.lock() else {
            return Sent::Gone;
        };
        if st.ended || st.ending {
            return Sent::Gone;
        }
        if st.sending {
            return Sent::WouldBlock;
        }
        let mut given = Vec::with_capacity(fds.len());
        let peer = conn.peer.get();
        for end in fds {
            match end
                .file_handle()
                .and_then(|handle| peer.map(|p| p.give(handle)))
            {
                Some(Ok(value)) => given.push(value),
                _ => {
                    take_back(peer, &given);
                    drop(st);
                    self.shutdown(conn);
                    return Sent::Gone;
                }
            }
        }
        let framed = socket::envelope(&given, bytes)
            .ok()
            .and_then(|enveloped| stream::frame(&enveloped, FRAME_BOUND).ok());
        let Some(framed) = framed else {
            take_back(peer, &given);
            return Sent::Gone;
        };
        match socket::send_all(&conn.socket, conn.token.load(Ordering::SeqCst), framed) {
            Ok(()) => {
                st.sending = true;
                Sent::Sent
            }
            Err(_) => {
                take_back(peer, &given);
                Sent::Gone
            }
        }
    }

    /// The core ends `conn`: the first phase of its two-phase close. It is held first (so a send completing
    /// on the I/O thread finds it), then marked ending; once its last send has completed it waits for its
    /// peer's close ([`Windows::hold`]), and is reset once the wait passes ([`Windows::reset_overdue`]).
    fn shutdown(&self, conn: &Arc<Stream>) {
        let tag = conn.token.load(Ordering::SeqCst);
        if let Ok(mut ending) = self.ending.lock() {
            ending.entry(tag).or_insert_with(|| Ending {
                conn: Arc::clone(conn),
                since: None,
            });
        }
        let sending = {
            let Ok(mut st) = conn.state.lock() else {
                return;
            };
            if st.ending {
                return;
            }
            st.ending = true;
            st.reading = false;
            st.sending
        };
        if !sending {
            let _ = self.hold(tag, conn);
        }
        // The I/O thread's wait takes the new deadline.
        let _ = self.wake();
    }

    fn hung_up(&self, conn: &Arc<Stream>) -> bool {
        conn.state.lock().map_or(true, |st| st.ended)
    }

    fn watch_program(&self, pid: u32, token: u64) -> Result<Watch, Unwatched> {
        socket::watch_program(&self.port, pid, token)
            .map(|program| Watch { _program: program })
            .map_err(|e| {
                if e.kind() == io::ErrorKind::NotFound {
                    return Unwatched::Gone;
                }
                Unwatched::Runs(Refusal::Os {
                    op: "watching a started program",
                    why: format!(
                        "{e}: it runs on unwatched by the socket, and the node's sweep reaps it"
                    ),
                })
            })
    }

    fn unwatch_program(&self, watch: Watch) {
        drop(watch);
    }

    fn stdio(&self, streams: Streams, passed: Vec<NoHandle>) -> Result<Stdio, Refusal> {
        if let Some(never) = passed.into_iter().next() {
            match never {}
        }
        if streams.count() > 0 {
            return Err(Refusal::Platform {
                what: "an exec's stdio through the node agent's socket",
                platform: std::env::consts::OS,
                why: "Windows' AF_UNIX passes no handles, so a stream the caller gives (its own pipe) \
                      can't reach the node: a Windows caller asks the node for an exec's streams, and \
                      is given handles of its own to them (native plan step 8)"
                    .to_owned(),
            });
        }
        Ok(Stdio::null())
    }

    /// Watches a host's own connection by its socket's network events ([`socket::SourceWatch`]), or watches
    /// it again: reported while it has bytes to read or has ended, and room with `write`, until the core
    /// watches it again; each watch reads afresh what holds.
    fn watch_source(
        &self,
        handle: super::Handle<'_>,
        token: u64,
        write: bool,
    ) -> Result<(), Refusal> {
        let watching = |e: io::Error| Refusal::os("watching the node's guest connection", e);
        let mut sources = self.sources()?;
        let (s, fresh) = match sources.entry(token) {
            Entry::Occupied(o) => (o.into_mut(), false),
            Entry::Vacant(v) => {
                let watch = socket::SourceWatch::new(token).map_err(watching)?;
                let s = v.insert(Sourced {
                    watch,
                    write,
                    fired: false,
                });
                (s, true)
            }
        };
        s.write = write;
        s.fired = false;
        let armed = s.watch.arm(&self.port, handle, write).map_err(watching);
        if armed.is_err() && fresh {
            sources.remove(&token);
        }
        armed
    }

    /// Stops watching a host's own connection: its socket is its owner's again, and no report of it comes.
    fn unwatch_source(&self, handle: super::Handle<'_>, token: u64) {
        let taken = self.sources.lock().ok().and_then(|mut s| s.remove(&token));
        if let Some(s) = taken {
            s.watch.stop(handle);
        }
    }
}
