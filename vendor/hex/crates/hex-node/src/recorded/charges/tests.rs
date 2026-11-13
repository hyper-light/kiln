//! The macOS and Windows nodes' charge learner (joint plan step 14, AC-T7.4.1; `docs/design/learned-charge.md`).

use std::cell::RefCell;

use super::*;
use hex_api::{Allocation, BoxId, BoxSpec, ClassSpec, OciSpec};
use hex_core::record::{NativeScope, ProcId};

const GIB: u64 = 1 << 30;

fn res(cpu_milli: u64, mem_bytes: u64) -> CapacityResources {
    CapacityResources {
        cpu_milli,
        mem_bytes,
        disk_bytes: 0,
    }
}

/// A Shards VM's profile: 16 CPUs and 8 GiB, its envelope its floor and ceiling.
fn cost() -> RuntimeCost {
    let envelope = res(16_000, 8 * GIB);
    RuntimeCost {
        id: [14; 32],
        floor: envelope,
        ceiling: envelope,
        quantum: CapacityResources {
            cpu_milli: 1,
            mem_bytes: 1,
            disk_bytes: 1,
        },
        guest_overhead: CapacityResources::default(),
        overhead: CapacityResources::default(),
    }
}

/// A running Shards VM's record, booked at 4 CPUs and 4 GiB (its class's prior), its lifetime `n`.
fn vm(name: &str, n: u8) -> BoxRecord {
    let allocation: Allocation = cost()
        .allocate(CapacityResources::default())
        .unwrap()
        .booked_at(res(4_000, 4 * GIB));
    let spec = BoxSpec {
        id: BoxId::new(name).unwrap(),
        bounds: hex_api::Bounds {
            cpu_max_milli: Some(16_000),
            memory_max: Some(8 * GIB),
            ..hex_api::Bounds::default()
        },
        class: ClassSpec::Shards(Box::new(OciSpec {
            rootfs: hex_api::Rootfs::Dir(std::path::PathBuf::from("/images/agent")),
            init: hex_api::Program::of(&["/bin/true"]),
            mounts: Vec::new(),
            hostname: None,
            network: hex_api::Network::Private,
            hosts: Vec::new(),
        })),
        owner: None,
        devices: Vec::new(),
        allocation: Some(allocation),
        route: None,
    };
    let mut record = BoxRecord::new(spec, Phase::Ready, Vec::new());
    record.native_scope = Some(NativeScope::from_bytes([n; 16]));
    record.init = Some(ProcId {
        pid: u32::from(n),
        start_ticks: 1,
    });
    record
}

/// The managed VMs as a record: each one's CPU and memory as the test sets them, and every cap set, in order.
#[derive(Default)]
struct Record {
    usage: RefCell<BTreeMap<String, Usage>>,
    rates: RefCell<Vec<(String, u64)>>,
    refuse_rates: std::cell::Cell<bool>,
}

impl Managed for Record {
    fn usage(&self, record: &BoxRecord) -> Result<Usage, Refusal> {
        self.usage
            .borrow()
            .get(record.spec.id.as_str())
            .cloned()
            .ok_or_else(|| Refusal::Broken("no such VM".into()))
    }

    fn set_rate(&self, record: &BoxRecord, rate: u64) -> Result<(), Refusal> {
        if self.refuse_rates.get() {
            return Err(Refusal::Broken("the rate was refused".into()));
        }
        self.rates
            .borrow_mut()
            .push((record.spec.id.as_str().to_owned(), rate));
        Ok(())
    }
}

impl Record {
    /// VM `name` has used `cpu_usec` of CPU in all and holds `memory`.
    fn set(&self, name: &str, cpu_usec: u64, memory: u64) {
        self.usage.borrow_mut().insert(
            name.to_owned(),
            Usage {
                cpu_usec,
                memory_bytes: memory,
                ..Usage::default()
            },
        );
    }
}

const WINDOW: Duration = Duration::from_millis(300);

