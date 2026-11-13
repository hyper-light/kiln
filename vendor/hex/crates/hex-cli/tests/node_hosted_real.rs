//! Joint plan step 39, AC-T7.4.1 (docs/design/node-under-shards-init.md §9): Hex's node under shards-init, with Hex's
//! own PID 1 kept, booted to its readiness and measured.
//!
//! The probe boots the node image (`scripts/guest/node.Agentfile`, built by `scripts/guest/node-image.sh`) through
//! the node's own boot of its guest under shards-init (`hex_runtime::guest::hosted::Hosted::boot`): shards-api's image
//! boot, then its READY followed, exactly as a node does it. Hex's init runs as the run's own command
//! under the nesting grant (Shards' D234), PID 1 of the workload's own PID namespace. Its READY=1 comes only once init
//! has done its duties there and its node and relay have each said they are ready, so a READY is that init's whole
//! boot under shards-init. Each run prints:
//! - its readiness, from the boot's ask to READY (`HostedGuest::took`);
//! - its stop, to the run's end;
//! - the run's CPU time, memory and tasks as its init reads its cgroup (STATS).
//!
//! The probe ends with n, p50, p90, p99 and max of each. The host's own cost of the VM, its VM and network processes,
//! is read beside it (ROOT's).
//!
//! Its waits are a node's own:
//! - the boot follows its run host, and READY follows its run at the cold start, then waits for the slowest READY
//!   this node has seen (decision 9, the person's rule of 2026-11-13);
//! - the stats and the stop wait for their fact (`Duration::MAX`), the run's end refusing them.
//!
//! Explicit inputs, each a variable; a missing one fails by name:
//! - `SHARDS_BIN`: the `shards` binary;
//! - `SHARDS_HOME`: the Shards home whose store holds `HEX_GUEST_IMAGE`, the node image pinned by digest
//!   (`TAG@sha256:...`, node-image.sh's output);
//! - `HEX_GUEST_CPUS` and `HEX_GUEST_MEMORY_MIB`: the node VM's size, as the direct boot's;
//! - `HEX_NODE_HOSTED_RUNS`: how many runs, n.
//!
//! `SHARDS_KERNEL` and `SHARDS_INIT`, where set, go to the run host as a run takes them (image_real.rs's).
//!
//! Run: `cargo test --release --locked -p hex-cli --test node_hosted_real -- --ignored --nocapture` with those set,
//! where Shards boots VMs: HVF on macOS, WHP on Windows, KVM on Linux. The direct boot's record, its counterpart, is
//! `guest_boot_record.rs`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros
)]

use std::ffi::OsString;
use std::sync::Arc;
use std::time::{Duration, Instant};

use hex_runtime::guest::hosted::{Hosted, HostedConfig};
use shards_api::image::{ImageShards, Ready, RunEnd, StatsScope};

fn given(name: &str) -> OsString {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| panic!("{name} is unset"))
}

fn number<T: std::str::FromStr>(name: &str) -> T
where
    T::Err: std::fmt::Debug,
{
    given(name).to_str().unwrap().parse().unwrap()
}

/// The node's guest under shards-init as `hex node` would configure it, with no share: Shards' binary and home, the
/// pinned node image and the VM's size.
fn config() -> HostedConfig {
    let mut env = vec![(OsString::from("SHARDS_HOME"), given("SHARDS_HOME"))];
    for name in ["SHARDS_KERNEL", "SHARDS_INIT"] {
        if let Some(value) = std::env::var_os(name) {
            env.push((OsString::from(name), value));
        }
    }
    HostedConfig {
        binary: given("SHARDS_BIN").into(),
        image: given("HEX_GUEST_IMAGE").into_string().unwrap(),
        vcpus: number("HEX_GUEST_CPUS"),
        memory_mib: number("HEX_GUEST_MEMORY_MIB"),
        shares: Vec::new(),
        env,
    }
}

/// `values`' nearest-rank p50, p90, p99 and max, sorted in place.
fn summary(values: &mut [u128]) -> String {
    values.sort_unstable();
    let rank = |p: usize| values[(values.len() * p).div_ceil(100).max(1) - 1];
    format!(
        "n={} p50={} p90={} p99={} max={}",
        values.len(),
        rank(50),
        rank(90),
        rank(99),
        values[values.len() - 1]
    )
}

#[test]
#[ignore = "a measurement: run with --ignored --nocapture, release, where Shards boots VMs"]
fn the_node_under_shards_init_boots_to_ready_with_hexs_own_init() {
    let config = config();
    let n: usize = number("HEX_NODE_HOSTED_RUNS");
    println!(
        "NODE-HOSTED image={} vcpus={} memory_mib={} n={n}",
        config.image, config.vcpus, config.memory_mib
    );
    let hosted = Hosted::new(Arc::new(ImageShards::new()), config);
    let (mut readies, mut stops, mut memory) = (vec![], vec![], vec![]);
    for i in 1..=n {
        let booted = hosted.boot().unwrap();
        // No deadline at the cold start, so READY=1 is on time, from Hex's init once its node and relay are ready.
        assert_eq!(booted.ready, Ready::OnTime, "run {i}");
        let stats = booted.guest.stats(StatsScope::Run, Duration::MAX).unwrap();
        let stopping = Instant::now();
        booted.guest.stop().unwrap();
        let end = booted.guest.wait_for(Duration::MAX).unwrap();
        let stopped = stopping.elapsed();
        assert_eq!(end, Some(RunEnd::Stopped), "run {i}");
        println!(
            "NODE-HOSTED {i} ready_us={} stop_us={} cpu_ns={:?} memory_bytes={:?} pids={:?}",
            booted.took.as_micros(),
            stopped.as_micros(),
            stats.cpu_ns,
            stats.memory_bytes,
            stats.pids
        );
        readies.push(booted.took.as_micros());
        stops.push(stopped.as_micros());
        memory.push(u128::from(stats.memory_bytes.unwrap()));
    }
    println!("NODE-HOSTED ready_us {}", summary(&mut readies));
    println!("NODE-HOSTED stop_us {}", summary(&mut stops));
    println!("NODE-HOSTED memory_bytes {}", summary(&mut memory));
}
