//! The capacity view under a nemesis (joint plan step 13, `docs/design/capacity-view.md`; AC-T7.4.1):
//! one machine whose room Hex's node keeps, Hex placing boxes of its own against the view and kiln's
//! runners reserving runs through the keeper ([`hex_api::capacity`]), while the nemesis crashes either
//! side (the keeper also at one of its writes, before or after the write lands), drops links with what
//! was on them, partitions and heals them, lets a runner's wait for an answer run out, retires a
//! runner for good, whose reservations the person then releases, and has the person release a runner
//! wrongly: one that is away with work still running (CV6), which its next sync adopts (CV5).
//!
//! After every step the work actually running fits the room but for what such a wrong release hid from the
//! keeper and its sync then adopted, past the room, which the keeper's view then says; and every running run
//! is in the keeper's books whenever the keeper is up, but one wrongly released, until its runner's next
//! sync. Once the nemesis stops and every link is healed and quiet
//! ([`World::settle`]), the books each side keeps agree: the keeper's of each runner, the runner's own,
//! and the runs it actually runs (CV5's "the counts agree after a partition heals").
//!
//! The room is measured, as the node measures it: every CPU, and the memory that is free or Hex's own boxes', which
//! lacks what each runner's process and its runs use, a runner's runs held besides in its reservations (counted twice,
//! CV12 being withdrawn: under-admission, never more than the machine holds). Each run uses memory within its
//! reservation, moved by the nemesis between samples and, both ways, several times, on either side of the free memory's
//! read in a sample's window. After every step the keeper decides in no more room than its host last measured: nothing
//! is credited.
//!
//! Pure, as the rest of hex-sim: one seed decides every step.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use hex_api::capacity::{
    self, Amounts, Budget, Event, Held, Holder, Keeper, Out, ReservationId, Stop, ToHolder,
    ToKeeper,
};

use crate::rng::Rng;

/// A mistake planted in a shell, for the scenarios to catch (non-vacuity, CLAUDE.md §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mutant {
    /// The keeper's shell releases a holder's reservations when its link drops: on silence (CV5).
    ReleaseOnSilence,
    /// A runner starts a run when it asks for it, not when it is granted.
    StartBeforeGrant,
    /// A runner that starts again lists none of the runs that outlived its last process.
    ForgetSurvivors,
    /// The keeper answers before its write lands, and its crash loses the write (CV2).
    AnswerBeforeWrite,
    /// Hex admits its own boxes against the room less its own alone, not less the holders' (CV3).
    LocalIgnoresForeign,
    /// The keeper's shell drops from a sync what the keeper doesn't hold, so nothing is adopted (CV5).
    DropUnknownAtSync,
    /// The host credits the room each synced runner's work, the smaller of two reads on either side of its read of
    /// free memory, up to the runner's reservations: CV12's withdrawn rule, standing for any credit.
    Credit,
}

/// What a scenario found wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Violation {
    /// The work running needs more than the room in some dimension.
    Oversubscribed {
        step: u64,
        running: Amounts,
        room: Amounts,
    },
    /// The keeper books work past the room (a wrong release's adoption), and its view doesn't say so.
    Unreported {
        step: u64,
        booked: Amounts,
        held: Amounts,
    },
    /// A runner runs a run the keeper's books don't hold.
    Unbooked {
        step: u64,
        runner: usize,
        id: ReservationId,
    },
    /// A runner was told after a sync that the counts don't agree.
    SyncDisagreed { step: u64, runner: usize },
    /// The keeper decides in more room than its host last measured: a credit (CV12, withdrawn), which no read of the
    /// OS's makes safe.
    Overcredited {
        step: u64,
        room: Amounts,
        measured: Amounts,
    },
    /// Once settled, the books disagree: the keeper's of the runner, the runner's own, what it runs.
    Disagree {
        runner: usize,
        keeper: Vec<Held>,
        holder: Vec<Held>,
        running: Vec<Held>,
    },
    /// A core refused what the scenario can't go on from.
    Stopped(String),
    /// Settling didn't quiesce within its bound.
    Unsettled,
}

/// What a scenario did, so its tests show the nemesis struck (non-vacuity).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Report {
    pub steps: u64,
    pub hex_started: u64,
    pub hex_refused: u64,
    pub runs_granted: u64,
    pub runs_refused: u64,
    pub runs_unanswered: u64,
    pub link_drops: u64,
    pub keeper_crashes: u64,
    pub crashes_at_write: u64,
    pub runner_crashes: u64,
    pub released_at_sync: u64,
    pub adopted_at_sync: u64,
    pub forgotten: u64,
    /// Releases the person made of a runner away with work still running (CV6's mistake).
    pub wrongly_forgotten: u64,
    /// The host's samples of its room.
    pub samples: u64,
    /// Samples in whose window a run's use moved, on either side of the free memory's read.
    pub moved_in_window: u64,
}

/// The machine's disk: the keeper's records, which outlive its process, and the crash armed at one of
/// its writes.
#[derive(Debug, Default)]
struct Disk {
    records: BTreeMap<String, String>,
    writes: u64,
    /// The write the keeper's process crashes at, and whether the write lands first.
    crash: Option<(u64, bool)>,
    /// The last write landed, and what it replaced: undone where the planted keeper answered first.
    last: Option<(String, Option<String>)>,
}

