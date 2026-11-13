//! The client of a node agent in a Shards guest (docs/design/shards-guest.md §3): the same calls as the
//! local socket's, over a byte stream that carries no descriptors. Its control connection carries the
//! wire's messages framed by length ([`hex_core::stream`]); each exec's stdin, stdout and stderr ride a
//! connection of their own, named by its first bytes. One asked for is the caller's to hold, its socket the
//! stream's end; one the caller gives as a pipe of its own is copied between that pipe and the connection in
//! the caller's process, one thread and one buffer per stream: here, or, through a node's socket, by the
//! caller's socket client, which the node gives the connection's end (native plan step 8). The event form, a
//! node's, copies none. A [`Link`] dials the guest's ports: vsock through Shards in production, a loopback
//! socket in tests. The control connection is read as every client's is (`crate::calls`), by whoever waits on it, or
//! in the event form by the node's loop ([`Pump`]), never by a thread of its own.

use std::collections::{BTreeMap, VecDeque};
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hex_api::{Refusal, Stdio};
use hex_core::patience::Patience;
use hex_core::stream::{self, Deframer, Exec, Named};
use hex_core::wire::{self, Received};

use crate::calls::{
    BOX_KINDS, CLASSES, Core, KINDS, Lent, ReplyDeadline, Sent, Shared, SharedOwner, Take,
    Transport, by_class, class_waits, poisoned, protocol_ready,
};
use crate::copy::{Cut, Cuts, DuplexIo, Waiting, copy, copy_in};

/// The OS handle under a guest connection, which an event loop waits on: a file descriptor on Unix, a
/// Winsock socket on Windows.
pub use shards_api::Handle;

/// One fallibly allocated, uniquely owned duplex facade. Native stream cloning remains explicit.
pub type DuplexOwner = triomphe::UniqueArc<dyn Duplex>;

/// Owns a stream facade without adding a native lifecycle owner or an infallible allocation.
/// Its only refusal is exhaustion of the facade's allocation.
pub fn own_duplex<D: Duplex>(duplex: D) -> Result<DuplexOwner, Refusal> {
    use unsize::CoerceUnsize;

    let owner = triomphe::UniqueArc::try_new(duplex).map_err(|_| Refusal::Exhausted {
        what: "guest duplex owner",
        bound: std::mem::size_of::<D>(),
    })?;
    Ok(owner.unsize(unsize::Coercion!(to dyn Duplex)))
}

/// A connection to one of the guest's ports.
pub trait Duplex: Read + Write + Send + Sync + 'static {
    /// Another handle on the same connection.
    fn try_clone(&self) -> io::Result<DuplexOwner>;
    fn shutdown(&self, how: Shutdown) -> io::Result<()>;
    fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()>;
    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()>;
    /// The OS handle under it, borrowed, for an event loop to wait on; it stays the connection's.
    fn handle(&self) -> Handle<'_>;
    /// With `on`, a read or a write that would wait fails `WouldBlock` at once; shared by its clones.
    fn set_nonblocking(&self, on: bool) -> io::Result<()>;
    /// What cuts it short where its reads alone can't tell its caller (a QUIC connection bridged to a socket pair,
    /// `hex_quic::bridge`): none where its socket reports a reset itself.
    fn cut(&self) -> Option<Cut> {
        None
    }
}

impl Duplex for TcpStream {
    fn try_clone(&self) -> io::Result<DuplexOwner> {
        own_duplex(TcpStream::try_clone(self)?)
            .map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))
    }

    fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        TcpStream::shutdown(self, how)
    }

    fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        TcpStream::set_write_timeout(self, timeout)
    }

    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        TcpStream::set_read_timeout(self, timeout)
    }

    fn handle(&self) -> Handle<'_> {
        #[cfg(unix)]
        return std::os::fd::AsFd::as_fd(self);
        #[cfg(windows)]
        std::os::windows::io::AsSocket::as_socket(self)
    }

    fn set_nonblocking(&self, on: bool) -> io::Result<()> {
        TcpStream::set_nonblocking(self, on)
    }
}

#[cfg(unix)]
impl Duplex for std::os::unix::net::UnixStream {
    fn try_clone(&self) -> io::Result<DuplexOwner> {
        own_duplex(std::os::unix::net::UnixStream::try_clone(self)?)
            .map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))
    }

    fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        std::os::unix::net::UnixStream::shutdown(self, how)
    }

    fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        std::os::unix::net::UnixStream::set_write_timeout(self, timeout)
    }

    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        std::os::unix::net::UnixStream::set_read_timeout(self, timeout)
    }

    fn handle(&self) -> Handle<'_> {
        std::os::fd::AsFd::as_fd(self)
    }

    fn set_nonblocking(&self, on: bool) -> io::Result<()> {
        std::os::unix::net::UnixStream::set_nonblocking(self, on)
    }
}

