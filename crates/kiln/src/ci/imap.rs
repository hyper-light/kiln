//! Reading a mailbox (docs/ci.md W14-R05): a small IMAP4rev1 client on kiln's own stack, for the
//! replies people send to kiln's review emails. It connects with TLS from the start (993, the
//! default) or STARTTLS (143, required when asked for), signs in with LOGIN, and takes the
//! folder's unseen messages, each whole (`BODY[]`, which marks it seen). A password goes in the
//! clear only to this machine's own loopback (a local mail server's).

use std::time::Duration;

use anyhow::Context as _;
use futures::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};

use crate::http::client::Roots;
use crate::http::io::{Reach, RtFuturesIo};
use crate::http::tls::{ClientTls, TlsStream, server_name};

/// How long one answer may take.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(60);
/// The longest line kiln reads outside a literal.
const MAX_LINE: usize = 64 << 10;
/// The biggest message kiln takes (a reply is small; anything bigger isn't one).
const MAX_MESSAGE: usize = 4 << 20;

/// How the connection is protected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tls {
    /// TLS from the first byte (port 993).
    Implicit,
    /// STARTTLS, required.
    StartTls,
    /// In the clear: only to this machine's loopback.
    None,
}

impl Tls {
    pub fn parse(s: &str) -> Result<Tls, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "" | "tls" | "implicit" | "ssl" => Ok(Tls::Implicit),
            "starttls" => Ok(Tls::StartTls),
            "none" | "off" => Ok(Tls::None),
            other => Err(format!("`imap_tls: {other}`: tls, starttls or none")),
        }
    }

    pub fn default_port(self) -> u16 {
        match self {
            Tls::Implicit => 993,
            Tls::StartTls | Tls::None => 143,
        }
    }
}

/// A mailbox, and how to sign in to it.
#[derive(Debug, Clone)]
pub struct Mailbox {
    pub host: String,
    pub port: u16,
    pub tls: Tls,
    pub user: String,
    pub password: String,
    /// `INBOX` by default.
    pub folder: String,
    /// Where kiln may connect (W22-AC07); `None`: anywhere.
    pub reach: Option<Reach>,
}

