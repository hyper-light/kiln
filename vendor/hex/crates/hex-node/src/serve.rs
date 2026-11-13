//! The node agent's local socket (decision 3 of docs/design/runtime-architecture.md): the `Runtime`
//! seam served over a local socket to callers that don't link Hex, in [`hex_core::wire`]'s format, and
//! the node's network namespace counts ([`Host::netns`]). An exec's stdio comes in
//! with its request, and a program's end goes out as a message after its exec's answer.
//!
//! This is the one core for every OS (native plan step 8): the protocol, the bounds, the disconnect rule
//! and the workers, written once over an [`Os`], each OS's event sources and local socket. Linux's is
//! [`linux::Linux`]: a `SOCK_SEQPACKET` socket (unix(7)), descriptors passed by `SCM_RIGHTS`, and one epoll
//! set over the listener, the connections and the programs' pidfds. macOS and Windows have no
//! `SOCK_SEQPACKET` local socket (macOS 26.2 refuses it, EPROTONOSUPPORT, measured on studio-3,
//! 2026-11-07; Windows' AF_UNIX is `SOCK_STREAM` alone), so theirs deliver whole messages framed over a
//! stream ([`hex_core::stream`]).
//!
//! One thread does every connection's I/O and watches every exec'd program's end through the OS's event
//! set; it never blocks but in [`Os::wait`]. The runtime's calls, which may block (a delete waits for its
//! box to empty), run on a pool of workers, which answer their calls themselves. A run (decision 7) is
//! answered as an exec is: its program's end comes once its node has removed its box, so an end its watch
//! reports before the runtime can tell it is waited for on a worker, never on the I/O thread.
//!
//! Each resource has a bound (CLAUDE.md §2), refused past by type, naming it (§3):
//! - connections ([`Limits::connections`]): one more is answered `Exhausted` and closed;
//! - a message's size ([`Limits::message_bytes`]): a larger one is answered `Invalid`, naming it;
//! - requests in flight on the node's connections together ([`Limits::in_flight`]): one more is answered
//!   `Exhausted`;
//! - programs the node's connections watch together ([`Limits::programs`]), and given answers' ends whose
//!   client hasn't yet said it took them (`Request::Taken`): an exec past them is answered `Exhausted`.
//!
//! Each is a budget for the node, never split among connections (the lead, 2026-11-06): the node's clients are
//! its own control plane, so no fairness between them is owed. Each is its platform's own ([`limits`]). Of each
//! budget the node keeps [`KEPT`] for every connection it admits, which is what a hello tells a client in its
//! limits, so a client within what it was told is never refused; a connection the node can't keep that for is
//! refused as it comes, naming the budget. A hello tells the budgets beside;
//! - answers waiting for room in a connection's socket: while any wait, its requests aren't read, so a
//!   client that doesn't read its answers stops being read rather than growing the agent's memory.
//!   By construction they number at most `in_flight + programs + 1`: an answer for each call in
//!   flight (a list's parts go one at a time), an end for each program, and the one refusal read
//!   before reading stopped;
//! - workers (the platform's threads, [`Platform::threads`]): one for every call the connections' in-flight
//!   bounds admit, where the kernel's thread limit allows; a call past them waits in the queue those
//!   bounds hold.
//!
//! The disconnect rule (decision 3): when a connection closes, every program its execs started that it
//! still watches is killed by its own runtime (`Proc::kill`), reaped, and counted in its box ([`Host::abandoned`]),
//! which inspect reports; none is left running unwatched. A program the client released (it dropped
//! the `Proc`) runs on, as a dropped in-process `Proc` does. A request the agent has read is carried
//! out whether or not its client stays to take the answer; one it hadn't read is let go with the
//! connection.

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use hex_api::{BoxId, BoxSpec, Exit, Proc, Program, Refusal, Runtime, Stdio};
use hex_core::netns;
use hex_core::patience::Patience;
use hex_core::wire::{self, Answer, Call, Header, Hello, Received, Request, Streams};

pub use hex_core::wire::Limits;

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(unix)]
mod unix;
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
mod unsupported;
#[cfg(windows)]
pub mod windows;

/// This OS's event sources and local socket.
#[cfg(target_os = "linux")]
pub type Native = linux::Linux;
#[cfg(target_os = "macos")]
pub type Native = macos::MacOs;
#[cfg(windows)]
pub type Native = windows::Windows;
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub type Native = unsupported::Unsupported;

/// What the node's platform lets it hold, read as the node starts: what its socket's bounds are ([`limits`]), and its
/// workers. Linux's node reads it with its sizes ([`crate::sizes::Sizes::platform`]), macOS's from its kernel
/// ([`macos::platform`]); none is picked (Ada's rule, 2026-11-06).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Platform {
    /// The most descriptors the node may hold.
    pub descriptors: usize,
    /// The most threads the node may run.
    pub threads: usize,
    /// The most programs the node can watch at once.
    pub programs: usize,
}

/// The node's bounds on its socket ([`Server::bind`]'s `limits`), each a budget for the node and each its platform's
/// own (Ada's rule and the lead's call, 2026-11-06; one rule on every OS, the lead's call of 2026-11-09 that unified
/// the macOS line's admission with it); a hello tells its `in_flight` and `programs` as the node's budgets and
/// [`KEPT`] as each connection's limits:
/// - connections: the descriptors the node may hold, one each; the kernel refuses an accept past what remains of them
///   (`EMFILE`), and the socket pauses accepting until one closes;
/// - messages of [`wire::MAX_MESSAGE`] bytes, the largest the format sends;
/// - calls in flight: the threads the node may run, its workers' bound (`Server::bind`'s `workers`), so a call
///   admitted has a worker that can exist;
/// - programs watched: the programs the node can watch at once.
///
/// Each is a ceiling the platform sets, not the room left under it now: what else holds the platform's room, the
/// kernel refuses past as the node takes it (`EMFILE`, `EAGAIN`), typed, at no cost to an admission (the lead,
/// 2026-11-09, over reading the room at each admission, which cost every exec a listing of the node's descriptors).
pub fn limits(platform: &Platform) -> Limits {
    Limits {
        connections: platform.descriptors,
        message_bytes: wire::MAX_MESSAGE,
        in_flight: platform.threads,
        programs: platform.programs,
    }
}

/// What the node keeps for every connection it admits, of its calls in flight and of its programs watched: one of
/// each, the fewest by which a connection can do anything (the lead's ruling, 2026-11-06). A hello tells a client
/// this in its limits, each connection's own, so a client within it is never refused, however much of the budgets
/// other connections hold. It is less than the 64 calls
/// and 1,024 programs a hello told before 2026-11-06: those were per-connection caps, which the node no longer
/// keeps, and keeping 64 and 1,024 for every connection would cap the connections at the budgets over them (64 at
/// the supervisor's 65,536 pidfds). Past it, a connection's calls and programs share what the budgets leave.
pub const KEPT: usize = 1;

/// Whether `call` starts a program, which takes a place among the programs as it is read ([`Limits::programs`]): a
/// member handed over is one, its end told as a program's.
fn starts_program(call: &Call) -> bool {
    matches!(
        call,
        Call::Exec { .. } | Call::ExecStreams { .. } | Call::Run { .. } | Call::Member { .. }
    )
}

/// What [`Refusal::Exhausted`] names for each bound.
pub const CONNECTIONS: &str = "connections to the node agent's socket";
pub const IN_FLIGHT: &str = "requests in flight on the node agent's connections together";
pub const PROGRAMS: &str = "programs watched by the node agent's connections together";

/// Messages read from one connection before the others get a turn: a fairness bound, so one busy
/// client can't hold the I/O thread.
const READS_PER_WAKE: usize = 64;
/// Most events one wake takes; more come on the next.
pub const EVENTS: usize = 256;
/// How long a list's worker waits for its client to read the parts before the one it sends next: the slowest such wait
/// this process has seen (decision 9), a bound only while the reads come within it. Past it the run-out is told to the
/// census and the wait follows its client by its life alone, its connection, until the parts go or the connection
/// closes ([`Shared::drained`]). No record is read: `bench/competitive/probes/waits_census.md`'s `serve.reads` row is
/// evidence.
static READS: Patience = Patience::measuring("serve.reads");

/// The first token a connection or program takes: below it are the listener's and the wake's.
const FIRST_TOKEN: u64 = 2;

/// A forwarded call's answer, given once, from whichever thread it comes on.
pub type Reply = Box<dyn FnOnce(Result<Forwarded, Refusal>) + Send>;
/// A remote program's end, given once, from whichever thread it comes on: its exit, or why it can't be
/// known (its guest's connection ended first).
pub type Ended = Box<dyn FnOnce(Result<Exit, Refusal>) + Send>;
/// Whether a remote program's kill was sent, given once.
pub type Killed = Box<dyn FnOnce(Result<(), Refusal>) + Send>;

/// What a host does with a call ([`Host::forward`]).
pub enum Forward {
    /// Its answer comes later, through the reply it was given.
    Taken,
    /// Caller-owned guest connections must be opened before this exec can be sent.
    Offered(Offering),
    /// The host runs it itself, on the worker: the call and its exec's stdio, as they were given.
    Back(Box<(Call, Stdio)>),
    /// The host couldn't send it (its reply spent with the attempt): refused, saying why.
    Refused(Refusal),
}

/// A bounded program reservation whose guest streams will be opened by its caller (HEX-RT-17).
/// Dropping `ready` cancels the unsent exec and closes its dedicated control connection.
pub struct Offering {
    pub streams: wire::StreamOffer,
    pub ready: Box<dyn FnOnce() -> Result<(), Refusal> + Send>,
}

/// A forwarded call's answer.
pub enum Forwarded {
    /// Any call's answer but a started exec's: boxed, as an answer is many times a started exec's size.
    Answer(Box<Answer>),
    /// An exec's program started where the host isn't: its pid there, which names no process here, what
    /// kills it or lets it go, and the caller's ends of its streams (its guest's own connections, for those
    /// the exec carried or asked for), which go with its answer. Its end comes to the exec's [`Ended`].
    Started {
        pid: u32,
        program: Box<dyn Remote>,
        given: Option<hex_api::Given>,
    },
}

/// A program that runs where the node's host isn't (its guest): the OS can't watch it, so its end comes as
/// a message, and it is killed or let go by message too.
/// Neither gives the program's end before it returns: that comes later, as a message.
pub trait Remote: Send + Sync {
    /// KILL to it alone; `done` hears whether that was sent: later, or before this returns, on the
    /// caller's thread, when it can't be sent at all.
    fn kill(&self, done: Killed);
    /// No one waits for its end any longer: it runs on, as a released in-process program does.
    fn release(&self);
}

/// A host's own connection's OS handle, borrowed from it, for the core's event set: a file descriptor on
/// Unix, a Winsock socket on Windows.
#[cfg(unix)]
pub type Handle<'a> = std::os::fd::BorrowedFd<'a>;
#[cfg(windows)]
pub type Handle<'a> = std::os::windows::io::BorrowedSocket<'a>;
#[cfg(not(any(unix, windows)))]
pub type Handle<'a> = std::marker::PhantomData<&'a ()>;

/// A host's connection to a node behind it (its guest), watched in the core's one event set, so its answers
/// arrive as events on the core's thread and no thread waits in its reads.
pub trait Source: Send {
    /// Its OS handle, borrowed: open while the source is, and out of the event set before it drops.
    fn handle(&self) -> Handle<'_>;
    /// It is ready: reads what has come and writes what waits, never waiting; what to watch it for
    /// next. Runs on the core's thread, and its completions with it.
    fn ready(&mut self) -> Want;
}

/// What a [`Source`] wants watched next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Want {
    Read,
    /// Readable, and writable: bytes wait to be written.
    ReadWrite,
    /// Nothing: its connection has ended; it is let go.
    Done,
}

/// The core's loop as a host sees it ([`Host::attach`]).
pub trait Loop: Send + Sync {
    /// A token for a source to come, so what is made before it is added can name it ([`Loop::room`]).
    fn token(&self) -> Result<u64, Refusal>;
    /// Watches `source` under `token` from now, for reading.
    fn add(&self, token: u64, source: Box<dyn Source>) -> Result<(), Refusal>;
    /// Source `token` has bytes waiting to be written: watched for room too. Safe from any thread.
    fn room(&self, token: u64);
}

/// What the socket needs of the runtime it serves, beyond the seam.
pub trait Host: Runtime {
    /// CP-T7.4: the optional live placement driver, using the normal worker admission.
    fn schedule(
        &self,
        _command: hex_core::schedule::Command,
    ) -> Result<hex_core::schedule::Reply, Refusal> {
        Err(Refusal::Invalid {
            what: "scheduler",
            why: "this node has no configured placement driver".into(),
        })
    }

