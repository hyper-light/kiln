//! What every client of a node agent does whatever carries its messages: a call waits for its answer by its
//! request's id, a program's end goes to its `Proc`, and the connection's end reaches every call and program still
//! waiting. A [`Transport`] moves the bytes: the agent's local `SOCK_SEQPACKET` socket on Linux (`linux.rs`), its
//! framed `SOCK_STREAM` socket on macOS or Windows (`macos.rs`, `windows.rs`), or a framed byte stream to the node agent in a Shards guest
//! (`guest.rs`).
//!
//! No thread of the client's reads the connection: whoever waits on it reads, one at a time, the reader's turn (a
//! call for its answer or for room to send in, a `Proc` for its program's end), handing each message to whoever it
//! is for and the turn to another still waiting as it goes, so an answer wakes the thread it answers and no thread
//! between (goal 16: the socket's per-call cost; decision 3's client, which compares this with a reading thread of the
//! client's own). In the event form a node's loop holds the turn for good and reads as its connection turns ready
//! ([`Core::begin`], the guest's `Pump`). Each message is sent whole under the connection's send lock, so a stream's
//! frames never interleave; one that finds no room waits for it reading what the agent sends while it holds the turn,
//! as the agent reads no request while its answers wait for room in the client's socket, else waits on the connection
//! for room and on a wake of its own, which the turn's holder writes as it hands the turn to it. The core waits on
//! every connection the same way (poll(2), Winsock's `WSAPoll`), through the handle its transport lends it.
//!
//! The word that a given answer's ends were taken ([`Request::Taken`]), which the agent waits for before it lets its
//! own copies go, is owed from the read that hands the answer on, before its taker wakes: queued ahead of every later
//! request, never sent from inside a read, so the reader never waits on itself, and never lost ([`Owed`]).

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags};

use hex_api::{
    BoxId, BoxInfo, BoxSpec, Class, Exit, Proc, Program, Refusal, Runtime, Signal, Stdio, Usage,
};
use hex_core::netns;
use hex_core::patience::Patience;
use hex_core::schedule::waits::WaitKind;
use hex_core::wire::{self, Answer, Call, Header, Limits, Received, Request, Streams};

use crate::MAX_LIST;
use crate::copy::{copy, copy_in};

/// What takes a call's answer in the event form ([`Core::begin`]), with its request's id (an exec's names
/// its program): run once, on the thread that reads the connection, after the client's lock is let go.
pub type Then = Box<dyn FnOnce(u64, Result<Answer, Refusal>) + Send>;
/// What takes an exec'd program's end in the event form: run once, as [`Then`] is.
pub type Ended = Box<dyn FnOnce(Result<Exit, Refusal>) + Send>;

/// The kinds whose answer no box's class times, in their table's order ([`Transport::answers`]): a hello, a list, the
/// network namespaces' counts; then the scheduler's operations in [`WaitKind`] order up to the person's release; then
/// a native physical report and the backing lifecycle's three; then the release (joint plan step 13). Waits only
/// append.
#[repr(usize)]
enum PlainKind {
    Hello,
    List,
    Netns,
}
impl PlainKind {
    const COUNT: usize = Self::Netns as usize + 1;
}

/// The scheduler's kinds laid out after the plain ones, those before the person's release: the release's wait came
/// later, so it appends after the backing lifecycle's.
const SCHEDULED: usize = WaitKind::Forget as usize;

/// The plain kinds' waits, scheduler's among them ([`PlainKind`]).
pub const KINDS: usize = PlainKind::COUNT + SCHEDULED + 5;

/// AC-T7.4.1: the kinds whose answer a box's class times, in their tables' order ([`Transport::class_answers`]): its
/// class's runtime does their work, a microVM's boot, its guest's relay, its teardown, orders of magnitude from a
/// process's or a container's, so each is learned per (kind, class), and a class with no record yet measures rather
/// than borrow another's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum BoxKind {
    Create,
    Start,
    Exec,
    Signal,
    Inspect,
    Usage,
    Delete,
    Kill,
    ExecStreams,
    Run,
    Capacity,
}

/// The box kinds' waits for one class ([`BoxKind`]).
pub const BOX_KINDS: usize = BoxKind::Capacity as usize + 1;

/// The classes a box is of ([`Class`]), a table of box kinds' waits for each, in [`by_class`]'s order.
pub const CLASSES: usize = 4;

/// `tables`' entry for `class`: process, oci, firecracker, shards, in that order.
pub(crate) fn by_class<W>(tables: &[W; CLASSES], class: Class) -> &W {
    let [process, oci, firecracker, shards] = tables;
    match class {
        Class::Process => process,
        Class::Oci => oci,
        Class::Firecracker => firecracker,
        Class::Shards => shards,
    }
}

/// One wait of a class's table ([`class_waits`]): `measuring`, or `(release, debug)`'s records as
/// [`Patience::by_profile`] takes them.
macro_rules! class_wait {
    ($name:expr, measuring) => {
        hex_core::patience::Patience::measuring($name)
    };
    ($name:expr, ($release:expr, $debug:expr)) => {
        hex_core::patience::Patience::by_profile($name, $release, $debug)
    };
}

/// One class's box-kind waits, in [`BoxKind`]'s order, each named `<prefix>.<kind>.<class>` (AC-T7.4.1: one census
/// name per (kind, class)); `measuring` makes every one of them so.
macro_rules! class_waits {
    ($prefix:literal, $class:literal, measuring) => {
        $crate::calls::class_waits!(
            $prefix,
            $class,
            [
                measuring, measuring, measuring, measuring, measuring, measuring, measuring,
                measuring, measuring, measuring, measuring
            ]
        )
    };
    (
        $prefix:literal,
        $class:literal,
        [$create:tt, $start:tt, $exec:tt, $signal:tt, $inspect:tt, $usage:tt, $delete:tt, $kill:tt,
            $exec_streams:tt, $run:tt, $capacity:tt $(,)?]
    ) => {
        [
            $crate::calls::class_wait!(concat!($prefix, ".create.", $class), $create),
            $crate::calls::class_wait!(concat!($prefix, ".start.", $class), $start),
            $crate::calls::class_wait!(concat!($prefix, ".exec.", $class), $exec),
            $crate::calls::class_wait!(concat!($prefix, ".signal.", $class), $signal),
            $crate::calls::class_wait!(concat!($prefix, ".inspect.", $class), $inspect),
            $crate::calls::class_wait!(concat!($prefix, ".usage.", $class), $usage),
            $crate::calls::class_wait!(concat!($prefix, ".delete.", $class), $delete),
            $crate::calls::class_wait!(concat!($prefix, ".kill.", $class), $kill),
            $crate::calls::class_wait!(concat!($prefix, ".exec_streams.", $class), $exec_streams),
            $crate::calls::class_wait!(concat!($prefix, ".run.", $class), $run),
            $crate::calls::class_wait!(concat!($prefix, ".capacity.", $class), $capacity),
        ]
    };
}
pub(crate) use {class_wait, class_waits};

/// The box kinds' waits of a transport with none recorded for any class (a test's): every one measuring.
static UNRECORDED: [[Patience; BOX_KINDS]; CLASSES] = [
    class_waits!("client.unrecorded", "process", measuring),
    class_waits!("client.unrecorded", "oci", measuring),
    class_waits!("client.unrecorded", "firecracker", measuring),
    class_waits!("client.unrecorded", "shards", measuring),
];

/// The wait of `request`'s plain kind in `waits`, a client's table of its plain kinds' learned waits (decision 9: a
/// client's call's wait for its answer and its send is learned per call kind, since a list waits on its parts, so a
/// fast hello's answer says nothing of its); `None` for a box kind ([`box_kind_of`]) and for one that has no answer
/// (a release).
pub(crate) fn kind_of<'a>(waits: &'a [Patience; KINDS], request: &Request) -> Option<&'a Patience> {
    let n = match request {
        Request::Hello { .. } => PlainKind::Hello as usize,
        Request::Call(call) => match call {
            Call::List => PlainKind::List as usize,
            Call::Netns => PlainKind::Netns as usize,
            Call::Schedule(command) => match WaitKind::classify(command) {
                WaitKind::Forget => PlainKind::COUNT.checked_add(SCHEDULED)?.checked_add(4)?,
                kind => PlainKind::COUNT.checked_add(kind.index())?,
            },
            Call::PhysicalReport => PlainKind::COUNT.checked_add(SCHEDULED)?,
            Call::InitializeBacking(_) => {
                PlainKind::COUNT.checked_add(SCHEDULED)?.checked_add(1)?
            }
            Call::ReleaseBacking(_) => PlainKind::COUNT.checked_add(SCHEDULED)?.checked_add(2)?,
            Call::BackingUsage(_) => PlainKind::COUNT.checked_add(SCHEDULED)?.checked_add(3)?,
            Call::Create(_)
            | Call::Start(_)
            | Call::Exec { .. }
            | Call::Signal { .. }
            | Call::Inspect(_)
            | Call::Usage(_)
            | Call::Delete(_)
            | Call::Kill { .. }
            | Call::ExecStreams { .. }
            | Call::Run { .. }
            | Call::Capacity(_) => return None,
        },
        Request::Release | Request::Taken | Request::StreamsReady => return None,
    };
    waits.get(n)
}

/// What names the class that times a box kind's answer: the class itself (a create's or run's spec's, a capacity's),
/// or the box or program the call is for, whose class the connection may know ([`Known`]).
enum Timed<'a> {
    Class(Class),
    Box(&'a BoxId),
    Program(u64),
}

/// `request`'s box kind ([`BoxKind`]) and what names its class; `None` for a plain kind's request.
fn box_kind_of(request: &Request) -> Option<(BoxKind, Timed<'_>)> {
    let Request::Call(call) = request else {
        return None;
    };
    Some(match call {
        Call::Create(spec) => (BoxKind::Create, Timed::Class(spec.class.class())),
        Call::Start(id) => (BoxKind::Start, Timed::Box(id)),
        Call::Exec { id, .. } => (BoxKind::Exec, Timed::Box(id)),
        Call::Signal { id, .. } => (BoxKind::Signal, Timed::Box(id)),
        Call::Inspect(id) => (BoxKind::Inspect, Timed::Box(id)),
        Call::Usage(id) => (BoxKind::Usage, Timed::Box(id)),
        Call::Delete(id) => (BoxKind::Delete, Timed::Box(id)),
        Call::Kill { exec } => (BoxKind::Kill, Timed::Program(*exec)),
        Call::ExecStreams { id, .. } => (BoxKind::ExecStreams, Timed::Box(id)),
        Call::Run { spec, .. } => (BoxKind::Run, Timed::Class(spec.class.class())),
        Call::Capacity(class) => (BoxKind::Capacity, Timed::Class(*class)),
        Call::List
        | Call::Netns
        | Call::Schedule(_)
        | Call::PhysicalReport
        | Call::InitializeBacking(_)
        | Call::ReleaseBacking(_)
        | Call::BackingUsage(_) => return None,
    })
}

/// What a call's answer waits by (decision 9; AC-T7.4.1).
#[derive(Clone, Copy)]
enum AnswerWait {
    /// A release, a taken word: no answer comes.
    Unanswered,
    /// Its kind's learned wait, its class's where its box's class times it: learned from its answers.
    Learned(&'static Patience),
    /// A box or program of a class this connection doesn't know: the slowest its agent's classes allow, unarmed where
    /// any of them still measures; no class's, so learned by none.
    Unattributed(Option<Duration>),
}

impl AnswerWait {
    fn deadline(self) -> Option<Duration> {
        match self {
            Self::Unanswered => None,
            Self::Learned(waits) => waits.deadline(),
            Self::Unattributed(slowest) => slowest,
        }
    }

    fn learned(self) -> Option<&'static Patience> {
        match self {
            Self::Learned(waits) => Some(waits),
            Self::Unanswered | Self::Unattributed(_) => None,
        }
    }
}

/// The slowest wait `kind` has among `classes`, in transport `T`: unarmed where any of them still measures, or none
/// is named.
fn slowest<T: Transport>(kind: BoxKind, classes: &[Class]) -> Option<Duration> {
    let mut slowest: Option<Duration> = None;
    for class in classes {
        let wait = T::class_answers(*class).get(kind as usize)?.deadline()?;
        slowest = Some(slowest.map_or(wait, |s| s.max(wait)));
    }
    slowest
}

/// What a connection knows of its boxes' and programs' classes (AC-T7.4.1), so a call naming one waits as that class's
/// calls do: each box it created, ran, inspected or listed until its delete, at most [`MAX_LIST`] (a list's most), and
/// each program it started until let go, at most the agent's budget of programs; one past either goes unknown.
#[derive(Default)]
struct Known {
    /// The agent's classes, as its hello said: what a box or program the connection doesn't know could be of.
    agent: Vec<Class>,
    boxes: BTreeMap<BoxId, Class>,
    programs: BTreeMap<u64, Class>,
    /// The agent's budget of programs, as its hello said.
    programs_bound: usize,
}

impl Known {
    /// What `answer` to `request` (request id `id`) says of its box's or program's class.
    fn learn(&mut self, request: &Request, id: u64, answer: &Answer) {
        let Request::Call(call) = request else {
            return;
        };
        match (call, answer) {
            (Call::Create(_) | Call::Inspect(_), Answer::Info(info)) => {
                self.boxed(&info.id, info.class);
            }
            (Call::List, Answer::Boxes { infos, .. }) => {
                for info in infos {
                    self.boxed(&info.id, info.class);
                }
            }
            (Call::Run { spec, .. }, Answer::Started { .. } | Answer::Given { .. }) => {
                let class = spec.class.class();
                self.boxed(&spec.id, class);
                self.started(id, class);
            }
            (
                Call::Exec { id: of, .. } | Call::ExecStreams { id: of, .. },
                Answer::Started { .. } | Answer::Given { .. },
            ) => {
                if let Some(class) = self.boxes.get(of).copied() {
                    self.started(id, class);
                }
            }
            (Call::Delete(of), Answer::Done | Answer::Refused(Refusal::NoSuchBox(_))) => {
                self.boxes.remove(of);
            }
            _ => {}
        }
    }

    fn boxed(&mut self, id: &BoxId, class: Class) {
        if self.boxes.len() >= MAX_LIST && !self.boxes.contains_key(id) {
            return;
        }
        if let Ok(id) = id.try_clone() {
            self.boxes.insert(id, class);
        }
    }

