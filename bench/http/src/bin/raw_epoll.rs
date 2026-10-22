//! The kernel's ceiling: no HTTP stack and no runtime, only what answering takes. Each of
//! `--threads` threads has its own `SO_REUSEPORT` listener and a level-triggered epoll; a readable
//! connection is read once into the thread's buffer and every whole request in it (GETs: a head
//! ending `\r\n\r\n`) is answered with the same response the other servers send, all in one write.
//! No server on this kernel answers faster; how near a stack comes to this is its overhead.

use std::time::{Duration, Instant, SystemTime};

const IN: u32 = libc::EPOLLIN as u32;
const OUT: u32 = libc::EPOLLOUT as u32;
const LISTENER: u64 = u64::MAX;

fn os_err(what: &str) -> String {
    format!("{what}: {}", std::io::Error::last_os_error())
}

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// An IMF-fixdate (RFC 9110 §5.6.7) for `secs` since the epoch: 29 bytes.
fn http_date(secs: u64) -> String {
    let days = secs / 86_400;
    let (h, m, s) = ((secs / 3600) % 24, (secs / 60) % 60, secs % 60);
    let weekday = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"][(days % 7) as usize];
    // Civil from days (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(mo <= 2);
    let month = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"][(mo - 1) as usize];
    format!("{weekday}, {d:02} {month} {y} {h:02}:{m:02}:{s:02} GMT")
}

fn response() -> Vec<u8> {
    let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    format!("HTTP/1.1 200 OK\r\ncontent-length: 13\r\ndate: {}\r\n\r\nHello, World!", http_date(now)).into_bytes()
}

struct Conn {
    /// A request's start, read without its end.
    carry: Vec<u8>,
    /// Responses written short: the rest waits for writability.
    pending: Vec<u8>,
}

fn listener(port: u16) -> Result<i32, String> {
    // SAFETY: plain socket calls on a descriptor this function owns until it returns it.
    unsafe {
        let fd = libc::socket(libc::AF_INET, libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC, 0);
        if fd < 0 {
            return Err(os_err("socket"));
        }
        let one: libc::c_int = 1;
        libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_REUSEADDR, (&one as *const libc::c_int).cast(), 4);
        libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_REUSEPORT, (&one as *const libc::c_int).cast(), 4);
        let sa = libc::sockaddr_in { sin_family: libc::AF_INET as libc::sa_family_t, sin_port: port.to_be(), sin_addr: libc::in_addr { s_addr: u32::from_ne_bytes([127, 0, 0, 1]) }, sin_zero: [0; 8] };
        if libc::bind(fd, (&sa as *const libc::sockaddr_in).cast(), std::mem::size_of::<libc::sockaddr_in>() as u32) != 0 {
            return Err(os_err("bind"));
        }
        if libc::listen(fd, 4096) != 0 {
            return Err(os_err("listen"));
        }
        Ok(fd)
    }
}

fn ctl(ep: i32, op: i32, fd: i32, events: u32, token: u64) {
    let mut ev = libc::epoll_event { events, u64: token };
    // SAFETY: `ev` lives across the call.
    unsafe { libc::epoll_ctl(ep, op, fd, &mut ev) };
}

fn close(conns: &mut [Option<Conn>], fd: i32) {
    if let Some(c) = conns.get_mut(fd as usize) {
        *c = None;
    }
    // SAFETY: the connection's descriptor, closed once.
    unsafe { libc::close(fd) };
}

/// Writes `out`, keeping what the kernel didn't take; false when the connection failed.
fn send(ep: i32, fd: i32, conn: &mut Conn, out: &[u8]) -> bool {
    // SAFETY: `out` is valid for reads of its length.
    let n = unsafe { libc::send(fd, out.as_ptr().cast(), out.len(), libc::MSG_NOSIGNAL) };
    if n < 0 && errno() != libc::EAGAIN {
        return false;
    }
    let n = n.max(0) as usize;
    if n < out.len() {
        conn.pending.extend_from_slice(&out[n..]);
        ctl(ep, libc::EPOLL_CTL_MOD, fd, IN | OUT, fd as u64);
    }
    true
}

