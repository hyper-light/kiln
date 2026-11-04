//! What re-arming a one-shot readiness registration costs, against what it serves (docs/runtime.md §15
//! item 7, the registration's half): `cargo bench -p hyper-rt --bench rearm`.
//!
//! hyper-rt arms each readiness wait one-shot (`EPOLLONESHOT`, kqueue `EV_ONESHOT`, an AFD poll on
//! Windows), so a wait that finds its socket empty arms the socket again before the shard parks: one
//! `epoll_ctl(MOD)` on Linux, one `kevent` change a direction on macOS, where a persistent, edge-triggered
//! registration (`EPOLLET`, `EV_CLEAR`) would make none. §3.7 asks for the two to be measured against each
//! other on §12's rows before the choice is fixed; first, what the re-arm is worth on the row it would
//! serve most, a UDP echo:
//!
//! - **The call alone**: the re-arm made `CALLS` times back to back on a bound UDP socket, nanoseconds a
//!   call, the median of `ROUNDS` rounds, beside `getppid`, the cheapest system call, for scale.
//! - **A UDP echo through one shard**: `clients` plain threads each send a datagram to a task of their own
//!   on the shard and block for its answer, one at a time; round trips a second, and the shard thread's CPU
//!   a round trip from its own clock (`registry::shard_cpu`). Each task receives as `UdpSocket::recv_from`
//!   does (take, else await readability) and counts the takes that found nothing, each of which armed its
//!   socket once: the re-arms a round trip, exactly. The process's instructions a round trip
//!   (`hyper_measure::usage`, where the OS counts them) are read over the same round trips, the
//!   clients' beside the shard's, so two builds differ in it by what their shards do.
//!
//! The re-arms a round trip at the call's cost, over the shard's CPU a round trip, bound what an
//! edge-triggered registration could take off the row. Windows has no persistent registration to set
//! beside its AFD poll, which is one request a wait; the echo runs there for its numbers alone.
#![allow(
    missing_docs,
    unsafe_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    clippy::arithmetic_side_effects,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::indexing_slicing
)]

use std::net::{SocketAddr, UdpSocket as StdUdp};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use hyper_rt::machine::calibration::{Calibration, Policy};
use hyper_rt::registry;
use hyper_rt::runtime::{Runtime, RuntimeConfig};
use hyper_rt::sync::channel;
use hyper_rt::udp::UdpSocket;

/// Re-arms a round of the call alone.
const CALLS: u32 = 100_000;
/// Rounds of each call, alternating, whose median is reported.
const ROUNDS: usize = 9;
/// Round trips each client makes unmeasured, then measured.
const WARM: usize = 2_000;
const TRIPS: usize = 20_000;

/// Takes that found the socket empty, over every task: each armed its socket once.
static ARMS: AtomicU64 = AtomicU64::new(0);

/// Nanoseconds a call of `f`, the median over `ROUNDS` rounds of `CALLS` calls, beside `g`'s, the rounds
/// of the two alternating so the host's load moves both alike.
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "freebsd"))]
fn rounds(mut f: impl FnMut(), mut g: impl FnMut()) -> (f64, f64) {
    let mut fs = Vec::with_capacity(ROUNDS);
    let mut gs = Vec::with_capacity(ROUNDS);
    for _ in 0..ROUNDS {
        let started = Instant::now();
        for _ in 0..CALLS {
            f();
        }
        fs.push(started.elapsed().as_nanos() as f64 / f64::from(CALLS));
        let started = Instant::now();
        for _ in 0..CALLS {
            g();
        }
        gs.push(started.elapsed().as_nanos() as f64 / f64::from(CALLS));
    }
    fs.sort_by(f64::total_cmp);
    gs.sort_by(f64::total_cmp);
    (fs[ROUNDS / 2], gs[ROUNDS / 2])
}

/// The cheapest system call, for scale.
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "freebsd"))]
fn getppid() {
    std::hint::black_box(rustix::process::getppid());
}

/// The re-arm alone, and `getppid`: `epoll_ctl(MOD)` of `EPOLLIN | EPOLLONESHOT`, as the epoll driver arms.
#[cfg(target_os = "linux")]
fn rearm_alone() -> Option<(f64, f64)> {
    use rustix::event::epoll;
    let ep = epoll::create(epoll::CreateFlags::CLOEXEC).unwrap();
    let socket = StdUdp::bind("127.0.0.1:0").unwrap();
    let data = epoll::EventData::new_u64(1);
    let flags = epoll::EventFlags::IN | epoll::EventFlags::ONESHOT;
    epoll::add(&ep, &socket, data, flags).unwrap();
    Some(rounds(
        || epoll::modify(&ep, &socket, data, flags).unwrap(),
        getppid,
    ))
}

