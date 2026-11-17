//! The guest's ports through Shards (docs/design/shards-guest.md §2, Appendix A "Hex's side"): a
//! [`Link`] whose first dial is `shards_api::Guest::connect_before` and every later one
//! `connect_once_before`, a dial past its W* `connect_while_runs`, Firecracker's `CONNECT <port>` on the
//! guest's vsock, and a [`Duplex`](super::Duplex) over the stream it returns. Hex never names Shards' stream
//! type. On Windows it is two named pipes (Shards' D272: a way down, read, and a way up, written), on which neither
//! this client's readiness waits (`WSAPoll`) nor its node's completion port (Winsock's events) wait: there a
//! connection the client waits on is bridged to a local socket pair, and an exec's stream is carried on a pipe whose
//! end is its caller's, as a node VM's lanes are ([`super::lanes`]).

use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use hex_api::{
    ConnectFailure, ConnectPhase, ConnectReply, GuestEnd, GuestFailure, IoKind, Refusal,
};
use hex_core::patience::{Patience, Piece};

#[cfg(windows)]
use super::Piped;
#[cfg(windows)]
use super::bridge::{Bridged, Local, end, pair};
#[cfg(unix)]
use super::{Duplex, Handoff};
use super::{DuplexOwner, Link, own_duplex};
#[cfg(windows)]
use crate::Cut;
#[cfg(windows)]
use crate::calls::ReplyDeadline;

/// This host's class, as the records name it (bench/native/guest/records.tsv, evidence alone: no wait reads it, the
/// review's C-9): the hypervisor Shards boots its guests with here.
pub const CLASS: &str = if cfg!(target_os = "macos") {
    "macOS-HVF"
} else if cfg!(target_os = "windows") {
    "Windows-WHP"
} else {
    "Linux-KVM"
};

/// A connection to one of the guest's ports, as Shards gives it. Its handles share it: each reads and writes it through
/// `&dyn Stream` (Shards' `read_shared`, `write_shared`), never a duplicate socket, which on Windows passes to a process
/// another thread starts until its inheritance is cleared (the review's D-5); and its caller's end is its own socket,
/// moved ([`ShardsStream::into_end`]).
pub struct ShardsStream(triomphe::Arc<Box<dyn shards_api::Stream>>);

impl ShardsStream {
    /// `stream`, its one handle until another shares it.
    pub(crate) fn new(stream: Box<dyn shards_api::Stream>) -> Result<Self, Refusal> {
        let owner = triomphe::UniqueArc::try_new(stream).map_err(|_| Refusal::Exhausted {
            what: "guest stream owner",
            bound: std::mem::size_of::<Box<dyn shards_api::Stream>>(),
        })?;
        Ok(Self(owner.shareable()))
    }

    /// Its socket, its caller's from here (HEX-RT-17): moved, never duplicated, so no copy of it is open to pass to a
    /// process (the review's D-5), and left waiting, as a caller's file does. Refused while another handle shares it.
    #[cfg(unix)]
    pub(crate) fn into_end(self) -> Result<hex_api::End, Refusal> {
        let stream = triomphe::Arc::try_unwrap(self.0).map_err(|_| {
            Refusal::Broken(
                "a guest stream another handle shares can't be handed to its caller whole"
                    .to_owned(),
            )
        })?;
        super::waiting_end(stream.into_socket())
    }
}

impl Read for ShardsStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read_shared(buf)
    }
}

impl Write for ShardsStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write_shared(buf)
    }

    /// Nothing waits: Shards' stream writes straight to its socket, as its own `flush` says.
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(unix)]
impl Duplex for ShardsStream {
    /// Another handle on the same stream, shared, never a duplicate (the review's D-5).
    fn try_clone(&self) -> io::Result<DuplexOwner> {
        own_duplex(Self(triomphe::Arc::clone(&self.0)))
            .map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))
    }

    fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        self.0.shutdown(how)
    }

    fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.0.set_write_timeout(timeout)
    }

    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.0.set_read_timeout(timeout)
    }
    fn handle(&self) -> super::Handle<'_> {
        self.0.handle()
    }

    fn set_nonblocking(&self, on: bool) -> io::Result<()> {
        self.0.set_nonblocking(on)
    }
}

