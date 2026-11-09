//! Room tokens through the place core (docs/lb.md "Room tokens", "Adopted"; LB08, W05-R03, W05-R23):
//! several owners and their runners, carried as the reach shell carries them and with the server
//! passing runners' hints on. Each owner hears what it holds on placement's channel and takes no
//! hint of that runner's after; a token an owner can't use goes back and is forwarded once; a mixed
//! fleet keeps hints with the owners whose links didn't name `place.tokens`; and nothing changes
//! until tokens are declared.

use std::collections::VecDeque;

use super::answer::Answer;
use super::ask::{Ask, Peer, Sample};
use super::msg::{Down, FastWord, Up};
use super::owner::{Cmd, Machine, Owner, Size};
use super::runner::{Admission, Init, Records, Reply, Settings, Start};
use crate::facts::RunnerFacts;
use crate::sched::needs;
use crate::sched::state::{Cpus, Resources};

const GIB: u64 = 1 << 30;
const CPU: u64 = 1000;

fn ask(run: &str, spelling: &str) -> Result<Ask, String> {
    Ok(Ask { run: run.into(), needs: needs::parse(spelling)?, repo: "acme/app".into(), actor: "agent:1".into(), ..Ask::default() })
}

fn size(cpus: u64) -> Resources {
    Resources { cpu: Cpus::whole(cpus), mem: 64 * GIB, disk: 100 * GIB, gpu: 0 }
}

fn facts() -> RunnerFacts {
    RunnerFacts { os: "linux".into(), arch: "x86_64".into(), ..RunnerFacts::default() }
}

fn rname(j: usize) -> String {
    format!("r{j}")
}

/// A node that owns runs: its core, and whether its links name `place.tokens`.
struct OwnerNode {
    name: String,
    epoch: u64,
    core: Owner,
    rids: Vec<u32>,
    speaks: bool,
}

/// What's in flight.
enum Ev {
    Cmd(usize, Cmd),
    Word(usize, Reply),
}

struct Fleet {
    owners: Vec<OwnerNode>,
    runners: Vec<Admission>,
    cpus: u64,
    /// The server passes runners' hints on after each step.
    hints: bool,
    /// What owners said: its kind, the owner, the runner.
    said: Vec<(&'static str, usize, usize)>,
    /// Each kernel word a runner said to an owner: the runner, the owner, the word.
    words: Vec<(usize, usize, FastWord)>,
    /// Of those, the ones that were [`Up::FastHint`]s.
    direct: Vec<(usize, usize, FastWord)>,
    started: Vec<(String, usize)>,
    answers: Vec<(String, Answer)>,
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
        Down::FastRelease { .. } => "fast_release",
    }
}

fn machines(n: usize, cpus: u64) -> Vec<Machine> {
    (0..n).map(|j| Machine { name: rname(j), facts: facts(), labels: Vec::new(), size: Size::known(&size(cpus)), epoch: 1, connected: true, places: true, syncs: false, idle: None, fast: true }).collect()
}

impl Fleet {
    /// Owners (name, whether its links name `place.tokens`), and `n` runners of `cpus` CPUs each,
    /// room tokens declared there or not; every owner has heard every runner's hints.
    fn new(owners: &[(&str, bool)], n: usize, cpus: u64, tokens: bool) -> Fleet {
        let runners = (0..n).map(|j| Admission::new(Init { me: rname(j), epoch: 1, since: None, facts: facts(), labels: Vec::new(), sample: Sample { size: size(cpus), ..Sample::default() }, records: Records::new(), settings: Settings { tokens, ..Settings::default() } }, 0)).collect();
        let owners = owners.iter().zip(7u64..).map(|((name, speaks), seed)| Fleet::node(name, 1, seed, *speaks, n, cpus)).collect();
        let mut f = Fleet { owners, runners, cpus, hints: true, said: Vec::new(), words: Vec::new(), direct: Vec::new(), started: Vec::new(), answers: Vec::new() };
        f.broadcast();
        f
    }

    fn node(name: &str, epoch: u64, seed: u64, speaks: bool, n: usize, cpus: u64) -> OwnerNode {
        let mut core = Owner::seeded(name, epoch, seed);
        let _ = core.machines(machines(n, cpus));
        let rids = (0..n).map(|j| core.runner_id(&rname(j))).collect();
        OwnerNode { name: name.to_owned(), epoch, core, rids, speaks }
    }

