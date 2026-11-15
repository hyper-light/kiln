//! The node agent's wire over a byte stream (docs/design/shards-guest.md §3): how the host's client
//! reaches the node agent in a Shards guest over vsock, which carries bytes and no descriptors. Pure: the
//! shells, the host's guest client and the guest's relay, move the bytes; this decides where each message
//! ends, what each stdio connection names, and when an exec's streams are all there.
//!
//! - **Control:** each [`wire`] message framed by its length, a big-endian `u32`, at most a bound the
//!   reader holds ([`wire::MAX_MESSAGE`] or the node's own); a longer one is refused from its prefix,
//!   before its body is buffered.
//! - **Stdio:** an exec's stdin, stdout and stderr each ride a connection of their own, whose first
//!   [`NAME_BYTES`] bytes name its control connection (the [`TOKEN_BYTES`] the relay sent first on it),
//!   the exec (its request's id, which is its connection's own) and the stream. In the guest, that
//!   connection's socket is the descriptor the relay passes the node as the program's stream.
//! - **Gather:** the relay holds an exec, or the streams that came before it, until the set its exec
//!   names is whole ([`Gather`]), at most a bound of sets at once, each until it is whole or its control
//!   connection ends.

use std::collections::BTreeMap;

use crate::wire::{self, Fault};

pub mod files;

/// The guest's vsock port the host's client dials for its control connection, and the one it dials for
/// each stdio stream: identifiers assigned in docs/design/shards-guest.md §3a, above vsock(7)'s
/// privileged ports (below 1024 take `CAP_NET_BIND_SERVICE` to bind) and below its `VMADDR_PORT_ANY`,
/// spelling `HEX` so a capture reads them.
pub const CONTROL_PORT: u32 = 0x4845_5800;
pub const STDIO_PORT: u32 = 0x4845_5801;
/// The guest's vsock port that reports its boot's own stamps (§3a): what a boot-time record splits a boot
/// by, a line of `CLOCK_BOOTTIME` nanoseconds at init's start, its mounts, its node ready and its relay
/// listening.
pub const STAMPS_PORT: u32 = 0x4845_5803;

/// The file name of the relay's listener for `port` where it listens on AF_UNIX stream sockets in a directory of its
/// own rather than on vsock: hex-cli's `hex guest relay --unix`, a node under shards-init, whose host reaches each by a
/// socket lane of its run's agent-control endpoint (docs/design/node-under-shards-init.md §5). `None` for a port the
/// relay doesn't listen on. docs/design/shards-guest.md §3a's ports table names each (HEX-GUEST-30).
pub fn socket_name(port: u32) -> Option<&'static str> {
    match port {
        CONTROL_PORT => Some("control.sock"),
        STDIO_PORT => Some("stdio.sock"),
        files::FILES_PORT => Some("files.sock"),
        STAMPS_PORT => Some("stamps.sock"),
        _ => None,
    }
}

/// AC-T7.4.1: a Firecracker guest sends this versioned readiness receipt only after
/// its node and relay listeners are ready. The host owns the private per-VM socket;
/// this is a boot notification, not a control connection or an attachment grant.
pub const GUEST_READY: &[u8] = b"HEX_READY 1\n";
/// Firecracker reserves [2^30, 2^31) for host-initiated connection ports and rejects
/// guest-initiated requests there (pinned500efc6 docs/vsock.md). Clear that reserved
/// bit in the assigned HEX control port for the separate guest-to-host notification.
pub const GUEST_READY_PORT: u32 = CONTROL_PORT & !(1u32 << 30);

/// A frame's prefix: the message's length, a big-endian `u32` (shards-guest.md §3a).
const PREFIX: usize = size_of::<u32>();

/// The longest frame of a message at the wire's bound: what one read of a control connection takes at
/// most, the unit it reads.
pub const MAX_FRAME: usize = wire::MAX_MESSAGE + PREFIX;

/// A control connection's first bytes, the relay's, before any frame: the connection's token (`u64`,
/// little-endian), which its stdio connections name, as its request ids are its own (§3a).
pub const TOKEN_BYTES: usize = size_of::<u64>();

