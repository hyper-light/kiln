use std::io::{Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use hex_api::{BoxId, ConnectFailure, ConnectPhase, ConnectReply, IoKind, Refusal};
use hex_client::guest::{Duplex, DuplexOwner, Link, own_duplex};
use rustix::event::{PollFd, PollFlags, Timespec};
use serde::Deserialize;
use socket2::{Domain, SockAddr, Socket, Type};

use super::{exhausted, invalid};

pub(super) struct Endpoint {
    pub path: PathBuf,
    pub timeout: Duration,
    /// Present only while creating a VM. Normal control/stdio connections have no lifetime cap.
    pub create_deadline: Option<Instant>,
}

impl Endpoint {
    fn remaining(&self) -> Duration {
        self.create_deadline.map_or(self.timeout, |until| {
            self.timeout
                .min(until.saturating_duration_since(Instant::now()))
        })
    }
}

/// Why one bounded socket step failed. The CONNECT handshake types it by its phase
/// ([`Step::connect`]); the API witness and readiness reads keep their OS text (`From`).
enum Step {
    /// The step's absolute deadline passed.
    Expired,
    /// The peer ended the stream during the step `op`.
    Ended(&'static str),
    /// The OS refused the step `op`.
    Io(&'static str, std::io::Error),
    /// A local bound or invariant, already typed.
    Refused(Refusal),
}

impl Step {
    /// A failed step of Firecracker's CONNECT/OK handshake (vsock.md) as Hex's typed carrier, as the
    /// Shards adapter carries Shards' own (hex-client guest/shards.rs): an end of stream before the
    /// answer's newline is Closed, one while writing a zero write.
    fn connect(self, port: u32, phase: ConnectPhase) -> Refusal {
        let failure = match self {
            Self::Refused(refusal) => return refusal,
            Self::Expired => ConnectFailure::Expired { phase },
            Self::Ended(_) if phase == ConnectPhase::Read => ConnectFailure::Closed,
            Self::Ended(_) => ConnectFailure::Io {
                phase,
                kind: IoKind::new(std::io::ErrorKind::WriteZero),
                code: None,
            },
            Self::Io(_, error) => ConnectFailure::Io {
                phase,
                kind: IoKind::new(error.kind()),
                code: error.raw_os_error(),
            },
        };
        Refusal::Connect { port, failure }
    }
}

impl From<Step> for Refusal {
    fn from(step: Step) -> Self {
        match step {
            Step::Expired => Refusal::os("waiting for Firecracker", "operation deadline elapsed"),
            Step::Ended(op) => Refusal::os(op, "connection ended"),
            Step::Io(op, error) => Refusal::os(op, error),
            Step::Refused(refusal) => refusal,
        }
    }
}

fn left(deadline: Instant) -> Result<Duration, Step> {
    let duration = deadline.saturating_duration_since(Instant::now());
    if duration.is_zero() {
        Err(Step::Expired)
    } else {
        Ok(duration)
    }
}

/// A read or write bounded by `until` that failed: once `until` has passed, the deadline is why.
fn failed(until: Instant, op: &'static str, error: std::io::Error) -> Step {
    match left(until) {
        Ok(_) => Step::Io(op, error),
        Err(expired) => expired,
    }
}

fn deadline(timeout: Duration) -> Result<Instant, Refusal> {
    if timeout.is_zero() {
        return Err(invalid("operation deadline has elapsed"));
    }
    Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| invalid("operation deadline is unrepresentable"))
}

fn wait(fd: impl AsFd, flags: PollFlags, until: Instant) -> Result<(), Step> {
    loop {
        let duration = left(until)?;
        let timespec = Timespec {
            tv_sec: i64::try_from(duration.as_secs())
                .map_err(|_| Step::Refused(invalid("operation deadline is unrepresentable")))?,
            tv_nsec: i64::from(duration.subsec_nanos()),
        };
        let mut descriptors = [PollFd::new(&fd, flags)];
        match rustix::event::poll(&mut descriptors, Some(&timespec)) {
            Ok(0) => return Err(Step::Expired),
            Ok(_) => return Ok(()),
            Err(rustix::io::Errno::INTR) => continue,
            Err(error) => return Err(Step::Io("waiting for Firecracker", error.into())),
        }
    }
}

fn connect(path: &Path, until: Instant) -> Result<UnixStream, Step> {
    let socket = Socket::new(Domain::UNIX, Type::STREAM, None)
        .map_err(|e| Step::Io("opening Firecracker connection", e))?;
    socket
        .set_nonblocking(true)
        .map_err(|e| Step::Io("making Firecracker connection nonblocking", e))?;
    let address = SockAddr::unix(path).map_err(|e| Step::Io("naming Firecracker connection", e))?;
    match socket.connect(&address) {
        Ok(()) => {}
        Err(error)
            if error.kind() == std::io::ErrorKind::WouldBlock
                || error.raw_os_error() == Some(libc::EINPROGRESS) =>
        {
            wait(&socket, PollFlags::OUT, until)?;
            if let Some(error) = socket
                .take_error()
                .map_err(|e| Step::Io("checking Firecracker connection", e))?
            {
                return Err(Step::Io("connecting to Firecracker", error));
            }
        }
        Err(error) => return Err(Step::Io("connecting to Firecracker", error)),
    }
    socket
        .set_nonblocking(false)
        .map_err(|e| Step::Io("configuring Firecracker connection", e))?;
    let descriptor: OwnedFd = socket.into();
    Ok(UnixStream::from(descriptor))
}

fn write(stream: &mut impl Duplex, mut bytes: &[u8], until: Instant) -> Result<(), Step> {
    while !bytes.is_empty() {
        stream
            .set_write_timeout(Some(left(until)?))
            .map_err(|e| Step::Io("bounding Firecracker write", e))?;
        let count = match stream.write(bytes) {
            Ok(count) => count,
            // A signal fails a socket call with a timeout set, never restarted (signal(7)). Make
            // it again: the next pass checks the same absolute deadline first, adding no wait.
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(failed(until, "writing Firecracker request", error)),
        };
        if count == 0 {
            return Err(Step::Ended("writing Firecracker request"));
        }
        bytes = bytes
            .get(count..)
            .ok_or_else(|| Step::Refused(invalid("write count exceeds request")))?;
    }
    Ok(())
}

fn read_exact(stream: &mut impl Duplex, mut bytes: &mut [u8], until: Instant) -> Result<(), Step> {
    while !bytes.is_empty() {
        stream
            .set_read_timeout(Some(left(until)?))
            .map_err(|e| Step::Io("bounding Firecracker read", e))?;
        let count = match stream.read(bytes) {
            Ok(count) => count,
            // As in `write`: interrupted, made again within the same absolute deadline.
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(failed(until, "reading Firecracker response", error)),
        };
        if count == 0 {
            return Err(Step::Ended("reading Firecracker response"));
        }
        bytes = bytes
            .get_mut(count..)
            .ok_or_else(|| Step::Refused(invalid("read count exceeds response")))?;
    }
    Ok(())
}

/// Firecracker's host-initiated vsock handshake (vsock.md) on a connected `stream`: `CONNECT
/// <port>\n` out, `OK <host-port>\n` back, then the stream's timeouts cleared for its owner.
fn handshake(stream: &mut impl Duplex, port: u32, until: Instant) -> Result<(), Refusal> {
    // A u32 has at most ten decimal digits; the pinned vsock protocol adds this fixed
    // prefix/newline. Cursor refuses an overflow instead of allocating a formatting String.
    let mut request = [0u8; b"CONNECT ".len() + 10 + 1];
    let used = {
        let mut buffer = std::io::Cursor::new(request.as_mut_slice());
        writeln!(&mut buffer, "CONNECT {port}").map_err(|error| {
            Step::Io("encoding Firecracker connect request", error)
                .connect(port, ConnectPhase::Write)
        })?;
        usize::try_from(buffer.position())
            .map_err(|_| invalid("vsock request size is unrepresentable"))?
    };
    write(
        &mut *stream,
        request
            .get(..used)
            .ok_or_else(|| invalid("vsock request exceeds its fixed buffer"))?,
        until,
    )
    .map_err(|step| step.connect(port, ConnectPhase::Write))?;
    // Firecracker vsock.md: `OK <host-port>\n`, the port an unsigned 32-bit integer.
    let bound = "OK "
        .len()
        .checked_add(10)
        .and_then(|n| n.checked_add(1))
        .ok_or_else(|| invalid("vsock response bound overflow"))?;
    let mut line = Vec::new();
    line.try_reserve_exact(bound)
        .map_err(|_| exhausted("vsock response", bound))?;
    while line.len() < bound {
        let mut byte = [0u8; 1];
        read_exact(&mut *stream, &mut byte, until)
            .map_err(|step| step.connect(port, ConnectPhase::Read))?;
        let byte = *byte.first().ok_or_else(|| invalid("empty vsock byte"))?;
        line.push(byte);
        if byte == b'\n' {
            break;
        }
    }
    let reply = |reply| Refusal::Connect {
        port,
        failure: ConnectFailure::Reply(reply),
    };
    if line.last() != Some(&b'\n') {
        return Err(reply(ConnectReply::TooLong));
    }
    if std::str::from_utf8(&line)
        .ok()
        .and_then(|text| text.strip_prefix("OK "))
        .and_then(|n| n.strip_suffix('\n'))
        .and_then(|n| n.parse::<u32>().ok())
        .filter(|n| *n > 0)
        .is_none()
    {
        return Err(reply(ConnectReply::NotOk));
    }
    stream
        .set_read_timeout(None)
        .and_then(|()| stream.set_write_timeout(None))
        .map_err(|e| {
            Step::Io("handing off guest stream", e).connect(port, ConnectPhase::ClearTimeouts)
        })?;
    Ok(())
}

impl Link for Endpoint {
    fn reply_budget(&self) -> Option<Duration> {
        Some(self.remaining())
    }

    fn dial(&self, port: u32) -> Result<DuplexOwner, Refusal> {
        self.dial_before(port, None)
    }

    fn dial_before(
        &self,
        port: u32,
        caller_deadline: Option<Instant>,
    ) -> Result<DuplexOwner, Refusal> {
        // The configured operation budget is positive (config.rs), so none left is a spent create.
        let remaining = self.remaining();
        if remaining.is_zero() {
            return Err(Step::Expired.connect(port, ConnectPhase::Dial));
        }
        let own = deadline(remaining)?;
        let until = caller_deadline.map_or(own, |caller| own.min(caller));
        left(until).map_err(|step| step.connect(port, ConnectPhase::Dial))?;
        let mut stream =
            connect(&self.path, until).map_err(|step| step.connect(port, ConnectPhase::Dial))?;
        handshake(&mut stream, port, until)?;
        if port == hex_core::stream::files::FILES_PORT {
            // A staging connection is exactly one operation. Its current client otherwise resets
            // per-read timeouts and may be uncalibrated; retain one explicit absolute stage budget.
            return own_duplex(StagingStream { stream, until });
        }
        own_duplex(stream)
    }
}

struct StagingStream {
    stream: UnixStream,
    until: Instant,
}

impl StagingStream {
    fn limit(&self, requested: Option<Duration>) -> std::io::Result<Option<Duration>> {
        let remaining = self.until.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "Firecracker staging operation deadline elapsed",
            ));
        }
        Ok(Some(
            requested.map_or(remaining, |requested| requested.min(remaining)),
        ))
    }
}

