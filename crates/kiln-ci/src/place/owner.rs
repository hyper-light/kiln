//! The owner's side of placement (docs/reach.md §2; docs/needs.md §9 and §13): the node a run is
//! asked of owns it (Ray's ownership). There's no queue at the owner: each run is a ticket whose
//! probes wait at the runners that could take it, and the first runner with room starts it (late
//! binding: Sparrow).
//!
//! - **Asked**: a run no runner could run even idle (a constraint none meets, or more than any is
//!   known to have) is answered never at once, naming the need and the nearest runner; a size not
//!   yet measured is unknown, never too small. Otherwise every runner that could, and admits
//!   probes, is probed.
//! - **Offered**: the first offer is taken (no scan of runners: the decision is O(1)) and every
//!   other probe withdrawn, unless another runner is sure to finish it sooner: its latest start
//!   plus its predicted run there before this one's predicted end (both runners' predictions, the
//!   shell's from history, docs/needs.md E6). A runner holding the take gets the run: the asker is
//!   told it's starting there, and the shell copies its tree and starts it under that claim if
//!   [`Owner::starting`] still says so.
//! - **Started** only on the runner's word that its process started ([`ToOwner::Started`]); a
//!   claim that lapses before that is told to the asker as a correction.
//! - **Once**: a run's start goes to one runner. It goes back to waiting only on that runner's word
//!   that it didn't start it ([`ToOwner::Lapsed`]), never on its silence. A runner whose link is
//!   down is waited for, with everything it holds; only its absence from the directory, or a new
//!   epoch of its process, ends a claim there, and a run there that may have started is then lost
//!   with it.
//! - **News**: the asker hears its first answer once every runner probed has answered (or its
//!   link's retransmission timeout passed, RFC 6298), then each change of its answer as it happens:
//!   each runner's word of a place or reason changed, corrected to this node's clock by the link's
//!   measured offset (RFC 5905).
//! - **Older runners** (a rolling upgrade): a run no runner that places could start now goes to an
//!   older one the shell's own pick can start it on, idle as the directory last saw it; refused
//!   there, it waits at the runners that place.

use std::collections::{BTreeMap, BTreeSet};

use super::answer::{self, Answer, At, Count, Nearest};
use super::ask::{Ask, Peer};
use super::link::{Offset, Rtt};
use super::msg::{Dim, Estimate, Place, Reason, ToOwner, ToRunner};
use crate::facts::RunnerFacts;
use crate::sched::needs;
use crate::sched::routing::{Constraint, Op};
use crate::sched::state::{Cpus, Fence, Resources};
use crate::sched::units::Ms;

/// What a runner has, as far as the directory knows: each dimension `None` until it's measured.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Size {
    pub cpu: Option<Cpus>,
    pub mem: Option<u64>,
    pub disk: Option<u64>,
    pub gpu: Option<u32>,
}

impl Size {
    /// Every dimension of `r`, known.
    pub fn known(r: &Resources) -> Size {
        Size { cpu: Some(r.cpu), mem: Some(r.mem), disk: Some(r.disk), gpu: Some(r.gpu) }
    }
}

/// A runner as the owner's directory has it. A runner not in the directory is gone.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Machine {
    pub name: String,
    /// What constraints are checked against: its facts.
    pub facts: RunnerFacts,
    pub labels: Vec<String>,
    pub size: Size,
    /// Its process's epoch: a new one ends every claim made under an older one.
    pub epoch: u64,
    /// The link to it is up. One that's down is still there: what it holds waits for it.
    pub connected: bool,
    /// It admits probes (it announced placement). An older one doesn't: the shell places on it as
    /// before placement.
    pub places: bool,
    /// For an older runner, what's idle there as the directory last saw it (`None`: not known).
    pub idle: Option<Resources>,
}

/// What a link to a runner measures, as the shell measures it ([`super::link`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Link {
    pub rtt: Option<Rtt>,
    pub offset: Offset,
}

/// What the owner's shell is to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Act {
    /// Say this to that runner.
    Tell { runner: String, msg: ToRunner },
    /// Tell the run's asker this: its first answer, then each change as news.
    Answer { run: String, answer: Answer },
    /// Copy the run's tree to `runner`, then start it there under claim `fence` if
    /// [`Owner::starting`] still says so.
    Go { run: String, runner: String, fence: Fence },
    /// No runner that places could start it now, and older ones could (`runners`): place it as
    /// before placement, by the shell's own pick among them, and say how that went
    /// ([`Owner::fallback_started`], [`Owner::fallback_refused`]).
    Fallback { run: String, runners: Vec<String> },
    /// Say the run's start to `runner` again, under claim `fence`, as it was said (its link came
    /// back, or it started again): it starts nothing twice, and answers that it ran, how it ended,
    /// or that the claim lapsed.
    Restart { run: String, runner: String, fence: Fence },
}

/// A probed runner's last word on a ticket.
#[derive(Debug, Clone, PartialEq)]
enum Word {
    /// Probed, not answered yet.
    Asked,
    Waiting { reason: Reason, place: Place, start: Estimate, predicted: Option<Ms>, free: Resources },
    Offered,
    Never(String),
    /// Its link is down: probed again when it's back.
    Away,
    /// It offered, and the offer was declined for a runner sure to finish sooner: probed again
    /// when what's known of the others changes.
    Passed,
}

/// Where a ticket stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// Probes out, no offer taken.
    Waiting,
    /// Its runner's offer `fence` taken: waiting for the runner to hold it.
    Taking { fence: Fence },
    /// Its runner holds it: its tree is being copied there.
    Held { fence: Fence },
    /// Its start went to its runner under `fence`.
    Starting { fence: Fence },
    /// It runs on its runner, by the runner's word.
    Running { fence: Option<Fence> },
    /// An older runner is being tried by the shell's own pick.
    Fallback,
}