/// A guest's ports. A dial given its caller's deadline waits within it; one given none waits within W* and
/// past it follows the guest ([`ShardsLink::dial_following`], decision 9). Until a dial has reached the
/// guest's relay its bounded attempt is one `connect_before`, within which Shards' client asks again while
/// the guest doesn't listen there yet. The relay binds every port it serves before it serves any (`hex
/// guest`), so after that a refused port has no relay behind it: each later attempt is one
/// `connect_once_before`, refused at once if nothing listens. `bound` is its node's W* for the guest, the
/// slowest boot to ready it has seen (`hex_runtime::guest::GuestRuntime::boot_bound`), whose ready includes a
/// whole CONNECT/OK exchange; none before its node has seen one. Never a relative patience: Shards' client
/// awaits a zero or `Duration::MAX` patience's answer without a bound (HEX-GUEST-33).
pub struct ShardsLink {
    guest: ShardsGuest,
    bound: Option<Duration>,
    /// Its guest's boot's start, for a link made as its guest boots: its first dial is that boot's own wait.
    booted: Option<Instant>,
    listened: AtomicBool,
}

enum ShardsGuest {
    Owned(Arc<dyn shards_api::Guest>),
    Managed(shards_api::ManagedGuest),
}

impl ShardsGuest {
    fn guest(&self) -> &dyn shards_api::Guest {
        match self {
            Self::Owned(guest) => guest.as_ref(),
            Self::Managed(guest) => guest,
        }
    }
}

/// A guest link's dials with no caller deadline, as the waits census keeps them (decision 9): each stream, timed from
/// its dial's start, and each dial's run-out past its W* ([`ShardsLink::dial_following`]). Its own slowest answer is the
/// W* of a link whose node has seen no boot.
static FOLLOWED: Patience = Patience::measuring("client.guest.link.dial");

impl ShardsLink {
    pub fn new(guest: Arc<dyn shards_api::Guest>, bound: Option<Duration>) -> Self {
        Self {
            guest: ShardsGuest::Owned(guest),
            bound,
            booted: None,
            listened: AtomicBool::new(false),
        }
    }

    /// AC-T7.4.1: retain the API's existing native/protocol receipt directly. The
    /// caller uses ManagedGuest::try_clone when it also retains a boot handle;
    /// adapting that handle needs no second Arc allocation or lifecycle owner.
    pub fn managed(guest: shards_api::ManagedGuest, bound: Option<Duration>) -> Self {
        Self {
            guest: ShardsGuest::Managed(guest),
            bound,
            booted: None,
            listened: AtomicBool::new(false),
        }
    }

    /// The same link to a guest whose boot began at `began`: its first dial with no caller deadline is that boot's own
    /// wait, its W* counted from the boot's start, as the slowest boot was (HEX-GUEST-34).
    pub fn booted_at(mut self, began: Instant) -> Self {
        self.booted = Some(began);
        self
    }

    /// A dial to `port` with no caller deadline, as decision 9 waits (the person's ruling of 2026-11-13; the review's
    /// C-9). Its W* is the link's bound, the slowest boot to ready its node has seen, whose ready includes a whole
    /// CONNECT/OK exchange. A booting link's first dial is its boot's own wait: its W* is that bound alone, counted from
    /// the boot's start, none before its node's first boot. Any other dial with no bound takes the slowest such dial
    /// this process has seen ([`FOLLOWED`]).
    /// - **Within it** one attempt, `connect_before` (`connect_once_before` once the relay listens): its stream is the
    ///   answer, and a refusal before W* or at the guest's end is the dial's.
    /// - **Past it** the run-out is told, once, and one dial follows the guest's VM by its life, Shards'
    ///   `connect_while_runs`: refused at the guest's end, or as `Stalled` where its VM ran no CPU for a whole W* of
    ///   Shards' own (Part B, the VM's liveness), never at a bound of Hex's. No attempt is given up and made again past
    ///   it: each one closed unanswered is a connection the guest takes for nothing.
    /// - **With no W*** it follows from its start, told once as it starts.
    fn dial_following(&self, port: u32) -> Result<Box<dyn shards_api::Stream>, Refusal> {
        let began = Instant::now();
        let guest = self.guest.guest();
        let party = [
            Piece::Kind("the guest's VM, its port "),
            Piece::Number(u64::from(port)),
            Piece::Kind("'s answer to a dial"),
        ];
        let listened = self.listened.load(Ordering::Acquire);
        let booting = self.booted.filter(|_| !listened);
        match self.w_star(booting.is_some())? {
            Some(w_star) => {
                let from = booting.unwrap_or(began);
                let at = from.checked_add(w_star).ok_or_else(unbounded)?;
                let dialed = if listened {
                    guest.connect_once_before(port, at)
                } else {
                    guest.connect_before(port, at)
                };
                match dialed {
                    Ok(stream) => return Ok(self.answered(stream, began)),
                    Err(why) if Instant::now() < at || gone(&why) => {
                        return Err(refusal("connecting to the guest", &why));
                    }
                    // No sample (Karn's point): the run-out is told, and the dial follows the guest.
                    Err(_) => FOLLOWED.ran_out(w_star, from.elapsed(), &party),
                }
            }
            // A boot's own W* is its node's, whatever this process has learned of other dials (`follows`).
            None => FOLLOWED.follows(&party),
        }
        let stream = guest
            .connect_while_runs(port)
            .map_err(|why| refusal("connecting to the guest", &why))?;
        Ok(self.answered(stream, began))
    }

