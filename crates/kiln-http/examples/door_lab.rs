//! A lab program (goal 4 step 7, plan 01a1a832eb597110a6dcfda227981604): the HTTP/3 door on a
//! real UDP socket under hyper-rt, for real browsers. It serves the lab's test page and `/info`
//! (the door's URL with a single-use ticket, and the hash of its certificate) over HTTP/1.1 on
//! 127.0.0.1, and the door on 127.0.0.1's UDP: sessions at `/wt/echo` echo their streams and
//! datagrams, and with `drain` in their path, the door sends GOAWAY and WT_DRAIN_SESSION after the
//! first datagram. Never on 7070; never in production.
//!
//!   cargo run -p kiln-http --example door_lab -- <page.html> [http port] [udp port] [--open] [--cert-out <file>]
//!
//! `--open` admits any ticket (the soak's generator, step 10); `--cert-out` writes the
//! certificate's DER for a generator to trust.

#![allow(clippy::print_stdout, clippy::print_stderr, reason = "a lab program says what it does")]

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use http::{Request, Response, StatusCode};
use http_body_util::Full;
use hyper_quic::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use hyper_rt::tcp::TcpListener;
use hyper_rt::{Runtime, RuntimeConfig};
use kiln_http::h3::conn::{Admission, Connect, Event};
use kiln_http::h3::door::{ConnId, Door, DoorConfig};
use kiln_http::h3::driver::{Driver, Host};
use kiln_http::h3::{Admit, Read};
use kiln_http::{Incoming, TcpIo, service_fn};

// Live heap bytes, counted across the process (the soak tells them apart from resident memory,
// which an allocator's fragmentation also moves).
#[global_allocator]
static ALLOCATOR: hyper_measure::alloc::Counting = hyper_measure::alloc::Counting;

/// Days from 1970-01-01 to a civil date (Howard Hinnant's algorithm).
fn civil(days: i64) -> (i32, u8, u8) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u8;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u8;
    (if m <= 2 { y + 1 } else { y } as i32, m, d)
}

/// A self-signed ECDSA P-256 certificate for 127.0.0.1 valid from yesterday for 10 days (what
/// serverCertificateHashes takes), its key, and its SHA-256.
fn cert() -> Result<(Vec<u8>, Vec<u8>, [u8; 32]), String> {
    let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).map_err(|e| e.to_string())?;
    let mut p = rcgen::CertificateParams::new(vec!["localhost".to_owned(), "127.0.0.1".to_owned()]).map_err(|e| e.to_string())?;
    let today = SystemTime::now().duration_since(UNIX_EPOCH).map_err(|e| e.to_string())?.as_secs() as i64 / 86_400;
    let (y, m, d) = civil(today - 1);
    p.not_before = rcgen::date_time_ymd(y, m, d);
    let (y, m, d) = civil(today + 9);
    p.not_after = rcgen::date_time_ymd(y, m, d);
    let c = p.self_signed(&key).map_err(|e| e.to_string())?;
    let der = c.der().to_vec();
    use sha2::Digest as _;
    let h: [u8; 32] = sha2::Sha256::digest(&der).into();
    Ok((der, key.serialize_der(), h))
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

struct Lab {
    tickets: Arc<Mutex<BTreeSet<String>>>,
    drained: BTreeSet<ConnId>,
    /// Admits any ticket at /wt/echo (the soak's load generator has none to fetch).
    open: bool,
}

impl Admit for Lab {
    fn admit(&mut self, c: &Connect) -> Admission {
        let path = String::from_utf8_lossy(&c.path).into_owned();
        let ticket = path.split_once("ticket=").map(|(_, t)| t.split('&').next().unwrap_or_default().to_owned()).unwrap_or_default();
        let ok = self.open || self.tickets.lock().map(|mut t| t.remove(&ticket)).unwrap_or(false);
        if self.open {
            return if path.starts_with("/wt/echo") { Admission::Accept } else { Admission::NotFound };
        }
        println!("door: CONNECT {} origin {:?} {} -> {}", path.split('?').next().unwrap_or_default(), c.origin.as_deref().map(String::from_utf8_lossy), c.draft.name(), if ok { 200 } else { 403 });
        if !path.starts_with("/wt/echo") {
            return Admission::NotFound;
        }
        if ok { Admission::Accept } else { Admission::Forbidden }
    }
}

