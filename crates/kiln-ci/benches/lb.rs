//! kiln's placement kernel under the load-balancer bench's workload (bench-spec.md; docs/lb.md
//! "Measured"; W05-R26): the same phases, generator and timing as every competitor's driver
//! (ci/bench/common.h in the bench worktree), so the numbers compare.
//!
//! N runners of C = 8 CPUs, one owner, one signature with every runner in it. F' = max(1,
//! floor(rho N C)) requests in flight. A step draws a slot uniformly (SplitMix64 from seed 0,
//! Lemire's reduction), releases the request in it (the run ends at its runner and the runner's
//! word reaches the owner), then picks a new one into that slot (the owner decides, the runner
//! admits, the owner takes its word): the whole critical path. A run asks one CPU at d = 1; at
//! d = 2 an eighth of the runner's memory as well, at d = 4 an eighth of its disk and one of its 8
//! GPUs too (each dimension as tight as the CPU).
//!
//! Phases, as common.h runs them: the null (the same phases with no-op calls), the fill (F'
//! picks), W warm-up steps, M timed blocks of B steps (ns_step: the median block over B), M x B
//! untimed steps for quality (oversubscription, refusals, the CV of in-flight counts every N
//! steps), and release-only blocks (ns_release). The release and the pick are called through
//! function pointers the compiler can't see through, the null's too, as common.h calls them.
//! Release-only steps run on copies of the state (an implementation that admits exactly can't be
//! topped up past its capacity, as common.h tops the competitors up): each copy's runs released
//! in a timed stretch, stretches summed to B releases a block.
//!
//! `--batch K` runs the batched path instead: each call releases K requests (drawn without
//! repeats in the batch) and picks K, kiln's `ask_many` deciding them in one pass.
//!
//! `--measured` has every runner measure its per-core speed and slowness first, differing by
//! runner as a mixed fleet's do (docs/lb.md "Rank"), and asks stay plain: the cost of a decision
//! on a measured fleet, where narrow asks rank by speed and slowness.
//!
//! `cargo bench -p kiln-ci --bench lb -- --json --n N --rho R --d D --runs K [--b B] [--w W]
//! [--run R] [--batch K]` prints one JSON line a run; without `--json`, a table.

use std::hint::black_box;
use std::io::Write as _;
use std::process::Command;
use std::time::Instant;

use kiln_ci::lb::{Amounts, Brief, CPU, Handle, Link, Out, Owner, OwnerId, Runner, RunnerId, RunnerOut, SigId, Start, Terms, ToOwner, ToRunner, Warm};

/// Requests each backend takes at once (bench-spec.md: kiln's 8 CPUs, one a run).
const C: u32 = 8;
/// The generator's seed: the sequence every implementation sees (common.h's LB_SEED).
const SEED: u64 = 0;
/// A block must last this many counter ticks (bench-spec.md: Timing).
const MIN_TICKS: u64 = 1000;
/// The fewest blocks a measurement takes: the smallest count whose median survives one
/// disturbed block (common.h's LB_MIN_BLOCKS).
const MIN_BLOCKS: usize = 3;
/// The most blocks a measurement keeps (common.h's array of 64).
const MAX_BLOCKS: usize = 64;
/// Memory and disk per runner, in the units a need asks them: the workload gives each request an
/// eighth of every dimension it asks, so only these ratios matter.
const MEM: u64 = 32 << 30;
const DISK: u64 = 400 << 30;

/// SplitMix64, common.h's `lb_next`.
#[derive(Clone)]
struct Rng(u64);

impl Rng {
    #[inline(always)]
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
}

/// Uniform in [0, n): the high word of x * n (common.h's `lb_uniform`).
#[inline(always)]
fn uniform(x: u64, n: u32) -> u32 {
    ((u128::from(x) * u128::from(n)) >> 64) as u32
}

/// An implementation as the harness calls it.
#[derive(Clone)]
struct Impl<S: Clone> {
    state: S,
    release: fn(&mut S, u32),
    pick: fn(&mut S, u32, u64) -> i32,
    many_release: fn(&mut S, &[u32]),
    many_pick: fn(&mut S, &[u32], &[u64], &mut [i32]),
    /// The backend of the request in each slot (-1: none), as the harness keeps it.
    sb: Vec<i32>,
}

// ---- the null: the harness's bookkeeping alone ----

#[inline(never)]
fn null_release(_: &mut (), _: u32) {
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
}

#[inline(never)]
fn null_pick(_: &mut (), _: u32, _: u64) -> i32 {
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
    0
}

#[inline(never)]
fn null_many_release(_: &mut (), _: &[u32]) {
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
}

#[inline(never)]
fn null_many_pick(_: &mut (), _: &[u32], _: &[u64], out: &mut [i32]) {
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
    for o in out.iter_mut() {
        *o = 0;
    }
}

// ---- the floor: what any balancer that keeps a count per backend does at the least ----

/// A count per backend, decremented on release; round robin on pick (no load read, no
/// admission): the hardware's cost of keeping per-backend state at all, the floor under every
/// implementation's step, competitors' and kiln's.
#[derive(Clone)]
struct Floor {
    counts: Vec<u32>,
    slot: Vec<u32>,
    next: u32,
    n: u32,
}

fn floor_release(f: &mut Floor, s: u32) {
    if let Some(&b) = f.slot.get(s as usize)
        && let Some(c) = f.counts.get_mut(b as usize)
    {
        *c = c.wrapping_sub(1);
    }
}

fn floor_pick(f: &mut Floor, s: u32, _key: u64) -> i32 {
    let b = f.next;
    f.next = if b + 1 == f.n { 0 } else { b + 1 };
    if let Some(c) = f.counts.get_mut(b as usize) {
        *c += 1;
    }
    if let Some(x) = f.slot.get_mut(s as usize) {
        *x = b;
    }
    b as i32
}

fn floor_many_release(f: &mut Floor, slots: &[u32]) {
    for &s in slots {
        floor_release(f, s);
    }
}

fn floor_many_pick(f: &mut Floor, slots: &[u32], keys: &[u64], out: &mut [i32]) {
    for ((&s, &k), o) in slots.iter().zip(keys).zip(out.iter_mut()) {
        *o = floor_pick(f, s, k);
    }
}

fn floor_impl(n: u32, slots: usize) -> Impl<Floor> {
    let state = Floor { counts: vec![0; n as usize], slot: vec![0; slots], next: 0, n };
    Impl { state, release: floor_release, pick: floor_pick, many_release: floor_many_release, many_pick: floor_many_pick, sb: vec![-1; slots] }
}

// ---- kiln's parts, apart: the owner with its runners' words made up, the runners without an owner ----

/// The owner alone: each word it hears is made up from a count of each runner's free CPU, as
/// the runner would say it (what the owner's side of a step costs).
#[derive(Clone)]
struct OwnerPart {
    owner: Owner,
    sig: SigId,
    need: Amounts,
    free: Vec<u64>,
    slots: Vec<(u32, Handle)>,
    out: Out,
    /// The need's CPU when it asks nothing else (d = 1): the fast entries' argument.
    cpu: Option<u32>,
}

fn word_of(cpu: u64) -> kiln_ci::lb::Word {
    kiln_ci::lb::Word { epoch: 1, whole: true, cpu, ..Default::default() }
}

fn owner_part_release(k: &mut OwnerPart, s: u32) {
    let Some(&(r, run)) = k.slots.get(s as usize) else { return };
    let Some(f) = k.free.get_mut(r as usize) else { return };
    *f += k.need[0];
    let w = word_of(*f);
    if !k.owner.ended_fast(r, run, w.brief()) {
        k.owner.hear(ToOwner::Ended { runner: RunnerId(r), run, word: w }, &mut k.out);
    }
}

