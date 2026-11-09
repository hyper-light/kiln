//! A decision on the uncontended path through the place core, timed before and after the
//! placement kernel became its hot path (docs/lb.md, "Adopted"; W05-R26). Not a test: run it with
//! `cargo bench -p kiln-ci --bench place_fast -- [--n N] [--blocks B] [--steps S]`, in release, on
//! a quiet core.
//!
//! An owner and `N` runners of 64 CPUs in one process, every message carried at once by handle
//! (as the in-process path carries them), no wire. Each step asks a run of one CPU, carries what
//! it says until the runner's word that it started (the decision: the owner's and the runner's
//! work both), then ends it, so the fleet stays uncontended. `probing`: runners that don't name
//! `place.fast`, placed as before (two probes, two offers, the take, the start); `kernel`: runners
//! that do (the kernel's start alone). Reported: the median over blocks of nanoseconds a
//! decision, and of a whole step (with its end).

use std::time::Instant;

use kiln_ci::facts::RunnerFacts;
use kiln_ci::place::ask::{Ask, Peer, Sample};
use kiln_ci::place::msg::Down;
use kiln_ci::place::owner::{Cmd, Machine, Owner, Size};
use kiln_ci::place::runner::{Admission, Init, Records, Reply, Settings, Start};
use kiln_ci::sched::needs::Needs;
use kiln_ci::sched::state::{Cpus, Resources};

const OWNER: &str = "owner";

struct Fleet {
    owner: Owner,
    runners: Vec<Admission>,
    rids: Vec<u32>,
    cmds: Vec<Cmd>,
    replies: Vec<(usize, Reply)>,
    out: Vec<Reply>,
    started: Option<(String, usize)>,
}

impl Fleet {
    fn new(n: usize, fast: bool) -> Fleet {
        let size = Resources { cpu: Cpus::whole(64), mem: 256 << 30, disk: 1 << 40, gpu: 0 };
        let facts = RunnerFacts { os: "linux".into(), arch: "x86_64".into(), ..RunnerFacts::default() };
        let names: Vec<String> = (0..n).map(|j| format!("r{j}")).collect();
        let runners = names
            .iter()
            .map(|me| Admission::new(Init { me: me.clone(), epoch: 1, since: None, facts: facts.clone(), labels: Vec::new(), sample: Sample { size, ..Sample::default() }, records: Records::new(), settings: Settings::default() }, 0))
            .collect();
        let mut owner = Owner::seeded(OWNER, 1, 7);
        let _ = owner.machines(names.iter().map(|name| Machine { name: name.clone(), facts: facts.clone(), labels: Vec::new(), size: Size::known(&size), epoch: 1, connected: true, places: true, syncs: false, idle: None, fast }).collect());
        let rids = names.iter().map(|n| owner.runner_id(n)).collect();
        let mut f = Fleet { owner, runners, rids, cmds: Vec::new(), replies: Vec::new(), out: Vec::new(), started: None };
        for (r, rid) in f.runners.iter().zip(&f.rids) {
            f.owner.fast_hint_c(*rid, &r.fast_word());
        }
        f
    }

    fn index_of(&self, rid: u32) -> Option<usize> {
        self.rids.iter().position(|r| *r == rid)
    }

