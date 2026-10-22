//! The load generator every server here is measured with, so all of them meet the same client:
//!
//! * `--mode h1`: HTTP/1.1 keep-alive, `--depth` requests pipelined per connection (1: one at a
//!   time), written in one write and awaited together, as wrk's pipelining does.
//! * `--mode churn`: HTTP/1.1, a new connection for every request (`Connection: close`).
//! * `--mode h2`: HTTP/2 with prior knowledge, `--streams` requests open at once per connection.
//! * `--mode idle`: `--conns` connections opened, each makes one request and then stays open, to
//!   weigh what a server holds per connection (`--server-pid`: its resident memory before and after).
//!
//! Every thread has its own epoll and its own connections, pinned to the next CPU of `--cpus` when
//! given. Latency goes into a log-linear histogram (32 steps per power of two: within about 3%).

use std::net::{Ipv4Addr, SocketAddrV4};
use std::time::{Duration, Instant};

fn main() {
    let args = match Args::parse() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("load: {e}\n\n{USAGE}");
            std::process::exit(2);
        }
    };
    let out = match args.mode {
        Mode::Idle => idle(&args),
        _ => run(&args),
    };
    match out {
        Ok(line) => println!("{line}"),
        Err(e) => {
            eprintln!("load: {e}");
            std::process::exit(1);
        }
    }
}

const USAGE: &str =
    "load --addr 127.0.0.1:8080 [--mode h1|churn|h2|idle] [--conns 256] [--threads 4] [--depth 1] [--streams 100] [--secs 10] [--warmup 2] [--path /] [--cpus 16-19] [--server-pid PID]";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    H1,
    Churn,
    H2,
    Idle,
}

#[derive(Clone, Debug)]
struct Args {
    addr: SocketAddrV4,
    mode: Mode,
    conns: usize,
    threads: usize,
    depth: usize,
    streams: usize,
    secs: f64,
    warmup: f64,
    path: String,
    cpus: Vec<usize>,
    server_pid: Option<u32>,
}

impl Args {
    fn parse() -> Result<Args, String> {
        let mut a = Args {
            addr: SocketAddrV4::new(Ipv4Addr::LOCALHOST, 8080),
            mode: Mode::H1,
            conns: 256,
            threads: 4,
            depth: 1,
            streams: 100,
            secs: 10.0,
            warmup: 2.0,
            path: "/".into(),
            cpus: Vec::new(),
            server_pid: None,
        };
        let mut it = std::env::args().skip(1);
        while let Some(k) = it.next() {
            let mut v = || it.next().ok_or_else(|| format!("{k} wants a value"));
            match k.as_str() {
                "--addr" => a.addr = v()?.parse().map_err(|e| format!("--addr: {e}"))?,
                "--mode" => {
                    a.mode = match v()?.as_str() {
                        "h1" => Mode::H1,
                        "churn" => Mode::Churn,
                        "h2" => Mode::H2,
                        "idle" => Mode::Idle,
                        m => return Err(format!("unknown mode {m}")),
                    }
                }
                "--conns" => a.conns = num(&v()?)?,
                "--threads" => a.threads = num(&v()?)?,
                "--depth" => a.depth = num(&v()?)?,
                "--streams" => a.streams = num(&v()?)?,
                "--secs" => a.secs = v()?.parse().map_err(|e| format!("--secs: {e}"))?,
                "--warmup" => a.warmup = v()?.parse().map_err(|e| format!("--warmup: {e}"))?,
                "--path" => a.path = v()?,
                "--cpus" => a.cpus = cpus(&v()?)?,
                "--server-pid" => a.server_pid = Some(num(&v()?)? as u32),
                "-h" | "--help" => return Err("usage".into()),
                other => return Err(format!("unknown flag {other}")),
            }
        }
        if a.threads == 0 || a.conns == 0 || a.depth == 0 || a.streams == 0 {
            return Err("--threads, --conns, --depth and --streams are at least 1".into());
        }
        a.threads = a.threads.min(a.conns);
        Ok(a)
    }
}

fn num(s: &str) -> Result<usize, String> {
    s.parse().map_err(|e| format!("{s}: {e}"))
}