impl Read for StagingStream {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        self.set_read_timeout(self.stream.read_timeout()?)?;
        self.stream.read(bytes)
    }
}

impl Write for StagingStream {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.set_write_timeout(self.stream.write_timeout()?)?;
        self.stream.write(bytes)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.stream.flush()
    }
}

impl Duplex for StagingStream {
    fn try_clone(&self) -> std::io::Result<DuplexOwner> {
        own_duplex(Self {
            stream: self.stream.try_clone()?,
            until: self.until,
        })
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::OutOfMemory))
    }
    fn shutdown(&self, how: std::net::Shutdown) -> std::io::Result<()> {
        self.stream.shutdown(how)
    }
    fn set_write_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()> {
        self.stream.set_write_timeout(self.limit(timeout)?)
    }
    fn set_read_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()> {
        self.stream.set_read_timeout(self.limit(timeout)?)
    }
    fn handle(&self) -> hex_client::guest::Handle<'_> {
        self.stream.as_fd()
    }
    fn set_nonblocking(&self, on: bool) -> std::io::Result<()> {
        self.stream.set_nonblocking(on)
    }
}

/// What a VMM's refusal says of it when it ends before its guest is ready.
const ENDED: &str = "Firecracker ended before guest readiness";
const SAID: &str = "; its last output:\n";

