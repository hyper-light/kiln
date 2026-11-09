//! The runner's ledger (docs/lb.md "The runner"): the one place a run is admitted, against the
//! runner's own room, never past its budget in any dimension (W05-R03).
//!
//! It keeps what is free of its budget, its epoch (a start sent under another is declined: a
//! fence, since handles here mean nothing to a runner that started again), its admitted runs and
//! its line. A start is admitted when its need fits the room for new runs: what's free less what
//! the first run in the line keeps. So a run passes that one only in room it can't use (EASY
//! backfilling's guarantee to the first in line, Mu'alem and Feitelson, IEEE TPDS 12(6), 2001,
//! taken without run times: room the first run needs is never given to a later one). A run sent
//! to wait is held as soon as it fits that way, the first in line as soon as it fits what's free,
//! and its owner is told Ready; the room stays held until the owner says Go or Withdraw.
//!
//! Every word it sends carries its room ([`Word`]). Owners that watch it (that hold it in an
//! index) are told of each change: the owner it is answering by its answer, the others by a Hint
//! that waits until the shell drains it, the newest replacing an older one (backpressure, no
//! timer). It counts each owner's runs that end here, and says the count when their link comes
//! back (Synced): what the owner missed while the link was down is told apart from what never
//! arrived, with nothing kept per run.

use super::msg::{self, Brief, Of, Start, ToOwner, ToRunner, Why, Word};
use super::slab::{Handle, Slab};
use super::tokens::Book;
use super::{Amounts, NIL, OwnerId, RunnerId, multi, rest, rest_fits};

/// What a runner sends besides the answer a call returns: to whom, and what.
pub type RunnerOut = Vec<(OwnerId, ToOwner)>;

/// An admitted run, 24 bytes: its owner, the owner's handle, its CPU, and its memory, disk and
/// GPUs in a side slab when it asks any (NIL: none), so a slot is four to a cache line.
#[derive(Debug, Clone, Copy, Default)]
struct Admitted {
    owner: OwnerId,
    run: Handle,
    cpu: u64,
    rest: u32,
}

/// A run in the line: queued (in the line's order) or held (its room taken, until Go or
/// Withdraw).
#[derive(Debug, Clone, Copy)]
struct Entry {
    owner: OwnerId,
    run: Handle,
    need: Amounts,
    held: bool,
    prev: u32,
    next: u32,
}

/// An owner it talks to.
#[derive(Debug, Clone, Copy)]
struct Watch {
    owner: OwnerId,
    /// A Hint is owed: its room changed since the owner last heard it.
    pending: bool,
    /// How many of this owner's runs have ended here, this epoch.
    ended: u32,
}

/// No owner (the first watch's place, empty).
const NOBODY: OwnerId = OwnerId(NIL);

impl Watch {
    const fn new(owner: OwnerId) -> Watch {
        Watch { owner, pending: false, ended: 0 }
    }
}

/// How many of its admitted runs a runner holds in its own record, compactly (16 bytes each): the
/// six that fill one of Apple silicon's 128-byte cache lines after the fast entries' fields (two
/// on an x86 line), and two more; the bits of one byte say which are free. A layout: more spill
/// to the heap, nothing else differs.
const RUNS_INLINE: usize = 8;

/// What the fast entries read of an admitted run: its handle's generation (odd while it is
/// there), its CPU (a run of more than a u32 goes on the heap) and the owner's handle.
#[derive(Debug, Clone, Copy, Default)]
#[repr(C)]
struct Held {
    generation: u32,
    cpu: u32,
    run: Handle,
}

/// The rest of an admitted run the general paths read: its owner and its slot of memory, disk
/// and GPUs. What the fast entries admit they don't write here: `Runner::slot_bits` says its
/// owner is the first watch and it asks nothing but CPU.
#[derive(Debug, Clone, Copy, Default)]
struct HeldCold {
    owner: OwnerId,
    rest: u32,
}

/// What a runner counts of its own decisions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RunnerCounts {
    /// Starts, waits and Gos declined.
    pub declined: u64,
    /// Runs held for their owners (Ready sent).
    pub held: u64,
}

/// A runner's ledger. Everything a start, an end or a word reads comes first, together
/// (`repr(C)`), in its first 128 bytes (one cache line of Apple silicon's, two of x86's).
#[derive(Debug, Clone)]
#[repr(C, align(64))]
pub struct Runner {
    /// What its words stamp them with ([`msg::stamp`]).
    stamp: u64,
    /// CPU free: its budget less what its admitted and held runs take.
    cpu: u64,
    /// The first owner it talks to (most runners have one).
    w0: Watch,
    /// The fast entries' whole condition: no owner but the first, no one in its line, nothing
    /// kept back, no run it holds asking more than CPU, a budget a view can say exactly. Kept as
    /// those change ([`Runner::refresh`]).
    plain: bool,
    /// Of `held`: which are free (the low byte) and which the fast entries admitted, so their
    /// owner is the first watch and they ask nothing but CPU (the high byte).
    slot_bits: u16,
    /// Its first admitted runs, the fast entries' whole touch of them one line away at most.
    held: [Held; RUNS_INLINE],
    /// No owner but the first watches it.
    alone: bool,
    /// Whether memory, disk and GPUs are all free for a new run (its words' `whole`), and how
    /// much of them is (their `rest`), kept as they change so a word is copied, not worked out.
    whole: bool,
    /// The rest of `held`, as the fast entries don't write it.
    held_cold: [HeldCold; RUNS_INLINE],
    /// Admitted runs past [`RUNS_INLINE`], or of more CPU than a u32 holds.
    spill: Slab<Admitted>,
    epoch: u32,
    id: RunnerId,
    /// CPU the first queued run keeps (0: none is queued).
    keep: u64,
    /// The first queued run (a line slot; NIL: none).
    first: u32,
    /// Admitted and held runs that take any memory, disk or GPUs.
    holders: u32,
    /// Its measured per-core speed and slowness, as its words say them (0: not measured).
    speed: u32,
    slowness: u32,
    room: [u64; 3],
    /// The other owners it talks to.
    watch: Vec<Watch>,
    /// What admitted runs ask of memory, disk and GPUs (the few that do).
    rests: Slab<[u64; 3]>,
    /// Memory, disk and GPUs free.
    rest: [u64; 3],
    /// What the first queued run keeps of them.
    keep_rest: [u64; 3],
    keeps_rest: bool,
    last: u32,
    budget: Amounts,
    /// Room in use by work it doesn't hold, as the shell measures it: kept off the room for new
    /// runs, with what the first queued run keeps.
    outside: Amounts,
    line: Slab<Entry>,
    pub counts: RunnerCounts,
    /// Its disk's and its network's measured rates, bytes a second (0: not measured).
    disk: u64,
    net: u64,
    /// How many times what it measured changed ([`Word::measured`]).
    measured: u32,
    /// It takes no new run ([`Runner::cordon`]).
    cordoned: bool,
    /// What each of several owners holds of its room, under room tokens ([`super::tokens`]).
    book: Book,
}

