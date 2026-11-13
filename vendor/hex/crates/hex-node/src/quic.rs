//! The node's listener over QUIC (docs/design/hex-quic.md Q4): the node agent's socket server ([`crate::serve`]) serves
//! the connections peers open to its ports over hyper-transport beside its local socket's, through one event set and
//! one set of bounds, workers and disconnect rule. [`Quic`] wraps this OS's [`Os`]:
//!
//! - **Control:** a peer's connection to the control port is one of the server's connections. As the guest's relay
//!   does (`crate::relay` on Linux, docs/design/shards-guest.md §3), it is sent its token first, and its requests come
//!   framed by length ([`hex_core::stream`]), each handed to the server whole; its answers go framed.
//! - **Stdio:** an exec's streams come on connections of their own to the stdio port, named by their first bytes. Each
//!   becomes a pipe the program holds one end of, its other end copied to or from the connection by a thread of its
//!   own; an exec whose streams haven't all come is parked, and its control read on, so the set waits on facts alone
//!   ([`hex_core::stream::Gather`]).
//! - **Staging** (Linux): the relay's staging server takes each connection to the files port.
//! - **Log:** each connection to the log port is sent the node's log lines ([`Log`]).
//!
//! Each resource has a bound, refused past by type (CLAUDE.md §2, §3), each the node's own ([`Bounds::of`]).

use std::collections::{BTreeMap, VecDeque};
use std::io::{PipeReader, PipeWriter, Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use hex_api::{Refusal, Stdio};
use hex_core::stream::{self, Deframer, Exec, Gather, Named};
use hex_core::wire::{self, Call, Request, Streams};
use hex_quic::{Acceptor, Endpoint, PeerId, Shape, Why};

use crate::serve::{
    Accepted, EVENTS, Event, Handle, Interest, Limits, Os, Peer, Recv, Sent, Unwatched,
};

/// The ports a node's endpoint serves: its control, its programs' stdio, its log, and its staging where it stages
/// (`staging`: a Linux node given a folder to stage into, [`Quic::new`]). A port it doesn't serve is refused by type
/// ([`Why::NoPort`]).
pub fn ports(staging: bool) -> Vec<u32> {
    let mut ports = vec![
        stream::CONTROL_PORT,
        stream::STDIO_PORT,
        hex_core::lanes::LOG_PORT,
    ];
    if staging {
        ports.push(stream::files::FILES_PORT);
    }
    ports
}

/// A node endpoint's shape at `limits` (docs/design/hex-quic.md Q8), each number the node's own: as many peers as its
/// connections bound, and for one peer every connection to its ports that can do work at once
/// ([`hex_quic::limits::peer_lanes`] of its calls in flight, `serve::limits`: a stdio connection is named and copied by
/// a thread of its own, [`streams`], so one past its threads has none, whatever its programs), the derivation its
/// clients hold themselves to. hyper-transport grows to these and lays none out ahead
/// (vendor/patches/hyper-raft/0003); hyper-quic lays out a table of the lanes a connected peer may open (quinn's
/// `StreamsState::new`), a row each, on the driver's thread as the peer's first datagram comes, so a peer costs that
/// table while it is connected and no peer is answered while it is laid out.
pub fn shape(limits: &Limits, staging: bool) -> Shape {
    Shape {
        connections: limits.connections,
        lanes: hex_quic::limits::peer_lanes(limits.in_flight, staging),
    }
}

/// The listener's bounds, each the node's own: the relay's rules over the node's socket bounds (`crate::relay`'s
/// `Limits::of`), so a peer within the node's budgets meets none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bounds {
    /// Control connections accepted and not yet taken by the server, staging connections at once, and log
    /// connections: the node's connections.
    pub connections: usize,
    /// Execs and early streams waiting for each other: the node's calls in flight together.
    pub sets: usize,
    /// Stdio connections yet to name themselves: every stream of every set at once.
    pub unnamed: usize,
}

impl Bounds {
    pub fn of(limits: &Limits) -> Self {
        Self {
            connections: limits.connections,
            sets: limits.in_flight,
            unnamed: limits.in_flight.saturating_mul(wire::MAX_FDS),
        }
    }
}

/// A connection the server serves: this OS's, or a peer's over QUIC.
pub enum Conn<C> {
    Native(C),
    Quic(Arc<Control>),
}

/// What came with a request: this OS's descriptor or handle, or a stdio pipe's end over QUIC.
pub enum Passed<P> {
    Native(P),
    Pipe(PipeEnd),
}