    fn started(&mut self, exec: u64, class: Class) {
        if self.programs.len() < self.programs_bound || self.programs.contains_key(&exec) {
            self.programs.insert(exec, class);
        }
    }
}

/// Why a send didn't go.
pub enum Sent {
    /// The connection is gone: the call's wait says why.
    Gone,
    /// No room in the agent's socket in all the call's patience: the agent isn't reading.
    Full(Refusal),
    Refused(Refusal),
}

/// One operation's absolute reply/setup deadline. The caller's authority and the learned answer
/// wait stay distinct: exhausting caller authority is not a slower-answer calibration sample.
#[derive(Clone, Copy, Debug)]
pub struct ReplyDeadline {
    at: Instant,
    source: ReplyBound,
}

#[derive(Clone, Copy, Debug)]
enum ReplyBound {
    Caller(Duration),
    CallerAbsolute,
    Learned(Duration),
    /// A guest link's own bound on a connection's setup, where its caller gave no deadline.
    Link(Duration),
}

impl ReplyDeadline {
    pub(crate) fn caller(budget: Option<Duration>) -> Result<Option<Self>, Refusal> {
        budget
            .map(|budget| Self::from_now(budget, ReplyBound::Caller(budget), "client reply budget"))
            .transpose()
    }

    /// A link's own setup bound from now ([`crate::guest::Link::setup_bound`]): none where it has none.
    pub(crate) fn link(bound: Option<Duration>) -> Result<Option<Self>, Refusal> {
        bound
            .map(|bound| Self::from_now(bound, ReplyBound::Link(bound), "guest link setup bound"))
            .transpose()
    }

    fn from_now(span: Duration, source: ReplyBound, what: &'static str) -> Result<Self, Refusal> {
        let at = (!span.is_zero())
            .then(|| Instant::now().checked_add(span))
            .flatten()
            .ok_or_else(|| Refusal::Invalid {
                what,
                why: "it must be positive and fit a monotonic deadline".to_owned(),
            })?;
        Ok(Self { at, source })
    }

    pub(crate) fn caller_before(at: Instant) -> Result<Self, Refusal> {
        let deadline = Self {
            at,
            source: ReplyBound::CallerAbsolute,
        };
        deadline.remaining("starting a client operation")?;
        Ok(deadline)
    }

    pub(crate) fn limited_by(self, configured: Option<Self>) -> Self {
        match configured {
            Some(configured) if configured.at < self.at => configured,
            _ => self,
        }
    }

    /// The same instant may be lent to a connector; partial progress must never replace it.
    pub fn at(self) -> Instant {
        self.at
    }

    /// Remaining authority for protocol setup, or a refusal naming the bound that expired.
    pub fn remaining(self, op: &'static str) -> Result<Duration, Refusal> {
        let left = self.at.saturating_duration_since(Instant::now());
        if left.is_zero() {
            Err(self.refusal(op))
        } else {
            Ok(left)
        }
    }

    /// The bound that fails a call asked at `asked`, and the learned wait's end it passes on the way (decision 9,
    /// AC-T7.4.1). With a caller's deadline only that fails it: its kind's learned wait (`patience`), where it ends
    /// first, is a run-out to record as it passes, an answer slower than any before, not an agent taken for dead.
    /// With none, the learned wait is the call's bound, its hang detection.
    fn combine(
        caller: Option<Self>,
        asked: Instant,
        patience: Option<Duration>,
    ) -> (Option<Self>, Option<Self>) {
        let learned = patience.and_then(|wait| {
            asked.checked_add(wait).map(|at| Self {
                at,
                source: ReplyBound::Learned(wait),
            })
        });
        match (caller, learned) {
            // A tie belongs to caller authority, so it cannot enlarge the calibration census.
            (Some(caller), Some(learned)) if learned.at < caller.at => {
                (Some(caller), Some(learned))
            }
            (Some(caller), _) => (Some(caller), None),
            (None, learned) => (learned, None),
        }
    }

    fn refusal(self, op: &'static str) -> Refusal {
        let why = match self.source {
            ReplyBound::Caller(budget) => {
                format!("the caller's explicit reply budget of {budget:?} expired")
            }
            ReplyBound::CallerAbsolute => {
                "the caller's explicit absolute reply deadline expired".to_owned()
            }
            ReplyBound::Learned(wait) => {
                format!("none came in {wait:?}, twice the slowest answer to a call of its kind")
            }
            ReplyBound::Link(bound) => {
                format!("the guest link's own setup bound of {bound:?} expired")
            }
        };
        Refusal::Os { op, why }
    }

    fn ran_out(self, waits: Option<&Patience>) {
        if let (ReplyBound::Learned(wait), Some(waits)) = (self.source, waits) {
            waits.ran_out(wait);
        }
    }

    /// A call's `budget` passed with no answer, and with it every learned wait before it: the call's own where its
    /// caller gave no deadline, else `learned`, the one under the caller's not yet told, each a run-out to `waits`.
    fn ended(budget: Option<Self>, learned: Option<Self>, waits: Option<&Patience>) {
        for bound in [budget, learned].into_iter().flatten() {
            bound.ran_out(waits);
        }
    }
}

/// A descriptor lent to a message as it is sent (an exec's stdio, a pipe's end of the caller's): the agent gets
/// its own, and this one stays the caller's.
#[cfg(unix)]
pub type Lent<'a> = std::os::fd::BorrowedFd<'a>;
#[cfg(windows)]
pub type Lent<'a> = std::os::windows::io::BorrowedHandle<'a>;

/// No descriptors: what lends nothing to a message.
const NONE: &[Lent<'static>] = &[];

/// An OS handle the core waits on: a file descriptor on Unix, a Winsock socket on Windows.
#[cfg(unix)]
pub type Handle<'a> = std::os::fd::BorrowedFd<'a>;
#[cfg(windows)]
pub type Handle<'a> = std::os::windows::io::BorrowedSocket<'a>;

/// What a wait on a connection found.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Ready {
    /// Something to read, or its end.
    read: bool,
    /// Room to write.
    write: bool,
    /// The waiter's wake was written: the reader's turn was handed to it, or the connection ended.
    woken: bool,
}

/// Waits until `conn` has something to read or has ended, or has room to write (with `write`), or `wake` is
/// written, at most `wait` (none: until one does). An interrupted wait finds nothing.
fn ready(
    conn: Handle<'_>,
    write: bool,
    wake: Option<&Wake>,
    wait: Option<Duration>,
) -> Result<Ready, Refusal> {
    let flags = if write {
        PollFlags::IN | PollFlags::OUT
    } else {
        PollFlags::IN
    };
    poll_for(conn, flags, wake, wait)
}

/// Protocol setup waits for only the direction that can advance it. Waiting for IN while a
/// naming write needs OUT would repeatedly wake on unrelated peer bytes without making progress.
pub(crate) fn protocol_ready(
    conn: Handle<'_>,
    write: bool,
    deadline: ReplyDeadline,
    op: &'static str,
) -> Result<(), Refusal> {
    loop {
        let left = deadline.remaining(op)?;
        let flags = if write { PollFlags::OUT } else { PollFlags::IN };
        let got = poll_for(conn, flags, None, Some(left))?;
        if got.read || got.write {
            return Ok(());
        }
    }
}

fn poll_for(
    conn: Handle<'_>,
    flags: PollFlags,
    wake: Option<&Wake>,
    wait: Option<Duration>,
) -> Result<Ready, Refusal> {
    let mut fds = vec![PollFd::from_borrowed_fd(conn, flags)];
    if let Some(wake) = wake {
        fds.push(PollFd::from_borrowed_fd(wake.handle(), PollFlags::IN));
    }
    match rustix::event::poll(&mut fds, wait.map(timespec).as_ref()) {
        Ok(_) => {}
        Err(e) if e == rustix::io::Errno::INTR => return Ok(Ready::default()),
        Err(e) => {
            return Err(Refusal::Broken(format!(
                "waiting on the node agent's connection: {e}"
            )));
        }
    }
    let got = |n: usize| fds.get(n).map_or(PollFlags::empty(), PollFd::revents);
    let (conn, woken) = (got(0), got(1));
    Ok(Ready {
        read: conn.intersects(PollFlags::IN | PollFlags::ERR | PollFlags::HUP),
        write: conn.contains(PollFlags::OUT),
        woken: !woken.is_empty(),
    })
}

/// What wakes a send waiting for room on the connection when the reader's turn is handed to it, or the connection
/// ends: one end of a pair the waiter polls beside the connection, made only when a send first waits with the turn
/// another's, so a send that finds room pays nothing for it. A Unix socket pair, or on Windows (whose `WSAPoll` waits
/// on sockets alone) a loopback datagram socket connected to itself, so no other socket can write it.
struct Wake {
    #[cfg(unix)]
    read: std::os::unix::net::UnixStream,
    #[cfg(unix)]
    write: std::os::unix::net::UnixStream,
    #[cfg(windows)]
    sock: std::net::UdpSocket,
}

impl Wake {
    #[cfg(unix)]
    fn new() -> std::io::Result<Self> {
        let (read, write) = std::os::unix::net::UnixStream::pair()?;
        read.set_nonblocking(true)?;
        write.set_nonblocking(true)?;
        Ok(Self { read, write })
    }

    #[cfg(windows)]
    fn new() -> std::io::Result<Self> {
        let sock = std::net::UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
        sock.connect(sock.local_addr()?)?;
        sock.set_nonblocking(true)?;
        Ok(Self { sock })
    }

    /// Wakes the waiter: a byte it reads as one wake, however many came.
    fn wake(&self) {
        #[cfg(unix)]
        let _ = std::io::Write::write(&mut &self.write, &[1]);
        #[cfg(windows)]
        let _ = self.sock.send(&[1]);
    }

    /// What woke it, read, so it wakes no one until written again.
    fn drain(&self) {
        let mut buf = [0u8; 64];
        #[cfg(unix)]
        while matches!(std::io::Read::read(&mut &self.read, &mut buf), Ok(n) if n > 0) {}
        #[cfg(windows)]
        while self.sock.recv(&mut buf).is_ok() {}
    }

    fn handle(&self) -> Handle<'_> {
        #[cfg(unix)]
        return std::os::fd::AsFd::as_fd(&self.read);
        #[cfg(windows)]
        std::os::windows::io::AsSocket::as_socket(&self.sock)
    }
}

/// What a transport's read hands each whole message to: the message, with the descriptors that came with it.
pub(crate) type Take<'a> = dyn FnMut(&Received<'_>, Vec<hex_api::End>) -> Result<(), Refusal> + 'a;