    /// Hands the host the core's loop, before the socket serves, so connections of its own (its guest's)
    /// are watched there ([`Source`]). By default it has none.
    fn attach(&self, lp: Arc<dyn Loop>) -> Result<(), Refusal> {
        let _ = lp;
        Ok(())
    }

    /// Takes `call` to answer through `reply` once its answer comes, or gives it back to run on the
    /// worker. A host takes the calls about boxes a node behind a connection holds (the node's guest):
    /// what must be done here first (staging a box's root) is done on the worker, then the request goes
    /// without waiting for its answer, so the worker is free and no other client's call waits on that
    /// node. An exec's program's end goes to `ended`; the caller's client takes a started program's streams' ends
    /// with its answer ([`Answer::Given`]). By default every call runs here.
    fn forward(&self, call: Call, stdio: Stdio, reply: Reply, ended: Ended) -> Forward {
        let _ = (reply, ended);
        Forward::Back(Box::new((call, stdio)))
    }

    /// Counts in box `id` a program exec'd in it that the node ended because the connection watching
    /// it closed (the disconnect rule): reported as [`hex_api::BoxInfo::abandoned`].
    fn abandoned(&self, id: &BoxId) -> Result<(), Refusal>;

    /// The node's network namespace counts (decision 4).
    fn netns(&self) -> Result<netns::Counts, Refusal>;
}

/// What a wake of the OS's event set says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// The listener has connections waiting.
    Listener,
    /// [`Os::wake`] was called.
    Wake,
    /// A connection or watched program, by its token: a connection readable (or its peer half-closed),
    /// writable, or hung up; a program's watch ready, which is its end.
    Ready {
        token: u64,
        readable: bool,
        writable: bool,
        hung_up: bool,
    },
}

/// What a connection is watched for: its requests, or room to send its waiting answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Interest {
    Read,
    Write,
}

/// What [`Os::accept`] found.
pub enum Accepted<C> {
    Conn(C),
    /// None waits.
    None,
    /// The agent is out of descriptors or handles: the caller waits in the backlog until a connection
    /// closes or a program ends, which resumes accepting.
    Spent,
    Failed(Refusal),
}

/// Who a connection's peer is.
pub enum Peer {
    /// This user, or root.
    Allowed,
    /// Anyone else, and why, for the refusal.
    Refused(String),
    /// Gone before it could be asked.
    Gone,
}

/// What [`Os::recv`] read: one whole message, as the client sent it.
pub enum Recv<P> {
    /// A message of `size` bytes, its first `got` in the buffer (fewer when it is past the buffer), and
    /// what came with it (`cut`: more came than a message takes, or came where none belongs).
    Message {
        size: usize,
        got: usize,
        passed: Vec<P>,
        cut: bool,
    },
    /// Nothing waits.
    WouldBlock,
    /// The client closed with answers unread: reported once, before what it sent, which is still there
    /// to read (Linux, net/unix/af_unix.c `unix_release_sock`).
    Reset,
    /// The connection failed.
    Gone,
}

/// What [`Os::send`] did with one whole message.
pub enum Sent {
    Sent,
    /// No room: try again once the connection is writable.
    WouldBlock,
    /// Its client is gone.
    Gone,
}

/// Why a started program couldn't be watched.
pub enum Unwatched {
    /// No handle to it could be opened: it runs on, unwatched, for the node's sweep.
    Runs(Refusal),
    /// A handle to it was opened but couldn't be watched, and the OS ended it through that handle.
    Killed(Refusal),
    /// It had ended and been reaped before it could be watched: a run's program, which its node reaps to remove
    /// its box (decision 7). Its end is its runtime's to tell, through its `Proc`.
    Gone,
}

