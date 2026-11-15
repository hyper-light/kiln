//! Joint plan step 39, AC-T7.4.1 (docs/design/node-under-shards-init.md §9): Hex's node under shards-init, with Hex's
//! own PID 1 kept, booted to its readiness and measured.
//!
//! The probe boots the node image (`scripts/guest/node.Agentfile`, built by `scripts/guest/node-image.sh`) as a node's
//! guest runtime boots its node VM, through the one path production runs (the review's C-6): the image class's
//! `boot_node` (`hex_runtime::guest::ImageClass`), its records hex-node's image runtime's, its run's agent-control
//! endpoint asked, its row kept, its READY followed. Hex's init runs as the run's own command under the nesting grant
//! (Shards' D234), PID 1 of the workload's own PID namespace. Its READY=1 comes only once init has done its duties
//! there and its node and relay have each said they are ready, so a READY is that init's whole boot under shards-init.
//! Each run prints:
//! - its readiness, from the boot's ask to READY (`NodeBooted::took`), and how its run started;
//! - its VM's use of its host as Hex reads it (`host_use_node`: its VM process's CPU time, footprint and resident
//!   memory, D241), the reading §9 takes beside the OS's count of the VM's processes (ROOT's);
//! - its stop, to its run's end kept in its row (`stop_node`).
//!
//! The probe ends with n, p50, p90, p99 and max of each.
//!
//! Its waits are a node's own: the boot follows its run host, READY follows its run at the cold start, then waits for
//! the slowest READY this process has seen (decision 9, the person's rule of 2026-11-13), and the host use and the stop
//! wait for their fact, the run's end refusing them.
//!
//! Explicit inputs, each a variable; a missing one fails by name:
//! - `SHARDS_BIN`: the `shards` binary;
//! - `SHARDS_HOME` and `SHARDS_CACHE`: the Shards home whose store holds `HEX_GUEST_IMAGE`, the node image pinned by
//!   digest (`TAG@sha256:...`, node-image.sh's output), and its cache, each run host's own;
//! - `HEX_GUEST_CPUS` and `HEX_GUEST_MEMORY_MIB`: the node VM's size, as the direct boot's;
//! - `HEX_NODE_HOSTED_RUNS`: how many runs, n.
//!
//! The node VM shares no folder of its host's. Its records are kept in a folder of the probe's own beside the Shards
//! home, gone with it. `SHARDS_KERNEL` and `SHARDS_INIT`, where set, go to the run host as a run takes them
//! (image_real.rs's).
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
use std::path::PathBuf;
use std::time::Instant;

use hex_api::{CapacityResources, RuntimeCost};
use hex_core::stream::files::FILES_PORT;
use hex_core::stream::{CONTROL_PORT, STAMPS_PORT, STDIO_PORT, socket_name};
use hex_runtime::devices::Books as _;
use hex_runtime::guest::NodeVm;
use shards_api::image::Ready;

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

/// The node VM's receipt as a node's guest runtime charges it, for the VM `HEX_GUEST_CPUS` and `HEX_GUEST_MEMORY_MIB`
/// size: a cost profile whose floor and ceiling are that VM, in the units a VM is sized in (a vCPU's milli-CPU, a MiB).
fn allocation() -> hex_api::Allocation {
    let vcpus: u64 = number("HEX_GUEST_CPUS");
    let memory_mib: u64 = number("HEX_GUEST_MEMORY_MIB");
    let vm = CapacityResources {
        cpu_milli: vcpus * 1_000,
        mem_bytes: memory_mib << 20,
        disk_bytes: 0,
    };
    let allocation = RuntimeCost {
        id: *b"hex-cli node_hosted_real node VM",
        floor: vm,
        ceiling: vm,
        quantum: CapacityResources {
            cpu_milli: 1_000,
            mem_bytes: 1 << 20,
            disk_bytes: 1,
        },
        guest_overhead: CapacityResources::default(),
        overhead: CapacityResources::default(),
    }
    .allocate(CapacityResources::default())
    .unwrap();
    let size = allocation.vm_size().unwrap();
    assert_eq!(
        (u64::from(size.vcpus), size.memory_mib),
        (vcpus, memory_mib),
        "the receipt sizes the configured VM"
    );
    allocation
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
    let shards = PathBuf::from(given("SHARDS_BIN"));
    let home = PathBuf::from(given("SHARDS_HOME"));
    let image = given("HEX_GUEST_IMAGE").into_string().unwrap();
    let n: usize = number("HEX_NODE_HOSTED_RUNS");
    let allocation = allocation();
    let paths =
        shards_api::RuntimePaths::new(PathBuf::from(given("SHARDS_CACHE")), home.clone()).unwrap();
    let records = tempfile::tempdir_in(home.parent().unwrap()).unwrap();
    let runtime = hex_node::images::ImageRuntime::new(shards, paths, records.path()).unwrap();
    // The node's key, the one its run's endpoint takes (D252), and one lane for each listener of the relay's
    // (hex_core::stream::socket_name), as many as a connection of each takes at once.
    let controller = shards_api::control::Identity::generate()
        .unwrap()
        .fingerprint();
    let lanes = u32::try_from(
        [CONTROL_PORT, STDIO_PORT, FILES_PORT, STAMPS_PORT]
            .into_iter()
            .filter(|port| socket_name(*port).is_some())
            .count(),
    )
    .unwrap();
    let size = allocation.vm_size().unwrap();
    println!(
        "NODE-HOSTED image={image} vcpus={} memory_mib={} n={n}",
        size.vcpus, size.memory_mib
    );
    let (mut readies, mut stops, mut footprints) = (vec![], vec![], vec![]);
    for i in 1..=n {
        let vm = NodeVm {
            run: runtime.mint_node_run().unwrap(),
            image: image.clone(),
            allocation,
            shares: Vec::new(),
            controller,
            lanes,
        };
        let booted = runtime.class().boot_node(&vm).unwrap();
        // No deadline of Hex's, so READY=1 is on time, from Hex's init once its node and relay are ready.
        assert_eq!(booted.ready, Ready::OnTime, "run {i}");
        let used = runtime.class().host_use_node(vm.run).unwrap();
        let stopping = Instant::now();
        let exit = runtime.class().stop_node(vm.run).unwrap();
        let stopped = stopping.elapsed();
        println!(
            "NODE-HOSTED {i} ready_us={} started_from={:?} stop_us={} exit={exit:?} cpu_ns={} footprint_bytes={} \
             resident_bytes={}",
            booted.took.as_micros(),
            booted.started_from,
            stopped.as_micros(),
            used.cpu_ns,
            used.footprint_bytes,
            used.resident_bytes
        );
        readies.push(booted.took.as_micros());
        stops.push(stopped.as_micros());
        footprints.push(u128::from(used.footprint_bytes));
    }
    println!("NODE-HOSTED ready_us {}", summary(&mut readies));
    println!("NODE-HOSTED stop_us {}", summary(&mut stops));
    println!("NODE-HOSTED footprint_bytes {}", summary(&mut footprints));
}
