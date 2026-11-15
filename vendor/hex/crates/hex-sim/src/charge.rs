//! Joint plan step 14 (AC-T7.4.1; `docs/design/learned-charge.md`): what a microVM is charged, learned from its
//! measured use with its allocation the ceiling (C1); what a new one is booked at, its class's learned prior
//! (C2); the bookings a node holds within its fleet's budget, following the charges (C3); CPU shared by booking
//! (C4); memory held to budgets by booking, a VM past its booking ended last (C5); and what a node admits and
//! publishes (C6, C8). Pure (CLAUDE.md §9): the node's shell reads every VM once a window and acts on what this
//! decides, and hex-sim drives it with simulated VMs.

use std::collections::BTreeMap;
use std::time::Duration;

use hex_api::CapacityResources as Amounts;
use hex_api::Upstream;

use crate::model::Class;

pub mod sim;
pub mod trace;
pub mod trial;

/// The fewest windows whose nearest-rank p99.9 is an observed sample rather than their maximum: decision 9's
/// census rule (`SEEDABLE` in `bench/competitive/probes/waits_census.py`, held to it by a doc-truth test). A
/// charge is the nearest-rank p99.9 of a VM's last this many windows, and a VM is cold until it has them.
pub const SEEDABLE: usize = 1_000;

/// `cpu.weight`'s range: "All weights are in the range [1, 10000]" (Linux
/// `Documentation/admin-guide/cgroup-v2.rst`, "Weights").
pub const WEIGHT_MIN: u16 = 1;
pub const WEIGHT_MAX: u16 = 10_000;

/// Why the core refused, typed (CLAUDE.md §3).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ChargeError {
    #[error("{what} isn't valid: {why}")]
    Invalid {
        what: &'static str,
        why: &'static str,
    },
    #[error("{what} is full at its bound of {bound}")]
    Full { what: &'static str, bound: usize },
    #[error("{0} overflows")]
    Overflow(&'static str),
    #[error("the node holds no such VM")]
    Unknown,
    /// What another product owes Hex (`hex_api::Upstream`), as `Refusal::Awaits` names it.
    #[error("this needs {0}, which the revision Hex builds against doesn't give yet")]
    Awaits(Upstream),
    /// A start the node can't book now: its booking and the room it has (B3: a refusal carries the room).
    #[error("no room: its booking {booking:?} against the room {room:?}")]
    NoRoom { booking: Amounts, room: Amounts },
}

/// A window's CPU reading of a VM's VMM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cpu {
    /// The VMM's whole CPU, its guest's counted in it as KVM and HVF count it (guest_cost's `Use`). The split
    /// by vCPU is upstream's ([`Upstream::GuestCpuTimePerVcpu`]): absent, never made up.
    Whole(Duration),
    /// Each vCPU's guest time, and the VMM's own beside it.
    Split { guest: Vec<Duration>, own: Duration },
}

impl Cpu {
    /// Each vCPU's guest time, where the host reported it.
    pub fn per_vcpu(&self) -> Result<&[Duration], ChargeError> {
        match self {
            Self::Whole(_) => Err(ChargeError::Awaits(Upstream::GuestCpuTimePerVcpu)),
            Self::Split { guest, .. } => Ok(guest),
        }
    }

    /// The VMM's whole CPU: every vCPU's guest time and its own. A split names every vCPU of the VM's.
    fn total(&self, vcpus: u32) -> Result<Duration, ChargeError> {
        match self {
            Self::Whole(total) => Ok(*total),
            Self::Split { guest, own } => {
                if u32::try_from(guest.len()).ok() != Some(vcpus) {
                    return Err(ChargeError::Invalid {
                        what: "a CPU reading",
                        why: "its guest times name a vCPU count other than the VM's",
                    });
                }
                guest.iter().try_fold(*own, |sum, vcpu| {
                    sum.checked_add(*vcpu)
                        .ok_or(ChargeError::Overflow("a VM's CPU over a window"))
                })
            }
        }
    }
}

/// One window's reading of a VM (C1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sample {
    /// The VMM's CPU over the reading's span.
    pub cpu: Cpu,
    /// The time since the reading before. A reading is never charged over less than the window its node reads over
    /// ([`Node::set_window`]).
    pub span: Duration,
    /// The most memory the VMM held in the window, as the census reads it, its guest's touched RAM included.
    pub mem_bytes: u64,
    /// The bytes its own folders hold, where the host reads them: unread, it is read as its booking, so no evidence
    /// below the booking is made up.
    pub disk_bytes: Option<u64>,
    /// The CPU cap it ran under over the span, in milli-CPU, where its platform held one: a window that reached it is
    /// no sample of its VMM's own CPU, which the cap cut (Karn's point, decision 9).
    pub cap_milli: Option<u64>,
}

fn dims(a: Amounts) -> [u64; 3] {
    [a.cpu_milli, a.mem_bytes, a.disk_bytes]
}

fn amounts([cpu_milli, mem_bytes, disk_bytes]: [u64; 3]) -> Amounts {
    Amounts {
        cpu_milli,
        mem_bytes,
        disk_bytes,
    }
}

fn each(a: [u64; 3], b: [u64; 3], f: impl Fn(u64, u64) -> u64) -> [u64; 3] {
    let ([a0, a1, a2], [b0, b1, b2]) = (a, b);
    [f(a0, b0), f(a1, b1), f(a2, b2)]
}

fn sum(a: [u64; 3], b: [u64; 3], what: &'static str) -> Result<[u64; 3], ChargeError> {
    let ([a0, a1, a2], [b0, b1, b2]) = (a, b);
    let add = |x: u64, y: u64| x.checked_add(y).ok_or(ChargeError::Overflow(what));
    Ok([add(a0, b0)?, add(a1, b1)?, add(a2, b2)?])
}

/// The last SEEDABLE readings, each of CPU, memory and disk, and the nearest-rank p99.9 of each.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Tail {
    values: Vec<[u64; 3]>,
    /// Where the next reading goes once the tail is full: over the oldest.
    next: usize,
    top: [Top; 3],
}

/// One dimension's largest reading in a tail and how many it holds, and the largest below it with its count: all
/// the nearest-rank p99.9 of at most SEEDABLE readings needs, counted so a run of equal readings costs no rescan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Top {
    first: u64,
    firsts: usize,
    second: Option<(u64, usize)>,
}

impl Top {
    fn add(&mut self, value: u64) {
        if self.firsts == 0 {
            *self = Self {
                first: value,
                firsts: 1,
                second: None,
            };
            return;
        }
        match value.cmp(&self.first) {
            std::cmp::Ordering::Greater => {
                self.second = Some((self.first, self.firsts));
                self.first = value;
                self.firsts = 1;
            }
            std::cmp::Ordering::Equal => self.firsts = self.firsts.saturating_add(1),
            std::cmp::Ordering::Less => match self.second {
                Some((second, n)) if value == second => {
                    self.second = Some((second, n.saturating_add(1)));
                }
                Some((second, _)) if value < second => {}
                _ => self.second = Some((value, 1)),
            },
        }
    }

