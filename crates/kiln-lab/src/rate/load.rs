//! The generator: `lanes` threads, each with its share of the connections in a poller of its own, sending its
//! share of the rate as each request falls due ([`Lane`]) and reading the answers as they come, while the main
//! thread samples the server. The connections open first (WebSockets upgraded and their hellos answered), then
//! the clock starts: the warm-up at the rate, the window, and the drain, where the window's backlog may still go
//! out and what's in flight has until its timeout.

use std::collections::{HashMap, VecDeque};
use std::io::{self, Read as _, Write as _};
use std::net::{SocketAddr, TcpStream};
use std::num::NonZeroUsize;
use std::sync::{Barrier, OnceLock};
use std::time::{Duration, Instant};

use polling::Events;

use super::lane::{Lane, Ns, Outcome, Req, Schedule, Seen};
use super::os::{self, Sockets};
use super::target::{Kind, Pick, Template, Vars, Workload, WsRpc};
use super::wire::{self, Answer, Found, H1, Ws, WsIn};

/// A hello's `${id}`: 10^12 + the socket's number x 100 + the hello's place, past any timed request's (a million
/// a second for a day is 8.64 x 10^10) and below 2^53, the integers JSON keeps exact.
const HELLO_IDS: u64 = 1_000_000_000_000;

/// The mask a client's frames carry (RFC 6455 asks for one; servers don't read meaning into it): the lab's own
/// client's (`crate::client::Socket`).
const MASK: [u8; 4] = [0x37, 0xfa, 0x21, 0x3d];

/// A connection that couldn't open is tried again a second later, as the lab's viewers try again a second after a
/// failed attach (`crate::lab`, `viewer`).
const RETRY: Ns = 1_000_000_000;

/// How far ahead of a send's due time a thread stops sleeping and spins, until it has measured its own oversleeps:
/// Linux's default timer slack (50 µs, prctl(2)), the most a timed wake runs late there by design.
const FIRST_MARGIN: Ns = 50_000;

/// The oversleeps a thread keeps to set its margin by: its last 64, their 90th percentile (so nine sends in ten
/// are on time to the microsecond, and a rare long oversleep doesn't set the thread spinning for good).
const OVERSLEEPS: usize = 64;

/// Room for the events of one wait: a thread's connections at a million a second (2,000 over 8 threads) fit
/// four times over.
const EVENTS: usize = 1024;

/// Distinct errors kept for the report.
const ERRORS_KEPT: usize = 8;

fn ns(d: Duration) -> Ns {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

/// A run: its rate, threads and connections, and its phases.
#[derive(Clone, Copy, Debug)]
pub struct Plan {
    pub rate: u64,
    pub lanes: usize,
    pub conns: usize,
    pub warm: Duration,
    pub window: Duration,
    /// How long after the window the drain still sends what the window left waiting for a connection.
    pub grace: Duration,
    /// How long a request may wait for its answer (and a connection to open, a WebSocket to upgrade).
    pub timeout: Duration,
}

/// A workload made ready to send: its requests' bytes (made once where every request is the same), its upgrade,
/// and what its answers are matched by.
pub struct Job<'a> {
    w: &'a Workload,
    vars: &'a Vars,
    host: String,
    fixed: Option<Vec<u8>>,
    upgrade: Vec<u8>,
    /// ws-rpc: what's picked out of each answer: its id, then `final`'s pointer and `ok`'s where they're set.
    wants: Vec<Vec<String>>,
}

fn render_headers(h: &[(String, Template)], vars: &Vars, pick: Pick, id: u64, n: u64) -> Result<Vec<(String, String)>, String> {
    h.iter()
        .map(|(k, t)| {
            let v = t.render(vars, pick, id, n)?;
            if v.contains(['\r', '\n']) {
                return Err(format!("header {k}'s value holds a line break"));
            }
            Ok((k.clone(), v))
        })
        .collect()
}

fn checked_path(path: String) -> Result<String, String> {
    if path.contains([' ', '\r', '\n']) { Err("a path holds a space or a line break".into()) } else { Ok(path) }
}

impl<'a> Job<'a> {
    pub fn new(w: &'a Workload, vars: &'a Vars, port: u16) -> Result<Self, String> {
        let mut job = Self { w, vars, host: format!("127.0.0.1:{port}"), fixed: None, upgrade: Vec::new(), wants: Vec::new() };
        match &w.kind {
            Kind::Http(h) => {
                // Rendered once whatever it is, so a mistake is said before anything is timed.
                let mut first = Vec::new();
                job.http(0, &mut first)?;
                let fixed = h.path.fixed(vars, w.pick) && h.headers.iter().all(|(_, t)| t.fixed(vars, w.pick)) && h.body.as_ref().is_none_or(|b| b.fixed(vars, w.pick));
                if fixed {
                    job.fixed = Some(first);
                }
            }
            Kind::WsRpc(r) => {
                let path = checked_path(r.path.render(vars, w.pick, 0, 0)?)?;
                job.upgrade = wire::upgrade(&path, &job.host, &render_headers(&r.headers, vars, w.pick, 0, 0)?);
                job.wants.push(r.id.clone());
                job.wants.extend(r.last.iter().chain(r.ok.iter()).map(|t| t.at().to_vec()));
                let mut first = Vec::new();
                job.ws_request(0, &mut first)?;
            }
        }
        Ok(job)
    }

