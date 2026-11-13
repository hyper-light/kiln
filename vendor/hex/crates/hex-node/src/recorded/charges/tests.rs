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
        band: 0,
    };
    let mut record = BoxRecord::new(spec, Phase::Ready, Vec::new());
    record.native_scope = Some(NativeScope::from_bytes([n; 16]));
    record.init = Some(ProcId {
        pid: u32::from(n),
        start_ticks: 1,
    });
    record
}

/// The managed VMs as a record: each one's CPU and memory as the test sets them, every cap set and every VM stopped,
/// in order.
#[derive(Default)]
struct Record {
    usage: RefCell<BTreeMap<String, Usage>>,
    /// Each image VM's host use, as the test sets it, and each room it was told, in order.
    host: RefCell<BTreeMap<String, HostUse>>,
    rooms: RefCell<Vec<(String, i64)>>,
    rates: RefCell<Vec<(String, u64)>>,
    refuse_rates: std::cell::Cell<bool>,
    stops: RefCell<Vec<String>>,
    /// The node's records, where the test keeps them: an intent is written there, as the node writes it.
    records: RefCell<Option<Store>>,
    /// Every intent asked and every stop, in order: `intend <box>`, `stop <box>`.
    events: RefCell<Vec<String>>,
}

impl Managed for Record {
    fn usage(&self, record: &BoxRecord) -> Result<Usage, Refusal> {
        self.usage
            .borrow()
            .get(record.spec.id.as_str())
            .cloned()
            .ok_or_else(|| Refusal::Broken("no such VM".into()))
    }

    fn host_use(&self, record: &BoxRecord) -> Result<HostUse, Refusal> {
        self.host
            .borrow()
            .get(record.spec.id.as_str())
            .cloned()
            .ok_or_else(|| Refusal::Broken("no such VM".into()))
    }

