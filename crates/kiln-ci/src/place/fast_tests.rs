//! The placement kernel as the place core's hot path (docs/lb.md, "Adopted"; W05-R03, W05-R04,
//! W05-R16, W05-R23): an ordinary ask a member has room for starts at once by the kernel's
//! decision, with no probe; a contended one is probed as before, and waits in the runners' lines;
//! a kernel start declined when no other member has room goes to probing; and a run takes its
//! room once in its runner's ledger, whichever path admitted it.

use super::answer::Answer;
use super::ask::{Ask, Peer, Sample};
use super::msg::{Down, Up};
use super::owner::{Cmd, Machine, Owner, Size};
use super::runner::{Admission, Init, Records, Reply, Settings, Start};
use crate::facts::RunnerFacts;
use crate::sched::needs;
use crate::sched::state::{Cpus, Resources};

const OWNER: &str = "owner";
const GIB: u64 = 1 << 30;

fn ask(run: &str, spelling: &str) -> Result<Ask, String> {
    Ok(Ask { run: run.into(), needs: needs::parse(spelling)?, repo: "acme/app".into(), actor: "agent:1".into(), ..Ask::default() })
}

/// An owner and its runners, every message carried at once, in order; the runners' hints passed
/// on after each round, as the server passes them (unless `hints` is off).
struct Lab {
    owner: Owner,
    runners: Vec<Admission>,
    rids: Vec<u32>,
    hints: bool,
    /// What the owner said, in order: its kind and to which runner.
    said: Vec<(&'static str, usize)>,
    answers: Vec<(String, Answer)>,
    /// The runs whose process started, and where.
    started: Vec<(String, usize)>,
}

fn kind(d: &Down) -> &'static str {
    match d {
        Down::Probe { .. } => "probe",
        Down::Withdraw => "withdraw",
        Down::Take { .. } => "take",
        Down::Decline { .. } => "decline",
        Down::Start { .. } => "start",
        Down::Ack => "ack",
        Down::Fast { .. } => "fast",
        Down::Sync => "sync",
    }
}

impl Lab {
    fn new(n: usize, cpus: u64, fast: bool) -> Lab {
        let size = Resources { cpu: Cpus::whole(cpus), mem: 64 * GIB, disk: 100 * GIB, gpu: 0 };
        let facts = RunnerFacts { os: "linux".into(), arch: "x86_64".into(), ..RunnerFacts::default() };
        let names: Vec<String> = (0..n).map(|j| format!("r{j}")).collect();
        let runners = names
            .iter()
            .map(|me| Admission::new(Init { me: me.clone(), epoch: 1, since: None, facts: facts.clone(), labels: Vec::new(), sample: Sample { size, ..Sample::default() }, records: Records::new(), settings: Settings::default() }, 0))
            .collect();
        let mut owner = Owner::seeded(OWNER, 1, 7);
        let _ = owner.machines(names.iter().map(|name| Machine { name: name.clone(), facts: facts.clone(), labels: Vec::new(), size: Size::known(&size), epoch: 1, connected: true, places: true, syncs: false, idle: None, fast }).collect());
        let rids = names.iter().map(|n| owner.runner_id(n)).collect();
        let mut lab = Lab { owner, runners, rids, hints: true, said: Vec::new(), answers: Vec::new(), started: Vec::new() };
        lab.hint();
        lab
    }

    fn hint(&mut self) {
        for (r, rid) in self.runners.iter().zip(&self.rids) {
            self.owner.fast_hint_c(*rid, &r.fast_word());
        }
    }

    fn index_of(&self, rid: u32) -> Option<usize> {
        self.rids.iter().position(|r| *r == rid)
    }

    fn ask(&mut self, a: Ask, now: i64) {
        let mut cmds = Vec::new();
        self.owner.ask_c(a, now, &mut cmds);
        self.carry(cmds, now);
    }

    fn cancel(&mut self, run: &str, now: i64) {
        let mut cmds = Vec::new();
        self.owner.cancel_c(run, &mut cmds);
        self.carry(cmds, now);
    }