/// What carries a client's messages to its node agent. Its connection is read and written without waiting, and
/// waited on through its handle ([`Transport::handle`]), by whoever holds the reader's turn.
pub trait Transport: Send + Sync + 'static {
    /// What the turn's holder reads into, and what of a message has come so far: kept across reads and across
    /// holders, so a frame cut across reads is whole when its last bytes come.
    type Reading: Send;
    /// Its plain kinds' learned waits for an answer, by [`kind_of`]'s order: each its own record's, as its
    /// transport and its agent answer it, measuring where none is recorded yet (decision 9). A call's bound where its
    /// caller gives no deadline; under one, a run-out recorded as it passes ([`ReplyDeadline::combine`]).
    fn answers() -> &'static [Patience; KINDS];
    /// AC-T7.4.1: the learned waits of the kinds a box's class times ([`BoxKind`]) for boxes of `class`, each (kind,
    /// class) its own record and census name, a class with none yet measuring, never another class's. A transport
    /// with no class's record (a test's) leaves every class measuring.
    fn class_answers(class: Class) -> &'static [Patience; BOX_KINDS] {
        by_class(&UNRECORDED, class)
    }
    /// Optional caller policy for one synchronous reply, separate from learned calibration.
    /// None preserves the transport's existing wait behavior.
    fn reply_budget(&self) -> Option<Duration> {
        None
    }
    /// A read state for a fresh connection.
    fn reading(&self) -> Self::Reading;
    /// The connection's OS handle, lent for the core to wait on.
    fn handle(&self) -> Handle<'_>;
    /// Reads what has come, without waiting: each whole message, with the descriptors that came with it, handed to
    /// `take`, a message cut across reads kept in `reading` for the next. Whether anything came; the connection's end,
    /// or a message `take` refuses, is the error, saying why.
    fn recv(&self, reading: &mut Self::Reading, take: &mut Take<'_>) -> Result<bool, Refusal>;
    /// The bytes a message goes as: itself where the transport keeps messages whole, framed where it is a stream.
    fn frame(message: Vec<u8>) -> Result<Vec<u8>, Refusal> {
        Ok(message)
    }
    /// Writes what it can of `bytes` past `*at` without waiting, the descriptors lent riding with its first byte,
    /// moving `at` on: whether all of it is out. The caller holds the connection's send lock across a message's
    /// writes, or owes the rest of one cut short ahead of all else before letting it go, so no other message's bytes
    /// come between them.
    fn try_send(&self, bytes: &[u8], at: &mut usize, lent: &[Lent<'_>]) -> Result<bool, Sent>;
    /// Ends the connection both ways: whoever reads next sees the end.
    fn shutdown(&self);
    /// Whether every stream of an exec is a connection of its own that it makes, and no descriptor rides with a
    /// message: a guest's. It makes those an exec asks for ([`Transport::asks`]) and takes the pipes the caller
    /// carries ([`Transport::carries`]); elsewhere the carried pipes ride with the exec, and a stream asked for
    /// is the agent's to make, which gives its end with its answer ([`Answer::Given`]).
    const MAKES_ASKED: bool = false;
    /// The caller's own pipes for exec `exec`, each copied between it and a connection of its own made for it,
    /// here in the caller's client: only where [`Transport::MAKES_ASKED`].
    fn carries(&self, exec: u64, stdio: Stdio) -> Result<(), Refusal> {
        let _ = (exec, stdio);
        Err(Refusal::Broken(
            "this transport lends an exec's pipes to its message".to_owned(),
        ))
    }
    /// Deadline-aware setup for a transport that opts into [`Transport::reply_budget`].
    /// The default refuses before making streams rather than claiming an unbounded adapter is bounded.
    fn carries_before(
        &self,
        exec: u64,
        stdio: Stdio,
        deadline: ReplyDeadline,
    ) -> Result<(), Refusal> {
        let _ = (exec, stdio, deadline);
        Err(unbounded_setup("carried streams"))
    }
    /// The streams `ask` names for exec `exec`, made here, its program's ends the agent's to find (a
    /// guest's stdio connections, named for it): only where [`Transport::MAKES_ASKED`].
    /// Opens an offered guest's named streams in this caller, never by importing a reserved remote socket.
    /// The synchronous call temporarily hands off its reader turn while this bounded connect runs.
    /// Names a reserved guest exec without opening any of its stdio connections here.
    fn stream_offer(&self, exec: u64, streams: Streams) -> Result<wire::StreamOffer, Refusal> {
        let _ = (exec, streams);
        Err(Refusal::Invalid {
            what: "stream offer",
            why: "this transport passes native ends".to_owned(),
        })
    }

    fn open_offer(&self, offer: &wire::StreamOffer) -> Result<hex_api::Given, Refusal> {
        let _ = offer;
        Err(Refusal::Platform {
            what: "caller-owned guest streams",
            platform: std::env::consts::OS,
            why: "this transport receives native stream ends instead of guest connection offers"
                .to_owned(),
        })
    }

    fn open_offer_before(
        &self,
        offer: &wire::StreamOffer,
        deadline: ReplyDeadline,
    ) -> Result<hex_api::Given, Refusal> {
        let _ = (offer, deadline);
        Err(unbounded_setup("offered streams"))
    }

    fn asks(&self, exec: u64, ask: hex_api::Ask) -> Result<hex_api::Given, Refusal> {
        let _ = (exec, ask);
        Err(Refusal::Broken(
            "this transport asks the agent for an exec's streams".to_owned(),
        ))
    }

    fn asks_before(
        &self,
        exec: u64,
        ask: hex_api::Ask,
        deadline: ReplyDeadline,
    ) -> Result<hex_api::Given, Refusal> {
        let _ = (exec, ask, deadline);
        Err(unbounded_setup("asked streams"))
    }
}

fn unbounded_setup(what: &'static str) -> Refusal {
    Refusal::Invalid {
        what,
        why: "this transport has no deadline-aware stream setup".to_owned(),
    }
}

pub(crate) fn poisoned<T>(_: std::sync::PoisonError<T>) -> Refusal {
    Refusal::Broken("a lock of the node agent's client was poisoned".to_owned())
}

/// The longest one wait on a connection can be: `i32::MAX` milliseconds, the most poll(2) takes where it has no
/// `ppoll` (macOS; Windows' `WSAPoll`), its timeout a C `int` of milliseconds. A longer wait waits this long at a
/// time, its caller waiting again until its own deadline.
const MOST_ONE_WAIT: Duration = Duration::from_millis(2_147_483_647);
/// [`MOST_ONE_WAIT`] as poll(2) takes it.
const MOST_ONE_WAIT_SPEC: rustix::event::Timespec = rustix::event::Timespec {
    tv_sec: 2_147_483,
    tv_nsec: 647_000_000,
};

/// `wait` as poll(2) takes it, at most [`MOST_ONE_WAIT`].
fn timespec(wait: Duration) -> rustix::event::Timespec {
    if wait >= MOST_ONE_WAIT {
        return MOST_ONE_WAIT_SPEC;
    }
    rustix::event::Timespec::try_from(wait).unwrap_or(MOST_ONE_WAIT_SPEC)
}

/// What the callers and the `Proc`s share: the connection, what waits on it, what its reader's turn reads into, and
/// its send lock.
pub struct Shared<T: Transport> {
    pub(crate) transport: T,
    state: Mutex<State>,
    /// The turn's holder's: one read at a time.
    reading: Mutex<T::Reading>,
    /// Held across one whole message's writes.
    sending: SendLock,
    /// What the client owes the agent ahead of any request it sends from now.
    owed: Mutex<Owed>,
    /// The classes of the boxes and programs it knows, so a call naming one waits by its class ([`Known`]).
    known: Mutex<Known>,
}

/// One connection state allocation, shared by its Core, Pump and remote programs.
pub(crate) type SharedOwner<T> = triomphe::Arc<Shared<T>>;

/// The existing whole-message send authority. Only its acquisition changes: a queued caller can
/// wait on the current sender's release with its own deadline instead of blocking inside Mutex::lock.
#[derive(Default)]
struct SendLock {
    busy: Mutex<bool>,
    released: Condvar,
}

struct Whole<'a>(&'a SendLock);

impl SendLock {
    fn lock(&self, deadline: Option<Instant>) -> Result<Option<Whole<'_>>, Refusal> {
        let mut busy = self.busy.lock().map_err(poisoned)?;
        while *busy {
            busy = match deadline {
                Some(at) => {
                    let left = at.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Ok(None);
                    }
                    self.released.wait_timeout(busy, left).map_err(poisoned)?.0
                }
                None => self.released.wait(busy).map_err(poisoned)?,
            };
        }
        *busy = true;
        Ok(Some(Whole(self)))
    }

    fn try_lock(&self) -> Result<Option<Whole<'_>>, Refusal> {
        let mut busy = self.busy.lock().map_err(poisoned)?;
        if *busy {
            return Ok(None);
        }
        *busy = true;
        Ok(Some(Whole(self)))
    }
}

impl Drop for Whole<'_> {
    fn drop(&mut self) {
        if let Ok(mut busy) = self.0.busy.lock() {
            *busy = false;
            self.0.released.notify_one();
        }
    }
}

/// The words a client owes its agent ahead of any later request: each given answer's [`Request::Taken`], framed,
/// queued by the read that hands the answer on before its taker wakes, so a read never waits to send; and the rest of
/// a release cut short on a stream, queued first by its sender before it lets the send lock go ([`Conn::send_now`]).
/// They go out whole, in order, under the send lock: first by whoever sends next, before its own message, and by the
/// reader's turn's holder, which waits for room for them beside what comes, so one goes as soon as the agent reads.
/// None is dropped: an unsent one leaves the agent holding the given ends, and with them its program's stdin open,
/// until the connection closes. At most `bound` wait: the agent's budget of programs (`Hello::budgets`), which counts
/// the given ends not yet taken, so the queue can't grow past what the agent can give (CLAUDE.md §2).
#[derive(Default)]
struct Owed {
    words: VecDeque<Vec<u8>>,
    bound: usize,
}

#[derive(Default)]
struct State {
    /// Calls waiting for their answers, by request id.
    calls: BTreeMap<u64, Waiting>,
    /// Programs whose end something waits for, by their exec's id.
    programs: BTreeMap<u64, Watching>,
    next: u64,
    /// Why the connection ended, once it has.
    closed: Option<Refusal>,
    /// Why the agent won't serve the connection, when it said (an answer to no request, id 0).
    refused: Option<Refusal>,
    /// Each exec's streams' ends, by its request's id, until its exec takes them: those its transport made (a
    /// guest's connections), or those its agent's answer gave as its descriptors. An exec refused has its ends
    /// closed with its answer.
    given: BTreeMap<u64, hex_api::Given>,
    /// Whether a thread holds the reader's turn, or the event form's loop does for good.
    reading: bool,
    /// The execs whose `Proc` is in its wait, which the turn may be handed to.
    waiting: BTreeSet<u64>,
}

impl State {
    /// The reader's turn given up by its holder: handed to the oldest call that can take it (one whose request
    /// went, or whose send waits for room), else to a `Proc` in its wait, else free, so while anyone waits on the
    /// connection someone reads it. A call queued for the send lock can't read until it has it, so it is passed by.
    fn pass_turn(&mut self) {
        for waiting in self.calls.values() {
            if waiting.reads
                && let Taker::Wait(mail) = &waiting.answer
                && mail.try_send(Mail::Turn).is_ok()
            {
                if let Some(wake) = &waiting.wake {
                    wake.wake();
                }
                return;
            }
        }
        for exec in &self.waiting {
            if let Some(Watching::Proc(ending)) = self.programs.get(exec)
                && let Ok(mut ended) = ending.state.lock()
            {
                ended.turn = true;
                ending.came.notify_all();
                return;
            }
        }
        self.reading = false;
    }
}

struct Waiting {
    answer: Taker,
    /// At most one preparation precedes an exec's final answer.
    offered: bool,
    /// A list's boxes from the parts that came so far.
    boxes: Vec<BoxInfo>,
    /// Whether it can take the reader's turn: its request sent, or its send waiting for room with the send lock held.
    reads: bool,
    /// While its send waits for room with the reader's turn another's: the wake it waits on besides the connection,
    /// written as the turn is handed to it or the connection ends.
    wake: Option<Arc<Wake>>,
}

/// What a waiting call is handed: its answer, or the reader's turn. The answer is boxed, so a turn is no bigger than
/// a pointer.
enum Mail {
    Answer(Box<Result<Answer, Refusal>>),
    Turn,
}

/// What takes a call's answer: a caller waiting on it, or a completion ([`Then`]).
enum Taker {
    Wait(SyncSender<Mail>),
    Then(Then),
}

/// What waits for a program's end: its `Proc`, or a completion ([`Ended`]).
enum Watching {
    Proc(Arc<Ending>),
    Then(Ended),
}

/// A completion to run once the client's lock is let go.
type Due = Box<dyn FnOnce() + Send>;

impl Taker {
    /// Gives `answer` to its taker: at once to a waiting caller, or as a completion run later.
    fn give(self, id: u64, answer: Result<Answer, Refusal>, due: &mut Vec<Due>) {
        match self {
            Self::Wait(mail) => {
                let _ = mail.try_send(Mail::Answer(Box::new(answer)));
            }
            Self::Then(then) => due.push(Box::new(move || then(id, answer))),
        }
    }
}

/// A program's end, once it has come, and its `Proc`'s waking: by its end, or by the reader's turn handed to its wait.
#[derive(Default)]
struct Ending {
    state: Mutex<EndState>,
    came: Condvar,
}

#[derive(Default)]
struct EndState {
    exit: Option<Result<Exit, Refusal>>,
    turn: bool,
}

impl<T: Transport> Shared<T> {
    pub(crate) fn try_new(transport: T) -> Result<SharedOwner<T>, Refusal> {
        let reading = transport.reading();
        triomphe::Arc::try_new(Self {
            transport,
            state: Mutex::new(State::default()),
            reading: Mutex::new(reading),
            sending: SendLock::default(),
            owed: Mutex::new(Owed::default()),
            known: Mutex::new(Known::default()),
        })
        .map_err(|_| Refusal::Exhausted {
            what: "client shared state owner",
            bound: std::mem::size_of::<Self>(),
        })
    }