/// A stdio connection's first bytes: `HEXS`, the version (`u16`, little-endian as the wire's
/// integers), the stream, a zero byte, its control connection's token (`u64`) and the exec's id (`u64`):
/// §3a's table, the sum of its fields.
pub const NAME_BYTES: usize = NAME_MAGIC.len()
    + size_of::<u16>()
    + size_of::<u8>()
    + size_of::<u8>()
    + TOKEN_BYTES
    + size_of::<u64>();
const NAME_MAGIC: [u8; 4] = *b"HEXS";
/// The stdio name's only version. It changes append-only, as the wire's.
const NAME_VERSION: u16 = 1;

/// `message` framed, when it is at most `bound` bytes.
pub fn frame(message: &[u8], bound: usize) -> Result<Vec<u8>, Fault> {
    let oversized = || Fault::Oversized {
        bytes: message.len(),
        bound,
    };
    if message.len() > bound {
        return Err(oversized());
    }
    let len = u32::try_from(message.len()).map_err(|_| oversized())?;
    let mut out = Vec::with_capacity(message.len().checked_add(PREFIX).ok_or_else(oversized)?);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(message);
    Ok(out)
}

/// Reads framed messages from a stream's bytes as they come. It holds at most one frame: its prefix,
/// then its body, never more than its bound.
#[derive(Debug)]
pub struct Deframer {
    bound: usize,
    prefix: [u8; PREFIX],
    have_prefix: usize,
    /// The body under way, once its prefix is whole, and its announced length.
    body: Option<(usize, Vec<u8>)>,
    /// Why the stream can't be read further, once it can't: every later take is refused the same.
    broken: Option<Fault>,
}

impl Deframer {
    /// A reader of messages of at most `bound` bytes.
    pub fn new(bound: usize) -> Self {
        Self {
            bound,
            prefix: [0; PREFIX],
            have_prefix: 0,
            body: None,
            broken: None,
        }
    }

    /// Whether a frame is under way: some of it has come and the rest hasn't.
    pub fn in_frame(&self) -> bool {
        self.have_prefix > 0 || self.body.is_some()
    }

    /// Takes bytes from `input`, at most to the end of the frame under way, and returns how many it took
    /// (at least one, when `input` has any) and the message, once whole. The caller takes again with
    /// what is left.
    pub fn take(&mut self, input: &[u8]) -> Result<(usize, Option<Vec<u8>>), Fault> {
        if let Some(fault) = &self.broken {
            return Err(fault.clone());
        }
        match self.step(input) {
            Ok(taken) => Ok(taken),
            Err(fault) => {
                self.broken = Some(fault.clone());
                Err(fault)
            }
        }
    }

    fn step(&mut self, input: &[u8]) -> Result<(usize, Option<Vec<u8>>), Fault> {
        let Some((len, body)) = self.body.as_mut() else {
            let room = self
                .prefix
                .get_mut(self.have_prefix..)
                .ok_or_else(|| Fault::value("frame", "its prefix is over-full"))?;
            let n = room.len().min(input.len());
            let (Some(into), Some(from)) = (room.get_mut(..n), input.get(..n)) else {
                return Err(Fault::value("frame", "its prefix is over-full"));
            };
            into.copy_from_slice(from);
            self.have_prefix = self
                .have_prefix
                .checked_add(n)
                .ok_or_else(|| Fault::value("frame", "its prefix is over-full"))?;
            if self.have_prefix == PREFIX {
                let len = usize::try_from(u32::from_be_bytes(self.prefix)).map_err(|_| {
                    Fault::value("frame", "its length doesn't fit this machine's addresses")
                })?;
                if len > self.bound {
                    return Err(Fault::Oversized {
                        bytes: len,
                        bound: self.bound,
                    });
                }
                if len < wire::HEADER_BYTES {
                    return Err(Fault::Truncated {
                        need: wire::HEADER_BYTES,
                        have: len,
                    });
                }
                self.body = Some((len, Vec::with_capacity(len)));
            }
            return Ok((n, None));
        };
        let want = len.saturating_sub(body.len());
        let n = want.min(input.len());
        body.extend_from_slice(input.get(..n).unwrap_or_default());
        if body.len() < *len {
            return Ok((n, None));
        }
        let whole = std::mem::take(body);
        self.body = None;
        self.have_prefix = 0;
        Ok((n, Some(whole)))
    }