impl Disk {
    fn write(&mut self, name: &str, value: Option<&str>) -> Result<(), Stop> {
        let n = self.writes;
        self.writes = self.writes.saturating_add(1);
        let crash = match self.crash {
            Some((at, lands)) if at == n => {
                self.crash = None;
                if !lands {
                    return Err(Stop::Crashed);
                }
                true
            }
            _ => false,
        };
        let before = match value {
            Some(value) => self.records.insert(name.to_owned(), value.to_owned()),
            None => self.records.remove(name),
        };
        self.last = Some((name.to_owned(), before));
        if crash { Err(Stop::Crashed) } else { Ok(()) }
    }

    fn undo_last(&mut self) {
        if let Some((name, before)) = self.last.take() {
            match before {
                Some(value) => self.records.insert(name, value),
                None => self.records.remove(&name),
            };
        }
    }
}

impl capacity::Records for Disk {
    fn put(&mut self, name: &str, value: &str) -> Result<(), Stop> {
        self.write(name, Some(value))
    }

    fn remove(&mut self, name: &str) -> Result<(), Stop> {
        self.write(name, None)
    }

    fn all(&mut self) -> Result<Vec<(String, String)>, Stop> {
        Ok(self
            .records
            .iter()
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect())
    }
}

/// A runner's link to the keeper: what is on it each way, in order, lost when it drops.
#[derive(Debug, Default)]
struct Link {
    up: bool,
    /// Down and held down until healed.
    partitioned: bool,
    /// The session its welcome opened.
    session: Option<u64>,
    to_keeper: VecDeque<ToKeeper>,
    to_holder: VecDeque<ToHolder>,
}

/// One of kiln's runners on the machine.
#[derive(Debug)]
struct Runner {
    name: String,
    /// Its process's epoch, durable: one more at every start.
    epoch: u64,
    /// Its process, while it runs.
    holder: Option<Holder>,
    /// Runs asked for and not answered, with their amounts.
    asked: BTreeMap<ReservationId, Amounts>,
    /// Runs running, started on a grant: the runner's own records, which its next process reads.
    running: BTreeMap<ReservationId, Amounts>,
    link: Link,
    /// Whether its runs outlive its process.
    survive: bool,
    /// Gone for good.
    retired: bool,
    /// Gone for good, and the person hasn't released its reservations yet.
    unforgotten: bool,
    /// Its runs the person released while it was away, still running, which its next sync adopts (CV6):
    /// missing from the keeper's books until then.
    hidden: BTreeMap<ReservationId, Amounts>,
    /// Its runs a sync adopted after such a release, still running: what the keeper may book past the room.
    adopted: BTreeMap<ReservationId, Amounts>,
    /// Its process's own memory, in no reservation. None while its process is down.
    own: u64,
    /// Each running run's memory use, at most its reservation's; a run not here uses none.
    uses: BTreeMap<ReservationId, u64>,
}

impl Runner {
    /// What its running work uses of memory now.
    fn work(&self) -> Option<u64> {
        self.running.keys().try_fold(0u64, |sum, id| {
            sum.checked_add(self.uses.get(id).copied().unwrap_or(0))
        })
    }

    /// A run of its no longer runs: its use goes with it.
    fn ended(&mut self, id: &ReservationId) {
        self.running.remove(id);
        self.hidden.remove(id);
        self.adopted.remove(id);
        self.uses.remove(id);
    }

    /// Every run of its ended with its process.
    fn all_ended(&mut self) {
        self.running.clear();
        self.hidden.clear();
        self.adopted.clear();
        self.uses.clear();
    }
}

/// The nemesis's mix: each event and its weight among them all. A test generator's parameters, not a tuning
/// of the system under test: delivery dominates so links drain between faults.
const MIX: [(Kind, u64); 20] = [
    (Kind::Deliver, 420),
    (Kind::HexAdmit, 70),
    (Kind::HexEnd, 50),
    (Kind::Ask, 110),
    (Kind::EndRun, 70),
    (Kind::Hints, 40),
    (Kind::Drop, 25),
    (Kind::Connect, 60),
    (Kind::Partition, 8),
    (Kind::Heal, 15),
    (Kind::CrashKeeper, 8),
    (Kind::CrashAtWrite, 8),
    (Kind::RestartKeeper, 40),
    (Kind::CrashRunner, 10),
    (Kind::RestartRunner, 40),
    (Kind::RanOut, 15),
    (Kind::Retire, 1),
    (Kind::ForgetAway, 15),
    (Kind::Use, 40),
    (Kind::Sample, 30),
];

/// The most times a run's use moves on each side of the free memory's read in a sample's window, each move anywhere from
/// none to its reservation: both ways, as often, so a use may fall and rise again while free memory is read (any
/// movement, which no credit from the OS's reads survives: CV12). A test generator's parameter.
const MOVES_IN_WINDOW: u64 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Deliver,
    HexAdmit,
    HexEnd,
    Ask,
    EndRun,
    Hints,
    Drop,
    Connect,
    Partition,
    Heal,
    CrashKeeper,
    CrashAtWrite,
    RestartKeeper,
    CrashRunner,
    RestartRunner,
    RanOut,
    Retire,
    ForgetAway,
    /// A run's use moves.
    Use,
    /// The host measures its room.
    Sample,
}

/// The machine's room: sixteen CPUs and 64 GiB, the shape of the recorded day's larger hosts (H10).
pub const ROOM: Amounts = Amounts {
    cpu_milli: 16_000,
    mem_bytes: 64 << 30,
    disk_bytes: 0,
    gpus: 0,
};

