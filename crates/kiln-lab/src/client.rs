//! The lab's clients, on blocking sockets: HTTP/1.1 with keep-alive, and a WebSocket. Each worker
//! of the load owns its own, so nothing is shared while the lab measures.

use std::io::{Read as _, Write as _};
use std::net::TcpStream;
use std::time::Duration;

/// How long a request or a frame may take before the lab gives up on it: past any stall the lab
/// means to see (a minute: `kiln_route_seconds`' last bucket), so a stall is measured, not cut.
pub const PATIENCE: Duration = Duration::from_secs(90);

/// An answer: its status and body.
pub struct Answer {
    pub status: u16,
    pub body: Vec<u8>,
}

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
}

impl Http {
    pub fn new(port: u16, token: &str) -> Self {
        Self { port, token: token.to_owned(), conn: None, buf: Vec::new(), accept: "identity", wants: "*/*" }
    }

    /// One that asks as a browser loading pages does: a page, packed (Chromium's `Accept` for a
    /// navigation).
    pub fn browser(port: u16, token: &str) -> Self {
        Self { accept: "br, gzip", wants: "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8", ..Self::new(port, token) }
    }

    fn open(&mut self) -> anyhow::Result<()> {
        if self.conn.is_none() {
            let s = TcpStream::connect(("127.0.0.1", self.port))?;
            s.set_nodelay(true)?;
            s.set_read_timeout(Some(PATIENCE))?;
            self.buf.clear();
            self.conn = Some(s);
        }
        Ok(())
    }

    /// `method path` with `body` (JSON) when there is one: the answer. A connection the server had
    /// closed is opened again once.
    pub fn send(&mut self, method: &str, path: &str, body: Option<&[u8]>) -> anyhow::Result<Answer> {
        let mut head = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {}\r\nAccept: {}\r\nAccept-Encoding: {}\r\n", self.port, self.token, self.wants, self.accept);
        if let Some(b) = body {
            head.push_str(&format!("Content-Type: application/json\r\nContent-Length: {}\r\n", b.len()));
        }
        head.push_str("\r\n");
        let reused = self.conn.is_some();
        match self.once(&head, body) {
            Ok(a) => Ok(a),
            // A kept connection the server closed meanwhile: once more on a new one.
            Err(e) if reused && e.downcast_ref::<Closed>().is_some() => {
                self.conn = None;
                self.once(&head, body)
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

    fn once(&mut self, head: &str, body: Option<&[u8]>) -> anyhow::Result<Answer> {
        self.open()?;
        let Http { conn, buf, .. } = self;
        let s = conn.as_mut().ok_or_else(|| anyhow::anyhow!("no connection"))?;
        s.write_all(head.as_bytes()).map_err(|_| Closed)?;
        if let Some(b) = body {
            s.write_all(b).map_err(|_| Closed)?;
        }
        let (status, close, body) = read_response(s, buf)?;
        if close {
            *conn = None;
        }
        Ok(Answer { status, body })
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

/// Reads more into `buf`: how many bytes came (0: the peer closed).
fn fill(s: &mut TcpStream, buf: &mut Vec<u8>) -> anyhow::Result<usize> {
    let mut chunk = [0u8; 64 << 10];
    let n = s.read(&mut chunk)?;
    buf.extend_from_slice(chunk.get(..n).unwrap_or_default());
    Ok(n)
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// One response off `s` (what's left over stays in `buf`): its status, whether the server closes
/// the connection after it, and its body (chunks joined).
fn read_response(s: &mut TcpStream, buf: &mut Vec<u8>) -> anyhow::Result<(u16, bool, Vec<u8>)> {
    let head_end = loop {
        if let Some(i) = find(buf, b"\r\n\r\n") {
            break i;
        }
        if fill(s, buf)? == 0 {
            return Err(if buf.is_empty() { anyhow::Error::new(Closed) } else { anyhow::anyhow!("the connection ended in a response's head") });
        }
    };
    let head = String::from_utf8_lossy(buf.get(..head_end).unwrap_or_default()).into_owned();
    buf.drain(..head_end + 4);
    let status: u16 = head.split_whitespace().nth(1).and_then(|c| c.parse().ok()).ok_or_else(|| anyhow::anyhow!("no status in {head:?}"))?;
    let mut length = None;
    let (mut chunked, mut close) = (false, false);
    for line in head.lines().skip(1) {
        let Some((k, v)) = line.split_once(':') else { continue };
        let (k, v) = (k.trim().to_ascii_lowercase(), v.trim().to_ascii_lowercase());
        match k.as_str() {
            "content-length" => length = v.parse::<usize>().ok(),
            "transfer-encoding" => chunked = v.contains("chunked"),
            "connection" => close = v.contains("close"),
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
                anyhow::ensure!(fill(s, buf)? > 0, "the connection ended in a chunk's size");
            };
            let size_text = String::from_utf8_lossy(buf.get(..line_end).unwrap_or_default()).into_owned();
            let size = usize::from_str_radix(size_text.split(';').next().unwrap_or("").trim(), 16)?;
            buf.drain(..line_end + 2);
            while buf.len() < size + 2 {
                anyhow::ensure!(fill(s, buf)? > 0, "the connection ended in a chunk");
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
            anyhow::ensure!(fill(s, buf)? > 0, "the connection ended in a body");
        }
        buf.drain(..n).collect()
    } else if status == 204 || status == 304 || (100..200).contains(&status) {
        Vec::new()
    } else {
        // To the close.
        while fill(s, buf)? > 0 {}
        close = true;
        std::mem::take(buf)
    };
    Ok((status, close, body))
}

/// A WebSocket to the server (RFC 6455): frames out masked, as a client's are.
pub struct Socket {
    s: TcpStream,
    buf: Vec<u8>,
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
    /// A WebSocket to `path`, upgraded; reads time out after `tick`, so its reader can look up.
    pub fn open(port: u16, token: &str, path: &str, tick: Duration) -> anyhow::Result<Self> {
        let mut s = TcpStream::connect(("127.0.0.1", port))?;
        s.set_nodelay(true)?;
        s.set_read_timeout(Some(PATIENCE))?;
        write!(
            s,
            "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n"
        )?;
        let mut buf = Vec::new();
        let end = loop {
            if let Some(i) = find(&buf, b"\r\n\r\n") {
                break i;
            }
            anyhow::ensure!(fill(&mut s, &mut buf)? > 0, "no answer to the upgrade");
            anyhow::ensure!(buf.len() < 64 << 10, "no end to the upgrade's answer");
        };
        let head = String::from_utf8_lossy(buf.get(..end).unwrap_or_default()).into_owned();
        anyhow::ensure!(head.starts_with("HTTP/1.1 101"), "not upgraded: {}", head.lines().next().unwrap_or(""));
        buf.drain(..end + 4);
        s.set_read_timeout(Some(tick))?;
        Ok(Self { s, buf })
    }

    /// The next whole message (continuations joined; pings answered), or `Nothing` once a read
    /// times out with none whole.
    pub fn next(&mut self) -> anyhow::Result<Frame> {
        let mut message: Option<(u8, Vec<u8>)> = None;
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
            match fill(&mut self.s, &mut self.buf) {
                Ok(0) => return Ok(Frame::Closed),
                Ok(_) => {}
                Err(e) if e.downcast_ref::<std::io::Error>().is_some_and(|e| matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut)) => {
                    if message.is_none() {
                        return Ok(Frame::Nothing);
                    }
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
