//! The lab's clients, on blocking sockets: HTTP/1.1 with keep-alive, and a WebSocket. Each worker
//! of the load owns its own, so nothing is shared while the lab measures.

use std::io::Write as _;
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::time::{Duration, Instant};

/// How long a request or a frame may take before the lab gives up on it: past any stall the lab
/// means to see (a minute: `kiln_route_seconds`' last bucket), so a stall is measured, not cut.
/// Every wait of a client's has an end by it (goal 1, plan 01a1a59c2a68 step 3): a request's connect,
/// its retry and its reads share one deadline, each socket's writes and reads time out, and an
/// interrupted read is read again only within the time it had (`crate::rx::read`).
pub const PATIENCE: Duration = Duration::from_secs(90);

/// A connection to the server on `port`, made by `until` (a connect a full backlog never answers waits out the kernel's
/// SYN retries otherwise, minutes): no delay, its reads and writes each timing out at [`PATIENCE`], its arrivals stamped
/// where the socket stamps them.
fn connect(port: u16, until: Instant) -> anyhow::Result<TcpStream> {
    let left = until.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "no time left to connect").into());
    }
    let s = TcpStream::connect_timeout(&SocketAddr::from((Ipv4Addr::LOCALHOST, port)), left)?;
    s.set_nodelay(true)?;
    s.set_read_timeout(Some(PATIENCE))?;
    s.set_write_timeout(Some(PATIENCE))?;
    crate::rx::stamp_arrivals(&s);
    Ok(s)
}

/// An answer: its status and body, its `Retry-After` (whole seconds) where it has one, what the server said of
/// its own time with the request, where it was asked ([`Http::timed`]), and when its last bytes arrived at the lab's
/// socket, microseconds since the epoch, where the socket stamps arrivals (`crate::rx`; goal 10, plan 01a1a87b2dae step 5).
pub struct Answer {
    pub status: u16,
    pub body: Vec<u8>,
    pub retry_after: Option<Duration>,
    pub server: Option<ServerTime>,
    pub arrived_us: Option<u64>,
}

/// What the server said of its time with a request (kiln's `Server-Timing`: `wait;dur=…, took;dur=…`, milliseconds):
/// how long it waited before its handler started, from its head's arrival, where the server knew, and how long the
/// handler took, in microseconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ServerTime {
    pub wait_us: Option<u64>,
    pub took_us: u64,
}

impl ServerTime {
    /// From a `Server-Timing` value: none without a `took`.
    pub fn parse(v: &str) -> Option<ServerTime> {
        let us = |name: &str| {
            v.split(',').find_map(|m| m.trim().strip_prefix(name)?.strip_prefix(";dur=")?.trim().parse::<f64>().ok()).filter(|ms| ms.is_finite() && *ms >= 0.0).map(|ms| {
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let us = (ms * 1e3).round() as u64;
                us
            })
        };
        Some(ServerTime { wait_us: us("wait"), took_us: us("took")? })
    }
}

/// The request header that asks kiln for a request's own timing (kiln's `crate::api::ASK_TIMING`).
const ASK_TIMING: &str = "Kiln-Timing: 1\r\n";

/// One HTTP/1.1 connection to the server, kept open between requests and opened again when the
/// server closes it.
pub struct Http {
    port: u16,
    token: String,
    conn: Option<TcpStream>,
    buf: Vec<u8>,
    /// What it takes packed: `identity` (the lab reads what it's sent) unless it loads pages, which
    /// a browser takes packed (brotli, gzip), and what it asks for (a browser asks for a page).
    accept: &'static str,
    wants: &'static str,
    /// Whether it asks the server for each request's own timing ([`Http::timed`]).
    timing: bool,
}

impl Http {
    pub fn new(port: u16, token: &str) -> Self {
        Self { port, token: token.to_owned(), conn: None, buf: Vec::new(), accept: "identity", wants: "*/*", timing: false }
    }

    /// One that asks the server, with each request, how long it waited for its handler and how long that took
    /// ([`Answer::server`]; goal 10, plan 01a1a87b2dae step 5: a slow read split into the server's share and the
    /// rest, the hops and the transfer).
    pub fn timed(self) -> Self {
        Self { timing: true, ..self }
    }

