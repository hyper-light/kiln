//! Kafka's protocol, as a consumer speaks it (`docs/observe.md` §10.3): enough to read a topic's
//! partitions from offsets kiln keeps, of any broker from Kafka 1.0 on and of Azure Event Hubs'
//! Kafka endpoint ("Kafka protocol guide", kafka.apache.org).
//!
//! - `ApiVersions` (v0) first, so a broker that does not answer what kiln asks is said plainly;
//!   then SASL (`SaslHandshake` v1, `SaslAuthenticate` v0): PLAIN (RFC 4616) or OAUTHBEARER (RFC
//!   7628's initial response); `Metadata` v1; `ListOffsets` v1 (an offset by time); `Fetch` v4,
//!   reading uncommitted (Kafka's consumers' default), from offsets the caller keeps.
//! - Record batches of magic 2 (Kafka 0.11 on; Kafka 4 reads no other), each one's CRC-32C
//!   checked, gzip'd or not (Event Hubs compresses with gzip alone); snappy, lz4 and zstd are said
//!   and not read. Control batches (transactions' markers) are passed over; a batch cut short at
//!   the end of an answer is fetched again from its offset.
//! - Every length a broker sends is checked against what is there: a malformed answer is an error,
//!   never a panic, nor an allocation it asks for.

use std::collections::{BTreeMap, HashMap};
use std::io::Read as _;
use std::sync::Arc;
use std::time::Duration;

use futures::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};

use crate::http::tls::ClientTls;

/// The APIs kiln calls, by key, and the version it asks of each.
pub const FETCH: (i16, i16) = (1, 4);
pub const LIST_OFFSETS: (i16, i16) = (2, 1);
pub const METADATA: (i16, i16) = (3, 1);
pub const SASL_HANDSHAKE: (i16, i16) = (17, 1);
pub const API_VERSIONS: (i16, i16) = (18, 0);
pub const SASL_AUTHENTICATE: (i16, i16) = (36, 0);

/// An answer's bytes at the most: a fetch's [`FETCH_MAX_BYTES`] and its framing, with room.
const ANSWER_MAX: usize = 64 << 20;
/// What a fetch asks for at the most, all partitions together and each (Kafka's consumers'
/// defaults: `fetch.max.bytes` 50 MiB, `max.partition.fetch.bytes` 1 MiB).
pub const FETCH_MAX_BYTES: i32 = 16 << 20;
pub const PARTITION_MAX_BYTES: i32 = 1 << 20;
/// How long a broker holds a fetch with nothing to send (Kafka's consumers: `fetch.max.wait.ms`
/// 500).
pub const FETCH_WAIT_MS: i32 = 500;
/// A batch's records unpacked at the most (a broker's `message.max.bytes` is 1 MiB unless raised;
/// Event Hubs' events are 1 MB at the most): a gzip bomb stops here.
const UNPACKED_MAX: usize = 32 << 20;
/// What a call is given beyond the broker's own wait.
const CALL_TIMEOUT: Duration = Duration::from_secs(30);
/// A record batch's header, up to its records (magic 2): the offsets of its fields.
const BATCH_HEAD: usize = 61;
/// `ListOffsets`' times that mean the earliest and the latest offset.
pub const EARLIEST: i64 = -2;
pub const LATEST: i64 = -1;

// --------------------------------------------------------------------------------------- errors

/// What went wrong reading an answer, or what a broker said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KafkaError {
    /// The answer does not read as the protocol says.
    Malformed(&'static str),
    /// Something kiln does not read (a compression, an old message format).
    Unread(String),
    /// A broker's error code, and of what.
    Code(i16, String),
}

impl std::fmt::Display for KafkaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed(m) => write!(f, "a malformed answer: {m}"),
            Self::Unread(m) => f.write_str(m),
            Self::Code(c, of) => write!(f, "{of}: {} ({c})", code_name(*c)),
        }
    }
}

impl std::error::Error for KafkaError {}

/// The protocol's name of an error code, for what kiln says.
pub fn code_name(code: i16) -> &'static str {
    match code {
        -1 => "UNKNOWN_SERVER_ERROR",
        1 => "OFFSET_OUT_OF_RANGE",
        2 => "CORRUPT_MESSAGE",
        3 => "UNKNOWN_TOPIC_OR_PARTITION",
        5 => "LEADER_NOT_AVAILABLE",
        6 => "NOT_LEADER_OR_FOLLOWER",
        7 => "REQUEST_TIMED_OUT",
        9 => "REPLICA_NOT_AVAILABLE",
        13 => "NETWORK_EXCEPTION",
        29 => "TOPIC_AUTHORIZATION_FAILED",
        31 => "CLUSTER_AUTHORIZATION_FAILED",
        33 => "UNSUPPORTED_SASL_MECHANISM",
        34 => "ILLEGAL_SASL_STATE",
        35 => "UNSUPPORTED_VERSION",
        58 => "SASL_AUTHENTICATION_FAILED",
        74 => "FENCED_LEADER_EPOCH",
        75 => "UNKNOWN_LEADER_EPOCH",
        76 => "UNSUPPORTED_COMPRESSION_TYPE",
        _ => "an error",
    }
}

/// Whether a partition's error passes once the brokers' metadata is read again (its leader moved,
/// or is being chosen).
pub fn moved(code: i16) -> bool {
    matches!(code, 3 | 5 | 6 | 7 | 9 | 13 | 74 | 75)
}

// ------------------------------------------------------------------------------------------ wire

/// A request's body, written big-endian as the protocol has it.
#[derive(Debug, Default)]
pub struct Out(pub Vec<u8>);

