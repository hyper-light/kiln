//! The guest's relay (docs/design/shards-guest.md §3): in a Shards guest, it takes the host's
//! connections and speaks the node agent's own socket, so the node, its wire and its disconnect rule stay
//! as they are on any Linux host (CLAUDE.md §8).
//!
//! - **Control:** each host connection is sent its token ([`stream::TOKEN_BYTES`]) and given a node
//!   connection of its own. Its framed requests go to the node one message each; the node's answers go
//!   back framed. When either side ends, so does the other: the node's disconnect rule then ends the
//!   programs the host left.
//! - **Stdio:** an exec's streams come on connections of their own, named by their first bytes, each read
//!   on the thread that later copies it. The relay gathers them ([`Gather`]), makes a pipe for each,
//!   passes the pipe's end to the node with the exec, and copies between the pipe and the connection, one
//!   thread and one buffer per stream. An exec whose streams haven't all come is parked, and the host's
//!   connection read on, so its set waits on facts alone: its last stream, or its host's end.
//!
//! Each resource has a bound, refused past by type (CLAUDE.md §2, §3):
//! - control connections ([`Limits::connections`], the node's own, as its hello told them: [`Limits::of`]): one
//!   more is sent its token and a refusal (`Exhausted`, as the node's socket refuses one) and closed;
//! - execs and early streams waiting for each other ([`Limits::sets`]): one more is refused, the exec
//!   answered `Invalid` naming the bound, the stream closed; a set goes when its host's connection ends,
//!   which no exec of its can outlive, its streams closed;
//! - stdio connections yet to name themselves: as many as the sets' streams ([`Limits::unnamed`]); one
//!   more is refused (`Exhausted`, logged on `hex.guest` as `stdio_unnamed`) and closed;
//! - staging connections at once (the control connections' bound again): one more is answered
//!   `Exhausted` and closed; a stage's entries, bytes and path lengths ([`Limits::files`]).
//!
//! - **Files:** a box's bundle and root come on the staging port before its create
//!   (docs/design/shards-guest.md §4a, [`files`]), written under the staged folder, and go with it.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, IoSlice, IoSliceMut, PipeReader, PipeWriter, Read, Write};
use std::mem::MaybeUninit;
use std::net::Shutdown;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use hex_api::Refusal;
use hex_client::guest::Duplex;
use hex_core::stream::{self, Deframer, Exec, Gather, Stray};
use hex_core::wire::{self, Answer, Call, Request};
use rustix::event::{PollFd, PollFlags};
use rustix::io::Errno;
use rustix::net::{
    AddressFamily, RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, SendAncillaryBuffer,
    SendAncillaryMessage, SendFlags, SocketAddrUnix, SocketFlags, SocketType,
};

/// The relay's bounds.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Host control connections at once.
    pub connections: usize,
    /// Execs and early streams waiting for each other at once.
    pub sets: usize,
    /// Stdio connections accepted and yet to name themselves at once.
    pub unnamed: usize,
    /// A stage's entries, bytes and path lengths.
    pub files: hex_core::stream::files::Limits,
}

impl Limits {
    /// The relay's bounds over its node, as the node's hello told them (`node`, each connection's, and `budgets`,
    /// over them all), so none is picked (Ada's rule, 2026-11-06) and none is the relay's own guess at the node's:
    /// - the node's connections, as each control connection holds one of the node's;
    /// - as many sets as the node's calls in flight together, so no host within the node's budgets meets the
    ///   sets' bound;
    /// - every stream of every set naming itself at once: [`STREAMS`] a set.
    pub fn of(node: &wire::Limits, budgets: &wire::Budgets) -> Self {
        Self {
            connections: node.connections,
            sets: budgets.in_flight,
            unnamed: budgets.in_flight.saturating_mul(STREAMS),
            files: hex_core::stream::files::LIMITS,
        }
    }

    /// The relay's bounds over the node agent at `node`, asked of it in a hello of their own, within `deadline` where
    /// its caller gives one: the guest's boot's (AC-T7.4.1), which a node busy past the hello's learned wait, as it
    /// initializes the guest's GPU, doesn't end sooner.
    pub fn asked(node: &Path, deadline: Option<Instant>) -> Result<Self, Refusal> {
        let told = match deadline {
            Some(deadline) => hex_client::Client::connect_before(node, deadline)?,
            None => hex_client::Client::connect(node)?,
        };
        Ok(Self::of(&told.limits(), &told.budgets()))
    }
}

