//! The hand-off of runs whose owner can't be reached, in this node (LB03, W05-R25; docs/reach.md
//! §2.4), around `kiln_ci::place::handoff`: as a runner that holds the news and a member that
//! promises; as a claimant that adopts (its words then go under its claim's ballot, and its answers
//! to the run's agent through told, its owner's rows its own); and as an owner whose runs' members
//! grow only with every holding member's word.
//!
//! Who's suspected comes from `kiln_ci::detect`, as the node feeds it ([`Placer::suspect`]): a
//! dropped link, an answer overdue, a peer's report. A runner acts on a suspicion only where it
//! needs the owner: a run's end to tell, room for a run that waits, a claim it promised whose
//! claimant went.

use std::collections::{BTreeMap, BTreeSet};

use kiln_ci::place::answer::Answer;
use kiln_ci::place::ask::{Ask, Peer};
use kiln_ci::place::handoff::{Acceptor, Ballot, Claim, Decision, Gate, Handle, Held, Members, Released, Row, Verdict, Word, ranked};
use kiln_ci::place::msg::{ToOwner, ToRunner};
use kiln_ci::sched::state::Resources;

use super::{Envelope, Out, Placer};
use crate::reach::directory::RunnerInfo;

/// The records family of the promises this runner gave (LB03): one row a run, by its handle, kept
/// as Paxos' acceptors keep theirs.
pub const HAND: &str = "place.hand";

/// A promise's key: its run's handle, as a run's record is keyed.
pub fn hand_key(h: &Handle) -> Vec<u8> {
    super::ran_key(&h.owner, h.epoch, &h.run)
}

/// The handle a promise's key names.
pub fn hand_of(key: &[u8]) -> Option<Handle> {
    super::ran_of(key).map(|(owner, epoch, run)| Handle { owner, epoch, run })
}

/// The run a placement word is about, if one.
pub fn run_of(m: &ToRunner) -> Option<&str> {
    match m {
        ToRunner::Probe { ask } => Some(ask.run.as_str()),
        ToRunner::Withdraw { run } | ToRunner::Take { run, .. } | ToRunner::Decline { run, .. } | ToRunner::Start { run, .. } | ToRunner::Ack { run } => Some(run.as_str()),
        ToRunner::Sync => None,
    }
}

/// This node's part in hand-offs.
#[derive(Default)]
pub struct Hand {
    /// Its promises, as a runner (kept).
    pub acc: Acceptor,
    /// Its runs' members, as their owner.
    pub members: Members,
    claims: BTreeMap<Handle, Claim>,
    yielded: BTreeMap<Handle, Claim>,
    /// Runs it adopted, under its claim's ballot: its words about them carry it.
    adopted: BTreeMap<Handle, Ballot>,
    /// Its own runs it claimed back, under that claim's ballot.
    reclaimed: BTreeMap<String, Ballot>,
    /// Nodes it suspects, and why.
    suspects: BTreeMap<String, String>,
    /// Runs whose news it sent, and to whom.
    handing: BTreeMap<Handle, String>,
    /// Its probes held until their runner is a member of the run, by run and runner: the word, or,
    /// for this runner itself, the ask.
    held: BTreeMap<(String, String), String>,
    held_here: BTreeMap<String, std::sync::Arc<Ask>>,
    /// The nodes that take hand-offs (the directory's that name `place.handoff`), this one among
    /// them, and those of them connected now.
    speakers: BTreeSet<String>,
    connected: BTreeSet<String>,
}

impl Hand {
    /// The ballot this owner's words about run `run` go under: its claim's, for a run it adopted or
    /// claimed back; none (its first owner's) otherwise.
    pub fn ballot_of(&self, run: &str) -> Option<Ballot> {
        match Handle::of_adopted(run) {
            Some(h) => self.adopted.get(&h).cloned(),
            None => self.reclaimed.get(run).cloned(),
        }
    }
}