    /// Whether the stream may end here: no frame is partly read.
    pub fn at_boundary(&self) -> bool {
        self.broken.is_none() && self.have_prefix == 0 && self.body.is_none()
    }

    /// Bytes held of the frame under way, its prefix's included.
    pub fn buffered(&self) -> usize {
        self.body
            .as_ref()
            .map_or(self.have_prefix, |(_, body)| body.len())
    }
}

/// A frame cut short where its stream ends may be a message carrying a lease's value or a sign-in's tokens, which it
/// can't yet tell: what came of it is wiped as the reader goes (docs/design/credential-grants.md C7, C8; the review's
/// E-6). A whole message is its taker's, who wipes it as its kind says.
impl Drop for Deframer {
    fn drop(&mut self) {
        if let Some((_, body)) = self.body.as_mut() {
            hex_api::lease::wipe(body);
        }
    }
}

/// One of a program's standard streams.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Stdio {
    In,
    Out,
    Err,
}

impl Stdio {
    fn tag(self) -> u8 {
        match self {
            Self::In => 0,
            Self::Out => 1,
            Self::Err => 2,
        }
    }

    fn of(tag: u8) -> Result<Self, Fault> {
        match tag {
            0 => Ok(Self::In),
            1 => Ok(Self::Out),
            2 => Ok(Self::Err),
            other => Err(Fault::value(
                "stdio name",
                format!("stream {other} is none of stdin, stdout and stderr"),
            )),
        }
    }

    fn wanted(self, streams: wire::Streams) -> bool {
        match self {
            Self::In => streams.stdin,
            Self::Out => streams.stdout,
            Self::Err => streams.stderr,
        }
    }
}

/// An exec, by its control connection's token and its request id there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Exec {
    pub conn: u64,
    pub id: u64,
}

/// What a stdio connection names: its exec and which stream it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Named {
    pub exec: Exec,
    pub stream: Stdio,
}

/// A stdio connection's first bytes.
pub fn name(named: Named) -> [u8; NAME_BYTES] {
    let mut out = [0; NAME_BYTES];
    let parts: [&[u8]; 6] = [
        &NAME_MAGIC,
        &NAME_VERSION.to_le_bytes(),
        &[named.stream.tag()],
        &[0],
        &named.exec.conn.to_le_bytes(),
        &named.exec.id.to_le_bytes(),
    ];
    for (to, from) in out.iter_mut().zip(parts.into_iter().flatten()) {
        *to = *from;
    }
    out
}

/// What a stdio connection's first bytes name, or why they name nothing.
pub fn read_name(bytes: &[u8; NAME_BYTES]) -> Result<Named, Fault> {
    let [
        m0,
        m1,
        m2,
        m3,
        v0,
        v1,
        stream,
        pad,
        c0,
        c1,
        c2,
        c3,
        c4,
        c5,
        c6,
        c7,
        id @ ..,
    ] = *bytes;
    if [m0, m1, m2, m3] != NAME_MAGIC {
        return Err(Fault::Magic);
    }
    let version = u16::from_le_bytes([v0, v1]);
    if version != NAME_VERSION {
        return Err(Fault::Version {
            got: version,
            speaks: NAME_VERSION,
        });
    }
    if pad != 0 {
        return Err(Fault::value(
            "stdio name",
            format!("its pad byte is {pad}, not 0"),
        ));
    }
    Ok(Named {
        exec: Exec {
            conn: u64::from_le_bytes([c0, c1, c2, c3, c4, c5, c6, c7]),
            id: u64::from_le_bytes(id),
        },
        stream: Stdio::of(stream)?,
    })
}

/// Why the gather refused an exec or a stream.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Why {
    #[error("it came twice")]
    Twice,
    #[error("its exec didn't ask for that stream")]
    Unwanted,
    #[error("{bound} execs are already waiting for their streams")]
    Full { bound: usize },
}

/// A stream the gather refused, handed back for the shell to close.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stray<C> {
    pub conn: C,
    pub why: Why,
}

