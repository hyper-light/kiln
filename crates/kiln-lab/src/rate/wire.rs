//! What goes over the rate mode's sockets: HTTP/1.1 answers read as their bytes come (bodies counted, not kept),
//! WebSocket frames both ways (RFC 6455), and the few values an RPC answer is matched by, picked out of its JSON
//! without building it: at a hundred thousand answers a second the generator must not spend the CPU the server
//! is being measured by.

// ------------------------------------------------------------------------------------- HTTP/1.1

/// A head past this is refused: kiln's own server's limit (docs/http.md, "A head over 1 MiB (kiln's setting)").
const MAX_HEAD: usize = 1 << 20;

/// An answer read whole: its status, its body's bytes, and whether the server closes the connection after it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Answer {
    pub status: u16,
    pub bytes: u64,
    pub close: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Head,
    /// A body by its length: what's left of it.
    Body(u64),
    /// A chunk's size line.
    Size,
    /// A chunk's data: what's left of it, then its CRLF.
    Chunk(u64),
    ChunkEnd(u8),
    /// The trailer after the last chunk, a line at a time to the empty one.
    Trailer,
    /// A body to the connection's end.
    ToClose,
}

/// One connection's answers, read as their bytes come, in order (HTTP/1.1 answers in the order asked).
#[derive(Debug)]
pub struct H1 {
    state: State,
    /// A head, a chunk's size line or a trailer line not yet whole.
    line: Vec<u8>,
    status: u16,
    close: bool,
    bytes: u64,
    /// What it asked was `HEAD`: its answers carry no body.
    head_only: bool,
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// A head's status, its length (none: to the close, or chunked), whether it's chunked, and whether the server
/// closes after it.
fn read_head(head: &[u8]) -> Result<(u16, Option<u64>, bool, bool), String> {
    let text = String::from_utf8_lossy(head);
    let mut lines = text.split("\r\n");
    let first = lines.next().unwrap_or("");
    let mut words = first.split(' ');
    let version = words.next().unwrap_or("");
    let status: u16 = words.next().and_then(|s| s.parse().ok()).ok_or_else(|| format!("no status in {:?}", first.chars().take(64).collect::<String>()))?;
    let (mut says_close, mut says_keep) = (false, false);
    let (mut length, mut chunked) = (None, false);
    for line in lines {
        let Some((k, v)) = line.split_once(':') else { continue };
        let (k, v) = (k.trim(), v.trim());
        if k.eq_ignore_ascii_case("content-length") {
            length = Some(v.parse::<u64>().map_err(|_| format!("a content-length of {v:?}"))?);
        } else if k.eq_ignore_ascii_case("transfer-encoding") {
            chunked = v.to_ascii_lowercase().split(',').any(|c| c.trim() == "chunked");
        } else if k.eq_ignore_ascii_case("connection") {
            let v = v.to_ascii_lowercase();
            says_close |= v.split(',').any(|c| c.trim() == "close");
            says_keep |= v.split(',').any(|c| c.trim() == "keep-alive");
        }
    }
    // HTTP/1.1 keeps the connection unless it says close; HTTP/1.0 closes it unless it says keep-alive.
    let close = if version == "HTTP/1.0" { !says_keep } else { says_close };
    Ok((status, length, chunked, close))
}

impl H1 {
    pub fn new(head_only: bool) -> Self {
        Self { state: State::Head, line: Vec::new(), status: 0, close: false, bytes: 0, head_only }
    }

    /// Whether it's between answers (nothing of one read yet).
    #[cfg(test)]
    pub fn between(&self) -> bool {
        self.state == State::Head && self.line.is_empty()
    }

    fn answer(&mut self, out: &mut Vec<Answer>) {
        out.push(Answer { status: self.status, bytes: self.bytes, close: self.close });
        self.state = State::Head;
        self.bytes = 0;
    }

    /// A head read whole: what follows it.
    fn headed(&mut self, head: &[u8], out: &mut Vec<Answer>) -> Result<(), String> {
        let (status, length, chunked, close) = read_head(head)?;
        (self.status, self.close, self.bytes) = (status, close, 0);
        if (100..200).contains(&status) {
            // An interim answer (100 Continue): the real one follows.
            return Ok(());
        }
        if self.head_only || status == 204 || status == 304 {
            self.answer(out);
        } else if chunked {
            self.state = State::Size;
        } else {
            match length {
                Some(0) => self.answer(out),
                Some(n) => self.state = State::Body(n),
                None => self.state = State::ToClose,
            }
        }
        Ok(())
    }