#[derive(Debug, Clone)]
struct Ticket {
    ask: Ask,
    stage: Stage,
    /// The runner of a stage past waiting.
    runner: Option<String>,
    /// What it got there and what was free: the offer taken.
    got: Option<(Resources, Resources)>,
    /// Each runner probed, and its last word.
    words: BTreeMap<String, Word>,
    /// Each runner known and not probed, and why.
    passed: BTreeMap<String, String>,
    /// What its asker was told last.
    told: Option<Answer>,
    /// When its first answer is due, if it hasn't been given.
    due: Option<i64>,
    /// A second offer taken: what [`super::Mutant::TakeTwo`] plants.
    #[cfg(test)]
    second: Option<(String, Fence)>,
}

/// The tickets a node owns.
#[derive(Debug, Clone, Default)]
pub struct Owner {
    me: String,
    epoch: u64,
    machines: BTreeMap<String, Machine>,
    links: BTreeMap<String, Link>,
    tickets: BTreeMap<String, Ticket>,
    /// For each runner, the runs with a word or a claim there.
    at: BTreeMap<String, BTreeSet<String>>,
    /// First answers due, by when.
    due: BTreeSet<(i64, String)>,
}

/// Why `m` could never run `ask` as asked, if that's known: a constraint it doesn't meet (in the
/// words a person reads, "not linux"), more than it's known to have, or a gang. A dimension of its
/// size not measured yet is no reason: the runner itself answers for it.
pub fn unsuited(m: &Machine, ask: &Ask) -> Option<String> {
    for src in &ask.needs.requires {
        match Constraint::parse(src) {
            Ok(c) if c.eval(&m.facts, &m.labels) => {}
            Ok(Constraint::Cmp { fact, op: Op::Eq, value }) if fact == "os" || fact == "arch" => return Some(format!("not {}", value.show())),
            Ok(Constraint::Label(l)) => return Some(format!("no label {l}")),
            Ok(_) => return Some(format!("not {src}")),
            Err(e) => return Some(e),
        }
    }
    if ask.needs.together.is_some() {
        return Some("commands can't start together yet".into());
    }
    #[cfg(test)]
    let unknown_small = super::mutant() == Some(super::Mutant::NeverOnUnknown);
    #[cfg(not(test))]
    let unknown_small = false;
    let small = |asked: u64, has: Option<u64>| match has {
        Some(h) => asked > h,
        None => unknown_small && asked > 0,
    };
    let (q, s) = (&ask.needs.request, &m.size);
    let short: Vec<String> = [
        small(q.cpu.get(), s.cpu.map(Cpus::get)).then(|| needs::cpus(s.cpu.unwrap_or_default())),
        small(q.mem, s.mem).then(|| format!("{} of memory", needs::show_size(s.mem.unwrap_or_default()))),
        small(q.disk, s.disk).then(|| format!("{} of disk", needs::show_size(s.disk.unwrap_or_default()))),
        small(u64::from(q.gpu), s.gpu.map(u64::from)).then(|| needs::gpus(s.gpu.unwrap_or_default())),
    ]
    .into_iter()
    .flatten()
    .collect();
    (!short.is_empty()).then(|| format!("too small ({})", short.join(", ")))
}

/// A reason in the words counted across runners.
fn reason_words(r: &Reason) -> String {
    match r {
        Reason::Room { short } => match short.first().map(|s| s.dim) {
            Some(Dim::Cpu) => "too few idle CPUs".into(),
            Some(Dim::Mem) => "too little free memory".into(),
            Some(Dim::Disk) => "too little free disk".into(),
            Some(Dim::Gpu) => "too few free GPUs".into(),
            None => "no room".into(),
        },
        Reason::Kept { .. } => "room kept for it".into(),
        Reason::Line => "others ahead of it".into(),
        Reason::Whole { .. } => "other kiln work on it".into(),
        Reason::Draining => "draining".into(),
        Reason::Away => "link down".into(),
    }
}

/// The lesser of two times, either known.
fn min_some(a: Option<i64>, b: Option<i64>) -> Option<i64> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.min(y)),
        (x, y) => x.or(y),
    }
}

fn later(now: i64, ms: Ms) -> i64 {
    now.saturating_add(i64::try_from(ms).unwrap_or(i64::MAX))
}

/// How a runner changed between two directories.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Change {
    Same,
    Gone,
    Restarted,
    Away,
    Back,
}

// Bookkeeping, words and answers.
impl Owner {
    /// The tickets of the node `me`, in its epoch (one past the last it took, kept on its disk).
    pub fn new(me: &str, epoch: u64) -> Owner {
        Owner { me: me.to_owned(), epoch, ..Owner::default() }
    }