impl Out {
    pub fn i8(&mut self, v: i8) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }

    pub fn i16(&mut self, v: i16) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }

    pub fn i32(&mut self, v: i32) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }

    pub fn i64(&mut self, v: i64) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }

    /// A `STRING`: its length (`INT16`), then its bytes; a name longer than that is refused where
    /// it is given, so none is cut here but at the bound.
    pub fn string(&mut self, s: &str) {
        let b = s.as_bytes().get(..s.len().min(i16::MAX as usize)).unwrap_or_default();
        self.i16(i16::try_from(b.len()).unwrap_or(i16::MAX));
        self.0.extend_from_slice(b);
    }

    /// `BYTES`: its length (`INT32`), then them.
    pub fn bytes(&mut self, b: &[u8]) {
        self.len(b.len());
        self.0.extend_from_slice(b);
    }

    /// An array's length.
    pub fn len(&mut self, n: usize) {
        self.i32(i32::try_from(n).unwrap_or(i32::MAX));
    }
}

/// An answer, read in place.
#[derive(Debug, Clone, Copy)]
pub struct In<'a>(pub &'a [u8]);

impl<'a> In<'a> {
    pub fn take(&mut self, n: usize) -> Result<&'a [u8], KafkaError> {
        let (a, b) = self.0.split_at_checked(n).ok_or(KafkaError::Malformed("it ended inside a field"))?;
        self.0 = b;
        Ok(a)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], KafkaError> {
        let (a, b) = self.0.split_first_chunk::<N>().ok_or(KafkaError::Malformed("it ended inside a number"))?;
        self.0 = b;
        Ok(*a)
    }

    pub fn i8(&mut self) -> Result<i8, KafkaError> {
        Ok(i8::from_be_bytes(self.array()?))
    }

    pub fn i16(&mut self) -> Result<i16, KafkaError> {
        Ok(i16::from_be_bytes(self.array()?))
    }

    pub fn i32(&mut self) -> Result<i32, KafkaError> {
        Ok(i32::from_be_bytes(self.array()?))
    }

    pub fn i64(&mut self) -> Result<i64, KafkaError> {
        Ok(i64::from_be_bytes(self.array()?))
    }

    /// A `STRING` or a `NULLABLE_STRING` (null: none).
    pub fn string(&mut self) -> Result<Option<&'a str>, KafkaError> {
        let n = self.i16()?;
        let Ok(n) = usize::try_from(n) else { return Ok(None) };
        std::str::from_utf8(self.take(n)?).map(Some).map_err(|_| KafkaError::Malformed("a string not UTF-8"))
    }

    /// `BYTES` or `NULLABLE_BYTES` (null: none).
    pub fn bytes(&mut self) -> Result<Option<&'a [u8]>, KafkaError> {
        let n = self.i32()?;
        let Ok(n) = usize::try_from(n) else { return Ok(None) };
        self.take(n).map(Some)
    }

    /// An array's length (a null array: none), no longer than what is left could hold at `each`
    /// bytes an element.
    pub fn count(&mut self, each: usize) -> Result<usize, KafkaError> {
        let n = self.i32()?;
        let Ok(n) = usize::try_from(n) else { return Ok(0) };
        if n.saturating_mul(each.max(1)) > self.0.len() {
            return Err(KafkaError::Malformed("an array longer than the answer"));
        }
        Ok(n)
    }

    /// A `VARLONG`: protobuf's zigzag varint, ten bytes at the most.
    pub fn varlong(&mut self) -> Result<i64, KafkaError> {
        let mut v: u64 = 0;
        for i in 0..10 {
            let [b] = self.array::<1>()?;
            v |= u64::from(b & 0x7f) << (7 * i);
            if b & 0x80 == 0 {
                return Ok(((v >> 1) as i64) ^ -((v & 1) as i64));
            }
        }
        Err(KafkaError::Malformed("a varint of more than ten bytes"))
    }

    /// A `VARINT`.
    pub fn varint(&mut self) -> Result<i32, KafkaError> {
        i32::try_from(self.varlong()?).map_err(|_| KafkaError::Malformed("a varint past 32 bits"))
    }

    /// A length as a record has them (a `VARINT`; -1: null).
    fn var_bytes(&mut self) -> Result<Option<&'a [u8]>, KafkaError> {
        let n = self.varint()?;
        let Ok(n) = usize::try_from(n) else { return Ok(None) };
        self.take(n).map(Some)
    }
}

// ------------------------------------------------------------------------------------- requests

/// The versions a broker answers (`ApiVersions` v0), checked against what kiln asks: what it does
/// not answer, said.
pub fn read_api_versions(body: &[u8]) -> Result<Vec<String>, KafkaError> {
    let mut r = In(body);
    let code = r.i16()?;
    if code != 0 {
        return Err(KafkaError::Code(code, "ApiVersions".into()));
    }
    let mut known: HashMap<i16, (i16, i16)> = HashMap::new();
    for _ in 0..r.count(6)? {
        let (key, min, max) = (r.i16()?, r.i16()?, r.i16()?);
        known.insert(key, (min, max));
    }
    let asked = [(FETCH, "Fetch"), (LIST_OFFSETS, "ListOffsets"), (METADATA, "Metadata")];
    Ok(asked
        .iter()
        .filter_map(|&((key, v), name)| match known.get(&key) {
            Some(&(min, max)) if (min..=max).contains(&v) => None,
            Some(&(min, max)) => Some(format!("{name} v{v} (it answers v{min} to v{max})")),
            None => Some(format!("{name} (it does not answer it)")),
        })
        .collect())
}

/// `SaslHandshake` v1: the mechanism asked.
pub fn sasl_handshake_request(mechanism: &str) -> Vec<u8> {
    let mut o = Out::default();
    o.string(mechanism);
    o.0
}