    /// How many requests a connection carries at once.
    pub fn depth(&self) -> u32 {
        match &self.w.kind {
            Kind::Http(h) => h.pipeline,
            Kind::WsRpc(r) => r.in_flight,
        }
    }

    /// Request `k`'s bytes (its `${id}` `k + 1`) onto `out`.
    fn http(&self, k: u64, out: &mut Vec<u8>) -> Result<(), String> {
        let Kind::Http(h) = &self.w.kind else { return Err("not an http workload".into()) };
        let (id, pick) = (k + 1, self.w.pick);
        let path = checked_path(h.path.render(self.vars, pick, id, k)?)?;
        let headers = render_headers(&h.headers, self.vars, pick, id, k)?;
        let body = h.body.as_ref().map(|b| b.render(self.vars, pick, id, k)).transpose()?;
        out.extend_from_slice(&wire::request(&h.method, &path, &self.host, &headers, body.as_deref().map(str::as_bytes)));
        Ok(())
    }

    /// Request `k`'s frame onto `out`.
    fn ws_request(&self, k: u64, out: &mut Vec<u8>) -> Result<(), String> {
        let Kind::WsRpc(r) = &self.w.kind else { return Err("not a ws-rpc workload".into()) };
        let text = r.request.render(self.vars, self.w.pick, k + 1, k)?;
        wire::frame(0x1, text.as_bytes(), MASK, out);
        Ok(())
    }

    fn rpc(&self) -> Option<&WsRpc> {
        match &self.w.kind {
            Kind::WsRpc(r) => Some(r),
            Kind::Http(_) => None,
        }
    }

    fn head_only(&self) -> bool {
        matches!(&self.w.kind, Kind::Http(h) if h.method == "HEAD")
    }

    fn good_status(&self, status: u16) -> bool {
        match &self.w.kind {
            Kind::Http(h) => h.ok_status.as_ref().map_or(status < 400, |ok| ok.contains(&status)),
            Kind::WsRpc(_) => false,
        }
    }
}

/// What a run's threads saw, together.
#[derive(Debug, Default)]
pub struct Ran {
    pub seen: Seen,
    /// The window's requests never sent: still waiting for a connection when the drain ended.
    pub unsent: u64,
    /// Connections open as the clock started.
    pub opened: usize,
    /// Connections that wouldn't open (at the start, or again after one ended), and those opened again.
    pub refused: u64,
    pub reconnects: u64,
    /// ws-rpc messages that matched no request in flight (pushes, late answers).
    pub unmatched: u64,
    /// Each thread's CPU over the window, less its spinning before sends (its work), and its spinning, as shares
    /// of the window; none where the OS won't say.
    pub busy: Vec<Option<f64>>,
    pub spin: Vec<f64>,
    /// What the threads got of their OS to be punctual.
    pub punctual: String,
    pub errors: Vec<String>,
}

impl Ran {
    fn note(&mut self, e: String) {
        if self.errors.len() < ERRORS_KEPT && !self.errors.contains(&e) {
            self.errors.push(e);
        }
    }
}

/// One thread's connection.
#[derive(Default)]
struct Conn {
    open: bool,
    /// HTTP: its answers' reader, and what's in flight on it, in order.
    h1: Option<H1>,
    in_flight: VecDeque<Req>,
    /// ws-rpc: its frames' reader.
    ws: Ws,
    /// What it has to write that its socket didn't take yet.
    out: Vec<u8>,
    /// Closed: when to open it again.
    retry_at: Option<Ns>,
}

/// The 90th percentile of a thread's last oversleeps ([`OVERSLEEPS`]): how far ahead of a send it stops sleeping.
struct Margin {
    seen: Vec<Ns>,
    at: usize,
    ns: Ns,
}

impl Margin {
    fn new() -> Self {
        Self { seen: Vec::with_capacity(OVERSLEEPS), at: 0, ns: FIRST_MARGIN }
    }

    fn saw(&mut self, over: Ns) {
        if self.seen.len() < OVERSLEEPS {
            self.seen.push(over);
        } else if let Some(s) = self.seen.get_mut(self.at) {
            *s = over;
        }
        self.at = (self.at + 1) % OVERSLEEPS;
        let mut v = self.seen.clone();
        v.sort_unstable();
        let rank = (v.len() * 9).div_ceil(10).max(1);
        self.ns = v.get(rank - 1).copied().unwrap_or(FIRST_MARGIN);
    }
}