impl Placer {
    /// The directory came: which nodes take hand-offs.
    pub(super) fn hand_directory(&mut self, runners: &[RunnerInfo]) {
        let speaking = runners.iter().filter(|r| crate::proto::feature::speaks(&r.features, crate::proto::feature::PLACE_HANDOFF));
        self.hand.connected = speaking.clone().filter(|r| r.connected).map(|r| r.name.clone()).collect();
        self.hand.speakers = speaking.map(|r| r.name.clone()).collect();
        self.hand.speakers.insert(self.me.clone());
        self.hand.connected.insert(self.me.clone());
    }

    /// The promises its records kept, as it starts (LB03); its own claims went with its process,
    /// and the members it promised them to still wait: each is made again.
    pub(super) fn hand_restore(&mut self, rows: Vec<(Handle, Row)>, now: i64) -> Vec<Out> {
        let me = self.me.clone();
        let mine: Vec<Claim> = rows.iter().filter(|(_, r)| !r.owned && r.ballot.by == me).map(|(h, r)| Claim::new(h.clone(), r.ballot.clone(), r.members.clone(), r.held.clone().map(|held| (me.clone(), held)))).collect();
        self.hand.acc = Acceptor::restore(rows);
        let mut out = Vec::new();
        for c in mine {
            out.extend(self.hand_claim(c, now));
        }
        out
    }

    /// The promises changed: kept with the rest of placement's hard state, before anything they
    /// gate is said (a promise goes only once it's kept).
    pub(super) fn hand_changes(&mut self) {
        for (h, row) in self.hand.acc.changes() {
            self.unkept.insert((HAND, hand_key(&h)), row.and_then(|r| serde_json::to_vec(&r).ok()));
        }
    }

    /// Node `peer` is suspected (`why`, as `kiln_ci::detect` said): what this node needs it for is
    /// handed off. As a runner: its runs here with an end to tell or room to start, and the runs
    /// whose claim it was promised and hasn't finished, or that it owns and whose end is here. As
    /// the one that sent news to it: the news goes to the next. As a claimant: a claim it yielded
    /// to `peer` is made again, a round up.
    pub fn suspect(&mut self, peer: &str, why: &str, now: i64) -> Vec<Out> {
        if peer == self.me {
            return Vec::new();
        }
        self.hand.suspects.insert(peer.to_owned(), why.to_owned());
        let mut out = Vec::new();
        for (epoch, run) in self.admission.runs_of(peer) {
            let from = Peer { name: peer, epoch };
            let need = match self.admission.holding(from, &run) {
                Some(held @ Held::Ended { .. }) => Some(held),
                Some(held @ Held::Waiting { .. }) if self.admission.could_start(from, &run) => Some(held),
                _ => None,
            };
            if let Some(held) = need {
                out.extend(self.hand_off(Handle::new(peer, epoch, &run), held, peer, now));
            }
        }
        let promised: Vec<(Handle, Option<Held>)> = self
            .hand
            .acc
            .rows()
            .filter(|(_, r)| r.ballot.by == peer && (!r.owned || matches!(r.held, Some(Held::Ended { .. }))))
            .map(|(h, r)| (h.clone(), r.held.clone()))
            .collect();
        let handing: Vec<Handle> = self.hand.handing.iter().filter(|(_, a)| a.as_str() == peer).map(|(h, _)| h.clone()).collect();
        for (h, kept) in promised {
            let held = self.admission.holding(Peer { name: &h.owner, epoch: h.epoch }, &h.run).or(kept);
            if let Some(held) = held {
                out.extend(self.hand_off(h, held, peer, now));
            }
        }
        for h in handing {
            self.hand.handing.remove(&h);
            let kept = self.hand.acc.row(&h).and_then(|r| r.held.clone());
            if let Some(held) = self.admission.holding(Peer { name: &h.owner, epoch: h.epoch }, &h.run).or(kept) {
                let owner = h.owner.clone();
                out.extend(self.hand_off(h, held, &owner, now));
            }
        }
        let again: Vec<Handle> = self.hand.yielded.iter().filter(|(_, c)| matches!(c.decide(), Decision::Yield(b) if b.by == peer)).map(|(h, _)| h.clone()).collect();
        for h in again {
            out.extend(self.hand_again(&h, now));
        }
        out
    }