fn serve(port: u16) -> Result<(), String> {
    let lfd = listener(port)?;
    // SAFETY: creates a descriptor this thread owns.
    let ep = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
    if ep < 0 {
        return Err(os_err("epoll_create1"));
    }
    ctl(ep, libc::EPOLL_CTL_ADD, lfd, IN, LISTENER);
    let mut conns: Vec<Option<Conn>> = Vec::new();
    let mut buf = vec![0u8; 256 << 10];
    let mut out: Vec<u8> = Vec::with_capacity(256 << 10);
    let mut resp = response();
    let mut stamped = Instant::now();
    let mut events = vec![libc::epoll_event { events: 0, u64: 0 }; 1024];
    loop {
        // SAFETY: `events` is valid for writes of its length.
        let n = unsafe { libc::epoll_wait(ep, events.as_mut_ptr(), events.len() as i32, 1000) };
        if stamped.elapsed() >= Duration::from_millis(500) {
            resp = response();
            stamped = Instant::now();
        }
        for ev in events.iter().take(n.max(0) as usize) {
            let (token, flags) = (ev.u64, ev.events);
            if token == LISTENER {
                loop {
                    // SAFETY: a null address is allowed; the new descriptor is owned by `conns`.
                    let fd = unsafe { libc::accept4(lfd, std::ptr::null_mut(), std::ptr::null_mut(), libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC) };
                    if fd < 0 {
                        break;
                    }
                    let one: libc::c_int = 1;
                    // SAFETY: `one` lives across the call.
                    unsafe { libc::setsockopt(fd, libc::IPPROTO_TCP, libc::TCP_NODELAY, (&one as *const libc::c_int).cast(), 4) };
                    if conns.len() <= fd as usize {
                        conns.resize_with(fd as usize + 1, || None);
                    }
                    conns[fd as usize] = Some(Conn { carry: Vec::new(), pending: Vec::new() });
                    ctl(ep, libc::EPOLL_CTL_ADD, fd, IN, fd as u64);
                }
                continue;
            }
            let fd = token as i32;
            let Some(Some(conn)) = conns.get_mut(fd as usize) else { continue };
            if flags & OUT != 0 && !conn.pending.is_empty() {
                let pending = std::mem::take(&mut conn.pending);
                if !send(ep, fd, conn, &pending) {
                    close(&mut conns, fd);
                    continue;
                }
                if conn.pending.is_empty() {
                    ctl(ep, libc::EPOLL_CTL_MOD, fd, IN, fd as u64);
                }
                continue;
            }
            if !conn.pending.is_empty() {
                continue;
            }
            let carried = conn.carry.len();
            buf[..carried].copy_from_slice(&conn.carry);
            conn.carry.clear();
            // SAFETY: the buffer is valid for writes past what was carried.
            let r = unsafe { libc::read(fd, buf[carried..].as_mut_ptr().cast(), buf.len() - carried) };
            if r == 0 || (r < 0 && errno() != libc::EAGAIN) {
                close(&mut conns, fd);
                continue;
            }
            let have = carried + r.max(0) as usize;
            let data = &buf[..have];
            let (mut at, mut answers) = (0, 0usize);
            while let Some(i) = memchr::memmem::find(&data[at..], b"\r\n\r\n") {
                at += i + 4;
                answers += 1;
            }
            conn.carry.extend_from_slice(&data[at..]);
            if answers == 0 {
                continue;
            }
            out.clear();
            for _ in 0..answers {
                out.extend_from_slice(&resp);
            }
            if !send(ep, fd, conn, &out) {
                close(&mut conns, fd);
            }
        }
    }
}

fn main() {
    let (mut port, mut threads) = (8080u16, 4usize);
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        let v = it.next().and_then(|v| v.parse::<usize>().ok());
        match (k.as_str(), v) {
            ("--port", Some(v)) => port = v as u16,
            ("--threads", Some(v)) => threads = v,
            _ => {
                eprintln!("raw-epoll --port 8080 --threads 4");
                std::process::exit(2);
            }
        }
    }
    eprintln!("raw-epoll: 127.0.0.1:{port}, {threads} threads");
    let handles: Vec<_> = (0..threads).map(|_| std::thread::spawn(move || serve(port))).collect();
    for h in handles {
        if let Ok(Err(e)) = h.join() {
            eprintln!("raw-epoll: {e}");
        }
    }
}