    fn set_room(&self, record: &BoxRecord, room: i64) -> Result<(), Refusal> {
        self.rooms
            .borrow_mut()
            .push((record.spec.id.as_str().to_owned(), room));
        Ok(())
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

    fn stop(&self, record: &BoxRecord) -> Result<(), Refusal> {
        self.stops
            .borrow_mut()
            .push(record.spec.id.as_str().to_owned());
        self.events
            .borrow_mut()
            .push(format!("stop {}", record.spec.id));
        Ok(())
    }

    fn intend(&self, id: &BoxId, scope: Scope, end: &LearnedEnd) -> Result<bool, Refusal> {
        self.events.borrow_mut().push(format!("intend {id}"));
        match self.records.borrow().as_ref() {
            Some(store) => record_last_resort(store, id, scope, Some(end)),
            None => Ok(false),
        }
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

/// A learner of every VM, each kind's cap's period taken.
const LEARNS: Learns = Learns {
    managed: true,
    images: true,
};

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
    let mut learner = Learner::new(WINDOW, res(32_000, 64 * GIB), LEARNS).unwrap();
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
        .map(|r| learner.rate(&Scope::of(r).unwrap()).unwrap())
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
    let mut learner = Learner::new(WINDOW, res(32_000, 64 * GIB), LEARNS).unwrap();
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

/// Joint plan step 14 (AC-T7.4.1), C2: a learner carries a cold class at the workload's admission, the census's VMM
/// overhead its ceiling's alone. Do ask a learner that has learned no VM for the capacity answer of a Shards profile
/// whose receipt is its 16-CPU, 8 GiB envelope and a census overhead of half a CPU and 256 MiB. Expect the profile's
/// prior carried at the envelope its VMs are admitted at, its ceiling the whole receipt.
#[test]
fn w14_a_cold_class_is_carried_at_its_admission() {
    let census = RuntimeCost {
        overhead: res(500, 256 << 20),
        ..cost()
    };
    let receipt = census.allocate(CapacityResources::default()).unwrap();
    let learner = Learner::new(WINDOW, res(32_000, 64 * GIB), LEARNS).unwrap();
    let learned = learner
        .learned(Class::Shards, &[census], &[])
        .unwrap()
        .unwrap();
    assert_eq!(
        learned.priors,
        vec![hex_api::LearnedPrior {
            profile: census.id,
            ceiling: receipt.resources,
            booking: res(16_000, 8 * GIB),
        }]
    );
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

/// Three running Shards VMs, each booked at 4 GiB (their class's prior), whose fleet's memory budget is 16 GiB.
fn fleet_of_three() -> Vec<BoxRecord> {
    vec![vm("vm-1", 1), vm("vm-2", 2), vm("vm-3", 3)]
}

/// Joint plan step 14 (AC-T7.4.1), C5's last resort on macOS and Windows: a capping node ends a VM through its managed
/// receipt. Do run a window of three Shards VMs booked at 4 GiB in a 16 GiB fleet, then one where they hold 6, 7 and
/// 2 GiB, past the fleet's budget with the growth on record; then another with all three still running; then one with
/// vm-2 gone. Expect vm-2, the VM holding most past its own budget, stopped through its receipt, the class's capacity
/// answer saying why (its 7 GiB past its budget, the fleet's 15 GiB, 15 GiB of growth, its 16 GiB budget, its move
/// awaiting D176); no second stop while vm-2 runs; and none once it's gone, every VM then within its budget.
#[test]
fn w14_a_capping_node_ends_the_vm_most_past_its_budget_through_its_receipt() {
    let io = Record::default();
    let vms = fleet_of_three();
    for record in &vms {
        io.set(record.spec.id.as_str(), 0, 0);
    }
    let budget = res(24_000, 16 * GIB);
    let mut learner = Learner::new(WINDOW, res(32_000, 64 * GIB), LEARNS).unwrap();
    let start = Instant::now();
    learner.window(&io, &vms, budget, start).unwrap();
    for (name, held) in [("vm-1", 6 * GIB), ("vm-2", 7 * GIB), ("vm-3", 2 * GIB)] {
        io.set(name, 0, held);
    }
    learner.window(&io, &vms, budget, start + WINDOW).unwrap();
    assert_eq!(*io.stops.borrow(), ["vm-2"]);
    assert!(
        matches!(learner.last.resort(), Resort::Ending { id, .. } if id.as_str() == "vm-2"),
        "{:?}",
        learner.last.resort()
    );
    let learned = learner
        .learned(Class::Shards, &[cost()], &vms)
        .unwrap()
        .unwrap();
    let ends: Vec<_> = learned
        .ended
        .iter()
        .map(|end| {
            (
                end.id.as_str(),
                end.held,
                end.budget < end.held,
                (end.fleet_held, end.fleet_growth, end.fleet_budget),
                end.unmoved,
            )
        })
        .collect();
    assert_eq!(
        ends,
        [(
            "vm-2",
            7 * GIB,
            true,
            (15 * GIB, 15 * GIB, 16 * GIB),
            Some(hex_api::Upstream::LiveMove)
        )]
    );
    learner
        .window(&io, &vms, budget, start + WINDOW * 2)
        .unwrap();
    assert_eq!(io.stops.borrow().len(), 1, "one end is never two");
    let left = vec![vm("vm-1", 1), vm("vm-3", 3)];
    learner
        .window(&io, &left, budget, start + WINDOW * 3)
        .unwrap();
    assert_eq!(io.stops.borrow().len(), 1, "never a VM within its budget");
    assert!(
        matches!(
            learner.last.resort(),
            Resort::Refused(Refusal::Exhausted { .. })
        ),
        "{:?}",
        learner.last.resort()
    );
}

/// A running Shards VM's record (as [`vm`]: booked at 4 GiB, its lifetime `scope`), begun `started`-th on its node.
fn begun(name: &str, scope: u8, started: u64) -> BoxRecord {
    let mut record = vm(name, scope);
    record.started = Some(started);
    record
}

/// Joint plan step 14 (AC-T7.4.1), C3's order on every OS (ROOT, 2026-11-12): of two VMs rising past their bookings
/// into the same room, the one begun later is over and the last resort ends it, whatever their scopes. Do run a window of
/// three Shards VMs booked at 4 GiB in a 16 GiB fleet, begun first, second and third on their node, their scopes in the
/// opposite order (the first's the largest), then one where the first two hold 7 GiB each and the third 2 GiB. Expect the
/// fleet's 4 GiB of room to the first (its budget 7 GiB) and the rest of it, 1 GiB, to the second (its budget 5 GiB), so
/// the second, 2 GiB past its budget, stopped through its receipt and no other VM; and its why on the class's capacity
/// answer naming it: 7 GiB held against its 5 GiB budget, the fleet's 16 GiB held, 16 GiB of growth, its 16 GiB budget.
#[test]
fn w14_of_two_vms_rising_into_one_room_the_later_begun_is_ended() {
    let io = Record::default();
    let vms = vec![
        begun("vm-first", 9, 1),
        begun("vm-second", 5, 2),
        begun("vm-third", 1, 3),
    ];
    for record in &vms {
        io.set(record.spec.id.as_str(), 0, 0);
    }
    let budget = res(24_000, 16 * GIB);
    let mut learner = Learner::new(WINDOW, res(32_000, 64 * GIB), LEARNS).unwrap();
    let start = Instant::now();
    learner.window(&io, &vms, budget, start).unwrap();
    for (name, held) in [
        ("vm-first", 7 * GIB),
        ("vm-second", 7 * GIB),
        ("vm-third", 2 * GIB),
    ] {
        io.set(name, 0, held);
    }
    learner.window(&io, &vms, budget, start + WINDOW).unwrap();
    assert_eq!(*io.stops.borrow(), ["vm-second"]);
    let learned = learner
        .learned(Class::Shards, &[cost()], &vms)
        .unwrap()
        .unwrap();
    assert_eq!(
        learned.ended,
        vec![LearnedEnd {
            id: BoxId::new("vm-second").unwrap(),
            held: 7 * GIB,
            budget: 5 * GIB,
            band: 0,
            fleet_held: 16 * GIB,
            fleet_growth: 16 * GIB,
            fleet_budget: 16 * GIB,
            unmoved: Some(hex_api::Upstream::LiveMove),
        }]
    );
}

/// The records a node keeps of its boxes, in `dir`, as `Recorded::open` makes them.
fn books(dir: &std::path::Path) -> crate::recorded::Books {
    crate::recorded::Books {
        store: crate::store::Store::open(&dir.join("state"), &dir.join("run")).unwrap(),
        prober: None,
        lifecycle: crate::turns::Turns::new(
            "changing a recorded box's lifecycle",
            "recorded lifecycle methods",
            crate::store::TURNS,
            hex_core::patience::Patience::seeded("recorded.lifecycle", Duration::ZERO),
        ),
        abandoned: crate::abandoned::Abandoned::default(),
        adopted: std::sync::Mutex::new(std::collections::BTreeSet::new()),
        last_boot: None,
        charges: std::sync::OnceLock::new(),
    }
}

/// Joint plan step 14 (AC-T7.4.1), C5 on the box itself on macOS and Windows (ROOT, 2026-11-12): a capping node's
/// inspect says why its last resort ended a VM. Do record a Shards VM in a node's store as its create does, write
/// why the last resort ended it into its record as the node does after the window, and count its inspect from its
/// backend's info as the node does. Expect the inspect to say why.
#[test]
fn w14_a_capping_nodes_inspect_says_why_its_last_resort_ended_a_vm() {
    let dir = tempfile::tempdir().unwrap();
    let books = books(dir.path());
    let spec = vm("vm-2", 2).spec;
    let made = BoxRecord::new(spec.clone(), Phase::Creating, Vec::new());
    books.store.begin(&made).unwrap();
    books.store.made(&made).unwrap();
    let end = hex_api::LearnedEnd {
        id: spec.id.clone(),
        held: 7 * GIB,
        budget: 6 * GIB,
        band: 0,
        fleet_held: 15 * GIB,
        fleet_growth: 15 * GIB,
        fleet_budget: 16 * GIB,
        unmoved: Some(hex_api::Upstream::LiveMove),
    };
    let written = end.clone();
    assert!(
        books
            .store
            .update(&spec.id, move |record| {
                record.last_resort = Some(written);
                true
            })
            .unwrap()
    );
    let info = books
        .counted(hex_api::BoxInfo {
            id: spec.id.clone(),
            class: hex_api::Class::Shards,
            state: hex_api::BoxState::Empty,
            bounds: spec.bounds.clone(),
            allocation: spec.allocation,
            adopted: false,
            membership: None,
            exit: Some(hex_api::Exit::Signal(9)),
            abandoned: 0,
            route: None,
            grants: Vec::new(),
            published: Vec::new(),
            last_resort: None,
        })
        .unwrap();
    assert_eq!(info.last_resort, Some(end));
}

/// Joint plan step 14 (AC-T7.4.1), C5's record of why, bounded (CLAUDE.md §2), as every OS's learner keeps it. Do record
/// three ends on a learner for a 20 milli-CPU machine, whose population is two VMs, the third of another class. Expect
/// the class's capacity answer to carry its two latest ends, latest first, and the oldest forgotten once a third came.
#[test]
fn w14_the_last_resorts_ends_are_kept_to_the_machines_population() {
    let mut learner = Learner::new(WINDOW, res(20, 64 * GIB), LEARNS).unwrap();
    let why = |held: u64| charge::Excess {
        held,
        budget: GIB,
        band: 0,
        fleet: charge::Pressure {
            held: 4 * GIB,
            growth: GIB,
            budget: 4 * GIB,
        },
    };
    for (n, class) in [
        (1, Class::Shards),
        (2, Class::Shards),
        (3, Class::Firecracker),
    ] {
        let id = BoxId::new(&format!("vm-{n}")).unwrap();
        learner
            .last
            .record_end(
                class,
                &id,
                Scope::Native([u8::try_from(n).unwrap(); 16]),
                why(n * GIB),
            )
            .unwrap();
    }
    let ids = |class| -> Vec<String> {
        learner
            .learned(class, &[], &[])
            .unwrap()
            .unwrap()
            .ended
            .iter()
            .map(|end| end.id.as_str().to_owned())
            .collect()
    };
    assert_eq!(ids(Class::Shards), ["vm-2"], "vm-1 forgotten past two");
    assert_eq!(ids(Class::Firecracker), ["vm-3"]);
}

/// Three Shards VMs recorded in `store` as a node's create records them, booked at 4 GiB, as the node reads them.
fn recorded_three(store: &Store) -> Vec<BoxRecord> {
    ["vm-1", "vm-2", "vm-3"]
        .iter()
        .zip(1..)
        .map(|(name, n)| {
            let made = BoxRecord::new(vm(name, n).spec, Phase::Creating, Vec::new());
            store.begin(&made).unwrap();
            store.made(&made).unwrap();
            store.get(&made.spec.id).unwrap().unwrap().unwrap()
        })
        .collect()
}

/// Joint plan step 14 (AC-T7.4.1), C5 intent first on macOS and Windows: why survives a node that crashes after a
/// VM's end and before the window's writes. Do record three Shards VMs in a node's store as its create does, run the
/// learner's windows past their fleet's 16 GiB memory budget, vm-2 holding 12 GiB and the others 2 GiB (so the VM past
/// its budget is vm-2 whatever order the store's fresh lifetimes put them in), until it stops vm-2, then crash: leave
/// unmade the writes the window keeps for after it, and reopen the store as the restarted node does. Expect vm-2's
/// record to say why it was ended, its intent written before its stop, and vm-1's and vm-3's records nothing.
#[test]
fn w14_why_survives_a_crash_between_a_capping_nodes_end_and_its_writes() {
    let dir = tempfile::tempdir().unwrap();
    let open = || Store::open(&dir.path().join("state"), &dir.path().join("run")).unwrap();
    let store = open();
    let vms = recorded_three(&store);
    let io = Record::default();
    *io.records.borrow_mut() = Some(store.clone());
    for record in &vms {
        io.set(record.spec.id.as_str(), 0, 0);
    }
    let budget = res(24_000, 16 * GIB);
    let mut learner = Learner::new(WINDOW, res(32_000, 64 * GIB), LEARNS).unwrap();
    let start = Instant::now();
    learner.window(&io, &vms, budget, start).unwrap();
    for (name, held) in [("vm-1", 2 * GIB), ("vm-2", 12 * GIB), ("vm-3", 2 * GIB)] {
        io.set(name, 0, held);
    }
    learner.window(&io, &vms, budget, start + WINDOW).unwrap();
    assert_eq!(*io.stops.borrow(), ["vm-2"]);
    let events = io.events.take();
    // The crash: the learner and its store go with the node, the window's writes unmade.
    drop(learner);
    drop(io);
    drop(store);
    let store = open();
    let end = store
        .get(&vms[1].spec.id)
        .unwrap()
        .unwrap()
        .unwrap()
        .last_resort;
    let end = end.expect("vm-2's why, written before its end");
    assert_eq!((end.id.as_str(), end.held), ("vm-2", 12 * GIB));
    for other in [&vms[0], &vms[2]] {
        let record = store.get(&other.spec.id).unwrap().unwrap().unwrap();
        assert_eq!(record.last_resort, None);
    }
    assert_eq!(events, ["intend vm-2", "stop vm-2"]);
}

/// Joint plan step 14 (AC-T7.4.1), C5 intent first on macOS and Windows: a node that crashed between a VM's intent and
/// its end leaves that end to the next. Do record three Shards VMs in a node's store, vm-2's record naming why it is
/// to be ended as a node that crashed right after writing it leaves it, then run a restarted node's learner over the
/// three, each holding 2 GiB of their 16 GiB fleet, within every budget, then another window. Expect vm-2 stopped in
/// the first window for the why its record names, that why on its class's capacity answer, and no other VM stopped.
#[test]
fn w14_a_restarted_capping_node_carries_out_the_end_its_intent_names() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("state"), &dir.path().join("run")).unwrap();
    let made = recorded_three(&store);
    let scope = Scope::Native(*made[1].native_scope.unwrap().bytes());
    let intent = LearnedEnd {
        id: made[1].spec.id.clone(),
        held: 7 * GIB,
        budget: 6 * GIB,
        band: 0,
        fleet_held: 15 * GIB,
        fleet_growth: 15 * GIB,
        fleet_budget: 16 * GIB,
        unmoved: Some(hex_api::Upstream::LiveMove),
    };
    assert!(record_last_resort(&store, &made[1].spec.id, scope, Some(&intent)).unwrap());
    let vms: Vec<BoxRecord> = made
        .iter()
        .map(|record| store.get(&record.spec.id).unwrap().unwrap().unwrap())
        .collect();
    let io = Record::default();
    for record in &vms {
        io.set(record.spec.id.as_str(), 0, 2 * GIB);
    }
    let budget = res(24_000, 16 * GIB);
    let mut learner = Learner::new(WINDOW, res(32_000, 64 * GIB), LEARNS).unwrap();
    let start = Instant::now();
    learner.window(&io, &vms, budget, start).unwrap();
    assert_eq!(*io.stops.borrow(), ["vm-2"]);
    let learned = learner
        .learned(Class::Shards, &[cost()], &vms)
        .unwrap()
        .unwrap();
    assert_eq!(learned.ended, vec![intent]);
    learner.window(&io, &vms, budget, start + WINDOW).unwrap();
    assert_eq!(io.stops.borrow().len(), 1, "one end is never two");
}

/// Joint plan step 14 (AC-T7.4.1), the learner's bounds (CLAUDE.md §2) on macOS and Windows, as Linux's ([`bounds`]):
/// VMs, its machine's population; classes, as many, the least at which no VM's class is refused. Do run, on a machine
/// of 20 milli-CPU (a population of two), two VMs each of a class of its own; then the first gone and a third of a third
/// class; then a fourth beside those two. Expect the first two held; the third held, the gone VM's class forgotten for
/// it; and the fourth refused, typed, naming the machine's population.
#[test]
fn w14_a_capping_learner_holds_its_machines_population_of_vms_of_as_many_classes() {
    let io = Record::default();
    // Each VM a class of its own: its receipt's profile is its number.
    let own = |name: &str, n: u8| {
        let mut record = vm(name, n);
        if let Some(allocation) = record.spec.allocation.as_mut() {
            allocation.cost.id = [n; 32];
        }
        io.set(name, 0, GIB);
        record
    };
    let (one, two, three, four) = (
        own("vm-1", 1),
        own("vm-2", 2),
        own("vm-3", 3),
        own("vm-4", 4),
    );
    let machine = res(20, 64 * GIB);
    let mut learner = Learner::new(WINDOW, machine, LEARNS).unwrap();
    let start = Instant::now();
    learner
        .window(&io, &[one, two.clone()], machine, start)
        .unwrap();
    learner
        .window(&io, &[two.clone(), three.clone()], machine, start + WINDOW)
        .unwrap();
    assert_eq!(
        learner.window(&io, &[two, three, four], machine, start + WINDOW * 2),
        Err(Refusal::Exhausted {
            what: "a node's VMs",
            bound: 2
        })
    );
}

/// Joint plan step 14 (AC-T7.4.1), C5's queues, bounded (CLAUDE.md §2) at the ends a last resort keeps: the ends a node
/// before this one decided, due, at most one a VM met; the writes into VMs' records not yet made, one a VM lifetime, the
/// oldest forgotten past the bound. Do meet three VMs whose records name intents, and keep an end for each of three
/// VMs, on a last resort keeping two. Expect the third intent refused, typed, naming the bound, and the first VM's write
/// forgotten.
#[test]
fn w14_the_last_resorts_queues_are_kept_to_its_bound() {
    let end = |n: u8| LearnedEnd {
        id: BoxId::new(&format!("vm-{n}")).unwrap(),
        held: 2 * GIB,
        budget: GIB,
        band: 0,
        fleet_held: 4 * GIB,
        fleet_growth: GIB,
        fleet_budget: 4 * GIB,
        unmoved: Some(Upstream::LiveMove),
    };
    let mut last = LastResort::new(2);
    last.met(Class::Shards, Scope::Native([1; 16]), Some(end(1)))
        .unwrap();
    last.met(Class::Shards, Scope::Native([2; 16]), Some(end(2)))
        .unwrap();
    assert_eq!(
        last.met(Class::Shards, Scope::Native([3; 16]), Some(end(3))),
        Err(Refusal::Exhausted {
            what: "the last resort's ends due",
            bound: 2
        })
    );
    for n in 1..=3 {
        last.write(end(n).id, Scope::Native([n; 16]), Some(end(n)))
            .unwrap();
    }
    let kept: Vec<String> = last
        .unrecorded()
        .into_iter()
        .map(|(id, _, _)| id.as_str().to_owned())
        .collect();
    assert_eq!(kept, ["vm-2", "vm-3"]);
}

/// A running Shards image VM's record, booked at 4 CPUs and 4 GiB (its class's prior), its lifetime the `started`th its
/// node began: no native scope, its run its node's.
fn image_vm(name: &str, started: u64) -> BoxRecord {
    let allocation: Allocation = cost()
        .allocate(CapacityResources::default())
        .unwrap()
        .booked_at(res(4_000, 4 * GIB));
    let spec = BoxSpec {
        id: BoxId::new(name).unwrap(),
        bounds: hex_api::Bounds::default(),
        class: ClassSpec::ShardsImage(Box::new(hex_api::ShardsImage::new(hex_api::sim::SIM_IMAGE))),
        owner: None,
        devices: Vec::new(),
        allocation: Some(allocation),
        route: None,
        band: 0,
    };
    let mut record = BoxRecord::new(spec, Phase::Ready, Vec::new());
    record.started = Some(started);
    record
}

/// Joint plan step 14 (C1, C4, C5, C7; AC-T7.4.1): a capping node learns a Shards image VM through its run (D241, D247,
/// D188). Do run a window of an image VM of 16 vCPUs, booked at 4 CPUs and 4 GiB, in a 24-CPU fleet budget; then one in
/// which its VM process ran 3.2 s of CPU in 300 ms, 2.4 s of it its vCPUs' guest time, holding 3 GiB; then one whose
/// run host told 15 vCPUs' guest time. Expect it read through its host use, its lifetime its place in the start order;
/// held at its ceiling's 16 CPUs until it has run, then capped through its rate at the 10.667 CPUs it was charged; its
/// guest told its room, its budget less what it holds, as it moves (64 GiB, then 61 GiB); and the third window refused,
/// typed, its guest time naming a vCPU count other than its reading before's.
#[test]
fn w14_a_capping_node_learns_an_image_vm_through_its_run() {
    let io = Record::default();
    let vm = image_vm("img-1", 1);
    let use_at = |cpu_ns: u64, each_ns: u64, vcpus: u64, held: u64| HostUse {
        cpu_ns,
        footprint_bytes: held,
        resident_bytes: held,
        guest_ns: vec![each_ns; usize::try_from(vcpus).unwrap()],
    };
    io.host
        .borrow_mut()
        .insert("img-1".to_owned(), use_at(0, 0, 16, GIB));
    let budget = res(24_000, 64 * GIB);
    let mut learner = Learner::new(WINDOW, res(32_000, 64 * GIB), LEARNS).unwrap();
    let start = Instant::now();
    learner
        .window(&io, std::slice::from_ref(&vm), budget, start)
        .unwrap();
    assert_eq!(
        *io.rates.borrow(),
        Vec::<(String, u64)>::new(),
        "held at its 16-CPU ceiling until it has run"
    );
    io.host.borrow_mut().insert(
        "img-1".to_owned(),
        use_at(3_200_000_000, 150_000_000, 16, 3 * GIB),
    );
    learner
        .window(&io, std::slice::from_ref(&vm), budget, start + WINDOW)
        .unwrap();
    assert_eq!(*io.rates.borrow(), vec![("img-1".to_owned(), 10_667)]);
    assert_eq!(
        *io.rooms.borrow(),
        vec![
            ("img-1".to_owned(), i64::try_from(64 * GIB).unwrap()),
            ("img-1".to_owned(), i64::try_from(61 * GIB).unwrap()),
        ]
    );
    io.host.borrow_mut().insert(
        "img-1".to_owned(),
        use_at(3_500_000_000, 160_000_000, 15, 3 * GIB),
    );
    assert!(matches!(
        learner.window(&io, &[vm], budget, start + WINDOW * 2),
        Err(Refusal::Invalid { why, .. }) if why.contains("vCPU count")
    ));
}

/// Joint plan step 14 (AC-T7.4.1), C1 with no census (ROOT, 2026-11-13; decision 9): a node whose cost carries no VMM
/// overhead books a cold class at its envelope, says its cold start, and learns the overhead from its windows. Do
/// answer for a Shards profile of a 16-CPU, 8 GiB envelope and no census overhead before any window; then run a window
/// of a VM of it booked at its envelope, and one in which its VMM holds 300 MiB past the envelope and runs 1 CPU.
/// Expect the first answer's ceiling the envelope, every dimension of its overhead cold, typed; then the VM charged its
/// 8 GiB and 300 MiB, past the envelope, and the answer's ceiling carrying 300 MiB of memory overhead and no CPU past
/// the envelope, its unread disk still cold.
#[test]
fn w14_a_census_free_class_starts_cold_and_learns_its_vmm_overhead() {
    let mut record = vm("vm-1", 1);
    record.spec.allocation = Some(cost().allocate(CapacityResources::default()).unwrap());
    let records = std::slice::from_ref(&record);
    let io = Record::default();
    io.set("vm-1", 0, GIB);
    let mut learner = Learner::new(WINDOW, res(32_000, 64 * GIB), LEARNS).unwrap();
    let envelope = res(16_000, 8 * GIB);
    let ceilings = |learner: &Learner| {
        learner
            .learned(Class::Shards, &[cost()], records)
            .unwrap()
            .unwrap()
            .ceilings
    };
    assert_eq!(
        ceilings(&learner),
        vec![hex_api::LearnedCeiling {
            profile: cost().id,
            envelope,
            overhead: hex_api::LearnedOverhead::default(),
        }]
    );
    let start = Instant::now();
    let budget = res(24_000, 64 * GIB);
    learner.window(&io, records, budget, start).unwrap();
    io.set("vm-1", 300_000, 8 * GIB + (300 << 20));
    learner
        .window(&io, records, budget, start + WINDOW)
        .unwrap();
    let lifetime = learner.vms.get(&Scope::Native([1; 16])).unwrap().lifetime;
    assert_eq!(
        learner.node.charge(&lifetime).unwrap().mem_bytes,
        8 * GIB + (300 << 20)
    );
    assert_eq!(
        ceilings(&learner),
        vec![hex_api::LearnedCeiling {
            profile: cost().id,
            envelope,
            overhead: hex_api::LearnedOverhead {
                cpu_milli: Some(0),
                mem_bytes: Some(300 << 20),
                disk_bytes: None,
            },
        }]
    );
}

/// Joint plan step 14 (AC-T7.4.1), C8: a class's capacity answer carries the holding of each VM its learner learns, a
/// Shards image VM's as a managed VM's. Do run a window of a managed VM and an image VM, then answer for the class from
/// the node's records. Expect both VMs' holdings on the answer.
#[test]
fn w14_an_answer_carries_each_learned_vms_holding_an_image_vms_too() {
    let io = Record::default();
    io.set("vm-1", 0, GIB);
    io.host.borrow_mut().insert(
        "img-1".to_owned(),
        HostUse {
            cpu_ns: 0,
            footprint_bytes: GIB,
            resident_bytes: GIB,
            guest_ns: vec![0; 16],
        },
    );
    let records = vec![vm("vm-1", 1), image_vm("img-1", 2)];
    let mut learner = Learner::new(WINDOW, res(32_000, 64 * GIB), LEARNS).unwrap();
    learner
        .window(&io, &records, res(24_000, 64 * GIB), Instant::now())
        .unwrap();
    let mut capacity = hex_api::Capacity {
        id: "shards".into(),
        total: res(32_000, 64 * GIB),
        free: CapacityResources::default(),
        devices: Vec::new(),
        cost: Some(cost()),
        routes: Vec::new(),
        device_routes: Vec::new(),
        backings: Vec::new(),
        unavailable: Vec::new(),
        learned: None,
    };
    learner
        .answer(Class::Shards, &mut capacity, &records)
        .unwrap();
    let mut held: Vec<String> = capacity
        .learned
        .unwrap()
        .holdings
        .iter()
        .map(|holding| holding.id.as_str().to_owned())
        .collect();
    held.sort();
    assert_eq!(held, ["img-1", "vm-1"]);
}

/// Joint plan step 14 (C4; AC-T7.4.1): a window books off the fleet's budget every box it doesn't learn. Do sort a
/// managed VM, a Shards image VM, a process box capped at 2 CPUs and 1 GiB and an ended image VM, for a learner that
/// learns managed VMs alone, then for one that learns both. Expect, alone, the managed VM learned, the image VM booked
/// whole at its 16-CPU, 8 GiB allocation and the box at its caps, the ended VM nowhere; with both, the image VM learned.
#[test]
fn w14_a_window_books_whole_each_vm_it_doesnt_learn() {
    let boxed = BoxRecord::new(
        BoxSpec {
            id: BoxId::new("box-1").unwrap(),
            bounds: hex_api::Bounds {
                cpu_max_milli: Some(2_000),
                memory_max: Some(GIB),
                ..hex_api::Bounds::default()
            },
            class: ClassSpec::Process,
            owner: None,
            devices: Vec::new(),
            allocation: None,
            route: None,
            band: 0,
        },
        Phase::Ready,
        Vec::new(),
    );
    let mut ended = image_vm("img-2", 2);
    ended.exit = Some(hex_api::Exit::Code(0));
    let records = || {
        vec![
            vm("vm-1", 1),
            image_vm("img-1", 1),
            boxed.clone(),
            ended.clone(),
        ]
    };
    let ids = |sorted: &Sorted| -> Vec<String> {
        sorted
            .learned
            .iter()
            .map(|r| r.spec.id.as_str().to_owned())
            .collect()
    };
    let alone = sort(
        records(),
        Learns {
            managed: true,
            images: false,
        },
    )
    .unwrap();
    assert_eq!(ids(&alone), ["vm-1"]);
    assert_eq!(alone.whole, res(16_000, 8 * GIB));
    assert_eq!(alone.native, res(2_000, GIB));
    let both = sort(records(), LEARNS).unwrap();
    assert_eq!(ids(&both), ["vm-1", "img-1"]);
    assert_eq!(both.whole, CapacityResources::default());
    assert_eq!(both.native, res(2_000, GIB));
}