impl Runner {
    /// A runner whose whole budget is `budget`, under `epoch` (grown each time it starts again).
    pub fn new(id: RunnerId, budget: Amounts, epoch: u32) -> Runner {
        Runner {
            epoch,
            id,
            cpu: budget[0],
            keep: 0,
            first: NIL,
            holders: 0,
            w0: Watch::new(NOBODY),
            speed: 0,
            slowness: 0,
            alone: true,
            slot_bits: 0xff,
            held: [Held::default(); RUNS_INLINE],
            held_cold: [HeldCold::default(); RUNS_INLINE],
            spill: Slab::new(),
            watch: Vec::new(),
            plain: budget[0] <= u64::from(u32::MAX),
            stamp: msg::stamp(epoch, true, 0),
            rests: Slab::new(),
            room: rest(&budget),
            whole: true,
            rest: rest(&budget),
            keep_rest: [0; 3],
            keeps_rest: false,
            last: NIL,
            budget,
            outside: [0; 4],
            line: Slab::new(),
            counts: RunnerCounts::default(),
            disk: 0,
            net: 0,
            measured: 0,
            cordoned: false,
            book: Book::default(),
        }
    }

    pub fn id(&self) -> RunnerId {
        self.id
    }

    pub fn epoch(&self) -> u32 {
        self.epoch
    }

    pub fn budget(&self) -> Amounts {
        self.budget
    }

    /// Room for `runs` admitted runs before its slab grows.
    pub fn reserve(&mut self, runs: usize) {
        self.spill = Slab::with_capacity(runs.saturating_sub(RUNS_INLINE));
    }

    /// Keeps an admitted run's record: its memory, disk and GPUs in the side slab if it asks any.
    fn admit(&mut self, owner: OwnerId, run: Handle, need: &Amounts) -> Option<Handle> {
        let rest_slot = if multi(need) { self.rests.insert(rest(need))?.slot } else { NIL };
        let at = self.keep_run(owner, run, need[0], rest_slot);
        if at.is_none()
            && let Some(h) = self.rests.handle(rest_slot)
        {
            let _ = self.rests.remove(h);
        }
        at
    }

    /// Keeps an admitted run's record: in `held` while there's room and its CPU fits a u32,
    /// else on the heap.
    fn keep_run(&mut self, owner: OwnerId, run: Handle, cpu: u64, rest: u32) -> Option<Handle> {
        let free = self.slot_bits & 0xff;
        if free != 0
            && let Ok(c) = u32::try_from(cpu)
        {
            let i = free.trailing_zeros() as usize;
            let h = self.held.get_mut(i)?;
            let generation = h.generation.wrapping_add(1);
            *h = Held { generation, cpu: c, run };
            *self.held_cold.get_mut(i)? = HeldCold { owner, rest };
            self.slot_bits &= !(1u16 << i);
            return Some(Handle { slot: i as u32, generation });
        }
        let h = self.spill.insert(Admitted { owner, run, cpu, rest })?;
        Some(Handle { slot: h.slot.saturating_add(RUNS_INLINE as u32), generation: h.generation })
    }

    /// `held`'s run `i` whole, its owner and memory, disk and GPUs as the general paths read them.
    fn compose(&self, i: usize, h: &Held) -> Admitted {
        let implied = self.slot_bits & (256u16 << i) != 0;
        let (owner, rest) = if implied { (self.w0.owner, NIL) } else { self.held_cold.get(i).map_or((self.w0.owner, NIL), |c| (c.owner, c.rest)) };
        Admitted { owner, run: h.run, cpu: u64::from(h.cpu), rest }
    }

    /// The handle on the heap of a handle past `held`.
    fn spilled(at: Handle) -> Handle {
        Handle { slot: at.slot.saturating_sub(RUNS_INLINE as u32), generation: at.generation }
    }

    /// Takes out the admitted run `at`.
    fn release_at(&mut self, at: Handle) -> Option<Admitted> {
        let i = at.slot as usize;
        let Some(h) = self.held.get(i).copied() else {
            return self.spill.remove(Self::spilled(at));
        };
        if h.generation != at.generation || h.generation & 1 == 0 {
            return None;
        }
        let a = self.compose(i, &h);
        if let Some(x) = self.held.get_mut(i) {
            x.generation = x.generation.wrapping_add(1);
        }
        self.slot_bits = (self.slot_bits | (1u16 << i)) & !(256u16 << i);
        Some(a)
    }

    /// Every admitted run with its handle.
    fn admitted_now(&self) -> Vec<(Handle, Admitted)> {
        let held = self.held.iter().zip(0u32..).filter(|(h, _)| h.generation & 1 == 1).map(|(h, i)| (Handle { slot: i, generation: h.generation }, self.compose(i as usize, h)));
        held.chain(self.spill.iter().map(|(h, a)| (Handle { slot: h.slot.saturating_add(RUNS_INLINE as u32), generation: h.generation }, *a))).collect()
    }

    /// The first watch changes: writes down the owner of the runs the fast entries admitted, which
    /// was implied by it.
    fn settle_implied(&mut self) {
        let o = self.w0.owner;
        let mut bits = self.slot_bits >> 8;
        while bits != 0 {
            let i = bits.trailing_zeros() as usize;
            if let Some(c) = self.held_cold.get_mut(i) {
                *c = HeldCold { owner: o, rest: NIL };
            }
            bits &= bits - 1;
        }
        self.slot_bits &= 0xff;
    }

    /// An admitted run's whole need.
    fn need_of(&self, a: &Admitted) -> Amounts {
        let [m, d, g] = self.rests.at(a.rest).copied().unwrap_or([0; 3]);
        [a.cpu, m, d, g]
    }