/// The most reservations one runner holds: every run of the smallest charge the generator asks, a
/// quarter CPU, fits the room's sixteen CPUs 64 times.
pub const RESERVATIONS: usize = 64;

/// A runner's name, at most this many bytes.
const NAME_BYTES: usize = 32;

/// One machine under the nemesis (module docs).
#[derive(Debug)]
pub struct World {
    rng: Rng,
    budget: Budget,
    disk: Disk,
    keeper: Option<Keeper>,
    boxes: BTreeMap<u64, Amounts>,
    next_box: u64,
    runners: Vec<Runner>,
    /// The room as the host last measured it: the most the keeper decides in (CV12, withdrawn).
    measured: Amounts,
    mutant: Option<Mutant>,
    report: Report,
}

/// A runner process's own memory: a CI runner agent's resident set, 64 MiB to 1 GiB. A test generator's
/// parameter, as the charges are.
fn own(rng: &mut Rng) -> u64 {
    rng.range(1, 16).saturating_mul(64 << 20)
}

/// Runs `steps` steps of seed `seed` with `runners` runners, then settles.
pub fn run(
    seed: u64,
    runners: usize,
    steps: u64,
    mutant: Option<Mutant>,
) -> Result<Report, Violation> {
    let mut world = World::new(seed, runners, mutant)?;
    for _ in 0..steps {
        world.step()?;
    }
    world.settle()
}

fn stopped(why: impl std::fmt::Debug) -> Violation {
    Violation::Stopped(format!("{why:?}"))
}

fn sum<'a>(mut amounts: impl Iterator<Item = &'a Amounts>) -> Option<Amounts> {
    amounts.try_fold(Amounts::ZERO, |sum, more| sum.checked_add(*more))
}

fn held(map: &BTreeMap<ReservationId, Amounts>) -> Vec<Held> {
    map.iter()
        .map(|(id, amount)| Held {
            id: *id,
            amount: *amount,
        })
        .collect()
}

impl World {
    pub fn new(seed: u64, runners: usize, mutant: Option<Mutant>) -> Result<Self, Violation> {
        let mut rng = Rng::new(seed);
        let budget = Budget {
            holders: runners,
            reservations: RESERVATIONS,
            name_bytes: NAME_BYTES,
        };
        let mut disk = Disk::default();
        let keeper = Keeper::boot(budget, &mut disk).map_err(stopped)?;
        let mut all = Vec::new();
        for n in 0..runners {
            let name = format!("kiln-runner-{n}");
            let survive = rng.chance(500);
            let holder = Holder::start(name.clone(), 1, RESERVATIONS, &[]).map_err(stopped)?;
            all.push(Runner {
                name,
                epoch: 1,
                holder: Some(holder),
                asked: BTreeMap::new(),
                running: BTreeMap::new(),
                link: Link::default(),
                survive,
                retired: false,
                unforgotten: false,
                hidden: BTreeMap::new(),
                adopted: BTreeMap::new(),
                own: own(&mut rng),
                uses: BTreeMap::new(),
            });
        }
        let mut world = Self {
            rng,
            budget,
            disk,
            keeper: Some(keeper),
            boxes: BTreeMap::new(),
            next_box: 0,
            runners: all,
            measured: ROOM,
            mutant,
            report: Report::default(),
        };
        let room = world.measure()?;
        if let Some(keeper) = world.keeper.as_mut() {
            keeper.set_room(room);
        }
        Ok(world)
    }

    pub fn report(&self) -> Report {
        self.report
    }

    fn planted(&self, mutant: Mutant) -> bool {
        self.mutant == Some(mutant)
    }

    /// What Hex's own boxes hold.
    fn local(&self) -> Result<Amounts, Violation> {
        sum(self.boxes.values()).ok_or_else(|| stopped("Hex's boxes overflow a sum"))
    }

    /// A charge as the runners and Hex ask: a quarter CPU to four, 256 MiB to 8 GiB.
    fn charge(&mut self) -> Amounts {
        Amounts {
            cpu_milli: self.rng.range(1, 16).saturating_mul(250),
            mem_bytes: self.rng.range(1, 32).saturating_mul(256 << 20),
            ..Amounts::ZERO
        }
    }

    fn pick(&mut self, len: usize) -> Option<usize> {
        let last = u64::try_from(len).ok()?.checked_sub(1)?;
        usize::try_from(self.rng.range(0, last)).ok()
    }

    fn kind(&mut self) -> Kind {
        let total = MIX.iter().map(|(_, w)| *w).fold(0u64, u64::saturating_add);
        let mut roll = self.rng.range(0, total.saturating_sub(1));
        for (kind, weight) in MIX {
            if roll < weight {
                return kind;
            }
            roll = roll.saturating_sub(weight);
        }
        Kind::Deliver
    }

