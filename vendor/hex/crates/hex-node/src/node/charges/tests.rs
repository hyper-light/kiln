//! The node agent's charge learner (joint plan step 14, AC-T7.4.1; `docs/design/learned-charge.md`).

use std::cell::{Cell, RefCell};

use super::*;
use crate::recorded::charges::{Resort, record_last_resort, with_last_resort};
use hex_api::{LearnedEnd, LearnedPrior, LearnedUse};
use hex_core::record::BoxRecord;
use hex_sim::charge::{SEEDABLE, WEIGHT_MAX};

const GIB: u64 = 1 << 30;

/// A VM's first read: its class, its receipt and its band, no place in a start order nor intent on its record.
fn met(class: Class, allocation: Allocation, band: u8) -> Option<Met> {
    Some(Met {
        class,
        allocation,
        band,
        started: None,
        intent: None,
    })
}

/// A Shards VM's profile: half of a 32-CPU host and 8 GiB, its envelope its floor and ceiling.
fn cost() -> RuntimeCost {
    let envelope = CapacityResources {
        cpu_milli: 16_000,
        mem_bytes: 8 * GIB,
        disk_bytes: 0,
    };
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

fn res(cpu_milli: u64, mem_bytes: u64) -> CapacityResources {
    CapacityResources {
        cpu_milli,
        mem_bytes,
        disk_bytes: 0,
    }
}

fn machine() -> CapacityResources {
    res(32_000, 64 * GIB)
}

/// The cgroups as a record: each VM's use as the test sets it, each cgroup's files, which writes change, and every
/// write by path.
#[derive(Default)]
struct Record {
    usage: RefCell<BTreeMap<PathBuf, Usage>>,
    files: RefCell<BTreeMap<PathBuf, BTreeMap<String, String>>>,
    writes: RefCell<Vec<(PathBuf, &'static str, String)>>,
    /// Every cgroup killed, in order, and whether a kill is refused.
    kills: RefCell<Vec<PathBuf>>,
    refuse_kills: Cell<bool>,
    /// The node's records, where the test keeps them: an intent is written there, as the node writes it.
    records: RefCell<Option<crate::store::Store>>,
    /// Every intent asked and every kill, in order: `intend <box>`, `kill <cgroup>`.
    events: RefCell<Vec<String>>,
}

impl Cgroups for Record {
    fn usage(&self, cgroup: &BoxCgroup) -> Result<Usage, Refusal> {
        self.usage
            .borrow()
            .get(cgroup.dir())
            .cloned()
            .ok_or_else(|| Refusal::Broken("no such cgroup".into()))
    }

    fn write(&self, cgroup: &BoxCgroup, write: &Write) -> Result<(), Refusal> {
        self.files
            .borrow_mut()
            .entry(cgroup.dir().to_path_buf())
            .or_default()
            .insert(write.file.to_owned(), write.value.clone());
        self.writes.borrow_mut().push((
            cgroup.dir().to_path_buf(),
            write.file,
            write.value.clone(),
        ));
        Ok(())
    }

    fn children(&self, dir: &Path) -> Result<Vec<PathBuf>, Refusal> {
        Ok(self
            .files
            .borrow()
            .keys()
            .filter(|path| path.parent() == Some(dir))
            .cloned()
            .collect())
    }

    fn read(&self, dir: &Path, file: &str) -> Result<Option<String>, Refusal> {
        Ok(self
            .files
            .borrow()
            .get(dir)
            .and_then(|files| files.get(file))
            .cloned())
    }

    fn kill(&self, cgroup: &BoxCgroup) -> Result<(), Refusal> {
        if self.refuse_kills.get() {
            return Err(Refusal::Broken("cgroup.kill refused".into()));
        }
        self.kills.borrow_mut().push(cgroup.dir().to_path_buf());
        let name = cgroup.dir().file_name().and_then(|n| n.to_str()).unwrap();
        self.events.borrow_mut().push(format!("kill {name}"));
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
    /// A cgroup at `dir` holding `files`.
    fn dir(&self, dir: &Path, files: &[(&str, &str)]) {
        let mut all = self.files.borrow_mut();
        let entry = all.entry(dir.to_path_buf()).or_default();
        for (file, value) in files {
            entry.insert((*file).to_owned(), (*value).to_owned());
        }
    }

    fn file(&self, dir: &Path, file: &str) -> Option<String> {
        self.read(dir, file).unwrap()
    }

    fn set(&self, dir: &Path, cpu_usec: u64, memory_bytes: u64) {
        self.usage.borrow_mut().insert(
            dir.to_path_buf(),
            Usage {
                cpu_usec,
                memory_bytes,
                ..Usage::default()
            },
        );
    }

    fn take(&self) -> Vec<(PathBuf, &'static str, String)> {
        std::mem::take(&mut *self.writes.borrow_mut())
    }
}

fn root() -> PathBuf {
    PathBuf::from("/sys/fs/cgroup/runner/hex")
}

fn fleet() -> PathBuf {
    root().join(".vm").join("node-a")
}

fn running(ids: &[(&str, u8)]) -> Vec<(BoxId, Scope, BoxCgroup)> {
    ids.iter()
        .map(|(id, n)| {
            (
                BoxId::new(id).unwrap(),
                Scope::Native([*n; 16]),
                BoxCgroup::at(fleet().join(id)),
            )
        })
        .collect()
}

fn whole() -> Allocation {
    cost().allocate(CapacityResources::default()).unwrap()
}

fn shards(_: &BoxId) -> Result<Option<Met>, Refusal> {
    Ok(met(Class::Shards, whole(), 0))
}

/// Joint plan step 14 (AC-T7.4.1), C4 and C5 in the node: the placement's cgroup paths and weights. Do run a window
/// of two cold Shards VMs, each booked at its whole 16-CPU, 8 GiB receipt, on a 32-CPU machine whose fleet budget is
/// half of it, beside another node's fleet budgeted 8 CPUs; then a second window with nothing changed. Expect each
/// VM's own cgroup's `memory.max` its create wrote lifted to the kernel's none, its `memory.swap.max` kept off so all
/// it holds is RAM (ROOT, 2026-11-13: no chosen memory bound; HEX-RT-15), then its weight by booking and its
/// `memory.high` its budget, 32 GiB, half the
/// fleet's (its `cpu.max` left as its create wrote it, its ceiling); `.vm/node-a` capped at 16 CPUs and weighing half
/// the machine; `.vm` weighing both fleets' 24 CPUs of 32; and the second window writing nothing.
#[test]
fn w14_the_learner_places_each_vm_its_fleet_and_vm_by_path_and_weight() {
    let io = Record::default();
    io.dir(&fleet(), &[]);
    io.dir(
        &root().join(".vm").join("node-b"),
        &[("cpu.max", "800000 100000")],
    );
    let vms = running(&[("vm-1", 1), ("vm-2", 2)]);
    for (_, _, cgroup) in &vms {
        io.set(cgroup.dir(), 0, GIB);
    }
    let mut learner = Learner::new(&fleet(), machine()).unwrap();
    let start = Instant::now();
    learner
        .window(&io, vms, shards, &[], res(16_000, 64 * GIB), start)
        .unwrap();
    let vm = |id: &str, file: &'static str, value: &str| (fleet().join(id), file, value.to_owned());
    assert_eq!(
        io.take(),
        vec![
            vm("vm-1", "memory.max", "max"),
            vm("vm-1", "cpu.weight", "10000"),
            vm("vm-1", "memory.high", &(32 * GIB).to_string()),
            vm("vm-2", "memory.max", "max"),
            vm("vm-2", "cpu.weight", "10000"),
            vm("vm-2", "memory.high", &(32 * GIB).to_string()),
            (fleet(), "cpu.weight", "5000".to_owned()),
            (fleet(), "cpu.max", "1600000 100000".to_owned()),
            (root().join(".vm"), "cpu.weight", "7500".to_owned()),
        ]
    );
    learner
        .window(
            &io,
            running(&[("vm-1", 1), ("vm-2", 2)]),
            shards,
            &[],
            res(16_000, 64 * GIB),
            start + WINDOW,
        )
        .unwrap();
    assert_eq!(io.take(), Vec::new(), "nothing changed, nothing written");
}

/// Joint plan step 14 (AC-T7.4.1), C7: a Shards guest can't yet be told its room, though its ring pins keep within any
/// room it is told (Shards' ring-pins-v1, D230, vendored at 66113379). Do run a window of a Shards VM and a Firecracker
/// VM; expect the Shards guest's room refused as awaiting `GuestRoom` alone, its being told it, the Firecracker one's
/// nothing.
#[test]
fn w14_a_shards_guests_room_awaits_guest_room_alone() {
    let io = Record::default();
    let vms = running(&[("vm-1", 1), ("vm-2", 2)]);
    for (_, _, cgroup) in &vms {
        io.set(cgroup.dir(), 0, GIB);
    }
    let mut learner = Learner::new(&fleet(), machine()).unwrap();
    let record = |id: &BoxId| -> Result<Option<Met>, Refusal> {
        let class = if id.as_str() == "vm-1" {
            Class::Shards
        } else {
            Class::Firecracker
        };
        Ok(met(class, whole(), 0))
    };
    learner
        .window(&io, vms, record, &[], machine(), Instant::now())
        .unwrap();
    let vm1 = BoxId::new("vm-1").unwrap();
    assert_eq!(learner.awaits(), vec![(&vm1, Upstream::GuestRoom)]);
}

/// Joint plan step 14 (AC-T7.4.1), C2 and C8 in the node: what a capacity answer carries. Do warm one VM, booked at
/// its whole receipt, at a tenth of a CPU and 1 GiB for SEEDABLE windows, and admit a second at the class's prior
/// that then holds 2 GiB; then end the second; then fail a window. Expect the prior its learned tail; the second
/// VM's holding first, past its receipt by a GiB; the use summing receipts and holdings; the ended VM gone from
/// them; and nothing learned carried after the failed window, until a window passes.
#[test]
fn w14_a_capacity_answer_carries_priors_use_and_holdings_most_past_first() {
    let io = Record::default();
    let first = running(&[("vm-1", 1)]);
    let dir = first[0].2.dir().to_path_buf();
    let mut learner = Learner::new(&fleet(), machine()).unwrap();
    let start = Instant::now();
    for t in 0..=u64::try_from(SEEDABLE).unwrap() {
        io.set(&dir, t * 10_000, GIB);
        let at = start + WINDOW * u32::try_from(t).unwrap();
        learner
            .window(&io, running(&[("vm-1", 1)]), shards, &[], machine(), at)
            .unwrap();
    }
    let learned = learner.learned(Class::Shards, &[cost()]).unwrap().unwrap();
    let prior = res(100, GIB);
    assert_eq!(
        learned.priors,
        vec![LearnedPrior {
            profile: [14; 32],
            ceiling: res(16_000, 8 * GIB),
            booking: prior,
        }]
    );
    let booked = whole().booked_at(prior);
    assert_eq!(booked.booking(), prior);
    let both = running(&[("vm-1", 1), ("vm-2", 2)]);
    io.set(both[1].2.dir(), 0, 2 * GIB);
    let record = move |id: &BoxId| -> Result<Option<Met>, Refusal> {
        let allocation = if id.as_str() == "vm-2" {
            booked
        } else {
            whole()
        };
        Ok(met(Class::Shards, allocation, 0))
    };
    let later = start + WINDOW * 2_000;
    learner
        .window(&io, both, record, &[], machine(), later)
        .unwrap();
    learner
        .window(
            &io,
            running(&[("vm-1", 1), ("vm-2", 2)]),
            record,
            &[],
            machine(),
            later + WINDOW,
        )
        .unwrap();
    let learned = learner.learned(Class::Shards, &[cost()]).unwrap().unwrap();
    let ids: Vec<&str> = learned.holdings.iter().map(|h| h.id.as_str()).collect();
    assert_eq!(ids, ["vm-2", "vm-1"], "most past its receipt first");
    assert_eq!(learned.holdings[0].holding.mem_bytes, 2 * GIB);
    assert_eq!(
        learned.used,
        LearnedUse {
            booked: res(16_000 + 100, 8 * GIB + GIB),
            reservation: res(100 + 100, GIB + 2 * GIB),
        }
    );
    learner
        .window(
            &io,
            running(&[("vm-1", 1)]),
            record,
            &[],
            machine(),
            later + WINDOW * 2,
        )
        .unwrap();
    let learned = learner.learned(Class::Shards, &[cost()]).unwrap().unwrap();
    assert_eq!(learned.holdings.len(), 1);
    assert_eq!(learned.used.booked, res(16_000, 8 * GIB));
    learner.failed(Refusal::Broken("a read failed".into()));
    assert_eq!(learner.learned(Class::Shards, &[cost()]).unwrap(), None);
    learner
        .window(
            &io,
            running(&[("vm-1", 1)]),
            record,
            &[],
            machine(),
            later + WINDOW * 3,
        )
        .unwrap();
    assert!(learner.learned(Class::Shards, &[cost()]).unwrap().is_some());
}

/// The node root of the one-scale tests: a process box capped at 4 CPUs and 4 GiB, one with no cap, and under `.oci` a
/// box capped at 2 CPUs, each still at its owner's weight, beside a fleet of one VM; the VM's running list.
fn boxes_beside_a_fleet(io: &Record) -> Vec<(BoxId, Scope, BoxCgroup)> {
    let gib4 = (4 * GIB).to_string();
    io.dir(
        &root().join("box-a"),
        &[
            ("cpu.max", "400000 100000"),
            ("memory.max", &gib4),
            ("cpu.weight", "100"),
        ],
    );
    io.dir(
        &root().join("box-b"),
        &[
            ("cpu.max", "max 100000"),
            ("memory.max", "max"),
            ("cpu.weight", "100"),
        ],
    );
    io.dir(&root().join(".oci"), &[("cpu.weight", "100")]);
    io.dir(
        &root().join(".oci").join("box-c"),
        &[
            ("cpu.max", "200000 100000"),
            ("memory.max", "max"),
            ("cpu.weight", "100"),
        ],
    );
    io.dir(&root().join(".vm"), &[("cpu.weight", "100")]);
    io.dir(&fleet(), &[]);
    let vms = running(&[("vm-1", 1)]);
    io.set(vms[0].2.dir(), 0, GIB);
    vms
}

/// A cgroup's `cpu.max` in milli-CPU, as the learner wrote it.
fn cap_of(io: &Record, dir: &Path) -> u64 {
    let max = io.file(dir, "cpu.max").unwrap();
    let parsed = CpuMax::parse(&max).unwrap();
    parsed.quota_us.unwrap() * 1_000 / parsed.period_us
}

/// Joint plan step 14 (AC-T7.4.1), C4 in the node: every child of the node's cgroup under contention gets at least
/// min(its need, its booking) by the weights the learner wrote (ROOT, 2026-11-12). Do run a window on a 32-CPU machine
/// whose node root holds a process box capped at 4 CPUs, one with no cap, and under `.oci` a box capped at 2 CPUs,
/// beside a fleet of one VM; then share the machine among the root's children by the `cpu.weight`s written, as the
/// kernel does (`charge::shares`, cgroup-v2.rst "Weights"), for every set of them asking the whole machine, the fleet
/// no more than its `cpu.max`. Expect each asking box given at least its booking, `.oci` its box's, and the fleet its
/// budget, its `cpu.max`.
#[test]
fn w14_a_box_under_contention_gets_its_booking_by_the_weights_written() {
    let io = Record::default();
    let vms = boxes_beside_a_fleet(&io);
    let whole = machine().cpu_milli;
    let mut learner = Learner::new(&fleet(), machine()).unwrap();
    learner
        .window(&io, vms, shards, &[], machine(), Instant::now())
        .unwrap();
    let budget = cap_of(&io, &fleet());
    let children = [
        (root().join("box-a"), 4_000, whole),
        (root().join("box-b"), 0, whole),
        (root().join(".oci"), 2_000, whole),
        (root().join(".vm"), budget, budget),
    ];
    let weight = |dir: &Path| io.file(dir, "cpu.weight").unwrap().parse::<u64>().unwrap();
    for asking in 1u32..16 {
        let asks = |k: usize| asking & (1 << k) != 0;
        let entries: Vec<(u64, u64)> = children
            .iter()
            .enumerate()
            .map(|(k, (dir, _, need))| (weight(dir), if asks(k) { *need } else { 0 }))
            .collect();
        let given = charge::shares(whole, &entries).unwrap();
        for (k, ((dir, booking, need), got)) in children.iter().zip(&given).enumerate() {
            if asks(k) {
                assert!(
                    *got >= (*booking).min(*need),
                    "{} given {got} of its booking {booking} (asking {asking:04b}, weights {entries:?})",
                    dir.display()
                );
            }
        }
    }
}

/// Joint plan step 14 (AC-T7.4.1), C4 in the node: a reuse slot no box holds books nothing (ROOT, 2026-11-12). Do run a
/// window on a 32-CPU machine whose `.oci` holds slot 7, idle with the 2-CPU, 2 GiB caps and the weight its last box
/// left (a slot given back keeps them: `Cgroups::give_back` writes no bound), and slot 8 held by a box capped at 1 CPU,
/// beside a fleet of one VM. Expect slot 8 weighed by its box's booking and `.oci` by it alone, slot 7 at the least
/// weight, and the fleet's budget the CPU the units slot 8's box leaves carry.
#[test]
fn w14_an_idle_slot_books_nothing() {
    let io = Record::default();
    io.dir(&root().join(".oci"), &[("cpu.weight", "100")]);
    io.dir(
        &root().join(".oci").join(".7"),
        &[
            ("cpu.max", "200000 100000"),
            ("memory.max", &(2 * GIB).to_string()),
            ("cpu.weight", "625"),
        ],
    );
    io.dir(
        &root().join(".oci").join(".8"),
        &[
            ("cpu.max", "100000 100000"),
            ("memory.max", &GIB.to_string()),
            ("cpu.weight", "100"),
        ],
    );
    io.dir(&root().join(".vm"), &[("cpu.weight", "100")]);
    io.dir(&fleet(), &[]);
    let vms = running(&[("vm-1", 1)]);
    io.set(vms[0].2.dir(), 0, GIB);
    let mut learner = Learner::new(&fleet(), machine()).unwrap();
    learner
        .window(&io, vms, shards, &[8], machine(), Instant::now())
        .unwrap();
    let weight = |dir: &Path| io.file(dir, "cpu.weight");
    // Slot 8's box: ceil(10,000 x 1,000 / 32,000) = 313 units; the fleet the 9,687 left, 30,998.4 milli-CPU, floored.
    assert_eq!(
        weight(&root().join(".oci").join(".8")).as_deref(),
        Some("313")
    );
    assert_eq!(weight(&root().join(".oci")).as_deref(), Some("313"));
    assert_eq!(
        weight(&root().join(".oci").join(".7")).as_deref(),
        Some("1")
    );
    assert_eq!(cap_of(&io, &fleet()), 30_998);
    assert_eq!(weight(&root().join(".vm")).as_deref(), Some("9687"));
}

/// Joint plan step 14 (AC-T7.4.1), C4 in the node: every child of the node's cgroup on one scale (ROOT, 2026-11-12). Do
/// run a window on a 32-CPU, 64 GiB machine whose node root holds a process box capped at 4 CPUs and 4 GiB, one with
/// no cap, and under `.oci` a box capped at 2 CPUs, beside a fleet of one VM. Expect each box weighed by its booking
/// (its caps until learned), the uncapped one booked nothing and so at the least weight, `.oci` by its boxes'
/// weights together, the fleet the units the boxes leave (8,124 of 10,000), its budget the CPU they carry rounded down
/// (25,996 milli-CPU, within the machine less the boxes' bookings) with its cap and weight and `.vm`'s following it,
/// and the node root's children weighing the scale's top exactly.
#[test]
fn w14_every_child_of_the_nodes_cgroup_weighs_on_one_scale() {
    let io = Record::default();
    let vms = boxes_beside_a_fleet(&io);
    let mut learner = Learner::new(&fleet(), machine()).unwrap();
    learner
        .window(&io, vms, shards, &[], machine(), Instant::now())
        .unwrap();
    let weight = |dir: &Path| io.file(dir, "cpu.weight");
    assert_eq!(weight(&root().join("box-a")).as_deref(), Some("1250"));
    assert_eq!(weight(&root().join("box-b")).as_deref(), Some("1"));
    assert_eq!(
        weight(&root().join(".oci").join("box-c")).as_deref(),
        Some("625")
    );
    assert_eq!(weight(&root().join(".oci")).as_deref(), Some("625"));
    assert_eq!(
        io.file(&fleet(), "cpu.max").as_deref(),
        Some("2599600 100000")
    );
    assert_eq!(weight(&fleet()).as_deref(), Some("8124"));
    assert_eq!(weight(&root().join(".vm")).as_deref(), Some("8124"));
    let siblings: u64 = io
        .children(&root())
        .unwrap()
        .iter()
        .map(|dir| weight(dir).unwrap().parse::<u64>().unwrap())
        .sum();
    assert_eq!(siblings, u64::from(WEIGHT_MAX));
}

/// Three Shards VMs of 16 CPUs and 8 GiB, each booked at 4 GiB of memory (their class's prior), in a fleet whose
/// memory budget is 16 GiB, so each VM's budget is its booking and a third of the 4 GiB slack: what `running` lists.
fn booked_at_four(_: &BoxId) -> Result<Option<Met>, Refusal> {
    Ok(met(
        Class::Shards,
        whole().booked_at(res(16_000, 4 * GIB)),
        0,
    ))
}

fn fleet_of_three() -> Vec<(BoxId, Scope, BoxCgroup)> {
    running(&[("vm-1", 1), ("vm-2", 2), ("vm-3", 3)])
}

/// Joint plan step 14 (AC-T7.4.1), C5's last resort in the node agent. Do run a window of three VMs, each booked at
/// 4 GiB in a 16 GiB fleet, then one where they hold 6, 7 and 2 GiB, past the fleet's budget with the growth on
/// record; then another with all still running; then one with vm-2 gone. Expect vm-2, the VM holding most past its own
/// budget, its cgroup killed and the state `Ending` with why (its 7 GiB against its budget, the fleet's 15 GiB, 15 GiB
/// of growth, its 16 GiB budget), the same why on the class's capacity answer and none on another class's; no second
/// kill while vm-2 runs; and with it gone, vm-1 within its new budget and vm-3 within its own, nothing more killed and
/// the state refused, typed: every VM within its budget.
#[test]
fn w14_the_last_resort_ends_the_vm_most_past_its_budget_and_no_other() {
    let io = Record::default();
    let fleet_budget = res(32_000, 16 * GIB);
    let mut learner = Learner::new(&fleet(), machine()).unwrap();
    let start = Instant::now();
    let vms = fleet_of_three();
    for (_, _, cgroup) in &vms {
        io.set(cgroup.dir(), 0, 0);
    }
    learner
        .window(&io, vms, booked_at_four, &[], fleet_budget, start)
        .unwrap();
    assert_eq!(learner.resort(), &Resort::Held);
    for (id, held) in [("vm-1", 6 * GIB), ("vm-2", 7 * GIB), ("vm-3", 2 * GIB)] {
        io.set(&fleet().join(id), 0, held);
    }
    learner
        .window(
            &io,
            fleet_of_three(),
            booked_at_four,
            &[],
            fleet_budget,
            start + WINDOW,
        )
        .unwrap();
    assert_eq!(*io.kills.borrow(), vec![fleet().join("vm-2")]);
    let Resort::Ending { id, why, .. } = learner.resort() else {
        panic!("not ending: {:?}", learner.resort());
    };
    assert_eq!(id.as_str(), "vm-2");
    assert_eq!(why.held, 7 * GIB);
    assert!(why.budget < why.held, "{why:?}");
    assert_eq!(
        why.fleet,
        charge::Pressure {
            held: 15 * GIB,
            growth: 15 * GIB,
            budget: 16 * GIB,
        }
    );
    let why = *why;
    let learned = learner.learned(Class::Shards, &[cost()]).unwrap().unwrap();
    assert_eq!(
        learned.ended,
        vec![LearnedEnd {
            id: BoxId::new("vm-2").unwrap(),
            held: 7 * GIB,
            budget: why.budget,
            band: 0,
            fleet_held: 15 * GIB,
            fleet_growth: 15 * GIB,
            fleet_budget: 16 * GIB,
            unmoved: Some(Upstream::LiveMove),
        }]
    );
    let other = learner.learned(Class::Firecracker, &[]).unwrap().unwrap();
    assert!(other.ended.is_empty());
    learner
        .window(
            &io,
            fleet_of_three(),
            booked_at_four,
            &[],
            fleet_budget,
            start + WINDOW * 2,
        )
        .unwrap();
    assert_eq!(io.kills.borrow().len(), 1, "one end is never two");
    learner
        .window(
            &io,
            running(&[("vm-1", 1), ("vm-3", 3)]),
            booked_at_four,
            &[],
            fleet_budget,
            start + WINDOW * 3,
        )
        .unwrap();
    assert_eq!(io.kills.borrow().len(), 1, "never a VM within its budget");
    assert!(
        matches!(learner.resort(), Resort::Refused(Refusal::Exhausted { .. })),
        "{:?}",
        learner.resort()
    );
}

/// Joint plan step 14 (AC-T7.4.1), C3 and C5 in the node agent: each VM in the band its record names. Do run a window
/// of three VMs booked at 4 GiB in a 16 GiB fleet, vm-1 in band 2, vm-2 in band 3 and vm-3 in band 1, then one in
/// which each holds its whole 8 GiB. Expect the fleet's 4 GiB of room raising vm-2's booking, the highest band's, to its
/// 8 GiB, so vm-1 and vm-3 are each 4 GiB past their budgets; vm-3, the lower band of the two, ended; and its end
/// saying band 1.
#[test]
fn w14_the_last_resort_ends_the_lowest_band_first() {
    let banded = |id: &BoxId| -> Result<Option<Met>, Refusal> {
        let band = match id.as_str() {
            "vm-1" => 2,
            "vm-2" => 3,
            _ => 1,
        };
        Ok(met(
            Class::Shards,
            whole().booked_at(res(16_000, 4 * GIB)),
            band,
        ))
    };
    let io = Record::default();
    let fleet_budget = res(32_000, 16 * GIB);
    let mut learner = Learner::new(&fleet(), machine()).unwrap();
    let start = Instant::now();
    for (_, _, cgroup) in &fleet_of_three() {
        io.set(cgroup.dir(), 0, 0);
    }
    learner
        .window(&io, fleet_of_three(), banded, &[], fleet_budget, start)
        .unwrap();
    for id in ["vm-1", "vm-2", "vm-3"] {
        io.set(&fleet().join(id), 0, 8 * GIB);
    }
    learner
        .window(
            &io,
            fleet_of_three(),
            banded,
            &[],
            fleet_budget,
            start + WINDOW,
        )
        .unwrap();
    assert_eq!(*io.kills.borrow(), vec![fleet().join("vm-3")]);
    let learned = learner.learned(Class::Shards, &[cost()]).unwrap().unwrap();
    let ends: Vec<_> = learned
        .ended
        .iter()
        .map(|end| (end.id.as_str(), end.band, end.held, end.budget))
        .collect();
    assert_eq!(ends, [("vm-3", 1, 8 * GIB, 4 * GIB)]);
}

/// Joint plan step 14 (AC-T7.4.1), C3's order in the node agent (ROOT, 2026-11-12): of two VMs rising past their
/// bookings into the same room, the one begun later is over and the last resort ends it, whatever their scopes. Do run a
/// window of three VMs booked at 4 GiB in a 16 GiB fleet, begun first, second and third on the node (their records' places
/// in its start order), their scopes in the opposite order (the first's the largest), then one where the first two hold
/// 7 GiB each and the third 2 GiB. Expect the fleet's 4 GiB of room to the first (its budget 7 GiB) and the rest, 1 GiB,
/// to the second (its budget 5 GiB), so the second, 2 GiB past its budget, its cgroup killed and no other; and its why
/// on the class's capacity answer naming it: 7 GiB held against its 5 GiB budget, the fleet's 16 GiB held, 16 GiB of
/// growth, its 16 GiB budget.
#[test]
fn w14_of_two_vms_rising_into_one_room_the_later_begun_is_ended() {
    let begun = |id: &BoxId| -> Result<Option<Met>, Refusal> {
        let started = match id.as_str() {
            "vm-first" => 1,
            "vm-second" => 2,
            _ => 3,
        };
        Ok(Some(Met {
            started: Some(started),
            ..booked_at_four(id)?.unwrap()
        }))
    };
    let vms = || running(&[("vm-first", 9), ("vm-second", 5), ("vm-third", 1)]);
    let io = Record::default();
    for (_, _, cgroup) in &vms() {
        io.set(cgroup.dir(), 0, 0);
    }
    let fleet_budget = res(32_000, 16 * GIB);
    let mut learner = Learner::new(&fleet(), machine()).unwrap();
    let start = Instant::now();
    learner
        .window(&io, vms(), begun, &[], fleet_budget, start)
        .unwrap();
    for (id, held) in [
        ("vm-first", 7 * GIB),
        ("vm-second", 7 * GIB),
        ("vm-third", 2 * GIB),
    ] {
        io.set(&fleet().join(id), 0, held);
    }
    learner
        .window(&io, vms(), begun, &[], fleet_budget, start + WINDOW)
        .unwrap();
    assert_eq!(*io.kills.borrow(), vec![fleet().join("vm-second")]);
    let learned = learner.learned(Class::Shards, &[cost()]).unwrap().unwrap();
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
            unmoved: Some(Upstream::LiveMove),
        }]
    );
}

