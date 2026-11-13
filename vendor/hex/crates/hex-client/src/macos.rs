//! The client on macOS (native plan step 8b): one AF_UNIX `SOCK_STREAM` connection to the node agent's
//! socket, as macOS has no `SOCK_SEQPACKET` local socket, each message framed ([`hex_core::stream`]), an exec's stdio
//! as `SCM_RIGHTS` descriptors at its message's first byte, on the core every transport shares (`crate::calls`): no
//! thread of its own reads it, each message is written whole under the connection's send lock, and a call waits for
//! its answer until its caller's deadline, or with none at most twice the slowest answer to a call of its kind
//! ([`ANSWERS`]).

use std::io::{IoSlice, IoSliceMut};
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, OwnedFd};
use std::path::Path;
use std::time::Duration;

use hex_api::{Class, Refusal};
use hex_core::patience::Patience;
use hex_core::stream::{self, Deframer};
use hex_core::wire::{self, Received};
use rustix::io::{Errno, FdFlags};
use rustix::net::{
    AddressFamily, RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, SendAncillaryBuffer,
    SendAncillaryMessage, SendFlags, Shutdown, SocketAddrUnix, SocketType,
};

use crate::calls::{
    BOX_KINDS, CLASSES, Core, Handle, KINDS, Lent, Sent, Shared, Take, Transport, by_class,
    class_waits,
};

/// How long a call waits for the macOS node agent's answer, and its send for room in the agent's socket, one wait a
/// kind of call (`crate::calls::kind_of`'s order), as Linux's client waits (decision 9): twice the slowest answer this
/// process has had to a call of that kind. Hello, scheduler operations and capacity use the exact per-profile
/// response maxima and censored run-out lower bounds of the root-verified macOS/SSH guest census (AC-T7.4.1):
/// `bench/sched/runtime-pools/calibration-macos-cold-extension-1bb73f74.json`. These time wire replies, not workload success.
/// A debug build's seed is never under the release record (`Patience::by_profile`), which six scheduler rows'
/// debug campaigns fall under. Other operations remain measuring until their own actual macOS rows are recorded. The
/// kinds a box's class times are [`CLASS_ANSWERS`].
static ANSWERS: [Patience; KINDS] = [
    Patience::by_profile(
        "client.answer.hello",
        Duration::from_nanos(829_500),
        Some(Duration::from_nanos(2_607_584)),
    ),
    Patience::measuring("client.answer.list"),
    Patience::measuring("client.answer.netns"),
    Patience::by_profile(
        "client.answer.schedule.describe",
        Duration::from_nanos(23_338_333),
        Some(Duration::from_nanos(22_181_708)),
    ),
    Patience::by_profile(
        "client.answer.schedule.advertise",
        Duration::from_nanos(88_998_417),
        Some(Duration::from_nanos(108_619_750)),
    ),
    Patience::by_profile(
        "client.answer.schedule.submit",
        Duration::from_nanos(228_296_000),
        Some(Duration::from_nanos(401_317_333)),
    ),
    Patience::by_profile(
        "client.answer.schedule.hear_create",
        Duration::from_nanos(244_933_375),
        Some(Duration::from_nanos(392_913_792)),
    ),
    Patience::by_profile(
        "client.answer.schedule.hear_delete",
        Duration::from_nanos(152_498_583),
        Some(Duration::from_nanos(130_079_458)),
    ),
    Patience::by_profile(
        "client.answer.schedule.hear_control",
        Duration::from_nanos(122_932_458),
        Some(Duration::from_nanos(119_427_250)),
    ),
    Patience::by_profile(
        "client.answer.schedule.link_up",
        Duration::from_nanos(144_625),
        Some(Duration::from_nanos(250_209)),
    ),
    Patience::by_profile(
        "client.answer.schedule.retry",
        Duration::from_nanos(18_684_542),
        Some(Duration::from_nanos(16_694_750)),
    ),
    Patience::by_profile(
        "client.answer.schedule.delete",
        Duration::from_nanos(166_381_084),
        Some(Duration::from_nanos(125_323_459)),
    ),
    Patience::by_profile(
        "client.answer.schedule.status",
        Duration::from_nanos(7_577_917),
        Some(Duration::from_nanos(7_264_708)),
    ),
    Patience::measuring("client.answer.physical_report"),
    Patience::measuring("client.answer.initialize_backing"),
    Patience::measuring("client.answer.release_backing"),
    Patience::measuring("client.answer.backing_usage"),
    // MEASURING: the waits census's next run records a capacity holder's release (joint plan step 13, CV6).
    Patience::measuring("client.answer.schedule.forget"),
];

