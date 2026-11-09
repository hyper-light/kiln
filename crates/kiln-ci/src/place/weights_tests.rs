//! Learned weights in the place core (docs/lb.md "Learned wait weights" and "Adopted"; LB08,
//! W05-R23): where a ticket's probes are drawn once they're declared, what the runs probed teach
//! them (Lattanzi, Lavastida, Moseley and Vassilvitskii, SODA 2020, Algorithm 1), their fall back
//! to the random draw, and that with them on or off every run starts once. Each rule with the
//! mistake planted against it caught.

use super::answer::{Answer, Count};
use super::ask::{Ask, Peer, Sample};
use super::keys::SplitMix;
use super::msg::Down;
use super::owner::{Cmd, Machine, Owner, Size};
use super::runner::{Admission, Init, Records, Reply, Settings, Start};
use super::{Mutant, planted};
use crate::facts::RunnerFacts;
use crate::lb::weights::Weights;
use crate::pace::Pace;
use crate::sched::needs;
use crate::sched::state::{Cpus, Resources};

pub(super) const OWNER: &str = "owner";
const GIB: u64 = 1 << 30;

fn facts() -> RunnerFacts {
    RunnerFacts { os: "linux".into(), arch: "x86_64".into(), ..RunnerFacts::default() }
}

fn res(cpus: u64) -> Resources {
    Resources { cpu: Cpus::whole(cpus), mem: 64 * GIB, disk: 100 * GIB, gpu: 0 }
}

pub(super) fn machine(name: &str, cpus: u64, fast: bool) -> Machine {
    Machine { name: name.into(), facts: facts(), labels: Vec::new(), size: Size::known(&res(cpus)), epoch: 1, connected: true, places: true, syncs: false, idle: None, fast }
}

pub(super) fn ask(run: &str, spelling: &str, median: Option<u64>) -> Result<Ask, String> {
    Ok(Ask { run: run.into(), needs: needs::parse(spelling)?, repo: "acme/app".into(), actor: "agent:1".into(), median, ..Ask::default() })
}

/// Runners by their CPUs, an owner of them under `settings`, every message carried at once, in
/// order (`fast_tests`' lab, with runners of different sizes and speeds): each runner paced as the
/// shell paces it (`Admission::paced`, the speed and slowness its sense measured), so its words,
/// its kernel's among them, say what it measured.
pub(super) struct Lab {
    pub(super) owner: Owner,
    runners: Vec<Admission>,
    rids: Vec<u32>,
    /// What runs now, and where.
    pub(super) running: Vec<(String, usize)>,
    /// Every start, in order.
    pub(super) starts: Vec<String>,
    pub(super) answers: Vec<(String, Answer)>,
}

impl Lab {
    pub(super) fn new(runners: &[(u64, u32, u32)], fast: bool, settings: &Settings) -> Lab {
        let mut owner = Owner::seeded(OWNER, 1, 7);
        owner.configure(settings);
        let names: Vec<String> = (0..runners.len()).map(|j| format!("r{j}")).collect();
        let admissions = names
            .iter()
            .zip(runners)
            .map(|(me, (cpus, speed, slowness))| {
                let mut a = Admission::new(Init { me: me.clone(), epoch: 1, since: None, facts: facts(), labels: Vec::new(), sample: Sample { size: res(*cpus), ..Sample::default() }, records: Records::new(), settings: Settings::default() }, 0);
                a.paced(Pace { speed: *speed, slowness: *slowness, disk_bps: 0 });
                a
            })
            .collect();
        let _ = owner.machines(names.iter().zip(runners).map(|(n, (cpus, ..))| machine(n, *cpus, fast)).collect());
        let rids = names.iter().map(|n| owner.runner_id(n)).collect();
        let mut lab = Lab { owner, runners: admissions, rids, running: Vec::new(), starts: Vec::new(), answers: Vec::new() };
        lab.hint();
        lab
    }

    pub(super) fn hint(&mut self) {
        for i in 0..self.runners.len() {
            let Some(w) = self.runners.get(i).map(Admission::fast_word) else { continue };
            if let Some(rid) = self.rids.get(i).copied() {
                self.owner.fast_hint_c(rid, &w);
            }
        }
    }

    /// Where each run started, by runner name.
    pub(super) fn went(&self) -> Vec<(String, String)> {
        self.running.iter().map(|(run, i)| (run.clone(), format!("r{i}"))).collect()
    }

    pub(super) fn ask(&mut self, a: Ask, now: i64) {
        let mut cmds = Vec::new();
        self.owner.ask_c(a, now, &mut cmds);
        self.carry(cmds, now);
    }

