//! Syslog received (`docs/observe.md` §8.1), as Datadog's Agent and rsyslog receive it: over UDP
//! (a message a datagram) and TCP (RFC 6587: each message counted, `<length> <message>`, or ended by
//! a newline), each message RFC 5424's or the BSD format RFC 3164 describes, read leniently: what
//! does not parse is the message whole.
//!
//! Syslog has no acknowledgement: what a datagram carries is lost if there is no room for it (and
//! counted), while a TCP sender is held back (kiln stops reading its connection until there is
//! room), as TCP holds any sender back.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::FutureExt as _;
use kiln_obs::logs::record::Record;
use kiln_obs::query::calendar::Zone;

use super::{Batch, Compiled, InFlight, MAX_LINE, State, Stop};
use crate::observe::ingest::Ack;

/// The most TCP connections a source holds at once; and how long one may stay silent.
const CONNECTIONS: usize = 1_024;
const IDLE: Duration = Duration::from_secs(300);
/// Messages received and not yet offered, at the most: past it, a datagram is dropped (counted) and
/// a TCP connection waits.
const WAITING: usize = 16_384;
/// The largest datagram.
const DATAGRAM: usize = 64 << 10;

/// Where records go, from tasks of their own.
pub(crate) type Sink = Arc<dyn Fn(Vec<Record>, Ack) -> Result<(), Vec<Record>> + Send + Sync>;

const FACILITIES: [&str; 24] = [
    "kern", "user", "mail", "daemon", "auth", "syslog", "lpr", "news", "uucp", "cron", "authpriv", "ftp", "ntp", "security", "console", "solaris-cron", "local0", "local1", "local2", "local3", "local4", "local5", "local6", "local7",
];
const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// A message read: what its header says, its structured data, its text.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Message<'a> {
    pub pri: Option<u8>,
    /// Nanoseconds since the epoch.
    pub ts: Option<i64>,
    pub host: Option<&'a str>,
    pub app: Option<&'a str>,
    pub procid: Option<&'a str>,
    pub msgid: Option<&'a str>,
    pub sd: Vec<(String, String)>,
    pub msg: &'a str,
}

/// `<PRI>`: its value (0–191) and what follows.
fn pri(s: &str) -> Option<(u8, &str)> {
    let rest = s.strip_prefix('<')?;
    let end = rest.find('>')?;
    let digits = rest.get(..end)?;
    if digits.is_empty() || digits.len() > 3 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let p: u16 = digits.parse().ok()?;
    (p <= 191).then_some((u8::try_from(p).ok()?, rest.get(end + 1..)?))
}

/// The next field before a space (all of it when there is none) and what follows the space.
fn field(s: &str) -> (&str, &str) {
    s.split_once(' ').unwrap_or((s, ""))
}

fn nil(f: &str) -> Option<&str> {
    (f != "-" && !f.is_empty()).then_some(f)
}

/// RFC 5424's structured data: `[id name="value" ...][...]`, and what follows it.
fn structured(s: &str, out: &mut Vec<(String, String)>) -> Option<usize> {
    let b = s.as_bytes();
    let mut i = 0;
    while b.get(i) == Some(&b'[') {
        i += 1;
        let id_end = s.get(i..)?.find([' ', ']']).map(|k| i + k)?;
        let id = s.get(i..id_end)?;
        i = id_end;
        loop {
            match b.get(i) {
                Some(b']') => {
                    i += 1;
                    break;
                }
                Some(b' ') => {
                    i += 1;
                    let eq = s.get(i..)?.find('=').map(|k| i + k)?;
                    let name = s.get(i..eq)?;
                    if b.get(eq + 1) != Some(&b'"') {
                        return None;
                    }
                    let mut value = String::new();
                    let mut j = eq + 2;
                    loop {
                        match b.get(j)? {
                            b'\\' if matches!(b.get(j + 1), Some(b'"' | b'\\' | b']')) => {
                                value.push(char::from(*b.get(j + 1)?));
                                j += 2;
                            }
                            b'"' => break,
                            _ => {
                                let c = s.get(j..)?.chars().next()?;
                                value.push(c);
                                j += c.len_utf8();
                            }
                        }
                    }
                    if out.len() < 64 {
                        out.push((format!("sd.{id}.{name}"), value));
                    }
                    i = j + 1;
                }
                _ => return None,
            }
        }
    }
    Some(i)
}

