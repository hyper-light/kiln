//! The placement kernel at the server, the owner of CI legs (docs/lb.md, "Adopted"; docs/ci.md
//! W05-R03, W05-R04, W05-R23): an ordinary leg goes where the kernel decides, from what the
//! runners said of their room in their hints (the `offers` feature's `fast`, from runners that
//! named `place.fast`) less what the server has sent them since and not yet heard of. When it can't
//! (no member has room, its pick fails what's this job's alone), the engine's own pick decides, as
//! before. Either way the runner's admission, in its one ledger, is the last word on its room.
//!
//! A kind of leg (its labels, constraints and where a person's retry asked it to run) is a
//! signature, its members the runners it may run on as the agents' records stand: synced as those
//! records change, by their address (a record is replaced whole on every change), so a pick reads
//! no record that didn't change since the last.

use std::collections::HashMap;
use std::sync::Arc;

use kiln_ci::lb;
use kiln_ci::place::msg::FastWord;

/// The server's kernel and what it follows.
#[derive(Debug)]
pub(crate) struct Legs {
    lb: lb::Owner,
    out: lb::Out,
    /// Runners by name, interned to the kernel's dense ids, and their budgets as last said.
    ids: HashMap<String, u32>,
    names: Vec<String>,
    budgets: Vec<Option<lb::Amounts>>,
    /// Each kind of leg's signature.
    sigs: HashMap<u64, Sig>,
    /// Each leg the kernel sent and not yet heard of (started, refused, ended): its handle.
    sent: HashMap<String, lb::Handle>,
}

#[derive(Debug)]
struct Sig {
    id: lb::SigId,
    /// Each runner as last looked at: its record's address and whether it's a member.
    seen: HashMap<u32, (usize, bool)>,
}

impl Default for Legs {
    fn default() -> Self {
        Legs { lb: lb::Owner::new(lb::OwnerId(0)), out: lb::Out::default(), ids: HashMap::new(), names: Vec::new(), budgets: Vec::new(), sigs: HashMap::new(), sent: HashMap::new() }
    }
}

impl Legs {
    fn id(&mut self, name: &str) -> u32 {
        if let Some(r) = self.ids.get(name) {
            return *r;
        }
        let r = u32::try_from(self.names.len()).unwrap_or(u32::MAX);
        self.ids.insert(name.to_owned(), r);
        self.names.push(name.to_owned());
        self.budgets.push(None);
        r
    }

    fn joined(&self, r: u32) -> bool {
        self.budgets.get(r as usize).is_some_and(Option::is_some)
    }

    /// Runner `runner`'s kernel word, from its hint: it joins the kernel at its first, its budget
    /// followed, its room what the kernel picks by from now. O(1) but on joining.
    pub(crate) fn hint(&mut self, runner: &str, w: &FastWord) {
        let r = self.id(runner);
        self.out.clear();
        match self.budgets.get(r as usize).copied().flatten() {
            None => {
                self.lb.join(lb::RunnerId(r), w.budget, w.word(), &mut self.out);
                if let Some(b) = self.budgets.get_mut(r as usize) {
                    *b = Some(w.budget);
                }
                // Its membership is looked at again on each kind's next pick.
                for s in self.sigs.values_mut() {
                    s.seen.remove(&r);
                }
            }
            Some(b) => {
                if b != w.budget {
                    self.lb.set_budget(lb::RunnerId(r), w.budget);
                    if let Some(b) = self.budgets.get_mut(r as usize) {
                        *b = Some(w.budget);
                    }
                }
                let _ = self.lb.hear(lb::ToOwner::Hint { runner: lb::RunnerId(r), word: w.word() }, &mut self.out);
            }
        }
        self.out.clear();
    }

    /// The kernel's signature for kind `key`, its members synced with `agents`: each record that
    /// changed since it was last looked at is looked at again (`member`), and runners gone from
    /// `agents` leave it.
    fn sync<A>(&mut self, key: u64, agents: &HashMap<String, Arc<A>>, name: impl Fn(&A) -> &str, member: impl Fn(&A) -> bool) -> lb::SigId {
        if !self.sigs.contains_key(&key) {
            let id = self.lb.sig();
            self.sigs.insert(key, Sig { id, seen: HashMap::new() });
        }
        let mut changes: Vec<(u32, usize, bool)> = Vec::new();
        let mut here: Vec<u32> = Vec::with_capacity(agents.len());
        for a in agents.values() {
            let r = self.id(name(a));
            here.push(r);
            let at = Arc::as_ptr(a) as usize;
            let seen = self.sigs.get(&key).and_then(|s| s.seen.get(&r)).copied();
            if seen.is_some_and(|(p, _)| p == at) {
                continue;
            }
            let m = self.joined(r) && member(a);
            changes.push((r, at, m));
        }
        let Some(s) = self.sigs.get_mut(&key) else { return lb::SigId(0) };
        let id = s.id;
        let mut moves: Vec<(u32, bool)> = Vec::new();
        for (r, at, m) in changes {
            let was = s.seen.insert(r, (at, m)).is_some_and(|(_, w)| w);
            if was != m {
                moves.push((r, m));
            }
        }
        here.sort_unstable();
        let gone: Vec<u32> = s.seen.keys().copied().filter(|r| here.binary_search(r).is_err()).collect();
        for r in gone {
            if s.seen.remove(&r).is_some_and(|(_, w)| w) {
                moves.push((r, false));
            }
        }
        self.out.clear();
        for (r, m) in moves {
            if m {
                self.lb.add(id, lb::RunnerId(r), &mut self.out);
            } else {
                self.lb.remove(id, lb::RunnerId(r));
            }
        }
        self.out.clear();
        id
    }