    /// `peer` spoke: it's no longer suspected.
    pub(super) fn hand_heard_from(&mut self, peer: &str) {
        self.hand.suspects.remove(peer);
    }

    /// The link to runner `peer` is up again: what it was asked and hasn't answered is asked again
    /// (a claim it hasn't promised, a runner it hasn't taken into a run's members).
    pub(super) fn hand_relinked(&mut self, peer: &str, now: i64) -> Vec<Out> {
        self.hand_heard_from(peer);
        let claims: Vec<Word> = self
            .hand
            .claims
            .values()
            .filter(|c| c.unpromised().iter().any(|m| m == peer))
            .map(|c| Word::Claim { handle: c.handle.clone(), ballot: c.ballot.clone(), members: c.members() })
            .collect();
        let me = self.me.clone();
        let epoch = self.epoch;
        let extends: Vec<Word> = self
            .hand
            .members
            .runs_asking(peer)
            .into_iter()
            .map(|(run, add)| Word::Extend { ballot: self.hand.ballot_of(&run).unwrap_or(Ballot::FIRST), handle: Handle::new(&me, epoch, &run), add })
            .collect();
        let mut out = Vec::new();
        for w in claims.into_iter().chain(extends) {
            out.extend(self.hand_tell(peer, w, now));
        }
        out
    }

    /// Room may have come for runs of owners it suspects: those that could start now are handed off.
    pub(super) fn hand_room(&mut self, now: i64) -> Vec<Out> {
        let mut out = Vec::new();
        let suspects: Vec<String> = self.hand.suspects.keys().cloned().collect();
        for peer in suspects {
            for (epoch, run) in self.admission.runs_of(&peer) {
                let from = Peer { name: &peer, epoch };
                if let Some(held @ Held::Waiting { .. }) = self.admission.holding(from, &run)
                    && self.admission.could_start(from, &run)
                {
                    out.extend(self.hand_off(Handle::new(&peer, epoch, &run), held, &peer, now));
                }
            }
        }
        out
    }

    /// A run of `owner` (in `epoch`) ended here: to its claimant if one owns it, kept in the
    /// promise if one is under way, handed off if its owner is suspected.
    pub(super) fn hand_ended(&mut self, owner: &str, epoch: u64, run: &str, now: i64) -> Vec<Out> {
        let h = Handle::new(owner, epoch, run);
        match self.hand.acc.row(&h).map(|r| r.owned) {
            Some(true) => self.hand_report(&h, now),
            Some(false) => {
                let held = self.admission.holding(Peer { name: owner, epoch }, run);
                self.hand.acc.now_holds(&h, held);
                Vec::new()
            }
            None if self.hand.suspects.contains_key(owner) => match self.admission.holding(Peer { name: owner, epoch }, run) {
                Some(held) => self.hand_off(h, held, owner, now),
                None => Vec::new(),
            },
            None => Vec::new(),
        }
    }

    /// A word from `peer` (epoch `epoch`) about run `run` under `ballot`: its handle if it's taken;
    /// refused, and said so, if a claim above it was promised.
    pub(super) fn hand_gate(&mut self, peer: &str, epoch: u64, run: &str, ballot: &Ballot, now: i64, out: &mut Vec<Out>) -> Option<Handle> {
        let h = Handle::of(peer, epoch, run);
        match self.hand.acc.gate(&h, ballot) {
            Gate::Pass => Some(h),
            Gate::Refuse(b) => {
                out.extend(self.hand_say(peer, Word::Refused { handle: h, ballot: b }, now));
                None
            }
        }
    }

