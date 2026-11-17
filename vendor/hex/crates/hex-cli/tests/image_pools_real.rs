//! Kiln plan `01a1dd2e313f72f69c6d93061143cc2e`, P3.1 (CP-T7.2, AC-T7.6.2; docs/design/shards-image-class.md §10): the
//! image class's warm pools on real VMs, through hex-node's image runtime as kiln makes and drives it (Shards' D253
//! `ImagePools`, each kept pool kiln's to name). Its room is the owner's ledger kiln's runner keeps ([`Ledger`]): each
//! warm host's claim and each box's admission taken from one budget, never past it.
//!
//! - **The second start is warm**: two starts of a kept pool's image and size, the second once its pool holds a warm
//!   host, report `Warm` (the plan's first test).
//! - **No box past the room**: a node holding a warm host in a room with space for one box beside it admits no second
//!   box, and does once the pool is let go and its host's room is back (the plan's second test).
//! - **Warm starts, measured**: n warm starts end to end through Hex, from the box's create to its start's answer, each
//!   asserted `Warm`, with n, p50, p90, p99 and max (the plan's p50 <= 300 ms bar; Shards'
//!   docs/research/measurements/p3-warm-start measures Shards' own side of it).
//!
//! Explicit inputs, each a variable; a missing one fails by name:
//! - `SHARDS_BIN`: the `shards` binary; `SHARDS_HOME` and `SHARDS_CACHE`: the Shards home whose store holds
//!   `HEX_IMAGE`, an image pinned by digest whose members run until stopped (`crates/hex-cli/tests/image_harness` built
//!   there, as image_real.rs builds it), and its cache. `SHARDS_KERNEL` and `SHARDS_INIT`, where set, go to the run
//!   hosts as a run takes them;
//! - `HEX_GUEST_CPUS` and `HEX_GUEST_MEMORY_MIB`: each VM's size, one Shards boots as asked;
//! - `HEX_IMAGE_POOL_RUNS`: how many warm starts the measurement makes, n (20 or more for the plan's bar).
//!
//! Run: `cargo test --release --locked -p hex-cli --test image_pools_real -- --ignored --nocapture --test-threads=1`
//! with those set, where Shards boots VMs: HVF on macOS, KVM on Linux, WHP on Windows.

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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use hex_api::{
    Allocation, Bounds, BoxId, BoxSpec, CapacityResources, ClassSpec, LifecycleCause, Refusal,
    Runtime, RuntimeCost, ShardsImage, StartedFrom, WarmPool,
};
use hex_core::patience::Patience;
use hex_node::images::ImageRuntime;
use hex_testkit::waits::follow::{self, Party};
use shards_api::image::{Gave, WarmHost, WarmRoom};

/// A kept pool's first warm host ready: this test's fact alone.
static WARMED: Patience = Patience::measuring("test.hex_cli.image_pool_warmed");

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

/// The owner's room as kiln's runner keeps one: `budget` bytes, every warm host's claim and every box's admission taken
/// from it and never past it; what it holds now and the most it ever held; and the claims it refused and the starts
/// that failed, as their room came back.
struct Ledger {
    budget: u64,
    held: Mutex<u64>,
    most: AtomicU64,
    refused: AtomicU64,
    failed: AtomicU64,
}

impl Ledger {
    fn new(budget: u64) -> Arc<Self> {
        Arc::new(Self {
            budget,
            held: Mutex::new(0),
            most: AtomicU64::new(0),
            refused: AtomicU64::new(0),
            failed: AtomicU64::new(0),
        })
    }

    /// `bytes` taken where they fit what is free now, with no headroom kept (Shards' `WarmRoom::claim`); else refused.
    fn take(&self, bytes: u64) -> bool {
        let mut held = self.held.lock().unwrap();
        if *held + bytes > self.budget {
            self.refused.fetch_add(1, Ordering::SeqCst);
            return false;
        }
        *held += bytes;
        self.most.fetch_max(*held, Ordering::SeqCst);
        true
    }

    fn back(&self, bytes: u64) {
        *self.held.lock().unwrap() -= bytes;
    }

    fn held(&self) -> u64 {
        *self.held.lock().unwrap()
    }
}

/// The ledger as the image runtime's warm room: a host's claim taken from it, its give put back.
struct Room(Arc<Ledger>);