impl Host for Lab {
    fn event(&mut self, door: &mut Door, conn: ConnId, event: Event, now: Instant) {
        match &event {
            Event::Accepted(c) => println!("door: session {conn} up ({})", c.draft.name()),
            Event::Closed { code, message } => println!("door: session {conn} closed {code} {:?}", String::from_utf8_lossy(message)),
            Event::ConnectionClosed { code } => println!("door: connection {conn} closed {code:#x}"),
            _ => {}
        }
        let drain = door.session(conn, now).and_then(|s| s.connect().map(|c| c.path.windows(5).any(|w| w == b"drain"))).unwrap_or(false);
        let Some(mut s) = door.session(conn, now) else { return };
        match event {
            Event::Readable(id) => loop {
                match s.read(id, 16 << 10) {
                    Read::Data(b) => {
                        if id & 2 == 0 {
                            let _ = s.write(id, &b);
                        } else if let Some(out) = s.open(false) {
                            let _ = s.write(out, &b);
                            s.finish(out);
                        }
                    }
                    Read::Fin => {
                        if id & 2 == 0 {
                            s.finish(id);
                        }
                        s.forget(id);
                        break;
                    }
                    _ => break,
                }
            },
            Event::Datagram => {
                while let Some(d) = s.recv_datagram() {
                    let _ = s.send_datagram(&d);
                }
                if drain && self.drained.insert(conn) {
                    println!("door: session {conn}: GOAWAY and WT_DRAIN_SESSION");
                    s.drain(true);
                }
            }
            _ => {}
        }
    }
}

fn main() {
    if let Err(e) = run() {
        eprintln!("door_lab: {e}");
    }
}