    /// Takes one `value` out: whether what it knows still holds, else a rescan is due.
    fn take(&mut self, value: u64) -> bool {
        if value == self.first {
            self.firsts = self.firsts.saturating_sub(1);
            return self.firsts > 0;
        }
        match self.second {
            Some((second, n)) if value == second && n > 1 => {
                self.second = Some((second, n.saturating_sub(1)));
                true
            }
            Some((second, _)) => value < second,
            None => false,
        }
    }

    /// The largest, or with `second` the second largest counting equal readings apart.
    fn kth(&self, second: bool) -> u64 {
        match self.second {
            Some((below, _)) if second && self.firsts < 2 => below,
            _ => self.first,
        }
    }
}

impl Tail {
    fn new() -> Result<Self, ChargeError> {
        let mut values = Vec::new();
        values
            .try_reserve_exact(SEEDABLE)
            .map_err(|_| ChargeError::Full {
                what: "a tail's readings",
                bound: SEEDABLE,
            })?;
        Ok(Self {
            values,
            next: 0,
            top: [Top::default(); 3],
        })
    }

    fn full(&self) -> bool {
        self.values.len() >= SEEDABLE
    }

    fn push(&mut self, reading: [u64; 3]) -> Result<(), ChargeError> {
        let evicted = if self.full() {
            let slot = self.values.get_mut(self.next).ok_or(ChargeError::Invalid {
                what: "a tail",
                why: "its next slot is past its readings",
            })?;
            let old = std::mem::replace(slot, reading);
            self.next = self
                .next
                .checked_add(1)
                .filter(|next| *next < SEEDABLE)
                .unwrap_or(0);
            Some(old)
        } else {
            self.values.push(reading);
            None
        };
        for (d, (top, new)) in self.top.iter_mut().zip(reading).enumerate() {
            let whole = match evicted.as_ref().and_then(|old| old.get(d)) {
                Some(old) => top.take(*old),
                None => true,
            };
            if whole {
                top.add(new);
            } else {
                *top = Top::default();
                for value in self.values.iter().filter_map(|r| r.get(d)) {
                    top.add(*value);
                }
            }
        }
        Ok(())
    }

    /// Each dimension's nearest-rank p99.9: its `n / SEEDABLE + 1`-th largest of `n` readings, the largest
    /// below SEEDABLE and the second largest at it (ceil(0.999 n) = n - floor(n / SEEDABLE)). None when empty.
    fn p999(&self) -> Option<[u64; 3]> {
        if self.values.is_empty() {
            return None;
        }
        let second = self.full();
        let [a, b, c] = self.top;
        Some([a.kth(second), b.kth(second), c.kth(second)])
    }
}

/// A VM's class (C2): VMs of one runtime class and cost profile, admitted at one envelope, that run one kind of thing:
/// one Agentfile image and size. Its ceiling is no part of it: learned from its VMs' windows (C1), it would split the
/// class as it rose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ClassKey {
    class: Class,
    profile: [u8; 32],
    /// What its VMs' workload is admitted at, their guest envelope: a cold class's prior, and its ceiling until its
    /// VMs' windows show their VMM's own past it (C1; ROOT, 2026-11-13: no census number is a ceiling).
    admission: [u64; 3],
    runs: Runs,
}

/// What a class's VMs run (joint plan step 39; ROOT, 2026-11-13): a box's workload, or a node's own guest, its node VM,
/// which runs every oci box of its node. A node VM's windows are a container host's use, no box's workload, so it
/// learns apart from the box VMs of its profile and envelope, never raising their priors or ceilings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum Runs {
    Box,
    Node,
}

impl ClassKey {
    /// The class of box VMs of `class` and cost `profile`, each admitted at `admission`.
    pub fn new(class: Class, profile: [u8; 32], admission: Amounts) -> Self {
        Self {
            class,
            profile,
            admission: dims(admission),
            runs: Runs::Box,
        }
    }

    /// The class of the box VMs `allocation` admits for `class`: its cost profile, and its guest envelope
    /// ([`hex_api::Allocation::guest`]) the admission. Refused where the receipt holds no envelope.
    pub fn of(class: Class, allocation: hex_api::Allocation) -> Result<Self, hex_api::Refusal> {
        Ok(Self::new(class, allocation.cost.id, allocation.guest()?))
    }

    /// The class of node VMs `allocation` admits for `class` (joint plan step 39): as [`ClassKey::of`], apart from
    /// every class of box VMs, as a node VM runs every oci box of its node.
    pub fn node(class: Class, allocation: hex_api::Allocation) -> Result<Self, hex_api::Refusal> {
        Ok(Self {
            runs: Runs::Node,
            ..Self::of(class, allocation)?
        })
    }

    /// What its VMs are admitted at.
    pub fn admission(&self) -> Amounts {
        amounts(self.admission)
    }
}

/// A VM of `class`'s ceiling (C1): its admission and what its class's VMs have shown of their VMM's own past it, the
/// admission alone in a dimension none has shown yet (the cold start).
fn ceiling_of(classes: &BTreeMap<ClassKey, Record>, class: &ClassKey) -> [u64; 3] {
    let [o0, o1, o2] = classes
        .get(class)
        .map_or([None; 3], |record| record.overhead);
    let [a0, a1, a2] = class.admission;
    let past = |admitted: u64, own: Option<u64>| admitted.saturating_add(own.unwrap_or(0));
    [past(a0, o0), past(a1, o1), past(a2, o2)]
}

/// A node's window (C1): some time, where it has one.
fn check_window(window: Option<Duration>) -> Result<(), ChargeError> {
    if window.is_some_and(|window| window.is_zero()) {
        return Err(ChargeError::Invalid {
            what: "a node's window",
            why: "a window of no time",
        });
    }
    Ok(())
}

/// `cpu` over `over` nanoseconds, in milli-CPU, rounded up.
fn milli_of(cpu: Duration, over: u128) -> Result<u64, ChargeError> {
    cpu.as_nanos()
        .checked_mul(1000)
        .map(|n| n.div_ceil(over))
        .and_then(|m| u64::try_from(m).ok())
        .ok_or(ChargeError::Overflow("a VM's CPU rate"))
}

/// What a node holds of one VM.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Vm {
    class: ClassKey,
    band: u8,
    vcpus: u32,
    booking: [u64; 3],
    /// Its last reading: CPU in milli-CPU over the window, memory's and disk's high-water marks; none before its first,
    /// what it holds being no fact yet (ROOT, 2026-11-13: its first reading is a baseline, never a window's growth).
    held: Option<[u64; 3]>,
    tail: Tail,
}