/// Whether `host` is this machine's loopback.
fn loopback(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost") || host.trim_matches(['[', ']']).parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// `s` as an IMAP quoted string.
fn quoted(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// A connection: the stream, what was read past the last line, and the next tag's number.
struct Conn<S> {
    io: S,
    buf: Vec<u8>,
    tag: u32,
}

/// One piece of an answer: a line, and the literal (`{n}`) it ended with, if any.
struct Piece {
    line: String,
    literal: Option<Vec<u8>>,
}

impl<S: AsyncRead + AsyncWrite + Unpin> Conn<S> {
    async fn fill(&mut self) -> anyhow::Result<()> {
        let mut chunk = [0u8; 8192];
        let n = crate::rt::timeout(ANSWER_TIMEOUT, self.io.read(&mut chunk)).await.map_err(|_| anyhow::anyhow!("the IMAP server didn't answer in {} s", ANSWER_TIMEOUT.as_secs()))??;
        anyhow::ensure!(n > 0, "the IMAP server closed the connection");
        self.buf.extend_from_slice(chunk.get(..n).unwrap_or_default());
        Ok(())
    }

    async fn line(&mut self) -> anyhow::Result<String> {
        loop {
            if let Some(i) = self.buf.windows(2).position(|w| w == b"\r\n") {
                let line: Vec<u8> = self.buf.drain(..i + 2).collect();
                return Ok(String::from_utf8_lossy(line.get(..i).unwrap_or_default()).into_owned());
            }
            anyhow::ensure!(self.buf.len() < MAX_LINE, "the IMAP server sent a line longer than {MAX_LINE} bytes");
            self.fill().await?;
        }
    }

    /// A line, and the literal it announces (`… {123}`), read whole.
    async fn piece(&mut self) -> anyhow::Result<Piece> {
        let line = self.line().await?;
        let size = line.strip_suffix('}').and_then(|l| l.rsplit_once('{')).and_then(|(_, n)| n.parse::<usize>().ok());
        let Some(n) = size else { return Ok(Piece { line, literal: None }) };
        anyhow::ensure!(n <= MAX_MESSAGE, "the IMAP server sent {n} bytes at once, more than a reply ever is");
        while self.buf.len() < n {
            self.fill().await?;
        }
        let literal: Vec<u8> = self.buf.drain(..n).collect();
        Ok(Piece { line, literal: Some(literal) })
    }

    /// Sends a command with a tag of its own and reads its answer up to the tagged status, which
    /// is `OK` or an error naming `what` (never the command itself, which may hold a password).
    async fn run(&mut self, command: &str, what: &str) -> anyhow::Result<Vec<Piece>> {
        self.tag += 1;
        let tag = format!("k{}", self.tag);
        self.io.write_all(format!("{tag} {command}\r\n").as_bytes()).await?;
        self.io.flush().await?;
        let mut pieces = Vec::new();
        loop {
            let p = self.piece().await?;
            if let Some(rest) = p.line.strip_prefix(&format!("{tag} ")) {
                let (status, text) = rest.split_once(' ').unwrap_or((rest, ""));
                anyhow::ensure!(status.eq_ignore_ascii_case("OK"), "{what}: the IMAP server said {status} {text}");
                return Ok(pieces);
            }
            pieces.push(p);
        }
    }
}

/// The UIDs a `UID SEARCH` answered with.
fn searched(pieces: &[Piece]) -> Vec<u32> {
    pieces.iter().filter_map(|p| p.line.strip_prefix("* SEARCH")).flat_map(|rest| rest.split_whitespace().filter_map(|n| n.parse::<u32>().ok()).collect::<Vec<_>>()).collect()
}

/// A `UID FETCH (UID BODY[])`'s messages: each UID and its bytes. The UID is said before the
/// literal or after it, as the server likes.
fn fetched(pieces: &[Piece]) -> Vec<(u32, Vec<u8>)> {
    let uid_in = |s: &str| {
        let upper = s.to_ascii_uppercase();
        let at = upper.find("UID ")?;
        upper.get(at + 4..)?.split(|c: char| !c.is_ascii_digit()).next()?.parse::<u32>().ok()
    };
    let mut out = Vec::new();
    for (i, p) in pieces.iter().enumerate() {
        let Some(body) = &p.literal else { continue };
        if !p.line.starts_with("* ") || !p.line.to_ascii_uppercase().contains("FETCH") {
            continue;
        }
        let after = pieces.get(i + 1).filter(|n| n.literal.is_none()).map(|n| n.line.as_str()).unwrap_or_default();
        if let Some(uid) = uid_in(p.line.rsplit_once('{').map_or(p.line.as_str(), |(l, _)| l)).or_else(|| uid_in(after)) {
            out.push((uid, body.clone()));
        }
    }
    out
}

async fn take<S: AsyncRead + AsyncWrite + Unpin>(c: &mut Conn<S>, m: &Mailbox, limit: usize) -> anyhow::Result<Vec<(u32, Vec<u8>)>> {
    c.run(&format!("LOGIN {} {}", quoted(&m.user), quoted(&m.password)), "signing in").await?;
    let folder = if m.folder.trim().is_empty() { "INBOX" } else { m.folder.trim() };
    c.run(&format!("SELECT {}", quoted(folder)), &format!("the folder {folder}")).await?;
    let mut uids = searched(&c.run("UID SEARCH UNSEEN", "the unseen messages").await?);
    uids.truncate(limit);
    if uids.is_empty() {
        let _ = c.run("LOGOUT", "signing out").await;
        return Ok(Vec::new());
    }
    let set = uids.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
    let got = fetched(&c.run(&format!("UID FETCH {set} (UID BODY[])"), "the messages").await?);
    let _ = c.run("LOGOUT", "signing out").await;
    Ok(got)
}

/// The folder's unseen messages (at most `limit`), each by UID with its bytes, now seen.
pub async fn unseen(m: &Mailbox, limit: usize) -> anyhow::Result<Vec<(u32, Vec<u8>)>> {
    anyhow::ensure!(m.tls != Tls::None || loopback(&m.host), "a password goes in the clear only to this machine: use `imap_tls: tls` or `starttls` for {}", m.host);
    let tcp = match &m.reach {
        Some(r) => crate::http::io::connect_direct_reaching(&m.host, m.port, r).await?,
        None => crate::http::io::connect(&m.host, m.port).await?,
    };
    match m.tls {
        Tls::Implicit => {
            let tls = ClientTls::new(Roots::WebPki, None)?;
            let stream = TlsStream::connect(RtFuturesIo::new(tcp), tls.config_alpn(&[])?, server_name(&m.host)?).await.with_context(|| format!("TLS to {}", m.host))?;
            let mut c = Conn { io: stream, buf: Vec::new(), tag: 0 };
            greeting(&mut c).await?;
            take(&mut c, m, limit).await
        }
        Tls::StartTls => {
            let mut c = Conn { io: RtFuturesIo::new(tcp), buf: Vec::new(), tag: 0 };
            greeting(&mut c).await?;
            c.run("STARTTLS", "STARTTLS").await?;
            // Nothing may come between STARTTLS's answer and the handshake.
            anyhow::ensure!(c.buf.is_empty(), "the IMAP server sent more after STARTTLS's answer");
            let tls = ClientTls::new(Roots::WebPki, None)?;
            let stream = TlsStream::connect(c.io, tls.config_alpn(&[])?, server_name(&m.host)?).await.with_context(|| format!("TLS to {}", m.host))?;
            let mut c = Conn { io: stream, buf: Vec::new(), tag: c.tag };
            take(&mut c, m, limit).await
        }
        Tls::None => {
            let mut c = Conn { io: RtFuturesIo::new(tcp), buf: Vec::new(), tag: 0 };
            greeting(&mut c).await?;
            take(&mut c, m, limit).await
        }
    }
}

async fn greeting<S: AsyncRead + AsyncWrite + Unpin>(c: &mut Conn<S>) -> anyhow::Result<()> {
    let line = c.line().await?;
    anyhow::ensure!(line.starts_with("* OK") || line.starts_with("* PREAUTH"), "the IMAP server's greeting: {line}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w14_r05_what_an_imap_server_says() {
        let p = |line: &str, literal: Option<&[u8]>| Piece { line: line.to_owned(), literal: literal.map(<[u8]>::to_vec) };
        assert_eq!(searched(&[p("* SEARCH 4 9 12", None)]), [4, 9, 12]);
        assert!(searched(&[p("* SEARCH", None)]).is_empty());
        // The UID before the literal, and after it.
        let got = fetched(&[p("* 1 FETCH (UID 4 BODY[] {5}", Some(b"hello")), p(")", None), p("* 2 FETCH (BODY[] {3}", Some(b"abc")), p(" UID 9)", None)]);
        assert_eq!(got, [(4, b"hello".to_vec()), (9, b"abc".to_vec())]);
        assert_eq!(quoted("a\"b\\c"), "\"a\\\"b\\\\c\"");
        assert!(loopback("127.0.0.1") && loopback("localhost") && loopback("[::1]") && !loopback("imap.example.com"));
        assert_eq!((Tls::parse(""), Tls::parse("starttls"), Tls::parse("none")), (Ok(Tls::Implicit), Ok(Tls::StartTls), Ok(Tls::None)));
    }
}
