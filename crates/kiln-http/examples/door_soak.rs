//! A lab load generator (goal 4 step 10, plan 01a1a832eb597110a6dcfda227981604): many admitted
//! sessions that echo now and then, and attacking connections running step 8's attacks in
//! rotation, against a door on a real UDP socket (`door_lab --open`), from another machine. It
//! drops `loss` percent of what it sends and receives (the loss a lossy path gives, on its side).
//! Every minute it prints what it has: sessions up, echoes answered and missed, attack bytes sent.
//!
//!   door_soak <door udp addr> <door cert DER file> <sessions> <attackers> <seconds> <loss %>
//!
//! A lab program: std's blocking UDP socket, one thread, no runtime. Never against production.

#![allow(clippy::print_stdout, clippy::print_stderr, reason = "a lab program says what it does")]

use std::collections::BTreeMap;
use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use hyper_quic::rustls::pki_types::CertificateDer;
use hyper_quic::{ClientConfig, Connection, ConnectionHandle, DatagramEvent, Dir, Endpoint, EndpointConfig, Event, StreamEvent, StreamId, TransportConfig, VarInt};

/// What a connection does.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Role {
    Session,
    Attack(u8),
}

const ATTACKS: [&str; 8] = ["count accordion", "static accordion", "unknown uni streams", "pre-session WT streams", "rapid reset", "huge capsule", "connect dribble", "datagrams for no session"];

