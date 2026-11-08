//! A member's side of a hand-off (LB03): the promises a runner gives for the runs it holds, kept on
//! its disk as Paxos' acceptors keep theirs, and the gate every word about a run passes through.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::{Ballot, Handle};
use crate::place::ask::Ask;
use crate::sched::state::Fence;

/// What a member holds of a run, as its promise says.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Held {
    /// It waits here and never started here: in the line, set aside, offered or taken.
    Waiting { ask: Ask },
    /// It runs here, under claim `fence`.
    Running {
        fence: Fence,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ask: Option<Ask>,
    },
    /// It ended here under claim `fence` (`ok`: its process exited), and its owner hasn't heard.
    Ended {
        fence: Fence,
        ok: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ask: Option<Ask>,
    },
}

impl Held {
    pub fn ask(&self) -> Option<&Ask> {
        match self {
            Held::Waiting { ask } => Some(ask),
            Held::Running { ask, .. } | Held::Ended { ask, .. } => ask.as_ref(),
        }
    }

    /// It started here.
    pub fn started(&self) -> bool {
        !matches!(self, Held::Waiting { .. })
    }
}

/// A member's promise for a run, as its disk keeps it: the ballot promised, whether its claimant
/// said it owns the run, what was held here as it promised, and the members it knew.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Row {
    pub ballot: Ballot,
    #[serde(default)]
    pub owned: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub held: Option<Held>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<String>,
}

/// A promise given: the ballot, what's held here, the members this member knows.
#[derive(Debug, Clone, PartialEq)]
pub struct Promise {
    pub ballot: Ballot,
    pub held: Option<Held>,
    pub members: Vec<String>,
}

/// What becomes of a word about a run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gate {
    /// Taken.
    Pass,
    /// Refused: this ballot was promised (to its `by`), and the word's sender is below it.
    Refuse(Ballot),
}

/// A runner's promises (kept), and the members of each run it holds (not kept: they go with its
/// process, as its probes do; every member that may hold a run takes each one added, so whichever
/// holds the news knows them all).
#[derive(Debug, Default)]
pub struct Acceptor {
    rows: BTreeMap<Handle, Row>,
    soft: BTreeMap<Handle, BTreeSet<String>>,
    changed: BTreeSet<Handle>,
}

impl Acceptor {
    /// The promises its disk kept, as its process starts.
    pub fn restore(rows: impl IntoIterator<Item = (Handle, Row)>) -> Acceptor {
        let rows = rows.into_iter();
        #[cfg(test)]
        let rows = rows.filter(|_| super::mutant() != Some(super::Mutant::ForgetPromise));
        Acceptor { rows: rows.collect(), soft: BTreeMap::new(), changed: BTreeSet::new() }
    }

    /// A word about run `h` under `ballot` (its first owner's, [`Ballot::FIRST`], or the claim its
    /// sender owns it by): taken if no claim was promised, or if it's the claim promised and that
    /// claim owns the run; refused with the ballot promised otherwise. By ballot, never by sender:
    /// a first owner that claimed its run back has its own words from before refused too.
    ///
    /// The same run under another of its owner's epochs is the same run (LB03): an owner that
    /// started again asks its kept runs again under its new epoch, and a claim promised for one
    /// before still holds it. Words of the new epoch are refused with that claim's ballot, unless
    /// the claim was the owner's own (it claimed the run back before it started again, and its
    /// ballot went with its process).
    pub fn gate(&self, h: &Handle, ballot: &Ballot) -> Gate {
        #[cfg(test)]
        if super::mutant() == Some(super::Mutant::AcceptLowerBallot) {
            return Gate::Pass;
        }
        if let Some(r) = self.rows.get(h) {
            return if r.owned && r.ballot == *ballot { Gate::Pass } else { Gate::Refuse(r.ballot.clone()) };
        }
        if self.rows.is_empty() {
            return Gate::Pass;
        }
        #[cfg(test)]
        if super::mutant() == Some(super::Mutant::EpochBlind) {
            return Gate::Pass;
        }
        let first = Handle { owner: h.owner.clone(), epoch: 0, run: String::new() };
        let before = self.rows.range(first..).take_while(|(k, _)| k.owner == h.owner).find(|(k, r)| k.run == h.run && k.epoch != h.epoch && r.ballot.by != h.owner);
        match before {
            Some((_, r)) => Gate::Refuse(r.ballot.clone()),
            None => Gate::Pass,
        }
    }