/// Joint plan step 14 (AC-T7.4.1), C5's last resort where the node can't act. Do run a window of three VMs booked at
/// 4 GiB in a 16 GiB fleet, then one past its budget whose kill the cgroup refuses. Expect no end taken for done, and
/// the state the kill's refusal, typed.
#[test]
fn w14_a_refused_end_is_kept_typed() {
    let io = Record::default();
    let fleet_budget = res(32_000, 16 * GIB);
    let mut learner = Learner::new(&fleet(), machine()).unwrap();
    let start = Instant::now();
    for (_, _, cgroup) in &fleet_of_three() {
        io.set(cgroup.dir(), 0, 0);
    }
    learner
        .window(
            &io,
            fleet_of_three(),
            booked_at_four,
            &[],
            fleet_budget,
            start,
        )
        .unwrap();
    for (id, held) in [("vm-1", 6 * GIB), ("vm-2", 7 * GIB), ("vm-3", 2 * GIB)] {
        io.set(&fleet().join(id), 0, held);
    }
    io.refuse_kills.set(true);
    learner
        .window(
            &io,
            fleet_of_three(),
            booked_at_four,
            &[],
            fleet_budget,
            start + WINDOW,
        )
        .unwrap();
    assert!(io.kills.borrow().is_empty());
    assert_eq!(
        learner.resort(),
        &Resort::Refused(Refusal::Broken("cgroup.kill refused".into()))
    );
}