    /// One that asks as a browser loading pages does: a page, packed (Chromium's `Accept` for a
    /// navigation).
    pub fn browser(port: u16, token: &str) -> Self {
        Self { accept: "br, gzip", wants: "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8", ..Self::new(port, token) }
    }

    fn open(&mut self, until: Instant) -> anyhow::Result<()> {
        if self.conn.is_none() {
            let s = connect(self.port, until)?;
            self.buf.clear();
            self.conn = Some(s);
        }
        Ok(())
    }

    /// `method path` with `body` (JSON) when there is one: the answer. A connection the server had
    /// closed is opened again once. All of it, the retry too, within [`PATIENCE`].
    pub fn send(&mut self, method: &str, path: &str, body: Option<&[u8]>) -> anyhow::Result<Answer> {
        let until = Instant::now() + PATIENCE;
        let mut head = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {}\r\nAccept: {}\r\nAccept-Encoding: {}\r\n", self.port, self.token, self.wants, self.accept);
        if self.timing {
            head.push_str(ASK_TIMING);
        }
        if let Some(b) = body {
            head.push_str(&format!("Content-Type: application/json\r\nContent-Length: {}\r\n", b.len()));
        }
        head.push_str("\r\n");
        let reused = self.conn.is_some();
        match self.once(&head, body, until) {
            Ok(a) => Ok(a),
            // A kept connection the server closed meanwhile: once more on a new one.
            Err(e) if reused && e.downcast_ref::<Closed>().is_some() => {
                self.conn = None;
                self.once(&head, body, until)
            }
            Err(e) => {
                self.conn = None;
                Err(e)
            }
        }
    }

    /// [`Http::send`], asked again a second later while the server answers 503 (a kiln overloaded on purpose
    /// shedding it: goal 6, `crate::overload`, whose answers ask a second at least), for up to [`PATIENCE`]: what
    /// sets a round up must be answered, where the window's own requests are measured as they come, a shed one an
    /// error.
    pub fn send_settled(&mut self, method: &str, path: &str, body: Option<&[u8]>) -> anyhow::Result<Answer> {
        let until = std::time::Instant::now() + PATIENCE;
        loop {
            let a = self.send(method, path, body)?;
            if a.status != 503 || std::time::Instant::now() >= until {
                return Ok(a);
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    }

    fn once(&mut self, head: &str, body: Option<&[u8]>, until: Instant) -> anyhow::Result<Answer> {
        self.open(until)?;
        let Http { conn, buf, .. } = self;
        let s = conn.as_mut().ok_or_else(|| anyhow::anyhow!("no connection"))?;
        s.write_all(head.as_bytes()).map_err(|_| Closed)?;
        if let Some(b) = body {
            s.write_all(b).map_err(|_| Closed)?;
        }
        let (status, close, body, retry_after, server, arrived_us) = read_response(s, buf, until)?;
        if close {
            *conn = None;
        }
        Ok(Answer { status, body, retry_after, server, arrived_us })
    }
}

/// The server closed a kept connection before answering on it.
#[derive(Debug)]
struct Closed;

impl std::fmt::Display for Closed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the connection was closed")
    }
}

impl std::error::Error for Closed {}