    fn state(&self) -> Result<MutexGuard<'_, State>, Refusal> {
        self.state.lock().map_err(poisoned)
    }

    /// Hands one message to whoever waits for it: an answer to its call, a program's end to its `Proc` or completion;
    /// a given answer's descriptors, one for each stream it names, in order, kept for its exec while its call waits. One
    /// nobody waits for (a call that gave up waiting, a program released) has no taker, and a given one's ends close.
    /// Completions run after the lock is let go, so one may call the client again.
    fn deliver(
        &self,
        header: Header,
        answer: Answer,
        fds: Vec<hex_api::End>,
    ) -> Result<(), Refusal> {
        if let Answer::Given { .. } = &answer {
            // Owed before its taker wakes below, so the word goes ahead of anything its caller sends after.
            self.owe(header.id, &Request::Taken)?;
        }
        let mut due: Vec<Due> = Vec::new();
        {
            let mut state = self.state()?;
            if let Answer::Given { given, .. } = &answer {
                let mut fds = fds.into_iter();
                let mut take = |on: bool| if on { fds.next() } else { None };
                let ends = hex_api::Given {
                    stdin: take(given.stdin),
                    stdout: take(given.stdout),
                    stderr: take(given.stderr),
                };
                // AC-T7.4.1: kept only while its call waits. One that gave up never takes them, so
                // they close here, and the Taken word owed above lets the agent's copies go too.
                if state.calls.contains_key(&header.id) {
                    state.given.insert(header.id, ends);
                }
            }
            match answer {
                Answer::Offered(offer) => {
                    if !state.programs.contains_key(&header.id)
                        || !state.calls.contains_key(&header.id)
                    {
                        // A legitimate answer can outlive its caller's patience. Cancel just its preparation,
                        // under the same program-bound word queue as Taken, without ending unrelated programs.
                        self.owe(header.id, &Request::Release)?;
                        return Ok(());
                    }
                    let Some(waiting) = state.calls.get_mut(&header.id) else {
                        return Ok(());
                    };
                    if waiting.offered {
                        return Err(Refusal::Broken(
                            "an exec was offered streams twice".to_owned(),
                        ));
                    }
                    let Taker::Wait(mail) = &waiting.answer else {
                        return Err(Refusal::Invalid {
                            what: "event-form guest streams",
                            why: "a connection offer must be opened by its synchronous caller"
                                .to_owned(),
                        });
                    };
                    mail.try_send(Mail::Answer(Box::new(Ok(Answer::Offered(offer)))))
                        .map_err(|_| {
                            Refusal::Broken("a stream offer has no waiting taker".to_owned())
                        })?;
                    waiting.offered = true;
                }
                Answer::Exited(exit) => match state.programs.remove(&header.id) {
                    Some(Watching::Proc(ending)) => {
                        ending.state.lock().map_err(poisoned)?.exit = Some(Ok(exit));
                        ending.came.notify_all();
                        // Its Proc forgets it when it is dropped or has its end.
                        state.programs.insert(header.id, Watching::Proc(ending));
                    }
                    Some(Watching::Then(ended)) => due.push(Box::new(move || ended(Ok(exit)))),
                    None => {}
                },
                Answer::Refused(refusal) if header.id == 0 => state.refused = Some(refusal),
                Answer::Boxes { infos, more: true } => {
                    if let Some(waiting) = state.calls.get_mut(&header.id) {
                        if waiting.boxes.len().saturating_add(infos.len()) > MAX_LIST {
                            if let Some(w) = state.calls.remove(&header.id) {
                                w.answer.give(
                                    header.id,
                                    Err(Refusal::Exhausted {
                                        what: "boxes in one list",
                                        bound: MAX_LIST,
                                    }),
                                    &mut due,
                                );
                            }
                        } else {
                            waiting.boxes.extend(infos);
                        }
                    }
                }
                answer => {
                    if matches!(&answer, Answer::Started { .. })
                        && state
                            .calls
                            .get(&header.id)
                            .is_some_and(|waiting| waiting.offered)
                        && !state.given.contains_key(&header.id)
                    {
                        return Err(Refusal::Broken(
                            "an offered exec started before its caller opened the streams"
                                .to_owned(),
                        ));
                    }
                    // An exec that started no program has no end to come, and no streams: a completion waiting for
                    // its end is let go, and the ends made for it close.
                    if !matches!(answer, Answer::Started { .. } | Answer::Given { .. }) {
                        if matches!(state.programs.get(&header.id), Some(Watching::Then(_))) {
                            state.programs.remove(&header.id);
                        }
                        state.given.remove(&header.id);
                    }
                    if let Some(mut waiting) = state.calls.remove(&header.id) {
                        let answer = match answer {
                            Answer::Boxes { infos, more } => {
                                waiting.boxes.extend(infos);
                                Answer::Boxes {
                                    infos: std::mem::take(&mut waiting.boxes),
                                    more,
                                }
                            }
                            other => other,
                        };
                        waiting.answer.give(header.id, Ok(answer), &mut due);
                    }
                }
            }
        }
        for run in due {
            run();
        }
        Ok(())
    }

    /// The wait `request`'s answer has (AC-T7.4.1): its plain kind's, or its box kind's for the class that times it,
    /// the class the request names or the one this connection knows its box or program is of; for one it doesn't
    /// know, the slowest of the agent's classes' waits, learned by none.
    fn answer_wait(&self, request: &Request) -> AnswerWait {
        let Some((kind, timed)) = box_kind_of(request) else {
            return kind_of(T::answers(), request)
                .map_or(AnswerWait::Unanswered, AnswerWait::Learned);
        };
        let known = self.known.lock().ok();
        let class = match timed {
            Timed::Class(class) => Some(class),
            Timed::Box(id) => known.as_ref().and_then(|k| k.boxes.get(id).copied()),
            Timed::Program(exec) => known.as_ref().and_then(|k| k.programs.get(&exec).copied()),
        };
        match class {
            Some(class) => T::class_answers(class)
                .get(kind as usize)
                .map_or(AnswerWait::Unanswered, AnswerWait::Learned),
            None => AnswerWait::Unattributed(known.and_then(|k| slowest::<T>(kind, &k.agent))),
        }
    }

    /// Owes the agent the word that exec `exec`'s given ends were taken, ahead of every later request ([`Owed`]). Past
    /// the agent's budget of programs, which each given answer's ends hold until taken, the agent has given more than
    /// it can, and the read that brought it ends the connection.
    fn owe(&self, exec: u64, request: &Request) -> Result<(), Refusal> {
        let word = wire::request(exec, request, wire::MAX_MESSAGE)
            .map_err(Refusal::from)
            .and_then(T::frame)?;
        let mut owed = self.owed.lock().map_err(poisoned)?;
        if owed.words.len() >= owed.bound {
            return Err(Refusal::Exhausted {
                what: "given ends and stream preparations awaiting the client's acknowledgement or cancellation, the node agent's programs",
                bound: owed.bound,
            });
        }
        owed.words.push_back(word);
        Ok(())
    }

    /// Whether anything is owed ([`Owed`]).
    fn owing(&self) -> bool {
        self.owed.lock().is_ok_and(|owed| !owed.words.is_empty())
    }

    /// Writes what is owed, each word whole and in order, without waiting, the send lock held (`_whole`): whether all
    /// of it is out. A word cut short on a stream keeps its rest first, so nothing comes between its bytes; one a
    /// failed write cut short leaves the stream unable to carry another, and the connection goes.
    fn send_owed(&self, _whole: &Whole<'_>) -> Result<bool, Sent> {
        loop {
            let word = self
                .owed
                .lock()
                .map_err(|e| Sent::Refused(poisoned(e)))?
                .words
                .pop_front();
            let Some(word) = word else {
                return Ok(true);
            };
            let mut at = 0;
            let sent = self.transport.try_send(&word, &mut at, NONE);
            if !matches!(sent, Ok(true)) {
                let rest = word.get(at..).unwrap_or_default().to_vec();
                self.owed
                    .lock()
                    .map_err(|e| Sent::Refused(poisoned(e)))?
                    .words
                    .push_front(rest);
            }
            match sent {
                Ok(true) => {}
                Ok(false) => return Ok(false),
                Err(e) => {
                    if at > 0 {
                        self.transport.shutdown();
                    }
                    return Err(e);
                }
            }
        }
    }

    /// Writes what is owed now, if the send lock is free and there is room; whoever holds the lock writes it before its
    /// own message and after it. The connection's end, should a word find it, is the next read's to see.
    pub(crate) fn send_owed_now(&self) {
        if let Ok(Some(whole)) = self.sending.try_lock() {
            let _ = self.send_owed(&whole);
        }
    }

    /// The connection has ended: every call waiting and every program's `Proc` or completion hear why, and no one
    /// reads any more.
    pub(crate) fn close(&self, why: Refusal) {
        let mut due: Vec<Due> = Vec::new();
        {
            let Ok(mut state) = self.state() else {
                return;
            };
            let why = state.refused.take().unwrap_or(why);
            // HEX-RT-17: an unanswered exec has not started, including an unsent Deferred. Its call alone
            // gets the refusal; reporting an end too would invent a program and can orphan a remote early end.
            // Snapshot before draining: at most the pending calls already held by this connection.
            let pending: BTreeSet<u64> = state.calls.keys().copied().collect();
            for (id, waiting) in std::mem::take(&mut state.calls) {
                waiting.answer.give(id, Err(why.clone()), &mut due);
                if let Some(wake) = &waiting.wake {
                    wake.wake();
                }
            }
            let lost = Refusal::Broken(format!(
                "the connection to the node agent closed before the program's end came: {why}"
            ));
            for (exec, watching) in std::mem::take(&mut state.programs) {
                match watching {
                    Watching::Proc(ending) => {
                        if let Ok(mut ended) = ending.state.lock()
                            && ended.exit.is_none()
                        {
                            ended.exit = Some(Err(lost.clone()));
                            ending.came.notify_all();
                        }
                        state.programs.insert(exec, Watching::Proc(ending));
                    }
                    Watching::Then(ended) if pending.contains(&exec) => {
                        // Release the callback's captures outside the state lock, without invoking it.
                        due.push(Box::new(move || drop(ended)));
                    }
                    Watching::Then(ended) => {
                        let lost = lost.clone();
                        due.push(Box::new(move || ended(Err(lost))));
                    }
                }
            }
            state.closed = Some(why);
            state.reading = false;
        }
        for run in due {
            run();
        }
    }

    /// The connection ended for `why`: everyone waiting told ([`Shared::close`]), and why, as each is told.
    fn end(&self, why: Refusal) -> Refusal {
        self.transport.shutdown();
        self.close(why.clone());
        self.state()
            .ok()
            .and_then(|state| state.closed.clone())
            .unwrap_or(why)
    }

    /// One wait and read by the turn's holder: the connection waited on at most `wait` (none: as long as it takes),
    /// for what comes and, while words are owed ([`Owed`]), for room to send them, then what has come read and handed
    /// to whoever it is for, and what is owed sent if it can go. Whether anything came; the connection's end is the
    /// error, everyone waiting told why.
    fn turn(&self, wait: Option<Duration>) -> Result<bool, Refusal> {
        let owing = self.owing();
        let ready = match ready(self.transport.handle(), owing, None, wait) {
            Ok(ready) => ready,
            Err(why) => return Err(self.end(why)),
        };
        let came = if ready.read { self.recv()? } else { false };
        if owing || came {
            self.send_owed_now();
        }
        Ok(came)
    }

    /// What has come, read by the turn's holder (or the event form's loop) without waiting and handed to whoever it
    /// is for. Whether anything came; the connection's end, or a message that isn't an answer, is the error, everyone
    /// waiting told why.
    pub(crate) fn recv(&self) -> Result<bool, Refusal> {
        // A lock poisoned mid-read leaves the connection's messages unknown: it ends, everyone waiting told.
        let mut reading = match self.reading.lock() {
            Ok(reading) => reading,
            Err(e) => return Err(self.end(poisoned(e))),
        };
        let mut take = |msg: &Received<'_>, fds: Vec<hex_api::End>| -> Result<(), Refusal> {
            let (header, answer) = wire::read_answer(msg, wire::MAX_MESSAGE).map_err(|fault| {
                match fault {
                    // An agent of another wire: refused by type, naming both wires and both builds.
                    wire::Fault::Wire { .. } => Refusal::from(fault),
                    fault => Refusal::Broken(format!(
                        "the node agent sent a message that isn't an answer: {fault}"
                    )),
                }
            })?;
            self.deliver(header, answer, fds)
        };
        let got = self.transport.recv(&mut reading, &mut take);
        drop(reading);
        got.map_err(|why| self.end(why))
    }

    /// The event form: the connection is read by the caller's loop from now on ([`Shared::recv`]), never by a call
    /// or a `Proc`, which wait for what the loop hands them.
    pub(crate) fn read_by_loop(&self) -> Result<(), Refusal> {
        self.state()?.reading = true;
        Ok(())
    }

    /// Whether its connection has ended.
    fn closed(&self) -> bool {
        !self.state().is_ok_and(|state| state.closed.is_none())
    }
}

/// A call while it is made: its place among the calls, its mailbox, its answer once come, and whether it holds the
/// reader's turn. However it goes it hands on a turn it holds or was handed, and forgets the call if unanswered.
struct Caller<'a, T: Transport> {
    shared: &'a Shared<T>,
    id: u64,
    mail: Receiver<Mail>,
    /// Whether it holds the reader's turn.
    mine: bool,
    /// Its answer, come to its mailbox and not yet returned.
    answer: Option<Result<Answer, Refusal>>,
    /// Answered: an exec's place for its program's end is then its `Proc`'s, and its streams' ends its exec's.
    answered: bool,
}

impl<'a, T: Transport> Caller<'a, T> {
    /// A call's place among the calls, with `program`, an exec's place for the end of the program it starts, made
    /// before the exec goes, so the end can't come first.
    fn new(shared: &'a Shared<T>, program: Option<Arc<Ending>>) -> Result<Self, Refusal> {
        // An answer, and the reader's turn handed to it before that: never more at once.
        let (tx, mail) = sync_channel(2);
        let mut state = shared.state()?;
        if let Some(why) = &state.closed {
            return Err(why.clone());
        }
        let id = state
            .next
            .checked_add(1)
            .ok_or_else(|| Refusal::Broken("the client's request ids ran out".to_owned()))?;
        state.next = id;
        state.calls.insert(
            id,
            Waiting {
                answer: Taker::Wait(tx),
                offered: false,
                boxes: Vec::new(),
                reads: false,
                wake: None,
            },
        );
        if let Some(program) = program {
            state.programs.insert(id, Watching::Proc(program));
        }
        drop(state);
        Ok(Self {
            shared,
            id,
            mail,
            mine: false,
            answer: None,
            answered: false,
        })
    }

    /// What came to its mailbox: the reader's turn, its answer.
    fn collect(&mut self) {
        while let Ok(mail) = self.mail.try_recv() {
            self.take(mail);
        }
    }

    fn take(&mut self, mail: Mail) {
        match mail {
            Mail::Turn => self.mine = true,
            Mail::Answer(answer) => self.answer = Some(*answer),
        }
    }

    /// Whether it can take the reader's turn from now: its send under way with the send lock held, or its request
    /// sent and its answer awaited.
    fn reads(&self) -> Result<(), Refusal> {
        if let Some(waiting) = self.shared.state()?.calls.get_mut(&self.id) {
            waiting.reads = true;
        }
        Ok(())
    }