/// A Shards VM's spec of `cost()`'s 16 CPUs and 8 GiB, as a node records it.
fn shards_spec(id: &BoxId) -> hex_api::BoxSpec {
    hex_api::BoxSpec {
        id: id.clone(),
        bounds: Bounds {
            cpu_max_milli: Some(16_000),
            memory_max: Some(8 * GIB),
            ..Bounds::default()
        },
        class: hex_api::ClassSpec::Shards(Box::new(hex_api::OciSpec {
            rootfs: hex_api::Rootfs::Dir(PathBuf::from("/images/agent")),
            init: hex_api::Program::of(&["/bin/true"]),
            mounts: Vec::new(),
            hostname: None,
            network: hex_api::Network::Private,
            hosts: Vec::new(),
        })),
        owner: None,
        devices: Vec::new(),
        allocation: Some(whole()),
        route: None,
        band: 0,
    }
}

/// Three Shards VMs recorded in `store` as a node's create records them, and their running list.
fn recorded_three(store: &crate::store::Store) -> Vec<(BoxId, Scope, BoxCgroup)> {
    let mut vms = Vec::new();
    for name in ["vm-1", "vm-2", "vm-3"] {
        let id = BoxId::new(name).unwrap();
        let made = BoxRecord::new(
            shards_spec(&id),
            hex_core::record::Phase::Creating,
            Vec::new(),
        );
        store.begin(&made).unwrap();
        store.made(&made).unwrap();
        let record = store.get(&id).unwrap().unwrap().unwrap();
        let scope = Scope::Native(*record.native_scope.unwrap().bytes());
        vms.push((id, scope, BoxCgroup::at(fleet().join(name))));
    }
    vms
}