    /// Owner `o` started again, knowing what the directory and the server's hints say.
    fn restart(&mut self, o: usize) -> Result<(), String> {
        let n = self.runners.len();
        let cpus = self.cpus;
        let x = self.owners.get_mut(o).ok_or("owner")?;
        *x = Fleet::node(&x.name.clone(), x.epoch + 1, 99, x.speaks, n, cpus);
        self.broadcast();
        Ok(())
    }

    /// Every runner's hint, passed on to every owner (`ServerMsg::Offers`).
    fn broadcast(&mut self) {
        for (j, r) in self.runners.iter().enumerate() {
            let w = r.fast_word();
            for o in &mut self.owners {
                if let Some(rid) = o.rids.get(j).copied() {
                    o.core.fast_hint_c(rid, &w);
                }
            }
        }
    }

    fn ask(&mut self, o: usize, a: Ask, now: i64) -> Result<(), String> {
        let mut cmds = Vec::new();
        self.owners.get_mut(o).ok_or("owner")?.core.ask_c(a, now, &mut cmds);
        self.run(cmds.into_iter().map(|c| Ev::Cmd(o, c)).collect(), now)
    }

    /// Run `run` of owner `o` on runner `j` ended.
    fn end(&mut self, o: usize, j: usize, run: &str, now: i64) -> Result<(), String> {
        let x = self.owners.get(o).ok_or("owner")?;
        let (name, epoch) = (x.name.clone(), x.epoch);
        let mut out = Vec::new();
        self.runners.get_mut(j).ok_or("runner")?.ended_r(Peer { name: &name, epoch }, run, true, now, &mut out);
        self.started.retain(|(r, i)| !(r == run && *i == j));
        self.run(out.into_iter().map(|m| Ev::Word(j, m)).collect(), now)
    }

    /// Owner `o`'s link to runner `j` went down, both ends told.
    fn link_down(&mut self, o: usize, j: usize, now: i64) -> Result<(), String> {
        let x = self.owners.get_mut(o).ok_or("owner")?;
        let mut cmds = Vec::new();
        x.core.away_c(&rname(j), &mut cmds);
        let name = x.name.clone();
        let mut out = Vec::new();
        self.runners.get_mut(j).ok_or("runner")?.away_r(&name, now, &mut out);
        self.run(out.into_iter().map(|m| Ev::Word(j, m)).chain(cmds.into_iter().map(|c| Ev::Cmd(o, c))).collect(), now)
    }

    /// Owner `o`'s link to runner `j` is back, both ends told.
    fn link_up(&mut self, o: usize, j: usize, now: i64) -> Result<(), String> {
        let x = self.owners.get_mut(o).ok_or("owner")?;
        let mut cmds = Vec::new();
        x.core.back_c(&rname(j), &mut cmds);
        let (name, epoch, speaks) = (x.name.clone(), x.epoch, x.speaks);
        let r = self.runners.get_mut(j).ok_or("runner")?;
        r.tokens_with(&name, speaks);
        let mut out = Vec::new();
        r.back_r(Peer { name: &name, epoch }, now, &mut out);
        self.run(out.into_iter().map(|m| Ev::Word(j, m)).chain(cmds.into_iter().map(|c| Ev::Cmd(o, c))).collect(), now)
    }

    /// Carries everything in flight, in the order it was said, then the server's hints.
    fn run(&mut self, mut q: VecDeque<Ev>, now: i64) -> Result<(), String> {
        let mut steps = 0;
        while let Some(ev) = q.pop_front() {
            steps += 1;
            if steps > 4096 {
                return Err("the fleet never settled".into());
            }
            match ev {
                Ev::Cmd(o, c) => self.cmd(o, c, now, &mut q),
                Ev::Word(j, m) => self.word(j, m, now, &mut q),
            }
        }
        if self.hints {
            self.broadcast();
        }
        Ok(())
    }