/// One OS's event sources and local socket: everything the core does that isn't the same on every OS.
/// The event set is level-triggered: a connection watched for [`Interest::Read`] is reported while a
/// message waits or its client has gone, one watched for [`Interest::Write`] while it has room.
pub trait Os: Send + Sync + 'static {
    /// A connection's socket.
    type Conn: Send + Sync + 'static;
    /// A watched program: what its end is seen through and it is killed through, so a kill can't reach a
    /// process given its pid since.
    type Watch: Send + Sync + 'static;
    /// A descriptor or handle that came with a request: an exec's stdio.
    type Passed: Send + 'static;

    /// Waits for events, at most `timeout` (`None`: until one comes), appending them to `events`, at
    /// most [`EVENTS`]; an interrupted wait returns none.
    fn wait(&self, events: &mut Vec<Event>, timeout: Option<Duration>) -> Result<(), Refusal>;
    /// Makes the next (or current) [`Os::wait`] return [`Event::Wake`].
    fn wake(&self) -> Result<(), Refusal>;
    /// Takes one waiting connection.
    fn accept(&self) -> Accepted<Self::Conn>;
    /// Watches the listener (`on`) or stops: accepting is paused while descriptors are spent.
    fn accepting(&self, on: bool) -> Result<(), Refusal>;
    /// Stops watching the listener and the wake, for a stopping server.
    fn stop_listening(&self);
    /// Who `conn`'s peer is.
    fn peer(&self, conn: &Self::Conn) -> Peer;
    /// Starts watching `conn` under `token`.
    fn watch(&self, conn: &Self::Conn, token: u64, interest: Interest) -> Result<(), Refusal>;
    /// Changes what `conn` is watched for.
    fn rewatch(&self, conn: &Self::Conn, token: u64, interest: Interest) -> Result<(), Refusal>;
    /// Stops watching `conn`.
    fn unwatch(&self, conn: &Self::Conn);
    /// Reads one whole message into `buf` without blocking; interrupted reads are retried.
    fn recv(&self, conn: &Self::Conn, buf: &mut [u8]) -> Recv<Self::Passed>;
    /// Sends one whole message without blocking, or none of it.
    fn send(&self, conn: &Self::Conn, bytes: &[u8], fds: &[hex_api::End]) -> Sent;
    /// Shuts `conn` both ways: its client sees the end, and the event set reports it hung up.
    fn shutdown(&self, conn: &Self::Conn);
    /// Whether `conn`'s peer has gone: a read of nothing then is its end, not an empty message.
    fn hung_up(&self, conn: &Self::Conn) -> bool;
    /// Watches the program `pid` under `token` until it ends.
    fn watch_program(&self, pid: u32, token: u64) -> Result<Self::Watch, Unwatched>;
    /// Stops watching a program.
    fn unwatch_program(&self, watch: Self::Watch);
    /// A program's stdio from what its exec carried, in the order `streams` names them; anything that
    /// isn't a stream's right end is refused by name.
    fn stdio(&self, streams: Streams, passed: Vec<Self::Passed>) -> Result<Stdio, Refusal>;
    /// Watches a host's own connection ([`Source`]) under `token`, or changes its watch: reported
    /// readable while bytes wait or it has ended, and writable too with `write`.
    fn watch_source(&self, handle: Handle<'_>, token: u64, write: bool) -> Result<(), Refusal>;
    /// Stops watching a host's own connection.
    fn unwatch_source(&self, handle: Handle<'_>, token: u64);
    /// The node's links over QUIC ([`Call::Links`]): a node serving this OS's socket alone has no QUIC listener.
    fn links(&self) -> Result<Vec<wire::Link>, Refusal> {
        Err(Refusal::Invalid {
            what: "links",
            why: "this node serves no QUIC listener (`hex node --quic`)".into(),
        })
    }
}

impl<O: Os> Os for Arc<O> {
    type Conn = O::Conn;
    type Watch = O::Watch;
    type Passed = O::Passed;

    fn wait(&self, events: &mut Vec<Event>, timeout: Option<Duration>) -> Result<(), Refusal> {
        (**self).wait(events, timeout)
    }
    fn wake(&self) -> Result<(), Refusal> {
        (**self).wake()
    }
    fn accept(&self) -> Accepted<Self::Conn> {
        (**self).accept()
    }
    fn accepting(&self, on: bool) -> Result<(), Refusal> {
        (**self).accepting(on)
    }
    fn stop_listening(&self) {
        (**self).stop_listening();
    }
    fn peer(&self, conn: &Self::Conn) -> Peer {
        (**self).peer(conn)
    }
    fn watch(&self, conn: &Self::Conn, token: u64, interest: Interest) -> Result<(), Refusal> {
        (**self).watch(conn, token, interest)
    }
    fn rewatch(&self, conn: &Self::Conn, token: u64, interest: Interest) -> Result<(), Refusal> {
        (**self).rewatch(conn, token, interest)
    }
    fn unwatch(&self, conn: &Self::Conn) {
        (**self).unwatch(conn);
    }
    fn recv(&self, conn: &Self::Conn, buf: &mut [u8]) -> Recv<Self::Passed> {
        (**self).recv(conn, buf)
    }
    fn send(&self, conn: &Self::Conn, bytes: &[u8], fds: &[hex_api::End]) -> Sent {
        (**self).send(conn, bytes, fds)
    }
    fn shutdown(&self, conn: &Self::Conn) {
        (**self).shutdown(conn);
    }
    fn hung_up(&self, conn: &Self::Conn) -> bool {
        (**self).hung_up(conn)
    }
    fn watch_program(&self, pid: u32, token: u64) -> Result<Self::Watch, Unwatched> {
        (**self).watch_program(pid, token)
    }
    fn unwatch_program(&self, watch: Self::Watch) {
        (**self).unwatch_program(watch);
    }
    fn stdio(&self, streams: Streams, passed: Vec<Self::Passed>) -> Result<Stdio, Refusal> {
        (**self).stdio(streams, passed)
    }
    fn watch_source(&self, handle: Handle<'_>, token: u64, write: bool) -> Result<(), Refusal> {
        (**self).watch_source(handle, token, write)
    }
    fn unwatch_source(&self, handle: Handle<'_>, token: u64) {
        (**self).unwatch_source(handle, token)
    }
    fn links(&self) -> Result<Vec<wire::Link>, Refusal> {
        (**self).links()
    }
}

/// The socket, served until dropped.
pub struct Server<O: Os = Native> {
    shared: Arc<Shared<O>>,
    io: Option<JoinHandle<()>>,
}

struct Shared<O: Os> {
    os: O,
    host: Arc<dyn Host>,
    /// The host's classes, asked once: a hello is answered on the I/O thread, which never calls the
    /// host.
    classes: Vec<hex_api::Class>,
    limits: Limits,
    workers: usize,
    path: PathBuf,
    /// How long a list's next part waits for the parts before it to go: [`READS`], or a test's own.
    reads: &'static Patience,
    stopping: AtomicBool,
    /// Calls in flight on every connection together. With `idle_calls`, at most `limits.in_flight`: what is
    /// in flight and what is kept for the open connections that have none in flight never pass the budget.
    in_flight: AtomicUsize,
    /// Open connections with no call in flight, each with its [`KEPT`] call kept for it.
    idle_calls: AtomicUsize,
    /// Open connections watching no program, each with its [`KEPT`] program kept for it; changed under the table's
    /// lock, as the programs watched are.
    idle_programs: AtomicUsize,
    /// Execs under way holding a place among the programs watched, which with the table's watched programs are
    /// at most `limits.programs`; changed under the table's lock.
    reserving: AtomicUsize,
    /// Given answers' ends kept for programs no longer watched ([`Out::kept`]), on every connection together, each a
    /// place among the programs until its client says it took them; changed under the table's lock, as the programs
    /// watched are.
    kept: AtomicUsize,
    table: hex_wait::Mutex<Table<O>>,
    /// Signalled when a remote program's end is taken, for a stopping server that waits on them.
    remote_ended: hex_wait::Condvar,
    /// The host's own connections watched in the event set, by token ([`Source`]).
    sources: Mutex<BTreeMap<u64, Sourced>>,
    jobs: Mutex<Jobs<O>>,
    work: Condvar,
    /// Why the I/O thread stopped, if it failed.
    fault: Mutex<Option<String>>,
}

/// The connections and the programs they watch, by token. Locked before a connection's own state when
/// both are held.
struct Table<O: Os> {
    conns: BTreeMap<u64, Arc<Conn<O>>>,
    watched: BTreeMap<u64, Watched<O>>,
    next: u64,
    /// Accepting paused: the agent ran out of descriptors, so a closed connection or a reaped program
    /// resumes it.
    accept_paused: bool,
    /// Remote programs' ends that came before their start was registered, by token: at most one for each
    /// exec under way, which the connections' program bounds hold.
    early: BTreeMap<u64, Result<Exit, Refusal>>,
}

impl<O: Os> Default for Table<O> {
    fn default() -> Self {
        Self {
            conns: BTreeMap::new(),
            watched: BTreeMap::new(),
            next: FIRST_TOKEN,
            accept_paused: false,
            early: BTreeMap::new(),
        }
    }
}

impl<O: Os> Table<O> {
    fn token(&mut self) -> Result<u64, Refusal> {
        let token = self.next.max(FIRST_TOKEN);
        self.next = token
            .checked_add(1)
            .ok_or_else(|| Refusal::Broken("the socket's tokens ran out".to_owned()))?;
        Ok(token)
    }
}

struct Conn<O: Os> {
    token: u64,
    sock: O::Conn,
    out: hex_wait::Mutex<Out>,
    /// Signalled when its waiting answers are sent, or it closes.
    drained: hex_wait::Condvar,
}

/// The single admission owner shared by an offer, its guest answer and cancellation.
struct PendingOffer {
    ready: Box<dyn FnOnce() -> Result<(), Refusal> + Send>,
    completed: Arc<AtomicBool>,
}

struct Out {
    open: bool,
    in_flight: usize,
    /// The programs it watches, by their exec's id, with their watch's token.
    programs: BTreeMap<u64, u64>,
    /// Places taken by execs under way.
    reserved: usize,
    /// At most one per reserved program; no stream handle has been exported to another process.
    offered: BTreeMap<u64, PendingOffer>,
    /// Answers waiting for room in the socket, oldest first, each with the descriptors riding with it and, for a given
    /// answer, its exec's id, whose client is to say it took them.
    backlog: VecDeque<(Vec<u8>, Vec<hex_api::End>, Option<u64>)>,
    /// Its given answers whose client hasn't yet said it took their ends ([`Request::Taken`]), by their exec's id, each
    /// with the node's own copies of the ends once the answer has gone: kept from its send until the word, or until
    /// the connection closes, so none is ever in flight alone, as XNU's collector of descriptors in flight flushes a
    /// socket whose only reference is a message in flight (HEX-GUEST-41). Each holds its program's place among the
    /// programs until then ([`Out::places`]), so a client that never says it took them holds the node to no more
    /// descriptors than its places.
    given: BTreeMap<u64, Vec<hex_api::End>>,
    /// Of `given`, those whose programs it no longer watches: each a place of its own.
    kept: usize,
    /// Whether its requests are read ([`Interest::Read`]) or only its backlog written.
    reading: bool,
}

impl Out {
    /// Its places among the programs: those it watches, those its execs under way hold, and those its given answers'
    /// ends hold for programs it no longer watches.
    fn places(&self) -> usize {
        self.programs
            .len()
            .saturating_add(self.reserved)
            .saturating_add(self.kept)
    }

    /// A message has gone with `ends` riding as its descriptors: a given answer's ends (`given`, its exec's id) kept
    /// until its client says it took them, any other's let go.
    fn sent(&mut self, ends: Vec<hex_api::End>, given: Option<u64>) {
        if let Some(slot) = given.and_then(|exec| self.given.get_mut(&exec)) {
            *slot = ends;
        }
    }
}

/// A program a connection's exec started.
struct Watched<O: Os> {
    conn: Arc<Conn<O>>,
    /// Its exec's id, which its end is sent with.
    exec: u64,
    box_id: BoxId,
    run: Run<O>,
    /// Sent KILL because its connection closed: its end is counted in its box, not sent.
    killed: bool,
}

/// Where a watched program runs, and so what its end is seen through.
enum Run<O: Os> {
    /// Here: the OS's watch reports its end.
    Local {
        proc: Box<dyn Proc>,
        watch: Option<O::Watch>,
    },
    /// Where the host isn't (its guest): its end comes as a message.
    Remote(Arc<dyn Remote>),
}

/// A host's source in the event set: out of it while its `ready` runs, and whether room was asked for meanwhile.
#[derive(Default)]
struct Sourced {
    source: Option<Box<dyn Source>>,
    room: bool,
}

/// The core's loop as its host sees it: the server, held weakly, as the server holds the host.
struct CoreLoop<O: Os>(std::sync::Weak<Shared<O>>);

impl<O: Os> CoreLoop<O> {
    fn shared(&self) -> Result<Arc<Shared<O>>, Refusal> {
        self.0
            .upgrade()
            .ok_or_else(|| Refusal::Broken("the node agent's socket has stopped".to_owned()))
    }
}

impl<O: Os> Loop for CoreLoop<O> {
    fn token(&self) -> Result<u64, Refusal> {
        self.shared()?.table()?.token()
    }

    fn add(&self, token: u64, source: Box<dyn Source>) -> Result<(), Refusal> {
        let shared = self.shared()?;
        let mut sources = shared.sources.lock().map_err(poisoned)?;
        shared.os.watch_source(source.handle(), token, false)?;
        sources.insert(
            token,
            Sourced {
                source: Some(source),
                room: false,
            },
        );
        Ok(())
    }

    fn room(&self, token: u64) {
        let Ok(shared) = self.shared() else { return };
        let Ok(mut sources) = shared.sources.lock() else {
            return;
        };
        if let Some(entry) = sources.get_mut(&token) {
            match &entry.source {
                Some(source) => {
                    if shared
                        .os
                        .watch_source(source.handle(), token, true)
                        .is_err()
                    {
                        entry.room = true;
                    }
                }
                // Its ready runs now, and is watched again as it ends: for room too.
                None => entry.room = true,
            }
        }
    }
}

/// A program just started: the token its exec took, its box, its pid, and the caller's ends of the streams
/// its exec asked for, which go with its answer.
struct Start {
    token: u64,
    box_id: BoxId,
    pid: u32,
    given: Option<hex_api::Given>,
}

struct Jobs<O: Os> {
    queue: VecDeque<Job<O>>,
    workers: usize,
    idle: usize,
}

impl<O: Os> Default for Jobs<O> {
    fn default() -> Self {
        Self {
            queue: VecDeque::new(),
            workers: 0,
            idle: 0,
        }
    }
}

enum Job<O: Os> {
    Call {
        conn: Arc<Conn<O>>,
        header: Header,
        call: Box<Call>,
        passed: Vec<O::Passed>,
    },
    Abandoned(BoxId),
    /// A forwarded list's answer, sent in its parts.
    List {
        conn: Arc<Conn<O>>,
        header: Header,
        infos: Vec<hex_api::BoxInfo>,
    },
    /// A program here whose watch reported its end before its runtime could tell it: a run's, whose box its node
    /// is removing (decision 7). Its end is waited for on a worker, so the I/O thread waits on no removal.
    Collect(Box<Watched<O>>),
}

/// What a call that starts a program starts: a program in a box there is (an exec), or a `process` box made for
/// the program's life, with it (a run, decision 7).
enum Launch {
    Exec(BoxId, Program),
    Run(Box<BoxSpec>, Program),
}

impl Launch {
    /// The box the program runs in.
    fn box_id(&self) -> BoxId {
        match self {
            Self::Exec(id, _) => id.clone(),
            Self::Run(spec, _) => spec.id.clone(),
        }
    }
}

/// A member handed over through the node's wire ([`Shared::member`]), as the node holds it while its caller waits for
/// its end: told by the thread that waits for it unless its caller let it go first.
struct MemberEnd<O: Os> {
    shared: std::sync::Weak<Shared<O>>,
    token: u64,
    /// Its caller let it go: its end goes to no one. Taken for its end's telling and for its release alike, so a
    /// release either comes first, and nothing is told, or after, and clears what was told before its start was
    /// registered ([`Table::early`]).
    released: Mutex<bool>,
}

impl<O: Os> MemberEnd<O> {
    /// Its end, told as a remote program's ([`Shared::remote_ended`]) while its caller still waits for it.
    fn told(&self, ended: Result<Exit, Refusal>) {
        let Ok(released) = self.released.lock() else {
            return;
        };
        if *released {
            return;
        }
        if let Some(shared) = self.shared.upgrade() {
            let _ = shared.remote_ended(self.token, ended);
        }
    }
}

impl<O: Os> Remote for MemberEnd<O> {
    /// A member's own kill awaits Shards' agent-control endpoint (joint plan step 11), as the `Proc`
    /// `Runtime::member` gives refuses it in process: refused by type, its box's KILL or delete ending it with its VM.
    fn kill(&self, done: Killed) {
        done(Err(Refusal::Awaits {
            needs: hex_api::Upstream::MemberSignal,
        }));
    }

    /// No one waits for its end any longer: it runs on, its end told no one, and an end told before its start was
    /// registered no longer kept for it.
    fn release(&self) {
        let Ok(mut released) = self.released.lock() else {
            return;
        };
        *released = true;
        if let Some(shared) = self.shared.upgrade()
            && let Ok(mut table) = shared.table()
        {
            table.early.remove(&self.token);
        }
    }
}

/// A member's own ends ([`Shared::member`]): given for those `stdio` asks, joined to those it carries, each by a copy
/// on a thread of its own to the source's end (std's `io::copy`), its destination closed then so the far side reads
/// the end. A stream neither asked for nor carried is let go: the member's stdin then ends, and what it writes to the
/// other has no reader.
fn joined(own: hex_api::Given, stdio: Stdio) -> Result<Option<hex_api::Given>, Refusal> {
    stdio.check()?;
    let hex_api::Given {
        stdin,
        stdout,
        stderr,
    } = own;
    let mut given = hex_api::Given::default();
    match (stdio.ask.stdin, stdio.stdin) {
        (true, _) => given.stdin = stdin,
        (false, Some(carried)) => {
            if let Some(to) = stdin {
                join(
                    "hex-node member stdin",
                    carried,
                    hex_api::OwnedStream::from(to),
                )?;
            }
        }
        (false, None) => {}
    }
    match (stdio.ask.stdout, stdio.stdout) {
        (true, _) => given.stdout = stdout,
        (false, Some(carried)) => {
            if let Some(from) = stdout {
                join(
                    "hex-node member stdout",
                    hex_api::OwnedStream::from(from),
                    carried,
                )?;
            }
        }
        (false, None) => {}
    }
    match (stdio.ask.stderr, stdio.stderr) {
        (true, _) => given.stderr = stderr,
        (false, Some(carried)) => {
            if let Some(from) = stderr {
                join(
                    "hex-node member stderr",
                    hex_api::OwnedStream::from(from),
                    carried,
                )?;
            }
        }
        (false, None) => {}
    }
    if stdio.ask == hex_api::Ask::default() {
        Ok(None)
    } else {
        Ok(Some(given))
    }
}

/// Copies `from` to `to` on a thread named `name` until `from` ends or either fails, then lets both go.
fn join(
    name: &str,
    mut from: impl std::io::Read + Send + 'static,
    mut to: impl std::io::Write + Send + 'static,
) -> Result<(), Refusal> {
    std::thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || {
            let _ = std::io::copy(&mut from, &mut to);
        })
        .map(drop)
        .map_err(|e| Refusal::os("starting a thread that joins a member's stream", e))
}

fn poisoned<T>(_: std::sync::PoisonError<T>) -> Refusal {
    Refusal::Broken("a socket lock was poisoned".to_owned())
}

/// A wait on a socket lock that couldn't wait: the lock poisoned, or the OS refused it (macOS's kqueue).
fn wait_refused(why: hex_wait::Failed) -> Refusal {
    match why {
        hex_wait::Failed::Poisoned => Refusal::Broken("a socket lock was poisoned".to_owned()),
        hex_wait::Failed::Os(why) => Refusal::Os {
            op: "waiting on a socket lock",
            why: why.to_string(),
        },
    }
}

impl Server<Native> {
    /// Serves `host` on this OS's socket at `path`, readable and writable by this user alone, holding
    /// `limits` and running calls on at most `workers` threads. A socket left at `path` by an agent that
    /// has ended is replaced; one an agent still serves, or a file that isn't a socket, is refused.
    pub fn bind(
        path: &Path,
        host: Arc<dyn Host>,
        limits: Limits,
        workers: usize,
    ) -> Result<Self, Refusal> {
        check_limits(&limits, workers)?;
        let os = Native::bind(path)?;
        Self::with_os(os, host, limits, workers, path)
    }
}

impl<O: Os> Server<O> {
    /// Serves `host` over `os`, listening at `path`.
    pub fn with_os(
        os: O,
        host: Arc<dyn Host>,
        limits: Limits,
        workers: usize,
        path: &Path,
    ) -> Result<Self, Refusal> {
        Self::serving(os, host, limits, workers, path, &READS)
    }