    /// The word of the claimant that owns run `h` (it passed the gate): about the run as its
    /// runners know it, under its first owner's name and epoch.
    pub(super) fn hand_owner_word(&mut self, h: Handle, msg: ToRunner, now: i64) -> Vec<Out> {
        let from = Peer { name: &h.owner, epoch: h.epoch };
        let mut said = Vec::new();
        match msg {
            ToRunner::Ack { .. } => {
                self.admission.ack(from, &h.run);
                self.hand.acc.now_holds(&h, None);
                self.hand.acc.unheld(&h);
            }
            ToRunner::Withdraw { .. } => self.admission.withdraw(from, &h.run, now, &mut said),
            _ => {}
        }
        // What its admission says goes to the run's first owner, which can't take it: not said.
        drop(said);
        Vec::new()
    }

    /// A hand-off's word from `peer`, which asked this node: a claim, or a claim's `Own`, to this
    /// runner as a member; a run's members growing; or the news of a run, for this node to adopt.
    pub(super) fn hand_told(&mut self, peer: &str, w: Word, now: i64) -> Vec<Out> {
        self.hand_heard_from(peer);
        let mut out = Vec::new();
        match w {
            Word::Claim { handle, ballot, members } => {
                let held = self.admission.holding(Peer { name: &handle.owner, epoch: handle.epoch }, &handle.run);
                match self.hand.acc.claim(&handle, &ballot, held.clone(), &members) {
                    Ok(p) => {
                        // Promised away: a run that waits here leaves its line, its ask kept in the
                        // promise.
                        if matches!(held, Some(Held::Waiting { .. })) {
                            let mut said = Vec::new();
                            self.admission.withdraw(Peer { name: &handle.owner, epoch: handle.epoch }, &handle.run, now, &mut said);
                            out.extend(self.said(said, now));
                        }
                        out.extend(self.hand_say(peer, Word::Promise { handle, ballot, held: p.held, members: p.members }, now));
                    }
                    Err(b) => out.extend(self.hand_say(peer, Word::Refused { handle, ballot: b }, now)),
                }
            }
            Word::Own { handle, ballot } => {
                if self.hand.acc.own(&handle, &ballot) {
                    out.extend(self.hand_report(&handle, now));
                }
            }
            Word::Extend { handle, add, ballot } => {
                let w = match self.hand.acc.extend(&handle, &ballot, std::slice::from_ref(&add)) {
                    Ok(()) => Word::Extended { handle, add },
                    Err(b) => Word::Refused { handle, ballot: b },
                };
                out.extend(self.hand_say(peer, w, now));
            }
            Word::Orphan { handle, members, held, ballot, why } => {
                out.extend(self.hand_adopt(Some(peer), handle, members, Some((peer.to_owned(), held)), ballot, Some(why), now));
            }
            Word::Promise { .. } | Word::Refused { .. } | Word::Extended { .. } | Word::Adopting { .. } => {}
        }
        out
    }

    /// A hand-off's word from runner `peer` on this node's link to it: a promise or a refusal of a
    /// claim; a refusal of this owner's word (its run was handed off); a member's word that it took
    /// a runner into a run's members.
    pub(super) fn hand_heard(&mut self, peer: &str, w: Word, now: i64) -> Vec<Out> {
        self.hand_heard_from(peer);
        let mut out = Vec::new();
        match w {
            Word::Promise { handle, ballot, held, members } => {
                let Some(c) = self.hand.claims.get_mut(&handle) else { return out };
                let new = c.promised(peer, &ballot, held, &members);
                let all = c.members();
                for m in new {
                    out.extend(self.hand_tell(&m, Word::Claim { handle: handle.clone(), ballot: ballot.clone(), members: all.clone() }, now));
                }
                out.extend(self.hand_decide(&handle, now));
            }
            Word::Refused { handle, ballot } => {
                if let Some(c) = self.hand.claims.get_mut(&handle) {
                    c.refused(ballot);
                    out.extend(self.hand_decide(&handle, now));
                } else if handle.owner == self.me && !self.hand.reclaimed.contains_key(&handle.run) {
                    // Its own run was handed off: it stands down, saying nothing more of it.
                    tracing::info!(run = %handle.run, by = %ballot.by, "a run of this node's was handed off while a runner holding it couldn't reach this one");
                    self.hand_forget(&handle.run);
                    let acts = self.owner.handed(&handle.run, &ballot.by);
                    out.extend(self.acts(acts, now));
                } else if self.hand.adopted.get(&handle).is_some_and(|b| *b < ballot) {
                    // Another claim took over a run this node adopted.
                    self.hand.adopted.remove(&handle);
                    let acts = self.owner.handed(&handle.adopted_id(), &ballot.by);
                    out.extend(self.acts(acts, now));
                }
            }
            Word::Extended { handle, add } => {
                let released = self.hand.members.extended(&handle.run, peer, &add);
                out.extend(self.hand_release(&handle.run, released, now));
            }
            Word::Adopting { .. } | Word::Orphan { .. } | Word::Claim { .. } | Word::Own { .. } | Word::Extend { .. } => {}
        }
        out
    }