struct Peer {
    handle: ConnectionHandle,
    conn: Connection,
    role: Role,
    opened: bool,
    connect: Option<u64>,
    admitted: bool,
    next_echo: Instant,
    echo_out: Option<(u64, Instant)>,
    got: BTreeMap<u64, Vec<u8>>,
    tick: u64,
    /// Why its connection ended, once it has.
    lost: Option<String>,
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0.max(1);
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

fn v(n: u64) -> Vec<u8> {
    let mut out = Vec::new();
    match n {
        0..=0x3f => out.push(n as u8),
        0x40..=0x3fff => out.extend_from_slice(&((n as u16) | 0x4000).to_be_bytes()),
        0x4000..=0x3fff_ffff => out.extend_from_slice(&((n as u32) | 0x8000_0000).to_be_bytes()),
        _ => out.extend_from_slice(&(n | 0xc000_0000_0000_0000).to_be_bytes()),
    }
    out
}

fn frame(kind: u64, payload: &[u8]) -> Vec<u8> {
    let mut out = v(kind);
    out.extend(v(payload.len() as u64));
    out.extend_from_slice(payload);
    out
}

fn sid(id: u64) -> Option<StreamId> {
    VarInt::from_u64(id).ok().map(StreamId::from)
}

/// A field section of literal fields (no table, no Huffman).
fn section(fields: &[(&[u8], &[u8])]) -> Vec<u8> {
    let mut b = vec![0u8, 0u8];
    for (n, val) in fields {
        push_int(&mut b, 0x20, 3, n.len() as u64);
        b.extend_from_slice(n);
        push_int(&mut b, 0x00, 7, val.len() as u64);
        b.extend_from_slice(val);
    }
    frame(0x01, &b)
}

fn push_int(out: &mut Vec<u8>, flags: u8, n: u32, mut value: u64) {
    let mask = (1u64 << n) - 1;
    if value < mask {
        out.push(flags | value as u8);
        return;
    }
    out.push(flags | mask as u8);
    value -= mask;
    while value >= 0x80 {
        out.push((value & 0x7f) as u8 | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn connect_frame(path: &[u8]) -> Vec<u8> {
    section(&[(b":method", b"CONNECT"), (b":protocol", b"webtransport"), (b":scheme", b"https"), (b":authority", b"127.0.0.1"), (b":path", path), (b"origin", b"http://127.0.0.1"), (b"sec-webtransport-http3-draft02", b"1")])
}

fn control() -> Vec<u8> {
    let mut p = Vec::new();
    for (id, val) in [(0x01u64, 0u64), (0x07, 0), (0x33, 1), (0xff_d277, 1), (0x2b60_3742, 1)] {
        p.extend(v(id));
        p.extend(v(val));
    }
    let mut out = v(0);
    out.extend(frame(0x04, &p));
    out
}

impl Peer {
    fn write(&mut self, id: u64, bytes: &[u8]) -> usize {
        sid(id).map_or(0, |s| self.conn.send_stream(s).write(bytes).unwrap_or(0))
    }
    fn open(&mut self, bidi: bool) -> Option<u64> {
        self.conn.streams().open(if bidi { Dir::Bi } else { Dir::Uni }).map(u64::from)
    }
    fn finish(&mut self, id: u64) {
        if let Some(s) = sid(id) {
            let _ = self.conn.send_stream(s).finish();
        }
    }
    fn reset(&mut self, id: u64) {
        if let Some(s) = sid(id) {
            let _ = self.conn.send_stream(s).reset(VarInt::from_u32(0x10c));
        }
    }

    /// Reads every stream it watches.
    fn pump(&mut self) {
        while let Some(ev) = self.conn.poll() {
            match ev {
                Event::Stream(StreamEvent::Opened { dir }) => {
                    while let Some(s) = self.conn.streams().accept(dir) {
                        self.got.entry(u64::from(s)).or_default();
                    }
                }
                Event::DatagramReceived => while self.conn.datagrams().recv().is_some() {},
                Event::ConnectionLost { reason } => self.lost = Some(format!("{reason}")),
                _ => {}
            }
        }
        let ids: Vec<u64> = self.got.keys().copied().collect();
        for id in ids {
            let Some(s) = sid(id) else { continue };
            let mut recv = self.conn.recv_stream(s);
            let Ok(mut chunks) = recv.read(true) else {
                self.got.remove(&id);
                continue;
            };
            let mut ended = false;
            loop {
                match chunks.next(usize::MAX) {
                    Ok(Some(c)) => self.got.entry(id).or_default().extend_from_slice(&c.bytes),
                    Ok(None) => {
                        ended = true;
                        break;
                    }
                    Err(hyper_quic::ReadError::Reset(_)) => {
                        ended = true;
                        break;
                    }
                    Err(_) => break,
                }
            }
            let _ = chunks.finalize();
            if ended && Some(id) != self.connect && self.echo_out.is_none_or(|(e, _)| e != id) {
                self.got.remove(&id);
            }
        }
    }
}

fn client_config(cert: &[u8]) -> Result<ClientConfig, String> {
    use hyper_quic::rustls;
    let mut roots = rustls::RootCertStore::empty();
    roots.add(CertificateDer::from(cert.to_vec())).map_err(|e| e.to_string())?;
    let mut c = rustls::ClientConfig::builder_with_provider(&rustls::crypto::aws_lc_rs::DEFAULT_PROVIDER)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| e.to_string())?
        .with_root_certificates(roots)
        .with_no_client_auth();
    c.alpn_protocols = vec![b"h3".to_vec()];
    let crypto = hyper_quic::crypto::rustls::QuicClientConfig::try_from(c).map_err(|e| format!("{e:?}"))?;
    let mut cc = ClientConfig::new(Box::new(crypto));
    let mut t = TransportConfig::default();
    t.datagram_receive_buffer_size(Some(64 << 10)).max_idle_timeout(hyper_quic::IdleTimeout::try_from(Duration::from_secs(120)).ok()).keep_alive_interval(Some(Duration::from_secs(10)));
    cc.transport_config(t);
    Ok(cc)
}

#[derive(Default, Debug)]
struct Stats {
    dialled: u64,
    /// Attackers' connections made again after the door ended theirs.
    redialled: u64,
    admitted: u64,
    echoes_ok: u64,
    echoes_missed: u64,
    attack_bytes: u64,
    lost: u64,
}

fn main() {
    if let Err(e) = run() {
        eprintln!("door_soak: {e}");
    }
}

#[allow(clippy::disallowed_methods, reason = "a lab program on the host's clock")]
fn run() -> Result<(), String> {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let arg = |i: usize| a.get(i).cloned().ok_or("usage: door_soak <door addr> <cert.der> <sessions> <attackers> <seconds> <loss %>");
    let door: SocketAddr = arg(0)?.parse().map_err(|e| format!("{e}"))?;
    let cert = std::fs::read(arg(1)?).map_err(|e| e.to_string())?;
    let sessions: usize = arg(2)?.parse().map_err(|e| format!("{e}"))?;
    let attackers: usize = arg(3)?.parse().map_err(|e| format!("{e}"))?;
    let seconds: u64 = arg(4)?.parse().map_err(|e| format!("{e}"))?;
    let loss: f64 = arg(5)?.parse().map_err(|e| format!("{e}"))?;
    let socket = UdpSocket::bind(if door.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" }).map_err(|e| e.to_string())?;
    socket.set_read_timeout(Some(Duration::from_millis(2))).map_err(|e| e.to_string())?;
    let local = socket.local_addr().map_err(|e| e.to_string())?;
    let mut endpoint = Endpoint::new(EndpointConfig::default(), None, true, None).map_err(|e| format!("{e:?}"))?;
    let cfg = endpoint.insert_client_config(client_config(&cert)?).map_err(|e| format!("{e:?}"))?;
    let start = Instant::now();
    let end = start + Duration::from_secs(seconds);
    let mut peers: Vec<Peer> = Vec::new();
    let mut by_handle: BTreeMap<usize, usize> = BTreeMap::new();
    let mut rng = Rng(0x5eed_d00d);
    let mut stats = Stats::default();
    let mut buf = vec![0u8; 65_536];
    let mut out = Vec::with_capacity(65_536);
    let mut next_report = start + Duration::from_secs(60);
    // Where the loop's time goes, told every 10 seconds (the generator must keep up for the
    // numbers to be the door's).
    let mut next_pulse = start + Duration::from_secs(10);
    let (mut turns, mut t_recv, mut t_work, mut t_send) = (0u64, Duration::ZERO, Duration::ZERO, Duration::ZERO);
    let total = sessions + attackers;
    let accordion = section(&(0..1500).map(|_| (&b"a"[..], &b""[..])).collect::<Vec<_>>());
    let statics = frame(0x01, &[&[0u8, 0][..], &vec![0xc0 | 58; 4500]].concat());
    // One DATA frame as long as QUIC allows, holding one unknown capsule of 2^62 - 64 bytes.
    let mut huge = v(0x00);
    huge.extend(v((1 << 62) - 1));
    huge.extend(v(0x1f * 9 + 0x29));
    huge.extend(v((1 << 62) - 64));
    let chunk = vec![0u8; 8192];
    loop {
        let now = Instant::now();
        if now >= end {
            break;
        }
        // Dial up to 200 a second until every connection is up.
        let due = ((now - start).as_millis() as usize / 5).min(total);
        while peers.len() < due {
            let i = peers.len();
            let role = if i < sessions { Role::Session } else { Role::Attack(((i - sessions) % ATTACKS.len()) as u8) };
            match endpoint.connect(now, cfg, door, "localhost", None) {
                Ok((handle, conn)) => {
                    by_handle.insert(handle.0, i);
                    peers.push(Peer { handle, conn, role, opened: false, connect: None, admitted: false, next_echo: now + Duration::from_secs(5 + i as u64 % 30), echo_out: None, got: BTreeMap::new(), tick: 0, lost: None });
                    stats.dialled += 1;
                }
                Err(e) => return Err(format!("connect: {e:?}")),
            }
        }
        turns += 1;
        let t0 = Instant::now();
        // Receive.
        for _ in 0..4096 {
            match socket.recv_from(&mut buf) {
                Ok((n, from)) => {
                    if (rng.next() % 10_000) as f64 / 100.0 < loss {
                        stats.lost += 1;
                        continue;
                    }
                    let data = BytesMut::from(buf.get(..n).unwrap_or_default());
                    out.clear();
                    match endpoint.handle(now, from, None, None, data, &mut out) {
                        Some(DatagramEvent::ConnectionEvent(h, ev)) => {
                            if let Some(p) = by_handle.get(&h.0).and_then(|&i| peers.get_mut(i)) {
                                p.conn.handle_event(ev, endpoint.configs_mut());
                            }
                        }
                        Some(DatagramEvent::Response(t)) => {
                            let _ = socket.send_to(out.get(..t.size).unwrap_or_default(), t.destination);
                        }
                        _ => {}
                    }
                }
                Err(_) => break,
            }
        }
        let t1 = Instant::now();
        t_recv += t1 - t0;
        // Each connection's work.
        for (i, p) in peers.iter_mut().enumerate() {
            // An attacker whose connection the door ended comes back on a new one (an attacker
            // keeps attacking), two seconds on.
            if let Role::Attack(_) = p.role
                && p.conn.is_closed()
                && now >= p.next_echo
            {
                if let Ok((handle, conn)) = endpoint.connect(now, cfg, door, "localhost", None) {
                    if by_handle.get(&p.handle.0) == Some(&i) {
                        by_handle.remove(&p.handle.0);
                    }
                    by_handle.insert(handle.0, i);
                    *p = Peer { handle, conn, role: p.role, opened: false, connect: None, admitted: false, next_echo: now + Duration::from_secs(2), echo_out: None, got: BTreeMap::new(), tick: 0, lost: p.lost.take() };
                    stats.redialled += 1;
                }
                continue;
            }
            if p.conn.poll_timeout().is_some_and(|t| t <= now) {
                p.conn.handle_timeout(now);
            }
            p.pump();
            if p.conn.is_handshaking() || p.conn.is_closed() {
                continue;
            }
            if !p.opened {
                p.opened = true;
                if let Some(c) = p.open(false) {
                    let ctl = control();
                    p.write(c, &ctl);
                }
                let post = matches!(p.role, Role::Session | Role::Attack(4) | Role::Attack(5));
                if post && let Some(id) = p.open(true) {
                    p.write(id, &connect_frame(b"/wt/echo?ticket=soak"));
                    p.connect = Some(id);
                    p.got.entry(id).or_default();
                }
            }
            if !p.admitted && p.connect.is_some_and(|c| p.got.get(&c).is_some_and(|g| g.len() > 2)) {
                p.admitted = true;
                stats.admitted += 1;
            }
            p.tick += 1;
            match p.role {
                Role::Session if p.admitted => {
                    if let Some((id, at)) = p.echo_out {
                        if p.got.get(&id).is_some_and(|g| g.as_slice() == b"ping") {
                            stats.echoes_ok += 1;
                            p.got.remove(&id);
                            p.echo_out = None;
                        } else if now.duration_since(at) > Duration::from_secs(20) {
                            stats.echoes_missed += 1;
                            p.got.remove(&id);
                            p.echo_out = None;
                        }
                    } else if now >= p.next_echo
                        && let Some(id) = p.open(true)
                    {
                        let mut b = vec![0x40, 0x41];
                        b.extend(v(p.connect.unwrap_or_default()));
                        b.extend_from_slice(b"ping");
                        p.write(id, &b);
                        p.finish(id);
                        p.got.entry(id).or_default();
                        p.echo_out = Some((id, now));
                        p.next_echo = now + Duration::from_secs(30);
                    }
                }
                Role::Attack(k) if p.tick % 4 == 0 => {
                    let sent = match k {
                        0 | 1 => p.open(true).map_or(0, |id| {
                            let n = p.write(id, if k == 0 { &accordion } else { &statics });
                            p.finish(id);
                            n
                        }),
                        2 => p.open(false).map_or(0, |id| {
                            let mut b = v(0x1f * (rng.next() % 100) + 0x21);
                            b.extend_from_slice(chunk.get(..512).unwrap_or_default());
                            let n = p.write(id, &b);
                            p.finish(id);
                            n
                        }),
                        3 => p.open(false).map_or(0, |id| {
                            let mut b = v(0x54);
                            b.extend(v(4 * 1000));
                            b.extend_from_slice(chunk.get(..512).unwrap_or_default());
                            p.write(id, &b)
                        }),
                        4 if p.admitted => p.open(true).map_or(0, |id| {
                            let n = p.write(id, &connect_frame(b"/x"));
                            p.reset(id);
                            n
                        }),
                        5 if p.admitted => {
                            let c = p.connect.unwrap_or_default();
                            if p.tick == 4 { p.write(c, &huge) } else { p.write(c, &chunk) }
                        }
                        6 => {
                            let c = p.connect.get_or_insert(0);
                            let id = *c;
                            if p.tick == 4 {
                                let _ = p.open(true);
                            }
                            let whole = connect_frame(b"/wt/echo?ticket=dribble");
                            match whole.get((p.tick / 4) as usize % (whole.len() - 1)) {
                                Some(b) => p.write(id, &[*b]),
                                None => 0,
                            }
                        }
                        7 => {
                            let mut d = v(4 * (rng.next() % 1000) + 1);
                            d.extend_from_slice(chunk.get(..1000).unwrap_or_default());
                            let n = d.len();
                            if p.conn.datagrams().send(Bytes::from(d), true).is_ok() { n } else { 0 }
                        }
                        _ => 0,
                    };
                    stats.attack_bytes += sent as u64;
                }
                _ => {}
            }
        }
        let t2 = Instant::now();
        t_work += t2 - t1;
        // Send.
        for p in &mut peers {
            for _ in 0..16 {
                out.clear();
                let Some(t) = p.conn.poll_transmit(now, 1, &mut out, endpoint.configs()) else { break };
                if (rng.next() % 10_000) as f64 / 100.0 < loss {
                    stats.lost += 1;
                    continue;
                }
                let _ = socket.send_to(out.get(..t.size).unwrap_or_default(), t.destination);
            }
            while let Some(ev) = p.conn.poll_endpoint_events() {
                if let Some(back) = endpoint.handle_event(p.handle, ev) {
                    p.conn.handle_event(back, endpoint.configs_mut());
                }
            }
        }
        t_send += Instant::now() - t2;
        if now >= next_pulse {
            next_pulse += Duration::from_secs(10);
            let up = peers.iter().filter(|p| !p.conn.is_handshaking() && !p.conn.is_closed()).count();
            println!("soak: pulse t={}s: {} peers, {up} up, {} admitted; {turns} turns: receive {:?}, work {:?}, send {:?}", (now - start).as_secs(), peers.len(), stats.admitted, t_recv, t_work, t_send);
            (turns, t_recv, t_work, t_send) = (0, Duration::ZERO, Duration::ZERO, Duration::ZERO);
        }
        if now >= next_report {
            next_report += Duration::from_secs(60);
            let alive = peers.iter().filter(|p| !p.conn.is_closed()).count();
            let mut why: BTreeMap<(String, &str), usize> = BTreeMap::new();
            for p in &peers {
                if let Some(l) = &p.lost {
                    let role = match p.role {
                        Role::Session => "session",
                        Role::Attack(k) => ATTACKS.get(k as usize).copied().unwrap_or("attack"),
                    };
                    *why.entry((l.clone(), role)).or_default() += 1;
                }
            }
            println!("soak: connections ended: {why:?}");
            println!("soak: t={}s from {local}: {alive} connections alive of {} dialled ({} attackers' again); {} sessions admitted; echoes {} answered, {} missed; {} attack bytes sent; {} datagrams dropped for loss", (now - start).as_secs(), stats.dialled, stats.redialled, stats.admitted, stats.echoes_ok, stats.echoes_missed, stats.attack_bytes, stats.lost);
        }
    }
    let alive = peers.iter().filter(|p| !p.conn.is_closed()).count();
    println!("soak: done after {seconds}s: {alive} alive; {stats:?}");
    Ok(())
}