    /// Sends `bytes` whole with `lent`, under the send lock, waiting for room in the agent's socket until `deadline`
    /// (none: as long as it takes), its call's; `patience`, its learned wait, is named in a refusal. The node agent
    /// reads no request while its answers wait for room in the client's socket, so a send waiting for room has
    /// someone reading: itself while it holds the reader's turn, which it takes when free or is handed, reading what
    /// comes as it waits; else the turn's holder, which hands the turn to it, waking it, as it goes.
    fn send(
        &mut self,
        bytes: &[u8],
        lent: &[Lent<'_>],
        deadline: Option<Instant>,
        patience: Option<Duration>,
    ) -> Result<(), Sent> {
        let shared = self.shared;
        let no_room = || Refusal::Os {
            op: "sending to the node agent",
            why: format!(
                "no room in its socket for {:?}, twice the slowest answer to a call of its kind: it isn't reading",
                patience.unwrap_or_default()
            ),
        };
        let whole = shared
            .sending
            .lock(deadline)
            .map_err(Sent::Refused)?
            .ok_or_else(|| Sent::Full(no_room()))?;
        self.reads().map_err(Sent::Refused)?;
        let mut at = 0;
        // Made the first time the send waits with the turn another's, so a send that finds room pays nothing for it.
        let mut woken: Option<Arc<Wake>> = None;
        let sent = loop {
            if deadline.is_some_and(|at| at <= Instant::now()) {
                break Err(Sent::Full(no_room()));
            }
            // What is owed goes first, whole, before this message's first byte.
            let owed = if at == 0 {
                shared.send_owed(&whole)
            } else {
                Ok(true)
            };
            match owed {
                Ok(true) => match shared.transport.try_send(bytes, &mut at, lent) {
                    Ok(true) => break Ok(()),
                    Ok(false) => {}
                    Err(e) => break Err(e),
                },
                Ok(false) => {}
                Err(e) => break Err(e),
            }
            let left = deadline.map(|d| d.saturating_duration_since(Instant::now()));
            if left.is_some_and(|l| l.is_zero()) {
                break Err(Sent::Full(no_room()));
            }
            if let Err(e) = self.wait_for_room(&mut woken, left) {
                break Err(e);
            }
        };
        if woken.is_some()
            && let Ok(mut state) = shared.state()
            && let Some(waiting) = state.calls.get_mut(&self.id)
        {
            waiting.wake = None;
        }
        // A message cut short on a stream leaves the connection unable to carry another: it goes.
        if sent.is_err() && at > 0 {
            shared.transport.shutdown();
        }
        // What came owed while it sent goes now if it can; the rest is the next sender's, or the reader's.
        if sent.is_ok() {
            let _ = shared.send_owed(&whole);
        }
        sent
    }

    /// One wait for room for its send, at most `left` (none: as long as it takes): reading what the agent sends as
    /// it waits while it holds the reader's turn, which it takes when free or is handed; else waiting on the
    /// connection for room and on its wake (`woken`, made once and kept with the call, where the holder handing the
    /// turn on finds it) for the turn. The connection's end is [`Sent::Gone`], its why the call's answer.
    fn wait_for_room(
        &mut self,
        woken: &mut Option<Arc<Wake>>,
        left: Option<Duration>,
    ) -> Result<(), Sent> {
        let shared = self.shared;
        let wake = {
            let mut state = shared.state().map_err(Sent::Refused)?;
            self.collect();
            if self.answer.is_some() || state.closed.is_some() {
                return Err(Sent::Gone);
            }
            if !self.mine && !state.reading {
                state.reading = true;
                self.mine = true;
            }
            if self.mine {
                None
            } else {
                let wake = match woken {
                    Some(wake) => Arc::clone(wake),
                    None => {
                        let made = Wake::new().map_err(|e| {
                            Sent::Refused(Refusal::os(
                                "making the wake a send waits on for the reader's turn",
                                e,
                            ))
                        })?;
                        Arc::clone(woken.insert(Arc::new(made)))
                    }
                };
                if let Some(waiting) = state.calls.get_mut(&self.id) {
                    waiting.wake = Some(Arc::clone(&wake));
                }
                Some(wake)
            }
        };
        let handle = shared.transport.handle();
        match wake {
            // The turn its: what the agent sends read as it comes, until there's room.
            None => {
                let got =
                    ready(handle, true, None, left).map_err(|e| Sent::Refused(shared.end(e)))?;
                if got.read && shared.recv().is_err() {
                    // The connection ended: every call waiting was told why, this one among them.
                    self.mine = false;
                    return Err(Sent::Gone);
                }
            }
            // The turn another's: room waited for, and the turn, should it be handed to this call.
            Some(wake) => {
                let got = ready(handle, true, Some(&wake), left)
                    .map_err(|e| Sent::Refused(shared.end(e)))?;
                if got.woken {
                    wake.drain();
                }
            }
        }
        Ok(())
    }

    /// Its answer, waited for until `deadline` (none: as long as it takes): read by this thread while it holds the
    /// reader's turn, taken when free or handed to it, else handed to it by the thread that holds the turn (or the
    /// event form's loop). At the deadline, an already queued complete answer still wins; no further wait begins.
    fn wait(&mut self, deadline: Option<Instant>) -> Option<Result<Answer, Refusal>> {
        let shared = self.shared;
        if let Err(why) = self.reads() {
            return Some(Err(why));
        }
        loop {
            self.collect();
            if let Some(answer) = self.answer.take() {
                self.answered = answer.is_ok();
                return Some(answer);
            }
            if !self.mine {
                match shared.state() {
                    Ok(mut state) => {
                        if !state.reading && state.closed.is_none() {
                            state.reading = true;
                            self.mine = true;
                        }
                    }
                    Err(why) => return Some(Err(why)),
                }
            }
            let left = deadline.map(|d| d.saturating_duration_since(Instant::now()));
            if left.is_some_and(|l| l.is_zero()) {
                // AC-T7.4.1: scheduling can resume after the deadline with the reply already queued.
                // Match mailbox precedence without extending the wait or accepting an incomplete frame.
                self.collect();
                if self.answer.is_none() && self.mine {
                    if shared.turn(Some(Duration::ZERO)).is_err() {
                        self.mine = false;
                    }
                    self.collect();
                }
                if let Some(answer) = self.answer.take() {
                    self.answered = answer.is_ok();
                    return Some(answer);
                }
                return None;
            }
            if self.mine {
                if shared.turn(left).is_err() {
                    // The connection ended: every call waiting was told why, this one among them.
                    self.mine = false;
                }
            } else {
                match self.mail.recv_timeout(left.unwrap_or(Duration::MAX)) {
                    Ok(mail) => self.take(mail),
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => {
                        return Some(Err(Refusal::Broken(
                            "the node agent's answer was lost".to_owned(),
                        )));
                    }
                }
            }
        }
    }

    /// Its answer, waited for until `deadline` as [`Caller::wait`] waits; where `learned`, its kind's learned wait
    /// under its caller's deadline, ends first with none come, the run-out is told to `waits` as it passes, once, and
    /// the wait goes on (decision 9, AC-T7.4.1).
    fn wait_past(
        &mut self,
        learned: &mut Option<ReplyDeadline>,
        deadline: Option<Instant>,
        waits: Option<&Patience>,
    ) -> Option<Result<Answer, Refusal>> {
        if let Some(bound) = *learned {
            if let Some(answer) = self.wait(Some(bound.at)) {
                return Some(answer);
            }
            *learned = None;
            bound.ran_out(waits);
        }
        self.wait(deadline)
    }
}

impl<T: Transport> Drop for Caller<'_, T> {
    /// Going: the call forgotten if unanswered, its streams' ends with it, and the reader's turn it holds, or was
    /// handed as it went, handed on.
    fn drop(&mut self) {
        let Ok(mut state) = self.shared.state() else {
            return;
        };
        if !self.answered {
            state.calls.remove(&self.id);
            state.programs.remove(&self.id);
            state.given.remove(&self.id);
        }
        while let Ok(mail) = self.mail.try_recv() {
            if matches!(mail, Mail::Turn) {
                self.mine = true;
            }
        }
        if self.mine && state.closed.is_none() {
            state.pass_turn();
        }
    }
}

/// The connection, held by the client and by each `Proc` it returned: it closes when the last goes.
pub(crate) struct Conn<T: Transport> {
    shared: SharedOwner<T>,
}

impl<T: Transport> Drop for Conn<T> {
    fn drop(&mut self) {
        // The agent sees the hang-up.
        self.shared.transport.shutdown();
    }
}

impl<T: Transport> Conn<T> {
    /// Sends `request` with the descriptors `with` lends it, once `with` has made what its id names (an exec's
    /// streams its transport makes), and waits for its answer: with no deadline of its caller's, at most twice the
    /// slowest answer this process has had to a call of its kind ([`Transport::answers`]), past which the call is
    /// refused by type, the only way left to it, and as long as it takes while its kind's wait measures; with one,
    /// until that deadline, its kind's learned wait a run-out recorded as it passes ([`ReplyDeadline::combine`]). With
    /// `program`, a `Proc`'s place for the end of the program an exec starts is made before the exec goes, so the end
    /// can't come first.
    fn call<'l>(
        &self,
        bound: usize,
        request: &Request,
        with: impl FnOnce(u64) -> Result<&'l [Lent<'l>], Refusal>,
        program: Option<Arc<Ending>>,
    ) -> Result<(u64, Answer), Refusal> {
        let caller = ReplyDeadline::caller(self.shared.transport.reply_budget())?;
        self.call_before(bound, request, |id, _| with(id), program, caller)
    }

    fn call_before<'l>(
        &self,
        bound: usize,
        request: &Request,
        with: impl FnOnce(u64, Option<ReplyDeadline>) -> Result<&'l [Lent<'l>], Refusal>,
        program: Option<Arc<Ending>>,
        cap: Option<ReplyDeadline>,
    ) -> Result<(u64, Answer), Refusal> {
        let shared = &*self.shared;
        if let Some(cap) = cap {
            cap.remaining("starting a client operation")?;
        }
        let wait = shared.answer_wait(request);
        let waits = wait.learned();
        let patience = wait.deadline();
        let asked = Instant::now();
        let (budget, mut learned) = ReplyDeadline::combine(cap, asked, patience);
        let deadline = budget.map(ReplyDeadline::at);
        let mut caller = Caller::new(shared, program)?;
        let bytes = wire::request(caller.id, request, bound)
            .map_err(Refusal::from)
            .and_then(T::frame)?;
        // Legacy transports retain their existing setup path. An explicit cap requires an adapter
        // that consumes the caller's deadline, the call's, before any request is sent.
        let lent = with(caller.id, cap)?;
        let mut pending = None;
        match caller.send(&bytes, lent, deadline, patience) {
            // Sent, or the agent has gone: why reaches the call as it waits, which sees the end.
            Ok(()) | Err(Sent::Gone) => {}
            // No room for the request until the call's deadline: it ran out, as an answer's wait would.
            Err(Sent::Full(r)) => {
                pending = caller.wait(Some(Instant::now()));
                if pending.is_none() {
                    ReplyDeadline::ended(budget, learned.take(), waits);
                    return Err(budget.map_or(r, |d| d.refusal("sending to the node agent")));
                }
            }
            Err(Sent::Refused(r)) => return Err(r),
        }
        let mut offered = false;
        loop {
            match pending
                .take()
                .or_else(|| caller.wait_past(&mut learned, deadline, waits))
            {
                Some(Ok(Answer::Offered(offer))) => {
                    offered = true;
                    caller.answered = false;
                    // No read turn is held across a guest dial: other calls and program ends keep progressing.
                    {
                        let mut state = shared.state()?;
                        // A reader may have handed us its turn after the offer reached this mailbox.
                        caller.collect();
                        if let Some(waiting) = state.calls.get_mut(&caller.id) {
                            waiting.reads = false;
                        }
                        if caller.mine {
                            state.pass_turn();
                            caller.mine = false;
                        }
                    }
                    let opened = match cap {
                        Some(deadline) => deadline
                            .remaining("opening offered streams")
                            .and_then(|_| shared.transport.open_offer_before(&offer, deadline)),
                        None => shared.transport.open_offer(&offer),
                    };
                    // A guest may have refused/ended while this caller dialed without holding the turn.
                    caller.collect();
                    if caller.answer.is_some() {
                        drop(opened);
                        continue;
                    }
                    let given = match opened {
                        Ok(given) => given,
                        Err(why) => {
                            shared.owe(caller.id, &Request::Release)?;
                            shared.send_owed_now();
                            return Err(why);
                        }
                    };
                    shared.state()?.given.insert(caller.id, given);
                    let bytes = wire::request(caller.id, &Request::StreamsReady, bound)
                        .map_err(Refusal::from)
                        .and_then(T::frame)?;
                    if let Err(why) = caller.send(&bytes, NONE, deadline, patience) {
                        pending = caller.wait(Some(Instant::now()));
                        if pending.is_some() {
                            continue;
                        }
                        shared.owe(caller.id, &Request::Release)?;
                        shared.send_owed_now();
                        return Err(match why {
                            Sent::Full(why) => {
                                ReplyDeadline::ended(budget, learned.take(), waits);
                                budget.map_or(why, |d| d.refusal("accepting offered streams"))
                            }
                            Sent::Refused(why) => why,
                            Sent::Gone => Refusal::Broken(
                                "the node ended before accepting the stream offer".to_owned(),
                            ),
                        });
                    }
                }
                Some(Ok(answer)) => {
                    if let Some(waits) = waits {
                        let took = asked.elapsed();
                        if matches!(&answer, Answer::Refused(_)) {
                            waits.refused(took);
                        } else {
                            waits.answered(took);
                        }
                    }
                    if let Ok(mut known) = shared.known.lock() {
                        known.learn(request, caller.id, &answer);
                    }
                    return Ok((caller.id, answer));
                }
                Some(Err(why)) => return Err(why),
                None => {
                    if offered {
                        shared.owe(caller.id, &Request::Release)?;
                        shared.send_owed_now();
                    }
                    ReplyDeadline::ended(budget, learned.take(), waits);
                    return Err(budget.map_or_else(
                        || {
                            Refusal::Broken(
                                "the node agent's unbounded answer wait ended".to_owned(),
                            )
                        },
                        |d| d.refusal("waiting for the node agent's answer"),
                    ));
                }
            }
        }
    }

    /// Sends one message that has no answer (a release) now or not at all: a release that finds no room is the
    /// agent's disconnect rule's to settle, so dropping a `Proc` never waits on the agent. One cut short on a stream
    /// has its rest owed ahead of every word ([`Owed`]) before the send lock is let go, so the next holder writes it
    /// first and nothing comes between its bytes. One whose rest can't be owed (the words at their bound, or a write
    /// that failed part way) leaves the stream unable to carry another message, and the connection goes.
    fn send_now(&self, bytes: &[u8]) {
        self.send_now_before(bytes, None);
    }

    fn send_now_before(&self, bytes: &[u8], cap: Option<ReplyDeadline>) {
        let shared = &*self.shared;
        // A bounded run retains its original authority through Drop. Starting another relative
        // budget here could strand its caller after the operation had already expired.
        let budget = match cap {
            Some(cap) => Some(cap),
            None => match ReplyDeadline::caller(shared.transport.reply_budget()) {
                Ok(budget) => budget,
                Err(_) => return,
            },
        };
        if cap.is_some_and(|cap| cap.remaining("releasing a program").is_err()) {
            return;
        }
        let Ok(framed) = T::frame(bytes.to_vec()) else {
            return;
        };
        let Ok(Some(whole)) = shared.sending.lock(budget.map(ReplyDeadline::at)) else {
            return;
        };
        if cap.is_some_and(|cap| cap.remaining("releasing a program").is_err()) {
            return;
        }
        // What is owed goes first: if it can't go now, neither can this, which comes after it.
        if !matches!(shared.send_owed(&whole), Ok(true)) {
            return;
        }
        let mut at = 0;
        let sent = shared.transport.try_send(&framed, &mut at, NONE);
        if at == 0 || matches!(sent, Ok(true)) {
            return;
        }
        let queued = sent.is_ok()
            && shared.owed.lock().is_ok_and(|mut owed| {
                let room = owed.words.len() < owed.bound;
                if room {
                    owed.words
                        .push_front(framed.get(at..).unwrap_or_default().to_vec());
                }
                room
            });
        if !queued {
            shared.transport.shutdown();
        }
        // Only now, its rest owed first or the connection ended, can another holder write.
        drop(whole);
    }
}

/// A node agent, reached through a transport.
pub struct Core<T: Transport> {
    conn: triomphe::Arc<Conn<T>>,
    classes: Vec<Class>,
    limits: Limits,
    budgets: wire::Budgets,
}

/// An event-form call whose id and completions are reserved, but whose request has not gone (HEX-RT-17).
/// The caller may first open guest streams named for [`Deferred::id`], then send the explicit call. Dropping it
/// before a successful send cancels only its reservation; it sends no request or release and makes no stream.
#[must_use = "a deferred call sends nothing until send is called"]
pub struct Deferred<T: Transport> {
    conn: triomphe::Arc<Conn<T>>,
    id: u64,
    bytes: Vec<u8>,
    reserved: bool,
}

#[cfg(test)]
mod tests;

#[cfg(test)]
#[path = "calls/budget_tests.rs"]
mod budget_tests;

impl<T: Transport> Deferred<T> {
    /// The request's reserved id, also the name of the program an exec starts.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Sends the framed call now, through the same event-form path as [`Core::begin`]. A failure cancels its
    /// reservation; a successful send leaves its answer and program's end to the existing reader's loop.
    pub fn send(self) -> Result<(), Refusal> {
        self.send_with(NONE)
    }