    /// A line (to its CRLF) out of `data`, kept in `self.line` until whole: the line, without its CRLF, once
    /// whole, and what of `data` is left.
    fn line_of<'a>(&mut self, data: &'a [u8], end: &[u8]) -> Result<(Option<Vec<u8>>, &'a [u8]), String> {
        let before = self.line.len();
        if before == 0
            && let Some(i) = find(data, end)
        {
            return Ok((Some(data.get(..i).unwrap_or_default().to_vec()), data.get(i + end.len()..).unwrap_or_default()));
        }
        // Across reads: only up to where it may end is kept.
        let take = data.len().min(MAX_HEAD.saturating_sub(before) + end.len());
        self.line.extend_from_slice(data.get(..take).unwrap_or_default());
        let from = before.saturating_sub(end.len() - 1);
        match find(self.line.get(from..).unwrap_or_default(), end) {
            Some(i) => {
                let stop = from + i;
                let used = stop + end.len() - before;
                let mut whole = std::mem::take(&mut self.line);
                whole.truncate(stop);
                Ok((Some(whole), data.get(used..).unwrap_or_default()))
            }
            None if self.line.len() > MAX_HEAD => Err(format!("no end to a head or line in {} bytes", self.line.len())),
            None => Ok((None, data.get(take..).unwrap_or_default())),
        }
    }

    /// Reads `data`, the next bytes off the connection: each answer they finish, in order.
    pub fn feed(&mut self, mut data: &[u8], out: &mut Vec<Answer>) -> Result<(), String> {
        while !data.is_empty() {
            match self.state {
                State::Head => {
                    let (head, rest) = self.line_of(data, b"\r\n\r\n")?;
                    data = rest;
                    if let Some(h) = head {
                        self.headed(&h, out)?;
                    }
                }
                State::Body(left) => {
                    let n = usize::try_from(left).unwrap_or(usize::MAX).min(data.len());
                    self.bytes += n as u64;
                    data = data.get(n..).unwrap_or_default();
                    if left == n as u64 {
                        self.answer(out);
                    } else {
                        self.state = State::Body(left - n as u64);
                    }
                }
                State::Chunk(left) => {
                    let n = usize::try_from(left).unwrap_or(usize::MAX).min(data.len());
                    self.bytes += n as u64;
                    data = data.get(n..).unwrap_or_default();
                    self.state = if left == n as u64 { State::ChunkEnd(2) } else { State::Chunk(left - n as u64) };
                }
                State::ToClose => {
                    self.bytes += data.len() as u64;
                    data = &[];
                }
                State::ChunkEnd(left) => {
                    let n = usize::from(left).min(data.len());
                    data = data.get(n..).unwrap_or_default();
                    let left = left - u8::try_from(n).unwrap_or(left);
                    self.state = if left == 0 { State::Size } else { State::ChunkEnd(left) };
                }
                State::Size => {
                    let (line, rest) = self.line_of(data, b"\r\n")?;
                    data = rest;
                    if let Some(l) = line {
                        let text = String::from_utf8_lossy(&l);
                        let hex = text.split(';').next().unwrap_or("").trim();
                        let size = u64::from_str_radix(hex, 16).map_err(|_| format!("a chunk size of {hex:?}"))?;
                        self.state = if size == 0 { State::Trailer } else { State::Chunk(size) };
                    }
                }
                State::Trailer => {
                    let (line, rest) = self.line_of(data, b"\r\n")?;
                    data = rest;
                    if line.is_some_and(|l| l.is_empty()) {
                        self.answer(out);
                    }
                }
            }
        }
        Ok(())
    }

    /// The connection ended: an answer read to its close ends with it.
    pub fn eof(&mut self, out: &mut Vec<Answer>) {
        if self.state == State::ToClose {
            self.close = true;
            self.answer(out);
        }
    }
}

