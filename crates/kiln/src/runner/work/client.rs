//! A small blocking HTTP/1.1 client of the cache service (docs/ci.md W24-R08), for `kiln
//! gocacheprog` (which answers the go command on stdin and stdout, one request at a time) and for
//! tests: one kept-alive connection to `http://host:port` or to the service's unix socket
//! (`unix:/path`), opened again once when the service closed it while idle.

use std::io::{Read as _, Write as _};
use std::time::Duration;

/// How long one request may take.
const TIMEOUT: Duration = Duration::from_secs(300);
/// The largest response head taken.
const MAX_HEAD: usize = 64 << 10;

enum Stream {
    Tcp(std::net::TcpStream),
    #[cfg(unix)]
    Unix(std::os::unix::net::UnixStream),
}

impl Stream {
    fn write_all(&mut self, b: &[u8]) -> std::io::Result<()> {
        match self {
            Stream::Tcp(s) => s.write_all(b),
            #[cfg(unix)]
            Stream::Unix(s) => s.write_all(b),
        }
    }

    fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Stream::Tcp(s) => s.read(b),
            #[cfg(unix)]
            Stream::Unix(s) => s.read(b),
        }
    }
}

/// Where the service is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    /// `host:port`, and a path every request's goes under.
    Tcp { addr: String, prefix: String },
    /// A unix socket.
    Unix(std::path::PathBuf),
}

impl Endpoint {
    /// `http://127.0.0.1:41234`, `http://127.0.0.1:41234/prefix` or `unix:/run/kiln/cas.sock`.
    pub fn parse(s: &str) -> anyhow::Result<Endpoint> {
        if let Some(path) = s.strip_prefix("unix:") {
            anyhow::ensure!(!path.is_empty(), "a unix endpoint names its socket: unix:/path");
            return Ok(Endpoint::Unix(path.into()));
        }
        let rest = s.strip_prefix("http://").ok_or_else(|| anyhow::anyhow!("the cache service is reached at http://host:port or unix:/path, not {s}"))?;
        let (addr, prefix) = match rest.split_once('/') {
            Some((a, p)) => (a, format!("/{}", p.trim_end_matches('/'))),
            None => (rest, String::new()),
        };
        anyhow::ensure!(!addr.is_empty(), "no host in {s}");
        Ok(Endpoint::Tcp { addr: addr.to_owned(), prefix: prefix.trim_end_matches('/').to_owned() })
    }
}

/// A response.
#[derive(Debug, Clone, Default)]
pub struct Reply {
    pub status: u16,
    /// Names lowercase.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Reply {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

/// One connection to the service.
pub struct Conn {
    endpoint: Endpoint,
    stream: Option<Stream>,
    /// Bytes read past the last response.
    buf: Vec<u8>,
}

impl Conn {
    pub fn new(endpoint: Endpoint) -> Conn {
        Conn { endpoint, stream: None, buf: Vec::new() }
    }

    fn open(&self) -> anyhow::Result<Stream> {
        match &self.endpoint {
            Endpoint::Tcp { addr, .. } => {
                let s = std::net::TcpStream::connect(addr.as_str()).map_err(|e| anyhow::anyhow!("the cache service at {addr}: {e}"))?;
                s.set_nodelay(true)?;
                s.set_read_timeout(Some(TIMEOUT))?;
                s.set_write_timeout(Some(TIMEOUT))?;
                Ok(Stream::Tcp(s))
            }
            #[cfg(unix)]
            Endpoint::Unix(path) => {
                let s = std::os::unix::net::UnixStream::connect(path).map_err(|e| anyhow::anyhow!("the cache service at {}: {e}", path.display()))?;
                s.set_read_timeout(Some(TIMEOUT))?;
                s.set_write_timeout(Some(TIMEOUT))?;
                Ok(Stream::Unix(s))
            }
            #[cfg(not(unix))]
            Endpoint::Unix(path) => anyhow::bail!("{}: unix sockets need a Unix machine", path.display()),
        }
    }