    /// [`Server::with_os`], a list's next part waiting as `reads` has learned.
    fn serving(
        os: O,
        host: Arc<dyn Host>,
        limits: Limits,
        workers: usize,
        path: &Path,
        reads: &'static Patience,
    ) -> Result<Self, Refusal> {
        check_limits(&limits, workers)?;
        os.accepting(true)?;
        let shared = Arc::new(Shared {
            os,
            classes: host.classes(),
            host,
            limits,
            workers,
            path: path.to_owned(),
            reads,
            stopping: AtomicBool::new(false),
            in_flight: AtomicUsize::new(0),
            idle_calls: AtomicUsize::new(0),
            idle_programs: AtomicUsize::new(0),
            reserving: AtomicUsize::new(0),
            kept: AtomicUsize::new(0),
            table: hex_wait::Mutex::new(Table::default()),
            remote_ended: hex_wait::Condvar::new(),
            sources: Mutex::new(BTreeMap::new()),
            jobs: Mutex::new(Jobs::default()),
            work: Condvar::new(),
            fault: Mutex::new(None),
        });
        // The host's own connections join the event set before the socket serves.
        shared
            .host
            .attach(Arc::new(CoreLoop(Arc::downgrade(&shared))))?;
        let io = {
            let shared = Arc::clone(&shared);
            std::thread::Builder::new()
                .name("hex-socket".to_owned())
                .spawn(move || shared.run())
                .map_err(|e| Refusal::os("starting the socket's thread", e))?
        };
        Ok(Self {
            shared,
            io: Some(io),
        })
    }

    /// Where it listens.
    pub fn path(&self) -> &Path {
        &self.shared.path
    }

    /// Why its I/O thread stopped, if it failed: it then serves nothing more.
    pub fn fault(&self) -> Option<String> {
        self.shared.fault.lock().ok().and_then(|f| f.clone())
    }

    /// Serves until the I/O thread stops, which it does only when it fails; returns why.
    pub fn wait(&mut self) -> Option<String> {
        if let Some(io) = self.io.take() {
            let _ = io.join();
        }
        self.fault()
    }
}

impl<O: Os> Drop for Server<O> {
    /// Stops serving: the listener and every connection close, their programs are ended by the
    /// disconnect rule, and the socket's file is removed.
    fn drop(&mut self) {
        self.shared.stopping.store(true, Ordering::SeqCst);
        let _ = self.shared.os.wake();
        if let Some(io) = self.io.take() {
            let _ = io.join();
        }
        let _ = std::fs::remove_file(&self.shared.path);
    }
}

fn check_limits(limits: &Limits, workers: usize) -> Result<(), Refusal> {
    let invalid = |why: String| {
        Err(Refusal::Invalid {
            what: "socket limits",
            why,
        })
    };
    if limits.message_bytes < wire::HEADER_BYTES || limits.message_bytes > wire::MAX_MESSAGE {
        return invalid(format!(
            "a message bound of {} bytes is outside {}..={}",
            limits.message_bytes,
            wire::HEADER_BYTES,
            wire::MAX_MESSAGE
        ));
    }
    if [
        limits.connections,
        limits.in_flight,
        limits.programs,
        workers,
    ]
    .contains(&0)
    {
        return invalid(
            "a bound of 0 connections, requests, programs or workers serves nothing".to_owned(),
        );
    }
    Ok(())
}