    /// What owner `o`'s core says to do, done as the shell does it: each word goes to its runner
    /// with what the link says of the owner's features.
    fn cmd(&mut self, o: usize, c: Cmd, now: i64, q: &mut VecDeque<Ev>) {
        let Some(x) = self.owners.get(o) else { return };
        let (name, epoch, speaks) = (x.name.clone(), x.epoch, x.speaks);
        let peer = Peer { name: &name, epoch };
        match c {
            Cmd::Tell { to, run, msg } => {
                let Some(j) = x.rids.iter().position(|r| *r == to) else { return };
                let Some(r) = self.runners.get_mut(j) else { return };
                r.tokens_with(&name, speaks);
                self.said.push((kind(&msg), o, j));
                let mut out = Vec::new();
                match msg {
                    Down::Probe { ask, token, elsewhere } => {
                        r.carried_r(peer, &ask.run, Some(&*ask), elsewhere, now, &mut out);
                        r.probe_r(peer, ask, token, None, now, &mut out);
                    }
                    Down::Withdraw => r.withdraw_r(peer, &run, now, &mut out),
                    Down::Take { fence } => r.take_r(peer, &run, fence, now, &mut out),
                    Down::Decline { fence } => r.decline_r(peer, &run, fence, now, &mut out),
                    Down::Ack => r.ack(peer, &run),
                    // Starts are the shell's, after its tree is copied: on Go, below.
                    Down::Start { .. } | Down::Fast { .. } => {}
                    Down::Sync => r.sync_r(peer, now, &mut out),
                    Down::FastRelease { epoch, amount } => r.release_r(peer, epoch, amount, now, &mut out),
                }
                q.extend(out.into_iter().map(|m| Ev::Word(j, m)));
            }
            Cmd::Go { run, to, fence } => {
                let Some(j) = x.rids.iter().position(|r| *r == to) else { return };
                let Some(x) = self.owners.get_mut(o) else { return };
                let fast = x.core.fast_start(&run, fence);
                if !x.core.starting_c(&run, to, fence) {
                    return;
                }
                let Some(r) = self.runners.get_mut(j) else { return };
                r.tokens_with(&name, speaks);
                let mut out = Vec::new();
                let started = match fast {
                    Some((ask, token, kernel, at)) => {
                        self.said.push(("fast", o, j));
                        r.fast_r(peer, ask, token, kernel, at, None, now, &mut out)
                    }
                    None => {
                        self.said.push(("start", o, j));
                        r.start_r(peer, &run, fence, now, &mut out)
                    }
                };
                if started == Some(Start::New) {
                    r.spawned_r(peer, &run, &mut out);
                    self.started.push((run.to_string(), j));
                }
                q.extend(out.into_iter().map(|m| Ev::Word(j, m)));
            }
            Cmd::Answer { run, answer } => self.answers.push((run.to_string(), answer)),
            Cmd::Fallback { .. } | Cmd::Restart { .. } => {}
        }
    }

    /// Runner `j`'s word reaches its owner.
    fn word(&mut self, j: usize, m: Reply, now: i64, q: &mut VecDeque<Ev>) {
        let Some(name) = self.runners.get(j).map(|r| r.owner_name(m.owner).to_owned()) else { return };
        let Some(o) = self.owners.iter().position(|x| x.name == name) else { return };
        match &m.msg {
            Up::FastStarted { word, .. } | Up::FastDeclined { word, .. } | Up::FastEnded { word, .. } => self.words.push((j, o, *word)),
            Up::FastHint { word } => {
                self.words.push((j, o, *word));
                self.direct.push((j, o, *word));
            }
            _ => {}
        }
        let Some(x) = self.owners.get_mut(o) else { return };
        let Some(rid) = x.rids.get(j).copied() else { return };
        let mut cmds = Vec::new();
        x.core.heard_c(rid, m.token, &m.run, m.msg, now, &mut cmds);
        q.extend(cmds.into_iter().map(|c| Ev::Cmd(o, c)));
    }

    fn count(&self, what: &str) -> usize {
        self.said.iter().filter(|(k, ..)| *k == what).count()
    }

    fn last(&self, run: &str) -> Option<&Answer> {
        self.answers.iter().rev().find(|(r, _)| r == run).map(|(_, a)| a)
    }

    /// What owner `o`'s kernel makes of runner `j`, in thousandths of a CPU.
    fn view(&self, o: usize, j: usize) -> u64 {
        self.owners.get(o).and_then(|x| x.core.fast_view(&rname(j))).map_or(0, |v| v[0])
    }