    /// One step of the nemesis, then the checks.
    pub fn step(&mut self) -> Result<(), Violation> {
        self.report.steps = self.report.steps.saturating_add(1);
        let runner = self.pick(self.runners.len());
        match self.kind() {
            Kind::Deliver => self.deliver_any()?,
            Kind::HexAdmit => self.hex_admit()?,
            Kind::HexEnd => self.hex_end(),
            Kind::Ask => self.ask(runner),
            Kind::EndRun => self.end_run(runner),
            Kind::Hints => self.hints()?,
            Kind::Drop => self.drop_link(runner)?,
            Kind::Connect => self.connect(runner),
            Kind::Partition => {
                self.drop_link(runner)?;
                if let Some(r) = runner.and_then(|r| self.runners.get_mut(r)) {
                    r.link.partitioned = true;
                }
            }
            Kind::Heal => {
                if let Some(r) = runner.and_then(|r| self.runners.get_mut(r)) {
                    r.link.partitioned = false;
                }
            }
            Kind::CrashKeeper => self.crash_keeper(false),
            Kind::CrashAtWrite => {
                if self.keeper.is_some() && self.disk.crash.is_none() {
                    let at = self.disk.writes.saturating_add(self.rng.range(0, 2));
                    self.disk.crash = Some((at, self.rng.chance(500)));
                }
            }
            Kind::RestartKeeper => self.restart_keeper()?,
            Kind::CrashRunner => self.crash_runner(runner)?,
            Kind::RestartRunner => self.restart_runner(runner)?,
            Kind::RanOut => self.ran_out(runner)?,
            Kind::Retire => self.retire(runner)?,
            Kind::ForgetAway => self.forget_away(runner)?,
            Kind::Use => {
                self.move_use(runner);
            }
            Kind::Sample => self.sample()?,
        }
        self.forget_retired()?;
        self.check()
    }

    /// What each runner's reserved work uses of memory now.
    fn works(&self) -> Result<Vec<u64>, Violation> {
        self.runners
            .iter()
            .map(|r| {
                r.work()
                    .ok_or_else(|| stopped("a runner's work overflows a sum"))
            })
            .collect()
    }

    /// The machine's room as its host measures it now (`Facts::node_with_owned`): every CPU, and the memory that is
    /// free or Hex's own boxes', so it lacks what the runners' processes and their work use. None is left in a
    /// dimension they use whole. It is the most the keeper may decide in until the next measure.
    fn measure(&mut self) -> Result<Amounts, Violation> {
        let mut taken = 0u64;
        for (r, work) in self.runners.iter().zip(self.works()?) {
            taken = taken
                .checked_add(r.own)
                .and_then(|t| t.checked_add(work))
                .ok_or_else(|| stopped("the runners' memory overflows a sum"))?;
        }
        self.measured = Amounts {
            mem_bytes: ROOM.mem_bytes.saturating_sub(taken),
            ..ROOM
        };
        Ok(self.measured)
    }

    /// One of runner `runner`'s running runs moves its use: anywhere from none to its reservation's memory, a holder's
    /// work within what it reserved. Whether one moved.
    fn move_use(&mut self, runner: Option<usize>) -> bool {
        let Some(n) = runner else {
            return false;
        };
        let len = self.runners.get(n).map_or(0, |r| r.running.len());
        let Some(at) = self.pick(len) else {
            return false;
        };
        let Some((id, most)) = self
            .runners
            .get(n)
            .and_then(|r| r.running.iter().nth(at))
            .map(|(id, amount)| (*id, amount.mem_bytes))
        else {
            return false;
        };
        let to = self.rng.range(0, most);
        match self.runners.get_mut(n) {
            Some(r) => {
                r.uses.insert(id, to);
                true
            }
            None => false,
        }
    }

    /// The host's sample: the room measured and given the keeper as it is (CV12, withdrawn). A run's use moves, both
    /// ways, several times, on either side of the free memory's read. The planted host also reads each joined runner's
    /// work on either side, and credits the room the smaller, up to the runner's reservations: the withdrawn rule.
    fn sample(&mut self) -> Result<(), Violation> {
        if self.keeper.is_none() {
            return Ok(());
        }
        let joined: Vec<usize> = (0..self.runners.len())
            .filter(|n| {
                self.runners
                    .get(*n)
                    .is_some_and(|r| r.link.session.is_some())
            })
            .collect();
        let before = self.works()?;
        let mover = self.pick(self.runners.len());
        let mut moved = false;
        for _ in 0..self.rng.range(0, MOVES_IN_WINDOW) {
            moved |= self.move_use(mover);
        }
        let room = self.measure()?;
        for _ in 0..self.rng.range(0, MOVES_IN_WINDOW) {
            moved |= self.move_use(mover);
        }
        let mut credit = Amounts::ZERO;
        if self.planted(Mutant::Credit) {
            let after = self.works()?;
            for n in joined {
                let (Some(r), Some(keeper)) = (self.runners.get(n), self.keeper.as_ref()) else {
                    continue;
                };
                let books = keeper
                    .held_by(&r.name)
                    .try_fold(Amounts::ZERO, |sum, h| sum.checked_add(h.amount))
                    .ok_or_else(|| stopped("a runner's books overflow a sum"))?;
                let used = before
                    .get(n)
                    .zip(after.get(n))
                    .map_or(0, |(b, a)| (*b).min(*a));
                let read = Amounts {
                    mem_bytes: used,
                    ..Amounts::ZERO
                };
                credit = credit
                    .checked_add(read.min(books))
                    .ok_or_else(|| stopped("the planted credit overflows a sum"))?;
            }
        }
        let given = room
            .checked_add(credit)
            .ok_or_else(|| stopped("the room and its credit overflow a sum"))?;
        if let Some(keeper) = self.keeper.as_mut() {
            keeper.set_room(given);
        }
        let report = &mut self.report;
        report.samples = report.samples.saturating_add(1);
        if moved {
            report.moved_in_window = report.moved_in_window.saturating_add(1);
        }
        Ok(())
    }