    /// What runner `from` said of run `run` (as its owner hears it): whether it may hold it, for
    /// the run's members.
    pub(super) fn hand_member_said(&mut self, from: &str, msg: &ToOwner, now: i64) -> Vec<Out> {
        let run = msg.run().to_owned();
        if run.is_empty() {
            return Vec::new();
        }
        let holds = !matches!(msg, ToOwner::Never { .. });
        let released = self.hand.members.heard(&run, from, holds);
        self.hand_release(&run, released, now)
    }

    /// The owner would probe run `ask.run` at `runner` (`first`: among the run's first probes, all
    /// in one step): the members to say with it, to send it now, if `runner` is a member or no member
    /// may hold the run; none, the probe held, until every member that may hold it took `runner`
    /// ([`Word::Extend`] to each). A run held where a runner doesn't name `place.handoff` can never
    /// be handed off (that runner can't promise), so nothing waits for its members' word.
    pub(super) fn hand_probe(&mut self, runner: &str, ask: &std::sync::Arc<Ask>, first: bool, now: i64, out: &mut Vec<Out>) -> Option<Vec<String>> {
        let run = ask.run.clone();
        let fenced = self.hand.members.members(&run).iter().all(|m| self.hand.speakers.contains(m));
        match self.hand.members.write(&run, runner, first || !fenced) {
            Verdict::Pass(members) => Some(members),
            Verdict::Ask { members, add } => {
                let handle = Handle::new(&self.me, self.epoch, &run);
                let ballot = self.hand.ballot_of(&run).unwrap_or(Ballot::FIRST);
                for m in members {
                    out.extend(self.hand_tell(&m, Word::Extend { handle: handle.clone(), add: add.clone(), ballot: ballot.clone() }, now));
                }
                self.hand_hold(runner, ask);
                None
            }
            Verdict::Wait => {
                self.hand_hold(runner, ask);
                None
            }
        }
    }

    fn hand_hold(&mut self, runner: &str, ask: &std::sync::Arc<Ask>) {
        if runner == self.me {
            self.hand.held_here.insert(ask.run.clone(), std::sync::Arc::clone(ask));
            return;
        }
        let msg = ToRunner::Probe { ask: std::sync::Arc::clone(ask) };
        let ballot = self.hand.ballot_of(&ask.run);
        if let Ok(json) = serde_json::to_string(&Envelope { epoch: self.epoch, spec: None, ballot, members: Vec::new(), msg }) {
            self.hand.held.insert((ask.run.clone(), runner.to_owned()), json);
        }
    }