/// What dials the guest's ports.
pub trait Link: Send + Sync + 'static {
    /// A new connection to the guest's `port`.
    fn dial(&self, port: u32) -> Result<DuplexOwner, Refusal>;
    /// Optional caller budget for one control/setup operation; never a connection lifetime.
    fn reply_budget(&self) -> Option<Duration> {
        None
    }
    /// The link's own bound on a connection's setup, its dial and the relay's token, spent once from
    /// the setup's start where the caller gives no deadline. None: this link's dial has no bound of
    /// its own (a test's loopback), and the token's read then waits as long as the relay takes.
    fn setup_bound(&self) -> Option<Duration> {
        None
    }
    /// Dials within the operation's remaining absolute deadline. A link opting into a reply
    /// budget must override this; an unbounded legacy dial cannot truthfully implement the cap.
    fn dial_before(&self, port: u32, deadline: Option<Instant>) -> Result<DuplexOwner, Refusal> {
        if deadline.is_some() {
            return Err(Refusal::Invalid {
                what: "guest dial deadline",
                why: "this link has no deadline-aware dial".to_owned(),
            });
        }
        self.dial(port)
    }
    /// Whether this link's transport carries a half-close: this side's end of sending reaches the peer as its end of
    /// input, the other way still open, as Shards' vsock does (D12) and QUIC's lanes do. Firecracker's vsock doesn't: it
    /// makes a host's half-close a full shutdown and drops what the guest sends after it (`vsock/csm/connection.rs`
    /// 228-239 and 307 at upstream 500efc6; docs/design/shards-guest.md).
    fn carries_half_close(&self) -> bool {
        false
    }
    /// An identity safe to lend to a caller that opens its own guest streams, without owning the VM.
    fn identity(&self) -> Result<shards_api::Identity, Refusal> {
        Err(Refusal::Invalid {
            what: "guest stream identity",
            why: "this link has no lendable VM identity".to_owned(),
        })
    }
}

/// The same fallibly allocated link retained by every stream on a connection.
pub type LinkOwner = triomphe::Arc<dyn Link>;

/// Owns `link` before dialing or any other link effect. Unsizing keeps the same allocation.
pub fn own_link<L: Link>(link: L) -> Result<LinkOwner, Refusal> {
    use unsize::CoerceUnsize;

    let owner = triomphe::Arc::try_new(link).map_err(|_| Refusal::Exhausted {
        what: "guest link owner",
        bound: std::mem::size_of::<L>(),
    })?;
    Ok(owner.unsize(unsize::Coercion!(to dyn Link)))
}

/// How long a call waits for the guest's node agent's answer, one wait a kind of call (`crate::calls::kind_of`'s
/// order), as the socket's client waits (decision 9): twice the slowest answer this process has had to a call of
/// that kind, through the guest. On macOS, Hello and capacity ([`CLASS_ANSWERS`]'s, the oci class's) use exact
/// per-profile maxima from
/// `bench/sched/runtime-pools/calibration-macos-cold-extension-1bb73f74.json` (AC-T7.4.1). Hello begins after dial/token setup;
/// fresh-backend, subsequent and warm borrower cohorts are retained separately in the record, not per-row VM proof.
/// Neither row measures VM boot or workload completion. A debug build's seed is never under the release record
/// (`Patience::by_profile`): Hello's debug campaign met no cold first hello, so its debug seed is the release one.
/// Other host platforms and operations remain measuring until their own actual rows are recorded; macOS evidence
/// is not extrapolated across OS transports.
static ANSWERS: [Patience; KINDS] = [
    #[cfg(target_os = "macos")]
    Patience::by_profile(
        "client.guest.answer.hello",
        Duration::from_nanos(3_022_291),
        Some(Duration::from_nanos(295_291)),
    ),
    #[cfg(not(target_os = "macos"))]
    Patience::measuring("client.guest.answer.hello"),
    Patience::measuring("client.guest.answer.list"),
    Patience::measuring("client.guest.answer.netns"),
    Patience::measuring("client.guest.answer.schedule.describe"),
    Patience::measuring("client.guest.answer.schedule.advertise"),
    Patience::measuring("client.guest.answer.schedule.submit"),
    Patience::measuring("client.guest.answer.schedule.hear_create"),
    Patience::measuring("client.guest.answer.schedule.hear_delete"),
    Patience::measuring("client.guest.answer.schedule.hear_control"),
    Patience::measuring("client.guest.answer.schedule.link_up"),
    Patience::measuring("client.guest.answer.schedule.retry"),
    Patience::measuring("client.guest.answer.schedule.delete"),
    Patience::measuring("client.guest.answer.schedule.status"),
    Patience::measuring("client.guest.answer.physical_report"),
    Patience::measuring("client.guest.answer.initialize_backing"),
    Patience::measuring("client.guest.answer.release_backing"),
    Patience::measuring("client.guest.answer.backing_usage"),
    Patience::measuring("client.guest.answer.schedule.forget"),
];

/// AC-T7.4.1: the kinds a box's class times, a wait for each (kind, class), named
/// `client.guest.answer.<kind>.<class>`. On macOS the guest's capacity row above is an `oci` box's (every guest
/// capacity call asks `oci`): release 6.849583 ms, debug 6.826667 ms. No census has timed another kind or class
/// through a guest: MEASURING, each, never another class's record, until a census records it.
#[cfg(target_os = "macos")]
static CLASS_ANSWERS: [[Patience; BOX_KINDS]; CLASSES] = [
    class_waits!("client.guest.answer", "process", measuring),
    class_waits!(
        "client.guest.answer",
        "oci",
        [
            measuring,
            measuring,
            measuring,
            measuring,
            measuring,
            measuring,
            measuring,
            measuring,
            measuring,
            measuring,
            (
                Duration::from_nanos(6_849_583),
                Some(Duration::from_nanos(6_826_667))
            ),
        ]
    ),
    class_waits!("client.guest.answer", "firecracker", measuring),
    class_waits!("client.guest.answer", "shards", measuring),
];
#[cfg(not(target_os = "macos"))]
static CLASS_ANSWERS: [[Patience; BOX_KINDS]; CLASSES] = [
    class_waits!("client.guest.answer", "process", measuring),
    class_waits!("client.guest.answer", "oci", measuring),
    class_waits!("client.guest.answer", "firecracker", measuring),
    class_waits!("client.guest.answer", "shards", measuring),
];

