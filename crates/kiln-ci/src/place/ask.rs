//! What a run asks of placement, as its owner sends it in a probe, and what a runner measures of
//! itself to admit it (docs/reach.md §2, docs/needs.md §6 and E5).
//!
//! Amounts are integers in four dimensions, [`DIMS`]: milli-CPUs, bytes of memory, bytes of disk
//! and GPUs. Memory, disk and GPUs are never overcommitted; CPUs only as far as the person's
//! `overcommit_cpu` setting says (docs/needs.md §3's table).

use serde::{Deserialize, Serialize};

use crate::sched::needs::Needs;
use crate::sched::state::{Class, Cpus, PerMille, Resources};
use crate::sched::units::Ms;

/// The dimensions a run asks for and a runner offers: milli-CPUs, memory bytes, disk bytes, GPUs.
pub const DIMS: usize = 4;

/// An amount in each dimension, in [`DIMS`]' order.
pub type Amounts = [u64; DIMS];

/// A run as its owner asks for it to be placed: its id, what it needs, its class, whose it is
/// (fair share's tenant: its repository, then its actor, the agent or person it's for), and what
/// history knows of how long it takes.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Ask {
    /// Its id, unique among its owner's runs in one epoch of its owner: a runner keeps a run by its
    /// owner, the owner's epoch and this id, so a probe that comes again is the same run, and one it
    /// started is never started again (docs/needs.md §8).
    pub run: String,
    pub needs: Needs,
    pub class: Class,
    pub repo: String,
    pub actor: String,
    /// Its lane (a repository's workflow on a branch, W28-R66): fair share's third level, where
    /// `fair_by` goes that deep. Appended: optional, empty from an owner before it.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub lane: String,
    /// Its priority in its tenant's own order (W05-R09: higher first, aged as it waits). Appended:
    /// optional, 0 from an owner before it.
    #[serde(skip_serializing_if = "is_zero")]
    pub priority: u8,
    /// Its median and 90th-percentile durations, from history (docs/needs.md E6), when it has any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub median: Option<Ms>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub p90: Option<Ms>,
    /// Its own limit (`--timeout`): its runner stops it then.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<Ms>,
    /// The runners it may go to, by name, when its asker knows which can run it and the runners'
    /// facts don't say (an agent session's harness: kiln's server knows where each is installed;
    /// a machine named): a filter, never an order. Empty: any that suits. Appended: optional.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub among: Vec<String>,
    /// What kind of work it is, for the runner's health (sense's run key: `cmd:…`, `leg:…`,
    /// `session:<harness>`): a cause its own kind of work met there bars it (AUDIT L2). Appended:
    /// optional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

fn is_zero(p: &u8) -> bool {
    *p == 0
}

/// An owner as a runner hears it: its node's name and its epoch, one past the last it took, kept
/// on its disk, so every start of its process raises it (docs/needs.md §8). A run is named by its
/// owner, the owner's epoch and its id: a restarted owner's run with an id an earlier process used
/// is another run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Peer<'a> {
    pub name: &'a str,
    pub epoch: u64,
}

/// What the runner's disk is, as the runner's shell measures it: its kind, its measured speeds
/// and its error count. The core doesn't weigh them itself: they reach the owner with the
/// runner's word, and the shell's predictions of run time (the history of runs on disks of this
/// kind and speed, docs/needs.md E6) take them in (docs/needs.md §6).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DiskFacts {
    pub kind: DiskKind,
    /// Measured sequential read and write rates, bytes a second.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read_bps: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub write_bps: Option<u64>,
    /// I/O errors the disk reported since the runner started (SMART's, or the kernel's).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub errors: Option<u64>,
}

/// A disk's kind, as the system reports it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiskKind {
    #[default]
    Unknown,
    Nvme,
    Ssd,
    Hdd,
    Network,
}

/// What one placed run uses now, by its owner, the owner's epoch and its id (its process group's
/// CPUs, memory and the disk its scratch folder holds).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunUse {
    pub owner: String,
    pub epoch: u64,
    pub run: String,
    pub used: Resources,
}

/// What a runner measures of itself, as its latest sample has it. Every number in it is measured.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sample {
    /// The machine: its CPUs, memory, its work volume's size and GPUs.
    pub size: Resources,
    /// What's in use of it now, by everything: CPUs busy (their busy share, never a load average),
    /// memory in use, disk used (its size less what's free), GPUs in use.
    pub used: Resources,
    /// CI legs running on it: a run that takes the runner whole waits for none.
    pub ci_jobs: u32,
    /// What each placed run it does uses now.
    pub runs: Vec<RunUse>,
    pub disk: DiskFacts,
}

/// The amounts of `r`, in [`DIMS`]' order.
pub fn amounts(r: &Resources) -> Amounts {
    [r.cpu.get(), r.mem, r.disk, u64::from(r.gpu)]
}