    /// Runners a run's members took: their probes go, each saying the members as they are now.
    fn hand_release(&mut self, run: &str, released: Released, now: i64) -> Vec<Out> {
        let mut out = Vec::new();
        for to in released.to {
            if to == self.me {
                if let Some(ask) = self.hand.held_here.remove(run) {
                    let (me, epoch) = (self.me.clone(), self.epoch);
                    let mut said = Vec::new();
                    self.hand.acc.probed(&Handle::new(&me, epoch, run), released.members.iter().cloned());
                    self.probe(&me, epoch, ask, now, &mut said);
                    out.extend(self.said(said, now));
                }
                continue;
            }
            let Some(json) = self.hand.held.remove(&(run.to_owned(), to.clone())) else { continue };
            let json = serde_json::from_str::<Envelope<serde_json::Value>>(&json)
                .ok()
                .and_then(|mut e| {
                    e.members.clone_from(&released.members);
                    serde_json::to_string(&e).ok()
                })
                .unwrap_or(json);
            out.push(Out::Tell { peer: to, json });
        }
        out
    }

    /// The owner's done with run `run` (it ended, was answered or handed off): its members and
    /// probes held go.
    pub(super) fn hand_forget(&mut self, run: &str) {
        self.hand.members.forget(run);
        self.hand.held.retain(|(r, _), _| r != run);
        self.hand.held_here.remove(run);
        self.hand.reclaimed.remove(run);
    }

    /// Says `w` to `peer` on its link (it asked this node): a runner's word to an owner or claimant.
    fn hand_say(&mut self, peer: &str, w: Word, now: i64) -> Vec<Out> {
        if peer == self.me {
            let me = self.me.clone();
            return self.hand_heard(&me, w, now);
        }
        match serde_json::to_string(&Envelope { epoch: self.epoch, spec: None, ballot: None, members: Vec::new(), msg: w }) {
            Ok(json) => vec![Out::Say { peer: peer.to_owned(), json }],
            Err(_) => Vec::new(),
        }
    }

    /// Says `w` to runner `peer`, this node asking.
    fn hand_tell(&mut self, peer: &str, w: Word, now: i64) -> Vec<Out> {
        if peer == self.me {
            let me = self.me.clone();
            return self.hand_told(&me, w, now);
        }
        match serde_json::to_string(&Envelope { epoch: self.epoch, spec: None, ballot: None, members: Vec::new(), msg: w }) {
            Ok(json) => vec![Out::Tell { peer: peer.to_owned(), json }],
            Err(_) => Vec::new(),
        }
    }

    /// Placement's word `msg` to owner `to` (this node's owner, or another's on its link).
    fn say_owner(&mut self, to: &str, msg: ToOwner, now: i64) -> Vec<Out> {
        if to == self.me {
            let me = self.me.clone();
            let acts = self.owner.heard(&me, msg, now);
            return self.acts(acts, now);
        }
        match serde_json::to_string(&Envelope { epoch: self.epoch, spec: None, ballot: None, members: Vec::new(), msg }) {
            Ok(json) => vec![Out::Say { peer: to.to_owned(), json }],
            Err(_) => Vec::new(),
        }
    }

    /// To the claimant that owns run `h`, where it stands here: it runs, or ended.
    fn hand_report(&mut self, h: &Handle, now: i64) -> Vec<Out> {
        let Some(row) = self.hand.acc.row(h).filter(|r| r.owned).cloned() else { return Vec::new() };
        let held = self.admission.holding(Peer { name: &h.owner, epoch: h.epoch }, &h.run);
        self.hand.acc.now_holds(h, held.clone());
        let to = row.ballot.by;
        // A first owner that claimed its run back knows it by its own id.
        let run = if to == h.owner { h.run.clone() } else { h.adopted_id() };
        let msg = match held.clone() {
            Some(Held::Running { fence, .. }) => ToOwner::Started { run, fence },
            Some(Held::Ended { fence, ok, .. }) => ToOwner::Ended { run, fence, ok },
            _ => return Vec::new(),
        };
        if self.hand.suspects.contains_key(&to)
            && let Some(held) = held
        {
            return self.hand_off(h.clone(), held, &to, now);
        }
        self.say_owner(&to, msg, now)
    }