/// The program's end of one of its stdio pipes.
pub enum PipeEnd {
    /// Its stdin: the end it reads.
    Stdin(PipeReader),
    /// Its stdout or stderr: the end it writes.
    Out(PipeWriter),
}

/// A peer's control connection, as the server holds it.
pub struct Control {
    conn: hex_quic::Conn,
    token: u64,
    peer: PeerId,
    reading: Mutex<Reading>,
    /// Whole messages for the server, each with the pipes its exec's streams became.
    ready: Mutex<VecDeque<Ready>>,
    /// The server shut it.
    shut: AtomicBool,
    /// The server's token and interest while it watches it.
    watch: Mutex<Option<(u64, Interest)>>,
    shared: Weak<Shared>,
}

struct Reading {
    deframer: Deframer,
    buf: Vec<u8>,
}

struct Ready {
    message: Vec<u8>,
    pipes: Vec<PipeEnd>,
}

/// Execs and their streams waiting for each other, and the execs parked until their streams have all come, under one
/// lock, so a stream that completes a set always finds its parked exec.
struct Sets {
    gather: Gather<PipeEnd>,
    parked: BTreeMap<Exec, (Weak<Control>, Vec<u8>)>,
}

struct Shared {
    bounds: Bounds,
    sets: Mutex<Sets>,
    /// Open controls by token: what a stdio connection names.
    controls: Mutex<BTreeMap<u64, Weak<Control>>>,
    /// Accepted controls the server hasn't taken yet.
    pending: Mutex<VecDeque<Arc<Control>>>,
    accepting: AtomicBool,
    tokens: AtomicU64,
    unnamed: AtomicUsize,
    /// Staging connections being served: only Linux nodes serve the files port.
    #[cfg(target_os = "linux")]
    stagings: AtomicUsize,
    /// Wakes the server's wait.
    wake: hex_quic::Notify,
}

fn poisoned<T>(_: std::sync::PoisonError<T>) -> Refusal {
    Refusal::Broken("a lock of the node's QUIC listener was poisoned".to_owned())
}

impl Drop for Control {
    fn drop(&mut self) {
        if let Some(shared) = self.shared.upgrade() {
            if let Ok(mut controls) = shared.controls.lock() {
                controls.remove(&self.token);
            }
            if let Ok(mut sets) = shared.sets.lock() {
                // Its sets' pipes close, and their copiers end with them.
                drop(sets.gather.forget(self.token));
                sets.parked.retain(|exec, _| exec.conn != self.token);
            }
        }
    }
}

/// This OS's event set and socket, and a node endpoint's connections beside them.
pub struct Quic<N: Os> {
    native: Arc<N>,
    shared: Arc<Shared>,
    watched: Mutex<BTreeMap<u64, Arc<Control>>>,
    log: Arc<Log>,
    /// The endpoint, ended with the server: its acceptors then end too.
    _endpoint: Arc<Endpoint>,
}

impl<N: Os> Quic<N> {
    /// Serves `endpoint`'s connections beside `native`'s, held to `bounds`; staging under `staged`, which a Linux node
    /// alone takes: the staging server is the guest relay's (`crate::relay::files`), and a macOS or Windows node stages
    /// no file tree on any transport, only into its own guest, as its runtime's client (`hex_runtime::guest`).
    pub fn new(
        native: N,
        endpoint: Arc<Endpoint>,
        bounds: Bounds,
        staged: Option<std::path::PathBuf>,
    ) -> Result<Self, Refusal> {
        #[cfg(not(target_os = "linux"))]
        if staged.is_some() {
            return Err(Refusal::Platform {
                what: "staging file trees over QUIC",
                platform: std::env::consts::OS,
                why: "a node stages file trees only on Linux, where the relay's staging server runs; this node \
                      stages none, on any transport"
                    .to_owned(),
            });
        }
        let native = Arc::new(native);
        let wake: hex_quic::Notify = {
            let native = Arc::clone(&native);
            Arc::new(move || {
                let _ = native.wake();
            })
        };
        let shared = Arc::new(Shared {
            bounds,
            sets: Mutex::new(Sets {
                gather: Gather::new(bounds.sets),
                parked: BTreeMap::new(),
            }),
            controls: Mutex::new(BTreeMap::new()),
            pending: Mutex::new(VecDeque::new()),
            accepting: AtomicBool::new(true),
            tokens: AtomicU64::new(0),
            unnamed: AtomicUsize::new(0),
            #[cfg(target_os = "linux")]
            stagings: AtomicUsize::new(0),
            wake,
        });
        let log = Arc::new(Log::new(bounds.connections));
        spawn("hex-quic-control", {
            let (shared, acceptor) = (Arc::clone(&shared), endpoint.acceptor(stream::CONTROL_PORT));
            move || controls(&shared, &acceptor)
        })?;
        spawn("hex-quic-stdio", {
            let (shared, acceptor) = (Arc::clone(&shared), endpoint.acceptor(stream::STDIO_PORT));
            move || streams(&shared, &acceptor)
        })?;
        spawn("hex-quic-log", {
            let (log, acceptor) = (
                Arc::clone(&log),
                endpoint.acceptor(hex_core::lanes::LOG_PORT),
            );
            move || log.take(&acceptor)
        })?;
        #[cfg(target_os = "linux")]
        if let Some(staged) = staged {
            spawn("hex-quic-files", {
                let shared = Arc::clone(&shared);
                let acceptor = endpoint.acceptor(stream::files::FILES_PORT);
                move || stagings(&shared, &acceptor, &staged)
            })?;
        }
        Ok(Self {
            native,
            shared,
            watched: Mutex::new(BTreeMap::new()),
            log,
            _endpoint: endpoint,
        })
    }

