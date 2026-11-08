//! WebSocket compression: permessage-deflate (RFC 7692) for kiln's server sockets (W21-R15: the
//! events socket, agent sessions, terminals, logs: every socket `super::ws::accept` answers).
//!
//! * **Agreed in the handshake** ([`negotiate`]): a client that offers `permessage-deflate` gets it,
//!   with its parameters honoured; one that offers nothing (kiln's runners, its own client, scripts)
//!   gets exactly the frames it always had, through no layer at all.
//! * **Context takeover, server to client.** The compressor's window carries from message to message,
//!   so a message that repeats what came before (the next `harness` event, the same forty fields with
//!   two changed) costs a few bytes. T3 Code does the same (`apps/server/src/server.ts`: `ws`'s
//!   `perMessageDeflate` with context takeover, and so no size threshold: with the window shared, small
//!   messages compress too). A client that asks `server_no_context_takeover` gets a fresh window each
//!   message.
//! * **Every text and binary message** goes compressed, except a binary message that's gzip already
//!   (an agent session's packed snapshot, `z=gzip`), which goes as it is (RSV1 clear: RFC 7692 §6).
//! * **Messages that come compressed** (RSV1) are inflated as their frames arrive, fragments and all,
//!   with control frames between them passed on. One that inflates past the socket's message bound is
//!   handed to tungstenite as a frame head past that bound, so it fails as an oversized message always
//!   has (`Capacity`, and kiln's 1009 close): a decompression bomb costs at most the bound.
//!
//! It lies under tungstenite, as the connection it reads and writes ([`Socket`]): what goes up to
//! tungstenite is plain RFC 6455 frames, and what tungstenite writes is compressed on its way out.
//! A large message is compressed on the compute pool, in order, so it never holds the shard
//! (`super::compress::inline`).

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll, ready};

use flate2::{Compress, Decompress, FlushCompress, FlushDecompress, Status};
use futures::io::{AsyncRead, AsyncWrite};
use http::{HeaderMap, HeaderValue, header};

use super::compress::{Work, inline};

/// The extension's name.
const NAME: &str = "permessage-deflate";

/// What a deflate block ending in a sync flush ends with, which a message leaves off (RFC 7692
/// §7.2.1) and its reader puts back (§7.2.2).
const TAIL: [u8; 4] = [0x00, 0x00, 0xff, 0xff];

/// The zlib level for messages: zlib's own default (`Z_DEFAULT_COMPRESSION`, 6), which `ws` (T3 Code's
/// socket library) uses for permessage-deflate, and the knee measured on 500 `harness` events
/// (602,780 B; adacachy, release, 2026-11-04) with the window carried: level 1, 54,027 B at 2.6 µs a
/// message; 3, 32,350 B, 3.3 µs; 5, 31,541 B, 3.4 µs; 6, 28,271 B, 4.0 µs; 9, 28,271 B, 5.4 µs.
/// Without context takeover, level 6 sends 352,984 B (11.7 µs a message: each starts cold).
const LEVEL: u32 = 6;

/// The window both directions use: 15 bits (32 KiB), deflate's largest and RFC 7692's default.
/// kiln's deflate (flate2 on miniz_oxide) can't make it smaller, so an offer that asks the server to
/// (`server_max_window_bits` under 15) is declined, as §7.1.2.1 lets a server; the reader takes any
/// window up to 15, so the client's own may be whatever it says.
const WINDOW_BITS: u8 = 15;

/// The gzip magic number (RFC 1952 §2.3.1): a binary message that starts so is packed already.
const GZIP: [u8; 2] = [0x1f, 0x8b];

/// What was agreed with a client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Agreed {
    /// The client asked that each server message start from an empty window.
    pub server_no_context_takeover: bool,
}

impl Agreed {
    /// The `Sec-WebSocket-Extensions` to answer with.
    pub fn header(&self) -> HeaderValue {
        HeaderValue::from_static(if self.server_no_context_takeover { "permessage-deflate; server_no_context_takeover" } else { "permessage-deflate" })
    }
}

/// The first offer of permessage-deflate in a handshake's `Sec-WebSocket-Extensions` that kiln can
/// take (RFC 7692 §5, §7.1): its parameters each known, at most once, with values in range. None
/// when there's none (and then nothing is answered and nothing changes).
pub fn negotiate(headers: &HeaderMap) -> Option<Agreed> {
    let values: Vec<&str> = headers.get_all(header::SEC_WEBSOCKET_EXTENSIONS).iter().filter_map(|v| v.to_str().ok()).collect();
    values.iter().flat_map(|v| split_outside_quotes(v, ',')).find_map(|offer| take(&offer))
}

/// One offer, if it's permessage-deflate and kiln can take it.
fn take(offer: &str) -> Option<Agreed> {
    let mut parts = split_outside_quotes(offer, ';').into_iter();
    if !parts.next()?.trim().eq_ignore_ascii_case(NAME) {
        return None;
    }
    let mut agreed = Agreed::default();
    let (mut snct, mut cnct, mut smwb, mut cmwb) = (false, false, false, false);
    for p in parts {
        let (name, value) = match p.split_once('=') {
            Some((n, v)) => (n.trim(), Some(v.trim().trim_matches('"'))),
            None => (p.trim(), None),
        };
        let seen = match name.to_ascii_lowercase().as_str() {
            "server_no_context_takeover" if value.is_none() => {
                agreed.server_no_context_takeover = true;
                &mut snct
            }
            "client_no_context_takeover" if value.is_none() => &mut cnct,
            // A smaller window than kiln's deflate can keep to: declined (see WINDOW_BITS).
            "server_max_window_bits" if window_bits(value?)? == WINDOW_BITS => &mut smwb,
            "client_max_window_bits" if value.is_none_or(|v| window_bits(v).is_some()) => &mut cmwb,
            _ => return None,
        };
        if std::mem::replace(seen, true) {
            return None;
        }
    }
    Some(agreed)
}

/// A window size as RFC 7692 §7.1.2 writes one: 8 to 15, decimal, no leading zero.
fn window_bits(v: &str) -> Option<u8> {
    if v.starts_with('0') || !v.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    v.parse::<u8>().ok().filter(|b| (8..=15).contains(b))
}