    /// Who it is to a runner.
    pub fn peer(&self) -> Peer<'_> {
        Peer { name: &self.me, epoch: self.epoch }
    }

    /// What the link to `runner` measures now (its round trip, RFC 6298; its clock's offset,
    /// RFC 5905).
    pub fn link(&mut self, runner: &str, link: Link) {
        self.links.insert(runner.to_owned(), link);
    }

    fn rto(&self, runner: &str) -> Ms {
        Rtt::rto_of(self.links.get(runner).and_then(|l| l.rtt.as_ref()))
    }

    /// A time in `runner`'s clock, in this node's.
    fn owner_time(&self, runner: &str, at: i64) -> i64 {
        #[cfg(test)]
        if super::mutant() == Some(super::Mutant::SkewedClock) {
            return at;
        }
        self.links.get(runner).map_or(at, |l| l.offset.owner_time(at))
    }

    fn note(&mut self, runner: &str, run: &str) {
        self.at.entry(runner.to_owned()).or_default().insert(run.to_owned());
    }

    fn unnote(&mut self, runner: &str, run: &str) {
        if let Some(runs) = self.at.get_mut(runner) {
            runs.remove(run);
            if runs.is_empty() {
                self.at.remove(runner);
            }
        }
    }

    fn runs_at(&self, runner: &str) -> Vec<String> {
        self.at.get(runner).map(|runs| runs.iter().cloned().collect()).unwrap_or_default()
    }

    fn set_word(&mut self, run: &str, runner: &str, w: Word) {
        if let Some(t) = self.tickets.get_mut(run) {
            t.words.insert(runner.to_owned(), w);
            self.note(runner, run);
        }
    }

    fn drop_word(&mut self, run: &str, runner: &str) {
        if let Some(t) = self.tickets.get_mut(run) {
            t.words.remove(runner);
        }
        self.unnote(runner, run);
    }

    fn remove(&mut self, run: &str) -> Option<Ticket> {
        let t = self.tickets.remove(run)?;
        for r in t.words.keys() {
            self.unnote(r, run);
        }
        if let Some(d) = t.due {
            self.due.remove(&(d, run.to_owned()));
        }
        Some(t)
    }

    /// Probes `run` at `runner`.
    fn probe_at(&mut self, run: &str, runner: &str, out: &mut Vec<Act>) {
        let Some(t) = self.tickets.get(run) else { return };
        let ask = Box::new(t.ask.clone());
        self.set_word(run, runner, Word::Asked);
        out.push(Act::Tell { runner: runner.to_owned(), msg: ToRunner::Probe { ask } });
    }

    /// Withdraws every probe of `run` but at `keep` and the runners that said never.
    fn withdraw_others(&mut self, run: &str, keep: &str, out: &mut Vec<Act>) {
        let others: Vec<String> = self.tickets.get(run).map(|t| t.words.iter().filter(|(r, w)| r.as_str() != keep && !matches!(w, Word::Never(_))).map(|(r, _)| r.clone()).collect()).unwrap_or_default();
        for r in others {
            self.drop_word(run, &r);
            out.push(Act::Tell { runner: r, msg: ToRunner::Withdraw { run: run.to_owned() } });
        }
    }

    /// Probes a ticket afresh at every runner that could take it (after a claim lapsed or its
    /// runner went), a runner whose link is down marked so.
    fn probe_afresh(&mut self, run: &str, out: &mut Vec<Act>) {
        let Some(t) = self.tickets.get(run) else { return };
        let stale: Vec<String> = t.words.iter().filter(|(_, w)| !matches!(w, Word::Never(_))).map(|(r, _)| r.clone()).collect();
        let (mut probe, mut away) = (Vec::new(), Vec::new());
        for m in self.machines.values().filter(|m| m.places && unsuited(m, &t.ask).is_none() && !matches!(t.words.get(&m.name), Some(Word::Never(_)))) {
            if m.connected {
                probe.push(m.name.clone());
            } else {
                away.push(m.name.clone());
            }
        }
        for r in stale {
            self.drop_word(run, &r);
        }
        for r in away {
            self.set_word(run, &r, Word::Away);
        }
        for r in probe {
            self.probe_at(run, &r, out);
        }
    }

    /// Probes again each runner passed over for one sure to finish sooner (what's known of the
    /// others changed): news, never a timer, decides when.
    fn unpass(&mut self, run: &str, out: &mut Vec<Act>) {
        let passed: Vec<String> = self.tickets.get(run).map(|t| t.words.iter().filter(|(_, w)| **w == Word::Passed).map(|(r, _)| r.clone()).collect()).unwrap_or_default();
        for r in passed {
            if self.machines.get(&r).is_some_and(|m| m.connected) {
                self.probe_at(run, &r, out);
            }
        }
    }

    /// The queued answer, from every runner's word, its times in this node's clock.
    fn queued(&self, t: &Ticket) -> Answer {
        let mut counts: BTreeMap<String, u32> = BTreeMap::new();
        let mut place: Option<At> = None;
        let (mut soonest, mut latest): (Option<i64>, Option<i64>) = (None, None);
        let mut nearest: Option<(u64, Nearest)> = None;
        for (r, w) in &t.words {
            let why = match w {
                Word::Asked => "haven't answered".to_owned(),
                Word::Offered => "offered it".to_owned(),
                Word::Away => "link down".to_owned(),
                Word::Passed => "offered it, another sure to finish sooner".to_owned(),
                Word::Never(why) => why.clone(),
                Word::Waiting { reason, place: p, start, free, .. } => {
                    if p.lo > 0 && place.as_ref().is_none_or(|b| p.lo < b.lo) {
                        place = Some(At { lo: p.lo, hi: p.hi, runner: r.clone() });
                    }
                    soonest = min_some(soonest, start.soonest.map(|at| self.owner_time(r, at)));
                    latest = min_some(latest, start.latest.map(|at| self.owner_time(r, at)));
                    if matches!(reason, Reason::Room { .. } | Reason::Kept { .. })
                        && let Some(n) = answer::nearest(&t.ask.needs.request, free)
                    {
                        let k = answer::kept(&t.ask.needs.request, &n);
                        if nearest.as_ref().is_none_or(|(best, _)| k > *best) {
                            nearest = Some((k, Nearest { ask: n, runner: r.clone() }));
                        }
                    }
                    reason_words(reason)
                }
            };
            *counts.entry(why).or_default() += 1;
        }
        for why in t.passed.values() {
            *counts.entry(why.clone()).or_default() += 1;
        }
        let of = u32::try_from(t.words.len().saturating_add(t.passed.len())).unwrap_or(u32::MAX);
        let mut reasons: Vec<Count> = counts.into_iter().map(|(reason, runners)| Count { reason, runners }).collect();
        reasons.sort_by(|a, b| b.runners.cmp(&a.runners).then_with(|| a.reason.cmp(&b.reason)));
        Answer::Queued { of, reasons, place, start: Estimate { soonest, latest }, nearest: nearest.map(|(_, n)| n) }
    }

    /// Tells the asker `answer`, if it isn't what it was told last (only real changes are news).
    fn tell(&mut self, run: &str, answer: Answer, out: &mut Vec<Act>) {
        let Some(t) = self.tickets.get_mut(run) else { return };
        if t.told.as_ref() == Some(&answer) {
            return;
        }
        #[cfg(test)]
        if t.told.is_some() && matches!(answer, Answer::Queued { .. }) && super::mutant() == Some(super::Mutant::QuietAnswer) {
            return;
        }
        t.told = Some(answer.clone());
        out.push(Act::Answer { run: run.to_owned(), answer });
    }

    /// Tells a waiting ticket's asker where it stands now: its first answer, or news.
    fn answer(&mut self, run: &str, out: &mut Vec<Act>) {
        let Some(t) = self.tickets.get(run) else { return };
        if t.stage != Stage::Waiting {
            return;
        }
        let first = t.due.is_some() || t.told.is_none();
        let a = self.queued(t);
        if let Some(d) = t.due {
            self.due.remove(&(d, run.to_owned()));
        }
        if let Some(t) = self.tickets.get_mut(run) {
            t.due = None;
        }
        self.tell(run, a, out);
        if first {
            self.try_older_for(run, out);
        }
    }

    /// Whether every runner probed has answered.
    fn all_answered(&self, run: &str) -> bool {
        self.tickets.get(run).is_some_and(|t| t.words.values().all(|w| !matches!(w, Word::Asked)))
    }

    /// A ticket's runner's word changed what's known of it: its first answer once every runner
    /// probed has answered, news after.
    fn heard_of(&mut self, run: &str, out: &mut Vec<Act>) {
        let pending = self.tickets.get(run).is_some_and(|t| t.due.is_some());
        if !pending || self.all_answered(run) {
            self.answer(run, out);
        }
    }

    /// The older runners the shell's pick could start `ask` on now: suited to it, linked, and idle
    /// enough as the directory last saw them.
    fn older_for(&self, ask: &Ask) -> Vec<String> {
        let q = super::ask::amounts(&ask.needs.request);
        self.machines.values().filter(|m| !m.places && m.connected && unsuited(m, ask).is_none() && m.idle.is_some_and(|i| super::ask::fits(&q, &super::ask::amounts(&i)))).map(|m| m.name.clone()).collect()
    }

    /// A waiting ticket that no runner that places offered goes to an older runner idle enough for
    /// it, if there's one (a rolling upgrade: placed as before placement rather than waiting).
    fn try_older_for(&mut self, run: &str, out: &mut Vec<Act>) {
        #[cfg(test)]
        if super::mutant() == Some(super::Mutant::SkipOlder) {
            return;
        }
        let Some(t) = self.tickets.get(run) else { return };
        if t.stage != Stage::Waiting || t.words.values().any(|w| *w == Word::Offered) {
            return;
        }
        let older = self.older_for(&t.ask);
        self.fallback(run, older, out);
    }

    fn fallback(&mut self, run: &str, runners: Vec<String>, out: &mut Vec<Act>) {
        if runners.is_empty() {
            return;
        }
        if let Some(t) = self.tickets.get_mut(run) {
            t.stage = Stage::Fallback;
        }
        out.push(Act::Fallback { run: run.to_owned(), runners });
    }

    /// Never: what no runner meets, and the runner nearest to it.
    fn never(&self, ask: &Ask) -> Answer {
        if self.machines.is_empty() {
            return Answer::Never { need: "is known yet".into(), nearest: None };
        }
        let meets = |m: &Machine, src: &str| Constraint::parse(src).is_ok_and(|c| c.eval(&m.facts, &m.labels));
        let need = ask
            .needs
            .requires
            .iter()
            .find(|src| !self.machines.values().any(|m| meets(m, src)))
            .map(|src| format!("meets {src}"))
            .or_else(|| (!ask.needs.requires.is_empty() && !self.machines.values().any(|m| ask.needs.requires.iter().all(|s| meets(m, s)))).then(|| format!("meets all of {}", ask.needs.requires.join(", "))))
            .or_else(|| ask.needs.together.as_ref().map(|_| "starts commands together yet".to_owned()))
            .unwrap_or_else(|| format!("has room for {}", needs::request_words(&ask.needs.request).join(", ")));
        let met = |m: &Machine| ask.needs.requires.iter().filter(|s| meets(m, s)).count();
        let nearest = self.machines.values().max_by(|a, b| met(a).cmp(&met(b)).then_with(|| (a.size.cpu, a.size.mem).cmp(&(b.size.cpu, b.size.mem))).then_with(|| b.name.cmp(&a.name)));
        let nearest = nearest.map(|m| {
            let cpus = m.size.cpu.map_or_else(|| "CPUs not measured".to_owned(), needs::cpus);
            let mem = m.size.mem.map_or_else(|| "memory not measured".to_owned(), |b| format!("{} of memory", needs::show_size(b)));
            format!("{}, {}/{}, {cpus}, {mem}", m.name, m.facts.os, m.facts.arch)
        });
        Answer::Never { need, nearest }
    }

    /// Whether another runner is sure to finish `run` before an offer from `runner` would: its
    /// latest start (in this node's clock) plus its predicted run there, before now plus the
    /// predicted run here. Unknown predictions are never sure.
    fn sooner_elsewhere(&self, run: &str, runner: &str, predicted: Option<Ms>, now: i64) -> bool {
        let Some(here) = predicted.map(|p| later(now, p)) else { return false };
        let Some(t) = self.tickets.get(run) else { return false };
        t.words.iter().any(|(o, w)| match w {
            Word::Waiting { start: Estimate { latest: Some(l), .. }, predicted: Some(p), .. } if o.as_str() != runner => later(self.owner_time(o, *l), *p) < here,
            _ => false,
        })
    }
}