pub fn read_sasl_handshake(body: &[u8], mechanism: &str) -> Result<(), KafkaError> {
    let mut r = In(body);
    let code = r.i16()?;
    if code == 0 {
        return Ok(());
    }
    let mut offered = Vec::new();
    for _ in 0..r.count(2)? {
        offered.push(r.string()?.unwrap_or_default().to_owned());
    }
    Err(KafkaError::Code(code, format!("SASL {mechanism} (the broker takes {})", offered.join(", "))))
}

/// `SaslAuthenticate` v0: the mechanism's message.
pub fn sasl_authenticate_request(auth: &[u8]) -> Vec<u8> {
    let mut o = Out::default();
    o.bytes(auth);
    o.0
}

pub fn read_sasl_authenticate(body: &[u8]) -> Result<(), KafkaError> {
    let mut r = In(body);
    let code = r.i16()?;
    if code == 0 {
        return Ok(());
    }
    let said = r.string()?.unwrap_or_default();
    Err(KafkaError::Code(code, if said.is_empty() { "SASL authentication".into() } else { format!("SASL authentication: {said}") }))
}

/// How kiln authenticates to a broker.
#[derive(Clone, PartialEq, Eq)]
pub enum Sasl {
    /// RFC 4616: Event Hubs' `$ConnectionString` and its connection string, or a user's.
    Plain { user: String, password: String },
    /// RFC 7628: an OAuth 2.0 bearer token (Microsoft Entra ID's, for Event Hubs).
    OAuthBearer { token: String },
}

impl std::fmt::Debug for Sasl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Plain { user, .. } => write!(f, "PLAIN {user}"),
            Self::OAuthBearer { .. } => f.write_str("OAUTHBEARER"),
        }
    }
}

impl Sasl {
    pub fn mechanism(&self) -> &'static str {
        match self {
            Self::Plain { .. } => "PLAIN",
            Self::OAuthBearer { .. } => "OAUTHBEARER",
        }
    }

    /// The client's message: PLAIN's `authzid NUL authcid NUL passwd` (no authzid), OAUTHBEARER's
    /// GS2 header and `auth` key-value pair.
    pub fn message(&self) -> Vec<u8> {
        match self {
            Self::Plain { user, password } => {
                let mut m = Vec::with_capacity(2 + user.len() + password.len());
                m.push(0);
                m.extend_from_slice(user.as_bytes());
                m.push(0);
                m.extend_from_slice(password.as_bytes());
                m
            }
            Self::OAuthBearer { token } => format!("n,,\u{1}auth=Bearer {token}\u{1}\u{1}").into_bytes(),
        }
    }
}

/// A topic's partitions as `Metadata` says them, and the brokers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Metadata {
    /// Each broker: its node id, host and port.
    pub brokers: Vec<(i32, String, u16)>,
    /// The topic's error (0: none).
    pub error: i16,
    /// Each partition: its id, its leader's node id and its error.
    pub partitions: Vec<(i32, i32, i16)>,
}

/// `Metadata` v1 of one topic.
pub fn metadata_request(topic: &str) -> Vec<u8> {
    let mut o = Out::default();
    o.len(1);
    o.string(topic);
    o.0
}

pub fn read_metadata(body: &[u8], topic: &str) -> Result<Metadata, KafkaError> {
    let mut r = In(body);
    let mut m = Metadata::default();
    for _ in 0..r.count(12)? {
        let node = r.i32()?;
        let host = r.string()?.unwrap_or_default().to_owned();
        let port = u16::try_from(r.i32()?).map_err(|_| KafkaError::Malformed("a broker's port"))?;
        let _rack = r.string()?;
        m.brokers.push((node, host, port));
    }
    let _controller = r.i32()?;
    let mut found = false;
    for _ in 0..r.count(9)? {
        let error = r.i16()?;
        let name = r.string()?.unwrap_or_default();
        let _internal = r.i8()?;
        let mine = name == topic;
        if mine {
            found = true;
            m.error = error;
        }
        for _ in 0..r.count(18)? {
            let (perr, id, leader) = (r.i16()?, r.i32()?, r.i32()?);
            for _ in 0..r.count(4)? {
                r.i32()?;
            }
            for _ in 0..r.count(4)? {
                r.i32()?;
            }
            if mine {
                m.partitions.push((id, leader, perr));
            }
        }
    }
    if !found {
        m.error = 3;
    }
    m.partitions.sort_unstable();
    Ok(m)
}

/// `ListOffsets` v1: each partition's offset at a time ([`EARLIEST`], [`LATEST`], or unix ms: the
/// first record at or after it).
pub fn list_offsets_request(topic: &str, at: &[(i32, i64)]) -> Vec<u8> {
    let mut o = Out::default();
    o.i32(-1);
    o.len(1);
    o.string(topic);
    o.len(at.len());
    for &(p, t) in at {
        o.i32(p);
        o.i64(t);
    }
    o.0
}

/// Each partition: its error and offset (-1: none at or after the time asked).
pub fn read_list_offsets(body: &[u8]) -> Result<Vec<(i32, i16, i64)>, KafkaError> {
    let mut r = In(body);
    let mut out = Vec::new();
    for _ in 0..r.count(6)? {
        let _name = r.string()?;
        for _ in 0..r.count(22)? {
            let (p, code, _ts, offset) = (r.i32()?, r.i16()?, r.i64()?, r.i64()?);
            out.push((p, code, offset));
        }
    }
    Ok(out)
}

/// `Fetch` v4: each partition from its offset.
pub fn fetch_request(topic: &str, from: &[(i32, i64)], wait_ms: i32) -> Vec<u8> {
    let mut o = Out::default();
    o.i32(-1);
    o.i32(wait_ms);
    o.i32(1);
    o.i32(FETCH_MAX_BYTES);
    // Uncommitted records too: Kafka's consumers' default (`isolation.level`).
    o.i8(0);
    o.len(1);
    o.string(topic);
    o.len(from.len());
    for &(p, offset) in from {
        o.i32(p);
        o.i64(offset);
        o.i32(PARTITION_MAX_BYTES);
    }
    o.0
}