/// `16-19`, `3,5,7` or a mix.
fn cpus(s: &str) -> Result<Vec<usize>, String> {
    let mut out = Vec::new();
    for part in s.split(',').filter(|p| !p.is_empty()) {
        match part.split_once('-') {
            Some((lo, hi)) => out.extend(num(lo)?..=num(hi)?),
            None => out.push(num(part)?),
        }
    }
    Ok(out)
}

// ----------------------------------------------------------------------------------- histogram

const SUB: u32 = 5;
const BUCKETS: usize = ((64 - SUB as usize) << SUB) + (1 << SUB);

/// Latency in nanoseconds, log-linear: exact below 32 ns, then 32 steps per power of two.
#[derive(Clone)]
struct Hist {
    counts: Vec<u64>,
    max: u64,
}

impl Hist {
    fn new() -> Hist {
        Hist { counts: vec![0; BUCKETS], max: 0 }
    }

    fn index(v: u64) -> usize {
        if v < (1 << SUB) {
            return v as usize;
        }
        let msb = 63 - v.leading_zeros();
        let shift = msb - SUB;
        let sub = (v >> shift) & ((1 << SUB) - 1);
        (((shift + 1) << SUB) as usize) + sub as usize
    }

    fn lower(i: usize) -> u64 {
        if i < (1 << SUB) {
            return i as u64;
        }
        let shift = (i >> SUB) as u32 - 1;
        let sub = (i & ((1 << SUB) - 1)) as u64;
        ((1 << SUB) + sub) << shift
    }

    fn record(&mut self, v: u64, n: u64) {
        if let Some(c) = self.counts.get_mut(Self::index(v)) {
            *c += n;
        }
        self.max = self.max.max(v);
    }

    fn merge(&mut self, o: &Hist) {
        for (a, b) in self.counts.iter_mut().zip(&o.counts) {
            *a += b;
        }
        self.max = self.max.max(o.max);
    }

    fn total(&self) -> u64 {
        self.counts.iter().sum()
    }

    /// The value at quantile `q` (the bucket's lower bound).
    fn quantile(&self, q: f64) -> u64 {
        let total = self.total();
        if total == 0 {
            return 0;
        }
        let want = ((total as f64) * q).ceil().max(1.0) as u64;
        let mut seen = 0;
        for (i, c) in self.counts.iter().enumerate() {
            seen += c;
            if seen >= want {
                return Self::lower(i);
            }
        }
        self.max
    }
}

// ----------------------------------------------------------------------------------- the OS

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

fn os_err(what: &str) -> String {
    format!("{what}: {}", std::io::Error::last_os_error())
}

fn pin(cpu: usize) {
    // SAFETY: a zeroed cpu_set_t is a valid empty set; the call affects only this thread.
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_SET(cpu, &mut set);
        libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set);
    }
}

fn sockaddr(a: SocketAddrV4) -> libc::sockaddr_in {
    libc::sockaddr_in { sin_family: libc::AF_INET as libc::sa_family_t, sin_port: a.port().to_be(), sin_addr: libc::in_addr { s_addr: u32::from_ne_bytes(a.ip().octets()) }, sin_zero: [0; 8] }
}