    /// Its room for a new run, as it says it.
    #[inline]
    pub fn word(&self) -> Word {
        Word { epoch: self.epoch, whole: self.whole, cpu: self.cpu.saturating_sub(self.keep), rest: self.room, speed: self.speed, slowness: self.slowness, disk: self.disk, net: self.net, measured: self.measured, wants: false }
    }

    /// Works out `room` and `whole` again (memory, disk or GPUs, or what the line keeps, changed).
    fn refresh(&mut self) {
        let [m, d, g] = self.rest;
        let [km, kd, kg] = self.keep_rest;
        self.room = [m.saturating_sub(km), d.saturating_sub(kd), g.saturating_sub(kg)];
        self.whole = self.holders == 0 && !self.keeps_rest;
        self.restamp();
    }

    /// Works out what the fast entries read: its stamp, and whether they may go at all.
    fn restamp(&mut self) {
        self.stamp = msg::stamp(self.epoch, self.whole, self.measured);
        self.plain = self.alone && self.first == NIL && self.keep == 0 && self.holders == 0 && self.budget[0] <= u64::from(u32::MAX);
    }

    /// What it measured of itself (docs/lb.md "Rank"): its per-core speed and its per-core
    /// slowness under contention now, each in 1/1024 (0: not measured). Its owners hear them with
    /// its next word. A machine that throttles (heat, a battery's power limit) measures a lower
    /// speed: that is the whole of what the kernel knows of it.
    pub fn measure(&mut self, speed: u32, slowness: u32) {
        if (speed, slowness) != (self.speed, self.slowness) {
            self.speed = speed;
            self.slowness = slowness;
            self.remeasured();
        }
    }

    /// Its disk's effective rate under its load now and its own network's rate, bytes a second
    /// (0: not measured; docs/lb.md "Per-ask terms"). Its owners hear them with its next word.
    pub fn measure_rates(&mut self, disk: u64, net: u64) {
        if (disk, net) != (self.disk, self.net) {
            self.disk = disk;
            self.net = net;
            self.remeasured();
        }
    }

    /// Something measured changed: the count its words carry moves on (round to 1 before
    /// [`msg::NO_FAST`], which no word may say), so its owners read the word whole once.
    fn remeasured(&mut self) {
        self.measured = if self.measured.saturating_add(1) >= msg::NO_FAST { 1 } else { self.measured + 1 };
        self.restamp();
        self.changed_all();
        // Every word says what it measured: under room tokens every owner is owed one.
        if !self.alone && self.book.on {
            self.owe_all();
        }
    }

    /// Takes no new run while `on` (the shell's call: a laptop on battery, a machine being
    /// drained), or takes them again. Runs it holds run on, and runs waiting in its line stay
    /// there, none held until it takes runs again (each waits at another runner as well). Its
    /// words say no room, so owners pick elsewhere, and its owners hear it.
    pub fn cordon(&mut self, on: bool, out: &mut RunnerOut) {
        if on == self.cordoned {
            return;
        }
        self.cordoned = on;
        self.set_keep();
        if !on && self.first != NIL {
            self.pump(out);
        }
        self.changed_all();
    }

    /// Whether it takes no new run.
    pub fn cordoned(&self) -> bool {
        self.cordoned
    }

    /// Memory, disk and GPUs free for a new run.
    #[inline]
    fn rest_room(&self) -> [u64; 3] {
        #[cfg(test)]
        if super::planted(super::Mutant::NoKeep) {
            return self.rest;
        }
        let [m, d, g] = self.rest;
        let [km, kd, kg] = self.keep_rest;
        [m.saturating_sub(km), d.saturating_sub(kd), g.saturating_sub(kg)]
    }

    /// CPU free for a new run.
    #[inline]
    fn cpu_room(&self) -> u64 {
        #[cfg(test)]
        if super::planted(super::Mutant::NoKeep) {
            return self.cpu;
        }
        self.cpu.saturating_sub(self.keep)
    }

    /// Whether `need` fits the room for a new run (`free`: all that's free, the first in line's
    /// own room).
    #[inline]
    fn fits(&self, need: &Amounts, free: bool) -> bool {
        if self.cordoned {
            return false;
        }
        let (cpu, room) = if free { self.free_less_outside() } else { (self.cpu_room(), self.rest_room()) };
        if need[0] > cpu {
            return false;
        }
        #[cfg(test)]
        if super::planted(super::Mutant::AdmitPastRoom) {
            return true;
        }
        !multi(need) || rest_fits(&rest(need), &room)
    }

    /// What's free less what's in use outside the ledger: the first in line's own room.
    fn free_less_outside(&self) -> (u64, [u64; 3]) {
        let [m, d, g] = self.rest;
        let [_, om, od, og] = self.outside;
        (self.cpu.saturating_sub(self.outside[0]), [m.saturating_sub(om), d.saturating_sub(od), g.saturating_sub(og)])
    }

    /// Room in use by work it doesn't hold, measured (lb/wire's ask 2: the shell gives max(0,
    /// measured use - what's admitted), per dimension). Free for a new run is then its budget
    /// less what's admitted, held and outside; a waiting run is held only when that covers it;
    /// owners hear the new room.
    pub fn outside(&mut self, used: Amounts, out: &mut RunnerOut) {
        if used == self.outside {
            return;
        }
        let from = out.len();
        self.outside = used;
        self.set_keep();
        if self.first != NIL {
            self.pump(out);
        }
        self.changed_all();
        if !self.alone && self.book.on {
            self.tokens_after(None, out, from);
        }
    }

    /// Its budget changed (lb/wire's ask 2: GPUs and disk probed after it started, memory on a
    /// VM): what's free is worked out again from what it holds, what waits and fits now is held,
    /// and owners hear its room. Runs it holds past a smaller budget run on; nothing new is
    /// admitted until they fit.
    pub fn set_budget(&mut self, budget: Amounts, out: &mut RunnerOut) {
        let mut taken = [0u64; 4];
        let held: Vec<Amounts> = self.line.iter().filter(|(_, e)| e.held).map(|(_, e)| e.need).collect();
        let admitted: Vec<Amounts> = self.admitted_now().iter().map(|(_, a)| self.need_of(a)).collect();
        for need in admitted.iter().chain(&held) {
            for (t, n) in taken.iter_mut().zip(need) {
                *t = t.saturating_add(*n);
            }
        }
        let from = out.len();
        self.budget = budget;
        let [c, m, d, g] = budget;
        let [tc, tm, td, tg] = taken;
        self.cpu = c.saturating_sub(tc);
        self.rest = [m.saturating_sub(tm), d.saturating_sub(td), g.saturating_sub(tg)];
        self.set_keep();
        if self.first != NIL {
            self.pump(out);
        }
        self.changed_all();
        if !self.alone && self.book.on {
            self.tokens_after(None, out, from);
        }
    }