    fn views(&self, j: usize) -> Vec<u64> {
        (0..self.owners.len()).map(|o| self.view(o, j)).collect()
    }

    fn marked(&self, o: usize, j: usize) -> bool {
        self.owners.get(o).is_some_and(|x| x.core.fast_tokens(&rname(j)))
    }

    /// The room runner `j`'s kernel has for a new run, in thousandths of a CPU.
    fn room(&self, j: usize) -> u64 {
        self.runners.get(j).map_or(0, |r| r.fast_word().room[0])
    }
}

/// LB08: each owner hears from the runner what it holds there, on placement's channel, and takes no
/// hint of that runner's once it has: an owner's view of it is its own token, the room its run
/// freed goes back to it, and what they hold is never more than the room.
#[test]
fn lb08_under_tokens_each_owner_hears_what_it_holds() -> Result<(), String> {
    let mut f = Fleet::new(&[("a", true), ("b", true)], 1, 8, true);
    if f.views(0) != [8 * CPU, 8 * CPU] || f.marked(0, 0) || f.marked(1, 0) {
        return Err(format!("both start from the server's hints: {:?}", f.views(0)));
    }
    f.ask(0, ask("x", "cpus=2")?, 1)?;
    if f.count("fast") != 1 || f.count("probe") != 0 || !f.marked(0, 0) || f.view(0, 0) != 6 * CPU {
        return Err(format!("said {:?}, a's view {}", f.said, f.view(0, 0)));
    }
    // b's first start goes by the hints it has heard; its answer says it holds nothing, and from
    // then on it hears only what it holds. The first watcher is told what's left.
    f.ask(1, ask("y", "cpus=2")?, 2)?;
    if !f.marked(1, 0) || f.views(0) != [4 * CPU, 0] || f.room(0) != 4 * CPU {
        return Err(format!("after b's start: views {:?}, room {}", f.views(0), f.room(0)));
    }
    if f.direct.last().is_none_or(|(_, o, w)| *o != 0 || w.room[0] != 4 * CPU || !w.tokens) {
        return Err(format!("the first watcher is told what's left to it: {:?}", f.direct));
    }
    // b's run ends: the room it freed goes back to it, and a's stays what it was.
    f.end(1, 0, "y", 3)?;
    if f.views(0) != [4 * CPU, 2 * CPU] || f.room(0) != 6 * CPU {
        return Err(format!("after b's end: views {:?}, room {}", f.views(0), f.room(0)));
    }
    // The server's hints say all of the room: neither takes that for what it holds.
    f.broadcast();
    if f.views(0) != [4 * CPU, 2 * CPU] {
        return Err(format!("the runner's hints changed what they hold: {:?}", f.views(0)));
    }
    f.end(0, 0, "x", 4)?;
    if f.views(0) != [6 * CPU, 2 * CPU] || f.room(0) != 8 * CPU {
        return Err(format!("after a's end: views {:?}, room {}", f.views(0), f.room(0)));
    }
    // What b holds is its own to use: an ask of it starts by the kernel, no probe.
    f.ask(1, ask("z", "cpus=2")?, 5)?;
    if f.count("probe") != 0 || f.count("fast") != 3 || !f.started.iter().any(|(r, _)| r == "z") {
        return Err(format!("said {:?}, started {:?}", f.said, f.started));
    }
    Ok(())
}

/// LB08: a mixed fleet keeps hints with the owner whose link didn't name `place.tokens`: its words
/// say all of the room, none of them is its own, it keeps taking the server's hints, nothing is
/// said to it on placement's channel, and the room its runs free reaches the owners that hear
/// tokens through the first watcher.
#[test]
fn lb08_a_mixed_fleet_keeps_hints_with_an_owner_that_did_not_name_tokens() -> Result<(), String> {
    let mut f = Fleet::new(&[("a", true), ("old", false)], 1, 8, true);
    f.ask(0, ask("x", "cpus=2")?, 1)?;
    f.ask(1, ask("y", "cpus=2")?, 2)?;
    if !f.marked(0, 0) || f.marked(1, 0) {
        return Err("only the owner that named it hears tokens".into());
    }
    if f.views(0) != [4 * CPU, 4 * CPU] || f.direct.iter().any(|(_, o, _)| *o == 1) || f.words.iter().any(|(_, o, w)| *o == 1 && w.tokens) {
        return Err(format!("views {:?}, words {:?}", f.views(0), f.words));
    }
    // Its run ends: what it freed is no one's, so the first watcher's, told at once.
    f.end(1, 0, "y", 3)?;
    if f.views(0) != [6 * CPU, 6 * CPU] {
        return Err(format!("after old's end: {:?}", f.views(0)));
    }
    if f.direct.last().is_none_or(|(_, o, w)| *o != 0 || w.room[0] != 6 * CPU) {
        return Err(format!("the first watcher is told: {:?}", f.direct));
    }
    f.end(0, 0, "x", 4)?;
    if f.views(0) != [8 * CPU, 8 * CPU] {
        return Err(format!("after a's end: {:?}", f.views(0)));
    }
    Ok(())
}