/// Where the guest's relay writes staged bundles and roots: the guest's tmpfs `/run`, charged to its
/// memory (docs/design/shards-guest.md §4).
pub const STAGED: &str = "/run/hex/staged";
/// The most streams an exec has: stdin, stdout and stderr (docs/design/shards-guest.md §3a).
pub const STREAMS: usize = 3;

/// A host connection as the relay takes it: a [`Duplex`] whose descriptor the stdio copiers splice
/// through.
pub trait Conn: Duplex + AsFd {}

impl<T: Duplex + AsFd> Conn for T {}

/// What accepts the host's connections: AF_VSOCK in the guest, a Unix socket in tests.
pub trait Listen: Send + Sync + 'static {
    fn accept(&self) -> io::Result<Box<dyn Conn>>;
}

impl Listen for std::os::unix::net::UnixListener {
    fn accept(&self) -> io::Result<Box<dyn Conn>> {
        Ok(Box::new(std::os::unix::net::UnixListener::accept(self)?.0))
    }
}

/// The relay's state, shared by its threads.
pub struct Relay {
    node: PathBuf,
    staged: PathBuf,
    limits: Limits,
    sets: Mutex<Sets>,
    tokens: AtomicU64,
    connections: AtomicUsize,
    stagings: AtomicUsize,
    unnamed: AtomicUsize,
    /// Stdio connections refused past [`Limits::unnamed`], for the node's counters.
    refused_unnamed: AtomicU64,
}

/// A named stream's thread, waiting for the copier its exec's pipe gives it; dropped, the thread ends
/// and its connection closes.
type Waiting = SyncSender<Copier>;

/// Live controls, their sets under way, and the execs parked until their sets are whole.
struct Sets {
    /// Tokens whose controls can still supply execs, at most the admitted control connection limit.
    active: BTreeSet<u64>,
    gather: Gather<Waiting>,
    parked: BTreeMap<Exec, Parked>,
}

/// An exec whose streams haven't all come: what to send the node, and where, once they have.
struct Parked {
    message: Vec<u8>,
    id: u64,
    streams: wire::Streams,
    node: Arc<OwnedFd>,
    writer: Arc<Mutex<hex_client::guest::DuplexOwner>>,
}

fn poisoned<T>(_: std::sync::PoisonError<T>) -> Refusal {
    Refusal::Broken("a lock of the guest's relay was poisoned".to_owned())
}

impl Relay {
    /// A relay to the node agent's socket at `node`, staging under `staged`, holding `limits`.
    pub fn new(node: &Path, staged: &Path, limits: Limits) -> Arc<Self> {
        Arc::new(Self {
            node: node.to_owned(),
            staged: staged.to_owned(),
            limits,
            sets: Mutex::new(Sets {
                active: BTreeSet::new(),
                gather: Gather::new(limits.sets),
                parked: BTreeMap::new(),
            }),
            tokens: AtomicU64::new(0),
            connections: AtomicUsize::new(0),
            stagings: AtomicUsize::new(0),
            unnamed: AtomicUsize::new(0),
            refused_unnamed: AtomicU64::new(0),
        })
    }

    /// Stdio connections refused because more were yet to name themselves than [`Limits::unnamed`].
    pub fn refused_unnamed(&self) -> u64 {
        self.refused_unnamed.load(Ordering::Acquire)
    }