    pub(super) fn ask_many(&mut self, asks: Vec<Ask>, now: i64) {
        let mut cmds = Vec::new();
        self.owner.ask_many_c(asks, now, &mut cmds);
        self.carry(cmds, now);
    }

    /// The first run still running ended (its process exited): false when nothing runs.
    pub(super) fn end_one(&mut self, now: i64) -> bool {
        let Some((run, i)) = self.running.first().cloned() else { return false };
        self.running.remove(0);
        let mut out = Vec::new();
        if let Some(r) = self.runners.get_mut(i) {
            r.ended_r(Peer { name: OWNER, epoch: 1 }, &run, true, now, &mut out);
        }
        let mut cmds = Vec::new();
        self.heard(i, out, now, &mut cmds);
        self.carry(cmds, now);
        true
    }

    fn heard(&mut self, i: usize, out: Vec<Reply>, now: i64, cmds: &mut Vec<Cmd>) {
        let Some(rid) = self.rids.get(i).copied() else { return };
        for m in out {
            self.owner.heard_c(rid, m.token, &m.run, m.msg, now, cmds);
        }
    }

    pub(super) fn carry(&mut self, mut cmds: Vec<Cmd>, now: i64) {
        let peer = Peer { name: OWNER, epoch: 1 };
        let mut rounds = 0;
        while !cmds.is_empty() && rounds < 256 {
            rounds += 1;
            let mut replies: Vec<(usize, Vec<Reply>)> = Vec::new();
            for c in std::mem::take(&mut cmds) {
                let mut out = Vec::new();
                let at = match c {
                    Cmd::Tell { to, run, msg } => {
                        let Some(i) = self.rids.iter().position(|r| *r == to) else { continue };
                        let Some(r) = self.runners.get_mut(i) else { continue };
                        match msg {
                            Down::Probe { ask, token, elsewhere } => {
                                r.carried_r(peer, &ask.run, Some(&*ask), elsewhere, now, &mut out);
                                r.probe_r(peer, ask, token, None, now, &mut out);
                            }
                            Down::Withdraw => r.withdraw_r(peer, &run, now, &mut out),
                            Down::Take { fence } => r.take_r(peer, &run, fence, now, &mut out),
                            Down::Decline { fence } => r.decline_r(peer, &run, fence, now, &mut out),
                            Down::Ack => r.ack(peer, &run),
                            Down::Start { .. } | Down::Fast { .. } => {}
                            Down::Sync => r.sync_r(peer, now, &mut out),
                        }
                        Some(i)
                    }
                    Cmd::Go { run, to, fence } => {
                        let Some(i) = self.rids.iter().position(|r| *r == to) else { continue };
                        let fast = self.owner.fast_start(&run, fence);
                        if !self.owner.starting_c(&run, to, fence) {
                            continue;
                        }
                        let Some(r) = self.runners.get_mut(i) else { continue };
                        let started = match fast {
                            Some((ask, token, kernel, epoch)) => r.fast_r(peer, ask, token, kernel, epoch, None, now, &mut out),
                            None => r.start_r(peer, &run, fence, now, &mut out),
                        };
                        if started == Some(Start::New) {
                            r.spawned_r(peer, &run, &mut out);
                            self.running.push((run.to_string(), i));
                            self.starts.push(run.to_string());
                        }
                        Some(i)
                    }
                    Cmd::Answer { run, answer } => {
                        self.answers.push((run.to_string(), answer));
                        None
                    }
                    Cmd::Fallback { .. } | Cmd::Restart { .. } => None,
                };
                if let Some(i) = at {
                    replies.push((i, out));
                }
            }
            for (i, out) in replies {
                self.heard(i, out, now, &mut cmds);
            }
            self.hint();
        }
    }
}

/// Runs `f` and says whether it fails with `m` planted (caught).
pub(super) fn caught(m: Mutant, f: impl Fn() -> Result<(), String>) -> Result<(), String> {
    f()?;
    match planted(Some(m), &f) {
        Err(_) => Ok(()),
        Ok(()) => Err(format!("{m:?} was not caught")),
    }
}

/// An owner of runners of the given CPUs, none admitting kernel starts (every ask is probed).
fn owner(cpus: &[u64], learned: bool, seed: u64) -> Owner {
    let mut o = Owner::seeded(OWNER, 1, seed);
    o.configure(&Settings { learned_weights: learned, ..Settings::default() });
    let _ = o.machines(cpus.iter().enumerate().map(|(i, c)| machine(&format!("r{i}"), *c, false)).collect());
    o
}