    /// Whether `need` is more than its whole budget in some dimension.
    fn never(&self, need: &Amounts) -> bool {
        need.iter().zip(&self.budget).any(|(n, b)| n > b)
    }

    /// Takes `need` out of what's free.
    #[inline]
    fn take(&mut self, need: &Amounts) {
        self.cpu = self.cpu.saturating_sub(need[0]);
        if multi(need) {
            for (f, n) in self.rest.iter_mut().zip(rest(need)) {
                *f = f.saturating_sub(n);
            }
            self.holders = self.holders.saturating_add(1);
            self.refresh();
        }
    }

    /// Gives `need` back to what's free.
    #[inline]
    fn give(&mut self, need: &Amounts) {
        self.cpu = self.cpu.saturating_add(need[0]);
        if multi(need) {
            for (f, n) in self.rest.iter_mut().zip(rest(need)) {
                *f = f.saturating_add(n);
            }
            self.holders = self.holders.saturating_sub(1);
            self.refresh();
        }
    }

    /// Its room changed: every owner that watches it is owed a Hint, but `told`, which hears it
    /// in the answer it gets now.
    #[inline(always)]
    fn changed(&mut self, told: OwnerId) {
        #[cfg(test)]
        if super::planted(super::Mutant::NoHint) {
            return;
        }
        if !self.alone && self.book.on {
            return;
        }
        if self.w0.owner != NOBODY {
            self.w0.pending = self.w0.owner != told;
        }
        if !self.alone {
            for w in &mut self.watch {
                w.pending = w.owner != told;
            }
        }
    }

    /// Its room changed and no one hears it in an answer.
    fn changed_all(&mut self) {
        #[cfg(test)]
        if super::planted(super::Mutant::NoHint) {
            return;
        }
        if !self.alone && self.book.on {
            return;
        }
        if self.w0.owner != NOBODY {
            self.w0.pending = true;
        }
        for w in &mut self.watch {
            w.pending = true;
        }
    }

    /// Owner `o` holds it in an index: tell it of each change.
    pub fn watch(&mut self, o: OwnerId) {
        if self.w0.owner == o || self.watch.iter().any(|w| w.owner == o) {
            return;
        }
        let was_alone = self.alone && self.w0.owner != NOBODY;
        if self.w0.owner == NOBODY {
            self.w0 = Watch::new(o);
        } else {
            self.watch.push(Watch::new(o));
        }
        self.alone = self.watch.is_empty();
        self.restamp();
        if self.book.on && !self.alone {
            self.tokens_watched(o, was_alone);
        }
    }

    pub fn unwatch(&mut self, o: OwnerId) {
        if self.w0.owner == o {
            self.settle_implied();
            self.w0 = if self.watch.is_empty() { Watch::new(NOBODY) } else { self.watch.remove(0) };
        }
        self.watch.retain(|w| w.owner != o);
        self.alone = self.watch.is_empty();
        self.restamp();
        if self.book.on {
            self.book.forget(o);
            // The first watcher now may have the room no one holds.
            if self.w0.owner != NOBODY {
                self.w0.pending = true;
            }
            let first = self.first();
            if first != NOBODY {
                self.owe(first);
            }
        }
    }

    /// An owner that starts runs here is counted as one that watches (its ends are counted).
    #[inline(always)]
    fn known(&mut self, o: OwnerId) {
        if self.w0.owner != o {
            self.known_slow(o);
        }
    }

    #[cold]
    #[inline(never)]
    fn known_slow(&mut self, o: OwnerId) {
        self.watch(o);
    }

    /// Owner `o`'s count of ends here.
    fn ends(&mut self, o: OwnerId) -> Option<&mut u32> {
        if self.w0.owner == o {
            return Some(&mut self.w0.ended);
        }
        self.watch.iter_mut().find(|w| w.owner == o).map(|w| &mut w.ended)
    }

    /// Hears an owner's word: the answer to send it (none for a Withdraw); what else to send goes
    /// in `out`.
    #[inline(always)]
    pub fn hear(&mut self, msg: ToRunner, out: &mut RunnerOut) -> Option<ToOwner> {
        if self.book.on && (!self.alone || msg.owner() != self.w0.owner) {
            return self.hear_tokens(msg, out);
        }
        match msg {
            ToRunner::Start(s) => Some(self.start(s)),
            other => self.hear_other(other, out),
        }
    }

    /// Every word but a start: off the fast path.
    #[cold]
    #[inline(never)]
    fn hear_other(&mut self, msg: ToRunner, out: &mut RunnerOut) -> Option<ToOwner> {
        match msg {
            ToRunner::Start(s) => Some(self.start(s)),
            ToRunner::Wait(s) => Some(self.wait(s)),
            ToRunner::Go { owner, run, at, epoch } => Some(self.go(owner, run, at, epoch)),
            ToRunner::Withdraw { owner, run, at } => {
                self.withdraw(owner, run, at, out);
                None
            }
            // One owner holds all of a runner's room: there is no one to forward it to.
            ToRunner::Release { .. } => None,
        }
    }

    #[cold]
    #[inline(never)]
    fn declined(&mut self, run: Handle, why: Why, of: Of) -> ToOwner {
        self.counts.declined = self.counts.declined.saturating_add(1);
        ToOwner::Declined { runner: self.id, run, word: self.word(), why, of }
    }

    /// Whether a word sent under `epoch` is for this runner as it is now.
    #[inline]
    fn current(&self, epoch: u32) -> bool {
        #[cfg(test)]
        if super::planted(super::Mutant::NoFence) {
            return true;
        }
        epoch == self.epoch
    }