    /// The W* of a dial with no caller deadline ([`ShardsLink::dial_following`]): its bound, refused by type where that is
    /// zero; with none, a boot's own first dial has none, and any other the slowest such dial this process has seen.
    fn w_star(&self, booting: bool) -> Result<Option<Duration>, Refusal> {
        match self.bound {
            Some(bound) if bound.is_zero() => Err(unbounded()),
            Some(bound) => Ok(Some(bound)),
            None if booting => Ok(None),
            None => Ok(FOLLOWED.deadline()),
        }
    }

    /// `stream`, a dial's answer begun at `began`: its port's relay listens, and the dial's time is its sample.
    fn answered(
        &self,
        stream: Box<dyn shards_api::Stream>,
        began: Instant,
    ) -> Box<dyn shards_api::Stream> {
        self.listened.store(true, Ordering::Release);
        FOLLOWED.answered(began.elapsed());
        stream
    }

    /// A stream to port `port`, within `deadline` where the caller gives one ([`Link::dial_before`]), else waiting
    /// within W* and past it following the guest ([`ShardsLink::dial_following`]).
    fn stream_before(&self, port: u32, deadline: Option<Instant>) -> Result<ShardsStream, Refusal> {
        let Some(at) = deadline else {
            return ShardsStream::new(self.dial_following(port)?);
        };
        let cap = crate::calls::ReplyDeadline::caller_before(at)?;
        cap.remaining("connecting to the guest within the original operation deadline")?;
        let guest = self.guest.guest();
        let stream = if self.listened.load(Ordering::Acquire) {
            guest.connect_once_before(port, at)
        } else {
            guest.connect_before(port, at)
        }
        .map_err(|why| refusal("connecting to the guest", &why))?;
        self.listened.store(true, Ordering::Release);
        let stream = ShardsStream::new(stream)?;
        cap.remaining("retaining the guest connection within the original operation deadline")?;
        Ok(stream)
    }
}

/// A link's bound that names no finite deadline, zero or past a monotonic one, refused by type before Shards is asked
/// (HEX-GUEST-33).
fn unbounded() -> Refusal {
    Refusal::Invalid {
        what: "guest dial bound",
        why: "it must be positive and fit a monotonic deadline".to_owned(),
    }
}

/// Whether Shards' refusal says its guest is gone: ended, no longer at its pid, its process unobservable or another,
/// or its VM stalled.
fn gone(why: &shards_api::Refusal) -> bool {
    matches!(
        why,
        shards_api::Refusal::Ended(_)
            | shards_api::Refusal::Gone { .. }
            | shards_api::Refusal::Process { .. }
            | shards_api::Refusal::Scope(_)
            | shards_api::Refusal::Stalled(_)
    )
}

impl Link for ShardsLink {
    fn identity(&self) -> Result<shards_api::Identity, Refusal> {
        self.guest
            .guest()
            .identity()
            .map_err(|why| refusal("naming the guest's stream endpoint", &why))
    }