/// One generator thread.
struct Gen<'a> {
    job: &'a Job<'a>,
    addr: SocketAddr,
    plan: Plan,
    sockets: Sockets,
    conns: Vec<Conn>,
    /// How many of them are closed (so the loop looks for one to open again only when there is one).
    closed: usize,
    /// The number of this thread's first socket among all the run's (a hello's `${id}` says it).
    first_socket: u64,
    lane: Lane,
    t0: Instant,
    /// ws-rpc: what's in flight, by `${id}`: its connection and itself.
    pending: HashMap<u64, (usize, Req)>,
    scratch: Vec<u8>,
    buf: Vec<u8>,
    answers: Vec<Answer>,
    msgs: Vec<WsIn>,
    out: Ran,
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Writes `bytes` to connection `c`, or keeps what its socket won't take now (and watches for room to write).
fn put_out(sockets: &mut Sockets, conn: &mut Conn, c: usize, bytes: &[u8]) -> io::Result<()> {
    if !conn.out.is_empty() {
        conn.out.extend_from_slice(bytes);
        return Ok(());
    }
    let Some(s) = sockets.get(c) else { return Err(io::ErrorKind::NotConnected.into()) };
    let mut w: &TcpStream = s;
    let mut at = 0;
    while at < bytes.len() {
        match w.write(bytes.get(at..).unwrap_or_default()) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => at += n,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    if at < bytes.len() {
        conn.out.extend_from_slice(bytes.get(at..).unwrap_or_default());
        sockets.want_write(c, true)?;
    }
    Ok(())
}

impl<'a> Gen<'a> {
    fn new(addr: SocketAddr, job: &'a Job<'a>, plan: Plan, index: usize, conns: usize, first_socket: u64) -> Result<Self, String> {
        let lanes = plan.lanes.max(1);
        let warm = ns(plan.warm);
        let end = warm.saturating_add(ns(plan.window));
        let sched = Schedule { rate: plan.rate.max(1), lanes: lanes as u64, lane: index as u64 };
        let mut g = Self {
            job,
            addr,
            plan,
            sockets: Sockets::new().map_err(|e| format!("a poller: {e}"))?,
            conns: (0..conns).map(|_| Conn::default()).collect(),
            closed: 0,
            first_socket,
            lane: Lane::new(sched, warm, end, end.saturating_add(ns(plan.grace))),
            t0: Instant::now(),
            pending: HashMap::new(),
            scratch: vec![0; 64 << 10],
            buf: Vec::new(),
            answers: Vec::new(),
            msgs: Vec::new(),
            out: Ran::default(),
        };
        for c in 0..conns {
            match g.open(first_socket + c as u64) {
                Ok((s, ws)) => match g.sockets.put(c, s) {
                    Ok(()) => {
                        if let Some(conn) = g.conns.get_mut(c) {
                            (conn.open, conn.ws, conn.h1) = (true, ws, Some(H1::new(job.head_only())));
                        }
                        g.lane.add_conn(c, job.depth(), 0);
                        g.out.opened += 1;
                    }
                    Err(e) => g.out.note(format!("watch: {e}")),
                },
                Err(e) => {
                    g.out.refused += 1;
                    g.out.note(e);
                    if let Some(conn) = g.conns.get_mut(c) {
                        conn.retry_at = Some(RETRY);
                    }
                }
            }
        }
        g.closed = g.conns.iter().filter(|x| !x.open).count();
        Ok(g)
    }

    fn now(&self) -> Ns {
        ns(self.t0.elapsed())
    }

    /// Opens socket number `socket` (blocking): connected, upgraded and greeted where it's a WebSocket, then
    /// non-blocking; with the frames that came after its hellos.
    fn open(&self, socket: u64) -> Result<(TcpStream, Ws), String> {
        let within = self.plan.timeout;
        let s = TcpStream::connect_timeout(&self.addr, within).map_err(|e| format!("connect: {e}"))?;
        s.set_nodelay(true).map_err(|e| format!("nodelay: {e}"))?;
        let mut ws = Ws::default();
        if let Some(r) = self.job.rpc() {
            let io = |e: io::Error| format!("upgrade: {e}");
            s.set_read_timeout(Some(within)).map_err(io)?;
            s.set_write_timeout(Some(within)).map_err(io)?;
            let mut rw: &TcpStream = &s;
            rw.write_all(&self.job.upgrade).map_err(io)?;
            let mut head = Vec::new();
            let mut chunk = [0u8; 4096];
            let end = loop {
                if let Some(i) = find(&head, b"\r\n\r\n") {
                    break i;
                }
                if head.len() > 64 << 10 {
                    return Err("upgrade: no end to its answer's head".into());
                }
                let n = rw.read(&mut chunk).map_err(io)?;
                if n == 0 {
                    return Err("upgrade: closed before answering".into());
                }
                head.extend_from_slice(chunk.get(..n).unwrap_or_default());
            };
            let first = String::from_utf8_lossy(head.get(..end).unwrap_or_default()).lines().next().unwrap_or("").to_owned();
            if !first.starts_with("HTTP/1.1 101") {
                return Err(format!("upgrade: answered {first:?}"));
            }
            let mut msgs = Vec::new();
            ws.feed(head.get(end + 4..).unwrap_or_default(), &mut msgs)?;
            let pick = self.job.w.pick;
            for (i, h) in r.hello.iter().enumerate() {
                let id = HELLO_IDS + socket * 100 + i as u64;
                let mut f = Vec::new();
                wire::frame(0x1, h.send.render(self.job.vars, pick, id, socket)?.as_bytes(), MASK, &mut f);
                rw.write_all(&f).map_err(|e| format!("hello: {e}"))?;
                let Some(until) = &h.until else { continue };
                let want = until.render(self.job.vars, pick, id, socket)?;
                let deadline = Instant::now() + within;
                'answered: loop {
                    for m in msgs.drain(..) {
                        match m {
                            WsIn::Text(t) if find(&t, want.as_bytes()).is_some() => break 'answered,
                            WsIn::Ping(p) => {
                                let mut f = Vec::new();
                                wire::frame(0xA, &p, MASK, &mut f);
                                rw.write_all(&f).map_err(|e| format!("hello: {e}"))?;
                            }
                            WsIn::Close => return Err("hello: the socket closed".into()),
                            WsIn::Text(_) | WsIn::Binary(_) => {}
                        }
                    }
                    if Instant::now() >= deadline {
                        return Err(format!("hello {i}: nothing said {want:?} within {within:?}"));
                    }
                    let n = rw.read(&mut chunk).map_err(|e| format!("hello: {e}"))?;
                    if n == 0 {
                        return Err("hello: the socket closed".into());
                    }
                    ws.feed(chunk.get(..n).unwrap_or_default(), &mut msgs)?;
                }
            }
            s.set_read_timeout(None).map_err(io)?;
            s.set_write_timeout(None).map_err(io)?;
        }
        s.set_nonblocking(true).map_err(|e| format!("non-blocking: {e}"))?;
        Ok((s, ws))
    }

    /// Sends `req` on `c`.
    fn send(&mut self, c: usize, req: Req) {
        self.buf.clear();
        let made = match (&self.job.w.kind, &self.job.fixed) {
            (Kind::Http(_), Some(b)) => {
                self.buf.extend_from_slice(b);
                Ok(())
            }
            (Kind::Http(_), None) => self.job.http(req.k, &mut self.buf),
            (Kind::WsRpc(_), _) => self.job.ws_request(req.k, &mut self.buf),
        };
        if let Err(e) = made {
            self.out.note(e);
            self.lane.done(c, req, req.sent, Outcome::Bad, 0, true);
            return;
        }
        let Gen { sockets, conns, buf, pending, job, .. } = self;
        let Some(conn) = conns.get_mut(c) else { return };
        if job.rpc().is_some() {
            pending.insert(req.k + 1, (c, req));
        } else {
            conn.in_flight.push_back(req);
        }
        if let Err(e) = put_out(sockets, conn, c, buf) {
            self.out.note(format!("write: {e}"));
            self.kill(c, req.sent, Outcome::Closed);
        }
    }

    /// Writes what `c` kept back, now its socket has room.
    fn flush(&mut self, c: usize) {
        let Gen { sockets, conns, .. } = self;
        let (Some(conn), Some(s)) = (conns.get_mut(c), sockets.get(c)) else { return };
        let mut w: &TcpStream = s;
        let mut at = 0;
        let mut failed = None;
        while at < conn.out.len() {
            match w.write(conn.out.get(at..).unwrap_or_default()) {
                Ok(0) => {
                    failed = Some(io::Error::from(io::ErrorKind::WriteZero));
                    break;
                }
                Ok(n) => at += n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => {
                    failed = Some(e);
                    break;
                }
            }
        }
        conn.out.drain(..at);
        let empty = conn.out.is_empty();
        if let Some(e) = failed.or_else(|| if empty { sockets.want_write(c, false).err() } else { None }) {
            self.out.note(format!("write: {e}"));
            let now = self.now();
            self.kill(c, now, Outcome::Closed);
        }
    }

    /// Reads what `c` has, ending each request it answers.
    fn read(&mut self, c: usize) {
        loop {
            let got = match self.sockets.get(c) {
                Some(s) => {
                    let mut r: &TcpStream = s;
                    r.read(&mut self.scratch)
                }
                None => return,
            };
            match got {
                Ok(0) => {
                    let now = self.now();
                    // A body to the close ends here.
                    if let Some(h1) = self.conns.get_mut(c).and_then(|x| x.h1.as_mut()) {
                        self.answers.clear();
                        h1.eof(&mut self.answers);
                    }
                    self.answered(c, now);
                    self.kill(c, now, Outcome::Closed);
                    return;
                }
                Ok(n) => {
                    let now = self.now();
                    if !self.take_in(c, n, now) {
                        return;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => {
                    self.out.note(format!("read: {e}"));
                    let now = self.now();
                    self.kill(c, now, Outcome::Closed);
                    return;
                }
            }
        }
    }

    /// Ends the requests `self.answers` answers on `c`, in order: whether `c` stays open.
    fn answered(&mut self, c: usize, now: Ns) -> bool {
        let mut closing = false;
        let mut orphan = false;
        let answers = std::mem::take(&mut self.answers);
        for a in &answers {
            let Some(req) = self.conns.get_mut(c).and_then(|x| x.in_flight.pop_front()) else {
                orphan = true;
                break;
            };
            let outcome = if self.job.good_status(a.status) { Outcome::Ok } else { Outcome::Bad };
            self.lane.done(c, req, now, outcome, a.bytes, !a.close);
            closing |= a.close;
        }
        self.answers = answers;
        self.answers.clear();
        if orphan {
            self.out.note("an answer to no request".into());
        }
        if closing || orphan {
            self.kill(c, now, Outcome::Closed);
            return false;
        }
        true
    }

    /// Takes in `n` bytes read off `c` at `now`: whether `c` stays open.
    fn take_in(&mut self, c: usize, n: usize, now: Ns) -> bool {
        let data = self.scratch.get(..n).unwrap_or_default();
        let Some(conn) = self.conns.get_mut(c) else { return false };
        if let Some(h1) = conn.h1.as_mut()
            && self.job.rpc().is_none()
        {
            self.answers.clear();
            if let Err(e) = h1.feed(data, &mut self.answers) {
                self.out.note(format!("answer: {e}"));
                self.kill(c, now, Outcome::Closed);
                return false;
            }
            return self.answered(c, now);
        }
        self.msgs.clear();
        if let Err(e) = conn.ws.feed(data, &mut self.msgs) {
            self.out.note(format!("frames: {e}"));
            self.kill(c, now, Outcome::Closed);
            return false;
        }
        self.messages(c, now)
    }

    /// Ends the requests `self.msgs` answers (each by its id); answers pings: whether `c` stays open.
    fn messages(&mut self, c: usize, now: Ns) -> bool {
        let job = self.job;
        let Some(r) = job.rpc() else { return true };
        let wants: Vec<&[String]> = job.wants.iter().map(Vec::as_slice).collect();
        let mut writes: Vec<(usize, Vec<u8>)> = Vec::new();
        let mut closed = false;
        let msgs = std::mem::take(&mut self.msgs);
        for m in &msgs {
            match m {
                WsIn::Text(t) => {
                    for msg in wire::messages(t) {
                        let found = wire::pick(msg, &wants);
                        let id = match found.first() {
                            Some(Some(Found::Scalar(s))) => s.parse::<u64>().ok(),
                            _ => None,
                        };
                        let mut at = 1;
                        let last = r.last.as_ref().is_none_or(|t| {
                            at += 1;
                            t.holds(found.get(1).and_then(Option::as_ref))
                        });
                        let Some(id) = id.filter(|id| self.pending.contains_key(id)) else {
                            self.out.unmatched += 1;
                            continue;
                        };
                        if !last {
                            // A part of its answer (a stream's chunk): it goes on.
                            continue;
                        }
                        let good = r.ok.as_ref().is_none_or(|t| t.holds(found.get(at).and_then(Option::as_ref)));
                        if let Some((on, req)) = self.pending.remove(&id) {
                            self.lane.done(on, req, now, if good { Outcome::Ok } else { Outcome::Bad }, msg.len() as u64, true);
                            if let Some(after) = &r.after {
                                match after.render(job.vars, job.w.pick, id, req.k) {
                                    Ok(text) => {
                                        let mut f = Vec::new();
                                        wire::frame(0x1, text.as_bytes(), MASK, &mut f);
                                        writes.push((on, f));
                                    }
                                    Err(e) => self.out.note(e),
                                }
                            }
                        }
                    }
                }
                WsIn::Binary(_) => self.out.unmatched += 1,
                WsIn::Ping(p) => {
                    let mut f = Vec::new();
                    wire::frame(0xA, p, MASK, &mut f);
                    writes.push((c, f));
                }
                WsIn::Close => closed = true,
            }
        }
        self.msgs = msgs;
        self.msgs.clear();
        for (on, f) in writes {
            let Gen { sockets, conns, .. } = self;
            let Some(conn) = conns.get_mut(on).filter(|x| x.open) else { continue };
            if let Err(e) = put_out(sockets, conn, on, &f) {
                self.out.note(format!("write: {e}"));
                self.kill(on, now, Outcome::Closed);
            }
        }
        if closed {
            self.kill(c, now, Outcome::Closed);
            return false;
        }
        self.conns.get(c).is_some_and(|x| x.open)
    }

    /// Connection `c` is gone at `now`: what it had in flight ends as `outcome`, and it opens again (at once, the
    /// first time).
    fn kill(&mut self, c: usize, now: Ns, outcome: Outcome) {
        drop(self.sockets.take(c));
        self.lane.drop_conn(c);
        let head_only = self.job.head_only();
        let Some(conn) = self.conns.get_mut(c) else { return };
        let was_open = conn.open;
        for req in conn.in_flight.drain(..) {
            self.lane.done(c, req, now, outcome, 0, false);
        }
        (conn.open, conn.h1, conn.ws) = (false, Some(H1::new(head_only)), Ws::default());
        conn.out.clear();
        if was_open {
            conn.retry_at = Some(now);
            self.closed += 1;
        }
        let gone: Vec<u64> = self.pending.iter().filter(|(_, (on, _))| *on == c).map(|(id, _)| *id).collect();
        for id in gone {
            if let Some((_, req)) = self.pending.remove(&id) {
                self.lane.done(c, req, now, outcome, 0, false);
            }
        }
    }

    /// Opens again the connections whose time to has come, while there's still something to send.
    fn reopen(&mut self, now: Ns) {
        let due: Vec<usize> = self.conns.iter().enumerate().filter(|(_, x)| !x.open && x.retry_at.is_some_and(|t| t <= now)).map(|(c, _)| c).collect();
        for c in due {
            if self.lane.next_due(now).is_none() {
                return;
            }
            match self.open(self.first_socket + c as u64).and_then(|(s, ws)| self.sockets.put(c, s).map(|()| ws).map_err(|e| format!("watch: {e}"))) {
                Ok(ws) => {
                    let head_only = self.job.head_only();
                    if let Some(conn) = self.conns.get_mut(c) {
                        (conn.open, conn.ws, conn.h1, conn.retry_at) = (true, ws, Some(H1::new(head_only)), None);
                        self.closed = self.closed.saturating_sub(1);
                    }
                    let opened = self.now();
                    self.lane.add_conn(c, self.job.depth(), opened);
                    self.out.reconnects += 1;
                }
                Err(e) => {
                    self.out.refused += 1;
                    self.out.note(e);
                    if let Some(conn) = self.conns.get_mut(c) {
                        conn.retry_at = Some(now.saturating_add(RETRY));
                    }
                }
            }
        }
    }

    /// Ends as timed out what's waited past the timeout (all of it, at the drain's end).
    fn expire(&mut self, now: Ns, all: bool) {
        let timeout = ns(self.plan.timeout);
        let late = |r: &Req| all || now.saturating_sub(r.sent) >= timeout;
        if self.job.rpc().is_some() {
            let ids: Vec<u64> = self.pending.iter().filter(|(_, (_, r))| late(r)).map(|(id, _)| *id).collect();
            for id in ids {
                if let Some((on, req)) = self.pending.remove(&id) {
                    self.lane.done(on, req, now, Outcome::Timeout, 0, true);
                }
            }
        } else {
            // An HTTP/1.1 connection answers in order: one whose oldest request timed out is closed, and all it had
            // in flight timed out with it.
            let stuck: Vec<usize> = self.conns.iter().enumerate().filter(|(_, x)| x.in_flight.front().is_some_and(late)).map(|(c, _)| c).collect();
            for c in stuck {
                self.kill(c, now, Outcome::Timeout);
            }
        }
    }

    /// When a closed connection is next to open again, while there's something left to send.
    fn next_retry(&self, now: Ns) -> Option<Ns> {
        if self.closed == 0 {
            return None;
        }
        self.lane.next_due(now)?;
        self.conns.iter().filter(|x| !x.open).filter_map(|x| x.retry_at).min()
    }

    /// The run, from the clock's start to the drain's end.
    fn run(&mut self) {
        let (warm, end) = (self.lane.warm, self.lane.end);
        let timeout = ns(self.plan.timeout);
        let last = end.saturating_add(timeout).max(self.lane.backlog_until);
        let check = (timeout / 10).max(1);
        let mut next_check = check;
        let mut events = Events::with_capacity(NonZeroUsize::new(EVENTS).unwrap_or(NonZeroUsize::MIN));
        let mut margin = Margin::new();
        let (mut from, mut to): (Option<(Option<u64>, Ns)>, Option<(Option<u64>, Ns)>) = (None, None);
        let mut spun: Ns = 0;
        loop {
            let now = self.now();
            if from.is_none() && now >= warm {
                from = Some((os::thread_cpu_ns(), now));
            }
            if to.is_none() && now >= end {
                to = Some((os::thread_cpu_ns(), now));
            }
            while let Some((c, req)) = self.lane.take(now) {
                self.send(c, req);
            }
            if now >= next_check {
                self.expire(now, false);
                next_check = now.saturating_add(check);
            }
            if self.next_retry(now).is_some_and(|t| t <= now) {
                self.reopen(now);
            }
            if now >= end && self.lane.flying() == 0 && !self.lane.backlog(now) {
                break;
            }
            if now >= last {
                self.expire(now, true);
                break;
            }
            let mut wake = next_check.min(last);
            if now < warm {
                wake = wake.min(warm);
            } else if now < end {
                wake = wake.min(end);
            }
            if self.lane.has_room()
                && let Some(d) = self.lane.next_due(now)
            {
                wake = wake.min(d);
            }
            if let Some(r) = self.next_retry(now) {
                wake = wake.min(r);
            }
            // Asleep until the margin before the wake, then spinning (a wait that returns at once) the rest; an
            // oversleep is measured from the wait itself, not the loop's work before it.
            let asleep = self.now();
            let sleep = wake.saturating_sub(asleep);
            let spin = sleep <= margin.ns;
            let nap = if spin { 0 } else { sleep - margin.ns };
            let got = match self.sockets.wait(&mut events, Duration::from_nanos(nap)) {
                Ok(n) => n,
                Err(e) => {
                    self.out.note(format!("wait: {e}"));
                    0
                }
            };
            if got == 0 {
                let woke = self.now();
                if spin {
                    if now >= warm && now < end {
                        spun += woke.saturating_sub(now);
                    }
                } else {
                    margin.saw(woke.saturating_sub(asleep.saturating_add(nap)));
                }
            }
            for ev in events.iter() {
                if ev.writable {
                    self.flush(ev.key);
                }
                if ev.readable {
                    self.read(ev.key);
                }
                if self.sockets.rearm(ev.key).is_err() {
                    self.out.note("rearm failed".into());
                }
            }
        }
        // The window's CPU, less its spinning before sends: the thread's work.
        if let (Some((Some(a), t_a)), Some((Some(b), t_b))) = (from, to)
            && t_b > t_a
        {
            #[allow(clippy::cast_precision_loss)]
            let (work, spin, span) = (b.saturating_sub(a).saturating_sub(spun) as f64, spun as f64, (t_b - t_a) as f64);
            self.out.busy.push(Some(work / span));
            self.out.spin.push(spin / span);
        } else {
            self.out.busy.push(None);
            self.out.spin.push(0.0);
        }
    }
}

/// One generator thread: opens its connections, waits for every thread's to be open, runs from the shared start.
#[allow(clippy::too_many_arguments)]
fn lane_main(addr: SocketAddr, job: &Job<'_>, plan: Plan, index: usize, conns: usize, first_socket: u64, ready: &Barrier, go: &Barrier, start: &OnceLock<Instant>) -> Ran {
    let punctual = os::punctual();
    let made = Gen::new(addr, job, plan, index, conns, first_socket);
    ready.wait();
    go.wait();
    let mut g = match (made, start.get().copied()) {
        (Ok(g), Some(t0)) => Gen { t0, ..g },
        (Err(e), _) => {
            let mut out = Ran::default();
            out.note(e);
            return out;
        }
        (Ok(g), None) => return g.out,
    };
    g.run();
    let mut out = std::mem::take(&mut g.out);
    out.seen = std::mem::take(&mut g.lane.seen);
    out.unsent = g.lane.unsent();
    out.punctual = punctual.to_owned();
    out
}

/// Runs `job` at `plan` against `addr`, calling `during` on this thread with the window's start and end once the
/// clock has started (the server's sampling): what the threads saw, and what `during` gave.
pub fn run<T>(addr: SocketAddr, job: &Job<'_>, plan: &Plan, during: impl FnOnce(Instant, Instant) -> T) -> (Ran, T) {
    let lanes = plan.lanes.max(1);
    let per: Vec<usize> = (0..lanes).map(|i| plan.conns / lanes + usize::from(i < plan.conns % lanes)).collect();
    let firsts: Vec<u64> = per
        .iter()
        .scan(0u64, |at, n| {
            let first = *at;
            *at += *n as u64;
            Some(first)
        })
        .collect();
    let (ready, go, start) = (Barrier::new(lanes + 1), Barrier::new(lanes + 1), OnceLock::new());
    let plan = *plan;
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..lanes)
            .map(|i| {
                let (ready, go, start) = (&ready, &go, &start);
                let (n, first) = (per.get(i).copied().unwrap_or(0), firsts.get(i).copied().unwrap_or(0));
                scope.spawn(move || lane_main(addr, job, plan, i, n, first, ready, go, start))
            })
            .collect();
        ready.wait();
        let t0 = Instant::now();
        let _ = start.set(t0);
        go.wait();
        let from = t0 + plan.warm;
        let t = during(from, from + plan.window);
        let mut all = Ran::default();
        for h in handles {
            let one = h.join().unwrap_or_else(|_| {
                let mut r = Ran::default();
                r.note("a generator thread panicked".into());
                r
            });
            all.seen.merge(&one.seen);
            all.unsent += one.unsent;
            all.opened += one.opened;
            all.refused += one.refused;
            all.reconnects += one.reconnects;
            all.unmatched += one.unmatched;
            all.busy.extend(one.busy);
            all.spin.extend(one.spin);
            if all.punctual.is_empty() {
                all.punctual = one.punctual;
            }
            for e in one.errors {
                all.note(e);
            }
        }
        (all, t)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rate::target::{Http, Value};

    /// Goal 10 (Ada's fixed-rate benchmark), end to end on loopback: at 1,000 requests a second for half a second,
    /// over two threads and four keep-alive connections, against a server that answers each request after 1 ms
    /// but, from its 300th request (300 ms into the clock, the window's 200th ms), holds every answer for 150 ms:
    /// every request of the window is answered, and the stall is charged to each request it held back, from when it
    /// fell due (no coordinated omission), where timed from each send most look like the server's millisecond.
    #[test]
    fn rate_the_generator_runs_open_loop_against_a_real_socket() -> Result<(), String> {
        use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
        let e = |e: io::Error| e.to_string();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").map_err(e)?;
        let addr = listener.local_addr().map_err(e)?;
        let (stop, seen, stall_until) = (AtomicBool::new(false), AtomicU64::new(0), AtomicU64::new(0));
        let started = Instant::now();
        let w = Workload {
            name: "x".into(),
            pick: Pick::First,
            kind: Kind::Http(Http { method: "GET".into(), path: Template::parse("/x/${n}")?, headers: Vec::new(), body: None, ok_status: None, pipeline: 1 }),
        };
        let mut vars = Vars::default();
        vars.set("n", Value::One("7".into()));
        let job = Job::new(&w, &vars, addr.port())?;
        // A drain and a timeout long enough that a machine busy with other work still answers everything.
        let plan = Plan { rate: 1000, lanes: 2, conns: 4, warm: Duration::from_millis(100), window: Duration::from_millis(500), grace: Duration::from_secs(2), timeout: Duration::from_secs(10) };
        let (ran, ()) = std::thread::scope(|scope| {
            // The server: each connection a thread, each request answered 1 ms after it came, but from the 300th
            // on, every answer waits until 150 ms after the 300th came.
            scope.spawn(|| {
                let _ = listener.set_nonblocking(true);
                let mut conns = Vec::new();
                while !stop.load(Relaxed) && conns.len() < 4 {
                    match listener.accept() {
                        // Blocking, whatever it inherited of the listener (macOS's accepted sockets do).
                        Ok((s, _)) => conns.extend(s.set_nonblocking(false).map(|()| s)),
                        Err(_) => std::thread::sleep(Duration::from_millis(1)),
                    }
                }
                std::thread::scope(|inner| {
                    for s in conns {
                        let (stop, seen, stall_until) = (&stop, &seen, &stall_until);
                        inner.spawn(move || {
                            let _ = s.set_read_timeout(Some(Duration::from_millis(20)));
                            let mut r: &TcpStream = &s;
                            let mut buf = Vec::new();
                            let mut chunk = [0u8; 4096];
                            while !stop.load(Relaxed) {
                                if let Some(i) = find(&buf, b"\r\n\r\n") {
                                    buf.drain(..i + 4);
                                    if seen.fetch_add(1, Relaxed) == 300 {
                                        stall_until.store(ns(started.elapsed()) + 150_000_000, Relaxed);
                                    }
                                    std::thread::sleep(Duration::from_millis(1));
                                    let (at, until) = (ns(started.elapsed()), stall_until.load(Relaxed));
                                    if at < until {
                                        std::thread::sleep(Duration::from_nanos(until - at));
                                    }
                                    if r.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok").is_err() {
                                        return;
                                    }
                                    continue;
                                }
                                match r.read(&mut chunk) {
                                    Ok(0) => return,
                                    Ok(n) => buf.extend_from_slice(chunk.get(..n).unwrap_or_default()),
                                    Err(_) => {}
                                }
                            }
                        });
                    }
                });
            });
            let out = run(addr, &job, &plan, |_, _| ());
            stop.store(true, Relaxed);
            out
        });
        let s = &ran.seen;
        assert_eq!((ran.opened, ran.refused, ran.unsent), (4, 0, 0), "{:?}", ran.errors);
        assert_eq!((s.ok, s.bad, s.timeouts, s.closed), (500, 0, 0, 0), "the window's 500 requests answered: {:?}", ran.errors);
        assert_eq!(s.bytes, 1000);
        // About 150 requests fell due in the stall, and those it held back waited from its length down, with the
        // backlog's drain after: about a third of the window's requests waited 40 ms or more.
        let held = s.latency.quantile(0.75).unwrap_or(0);
        assert!(held >= 20_000_000, "a quarter of the window's requests waited 20 ms or more: p75 {held} ns");
        assert!(s.latency.max().unwrap_or(0) >= 140_000_000, "the first held waited the stall: {:?}", s.latency.max());
        assert!(s.waited >= 100, "{} waited for a connection", s.waited);
        // Timed from each send, the stall all but vanishes: most answers look like the server's millisecond.
        let (service, latency) = (s.service.quantile(0.5).unwrap_or(u64::MAX), s.latency.quantile(0.75).unwrap_or(0));
        assert!(service.saturating_mul(4) <= latency, "timed from each send, p50 {service} ns; from when each fell due, p75 {latency} ns");
        assert_eq!(ran.busy.len(), 2);
        Ok(())
    }
}