/// The learned waits a guest's protocol carries a client's calls under (decision 9; AC-T7.4.1): each transport that
/// carries it learns its own, never another's record, so the type names whose tables a [`Stream`] times by.
pub trait Waits: Send + Sync + 'static {
    /// The plain kinds' waits ([`Transport::answers`]).
    fn answers() -> &'static [Patience; KINDS];
    /// A class's box kinds' waits ([`Transport::class_answers`]).
    fn class_answers(class: hex_api::Class) -> &'static [Patience; BOX_KINDS];
}

/// A guest's node, over vsock or its stand-in: the tables above.
pub struct Guest;

impl Waits for Guest {
    fn answers() -> &'static [Patience; KINDS] {
        &ANSWERS
    }

    fn class_answers(class: hex_api::Class) -> &'static [Patience; BOX_KINDS] {
        by_class(&CLASS_ANSWERS, class)
    }
}

/// A node over QUIC (`hex-quic`'s link; docs/design/hex-quic.md Q7): the guest's protocol on hyper-transport's lanes,
/// its waits its own, `client.quic.answer.<kind>` and `client.quic.answer.<kind>.<class>`, each MEASURING until a
/// census over QUIC records it, never the guest's record.
pub struct Quic;

static QUIC_ANSWERS: [Patience; KINDS] = [
    Patience::measuring("client.quic.answer.hello"),
    Patience::measuring("client.quic.answer.list"),
    Patience::measuring("client.quic.answer.netns"),
    Patience::measuring("client.quic.answer.schedule.describe"),
    Patience::measuring("client.quic.answer.schedule.advertise"),
    Patience::measuring("client.quic.answer.schedule.submit"),
    Patience::measuring("client.quic.answer.schedule.hear_create"),
    Patience::measuring("client.quic.answer.schedule.hear_delete"),
    Patience::measuring("client.quic.answer.schedule.hear_control"),
    Patience::measuring("client.quic.answer.schedule.link_up"),
    Patience::measuring("client.quic.answer.schedule.retry"),
    Patience::measuring("client.quic.answer.schedule.delete"),
    Patience::measuring("client.quic.answer.schedule.status"),
    Patience::measuring("client.quic.answer.physical_report"),
    Patience::measuring("client.quic.answer.initialize_backing"),
    Patience::measuring("client.quic.answer.release_backing"),
    Patience::measuring("client.quic.answer.backing_usage"),
    Patience::measuring("client.quic.answer.schedule.forget"),
];

static QUIC_CLASS_ANSWERS: [[Patience; BOX_KINDS]; CLASSES] = [
    class_waits!("client.quic.answer", "process", measuring),
    class_waits!("client.quic.answer", "oci", measuring),
    class_waits!("client.quic.answer", "firecracker", measuring),
    class_waits!("client.quic.answer", "shards", measuring),
];

impl Waits for Quic {
    fn answers() -> &'static [Patience; KINDS] {
        &QUIC_ANSWERS
    }

    fn class_answers(class: hex_api::Class) -> &'static [Patience; BOX_KINDS] {
        by_class(&QUIC_CLASS_ANSWERS, class)
    }
}

/// A node agent reached through a [`Link`]: in a guest by default, its calls timed by `W`'s waits.
pub type Client<W = Guest> = Core<Stream<W>>;

/// The control connection, as a client's transport: read and written without waiting, waited on by the core.
pub struct Stream<W = Guest> {
    waits: std::marker::PhantomData<fn() -> W>,
    link: LinkOwner,
    /// Configuration, not a connection-lifetime deadline: each operation starts its own cap.
    reply_budget: Option<Duration>,
    /// The token the relay sent first on the control connection, which its stdio connections name.
    token: u64,
    /// The writing handle, its messages one at a time under the core's send lock.
    write: Mutex<DuplexOwner>,
    /// A handle to wait on and to end the connection with while a send holds the writing one.
    end: DuplexOwner,
    /// The reading handle, until the core's read state takes it.
    read: Mutex<Option<DuplexOwner>>,
    /// In the event form, what waits to be written ([`Backlog`]); else nothing.
    backlog: Mutex<Option<Backlog>>,
    /// Each exec's streams being set up whose cut is told, by name, until its start takes them
    /// ([`Transport::cuts`]): at most the calls in flight hold one.
    cuts: Mutex<BTreeMap<u64, Cuts>>,
}

/// What the turn's holder reads with and into, and what of a frame has come.
pub struct Reading {
    conn: Option<DuplexOwner>,
    /// A read takes at most one whole frame, the most the deframer holds.
    buf: Vec<u8>,
    deframer: Deframer,
}

/// The event form's bytes waiting to be written, oldest first, flushed by its [`Pump`] as the
/// connection takes them: at most `bound` bytes, the agent's in-flight bound in whole messages, so a
/// send past it is refused, naming the bound (CLAUDE.md §2), and a whole message is queued or none.
struct Backlog {
    bytes: VecDeque<u8>,
    bound: usize,
    /// Told when the backlog stops being empty, so its loop watches for room.
    wants_room: Arc<dyn Fn() + Send + Sync>,
}

/// What [`Backlog::bound`] names in its refusal.
pub const BACKLOG: &str = "bytes waiting to be written to the guest's node agent";

fn os(op: &'static str) -> impl Fn(io::Error) -> Refusal {
    move |e| Refusal::os(op, e)
}