    /// The node's log, as its log port's connections are sent it.
    pub fn log(&self) -> Arc<Log> {
        Arc::clone(&self.log)
    }

    fn watched(&self) -> Result<MutexGuard<'_, BTreeMap<u64, Arc<Control>>>, Refusal> {
        self.watched.lock().map_err(poisoned)
    }

    /// The events the QUIC connections make now, level-triggered as the native set is: a control watched for reading
    /// while a message waits or its peer has gone, one watched for writing while it has room; the listener while
    /// accepted controls wait.
    fn ready(&self, events: &mut Vec<Event>) {
        let accepting = self.shared.accepting.load(Ordering::Acquire);
        if accepting
            && self
                .shared
                .pending
                .lock()
                .is_ok_and(|pending| !pending.is_empty())
        {
            events.push(Event::Listener);
        }
        let Ok(watched) = self.watched.lock() else {
            return;
        };
        for (token, control) in watched.iter() {
            let interest = control
                .watch
                .lock()
                .ok()
                .and_then(|watch| watch.map(|(_, interest)| interest));
            let hung_up = control.shut.load(Ordering::Acquire) || control.conn.hung_up();
            let has = control.ready.lock().is_ok_and(|ready| !ready.is_empty());
            let (readable, writable) = match interest {
                Some(Interest::Read) => (has || control.conn.readable(), false),
                Some(Interest::Write) => (false, control.conn.writable()),
                None => (false, false),
            };
            if readable || writable || hung_up {
                events.push(Event::Ready {
                    token: *token,
                    readable,
                    writable,
                    hung_up,
                });
            }
        }
    }

    /// One whole message of `control`'s for the server: one made ready, else what has come read and framed, at most
    /// what the connection's seats hold a call.
    fn recv_quic(&self, control: &Control, buf: &mut [u8]) -> Recv<Passed<N::Passed>> {
        for _ in 0..=hex_quic::HELD_AT {
            if let Some(ready) = control.ready.lock().ok().and_then(|mut r| r.pop_front()) {
                let size = ready.message.len();
                let got = size.min(buf.len());
                if let (Some(to), Some(from)) = (buf.get_mut(..got), ready.message.get(..got)) {
                    to.copy_from_slice(from);
                }
                return Recv::Message {
                    size,
                    got,
                    passed: ready.pipes.into_iter().map(Passed::Pipe).collect(),
                    cut: false,
                };
            }
            let Ok(mut reading) = control.reading.lock() else {
                return Recv::Gone;
            };
            let Reading {
                deframer,
                buf: chunk,
            } = &mut *reading;
            let n = match control.conn.try_read(chunk) {
                Ok(Some(0)) | Err(_) => return Recv::Gone,
                Ok(Some(n)) => n,
                Ok(None) => return Recv::WouldBlock,
            };
            let mut input = chunk.get(..n).unwrap_or_default();
            while !input.is_empty() {
                let Ok((took, whole)) = deframer.take(input) else {
                    // Bytes that aren't a frame: the connection can't be read further, so it ends.
                    return Recv::Gone;
                };
                input = input.get(took..).unwrap_or_default();
                if let Some(message) = whole {
                    self.route(control, message);
                }
            }
        }
        Recv::WouldBlock
    }

    /// A whole message: an exec or a run with streams waits for them; anything else is ready as it came.
    fn route(&self, control: &Control, message: Vec<u8>) {
        let streams = match wire::read_request_apart(&message, wire::MAX_MESSAGE) {
            Ok((header, Request::Call(Call::Exec { streams, .. } | Call::Run { streams, .. })))
                if streams != Streams::default() =>
            {
                Some((header.id, streams))
            }
            _ => None,
        };
        let ready = match streams {
            None => Some(Ready {
                message,
                pipes: Vec::new(),
            }),
            Some((id, streams)) => {
                let exec = Exec {
                    conn: control.token,
                    id,
                };
                match self.shared.sets.lock() {
                    Ok(mut sets) => match sets.gather.expect(exec, streams) {
                        Ok(Some((_, pipes))) => Some(Ready { message, pipes }),
                        Ok(None) => {
                            let me = self
                                .shared
                                .controls
                                .lock()
                                .ok()
                                .and_then(|c| c.get(&control.token).cloned())
                                .unwrap_or_default();
                            sets.parked.insert(exec, (me, message));
                            None
                        }
                        // Refused (its streams' set past the bound, or one came twice or unasked): the exec goes
                        // with no stream, and the server refuses it by type for want of them.
                        Err(_) => Some(Ready {
                            message,
                            pipes: Vec::new(),
                        }),
                    },
                    Err(_) => Some(Ready {
                        message,
                        pipes: Vec::new(),
                    }),
                }
            }
        };
        if let Some(ready) = ready
            && let Ok(mut queue) = control.ready.lock()
        {
            queue.push_back(ready);
        }
    }
}

