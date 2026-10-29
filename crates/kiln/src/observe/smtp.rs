//! Mail through an SMTP relay (RFC 5321), as monitors' notifications are sent by email
//! (`docs/observe.md` §10): the relay a site already has (its own, or a provider's: SES, SendGrid,
//! Mailgun, Microsoft 365, Gmail), reached with STARTTLS (RFC 3207; 587 unless said), TLS from the
//! first byte (RFC 8314; 465) or neither (25: a relay on its own network), and authenticated with
//! AUTH PLAIN (RFC 4616) or LOGIN when it asks.
//!
//! - A reply is read whole (its continued lines, `250-…`); a 4xx is transient (the notification is
//!   tried again), a 5xx permanent (it is not), each said in the relay's words.
//! - STARTTLS: what the relay sends after its 220 and before the handshake is refused, not read as
//!   if it came over TLS (the plaintext injection of CVE-2011-0411); its certificate is checked
//!   against the public roots, or a CA given.
//! - The message (RFC 5322, MIME): `multipart/alternative`, its text and HTML each base64 (no line
//!   past 76, none a lone `.`), its subject an encoded word (RFC 2047) when not ASCII; `Date`,
//!   `Message-ID`, and `Auto-Submitted: auto-generated` (RFC 3834: not to be answered by a robot).
//!   Every header's value is refused if it holds a line break: a monitor's name cannot add one.

use std::time::Duration;

use base64::Engine as _;
use futures::io::{AsyncReadExt as _, AsyncWriteExt as _};
use serde::{Deserialize, Serialize};

use crate::http::client::Roots;
use crate::http::io::RtFuturesIo;
use crate::http::tls::{ClientTls, TlsStream};

/// How a relay is reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Security {
    /// Plain, then TLS once the relay says it can (STARTTLS): the submission port's (587).
    #[default]
    Starttls,
    /// TLS from the first byte: submissions' (465).
    Tls,
    /// Neither: a relay on its own network (25).
    None,
}

impl Security {
    pub fn port(self) -> u16 {
        match self {
            Self::Starttls => 587,
            Self::Tls => 465,
            Self::None => 25,
        }
    }
}

/// A relay and how to reach it.
#[derive(Debug, Clone)]
pub struct Relay {
    pub host: String,
    pub port: u16,
    pub security: Security,
    pub username: Option<String>,
    pub password: Option<String>,
    /// A CA's certificates (PEM) the relay's is signed by, when not a public one.
    pub ca: Option<String>,
}

/// A message to send.
#[derive(Debug, Clone, Default)]
pub struct Mail {
    pub from: String,
    pub to: Vec<String>,
    pub subject: String,
    pub text: String,
    pub html: String,
    /// More headers (`References`, `X-Kiln-Monitor`), each checked as the others are.
    pub headers: Vec<(String, String)>,
}

/// Why a message was not sent: tried again (a 4xx, the network) or not (a 5xx, a refusal).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    Transient(String),
    Permanent(String),
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transient(m) | Self::Permanent(m) => f.write_str(m),
        }
    }
}

/// What a command waits for an answer (RFC 5321 §4.5.3.2 gives minutes; a notification waits
/// less, and is tried again), and the end of a message's data.
const REPLY_WAIT: Duration = Duration::from_secs(60);
const DATA_WAIT: Duration = Duration::from_secs(120);
/// A reply's bytes at the most.
const REPLY_MAX: usize = 64 << 10;
/// Recipients a message has at the most.
pub const RECIPIENTS: usize = 50;

/// Whether `a` is an address a message may name: `local@domain`, no space, control character or
/// angle bracket, 254 bytes at the most (RFC 5321 §4.5.3.1).
pub fn valid_address(a: &str) -> bool {
    let Some((local, domain)) = a.rsplit_once('@') else { return false };
    !local.is_empty() && !domain.is_empty() && a.len() <= 254 && !a.chars().any(|c| c.is_control() || c.is_whitespace() || "<>,;\"\\()[]".contains(c))
}

// ---------------------------------------------------------------------------------------- message

