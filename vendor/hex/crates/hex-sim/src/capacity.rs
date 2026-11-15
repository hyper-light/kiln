//! The capacity view under a nemesis (joint plan step 13, `docs/design/capacity-view.md`; AC-T7.4.1):
//! one machine whose room Hex's node keeps, Hex placing boxes of its own against the view and kiln's
//! runners reserving runs through the keeper ([`hex_api::capacity`]), while the nemesis crashes either
//! side (the keeper also at one of its writes, before or after the write lands), drops links with what
//! was on them, partitions and heals them, lets a runner's wait for an answer run out, retires a
//! runner for good, whose reservations the person then releases, and has the person release a runner
//! wrongly: one that is away with work still running (CV6), which its next sync adopts (CV5).
//!
//! Every rate is a record's (ROOT's rulings of 2026-11-14; [`rates`]): kiln's runs arrive as they did at the machine
//! whose room this is, Hex's boxes as the fleet day's `oci` boxes did at each of its `oci` hosts, each run and box ends
//! at one over its recorded length, and links drop, runners crash and outages end as kiln's records of its runners'
//! links say. A step is one event of that continuous-time chain, each kind as likely as its rate in the state the
//! machine is in; a fault no record times strikes once a run ([`Fault`]). What the code does on its own trigger, it
//! does there: a runner dials again as soon as its link can open, the keeper's hints go out after each message it
//! hears and each command of its host, and Hex's node measures its room as it is asked to admit. In the bounded suite's
//! time ([`Time::Filled`]) each run's and box's length is stretched by the one factor at which the recorded mean demand
//! fills the room, every other ratio as recorded.
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
//! reservation, moving at every step, a sample's window among them. After every step the keeper decides in no more
//! room than its host last measured: nothing is credited.
//!
//! Pure, as the rest of hex-sim: one seed decides every step.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::OnceLock;

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
    /// Reserves a runner let go of before their answers came (CV5: the holder's own release of a waiting reserve).
    pub runs_let_go: u64,
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
    /// Each run's recorded length in seconds, asked or running ([`Leg::runs_for`]).
    lengths: BTreeMap<ReservationId, f64>,
    link: Link,
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
        self.lengths.remove(id);
    }

    /// Every run of its ended with its process.
    fn all_ended(&mut self) {
        self.running.clear();
        self.hidden.clear();
        self.adopted.clear();
        self.uses.clear();
        self.lengths.clear();
    }

    /// A run asked for has no answer coming: it won't run, but where the planted runner started it anyway.
    fn unasked(&mut self, id: &ReservationId) {
        self.asked.remove(id);
        if !self.running.contains_key(id) {
            self.lengths.remove(id);
        }
    }

    /// No asked run has an answer coming: its process ended.
    fn all_unasked(&mut self) {
        let asked: Vec<ReservationId> = self.asked.keys().copied().collect();
        for id in asked {
            self.unasked(&id);
        }
    }
}

/// The chain's events, each at the rate the records give it in the state the machine is in ([`World::rates_now`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// A word on a link is delivered.
    Deliver,
    /// Hex's node is asked to admit a box of its own (`Submit`).
    HexAdmit,
    /// One of Hex's own boxes ends.
    HexEnd,
    /// kiln asks a runner for a run.
    Ask,
    /// A run ends.
    EndRun,
    /// A link ends with what was on it; its runner dials again at once.
    Drop,
    /// A link ends with what was on it and is held down: an outage.
    Partition,
    /// An outage ends.
    Heal,
    /// A runner's process ends.
    CrashRunner,
    /// A runner's process starts again.
    RestartRunner,
    /// The keeper's process starts again.
    RestartKeeper,
}

/// The faults no record times (ROOT, 2026-11-14 (a): the least at which every fault happens in every seed's run): each
/// strikes once a run, at a step drawn for it, or at the first step after where it can.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fault {
    /// The keeper's process ends between messages.
    CrashKeeper,
    /// The keeper's process ends at its next write, which is lost.
    CrashBeforeWrite,
    /// The keeper's process ends at its next write, which lands.
    CrashAfterWrite,
    /// A runner lets go of a reserve before its answer: its run is no longer wanted (CV5), the one way a reserve is
    /// abandoned with its link up, no wait of a holder's ending at a time (CV6; the review's E-8).
    LetGo,
    /// The person releases a runner away with work still running (CV6's mistake).
    ForgetAway,
    /// A runner leaves for good, and the person releases what it held: a live runner, never the last.
    Retire,
}

const FAULTS: [Fault; 6] = [
    Fault::CrashKeeper,
    Fault::CrashBeforeWrite,
    Fault::CrashAfterWrite,
    Fault::LetGo,
    Fault::ForgetAway,
    Fault::Retire,
];

/// How the runs' lengths stand to the recorded rates (ROOT, 2026-11-14).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Time {
    /// ROOT's (ii), the bounded suite's: each run's and box's length stretched by the factor at which the recorded mean
    /// demand fills the room ([`Rates::fill`]), the regime of contention and refusals; every other ratio as recorded.
    Filled,
    /// ROOT's (i), the record sweep's mode, reported and not gated: every rate and length as recorded.
    Recorded,
}

