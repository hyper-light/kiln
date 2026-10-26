//! hyper-tls against upstream rustls 0.23.45, unmodified (the transports it replaces: slates' and
//! focal's both run upstream rustls; `docs/STATUS.md`'s owed bench), in one process with the same
//! certificates (ECDSA P-256, `test-ca`) and the same provider (aws-lc-rs, X25519MLKEM768 first):
//!
//! - **a handshake's time**, client and server together over memory, each handshake timed on its own,
//!   for a TLS 1.3 full handshake, a TLS 1.3 handshake resumed by ticket, and a TLS 1.2 full one;
//! - **a bulk transfer's time a MiB**, client to server on an established TLS 1.3 connection, written
//!   in 16 KiB plaintext records (the protocol's largest, RFC 8446 §5.1).
//!
//! ```text
//! cargo bench -p hyper-tls --bench vs_upstream -- [ROUNDS]
//! ```
//!
//! The two run in turn, `ROUNDS` times (5 by default), the order turned each round. Each shape's
//! handshakes are the run length a p99 needs for an upper end at 95% coverage (`docs/tails.md` §3.3),
//! shared among the rounds and pooled; a quantile is stated with its interval. The bulk transfer's
//! time a MiB is the median of its rounds. Beside the wall clock, which a loaded host moves between the
//! two sides' turns, the process's own account (`hyper_measure::usage`): its CPU time and, where the OS
//! counts them, its instructions and cycles a handshake, summed over every round, the bench being one
//! thread.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    clippy::disallowed_types,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cognitive_complexity,
    missing_docs
)]

use std::time::Instant;

use hyper_measure::quantile::{self, Level};

/// Shape: the intervals' coverage (`docs/tails.md` §3.3).
const COVERAGE: Level = match Level::new(95, 100) {
    Ok(level) => level,
    Err(_) => Level::P50,
};

/// Shape: the largest TLS plaintext record, RFC 8446 §5.1 (2^14 bytes).
const RECORD: usize = 1 << 14;

/// Shape: the bytes a bulk round moves, 64 MiB: long against a round's clock reads, and four thousand
/// records of the largest size.
const BULK_BYTES: usize = 64 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Shape {
    Tls13Full,
    Tls13Resumed,
    Tls12Full,
}

impl Shape {
    const ALL: [Self; 3] = [Self::Tls13Full, Self::Tls13Resumed, Self::Tls12Full];

    fn name(self) -> &'static str {
        match self {
            Self::Tls13Full => "TLS 1.3 full",
            Self::Tls13Resumed => "TLS 1.3 resumed by ticket",
            Self::Tls12Full => "TLS 1.2 full",
        }
    }

    fn tls13(self) -> bool {
        self != Self::Tls12Full
    }

    fn resumes(self) -> bool {
        self == Self::Tls13Resumed
    }
}

fn file(name: &str) -> &'static [u8] {
    match name {
        "ca.cert" => include_bytes!("../test-ca/ecdsa-p256/ca.cert"),
        "end.fullchain" => include_bytes!("../test-ca/ecdsa-p256/end.fullchain"),
        "end.key" => include_bytes!("../test-ca/ecdsa-p256/end.key"),
        other => panic!("no test file {other}"),
    }
}

/// hyper-tls: configurations lent to each call.
mod ours {
    use std::io::{self, Read, Write};

    use hyper_tls::crypto::aws_lc_rs;
    use hyper_tls::pki_types::pem::PemObject;
    use hyper_tls::pki_types::{CertificateDer, PrivateKeyDer};
    use hyper_tls::{
        ClientConfig, ClientConnection, RootCertStore, ServerConfig, ServerConnection,
    };

    use super::{file, Shape, RECORD};

    pub(super) struct Pair {
        client: ClientConfig,
        server: ServerConfig,
    }

    static TLS13: [&hyper_tls::SupportedProtocolVersion; 1] = [&hyper_tls::version::TLS13];
    static TLS12: [&hyper_tls::SupportedProtocolVersion; 1] = [&hyper_tls::version::TLS12];