/// Protocol bytes use readiness rather than per-read socket timeouts: partial reads/writes and
/// interruptions consume one deadline, and no timeout is left behind on a workload's stdio.
fn protocol_read(
    conn: &mut dyn Duplex,
    mut bytes: &mut [u8],
    deadline: ReplyDeadline,
    op: &'static str,
) -> Result<(), Refusal> {
    while !bytes.is_empty() {
        deadline.remaining(op)?;
        match conn.read(bytes) {
            Ok(0) => return Err(Refusal::os(op, io::ErrorKind::UnexpectedEof)),
            Ok(n) => {
                bytes = bytes.get_mut(n..).ok_or_else(|| {
                    Refusal::Broken("a guest stream read more bytes than it was lent".to_owned())
                })?;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                protocol_ready(conn.handle(), false, deadline, op)?;
            }
            Err(e) => return Err(Refusal::os(op, e)),
        }
    }
    Ok(())
}

fn protocol_write(
    conn: &mut dyn Duplex,
    mut bytes: &[u8],
    deadline: ReplyDeadline,
    op: &'static str,
) -> Result<(), Refusal> {
    while !bytes.is_empty() {
        deadline.remaining(op)?;
        match conn.write(bytes) {
            Ok(0) => return Err(Refusal::os(op, io::ErrorKind::WriteZero)),
            Ok(n) => {
                bytes = bytes.get(n..).ok_or_else(|| {
                    Refusal::Broken("a guest stream wrote more bytes than it was lent".to_owned())
                })?;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                protocol_ready(conn.handle(), true, deadline, op)?;
            }
            Err(e) => return Err(Refusal::os(op, e)),
        }
    }
    Ok(())
}

impl<W: Waits> Stream<W> {
    /// A stdio connection of its own to the guest, named for `named`, so the guest's relay hands it to that exec's
    /// program as its stream.
    fn dial_named(&self, named: Named) -> Result<DuplexOwner, Refusal> {
        self.dial_named_before(named, None)
    }

    fn dial_named_before(
        &self,
        named: Named,
        deadline: Option<ReplyDeadline>,
    ) -> Result<DuplexOwner, Refusal> {
        if let Some(deadline) = deadline {
            deadline.remaining("dialing a stdio connection")?;
        }
        let mut conn = self
            .link
            .dial_before(stream::STDIO_PORT, deadline.map(ReplyDeadline::at))?;
        match deadline {
            Some(deadline) => name_before(&mut *conn, named, deadline)?,
            None => conn
                .write_all(&stream::name(named))
                .map_err(os("naming a stdio connection"))?,
        }
        // An output stream carries nothing this way once named: where the link carries a half-close, its own half
        // ends, so its peer's End closes the connection whole, which no reader sleeps through (Shards M236), and the
        // peer's side ends both halves with nothing left open to reset (AC-T7.4.1; joint plan step 4, ROOT's decision
        // of 2026-11-12).
        if self.link.carries_half_close()
            && matches!(named.stream, stream::Stdio::Out | stream::Stdio::Err)
        {
            conn.shutdown(Shutdown::Write)
                .map_err(os("ending an output stream's own half"))?;
        }
        Ok(conn)
    }
}

/// Writes a stdio connection's name within `deadline`, by readiness, so no timeout is left on the stream.
fn name_before(
    conn: &mut dyn Duplex,
    named: Named,
    deadline: ReplyDeadline,
) -> Result<(), Refusal> {
    conn.set_nonblocking(true)
        .map_err(os("naming a stdio connection without waiting"))?;
    protocol_write(
        conn,
        &stream::name(named),
        deadline,
        "naming a stdio connection",
    )?;
    // The deadline ends with setup. Copies and caller-owned descriptors are workload I/O.
    conn.set_nonblocking(false)
        .map_err(os("handing off a named stdio connection"))
}

impl<W: Waits> Stream<W> {
    /// Each stream asked for is a stdio connection of its own to the guest, named for the exec, and the
    /// caller holds that connection's socket: the guest's relay splices it to the program, so no byte is
    /// copied on the host (native plan step 8).
    #[cfg(any(unix, windows))]
    fn asked(
        &self,
        exec: u64,
        ask: hex_api::Ask,
        deadline: Option<ReplyDeadline>,
    ) -> Result<hex_api::Given, Refusal> {
        let exec = Exec {
            conn: self.token,
            id: exec,
        };
        let dial = |on: bool, stream| -> Result<Option<hex_api::End>, Refusal> {
            if !on {
                return Ok(None);
            }
            let conn = self.dial_named_before(Named { exec, stream }, deadline)?;
            if let (Some(name), Some(cut)) = (output(stream), conn.cut()) {
                self.watch(exec.id, name, cut)?;
            }
            caller_end(&*conn).map(Some)
        };
        Ok(hex_api::Given {
            stdin: dial(ask.stdin, stream::Stdio::In)?,
            stdout: dial(ask.stdout, stream::Stdio::Out)?,
            stderr: dial(ask.stderr, stream::Stdio::Err)?,
        })
    }

    /// Copies output stream `name`'s connection `conn` into the caller's own pipe `to`, its cut told to its program.
    fn carry(
        &self,
        exec: u64,
        name: &'static str,
        conn: DuplexOwner,
        to: std::io::PipeWriter,
    ) -> Result<(), Refusal> {
        let cut = Cut::default();
        copy(DuplexIo(conn), to, cut.clone(), drop)?;
        self.watch(exec, name, cut)
    }

    /// Keeps exec `exec`'s stream `name`'s cut until its start takes it ([`Transport::cuts`]).
    fn watch(&self, exec: u64, name: &'static str, cut: Cut) -> Result<(), Refusal> {
        self.cuts
            .lock()
            .map_err(poisoned)?
            .entry(exec)
            .or_default()
            .push((name, cut));
        Ok(())
    }
}