fn b64() -> base64::engine::GeneralPurpose {
    base64::engine::general_purpose::STANDARD
}

/// `bytes` in base64, 76 characters a line (RFC 2045 §6.8).
fn base64_lines(bytes: &[u8]) -> String {
    let all = b64().encode(bytes);
    let mut out = String::with_capacity(all.len() + all.len() / 76 * 2 + 2);
    for chunk in all.as_bytes().chunks(76) {
        out.push_str(std::str::from_utf8(chunk).unwrap_or_default());
        out.push_str("\r\n");
    }
    out
}

/// A subject as a header holds it: itself when printable ASCII, else encoded words (RFC 2047's
/// `B` encoding), each 75 characters at the most, folded onto lines of their own.
fn encode_subject(s: &str) -> String {
    let s: String = s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    if s.is_ascii() {
        return s;
    }
    // `=?UTF-8?B?` and `?=` leave 63 characters: 45 bytes, whole characters.
    let mut words = Vec::new();
    let mut chunk = String::new();
    for c in s.chars() {
        if chunk.len() + c.len_utf8() > 45 {
            words.push(format!("=?UTF-8?B?{}?=", b64().encode(chunk.as_bytes())));
            chunk.clear();
        }
        chunk.push(c);
    }
    if !chunk.is_empty() {
        words.push(format!("=?UTF-8?B?{}?=", b64().encode(chunk.as_bytes())));
    }
    words.join("\r\n ")
}

/// A date as RFC 5322 writes one (`Mon, 19 Oct 2026 06:00:00 +0000`).
fn rfc5322_date(ms: i64) -> String {
    let t = time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000).unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
    let format = time::macros::format_description!("[weekday repr:short], [day padding:none] [month repr:short] [year] [hour]:[minute]:[second] +0000");
    t.format(&format).unwrap_or_default()
}

/// The message's bytes (RFC 5322, MIME), its lines ending CRLF: `id` its own (a `Message-ID`'s
/// left part), `boundary` its parts'. A header with a line break is refused.
pub fn message(mail: &Mail, date_ms: i64, id: &str, boundary: &str) -> Result<Vec<u8>, Failure> {
    let domain = mail.from.rsplit_once('@').map_or("kiln", |(_, d)| d);
    let mut headers: Vec<(String, String)> = vec![
        ("Date".into(), rfc5322_date(date_ms)),
        ("From".into(), mail.from.clone()),
        ("To".into(), mail.to.join(", ")),
        ("Subject".into(), encode_subject(&mail.subject)),
        ("Message-ID".into(), format!("<{id}@{domain}>")),
        ("MIME-Version".into(), "1.0".into()),
        ("Auto-Submitted".into(), "auto-generated".into()),
    ];
    headers.extend(mail.headers.iter().cloned());
    headers.push(("Content-Type".into(), format!("multipart/alternative; boundary=\"{boundary}\"")));
    let mut out = String::new();
    for (k, v) in &headers {
        // Folded lines (an encoded subject's) are this module's own; a value given holds none.
        let own_fold = k == "Subject";
        if k.is_empty() || k.contains([':', '\r', '\n']) || (!own_fold && v.contains(['\r', '\n'])) {
            return Err(Failure::Permanent(format!("a header that would break the message: {k}")));
        }
        out.push_str(k);
        out.push_str(": ");
        out.push_str(v);
        out.push_str("\r\n");
    }
    out.push_str("\r\n");
    for (kind, body) in [("text/plain", &mail.text), ("text/html", &mail.html)] {
        out.push_str(&format!("--{boundary}\r\nContent-Type: {kind}; charset=utf-8\r\nContent-Transfer-Encoding: base64\r\n\r\n"));
        out.push_str(&base64_lines(body.as_bytes()));
    }
    out.push_str(&format!("--{boundary}--\r\n"));
    Ok(out.into_bytes())
}