    fn hex_admit(&mut self) -> Result<(), Violation> {
        let local = self.local()?;
        let charge = self.charge();
        let ignores = self.planted(Mutant::LocalIgnoresForeign);
        let Some(keeper) = self.keeper.as_mut() else {
            return Ok(());
        };
        let free = if ignores {
            keeper.room().less(local)
        } else {
            keeper.view(local).free()
        };
        if charge.fits(free) {
            self.boxes.insert(self.next_box, charge);
            self.next_box = self.next_box.saturating_add(1);
            keeper.local_changed();
            self.report.hex_started = self.report.hex_started.saturating_add(1);
        } else {
            self.report.hex_refused = self.report.hex_refused.saturating_add(1);
        }
        Ok(())
    }

    fn hex_end(&mut self) {
        let Some(at) = self.pick(self.boxes.len()) else {
            return;
        };
        if let Some(key) = self.boxes.keys().nth(at).copied() {
            self.boxes.remove(&key);
            if let Some(keeper) = self.keeper.as_mut() {
                keeper.local_changed();
            }
        }
    }

    fn ask(&mut self, runner: Option<usize>) {
        let charge = self.charge();
        let early = self.planted(Mutant::StartBeforeGrant);
        let Some(r) = runner.and_then(|r| self.runners.get_mut(r)) else {
            return;
        };
        if r.retired {
            return;
        }
        let Some(holder) = r.holder.as_mut() else {
            return;
        };
        if let Ok((id, msg)) = holder.reserve(charge) {
            r.asked.insert(id, charge);
            r.link.to_keeper.push_back(msg);
            if early {
                r.running.insert(id, charge);
            }
        }
    }

    fn end_run(&mut self, runner: Option<usize>) {
        let Some(r) = runner else {
            return;
        };
        let len = self.runners.get(r).map_or(0, |r| r.running.len());
        let Some(at) = self.pick(len) else {
            return;
        };
        let Some(runner) = self.runners.get_mut(r) else {
            return;
        };
        let Some(id) = runner.running.keys().nth(at).copied() else {
            return;
        };
        runner.ended(&id);
        if let Some(holder) = runner.holder.as_mut() {
            let mut out = Out::default();
            holder.release(id, &mut out);
            runner.link.to_keeper.extend(out.send);
        }
    }

    /// Delivers one message waiting on a link whose receiver is up.
    fn deliver_any(&mut self) -> Result<(), Violation> {
        let keeper_up = self.keeper.is_some();
        let ready: Vec<(usize, bool)> = self
            .runners
            .iter()
            .enumerate()
            .flat_map(|(n, r)| {
                let to_keeper = keeper_up && !r.link.to_keeper.is_empty();
                let to_holder = r.holder.is_some() && !r.link.to_holder.is_empty();
                [(n, true, to_keeper), (n, false, to_holder)]
            })
            .filter(|(_, _, ready)| *ready)
            .map(|(n, to_keeper, _)| (n, to_keeper))
            .collect();
        let Some(at) = self.pick(ready.len()) else {
            return Ok(());
        };
        match ready.get(at).copied() {
            Some((n, true)) => self.deliver_to_keeper(n),
            Some((n, false)) => self.deliver_to_holder(n),
            None => Ok(()),
        }
    }

    fn deliver_to_keeper(&mut self, n: usize) -> Result<(), Violation> {
        let local = self.local()?;
        let drops = self.planted(Mutant::DropUnknownAtSync);
        let Some((msg, name)) = self
            .runners
            .get_mut(n)
            .and_then(|r| r.link.to_keeper.pop_front().map(|m| (m, r.name.clone())))
        else {
            return Ok(());
        };
        let Some(keeper) = self.keeper.as_mut() else {
            return Ok(());
        };
        let msg = match msg {
            // The planted shell's mistake: a sync loses what the keeper doesn't hold, which it would adopt.
            ToKeeper::Sync { fence, held } if drops => {
                let books: BTreeSet<ReservationId> = keeper.held_by(&name).map(|h| h.id).collect();
                ToKeeper::Sync {
                    fence,
                    held: held.into_iter().filter(|h| books.contains(&h.id)).collect(),
                }
            }
            msg => msg,
        };
        match keeper.hear(msg, local, &mut self.disk) {
            Ok(answer) => {
                if let Some(r) = self.runners.get_mut(n) {
                    match &answer {
                        ToHolder::Welcome { fence, .. } => r.link.session = Some(fence.session),
                        // Its sync is heard: what it listed the keeper holds now, adopting what the person's release
                        // had dropped (CV5), so none of its runs is hidden any more.
                        ToHolder::Synced { .. } => {
                            let hidden = std::mem::take(&mut r.hidden);
                            r.adopted.extend(hidden);
                        }
                        _ => {}
                    }
                    r.link.to_holder.push_back(answer);
                }
                Ok(())
            }
            Err(Stop::Crashed) => {
                self.report.crashes_at_write = self.report.crashes_at_write.saturating_add(1);
                self.crash_keeper(true);
                Ok(())
            }
            Err(why) => Err(stopped(why)),
        }
    }

    fn deliver_to_holder(&mut self, n: usize) -> Result<(), Violation> {
        let Some(r) = self.runners.get_mut(n) else {
            return Ok(());
        };
        let (Some(holder), Some(msg)) = (r.holder.as_mut(), r.link.to_holder.pop_front()) else {
            return Ok(());
        };
        let mut out = Out::default();
        holder.hear(msg, &mut out);
        r.link.to_keeper.extend(out.send);
        self.events(n, out.events)
    }

