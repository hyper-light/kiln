//! HPACK (RFC 7541): header compression for HTTP/2. The decoder keeps the peer's dynamic table and
//! hands out fields whose values share the header block's bytes (Huffman-coded ones are decoded into
//! one buffer per block); the encoder indexes only headers whose values repeat (a response's type,
//! its date, ...), so its table stays small enough to search by scanning.

pub(crate) mod codes;
pub(crate) mod huffman;

use std::collections::VecDeque;

use bytes::{BufMut as _, Bytes, BytesMut};
use http::StatusCode;
use http::header::{self, HeaderName, HeaderValue};

/// A field's name: a pseudo-header, a header, or bytes that are neither (kept, so the table stays in
/// step with the peer's, and refused when a message is built from them).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Name {
    Authority,
    Method,
    Path,
    Scheme,
    Status,
    Protocol,
    Header(HeaderName),
    Invalid(Bytes),
}

impl Name {
    fn len(&self) -> usize {
        match self {
            Name::Authority => 10,
            Name::Method | Name::Scheme | Name::Status => 7,
            Name::Path => 5,
            Name::Protocol => 9,
            Name::Header(h) => h.as_str().len(),
            Name::Invalid(b) => b.len(),
        }
    }

    fn parse(raw: Bytes) -> Name {
        match &raw[..] {
            b":authority" => Name::Authority,
            b":method" => Name::Method,
            b":path" => Name::Path,
            b":scheme" => Name::Scheme,
            b":status" => Name::Status,
            b":protocol" => Name::Protocol,
            _ => match HeaderName::from_lowercase(&raw) {
                Ok(h) if !raw.starts_with(b":") => Name::Header(h),
                _ => Name::Invalid(raw),
            },
        }
    }
}

/// A decoded field.
#[derive(Clone, Debug)]
pub(crate) struct Field {
    pub name: Name,
    pub value: Bytes,
}

impl Field {
    fn size(&self) -> usize {
        self.name.len() + self.value.len() + 32
    }
}

/// Why a header block couldn't be decoded: a connection error (COMPRESSION_ERROR).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DecodeError;

fn st(name: Name, value: &'static str) -> Field {
    Field { name, value: Bytes::from_static(value.as_bytes()) }
}

fn sh(name: HeaderName, value: &'static str) -> Field {
    st(Name::Header(name), value)
}

