//! TCP on every OS (docs/runtime.md §5.2): IPv6 with vectored reads into the caller's buffers and
//! vectored writes; the stream options read back from the OS; the process-wide connection budget closes
//! a connection past it, counts it, and takes connections again once a slot returns; shared listeners
//! bind one port where the OS spreads connections among them; congestion control, the kernel buffers and
//! the connection's own reading (`tcp_info`) where each platform offers them (bp-g3 step 1).

// Test harness code: a panic here is a failed test (CLAUDE.md §1).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    clippy::missing_panics_doc,
    clippy::cognitive_complexity
)]

use std::io::{IoSlice, IoSliceMut};
use std::sync::mpsc::channel;

use hyper_rt::runtime::{LocalRuntime, RuntimeConfig};
use hyper_rt::tcp::{
    CONGESTION_NAME_MAX, CongestionName, CongestionRefusal, ConnectionBudget, Ipv4Addr, Ipv6Addr,
    Shutdown, SocketAddr, TcpListener, TcpStream,
};

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

/// Shape: the pending connections a test listener queues; the tests make at most three.
const BACKLOG: u32 = 4;
/// Shape: how many 1 ms sleeps a test task waits for another task's answer before it fails: a step
/// takes microseconds, so a second is generous and still ends a broken run.
const ANSWER_SLEEPS: u32 = 1_000;
/// Shape: one sleep of that wait, nanoseconds.
const SLEEP_NS: u64 = 1_000_000;

/// Do: over IPv6 loopback, a client writes eight bytes from two slices in one call and reads the echo
/// into two buffers of its own in one call. Expect: the server saw the bytes in order, and the reply
/// landed split across the client's buffers.
#[test]
fn an_ipv6_stream_reads_and_writes_vectored_into_the_callers_buffers() {
    let mut rt = LocalRuntime::new(&config()).unwrap();
    rt.block_on(async {
        let listener =
            TcpListener::bind(SocketAddr::from((Ipv6Addr::LOCALHOST, 0)), BACKLOG).unwrap();
        let addr = listener.local_addr().unwrap();
        assert!(addr.is_ipv6());
        hyper_rt::futures::spawn(async move {
            let stream = listener.accept().await.unwrap();
            assert!(stream.peer_addr().unwrap().is_ipv6());
            let mut request = [0u8; 8];
            let mut got = 0;
            while got < request.len() {
                got += stream.read(&mut request[got..]).await.unwrap();
            }
            stream.write_all(&request).await.unwrap();
            stream.shutdown(Shutdown::Write).unwrap();
        })
        .unwrap();
        let client = TcpStream::connect(addr).await.unwrap();
        let sent = client
            .write_vectored(&[IoSlice::new(b"abc"), IoSlice::new(b"defgh")])
            .await
            .unwrap();
        assert_eq!(sent, 8, "one call took both slices");
        let (mut head, mut tail) = ([0u8; 3], [0u8; 5]);
        let mut got = 0;
        while got < 8 {
            let n = {
                let (head_rest, tail_rest) = if got < 3 {
                    (&mut head[got..], &mut tail[..])
                } else {
                    (&mut head[3..], &mut tail[got - 3..])
                };
                let mut bufs = [IoSliceMut::new(head_rest), IoSliceMut::new(tail_rest)];
                client.read_vectored(&mut bufs).await.unwrap()
            };
            assert_ne!(n, 0, "the echo ended early");
            got += n;
        }
        assert_eq!(&head, b"abc");
        assert_eq!(&tail, b"defgh");
        let mut end = [0u8; 1];
        assert_eq!(client.read(&mut end).await.unwrap(), 0, "end of stream");
    })
    .unwrap();
}

/// Do: set the options on a connected stream and read them back from the OS. Expect: `TCP_NODELAY`
/// always; `TCP_NOTSENT_LOWAT` on Linux and macOS (macOS's value is declared in the crate, so a wrong one
/// fails here); the retransmission deadline on all three (macOS keeps whole seconds).
#[test]
fn the_stream_options_read_back() {
    /// Shape: a low-water mark, one write's worth.
    const LOWAT: u32 = 16_384;
    /// Shape: a deadline in whole seconds, so macOS's rounding keeps it exactly.
    const DEADLINE_MS: u32 = 5_000;
    let mut rt = LocalRuntime::new(&config()).unwrap();
    rt.block_on(async {
        let listener =
            TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)), BACKLOG).unwrap();
        let addr = listener.local_addr().unwrap();
        let client = TcpStream::connect(addr).await.unwrap();
        let server = listener.accept().await.unwrap();
        for stream in [&client, &server] {
            assert!(stream.nodelay().unwrap(), "TCP_NODELAY");
            let offered = stream.set_notsent_lowat(LOWAT).unwrap();
            assert_eq!(offered, cfg!(any(target_os = "linux", target_os = "macos")));
            if offered {
                assert_eq!(stream.notsent_lowat().unwrap(), Some(LOWAT));
            }
            assert!(stream.set_user_timeout(DEADLINE_MS).unwrap());
            assert_eq!(stream.user_timeout_ms().unwrap(), Some(DEADLINE_MS));
        }
    })
    .unwrap();
}

