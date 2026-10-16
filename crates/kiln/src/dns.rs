//! The slice of the DNS wire format (RFC 1035) that multicast DNS service discovery needs
//! (RFC 6762, RFC 6763): messages with questions and A, AAAA, PTR, SRV and TXT records.
//!
//! Everything read here comes from the network, so decoding is defensive: every read is bounds
//! checked, names are capped at 255 bytes and labels at 63, compression pointers must point
//! backwards and may be followed at most [`MAX_JUMPS`] times, and a message may hold at most
//! [`MAX_RECORDS`] records. A malformed message is an [`Error`], never a panic.
//!
//! Encoding compresses names (RFC 1035 §4.1.4), so a full service announcement fits easily in
//! one datagram.

use std::collections::HashMap;
use std::fmt;
use std::net::{Ipv4Addr, Ipv6Addr};

pub const TYPE_A: u16 = 1;
pub const TYPE_PTR: u16 = 12;
pub const TYPE_TXT: u16 = 16;
pub const TYPE_AAAA: u16 = 28;
pub const TYPE_SRV: u16 = 33;
pub const TYPE_ANY: u16 = 255;
pub const CLASS_IN: u16 = 1;
/// The class field's top bit: "cache flush" on a record, "unicast response wanted" on a question.
pub const CLASS_TOP: u16 = 0x8000;

/// Header flag: this message is a response.
pub const FLAG_RESPONSE: u16 = 0x8000;
/// Header flag: authoritative answer (every mDNS response sets it).
pub const FLAG_AUTHORITATIVE: u16 = 0x0400;
/// Header flag: truncated; more known answers follow in another packet.
pub const FLAG_TRUNCATED: u16 = 0x0200;

/// The largest message kiln sends or reads (RFC 6762 §17).
pub const MAX_MESSAGE: usize = 9000;
/// Most records (all sections together) one message may carry.
pub const MAX_RECORDS: usize = 256;
/// Most compression pointers followed while reading one name.
pub const MAX_JUMPS: usize = 32;
const MAX_NAME: usize = 255;
const MAX_LABEL: usize = 63;
/// Most strings one TXT record may carry.
const MAX_TXT_STRINGS: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The message ends before a field it announces.
    Truncated,
    /// A label longer than 63 bytes, or an empty label inside a name.
    BadLabel,
    /// A name longer than 255 bytes.
    NameTooLong,
    /// A compression pointer that does not point backwards, or too many of them.
    BadPointer,
    /// More records than [`MAX_RECORDS`].
    TooManyRecords,
    /// Record data that does not fit the length it declares.
    BadRdata,
    /// A message larger than [`MAX_MESSAGE`] (encoding).
    TooLarge,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Truncated => "message truncated",
            Self::BadLabel => "bad label",
            Self::NameTooLong => "name longer than 255 bytes",
            Self::BadPointer => "bad compression pointer",
            Self::TooManyRecords => "too many records",
            Self::BadRdata => "record data does not match its length",
            Self::TooLarge => "message too large",
        })
    }
}

impl std::error::Error for Error {}

/// A domain name as its labels. Labels are compared ASCII case-insensitively, as DNS requires;
/// they may hold any UTF-8 (DNS-SD instance names do), including dots and spaces.
#[derive(Clone, Debug, Default)]
pub struct Name(Vec<String>);

impl Name {
    pub fn from_labels<I, S>(labels: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self(labels.into_iter().map(Into::into).collect())
    }

    /// A dotted name whose labels contain no dots (`_kiln._tcp.local`); a trailing dot is fine.
    pub fn parse(dotted: &str) -> Self {
        Self(dotted.split('.').filter(|l| !l.is_empty()).map(str::to_owned).collect())
    }

    /// `label` prepended to `parent` (an instance name under a service type).
    pub fn child(label: &str, parent: &Name) -> Self {
        let mut labels = Vec::with_capacity(parent.0.len() + 1);
        labels.push(label.to_owned());
        labels.extend(parent.0.iter().cloned());
        Self(labels)
    }

    pub fn labels(&self) -> &[String] {
        &self.0
    }