// The inputs: the directory, the asker, the runners' words, the links and the time.
impl Owner {
    /// The runners as the directory has them now. One no longer in it is gone: its claims end, and
    /// a run there that may have started is lost. One whose epoch changed started again: claims of
    /// its old epoch end, and a start sent to it is said again for its record to answer. One whose
    /// link went down is waited for; back, what it may have missed is said again. One new, or newly
    /// placing, is probed with each waiting run it could take (idle runners take waiting work), and
    /// an older one newly idle is tried for waiting runs no runner that places offered.
    pub fn machines(&mut self, ms: Vec<Machine>) -> Vec<Act> {
        let mut out = Vec::new();
        let new: BTreeMap<String, Machine> = ms.into_iter().map(|m| (m.name.clone(), m)).collect();
        let old = std::mem::replace(&mut self.machines, new);
        for (name, m) in &old {
            let change = match self.machines.get(name) {
                None => Change::Gone,
                Some(n) if n.epoch != m.epoch => Change::Restarted,
                Some(n) if m.connected && !n.connected => Change::Away,
                Some(n) if !m.connected && n.connected => Change::Back,
                Some(_) => Change::Same,
            };
            match change {
                Change::Gone => self.gone(name, &mut out),
                Change::Restarted => self.restarted(name, &mut out),
                Change::Away => self.went_away(name, &mut out),
                Change::Back => self.came_back(name, &mut out),
                Change::Same => {}
            }
        }
        let fresh: Vec<String> = self.machines.values().filter(|m| m.places && m.connected && old.get(&m.name).is_none_or(|o| !o.places || o.epoch != m.epoch)).map(|m| m.name.clone()).collect();
        if !fresh.is_empty() {
            let waiting: Vec<String> = self.tickets.iter().filter(|(_, t)| t.stage == Stage::Waiting).map(|(r, _)| r.clone()).collect();
            for run in waiting {
                for r in &fresh {
                    let probe = match (self.tickets.get(&run), self.machines.get(r)) {
                        (Some(t), Some(m)) => !matches!(t.words.get(r), Some(Word::Asked | Word::Waiting { .. } | Word::Offered | Word::Never(_))) && unsuited(m, &t.ask).is_none(),
                        _ => false,
                    };
                    if probe {
                        if let Some(t) = self.tickets.get_mut(&run) {
                            t.passed.remove(r);
                        }
                        self.probe_at(&run, r, &mut out);
                        self.heard_of(&run, &mut out);
                    }
                }
            }
        }
        let idler = self.machines.values().any(|m| !m.places && m.idle.is_some() && old.get(&m.name).is_none_or(|o| o.idle != m.idle));
        if idler {
            let waiting: Vec<String> = self.tickets.iter().filter(|(_, t)| t.stage == Stage::Waiting && t.due.is_none()).map(|(r, _)| r.clone()).collect();
            for run in waiting {
                self.try_older_for(&run, &mut out);
            }
        }
        out
    }