    /// The kernel's pick for a leg of kind `key` needing `need` (CPUs in thousandths, memory,
    /// disk, GPUs): the member with the most room that covers it, its need taken from the
    /// kernel's view of it at once (`sent` once the caller sends it, `unsent` if it doesn't).
    /// `None`: no member has room now.
    pub(crate) fn pick<A>(&mut self, key: u64, need: lb::Amounts, agents: &HashMap<String, Arc<A>>, name: impl Fn(&A) -> &str, member: impl Fn(&A) -> bool) -> Option<(String, lb::Handle)> {
        if self.ids.is_empty() || !self.budgets.iter().any(Option::is_some) {
            return None;
        }
        let sig = self.sync(key, agents, name, member);
        self.out.clear();
        let asked = self.lb.ask(sig, need, &mut self.out);
        self.out.clear();
        match asked {
            Ok(lb::Asked { run, send: Some((r, _)) }) => match self.names.get(r.0 as usize) {
                Some(n) => Some((n.clone(), run)),
                None => {
                    let _ = self.lb.unsent(run);
                    None
                }
            },
            Ok(lb::Asked { run, .. }) => {
                // Waiting is the engine's (its queue), never the kernel's.
                let _ = self.lb.cancel(run, &mut self.out);
                self.out.clear();
                None
            }
            Err(_) => None,
        }
    }

    /// Leg `job` went to the runner the kernel picked for it under `run`.
    pub(crate) fn sent(&mut self, job: &str, run: lb::Handle) {
        if let Some(old) = self.sent.insert(job.to_owned(), run) {
            let _ = self.lb.unsent(old);
        }
    }

    /// A pick not sent (the job's own conditions refused it, or its claim didn't take): its room
    /// is back in the kernel's view at once.
    pub(crate) fn unsent(&mut self, run: lb::Handle) {
        let _ = self.lb.unsent(run);
    }

    /// Leg `job` was heard of at its runner (it started there, was refused, or left it): the room
    /// it took is in that runner's own words from now, no longer the server's to take off.
    pub(crate) fn heard(&mut self, job: &str) {
        if let Some(run) = self.sent.remove(job) {
            let _ = self.lb.unsent(run);
        }
    }

    /// The kind of leg a job is, by what decides which runners it may run on.
    pub(crate) fn kind(labels: &[String], requires: &[String], placement: &str) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        labels.hash(&mut h);
        requires.hash(&mut h);
        placement.hash(&mut h);
        h.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct Rec {
        name: String,
        ok: bool,
    }

    fn word(cpus: u64) -> FastWord {
        FastWord { epoch: 1, room: [cpus * 1000, 0, 0, 0], whole: true, budget: [8000, 64 << 30, 0, 0], ..FastWord::default() }
    }

    fn fleet(names: &[(&str, bool)]) -> HashMap<String, Arc<Rec>> {
        names.iter().map(|(n, ok)| ((*n).to_owned(), Arc::new(Rec { name: (*n).to_owned(), ok: *ok }))).collect()
    }

    #[test]
    fn w05_r23_the_server_s_kernel_picks_the_roomiest_member_and_takes_its_room_at_once() -> anyhow::Result<()> {
        let mut legs = Legs::default();
        let agents = fleet(&[("a", true), ("b", true), ("c", false)]);
        legs.hint("a", &word(2));
        legs.hint("b", &word(6));
        legs.hint("c", &word(8));
        let key = Legs::kind(&["linux".into()], &[], "");
        // c has the most room but isn't one this kind may run on.
        let (first, run) = legs.pick(key, [4000, 0, 0, 0], &agents, |r| &r.name, |r| r.ok).ok_or_else(|| anyhow::anyhow!("no pick"))?;
        anyhow::ensure!(first == "b", "picked {first}");
        legs.sent("j1", run);
        // b's room is taken in the kernel's view: a second leg of 4 fits nowhere now.
        anyhow::ensure!(legs.pick(key, [4000, 0, 0, 0], &agents, |r| &r.name, |r| r.ok).is_none(), "picked twice into b's room");
        // Heard of (started there): its room is b's own word's from now.
        legs.heard("j1");
        legs.hint("b", &word(2));
        let (next, _) = legs.pick(key, [2000, 0, 0, 0], &agents, |r| &r.name, |r| r.ok).ok_or_else(|| anyhow::anyhow!("no pick"))?;
        anyhow::ensure!(next == "a" || next == "b", "picked {next}");
        Ok(())
    }

    #[test]
    fn w05_r04_a_runner_that_changes_or_goes_leaves_its_kinds() -> anyhow::Result<()> {
        let mut legs = Legs::default();
        let mut agents = fleet(&[("a", true), ("b", true)]);
        legs.hint("a", &word(8));
        legs.hint("b", &word(4));
        let key = Legs::kind(&[], &[], "");
        let (p, run) = legs.pick(key, [1000, 0, 0, 0], &agents, |r| &r.name, |r| r.ok).ok_or_else(|| anyhow::anyhow!("no pick"))?;
        anyhow::ensure!(p == "a", "picked {p}");
        legs.unsent(run);
        // a's record changes: it may no longer take this kind.
        agents.insert("a".into(), Arc::new(Rec { name: "a".into(), ok: false }));
        let (p, run) = legs.pick(key, [1000, 0, 0, 0], &agents, |r| &r.name, |r| r.ok).ok_or_else(|| anyhow::anyhow!("no pick"))?;
        anyhow::ensure!(p == "b", "picked {p}");
        legs.unsent(run);
        // b leaves the fleet.
        agents.remove("b");
        anyhow::ensure!(legs.pick(key, [1000, 0, 0, 0], &agents, |r| &r.name, |r| r.ok).is_none(), "picked a runner gone");
        Ok(())
    }
}