/// Joint plan step 14 (AC-T7.4.1), C5 intent first: why survives a node that crashes after a VM's end and before the
/// window's writes. Do record three Shards VMs in a node's store as its create does, run the learner's windows past
/// their fleet's 16 GiB memory budget, vm-2 holding 12 GiB, until it ends vm-2, then crash: leave unmade the writes
/// the window keeps for after it, and reopen the store as the restarted node does. Expect vm-2's record to say why it
/// was ended, its intent written before its cgroup's kill, and vm-1's and vm-3's records to say nothing.
#[test]
fn w14_why_survives_a_crash_between_the_end_and_the_windows_writes() {
    use crate::store::Store;
    let dir = tempfile::tempdir().unwrap();
    let open = || Store::open(&dir.path().join("state"), &dir.path().join("run")).unwrap();
    let store = open();
    let vms = recorded_three(&store);
    let io = Record::default();
    *io.records.borrow_mut() = Some(store.clone());
    for (_, _, cgroup) in &vms {
        io.set(cgroup.dir(), 0, 0);
    }
    let fleet_budget = res(32_000, 16 * GIB);
    let mut learner = Learner::new(&fleet(), machine()).unwrap();
    let start = Instant::now();
    learner
        .window(&io, vms.clone(), booked_at_four, &[], fleet_budget, start)
        .unwrap();
    for (name, held) in [("vm-1", 2 * GIB), ("vm-2", 12 * GIB), ("vm-3", 2 * GIB)] {
        io.set(&fleet().join(name), 0, held);
    }
    learner
        .window(
            &io,
            vms.clone(),
            booked_at_four,
            &[],
            fleet_budget,
            start + WINDOW,
        )
        .unwrap();
    assert_eq!(*io.kills.borrow(), vec![fleet().join("vm-2")]);
    let events = io.events.take();
    // The crash: the learner and its store go with the node, the window's writes unmade.
    drop(learner);
    drop(io);
    drop(store);
    let store = open();
    let end = store.get(&vms[1].0).unwrap().unwrap().unwrap().last_resort;
    let end = end.expect("vm-2's why, written before its end");
    assert_eq!(
        (end.id.as_str(), end.held, end.budget),
        ("vm-2", 12 * GIB, 8 * GIB)
    );
    for other in [&vms[0].0, &vms[2].0] {
        assert_eq!(
            store.get(other).unwrap().unwrap().unwrap().last_resort,
            None
        );
    }
    assert_eq!(events, ["intend vm-2", "kill vm-2"]);
}