    /// A Shards guest's vsock socket takes a borrowed CONNECT from any caller its identity names (HEX-RT-17).
    fn lends_identity(&self) -> bool {
        true
    }

    /// The W* of a dial with no caller deadline ([`ShardsLink::dial_following`]), whose answer includes the CONNECT/OK
    /// exchange: what a stream offer carries for a caller's borrowed CONNECTs (HEX-RT-17); none before any is learned.
    fn setup_bound(&self) -> Option<Duration> {
        self.bound.or_else(|| FOLLOWED.deadline())
    }

    /// Shards carries a half-close (its D12, held by its conformance case).
    fn carries_half_close(&self) -> bool {
        true
    }

    fn dial(&self, port: u32) -> Result<DuplexOwner, Refusal> {
        self.dial_before(port, None)
    }

    fn dial_before(&self, port: u32, deadline: Option<Instant>) -> Result<DuplexOwner, Refusal> {
        duplex(self.stream_before(port, deadline)?)
    }

    /// A Shards stream is handed to its caller as its own socket (the review's D-5). On Windows, where it is two pipes, an
    /// exec's stream is carried on a pipe of the caller's instead ([`Link::dial_piped`]).
    #[cfg(unix)]
    fn dial_handoff_before(
        &self,
        port: u32,
        deadline: Option<Instant>,
    ) -> Option<Result<Handoff, Refusal>> {
        Some(self.stream_before(port, deadline).and_then(handoff))
    }

    /// An exec's stream on a Shards connection of its own, carried on a pipe whose end is the caller's ([`piped`]).
    #[cfg(windows)]
    fn dial_piped(
        &self,
        named: hex_core::stream::Named,
        deadline: Option<Instant>,
    ) -> Option<Result<Piped, Refusal>> {
        Some(self.piped(named, deadline))
    }
}

#[cfg(windows)]
impl ShardsLink {
    /// [`Link::dial_piped`]: a stdio connection within `deadline` where the caller gives one, named and carried.
    fn piped(
        &self,
        named: hex_core::stream::Named,
        deadline: Option<Instant>,
    ) -> Result<Piped, Refusal> {
        let cap = deadline.map(ReplyDeadline::caller_before).transpose()?;
        let stream = self.stream_before(hex_core::stream::STDIO_PORT, deadline)?;
        piped(stream, named, cap)
    }
}

/// `stream` as a connection the client waits on: itself on Unix, where it is a stream socket.
#[cfg(unix)]
fn duplex(stream: ShardsStream) -> Result<DuplexOwner, Refusal> {
    own_duplex(stream)
}

/// `stream` as a connection the client waits on, on Windows: its two pipes bridged to a local socket ([`bridge`]).
#[cfg(windows)]
fn duplex(stream: ShardsStream) -> Result<DuplexOwner, Refusal> {
    own_duplex(bridge(stream)?)
}

/// `stream` as a connection handed to its caller whole: its own socket.
#[cfg(unix)]
pub(crate) fn handoff(stream: ShardsStream) -> Result<Handoff, Refusal> {
    Ok(Handoff::Shards(stream))
}