/// An output stream's name: whether its cut is its program's to tell. A stdin connection's reset is its program
/// letting its stdin go (HEX-GUEST-41), the stream's end.
fn output(stream: stream::Stdio) -> Option<&'static str> {
    match stream {
        stream::Stdio::In => None,
        stream::Stdio::Out => Some("stdout"),
        stream::Stdio::Err => Some("stderr"),
    }
}

/// The caller's own end of a guest stdio connection: its descriptor, duplicated; this side's closes as the
/// connection drops. HEX-RT-17: Shards waits around a non-blocking socket in its Stream wrapper. Only the raw
/// descriptor outlives this handoff, so its caller's OwnedStream must block itself; the control connection
/// stays evented.
#[cfg(any(unix, windows))]
fn caller_end(conn: &dyn Duplex) -> Result<hex_api::End, Refusal> {
    let end = conn
        .handle()
        .try_clone_to_owned()
        .map_err(os("handing a stdio connection to its caller"))?;
    #[cfg(unix)]
    {
        let flags = rustix::fs::fcntl_getfl(&end)
            .map_err(|e| Refusal::os("reading a stdio connection's mode", e))?;
        rustix::fs::fcntl_setfl(&end, flags & !rustix::fs::OFlags::NONBLOCK)
            .map_err(|e| Refusal::os("making a caller's stdio connection wait", e))?;
        Ok(end)
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsSocket;
        hex_afunix::set_nonblocking(end.as_socket(), false)
            .map_err(os("making a caller's stdio connection wait"))?;
        Ok(end.into())
    }
}

/// HEX-RT-17: an offered guest exec's streams, opened in this caller (a Windows node's, which can't lend its
/// own across processes): each a borrowed CONNECT to the guest the offer names, taking no VM lifetime, named
/// for the exec as the guest's own stdio dial names it, the caller's end handed off as its own.
///
/// AC-T7.4.1: Shards' borrowed CONNECT waits for its OK only within a finite deadline (zero patience reads it
/// without waiting, so a guest that hasn't answered yet is refused), so the call's deadline bounds every
/// stream's CONNECT and name, and a call with none is refused by type before any is made.
// Windows calls it; elsewhere only its test does, which needs Linux's /proc for a stand-in VM's identity.
#[cfg(any(windows, all(test, target_os = "linux")))]
pub(crate) fn open_offered(
    offer: &wire::StreamOffer,
    deadline: Option<ReplyDeadline>,
) -> Result<hex_api::Given, Refusal> {
    let deadline = deadline.ok_or_else(|| Refusal::Invalid {
        what: "offered guest streams",
        why:
            "a borrowed guest's CONNECT waits for its OK only within a deadline, and this call has \
              none: give the client a reply budget or call with a deadline"
                .to_owned(),
    })?;
    let identity = shards_api::Identity {
        pid: offer.pid,
        started: offer.started,
        vsock: offer.vsock.clone(),
    };
    let exec = Exec {
        conn: offer.token,
        id: offer.exec,
    };
    let open = |on: bool, stream| -> Result<Option<hex_api::End>, Refusal> {
        if !on {
            return Ok(None);
        }
        const OP: &str = "opening an offered guest stream";
        // One CONNECT to the already-listening guest, waiting for its OK within what is left of the
        // call's deadline; one that completes past it is refused.
        let conn =
            shards_api::connect_borrowed(&identity, stream::STDIO_PORT, deadline.remaining(OP)?)
                .map_err(|why| shards::refusal(OP, &why))?;
        deadline.remaining(OP)?;
        let mut conn = shards::ShardsStream(conn);
        name_before(&mut conn, Named { exec, stream }, deadline)?;
        caller_end(&conn).map(Some)
    };
    Ok(hex_api::Given {
        stdin: open(offer.streams.stdin, stream::Stdio::In)?,
        stdout: open(offer.streams.stdout, stream::Stdio::Out)?,
        stderr: open(offer.streams.stderr, stream::Stdio::Err)?,
    })
}

impl<W: Waits> Transport for Stream<W> {
    type Reading = Reading;
    const MAKES_ASKED: bool = true;

    fn stream_offer(
        &self,
        exec: u64,
        streams: wire::Streams,
    ) -> Result<wire::StreamOffer, Refusal> {
        let identity = self.link.identity()?;
        Ok(wire::StreamOffer {
            pid: identity.pid,
            started: identity.started,
            vsock: identity.vsock,
            token: self.token,
            exec,
            streams,
        })
    }