/// Reads more into `buf`: how many bytes came (0: the peer closed), and when the last of them arrived at the socket
/// where it stamps arrivals (`crate::rx`), into `arrived`. Past `until` it reads nothing and is `TimedOut`, so what
/// keeps coming a little at a time can't keep a read going without end; a read interrupted is read again within it.
fn fill(s: &TcpStream, buf: &mut Vec<u8>, arrived: &mut Option<u64>, until: Instant) -> anyhow::Result<usize> {
    if Instant::now() >= until {
        return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, format!("nothing whole within {PATIENCE:?}")).into());
    }
    let mut chunk = [0u8; 64 << 10];
    let (n, at) = crate::rx::read(s, &mut chunk, until)?;
    buf.extend_from_slice(chunk.get(..n).unwrap_or_default());
    if at.is_some() {
        *arrived = at;
    }
    Ok(n)
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// One response off `s` (what's left over stays in `buf`): its status, whether the server closes
/// the connection after it, its body (chunks joined), its `Retry-After` in whole seconds, its
/// `Server-Timing` ([`ServerTime`]) and when the last of it arrived at the socket, where it stamps arrivals
/// (`crate::rx`): what was read of it before it came, whatever the read that brought it.
#[allow(clippy::type_complexity)]
fn read_response(s: &TcpStream, buf: &mut Vec<u8>, until: Instant) -> anyhow::Result<(u16, bool, Vec<u8>, Option<Duration>, Option<ServerTime>, Option<u64>)> {
    let mut arrived = None;
    let head_end = loop {
        if let Some(i) = find(buf, b"\r\n\r\n") {
            break i;
        }
        if fill(s, buf, &mut arrived, until)? == 0 {
            return Err(if buf.is_empty() { anyhow::Error::new(Closed) } else { anyhow::anyhow!("the connection ended in a response's head") });
        }
    };
    let head = String::from_utf8_lossy(buf.get(..head_end).unwrap_or_default()).into_owned();
    buf.drain(..head_end + 4);
    let status: u16 = head.split_whitespace().nth(1).and_then(|c| c.parse().ok()).ok_or_else(|| anyhow::anyhow!("no status in {head:?}"))?;
    let (mut length, mut retry_after, mut server) = (None, None, None);
    let (mut chunked, mut close) = (false, false);
    for line in head.lines().skip(1) {
        let Some((k, v)) = line.split_once(':') else { continue };
        let (k, v) = (k.trim().to_ascii_lowercase(), v.trim().to_ascii_lowercase());
        match k.as_str() {
            "content-length" => length = v.parse::<usize>().ok(),
            "transfer-encoding" => chunked = v.contains("chunked"),
            "connection" => close = v.contains("close"),
            "retry-after" => retry_after = v.parse::<u64>().ok().map(Duration::from_secs),
            "server-timing" => server = ServerTime::parse(&v),
            _ => {}
        }
    }
    let body = if chunked {
        let mut out = Vec::new();
        loop {
            let line_end = loop {
                if let Some(i) = find(buf, b"\r\n") {
                    break i;
                }
                anyhow::ensure!(fill(s, buf, &mut arrived, until)? > 0, "the connection ended in a chunk's size");
            };
            let size_text = String::from_utf8_lossy(buf.get(..line_end).unwrap_or_default()).into_owned();
            let size = usize::from_str_radix(size_text.split(';').next().unwrap_or("").trim(), 16)?;
            buf.drain(..line_end + 2);
            while buf.len() < size + 2 {
                anyhow::ensure!(fill(s, buf, &mut arrived, until)? > 0, "the connection ended in a chunk");
            }
            out.extend_from_slice(buf.get(..size).unwrap_or_default());
            buf.drain(..size + 2);
            if size == 0 {
                break;
            }
        }
        out
    } else if let Some(n) = length {
        while buf.len() < n {
            anyhow::ensure!(fill(s, buf, &mut arrived, until)? > 0, "the connection ended in a body");
        }
        buf.drain(..n).collect()
    } else if status == 204 || status == 304 || (100..200).contains(&status) {
        Vec::new()
    } else {
        // To the close.
        while fill(s, buf, &mut arrived, until)? > 0 {}
        close = true;
        std::mem::take(buf)
    };
    Ok((status, close, body, retry_after, server, arrived))
}

/// A WebSocket to the server (RFC 6455): frames out masked, as a client's are; and when what it read last arrived
/// at its socket, where the socket stamps arrivals (`crate::rx`).
pub struct Socket {
    s: TcpStream,
    buf: Vec<u8>,
    arrived: Option<u64>,
    /// How long a read waits before its reader can look up ([`Socket::open`]).
    tick: Duration,
}

/// What a socket brought.
pub enum Frame {
    Text(Vec<u8>),
    /// A binary message (a packed snapshot): only that it came.
    Binary,
    /// The read timed out with nothing whole: nothing came yet.
    Nothing,
    Closed,
}