/// A TCP socket to `to`, from `from` when given; non-blocking when `nonblocking` (the connect is then
/// in progress: wait for writability), `TCP_NODELAY` either way.
fn connect(to: SocketAddrV4, from: Option<Ipv4Addr>, nonblocking: bool) -> Result<i32, String> {
    // SAFETY: plain socket calls on a descriptor this function owns until it returns it.
    unsafe {
        let ty = libc::SOCK_STREAM | libc::SOCK_CLOEXEC | if nonblocking { libc::SOCK_NONBLOCK } else { 0 };
        let fd = libc::socket(libc::AF_INET, ty, 0);
        if fd < 0 {
            return Err(os_err("socket"));
        }
        let one: libc::c_int = 1;
        libc::setsockopt(fd, libc::IPPROTO_TCP, libc::TCP_NODELAY, (&one as *const libc::c_int).cast(), 4);
        if let Some(ip) = from {
            libc::setsockopt(fd, libc::SOL_IP, libc::IP_BIND_ADDRESS_NO_PORT, (&one as *const libc::c_int).cast(), 4);
            let sa = sockaddr(SocketAddrV4::new(ip, 0));
            if libc::bind(fd, (&sa as *const libc::sockaddr_in).cast(), std::mem::size_of::<libc::sockaddr_in>() as u32) != 0 {
                let e = os_err("bind");
                libc::close(fd);
                return Err(e);
            }
        }
        let sa = sockaddr(to);
        let r = libc::connect(fd, (&sa as *const libc::sockaddr_in).cast(), std::mem::size_of::<libc::sockaddr_in>() as u32);
        if r != 0 && !(nonblocking && errno() == libc::EINPROGRESS) {
            let e = os_err("connect");
            libc::close(fd);
            return Err(e);
        }
        if !nonblocking {
            let flags = libc::fcntl(fd, libc::F_GETFL);
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
        Ok(fd)
    }
}

fn close(fd: i32) {
    // SAFETY: the caller owns `fd` and drops it here.
    unsafe {
        libc::close(fd);
    }
}

/// Read into `buf`: `Ok(Some(n))` (0 at end of stream), `Ok(None)` when nothing is there now.
fn read(fd: i32, buf: &mut [u8]) -> Result<Option<usize>, String> {
    // SAFETY: `buf` is valid for writes of its length.
    let r = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
    if r >= 0 {
        return Ok(Some(r as usize));
    }
    match errno() {
        libc::EAGAIN | libc::EINTR => Ok(None),
        _ => Err(os_err("read")),
    }
}

/// Write from `buf`: the bytes taken (0 when the send buffer is full).
fn write(fd: i32, buf: &[u8]) -> Result<usize, String> {
    // SAFETY: `buf` is valid for reads of its length.
    let r = unsafe { libc::send(fd, buf.as_ptr().cast(), buf.len(), libc::MSG_NOSIGNAL) };
    if r >= 0 {
        return Ok(r as usize);
    }
    match errno() {
        libc::EAGAIN | libc::EINTR => Ok(0),
        _ => Err(os_err("write")),
    }
}

struct Epoll(i32);

impl Epoll {
    fn new() -> Result<Epoll, String> {
        // SAFETY: creates a descriptor this value owns.
        let fd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
        if fd < 0 { Err(os_err("epoll_create1")) } else { Ok(Epoll(fd)) }
    }

    fn ctl(&self, op: i32, fd: i32, events: u32, token: u64) -> Result<(), String> {
        let mut ev = libc::epoll_event { events, u64: token };
        // SAFETY: `ev` lives across the call.
        if unsafe { libc::epoll_ctl(self.0, op, fd, &mut ev) } != 0 { Err(os_err("epoll_ctl")) } else { Ok(()) }
    }

    fn wait(&self, events: &mut [libc::epoll_event], ms: i32) -> usize {
        // SAFETY: `events` is valid for writes of its length.
        let n = unsafe { libc::epoll_wait(self.0, events.as_mut_ptr(), events.len() as i32, ms) };
        n.max(0) as usize
    }
}

impl Drop for Epoll {
    fn drop(&mut self) {
        close(self.0);
    }
}

const IN: u32 = libc::EPOLLIN as u32;
const OUT: u32 = libc::EPOLLOUT as u32;

// ----------------------------------------------------------------------------------- HTTP/1.1

fn request_h1(args: &Args, close: bool) -> Vec<u8> {
    let conn = if close { "Connection: close\r\n" } else { "" };
    format!("GET {} HTTP/1.1\r\nHost: {}\r\n{conn}\r\n", args.path, args.addr).into_bytes()
}

/// The `content-length` of a response head, any case.
fn content_length(head: &[u8]) -> Option<usize> {
    for line in head.split(|b| *b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.len() > 15 && line[..15].eq_ignore_ascii_case(b"content-length:") {
            return std::str::from_utf8(&line[15..]).ok()?.trim().parse().ok();
        }
    }
    None
}

/// A connection's read side: responses parsed out of `buf[start..end]` as they complete.
struct H1Read {
    buf: Vec<u8>,
    start: usize,
    end: usize,
    body_left: usize,
}

impl H1Read {
    fn new() -> H1Read {
        H1Read { buf: vec![0; 64 << 10], start: 0, end: 0, body_left: 0 }
    }

    /// Room to read into, compacting or growing first.
    fn room(&mut self) -> &mut [u8] {
        if self.start == self.end {
            self.start = 0;
            self.end = 0;
        } else if self.end == self.buf.len() {
            if self.start > 0 {
                self.buf.copy_within(self.start..self.end, 0);
                self.end -= self.start;
                self.start = 0;
            } else {
                self.buf.resize(self.buf.len() * 2, 0);
            }
        }
        &mut self.buf[self.end..]
    }

    /// The responses completed by what was read.
    fn parse(&mut self) -> Result<u64, String> {
        let mut done = 0;
        loop {
            if self.body_left > 0 {
                let take = (self.end - self.start).min(self.body_left);
                self.start += take;
                self.body_left -= take;
                if self.body_left > 0 {
                    break;
                }
                done += 1;
                continue;
            }
            let window = &self.buf[self.start..self.end];
            let Some(i) = memchr::memmem::find(window, b"\r\n\r\n") else { break };
            let head = &window[..i];
            if !head.starts_with(b"HTTP/1.1 200") {
                return Err(format!("unexpected response: {:?}", String::from_utf8_lossy(&head[..head.len().min(120)])));
            }
            let len = content_length(head).ok_or_else(|| format!("no content-length: {:?}", String::from_utf8_lossy(head)))?;
            self.start += i + 4;
            if len == 0 {
                done += 1;
            } else {
                self.body_left = len;
            }
        }
        Ok(done)
    }
}

#[derive(Default)]
struct Totals {
    requests: u64,
    errors: u64,
    connects: u64,
}

struct H1Conn {
    fd: i32,
    read: H1Read,
    in_flight: u64,
    sent: Instant,
    /// Bytes of the batch still to write after a short write.
    unwritten: usize,
}

/// Writes the rest of `batch` (its last `c.unwritten` bytes); arms writability while some remain.
fn push(ep: &Epoll, c: &mut H1Conn, i: usize, batch: &[u8]) -> Result<(), String> {
    while c.unwritten > 0 {
        let n = write(c.fd, &batch[batch.len() - c.unwritten..])?;
        if n == 0 {
            return ep.ctl(libc::EPOLL_CTL_MOD, c.fd, IN | OUT, i as u64);
        }
        c.unwritten -= n;
    }
    Ok(())
}

fn h1_thread(args: &Args, conns: usize, warm: Instant, end: Instant) -> Result<(Totals, Hist), String> {
    let ep = Epoll::new()?;
    let one = request_h1(args, false);
    let batch: Vec<u8> = one.repeat(args.depth);
    let depth = args.depth as u64;
    let mut cs = Vec::with_capacity(conns);
    for i in 0..conns {
        let fd = connect(args.addr, None, false)?;
        ep.ctl(libc::EPOLL_CTL_ADD, fd, IN, i as u64)?;
        cs.push(H1Conn { fd, read: H1Read::new(), in_flight: depth, sent: Instant::now(), unwritten: batch.len() });
    }
    for (i, c) in cs.iter_mut().enumerate() {
        c.sent = Instant::now();
        push(&ep, c, i, &batch)?;
    }
    let (mut t, mut h) = (Totals::default(), Hist::new());
    let mut events = vec![libc::epoll_event { events: 0, u64: 0 }; 512];
    loop {
        let n = ep.wait(&mut events, 50);
        let now = Instant::now();
        if now >= end {
            break;
        }
        let measuring = now >= warm;
        for ev in &events[..n] {
            let i = ev.u64 as usize;
            let Some(c) = cs.get_mut(i) else { continue };
            if ev.events & OUT != 0 && c.unwritten > 0 {
                push(&ep, c, i, &batch)?;
                if c.unwritten == 0 {
                    ep.ctl(libc::EPOLL_CTL_MOD, c.fd, IN, i as u64)?;
                }
            }
            if ev.events & !OUT == 0 {
                continue;
            }
            match read(c.fd, c.read.room())? {
                None => continue,
                Some(0) => return Err("the server closed a keep-alive connection".into()),
                Some(got) => c.read.end += got,
            }
            let done = c.read.parse()?;
            if done == 0 {
                continue;
            }
            if measuring {
                t.requests += done;
                h.record((now - c.sent).as_nanos() as u64, done);
            }
            c.in_flight = c.in_flight.saturating_sub(done);
            if c.in_flight == 0 {
                c.in_flight = depth;
                c.sent = now;
                c.unwritten = batch.len();
                push(&ep, c, i, &batch)?;
            }
        }
    }
    for c in &cs {
        close(c.fd);
    }
    Ok((t, h))
}

// ----------------------------------------------------------------------------------- churn

struct ChurnConn {
    fd: i32,
    read: H1Read,
    started: Instant,
    connected: bool,
    unwritten: usize,
}

fn churn_open(ep: &Epoll, args: &Args, i: usize) -> Result<ChurnConn, String> {
    let fd = connect(args.addr, None, true)?;
    ep.ctl(libc::EPOLL_CTL_ADD, fd, OUT, i as u64)?;
    Ok(ChurnConn { fd, read: H1Read::new(), started: Instant::now(), connected: false, unwritten: 0 })
}

fn churn_thread(args: &Args, conns: usize, warm: Instant, end: Instant) -> Result<(Totals, Hist), String> {
    let ep = Epoll::new()?;
    let req = request_h1(args, true);
    let mut cs = Vec::with_capacity(conns);
    for i in 0..conns {
        cs.push(churn_open(&ep, args, i)?);
    }
    let (mut t, mut h) = (Totals::default(), Hist::new());
    let mut events = vec![libc::epoll_event { events: 0, u64: 0 }; 512];
    loop {
        let n = ep.wait(&mut events, 50);
        let now = Instant::now();
        if now >= end {
            break;
        }
        let measuring = now >= warm;
        for ev in &events[..n] {
            let i = ev.u64 as usize;
            let Some(c) = cs.get_mut(i) else { continue };
            if !c.connected {
                let mut err: libc::c_int = 0;
                let mut len = 4u32;
                // SAFETY: `err` and `len` outlive the call.
                unsafe { libc::getsockopt(c.fd, libc::SOL_SOCKET, libc::SO_ERROR, (&mut err as *mut libc::c_int).cast(), &mut len) };
                if err != 0 {
                    t.errors += 1;
                    close(c.fd);
                    *c = churn_open(&ep, args, i)?;
                    continue;
                }
                c.connected = true;
                c.unwritten = req.len();
            }
            if c.unwritten > 0 {
                let n = write(c.fd, &req[req.len() - c.unwritten..])?;
                c.unwritten -= n;
                if c.unwritten == 0 {
                    ep.ctl(libc::EPOLL_CTL_MOD, c.fd, IN, i as u64)?;
                }
                continue;
            }
            let got = match read(c.fd, c.read.room()) {
                Ok(None) => continue,
                Ok(Some(got)) => got,
                Err(_) => 0,
            };
            c.read.end += got;
            let done = c.read.parse()?;
            if done > 0 || got == 0 {
                if done > 0 && measuring {
                    t.requests += 1;
                    t.connects += 1;
                    h.record((now - c.started).as_nanos() as u64, 1);
                }
                if done == 0 {
                    t.errors += 1;
                }
                close(c.fd);
                *c = churn_open(&ep, args, i)?;
            }
        }
    }
    for c in &cs {
        close(c.fd);
    }
    Ok((t, h))
}

// ----------------------------------------------------------------------------------- HTTP/2

const PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
const DATA: u8 = 0;
const HEADERS: u8 = 1;
const RST_STREAM: u8 = 3;
const SETTINGS: u8 = 4;
const PING: u8 = 6;
const GOAWAY: u8 = 7;
const CONTINUATION: u8 = 9;
const END_STREAM: u8 = 1;
const ACK: u8 = 1;
const END_HEADERS: u8 = 4;

fn frame(out: &mut Vec<u8>, ty: u8, flags: u8, stream: u32, payload: &[u8]) {
    let len = payload.len() as u32;
    out.extend_from_slice(&len.to_be_bytes()[1..]);
    out.push(ty);
    out.push(flags);
    out.extend_from_slice(&stream.to_be_bytes());
    out.extend_from_slice(payload);
}

/// The request's header block: HPACK with no dynamic table, so the same bytes serve every stream.
fn h2_block(args: &Args) -> Vec<u8> {
    let mut b = vec![0x82, 0x86]; // :method GET, :scheme http
    if args.path == "/" {
        b.push(0x84);
    } else {
        b.push(0x04); // :path, a literal never indexed
        b.push(args.path.len() as u8);
        b.extend_from_slice(args.path.as_bytes());
    }
    let authority = args.addr.to_string();
    b.push(0x01); // :authority, a literal never indexed
    b.push(authority.len() as u8);
    b.extend_from_slice(authority.as_bytes());
    b
}

/// What a connection says first: the preface, settings (no push; stream windows at their largest),
/// and the connection's window opened all the way.
fn h2_opening() -> Vec<u8> {
    let mut out = PREFACE.to_vec();
    let mut s = Vec::new();
    s.extend_from_slice(&2u16.to_be_bytes());
    s.extend_from_slice(&0u32.to_be_bytes());
    s.extend_from_slice(&4u16.to_be_bytes());
    s.extend_from_slice(&0x7fff_ffffu32.to_be_bytes());
    frame(&mut out, SETTINGS, 0, 0, &s);
    frame(&mut out, 8, 0, 0, &(0x7fff_ffffu32 - 65_535).to_be_bytes());
    out
}

struct H2Conn {
    fd: i32,
    rbuf: Vec<u8>,
    start: usize,
    end: usize,
    wbuf: Vec<u8>,
    wpos: usize,
    next_id: u32,
    in_flight: usize,
    limit: usize,
    sent: Vec<(u32, Instant)>,
    taken: u64,
    /// A stream whose header block ended its stream and goes on in CONTINUATION frames.
    ending: Option<u32>,
}

const RING: usize = 4096;

impl H2Conn {
    fn new(fd: i32, limit: usize) -> H2Conn {
        H2Conn { fd, rbuf: vec![0; 256 << 10], start: 0, end: 0, wbuf: h2_opening(), wpos: 0, next_id: 1, in_flight: 0, limit, sent: vec![(0, Instant::now()); RING], taken: 0, ending: None }
    }

    fn open_streams(&mut self, block: &[u8], now: Instant) {
        while self.in_flight < self.limit {
            let id = self.next_id;
            self.next_id += 2;
            frame(&mut self.wbuf, HEADERS, END_STREAM | END_HEADERS, id, block);
            if let Some(slot) = self.sent.get_mut((id as usize >> 1) % RING) {
                *slot = (id, now);
            }
            self.in_flight += 1;
        }
    }

    fn flush(&mut self) -> Result<bool, String> {
        while self.wpos < self.wbuf.len() {
            let n = write(self.fd, &self.wbuf[self.wpos..])?;
            if n == 0 {
                return Ok(false);
            }
            self.wpos += n;
        }
        self.wbuf.clear();
        self.wpos = 0;
        Ok(true)
    }

    fn done(&mut self, id: u32, now: Instant, measuring: bool, t: &mut Totals, h: &mut Hist) {
        self.in_flight = self.in_flight.saturating_sub(1);
        if !measuring {
            return;
        }
        t.requests += 1;
        if let Some((sid, at)) = self.sent.get((id as usize >> 1) % RING)
            && *sid == id
        {
            h.record((now - *at).as_nanos() as u64, 1);
        }
    }

    /// Takes every whole frame read; false once the server ended the connection.
    fn frames(&mut self, now: Instant, measuring: bool, t: &mut Totals, h: &mut Hist) -> Result<bool, String> {
        while self.end - self.start >= 9 {
            let head = &self.rbuf[self.start..self.start + 9];
            let len = (usize::from(head[0]) << 16) | (usize::from(head[1]) << 8) | usize::from(head[2]);
            let (ty, flags) = (head[3], head[4]);
            let id = u32::from_be_bytes([head[5], head[6], head[7], head[8]]) & 0x7fff_ffff;
            if self.end - self.start < 9 + len {
                if 9 + len > self.rbuf.len() {
                    self.rbuf.resize(9 + len, 0);
                }
                break;
            }
            let payload = self.start + 9..self.start + 9 + len;
            self.start = payload.end;
            match ty {
                DATA => {
                    self.taken += len as u64;
                    if flags & END_STREAM != 0 {
                        self.done(id, now, measuring, t, h);
                    }
                }
                HEADERS if flags & END_STREAM != 0 => {
                    if flags & END_HEADERS != 0 {
                        self.done(id, now, measuring, t, h);
                    } else {
                        self.ending = Some(id);
                    }
                }
                CONTINUATION if flags & END_HEADERS != 0 && self.ending == Some(id) => {
                    self.ending = None;
                    self.done(id, now, measuring, t, h);
                }
                SETTINGS if flags & ACK == 0 => {
                    for s in self.rbuf[payload].chunks_exact(6) {
                        if u16::from_be_bytes([s[0], s[1]]) == 3 {
                            let max = u32::from_be_bytes([s[2], s[3], s[4], s[5]]) as usize;
                            self.limit = self.limit.min(max.max(1));
                        }
                    }
                    frame(&mut self.wbuf, SETTINGS, ACK, 0, &[]);
                }
                PING if flags & ACK == 0 => {
                    let data = self.rbuf[payload].to_vec();
                    frame(&mut self.wbuf, PING, ACK, 0, &data);
                }
                RST_STREAM => {
                    t.errors += 1;
                    self.in_flight = self.in_flight.saturating_sub(1);
                }
                GOAWAY => return Ok(false),
                _ => {}
            }
        }
        if self.start == self.end {
            self.start = 0;
            self.end = 0;
        } else if self.start > 0 && self.end == self.rbuf.len() {
            self.rbuf.copy_within(self.start..self.end, 0);
            self.end -= self.start;
            self.start = 0;
        }
        if self.taken > 1 << 30 {
            frame(&mut self.wbuf, 8, 0, 0, &(self.taken as u32).to_be_bytes());
            self.taken = 0;
        }
        Ok(true)
    }
}

fn h2_thread(args: &Args, conns: usize, warm: Instant, end: Instant) -> Result<(Totals, Hist), String> {
    let ep = Epoll::new()?;
    let block = h2_block(args);
    let mut cs = Vec::with_capacity(conns);
    let now = Instant::now();
    for i in 0..conns {
        let fd = connect(args.addr, None, false)?;
        ep.ctl(libc::EPOLL_CTL_ADD, fd, IN, i as u64)?;
        let mut c = H2Conn::new(fd, args.streams);
        c.open_streams(&block, now);
        if !c.flush()? {
            ep.ctl(libc::EPOLL_CTL_MOD, fd, IN | OUT, i as u64)?;
        }
        cs.push(c);
    }
    let (mut t, mut h) = (Totals::default(), Hist::new());
    let mut events = vec![libc::epoll_event { events: 0, u64: 0 }; 512];
    loop {
        let n = ep.wait(&mut events, 50);
        let now = Instant::now();
        if now >= end {
            break;
        }
        let measuring = now >= warm;
        for ev in &events[..n] {
            let i = ev.u64 as usize;
            let Some(c) = cs.get_mut(i) else { continue };
            if ev.events & !OUT != 0 {
                let (start, end) = (c.end, c.rbuf.len());
                match read(c.fd, &mut c.rbuf[start..end])? {
                    None => {}
                    Some(0) => return Err("the server closed an HTTP/2 connection".into()),
                    Some(got) => {
                        c.end += got;
                        if !c.frames(now, measuring, &mut t, &mut h)? {
                            return Err("the server sent GOAWAY".into());
                        }
                        c.open_streams(&block, now);
                    }
                }
            }
            let flushed = c.flush()?;
            ep.ctl(libc::EPOLL_CTL_MOD, c.fd, if flushed { IN } else { IN | OUT }, i as u64)?;
        }
    }
    for c in &cs {
        close(c.fd);
    }
    Ok((t, h))
}

// ----------------------------------------------------------------------------------- running

fn run(args: &Args) -> Result<String, String> {
    let start = Instant::now();
    let warm = start + Duration::from_secs_f64(args.warmup);
    let end = warm + Duration::from_secs_f64(args.secs);
    let mut handles = Vec::new();
    for th in 0..args.threads {
        let conns = args.conns / args.threads + usize::from(th < args.conns % args.threads);
        let a = args.clone();
        let cpu = args.cpus.get(th % args.cpus.len().max(1)).copied();
        handles.push(std::thread::spawn(move || {
            if let Some(cpu) = cpu {
                pin(cpu);
            }
            match a.mode {
                Mode::H1 => h1_thread(&a, conns, warm, end),
                Mode::Churn => churn_thread(&a, conns, warm, end),
                _ => h2_thread(&a, conns, warm, end),
            }
        }));
    }
    let (mut t, mut h) = (Totals::default(), Hist::new());
    for handle in handles {
        let (tt, hh) = handle.join().map_err(|_| "a load thread panicked".to_string())??;
        t.requests += tt.requests;
        t.errors += tt.errors;
        t.connects += tt.connects;
        h.merge(&hh);
    }
    let us = |ns: u64| ns as f64 / 1000.0;
    let mode = match args.mode {
        Mode::H1 => "h1",
        Mode::Churn => "churn",
        Mode::H2 => "h2",
        Mode::Idle => "idle",
    };
    Ok(format!(
        "{{\"mode\":\"{mode}\",\"conns\":{},\"threads\":{},\"depth\":{},\"streams\":{},\"secs\":{},\"requests\":{},\"rps\":{:.0},\"errors\":{},\"p50_us\":{:.1},\"p90_us\":{:.1},\"p99_us\":{:.1},\"p999_us\":{:.1},\"max_us\":{:.1}}}",
        args.conns,
        args.threads,
        args.depth,
        args.streams,
        args.secs,
        t.requests,
        t.requests as f64 / args.secs,
        t.errors,
        us(h.quantile(0.50)),
        us(h.quantile(0.90)),
        us(h.quantile(0.99)),
        us(h.quantile(0.999)),
        us(h.max),
    ))
}

// ----------------------------------------------------------------------------------- idle

fn rss_kib(pid: u32) -> Result<u64, String> {
    let s = std::fs::read_to_string(format!("/proc/{pid}/status")).map_err(|e| format!("server status: {e}"))?;
    s.lines().find_map(|l| l.strip_prefix("VmRSS:")).and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok()).ok_or_else(|| "no VmRSS".into())
}