/// An exec's stream on its own Shards connection `stream`, carried on a pipe whose end is the caller's (Windows; HEX-RT-17:
/// a caller there is handed handles alone, and a Shards connection is two pipes, Shards' D272, which no one handle
/// carries both ways), its name written first, within `deadline` where the call gives one. An output stream's connection
/// ends its own half once named, and a thread copies the connection into the pipe ([`piped_out`]); stdin's copies the
/// pipe into the connection ([`piped_in`]): one thread a stream, as a node VM's lane carried so ([`super::lanes`]). A
/// pipe carries no reset: an output's cut is told by its [`Piped::cut`], where the node watches it.
#[cfg(windows)]
pub(crate) fn piped(
    stream: ShardsStream,
    named: hex_core::stream::Named,
    deadline: Option<ReplyDeadline>,
) -> Result<Piped, Refusal> {
    const NAMING: &str = "naming a stdio connection";
    let conn: &dyn shards_api::Stream = &**stream.0;
    if let Some(deadline) = deadline {
        conn.set_write_timeout(Some(deadline.remaining(NAMING)?))
            .map_err(|e| Refusal::os(NAMING, e))?;
    }
    let mut to = conn;
    to.write_all(&hex_core::stream::name(named))
        .map_err(|e| Refusal::os(NAMING, e))?;
    if deadline.is_some() {
        conn.set_write_timeout(None)
            .map_err(|e| Refusal::os(NAMING, e))?;
    }
    let (from, to) =
        std::io::pipe().map_err(|e| Refusal::os("making an exec's stream's pipe", e))?;
    match named.stream {
        hex_core::stream::Stdio::In => {
            std::thread::Builder::new()
                .name("hex-shards-stdin".to_owned())
                .spawn(move || piped_in(from, stream))
                .map_err(|e| Refusal::os("starting a stdin connection's copier", e))?;
            Ok(Piped {
                end: to.into(),
                cut: None,
            })
        }
        hex_core::stream::Stdio::Out | hex_core::stream::Stdio::Err => {
            // It carries nothing this way once named: its peer's End then closes the stream whole.
            conn.shutdown(Shutdown::Write)
                .map_err(|e| Refusal::os("ending an output stream's own half", e))?;
            let cut = Cut::default();
            let recorded = cut.clone();
            std::thread::Builder::new()
                .name("hex-shards-output".to_owned())
                .spawn(move || piped_out(stream, to, &recorded))
                .map_err(|e| Refusal::os("starting an output connection's copier", e))?;
            Ok(Piped {
                end: from.into(),
                cut: Some(cut),
            })
        }
    }
}