/// RFC 3164's time, `Mmm dd hh:mm:ss` (a day of one digit padded with a space; seconds' fraction
/// and a year after the day allowed, as some senders give them), in `zone`, the year the one it is
/// unless that is more than a day ahead (a December message read in January); and what follows.
fn bsd_time(s: &str, zone: Zone, now_ns: i64) -> Option<(i64, &str)> {
    let month = MONTHS.iter().position(|m| s.starts_with(m))?;
    let rest = s.get(3..)?.strip_prefix(' ')?;
    let rest = rest.strip_prefix(' ').unwrap_or(rest);
    let (day, rest) = field(rest);
    let day: i8 = day.parse().ok().filter(|d| (1..=31).contains(d))?;
    let (mut clock, mut rest) = field(rest);
    let mut year = None;
    if clock.len() == 4 && clock.bytes().all(|b| b.is_ascii_digit()) {
        year = clock.parse::<i16>().ok();
        (clock, rest) = field(rest);
    }
    let (hms, frac) = clock.split_once('.').unwrap_or((clock, ""));
    let mut parts = hms.split(':');
    let (h, m, sec): (i8, i8, i8) = (parts.next()?.parse().ok()?, parts.next()?.parse().ok()?, parts.next()?.parse().ok()?);
    let nanos: i32 = if frac.is_empty() || frac.len() > 9 || !frac.bytes().all(|b| b.is_ascii_digit()) {
        0
    } else {
        frac.parse::<i32>().ok()? * 10i32.pow(9 - u32::try_from(frac.len()).ok()?)
    };
    let month = i8::try_from(month + 1).ok()?;
    let ns = match year {
        Some(y) => zone.civil_ns(y, month, day, h, m, sec, nanos)?,
        None => {
            let y = zone.year_at(now_ns)?;
            let t = zone.civil_ns(y, month, day, h, m, sec, nanos)?;
            if t > now_ns + 86_400_000_000_000 { zone.civil_ns(y - 1, month, day, h, m, sec, nanos)? } else { t }
        }
    };
    Some((ns, rest))
}

/// A tag, `name[pid]:` or `name:`: its name and pid, and what follows.
fn tag(s: &str) -> Option<(&str, Option<&str>, &str)> {
    let (word, rest) = field(s);
    let word = word.strip_suffix(':')?;
    let (name, pid) = match word.split_once('[') {
        Some((n, p)) => (n, Some(p.strip_suffix(']')?)),
        None => (word, None),
    };
    // A program's name: not a number (a router's sequence number, `123:`, is not one).
    (!name.is_empty() && name.len() <= 64 && name.bytes().all(|b| b.is_ascii_graphic() && b != b'[' && b != b']') && !name.bytes().all(|b| b.is_ascii_digit())).then_some((name, pid, rest))
}