    fn answers() -> &'static [Patience; KINDS] {
        W::answers()
    }

    fn class_answers(class: hex_api::Class) -> &'static [Patience; BOX_KINDS] {
        W::class_answers(class)
    }

    fn reply_budget(&self) -> Option<Duration> {
        self.reply_budget
    }

    fn reading(&self) -> Reading {
        Reading {
            conn: self.read.lock().ok().and_then(|mut r| r.take()),
            buf: vec![0u8; stream::MAX_FRAME],
            deframer: Deframer::new(wire::MAX_MESSAGE),
        }
    }

    fn handle(&self) -> crate::calls::Handle<'_> {
        self.end.handle()
    }

    #[cfg(any(unix, windows))]
    fn asks(&self, exec: u64, ask: hex_api::Ask) -> Result<hex_api::Given, Refusal> {
        self.asked(exec, ask, None)
    }

    #[cfg(any(unix, windows))]
    fn asks_before(
        &self,
        exec: u64,
        ask: hex_api::Ask,
        deadline: ReplyDeadline,
    ) -> Result<hex_api::Given, Refusal> {
        self.asked(exec, ask, Some(deadline))
    }

    #[cfg(not(any(unix, windows)))]
    fn asks(&self, _exec: u64, _ask: hex_api::Ask) -> Result<hex_api::Given, Refusal> {
        Err(Refusal::Platform {
            what: "stdio streams made by the guest's transport",
            platform: std::env::consts::OS,
            why: "a guest stream's handle as the caller's is the Windows agent's to add".to_owned(),
        })
    }

    /// Each pipe of the caller's own is copied between it and a stdio connection of its own to the guest, named
    /// for the exec: here, in the caller's process, whose client this is (native plan step 8).
    fn carries(&self, exec: u64, stdio: Stdio) -> Result<(), Refusal> {
        let exec = Exec {
            conn: self.token,
            id: exec,
        };
        let named = |stream| Named { exec, stream };
        if let Some(r) = stdio.stdin {
            copy_in(r, DuplexIo(self.dial_named(named(stream::Stdio::In))?))?;
        }
        if let Some(w) = stdio.stdout {
            self.carry(
                exec.id,
                "stdout",
                self.dial_named(named(stream::Stdio::Out))?,
                w,
            )?;
        }
        if let Some(w) = stdio.stderr {
            self.carry(
                exec.id,
                "stderr",
                self.dial_named(named(stream::Stdio::Err))?,
                w,
            )?;
        }
        Ok(())
    }

    fn carries_before(
        &self,
        exec: u64,
        stdio: Stdio,
        deadline: ReplyDeadline,
    ) -> Result<(), Refusal> {
        let exec = Exec {
            conn: self.token,
            id: exec,
        };
        let dial = |stream| self.dial_named_before(Named { exec, stream }, Some(deadline));
        // Finish all fallible protocol setup before starting any workload copy. A timeout while
        // naming a later stream then drops exactly the connections already made, with no copy left waiting.
        let input = stdio
            .stdin
            .as_ref()
            .map(|_| dial(stream::Stdio::In))
            .transpose()?;
        let output = stdio
            .stdout
            .as_ref()
            .map(|_| dial(stream::Stdio::Out))
            .transpose()?;
        let error = stdio
            .stderr
            .as_ref()
            .map(|_| dial(stream::Stdio::Err))
            .transpose()?;
        if let (Some(r), Some(conn)) = (stdio.stdin, input) {
            copy_in(r, DuplexIo(conn))?;
        }
        if let (Some(w), Some(conn)) = (stdio.stdout, output) {
            self.carry(exec.id, "stdout", conn, w)?;
        }
        if let (Some(w), Some(conn)) = (stdio.stderr, error) {
            self.carry(exec.id, "stderr", conn, w)?;
        }
        Ok(())
    }

    fn cuts(&self, exec: u64) -> Result<Cuts, Refusal> {
        Ok(self
            .cuts
            .lock()
            .map_err(poisoned)?
            .remove(&exec)
            .unwrap_or_default())
    }

    /// What one read takes, each whole message in it handed on; a frame cut across reads stays in the deframer.
    fn recv(&self, reading: &mut Reading, take: &mut Take<'_>) -> Result<bool, Refusal> {
        let Reading {
            conn,
            buf,
            deframer,
        } = reading;
        let Some(conn) = conn.as_mut() else {
            return Err(Refusal::Broken(
                "the guest's connection has no reading handle".to_owned(),
            ));
        };
        let n = match conn.read(buf) {
            Ok(0) => return Err(ended(deframer)),
            Ok(n) => n,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                return Ok(false);
            }
            Err(e) => {
                return Err(Refusal::Broken(format!(
                    "reading the node agent's connection: {e}"
                )));
            }
        };
        let mut input = buf.get(..n).unwrap_or_default();
        while !input.is_empty() {
            let (took, whole) = deframer.take(input).map_err(|fault| {
                Refusal::Broken(format!(
                    "the node agent sent bytes that aren't a message: {fault}"
                ))
            })?;
            input = input.get(took..).unwrap_or_default();
            let Some(message) = whole else { continue };
            let received = Received {
                bytes: &message,
                size: message.len(),
                descriptors: 0,
                descriptors_cut: false,
            };
            take(&received, Vec::new())?;
        }
        Ok(true)
    }

    fn frame(message: Vec<u8>) -> Result<Vec<u8>, Refusal> {
        stream::frame(&message, wire::MAX_MESSAGE).map_err(Refusal::from)
    }

    /// What the connection takes of the frame now. It carries no descriptor: its streams are connections of their
    /// own. In the event form the frame is queued whole behind what waits ([`Backlog`]), for the loop to write.
    fn try_send(&self, bytes: &[u8], at: &mut usize, lent: &[Lent<'_>]) -> Result<bool, Sent> {
        if !lent.is_empty() {
            return Err(Sent::Refused(Refusal::Broken(
                "a guest's connection carries no descriptors".to_owned(),
            )));
        }
        let mut write = self.write.lock().map_err(|e| Sent::Refused(poisoned(e)))?;
        let mut backlog = self
            .backlog
            .lock()
            .map_err(|e| Sent::Refused(poisoned(e)))?;
        if let Some(backlog) = backlog.as_mut() {
            backlog.send(&mut **write, bytes)?;
            *at = bytes.len();
            return Ok(true);
        }
        drop(backlog);
        while let Some(rest) = bytes.get(*at..).filter(|r| !r.is_empty()) {
            match write.write(rest) {
                Ok(0) => return Err(Sent::Gone),
                Ok(n) => *at = at.saturating_add(n),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(false),
                Err(e) if gone(&e) => return Err(Sent::Gone),
                Err(e) => return Err(Sent::Refused(Refusal::os("sending to the node agent", e))),
            }
        }
        Ok(true)
    }

    fn shutdown(&self) {
        let _ = self.end.shutdown(Shutdown::Both);
    }
}