/// A case the nemesis runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Case {
    /// kiln's runners on the machine.
    pub runners: usize,
    /// Whether their runs outlive their processes: kiln states it for neither its runs nor its CI legs (ROOT,
    /// 2026-11-14), so each outcome is its own case.
    pub survive: bool,
    pub time: Time,
}

/// The machine's room: the recorded fleet's (H10's `bench/sched/huawei-r5-day30.fleet.trace`, from kiln's runner facts
/// of 2026-11-05) smallest by memory of the hosts where Hex's own boxes and kiln's runs share one room, those that run
/// Hex's `oci` class: fedora's 32 hardware threads and 62.5 GiB, no GPU, where the two contend for the least room
/// (`the_machine_is_the_recorded_fleets_smallest_shared_host` reads it from the trace).
pub const ROOM: Amounts = Amounts {
    cpu_milli: 32_000,
    mem_bytes: 67_128_643_584,
    disk_bytes: 0,
    gpus: 0,
};

/// kiln's own CI as recorded: fourteen days of its legs, each one's need what kiln measured it using, its mean cores in
/// milli-cores and its peak RSS in bytes, and its length (`bench/sched/kiln_workload.md`): the runs kiln's runners
/// ask (ROOT's ruling of 2026-11-14: drawn as recorded).
const KILN_LEGS: &str = include_str!("../../../bench/sched/kiln-ci-20261022-20261105.trace");

/// The recorded fleet's day (H10), whose `oci` legs are the boxes Hex places of its own (ROOT, 2026-11-14), on the room
/// [`ROOM`] is read from.
const FLEET: &str = include_str!("../../../bench/sched/huawei-r5-day30.fleet.trace");

/// The resolution of a recorded leg's need: the trace's unit of CPU, a milli-core, to which `kiln_trace.py` rounds each
/// mean, and the export's of memory, a 4 KiB page, of which every peak RSS kiln measured is a whole number
/// (`a_recorded_legs_need_is_whole_units_of_its_resolution`). A leg recorded below it, 0, used between nothing and one
/// unit: it is drawn as one unit, the bound above its true need (ROOT, 2026-11-14), so none is dropped and none made 0.
const RESOLUTION: Amounts = Amounts {
    cpu_milli: 1,
    mem_bytes: 4096,
    disk_bytes: 0,
    gpus: 0,
};

/// The legs kiln ran on the machine whose room this is, fedora, in its export's fourteen days: 355 as
/// fedora.attlocal.net up to 2026-10-26, then 341 as kiln-a094a12d.attlocal.net from 2026-10-27, the same machine
/// renamed (`fedora_ran_696_of_the_exports_legs` counts them in the export).
const FEDORA_LEGS: u32 = 696;

/// The export's window, in seconds: runs created from 2026-10-22T00:00:00Z up to 2026-11-05T00:00:00Z (its header).
const KILN_WINDOW: f64 = 1_209_600.0;

/// The fleet trace's window, in seconds: Huawei Cloud region 5's day 30 (its header).
const FLEET_DAY: f64 = 86_400.0;

/// kiln's records of its runners' links to its server, read 2026-11-14T04:36Z through kiln's `runners`: each link's
/// drops while up (`topo.drops`), its time up (`topo.up_ms`) and its outages' total (`topo.path.outage_ms`), for
/// adas-mac-studio.local, adas-macbook-pro-2.local, adas-mac-studio-3.local, adas-mac-studio.local-2 and adacachy2, every
/// link with a drop (adacachy's has none, so no outage of its is a drop's). kiln doesn't tell a crash, a restart and a
/// network's drop apart: a drop is a drop (ROOT, 2026-11-14).
const KILN_LINKS: [(u64, u64, u64); 5] = [
    (178, 190_790_534, 114_855),
    (78, 135_604_022, 75_192),
    (67, 190_345_527, 113_336),
    (46, 229_313_558, 41_087),
    (8, 133_839_471, 1_078_559),
];

/// A recorded leg: what it needed, and how long it ran, in seconds.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Leg {
    need: Amounts,
    runs_for: f64,
}

/// One leg from its trace row, `leg,<arrival ns>,<runs for ns>,<cpu milli>,<memory bytes>,...`: `None` for any other
/// row.
fn leg(row: &str) -> Option<Leg> {
    let mut fields = row.trim().split(',');
    if fields.next()? != "leg" {
        return None;
    }
    let _arrives: u64 = fields.next()?.parse().ok()?;
    let runs_for: u64 = fields.next()?.parse().ok()?;
    let cpu_milli = fields.next()?.parse().ok()?;
    let mem_bytes = fields.next()?.parse().ok()?;
    Some(Leg {
        need: Amounts {
            cpu_milli,
            mem_bytes,
            ..Amounts::ZERO
        },
        // A trace's times are nanoseconds.
        runs_for: runs_for as f64 / 1e9,
    })
}

/// kiln's legs as its runners ask them ([`KILN_LEGS`]), none below its resolution.
fn kiln_legs() -> &'static [Leg] {
    static LEGS: OnceLock<Vec<Leg>> = OnceLock::new();
    LEGS.get_or_init(|| {
        KILN_LEGS
            .lines()
            .filter_map(leg)
            .map(|leg| Leg {
                need: Amounts {
                    cpu_milli: leg.need.cpu_milli.max(RESOLUTION.cpu_milli),
                    mem_bytes: leg.need.mem_bytes.max(RESOLUTION.mem_bytes),
                    ..leg.need
                },
                ..leg
            })
            .collect()
    })
}