fn owner_part_pick(k: &mut OwnerPart, s: u32, _key: u64) -> i32 {
    let at = Handle { slot: s, generation: 1 };
    let (to, run) = match k.cpu.and_then(|c| k.owner.ask_fast(k.sig, c)) {
        Some((r, run, _)) => (r, run),
        None => {
            let Ok(a) = k.owner.ask(k.sig, k.need, &mut k.out) else { return -1 };
            let Some((to, _)) = a.send else { return -1 };
            (to.0, a.run)
        }
    };
    let Some(f) = k.free.get_mut(to as usize) else { return -1 };
    *f -= k.need[0];
    let w = word_of(*f);
    if !k.owner.started_fast(to, run, at, w.brief()) {
        k.owner.hear(ToOwner::Started { runner: RunnerId(to), run, at, word: w }, &mut k.out);
    }
    if let Some(x) = k.slots.get_mut(s as usize) {
        *x = (to, run);
    }
    to as i32
}

/// The runners alone: round robin over them, each admitting or declining against its ledger
/// (what the runners' side of a step costs).
#[derive(Clone)]
struct RunnerPart {
    runners: Vec<Runner>,
    need: Amounts,
    next: u32,
    slots: Vec<(u32, Handle)>,
    rout: RunnerOut,
    cpu: Option<u32>,
}

fn runner_part_release(k: &mut RunnerPart, s: u32) {
    let Some(&(r, at)) = k.slots.get(s as usize) else { return };
    if let Some(x) = k.runners.get_mut(r as usize)
        && x.end_fast(at).is_none()
    {
        black_box(x.end(at, &mut k.rout));
    }
}

fn runner_part_pick(k: &mut RunnerPart, s: u32, _key: u64) -> i32 {
    let n = k.runners.len() as u32;
    for _ in 0..n {
        let r = k.next;
        k.next = if r + 1 == n { 0 } else { r + 1 };
        let Some(x) = k.runners.get_mut(r as usize) else { return -1 };
        if let Some((at, _)) = k.cpu.and_then(|c| x.start_fast(OwnerId(0), Handle::NONE, 1, c)) {
            if let Some(y) = k.slots.get_mut(s as usize) {
                *y = (r, at);
            }
            return r as i32;
        }
        if let ToOwner::Started { at, .. } = x.start(Start { owner: OwnerId(0), run: Handle::NONE, epoch: 1, need: k.need }) {
            if let Some(y) = k.slots.get_mut(s as usize) {
                *y = (r, at);
            }
            return r as i32;
        }
    }
    -1
}

fn parts_many_release<S>(st: &mut S, slots: &[u32], one: fn(&mut S, u32)) {
    for &s in slots {
        one(st, s);
    }
}

fn owner_part_impl(n: u32, d: u32, slots: usize) -> Impl<OwnerPart> {
    let k = Kiln::new(n, d, slots);
    let free = vec![u64::from(C) * CPU; n as usize];
    let state = OwnerPart { owner: k.owner, sig: k.sig, need: k.need, free, slots: vec![(0, Handle::NONE); slots], out: Out::default(), cpu: k.cpu };
    Impl {
        state,
        release: owner_part_release,
        pick: owner_part_pick,
        many_release: |st, sl| parts_many_release(st, sl, owner_part_release),
        many_pick: |st, sl, keys, out| {
            for ((&s, &key), o) in sl.iter().zip(keys).zip(out.iter_mut()) {
                *o = owner_part_pick(st, s, key);
            }
        },
        sb: vec![-1; slots],
    }
}

fn runner_part_impl(n: u32, d: u32, slots: usize) -> Impl<RunnerPart> {
    let k = Kiln::new(n, d, slots);
    let state = RunnerPart { runners: k.runners, need: k.need, next: 0, slots: vec![(0, Handle::NONE); slots], rout: RunnerOut::new(), cpu: k.cpu };
    Impl {
        state,
        release: runner_part_release,
        pick: runner_part_pick,
        many_release: |st, sl| parts_many_release(st, sl, runner_part_release),
        many_pick: |st, sl, keys, out| {
            for ((&s, &key), o) in sl.iter().zip(keys).zip(out.iter_mut()) {
                *o = runner_part_pick(st, s, key);
            }
        },
        sb: vec![-1; slots],
    }
}

// ---- the design's floor: what kiln's step must do at the least ----

/// One owner view update and one runner ledger update each way, and a run record each way, with
/// no index, no guards, no generations and no free list (records by the slot, so nothing
/// allocates): the work the protocol can't do without. The choice is the runner a run just
/// ended on (the step kiln's own pick makes, without looking), else the next in turn.
#[derive(Clone)]
struct Min {
    view: Vec<u32>,
    ledger: Vec<u32>,
    owner_runs: Vec<[u32; 4]>,
    runner_runs: Vec<[u32; 4]>,
    slots: Vec<u32>,
    last: u32,
    next: u32,
    n: u32,
}

fn min_release(k: &mut Min, s: u32) {
    let Some(&r) = k.slots.get(s as usize) else { return };
    let Some(&[cpu_r, ..]) = k.runner_runs.get(s as usize) else { return };
    let Some(&[cpu_o, ..]) = k.owner_runs.get(s as usize) else { return };
    if let Some(l) = k.ledger.get_mut(r as usize) {
        *l += cpu_r;
    }
    if let Some(v) = k.view.get_mut(r as usize) {
        *v += cpu_o;
    }
    k.last = r;
}

fn min_pick(k: &mut Min, s: u32, _key: u64) -> i32 {
    let r = if k.last != u32::MAX {
        k.last
    } else {
        k.next = if k.next + 1 >= k.n { 0 } else { k.next + 1 };
        k.next
    };
    k.last = u32::MAX;
    let c = CPU as u32;
    if let Some(v) = k.view.get_mut(r as usize) {
        *v -= c;
    }
    if let Some(x) = k.owner_runs.get_mut(s as usize) {
        *x = [c, r, s, 1];
    }
    if let Some(l) = k.ledger.get_mut(r as usize) {
        *l -= c;
    }
    if let Some(x) = k.runner_runs.get_mut(s as usize) {
        *x = [c, r, s, 1];
    }
    if let Some(x) = k.slots.get_mut(s as usize) {
        *x = r;
    }
    r as i32
}

fn min_impl(n: u32, slots: usize) -> Impl<Min> {
    let free = (C as u64 * CPU) as u32;
    let state = Min { view: vec![free; n as usize], ledger: vec![free; n as usize], owner_runs: vec![[0; 4]; slots], runner_runs: vec![[0; 4]; slots], slots: vec![0; slots], last: u32::MAX, next: 0, n };
    Impl {
        state,
        release: min_release,
        pick: min_pick,
        many_release: |st, sl| parts_many_release(st, sl, min_release),
        many_pick: |st, sl, keys, out| {
            for ((&s, &key), o) in sl.iter().zip(keys).zip(out.iter_mut()) {
                *o = min_pick(st, s, key);
            }
        },
        sb: vec![-1; slots],
    }
}

// ---- kiln ----

/// Where a slot's run is: its runner, its handle there, and its own handle (what a Started word
/// told the shell, and an Ended word carries back to the owner: the harness is the wire, and
/// the owner's look-up of a run's record starts from what the word says, not from a load out of
/// the runner's memory, as on the two machines they are on).
#[derive(Clone, Copy)]
struct Slot {
    runner: u32,
    at: Handle,
    run: Handle,
}

/// The part of a slot's record a release reads last: its run's handle at its runner, and the
/// run's generation at the owner. Sixteen bytes, four to a line, none across two.
#[derive(Clone, Copy)]
#[repr(align(16))]
struct Rest {
    at: Handle,
    generation: u32,
}

#[derive(Clone)]
struct Kiln {
    owner: Owner,
    runners: Vec<Runner>,
    sig: SigId,
    need: Amounts,
    /// Each slot's record ([`Slot`]), in three parts by how soon a release reads them (W05-R26).
    /// Its runner, and its run's slot at the owner, say where the runner's and the owner's own
    /// records are: each is kept dense (sixteen slots to a 64-byte line, so a line is read again
    /// four times sooner than a 20-byte record's and is likelier still near), and those records
    /// are on their way while the rest of the slot's record is still being read.
    slot_runner: Vec<u32>,
    slot_run: Vec<u32>,
    slot_rest: Vec<Rest>,
    out: Out,
    rout: RunnerOut,
    starts: Vec<(RunnerId, Start)>,
    waits: Vec<Handle>,
    /// A batch's scratch, kept to reuse: the slots' records, the words their runs' ends said, and
    /// the picks' answers.
    stage: Vec<Slot>,
    words: Vec<(Slot, Brief)>,
    admits: Vec<(u32, u32, Handle, u32, Handle, Brief)>,
    /// Asks that found no room now: they wait at runners (none below rho = 1).
    waited: u64,
    /// The need's CPU when it asks nothing else (d = 1): the fast entries' argument.
    cpu: Option<u32>,
}