/// Joint plan step 14 (AC-T7.4.1), C4 on macOS and Windows: a capping platform's node caps each managed VM at its
/// water-filled share. Do run a window of three Shards VMs of 16-CPU allocations, each booked at 4 CPUs, in a 24-CPU
/// fleet budget, then a window in which the first uses its whole 16-CPU cap, the second 1 CPU and the third 2; then
/// one more with nothing changed. Expect each VM capped first at its booking and an equal share of the 12 CPUs the
/// bookings leave (8 CPUs each); then the second and third at their bookings (they asked less) and the first at its
/// 16-CPU ceiling, its booking risen to the 8 CPUs it used and the 8 the others leave; the caps within the budget;
/// nothing set again when nothing changed; and the memory budgets refused, typed, naming what the platform lacks.
#[test]
fn w14_a_capping_node_caps_each_vm_at_its_water_filled_share() {
    let io = Record::default();
    let vms = vec![vm("vm-1", 1), vm("vm-2", 2), vm("vm-3", 3)];
    for record in &vms {
        io.set(record.spec.id.as_str(), 0, GIB);
    }
    let budget = res(24_000, 64 * GIB);
    let mut learner = Learner::new(WINDOW, res(32_000, 64 * GIB)).unwrap();
    let start = Instant::now();
    learner.window(&io, &vms, budget, start).unwrap();
    assert_eq!(
        *io.rates.borrow(),
        vec![
            ("vm-1".to_owned(), 8_000),
            ("vm-2".to_owned(), 8_000),
            ("vm-3".to_owned(), 8_000),
        ]
    );
    io.rates.borrow_mut().clear();
    // 300 ms windows: the first used its whole 8-CPU cap (2.4 s), the second 1 CPU, the third 2 CPUs.
    io.set("vm-1", 2_400_000, GIB);
    io.set("vm-2", 300_000, GIB);
    io.set("vm-3", 600_000, GIB);
    learner.window(&io, &vms, budget, start + WINDOW).unwrap();
    assert_eq!(
        *io.rates.borrow(),
        vec![
            ("vm-1".to_owned(), 16_000),
            ("vm-2".to_owned(), 4_000),
            ("vm-3".to_owned(), 4_000),
        ]
    );
    let caps: u64 = vms
        .iter()
        .map(|r| {
            learner
                .rate(r.native_scope.as_ref().unwrap().bytes())
                .unwrap()
        })
        .sum();
    assert!(caps <= budget.cpu_milli, "{caps} past the budget");
    io.rates.borrow_mut().clear();
    io.set("vm-1", 2_400_000 + 4_800_000, GIB);
    io.set("vm-2", 300_000 + 300_000, GIB);
    io.set("vm-3", 600_000 + 600_000, GIB);
    learner
        .window(&io, &vms, budget, start + WINDOW * 2)
        .unwrap();
    assert!(io.rates.borrow().is_empty(), "{:?}", io.rates.borrow());
    assert!(matches!(
        learner.unheld(),
        Refusal::Platform {
            what: "memory_high",
            ..
        }
    ));
}

/// Joint plan step 14 (AC-T7.4.1), C4 and C8 on macOS and Windows: a cap the platform refuses fails the window. Do run
/// a window of two VMs whose caps the native owner refuses, then ask for the class's capacity answer, then let a
/// window pass. Expect the window refused with the owner's refusal, typed, no learned use on the answer, and the
/// answer's learned use again once a window passes: the VMs' receipts and holdings.
#[test]
fn w14_a_refused_cap_fails_the_window_typed() {
    let io = Record::default();
    let vms = vec![vm("vm-1", 1), vm("vm-2", 2)];
    for record in &vms {
        io.set(record.spec.id.as_str(), 0, GIB);
    }
    let mut learner = Learner::new(WINDOW, res(32_000, 64 * GIB)).unwrap();
    io.refuse_rates.set(true);
    let start = Instant::now();
    assert_eq!(
        learner.window(&io, &vms, res(24_000, 64 * GIB), start),
        Err(Refusal::Broken("the rate was refused".into()))
    );
    assert_eq!(
        learner.learned(Class::Shards, &[cost()], &vms).unwrap(),
        None
    );
    io.refuse_rates.set(false);
    learner
        .window(&io, &vms, res(24_000, 64 * GIB), start + WINDOW)
        .unwrap();
    let learned = learner
        .learned(Class::Shards, &[cost()], &vms)
        .unwrap()
        .unwrap();
    assert_eq!(learned.used.booked, res(8_000, 8 * GIB));
    assert_eq!(learned.holdings.len(), 2);
}

/// Joint plan step 14 (AC-T7.4.1): which records are running managed VMs. Do judge a running Shards VM's record, then
/// the same deleting, ended, with no controller, with no lifetime, and a process box's. Expect only the first running.
#[test]
fn w14_only_a_made_live_managed_vm_is_running() {
    let live = vm("vm-1", 1);
    assert!(running(&live));
    let mut deleting = live.clone();
    deleting.phase = Phase::Deleting;
    let mut ended = live.clone();
    ended.exit = Some(hex_api::Exit::Signal(9));
    let mut uncontrolled = live.clone();
    uncontrolled.init = None;
    let mut unscoped = live.clone();
    unscoped.native_scope = None;
    let mut process = live.clone();
    process.spec.class = ClassSpec::Process;
    for record in [deleting, ended, uncontrolled, unscoped, process] {
        assert!(!running(&record), "{record:?}");
    }
}