/// AC-T7.4.1: the kinds a box's class times, a wait for each (kind, class), named `client.answer.<kind>.<class>`. The
/// census above ran in its guest mode, its scheduler's backend of the `shards` class, so its capacity row is a `shards`
/// box's: release 13.172125 ms, debug 14.254417 ms. No census has timed another kind or class here: MEASURING, each,
/// never another class's record, until a census records it.
static CLASS_ANSWERS: [[Patience; BOX_KINDS]; CLASSES] = [
    class_waits!("client.answer", "process", measuring),
    class_waits!("client.answer", "oci", measuring),
    class_waits!("client.answer", "firecracker", measuring),
    class_waits!(
        "client.answer",
        "shards",
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
                Duration::from_nanos(13_172_125),
                Some(Duration::from_nanos(14_254_417))
            ),
        ]
    ),
];

/// A node agent, reached through its socket.
pub type Client = Core<Socket>;

/// The node agent's socket, as a client's transport.
pub struct Socket {
    fd: OwnedFd,
}

/// What the turn's holder reads into, what of a frame has come, and the descriptors that came before the message they
/// ride with is whole.
pub struct Reading {
    buf: Vec<u8>,
    deframer: Deframer,
    /// Descriptors that came, at the first byte of the message they ride with (XNU stops a stream read before a record
    /// that carries them): a given answer's, kept for its exec; any other answer is refused for them.
    fds: Vec<OwnedFd>,
    /// An agent that closed the connection with a request of ours unread leaves a reset, reported once before what it
    /// sent, which is still there to read.
    reset: Option<Refusal>,
}

impl Transport for Socket {
    type Reading = Reading;