    pub fn first(&self) -> Option<&str> {
        self.0.first().map(String::as_str)
    }

    /// The name without its first label.
    pub fn parent(&self) -> Name {
        Self(self.0.iter().skip(1).cloned().collect())
    }

    /// Bytes on the wire, without compression.
    pub fn wire_len(&self) -> usize {
        self.0.iter().map(|l| l.len() + 1).sum::<usize>() + 1
    }
}

impl PartialEq for Name {
    fn eq(&self, other: &Self) -> bool {
        self.0.len() == other.0.len() && self.0.iter().zip(&other.0).all(|(a, b)| a.eq_ignore_ascii_case(b))
    }
}

impl Eq for Name {}

impl fmt::Display for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for l in &self.0 {
            write!(f, "{l}.")?;
        }
        if self.0.is_empty() {
            f.write_str(".")?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Question {
    pub name: Name,
    pub qtype: u16,
    pub qclass: u16,
    /// The QU bit: the asker wants a unicast reply (RFC 6762 §5.4).
    pub unicast: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RData {
    A(Ipv4Addr),
    Aaaa(Ipv6Addr),
    Ptr(Name),
    Srv { priority: u16, weight: u16, port: u16, target: Name },
    /// The record's strings, each at most 255 bytes.
    Txt(Vec<Vec<u8>>),
    /// A type kiln does not interpret, kept verbatim.
    Other { rtype: u16, data: Vec<u8> },
}

impl RData {
    pub fn rtype(&self) -> u16 {
        match self {
            Self::A(_) => TYPE_A,
            Self::Aaaa(_) => TYPE_AAAA,
            Self::Ptr(_) => TYPE_PTR,
            Self::Srv { .. } => TYPE_SRV,
            Self::Txt(_) => TYPE_TXT,
            Self::Other { rtype, .. } => *rtype,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    pub name: Name,
    /// The class without its top bit.
    pub class: u16,
    /// The cache-flush bit: this record replaces every other of its name and type (RFC 6762 §10.2).
    pub cache_flush: bool,
    pub ttl: u32,
    pub data: RData,
}

impl Record {
    pub fn new(name: Name, ttl: u32, cache_flush: bool, data: RData) -> Self {
        Self { name, class: CLASS_IN, cache_flush, ttl, data }
    }

    pub fn rtype(&self) -> u16 {
        self.data.rtype()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Message {
    pub id: u16,
    pub flags: u16,
    pub questions: Vec<Question>,
    pub answers: Vec<Record>,
    pub authorities: Vec<Record>,
    pub additionals: Vec<Record>,
}

impl Message {
    pub fn is_response(&self) -> bool {
        self.flags & FLAG_RESPONSE != 0
    }

    /// Answers and additional records (where mDNS responders put what they announce).
    pub fn records(&self) -> impl Iterator<Item = &Record> {
        self.answers.iter().chain(&self.additionals)
    }

    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        let mut w = Writer::default();
        w.u16(self.id);
        w.u16(self.flags);
        for n in [self.questions.len(), self.answers.len(), self.authorities.len(), self.additionals.len()] {
            w.u16(u16::try_from(n).map_err(|_| Error::TooManyRecords)?);
        }
        for q in &self.questions {
            w.name(&q.name)?;
            w.u16(q.qtype);
            w.u16(q.qclass | if q.unicast { CLASS_TOP } else { 0 });
        }
        for r in self.answers.iter().chain(&self.authorities).chain(&self.additionals) {
            w.record(r)?;
        }
        if w.buf.len() > MAX_MESSAGE {
            return Err(Error::TooLarge);
        }
        Ok(w.buf)
    }

    pub fn decode(buf: &[u8]) -> Result<Self, Error> {
        let mut r = Reader { buf, pos: 0 };
        let id = r.u16()?;
        let flags = r.u16()?;
        let qd = usize::from(r.u16()?);
        let an = usize::from(r.u16()?);
        let ns = usize::from(r.u16()?);
        let ar = usize::from(r.u16()?);
        if qd + an + ns + ar > MAX_RECORDS {
            return Err(Error::TooManyRecords);
        }
        // Each question takes at least 5 bytes and each record at least 11: a count the message
        // cannot hold is refused before anything is allocated for it.
        if qd * 5 + (an + ns + ar) * 11 > buf.len().saturating_sub(12) {
            return Err(Error::Truncated);
        }
        let mut questions = Vec::with_capacity(qd);
        for _ in 0..qd {
            let name = r.name()?;
            let qtype = r.u16()?;
            let class = r.u16()?;
            questions.push(Question { name, qtype, qclass: class & !CLASS_TOP, unicast: class & CLASS_TOP != 0 });
        }
        let mut section = |n: usize| -> Result<Vec<Record>, Error> {
            let mut out = Vec::with_capacity(n);
            for _ in 0..n {
                out.push(r.record()?);
            }
            Ok(out)
        };
        let answers = section(an)?;
        let authorities = section(ns)?;
        let additionals = section(ar)?;
        Ok(Self { id, flags, questions, answers, authorities, additionals })
    }
}

// ------------------------------------------------------------------------------------ reading

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], Error> {
        let end = self.pos.checked_add(n).ok_or(Error::Truncated)?;
        let s = self.buf.get(self.pos..end).ok_or(Error::Truncated)?;
        self.pos = end;
        Ok(s)
    }

    fn u8(&mut self) -> Result<u8, Error> {
        let b = *self.buf.get(self.pos).ok_or(Error::Truncated)?;
        self.pos += 1;
        Ok(b)
    }

    fn u16(&mut self) -> Result<u16, Error> {
        let s = self.take(2)?;
        match s {
            [a, b] => Ok(u16::from_be_bytes([*a, *b])),
            _ => Err(Error::Truncated),
        }
    }

    fn u32(&mut self) -> Result<u32, Error> {
        let s = self.take(4)?;
        match s {
            [a, b, c, d] => Ok(u32::from_be_bytes([*a, *b, *c, *d])),
            _ => Err(Error::Truncated),
        }
    }

    fn name(&mut self) -> Result<Name, Error> {
        let (name, end) = read_name(self.buf, self.pos)?;
        self.pos = end;
        Ok(name)
    }

    fn record(&mut self) -> Result<Record, Error> {
        let name = self.name()?;
        let rtype = self.u16()?;
        let class = self.u16()?;
        let ttl = self.u32()?;
        let len = usize::from(self.u16()?);
        let start = self.pos;
        let end = start.checked_add(len).ok_or(Error::Truncated)?;
        if end > self.buf.len() {
            return Err(Error::Truncated);
        }
        let data = match rtype {
            TYPE_A => match self.take(len)? {
                [a, b, c, d] => RData::A(Ipv4Addr::new(*a, *b, *c, *d)),
                _ => return Err(Error::BadRdata),
            },
            TYPE_AAAA => {
                let bytes: [u8; 16] = self.take(len)?.try_into().map_err(|_| Error::BadRdata)?;
                RData::Aaaa(Ipv6Addr::from(bytes))
            }
            TYPE_PTR => RData::Ptr(self.name()?),
            TYPE_SRV => {
                let priority = self.u16()?;
                let weight = self.u16()?;
                let port = self.u16()?;
                RData::Srv { priority, weight, port, target: self.name()? }
            }
            TYPE_TXT => {
                let mut strings = Vec::new();
                while self.pos < end {
                    let n = usize::from(self.u8()?);
                    if self.pos + n > end || strings.len() >= MAX_TXT_STRINGS {
                        return Err(Error::BadRdata);
                    }
                    strings.push(self.take(n)?.to_vec());
                }
                RData::Txt(strings)
            }
            _ => RData::Other { rtype, data: self.take(len)?.to_vec() },
        };
        // A name inside the data may not run past the declared length (it may point backwards
        // out of it, which compression allows).
        if self.pos != end {
            return Err(Error::BadRdata);
        }
        Ok(Record { name, class: class & !CLASS_TOP, cache_flush: class & CLASS_TOP != 0, ttl, data })
    }
}

/// Read the name at `start`. Returns it and the position just after it in the stream (after the
/// first compression pointer, if it used one).
fn read_name(buf: &[u8], start: usize) -> Result<(Name, usize), Error> {
    let mut labels = Vec::new();
    let mut pos = start;
    let mut end = None;
    let mut jumps = 0usize;
    let mut wire = 1usize;
    loop {
        let len = *buf.get(pos).ok_or(Error::Truncated)?;
        match len & 0xC0 {
            0x00 if len == 0 => {
                pos += 1;
                break;
            }
            0x00 => {
                let n = usize::from(len);
                let label = buf.get(pos + 1..pos + 1 + n).ok_or(Error::Truncated)?;
                wire += n + 1;
                if wire > MAX_NAME {
                    return Err(Error::NameTooLong);
                }
                labels.push(String::from_utf8_lossy(label).into_owned());
                pos += 1 + n;
            }
            0xC0 => {
                let low = *buf.get(pos + 1).ok_or(Error::Truncated)?;
                let target = (usize::from(len & 0x3F) << 8) | usize::from(low);
                if end.is_none() {
                    end = Some(pos + 2);
                }
                jumps += 1;
                // Strictly backwards, and a bounded number of times: no loops, no forward jumps.
                if target >= pos || jumps > MAX_JUMPS {
                    return Err(Error::BadPointer);
                }
                pos = target;
            }
            // 0x40 and 0x80 label types are obsolete or reserved.
            _ => return Err(Error::BadLabel),
        }
    }
    Ok((Name(labels), end.unwrap_or(pos)))
}

// ------------------------------------------------------------------------------------ writing

#[derive(Default)]
struct Writer {
    buf: Vec<u8>,
    /// Where each name suffix was first written, for compression.
    suffixes: HashMap<Vec<String>, u16>,
}

impl Writer {
    fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    fn name(&mut self, name: &Name) -> Result<(), Error> {
        if name.wire_len() > MAX_NAME {
            return Err(Error::NameTooLong);
        }
        let labels = name.labels();
        for i in 0..labels.len() {
            let key: Vec<String> = labels.iter().skip(i).map(|l| l.to_ascii_lowercase()).collect();
            if let Some(&at) = self.suffixes.get(&key) {
                self.u16(0xC000 | at);
                return Ok(());
            }
            if let Ok(at) = u16::try_from(self.buf.len())
                && at < 0x3FFF
            {
                self.suffixes.insert(key, at);
            }
            let label = labels.get(i).ok_or(Error::BadLabel)?.as_bytes();
            if label.is_empty() || label.len() > MAX_LABEL {
                return Err(Error::BadLabel);
            }
            self.buf.push(u8::try_from(label.len()).map_err(|_| Error::BadLabel)?);
            self.buf.extend_from_slice(label);
        }
        self.buf.push(0);
        Ok(())
    }

    fn record(&mut self, r: &Record) -> Result<(), Error> {
        self.name(&r.name)?;
        self.u16(r.rtype());
        self.u16(r.class | if r.cache_flush { CLASS_TOP } else { 0 });
        self.u32(r.ttl);
        let len_at = self.buf.len();
        self.u16(0);
        match &r.data {
            RData::A(ip) => self.buf.extend_from_slice(&ip.octets()),
            RData::Aaaa(ip) => self.buf.extend_from_slice(&ip.octets()),
            RData::Ptr(n) => self.name(n)?,
            RData::Srv { priority, weight, port, target } => {
                self.u16(*priority);
                self.u16(*weight);
                self.u16(*port);
                self.name(target)?;
            }
            RData::Txt(strings) => {
                // An empty TXT record still holds one (empty) string (RFC 6763 §6.1).
                if strings.is_empty() {
                    self.buf.push(0);
                }
                for s in strings {
                    self.buf.push(u8::try_from(s.len()).map_err(|_| Error::BadRdata)?);
                    self.buf.extend_from_slice(s);
                }
            }
            RData::Other { data, .. } => self.buf.extend_from_slice(data),
        }
        let len = u16::try_from(self.buf.len() - len_at - 2).map_err(|_| Error::BadRdata)?;
        let bytes = len.to_be_bytes();
        if let Some(slot) = self.buf.get_mut(len_at..len_at + 2) {
            slot.copy_from_slice(&bytes);
        }
        Ok(())
    }
}

// ------------------------------------------------------------------------------------ TXT

/// `key=value` strings for a TXT record. Keys and values longer than a TXT string allows are cut.
pub fn txt(pairs: &[(&str, &str)]) -> Vec<Vec<u8>> {
    pairs
        .iter()
        .map(|(k, v)| {
            let mut s = format!("{k}={v}").into_bytes();
            s.truncate(255);
            s
        })
        .collect()
}

/// A TXT record's `key=value` pairs (RFC 6763 §6): keys lowercased, the first occurrence of a key
/// wins, strings without `=` and keys that are empty or longer than 64 bytes are skipped.
pub fn txt_pairs(strings: &[Vec<u8>]) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for s in strings.iter().take(MAX_TXT_STRINGS) {
        let Some(eq) = s.iter().position(|b| *b == b'=') else { continue };
        let (k, v) = (s.get(..eq).unwrap_or_default(), s.get(eq + 1..).unwrap_or_default());
        if k.is_empty() || k.len() > 64 {
            continue;
        }
        let key = String::from_utf8_lossy(k).to_ascii_lowercase();
        if out.iter().any(|(have, _)| *have == key) {
            continue;
        }
        out.push((key, String::from_utf8_lossy(v).into_owned()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service() -> Name {
        Name::parse("_kiln._tcp.local.")
    }

    fn announcement() -> Message {
        let instance = Name::child("kiln server on box (1a2b3c4d)", &service());
        let host = Name::parse("kiln-1a2b3c4d.local");
        Message {
            id: 0,
            flags: FLAG_RESPONSE | FLAG_AUTHORITATIVE,
            questions: vec![],
            answers: vec![
                Record::new(service(), 120, false, RData::Ptr(instance.clone())),
                Record::new(instance.clone(), 120, true, RData::Srv { priority: 0, weight: 0, port: 7070, target: host.clone() }),
                Record::new(instance, 120, true, RData::Txt(txt(&[("id", "1a2b3c4d"), ("role", "server"), ("quic", "7071")]))),
            ],
            authorities: vec![],
            additionals: vec![
                Record::new(host.clone(), 120, true, RData::A(Ipv4Addr::new(192, 168, 1, 77))),
                Record::new(host, 120, true, RData::Aaaa("fe80::1".parse().unwrap_or(Ipv6Addr::UNSPECIFIED))),
            ],
        }
    }

    #[test]
    fn round_trips() -> Result<(), Error> {
        let m = announcement();
        let bytes = m.encode()?;
        assert_eq!(Message::decode(&bytes)?, m);
        // Compression: the service and host names are written once.
        let plain: usize = m.records().map(|r| r.name.wire_len()).sum();
        assert!(bytes.len() < plain + 100, "{} bytes", bytes.len());

        let q = Message {
            id: 7,
            flags: 0,
            questions: vec![Question { name: service(), qtype: TYPE_PTR, qclass: CLASS_IN, unicast: true }],
            answers: vec![Record::new(service(), 60, false, RData::Ptr(Name::child("x", &service())))],
            ..Message::default()
        };
        let back = Message::decode(&q.encode()?)?;
        assert_eq!(back, q);
        assert!(back.questions.first().is_some_and(|q| q.unicast));
        assert!(!back.is_response());

        // Unknown types and empty TXT records survive.
        let o = Message {
            answers: vec![
                Record::new(Name::parse("a.local"), 1, false, RData::Other { rtype: 47, data: vec![1, 2, 3] }),
                Record::new(Name::parse("b.local"), 1, false, RData::Txt(vec![])),
            ],
            ..Message::default()
        };
        let back = Message::decode(&o.encode()?)?;
        assert_eq!(back.answers.first(), o.answers.first());
        assert_eq!(back.answers.get(1).map(|r| &r.data), Some(&RData::Txt(vec![vec![]])));
        Ok(())
    }

    #[test]
    fn names_compare_case_insensitively() {
        assert_eq!(Name::parse("_KILN._tcp.Local."), service());
        assert_ne!(Name::parse("_kiln._udp.local"), service());
        assert_eq!(service().to_string(), "_kiln._tcp.local.");
        assert_eq!(Name::child("a.b", &service()).parent(), service());
    }

    #[test]
    fn txt_pairs_are_parsed_defensively() {
        let strings = vec![
            b"ID=abc".to_vec(),
            b"id=second".to_vec(),
            b"novalue".to_vec(),
            b"=empty-key".to_vec(),
            b"name=a=b".to_vec(),
            vec![b'k'; 65].into_iter().chain(*b"=v").collect(),
            b"bin=\xff\xfe".to_vec(),
        ];
        let p = txt_pairs(&strings);
        assert_eq!(p.first(), Some(&("id".to_owned(), "abc".to_owned())));
        assert!(p.contains(&("name".to_owned(), "a=b".to_owned())));
        assert_eq!(p.len(), 3);
        let long = txt(&[("k", &"v".repeat(400))]);
        assert_eq!(long.first().map(Vec::len), Some(255));
    }

    #[test]
    fn truncated_messages_are_errors() -> Result<(), Error> {
        let bytes = announcement().encode()?;
        for n in 0..bytes.len() {
            assert!(Message::decode(bytes.get(..n).unwrap_or_default()).is_err(), "prefix of {n} bytes decoded");
        }
        Ok(())
    }

    fn header(qd: u16, an: u16) -> Vec<u8> {
        let mut b = vec![0, 0, 0x84, 0];
        b.extend_from_slice(&qd.to_be_bytes());
        b.extend_from_slice(&an.to_be_bytes());
        b.extend_from_slice(&[0, 0, 0, 0]);
        b
    }

    #[test]
    fn compression_loops_and_forward_pointers_are_refused() {
        // A question whose name points at itself.
        let mut m = header(1, 0);
        m.extend_from_slice(&[0xC0, 12, 0, 1, 0, 1]);
        assert_eq!(Message::decode(&m), Err(Error::BadPointer));
        // Two labels that point at each other: "a" then a pointer back to the start.
        let mut m = header(1, 0);
        m.extend_from_slice(&[1, b'a', 0xC0, 12, 0, 1, 0, 1]);
        assert!(matches!(Message::decode(&m), Err(Error::BadPointer | Error::NameTooLong)));
        // A forward pointer.
        let mut m = header(1, 0);
        m.extend_from_slice(&[0xC0, 20, 0, 1, 0, 1, 0, 0, 1, b'a', 0]);
        assert_eq!(Message::decode(&m), Err(Error::BadPointer));
        // A pointer past the end.
        let mut m = header(1, 0);
        m.extend_from_slice(&[0xC0]);
        assert_eq!(Message::decode(&m), Err(Error::Truncated));
        // Reserved label types.
        let mut m = header(1, 0);
        m.extend_from_slice(&[0x41, 0, 0, 1, 0, 1]);
        assert_eq!(Message::decode(&m), Err(Error::BadLabel));
        // A long chain of backward pointers is cut off; a short one is fine.
        assert_eq!(Message::decode(&pointer_chain(MAX_JUMPS + 8)), Err(Error::BadPointer));
        assert!(Message::decode(&pointer_chain(8)).is_ok());
    }

    /// A record whose (uninterpreted) data holds a chain of `links` pointers, each to the one
    /// before, ending at a root name; then an A record named by a pointer to the chain's end.
    fn pointer_chain(links: usize) -> Vec<u8> {
        let be = |n: usize| (0xC000 | u16::try_from(n).unwrap_or(0)).to_be_bytes();
        let mut m = header(0, 2);
        m.extend_from_slice(&[0, 0, 99, 0, 1, 0, 0, 0, 0]);
        m.extend_from_slice(&u16::try_from(1 + 2 * links).unwrap_or(0).to_be_bytes());
        let mut prev = m.len();
        m.push(0);
        for _ in 0..links {
            let here = m.len();
            m.extend_from_slice(&be(prev));
            prev = here;
        }
        m.extend_from_slice(&be(prev));
        m.extend_from_slice(&[0, 1, 0, 1, 0, 0, 0, 0, 0, 4, 10, 0, 0, 1]);
        m
    }

    #[test]
    fn oversized_names_and_counts_are_refused() -> Result<(), Error> {
        // A 64-byte label cannot be written, and a 300-byte name cannot either.
        let long_label = Message {
            questions: vec![Question { name: Name::from_labels(["x".repeat(64)]), qtype: TYPE_A, qclass: CLASS_IN, unicast: false }],
            ..Message::default()
        };
        assert_eq!(long_label.encode(), Err(Error::BadLabel));
        let long_name = Message {
            questions: vec![Question { name: Name::from_labels(vec!["y".repeat(60); 5]), qtype: TYPE_A, qclass: CLASS_IN, unicast: false }],
            ..Message::default()
        };
        assert_eq!(long_name.encode(), Err(Error::NameTooLong));
        // On the wire: a name made of five 60-byte labels.
        let mut m = header(1, 0);
        for _ in 0..5 {
            m.push(60);
            m.extend(std::iter::repeat_n(b'y', 60));
        }
        m.extend_from_slice(&[0, 0, 1, 0, 1]);
        assert_eq!(Message::decode(&m), Err(Error::NameTooLong));
        // Counts the message cannot hold.
        assert_eq!(Message::decode(&header(0, 60_000)), Err(Error::TooManyRecords));
        assert_eq!(Message::decode(&header(3, 3)), Err(Error::Truncated));
        // rdlength beyond the message, and TXT strings beyond rdlength.
        let mut m = header(0, 1);
        m.extend_from_slice(&[0, 0, 16, 0, 1, 0, 0, 0, 10, 0x10, 0x00, 3, b'a']);
        assert_eq!(Message::decode(&m), Err(Error::Truncated));
        let mut m = header(0, 1);
        m.extend_from_slice(&[0, 0, 16, 0, 1, 0, 0, 0, 10, 0, 3, 5, b'a', b'b']);
        assert_eq!(Message::decode(&m), Err(Error::BadRdata));
        // An A record with the wrong length; a PTR whose name overruns its rdlength.
        let mut m = header(0, 1);
        m.extend_from_slice(&[0, 0, 1, 0, 1, 0, 0, 0, 10, 0, 3, 1, 2, 3]);
        assert_eq!(Message::decode(&m), Err(Error::BadRdata));
        let mut m = header(0, 1);
        m.extend_from_slice(&[0, 0, 12, 0, 1, 0, 0, 0, 10, 0, 1, 1, b'a', 0]);
        assert_eq!(Message::decode(&m), Err(Error::BadRdata));
        // A too-large message cannot be encoded.
        let big = Message {
            answers: (0..40)
                .map(|i| Record::new(Name::parse(&format!("r{i}.local")), 1, false, RData::Txt(vec![vec![b'z'; 250]])))
                .collect(),
            ..Message::default()
        };
        assert_eq!(big.encode(), Err(Error::TooLarge));
        Ok(())
    }

    /// Random mutations of a valid message never panic (they decode or fail cleanly).
    #[test]
    fn mutated_messages_never_panic() -> Result<(), Error> {
        let base = announcement().encode()?;
        let mut state = 0x2545_F491_4F6C_DD1D_u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..20_000 {
            let mut m = base.clone();
            let edits = 1 + next() % 6;
            for _ in 0..edits {
                let len = u64::try_from(m.len()).unwrap_or(1).max(1);
                let at = usize::try_from(next() % len).unwrap_or(0);
                match next() % 4 {
                    0 => {
                        if let Some(b) = m.get_mut(at) {
                            *b = u8::try_from(next() & 0xff).unwrap_or(0);
                        }
                    }
                    1 => m.truncate(at),
                    2 => m.insert(at.min(m.len()), 0xC0),
                    _ => {
                        if let Some(b) = m.get_mut(at) {
                            *b ^= 0x40;
                        }
                    }
                }
            }
            if let Ok(msg) = Message::decode(&m) {
                // Whatever decodes, re-encodes (or fails cleanly) too.
                let _ = msg.encode();
            }
        }
        Ok(())
    }
}