fn run() -> Result<(), String> {
    let all: Vec<String> = std::env::args().skip(1).collect();
    let open = all.iter().any(|a| a == "--open");
    let cert_out = all.iter().position(|a| a == "--cert-out").and_then(|i| all.get(i + 1)).cloned();
    let mut args = all.into_iter().filter(|a| !a.starts_with("--")).filter(|a| cert_out.as_deref() != Some(a.as_str()));
    let page = std::fs::read(args.next().ok_or("usage: door_lab <page.html> [http port] [udp port]")?).map_err(|e| e.to_string())?;
    let http_port: u16 = args.next().and_then(|p| p.parse().ok()).unwrap_or(18481);
    let udp_port: u16 = args.next().and_then(|p| p.parse().ok()).unwrap_or(18444);
    let (der, key, hash) = cert()?;
    if let Some(path) = &cert_out {
        std::fs::write(path, &der).map_err(|e| e.to_string())?;
    }
    let tls = kiln_http::h3::door::tls(vec![CertificateDer::from(der)], PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key))).map_err(|e| e.to_string())?;
    let door = Door::new(DoorConfig::door(), tls, None).map_err(|e| e.to_string())?;
    let cfg = RuntimeConfig {
        shards: 1,
        tasks_per_shard: 4096,
        timers_per_shard: 8192,
        interests_per_shard: hyper_rt::runtime::interests_for(4096),
        ring_entries: 1024,
        step_budget_ns: 250_000,
        timer_tick_ns: 100_000,
        batch: 64,
        pin: false,
        cores: Vec::new(),
        page_bytes: 4096,
        spin_ns: 0,
        wake_tracking: None,
        latency_objective_ns: None,
    };
    let runtime = Runtime::start(&cfg).map_err(|e| e.to_string())?;
    if std::env::var_os("DOOR_LAB_LIVE").is_some() {
        hyper_measure::alloc::begin_process();
        let _ = std::thread::Builder::new().name("live".into()).spawn(|| {
            let mut m = 0u64;
            loop {
                let c = hyper_measure::alloc::read_process();
                println!("live: minute={m} live={} B peak={} B allocations={}", c.live, c.peak, c.allocations);
                std::thread::sleep(std::time::Duration::from_secs(60));
                m += 1;
            }
        });
    }
    let holder = runtime.shard_ids().first().and_then(|id| runtime.holder_of(*id)).ok_or("no shard")?;
    let tickets = Arc::new(Mutex::new(BTreeSet::new()));
    let (done_tx, done_rx) = std::sync::mpsc::channel::<String>();
    let t2 = Arc::clone(&tickets);
    hyper_rt::runtime::submit_to_holder(holder, async move {
        // DOOR_LAB_BIND=0.0.0.0 for a generator on another machine (step 10's soak).
        let ip: std::net::IpAddr = std::env::var("DOOR_LAB_BIND").ok().and_then(|a| a.parse().ok()).unwrap_or(std::net::IpAddr::from([127, 0, 0, 1]));
        let udp = SocketAddr::new(ip, udp_port);
        let driver = match Driver::bind(door, udp) {
            Ok(d) => d,
            Err(e) => {
                let _ = done_tx.send(format!("bind {udp}: {e}"));
                return;
            }
        };
        let listener = match TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], http_port)), 64) {
            Ok(l) => l,
            Err(e) => {
                let _ = done_tx.send(format!("bind http {http_port}: {e}"));
                return;
            }
        };
        println!("door_lab: page http://127.0.0.1:{http_port}/ door udp 127.0.0.1:{udp_port} hash {}", hex(&hash));
        let page = Bytes::from(page);
        let _ = hyper_rt::futures::spawn_detached(async move {
            let mut n = 0u64;
            while let Ok(stream) = listener.accept().await {
                let (page, tickets) = (page.clone(), Arc::clone(&t2));
                n += 1;
                let seed = n;
                let _ = hyper_rt::futures::spawn_detached(async move {
                    let svc = service_fn(move |req: Request<Incoming>| {
                        let (page, tickets) = (page.clone(), Arc::clone(&tickets));
                        async move {
                            let path = req.uri().path().to_owned();
                            let r = if path == "/info" {
                                let raw: [u8; 16] = std::array::from_fn(|i| (seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) >> (i % 8 * 8)) as u8 ^ i as u8 ^ (SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0) >> (i % 4 * 8)) as u8);
                                let ticket = hex(&raw);
                                if let Ok(mut t) = tickets.lock() {
                                    t.insert(ticket.clone());
                                }
                                let q = req.uri().query().unwrap_or_default();
                                let route = if q.contains("drain") { "/wt/echo/drain" } else { "/wt/echo" };
                                let body = format!("{{\"url\":\"https://127.0.0.1:{udp_port}{route}?ticket={ticket}\",\"hash\":\"{}\"}}", hex(&hash));
                                Response::builder().header("content-type", "application/json").body(Full::new(Bytes::from(body)))
                            } else if path == "/result" {
                                // What the page saw, for the lab's log.
                                use http_body_util::BodyExt as _;
                                if let Ok(body) = req.into_body().collect().await {
                                    println!("page: {}", String::from_utf8_lossy(&body.to_bytes()));
                                }
                                Response::builder().status(StatusCode::NO_CONTENT).body(Full::new(Bytes::new()))
                            } else {
                                Response::builder().header("content-type", "text/html").body(Full::new(page))
                            };
                            r.unwrap_or_default()
                        }
                    });
                    let _ = kiln_http::server::http1::Builder::new().serve(TcpIo::new(stream), svc).await;
                });
            }
        });
        let mut lab = Lab { tickets: Arc::clone(&tickets), drained: BTreeSet::new(), open };
        let r = driver.run(&mut lab).await;
        let _ = done_tx.send(format!("driver ended: {r:?}"));
    })
    .map_err(|e| e.to_string())?;
    let why = done_rx.recv().map_err(|e| e.to_string())?;
    drop(runtime);
    Err(why)
}