    /// This runner holds the news of run `h` (`held`) and can't reach its owner `owner`: the first
    /// live node in the run's rendezvous order is asked to adopt it (this one, if it's first).
    fn hand_off(&mut self, h: Handle, held: Held, owner: &str, now: i64) -> Vec<Out> {
        if self.hand.handing.get(&h).is_some_and(|a| !self.hand.suspects.contains_key(a)) {
            return Vec::new();
        }
        let live: Vec<&str> = self.hand.connected.iter().filter(|n| n.as_str() != owner && !self.hand.suspects.contains_key(n.as_str())).map(String::as_str).collect();
        let Some(first) = ranked(&h, live).first().map(|s| (*s).to_owned()) else { return Vec::new() };
        self.hand.handing.insert(h.clone(), first.clone());
        let mut members = self.hand.acc.members(&h);
        if !members.contains(&self.me) {
            members.push(self.me.clone());
        }
        let ballot = self.hand.acc.row(&h).map(|r| r.ballot.clone());
        tracing::info!(run = %h.run, owner = %h.owner, to = %first, suspected = %owner, "handing a run off: a node it needs can't be reached");
        if first == self.me {
            let me = self.me.clone();
            return self.hand_adopt(None, h, members, Some((me, held)), ballot, Some(owner.to_owned()), now);
        }
        self.hand_tell(&first, Word::Orphan { handle: h, members, held, ballot, why: owner.to_owned() }, now)
    }

    /// This node adopts run `h`, as runner `from` asked (or itself): it claims it at every member,
    /// above the ballot the news said was promised, a round up from it. A claim under way goes on
    /// unless the news names a ballot at or above it; a run it thought it owned is claimed again.
    #[allow(clippy::too_many_arguments)]
    fn hand_adopt(&mut self, from: Option<&str>, h: Handle, members: Vec<String>, news: Option<(String, Held)>, promised: Option<Ballot>, gone: Option<String>, now: i64) -> Vec<Out> {
        if let Some(g) = gone.filter(|g| *g != self.me) {
            self.hand.suspects.entry(g).or_insert_with(|| "a runner holding its run can't reach it".to_owned());
        }
        if self.hand.claims.get(&h).is_some_and(|c| promised.as_ref().is_none_or(|p| c.ballot > *p)) {
            return Vec::new();
        }
        self.hand.adopted.remove(&h);
        self.hand.claims.remove(&h);
        let me = self.me.clone();
        let claim = match self.hand.yielded.remove(&h) {
            Some(y) if promised.as_ref().is_none_or(|p| y.ballot > *p) => match y.again() {
                Some(c) => c,
                None => {
                    self.hand.yielded.insert(h.clone(), y);
                    return Vec::new();
                }
            },
            _ => Claim::new(h.clone(), Ballot::above(promised.as_ref(), &h, &me), members, news),
        };
        let ballot = claim.ballot.clone();
        let mut out = self.hand_claim(claim, now);
        if let Some(f) = from {
            out.extend(self.hand_say(f, Word::Adopting { handle: h, ballot: Some(ballot), why: None }, now));
        }
        out
    }

    /// Phase 1: the claim goes to each member yet to promise.
    fn hand_claim(&mut self, c: Claim, now: i64) -> Vec<Out> {
        let (h, ballot, members, to) = (c.handle.clone(), c.ballot.clone(), c.members(), c.unpromised());
        self.hand.claims.insert(h.clone(), c);
        let mut out = Vec::new();
        for m in to {
            out.extend(self.hand_tell(&m, Word::Claim { handle: h.clone(), ballot: ballot.clone(), members: members.clone() }, now));
        }
        out.extend(self.hand_decide(&h, now));
        out
    }

    /// A claim yielded to one now gone: made again, a round above it.
    fn hand_again(&mut self, h: &Handle, now: i64) -> Vec<Out> {
        let Some(y) = self.hand.yielded.remove(h) else { return Vec::new() };
        match y.again() {
            Some(c) => self.hand_claim(c, now),
            None => {
                self.hand.yielded.insert(h.clone(), y);
                Vec::new()
            }
        }
    }