    /// Starts a run of CPU alone on the fast path: its epoch, its one owner, room for it. Its
    /// handle here and its room; `None`: nothing done, [`Runner::start`]'s general path decides.
    #[inline(always)]
    pub fn start_fast(&mut self, owner: OwnerId, run: Handle, epoch: u32, cpu: u32) -> Option<(Handle, Brief)> {
        #[cfg(test)]
        if super::any_planted() {
            return None;
        }
        let c = u64::from(cpu);
        if !self.plain || epoch != self.stamp as u32 || owner != self.w0.owner || c > self.cpu {
            return None;
        }
        let bits = self.slot_bits;
        // No slot free: 16, past `held`, and the general path keeps it on the heap.
        let i = (bits & 0xff).trailing_zeros() as usize;
        let h = self.held.get_mut(i)?;
        let generation = h.generation.wrapping_add(1);
        *h = Held { generation, cpu, run };
        self.slot_bits = bits ^ (257u16 << i);
        self.cpu -= c;
        self.w0.pending = false;
        Some((Handle { slot: i as u32, generation }, Brief { stamp: self.stamp, cpu: self.cpu as u32 }))
    }

    /// Starts a run now if it fits the room for a new run; declines it, saying why and its room,
    /// if not.
    #[inline(always)]
    pub fn start(&mut self, s: Start) -> ToOwner {
        if !multi(&s.need)
            && let Ok(cpu) = u32::try_from(s.need[0])
            && let Some((at, _)) = self.start_fast(s.owner, s.run, s.epoch, cpu)
        {
            return ToOwner::Started { runner: self.id, run: s.run, at, word: self.word() };
        }
        self.start_slow(s)
    }

    /// Every other start: another epoch, another owner, memory, disk or GPUs, no room.
    #[cold]
    #[inline(never)]
    fn start_slow(&mut self, s: Start) -> ToOwner {
        if !self.current(s.epoch) {
            return self.declined(s.run, Why::Epoch, Of::Start);
        }
        self.known(s.owner);
        if !self.fits(&s.need, false) {
            let why = if self.never(&s.need) { Why::Never } else { Why::Room };
            return self.declined(s.run, why, Of::Start);
        }
        let Some(at) = self.admit(s.owner, s.run, &s.need) else { return self.declined(s.run, Why::Room, Of::Start) };
        self.take(&s.need);
        self.changed(s.owner);
        ToOwner::Started { runner: self.id, run: s.run, at, word: self.word() }
    }

    /// Takes a run into its line: held at once if it fits the room for a new run (Ready), queued
    /// if not; declined if it never could fit here.
    pub fn wait(&mut self, s: Start) -> ToOwner {
        if !self.current(s.epoch) {
            return self.declined(s.run, Why::Epoch, Of::Wait);
        }
        self.known(s.owner);
        if self.never(&s.need) {
            return self.declined(s.run, Why::Never, Of::Wait);
        }
        let now = self.fits(&s.need, false);
        let Some(at) = self.line.insert(Entry { owner: s.owner, run: s.run, need: s.need, held: now, prev: NIL, next: NIL }) else { return self.declined(s.run, Why::Room, Of::Wait) };
        if now {
            self.take(&s.need);
            self.counts.held = self.counts.held.saturating_add(1);
            self.changed(s.owner);
            return ToOwner::Ready { runner: self.id, run: s.run, at, word: self.word() };
        }
        self.enqueue(at.slot);
        self.changed(s.owner);
        ToOwner::Queued { runner: self.id, run: s.run, at, word: self.word() }
    }

    /// The owner takes the room held for its run: it starts.
    pub fn go(&mut self, owner: OwnerId, run: Handle, at: Handle, epoch: u32) -> ToOwner {
        if !self.current(epoch) {
            return self.declined(run, Why::Epoch, Of::Go);
        }
        match self.line.get(at).copied() {
            Some(e) if e.owner == owner && e.run == run && e.held => {
                let _ = self.line.remove(at);
                match self.admit(owner, run, &e.need) {
                    // Its room was taken as it was held: what's free doesn't change.
                    Some(a) => ToOwner::Started { runner: self.id, run, at: a, word: self.word() },
                    None => {
                        self.give(&e.need);
                        self.changed_all();
                        self.declined(run, Why::Room, Of::Go)
                    }
                }
            }
            _ => self.declined(run, Why::Gone, Of::Go),
        }
    }

    /// Forgets a run in its line; room held for it is free again, and what fits now is held.
    pub fn withdraw(&mut self, owner: OwnerId, run: Handle, at: Handle, out: &mut RunnerOut) {
        let Some(e) = self.line.get(at).copied().filter(|e| e.owner == owner && e.run == run) else { return };
        self.drop_entry(at, &e);
        self.pump(out);
        self.changed_all();
    }

    fn drop_entry(&mut self, at: Handle, e: &Entry) {
        if e.held {
            self.give(&e.need);
        } else {
            self.dequeue(at.slot, e);
        }
        let _ = self.line.remove(at);
    }

    /// A run of CPU alone ended here, on the fast path: its one owner, nothing in line. The
    /// owner's handle for it and the room; `None`: nothing done, [`Runner::end`]'s general path
    /// decides.
    #[inline(always)]
    pub fn end_fast(&mut self, at: Handle) -> Option<(Handle, Brief)> {
        #[cfg(test)]
        if super::any_planted() {
            return None;
        }
        if !self.plain {
            return None;
        }
        let i = at.slot as usize;
        let h = self.held.get_mut(i)?;
        let bits = self.slot_bits;
        // The implied bit: one the fast entries admitted, and still there (a free slot has none).
        if h.generation != at.generation || bits & (256u16 << i) == 0 {
            return None;
        }
        h.generation = h.generation.wrapping_add(1);
        self.slot_bits = bits ^ (257u16 << i);
        let (run, c) = (h.run, h.cpu);
        // Every run it holds was taken from `cpu`, and `plain` says that fits a u32: this can't wrap.
        self.cpu += u64::from(c);
        self.w0.pending = false;
        self.w0.ended = self.w0.ended.wrapping_add(1);
        Some((run, Brief { stamp: self.stamp, cpu: self.cpu as u32 }))
    }

    /// A run here ended: its room is free, what waits and fits now is held, and its owner is told
    /// (and counts it).
    #[inline(always)]
    pub fn end(&mut self, at: Handle, out: &mut RunnerOut) -> Option<(OwnerId, ToOwner)> {
        if let Some((run, _)) = self.end_fast(at) {
            return Some((self.w0.owner, ToOwner::Ended { runner: self.id, run, word: self.word() }));
        }
        let a = self.release_at(at)?;
        if !self.alone && self.book.on {
            return self.end_tokens(a, out);
        }
        self.end_slow(a, out)
    }