/// `s` split at `sep`, but not inside a quoted string (RFC 9110 §5.6.4).
fn split_outside_quotes(s: &str, sep: char) -> Vec<String> {
    let (mut out, mut cur, mut quoted) = (Vec::new(), String::new(), false);
    for c in s.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                cur.push(c);
            }
            c if c == sep && !quoted => out.push(std::mem::take(&mut cur)),
            c => cur.push(c),
        }
    }
    out.push(cur);
    out.into_iter().filter(|p| !p.trim().is_empty()).collect()
}

// ------------------------------------------------------------------------------------ frames

/// A frame's head (RFC 6455 §5.2).
#[derive(Debug, Clone, Copy)]
struct Head {
    fin: bool,
    rsv1: bool,
    /// RSV2 or RSV3: nothing kiln agrees defines them.
    rsv23: bool,
    opcode: u8,
    masked: bool,
    key: [u8; 4],
    len: u64,
    /// The head's own length.
    size: usize,
}

impl Head {
    fn control(&self) -> bool {
        self.opcode & 0x08 != 0
    }

    fn data(&self) -> bool {
        matches!(self.opcode, 1 | 2)
    }
}

/// The head at the start of `b`, once it's all there.
fn head(b: &[u8]) -> Option<Head> {
    let (&b0, &b1) = (b.first()?, b.get(1)?);
    let (len, mut size) = match b1 & 0x7f {
        126 => (u64::from(u16::from_be_bytes(*b.get(2..4)?.first_chunk::<2>()?)), 4),
        127 => (u64::from_be_bytes(*b.get(2..10)?.first_chunk::<8>()?), 10),
        n => (u64::from(n), 2),
    };
    let masked = b1 & 0x80 != 0;
    let mut key = [0u8; 4];
    if masked {
        key = *b.get(size..size + 4)?.first_chunk::<4>()?;
        size += 4;
    }
    Some(Head { fin: b0 & 0x80 != 0, rsv1: b0 & 0x40 != 0, rsv23: b0 & 0x30 != 0, opcode: b0 & 0x0f, masked, key, len, size })
}

/// Writes a frame's head: final, `rsv1` as given, `opcode`, the payload's `len`; masked with
/// `key` when there is one (a frame handed to the server's tungstenite must be masked, as a client's
/// are: an all-zero key leaves the payload as it is).
fn put_head(out: &mut Vec<u8>, rsv1: bool, opcode: u8, len: u64, key: Option<[u8; 4]>) {
    out.push(0x80 | if rsv1 { 0x40 } else { 0 } | (opcode & 0x0f));
    let mask = if key.is_some() { 0x80 } else { 0 };
    match len {
        n if n < 126 => out.push(mask | (n as u8)),
        n if n <= u64::from(u16::MAX) => {
            out.push(mask | 126);
            out.extend_from_slice(&(n as u16).to_be_bytes());
        }
        n => {
            out.push(mask | 127);
            out.extend_from_slice(&n.to_be_bytes());
        }
    }
    if let Some(k) = key {
        out.extend_from_slice(&k);
    }
}

fn invalid(why: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, why)
}

// ------------------------------------------------------------------------------- compression

/// `input`, one whole message, deflated onto the window `c` holds (RFC 7692 §7.2.1): a sync flush,
/// its last four bytes left off; with `reset`, the window emptied after.
fn deflate(c: &mut Compress, input: &[u8], reset: bool) -> io::Result<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() / 2 + 64);
    let mut rest = input;
    loop {
        if out.capacity() - out.len() < 64 {
            out.reserve(out.capacity().max(256));
        }
        let before = c.total_in();
        c.compress_vec(rest, &mut out, FlushCompress::Sync).map_err(io::Error::other)?;
        let used = usize::try_from(c.total_in() - before).map_err(io::Error::other)?;
        rest = rest.get(used..).unwrap_or_default();
        // Done once it's taken it all and the flush left room: the flush is out whole (zlib's contract).
        if rest.is_empty() && out.len() < out.capacity() {
            break;
        }
    }
    if out.ends_with(&TAIL) {
        out.truncate(out.len() - TAIL.len());
    }
    if reset {
        c.reset();
    }
    Ok(out)
}

/// `input` (part of a message) inflated onto `out`, which may grow to `max` bytes and no further:
/// `Ok(false)` once it would.
fn inflate(d: &mut Decompress, mut input: &[u8], out: &mut Vec<u8>, max: usize) -> io::Result<bool> {
    loop {
        if out.len() == out.capacity() {
            if out.len() > max {
                return Ok(false);
            }
            // Room for one more byte than the bound, so a message past it is seen to be.
            let room = out.len().max(4096).min(max.saturating_add(1).saturating_sub(out.len())).max(1);
            out.reserve_exact(room);
        }
        let (before_in, before_out) = (d.total_in(), d.total_out());
        let status = d.decompress_vec(input, out, FlushDecompress::Sync).map_err(|_| invalid("a compressed message that doesn't inflate"))?;
        let used = usize::try_from(d.total_in() - before_in).map_err(io::Error::other)?;
        let made = d.total_out() - before_out;
        input = input.get(used..).unwrap_or_default();
        if out.len() > max {
            return Ok(false);
        }
        if status == Status::StreamEnd {
            // A final block (BFINAL) ends this deflate stream; what follows starts a new one.
            d.reset(false);
        }
        if input.is_empty() && out.len() < out.capacity() {
            return Ok(true);
        }
        if used == 0 && made == 0 && out.len() < out.capacity() {
            return Err(invalid("a compressed message that doesn't inflate"));
        }
    }
}

// ---------------------------------------------------------------------------------- the socket

/// The connection under a server WebSocket: as it is, or with permessage-deflate agreed.
pub struct Socket<T> {
    io: T,
    pmd: Option<Box<Pmd>>,
}

impl<T> Socket<T> {
    /// No extension: every byte passes as it is.
    pub fn plain(io: T) -> Self {
        Self { io, pmd: None }
    }