/// A partition's part of a fetch's answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fetched<'a> {
    pub partition: i32,
    pub error: i16,
    /// The offset after the last record written to it: what is left to read is this less where
    /// the reader is.
    pub high_watermark: i64,
    /// Its record batches, the last perhaps cut short.
    pub records: &'a [u8],
}

/// A fetch's answer: the time the broker throttled it for, and each partition's part.
pub fn read_fetch(body: &[u8]) -> Result<(i32, Vec<Fetched<'_>>), KafkaError> {
    let mut r = In(body);
    let throttle = r.i32()?;
    let mut out = Vec::new();
    for _ in 0..r.count(6)? {
        let _topic = r.string()?;
        for _ in 0..r.count(30)? {
            let (partition, error, high_watermark, _last_stable) = (r.i32()?, r.i16()?, r.i64()?, r.i64()?);
            for _ in 0..r.count(16)? {
                r.i64()?;
                r.i64()?;
            }
            let records = r.bytes()?.unwrap_or_default();
            out.push(Fetched { partition, error, high_watermark, records });
        }
    }
    Ok((throttle, out))
}

// -------------------------------------------------------------------------------------- records

/// A record of a batch, read in place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Record<'a> {
    pub offset: i64,
    /// Unix ms: its producer's, or when the broker took it (a topic's `LogAppendTime`).
    pub ts_ms: i64,
    pub key: Option<&'a [u8]>,
    pub value: Option<&'a [u8]>,
    headers: (usize, &'a [u8]),
}

impl<'a> Record<'a> {
    /// Its headers, in order (a malformed one ends them).
    pub fn headers(&self) -> impl Iterator<Item = (&'a str, Option<&'a [u8]>)> + 'a {
        let mut r = In(self.headers.1);
        (0..self.headers.0).map_while(move |_| {
            let key = r.var_bytes().ok()??;
            let value = r.var_bytes().ok()?;
            Some((std::str::from_utf8(key).ok()?, value))
        })
    }
}

/// What reading a partition's batches came to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Batches {
    /// The offset after the last whole batch read: where the next fetch starts (none: no whole
    /// batch; fetch from where it was).
    pub next: Option<i64>,
    /// Records given to the reader.
    pub records: u64,
    /// Bytes the records took, unpacked.
    pub bytes: u64,
}

