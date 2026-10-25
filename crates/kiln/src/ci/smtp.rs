//! Email for notifications (docs/ci.md W11-R06, W19-R06): a small SMTP client on kiln's own
//! stack. It talks to a submission server with STARTTLS (port 587, the default) or TLS from the
//! start (465), or in the clear to a relay that takes mail that way (`tls: none`, never with a
//! password); signs in with AUTH PLAIN, or LOGIN where that's all a server offers; and sends one
//! plain-text message.

use std::time::Duration;

use anyhow::Context as _;
use base64::Engine as _;
use futures::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};

use crate::http::client::Roots;
use crate::http::io::{Reach, RtFuturesIo};
use crate::http::tls::{ClientTls, TlsStream, server_name};

/// How long one reply may take.
const REPLY_TIMEOUT: Duration = Duration::from_secs(60);
/// The longest reply line kiln reads (RFC 5321 allows 512 bytes; servers say more).
const MAX_LINE: usize = 16 << 10;

/// How the connection is protected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tls {
    /// STARTTLS, required: a server that doesn't offer it isn't sent to.
    StartTls,
    /// TLS from the first byte (port 465).
    Implicit,
    /// In the clear, to a relay that takes mail that way.
    None,
}

impl Tls {
    pub fn parse(s: &str) -> Result<Tls, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "" | "starttls" => Ok(Tls::StartTls),
            "tls" | "implicit" | "ssl" => Ok(Tls::Implicit),
            "none" | "off" => Ok(Tls::None),
            other => Err(format!("`tls: {other}`: starttls, tls or none")),
        }
    }

    pub fn default_port(self) -> u16 {
        match self {
            Tls::StartTls => 587,
            Tls::Implicit => 465,
            Tls::None => 25,
        }
    }
}

/// A server and how to sign in to it.
#[derive(Debug, Clone)]
pub struct Server {
    pub host: String,
    pub port: u16,
    pub tls: Tls,
    pub user: Option<String>,
    pub password: Option<String>,
    /// Where kiln may connect (W22-AC07); `None`: anywhere.
    pub reach: Option<Reach>,
}

/// One message: plain text.
#[derive(Debug, Clone)]
pub struct Message {
    /// `kiln <ci@example.com>`, or just the address.
    pub from: String,
    pub to: Vec<String>,
    pub subject: String,
    pub body: String,
    /// Where replies go (a review thread's reply address, docs/ci.md W14-R05).
    pub reply_to: Option<String>,
}

/// The address in `kiln <ci@example.com>` (or the whole, when it's just an address).
pub fn address(s: &str) -> &str {
    let s = s.trim();
    match (s.rfind('<'), s.rfind('>')) {
        (Some(a), Some(b)) if a < b => s.get(a + 1..b).unwrap_or(s).trim(),
        _ => s,
    }
}

/// Whether `s` looks like an address mail can go to: something, `@`, a domain, no spaces or
/// angle brackets, nothing that would end a line.
pub fn valid_address(s: &str) -> bool {
    // Nothing that ends a line or a command, anywhere in it.
    if s.chars().any(char::is_control) {
        return false;
    }
    let a = address(s);
    let ok_chars = !a.chars().any(|c| c.is_whitespace() || c.is_control() || matches!(c, '<' | '>' | ',' | ';'));
    ok_chars && a.split_once('@').is_some_and(|(local, domain)| !local.is_empty() && domain.contains('.') && !domain.starts_with('.') && !domain.ends_with('.'))
}

/// A header's text on one line: no CR or LF, and RFC 2047's encoded word when it isn't ASCII.
fn header_text(s: &str) -> String {
    let one: String = s.chars().map(|c| if c == '\r' || c == '\n' { ' ' } else { c }).collect();
    if one.is_ascii() { one } else { format!("=?utf-8?B?{}?=", base64::engine::general_purpose::STANDARD.encode(one.as_bytes())) }
}