    /// Every other end.
    #[cold]
    #[inline(never)]
    fn end_slow(&mut self, a: Admitted, out: &mut RunnerOut) -> Option<(OwnerId, ToOwner)> {
        let need = self.need_of(&a);
        if let Some(h) = self.rests.handle(a.rest) {
            let _ = self.rests.remove(h);
        }
        self.give(&need);
        if self.first != NIL {
            self.pump(out);
        }
        self.changed(a.owner);
        if self.w0.owner == a.owner {
            self.w0.ended = self.w0.ended.wrapping_add(1);
        } else if let Some(n) = self.ends(a.owner) {
            *n = n.wrapping_add(1);
        }
        Some((a.owner, ToOwner::Ended { runner: self.id, run: a.run, word: self.word() }))
    }

    /// Holds what waits and fits: the first in line while it fits what's free, then any behind it
    /// that fits the room it can't use.
    #[cold]
    #[inline(never)]
    fn pump(&mut self, out: &mut RunnerOut) {
        while let Some(e) = self.line.at(self.first).copied() {
            if !self.fits(&e.need, true) {
                break;
            }
            self.hold(self.first, &e, out);
        }
        let mut s = self.line.at(self.first).map_or(NIL, |e| e.next);
        while let Some(e) = self.line.at(s).copied() {
            let next = e.next;
            if self.fits(&e.need, false) {
                self.hold(s, &e, out);
            }
            s = next;
        }
    }

    /// Takes a queued run's room for it, and tells its owner Ready.
    fn hold(&mut self, slot: u32, e: &Entry, out: &mut RunnerOut) {
        self.dequeue(slot, e);
        self.take(&e.need);
        if let Some(x) = self.line.at_mut(slot) {
            x.held = true;
        }
        self.counts.held = self.counts.held.saturating_add(1);
        if let Some(at) = self.line.handle(slot) {
            out.push((e.owner, ToOwner::Ready { runner: self.id, run: e.run, at, word: self.word() }));
        }
        self.changed(e.owner);
    }

    /// Puts a line slot at the tail of the queued runs.
    fn enqueue(&mut self, slot: u32) {
        let tail = self.last;
        if let Some(x) = self.line.at_mut(slot) {
            x.prev = tail;
            x.next = NIL;
        }
        match self.line.at_mut(tail) {
            Some(t) => t.next = slot,
            None => self.first = slot,
        }
        self.last = slot;
        self.set_keep();
    }

    /// Takes a line slot out of the queued runs.
    fn dequeue(&mut self, slot: u32, e: &Entry) {
        let (prev, next) = match self.line.at(slot) {
            Some(x) => (x.prev, x.next),
            None => (e.prev, e.next),
        };
        match self.line.at_mut(prev) {
            Some(p) => p.next = next,
            None => {
                if self.first == slot {
                    self.first = next;
                }
            }
        }
        match self.line.at_mut(next) {
            Some(n) => n.prev = prev,
            None => {
                if self.last == slot {
                    self.last = prev;
                }
            }
        }
        if let Some(x) = self.line.at_mut(slot) {
            x.prev = NIL;
            x.next = NIL;
        }
        self.set_keep();
    }

    /// What is kept off the room for new runs: the first queued run's need, and room in use outside.
    fn set_keep(&mut self) {
        let need = self.line.at(self.first).map_or([0; 4], |e| e.need);
        let [_, om, od, og] = self.outside;
        let [m, d, g] = rest(&need);
        self.keep = need[0].saturating_add(self.outside[0]);
        self.keep_rest = [m.saturating_add(om), d.saturating_add(od), g.saturating_add(og)];
        self.keeps_rest = multi(&need) || (om | od | og) != 0;
        if self.cordoned {
            // Nothing is room for a new run: its words say none, and the fast entries don't go.
            self.keep = u64::MAX;
            self.keep_rest = [u64::MAX; 3];
            self.keeps_rest = true;
        }
        self.refresh();
    }

    /// The Hints owed, each with its room now (one per owner, however many changes it missed).
    pub fn drain(&mut self, out: &mut RunnerOut) {
        if !self.alone && self.book.on {
            return self.drain_tokens(out);
        }
        let word = self.word();
        if self.w0.pending {
            self.w0.pending = false;
            out.push((self.w0.owner, ToOwner::Hint { runner: self.id, word }));
        }
        for w in &mut self.watch {
            if w.pending {
                w.pending = false;
                out.push((w.owner, ToOwner::Hint { runner: self.id, word }));
            }
        }
    }

    /// Owner `o`'s link dropped: what of its waited here is gone (its room free again); what of
    /// its runs here runs on.
    pub fn link_down(&mut self, o: OwnerId, out: &mut RunnerOut) {
        let from = out.len();
        if !self.alone && self.book.on {
            // What it held is no one's while its view of this runner is gone.
            self.book.forget(o);
        }
        let mine: Vec<(Handle, Entry)> = self.line.iter().filter(|(_, e)| e.owner == o).map(|(h, e)| (h, *e)).collect();
        if !mine.is_empty() {
            for (h, e) in mine {
                self.drop_entry(h, &e);
            }
            self.pump(out);
            self.changed_all();
        }
        if !self.alone && self.book.on {
            self.tokens_after(None, out, from);
        }
    }

    /// Owner `o`'s link is back: each of its live runs here said again, then Synced with how many
    /// of its runs have ended here.
    pub fn link_up(&mut self, o: OwnerId, out: &mut RunnerOut) {
        let from = out.len();
        self.link_up_words(o, out);
        if !self.alone && self.book.on {
            self.tokens_after(None, out, from);
        }
    }

    fn link_up_words(&mut self, o: OwnerId, out: &mut RunnerOut) {
        let word = self.word();
        for (at, a) in self.admitted_now().into_iter().filter(|(_, a)| a.owner == o) {
            out.push((o, ToOwner::Started { runner: self.id, run: a.run, at, word }));
        }
        let ended = self.ends(o).map_or(0, |n| *n);
        if self.w0.owner == o {
            self.w0.pending = false;
        }
        if let Some(w) = self.watch.iter_mut().find(|w| w.owner == o) {
            w.pending = false;
        }
        out.push((o, ToOwner::Synced { runner: self.id, word, ended }));
    }

