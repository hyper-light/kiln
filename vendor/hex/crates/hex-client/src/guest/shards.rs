//! The guest's ports through Shards (docs/design/shards-guest.md §2, Appendix A "Hex's side"): a
//! [`Link`] whose first dial is `shards_api::Guest::connect_before` and every later one
//! `connect_once_before`, a dial past its W* `connect_while_runs`, Firecracker's `CONNECT <port>` on the
//! guest's vsock, and a [`Duplex`] over the stream it returns. Hex never names Shards' stream type: on
//! Windows it is Shards' overlapped AF_UNIX socket, whose timeouts poll first, behind the same trait.

use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use hex_api::{
    ConnectFailure, ConnectPhase, ConnectReply, GuestEnd, GuestFailure, IoKind, Refusal,
};
use hex_core::patience::{Patience, Piece};

use super::{Duplex, DuplexOwner, Handoff, Link, own_duplex};

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
        own_duplex(self.stream_before(port, deadline)?)
    }

    /// A Shards stream is handed to its caller as its own socket (the review's D-5).
    fn dial_handoff_before(
        &self,
        port: u32,
        deadline: Option<Instant>,
    ) -> Option<Result<Handoff, Refusal>> {
        Some(self.stream_before(port, deadline).map(Handoff::Shards))
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