/// `data` as `DATA` sends it: each line beginning with `.` given another (RFC 5321 §4.5.2), lines
/// ending CRLF, then the end, `.` alone.
pub fn dot_stuffed(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 16);
    let text = String::from_utf8_lossy(data);
    for line in text.split("\r\n") {
        let line = line.trim_end_matches('\n');
        if line.starts_with('.') {
            out.push(b'.');
        }
        out.extend_from_slice(line.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    // The split leaves an empty last line after the final CRLF: one CRLF too many.
    if data.ends_with(b"\r\n") {
        out.truncate(out.len() - 2);
    }
    if !out.ends_with(b"\r\n") {
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b".\r\n");
    out
}

// ------------------------------------------------------------------------------------- the relay

/// A reply: its code and its lines' text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub code: u16,
    pub lines: Vec<String>,
}

impl Reply {
    fn text(&self) -> String {
        format!("{} {}", self.code, self.lines.join(" "))
    }
}

/// Reads one reply from `buf` if it holds a whole one, taking its bytes: `None` while more is to
/// come. A line that is no reply's is an error.
pub fn take_reply(buf: &mut Vec<u8>) -> Result<Option<Reply>, Failure> {
    let mut lines = Vec::new();
    let mut at = 0;
    loop {
        let Some(end) = buf.get(at..).and_then(|b| b.iter().position(|&c| c == b'\n')) else { return Ok(None) };
        let line = String::from_utf8_lossy(buf.get(at..at + end).unwrap_or_default()).trim_end_matches('\r').to_owned();
        at += end + 1;
        let code = line.get(..3).and_then(|c| c.parse::<u16>().ok()).filter(|c| (200..600).contains(c)).ok_or_else(|| Failure::Permanent(format!("not an SMTP reply: {line:?}")))?;
        let more = line.as_bytes().get(3) == Some(&b'-');
        lines.push(line.get(4..).unwrap_or("").to_owned());
        if !more {
            buf.drain(..at);
            return Ok(Some(Reply { code, lines }));
        }
    }
}

enum Stream {
    Plain(RtFuturesIo),
    Tls(Box<TlsStream<RtFuturesIo>>),
    Gone,
}

struct Conn {
    stream: Stream,
    buf: Vec<u8>,
    relay: String,
}

fn io_failure(relay: &str, e: impl std::fmt::Display) -> Failure {
    Failure::Transient(format!("{relay}: {e}"))
}

/// A handshake that failed: its certificate not trusted (or another TLS refusal) is permanent;
/// the network's failing is not.
fn tls_failure(relay: &str, e: &std::io::Error) -> Failure {
    let said = format!("{relay}: TLS: {e}");
    if e.kind() == std::io::ErrorKind::InvalidData { Failure::Permanent(said) } else { Failure::Transient(said) }
}

impl Conn {
    async fn write(&mut self, bytes: &[u8]) -> Result<(), Failure> {
        let r = match &mut self.stream {
            Stream::Plain(s) => s.write_all(bytes).await.and(s.flush().await),
            Stream::Tls(s) => s.write_all(bytes).await.and(s.flush().await),
            Stream::Gone => return Err(Failure::Transient(format!("{}: the connection is gone", self.relay))),
        };
        r.map_err(|e| io_failure(&self.relay, e))
    }

    async fn reply(&mut self, within: Duration) -> Result<Reply, Failure> {
        let relay = self.relay.clone();
        let read = async {
            let mut chunk = [0u8; 4096];
            loop {
                if let Some(r) = take_reply(&mut self.buf)? {
                    return Ok(r);
                }
                if self.buf.len() > REPLY_MAX {
                    return Err(Failure::Permanent(format!("{relay}: a reply past {REPLY_MAX} bytes")));
                }
                let n = match &mut self.stream {
                    Stream::Plain(s) => s.read(&mut chunk).await,
                    Stream::Tls(s) => s.read(&mut chunk).await,
                    Stream::Gone => Ok(0),
                }
                .map_err(|e| io_failure(&relay, e))?;
                if n == 0 {
                    return Err(Failure::Transient(format!("{relay} closed the connection")));
                }
                self.buf.extend_from_slice(chunk.get(..n).unwrap_or_default());
            }
        };
        crate::rt::timeout(within, read).await.map_err(|_| Failure::Transient(format!("{} did not answer within {} s", self.relay, within.as_secs())))?
    }