impl WarmRoom for Room {
    fn claim(&self, host: &WarmHost) -> bool {
        self.0.take(host.memory)
    }

    fn give(&self, host: &WarmHost, why: Gave) {
        if why == Gave::Failed {
            self.0.failed.fetch_add(1, Ordering::SeqCst);
        }
        self.0.back(host.memory);
    }
}

/// An image runtime as kiln makes it, its records in a folder of the test's own, and the image, VM size and allocation
/// its boxes take.
struct Setup {
    runtime: Arc<ImageRuntime>,
    image: String,
    allocation: Allocation,
    vm_bytes: u64,
    _records: tempfile::TempDir,
}

fn setup(ledger: &Arc<Ledger>) -> Setup {
    let home = PathBuf::from(given("SHARDS_HOME"));
    let paths =
        shards_api::RuntimePaths::new(PathBuf::from(given("SHARDS_CACHE")), home.clone()).unwrap();
    let records = tempfile::tempdir_in(home.parent().unwrap()).unwrap();
    let runtime =
        ImageRuntime::new(PathBuf::from(given("SHARDS_BIN")), paths, records.path()).unwrap();
    runtime
        .bind_room(Arc::new(Room(Arc::clone(ledger))))
        .unwrap();
    let vcpus: u64 = number("HEX_GUEST_CPUS");
    let memory_mib: u64 = number("HEX_GUEST_MEMORY_MIB");
    let vm = CapacityResources {
        cpu_milli: vcpus * 1_000,
        mem_bytes: memory_mib << 20,
        disk_bytes: 0,
    };
    let allocation = RuntimeCost {
        id: *b"hex-cli image_pools_real VM size",
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
    Setup {
        runtime,
        image: given("HEX_IMAGE").into_string().unwrap(),
        allocation,
        vm_bytes: memory_mib << 20,
        _records: records,
    }
}

impl Setup {
    fn pool(&self) -> WarmPool {
        WarmPool {
            image: self.image.clone(),
            size: self.allocation.vm_size().unwrap(),
        }
    }

    /// A box of the pool's image and size, admitted from `ledger` first as its owner admits one: none where it doesn't
    /// fit, nothing made.
    fn admit(&self, ledger: &Ledger, name: &str) -> Option<BoxId> {
        if !ledger.take(self.vm_bytes) {
            return None;
        }
        let id = BoxId::new(name).unwrap();
        self.runtime
            .create(BoxSpec {
                id: id.clone(),
                bounds: Bounds::default(),
                class: ClassSpec::ShardsImage(Box::new(ShardsImage::new(&self.image))),
                owner: None,
                devices: Vec::new(),
                allocation: Some(self.allocation),
                route: None,
                band: 0,
            })
            .unwrap();
        Some(id)
    }

    /// Box `id` started, its start followed past W* as kiln follows one (`Runtime::started`); how its microVM started.
    fn start(&self, id: &BoxId) -> StartedFrom {
        match self.runtime.start(id) {
            Ok(()) => {}
            Err(Refusal::Lifecycle {
                cause: LifecycleCause::Pending,
                ..
            }) => {
                self.runtime.started(id).unwrap();
            }
            Err(why) => panic!("box {id}'s start: {why}"),
        }
        self.runtime.usage(id).unwrap().started_from.unwrap()
    }

    /// Box `id` deleted and its admission given back to `ledger`.
    fn delete(&self, ledger: &Ledger, id: &BoxId) {
        self.runtime.delete(id).unwrap();
        ledger.back(self.vm_bytes);
    }

    /// Until the pool holds a warm host: followed while no start of its hosts has failed.
    fn warmed(&self, ledger: &Ledger) {
        let pool = self.pool();
        follow::until(
            &WARMED,
            "a warm host of the kept pool",
            || {
                if ledger.failed.load(Ordering::SeqCst) == 0 {
                    Party::Live
                } else {
                    Party::Ended
                }
            },
            || {
                self.runtime
                    .pools()
                    .unwrap()
                    .iter()
                    .any(|held| held.pool == pool && held.warm > 0)
            },
        )
        .unwrap();
    }
}

/// P3.1's first test: do keep a pool, start a box of its image and size, wait until the pool holds a warm host, and
/// start another. Expect: the second reports `Warm`; every claim within the room; and, once both are deleted and the
/// pool let go, all its room back.
#[test]
#[ignore = "real VMs: run with --ignored --nocapture, release, where Shards boots VMs"]
fn p3_1_a_kept_pools_second_start_is_warm() {
    let ledger = Ledger::new(u64::MAX);
    let setup = setup(&ledger);
    setup.runtime.keep_pool(&setup.pool()).unwrap();
    let first = setup.admit(&ledger, "p31-first").unwrap();
    let from = setup.start(&first);
    setup.warmed(&ledger);
    let second = setup.admit(&ledger, "p31-second").unwrap();
    assert_eq!(
        setup.start(&second),
        StartedFrom::Warm,
        "the first {from:?}"
    );
    setup.delete(&ledger, &first);
    setup.delete(&ledger, &second);
    setup.runtime.let_go_pool(&setup.pool()).unwrap();
    assert_eq!(ledger.held(), 0, "room held past the pool's let-go");
}

/// P3.1's second test: do keep a pool in a room with space for two VMs, start a box of it, wait until the pool holds a
/// warm host, and ask another box admitted; then let the pool go and ask again. Expect: the room never held past its
/// budget; the second box refused by its owner while the warm host holds its room, nothing made; and, the pool let go,
/// admitted, booting on a run host of its own, no pool kept for it.
#[test]
#[ignore = "real VMs: run with --ignored --nocapture, release, where Shards boots VMs"]
fn p3_1_a_node_holding_a_warm_host_admits_no_box_past_its_room() {
    let ledger = Ledger::new(2 * (number::<u64>("HEX_GUEST_MEMORY_MIB") << 20));
    let setup = setup(&ledger);
    setup.runtime.keep_pool(&setup.pool()).unwrap();
    let first = setup.admit(&ledger, "p31-room-first").unwrap();
    setup.start(&first);
    setup.warmed(&ledger);
    assert_eq!(setup.admit(&ledger, "p31-room-second"), None);
    assert_eq!(
        setup
            .runtime
            .inspect(&BoxId::new("p31-room-second").unwrap()),
        Err(Refusal::NoSuchBox(BoxId::new("p31-room-second").unwrap()))
    );
    setup.runtime.let_go_pool(&setup.pool()).unwrap();
    let second = setup.admit(&ledger, "p31-room-second").unwrap();
    assert_ne!(setup.start(&second), StartedFrom::Warm);
    assert!(ledger.most.load(Ordering::SeqCst) <= ledger.budget);
    setup.delete(&ledger, &first);
    setup.delete(&ledger, &second);
    assert_eq!(ledger.held(), 0);
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

/// AC-T7.6.2, the plan's bar (a measurement, ROOT's): do keep a pool, start one box of it cold, then n times wait until
/// the pool holds a warm host and time a box's create and start to its answer, deleting each. Expect: each `Warm`; the
/// starts' n, p50, p90, p99 and max printed (`P31-WARM`), with the host's OS and architecture.
#[test]
#[ignore = "a measurement: run with --ignored --nocapture, release, where Shards boots VMs"]
fn p3_1_warm_starts_end_to_end() {
    let n: usize = number("HEX_IMAGE_POOL_RUNS");
    let ledger = Ledger::new(u64::MAX);
    let setup = setup(&ledger);
    setup.runtime.keep_pool(&setup.pool()).unwrap();
    let cold = setup.admit(&ledger, "p31-cold").unwrap();
    let from = setup.start(&cold);
    let mut starts = Vec::with_capacity(n);
    for i in 0..n {
        setup.warmed(&ledger);
        let began = Instant::now();
        let id = setup.admit(&ledger, &format!("p31-warm-{i}")).unwrap();
        let started = setup.start(&id);
        let took = began.elapsed();
        assert_eq!(started, StartedFrom::Warm, "start {i}");
        starts.push(took.as_micros());
        setup.delete(&ledger, &id);
    }
    setup.delete(&ledger, &cold);
    setup.runtime.let_go_pool(&setup.pool()).unwrap();
    println!(
        "P31-WARM os={} arch={} image={} cold={from:?} start_us {}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        setup.image,
        summary(&mut starts)
    );
}