impl Kiln {
    fn new(n: u32, d: u32, slots: usize) -> Kiln {
        let budget = [u64::from(C) * CPU, if d >= 2 { MEM } else { 0 }, if d >= 4 { DISK } else { 0 }, if d >= 4 { u64::from(C) } else { 0 }];
        let need = [CPU, budget[1] / u64::from(C), budget[2] / u64::from(C), budget[3] / u64::from(C)];
        let mut owner = Owner::new(OwnerId(0));
        owner.reserve(slots);
        let mut out = Out::default();
        let mut runners = Vec::with_capacity(n as usize);
        for i in 0..n {
            let mut r = Runner::new(RunnerId(i), budget, 1);
            r.reserve(C as usize);
            r.watch(OwnerId(0));
            owner.join(RunnerId(i), budget, r.word(), &mut out);
            runners.push(r);
        }
        let sig = owner.sig();
        for i in 0..n {
            owner.add(sig, RunnerId(i), &mut out);
        }
        let cpu = if d == 1 { u32::try_from(need[0]).ok() } else { None };
        let rest = Rest { at: Handle::NONE, generation: 0 };
        Kiln { owner, runners, sig, need, slot_runner: vec![u32::MAX; slots], slot_run: vec![u32::MAX; slots], slot_rest: vec![rest; slots], out, rout: RunnerOut::new(), starts: Vec::new(), waits: Vec::new(), stage: Vec::new(), words: Vec::new(), admits: Vec::new(), waited: 0, cpu }
    }

    /// Carries whatever else was said, until nothing is (nothing is, in this workload).
    #[cold]
    fn deliver(&mut self) {
        loop {
            let to = std::mem::take(&mut self.out.to);
            let said = std::mem::take(&mut self.rout);
            if to.is_empty() && said.is_empty() {
                self.out.told.clear();
                return;
            }
            for (r, m) in to {
                if let Some(reply) = self.runners.get_mut(r.0 as usize).and_then(|x| x.hear(m, &mut self.rout)) {
                    self.owner.hear(reply, &mut self.out);
                }
            }
            for (_, m) in said {
                self.owner.hear(m, &mut self.out);
            }
        }
    }

    /// Slot `s`'s whole record.
    #[inline(always)]
    fn slot(&self, s: u32) -> Option<Slot> {
        let i = s as usize;
        let (runner, run, rest) = (*self.slot_runner.get(i)?, *self.slot_run.get(i)?, self.slot_rest.get(i)?);
        Some(Slot { runner, at: rest.at, run: Handle { slot: run, generation: rest.generation } })
    }

    /// Slot `s` holds this run now.
    #[inline(always)]
    fn set_slot(&mut self, s: u32, slot: Slot) {
        let i = s as usize;
        if let (Some(runner), Some(run), Some(rest)) = (self.slot_runner.get_mut(i), self.slot_run.get_mut(i), self.slot_rest.get_mut(i)) {
            *runner = slot.runner;
            *run = slot.run.slot;
            *rest = Rest { at: slot.at, generation: slot.run.generation };
        }
    }

    /// A release through the kernel's fast entries (the run ends at its runner, its word reaches
    /// the owner); whatever they decline takes the general path whole.
    #[inline(always)]
    fn release_one(&mut self, s: u32) {
        let Some(slot) = self.slot(s) else { return };
        let Some(x) = self.runners.get_mut(slot.runner as usize) else { return };
        match x.end_fast(slot.at) {
            Some((_, b)) => {
                if !self.owner.ended_fast(slot.runner, slot.run, b) {
                    self.ended_cold(slot.runner, slot.run);
                }
            }
            None => self.release_cold_at(s),
        }
    }

    /// W05-R26: materialize the full wire record only when the runner declines the fast path.
    /// Passing its slot number here keeps the cold by-value argument off the warmed stack.
    #[cold]
    #[inline(never)]
    fn release_cold_at(&mut self, s: u32) {
        if let Some(slot) = self.slot(s) {
            self.release_cold(slot);
        }
    }

    /// The runner said the run ended but the owner's fast path declined its word: the general
    /// path, with the word whole.
    #[cold]
    #[inline(never)]
    fn ended_cold(&mut self, r: u32, run: Handle) {
        let Some(word) = self.runners.get(r as usize).map(Runner::word) else { return };
        self.owner.hear(ToOwner::Ended { runner: RunnerId(r), run, word }, &mut self.out);
        if !(self.out.to.is_empty() && self.rout.is_empty()) {
            self.deliver();
        }
    }

    /// A release the runner's fast path declined: the general path.
    #[cold]
    #[inline(never)]
    fn release_cold(&mut self, slot: Slot) {
        if let Some(r) = self.runners.get_mut(slot.runner as usize)
            && let Some((_, m)) = r.end(slot.at, &mut self.rout)
        {
            self.owner.hear(m, &mut self.out);
        }
        if !(self.out.to.is_empty() && self.rout.is_empty()) {
            self.deliver();
        }
    }

    /// The chosen runner admits what the owner sent and the owner takes its word, through the
    /// kernel's fast entries. Where it started, or -1.
    #[inline(always)]
    fn admit_fast(&mut self, s: u32, r: u32, run: Handle, epoch: u32, cpu: u32) -> i32 {
        let Some(x) = self.runners.get_mut(r as usize) else { return -1 };
        let Some((at, b)) = x.start_fast(OwnerId(0), run, epoch, cpu) else { return self.admit_cold(s, r, run, epoch) };
        if !self.owner.started_fast(r, run, at, b) {
            return self.started_cold(s, r, run, at);
        }
        self.set_slot(s, Slot { runner: r, at, run });
        r as i32
    }

    /// The runner's fast path declined a start: the general path, as `hear` carries it.
    #[cold]
    #[inline(never)]
    fn admit_cold(&mut self, s: u32, r: u32, run: Handle, epoch: u32) -> i32 {
        let start = Start { owner: OwnerId(0), run, epoch, need: self.need };
        self.admit(s, RunnerId(r), ToRunner::Start(start))
    }

    /// The runner started it but the owner's fast path declined its word: the general path, with
    /// the word whole, and the slot's record, so the pick that comes here has nothing left to do
    /// (W05-R26: its fast path then calls nothing it has to come back from).
    #[cold]
    #[inline(never)]
    fn started_cold(&mut self, s: u32, r: u32, run: Handle, at: Handle) -> i32 {
        let Some(word) = self.runners.get(r as usize).map(Runner::word) else { return -1 };
        self.owner.hear(ToOwner::Started { runner: RunnerId(r), run, at, word }, &mut self.out);
        if !(self.out.to.is_empty() && self.rout.is_empty()) {
            self.deliver();
        }
        self.set_slot(s, Slot { runner: r, at, run });
        r as i32
    }

    /// The runner admits what the owner sent; the owner takes its word. Where it started, or -1.
    #[inline(always)]
    fn admit(&mut self, s: u32, to: RunnerId, m: ToRunner) -> i32 {
        let Some(reply) = self.runners.get_mut(to.0 as usize).and_then(|r| r.hear(m, &mut self.rout)) else { return -1 };
        let (at, run) = if let ToOwner::Started { at, run, .. } = reply { (at, run) } else { (Handle::NONE, Handle::NONE) };
        self.owner.hear(reply, &mut self.out);
        if at == Handle::NONE {
            // Declined (never with one owner deducting at once): placed again, carried through.
            self.deliver();
            return -1;
        }
        self.set_slot(s, Slot { runner: to.0, at, run });
        to.0 as i32
    }
}

fn kiln_release(k: &mut Kiln, s: u32) {
    k.release_one(s);
}