/// The message as it goes over the wire, after DATA: headers, then the body in base64 lines
/// (which no line of `.` can end early), then the end of data.
pub fn render(m: &Message, host: &str, now: time::OffsetDateTime) -> String {
    let date = now.format(&time::format_description::well_known::Rfc2822).unwrap_or_default();
    let id = format!("<{}@{}>", crate::util::new_id(), host);
    let body = base64::engine::general_purpose::STANDARD.encode(m.body.as_bytes());
    let mut out = String::new();
    for (k, v) in [
        ("From", header_text(&m.from)),
        ("To", m.to.iter().map(|t| header_text(t)).collect::<Vec<_>>().join(", ")),
        ("Subject", header_text(&m.subject)),
        ("Date", date),
        ("Message-ID", id),
        ("MIME-Version", "1.0".into()),
        ("Content-Type", "text/plain; charset=utf-8".into()),
        ("Content-Transfer-Encoding", "base64".into()),
        // RFC 3834: it's from a machine, so nothing answers it automatically.
        ("Auto-Submitted", "auto-generated".into()),
    ] {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    if let Some(r) = &m.reply_to {
        out.push_str(&format!("Reply-To: {}\r\n", header_text(r)));
    }
    out.push_str("\r\n");
    let bytes = body.as_bytes();
    for line in bytes.chunks(76) {
        out.push_str(&String::from_utf8_lossy(line));
        out.push_str("\r\n");
    }
    out.push_str(".\r\n");
    out
}

/// A connection: the stream, and what was read past the last reply.
struct Conn<S> {
    io: S,
    buf: Vec<u8>,
}

impl<S: AsyncRead + AsyncWrite + Unpin> Conn<S> {
    async fn line(&mut self) -> anyhow::Result<String> {
        loop {
            if let Some(i) = self.buf.windows(2).position(|w| w == b"\r\n") {
                let line: Vec<u8> = self.buf.drain(..i + 2).collect();
                return Ok(String::from_utf8_lossy(line.get(..i).unwrap_or_default()).into_owned());
            }
            anyhow::ensure!(self.buf.len() < MAX_LINE, "the SMTP server sent a line longer than {MAX_LINE} bytes");
            let mut chunk = [0u8; 2048];
            let n = crate::rt::timeout(REPLY_TIMEOUT, self.io.read(&mut chunk)).await.map_err(|_| anyhow::anyhow!("the SMTP server didn't answer in {} s", REPLY_TIMEOUT.as_secs()))??;
            anyhow::ensure!(n > 0, "the SMTP server closed the connection");
            self.buf.extend_from_slice(chunk.get(..n).unwrap_or_default());
        }
    }

    /// One reply: its code, and its lines' text.
    async fn reply(&mut self) -> anyhow::Result<(u16, Vec<String>)> {
        let mut lines = Vec::new();
        loop {
            let line = self.line().await?;
            let code: u16 = line.get(..3).and_then(|c| c.parse().ok()).with_context(|| format!("not an SMTP reply: {line}"))?;
            lines.push(line.get(4..).unwrap_or_default().to_owned());
            if line.as_bytes().get(3) != Some(&b'-') {
                return Ok((code, lines));
            }
        }
    }

    /// Sends `line` and wants a reply of class `want` (2: done, 3: go on); `what` names it in
    /// errors, so a password never shows in one.
    async fn say(&mut self, line: &str, what: &str, want: u16) -> anyhow::Result<Vec<String>> {
        self.io.write_all(format!("{line}\r\n").as_bytes()).await?;
        self.io.flush().await?;
        let (code, text) = self.reply().await?;
        anyhow::ensure!(code / 100 == want, "{what}: the SMTP server said {code} {}", text.join(" "));
        Ok(text)
    }
}

/// What the server can do, from its EHLO reply: `STARTTLS`, `AUTH PLAIN LOGIN`, ….
fn has(ehlo: &[String], word: &str) -> bool {
    ehlo.iter().any(|l| l.split_whitespace().next().is_some_and(|w| w.eq_ignore_ascii_case(word)))
}

fn auth_methods(ehlo: &[String]) -> Vec<String> {
    ehlo.iter()
        .filter_map(|l| {
            let mut w = l.split_whitespace();
            w.next().filter(|x| x.eq_ignore_ascii_case("AUTH")).map(|_| w.map(str::to_ascii_uppercase).collect::<Vec<_>>())
        })
        .flatten()
        .collect()
}

/// The rest of a session after the greeting and EHLO (on TLS, when there is TLS): sign in, then
/// send `m`.
async fn session<S: AsyncRead + AsyncWrite + Unpin>(c: &mut Conn<S>, ehlo: &[String], server: &Server, m: &Message, data: &str) -> anyhow::Result<()> {
    if let Some(user) = server.user.as_deref().filter(|u| !u.is_empty()) {
        let password = server.password.clone().unwrap_or_default();
        let methods = auth_methods(ehlo);
        let b64 = |s: &str| base64::engine::general_purpose::STANDARD.encode(s.as_bytes());
        if methods.iter().any(|m| m == "PLAIN") || methods.is_empty() {
            c.say(&format!("AUTH PLAIN {}", b64(&format!("\0{user}\0{password}"))), "signing in (AUTH PLAIN)", 2).await?;
        } else if methods.iter().any(|m| m == "LOGIN") {
            c.say("AUTH LOGIN", "signing in (AUTH LOGIN)", 3).await?;
            c.say(&b64(user), "signing in (the user)", 3).await?;
            c.say(&b64(&password), "signing in (the password)", 2).await?;
        } else {
            anyhow::bail!("the SMTP server signs in only with {}: kiln speaks PLAIN and LOGIN", methods.join(", "));
        }
    }
    c.say(&format!("MAIL FROM:<{}>", address(&m.from)), "the sender", 2).await?;
    for to in &m.to {
        c.say(&format!("RCPT TO:<{}>", address(to)), &format!("the recipient {}", address(to)), 2).await?;
    }
    c.say("DATA", "the message", 3).await?;
    c.io.write_all(data.as_bytes()).await?;
    c.io.flush().await?;
    let (code, text) = c.reply().await?;
    anyhow::ensure!(code / 100 == 2, "the message: the SMTP server said {code} {}", text.join(" "));
    // Done; QUIT is a courtesy.
    let _ = c.say("QUIT", "quitting", 2).await;
    Ok(())
}

/// Sends `m` through `server`.
pub async fn send(server: &Server, m: &Message) -> anyhow::Result<()> {
    anyhow::ensure!(valid_address(&m.from), "the sender `{}` isn't an address", m.from);
    anyhow::ensure!(!m.to.is_empty(), "the message has no recipient");
    for t in &m.to {
        anyhow::ensure!(valid_address(t), "`{t}` isn't an address");
    }
    // A password is only sent to sign in as `user`.
    let signs_in = server.user.as_deref().is_some_and(|u| !u.is_empty()) && server.password.as_deref().is_some_and(|p| !p.is_empty());
    anyhow::ensure!(server.tls != Tls::None || !signs_in, "a password never goes in the clear: use `tls: starttls` or `tls: tls`");
    let tcp = match &server.reach {
        Some(r) => crate::http::io::connect_direct_reaching(&server.host, server.port, r).await?,
        None => crate::http::io::connect(&server.host, server.port).await?,
    };
    let tls = ClientTls::new(Roots::WebPki, None)?;
    let me = hostname();
    let data = render(m, &me, time::OffsetDateTime::now_utc());
    let ehlo_line = format!("EHLO {me}");
    match server.tls {
        Tls::Implicit => {
            let stream = TlsStream::connect(RtFuturesIo::new(tcp), tls.config_alpn(&[])?, server_name(&server.host)?).await.with_context(|| format!("TLS to {}", server.host))?;
            let mut c = Conn { io: stream, buf: Vec::new() };
            greeting(&mut c).await?;
            let ehlo = c.say(&ehlo_line, "EHLO", 2).await?;
            session(&mut c, &ehlo, server, m, &data).await
        }
        Tls::StartTls => {
            let mut c = Conn { io: RtFuturesIo::new(tcp), buf: Vec::new() };
            greeting(&mut c).await?;
            let ehlo = c.say(&ehlo_line, "EHLO", 2).await?;
            anyhow::ensure!(has(&ehlo, "STARTTLS"), "{} doesn't offer STARTTLS (for TLS from the start, `tls: tls` and port 465)", server.host);
            c.say("STARTTLS", "STARTTLS", 2).await?;
            // Nothing may come between STARTTLS's reply and the handshake.
            anyhow::ensure!(c.buf.is_empty(), "the SMTP server sent more after STARTTLS's reply");
            let stream = TlsStream::connect(c.io, tls.config_alpn(&[])?, server_name(&server.host)?).await.with_context(|| format!("TLS to {}", server.host))?;
            let mut c = Conn { io: stream, buf: Vec::new() };
            let ehlo = c.say(&ehlo_line, "EHLO", 2).await?;
            session(&mut c, &ehlo, server, m, &data).await
        }
        Tls::None => {
            let mut c = Conn { io: RtFuturesIo::new(tcp), buf: Vec::new() };
            greeting(&mut c).await?;
            let ehlo = c.say(&ehlo_line, "EHLO", 2).await?;
            session(&mut c, &ehlo, server, m, &data).await
        }
    }
}

async fn greeting<S: AsyncRead + AsyncWrite + Unpin>(c: &mut Conn<S>) -> anyhow::Result<()> {
    let (code, text) = c.reply().await?;
    anyhow::ensure!(code == 220, "the SMTP server's greeting: {code} {}", text.join(" "));
    Ok(())
}

/// The name kiln gives itself in EHLO.
fn hostname() -> String {
    let h = sysinfo::System::host_name().unwrap_or_default();
    if h.is_empty() || !h.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.') { "kiln.localdomain".to_owned() } else { h }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w19_r06_messages() {
        assert_eq!(address("kiln <ci@example.com>"), "ci@example.com");
        assert_eq!(address(" ada@example.com "), "ada@example.com");
        assert!(valid_address("kiln <ci@example.com>") && valid_address("ada@example.co.uk"));
        for bad in ["ada", "ada@example", "a b@example.com", "ada@example.com\r\nRCPT TO:<x@y.z>", "<>", "@example.com"] {
            assert!(!valid_address(bad), "{bad:?}");
        }
        let m = Message { from: "kiln <ci@example.com>".into(), to: vec!["ada@example.com".into()], subject: "✗ CI #7 failed\r\nBcc: x@y.z".into(), body: ".\r\nline two".into(), reply_to: None };
        let at = time::OffsetDateTime::from_unix_timestamp(1_792_400_000).unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
        let data = render(&m, "kiln.test", at);
        assert!(data.contains("From: kiln <ci@example.com>\r\nTo: ada@example.com\r\n"), "{data}");
        // The subject stays one header (encoded: it isn't ASCII), and nothing in the body ends it.
        assert!(data.contains("Subject: =?utf-8?B?") && !data.contains("\r\nBcc:"), "{data}");
        assert!(data.ends_with("\r\n.\r\n") && data.matches("\r\n.\r\n").count() == 1, "{data}");
        assert!(data.contains("Date: ") && data.contains("Auto-Submitted: auto-generated"));
        assert!(!data.contains("Reply-To:"), "{data}");
        let replied = render(&Message { reply_to: Some("kiln <review+t0k@example.com>".into()), ..m }, "kiln.test", at);
        assert!(replied.contains("\r\nReply-To: kiln <review+t0k@example.com>\r\n"), "{replied}");
        assert_eq!(Tls::parse("").ok(), Some(Tls::StartTls));
        assert_eq!(Tls::parse("TLS").ok(), Some(Tls::Implicit));
        assert!(Tls::parse("maybe").is_err());
        assert_eq!(auth_methods(&["smtp.example.com".into(), "AUTH PLAIN LOGIN".into(), "STARTTLS".into()]), vec!["PLAIN", "LOGIN"]);
        assert!(has(&["STARTTLS".into()], "starttls"));
    }
}