    fn events(&mut self, n: usize, events: Vec<Event>) -> Result<(), Violation> {
        for event in events {
            let Some(r) = self.runners.get_mut(n) else {
                return Ok(());
            };
            let report = &mut self.report;
            match event {
                Event::Granted(id) => {
                    if let Some(amount) = r.asked.remove(&id) {
                        r.running.insert(id, amount);
                        // Its work takes memory as it starts, within what it reserved.
                        r.uses.insert(id, self.rng.range(0, amount.mem_bytes));
                    }
                    report.runs_granted = report.runs_granted.saturating_add(1);
                }
                Event::Refused(id, _) => {
                    r.asked.remove(&id);
                    report.runs_refused = report.runs_refused.saturating_add(1);
                }
                Event::Unanswered(id) => {
                    r.asked.remove(&id);
                    report.runs_unanswered = report.runs_unanswered.saturating_add(1);
                }
                Event::Released(_) => {}
                Event::Synced {
                    agree,
                    released,
                    adopted,
                    ..
                } => {
                    if !agree {
                        return Err(Violation::SyncDisagreed {
                            step: report.steps,
                            runner: n,
                        });
                    }
                    report.released_at_sync = report.released_at_sync.saturating_add(released);
                    report.adopted_at_sync = report.adopted_at_sync.saturating_add(adopted);
                }
                Event::Closed(_) => {
                    r.link.up = false;
                    r.link.session = None;
                    r.link.to_keeper.clear();
                    r.link.to_holder.clear();
                }
            }
        }
        Ok(())
    }

    fn hints(&mut self) -> Result<(), Violation> {
        let local = self.local()?;
        let Some(keeper) = self.keeper.as_mut() else {
            return Ok(());
        };
        for (session, hint) in keeper.hints(local) {
            let r = self
                .runners
                .iter_mut()
                .find(|r| r.link.up && r.link.session == Some(session))
                .ok_or_else(|| stopped("a hint owed to a session no link holds"))?;
            r.link.to_holder.push_back(hint);
        }
        Ok(())
    }

    fn drop_link(&mut self, runner: Option<usize>) -> Result<(), Violation> {
        let Some(n) = runner else {
            return Ok(());
        };
        let silence = self.planted(Mutant::ReleaseOnSilence);
        let Some(r) = self.runners.get_mut(n) else {
            return Ok(());
        };
        if !r.link.up {
            return Ok(());
        }
        r.link.up = false;
        r.link.to_keeper.clear();
        r.link.to_holder.clear();
        if let (Some(keeper), Some(session)) = (self.keeper.as_mut(), r.link.session.take()) {
            keeper.link_down(session);
            if silence {
                // The planted shell's own mistake: what it releases, or how it fails, it doesn't heed.
                let _ = keeper.forget(&r.name, &mut self.disk);
            }
        }
        let mut out = Out::default();
        if let Some(holder) = r.holder.as_mut() {
            holder.link_down(&mut out);
        }
        self.report.link_drops = self.report.link_drops.saturating_add(1);
        self.events(n, out.events)
    }

    fn connect(&mut self, runner: Option<usize>) {
        let keeper_up = self.keeper.is_some();
        let Some(r) = runner.and_then(|r| self.runners.get_mut(r)) else {
            return;
        };
        if r.link.up || r.link.partitioned || r.retired || !keeper_up {
            return;
        }
        if let Some(hello) = r.holder.as_mut().and_then(Holder::connect) {
            r.link.up = true;
            r.link.to_keeper.push_back(hello);
        }
    }

    /// The keeper's process ends: every link drops with it. Hex's boxes and the runners' runs go on.
    fn crash_keeper(&mut self, at_write: bool) {
        if self.keeper.take().is_none() {
            return;
        }
        if self.planted(Mutant::AnswerBeforeWrite) {
            self.disk.undo_last();
        }
        if !at_write {
            self.disk.crash = None;
        }
        self.report.keeper_crashes = self.report.keeper_crashes.saturating_add(1);
        for r in &mut self.runners {
            if !r.link.up {
                continue;
            }
            r.link = Link {
                partitioned: r.link.partitioned,
                ..Link::default()
            };
            if let Some(holder) = r.holder.as_mut() {
                let mut out = Out::default();
                holder.link_down(&mut out);
                for event in out.events {
                    if let Event::Unanswered(id) = event {
                        r.asked.remove(&id);
                    }
                }
            }
        }
    }

    fn restart_keeper(&mut self) -> Result<(), Violation> {
        if self.keeper.is_some() {
            return Ok(());
        }
        match Keeper::boot(self.budget, &mut self.disk) {
            Ok(mut keeper) => {
                // The room measured anew as it starts.
                keeper.set_room(self.measure()?);
                self.keeper = Some(keeper);
                Ok(())
            }
            // The crash armed at the epoch's write: still down, to start again later.
            Err(Stop::Crashed) => Ok(()),
            Err(why) => Err(stopped(why)),
        }
    }

    fn crash_runner(&mut self, runner: Option<usize>) -> Result<(), Violation> {
        let Some(n) = runner else {
            return Ok(());
        };
        if self
            .runners
            .get(n)
            .is_none_or(|r| r.holder.is_none() || r.retired)
        {
            return Ok(());
        }
        self.drop_link(Some(n))?;
        if let Some(r) = self.runners.get_mut(n) {
            r.holder = None;
            r.own = 0;
            r.asked.clear();
            if !r.survive {
                r.all_ended();
            }
            self.report.runner_crashes = self.report.runner_crashes.saturating_add(1);
        }
        Ok(())
    }