/// Joint plan step 14 (AC-T7.4.1), C5 intent first: a node that crashed between a VM's intent and its end leaves that
/// end to the next. Do record three Shards VMs in a node's store, vm-2's record naming why it is to be ended as a
/// node that crashed right after writing it leaves it, then run a restarted node's learner over the three, each
/// holding 2 GiB of their 16 GiB fleet, within every budget, then another window. Expect vm-2 ended in the first window
/// for the why its record names, that why on its class's capacity answer, and no other VM ended.
#[test]
fn w14_a_restarted_node_carries_out_the_end_its_intent_names() {
    use crate::store::Store;
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("state"), &dir.path().join("run")).unwrap();
    let vms = recorded_three(&store);
    let intent = LearnedEnd {
        id: vms[1].0.clone(),
        held: 12 * GIB,
        budget: 8 * GIB,
        band: 0,
        fleet_held: 16 * GIB,
        fleet_growth: 16 * GIB,
        fleet_budget: 16 * GIB,
        unmoved: Some(Upstream::LiveMove),
    };
    assert!(record_last_resort(&store, &vms[1].0, vms[1].1, Some(&intent)).unwrap());
    let record = |id: &BoxId| -> Result<Option<Met>, Refusal> {
        let recorded = store.get(id).unwrap().unwrap().unwrap();
        Ok(Some(Met {
            intent: recorded.last_resort,
            ..met(Class::Shards, whole().booked_at(res(16_000, 4 * GIB)), 0).unwrap()
        }))
    };
    let io = Record::default();
    for (_, _, cgroup) in &vms {
        io.set(cgroup.dir(), 0, 2 * GIB);
    }
    let fleet_budget = res(32_000, 16 * GIB);
    let mut learner = Learner::new(&fleet(), machine()).unwrap();
    let start = Instant::now();
    learner
        .window(&io, vms.clone(), record, &[], fleet_budget, start)
        .unwrap();
    assert_eq!(*io.kills.borrow(), vec![fleet().join("vm-2")]);
    assert!(
        matches!(learner.resort(), Resort::Ending { .. }),
        "{:?}",
        learner.resort()
    );
    let learned = learner.learned(Class::Shards, &[cost()]).unwrap().unwrap();
    assert_eq!(learned.ended, vec![intent]);
    learner
        .window(&io, vms, record, &[], fleet_budget, start + WINDOW)
        .unwrap();
    assert_eq!(io.kills.borrow().len(), 1, "one end is never two");
}

