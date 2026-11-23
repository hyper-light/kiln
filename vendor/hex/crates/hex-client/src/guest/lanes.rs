//! The guest's ports through its run's agent-control endpoint (docs/design/node-under-shards-init.md §5): a node VM
//! under shards-init is reached by Shards' socket lanes (D251) to its relay's listeners in the run's socket directory,
//! [`DIR`], over the run's own endpoint (D252), whose controller is Shards' own (`shards_api::control`). [`Lanes`]
//! dials a port as a lane to the socket that port names ([`hex_core::stream::socket_name`]), bridged to a local
//! socket pair ([`super::bridge`]), so the client's event loop waits on an OS handle as it does on vsock's. The lane
//! carries a half-close each way, and its reset or its connection's end is told after its bytes. On Windows an exec's
//! stream is a lane carried on a pipe instead (`Lanes::piped`): the node hands a caller in another process handles
//! alone, and its run's endpoint lends no caller the lanes themselves (the review's D-10).

use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use hex_api::{ConnectFailure, ConnectPhase, IoKind, Refusal};
use hex_core::lanes::DATA_BOUND;
use hex_core::stream;
use shards_api::control::{
    Controller, Dialed, Fault, Hangup, Identity, Reason, Socket, SocketAdmission, SocketReader,
    SocketWriter, Sockets, Subscriptions,
};

#[cfg(windows)]
use super::Piped;
use super::bridge::{Bridged, Local, end, pair};
use super::{DuplexOwner, Link, own_duplex};
use crate::Cut;

/// The run's socket directory: where the node's relay listens under shards-init (hex-cli's guest init), and the one
/// directory the run grants its controller lanes to.
pub const DIR: &str = "/run/hex/channel";

/// The descriptors a lane holds on this node's side while it lives, as [`Link::dial`] hands it over ([`bridge`]): its
/// local socket pair's two ends, the owner's and the copiers', which its two copiers share, as a control connection's
/// handles, the client `Stream`'s reading and ending ones, share the owner's: no duplicate, which Windows makes
/// inheritable (the review's D-5). `hex-node`'s descriptor fit counts each lane by it, a control connection's too
/// (`serve::fit::hosted_descriptors`); the tests count the real ones (the review's D-6).
pub const FDS_A_LANE: u64 = 2;

/// The controller's connection to a node VM's endpoint and the lanes it opens on it to the node's relay.
pub struct Lanes {
    sockets: Sockets,
    /// The connection: kept for the lanes' life, which end with it (dropped, it ends the connection and every lane).
    _hangup: Hangup,
    /// The most lanes open at once, as the run admits them: what a dial past it is refused naming.
    lanes: u32,
}

impl Lanes {
    /// Dials the endpoint at `at` of the node VM whose key's SHA-256 is `vm`, under this node's own key `own`, for
    /// socket lanes alone as `admission` admits them (its run's grant, D252), its controller's driver on a thread of
    /// its own; then waits for both ends' word on the connection's TLS ([`tls`]). That wait follows the connection:
    /// its handshake's end, the endpoint's refusal or the connection's end answers it (decision 9).
    pub fn dial(
        own: &Identity,
        vm: [u8; 32],
        at: SocketAddr,
        admission: SocketAdmission,
    ) -> Result<Lanes, Refusal> {
        let (controller, dialed) = Controller::dial(
            own,
            vm,
            at,
            Subscriptions {
                outputs: &[],
                inputs: &[],
                execs: None,
                sockets: Some(admission),
            },
        )
        .map_err(|e| Refusal::Os {
            op: DIALING_VM,
            why: e.to_string(),
        })?;
        let Dialed {
            sockets,
            mut link,
            hangup,
            ..
        } = dialed;
        std::thread::Builder::new()
            .name("hex-lanes-driver".to_owned())
            .spawn(move || {
                // Its end is the connection's, which every lane on it then reads as its fault.
                let _ = controller.run();
            })
            .map_err(|e| Refusal::os("starting the node VM's controller", e))?;
        let negotiated = link.wait().map_err(|fault| Refusal::Os {
            op: DIALING_VM,
            why: format!("its endpoint's connection: {fault:?}"),
        })?;
        tls(negotiated)?;
        let sockets = sockets.ok_or_else(|| {
            Refusal::Broken("a controller dialed for socket lanes was given none".to_owned())
        })?;
        Ok(Lanes {
            sockets,
            _hangup: hangup,
            lanes: admission.lanes,
        })
    }
}

impl Link for Lanes {
    fn dial(&self, port: u32) -> Result<DuplexOwner, Refusal> {
        self.dial_before(port, None)
    }