    fn restart_runner(&mut self, runner: Option<usize>) -> Result<(), Violation> {
        let forget = self.planted(Mutant::ForgetSurvivors);
        let Some(n) = runner else {
            return Ok(());
        };
        if self
            .runners
            .get(n)
            .is_none_or(|r| r.holder.is_some() || r.retired)
        {
            return Ok(());
        }
        let own = own(&mut self.rng);
        let Some(r) = self.runners.get_mut(n) else {
            return Ok(());
        };
        r.epoch = r.epoch.saturating_add(1);
        let survivors = if forget { Vec::new() } else { held(&r.running) };
        let holder =
            Holder::start(r.name.clone(), r.epoch, RESERVATIONS, &survivors).map_err(stopped)?;
        r.holder = Some(holder);
        r.own = own;
        Ok(())
    }

    fn ran_out(&mut self, runner: Option<usize>) -> Result<(), Violation> {
        let Some(n) = runner else {
            return Ok(());
        };
        let Some(r) = self.runners.get_mut(n) else {
            return Ok(());
        };
        let (Some(holder), Some(id)) = (r.holder.as_mut(), r.asked.keys().next().copied()) else {
            return Ok(());
        };
        let mut out = Out::default();
        holder.ran_out(id, &mut out);
        r.link.to_keeper.extend(out.send);
        self.events(n, out.events)
    }

    /// A runner leaves for good: its process and runs end, and the person releases what it held once
    /// its keeper is up (CV6). At most one a machine, and never its last.
    fn retire(&mut self, runner: Option<usize>) -> Result<(), Violation> {
        let live = self.runners.iter().filter(|r| !r.retired).count();
        if live < 2 || self.runners.iter().any(|r| r.retired) {
            return Ok(());
        }
        let Some(n) = runner else {
            return Ok(());
        };
        self.drop_link(Some(n))?;
        if let Some(r) = self.runners.get_mut(n) {
            r.holder = None;
            r.own = 0;
            r.asked.clear();
            r.all_ended();
            r.retired = true;
            r.unforgotten = true;
        }
        Ok(())
    }

    /// The person releases a runner it takes for gone, wrongly (CV6): one away from the keeper, its link down
    /// (partitioned, dropped, or its process crashed with its runs outliving it), not retired, with work still
    /// running. That work is missing from the keeper's books until the runner's next sync adopts it (CV5). A
    /// release the keeper refuses (the runner connected, or never seen) changes nothing.
    fn forget_away(&mut self, runner: Option<usize>) -> Result<(), Violation> {
        let Some(n) = runner else {
            return Ok(());
        };
        let Some(name) = self
            .runners
            .get(n)
            .filter(|r| !r.retired && !r.link.up && !r.running.is_empty())
            .map(|r| r.name.clone())
        else {
            return Ok(());
        };
        let Some(keeper) = self.keeper.as_mut() else {
            return Ok(());
        };
        let released = keeper.forget(&name, &mut self.disk);
        if matches!(released, Ok(Err(_))) {
            return Ok(());
        }
        // Released, or the keeper crashed at one of its removals: what still runs may be missing either way.
        if let Some(r) = self.runners.get_mut(n) {
            r.hidden
                .extend(r.running.iter().map(|(id, amount)| (*id, *amount)));
            r.adopted.clear();
        }
        match released {
            Ok(_) => {
                self.report.wrongly_forgotten = self.report.wrongly_forgotten.saturating_add(1);
                Ok(())
            }
            Err(Stop::Crashed) => {
                self.report.crashes_at_write = self.report.crashes_at_write.saturating_add(1);
                self.crash_keeper(true);
                Ok(())
            }
            Err(why) => Err(stopped(why)),
        }
    }

    fn forget_retired(&mut self) -> Result<(), Violation> {
        for n in 0..self.runners.len() {
            let Some(name) = self
                .runners
                .get(n)
                .filter(|r| r.unforgotten)
                .map(|r| r.name.clone())
            else {
                continue;
            };
            let Some(keeper) = self.keeper.as_mut() else {
                return Ok(());
            };
            match keeper.forget(&name, &mut self.disk) {
                // Unknown: it never said hello, so the keeper holds nothing of it.
                Ok(Ok(_) | Err(capacity::NotForgotten::Unknown)) => {
                    if let Some(r) = self.runners.get_mut(n) {
                        r.unforgotten = false;
                    }
                    self.report.forgotten = self.report.forgotten.saturating_add(1);
                }
                Ok(Err(why)) => return Err(stopped(why)),
                Err(Stop::Crashed) => {
                    self.report.crashes_at_write = self.report.crashes_at_write.saturating_add(1);
                    self.crash_keeper(true);
                    return Ok(());
                }
                Err(why) => return Err(stopped(why)),
            }
        }
        Ok(())
    }