/// A request's bytes: `method path` with `headers` (each a name and its value) and `body`, kept alive.
pub fn request(method: &str, path: &str, host: &str, headers: &[(String, String)], body: Option<&[u8]>) -> Vec<u8> {
    let mut out = Vec::with_capacity(128 + path.len() + body.map_or(0, <[u8]>::len));
    out.extend_from_slice(format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\n").as_bytes());
    for (k, v) in headers {
        out.extend_from_slice(format!("{k}: {v}\r\n").as_bytes());
    }
    if let Some(b) = body {
        out.extend_from_slice(format!("Content-Length: {}\r\n\r\n", b.len()).as_bytes());
        out.extend_from_slice(b);
    } else {
        out.extend_from_slice(b"\r\n");
    }
    out
}

// ------------------------------------------------------------------------------------- WebSockets

/// The key a client sends to upgrade (RFC 6455's own example: servers check its form, not its value).
pub const WS_KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";

/// The upgrade's request for `path`, with `headers`.
pub fn upgrade(path: &str, host: &str, headers: &[(String, String)]) -> Vec<u8> {
    let mut h: Vec<(String, String)> =
        vec![("Upgrade".into(), "websocket".into()), ("Connection".into(), "Upgrade".into()), ("Sec-WebSocket-Key".into(), WS_KEY.into()), ("Sec-WebSocket-Version".into(), "13".into())];
    h.extend(headers.iter().cloned());
    request("GET", path, host, &h, None)
}

/// A frame of `op` with `payload`, masked as a client's are, onto `out`.
pub fn frame(op: u8, payload: &[u8], mask: [u8; 4], out: &mut Vec<u8>) {
    out.push(0x80 | op);
    match payload.len() {
        n if n < 126 => out.push(0x80 | u8::try_from(n).unwrap_or(0)),
        n if n <= 0xffff => {
            out.push(0x80 | 126);
            out.extend_from_slice(&u16::try_from(n).unwrap_or(0).to_be_bytes());
        }
        n => {
            out.push(0x80 | 127);
            out.extend_from_slice(&(n as u64).to_be_bytes());
        }
    }
    out.extend_from_slice(&mask);
    out.extend(payload.iter().zip(mask.iter().cycle()).map(|(b, m)| b ^ m));
}

/// What a socket brought.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WsIn {
    /// A whole text message.
    Text(Vec<u8>),
    /// A whole binary message: its length.
    Binary(u64),
    /// A ping, to be answered with its payload.
    Ping(Vec<u8>),
    Close,
}

/// A message larger than this is refused rather than kept (a gibibyte: no read the lab times comes near it).
const MAX_MESSAGE: u64 = 1 << 30;

/// One socket's frames, read as their bytes come.
#[derive(Debug, Default)]
pub struct Ws {
    buf: Vec<u8>,
    at: usize,
    message: Option<(u8, Vec<u8>)>,
}