/// An entry of the static table (RFC 7541 Appendix A), 1-based.
fn static_field(i: usize) -> Option<Field> {
    Some(match i {
        1 => st(Name::Authority, ""),
        2 => st(Name::Method, "GET"),
        3 => st(Name::Method, "POST"),
        4 => st(Name::Path, "/"),
        5 => st(Name::Path, "/index.html"),
        6 => st(Name::Scheme, "http"),
        7 => st(Name::Scheme, "https"),
        8 => st(Name::Status, "200"),
        9 => st(Name::Status, "204"),
        10 => st(Name::Status, "206"),
        11 => st(Name::Status, "304"),
        12 => st(Name::Status, "400"),
        13 => st(Name::Status, "404"),
        14 => st(Name::Status, "500"),
        15 => sh(header::ACCEPT_CHARSET, ""),
        16 => sh(header::ACCEPT_ENCODING, "gzip, deflate"),
        17 => sh(header::ACCEPT_LANGUAGE, ""),
        18 => sh(header::ACCEPT_RANGES, ""),
        19 => sh(header::ACCEPT, ""),
        20 => sh(header::ACCESS_CONTROL_ALLOW_ORIGIN, ""),
        21 => sh(header::AGE, ""),
        22 => sh(header::ALLOW, ""),
        23 => sh(header::AUTHORIZATION, ""),
        24 => sh(header::CACHE_CONTROL, ""),
        25 => sh(header::CONTENT_DISPOSITION, ""),
        26 => sh(header::CONTENT_ENCODING, ""),
        27 => sh(header::CONTENT_LANGUAGE, ""),
        28 => sh(header::CONTENT_LENGTH, ""),
        29 => sh(header::CONTENT_LOCATION, ""),
        30 => sh(header::CONTENT_RANGE, ""),
        31 => sh(header::CONTENT_TYPE, ""),
        32 => sh(header::COOKIE, ""),
        33 => sh(header::DATE, ""),
        34 => sh(header::ETAG, ""),
        35 => sh(header::EXPECT, ""),
        36 => sh(header::EXPIRES, ""),
        37 => sh(header::FROM, ""),
        38 => sh(header::HOST, ""),
        39 => sh(header::IF_MATCH, ""),
        40 => sh(header::IF_MODIFIED_SINCE, ""),
        41 => sh(header::IF_NONE_MATCH, ""),
        42 => sh(header::IF_RANGE, ""),
        43 => sh(header::IF_UNMODIFIED_SINCE, ""),
        44 => sh(header::LAST_MODIFIED, ""),
        45 => sh(header::LINK, ""),
        46 => sh(header::LOCATION, ""),
        47 => sh(header::MAX_FORWARDS, ""),
        48 => sh(header::PROXY_AUTHENTICATE, ""),
        49 => sh(header::PROXY_AUTHORIZATION, ""),
        50 => sh(header::RANGE, ""),
        51 => sh(header::REFERER, ""),
        52 => sh(header::REFRESH, ""),
        53 => sh(header::RETRY_AFTER, ""),
        54 => sh(header::SERVER, ""),
        55 => sh(header::SET_COOKIE, ""),
        56 => sh(header::STRICT_TRANSPORT_SECURITY, ""),
        57 => sh(header::TRANSFER_ENCODING, ""),
        58 => sh(header::USER_AGENT, ""),
        59 => sh(header::VARY, ""),
        60 => sh(header::VIA, ""),
        61 => sh(header::WWW_AUTHENTICATE, ""),
        _ => return None,
    })
}

const STATIC_LEN: usize = 61;

/// The static table's index of a header's name, if it has one.
fn static_name(name: &HeaderName) -> Option<usize> {
    Some(match name.as_str() {
        "accept-charset" => 15,
        "accept-encoding" => 16,
        "accept-language" => 17,
        "accept-ranges" => 18,
        "accept" => 19,
        "access-control-allow-origin" => 20,
        "age" => 21,
        "allow" => 22,
        "authorization" => 23,
        "cache-control" => 24,
        "content-disposition" => 25,
        "content-encoding" => 26,
        "content-language" => 27,
        "content-length" => 28,
        "content-location" => 29,
        "content-range" => 30,
        "content-type" => 31,
        "cookie" => 32,
        "date" => 33,
        "etag" => 34,
        "expect" => 35,
        "expires" => 36,
        "from" => 37,
        "host" => 38,
        "if-match" => 39,
        "if-modified-since" => 40,
        "if-none-match" => 41,
        "if-range" => 42,
        "if-unmodified-since" => 43,
        "last-modified" => 44,
        "link" => 45,
        "location" => 46,
        "max-forwards" => 47,
        "proxy-authenticate" => 48,
        "proxy-authorization" => 49,
        "range" => 50,
        "referer" => 51,
        "refresh" => 52,
        "retry-after" => 53,
        "server" => 54,
        "set-cookie" => 55,
        "strict-transport-security" => 56,
        "transfer-encoding" => 57,
        "user-agent" => 58,
        "vary" => 59,
        "via" => 60,
        "www-authenticate" => 61,
        _ => return None,
    })
}

// ------------------------------------------------------------------------------------------ integers