impl Socket {
    /// A WebSocket to `path`, upgraded within [`PATIENCE`]; reads time out after `tick`, so its reader can look up.
    pub fn open(port: u16, token: &str, path: &str, tick: Duration) -> anyhow::Result<Self> {
        let until = Instant::now() + PATIENCE;
        let mut s = connect(port, until)?;
        write!(
            s,
            "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n"
        )?;
        let (mut buf, mut arrived) = (Vec::new(), None);
        let end = loop {
            if let Some(i) = find(&buf, b"\r\n\r\n") {
                break i;
            }
            anyhow::ensure!(fill(&s, &mut buf, &mut arrived, until)? > 0, "no answer to the upgrade");
            anyhow::ensure!(buf.len() < 64 << 10, "no end to the upgrade's answer");
        };
        let head = String::from_utf8_lossy(buf.get(..end).unwrap_or_default()).into_owned();
        anyhow::ensure!(head.starts_with("HTTP/1.1 101"), "not upgraded: {}", head.lines().next().unwrap_or(""));
        buf.drain(..end + 4);
        s.set_read_timeout(Some(tick))?;
        Ok(Self { s, buf, arrived, tick })
    }

    /// When the last bytes it read arrived at its socket, microseconds since the epoch, where the socket stamps
    /// arrivals (`crate::rx`): those of the message [`Socket::next`] gave last, which came whole with them or before
    /// them (goal 10, plan 01a1a87b2dae step 5).
    pub fn arrived(&self) -> Option<u64> {
        self.arrived
    }