/// The runners a run asked now is probed at, in the order drawn.
fn probed(o: &mut Owner, a: Ask) -> Vec<String> {
    let mut cmds = Vec::new();
    o.ask_c(a, 0, &mut cmds);
    cmds.iter().filter_map(|c| if let Cmd::Tell { to, msg: Down::Probe { .. }, .. } = c { Some(o.runner_name(*to).to_owned()) } else { None }).collect()
}

/// The reasons a waiting run's first answer counts, once its runners' answers are overdue.
fn first_reasons(o: &mut Owner, run: &str) -> Result<Vec<Count>, String> {
    let mut cmds = Vec::new();
    o.tick_c(3_600_000, &mut cmds);
    cmds.iter()
        .find_map(|c| match c {
            Cmd::Answer { run: r, answer: Answer::Queued { reasons, .. } } if r.as_ref() == run => Some(reasons.clone()),
            _ => None,
        })
        .ok_or_else(|| format!("{run} was told nothing: {cmds:?}"))
}

/// Off until declared: a run's probes are what Floyd's sample from the seed draws, as before the
/// weights, and its answer says they were drawn at random.
#[test]
fn lb08_learned_weights_are_off_until_declared() -> Result<(), String> {
    let cpus = [8, 8, 2, 2, 16, 4];
    for seed in 0..64 {
        let mut o = owner(&cpus, false, seed);
        let got = probed(&mut o, ask("a", "cpus=1", Some(1000))?);
        let (mut r, mut draw) = (SplitMix(seed), Vec::new());
        r.sample(6, 2, &mut draw);
        let want: Vec<String> = draw.iter().map(|i| format!("r{i}")).collect();
        if got != want {
            return Err(format!("seed {seed}: probed {got:?}, the random draw is {want:?}"));
        }
        if seed == 0 {
            let reasons = first_reasons(&mut o, "a")?;
            if !reasons.iter().any(|c| c.reason == "not probed (two drawn at random)" && c.runners == 4) {
                return Err(format!("{reasons:?}"));
            }
        }
        if o.weight("r0") != 8000 || o.fell_back().is_some() {
            return Err(format!("off, nothing is learned: weight {}, fell back {:?}", o.weight("r0"), o.fell_back()));
        }
    }
    Ok(())
}

/// Probes are drawn in proportion to the weights (their CPUs until learned), two without
/// replacement: of runners of 2, 2, 2 and 3 CPUs the last is one of the two 13/21 of the time (a
/// half, drawn uniformly). With no median nothing is learned, so the weights stand; and the answer
/// says how the two were drawn.
fn proportional() -> Result<(), String> {
    let mut o = owner(&[2, 2, 2, 3], true, 11);
    let n = 4000;
    let mut heavy = 0u32;
    for i in 0..n {
        if probed(&mut o, ask(&format!("a{i}"), "cpus=1", None)?).iter().any(|r| r == "r3") {
            heavy += 1;
        }
    }
    // 13/21 of 4,000 is 2,476, with a standard deviation of 31; a half is 2,000.
    if !(2360..=2590).contains(&heavy) {
        return Err(format!("the runner of 3 CPUs of 2, 2, 2 and 3 was probed {heavy} times of {n}"));
    }
    let reasons = first_reasons(&mut o, "a0")?;
    if !reasons.iter().any(|c| c.reason == "not probed (two drawn in proportion to their learned weights)" && c.runners == 2) {
        return Err(format!("{reasons:?}"));
    }
    Ok(())
}

#[test]
fn lb08_w05_r23_probes_are_drawn_in_proportion_to_their_learned_weights() -> Result<(), String> {
    caught(Mutant::UnweightedProbes, proportional)
}