    fn send_with(self, lent: &[Lent<'_>]) -> Result<(), Refusal> {
        let deadline = ReplyDeadline::caller(self.conn.shared.transport.reply_budget())?;
        self.send_before(lent, deadline)
    }

    fn send_before(
        mut self,
        lent: &[Lent<'_>],
        deadline: Option<ReplyDeadline>,
    ) -> Result<(), Refusal> {
        let shared = &*self.conn.shared;
        if let Some(deadline) = deadline {
            deadline.remaining("sending an event-form call")?;
        }
        if let Some(why) = &shared.state()?.closed {
            return Err(why.clone());
        }
        let whole = shared
            .sending
            .lock(deadline.map(ReplyDeadline::at))?
            .ok_or_else(|| match deadline {
                Some(deadline) => deadline.refusal("queuing an event-form call"),
                None => Refusal::Broken("an unbounded send lock wait ended".to_owned()),
            })?;
        if let Some(deadline) = deadline {
            deadline.remaining("sending an event-form call")?;
        }
        let mut at = 0;
        // What is owed goes first, as it does before any message.
        let sent = match shared.send_owed(&whole) {
            Ok(true) => shared.transport.try_send(&self.bytes, &mut at, lent),
            Ok(false) => Ok(false),
            Err(e) => Err(e),
        };
        match sent {
            // The agent has gone: why reaches the completion through the loop, which sees the end.
            Ok(true) | Err(Sent::Gone) => {
                self.reserved = false;
                Ok(())
            }
            Ok(false) => {
                if at > 0 {
                    shared.transport.shutdown();
                }
                Err(Refusal::Os {
                    op: "sending to the node agent",
                    why: "no room in its connection now, and the event form doesn't wait"
                        .to_owned(),
                })
            }
            Err(Sent::Full(r) | Sent::Refused(r)) => Err(r),
        }
    }
}

impl<T: Transport> Drop for Deferred<T> {
    fn drop(&mut self) {
        if self.reserved
            && let Ok(mut state) = self.conn.shared.state()
        {
            state.calls.remove(&self.id);
            state.programs.remove(&self.id);
            state.given.remove(&self.id);
        }
    }
}

impl<T: Transport> Core<T> {
    /// Says hello over `shared`, naming this build, and learns what the agent runs and its bounds. An agent of
    /// another wire is refused here, by type, naming both wires and both builds (`hex_core::wire::Fault::Wire`):
    /// two sides talk only when built from the same wire.
    pub(crate) fn hello(shared: SharedOwner<T>) -> Result<Self, Refusal> {
        let deadline = ReplyDeadline::caller(shared.transport.reply_budget())?;
        Self::hello_before(shared, deadline)
    }

    pub(crate) fn hello_before(
        shared: SharedOwner<T>,
        deadline: Option<ReplyDeadline>,
    ) -> Result<Self, Refusal> {
        let client = Self {
            conn: triomphe::Arc::try_new(Conn { shared }).map_err(|_| Refusal::Exhausted {
                what: "client connection owner",
                bound: std::mem::size_of::<Conn<T>>(),
            })?,
            classes: Vec::new(),
            limits: Limits {
                connections: 1,
                message_bytes: wire::MAX_MESSAGE,
                in_flight: 1,
                programs: 0,
            },
            budgets: wire::Budgets {
                in_flight: 1,
                programs: 0,
            },
        };
        let hello = Request::Hello {
            build: wire::build(),
        };
        let answer = client
            .conn
            .call_before(
                client.limits.message_bytes,
                &hello,
                |_, _| Ok(NONE),
                None,
                deadline,
            )?
            .1;
        client.greeted(answer)
    }

    /// Whether its connection has ended: no call it sends is answered.
    pub fn closed(&self) -> bool {
        self.conn.shared.closed()
    }

    /// This client, told by the hello's `answer` what its agent runs and its bounds.
    fn greeted(mut self, answer: Answer) -> Result<Self, Refusal> {
        match answer {
            Answer::Hello(hello) => {
                self.classes = hello.classes;
                self.limits = Limits {
                    message_bytes: hello.limits.message_bytes.min(wire::MAX_MESSAGE),
                    ..hello.limits
                };
                self.conn.shared.owed.lock().map_err(poisoned)?.bound = hello.budgets.programs;
                let mut known = self.conn.shared.known.lock().map_err(poisoned)?;
                known.agent.clone_from(&self.classes);
                known.programs_bound = hello.budgets.programs;
                drop(known);
                self.budgets = hello.budgets;
                Ok(self)
            }
            Answer::Refused(r) => Err(r),
            other => Err(unexpected("hello", &other)),
        }
    }

    /// The event form of a call: sends `call`, an exec's with `stdio`, now, and returns its request's id at once.
    /// `then` gets its answer on the thread that reads the connection (the event form's loop), and an exec's program's
    /// end goes to `ended` once its start has come; the ends of the streams it asked for are taken with
    /// [`Core::given`]. The event form copies no stream: where the transport makes an exec's streams (a guest's), each
    /// is asked for, and a pipe of the caller's own is refused. A call that can't be sent now is refused here, and
    /// `then` never runs.
    pub fn begin(
        &self,
        call: Call,
        stdio: Stdio,
        then: Then,
        ended: Ended,
    ) -> Result<u64, Refusal> {
        let deadline = ReplyDeadline::caller(self.conn.shared.transport.reply_budget())?;
        let (launch, program) = match call {
            Call::Exec { id, program, .. } | Call::ExecStreams { id, program, .. } => {
                (Launch::Exec(id), program)
            }
            Call::Run { spec, program, .. } => (Launch::Run(spec), program),
            call => {
                return self.begun(&Request::Call(call), |_| Ok(NONE), then, None, deadline);
            }
        };
        stdio.check()?;
        let (carried, asked) = (carried(&stdio), streams(stdio.ask));
        let (request, made) = if T::MAKES_ASKED {
            if carried != Streams::default() {
                return Err(Refusal::Invalid {
                    what: "stdio",
                    why: "the event form copies no stream: ask for each of a guest program's streams, whose ends are the caller's".to_owned(),
                });
            }
            (launch.call(program, asked, Streams::default()), asked)
        } else {
            (launch.call(program, carried, asked), Streams::default())
        };
        let lent = lend(&stdio);
        let shared = &*self.conn.shared;
        self.begun(
            &Request::Call(request),
            |exec| {
                make(shared, exec, made, deadline)?;
                Ok(&lent[..])
            },
            then,
            Some(Watching::Then(ended)),
            deadline,
        )
    }

    /// Reserves an event-form call's id, answer and (for an exec or run) program's end before sending it. The
    /// explicit `call` is framed unchanged: no stdio is made, copied or lent, and [`Transport::MAKES_ASKED`] is
    /// not used. A guest caller that has arranged its own named streams uses [`Call::Exec`] with their stream
    /// bits, then calls [`Deferred::send`] only once those connections are open. Until then, drop cancels it.
    pub fn defer(&self, call: Call, then: Then, ended: Ended) -> Result<Deferred<T>, Refusal> {
        let program = matches!(
            call,
            Call::Exec { .. } | Call::ExecStreams { .. } | Call::Run { .. }
        )
        .then(|| Watching::Then(ended));
        self.deferred(&Request::Call(call), then, program)
    }

    /// Sends `request` now with what `with` makes for its id and lends it, its answer to go to `then` and an exec's
    /// program's end to `program`, each placed before the request goes; refused, and forgotten, when it can't go now.
    fn begun<'l>(
        &self,
        request: &Request,
        with: impl FnOnce(u64) -> Result<&'l [Lent<'l>], Refusal>,
        then: Then,
        program: Option<Watching>,
        deadline: Option<ReplyDeadline>,
    ) -> Result<u64, Refusal> {
        let deferred = self.deferred(request, then, program)?;
        let id = deferred.id();
        let lent = with(id)?;
        deferred.send_before(lent, deadline)?;
        Ok(id)
    }

    /// The common event-form reservation and framing path. The RAII value forgets its id if framing or a later
    /// stream preparation fails before send, just as it does when a deferred caller cancels it.
    fn deferred(
        &self,
        request: &Request,
        then: Then,
        program: Option<Watching>,
    ) -> Result<Deferred<T>, Refusal> {
        let shared = &*self.conn.shared;
        let id = {
            let mut state = shared.state()?;
            if let Some(why) = &state.closed {
                return Err(why.clone());
            }
            let id = state
                .next
                .checked_add(1)
                .ok_or_else(|| Refusal::Broken("the client's request ids ran out".to_owned()))?;
            state.next = id;
            state.calls.insert(
                id,
                Waiting {
                    answer: Taker::Then(then),
                    offered: false,
                    boxes: Vec::new(),
                    reads: false,
                    wake: None,
                },
            );
            if let Some(program) = program {
                state.programs.insert(id, program);
            }
            id
        };
        let mut deferred = Deferred {
            conn: triomphe::Arc::clone(&self.conn),
            id,
            bytes: Vec::new(),
            reserved: true,
        };
        deferred.bytes = wire::request(id, request, self.limits.message_bytes)
            .map_err(Refusal::from)
            .and_then(T::frame)?;
        Ok(deferred)
    }

    /// The ends of exec `exec`'s streams once its start has come, taken once: those its transport made (a
    /// guest's connections) or its agent's answer gave. The event form's completion ([`Then`]) takes them
    /// here; `None` when there are none, or they were taken.
    pub fn given(&self, exec: u64) -> Option<hex_api::Given> {
        self.conn.shared.state().ok()?.given.remove(&exec)
    }

    /// The address and names of a reserved guest exec's streams; no stream is opened until its caller does so.
    pub fn stream_offer(&self, exec: u64, streams: Streams) -> Result<wire::StreamOffer, Refusal> {
        {
            let state = self.conn.shared.state()?;
            if !state.calls.contains_key(&exec) || !state.programs.contains_key(&exec) {
                return Err(Refusal::Invalid {
                    what: "stream offer",
                    why: "no reserved exec has that id".to_owned(),
                });
            }
        }
        self.conn.shared.transport.stream_offer(exec, streams)
    }

    /// The event form's release of exec `exec`'s program: its end is no longer taken, and the agent lets
    /// it run on, as dropping an in-process `Proc` does. One the connection can't carry needs none: the
    /// agent's disconnect rule has it.
    pub fn release(&self, exec: u64) {
        if let Ok(mut state) = self.conn.shared.state() {
            state.programs.remove(&exec);
        }
        if let Ok(bytes) = wire::request(exec, &Request::Release, self.limits.message_bytes) {
            self.conn.send_now(&bytes);
        }
    }

    /// The event form's kill of exec `exec`'s program: `then` hears whether the agent sent it.
    pub fn kill_then(&self, exec: u64, then: Then) -> Result<u64, Refusal> {
        self.begin(Call::Kill { exec }, Stdio::null(), then, Box::new(|_| {}))
    }

    /// The agent's bounds on this connection and its socket, as its hello told them.
    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// The agent's budgets over all its connections together, as its hello told them.
    pub fn budgets(&self) -> wire::Budgets {
        self.budgets
    }

    /// The agent's network namespace counts (decision 4), as `hex_node::Node::netns` reads them in
    /// process.
    pub fn netns(&self) -> Result<netns::Counts, Refusal> {
        match self
            .call(&Request::Call(Call::Netns), |_| Ok(NONE), None)?
            .1
        {
            Answer::Netns(counts) => Ok(counts),
            Answer::Refused(r) => Err(r),
            other => Err(unexpected("netns", &other)),
        }
    }

    /// The agent's network namespace counts within the caller's original absolute deadline, its kind's learned wait
    /// only a run-out recorded (AC-T7.4.1).
    pub fn netns_before(&self, deadline: Instant) -> Result<netns::Counts, Refusal> {
        match self.plain_before(Call::Netns, deadline)?.1 {
            Answer::Netns(counts) => Ok(counts),
            Answer::Refused(r) => Err(r),
            other => Err(unexpected("netns", &other)),
        }
    }

