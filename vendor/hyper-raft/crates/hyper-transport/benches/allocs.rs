//! What an exchange and a lane frame cost: allocations, reallocations, bytes and time, counted
//! by hyper-measure's allocator. `cargo bench -p hyper-transport --bench allocs`
//! (docs/benchmarks.md, "hyper-transport").
//!
//! Two endpoints are joined by the in-memory network of `tests/common`, which reuses its datagram
//! buffers, so the counts are the two endpoints' own: both sides of every exchange, hyper-quic
//! included. The same exchanges on bare hyper-quic streams, with the same network, are the
//! baseline: the difference is what the application layer adds.

#![allow(
    clippy::unwrap_in_result,
    clippy::type_complexity,
    clippy::too_many_arguments,
    clippy::string_slice,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cognitive_complexity,
    missing_docs
)]

#[path = "../tests/common/mod.rs"]
mod common;

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use common::*;
use hyper_measure::alloc;
use hyper_quic::{
    ClientConfig, Connection, ConnectionHandle, DatagramEvent, Dir, Endpoint as QuicEndpoint,
    EndpointConfig, ServerConfig, StreamId, Transmit,
};
use hyper_transport::tls::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use hyper_transport::{Event, ExchangeId, Progress};

#[global_allocator]
static ALLOCATOR: alloc::Counting = alloc::Counting;

/// Exchanges counted per row, after the warm-up.
const ROUNDS: u64 = 2_000;
const WARM: u64 = 200;
/// The head each side sends: a request's or a reply's header.
const HEAD: usize = 16;

/// One request and its reply on hyper-transport, `body` bytes each way.
fn exchange(net: &mut Net<Node<Mantle>, Node<Mantle>>, body: Option<u64>, piece: &[u8]) {
    let progress = Progress::new(Duration::from_secs(10)).unwrap();
    let head = [7u8; HEAD];
    let id = net
        .a
        .open(net.now, 2, Kind::Get, &head, body, progress)
        .unwrap();
    let length = body.unwrap_or(0);
    let (mut written, mut served): (u64, Option<(ExchangeId, u64, u64, bool)>) = (0, None);
    let (mut read, mut answered, mut done) = (0u64, false, false);
    for _ in 0..100_000 {
        while written < length {
            let take = ((length - written) as usize).min(piece.len());
            match net.a.write_body(id, &piece[..take]).unwrap() {
                0 => break,
                took => written += took as u64,
            }
        }
        net.exchange();
        while let Some(event) = net.b.poll_event() {
            if let Event::Request { exchange, .. } = event {
                served = Some((exchange, 0, 0, false));
            }
        }
        if let Some((exchange, got, sent, replied)) = &mut served {
            while *got < length {
                let mut into = net.b.reserve(Class::Request, piece.len() as u64).unwrap();
                let n = net.b.read_body(*exchange, &mut into).unwrap();
                net.b.release(into);
                if n == 0 {
                    break;
                }
                *got += n as u64;
            }
            if *got == length && (body.is_none() || net.b.body_complete(*exchange)) && !*replied {
                net.b.reply(*exchange, &head, body).unwrap();
                *replied = true;
            }
            while *replied && *sent < length {
                let take = ((length - *sent) as usize).min(piece.len());
                match net.b.write_body(*exchange, &piece[..take]).unwrap() {
                    0 => break,
                    took => *sent += took as u64,
                }
            }
            if *replied && *sent == length {
                net.b.end(*exchange);
                served = None;
            }
        }
        net.exchange();
        while let Some(event) = net.a.poll_event() {
            if let Event::Reply { .. } = event {
                answered = true;
            }
        }
        if answered {
            while read < length {
                let mut into = net.a.reserve(Class::Request, piece.len() as u64).unwrap();
                let n = net.a.read_body(id, &mut into).unwrap();
                net.a.release(into);
                if n == 0 {
                    break;
                }
                read += n as u64;
            }
            if read == length && (body.is_none() || net.a.body_complete(id)) {
                net.a.end(id);
                done = true;
            }
        }
        if done && served.is_none() {
            net.exchange();
            return;
        }
        if !net.exchange() {
            net.advance();
        }
    }
    panic!("the exchange never completed");
}

#[allow(
    clippy::disallowed_methods,
    reason = "a benchmark measures real time on the host"
)]
fn transport_net() -> Net<Node<Mantle>, Node<Mantle>> {
    let pair = Pair::new();
    let now = Instant::now();
    let mut wide = limits();
    wide.exchanges = 1_024;
    let book = || pair.book(Role::Node, Role::Node);
    let a = pair.node::<Mantle>(1, Role::Node, book(), wide, 1 << 30, now);
    let b = pair.node::<Mantle>(2, Role::Node, book(), wide, 1 << 30, now);
    let mut net = Net::new(now, a, b);
    let address = net.b_address;
    net.a.connect(now, 2, address).unwrap();
    net.until(10_000, |net| {
        let mut connected = false;
        while let Some(event) = net.a.poll_event() {
            connected |= matches!(event, Event::Connected { .. });
        }
        while net.b.poll_event().is_some() {}
        connected
    });
    net
}