    fn sets(&self) -> Result<MutexGuard<'_, Sets>, Refusal> {
        self.sets.lock().map_err(poisoned)
    }

    /// Register before publishing the token, so even a stream that arrives before its exec belongs to a live
    /// control. The guard closes registration and forgets its sets together on every control return (HEX-RT-17).
    fn activate(&self, token: u64) -> Result<ActiveControl<'_>, Refusal> {
        let mut sets = self.sets()?;
        if sets.active.len() >= self.limits.connections {
            return Err(Refusal::Exhausted {
                what: crate::serve::CONNECTIONS,
                bound: self.limits.connections,
            });
        }
        if !sets.active.insert(token) {
            return Err(Refusal::Broken(
                "the guest relay reused a live control token".to_owned(),
            ));
        }
        Ok(ActiveControl { relay: self, token })
    }

    /// Serves `control`, `stdio` and `files` until a listener fails, which is returned.
    pub fn serve(
        self: &Arc<Self>,
        control: Box<dyn Listen>,
        stdio: Box<dyn Listen>,
        files: Box<dyn Listen>,
    ) -> Result<(), Refusal> {
        let streams = {
            let relay = Arc::clone(self);
            std::thread::Builder::new()
                .name("hex-relay-stdio".to_owned())
                .spawn(move || relay.take_streams(stdio.as_ref()))
                .map_err(|e| Refusal::os("starting the relay's stdio thread", e))?
        };
        let stagings = {
            let relay = Arc::clone(self);
            std::thread::Builder::new()
                .name("hex-relay-files".to_owned())
                .spawn(move || relay.take_stagings(files.as_ref()))
                .map_err(|e| Refusal::os("starting the relay's staging thread", e))?
        };
        let why = loop {
            let conn = match control.accept() {
                Ok(conn) => conn,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => break Refusal::os("accepting a host's control connection", e),
            };
            let relay = Arc::clone(self);
            if let Err(e) = std::thread::Builder::new()
                .name("hex-relay-control".to_owned())
                .spawn(move || relay.control(conn))
            {
                break Refusal::os("starting a control connection's thread", e);
            }
        };
        drop((streams, stagings));
        Err(why)
    }

    /// Takes staging connections as they come, each on a thread of its own, at most the control
    /// connections' bound at once.
    fn take_stagings(self: &Arc<Self>, listener: &dyn Listen) -> Refusal {
        loop {
            let mut conn = match listener.accept() {
                Ok(conn) => conn,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Refusal::os("accepting a host's staging connection", e),
            };
            let held = self.stagings.fetch_add(1, Ordering::AcqRel);
            if held >= self.limits.connections {
                self.stagings.fetch_sub(1, Ordering::AcqRel);
                refuse(
                    conn.as_mut(),
                    Refusal::Exhausted {
                        what: "staging connections",
                        bound: self.limits.connections,
                    },
                );
                continue;
            }
            let relay = Arc::clone(self);
            let started = std::thread::Builder::new()
                .name("hex-relay-stage".to_owned())
                .spawn(move || {
                    let _release = Release(&relay.stagings);
                    files::serve(conn, &relay.staged, &relay.limits.files);
                });
            if let Err(e) = started {
                return Refusal::os("starting a staging connection's thread", e);
            }
        }
    }

    /// Takes stdio connections as they come, each on a thread of its own that reads its name, joins
    /// its exec's set and, once the exec has its pipe, copies it; at most [`Limits::unnamed`] at once
    /// yet to name themselves.
    fn take_streams(self: &Arc<Self>, stdio: &dyn Listen) -> Refusal {
        loop {
            let conn = match stdio.accept() {
                Ok(conn) => conn,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Refusal::os("accepting a host's stdio connection", e),
            };
            let held = self.unnamed.fetch_add(1, Ordering::AcqRel);
            if held >= self.limits.unnamed {
                self.unnamed.fetch_sub(1, Ordering::AcqRel);
                self.refused_unnamed.fetch_add(1, Ordering::AcqRel);
                log_unnamed(&Refusal::Exhausted {
                    what: "stdio connections yet to name themselves",
                    bound: self.limits.unnamed,
                });
                continue;
            }
            let relay = Arc::clone(self);
            let started = std::thread::Builder::new()
                .name("hex-relay-stdio-copy".to_owned())
                .spawn(move || relay.stream(conn));
            if let Err(e) = started {
                self.unnamed.fetch_sub(1, Ordering::AcqRel);
                return Refusal::os("starting a stdio connection's thread", e);
            }
        }
    }

    /// One stdio connection: its name, read as long as the host takes to write it (the host's end ends
    /// it), its place in its exec's set, then its copy, once its exec has its pipe. A connection that
    /// names nothing, or a stream its set refuses, is closed as it drops.
    fn stream(self: Arc<Self>, mut conn: Box<dyn Conn>) {
        let named = {
            let _release = Release(&self.unnamed);
            let mut name = [0u8; stream::NAME_BYTES];
            conn.read_exact(&mut name)
                .ok()
                .and_then(|()| stream::read_name(&name).ok())
        };
        let Some(named) = named else { return };
        let (copier, copy) = std::sync::mpsc::sync_channel(1);
        let ready = {
            let Ok(mut sets) = self.sets() else { return };
            // A control's guard removes registration and its sets under this same lock. No late stream can
            // recreate a set after forget, nor can an unknown control reserve one (HEX-GUEST-7, HEX-RT-17).
            if !sets.active.contains(&named.exec.conn) {
                return;
            }
            match sets.gather.arrive(named, copier) {
                Ok(Some((exec, waiting))) => sets.parked.remove(&exec).map(|p| (p, waiting)),
                Ok(None) => None,
                Err(Stray { .. }) => return,
            }
        };
        if let Some((parked, waiting)) = ready {
            let _ = deliver(&parked, waiting);
        }
        if let Ok(copy) = copy.recv() {
            copy.run(conn);
        }
    }

    /// Serves one host control connection until either side ends.
    fn control(self: Arc<Self>, mut host: Box<dyn Duplex>) {
        let token = self.tokens.fetch_add(1, Ordering::Relaxed);
        let held = self.connections.fetch_add(1, Ordering::AcqRel);
        let _release = Release(&self.connections);
        if held >= self.limits.connections {
            if host.write_all(&token.to_le_bytes()).is_ok() {
                refuse(
                    host.as_mut(),
                    Refusal::Exhausted {
                        what: crate::serve::CONNECTIONS,
                        bound: self.limits.connections,
                    },
                );
            }
            return;
        }
        let active = match self.activate(token) {
            Ok(active) => active,
            Err(why) => {
                if host.write_all(&token.to_le_bytes()).is_ok() {
                    refuse(host.as_mut(), why);
                }
                return;
            }
        };
        if host.write_all(&token.to_le_bytes()).is_err() {
            return;
        }
        let node = match connect(&self.node) {
            Ok(node) => Arc::new(node),
            Err(why) => {
                refuse(host.as_mut(), why);
                return;
            }
        };
        let Ok(writer) = host.try_clone() else { return };
        let writer = Arc::new(Mutex::new(writer));
        let answers = {
            let (node, writer) = (Arc::clone(&node), Arc::clone(&writer));
            std::thread::Builder::new()
                .name("hex-relay-answers".to_owned())
                .spawn(move || answers(&node, &writer))
        };
        let Ok(answers) = answers else { return };
        self.requests(token, host.as_mut(), &node, &writer);
        // The host has gone: its execs can't be answered, nor their streams come, so its sets go, their
        // streams' threads ending as their connections close.
        drop(active);
        // The node sees its connection end and applies its disconnect rule.
        let _ = rustix::net::shutdown(&*node, rustix::net::Shutdown::Both);
        let _ = answers.join();
    }

    /// The host's requests, each to the node, until the host's connection ends.
    fn requests(
        &self,
        token: u64,
        host: &mut dyn Duplex,
        node: &Arc<OwnedFd>,
        writer: &Arc<Mutex<hex_client::guest::DuplexOwner>>,
    ) {
        let mut deframer = Deframer::new(wire::MAX_MESSAGE);
        // A read takes at most one whole frame, the most the deframer holds.
        let mut buf = vec![0u8; stream::MAX_FRAME];
        loop {
            let n = match host.read(&mut buf) {
                Ok(0) => return,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => return,
            };
            let mut input = buf.get(..n).unwrap_or_default();
            while !input.is_empty() {
                let Ok((took, whole)) = deframer.take(input) else {
                    // Bytes that aren't a frame: the connection can't be read further, so it ends.
                    return;
                };
                input = input.get(took..).unwrap_or_default();
                let Some(message) = whole else { continue };
                if self.request(token, &message, node, writer).is_err() {
                    return;
                }
            }
        }
    }

    /// One request to the node: an exec or a run (decision 7) with its streams' pipes, once they have all
    /// come, anything else as it came (the node refuses what isn't a request itself). One whose streams
    /// haven't all come is parked, and the host's connection read on.
    fn request(
        &self,
        token: u64,
        message: &[u8],
        node: &Arc<OwnedFd>,
        writer: &Arc<Mutex<hex_client::guest::DuplexOwner>>,
    ) -> Result<(), Gone> {
        let exec = match wire::read_request_apart(message, wire::MAX_MESSAGE) {
            Ok((header, Request::Call(Call::Exec { streams, .. } | Call::Run { streams, .. }))) => {
                Some((header, streams))
            }
            _ => None,
        };
        let Some((header, streams)) = exec else {
            return send(node, message, &[]);
        };
        let exec = Exec {
            conn: token,
            id: header.id,
        };
        let parked = Parked {
            message: message.to_vec(),
            id: header.id,
            streams,
            node: Arc::clone(node),
            writer: Arc::clone(writer),
        };
        let ready = {
            let mut sets = self.sets().map_err(|_| Gone)?;
            match sets.gather.expect(exec, streams) {
                Ok(Some((_, waiting))) => Ok(waiting),
                Ok(None) => {
                    sets.parked.insert(exec, parked);
                    return Ok(());
                }
                Err(refused) => Err(Refusal::Invalid {
                    what: "stdio",
                    why: format!("the exec's streams were refused: {}", refused.why),
                }),
            }
        };
        match ready {
            Ok(waiting) => deliver(&parked, waiting),
            Err(why) => refuse_exec(&parked, why),
        }
    }
}

