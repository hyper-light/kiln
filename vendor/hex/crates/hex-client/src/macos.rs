//! The client on macOS (native plan step 8b): one AF_UNIX `SOCK_STREAM` connection to the node agent's
//! socket, as macOS has no `SOCK_SEQPACKET` local socket, each message framed ([`hex_core::stream`]), an exec's stdio
//! as `SCM_RIGHTS` descriptors at its message's first byte, on the core every transport shares (`crate::calls`): no
//! thread of its own reads it, each message is written whole under the connection's send lock, and a call waits for
//! its answer until its caller's deadline, or with none at most the slowest answer this process has had to a call of
//! its kind, following the node agent before its first answer and after a run-out ([`ANSWERS`]; decision 9).

use std::io::{IoSlice, IoSliceMut};
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, OwnedFd};
use std::path::Path;

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

/// How long a call waits for the macOS node agent's answer, and its send for room in the agent's socket, one wait a kind
/// of call (`crate::calls::kind_of`'s order), since a list waits on its parts, so a fast hello's answer says nothing of
/// its: the slowest answer this process has had to a call of that kind (decision 9), past which a call's run-out is
/// recorded and its wait goes on, refusing nothing by itself (the person's ruling of 2026-11-13). A call's wait follows
/// the node agent: it lasts while the connection stands, its reset or close the refusal, or to its caller's deadline. No record is read: the `client.answer.<kind>` rows of `bench/sched/runtime-pools/calibration-macos-cold-extension-1bb73f74.json` are the
/// evidence the rule is held to. The kinds a box's class times are [`CLASS_ANSWERS`].
static ANSWERS: [Patience; KINDS] = [
    Patience::measuring("client.answer.hello"),
    Patience::measuring("client.answer.list"),
    Patience::measuring("client.answer.netns"),
    Patience::measuring("client.answer.schedule.describe"),
    Patience::measuring("client.answer.schedule.advertise"),
    Patience::measuring("client.answer.schedule.submit"),
    Patience::measuring("client.answer.schedule.hear_create"),
    Patience::measuring("client.answer.schedule.hear_delete"),
    Patience::measuring("client.answer.schedule.hear_control"),
    Patience::measuring("client.answer.schedule.link_up"),
    Patience::measuring("client.answer.schedule.retry"),
    Patience::measuring("client.answer.schedule.delete"),
    Patience::measuring("client.answer.schedule.status"),
    Patience::measuring("client.answer.physical_report"),
    Patience::measuring("client.answer.initialize_backing"),
    Patience::measuring("client.answer.release_backing"),
    Patience::measuring("client.answer.backing_usage"),
    // A capacity holder's release (joint plan step 13, CV6), learned as every kind is.
    Patience::measuring("client.answer.schedule.forget"),
    // MEASURING until a waits census records the node's links (docs/design/hex-quic.md Q12).
    Patience::measuring("client.answer.links"),
];

/// AC-T7.4.1: the kinds a box's class times, a wait for each (kind, class), named `client.answer.<kind>.<class>`,
/// each learned as [`ANSWERS`] are.
static CLASS_ANSWERS: [[Patience; BOX_KINDS]; CLASSES] = [
    class_waits!("client.answer", "process", measuring),
    class_waits!("client.answer", "oci", measuring),
    class_waits!("client.answer", "firecracker", measuring),
    class_waits!(
        "client.answer",
        "shards",
        [
            measuring, measuring, measuring, measuring, measuring, measuring, measuring, measuring,
            measuring, measuring, measuring,
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