    /// Run `run` on runner `i` ended (its process exited).
    fn end(&mut self, i: usize, run: &str, now: i64) {
        let mut out = Vec::new();
        if let Some(r) = self.runners.get_mut(i) {
            r.ended_r(Peer { name: OWNER, epoch: 1 }, run, true, now, &mut out);
        }
        self.started.retain(|(x, j)| !(x == run && *j == i));
        let mut cmds = Vec::new();
        self.heard(i, out, now, &mut cmds);
        self.carry(cmds, now);
    }

    fn heard(&mut self, i: usize, out: Vec<Reply>, now: i64, cmds: &mut Vec<Cmd>) {
        let Some(rid) = self.rids.get(i).copied() else { return };
        for m in out {
            self.owner.heard_c(rid, m.token, &m.run, m.msg, now, cmds);
        }
    }

    fn carry(&mut self, mut cmds: Vec<Cmd>, now: i64) {
        let peer = Peer { name: OWNER, epoch: 1 };
        let mut rounds = 0;
        while !cmds.is_empty() && rounds < 64 {
            rounds += 1;
            let mut replies: Vec<(usize, Vec<Reply>)> = Vec::new();
            for c in std::mem::take(&mut cmds) {
                let mut out = Vec::new();
                match c {
                    Cmd::Tell { to, run, msg } => {
                        let Some(i) = self.index_of(to) else { continue };
                        self.said.push((kind(&msg), i));
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
                            // Starts are the shell's, after its tree is copied: below, on Go.
                            Down::Start { .. } | Down::Fast { .. } => {}
                            Down::Sync => r.sync_r(peer, now, &mut out),
                        }
                    }
                    Cmd::Go { run, to, fence } => {
                        let Some(i) = self.index_of(to) else { continue };
                        let fast = self.owner.fast_start(&run, fence);
                        if !self.owner.starting_c(&run, to, fence) {
                            continue;
                        }
                        let Some(r) = self.runners.get_mut(i) else { continue };
                        let started = match fast {
                            Some((ask, token, kernel, epoch)) => {
                                self.said.push(("fast", i));
                                r.fast_r(peer, ask, token, kernel, epoch, None, now, &mut out)
                            }
                            None => {
                                self.said.push(("start", i));
                                r.start_r(peer, &run, fence, now, &mut out)
                            }
                        };
                        if started == Some(Start::New) {
                            r.spawned_r(peer, &run, &mut out);
                            self.started.push((run.to_string(), i));
                        }
                    }
                    Cmd::Answer { run, answer } => self.answers.push((run.to_string(), answer)),
                    Cmd::Fallback { .. } | Cmd::Restart { .. } => {}
                }
                if let Some(i) = self.said.last().map(|(_, i)| *i) {
                    replies.push((i, out));
                }
            }
            for (i, out) in replies {
                self.heard(i, out, now, &mut cmds);
            }
            if self.hints {
                self.hint();
            }
        }
    }

    fn count(&self, what: &str) -> usize {
        self.said.iter().filter(|(k, _)| *k == what).count()
    }

    fn last(&self, run: &str) -> Option<&Answer> {
        self.answers.iter().rev().find(|(r, _)| r == run).map(|(_, a)| a)
    }

    /// Room for a new run at runner `i` as its kernel ledger says it, in whole CPUs.
    fn fast_cpus(&self, i: usize) -> u64 {
        self.runners.get(i).map_or(0, |r| r.fast_word().room[0] / 1000)
    }

    /// What runner `i`'s own ledger holds, in whole CPUs.
    fn held_cpus(&self, i: usize) -> u64 {
        self.runners.get(i).map_or(0, |r| r.room().1[0] / 1000)
    }
}

