//! HTTP/2 frames (RFC 9113 §4, §6): the nine-byte head, the kinds, their flags, and the settings.

/// What a client says first (RFC 9113 §3.4).
pub(crate) const PREFACE: &[u8; 24] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

pub(crate) const DATA: u8 = 0x0;
pub(crate) const HEADERS: u8 = 0x1;
pub(crate) const PRIORITY: u8 = 0x2;
pub(crate) const RST_STREAM: u8 = 0x3;
pub(crate) const SETTINGS: u8 = 0x4;
pub(crate) const PUSH_PROMISE: u8 = 0x5;
pub(crate) const PING: u8 = 0x6;
pub(crate) const GOAWAY: u8 = 0x7;
pub(crate) const WINDOW_UPDATE: u8 = 0x8;
pub(crate) const CONTINUATION: u8 = 0x9;

pub(crate) const END_STREAM: u8 = 0x1;
pub(crate) const ACK: u8 = 0x1;
pub(crate) const END_HEADERS: u8 = 0x4;
pub(crate) const PADDED: u8 = 0x8;
pub(crate) const PRIORITY_FLAG: u8 = 0x20;

pub(crate) const HEADER_TABLE_SIZE: u16 = 0x1;
pub(crate) const ENABLE_PUSH: u16 = 0x2;
pub(crate) const MAX_CONCURRENT_STREAMS: u16 = 0x3;
pub(crate) const INITIAL_WINDOW_SIZE: u16 = 0x4;
pub(crate) const MAX_FRAME_SIZE: u16 = 0x5;
pub(crate) const MAX_HEADER_LIST_SIZE: u16 = 0x6;

pub(crate) const NO_ERROR: u32 = 0x0;
pub(crate) const PROTOCOL_ERROR: u32 = 0x1;
pub(crate) const INTERNAL_ERROR: u32 = 0x2;
pub(crate) const FLOW_CONTROL_ERROR: u32 = 0x3;
pub(crate) const STREAM_CLOSED: u32 = 0x5;
pub(crate) const FRAME_SIZE_ERROR: u32 = 0x6;
pub(crate) const REFUSED_STREAM: u32 = 0x7;
pub(crate) const CANCEL: u32 = 0x8;
pub(crate) const COMPRESSION_ERROR: u32 = 0x9;
pub(crate) const ENHANCE_YOUR_CALM: u32 = 0xb;

/// The default (and smallest) largest frame payload (RFC 9113 §6.5.2).
pub(crate) const DEFAULT_MAX_FRAME: u32 = 16_384;
/// The largest window (RFC 9113 §6.9.1).
pub(crate) const MAX_WINDOW: i64 = (1 << 31) - 1;
/// Every stream's and the connection's window before any settings (RFC 9113 §6.9.2).
pub(crate) const DEFAULT_WINDOW: i64 = 65_535;

/// A frame's head.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Head {
    pub len: usize,
    pub kind: u8,
    pub flags: u8,
    pub stream: u32,
}

/// The head at the start of `buf`, when nine bytes are there.
pub(crate) fn head(buf: &[u8]) -> Option<Head> {
    let h: &[u8; 9] = buf.get(..9)?.try_into().ok()?;
    let [l0, l1, l2, kind, flags, s0, s1, s2, s3] = *h;
    Some(Head { len: (usize::from(l0) << 16) | (usize::from(l1) << 8) | usize::from(l2), kind, flags, stream: u32::from_be_bytes([s0, s1, s2, s3]) & 0x7fff_ffff })
}

pub(crate) fn put_head(out: &mut Vec<u8>, len: usize, kind: u8, flags: u8, stream: u32) {
    let l = (len as u32).to_be_bytes();
    let s = stream.to_be_bytes();
    out.extend_from_slice(&[l[1], l[2], l[3], kind, flags, s[0], s[1], s[2], s[3]]);
}

pub(crate) fn put_settings(out: &mut Vec<u8>, settings: &[(u16, u32)]) {
    put_head(out, settings.len() * 6, SETTINGS, 0, 0);
    for (id, v) in settings {
        out.extend_from_slice(&id.to_be_bytes());
        out.extend_from_slice(&v.to_be_bytes());
    }
}

pub(crate) fn put_window_update(out: &mut Vec<u8>, stream: u32, increment: u32) {
    put_head(out, 4, WINDOW_UPDATE, 0, stream);
    out.extend_from_slice(&(increment & 0x7fff_ffff).to_be_bytes());
}

pub(crate) fn put_rst(out: &mut Vec<u8>, stream: u32, code: u32) {
    put_head(out, 4, RST_STREAM, 0, stream);
    out.extend_from_slice(&code.to_be_bytes());
}

pub(crate) fn put_goaway(out: &mut Vec<u8>, last_stream: u32, code: u32) {
    put_head(out, 8, GOAWAY, 0, 0);
    out.extend_from_slice(&(last_stream & 0x7fff_ffff).to_be_bytes());
    out.extend_from_slice(&code.to_be_bytes());
}

pub(crate) fn put_ping(out: &mut Vec<u8>, ack: bool, data: [u8; 8]) {
    put_head(out, 8, PING, if ack { ACK } else { 0 }, 0);
    out.extend_from_slice(&data);
}

/// A big-endian `u32` at `at`.
pub(crate) fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heads_round_trip() {
        let mut out = Vec::new();
        put_head(&mut out, 0x01_02_03, HEADERS, END_HEADERS | END_STREAM, 0x7fff_ffff);
        assert_eq!(head(&out), Some(Head { len: 0x01_02_03, kind: HEADERS, flags: 5, stream: 0x7fff_ffff }));
        // The reserved bit is ignored on receipt.
        assert_eq!(head(&[0, 0, 0, 0, 0, 0x80, 0, 0, 1]).map(|h| h.stream), Some(1));
        assert_eq!(head(&[0, 0, 0]), None);
    }
}