impl Ws {
    /// Reads `data`: each message, ping and close it finishes, in order.
    pub fn feed(&mut self, data: &[u8], out: &mut Vec<WsIn>) -> Result<(), String> {
        self.buf.extend_from_slice(data);
        loop {
            let avail = self.buf.get(self.at..).unwrap_or_default();
            let (Some(&b0), Some(&b1)) = (avail.first(), avail.get(1)) else { break };
            let (mut head, len) = match b1 & 0x7f {
                126 => match avail.get(2..4) {
                    Some(b) => (4usize, b.iter().fold(0u64, |a, x| (a << 8) | u64::from(*x))),
                    None => break,
                },
                127 => match avail.get(2..10) {
                    Some(b) => (10usize, b.iter().fold(0u64, |a, x| (a << 8) | u64::from(*x))),
                    None => break,
                },
                n => (2usize, u64::from(n)),
            };
            if len > MAX_MESSAGE {
                return Err(format!("a frame of {len} bytes"));
            }
            let mask = if b1 & 0x80 != 0 {
                let Some(m) = avail.get(head..head + 4) else { break };
                head += 4;
                Some([m.first().copied().unwrap_or(0), m.get(1).copied().unwrap_or(0), m.get(2).copied().unwrap_or(0), m.get(3).copied().unwrap_or(0)])
            } else {
                None
            };
            let len = usize::try_from(len).map_err(|_| "a frame too large to hold".to_owned())?;
            let Some(payload) = avail.get(head..head + len) else { break };
            let mut payload = payload.to_vec();
            if let Some(m) = mask {
                for (b, k) in payload.iter_mut().zip(m.iter().cycle()) {
                    *b ^= k;
                }
            }
            self.at += head + len;
            let (fin, op) = (b0 & 0x80 != 0, b0 & 0x0f);
            match op {
                0x8 => out.push(WsIn::Close),
                0x9 => out.push(WsIn::Ping(payload)),
                0xA => {}
                0x0 => {
                    let Some((_, m)) = self.message.as_mut() else { return Err("a continuation with no message".into()) };
                    m.extend_from_slice(&payload);
                    if m.len() as u64 > MAX_MESSAGE {
                        return Err("a message past a gibibyte".into());
                    }
                }
                0x1 | 0x2 => self.message = Some((op, payload)),
                op => return Err(format!("a frame of opcode {op}")),
            }
            if fin
                && matches!(op, 0x0..=0x2)
                && let Some((op, m)) = self.message.take()
            {
                out.push(if op == 0x1 { WsIn::Text(m) } else { WsIn::Binary(m.len() as u64) });
            }
        }
        // What's been read goes once it's most of the buffer, so a long run of small frames costs no copying.
        if self.at > 0 && (self.at == self.buf.len() || self.at >= self.buf.len() / 2) {
            self.buf.drain(..self.at);
            self.at = 0;
        }
        Ok(())
    }
}

// ------------------------------------------------------------------------------------- JSON

/// A JSON pointer's reference tokens (RFC 6901): `/a/0/b` is `a`, `0`, `b`; `~1` is `/` and `~0` is `~`.
pub fn pointer(p: &str) -> Result<Vec<String>, String> {
    if p.is_empty() {
        return Ok(Vec::new());
    }
    let rest = p.strip_prefix('/').ok_or_else(|| format!("a JSON pointer starts with /: {p:?}"))?;
    Ok(rest.split('/').map(|t| t.replace("~1", "/").replace("~0", "~")).collect())
}

/// What a pointer reached.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Found {
    /// A string's contents, or a number, `true`, `false` or `null` as written.
    Scalar(String),
    /// An object or an array.
    Container,
}

struct Scan<'a> {
    s: &'a [u8],
    i: usize,
}