/// Do: a budget of one; a first connection accepted, a second connected, the listener asked for the
/// next. Expect: the second is closed at once (its read ends) and counted; once the first stream drops,
/// its slot returns and a third connection is accepted under the budget.
#[test]
fn a_connection_past_the_budget_is_closed_and_counted_until_a_slot_returns() {
    let mut rt = LocalRuntime::new(&config()).unwrap();
    rt.block_on(async {
        let budget = ConnectionBudget::new(1).unwrap();
        let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)), BACKLOG)
            .unwrap()
            .with_budget(budget.clone());
        let addr = listener.local_addr().unwrap();
        let _first_client = TcpStream::connect(addr).await.unwrap();
        let first = listener.accept().await.unwrap();
        assert_eq!(budget.open(), 1);
        let second = TcpStream::connect(addr).await.unwrap();
        let (answer, answered) = channel();
        hyper_rt::futures::spawn(async move {
            let third = listener.accept().await.map(|_| ());
            let _ = answer.send((third, listener.refused()));
        })
        .unwrap();
        let mut byte = [0u8; 1];
        let ended = second.read(&mut byte).await;
        assert!(
            matches!(ended, Ok(0) | Err(_)),
            "the refused connection ends: {ended:?}"
        );
        drop(first);
        assert_eq!(budget.open(), 0, "the slot returned with its stream");
        let _third_client = TcpStream::connect(addr).await.unwrap();
        let mut waited = 0;
        let (third, refused) = loop {
            if let Ok(answer) = answered.try_recv() {
                break answer;
            }
            assert!(
                waited < ANSWER_SLEEPS,
                "the third connection was not accepted"
            );
            waited += 1;
            hyper_rt::futures::sleep(SLEEP_NS).await.unwrap();
        };
        third.unwrap();
        assert_eq!(refused, 1, "the second connection was refused, once");
    })
    .unwrap();
}

/// Do: two shared listeners on one port, then a plain one. Expect: the shared ones both bind
/// (`SO_REUSEPORT`), and the plain one is refused the port.
#[cfg(unix)]
#[test]
fn shared_listeners_bind_one_port() {
    let mut rt = LocalRuntime::new(&config()).unwrap();
    rt.block_on(async {
        let first =
            TcpListener::bind_shared(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)), BACKLOG).unwrap();
        let addr = first.local_addr().unwrap();
        let second = TcpListener::bind_shared(addr, BACKLOG);
        assert!(second.is_ok(), "a second shared listener binds: {second:?}");
        assert!(
            TcpListener::bind(addr, BACKLOG).is_err(),
            "a plain one does not"
        );
    })
    .unwrap();
}

/// A congestion control by name (the tests' names are valid ones).
fn cc(name: &str) -> CongestionName {
    CongestionName::new(name).unwrap()
}

/// The algorithms one of Linux's congestion-control lists names (`/proc/sys/net/ipv4/<list>`).
#[cfg(target_os = "linux")]
fn listed(list: &str) -> Vec<String> {
    std::fs::read_to_string(format!("/proc/sys/net/ipv4/{list}"))
        .unwrap_or_default()
        .split_whitespace()
        .map(str::to_owned)
        .collect()
}

/// Do: name congestion controls the kernel can't take. Expect: none is a name: empty, longer than
/// `TCP_CA_NAME_MAX` less its NUL, or with a NUL or a space in it (bp-g3 step 1).
#[test]
fn bp_g3_s1_a_congestion_name_is_what_the_kernel_keeps() {
    assert_eq!(cc("cubic").as_str(), "cubic");
    assert_eq!(cc(&"x".repeat(CONGESTION_NAME_MAX)).as_str().len(), CONGESTION_NAME_MAX);
    for bad in ["", "a-name-sixteen-b", "nul\0", "two words"] {
        assert!(CongestionName::new(bad).is_none(), "{bad:?} is no name");
    }
}