/// A message, read: RFC 5424's when its version follows the priority, else RFC 3164's as far as it
/// goes (its time, host and tag each optional), else its text whole.
pub(crate) fn parse(s: &str, zone: Zone, now_ns: i64) -> Message<'_> {
    let s = s.trim_end_matches(['\n', '\r', '\0']);
    let Some((p, rest)) = pri(s) else { return Message { msg: s, ..Message::default() } };
    let mut m = Message { pri: Some(p), msg: rest, ..Message::default() };
    // RFC 5424: `1 TIMESTAMP HOSTNAME APP-NAME PROCID MSGID SD [MSG]`.
    if let Some(after) = rest.strip_prefix("1 ") {
        let (ts, r) = field(after);
        let (host, r) = field(r);
        let (app, r) = field(r);
        let (procid, r) = field(r);
        let (msgid, r) = field(r);
        // Its time is a time or `-`, its structured data `-` or `[...]`: else it is not RFC 5424's.
        let mut sd = Vec::new();
        let used = if r == "-" || r.starts_with("- ") {
            Some(1)
        } else if r.starts_with('[') {
            structured(r, &mut sd)
        } else {
            None
        };
        let ts_ok = ts == "-" || crate::observe::cloud::parse_time_ns(ts).is_some();
        if let Some(used) = used.filter(|_| ts_ok) {
            m.ts = nil(ts).and_then(crate::observe::cloud::parse_time_ns);
            m.host = nil(host);
            m.app = nil(app);
            m.procid = nil(procid);
            m.msgid = nil(msgid);
            m.sd = sd;
            let msg = r.get(used..).unwrap_or("");
            let msg = msg.strip_prefix(' ').unwrap_or(msg);
            m.msg = msg.strip_prefix('\u{feff}').unwrap_or(msg);
            return m;
        }
    }
    // RFC 3164: `TIMESTAMP HOSTNAME TAG: MSG`, or an RFC 3339 time where the timestamp goes.
    let mut r = rest;
    if let Some((ns, after)) = bsd_time(r, zone, now_ns) {
        m.ts = Some(ns);
        r = after;
    } else if let Some(ns) = crate::observe::cloud::parse_time_ns(field(r).0) {
        m.ts = Some(ns);
        r = field(r).1;
    }
    if m.ts.is_some() {
        match tag(r) {
            // No host: the tag right after the time.
            Some((name, pid, after)) => {
                m.app = Some(name);
                m.procid = pid;
                r = after;
            }
            None => {
                let (host, after) = field(r);
                m.host = nil(host);
                r = after;
                if let Some((name, pid, after)) = tag(r) {
                    m.app = Some(name);
                    m.procid = pid;
                    r = after;
                }
            }
        }
    } else if let Some((name, pid, after)) = tag(r) {
        m.app = Some(name);
        m.procid = pid;
        r = after;
    }
    m.msg = r;
    m
}

impl Compiled {
    /// A syslog message's record: its severity and facility its priority's, its host its sender's
    /// (as the message names it, else its address), its service the source's or its app's.
    pub(crate) fn syslog_record(&self, text: &str, from: SocketAddr, zone: Zone, now_ns: i64) -> Option<Record> {
        let m = parse(text, zone, now_ns);
        let mut message = m.msg.to_owned();
        if !self.keep(&mut message) || message.trim().is_empty() {
            return None;
        }
        let mut attrs = Vec::with_capacity(m.sd.len() + self.tags.len() + 5);
        if let Some(p) = m.pri {
            attrs.push(("syslog.facility".into(), FACILITIES.get(usize::from(p >> 3)).copied().unwrap_or("?").to_owned()));
        }
        for (k, v) in [("syslog.appname", m.app), ("syslog.procid", m.procid), ("syslog.msgid", m.msgid)] {
            if let Some(v) = v {
                attrs.push((k.to_owned(), v.to_owned()));
            }
        }
        attrs.push(("network.client.ip".into(), from.ip().to_string()));
        attrs.extend(m.sd);
        attrs.extend(self.tags.iter().cloned());
        let ts = m.ts.filter(|&t| t <= now_ns + super::AHEAD_MS * 1_000_000).unwrap_or(now_ns);
        Some(Record {
            ts,
            severity: m.pri.map_or_else(|| super::super::cloud::severity_of(&message), |p| super::syslog(p & 7)),
            service: self.service.clone().or_else(|| m.app.map(str::to_owned)).unwrap_or_else(|| self.name.clone()),
            host: m.host.map_or_else(|| from.ip().to_string(), str::to_owned),
            source: self.source.clone(),
            message: crate::observe::cloud::message(message),
            attrs,
            ..Record::default()
        })
    }
}

/// What the listeners count, for the source's state.
#[derive(Default)]
struct Counts {
    bytes: AtomicU64,
    dropped: AtomicU64,
    connections: AtomicU64,
    excluded: AtomicU64,
}

/// A listen address: `udp://host:port` or `tcp://host:port`.
pub(crate) fn listen(s: &str) -> Result<(bool, SocketAddr), String> {
    let (udp, addr) = if let Some(a) = s.strip_prefix("udp://") {
        (true, a)
    } else if let Some(a) = s.strip_prefix("tcp://") {
        (false, a)
    } else {
        return Err(format!("a listen address: udp://host:port or tcp://host:port, not {s:?}"));
    };
    addr.parse::<SocketAddr>().map(|a| (udp, a)).map_err(|_| format!("a listen address's host and port (an IP address, 0.0.0.0 for every one): {s}"))
}