/// Reads an integer with an `n`-bit prefix (RFC 7541 §5.1) at `*pos`.
fn read_int(block: &[u8], pos: &mut usize, n: u32) -> Result<usize, DecodeError> {
    let mask = (1usize << n) - 1;
    let first = usize::from(*block.get(*pos).ok_or(DecodeError)?);
    *pos += 1;
    let mut value = first & mask;
    if value < mask {
        return Ok(value);
    }
    let mut shift = 0u32;
    loop {
        let b = usize::from(*block.get(*pos).ok_or(DecodeError)?);
        *pos += 1;
        // Nothing larger than 2^28 is ever needed (and more would overflow on the way).
        if shift > 21 {
            return Err(DecodeError);
        }
        value += (b & 0x7f) << shift;
        shift += 7;
        if b & 0x80 == 0 {
            return Ok(value);
        }
    }
}

/// Writes `value` with an `n`-bit prefix whose other bits are `flags`.
pub(crate) fn put_int(out: &mut Vec<u8>, flags: u8, n: u32, mut value: usize) {
    let mask = (1usize << n) - 1;
    if value < mask {
        out.push(flags | value as u8);
        return;
    }
    out.push(flags | mask as u8);
    value -= mask;
    while value >= 0x80 {
        out.push((value & 0x7f) as u8 | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

/// Writes a raw (not Huffman-coded) string.
fn put_str(out: &mut Vec<u8>, s: &[u8]) {
    put_int(out, 0, 7, s.len());
    out.extend_from_slice(s);
}

// ------------------------------------------------------------------------------------------ decoding

/// The decoder: the peer's dynamic table, as its header blocks build it.
pub(crate) struct Decoder {
    table: VecDeque<Field>,
    size: usize,
    /// The table's current bound (the peer moves it, up to `limit`).
    max: usize,
    /// What we told the peer the table may hold (SETTINGS_HEADER_TABLE_SIZE).
    limit: usize,
}

impl Decoder {
    pub(crate) fn new(limit: usize) -> Decoder {
        Decoder { table: VecDeque::new(), size: 0, max: limit, limit }
    }

    fn get(&self, i: usize) -> Result<Field, DecodeError> {
        if i == 0 {
            return Err(DecodeError);
        }
        if i <= STATIC_LEN {
            return static_field(i).ok_or(DecodeError);
        }
        self.table.get(i - STATIC_LEN - 1).cloned().ok_or(DecodeError)
    }

    fn evict_to(&mut self, max: usize) {
        while self.size > max {
            match self.table.pop_back() {
                Some(f) => self.size -= f.size(),
                None => {
                    self.size = 0;
                    break;
                }
            }
        }
    }

    fn insert(&mut self, f: Field) {
        let size = f.size();
        if size > self.max {
            // Larger than the whole table: it empties it (RFC 7541 §4.4).
            self.table.clear();
            self.size = 0;
            return;
        }
        self.evict_to(self.max - size);
        self.size += size;
        self.table.push_front(f);
    }

    /// Reads a string at `*pos`: a slice of the block when raw, decoded into `scratch` when Huffman-coded.
    fn string(block: &Bytes, pos: &mut usize, scratch: &mut BytesMut) -> Result<Bytes, DecodeError> {
        let huffman = block.get(*pos).ok_or(DecodeError)? & 0x80 != 0;
        let len = read_int(block, pos, 7)?;
        let end = pos.checked_add(len).filter(|e| *e <= block.len()).ok_or(DecodeError)?;
        let raw = block.slice(*pos..end);
        *pos = end;
        if !huffman {
            return Ok(raw);
        }
        let mut out = Vec::with_capacity(len * 8 / 5 + 1);
        huffman::decode(&raw, &mut out).map_err(|_| DecodeError)?;
        scratch.put_slice(&out);
        Ok(scratch.split().freeze())
    }

    /// Decodes a header block onto `out`. Fields past `max_list` (the header list's size, RFC 9113
    /// §6.5.2) are decoded, to keep the table in step, but not kept: `Ok(false)` says some were dropped.
    pub(crate) fn decode(&mut self, block: &Bytes, max_list: usize, out: &mut Vec<Field>) -> Result<bool, DecodeError> {
        let mut pos = 0usize;
        let mut scratch = BytesMut::new();
        let mut list = 0usize;
        let mut whole = true;
        let mut at_start = true;
        while let Some(&b) = block.get(pos) {
            let field = if b & 0x80 != 0 {
                let i = read_int(block, &mut pos, 7)?;
                self.get(i)?
            } else if b & 0xe0 == 0x20 {
                // A table size update: only at the start of a block (RFC 7541 §4.2), within our limit.
                if !at_start {
                    return Err(DecodeError);
                }
                let max = read_int(block, &mut pos, 5)?;
                if max > self.limit {
                    return Err(DecodeError);
                }
                self.max = max;
                self.evict_to(max);
                continue;
            } else {
                let (n, indexing) = if b & 0xc0 == 0x40 { (6, true) } else { (4, false) };
                let i = read_int(block, &mut pos, n)?;
                let name = if i == 0 { Name::parse(Self::string(block, &mut pos, &mut scratch)?) } else { self.get(i)?.name };
                let value = Self::string(block, &mut pos, &mut scratch)?;
                let f = Field { name, value };
                if indexing {
                    self.insert(f.clone());
                }
                f
            };
            at_start = false;
            list += field.size();
            if list > max_list {
                whole = false;
            } else {
                out.push(field);
            }
        }
        Ok(whole)
    }
}

// ------------------------------------------------------------------------------------------ encoding

/// Headers whose values tend to repeat from message to message: worth a place in the table.
fn worth_indexing(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "content-type"
            | "date"
            | "server"
            | "cache-control"
            | "vary"
            | "content-encoding"
            | "access-control-allow-origin"
            | "access-control-allow-credentials"
            | "x-content-type-options"
            | "x-frame-options"
            | "strict-transport-security"
            | "referrer-policy"
            | "content-security-policy"
            | "accept"
            | "accept-encoding"
            | "accept-language"
            | "user-agent"
            | "host"
    )
}

/// Headers never put in a table, by either side (RFC 7541 §7.1.3).
fn sensitive(name: &HeaderName) -> bool {
    matches!(name.as_str(), "authorization" | "proxy-authorization" | "cookie" | "set-cookie")
}

/// The encoder: our dynamic table, as the peer's decoder rebuilds it.
pub(crate) struct Encoder {
    table: VecDeque<(HeaderName, HeaderValue)>,
    size: usize,
    max: usize,
    /// A table size the peer set, to say at the start of the next block.
    update: Option<usize>,
}

impl Encoder {
    pub(crate) fn new() -> Encoder {
        Encoder { table: VecDeque::new(), size: 0, max: 4096, update: None }
    }

    /// The peer set its table's size (SETTINGS_HEADER_TABLE_SIZE).
    pub(crate) fn set_max(&mut self, max: usize) {
        // Our table needn't be as large as allowed: 4 KiB covers what's worth indexing.
        let max = max.min(4096);
        if max != self.max {
            self.max = max;
            self.update = Some(max);
            self.evict_to(max);
        }
    }

    fn evict_to(&mut self, max: usize) {
        while self.size > max {
            match self.table.pop_back() {
                Some((n, v)) => self.size -= n.as_str().len() + v.len() + 32,
                None => {
                    self.size = 0;
                    break;
                }
            }
        }
    }

    /// Starts a header block (saying a table size change first, if there is one).
    pub(crate) fn begin(&mut self, out: &mut Vec<u8>) {
        if let Some(max) = self.update.take() {
            put_int(out, 0x20, 5, max);
        }
    }

    pub(crate) fn status(&mut self, status: StatusCode, out: &mut Vec<u8>) {
        let i = match status.as_u16() {
            200 => 8,
            204 => 9,
            206 => 10,
            304 => 11,
            400 => 12,
            404 => 13,
            500 => 14,
            _ => {
                // :status's name (index 8), the value as it is, not indexed.
                put_int(out, 0x00, 4, 8);
                put_str(out, status.as_str().as_bytes());
                return;
            }
        };
        put_int(out, 0x80, 7, i);
    }

    /// A pseudo-header of a request: `name` its static index, `value` as it is.
    pub(crate) fn pseudo(&mut self, index: usize, value: &[u8], out: &mut Vec<u8>) {
        match (index, value) {
            (2, b"GET") | (4, b"/") | (6, b"http") | (7, b"https") => put_int(out, 0x80, 7, index),
            (2, b"POST") => put_int(out, 0x80, 7, 3),
            _ => {
                put_int(out, 0x00, 4, index);
                put_str(out, value);
            }
        }
    }

    /// [`Encoder::header`] for a value given as bytes: made a header value only when it's new to the table.
    pub(crate) fn header_bytes(&mut self, name: &HeaderName, value: &[u8], out: &mut Vec<u8>) {
        if self.max > 0
            && let Some(i) = self.table.iter().position(|(n, v)| n == name && v.as_bytes() == value)
        {
            put_int(out, 0x80, 7, STATIC_LEN + 1 + i);
            return;
        }
        match HeaderValue::from_bytes(value) {
            Ok(v) => self.header(name, &v, out),
            Err(_) => {
                put_int(out, 0x00, 4, static_name(name).unwrap_or(0));
                if static_name(name).is_none() {
                    put_str(out, name.as_str().as_bytes());
                }
                put_str(out, value);
            }
        }
    }

    pub(crate) fn header(&mut self, name: &HeaderName, value: &HeaderValue, out: &mut Vec<u8>) {
        let stat = static_name(name);
        if sensitive(name) {
            put_int(out, 0x10, 4, stat.unwrap_or(0));
            if stat.is_none() {
                put_str(out, name.as_str().as_bytes());
            }
            put_str(out, value.as_bytes());
            return;
        }
        if worth_indexing(name) && self.max > 0 {
            if let Some(i) = self.table.iter().position(|(n, v)| n == name && v == value) {
                put_int(out, 0x80, 7, STATIC_LEN + 1 + i);
                return;
            }
            let size = name.as_str().len() + value.len() + 32;
            if size <= self.max {
                put_int(out, 0x40, 6, stat.unwrap_or(0));
                if stat.is_none() {
                    put_str(out, name.as_str().as_bytes());
                }
                put_str(out, value.as_bytes());
                self.evict_to(self.max - size);
                self.size += size;
                self.table.push_front((name.clone(), value.clone()));
                return;
            }
        }
        put_int(out, 0x00, 4, stat.unwrap_or(0));
        if stat.is_none() {
            put_str(out, name.as_str().as_bytes());
        }
        put_str(out, value.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_all(d: &mut Decoder, block: &[u8]) -> Result<Vec<(String, String)>, DecodeError> {
        let mut out = Vec::new();
        d.decode(&Bytes::copy_from_slice(block), 64 << 10, &mut out)?;
        Ok(out
            .into_iter()
            .map(|f| {
                let name = match f.name {
                    Name::Authority => ":authority".to_owned(),
                    Name::Method => ":method".to_owned(),
                    Name::Path => ":path".to_owned(),
                    Name::Scheme => ":scheme".to_owned(),
                    Name::Status => ":status".to_owned(),
                    Name::Protocol => ":protocol".to_owned(),
                    Name::Header(h) => h.as_str().to_owned(),
                    Name::Invalid(b) => String::from_utf8_lossy(&b).into_owned(),
                };
                (name, String::from_utf8_lossy(&f.value).into_owned())
            })
            .collect())
    }

    fn pairs(p: &[(&str, &str)]) -> Vec<(String, String)> {
        p.iter().map(|(a, b)| ((*a).to_owned(), (*b).to_owned())).collect()
    }

    /// RFC 7541 C.4: three requests with Huffman coding, the table carried between them.
    #[test]
    fn the_rfcs_requests_decode() -> Result<(), DecodeError> {
        let mut d = Decoder::new(4096);
        let c41 = [0x82, 0x86, 0x84, 0x41, 0x8c, 0xf1, 0xe3, 0xc2, 0xe5, 0xf2, 0x3a, 0x6b, 0xa0, 0xab, 0x90, 0xf4, 0xff];
        assert_eq!(decode_all(&mut d, &c41)?, pairs(&[(":method", "GET"), (":scheme", "http"), (":path", "/"), (":authority", "www.example.com")]));
        let c42 = [0x82, 0x86, 0x84, 0xbe, 0x58, 0x86, 0xa8, 0xeb, 0x10, 0x64, 0x9c, 0xbf];
        assert_eq!(decode_all(&mut d, &c42)?, pairs(&[(":method", "GET"), (":scheme", "http"), (":path", "/"), (":authority", "www.example.com"), ("cache-control", "no-cache")]));
        let c43 = [0x82, 0x87, 0x85, 0xbf, 0x40, 0x88, 0x25, 0xa8, 0x49, 0xe9, 0x5b, 0xa9, 0x7d, 0x7f, 0x89, 0x25, 0xa8, 0x49, 0xe9, 0x5b, 0xb8, 0xe8, 0xb4, 0xbf];
        assert_eq!(decode_all(&mut d, &c43)?, pairs(&[(":method", "GET"), (":scheme", "https"), (":path", "/index.html"), (":authority", "www.example.com"), ("custom-key", "custom-value")]));
        assert_eq!(d.size, 164);
        Ok(())
    }

    #[test]
    fn integers_round_trip_and_overlong_ones_are_refused() -> Result<(), DecodeError> {
        for (n, v) in [(5u32, 10usize), (5, 1337), (8, 42), (7, 127), (4, 15), (6, 1 << 20)] {
            let mut out = Vec::new();
            put_int(&mut out, 0, n, v);
            let mut pos = 0;
            assert_eq!(read_int(&out, &mut pos, n)?, v);
            assert_eq!(pos, out.len());
        }
        let mut pos = 0;
        assert!(read_int(&[0x1f, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01], &mut pos, 5).is_err());
        Ok(())
    }

    #[test]
    fn what_the_encoder_writes_the_decoder_reads() -> Result<(), DecodeError> {
        let mut e = Encoder::new();
        let mut d = Decoder::new(4096);
        for round in 0..3 {
            let mut out = Vec::new();
            e.begin(&mut out);
            e.status(StatusCode::OK, &mut out);
            e.header(&header::CONTENT_TYPE, &HeaderValue::from_static("application/json"), &mut out);
            e.header(&header::CONTENT_LENGTH, &HeaderValue::from_static("42"), &mut out);
            e.header(&header::SET_COOKIE, &HeaderValue::from_static("a=b"), &mut out);
            e.header(&HeaderName::from_static("x-trace"), &HeaderValue::from_static("t1"), &mut out);
            let got = decode_all(&mut d, &out)?;
            assert_eq!(got, pairs(&[(":status", "200"), ("content-type", "application/json"), ("content-length", "42"), ("set-cookie", "a=b"), ("x-trace", "t1")]), "round {round}");
            if round > 0 {
                // The type is indexed now: one byte.
                assert!(out.len() < 30, "{}", out.len());
            }
        }
        // A smaller table, said at the start of the next block.
        e.set_max(0);
        let mut out = Vec::new();
        e.begin(&mut out);
        e.header(&header::CONTENT_TYPE, &HeaderValue::from_static("text/plain"), &mut out);
        assert_eq!(out.first(), Some(&0x20));
        assert_eq!(decode_all(&mut d, &out)?, pairs(&[("content-type", "text/plain")]));
        Ok(())
    }

    #[test]
    fn bad_blocks_are_refused() {
        let mut d = Decoder::new(4096);
        // Index 0, an index past the table, a size update past our limit, one after a field.
        for bad in [&[0x80u8][..], &[0xff, 0x7f], &[0x3f, 0xe2, 0x1f], &[0x82, 0x20]] {
            assert!(decode_all(&mut d, bad).is_err(), "{bad:?}");
        }
    }
}