#[test]
fn w05_r23_an_uncontended_ask_starts_by_the_kernel_with_no_probe() -> Result<(), String> {
    let mut lab = Lab::new(4, 8, true);
    lab.ask(ask("a", "cpus=2")?, 1);
    if lab.count("probe") != 0 || lab.count("fast") != 1 || lab.count("start") != 0 {
        return Err(format!("said {:?}", lab.said));
    }
    let [(run, i)] = lab.started.as_slice() else { return Err(format!("started {:?}", lab.started)) };
    if run != "a" || !matches!(lab.last("a"), Some(Answer::Started { .. })) {
        return Err(format!("answers {:?}", lab.answers));
    }
    // Its room taken once, in both of its runner's ledgers.
    if lab.held_cpus(*i) != 2 || lab.fast_cpus(*i) != 6 {
        return Err(format!("held {} fast room {}", lab.held_cpus(*i), lab.fast_cpus(*i)));
    }
    let i = *i;
    lab.end(i, "a", 2);
    if !matches!(lab.last("a"), Some(Answer::Ended { ok: true, .. })) || lab.held_cpus(i) != 0 || lab.fast_cpus(i) != 8 {
        return Err(format!("after its end: {:?}, held {} room {}", lab.last("a"), lab.held_cpus(i), lab.fast_cpus(i)));
    }
    // Its record went with the owner's word that it heard the end.
    if lab.count("ack") != 1 {
        return Err(format!("said {:?}", lab.said));
    }
    Ok(())
}

#[test]
fn w05_r03_a_runner_that_does_not_name_the_kernel_is_probed_as_before() -> Result<(), String> {
    let mut lab = Lab::new(2, 8, false);
    lab.ask(ask("a", "cpus=2")?, 1);
    if lab.count("fast") != 0 || lab.count("probe") != 2 || lab.count("start") != 1 {
        return Err(format!("said {:?}", lab.said));
    }
    if !matches!(lab.last("a"), Some(Answer::Started { .. })) {
        return Err(format!("answers {:?}", lab.answers));
    }
    Ok(())
}

#[test]
fn w05_r03_a_run_takes_its_room_once_whichever_path_admitted_it() -> Result<(), String> {
    let mut lab = Lab::new(1, 8, true);
    lab.ask(ask("a", "cpus=3")?, 1);
    lab.ask(ask("b", "cpus=5")?, 2);
    if lab.count("fast") != 2 || lab.count("probe") != 0 || lab.held_cpus(0) != 8 || lab.fast_cpus(0) != 0 {
        return Err(format!("said {:?}, held {}, room {}", lab.said, lab.held_cpus(0), lab.fast_cpus(0)));
    }
    // Contended: no member has room, so it's probed and waits in the runner's line.
    lab.ask(ask("c", "cpus=1")?, 3);
    if lab.count("probe") != 1 || !matches!(lab.last("c"), Some(Answer::Queued { .. })) {
        return Err(format!("said {:?}, answers {:?}", lab.said, lab.answers));
    }
    // An end frees room: the line's first takes it by probing's claim, the kernel's room what is
    // left once nothing waits.
    lab.end(0, "a", 4);
    if !lab.started.iter().any(|(r, _)| r == "c") || lab.count("start") != 1 {
        return Err(format!("started {:?}, said {:?}", lab.started, lab.said));
    }
    if lab.held_cpus(0) != 6 || lab.fast_cpus(0) != 2 {
        return Err(format!("held {} room {}", lab.held_cpus(0), lab.fast_cpus(0)));
    }
    // The kernel decides again once there's room and nothing waits.
    lab.ask(ask("d", "cpus=2")?, 5);
    if lab.count("fast") != 3 || lab.held_cpus(0) != 8 || lab.fast_cpus(0) != 0 {
        return Err(format!("said {:?}, held {} room {}", lab.said, lab.held_cpus(0), lab.fast_cpus(0)));
    }
    Ok(())
}

#[test]
fn w05_r03_while_runs_wait_in_a_line_the_kernel_starts_nothing_past_them() -> Result<(), String> {
    let mut lab = Lab::new(1, 4, true);
    lab.ask(ask("a", "cpus=3")?, 1);
    lab.ask(ask("big", "cpus=4")?, 2);
    if !matches!(lab.last("big"), Some(Answer::Queued { .. })) {
        return Err(format!("answers {:?}", lab.answers));
    }
    // One CPU is free, and a run of one waits behind the line's first rather than pass it on the
    // kernel's path: the line decides while anything waits.
    if lab.fast_cpus(0) != 0 {
        return Err(format!("kernel room {} while a run waits", lab.fast_cpus(0)));
    }
    let fast = lab.count("fast");
    lab.ask(ask("small", "cpus=1")?, 3);
    if lab.count("fast") != fast {
        return Err(format!("said {:?}", lab.said));
    }
    Ok(())
}