    /// The next whole message (continuations joined; pings answered), or `Nothing` once a read
    /// times out with none whole. A message begun is waited for up to [`PATIENCE`] in all, not
    /// without end where its rest never comes.
    pub fn next(&mut self) -> anyhow::Result<Frame> {
        let mut message: Option<(u8, Vec<u8>)> = None;
        let begun = Instant::now();
        loop {
            while let Some((fin, op, payload)) = self.take()? {
                match op {
                    0x9 => self.send(0xA, &payload)?,
                    0xA => {}
                    0x8 => return Ok(Frame::Closed),
                    0x0 => {
                        if let Some((_, m)) = message.as_mut() {
                            m.extend_from_slice(&payload);
                        }
                    }
                    op => message = Some((op, payload)),
                }
                if fin && let Some((op, m)) = message.take() {
                    return Ok(if op == 0x2 { Frame::Binary } else { Frame::Text(m) });
                }
            }
            match fill(&self.s, &mut self.buf, &mut self.arrived, Instant::now() + self.tick) {
                Ok(0) => return Ok(Frame::Closed),
                Ok(_) => {}
                Err(e) if e.downcast_ref::<std::io::Error>().is_some_and(|e| matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut)) => {
                    if message.is_none() {
                        return Ok(Frame::Nothing);
                    }
                    anyhow::ensure!(begun.elapsed() < PATIENCE, "a message's rest didn't come within {PATIENCE:?}");
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// A whole frame off the buffer, if one is there: its FIN bit, opcode and payload.
    fn take(&mut self) -> anyhow::Result<Option<(bool, u8, Vec<u8>)>> {
        let (Some(&b0), Some(&b1)) = (self.buf.first(), self.buf.get(1)) else { return Ok(None) };
        let (mut at, len) = match b1 & 0x7f {
            126 => match self.buf.get(2..4) {
                Some(b) => (4usize, b.iter().fold(0usize, |a, x| (a << 8) | usize::from(*x))),
                None => return Ok(None),
            },
            127 => match self.buf.get(2..10) {
                Some(b) => (10usize, usize::try_from(b.iter().fold(0u64, |a, x| (a << 8) | u64::from(*x)))?),
                None => return Ok(None),
            },
            n => (2usize, usize::from(n)),
        };
        // The server's frames are never masked; one that is, is read past its key.
        if b1 & 0x80 != 0 {
            at += 4;
        }
        if self.buf.len() < at + len {
            return Ok(None);
        }
        let payload = self.buf.get(at..at + len).map(<[u8]>::to_vec).unwrap_or_default();
        self.buf.drain(..at + len);
        Ok(Some((b0 & 0x80 != 0, b0 & 0x0f, payload)))
    }

    /// A frame of `op` with `payload`, masked.
    pub fn send(&mut self, op: u8, payload: &[u8]) -> anyhow::Result<()> {
        let mut f = vec![0x80 | op];
        match payload.len() {
            n if n < 126 => f.push(0x80 | u8::try_from(n)?),
            n if n < 1 << 16 => {
                f.push(0x80 | 126);
                f.extend(u16::try_from(n)?.to_be_bytes());
            }
            n => {
                f.push(0x80 | 127);
                f.extend(u64::try_from(n)?.to_be_bytes());
            }
        }
        let mask = [0x37u8, 0xfa, 0x21, 0x3d];
        f.extend(mask);
        f.extend(payload.iter().zip(mask.iter().cycle()).map(|(b, m)| b ^ m));
        self.s.write_all(&f)?;
        Ok(())
    }

    /// Says goodbye (a close frame), and lets the socket go.
    pub fn close(mut self) {
        let _ = self.send(0x8, &[0x03, 0xe8]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read as _;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Goal 1, plan 01a1a59c2a68 step 3: a request through [`connect`]'s connection is asked and answered as before.
    #[test]
    fn g1_s3_a_request_is_answered_within_its_deadline() -> anyhow::Result<()> {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
        let port = listener.local_addr()?.port();
        let server = std::thread::spawn(move || -> std::io::Result<Vec<u8>> {
            let (mut s, _) = listener.accept()?;
            let (mut head, mut chunk) = (Vec::new(), [0u8; 1024]);
            while find(&head, b"\r\n\r\n").is_none() {
                let n = s.read(&mut chunk)?;
                if n == 0 {
                    break;
                }
                head.extend_from_slice(chunk.get(..n).unwrap_or_default());
            }
            s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")?;
            Ok(head)
        });
        let a = Http::new(port, "lab").send("GET", "/api/me", None)?;
        let head = server.join().map_err(|_| anyhow::anyhow!("the server panicked"))??;
        anyhow::ensure!((a.status, a.body.as_slice()) == (200, &b"ok"[..]), "{} {:?}", a.status, a.body);
        anyhow::ensure!(head.starts_with(b"GET /api/me HTTP/1.1\r\n"), "{:?}", String::from_utf8_lossy(&head));
        Ok(())
    }

    /// Goal 1, plan 01a1a59c2a68 step 3: an answer that keeps coming a byte at a time and never ends is given up at the
    /// request's deadline, `TimedOut`, rather than read on as long as bytes come (each read's own timeout starts over).
    #[test]
    fn g1_s3_an_answer_that_trickles_ends_at_its_deadline() -> anyhow::Result<()> {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
        let client = TcpStream::connect(listener.local_addr()?)?;
        let (mut server, _) = listener.accept()?;
        client.set_read_timeout(Some(Duration::from_secs(5)))?;
        let done = Arc::new(AtomicBool::new(false));
        let trickling = Arc::clone(&done);
        let trickle = std::thread::spawn(move || {
            let _ = server.write_all(b"HTTP/1.1 200 OK\r\n");
            // A header a byte at a time, never its end, until the client gives up (or a minute: the test then fails).
            let t = Instant::now();
            while !trickling.load(Ordering::Relaxed) && t.elapsed() < Duration::from_secs(60) {
                if server.write_all(b"x").is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        });
        let started = Instant::now();
        let mut buf = Vec::new();
        let got = read_response(&client, &mut buf, started + Duration::from_millis(300));
        let took = started.elapsed();
        done.store(true, Ordering::Relaxed);
        let _ = trickle.join();
        let e = got.err().ok_or_else(|| anyhow::anyhow!("a trickled answer was read whole"))?;
        anyhow::ensure!(e.downcast_ref::<std::io::Error>().is_some_and(|e| e.kind() == std::io::ErrorKind::TimedOut), "{e:#}");
        anyhow::ensure!(took < Duration::from_secs(10), "it read on for {took:?}");
        Ok(())
    }
}