/// The syslog source's thread: its listeners bound and served by tasks until it is told to stop;
/// their records offered from here, held back while there is no room.
pub(crate) fn run(c: Compiled, stop: &Stop, state: &Mutex<State>, sink: &Sink) {
    let c = Arc::new(c);
    let zone = c.timezone;
    let shutdown = crate::rt::Shutdown::new();
    let counts = Arc::new(Counts::default());
    let (tx, rx) = async_channel::bounded::<Record>(WAITING);
    let mut errors: Vec<String> = Vec::new();
    for l in &c.listen {
        let (udp, addr) = match listen(l) {
            Ok(x) => x,
            Err(e) => {
                errors.push(e);
                continue;
            }
        };
        let (c2, tx2, counts2, sd) = (c.clone(), tx.clone(), counts.clone(), shutdown.clone());
        if udp {
            match std::net::UdpSocket::bind(addr) {
                Ok(sock) => {
                    crate::rt::spawn(async move { serve_udp(sock, c2, zone, tx2, counts2, sd).await });
                }
                Err(e) => errors.push(format!("udp://{addr}: {e}")),
            }
        } else {
            match std::net::TcpListener::bind(addr) {
                Ok(l) => {
                    crate::rt::spawn(async move { serve_tcp(l, c2, zone, tx2, counts2, sd).await });
                }
                Err(e) => errors.push(format!("tcp://{addr}: {e}")),
            }
        }
    }
    drop(tx);
    let error = (!errors.is_empty()).then(|| errors.join("; "));
    let mut batch: Batch<()> = Batch::default();
    let mut flight: InFlight<()> = InFlight::default();
    let (mut records, mut last_ms) = (0u64, None);
    let offer = |r: Vec<Record>, a: Ack| sink(r, a);
    while !stop.stopped() {
        // Offered while there is room: a refused offer is held, and nothing more taken meanwhile.
        if batch.is_empty() || flight.offer(&mut batch, &offer) {
            while !batch.full() {
                match rx.try_recv() {
                    Ok(r) => batch.push(r),
                    Err(_) => break,
                }
            }
            if !batch.records.is_empty() {
                batch.marks.push(());
                let _ = flight.offer(&mut batch, &offer);
            }
        }
        flight.written(|_, n| {
            records += n;
            last_ms = Some(crate::observe::clock_ms());
        });
        {
            let mut s = super::lock(state);
            s.records = records;
            s.in_flight = flight.records();
            s.bytes = counts.bytes.load(Ordering::Relaxed);
            s.dropped = counts.dropped.load(Ordering::Relaxed);
            s.connections = counts.connections.load(Ordering::Relaxed);
            s.excluded = counts.excluded.load(Ordering::Relaxed);
            s.last_ms = last_ms;
            s.error.clone_from(&error);
            s.errors = errors.len() as u64;
        }
        if rx.is_empty() {
            stop.sleep(Duration::from_millis(50));
        }
    }
    shutdown.cancel();
    flight.drain(|_, n| records += n);
    super::lock(state).records = records;
}