/// The end of a VMM's output (stdout and stderr, one pipe), kept while create waits for readiness:
/// as many bytes as the refusal carrying them can hold in one wire answer, sized by the refused
/// answer's own codec, so no size is picked here.
struct Tail {
    bytes: std::collections::VecDeque<u8>,
    room: usize,
}

impl Tail {
    fn new() -> Result<Self, Refusal> {
        let mut text = String::new();
        let bound = ENDED
            .len()
            .checked_add(SAID.len())
            .ok_or_else(|| invalid("readiness refusal text overflows"))?;
        text.try_reserve_exact(bound)
            .map_err(|_| exhausted("readiness refusal text", bound))?;
        text.push_str(ENDED);
        text.push_str(SAID);
        let base =
            hex_core::wire::snapshot_refusal(&Refusal::Broken(text), hex_core::wire::MAX_MESSAGE)
                .map_err(|_| invalid("readiness refusal exceeds the wire bound"))?
                .len();
        let room = hex_core::wire::MAX_MESSAGE
            .checked_sub(base)
            .ok_or_else(|| invalid("readiness refusal exceeds the wire bound"))?;
        let mut bytes = std::collections::VecDeque::new();
        bytes
            .try_reserve_exact(room)
            .map_err(|_| exhausted("VMM output tail", room))?;
        Ok(Self { bytes, room })
    }

    fn keep(&mut self, chunk: &[u8]) {
        let chunk = chunk
            .get(chunk.len().saturating_sub(self.room)..)
            .unwrap_or_default();
        let excess = self
            .bytes
            .len()
            .saturating_add(chunk.len())
            .saturating_sub(self.room)
            .min(self.bytes.len());
        self.bytes.drain(..excess);
        self.bytes.extend(chunk);
    }