impl Vm {
    /// What it holds of memory and disk as the rule counts it: its last reading; before its first, its booking, what
    /// the ledger holds of it, so a VM never read is never past its budget nor counted as nothing.
    fn holding(&self) -> [u64; 3] {
        self.held.unwrap_or(self.booking)
    }

    /// Its charge (C1): the p99.9 of its last SEEDABLE windows, no less than its booking while it is cold, each at
    /// most `ceiling`, its class's now ([`ceiling_of`]).
    fn charge(&self, ceiling: [u64; 3]) -> [u64; 3] {
        let cold = !self.tail.full();
        let observed = self.tail.p999().unwrap_or([0; 3]);
        let floor = if cold { self.booking } else { [0; 3] };
        each(each(observed, floor, u64::max), ceiling, u64::min)
    }
}

/// What a VM's window gives its class (C2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Evidence {
    /// Nothing: the VM is cold.
    None,
    /// Its whole tail: it warmed with this window.
    Tail,
    /// This window's reading: it was warm.
    One([u64; 3]),
}

/// A class's record: the last SEEDABLE windows of its warmed VMs, when it last took one, and the most its VMs' windows
/// have shown of their VMM's own past their admission (C1), each dimension none until one has: the class's cold start,
/// typed, never a zero. Only rising, as the slowest answer seen is (decision 9).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Record {
    tail: Tail,
    touched: u64,
    overhead: [Option<u64>; 3],
}

/// The most a node's records hold: VMs (its runner population's ceiling) and classes it remembers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bounds {
    pub vms: usize,
    pub classes: usize,
}

/// A VM's memory budget and the room it leaves its guest (C5): what D188 tells its guest, past what it holds;
/// negative where it holds past its budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    pub bytes: u64,
    pub room: i64,
}

/// A fleet's memory against its budget (C5): what its VMs hold, the fastest growth of that on record over one window,
/// the one its node reads over now ([`Node::growth`]), and the budget, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pressure {
    pub held: u64,
    pub growth: u64,
    pub budget: u64,
}

/// Why the last resort ends a VM (C5): what it holds past its own memory budget, its band, and its fleet's memory,
/// which with the fastest growth on record passes the fleet's budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Excess {
    pub held: u64,
    pub budget: u64,
    pub band: u8,
    pub fleet: Pressure,
}

/// The last resort's verdict on a window (C5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resort<K> {
    /// What the fleet's VMs hold, with the fastest growth on record in one window, is within its budget.
    Held,
    /// It isn't: end `vm`, which holds most past its own budget, lowest band first.
    End { vm: K, why: Excess },
    /// It isn't, and every VM holds within its own budget, so none may be ended: growth is no fact yet.
    Within(Pressure),
}

/// What holds a VM its shell couldn't move from what it last set (A-7; ROOT, 2026-11-14: the budget is the contract):
/// the CPU cap ([`Node::rates`]) and the memory budget ([`Node::budgets`]) it runs under as last set, where its shell
/// holds it at one. The node counts it there and shares the rest among the others, so the caps and budgets hold the
/// fleet's whether or not its next setting takes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Held {
    pub cpu_milli: Option<u64>,
    pub mem_bytes: Option<u64>,
}

/// The bounds that hold the rule on Linux (C4, C5): the fleet's parent's, and each of its VMs'. A weight means a
/// share only among its cgroup's siblings, so the fleet's VMs sit alone under its parent, and the parent carries
/// the fleet among the parent's own siblings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placement<K> {
    /// The fleet's parent: `cpu.max` the fleet's CPU budget, `cpu.weight` that budget on the machine's scale
    /// ([`fleet_weight`]). No memory bound of its own: its VMs' budgets sum to the fleet's (C5).
    pub fleet: hex_api::Bounds,
    /// Each VM: `cpu.weight` by its booking ([`Node::weights`]), `cpu.max` its ceiling, and `memory.high` its budget
    /// ([`Node::budgets`]). No `memory.max` (ROOT, 2026-11-13): Hex can't read a VMM's own memory apart from its guest's
    /// touched RAM, so a hard cap there would be a chosen bound, and a VM whose VMM first needs more than its windows
    /// have shown would be killed at it; its budget's hold and the last resort take it instead (C5).
    pub vms: Vec<(K, hex_api::Bounds)>,
}

/// Bugs planted in the core for its tests and hex-sim's scenarios to catch (as `lb::Mutant`). Tests only, on
/// one thread at a time.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mutant {
    /// CPU shared by what each VM asks rather than its booking: a riser outweighs its neighbours.
    WeightsByDemand,
    /// A booking raised to its charge whatever the room: the fleet booked past its budget.
    BookPastRoom,
    /// The last resort ends the VM holding most, within its budget or not.
    EndLargestHolder,
}

