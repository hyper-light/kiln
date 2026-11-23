//! AD-1. Kernel receive evidence from a real Windows UDP driver.
//! Set KILN_TEST_STAMP_BIND to a local address and KILN_TEST_STAMP_ECHO to a peer UDP echo
//! service when local delivery bypasses the enabled adapter. Run explicitly on a prepared runner.
#![cfg(windows)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::disallowed_macros,
    clippy::disallowed_methods
)]

use hyper_rt::futures::{sleep, within};
use hyper_rt::runtime::{LocalRuntime, RuntimeConfig};
use hyper_rt::udp::{Batched, Io, Ring, Sent};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

fn now() -> u64 {
    hyper_rt::registry::with_current(|ctx| ctx.now_ns()).unwrap()
}

#[test]
#[ignore = "requires an enabled Windows receive timestamp adapter and a UDP echo path through it"]
fn windows_kernel_stamps_a_real_udp_datagram() {
    let bound = std::env::var("KILN_TEST_STAMP_BIND")
        .ok()
        .map(|s| {
            s.parse::<IpAddr>()
                .expect("KILN_TEST_STAMP_BIND must be a local IP")
        })
        .unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST));
    let peer = std::env::var("KILN_TEST_STAMP_ECHO").ok().map(|s| {
        s.parse::<SocketAddr>()
            .expect("KILN_TEST_STAMP_ECHO must be an IP:port")
    });
    let config = RuntimeConfig {
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
    };
    let mut rt = LocalRuntime::new(&config).unwrap();
    rt.block_on(async move {
        // Exercise the same lent UDP buffers as the production QUIC transport.
        let mut rx = Batched::bind_lent(SocketAddr::new(bound, 0), true).unwrap();
        assert!(
            rx.stats().kernel_stamps,
            "Windows reported no active, usable timestamp clock for {bound}"
        );
        let mut ring = Ring::new(Io { batch: 1 }).unwrap();
        let local = rx.local_addr().unwrap();
        let peer = peer.unwrap_or(local);
        let before = now();
        let body = before.to_ne_bytes();
        let (queued, observed) = within(10_000_000_000, async {
            assert!(rx.queue(peer, &body));
            while rx.send() == Sent::Blocked {
                rx.writable().await.unwrap();
            }
            while rx.socket().queued_bytes().unwrap() == 0 {
                rx.readable().await.unwrap();
            }
            (now(), std::time::Instant::now())
        })
        .await
        .unwrap()
        .expect("no UDP echo reached the bound interface");
        // Deliberately leave kernel data queued so a read-time fallback is distinguishable.
        sleep(10_000_000).await.unwrap();
        let held = observed.elapsed();
        let mut got = None;
        let n = rx
            .receive_with(&mut ring, |arrival, packet| {
                assert_eq!(packet, &body);
                assert_eq!(arrival.from, peer);
                assert_eq!(arrival.to, Some(bound));
                got = Some(arrival);
            })
            .unwrap();
        assert_eq!(n, 1);
        let arrival = got.unwrap();
        assert!(
            arrival.kernel,
            "Winsock returned no convertible SO_TIMESTAMP for the packet from {peer}"
        );
        assert_eq!(arrival.kernel_at_ns, Some(arrival.at_ns));
        assert!(
            before <= arrival.at_ns
                && arrival.at_ns <= arrival.read_ns
                && queued < arrival.read_ns
                && u128::from(arrival.read_ns.saturating_sub(arrival.at_ns)).saturating_add(1)
                    >= held.as_nanos(),
            "send {before}, kernel {}, queued {queued}, held {held:?}, read {}",
            arrival.at_ns,
            arrival.read_ns
        );
    })
    .unwrap();
}
