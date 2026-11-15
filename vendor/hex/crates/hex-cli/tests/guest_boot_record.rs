//! A host class's boot-to-ready and stop record (native plan step 5, constants; the lead's rule that a
//! class's seeds come from its own records, bench/native/guest/records.tsv): boots the guest image through
//! `shards_api::Shards` N times, as `hex_runtime::guest::GuestRuntime` does, and prints each boot's time from
//! `boot` to the relay's first bytes on its control port (the connection's token), and each stop's time
//! from `stop` to the guest's end. Portable: it is how a host without Python (jupiter) takes its record.
//! With each boot it prints the guest's own stamps (`STAMPS_PORT`, `CLOCK_BOOTTIME` nanoseconds at init's
//! start, its mounts, its node ready and its relay listening), which split the boot
//! (bench/native/guest/boot-split/RECORD.md).
//!
//! Ignored by default: `cargo test -p hex-cli --release --test guest_boot_record -- --ignored --nocapture`,
//! with `SHARDS_BIN`, `HEX_GUEST_KERNEL`, `HEX_GUEST_INITRD` and `BOOT_RECORD_N` (default 40).
//! `BOOT_RECORD_DIAL=tight` dials in single tries back to back (`Duration::ZERO`, no pause) instead of
//! shards_api's paced `connect`, and prints each attempt's start and result: the dial's own pacing cost is
//! the paced boot less the tight one, and the attempts refused after the guest listens are the VMM's.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros
)]

use std::io::Read;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use hex_core::stream;
use hex_runtime::guest::{CLASS, GuestConfig};
use shards_api::Guests;

fn given(name: &str) -> PathBuf {
    PathBuf::from(std::env::var_os(name).unwrap_or_else(|| panic!("{name} is unset")))
}

#[test]
#[ignore = "a measurement: run with --ignored --nocapture, release, where a guest boots"]
fn boot_to_ready_and_stop() {
    let config = GuestConfig {
        binary: given("SHARDS_BIN"),
        boot: hex_runtime::guest::Boot::Kernel {
            kernel: given("HEX_GUEST_KERNEL"),
            initrd: given("HEX_GUEST_INITRD"),
        },
        vcpus: host_cpus(),
        gpu: false,
        // Shards' least guest and twice HEX-GUEST-19's measured peak, as guest_real sizes it.
        memory_mib: shards_api::min_memory_mib(std::env::consts::ARCH) + 2 * 284,
        shares: Vec::new(),
        staged: hex_client::guest::stage::STAGED.to_owned(),
    };
    let n: usize = std::env::var("BOOT_RECORD_N").map_or(40, |v| v.parse().unwrap());
    let tight = std::env::var("BOOT_RECORD_DIAL").is_ok_and(|v| v == "tight");
    let shards = shards_api::Shards::new();
    println!(
        "RECORD class={CLASS} n={n} dial={}",
        if tight { "tight" } else { "paced" }
    );
    let mut spec = config.spec().unwrap();
    spec.ports.push(stream::STAMPS_PORT);
    for i in 1..=n {
        let began = Instant::now();
        let guest = shards.boot(&spec).unwrap();
        // The relay's first bytes, its token: the guest is ready (the facts alone; no clock).
        let mut attempts = Vec::new();
        let mut conn = if tight {
            loop {
                let at = began.elapsed();
                match guest.connect(stream::CONTROL_PORT, Duration::ZERO) {
                    Ok(conn) => {
                        attempts.push(format!("{}:ok", at.as_micros()));
                        break conn;
                    }
                    Err(why) => attempts.push(format!("{}:{}", at.as_micros(), kind(&why))),
                }
            }
        } else {
            guest.connect(stream::CONTROL_PORT, Duration::MAX).unwrap()
        };
        let mut token = [0u8; stream::TOKEN_BYTES];
        conn.read_exact(&mut token).unwrap();
        let ready = began.elapsed();
        drop(conn);
        let mut stamps = String::new();
        guest
            .connect(stream::STAMPS_PORT, Duration::ZERO)
            .unwrap()
            .read_to_string(&mut stamps)
            .unwrap();
        let stopping = Instant::now();
        guest.stop().unwrap();
        let end = guest.wait_for(Duration::MAX).unwrap();
        let stopped = stopping.elapsed();
        println!(
            "RECORD {i} boot_ready_us={} stop_us={} end={end:?} stamps_ns={}",
            ready.as_micros(),
            stopped.as_micros(),
            stamps.trim()
        );
        if tight {
            // Each attempt's start (µs from `boot`) and how it ended; the last few before the OK.
            let shown = attempts.len().saturating_sub(12);
            println!(
                "DIAL {i} attempts={} last={}",
                attempts.len(),
                attempts[shown..].join(",")
            );
        }
    }
}

/// A refused dial's kind, short.
fn kind(why: &shards_api::Refusal) -> &'static str {
    match why {
        shards_api::Refusal::Channel { .. } => "refused",
        shards_api::Refusal::Ended(_) => "ended",
        shards_api::Refusal::Boot(_) => "boot",
        _ => "other",
    }
}

/// The host's CPUs, as std reads them: a guest here has every one, as a vCPU costs nothing while
/// it idles.
fn host_cpus() -> u32 {
    std::thread::available_parallelism().map_or(1, |n| u32::try_from(n.get()).unwrap())
}