fn kiln_pick(k: &mut Kiln, s: u32, _key: u64) -> i32 {
    let Some(cpu) = k.cpu else { return kiln_pick_cold(k, s) };
    // The pick that goes back to the runner a run just ended on calls nothing it comes back
    // from; every other pick is one call made last, so this one keeps no frame (W05-R26).
    match k.owner.ask_alone(k.sig, cpu) {
        Some((r, run, epoch)) => k.admit_fast(s, r, run, epoch, cpu),
        None => kiln_pick_rest(k, s, cpu),
    }
}

/// A pick of CPU alone that didn't go back to the runner a run just ended on: the owner's other
/// fast picks, then the general path.
#[inline(never)]
fn kiln_pick_rest(k: &mut Kiln, s: u32, cpu: u32) -> i32 {
    match k.owner.ask_rest(k.sig, cpu) {
        Some((r, run, epoch)) => k.admit_fast(s, r, run, epoch, cpu),
        None => kiln_pick_cold(k, s),
    }
}

/// A pick the owner's fast path declined (d past 1, a walk, nothing to pick), or that the fast
/// path can't take: the general path.
#[cold]
#[inline(never)]
fn kiln_pick_cold(k: &mut Kiln, s: u32) -> i32 {
    let Ok(a) = k.owner.ask(k.sig, k.need, &mut k.out) else { return -1 };
    match a.send {
        Some((to, m)) => k.admit(s, to, m),
        None => {
            k.waited += 1;
            k.deliver();
            -1
        }
    }
}

/// W05-R26: fresh-state construction and first-use costs, outside the warmed throughput grid.
/// This makes no claim to cold OS page caches: the coordinator never flushes shared caches.
fn cold_sample(n: u32, d: u32) -> std::result::Result<(u128, u128, u128, u128, u128), &'static str> {
    let t = Instant::now();
    let mut k = Kiln::new(n, d, 1);
    let build = t.elapsed().as_nanos();
    let t = Instant::now();
    if kiln_pick(&mut k, 0, SEED) < 0 { return Err("first cold placement refused"); }
    let first = t.elapsed().as_nanos();
    kiln_release(&mut k, 0);
    let t = Instant::now();
    // The existing mixed-fleet fixture's first speed: three quarters of reference, not a
    // tuning threshold. Its first non-reference CPU word activates measured-core ranking.
    let runner = k.runners.first_mut().ok_or("no cold runner")?;
    runner.measure(768, 1024);
    // The same word also reports byte rates, so the cold comparison includes the later
    // first I/O ask and exposes the cost deliberately deferred to that ask.
    runner.measure_rates(1 << 20, 1 << 20);
    runner.drain(&mut k.rout);
    k.deliver();
    let measured = t.elapsed().as_nanos();
    let t = Instant::now();
    if kiln_pick(&mut k, 0, SEED) < 0 { return Err("first measured placement refused"); }
    let first_measured = t.elapsed().as_nanos();
    kiln_release(&mut k, 0);
    let t = Instant::now();
    let a = k.owner.ask_with(k.sig, k.need, &Terms { cpu_ns: 1, io_bytes: 1, ..Terms::default() }, &mut k.out).map_err(|_| "first byte ask refused")?;
    let (to, msg) = a.send.ok_or("first byte ask did not start")?;
    if k.admit(0, to, msg) < 0 { return Err("first byte placement refused"); }
    let first_byte = t.elapsed().as_nanos();
    Ok((build, first, measured, first_measured, first_byte))
}

fn kiln_many_release(k: &mut Kiln, slots: &[u32]) {
    // Every slot's record first, then every run's end at its runner, then every word at the owner:
    // three passes of independent steps, whose cache misses overlap.
    let (mut stage, mut words) = (std::mem::take(&mut k.stage), std::mem::take(&mut k.words));
    stage.clear();
    stage.extend(slots.iter().filter_map(|&s| k.slot(s)));
    words.clear();
    for slot in &stage {
        let Some(x) = k.runners.get_mut(slot.runner as usize) else { continue };
        match x.end_fast(slot.at) {
            Some((_, b)) => words.push((*slot, b)),
            None => k.release_cold(*slot),
        }
    }
    for (slot, b) in &words {
        if !k.owner.ended_batch(slot.runner, slot.run, *b) {
            k.ended_cold(slot.runner, slot.run);
        }
    }
    k.stage = stage;
    k.words = words;
}

fn kiln_many_pick(k: &mut Kiln, slots: &[u32], _keys: &[u64], out: &mut [i32]) {
    let (mut starts, mut waits) = (std::mem::take(&mut k.starts), std::mem::take(&mut k.waits));
    let mut admits = std::mem::take(&mut k.admits);
    starts.clear();
    waits.clear();
    admits.clear();
    if k.owner.ask_many(k.sig, k.need, slots.len(), &mut starts, &mut waits, &mut k.out).is_err() {
        for o in out.iter_mut() {
            *o = -1;
        }
    }
    let cpu = k.cpu;
    // Each chosen runner admits (independent runners: their misses overlap), then the owner takes
    // their words.
    let mut i = 0usize;
    for &(to, m) in &starts {
        let (Some(&s), Some(o)) = (slots.get(i), out.get_mut(i)) else { break };
        match cpu.and_then(|c| k.runners.get_mut(to.0 as usize).and_then(|x| x.start_fast(m.owner, m.run, m.epoch, c))) {
            Some((at, b)) => admits.push((i as u32, to.0, m.run, s, at, b)),
            None => *o = k.admit(s, to, ToRunner::Start(m)),
        }
        i += 1;
    }
    for &(ix, r, run, s, at, b) in &admits {
        if !k.owner.started_fast(r, run, at, b) {
            k.started_cold(s, r, run, at);
        } else {
            k.set_slot(s, Slot { runner: r, at, run });
        }
        if let Some(o) = out.get_mut(ix as usize) {
            *o = r as i32;
        }
    }
    k.waited += waits.len() as u64;
    for o in out.iter_mut().skip(i) {
        *o = -1;
    }
    k.starts = starts;
    k.waits = waits;
    k.admits = admits;
    if !k.out.to.is_empty() {
        k.deliver();
    }
}

// ---- the phases, as common.h runs them ----

struct Params {
    n: u32,
    f: u32,
    b: u64,
    w: u64,
    m: usize,
    m_r: usize,
    batch: usize,
}

fn fill<S: Clone>(rng: &mut Rng, im: &mut Impl<S>, from: u32, to: u32) {
    let pick = black_box(im.pick);
    for s in from..to {
        let key = rng.next();
        let b = pick(&mut im.state, s, key);
        if let Some(x) = im.sb.get_mut(s as usize) {
            *x = b;
        }
    }
}

#[inline(never)]
fn steps<S: Clone>(rng: &mut Rng, im: &mut Impl<S>, f: u32, n: u64) {
    let release = black_box(im.release);
    let pick = black_box(im.pick);
    let state = &mut im.state;
    let sb = &mut im.sb;
    for _ in 0..n {
        let s = uniform(rng.next(), f);
        let key = rng.next();
        let Some(x) = sb.get_mut(s as usize) else { continue };
        if *x >= 0 {
            release(state, s);
        }
        *x = pick(state, s, key);
    }
}

/// Scratch for the batched steps: the batch's slots, keys and picks, and a stamp per slot (the
/// batch it was last drawn in) so a batch never draws a slot twice.
#[derive(Clone, Default)]
struct Batch {
    slots: Vec<u32>,
    keys: Vec<u64>,
    picked: Vec<i32>,
    /// W05-R26: only the selected slots that still hold a run, reused between batches.
    released: Vec<u32>,
    stamp: Vec<u64>,
    id: u64,
}

