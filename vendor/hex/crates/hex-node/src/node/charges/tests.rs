//! The node agent's charge learner (joint plan step 14, AC-T7.4.1; `docs/design/learned-charge.md`).

use std::cell::{Cell, RefCell};

use super::*;
use hex_api::{LearnedPrior, LearnedUse};
use hex_sim::charge::{SEEDABLE, WEIGHT_MAX};

const GIB: u64 = 1 << 30;

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
        Ok(())
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
                [*n; 16],
                BoxCgroup::at(fleet().join(id)),
            )
        })
        .collect()
}

fn whole() -> Allocation {
    cost().allocate(CapacityResources::default()).unwrap()
}

fn shards(_: &BoxId) -> Result<Option<(Class, Allocation)>, Refusal> {
    Ok(Some((Class::Shards, whole())))
}

/// Joint plan step 14 (AC-T7.4.1), C4 and C5 in the node: the placement's cgroup paths and weights. Do run a window
/// of two cold Shards VMs, each booked at its whole 16-CPU, 8 GiB receipt, on a 32-CPU machine whose fleet budget is
/// half of it, beside another node's fleet budgeted 8 CPUs; then a second window with nothing changed. Expect each
/// VM's own cgroup given its weight by booking and `memory.high` its budget held at its 8 GiB ceiling (its `cpu.max`
/// and `memory.max` left as its create wrote them); `.vm/node-a` capped at 16 CPUs and weighing half the machine;
/// `.vm` weighing both fleets' 24 CPUs of 32; and the second window writing nothing.
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
            vm("vm-1", "cpu.weight", "10000"),
            vm("vm-1", "memory.high", &(8 * GIB).to_string()),
            vm("vm-2", "cpu.weight", "10000"),
            vm("vm-2", "memory.high", &(8 * GIB).to_string()),
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

/// Joint plan step 14 (AC-T7.4.1), C7: a Shards guest can't yet be held within its room. Do run a window of a Shards
/// VM and a Firecracker VM; expect the Shards guest's room refused as awaiting `GuestRoom`, its being told it, and
/// `RingPinsWithinRoom`, its ring pins kept within it, the Firecracker one's neither.
#[test]
fn w14_a_shards_guests_room_awaits_guest_room_and_its_ring_pins_within_it() {
    let io = Record::default();
    let vms = running(&[("vm-1", 1), ("vm-2", 2)]);
    for (_, _, cgroup) in &vms {
        io.set(cgroup.dir(), 0, GIB);
    }
    let mut learner = Learner::new(&fleet(), machine()).unwrap();
    let record = |id: &BoxId| -> Result<Option<(Class, Allocation)>, Refusal> {
        let class = if id.as_str() == "vm-1" {
            Class::Shards
        } else {
            Class::Firecracker
        };
        Ok(Some((class, whole())))
    };
    learner
        .window(&io, vms, record, &[], machine(), Instant::now())
        .unwrap();
    let vm1 = BoxId::new("vm-1").unwrap();
    assert_eq!(
        learner.awaits(),
        vec![
            (&vm1, Upstream::GuestRoom),
            (&vm1, Upstream::RingPinsWithinRoom)
        ]
    );
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
    let record = move |id: &BoxId| -> Result<Option<(Class, Allocation)>, Refusal> {
        let allocation = if id.as_str() == "vm-2" {
            booked
        } else {
            whole()
        };
        Ok(Some((Class::Shards, allocation)))
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
fn booked_at_four(_: &BoxId) -> Result<Option<(Class, Allocation)>, Refusal> {
    Ok(Some((
        Class::Shards,
        whole().booked_at(res(16_000, 4 * GIB)),
    )))
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

/// Joint plan step 14 (AC-T7.4.1), C5's record of why, bounded (CLAUDE.md §2). Do record three ends on a learner for a
/// 20 milli-CPU machine, whose population is two VMs, the third of another class. Expect the class's capacity answer
/// to carry its two latest ends, latest first, and the oldest forgotten once a third came.
#[test]
fn w14_the_last_resorts_ends_are_kept_to_the_machines_population() {
    let mut learner = Learner::new(&fleet(), res(20, 64 * GIB)).unwrap();
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
            .record_end(class, &id, [u8::try_from(n).unwrap(); 16], why(n * GIB))
            .unwrap();
    }
    let ids = |class| -> Vec<String> {
        learner
            .learned(class, &[])
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
    }
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
        let scope = *record.native_scope.unwrap().bytes();
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
    ends.retain(|(id, scope, end)| !record_last_resort(&store, id, *scope, end).unwrap());
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

    let made_again = [7; 16];
    assert!(!record_last_resort(&store, &vms[1].0, made_again, &end).unwrap());
}