/// One request on a fresh connection, blocking until its response; the connection stays open.
fn one_request(fd: i32, req: &[u8]) -> Result<(), String> {
    let mut sent = 0;
    while sent < req.len() {
        sent += write(fd, &req[sent..])?;
    }
    let mut r = H1Read::new();
    loop {
        match read(fd, r.room())? {
            None => std::thread::sleep(Duration::from_micros(50)),
            Some(0) => return Err("closed before answering".into()),
            Some(n) => {
                r.end += n;
                if r.parse()? > 0 {
                    return Ok(());
                }
            }
        }
    }
}

fn idle(args: &Args) -> Result<String, String> {
    let before = match args.server_pid {
        Some(pid) => Some(rss_kib(pid)?),
        None => None,
    };
    let req = request_h1(args, false);
    let mut fds = Vec::with_capacity(args.conns);
    for i in 0..args.conns {
        // 20 000 connections to a source address, so the ephemeral ports never run out.
        let from = Ipv4Addr::new(127, 0, 0, 1 + (i / 20_000) as u8);
        let fd = connect(args.addr, Some(from), false)?;
        one_request(fd, &req)?;
        fds.push(fd);
    }
    std::thread::sleep(Duration::from_secs(1));
    let after = match args.server_pid {
        Some(pid) => Some(rss_kib(pid)?),
        None => None,
    };
    let line = match (before, after) {
        (Some(b), Some(a)) => {
            format!("{{\"mode\":\"idle\",\"conns\":{},\"rss_before_kib\":{b},\"rss_after_kib\":{a},\"bytes_per_conn\":{:.0}}}", args.conns, (a.saturating_sub(b) * 1024) as f64 / args.conns as f64)
        }
        _ => format!("{{\"mode\":\"idle\",\"conns\":{}}}", args.conns),
    };
    std::thread::sleep(Duration::from_secs_f64(args.secs.min(2.0)));
    for fd in fds {
        close(fd);
    }
    Ok(line)
}