#[inline(never)]
fn steps_many<S: Clone>(rng: &mut Rng, im: &mut Impl<S>, f: u32, n: u64, bt: &mut Batch, k: usize) {
    let release = black_box(im.many_release);
    let pick = black_box(im.many_pick);
    let k = k.min(f as usize).max(1);
    let mut done = 0u64;
    while done < n {
        let take = (n - done).min(k as u64) as usize;
        bt.id += 1;
        bt.slots.clear();
        bt.keys.clear();
        while bt.slots.len() < take {
            let s = uniform(rng.next(), f);
            let key = rng.next();
            let Some(st) = bt.stamp.get_mut(s as usize) else { continue };
            if *st == bt.id {
                continue;
            }
            *st = bt.id;
            bt.slots.push(s);
            bt.keys.push(key);
        }
        // Release those that hold a request.
        bt.released.clear();
        bt.released.extend(bt.slots.iter().copied().filter(|&s| im.sb.get(s as usize).is_some_and(|b| *b >= 0)));
        release(&mut im.state, &bt.released);
        bt.picked.clear();
        bt.picked.resize(take, -1);
        pick(&mut im.state, &bt.slots, &bt.keys, &mut bt.picked);
        for (&s, &b) in bt.slots.iter().zip(&bt.picked) {
            if let Some(x) = im.sb.get_mut(s as usize) {
                *x = b;
            }
        }
        done += take as u64;
    }
}

/// Untimed steps that count: oversubscription, refusals, and the CV of in-flight counts sampled
/// every N steps (common.h's `lb_quality_steps`).
#[derive(Default, Clone, Copy)]
struct Quality {
    picks: u64,
    oversub: u64,
    refusals: u64,
    samples: u64,
    cv_sum: f64,
}

fn cv(counts: &[u32]) -> f64 {
    let (mut sum, mut sq) = (0.0f64, 0.0f64);
    for &c in counts {
        let c = f64::from(c);
        sum += c;
        sq += c * c;
    }
    if sum <= 0.0 {
        return 0.0;
    }
    let n = counts.len() as f64;
    let mean = sum / n;
    let var = sq / n - mean * mean;
    if var > 0.0 { var.sqrt() / mean } else { 0.0 }
}

fn quality_steps<S: Clone>(rng: &mut Rng, im: &mut Impl<S>, n: u32, f: u32, steps: u64, series: Option<&mut Vec<f64>>) -> Quality {
    let mut q = Quality::default();
    let mut counts = vec![0u32; n as usize];
    for &b in im.sb.iter().take(f as usize) {
        if let Some(c) = usize::try_from(b).ok().and_then(|b| counts.get_mut(b)) {
            *c += 1;
        }
    }
    let mut series = series;
    for i in 0..steps {
        let s = uniform(rng.next(), f);
        let key = rng.next();
        let held = im.sb.get(s as usize).copied().unwrap_or(-1);
        if held >= 0 {
            (im.release)(&mut im.state, s);
            if let Some(c) = counts.get_mut(held as usize) {
                *c = c.saturating_sub(1);
            }
        }
        let b = (im.pick)(&mut im.state, s, key);
        q.picks += 1;
        match usize::try_from(b).ok().filter(|b| *b < n as usize) {
            Some(bi) => {
                if let Some(c) = counts.get_mut(bi) {
                    if *c >= C {
                        q.oversub += 1;
                    }
                    *c += 1;
                }
            }
            None => q.refusals += 1,
        }
        if let Some(x) = im.sb.get_mut(s as usize) {
            *x = if b >= 0 && (b as u32) < n { b } else { -1 };
        }
        if (i + 1) % u64::from(n) == 0 {
            let v = cv(&counts);
            q.cv_sum += v;
            q.samples += 1;
            if let Some(sr) = series.as_deref_mut() {
                sr.push(v);
            }
        }
    }
    q
}

/// The counter's tick: the smallest step the monotonic clock is seen to take.
fn tick_ns() -> f64 {
    let mut best = u128::MAX;
    for _ in 0..20_000 {
        let a = Instant::now();
        let mut b = Instant::now();
        while b == a {
            b = Instant::now();
        }
        best = best.min(b.duration_since(a).as_nanos());
    }
    best as f64
}

fn median(v: &mut [f64]) -> f64 {
    v.sort_by(f64::total_cmp);
    let n = v.len();
    if n == 0 {
        return f64::NAN;
    }
    let (a, b) = (v.get((n - 1) / 2).copied().unwrap_or(f64::NAN), v.get(n / 2).copied().unwrap_or(f64::NAN));
    (a + b) / 2.0
}

/// The clock's own cost: the mean of back-to-back reads (its tick quantizes each, so the mean, not
/// the median, is the unbiased cost a timed stretch carries).
fn timer_ns() -> f64 {
    let n = 100_000u32;
    let mut sum = 0u128;
    for _ in 0..n {
        let a = Instant::now();
        sum += Instant::now().duration_since(a).as_nanos();
    }
    sum as f64 / f64::from(n)
}

/// Release-only stretches need this many requests in flight a copy: below it the clock's own cost
/// (subtracted, but quantized to its tick) is more than a few percent of a stretch.
const MIN_RELEASE_STRETCH: usize = 64;

/// What one implementation's measurement yields.
struct Result {
    step_ns: Vec<f64>,
    rel_ns: Vec<f64>,
    q: Quality,
}

/// One implementation through every phase.
fn measure<S: Clone>(im: &Impl<S>, p: &Params, quality: bool, timer: f64) -> Result {
    let mut im = im.clone();
    let mut rng = Rng(SEED);
    fill(&mut rng, &mut im, 0, p.f);
    let mut bt = Batch { stamp: vec![0; p.f as usize], ..Batch::default() };
    let warm = |rng: &mut Rng, im: &mut Impl<S>, bt: &mut Batch, n: u64| {
        if p.batch > 1 { steps_many(rng, im, p.f, n, bt, p.batch) } else { steps(rng, im, p.f, n) }
    };
    warm(&mut rng, &mut im, &mut bt, p.w);
    let mut step_ns = Vec::with_capacity(p.m);
    for _ in 0..p.m {
        let t = Instant::now();
        warm(&mut rng, &mut im, &mut bt, p.b);
        step_ns.push(t.elapsed().as_nanos() as f64 / p.b as f64);
    }
    let q = if quality { quality_steps(&mut rng, &mut im, p.n, p.f, p.m as u64 * p.b, None) } else { Quality::default() };
    // Release-only: copies of the state, each one's runs released in a timed stretch, stretches
    // summed to B releases a block.
    let mut rel_ns = Vec::with_capacity(p.m_r);
    let live: Vec<u32> = (0..p.f).filter(|s| im.sb.get(*s as usize).is_some_and(|b| *b >= 0)).collect();
    if live.len() >= MIN_RELEASE_STRETCH {
        let release = black_box(im.release);
        let mut order = live.clone();
        for _ in 0..p.m_r {
            let (mut done, mut ns) = (0u64, 0u128);
            while done < p.b {
                let mut copy = im.state.clone();
                // A uniform order of release, as the steps draw them.
                for i in (1..order.len()).rev() {
                    let j = uniform(rng.next(), i as u32 + 1) as usize;
                    order.swap(i, j);
                }
                let take = (p.b - done).min(order.len() as u64) as usize;
                let t = Instant::now();
                for &s in order.iter().take(take) {
                    release(&mut copy, s);
                }
                ns += t.elapsed().as_nanos().saturating_sub(timer as u128);
                done += take as u64;
                drop(black_box(copy));
            }
            rel_ns.push(ns as f64 / p.b as f64);
        }
    }
    Result { step_ns, rel_ns, q }
}

/// `--profile`: the fill, W warm-up steps, then one timed stretch of B steps (ns a step, null not
/// taken off). Run under a counter at two lengths; the difference is B steps alone.
fn profile<S: Clone>(im: &Impl<S>, p: &Params) -> f64 {
    let mut im = im.clone();
    let mut rng = Rng(SEED);
    fill(&mut rng, &mut im, 0, p.f);
    let mut bt = Batch { stamp: vec![0; p.f as usize], ..Batch::default() };
    let warm = |rng: &mut Rng, im: &mut Impl<S>, bt: &mut Batch, n: u64| {
        if p.batch > 1 { steps_many(rng, im, p.f, n, bt, p.batch) } else { steps(rng, im, p.f, n) }
    };
    warm(&mut rng, &mut im, &mut bt, p.w);
    let t = Instant::now();
    warm(&mut rng, &mut im, &mut bt, p.b);
    t.elapsed().as_nanos() as f64 / p.b as f64
}