/// Hex's own boxes, each a recorded `oci` leg ([`FLEET`]).
fn oci_legs() -> &'static [Leg] {
    static LEGS: OnceLock<Vec<Leg>> = OnceLock::new();
    LEGS.get_or_init(|| {
        FLEET
            .lines()
            .filter(|row| row.trim().ends_with(",oci"))
            .filter_map(leg)
            .collect()
    })
}

/// What kiln's runs ask, one after another in recorded order ([`kiln_legs`], none below its resolution): a holder's runs
/// in the capacity view's tests on real machines (hex-cli's `capacity_view`; ROOT's ruling 7, 2026-11-14).
pub fn kiln_needs() -> impl Iterator<Item = Amounts> {
    kiln_legs().iter().map(|leg| leg.need)
}

/// What Hex's own boxes ask, one after another in recorded order ([`oci_legs`]): Hex's boxes in those tests, as in this
/// simulator's (ROOT's rulings 2 and 7, 2026-11-14).
pub fn oci_needs() -> impl Iterator<Item = Amounts> {
    oci_legs().iter().map(|leg| leg.need)
}

/// The most reservations one runner holds (CV8): runs of the smallest charge recorded ([`kiln_legs`]), as many as the
/// room holds in its dimension that holds fewest, but never more than one sync carries (`hex_core::capacity::most_held`):
/// the protocol's own bound, which wins (ROOT, 2026-11-14; `a_runners_reservation_bound_is_the_protocols_own`).
pub fn reservations() -> Result<usize, Violation> {
    let smallest = kiln_legs()
        .iter()
        .map(|leg| leg.need)
        .reduce(Amounts::min)
        .ok_or_else(|| stopped("no recorded leg"))?;
    let fits = ROOM
        .cpu_milli
        .checked_div(smallest.cpu_milli)
        .zip(ROOM.mem_bytes.checked_div(smallest.mem_bytes))
        .map(|(cpu, mem)| cpu.min(mem))
        .ok_or_else(|| stopped("a recorded charge of nothing"))?;
    let most = hex_core::capacity::most_held().map_err(stopped)?;
    Ok(usize::try_from(fits).map_or(most, |fits| fits.min(most)))
}

/// What the nemesis's chain runs at, per second of recorded time, each from a record (ROOT, 2026-11-14).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rates {
    /// kiln's runs arriving at the machine: fedora's legs over the export's window ([`FEDORA_LEGS`]).
    pub ask: f64,
    /// Hex's own boxes arriving at it: the fleet day's `oci` legs over its `oci` hosts and the day.
    pub admit: f64,
    /// A link's drops while it is up, the fleet's ([`KILN_LINKS`]): the rate of each fault a record times, a link's end
    /// (a drop, a partition) and a process's (a runner's crash), each cause at the whole rate since kiln can't tell
    /// them apart (ROOT, 2026-11-14).
    pub drop: f64,
    /// An outage's end, one over the fleet's mean outage ([`KILN_LINKS`]): a partition's heal, a runner's or the keeper's
    /// start again.
    pub heal: f64,
    /// The factor each run's and box's length is stretched by for the recorded mean demand to fill the room
    /// ([`Time::Filled`]): one over the larger, of CPU's and memory's, of ρ = (`ask` × the mean of length × need over
    /// kiln's legs + `admit` × the same over Hex's boxes) ÷ the room, the mean held being the arrival rate times the mean
    /// of length times need (Little's law; `the_bounded_time_is_where_the_recorded_demand_fills_the_room`).
    pub fill: f64,
}

/// The chain's rates ([`Rates`]), read from the records once.
pub fn rates() -> Result<Rates, Violation> {
    static RATES: OnceLock<Result<Rates, String>> = OnceLock::new();
    RATES
        .get_or_init(recorded)
        .clone()
        .map_err(Violation::Stopped)
}

/// A count as a rate's numerator.
fn counted(n: usize) -> Result<f64, String> {
    u32::try_from(n)
        .map(f64::from)
        .map_err(|_| format!("{n} is past a count a rate is made of"))
}

/// The mean of length × `of` over `legs`, in seconds × units.
fn held_per_arrival(legs: &[Leg], of: fn(&Amounts) -> u64) -> Result<f64, String> {
    let total: f64 = legs
        .iter()
        .map(|leg| leg.runs_for * of(&leg.need) as f64)
        .sum();
    Ok(total / counted(legs.len())?)
}