/// A control's registration and pending streams have one lifetime, including setup failures after its token
/// was published. Membership and Gather cleanup share one lock, so arrival cannot race behind forget.
struct ActiveControl<'a> {
    relay: &'a Relay,
    token: u64,
}

impl Drop for ActiveControl<'_> {
    fn drop(&mut self) {
        if let Ok(mut sets) = self.relay.sets() {
            sets.active.remove(&self.token);
            drop(sets.gather.forget(self.token));
            let gone: Vec<Exec> = sets
                .parked
                .range(
                    Exec {
                        conn: self.token,
                        id: 0,
                    }..=Exec {
                        conn: self.token,
                        id: u64::MAX,
                    },
                )
                .map(|(exec, _)| *exec)
                .collect();
            for exec in gone {
                sets.parked.remove(&exec);
            }
        }
    }
}

/// Gives back a connection's place in the bound when its thread ends.
struct Release<'a>(&'a AtomicUsize);

impl Drop for Release<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// The host's connection is gone, or the node's.
struct Gone;

/// Refuses a host connection whose token went: a refusal of no request (id 0), as the node's socket
/// refuses one, which the host's hello returns.
fn refuse(host: &mut dyn Duplex, why: Refusal) {
    if let Some(bytes) = wire::answer(0, &Answer::Refused(why), wire::MAX_MESSAGE)
        .ok()
        .and_then(|bytes| stream::frame(&bytes, wire::MAX_MESSAGE).ok())
    {
        let _ = host.write_all(&bytes);
    }
    let _ = host.shutdown(Shutdown::Both);
}

