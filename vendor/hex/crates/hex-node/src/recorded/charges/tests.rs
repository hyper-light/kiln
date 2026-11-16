//! The macOS and Windows nodes' charge learner (joint plan step 14, AC-T7.4.1; `docs/design/learned-charge.md`).

use std::cell::RefCell;

use super::*;
use hex_api::{Allocation, BoxId, BoxSpec, ClassSpec, OciSpec};
use hex_core::record::{NativeScope, ProcId};

/// The box a learned holding or end names: this learner holds boxes alone.
fn box_of(holder: &hex_api::LearnedHolder) -> &str {
    match holder {
        hex_api::LearnedHolder::Box(id) => id.as_str(),
        hex_api::LearnedHolder::Node(run) => panic!("a box's learner named a node VM, {run:?}"),
    }
}

/// `records` as the holders a learner reads.
fn holders(records: &[BoxRecord]) -> Vec<Holder<'_>> {
    boxes(records).unwrap()
}

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

/// [`cost`]'s profile sized to `machine`, the host a test runs on (`Recorded::machine`): as many CPUs as it has, and
/// 8 GiB where it has them, so a class answered over the real machine names a VM that machine can hold. [`cost`]'s 16 CPUs
/// passed the 12 of ROOT's Windows test VM ("minimum initialized runtime charge exceeds its physical pool").
fn machine_cost(machine: CapacityResources) -> RuntimeCost {
    let envelope = res(machine.cpu_milli, (8 * GIB).min(machine.mem_bytes));
    RuntimeCost {
        floor: envelope,
        ceiling: envelope,
        ..cost()
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

/// A cap as the learner told it (D260): the VM, its share, and its VMM's own as read or why not.
type Told = (String, u64, Result<u64, String>);

/// The managed VMs as a record: each one's CPU and memory as the test sets them, every cap set and every VM stopped,
/// in order.
#[derive(Default)]
struct Record {
    usage: RefCell<BTreeMap<String, Usage>>,
    /// Each image VM's host use, as the test sets it, and each room it was told, in order.
    host: RefCell<BTreeMap<String, HostUse>>,
    /// Each image VM's run's rate period, as the test sets it: the platform's window for a VM it sets none for.
    periods: RefCell<BTreeMap<String, Duration>>,
    /// The image VMs whose run's period the test has made unreadable.
    unread_periods: RefCell<Vec<String>>,
    /// Each managed VM's governor's period, as its VM process last answered it (D255), as the test sets it: the test's
    /// window for a VM its governor holds that it sets none for, none for one its platform's cap holds.
    governed: RefCell<BTreeMap<String, Duration>>,
    /// The cycle a platform's cap holds a VM over at any rate (C1), as the test sets it: none where it sets none.
    cycle: std::cell::Cell<Option<Duration>>,
    /// Why that cycle is cold, where the test says it is: with no cycle, its helper has measured no stop (2c-4b); with
    /// one, it is at a cold estimate, used (the review's A-14).
    cold: RefCell<Option<String>>,
    /// What holds each VM's rate once set (D255), as the test sets it: its governor for a VM it sets none for.
    held: RefCell<BTreeMap<String, hex_api::RateHeld>>,
    rooms: RefCell<Vec<(String, i64)>>,
    /// Each VM's share as it was capped, in order.
    rates: RefCell<Vec<(String, u64)>>,
    /// Each managed VM's guest time as its VM process answers its use (D260), as the test sets it: none apart for a VM
    /// it sets none for.
    guest: RefCell<BTreeMap<String, ManagedGuestTime>>,
    /// Each managed VM's life so far, as the test sets it.
    lives: RefCell<BTreeMap<String, Duration>>,
    /// Each cap as the learner told it (D260), in order.
    caps: RefCell<Vec<Told>>,
    /// Each VM's actuator's least share (C4), as the test sets it: a governor's for a VM it sets none for.
    leasts: RefCell<BTreeMap<String, u64>>,
    refuse_rates: std::cell::Cell<bool>,
    /// The VMs whose caps the native owner refuses, as the test names them (A-7): the rest are set.
    refuse_rate_of: RefCell<Vec<String>>,
    /// Each cap asked, its VM and share, in order, set or refused (A-7).
    asked: RefCell<Vec<(String, u64)>>,
    /// Whether an intent's write is refused, as a store whose write fails refuses it.
    refuse_intents: std::cell::Cell<bool>,
    stops: RefCell<Vec<String>>,
    /// The node's records, where the test keeps them: an intent is written there, as the node writes it.
    records: RefCell<Option<Store>>,
    /// Every intent asked and every stop, in order: `intend <box>`, `stop <box>`.
    events: RefCell<Vec<String>>,
    /// What each holder a window doesn't learn takes of the machine's free memory, as the test sets it (A-10): unread
    /// for one it sets none for.
    taken: RefCell<BTreeMap<String, u64>>,
    /// The machine's memory and disk free, as the test sets it (A-10).
    free: std::cell::Cell<CapacityResources>,
    /// Each read of a VM's use, of what a holder takes and of what is free, in order: `usage <vm>`, `host_use <vm>`,
    /// `taken <holder>`, `free`.
    reads: RefCell<Vec<String>>,
    /// The time as the test's clock tells it, where it keeps one (the review's A-11): a steady VM's use is read there, a
    /// window's first ask of a VM moving it on by `lag`, and its VM process's use answer by `round_trip`.
    time: std::cell::Cell<Option<Instant>>,
    /// How long a window's reads of the VMs before a VM take: its first ask of the VM moves the test's clock on so far.
    lag: std::cell::Cell<Duration>,
    /// How long a VM process's use answer takes, its sample taken halfway (Shards' `ManagedUse`).
    round_trip: std::cell::Cell<Duration>,
    /// The VMs whose use moves on with the test's clock (the review's A-11).
    steady: RefCell<BTreeMap<String, Steady>>,
}

/// A managed VM of 16 vCPUs whose use moves on at steady rates from `from` (the review's A-11): its scope's CPU from
/// `scope_usec` at `scope_milli`, and each vCPU's guest time from `vcpu_ns` at `vcpu_milli`.
#[derive(Clone, Copy)]
struct Steady {
    from: Instant,
    scope_usec: u64,
    scope_milli: u64,
    vcpu_ns: u64,
    vcpu_milli: u64,
}

impl Steady {
    /// Its scope's CPU in microseconds and each vCPU's guest time in nanoseconds at `now`.
    fn at(self, now: Instant) -> (u64, u64) {
        let ns = u64::try_from(now.saturating_duration_since(self.from).as_nanos()).unwrap();
        (
            self.scope_usec + ns * self.scope_milli / 1_000_000,
            self.vcpu_ns + ns * self.vcpu_milli / 1_000,
        )
    }
}

impl Managed for Record {
    fn usage(&self, holder: Holder<'_>) -> Result<Usage, Refusal> {
        self.reads
            .borrow_mut()
            .push(format!("usage {}", named(holder)));
        let steady = self.steady.borrow().get(&named(holder)).copied();
        if let (Some(now), Some(steady)) = (self.time.get(), steady) {
            return Ok(Usage {
                cpu_usec: steady.at(now).0,
                memory_bytes: GIB,
                ..Usage::default()
            });
        }
        self.usage
            .borrow()
            .get(&named(holder))
            .cloned()
            .ok_or_else(|| Refusal::Broken("no such VM".into()))
    }

    fn guest_time(&self, holder: Holder<'_>) -> Result<ManagedGuestTime, Refusal> {
        let steady = self.steady.borrow().get(&named(holder)).copied();
        if let (Some(asked), Some(steady)) = (self.time.get(), steady) {
            let half = self.round_trip.get() / 2;
            let answered = asked + half + half;
            self.time.set(Some(answered));
            return Ok(ManagedGuestTime::PerVcpu {
                ns: vec![steady.at(asked + half).1; 16],
                asked,
                answered,
                charged: hex_api::Charged::ToThread,
            });
        }
        Ok(self
            .guest
            .borrow()
            .get(&named(holder))
            .cloned()
            .unwrap_or_else(|| ManagedGuestTime::Unheld("the test tells none".to_owned())))
    }

    fn cap_cycle(&self, _: u64) -> Result<CapCycle, Refusal> {
        Ok(match (self.cycle.get(), self.cold.borrow().as_ref()) {
            (Some(cycle), Some(why)) => CapCycle::ColdOf {
                cycle,
                why: why.clone(),
            },
            (None, Some(why)) => CapCycle::Cold(why.clone()),
            (Some(cycle), None) => CapCycle::Of(cycle),
            (None, None) => CapCycle::Never,
        })
    }

    fn least(&self, holder: Holder<'_>) -> u64 {
        self.leasts
            .borrow()
            .get(&named(holder))
            .copied()
            .unwrap_or(GOVERNOR_LEAST.get())
    }

    fn life(&self, holder: Holder<'_>) -> Result<Duration, Refusal> {
        self.lives
            .borrow()
            .get(&named(holder))
            .copied()
            .ok_or_else(|| Refusal::Broken("no life set".into()))
    }

    fn host_use(&self, holder: Holder<'_>) -> Result<HostUse, Refusal> {
        self.reads
            .borrow_mut()
            .push(format!("host_use {}", named(holder)));
        self.host
            .borrow()
            .get(&named(holder))
            .cloned()
            .ok_or_else(|| Refusal::Broken("no such VM".into()))
    }

    fn taken(&self, holder: Holder<'_>) -> Result<u64, Refusal> {
        let name = named(holder);
        self.reads.borrow_mut().push(format!("taken {name}"));
        self.taken
            .borrow()
            .get(&name)
            .copied()
            .ok_or_else(|| Refusal::Broken(format!("{name}'s use unread")))
    }

    fn free(&self) -> Result<CapacityResources, Refusal> {
        self.reads.borrow_mut().push("free".to_owned());
        Ok(self.free.get())
    }

    fn rate_period(&self, holder: Holder<'_>) -> Result<Option<Duration>, Refusal> {
        if let Some(now) = self.time.get() {
            self.time.set(Some(now + self.lag.get()));
        }
        let name = named(holder);
        if self.unread_periods.borrow().contains(&name) {
            return Err(Refusal::Broken("its run's period unread".into()));
        }
        if !holder.image() {
            if let Some(period) = self.governed.borrow().get(&name) {
                return Ok(Some(*period));
            }
            if matches!(
                self.held.borrow().get(&name),
                Some(hex_api::RateHeld::Platform(_))
            ) {
                return Ok(None);
            }
            return Ok(Some(WINDOW));
        }
        Ok(Some(
            self.periods.borrow().get(&name).copied().unwrap_or(WINDOW),
        ))
    }

    fn set_room(&self, holder: Holder<'_>, room: i64) -> Result<(), Refusal> {
        // A managed Shards VM's guest has no room call from Hex, as the guest runtime answers it (C7).
        if holder.managed() {
            return Err(Refusal::Awaits {
                needs: Upstream::GuestRoom,
            });
        }
        self.rooms.borrow_mut().push((named(holder), room));
        Ok(())
    }

    fn set_rate(&self, holder: Holder<'_>, cap: Cap<'_>) -> Result<hex_api::RateHeld, Refusal> {
        let name = named(holder);
        self.asked.borrow_mut().push((name.clone(), cap.share));
        if self.refuse_rates.get() {
            return Err(Refusal::Broken("the rate was refused".into()));
        }
        if self.refuse_rate_of.borrow().contains(&name) {
            return Err(Refusal::Broken(format!("{name}'s rate was refused")));
        }
        self.rates.borrow_mut().push((name.clone(), cap.share));
        let own = match cap.own {
            Own::Read(own) => Ok(own),
            Own::Unheld(why) => Err(why.to_owned()),
        };
        self.caps.borrow_mut().push((name.clone(), cap.share, own));
        Ok(self
            .held
            .borrow()
            .get(&name)
            .cloned()
            .unwrap_or(hex_api::RateHeld::Governor))
    }

    fn stop(&self, holder: Holder<'_>) -> Result<(), Refusal> {
        let name = named(holder);
        self.events.borrow_mut().push(format!("stop {name}"));
        self.stops.borrow_mut().push(name);
        Ok(())
    }

    fn intend(&self, id: &LearnedHolder, scope: Scope, end: &LearnedEnd) -> Result<bool, Refusal> {
        self.events
            .borrow_mut()
            .push(format!("intend {}", holder_name(id)));
        if self.refuse_intents.get() {
            return Err(Refusal::Broken("the record's write failed".into()));
        }
        match self.records.borrow().as_ref() {
            Some(store) => record_last_resort(store, id, scope, Some(end)),
            None => Ok(false),
        }
    }
}

/// The name a VM goes by in these tests: a box's id, a node VM's `node-<n>` for its run of byte `n`.
fn holder_name(holder: &LearnedHolder) -> String {
    match holder {
        LearnedHolder::Box(id) => id.as_str().to_owned(),
        LearnedHolder::Node(run) => format!("node-{}", run.bytes()[0]),
    }
}

/// The name `holder` goes by in these tests ([`holder_name`]).
fn named(holder: Holder<'_>) -> String {
    holder_name(&holder.id().unwrap())
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

    /// The machine reads `mem_bytes` of memory and `disk_bytes` of disk free.
    fn set_free(&self, mem_bytes: u64, disk_bytes: u64) {
        self.free.set(CapacityResources {
            cpu_milli: 0,
            mem_bytes,
            disk_bytes,
        });
    }
}

/// A Ready process box's record, `name`, capped at `cpu_milli` of CPU and `memory` bytes.
fn process_box(name: &str, cpu_milli: u32, memory: u64) -> BoxRecord {
    BoxRecord::new(
        BoxSpec {
            id: BoxId::new(name).unwrap(),
            bounds: hex_api::Bounds {
                cpu_max_milli: Some(cpu_milli),
                memory_max: Some(memory),
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
    )
}

/// `cpu_milli` of CPU, `mem_bytes` of memory and `disk_bytes` of disk.
fn res3(cpu_milli: u64, mem_bytes: u64, disk_bytes: u64) -> CapacityResources {
    CapacityResources {
        cpu_milli,
        mem_bytes,
        disk_bytes,
    }
}

/// The review's A-10 (AC-T7.4.1; C7): the fleet's budget was the machine's whole less Hex's own bookings, so the person's
/// own work, kiln's runs and what nobody books went unseen, and its disk was never reduced. It is the room Hex has now:
/// every CPU less what the holders it doesn't learn book; the memory and disk free now and what its VMs hold, less what
/// each holder it doesn't learn may still grow into, its booking past what it takes. Do measure the budget of a 32-CPU
/// machine of 64 GiB and 100 GiB of disk, 10 GiB of memory and 20 GiB of disk free, its learned VMs holding 4 GiB, beside
/// a process box capped at 2 CPUs and 8 GiB taking 6 GiB, and a VM booked whole at 4 CPUs, 4 GiB and 10 GiB of disk
/// taking 1 GiB. Expect 26 CPUs, 10 + 4 - 2 - 3 = 9 GiB of memory, and 20 - 10 = 10 GiB of disk.
#[test]
fn a10_the_fleets_budget_is_what_is_free_and_what_its_vms_hold() {
    let io = Record::default();
    io.set_free(10 * GIB, 20 * GIB);
    io.taken.borrow_mut().insert("proc-1".to_owned(), 6 * GIB);
    io.taken.borrow_mut().insert("vm-9".to_owned(), GIB);
    let native = process_box("proc-1", 2_000, 8 * GIB);
    let whole = vm("vm-9", 9);
    let booked = [
        (Holder::Box(&native), res3(2_000, 8 * GIB, 0)),
        (Holder::Box(&whole), res3(4_000, 4 * GIB, 10 * GIB)),
    ];
    let budget = measured(
        &io,
        res3(32_000, 64 * GIB, 100 * GIB),
        &booked,
        4 * GIB,
        CapacityResources::default(),
    )
    .unwrap();
    assert_eq!(budget, res3(26_000, 9 * GIB, 10 * GIB));
}

/// The review's A-10 (ROOT, 2026-11-14: Hex's own boxes are never counted twice): what a box uses is missing from the
/// machine's free memory already, so the fleet loses what it may still grow into, its booking past its use, never its
/// whole booking beside its use; and a box past its booking takes the excess from the fleet. Do measure the budget of a
/// machine of 64 GiB beside one process box capped at 8 GiB, with no VM held: while the box takes nothing, 20 GiB free;
/// as it takes 6 GiB, 14 GiB free; as it takes 10 GiB, past its cap, 10 GiB free. Expect 12 GiB, 12 GiB, then 10 GiB.
#[test]
fn a10_hexs_own_boxes_are_never_counted_twice() {
    let native = process_box("proc-1", 2_000, 8 * GIB);
    let booked = [(Holder::Box(&native), res3(2_000, 8 * GIB, 0))];
    let budgets: Vec<u64> = [(0, 20 * GIB), (6 * GIB, 14 * GIB), (10 * GIB, 10 * GIB)]
        .into_iter()
        .map(|(taken, free)| {
            let io = Record::default();
            io.taken.borrow_mut().insert("proc-1".to_owned(), taken);
            io.set_free(free, 0);
            measured(
                &io,
                res3(32_000, 64 * GIB, 0),
                &booked,
                0,
                CapacityResources::default(),
            )
            .unwrap()
            .mem_bytes
        })
        .collect();
    assert_eq!(budgets, [12 * GIB, 12 * GIB, 10 * GIB]);
}

/// The review's A-10 (ROOT, 2026-11-14: the budget never passes what is free and what the VMs hold): whatever its holders
/// book or take, the fleet's memory budget is at most the machine's free memory and what its VMs hold, its disk budget at
/// most the disk free. Do measure budgets over machines of 16 and 64 GiB, 0, 8 and 48 GiB of memory and disk free, VMs
/// holding 0 and 12 GiB, and one box booking 0, 4 and 32 GiB taking 0, half and twice its booking. Expect each within.
#[test]
fn a10_the_fleets_budget_never_passes_what_is_free_and_what_its_vms_hold() {
    let native = process_box("proc-1", 2_000, 8 * GIB);
    for machine in [16 * GIB, 64 * GIB] {
        for free in [0, 8 * GIB, 48 * GIB] {
            for held in [0, 12 * GIB] {
                for booking in [0, 4 * GIB, 32 * GIB] {
                    for taken in [0, booking / 2, booking * 2] {
                        let io = Record::default();
                        io.taken.borrow_mut().insert("proc-1".to_owned(), taken);
                        io.set_free(free, free);
                        let booked = [(Holder::Box(&native), res3(0, booking, booking))];
                        let budget = measured(
                            &io,
                            res3(32_000, machine, machine),
                            &booked,
                            held,
                            CapacityResources::default(),
                        )
                        .unwrap();
                        assert!(
                            budget.mem_bytes <= free + held && budget.disk_bytes <= free,
                            "machine {machine}, free {free}, held {held}, booking {booking}, taken {taken}: \
                             {budget:?}"
                        );
                    }
                }
            }
        }
    }
}

/// The review's A-10 (C7): the machine's free memory is read after the window's VMs and the holders it doesn't learn,
/// so a VM or box that grows between the reads shrinks the fleet's budget, never swells it. Do run a window of an image
/// VM and a managed VM beside a process box, its budget measured. Expect the free memory read once, after every other.
#[test]
fn a10_a_window_reads_what_is_free_after_its_vms_and_its_boxes() {
    let io = Record::default();
    let image = image_vm("img-1", 1);
    let managed = vm("vm-1", 2);
    let native = process_box("proc-1", 2_000, 8 * GIB);
    unused(&io, "img-1", GIB);
    io.set("vm-1", 0, GIB);
    io.taken.borrow_mut().insert("proc-1".to_owned(), GIB);
    io.set_free(32 * GIB, 0);
    let booked = [(Holder::Box(&native), res3(2_000, 8 * GIB, 0))];
    let mut learner = Learner::new(true, res(32_000, 64 * GIB)).unwrap();
    window(
        &mut learner,
        &io,
        &holders(&[image, managed]),
        Vec::new(),
        |held| {
            measured(
                &io,
                res3(32_000, 64 * GIB, 0),
                &booked,
                held,
                CapacityResources::default(),
            )
        },
        Instant::now,
    )
    .unwrap();
    let reads = io.reads.borrow().clone();
    assert_eq!(
        reads.iter().filter(|read| *read == "free").count(),
        1,
        "{reads:?}"
    );
    assert_eq!(reads.last().map(String::as_str), Some("free"), "{reads:?}");
}

/// The review's A-10 and its dropped records (ROOT, 2026-11-14): what a box whose record can't be read takes of memory
/// and disk is missing from what is measured free already, so its count at the machine gives way there; its CPU, which no
/// measure of what is free holds, stays counted at its machine. Do run a window of one image VM booked at 4 CPUs and
/// 4 GiB, holding 1 GiB, in a fleet budget of 24 CPUs and 64 GiB, beside a box whose record can't be read. Expect the VM
/// capped at its 4-CPU booking, and its guest told the room of the whole memory budget less what it holds: 63 GiB.
#[test]
fn a10_a_box_whose_record_cant_be_read_is_counted_at_its_use_for_memory() {
    let io = Record::default();
    let image = image_vm("img-1", 1);
    unused(&io, "img-1", GIB);
    let mut learner = Learner::new(true, res(32_000, 64 * GIB)).unwrap();
    window(
        &mut learner,
        &io,
        &holders(std::slice::from_ref(&image)),
        vec![(
            BoxId::new("torn-1").unwrap(),
            "its record is torn".to_owned(),
        )],
        |_| Ok(res(24_000, 64 * GIB)),
        Instant::now,
    )
    .unwrap();
    assert_eq!(*io.rates.borrow(), [("img-1".to_owned(), 4_000)]);
    assert_eq!(
        *io.rooms.borrow(),
        [("img-1".to_owned(), i64::try_from(63 * GIB).unwrap())]
    );
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
    let mut learner = Learner::new(true, res(32_000, 64 * GIB)).unwrap();
    let start = Instant::now();
    learner.window(&io, &holders(&vms), budget, start).unwrap();
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
    learner
        .window(&io, &holders(&vms), budget, start + WINDOW)
        .unwrap();
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
        .window(&io, &holders(&vms), budget, start + WINDOW * 2)
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

/// Joint plan step 14 (AC-T7.4.1), C4 and C8 on macOS and Windows, and A-7 (the review's, 2026-11-14; ROOT's ruling):
/// a cap the platform refuses is its VM's alone, never its window's. Do run a window of two VMs whose caps the native
/// owner refuses, then ask for the class's capacity answer, then let a window pass. Expect the window passed, its
/// answer's learned use there and naming both VMs, each held at its last cap (its allocation's 16 CPUs, never capped
/// yet) with the owner's refusal; and once a window sets their caps, neither named, the answer's learned use the VMs'
/// receipts and holdings.
#[test]
fn a7_a_refused_cap_holds_its_vm_at_its_last_and_is_told() {
    let io = Record::default();
    let vms = vec![vm("vm-1", 1), vm("vm-2", 2)];
    for record in &vms {
        io.set(record.spec.id.as_str(), 0, GIB);
    }
    let mut learner = Learner::new(true, res(32_000, 64 * GIB)).unwrap();
    io.refuse_rates.set(true);
    let start = Instant::now();
    learner
        .window(&io, &holders(&vms), res(24_000, 64 * GIB), start)
        .unwrap();
    let learned = learner
        .learned(Class::Shards, &[cost()], &holders(&vms))
        .unwrap()
        .unwrap();
    assert_eq!(
        learned
            .unacted
            .iter()
            .map(|told| box_of(&told.id))
            .collect::<Vec<_>>(),
        ["vm-1", "vm-2"]
    );
    let why = format!(
        "its cap couldn't be set: {}; held at its last, 16000 milli-CPU",
        Refusal::Broken("the rate was refused".into())
    );
    assert!(
        learned.unacted.iter().all(|told| told.why == why),
        "{:?}",
        learned.unacted
    );
    io.refuse_rates.set(false);
    learner
        .window(&io, &holders(&vms), res(24_000, 64 * GIB), start + WINDOW)
        .unwrap();
    let learned = learner
        .learned(Class::Shards, &[cost()], &holders(&vms))
        .unwrap()
        .unwrap();
    assert!(learned.unacted.is_empty(), "{:?}", learned.unacted);
    assert_eq!(learned.used.booked, res(8_000, 8 * GIB));
    assert_eq!(learned.holdings.len(), 2);
}

/// A-7 (the review's, 2026-11-14; ROOT's ruling (ii); AC-T7.4.1), C4 on macOS and Windows: each VM's cap is its own,
/// and one its window couldn't move is held at its last and counted there. Do cap three Shards VMs of 16-CPU
/// allocations, each booked at 4 CPUs, in a 24-CPU fleet budget (8 CPUs each); then run a window in which the first
/// uses its whole cap and its cap is refused; then one in which every VM uses its whole cap, the first's still refused.
/// Expect the second window passed, the others capped as if none were refused (4 CPUs each) and the first named on its
/// class's answer, held at its last 8 CPUs; then the first asked again at its last, the others' caps summing to the
/// budget less it (16 CPUs), and the first still named.
#[test]
fn a7_a_vm_whose_cap_is_refused_is_held_at_its_last_and_the_others_share_the_rest() {
    let io = Record::default();
    let vms = vec![vm("vm-1", 1), vm("vm-2", 2), vm("vm-3", 3)];
    for record in &vms {
        io.set(record.spec.id.as_str(), 0, GIB);
    }
    let budget = res(24_000, 64 * GIB);
    let mut learner = Learner::new(true, res(32_000, 64 * GIB)).unwrap();
    let first = Scope::of(&vms[0]).unwrap();
    let start = Instant::now();
    learner.window(&io, &holders(&vms), budget, start).unwrap();
    assert_eq!(learner.rate(&first), Some(8_000));
    io.rates.borrow_mut().clear();
    io.refuse_rate_of.borrow_mut().push("vm-1".to_owned());
    // 300 ms windows: the first used its whole 8-CPU cap (2.4 s), the second 1 CPU, the third 2 CPUs.
    io.set("vm-1", 2_400_000, GIB);
    io.set("vm-2", 300_000, GIB);
    io.set("vm-3", 600_000, GIB);
    learner
        .window(&io, &holders(&vms), budget, start + WINDOW)
        .unwrap();
    assert_eq!(
        *io.rates.borrow(),
        vec![("vm-2".to_owned(), 4_000), ("vm-3".to_owned(), 4_000)]
    );
    assert_eq!(learner.rate(&first), Some(8_000));
    let unacted = |learner: &Learner| {
        learner
            .learned(Class::Shards, &[cost()], &holders(&vms))
            .unwrap()
            .unwrap()
            .unacted
    };
    let told = unacted(&learner);
    assert_eq!(
        told.iter().map(|told| box_of(&told.id)).collect::<Vec<_>>(),
        ["vm-1"]
    );
    assert_eq!(
        told[0].why,
        format!(
            "its cap couldn't be set: {}; held at its last, 8000 milli-CPU",
            Refusal::Broken("vm-1's rate was refused".into())
        )
    );
    io.asked.borrow_mut().clear();
    // Each used its whole cap: the first its 8 CPUs, the others their 4 (1.2 s each).
    io.set("vm-1", 2_400_000 + 2_400_000, GIB);
    io.set("vm-2", 300_000 + 1_200_000, GIB);
    io.set("vm-3", 600_000 + 1_200_000, GIB);
    learner
        .window(&io, &holders(&vms), budget, start + WINDOW * 2)
        .unwrap();
    assert!(
        io.asked.borrow().contains(&("vm-1".to_owned(), 8_000)),
        "{:?}",
        io.asked.borrow()
    );
    let others: u64 = vms[1..]
        .iter()
        .map(|r| learner.rate(&Scope::of(r).unwrap()).unwrap())
        .sum();
    assert_eq!(others, budget.cpu_milli - 8_000);
    assert_eq!(learner.rate(&first), Some(8_000));
    assert_eq!(unacted(&learner).len(), 1);
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
    let learner = Learner::new(true, res(32_000, 64 * GIB)).unwrap();
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
    let mut learner = Learner::new(true, res(32_000, 64 * GIB)).unwrap();
    let start = Instant::now();
    learner.window(&io, &holders(&vms), budget, start).unwrap();
    for (name, held) in [("vm-1", 6 * GIB), ("vm-2", 7 * GIB), ("vm-3", 2 * GIB)] {
        io.set(name, 0, held);
    }
    learner
        .window(&io, &holders(&vms), budget, start + WINDOW)
        .unwrap();
    assert_eq!(*io.stops.borrow(), ["vm-2"]);
    assert!(
        matches!(learner.last.resort(), Resort::Ending { id, .. } if box_of(id) == "vm-2"),
        "{:?}",
        learner.last.resort()
    );
    let learned = learner
        .learned(Class::Shards, &[cost()], &holders(&vms))
        .unwrap()
        .unwrap();
    let ends: Vec<_> = learned
        .ended
        .iter()
        .map(|end| {
            (
                box_of(&end.id),
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
        .window(&io, &holders(&vms), budget, start + WINDOW * 2)
        .unwrap();
    assert_eq!(io.stops.borrow().len(), 1, "one end is never two");
    let left = vec![vm("vm-1", 1), vm("vm-3", 3)];
    learner
        .window(&io, &holders(&left), budget, start + WINDOW * 3)
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
    let mut learner = Learner::new(true, res(32_000, 64 * GIB)).unwrap();
    let start = Instant::now();
    learner.window(&io, &holders(&vms), budget, start).unwrap();
    for (name, held) in [
        ("vm-first", 7 * GIB),
        ("vm-second", 7 * GIB),
        ("vm-third", 2 * GIB),
    ] {
        io.set(name, 0, held);
    }
    learner
        .window(&io, &holders(&vms), budget, start + WINDOW)
        .unwrap();
    assert_eq!(*io.stops.borrow(), ["vm-second"]);
    let learned = learner
        .learned(Class::Shards, &[cost()], &holders(&vms))
        .unwrap()
        .unwrap();
    assert_eq!(
        learned.ended,
        vec![LearnedEnd {
            id: hex_api::LearnedHolder::Box(BoxId::new("vm-second").unwrap()),
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
        node_vm_ends: std::sync::Mutex::new(Vec::new()),
        capacity_view: std::sync::atomic::AtomicBool::new(false),
        reserved: std::sync::Mutex::new(None),
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
        id: hex_api::LearnedHolder::Box(spec.id.clone()),
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
            ended: None,
            control: None,
            control_wake: None,
            credentials: Vec::new(),
        })
        .unwrap();
    assert_eq!(info.last_resort, Some(end));
}

/// Joint plan step 14 (AC-T7.4.1), C5's record of why, bounded (CLAUDE.md §2), as every OS's learner keeps it. Do record
/// three ends on a learner for a 20 milli-CPU machine, whose population is two VMs, the third of another class. Expect
/// the class's capacity answer to carry its two latest ends, latest first, and the oldest forgotten once a third came.
#[test]
fn w14_the_last_resorts_ends_are_kept_to_the_machines_population() {
    let mut learner = Learner::new(true, res(20, 64 * GIB)).unwrap();
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
                &LearnedHolder::Box(id),
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
            .map(|end| box_of(&end.id).to_owned())
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
    let mut learner = Learner::new(true, res(32_000, 64 * GIB)).unwrap();
    let start = Instant::now();
    learner.window(&io, &holders(&vms), budget, start).unwrap();
    for (name, held) in [("vm-1", 2 * GIB), ("vm-2", 12 * GIB), ("vm-3", 2 * GIB)] {
        io.set(name, 0, held);
    }
    learner
        .window(&io, &holders(&vms), budget, start + WINDOW)
        .unwrap();
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
    assert_eq!((box_of(&end.id), end.held), ("vm-2", 12 * GIB));
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
        id: hex_api::LearnedHolder::Box(made[1].spec.id.clone()),
        held: 7 * GIB,
        budget: 6 * GIB,
        band: 0,
        fleet_held: 15 * GIB,
        fleet_growth: 15 * GIB,
        fleet_budget: 16 * GIB,
        unmoved: Some(hex_api::Upstream::LiveMove),
    };
    assert!(
        record_last_resort(
            &store,
            &LearnedHolder::Box(made[1].spec.id.clone()),
            scope,
            Some(&intent)
        )
        .unwrap()
    );
    let vms: Vec<BoxRecord> = made
        .iter()
        .map(|record| store.get(&record.spec.id).unwrap().unwrap().unwrap())
        .collect();
    let io = Record::default();
    for record in &vms {
        io.set(record.spec.id.as_str(), 0, 2 * GIB);
    }
    let budget = res(24_000, 16 * GIB);
    let mut learner = Learner::new(true, res(32_000, 64 * GIB)).unwrap();
    let start = Instant::now();
    learner.window(&io, &holders(&vms), budget, start).unwrap();
    assert_eq!(*io.stops.borrow(), ["vm-2"]);
    let learned = learner
        .learned(Class::Shards, &[cost()], &holders(&vms))
        .unwrap()
        .unwrap();
    assert_eq!(learned.ended, vec![intent]);
    learner
        .window(&io, &holders(&vms), budget, start + WINDOW)
        .unwrap();
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
    let mut learner = Learner::new(true, machine).unwrap();
    let start = Instant::now();
    learner
        .window(&io, &holders(&[one, two.clone()]), machine, start)
        .unwrap();
    learner
        .window(
            &io,
            &holders(&[two.clone(), three.clone()]),
            machine,
            start + WINDOW,
        )
        .unwrap();
    assert_eq!(
        learner.window(
            &io,
            &holders(&[two, three, four]),
            machine,
            start + WINDOW * 2
        ),
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
        id: hex_api::LearnedHolder::Box(BoxId::new(&format!("vm-{n}")).unwrap()),
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
        .map(|(id, _, _)| box_of(&id).to_owned())
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
/// held at its ceiling's 16 CPUs until it has run, told so from its first window with its VMM's own unheld (D260: its
/// run here tells no span to reckon it over), then capped through its rate at the 10.667 CPUs it was charged, its
/// governor's rate leaving out its VMM's own over the window, 2.666 CPUs; its guest told its room, its budget less what
/// it holds, as it moves, from what it holds as it is met, its baseline (63 GiB, then 61 GiB; ROOT, 2026-11-13: never
/// its whole budget); and the third window refused, typed, its guest time naming a vCPU count other than its reading
/// before's.
#[test]
fn w14_a_capping_node_learns_an_image_vm_through_its_run() {
    let io = Record::default();
    let vm = image_vm("img-1", 1);
    let use_at = |cpu_ns: u64, each_ns: u64, vcpus: u64, held: u64| HostUse {
        cpu_ns,
        footprint_bytes: held,
        resident_bytes: held,
        guest_ns: vec![each_ns; usize::try_from(vcpus).unwrap()],
        window: std::time::Duration::ZERO,
        charged: hex_api::Charged::ToThread,
    };
    io.host
        .borrow_mut()
        .insert("img-1".to_owned(), use_at(0, 0, 16, GIB));
    let budget = res(24_000, 64 * GIB);
    let mut learner = Learner::new(true, res(32_000, 64 * GIB)).unwrap();
    let start = Instant::now();
    learner
        .window(&io, &holders(std::slice::from_ref(&vm)), budget, start)
        .unwrap();
    assert_eq!(
        *io.caps.borrow(),
        vec![(
            "img-1".to_owned(),
            16_000,
            Err("its life so far reads none".to_owned())
        )],
        "held at its 16-CPU ceiling until it has run"
    );
    io.host.borrow_mut().insert(
        "img-1".to_owned(),
        use_at(3_200_000_000, 150_000_000, 16, 3 * GIB),
    );
    learner
        .window(
            &io,
            &holders(std::slice::from_ref(&vm)),
            budget,
            start + WINDOW,
        )
        .unwrap();
    assert_eq!(
        io.caps.borrow().last(),
        Some(&("img-1".to_owned(), 10_667, Ok(2_666)))
    );
    assert_eq!(
        *io.rooms.borrow(),
        vec![
            ("img-1".to_owned(), i64::try_from(63 * GIB).unwrap()),
            ("img-1".to_owned(), i64::try_from(61 * GIB).unwrap()),
        ]
    );
    io.host.borrow_mut().insert(
        "img-1".to_owned(),
        use_at(3_500_000_000, 160_000_000, 15, 3 * GIB),
    );
    assert!(matches!(
        learner.window(&io, &holders(&[vm]), budget, start + WINDOW * 2),
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
    let mut learner = Learner::new(true, res(32_000, 64 * GIB)).unwrap();
    let envelope = res(16_000, 8 * GIB);
    let ceilings = |learner: &Learner| {
        learner
            .learned(Class::Shards, &[cost()], &holders(records))
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
    learner
        .window(&io, &holders(records), budget, start)
        .unwrap();
    io.set("vm-1", 300_000, 8 * GIB + (300 << 20));
    learner
        .window(&io, &holders(records), budget, start + WINDOW)
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
            window: std::time::Duration::ZERO,
            charged: hex_api::Charged::ToThread,
        },
    );
    let records = vec![vm("vm-1", 1), image_vm("img-1", 2)];
    let mut learner = Learner::new(true, res(32_000, 64 * GIB)).unwrap();
    learner
        .window(
            &io,
            &holders(&records),
            res(24_000, 64 * GIB),
            Instant::now(),
        )
        .unwrap();
    let mut capacity = hex_api::Capacity {
        own: Default::default(),
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
        node_vms: Vec::new(),
        unlearned: None,
    };
    learner
        .answer(
            Class::Shards,
            &mut capacity,
            records.iter().map(Holder::Box),
        )
        .unwrap();
    let mut held: Vec<String> = capacity
        .learned
        .unwrap()
        .holdings
        .iter()
        .map(|holding| box_of(&holding.id).to_owned())
        .collect();
    held.sort();
    assert_eq!(held, ["img-1", "vm-1"]);
}

/// Joint plan step 14 (C4; AC-T7.4.1): a window books off the fleet's budget every box it doesn't learn. Do sort a
/// managed VM, a Shards image VM, a process box capped at 2 CPUs and 1 GiB and an ended image VM, for a learner whose
/// platform has no window for managed VMs, then for one whose platform has one. Expect, with none, the image VM learned
/// (its cap's period its run's own), the managed VM booked whole at its 16-CPU, 8 GiB allocation and the box at its
/// caps, the ended VM nowhere; with one, the managed VM learned too.
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
            .map(|holder| box_of(&holder.id().unwrap()).to_owned())
            .collect()
    };
    let records = records();
    let unmanaged = sort(&records, &[], |_| false).unwrap();
    assert_eq!(ids(&unmanaged), ["img-1"]);
    assert_eq!(
        booked_sums(&unmanaged),
        (res(16_000, 8 * GIB), res(2_000, GIB))
    );
    let both = sort(&records, &[], |_| true).unwrap();
    assert_eq!(ids(&both), ["vm-1", "img-1"]);
    assert_eq!(
        booked_sums(&both),
        (CapacityResources::default(), res(2_000, GIB))
    );
}

/// What a window's sort books off the fleet's budget (A-10): of the VMs it doesn't learn, whole, and of the native boxes,
/// at their caps, each summed.
fn booked_sums(sorted: &Sorted<'_>) -> (CapacityResources, CapacityResources) {
    let mut whole = CapacityResources::default();
    let mut native = CapacityResources::default();
    for (holder, booking) in &sorted.booked {
        let sum = if holder.managed() || holder.image() {
            &mut whole
        } else {
            &mut native
        };
        *sum = sum.checked_add(*booking).unwrap();
    }
    (whole, native)
}

/// Image VM `name`'s host use with nothing used yet, its 16 vCPUs' guest time none and `held` held.
fn unused(io: &Record, name: &str, held: u64) {
    io.host.borrow_mut().insert(
        name.to_owned(),
        HostUse {
            cpu_ns: 0,
            footprint_bytes: held,
            resident_bytes: held,
            guest_ns: vec![0; 16],
            window: Duration::ZERO,
            charged: hex_api::Charged::ToThread,
        },
    );
}

/// Joint plan step 14 (C1; ROOT, 2026-11-13): a capping learner's window is the longest period of the caps it holds,
/// each image VM's its run's own (D247's `ImageGuest::rate_period`), re-read each window, never a number of its own:
/// its platform's cap's cycle counts only for a VM that cap holds. Do run a window of two image VMs whose runs say
/// 100 ms and 500 ms, on a learner whose platform's cap derives cycles; then one after the 500 ms run has ended; then one
/// after the other run says 700 ms (a period Shards derives later reaches Hex as it is). Expect its window after each
/// the longest of those it then holds: 500 ms, 100 ms, then 700 ms.
#[test]
fn w14_a_capping_learners_window_is_the_longest_period_of_the_caps_it_holds_re_read_each_window() {
    let io = Record::default();
    let short = image_vm("img-1", 1);
    let long = image_vm("img-2", 2);
    unused(&io, "img-1", GIB);
    unused(&io, "img-2", GIB);
    io.periods
        .borrow_mut()
        .insert("img-1".to_owned(), Duration::from_millis(100));
    io.periods
        .borrow_mut()
        .insert("img-2".to_owned(), Duration::from_millis(500));
    let budget = res(24_000, 64 * GIB);
    let mut learner = Learner::new(true, res(32_000, 64 * GIB)).unwrap();
    let start = Instant::now();
    learner
        .window(&io, &holders(&[short.clone(), long]), budget, start)
        .unwrap();
    assert_eq!(
        learner.next_window(),
        Some(Duration::from_millis(500)),
        "the longest run's period"
    );
    learner
        .window(
            &io,
            &holders(std::slice::from_ref(&short)),
            budget,
            start + Duration::from_millis(500),
        )
        .unwrap();
    assert_eq!(
        learner.next_window(),
        Some(Duration::from_millis(100)),
        "the shorter run's, once the longer run has ended: no cap of its platform's holds a VM"
    );
    io.periods
        .borrow_mut()
        .insert("img-1".to_owned(), Duration::from_millis(700));
    learner
        .window(
            &io,
            &holders(std::slice::from_ref(&short)),
            budget,
            start + Duration::from_millis(800),
        )
        .unwrap();
    assert_eq!(
        learner.next_window(),
        Some(Duration::from_millis(700)),
        "re-read each window"
    );
}

/// Joint plan step 14 (C1): the learner's thread waits out its window whatever comes meanwhile, as the window's end
/// reads every VM then held; with no window (a node with no platform window holding no image VM), until a box begins;
/// and ends once its node has gone. Do wait out a 40 ms window with a box's beginning told before it; wait with no
/// window, a box's beginning told; then wait, with no window and with one, once the node's end has dropped. Expect the
/// first to look only once its window has passed, the second to look at once, and the last two gone.
#[test]
fn w14_the_learners_thread_waits_out_its_window_and_with_none_for_a_box_to_begin() {
    let (begun, waits) = hex_wait::mpsc::sync_channel(1).unwrap();
    begun.try_send(()).unwrap();
    let window = Duration::from_millis(40);
    let asked = Instant::now();
    assert_eq!(wait(&waits, Some(window)).unwrap(), Waited::Look);
    assert!(
        asked.elapsed() >= window,
        "a beginning doesn't cut a window short"
    );
    begun.try_send(()).unwrap();
    assert_eq!(wait(&waits, None).unwrap(), Waited::Look);
    drop(begun);
    assert_eq!(wait(&waits, None).unwrap(), Waited::Gone);
    assert_eq!(wait(&waits, Some(window)).unwrap(), Waited::Gone);
}

/// A-9 (the review's, 2026-11-14; AC-T7.4.1): a learner whose platform holds no window waited for a box to begin before
/// its first window, so the VMs its node's recovery took back and its node VM went unread, uncapped and untold on an
/// idle node until one did. Its thread runs a window as it starts, before it waits. Do run the learner's thread with
/// nothing ever to tell it (its wake's sender gone before it starts), counting its windows. Expect one window, its
/// start's, then its end.
#[test]
fn a9_the_learners_thread_runs_a_window_at_its_start_before_it_waits() {
    let (begun, waits) = hex_wait::mpsc::sync_channel(1).unwrap();
    drop(begun);
    let mut windows = 0;
    learn(&waits, || {
        windows += 1;
        Some(None)
    })
    .unwrap();
    assert_eq!(windows, 1, "its start's window, then its end");
}

/// A native owner with no window of its own, as Windows' is (its job's hard cap holds per an undocumented interval),
/// over the simulated runtime: its Shards image VMs' runs each say the period the test sets for its box, answer their
/// rooms and rates as told and read as using nothing, and it counts the learner's reads of a period.
#[derive(Default)]
struct Imaging {
    sim: hex_api::sim::SimRuntime,
    periods: std::sync::Mutex<BTreeMap<String, Duration>>,
    reads: std::sync::atomic::AtomicUsize,
    /// Each node VM's run's rate period, as the test sets it (D247): none for one it doesn't run.
    node_periods: std::sync::Mutex<BTreeMap<hex_api::NodeRun, Duration>>,
    /// Each node VM's host use read, and each rate its run was held to and room its guest was told, in order.
    node_reads: std::sync::atomic::AtomicUsize,
    node_rates: std::sync::Mutex<Vec<(hex_api::NodeRun, u64)>>,
    node_rooms: std::sync::Mutex<Vec<(hex_api::NodeRun, i64)>>,
    /// Each rate an image box's run was held to, in order.
    box_rates: std::sync::Mutex<Vec<(String, u64)>>,
    /// Whether the lock its node's capacity answers take is held, where a test watches it (A-2); each ask the learner
    /// made of it, and each made while that lock was held, in order.
    watch: std::sync::OnceLock<Box<dyn Fn() -> bool + Send + Sync>>,
    asked: std::sync::Mutex<Vec<&'static str>>,
    held: std::sync::Mutex<Vec<&'static str>>,
}

#[derive(Clone, Default)]
struct Images(std::sync::Arc<Imaging>);

impl Images {
    /// Notes the learner's ask `what`, and whether the lock its node's capacity answers take was held as it was made.
    fn ask(&self, what: &'static str) {
        self.0.asked.lock().unwrap().push(what);
        if self.0.watch.get().is_some_and(|held| held()) {
            self.0.held.lock().unwrap().push(what);
        }
    }
}

impl hex_api::Runtime for Images {
    fn classes(&self) -> Vec<hex_api::Class> {
        self.0.sim.classes()
    }
    fn create(&self, spec: BoxSpec) -> Result<BoxInfo, Refusal> {
        self.0.sim.create(spec)
    }
    fn start(&self, id: &BoxId) -> Result<(), Refusal> {
        self.0.sim.start(id)
    }
    fn exec(
        &self,
        id: &BoxId,
        program: hex_api::Program,
        stdio: hex_api::Stdio,
    ) -> Result<Box<dyn hex_api::Proc>, Refusal> {
        self.0.sim.exec(id, program, stdio)
    }
    fn run(
        &self,
        spec: BoxSpec,
        program: hex_api::Program,
        stdio: hex_api::Stdio,
    ) -> Result<Box<dyn hex_api::Proc>, Refusal> {
        self.0.sim.run(spec, program, stdio)
    }
    fn signal(&self, id: &BoxId, signal: hex_api::Signal) -> Result<(), Refusal> {
        self.0.sim.signal(id, signal)
    }
    fn inspect(&self, id: &BoxId) -> Result<BoxInfo, Refusal> {
        self.0.sim.inspect(id)
    }
    fn usage(&self, id: &BoxId) -> Result<Usage, Refusal> {
        self.0.sim.usage(id)
    }
    fn list(&self) -> Result<Vec<BoxInfo>, Refusal> {
        self.0.sim.list()
    }
    fn delete(&self, id: &BoxId) -> Result<(), Refusal> {
        self.0.sim.delete(id)
    }
    fn room(&self, _: &BoxId, room: Option<i64>) -> Result<Option<u64>, Refusal> {
        self.ask("room");
        Ok(room.and_then(|room| u64::try_from(room).ok()))
    }
    fn rate(
        &self,
        id: &BoxId,
        rate: Option<std::num::NonZeroU64>,
    ) -> Result<Option<std::num::NonZeroU64>, Refusal> {
        self.ask("rate");
        self.0.box_rates.lock().unwrap().push((
            id.as_str().to_owned(),
            rate.map_or(0, std::num::NonZeroU64::get),
        ));
        Ok(rate)
    }
    fn host_use(&self, _: &BoxId) -> Result<HostUse, Refusal> {
        self.ask("host_use");
        Ok(HostUse {
            cpu_ns: 0,
            footprint_bytes: GIB,
            resident_bytes: GIB,
            guest_ns: Vec::new(),
            window: Duration::ZERO,
            charged: hex_api::Charged::ToThread,
        })
    }
}

impl crate::recorded::Recover for Images {
    /// It keeps no native metadata of its own: nothing to cross-check against the retained cohort.
    fn preflight_retained(
        &self,
        _: &hex_runtime::devices::Records,
        _: &[hex_core::record::PoolRecord],
    ) -> Result<(), Refusal> {
        Ok(())
    }
    fn trace(&self, _: &BoxId) -> Result<Option<ProcId>, Refusal> {
        Ok(None)
    }
    fn adopt(&self, _: &BoxSpec, _: Option<ProcId>) -> Result<(), Refusal> {
        Ok(())
    }
    fn end(&self, _: Option<ProcId>) -> Result<(), Refusal> {
        Ok(())
    }
    /// Its run's use as it reads first: nothing run yet, a GiB held, over the life it has had, one of its periods.
    fn node_vm_host_use(&self, run: hex_api::NodeRun) -> Result<HostUse, Refusal> {
        self.ask("node_vm_host_use");
        self.0
            .node_reads
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(HostUse {
            cpu_ns: 0,
            footprint_bytes: GIB,
            resident_bytes: GIB,
            guest_ns: Vec::new(),
            window: self.node_vm_rate_period(run)?.unwrap_or_default(),
            charged: hex_api::Charged::ToThread,
        })
    }
    fn node_vm_rate_period(&self, run: hex_api::NodeRun) -> Result<Option<Duration>, Refusal> {
        self.ask("node_vm_rate_period");
        self.0
            .node_periods
            .lock()
            .unwrap()
            .get(&run)
            .copied()
            .map(Some)
            .ok_or_else(|| Refusal::Broken("a node VM this runtime doesn't run".into()))
    }
    fn node_vm_rate(
        &self,
        run: hex_api::NodeRun,
        rate: Option<std::num::NonZeroU64>,
    ) -> Result<Option<std::num::NonZeroU64>, Refusal> {
        self.ask("node_vm_rate");
        self.0
            .node_rates
            .lock()
            .unwrap()
            .push((run, rate.map_or(0, std::num::NonZeroU64::get)));
        Ok(rate)
    }
    fn node_vm_room(&self, run: hex_api::NodeRun, room: i64) -> Result<(), Refusal> {
        self.ask("node_vm_room");
        self.0.node_rooms.lock().unwrap().push((run, room));
        Ok(())
    }
    fn image_rate_period(&self, record: &BoxRecord) -> Result<Option<Duration>, Refusal> {
        self.ask("image_rate_period");
        self.0
            .reads
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        match self.0.periods.lock().unwrap().get(record.spec.id.as_str()) {
            Some(period) => Ok(Some(*period)),
            None => Err(Refusal::NotRunning(
                record.spec.id.try_clone()?,
                "rate_period",
            )),
        }
    }
}

/// The learner's thread woken by an image VM's making: a fact no recorded operation makes, so it measures, for its
/// fact alone (hex_testkit::waits).
static WOKEN: hex_core::patience::Patience =
    hex_core::patience::Patience::measuring("test.hex_node.charges_woken");

/// Joint plan step 14 (C1; ROOT, 2026-11-13): a node whose platform holds no window of its own (Windows': its job's
/// interval undocumented) still learns its Shards image VMs, each over its run's period. Do start the charge learner
/// of such a node; run a window holding no VM; then make an image VM whose run says 70 ms. Expect the learner started,
/// its window none while it holds no VM, the VM's making waking it to read the run's period, and its window then the
/// run's 70 ms.
#[test]
fn w14_a_node_with_no_platform_window_learns_its_image_vms_over_their_runs_periods() {
    let dir = tempfile::tempdir().unwrap();
    let images = Images::default();
    let node =
        std::sync::Arc::new(crate::recorded::Recorded::open(images.clone(), dir.path()).unwrap());
    node.start_charges().unwrap();
    assert_eq!(node.learn_window(), None, "no window while it holds no VM");
    images
        .0
        .periods
        .lock()
        .unwrap()
        .insert("img-1".to_owned(), Duration::from_millis(70));
    let spec = BoxSpec {
        id: BoxId::new("img-1").unwrap(),
        bounds: hex_api::Bounds::default(),
        class: ClassSpec::ShardsImage(Box::new(hex_api::ShardsImage::new(hex_api::sim::SIM_IMAGE))),
        owner: None,
        devices: Vec::new(),
        allocation: Some(hex_api::sim::sim_vm_allocation().unwrap()),
        route: None,
        band: 0,
    };
    hex_api::Runtime::create(node.as_ref(), spec).unwrap();
    // A look: the stand-in tells no event as the learner reads it, its count read. Its party, the learner, ends where
    // it has failed: its thread, detached, records why on the learner and stops.
    hex_testkit::waits::follow::until(
        &WOKEN,
        "the learner woken by an image VM's making",
        || match node.books.charges.get() {
            Some((learner, _)) if learner.lock().is_ok_and(|learner| learner.fault.is_none()) => {
                hex_testkit::waits::follow::Party::Live
            }
            _ => hex_testkit::waits::follow::Party::Ended,
        },
        || images.0.reads.load(std::sync::atomic::Ordering::SeqCst) > 0,
    )
    .unwrap();
    assert_eq!(node.learn_window(), Some(Duration::from_millis(70)));
}

/// Joint plan step 14 (AC-T7.4.1; ROOT, 2026-11-13: the starving case the done-when names): a running VM whose read
/// fails counts whole, at its receipt, against the fleet's budget until a read succeeds, its learner untouched, and
/// the failure is told. Do run a window of three Shards VMs of 16-CPU allocations, each booked at 4 CPUs, in a 24-CPU
/// fleet budget, the third's read failing, and answer the class's capacity; then one in which it reads. Expect the first
/// two capped at their bookings, the 16 CPUs of the third's receipt taken from the budget, the third named among the
/// unread with its refusal and carrying no holding on the answer, so the scheduler counts it at its receipt; then none
/// unread, the third, which has run no window, capped at its 16-CPU ceiling and the two idle at their bookings.
#[test]
fn w14_a_vm_whose_read_fails_counts_whole_against_the_budget_until_it_reads() {
    let io = Record::default();
    let vms = vec![vm("vm-1", 1), vm("vm-2", 2), vm("vm-3", 3)];
    io.set("vm-1", 0, GIB);
    io.set("vm-2", 0, GIB);
    let budget = res(24_000, 64 * GIB);
    let mut learner = Learner::new(true, res(32_000, 64 * GIB)).unwrap();
    let start = Instant::now();
    learner.window(&io, &holders(&vms), budget, start).unwrap();
    assert_eq!(
        *io.rates.borrow(),
        vec![("vm-1".to_owned(), 4_000), ("vm-2".to_owned(), 4_000)]
    );
    let unread: Vec<&str> = learner.unread().iter().map(|(id, _)| box_of(id)).collect();
    assert_eq!(unread, ["vm-3"]);
    let learned = learner
        .learned(Class::Shards, &[cost()], &holders(&vms))
        .unwrap()
        .unwrap();
    let held: Vec<&str> = learned
        .holdings
        .iter()
        .map(|h| match &h.id {
            hex_api::LearnedHolder::Box(id) => id.as_str(),
            hex_api::LearnedHolder::Node(run) => panic!("a node VM's holding, {run:?}"),
        })
        .collect();
    assert!(!held.contains(&"vm-3"), "{held:?}");
    io.rates.borrow_mut().clear();
    io.set("vm-3", 0, GIB);
    learner
        .window(&io, &holders(&vms), budget, start + WINDOW)
        .unwrap();
    assert!(learner.unread().is_empty());
    let caps: Vec<u64> = vms
        .iter()
        .map(|r| learner.rate(&Scope::of(r).unwrap()).unwrap())
        .collect();
    assert_eq!(caps, [4_000, 4_000, 16_000]);
}

/// Joint plan step 14 (C1; AC-T7.4.1; ROOT, 2026-11-13): an image VM whose run's period can't be read counts whole, at
/// its receipt, against the fleet's budget until it reads, its learner untouched and the failure told, as one whose use
/// can't be read does. Do run a window of three image VMs of 16 vCPUs, each booked at 4 CPUs, in a 24-CPU fleet budget,
/// the third's period unreadable, and answer the class's capacity; then one in which it reads. Expect the first two
/// capped at their bookings, the 16 CPUs of the third's receipt taken from the budget, the third named among the unread
/// with its period's refusal and carrying no holding on the answer; then none unread, the third, which has run no
/// window, at its 16-CPU ceiling and the two idle at their bookings.
#[test]
fn w14_an_image_vm_whose_period_read_fails_counts_whole_against_the_budget_until_it_reads() {
    let io = Record::default();
    let vms = vec![
        image_vm("img-1", 1),
        image_vm("img-2", 2),
        image_vm("img-3", 3),
    ];
    for name in ["img-1", "img-2", "img-3"] {
        unused(&io, name, GIB);
    }
    io.unread_periods.borrow_mut().push("img-3".to_owned());
    let budget = res(24_000, 64 * GIB);
    let mut learner = Learner::new(true, res(32_000, 64 * GIB)).unwrap();
    let start = Instant::now();
    learner.window(&io, &holders(&vms), budget, start).unwrap();
    assert_eq!(
        *io.rates.borrow(),
        vec![("img-1".to_owned(), 4_000), ("img-2".to_owned(), 4_000)]
    );
    let unread: Vec<(&str, String)> = learner
        .unread()
        .iter()
        .map(|(id, why)| (box_of(id), why.to_string()))
        .collect();
    assert_eq!(unread.len(), 1, "{unread:?}");
    assert!(
        unread
            .iter()
            .all(|(id, why)| *id == "img-3" && why.contains("period")),
        "{unread:?}"
    );
    let learned = learner
        .learned(Class::Shards, &[cost()], &holders(&vms))
        .unwrap()
        .unwrap();
    let held: Vec<&str> = learned
        .holdings
        .iter()
        .map(|h| match &h.id {
            hex_api::LearnedHolder::Box(id) => id.as_str(),
            hex_api::LearnedHolder::Node(run) => panic!("a node VM's holding, {run:?}"),
        })
        .collect();
    assert!(!held.contains(&"img-3"), "{held:?}");
    io.rates.borrow_mut().clear();
    io.unread_periods.borrow_mut().clear();
    learner
        .window(&io, &holders(&vms), budget, start + WINDOW)
        .unwrap();
    assert!(learner.unread().is_empty());
    let caps: Vec<u64> = vms
        .iter()
        .map(|r| learner.rate(&Scope::of(r).unwrap()).unwrap())
        .collect();
    assert_eq!(caps, [4_000, 4_000, 16_000]);
}

/// Joint plan step 14 (D255, C1; ROOT, 2026-11-13): a managed VM's governor holds its rate over its VM process's own
/// period, so the learner's window is the longest of those periods, re-read each window, as an image VM's run's is: its
/// platform's cap's cycle counts only for a VM that cap holds. Do run a window of a managed VM whose governor answered a
/// 500 ms period, on a learner whose platform's cap derives cycles; then one after its process answered 200 ms. Expect
/// the window 500 ms, then 200 ms.
#[test]
fn w14_d255_a_managed_vms_governor_period_is_the_learners_window() {
    let io = Record::default();
    let vms = vec![vm("vm-1", 1)];
    io.set("vm-1", 0, GIB);
    io.governed
        .borrow_mut()
        .insert("vm-1".to_owned(), Duration::from_millis(500));
    let budget = res(24_000, 64 * GIB);
    let mut learner = Learner::new(true, res(32_000, 64 * GIB)).unwrap();
    let start = Instant::now();
    learner.window(&io, &holders(&vms), budget, start).unwrap();
    assert_eq!(learner.next_window(), Some(Duration::from_millis(500)));
    io.governed
        .borrow_mut()
        .insert("vm-1".to_owned(), Duration::from_millis(200));
    learner
        .window(&io, &holders(&vms), budget, start + WINDOW)
        .unwrap();
    assert_eq!(learner.next_window(), Some(Duration::from_millis(200)));
}

/// Joint plan step 14 (D255; ROOT, 2026-11-13): what holds each managed VM's rate is told on its class's capacity
/// answer, those its governor doesn't hold alone first. Do run a window of three Shards VMs of 16-CPU allocations in a
/// 24-CPU budget, whose caps the learner sets: the first's governor holds it, the second's process takes no rate so its
/// platform's cap holds it, the third's governor holds it under its job; then answer the class's capacity. Expect the
/// rates told: the second's platform and why, the third under its job, then the first's governor.
#[test]
fn w14_d255_what_holds_each_managed_vms_rate_is_told_on_its_classs_capacity_answer() {
    let io = Record::default();
    let vms = vec![vm("vm-1", 1), vm("vm-2", 2), vm("vm-3", 3)];
    for name in ["vm-1", "vm-2", "vm-3"] {
        io.set(name, 0, GIB);
    }
    let unsupported = "its VM process takes no rate".to_owned();
    io.held.borrow_mut().insert(
        "vm-2".to_owned(),
        hex_api::RateHeld::Platform(unsupported.clone()),
    );
    io.held
        .borrow_mut()
        .insert("vm-3".to_owned(), hex_api::RateHeld::GovernorUnderJob);
    let budget = res(24_000, 64 * GIB);
    let mut learner = Learner::new(true, res(32_000, 64 * GIB)).unwrap();
    learner
        .window(&io, &holders(&vms), budget, Instant::now())
        .unwrap();
    assert_eq!(io.rates.borrow().len(), 3, "{:?}", io.rates.borrow());
    let learned = learner
        .learned(Class::Shards, &[cost()], &holders(&vms))
        .unwrap()
        .unwrap();
    let told: Vec<(&str, &hex_api::RateHeld)> = learned
        .rates
        .iter()
        .map(|rate| match &rate.id {
            hex_api::LearnedHolder::Box(id) => (id.as_str(), &rate.held),
            hex_api::LearnedHolder::Node(run) => panic!("a node VM's rate, {run:?}"),
        })
        .collect();
    assert_eq!(
        told,
        [
            ("vm-2", &hex_api::RateHeld::Platform(unsupported)),
            ("vm-3", &hex_api::RateHeld::GovernorUnderJob),
            ("vm-1", &hex_api::RateHeld::Governor),
        ]
    );
}

/// Joint plan step 14 (D255; ROOT, 2026-11-13): on a platform with no window (Windows today), the learner's first sight
/// of a managed VM asks its governor at the VM's ceiling, which holds nothing below its vCPUs: one whose governor answers
/// a period is learned over it and capped at its learned rate; one whose process takes no rate stays booked whole, told.
/// Do, on a learner with no platform window, see two Shards VMs of 16-CPU allocations, the first's governor answering a
/// 100 ms period and the second's process taking no rate; sort them, run a window of what it learns in a 6-CPU budget,
/// and answer the class's capacity. Expect both asked at their 16-CPU ceilings; the first learned and the second booked
/// whole; the first capped below its ceiling; its 100 ms the window; and the rates told: the second's platform and why,
/// then the first's governor.
#[test]
fn w14_d255_with_no_platform_window_a_managed_vm_is_learned_over_its_governors_period() {
    let io = Record::default();
    let vms = vec![vm("vm-1", 1), vm("vm-2", 2)];
    for name in ["vm-1", "vm-2"] {
        io.set(name, 0, GIB);
    }
    io.governed
        .borrow_mut()
        .insert("vm-1".to_owned(), Duration::from_millis(100));
    let unsupported = "its VM process takes no rate".to_owned();
    io.held.borrow_mut().insert(
        "vm-2".to_owned(),
        hex_api::RateHeld::Platform(unsupported.clone()),
    );
    let mut learner = Learner::new(false, res(32_000, 64 * GIB)).unwrap();
    learner.govern(&io, &holders(&vms)).unwrap();
    assert_eq!(
        *io.rates.borrow(),
        vec![("vm-1".to_owned(), 16_000), ("vm-2".to_owned(), 16_000)]
    );
    let sorted = sort(&vms, &[], |holder| learner.learns(holder)).unwrap();
    let learned: Vec<String> = sorted
        .learned
        .iter()
        .map(|holder| box_of(&holder.id().unwrap()).to_owned())
        .collect();
    assert_eq!(learned, ["vm-1"]);
    assert_eq!(
        booked_sums(&sorted).0.cpu_milli,
        16_000,
        "the second's receipt"
    );
    io.rates.borrow_mut().clear();
    learner
        .window(&io, &sorted.learned, res(6_000, 64 * GIB), Instant::now())
        .unwrap();
    let rates = io.rates.borrow().clone();
    assert!(
        matches!(rates.as_slice(), [(id, rate)] if id == "vm-1" && *rate < 16_000),
        "{rates:?}"
    );
    assert_eq!(learner.next_window(), Some(Duration::from_millis(100)));
    let answer = learner
        .learned(Class::Shards, &[cost()], &holders(&vms))
        .unwrap()
        .unwrap();
    let told: Vec<(&str, &hex_api::RateHeld)> = answer
        .rates
        .iter()
        .map(|rate| match &rate.id {
            hex_api::LearnedHolder::Box(id) => (id.as_str(), &rate.held),
            hex_api::LearnedHolder::Node(run) => panic!("a node VM's rate, {run:?}"),
        })
        .collect();
    assert_eq!(
        told,
        [
            ("vm-2", &hex_api::RateHeld::Platform(unsupported)),
            ("vm-1", &hex_api::RateHeld::Governor),
        ]
    );
}

/// A managed Shards VM's guest time as its VM process answers its use (D260): each of its 16 vCPUs at `each_ns`, read
/// now.
fn sixteen(each_ns: u64) -> ManagedGuestTime {
    let at = Instant::now();
    ManagedGuestTime::PerVcpu {
        ns: vec![each_ns; 16],
        asked: at,
        answered: at,
        charged: hex_api::Charged::ToThread,
    }
}

/// Joint plan step 14 (D260; AC-T7.4.1; ROOT, 2026-11-13): a managed VM is held split from its VM process's first use
/// answer, its governor at its share less its VMM's own: over its life so far at that first answer, then over each
/// window. Do run a capping learner's first window over a managed Shards VM of 16 vCPUs, booked at 4 CPUs in a 24-CPU
/// budget, whose VM process answers its first use as the window reads it: its 16 vCPUs ran 2.5 s of its guest and its
/// scope 3 s of CPU in the 10 s it has run; then a window 300 ms on, in which its scope ran 360 ms of CPU, 300 ms of it
/// its vCPUs'. Expect it capped in its first window at its 16-CPU share with its VMM's own over its life, 50 milli-CPU,
/// left out of its governor's rate; then at its 4-CPU booking with its VMM's own over the window, 200 milli-CPU.
#[test]
fn w14_d260_a_managed_vm_is_held_split_from_its_first_use_answer() {
    let io = Record::default();
    let vm = vm("vm-1", 1);
    io.set("vm-1", 3_000_000, GIB);
    io.guest
        .borrow_mut()
        .insert("vm-1".to_owned(), sixteen(156_250_000));
    io.lives
        .borrow_mut()
        .insert("vm-1".to_owned(), Duration::from_secs(10));
    let budget = res(24_000, 64 * GIB);
    let mut learner = Learner::new(true, res(32_000, 64 * GIB)).unwrap();
    let start = Instant::now();
    learner
        .window(&io, &holders(std::slice::from_ref(&vm)), budget, start)
        .unwrap();
    assert_eq!(*io.caps.borrow(), vec![("vm-1".to_owned(), 16_000, Ok(50))]);
    io.set("vm-1", 3_360_000, GIB);
    io.guest
        .borrow_mut()
        .insert("vm-1".to_owned(), sixteen(175_000_000));
    learner
        .window(
            &io,
            &holders(std::slice::from_ref(&vm)),
            budget,
            start + WINDOW,
        )
        .unwrap();
    assert_eq!(
        io.caps.borrow().last(),
        Some(&("vm-1".to_owned(), 4_000, Ok(200)))
    );
}

/// The review's A-11: a managed VM of 16 vCPUs whose scope runs 1.8 CPUs, 1.6 of them its vCPUs' (200 milli-CPU its
/// VMM's own), from 3 s of scope CPU and 2.5 s of guest time in the 10 s it has run as its first window reads it.
fn a11_steady(io: &Record, start: Instant) -> BoxRecord {
    io.time.set(Some(start));
    io.steady.borrow_mut().insert(
        "vm-1".to_owned(),
        Steady {
            from: start,
            scope_usec: 3_000_000,
            scope_milli: 1_800,
            vcpu_ns: 156_250_000,
            vcpu_milli: 100,
        },
    );
    io.lives
        .borrow_mut()
        .insert("vm-1".to_owned(), Duration::from_secs(10));
    vm("vm-1", 1)
}

/// The review's A-11: a VM's use over a window is over the span between its own reads, not between the windows' starts,
/// as the reads of the VMs before it in its window take time. Do run two windows 300 ms apart over a managed VM whose
/// VMM runs 200 milli-CPU of its own ([`a11_steady`]), its second window's reads of it coming 100 ms after that window
/// starts. Expect its second cap's VMM's own at 200 milli-CPU, 80 ms over the 400 ms between its reads, never over the
/// windows' 300 ms.
#[test]
fn a11_a_vms_window_is_the_span_between_its_own_reads() {
    let io = Record::default();
    let start = Instant::now();
    let vm = a11_steady(&io, start);
    let budget = res(24_000, 64 * GIB);
    let mut learner = Learner::new(true, res(32_000, 64 * GIB)).unwrap();
    let clock = || io.time.get().unwrap();
    learner
        .window_by(&io, &holders(std::slice::from_ref(&vm)), budget, clock)
        .unwrap();
    io.time.set(Some(start + WINDOW));
    io.lag.set(Duration::from_millis(100));
    learner
        .window_by(&io, &holders(std::slice::from_ref(&vm)), budget, clock)
        .unwrap();
    let caps = io.caps.borrow();
    assert!(
        matches!(caps.last(), Some((name, _, Ok(200))) if name == "vm-1"),
        "{caps:?}"
    );
}

/// The review's A-11: a VM's VMM's own is its scope's CPU less its vCPUs' at the same instant, its VM process's sample
/// of its guest time, never counting what its busy vCPUs ran while its use's answer came back. Do run a window over a
/// managed VM whose VMM runs 200 milli-CPU of its own ([`a11_steady`]), its process answering its use at once; then one
/// 300 ms on whose answer takes 40 ms, its sample taken halfway. Expect its first cap's VMM's own over its life,
/// 50 milli-CPU; its second's 200 milli-CPU, never what its 1.6 busy CPUs ran in the answer's last 20 ms.
#[test]
fn a11_a_vms_own_is_its_scope_less_its_vcpus_at_its_sample() {
    let io = Record::default();
    let start = Instant::now();
    let vm = a11_steady(&io, start);
    let budget = res(24_000, 64 * GIB);
    let mut learner = Learner::new(true, res(32_000, 64 * GIB)).unwrap();
    let clock = || io.time.get().unwrap();
    learner
        .window_by(&io, &holders(std::slice::from_ref(&vm)), budget, clock)
        .unwrap();
    io.time.set(Some(start + WINDOW));
    io.round_trip.set(Duration::from_millis(40));
    learner
        .window_by(&io, &holders(std::slice::from_ref(&vm)), budget, clock)
        .unwrap();
    let caps = io.caps.borrow();
    assert!(
        matches!(caps.first(), Some((name, _, Ok(50))) if name == "vm-1"),
        "{caps:?}"
    );
    assert!(
        matches!(caps.last(), Some((name, _, Ok(200))) if name == "vm-1"),
        "{caps:?}"
    );
}

/// Joint plan step 14 (D260; AC-T7.4.1; ROOT, 2026-11-13): until its VM process's first use answer a managed VM's VMM's
/// own isn't read apart, and its cap says why; its first answer, whenever it comes, splits it over its life so far. Do
/// run a window over a managed Shards VM of 16 vCPUs, booked at 4 CPUs, whose process answers its microVM hasn't
/// started, its scope having run 2.94 s of CPU; then one 300 ms on whose answer tells its 16 vCPUs ran 2.5 s of its
/// guest in the 10 s it has run, its scope 3 s. Expect its first cap at its 16-CPU share, its VMM's own unheld in its
/// process's words; its second at its 4-CPU booking, its VMM's own over its life, 50 milli-CPU.
#[test]
fn w14_d260_until_its_first_use_answer_a_vms_own_is_unheld_and_told() {
    let io = Record::default();
    let vm = vm("vm-1", 1);
    io.set("vm-1", 2_940_000, GIB);
    io.guest.borrow_mut().insert(
        "vm-1".to_owned(),
        ManagedGuestTime::Unheld("the microVM has not started".to_owned()),
    );
    let budget = res(24_000, 64 * GIB);
    let mut learner = Learner::new(true, res(32_000, 64 * GIB)).unwrap();
    let start = Instant::now();
    learner
        .window(&io, &holders(std::slice::from_ref(&vm)), budget, start)
        .unwrap();
    assert_eq!(
        *io.caps.borrow(),
        vec![(
            "vm-1".to_owned(),
            16_000,
            Err("the microVM has not started".to_owned())
        )]
    );
    io.set("vm-1", 3_000_000, GIB);
    io.guest
        .borrow_mut()
        .insert("vm-1".to_owned(), sixteen(156_250_000));
    io.lives
        .borrow_mut()
        .insert("vm-1".to_owned(), Duration::from_secs(10));
    learner
        .window(
            &io,
            &holders(std::slice::from_ref(&vm)),
            budget,
            start + WINDOW,
        )
        .unwrap();
    assert_eq!(
        io.caps.borrow().last(),
        Some(&("vm-1".to_owned(), 4_000, Ok(50)))
    );
}

/// Joint plan step 14 (D260; AC-T7.4.1; ROOT, 2026-11-13): a VM's governor is told its share less its VMM's own, never
/// none, and what holds it says so. Do take its governor's rate and what holds it, on a platform whose job holds its
/// share and on one with none, for a 4-CPU share whose VMM's own is 300 milli-CPU; is the whole share; is past it; and
/// isn't read. Expect 3,700, held by its governor; the least rate a governor takes, 1 milli-CPU (`RATE`'s unit, Shards'
/// D247 and D255 taking milli-CPU and none letting the vCPUs be), told as its VMM's own taking its share on either
/// platform, a finding; and while unread its whole share, under its job where it has one, else its VMM's own unheld,
/// why told.
#[test]
fn w14_d260_a_vms_governor_is_its_share_less_its_vmms_own_never_none_and_says_so() {
    assert_eq!(GOVERNOR_LEAST, NonZeroU64::MIN);
    let cap = |own| Cap { share: 4_000, own };
    let read = cap(Own::Read(300));
    assert_eq!(read.governor().get(), 3_700);
    for job in [true, false] {
        assert_eq!(read.held(job), hex_api::RateHeld::Governor);
    }
    for own in [4_000, 5_000] {
        let eaten = cap(Own::Read(own));
        assert_eq!(eaten.governor(), GOVERNOR_LEAST);
        for job in [true, false] {
            assert_eq!(
                eaten.held(job),
                hex_api::RateHeld::OwnTakesShare { own, share: 4_000 }
            );
        }
    }
    let unheld = cap(Own::Unheld("the microVM has not started"));
    assert_eq!(unheld.governor().get(), 4_000);
    assert_eq!(unheld.held(true), hex_api::RateHeld::GovernorUnderJob);
    assert_eq!(
        unheld.held(false),
        hex_api::RateHeld::GovernorOwnUnheld("the microVM has not started".to_owned())
    );
}

/// Joint plan step 14 (D260; AC-T7.4.1; ROOT, 2026-11-13): an image VM's governor takes its share less its VMM's own
/// too, split from its run's first answer: Σown past the fleet's budget is what D260 closes. Do run a window of an image
/// VM of 16 vCPUs whose run host says its VM process ran 3 s of CPU, 2.5 s of it its vCPUs' guest time, in the 10 s since
/// its run started; then one 300 ms on, in which it ran 360 ms, 300 ms of it its vCPUs'. Expect its first cap at its
/// 16-CPU share, its VMM's own over its run, 50 milli-CPU; its second at its 4-CPU booking, its VMM's own over the
/// window, 200 milli-CPU.
#[test]
fn w14_d260_an_image_vms_governor_takes_its_share_less_its_vmms_own() {
    let io = Record::default();
    let vm = image_vm("img-1", 1);
    let use_at = |cpu_ns: u64, each_ns: u64, window: Duration| HostUse {
        cpu_ns,
        footprint_bytes: GIB,
        resident_bytes: GIB,
        guest_ns: vec![each_ns; 16],
        window,
        charged: hex_api::Charged::ToThread,
    };
    io.host.borrow_mut().insert(
        "img-1".to_owned(),
        use_at(3_000_000_000, 156_250_000, Duration::from_secs(10)),
    );
    let budget = res(24_000, 64 * GIB);
    let mut learner = Learner::new(true, res(32_000, 64 * GIB)).unwrap();
    let start = Instant::now();
    learner
        .window(&io, &holders(std::slice::from_ref(&vm)), budget, start)
        .unwrap();
    assert_eq!(
        *io.caps.borrow(),
        vec![("img-1".to_owned(), 16_000, Ok(50))]
    );
    io.host.borrow_mut().insert(
        "img-1".to_owned(),
        use_at(3_360_000_000, 175_000_000, Duration::from_millis(10_300)),
    );
    learner
        .window(
            &io,
            &holders(std::slice::from_ref(&vm)),
            budget,
            start + WINDOW,
        )
        .unwrap();
    assert_eq!(
        io.caps.borrow().last(),
        Some(&("img-1".to_owned(), 4_000, Ok(200)))
    );
}

/// Node VM `n`'s run.
fn node_run(n: u8) -> hex_api::NodeRun {
    hex_api::NodeRun::from_bytes([n; 16])
}

/// Node VM `n`'s row in `phase`, sized at the Shards profile's admission, an Ended one with its exit.
fn node_row(n: u8, phase: hex_api::NodeVmPhase) -> hex_core::record::NodeVmRecord {
    hex_core::record::NodeVmRecord {
        version: hex_core::record::RECORD_VERSION,
        run: node_run(n),
        image:
            "hex-node:rev@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                .to_owned(),
        allocation: cost().allocate(CapacityResources::default()).unwrap(),
        phase,
        diagnostics: None,
        exit: if phase == hex_api::NodeVmPhase::Ended {
            Some(hex_api::Exit::Signal(9))
        } else {
            None
        },
        last_resort: None,
        started: None,
    }
}

/// Why a last resort ended node VM `n`: 3 GiB held of a 2 GiB budget in band 0, its fleet past its budget.
fn node_end(n: u8) -> LearnedEnd {
    LearnedEnd {
        id: hex_api::LearnedHolder::Node(node_run(n)),
        held: 3 * GIB,
        budget: 2 * GIB,
        band: 0,
        fleet_held: 9 * GIB,
        fleet_growth: GIB,
        fleet_budget: 8 * GIB,
        unmoved: Some(Upstream::LiveMove),
    }
}

/// `row` as an earlier node process left it in the node state `dir`: its bytes in its place, its lock held by none.
fn leave_node_row(dir: &std::path::Path, row: &hex_core::record::NodeVmRecord) {
    let rows = dir.join(".node-vms");
    std::fs::create_dir_all(&rows).unwrap();
    let name: String = row.run.bytes().iter().map(|b| format!("{b:02x}")).collect();
    std::fs::write(rows.join(format!("{name}.json")), row.encode().unwrap()).unwrap();
}

/// A-9 (the review's, 2026-11-14; AC-T7.4.1): a node VM's boot tells its node's learner, as a box's create and start
/// do, so a learner holding no cap reads it as it boots, never at the next box's beginning. Do set the charge learner of
/// a node whose platform holds no window, then write a node VM's row through the node's books as the image class's boot
/// does: Booting, then Ready. Expect no wake told while it boots, and one once it is Ready.
#[test]
fn a9_a_node_vms_boot_wakes_its_learner() {
    let dir = tempfile::tempdir().unwrap();
    let node = std::sync::Arc::new(
        crate::recorded::Recorded::open(Images::default(), dir.path()).unwrap(),
    );
    let (waits, _, _) = node.set_charges().unwrap();
    let books: &dyn hex_runtime::devices::Books = &*node;
    books
        .put_node_vm(&node_row(7, hex_api::NodeVmPhase::Booting))
        .unwrap();
    assert!(
        matches!(waits.try_recv(), Err(hex_wait::mpsc::TryRecvError::Empty)),
        "a booting node VM is no VM to learn yet"
    );
    books
        .put_node_vm(&node_row(7, hex_api::NodeVmPhase::Ready))
        .unwrap();
    assert!(
        matches!(waits.try_recv(), Ok(())),
        "its Ready row wakes the learner"
    );
}

/// The learner's start window, read from the stand-in's asks: a fact no recorded operation makes, so it measures, for
/// its fact alone (hex_testkit::waits).
static STARTED: hex_core::patience::Patience =
    hex_core::patience::Patience::measuring("test.hex_node.charges_started");

/// A-9 (the review's, 2026-11-14; AC-T7.4.1): a node's learner reads what its node holds as it starts, with no box
/// begun, its own thread's first window reading, capping and telling each VM then held. Do keep, on a node whose
/// platform holds no window, a Shards image VM and a node VM's row Ready, each run saying 70 ms; then start its charge
/// learner and begin no box. Expect each VM's use read, its rate held and its guest told its room.
#[test]
fn a9_a_learner_reads_what_its_node_holds_at_its_start_with_no_box_begun() {
    let dir = tempfile::tempdir().unwrap();
    let images = Images::default();
    images
        .0
        .periods
        .lock()
        .unwrap()
        .insert("img-1".to_owned(), Duration::from_millis(70));
    images
        .0
        .node_periods
        .lock()
        .unwrap()
        .insert(node_run(7), Duration::from_millis(70));
    let node =
        std::sync::Arc::new(crate::recorded::Recorded::open(images.clone(), dir.path()).unwrap());
    let store = &node.books.store;
    store.put(&image_vm("img-1", 1)).unwrap();
    store
        .put_node_vm(&node_row(7, hex_api::NodeVmPhase::Booting))
        .unwrap();
    store
        .put_node_vm(&node_row(7, hex_api::NodeVmPhase::Ready))
        .unwrap();
    node.start_charges().unwrap();
    let wanted = [
        "host_use",
        "rate",
        "room",
        "node_vm_host_use",
        "node_vm_rate",
        "node_vm_room",
    ];
    // A look: the stand-in tells no event as the learner asks it. Its party, the learner, ends where it has failed: its
    // thread, detached, records why on the learner.
    hex_testkit::waits::follow::until(
        &STARTED,
        "the learner's start window",
        || match node.books.charges.get() {
            Some((learner, _)) if learner.lock().is_ok_and(|learner| learner.fault.is_none()) => {
                hex_testkit::waits::follow::Party::Live
            }
            _ => hex_testkit::waits::follow::Party::Ended,
        },
        || {
            let asked = images.0.asked.lock().unwrap();
            wanted.iter().all(|what| asked.contains(what))
        },
    )
    .unwrap();
}

/// Joint plan step 39, place 1 (C5, intent first; ROOT, 2026-11-13: a node VM's last-resort end lives in its row): the
/// last resort's intent for a node VM goes into its row, as a box's goes into its record. Do write why it is ending node
/// VM 7 into its Ready row through the write the learner makes; withdraw it; write it under another lifetime's scope;
/// and write one for a run with no row. Expect the end written and read back from the row, then gone; the other
/// lifetime's and the rowless run's not written.
#[test]
fn w39_the_last_resorts_intent_for_a_node_vm_is_written_into_its_row() {
    let dir = tempfile::tempdir().unwrap();
    let store = crate::store::Store::open(dir.path(), &dir.path().join("@records")).unwrap();
    store
        .put_node_vm(&node_row(7, hex_api::NodeVmPhase::Booting))
        .unwrap();
    store
        .put_node_vm(&node_row(7, hex_api::NodeVmPhase::Ready))
        .unwrap();
    let holder = hex_api::LearnedHolder::Node(node_run(7));
    let scope = Scope::Run(node_run(7));
    let row = |store: &crate::store::Store| store.node_vm(&node_run(7)).unwrap().unwrap();
    assert!(record_last_resort(&store, &holder, scope, Some(&node_end(7))).unwrap());
    assert_eq!(row(&store).last_resort, Some(node_end(7)));
    assert!(record_last_resort(&store, &holder, scope, None).unwrap());
    assert_eq!(row(&store).last_resort, None);
    assert!(
        !record_last_resort(&store, &holder, Scope::Run(node_run(8)), Some(&node_end(7))).unwrap()
    );
    assert_eq!(row(&store).last_resort, None);
    let rowless = hex_api::LearnedHolder::Node(node_run(9));
    assert!(
        !record_last_resort(
            &store,
            &rowless,
            Scope::Run(node_run(9)),
            Some(&node_end(9))
        )
        .unwrap()
    );
}

/// Joint plan step 39, place 1 (AC-T7.4.1; ROOT, 2026-11-13): a node VM's last-resort end survives its node's restart in
/// its row, and the next node's learner carries it, so its class's answers tell it as they told it before. Do leave
/// node VM 4's row Ended and node VM 5's Ready with no process holding its lock, each with why the last resort ended
/// it, as an earlier node would; then open a node over them, recover it and start its charge learner. Expect the
/// recovery to say both ends on record, and the learner's Shards ends to name both node VMs, by their runs.
#[test]
fn w39_a_node_vms_last_resort_end_is_read_back_at_start_into_the_learners_ends() {
    let dir = tempfile::tempdir().unwrap();
    let mut ended = node_row(4, hex_api::NodeVmPhase::Ended);
    ended.last_resort = Some(node_end(4));
    let mut unowned = node_row(5, hex_api::NodeVmPhase::Ready);
    unowned.last_resort = Some(node_end(5));
    leave_node_row(dir.path(), &ended);
    leave_node_row(dir.path(), &unowned);
    let node = std::sync::Arc::new(
        crate::recorded::Recorded::open(Images::default(), dir.path()).unwrap(),
    );
    let recovered = node.recover().unwrap();
    assert_eq!(
        recovered.node_vm_ends,
        crate::store::NodeVmEnds::Carried(vec![node_end(4), node_end(5)])
    );
    node.start_charges().unwrap();
    let (learner, _) = node.books.charges.get().unwrap();
    let learned = learner
        .lock()
        .unwrap()
        .learned(Class::Shards, &[cost()], &[])
        .unwrap()
        .unwrap();
    let mut ended: Vec<LearnedEnd> = learned.ended;
    ended.sort_by_key(|end| match &end.id {
        hex_api::LearnedHolder::Node(run) => *run,
        hex_api::LearnedHolder::Box(id) => panic!("a box's end, {id}"),
    });
    assert_eq!(ended, [node_end(4), node_end(5)]);
}

/// Joint plan step 39, place 1 (ROOT, 2026-11-13: a cold start is told, never covered by a guess): do open and recover
/// a node over a state no node VM row lies in. Expect the recovery to say no node VM's last-resort end is on record.
#[test]
fn w39_a_node_with_no_node_vm_end_on_record_says_so_at_start() {
    let dir = tempfile::tempdir().unwrap();
    let node = crate::recorded::Recorded::open(Images::default(), dir.path()).unwrap();
    let recovered = node.recover().unwrap();
    assert_eq!(
        recovered.node_vm_ends,
        crate::store::NodeVmEnds::NoneOnRecord
    );
    assert_eq!(
        recovered.node_vm_ends.to_string(),
        "node VM: no last-resort end on record"
    );
}

/// Joint plan step 14 (C4; ROOT, 2026-11-13; AC-T7.4.1): each VM's share is held at least at its own actuator's least,
/// never Hex's cgroup least: an image VM's governor's, a thousandth of a CPU (`GOVERNOR_LEAST`); a managed VM's its
/// platform's cap's, here a Windows job's on 128 processors, 13 thousandths (`hex_core::job::least_milli`). Do run a
/// window of an image VM and a managed VM, each booked no CPU, in a fleet budget of a thousandth of a CPU. Expect the
/// image VM's share 1 and the managed VM's 13.
#[test]
fn w14_each_vms_share_is_held_at_its_own_actuators_least() {
    let io = Record::default();
    let mut image = image_vm("img-1", 1);
    let mut managed = vm("vm-1", 2);
    for record in [&mut image, &mut managed] {
        record.spec.allocation = Some(
            cost()
                .allocate(CapacityResources::default())
                .unwrap()
                .booked_at(res(0, 4 * GIB)),
        );
    }
    unused(&io, "img-1", GIB);
    io.set("vm-1", 0, GIB);
    io.leasts.borrow_mut().insert("vm-1".to_owned(), 13);
    let mut learner = Learner::new(true, res(32_000, 64 * GIB)).unwrap();
    learner
        .window(
            &io,
            &holders(&[image, managed]),
            res(1, 64 * GIB),
            Instant::now(),
        )
        .unwrap();
    let mut shares = io.rates.borrow().clone();
    shares.sort();
    assert_eq!(
        shares,
        vec![("img-1".to_owned(), 1), ("vm-1".to_owned(), 13)]
    );
}

/// The review's A-14 (2c-4b; ROOT, 2026-11-13: a cold start told, never guessed): a VM its platform's cap holds, whose
/// cap's cycle isn't known yet (its helper has measured no stop), is held at its share with no cycle in the learner's
/// window, and what holds its rate says so. Do run a window of a managed VM whose process takes no rate, so its
/// platform's cap holds it, on a learner whose platform's cap's cycle is cold; then answer the class's capacity. Expect
/// no window taken from it, and its rate told held by its platform's cap, naming why its cycle is cold.
#[test]
fn w14_2c4b_a_cold_cap_cycle_is_told_on_its_vms_rate() {
    let io = Record::default();
    let held = vm("vm-2", 2);
    io.set("vm-2", 0, GIB);
    io.held.borrow_mut().insert(
        "vm-2".to_owned(),
        hex_api::RateHeld::Platform("its VM process takes no rate".into()),
    );
    *io.cold.borrow_mut() = Some("its helper has measured no stop".to_owned());
    let mut learner = Learner::new(true, res(32_000, 64 * GIB)).unwrap();
    learner
        .window(
            &io,
            &holders(std::slice::from_ref(&held)),
            res(24_000, 64 * GIB),
            Instant::now(),
        )
        .unwrap();
    assert_eq!(
        learner.next_window(),
        None,
        "no cycle taken from a cold cap"
    );
    let rates = learner
        .learned(
            Class::Shards,
            &[cost()],
            &holders(std::slice::from_ref(&held)),
        )
        .unwrap()
        .unwrap()
        .rates;
    assert_eq!(
        rates,
        [hex_api::LearnedRate {
            id: hex_api::LearnedHolder::Box(BoxId::new("vm-2").unwrap()),
            held: hex_api::RateHeld::Platform(
                "its VM process takes no rate; its helper has measured no stop".to_owned()
            ),
        }]
    );
}

/// The review's A-14 (ROOT, 2026-11-14; AC-T7.4.1): a VM its platform's cap holds, whose cap's cycle is at a cold
/// estimate (its helper's slowest stop, too few holds to have seen their p99.9), has that cycle taken into the learner's
/// window, and what holds its rate says it is cold. Do run a window of a managed VM whose process takes no rate, so its
/// platform's cap holds it, on a learner whose platform's cap's cycle is 2 s at a cold estimate; then answer the class's
/// capacity. Expect the next window 2 s, told naming the VM, and its rate told held by its platform's cap, naming why
/// its cycle is cold.
#[test]
fn a14_a_cold_estimates_cycle_sets_the_window_and_is_told_on_its_vms_rate() {
    let io = Record::default();
    let held = vm("vm-2", 2);
    io.set("vm-2", 0, GIB);
    io.held.borrow_mut().insert(
        "vm-2".to_owned(),
        hex_api::RateHeld::Platform("its VM process takes no rate".into()),
    );
    io.cycle.set(Some(Duration::from_secs(2)));
    *io.cold.borrow_mut() = Some("its helper's slowest stop, not their p99.9".to_owned());
    let mut learner = Learner::new(true, res(32_000, 64 * GIB)).unwrap();
    learner
        .window(
            &io,
            &holders(std::slice::from_ref(&held)),
            res(24_000, 64 * GIB),
            Instant::now(),
        )
        .unwrap();
    assert_eq!(learner.next_window(), Some(Duration::from_secs(2)));
    let learned = learner
        .learned(
            Class::Shards,
            &[cost()],
            &holders(std::slice::from_ref(&held)),
        )
        .unwrap()
        .unwrap();
    assert_eq!(
        learned.window,
        Some(hex_api::LearnedWindow {
            window_ns: 2_000_000_000,
            slowed_by: Some(LearnedHolder::Box(BoxId::new("vm-2").unwrap())),
        })
    );
    assert_eq!(
        learned.rates,
        [hex_api::LearnedRate {
            id: hex_api::LearnedHolder::Box(BoxId::new("vm-2").unwrap()),
            held: hex_api::RateHeld::Platform(
                "its VM process takes no rate; its helper's slowest stop, not their p99.9"
                    .to_owned()
            ),
        }]
    );
}

/// Joint plan step 14 (C1; ROOT, 2026-11-13): a platform cap's cycle sets the learner's window only for a VM that cap
/// holds, at that VM's own share, and where it does, the class's capacity answer names that VM and the window: every
/// VM's read waits on it, a slowdown told, never silent. Do run a window of a managed VM its governor holds over a
/// 100 ms period, on a learner whose platform's cap holds a VM over 2 s at any rate; then a window with a second managed
/// VM whose process takes no rate, so that cap holds it; then answer the class's capacity. Expect the first window's
/// next window 100 ms, told with no VM named; then 2 s, told naming the second VM.
#[test]
fn w14_a_platform_caps_cycle_sets_the_window_only_for_the_vms_it_holds_and_is_told() {
    let io = Record::default();
    let governed = vm("vm-1", 1);
    let held = vm("vm-2", 2);
    io.set("vm-1", 0, GIB);
    io.set("vm-2", 0, GIB);
    io.governed
        .borrow_mut()
        .insert("vm-1".to_owned(), Duration::from_millis(100));
    io.held.borrow_mut().insert(
        "vm-2".to_owned(),
        hex_api::RateHeld::Platform("its VM process takes no rate".into()),
    );
    io.cycle.set(Some(Duration::from_secs(2)));
    let budget = res(24_000, 64 * GIB);
    let mut learner = Learner::new(true, res(32_000, 64 * GIB)).unwrap();
    let start = Instant::now();
    learner
        .window(
            &io,
            &holders(std::slice::from_ref(&governed)),
            budget,
            start,
        )
        .unwrap();
    assert_eq!(learner.next_window(), Some(Duration::from_millis(100)));
    let told = |learner: &Learner| {
        learner
            .learned(
                Class::Shards,
                &[cost()],
                &holders(&[governed.clone(), held.clone()]),
            )
            .unwrap()
            .unwrap()
            .window
    };
    assert_eq!(
        told(&learner),
        Some(hex_api::LearnedWindow {
            window_ns: 100_000_000,
            slowed_by: None,
        })
    );
    learner
        .window(
            &io,
            &holders(&[governed.clone(), held.clone()]),
            budget,
            start + Duration::from_millis(100),
        )
        .unwrap();
    assert_eq!(learner.next_window(), Some(Duration::from_secs(2)));
    assert_eq!(
        told(&learner),
        Some(hex_api::LearnedWindow {
            window_ns: 2_000_000_000,
            slowed_by: Some(LearnedHolder::Box(BoxId::new("vm-2").unwrap())),
        })
    );
}

/// Joint plan step 39, place 3 (ROOT, 2026-11-13: a node VM's band is what ending it ends, and its unknown band told):
/// do derive a node VM's band from its node's records of oci boxes in bands 1 and 3, one of band 7 whose run ended and a
/// process box of band 9; then with a record that can't be read beside them; then from no record. Expect band 3, the
/// highest of the boxes it runs, known; then the highest band there is, as a box it runs may be of any band, told why,
/// naming the box whose record can't be read and its words; then band 0, as ending it ends no box.
#[test]
fn w39_a_node_vms_band_is_the_highest_of_the_boxes_it_runs() {
    let boxed =
        |name: &str, class: ClassSpec, band: u8, ended: bool| -> Result<BoxRecord, String> {
            let mut record = BoxRecord::new(
                BoxSpec {
                    id: BoxId::new(name).unwrap(),
                    bounds: hex_api::Bounds::default(),
                    class,
                    owner: None,
                    devices: Vec::new(),
                    allocation: None,
                    route: None,
                    band,
                },
                Phase::Ready,
                Vec::new(),
            );
            if ended {
                record.exit = Some(hex_api::Exit::Code(0));
            }
            Ok(record)
        };
    let oci = || {
        ClassSpec::Oci(Box::new(OciSpec {
            rootfs: hex_api::Rootfs::Dir(std::path::PathBuf::from("/images/agent")),
            init: hex_api::Program::of(&["/bin/true"]),
            mounts: Vec::new(),
            hostname: None,
            network: hex_api::Network::Private,
            hosts: Vec::new(),
        }))
    };
    let mut records: BTreeMap<BoxId, Result<BoxRecord, String>> = [
        ("oci-1", boxed("oci-1", oci(), 1, false)),
        ("oci-3", boxed("oci-3", oci(), 3, false)),
        ("oci-7", boxed("oci-7", oci(), 7, true)),
        ("proc-9", boxed("proc-9", ClassSpec::Process, 9, false)),
    ]
    .into_iter()
    .map(|(id, record)| (BoxId::new(id).unwrap(), record))
    .collect();
    assert_eq!(
        node_band(records.iter()),
        NodeBand {
            band: 3,
            unknown: None
        }
    );
    records.insert(
        BoxId::new("torn-1").unwrap(),
        Err("its record is torn".to_owned()),
    );
    let unknown = node_band(records.iter());
    assert_eq!(unknown.band, u8::MAX);
    let why = unknown.unknown.unwrap_or_default();
    assert!(
        why.contains("torn-1") && why.contains("its record is torn"),
        "{why:?}"
    );
    assert_eq!(
        node_band(BTreeMap::<BoxId, Result<BoxRecord, String>>::new().iter()).band,
        0
    );
}

/// Joint plan step 39, place 3 (AC-T7.4.1; ROOT, 2026-11-13: the node VM is capped through its run's rate, D247, told
/// Governor): a node whose guest is a node VM under shards-init learns that VM as it learns an image VM, over its run's
/// period, and holds it through its run's rate. Do open a node whose platform holds no window, keep its node VM's row
/// Ready, its receipt as large as the machine ([`machine_cost`]) booked at 4 CPUs and its run saying 70 ms, and a process
/// box capped at all but 6 CPUs of the machine; set its charge learner, with no thread of its own, run a window, and
/// answer the Shards class. Expect the node VM's host use read, its run held to a rate below its ceiling, the window its
/// run's 70 ms, and the answer naming its holding and its rate held by its governor, by its run.
#[test]
fn w39_a_node_vm_is_learned_over_its_runs_period_and_capped_through_its_runs_rate() {
    let dir = tempfile::tempdir().unwrap();
    let images = Images::default();
    images
        .0
        .node_periods
        .lock()
        .unwrap()
        .insert(node_run(7), Duration::from_millis(70));
    let node =
        std::sync::Arc::new(crate::recorded::Recorded::open(images.clone(), dir.path()).unwrap());
    let store = &node.books.store;
    let machine = node.machine().unwrap();
    let sized = machine_cost(machine);
    let booked = |phase| {
        let mut row = node_row(7, phase);
        row.allocation = sized
            .allocate(CapacityResources::default())
            .unwrap()
            .booked_at(res(4_000, 4 * GIB));
        row
    };
    store
        .put_node_vm(&booked(hex_api::NodeVmPhase::Booting))
        .unwrap();
    store
        .put_node_vm(&booked(hex_api::NodeVmPhase::Ready))
        .unwrap();
    let cap = u32::try_from(machine.cpu_milli.saturating_sub(6_000)).unwrap();
    let capped = BoxRecord::new(
        BoxSpec {
            id: BoxId::new("proc-1").unwrap(),
            bounds: hex_api::Bounds {
                cpu_max_milli: Some(cap),
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
    store.put(&capped).unwrap();
    node.set_charges().unwrap();
    assert_eq!(node.learn_window(), Some(Duration::from_millis(70)));
    assert!(
        images
            .0
            .node_reads
            .load(std::sync::atomic::Ordering::SeqCst)
            > 0,
        "its host use read"
    );
    let rates = images.0.node_rates.lock().unwrap().clone();
    assert!(
        matches!(rates.as_slice(), [(run, rate)] if *run == node_run(7) && *rate < sized.ceiling.cpu_milli),
        "{rates:?}"
    );
    let mut capacity = hex_api::Capacity {
        own: Default::default(),
        id: "shards".into(),
        total: machine,
        free: CapacityResources::default(),
        devices: Vec::new(),
        cost: Some(sized),
        routes: Vec::new(),
        device_routes: Vec::new(),
        backings: Vec::new(),
        unavailable: Vec::new(),
        learned: None,
        node_vms: Vec::new(),
        unlearned: None,
    };
    node.books
        .attach_learned(
            hex_api::Class::Shards,
            &mut capacity,
            &store.snapshot().unwrap(),
        )
        .unwrap();
    let learned = capacity.learned.unwrap();
    assert!(
        learned
            .holdings
            .iter()
            .any(|holding| holding.id == hex_api::LearnedHolder::Node(node_run(7))),
        "{:?}",
        learned.holdings
    );
    assert_eq!(
        learned.rates,
        [hex_api::LearnedRate {
            id: hex_api::LearnedHolder::Node(node_run(7)),
            held: hex_api::RateHeld::Governor,
        }]
    );
}

/// Joint plan step 39 (ROOT, 2026-11-13: a node VM's guest needs its room told like any guest's, D188): a node VM's guest is
/// told its room as an image VM's guest is, everything it holds coming out of it. Do run a window of a node VM of 16
/// vCPUs, booked at 4 CPUs and 4 GiB, in a 24-CPU fleet budget; then one in which its VM process ran 3.2 s of CPU in 300
/// ms, holding 3 GiB. Expect its guest told its room, its budget less what it holds, as it moves, from what it holds as
/// it is met, its baseline (ROOT, 2026-11-13, A-1: never its whole budget): 63 GiB, then 61 GiB.
#[test]
fn w39_a_node_vms_guest_is_told_its_room_as_an_image_vms_is() {
    let io = Record::default();
    let mut row = node_row(7, hex_api::NodeVmPhase::Ready);
    row.allocation = row.allocation.booked_at(res(4_000, 4 * GIB));
    row.started = Some(1);
    let held = |cpu_ns: u64, each_ns: u64, bytes: u64| HostUse {
        cpu_ns,
        footprint_bytes: bytes,
        resident_bytes: bytes,
        guest_ns: vec![each_ns; 16],
        window: Duration::ZERO,
        charged: hex_api::Charged::ToThread,
    };
    io.host
        .borrow_mut()
        .insert("node-7".to_owned(), held(0, 0, GIB));
    let node = [Holder::Node {
        row: &row,
        band: 0,
        unknown: None,
    }];
    let budget = res(24_000, 64 * GIB);
    let mut learner = Learner::new(true, res(32_000, 64 * GIB)).unwrap();
    let start = Instant::now();
    learner.window(&io, &node, budget, start).unwrap();
    io.host.borrow_mut().insert(
        "node-7".to_owned(),
        held(3_200_000_000, 150_000_000, 3 * GIB),
    );
    learner.window(&io, &node, budget, start + WINDOW).unwrap();
    assert_eq!(
        *io.rooms.borrow(),
        vec![
            ("node-7".to_owned(), i64::try_from(63 * GIB).unwrap()),
            ("node-7".to_owned(), i64::try_from(61 * GIB).unwrap()),
        ]
    );
}

/// Joint plan step 39 (ROOT, 2026-11-13; D188): a node whose guest is a node VM under shards-init tells that VM's guest its
/// room through its run, as it tells an image VM's. Do open a node whose platform holds no window, keep its node VM's row
/// Ready, its 16-CPU receipt booked at 4 CPUs and its run saying 70 ms, set its charge learner, with no thread of its
/// own, and run a window. Expect its guest told a room through its run, and only its.
#[test]
fn w39_a_nodes_node_vm_guest_is_told_its_room_through_its_run() {
    let dir = tempfile::tempdir().unwrap();
    let images = Images::default();
    images
        .0
        .node_periods
        .lock()
        .unwrap()
        .insert(node_run(7), Duration::from_millis(70));
    let node =
        std::sync::Arc::new(crate::recorded::Recorded::open(images.clone(), dir.path()).unwrap());
    let booked = |phase| {
        let mut row = node_row(7, phase);
        row.allocation = row.allocation.booked_at(res(4_000, 4 * GIB));
        row
    };
    let store = &node.books.store;
    store
        .put_node_vm(&booked(hex_api::NodeVmPhase::Booting))
        .unwrap();
    store
        .put_node_vm(&booked(hex_api::NodeVmPhase::Ready))
        .unwrap();
    node.set_charges().unwrap();
    node.learn_window();
    let told: Vec<hex_api::NodeRun> = images
        .0
        .node_rooms
        .lock()
        .unwrap()
        .iter()
        .map(|(run, _)| *run)
        .collect();
    assert_eq!(told, [node_run(7)]);
}

/// Joint plan step 39 (ROOT, 2026-11-13: a node VM's boot is charged): a node VM's receipt comes off the fleet's budget
/// from its row's first write, as the scheduler books it, so no room or rate hands out what its boot is using. Do keep,
/// on a node whose platform holds no window, a process box capped at all but 8 CPUs of the machine, a Shards image VM
/// of 16 vCPUs booked at 2 CPUs whose run says 70 ms, and a node VM's row Booting, its 16-CPU receipt booked at 4 CPUs;
/// then run a window. Expect the image VM, the one VM learned, capped at the 4 CPUs the booting node VM's booking leaves
/// of the 8.
#[test]
fn w39_a_booting_node_vms_booking_comes_off_the_fleets_budget() {
    let dir = tempfile::tempdir().unwrap();
    let images = Images::default();
    images
        .0
        .periods
        .lock()
        .unwrap()
        .insert("img-1".to_owned(), Duration::from_millis(70));
    let node =
        std::sync::Arc::new(crate::recorded::Recorded::open(images.clone(), dir.path()).unwrap());
    let store = &node.books.store;
    let machine = node.machine().unwrap();
    let left = 8_000;
    let capped = BoxRecord::new(
        BoxSpec {
            id: BoxId::new("proc-1").unwrap(),
            bounds: hex_api::Bounds {
                cpu_max_milli: Some(u32::try_from(machine.cpu_milli.saturating_sub(left)).unwrap()),
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
    store.put(&capped).unwrap();
    let mut image = image_vm("img-1", 1);
    image.spec.allocation = image
        .spec
        .allocation
        .map(|allocation| allocation.booked_at(res(2_000, 2 * GIB)));
    store.put(&image).unwrap();
    let booking = res(4_000, 4 * GIB);
    let mut booting = node_row(8, hex_api::NodeVmPhase::Booting);
    booting.allocation = booting.allocation.booked_at(booking);
    store.put_node_vm(&booting).unwrap();
    node.set_charges().unwrap();
    node.learn_window();
    assert_eq!(
        *images.0.box_rates.lock().unwrap(),
        [("img-1".to_owned(), left - booking.cpu_milli)]
    );
}

/// The review's dropped records (ROOT, 2026-11-14; AC-T7.4.1): a record its node can't read names a box it may still
/// run, whose class, caps and receipt only that record says, so nothing bounds the box below its machine. Its window
/// counts its CPU there, leaving the fleet only the CPU bookings it reads (what it takes of memory and disk is missing
/// from what the machine reads free already, A-10), and every VM class's answer names it and why, never dropping it. Do
/// keep, on a node whose platform holds no window, a process box capped at all but 8 CPUs of the machine and a Shards
/// image VM as large as the machine ([`machine_cost`]) booked at 2 CPUs whose run says 70 ms, and run a window; then end
/// that VM, make the process box's record unreadable, keep a second such image VM, run another window and answer the
/// Shards class. Expect the first VM capped at the 8 CPUs the process box leaves; then the second held at its 2-CPU
/// booking though it asks its ceiling, as the box whose record can't be read may hold all the rest of the CPU; and the
/// answer naming that box among the unrecorded, with its record's words and its count.
#[test]
fn w14_dropped_a_box_whose_record_cant_be_read_is_counted_at_its_machine_and_told() {
    let dir = tempfile::tempdir().unwrap();
    let images = Images::default();
    for name in ["img-1", "img-2"] {
        images
            .0
            .periods
            .lock()
            .unwrap()
            .insert(name.to_owned(), Duration::from_millis(70));
    }
    let node =
        std::sync::Arc::new(crate::recorded::Recorded::open(images.clone(), dir.path()).unwrap());
    let store = &node.books.store;
    let machine = node.machine().unwrap();
    let sized = machine_cost(machine);
    let left = 8_000;
    let capped = BoxRecord::new(
        BoxSpec {
            id: BoxId::new("proc-1").unwrap(),
            bounds: hex_api::Bounds {
                cpu_max_milli: Some(u32::try_from(machine.cpu_milli.saturating_sub(left)).unwrap()),
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
    store.put(&capped).unwrap();
    let booking = res(2_000, 2 * GIB);
    let booked = |name: &str, started: u64| {
        let mut image = image_vm(name, started);
        image.spec.allocation = Some(
            sized
                .allocate(CapacityResources::default())
                .unwrap()
                .booked_at(booking),
        );
        image
    };
    store.put(&booked("img-1", 1)).unwrap();
    node.set_charges().unwrap();
    node.learn_window();
    assert_eq!(
        *images.0.box_rates.lock().unwrap(),
        [("img-1".to_owned(), left)]
    );
    images.0.box_rates.lock().unwrap().clear();
    store.remove(&BoxId::new("img-1").unwrap()).unwrap();
    // A record damaged behind its store's back is out of the store's contract: the next node's open reads it (ROOT,
    // 2026-11-14), so the node restarts over it.
    let place = store.runtime_place(&capped.spec.id);
    drop(node);
    std::fs::write(place, b"{\"version\":").unwrap();
    let node =
        std::sync::Arc::new(crate::recorded::Recorded::open(images.clone(), dir.path()).unwrap());
    let store = &node.books.store;
    let Some(Err(why)) = store.get(&capped.spec.id).unwrap() else {
        panic!("the process box's record still reads");
    };
    store.put(&booked("img-2", 2)).unwrap();
    node.set_charges().unwrap();
    node.learn_window();
    let mut capacity = hex_api::Capacity {
        own: Default::default(),
        id: "shards".into(),
        total: machine,
        free: CapacityResources::default(),
        devices: Vec::new(),
        cost: Some(sized),
        routes: Vec::new(),
        device_routes: Vec::new(),
        backings: Vec::new(),
        unavailable: Vec::new(),
        learned: None,
        node_vms: Vec::new(),
        unlearned: None,
    };
    node.books
        .attach_learned(
            hex_api::Class::Shards,
            &mut capacity,
            &store.snapshot().unwrap(),
        )
        .unwrap();
    let told: Vec<(String, String)> = capacity
        .learned
        .iter()
        .flat_map(|learned| &learned.unrecorded)
        .map(|told| (holder_name(&told.id), told.why.clone()))
        .collect();
    assert_eq!(
        (images.0.box_rates.lock().unwrap().clone(), told),
        (
            vec![("img-2".to_owned(), booking.cpu_milli)],
            vec![(
                "proc-1".to_owned(),
                format!("its record can't be read: {why}; counted at its machine")
            )]
        )
    );
}

/// A node whose platform holds no window (A-10), `served` where a capacity view serves its machine, beside a process box
/// capped at all but 8 CPUs of the machine, keeping a Shards image VM as large as the machine booked at 2 CPUs whose run
/// says 70 ms; its charge learner set with no thread of its own. Its stand-in keeps each rate it holds.
fn reserving(
    dir: &std::path::Path,
    served: bool,
) -> (std::sync::Arc<crate::recorded::Recorded<Images>>, Images) {
    let images = Images::default();
    images
        .0
        .periods
        .lock()
        .unwrap()
        .insert("img-1".to_owned(), Duration::from_millis(70));
    let node = std::sync::Arc::new(crate::recorded::Recorded::open(images.clone(), dir).unwrap());
    node.set_capacity_view(served);
    let store = &node.books.store;
    let machine = node.machine().unwrap();
    let cap = u32::try_from(machine.cpu_milli.saturating_sub(8_000)).unwrap();
    store.put(&process_box("proc-1", cap, GIB)).unwrap();
    let mut image = image_vm("img-1", 1);
    image.spec.allocation = Some(
        machine_cost(machine)
            .allocate(CapacityResources::default())
            .unwrap()
            .booked_at(res(2_000, 2 * GIB)),
    );
    store.put(&image).unwrap();
    node.set_charges().unwrap();
    (node, images)
}

/// What `node`'s Shards class's capacity answer tells its learner takes off its fleet's budget for the capacity view's
/// holders (A-10).
fn told_reservations(
    node: &crate::recorded::Recorded<Images>,
) -> Option<hex_api::LearnedReservations> {
    let machine = node.machine().unwrap();
    let mut capacity = hex_api::Capacity {
        own: Default::default(),
        id: "shards".into(),
        total: machine,
        free: CapacityResources::default(),
        devices: Vec::new(),
        cost: Some(machine_cost(machine)),
        routes: Vec::new(),
        device_routes: Vec::new(),
        backings: Vec::new(),
        unavailable: Vec::new(),
        learned: None,
        node_vms: Vec::new(),
        unlearned: None,
    };
    node.books
        .attach_learned(
            hex_api::Class::Shards,
            &mut capacity,
            &node.books.store.snapshot().unwrap(),
        )
        .unwrap();
    capacity.learned.and_then(|learned| learned.reservations)
}

/// The keeper of epoch `keeper`'s word: `cpu_milli` of CPU reserved.
fn word(keeper: u64, cpu_milli: u64) -> hex_api::Reserved {
    hex_api::Reserved {
        keeper,
        reserved: res(cpu_milli, 0),
    }
}

/// The review's A-10 (AC-T7.4.1; ROOT, 2026-11-14): what the capacity view's holders reserve comes off the fleet's budget
/// once its keeper says it. Do tell a node a capacity view serves, beside a process box capped at all but 8 CPUs of the
/// machine, keeping an image VM booked at 2 CPUs that asks its ceiling, its keeper's word of 3 CPUs reserved, and run a
/// window. Expect the word taken, the VM capped at the 5 CPUs left, and the answer telling the word heard.
#[test]
fn a10_a_keepers_word_comes_off_the_fleets_budget() {
    let dir = tempfile::tempdir().unwrap();
    let (node, images) = reserving(dir.path(), true);
    hex_api::Runtime::reserved(&*node, word(1, 3_000)).unwrap();
    node.learn_window();
    assert_eq!(
        *images.0.box_rates.lock().unwrap(),
        [("img-1".to_owned(), 5_000)]
    );
    assert_eq!(
        told_reservations(&node),
        Some(hex_api::LearnedReservations::Heard {
            keeper: 1,
            reserved: res(3_000, 0),
            unconfigured: false,
        })
    );
}

/// The review's A-10 (ROOT, 2026-11-14: the cold case told, never guessed): a node a capacity view serves whose keeper
/// hasn't spoken since it started learns nothing, its VMs booked whole at their receipts; its keeper's word then has it
/// learn. Do run a window of such a node; then tell it its keeper's word, 3 CPUs reserved, and run another. Expect no rate
/// held and the answer telling kiln's reservations unheard; then the VM capped at the 5 CPUs left, the word told heard.
#[test]
fn a10_a_node_whose_keeper_hasnt_spoken_books_its_vms_whole_and_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let (node, images) = reserving(dir.path(), true);
    assert_eq!(node.learn_window(), None, "no window while it learns none");
    assert_eq!(
        *images.0.box_rates.lock().unwrap(),
        Vec::<(String, u64)>::new()
    );
    assert_eq!(
        told_reservations(&node),
        Some(hex_api::LearnedReservations::Unheard)
    );
    hex_api::Runtime::reserved(&*node, word(1, 3_000)).unwrap();
    node.learn_window();
    assert_eq!(
        *images.0.box_rates.lock().unwrap(),
        [("img-1".to_owned(), 5_000)]
    );
    assert!(matches!(
        told_reservations(&node),
        Some(hex_api::LearnedReservations::Heard { keeper: 1, .. })
    ));
}

/// The review's A-10 (ROOT, 2026-11-14): where no capacity view serves a node's machine, no holder reserves room there,
/// so none is counted, and its answers say so plainly. Do run a window of a node no view serves. Expect the VM capped at
/// the 8 CPUs the process box leaves, and the answer telling no view serves the machine.
#[test]
fn a10_a_node_no_capacity_view_serves_counts_no_reservations_and_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let (node, images) = reserving(dir.path(), false);
    node.learn_window();
    assert_eq!(
        *images.0.box_rates.lock().unwrap(),
        [("img-1".to_owned(), 8_000)]
    );
    assert_eq!(
        told_reservations(&node),
        Some(hex_api::LearnedReservations::Unserved)
    );
}

/// The review's A-10 (ROOT, 2026-11-14: a word that comes without the flag is still taken, told): a keeper's word to a
/// node whose configuration names no capacity view is counted, and its answers say the configuration names none. Do tell
/// a node no view serves a keeper's word of 3 CPUs reserved, and run a window. Expect the VM capped at the 5 CPUs left
/// and the answer telling the word heard, its configuration naming no view.
#[test]
fn a10_a_keepers_word_to_a_node_configured_with_none_is_taken_and_told() {
    let dir = tempfile::tempdir().unwrap();
    let (node, images) = reserving(dir.path(), false);
    hex_api::Runtime::reserved(&*node, word(1, 3_000)).unwrap();
    node.learn_window();
    assert_eq!(
        *images.0.box_rates.lock().unwrap(),
        [("img-1".to_owned(), 5_000)]
    );
    assert_eq!(
        told_reservations(&node),
        Some(hex_api::LearnedReservations::Heard {
            keeper: 1,
            reserved: res(3_000, 0),
            unconfigured: true,
        })
    );
}

/// The review's A-10 (CV2: a keeper's epoch grows at each of its boots): a node keeps its latest keeper's word, and an
/// older keeper's changes nothing, refused. Do tell a node keeper 2's word of 3 CPUs, then keeper 1's of 1 CPU, then
/// keeper 2's of 4 CPUs, and run a window. Expect keeper 1's refused naming both keepers, and the VM capped at the 4 CPUs
/// the latest word leaves.
#[test]
fn a10_an_older_keepers_word_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (node, images) = reserving(dir.path(), true);
    hex_api::Runtime::reserved(&*node, word(2, 3_000)).unwrap();
    let why = hex_api::Runtime::reserved(&*node, word(1, 1_000)).unwrap_err();
    assert!(
        why.to_string().contains("keeper 1") && why.to_string().contains("keeper 2"),
        "{why}"
    );
    hex_api::Runtime::reserved(&*node, word(2, 4_000)).unwrap();
    node.learn_window();
    assert_eq!(
        *images.0.box_rates.lock().unwrap(),
        [("img-1".to_owned(), 4_000)]
    );
}

/// A-5 (ROOT's audit, 2026-11-13: the oci boxes a node VM hosts were counted twice, at their caps as native boxes and
/// again in the node VM's footprint, so the VMs learned beside them got less room and the last resort fired early): an
/// oci box's memory is held by the guest that runs it, so its caps are native only where no node VM on record runs the
/// node's oci class. Do sort a node's records, an oci box capped at 2 CPUs and 2 GiB and a process box at 1 CPU and
/// 1 GiB, with no node VM on record; then with node VM 7 on record Ready, then Booting, then Ended. Expect native both
/// boxes' caps with none, the oci box's guest no holder its learner reads; and the process box's alone with each, the
/// oci box its node VM's: in its footprint while it runs, in its booking while it boots, and nothing once it has ended.
#[test]
fn w39_a5_the_oci_boxes_a_node_vm_hosts_are_its_never_native_too() {
    let capped = |name: &str, class: ClassSpec, cpu_milli: u32, mem_bytes: u64| {
        BoxRecord::new(
            BoxSpec {
                id: BoxId::new(name).unwrap(),
                bounds: hex_api::Bounds {
                    cpu_max_milli: Some(cpu_milli),
                    memory_max: Some(mem_bytes),
                    ..hex_api::Bounds::default()
                },
                class,
                owner: None,
                devices: Vec::new(),
                allocation: None,
                route: None,
                band: 0,
            },
            Phase::Ready,
            Vec::new(),
        )
    };
    let oci = ClassSpec::Oci(Box::new(OciSpec {
        rootfs: hex_api::Rootfs::Dir(std::path::PathBuf::from("/images/agent")),
        init: hex_api::Program::of(&["/bin/true"]),
        mounts: Vec::new(),
        hostname: None,
        network: hex_api::Network::Private,
        hosts: Vec::new(),
    }));
    let records = [
        capped("oci-1", oci, 2_000, 2 * GIB),
        capped("proc-1", ClassSpec::Process, 1_000, GIB),
    ];
    let native = |rows: &[hex_core::record::NodeVmRecord]| {
        booked_sums(&sort(&records, rows, |_| true).unwrap()).1
    };
    assert_eq!(
        native(&[]),
        res(3_000, 3 * GIB),
        "no node VM: the oci box's caps native"
    );
    for phase in [
        hex_api::NodeVmPhase::Ready,
        hex_api::NodeVmPhase::Booting,
        hex_api::NodeVmPhase::Ended,
    ] {
        assert_eq!(
            native(&[node_row(7, phase)]),
            res(1_000, GIB),
            "node VM {phase:?}"
        );
    }
}

/// Reviewer A's suspicion, verified (ROOT, 2026-11-13): a node VM learns under a key of its own, never its profile's
/// box VMs': it runs every oci box of its node, so its windows are a container host's use, no box VM's, and they would
/// raise its profile's priors and ceilings. Do run two windows of node VM 7 alone, of the Shards profile and envelope,
/// booked at 4 CPUs and 4 GiB, its VM process holding 9 GiB, 1 GiB past its 8 GiB envelope; then answer the Shards
/// class for that profile. Expect the profile's ceiling cold in every dimension: no box VM of it has shown its VMM's
/// own, and the node VM's windows are none of its evidence.
#[test]
fn w39_a_node_vm_learns_under_a_key_of_its_own() {
    let io = Record::default();
    let mut row = node_row(7, hex_api::NodeVmPhase::Ready);
    row.allocation = row.allocation.booked_at(res(4_000, 4 * GIB));
    row.started = Some(1);
    unused(&io, "node-7", 9 * GIB);
    let node = [Holder::Node {
        row: &row,
        band: 0,
        unknown: None,
    }];
    let budget = res(24_000, 64 * GIB);
    let mut learner = Learner::new(true, res(32_000, 64 * GIB)).unwrap();
    let start = Instant::now();
    learner.window(&io, &node, budget, start).unwrap();
    learner.window(&io, &node, budget, start + WINDOW).unwrap();
    let learned = learner
        .learned(Class::Shards, &[cost()], &node)
        .unwrap()
        .unwrap();
    assert_eq!(
        learned
            .ceilings
            .iter()
            .map(|ceiling| ceiling.overhead)
            .collect::<Vec<_>>(),
        [hex_api::LearnedOverhead {
            cpu_milli: None,
            mem_bytes: None,
            disk_bytes: None,
        }]
    );
}

/// Reviewer A's suspicion, verified (ROOT, 2026-11-13): an intent the last resort couldn't write into its VM's record
/// is told, typed, never dropped. Do run a window of three Shards VMs booked at 4 GiB in a 16 GiB fleet, then one where
/// they hold 6, 7 and 2 GiB, every intent's write refused. Expect vm-2 still stopped, the fleet held before its record;
/// the window refused with the write's own refusal; and why it ended vm-2 kept for its record after the window.
#[test]
fn w39_an_intent_the_last_resort_couldnt_write_is_told_typed() {
    let io = Record::default();
    let vms = fleet_of_three();
    for record in &vms {
        io.set(record.spec.id.as_str(), 0, 0);
    }
    io.refuse_intents.set(true);
    let budget = res(24_000, 16 * GIB);
    let mut learner = Learner::new(true, res(32_000, 64 * GIB)).unwrap();
    let start = Instant::now();
    learner.window(&io, &holders(&vms), budget, start).unwrap();
    for (name, held) in [("vm-1", 6 * GIB), ("vm-2", 7 * GIB), ("vm-3", 2 * GIB)] {
        io.set(name, 0, held);
    }
    let told = learner.window(&io, &holders(&vms), budget, start + WINDOW);
    assert_eq!(*io.stops.borrow(), ["vm-2"]);
    assert_eq!(
        told,
        Err(Refusal::Broken("the record's write failed".into()))
    );
    let kept: Vec<_> = learner
        .unrecorded()
        .into_iter()
        .map(|(id, _, end)| (box_of(&id).to_owned(), end.map(|end| end.held)))
        .collect();
    assert_eq!(kept, [("vm-2".to_owned(), Some(7 * GIB))]);
}

/// Place 4 (ROOT, 2026-11-13: what Hex decides on its own says why): a VM a learner's window couldn't read is counted
/// whole, and its class's answer names it, a box and a node VM alike, with its refusal's words; and a guest whose room
/// can't be told is named with what that awaits, as its runtime answers it. Do run a window of a managed Shards VM it
/// reads, whose guest's room its runtime answers awaiting GuestRoom, an image VM whose run's period can't be read and a
/// node VM whose host use can't be, then answer the Shards class. Expect the image VM and the node VM named unread, by
/// their holders, each with its refusal's words, the managed VM not; and the managed VM named awaiting GuestRoom.
#[test]
fn w39_an_answer_names_each_vm_its_window_couldnt_read_and_why() {
    let io = Record::default();
    io.set("vm-1", 0, GIB);
    let image = image_vm("img-1", 2);
    unused(&io, "img-1", GIB);
    io.unread_periods.borrow_mut().push("img-1".to_owned());
    let mut row = node_row(7, hex_api::NodeVmPhase::Ready);
    row.allocation = row.allocation.booked_at(res(4_000, 4 * GIB));
    row.started = Some(3);
    let vms = [vm("vm-1", 1), image];
    let mut running = holders(&vms);
    running.push(Holder::Node {
        row: &row,
        band: 0,
        unknown: None,
    });
    let mut learner = Learner::new(true, res(32_000, 64 * GIB)).unwrap();
    learner
        .window(&io, &running, res(24_000, 64 * GIB), Instant::now())
        .unwrap();
    let learned = learner
        .learned(Class::Shards, &[cost()], &running)
        .unwrap()
        .unwrap();
    let told: Vec<(String, String)> = learned
        .unread
        .iter()
        .map(|unread| (holder_name(&unread.id), unread.why.clone()))
        .collect();
    assert_eq!(
        told,
        [
            (
                "img-1".to_owned(),
                Refusal::Broken("its run's period unread".into()).to_string()
            ),
            (
                "node-7".to_owned(),
                Refusal::Broken("no such VM".into()).to_string()
            ),
        ]
    );
    let awaited: Vec<(String, Upstream)> = learned
        .awaits
        .iter()
        .map(|awaited| (holder_name(&awaited.id), awaited.needs))
        .collect();
    assert_eq!(awaited, [("vm-1".to_owned(), Upstream::GuestRoom)]);
}

/// Place 4 with place 3's band (ROOT, 2026-11-13: "an unreadable record ranking the node VM above every box is safe,
/// but untold"): do run a window of a node VM ranked above every box because a record of its node's can't be read, then
/// answer the Shards class. Expect the node VM named ranked, by its run, with that reason.
#[test]
fn w39_an_answer_names_a_node_vm_ranked_above_every_box_and_why() {
    let io = Record::default();
    let mut row = node_row(7, hex_api::NodeVmPhase::Ready);
    row.allocation = row.allocation.booked_at(res(4_000, 4 * GIB));
    row.started = Some(1);
    unused(&io, "node-7", GIB);
    let why = "box torn-1's record can't be read: its record is torn";
    let running = [Holder::Node {
        row: &row,
        band: u8::MAX,
        unknown: Some(why),
    }];
    let mut learner = Learner::new(true, res(32_000, 64 * GIB)).unwrap();
    learner
        .window(&io, &running, res(24_000, 64 * GIB), Instant::now())
        .unwrap();
    let learned = learner
        .learned(Class::Shards, &[cost()], &running)
        .unwrap()
        .unwrap();
    assert_eq!(
        learned.ranked,
        [hex_api::LearnedRank {
            run: node_run(7),
            why: why.to_owned(),
        }]
    );
}

/// A-3 (ROOT's audit, 2026-11-13: a failed window was told nowhere, its answers carrying nothing learned and no why):
/// a capping learner whose window failed says why on its class's capacity answer. Do fail a learner's window, typed, as
/// its node does where it can't read its records; then answer the Shards class. Expect nothing learned, and the answer
/// naming the window's refusal in its words.
#[test]
fn w14_a3_a_failed_window_says_why_on_its_class_answer() {
    let mut learner = Learner::new(true, res(32_000, 64 * GIB)).unwrap();
    let why = Refusal::Broken("the node's records can't be read".into());
    learner.failed(why.clone());
    let mut capacity = hex_api::Capacity {
        own: Default::default(),
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
        node_vms: Vec::new(),
        unlearned: None,
    };
    learner
        .answer(Class::Shards, &mut capacity, std::iter::empty())
        .unwrap();
    assert!(capacity.learned.is_none(), "{:?}", capacity.learned);
    assert_eq!(capacity.unlearned, Some(why.to_string()));
}

/// A Ready oci box's record, `name`, its run ended where `ended`.
fn oci_box(name: &str, ended: bool) -> BoxRecord {
    let mut record = BoxRecord::new(
        BoxSpec {
            id: BoxId::new(name).unwrap(),
            bounds: hex_api::Bounds::default(),
            class: ClassSpec::Oci(Box::new(OciSpec {
                rootfs: hex_api::Rootfs::Dir(std::path::PathBuf::from("/images/agent")),
                init: hex_api::Program::of(&["/bin/true"]),
                mounts: Vec::new(),
                hostname: None,
                network: hex_api::Network::Private,
                hosts: Vec::new(),
            })),
            owner: None,
            devices: Vec::new(),
            allocation: None,
            route: None,
            band: 0,
        },
        Phase::Ready,
        Vec::new(),
    );
    if ended {
        record.exit = Some(hex_api::Exit::Code(0));
    }
    record
}

/// The record `store` keeps of box `name`, which it must.
fn kept(store: &crate::store::Store, name: &str) -> BoxRecord {
    store
        .get(&BoxId::new(name).unwrap())
        .unwrap()
        .unwrap()
        .unwrap()
}

/// A-6 (ROOT's audit, 2026-11-13: ending a node VM ends every oci box it runs, and none of them said why): the last
/// resort's intent for a node VM goes into the record of each oci box it runs too, the end that will end it, and comes
/// out of each when it is withdrawn. Do keep node VM 7's row Ready beside two running oci boxes, an ended oci box and a
/// process box; write why the last resort is ending node VM 7 through the write the learner makes; then withdraw it.
/// Expect the end on each running oci box's record, by the node VM's run, and none on the ended oci box's or the
/// process box's; then none on any.
#[test]
fn w39_a6_the_last_resorts_intent_for_a_node_vm_goes_into_each_oci_box_it_runs() {
    let dir = tempfile::tempdir().unwrap();
    let store = crate::store::Store::open(dir.path(), &dir.path().join("@records")).unwrap();
    store
        .put_node_vm(&node_row(7, hex_api::NodeVmPhase::Booting))
        .unwrap();
    store
        .put_node_vm(&node_row(7, hex_api::NodeVmPhase::Ready))
        .unwrap();
    let mut process = oci_box("proc-1", false);
    process.spec.class = ClassSpec::Process;
    for record in [
        oci_box("oci-1", false),
        oci_box("oci-2", false),
        oci_box("oci-3", true),
        process,
    ] {
        store.put(&record).unwrap();
    }
    let holder = hex_api::LearnedHolder::Node(node_run(7));
    let scope = Scope::Run(node_run(7));
    let said = |store: &crate::store::Store| -> Vec<Option<LearnedEnd>> {
        ["oci-1", "oci-2", "oci-3", "proc-1"]
            .into_iter()
            .map(|name| kept(store, name).last_resort)
            .collect()
    };
    assert!(record_last_resort(&store, &holder, scope, Some(&node_end(7))).unwrap());
    assert_eq!(
        said(&store),
        [Some(node_end(7)), Some(node_end(7)), None, None]
    );
    assert!(record_last_resort(&store, &holder, scope, None).unwrap());
    assert_eq!(said(&store), [None, None, None, None]);
}

/// A-6: an oci box whose record names its node VM's last-resort end is retired with it once that node VM's run has
/// ended, kept with why until its owner's delete; one naming a node VM that still runs, and one naming no end, are left
/// as they were. Do keep node VM 7's row Ended and node VM 8's Ready, two oci boxes naming 7's end, one naming 8's and
/// one naming none; then retire what ended with its node VM, as each window and each start does, and count a retired
/// box's inspect as the node does. Expect the two naming 7's end retired for NodeVmEnded, Ready, their ends unknown,
/// why still on their records and said by their inspects; the others not retired.
#[test]
fn w39_a6_the_oci_boxes_a_node_vms_last_resort_ended_are_retired_with_why() {
    let dir = tempfile::tempdir().unwrap();
    let books = books(dir.path());
    let store = &books.store;
    for phase in [
        hex_api::NodeVmPhase::Booting,
        hex_api::NodeVmPhase::Ready,
        hex_api::NodeVmPhase::Ended,
    ] {
        store.put_node_vm(&node_row(7, phase)).unwrap();
    }
    for phase in [hex_api::NodeVmPhase::Booting, hex_api::NodeVmPhase::Ready] {
        store.put_node_vm(&node_row(8, phase)).unwrap();
    }
    for (name, end) in [
        ("oci-1", Some(node_end(7))),
        ("oci-2", Some(node_end(7))),
        ("oci-3", Some(node_end(8))),
        ("oci-4", None),
    ] {
        let mut record = oci_box(name, false);
        record.last_resort = end;
        store.put(&record).unwrap();
    }
    let retired =
        retire_with_node_vms(store, &store.all().unwrap(), &store.all_node_vms().unwrap()).unwrap();
    assert_eq!(
        retired,
        [BoxId::new("oci-1").unwrap(), BoxId::new("oci-2").unwrap()]
    );
    for name in ["oci-1", "oci-2"] {
        let record = kept(store, name);
        assert!(record.retired && record.phase == Phase::Ready, "{record:?}");
        assert_eq!(
            record.retired_for,
            Some(hex_api::LifecycleCause::NodeVmEnded)
        );
        assert_eq!(record.exit, Some(hex_api::Exit::Unknown));
        assert_eq!(record.last_resort, Some(node_end(7)));
        let info = books.counted(record.retired_info().unwrap()).unwrap();
        assert_eq!(info.last_resort, Some(node_end(7)));
    }
    for name in ["oci-3", "oci-4"] {
        assert!(!kept(store, name).retired, "{name}");
    }
}

/// A-6, across a restart: an oci box its node VM's last-resort end ended survives its node's restart, retired with why,
/// never removed as a box nothing runs. Do leave node VM 4's row Ended with why the last resort ended it, as a node
/// leaves it once that VM's run is stopped, and an oci box's record naming that end, not yet retired (the node stopped
/// between the two); then open a node over them and recover it. Expect the box kept, retired for NodeVmEnded, and its
/// inspect saying why.
#[test]
fn w39_a6_an_oci_box_its_node_vm_ended_is_kept_with_why_across_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    {
        let earlier = crate::recorded::Recorded::open(Images::default(), dir.path()).unwrap();
        let mut record = oci_box("oci-1", false);
        record.last_resort = Some(node_end(4));
        earlier.books.store.put(&record).unwrap();
    }
    let mut ended = node_row(4, hex_api::NodeVmPhase::Ended);
    ended.last_resort = Some(node_end(4));
    leave_node_row(dir.path(), &ended);
    let node = std::sync::Arc::new(
        crate::recorded::Recorded::open(Images::default(), dir.path()).unwrap(),
    );
    node.recover().unwrap();
    let record = kept(&node.books.store, "oci-1");
    assert_eq!(
        (record.retired, record.retired_for),
        (true, Some(hex_api::LifecycleCause::NodeVmEnded))
    );
    let info = node.books.counted(record.retired_info().unwrap()).unwrap();
    assert_eq!(info.last_resort, Some(node_end(4)));
}

/// A-2 (the review's, 2026-11-14: a window held its learner's lock across every ask of its VMs, so one wedged VM process
/// stalled every capacity answer; AC-T7.4.1): a window asks its VMs with the lock its capacity answers take let go. Do
/// set the charge learner of a node whose platform holds no window, with no thread of its own, so no window runs beside
/// this one; then keep a Shards image VM and a node VM's row Ready, each run saying 70 ms, and run a window, noting at
/// each ask of either whether that lock is held. Expect each VM asked its period, its use, its rate and its room, and
/// none of those asked while that lock is held.
#[test]
fn a2_a_window_asks_its_vms_with_the_capacity_answers_lock_let_go() {
    let dir = tempfile::tempdir().unwrap();
    let images = Images::default();
    let node =
        std::sync::Arc::new(crate::recorded::Recorded::open(images.clone(), dir.path()).unwrap());
    let weak = std::sync::Arc::downgrade(&node);
    let watching = images.0.watch.set(Box::new(move || {
        weak.upgrade().is_some_and(|node| {
            node.books
                .charges
                .get()
                .is_some_and(|(learner, _)| learner.try_lock().is_err())
        })
    }));
    assert!(watching.is_ok(), "the lock watched once");
    node.set_charges().unwrap();
    images
        .0
        .periods
        .lock()
        .unwrap()
        .insert("img-1".to_owned(), Duration::from_millis(70));
    images
        .0
        .node_periods
        .lock()
        .unwrap()
        .insert(node_run(7), Duration::from_millis(70));
    let store = &node.books.store;
    store.put(&image_vm("img-1", 1)).unwrap();
    store
        .put_node_vm(&node_row(7, hex_api::NodeVmPhase::Booting))
        .unwrap();
    store
        .put_node_vm(&node_row(7, hex_api::NodeVmPhase::Ready))
        .unwrap();
    assert_eq!(node.learn_window(), Some(Duration::from_millis(70)));
    let asked = images.0.asked.lock().unwrap().clone();
    for what in [
        "image_rate_period",
        "host_use",
        "rate",
        "room",
        "node_vm_rate_period",
        "node_vm_host_use",
        "node_vm_rate",
        "node_vm_room",
    ] {
        assert!(asked.contains(&what), "{what} asked: {asked:?}");
    }
    assert_eq!(
        *images.0.held.lock().unwrap(),
        Vec::<&str>::new(),
        "asked while the lock its capacity answers take was held"
    );
}

/// Batch 1114c's per-window reads, measured before any change (AC-T7.4.1): what one charge window's `Store::all` and
/// `Store::all_node_vms` cost a node holding `n` image VMs' records and `n` Ended node VM rows, the reads every window
/// makes. A measurement, not a verdict: run alone (`--ignored --exact --nocapture --test-threads=1`) with
/// `HEX_WINDOW_READS_N`, the counts to measure at (comma-separated), and `HEX_WINDOW_READS_ROUNDS`, the reads of each;
/// unset, it measures nothing. Prints one line a count: each read's n, p50, p90, p99 and max in nanoseconds (nearest
/// rank), the files read from the page cache as a running node's are.
#[test]
#[ignore = "a measurement, run alone (its docs)"]
fn measure_a_windows_record_reads() {
    let (Ok(counts), Some(rounds)) = (
        std::env::var("HEX_WINDOW_READS_N"),
        std::env::var("HEX_WINDOW_READS_ROUNDS")
            .ok()
            .and_then(|r| r.parse::<usize>().ok()),
    ) else {
        return;
    };
    let rank = |sorted: &[u128], p: usize| sorted[(sorted.len() * p).div_ceil(100).max(1) - 1];
    for n in counts
        .split(',')
        .map(|n| n.trim().parse::<usize>().unwrap())
    {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("state"), &dir.path().join("run")).unwrap();
        for i in 0..n {
            store
                .put(&image_vm(&format!("img-{i}"), u64::try_from(i).unwrap()))
                .unwrap();
            let mut run = [0u8; 16];
            run[..8].copy_from_slice(&u64::try_from(i).unwrap().to_le_bytes());
            let row = hex_core::record::NodeVmRecord {
                run: hex_api::NodeRun::from_bytes(run),
                ..node_row(0, hex_api::NodeVmPhase::Ended)
            };
            store.put_node_vm(&row).unwrap();
        }
        let time = |read: &dyn Fn() -> usize| {
            let mut took: Vec<u128> = (0..rounds)
                .map(|_| {
                    let began = Instant::now();
                    assert_eq!(read(), n);
                    began.elapsed().as_nanos()
                })
                .collect();
            took.sort_unstable();
            let max = took.last().copied().unwrap_or_default();
            format!(
                "n={} p50={} p90={} p99={} max={max}",
                took.len(),
                rank(&took, 50),
                rank(&took, 90),
                rank(&took, 99)
            )
        };
        let all = time(&|| store.all().unwrap().len());
        let rows = time(&|| store.all_node_vms().unwrap().len());
        eprintln!("WINDOW-READS records={n} all: {all}; all_node_vms: {rows}");
    }
}

/// The review's per-window reads, step 2 (batch 1114d's reads2; AC-T7.4.1): a charge window's reads of its node's records
/// read no file, its node's store being their only writer. Do, at 10, 100 and 1000 of each: leave image VMs' records and
/// Ended node VM rows as a node leaves them; open the store as the next node does; then read as a window does (every
/// record and every row), as a VM's guest time does (each record by its id) and as a capacity answer's receipts do (each
/// row by its run). Expect no record or row file read once the store is open.
#[test]
fn reads2_a_windows_reads_read_no_record_file_at_10_100_and_1000() {
    for n in [10_usize, 100, 1000] {
        let dir = tempfile::tempdir().unwrap();
        let (state, run) = (dir.path().join("state"), dir.path().join("run"));
        let ids: Vec<BoxId> = (0..n)
            .map(|i| BoxId::new(&format!("img-{i}")).unwrap())
            .collect();
        {
            let store = Store::open(&state, &run).unwrap();
            for (i, id) in ids.iter().enumerate() {
                store
                    .put(&image_vm(id.as_str(), u64::try_from(i).unwrap()))
                    .unwrap();
            }
        }
        let rows = state.join(".node-vms");
        std::fs::create_dir_all(&rows).unwrap();
        let runs: Vec<hex_api::NodeRun> = (0..n)
            .map(|i| {
                let mut run = [0u8; 16];
                run[..8].copy_from_slice(&u64::try_from(i).unwrap().to_le_bytes());
                let row = hex_core::record::NodeVmRecord {
                    run: hex_api::NodeRun::from_bytes(run),
                    ..node_row(0, hex_api::NodeVmPhase::Ended)
                };
                let name: String = run.iter().map(|byte| format!("{byte:02x}")).collect();
                std::fs::write(rows.join(format!("{name}.json")), row.encode().unwrap()).unwrap();
                row.run
            })
            .collect();
        let store = Store::open(&state, &run).unwrap();
        let opened = store.reads();
        assert_eq!(store.all().unwrap().len(), n);
        assert_eq!(store.all_node_vms().unwrap().len(), n);
        for id in &ids {
            assert!(matches!(store.get(id).unwrap(), Some(Ok(_))));
        }
        for run in &runs {
            assert!(store.node_vm(run).unwrap().is_some());
        }
        let read = store.reads() - opened;
        assert_eq!(
            read, 0,
            "at {n} records and rows, a window's reads read {read} files"
        );
    }
}

/// Shards' D265 (AC-T7.4.1): a VM whose hypervisor runs its vCPUs apart from its process's threads (WHP's classic and
/// core schedulers, every Windows Server's) counts its vCPUs' guest time beside its process's CPU, never in it. Do:
/// split one window's 900 ms of the VM process's CPU beside 600 ms of its vCPUs' guest time, as each charge says.
/// Expect: where its threads hold their guest time, its own the whole less theirs (300 ms) and its total the whole
/// (900 ms); where they ran apart, or its process hasn't told, its own the whole (900 ms) and its total the whole and
/// theirs (1500 ms): a VM is never charged less than it ran.
#[test]
fn d265_a_vm_whose_vcpus_run_apart_is_charged_its_whole_cpu_and_theirs_beside() {
    let ms = |n: u64| n * 1_000_000;
    let (now, before) = ([ms(400), ms(200)], [0u64, 0]);
    for (charged, own, total) in [
        (hex_api::Charged::ToThread, ms(300), ms(900)),
        (hex_api::Charged::Apart, ms(900), ms(1500)),
        (hex_api::Charged::Unknown, ms(900), ms(1500)),
    ] {
        let cpu = ran(ms(900), Some(&now), Some(&before), charged).unwrap();
        let Cpu::Split { guest, own: split } = &cpu else {
            panic!("{charged:?}: a whole reading {cpu:?}");
        };
        assert_eq!(split.as_nanos(), u128::from(own), "{charged:?}: its own");
        let sum = guest
            .iter()
            .fold(split.as_nanos(), |sum, vcpu| sum + vcpu.as_nanos());
        assert_eq!(sum, u128::from(total), "{charged:?}: its total");
    }
}
