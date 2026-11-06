//! HTTP/3 (RFC 9114) for one purpose: the door browsers open a WebTransport session through
//! (goal 4, plan 01a1a832eb597110a6dcfda227981604). It accepts one extended CONNECT with
//! `:protocol webtransport` per connection and nothing else, decodes field sections with QPACK at
//! table capacity 0 within a budget counted field by field ([`qpack`]), and holds no buffer an
//! attacker sizes.
//!
//! Sans-IO, in layers that each take what they need passed in (no clock, no socket, no randomness):
//!
//! * [`qpack`]: field sections, and the peer's encoder and decoder streams.
//! * [`conn`]: one connection's HTTP/3 (its control stream, its SETTINGS, the one CONNECT) and the
//!   WebTransport session it admits ([`conn::Connection`]), over any [`Transport`]: the QUIC
//!   streams and datagrams a caller reads and writes for it.
//! * [`capsule`]: the CONNECT stream's capsules (RFC 9297), read without holding their bodies.
//! * [`door`]: the QUIC endpoint (hyper-quic) with every transport limit set from one memory
//!   bound a connection, and the HTTP/3 core on each connection.
//!
//! Every browser's handshake at once: Chrome, Edge and Firefox speak draft-02's
//! (`SETTINGS_ENABLE_WEBTRANSPORT` 0x2b603742), Safari drafts 07 to 14 (0xc671706a, 0x14e9cd29); the
//! door advertises all three and records which one each connection spoke ([`Draft`]). Drafts 15
//! and 16 also need RESET_STREAM_AT, which hyper-quic doesn't have yet: a later dialect, added as
//! a setting (the protocol only grows).

pub mod capsule;
pub mod conn;
pub mod door;
pub mod driver;
pub mod qpack;
pub(crate) mod varint;

#[cfg(test)]
mod attacks;
#[cfg(test)]
mod door_tests;
#[cfg(test)]
mod fuzz;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod testnet;
#[cfg(test)]
mod vectors;

pub use conn::{Admission, Admit, Connect, Connection, Event, Read, Transport};
pub use qpack::Budget;

/// The door's field-section budget (goal 4 step 1), from what the browsers sent in the capture
/// (`crates/kiln-http/tests/vectors/webtransport/`): the largest CONNECT field section was
/// Firefox 157's, 1,020 bytes in 17 fields (Chromium 156's 414 in 7), so four times it plus 214
/// bytes for a ticket and a principal in the path, 4,294, rounded up to 4,608 (at most 16 KiB),
/// and four times its fields, 68, rounded up to 72. Advertised as `SETTINGS_MAX_FIELD_SECTION_SIZE`
/// and enforced by the decoder: the same constant (FoxIO's mistake 5).
pub const DOOR_BUDGET: Budget = Budget { bytes: 4608, fields: 72 };

/// HTTP/3's error codes (RFC 9114 §8.1) and WebTransport's (draft-ietf-webtrans-http3-14 §9.5).
pub mod code {
    pub const H3_NO_ERROR: u64 = 0x0100;
    pub const H3_GENERAL_PROTOCOL_ERROR: u64 = 0x0101;
    pub const H3_INTERNAL_ERROR: u64 = 0x0102;
    pub const H3_STREAM_CREATION_ERROR: u64 = 0x0103;
    pub const H3_CLOSED_CRITICAL_STREAM: u64 = 0x0104;
    pub const H3_FRAME_UNEXPECTED: u64 = 0x0105;
    pub const H3_FRAME_ERROR: u64 = 0x0106;
    pub const H3_EXCESSIVE_LOAD: u64 = 0x0107;
    pub const H3_ID_ERROR: u64 = 0x0108;
    pub const H3_SETTINGS_ERROR: u64 = 0x0109;
    pub const H3_MISSING_SETTINGS: u64 = 0x010a;
    pub const H3_REQUEST_REJECTED: u64 = 0x010b;
    pub const H3_REQUEST_CANCELLED: u64 = 0x010c;
    pub const H3_REQUEST_INCOMPLETE: u64 = 0x010d;
    pub const H3_MESSAGE_ERROR: u64 = 0x010e;
    pub const H3_CONNECT_ERROR: u64 = 0x010f;
    pub const WT_BUFFERED_STREAM_REJECTED: u64 = 0x3994_bd84;
    pub const WT_SESSION_GONE: u64 = 0x170d_7b68;
    pub const WT_FLOW_CONTROL_ERROR: u64 = 0x045d_4487;
    /// The first and last codes of the range WebTransport's application errors map into.
    pub const WT_APPLICATION_ERROR_FIRST: u64 = 0x52e4_a40f_a8db;
    pub const WT_APPLICATION_ERROR_LAST: u64 = 0x52e5_ac98_3162;
}

/// A WebTransport application error code (32 bits) as the HTTP/3 code a stream reset carries
/// (draft-14 §4.4: the range skips the reserved codepoints `0x1f * N + 0x21`).
pub fn wt_error_to_h3(n: u32) -> u64 {
    let n = u64::from(n);
    code::WT_APPLICATION_ERROR_FIRST + n + n / 0x1e
}

/// The WebTransport application error an HTTP/3 code carries, if it is one.
pub fn h3_error_to_wt(h: u64) -> Option<u32> {
    if !(code::WT_APPLICATION_ERROR_FIRST..=code::WT_APPLICATION_ERROR_LAST).contains(&h) || (h - 0x21).is_multiple_of(0x1f) {
        return None;
    }
    let shifted = h - code::WT_APPLICATION_ERROR_FIRST;
    u32::try_from(shifted - shifted / 0x1f).ok()
}