/// Reads a partition's record batches (a fetch's `records`), giving each record at or after `from`
/// to `each`; `scratch` holds a compressed batch's records unpacked (kept, to unpack the next into).
pub fn read_batches(data: &[u8], from: i64, scratch: &mut Vec<u8>, each: &mut dyn FnMut(Record<'_>)) -> Result<Batches, KafkaError> {
    let mut out = Batches::default();
    let mut rest = data;
    loop {
        let mut head = In(rest);
        let (Ok(base), Ok(len)) = (head.i64(), head.i32()) else { break };
        let Ok(len) = usize::try_from(len) else { return Err(KafkaError::Malformed("a record batch's length")) };
        // A batch cut short at the end of the answer: fetched again from its offset.
        let Some(batch) = rest.get(..12 + len) else { break };
        rest = rest.get(12 + len..).unwrap_or_default();
        if batch.len() < BATCH_HEAD {
            return Err(KafkaError::Malformed("a record batch shorter than its header"));
        }
        let mut h = In(batch.get(16..).unwrap_or_default());
        let magic = h.i8()?;
        if magic != 2 {
            return Err(KafkaError::Unread(format!("a record batch of magic {magic} (message format v{magic}): kiln reads magic 2, Kafka 0.11's on")));
        }
        let crc = u32::from_be_bytes(h.array()?);
        if kiln_obs::crc32c::checksum(batch.get(21..).unwrap_or_default()) != crc {
            return Err(KafkaError::Malformed("a record batch's CRC-32C does not match it"));
        }
        let attributes = h.i16()?;
        let last_delta = h.i32()?;
        let (base_ts, max_ts) = (h.i64()?, h.i64()?);
        let _producer = (h.i64()?, h.i16()?, h.i32()?);
        let count = h.i32()?;
        out.next = Some(base.saturating_add(i64::from(last_delta)).saturating_add(1));
        // Transactions' markers, not records.
        if attributes & 0x20 != 0 {
            continue;
        }
        let packed = batch.get(BATCH_HEAD..).unwrap_or_default();
        let records: &[u8] = match attributes & 7 {
            0 => packed,
            1 => {
                scratch.clear();
                let mut gz = flate2::read::MultiGzDecoder::new(packed).take(UNPACKED_MAX as u64 + 1);
                gz.read_to_end(scratch).map_err(|_| KafkaError::Malformed("a gzip'd record batch that does not unpack"))?;
                if scratch.len() > UNPACKED_MAX {
                    return Err(KafkaError::Unread(format!("a record batch past {UNPACKED_MAX} bytes unpacked")));
                }
                scratch
            }
            c => {
                let name = match c {
                    2 => "snappy",
                    3 => "lz4",
                    4 => "zstd",
                    _ => "an unknown compression",
                };
                return Err(KafkaError::Unread(format!("a record batch compressed with {name}: kiln reads none or gzip (the producer's compression.type)")));
            }
        };
        out.bytes += records.len() as u64;
        let mut r = In(records);
        for _ in 0..usize::try_from(count).unwrap_or(0) {
            let n = usize::try_from(r.varint()?).map_err(|_| KafkaError::Malformed("a record's length"))?;
            let mut b = In(r.take(n)?);
            let _attributes = b.i8()?;
            let ts_delta = b.varlong()?;
            let offset_delta = b.varint()?;
            let key = b.var_bytes()?;
            let value = b.var_bytes()?;
            let headers = usize::try_from(b.varint()?).unwrap_or(0);
            let offset = base.saturating_add(i64::from(offset_delta));
            if offset < from {
                continue;
            }
            // The topic's `LogAppendTime`: every record's time is the broker's, the batch's latest.
            let ts_ms = if attributes & 8 != 0 { max_ts } else { base_ts.saturating_add(ts_delta) };
            out.records += 1;
            each(Record { offset, ts_ms, key, value, headers: (headers, b.0) });
        }
    }
    Ok(out)
}

// ----------------------------------------------------------------------------------- connection

/// A connection's byte stream: TCP, or TLS over it.
trait Io: AsyncRead + AsyncWrite + Unpin + Send {}

impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

/// One connection to one broker, authenticated, a request at a time.
pub struct Conn {
    io: Box<dyn Io>,
    next: i32,
    client: String,
    pub broker: String,
}

impl Conn {
    /// Connects to `host:port` (TLS when `tls`, as Event Hubs requires), checks the versions it
    /// answers and authenticates with `sasl`.
    pub async fn open(host: &str, port: u16, tls: Option<&ClientTls>, sasl: Option<&Sasl>, client: &str) -> anyhow::Result<Self> {
        let tcp = crate::http::io::connect(host, port).await?;
        let io: Box<dyn Io> = match tls {
            Some(t) => Box::new(
                crate::http::tls::TlsStream::connect(crate::http::io::RtFuturesIo::new(tcp), t.config()?, crate::http::tls::server_name(host)?)
                    .await
                    .map_err(|e| anyhow::anyhow!("TLS to {host}:{port}: {e}"))?,
            ),
            None => Box::new(crate::http::io::RtFuturesIo::new(tcp)),
        };
        let mut c = Self { io, next: 1, client: client.to_owned(), broker: format!("{host}:{port}") };
        let mut answer = Vec::new();
        c.call(API_VERSIONS, &[], &mut answer, CALL_TIMEOUT).await?;
        let missing = read_api_versions(&answer)?;
        anyhow::ensure!(missing.is_empty(), "{} does not answer what kiln asks: {}", c.broker, missing.join("; "));
        if let Some(s) = sasl {
            c.call(SASL_HANDSHAKE, &sasl_handshake_request(s.mechanism()), &mut answer, CALL_TIMEOUT).await?;
            read_sasl_handshake(&answer, s.mechanism())?;
            c.call(SASL_AUTHENTICATE, &sasl_authenticate_request(&s.message()), &mut answer, CALL_TIMEOUT).await?;
            read_sasl_authenticate(&answer)?;
        }
        Ok(c)
    }

    /// One request and its answer's body (into `answer`), within `within`.
    pub async fn call(&mut self, (key, version): (i16, i16), body: &[u8], answer: &mut Vec<u8>, within: Duration) -> anyhow::Result<()> {
        let id = self.next;
        self.next = self.next.wrapping_add(1).max(1);
        let mut head = Out::default();
        head.i16(key);
        head.i16(version);
        head.i32(id);
        head.string(&self.client);
        let size = i32::try_from(head.0.len() + body.len()).map_err(|_| anyhow::anyhow!("a request past 2 GiB"))?;
        let mut frame = Vec::with_capacity(4 + head.0.len() + body.len());
        frame.extend_from_slice(&size.to_be_bytes());
        frame.extend_from_slice(&head.0);
        frame.extend_from_slice(body);
        let (io, buf) = (&mut self.io, &mut *answer);
        let exchange = async move {
            io.write_all(&frame).await?;
            io.flush().await?;
            let mut len = [0u8; 4];
            io.read_exact(&mut len).await?;
            let n = usize::try_from(i32::from_be_bytes(len)).unwrap_or(0);
            anyhow::ensure!((4..=ANSWER_MAX).contains(&n), "an answer of {n} bytes");
            buf.clear();
            buf.resize(n, 0);
            io.read_exact(buf).await?;
            anyhow::Ok(())
        };
        crate::rt::timeout(within, exchange).await.map_err(|_| anyhow::anyhow!("{} did not answer within {} s", self.broker, within.as_secs()))??;
        let got = i32::from_be_bytes(answer.first_chunk::<4>().copied().unwrap_or_default());
        anyhow::ensure!(got == id, "{} answered request {got}, not {id}", self.broker);
        answer.drain(..4);
        Ok(())
    }
}

// ------------------------------------------------------------------------------------- consumer

/// Where a topic is read from and how.
#[derive(Clone)]
pub struct Source {
    /// Brokers to ask first (`bootstrap.servers`): host and port.
    pub bootstrap: Vec<(String, u16)>,
    pub topic: String,
    pub tls: Option<Arc<ClientTls>>,
    pub sasl: Option<Sasl>,
    /// The client id brokers see.
    pub client: String,
}

/// A topic read partition by partition from offsets its caller keeps: a connection to each leader,
/// opened again when it fails.
pub struct Consumer {
    pub source: Source,
    brokers: HashMap<i32, (String, u16)>,
    /// Each partition's leader.
    pub leaders: BTreeMap<i32, i32>,
    conns: HashMap<i32, Conn>,
    answer: Vec<u8>,
    scratch: Vec<u8>,
}

/// A round of fetches: for each partition, the offset to fetch from next and what is left past it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Round {
    pub next: BTreeMap<i32, i64>,
    /// Records past the offset read to, by partition (the high watermark less it).
    pub lag: BTreeMap<i32, i64>,
    /// Partitions whose offset is gone (retention passed it, or the topic was made again).
    pub out_of_range: Vec<i32>,
    /// Partitions to ask the metadata again for.
    pub moved: Vec<i32>,
    pub records: u64,
    pub bytes: u64,
    /// The longest a broker throttled a fetch for.
    pub throttled_ms: i32,
}