impl<O: Os> Shared<O> {
    fn table(&self) -> Result<hex_wait::MutexGuard<'_, Table<O>>, Refusal> {
        self.table.lock().map_err(poisoned)
    }

    /// The I/O thread: until the server stops, or fails (every connection then closes, so no client
    /// waits on a server that is gone).
    fn run(self: &Arc<Self>) {
        let mut buf = vec![0u8; self.limits.message_bytes];
        let mut events = Vec::with_capacity(EVENTS);
        let outcome = loop {
            events.clear();
            if let Err(e) = self.os.wait(&mut events, None) {
                break Err(e);
            }
            if self.stopping.load(Ordering::SeqCst) {
                break Ok(());
            }
            let mut failed = None;
            for event in &events {
                if let Err(e) = self.event(*event, &mut buf) {
                    failed.get_or_insert(e);
                }
            }
            if let Some(e) = failed {
                break Err(e);
            }
        };
        if let Err(e) = &outcome
            && let Ok(mut fault) = self.fault.lock()
        {
            *fault = Some(e.to_string());
        }
        self.stop();
    }

    fn event(self: &Arc<Self>, event: Event, buf: &mut [u8]) -> Result<(), Refusal> {
        match event {
            Event::Listener => self.accept(),
            Event::Wake => Ok(()),
            Event::Ready {
                token,
                readable,
                writable,
                hung_up,
            } => {
                let (conn, watched) = {
                    let table = self.table()?;
                    (
                        table.conns.get(&token).cloned(),
                        table.watched.contains_key(&token),
                    )
                };
                if let Some(conn) = conn {
                    self.connection(&conn, readable, writable, hung_up, buf)
                } else if watched {
                    self.ended(token)
                } else {
                    // A host's own connection, or one removed since the event was queued.
                    self.source(token)
                }
            }
        }
    }

    /// A host's own connection is ready: its `ready` runs here, out of the table of sources, so a completion
    /// it runs may ask for room ([`Loop::room`]); then it is watched as it says, or let go.
    fn source(&self, token: u64) -> Result<(), Refusal> {
        let taken = self
            .sources
            .lock()
            .map_err(poisoned)?
            .get_mut(&token)
            .and_then(|s| s.source.take());
        let Some(mut source) = taken else {
            return Ok(());
        };
        let want = source.ready();
        let mut sources = self.sources.lock().map_err(poisoned)?;
        match want {
            Want::Done => {
                self.os.unwatch_source(source.handle(), token);
                sources.remove(&token);
                drop(source);
                Ok(())
            }
            Want::Read | Want::ReadWrite => {
                let entry = sources.entry(token).or_default();
                let write = want == Want::ReadWrite || std::mem::take(&mut entry.room);
                let watched = self.os.watch_source(source.handle(), token, write);
                entry.source = Some(source);
                watched
            }
        }
    }

    /// Takes every connection waiting, refusing past the connection bound and any caller that isn't
    /// this user (or root).
    fn accept(self: &Arc<Self>) -> Result<(), Refusal> {
        loop {
            match self.os.accept() {
                Accepted::Conn(sock) => self.admit(sock)?,
                Accepted::None => return Ok(()),
                // Out of descriptors: the caller waits in the backlog until a connection closes or a
                // program ends, which resumes accepting.
                Accepted::Spent => {
                    let mut table = self.table()?;
                    table.accept_paused = true;
                    return self.os.accepting(false);
                }
                Accepted::Failed(e) => return Err(e),
            }
        }
    }

    fn admit(self: &Arc<Self>, sock: O::Conn) -> Result<(), Refusal> {
        match self.os.peer(&sock) {
            Peer::Allowed => {}
            Peer::Gone => return Ok(()),
            Peer::Refused(why) => {
                self.refuse(
                    &sock,
                    Refusal::Invalid {
                        what: "caller",
                        why,
                    },
                );
                return Ok(());
            }
        }
        let mut table = self.table()?;
        if table.conns.len() >= self.limits.connections {
            drop(table);
            self.refuse(
                &sock,
                Refusal::Exhausted {
                    what: CONNECTIONS,
                    bound: self.limits.connections,
                },
            );
            return Ok(());
        }
        // A connection is admitted only if what it is kept fits in both budgets beside what is used and kept now.
        let calls = self
            .in_flight
            .load(Ordering::Acquire)
            .saturating_add(self.idle_calls.load(Ordering::Acquire))
            .saturating_add(KEPT);
        let programs = table
            .watched
            .len()
            .saturating_add(self.reserving.load(Ordering::Acquire))
            .saturating_add(self.idle_programs.load(Ordering::Acquire))
            .saturating_add(self.kept.load(Ordering::Acquire))
            .saturating_add(KEPT);
        let spent = if calls > self.limits.in_flight {
            Some((IN_FLIGHT, self.limits.in_flight))
        } else if programs > self.limits.programs {
            Some((PROGRAMS, self.limits.programs))
        } else {
            None
        };
        if let Some((what, bound)) = spent {
            drop(table);
            self.refuse(&sock, Refusal::Exhausted { what, bound });
            return Ok(());
        }
        let token = table.token()?;
        if let Err(e) = self.os.watch(&sock, token, Interest::Read) {
            // This connection can't be watched (the OS's watches are spent): refused, and the ones
            // held go on being served.
            drop(table);
            self.refuse(&sock, e);
            return Ok(());
        }
        table.conns.insert(
            token,
            Arc::new(Conn {
                token,
                sock,
                out: hex_wait::Mutex::new(Out {
                    open: true,
                    in_flight: 0,
                    programs: BTreeMap::new(),
                    reserved: 0,
                    offered: BTreeMap::new(),
                    backlog: VecDeque::new(),
                    given: BTreeMap::new(),
                    kept: 0,
                    reading: true,
                }),
                drained: hex_wait::Condvar::new(),
            }),
        );
        self.idle_calls.fetch_add(1, Ordering::AcqRel);
        self.idle_programs.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    /// Answers a connection that won't be served with why, as id 0 (its client hasn't asked anything
    /// yet), then lets it go.
    fn refuse(&self, sock: &O::Conn, refusal: Refusal) {
        if let Ok(bytes) = wire::answer(0, &Answer::Refused(refusal), wire::MAX_MESSAGE) {
            let _ = self.os.send(sock, &bytes, &[]);
        }
        self.os.shutdown(sock);
    }

    fn connection(
        self: &Arc<Self>,
        conn: &Arc<Conn<O>>,
        readable: bool,
        writable: bool,
        hung_up: bool,
        buf: &mut [u8],
    ) -> Result<(), Refusal> {
        // Closed by its client: what it sent before it went is read and carried out (a release among
        // it lets its program go), then the connection closes. Its answers can't be taken.
        if writable && !hung_up {
            self.flush(conn)?;
        }
        let ended = if hung_up || readable {
            self.read(conn, buf, hung_up)?
        } else {
            false
        };
        if ended { self.close(conn) } else { Ok(()) }
    }

    /// Reads a connection's requests until none wait, its answers back up, or it has had its turn;
    /// or, once its client has gone (`gone`), every request it left. Whether the connection ends: its client
    /// has gone, or it sent a message of another wire.
    fn read(
        self: &Arc<Self>,
        conn: &Arc<Conn<O>>,
        buf: &mut [u8],
        mut gone: bool,
    ) -> Result<bool, Refusal> {
        if gone {
            conn.out.lock().map_err(poisoned)?.backlog.clear();
        }
        let mut turn = 0usize;
        let mut reset = false;
        loop {
            if !gone {
                if turn >= READS_PER_WAKE || !conn.out.lock().map_err(poisoned)?.reading {
                    return Ok(false);
                }
                turn = turn.saturating_add(1);
            }
            let (size, got, passed, cut) = match self.os.recv(&conn.sock, buf) {
                Recv::Message {
                    size,
                    got,
                    passed,
                    cut,
                } => (size, got, passed, cut),
                Recv::WouldBlock => return Ok(gone),
                Recv::Reset if !reset => {
                    reset = true;
                    gone = true;
                    conn.out.lock().map_err(poisoned)?.backlog.clear();
                    continue;
                }
                Recv::Reset | Recv::Gone => return Ok(true),
            };
            // A read of nothing is the client's end, unless it sent an empty message on purpose,
            // which is refused as one.
            if size == 0 && passed.is_empty() && (gone || self.os.hung_up(&conn.sock)) {
                return Ok(true);
            }
            let bytes = buf.get(..got.min(size).min(buf.len())).unwrap_or_default();
            let received = Received {
                bytes,
                size,
                descriptors: passed.len(),
                descriptors_cut: cut,
            };
            if self.request(conn, &received, passed)? {
                return Ok(true);
            }
        }
    }

    /// This node's hello: its build, the classes it runs, what it keeps for each connection ([`KEPT`]) and its
    /// budgets over them all.
    fn hello(&self) -> Hello {
        Hello {
            build: wire::build(),
            classes: self.classes.clone(),
            limits: Limits {
                in_flight: KEPT,
                programs: KEPT,
                ..self.limits
            },
            budgets: wire::Budgets {
                in_flight: self.limits.in_flight,
                programs: self.limits.programs,
            },
        }
    }

    /// Carries out a request a connection sent. Whether the connection ends with it: one of another wire ends it.
    fn request(
        self: &Arc<Self>,
        conn: &Arc<Conn<O>>,
        msg: &Received<'_>,
        passed: Vec<O::Passed>,
    ) -> Result<bool, Refusal> {
        let carried = match wire::read_request(msg, self.limits.message_bytes) {
            // A client of another wire is refused at its hello: a message of another wire, its hello first, is
            // answered with this node's hello, whose head (the header, then the build) every build reads, so the
            // client refuses it naming both wires and both builds; then the connection ends, nothing more of it read.
            Err(wire::Bad {
                id,
                fault: wire::Fault::Wire { .. },
            }) => {
                drop(passed);
                self.answer(conn, id, &Answer::Hello(self.hello()))?;
                return Ok(true);
            }
            Err(bad) => {
                drop(passed);
                self.answer(conn, bad.id, &Answer::Refused(bad.fault.into()))
            }
            Ok((h, Request::Hello { .. })) => self.answer(conn, h.id, &Answer::Hello(self.hello())),
            Ok((h, Request::Release)) => self.release(conn, h.id),
            Ok((h, Request::Taken)) => self.taken(conn, h.id),
            Ok((h, Request::StreamsReady)) => self.streams_ready(conn, h),
            Ok((header, Request::Call(call))) => {
                let admitted = {
                    let mut out = conn.out.lock().map_err(poisoned)?;
                    let admitted = if out.in_flight < KEPT {
                        // Its kept call: always there, since what is kept and in flight never pass the budget.
                        if out.open {
                            self.idle_calls.fetch_sub(1, Ordering::AcqRel);
                        }
                        self.in_flight.fetch_add(1, Ordering::AcqRel);
                        true
                    } else {
                        let kept = self.idle_calls.load(Ordering::Acquire);
                        self.in_flight
                            .try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                                n.checked_add(1)
                                    .filter(|&m| m.saturating_add(kept) <= self.limits.in_flight)
                            })
                            .is_ok()
                    };
                    if admitted {
                        out.in_flight = out.in_flight.saturating_add(1);
                    }
                    admitted
                };
                if !admitted {
                    drop(passed);
                    self.answer(
                        conn,
                        header.id,
                        &Answer::Refused(Refusal::Exhausted {
                            what: IN_FLIGHT,
                            bound: self.limits.in_flight,
                        }),
                    )
                } else if starts_program(&call) && !self.reserve(conn)? {
                    // A call that starts a program takes its place among the programs as it is read, so the streams
                    // riding it are held only within a place, which the node counts among its own (the lead,
                    // 2026-11-11): past the budget it is refused here, its streams closed before the answer goes.
                    drop(passed);
                    self.done(conn)?;
                    self.answer(
                        conn,
                        header.id,
                        &Answer::Refused(Refusal::Exhausted {
                            what: PROGRAMS,
                            bound: self.limits.programs,
                        }),
                    )
                } else {
                    self.enqueue(Job::Call {
                        conn: Arc::clone(conn),
                        header,
                        call: Box::new(call),
                        passed,
                    })
                }
            }
        };
        carried.map(|()| false)
    }

    /// Sends one answer on a connection, or keeps it until there is room; an answer that can't be
    /// written is refused to its request in its place.
    fn answer(&self, conn: &Conn<O>, id: u64, answer: &Answer) -> Result<(), Refusal> {
        let bytes = wire::answer(id, answer, self.limits.message_bytes)
            .or_else(|fault| {
                let refused = Answer::Refused(Refusal::Broken(format!(
                    "the node agent's answer couldn't be written: {fault}"
                )));
                wire::answer(id, &refused, self.limits.message_bytes)
            })
            .map_err(|e| Refusal::Broken(format!("a refusal couldn't be written: {e}")))?;
        self.send(conn, bytes, Vec::new(), None, false)
    }

    /// Sends a given answer with the caller's ends of its streams riding as its descriptors, the node's own copies
    /// kept once it has gone until its client says it took them ([`Out::given`]).
    fn answer_given(
        &self,
        conn: &Conn<O>,
        header: Header,
        pid: u32,
        given: hex_api::Given,
    ) -> Result<(), Refusal> {
        let streams = Streams {
            stdin: given.stdin.is_some(),
            stdout: given.stdout.is_some(),
            stderr: given.stderr.is_some(),
        };
        let fds: Vec<hex_api::End> = [given.stdin, given.stdout, given.stderr]
            .into_iter()
            .flatten()
            .collect();
        let answer = Answer::Given {
            pid,
            given: streams,
        };
        match wire::answer(header.id, &answer, self.limits.message_bytes) {
            Ok(bytes) => self.send(conn, bytes, fds, Some(header.id), false),
            Err(fault) => self.answer(
                conn,
                header.id,
                &Answer::Refused(Refusal::Broken(format!(
                    "the node agent's answer couldn't be written: {fault}"
                ))),
            ),
        }
    }

    /// Queues `bytes` after a connection's waiting answers, or sends them now when none wait, `fds` riding as its
    /// descriptors: a given answer's (`given`, its exec's id) kept once it has gone until its client says it took
    /// them ([`Out::given`]). With an `after_read` (a list's next part), first waits for the waiting ones to go
    /// ([`Shared::drained`]). Sending on a closed connection does nothing: its client is gone.
    fn send(
        &self,
        conn: &Conn<O>,
        bytes: Vec<u8>,
        fds: Vec<hex_api::End>,
        given: Option<u64>,
        after_read: bool,
    ) -> Result<(), Refusal> {
        let mut out = conn.out.lock().map_err(poisoned)?;
        if after_read {
            out = self.drained(conn, out)?;
        }
        if !out.open {
            return Ok(());
        }
        if let Some(exec) = given {
            out.given.insert(exec, Vec::new());
        }
        if out.backlog.is_empty() {
            match self.os.send(&conn.sock, &bytes, &fds) {
                Sent::Sent => {
                    out.sent(fds, given);
                    return Ok(());
                }
                // Its client is gone, and the I/O thread sees the hang-up and closes it.
                Sent::Gone => return Ok(()),
                Sent::WouldBlock => {}
            }
        }
        out.backlog.push_back((bytes, fds, given));
        if out.reading {
            out.reading = false;
            self.os
                .rewatch(&conn.sock, conn.token, Interest::Write)
                .map_err(|e| Refusal::os("waiting for room in a connection", e))?;
        }
        Ok(())
    }

    /// Waits for a connection's waiting answers to go, before a list's next part, as decision 9 waits (the person's
    /// ruling of 2026-11-13) on the server's reads' wait ([`Shared::reads`], [`READS`]):
    /// - **Within W***, the slowest such wait this process has seen, the answers going end it.
    /// - **Past W*** the run-out is told to the census, no sample, and the wait follows its client by its life alone,
    ///   as at every local site (docs/research/runtime/learned-waits.md): the answers going, or the connection's close,
    ///   ends it, however long that takes.
    /// - **With no W*** (before the first such wait, and from a run-out to the next answer) it follows the client from
    ///   its start.
    /// - **The connection's close** ends it at any time: the part then goes to no one.
    ///
    /// Its answer, however long it took, is the sample W* rises to.
    fn drained<'a>(
        &self,
        conn: &'a Conn<O>,
        mut out: hex_wait::MutexGuard<'a, Out>,
    ) -> Result<hex_wait::MutexGuard<'a, Out>, Refusal> {
        if !out.open || out.backlog.is_empty() {
            return Ok(out);
        }
        let began = Instant::now();
        let mut bound = self.reads.deadline();
        while out.open && !out.backlog.is_empty() {
            let now = began.elapsed();
            if let Some(within) = bound
                && now >= within
            {
                // Past W*: no sample (Karn's point), the run-out told once, and the wait goes on.
                self.reads.ran_out(within);
                bound = None;
            }
            out = match bound {
                None => conn.drained.wait(out).map_err(wait_refused)?,
                Some(within) => {
                    conn.drained
                        .wait_timeout(out, within.saturating_sub(now))
                        .map_err(wait_refused)?
                        .0
                }
            };
        }
        if out.open {
            self.reads.answered(began.elapsed());
        }
        Ok(out)
    }

    /// Writes a connection's waiting answers while there is room; once all are out its requests are
    /// read again.
    fn flush(&self, conn: &Conn<O>) -> Result<(), Refusal> {
        let mut out = conn.out.lock().map_err(poisoned)?;
        while let Some((front, fds, _)) = out.backlog.front() {
            match self.os.send(&conn.sock, front, fds) {
                Sent::Sent => {
                    if let Some((_, fds, given)) = out.backlog.pop_front() {
                        out.sent(fds, given);
                    }
                }
                Sent::WouldBlock => return Ok(()),
                Sent::Gone => {
                    // Its client is gone: the hang-up closes it.
                    out.backlog.clear();
                    break;
                }
            }
        }
        if out.open && !out.reading {
            out.reading = true;
            self.os
                .rewatch(&conn.sock, conn.token, Interest::Read)
                .map_err(|e| Refusal::os("reading a connection again", e))?;
        }
        conn.drained.notify_all();
        Ok(())
    }

    /// Closes a connection and applies the disconnect rule to the programs it still watches.
    fn close(&self, conn: &Conn<O>) -> Result<(), Refusal> {
        let mut table = self.table()?;
        let (programs, offered): (Vec<u64>, _) = {
            let mut out = conn.out.lock().map_err(poisoned)?;
            if !out.open {
                return Ok(());
            }
            out.open = false;
            // What was kept for it is let go with it.
            if out.in_flight == 0 {
                self.idle_calls.fetch_sub(1, Ordering::AcqRel);
            }
            if out.places() == 0 {
                self.idle_programs.fetch_sub(1, Ordering::AcqRel);
            }
            out.backlog.clear();
            // Its given ends go with it, and the places those of programs it no longer watches held.
            self.kept.fetch_sub(out.kept, Ordering::AcqRel);
            out.kept = 0;
            out.given.clear();
            let offered = std::mem::take(&mut out.offered);
            for pending in offered.values() {
                if !pending.completed.swap(true, Ordering::AcqRel) {
                    self.reserving.fetch_sub(1, Ordering::AcqRel);
                    out.reserved = out.reserved.saturating_sub(1);
                    self.call_done(&mut out);
                }
            }
            (out.programs.values().copied().collect(), offered)
        };
        conn.drained.notify_all();
        self.os.unwatch(&conn.sock);
        self.os.shutdown(&conn.sock);
        table.conns.remove(&conn.token);
        for token in programs {
            if let Some(w) = table.watched.get_mut(&token) {
                self.end(w);
            }
        }
        self.resume_accepting(&mut table)?;
        drop(table);
        drop(offered);
        Ok(())
    }

    /// The caller opened its named guest streams; commit its already-admitted exec without waiting here.
    fn streams_ready(&self, conn: &Conn<O>, header: Header) -> Result<(), Refusal> {
        let pending = conn
            .out
            .lock()
            .map_err(poisoned)?
            .offered
            .remove(&header.id);
        let Some(pending) = pending else {
            // A terminal guest refusal may already have cancelled this exact offer and answered the exec.
            return Ok(());
        };
        if pending.completed.load(Ordering::Acquire) {
            return Ok(());
        }
        match (pending.ready)() {
            Ok(()) => Ok(()),
            Err(why) => {
                let owns = {
                    let _out = conn.out.lock().map_err(poisoned)?;
                    !pending.completed.swap(true, Ordering::AcqRel)
                };
                if owns {
                    self.unreserve(conn)?;
                    self.done(conn)?;
                    self.answer(conn, header.id, &Answer::Refused(why))?;
                }
                Ok(())
            }
        }
    }

    /// The client no longer waits for exec `exec`'s program: it runs on, unwatched by the socket,
    /// and the node's sweep reaps it once it ends, as with a dropped in-process `Proc`; a remote one is let
    /// go by its host.
    fn release(&self, conn: &Conn<O>, exec: u64) -> Result<(), Refusal> {
        let offered = {
            let mut out = conn.out.lock().map_err(poisoned)?;
            out.offered.remove(&exec).map(|pending| {
                let owns = !pending.completed.swap(true, Ordering::AcqRel);
                (pending, owns)
            })
        };
        if let Some((pending, owns)) = offered {
            if owns {
                self.unreserve(conn)?;
                self.done(conn)?;
            }
            drop(pending);
            return Ok(());
        }
        let remote = {
            let mut table = self.table()?;
            let token = {
                let mut out = conn.out.lock().map_err(poisoned)?;
                let token = out.programs.remove(&exec);
                if token.is_some() {
                    self.program_gone(&mut out, Some(exec));
                }
                token
            };
            let remote = match token.and_then(|t| table.watched.remove(&t)).map(|w| w.run) {
                Some(Run::Local {
                    watch: Some(watch), ..
                }) => {
                    self.os.unwatch_program(watch);
                    None
                }
                Some(Run::Remote(program)) => Some(program),
                Some(Run::Local { watch: None, .. }) | None => None,
            };
            self.resume_accepting(&mut table)?;
            remote
        };
        // Outside the table, so a host that answers at once can't wait on it.
        if let Some(program) = remote {
            program.release();
        }
        Ok(())
    }

    /// KILL to the program exec `exec` of `conn` started: by the program's own runtime, its held `Proc::kill`,
    /// which never reaches a process given its pid since and ends the program as its class does (Windows'
    /// with KILL's job code, so it reads `Signal(9)`; macOS's telling its box's watcher first), or by its host
    /// for a remote one, answered once the host says it was sent. Its end is then sent as any end is; one that
    /// has ended (or was released) is no longer the connection's, and there is nothing to kill.
    fn kill(
        self: &Arc<Self>,
        conn: &Arc<Conn<O>>,
        header: Header,
        exec: u64,
    ) -> Result<(), Refusal> {
        let remote = {
            let mut table = self.table()?;
            let token = conn
                .out
                .lock()
                .map_err(poisoned)?
                .programs
                .get(&exec)
                .copied();
            match token
                .and_then(|t| table.watched.get_mut(&t))
                .map(|w| &mut w.run)
            {
                Some(Run::Local { proc, .. }) => proc.kill().map(|()| None),
                Some(Run::Remote(program)) => Ok(Some(Arc::clone(program))),
                None => Ok(None),
            }
        };
        let program = match remote {
            Ok(Some(program)) => program,
            local => {
                self.done(conn)?;
                let answer = match local {
                    Ok(_) => Answer::Done,
                    Err(why) => Answer::Refused(why),
                };
                return self.answer(conn, header.id, &answer);
            }
        };
        let shared = Arc::clone(self);
        let conn = Arc::clone(conn);
        program.kill(Box::new(move |sent| {
            let answer = match sent {
                Ok(()) => Answer::Done,
                Err(why) => Answer::Refused(why),
            };
            let _ = shared
                .done(&conn)
                .and_then(|()| shared.answer(&conn, header.id, &answer));
        }));
        Ok(())
    }

    /// A watched program's watch is ready: it has ended. Reaped, and its end sent to its connection,
    /// or counted in its box when the connection closed (the disconnect rule).
    fn ended(self: &Arc<Self>, token: u64) -> Result<(), Refusal> {
        let (mut w, open) = {
            let mut table = self.table()?;
            let Some(taken) = self.take_watched(&mut table, token)? else {
                return Ok(());
            };
            self.resume_accepting(&mut table)?;
            taken
        };
        let exit = match &mut w.run {
            Run::Local { proc, .. } => match proc.try_wait() {
                Ok(Some(exit)) => Some(exit),
                // Ready but not yet told: a run's program, its box being removed (decision 7).
                Ok(None) => None,
                // Reaped elsewhere in this process: how it ended is no longer known.
                Err(_) => Some(Exit::Unknown),
            },
            // A remote program's end comes as its host's message, never as this OS's event.
            Run::Remote(_) => None,
        };
        let Some(exit) = exit else {
            if let Run::Local { watch, .. } = &mut w.run {
                // Ended, but its end not yet told: a run's, whose box its node is removing, which tells the end once
                // the box is gone. Off the event set, its end is waited for on a worker.
                if let Some(watch) = watch.take() {
                    self.os.unwatch_program(watch);
                }
                return self.enqueue(Job::Collect(Box::new(w)));
            }
            let mut table = self.table()?;
            {
                let mut out = w.conn.out.lock().map_err(poisoned)?;
                if out.open {
                    if out.places() == 0 {
                        self.idle_programs.fetch_sub(1, Ordering::AcqRel);
                    }
                    if out.given.contains_key(&w.exec) {
                        // Watched again: its place is its program's, no longer its given ends'.
                        out.kept = out.kept.saturating_sub(1);
                        self.kept.fetch_sub(1, Ordering::AcqRel);
                    }
                    out.programs.insert(w.exec, token);
                }
            }
            table.watched.insert(token, w);
            return Ok(());
        };
        if let Run::Local { watch, .. } = &mut w.run
            && let Some(watch) = watch.take()
        {
            self.os.unwatch_program(watch);
        }
        self.finish(w, open, exit)
    }

    /// Takes watched program `token` from the table (its lock held) and from its connection's programs, the
    /// connection's count going with the node's under the same lock, so the two never disagree; with whether its
    /// connection is open.
    fn take_watched(
        &self,
        table: &mut Table<O>,
        token: u64,
    ) -> Result<Option<(Watched<O>, bool)>, Refusal> {
        let Some(w) = table.watched.remove(&token) else {
            return Ok(None);
        };
        let open = {
            let mut out = w.conn.out.lock().map_err(poisoned)?;
            if out.programs.remove(&w.exec).is_some() {
                self.program_gone(&mut out, Some(w.exec));
            }
            out.open
        };
        Ok(Some((w, open)))
    }

    /// One of `out`'s programs (`exec`'s) is no longer watched, or an exec under way let its place go (`None`), the
    /// table's lock held: a program whose given ends its client hasn't said it took keeps its place for them
    /// ([`Out::given`]), and a connection left with no place has its kept program kept again.
    fn program_gone(&self, out: &mut Out, exec: Option<u64>) {
        if exec.is_some_and(|exec| out.given.contains_key(&exec)) {
            out.kept = out.kept.saturating_add(1);
            self.kept.fetch_add(1, Ordering::AcqRel);
        } else if out.open && out.places() == 0 {
            self.idle_programs.fetch_add(1, Ordering::AcqRel);
        }
    }

    /// Its client took exec `exec`'s given ends ([`Request::Taken`]): the node lets its own copies go, so the streams'
    /// ends are the client's and its program's alone (its stdin ends with the client's last copy of its end, its stdout
    /// has no reader once the client lets its own go), and the place they held past their program's watch is free.
    /// The descriptors they held are free too: accepting resumes if it had paused for want of them.
    fn taken(&self, conn: &Conn<O>, exec: u64) -> Result<(), Refusal> {
        let mut table = self.table()?;
        let ends = {
            let mut out = conn.out.lock().map_err(poisoned)?;
            let ends = out.given.remove(&exec);
            if ends.is_some() && !out.programs.contains_key(&exec) {
                out.kept = out.kept.saturating_sub(1);
                self.kept.fetch_sub(1, Ordering::AcqRel);
                if out.open && out.places() == 0 {
                    self.idle_programs.fetch_add(1, Ordering::AcqRel);
                }
            }
            ends
        };
        if ends.is_some() {
            drop(ends);
            self.resume_accepting(&mut table)?;
        }
        Ok(())
    }

    /// A remote program's end came, on whichever thread its host reads it on: sent or counted as any end
    /// is. Its end unknowable (its host's connection to it ended first) reads as a program reaped
    /// elsewhere's. One that comes before its start is registered is kept for it.
    fn remote_ended(
        self: &Arc<Self>,
        token: u64,
        end: Result<Exit, Refusal>,
    ) -> Result<(), Refusal> {
        let (w, open) = {
            let mut table = self.table()?;
            let Some(taken) = self.take_watched(&mut table, token)? else {
                table.early.insert(token, end);
                return Ok(());
            };
            self.resume_accepting(&mut table)?;
            taken
        };
        self.remote_ended.notify_all();
        self.finish(w, open, end.unwrap_or(Exit::Unknown))
    }

    /// A program's end, taken from the table and its connection's programs ([`Shared::take_watched`]): sent to its
    /// connection while it is `open`, or counted in its box when the connection closed or the disconnect rule
    /// killed it.
    fn finish(self: &Arc<Self>, w: Watched<O>, open: bool, exit: Exit) -> Result<(), Refusal> {
        if open && !w.killed {
            self.answer(&w.conn, w.exec, &Answer::Exited(exit))
        } else {
            self.enqueue(Job::Abandoned(w.box_id))
        }
    }

    fn resume_accepting(&self, table: &mut Table<O>) -> Result<(), Refusal> {
        if table.accept_paused {
            table.accept_paused = false;
            self.os
                .accepting(true)
                .map_err(|e| Refusal::os("resuming the node agent's socket", e))?;
        }
        Ok(())
    }

    /// Ends a program whose connection closed: by its own runtime's kill (its held `Proc::kill`, which can't
    /// reach a process given its pid since and tells its box's bookkeeping first, as macOS's watcher needs to
    /// tell it from its cap's), or by its host for a remote one; its end, when it comes, is counted in its
    /// box. A kill its runtime refuses, or its host can't send, leaves the program to end of itself, counted
    /// then all the same.
    fn end(&self, w: &mut Watched<O>) {
        match &mut w.run {
            Run::Local { proc, .. } => {
                let _ = proc.kill();
            }
            Run::Remote(program) => program.kill(Box::new(|_| {})),
        }
        w.killed = true;
    }

    /// Hands a job to a worker, starting one when none is idle and fewer than the bound run.
    fn enqueue(self: &Arc<Self>, job: Job<O>) -> Result<(), Refusal> {
        let mut jobs = self.jobs.lock().map_err(poisoned)?;
        jobs.queue.push_back(job);
        // Each job queued has a worker of its own: an idle worker counts once, for the job it is woken for, so two
        // jobs queued before it wakes start a second (the waits census's run 5: a call left behind another's).
        if jobs.queue.len() > jobs.idle && jobs.workers < self.workers {
            let shared = Arc::clone(self);
            match std::thread::Builder::new()
                .name("hex-socket-worker".to_owned())
                .spawn(move || shared.work())
            {
                Ok(_) => jobs.workers = jobs.workers.saturating_add(1),
                // With no worker at all the job would never run: a call is refused to its request
                // instead, and a count made here.
                Err(e) if jobs.workers == 0 => {
                    let job = jobs.queue.pop_back();
                    drop(jobs);
                    return match job {
                        Some(Job::Call {
                            conn, header, call, ..
                        }) => {
                            if starts_program(&call) {
                                self.unreserve(&conn)?;
                            }
                            self.done(&conn)?;
                            self.answer(
                                &conn,
                                header.id,
                                &Answer::Refused(Refusal::os("starting a worker for a call", e)),
                            )
                        }
                        Some(Job::Abandoned(id)) => {
                            let _ = self.host.abandoned(&id);
                            Ok(())
                        }
                        Some(Job::List {
                            conn,
                            header,
                            infos,
                        }) => self.list(&conn, header, &infos),
                        // No worker to wait in: its end isn't known, and is sent so, or counted in its box as
                        // `finish` counts one.
                        Some(Job::Collect(w)) => {
                            let open = w.conn.out.lock().is_ok_and(|out| out.open);
                            if open && !w.killed {
                                self.answer(&w.conn, w.exec, &Answer::Exited(Exit::Unknown))
                            } else {
                                let _ = self.host.abandoned(&w.box_id);
                                Ok(())
                            }
                        }
                        None => Ok(()),
                    };
                }
                // Another worker takes it once free.
                Err(_) => {}
            }
        }
        self.work.notify_one();
        Ok(())
    }

    /// A worker: runs jobs until the server stops and the queue is empty.
    fn work(self: Arc<Self>) {
        loop {
            let job = {
                let Ok(mut jobs) = self.jobs.lock() else {
                    return;
                };
                loop {
                    if let Some(job) = jobs.queue.pop_front() {
                        break job;
                    }
                    if self.stopping.load(Ordering::SeqCst) {
                        jobs.workers = jobs.workers.saturating_sub(1);
                        return;
                    }
                    jobs.idle = jobs.idle.saturating_add(1);
                    let Ok(woken) = self.work.wait(jobs) else {
                        return;
                    };
                    jobs = woken;
                    jobs.idle = jobs.idle.saturating_sub(1);
                }
            };
            match job {
                Job::Call {
                    conn,
                    header,
                    call,
                    passed,
                } => {
                    let _ = self.call(&conn, header, *call, passed);
                }
                // Its box gone since (deleted by another caller) is nothing to count it in; any other
                // failure the node keeps and reports itself (`Node::abandoned`).
                Job::Abandoned(id) => {
                    let _ = self.host.abandoned(&id);
                }
                Job::List {
                    conn,
                    header,
                    infos,
                } => {
                    let _ = self.list(&conn, header, &infos);
                }
                Job::Collect(mut w) => {
                    let exit = match &mut w.run {
                        Run::Local { proc, .. } => {
                            // Told once its box is gone: waited for as a killed box's emptying is, past its W* by the
                            // program's life alone (decision 9), the removal waiting on that emptying.
                            crate::emptying::end_of_killed(&mut **proc, Instant::now())
                                .unwrap_or(Exit::Unknown)
                        }
                        Run::Remote(_) => Exit::Unknown,
                    };
                    let open = w.conn.out.lock().is_ok_and(|out| out.open);
                    let _ = self.finish(*w, open, exit);
                }
            }
        }
    }

    /// Runs one call and answers it, or hands it to the host to answer later ([`Host::forward`]); its place
    /// in flight is given back before the answer goes, so a client that sends its next call on reading
    /// the answer never meets the bound. An exec took its place among the connection's programs as it was read
    /// ([`starts_program`]), and takes its stdio and its token first, so a forwarded one is held to the same bound.
    fn call(
        self: &Arc<Self>,
        conn: &Arc<Conn<O>>,
        header: Header,
        call: Call,
        passed: Vec<O::Passed>,
    ) -> Result<(), Refusal> {
        let reply = |answer: Answer| -> Result<(), Refusal> {
            self.done(conn)?;
            self.answer(conn, header.id, &answer)
        };
        let starts = match &call {
            Call::Exec { id, streams, .. }
            | Call::ExecStreams { id, streams, .. }
            | Call::Member { id, streams, .. } => Some((id, *streams)),
            Call::Run { spec, streams, .. } => Some((&spec.id, *streams)),
            _ => None,
        };
        // A call that starts a program took its place among the programs as it was read.
        let (stdio, program) = match starts {
            Some((id, streams)) => {
                let mut stdio = match self.os.stdio(streams, passed) {
                    Ok(stdio) => stdio,
                    Err(r) => {
                        self.unreserve(conn)?;
                        return reply(Answer::Refused(r));
                    }
                };
                if let Call::ExecStreams { ask, .. }
                | Call::Run { ask, .. }
                | Call::Member { ask, .. } = &call
                {
                    stdio.ask = hex_api::Ask {
                        stdin: ask.stdin,
                        stdout: ask.stdout,
                        stderr: ask.stderr,
                    };
                }
                (stdio, Some((self.table()?.token()?, id.clone())))
            }
            None => (Stdio::null(), None),
        };
        let ended: Ended = match &program {
            Some((token, _)) => {
                let (shared, token) = (Arc::clone(self), *token);
                Box::new(move |end| {
                    let _ = shared.remote_ended(token, end);
                })
            }
            None => Box::new(|_| {}),
        };
        let completed = Arc::new(AtomicBool::new(false));
        let forwarded: Reply = {
            let (shared, conn, program, completed) = (
                Arc::clone(self),
                Arc::clone(conn),
                program.clone(),
                Arc::clone(&completed),
            );
            Box::new(move |got| {
                let _ = shared.complete_forwarding(&conn, header, program, &completed, got);
            })
        };
        let call = match self.host.forward(call, stdio, forwarded, ended) {
            Forward::Taken => return Ok(()),
            Forward::Offered(offering) => {
                if program.is_none() {
                    return reply(Answer::Refused(Refusal::Broken(
                        "a non-exec offered guest streams".to_owned(),
                    )));
                }
                let encoded = wire::answer(
                    header.id,
                    &Answer::Offered(offering.streams),
                    self.limits.message_bytes,
                );
                let mut out = conn.out.lock().map_err(poisoned)?;
                if completed.load(Ordering::Acquire) {
                    return Ok(());
                }
                if !out.open || encoded.is_err() {
                    completed.store(true, Ordering::Release);
                    drop(out);
                    self.unreserve(conn)?;
                    self.done(conn)?;
                    return match encoded {
                        Err(why) => self.answer(conn, header.id, &Answer::Refused(why.into())),
                        Ok(_) => Ok(()),
                    };
                }
                if out.offered.contains_key(&header.id) {
                    completed.store(true, Ordering::Release);
                    drop(out);
                    self.unreserve(conn)?;
                    self.done(conn)?;
                    return self.answer(
                        conn,
                        header.id,
                        &Answer::Refused(Refusal::Invalid {
                            what: "stream offer",
                            why: "its exec id already has an active preparation".to_owned(),
                        }),
                    );
                }
                out.offered.insert(
                    header.id,
                    PendingOffer {
                        ready: offering.ready,
                        completed,
                    },
                );
                drop(out);
                let bytes = encoded.map_err(Refusal::from)?;
                if let Err(why) = self.send(conn, bytes, Vec::new(), None, false) {
                    self.close(conn)?;
                    return Err(why);
                }
                return Ok(());
            }
            Forward::Refused(why) => {
                let owns = {
                    let _out = conn.out.lock().map_err(poisoned)?;
                    !completed.swap(true, Ordering::AcqRel)
                };
                if !owns {
                    return Ok(());
                }
                if program.is_some() {
                    self.unreserve(conn)?;
                }
                return reply(Answer::Refused(why));
            }
            Forward::Back(back) => match (*back, program) {
                ((Call::Exec { id, program, .. }, stdio), Some((token, _))) => {
                    return self.exec(conn, header, token, Launch::Exec(id, program), stdio);
                }
                ((Call::ExecStreams { id, program, .. }, stdio), Some((token, _))) => {
                    return self.exec(conn, header, token, Launch::Exec(id, program), stdio);
                }
                ((Call::Run { spec, program, .. }, stdio), Some((token, _))) => {
                    return self.exec(conn, header, token, Launch::Run(spec, program), stdio);
                }
                ((Call::Member { id, member, .. }, stdio), Some((token, _))) => {
                    return self.member(conn, header, token, id, member, stdio);
                }
                ((call, _), _) => call,
            },
        };
        let host = &self.host;
        let answer = match call {
            Call::Exec { .. }
            | Call::ExecStreams { .. }
            | Call::Run { .. }
            | Call::Member { .. } => Err(Refusal::Broken(
                "an exec came back from its host without its place".to_owned(),
            )),
            Call::Kill { exec } => return self.kill(conn, header, exec),
            Call::List => {
                let listed = host.list();
                self.done(conn)?;
                return match listed {
                    Ok(infos) => self.list(conn, header, &infos),
                    Err(r) => self.answer(conn, header.id, &Answer::Refused(r)),
                };
            }
            Call::Schedule(command) => host
                .schedule(*command)
                .map(|reply| Answer::Scheduled(Box::new(reply))),
            Call::Capacity(class) => host.capacity(class).map(Answer::Capacity),
            Call::PhysicalReport => host.physical_report().map(Answer::PhysicalReport),
            Call::InitializeBacking(grant) => {
                let grant = triomphe::Arc::try_unwrap(grant).map_err(|grant| Refusal::Backing {
                    key: grant.key,
                    cause: hex_api::LifecycleCause::Uncertain,
                });
                grant
                    .and_then(|grant| host.initialize_backing(grant))
                    .map(Answer::BackingOffer)
            }
            Call::ReleaseBacking(key) => host.release_backing(key).map(Answer::BackingOffer),
            Call::BackingUsage(key) => host.backing_usage(&key).map(Answer::Usage),
            Call::Create(spec) => host.create(*spec).map(Answer::Info),
            Call::Start(id) => host.start(&id).map(|()| Answer::Done),
            Call::Signal { id, signal } => host.signal(&id, signal).map(|()| Answer::Done),
            Call::Inspect(id) => host.inspect(&id).map(Answer::Info),
            Call::Usage(id) => host.usage(&id).map(Answer::Usage),
            Call::Delete(id) => host.delete(&id).map(|()| Answer::Done),
            Call::Netns => host.netns().map(Answer::Netns),
            // The endpoint's links at one moment, in as many parts as they need.
            Call::Links => {
                let links = self.os.links();
                self.done(conn)?;
                return match links {
                    Ok(links) => {
                        let parts = wire::links(header.id, &links, self.limits.message_bytes);
                        self.parts(conn, header, "links", parts)
                    }
                    Err(r) => self.answer(conn, header.id, &Answer::Refused(r)),
                };
            }
        };
        reply(answer.unwrap_or_else(Answer::Refused))
    }

    /// Claim an exec's original admission exactly once, atomically with offer registration/removal.
    /// An error can arrive before the host returns its offer, while Ready runs, or after cancellation.
    fn complete_forwarding(
        self: &Arc<Self>,
        conn: &Arc<Conn<O>>,
        header: Header,
        program: Option<(u64, BoxId)>,
        completed: &AtomicBool,
        got: Result<Forwarded, Refusal>,
    ) -> Result<(), Refusal> {
        let pending = {
            let mut out = conn.out.lock().map_err(poisoned)?;
            if completed.swap(true, Ordering::AcqRel) {
                drop(out);
                if let Ok(Forwarded::Started { program, .. }) = got {
                    // A concurrently cancelled exec still obeys the disconnect rule.
                    program.kill(Box::new(|_| {}));
                }
                return Ok(());
            }
            let mine = out
                .offered
                .get(&header.id)
                .is_some_and(|pending| std::ptr::eq(&*pending.completed, completed));
            if mine {
                out.offered.remove(&header.id)
            } else {
                None
            }
        };
        drop(pending);
        self.forwarded(conn, header, program, got)
    }

    /// A forwarded call's answer, on whichever thread its host gives it: sent to its client, or, for a
    /// program started remotely, registered under the token its exec took and announced. A list's parts
    /// go from a worker, as they may wait on their client.
    fn forwarded(
        self: &Arc<Self>,
        conn: &Arc<Conn<O>>,
        header: Header,
        program: Option<(u64, BoxId)>,
        got: Result<Forwarded, Refusal>,
    ) -> Result<(), Refusal> {
        let answer = match (got, program) {
            (
                Ok(Forwarded::Started {
                    pid,
                    program,
                    given,
                }),
                Some((token, box_id)),
            ) => {
                let table = self.table()?;
                let start = Start {
                    token,
                    box_id,
                    pid,
                    given,
                };
                return self.started(table, conn, header, start, Run::Remote(Arc::from(program)));
            }
            (Ok(Forwarded::Started { program, .. }), None) => {
                program.release();
                Answer::Refused(Refusal::Broken(
                    "the node's host started a program for a call that isn't an exec".to_owned(),
                ))
            }
            (got, program) => {
                if let Some((token, _)) = program {
                    self.unreserve(conn)?;
                    self.table()?.early.remove(&token);
                }
                match got.map(|f| match f {
                    Forwarded::Answer(answer) => Ok(*answer),
                    started @ Forwarded::Started { .. } => Err(started),
                }) {
                    Ok(Ok(Answer::Boxes { infos, .. })) => {
                        self.done(conn)?;
                        return self.enqueue(Job::List {
                            conn: Arc::clone(conn),
                            header,
                            infos,
                        });
                    }
                    Ok(Ok(answer)) => answer,
                    Ok(Err(_started)) => Answer::Refused(Refusal::Broken(
                        "the node's host started a program twice for one exec".to_owned(),
                    )),
                    Err(why) => Answer::Refused(why),
                }
            }
        };
        self.done(conn)?;
        self.answer(conn, header.id, &answer)
    }

    fn done(&self, conn: &Conn<O>) -> Result<(), Refusal> {
        let mut out = conn.out.lock().map_err(poisoned)?;
        self.call_done(&mut out);
        Ok(())
    }

    /// One of `out`'s calls is done. A connection left with none has its kept call kept again, counted before the
    /// call leaves the node's count, so the two together never read past the budget.
    fn call_done(&self, out: &mut Out) {
        out.in_flight = out.in_flight.saturating_sub(1);
        if out.in_flight == 0 && out.open {
            self.idle_calls.fetch_add(1, Ordering::AcqRel);
        }
        let _ = self
            .in_flight
            .try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                Some(n.saturating_sub(1))
            });
    }

    /// Takes a place among the programs the node's connections watch, before a program starts, so none starts
    /// that can't be watched: under the table's lock, which every watch is added under; false when none is left.
    fn reserve(&self, conn: &Conn<O>) -> Result<bool, Refusal> {
        let table = self.table()?;
        let mut out = conn.out.lock().map_err(poisoned)?;
        let mine = out.places();
        if mine < KEPT {
            // Its kept program: always there, since what is kept and watched never pass the budget.
            if out.open {
                self.idle_programs.fetch_sub(1, Ordering::AcqRel);
            }
        } else {
            let held = table
                .watched
                .len()
                .saturating_add(self.reserving.load(Ordering::Acquire))
                .saturating_add(self.idle_programs.load(Ordering::Acquire))
                .saturating_add(self.kept.load(Ordering::Acquire));
            if held >= self.limits.programs {
                return Ok(false);
            }
        }
        self.reserving.fetch_add(1, Ordering::AcqRel);
        out.reserved = out.reserved.saturating_add(1);
        Ok(true)
    }

    fn unreserve(&self, conn: &Conn<O>) -> Result<(), Refusal> {
        let _table = self.table()?;
        self.reserving.fetch_sub(1, Ordering::AcqRel);
        let mut out = conn.out.lock().map_err(poisoned)?;
        out.reserved = out.reserved.saturating_sub(1);
        self.program_gone(&mut out, None);
        Ok(())
    }

    /// A list's answer, in as many parts as its boxes need, each sent once the one before has gone.
    fn list(
        &self,
        conn: &Conn<O>,
        header: Header,
        infos: &[hex_api::BoxInfo],
    ) -> Result<(), Refusal> {
        let parts = wire::boxes(header.id, infos, self.limits.message_bytes);
        self.parts(conn, header, "list", parts)
    }

    /// An answer in `parts` ([`wire::boxes`], [`wire::links`]), each sent once the one before has gone; one that
    /// couldn't be written is refused, naming `what`.
    fn parts(
        &self,
        conn: &Conn<O>,
        header: Header,
        what: &str,
        parts: Result<Vec<Vec<u8>>, wire::Fault>,
    ) -> Result<(), Refusal> {
        let parts = match parts {
            Ok(parts) => parts,
            Err(fault) => {
                return self.answer(
                    conn,
                    header.id,
                    &Answer::Refused(Refusal::Broken(format!(
                        "the node agent's {what} couldn't be written: {fault}"
                    ))),
                );
            }
        };
        for (n, part) in parts.into_iter().enumerate() {
            self.send(conn, part, Vec::new(), None, n > 0)?;
        }
        Ok(())
    }

    /// An exec, or a run, the host runs here: the program started on its stdio, then watched by the OS, announced,
    /// and its end sent after.
    fn exec(
        self: &Arc<Self>,
        conn: &Arc<Conn<O>>,
        header: Header,
        token: u64,
        launch: Launch,
        stdio: Stdio,
    ) -> Result<(), Refusal> {
        let reply = |answer: Answer| -> Result<(), Refusal> {
            self.done(conn)?;
            self.answer(conn, header.id, &answer)
        };
        let asked = stdio.ask != hex_api::Ask::default();
        let id = launch.box_id();
        let started = match launch {
            Launch::Exec(b, program) => self.host.exec(&b, program, stdio),
            Launch::Run(spec, program) => self.host.run(*spec, program, stdio),
        };
        let mut proc = match started {
            Ok(proc) => proc,
            Err(r) => {
                self.unreserve(conn)?;
                return reply(Answer::Refused(r));
            }
        };
        let given = if asked {
            match proc.streams() {
                Ok(given) => Some(given),
                Err(r) => {
                    let _ = proc.kill();
                    self.unreserve(conn)?;
                    return reply(Answer::Refused(r));
                }
            }
        } else {
            None
        };
        let pid = proc.pid();
        // A program another host runs (a VM guest's, its pid the guest's) is never watched or signalled here by its
        // pid, which may name no process of this host or an unrelated one: its host forwards its exec
        // ([`Host::forward`]), its end coming as that host's message. One that reaches here is ended by its own
        // means and refused.
        if !proc.on_this_host() {
            let _ = proc.kill();
            self.unreserve(conn)?;
            return reply(Answer::Refused(Refusal::Invalid {
                what: "a started program",
                why: format!(
                    "pid {pid} is another host's (a VM guest's), which the node never watches or signals: its \
                     host forwards its exec"
                ),
            }));
        }
        // Announced and watched under the table, so the I/O thread can't send its end before its
        // start: it looks the program up under the same lock.
        let table = self.table()?;
        let watch = match self.os.watch_program(pid, token) {
            Ok(watch) => watch,
            Err(Unwatched::Gone) => {
                drop(table);
                return self.gone(conn, header, pid, given, proc);
            }
            Err(Unwatched::Killed(why)) => {
                drop(table);
                // A killed process's end: waited for as a killed box's emptying is, its kind of wait, past its W* by
                // the program's life alone (decision 9).
                let _ = crate::emptying::end_of_killed(&mut *proc, Instant::now());
                self.unreserve(conn)?;
                return reply(Answer::Refused(why));
            }
            Err(Unwatched::Runs(why)) => {
                drop(table);
                self.unreserve(conn)?;
                return reply(Answer::Refused(why));
            }
        };
        let start = Start {
            token,
            box_id: id,
            pid,
            given,
        };
        let run = Run::Local {
            proc,
            watch: Some(watch),
        };
        self.started(table, conn, header, start, run)
    }

    /// Member `member` of box `id` handed to its caller ([`Call::Member`], joint plan step 17), from its runtime's
    /// `Runtime::member`: its own ends given with the answer for those the call asks ([`Answer::Given`]), or joined to
    /// those it carried (a QUIC client's lanes, [`joined`]). It is its guest's, with no pid here for an OS watch, so a
    /// thread of its own waits for its end and tells it as a remote program's ([`MemberEnd`]).
    fn member(
        self: &Arc<Self>,
        conn: &Arc<Conn<O>>,
        header: Header,
        token: u64,
        id: BoxId,
        member: hex_api::Member,
        stdio: Stdio,
    ) -> Result<(), Refusal> {
        let reply = |answer: Answer| -> Result<(), Refusal> {
            self.done(conn)?;
            self.answer(conn, header.id, &answer)
        };
        let mut proc = match self.host.member(&id, &member) {
            Ok(proc) => proc,
            Err(r) => {
                self.unreserve(conn)?;
                return reply(Answer::Refused(r));
            }
        };
        let given = match proc.streams().and_then(|own| joined(own, stdio)) {
            Ok(given) => given,
            Err(r) => {
                self.unreserve(conn)?;
                return reply(Answer::Refused(r));
            }
        };
        let end = Arc::new(MemberEnd {
            shared: Arc::downgrade(self),
            token,
            released: Mutex::new(false),
        });
        let telling = Arc::clone(&end);
        let waited = std::thread::Builder::new()
            .name("hex-node member end".to_owned())
            .spawn(move || {
                let ended = proc.wait_for(Duration::MAX).and_then(|ended| {
                    ended.ok_or_else(|| Refusal::Broken("a member's end came as none".to_owned()))
                });
                telling.told(ended);
            });
        if let Err(e) = waited {
            self.unreserve(conn)?;
            return reply(Answer::Refused(Refusal::os(
                "starting the thread that waits for a member's end",
                e,
            )));
        }
        let table = self.table()?;
        let start = Start {
            token,
            box_id: id,
            pid: 0,
            given,
        };
        self.started(table, conn, header, start, Run::Remote(end))
    }

    /// A program that had ended and been reaped before it could be watched: a run's, whose node reaps it to remove
    /// its box (decision 7). With nothing left to watch, its end, its `Proc`'s to tell once its box is gone, is waited
    /// for here on the worker, bounded by the node's learned wait for a box to empty, which the removal waits on; then
    /// its start and its end are sent, one after the other.
    fn gone(
        &self,
        conn: &Conn<O>,
        header: Header,
        pid: u32,
        given: Option<hex_api::Given>,
        mut proc: Box<dyn Proc>,
    ) -> Result<(), Refusal> {
        // Waited for as a killed box's emptying is, past its W* by the program's life alone (decision 9).
        let exit =
            crate::emptying::end_of_killed(&mut *proc, Instant::now()).unwrap_or(Exit::Unknown);
        self.unreserve(conn)?;
        self.done(conn)?;
        match given {
            Some(given) => self.answer_given(conn, header, pid, given)?,
            None => self.answer(conn, header.id, &Answer::Started { pid })?,
        }
        self.answer(conn, header.id, &Answer::Exited(exit))
    }

    /// A program started: watched from now under `table`, held since its watch began, its place among
    /// `conn`'s programs taken from its reservation, and announced; one whose connection closed while it
    /// started is ended by the disconnect rule. A remote end that came before is taken now.
    fn started(
        self: &Arc<Self>,
        mut table: hex_wait::MutexGuard<'_, Table<O>>,
        conn: &Arc<Conn<O>>,
        header: Header,
        start: Start,
        run: Run<O>,
    ) -> Result<(), Refusal> {
        let open = {
            self.reserving.fetch_sub(1, Ordering::AcqRel);
            let mut out = conn.out.lock().map_err(poisoned)?;
            out.reserved = out.reserved.saturating_sub(1);
            self.call_done(&mut out);
            if out.open {
                out.programs.insert(header.id, start.token);
            }
            out.open
        };
        let mut watched = Watched {
            conn: Arc::clone(conn),
            exec: header.id,
            box_id: start.box_id,
            run,
            killed: false,
        };
        if !open {
            // Its connection closed while it started: ended by the disconnect rule.
            self.end(&mut watched);
        }
        let early = table.early.remove(&start.token);
        table.watched.insert(start.token, watched);
        if open {
            match start.given {
                Some(given) => self.answer_given(conn, header, start.pid, given)?,
                None => self.answer(conn, header.id, &Answer::Started { pid: start.pid })?,
            }
        }
        drop(table);
        match early {
            Some(end) => self.remote_ended(start.token, end),
            None => Ok(()),
        }
    }

    /// The server stops: every connection closes, ending its programs, and the thread waits for them to be reaped and
    /// counted before the workers finish the queue and go: as a killed box's emptying is waited for, within its W*
    /// and past it, its run-out told once, by the programs' lives alone (decision 9).
    fn stop(self: &Arc<Self>) {
        // Nothing new comes in, and the stop's own wake no longer wakes the wait below.
        self.os.stop_listening();
        let conns: Vec<Arc<Conn<O>>> = match self.table() {
            Ok(table) => table.conns.values().cloned().collect(),
            Err(_) => Vec::new(),
        };
        // The programs the closes end are waited for as a killed box's processes are: the node's learned wait
        // for an emptying, from their kill.
        let killed = Instant::now();
        for conn in &conns {
            let _ = self.close(conn);
        }
        let mut events = Vec::with_capacity(EVENTS);
        let bound = crate::emptying::EMPTYING.deadline();
        let mut past = false;
        loop {
            let Ok(table) = self.table() else { break };
            if table.watched.is_empty() {
                break;
            }
            let mut left = if past {
                None
            } else {
                bound.map(|window| window.saturating_sub(killed.elapsed()))
            };
            if let (Some(window), Some(Duration::ZERO)) = (bound, left) {
                // Past W*: no sample (Karn's point), the run-out told once, and the drain goes on by the programs'
                // lives alone.
                crate::emptying::EMPTYING.ran_out(window);
                past = true;
                left = None;
            }
            let tokens: Vec<u64> = table
                .watched
                .iter()
                .filter(|(_, w)| matches!(w.run, Run::Local { .. }))
                .map(|(token, _)| *token)
                .collect();
            let sourced = self.sources.lock().is_ok_and(|s| !s.is_empty());
            if tokens.is_empty() && !sourced {
                // Only remote programs are left, whose ends come from their host's own thread.
                let waited = match left {
                    Some(left) => self.remote_ended.wait_timeout(table, left).map(drop),
                    None => self.remote_ended.wait(table).map(drop),
                };
                if waited.is_err() {
                    break;
                }
                continue;
            }
            drop(table);
            events.clear();
            if self.os.wait(&mut events, left).is_err() {
                break;
            }
            for event in &events {
                if let Event::Ready { token, .. } = event {
                    if tokens.contains(token) {
                        let _ = self.ended(*token);
                    } else {
                        // A host's own connection, on which its remote programs' ends come.
                        let _ = self.source(*token);
                    }
                }
            }
        }
        if let Ok(mut sources) = self.sources.lock() {
            for (token, entry) in std::mem::take(&mut *sources) {
                if let Some(source) = entry.source {
                    self.os.unwatch_source(source.handle(), token);
                }
            }
        }
        self.stopping.store(true, Ordering::SeqCst);
        if let Ok(_jobs) = self.jobs.lock() {
            self.work.notify_all();
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod linux_tests;
#[cfg(all(test, target_os = "macos"))]
mod macos_tests;
#[cfg(test)]
mod sim_tests;
#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod unix_tests;
#[cfg(all(test, windows))]
mod windows_tests;