/// What the runs probed teach is Algorithm 1's job for each (Lattanzi et al.; [`crate::lb::weights`]):
/// a run's size its CPUs times its median, over the live runners its kind suits, each with its
/// CPUs, in a fleet of the runners that place. The place core's weights and T are the kernel
/// learner's fed those jobs by hand, after every run: runner 0 of 1, 2, 4 and 8 CPUs predicted at
/// 64 times its CPU halves at once, and runs of a CPU for 1 to 5 seconds move T by what they
/// show.
fn as_the_kernel() -> Result<(), String> {
    let cpus = [1u64, 2, 4, 8];
    let mut o = owner(&cpus, true, 3);
    o.set_weight("r0", 64_000);
    let mut k = Weights::new(true);
    k.set(0, 64_000);
    let members: Vec<(u32, u64)> = cpus.iter().map(|c| *c * 1000).enumerate().map(|(i, c)| (o.runner_id(&format!("r{i}")), c)).collect();
    let whole = cpus.iter().sum::<u64>() * 1000;
    for i in 0..40u64 {
        let median = 1000 * (1 + i % 5);
        probed(&mut o, ask(&format!("a{i}"), "cpus=1", Some(median))?);
        // 1,000 thousandths of a CPU for `median` ms: thousandths of a CPU times seconds.
        k.waited(0, median, &members, whole, 4);
        for (j, (r, c)) in members.iter().enumerate() {
            let (got, want) = (o.weight(&format!("r{j}")), k.weight(*r, *c));
            if got != want {
                return Err(format!("run {i}: r{j} weighs {got}, Algorithm 1 fed the same runs {want}"));
            }
        }
        if o.weights_t() != k.t() {
            return Err(format!("run {i}: T is {:?}, Algorithm 1's {:?}", o.weights_t(), k.t()));
        }
    }
    if o.weight("r0") == 64_000 {
        return Err("runner 0's weight never halved".into());
    }
    Ok(())
}

#[test]
fn lb08_w05_r23_the_runs_probed_are_algorithm_1s_jobs() -> Result<(), String> {
    caught(Mutant::NoProbeLearn, as_the_kernel)?;
    caught(Mutant::SizeByCpu, as_the_kernel)
}

/// Predictions that aren't helping (one runner of 16 weighted 2^20 times the rest) put more than
/// 2⌈log2 m⌉ times T on it with the first run: the owner says which runner and why, and draws at
/// random from then on (runs history knows no time for teach nothing, so T stays and so does the
/// switch), each ticket drawn so saying so.
fn falls_back() -> Result<(), String> {
    let mut o = owner(&[1; 16], true, 5);
    o.set_weight("r0", 1000 << 20);
    probed(&mut o, ask("a", "cpus=1", Some(1000))?);
    let f = o.fell_back().ok_or("the owner kept predictions that weren't helping")?;
    if o.runner_name(f.runner.0) != "r0" || f.load <= f.bound {
        return Err(format!("{f:?}"));
    }
    // One of two of 16 at random an eighth of the time, not nearly always.
    let n = 400;
    let mut r0 = 0u32;
    for i in 0..n {
        if probed(&mut o, ask(&format!("b{i}"), "cpus=1", None)?).iter().any(|r| r == "r0") {
            r0 += 1;
        }
    }
    // An eighth of 400 is 50, with a standard deviation of 7; by the weights, nearly all of them.
    if r0 > 100 {
        return Err(format!("r0 was probed {r0} times of {n} after the weights fell back"));
    }
    let reasons = first_reasons(&mut o, "b0")?;
    if !reasons.iter().any(|c| c.reason == "not probed (two drawn at random: the learned weights weren't helping)" && c.runners == 14) {
        return Err(format!("{reasons:?}"));
    }
    Ok(())
}

#[test]
fn lb08_predictions_that_dont_help_fall_back_to_random_probes_and_say_why() -> Result<(), String> {
    caught(Mutant::KeepWeights, falls_back)
}

/// With learned weights on or off, on runners that take kernel starts or don't: a stream of runs
/// past the fleet's room, each started once as the runs before end, none lost.
#[test]
fn lb08_with_learned_weights_on_or_off_every_run_starts_once() -> Result<(), String> {
    for (learned, fast) in [(false, true), (true, true), (false, false), (true, false)] {
        let settings = Settings { learned_weights: learned, ..Settings::default() };
        let mut lab = Lab::new(&[(2, 0, 0), (2, 0, 0), (4, 0, 0)], fast, &settings);
        let n = 20;
        for i in 0..n {
            lab.ask(ask(&format!("a{i}"), "cpus=1", Some(1000))?, 1);
        }
        let mut ended = 0;
        while lab.end_one(2) {
            ended += 1;
            if ended > 4 * n {
                return Err(format!("learned {learned}, fast {fast}: {ended} ends and runs still going"));
            }
        }
        let mut starts = lab.starts.clone();
        starts.sort_unstable();
        starts.dedup();
        if lab.starts.len() != n || starts.len() != n || ended != n {
            return Err(format!("learned {learned}, fast {fast}: {} starts ({} distinct), {ended} ended, of {n}: {:?}", lab.starts.len(), starts.len(), lab.answers));
        }
    }
    Ok(())
}