/// Joint plan step 14 (AC-T7.4.1), C5 intent first: an end refused withdraws its intent, so no record says a VM the node
/// couldn't end was ended. Do record three Shards VMs in a node's store, refuse their cgroups' kills, run the learner's
/// windows past their fleet's 16 GiB memory budget, vm-2 holding 12 GiB, then make the window's writes as the node does
/// after it. Expect vm-2's intent asked and its kill refused, the state that refusal, typed, and vm-2's record naming
/// no end.
#[test]
fn w14_a_refused_end_withdraws_its_intent() {
    use crate::store::Store;
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("state"), &dir.path().join("run")).unwrap();
    let vms = recorded_three(&store);
    let io = Record::default();
    *io.records.borrow_mut() = Some(store.clone());
    io.refuse_kills.set(true);
    for (_, _, cgroup) in &vms {
        io.set(cgroup.dir(), 0, 0);
    }
    let fleet_budget = res(32_000, 16 * GIB);
    let mut learner = Learner::new(&fleet(), machine()).unwrap();
    let start = Instant::now();
    learner
        .window(&io, vms.clone(), booked_at_four, &[], fleet_budget, start)
        .unwrap();
    for (name, held) in [("vm-1", 2 * GIB), ("vm-2", 12 * GIB), ("vm-3", 2 * GIB)] {
        io.set(&fleet().join(name), 0, held);
    }
    learner
        .window(
            &io,
            vms.clone(),
            booked_at_four,
            &[],
            fleet_budget,
            start + WINDOW,
        )
        .unwrap();
    assert_eq!(*io.events.borrow(), ["intend vm-2"]);
    assert_eq!(
        learner.resort(),
        &Resort::Refused(Refusal::Broken("cgroup.kill refused".into()))
    );
    let (left, failed) = write_ends(&store, learner.unrecorded());
    assert!(left.is_empty() && failed.is_none(), "{left:?} {failed:?}");
    let record = store.get(&vms[1].0).unwrap().unwrap().unwrap();
    assert_eq!(record.last_resort, None, "its intent withdrawn");
}