/// LB08: a token its owner can't use is given back, as a placement word (`FastRelease`), and the
/// runner forwards it to the owner whose run last waited there for room, once. The owner that
/// waited then starts by the kernel, on what was forwarded to it.
#[test]
fn lb08_a_token_given_back_is_forwarded_once_to_the_owner_that_waited() -> Result<(), String> {
    let mut f = Fleet::new(&[("a", true), ("b", true)], 1, 4, true);
    f.ask(0, ask("x", "cpus=4")?, 1)?;
    // No room in b's view, so it's probed: it waits in the runner's line, the place core's Wait.
    f.ask(1, ask("y", "cpus=1")?, 2)?;
    if f.count("probe") != 1 || !matches!(f.last("y"), Some(Answer::Queued { .. })) {
        return Err(format!("said {:?}, answers {:?}", f.said, f.answers));
    }
    // a's run ends: the line takes what y needs, and a, holding the rest, hears that another wants
    // it, and gives it back. It goes to b.
    f.end(0, 0, "x", 3)?;
    if f.count("fast_release") != 1 || !f.started.iter().any(|(r, _)| r == "y") {
        return Err(format!("said {:?}, started {:?}", f.said, f.started));
    }
    if f.views(0) != [0, 3 * CPU] || f.room(0) != 3 * CPU {
        return Err(format!("b holds what a gave back: views {:?}, room {}", f.views(0), f.room(0)));
    }
    // Once: no word after says another still wants.
    if f.words.iter().rev().take(2).any(|(_, _, w)| w.wants) {
        return Err(format!("forwarded, it's wanted no more: {:?}", f.words));
    }
    // b's next run starts by the kernel, on the token forwarded to it.
    let probes = f.count("probe");
    f.ask(1, ask("z", "cpus=3")?, 4)?;
    if f.count("probe") != probes || !f.started.iter().any(|(r, _)| r == "z") {
        return Err(format!("said {:?}, started {:?}", f.said, f.started));
    }
    Ok(())
}

/// LB08: an owner that gives nothing back keeps what it holds: a word says another wants only when
/// a run really waited for room there, not for a run that started at once.
#[test]
fn lb08_a_run_offered_at_once_wants_nothing_of_anyone() -> Result<(), String> {
    let mut f = Fleet::new(&[("a", true), ("b", true)], 1, 8, true);
    f.ask(0, ask("x", "cpus=2")?, 1)?;
    // b holds nothing and hears so, so its next ask finds no room in its view and is probed: the
    // runner has room for it, and offers it at once.
    f.ask(1, ask("y", "cpus=1")?, 2)?;
    f.ask(1, ask("z", "cpus=1")?, 3)?;
    if f.count("probe") != 1 || !f.started.iter().any(|(r, _)| r == "z") {
        return Err(format!("said {:?}, started {:?}", f.said, f.started));
    }
    if f.count("fast_release") != 0 || f.words.iter().any(|(_, _, w)| w.wants) {
        return Err(format!("nothing waited, and nothing was asked of a: {:?}", f.words));
    }
    Ok(())
}