    /// permessage-deflate as `agreed`, messages up to `max` bytes (inflated).
    pub fn deflating(io: T, agreed: Agreed, max: usize) -> Self {
        Self { io, pmd: Some(Box::new(Pmd::new(agreed, max))) }
    }

    /// Whether permessage-deflate is on.
    pub fn deflates(&self) -> bool {
        self.pmd.is_some()
    }
}

/// A compressed message's deflating on the compute pool: its compressor back, with the message.
type Job = Pin<Box<dyn Future<Output = anyhow::Result<(Compress, io::Result<Vec<u8>>)>> + Send>>;

struct Pmd {
    max: usize,
    // Reading: what came off the connection, and what's ready for tungstenite.
    raw: Vec<u8>,
    raw_at: usize,
    plain: Vec<u8>,
    plain_at: usize,
    reading: Reading,
    inflater: Decompress,
    /// A compressed message coming: its opcode and what it inflated to so far.
    message: Option<(u8, Vec<u8>)>,
    /// Inside a fragmented message that isn't compressed: its continuations pass.
    passing_fragments: bool,
    // Writing: tungstenite's bytes not yet whole frames, and what's ready for the connection.
    pending: Vec<u8>,
    wire: Vec<u8>,
    wire_at: usize,
    deflater: Option<Compress>,
    job: Option<(u8, Job)>,
    reset: bool,
    sending_fragments: bool,
}

#[derive(Debug, Clone, Copy)]
enum Reading {
    /// Waiting for a frame's head.
    Head,
    /// An uncompressed frame's payload passing through.
    Pass { left: u64 },
    /// A compressed message's frame: its payload unmasked and inflated as it comes.
    Inflate { left: u64, key: [u8; 4], at: usize, fin: bool },
    /// A message past the bound was handed on as such: nothing more is read.
    Over,
}

impl Pmd {
    fn new(agreed: Agreed, max: usize) -> Self {
        Self {
            max,
            raw: Vec::new(),
            raw_at: 0,
            plain: Vec::new(),
            plain_at: 0,
            reading: Reading::Head,
            inflater: Decompress::new(false),
            message: None,
            passing_fragments: false,
            pending: Vec::new(),
            wire: Vec::new(),
            wire_at: 0,
            deflater: Some(Compress::new(flate2::Compression::new(LEVEL), false)),
            job: None,
            reset: agreed.server_no_context_takeover,
            sending_fragments: false,
        }
    }

    /// Hands tungstenite a frame head past its bound: it fails the message as too long, as it does
    /// one that came uncompressed.
    fn over(&mut self, opcode: u8) {
        put_head(&mut self.plain, false, opcode, (self.max as u64).saturating_add(1), Some([0; 4]));
        self.message = None;
        self.reading = Reading::Over;
    }

    /// Works on what came; `Ok(false)` when it needs more.
    fn step(&mut self) -> io::Result<bool> {
        let avail = self.raw.get(self.raw_at..).unwrap_or_default();
        match self.reading {
            Reading::Over => Ok(false),
            Reading::Head => {
                let Some(h) = head(avail) else { return Ok(false) };
                if h.data() && h.rsv1 && !h.rsv23 && self.message.is_none() && !self.passing_fragments {
                    // The first frame of a compressed message (RSV1 only there: RFC 7692 §6.1).
                    self.raw_at += h.size;
                    if h.len > self.max as u64 {
                        self.over(h.opcode);
                        return Ok(true);
                    }
                    self.message = Some((h.opcode, Vec::new()));
                    self.reading = Reading::Inflate { left: h.len, key: h.key, at: 0, fin: h.fin };
                } else if self.message.is_some() && !h.control() {
                    if h.opcode != 0 || h.rsv1 || h.rsv23 {
                        return Err(invalid("a compressed message broken off"));
                    }
                    self.raw_at += h.size;
                    self.reading = Reading::Inflate { left: h.len, key: h.key, at: 0, fin: h.fin };
                } else {
                    // Uncompressed, or a control frame: on as it is, for tungstenite to judge.
                    if h.data() {
                        self.passing_fragments = !h.fin;
                    } else if h.opcode == 0 && h.fin {
                        self.passing_fragments = false;
                    }
                    self.plain.extend_from_slice(avail.get(..h.size).unwrap_or_default());
                    self.raw_at += h.size;
                    self.reading = Reading::Pass { left: h.len };
                }
                Ok(true)
            }
            Reading::Pass { left } => {
                let n = usize::try_from(left).unwrap_or(usize::MAX).min(avail.len());
                if n == 0 && left > 0 {
                    return Ok(false);
                }
                self.plain.extend_from_slice(avail.get(..n).unwrap_or_default());
                self.raw_at += n;
                let left = left - n as u64;
                self.reading = if left == 0 { Reading::Head } else { Reading::Pass { left } };
                Ok(true)
            }
            Reading::Inflate { left, key, at, fin } => {
                let n = usize::try_from(left).unwrap_or(usize::MAX).min(avail.len());
                if n == 0 && left > 0 {
                    return Ok(false);
                }
                let (start, end) = (self.raw_at, self.raw_at + n);
                if let Some(chunk) = self.raw.get_mut(start..end) {
                    for (i, b) in chunk.iter_mut().enumerate() {
                        *b ^= key.get((at + i) % 4).copied().unwrap_or(0);
                    }
                }
                self.raw_at = end;
                let left = left - n as u64;
                let Some((opcode, mut out)) = self.message.take() else { return Err(invalid("a compressed frame outside a message")) };
                let mut fits = inflate(&mut self.inflater, self.raw.get(start..end).unwrap_or_default(), &mut out, self.max)?;
                if fits && left == 0 && fin {
                    fits = inflate(&mut self.inflater, &TAIL, &mut out, self.max)?;
                }
                if !fits {
                    self.over(opcode);
                    return Ok(true);
                }
                if left > 0 {
                    self.message = Some((opcode, out));
                    self.reading = Reading::Inflate { left, key, at: at + n, fin };
                } else if fin {
                    put_head(&mut self.plain, false, opcode, out.len() as u64, Some([0; 4]));
                    self.plain.extend_from_slice(&out);
                    self.reading = Reading::Head;
                } else {
                    self.message = Some((opcode, out));
                    self.reading = Reading::Head;
                }
                Ok(true)
            }
        }
    }