impl Consumer {
    pub fn new(source: Source) -> Self {
        Self { source, brokers: HashMap::new(), leaders: BTreeMap::new(), conns: HashMap::new(), answer: Vec::new(), scratch: Vec::new() }
    }

    /// Reads the topic's partitions and their leaders, from a broker known or a bootstrap one.
    pub async fn refresh(&mut self) -> anyhow::Result<Vec<i32>> {
        let request = metadata_request(&self.source.topic);
        let mut last = None;
        let known: Vec<i32> = self.conns.keys().copied().collect();
        for node in known {
            let Some(c) = self.conns.get_mut(&node) else { continue };
            match c.call(METADATA, &request, &mut self.answer, CALL_TIMEOUT).await {
                Ok(()) => return self.take_metadata(),
                Err(e) => {
                    self.conns.remove(&node);
                    last = Some(e);
                }
            }
        }
        for (host, port) in self.source.bootstrap.clone() {
            match Conn::open(&host, port, self.source.tls.as_deref(), self.source.sasl.as_ref(), &self.source.client).await {
                Ok(mut c) => match c.call(METADATA, &request, &mut self.answer, CALL_TIMEOUT).await {
                    Ok(()) => {
                        let parts = self.take_metadata()?;
                        // Kept as the broker it is, when it says which.
                        if let Some(node) = self.brokers.iter().find(|(_, (h, p))| *h == host && *p == port).map(|(n, _)| *n) {
                            self.conns.insert(node, c);
                        }
                        return Ok(parts);
                    }
                    Err(e) => last = Some(e),
                },
                Err(e) => last = Some(e.context(format!("{host}:{port}"))),
            }
        }
        Err(last.unwrap_or_else(|| anyhow::anyhow!("no broker to ask")))
    }

    fn take_metadata(&mut self) -> anyhow::Result<Vec<i32>> {
        let m = read_metadata(&self.answer, &self.source.topic)?;
        if m.error != 0 {
            return Err(KafkaError::Code(m.error, format!("topic {}", self.source.topic)).into());
        }
        self.brokers = m.brokers.into_iter().map(|(n, h, p)| (n, (h, p))).collect();
        self.leaders = m.partitions.iter().map(|&(p, leader, _)| (p, leader)).collect();
        // A connection to a broker that is no longer one is closed.
        let brokers = &self.brokers;
        self.conns.retain(|n, _| brokers.contains_key(n));
        Ok(m.partitions.iter().map(|&(p, _, _)| p).collect())
    }

    /// The connection to `node`, opened if it is not.
    async fn conn(&mut self, node: i32) -> anyhow::Result<&mut Conn> {
        if !self.conns.contains_key(&node) {
            let (host, port) = self.brokers.get(&node).cloned().ok_or_else(|| anyhow::anyhow!("broker {node} is not in the metadata"))?;
            let c = Conn::open(&host, port, self.source.tls.as_deref(), self.source.sasl.as_ref(), &self.source.client).await?;
            self.conns.insert(node, c);
        }
        self.conns.get_mut(&node).ok_or_else(|| anyhow::anyhow!("broker {node}"))
    }

    /// Each partition's offset at `at` (unix ms, [`EARLIEST`] or [`LATEST`]), asked of its leader:
    /// -1 where there is no record at or after it.
    pub async fn offsets_at(&mut self, partitions: &[i32], at: i64) -> anyhow::Result<BTreeMap<i32, i64>> {
        let mut by_leader: BTreeMap<i32, Vec<(i32, i64)>> = BTreeMap::new();
        for &p in partitions {
            let leader = self.leaders.get(&p).copied().ok_or_else(|| anyhow::anyhow!("partition {p} has no leader"))?;
            by_leader.entry(leader).or_default().push((p, at));
        }
        let mut out = BTreeMap::new();
        for (leader, asked) in by_leader {
            let request = list_offsets_request(&self.source.topic, &asked);
            let mut answer = std::mem::take(&mut self.answer);
            let called = self.conn(leader).await?.call(LIST_OFFSETS, &request, &mut answer, CALL_TIMEOUT).await;
            self.answer = answer;
            if let Err(e) = called {
                self.conns.remove(&leader);
                return Err(e);
            }
            for (p, code, offset) in read_list_offsets(&self.answer)? {
                if code != 0 {
                    return Err(KafkaError::Code(code, format!("partition {p}'s offset")).into());
                }
                out.insert(p, offset);
            }
        }
        Ok(out)
    }