    /// A lane to the socket `port` names ([`Lanes::connect`]), bridged to a local socket.
    fn dial_before(&self, port: u32, deadline: Option<Instant>) -> Result<DuplexOwner, Refusal> {
        own_duplex(self.bridged(port, deadline)?)
    }

    /// A bridged lane is handed to its caller as the owner's end of its socket pair (the review's D-5); on Windows an
    /// exec's stream goes on a pipe of its own first ([`Link::dial_piped`]).
    fn dial_handoff_before(
        &self,
        port: u32,
        deadline: Option<Instant>,
    ) -> Option<Result<super::Handoff, Refusal>> {
        Some(self.bridged(port, deadline).map(super::Handoff::Bridged))
    }

    fn carries_half_close(&self) -> bool {
        true
    }

    /// Its run's endpoint takes this node's key alone (D252): no caller in another process can open its lanes.
    fn lends_identity(&self) -> bool {
        false
    }

    /// An exec's stream on a lane of its own carried on a pipe ([`Lanes::piped`]).
    #[cfg(windows)]
    fn dial_piped(
        &self,
        named: stream::Named,
        deadline: Option<Instant>,
    ) -> Option<Result<Piped, Refusal>> {
        Some(self.piped(named, deadline))
    }
}

impl Lanes {
    /// A lane to the socket `port` names ([`Lanes::connect`]), bridged to a local socket.
    fn bridged(
        &self,
        port: u32,
        deadline: Option<Instant>,
    ) -> Result<super::bridge::Bridged, Refusal> {
        let socket = self.connect(port, deadline)?;
        bridge(socket).map_err(|e| Refusal::os("bridging a lane to a local socket", e))
    }

    /// A lane to the socket `port` names. Its answer (connected, the endpoint's refusal, its run host's errno) is
    /// waited for by the connection's life, and within `deadline` where the caller gives one: past it the dial is
    /// refused, expired, and a lane that connects after it is let go, aborted.
    fn connect(&self, port: u32, deadline: Option<Instant>) -> Result<Socket, Refusal> {
        let name = named(port)?;
        let patience = match deadline {
            None => Duration::MAX,
            Some(at) => at
                .checked_duration_since(Instant::now())
                .filter(|left| !left.is_zero())
                .ok_or_else(|| expired(port))?,
        };
        // Asked once; past the deadline the ask is let go, and a lane that connects after is closed as it opens.
        let answer = self
            .sockets
            .connect(name)
            .and_then(|mut asked| asked.wait(patience));
        let socket = match answer {
            Ok(Some(socket)) => socket,
            Ok(None) if deadline.is_some() => return Err(expired(port)),
            Ok(None) => {
                return Err(Refusal::Broken(
                    "a lane's dial with no deadline returned unanswered".to_owned(),
                ));
            }
            Err(fault) => return Err(refused(port, fault, self.lanes)),
        };
        Ok(socket)
    }

    /// An exec's stream on a lane of its own, carried on a pipe whose end is the caller's (Windows, HEX-RT-17: the node
    /// hands a caller in another process handles alone, never sockets), its name written on the lane first. An output
    /// stream's lane ends its own half once named, and a thread copies the lane into the pipe ([`piped_out`]); stdin's
    /// copies the pipe into the lane ([`piped_in`]).
    ///
    /// One thread a stream, for want of an evented lane: Shards' socket lanes block per lane, each `SocketReader` and
    /// `SocketWriter` waiting on a channel of its own, with no try-read and no waker, so each stream's bridge holds a
    /// thread until Shards gives an evented lane API; then one loop per node VM drives every lane and its pipes. It is
    /// half what each lane's bridge to a socket pair holds ([`bridge`]'s two), with no loopback TCP pair.
    #[cfg(windows)]
    fn piped(&self, named: stream::Named, deadline: Option<Instant>) -> Result<Piped, Refusal> {
        let (reader, mut writer) = self.connect(stream::STDIO_PORT, deadline)?.split();
        writer
            .write_all(&stream::name(named))
            .map_err(|e| Refusal::os("naming an exec's stream on its lane", e))?;
        let (from, to) =
            std::io::pipe().map_err(|e| Refusal::os("making an exec's stream's pipe", e))?;
        match named.stream {
            stream::Stdio::In => {
                std::thread::Builder::new()
                    .name("hex-lane-stdin".to_owned())
                    .spawn(move || piped_in(from, writer, reader))
                    .map_err(|e| Refusal::os("starting a stdin lane's copier", e))?;
                Ok(Piped {
                    end: to.into(),
                    cut: None,
                })
            }
            stream::Stdio::Out | stream::Stdio::Err => {
                // It carries nothing this way once named: its peer's End then closes the stream whole.
                writer.shutdown_write().map_err(|fault| Refusal::Os {
                    op: "ending an output stream's own half",
                    why: fault.to_string(),
                })?;
                let cut = Cut::default();
                let recorded = cut.clone();
                std::thread::Builder::new()
                    .name("hex-lane-output".to_owned())
                    .spawn(move || piped_out(reader, to, &recorded))
                    .map_err(|e| Refusal::os("starting an output lane's copier", e))?;
                Ok(Piped {
                    end: from.into(),
                    cut: Some(cut),
                })
            }
        }
    }
}