    /// Sends `line` and reads its reply, which must be one of `ok`.
    async fn command(&mut self, line: &str, ok: &[u16], what: &str) -> Result<Reply, Failure> {
        self.write(format!("{line}\r\n").as_bytes()).await?;
        let r = self.reply(REPLY_WAIT).await?;
        expect(r, ok, what)
    }
}

/// `r` if its code is one of `ok`; else a failure in the relay's words, transient for a 4xx.
fn expect(r: Reply, ok: &[u16], what: &str) -> Result<Reply, Failure> {
    if ok.contains(&r.code) {
        return Ok(r);
    }
    let said = format!("{what}: {}", r.text());
    Err(if (400..500).contains(&r.code) { Failure::Transient(said) } else { Failure::Permanent(said) })
}

/// The name this machine says in `EHLO`: its host name, else an address literal.
fn ehlo_name() -> String {
    let h = crate::os::hostname();
    if !h.is_empty() && h.bytes().all(|b| b.is_ascii_alphanumeric() || b"-.".contains(&b)) { h } else { "[127.0.0.1]".into() }
}

/// What `EHLO`'s reply offers: its extensions' keywords and parameters, upper case.
fn extensions(r: &Reply) -> Vec<String> {
    r.lines.iter().skip(1).map(|l| l.to_ascii_uppercase()).collect()
}

fn tls_of(relay: &Relay) -> Result<ClientTls, Failure> {
    let roots = match &relay.ca {
        Some(pem) => Roots::Pem(pem.as_bytes().to_vec()),
        None => Roots::WebPki,
    };
    ClientTls::new(roots, None).map_err(|e| Failure::Permanent(format!("the relay's CA: {e:#}")))
}