    fn versions(shape: Shape) -> &'static [&'static hyper_tls::SupportedProtocolVersion] {
        if shape.tls13() {
            &TLS13
        } else {
            &TLS12
        }
    }

    pub(super) fn pair(shape: Shape) -> Pair {
        let provider: &'static _ = Box::leak(Box::new(aws_lc_rs::default_provider()));
        let mut server = ServerConfig::builder_with_provider(provider)
            .with_protocol_versions(versions(shape))
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                CertificateDer::pem_slice_iter(file("end.fullchain"))
                    .map(Result::unwrap)
                    .collect(),
                PrivateKeyDer::from_pem_slice(file("end.key")).unwrap(),
            )
            .unwrap();
        server.ticketer = aws_lc_rs::Ticketer::new().unwrap();
        let mut roots = RootCertStore::empty();
        roots
            .add(CertificateDer::from_pem_slice(file("ca.cert")).unwrap())
            .unwrap();
        let mut client = ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(versions(shape))
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        if !shape.resumes() {
            client.resumption = hyper_tls::client::Resumption::disabled();
        }
        Pair { client, server }
    }

    fn transfer(
        from: &mut impl std::ops::DerefMut<
            Target = hyper_tls::ConnectionCommon<impl hyper_tls::SideData>,
        >,
        to: &mut impl std::ops::DerefMut<Target = hyper_tls::ConnectionCommon<impl hyper_tls::SideData>>,
    ) {
        let mut buf = [0u8; 65536];
        while from.wants_write() {
            let n = from.write_tls(&mut &mut buf[..]).unwrap();
            let mut offs = 0;
            while offs < n {
                offs += to.read_tls(&mut io::Cursor::new(&buf[offs..n])).unwrap();
            }
        }
    }

    pub(super) fn handshake(pair: &mut Pair) -> (ClientConnection, ServerConnection) {
        let mut c =
            ClientConnection::new(&mut pair.client, "testserver.com".try_into().unwrap()).unwrap();
        let mut s = ServerConnection::new(&pair.server).unwrap();
        while c.is_handshaking() || s.is_handshaking() || c.wants_write() || s.wants_write() {
            transfer(&mut c, &mut s);
            s.process_new_packets(&mut pair.server).unwrap();
            transfer(&mut s, &mut c);
            c.process_new_packets(&mut pair.client).unwrap();
        }
        (c, s)
    }

    /// Moves `bytes` of plaintext client to server in records of `RECORD`, reading every byte.
    pub(super) fn bulk(pair: &mut Pair, bytes: usize) {
        let (mut c, mut s) = handshake(pair);
        let chunk = [0x5au8; RECORD];
        let mut sink = vec![0u8; RECORD];
        let mut moved = 0;
        while moved < bytes {
            c.writer().write_all(&chunk).unwrap();
            transfer(&mut c, &mut s);
            s.process_new_packets(&mut pair.server).unwrap();
            let mut got = 0;
            while got < RECORD {
                let n = s.reader().read(&mut sink).unwrap();
                assert!(n > 0, "the record arrived whole");
                got += n;
            }
            moved += RECORD;
        }
    }
}

/// Upstream rustls: configurations shared by `Arc`, as upstream has them.
mod theirs {
    use std::io::{self, Read, Write};
    use std::sync::Arc;

    use upstream_rustls::crypto::aws_lc_rs;
    use upstream_rustls::pki_types::pem::PemObject;
    use upstream_rustls::pki_types::{CertificateDer, PrivateKeyDer};
    use upstream_rustls::{
        ClientConfig, ClientConnection, RootCertStore, ServerConfig, ServerConnection,
    };

    use super::{file, Shape, RECORD};

    pub(super) struct Pair {
        client: Arc<ClientConfig>,
        server: Arc<ServerConfig>,
    }

    static TLS13: [&upstream_rustls::SupportedProtocolVersion; 1] =
        [&upstream_rustls::version::TLS13];
    static TLS12: [&upstream_rustls::SupportedProtocolVersion; 1] =
        [&upstream_rustls::version::TLS12];