    /// Turns what tungstenite wrote into frames for the connection, whole frames at a time, in order:
    /// a message to compress waits behind one being compressed on the pool.
    fn frames(&mut self) -> io::Result<()> {
        let mut at = 0;
        while self.job.is_none() {
            let Some(h) = head(self.pending.get(at..).unwrap_or_default()) else { break };
            let len = usize::try_from(h.len).map_err(|_| invalid("a frame larger than memory"))?;
            let total = h.size.checked_add(len).ok_or_else(|| invalid("a frame larger than memory"))?;
            let Some(frame) = self.pending.get(at..at + total) else { break };
            let payload = frame.get(h.size..).unwrap_or_default();
            let whole = h.data() && h.fin && !self.sending_fragments && !h.masked && !h.rsv1 && !h.rsv23;
            if h.data() {
                self.sending_fragments = !h.fin;
            } else if h.opcode == 0 && h.fin {
                self.sending_fragments = false;
            }
            let packed = h.opcode == 2 && payload.starts_with(&GZIP);
            if whole && !packed {
                if inline(Work::Deflate, len) {
                    let c = self.deflater.as_mut().ok_or_else(|| invalid("the compressor is away"))?;
                    let out = deflate(c, payload, self.reset)?;
                    put_head(&mut self.wire, true, h.opcode, out.len() as u64, None);
                    self.wire.extend_from_slice(&out);
                } else {
                    let mut c = self.deflater.take().ok_or_else(|| invalid("the compressor is away"))?;
                    let (input, reset) = (payload.to_vec(), self.reset);
                    let job: Job = Box::pin(crate::rt::compute(move || {
                        let out = deflate(&mut c, &input, reset);
                        (c, out)
                    }));
                    self.job = Some((h.opcode, job));
                }
            } else {
                self.wire.extend_from_slice(frame);
            }
            at += total;
        }
        self.pending.drain(..at);
        if self.pending.is_empty() {
            self.pending = Vec::new();
        }
        Ok(())
    }

    /// Ready once no message is being compressed on the pool.
    fn poll_job(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if let Some((opcode, job)) = &mut self.job {
            let done = ready!(job.as_mut().poll(cx));
            let opcode = *opcode;
            self.job = None;
            let (c, out) = done.map_err(io::Error::other)?;
            self.deflater = Some(c);
            let out = out?;
            put_head(&mut self.wire, true, opcode, out.len() as u64, None);
            self.wire.extend_from_slice(&out);
        }
        Poll::Ready(Ok(()))
    }
}

impl<T: AsyncWrite + Unpin> Socket<T> {
    /// Writes what's ready for the connection; ready once it's all written.
    fn poll_wire(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let Some(p) = self.pmd.as_mut() else { return Poll::Ready(Ok(())) };
        while p.wire_at < p.wire.len() {
            let n = ready!(Pin::new(&mut self.io).poll_write(cx, p.wire.get(p.wire_at..).unwrap_or_default()))?;
            if n == 0 {
                return Poll::Ready(Err(io::ErrorKind::WriteZero.into()));
            }
            p.wire_at += n;
        }
        // An idle connection holds no buffer (docs/http.md): what a message needed goes with it.
        p.wire = Vec::new();
        p.wire_at = 0;
        Poll::Ready(Ok(()))
    }