    /// Reads what the pipe holds now without waiting; true at its end (every writer closed).
    fn drain(&mut self, output: &mut std::io::PipeReader, until: Instant) -> Result<bool, Refusal> {
        // One read takes at most a pipe's atomic write unit (pipe(7)); the tail keeps the last
        // `room` bytes of them all.
        let mut buffer = [0u8; rustix::pipe::PIPE_BUF];
        loop {
            left(until)?;
            match output.read(&mut buffer) {
                Ok(0) => return Ok(true),
                Ok(count) => self.keep(
                    buffer
                        .get(..count)
                        .ok_or_else(|| invalid("VMM output read overflows"))?,
                ),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(false),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => return Err(Refusal::os("reading VMM output", error)),
            }
        }
    }

    fn refusal(&self) -> Refusal {
        let (front, back) = self.bytes.as_slices();
        let mut raw = Vec::new();
        if self.bytes.is_empty() || raw.try_reserve_exact(self.bytes.len()).is_err() {
            return Refusal::Broken(ENDED.into());
        }
        raw.extend_from_slice(front);
        raw.extend_from_slice(back);
        // A lossy text can outgrow its bytes (U+FFFD for an invalid one): keep its end.
        readiness(&String::from_utf8_lossy(&raw), self.room)
    }
}

/// A readiness refusal: `ENDED`, `SAID`, then the end of `output` that `room` bytes hold, cut at a character.
fn readiness(output: &str, room: usize) -> Refusal {
    let mut start = output.len().saturating_sub(room);
    while start < output.len() && !output.is_char_boundary(start) {
        start = start.saturating_add(1);
    }
    let tail = output.get(start..).unwrap_or_default();
    let mut why = String::new();
    let bound = ENDED
        .len()
        .saturating_add(SAID.len())
        .saturating_add(tail.len());
    if why.try_reserve_exact(bound).is_err() {
        return Refusal::Broken(ENDED.into());
    }
    why.push_str(ENDED);
    why.push_str(SAID);
    why.push_str(tail);
    Refusal::Broken(why)
}

/// A create's `refusal` and its failed `cleanup` together, as [`Refusal::Uncertain`] (AC-T7.4.1), within one refused
/// answer under the wire bound, as a readiness refusal is alone: a readiness refusal keeps the end of its VMM's output
/// that fits beside the box's id and the cleanup's refusal, the room measured by the wire itself, as [`Tail::new`]
/// measures it.
pub(super) fn uncertain_after(id: BoxId, refusal: Refusal, cleanup: Refusal) -> Refusal {
    let output = match &refusal {
        Refusal::Broken(why) => why
            .strip_prefix(ENDED)
            .and_then(|rest| rest.strip_prefix(SAID)),
        _ => None,
    };
    let Some(output) = output else {
        return Refusal::Uncertain {
            id,
            refusal: Box::new(refusal),
            cleanup: Box::new(cleanup),
        };
    };
    let mut whole = Refusal::Uncertain {
        id,
        refusal: Box::new(readiness("", 0)),
        cleanup: Box::new(cleanup),
    };
    let room = hex_core::wire::snapshot_refusal(&whole, hex_core::wire::MAX_MESSAGE)
        .map_or(0, |bytes| {
            hex_core::wire::MAX_MESSAGE.saturating_sub(bytes.len())
        });
    if let Refusal::Uncertain { refusal: kept, .. } = &mut whole {
        **kept = readiness(output, room);
    }
    whole
}