async fn serve_udp(sock: std::net::UdpSocket, c: Arc<Compiled>, zone: Zone, tx: async_channel::Sender<Record>, counts: Arc<Counts>, stop: crate::rt::Shutdown) {
    let Ok(sock) = hyper_rt::udp::UdpSocket::adopt(sock.into()) else { return };
    let mut buf = vec![0u8; DATAGRAM];
    loop {
        let got = futures::select_biased! {
            () = stop.cancelled().fuse() => return,
            r = sock.recv_from(&mut buf).fuse() => r,
        };
        let Ok((n, from)) = got else {
            crate::rt::sleep(Duration::from_millis(20)).await;
            continue;
        };
        counts.bytes.fetch_add(n as u64, Ordering::Relaxed);
        let text = String::from_utf8_lossy(buf.get(..n).unwrap_or_default());
        let now_ns = crate::observe::clock_ms().saturating_mul(1_000_000);
        match c.syslog_record(&text, from, zone, now_ns) {
            // A datagram has no one to hold back: no room, it is dropped and counted.
            Some(r) => {
                if tx.try_send(r).is_err() {
                    counts.dropped.fetch_add(1, Ordering::Relaxed);
                }
            }
            None => {
                counts.excluded.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

async fn serve_tcp(listener: std::net::TcpListener, c: Arc<Compiled>, zone: Zone, tx: async_channel::Sender<Record>, counts: Arc<Counts>, stop: crate::rt::Shutdown) {
    let Ok(listener) = hyper_rt::tcp::TcpListener::from_owned(hyper_rt::tcp::OwnedStream::from(listener), 64) else { return };
    loop {
        let accepted = futures::select_biased! {
            () = stop.cancelled().fuse() => return,
            r = listener.accept().fuse() => r,
        };
        let Ok(stream) = accepted else {
            crate::rt::sleep(Duration::from_millis(100)).await;
            continue;
        };
        if counts.connections.load(Ordering::Relaxed) >= CONNECTIONS as u64 {
            continue;
        }
        counts.connections.fetch_add(1, Ordering::Relaxed);
        let (c, tx, counts, stop) = (c.clone(), tx.clone(), counts.clone(), stop.clone());
        crate::rt::spawn(async move {
            let _ = crate::discovery::until_stopped(&stop, connection(&stream, &c, zone, &tx, &counts)).await;
            counts.connections.fetch_sub(1, Ordering::Relaxed);
        });
    }
}

/// One TCP connection's messages, framed as RFC 6587 says: counted (`<length> <message>`, its first
/// byte a digit) or each ended by a newline (or a NUL, as some senders end them).
async fn connection(stream: &hyper_rt::tcp::TcpStream, c: &Compiled, zone: Zone, tx: &async_channel::Sender<Record>, counts: &Counts) {
    let Ok(from) = stream.peer_addr() else { return };
    let mut buf: Vec<u8> = Vec::with_capacity(64 << 10);
    let mut chunk = vec![0u8; 64 << 10];
    // Bytes of a message too long to take, still to skip.
    let mut skip = 0usize;
    loop {
        let n = match crate::rt::timeout(IDLE, stream.read(&mut chunk)).await {
            Ok(Ok(n)) if n > 0 => n,
            _ => return,
        };
        counts.bytes.fetch_add(n as u64, Ordering::Relaxed);
        buf.extend_from_slice(chunk.get(..n).unwrap_or_default());
        let mut at = 0usize;
        loop {
            let rest = buf.get(at..).unwrap_or_default();
            if skip > 0 {
                let k = skip.min(rest.len());
                skip -= k;
                at += k;
                if skip > 0 {
                    break;
                }
                continue;
            }
            let Some(&first) = rest.first() else { break };
            let message: Option<(usize, &[u8])> = if first.is_ascii_digit() {
                // Counted: its length, a space, then that many bytes.
                let Some(sp) = rest.iter().take(8).position(|&b| b == b' ') else {
                    if rest.len() >= 8 {
                        return; // Not framing anyone uses: the connection is let go.
                    }
                    break;
                };
                let Some(len) = std::str::from_utf8(rest.get(..sp).unwrap_or_default()).ok().and_then(|l| l.parse::<usize>().ok()) else { return };
                if len > MAX_LINE {
                    skip = len;
                    at += sp + 1;
                    continue;
                }
                match rest.get(sp + 1..sp + 1 + len) {
                    Some(m) => Some((sp + 1 + len, m)),
                    None => break,
                }
            } else {
                match rest.iter().position(|&b| b == b'\n' || b == 0) {
                    Some(i) => Some((i + 1, rest.get(..i).unwrap_or_default())),
                    None if rest.len() > MAX_LINE => {
                        // Too long without its end: what there is taken, the rest of it skipped.
                        Some((rest.len(), rest.get(..MAX_LINE).unwrap_or_default()))
                    }
                    None => break,
                }
            };
            let Some((used, m)) = message else { break };
            at += used;
            let text = String::from_utf8_lossy(m);
            let now_ns = crate::observe::clock_ms().saturating_mul(1_000_000);
            match c.syslog_record(&text, from, zone, now_ns) {
                // A sender is held back while there is no room: its connection is not read.
                Some(r) => {
                    if tx.send(r).await.is_err() {
                        return;
                    }
                }
                None => {
                    counts.excluded.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        buf.drain(..at.min(buf.len()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc() -> Zone {
        Zone::UTC
    }

    #[test]
    fn rfc_5424_reads_whole_with_its_structured_data() {
        let s = r#"<165>1 2003-10-11T22:14:15.003Z mymachine.example.com evntslog - ID47 [exampleSDID@32473 iut="3" eventSource="Application" eventID="1011"][examplePriority@32473 class="high"] An application event log entry"#;
        let m = parse(s, utc(), 0);
        assert_eq!(m.pri, Some(165));
        assert_eq!(m.ts, Some(1_065_910_455_003_000_000));
        assert_eq!((m.host, m.app, m.procid, m.msgid), (Some("mymachine.example.com"), Some("evntslog"), None, Some("ID47")));
        assert_eq!(m.sd.len(), 4);
        assert!(m.sd.contains(&("sd.exampleSDID@32473.eventSource".to_owned(), "Application".to_owned())));
        assert_eq!(m.msg, "An application event log entry");
        // Escapes in a value; no structured data; a byte order mark before the message.
        let m = parse("<34>1 2003-10-11T22:14:15Z host app 42 - [a b=\"x\\\"y\\]z\"] hi", utc(), 0);
        assert_eq!((m.procid, m.sd.first().map(|s| s.1.as_str()), m.msg), (Some("42"), Some("x\"y]z"), "hi"));
        let m = parse("<34>1 - - - - - - \u{feff}text", utc(), 0);
        assert_eq!((m.ts, m.host, m.msg), (None, None, "text"));
    }

    #[test]
    fn rfc_3164_reads_as_far_as_it_goes() {
        let now = 1_792_324_800_000_000_000; // 2026-10-18T12:00:00Z
        let m = parse("<34>Oct 11 22:14:15 mymachine su: 'su root' failed for lonvick on /dev/pts/8", utc(), now);
        assert_eq!((m.pri, m.host, m.app, m.procid), (Some(34), Some("mymachine"), Some("su"), None));
        assert_eq!(m.msg, "'su root' failed for lonvick on /dev/pts/8");
        assert_eq!(m.ts, Some(1_791_756_855_000_000_000)); // 2026-10-11T22:14:15Z
        // A day of one digit, a pid, no host; and a timezone.
        let m = parse("<13>Oct  1 02:03:04 sshd[812]: Accepted publickey", utc(), now);
        assert_eq!((m.host, m.app, m.procid, m.msg), (None, Some("sshd"), Some("812"), "Accepted publickey"));
        let la = Zone::named("America/Los_Angeles").unwrap_or(Zone::UTC);
        let m = parse("<13>Oct 11 15:14:15 web nginx: x", la, now);
        assert_eq!(m.ts, Some(1_791_756_855_000_000_000)); // 15:14:15 PDT is 22:14:15 UTC
        // A December message read in January: last year's.
        let jan = 1_798_761_600_000_000_000; // 2027-01-01T00:00:00Z
        let m = parse("<13>Dec 31 23:59:00 h a: x", utc(), jan);
        assert_eq!(m.ts, Some(1_798_761_540_000_000_000));
        // An RFC 3339 time where the BSD one goes (rsyslog's forward format).
        let m = parse("<30>2026-10-18T12:00:01.5Z web-1 nginx[9]: GET /", utc(), now);
        assert_eq!((m.ts, m.host, m.app, m.msg), (Some(1_792_324_801_500_000_000), Some("web-1"), Some("nginx"), "GET /"));
        // What does not parse is the message whole.
        assert_eq!(parse("plain text", utc(), now).msg, "plain text");
        assert_eq!(parse("<999>x", utc(), now), Message { msg: "<999>x", ..Message::default() });
        let m = parse("<189>123: *Mar  1 00:00:00.000: %SYS-5-CONFIG_I: Configured", utc(), now);
        assert_eq!((m.pri, m.app, m.msg), (Some(189), None, "123: *Mar  1 00:00:00.000: %SYS-5-CONFIG_I: Configured"));
        // Text that begins as RFC 5424's version does is not taken for it.
        let m = parse("<13>1 apple a day", utc(), now);
        assert_eq!((m.ts, m.host, m.msg), (None, None, "1 apple a day"));
    }

    #[test]
    fn listen_addresses_are_checked() {
        assert_eq!(listen("udp://0.0.0.0:514").map(|x| x.0), Ok(true));
        assert_eq!(listen("tcp://[::1]:6514").map(|x| x.0), Ok(false));
        assert!(listen("udp://localhost:514").is_err() && listen("0.0.0.0:514").is_err() && listen("tcp://1.2.3.4").is_err());
    }
}