struct Row {
    name: String,
    counts: alloc::Counts,
    took: Duration,
    rounds: u64,
}

fn report(rows: &[Row]) {
    println!("| Workload | Allocations | Reallocations | Bytes | Time |");
    println!("|---|---|---|---|---|");
    for row in rows {
        let rounds = row.rounds as f64;
        println!(
            "| {} | {:.2} | {:.2} | {:.0} | {:.1} µs |",
            row.name,
            row.counts.allocations as f64 / rounds,
            row.counts.reallocations as f64 / rounds,
            row.counts.bytes as f64 / rounds,
            row.took.as_secs_f64() * 1e6 / rounds,
        );
    }
}

#[allow(
    clippy::disallowed_methods,
    reason = "a benchmark measures real time on the host"
)]
fn measure(name: &str, rounds: u64, mut work: impl FnMut()) -> Row {
    for _ in 0..WARM {
        work();
    }
    let began = Instant::now();
    alloc::begin();
    for _ in 0..rounds {
        work();
    }
    let counts = alloc::end();
    Row {
        name: name.to_owned(),
        counts,
        took: began.elapsed(),
        rounds,
    }
}

// ---------------------------------------------------------------------------------------------
// The baseline: the same exchanges on bare hyper-quic streams.

struct Raw {
    endpoint: QuicEndpoint,
    connection: Option<(ConnectionHandle, Connection)>,
    scratch: Vec<u8>,
    receive: bytes::BytesMut,
}

impl Drive for Raw {
    fn datagram(&mut self, now: Instant, from: SocketAddr, bytes: &[u8]) {
        self.receive.reserve(bytes.len());
        self.receive.extend_from_slice(bytes);
        let datagram = self.receive.split_to(bytes.len());
        self.scratch.clear();
        match self
            .endpoint
            .handle(now, from, None, None, datagram, &mut self.scratch)
        {
            Some(DatagramEvent::NewConnection(incoming)) => {
                let mut out = Vec::new();
                self.connection = Some(
                    self.endpoint
                        .accept(incoming, now, &mut out, None, None)
                        .unwrap(),
                );
            }
            Some(DatagramEvent::ConnectionEvent(_, event)) => {
                if let Some((_, connection)) = &mut self.connection {
                    connection.handle_event(event, self.endpoint.configs_mut());
                }
            }
            _ => {}
        }
        self.events();
    }
    fn transmit(&mut self, now: Instant, out: &mut Vec<u8>) -> Option<Transmit> {
        let (_, connection) = self.connection.as_mut()?;
        out.clear();
        let transmit = connection.poll_transmit(now, 1, out, self.endpoint.configs());
        self.events();
        transmit
    }
    fn timeout(&mut self) -> Option<Instant> {
        self.connection
            .as_mut()
            .and_then(|(_, connection)| connection.poll_timeout())
    }
    fn fire(&mut self, now: Instant) {
        if let Some((_, connection)) = &mut self.connection {
            connection.handle_timeout(now);
        }
        self.events();
    }
}

impl Raw {
    fn events(&mut self) {
        if let Some((handle, connection)) = &mut self.connection {
            while let Some(event) = connection.poll_endpoint_events() {
                if let Some(event) = self.endpoint.handle_event(*handle, event) {
                    connection.handle_event(event, self.endpoint.configs_mut());
                }
            }
        }
    }
    fn connection(&mut self) -> &mut Connection {
        &mut self.connection.as_mut().unwrap().1
    }
}

/// Reads stream `id` to its end or to what has arrived; returns the bytes and whether it ended.
fn drain(connection: &mut Connection, id: StreamId) -> (u64, bool) {
    let mut recv = connection.recv_stream(id);
    let Ok(mut chunks) = recv.read(true) else {
        return (0, true);
    };
    let mut got = 0u64;
    let finished = loop {
        match chunks.next(usize::MAX) {
            Ok(Some(chunk)) => got += chunk.bytes.len() as u64,
            Ok(None) => break true,
            Err(_) => break false,
        }
    };
    let _ = chunks.finalize();
    (got, finished)
}

fn write_all(
    connection: &mut Connection,
    id: StreamId,
    written: &mut u64,
    total: u64,
    piece: &[u8],
) -> bool {
    while *written < total {
        let take = ((total - *written) as usize).min(piece.len());
        match connection.send_stream(id).write(&piece[..take]) {
            Ok(took) if took > 0 => *written += took as u64,
            _ => return false,
        }
    }
    connection.send_stream(id).finish().is_ok()
}

