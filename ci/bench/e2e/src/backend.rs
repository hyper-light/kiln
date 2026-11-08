//! N test backends for the end-to-end bench (LB14): each a listener on 127.0.0.1 (a port the system
//! picks) that answers every HTTP/1.1 request on a kept connection at once with `204 No Content`,
//! and counts what it answered, so the spread of the work across backends (quality, beside speed)
//! can be read after a run.
//!
//! Usage: e2e-backend --n N
//! Its first line on stdout: `{"ports":[...],"control":PORT,"pid":PID}`. On the control port,
//! `GET /counts` answers `{"counts":[...]}` (one per backend, in the order of `ports`) and
//! `GET /reset` zeroes them. A thread per connection, blocking: a request is answered as soon as
//! its head is read, with no event loop between.

use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

const ANSWER: &[u8] = b"HTTP/1.1 204 No Content\r\n\r\n";

/// The requests in `buf` that are whole: each one's end, and whether it asked to close.
fn whole(buf: &[u8]) -> Vec<(usize, bool)> {
    let mut out = Vec::new();
    let mut at = 0;
    while let Some(p) = buf.get(at..).and_then(|b| b.windows(4).position(|w| w == b"\r\n\r\n")) {
        let head_end = at + p + 4;
        let head = String::from_utf8_lossy(buf.get(at..head_end).unwrap_or_default()).to_ascii_lowercase();
        let body = head.lines().find_map(|l| l.strip_prefix("content-length:")).and_then(|v| v.trim().parse::<usize>().ok()).unwrap_or(0);
        let end = head_end + body;
        if end > buf.len() {
            break;
        }
        out.push((end, head.contains("\r\nconnection: close")));
        at = end;
    }
    out
}

fn serve(mut s: TcpStream, count: Arc<Vec<AtomicU64>>, i: usize) {
    let _ = s.set_nodelay(true);
    let mut buf: Vec<u8> = Vec::with_capacity(16 * 1024);
    let mut chunk = [0u8; 16 * 1024];
    loop {
        let n = match s.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        buf.extend_from_slice(chunk.get(..n).unwrap_or_default());
        let done = whole(&buf);
        let Some(&(last, _)) = done.last() else { continue };
        let mut out = Vec::with_capacity(ANSWER.len() * done.len());
        for _ in &done {
            out.extend_from_slice(ANSWER);
        }
        if let Some(c) = count.get(i) {
            c.fetch_add(done.len() as u64, Ordering::Relaxed);
        }
        if s.write_all(&out).is_err() || done.iter().any(|&(_, close)| close) {
            return;
        }
        buf.drain(..last);
    }
}

fn control(mut s: TcpStream, count: Arc<Vec<AtomicU64>>) {
    let mut buf = [0u8; 4096];
    let n = s.read(&mut buf).unwrap_or(0);
    let req = String::from_utf8_lossy(buf.get(..n).unwrap_or_default()).into_owned();
    let body = if req.starts_with("GET /reset") {
        for c in count.iter() {
            c.store(0, Ordering::Relaxed);
        }
        "{\"reset\":true}".to_owned()
    } else {
        let v: Vec<String> = count.iter().map(|c| c.load(Ordering::Relaxed).to_string()).collect();
        format!("{{\"counts\":[{}]}}", v.join(","))
    };
    let _ = s.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes());
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let n = match argv.as_slice() {
        [flag, v] if flag == "--n" => match v.parse::<usize>() {
            Ok(n) if n > 0 => n,
            _ => {
                eprintln!("e2e-backend: --n takes a whole number of at least 1");
                return ExitCode::from(2);
            }
        },
        _ => {
            eprintln!("usage: e2e-backend --n N");
            return ExitCode::from(2);
        }
    };
    let count: Arc<Vec<AtomicU64>> = Arc::new((0..n).map(|_| AtomicU64::new(0)).collect());
    let mut ports = Vec::with_capacity(n);
    let mut listeners = Vec::with_capacity(n);
    for _ in 0..n {
        let l = match TcpListener::bind("127.0.0.1:0") {
            Ok(l) => l,
            Err(e) => {
                eprintln!("e2e-backend: bind: {e}");
                return ExitCode::from(1);
            }
        };
        ports.push(l.local_addr().map(|a| a.port()).unwrap_or(0));
        listeners.push(l);
    }
    let ctl = match TcpListener::bind("127.0.0.1:0") {
        Ok(l) => l,
        Err(e) => {
            eprintln!("e2e-backend: bind the control port: {e}");
            return ExitCode::from(1);
        }
    };
    let ctl_port = ctl.local_addr().map(|a| a.port()).unwrap_or(0);
    for (i, l) in listeners.into_iter().enumerate() {
        let count = Arc::clone(&count);
        std::thread::spawn(move || {
            for s in l.incoming().flatten() {
                let count = Arc::clone(&count);
                std::thread::spawn(move || serve(s, count, i));
            }
        });
    }
    let list: Vec<String> = ports.iter().map(u16::to_string).collect();
    println!("{{\"ports\":[{}],\"control\":{ctl_port},\"pid\":{}}}", list.join(","), std::process::id());
    let _ = std::io::stdout().flush();
    for s in ctl.incoming().flatten() {
        control(s, Arc::clone(&count));
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    /// LB14: pipelined requests, one with a body, are each answered; a close is seen.
    #[test]
    fn lb14_whole_requests() {
        let b = b"GET / HTTP/1.1\r\nHost: a\r\n\r\nPOST / HTTP/1.1\r\nContent-Length: 3\r\n\r\nabcGET / HTTP/1.1\r\nConnection: close\r\n\r\nGET /";
        let w = whole(b);
        assert_eq!(w.len(), 3);
        assert!(w.last().is_some_and(|&(_, close)| close));
        assert!(whole(b"GET / HTTP/1.1\r\nContent-Length: 9\r\n\r\nabc").is_empty());
    }
}
