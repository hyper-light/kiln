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
//!
//! A run's ordinary legs of one kind and need that are waiting in one look at the queue arrive in
//! one step: with `[reach] auction` declared they go together where the kernel's integer auction
//! puts them (docs/lb.md "The auction"; LB08, W05-R26), at the least total expected finish, when
//! their times and the members' rates tell them apart ([`Legs::pick_batch`]); else one at a time.

use std::collections::HashMap;
use std::sync::Arc;

use kiln_ci::lb;
use kiln_ci::place::msg::FastWord;

/// A pick: the runner's name, and the kernel's run its room is taken under.
pub(crate) type Pick = (String, lb::Handle);

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
    /// A run's legs that arrive together go where the integer auction puts them (`[reach]
    /// auction`): off until declared.
    auction: bool,
}

#[derive(Debug)]
struct Sig {
    id: lb::SigId,
    /// Each runner as last looked at: its record's address and whether it's a member.
    seen: HashMap<u32, (usize, bool)>,
}

impl Default for Legs {
    fn default() -> Self {
        Legs { lb: lb::Owner::new(lb::OwnerId(0)), out: lb::Out::default(), ids: HashMap::new(), names: Vec::new(), budgets: Vec::new(), sigs: HashMap::new(), sent: HashMap::new(), auction: false }
    }
}

/// What runners' connections say to the server's kernel between the scheduling loop's looks
/// (docs/lb.md "At scale"): each runner's newest word, the jobs heard of at their runners, and each
/// runner's pace as last passed to the CI engine. A connection puts its runner's word here with no
/// lock (goal 5, plan 01a1a59c2aae), never in the kernel's own records: the scheduling loop takes it
/// whole as it looks ([`Mail::take`]) and applies it ([`Legs::apply`]), so the kernel's records are
/// written by that loop alone and stay in its core's cache. A lock the connections' cores wrote the
/// records through cost a decision 2.2x to 58x as one to eight of them hammered it (`kiln_ci`'s
/// bench, `--hinters`). Bounded by the fleet: one word a runner, the newest; the jobs heard of are
/// those started, refused or ended between two looks.
pub(crate) struct Mail {
    /// Each runner's newest word since the last look, in a take-once cell of its own: the cells' map
    /// changes only as a runner first speaks.
    words: crate::rcu::Rcu<HashMap<String, Arc<arc_swap::ArcSwapOption<FastWord>>>>,
    /// The jobs heard of, in the order they were.
    heard: (crate::memory::queue::Sender<String>, crate::memory::queue::Receiver<String>),
    /// Each runner's pace as last passed on, changed only when it does.
    paces: crate::rcu::Rcu<HashMap<String, kiln_ci::sched::balance::Pace>>,
}

impl Default for Mail {
    fn default() -> Self {
        Mail { words: crate::rcu::Rcu::default(), heard: crate::memory::queue::unbounded(&crate::memory::owners::CHANNELS), paces: crate::rcu::Rcu::default() }
    }
}

impl std::fmt::Debug for Mail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Mail").field("runners", &self.words.load().len()).field("heard", &self.heard.1.len()).finish()
    }
}

/// What [`Mail::take`] took: the jobs heard of, in order, and each runner's newest word.
#[derive(Debug, Default)]
pub(crate) struct Taken {
    heard: Vec<String>,
    words: HashMap<String, FastWord>,
}

impl Taken {
    pub(crate) fn is_empty(&self) -> bool {
        self.heard.is_empty() && self.words.is_empty()
    }
}

impl Mail {
    /// Runner `runner`'s newest word, in place of what it said before since the last look. Its
    /// measured pace when that changed, for the CI engine's load balancer (docs/lb.md "The CI
    /// engine"): the same runner's speed, whichever kernel ranks it.
    pub(crate) fn word(&self, runner: &str, w: &FastWord) -> Option<kiln_ci::sched::balance::Pace> {
        let pace = kiln_ci::sched::balance::Pace { speed: w.speed, slowness: w.slowness, measured: w.measured, disk: w.disk, net: w.net };
        let changed = if self.paces.load().get(runner) == Some(&pace) {
            None
        } else {
            self.paces.update(|m| match m.insert(runner.to_owned(), pace) {
                Some(p) if p == pace => None,
                Some(_) => Some(pace),
                None => (pace != kiln_ci::sched::balance::Pace::default()).then_some(pace),
            })
        };
        let cell = self.words.load().get(runner).cloned();
        let cell = match cell {
            Some(c) => c,
            None => self.words.update(|m| Arc::clone(m.entry(runner.to_owned()).or_insert_with(|| Arc::new(arc_swap::ArcSwapOption::empty())))),
        };
        cell.store(Some(Arc::new(*w)));
        changed
    }

    /// Job `job` was heard of at its runner (started, refused, or left it).
    pub(crate) fn heard(&self, job: &str) {
        // Unbounded, and its receiver is this mail's own: the send always goes.
        let _ = self.heard.0.try_send(job.to_owned());
    }