/// The re-arm alone, and `getppid`: one `kevent` change of `EVFILT_READ` with `EV_ADD | EV_ONESHOT`, as the
/// kqueue driver arms a direction.
#[cfg(any(target_os = "macos", target_os = "freebsd"))]
fn rearm_alone() -> Option<(f64, f64)> {
    use rustix::event::kqueue::{Event, EventFilter, EventFlags, kevent, kqueue};
    use std::os::fd::AsRawFd;
    let kq = kqueue().unwrap();
    let socket = StdUdp::bind("127.0.0.1:0").unwrap();
    let change = Event::new(
        EventFilter::Read(socket.as_raw_fd()),
        EventFlags::ADD | EventFlags::ONESHOT,
        std::ptr::null_mut(),
    );
    let mut none: Vec<Event> = Vec::new();
    Some(rounds(
        || {
            // SAFETY: one valid change record on the open queue, its udata null and never read; the
            // output buffer receives nothing.
            unsafe { kevent(&kq, &[change], &mut none, None) }.unwrap();
        },
        getppid,
    ))
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "freebsd")))]
fn rearm_alone() -> Option<(f64, f64)> {
    None
}

struct Echo {
    trips: usize,
    wall: Duration,
    shard_cpu_ns: u64,
    arms: u64,
    instructions: Option<u64>,
}

/// `clients` threads, each echoing `TRIPS` datagrams of 8 bytes through a task of its own on one shard.
fn echo(config: &RuntimeConfig, clients: usize) -> Echo {
    let runtime = Runtime::start(config).unwrap();
    let shard = runtime.shard_ids()[0];
    let holder = runtime.holder_of(shard).unwrap();
    let (tell, mut told) = channel::<SocketAddr>(clients).unwrap();
    for _ in 0..clients {
        let tell = tell.clone();
        runtime
            .spawn_on(shard, async move {
                let socket = UdpSocket::bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
                tell.try_send(socket.local_addr().unwrap()).unwrap();
                drop(tell);
                let mut buf = [0u8; 64];
                loop {
                    let (n, from) = match socket.try_recv_from(&mut buf).unwrap() {
                        Some(received) => received,
                        None => {
                            ARMS.fetch_add(1, Ordering::Relaxed);
                            socket.readable().await.unwrap();
                            continue;
                        }
                    };
                    socket.send_to(&buf[..n], from).unwrap();
                }
            })
            .unwrap();
    }
    drop(tell);
    let servers: Vec<SocketAddr> = (0..clients)
        .map(|_| told.blocking_recv().unwrap())
        .collect();
    let sockets: Vec<StdUdp> = servers
        .iter()
        .map(|server| {
            let socket = StdUdp::bind("127.0.0.1:0").unwrap();
            socket.connect(server).unwrap();
            socket
        })
        .collect();
    let trips = |count: usize| {
        std::thread::scope(|scope| {
            for socket in &sockets {
                scope.spawn(move || {
                    let mut buf = [0u8; 64];
                    for i in 0..count as u64 {
                        socket.send(&i.to_ne_bytes()).unwrap();
                        let n = socket.recv(&mut buf).unwrap();
                        assert_eq!(n, 8);
                    }
                });
            }
        });
    };
    trips(WARM);
    let arms_before = ARMS.load(Ordering::Relaxed);
    let cpu_before = registry::shard_cpu(holder).map(|reading| reading.spent_ns);
    let usage_before = hyper_measure::usage::this().ok();
    let started = Instant::now();
    trips(TRIPS);
    let wall = started.elapsed();
    let usage_after = hyper_measure::usage::this().ok();
    let cpu_after = registry::shard_cpu(holder).map(|reading| reading.spent_ns);
    let arms = ARMS.load(Ordering::Relaxed) - arms_before;
    runtime.shutdown().unwrap();
    Echo {
        trips: TRIPS * clients,
        wall,
        shard_cpu_ns: cpu_after.zip(cpu_before).map_or(0, |(a, b)| a - b),
        arms,
        instructions: usage_after
            .zip(usage_before)
            .and_then(|(after, before)| after.since(&before).instructions),
    }
}

fn main() {
    let calibration = Calibration::measure(Duration::from_millis(40), 1).unwrap();
    let constants = calibration
        .constants(&Policy {
            reserved_cores: 1,
            lateness_tolerance_ns: None,
            latency_objective_ns: None,
        })
        .unwrap();
    let mut config = RuntimeConfig::from_calibration(&calibration, &constants, 64, 64);
    config.shards = 1;
    config.pin = false;
    config.cores.clear();
    let alone = rearm_alone();
    match alone {
        Some((rearm, floor)) => println!(
            "re-arm alone: {rearm:.0} ns a call; getppid {floor:.0} ns (median of {ROUNDS} rounds of {CALLS})"
        ),
        None => println!("re-arm alone: none here (an AFD poll is one request a wait)"),
    }
    println!(
        "clients  round trips/s  shard CPU/trip ns  re-arms/trip  instructions/trip  re-arm share of the shard's CPU"
    );
    for clients in [1, 8] {
        let row = echo(&config, clients);
        let per_trip = row.shard_cpu_ns as f64 / row.trips as f64;
        let arms = row.arms as f64 / row.trips as f64;
        let share = alone.map_or(String::from("-"), |(rearm, _)| {
            format!("{:.1} %", 100.0 * arms * rearm / per_trip)
        });
        let instructions = row.instructions.map_or(String::from("-"), |count| {
            format!("{:.0}", count as f64 / row.trips as f64)
        });
        println!(
            "{clients:>7}  {:>13.0}  {per_trip:>17.0}  {arms:>12.3}  {instructions:>17}  {share}",
            row.trips as f64 / row.wall.as_secs_f64()
        );
    }
}