/// Joint plan step 14 (AC-T7.4.1), C5 on the box itself (ROOT, 2026-11-12): a last-resort end shows in inspect and
/// survives a restart. Do record three Shards VMs in a node's store as its create does, run the learner's windows past
/// their fleet's 16 GiB memory budget, vm-2 holding 12 GiB, until it ends vm-2, write the ends it hasn't recorded into
/// their records as the node does after each window, reopen the store as a restarted node would, build vm-2's inspect
/// from its backend's info and its record, and read that through the node's wire; then write the end against a
/// lifetime vm-2's record isn't. Expect vm-2's record to say why after the reopen, its inspect and the wire's to say
/// the same, vm-1's and vm-3's none, no end left unrecorded, and the end for another lifetime not written.
#[test]
fn w14_a_last_resort_end_shows_in_inspect_and_survives_a_restart() {
    use crate::store::Store;
    use hex_core::record::Phase;
    let dir = tempfile::tempdir().unwrap();
    let open = || Store::open(&dir.path().join("state"), &dir.path().join("run")).unwrap();
    let store = open();
    let mut vms = Vec::new();
    for name in ["vm-1", "vm-2", "vm-3"] {
        let id = BoxId::new(name).unwrap();
        let made = BoxRecord::new(shards_spec(&id), Phase::Creating, Vec::new());
        store.begin(&made).unwrap();
        store.made(&made).unwrap();
        let record = store.get(&id).unwrap().unwrap().unwrap();
        let scope = Scope::Native(*record.native_scope.unwrap().bytes());
        vms.push((id, scope, BoxCgroup::at(fleet().join(name))));
    }
    let io = Record::default();
    for (_, _, cgroup) in &vms {
        io.set(cgroup.dir(), 0, 0);
    }
    let fleet_budget = res(32_000, 16 * GIB);
    let mut learner = Learner::new(&fleet(), machine()).unwrap();
    let start = Instant::now();
    learner
        .window(&io, vms.clone(), booked_at_four, &[], fleet_budget, start)
        .unwrap();
    // vm-2 alone passes its budget whatever order the VMs' scopes put them in: its charge is its 8 GiB ceiling, which
    // the 4 GiB of room raise its booking to, and it holds 12.
    for (name, held) in [("vm-1", 2 * GIB), ("vm-2", 12 * GIB), ("vm-3", 2 * GIB)] {
        io.set(&fleet().join(name), 0, held);
    }
    learner
        .window(
            &io,
            vms.clone(),
            booked_at_four,
            &[],
            fleet_budget,
            start + WINDOW,
        )
        .unwrap();
    assert_eq!(*io.kills.borrow(), vec![fleet().join("vm-2")]);
    let mut ends = learner.unrecorded();
    assert_eq!(ends.len(), 1);
    ends.retain(|(id, scope, end)| !record_last_resort(&store, id, *scope, end.as_ref()).unwrap());
    assert!(ends.is_empty(), "every end written");
    assert!(learner.unrecorded().is_empty(), "taken once");
    drop(store);

    let store = open();
    let ended = store.get(&vms[1].0).unwrap().unwrap().unwrap();
    let end = ended.last_resort.clone().unwrap();
    assert_eq!(end.id.as_str(), "vm-2");
    assert_eq!((end.held, end.budget), (12 * GIB, 8 * GIB));
    assert_eq!(
        (end.fleet_held, end.fleet_growth, end.fleet_budget),
        (16 * GIB, 16 * GIB, 16 * GIB)
    );
    for other in [&vms[0].0, &vms[2].0] {
        assert_eq!(
            store.get(other).unwrap().unwrap().unwrap().last_resort,
            None
        );
    }
    let mut info = hex_api::BoxInfo {
        id: vms[1].0.clone(),
        class: hex_api::Class::Shards,
        state: hex_api::BoxState::Empty,
        bounds: ended.spec.bounds.clone(),
        allocation: ended.spec.allocation,
        adopted: true,
        membership: None,
        exit: Some(hex_api::Exit::Signal(9)),
        abandoned: 0,
        route: None,
        grants: Vec::new(),
        published: Vec::new(),
        last_resort: None,
        ended: None,
        credentials: Vec::new(),
    };
    with_last_resort(&mut info, &ended).unwrap();
    assert_eq!(info.last_resort.as_ref(), Some(&end));
    let answer = hex_core::wire::Answer::Info(info);
    let bytes = hex_core::wire::answer(7, &answer, hex_core::wire::MAX_MESSAGE).unwrap();
    let received = hex_core::wire::Received {
        bytes: &bytes,
        size: bytes.len(),
        descriptors: 0,
        descriptors_cut: false,
    };
    let read = hex_core::wire::read_answer(&received, hex_core::wire::MAX_MESSAGE)
        .unwrap()
        .1;
    assert_eq!(read, answer);

    let made_again = Scope::Native([7; 16]);
    assert!(!record_last_resort(&store, &vms[1].0, made_again, Some(&end)).unwrap());
}