/// Do: on a loopback pair and its listener, pick reno (always allowed on Linux), then an algorithm that
/// doesn't exist, then bbr. Expect: reno reads back on the stream and on what the listener accepts; the
/// missing one is refused as not loaded and the stream keeps reno; bbr is taken where the host allows it
/// and refused, by name and reason, where it doesn't. Where there is no per-socket choice (macOS,
/// Windows), each call says it isn't offered (bp-g3 step 1).
#[test]
fn bp_g3_s1_congestion_control_is_picked_read_back_and_refused_by_name() {
    let mut rt = LocalRuntime::new(&config()).unwrap();
    rt.block_on(async {
        let listener =
            TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)), BACKLOG).unwrap();
        let offered = listener.set_congestion(cc("reno")).unwrap();
        assert_eq!(offered, cfg!(target_os = "linux"));
        let addr = listener.local_addr().unwrap();
        let client = TcpStream::connect(addr).await.unwrap();
        let server = listener.accept().await.unwrap();
        if !offered {
            assert_eq!(listener.congestion().unwrap(), None);
            assert!(!client.set_congestion(cc("reno")).unwrap(), "not offered");
            assert_eq!(client.congestion().unwrap(), None);
            return;
        }
        assert_eq!(listener.congestion().unwrap(), Some(cc("reno")));
        assert_eq!(server.congestion().unwrap(), Some(cc("reno")), "accepted inherits");
        assert!(client.set_congestion(cc("reno")).unwrap());
        assert_eq!(client.congestion().unwrap(), Some(cc("reno")));

        let missing = client.set_congestion(cc("no-such-cc")).unwrap_err();
        assert_eq!(missing.algorithm, cc("no-such-cc"));
        assert_eq!(missing.reason, CongestionRefusal::NotLoaded);
        assert!(missing.to_string().contains("no-such-cc"), "{missing}");
        assert!(missing.to_string().contains("not loaded"), "{missing}");
        assert_eq!(client.congestion().unwrap(), Some(cc("reno")), "kept its own");

        #[cfg(target_os = "linux")]
        {
            let privileged = rustix::process::geteuid().is_root();
            let allowed = listed("tcp_allowed_congestion_control").contains(&"bbr".to_owned());
            let loaded = listed("tcp_available_congestion_control").contains(&"bbr".to_owned());
            match client.set_congestion(cc("bbr")) {
                Ok(true) => {
                    assert!(allowed || privileged, "bbr taken though not allowed");
                    assert_eq!(client.congestion().unwrap(), Some(cc("bbr")));
                }
                Ok(false) => panic!("Linux offers TCP_CONGESTION"),
                Err(refusal) => {
                    assert!(!allowed, "bbr is allowed here, yet refused: {refusal}");
                    let want = if loaded {
                        CongestionRefusal::NotAllowed
                    } else {
                        CongestionRefusal::NotLoaded
                    };
                    assert_eq!(refusal.reason, want, "{refusal}");
                    assert!(refusal.to_string().contains("bbr"), "{refusal}");
                    assert_eq!(client.congestion().unwrap(), Some(cc("reno")), "kept its own");
                }
            }
        }

        // The stream still carries bytes after its refusals.
        client.write_all(b"ok").await.unwrap();
        let mut got = [0u8; 2];
        let mut n = 0;
        while n < got.len() {
            n += server.read(&mut got[n..]).await.unwrap();
        }
        assert_eq!(&got, b"ok");
    })
    .unwrap();
}

/// Do: set the send and receive buffers on both ends of a loopback pair and read them back. Expect: every
/// platform offers both; Linux reads back twice what was set (its bookkeeping, socket(7)), macOS and
/// Windows what was set (bp-g3 step 1).
#[test]
fn bp_g3_s1_the_kernel_buffers_read_back() {
    /// Shape: a buffer size every platform takes as asked.
    const BUFFER: u32 = 64 << 10;
    let want = if cfg!(target_os = "linux") { 2 * BUFFER } else { BUFFER };
    let mut rt = LocalRuntime::new(&config()).unwrap();
    rt.block_on(async move {
        let listener =
            TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)), BACKLOG).unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).await.unwrap();
        let server = listener.accept().await.unwrap();
        for stream in [&client, &server] {
            assert!(stream.set_send_buffer(BUFFER).unwrap());
            assert_eq!(stream.send_buffer().unwrap(), Some(want));
            assert!(stream.set_recv_buffer(BUFFER).unwrap());
            assert_eq!(stream.recv_buffer().unwrap(), Some(want));
        }
    })
    .unwrap();
}

