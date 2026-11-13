//! Which cgroup an `oci` box is given (goal 5 extended, step 6; decision 5, "The cgroup lock"): a box
//! whose spec names an owner may be given a cgroup the owner's earlier box left, rather than one made
//! for it, which takes the cgroup's `mkdir`, its `rmdir` and the kworkers' twelve holds of the host-wide
//! `cgroup_mutex` off its lifecycle (docs/research/runtime/goal5-cgroup-lock.md, f20, f23, f62). SOCK,
//! AFaaS, RunD and Lambda reuse within one owner too (f36-f39). This is the pure core: the idle cgroups
//! by owner, their bounds and what goes past them, the check an idle cgroup's files must pass before it
//! is handed on, and the counts that show each path taken. The node's shell reads the files, and makes,
//! bounds and removes the cgroups.
//!
//! A cgroup that may be handed on is named by its slot ([`name`]), never by a box's id, since a cgroup
//! can't be renamed (f35), and the box's record names the slot (f64). Slots are numbered once each, in
//! order, so a slot being removed is never made again under its name. A box with no owner, an owner that
//! opts out, or a box that could change its own cgroup gets a cgroup of its own named by its id, as
//! every box did before, and removed with it ([`Take::Own`]).

use std::collections::{BTreeMap, VecDeque};

use crate::cgroup::flat;
use crate::demand::Ghosts;

/// The name of slot `n`'s cgroup under the oci boxes' root: a `.` and its number. No box id starts with
/// `.`, so no slot's name is a box's.
pub fn name(n: u64) -> String {
    format!(".{n}")
}

/// The slot a cgroup's name says, if it is a slot's ([`name`]).
pub fn slot_of(name: &str) -> Option<u64> {
    let digits = name.strip_prefix('.')?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n = digits.parse().ok()?;
    // One spelling per slot, so a name like `.07` is no slot's.
    if self::name(n) == name { Some(n) } else { None }
}

/// Why a box was given a cgroup of its own or a fresh one, or why an idle cgroup wasn't handed on: each
/// counted ([`Counts::refused`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Refused {
    /// The box names no owner: a cgroup of its own.
    NoOwner,
    /// Its owner opts out: a cgroup of its own.
    OptedOut,
    /// The box could change its own cgroup or what is attached to it, so a later box could inherit what
    /// it left: its process may hold CAP_SYS_ADMIN (its spec gives no capability sets, so it keeps its
    /// creator's, or names it), or holds CAP_BPF with CAP_NET_ADMIN, which load and attach a cgroup device
    /// program (Linux v6.19, kernel/bpf/syscall.c:2930-2941). A cgroup of its own.
    Privileged,
    /// Its owner has idle cgroups, none of them under the box's device policy: a fresh one.
    Policy,
    /// Its owner has no idle cgroup but other owners do, and none crosses owners: a fresh one.
    Owner,
    /// The node has numbered every slot a `u64` names: a cgroup of its own.
    Slots,
    /// `cgroup.events` says something runs in it or below it.
    Populated,
    /// `cgroup.stat` counts a live sub-cgroup.
    Descendants,
    /// `cgroup.stat` counts a dying sub-cgroup.
    Dying,
    /// `cgroup.subtree_control` hands a controller down.
    SubtreeControl,
    /// `cgroup.type` isn't `domain`.
    NotDomain,
    /// `cgroup.freeze` or `cgroup.events` says it is frozen.
    Frozen,
    /// `memory.oom.group` isn't 0, as Hex leaves it.
    OomGroup,
    /// `memory.current` is over the threshold, or the threshold isn't under the box's memory bound.
    Charged,
    /// One of its files couldn't be read, or doesn't say what the kernel's says.
    Unreadable,
}

impl Refused {
    /// Every reason, in order.
    pub const ALL: [Self; 15] = [
        Self::NoOwner,
        Self::OptedOut,
        Self::Privileged,
        Self::Policy,
        Self::Owner,
        Self::Slots,
        Self::Populated,
        Self::Descendants,
        Self::Dying,
        Self::SubtreeControl,
        Self::NotDomain,
        Self::Frozen,
        Self::OomGroup,
        Self::Charged,
        Self::Unreadable,
    ];

    /// Its name, as counts and refusals print it.
    pub fn name(self) -> &'static str {
        match self {
            Self::NoOwner => "no owner",
            Self::OptedOut => "owner opted out",
            Self::Privileged => "privileged",
            Self::Policy => "another device policy",
            Self::Owner => "another owner's",
            Self::Slots => "slots spent",
            Self::Populated => "populated",
            Self::Descendants => "sub-cgroup",
            Self::Dying => "dying sub-cgroup",
            Self::SubtreeControl => "subtree_control",
            Self::NotDomain => "not a domain",
            Self::Frozen => "frozen",
            Self::OomGroup => "memory.oom.group",
            Self::Charged => "charged",
            Self::Unreadable => "unreadable",
        }
    }
}