/// Stdin's lane: what the caller writes to its pipe, into the lane, its pipe's end the lane's FIN. A write the lane
/// refuses (its reset: the program let its stdin go) lets the pipe go, so the caller's next write fails, as a closed
/// stdin's does. The lane's reading half, which carries nothing this way, is held to the lane's end, so it never stops
/// the relay's socket before the relay has read what came (STOP_SENDING).
#[cfg(windows)]
fn piped_in(mut from: std::io::PipeReader, mut writer: SocketWriter, mut reader: SocketReader) {
    let mut buf = vec![0u8; DATA_BOUND];
    loop {
        match from.read(&mut buf) {
            Ok(0) => {
                if writer.shutdown_write().is_err() {
                    return;
                }
                break;
            }
            Ok(n) => {
                if writer.write_all(buf.get(..n).unwrap_or_default()).is_err() {
                    return;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => {
                let _ = writer.abort();
                return;
            }
        }
    }
    drop(from);
    while matches!(reader.read(&mut buf), Ok(n) if n > 0) {}
}

/// An output stream's lane: its bytes into the caller's pipe, the lane's end in order the pipe's, and its reset or its
/// connection's end the cut, recorded before the pipe ends. A caller gone from its pipe lets the lane's reading go,
/// which stops the relay's sending (STOP_SENDING), as a program's write fails once its stdout's pipe has no reader.
#[cfg(windows)]
fn piped_out(mut reader: SocketReader, mut to: std::io::PipeWriter, cut: &Cut) {
    let mut buf = vec![0u8; DATA_BOUND];
    loop {
        match reader.read(&mut buf) {
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

const DIALING_VM: &str = "dialing the node VM's run's endpoint";

/// The socket `port` names in the run's socket directory: none is the caller's error, before any lane.
fn named(port: u32) -> Result<&'static str, Refusal> {
    stream::socket_name(port).ok_or_else(|| Refusal::Invalid {
        what: "a port of the node VM's relay",
        why: format!("{port:#x} names no socket the relay listens on"),
    })
}

fn expired(port: u32) -> Refusal {
    Refusal::Connect {
        port,
        failure: ConnectFailure::Expired {
            phase: ConnectPhase::Dial,
        },
    }
}

/// Linux's errno values the run host's connect reports for a lane (D251): the guest is Linux whatever the host, so its
/// numbers are Linux's (`include/uapi/asm-generic/errno-base.h`, `errno.h`), not this host's.
const ENOENT: u16 = 2;
const EACCES: u16 = 13;
const ECONNREFUSED: u16 = 111;

/// A lane's fault as the dial's refusal to `port`, typed: its run host's connect's errno as a connect failure at the
/// dial; the run's admission passed as Hex's own exhaustion, naming it; the connection's end, or the lane's reset
/// before it connected, as the port closed unanswered.
fn refused(port: u32, fault: Fault, lanes: u32) -> Refusal {
    match fault {
        Fault::Unreached(errno) => Refusal::Connect {
            port,
            failure: ConnectFailure::Io {
                phase: ConnectPhase::Dial,
                kind: IoKind::new(match errno.get() {
                    ENOENT => std::io::ErrorKind::NotFound,
                    EACCES => std::io::ErrorKind::PermissionDenied,
                    ECONNREFUSED => std::io::ErrorKind::ConnectionRefused,
                    _ => std::io::ErrorKind::Other,
                }),
                code: Some(i32::from(errno.get())),
            },
        },
        Fault::Refused(Reason::Capacity) => Refusal::Exhausted {
            what: "socket lanes to the node VM's relay",
            bound: usize::try_from(lanes).unwrap_or(usize::MAX),
        },
        Fault::Lost(_) | Fault::Revoked => Refusal::Connect {
            port,
            failure: ConnectFailure::Closed,
        },
        other => Refusal::Os {
            op: "dialing the node VM's relay through its run's endpoint",
            why: format!("a lane to port {port:#x}: {other:?}"),
        },
    }
}

/// Both ends of a node's control link took TLS13_AES_256_GCM_SHA384, the person's directive for every link of Hex's
/// and Shards' D239; a link either end of which took another suite is refused, naming it.
fn tls(link: shards_api::control::Link) -> Result<(), Refusal> {
    const SUITE: &str = "TLS13_AES_256_GCM_SHA384";
    for (end, negotiated) in [
        ("this node's end", link.controller),
        ("the node VM's endpoint", link.endpoint),
    ] {
        let suite = negotiated.suite_name();
        if suite != Some(SUITE) {
            return Err(Refusal::Invalid {
                what: "the node VM's control link",
                why: format!(
                    "{end} took {}, not {SUITE}",
                    suite.unwrap_or("a suite rustls doesn't name")
                ),
            });
        }
    }
    Ok(())
}

/// `socket` as the owner's end of a local socket pair, its bytes the lane's: two threads copy between the pair's other
/// end and the lane, one a way, each on one of the lane's halves ([`Socket::split`]), sharing the pair's end, one
/// reading it and one writing it: no duplicate, which Windows makes inheritable (the review's D-5). Each ends on a
/// fact: the owner's end of writing or its close, the lane's end in order, or its reset or its connection's end, which
/// the owner reads as the cut after the bytes before it. A relay's socket closed while the owner sends ends the lane's
/// reading in order, after every byte the relay wrote, and refuses the owner's writes (Shards' D251 as amended).
fn bridge(socket: Socket) -> std::io::Result<Bridged> {
    let (owner, ours) = pair()?;
    let (reader, writer) = socket.split();
    let (cut, refused) = (Cut::default(), Cut::default());
    let ours = Arc::new(ours);
    let (up, told) = (Arc::clone(&ours), refused.clone());
    std::thread::Builder::new()
        .name("hex-lane-up".to_owned())
        .spawn(move || upward(writer, &up, &told))?;
    let recorded = cut.clone();
    std::thread::Builder::new()
        .name("hex-lane-down".to_owned())
        .spawn(move || downward(reader, &ours, &recorded))?;
    Ok(Bridged::refusing(owner, cut, refused))
}

/// What the owner writes, to the lane; the owner's end of writing, or its close, is the lane's FIN: the relay's socket
/// shut for writing, its reads going on. A write the lane refuses (its relay's close, its reset, its connection's end)
/// ends this way, recorded in `refused`, and the owner's writes then fail ([`writes_refused`]). An error reading the
/// pair aborts the lane.
fn upward(mut writer: SocketWriter, mut from: &Local, refused: &Cut) {
    let mut buf = vec![0u8; DATA_BOUND];
    loop {
        #[cfg(target_os = "macos")]
        if crate::wait_readable(std::os::fd::AsFd::as_fd(from)).is_err() {
            let _ = writer.abort();
            return;
        }
        match from.read(&mut buf) {
            Ok(0) => {
                let _ = writer.shutdown_write();
                return;
            }
            Ok(n) => {
                if let Err(e) = writer.write_all(buf.get(..n).unwrap_or_default()) {
                    refused.record(&e);
                    writes_refused(from, &mut buf);
                    return;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => {
                let _ = writer.abort();
                return;
            }
        }
    }
}

/// The owner's writes once the lane refuses them, what the owner sent before read and let go, so the pair's other end
/// never closes with bytes unread: Linux tells such a close of a socket pair's end to its peer as a reset
/// (`unix_release_sock`'s `ECONNRESET`), as Winsock does a loopback TCP pair's (its RST), which the owner would read
/// after the bytes that came before it in place of the lane's end in order (Shards' D251 as amended). On Unix the
/// pair's other end stops reading first, so the owner's writes fail (Linux's at once, macOS's once the pair's end
/// closes) and the bytes left are those that came before; its reading shut, no read here waits, so macOS's sleep
/// through a half-close (Shards D175) can't hold it. On Windows its receiving stays open, as Winsock resets a TCP pair
/// whose receiving is shut while bytes wait or come (the review's D-4): what the owner sends is let go to its end of
/// writing, its close or its reset, and its writes fail at [`Bridged`]'s write.
fn writes_refused(mut from: &Local, buf: &mut [u8]) {
    #[cfg(unix)]
    let _ = from.shutdown(Shutdown::Read);
    loop {
        match from.read(buf) {
            Ok(0) => return,
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return,
        }
    }
}

/// What the relay sends, to the owner; the lane's end in order is the owner's end of input, and its reset or its
/// connection's end the cut, recorded before the owner's socket ends. An owner gone lets the lane's reading go, which
/// stops the relay's sending (STOP_SENDING).
fn downward(mut reader: SocketReader, mut to: &Local, cut: &Cut) {
    let mut buf = vec![0u8; DATA_BOUND];
    loop {
        match reader.read(&mut buf) {
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

#[cfg(test)]
mod tests;