/// HTTP/3 frame types (RFC 9114 §7.2) the door reads or writes.
pub(crate) mod frame {
    pub const DATA: u64 = 0x00;
    pub const HEADERS: u64 = 0x01;
    pub const CANCEL_PUSH: u64 = 0x03;
    pub const SETTINGS: u64 = 0x04;
    pub const PUSH_PROMISE: u64 = 0x05;
    pub const GOAWAY: u64 = 0x07;
    pub const MAX_PUSH_ID: u64 = 0x0d;
    /// WebTransport's bidirectional stream signal (not a frame: no length).
    pub const WT_STREAM: u64 = 0x41;

    /// HTTP/2's frame types with no HTTP/3 meaning (RFC 9114 §7.2.8): H3_FRAME_UNEXPECTED.
    pub fn reserved_h2(t: u64) -> bool {
        matches!(t, 0x02 | 0x06 | 0x08 | 0x09)
    }
}

/// Unidirectional stream types (RFC 9114 §6.2, RFC 9204 §4.2, draft-14 §4.2).
pub(crate) mod stream_type {
    pub const CONTROL: u64 = 0x00;
    pub const PUSH: u64 = 0x01;
    pub const ENCODER: u64 = 0x02;
    pub const DECODER: u64 = 0x03;
    pub const WEBTRANSPORT: u64 = 0x54;
}

/// Setting identifiers the door sends or reads.
pub mod setting {
    pub const QPACK_MAX_TABLE_CAPACITY: u64 = 0x01;
    pub const MAX_FIELD_SECTION_SIZE: u64 = 0x06;
    pub const QPACK_BLOCKED_STREAMS: u64 = 0x07;
    pub const ENABLE_CONNECT_PROTOCOL: u64 = 0x08;
    pub const H3_DATAGRAM: u64 = 0x33;
    /// SETTINGS_H3_DATAGRAM before RFC 9297 (draft-04's id), which Chrome and Firefox still read.
    pub const H3_DATAGRAM_DRAFT04: u64 = 0xff_d277;
    /// draft-02's SETTINGS_ENABLE_WEBTRANSPORT: Chrome, Edge and Firefox.
    pub const ENABLE_WEBTRANSPORT: u64 = 0x2b60_3742;
    /// drafts 07-12's SETTINGS_WEBTRANSPORT_MAX_SESSIONS.
    pub const WEBTRANSPORT_MAX_SESSIONS_DRAFT07: u64 = 0xc671_706a;
    /// drafts 13-14's SETTINGS_WT_MAX_SESSIONS: Safari.
    pub const WT_MAX_SESSIONS: u64 = 0x14e9_cd29;
    /// draft-15's SETTINGS_WT_ENABLED (read, not sent: draft-15 needs RESET_STREAM_AT).
    pub const WT_ENABLED_DRAFT15: u64 = 0x2c7c_f000;

    /// HTTP/2's settings with no HTTP/3 meaning (RFC 9114 §7.2.4.1): H3_SETTINGS_ERROR.
    pub fn reserved_h2(id: u64) -> bool {
        matches!(id, 0x02..=0x05)
    }
}

/// The WebTransport handshake a connection spoke.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Draft {
    /// draft-ietf-webtrans-http3-02: `SETTINGS_ENABLE_WEBTRANSPORT`, and the CONNECT's
    /// `sec-webtransport-http3-draft02: 1` answered with `sec-webtransport-http3-draft: draft02`.
    Draft02,
    /// drafts 07 to 12: `SETTINGS_WEBTRANSPORT_MAX_SESSIONS` (0xc671706a).
    Draft07,
    /// drafts 13 and 14: `SETTINGS_WT_MAX_SESSIONS` (0x14e9cd29).
    Draft14,
}

impl Draft {
    /// Its name, as reports and docs use it.
    pub fn name(self) -> &'static str {
        match self {
            Draft::Draft02 => "draft-02",
            Draft::Draft07 => "draft-07",
            Draft::Draft14 => "draft-14",
        }
    }
}

/// The settings a door sends: one list, written in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings(pub Vec<(u64, u64)>);

impl Settings {
    /// The door's (goal 4 step 3): no QPACK table, no blocked streams, the field-section budget,
    /// extended CONNECT, HTTP datagrams under both ids, and every WebTransport dialect, each at
    /// one session (no WebTransport flow control: draft-14 §5.1).
    pub fn door() -> Settings {
        Settings::with_budget(DOOR_BUDGET)
    }

    /// [`Settings::door`] advertising `budget`.
    pub fn with_budget(budget: Budget) -> Settings {
        use setting::*;
        Settings(vec![
            (QPACK_MAX_TABLE_CAPACITY, 0),
            (QPACK_BLOCKED_STREAMS, 0),
            (MAX_FIELD_SECTION_SIZE, budget.bytes as u64),
            (ENABLE_CONNECT_PROTOCOL, 1),
            (H3_DATAGRAM, 1),
            (H3_DATAGRAM_DRAFT04, 1),
            (ENABLE_WEBTRANSPORT, 1),
            (WEBTRANSPORT_MAX_SESSIONS_DRAFT07, 1),
            (WT_MAX_SESSIONS, 1),
        ])
    }
}

/// A SETTINGS frame's payload.
pub(crate) fn settings_payload(s: &Settings) -> Vec<u8> {
    let mut out = Vec::new();
    for &(id, v) in &s.0 {
        varint::put(&mut out, id);
        varint::put(&mut out, v);
    }
    out
}

/// A frame: its type, its length, its payload.
pub(crate) fn put_frame(out: &mut Vec<u8>, kind: u64, payload: &[u8]) {
    varint::put(out, kind);
    varint::put(out, payload.len() as u64);
    out.extend_from_slice(payload);
}