/// A connection to the node agent's socket at `path`.
fn connect(path: &Path) -> Result<OwnedFd, Refusal> {
    let addr = SocketAddrUnix::new(path).map_err(|e| Refusal::Invalid {
        what: "socket path",
        why: format!("{}: {e}", path.display()),
    })?;
    let fd = rustix::net::socket_with(
        AddressFamily::UNIX,
        SocketType::SEQPACKET,
        SocketFlags::CLOEXEC,
        None,
    )
    .map_err(|e| Refusal::os("making a socket to the node agent", e))?;
    rustix::net::connect(&fd, &addr).map_err(|e| Refusal::Os {
        op: "connecting to the node agent's socket",
        why: format!("{}: {e}", path.display()),
    })?;
    Ok(fd)
}

/// The parked exec `parked` to the node, with a pipe for each of its streams, whose threads `waiting`
/// copy them; or refused to its host if a stream's thread has gone.
fn deliver(parked: &Parked, waiting: Vec<Waiting>) -> Result<(), Gone> {
    match pipes(parked.streams, waiting) {
        Ok(fds) => send(&parked.node, &parked.message, &fds),
        Err(why) => refuse_exec(parked, why),
    }
}

/// Answers the parked exec `parked` with `why` on its host's connection.
fn refuse_exec(parked: &Parked, why: Refusal) -> Result<(), Gone> {
    let answer = wire::answer(parked.id, &Answer::Refused(why), wire::MAX_MESSAGE)
        .ok()
        .and_then(|bytes| stream::frame(&bytes, wire::MAX_MESSAGE).ok());
    let mut w = parked.writer.lock().map_err(|_| Gone)?;
    w.write_all(&answer.unwrap_or_default()).map_err(|_| Gone)
}

/// A stdio connection refused before it named itself, on `hex.guest` (the guest's console), as the
/// event `stdio_unnamed`.
fn log_unnamed(why: &Refusal) {
    let _ = io::stderr()
        .lock()
        .write_all(format!("hex.guest event=stdio_unnamed why=\"{why}\"\n").as_bytes());
}