    /// Sends `request` on the connection with what `with` makes for its id and lends it, and waits for its
    /// answer.
    fn call<'l>(
        &self,
        request: &Request,
        with: impl FnOnce(u64) -> Result<&'l [Lent<'l>], Refusal>,
        program: Option<Arc<Ending>>,
    ) -> Result<(u64, Answer), Refusal> {
        self.conn
            .call(self.limits.message_bytes, request, with, program)
    }

    fn call_before<'l>(
        &self,
        request: &Request,
        with: impl FnOnce(u64) -> Result<&'l [Lent<'l>], Refusal>,
        program: Option<Arc<Ending>>,
        cap: ReplyDeadline,
    ) -> Result<(u64, Answer), Refusal> {
        let configured = ReplyDeadline::caller(self.conn.shared.transport.reply_budget())?;
        self.conn.call_before(
            self.limits.message_bytes,
            request,
            |exec, _| with(exec),
            program,
            Some(cap.limited_by(configured)),
        )
    }

    /// Run with one original absolute deadline for control send/answer and stream setup.
    /// A smaller configured bound still wins; a learned one ending first is a run-out recorded,
    /// not the call's end ([`ReplyDeadline::combine`]). The returned program's lifetime and
    /// caller-owned stdio have no deadline attached; use [`Proc::wait_for`] for completion.
    /// An expired call does not prove whether the agent started or removed the workload.
    pub fn run_before(
        &self,
        spec: BoxSpec,
        program: Program,
        stdio: Stdio,
        deadline: Instant,
    ) -> Result<Box<dyn Proc>, Refusal> {
        let cap = ReplyDeadline::caller_before(deadline)?;
        self.run_with_cap(spec, program, stdio, Some(cap))
    }

    /// Delete within the caller's original absolute operation deadline on this SAME connection.
    /// A refusal or timeout leaves cleanup uncertain; it is never physical-absence evidence.
    pub fn delete_before(&self, id: &BoxId, deadline: Instant) -> Result<(), Refusal> {
        let cap = ReplyDeadline::caller_before(deadline)?;
        let request = Request::Call(Call::Delete(id.try_clone()?));
        match self.call_before(&request, |_| Ok(NONE), None, cap)?.1 {
            Answer::Done => Ok(()),
            Answer::Refused(why) => Err(why),
            other => Err(unexpected("delete", &other)),
        }
    }

    /// Reads capacity within the caller's original operation deadline on this connection.
    /// A smaller configured wait still wins, a learned one only a run-out recorded; telemetry
    /// grants no ownership.
    pub fn capacity_before(
        &self,
        class: Class,
        deadline: Instant,
    ) -> Result<Option<hex_api::Capacity>, Refusal> {
        let (cap, answer) = self.plain_before(Call::Capacity(class), deadline)?;
        match answer {
            Answer::Capacity(capacity) => {
                if let Some(capacity) = &capacity {
                    capacity.check()?;
                }
                cap.remaining("returning the agent's capacity")?;
                Ok(capacity)
            }
            Answer::Refused(why) => Err(why),
            other => Err(unexpected("capacity", &other)),
        }
    }

    /// Reads the same native physical report within the original operation deadline.
    /// It retains the ordinary report validation and conveys no holder authority.
    pub fn physical_report_before(
        &self,
        deadline: Instant,
    ) -> Result<hex_api::PhysicalReport, Refusal> {
        let (cap, answer) = self.plain_before(Call::PhysicalReport, deadline)?;
        match answer {
            Answer::PhysicalReport(report) => {
                report.check()?;
                cap.remaining("returning the native physical report")?;
                Ok(report)
            }
            Answer::Refused(why) => Err(why),
            other => Err(unexpected("native physical report", &other)),
        }
    }

    /// Creates within the caller's original operation deadline on this connection.
    /// Caller timeout leaves receiver-side creation uncertain: it proves neither that
    /// creation stopped nor that the workload never ran.
    pub fn create_before(&self, spec: BoxSpec, deadline: Instant) -> Result<BoxInfo, Refusal> {
        let cap = ReplyDeadline::caller_before(deadline)?;
        self.create_with_cap(spec, Some(cap))
    }

    fn create_with_cap(
        &self,
        spec: BoxSpec,
        cap: Option<ReplyDeadline>,
    ) -> Result<BoxInfo, Refusal> {
        if let hex_api::ClassSpec::Oci(oci) = &spec.class {
            oci.init.fits(&WIRE_EXEC_LIMIT)?;
        }
        let call = Call::Create(Box::new(spec));
        let Some(cap) = cap else {
            return self.info(call, "create");
        };
        match self
            .call_before(&Request::Call(call), |_| Ok(NONE), None, cap)?
            .1
        {
            Answer::Info(info) => Ok(info),
            Answer::Refused(why) => Err(why),
            other => Err(unexpected("create", &other)),
        }
    }

    /// CP-T7.4: scheduler control through this connection's existing bounded reader.
    pub fn schedule(
        &self,
        command: hex_core::schedule::Command,
    ) -> Result<hex_core::schedule::Reply, Refusal> {
        match self.plain(Call::Schedule(Box::new(command)))? {
            Answer::Scheduled(reply) => Ok(*reply),
            Answer::Refused(why) => Err(why),
            other => Err(unexpected("schedule", &other)),
        }
    }

    /// A call that carries nothing beside its message.
    fn plain(&self, call: Call) -> Result<Answer, Refusal> {
        Ok(self.call(&Request::Call(call), |_| Ok(NONE), None)?.1)
    }

    /// A metadata call retains the same effective caller/configured cap through validation.
    fn plain_before(
        &self,
        call: Call,
        deadline: Instant,
    ) -> Result<(ReplyDeadline, Answer), Refusal> {
        let cap = ReplyDeadline::caller_before(deadline)?.limited_by(ReplyDeadline::caller(
            self.conn.shared.transport.reply_budget(),
        )?);
        let answer = self
            .conn
            .call_before(
                self.limits.message_bytes,
                &Request::Call(call),
                |_, _| Ok(NONE),
                None,
                Some(cap),
            )?
            .1;
        Ok((cap, answer))
    }

    /// A call whose answer is `Done`.
    fn done(&self, call: Call, name: &'static str) -> Result<(), Refusal> {
        match self.plain(call)? {
            Answer::Done => Ok(()),
            Answer::Refused(r) => Err(r),
            other => Err(unexpected(name, &other)),
        }
    }

    /// A call whose answer is a box's info.
    fn info(&self, call: Call, name: &'static str) -> Result<BoxInfo, Refusal> {
        match self.plain(call)? {
            Answer::Info(info) => Ok(info),
            Answer::Refused(r) => Err(r),
            other => Err(unexpected(name, &other)),
        }
    }

    /// Starts `program` where `launch` says, an exec or a run, with `stdio` as [`Runtime::exec`] takes it: the
    /// program's `Proc`, whose end comes later as a message.
    fn spawn(
        &self,
        launch: Launch,
        program: Program,
        stdio: Stdio,
    ) -> Result<Box<dyn Proc>, Refusal> {
        self.spawn_before(launch, program, stdio, None)
    }

    fn run_with_cap(
        &self,
        spec: BoxSpec,
        program: Program,
        stdio: Stdio,
        cap: Option<ReplyDeadline>,
    ) -> Result<Box<dyn Proc>, Refusal> {
        self.spawn_before(Launch::Run(Box::new(spec)), program, stdio, cap)
    }

    fn spawn_before(
        &self,
        launch: Launch,
        program: Program,
        stdio: Stdio,
        caller: Option<ReplyDeadline>,
    ) -> Result<Box<dyn Proc>, Refusal> {
        if let Some(caller) = caller {
            caller.remaining("starting a program operation")?;
        }
        program.fits(&WIRE_EXEC_LIMIT)?;
        stdio.check()?;
        let name = launch.name();
        // What a failed setup names when its kill can't confirm the program's end; the call takes `launch`.
        let id = launch.id().try_clone()?;
        let (carried, asked) = (carried(&stdio), streams(stdio.ask));
        let ending = Arc::new(Ending::default());
        let shared = &*self.conn.shared;
        // Keep the operation's original effective caller/configured authority for Drop too.
        // Neither a later relative policy read nor a learned answer wait is its release budget.
        let caller = match caller {
            Some(caller) => {
                Some(caller.limited_by(ReplyDeadline::caller(shared.transport.reply_budget())?))
            }
            None => None,
        };
        let (exec, answer, kept) = if T::MAKES_ASKED {
            let request =
                Request::Call(launch.call(program, union(carried, asked), Streams::default()));
            let Stdio {
                stdin,
                stdout,
                stderr,
                ask,
            } = stdio;
            let pipes = Stdio {
                stdin,
                stdout,
                stderr,
                ..Stdio::null()
            };
            let cap = match caller {
                Some(caller) => Some(caller),
                None => ReplyDeadline::caller(shared.transport.reply_budget())?,
            };
            let (exec, answer) = self.conn.call_before(
                self.limits.message_bytes,
                &request,
                |exec, deadline| {
                    make(shared, exec, asked, deadline)?;
                    if let Some(deadline) = deadline {
                        deadline.remaining("opening carried streams")?;
                        shared.transport.carries_before(exec, pipes, deadline)?;
                    } else {
                        shared.transport.carries(exec, pipes)?;
                    }
                    Ok(NONE)
                },
                Some(Arc::clone(&ending)),
                cap,
            )?;
            let kept = Stdio {
                ask,
                ..Stdio::null()
            };
            (exec, answer, kept)
        } else {
            let request = Request::Call(launch.call(program, carried, asked));
            let (exec, answer) = {
                let lent = lend(&stdio);
                match caller {
                    Some(cap) => self.conn.call_before(
                        self.limits.message_bytes,
                        &request,
                        |_, _| Ok(&lent[..]),
                        Some(Arc::clone(&ending)),
                        Some(cap),
                    )?,
                    None => self.call(&request, |_| Ok(&lent[..]), Some(Arc::clone(&ending)))?,
                }
            };
            (exec, answer, stdio)
        };
        match answer {
            Answer::Started { pid } | Answer::Given { pid, .. } => {
                let ends = shared.state()?.given.remove(&exec).unwrap_or_default();
                let mut proc = RemoteProc {
                    conn: triomphe::Arc::clone(&self.conn),
                    bound: self.limits.message_bytes,
                    exec,
                    pid,
                    ending,
                    release_before: caller,
                    exit: None,
                    given: None,
                    kill_asked: false,
                };
                match settle(kept, ends) {
                    Ok(given) => {
                        proc.given = Some(given);
                        Ok(Box::new(proc))
                    }
                    Err(why) => {
                        // No caller will hold it, and unless its kill confirms its end it may still run:
                        // the refusal is then its box's uncertain lifecycle, not the setup's or the kill's.
                        proc.kill_asked = true;
                        let killed = match caller {
                            Some(cap) => proc.kill_before(cap),
                            None => proc.kill(),
                        };
                        Err(match killed {
                            Ok(()) => why,
                            Err(_) => Refusal::Lifecycle {
                                id,
                                cause: hex_api::LifecycleCause::Uncertain,
                            },
                        })
                    }
                }
            }
            other => {
                if let Ok(mut state) = shared.state() {
                    state.programs.remove(&exec);
                    state.given.remove(&exec);
                }
                match other {
                    Answer::Refused(r) => Err(r),
                    other => Err(unexpected(name, &other)),
                }
            }
        }
    }
}

/// Where a call that starts a program starts it: in a box there is (an exec), or in a `process` box made for its
/// life (a run, decision 7).
enum Launch {
    Exec(BoxId),
    Run(Box<BoxSpec>),
}

impl Launch {
    /// The call that starts `program` here, the streams `streams` names carried and those `ask` names asked for.
    fn call(self, program: Program, streams: Streams, ask: Streams) -> Call {
        match self {
            Self::Exec(id) if ask == Streams::default() => Call::Exec {
                id,
                program,
                streams,
            },
            Self::Exec(id) => Call::ExecStreams {
                id,
                program,
                streams,
                ask,
            },
            Self::Run(spec) => Call::Run {
                spec,
                program,
                streams,
                ask,
            },
        }
    }

    /// What its refusals name it.
    fn name(&self) -> &'static str {
        match self {
            Self::Exec(_) => "exec",
            Self::Run(_) => "run",
        }
    }

    /// The box it starts its program in.
    fn id(&self) -> &BoxId {
        match self {
            Self::Exec(id) => id,
            Self::Run(spec) => &spec.id,
        }
    }
}

/// Why `answer` can't be the answer to a `call`.
pub(crate) fn unexpected(call: &str, answer: &Answer) -> Refusal {
    let kind = match answer {
        Answer::Hello(_) => "a hello",
        Answer::Info(_) => "a box's info",
        Answer::Done => "done",
        Answer::Started { .. } => "a started program",
        Answer::Usage(_) => "a usage",
        Answer::Boxes { .. } => "a list",
        Answer::Refused(_) => "a refusal",
        Answer::Exited(_) => "a program's end",
        Answer::Netns(_) => "network namespace counts",
        Answer::Given { .. } => "a started program with streams",
        Answer::Offered(_) => "a guest stream preparation",
        Answer::Scheduled(_) => "a scheduler reply",
        Answer::Capacity(_) => "a runtime capacity",
        Answer::PhysicalReport(_) => "a native physical report",
        Answer::BackingOffer(_) => "a native backing receipt",
    };
    Refusal::Broken(format!("the node agent answered a {call} with {kind}"))
}

/// The most of a program's arguments and environment that crosses to the node agent: a program can't pass
/// the bound on one wire message (`hex_core::wire::MAX_MESSAGE`), so one that would is refused here, as a
/// program, before it is encoded; the node then holds it to its OS's own exec bound (HEX-RT-14).
pub const WIRE_EXEC_LIMIT: hex_api::ExecLimit = hex_api::ExecLimit {
    block: wire::MAX_MESSAGE,
    counts: hex_api::BlockCount::Execve,
    string: None,
    unit: hex_api::StringUnit::Bytes,
    source: "the node agent's wire (one message, hex_core::wire::MAX_MESSAGE)",
};

/// The streams an exec carries: the caller's own pipes.
fn carried(stdio: &Stdio) -> Streams {
    Streams {
        stdin: stdio.stdin.is_some(),
        stdout: stdio.stdout.is_some(),
        stderr: stdio.stderr.is_some(),
    }
}

/// The streams `ask` names.
fn streams(ask: hex_api::Ask) -> Streams {
    Streams {
        stdin: ask.stdin,
        stdout: ask.stdout,
        stderr: ask.stderr,
    }
}

/// The streams in `a` or `b`.
fn union(a: Streams, b: Streams) -> Streams {
    Streams {
        stdin: a.stdin || b.stdin,
        stdout: a.stdout || b.stdout,
        stderr: a.stderr || b.stderr,
    }
}

/// Makes the streams `made` names for exec `exec`, where its transport makes them (a guest's connections), kept
/// for its exec to take once its start comes; nothing when it names none.
fn make<T: Transport>(
    shared: &Shared<T>,
    exec: u64,
    made: Streams,
    deadline: Option<ReplyDeadline>,
) -> Result<(), Refusal> {
    if made == Streams::default() {
        return Ok(());
    }
    let ask = hex_api::Ask {
        stdin: made.stdin,
        stdout: made.stdout,
        stderr: made.stderr,
    };
    let given = match deadline {
        Some(deadline) => {
            deadline.remaining("opening asked streams")?;
            shared.transport.asks_before(exec, ask, deadline)?
        }
        None => shared.transport.asks(exec, ask)?,
    };
    shared.state()?.given.insert(exec, given);
    Ok(())
}

/// The descriptors of the pipes an exec carries, lent to its message, in the wire's order.
#[cfg(unix)]
fn lend(stdio: &Stdio) -> Vec<Lent<'_>> {
    use std::os::fd::AsFd;
    [
        stdio.stdin.as_ref().map(AsFd::as_fd),
        stdio.stdout.as_ref().map(AsFd::as_fd),
        stdio.stderr.as_ref().map(AsFd::as_fd),
    ]
    .into_iter()
    .flatten()
    .collect()
}

#[cfg(windows)]
fn lend(stdio: &Stdio) -> Vec<Lent<'_>> {
    use std::os::windows::io::AsHandle;
    [
        stdio.stdin.as_ref().map(AsHandle::as_handle),
        stdio.stdout.as_ref().map(AsHandle::as_handle),
        stdio.stderr.as_ref().map(AsHandle::as_handle),
    ]
    .into_iter()
    .flatten()
    .collect()
}

