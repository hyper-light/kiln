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
    let mut learner = Learner::new(WINDOW, res(32_000, 64 * GIB)).unwrap();
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
    let mut learner = Learner::new(WINDOW, res(32_000, 64 * GIB)).unwrap();
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
    let mut learner = Learner::new(WINDOW, res(20, 64 * GIB)).unwrap();
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
            .record_end(class, &id, [u8::try_from(n).unwrap(); 16], why(n * GIB))
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
    let mut learner = Learner::new(WINDOW, res(32_000, 64 * GIB)).unwrap();
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
    let scope = *made[1].native_scope.unwrap().bytes();
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
    let mut learner = Learner::new(WINDOW, res(32_000, 64 * GIB)).unwrap();
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