    fn versions(shape: Shape) -> &'static [&'static upstream_rustls::SupportedProtocolVersion] {
        if shape.tls13() {
            &TLS13
        } else {
            &TLS12
        }
    }

    pub(super) fn pair(shape: Shape) -> Pair {
        let provider = Arc::new(aws_lc_rs::default_provider());
        let mut server = ServerConfig::builder_with_provider(provider.clone())
            .with_protocol_versions(versions(shape))
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                CertificateDer::pem_slice_iter(file("end.fullchain"))
                    .map(Result::unwrap)
                    .collect(),
                PrivateKeyDer::from_pem_slice(file("end.key")).unwrap(),
            )
            .unwrap();
        server.ticketer = aws_lc_rs::Ticketer::new().unwrap();
        let mut roots = RootCertStore::empty();
        roots
            .add(CertificateDer::from_pem_slice(file("ca.cert")).unwrap())
            .unwrap();
        let mut client = ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(versions(shape))
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        if !shape.resumes() {
            client.resumption = upstream_rustls::client::Resumption::disabled();
        }
        Pair {
            client: Arc::new(client),
            server: Arc::new(server),
        }
    }

    fn transfer(
        from: &mut impl std::ops::DerefMut<
            Target = upstream_rustls::ConnectionCommon<impl upstream_rustls::SideData>,
        >,
        to: &mut impl std::ops::DerefMut<
            Target = upstream_rustls::ConnectionCommon<impl upstream_rustls::SideData>,
        >,
    ) {
        let mut buf = [0u8; 65536];
        while from.wants_write() {
            let n = from.write_tls(&mut &mut buf[..]).unwrap();
            let mut offs = 0;
            while offs < n {
                offs += to.read_tls(&mut io::Cursor::new(&buf[offs..n])).unwrap();
            }
        }
    }

    pub(super) fn handshake(pair: &mut Pair) -> (ClientConnection, ServerConnection) {
        let mut c =
            ClientConnection::new(pair.client.clone(), "testserver.com".try_into().unwrap())
                .unwrap();
        let mut s = ServerConnection::new(pair.server.clone()).unwrap();
        while c.is_handshaking() || s.is_handshaking() || c.wants_write() || s.wants_write() {
            transfer(&mut c, &mut s);
            s.process_new_packets().unwrap();
            transfer(&mut s, &mut c);
            c.process_new_packets().unwrap();
        }
        (c, s)
    }

    pub(super) fn bulk(pair: &mut Pair, bytes: usize) {
        let (mut c, mut s) = handshake(pair);
        let chunk = [0x5au8; RECORD];
        let mut sink = vec![0u8; RECORD];
        let mut moved = 0;
        while moved < bytes {
            c.writer().write_all(&chunk).unwrap();
            transfer(&mut c, &mut s);
            s.process_new_packets().unwrap();
            let mut got = 0;
            while got < RECORD {
                let n = s.reader().read(&mut sink).unwrap();
                assert!(n > 0, "the record arrived whole");
                got += n;
            }
            moved += RECORD;
        }
    }
}

/// Which implementation a run is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Side {
    Ours,
    Theirs,
}

/// The process's account over a call: CPU nanoseconds, instructions and cycles (none where the OS does
/// not count them).
fn accounted(f: impl FnOnce()) -> Account {
    let before = hyper_measure::usage::this().unwrap();
    f();
    let spent = hyper_measure::usage::this().unwrap().since(&before);
    Account {
        cpu_ns: spent.user_ns + spent.system_ns,
        instructions: spent.instructions.unwrap_or(0),
        cycles: spent.cycles.unwrap_or(0),
        counted: spent.instructions.is_some(),
    }
}

/// What the process's account charged a run.
#[derive(Clone, Copy, Debug, Default)]
struct Account {
    cpu_ns: u64,
    instructions: u64,
    cycles: u64,
    counted: bool,
}

impl Account {
    fn add(&mut self, other: Account) {
        self.cpu_ns += other.cpu_ns;
        self.instructions += other.instructions;
        self.cycles += other.cycles;
        self.counted |= other.counted;
    }
}

/// Times `count` handshakes of `shape` on `side`, each on its own, after a first one that primes a
/// resumed shape's ticket (and warms the configuration), appending each handshake's nanoseconds; the
/// process's account of the timed handshakes, each connection's drop among them, and, apart, of the
/// configurations' building (not of the priming handshake).
fn handshakes(side: Side, shape: Shape, count: usize, out: &mut Vec<u64>) -> (Account, Account) {
    match side {
        Side::Ours => {
            let mut pair = None;
            let built = accounted(|| pair = Some(ours::pair(shape)));
            let mut pair = pair.unwrap();
            drop(ours::handshake(&mut pair));
            let timed = accounted(|| {
                for _ in 0..count {
                    let began = Instant::now();
                    let done = ours::handshake(&mut pair);
                    out.push(began.elapsed().as_nanos() as u64);
                    drop(done);
                }
            });
            (timed, built)
        }
        Side::Theirs => {
            let mut pair = None;
            let built = accounted(|| pair = Some(theirs::pair(shape)));
            let mut pair = pair.unwrap();
            drop(theirs::handshake(&mut pair));
            let timed = accounted(|| {
                for _ in 0..count {
                    let began = Instant::now();
                    let done = theirs::handshake(&mut pair);
                    out.push(began.elapsed().as_nanos() as u64);
                    drop(done);
                }
            });
            (timed, built)
        }
    }
}

/// One bulk round's nanoseconds a MiB on `side`.
fn bulk(side: Side) -> f64 {
    let began = Instant::now();
    match side {
        Side::Ours => ours::bulk(&mut ours::pair(Shape::Tls13Full), BULK_BYTES),
        Side::Theirs => theirs::bulk(&mut theirs::pair(Shape::Tls13Full), BULK_BYTES),
    }
    began.elapsed().as_nanos() as f64 / (BULK_BYTES >> 20) as f64
}