/// Stdin's connection: what the caller writes to its pipe, into the connection, its pipe's end the connection's own end
/// of writing. A write the connection refuses (the program let its stdin go, HEX-GUEST-41) lets the pipe go, so the
/// caller's next write fails, as a closed stdin's does. The connection's reading, which carries nothing this way, is held
/// to its end, so the guest's relay is never cut off before it has read what came.
#[cfg(windows)]
fn piped_in(mut from: std::io::PipeReader, stream: ShardsStream) {
    let conn: &dyn shards_api::Stream = &**stream.0;
    let (mut to, mut rest) = (conn, conn);
    let mut buf = vec![0u8; hex_core::lanes::DATA_BOUND];
    loop {
        match from.read(&mut buf) {
            Ok(0) => {
                if conn.shutdown(Shutdown::Write).is_err() {
                    return;
                }
                break;
            }
            Ok(n) => {
                if to.write_all(buf.get(..n).unwrap_or_default()).is_err() {
                    return;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => {
                let _ = conn.shutdown(Shutdown::Both);
                return;
            }
        }
    }
    drop(from);
    while matches!(rest.read(&mut buf), Ok(n) if n > 0) {}
}

/// An output stream's connection: its bytes into the caller's pipe, its end the pipe's, and its error the cut, recorded
/// before the pipe ends. A caller gone from its pipe lets the connection go, so the guest's next write on it fails, as a
/// program's write fails once its stdout's pipe has no reader.
#[cfg(windows)]
fn piped_out(stream: ShardsStream, mut to: std::io::PipeWriter, cut: &Cut) {
    let mut from: &dyn shards_api::Stream = &**stream.0;
    let mut buf = vec![0u8; hex_core::lanes::DATA_BOUND];
    loop {
        match from.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => {
                if to.write_all(buf.get(..n).unwrap_or_default()).is_err() {
                    return;
                }
            }
            Err(e) => {
                cut.record(&e);
                return;
            }
        }
    }
}

/// `stream` as the owner's end of a local socket pair, its bytes the stream's (Windows; Shards' D272): two threads copy
/// between the pair's other end and the stream, one a way, each through the stream's shared reads or writes (`&dyn
/// Stream`'s, the handle the client holds, so no duplicate of its pipes is made: the review's D-5), sharing the pair's
/// end, one reading it and one writing it. Each ends on a fact: the owner's end of writing or its close, the stream's
/// end, or its error, which the owner reads as the cut after the bytes before it. Its cost is a node VM lane's (two
/// threads and one copy a way, [`super::lanes`]'s bridge) until the node's completion port waits on a pipe itself.
#[cfg(windows)]
fn bridge(stream: ShardsStream) -> Result<Bridged, Refusal> {
    let bridged = || -> io::Result<Bridged> {
        let (owner, ours) = pair()?;
        let cut = Cut::default();
        let ours = Arc::new(ours);
        let (up, writes) = (Arc::clone(&ours), triomphe::Arc::clone(&stream.0));
        std::thread::Builder::new()
            .name("hex-shards-up".to_owned())
            .spawn(move || upward(&**writes, &up))?;
        let (reads, recorded) = (stream.0, cut.clone());
        std::thread::Builder::new()
            .name("hex-shards-down".to_owned())
            .spawn(move || downward(&**reads, &ours, &recorded))?;
        Ok(Bridged::new(owner, cut))
    };
    bridged().map_err(|e| Refusal::os("bridging a Shards guest stream to a local socket", e))
}

/// What the owner writes, to the stream; the owner's end of writing, or its close, is the stream's own end of writing.
/// A write the stream refuses (its end, or its guest's) ends this way, the owner's writes then failing at [`Bridged`]'s
/// write once the cut is recorded, and what the owner sends meanwhile read here and let go ([`writes_refused`]). An error
/// reading the pair ends the stream both ways.
#[cfg(windows)]
fn upward(stream: &dyn shards_api::Stream, mut from: &Local) {
    let mut to = stream;
    let mut buf = vec![0u8; hex_core::lanes::DATA_BOUND];
    loop {
        match from.read(&mut buf) {
            Ok(0) => {
                let _ = stream.shutdown(Shutdown::Write);
                return;
            }
            Ok(n) => {
                if to.write_all(buf.get(..n).unwrap_or_default()).is_err() {
                    writes_refused(from, &mut buf);
                    return;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => {
                let _ = stream.shutdown(Shutdown::Both);
                return;
            }
        }
    }
}

/// The owner's writes once the stream refuses them: read and let go to the owner's end of writing, its close or its
/// reset, so the pair's other end never closes with bytes unread, which would reset the owner's socket and drop what it
/// hasn't read (Winsock, the review's D-4), as a lane's bridge does.
#[cfg(windows)]
fn writes_refused(mut from: &Local, buf: &mut [u8]) {
    loop {
        match from.read(buf) {
            Ok(0) => return,
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return,
        }
    }
}

/// What the guest sends, to the owner; the stream's end is the owner's end of input, and its error the cut, recorded
/// before the owner's socket ends. An owner gone ends the copy, its handle on the stream let go with it.
#[cfg(windows)]
fn downward(stream: &dyn shards_api::Stream, mut to: &Local, cut: &Cut) {
    let mut from = stream;
    let mut buf = vec![0u8; hex_core::lanes::DATA_BOUND];
    loop {
        match from.read(&mut buf) {
            Ok(0) => {
                let _ = to.shutdown(Shutdown::Write);
                return;
            }
            Ok(n) => {
                if to.write_all(buf.get(..n).unwrap_or_default()).is_err() {
                    end(to);
                    return;
                }
            }
            Err(e) => {
                cut.record(&e);
                end(to);
                return;
            }
        }
    }
}

/// Shards' refusal as Hex's: a missing backend is the platform's, a bound is a spec's, a CONNECT's
/// failure keeps its port, phase and cause (Shards R04), an owned guest's or a borrowed one's; the
/// rest is the guest's channel or its end, which Hex reads as an operating-system failure naming
/// what it did.
pub fn refusal(op: &'static str, why: &shards_api::Refusal) -> Refusal {
    match why {
        shards_api::Refusal::NoBackend { os, arch } => Refusal::Platform {
            what: "the oci class",
            platform: os,
            why: format!(
                "Shards has no backend on {os} {arch} to boot the Linux guest the class runs in"
            ),
        },
        shards_api::Refusal::Bounds { what, why } => Refusal::Invalid {
            what: "the node's guest",
            why: format!("its {what}: {why}"),
        },
        shards_api::Refusal::Gpu(why) => {
            use shards_api::PhysicalGpuError;
            let message = match why {
                PhysicalGpuError::Syntax => "malformed native GPU selector",
                PhysicalGpuError::PciAddress => "GPU PCI address fields are out of range",
                PhysicalGpuError::MetalRegistry => "Metal registry identity is unavailable",
                PhysicalGpuError::Count => "GPU count exceeds the native device address space",
                PhysicalGpuError::Duplicate => "GPU selection repeats a native identity",
                PhysicalGpuError::Allocation => {
                    return Refusal::Exhausted {
                        what: "physical GPU selection storage",
                        bound: shards_api::physical::MAX_PHYSICAL_GPUS,
                    };
                }
            };
            let mut reason = String::new();
            if reason.try_reserve_exact(message.len()).is_err() {
                return Refusal::Exhausted {
                    what: "physical GPU refusal text",
                    bound: message.len(),
                };
            }
            reason.push_str(message);
            Refusal::Invalid {
                what: "physical GPU selection",
                why: reason,
            }
        }
        // The VM process refused its selection, as it said in place of a receipt (Shards 18f59d497): certain, the
        // selection's own refusal, with its reason, the join's or the renderer's.
        shards_api::Refusal::GpuProbe(shards_api::GpuProbeError::Refused(refused)) => {
            Refusal::Invalid {
                what: "physical GPU selection",
                why: format!("its VM process refused it: {refused}"),
            }
        }
        // Its report writer closed with nothing written: the VM process ended before its renderer said anything of its
        // selection, which is left unknown; the process's own end says why.
        shards_api::Refusal::GpuProbe(shards_api::GpuProbeError::Unreported) => Refusal::Os {
            op,
            why: "its VM process ended before its renderer said anything of its GPU selection: no report \
                  was written"
                .to_owned(),
        },
        shards_api::Refusal::Connect { port, failure }
        | shards_api::Refusal::Borrowed {
            port,
            failure: shards_api::BorrowFailure::Channel(failure),
        } => Refusal::Connect {
            port: *port,
            failure: connect_failure(failure),
        },
        // The guest's own failures, each in its kind (the review's B-15).
        shards_api::Refusal::Stalled(stall) => Refusal::Guest {
            op,
            failure: GuestFailure::Stalled {
                waited: stall.waited(),
                over: stall.over(),
                slowest: stall.slowest(),
            },
        },
        shards_api::Refusal::Ended(end) => Refusal::Guest {
            op,
            failure: GuestFailure::Ended(match end {
                shards_api::End::PowerOff => GuestEnd::PowerOff,
                shards_api::End::Reset => GuestEnd::Reset,
                shards_api::End::Stopped => GuestEnd::Stopped,
                shards_api::End::Failed(why) => GuestEnd::Failed(why.clone()),
            }),
        },
        shards_api::Refusal::Gone { pid, why } => Refusal::Guest {
            op,
            failure: GuestFailure::Gone {
                pid: *pid,
                why: why.clone(),
            },
        },
        shards_api::Refusal::Rate(why) => Refusal::Guest {
            op,
            failure: GuestFailure::Rate(why.clone()),
        },
        shards_api::Refusal::HostUse(why) => Refusal::Guest {
            op,
            failure: GuestFailure::HostUse(why.to_string()),
        },
        other => Refusal::os(op, other),
    }
}

fn connect_failure(failure: &shards_api::HandshakeError) -> ConnectFailure {
    use shards_api::{HandshakeError, ReplyError};
    match failure {
        HandshakeError::Expired { phase } => ConnectFailure::Expired {
            phase: connect_phase(*phase),
        },
        HandshakeError::Io { phase, kind, code } => ConnectFailure::Io {
            phase: connect_phase(*phase),
            kind: IoKind::new(*kind),
            code: *code,
        },
        HandshakeError::Closed => ConnectFailure::Closed,
        HandshakeError::Reply(ReplyError::TooLong) => ConnectFailure::Reply(ConnectReply::TooLong),
        HandshakeError::Reply(ReplyError::NotOk) => ConnectFailure::Reply(ConnectReply::NotOk),
    }
}

fn connect_phase(phase: shards_api::HandshakePhase) -> ConnectPhase {
    match phase {
        shards_api::HandshakePhase::Dial => ConnectPhase::Dial,
        shards_api::HandshakePhase::Write => ConnectPhase::Write,
        shards_api::HandshakePhase::Read => ConnectPhase::Read,
        shards_api::HandshakePhase::ClearTimeouts => ConnectPhase::ClearTimeouts,
    }
}

#[cfg(test)]
mod connect_original_tests;
