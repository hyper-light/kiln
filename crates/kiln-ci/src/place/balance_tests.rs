//! Balance on the competitors' common trace (docs/needs.md §11, AUDIT P6; `benches/lb_vs.rs`,
//! `ci/bench/drivers/common.h`): N runners of C CPUs, K = round(rho·N·C) runs in flight, each
//! asking one CPU; each step one run chosen at random ends and one more is placed, the runners'
//! replies heard in a shuffled order. The cv of the runs in flight per runner, sampled after each
//! placement as the bench samples it, is held to be no worse than round robin's on the same trace.

use std::collections::HashMap;

use super::ask::{Ask, Peer, Sample};
use super::msg::{Down, ToOwner, ToRunner};
use super::owner::{Act, Cmd, Machine, Owner, Size};
use super::runner::{Admission, Init, Records, Reply, Said, Settings};
use crate::facts::RunnerFacts;
use crate::sched::needs::Needs;
use crate::sched::state::{Cpus, Resources};

const OWNER: &str = "owner";

fn splitmix(s: &mut u64) -> u64 {
    *s = s.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = *s;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// The mean over samples of the cv of the runs per runner.
#[derive(Default)]
struct Cv {
    sum: f64,
    samples: u64,
}

impl Cv {
    fn sample(&mut self, load: &[u32]) {
        let n = load.len() as f64;
        let total: f64 = load.iter().map(|l| f64::from(*l)).sum();
        if total == 0.0 {
            return;
        }
        let mean = total / n;
        let var = load.iter().map(|l| (f64::from(*l) - mean).powi(2)).sum::<f64>() / n;
        self.sum += var.sqrt() / mean;
        self.samples += 1;
    }

    fn mean(&self) -> f64 {
        if self.samples == 0 { 0.0 } else { self.sum / self.samples as f64 }
    }
}

struct Fleet {
    owner: Owner,
    runners: Vec<Admission>,
    index: HashMap<String, usize>,
    names: Vec<String>,
    net: u64,
    starts: Vec<(usize, String)>,
    /// The in-process path (words by handle), else the wire's.
    fast: bool,
    rids: Vec<u32>,
}

impl Fleet {
    fn new(n: u32, c: u64, seed: u64) -> Fleet {
        let size = Resources { cpu: Cpus::whole(c), ..Resources::default() };
        let facts = RunnerFacts { os: "linux".into(), arch: "x86_64".into(), ..RunnerFacts::default() };
        let names: Vec<String> = (0..n).map(|j| format!("r{j}")).collect();
        let settings = Settings { budget: None, overcommit_cpu: 1000, backfill: true, ..Settings::default() };
        let runners = names.iter().map(|me| Admission::new(Init { me: me.clone(), epoch: 1, facts: facts.clone(), labels: Vec::new(), sample: Sample { size, ..Sample::default() }, records: Records::new(), settings: settings.clone() }, 0)).collect();
        let mut owner = Owner::seeded(OWNER, 1, seed);
        let _ = owner.machines(names.iter().map(|name| Machine { name: name.clone(), facts: facts.clone(), labels: Vec::new(), size: Size::known(&size), epoch: 1, connected: true, places: true, idle: None, fast: false }).collect());
        let index = names.iter().cloned().enumerate().map(|(i, n)| (n, i)).collect();
        let rids = names.iter().map(|n| owner.runner_id(n)).collect();
        Fleet { owner, runners, index, names, net: seed ^ 0x6e65_7477_6f72_6b21, starts: Vec::new(), fast: false, rids }
    }

    fn index_of(&self, rid: u32) -> Option<usize> {
        self.rids.iter().position(|r| *r == rid)
    }

    /// The in-process path: commands by handle, replies by token.
    fn carry_fast(&mut self, mut cmds: Vec<Cmd>, now: i64) {
        let peer = Peer { name: OWNER, epoch: 1 };
        loop {
            let mut replies: Vec<(usize, Reply)> = Vec::new();
            for c in cmds.drain(..) {
                let mut out: Vec<Reply> = Vec::new();
                match c {
                    Cmd::Tell { to, run, msg } => {
                        let Some(i) = self.index_of(to) else { continue };
                        let Some(r) = self.runners.get_mut(i) else { continue };
                        match msg {
                            Down::Probe { ask, token } => r.probe_r(peer, ask, token, None, now, &mut out),
                            Down::Withdraw => r.withdraw_r(peer, &run, now, &mut out),
                            Down::Take { fence } => r.take_r(peer, &run, fence, now, &mut out),
                            Down::Decline { fence } => r.decline_r(peer, &run, fence, now, &mut out),
                            Down::Start { fence } => {
                                let _ = r.start_r(peer, &run, fence, now, &mut out);
                            }
                            Down::Ack => r.ack(peer, &run),
                            Down::Fast { ask, token, kernel, epoch } => {
                                let _ = r.fast_r(peer, ask, token, kernel, epoch, None, now, &mut out);
                            }
                        }
                        replies.extend(out.into_iter().map(|m| (i, m)));
                    }
                    Cmd::Go { run, to, fence } => {
                        let Some(i) = self.index_of(to) else { continue };
                        if self.owner.starting_c(&run, to, fence)
                            && let Some(r) = self.runners.get_mut(i)
                        {
                            let _ = r.start_r(peer, &run, fence, now, &mut out);
                            self.starts.push((i, run.to_string()));
                            replies.extend(out.into_iter().map(|m| (i, m)));
                        }
                    }
                    Cmd::Answer { .. } | Cmd::Fallback { .. } | Cmd::Restart { .. } => {}
                }
            }
            if replies.is_empty() {
                return;
            }
            for j in (1..replies.len()).rev() {
                let k = usize::try_from(splitmix(&mut self.net) % (j as u64 + 1)).unwrap_or(0);
                replies.swap(j, k);
            }
            for (i, m) in replies {
                let rid = self.rids.get(i).copied().unwrap_or(0);
                self.owner.heard_c(rid, m.token, &m.run, m.msg, now, &mut cmds);
            }
        }
    }

    fn carry(&mut self, mut acts: Vec<Act>, now: i64) {
        let peer = Peer { name: OWNER, epoch: 1 };
        loop {
            let mut replies: Vec<(usize, ToOwner)> = Vec::new();
            for act in acts.drain(..) {
                let mut said: Vec<Said> = Vec::new();
                match act {
                    Act::Tell { runner, msg } => {
                        let Some(&i) = self.index.get(&runner) else { continue };
                        let Some(r) = self.runners.get_mut(i) else { continue };
                        match msg {
                            ToRunner::Probe { ask } => r.probe(peer, ask, None, now, &mut said),
                            ToRunner::Withdraw { run } => r.withdraw(peer, &run, now, &mut said),
                            ToRunner::Take { run, fence } => r.take(peer, &run, fence, now, &mut said),
                            ToRunner::Decline { run, fence } => r.decline(peer, &run, fence, now, &mut said),
                            ToRunner::Start { run, fence } => {
                                let _ = r.start(peer, &run, fence, now, &mut said);
                            }
                            ToRunner::Ack { run } => r.ack(peer, &run),
                            ToRunner::Fast { ask, kernel, epoch } => {
                                let _ = r.fast(peer, ask, kernel.handle(), epoch, None, now, &mut said);
                            }
                        }
                        replies.extend(said.into_iter().map(|(_, m)| (i, m)));
                    }
                    Act::Go { run, runner, fence } => {
                        let Some(&i) = self.index.get(&runner) else { continue };
                        if self.owner.starting(&run, &runner, fence)
                            && let Some(r) = self.runners.get_mut(i)
                        {
                            let _ = r.start(peer, &run, fence, now, &mut said);
                            self.starts.push((i, run));
                            replies.extend(said.into_iter().map(|(_, m)| (i, m)));
                        }
                    }
                    Act::Answer { .. } | Act::Fallback { .. } | Act::Restart { .. } => {}
                }
            }
            if replies.is_empty() {
                return;
            }
            for j in (1..replies.len()).rev() {
                let k = usize::try_from(splitmix(&mut self.net) % (j as u64 + 1)).unwrap_or(0);
                replies.swap(j, k);
            }
            for (i, m) in replies {
                let name = self.names.get(i).cloned().unwrap_or_default();
                acts.extend(self.owner.heard(&name, m, now));
            }
        }
    }
}

/// kiln's cv and oversubscriptions on the trace, and round robin's cv on the same trace.
fn trace(n: u32, c: u64, rho: f64, steps: u32, seed: u64) -> (f64, u64, f64) {
    let (cv, over, rr, _) = trace_on(n, c, rho, steps, seed, false);
    (cv, over, rr)
}

/// The trace on the wire's words or the in-process ones; and every start, in order.
fn trace_on(n: u32, c: u64, rho: f64, steps: u32, seed: u64, fast: bool) -> (f64, u64, f64, Vec<(usize, String)>) {
    let k = (rho * f64::from(n) * c as f64).round() as usize;
    let mut f = Fleet::new(n, c, seed);
    f.fast = fast;
    let mut all_starts: Vec<(usize, String)> = Vec::new();
    let mut load = vec![0u32; n as usize];
    let mut live: Vec<(usize, String)> = Vec::new();
    let (mut dep, mut next, mut now, mut over) = (seed, 0u64, 0i64, 0u64);
    let (mut cv, mut rr_cv) = (Cv::default(), Cv::default());
    let peer = Peer { name: OWNER, epoch: 1 };
    let take_starts = |f: &mut Fleet, load: &mut Vec<u32>, live: &mut Vec<(usize, String)>, over: &mut u64, all: &mut Vec<(usize, String)>| {
        for (i, run) in std::mem::take(&mut f.starts) {
            all.push((i, run.clone()));
            if let Some(l) = load.get_mut(i) {
                if u64::from(*l) >= c {
                    *over += 1;
                }
                *l += 1;
            }
            live.push((i, run));
        }
    };
    for step in 0..(k as u32 + steps) {
        now += 1;
        if step >= k as u32 && !live.is_empty() {
            let j = usize::try_from(splitmix(&mut dep) % live.len() as u64).unwrap_or(0);
            let (i, run) = live.swap_remove(j);
            if let Some(l) = load.get_mut(i) {
                *l = l.saturating_sub(1);
            }
            if f.fast {
                let mut out = Vec::new();
                if let Some(r) = f.runners.get_mut(i) {
                    r.ended_r(peer, &run, true, now, &mut out);
                }
                let rid = f.rids.get(i).copied().unwrap_or(0);
                for m in out {
                    let mut cmds = Vec::new();
                    f.owner.heard_c(rid, m.token, &m.run, m.msg, now, &mut cmds);
                    f.carry_fast(cmds, now);
                }
            } else {
                let mut said = Vec::new();
                if let Some(r) = f.runners.get_mut(i) {
                    r.ended(peer, &run, true, now, &mut said);
                }
                let name = f.names.get(i).cloned().unwrap_or_default();
                for (_, m) in said {
                    let acts = f.owner.heard(&name, m, now);
                    f.carry(acts, now);
                }
            }
            take_starts(&mut f, &mut load, &mut live, &mut over, &mut all_starts);
        }
        next += 1;
        let ask = Ask { run: format!("b{next}"), needs: Needs { request: Resources { cpu: Cpus::whole(1), ..Resources::default() }, ..Needs::default() }, repo: "repo".into(), actor: OWNER.into(), ..Ask::default() };
        if f.fast {
            let mut cmds = Vec::new();
            f.owner.ask_c(ask, now, &mut cmds);
            f.carry_fast(cmds, now);
        } else {
            let acts = f.owner.ask(ask, now);
            f.carry(acts, now);
        }
        take_starts(&mut f, &mut load, &mut live, &mut over, &mut all_starts);
        if step >= k as u32 {
            cv.sample(&load);
        }
    }
    // Round robin on the same trace: the same departures, each placement the next runner in turn.
    let mut load = vec![0u32; n as usize];
    let mut live: Vec<usize> = Vec::new();
    let (mut dep, mut turn) = (seed, 0usize);
    for step in 0..(k as u32 + steps) {
        if step >= k as u32 && !live.is_empty() {
            let j = usize::try_from(splitmix(&mut dep) % live.len() as u64).unwrap_or(0);
            let i = live.swap_remove(j);
            if let Some(l) = load.get_mut(i) {
                *l = l.saturating_sub(1);
            }
        }
        if let Some(l) = load.get_mut(turn) {
            *l += 1;
        }
        live.push(turn);
        turn = (turn + 1) % n as usize;
        if step >= k as u32 {
            rr_cv.sample(&load);
        }
    }
    (cv.mean(), over, rr_cv.mean(), all_starts)
}

/// docs/needs.md §11, AUDIT P6: the in-process path (words by handle, replies by token, no names
/// copied) decides as the wire's does: the same runs start on the same runners, in the same order,
/// at half load and at 0.9 (where runs queue, and hints aren't given: both full, they wait).
#[test]
fn p6_the_in_process_path_places_as_the_wire_s_does() {
    for rho in [0.5, 0.9] {
        let (_, _, _, wire) = trace_on(40, 8, rho, 2_000, 7, false);
        let (_, _, _, fast) = trace_on(40, 8, rho, 2_000, 7, true);
        assert!(!wire.is_empty());
        assert_eq!(wire, fast, "rho {rho}");
    }
}

/// The power of two choices as its paper has it on the same trace (the least loaded of two drawn
/// at random, ties to the first): what kiln's pick is measured beside.
fn p2c(n: u32, c: u64, rho: f64, steps: u32, seed: u64) -> f64 {
    let k = (rho * f64::from(n) * c as f64).round() as usize;
    let mut load = vec![0u32; n as usize];
    let mut live: Vec<usize> = Vec::new();
    let (mut dep, mut pick) = (seed, seed ^ 99);
    let mut cv = Cv::default();
    for step in 0..(k as u32 + steps) {
        if step >= k as u32 && !live.is_empty() {
            let j = usize::try_from(splitmix(&mut dep) % live.len() as u64).unwrap_or(0);
            let i = live.swap_remove(j);
            if let Some(l) = load.get_mut(i) {
                *l = l.saturating_sub(1);
            }
        }
        let a = usize::try_from(splitmix(&mut pick) % u64::from(n)).unwrap_or(0);
        let b = usize::try_from(splitmix(&mut pick) % u64::from(n)).unwrap_or(0);
        let i = if load.get(a) <= load.get(b) { a } else { b };
        if let Some(l) = load.get_mut(i) {
            *l += 1;
        }
        live.push(i);
        if step >= k as u32 {
            cv.sample(&load);
        }
    }
    cv.mean()
}

/// docs/needs.md §11, AUDIT P6, the bench's balance finding: at rho = 0.5 on the bench's trace
/// (N = 100, C = 8), kiln's cv of runs per runner is no worse than round robin's on the same
/// trace, and nothing is oversubscribed (kiln's first-offer pick was 0.46 against 0.29 to 0.36).
#[test]
fn p6_balance_at_half_load_is_no_worse_than_round_robin_s() {
    for seed in [1u64, 2, 3] {
        let (cv, over, rr) = trace(100, 8, 0.5, 3_000, seed);
        assert_eq!(over, 0, "seed {seed}: oversubscribed");
        assert!(cv <= rr, "seed {seed}: kiln's cv {cv:.4} against round robin's {rr:.4} (two choices as the paper has it: {:.4})", p2c(100, 8, 0.5, 3_000, seed));
    }
}