/// Joint plan step 14 (AC-T7.4.1), the learner's bounds (CLAUDE.md §2), as every OS's learner holds them
/// (`recorded::charges::bounds`): VMs, its machine's population, the most boxes placement places on it; classes, as many,
/// the least bound at which no VM's class is refused. Do run, on a machine of 20 milli-CPU (a population of two at Hex's
/// least CPU bound), two VMs each of a class of its own; then the first gone and a third of a third class; then a fourth
/// beside those two. Expect the first two held; the third held, the gone VM's class forgotten for it; and the fourth
/// refused, typed, naming the machine's population.
#[test]
fn w14_the_learner_holds_its_machines_population_of_vms_of_as_many_classes() {
    let io = Record::default();
    let all = running(&[("vm-1", 1), ("vm-2", 2), ("vm-3", 3), ("vm-4", 4)]);
    for (_, _, cgroup) in &all {
        io.set(cgroup.dir(), 0, GIB);
    }
    // Each VM a class of its own: its receipt's profile is its number.
    let record = |id: &BoxId| -> Result<Option<Met>, Refusal> {
        let n: u8 = id.as_str().strip_prefix("vm-").unwrap().parse().unwrap();
        let mut allocation = whole();
        allocation.cost.id = [n; 32];
        Ok(met(Class::Shards, allocation, 0))
    };
    let machine = res(20, 64 * GIB);
    let mut learner = Learner::new(&fleet(), machine).unwrap();
    let start = Instant::now();
    learner
        .window(
            &io,
            running(&[("vm-1", 1), ("vm-2", 2)]),
            record,
            &[],
            machine,
            start,
        )
        .unwrap();
    learner
        .window(
            &io,
            running(&[("vm-2", 2), ("vm-3", 3)]),
            record,
            &[],
            machine,
            start + WINDOW,
        )
        .unwrap();
    let past = learner.window(
        &io,
        running(&[("vm-2", 2), ("vm-3", 3), ("vm-4", 4)]),
        record,
        &[],
        machine,
        start + WINDOW * 2,
    );
    assert_eq!(
        past,
        Err(Refusal::Exhausted {
            what: "a node's VMs",
            bound: 2
        })
    );
}

/// Joint plan step 14 (AC-T7.4.1), C1 in the node: the learner's window is the period the node enforces a VM's CPU over,
/// the period of the `cpu.max` a VM's create writes. Do take the writes of a VM's 16-CPU ceiling; expect its `cpu.max`
/// period the learner's window, the window the node says.
#[test]
fn w14_the_window_is_the_period_of_the_cpu_max_the_node_writes() {
    let writes = hex_core::cgroup::writes(
        &Bounds {
            cpu_max_milli: Some(16_000),
            ..Bounds::default()
        },
        &[],
    )
    .unwrap();
    let max = writes.iter().find(|w| w.file == "cpu.max").unwrap();
    let period: u64 = max
        .value
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(Duration::from_micros(period), WINDOW);
}

/// Joint plan step 14 (AC-T7.4.1; ROOT, 2026-11-13: the starving case the done-when names): a running VM whose cgroup
/// can't be read counts whole, at its receipt, against the fleet's budget until a read succeeds, its learner untouched,
/// and the failure is told. Do run a window of two Shards VMs of 16-CPU, 8 GiB receipts in a 16-CPU, 64 GiB fleet
/// budget, the second's cgroup unread. Expect the first's memory budget the fleet's 64 GiB less the second's 8 GiB
/// receipt, and the second named among the unread.
#[test]
fn w14_a_vm_whose_cgroup_cant_be_read_counts_whole_against_the_budget() {
    let io = Record::default();
    io.dir(&fleet(), &[]);
    let vms = running(&[("vm-1", 1), ("vm-2", 2)]);
    io.set(&fleet().join("vm-1"), 0, GIB);
    let mut learner = Learner::new(&fleet(), machine()).unwrap();
    learner
        .window(&io, vms, shards, &[], res(16_000, 64 * GIB), Instant::now())
        .unwrap();
    assert_eq!(
        io.file(&fleet().join("vm-1"), "memory.high"),
        Some((56 * GIB).to_string())
    );
    let unread: Vec<&str> = learner.unread().iter().map(|(id, _)| id.as_str()).collect();
    assert_eq!(unread, ["vm-2"]);
}