/// For each stream, in stdin-stdout-stderr order, the pipe end the node takes, with the stream's
/// thread given the copier between the pipe's other end and its connection.
fn pipes(streams: wire::Streams, waiting: Vec<Waiting>) -> Result<Vec<OwnedFd>, Refusal> {
    let order = [
        (streams.stdin, true),
        (streams.stdout, false),
        (streams.stderr, false),
    ];
    let mut waiting = waiting.into_iter();
    let mut fds = Vec::with_capacity(streams.count());
    for (on, input) in order {
        if !on {
            continue;
        }
        let thread = waiting.next().ok_or_else(|| Refusal::Invalid {
            what: "stdio",
            why: "a stream named by the exec didn't come".to_owned(),
        })?;
        let (r, w) = std::io::pipe().map_err(|e| Refusal::os("making a stdio pipe", e))?;
        let (theirs, copy): (OwnedFd, Copier) = if input {
            (OwnedFd::from(r), Copier::In(w))
        } else {
            (OwnedFd::from(w), Copier::Out(r))
        };
        thread.send(copy).map_err(|_| {
            Refusal::Broken("a stream's connection ended before its exec started".to_owned())
        })?;
        fds.push(theirs);
    }
    Ok(fds)
}

/// What one stdio copier copies.
enum Copier {
    /// From the host's connection into the program's stdin pipe.
    In(PipeWriter),
    /// From the program's stdout or stderr pipe to the host's connection, which then ends its sending
    /// half, so the host sees the stream end.
    Out(PipeReader),
}

impl Copier {
    /// Moves the stream until either side ends, by splice(2) through the pipe, so no byte passes through
    /// user space. Pipe to connection: the socket's `splice_write` (`splice_to_socket`, net/socket.c).
    /// Connection to pipe: the socket's `splice_read`, which for vsock, whose `vsock_stream_ops` names none
    /// (net/vmw_vsock/af_vsock.c), is the kernel's own copy into the pipe (`sock_splice_read` falls back to
    /// `copy_splice_read`, net/socket.c, Linux 6.18). Either way the copier waits only in poll ([`feed`],
    /// [`drain`]), never in a splice. Each call moves at most the pipe's capacity (`F_GETPIPE_SZ`, fcntl(2)),
    /// learned from the pipe itself. Its connection closes as the copy ends.
    fn run(self, conn: Box<dyn Conn>) {
        match self {
            Self::In(to) => {
                let _ = feed(&*conn, &to);
            }
            Self::Out(from) => {
                let _ = drain(&from, &*conn);
                let _ = conn.shutdown(Shutdown::Write);
            }
        }
    }
}

/// Moves the host's stdin from its connection into its program's pipe until either ends. A splice from a socket
/// into a pipe holds the pipe's lock while it waits for the socket (fs/splice.c, `splice_file_to_pipe` around
/// `do_splice_read`), and the program's close of its stdin, as its exit makes, takes that lock (fs/pipe.c,
/// `pipe_release`); so the copier waits in poll(2) for the host's bytes and for the program's end together, and
/// splices only what has come, its connection non-blocking, so no splice waits on the host holding the pipe. A
/// splice into a full pipe waits for room with the lock let go (`wait_for_space`). The pipe's write end reports
/// `POLLERR` once the program has no read end (`pipe_poll`): the copy then ends, and the stream's connection
/// closes with it, so the host sees its program let its stdin go.
fn feed(conn: &dyn Conn, to: &PipeWriter) -> io::Result<()> {
    conn.set_nonblocking(true)?;
    let unit = rustix::pipe::fcntl_getpipe_size(to)?;
    loop {
        let (came, gone) = {
            let from = conn.as_fd();
            let mut ready = [
                PollFd::new(&from, PollFlags::IN),
                PollFd::new(to, PollFlags::empty()),
            ];
            match rustix::event::poll(&mut ready, None) {
                Ok(_) => {}
                Err(Errno::INTR) => continue,
                Err(e) => return Err(e.into()),
            }
            let [came, gone] = &ready;
            (came.revents(), gone.revents())
        };
        if !gone.is_empty() {
            return Ok(());
        }
        if came.is_empty() {
            continue;
        }
        match rustix::pipe::splice(
            conn.as_fd(),
            None,
            to,
            None,
            unit,
            rustix::pipe::SpliceFlags::MOVE,
        ) {
            // The host ended its stdin.
            Ok(0) => return Ok(()),
            Ok(_) | Err(Errno::INTR | Errno::AGAIN) => {}
            Err(e) => return Err(e.into()),
        }
    }
}