/// Why idle cgroups were removed: each counted ([`Counts`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Evicted {
    /// Past its owner's bound, learned ([`Pool::new`]).
    OwnerBound,
    /// Past the node's bound.
    NodeBound,
    /// Its check refused it ([`Pool::refuse`]).
    Refused,
    /// The node let all of them go ([`Pool::drain`]).
    Drained,
}

/// What the pool has done since it opened: what tests, the bench and the node's metrics read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Counts {
    /// Idle cgroups handed to a box of their owner.
    pub handed_on: u64,
    /// Fresh slots given to boxes that may hand their cgroup on.
    pub fresh: u64,
    /// Boxes given a fresh slot because no owner had an idle cgroup.
    pub missed: u64,
    /// Each reason a box got a fresh slot or a cgroup of its own, or an idle cgroup wasn't handed on, by
    /// its count.
    pub refused: BTreeMap<Refused, u64>,
    pub evicted_owner_bound: u64,
    pub evicted_node_bound: u64,
    pub evicted_refused: u64,
    pub drained: u64,
    /// Idle cgroups now.
    pub idle: usize,
    /// The most idle cgroups an owner keeps now, learned ([`Pool::new`]).
    pub per_owner: usize,
}

impl Counts {
    /// How many times `reason` was counted.
    pub fn refused(&self, reason: Refused) -> u64 {
        self.refused.get(&reason).copied().unwrap_or(0)
    }
}

/// The cgroup a box is to be given ([`Pool::take`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Take {
    /// Idle slot `n`, its owner's under the box's device policy: hand it on once its files pass
    /// [`check`] ([`Pool::hand_on`]), else refuse it ([`Pool::refuse`]), remove it, and take again.
    Idle(u64),
    /// A fresh cgroup at slot `n`, which may be handed on after the box.
    Fresh(u64),
    /// A cgroup of the box's own, named by its id and removed with it, for this reason.
    Own(Refused),
}

/// Who a box belongs to, as the pool sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Whose<'a> {
    /// The owner's name, `None` when the box names none.
    pub owner: Option<&'a str>,
    /// Whether the owner lets its boxes' cgroups be handed on.
    pub reuse: bool,
    /// Whether the box could change its own cgroup ([`Refused::Privileged`]).
    pub privileged: bool,
}

/// An idle cgroup: its slot, the device policy whose program is attached to it (`K`, the shell's), and
/// when it went idle, among all the node's.
#[derive(Debug, Clone)]
struct Idle<K> {
    slot: u64,
    policy: K,
    seq: u64,
}

/// A node's idle cgroups, by owner, each owner's oldest first.
#[derive(Debug, Clone)]
pub struct Pool<K> {
    /// The most idle cgroups an owner keeps, learned from its misses.
    per_owner: Ghosts,
    per_node: usize,
    idle: BTreeMap<String, VecDeque<Idle<K>>>,
    total: usize,
    /// The next fresh slot's number.
    next: u64,
    /// The order idle cgroups went idle in, across owners, so the node's oldest is found.
    seq: u64,
    counts: Counts,
}

impl<K: PartialEq + Clone> Pool<K> {
    /// An empty pool keeping at most `per_node` idle cgroups for all owners, and for one owner `per_owner` at
    /// first, numbering fresh slots from `first`: one past every slot a record or a cgroup named when the node
    /// started, so no slot is made under a name already there.
    ///
    /// The owner bound is learned (Ada's rule, 2026-11-06): each idle cgroup it puts out is remembered as its
    /// owner's ghost, and a take that then finds the owner with none idle raises it by one, up to `per_node`
    /// ([`Ghosts`]: ARC's ghost hits, Megiddo and Modha, FAST 2003, §4.3). So an owner whose burst the bound cut
    /// one round finds the whole burst idle the next; it never falls, so no demand already seen meets it again.
    pub fn new(per_owner: usize, per_node: usize, first: u64) -> Self {
        let per_owner = Ghosts::new(per_owner, per_node);
        Self {
            counts: Counts {
                per_owner: per_owner.bound(),
                ..Counts::default()
            },
            per_owner,
            per_node,
            idle: BTreeMap::new(),
            total: 0,
            next: first,
            seq: 0,
        }
    }

    /// The most idle cgroups an owner keeps now.
    pub fn per_owner(&self) -> usize {
        self.per_owner.bound()
    }