fn null_impl(slots: usize) -> Impl<()> {
    Impl { state: (), release: null_release, pick: null_pick, many_release: null_many_release, many_pick: null_many_pick, sb: vec![-1; slots] }
}

/// Which per-ask terms a decision carries (`--terms`): each on its own or all together, against
/// the plain path (docs/lb.md "Per-ask terms"). Every set carries a learned CPU time.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct TermSet {
    /// A list of four runners, each with a saving.
    warm: bool,
    /// Bytes to move over the owner's link, its rate measured to every runner (the link order).
    transfer: bool,
    /// I/O bytes, every runner's disk rate measured (the disk order).
    disk: bool,
    /// Network bytes, every runner's network rate measured (the network order).
    net: bool,
    /// An interactive run: the link's latency and drops measured and weighed.
    stability: bool,
}

impl TermSet {
    fn parse(v: &str) -> std::result::Result<TermSet, String> {
        let mut t = TermSet::default();
        for w in v.split(',') {
            match w {
                "warm" => t.warm = true,
                "transfer" => t.transfer = true,
                "disk" => t.disk = true,
                "net" => t.net = true,
                "stability" => t.stability = true,
                "all" => t = TermSet { warm: true, transfer: true, disk: true, net: true, stability: true },
                other => return Err(format!("--terms {other}: warm, transfer, disk, net, stability or all")),
            }
        }
        Ok(t)
    }

    fn name(&self) -> String {
        let all = [(self.warm, "warm"), (self.transfer, "transfer"), (self.disk, "disk"), (self.net, "net"), (self.stability, "stability")];
        all.iter().filter(|x| x.0).map(|x| x.1).collect::<Vec<_>>().join("+")
    }
}

/// The kiln driver deciding through [`Owner::ask_with`] with a [`TermSet`]'s terms, on a fleet
/// measured for them (rates that differ by runner, so each order has a top).
#[derive(Clone)]
struct KilnTerms {
    k: Kiln,
    set: TermSet,
    n: u32,
}

fn kiln_terms_impl(n: u32, d: u32, slots: usize, set: TermSet) -> Impl<KilnTerms> {
    let mut k = Kiln::new(n, d, slots);
    for i in 0..n {
        let step = u64::from(i % 7 + 1);
        if let Some(r) = k.runners.get_mut(i as usize) {
            r.measure_rates(if set.disk { step * (100 << 20) } else { 0 }, if set.net { step * (20 << 20) } else { 0 });
            r.drain(&mut k.rout);
        }
        if set.transfer || set.stability {
            let link = Link { rate: u64::from(i % 5 + 1) * (50 << 20), latency_ns: u64::from(i % 3 + 1) * 1_000_000, between_drops_ns: if set.stability { u64::from(i % 4 + 1) * 3_600_000_000_000 } else { 0 }, outage_ns: 20_000_000_000 };
            k.owner.set_link(RunnerId(i), link);
        }
    }
    k.deliver();
    Impl { state: KilnTerms { k, set, n }, release: kiln_terms_release, pick: kiln_terms_pick, many_release: kiln_terms_many_release, many_pick: kiln_terms_many_pick, sb: vec![-1; slots] }
}

fn kiln_terms_release(t: &mut KilnTerms, s: u32) {
    t.k.release_one(s);
}

fn kiln_terms_pick(t: &mut KilnTerms, s: u32, key: u64) -> i32 {
    let set = t.set;
    let n = u64::from(t.n.max(1));
    // Four runners from the pick's key, each saving a share of the run's CPU time.
    let warm: [Warm; 4] = std::array::from_fn(|j| Warm { runner: RunnerId(((key >> (16 * j)) % n) as u32), saved_ns: 20_000_000_000 * (j as u64 + 1), missing: if set.transfer { 1 << 28 } else { 0 }, wait_ns: 0 });
    let terms = Terms {
        parallel: 0,
        cpu_ns: 300_000_000_000,
        io_bytes: if set.disk { 8 << 30 } else { 0 },
        net_bytes: if set.net { 1 << 30 } else { 0 },
        bytes: if set.transfer { 2 << 30 } else { 0 },
        interactive: set.stability,
        warm: if set.warm { &warm } else { &[] },
    };
    let Ok(a) = t.k.owner.ask_with(t.k.sig, t.k.need, &terms, &mut t.k.out) else { return -1 };
    match a.send {
        Some((to, m)) => t.k.admit(s, to, m),
        None => {
            t.k.waited += 1;
            t.k.deliver();
            -1
        }
    }
}

fn kiln_terms_many_release(t: &mut KilnTerms, slots: &[u32]) {
    for &s in slots {
        kiln_terms_release(t, s);
    }
}

fn kiln_terms_many_pick(t: &mut KilnTerms, slots: &[u32], keys: &[u64], out: &mut [i32]) {
    for ((&s, &key), o) in slots.iter().zip(keys).zip(out.iter_mut()) {
        *o = kiln_terms_pick(t, s, key);
    }
}

fn kiln_impl(n: u32, d: u32, slots: usize) -> Impl<Kiln> {
    kiln_impl_on(n, d, slots, false)
}

/// [`kiln_impl`]; `measured`: every runner measured its speed and slowness (in 1/1024) first, five
/// speeds from three quarters to one and three quarters of the reference and three slownesses,
/// so runners differ as a mixed fleet's do and each order has a top.
fn kiln_impl_on(n: u32, d: u32, slots: usize, measured: bool) -> Impl<Kiln> {
    let mut k = Kiln::new(n, d, slots);
    if measured {
        for i in 0..n {
            if let Some(r) = k.runners.get_mut(i as usize) {
                r.measure(768 + 256 * (i % 5), 1024 + 128 * (i % 3));
                r.drain(&mut k.rout);
            }
        }
        k.deliver();
    }
    Impl { state: k, release: kiln_release, pick: kiln_pick, many_release: kiln_many_release, many_pick: kiln_many_pick, sb: vec![-1; slots] }
}

/// F' (common.h's `lb_inflight`): rho in thousandths, so it is exact.
fn inflight(n: u32, rho_milli: u64) -> u32 {
    u32::try_from((rho_milli * u64::from(n) * u64::from(C) / 1000).max(1)).unwrap_or(u32::MAX)
}

/// B: the smallest power of two whose every one of MIN_BLOCKS null blocks lasts MIN_TICKS.
fn calibrate(n: u32, f: u32, tick: f64, batch: usize) -> u64 {
    let null = null_impl(f as usize);
    let mut b = 1u64;
    while b < 1 << 30 {
        let p = Params { n, f, b, w: 0, m: MIN_BLOCKS, m_r: 0, batch };
        let r = measure(&null, &p, false, 0.0);
        let least = r.step_ns.iter().copied().fold(f64::INFINITY, f64::min) * b as f64;
        if least >= MIN_TICKS as f64 * tick {
            return b;
        }
        b <<= 1;
    }
    b
}

/// MSER-5 (White 1997), as run.py's `mser5`: the 5-sample batches to cut that minimise the
/// marginal standard error of the batch means left.
fn mser5(series: &[f64]) -> (usize, usize) {
    let m = series.len() / 5;
    if m < 2 {
        return (0, m);
    }
    let b: Vec<f64> = series.as_chunks::<5>().0.iter().map(|c| c.iter().sum::<f64>() / 5.0).collect();
    let (mut best, mut arg) = (f64::INFINITY, 0usize);
    for d in 0..m - 1 {
        let tail = b.get(d..).unwrap_or(&[]);
        let mu = tail.iter().sum::<f64>() / tail.len() as f64;
        let v = tail.iter().map(|x| (x - mu) * (x - mu)).sum::<f64>() / (tail.len() * tail.len()) as f64;
        if v < best - 1e-18 {
            best = v;
            arg = d;
        }
    }
    (arg, m)
}