/// Do: a writer with a 16 KiB low-water mark writes 64 KiB at a time to a peer that never reads, until no
/// write has gone in for 200 ms, reading `tcp_info` after every try. Expect: where the platform counts
/// unsent bytes (Linux), they rise above zero once the peer's window closes and never pass the mark plus
/// one write; every platform's reading gives what it says it gives (Linux all of it, macOS the RTT and
/// window, Windows RTTs and in-flight bytes) (bp-g3 step 1).
#[test]
fn bp_g3_s1_tcp_info_sees_unsent_bytes_held_at_the_mark() {
    /// Shape: the writer's low-water mark.
    const LOWAT: u32 = 16 << 10;
    /// Shape: one write; at least the kernel's largest segment (64 KiB), which a write may fill past the
    /// mark without the check the next segment gets.
    const WRITE: usize = 64 << 10;
    /// Shape: the most a test writes before it calls the peer's buffers full, far past any kernel's.
    const CAP: usize = 256 << 20;
    /// Shape: 1 ms tries with nothing taken before the writer counts as stalled.
    const STALL_TRIES: u32 = 200;
    let mut rt = LocalRuntime::new(&config()).unwrap();
    rt.block_on(async {
        let listener =
            TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)), BACKLOG).unwrap();
        let writer = TcpStream::connect(listener.local_addr().unwrap()).await.unwrap();
        let _reader = listener.accept().await.unwrap();
        let lowat = writer.set_notsent_lowat(LOWAT).unwrap();
        let chunk = vec![7u8; WRITE];
        let (mut written, mut idle, mut most_unsent, mut samples) = (0usize, 0u32, 0u32, 0u32);
        let mut last = None;
        while idle < STALL_TRIES && written < CAP {
            match writer.try_write(&chunk).unwrap() {
                Some(n) => {
                    written += n;
                    idle = 0;
                }
                None => {
                    idle += 1;
                    hyper_rt::futures::sleep(SLEEP_NS).await.unwrap();
                }
            }
            let info = writer.tcp_info().unwrap();
            if let Some(unsent) = info.and_then(|i| i.unsent_bytes) {
                samples += 1;
                most_unsent = most_unsent.max(unsent);
                if lowat {
                    assert!(
                        usize::try_from(unsent).unwrap() <= usize::try_from(LOWAT).unwrap() + WRITE,
                        "unsent {unsent} past the mark {LOWAT} plus a write {WRITE}"
                    );
                }
            }
            last = info;
        }
        assert!(written < CAP, "the peer's buffers never filled");
        let info = last.expect("a reading");
        #[cfg(target_os = "linux")]
        {
            assert!(samples > 0);
            assert!(most_unsent > 0, "unsent bytes rose once the window closed");
            assert!(info.in_flight_bytes.is_some() && info.min_rtt_us.is_some());
            assert!(info.srtt_us.is_some() && info.delivery_rate.is_some());
            assert!(info.cwnd_bytes.is_some_and(|c| c > 0) && info.mss.is_some_and(|m| m > 0));
            assert!(info.retransmits.is_some() && info.retransmitted_bytes.is_some());
            assert!(info.congestion.is_some(), "the algorithm in use");
            eprintln!(
                "bp_g3_s1: wrote {written} B; unsent at most {most_unsent} B, last {:?}; {info:?}",
                info.unsent_bytes
            );
        }
        #[cfg(target_os = "macos")]
        {
            assert_eq!(samples, 0, "macOS doesn't count unsent bytes");
            assert!(info.srtt_us.is_some() && info.cwnd_bytes.is_some() && info.mss.is_some());
        }
        #[cfg(windows)]
        {
            assert_eq!(samples, 0, "Windows doesn't count unsent bytes");
            assert!(info.in_flight_bytes.is_some() && info.srtt_us.is_some());
            assert!(info.min_rtt_us.is_some() && info.mss.is_some_and(|m| m > 0));
        }
        let _ = (most_unsent, samples, info);
    })
    .unwrap();
}

/// Do: take a stream's options handle, set its low-water mark and buffers through it, drop the handle,
/// and use the stream. Expect: what the handle set reads back on the stream itself, its reading is the
/// stream's, and dropping it leaves the connection open; Windows makes no handle (bp-g3 step 1).
#[test]
fn bp_g3_s1_an_options_handle_sets_the_streams_own_socket() {
    /// Shape: a low-water mark.
    const LOWAT: u32 = 8_192;
    let mut rt = LocalRuntime::new(&config()).unwrap();
    rt.block_on(async {
        let listener =
            TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)), BACKLOG).unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).await.unwrap();
        let server = listener.accept().await.unwrap();
        let handle = server.options().unwrap();
        assert_eq!(handle.is_some(), cfg!(unix), "Unix makes a handle, Windows none");
        let Some(handle) = handle else { return };
        if handle.set_notsent_lowat(LOWAT).unwrap() {
            assert_eq!(server.notsent_lowat().unwrap(), Some(LOWAT), "the stream's own socket");
        }
        assert!(handle.set_recv_buffer(32 << 10).unwrap());
        assert_eq!(handle.recv_buffer().unwrap(), server.recv_buffer().unwrap());
        assert_eq!(handle.congestion().unwrap(), server.congestion().unwrap());
        assert!(handle.tcp_info().unwrap().is_some());
        drop(handle);
        client.write_all(b"still").await.unwrap();
        let mut got = [0u8; 5];
        let mut n = 0;
        while n < got.len() {
            n += server.read(&mut got[n..]).await.unwrap();
        }
        assert_eq!(&got, b"still");
    })
    .unwrap();
}