    /// A ticket's stage, whether `runner` holds its claim, and that runner's word on it.
    fn where_is(&self, run: &str, runner: &str) -> Option<(Stage, bool, Option<Word>)> {
        let t = self.tickets.get(run)?;
        Some((t.stage, t.runner.as_deref() == Some(runner), t.words.get(runner).cloned()))
    }

    /// `runner` is gone from the directory: what it held ends.
    fn gone(&mut self, runner: &str, out: &mut Vec<Act>) {
        for run in self.runs_at(runner) {
            let Some((stage, here, _)) = self.where_is(&run, runner) else { continue };
            match stage {
                Stage::Starting { .. } | Stage::Running { .. } if here => {
                    self.remove(&run);
                    out.push(Act::Answer { run, answer: Answer::Lost { runner: runner.to_owned(), why: "is gone from the directory, as the run started or ran there: it may have run, and it isn't started again".into() } });
                }
                Stage::Taking { .. } | Stage::Held { .. } if here => {
                    if let Some(t) = self.tickets.get_mut(&run) {
                        t.stage = Stage::Waiting;
                        t.runner = None;
                        t.got = None;
                    }
                    self.drop_word(&run, runner);
                    self.probe_afresh(&run, out);
                    self.answer(&run, out);
                }
                _ => {
                    self.drop_word(&run, runner);
                    if let Some(t) = self.tickets.get_mut(&run) {
                        t.passed.insert(runner.to_owned(), "gone".into());
                    }
                    self.unpass(&run, out);
                    self.heard_of(&run, out);
                }
            }
        }
    }

    /// `runner` started again under a new epoch: its claims of the old one end; a start sent to it
    /// is said again, for the record on its disk to answer (it ran, how it ended, or the claim
    /// lapsed); a run there under the old epoch is lost; its probes are made again.
    fn restarted(&mut self, runner: &str, out: &mut Vec<Act>) {
        let connected = self.machines.get(runner).is_some_and(|m| m.connected);
        for run in self.runs_at(runner) {
            let Some((stage, here, word)) = self.where_is(&run, runner) else { continue };
            match stage {
                Stage::Running { .. } if here => {
                    self.remove(&run);
                    out.push(Act::Answer { run, answer: Answer::Lost { runner: runner.to_owned(), why: "started again under a new epoch as the run ran there: it isn't started again".into() } });
                }
                Stage::Starting { fence } if here => {
                    if connected {
                        out.push(Act::Restart { run, runner: runner.to_owned(), fence });
                    }
                }
                Stage::Taking { .. } | Stage::Held { .. } if here => {
                    if let Some(t) = self.tickets.get_mut(&run) {
                        t.stage = Stage::Waiting;
                        t.runner = None;
                        t.got = None;
                    }
                    self.probe_afresh(&run, out);
                    self.answer(&run, out);
                }
                Stage::Waiting if connected && !matches!(word, Some(Word::Never(_))) => {
                    self.probe_at(&run, runner, out);
                    self.heard_of(&run, out);
                }
                _ => {}
            }
        }
    }