/// W, as run.py's `warmup` finds it, from kiln's own imbalance series: the horizon starts at four
/// residence times and 100 samples and doubles while the cut falls in its second half.
fn warmup(n: u32, f: u32, d: u32) -> u64 {
    let mut h = (4 * u64::from(f)).max(100 * u64::from(n));
    let five = 5 * u64::from(n);
    h = h.div_ceil(five) * five;
    for attempt in 0..4 {
        let mut im = kiln_impl(n, d, f as usize);
        let mut rng = Rng(SEED);
        fill(&mut rng, &mut im, 0, f);
        let mut series = Vec::new();
        quality_steps(&mut rng, &mut im, n, f, h, Some(&mut series));
        let (cut, m) = mser5(&series);
        if cut <= m / 2 || attempt == 3 {
            return cut as u64 * five;
        }
        h *= 2;
    }
    0
}

fn sh(cmd: &str, args: &[&str]) -> String {
    Command::new(cmd).args(args).output().ok().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned()).unwrap_or_default()
}

/// The 1-minute load average, as run.py records it.
fn load1() -> f64 {
    let s = if cfg!(target_os = "macos") { sh("sysctl", &["-n", "vm.loadavg"]) } else { std::fs::read_to_string("/proc/loadavg").unwrap_or_default() };
    s.split_whitespace().find_map(|w| w.trim_matches(|c| c == '{' || c == '}').parse().ok()).unwrap_or(f64::NAN)
}

fn json_list(v: &[f64]) -> String {
    let parts: Vec<String> = v.iter().map(|x| format!("{x:.4}")).collect();
    format!("[{}]", parts.join(","))
}

struct Args {
    json: bool,
    cold: bool,
    ns: Vec<u32>,
    rhos: Vec<u64>,
    ds: Vec<u32>,
    runs: Option<usize>,
    b: Option<u64>,
    w: Option<u64>,
    run: usize,
    batch: usize,
    floor: bool,
    part: Option<&'static str>,
    /// `--profile`: one stretch of B steps after the fill and W warm-up steps and nothing else, for
    /// a hardware counter's delta between two lengths to say what a step costs.
    profile: bool,
    /// `--impl null`: the harness's bookkeeping alone (with `--profile`, what to take off).
    null: bool,
    /// `--terms`: decisions through `ask_with` with these terms.
    terms: Option<TermSet>,
    /// `--measured`: every runner measured its speed and slowness first; asks stay plain.
    measured: bool,
}

fn args() -> std::result::Result<Args, String> {
    let mut a = Args { json: false, cold: false, ns: vec![100, 500, 4095, 10000], rhos: vec![0, 500, 900], ds: vec![1, 2, 4], runs: None, b: None, w: None, run: 0, batch: 1, floor: false, part: None, profile: false, null: false, terms: None, measured: false };
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0usize;
    let rho = |v: &str| -> std::result::Result<u64, String> {
        // In thousandths, exactly: "0.9" is 900.
        let (int, frac) = v.split_once('.').unwrap_or((v, ""));
        let frac3: String = frac.chars().chain(std::iter::repeat('0')).take(3).collect();
        let x = int.parse::<u64>().map_err(|e| format!("--rho {v}: {e}"))? * 1000 + frac3.parse::<u64>().map_err(|e| format!("--rho {v}: {e}"))?;
        if x >= 1000 { Err(format!("--rho {v}: must be below 1")) } else { Ok(x) }
    };
    let list = |v: &str| -> std::result::Result<Vec<u64>, String> { v.split(',').map(|x| x.parse::<u64>().map_err(|e| format!("{x}: {e}"))).collect() };
    while let Some(k) = argv.get(i) {
        let v = argv.get(i + 1).map(String::as_str).unwrap_or("");
        match k.as_str() {
            "--bench" => {
                i += 1;
                continue;
            }
            "--json" => {
                a.json = true;
                i += 1;
                continue;
            }
            "--cold" => {
                a.cold = true;
                i += 1;
                continue;
            }
            "--measured" => {
                a.measured = true;
                i += 1;
                continue;
            }
            "--profile" => {
                a.profile = true;
                i += 1;
                continue;
            }
            "--n" => a.ns = list(v)?.into_iter().map(|x| u32::try_from(x).unwrap_or(u32::MAX)).collect(),
            "--rho" => a.rhos = v.split(',').map(rho).collect::<std::result::Result<_, _>>()?,
            "--d" => a.ds = list(v)?.into_iter().map(|x| u32::try_from(x).unwrap_or(1)).collect(),
            "--runs" => a.runs = Some(v.parse().map_err(|e| format!("--runs {v}: {e}"))?),
            "--b" => a.b = Some(v.parse().map_err(|e| format!("--b {v}: {e}"))?),
            "--w" => a.w = Some(v.parse().map_err(|e| format!("--w {v}: {e}"))?),
            "--run" => a.run = v.parse().map_err(|e| format!("--run {v}: {e}"))?,
            "--batch" => a.batch = v.parse().map_err(|e| format!("--batch {v}: {e}"))?,
            "--terms" => a.terms = Some(TermSet::parse(v)?),
            "--impl" => match v {
                "kiln" => a.floor = false,
                "floor" => a.floor = true,
                "null" => a.null = true,
                "owner" => a.part = Some("owner"),
                "runner" => a.part = Some("runner"),
                "min" => a.part = Some("min"),
                other => return Err(format!("--impl {other}: kiln, floor, null, owner, runner or min")),
            },
            other => return Err(format!("unknown argument {other}")),
        }
        i += 2;
    }
    if a.ns.iter().any(|n| *n < 1) || a.ds.iter().any(|d| ![1, 2, 4].contains(d)) || a.batch == 0 {
        return Err("usage: lb [--json] [--n N,..] [--rho R,..] [--d 1|2|4,..] [--runs K] [--b B] [--w W] [--run R] [--batch K] [--impl kiln|floor]".into());
    }
    Ok(a)
}

/// The 95% percentile bootstrap interval of the median, as run.py's `boot_median_ci`.
fn median_ci(xs: &[f64], seed: u64) -> (f64, f64) {
    let mut rng = Rng(seed);
    let n = xs.len();
    let mut ms: Vec<f64> = (0..2000)
        .map(|_| {
            let mut s: Vec<f64> = (0..n).filter_map(|_| xs.get(uniform(rng.next(), n as u32) as usize).copied()).collect();
            median(&mut s)
        })
        .collect();
    ms.sort_by(f64::total_cmp);
    (ms.get(50).copied().unwrap_or(f64::NAN), ms.get(1949).copied().unwrap_or(f64::NAN))
}