/// The ends an exec's start gave, each settled: a stream the caller carried as a pipe of its own is copied between
/// that pipe and its end, here in the caller's client (the one copy left, native plan step 8); a stream it asked
/// for is the caller's to take ([`Proc::streams`]). A carried pipe whose program took it has no end, and is let go
/// here. An end of a stream the exec neither carried nor asked for is the agent's fault, refused.
fn settle(stdio: Stdio, ends: hex_api::Given) -> Result<hex_api::Given, Refusal> {
    let Stdio {
        stdin,
        stdout,
        stderr,
        ask,
    } = stdio;
    let unasked = |name: &str| {
        Refusal::Broken(format!(
            "the node agent gave {name}'s end, which its exec neither carried nor asked for"
        ))
    };
    let mut given = hex_api::Given::default();
    match (stdin, ends.stdin) {
        (Some(pipe), Some(end)) => copy_in(pipe, hex_api::OwnedStream::from(end))?,
        (None, Some(end)) if ask.stdin => given.stdin = Some(end),
        (None, Some(_)) => return Err(unasked("stdin")),
        (_, None) => {}
    }
    for (pipe, end, asked, name, to) in [
        (stdout, ends.stdout, ask.stdout, "stdout", &mut given.stdout),
        (stderr, ends.stderr, ask.stderr, "stderr", &mut given.stderr),
    ] {
        match (pipe, end) {
            (Some(pipe), Some(end)) => copy(hex_api::OwnedStream::from(end), pipe, drop)?,
            (None, Some(end)) if asked => *to = Some(end),
            (None, Some(_)) => return Err(unasked(name)),
            (_, None) => {}
        }
    }
    Ok(given)
}

impl<T: Transport> Runtime for Core<T> {
    fn backing_usage(&self, key: &hex_api::BackingKey) -> Result<Usage, Refusal> {
        match self.plain(Call::BackingUsage(*key))? {
            Answer::Usage(usage) => Ok(usage),
            Answer::Refused(why) => Err(why),
            other => Err(unexpected("native backing usage", &other)),
        }
    }

    fn initialize_backing(
        &self,
        grant: hex_api::BackingGrant,
    ) -> Result<hex_api::BackingOffer, Refusal> {
        grant.check()?;
        let key = grant.key;
        let grant = triomphe::Arc::try_new(grant).map_err(|_| Refusal::Exhausted {
            what: "native backing grant",
            bound: size_of::<hex_api::BackingGrant>(),
        })?;
        match self.plain(Call::InitializeBacking(grant))? {
            Answer::BackingOffer(offer) if offer.key == key => {
                offer.allocation()?;
                Ok(offer)
            }
            Answer::Refused(why) => Err(why),
            _ => Err(Refusal::Backing {
                key,
                cause: hex_api::LifecycleCause::Uncertain,
            }),
        }
    }

    fn release_backing(&self, key: hex_api::BackingKey) -> Result<hex_api::BackingOffer, Refusal> {
        match self.plain(Call::ReleaseBacking(key))? {
            Answer::BackingOffer(offer) if offer.key == key => {
                offer.allocation()?;
                Ok(offer)
            }
            Answer::Refused(why) => Err(why),
            _ => Err(Refusal::Backing {
                key,
                cause: hex_api::LifecycleCause::Uncertain,
            }),
        }
    }

    fn physical_report(&self) -> Result<hex_api::PhysicalReport, Refusal> {
        match self.plain(Call::PhysicalReport)? {
            Answer::PhysicalReport(report) => {
                report.check()?;
                Ok(report)
            }
            Answer::Refused(why) => Err(why),
            other => Err(unexpected("native physical report", &other)),
        }
    }

    fn capacity(&self, class: Class) -> Result<Option<hex_api::Capacity>, Refusal> {
        match self.plain(Call::Capacity(class))? {
            Answer::Capacity(capacity) => {
                if let Some(capacity) = &capacity {
                    capacity.check()?;
                }
                Ok(capacity)
            }
            Answer::Refused(r) => Err(r),
            other => Err(unexpected("capacity", &other)),
        }
    }

    fn classes(&self) -> Vec<Class> {
        self.classes.clone()
    }

    fn create(&self, spec: BoxSpec) -> Result<BoxInfo, Refusal> {
        self.create_with_cap(spec, None)
    }

    fn start(&self, id: &BoxId) -> Result<(), Refusal> {
        self.done(Call::Start(id.clone()), "start")
    }

    /// The caller's own pipes ride with the exec as descriptors, which a local program takes as its streams. A
    /// stream asked for is made where the program's streams are: by the transport, where they are connections of
    /// its own (a guest's), or by the agent, which gives its end with its answer (the wire's
    /// [`Call::ExecStreams`]). A program that can't take the caller's own pipe (a guest's) has its stream's
    /// connection copied to and from that pipe here: by the transport where it makes the connection, or once the
    /// agent's answer gives its end. The one copy left is this client's, the caller's, never the node's (native
    /// plan step 8).
    fn exec(&self, id: &BoxId, program: Program, stdio: Stdio) -> Result<Box<dyn Proc>, Refusal> {
        self.spawn(Launch::Exec(id.clone()), program, stdio)
    }

    /// One request, [`Call::Run`], its stdio as an exec's; its program's end comes once the agent has removed the box.
    fn run(&self, spec: BoxSpec, program: Program, stdio: Stdio) -> Result<Box<dyn Proc>, Refusal> {
        self.run_with_cap(spec, program, stdio, None)
    }

    fn signal(&self, id: &BoxId, signal: Signal) -> Result<(), Refusal> {
        self.done(
            Call::Signal {
                id: id.clone(),
                signal,
            },
            "signal",
        )
    }

    fn inspect(&self, id: &BoxId) -> Result<BoxInfo, Refusal> {
        self.info(Call::Inspect(id.clone()), "inspect")
    }

    fn usage(&self, id: &BoxId) -> Result<Usage, Refusal> {
        match self.plain(Call::Usage(id.clone()))? {
            Answer::Usage(usage) => Ok(usage),
            Answer::Refused(r) => Err(r),
            other => Err(unexpected("usage", &other)),
        }
    }

    fn list(&self) -> Result<Vec<BoxInfo>, Refusal> {
        match self.plain(Call::List)? {
            Answer::Boxes { infos, .. } => Ok(infos),
            Answer::Refused(r) => Err(r),
            other => Err(unexpected("list", &other)),
        }
    }

    fn delete(&self, id: &BoxId) -> Result<(), Refusal> {
        self.done(Call::Delete(id.clone()), "delete")
    }
}

/// A program a node agent started for this client: its end comes as a message.
struct RemoteProc<T: Transport> {
    conn: triomphe::Arc<Conn<T>>,
    /// The agent's message bound, which its kill goes in.
    bound: usize,
    exec: u64,
    pid: u32,
    ending: Arc<Ending>,
    /// Explicit run-before's effective caller/configured cap, never workload completion.
    release_before: Option<ReplyDeadline>,
    exit: Option<Exit>,
    /// The caller's ends of the streams its exec asked for, until taken.
    given: Option<hex_api::Given>,
    /// Asked to end because its setup failed: never released, which tells the agent to let it run on.
    kill_asked: bool,
}

impl<T: Transport> RemoteProc<T> {
    /// Its end if it has come, waiting for it at most `timeout` (none or zero: not at all, but what has come read
    /// once): read by this wait itself while it holds the reader's turn, taken when free or handed to it, else
    /// handed to it by the thread that reads; the turn handed on as it goes.
    fn wait(&mut self, timeout: Option<Duration>) -> Result<Option<Exit>, Refusal> {
        self.wait_before(timeout, None)
    }

    fn wait_before(
        &mut self,
        timeout: Option<Duration>,
        cap: Option<ReplyDeadline>,
    ) -> Result<Option<Exit>, Refusal> {
        if let Some(exit) = self.exit {
            return Ok(Some(exit));
        }
        let shared = triomphe::Arc::clone(&self.conn.shared);
        let deadline = timeout.map(|t| Instant::now().checked_add(t));
        shared.state()?.waiting.insert(self.exec);
        let mut mine = false;
        // Every way out of the loop goes through its end, so the wait is never left registered.
        let got = loop {
            {
                let mut ended = match self.ending.state.lock() {
                    Ok(ended) => ended,
                    Err(e) => break Err(poisoned(e)),
                };
                match &ended.exit {
                    Some(Ok(exit)) => break Ok(Some(*exit)),
                    Some(Err(why)) => break Err(why.clone()),
                    None => {}
                }
                if ended.turn {
                    ended.turn = false;
                    mine = true;
                }
            }
            if !mine {
                let mut state = match shared.state() {
                    Ok(state) => state,
                    Err(why) => break Err(why),
                };
                if !state.reading && state.closed.is_none() {
                    state.reading = true;
                    mine = true;
                }
            }
            let left = match deadline {
                None => Duration::ZERO,
                // A timeout past what an instant holds waits as long as it can at a time.
                Some(None) => timeout.unwrap_or(Duration::ZERO),
                Some(Some(deadline)) => deadline.saturating_duration_since(Instant::now()),
            };
            // Setup cleanup may poll for an already completed program, but continuous unrelated
            // traffic must not keep even that zero-duration poll past its original authority.
            let left = match cap {
                Some(cap) => match cap.remaining("waiting for a program during setup cleanup") {
                    Ok(remaining) => left.min(remaining),
                    Err(why) => break Err(why),
                },
                None => left,
            };
            if mine {
                match shared.turn(Some(left)) {
                    // AC-T7.4.1: time left, it waits on. None (a poll, a spent deadline or cap):
                    // what this one turn read is its end, or none, however many messages for
                    // others keep coming, so they can't hold it past its time.
                    Ok(_) if !left.is_zero() => continue,
                    Ok(_) => {
                        let ended = match self.ending.state.lock() {
                            Ok(ended) => ended,
                            Err(e) => break Err(poisoned(e)),
                        };
                        break match &ended.exit {
                            Some(Ok(exit)) => Ok(Some(*exit)),
                            Some(Err(why)) => Err(why.clone()),
                            None => Ok(None),
                        };
                    }
                    // The connection ended: its `Proc`s were told, this one among them.
                    Err(_) => mine = false,
                }
            } else {
                if left.is_zero() {
                    break Ok(None);
                }
                let ended = match self.ending.state.lock() {
                    Ok(ended) => ended,
                    Err(e) => break Err(poisoned(e)),
                };
                if ended.exit.is_none()
                    && !ended.turn
                    && let Err(e) = self.ending.came.wait_timeout(ended, left)
                {
                    break Err(poisoned(e));
                }
            }
        };
        // Going: no longer waiting, and the turn it holds, or was handed as it went, given to another.
        if let Ok(mut state) = shared.state() {
            state.waiting.remove(&self.exec);
            let handed = self
                .ending
                .state
                .lock()
                .is_ok_and(|mut ended| std::mem::take(&mut ended.turn));
            if (mine || handed) && state.closed.is_none() {
                state.pass_turn();
            }
        }
        if let Ok(Some(exit)) = got {
            self.exit = Some(exit);
        }
        got
    }

    /// Only failed bounded setup uses this cap; ordinary workload kill/wait keeps its old policy.
    fn kill_before(&mut self, cap: ReplyDeadline) -> Result<(), Refusal> {
        if self.wait_before(None, Some(cap))?.is_some() {
            return Ok(());
        }
        let kill = Request::Call(Call::Kill { exec: self.exec });
        match self
            .conn
            .call_before(self.bound, &kill, |_, _| Ok(NONE), None, Some(cap))?
            .1
        {
            Answer::Done => Ok(()),
            Answer::Refused(r) => Err(r),
            other => Err(unexpected("kill", &other)),
        }
    }
}

impl<T: Transport> Proc for RemoteProc<T> {
    fn pid(&self) -> u32 {
        self.pid
    }

    /// Its node's process, never this one's: its end comes as its node's message.
    fn on_this_host(&self) -> bool {
        false
    }

    fn try_wait(&mut self) -> Result<Option<Exit>, Refusal> {
        self.wait(None)
    }

    fn wait_for(&mut self, timeout: Duration) -> Result<Option<Exit>, Refusal> {
        self.wait(Some(timeout))
    }

    /// A kill request naming its exec, answered once the agent has sent KILL; its end then comes as any
    /// end does.
    fn kill(&mut self) -> Result<(), Refusal> {
        if self.wait(None)?.is_some() {
            return Ok(());
        }
        let kill = Request::Call(Call::Kill { exec: self.exec });
        match self.conn.call(self.bound, &kill, |_| Ok(NONE), None)?.1 {
            Answer::Done => Ok(()),
            Answer::Refused(r) => Err(r),
            other => Err(unexpected("kill", &other)),
        }
    }

    fn streams(&mut self) -> Result<hex_api::Given, Refusal> {
        Ok(self.given.take().unwrap_or_default())
    }
}

impl<T: Transport> Drop for RemoteProc<T> {
    /// Let go before its end came: the agent is told, and lets the program run on as dropping an
    /// in-process `Proc` does. A release that can't be sent now (no room, or the connection gone) needs none: the
    /// agent's disconnect rule has the program. One asked to end is never released: its end is its kill's, else
    /// that rule's.
    fn drop(&mut self) {
        let shared = &self.conn.shared;
        let ended = self.exit.is_some()
            || self
                .ending
                .state
                .lock()
                .map(|ended| ended.exit.is_some())
                .unwrap_or(true);
        if let Ok(mut state) = shared.state() {
            state.programs.remove(&self.exec);
        }
        if let Ok(mut known) = shared.known.lock() {
            known.programs.remove(&self.exec);
        }
        if !ended
            && !self.kill_asked
            && let Ok(bytes) = wire::request(self.exec, &Request::Release, wire::MAX_MESSAGE)
        {
            self.conn.send_now_before(&bytes, self.release_before);
        }
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
impl<T: Transport> Core<T> {
    /// The connection's shared state, for a test of a transport to reach its socket.
    pub(crate) fn shared(&self) -> &Shared<T> {
        &self.conn.shared
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
impl<T: Transport> Shared<T> {
    /// How many calls wait for their answers.
    #[cfg(target_os = "linux")]
    pub(crate) fn calls_waiting(&self) -> usize {
        self.state().map_or(0, |state| state.calls.len())
    }

    /// What `f` makes of the reader's read state, between the turn's reads.
    #[cfg(target_os = "macos")]
    pub(crate) fn with_reading<R>(&self, f: impl FnOnce(&T::Reading) -> R) -> R {
        f(&self.reading.lock().unwrap())
    }

    /// A reading thread's step, as the event form's loop reads: one wait on the connection with no bound, then what
    /// came handed on. For a test that sets this core beside a client with a reading thread of its own.
    #[cfg(target_os = "linux")]
    pub(crate) fn read_as_a_thread_would(&self) -> Result<bool, Refusal> {
        self.turn(None)
    }
}