    /// The link to `runner` went down: it's still there, and what it holds waits for it.
    fn went_away(&mut self, runner: &str, out: &mut Vec<Act>) {
        #[cfg(test)]
        if super::mutant() == Some(super::Mutant::LostOnDisconnect) {
            self.gone(runner, out);
            return;
        }
        for run in self.runs_at(runner) {
            let Some((stage, here, word)) = self.where_is(&run, runner) else { continue };
            match stage {
                Stage::Waiting if !matches!(word, Some(Word::Never(_))) => {
                    self.set_word(&run, runner, Word::Away);
                    self.unpass(&run, out);
                    self.heard_of(&run, out);
                }
                #[cfg(test)]
                Stage::Starting { .. } if here && super::mutant() == Some(super::Mutant::ReplaceOnSilence) => {
                    if let Some(t) = self.tickets.get_mut(&run) {
                        t.stage = Stage::Waiting;
                        t.runner = None;
                    }
                    self.drop_word(&run, runner);
                    self.probe_afresh(&run, out);
                }
                _ => {
                    let _ = here;
                }
            }
        }
    }

    /// The link to `runner` is back: what it may have missed is said again: a waiting ticket's
    /// probe (answered as it stands), a take not yet held, and a start (said again, it starts
    /// nothing twice).
    fn came_back(&mut self, runner: &str, out: &mut Vec<Act>) {
        for run in self.runs_at(runner) {
            let Some((stage, here, word)) = self.where_is(&run, runner) else { continue };
            match stage {
                Stage::Waiting if matches!(word, Some(Word::Away | Word::Asked | Word::Passed)) => {
                    self.probe_at(&run, runner, out);
                    self.heard_of(&run, out);
                }
                Stage::Taking { fence } if here => out.push(Act::Tell { runner: runner.to_owned(), msg: ToRunner::Take { run, fence } }),
                Stage::Starting { fence } if here => out.push(Act::Restart { run, runner: runner.to_owned(), fence }),
                _ => {}
            }
        }
    }

    /// The link to `runner` went down (the shell's word, between directories).
    pub fn away(&mut self, runner: &str) -> Vec<Act> {
        let mut out = Vec::new();
        if let Some(m) = self.machines.get_mut(runner)
            && m.connected
        {
            m.connected = false;
            self.went_away(runner, &mut out);
        }
        out
    }

    /// The link to `runner` is back (the shell's word, between directories).
    pub fn back(&mut self, runner: &str) -> Vec<Act> {
        let mut out = Vec::new();
        if let Some(m) = self.machines.get_mut(runner)
            && !m.connected
        {
            m.connected = true;
            self.came_back(runner, &mut out);
        }
        out
    }

    /// A run is asked of this node: answered never at once if no runner could run it even idle,
    /// handed to the older pick if only older runners could, else probed at every runner that
    /// could and places; its first answer comes once each has answered, or by the last of their
    /// links' timeouts.
    pub fn ask(&mut self, ask: Ask, now: i64) -> Vec<Act> {
        let mut out = Vec::new();
        let run = ask.run.clone();
        if self.tickets.contains_key(&run) {
            return out;
        }
        let mut passed = BTreeMap::new();
        let (mut probe, mut away, mut older) = (Vec::new(), Vec::new(), Vec::new());
        for m in self.machines.values() {
            match unsuited(m, &ask) {
                Some(why) => {
                    passed.insert(m.name.clone(), why);
                }
                None if !m.places => {
                    older.push(m.name.clone());
                    passed.insert(m.name.clone(), "an older runner, without placement".into());
                }
                None if !m.connected => away.push(m.name.clone()),
                None => probe.push(m.name.clone()),
            }
        }
        if probe.is_empty() && away.is_empty() && older.is_empty() {
            out.push(Act::Answer { run, answer: self.never(&ask) });
            return out;
        }
        let t = Ticket {
            ask,
            stage: Stage::Waiting,
            runner: None,
            got: None,
            words: BTreeMap::new(),
            passed,
            told: None,
            due: None,
            #[cfg(test)]
            second: None,
        };
        self.tickets.insert(run.clone(), t);
        for r in &away {
            self.set_word(&run, r, Word::Away);
        }
        if probe.is_empty() && away.is_empty() {
            self.fallback(&run, older, &mut out);
            return out;
        }
        let mut due: Option<i64> = None;
        for r in &probe {
            self.probe_at(&run, r, &mut out);
            let at = later(now, self.rto(r));
            due = Some(due.map_or(at, |d| d.max(at)));
        }
        match due {
            Some(d) => {
                if let Some(t) = self.tickets.get_mut(&run) {
                    t.due = Some(d);
                }
                self.due.insert((d, run));
            }
            // Only runners whose link is down could take it: answered so now.
            None => self.answer(&run, &mut out),
        }
        out
    }