impl Backlog {
    /// Writes `framed` now as far as the connection takes it, behind anything waiting, and queues the
    /// rest; refused whole when it wouldn't fit.
    fn send(&mut self, conn: &mut dyn Duplex, framed: &[u8]) -> Result<(), Sent> {
        if self.bytes.len().saturating_add(framed.len()) > self.bound {
            return Err(Sent::Refused(Refusal::Exhausted {
                what: BACKLOG,
                bound: self.bound,
            }));
        }
        let mut wrote = 0;
        if self.bytes.is_empty() {
            while let Some(rest) = framed.get(wrote..).filter(|r| !r.is_empty()) {
                match conn.write(rest) {
                    Ok(0) => return Err(Sent::Gone),
                    Ok(n) => wrote = wrote.saturating_add(n),
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) if gone(&e) => return Err(Sent::Gone),
                    Err(e) => {
                        return Err(Sent::Refused(Refusal::os("sending to the node agent", e)));
                    }
                }
            }
        }
        let rest = framed.get(wrote..).unwrap_or_default();
        if !rest.is_empty() {
            let was_empty = self.bytes.is_empty();
            self.bytes.extend(rest);
            if was_empty {
                (self.wants_room)();
            }
        }
        Ok(())
    }

    /// Writes what waits as far as the connection takes it: whether any is left.
    fn flush(&mut self, conn: &mut dyn Duplex) -> Result<bool, Refusal> {
        while !self.bytes.is_empty() {
            let (front, _) = self.bytes.as_slices();
            match conn.write(front) {
                Ok(0) => {
                    return Err(Refusal::Broken(
                        "the node agent's connection took no bytes".to_owned(),
                    ));
                }
                Ok(n) => {
                    self.bytes.drain(..n.min(front.len()));
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(true),
                Err(e) => return Err(Refusal::os("sending to the node agent", e)),
            }
        }
        Ok(false)
    }
}

fn gone(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset | io::ErrorKind::NotConnected
    )
}

/// Why a connection that read its end ended: between messages, or in the middle of one.
fn ended(deframer: &Deframer) -> Refusal {
    if deframer.at_boundary() {
        Refusal::Broken("the node agent closed the connection".to_owned())
    } else {
        Refusal::Broken(
            "the node agent closed the connection in the middle of a message".to_owned(),
        )
    }
}

/// Reads at most this many times on one [`Pump::ready`] before the loop's other sources get a turn: a
/// fairness bound, as the node's socket's `READS_PER_WAKE`.
const READS_PER_READY: usize = 64;

/// The event form's reading side ([`Client::connect_evented`]): no thread, but the loop that waits on
/// [`Pump::handle`] calls [`Pump::ready`] when it is ready, which reads what has come and hands each
/// message to its call's completion, and writes what waits. It holds the connection's reader's turn for good.
pub struct Pump<W: Waits = Guest> {
    shared: SharedOwner<Stream<W>>,
    ended: bool,
}

/// What a [`Pump`] wants watched next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pumped {
    /// Readable.
    Read,
    /// Readable, and writable: bytes wait to be written.
    ReadWrite,
    /// Nothing: the connection has ended, and every call and program waiting has heard why.
    Ended,
}

impl<W: Waits> Pump<W> {
    /// The connection's OS handle, borrowed, for the loop to wait on; out of the loop's set before the
    /// pump drops.
    pub fn handle(&self) -> Handle<'_> {
        self.shared.transport.end.handle()
    }

    /// Reads what has come, without waiting, and writes what waits as far as the connection takes it.
    pub fn ready(&mut self) -> Pumped {
        if self.ended {
            return Pumped::Ended;
        }
        for _ in 0..READS_PER_READY {
            match self.shared.recv() {
                Ok(true) => {}
                Ok(false) => break,
                // The connection ended: every call and program waiting was told why.
                Err(_) => {
                    self.ended = true;
                    return Pumped::Ended;
                }
            }
        }
        // What the reads owe the agent goes behind what waits, and with it.
        self.shared.send_owed_now();
        match self.flush() {
            Ok(true) => Pumped::ReadWrite,
            Ok(false) => Pumped::Read,
            Err(why) => {
                self.ended = true;
                self.shared.transport.shutdown();
                self.shared.close(why);
                Pumped::Ended
            }
        }
    }

    /// Writes what waits: whether any is left.
    fn flush(&mut self) -> Result<bool, Refusal> {
        let transport = &self.shared.transport;
        let mut write = transport.write.lock().map_err(poisoned)?;
        let mut backlog = transport.backlog.lock().map_err(poisoned)?;
        match backlog.as_mut() {
            Some(b) => b.flush(&mut **write),
            None => Ok(false),
        }
    }
}

impl<W: Waits> Core<Stream<W>> {
    /// [`Client::connect_before`] over a link whose transport learns its own waits, `W`'s (a node over QUIC); with no
    /// `deadline`, [`Client::connect`]: the link's reply budget where it has one, else the hello's learned wait.
    pub fn connect_timed_before(
        link: LinkOwner,
        deadline: Option<Instant>,
    ) -> Result<Self, Refusal> {
        let budget = link.reply_budget();
        let deadline = match deadline {
            Some(at) => {
                Some(ReplyDeadline::caller_before(at)?.limited_by(ReplyDeadline::caller(budget)?))
            }
            None => ReplyDeadline::caller(budget)?,
        };
        Core::hello_before(
            Shared::try_new(Stream::dial_before(link, budget, deadline)?)?,
            deadline,
        )
    }
}