    /// It started again: a new epoch, and nothing of before (its runs died with it, its line is
    /// empty, its handles start over). Every owner that watches it is owed its word.
    pub fn restart(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
        self.held = [Held::default(); RUNS_INLINE];
        self.slot_bits = 0xff;
        self.spill = Slab::new();
        self.rests = Slab::new();
        self.line = Slab::new();
        self.cpu = self.budget[0];
        self.rest = rest(&self.budget);
        self.holders = 0;
        self.refresh();
        self.first = NIL;
        self.last = NIL;
        self.set_keep();
        self.w0.ended = 0;
        for w in &mut self.watch {
            w.ended = 0;
        }
        self.changed_all();
        // A new epoch: nothing of before is held, and every owner is owed its word.
        self.book.reset();
        if !self.alone && self.book.on {
            self.owe_all();
        }
    }

    /// How many queued runs are ahead of the one at `at` in its line (`None`: it isn't queued
    /// here; held room has no place). O(line) (lb/wire's ask 3).
    pub fn ahead(&self, at: Handle) -> Option<u32> {
        let e = self.line.get(at)?;
        if e.held {
            return None;
        }
        let mut n = 0u32;
        let mut s = self.first;
        while s != at.slot {
            let x = self.line.at(s)?;
            n = n.saturating_add(1);
            s = x.next;
        }
        Some(n)
    }

    /// Its admitted runs: owner, the owner's handle, its handle here, its need.
    pub fn admitted(&self) -> Vec<(OwnerId, Handle, Handle, Amounts)> {
        self.admitted_now().iter().map(|(at, a)| (a.owner, a.run, *at, self.need_of(a))).collect()
    }

    /// Its line: owner, the owner's handle, its handle here, whether its room is held, its need.
    pub fn waiting(&self) -> Vec<(OwnerId, Handle, Handle, bool, Amounts)> {
        self.line.iter().map(|(at, e)| (e.owner, e.run, at, e.held, e.need)).collect()
    }

    /// What's free of its budget (held room taken).
    pub fn free(&self) -> Amounts {
        let [m, d, g] = self.rest;
        [self.cpu, m, d, g]
    }
}

// Room tokens (docs/lb.md "Room tokens", src/lb/tokens.rs): what each of several owners is told
// of this runner's room. Off the fast paths: they are taken only by a runner with one owner, which
// holds all of its room under tokens as under hints.
impl Runner {
    /// Room tokens on or off. Every owner it talks to is owed a word, as what they were told was
    /// said the other way.
    pub fn set_tokens(&mut self, on: bool) {
        if self.book.on == on {
            return;
        }
        self.book.on = on;
        if !self.alone {
            if on {
                let room = self.word().room();
                let first = self.first();
                for o in self.owners() {
                    // Each was told the whole room: written down so, it is owed what it holds.
                    self.book.told_room(o, first, &room);
                }
            }
            self.owe_all();
        }
    }

    /// Whether it tells its owners room tokens.
    pub fn tokens(&self) -> bool {
        self.book.on
    }

    /// Owner `o` hears hints (`on`), not tokens (docs/lb.md "Room tokens", a mixed fleet; LB08):
    /// its word is this runner's room, it holds no token here, it never hears that another wants,
    /// and the room its runs free is no one's, so the first watcher, the first owner that hears
    /// tokens, has it. For an owner whose link can't carry words of its own: it hears what every
    /// owner hears. Off until set. Each owner whose word changes with it is owed a Hint.
    pub fn hints_for(&mut self, o: OwnerId, on: bool) {
        if self.book.set_hints(o, on) && self.book.on && !self.alone {
            self.owe_changed();
        }
    }

    /// Whether owner `o` hears hints here ([`Runner::hints_for`]).
    pub fn hears_hints(&self, o: OwnerId) -> bool {
        self.book.hears_hints(o)
    }

    /// Owner `o`'s run waited here for room in a line its shell keeps, not this ledger's (the place
    /// core's probes: docs/lb.md "Adopted"): under room tokens, the owner a token given back is
    /// forwarded to, as a Wait of its queued here would make it.
    pub fn waited(&mut self, o: OwnerId) {
        if self.book.on {
            self.book.waited(o);
        }
    }

    /// The word for owner `o` as its shell says it now, in a word of the shell's own making rather
    /// than one of this ledger's answers (the place core's): under room tokens with several owners,
    /// what `o` holds here, written down as told, so it is owed a Hint once that changes and not
    /// before; else its room. Either way `o` hears its word now: no Hint is owed it for this one.
    pub fn say_to(&mut self, o: OwnerId) -> Word {
        let w = self.word();
        let said = if self.alone || !self.book.on {
            w
        } else {
            let first = self.first();
            let a = if self.watches(o) { self.book.tell(o, first, &w.room()) } else { self.book.amount(o, first, &w.room()) };
            self.token_word(w, o, &a)
        };
        if o != NOBODY {
            if self.w0.owner == o {
                self.w0.pending = false;
            }
            if let Some(x) = self.watch.iter_mut().find(|x| x.owner == o) {
                x.pending = false;
            }
        }
        said
    }

    /// The owners it talks to, the first first.
    fn owners(&self) -> Vec<OwnerId> {
        std::iter::once(self.w0.owner).filter(|o| *o != NOBODY).chain(self.watch.iter().map(|w| w.owner)).collect()
    }

    /// Whether owner `o` is one it talks to.
    fn watches(&self, o: OwnerId) -> bool {
        o != NOBODY && (self.w0.owner == o || self.watch.iter().any(|w| w.owner == o))
    }

    /// The first watcher under room tokens: the first owner it talks to that hears tokens, whose
    /// word says all room the others don't hold (`NOBODY`: every owner hears hints). O(owners)
    /// when the first it talks to hears hints, else O(1).
    fn first(&self) -> OwnerId {
        #[cfg(test)]
        if super::planted(super::Mutant::FirstHearsHints) {
            return self.w0.owner;
        }
        if self.w0.owner != NOBODY && !self.book.hears_hints(self.w0.owner) {
            return self.w0.owner;
        }
        self.watch.iter().map(|w| w.owner).find(|o| !self.book.hears_hints(*o)).unwrap_or(NOBODY)
    }

    /// Each owner whose word would say something other than its last said is owed a Hint.
    fn owe_changed(&mut self) {
        let first = self.first();
        let room = self.word().room();
        let book = &self.book;
        if self.w0.owner != NOBODY && book.owed(self.w0.owner, first, &room) {
            self.w0.pending = true;
        }
        for w in &mut self.watch {
            if book.owed(w.owner, first, &room) {
                w.pending = true;
            }
        }
    }