    /// A runner's word about a run.
    pub fn heard(&mut self, runner: &str, msg: ToOwner, now: i64) -> Vec<Act> {
        let mut out = Vec::new();
        let run = msg.run().to_owned();
        let r = runner.to_owned();
        let Some(t) = self.tickets.get(&run) else {
            // Its ticket is gone (ended, cancelled, answered never): an offer's room goes back, a
            // probe still waiting there is withdrawn, and an end is acknowledged.
            match msg {
                ToOwner::Offer { fence, .. } | ToOwner::Held { fence, .. } => out.push(Act::Tell { runner: r, msg: ToRunner::Decline { run, fence } }),
                ToOwner::Waiting { .. } => out.push(Act::Tell { runner: r, msg: ToRunner::Withdraw { run } }),
                ToOwner::Ended { .. } => out.push(Act::Tell { runner: r, msg: ToRunner::Ack { run } }),
                _ => {}
            }
            return out;
        };
        let stage = t.stage;
        let here = t.runner.as_deref() == Some(runner);
        let probed = t.words.contains_key(runner);
        let got = t.got;
        #[cfg(test)]
        let second = t.second.as_ref().is_some_and(|(s, _)| s == runner);
        match msg {
            ToOwner::Offer { fence, got: g, free, predicted, .. } => match stage {
                Stage::Waiting => {
                    if self.sooner_elsewhere(&run, runner, predicted, now) {
                        // Another runner is sure to finish it sooner: this one's room goes back, and
                        // it's probed again when what's known of the others changes.
                        out.push(Act::Tell { runner: r, msg: ToRunner::Decline { run: run.clone(), fence } });
                        self.set_word(&run, runner, Word::Passed);
                        self.heard_of(&run, &mut out);
                        return out;
                    }
                    if let Some(t) = self.tickets.get_mut(&run) {
                        t.stage = Stage::Taking { fence };
                        t.runner = Some(r.clone());
                        t.got = Some((g, free));
                    }
                    out.push(Act::Tell { runner: r.clone(), msg: ToRunner::Take { run: run.clone(), fence } });
                    self.withdraw_others(&run, runner, &mut out);
                    self.set_word(&run, runner, Word::Offered);
                    if let Some(d) = self.tickets.get_mut(&run).and_then(|t| t.due.take()) {
                        self.due.remove(&(d, run.clone()));
                    }
                }
                // An older runner is being tried: the offer lapses back into that runner's line.
                Stage::Fallback => {}
                Stage::Taking { fence: f } | Stage::Held { fence: f } | Stage::Starting { fence: f } | Stage::Running { fence: Some(f) } if here && f == fence => {}
                #[cfg(test)]
                _ if super::mutant() == Some(super::Mutant::TakeTwo) && self.tickets.get(&run).is_some_and(|t| t.second.is_none()) => {
                    if let Some(t) = self.tickets.get_mut(&run) {
                        t.second = Some((r.clone(), fence));
                    }
                    out.push(Act::Tell { runner: r, msg: ToRunner::Take { run, fence } });
                }
                _ => out.push(Act::Tell { runner: r, msg: ToRunner::Decline { run, fence } }),
            },
            ToOwner::Held { fence, .. } => match stage {
                Stage::Taking { fence: f } if f == fence && here => {
                    if let Some(t) = self.tickets.get_mut(&run) {
                        t.stage = Stage::Held { fence };
                    }
                    out.push(Act::Go { run: run.clone(), runner: r.clone(), fence });
                    let (g, free) = got.unwrap_or_default();
                    #[cfg(test)]
                    if super::mutant() == Some(super::Mutant::StartedAtHeld) {
                        self.tell(&run, Answer::Started { runner: r, got: g, why: format!("room for it there: {} free", words(&free)) }, &mut out);
                        return out;
                    }
                    let _ = free;
                    self.tell(&run, Answer::Starting { runner: r, got: g }, &mut out);
                }
                Stage::Held { fence: f } | Stage::Starting { fence: f } if f == fence && here => {}
                #[cfg(test)]
                _ if second => {
                    if let Some(t) = self.tickets.get_mut(&run) {
                        t.second = None;
                        t.stage = Stage::Held { fence };
                        t.runner = Some(r.clone());
                    }
                    out.push(Act::Go { run, runner: r, fence });
                }
                _ => out.push(Act::Tell { runner: r, msg: ToRunner::Decline { run, fence } }),
            },
            // The runner's word that the process started: the one on which the asker is told so.
            ToOwner::Started { fence, .. } => {
                if matches!(stage, Stage::Starting { fence: f } | Stage::Held { fence: f } if f == fence) && here {
                    if let Some(t) = self.tickets.get_mut(&run) {
                        t.stage = Stage::Running { fence: Some(fence) };
                    }
                    let (g, free) = got.unwrap_or_default();
                    self.tell(&run, Answer::Started { runner: r, got: g, why: format!("room for it there: {} free", words(&free)) }, &mut out);
                }
            }
            // It started there before: it runs there, and nowhere else.
            ToOwner::Ran { .. } => {
                if matches!(stage, Stage::Running { .. }) {
                    return out;
                }
                if let (Stage::Taking { fence } | Stage::Held { fence }, false) = (stage, here)
                    && let Some(w) = self.tickets.get(&run).and_then(|t| t.runner.clone())
                {
                    self.drop_word(&run, &w);
                    out.push(Act::Tell { runner: w, msg: ToRunner::Decline { run: run.clone(), fence } });
                }
                if let Some(t) = self.tickets.get_mut(&run) {
                    t.stage = Stage::Running { fence: None };
                    t.runner = Some(r.clone());
                }
                self.withdraw_others(&run, runner, &mut out);
                self.set_word(&run, runner, Word::Offered);
                if let Some(d) = self.tickets.get_mut(&run).and_then(|t| t.due.take()) {
                    self.due.remove(&(d, run.clone()));
                }
                let (g, free) = got.unwrap_or_default();
                self.tell(&run, Answer::Started { runner: r, got: g, why: format!("it started there before ({} free as it was offered)", words(&free)) }, &mut out);
            }
            ToOwner::Ended { ok, .. } => {
                self.withdraw_others(&run, runner, &mut out);
                self.remove(&run);
                out.push(Act::Answer { run: run.clone(), answer: Answer::Ended { runner: r.clone(), ok } });
                out.push(Act::Tell { runner: r, msg: ToRunner::Ack { run } });
            }
            ToOwner::Waiting { reason, place, start, predicted, free, .. } => {
                if stage == Stage::Waiting && probed {
                    self.set_word(&run, runner, Word::Waiting { reason, place, start, predicted, free });
                    self.unpass(&run, &mut out);
                    self.heard_of(&run, &mut out);
                }
            }
            ToOwner::Never { why, .. } => {
                if probed {
                    self.set_word(&run, runner, Word::Never(why));
                }
                if stage != Stage::Waiting {
                    return out;
                }
                let (all_never, older) = match self.tickets.get(&run) {
                    Some(t) => (!t.words.is_empty() && t.words.values().all(|w| matches!(w, Word::Never(_))), self.machines.values().filter(|m| !m.places && unsuited(m, &t.ask).is_none()).map(|m| m.name.clone()).collect::<Vec<_>>()),
                    None => (false, Vec::new()),
                };
                if all_never && !older.is_empty() {
                    self.fallback(&run, older, &mut out);
                } else if all_never {
                    let Some(t) = self.remove(&run) else { return out };
                    let need = t.words.values().filter_map(|w| if let Word::Never(why) = w { Some(why.clone()) } else { None }).collect::<BTreeSet<_>>().into_iter().collect::<Vec<_>>().join("; ");
                    let nearest = match self.never(&t.ask) {
                        Answer::Never { nearest, .. } => nearest,
                        _ => None,
                    };
                    out.push(Act::Answer { run, answer: Answer::Never { need: format!("takes it as asked ({need})"), nearest } });
                } else {
                    self.unpass(&run, &mut out);
                    self.heard_of(&run, &mut out);
                }
            }
            // The runner's word that the claim isn't live there and the run wasn't started under
            // it: the one word that sends a ticket past waiting back to waiting. The asker, told it
            // was starting there, is told where it stands instead.
            ToOwner::Lapsed { fence, .. } => {
                if matches!(stage, Stage::Taking { fence: f } | Stage::Held { fence: f } | Stage::Starting { fence: f } if f == fence) && here {
                    if let Some(t) = self.tickets.get_mut(&run) {
                        t.stage = Stage::Waiting;
                        t.runner = None;
                        t.got = None;
                    }
                    self.probe_afresh(&run, &mut out);
                    self.answer(&run, &mut out);
                }
            }
        }
        out
    }