/// Waits for the guest's private readiness frame. While it waits it keeps the end of the VMM's
/// `output`, so a VMM that ends first is refused with what it wrote; the caller drops `output`
/// once this returns, after which the VMM's writes fail with EPIPE, which it counts and drops
/// (its logger, serial device and SIGPIPE handler at the pinned commit), so nothing grows.
pub(super) fn ready(
    listener: &UnixListener,
    proc: &dyn shards_api::ManagedProcess,
    output: &mut std::io::PipeReader,
    until: Instant,
) -> Result<(), Refusal> {
    left(until)?;
    let flags = rustix::fs::fcntl_getfl(&*output)
        .map_err(|e| Refusal::os("reading VMM output flags", e))?;
    rustix::fs::fcntl_setfl(&*output, flags | rustix::fs::OFlags::NONBLOCK)
        .map_err(|e| Refusal::os("bounding VMM output reads", e))?;
    let mut tail = Tail::new()?;
    let mut writing = true;
    let identity = proc
        .identity()
        .map_err(|_| invalid("native readiness receipt has no identity"))?;
    let raw = i32::try_from(identity.pid)
        .map_err(|_| invalid("readiness identity is unrepresentable"))?;
    let pid = rustix::process::Pid::from_raw(raw)
        .ok_or_else(|| invalid("readiness identity is invalid"))?;
    // Readonly event subscription only. The SAME managed receipt remains sole reaping/stopping
    // authority; this descriptor performs no waitid, signal or native lifecycle transition. It is
    // the VMM's when, once it is open, a process with the VMM's recorded start runs at its pid. A
    // VMM that ended first leaves none (start_time reads none for a zombie; a reaped one has no
    // pid), which says nothing against its identity: its receipt, asked below before the observer
    // is needed, says it ended, and it is refused with what it wrote.
    let observer = match rustix::process::pidfd_open(pid, rustix::process::PidfdFlags::empty()) {
        Ok(observer)
            if identity.started != 0
                && crate::linux::oci::state::start_time(raw) == Some(identity.started) =>
        {
            Some(observer)
        }
        Ok(_) | Err(rustix::io::Errno::SRCH) => None,
        Err(e) => return Err(Refusal::os("observing native readiness controller", e)),
    };

    listener
        .set_nonblocking(true)
        .map_err(|e| Refusal::os("bounding guest readiness listener", e))?;
    loop {
        if writing {
            writing = !tail.drain(output, until)?;
        }
        if proc
            .terminal()
            .map_err(|_| invalid("native readiness observation failed"))?
            .is_some()
        {
            if writing {
                tail.drain(output, until)?;
            }
            return Err(tail.refusal());
        }
        // Not ended, its receipt says, yet none ran with its identity when the observer was opened.
        let Some(observer) = &observer else {
            return Err(invalid("native readiness identity changed"));
        };
        match listener.accept() {
            Ok((mut stream, _)) => {
                let mut frame = [0u8; hex_core::stream::GUEST_READY.len()];
                read_exact(&mut stream, &mut frame, until)?;
                if frame.as_slice() != hex_core::stream::GUEST_READY {
                    return Err(invalid("guest readiness frame disagrees"));
                }
                return Ok(());
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                let duration = left(until)?;
                let timespec = Timespec {
                    tv_sec: i64::try_from(duration.as_secs())
                        .map_err(|_| invalid("readiness deadline overflow"))?,
                    tv_nsec: i64::from(duration.subsec_nanos()),
                };

                let mut descriptors = [
                    PollFd::new(listener, PollFlags::IN),
                    PollFd::new(observer, PollFlags::IN),
                    PollFd::new(&*output, PollFlags::IN),
                ];
                // An ended pipe stays readable (POLLHUP): it leaves the set once read to its end.
                let watched = descriptors
                    .get_mut(..if writing { 3 } else { 2 })
                    .ok_or_else(|| invalid("readiness poll set overflows"))?;
                match rustix::event::poll(watched, Some(&timespec)) {
                    Ok(0) => {
                        return Err(Refusal::os(
                            "waiting for guest readiness",
                            "boot deadline elapsed",
                        ));
                    }
                    Ok(_) | Err(rustix::io::Errno::INTR) => {}
                    Err(error) => return Err(Refusal::os("waiting for guest readiness", error)),
                }
            }
            Err(error) => return Err(Refusal::os("accepting guest readiness", error)),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Attachment {
    pub id: String,
    pub sbdf: String,
    pub guest_sbdf: String,
}

/// Readonly witness supplied by Hex's pinned Firecracker extension, never an ordinal guess.
pub(super) fn attachment(
    api: &Path,
    id: &str,
    timeout: Duration,
    original: Option<Instant>,
) -> Result<Attachment, Refusal> {
    if id
        .bytes()
        .any(|c| !c.is_ascii_alphanumeric() && !matches!(c, b'_' | b'-' | b':' | b'.'))
    {
        return Err(invalid("invalid attachment witness identifier"));
    }
    let own = deadline(timeout)?;
    let until = original.map_or(own, |original| own.min(original));
    left(until)?;
    let prefix = "GET /device-passthrough/";
    let suffix = " HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    let size = prefix
        .len()
        .checked_add(id.len())
        .and_then(|size| size.checked_add(suffix.len()))
        .ok_or_else(|| exhausted("Firecracker API request", hex_core::wire::MAX_MESSAGE))?;
    let mut request = String::new();
    request
        .try_reserve_exact(size)
        .map_err(|_| exhausted("Firecracker API request", size))?;
    request.push_str(prefix);
    request.push_str(id);
    request.push_str(suffix);
    let mut socket = connect(api, until)?;
    write(&mut socket, request.as_bytes(), until)?;
    witness(&mut socket, until)
}

/// The attachment witness's whole HTTP response on `socket`, bounded by the API message size and
/// its own exact Content-Length, read within `until`.
fn witness(socket: &mut impl Duplex, until: Instant) -> Result<Attachment, Refusal> {
    let bound = hex_core::wire::MAX_MESSAGE;
    let room = bound
        .checked_add(1)
        .ok_or_else(|| exhausted("Firecracker API response", bound))?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(room)
        .map_err(|_| exhausted("Firecracker API response", room))?;
    bytes.resize(room, 0);
    let mut used = 0usize;
    let mut header_end = None;
    let mut response_end = None;
    loop {
        if response_end.is_some_and(|end| used == end) {
            break;
        }
        let end = response_end.unwrap_or(room);
        socket
            .set_read_timeout(Some(left(until)?))
            .map_err(|e| Refusal::os("bounding Firecracker API", e))?;
        let before = used;
        let tail = bytes
            .get_mut(used..end)
            .filter(|tail| !tail.is_empty())
            .ok_or_else(|| exhausted("Firecracker API response", bound))?;
        let count = match socket.read(tail) {
            Ok(count) => count,
            // As in `write`: interrupted, made again within the same absolute deadline.
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(Refusal::os("reading Firecracker API", error)),
        };
        if count == 0 {
            return Err(invalid(
                "attachment witness response ended before its Content-Length",
            ));
        }
        used = used
            .checked_add(count)
            .ok_or_else(|| invalid("API response count overflow"))?;
        if used > bound {
            return Err(exhausted("Firecracker API response", bound));
        }
        if header_end.is_none() {
            // Scan only new bytes plus the delimiter overlap; an adversarial one-byte sender
            // cannot make header recognition quadratic in the finite response byte budget.
            let overlap = before.saturating_sub(b"\r\n\r\n".len().saturating_sub(1));
            if let Some(index) = bytes.get(overlap..used).and_then(|part| {
                part.windows(b"\r\n\r\n".len())
                    .position(|part| part == b"\r\n\r\n")
            }) {
                let end = overlap
                    .checked_add(index)
                    .and_then(|index| index.checked_add(b"\r\n\r\n".len()))
                    .ok_or_else(|| invalid("HTTP header length overflow"))?;
                let header = bytes
                    .get(..end)
                    .ok_or_else(|| invalid("HTTP header is absent"))?;
                let length = content_length(header)?;
                let total = end
                    .checked_add(length)
                    .filter(|total| *total <= bound)
                    .ok_or_else(|| exhausted("Firecracker API response", bound))?;
                if used > total {
                    return Err(invalid(
                        "attachment witness has bytes beyond its Content-Length",
                    ));
                }
                header_end = Some(end);
                response_end = Some(total);
            }
        }
    }
    let end = header_end.ok_or_else(|| invalid("malformed Firecracker HTTP response"))?;
    let body = bytes
        .get(end..used)
        .ok_or_else(|| invalid("malformed Firecracker HTTP body"))?;
    serde_json::from_slice(body).map_err(|_| invalid("invalid Firecracker attachment witness"))
}

fn content_length(header: &[u8]) -> Result<usize, Refusal> {
    let text = std::str::from_utf8(header).map_err(|_| invalid("HTTP header is not UTF-8"))?;
    let mut lines = text.split("\r\n");
    if !lines
        .next()
        .is_some_and(|line| line.starts_with("HTTP/1.1 200 "))
    {
        return Err(invalid("Firecracker refused attachment witness"));
    }
    let mut length = None;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| invalid("malformed HTTP header field"))?;
        if name.eq_ignore_ascii_case("transfer-encoding") {
            return Err(invalid("attachment witness cannot use Transfer-Encoding"));
        }
        if name.eq_ignore_ascii_case("content-length") {
            if length.is_some() {
                return Err(invalid("duplicate HTTP Content-Length"));
            }
            let value = value.trim_matches([' ', '\t']);
            if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(invalid("invalid HTTP Content-Length"));
            }
            length = Some(
                value
                    .parse::<usize>()
                    .map_err(|_| invalid("HTTP Content-Length overflow"))?,
            );
        }
    }
    length.ok_or_else(|| invalid("attachment witness needs an exact Content-Length"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hex_api::{ConnectFailure, ConnectPhase, ConnectReply, IoKind};

    /// Firecracker's host-side vsock socket (vsock.md): one connection, its `CONNECT <port>\n`
    /// read whole, then `answer` written and the connection closed. Bound before any dial.
    fn vsock(path: &Path, answer: &'static [u8]) -> std::thread::JoinHandle<Vec<u8>> {
        let listener = UnixListener::bind(path).unwrap();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut byte = [0u8; 1];
            while request.last() != Some(&b'\n') {
                assert_eq!(
                    stream.read(&mut byte).unwrap(),
                    1,
                    "request ends at its newline"
                );
                request.push(byte[0]);
            }
            stream.write_all(answer).unwrap();
            request
        })
    }

    fn dial(path: &Path, create_deadline: Option<Instant>) -> Result<(), Refusal> {
        Endpoint {
            path: path.to_owned(),
            timeout: Duration::from_secs(30),
            create_deadline,
        }
        .dial_before(1024, None)
        .map(drop)
    }

    /// AC-T7.4.1: a Firecracker CONNECT that fails reaches the caller as Hex's typed Connect
    /// refusal, its port, phase and cause kept (as the Shards adapter carries Shards' own),
    /// never as Os or Invalid text.
    #[test]
    fn a_failed_connect_keeps_its_port_phase_and_cause() {
        let dir = tempfile::tempdir().unwrap();
        let connect = |failure| Refusal::Connect {
            port: 1024,
            failure,
        };

        let absent = dir.path().join("absent.sock");
        assert_eq!(
            dial(&absent, None),
            Err(connect(ConnectFailure::Io {
                phase: ConnectPhase::Dial,
                kind: IoKind::new(std::io::ErrorKind::NotFound),
                code: Some(libc::ENOENT),
            }))
        );

        let spent = Some(Instant::now());
        assert_eq!(
            dial(&absent, spent),
            Err(connect(ConnectFailure::Expired {
                phase: ConnectPhase::Dial
            }))
        );

        for (name, answer, failure) in [
            ("closed.sock", b"".as_slice(), ConnectFailure::Closed),
            (
                "refused.sock",
                b"NO 1\n".as_slice(),
                ConnectFailure::Reply(ConnectReply::NotOk),
            ),
            (
                "zero.sock",
                b"OK 0\n".as_slice(),
                ConnectFailure::Reply(ConnectReply::NotOk),
            ),
            (
                "long.sock",
                b"OK 12345678901\n".as_slice(),
                ConnectFailure::Reply(ConnectReply::TooLong),
            ),
        ] {
            let path = dir.path().join(name);
            let peer = vsock(&path, answer);
            assert_eq!(dial(&path, None), Err(connect(failure)), "{name}");
            assert_eq!(peer.join().unwrap(), b"CONNECT 1024\n");
        }

        let path = dir.path().join("ok.sock");
        let peer = vsock(&path, b"OK 1073741824\n");
        assert_eq!(dial(&path, None), Ok(()));
        assert_eq!(peer.join().unwrap(), b"CONNECT 1024\n");
    }

    /// AC-T7.4.1: an answer withheld past the deadline is Expired in Read, the socket's timed-out
    /// read never reported as an I/O failure. Only the read step runs, so no other phase can expire.
    #[test]
    fn a_withheld_answer_expires_in_its_read_phase() {
        let (mut ours, _withheld) = UnixStream::pair().unwrap();
        let until = Instant::now() + Duration::from_millis(20);
        let step = read_exact(&mut ours, &mut [0u8; 1], until).unwrap_err();
        assert_eq!(
            step.connect(1024, ConnectPhase::Read),
            Refusal::Connect {
                port: 1024,
                failure: ConnectFailure::Expired {
                    phase: ConnectPhase::Read
                },
            }
        );
    }

    /// A real connection whose next `reads` reads and `writes` writes fail Interrupted without
    /// touching it, as a signal makes a socket call with a timeout set fail (signal(7): never
    /// restarted, whatever SA_RESTART says), and that is otherwise itself.
    struct Interrupting {
        stream: UnixStream,
        reads: u32,
        writes: u32,
    }

    impl Read for Interrupting {
        fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
            if self.reads > 0 {
                self.reads -= 1;
                return Err(std::io::ErrorKind::Interrupted.into());
            }
            self.stream.read(bytes)
        }
    }

    impl Write for Interrupting {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if self.writes > 0 {
                self.writes -= 1;
                return Err(std::io::ErrorKind::Interrupted.into());
            }
            self.stream.write(bytes)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.stream.flush()
        }
    }

    impl Duplex for Interrupting {
        fn try_clone(&self) -> std::io::Result<DuplexOwner> {
            Duplex::try_clone(&self.stream)
        }
        fn shutdown(&self, how: std::net::Shutdown) -> std::io::Result<()> {
            self.stream.shutdown(how)
        }
        fn set_write_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()> {
            self.stream.set_write_timeout(timeout)
        }
        fn set_read_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()> {
            self.stream.set_read_timeout(timeout)
        }
        fn handle(&self) -> hex_client::guest::Handle<'_> {
            self.stream.as_fd()
        }
        fn set_nonblocking(&self, on: bool) -> std::io::Result<()> {
            self.stream.set_nonblocking(on)
        }
    }

    /// The vsock handshake against a peer that answers `OK`, its stream interrupted as given.
    fn interrupted_handshake(reads: u32, writes: u32) {
        let (ours, mut peer) = UnixStream::pair().unwrap();
        let answered = std::thread::spawn(move || {
            let mut request = Vec::new();
            let mut byte = [0u8; 1];
            while request.last() != Some(&b'\n') {
                assert_eq!(
                    peer.read(&mut byte).unwrap(),
                    1,
                    "request ends at its newline"
                );
                request.push(byte[0]);
            }
            peer.write_all(b"OK 1073741824\n").unwrap();
            request
        });
        let mut stream = Interrupting {
            stream: ours,
            reads,
            writes,
        };
        let until = Instant::now() + Duration::from_secs(30);
        assert_eq!(handshake(&mut stream, 1024, until), Ok(()));
        assert_eq!(answered.join().unwrap(), b"CONNECT 1024\n");
        assert_eq!((stream.reads, stream.writes), (0, 0));
    }

    /// AC-T7.4.1: a handshake write a signal interrupts is made again within the same absolute
    /// deadline, as Shards' handshake does; the dial never fails as Io(Interrupted).
    #[test]
    fn an_interrupted_handshake_write_is_made_again() {
        interrupted_handshake(0, 1);
    }

    /// AC-T7.4.1: a handshake read a signal interrupts is made again within the same absolute
    /// deadline, as Shards' handshake does; the dial never fails as Io(Interrupted).
    #[test]
    fn an_interrupted_handshake_read_is_made_again() {
        interrupted_handshake(1, 0);
    }

    /// AC-T7.4.1: the attachment witness's response read a signal interrupts is made again within
    /// the same absolute deadline, never failed as an OS error.
    #[test]
    fn an_interrupted_witness_read_is_made_again() {
        let (ours, mut peer) = UnixStream::pair().unwrap();
        let body = br#"{"id":"pci_100","sbdf":"0000:01:00.0","guest_sbdf":"0000:00:05.0"}"#;
        let mut response =
            format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len()).into_bytes();
        response.extend_from_slice(body);
        peer.write_all(&response).unwrap();
        let mut stream = Interrupting {
            stream: ours,
            reads: 1,
            writes: 0,
        };
        let until = Instant::now() + Duration::from_secs(30);
        let witnessed = witness(&mut stream, until).unwrap();
        assert_eq!(
            (
                witnessed.id.as_str(),
                witnessed.sbdf.as_str(),
                witnessed.guest_sbdf.as_str()
            ),
            ("pci_100", "0000:01:00.0", "0000:00:05.0")
        );
        assert_eq!(stream.reads, 0);
    }

    /// AC-T7.4.1: framing is finite on persistent HTTP connections and rejects ambiguous lengths.
    #[test]
    fn witness_response_requires_one_bounded_content_length() {
        assert_eq!(
            content_length(
                b"HTTP/1.1 200 OK\r\nContent-Length: 17\r\nConnection: keep-alive\r\n\r\n"
            )
            .unwrap(),
            17
        );
        for header in [
            b"HTTP/1.1 200 OK\r\n\r\n".as_slice(),
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\ncontent-length: 1\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nTransfer-Encoding: chunked\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nContent-Length: +1\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nContent-Length: 184467440737095516150\r\n\r\n",
        ] {
            assert!(content_length(header).is_err());
        }
    }

    /// AC-T7.4.1: a create refused for a VMM that ended before readiness, its output past what one wire answer holds,
    /// whose cleanup failed too. Do: keep both as one `Uncertain` refusal. Expect it within one refused answer under
    /// the wire bound, the readiness refusal first with the end of the output, then the cleanup's refusal whole; and a
    /// refusal of any other kind kept as it was.
    #[test]
    fn an_uncertain_refusal_keeps_both_causes_within_one_wire_answer() {
        let output = format!(
            "{}the last words\n",
            "x".repeat(hex_core::wire::MAX_MESSAGE * 2)
        );
        let refusal = readiness(&output, usize::MAX);
        let cleanup = Refusal::Os {
            op: "attaching the box's native scope",
            why: "ESRCH".to_owned(),
        };
        let whole = uncertain_after(BoxId::new("unclean").unwrap(), refusal, cleanup.clone());
        let bytes = hex_core::wire::snapshot_refusal(&whole, hex_core::wire::MAX_MESSAGE).unwrap();
        assert!(bytes.len() <= hex_core::wire::MAX_MESSAGE);
        let Refusal::Uncertain {
            id,
            refusal,
            cleanup: kept,
        } = &whole
        else {
            panic!("{whole:?}");
        };
        assert_eq!(id.as_str(), "unclean");
        let Refusal::Broken(why) = &**refusal else {
            panic!("{refusal:?}");
        };
        assert!(why.starts_with("Firecracker ended before guest readiness; its last output:\nxxx"));
        assert!(why.ends_with("xthe last words\n"));
        assert_eq!(**kept, cleanup);
        let other = Refusal::Os {
            op: "creating private VM directory",
            why: "ENOENT".to_owned(),
        };
        let whole = uncertain_after(
            BoxId::new("unclean").unwrap(),
            other.clone(),
            cleanup.clone(),
        );
        assert_eq!(
            whole,
            Refusal::Uncertain {
                id: BoxId::new("unclean").unwrap(),
                refusal: Box::new(other),
                cleanup: Box::new(cleanup),
            }
        );
    }
}