fn main() -> std::process::ExitCode {
    let a = match args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            return std::process::ExitCode::from(2);
        }
    };
    if a.cold {
        let commit = option_env!("KILN_LB_COMMIT").unwrap_or("unknown");
        for &n in &a.ns {
            for &d in &a.ds {
                match cold_sample(n, d) {
                    Ok((build, first, measured, first_measured, first_byte)) => println!("{{\"commit\":\"{commit}\",\"n\":{n},\"d\":{d},\"build_ns\":{build},\"first_placement_ns\":{first},\"first_cpu_measure_ns\":{measured},\"first_measured_placement_ns\":{first_measured},\"first_byte_placement_ns\":{first_byte},\"runner_bytes\":{},\"owner_inline_bytes\":{},\"harness_slot_bytes\":{},\"scope\":\"fresh process/state; OS page cache uncontrolled\"}}", std::mem::size_of::<Runner>(), std::mem::size_of::<Owner>(), 2 * std::mem::size_of::<u32>() + std::mem::size_of::<Rest>()),
                    Err(why) => { eprintln!("{why}"); return std::process::ExitCode::FAILURE; }
                }
            }
        }
        return std::process::ExitCode::SUCCESS;
    }
    if a.profile {
        for &n in &a.ns {
            for &rho in &a.rhos {
                let f0 = inflight(n, rho);
                let f = if a.batch > 1 { f0.max(u32::try_from(a.batch).unwrap_or(u32::MAX)) } else { f0 };
                for &d in &a.ds {
                    let p = Params { n, f, b: a.b.unwrap_or(1 << 24), w: a.w.unwrap_or(1 << 20), m: 1, m_r: 0, batch: a.batch };
                    let ns = if a.null {
                        profile(&null_impl(f as usize), &p)
                    } else if a.floor {
                        profile(&floor_impl(n, f as usize), &p)
                    } else if a.part == Some("owner") {
                        profile(&owner_part_impl(n, d, f as usize), &p)
                    } else if a.part == Some("runner") {
                        profile(&runner_part_impl(n, d, f as usize), &p)
                    } else if a.part == Some("min") {
                        profile(&min_impl(n, f as usize), &p)
                    } else {
                        profile(&kiln_impl(n, d, f as usize), &p)
                    };
                    println!("{n} {:.2} {d} {} {ns:.3}", rho as f64 / 1000.0, p.b);
                }
            }
        }
        return std::process::ExitCode::SUCCESS;
    }
    let tick = tick_ns();
    let timer = timer_ns();
    let machine = format!("{} {}", sh("hostname", &[]), if cfg!(target_os = "macos") { sh("sysctl", &["-n", "machdep.cpu.brand_string"]) } else { String::new() });
    // The commit it was built from, when the build said (run.py runs it from another worktree).
    let commit = option_env!("KILN_LB_COMMIT").map_or_else(|| sh("git", &["rev-parse", "HEAD"]), str::to_owned);
    let (imp, algo) = match (a.floor, a.batch > 1) {
        _ if a.part.is_some() => ("kiln-part", a.part.unwrap_or("").to_owned()),
        _ if a.terms.is_some() => ("kiln", format!("kiln-terms-{}", a.terms.map(|t| t.name()).unwrap_or_default())),
        (true, false) => ("floor", "counters".to_owned()),
        (true, true) => ("floor", format!("counters-batch{}", a.batch)),
        (false, false) if a.measured => ("kiln", "kiln-measured".to_owned()),
        (false, false) => ("kiln", "kiln".to_owned()),
        (false, true) if a.measured => ("kiln", format!("kiln-measured-batch{}", a.batch)),
        (false, true) => ("kiln", format!("kiln-batch{}", a.batch)),
    };
    if !a.json {
        println!("{:>6} {:>5} {:>2} {:>6} {:>8} {:>6} {:>22} {:>7} {:>8} {:>9} {:>4} {:>4} {:>6}", "N", "rho", "d", "B", "W", "runs", "ns/step (95% CI)", "null", "net", "release", "over", "ref", "cv");
    }
    for &n in &a.ns {
        for &rho in &a.rhos {
            let f0 = inflight(n, rho);
            // A batch needs that many in flight: at rho = 0, a burst into an idle fleet.
            let f = if a.batch > 1 { f0.max(u32::try_from(a.batch).unwrap_or(u32::MAX)) } else { f0 };
            let b = a.b.unwrap_or_else(|| calibrate(n, f, tick, a.batch));
            let m = (u64::from(f).div_ceil(b) as usize).clamp(MIN_BLOCKS, MAX_BLOCKS);
            for &d in &a.ds {
                let w = a.w.unwrap_or_else(|| warmup(n, f, d));
                let p = Params { n, f, b, w, m, m_r: MIN_BLOCKS, batch: a.batch };
                let kiln = kiln_impl_on(n, d, f as usize, a.measured);
                let floor = floor_impl(n, f as usize);
                let null = null_impl(f as usize);
                let runs = a.runs.unwrap_or(30);
                let mut nets = Vec::new();
                let mut rows = Vec::new();
                for r in 0..runs {
                    let before = load1();
                    // The null first, then kiln, as common.h measures within one run.
                    let nr = measure(&null, &p, false, timer);
                    let kr = match a.part {
                        Some("min") => measure(&min_impl(n, f as usize), &p, true, timer),
                        Some("owner") => measure(&owner_part_impl(n, d, f as usize), &p, true, timer),
                        Some(_) => measure(&runner_part_impl(n, d, f as usize), &p, true, timer),
                        None if a.floor => measure(&floor, &p, true, timer),
                        None if a.terms.is_some() => measure(&kiln_terms_impl(n, d, f as usize, a.terms.unwrap_or_default()), &p, true, timer),
                        None => measure(&kiln, &p, true, timer),
                    };
                    let after = load1();
                    let (ns_step, ns_null) = (median(&mut kr.step_ns.clone()), median(&mut nr.step_ns.clone()));
                    let ns_release = if kr.rel_ns.is_empty() { f64::NAN } else { median(&mut kr.rel_ns.clone()) };
                    let ns_null_release = if nr.rel_ns.is_empty() { 0.0 } else { median(&mut nr.rel_ns.clone()) };
                    let cv = if kr.q.samples > 0 { kr.q.cv_sum / kr.q.samples as f64 } else { 0.0 };
                    let least = |v: &[f64]| v.iter().copied().fold(f64::INFINITY, f64::min) * b as f64 / tick;
                    if a.json {
                        let rel = if ns_release.is_nan() { "null".to_owned() } else { format!("{ns_release:.4}") };
                        println!(
                            "{{\"impl\":\"{imp}\",\"algo\":\"{algo}\",\"n\":{n},\"rho\":{:.2},\"d\":{d},\"run\":{},\"b\":{b},\"ns_step\":{ns_step:.4},\"ns_release\":{rel},\"ns_null\":{ns_null:.4},\"ns_null_release\":{ns_null_release:.4},\"oversub\":{},\"refusals\":{},\"picks\":{},\"cv\":{cv:.6},\"cv_samples\":{},\"f\":{f},\"w\":{w},\"m\":{m},\"m_r\":{},\"min_step_ticks\":{:.0},\"min_release_ticks\":{:.0},\"tick_ns\":{tick:.3},\"batch\":{},\"load_before\":{before:.2},\"load_after\":{after:.2},\"machine\":\"{}\",\"commit\":\"{commit}\",\"step_blocks\":{},\"release_blocks\":{},\"null_blocks\":{}}}",
                            rho as f64 / 1000.0,
                            a.run + r,
                            kr.q.oversub,
                            kr.q.refusals,
                            kr.q.picks,
                            kr.q.samples,
                            p.m_r,
                            least(&kr.step_ns),
                            if kr.rel_ns.is_empty() { 0.0 } else { least(&kr.rel_ns) },
                            a.batch,
                            machine.replace('"', ""),
                            json_list(&kr.step_ns),
                            json_list(&kr.rel_ns),
                            json_list(&nr.step_ns),
                        );
                        let _ = std::io::stdout().flush();
                    }
                    nets.push(ns_step - ns_null);
                    rows.push((ns_step, ns_null, ns_release, kr.q, cv));
                    // Without --runs: until the median's 95% interval is within 5% of it (at least 6 runs: the fewest with a
                    // distribution-free interval of a median), as run.py's `precise`.
                    if a.runs.is_none() && nets.len() >= 6 {
                        let mid = median(&mut nets.clone());
                        let (lo, hi) = median_ci(&nets, 20261107);
                        if mid > 0.0 && lo >= mid * 0.95 && hi <= mid * 1.05 {
                            break;
                        }
                    }
                }
                if !a.json {
                    let mid = median(&mut nets.clone());
                    let (lo, hi) = median_ci(&nets, 20261107);
                    let step = median(&mut rows.iter().map(|r| r.0).collect::<Vec<_>>());
                    let null = median(&mut rows.iter().map(|r| r.1).collect::<Vec<_>>());
                    let rel = median(&mut rows.iter().map(|r| r.2).collect::<Vec<_>>());
                    let (over, refused) = rows.iter().fold((0u64, 0u64), |acc, r| (acc.0 + r.3.oversub, acc.1 + r.3.refusals));
                    let cvm = median(&mut rows.iter().map(|r| r.4).collect::<Vec<_>>());
                    println!(
                        "{n:>6} {:>5.2} {d:>2} {b:>6} {w:>8} {:>6} {:>8.2} [{lo:>5.2},{hi:>5.2}] {null:>7.2} {mid:>8.2} {:>9} {over:>4} {refused:>4} {cvm:>6.3}",
                        rho as f64 / 1000.0,
                        rows.len(),
                        step,
                        if rel.is_nan() { "-".to_owned() } else { format!("{:.2}", rel - null) },
                    );
                    let _ = std::io::stdout().flush();
                }
            }
        }
    }
    std::process::ExitCode::SUCCESS
}