fn spawn(name: &str, f: impl FnOnce() + Send + 'static) -> Result<(), Refusal> {
    std::thread::Builder::new()
        .name(name.to_owned())
        .spawn(f)
        .map(drop)
        .map_err(|e| Refusal::os("starting a thread of the node's QUIC listener", e))
}

/// Takes control connections as they come: each sent its token, then held for the server to take, at most the
/// node's connections waiting.
fn controls(shared: &Arc<Shared>, acceptor: &Acceptor) {
    while let Ok((peer, conn)) = acceptor.accept(None) {
        let token = shared.tokens.fetch_add(1, Ordering::Relaxed);
        if conn.write_all(&token.to_le_bytes(), None).is_err() {
            continue;
        }
        let full = !shared
            .pending
            .lock()
            .is_ok_and(|pending| pending.len() < shared.bounds.connections);
        if full {
            conn.reset(Why::Conns);
            continue;
        }
        let _ = conn.set_notify(Arc::clone(&shared.wake));
        let control = Arc::new(Control {
            conn,
            token,
            peer,
            reading: Mutex::new(Reading {
                deframer: Deframer::new(wire::MAX_MESSAGE),
                buf: vec![0u8; hex_core::lanes::DATA_BOUND],
            }),
            ready: Mutex::new(VecDeque::new()),
            shut: AtomicBool::new(false),
            watch: Mutex::new(None),
            shared: Arc::downgrade(shared),
        });
        if let Ok(mut controls) = shared.controls.lock() {
            controls.insert(token, Arc::downgrade(&control));
        }
        if let Ok(mut pending) = shared.pending.lock() {
            pending.push_back(control);
        }
        (shared.wake)();
    }
}

/// Takes stdio connections as they come, each on a thread of its own that reads its name, makes its pipe, joins its
/// exec's set and copies; at most [`Bounds::unnamed`] yet to name themselves.
fn streams(shared: &Arc<Shared>, acceptor: &Acceptor) {
    while let Ok((peer, conn)) = acceptor.accept(None) {
        if shared.unnamed.fetch_add(1, Ordering::AcqRel) >= shared.bounds.unnamed {
            shared.unnamed.fetch_sub(1, Ordering::AcqRel);
            conn.reset(Why::Conns);
            continue;
        }
        let shared = Arc::clone(shared);
        let started = std::thread::Builder::new()
            .name("hex-quic-stream".to_owned())
            .spawn(move || stream_conn(&shared, peer, conn));
        if started.is_err() {
            return;
        }
    }
}

/// The copy one stdio connection's thread makes once its exec has its pipe.
enum Ours {
    /// Its program's stdin: bytes from the connection, into the pipe.
    Writer(PipeWriter),
    /// Its program's stdout or stderr: bytes from the pipe, to the connection.
    Reader(PipeReader),
}

