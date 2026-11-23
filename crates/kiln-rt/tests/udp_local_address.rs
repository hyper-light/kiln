//! A datagram's local address (`Arrival::to`) and an answer sent from it (`Batched::send_segments_from`):
//! a socket bound to the unspecified address on a host with more than one address answers from the
//! address its peer reached, where the kernel's own choice is its route's source. kiln goal 90 R2
//! (2026-11-21): kiln's server on adacachy held 192.168.1.77 beside its advertised 192.168.1.90 and
//! answered every runner from .77, which a QUIC client does not accept from a server (RFC 9000 §9).
//! Linux gives loopback all of 127/8, so 127.0.0.2 is a second address whose route's source is 127.0.0.1;
//! macOS's loopback holds 127.0.0.1 alone, where the answer's source is checked only to go out and arrive.

// Test harness code: a panic here is a failed test (CLAUDE.md §1).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    clippy::missing_panics_doc
)]
#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::net::IpAddr;
use std::time::Duration;

use hyper_rt::RtError;
use hyper_rt::runtime::{LocalRuntime, RuntimeConfig};
use hyper_rt::udp::{Batched, Io, Ipv4Addr, SocketAddr};

fn config() -> RuntimeConfig {
    RuntimeConfig {
        shards: 1,
        tasks_per_shard: 64,
        timers_per_shard: 64,
        interests_per_shard: 64,
        ring_entries: 64,
        step_budget_ns: 1_000_000_000,
        timer_tick_ns: 100_000,
        batch: 64,
        pin: false,
        cores: Vec::new(),
        page_bytes: 4096,
        spin_ns: 0,
        wake_tracking: None,
        latency_objective_ns: None,
    }
}

/// A batched socket bound to the unspecified address is sent a datagram at `reached` and answers it,
/// from the datagram's local address when `from_it`, else as the kernel chooses. Returns the local address
/// the arrival told and the address the answer came from.
fn answer(reached: Ipv4Addr, from_it: bool, connect: bool) -> (Option<IpAddr>, SocketAddr) {
    let mut rt = LocalRuntime::new(&config()).unwrap();
    rt.block_on(async move {
        let mut server = Batched::bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)), Io { batch: 4 }, false)?;
        let port = server.local_addr()?.port();
        let client = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        client.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        client.send_to(b"ping", (reached, port)).unwrap();
        let mut arrived = None;
        while arrived.is_none() {
            let taken = server.receive(|arrival, _| arrived = Some(arrival))?;
            if taken == 0 {
                server.readable().await?;
            }
        }
        let arrival = arrived.unwrap();
        if connect { server.connect(arrival.from)?; }
        let source = if from_it { arrival.to } else { None };
        while server.send_segments_from(arrival.from, source, b"pong", 4) == 0 {
            server.writable().await?;
        }
        assert_eq!(server.stats().send_errors, 0);
        let mut buf = [0u8; 16];
        let (length, from) = client.recv_from(&mut buf).unwrap();
        assert_eq!(&buf[..length], b"pong");
        Ok::<_, RtError>((arrival.to, from))
    })
    .unwrap()
    .unwrap()
}

/// Do: reach the socket at 127.0.0.1 and answer from the arrival's local address. Expect: the arrival
/// tells 127.0.0.1, and the answer (sent with the address as its source) arrives from it.
#[test]
fn an_arrival_tells_its_local_address_and_an_answer_from_it_arrives() {
    let (to, from) = answer(Ipv4Addr::LOCALHOST, true, false);
    assert_eq!(to, Some(IpAddr::V4(Ipv4Addr::LOCALHOST)));
    assert_eq!(from.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
}

/// Do: reach the socket at 127.0.0.2, its host's second address, and answer as the kernel chooses, then
/// from the arrival's local address. Expect: the kernel's choice is its route's source, 127.0.0.1 (what
/// adacachy did, the fault itself), and the answer from the local address comes from 127.0.0.2.
#[cfg(target_os = "linux")]
#[test]
fn reached_at_a_second_address_it_answers_from_that_address() {
    let second = Ipv4Addr::new(127, 0, 0, 2);
    let (to, kernels) = answer(second, false, false);
    assert_eq!(to, Some(IpAddr::V4(second)), "the arrival tells the address it reached");
    assert_eq!(kernels.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST), "the kernel answers from its route's source");
    let (_, from) = answer(second, true, false);
    assert_eq!(from.ip(), IpAddr::V4(second), "answered from the address it was reached at");
}

/// CP-T3.2. The session uplink promotes a handshaking socket to a connected peer socket.
#[test]
fn a_connected_peer_socket_can_answer_from_the_reached_address() {
    let (to, from) = answer(Ipv4Addr::LOCALHOST, true, true);
    assert_eq!(to, Some(from.ip()));
}

/// CP-T3.2. The largest discovered datagram must leave room for the source address itself:
/// a permanently blocked datagram would hold all subsequent QUIC control frames.
#[cfg(target_os = "macos")]
#[test]
fn the_path_ceiling_fits_the_source_address_on_a_connected_socket() {
    let mut rt = LocalRuntime::new(&config()).unwrap();
    rt.block_on(async {
        let client = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        client.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let remote = client.local_addr().unwrap();
        let mut batched = Batched::bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)), Io { batch: 1 }, false)?;
        batched.connect(remote)?;
        let room = usize::from(batched.socket().path_payload_to(remote)?.unwrap());
        let local = batched.socket().local_addr()?.ip();
        let data = vec![7; room];
        assert_eq!(batched.send_segments_from(remote, Some(local), &data, room), 1);
        assert_eq!(batched.stats().send_errors, 0);
        let mut buf = vec![0; room];
        assert_eq!(client.recv(&mut buf).unwrap(), room);
        assert_eq!(buf, data);
        Ok::<_, RtError>(())
    }).unwrap().unwrap();
}