    /// The shell is about to start a run on `runner` under `fence`, its tree copied: whether it
    /// still may (its runner holds it under that claim). Once this says yes, the run's start has
    /// gone to that runner, and goes nowhere else unless it says it never started it.
    pub fn starting(&mut self, run: &str, runner: &str, fence: Fence) -> bool {
        match self.tickets.get_mut(run) {
            Some(t) if t.stage == (Stage::Held { fence }) && t.runner.as_deref() == Some(runner) => {
                t.stage = Stage::Starting { fence };
                true
            }
            _ => false,
        }
    }

    /// The asker doesn't want it any more: its probes are withdrawn and a claim it took declined.
    /// A run starting or running is stopped by the shell, as any run is.
    pub fn cancel(&mut self, run: &str) -> Vec<Act> {
        let mut out = Vec::new();
        let Some(t) = self.remove(run) else { return out };
        if let (Stage::Taking { fence } | Stage::Held { fence }, Some(r)) = (t.stage, t.runner.as_ref()) {
            out.push(Act::Tell { runner: r.clone(), msg: ToRunner::Decline { run: run.to_owned(), fence } });
        }
        for (r, w) in &t.words {
            if t.runner.as_ref() != Some(r) && !matches!(w, Word::Never(_)) {
                out.push(Act::Tell { runner: r.clone(), msg: ToRunner::Withdraw { run: run.to_owned() } });
            }
        }
        out
    }

    /// The shell's pick started the run on an older runner: it's followed as before placement,
    /// and its probes are withdrawn.
    pub fn fallback_started(&mut self, run: &str) -> Vec<Act> {
        let mut out = Vec::new();
        if self.tickets.get(run).is_some_and(|t| t.stage == Stage::Fallback) {
            self.withdraw_others(run, "", &mut out);
            self.remove(run);
        }
        out
    }

    /// The shell's pick couldn't start it on an older runner: it waits at the runners that place,
    /// its probes still in their lines (offers it let lapse meanwhile went back to their places).
    pub fn fallback_refused(&mut self, run: &str) -> Vec<Act> {
        let mut out = Vec::new();
        if let Some(t) = self.tickets.get_mut(run)
            && t.stage == Stage::Fallback
        {
            t.stage = Stage::Waiting;
            if t.words.is_empty() {
                // Only older runners could take it: it waits for one, or for a runner that places.
                self.answer(run, &mut out);
            } else {
                self.heard_of(run, &mut out);
            }
        }
        out
    }

    /// The time: first answers due (a runner probed hasn't answered within its link's timeout)
    /// are given with what's known.
    pub fn tick(&mut self, now: i64) -> Vec<Act> {
        let mut out = Vec::new();
        while let Some((at, run)) = self.due.first().cloned() {
            if at > now {
                break;
            }
            self.due.remove(&(at, run.clone()));
            if let Some(t) = self.tickets.get_mut(&run) {
                t.due = None;
            }
            self.answer(&run, &mut out);
        }
        out
    }

    /// When it's to be told the time again ([`Self::tick`]): the soonest first answer due.
    pub fn timer(&self) -> Option<i64> {
        self.due.first().map(|(at, _)| *at)
    }

    /// Where a run's ticket stands, in a word, and its runner, if it has one.
    pub fn stage(&self, run: &str) -> Option<(&'static str, Option<&str>)> {
        self.tickets.get(run).map(|t| {
            let s = match t.stage {
                Stage::Waiting => "waiting",
                Stage::Taking { .. } => "taking",
                Stage::Held { .. } => "held",
                Stage::Starting { .. } => "starting",
                Stage::Running { .. } => "running",
                Stage::Fallback => "fallback",
            };
            (s, t.runner.as_deref())
        })
    }

    /// What a run's asker was told last.
    pub fn told(&self, run: &str) -> Option<&Answer> {
        self.tickets.get(run).and_then(|t| t.told.as_ref())
    }

    /// What a waiting run's asker would be told now, once its first answer has been given (for
    /// `needs.explain`, and the property that every waiting asker hears each change).
    pub fn answer_now(&self, run: &str) -> Option<Answer> {
        let t = self.tickets.get(run).filter(|t| t.stage == Stage::Waiting && t.due.is_none() && t.told.is_some())?;
        Some(self.queued(t))
    }

    /// Its tickets' runs.
    pub fn runs(&self) -> Vec<String> {
        self.tickets.keys().cloned().collect()
    }
}

/// An amount in words: "12 CPUs, 14 GiB".
fn words(r: &Resources) -> String {
    let mut w = vec![needs::cpus(r.cpu), needs::show_size(r.mem)];
    if r.gpu > 0 {
        w.push(needs::gpus(r.gpu));
    }
    w.join(", ")
}