    /// The cgroup for a box `whose` is, whose device policy is `policy`: the owner's idle cgroup under
    /// that policy that went idle last, else a fresh slot, or a cgroup of its own for a box with no
    /// owner, an owner that opts out, or a box that could change its cgroup. A slot taken idle is no
    /// longer idle: the caller hands it on or refuses it.
    pub fn take(&mut self, whose: Whose<'_>, policy: &K) -> Take {
        let owner = match whose {
            Whose { owner: None, .. } => return self.own(Refused::NoOwner),
            Whose { reuse: false, .. } => return self.own(Refused::OptedOut),
            Whose {
                privileged: true, ..
            } => return self.own(Refused::Privileged),
            Whose { owner: Some(o), .. } => o,
        };
        let found = match self.idle.get(owner) {
            Some(list) => list
                .iter()
                .rposition(|i| i.policy == *policy)
                .ok_or(Some(Refused::Policy)),
            None => {
                self.per_owner.missed(owner);
                self.counts.per_owner = self.per_owner.bound();
                Err(if self.total > 0 {
                    Some(Refused::Owner)
                } else {
                    None
                })
            }
        };
        match found {
            Ok(at) => {
                let taken = self.idle.get_mut(owner).and_then(|list| list.remove(at));
                if self.idle.get(owner).is_some_and(VecDeque::is_empty) {
                    self.idle.remove(owner);
                }
                if let Some(idle) = taken {
                    self.total = self.total.saturating_sub(1);
                    self.counts.idle = self.total;
                    return Take::Idle(idle.slot);
                }
                self.counts.missed = self.counts.missed.saturating_add(1);
            }
            Err(Some(reason)) => self.count(reason),
            Err(None) => self.counts.missed = self.counts.missed.saturating_add(1),
        }
        self.fresh()
    }

    /// A slot number for a cgroup the shell makes ahead for boxes of no owner, each with an init in it (decision
    /// 8, "The p99.9 that stayed"), counted fresh; such a slot is removed with its box, never given back. `None`
    /// once every slot number is spent.
    pub fn ahead(&mut self) -> Option<u64> {
        match self.fresh() {
            Take::Fresh(n) => Some(n),
            _ => None,
        }
    }

    /// The idle slot taken last passed its check and is the box's.
    pub fn hand_on(&mut self) {
        self.counts.handed_on = self.counts.handed_on.saturating_add(1);
    }

    /// The idle slot taken last failed its check for `reason`: the caller removes it.
    pub fn refuse(&mut self, reason: Refused) {
        self.count(reason);
        self.counts.evicted_refused = self.counts.evicted_refused.saturating_add(1);
    }

    /// A box of `owner` whose cgroup is slot `n`, under device policy `policy`, has ended and left it
    /// empty: it goes idle, the owner's last. Returns the slots its bounds put out, oldest first, for the
    /// caller to remove off create's path.
    pub fn give(&mut self, owner: &str, n: u64, policy: K) -> Vec<u64> {
        self.seq = self.seq.saturating_add(1);
        let idle = Idle {
            slot: n,
            policy,
            seq: self.seq,
        };
        self.idle
            .entry(owner.to_owned())
            .or_default()
            .push_back(idle);
        self.total = self.total.saturating_add(1);
        let mut out = Vec::new();
        // At most one past each bound after one give; the loops end on the bounds all the same.
        while let Some(list) = self.idle.get_mut(owner)
            && list.len() > self.per_owner.bound()
        {
            let Some(old) = list.pop_front() else { break };
            if list.is_empty() {
                self.idle.remove(owner);
            }
            self.total = self.total.saturating_sub(1);
            self.counts.evicted_owner_bound = self.counts.evicted_owner_bound.saturating_add(1);
            self.per_owner.evicted(owner);
            out.push(old.slot);
        }
        while self.total > self.per_node {
            let Some(oldest) = self
                .idle
                .iter()
                .filter_map(|(o, list)| list.front().map(|i| (i.seq, o.clone())))
                .min()
                .map(|(_, o)| o)
            else {
                break;
            };
            let Some(list) = self.idle.get_mut(&oldest) else {
                break;
            };
            let Some(old) = list.pop_front() else { break };
            if list.is_empty() {
                self.idle.remove(&oldest);
            }
            self.total = self.total.saturating_sub(1);
            self.counts.evicted_node_bound = self.counts.evicted_node_bound.saturating_add(1);
            out.push(old.slot);
        }
        self.counts.idle = self.total;
        out
    }