/// LB08: an owner whose link is down holds nothing there: what it held is no one's while it's
/// away, the next that hears tokens the first watcher; back, it's told what it holds again.
#[test]
fn lb08_an_owner_away_holds_nothing_and_hears_its_word_when_it_is_back() -> Result<(), String> {
    let mut f = Fleet::new(&[("a", true), ("b", true)], 1, 4, true);
    f.ask(0, ask("x", "cpus=1")?, 1)?;
    f.ask(1, ask("y", "cpus=1")?, 2)?;
    f.end(1, 0, "y", 3)?;
    if f.views(0) != [2 * CPU, CPU] {
        return Err(format!("a holds what no one does, b what its run freed: {:?}", f.views(0)));
    }
    f.link_down(0, 0, 4)?;
    if f.view(1, 0) != 3 * CPU || f.marked(0, 0) || !f.marked(1, 0) {
        return Err(format!("a away, b is told all of the room: {:?}", f.views(0)));
    }
    // a takes hints while its link is down: its view is the room, as an owner's without tokens.
    if f.view(0, 0) != 3 * CPU {
        return Err(format!("a takes the server's hints while away: {:?}", f.views(0)));
    }
    f.link_up(0, 0, 5)?;
    if f.views(0) != [2 * CPU, CPU] || !f.marked(0, 0) {
        return Err(format!("a back, each holds what it did: {:?}", f.views(0)));
    }
    Ok(())
}

/// LB08: an owner's process that gave way to a new one holds nothing here, so what it held isn't
/// lost to the rest of the fleet: the new process starts from nothing, its first watcher's room
/// all that no one holds.
#[test]
fn lb08_a_process_retired_holds_nothing() -> Result<(), String> {
    let mut f = Fleet::new(&[("a", true), ("b", true)], 1, 4, true);
    f.ask(0, ask("x", "cpus=1")?, 1)?;
    f.ask(1, ask("y", "cpus=1")?, 2)?;
    f.end(1, 0, "y", 3)?;
    if f.views(0) != [2 * CPU, CPU] {
        return Err(format!("b holds what its run freed: {:?}", f.views(0)));
    }
    // b starts again: its new process's first start takes a CPU of what no one holds now: a's
    // token is what's left of the room, the old process's CPU not kept from it.
    f.restart(1)?;
    f.ask(1, ask("w", "cpus=1")?, 4)?;
    if f.view(0, 0) != 2 * CPU || f.view(1, 0) != 0 || f.room(0) != 2 * CPU {
        return Err(format!("b's new process holds nothing: views {:?}, room {}", f.views(0), f.room(0)));
    }
    Ok(())
}

/// LB08: until tokens are declared nothing changes: no word on placement's channel, no word that's
/// an owner's own, owners keep taking the hints, and each sees all of the room, even when every
/// link named `place.tokens`.
#[test]
fn lb08_tokens_are_off_until_declared() -> Result<(), String> {
    let mut f = Fleet::new(&[("a", true), ("b", true)], 1, 8, false);
    f.ask(0, ask("x", "cpus=2")?, 1)?;
    f.ask(1, ask("y", "cpus=2")?, 2)?;
    f.end(1, 0, "y", 3)?;
    if !f.direct.is_empty() || f.count("fast_release") != 0 || f.words.iter().any(|(_, _, w)| w.tokens || w.wants) {
        return Err(format!("said {:?}, words {:?}", f.said, f.words));
    }
    if f.marked(0, 0) || f.marked(1, 0) || f.views(0) != [6 * CPU, 6 * CPU] {
        return Err(format!("views {:?}", f.views(0)));
    }
    Ok(())
}

/// LB08: tokens declared with no owner naming them is a runner all of whose owners hear hints.
#[test]
fn lb08_tokens_declared_with_no_owner_naming_them_changes_nothing() -> Result<(), String> {
    let mut f = Fleet::new(&[("a", false), ("b", false)], 1, 8, true);
    f.ask(0, ask("x", "cpus=2")?, 1)?;
    f.ask(1, ask("y", "cpus=2")?, 2)?;
    f.end(1, 0, "y", 3)?;
    if !f.direct.is_empty() || f.words.iter().any(|(_, _, w)| w.tokens || w.wants) || f.views(0) != [6 * CPU, 6 * CPU] {
        return Err(format!("views {:?}, words {:?}", f.views(0), f.words));
    }
    Ok(())
}

/// W05-R09, LB08: `[reach]`'s settings default as placement did without them.
#[test]
fn lb08_the_settings_default_as_before() {
    let s = Settings::default();
    assert_eq!((s.overcommit_cpu, s.backfill, s.predicted_order, s.tokens), (1000, false, None, false));
}