    /// `method path` with `headers` and `body`; the reply, whatever its status.
    pub fn request(&mut self, method: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> anyhow::Result<Reply> {
        // A kept connection the service closed while idle fails before any answer: once more on a
        // new one.
        let fresh = self.stream.is_none();
        match self.attempt(method, path, headers, body) {
            Ok(r) => Ok(r),
            Err(_) if !fresh => {
                self.stream = None;
                self.buf.clear();
                self.attempt(method, path, headers, body)
            }
            Err(e) => {
                self.stream = None;
                Err(e)
            }
        }
    }

    fn attempt(&mut self, method: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> anyhow::Result<Reply> {
        if self.stream.is_none() {
            self.stream = Some(self.open()?);
            self.buf.clear();
        }
        let (host, prefix) = match &self.endpoint {
            Endpoint::Tcp { addr, prefix } => (addr.clone(), prefix.clone()),
            Endpoint::Unix(_) => ("localhost".to_owned(), String::new()),
        };
        let mut head = format!("{method} {prefix}{path} HTTP/1.1\r\nhost: {host}\r\ncontent-length: {}\r\n", body.len());
        for (k, v) in headers {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        head.push_str("\r\n");
        let Some(stream) = self.stream.as_mut() else { anyhow::bail!("not connected") };
        stream.write_all(head.as_bytes())?;
        if !body.is_empty() {
            stream.write_all(body)?;
        }
        let reply = read_reply(stream, &mut self.buf, method == "HEAD")?;
        if reply.header("connection").is_some_and(|c| c.eq_ignore_ascii_case("close")) {
            self.stream = None;
        }
        Ok(reply)
    }
}

/// More bytes into `buf`; an error at the end of the stream.
fn fill(stream: &mut Stream, buf: &mut Vec<u8>) -> anyhow::Result<()> {
    let mut chunk = [0u8; 64 << 10];
    let n = stream.read(&mut chunk)?;
    anyhow::ensure!(n > 0, "the cache service closed the connection");
    buf.extend_from_slice(chunk.get(..n).unwrap_or_default());
    Ok(())
}

fn read_reply(stream: &mut Stream, buf: &mut Vec<u8>, head_only: bool) -> anyhow::Result<Reply> {
    let (status, headers, used) = loop {
        let mut slots = [httparse::EMPTY_HEADER; 64];
        let mut r = httparse::Response::new(&mut slots);
        match r.parse(buf)? {
            httparse::Status::Complete(used) => {
                let headers: Vec<(String, String)> = r.headers.iter().map(|h| (h.name.to_ascii_lowercase(), String::from_utf8_lossy(h.value).into_owned())).collect();
                break (r.code.unwrap_or(0), headers, used);
            }
            httparse::Status::Partial => {
                anyhow::ensure!(buf.len() < MAX_HEAD, "the cache service's answer has an oversized head");
                fill(stream, buf)?;
            }
        }
    };
    buf.drain(..used);
    let mut reply = Reply { status, headers, body: Vec::new() };
    if head_only || status == 204 || status == 304 || (100..200).contains(&status) {
        return Ok(reply);
    }
    if reply.header("transfer-encoding").is_some_and(|t| t.to_ascii_lowercase().contains("chunked")) {
        loop {
            let line_end = loop {
                if let Some(i) = buf.windows(2).position(|w| w == b"\r\n") {
                    break i;
                }
                fill(stream, buf)?;
            };
            let line = String::from_utf8_lossy(buf.get(..line_end).unwrap_or_default()).into_owned();
            let size = usize::from_str_radix(line.split(';').next().unwrap_or("").trim(), 16).map_err(|_| anyhow::anyhow!("a chunk size that doesn't read: {line:?}"))?;
            buf.drain(..line_end + 2);
            if size == 0 {
                // Trailers, then the blank line.
                loop {
                    if let Some(i) = buf.windows(2).position(|w| w == b"\r\n") {
                        let blank = i == 0;
                        buf.drain(..i + 2);
                        if blank {
                            break;
                        }
                    } else {
                        fill(stream, buf)?;
                    }
                }
                break;
            }
            while buf.len() < size + 2 {
                fill(stream, buf)?;
            }
            reply.body.extend_from_slice(buf.get(..size).unwrap_or_default());
            buf.drain(..size + 2);
        }
        return Ok(reply);
    }
    let len: usize = reply.header("content-length").and_then(|v| v.trim().parse().ok()).unwrap_or(0);
    while buf.len() < len {
        fill(stream, buf)?;
    }
    // The body moved out whole, and only what came after it copied.
    let rest = buf.split_off(len);
    reply.body = std::mem::replace(buf, rest);
    Ok(reply)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w24_r08_endpoints_read() -> anyhow::Result<()> {
        assert_eq!(Endpoint::parse("http://127.0.0.1:4000")?, Endpoint::Tcp { addr: "127.0.0.1:4000".into(), prefix: String::new() });
        assert_eq!(Endpoint::parse("http://127.0.0.1:4000/x/")?, Endpoint::Tcp { addr: "127.0.0.1:4000".into(), prefix: "/x".into() });
        assert_eq!(Endpoint::parse("unix:/run/k.sock")?, Endpoint::Unix("/run/k.sock".into()));
        assert!(Endpoint::parse("https://x").is_err() && Endpoint::parse("unix:").is_err() && Endpoint::parse("http:///p").is_err());
        Ok(())
    }
}