fn raw_exchange(net: &mut Net<Raw, Raw>, body: u64, piece: &[u8]) {
    let total = HEAD as u64 + body;
    let id = net.a.connection().streams().open(Dir::Bi).unwrap();
    let (mut sent, mut finished) = (0u64, false);
    let (mut server, mut server_read, mut server_sent, mut server_done) =
        (None::<StreamId>, 0u64, 0u64, false);
    let mut read = 0u64;
    for _ in 0..100_000 {
        if !finished {
            finished = write_all(net.a.connection(), id, &mut sent, total, piece);
        }
        net.exchange();
        let b = net.b.connection();
        while b.poll().is_some() {}
        if server.is_none() {
            server = b.streams().accept(Dir::Bi);
        }
        if let Some(stream) = server {
            if server_read < total {
                server_read += drain(b, stream).0;
            }
            if server_read == total && !server_done {
                server_done = write_all(b, stream, &mut server_sent, total, piece);
            }
        }
        net.exchange();
        let a = net.a.connection();
        while a.poll().is_some() {}
        let (got, ended) = drain(a, id);
        read += got;
        if ended && read == total && server_done {
            net.exchange();
            return;
        }
        if !net.exchange() {
            net.advance();
        }
    }
    panic!("the raw exchange never completed");
}

#[allow(
    clippy::disallowed_methods,
    reason = "a benchmark measures real time on the host"
)]
fn raw_net() -> Net<Raw, Raw> {
    let pki = Pki::new();
    let (certificate, key) = pki.issue("node-2");
    let chain = vec![CertificateDer::from(certificate)];
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key));
    let server = ServerConfig::with_single_cert(chain, key).unwrap();
    let mut roots = hyper_quic::rustls::RootCertStore::empty();
    roots.add(CertificateDer::from(pki.root.clone())).unwrap();
    let client = ClientConfig::with_root_certificates(roots).unwrap();
    let now = Instant::now();
    let raw = |server: Option<ServerConfig>| Raw {
        endpoint: QuicEndpoint::new(EndpointConfig::default(), server, true, None).unwrap(),
        connection: None,
        scratch: Vec::new(),
        receive: bytes::BytesMut::new(),
    };
    let mut net = Net::new(now, raw(None), raw(Some(server)));
    let config = net.a.endpoint.insert_client_config(client).unwrap();
    let address = net.b_address;
    net.a.connection = Some(
        net.a
            .endpoint
            .connect(now, config, address, "node-2", None)
            .unwrap(),
    );
    net.until(10_000, |net| {
        net.b
            .connection
            .as_mut()
            .is_some_and(|(_, connection)| !connection.is_handshaking())
            && !net.a.connection().is_handshaking()
    });
    net
}

fn main() {
    let piece = vec![5u8; PIECE];
    let mut rows = Vec::new();
    let mut net = transport_net();
    rows.push(measure(
        "hyper-transport exchange, 16 B heads, no body",
        ROUNDS,
        || exchange(&mut net, None, &piece),
    ));
    rows.push(measure(
        "hyper-transport exchange, 16 B heads, 4 KiB bodies",
        ROUNDS,
        || {
            exchange(&mut net, Some(4 << 10), &piece);
        },
    ));
    rows.push(measure(
        "hyper-transport exchange, 16 B heads, 64 KiB bodies",
        ROUNDS / 4,
        || {
            exchange(&mut net, Some(64 << 10), &piece);
        },
    ));
    let frame = vec![9u8; 512];
    rows.push(measure(
        "hyper-transport lane frame, 512 B, 64 a batch",
        ROUNDS,
        || {
            for _ in 0..64 {
                net.a.send_frame(2, 0, Kind::Append, &frame).unwrap();
            }
            let mut arrived = 0;
            net.until(10_000, |net| {
                while let Some(event) = net.b.poll_event() {
                    if let Event::Frame { frame, .. } = event {
                        net.b.release(frame);
                        arrived += 1;
                    }
                }
                arrived == 64
            });
        },
    ));
    // A lane frame's row is per batch; per frame is a 64th of it.
    if let Some(row) = rows.last_mut() {
        row.rounds *= 64;
    }
    let mut raw = raw_net();
    rows.push(measure(
        "bare hyper-quic stream, 16 B each way",
        ROUNDS,
        || raw_exchange(&mut raw, 0, &piece),
    ));
    rows.push(measure(
        "bare hyper-quic stream, 4 KiB + 16 B each way",
        ROUNDS,
        || {
            raw_exchange(&mut raw, 4 << 10, &piece);
        },
    ));
    rows.push(measure(
        "bare hyper-quic stream, 64 KiB + 16 B each way",
        ROUNDS / 4,
        || {
            raw_exchange(&mut raw, 64 << 10, &piece);
        },
    ));
    assert!(
        alloc::installed(),
        "the counting allocator is not installed"
    );
    report(&rows);
}