fn recorded() -> Result<Rates, String> {
    let fleet = crate::replay::parse(FLEET, &crate::replay::Limits::default())
        .map_err(|e| format!("the fleet trace: {e}"))?;
    let hosts = fleet
        .nodes
        .iter()
        .filter(|n| n.classes.has(crate::model::Class::Oci))
        .count();
    let ask = f64::from(FEDORA_LEGS) / KILN_WINDOW;
    let admit = counted(oci_legs().len())? / (counted(hosts)? * FLEET_DAY);
    let (drops, up_ms, outage_ms) =
        KILN_LINKS
            .iter()
            .fold((0u64, 0u64, 0u64), |(drops, up, out), (d, u, o)| {
                (
                    drops.saturating_add(*d),
                    up.saturating_add(*u),
                    out.saturating_add(*o),
                )
            });
    let drop = drops as f64 / (up_ms as f64 / 1e3);
    let heal = drops as f64 / (outage_ms as f64 / 1e3);
    let rho = |of: fn(&Amounts) -> u64, room: u64| -> Result<f64, String> {
        Ok(
            (ask * held_per_arrival(kiln_legs(), of)? + admit * held_per_arrival(oci_legs(), of)?)
                / room as f64,
        )
    };
    let fill =
        1.0 / rho(|a| a.cpu_milli, ROOM.cpu_milli)?.max(rho(|a| a.mem_bytes, ROOM.mem_bytes)?);
    let rates = Rates {
        ask,
        admit,
        drop,
        heal,
        fill,
    };
    let all = [ask, admit, drop, heal, fill];
    if all.iter().all(|r| r.is_finite() && *r > 0.0) {
        Ok(rates)
    } else {
        Err(format!("a record gives no rate: {rates:?}"))
    }
}

/// The deliveries one event of `kind` causes on a quiet machine whose `runners` are all synced, each the protocol's own:
/// a reserve and its answer; a release and its answer; a hint to each other runner its change owes, or to every one
/// for its host's (a box admitted or ended); and for a link's or a runner's end, the redial it causes, a hello, the
/// welcome, the sync and its answer, at once for a drop and as the outage ends for the others, so an outage's end, the
/// cause's, says none of its own. Delivery's rate is the sum over the kinds of each one's rate times these: the least at
/// which the links drain (ROOT, 2026-11-14; `each_kinds_deliveries_are_the_protocols_own` counts them). The keeper's
/// crash, which no record times, strikes once a run ([`Fault`]): its runners' redials as it starts again drain at that
/// rate too.
fn deliveries(kind: Kind, runners: usize) -> f64 {
    let all = u32::try_from(runners).map_or(f64::MAX, f64::from);
    let others = (all - 1.0).max(0.0);
    match kind {
        Kind::Ask | Kind::EndRun => 2.0 + others,
        Kind::HexAdmit | Kind::HexEnd => all,
        Kind::Drop | Kind::Partition | Kind::CrashRunner => 4.0,
        Kind::Heal | Kind::RestartRunner | Kind::RestartKeeper | Kind::Deliver => 0.0,
    }
}

/// Hex's admission under way: a `Submit` (`Service::command`), its refresh of the room first (`refresh`, the code's own
/// trigger for measuring it: ROOT's (f)), its admission decided in that room, then the hints it owes. Its reads are a
/// step apart, so the runs' uses move between them: of each runner's work (as the planted host makes it), of free
/// memory, and the room given the keeper.
#[derive(Debug)]
struct Command {
    leg: Leg,
    before: Vec<u64>,
    room: Option<Amounts>,
    moved: bool,
}

/// A running run, by its runner's index and its id, with the hazard of its end ([`World::run_ends`]).
type RunEnd = ((usize, ReservationId), f64);

/// One machine under the nemesis (module docs).
#[derive(Debug)]
pub struct World {
    rng: Rng,
    budget: Budget,
    case: Case,
    rates: Rates,
    /// The factor each run's and box's recorded length is stretched by: [`Rates::fill`] in [`Time::Filled`], else 1.
    stretch: f64,
    /// Hex's admission under way ([`World::command`]).
    command: Option<Command>,
    /// The faults no record times, each with the step it strikes at or after ([`Fault`]).
    due: Vec<(u64, Fault)>,
    disk: Disk,
    keeper: Option<Keeper>,
    boxes: BTreeMap<u64, Leg>,
    next_box: u64,
    runners: Vec<Runner>,
    /// The room as the host last measured it: the most the keeper decides in (CV12, withdrawn).
    measured: Amounts,
    mutant: Option<Mutant>,
    report: Report,
}

/// A runner process's own memory: OWED. No record of a kiln runner agent's resident set exists; ROOT measures them on
/// the fleet (2026-11-14), and until then this, 64 MiB to 1 GiB, is the generator's old picked range, chosen anew by
/// nothing.
fn own(rng: &mut Rng) -> u64 {
    rng.range(1, 16).saturating_mul(64 << 20)
}