/// The resources `a` says, in [`DIMS`]' order.
pub fn resources(a: &Amounts) -> Resources {
    let [cpu, mem, disk, gpu] = *a;
    Resources { cpu: Cpus::milli(cpu), mem, disk, gpu: u32::try_from(gpu).unwrap_or(u32::MAX) }
}

/// Whether `ask` fits in `free`, every dimension.
pub fn fits(ask: &Amounts, free: &Amounts) -> bool {
    ask.iter().zip(free).all(|(a, f)| a <= f)
}

/// `a` plus `b`, each dimension, saturating.
pub fn plus(a: &Amounts, b: &Amounts) -> Amounts {
    let mut out = *a;
    for (o, x) in out.iter_mut().zip(b) {
        *o = o.saturating_add(*x);
    }
    out
}

/// `a` less `b`, each dimension, down to nothing.
pub fn less(a: &Amounts, b: &Amounts) -> Amounts {
    let mut out = *a;
    for (o, x) in out.iter_mut().zip(b) {
        *o = o.saturating_sub(*x);
    }
    out
}

/// The least of `a` and `b`, each dimension.
pub fn least(a: &Amounts, b: &Amounts) -> Amounts {
    let mut out = *a;
    for (o, x) in out.iter_mut().zip(b) {
        *o = (*o).min(*x);
    }
    out
}

/// What a runner may admit against, each dimension: what kiln may place there (the person's
/// budget, `[runner] budget`, docs/needs.md §6; the machine when none is set), held to what's
/// left of the machine once what kiln didn't place is taken out. What kiln didn't place is what's
/// in use less what its own runs use within what they asked (`own`); a run using more than it
/// asked takes the rest from the room, as anything else running there does. CPUs are multiplied
/// by `overcommit_cpu` (thousandths, the person's setting); nothing else is overcommitted.
pub fn avail(s: &Sample, budget: Option<&Resources>, own: &Amounts, overcommit_cpu: PerMille) -> Amounts {
    let size = amounts(&s.size);
    let other = less(&amounts(&s.used), own);
    let mut out = less(&size, &other);
    if let Some(b) = budget {
        out = least(&out, &amounts(b));
    }
    let [cpu, mem, disk, gpu] = out;
    let cpu = u64::try_from(u128::from(cpu).saturating_mul(u128::from(overcommit_cpu.max(1000))) / 1000).unwrap_or(u64::MAX);
    [cpu, mem, disk, gpu]
}

/// A run's share of what the runner may admit, in millionths, at its largest dimension (DRF's
/// dominant share, docs/needs.md §5): what fair share charges it per millisecond. Nothing where the
/// runner has none of a dimension.
pub fn dominant_ppm(ask: &Amounts, cap: &Amounts) -> u64 {
    ask.iter().zip(cap).filter(|(_, c)| **c > 0).map(|(a, c)| u64::try_from(u128::from(*a).saturating_mul(1_000_000).div_ceil(u128::from(*c))).unwrap_or(u64::MAX)).max().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;

    fn res(cpus: u64, mem_gib: u64) -> Resources {
        Resources { cpu: Cpus::whole(cpus), mem: mem_gib * GIB, ..Resources::default() }
    }

    /// docs/needs.md §6: the room is the budget held to what's left of the machine once what kiln
    /// didn't place is out. A run using part of its ask leaves the rest to it; one using more than
    /// it asked takes the excess from the room, as anything else running there does.
    #[test]
    fn e5_room_is_the_budget_held_to_what_others_leave() {
        let s = Sample { size: Resources { disk: 100 * GIB, ..res(32, 64) }, used: Resources { disk: 40 * GIB, ..res(20, 50) }, ..Sample::default() };
        // Nothing of kiln's runs: 12 CPUs, 14 GiB and 60 GiB of disk.
        assert_eq!(avail(&s, None, &[0; DIMS], 1000), [12_000, 14 * GIB, 60 * GIB, 0]);
        // Its own runs use 2 CPUs and 4 GiB within their asks: those count as its own.
        assert_eq!(avail(&s, None, &amounts(&res(2, 4)), 1000), [14_000, 18 * GIB, 60 * GIB, 0]);
        // A budget of 8 CPUs and 32 GiB holds it there.
        let b = res(8, 32);
        assert_eq!(avail(&s, Some(&b), &amounts(&res(2, 4)), 1000), [8_000, 18 * GIB, 0, 0]);
        // CPUs overcommitted half as much again, by the person's setting.
        assert_eq!(avail(&s, None, &[0; DIMS], 1500), [18_000, 14 * GIB, 60 * GIB, 0]);
        assert_eq!(dominant_ppm(&amounts(&res(8, 4)), &amounts(&res(16, 64))), 500_000);
        assert!(fits(&[1, 2, 3, 4], &[1, 2, 3, 4]) && !fits(&[1, 2, 3, 5], &[1, 2, 3, 4]));
    }
}