#[cfg(test)]
thread_local! {
    static MUTANT: std::cell::Cell<Option<Mutant>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
fn planted(m: Mutant) -> bool {
    MUTANT.with(|c| c.get() == Some(m))
}

/// Runs `f` with `m` planted on this thread, and takes it out after.
#[cfg(test)]
pub fn with_mutant<T>(m: Option<Mutant>, f: impl FnOnce() -> T) -> T {
    MUTANT.with(|c| c.set(m));
    let out = f();
    MUTANT.with(|c| c.set(None));
    out
}

/// One node's VMs, their charges and bookings, within its fleet's budget (C3 to C7). `K` names a VM, and its order is
/// the order the node began its VMs, which every order the rule takes among them follows (C3): a node's learners key
/// each VM by its record's place in the node's start order (`hex-node`'s `Lifetime`), never by anything random.
#[derive(Debug, Clone)]
pub struct Node<K> {
    /// The window it reads over now (C1), none while its shell holds no cap whose period sets one.
    window: Option<Duration>,
    budget: [u64; 3],
    bounds: Bounds,
    vms: BTreeMap<K, Vm>,
    /// The VMs its shell couldn't move from what it last set, each at what holds it (A-7, [`Node::set_held`]): each of
    /// its VMs at most.
    held: BTreeMap<K, Held>,
    classes: BTreeMap<ClassKey, Record>,
    clock: u64,
    /// The fastest growth of what its VMs hold on its record (C5), as the bytes one window grew by and that window's
    /// length: learned, only rising as a rate, so a window that changes with the caps its node holds (C1) neither
    /// shrinks nor swells it ([`Node::growth`]).
    growth: Option<(u64, Duration)>,
    /// This window's growth so far.
    grew: u64,
}

impl<K: Ord + Copy> Node<K> {
    /// A node reading its VMs over `window` (C1), none until its shell holds a cap whose period sets one, its fleet's
    /// budget `budget` (C7). Refused for a window of no time.
    pub fn new(
        window: Option<Duration>,
        budget: Amounts,
        bounds: Bounds,
    ) -> Result<Self, ChargeError> {
        check_window(window)?;
        Ok(Self {
            window,
            budget: dims(budget),
            bounds,
            vms: BTreeMap::new(),
            held: BTreeMap::new(),
            classes: BTreeMap::new(),
            clock: 0,
            growth: None,
            grew: 0,
        })
    }

    /// The fleet's budget changed (step 13's view): bookings past it stay, their VMs run on, and nothing is
    /// admitted until they fit.
    pub fn set_budget(&mut self, budget: Amounts) {
        self.budget = dims(budget);
    }

    /// The VMs its shell couldn't move from what it last set (A-7; ROOT, 2026-11-14), each window's afresh: each is
    /// counted at what holds it ([`Held`]) in [`Node::rates`] and [`Node::budgets`], and the rest share what it leaves.
    /// Only the VMs it tracks are kept.
    pub fn set_held(&mut self, mut held: BTreeMap<K, Held>) {
        let vms = &self.vms;
        held.retain(|id, _| vms.contains_key(id));
        self.held = held;
    }

    /// The CPU cap `id` is held at, where its shell holds it at one ([`Node::set_held`]).
    fn held_cpu(&self, id: &K) -> Option<u64> {
        self.held.get(id).and_then(|held| held.cpu_milli)
    }

    /// The memory budget `id` is held at, where its shell holds it at one ([`Node::set_held`]).
    fn held_mem(&self, id: &K) -> Option<u64> {
        self.held.get(id).and_then(|held| held.mem_bytes)
    }

    /// The window the node reads over from now (C1: a learner's window is the longest period of the caps it holds, an
    /// image VM's its run's own, re-read each window; none while it holds none): each reading after it is charged over
    /// at least it, and the fastest growth on record is projected over it (C5). Its shell sets it after a window's
    /// readings and their settling, before the window's last resort. Refused for a window of no time.
    pub fn set_window(&mut self, window: Option<Duration>) -> Result<(), ChargeError> {
        check_window(window)?;
        self.window = window;
        Ok(())
    }

    /// The window the node reads over now (C1).
    pub fn window(&self) -> Option<Duration> {
        self.window
    }

    pub fn budget(&self) -> Amounts {
        amounts(self.budget)
    }

    /// What a new VM of `class` would be booked at (C2): its class's prior, within its ceiling; with no VM of it
    /// warmed, what its workload is admitted at, its VMM's own learned from its windows.
    pub fn prior(&self, class: &ClassKey) -> Amounts {
        let ceiling = ceiling_of(&self.classes, class);
        let prior = self
            .classes
            .get(class)
            .filter(|record| record.tail.full())
            .and_then(|record| record.tail.p999())
            .map_or(class.admission, |p| each(p, ceiling, u64::min));
        amounts(prior)
    }

    /// A VM of `class`'s ceiling (C1): its admission and what its VMs' windows have shown of their VMM's own past it,
    /// the admission alone in a dimension none has shown yet.
    pub fn ceiling(&self, class: &ClassKey) -> Amounts {
        amounts(ceiling_of(&self.classes, class))
    }

    /// What `class`'s VMs' windows have shown of their VMM's own past their admission (C1), each dimension none while
    /// cold, or with the class forgotten.
    pub fn overhead(&self, class: &ClassKey) -> [Option<u64>; 3] {
        self.classes
            .get(class)
            .map_or([None; 3], |record| record.overhead)
    }

    /// Books a new VM `id` of `class` at its prior if that fits the room (C2, C6); refused with the room if not.
    pub fn admit(
        &mut self,
        id: K,
        class: ClassKey,
        band: u8,
        vcpus: u32,
    ) -> Result<Amounts, ChargeError> {
        if self.vms.contains_key(&id) {
            return Err(ChargeError::Invalid {
                what: "an admission",
                why: "the VM is booked already",
            });
        }
        if self.vms.len() >= self.bounds.vms {
            return Err(ChargeError::Full {
                what: "a node's VMs",
                bound: self.bounds.vms,
            });
        }
        let booking = self.prior(&class);
        let room = self.room()?;
        if !room.holds(booking) {
            return Err(ChargeError::NoRoom { booking, room });
        }
        self.remember(class)?;
        self.vms.insert(
            id,
            Vm {
                class,
                band,
                vcpus,
                booking: dims(booking),
                held: None,
                tail: Tail::new()?,
            },
        );
        Ok(booking)
    }

    /// Takes on a VM `id` its node's own ledger admitted, booked at `booking` (at most its allocation): no room is
    /// checked here, the ledger's admission being the one. Refused past the VMs' bound.
    pub fn track(
        &mut self,
        id: K,
        class: ClassKey,
        band: u8,
        vcpus: u32,
        booking: Amounts,
    ) -> Result<(), ChargeError> {
        if self.vms.contains_key(&id) {
            return Err(ChargeError::Invalid {
                what: "a tracked VM",
                why: "the VM is booked already",
            });
        }
        if self.vms.len() >= self.bounds.vms {
            return Err(ChargeError::Full {
                what: "a node's VMs",
                bound: self.bounds.vms,
            });
        }
        self.remember(class)?;
        let ceiling = ceiling_of(&self.classes, &class);
        self.vms.insert(
            id,
            Vm {
                class,
                band,
                vcpus,
                booking: each(dims(booking), ceiling, u64::min),
                held: None,
                tail: Tail::new()?,
            },
        );
        Ok(())
    }

    /// Takes VM `id`'s first reading, `mem_bytes` of memory held as its learner first meets it, as its baseline (C1, C5;
    /// ROOT, 2026-11-13): what it holds from then, which its room and its budget's hold reckon with, and what its next
    /// reading's growth is counted from, never a window's growth itself. Refused for a VM read before.
    pub fn baseline(&mut self, id: &K, mem_bytes: u64) -> Result<(), ChargeError> {
        let vm = self.vms.get_mut(id).ok_or(ChargeError::Unknown)?;
        if vm.held.is_some() {
            return Err(ChargeError::Invalid {
                what: "a VM's baseline",
                why: "it was read before: a baseline is its first reading",
            });
        }
        let [_, _, booked_disk] = vm.booking;
        vm.held = Some([0, mem_bytes, booked_disk]);
        Ok(())
    }

    /// Whether the node holds the VM `id`.
    pub fn holds(&self, id: &K) -> bool {
        self.vms.contains_key(id)
    }

    /// The VMs it holds.
    pub fn ids(&self) -> impl Iterator<Item = &K> {
        self.vms.keys()
    }

    /// Makes room for `class`'s record, forgetting the class longest untouched that no VM holds now.
    fn remember(&mut self, class: ClassKey) -> Result<(), ChargeError> {
        self.clock = self
            .clock
            .checked_add(1)
            .ok_or(ChargeError::Overflow("a node's class clock"))?;
        if let Some(record) = self.classes.get_mut(&class) {
            record.touched = self.clock;
            return Ok(());
        }
        if self.classes.len() >= self.bounds.classes {
            let held: Vec<ClassKey> = self.vms.values().map(|vm| vm.class).collect();
            let oldest = self
                .classes
                .iter()
                .filter(|(key, _)| !held.contains(key))
                .min_by_key(|(_, record)| record.touched)
                .map(|(key, _)| *key)
                .ok_or(ChargeError::Full {
                    what: "a node's classes",
                    bound: self.bounds.classes,
                })?;
            self.classes.remove(&oldest);
        }
        self.classes.insert(
            class,
            Record {
                tail: Tail::new()?,
                touched: self.clock,
                overhead: [None; 3],
            },
        );
        Ok(())
    }

    /// The VM `id`'s reading for a window (C1).
    pub fn observe(&mut self, id: K, sample: &Sample) -> Result<(), ChargeError> {
        let window = self.window.ok_or(ChargeError::Invalid {
            what: "a VM's reading",
            why: "its node reads over no window",
        })?;
        let vm = self.vms.get_mut(&id).ok_or(ChargeError::Unknown)?;
        let cpu = sample.cpu.total(vm.vcpus)?;
        let over = sample.span.max(window).as_nanos();
        let milli = milli_of(cpu, over)?;
        // What the window shows of its VMM's own past its admission (C1): its split's own CPU, else its whole CPU past
        // the envelope; its memory and its read disk past it. A window that reached its CPU cap is no CPU sample.
        let [cpu_in, mem_in, disk_in] = vm.class.admission;
        let own_cpu = match &sample.cpu {
            _ if sample.cap_milli.is_some_and(|cap| milli >= cap) => None,
            Cpu::Split { own, .. } => Some(milli_of(*own, over)?),
            Cpu::Whole(_) => Some(milli.saturating_sub(cpu_in)),
        };
        let shown = [
            own_cpu,
            Some(sample.mem_bytes.saturating_sub(mem_in)),
            sample.disk_bytes.map(|disk| disk.saturating_sub(disk_in)),
        ];
        let [_, _, booked_disk] = vm.booking;
        let reading = [
            milli,
            sample.mem_bytes,
            sample.disk_bytes.unwrap_or(booked_disk),
        ];
        // Growth is what it holds past its reading before; a first reading is a baseline, none (ROOT, 2026-11-13).
        let before = vm.held.map(|[_, mem, _]| mem);
        vm.held = Some(reading);
        let was_cold = !vm.tail.full();
        vm.tail.push(reading)?;
        // A warmed VM's windows are its class's evidence (C2): its whole tail as it warms, then each window.
        let evidence = match (was_cold, vm.tail.full()) {
            (true, true) => Evidence::Tail,
            (false, true) => Evidence::One(reading),
            _ => Evidence::None,
        };
        let class = vm.class;
        if let Some(before) = before {
            self.grew = self
                .grew
                .saturating_add(sample.mem_bytes.saturating_sub(before));
        }
        // A held VM's class is never forgotten (`remember`), so its record is there.
        let record = self.classes.get_mut(&class).ok_or(ChargeError::Invalid {
            what: "a class's record",
            why: "a held VM's class was forgotten",
        })?;
        let [r0, r1, r2] = record.overhead;
        let [s0, s1, s2] = shown;
        let most = |was: Option<u64>, now: Option<u64>| match (was, now) {
            (Some(was), Some(now)) => Some(was.max(now)),
            (was, now) => was.or(now),
        };
        record.overhead = [most(r0, s0), most(r1, s1), most(r2, s2)];
        if evidence == Evidence::None {
            return Ok(());
        }
        self.remember(class)?;
        let Self { vms, classes, .. } = self;
        let record = classes.get_mut(&class).ok_or(ChargeError::Invalid {
            what: "a class's record",
            why: "it was forgotten as it was taken",
        })?;
        match evidence {
            Evidence::One(reading) => record.tail.push(reading),
            Evidence::Tail => {
                // The tail just filled, so its readings are in the order they came.
                let vm = vms.get(&id).ok_or(ChargeError::Unknown)?;
                vm.tail
                    .values
                    .iter()
                    .try_for_each(|reading| record.tail.push(*reading))
            }
            Evidence::None => Ok(()),
        }
    }

    /// After a window's readings (C3): the fastest growth on record taken, every booking lowered to its charge,
    /// then raised toward it only into room no booking holds and, for memory and disk, no VM holds, higher bands
    /// first and, within a band, the earliest begun first (by `K`).
    pub fn settle(&mut self) -> Result<(), ChargeError> {
        // This window's growth, over the window its readings were charged over, where it is the fastest on record.
        if let Some(window) = self.window
            && self.grew > 0
        {
            let faster = self.growth.is_none_or(|(bytes, over)| {
                u128::from(self.grew).saturating_mul(over.as_nanos())
                    > u128::from(bytes).saturating_mul(window.as_nanos())
            });
            if faster {
                self.growth = Some((self.grew, window));
            }
        }
        self.grew = 0;
        for vm in self.vms.values_mut() {
            vm.booking = each(
                vm.booking,
                vm.charge(ceiling_of(&self.classes, &vm.class)),
                u64::min,
            );
        }
        let mut booked = [0u64; 3];
        let mut held = [0u64; 3];
        for vm in self.vms.values() {
            booked = sum(booked, vm.booking, "a node's bookings")?;
            held = sum(held, physical(vm), "what a node's VMs hold")?;
        }
        let mut room = each(self.budget, booked, u64::saturating_sub);
        let mut free = each(self.budget, held, u64::saturating_sub);
        let mut order: Vec<(u8, K)> = self.vms.iter().map(|(id, vm)| (vm.band, *id)).collect();
        order.sort_by(|(a, x), (b, y)| b.cmp(a).then(x.cmp(y)));
        for (_, id) in order {
            let Some(vm) = self.vms.get_mut(&id) else {
                continue;
            };
            let charge = vm.charge(ceiling_of(&self.classes, &vm.class));
            let before = physical(vm);
            #[cfg(test)]
            if planted(Mutant::BookPastRoom) {
                vm.booking = each(vm.booking, charge, u64::max);
                continue;
            }
            let [c0, c1, c2] = charge;
            let [b0, b1, b2] = vm.booking;
            let [h0, h1, h2] = before;
            let [r0, r1, r2] = room;
            let [f0, f1, f2] = free;
            let raise = |c: u64, b: u64, h: u64, r: u64, f: u64| {
                c.max(b).min(b.saturating_add(r)).min(h.saturating_add(f))
            };
            vm.booking = [
                raise(c0, b0, h0, r0, f0),
                raise(c1, b1, h1, r1, f1),
                raise(c2, b2, h2, r2, f2),
            ];
            let after = physical(vm);
            room = each(
                room,
                each(vm.booking, [b0, b1, b2], u64::saturating_sub),
                u64::saturating_sub,
            );
            free = each(
                free,
                each(after, before, u64::saturating_sub),
                u64::saturating_sub,
            );
        }
        Ok(())
    }

    /// What the node can book now (C6): its budget less every booking, and less what memory and disk VMs hold
    /// past their bookings.
    pub fn room(&self) -> Result<Amounts, ChargeError> {
        Ok(amounts(each(
            self.budget,
            dims(self.reservation()?),
            u64::saturating_sub,
        )))
    }

    /// What the node publishes as Hex's reservation (C8): every booking, and what memory and disk VMs hold past
    /// their bookings.
    pub fn reservation(&self) -> Result<Amounts, ChargeError> {
        let mut reserved = [0u64; 3];
        for vm in self.vms.values() {
            reserved = sum(reserved, physical(vm), "a node's reservation")?;
        }
        Ok(amounts(reserved))
    }

    /// What one VM takes of its node (C8): its booking, and the memory and disk it holds past it. The node's
    /// reservation is the sum of these.
    pub fn holding(&self, id: &K) -> Option<Amounts> {
        self.vms.get(id).map(|vm| amounts(physical(vm)))
    }

    pub fn charge(&self, id: &K) -> Option<Amounts> {
        self.vms
            .get(id)
            .map(|vm| amounts(vm.charge(ceiling_of(&self.classes, &vm.class))))
    }

    pub fn booking(&self, id: &K) -> Option<Amounts> {
        self.vms.get(id).map(|vm| amounts(vm.booking))
    }

    /// Whether the VM has its SEEDABLE windows (C1).
    pub fn warmed(&self, id: &K) -> Option<bool> {
        self.vms.get(id).map(|vm| vm.tail.full())
    }

    /// The VMs whose charge passes their booking, each by how much (C3): they alone bear the contention.
    pub fn over(&self) -> Vec<(K, Amounts)> {
        self.vms
            .iter()
            .filter_map(|(id, vm)| {
                let charge = vm.charge(ceiling_of(&self.classes, &vm.class));
                let excess = each(charge, vm.booking, u64::saturating_sub);
                (excess != [0; 3]).then(|| (*id, amounts(excess)))
            })
            .collect()
    }

    /// Each VM's CPU cap for its next window where the platform caps CPU rather than weighs it (C4 on macOS and
    /// Windows): each VM its booking, then what `capacity` milli-CPU (the fleet's CPU budget) leaves past the bookings
    /// water-filled by booking, as the kernel's weights would share it, over what each asked past its booking in its
    /// last window. `used` says what each used against the cap it ran under: one that used its whole cap, or has run no
    /// window yet, may ask its ceiling. So every VM keeps at least its booking, the caps sum to at most `capacity`
    /// where the bookings fit it (C3; bookings past it stay, as C3 keeps them), and each cap is at most its ceiling and
    /// at least `least` of it, the least rate the actuator holding it takes (its governor's, its platform's cap's). A
    /// VM held at its last cap ([`Node::set_held`], A-7) keeps it, counted there, and the rest share what it leaves.
    pub fn rates(
        &self,
        capacity: u64,
        used: &BTreeMap<K, (u64, u64)>,
        least: impl Fn(&K) -> u64,
    ) -> Result<Vec<(K, u64)>, ChargeError> {
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(self.vms.len())
            .map_err(|_| ChargeError::Full {
                what: "a window's rates",
                bound: self.vms.len(),
            })?;
        let mut booked = 0u64;
        let mut kept = 0u64;
        for (id, vm) in &self.vms {
            if let Some(cap) = self.held_cpu(id) {
                kept = kept
                    .checked_add(cap)
                    .ok_or(ChargeError::Overflow("a fleet's held CPU caps"))?;
                continue;
            }
            let [ceiling, _, _] = ceiling_of(&self.classes, &vm.class);
            let [booking, _, _] = vm.booking;
            let asked = match used.get(id) {
                Some((used, cap)) if used < cap => *used,
                _ => ceiling,
            };
            booked = booked
                .checked_add(booking)
                .ok_or(ChargeError::Overflow("a fleet's CPU bookings"))?;
            entries.push((booking.max(1), asked.min(ceiling).saturating_sub(booking)));
        }
        let mut extra = shares(
            capacity.saturating_sub(kept).saturating_sub(booked),
            &entries,
        )?
        .into_iter();
        self.vms
            .iter()
            .map(|(id, vm)| {
                if let Some(cap) = self.held_cpu(id) {
                    return Ok((*id, cap));
                }
                let [booking, _, _] = vm.booking;
                let extra = extra.next().ok_or(ChargeError::Invalid {
                    what: "a window's rates",
                    why: "a VM with no share",
                })?;
                let cap = booking
                    .checked_add(extra)
                    .ok_or(ChargeError::Overflow("a VM's CPU cap"))?;
                Ok((*id, cap.max(least(id))))
            })
            .collect()
    }

    /// CPU over a contended window (C4): `capacity` milli-CPU shared by water-filling, each VM weighted by its
    /// booking (at least 1, as the kernel's least weight) and given at most what it asks and its ceiling.
    pub fn shares(
        &self,
        capacity: u64,
        demands: &BTreeMap<K, u64>,
    ) -> Result<Vec<(K, u64)>, ChargeError> {
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(demands.len())
            .map_err(|_| ChargeError::Full {
                what: "a window's shares",
                bound: demands.len(),
            })?;
        for (id, demand) in demands {
            let vm = self.vms.get(id).ok_or(ChargeError::Unknown)?;
            let [ceiling, _, _] = ceiling_of(&self.classes, &vm.class);
            let [booking, _, _] = vm.booking;
            let need = (*demand).min(ceiling);
            let weight = booking.max(1);
            #[cfg(test)]
            let weight = if planted(Mutant::WeightsByDemand) {
                need.max(1)
            } else {
                weight
            };
            entries.push((weight, need));
        }
        let given = shares(capacity, &entries)?;
        Ok(demands.keys().copied().zip(given).collect())
    }

    /// Each VM's `cpu.weight` (C4): its booking's share of the largest, on the kernel's range, rounded up.
    pub fn weights(&self) -> Result<Vec<(K, u16)>, ChargeError> {
        let largest = self
            .vms
            .values()
            .map(|vm| vm.booking)
            .map(|[cpu, _, _]| cpu)
            .max()
            .unwrap_or(0);
        self.vms
            .iter()
            .map(|(id, vm)| {
                let [cpu, _, _] = vm.booking;
                let weight = if largest == 0 {
                    WEIGHT_MIN
                } else {
                    let scaled = u128::from(cpu)
                        .checked_mul(u128::from(WEIGHT_MAX))
                        .map(|n| n.div_ceil(u128::from(largest)))
                        .ok_or(ChargeError::Overflow("a CPU weight"))?;
                    u16::try_from(scaled)
                        .map_err(|_| ChargeError::Overflow("a CPU weight"))?
                        .max(WEIGHT_MIN)
                };
                Ok((*id, weight))
            })
            .collect()
    }

    /// Each VM's memory budget and room (C5): its booking and a share of the slack in proportion to its booking,
    /// the remainder a byte each to the first VMs, so the budgets sum to the fleet's budget. A VM held at its last
    /// budget ([`Node::set_held`], A-7) keeps it, counted there, and the rest share what it leaves.
    pub fn budgets(&self) -> Result<Vec<(K, Budget)>, ChargeError> {
        let [_, budget, _] = self.budget;
        let mut booked = 0u64;
        let mut kept = 0u64;
        let mut count = 0u64;
        for (id, vm) in &self.vms {
            if let Some(bytes) = self.held_mem(id) {
                kept = kept
                    .checked_add(bytes)
                    .ok_or(ChargeError::Overflow("a node's held memory budgets"))?;
                continue;
            }
            let [_, mem, _] = vm.booking;
            booked = booked
                .checked_add(mem)
                .ok_or(ChargeError::Overflow("a node's memory bookings"))?;
            count = count
                .checked_add(1)
                .ok_or(ChargeError::Overflow("a node's VM count"))?;
        }
        let slack = budget.saturating_sub(kept).saturating_sub(booked);
        let share = |mem: u64| -> Result<u64, ChargeError> {
            let parts = if booked == 0 {
                Some(slack.checked_div(count).unwrap_or(0))
            } else {
                u128::from(slack)
                    .checked_mul(u128::from(mem))
                    .and_then(|n| n.checked_div(u128::from(booked)))
                    .and_then(|n| u64::try_from(n).ok())
            };
            parts.ok_or(ChargeError::Overflow("a VM's share of the slack"))
        };
        let mut given = 0u64;
        let mut out = Vec::new();
        for (id, vm) in &self.vms {
            if self.held_mem(id).is_some() {
                continue;
            }
            let [_, mem, _] = vm.booking;
            given = given
                .checked_add(share(mem)?)
                .ok_or(ChargeError::Overflow("the slack's shares"))?;
        }
        let mut left = slack.saturating_sub(given);
        for (id, vm) in &self.vms {
            let bytes = match self.held_mem(id) {
                Some(bytes) => bytes,
                None => {
                    let [_, mem, _] = vm.booking;
                    let extra = u64::from(left > 0);
                    left = left.saturating_sub(extra);
                    mem.checked_add(share(mem)?)
                        .and_then(|b| b.checked_add(extra))
                        .ok_or(ChargeError::Overflow("a VM's memory budget"))?
                }
            };
            let [_, held, _] = vm.holding();
            let room = i64::try_from(bytes)
                .ok()
                .zip(i64::try_from(held).ok())
                .and_then(|(b, h)| b.checked_sub(h))
                .ok_or(ChargeError::Overflow("a VM's room"))?;
            out.push((*id, Budget { bytes, room }));
        }
        Ok(out)
    }

    /// The bounds that hold the rule (C4, C5) on a machine of `machine_milli` CPU: the fleet's parent, its CPU
    /// budget its cap (at least the kernel's least quota, `hex_api::MIN_CPU_MILLI`) and its weight among its
    /// siblings ([`fleet_weight`]); each VM by its weight, its CPU ceiling and its memory budget.
    pub fn placement(&self, machine_milli: u64) -> Result<Placement<K>, ChargeError> {
        let milli =
            |what: &'static str, n: u64| u32::try_from(n).map_err(|_| ChargeError::Overflow(what));
        let [cpu, _, _] = self.budget;
        let fleet = hex_api::Bounds {
            cpu_weight: Some(fleet_weight(cpu, machine_milli)?),
            cpu_max_milli: Some(milli(
                "a fleet's CPU budget",
                cpu.max(u64::from(hex_api::MIN_CPU_MILLI)),
            )?),
            ..hex_api::Bounds::default()
        };
        let mut vms = Vec::new();
        vms.try_reserve_exact(self.vms.len())
            .map_err(|_| ChargeError::Full {
                what: "a fleet's placement",
                bound: self.vms.len(),
            })?;
        let each = self.vms.iter().zip(self.weights()?).zip(self.budgets()?);
        for (((id, vm), (_, weight)), (_, budget)) in each {
            let [cpu, _, _] = ceiling_of(&self.classes, &vm.class);
            vms.push((
                *id,
                hex_api::Bounds {
                    cpu_weight: Some(weight),
                    cpu_max_milli: Some(milli("a VM's ceiling", cpu)?),
                    memory_high: Some(budget.bytes),
                    ..hex_api::Bounds::default()
                },
            ));
        }
        Ok(Placement { fleet, vms })
    }

    /// The last resort (C5): where what the fleet's VMs hold and the fastest growth on record in one window pass
    /// its budget, the VM holding most past its own budget, lowest band first, of VMs equally past theirs the latest
    /// begun (by `K`, C3); never one within its budget, so never one within its booking, nor one held at its budget as
    /// Linux's `memory.high` holds it.
    pub fn last_resort(&self) -> Result<Resort<K>, ChargeError> {
        let [_, budget, _] = self.budget;
        let held = self
            .vms
            .values()
            .map(Vm::holding)
            .fold(0u64, |total, [_, mem, _]| total.saturating_add(mem));
        let growth = self.growth();
        let fleet = Pressure {
            held,
            growth,
            budget,
        };
        if held.saturating_add(growth) <= budget {
            return Ok(Resort::Held);
        }
        let budgets = self.budgets()?;
        let excess = |id: &K, bytes: u64| {
            self.vms.get(id).map(|vm| {
                let [_, held, _] = vm.holding();
                Excess {
                    held,
                    budget: bytes,
                    band: vm.band,
                    fleet,
                }
            })
        };
        #[cfg(test)]
        if planted(Mutant::EndLargestHolder) {
            let largest = budgets.iter().max_by_key(|(id, _)| {
                self.vms.get(id).map_or(0, |vm| {
                    let [_, mem, _] = vm.holding();
                    mem
                })
            });
            return Ok(largest
                .and_then(|(id, b)| excess(id, b.bytes).map(|why| Resort::End { vm: *id, why }))
                .unwrap_or(Resort::Within(fleet)));
        }
        let chosen = budgets
            .iter()
            .filter_map(|(id, budget)| {
                let vm = self.vms.get(id)?;
                let past = u64::try_from(budget.room.checked_neg()?).ok()?;
                if past > 0 {
                    Some((
                        vm.band,
                        std::cmp::Reverse(past),
                        std::cmp::Reverse(*id),
                        budget.bytes,
                    ))
                } else {
                    None
                }
            })
            .min();
        Ok(chosen
            .and_then(|(_, _, std::cmp::Reverse(id), bytes)| {
                excess(&id, bytes).map(|why| Resort::End { vm: id, why })
            })
            .unwrap_or(Resort::Within(fleet)))
    }

    /// The fastest growth of what the fleet's VMs hold on this node's record (C5), over the window it reads over now:
    /// the rate on record over that window's length, rounded up; as it was seen where the node reads over none.
    pub fn growth(&self) -> u64 {
        let Some((bytes, over)) = self.growth else {
            return 0;
        };
        let Some(now) = self.window else {
            return bytes;
        };
        let projected = u128::from(bytes)
            .saturating_mul(now.as_nanos())
            .div_ceil(over.as_nanos().max(1));
        u64::try_from(projected).unwrap_or(u64::MAX)
    }

    /// The VM ended or was deleted: its booking frees; its class keeps what it learned.
    pub fn end(&mut self, id: &K) -> Result<(), ChargeError> {
        self.held.remove(id);
        self.vms.remove(id).map(drop).ok_or(ChargeError::Unknown)
    }

    pub fn len(&self) -> usize {
        self.vms.len()
    }

    pub fn is_empty(&self) -> bool {
        self.vms.is_empty()
    }
}

/// What a VM takes of its node: its booking, and for memory and disk what it holds past it.
fn physical(vm: &Vm) -> [u64; 3] {
    let [b0, b1, b2] = vm.booking;
    let [_, h1, h2] = vm.holding();
    [b0, b1.max(h1), b2.max(h2)]
}

/// A fleet's `cpu.weight` among its parent's children (C4): its CPU budget's share of the machine's CPU on the
/// kernel's range, rounded up, at least [`WEIGHT_MIN`]. [`WEIGHT_MAX`] is the whole machine, so siblings weighed
/// on the same scale share by their budgets; a budget past the machine weighs the whole machine.
pub fn fleet_weight(budget_milli: u64, machine_milli: u64) -> Result<u16, ChargeError> {
    if machine_milli == 0 {
        return Err(ChargeError::Invalid {
            what: "a machine's CPU",
            why: "a machine with no CPU",
        });
    }
    let scaled = u128::from(budget_milli.min(machine_milli))
        .checked_mul(u128::from(WEIGHT_MAX))
        .map(|n| n.div_ceil(u128::from(machine_milli)))
        .ok_or(ChargeError::Overflow("a fleet's CPU weight"))?;
    let weight =
        u16::try_from(scaled).map_err(|_| ChargeError::Overflow("a fleet's CPU weight"))?;
    Ok(weight.max(WEIGHT_MIN))
}

/// What the fleet may take of the machine's scale (C4) once every other child of the node's cgroup has its weight,
/// `taken` their sum: the units left, at least [`WEIGHT_MIN`], and the CPU those units carry, in milli-CPU rounded
/// down. Each other child weighs its booking rounded up ([`fleet_weight`]), so a fleet budgeted within that CPU and
/// weighed by [`fleet_weight`] keeps the children's weights within the scale's top: under contention each child,
/// given at least its weight's share of the machine, receives at least min(its need, its booking or budget).
pub fn fleet_room(taken: u64, machine_milli: u64) -> Result<(u16, u64), ChargeError> {
    if machine_milli == 0 {
        return Err(ChargeError::Invalid {
            what: "a machine's CPU",
            why: "a machine with no CPU",
        });
    }
    let left = u64::from(WEIGHT_MAX).saturating_sub(taken);
    let carried = u128::from(machine_milli)
        .checked_mul(u128::from(left))
        .and_then(|n| n.checked_div(u128::from(WEIGHT_MAX)))
        .and_then(|n| u64::try_from(n).ok())
        .ok_or(ChargeError::Overflow("the CPU a fleet's units carry"))?;
    let units = u16::try_from(left.max(u64::from(WEIGHT_MIN)))
        .map_err(|_| ChargeError::Overflow("a fleet's units"))?;
    Ok((units, carried))
}

/// Weighted water-filling (C4): `capacity` shared among `entries`, each `(weight, need)` with a weight of at least
/// 1, as the kernel shares CPU among groups by weight, work-conserving (cgroup-v2.rst, "Weights"). Taken in order
/// of need over weight, each is given its need while its weighted share of what is left covers it; from the first
/// it doesn't, every one left is given its weighted share, rounded down, and what rounding leaves a unit each in
/// that order. With the sum of `min(need, weight)` within capacity, each is given at least `min(need, weight)`.
pub fn shares(capacity: u64, entries: &[(u64, u64)]) -> Result<Vec<u64>, ChargeError> {
    if entries.iter().any(|(weight, _)| *weight == 0) {
        return Err(ChargeError::Invalid {
            what: "a share's weight",
            why: "a weight of 0, under the kernel's least",
        });
    }
    let mut order: Vec<usize> = (0..entries.len()).collect();
    let ratio = |i: &usize| entries.get(*i).copied().unwrap_or((1, 0));
    // A u64 times a u64 fits a u128: the products saturate never.
    order.sort_by(|a, b| {
        let ((wa, na), (wb, nb)) = (ratio(a), ratio(b));
        u128::from(na)
            .saturating_mul(u128::from(wb))
            .cmp(&u128::from(nb).saturating_mul(u128::from(wa)))
    });
    let mut given = vec![0u64; entries.len()];
    let mut left = u128::from(capacity);
    let mut weight = entries
        .iter()
        .try_fold(0u128, |total, (w, _)| total.checked_add(u128::from(*w)))
        .ok_or(ChargeError::Overflow("the shares' weights"))?;
    for (at, i) in order.iter().enumerate() {
        let (w, need) = ratio(i);
        if u128::from(need).saturating_mul(weight) <= left.saturating_mul(u128::from(w)) {
            if let Some(slot) = given.get_mut(*i) {
                *slot = need;
            }
            left = left.saturating_sub(u128::from(need));
            weight = weight.saturating_sub(u128::from(w));
            continue;
        }
        let rest = order.get(at..).unwrap_or(&[]);
        let mut spent = 0u128;
        for j in rest {
            let (wj, _) = ratio(j);
            let part = left
                .checked_mul(u128::from(wj))
                .and_then(|n| n.checked_div(weight))
                .ok_or(ChargeError::Overflow("a weighted share"))?;
            let part =
                u64::try_from(part).map_err(|_| ChargeError::Overflow("a weighted share"))?;
            if let Some(slot) = given.get_mut(*j) {
                *slot = part;
            }
            spent = spent.saturating_add(u128::from(part));
        }
        let mut spare = left.saturating_sub(spent);
        for j in rest {
            if spare == 0 {
                break;
            }
            let (_, need) = ratio(j);
            if let Some(slot) = given.get_mut(*j)
                && *slot < need
            {
                *slot = slot.saturating_add(1);
                spare = spare.saturating_sub(1);
            }
        }
        break;
    }
    Ok(given)
}

#[cfg(test)]
mod tests;