#[test]
fn w05_r16_a_declined_kernel_start_with_no_other_room_goes_to_probing() -> Result<(), String> {
    let mut lab = Lab::new(1, 4, true);
    // Another owner takes the runner's room by probing, and this owner doesn't hear of it yet.
    lab.hints = false;
    let other = Peer { name: "other", epoch: 1 };
    let mut out = Vec::new();
    let leg = ask("leg", "cpus=4")?;
    if let Some(r) = lab.runners.get_mut(0) {
        r.probe_r(other, leg, 0, None, 1, &mut out);
        let fence = out.iter().find_map(|m| if let Up::Offer { fence, .. } = m.msg { Some(fence) } else { None }).ok_or("no offer")?;
        r.take_r(other, "leg", fence, 1, &mut out);
        if r.start_r(other, "leg", fence, 1, &mut out) != Some(Start::New) {
            return Err("the other owner's run didn't start".into());
        }
    }
    lab.ask(ask("a", "cpus=2")?, 2);
    // The kernel picked by its stale view, the runner declined with its room, no other member has
    // room: probed, it waits there.
    let kinds: Vec<&str> = lab.said.iter().map(|(k, _)| *k).collect();
    if kinds.first() != Some(&"fast") || lab.count("probe") != 1 || !lab.started.is_empty() {
        return Err(format!("said {kinds:?}, started {:?}", lab.started));
    }
    if !matches!(lab.last("a"), Some(Answer::Queued { .. })) {
        return Err(format!("answers {:?}", lab.answers));
    }
    Ok(())
}

#[test]
fn w05_r16_a_declined_kernel_start_goes_to_another_member_with_room() -> Result<(), String> {
    let mut lab = Lab::new(2, 4, true);
    lab.hints = false;
    // Each runner's room taken where this owner doesn't see it, but r1's half of it only.
    for (i, cpus) in [(0usize, 4u64), (1, 2)] {
        let other = Peer { name: "other", epoch: 1 };
        let run = format!("leg{i}");
        let mut out = Vec::new();
        if let Some(r) = lab.runners.get_mut(i) {
            r.probe_r(other, ask(&run, &format!("cpus={cpus}"))?, 0, None, 1, &mut out);
            let fence = out.iter().find_map(|m| if let Up::Offer { fence, .. } = m.msg { Some(fence) } else { None }).ok_or("no offer")?;
            r.take_r(other, &run, fence, 1, &mut out);
            let _ = r.start_r(other, &run, fence, 1, &mut out);
        }
    }
    lab.ask(ask("a", "cpus=2")?, 2);
    // Whichever it picked first, it starts on r1 by the kernel, never twice, with no probe.
    if lab.count("probe") != 0 || lab.started != [("a".to_owned(), 1)] {
        return Err(format!("said {:?}, started {:?}", lab.said, lab.started));
    }
    if !matches!(lab.last("a"), Some(Answer::Started { .. })) {
        return Err(format!("answers {:?}", lab.answers));
    }
    Ok(())
}

#[test]
fn w05_r05_a_kernel_start_given_up_before_it_is_sent_gives_its_room_back() -> Result<(), String> {
    let mut lab = Lab::new(1, 4, true);
    let mut cmds = Vec::new();
    lab.owner.ask_c(ask("a", "cpus=4")?, 1, &mut cmds);
    if !cmds.iter().any(|c| matches!(c, Cmd::Go { .. })) {
        return Err(format!("cmds {cmds:?}"));
    }
    // Its asker gives up while its tree would be copied: nothing is sent, nothing starts.
    lab.cancel("a", 2);
    lab.carry(cmds, 2);
    if !lab.started.is_empty() || lab.count("fast") != 0 {
        return Err(format!("said {:?}, started {:?}", lab.said, lab.started));
    }
    // The kernel's view has its room back: a run of all of it starts by the kernel at once.
    lab.ask(ask("b", "cpus=4")?, 3);
    if lab.started != [("b".to_owned(), 0)] || lab.count("probe") != 0 {
        return Err(format!("said {:?}, started {:?}", lab.said, lab.started));
    }
    Ok(())
}