    /// Every message tungstenite wrote compressed and written to the connection.
    fn poll_drain(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        loop {
            let Some(p) = self.pmd.as_mut() else { return Poll::Ready(Ok(())) };
            ready!(p.poll_job(cx))?;
            p.frames()?;
            let waiting = p.job.is_some();
            ready!(self.poll_wire(cx))?;
            if !waiting {
                return Poll::Ready(Ok(()));
            }
        }
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for Socket<T> {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut [u8]) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let Some(p) = this.pmd.as_mut() else { return Pin::new(&mut this.io).poll_read(cx, buf) };
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        loop {
            if p.plain_at < p.plain.len() {
                let from = p.plain.get(p.plain_at..).unwrap_or_default();
                let n = from.len().min(buf.len());
                if let (Some(to), Some(from)) = (buf.get_mut(..n), from.get(..n)) {
                    to.copy_from_slice(from);
                }
                p.plain_at += n;
                if p.plain_at == p.plain.len() {
                    p.plain = Vec::new();
                    p.plain_at = 0;
                }
                return Poll::Ready(Ok(n));
            }
            while p.plain.is_empty() && p.step()? {}
            if !p.plain.is_empty() {
                continue;
            }
            if matches!(p.reading, Reading::Over) {
                return Poll::Ready(Ok(0));
            }
            // An uncompressed payload with nothing else buffered reads straight into tungstenite's buffer.
            if let Reading::Pass { left } = p.reading
                && p.raw_at == p.raw.len()
            {
                let want = usize::try_from(left).unwrap_or(usize::MAX).min(buf.len());
                let n = ready!(Pin::new(&mut this.io).poll_read(cx, buf.get_mut(..want).unwrap_or_default()))?;
                let left = left.saturating_sub(n as u64);
                p.reading = if left == 0 { Reading::Head } else { Reading::Pass { left } };
                return Poll::Ready(Ok(n));
            }
            // More off the connection: as much as tungstenite asked for.
            p.raw.drain(..p.raw_at);
            p.raw_at = 0;
            let old = p.raw.len();
            p.raw.resize(old + buf.len().max(1), 0);
            let read = Pin::new(&mut this.io).poll_read(cx, p.raw.get_mut(old..).unwrap_or_default());
            match read {
                Poll::Ready(Ok(n)) => {
                    p.raw.truncate(old + n);
                    if n == 0 {
                        return Poll::Ready(Ok(0));
                    }
                }
                Poll::Ready(Err(e)) => {
                    p.raw.truncate(old);
                    return Poll::Ready(Err(e));
                }
                Poll::Pending => {
                    p.raw.truncate(old);
                    if p.raw.is_empty() {
                        // Waiting with nothing read: no buffer held meanwhile (docs/http.md).
                        p.raw = Vec::new();
                    }
                    return Poll::Pending;
                }
            }
        }
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for Socket<T> {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if this.pmd.is_none() {
            return Pin::new(&mut this.io).poll_write(cx, buf);
        }
        // What was made ready before goes first: tungstenite keeps what's next until it has.
        ready!(this.poll_drain(cx))?;
        let Some(p) = this.pmd.as_mut() else { return Poll::Ready(Ok(0)) };
        p.pending.extend_from_slice(buf);
        p.frames()?;
        // Written as far as the connection takes now; the rest by the next write or flush.
        if let Poll::Ready(Err(e)) = this.poll_wire(cx) {
            return Poll::Ready(Err(e));
        }
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        ready!(this.poll_drain(cx))?;
        Pin::new(&mut this.io).poll_flush(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        ready!(this.poll_drain(cx))?;
        Pin::new(&mut this.io).poll_close(cx)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::{Arc, Mutex, PoisonError};

    use futures::StreamExt as _;
    use tungstenite::Message;
    use tungstenite::protocol::{Role, WebSocketConfig};

    use super::*;

    /// A connection in memory: what the peer sent, handed out `chunk` bytes a read; what was written,
    /// kept.
    pub(crate) struct Wire {
        input: Vec<u8>,
        at: usize,
        chunk: usize,
        pub(crate) output: Arc<Mutex<Vec<u8>>>,
    }

    impl Wire {
        pub(crate) fn new(input: Vec<u8>, chunk: usize) -> (Self, Arc<Mutex<Vec<u8>>>) {
            let output = Arc::new(Mutex::new(Vec::new()));
            (Self { input, at: 0, chunk, output: Arc::clone(&output) }, output)
        }
    }

    impl AsyncRead for Wire {
        fn poll_read(self: Pin<&mut Self>, _: &mut Context<'_>, buf: &mut [u8]) -> Poll<io::Result<usize>> {
            let this = self.get_mut();
            let from = this.input.get(this.at..).unwrap_or_default();
            let n = from.len().min(buf.len()).min(this.chunk);
            if let (Some(to), Some(from)) = (buf.get_mut(..n), from.get(..n)) {
                to.copy_from_slice(from);
            }
            this.at += n;
            Poll::Ready(Ok(n))
        }
    }

    impl AsyncWrite for Wire {
        fn poll_write(self: Pin<&mut Self>, _: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
            self.output.lock().unwrap_or_else(PoisonError::into_inner).extend_from_slice(buf);
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    fn config(max: usize) -> WebSocketConfig {
        WebSocketConfig::default().max_message_size(Some(max)).max_frame_size(Some(max))
    }

    /// A client's frame: masked, as RFC 6455 has them.
    pub(crate) fn client_frame(fin: bool, rsv1: bool, opcode: u8, payload: &[u8]) -> Vec<u8> {
        let key = [0x37u8, 0xfa, 0x21, 0x3d];
        let mut f = Vec::new();
        put_head(&mut f, rsv1, opcode, payload.len() as u64, Some(key));
        if let Some(b0) = f.first_mut()
            && !fin
        {
            *b0 &= 0x7f;
        }
        f.extend(payload.iter().zip(key.iter().cycle()).map(|(b, k)| b ^ k));
        f
    }

    /// A client's deflater (context taken over, as browsers do).
    pub(crate) fn client_deflater() -> Compress {
        Compress::new(flate2::Compression::new(LEVEL), false)
    }

    /// A server's frames as a client reads them: (rsv1, opcode, payload).
    pub(crate) fn server_frames(mut b: &[u8]) -> anyhow::Result<Vec<(bool, u8, Vec<u8>)>> {
        let mut out = Vec::new();
        while !b.is_empty() {
            let h = head(b).ok_or_else(|| anyhow::anyhow!("a frame cut short"))?;
            anyhow::ensure!(h.fin && !h.masked, "a server frame is final and unmasked: {h:?}");
            let end = h.size + usize::try_from(h.len)?;
            out.push((h.rsv1, h.opcode, b.get(h.size..end).ok_or_else(|| anyhow::anyhow!("a payload cut short"))?.to_vec()));
            b = b.get(end..).unwrap_or_default();
        }
        Ok(out)
    }

    /// A client's inflater over a server's messages: each payload as it was sent.
    pub(crate) fn inflate_all(frames: &[(bool, u8, Vec<u8>)], fresh_each: bool) -> anyhow::Result<Vec<(u8, Vec<u8>)>> {
        let mut d = Decompress::new(false);
        let mut out = Vec::new();
        for (rsv1, op, p) in frames {
            if !rsv1 {
                out.push((*op, p.clone()));
                continue;
            }
            if fresh_each {
                d = Decompress::new(false);
            }
            let mut m = Vec::new();
            let mut whole = p.clone();
            whole.extend_from_slice(&TAIL);
            anyhow::ensure!(inflate(&mut d, &whole, &mut m, 64 << 20)?, "too long");
            out.push((*op, m));
        }
        Ok(out)
    }

    /// `n` `harness` events of the events socket as kiln sends them (`crate::wire::events`): an agent
    /// entry each, as lists show it, of twelve agents in turn, each a little on from its last.
    pub(crate) fn harness_burst(n: usize) -> Vec<String> {
        use crate::harness::model::Status;
        use crate::harness::session::Info;
        let said = [
            "I've read the router and the handler it calls; the compression goes after the route, in one place, so no handler changes.",
            "The tests pass on Linux. Next: clippy with -D warnings for the crates I touched, then the integration test against a real server.",
            "Found it: the frame head is parsed before the payload arrives, so a message past the bound fails before it is inflated.",
            "Running cargo test -p kiln --lib http:: on the runner kiln picked (4 cores, 8 GiB).",
        ];
        (0..n)
            .map(|i| {
                let a = i % 12;
                let info = Info {
                    id: format!("01a1b{a:03}c2d4e6f8091a2b3c4d5e6f7a8b9c"),
                    kind: "claude".into(),
                    kind_name: "Claude Code".into(),
                    target: format!("agent:{}", ["adacachy", "fedora", "adas-macbook-pro-2", "jupiter"].get(a % 4).copied().unwrap_or("adacachy")),
                    name: format!("wire-track-{a}"),
                    cwd: format!("/home/adalundhe/.cache/kiln-wire/track-{a}"),
                    tree: Some(format!("/home/adalundhe/.cache/kiln-wire/track-{a}")),
                    created_at: 1_793_000_000_000 + (a as i64) * 61_000,
                    status: if i % 5 == 0 { Status::Waiting } else { Status::Running },
                    title: format!("Make the UI's wire smaller: track {a} of the plan, compression and validators"),
                    model: "claude-opus-5-5".into(),
                    mode: "bypassPermissions".into(),
                    harness_session: Some(format!("5f0c{a:02}a1-7d3e-4b2a-9c1f-0e8d7c6b5a49")),
                    queued: i % 3,
                    tasks_running: (i % 4) + 1,
                    cost_usd: 1.25 + (i as f64) * 0.037,
                    activity: Some(format!("Bash: cargo test -p kiln --lib http::deflate (step {i})")),
                    last_activity: 1_793_000_500_000 + (i as i64) * 1_250,
                    goal: "Everything kiln's browser UI receives goes compressed when the browser can take it: HTTP response compression for API responses, ETags and 304s for JSON GETs, and permessage-deflate on every WebSocket.".into(),
                    summary: said.get(i % said.len()).copied().unwrap_or_default().into(),
                    doing: Some(format!("Reading crates/kiln/src/http/ws.rs, line {}", 40 + i)),
                    todos_done: u32::try_from(i % 7).unwrap_or(0),
                    todos_total: 9,
                    files_changed: 6,
                    lines_added: 900 + u32::try_from(i).unwrap_or(0),
                    lines_removed: 12,
                    context_pct: Some(u8::try_from(20 + i % 60).unwrap_or(50)),
                    turn_started: Some(1_793_000_400_000 + (a as i64) * 1000),
                    plan: Some("max".into()),
                    ..Info::default()
                };
                crate::wire::events::ServerEvent::Harness { info: Box::new(info) }.text()
            })
            .collect()
    }

    /// Sends `messages` from a server socket agreed `agreed`, and returns what went on the wire.
    fn sent(agreed: Option<Agreed>, messages: Vec<Message>) -> anyhow::Result<Vec<u8>> {
        crate::rt::block_on(async move {
            let (wire, out) = Wire::new(Vec::new(), usize::MAX);
            let socket = match agreed {
                Some(a) => Socket::deflating(wire, a, 16 << 20),
                None => Socket::plain(wire),
            };
            let mut ws = async_tungstenite::WebSocketStream::from_raw_socket(socket, Role::Server, Some(config(16 << 20))).await;
            for m in messages {
                ws.send(m).await?;
            }
            let b = out.lock().unwrap_or_else(PoisonError::into_inner).clone();
            Ok(b)
        })
    }

    #[test]
    fn w21_r15_offers_are_taken_as_rfc_7692_says() {
        let offer = |v: &[&str]| {
            let mut h = HeaderMap::new();
            for v in v {
                h.append(header::SEC_WEBSOCKET_EXTENSIONS, HeaderValue::from_str(v).unwrap_or(HeaderValue::from_static("")));
            }
            negotiate(&h)
        };
        let plain = Some(Agreed { server_no_context_takeover: false });
        // What Chromium, WebKit and Firefox send.
        assert_eq!(offer(&["permessage-deflate; client_max_window_bits"]), plain);
        assert_eq!(offer(&["permessage-deflate"]), plain);
        assert_eq!(offer(&["Permessage-Deflate; Client_Max_Window_Bits=10"]), plain);
        assert_eq!(offer(&["permessage-deflate; server_no_context_takeover; client_no_context_takeover"]), Some(Agreed { server_no_context_takeover: true }));
        assert_eq!(offer(&["permessage-deflate; server_max_window_bits=15"]), plain);
        assert_eq!(offer(&["permessage-deflate; client_max_window_bits=\"12\""]), plain);
        // Nothing offered, another extension, or nothing kiln can take: none.
        assert_eq!(offer(&[]), None);
        assert_eq!(offer(&["x-webkit-deflate-frame"]), None);
        assert_eq!(offer(&["permessage-deflate; server_max_window_bits=10"]), None);
        assert_eq!(offer(&["permessage-deflate; server_max_window_bits"]), None);
        assert_eq!(offer(&["permessage-deflate; client_max_window_bits=7"]), None);
        assert_eq!(offer(&["permessage-deflate; client_max_window_bits=016"]), None);
        assert_eq!(offer(&["permessage-deflate; server_no_context_takeover=1"]), None);
        assert_eq!(offer(&["permessage-deflate; server_no_context_takeover; server_no_context_takeover"]), None);
        assert_eq!(offer(&["permessage-deflate; mystery"]), None);
        // The first offer kiln can take, across a list and across headers.
        assert_eq!(offer(&["permessage-deflate; server_max_window_bits=10, permessage-deflate; server_no_context_takeover"]), Some(Agreed { server_no_context_takeover: true }));
        assert_eq!(offer(&["foo", "permessage-deflate; client_max_window_bits"]), plain);
        assert_eq!(Agreed { server_no_context_takeover: true }.header(), "permessage-deflate; server_no_context_takeover");
    }

    #[test]
    fn w21_r15_a_socket_without_the_extension_sends_what_it_always_did() -> anyhow::Result<()> {
        let messages = || vec![Message::text("{\"type\":\"queue\"}".repeat(100)), Message::binary(vec![7u8; 3000]), Message::Ping(vec![1, 2].into())];
        let plain = sent(None, messages())?;
        // The frames tungstenite makes, byte for byte, with nothing set in RSV1.
        let frames = server_frames(&plain)?;
        assert_eq!(frames.len(), 3);
        assert!(frames.iter().all(|(rsv1, _, _)| !rsv1));
        let mut want = Vec::new();
        for m in messages() {
            let (op, p): (u8, Vec<u8>) = match m {
                Message::Text(t) => (1, t.as_bytes().to_vec()),
                Message::Binary(b) => (2, b.to_vec()),
                Message::Ping(b) => (9, b.to_vec()),
                _ => continue,
            };
            put_head(&mut want, false, op, p.len() as u64, None);
            want.extend_from_slice(&p);
        }
        assert_eq!(plain, want);
        Ok(())
    }

    #[test]
    fn w21_r15_server_messages_deflate_with_the_window_carried() -> anyhow::Result<()> {
        let burst = harness_burst(60);
        let packed = {
            let mut g = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
            std::io::Write::write_all(&mut g, b"a packed snapshot")?;
            g.finish()?
        };
        let mut messages: Vec<Message> = burst.iter().map(|t| Message::text(t.clone())).collect();
        messages.push(Message::binary(packed.clone()));
        messages.push(Message::Ping(b"p".to_vec().into()));
        messages.push(Message::text(""));
        let wire = sent(Some(Agreed::default()), messages)?;
        let frames = server_frames(&wire)?;
        assert_eq!(frames.len(), burst.len() + 3);
        // Text compressed; the gzip'd binary and the ping as they were.
        assert!(frames.iter().take(burst.len()).all(|(rsv1, op, _)| *rsv1 && *op == 1));
        assert!(matches!(frames.get(burst.len()), Some((false, 2, p)) if *p == packed));
        assert!(matches!(frames.get(burst.len() + 1), Some((false, 9, p)) if p == b"p"));
        let back = inflate_all(&frames, false)?;
        for (i, t) in burst.iter().enumerate() {
            assert_eq!(back.get(i).map(|(_, p)| p.as_slice()), Some(t.as_bytes()), "message {i}");
        }
        assert!(matches!(back.last(), Some((1, p)) if p.is_empty()));
        // With the window carried, a message like the ones before costs a fraction of the first.
        let first = frames.first().map_or(0, |f| f.2.len());
        let later: usize = frames.iter().skip(12).take(burst.len() - 12).map(|f| f.2.len()).sum::<usize>() / (burst.len() - 12);
        assert!(later * 3 < first, "first {first} B, later {later} B a message");
        // A fresh window each message, as a client may ask: each inflates alone.
        let wire = sent(Some(Agreed { server_no_context_takeover: true }), burst.iter().map(|t| Message::text(t.clone())).collect())?;
        let alone = inflate_all(&server_frames(&wire)?, true)?;
        assert_eq!(alone.iter().map(|(_, p)| p.clone()).collect::<Vec<_>>(), burst.iter().map(|t| t.as_bytes().to_vec()).collect::<Vec<_>>());
        Ok(())
    }

    #[test]
    fn w21_r15_a_large_message_compresses_off_the_shard_in_order() -> anyhow::Result<()> {
        // 4 MiB is past any shard's quantum at kiln's measured rate: it goes to the compute pool, and
        // the small messages around it keep their places.
        let big: String = harness_burst(4000).concat();
        assert!(!inline(Work::Deflate, big.len()) || hyper_rt::futures::step_budget_ns().is_none());
        let messages = vec![Message::text("before"), Message::text(big.clone()), Message::text("after")];
        let frames = server_frames(&sent(Some(Agreed::default()), messages)?)?;
        let back = inflate_all(&frames, false)?;
        let texts: Vec<&[u8]> = back.iter().map(|(_, p)| p.as_slice()).collect();
        assert_eq!(texts, vec![b"before".as_slice(), big.as_bytes(), b"after".as_slice()]);
        Ok(())
    }

    /// What a server socket agreed so reads of `input` (`chunk` bytes a read), with messages bound to
    /// `max`: the messages, and the error it ended with, if any; and what it wrote back.
    fn read(input: Vec<u8>, chunk: usize, max: usize) -> anyhow::Result<(Vec<Message>, Option<tungstenite::Error>, Vec<u8>)> {
        crate::rt::block_on(async move {
            let (wire, out) = Wire::new(input, chunk);
            let mut ws = async_tungstenite::WebSocketStream::from_raw_socket(Socket::deflating(wire, Agreed::default(), max), Role::Server, Some(config(max))).await;
            let mut got = Vec::new();
            let mut ended = None;
            while let Some(m) = ws.next().await {
                match m {
                    Ok(m) => got.push(m),
                    Err(e) => {
                        ended = Some(e);
                        break;
                    }
                }
            }
            let wrote = out.lock().unwrap_or_else(PoisonError::into_inner).clone();
            Ok((got, ended, wrote))
        })
    }

    #[test]
    fn w21_r15_client_messages_inflate_fragments_controls_and_all() -> anyhow::Result<()> {
        let burst = harness_burst(20);
        let mut c = client_deflater();
        let mut input = Vec::new();
        // Whole messages, compressed onto one window.
        for t in &burst {
            input.extend(client_frame(true, true, 1, &deflate(&mut c, t.as_bytes(), false)?));
        }
        // One message in three fragments (RSV1 on the first alone), a ping between.
        let big = burst.concat();
        let packed = deflate(&mut c, big.as_bytes(), false)?;
        let third = packed.len() / 3;
        input.extend(client_frame(false, true, 1, packed.get(..third).unwrap_or_default()));
        input.extend(client_frame(true, false, 9, b"between"));
        input.extend(client_frame(false, false, 0, packed.get(third..2 * third).unwrap_or_default()));
        input.extend(client_frame(true, false, 0, packed.get(2 * third..).unwrap_or_default()));
        // Uncompressed messages still pass, whole and fragmented, binary too.
        input.extend(client_frame(true, false, 1, b"as it was"));
        input.extend(client_frame(false, false, 2, b"in "));
        input.extend(client_frame(true, false, 0, b"parts"));
        // And a compressed binary message after them, on the same window.
        input.extend(client_frame(true, true, 2, &deflate(&mut c, &[9u8; 5000], false)?));
        input.extend(client_frame(true, false, 8, &1000u16.to_be_bytes()));
        for chunk in [1, 7, 4096, usize::MAX] {
            let (got, ended, wrote) = read(input.clone(), chunk, 1 << 20)?;
            assert!(ended.is_none(), "chunk {chunk}: {ended:?}");
            let texts: Vec<String> = got.iter().filter_map(|m| if let Message::Text(t) = m { Some(t.as_str().to_owned()) } else { None }).collect();
            let mut want = burst.clone();
            want.push(big.clone());
            want.push("as it was".into());
            assert_eq!(texts, want, "chunk {chunk}");
            assert!(got.iter().any(|m| matches!(m, Message::Ping(p) if p.as_ref() == b"between")));
            assert!(got.iter().any(|m| matches!(m, Message::Binary(b) if b.as_ref() == b"in parts")));
            assert!(got.iter().any(|m| matches!(m, Message::Binary(b) if b.as_ref() == [9u8; 5000].as_slice())));
            // The pong and the close went back out (control frames, as they were).
            let back = server_frames(&wrote)?;
            assert!(back.iter().any(|(rsv1, op, p)| !rsv1 && *op == 10 && p == b"between"), "{back:?}");
            assert!(back.iter().any(|(_, op, _)| *op == 8));
        }
        Ok(())
    }

    #[test]
    fn w21_r15_a_message_past_the_bound_after_inflating_fails_as_too_long() -> anyhow::Result<()> {
        // 4 MiB of zeros deflates to a few KiB: a bomb, against a 64 KiB bound.
        let mut c = client_deflater();
        let bomb = deflate(&mut c, &vec![0u8; 4 << 20], false)?;
        assert!(bomb.len() < 64 << 10);
        let input = client_frame(true, true, 1, &bomb);
        let (got, ended, _) = read(input, usize::MAX, 64 << 10)?;
        assert!(got.is_empty());
        assert!(matches!(ended, Some(tungstenite::Error::Capacity(_))), "{ended:?}");
        // Just under the bound inflates.
        let mut c = client_deflater();
        let fits = deflate(&mut c, &vec![b'a'; 64 << 10], false)?;
        let mut input = client_frame(true, true, 1, &fits);
        input.extend(client_frame(true, false, 8, &1000u16.to_be_bytes()));
        let (got, ended, _) = read(input, 3, 64 << 10)?;
        assert!(ended.is_none(), "{ended:?}");
        assert!(matches!(got.first(), Some(Message::Text(t)) if t.len() == 64 << 10));
        // Past it by fragments, each small.
        let mut c = client_deflater();
        let packed = deflate(&mut c, &vec![b'b'; 200 << 10], false)?;
        let half = packed.len() / 2;
        let mut input = client_frame(false, true, 2, packed.get(..half).unwrap_or_default());
        input.extend(client_frame(true, false, 0, packed.get(half..).unwrap_or_default()));
        let (_, ended, _) = read(input, usize::MAX, 64 << 10)?;
        assert!(matches!(ended, Some(tungstenite::Error::Capacity(_))), "{ended:?}");
        Ok(())
    }

    #[test]
    fn w21_r15_broken_compressed_messages_end_the_socket() -> anyhow::Result<()> {
        let mut c = client_deflater();
        let p = deflate(&mut c, b"{\"t\":\"hello\"}", false)?;
        // RSV1 on a continuation.
        let mut input = client_frame(false, true, 1, &p);
        input.extend(client_frame(true, true, 0, b""));
        assert!(read(input, usize::MAX, 1 << 20)?.1.is_some());
        // A new message inside a compressed one.
        let mut input = client_frame(false, true, 1, &p);
        input.extend(client_frame(true, false, 1, b"x"));
        assert!(read(input, usize::MAX, 1 << 20)?.1.is_some());
        // Bytes that aren't deflate.
        assert!(read(client_frame(true, true, 1, &[0xff; 32]), usize::MAX, 1 << 20)?.1.is_some());
        // RSV1 on a control frame: tungstenite's own refusal.
        assert!(matches!(read(client_frame(true, true, 9, b"x"), usize::MAX, 1 << 20)?.1, Some(tungstenite::Error::Protocol(_))));
        Ok(())
    }
}

/// What the wire saves on kiln's own payloads (W21-R15): bytes, before and after, as the report
/// states them. With `KILN_WIRE_DUMP=<dir>` the payloads are written there too, for the timings
/// taken in a release build (the codecs are unoptimized in a test build).
#[cfg(test)]
mod saved {
    use super::tests::harness_burst;
    use super::*;
    use crate::http::compress::Coding;

    #[test]
    fn w21_r15_bytes_saved_on_kilns_payloads() -> anyhow::Result<()> {
        let runners = crate::http::compress::tests::runners_list()?;
        let burst = harness_burst(500);
        let list = format!("[{}]", burst.iter().take(60).filter_map(|e| serde_json::from_str::<serde_json::Value>(e).ok()?.get("info").map(serde_json::Value::to_string)).collect::<Vec<_>>().join(","));
        for (name, body) in [("runners list (production fixture)", runners.as_slice()), ("agents list, 60 entries", list.as_bytes())] {
            let br = Coding::Br.encode(body)?.len();
            let gz = Coding::Gzip.encode(body)?.len();
            println!("HTTP {name}: {} B identity, {gz} B gzip, {br} B brotli", body.len());
            assert!(br * 4 < body.len() && gz * 3 < body.len(), "{name}");
        }
        let identity: usize = burst.iter().map(String::len).sum();
        let (mut carried, mut fresh) = (0usize, 0usize);
        let mut c = Compress::new(flate2::Compression::new(LEVEL), false);
        for e in &burst {
            carried += deflate(&mut c, e.as_bytes(), false)?.len();
            fresh += deflate(&mut Compress::new(flate2::Compression::new(LEVEL), false), e.as_bytes(), true)?.len();
        }
        println!(
            "WS 500 harness events: {identity} B identity ({} B a message), {fresh} B deflated without context takeover, {carried} B with it ({} B a message)",
            identity / burst.len(),
            carried / burst.len()
        );
        assert!(carried * 8 < identity, "{carried} of {identity}");
        if let Ok(dir) = std::env::var("KILN_WIRE_DUMP") {
            let dir = std::path::Path::new(&dir);
            std::fs::create_dir_all(dir)?;
            std::fs::write(dir.join("runners.json"), &runners)?;
            std::fs::write(dir.join("agents.json"), &list)?;
            std::fs::write(dir.join("burst.jsonl"), burst.join("\n"))?;
        }
        Ok(())
    }
}