/// An exec's id and its connections, in stdin-stdout-stderr order of those it named.
pub type Complete<C> = (Exec, Vec<C>);

/// An exec the gather refused, with the streams that came for it before, for the shell to close.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused<C> {
    pub why: Why,
    pub conns: Vec<C>,
}

/// Execs waiting for their streams, and streams waiting for their execs, by the exec's id.
#[derive(Debug)]
pub struct Gather<C> {
    bound: usize,
    sets: BTreeMap<Exec, Set<C>>,
}

#[derive(Debug)]
struct Set<C> {
    /// What the exec asked for, once it has come.
    want: Option<wire::Streams>,
    have: BTreeMap<Stdio, C>,
}

impl<C> Set<C> {
    fn complete(&mut self) -> Option<Vec<C>> {
        let want = self.want?;
        let needed = [Stdio::In, Stdio::Out, Stdio::Err]
            .into_iter()
            .filter(|s| s.wanted(want));
        if needed.clone().any(|s| !self.have.contains_key(&s)) {
            return None;
        }
        let have = std::mem::take(&mut self.have);
        Some(have.into_values().collect())
    }
}

impl<C> Gather<C> {
    /// A gather of at most `bound` sets at once.
    pub fn new(bound: usize) -> Self {
        Self {
            bound,
            sets: BTreeMap::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.sets.is_empty()
    }

    fn set(&mut self, exec: Exec) -> Result<&mut Set<C>, Why> {
        if !self.sets.contains_key(&exec) && self.sets.len() >= self.bound {
            return Err(Why::Full { bound: self.bound });
        }
        Ok(self.sets.entry(exec).or_insert_with(|| Set {
            want: None,
            have: BTreeMap::new(),
        }))
    }

    fn finish(&mut self, exec: Exec) -> Option<Complete<C>> {
        let conns = self.sets.get_mut(&exec)?.complete()?;
        self.sets.remove(&exec);
        Some((exec, conns))
    }

    /// The exec `exec` has come, asking for `streams`: its set, once whole.
    pub fn expect(
        &mut self,
        exec: Exec,
        streams: wire::Streams,
    ) -> Result<Option<Complete<C>>, Refused<C>> {
        let refused = |why| Refused {
            why,
            conns: Vec::new(),
        };
        let set = self.set(exec).map_err(refused)?;
        if set.want.is_some() {
            return Err(refused(Why::Twice));
        }
        if set.have.keys().any(|s| !s.wanted(streams)) {
            // A stream came that its exec doesn't ask for: the exec is refused, and its set with it.
            let conns = self
                .sets
                .remove(&exec)
                .map(|set| set.have.into_values().collect())
                .unwrap_or_default();
            return Err(Refused {
                why: Why::Unwanted,
                conns,
            });
        }
        set.want = Some(streams);
        Ok(self.finish(exec))
    }

    /// A stream named `named` has come on `conn`: its exec's set, once whole, or the stream handed back
    /// with why it is refused.
    pub fn arrive(&mut self, named: Named, conn: C) -> Result<Option<Complete<C>>, Stray<C>> {
        let set = match self.set(named.exec) {
            Ok(set) => set,
            Err(why) => return Err(Stray { conn, why }),
        };
        if set.want.is_some_and(|want| !named.stream.wanted(want)) {
            return Err(Stray {
                conn,
                why: Why::Unwanted,
            });
        }
        if set.have.contains_key(&named.stream) {
            return Err(Stray {
                conn,
                why: Why::Twice,
            });
        }
        set.have.insert(named.stream, conn);
        Ok(self.finish(named.exec))
    }

    /// The sets of control connection `conn`, which has ended, taken out with the connections they
    /// held, for the shell to close: none of its execs can come now, nor be answered.
    pub fn forget(&mut self, conn: u64) -> Vec<Complete<C>> {
        let ended: Vec<Exec> = self
            .sets
            .range(Exec { conn, id: 0 }..=Exec { conn, id: u64::MAX })
            .map(|(exec, _)| *exec)
            .collect();
        ended
            .into_iter()
            .filter_map(|exec| {
                self.sets
                    .remove(&exec)
                    .map(|set| (exec, set.have.into_values().collect()))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests;