    fn answers() -> &'static [Patience; KINDS] {
        &ANSWERS
    }

    fn class_answers(class: Class) -> &'static [Patience; BOX_KINDS] {
        by_class(&CLASS_ANSWERS, class)
    }

    fn reading(&self) -> Reading {
        Reading {
            buf: vec![0u8; stream::MAX_FRAME],
            deframer: Deframer::new(wire::MAX_MESSAGE),
            fds: Vec::new(),
            reset: None,
        }
    }

    fn handle(&self) -> Handle<'_> {
        self.fd.as_fd()
    }

    /// What one read takes, each whole message in it handed on with the descriptors that came before it; a frame cut
    /// across reads stays in the deframer for the next.
    fn recv(&self, reading: &mut Reading, take: &mut Take<'_>) -> Result<bool, Refusal> {
        let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(wire::MAX_FDS))];
        let mut control = RecvAncillaryBuffer::new(&mut space);
        let msg = match rustix::net::recvmsg(
            &self.fd,
            &mut [IoSliceMut::new(&mut reading.buf)],
            &mut control,
            RecvFlags::DONTWAIT,
        ) {
            Ok(msg) => msg,
            Err(Errno::INTR | Errno::AGAIN) => return Ok(false),
            Err(Errno::CONNRESET) if reading.reset.is_none() => {
                reading.reset = Some(Refusal::Broken(
                    "the node agent closed the connection with requests unread".to_owned(),
                ));
                return Ok(true);
            }
            Err(e) => {
                return Err(Refusal::Broken(format!(
                    "reading the node agent's socket: {e}"
                )));
            }
        };
        let before = reading.fds.len();
        for m in control.drain() {
            if let RecvAncillaryMessage::ScmRights(came) = m {
                reading.fds.extend(came);
            }
        }
        // macOS's recvmsg has no MSG_CMSG_CLOEXEC: each end that came is made close-on-exec at once, before anything
        // else is done with it, so no spawn of the caller's inherits it (CVE-2024-21626's class).
        for fd in reading.fds.get(before..).unwrap_or_default() {
            rustix::io::fcntl_setfd(fd, FdFlags::CLOEXEC)
                .map_err(|e| Refusal::os("making a given end close-on-exec", e))?;
        }
        if msg.bytes == 0 && reading.fds.len() == before {
            return Err(reading.reset.take().unwrap_or_else(|| {
                if reading.deframer.at_boundary() {
                    Refusal::Broken("the node agent closed the connection".to_owned())
                } else {
                    Refusal::Broken(
                        "the node agent closed the connection in the middle of a message"
                            .to_owned(),
                    )
                }
            }));
        }
        let Reading {
            buf, deframer, fds, ..
        } = reading;
        let mut input = buf.get(..msg.bytes).unwrap_or_default();
        while !input.is_empty() {
            let (took, whole) = deframer.take(input).map_err(|fault| {
                Refusal::Broken(format!("the node agent's stream can't be read: {fault}"))
            })?;
            input = input.get(took..).unwrap_or_default();
            let Some(message) = whole else { continue };
            let these = std::mem::take(fds);
            let received = Received {
                bytes: &message,
                size: message.len(),
                descriptors: these.len(),
                descriptors_cut: false,
            };
            take(&received, these)?;
        }
        Ok(true)
    }

    fn frame(message: Vec<u8>) -> Result<Vec<u8>, Refusal> {
        stream::frame(&message, wire::MAX_MESSAGE).map_err(|fault| Refusal::Invalid {
            what: "message",
            why: fault.to_string(),
        })
    }

    /// What the socket takes of the frame now: its first write with the descriptors lent, at its first byte.
    fn try_send(&self, bytes: &[u8], at: &mut usize, lent: &[Lent<'_>]) -> Result<bool, Sent> {
        let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(wire::MAX_FDS))];
        let mut control = SendAncillaryBuffer::new(&mut space);
        if *at == 0 && !lent.is_empty() && !control.push(SendAncillaryMessage::ScmRights(lent)) {
            return Err(Sent::Refused(Refusal::Invalid {
                what: "stdio",
                why: format!(
                    "{} descriptors, past the {} a message carries",
                    lent.len(),
                    wire::MAX_FDS
                ),
            }));
        }
        while *at < bytes.len() {
            let rest = bytes.get(*at..).unwrap_or_default();
            let n = if *at == 0 {
                rustix::net::sendmsg(
                    &self.fd,
                    &[IoSlice::new(rest)],
                    &mut control,
                    SendFlags::DONTWAIT,
                )
            } else {
                rustix::net::send(&self.fd, rest, SendFlags::DONTWAIT)
            };
            match n {
                Ok(n) => *at = at.saturating_add(n),
                Err(Errno::INTR) => {}
                Err(Errno::AGAIN) => return Ok(false),
                Err(Errno::PIPE | Errno::CONNRESET | Errno::NOTCONN) => return Err(Sent::Gone),
                Err(e) => return Err(Sent::Refused(Refusal::os("sending to the node agent", e))),
            }
        }
        Ok(true)
    }

    fn shutdown(&self) {
        let _ = rustix::net::shutdown(&self.fd, Shutdown::Both);
    }
}

impl Client {
    /// Connects to the node agent at `path` and learns what it runs and its bounds.
    pub fn connect(path: &Path) -> Result<Self, Refusal> {
        let addr = SocketAddrUnix::new(path).map_err(|e| Refusal::Invalid {
            what: "socket path",
            why: format!("{}: {e}", path.display()),
        })?;
        let fd = rustix::net::socket(AddressFamily::UNIX, SocketType::STREAM, None)
            .map_err(|e| Refusal::os("making a socket", e))?;
        // macOS's `socket` takes no flags: close-on-exec set here, and no SIGPIPE when the agent goes.
        rustix::io::fcntl_setfd(&fd, FdFlags::CLOEXEC)
            .map_err(|e| Refusal::os("making a socket close-on-exec", e))?;
        rustix::net::sockopt::set_socket_nosigpipe(&fd, true)
            .map_err(|e| Refusal::os("making a socket's sends signal-free", e))?;
        // A connect to a socket the agent listens on doesn't wait for it to accept; each call's send is bounded by
        // its kind's learned wait.
        rustix::net::connect(&fd, &addr).map_err(|e| Refusal::Os {
            op: "connecting to the node agent's socket",
            why: format!("{}: {e}", path.display()),
        })?;
        Core::hello(Shared::try_new(Socket { fd })?)
    }
}

#[cfg(test)]
mod tests;