fn median(values: &mut [f64]) -> f64 {
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

fn main() {
    let rounds: usize = std::env::args()
        .skip(1)
        .find(|a| !a.starts_with('-'))
        .and_then(|a| a.parse().ok())
        .unwrap_or(5)
        .max(1);
    let total = quantile::samples_for(Level::P99, COVERAGE).unwrap() as usize;
    let per_round = total.div_ceil(rounds);
    println!(
        "hyper-tls against upstream rustls 0.23.45: {rounds} rounds, {} handshakes of each shape a side",
        per_round * rounds
    );
    let mut times: Vec<(Side, Shape, Vec<u64>, Account, Account)> = Vec::new();
    for side in [Side::Ours, Side::Theirs] {
        for shape in Shape::ALL {
            times.push((
                side,
                shape,
                Vec::new(),
                Account::default(),
                Account::default(),
            ));
        }
    }
    let mut bulk_ns: [Vec<f64>; 2] = [Vec::new(), Vec::new()];
    // The process's one-time costs first (the cryptography library's start, about 21 ms of CPU here,
    // which the first configuration built would otherwise carry), once on each side.
    drop(ours::handshake(&mut ours::pair(Shape::Tls13Full)));
    drop(theirs::handshake(&mut theirs::pair(Shape::Tls13Full)));
    for round in 0..rounds {
        let order = if round % 2 == 0 {
            [Side::Ours, Side::Theirs]
        } else {
            [Side::Theirs, Side::Ours]
        };
        for side in order {
            for (s, shape, out, account, building) in &mut times {
                if *s == side {
                    let (timed, built) = handshakes(side, *shape, per_round, out);
                    account.add(timed);
                    building.add(built);
                }
            }
            bulk_ns[usize::from(side == Side::Theirs)].push(bulk(side));
        }
    }
    let ms = |ns: u64| format!("{:.3}", ns as f64 / 1e6);
    println!();
    let mut building_lines: Vec<String> = Vec::new();
    println!("| shape | hyper-tls p50 [95%] | upstream p50 [95%] | hyper-tls p99 [95%] | upstream p99 [95%] | p50 hyper-tls/upstream | CPU µs, hyper-tls / upstream | instructions, hyper-tls / upstream |");
    println!("|---|---|---|---|---|---|---|---|");
    for shape in Shape::ALL {
        let pick = |side: Side| -> (Vec<u64>, Account, Account) {
            let (mut v, account, building) = times
                .iter()
                .find(|(s, sh, _, _, _)| *s == side && *sh == shape)
                .map(|(_, _, v, a, b)| (v.clone(), *a, *b))
                .unwrap();
            v.sort_unstable();
            (v, account, building)
        };
        let (ours, ours_account, ours_building) = pick(Side::Ours);
        let (theirs, theirs_account, theirs_building) = pick(Side::Theirs);
        building_lines.push(format!(
            "{}: building a client and a server configuration, hyper-tls {:.0} µs and {:.0} instructions, upstream {:.0} µs and {:.0}",
            shape.name(),
            ours_building.cpu_ns as f64 / rounds as f64 / 1e3,
            ours_building.instructions as f64 / rounds as f64,
            theirs_building.cpu_ns as f64 / rounds as f64 / 1e3,
            theirs_building.instructions as f64 / rounds as f64,
        ));
        let each = per_round * rounds;
        let per = |a: Account| {
            (
                a.cpu_ns as f64 / each as f64 / 1e3,
                a.instructions as f64 / each as f64,
            )
        };
        let (ours_cpu, ours_ins) = per(ours_account);
        let (theirs_cpu, theirs_ins) = per(theirs_account);
        let p50 = |v: &[u64]| {
            quantile::quantile(v, Level::P50, COVERAGE)
                .unwrap()
                .estimate
        };
        let instructions = if ours_account.counted {
            format!(
                "{ours_ins:.0} / {theirs_ins:.0} ({:.3})",
                ours_ins / theirs_ins
            )
        } else {
            "not counted here".to_owned()
        };
        println!(
            "| {} | {} | {} | {} | {} | {:.3} | {ours_cpu:.1} / {theirs_cpu:.1} ({:.3}) | {instructions} |",
            shape.name(),
            quantile::cell(&ours, Level::P50, COVERAGE, ms),
            quantile::cell(&theirs, Level::P50, COVERAGE, ms),
            quantile::cell(&ours, Level::P99, COVERAGE, ms),
            quantile::cell(&theirs, Level::P99, COVERAGE, ms),
            p50(&ours) as f64 / p50(&theirs) as f64,
            ours_cpu / theirs_cpu,
        );
    }
    println!();
    for line in &building_lines {
        println!("{line}");
    }
    let ours = median(&mut bulk_ns[0]);
    let theirs = median(&mut bulk_ns[1]);
    println!();
    println!(
        "bulk, TLS 1.3, 16 KiB records, client to server: hyper-tls {:.3} ms a MiB, upstream {:.3} ms a MiB ({:.3}), medians of {rounds} rounds of {} MiB",
        ours / 1e6,
        theirs / 1e6,
        ours / theirs,
        BULK_BYTES >> 20
    );
}