/// Moves a program's stdout or stderr from its pipe into the host's connection until the program's end of the
/// pipe is gone and the pipe drained. A splice from a pipe into a socket holds the pipe's lock while it waits for
/// the socket's room (fs/splice.c, `splice_to_socket` around `sock_sendmsg`), and the program's close of its
/// stream, as its exit makes, takes that lock (fs/pipe.c, `pipe_release`); so the copier waits in poll(2), for the
/// pipe's bytes or, once the connection took none, for the connection's room, and splices only what fits,
/// non-blocking both ways (the connection non-blocking, `SPLICE_F_NONBLOCK` for the pipe), so no splice waits
/// holding the pipe. A splice that would wait says which wait comes next: after the pipe's bytes came, the
/// connection is full; after the connection's room came, the pipe is empty.
fn drain(from: &PipeReader, conn: &dyn Conn) -> io::Result<()> {
    conn.set_nonblocking(true)?;
    let unit = rustix::pipe::fcntl_getpipe_size(from)?;
    // Whether the connection took none at the last splice: then the copier waits for its room.
    let mut full = false;
    loop {
        {
            let to = conn.as_fd();
            let mut ready = [if full {
                PollFd::new(&to, PollFlags::OUT)
            } else {
                PollFd::new(from, PollFlags::IN)
            }];
            match rustix::event::poll(&mut ready, None) {
                Ok(_) => {}
                Err(Errno::INTR) => continue,
                Err(e) => return Err(e.into()),
            }
        }
        match rustix::pipe::splice(
            from,
            None,
            conn.as_fd(),
            None,
            unit,
            rustix::pipe::SpliceFlags::MOVE | rustix::pipe::SpliceFlags::NONBLOCK,
        ) {
            // The program's end of the pipe is gone, and the pipe drained.
            Ok(0) => return Ok(()),
            Ok(_) => full = false,
            Err(Errno::AGAIN) => full = !full,
            Err(Errno::INTR) => {}
            Err(e) => return Err(e.into()),
        }
    }
}

/// The node's answers, each framed to the host, until the node's connection ends; then the host's ends.
fn answers(node: &OwnedFd, writer: &Mutex<hex_client::guest::DuplexOwner>) {
    let mut buf = vec![0u8; wire::MAX_MESSAGE];
    loop {
        let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(wire::MAX_FDS))];
        let mut control = RecvAncillaryBuffer::new(&mut space);
        let msg = match rustix::net::recvmsg(
            node,
            &mut [IoSliceMut::new(&mut buf)],
            &mut control,
            RecvFlags::CMSG_CLOEXEC | RecvFlags::TRUNC,
        ) {
            Ok(msg) => msg,
            Err(Errno::INTR) => continue,
            Err(_) => break,
        };
        // An answer carries no descriptors: a message with any isn't one, and the connection ends.
        let descriptors = control
            .drain()
            .any(|m| matches!(m, RecvAncillaryMessage::ScmRights(_)));
        if msg.bytes == 0 || descriptors || msg.bytes > buf.len() {
            break;
        }
        let framed = buf
            .get(..msg.bytes)
            .and_then(|bytes| stream::frame(bytes, wire::MAX_MESSAGE).ok());
        let Some(framed) = framed else { break };
        let Ok(mut w) = writer.lock() else { break };
        if w.write_all(&framed).is_err() {
            break;
        }
    }
    if let Ok(w) = writer.lock() {
        let _ = w.shutdown(Shutdown::Both);
    }
}

/// Sends one message to the node with `fds`.
fn send(node: &OwnedFd, bytes: &[u8], fds: &[OwnedFd]) -> Result<(), Gone> {
    let borrowed: Vec<BorrowedFd<'_>> = fds.iter().map(AsFd::as_fd).collect();
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(wire::MAX_FDS))];
    let mut control = SendAncillaryBuffer::new(&mut space);
    if !borrowed.is_empty() && !control.push(SendAncillaryMessage::ScmRights(&borrowed)) {
        return Err(Gone);
    }
    loop {
        match rustix::net::sendmsg(
            node,
            &[IoSlice::new(bytes)],
            &mut control,
            SendFlags::NOSIGNAL,
        ) {
            Ok(_) => return Ok(()),
            Err(Errno::INTR) => continue,
            Err(_) => return Err(Gone),
        }
    }
}

pub mod files;

#[cfg(test)]
mod tests;