    /// Phase 1: claimant `ballot.by` asks for run `h`, holding here as `held` says now (none: nothing
    /// of it here), knowing `members`. Promised, with what's held and the members known, if the
    /// ballot is above every ballot promised for it and above the first owner's, or is the one
    /// promised (its claimant asking again: a ballot names one claimant, so it's never a duel);
    /// refused with the one promised otherwise. A run promised away is no longer the first owner's
    /// here: the shell takes a run that waits here out of its line once it promised (its ask is kept
    /// in the promise).
    pub fn claim(&mut self, h: &Handle, ballot: &Ballot, held: Option<Held>, members: &[String]) -> Result<Promise, Ballot> {
        let prior = self.rows.get(h);
        let promised = prior.map_or(Ballot::FIRST, |r| r.ballot.clone());
        #[cfg(test)]
        let lower_taken = super::mutant() == Some(super::Mutant::AcceptLowerBallot);
        #[cfg(not(test))]
        let lower_taken = false;
        if *ballot <= Ballot::FIRST || (*ballot < promised && !lower_taken) {
            return Err(promised);
        }
        if *ballot == promised
            && let Some(r) = prior
        {
            let held = held.or_else(|| r.held.clone());
            return Ok(Promise { ballot: promised, held, members: self.members(h) });
        }
        let mut known: BTreeSet<String> = members.iter().cloned().collect();
        known.extend(prior.map(|r| r.members.iter().cloned()).into_iter().flatten());
        known.extend(self.soft.get(h).into_iter().flatten().cloned());
        let held = held.or_else(|| prior.and_then(|r| r.held.clone()));
        let members: Vec<String> = known.into_iter().collect();
        self.rows.insert(h.clone(), Row { ballot: ballot.clone(), owned: false, held: held.clone(), members: members.clone() });
        self.changed.insert(h.clone());
        Ok(Promise { ballot: ballot.clone(), held, members })
    }

    /// The claimant under the ballot promised says it owns run `h` (every member promised it): its
    /// words about the run are taken from now on, and the run's go to it. False if another ballot
    /// was promised since.
    pub fn own(&mut self, h: &Handle, ballot: &Ballot) -> bool {
        match self.rows.get_mut(h) {
            Some(r) if r.ballot == *ballot => {
                if !r.owned {
                    r.owned = true;
                    self.changed.insert(h.clone());
                }
                true
            }
            _ => false,
        }
    }

    /// What's held of run `h` changed (it ended): kept in its promise, for a later claim to hear.
    pub fn now_holds(&mut self, h: &Handle, held: Option<Held>) {
        if let Some(r) = self.rows.get_mut(h)
            && r.held != held
        {
            r.held = held;
            self.changed.insert(h.clone());
        }
    }

    pub fn row(&self, h: &Handle) -> Option<&Row> {
        self.rows.get(h)
    }

    pub fn rows(&self) -> impl Iterator<Item = (&Handle, &Row)> {
        self.rows.iter()
    }

    /// The runs whose promise names claimant `by` as their owner.
    pub fn owned_by(&self, by: &str) -> Vec<Handle> {
        self.rows.iter().filter(|(_, r)| r.owned && r.ballot.by == by).map(|(h, _)| h.clone()).collect()
    }

    /// Run `h` came here with these members (its probe's), or more were added: known while it's here.
    pub fn probed(&mut self, h: &Handle, members: impl IntoIterator<Item = String>) {
        self.soft.entry(h.clone()).or_default().extend(members);
    }

    /// Its owner adds runners to run `h`'s members before it probes them there
    /// ([`super::Word::Extend`]), under `ballot`: taken as [`Self::gate`] takes a word.
    pub fn extend(&mut self, h: &Handle, ballot: &Ballot, add: &[String]) -> Result<(), Ballot> {
        if let Gate::Refuse(b) = self.gate(h, ballot) {
            return Err(b);
        }
        self.probed(h, add.iter().cloned());
        Ok(())
    }

    /// The members this runner knows of run `h`.
    pub fn members(&self, h: &Handle) -> Vec<String> {
        let mut m: BTreeSet<String> = self.soft.get(h).cloned().unwrap_or_default();
        m.extend(self.rows.get(h).map(|r| r.members.iter().cloned()).into_iter().flatten());
        m.into_iter().collect()
    }

    /// Run `h` isn't held here any more (withdrawn, or its end heard): its members go with it.
    pub fn unheld(&mut self, h: &Handle) {
        self.soft.remove(h);
    }

    /// Run `h`'s promise goes: its first owner can't speak of it again (it's gone from the
    /// directory, or its newer process heard it was handed off), and nothing of it is held here.
    pub fn forget(&mut self, h: &Handle) {
        if self.rows.remove(h).is_some() {
            self.changed.insert(h.clone());
        }
        self.soft.remove(h);
    }

    /// The promises changed since this was last asked, each as it is now (`None`: gone), for the
    /// shell to keep on its disk before any word that rests on them goes.
    pub fn changes(&mut self) -> Vec<(Handle, Option<Row>)> {
        std::mem::take(&mut self.changed).into_iter().map(|h| (h.clone(), self.rows.get(&h).cloned())).collect()
    }

    /// The process starts again: its promises as kept, nothing else.
    pub fn restarted(&mut self) {
        let rows = std::mem::take(&mut self.rows);
        *self = Acceptor::restore(rows);
    }
}
