//! A claimant's side of a hand-off (LB03): phase 1 of a run's Paxos at every member it learns of,
//! and what it decides once each has promised.

use std::collections::{BTreeMap, BTreeSet};

use super::{Ballot, Handle, Held};
use crate::place::ask::Ask;
use crate::sched::state::Fence;

/// One claim of a run: its ballot, the members known (the news', and every promise's), what each
/// member that promised holds, and a higher ballot found, if one was.
#[derive(Debug, Clone)]
pub struct Claim {
    pub handle: Handle,
    pub ballot: Ballot,
    members: BTreeSet<String>,
    promised: BTreeMap<String, Option<Held>>,
    higher: Option<Ballot>,
    /// What the runner holding the news said it holds: what a planted bug acts on alone.
    #[cfg_attr(not(test), allow(dead_code))]
    news: Option<(String, Held)>,
}

/// What a claim comes to.
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    /// These members are still to promise: nothing is decided (a member that can't be reached
    /// holds the run here until it can).
    Waiting(Vec<String>),
    /// A member promised a higher ballot: its claimant goes on, and this one yields.
    Yield(Ballot),
    /// Every member promised, and the run runs or ran at `at` under `fence` (`ended`: how, once it
    /// did): the claimant follows it there, its owner now.
    Runs { at: String, fence: Fence, ended: Option<bool>, ask: Option<Ask> },
    /// Every member promised and every one holds it waiting: it never started, and the claimant
    /// closes it out.
    NotStarted { ask: Option<Ask> },
    /// Every member promised, some hold it waiting and some nothing: a member with nothing may
    /// have run it and had its end heard (its record goes then), or never had it. Whether it ran
    /// isn't known; the claimant closes out what waits, and says so.
    Unsure { ask: Option<Ask> },
    /// Every member promised and none holds anything of it: its first owner withdrew it, or its
    /// end was heard. Nothing to adopt.
    Gone,
}

impl Claim {
    /// A claim of `handle` under `ballot`, at `members` (the news', its holder among them), the
    /// holder having said it holds `news`.
    pub fn new(handle: Handle, ballot: Ballot, members: impl IntoIterator<Item = String>, news: Option<(String, Held)>) -> Claim {
        Claim { handle, ballot, members: members.into_iter().collect(), promised: BTreeMap::new(), higher: None, news }
    }

    pub fn members(&self) -> Vec<String> {
        self.members.iter().cloned().collect()
    }

    /// The members still to promise: each is sent the claim.
    pub fn unpromised(&self) -> Vec<String> {
        self.members.iter().filter(|m| !self.promised.contains_key(*m)).cloned().collect()
    }

    /// Member `member` promised `ballot`, holding `held`, knowing `members`: the members new to
    /// this claim, which are claimed at too. A promise of another ballot is let go.
    pub fn promised(&mut self, member: &str, ballot: &Ballot, held: Option<Held>, members: &[String]) -> Vec<String> {
        if *ballot != self.ballot {
            return Vec::new();
        }
        self.members.insert(member.to_owned());
        self.promised.insert(member.to_owned(), held);
        let new: Vec<String> = members.iter().filter(|m| !self.members.contains(*m)).cloned().collect();
        self.members.extend(new.iter().cloned());
        new
    }

    /// A member promised `higher`, above this claim: it yields to it.
    pub fn refused(&mut self, higher: Ballot) {
        if higher > self.ballot && self.higher.as_ref().is_none_or(|h| higher > *h) {
            self.higher = Some(higher);
        }
    }

    /// The claim after a yield, once the one yielded to is gone: one round above it. None if it
    /// never yielded.
    pub fn again(&self) -> Option<Claim> {
        let over = self.higher.as_ref()?;
        let ballot = Ballot::above(Some(over), &self.handle, &self.ballot.by);
        Some(Claim { handle: self.handle.clone(), ballot, members: self.members.clone(), promised: BTreeMap::new(), higher: None, news: self.news.clone() })
    }

    /// What it comes to now.
    pub fn decide(&self) -> Decision {
        if let Some(h) = &self.higher {
            return Decision::Yield(h.clone());
        }
        #[cfg(test)]
        if super::mutant() == Some(super::Mutant::AdoptWithoutPromise)
            && let Some((at, held)) = &self.news
        {
            return of_held(std::iter::once((at, held)));
        }
        let missing = self.unpromised();
        #[cfg(test)]
        let missing = if super::mutant() == Some(super::Mutant::SkipPlace) && missing.len() == 1 && self.members.len() > 1 { Vec::new() } else { missing };
        if !missing.is_empty() {
            return Decision::Waiting(missing);
        }
        let decided = of_held(self.promised.iter().filter_map(|(m, h)| h.as_ref().map(|h| (m, h))));
        // Never started only if every member still holds it waiting: one with nothing may have run
        // it, its record gone with its owner's acknowledgement.
        match decided {
            Decision::NotStarted { ask } if self.promised.values().any(Option::is_none) => Decision::Unsure { ask },
            d => d,
        }
    }
}

/// What the members' holdings say of the run: started at the first that holds it started (the
/// protocol leaves at most one), else waiting where any holds it, else gone.
fn of_held<'a>(held: impl Iterator<Item = (&'a String, &'a Held)> + Clone) -> Decision {
    if let Some((at, h)) = held.clone().find(|(_, h)| h.started()) {
        let (fence, ended) = match h {
            Held::Running { fence, .. } => (*fence, None),
            Held::Ended { fence, ok, .. } => (*fence, Some(*ok)),
            Held::Waiting { .. } => (Fence::default(), None),
        };
        return Decision::Runs { at: at.clone(), fence, ended, ask: h.ask().cloned() };
    }
    let mut held = held;
    match held.next() {
        Some((_, h)) => Decision::NotStarted { ask: h.ask().cloned() },
        None => Decision::Gone,
    }
}