/// Sends `mail` through `relay`: what the relay said of it (its `250`), or why not.
pub async fn send(relay: &Relay, mail: &Mail, date_ms: i64) -> Result<String, Failure> {
    let name = format!("{}:{}", relay.host, relay.port);
    if mail.to.is_empty() || mail.to.len() > RECIPIENTS || !mail.to.iter().chain([&mail.from]).all(|a| valid_address(a)) {
        return Err(Failure::Permanent("a message's addresses: 1 to 50 recipients and a sender, each local@domain".into()));
    }
    let id = crate::util::new_id();
    let data = message(mail, date_ms, &id, &format!("kiln-{id}"))?;
    let tcp = crate::http::io::connect(&relay.host, relay.port).await.map_err(|e| io_failure(&name, format!("{e:#}")))?;
    let mut c = Conn { stream: Stream::Plain(RtFuturesIo::new(tcp)), buf: Vec::new(), relay: name.clone() };
    let server_name = crate::http::tls::server_name(&relay.host).map_err(|e| Failure::Permanent(format!("{name}: {e:#}")))?;
    if relay.security == Security::Tls {
        let Stream::Plain(plain) = std::mem::replace(&mut c.stream, Stream::Gone) else { return Err(io_failure(&name, "no connection")) };
        let tls = TlsStream::connect(plain, tls_of(relay)?.config().map_err(|e| Failure::Permanent(format!("{e:#}")))?, server_name.clone()).await.map_err(|e| tls_failure(&name, &e))?;
        c.stream = Stream::Tls(Box::new(tls));
    }
    let greeting = c.reply(REPLY_WAIT).await?;
    expect(greeting, &[220], "its greeting")?;
    let me = ehlo_name();
    let mut ehlo = c.command(&format!("EHLO {me}"), &[250], "EHLO").await?;
    if relay.security == Security::Starttls {
        if !extensions(&ehlo).iter().any(|e| e == "STARTTLS") {
            return Err(Failure::Permanent(format!("{name} does not offer STARTTLS (security: \"tls\" for port 465, \"none\" for a relay on its own network)")));
        }
        c.command("STARTTLS", &[220], "STARTTLS").await?;
        // Nothing may come between the relay's 220 and the handshake: what did would be read as
        // if it came over TLS.
        if !c.buf.is_empty() {
            return Err(Failure::Permanent(format!("{name} sent data after STARTTLS's 220: refused")));
        }
        let Stream::Plain(plain) = std::mem::replace(&mut c.stream, Stream::Gone) else { return Err(io_failure(&name, "no connection")) };
        let tls = TlsStream::connect(plain, tls_of(relay)?.config().map_err(|e| Failure::Permanent(format!("{e:#}")))?, server_name).await.map_err(|e| tls_failure(&name, &e))?;
        c.stream = Stream::Tls(Box::new(tls));
        ehlo = c.command(&format!("EHLO {me}"), &[250], "EHLO").await?;
    }
    let offered = extensions(&ehlo);
    if let Some(user) = relay.username.as_deref().filter(|u| !u.is_empty()) {
        let password = relay.password.as_deref().unwrap_or("");
        let mechanisms: Vec<&str> = offered.iter().filter_map(|e| e.strip_prefix("AUTH ").or_else(|| e.strip_prefix("AUTH="))).flat_map(|m| m.split_whitespace()).collect();
        if mechanisms.contains(&"PLAIN") {
            let token = b64().encode(format!("\0{user}\0{password}"));
            c.command(&format!("AUTH PLAIN {token}"), &[235], "AUTH PLAIN").await?;
        } else if mechanisms.contains(&"LOGIN") {
            c.command("AUTH LOGIN", &[334], "AUTH LOGIN").await?;
            c.command(&b64().encode(user), &[334], "AUTH LOGIN's user").await?;
            c.command(&b64().encode(password), &[235], "AUTH LOGIN").await?;
        } else {
            return Err(Failure::Permanent(format!("{name} offers no AUTH PLAIN or LOGIN{}", if relay.security == Security::None { " (relays offer it over TLS alone)" } else { "" })));
        }
    }
    let utf8 = mail.to.iter().chain([&mail.from]).any(|a| !a.is_ascii());
    if utf8 && !offered.iter().any(|e| e == "SMTPUTF8") {
        return Err(Failure::Permanent(format!("{name} does not take addresses that are not ASCII (SMTPUTF8)")));
    }
    c.command(&format!("MAIL FROM:<{}>{}", mail.from, if utf8 { " SMTPUTF8" } else { "" }), &[250], "MAIL FROM").await?;
    let mut refused = Vec::new();
    for to in &mail.to {
        if let Err(e) = c.command(&format!("RCPT TO:<{to}>"), &[250, 251], &format!("RCPT TO {to}")).await {
            refused.push(e);
        }
    }
    if refused.len() == mail.to.len() {
        // Every recipient refused: the message goes nowhere, transient if one was.
        let said = refused.iter().map(ToString::to_string).collect::<Vec<_>>().join("; ");
        return Err(if refused.iter().any(|e| matches!(e, Failure::Transient(_))) { Failure::Transient(said) } else { Failure::Permanent(said) });
    }
    c.command("DATA", &[354], "DATA").await?;
    c.write(&dot_stuffed(&data)).await?;
    let done = expect(c.reply(DATA_WAIT).await?, &[250], "the message")?;
    let _ = c.command("QUIT", &[221], "QUIT").await;
    let partly = if refused.is_empty() { String::new() } else { format!(" ({} refused: {})", refused.len(), refused.iter().map(ToString::to_string).collect::<Vec<_>>().join("; ")) };
    Ok(format!("{}{partly}", done.text()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replies_are_read_whole_and_their_codes_judged() {
        let mut buf = b"250-mail.example Hello\r\n250-STARTTLS\r\n250-AUTH PLAIN LOGIN\r\n250 SMTPUTF8\r\n220 next".to_vec();
        let r = take_reply(&mut buf).ok().flatten();
        assert_eq!(r, Some(Reply { code: 250, lines: vec!["mail.example Hello".into(), "STARTTLS".into(), "AUTH PLAIN LOGIN".into(), "SMTPUTF8".into()] }));
        assert_eq!(buf, b"220 next", "the next reply's bytes are left");
        assert_eq!(take_reply(&mut buf), Ok(None), "not whole yet");
        assert!(take_reply(&mut b"HTTP/1.1 400\r\n".to_vec()).is_err(), "not SMTP");
        let r = Reply { code: 451, lines: vec!["4.3.0 try later".into()] };
        assert!(matches!(expect(r, &[250], "MAIL FROM"), Err(Failure::Transient(m)) if m == "MAIL FROM: 451 4.3.0 try later"));
        let r = Reply { code: 535, lines: vec!["5.7.8 bad credentials".into()] };
        assert!(matches!(expect(r, &[235], "AUTH PLAIN"), Err(Failure::Permanent(_))));
    }

    #[test]
    fn a_message_is_mime_its_subject_encoded_its_lines_stuffed() -> Result<(), Failure> {
        let mail = Mail {
            from: "kiln@example.com".into(),
            to: vec!["a@example.com".into(), "b@example.com".into()],
            subject: "[Triggered] Température élevée on host:web-1".into(),
            text: "line one\n.starts with a dot".into(),
            html: "<p>hi</p>".into(),
            headers: vec![("References".into(), "<kiln-m1@example.com>".into())],
        };
        let m = String::from_utf8(message(&mail, 1_792_389_600_000, "abc", "b0").map_err(|e| Failure::Permanent(e.to_string()))?).unwrap_or_default();
        assert!(m.starts_with("Date: Mon, 19 Oct 2026 06:00:00 +0000\r\nFrom: kiln@example.com\r\nTo: a@example.com, b@example.com\r\nSubject: =?UTF-8?B?"), "{m}");
        assert!(m.contains("Message-ID: <abc@example.com>\r\n") && m.contains("Auto-Submitted: auto-generated\r\n") && m.contains("References: <kiln-m1@example.com>\r\n"), "{m}");
        assert!(m.contains("Content-Type: multipart/alternative; boundary=\"b0\"\r\n\r\n--b0\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: base64\r\n\r\n"), "{m}");
        assert!(m.ends_with("--b0--\r\n") && m.lines().all(|l| l.len() <= 998), "{m}");
        // The subject's encoded words decode to it.
        let subject: String = m.lines().skip_while(|l| !l.starts_with("Subject:")).take_while(|l| l.starts_with("Subject:") || l.starts_with(' ')).map(|l| l.trim_start_matches("Subject: ").trim()).collect::<Vec<_>>().join("");
        let decoded: String = subject
            .split("?==?")
            .map(|w| w.trim_start_matches("=?UTF-8?B?").trim_end_matches("?="))
            .filter_map(|w| b64().decode(w.replace("UTF-8?B?", "")).ok())
            .map(|b| String::from_utf8(b).unwrap_or_default())
            .collect();
        assert_eq!(decoded, mail.subject);
        // A header value with a line break would add headers of its own: refused.
        let evil = Mail { headers: vec![("X-Kiln-Monitor".into(), "m1\r\nBcc: all@example.com".into())], ..mail.clone() };
        assert!(message(&evil, 0, "x", "b").is_err());
        let evil_to = Mail { to: vec!["a@example.com\r\nBcc: x@y".into()], ..mail };
        assert!(evil_to.to.first().is_some_and(|a| !valid_address(a)));
        // Dot-stuffing: a line of a lone dot can't end the data early.
        assert_eq!(dot_stuffed(b"a\r\n.\r\n..b\r\n"), b"a\r\n..\r\n...b\r\n.\r\n");
        assert_eq!(dot_stuffed(b"no newline"), b"no newline\r\n.\r\n");
        Ok(())
    }

    #[test]
    fn addresses_are_checked() {
        for ok in ["ops@example.com", "a.b+tag@sub.example.co.uk", "δοκιμή@παράδειγμα.δοκιμή"] {
            assert!(valid_address(ok), "{ok}");
        }
        for bad in ["", "no-at", "@example.com", "a@", "a b@example.com", "<a@example.com>", "a@example.com,b@example.com", "a@example.com\n"] {
            assert!(!valid_address(bad), "{bad:?}");
        }
    }
}
