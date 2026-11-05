//! The capsules of a WebTransport session's CONNECT stream (RFC 9297 §3, draft-14 §6), read as
//! they arrive and without holding what an attacker sizes (goal 4 step 4).
//!
//! An unknown capsule is skipped by its length while only its header is held (RFC 9297 §3.2:
//! "MUST silently drop that Capsule and skip over it"; webtransport-go read such capsules whole,
//! CVE-2026-57497): one declaring 2^62 - 1 bytes costs the parser its few bytes of state, however
//! much of it arrives. A WT_CLOSE_SESSION's body is at most 4 + 1024 bytes (draft-14 §6), a
//! DATAGRAM capsule's at most the datagram bound (larger ones are discarded unbuffered, RFC 9297
//! §3.5), WT_DRAIN_SESSION is empty, and the parser allocates only while one of those is being read.
//! Draft-14's flow-control capsules are unknown here: the door doesn't enable WebTransport flow
//! control, and then they're ignored (draft-14 §5.1).

use super::varint::Partial;

/// Capsule types the door reads.
pub mod kind {
    /// HTTP Datagram (RFC 9297 §3.5).
    pub const DATAGRAM: u64 = 0x00;
    /// WT_CLOSE_SESSION (draft-14 §6; draft-02's CLOSE_WEBTRANSPORT_SESSION has the same id).
    pub const CLOSE_SESSION: u64 = 0x2843;
    /// WT_DRAIN_SESSION (draft-14 §4.7; draft-02's DRAIN_WEBTRANSPORT_SESSION has the same id).
    pub const DRAIN_SESSION: u64 = 0x78ae;
}

/// The longest message a WT_CLOSE_SESSION carries (draft-14 §6).
pub const CLOSE_MESSAGE_MAX: usize = 1024;

/// A capsule read whole.
#[derive(Debug, PartialEq, Eq)]
pub enum Capsule<'a> {
    /// The peer closed the session: its application error code and message.
    Close { code: u32, message: &'a [u8] },
    /// The peer asks for the session to end soon.
    Drain,
    /// A datagram carried on the stream (within the bound).
    Datagram(&'a [u8]),
}

#[derive(Debug)]
enum State {
    Kind(Partial),
    Length { kind: u64, length: Partial },
    /// Skipping this many more bytes of a capsule that isn't kept.
    Skip(u64),
    /// Reading the body of a capsule that is kept, up to `need` bytes.
    Body { kind: u64, need: usize, body: Vec<u8> },
}

/// The capsule stream's reader: its state between reads.
#[derive(Debug)]
pub struct Capsules {
    state: State,
    /// The largest DATAGRAM capsule kept.
    datagram_max: usize,
}

/// A malformed capsule: the session's stream error, H3_MESSAGE_ERROR (RFC 9114 §4.1.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Malformed;

impl Capsules {
    /// A reader that keeps DATAGRAM capsules of at most `datagram_max` bytes.
    pub fn new(datagram_max: usize) -> Capsules {
        Capsules { state: State::Kind(Partial::default()), datagram_max }
    }

    /// Whether the reader is between capsules.
    pub fn at_boundary(&self) -> bool {
        matches!(&self.state, State::Kind(p) if !p.started())
    }

    /// What it holds beyond its own size: a kept capsule's body being read.
    pub fn held(&self) -> usize {
        match &self.state {
            State::Body { body, .. } => body.capacity(),
            _ => 0,
        }
    }

    /// Takes the next bytes of the stream, handing each capsule read whole to `each`.
    pub fn read(&mut self, mut data: &[u8], each: &mut impl FnMut(Capsule<'_>)) -> Result<(), Malformed> {
        while !data.is_empty() {
            match &mut self.state {
                State::Kind(p) => {
                    let (v, n) = p.take(data);
                    data = data.get(n..).unwrap_or_default();
                    if let Some(kind) = v {
                        self.state = State::Length { kind, length: Partial::default() };
                    }
                }
                State::Length { kind, length } => {
                    let kind = *kind;
                    let (v, n) = length.take(data);
                    data = data.get(n..).unwrap_or_default();
                    let Some(len) = v else { continue };
                    self.state = self.begin(kind, len)?;
                    if let State::Body { kind, need: 0, .. } = self.state {
                        self.end(kind, &[], each)?;
                    }
                }
                State::Skip(left) => {
                    let n = usize::try_from(*left).unwrap_or(usize::MAX).min(data.len());
                    *left -= n as u64;
                    data = data.get(n..).unwrap_or_default();
                    if *left == 0 {
                        self.state = State::Kind(Partial::default());
                    }
                }
                State::Body { kind, need, body } => {
                    let n = (*need - body.len()).min(data.len());
                    let (take, rest) = data.split_at_checked(n).unwrap_or((data, &[]));
                    body.extend_from_slice(take);
                    data = rest;
                    if body.len() == *need {
                        let kind = *kind;
                        let body = std::mem::take(body);
                        self.end(kind, &body, each)?;
                    }
                }
            }
        }
        Ok(())
    }

    /// What follows a capsule's header.
    fn begin(&self, kind: u64, len: u64) -> Result<State, Malformed> {
        let keep = |need: u64| State::Body { kind, need: need as usize, body: Vec::with_capacity(need as usize) };
        Ok(match kind {
            kind::CLOSE_SESSION if (4..=4 + CLOSE_MESSAGE_MAX as u64).contains(&len) => keep(len),
            kind::CLOSE_SESSION => return Err(Malformed),
            kind::DRAIN_SESSION if len == 0 => keep(0),
            kind::DRAIN_SESSION => return Err(Malformed),
            kind::DATAGRAM if len <= self.datagram_max as u64 => keep(len),
            _ if len == 0 => State::Kind(Partial::default()),
            _ => State::Skip(len),
        })
    }

    /// A kept capsule read whole.
    fn end(&mut self, kind: u64, body: &[u8], each: &mut impl FnMut(Capsule<'_>)) -> Result<(), Malformed> {
        self.state = State::Kind(Partial::default());
        match kind {
            kind::CLOSE_SESSION => {
                let (code, message) = body.split_at_checked(4).ok_or(Malformed)?;
                let code = u32::from_be_bytes(code.try_into().map_err(|_| Malformed)?);
                each(Capsule::Close { code, message });
            }
            kind::DRAIN_SESSION => each(Capsule::Drain),
            kind::DATAGRAM => each(Capsule::Datagram(body)),
            _ => {}
        }
        Ok(())
    }
}

/// A WT_CLOSE_SESSION capsule (`message` cut to [`CLOSE_MESSAGE_MAX`]).
pub fn close(code: u32, message: &[u8]) -> Vec<u8> {
    let message = message.get(..CLOSE_MESSAGE_MAX).unwrap_or(message);
    let mut out = Vec::with_capacity(16 + message.len());
    super::varint::put(&mut out, kind::CLOSE_SESSION);
    super::varint::put(&mut out, 4 + message.len() as u64);
    out.extend_from_slice(&code.to_be_bytes());
    out.extend_from_slice(message);
    out
}

/// A WT_DRAIN_SESSION capsule.
pub fn drain() -> Vec<u8> {
    let mut out = Vec::with_capacity(4);
    super::varint::put(&mut out, kind::DRAIN_SESSION);
    super::varint::put(&mut out, 0);
    out
}
