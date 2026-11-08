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

use super::msg::{Of, Start, ToOwner, ToRunner, Why, Word};
use super::slab::{Handle, Slab};
use super::{Amounts, NIL, OwnerId, RunnerId, multi, rest, rest_fits};

/// What a runner sends besides the answer a call returns: to whom, and what.
pub type RunnerOut = Vec<(OwnerId, ToOwner)>;

/// An admitted run, 24 bytes: its owner, the owner's handle, its CPU, and its memory, disk and
/// GPUs in a side slab when it asks any (NIL: none), so a slot is four to a cache line.
#[derive(Debug, Clone, Copy)]
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
#[repr(C)]
pub struct Runner {
    epoch: u32,
    id: RunnerId,
    /// CPU free: its budget less what its admitted and held runs take.
    cpu: u64,
    /// CPU the first queued run keeps (0: none is queued).
    keep: u64,
    /// The first queued run (a line slot; NIL: none).
    first: u32,
    /// Admitted and held runs that take any memory, disk or GPUs.
    holders: u32,
    /// The first owner it talks to (most runners have one).
    w0: Watch,
    /// Its measured per-core speed and slowness, as its words say them (0: not measured).
    speed: u32,
    slowness: u32,
    /// No owner but the first watches it.
    alone: bool,
    /// Whether memory, disk and GPUs are all free for a new run (its words' `whole`), and how
    /// much of them is (their `rest`), kept as they change so a word is copied, not worked out.
    whole: bool,
    room: [u64; 3],
    runs: Slab<Admitted>,
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
            runs: Slab::new(),
            watch: Vec::new(),
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
        self.runs = Slab::with_capacity(runs);
    }

    /// Keeps an admitted run's record: its memory, disk and GPUs in the side slab if it asks any.
    fn admit(&mut self, owner: OwnerId, run: Handle, need: &Amounts) -> Option<Handle> {
        let rest_slot = if multi(need) { self.rests.insert(rest(need))?.slot } else { NIL };
        let at = self.runs.insert(Admitted { owner, run, cpu: need[0], rest: rest_slot });
        if at.is_none()
            && let Some(h) = self.rests.handle(rest_slot)
        {
            let _ = self.rests.remove(h);
        }
        at
    }

    /// An admitted run's whole need.
    fn need_of(&self, a: &Admitted) -> Amounts {
        let [m, d, g] = self.rests.at(a.rest).copied().unwrap_or([0; 3]);
        [a.cpu, m, d, g]
    }

    /// Its room for a new run, as it says it.
    #[inline]
    pub fn word(&self) -> Word {
        Word { epoch: self.epoch, whole: self.whole, cpu: self.cpu.saturating_sub(self.keep), rest: self.room, speed: self.speed, slowness: self.slowness }
    }

    /// Works out `room` and `whole` again (memory, disk or GPUs, or what the line keeps, changed).
    fn refresh(&mut self) {
        let [m, d, g] = self.rest;
        let [km, kd, kg] = self.keep_rest;
        self.room = [m.saturating_sub(km), d.saturating_sub(kd), g.saturating_sub(kg)];
        self.whole = self.holders == 0 && !self.keeps_rest;
    }

    /// What it measured of itself (docs/lb.md "Rank"): its per-core speed and its per-core
    /// slowness under contention now, each in 1/1024 (0: not measured). Its owners hear them with
    /// its next word.
    pub fn measure(&mut self, speed: u32, slowness: u32) {
        if (speed, slowness) != (self.speed, self.slowness) {
            self.speed = speed;
            self.slowness = slowness;
            self.changed_all();
        }
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
        self.outside = used;
        self.set_keep();
        if self.first != NIL {
            self.pump(out);
        }
        self.changed_all();
    }

    /// Its budget changed (lb/wire's ask 2: GPUs and disk probed after it started, memory on a
    /// VM): what's free is worked out again from what it holds, what waits and fits now is held,
    /// and owners hear its room. Runs it holds past a smaller budget run on; nothing new is
    /// admitted until they fit.
    pub fn set_budget(&mut self, budget: Amounts, out: &mut RunnerOut) {
        let mut taken = [0u64; 4];
        let held: Vec<Amounts> = self.line.iter().filter(|(_, e)| e.held).map(|(_, e)| e.need).collect();
        let admitted: Vec<Amounts> = self.runs.iter().map(|(_, a)| self.need_of(a)).collect();
        for need in admitted.iter().chain(&held) {
            for (t, n) in taken.iter_mut().zip(need) {
                *t = t.saturating_add(*n);
            }
        }
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
        if self.w0.owner == NOBODY {
            self.w0 = Watch::new(o);
        } else {
            self.watch.push(Watch::new(o));
        }
        self.alone = self.watch.is_empty();
    }

    pub fn unwatch(&mut self, o: OwnerId) {
        if self.w0.owner == o {
            self.w0 = if self.watch.is_empty() { Watch::new(NOBODY) } else { self.watch.remove(0) };
        }
        self.watch.retain(|w| w.owner != o);
        self.alone = self.watch.is_empty();
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

    /// Starts a run now if it fits the room for a new run; declines it, saying why and its room,
    /// if not.
    #[inline(always)]
    pub fn start(&mut self, s: Start) -> ToOwner {
        // The fast path: its epoch, its one owner, CPU alone, room for it.
        #[cfg(test)]
        let planted = super::any_planted();
        #[cfg(not(test))]
        let planted = false;
        if s.epoch == self.epoch
            && s.owner == self.w0.owner
            && self.alone
            && !multi(&s.need)
            && s.need[0] <= self.cpu.saturating_sub(self.keep)
            && !planted
            && let Some(at) = self.runs.insert(Admitted { owner: s.owner, run: s.run, cpu: s.need[0], rest: NIL })
        {
            self.cpu -= s.need[0];
            self.w0.pending = false;
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

    /// A run here ended: its room is free, what waits and fits now is held, and its owner is told
    /// (and counts it).
    #[inline(always)]
    pub fn end(&mut self, at: Handle, out: &mut RunnerOut) -> Option<(OwnerId, ToOwner)> {
        let a = self.runs.remove(at)?;
        #[cfg(test)]
        let planted = super::any_planted();
        #[cfg(not(test))]
        let planted = false;
        // The fast path: its one owner, CPU alone, nothing in line.
        if a.owner == self.w0.owner && self.alone && self.first == NIL && a.rest == NIL && !planted {
            self.cpu = self.cpu.saturating_add(a.cpu);
            self.w0.pending = false;
            self.w0.ended = self.w0.ended.wrapping_add(1);
            return Some((a.owner, ToOwner::Ended { runner: self.id, run: a.run, word: self.word() }));
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
        self.refresh();
    }

    /// The Hints owed, each with its room now (one per owner, however many changes it missed).
    pub fn drain(&mut self, out: &mut RunnerOut) {
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
        let mine: Vec<(Handle, Entry)> = self.line.iter().filter(|(_, e)| e.owner == o).map(|(h, e)| (h, *e)).collect();
        if mine.is_empty() {
            return;
        }
        for (h, e) in mine {
            self.drop_entry(h, &e);
        }
        self.pump(out);
        self.changed_all();
    }

    /// Owner `o`'s link is back: each of its live runs here said again, then Synced with how many
    /// of its runs have ended here.
    pub fn link_up(&mut self, o: OwnerId, out: &mut RunnerOut) {
        let word = self.word();
        for (at, a) in self.runs.iter().filter(|(_, a)| a.owner == o) {
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
        self.runs = Slab::new();
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
        self.runs.iter().map(|(at, a)| (a.owner, a.run, at, self.need_of(a))).collect()
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