    /// The word for owner `o`: under tokens, with several owners, what it holds; else its room.
    /// An owner joining a runner is given this one.
    pub fn word_for(&self, o: OwnerId) -> Word {
        let w = self.word();
        if self.alone || !self.book.on {
            return w;
        }
        let a = self.book.amount(o, self.first(), &w.room());
        self.token_word(w, o, &a)
    }

    fn token_word(&self, w: Word, o: OwnerId, a: &Amounts) -> Word {
        let [cpu, m, d, g] = *a;
        // A word that wants is never whole, so it takes its owner's general path, where a token
        // its owner can't use is given back.
        let wants = self.book.wants(o);
        Word { cpu, rest: [m, d, g], whole: w.whole && [m, d, g] == w.rest && !wants, wants, ..w }
    }

    /// Tells `o` its word in `m`: what it holds now, written down as told (an owner it no longer
    /// talks to holds nothing here, and nothing is written down for it).
    fn tell(&mut self, o: OwnerId, m: &mut ToOwner) {
        let w = self.word();
        let first = self.first();
        let a = if self.watches(o) { self.book.tell(o, first, &w.room()) } else { self.book.amount(o, first, &w.room()) };
        *m.word_mut() = self.token_word(w, o, &a);
    }

    fn owe(&mut self, o: OwnerId) {
        if self.w0.owner == o {
            self.w0.pending = true;
        }
        if let Some(w) = self.watch.iter_mut().find(|w| w.owner == o) {
            w.pending = true;
        }
    }

    fn owe_all(&mut self) {
        if self.w0.owner != NOBODY {
            self.w0.pending = true;
        }
        for w in &mut self.watch {
            w.pending = true;
        }
    }

    /// A second owner (or a later one) watches: the first was told all of its room alone, and
    /// the new one holds nothing yet (it joins with [`Runner::word_for`]).
    fn tokens_watched(&mut self, o: OwnerId, was_alone: bool) {
        let room = self.word().room();
        let first = self.first();
        if was_alone {
            let alone = self.w0.owner;
            self.book.told_room(alone, first, &room);
        }
        let _ = self.book.tell(o, first, &room);
    }

    /// After an operation: tokens fit the room; each word this operation sends (`reply`, and
    /// what it pushed in `out` from `from` on) says what its owner holds; each other owner whose
    /// holding changed without its hearing is owed a Hint.
    fn tokens_after(&mut self, reply: Option<(OwnerId, &mut ToOwner)>, out: &mut RunnerOut, from: usize) {
        let first = self.first();
        self.book.fit(first, &self.word().room());
        let mut told = Vec::new();
        if let Some((o, m)) = reply {
            self.tell(o, m);
            told.push(o);
        }
        for (o, m) in out.iter_mut().skip(from) {
            self.tell(*o, m);
            told.push(*o);
        }
        let room = self.word().room();
        let book = &self.book;
        let settle = |w: &mut Watch| {
            if told.contains(&w.owner) {
                w.pending = false;
            } else if book.owed(w.owner, first, &room) {
                w.pending = true;
            }
        };
        if self.w0.owner != NOBODY {
            settle(&mut self.w0);
        }
        for w in &mut self.watch {
            settle(w);
        }
    }

    /// An owner's word, under tokens: the room it takes comes out of its own token first; a Wait
    /// queued for want of room makes it the owner a token given back goes to; a token given back
    /// is forwarded once.
    #[cold]
    #[inline(never)]
    fn hear_tokens(&mut self, msg: ToRunner, out: &mut RunnerOut) -> Option<ToOwner> {
        let from = out.len();
        let first = self.first();
        let before = self.word().room();
        let o = msg.owner();
        let mut reply = match msg {
            ToRunner::Release { owner, epoch, amount } => {
                if self.current(epoch) {
                    let _ = self.book.release(owner, first, &amount, &before);
                    // It set its view of this runner aside as it gave this back: it hears what
                    // it holds now, whatever was in flight to it.
                    self.owe(owner);
                }
                None
            }
            ToRunner::Start(s) => Some(self.start_slow(s)),
            other => self.hear_other(other, out),
        };
        if matches!(msg, ToRunner::Start(_) | ToRunner::Wait(_)) {
            let after = self.word().room();
            let mut used = before;
            for (u, a) in used.iter_mut().zip(after) {
                *u = u.saturating_sub(a);
            }
            // The first watcher as the word left it (a start may have made its owner one).
            let first = self.first();
            self.book.pay(o, first, &used);
        }
        if matches!(reply, Some(ToOwner::Queued { .. })) {
            self.book.waited(o);
        }
        self.tokens_after(reply.as_mut().map(|m| (o, m)), out, from);
        reply
    }

    /// A run ended, under tokens: its room goes back to its owner, less what the line took.
    #[cold]
    #[inline(never)]
    fn end_tokens(&mut self, a: Admitted, out: &mut RunnerOut) -> Option<(OwnerId, ToOwner)> {
        let from = out.len();
        let need = self.need_of(&a);
        let (o, mut m) = self.end_slow(a, out)?;
        let first = self.first();
        let room = self.word().room();
        self.book.fit(first, &room);
        self.book.give_back(o, first, &need, &room);
        self.tokens_after(Some((o, &mut m)), out, from);
        Some((o, m))
    }

    /// The Hints owed under tokens, each with what its owner holds now.
    fn drain_tokens(&mut self, out: &mut RunnerOut) {
        let mut owed: Vec<OwnerId> = Vec::new();
        if self.w0.pending {
            self.w0.pending = false;
            owed.push(self.w0.owner);
        }
        for w in &mut self.watch {
            if w.pending {
                w.pending = false;
                owed.push(w.owner);
            }
        }
        for o in owed {
            let mut m = ToOwner::Hint { runner: self.id, word: self.word() };
            self.tell(o, &mut m);
            out.push((o, m));
        }
    }

    /// What every owner but the first holds here, for the model's oracle.
    #[cfg(test)]
    pub(crate) fn held_tokens(&self) -> Vec<(OwnerId, Amounts)> {
        if self.alone || !self.book.on {
            return Vec::new();
        }
        self.book.tokens(self.first())
    }
}
