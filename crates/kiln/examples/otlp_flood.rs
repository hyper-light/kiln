//! Floods a kiln (or any OTLP/HTTP receiver) with metrics, as the load test does
//! (`crates/kiln/tests/observe_load.rs`): `senders` threads, each with a kept-alive connection,
//! each request 1,000 series of a gauge at its own time (a millisecond after the last), protobuf,
//! gzip; a 503 is waited out (`Retry-After`) and the same request sent again. Prints what was taken
//! and refused each second, and at the end.
//!
//! `cargo run --release -p kiln --example otlp_flood -- <host:port> <token or -> <senders> <seconds>`
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::cast_precision_loss)]

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use kiln_obs::otlp::pb::Writer;

fn kv(w: &mut Writer, field: u32, k: &str, v: &str) {
    w.message(field, |kv| {
        kv.str(1, k);
        kv.message(2, |av| av.str(1, v));
    });
}

fn request(sender: u64, series: u64, ts_ns: u64) -> Vec<u8> {
    let mut w = Writer::default();
    w.message(1, |rm| {
        rm.message(1, |res| kv(res, 1, "host.name", &format!("flood-{sender}")));
        rm.message(2, |sm| {
            sm.message(2, |m| {
                m.str(1, "flood.v");
                m.message(5, |g| {
                    for j in 0..series {
                        g.message(1, |dp| {
                            kv(dp, 7, "s", &j.to_string());
                            dp.fixed64(3, ts_ns);
                            dp.double(4, 1.0);
                        });
                    }
                });
            });
        });
    });
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    e.write_all(&w.buf).unwrap();
    e.finish().unwrap()
}

/// One POST on a kept-alive connection: its status and its `Retry-After`; `None` when the
/// connection failed (it is opened again next time).
fn post(conn: &mut Option<BufReader<TcpStream>>, addr: &str, headers: &str, body: &[u8]) -> Option<(u16, Option<u64>)> {
    if conn.is_none() {
        let s = TcpStream::connect(addr).ok()?;
        s.set_nodelay(true).ok()?;
        s.set_read_timeout(Some(Duration::from_secs(60))).ok()?;
        *conn = Some(BufReader::new(s));
    }
    let s = conn.as_mut()?;
    let head = format!("POST /v1/metrics HTTP/1.1\r\nHost: {addr}\r\n{headers}Content-Length: {}\r\n\r\n", body.len());
    if s.get_mut().write_all(head.as_bytes()).and_then(|()| s.get_mut().write_all(body)).is_err() {
        *conn = None;
        return None;
    }
    let mut line = String::new();
    if s.read_line(&mut line).ok()? == 0 {
        *conn = None;
        return None;
    }
    let code: u16 = line.split_whitespace().nth(1)?.parse().ok()?;
    let (mut len, mut close, mut retry) = (0usize, false, None);
    loop {
        line.clear();
        s.read_line(&mut line).ok()?;
        let l = line.trim_end();
        if l.is_empty() {
            break;
        }
        let (k, v) = l.split_once(':').unwrap_or((l, ""));
        match k.to_ascii_lowercase().as_str() {
            "content-length" => len = v.trim().parse().unwrap_or(0),
            "connection" => close = v.trim().eq_ignore_ascii_case("close"),
            "retry-after" => retry = v.trim().parse().ok(),
            _ => {}
        }
    }
    let mut rest = vec![0; len];
    s.read_exact(&mut rest).ok()?;
    if close {
        *conn = None;
    }
    Some((code, retry))
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let addr = args.get(1).cloned().unwrap_or_else(|| "127.0.0.1:4318".into());
    let token = args.get(2).filter(|t| *t != "-").cloned();
    let senders: u64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(4);
    let seconds: u64 = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(30);
    let series = 1000u64;
    let t0_ms = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64 - 7_200_000;
    let (taken, refused, failed) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
    let stop = Arc::new(AtomicBool::new(false));
    let threads: Vec<_> = (0..senders)
        .map(|sender| {
            let (addr, token, taken, refused, failed, stop) = (addr.clone(), token.clone(), taken.clone(), refused.clone(), failed.clone(), stop.clone());
            std::thread::spawn(move || {
                let headers = format!("{}Content-Type: application/x-protobuf\r\nContent-Encoding: gzip\r\n", token.map(|t| format!("Authorization: Bearer {t}\r\n")).unwrap_or_default());
                let mut conn = None;
                let mut i = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    let body = request(sender, series, (t0_ms + i) * 1_000_000);
                    match post(&mut conn, &addr, &headers, &body) {
                        Some((200, _)) => {
                            taken.fetch_add(series, Ordering::Relaxed);
                            i += 1;
                        }
                        Some((503, retry)) => {
                            refused.fetch_add(series, Ordering::Relaxed);
                            std::thread::sleep(Duration::from_secs(retry.unwrap_or(1).min(5)));
                        }
                        _ => {
                            failed.fetch_add(1, Ordering::Relaxed);
                            std::thread::sleep(Duration::from_millis(500));
                        }
                    }
                }
            })
        })
        .collect();
    let start = Instant::now();
    let mut last = 0u64;
    while start.elapsed() < Duration::from_secs(seconds) {
        std::thread::sleep(Duration::from_secs(1));
        let t = taken.load(Ordering::Relaxed);
        println!("{:>4} s  taken {:>12} (+{:>9})  refused {:>10}  failed requests {}", start.elapsed().as_secs(), t, t - last, refused.load(Ordering::Relaxed), failed.load(Ordering::Relaxed));
        last = t;
    }
    stop.store(true, Ordering::Relaxed);
    for t in threads {
        let _ = t.join();
    }
    println!("taken {} points, refused {}, failed requests {}", taken.load(Ordering::Relaxed), refused.load(Ordering::Relaxed), failed.load(Ordering::Relaxed));
}