impl Scan<'_> {
    fn ws(&mut self) {
        while matches!(self.s.get(self.i), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.i += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    /// Past a string at its opening quote: where its contents begin and end.
    fn string(&mut self) -> Option<(usize, usize)> {
        if self.peek()? != b'"' {
            return None;
        }
        let start = self.i + 1;
        self.i = start;
        loop {
            match self.peek()? {
                b'"' => {
                    self.i += 1;
                    return Some((start, self.i - 1));
                }
                b'\\' => self.i += 2,
                _ => self.i += 1,
            }
        }
    }

    /// A string at its opening quote, its escapes undone.
    fn text(&mut self) -> Option<String> {
        let (a, b) = self.string()?;
        let raw = self.s.get(a..b)?;
        if !raw.contains(&b'\\') {
            return Some(String::from_utf8_lossy(raw).into_owned());
        }
        let mut out: Vec<u8> = Vec::with_capacity(raw.len());
        let mut it = raw.iter().copied();
        let mut high: Option<u32> = None;
        while let Some(c) = it.next() {
            if c != b'\\' {
                out.push(c);
                continue;
            }
            let ch = match it.next()? {
                b'n' => '\n',
                b'r' => '\r',
                b't' => '\t',
                b'b' => '\u{8}',
                b'f' => '\u{c}',
                b'u' => {
                    let hex: String = (0..4).filter_map(|_| it.next().map(char::from)).collect();
                    let u = u32::from_str_radix(&hex, 16).ok()?;
                    if (0xd800..0xdc00).contains(&u) {
                        high = Some(u);
                        continue;
                    }
                    let u = match high.take() {
                        Some(h) if (0xdc00..0xe000).contains(&u) => 0x10000 + ((h - 0xd800) << 10) + (u - 0xdc00),
                        _ => u,
                    };
                    char::from_u32(u).unwrap_or('\u{fffd}')
                }
                other => char::from(other),
            };
            let mut b = [0u8; 4];
            out.extend_from_slice(ch.encode_utf8(&mut b).as_bytes());
        }
        Some(String::from_utf8_lossy(&out).into_owned())
    }

    /// Past any value, nested ones by counting brackets (no recursion, so no depth is too deep).
    fn skip(&mut self) -> Option<()> {
        self.ws();
        match self.peek()? {
            b'"' => {
                self.string()?;
            }
            b'{' | b'[' => {
                let mut depth = 0usize;
                loop {
                    match self.peek()? {
                        b'"' => {
                            self.string()?;
                            continue;
                        }
                        b'{' | b'[' => depth += 1,
                        b'}' | b']' => {
                            depth = depth.checked_sub(1)?;
                            if depth == 0 {
                                self.i += 1;
                                return Some(());
                            }
                        }
                        _ => {}
                    }
                    self.i += 1;
                }
            }
            _ => {
                let start = self.i;
                while self.peek().is_some_and(|c| !matches!(c, b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r')) {
                    self.i += 1;
                }
                if self.i == start {
                    return None;
                }
            }
        }
        Some(())
    }

    /// A scalar as text.
    fn scalar(&mut self) -> Option<String> {
        if self.peek()? == b'"' {
            return self.text();
        }
        let start = self.i;
        self.skip()?;
        Some(String::from_utf8_lossy(self.s.get(start..self.i)?).into_owned())
    }

    /// Walks the value here at `path`, descending only where a wanted pointer goes, filling `out`.
    fn walk(&mut self, path: &mut Vec<String>, wants: &[&[String]], out: &mut [Option<Found>]) -> Option<()> {
        self.ws();
        let depth = path.len();
        let on = |w: &&[String]| w.len() >= depth && w.get(..depth) == Some(path.as_slice());
        if !wants.iter().any(on) {
            return self.skip();
        }
        let ends: Vec<usize> = wants.iter().enumerate().filter(|(_, w)| w.len() == depth && on(*w)).map(|(i, _)| i).collect();
        let deeper = wants.iter().any(|w| w.len() > depth && on(w));
        match self.peek()? {
            open @ (b'{' | b'[') => {
                for i in &ends {
                    if let Some(o) = out.get_mut(*i) {
                        *o = Some(Found::Container);
                    }
                }
                if !deeper {
                    return self.skip();
                }
                let close = if open == b'{' { b'}' } else { b']' };
                self.i += 1;
                self.ws();
                if self.peek()? == close {
                    self.i += 1;
                    return Some(());
                }
                let mut index = 0usize;
                loop {
                    self.ws();
                    let key = if open == b'{' {
                        let k = self.text()?;
                        self.ws();
                        if self.peek()? != b':' {
                            return None;
                        }
                        self.i += 1;
                        k
                    } else {
                        index += 1;
                        (index - 1).to_string()
                    };
                    path.push(key);
                    self.walk(path, wants, out)?;
                    path.pop();
                    // Everything wanted is found: the rest (a snapshot of a megabyte, say) needn't be read.
                    if out.iter().all(Option::is_some) {
                        return Some(());
                    }
                    self.ws();
                    match self.peek()? {
                        b',' => self.i += 1,
                        c if c == close => {
                            self.i += 1;
                            return Some(());
                        }
                        _ => return None,
                    }
                }
            }
            _ => {
                let text = self.scalar()?;
                for i in ends {
                    if let Some(o) = out.get_mut(i) {
                        *o = Some(Found::Scalar(text.clone()));
                    }
                }
                Some(())
            }
        }
    }
}

/// The messages in a text message: its elements where it's an array (a batch), else itself.
pub fn messages(s: &[u8]) -> Vec<&[u8]> {
    let mut p = Scan { s, i: 0 };
    p.ws();
    if p.peek() != Some(b'[') {
        return vec![s];
    }
    p.i += 1;
    let mut out = Vec::new();
    loop {
        p.ws();
        if p.peek() == Some(b']') || p.peek().is_none() {
            return out;
        }
        let start = p.i;
        if p.skip().is_none() {
            return vec![s];
        }
        out.push(s.get(start..p.i).unwrap_or_default());
        p.ws();
        match p.peek() {
            Some(b',') => p.i += 1,
            _ => return out,
        }
    }
}

/// What each of `pointers` reaches in the JSON value `s`, in one pass that builds nothing but those; none where a
/// pointer reaches nothing (or `s` isn't JSON as far as it was read).
pub fn pick(s: &[u8], pointers: &[&[String]]) -> Vec<Option<Found>> {
    let mut out = vec![None; pointers.len()];
    let mut p = Scan { s, i: 0 };
    let _ = p.walk(&mut Vec::new(), pointers, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_in(h: &mut H1, bytes: &[u8], step: usize) -> Result<Vec<Answer>, String> {
        let mut out = Vec::new();
        for chunk in bytes.chunks(step.max(1)) {
            h.feed(chunk, &mut out)?;
        }
        Ok(out)
    }

    /// Goal 10 (Ada's fixed-rate benchmark): answers by length, chunked (with a trailer), with no body, and
    /// pipelined back to back, read whole however the bytes are split, their bodies counted.
    #[test]
    fn rate_http_answers_are_read_whole_however_their_bytes_are_split() {
        let stream: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\n\r\n{\"a\":[1,2]}\
HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5;x=y\r\nhello\r\n0006\r\n world\r\n0\r\nTrailer: t\r\n\r\n\
HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 204 No Content\r\n\r\n\
HTTP/1.1 503 Service Unavailable\r\ncontent-length: 2\r\nconnection: close\r\n\r\nno";
        let want = vec![
            Answer { status: 200, bytes: 11, close: false },
            Answer { status: 200, bytes: 11, close: false },
            Answer { status: 204, bytes: 0, close: false },
            Answer { status: 503, bytes: 2, close: true },
        ];
        for step in [1, 2, 3, 7, 64, stream.len()] {
            let mut h = H1::new(false);
            assert_eq!(feed_in(&mut h, stream, step), Ok(want.clone()), "read {step} bytes at a time");
            assert!(h.between());
        }
        // A body to the close ends with the connection.
        let mut h = H1::new(false);
        let mut out = feed_in(&mut h, b"HTTP/1.0 200 OK\r\n\r\nall of it", 4).unwrap_or_default();
        assert!(out.is_empty());
        h.eof(&mut out);
        assert_eq!(out, vec![Answer { status: 200, bytes: 9, close: true }]);
        // A HEAD's answer has no body whatever its length says.
        let mut h = H1::new(true);
        assert_eq!(feed_in(&mut h, b"HTTP/1.1 200 OK\r\nContent-Length: 99\r\n\r\n", 5), Ok(vec![Answer { status: 200, bytes: 0, close: false }]));
        // Garbage is an error, not a panic.
        let mut h = H1::new(false);
        assert!(h.feed(b"HTTP/1.1 abc\r\n\r\n", &mut Vec::new()).is_err());
        assert!(H1::new(false).feed(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n", &mut Vec::new()).is_err());
    }

    /// Goal 10 (Ada's fixed-rate benchmark): a request is its line, its Host, its headers and its body with its
    /// length; an upgrade asks for a WebSocket.
    #[test]
    fn rate_requests_are_written_as_asked() {
        let r = request("POST", "/api/x?y=1", "127.0.0.1:9", &[("Authorization".into(), "Bearer t".into())], Some(b"{}"));
        assert_eq!(String::from_utf8_lossy(&r), "POST /api/x?y=1 HTTP/1.1\r\nHost: 127.0.0.1:9\r\nAuthorization: Bearer t\r\nContent-Length: 2\r\n\r\n{}");
        let u = String::from_utf8_lossy(&upgrade("/ws", "127.0.0.1:9", &[])).into_owned();
        assert!(u.starts_with("GET /ws HTTP/1.1\r\nHost: 127.0.0.1:9\r\nUpgrade: websocket\r\n") && u.ends_with("Sec-WebSocket-Version: 13\r\n\r\n"), "{u}");
    }

    /// Goal 10 (Ada's fixed-rate benchmark): frames written masked are read back (a server's unmasked ones too),
    /// a message in continuations is joined, pings and closes are said, whatever the split.
    #[test]
    fn rate_websocket_frames_round_trip() {
        let big = vec![b'x'; 70_000];
        let mut bytes = Vec::new();
        frame(0x1, b"{\"id\":1}", [1, 2, 3, 4], &mut bytes);
        frame(0x9, b"p", [9, 9, 9, 9], &mut bytes);
        frame(0x1, &big, [5, 6, 7, 8], &mut bytes);
        // A server's: unmasked, in two frames.
        bytes.extend_from_slice(&[0x01, 3]);
        bytes.extend_from_slice(b"abc");
        bytes.extend_from_slice(&[0x80, 2]);
        bytes.extend_from_slice(b"de");
        bytes.extend_from_slice(&[0x82, 1, 0]);
        bytes.extend_from_slice(&[0x88, 0]);
        for step in [1, 5, 1000, bytes.len()] {
            let (mut ws, mut out) = (Ws::default(), Vec::new());
            for chunk in bytes.chunks(step) {
                assert!(ws.feed(chunk, &mut out).is_ok());
            }
            assert_eq!(out, vec![WsIn::Text(b"{\"id\":1}".to_vec()), WsIn::Ping(b"p".to_vec()), WsIn::Text(big.clone()), WsIn::Text(b"abcde".to_vec()), WsIn::Binary(1), WsIn::Close], "{step} at a time");
        }
    }

    /// Goal 10 (Ada's fixed-rate benchmark): an RPC answer's id and tags are picked out of its JSON wherever they
    /// are (strings unescaped, numbers as written, containers said), past whatever else it holds; a batch is its
    /// messages.
    #[test]
    fn rate_an_answers_id_and_tags_are_picked_out_of_its_json() {
        let p = |s: &str| pointer(s).unwrap_or_default();
        let (id, tag, exit, error, deep, text) = (p("/requestId"), p("/_tag"), p("/exit/_tag"), p("/error"), p("/a/1/b~1c"), p("/exit/value/0/t"));
        let wants: [&[String]; 6] = [&id, &tag, &exit, &error, &deep, &text];
        let msg = br#"{"_tag":"Exit","big":{"x":[1,{"y":"}\"]"}],"z":"q"},"requestId":"42","exit":{"_tag":"Success","value":[{"t":"\u00e9\ud83d\ude00"}]},"a":[0,{"b/c":7}]}"#;
        let got = pick(msg, &wants);
        let s = |v: &str| Some(Found::Scalar(v.to_owned()));
        assert_eq!(got, vec![s("42"), s("Exit"), s("Success"), None, s("7"), s("\u{e9}\u{1f600}")]);
        let (rid, rerror, t) = (p("/id"), p("/error"), p("/t"));
        assert_eq!(pick(br#"{"jsonrpc":"2.0","id":7,"error":{"code":-1}}"#, &[rid.as_slice(), rerror.as_slice()]), vec![s("7"), Some(Found::Container)]);
        assert_eq!(pick(br#"{"t":"a\"b\\c\n"}"#, &[t.as_slice()]), vec![s("a\"b\\c\n")]);
        assert_eq!(pick(b"not json", &[t.as_slice()]), vec![None]);
        assert_eq!(pick(br#"{"id":"#, &[rid.as_slice()]), vec![None]);
        // Once everything wanted is found, the rest isn't read: a broken tail after it doesn't matter.
        assert_eq!(pick(br#"{"id":7,"error":{"code":-1},"snapshot":[1,2,"#, &[rid.as_slice(), rerror.as_slice()]), vec![s("7"), Some(Found::Container)]);
        let batch = br#" [ {"requestId":"1"} , {"requestId":"2","v":[1,[2]]} ] "#;
        let each: Vec<Vec<Option<Found>>> = messages(batch).into_iter().map(|m| pick(m, &[id.as_slice()])).collect();
        assert_eq!(each, vec![vec![s("1")], vec![s("2")]]);
        assert_eq!(messages(b"{\"a\":1}").len(), 1);
        assert_eq!(pointer("/a~0b/c~1d"), Ok(vec!["a~b".to_owned(), "c/d".to_owned()]));
        assert!(pointer("a").is_err());
    }
}