/// Runs `steps` steps of seed `seed` in `case`, then settles.
pub fn run(seed: u64, case: Case, steps: u64, mutant: Option<Mutant>) -> Result<Report, Violation> {
    let mut world = World::new(seed, case, steps, mutant)?;
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

/// The one of `weights` a roll in `[0, total)` lands in, by their order: the last with weight where rounding leaves it
/// past them all.
fn landed<T: Copy>(weights: &[(T, f64)], roll: f64) -> Option<T> {
    let mut left = roll;
    let mut last = None;
    for (item, weight) in weights {
        if *weight <= 0.0 {
            continue;
        }
        last = Some(*item);
        if left < *weight {
            break;
        }
        left -= weight;
    }
    last
}

impl World {
    /// A machine in `case` for a run of `steps` steps, the steps its faults no record times strike at drawn from them.
    pub fn new(
        seed: u64,
        case: Case,
        steps: u64,
        mutant: Option<Mutant>,
    ) -> Result<Self, Violation> {
        let rates = rates()?;
        let mut rng = Rng::new(seed);
        let names: Vec<String> = (0..case.runners)
            .map(|n| format!("kiln-runner-{n}"))
            .collect();
        // The keeper's bound on a holder's name (CV8): the longest its runners say in their hellos.
        let name_bytes = names.iter().map(String::len).max().unwrap_or(0);
        let budget = Budget {
            holders: case.runners,
            reservations: reservations()?,
            name_bytes,
        };
        let mut disk = Disk::default();
        let keeper = Keeper::boot(budget, &mut disk).map_err(stopped)?;
        let mut all = Vec::new();
        for name in names {
            let holder =
                Holder::start(name.clone(), 1, budget.reservations, &[]).map_err(stopped)?;
            all.push(Runner {
                name,
                epoch: 1,
                holder: Some(holder),
                asked: BTreeMap::new(),
                running: BTreeMap::new(),
                lengths: BTreeMap::new(),
                link: Link::default(),
                retired: false,
                unforgotten: false,
                hidden: BTreeMap::new(),
                adopted: BTreeMap::new(),
                own: own(&mut rng),
                uses: BTreeMap::new(),
            });
        }
        let due = FAULTS
            .iter()
            .map(|fault| (rng.range(0, steps.saturating_sub(1)), *fault))
            .collect();
        let stretch = match case.time {
            Time::Filled => rates.fill,
            Time::Recorded => 1.0,
        };
        let mut world = Self {
            rng,
            budget,
            case,
            rates,
            stretch,
            command: None,
            due,
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
        sum(self.boxes.values().map(|leg| &leg.need))
            .ok_or_else(|| stopped("Hex's boxes overflow a sum"))
    }

    /// A leg drawn from `recorded`: one of them, each as likely.
    fn draw(&mut self, recorded: &[Leg]) -> Result<Leg, Violation> {
        self.pick(recorded.len())
            .and_then(|at| recorded.get(at).copied())
            .ok_or_else(|| stopped("no recorded leg"))
    }

    fn pick(&mut self, len: usize) -> Option<usize> {
        let last = u64::try_from(len).ok()?.checked_sub(1)?;
        usize::try_from(self.rng.range(0, last)).ok()
    }

    /// One of the runners `which` holds for, each as likely.
    fn pick_runner(&mut self, which: impl Fn(&Runner) -> bool) -> Option<usize> {
        let eligible: Vec<usize> = (0..self.runners.len())
            .filter(|n| self.runners.get(*n).is_some_and(&which))
            .collect();
        let at = self.pick(eligible.len())?;
        eligible.get(at).copied()
    }

    /// A draw uniform in `[0, 1)`: the top 53 bits of the generator's next, a double's whole precision.
    fn unit(&mut self) -> f64 {
        self.rng.next_u64().checked_shr(11).unwrap_or(0) as f64 / 9_007_199_254_740_992.0
    }

    /// One step of the nemesis, then the checks: every run's use moves; Hex's admission under way takes its next
    /// phase; a fault no record times strikes if its step has come and it can, else the chain's next event; the person
    /// releases what a retired runner held; every runner that can dials again; then the checks.
    pub fn step(&mut self) -> Result<(), Violation> {
        self.report.steps = self.report.steps.saturating_add(1);
        self.move_uses();
        self.command()?;
        match self.due_now() {
            Some(fault) => self.strike(fault)?,
            None => self.event()?,
        }
        self.forget_retired()?;
        self.redial();
        self.check()
    }

    /// Each running run's length, the hazard of its end: one over its recorded length stretched.
    fn hazard(&self, length: f64) -> Result<f64, Violation> {
        let hazard = 1.0 / (self.stretch * length);
        if hazard.is_finite() && hazard > 0.0 {
            Ok(hazard)
        } else {
            Err(stopped("a run of no recorded length"))
        }
    }

    /// Every running run, with its runner and the hazard of its end.
    fn run_ends(&self) -> Result<Vec<RunEnd>, Violation> {
        let mut ends = Vec::new();
        for (n, r) in self.runners.iter().enumerate() {
            for id in r.running.keys() {
                let length = r
                    .lengths
                    .get(id)
                    .copied()
                    .ok_or_else(|| stopped("a run of no recorded length"))?;
                ends.push(((n, *id), self.hazard(length)?));
            }
        }
        Ok(ends)
    }

    /// Every box of Hex's, with the hazard of its end.
    fn box_ends(&self) -> Result<Vec<(u64, f64)>, Violation> {
        self.boxes
            .iter()
            .map(|(key, leg)| Ok((*key, self.hazard(leg.runs_for)?)))
            .collect()
    }

    /// Each kind's rate now, per second of recorded time ([`Rates`]): the arrivals' as recorded; each end's the sum of
    /// its runs' or boxes' hazards; each fault's the drop rate for each link up or process running it can strike; each
    /// outage's end one over the mean outage for each it can end; and delivery's the least at which the links drain
    /// ([`deliveries`]). A runner retired is in none.
    fn rates_now(&self) -> Result<[(Kind, f64); 11], Violation> {
        let recorded = self.rates;
        let count = |which: fn(&Runner) -> bool| {
            let n = self
                .runners
                .iter()
                .filter(|r| !r.retired && which(r))
                .count();
            u32::try_from(n).map_or(f64::MAX, f64::from)
        };
        let up = count(|r| r.link.up);
        let running = count(|r| r.holder.is_some());
        let down = count(|r| r.holder.is_none());
        let partitioned = count(|r| r.link.partitioned);
        let admit = if self.keeper.is_some() && self.command.is_none() {
            recorded.admit
        } else {
            0.0
        };
        let keeper_down = if self.keeper.is_none() {
            recorded.heal
        } else {
            0.0
        };
        let mut rates = [
            (Kind::Ask, recorded.ask),
            (Kind::HexAdmit, admit),
            (Kind::EndRun, self.run_ends()?.iter().map(|(_, h)| h).sum()),
            (Kind::HexEnd, self.box_ends()?.iter().map(|(_, h)| h).sum()),
            (Kind::Drop, recorded.drop * up),
            (Kind::Partition, recorded.drop * up),
            (Kind::CrashRunner, recorded.drop * running),
            (Kind::Heal, recorded.heal * partitioned),
            (Kind::RestartRunner, recorded.heal * down),
            (Kind::RestartKeeper, keeper_down),
            (Kind::Deliver, 0.0),
        ];
        let deliver = rates
            .iter()
            .map(|(kind, rate)| rate * deliveries(*kind, self.runners.len()))
            .sum();
        if let Some(last) = rates.last_mut() {
            *last = (Kind::Deliver, deliver);
        }
        Ok(rates)
    }

    /// The chain's next event, each kind as likely as its rate now: the direct method (D. T. Gillespie, "Exact
    /// stochastic simulation of coupled chemical reactions", J. Phys. Chem. 81(25), 1977).
    fn event(&mut self) -> Result<(), Violation> {
        let rates = self.rates_now()?;
        let total: f64 = rates.iter().map(|(_, rate)| rate).sum();
        let roll = self.unit() * total;
        let Some(kind) = landed(&rates, roll) else {
            return Ok(());
        };
        match kind {
            Kind::Deliver => self.deliver_any(),
            Kind::HexAdmit => self.hex_admit(),
            Kind::HexEnd => {
                let ends = self.box_ends()?;
                let total = ends.iter().map(|(_, h)| h).sum::<f64>();
                let roll = self.unit() * total;
                match landed(&ends, roll) {
                    Some(key) => self.hex_end(key),
                    None => Ok(()),
                }
            }
            Kind::Ask => {
                let runner = self.pick_runner(|r| !r.retired);
                self.ask(runner)
            }
            Kind::EndRun => {
                let ends = self.run_ends()?;
                let total = ends.iter().map(|(_, h)| h).sum::<f64>();
                let roll = self.unit() * total;
                if let Some((n, id)) = landed(&ends, roll) {
                    self.end_run(n, id);
                }
                Ok(())
            }
            Kind::Drop => {
                let runner = self.pick_runner(|r| !r.retired && r.link.up);
                self.drop_link(runner)
            }
            Kind::Partition => {
                let runner = self.pick_runner(|r| !r.retired && r.link.up);
                self.drop_link(runner)?;
                if let Some(r) = runner.and_then(|r| self.runners.get_mut(r)) {
                    r.link.partitioned = true;
                }
                Ok(())
            }
            Kind::Heal => {
                let runner = self.pick_runner(|r| !r.retired && r.link.partitioned);
                if let Some(r) = runner.and_then(|r| self.runners.get_mut(r)) {
                    r.link.partitioned = false;
                }
                Ok(())
            }
            Kind::CrashRunner => {
                let runner = self.pick_runner(|r| !r.retired && r.holder.is_some());
                self.crash_runner(runner)
            }
            Kind::RestartRunner => {
                let runner = self.pick_runner(|r| !r.retired && r.holder.is_none());
                self.restart_runner(runner)
            }
            Kind::RestartKeeper => self.restart_keeper(),
        }
    }

    /// The fault no record times whose step has come and that can strike now, the first by [`FAULTS`]' order.
    fn due_now(&mut self) -> Option<Fault> {
        let step = self.report.steps;
        let at = self
            .due
            .iter()
            .position(|(at, fault)| *at < step && self.can(*fault))?;
        (at < self.due.len()).then(|| self.due.remove(at).1)
    }

    /// Whether `fault` can strike now.
    fn can(&self, fault: Fault) -> bool {
        let keeper = self.keeper.is_some();
        match fault {
            Fault::CrashKeeper => keeper,
            Fault::CrashBeforeWrite | Fault::CrashAfterWrite => keeper && self.disk.crash.is_none(),
            Fault::LetGo => self
                .runners
                .iter()
                .any(|r| r.holder.is_some() && !r.asked.is_empty()),
            Fault::ForgetAway => keeper && self.runners.iter().any(Self::away_with_work),
            Fault::Retire => {
                self.runners.iter().filter(|r| !r.retired).count() >= 2
                    && !self.runners.iter().any(|r| r.retired)
            }
        }
    }

    /// A runner the person may take for gone while its work runs (CV6): not retired, its link down.
    fn away_with_work(r: &Runner) -> bool {
        !r.retired && !r.link.up && !r.running.is_empty()
    }

    fn strike(&mut self, fault: Fault) -> Result<(), Violation> {
        match fault {
            Fault::CrashKeeper => {
                self.crash_keeper(false);
                Ok(())
            }
            Fault::CrashBeforeWrite => {
                self.disk.crash = Some((self.disk.writes, false));
                Ok(())
            }
            Fault::CrashAfterWrite => {
                self.disk.crash = Some((self.disk.writes, true));
                Ok(())
            }
            Fault::LetGo => {
                let runner = self.pick_runner(|r| r.holder.is_some() && !r.asked.is_empty());
                self.let_go(runner)
            }
            Fault::ForgetAway => {
                let runner = self.pick_runner(Self::away_with_work);
                self.forget_away(runner)
            }
            Fault::Retire => {
                let runner = self.pick_runner(|r| !r.retired);
                self.retire(runner)
            }
        }
    }

    /// kiln's holder dials again as soon as a link ends, and again as each dial nothing answered ends (kiln's
    /// `capacity::links` and `dial_wait`): every runner whose process runs, its link down and its path open, opens a
    /// link at once while the keeper is up.
    fn redial(&mut self) {
        for n in 0..self.runners.len() {
            self.connect(Some(n));
        }
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

    /// The machine's room as its host reads it now (`Facts::node_with_owned`): every CPU, and the memory that is free
    /// or Hex's own boxes', so it lacks what the runners' processes and their work use. None is left in a dimension
    /// they use whole.
    fn room_now(&self) -> Result<Amounts, Violation> {
        let mut taken = 0u64;
        for (r, work) in self.runners.iter().zip(self.works()?) {
            taken = taken
                .checked_add(r.own)
                .and_then(|t| t.checked_add(work))
                .ok_or_else(|| stopped("the runners' memory overflows a sum"))?;
        }
        Ok(Amounts {
            mem_bytes: ROOM.mem_bytes.saturating_sub(taken),
            ..ROOM
        })
    }

    /// The room measured now ([`World::room_now`]), the most the keeper may decide in until the next it is given.
    fn measure(&mut self) -> Result<Amounts, Violation> {
        self.measured = self.room_now()?;
        Ok(self.measured)
    }

    /// Every running run's use moves, anywhere from none to its reservation's memory, a holder's work within what it
    /// reserved: memory moves at any time, so at every step, no rate picked (ROOT, 2026-11-14 (d), (f): no bound on the
    /// moves, the most adversarial).
    fn move_uses(&mut self) {
        let rng = &mut self.rng;
        let mut moved = false;
        for r in &mut self.runners {
            for (id, amount) in &r.running {
                let to = rng.range(0, amount.mem_bytes);
                moved |= r.uses.insert(*id, to) != Some(to);
            }
        }
        if moved && let Some(command) = self.command.as_mut() {
            command.moved = true;
        }
    }

    /// Hex's node is asked to admit a box of its own: its command begins with its refresh's first read ([`Command`]).
    /// The node, the keeper's host, admits nothing while the keeper is down, and serves one command at a time.
    fn hex_admit(&mut self) -> Result<(), Violation> {
        if self.keeper.is_none() || self.command.is_some() {
            return Ok(());
        }
        let leg = self.draw(oci_legs())?;
        let before = self.works()?;
        self.command = Some(Command {
            leg,
            before,
            room: None,
            moved: false,
        });
        Ok(())
    }

    /// Hex's admission under way takes its next phase: its read of free memory, then its end.
    fn command(&mut self) -> Result<(), Violation> {
        let Some(mut command) = self.command.take() else {
            return Ok(());
        };
        match command.room {
            None => {
                command.room = Some(self.room_now()?);
                self.command = Some(command);
                Ok(())
            }
            Some(room) => self.admit(command, room),
        }
    }

    /// The planted host's credit, nothing in a correct one: each joined runner's work, the smaller of its reads as the
    /// sample began and as it ends, up to the runner's reservations (CV12's withdrawn rule).
    fn credit(&self, before: &[u64]) -> Result<Amounts, Violation> {
        let mut credit = Amounts::ZERO;
        let Some(keeper) = self.keeper.as_ref() else {
            return Ok(credit);
        };
        if !self.planted(Mutant::Credit) {
            return Ok(credit);
        }
        let after = self.works()?;
        for (n, r) in self.runners.iter().enumerate() {
            if r.link.session.is_none() {
                continue;
            }
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
        Ok(credit)
    }

    /// The command's end: the room its refresh read given the keeper, the planted host's credit added; the box admitted
    /// where it fits the view's room left (or, planted, the room less Hex's own alone); then the hints the command owes.
    fn admit(&mut self, command: Command, room: Amounts) -> Result<(), Violation> {
        let credit = self.credit(&command.before)?;
        let given = room
            .checked_add(credit)
            .ok_or_else(|| stopped("the room and its credit overflow a sum"))?;
        let local = self.local()?;
        let ignores = self.planted(Mutant::LocalIgnoresForeign);
        let Some(keeper) = self.keeper.as_mut() else {
            return Ok(());
        };
        self.measured = room;
        keeper.set_room(given);
        let free = if ignores {
            keeper.room().less(local)
        } else {
            keeper.view(local).free()
        };
        let fits = command.leg.need.fits(free);
        if fits {
            keeper.local_changed();
        }
        let report = &mut self.report;
        report.samples = report.samples.saturating_add(1);
        if command.moved {
            report.moved_in_window = report.moved_in_window.saturating_add(1);
        }
        if fits {
            self.boxes.insert(self.next_box, command.leg);
            self.next_box = self.next_box.saturating_add(1);
            report.hex_started = report.hex_started.saturating_add(1);
        } else {
            report.hex_refused = report.hex_refused.saturating_add(1);
        }
        self.hints()
    }

    /// One of Hex's boxes ends: the host's command, then its hints.
    fn hex_end(&mut self, key: u64) -> Result<(), Violation> {
        if self.boxes.remove(&key).is_some()
            && let Some(keeper) = self.keeper.as_mut()
        {
            keeper.local_changed();
        }
        self.hints()
    }

    fn ask(&mut self, runner: Option<usize>) -> Result<(), Violation> {
        let leg = self.draw(kiln_legs())?;
        let early = self.planted(Mutant::StartBeforeGrant);
        let Some(r) = runner.and_then(|r| self.runners.get_mut(r)) else {
            return Ok(());
        };
        if r.retired {
            return Ok(());
        }
        let Some(holder) = r.holder.as_mut() else {
            return Ok(());
        };
        // A runner that can't reserve now takes nothing new (CV11): kiln places the run elsewhere.
        if let Ok((id, msg)) = holder.reserve(leg.need) {
            r.asked.insert(id, leg.need);
            r.lengths.insert(id, leg.runs_for);
            r.link.to_keeper.push_back(msg);
            if early {
                r.running.insert(id, leg.need);
            }
        }
        Ok(())
    }

    fn end_run(&mut self, n: usize, id: ReservationId) {
        let Some(runner) = self.runners.get_mut(n) else {
            return;
        };
        if !runner.running.contains_key(&id) {
            return;
        }
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

    /// A word delivered to the keeper: its answer goes back on the link, then the hints it owes, as the keeper's lane
    /// says them after each message a link said (`hex_quic::capacity::Server::hint`).
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
                self.hints()
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
                    r.unasked(&id);
                    report.runs_refused = report.runs_refused.saturating_add(1);
                }
                Event::Unanswered(id) => {
                    r.unasked(&id);
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

    /// The hints the keeper owes, each put on the link its session runs on.
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

    /// The keeper's process ends: every link drops with it, and Hex's admission under way with its node. Hex's boxes
    /// and the runners' runs go on.
    fn crash_keeper(&mut self, at_write: bool) {
        if self.keeper.take().is_none() {
            return;
        }
        self.command = None;
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
                        r.unasked(&id);
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
            r.all_unasked();
            if !self.case.survive {
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
        let holder = Holder::start(
            r.name.clone(),
            r.epoch,
            self.budget.reservations,
            &survivors,
        )
        .map_err(stopped)?;
        r.holder = Some(holder);
        r.own = own;
        Ok(())
    }

    /// A runner lets go of its oldest reserve waiting for an answer: abandoned, never started, and released at once
    /// where its link is up, else left out of its next sync (CV5).
    fn let_go(&mut self, runner: Option<usize>) -> Result<(), Violation> {
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
        holder.release(id, &mut out);
        r.link.to_keeper.extend(out.send);
        r.unasked(&id);
        self.report.runs_let_go = self.report.runs_let_go.saturating_add(1);
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
            r.all_unasked();
            r.all_ended();
            r.retired = true;
            r.unforgotten = true;
        }
        Ok(())
    }

    /// The person releases a runner it takes for gone, wrongly (CV6): one away from the keeper, its link down
    /// (partitioned, or its process crashed with its runs outliving it), not retired, with work still
    /// running. That work is missing from the keeper's books until the runner's next sync adopts it (CV5). A
    /// release the keeper refuses (the runner connected, or never seen) changes nothing. The release is a command of
    /// the keeper's host, its hints said after it.
    fn forget_away(&mut self, runner: Option<usize>) -> Result<(), Violation> {
        let Some(n) = runner else {
            return Ok(());
        };
        let Some(name) = self
            .runners
            .get(n)
            .filter(|r| Self::away_with_work(r))
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
                self.hints()
            }
            Err(Stop::Crashed) => {
                self.report.crashes_at_write = self.report.crashes_at_write.saturating_add(1);
                self.crash_keeper(true);
                Ok(())
            }
            Err(why) => Err(stopped(why)),
        }
    }

    /// The person releases what a retired runner held, once the keeper is up: a command of its host, its hints said
    /// after it.
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
                    self.hints()?;
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
            .chain(self.boxes.values().map(|leg| &leg.need)))
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

    /// The nemesis stops: no fault is due any more, and Hex's admission under way ends; the keeper and every runner
    /// start again where down; every partition heals and every link connects; everything said is delivered and every
    /// hint drained until nothing more is said. Then the books each side keeps must agree.
    pub fn settle(&mut self) -> Result<Report, Violation> {
        self.due.clear();
        self.disk.crash = None;
        // Its read of free memory where it hasn't made it, then its end.
        self.command()?;
        self.command()?;
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
    /// most the reservations it holds ([`reservations`]), each answered: two deliveries apiece. Once all is quiet the
    /// keeper owes each runner at most one hint, and two looks find it quiet, the first draining those hints.
    fn settle_bound(&self) -> Result<usize, Violation> {
        let overflow = || stopped("the settle's bound overflows");
        let releases = self
            .budget
            .reservations
            .checked_mul(2)
            .ok_or_else(overflow)?;
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