    /// One fetch of each leader's partitions from `from` (partition: offset), each record given to
    /// `each` with its partition; what the round came to. A leader that fails is dropped (its
    /// connection opened again next round) and its partitions are said to have moved.
    pub async fn fetch(&mut self, from: &BTreeMap<i32, i64>, each: &mut (dyn FnMut(i32, Record<'_>) + Send)) -> anyhow::Result<Round> {
        let mut round = Round { next: from.clone(), ..Round::default() };
        let mut by_leader: BTreeMap<i32, Vec<(i32, i64)>> = BTreeMap::new();
        for (&p, &offset) in from {
            match self.leaders.get(&p) {
                Some(&leader) if leader >= 0 => by_leader.entry(leader).or_default().push((p, offset)),
                _ => round.moved.push(p),
            }
        }
        let mut failed = None;
        for (leader, asked) in by_leader {
            let request = fetch_request(&self.source.topic, &asked, FETCH_WAIT_MS);
            let mut answer = std::mem::take(&mut self.answer);
            let wait = CALL_TIMEOUT + Duration::from_millis(u64::from(FETCH_WAIT_MS.unsigned_abs()));
            let called = match self.conn(leader).await {
                Ok(c) => c.call(FETCH, &request, &mut answer, wait).await,
                Err(e) => Err(e),
            };
            if let Err(e) = called {
                self.answer = answer;
                self.conns.remove(&leader);
                round.moved.extend(asked.iter().map(|(p, _)| *p));
                failed = Some(e);
                continue;
            }
            let (throttle, parts) = read_fetch(&answer)?;
            round.throttled_ms = round.throttled_ms.max(throttle);
            let mut refused = None;
            for part in parts {
                let Some(&start) = from.get(&part.partition) else { continue };
                match part.error {
                    0 => {}
                    1 => {
                        round.out_of_range.push(part.partition);
                        continue;
                    }
                    c if moved(c) => {
                        round.moved.push(part.partition);
                        continue;
                    }
                    c => {
                        refused = Some(KafkaError::Code(c, format!("fetching partition {}", part.partition)));
                        break;
                    }
                }
                let p = part.partition;
                let read = read_batches(part.records, start, &mut self.scratch, &mut |r| each(p, r))?;
                let next = read.next.map_or(start, |n| n.max(start));
                round.next.insert(p, next);
                round.lag.insert(p, part.high_watermark.saturating_sub(next).max(0));
                round.records += read.records;
                round.bytes += read.bytes;
            }
            self.answer = answer;
            if let Some(e) = refused {
                return Err(e.into());
            }
        }
        // Every leader failed: the caller waits before the next round.
        if let Some(e) = failed
            && round.records == 0
            && round.moved.len() == from.len()
        {
            return Err(e);
        }
        Ok(round)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn varint(out: &mut Vec<u8>, v: i64) {
        let mut z = ((v << 1) ^ (v >> 63)) as u64;
        loop {
            let b = (z & 0x7f) as u8;
            z >>= 7;
            if z == 0 {
                out.push(b);
                return;
            }
            out.push(b | 0x80);
        }
    }

    /// A record batch as a producer writes one (magic 2): `records` of (offset delta, time delta,
    /// key, value, headers), gzip'd when `gzip`.
    pub(crate) fn batch(base: i64, base_ts: i64, attributes: i16, records: &[(i32, i64, Option<&[u8]>, Option<&[u8]>, &[(&str, &[u8])])]) -> Vec<u8> {
        let mut recs = Vec::new();
        for &(od, td, key, value, headers) in records {
            let mut body = vec![0u8];
            varint(&mut body, td);
            varint(&mut body, i64::from(od));
            for part in [key, value] {
                match part {
                    Some(b) => {
                        varint(&mut body, b.len() as i64);
                        body.extend_from_slice(b);
                    }
                    None => varint(&mut body, -1),
                }
            }
            varint(&mut body, headers.len() as i64);
            for (k, v) in headers {
                varint(&mut body, k.len() as i64);
                body.extend_from_slice(k.as_bytes());
                varint(&mut body, v.len() as i64);
                body.extend_from_slice(v);
            }
            varint(&mut recs, body.len() as i64);
            recs.extend_from_slice(&body);
        }
        if attributes & 7 == 1 {
            use std::io::Write as _;
            let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            let _ = e.write_all(&recs);
            recs = e.finish().unwrap_or_default();
        }
        let last = records.iter().map(|r| r.0).max().unwrap_or(0);
        let max_ts = base_ts + records.iter().map(|r| r.1).max().unwrap_or(0);
        let mut tail = Out::default();
        tail.i16(attributes);
        tail.i32(last);
        tail.i64(base_ts);
        tail.i64(max_ts);
        tail.i64(-1);
        tail.i16(-1);
        tail.i32(-1);
        tail.len(records.len());
        tail.0.extend_from_slice(&recs);
        let mut b = Out::default();
        b.i64(base);
        b.i32(i32::try_from(4 + 1 + 4 + tail.0.len()).unwrap_or(0));
        b.i32(0);
        b.i8(2);
        b.0.extend_from_slice(&kiln_obs::crc32c::checksum(&tail.0).to_be_bytes());
        b.0.extend_from_slice(&tail.0);
        b.0
    }

    fn read_all(data: &[u8], from: i64) -> Result<(Batches, Vec<(i64, i64, Option<Vec<u8>>, Option<Vec<u8>>, Vec<(String, Vec<u8>)>)>), KafkaError> {
        let mut got = Vec::new();
        let mut scratch = Vec::new();
        let b = read_batches(data, from, &mut scratch, &mut |r| got.push((r.offset, r.ts_ms, r.key.map(<[u8]>::to_vec), r.value.map(<[u8]>::to_vec), r.headers().map(|(k, v)| (k.to_owned(), v.unwrap_or_default().to_vec())).collect())))?;
        Ok((b, got))
    }

    /// Batches plain and gzip'd read back record by record: their offsets, times, keys, values and
    /// headers; records before the offset asked passed over; a control batch passed over but its
    /// offsets counted; a batch cut short left for the next fetch.
    #[test]
    fn record_batches_read_back_as_they_were_written() -> Result<(), KafkaError> {
        let plain = batch(100, 1_800_000_000_000, 0, &[(0, 0, None, Some(b"a"), &[]), (1, 5, Some(b"k"), Some(b"b"), &[("h", b"v")]), (2, 9, None, None, &[])]);
        let gz = batch(103, 1_800_000_001_000, 1, &[(0, 0, None, Some(b"{\"records\":[]}"), &[]), (1, 1, None, Some(b"d"), &[])]);
        let control = batch(105, 1_800_000_002_000, 0x20, &[(0, 0, Some(b"\0\0\0\0"), Some(b"\0\0\0\0\0\0"), &[])]);
        let after = batch(106, 1_800_000_003_000, 0, &[(0, 0, None, Some(b"e"), &[])]);
        let mut all = [plain.clone(), gz.clone(), control, after.clone()].concat();
        let (b, got) = read_all(&all, 101)?;
        assert_eq!(b.next, Some(107));
        assert_eq!(got.iter().map(|g| g.0).collect::<Vec<_>>(), vec![101, 102, 103, 104, 106]);
        assert_eq!(got.first(), Some(&(101, 1_800_000_000_005, Some(b"k".to_vec()), Some(b"b".to_vec()), vec![("h".to_owned(), b"v".to_vec())])));
        assert_eq!(got.get(1).map(|g| g.3.clone()), Some(None), "a null value is none");
        assert_eq!(got.get(2).and_then(|g| g.3.as_deref()), Some(&b"{\"records\":[]}"[..]));
        // Cut short: the last batch is left whole for the next fetch.
        all.truncate(all.len() - 3);
        let (b, got) = read_all(&all, 100)?;
        assert_eq!((b.next, got.len()), (Some(106), 5));
        // A flipped bit is caught by the batch's CRC.
        let mut bad = plain.clone();
        if let Some(x) = bad.last_mut() {
            *x ^= 1;
        }
        assert_eq!(read_all(&bad, 0).err(), Some(KafkaError::Malformed("a record batch's CRC-32C does not match it")));
        // The broker's time when the topic keeps it: the batch's latest for every record.
        let appended = batch(0, 1_000, 8, &[(0, 0, None, Some(b"x"), &[]), (1, 7, None, Some(b"y"), &[])]);
        assert_eq!(read_all(&appended, 0)?.1.iter().map(|g| g.1).collect::<Vec<_>>(), vec![1_007, 1_007]);
        // Compressions kiln does not read are said, not misread.
        let snappy = batch(0, 0, 2, &[(0, 0, None, Some(b"x"), &[])]);
        assert!(matches!(read_all(&snappy, 0), Err(KafkaError::Unread(m)) if m.contains("snappy")));
        Ok(())
    }

    /// Answers as a broker writes them read back; lengths that claim more than there is are errors.
    #[test]
    fn answers_read_as_the_protocol_has_them() -> Result<(), KafkaError> {
        // Metadata v1: two brokers, the topic's three partitions, another topic's.
        let mut o = Out::default();
        o.len(2);
        for (node, host) in [(1, "b1"), (2, "b2")] {
            o.i32(node);
            o.string(host);
            o.i32(9093);
            o.i16(-1);
        }
        o.i32(1);
        o.len(2);
        for (name, parts) in [("other", 1), ("logs", 3)] {
            o.i16(0);
            o.string(name);
            o.i8(0);
            o.len(parts);
            for p in 0..parts {
                o.i16(if p == 2 { 5 } else { 0 });
                o.i32(i32::try_from(p).unwrap_or(0));
                o.i32(1 + i32::try_from(p % 2).unwrap_or(0));
                o.len(1);
                o.i32(1);
                o.len(0);
            }
        }
        let m = read_metadata(&o.0, "logs")?;
        assert_eq!(m.brokers, vec![(1, "b1".to_owned(), 9093), (2, "b2".to_owned(), 9093)]);
        assert_eq!(m.partitions, vec![(0, 1, 0), (1, 2, 0), (2, 1, 5)]);
        assert_eq!(read_metadata(&o.0, "nope")?.error, 3, "a topic not in the answer is unknown");
        // Fetch v4: a partition with records, one out of range.
        let records = batch(7, 1_000, 0, &[(0, 0, None, Some(b"v"), &[])]);
        let mut o = Out::default();
        o.i32(25);
        o.len(1);
        o.string("logs");
        o.len(2);
        o.i32(0);
        o.i16(0);
        o.i64(8);
        o.i64(8);
        o.len(0);
        o.bytes(&records);
        o.i32(1);
        o.i16(1);
        o.i64(0);
        o.i64(0);
        o.i32(-1);
        o.i32(-1);
        let (throttle, parts) = read_fetch(&o.0)?;
        assert_eq!(throttle, 25);
        assert_eq!(parts.first(), Some(&Fetched { partition: 0, error: 0, high_watermark: 8, records: &records }));
        assert_eq!(parts.get(1).map(|p| (p.error, p.records.len())), Some((1, 0)));
        // ApiVersions: what kiln asks, checked.
        let mut o = Out::default();
        o.i16(0);
        o.len(3);
        for (k, min, max) in [(1, 7, 13), (2, 0, 8), (3, 0, 12)] {
            o.i16(k);
            o.i16(min);
            o.i16(max);
        }
        assert_eq!(read_api_versions(&o.0)?, vec!["Fetch v4 (it answers v7 to v13)".to_owned()]);
        // An array that claims more than the answer holds.
        let mut o = Out::default();
        o.i32(1 << 30);
        assert_eq!(read_metadata(&o.0, "logs").err(), Some(KafkaError::Malformed("an array longer than the answer")));
        assert_eq!(read_fetch(&[0, 0]).err(), Some(KafkaError::Malformed("it ended inside a number")));
        Ok(())
    }

    #[test]
    fn sasl_messages_are_the_rfcs() {
        let plain = Sasl::Plain { user: "$ConnectionString".into(), password: "Endpoint=sb://ns.servicebus.windows.net/;SharedAccessKeyName=k;SharedAccessKey=s".into() };
        assert_eq!(plain.message(), b"\0$ConnectionString\0Endpoint=sb://ns.servicebus.windows.net/;SharedAccessKeyName=k;SharedAccessKey=s");
        assert_eq!(Sasl::OAuthBearer { token: "t0k".into() }.message(), b"n,,\x01auth=Bearer t0k\x01\x01");
        assert_eq!(format!("{plain:?}"), "PLAIN $ConnectionString", "the password never shown");
    }
}