    /// The work running fits the room but for what a wrong release hid from the keeper (CV6) and a sync then
    /// adopted (CV5), which is the only work the keeper may book past the room, and then its view says by how
    /// much it is over; every running run is in the keeper's books while it is up, but one the person released
    /// while its runner was away, until that runner's next sync; and the keeper decides in no more room than its host
    /// last measured (CV12, withdrawn).
    fn check(&self) -> Result<(), Violation> {
        let step = self.report.steps;
        let overflow = || Violation::Oversubscribed {
            step,
            running: Amounts::from_array([u64::MAX; 4]),
            room: ROOM,
        };
        let running = sum(self
            .runners
            .iter()
            .flat_map(|r| r.running.values())
            .chain(self.boxes.values()))
        .ok_or_else(overflow)?;
        let hidden =
            sum(self.runners.iter().flat_map(|r| r.hidden.values())).ok_or_else(overflow)?;
        let adopted =
            sum(self.runners.iter().flat_map(|r| r.adopted.values())).ok_or_else(overflow)?;
        // What the keeper books of what runs, and of that what it granted or admitted within the room itself.
        let booked = running
            .checked_sub(hidden)
            .ok_or_else(|| stopped("hidden work not running"))?;
        let granted = booked
            .checked_sub(adopted)
            .ok_or_else(|| stopped("adopted work not running"))?;
        if !granted.fits(ROOM) {
            return Err(Violation::Oversubscribed {
                step,
                running,
                room: ROOM,
            });
        }
        let Some(keeper) = self.keeper.as_ref() else {
            return Ok(());
        };
        for (n, r) in self.runners.iter().enumerate() {
            let books: BTreeMap<ReservationId, Amounts> =
                keeper.held_by(&r.name).map(|h| (h.id, h.amount)).collect();
            for (id, amount) in &r.running {
                if !r.hidden.contains_key(id)
                    && !books.get(id).is_some_and(|booked| amount.fits(*booked))
                {
                    return Err(Violation::Unbooked {
                        step,
                        runner: n,
                        id: *id,
                    });
                }
            }
        }
        if !booked.fits(ROOM) {
            let view = keeper.view(self.local()?);
            if !booked.less(ROOM).fits(view.over()) {
                return Err(Violation::Unreported {
                    step,
                    booked,
                    held: view.held,
                });
            }
        }
        // CV12, withdrawn: the keeper decides in no more room than its host last measured.
        let room = keeper.room();
        if !room.fits(self.measured) {
            return Err(Violation::Overcredited {
                step,
                room,
                measured: self.measured,
            });
        }
        Ok(())
    }

    /// The nemesis stops; the keeper and every runner start again where down; every partition heals and
    /// every link connects; everything said is delivered and every hint drained until nothing more is
    /// said. Then the books each side keeps must agree.
    pub fn settle(&mut self) -> Result<Report, Violation> {
        self.disk.crash = None;
        self.restart_keeper()?;
        if self.keeper.is_none() {
            return Err(Violation::Unsettled);
        }
        for n in 0..self.runners.len() {
            self.restart_runner(Some(n))?;
            if let Some(r) = self.runners.get_mut(n) {
                r.link.partitioned = false;
            }
            self.connect(Some(n));
        }
        self.forget_retired()?;
        for _ in 0..self.settle_bound()? {
            let pending = self
                .runners
                .iter()
                .any(|r| !r.link.to_keeper.is_empty() || !r.link.to_holder.is_empty());
            if pending {
                self.deliver_any()?;
            } else {
                self.hints()?;
                if self
                    .runners
                    .iter()
                    .all(|r| r.link.to_keeper.is_empty() && r.link.to_holder.is_empty())
                {
                    return self.agree();
                }
            }
            self.check()?;
        }
        Err(Violation::Unsettled)
    }

    /// The most steps settling takes from here: the protocol's own count of what the links carry (CLAUDE.md §2), so
    /// a settle past it says more than the protocol can. A word on a link to the keeper is answered once, and that
    /// answer has its runner say at most one more (a welcome its sync; a grant of a reserve it abandoned, or of another
    /// amount, its release), itself answered once: four deliveries. A word on a link to a runner starts at most three.
    /// Once a runner, its sync's answer has it say the release of each reservation it released while it synced, at
    /// most the [`RESERVATIONS`] it holds, each answered: two deliveries apiece. Once all is quiet the keeper owes each
    /// runner at most one hint, and two looks find it quiet, the first draining those hints.
    fn settle_bound(&self) -> Result<usize, Violation> {
        let overflow = || stopped("the settle's bound overflows");
        let releases = RESERVATIONS.checked_mul(2).ok_or_else(overflow)?;
        self.runners.iter().try_fold(2usize, |bound, r| {
            let to_keeper = r.link.to_keeper.len().checked_mul(4);
            let to_holder = r.link.to_holder.len().checked_mul(3);
            to_keeper
                .zip(to_holder)
                .and_then(|(k, h)| k.checked_add(h)?.checked_add(releases)?.checked_add(1))
                .and_then(|runner| bound.checked_add(runner))
                .ok_or_else(overflow)
        })
    }

    fn agree(&self) -> Result<Report, Violation> {
        let keeper = self.keeper.as_ref().ok_or(Violation::Unsettled)?;
        for (n, r) in self.runners.iter().enumerate() {
            let books: Vec<Held> = keeper.held_by(&r.name).collect();
            let holder: Vec<Held> = r
                .holder
                .as_ref()
                .map(|h| h.held().collect())
                .unwrap_or_default();
            let running = held(&r.running);
            let up = r.retired || r.holder.as_ref().is_some_and(Holder::is_up);
            if !up || !r.asked.is_empty() || books != holder || holder != running {
                return Err(Violation::Disagree {
                    runner: n,
                    keeper: books,
                    holder,
                    running,
                });
            }
        }
        Ok(self.report)
    }
}

#[cfg(test)]
mod tests;