    /// Carries what's to be said until nothing is.
    fn carry(&mut self, now: i64) {
        let peer = Peer { name: OWNER, epoch: 1 };
        while !self.cmds.is_empty() {
            for c in std::mem::take(&mut self.cmds) {
                match c {
                    Cmd::Tell { to, run, msg } => {
                        let Some(i) = self.index_of(to) else { continue };
                        let Some(r) = self.runners.get_mut(i) else { continue };
                        match msg {
                            Down::Probe { ask, token, elsewhere } => {
                                r.carried_r(peer, &ask.run, Some(&*ask), elsewhere, now, &mut self.out);
                                r.probe_r(peer, ask, token, None, now, &mut self.out);
                            }
                            Down::Withdraw => r.withdraw_r(peer, &run, now, &mut self.out),
                            Down::Take { fence } => r.take_r(peer, &run, fence, now, &mut self.out),
                            Down::Decline { fence } => r.decline_r(peer, &run, fence, now, &mut self.out),
                            Down::Ack => r.ack(peer, &run),
                            Down::Start { .. } | Down::Fast { .. } => {}
                            Down::Sync => r.sync_r(peer, now, &mut self.out),
                        }
                        self.replies.extend(self.out.drain(..).map(|m| (i, m)));
                    }
                    Cmd::Go { run, to, fence } => {
                        let Some(i) = self.index_of(to) else { continue };
                        let fast = self.owner.fast_start(&run, fence);
                        if !self.owner.starting_c(&run, to, fence) {
                            continue;
                        }
                        let Some(r) = self.runners.get_mut(i) else { continue };
                        let started = match fast {
                            Some((ask, token, kernel, epoch)) => r.fast_r(peer, ask, token, kernel, epoch, None, now, &mut self.out),
                            None => r.start_r(peer, &run, fence, now, &mut self.out),
                        };
                        if started == Some(Start::New) {
                            r.spawned_r(peer, &run, &mut self.out);
                            self.started = Some((run.to_string(), i));
                        }
                        self.replies.extend(self.out.drain(..).map(|m| (i, m)));
                    }
                    Cmd::Answer { .. } | Cmd::Fallback { .. } | Cmd::Restart { .. } => {}
                }
            }
            for (i, m) in std::mem::take(&mut self.replies) {
                let Some(rid) = self.rids.get(i).copied() else { continue };
                self.owner.heard_c(rid, m.token, &m.run, m.msg, now, &mut self.cmds);
            }
        }
    }

    /// One step: ask, decided and started (timed apart), then ended.
    fn step(&mut self, ask: Ask, now: i64) -> Result<(u64, u64), String> {
        let t0 = Instant::now();
        self.owner.ask_c(ask, now, &mut self.cmds);
        self.carry(now);
        let decided = t0.elapsed();
        let (run, i) = self.started.take().ok_or("a run that didn't start")?;
        let peer = Peer { name: OWNER, epoch: 1 };
        if let Some(r) = self.runners.get_mut(i) {
            r.ended_r(peer, &run, true, now, &mut self.out);
        }
        let rid = self.rids.get(i).copied().unwrap_or(0);
        for m in std::mem::take(&mut self.out) {
            self.owner.heard_c(rid, m.token, &m.run, m.msg, now, &mut self.cmds);
        }
        self.carry(now);
        let whole = t0.elapsed();
        Ok((u64::try_from(decided.as_nanos()).unwrap_or(u64::MAX), u64::try_from(whole.as_nanos()).unwrap_or(u64::MAX)))
    }
}

fn arg(args: &[String], name: &str, default: usize) -> usize {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn median(v: &mut [u64]) -> u64 {
    v.sort_unstable();
    v.get(v.len() / 2).copied().unwrap_or(0)
}

fn main() -> Result<(), String> {
    let args: Vec<String> = std::env::args().collect();
    let (n, blocks, steps) = (arg(&args, "--n", 16), arg(&args, "--blocks", 31), arg(&args, "--steps", 2000));
    let one = Needs { request: Resources { cpu: Cpus::whole(1), ..Resources::default() }, ..Needs::default() };
    for (name, fast) in [("probing", false), ("kernel", true)] {
        let mut f = Fleet::new(n, fast);
        let mut k = 0u64;
        let mut ask = || {
            k = k.wrapping_add(1);
            Ask { run: format!("run-{k}"), needs: one.clone(), repo: "acme/app".into(), actor: "bench".into(), ..Ask::default() }
        };
        for _ in 0..steps {
            f.step(ask(), 1)?;
        }
        let (mut dec, mut all) = (Vec::with_capacity(blocks), Vec::with_capacity(blocks));
        for _ in 0..blocks {
            let (mut d, mut w) = (0u64, 0u64);
            for _ in 0..steps {
                let (a, b) = f.step(ask(), 1)?;
                d = d.saturating_add(a);
                w = w.saturating_add(b);
            }
            let s = u64::try_from(steps.max(1)).unwrap_or(1);
            dec.push(d / s);
            all.push(w / s);
        }
        println!("{name}: n={n} decision {} ns, step with its end {} ns (median of {blocks} blocks of {steps})", median(&mut dec), median(&mut all));
    }
    Ok(())
}