    /// Every idle slot, let go: the caller removes them (the node's shutdown, or a test's end).
    pub fn drain(&mut self) -> Vec<u64> {
        let out: Vec<u64> = std::mem::take(&mut self.idle)
            .into_values()
            .flatten()
            .map(|i| i.slot)
            .collect();
        self.counts.drained = self
            .counts
            .drained
            .saturating_add(u64::try_from(out.len()).unwrap_or(u64::MAX));
        self.total = 0;
        self.counts.idle = 0;
        out
    }

    /// How many idle cgroups `owner` has.
    pub fn idle_of(&self, owner: &str) -> usize {
        self.idle.get(owner).map_or(0, VecDeque::len)
    }

    pub fn counts(&self) -> Counts {
        self.counts.clone()
    }

    fn own(&mut self, reason: Refused) -> Take {
        self.count(reason);
        Take::Own(reason)
    }

    fn fresh(&mut self) -> Take {
        match self.next.checked_add(1) {
            Some(after) => {
                let n = self.next;
                self.next = after;
                self.counts.fresh = self.counts.fresh.saturating_add(1);
                Take::Fresh(n)
            }
            None => self.own(Refused::Slots),
        }
    }

    fn count(&mut self, reason: Refused) {
        let n = self.counts.refused.entry(reason).or_default();
        *n = n.saturating_add(1);
    }
}

/// What the node reads of an idle cgroup before it hands it on: files whose reads take no lock in Linux
/// v6.19 (kernel/cgroup/cgroup.c: `cgroup_events_show` :3815, `cgroup_stat_show` :3825 under RCU,
/// `cgroup_subtree_control_show` :3160, `cgroup_type_show` :3691, `cgroup_freeze_show` :4144;
/// mm/memcontrol.c: `memory_oom_group_show` :4564, `memory_current_read` :4229; research f61). The memory
/// files are `None` where the memory controller isn't handed down, so nothing is charged to the cgroup.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Files {
    pub events: String,
    pub stat: String,
    pub subtree_control: String,
    pub kind: String,
    pub freeze: String,
    pub oom_group: Option<String>,
    pub memory_current: Option<String>,
}

/// Whether an idle cgroup whose files say `f` may be handed to a box bounded to `memory_max`: nothing
/// runs in it or below it, it has no sub-cgroup live or dying, hands no controller down, is a plain
/// domain, isn't frozen, its `memory.oom.group` is 0 as Hex leaves it, and what is still charged to it
/// (research f27-f30) is at most `threshold`, which is under the box's memory bound. The first file that
/// says otherwise is the reason (f81).
pub fn check(f: &Files, memory_max: Option<u64>, threshold: u64) -> Result<(), Refused> {
    check_with(f, memory_max, threshold, None)
}

/// [`check`] for an idle cgroup an init was made ahead in (decision 8, "The p99.9 that stayed"): it is populated,
/// by that init alone (the shell holds the cgroup's `cgroup.procs` to its one pid), and `parked` bytes of what is
/// charged to it are the init's own, measured when it was made, which go with it to the box that claims it.
/// Everything else as [`check`] holds it.
pub fn check_parked(
    f: &Files,
    memory_max: Option<u64>,
    threshold: u64,
    parked: u64,
) -> Result<(), Refused> {
    check_with(f, memory_max, threshold, Some(parked))
}

fn check_with(
    f: &Files,
    memory_max: Option<u64>,
    threshold: u64,
    parked: Option<u64>,
) -> Result<(), Refused> {
    let number = |text: &str, key: &str| flat(text, key).ok_or(Refused::Unreadable);
    if number(&f.events, "populated")? != u64::from(parked.is_some()) {
        return Err(Refused::Populated);
    }
    if number(&f.events, "frozen")? != 0 {
        return Err(Refused::Frozen);
    }
    if number(&f.stat, "nr_descendants")? != 0 {
        return Err(Refused::Descendants);
    }
    if number(&f.stat, "nr_dying_descendants")? != 0 {
        return Err(Refused::Dying);
    }
    if !f.subtree_control.trim().is_empty() {
        return Err(Refused::SubtreeControl);
    }
    if f.kind.trim() != "domain" {
        return Err(Refused::NotDomain);
    }
    if f.freeze.trim() != "0" {
        return Err(if f.freeze.trim() == "1" {
            Refused::Frozen
        } else {
            Refused::Unreadable
        });
    }
    if let Some(group) = &f.oom_group
        && group.trim() != "0"
    {
        return Err(if group.trim() == "1" {
            Refused::OomGroup
        } else {
            Refused::Unreadable
        });
    }
    if let Some(current) = &f.memory_current {
        let charged: u64 = current.trim().parse().map_err(|_| Refused::Unreadable)?;
        let charged = charged.saturating_sub(parked.unwrap_or(0));
        if charged > threshold || memory_max.is_some_and(|max| threshold >= max) {
            return Err(Refused::Charged);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