impl Client {
    /// Dials the guest's node agent through `link` and learns what it runs and its bounds.
    pub fn connect(link: LinkOwner) -> Result<Self, Refusal> {
        Self::connect_timed_before(link, None)
    }

    /// Dials, reads the relay token and says hello within the caller's original absolute
    /// deadline. A smaller configured wait still wins, a learned one only a run-out recorded.
    /// The resulting connection has no lifetime deadline; later bounded operations take their
    /// own original deadline.
    pub fn connect_before(link: LinkOwner, deadline: Instant) -> Result<Self, Refusal> {
        Self::connect_timed_before(link, Some(deadline))
    }

    /// The event form ([`Pump`]): dials the guest's node agent through `link` and says hello, its answer read by
    /// the hello itself, then hands back the client and its reading side, which no thread reads: the caller's loop
    /// waits on [`Pump::handle`] and calls [`Pump::ready`]. Calls made with [`Core::begin`] are answered from there.
    /// `wants_room` is told when bytes wait to be written, so the loop watches for room too. Every connection's
    /// waits are the loop's from here: its reads and writes never wait.
    pub fn connect_evented(
        link: LinkOwner,
        wants_room: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<(Self, Pump), Refusal> {
        let budget = link.reply_budget();
        let deadline = ReplyDeadline::caller(budget)?;
        Self::evented(link, wants_room, budget, deadline)
    }

    /// The same event form and pump, with one original absolute deadline for dial, relay
    /// token and hello. The deadline does not remain on the connection or its event loop.
    pub fn connect_evented_before(
        link: LinkOwner,
        wants_room: Arc<dyn Fn() + Send + Sync>,
        deadline: Instant,
    ) -> Result<(Self, Pump), Refusal> {
        let budget = link.reply_budget();
        let deadline =
            ReplyDeadline::caller_before(deadline)?.limited_by(ReplyDeadline::caller(budget)?);
        Self::evented(link, wants_room, budget, Some(deadline))
    }

    fn evented(
        link: LinkOwner,
        wants_room: Arc<dyn Fn() + Send + Sync>,
        budget: Option<Duration>,
        deadline: Option<ReplyDeadline>,
    ) -> Result<(Self, Pump), Refusal> {
        let shared = Shared::try_new(Stream::dial_before(link, budget, deadline)?)?;
        let client = Core::hello_before(triomphe::Arc::clone(&shared), deadline)?;
        let limits = client.limits();
        *shared.transport.backlog.lock().map_err(poisoned)? = Some(Backlog {
            bytes: VecDeque::new(),
            bound: limits.in_flight.saturating_mul(limits.message_bytes),
            wants_room,
        });
        shared.read_by_loop()?;
        Ok((
            client,
            Pump {
                shared,
                ended: false,
            },
        ))
    }
}

#[cfg(test)]
impl Stream<Guest> {
    /// The guest's control port dialed through `link`, its relay's token read, and the connection made to read and
    /// write without waiting, as the core waits on it.
    pub(crate) fn dial(link: LinkOwner) -> Result<Self, Refusal> {
        let budget = link.reply_budget();
        let deadline = ReplyDeadline::caller(budget)?;
        Self::dial_before(link, budget, deadline)
    }
}

impl<W: Waits> Stream<W> {
    fn dial_before(
        link: LinkOwner,
        budget: Option<Duration>,
        deadline: Option<ReplyDeadline>,
    ) -> Result<Self, Refusal> {
        // AC-T7.4.1: with no caller deadline the link's own setup bound bounds the dial and the relay's
        // token together, so a guest that takes the connection and never sends it can't hold the caller.
        let deadline = match deadline {
            Some(deadline) => Some(deadline),
            None => ReplyDeadline::link(link.setup_bound())?,
        };
        if let Some(deadline) = deadline {
            deadline.remaining("dialing the guest control connection")?;
        }
        let mut conn = link.dial_before(stream::CONTROL_PORT, deadline.map(ReplyDeadline::at))?;
        let mut token = [0u8; stream::TOKEN_BYTES];
        match deadline {
            Some(deadline) => {
                conn.set_nonblocking(true)
                    .map_err(os("reading the relay token without waiting"))?;
                protocol_read(
                    &mut *conn,
                    &mut token,
                    deadline,
                    "reading the relay's token for the connection",
                )?;
            }
            // As long as the link's transport takes, in poll(2) first on macOS (AC-T7.4.1, joint plan step 4, 2(a)).
            None => Waiting(&mut *conn)
                .read_exact(&mut token)
                .map_err(os("reading the relay's token for the connection"))?,
        }
        let token = u64::from_le_bytes(token);
        let read = conn
            .try_clone()
            .map_err(os("holding the node agent's connection"))?;
        let end = conn
            .try_clone()
            .map_err(os("holding the node agent's connection"))?;
        conn.set_nonblocking(true)
            .map_err(os("reading the node agent's connection without waiting"))?;
        Ok(Self {
            waits: std::marker::PhantomData,
            link,
            reply_budget: budget,
            token,
            write: Mutex::new(conn),
            end,
            read: Mutex::new(Some(read)),
            backlog: Mutex::new(None),
            cuts: Mutex::new(BTreeMap::new()),
        })
    }
}

pub mod shards;
pub mod stage;

#[cfg(test)]
mod tests;