    /// What was said since the last look, all of it, leaving the paces. The words first, then the jobs
    /// heard: a connection says a job's hearing before a word of its runner's that counts it, so every
    /// hearing before a word taken here is taken with it.
    pub(crate) fn take(&self) -> Taken {
        let words = self.words.load().iter().filter_map(|(r, c)| Some((r.clone(), *c.swap(None)?))).collect();
        let mut heard = Vec::new();
        while let Ok(job) = self.heard.1.try_recv() {
            heard.push(job);
        }
        Taken { heard, words }
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

    /// What runners' connections said between two of the scheduling loop's looks ([`Mail`]), taken
    /// in: the jobs heard of at their runners first, then each runner's newest word, which leaves the
    /// kernel as each said in order would.
    pub(crate) fn apply(&mut self, mail: Taken) {
        for job in &mail.heard {
            self.heard(job);
        }
        for (runner, w) in &mail.words {
            self.hint(runner, w);
        }
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

    /// A run's legs that arrive together are placed by the integer auction (`[reach] auction`,
    /// docs/lb.md "The auction"), or one at a time (off, the default).
    pub(crate) fn set_auction(&mut self, on: bool) {
        self.auction = on;
        self.lb.set_auction(on);
    }

    /// The kernel's picks for `times.len()` legs of kind `key` needing `need` that arrived
    /// together (a run's ordinary legs in one look at the queue), each with its expected run time
    /// at the reference core where history knows one (ms). With the auction declared, and their
    /// times and the members' rates telling them apart (otherwise every assignment costs the same,
    /// and one at a time spreads them by room, W05-R23), as many as the members' views hold go
    /// where Bertsekas's auction puts them, at the least total expected finish
    /// (`lb::auction::finish_at`: a leg's time over a member's key multiplier, its measured speed
    /// over its slowness as the kernel keeps it), one each, in the legs' order; the rest `None`,
    /// for the engine's own pick. Each pick's room is taken from the kernel's view at once (`sent`
    /// once sent, `unsent` if not). `None` in all: not decided together, each is picked as before.
    pub(crate) fn pick_batch<A>(&mut self, key: u64, need: lb::Amounts, times: &[Option<u64>], agents: &HashMap<String, Arc<A>>, name: impl Fn(&A) -> &str, member: impl Fn(&A) -> bool) -> Option<Vec<Option<Pick>>> {
        let n = times.len();
        if !self.auction || n < 2 || self.ids.is_empty() || !self.budgets.iter().any(Option::is_some) {
            return None;
        }
        let sig = self.sync(key, agents, name, member);
        if !self.tells_apart(key, need, times) {
            return None;
        }
        let rates: Vec<u32> = (0..u32::try_from(self.names.len()).unwrap_or(u32::MAX)).map(|r| self.lb.multiplier(lb::RunnerId(r))).collect();
        let cost = |i: usize, r: lb::RunnerId| Some(lb::auction::finish_at(times.get(i).copied().flatten().unwrap_or(0), rates.get(r.0 as usize).copied().unwrap_or(lb::SCALE), 0));
        let (mut starts, mut waits) = (Vec::new(), Vec::new());
        self.out.clear();
        // Refused (the kernel's runs full): what it started before is placed, the rest picked by the engine.
        let _ = self.lb.ask_batch(sig, need, n, cost, &mut starts, &mut waits, &mut self.out);
        self.out.clear();
        // Waiting is the engine's (its queue), never the kernel's.
        for h in waits {
            let _ = self.lb.cancel(h, &mut self.out);
        }
        self.out.clear();
        // Its starts are the legs' first, in their order.
        let mut picks: Vec<Option<Pick>> = Vec::with_capacity(n);
        for (r, st) in starts {
            match self.names.get(r.0 as usize) {
                Some(nm) => picks.push(Some((nm.clone(), st.run))),
                None => {
                    let _ = self.lb.unsent(st.run);
                    picks.push(None);
                }
            }
        }
        picks.resize(n, None);
        Some(picks)
    }

    /// Whether the auction's costs tell legs of kind `key` needing `need` apart: their `times`
    /// differ (none known is a time of its own), and so do the rates, at the kernel's resolution
    /// (each multiplier's class), of the kind's members whose view holds `need` now.
    fn tells_apart(&self, key: u64, need: lb::Amounts, times: &[Option<u64>]) -> bool {
        let mut t = times.iter();
        let first = t.next();
        if !t.any(|x| Some(x) != first) {
            return false;
        }
        let Some(s) = self.sigs.get(&key) else { return false };
        let fits = |r: u32| self.lb.view(lb::RunnerId(r)).is_some_and(|v| v.iter().zip(&need).all(|(v, n)| n <= v));
        let mut rates = s.seen.iter().filter(|(r, (_, m))| *m && fits(**r)).map(|(r, _)| lb::class_of(u64::from(self.lb.multiplier(lb::RunnerId(*r)))));
        let one = rates.next();
        rates.any(|x| Some(x) != one)
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

    /// LB08, W05-R26: a run's legs that arrive together go where the auction puts them once it's
    /// declared: of two runners of 2 CPUs, one measured twice as fast, the long legs take the fast
    /// one. Off, or with no times to tell the legs apart, they're not decided together.
    #[test]
    fn lb08_w05_r26_a_runs_legs_go_where_the_auction_puts_them() -> anyhow::Result<()> {
        let agents = fleet(&[("fast", true), ("slow", true)]);
        let key = Legs::kind(&[], &[], "");
        let need = [1000, 0, 0, 0];
        let times = [Some(10_000), Some(100_000), Some(10_000), Some(100_000)];
        let words = |legs: &mut Legs| {
            legs.hint("fast", &FastWord { speed: 2048, measured: 1, ..word(2) });
            legs.hint("slow", &FastWord { speed: 1024, measured: 1, ..word(2) });
        };
        let mut off = Legs::default();
        words(&mut off);
        anyhow::ensure!(off.pick_batch(key, need, &times, &agents, |r| &r.name, |r| r.ok).is_none(), "decided together though the auction is off");
        let mut on = Legs::default();
        on.set_auction(true);
        words(&mut on);
        anyhow::ensure!(on.pick_batch(key, need, &[None; 4], &agents, |r| &r.name, |r| r.ok).is_none(), "decided together with no times to tell the legs apart");
        let picks = on.pick_batch(key, need, &times, &agents, |r| &r.name, |r| r.ok).ok_or_else(|| anyhow::anyhow!("not decided together"))?;
        let at: Vec<Option<&str>> = picks.iter().map(|p| p.as_ref().map(|(n, _)| n.as_str())).collect();
        anyhow::ensure!(at == [Some("slow"), Some("fast"), Some("slow"), Some("fast")], "the legs went to {at:?}");
        for (_, run) in picks.into_iter().flatten() {
            on.unsent(run);
        }
        Ok(())
    }

    /// docs/lb.md "At scale": words come by mail, and taking it in leaves the kernel as hearing each
    /// as it came would: a runner's older word gives way to its newest, and a job heard of between
    /// gives back the room the kernel took for it. A pace is passed on only when it changes.
    #[test]
    fn mail_leaves_the_kernel_as_each_word_in_order_would() -> anyhow::Result<()> {
        let agents = fleet(&[("a", true), ("b", true)]);
        let key = Legs::kind(&[], &[], "");
        let mut direct = Legs::default();
        let mut mailed = Legs::default();
        let mail = Mail::default();
        for legs in [&mut direct, &mut mailed] {
            legs.hint("a", &word(8));
            legs.hint("b", &word(4));
        }
        // A pick on each: a has the most room.
        let (p1, r1) = direct.pick(key, [4000, 0, 0, 0], &agents, |r| &r.name, |r| r.ok).ok_or_else(|| anyhow::anyhow!("no pick"))?;
        direct.sent("j1", r1);
        let (p2, r2) = mailed.pick(key, [4000, 0, 0, 0], &agents, |r| &r.name, |r| r.ok).ok_or_else(|| anyhow::anyhow!("no pick"))?;
        mailed.sent("j1", r2);
        anyhow::ensure!(p1 == "a" && p2 == "a", "{p1} {p2}");
        // a says 6, then j1 is heard of, then a says 4; b says 2. Directly, in order; by mail, taken at once.
        direct.hint("a", &word(6));
        direct.heard("j1");
        direct.hint("a", &word(4));
        direct.hint("b", &word(2));
        let _ = mail.word("a", &word(6));
        mail.heard("j1");
        let _ = mail.word("a", &word(4));
        let _ = mail.word("b", &word(2));
        mailed.apply(mail.take());
        anyhow::ensure!(mail.take().is_empty(), "mail left after it was taken");
        for legs in [&mut direct, &mut mailed] {
            let (p, run) = legs.pick(key, [3000, 0, 0, 0], &agents, |r| &r.name, |r| r.ok).ok_or_else(|| anyhow::anyhow!("no pick"))?;
            anyhow::ensure!(p == "a", "picked {p}");
            legs.unsent(run);
            anyhow::ensure!(legs.pick(key, [5000, 0, 0, 0], &agents, |r| &r.name, |r| r.ok).is_none(), "room past every runner's word");
        }
        // Paces: none said yet is the reference, not passed on; a change is; the same again isn't.
        let faster = FastWord { speed: 2048, measured: 1, ..word(4) };
        anyhow::ensure!(mail.word("c", &word(4)).is_none());
        anyhow::ensure!(mail.word("c", &faster).is_some_and(|p| p.speed == 2048));
        anyhow::ensure!(mail.word("c", &faster).is_none());
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