    /// What claim `h` comes to: once every member promised, phase 2, `Own` to each, then the run
    /// followed where it started, or closed out where it never did (its agent told either way).
    fn hand_decide(&mut self, h: &Handle, now: i64) -> Vec<Out> {
        let Some(c) = self.hand.claims.get(h) else { return Vec::new() };
        let (decision, ballot, members) = (c.decide(), c.ballot.clone(), c.members());
        let mut out = Vec::new();
        match &decision {
            Decision::Waiting(_) => return out,
            Decision::Yield(b) => {
                let gone = self.hand.suspects.contains_key(&b.by);
                if let Some(c) = self.hand.claims.remove(h) {
                    self.hand.yielded.insert(h.clone(), c);
                }
                if gone {
                    out.extend(self.hand_again(h, now));
                }
                return out;
            }
            Decision::Gone => {
                self.hand.claims.remove(h);
                return out;
            }
            Decision::Runs { .. } | Decision::NotStarted { .. } | Decision::Unsure { .. } => {}
        }
        self.hand.claims.remove(h);
        for m in &members {
            out.extend(self.hand_tell(m, Word::Own { handle: h.clone(), ballot: ballot.clone() }, now));
        }
        let mine = h.owner == self.me;
        match decision {
            Decision::Runs { at, fence, ask, .. } => {
                if mine {
                    // Its own run, claimed back: its words about it go under this claim now.
                    self.hand.reclaimed.insert(h.run.clone(), ballot);
                    return out;
                }
                self.hand.adopted.insert(h.clone(), ballot);
                let id = h.adopted_id();
                let ask = Ask { run: id.clone(), ..ask.unwrap_or_default() };
                let asker = hand_asker(&ask);
                if self.owner.adopt_running(ask, &at, fence) {
                    tracing::info!(run = %h.run, owner = %h.owner, at = %at, "adopted a run whose owner can't be reached: following it there");
                    if let Some(a) = asker.clone() {
                        self.askers.insert(id.clone(), a);
                        self.restored.insert(id);
                    }
                    self.note_changes();
                }
                if let Some(a) = asker {
                    let why = format!("its owner {} can't be reached; {} follows it now", h.owner, self.me);
                    out.push(Out::Told { asker: a, answer: Answer::Started { runner: at, got: Resources::default(), why } });
                }
            }
            Decision::NotStarted { ask } => out.extend(self.hand_close(h, ask, true, now)),
            Decision::Unsure { ask } => out.extend(self.hand_close(h, ask, false, now)),
            Decision::Waiting(_) | Decision::Yield(_) | Decision::Gone => {}
        }
        out
    }

    /// A run that never started (`never`), or may have (a runner it was placed at no longer has it),
    /// closed out: every member took it out of its line as it promised; its agent is told it isn't
    /// started again. Its first owner, claiming it back, tells its own asker so.
    fn hand_close(&mut self, h: &Handle, ask: Option<Ask>, never: bool, now: i64) -> Vec<Out> {
        if h.owner == self.me {
            self.hand_forget(&h.run);
            let me = self.me.clone();
            let acts = self.owner.handed(&h.run, &me);
            return self.acts(acts, now);
        }
        let why = if never {
            format!("went away before it started; its runners handed it to {}, which closed it out: it never started, and isn't started again", self.me)
        } else {
            format!("went away, and a runner it was placed at no longer has it: it may have run there; {} closed out what waited, and it isn't started again", self.me)
        };
        tracing::info!(run = %h.run, owner = %h.owner, never, "closed out a run whose owner can't be reached");
        match ask.as_ref().and_then(hand_asker) {
            Some(asker) => vec![Out::Told { asker, answer: Answer::Lost { runner: h.owner.clone(), why } }],
            None => Vec::new(),
        }
    }
}

/// Who asked a run, as its ask carries them (`Ask::asker`): an agent session's line of it.
fn hand_asker(ask: &Ask) -> Option<crate::reach::told::Answers> {
    ask.asker.as_deref().and_then(|s| serde_json::from_str(s).ok())
}