fn stream_conn(shared: &Arc<Shared>, peer: PeerId, conn: hex_quic::Conn) {
    let named = {
        let mut name = [0u8; stream::NAME_BYTES];
        let mut have = 0;
        let read = loop {
            let Some(rest) = name.get_mut(have..).filter(|r| !r.is_empty()) else {
                break true;
            };
            match conn.read(rest, None) {
                Ok(0) | Err(_) => break false,
                Ok(n) => have = have.saturating_add(n),
            }
        };
        shared.unnamed.fetch_sub(1, Ordering::AcqRel);
        if read {
            stream::read_name(&name).ok()
        } else {
            None
        }
    };
    let Some(named) = named else {
        conn.reset(Why::Protocol);
        return;
    };
    // The control it names is open and the same peer's: no peer's stream joins another's exec.
    let control = shared
        .controls
        .lock()
        .ok()
        .and_then(|controls| controls.get(&named.exec.conn).and_then(Weak::upgrade))
        .filter(|control| control.peer == peer);
    if control.is_none() {
        conn.reset(Why::Protocol);
        return;
    }
    drop(control);
    let Ok((reader, writer)) = std::io::pipe() else {
        conn.reset(Why::Gone);
        return;
    };
    let (program, ours) = match named.stream {
        stream::Stdio::In => (PipeEnd::Stdin(reader), Ours::Writer(writer)),
        stream::Stdio::Out | stream::Stdio::Err => (PipeEnd::Out(writer), Ours::Reader(reader)),
    };
    if !gathered(shared, named, program) {
        conn.reset(Why::Protocol);
        return;
    }
    match ours {
        Ours::Writer(to) => feed(&conn, to),
        Ours::Reader(mut from) => {
            let mut buf = vec![0u8; hex_core::lanes::DATA_BOUND];
            loop {
                match from.read(&mut buf) {
                    Ok(0) => {
                        let _ = conn.end(None);
                        return;
                    }
                    Ok(n) => {
                        if conn
                            .write_all(buf.get(..n).unwrap_or_default(), None)
                            .is_err()
                        {
                            return;
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(_) => {
                        conn.reset(Why::Gone);
                        return;
                    }
                }
            }
        }
    }
}

/// Moves a program's stdin from its connection into its pipe until either ends, as the guest's relay does
/// (`crate::relay`'s `feed`, HEX-GUEST-41): the peer's End ends the pipe, so the program reads its stdin's end, and this
/// side's half then ends, closing the connection; the program's letting its stdin go (as its exit does) resets the
/// connection, so the peer's client lets its caller's pipe go, as a local program's stdin pipe has no reader once the
/// program has ended. Nothing goes to the peer on it. On Unix the copier waits for the connection and for the pipe's
/// readers together, in poll(2): the connection tells a pipe of the copier's own whenever it changes ([`Wake`]), and
/// the program's pipe's write end reports its readers gone, asked for `POLLHUP`: Linux reports `POLLERR` asked or not
/// (`pipe_poll`, fs/pipe.c), and macOS's poll registers its read filter for a `POLLHUP` asked, whose pipe filter
/// reports the reader's close as its end (`filt_piperead_common`, bsd/kern/sys_pipe.c), told as `POLLHUP`.
#[cfg(unix)]
fn feed(conn: &hex_quic::Conn, mut to: PipeWriter) {
    use rustix::event::{PollFd, PollFlags};
    let Ok(wake) = Wake::new() else {
        conn.reset(Why::Gone);
        return;
    };
    if conn.set_notify(wake.notify()).is_err() {
        conn.reset(Why::Gone);
        return;
    }
    let mut buf = vec![0u8; hex_core::lanes::DATA_BOUND];
    loop {
        match conn.try_read(&mut buf) {
            Ok(Some(0)) => {
                drop(to);
                let _ = conn.end(None);
                return;
            }
            Ok(Some(n)) => {
                if to.write_all(buf.get(..n).unwrap_or_default()).is_err() {
                    conn.reset(Why::Gone);
                    return;
                }
                continue;
            }
            Ok(None) => {}
            Err(_) => return,
        }
        let gone = {
            let mut ready = [
                PollFd::new(&to, PollFlags::HUP),
                PollFd::new(&wake.reader, PollFlags::IN),
            ];
            match rustix::event::poll(&mut ready, None) {
                Ok(_) => {}
                Err(rustix::io::Errno::INTR) => continue,
                Err(_) => {
                    conn.reset(Why::Gone);
                    return;
                }
            }
            let [pipe, _] = &ready;
            !pipe.revents().is_empty()
        };
        if gone {
            conn.reset(Why::Gone);
            return;
        }
        wake.drain(&mut buf);
    }
}

/// Where a pipe can't be waited on beside the connection (Windows'), the copy ends with the peer's End or a write the
/// program's pipe refuses, its readers gone, as the client's own stdin copy there does (hex-client's `copy_in`).
#[cfg(not(unix))]
fn feed(conn: &hex_quic::Conn, mut to: PipeWriter) {
    let mut buf = vec![0u8; hex_core::lanes::DATA_BOUND];
    loop {
        match conn.read(&mut buf, None) {
            Ok(0) => {
                drop(to);
                let _ = conn.end(None);
                return;
            }
            Ok(n) => {
                if to.write_all(buf.get(..n).unwrap_or_default()).is_err() {
                    conn.reset(Why::Gone);
                    return;
                }
            }
            Err(_) => return,
        }
    }
}

/// A pipe of a stdin copier's own, which its connection writes a byte to whenever it changes, never waiting: both
/// ends non-blocking, as a full pipe already says it changed. The copier empties it before it looks again.
#[cfg(unix)]
struct Wake {
    reader: PipeReader,
    writer: Arc<PipeWriter>,
}

#[cfg(unix)]
impl Wake {
    fn new() -> std::io::Result<Self> {
        let (reader, writer) = std::io::pipe()?;
        rustix::io::ioctl_fionbio(&reader, true)?;
        rustix::io::ioctl_fionbio(&writer, true)?;
        Ok(Self {
            reader,
            writer: Arc::new(writer),
        })
    }

    fn notify(&self) -> hex_quic::Notify {
        let writer = Arc::clone(&self.writer);
        Arc::new(move || {
            let _ = (&*writer).write(&[0]);
        })
    }

    fn drain(&self, buf: &mut [u8]) {
        while matches!((&self.reader).read(buf), Ok(n) if n > 0) {}
    }
}

/// `program`'s pipe end joins its exec's set: once whole, a parked exec goes to its control with the set's pipes.
/// Whether it was taken.
fn gathered(shared: &Arc<Shared>, named: Named, program: PipeEnd) -> bool {
    let Ok(mut sets) = shared.sets.lock() else {
        return false;
    };
    match sets.gather.arrive(named, program) {
        Ok(None) => true,
        Ok(Some((exec, pipes))) => {
            if let Some((control, message)) = sets.parked.remove(&exec)
                && let Some(control) = control.upgrade()
                && let Ok(mut ready) = control.ready.lock()
            {
                ready.push_back(Ready { message, pipes });
                drop(ready);
                drop(sets);
                (shared.wake)();
            }
            true
        }
        Err(_) => false,
    }
}

/// Takes staging connections as they come, each served by the relay's staging server on a thread of its own, at most
/// the node's connections at once.
#[cfg(target_os = "linux")]
fn stagings(shared: &Arc<Shared>, acceptor: &Acceptor, staged: &std::path::Path) {
    while let Ok((_, conn)) = acceptor.accept(None) {
        if shared.stagings.fetch_add(1, Ordering::AcqRel) >= shared.bounds.connections {
            shared.stagings.fetch_sub(1, Ordering::AcqRel);
            conn.reset(Why::Conns);
            continue;
        }
        let Ok(socket) = hex_quic::bridge::socket(conn) else {
            shared.stagings.fetch_sub(1, Ordering::AcqRel);
            continue;
        };
        let (shared, staged) = (Arc::clone(shared), staged.to_owned());
        let started = std::thread::Builder::new()
            .name("hex-quic-stage".to_owned())
            .spawn(move || {
                crate::relay::files::serve(
                    Box::new(socket),
                    &staged,
                    &hex_core::stream::files::LIMITS,
                );
                shared.stagings.fetch_sub(1, Ordering::AcqRel);
            });
        if started.is_err() {
            return;
        }
    }
}

/// The node's log lines, sent to each connection to the log port. A line goes to every reader that has room for it
/// now; one that hasn't misses it, counted, and is told how many it missed before its next line, so none is lost
/// unseen (CLAUDE.md §3) and no reader holds the node.
pub struct Log {
    readers: Mutex<Vec<Reader>>,
    bound: usize,
}

struct Reader {
    conn: hex_quic::Conn,
    missed: u64,
}

impl Log {
    fn new(bound: usize) -> Self {
        Self {
            readers: Mutex::new(Vec::new()),
            bound,
        }
    }

    fn take(&self, acceptor: &Acceptor) {
        while let Ok((_, conn)) = acceptor.accept(None) {
            let Ok(mut readers) = self.readers.lock() else {
                return;
            };
            if readers.len() >= self.bound {
                conn.reset(Why::Conns);
                continue;
            }
            readers.push(Reader { conn, missed: 0 });
        }
    }

    /// How many connections read the log now.
    pub fn readers(&self) -> usize {
        self.readers.lock().map_or(0, |readers| readers.len())
    }

    /// Lines its readers missed and haven't been told of yet.
    pub fn missed(&self) -> u64 {
        self.readers.lock().map_or(0, |readers| {
            readers
                .iter()
                .fold(0u64, |sum, reader| sum.saturating_add(reader.missed))
        })
    }

    /// Sends `line` to every reader with room for it, at most one frame of it (a longer one is cut at the frame's
    /// bound), the count of the lines a reader missed before it in the same frame, since a connection takes one
    /// frame at a time.
    pub fn say(&self, line: &str) {
        let Ok(mut readers) = self.readers.lock() else {
            return;
        };
        let bound = hex_core::lanes::DATA_BOUND;
        readers.retain_mut(|reader| {
            let mut frame = Vec::new();
            if reader.missed > 0 {
                frame.extend_from_slice(
                    format!("hex.log event=missed lines={}\n", reader.missed).as_bytes(),
                );
            }
            frame.extend_from_slice(line.as_bytes());
            if frame.len() > bound {
                frame.truncate(bound.saturating_sub(1));
                frame.push(b'\n');
            }
            match reader.conn.try_write(&frame) {
                Ok(true) => {
                    reader.missed = 0;
                    true
                }
                Ok(false) => {
                    reader.missed = reader.missed.saturating_add(1);
                    true
                }
                Err(_) => false,
            }
        });
    }
}

impl<N: Os> Os for Quic<N> {
    type Conn = Conn<N::Conn>;
    type Watch = N::Watch;
    type Passed = Passed<N::Passed>;

    fn wait(&self, events: &mut Vec<Event>, timeout: Option<Duration>) -> Result<(), Refusal> {
        let mut quic = Vec::new();
        self.ready(&mut quic);
        let timeout = if quic.is_empty() {
            timeout
        } else {
            Some(Duration::ZERO)
        };
        self.native.wait(events, timeout)?;
        if quic.is_empty() {
            self.ready(&mut quic);
        }
        let room = EVENTS.saturating_sub(events.len());
        events.extend(quic.into_iter().take(room));
        Ok(())
    }

    fn wake(&self) -> Result<(), Refusal> {
        self.native.wake()
    }

    fn accept(&self) -> Accepted<Self::Conn> {
        match self.native.accept() {
            Accepted::Conn(conn) => Accepted::Conn(Conn::Native(conn)),
            Accepted::Spent => Accepted::Spent,
            Accepted::Failed(e) => Accepted::Failed(e),
            Accepted::None => {
                if !self.shared.accepting.load(Ordering::Acquire) {
                    return Accepted::None;
                }
                match self
                    .shared
                    .pending
                    .lock()
                    .ok()
                    .and_then(|mut pending| pending.pop_front())
                {
                    Some(control) => Accepted::Conn(Conn::Quic(control)),
                    None => Accepted::None,
                }
            }
        }
    }

    fn accepting(&self, on: bool) -> Result<(), Refusal> {
        self.shared.accepting.store(on, Ordering::Release);
        self.native.accepting(on)
    }

    fn stop_listening(&self) {
        self.shared.accepting.store(false, Ordering::Release);
        self.native.stop_listening();
    }

    fn peer(&self, conn: &Self::Conn) -> Peer {
        match conn {
            Conn::Native(conn) => self.native.peer(conn),
            // Its handshake proved a certificate kiln's CA issued (hex_quic::tls).
            Conn::Quic(_) => Peer::Allowed,
        }
    }

    fn watch(&self, conn: &Self::Conn, token: u64, interest: Interest) -> Result<(), Refusal> {
        match conn {
            Conn::Native(conn) => self.native.watch(conn, token, interest),
            Conn::Quic(control) => {
                *control.watch.lock().map_err(poisoned)? = Some((token, interest));
                self.watched()?.insert(token, Arc::clone(control));
                Ok(())
            }
        }
    }

    fn rewatch(&self, conn: &Self::Conn, token: u64, interest: Interest) -> Result<(), Refusal> {
        match conn {
            Conn::Native(conn) => self.native.rewatch(conn, token, interest),
            Conn::Quic(_) => self.watch(conn, token, interest),
        }
    }

    fn unwatch(&self, conn: &Self::Conn) {
        match conn {
            Conn::Native(conn) => self.native.unwatch(conn),
            Conn::Quic(control) => {
                let token = control
                    .watch
                    .lock()
                    .ok()
                    .and_then(|mut watch| watch.take())
                    .map(|(token, _)| token);
                if let (Some(token), Ok(mut watched)) = (token, self.watched.lock()) {
                    watched.remove(&token);
                }
            }
        }
    }

    fn recv(&self, conn: &Self::Conn, buf: &mut [u8]) -> Recv<Self::Passed> {
        match conn {
            Conn::Native(conn) => match self.native.recv(conn, buf) {
                Recv::Message {
                    size,
                    got,
                    passed,
                    cut,
                } => Recv::Message {
                    size,
                    got,
                    passed: passed.into_iter().map(Passed::Native).collect(),
                    cut,
                },
                Recv::WouldBlock => Recv::WouldBlock,
                Recv::Reset => Recv::Reset,
                Recv::Gone => Recv::Gone,
            },
            Conn::Quic(control) => self.recv_quic(control, buf),
        }
    }

    fn send(&self, conn: &Self::Conn, bytes: &[u8], fds: &[hex_api::End]) -> Sent {
        match conn {
            Conn::Native(conn) => self.native.send(conn, bytes, fds),
            Conn::Quic(control) => {
                // A descriptor can't cross the network: a QUIC client makes the streams it asks for itself (the
                // guest's protocol), so an answer carrying ends has no taker, and the connection ends.
                if !fds.is_empty() {
                    return Sent::Gone;
                }
                let Ok(framed) = stream::frame(bytes, wire::MAX_MESSAGE) else {
                    return Sent::Gone;
                };
                match control.conn.try_write(&framed) {
                    Ok(true) => Sent::Sent,
                    Ok(false) => Sent::WouldBlock,
                    Err(_) => Sent::Gone,
                }
            }
        }
    }

    fn shutdown(&self, conn: &Self::Conn) {
        match conn {
            Conn::Native(conn) => self.native.shutdown(conn),
            Conn::Quic(control) => {
                control.shut.store(true, Ordering::Release);
                let _ = control.conn.close_write();
                (self.shared.wake)();
            }
        }
    }

    fn hung_up(&self, conn: &Self::Conn) -> bool {
        match conn {
            Conn::Native(conn) => self.native.hung_up(conn),
            Conn::Quic(control) => control.shut.load(Ordering::Acquire) || control.conn.hung_up(),
        }
    }

    fn watch_program(&self, pid: u32, token: u64) -> Result<Self::Watch, Unwatched> {
        self.native.watch_program(pid, token)
    }

    fn unwatch_program(&self, watch: Self::Watch) {
        self.native.unwatch_program(watch);
    }

    fn stdio(&self, streams: Streams, passed: Vec<Self::Passed>) -> Result<Stdio, Refusal> {
        let mut natives = Vec::with_capacity(passed.len());
        let mut pipes = Vec::with_capacity(passed.len());
        for p in passed {
            match p {
                Passed::Native(n) => natives.push(n),
                Passed::Pipe(pipe) => pipes.push(pipe),
            }
        }
        if pipes.is_empty() {
            return self.native.stdio(streams, natives);
        }
        if !natives.is_empty() {
            return Err(Refusal::Invalid {
                what: "stdio",
                why: "an exec's streams came both as descriptors and over QUIC".to_owned(),
            });
        }
        let mut pipes = pipes.into_iter();
        let missing = |name: &str| Refusal::Invalid {
            what: "stdio",
            why: format!("{name}'s stream didn't come"),
        };
        let stdin = match streams.stdin {
            false => None,
            true => match pipes.next() {
                Some(PipeEnd::Stdin(reader)) => Some(reader),
                _ => return Err(missing("stdin")),
            },
        };
        let mut out = |on: bool, name: &str| -> Result<Option<PipeWriter>, Refusal> {
            if !on {
                return Ok(None);
            }
            match pipes.next() {
                Some(PipeEnd::Out(writer)) => Ok(Some(writer)),
                _ => Err(missing(name)),
            }
        };
        let stdout = out(streams.stdout, "stdout")?;
        let stderr = out(streams.stderr, "stderr")?;
        Ok(Stdio {
            stdin,
            stdout,
            stderr,
            ..Stdio::null()
        })
    }

    fn watch_source(&self, handle: Handle<'_>, token: u64, write: bool) -> Result<(), Refusal> {
        self.native.watch_source(handle, token, write)
    }

    fn unwatch_source(&self, handle: Handle<'_>, token: u64) {
        self.native.unwatch_source(handle, token);
    }
}

#[cfg(test)]
mod tests;
