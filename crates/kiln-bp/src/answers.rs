//! Requests taken once per id (docs/browser-protocol.md, "Requests and answers"; docs/wire.md,
//! "The UI's sockets"). A request is run the first time its id comes; sent again while it runs,
//! it waits for the same answer; sent again after a final answer, it gets that answer; after
//! `Parked`, it's answered `Parked` until the real answer takes its place; after a not-this-time
//! answer it runs again. Kept per caller (ids are their caller's own), across that caller's
//! connections, so a resend after a reconnect is answered once, as the first was.

use std::collections::{BTreeMap, VecDeque};

use crate::frames::{Finality, Mark, Outcome};

/// How many final answers are kept per caller, and for how long.
pub const KEEP_ANSWERS: usize = 1024;
pub const KEEP_ANSWERS_MS: u64 = 10 * 60 * 1000;

/// What to do with a request that came.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Taking {
    /// Run it: the first time its id came (or its last answer was not this time).
    Run,
    /// It's running: its answer comes to this connection too, when it comes.
    Wait,
    /// Answer it with what it was answered (a final answer, or `Parked` while it waits).
    Kept { outcome: Outcome, at: Option<Mark> },
}

#[derive(Debug, Clone)]
struct Kept {
    outcome: Outcome,
    at: Option<Mark>,
    when: u64,
}

#[derive(Debug, Default, Clone)]
struct Caller {
    /// Requests running, with the connections waiting for each.
    running: BTreeMap<u64, Vec<u64>>,
    kept: BTreeMap<u64, Kept>,
    /// Final answers' ids, oldest first.
    order: VecDeque<u64>,
}

/// Every caller's requests.
#[derive(Debug, Default, Clone)]
pub struct Answers {
    callers: BTreeMap<String, Caller>,
}

impl Answers {
    pub fn new() -> Self {
        Self::default()
    }

    fn prune(c: &mut Caller, now: u64) {
        while let Some(id) = c.order.front().copied() {
            let old = c.kept.get(&id).is_none_or(|k| now.saturating_sub(k.when) > KEEP_ANSWERS_MS);
            if c.order.len() > KEEP_ANSWERS || old {
                c.order.pop_front();
                if c.kept.get(&id).is_some_and(|k| k.outcome.finality() == Finality::Final) {
                    c.kept.remove(&id);
                }
            } else {
                break;
            }
        }
    }

    /// A request `id` of `caller` came on connection `conn`.
    pub fn begin(&mut self, caller: &str, id: u64, conn: u64, now: u64) -> Taking {
        let c = self.callers.entry(caller.to_owned()).or_default();
        Self::prune(c, now);
        if let Some(k) = c.kept.get(&id) {
            let kept = Taking::Kept { outcome: k.outcome.clone(), at: k.at.clone() };
            if k.outcome.finality() == Finality::ForNow {
                let w = c.running.entry(id).or_default();
                if !w.contains(&conn) {
                    w.push(conn);
                }
            }
            return kept;
        }
        if let Some(w) = c.running.get_mut(&id) {
            if !w.contains(&conn) {
                w.push(conn);
            }
            return Taking::Wait;
        }
        c.running.insert(id, vec![conn]);
        Taking::Run
    }

    /// Its answer: the connections to send it to.
    pub fn finish(&mut self, caller: &str, id: u64, outcome: &Outcome, at: Option<&Mark>, now: u64) -> Vec<u64> {
        let c = self.callers.entry(caller.to_owned()).or_default();
        let waiting = match outcome.finality() {
            Finality::Final => {
                c.kept.insert(id, Kept { outcome: outcome.clone(), at: at.cloned(), when: now });
                c.order.push_back(id);
                c.running.remove(&id).unwrap_or_default()
            }
            Finality::ForNow => {
                c.kept.insert(id, Kept { outcome: outcome.clone(), at: at.cloned(), when: now });
                c.running.get(&id).cloned().unwrap_or_default()
            }
            Finality::NotThisTime => {
                c.kept.remove(&id);
                c.running.remove(&id).unwrap_or_default()
            }
        };
        Self::prune(c, now);
        waiting
    }

    /// A connection went: it waits for nothing any more.
    pub fn gone(&mut self, conn: u64) {
        for c in self.callers.values_mut() {
            for w in c.running.values_mut() {
                w.retain(|x| *x != conn);
            }
        }
    }

    /// Final answers kept for `caller`.
    pub fn kept(&self, caller: &str) -> usize {
        self.callers.get(caller).map_or(0, |c| c.kept.len())
    }
}
