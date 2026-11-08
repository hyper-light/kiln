//! An owner's side of a run's members (LB03): the runners a run could start on grow only with the
//! word of every member that may hold it, so a claim's phase 1, made at the members, reaches every
//! place the run could start. Without it, an owner that was thought gone could probe a run at new
//! runners and start it there while a claimant, its promises in hand, closes it out.

use std::collections::{BTreeMap, BTreeSet};

/// Each run's members as its owner knows them: whether each may hold it, and the runners waiting
/// to be added, each with the members still to take it.
#[derive(Debug, Default)]
pub struct Members {
    runs: BTreeMap<String, Config>,
}

#[derive(Debug, Default)]
struct Config {
    /// Each member, and whether it may hold the run (false: it said it never could, or the run was
    /// taken from it).
    members: BTreeMap<String, bool>,
    /// Runners to add, each with the members yet to take it.
    adding: BTreeMap<String, BTreeSet<String>>,
}

impl Config {
    /// The members, and the runners being added (a member probed now knows them all, so it can
    /// name them should it hold the news).
    fn known(&self) -> Vec<String> {
        let mut k: BTreeSet<String> = self.members.keys().cloned().collect();
        k.extend(self.adding.keys().cloned());
        k.into_iter().collect()
    }
}

/// What to do with a word that would put a run at a runner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Send it, saying these members with it.
    Pass(Vec<String>),
    /// Hold it until each of `members` took runner `add` ([`super::Word::Extend`] to each).
    Ask { members: Vec<String>, add: String },
    /// Hold it: the runner is being added already.
    Wait,
}

/// What a member's word let go: runners added, each with the members to say with its probe.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Released {
    pub to: Vec<String>,
    pub members: Vec<String>,
}

impl Members {
    /// The owner would probe (or start with no probe) run `run` at runner `to`. `first`: the run's
    /// first words, all in one step, which make its first members.
    pub fn write(&mut self, run: &str, to: &str, first: bool) -> Verdict {
        let c = self.runs.entry(run.to_owned()).or_default();
        if c.members.contains_key(to) {
            return Verdict::Pass(c.known());
        }
        if c.adding.contains_key(to) {
            return Verdict::Wait;
        }
        // No member may hold it: none can hand it off, so none need say.
        let free = first || c.members.values().all(|may| !may);
        #[cfg(test)]
        let free = free || super::mutant() == Some(super::Mutant::SkipExtend);
        if free {
            c.members.insert(to.to_owned(), true);
            return Verdict::Pass(c.known());
        }
        let ask: BTreeSet<String> = c.members.iter().filter(|(_, may)| **may).map(|(m, _)| m.clone()).collect();
        c.adding.insert(to.to_owned(), ask.clone());
        Verdict::Ask { members: ask.into_iter().collect(), add: to.to_owned() }
    }

    /// What member `from` said of run `run`: whether it may hold it. One that can't any more takes
    /// nothing more, and is waited for by no runner being added.
    pub fn heard(&mut self, run: &str, from: &str, holds: bool) -> Released {
        let Some(c) = self.runs.get_mut(run) else { return Released::default() };
        if let Some(may) = c.members.get_mut(from) {
            *may = holds;
        }
        if holds {
            return Released::default();
        }
        for waiting in c.adding.values_mut() {
            waiting.remove(from);
        }
        Self::release(c)
    }

    /// Member `from` took runner `add` into run `run`'s members.
    pub fn extended(&mut self, run: &str, from: &str, add: &str) -> Released {
        let Some(c) = self.runs.get_mut(run) else { return Released::default() };
        if let Some(waiting) = c.adding.get_mut(add) {
            waiting.remove(from);
        }
        Self::release(c)
    }

    fn release(c: &mut Config) -> Released {
        let done: Vec<String> = c.adding.iter().filter(|(_, w)| w.is_empty()).map(|(t, _)| t.clone()).collect();
        // A runner added learns those still being added from its probe's members: it needn't take
        // them again.
        for t in &done {
            c.adding.remove(t);
            c.members.insert(t.clone(), true);
        }
        Released { to: done, members: c.known() }
    }

    /// The members still to take a runner being added to run `run` (to ask again once a link to
    /// one is back), with the runner.
    pub fn asking(&self, run: &str) -> Vec<(String, String)> {
        let Some(c) = self.runs.get(run) else { return Vec::new() };
        c.adding.iter().flat_map(|(t, w)| w.iter().map(move |m| (m.clone(), t.clone()))).collect()
    }

    /// Whether run `run` has members yet (its first probes went).
    pub fn knows(&self, run: &str) -> bool {
        self.runs.contains_key(run)
    }

    /// Run `run`'s members as said with a probe.
    pub fn members(&self, run: &str) -> Vec<String> {
        self.runs.get(run).map(Config::known).unwrap_or_default()
    }

    /// The owner's done with run `run` (it ended, was answered, or handed off).
    pub fn forget(&mut self, run: &str) {
        self.runs.remove(run);
    }

    /// The runs whose members include `member` with words held for it: their runners being added.
    pub fn runs_asking(&self, member: &str) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for (run, c) in &self.runs {
            for (t, w) in &c.adding {
                if w.contains(member) {
                    out.push((run.clone(), t.clone()));
                }
            }
        }
        out
    }
}
