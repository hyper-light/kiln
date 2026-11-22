//! Groups that split, merge and move themselves, in a deterministic simulator (CP-T1.9; AC-T1.9.3,
//! T-T1.9.S1; docs/design/reshard.md, "The simulator"). Clients write, delete and read keys through
//! a router whose directory may be stale; the planner asks for splits, merges and moves from each
//! group's measured size and load and each node's cores, as nodes come, go and change; the driver
//! carries them out through groups that keep their rows, their log and their holding; and a
//! nemesis restarts the driver (between any two of its steps, with its stale actions still on their
//! way), restarts groups (losing what they were asked), and takes an old group's log away under a
//! catch-up. After every step: every position in exactly one group of the directory, no key
//! answered by two groups, and the group answering a key the directory's and holding every write
//! acknowledged. At every quiet point: every key's row in exactly one group, the directory's, with
//! the last write acknowledged for it (writes lost: 0).
//!
//! `AMPHORA_RESHARD_OPS` (default 300) sets the operations a seed runs to, `AMPHORA_RESHARD_SEEDS`
//! (default 3) the seeds.

#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects,
        clippy::disallowed_macros,
        clippy::cast_possible_truncation
    )
)]

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use amphora::reshard::directory::{Directory, Kind, Proposal};
use amphora::reshard::driver::{Action, Answer, Do, Driver, Failure, Out, Reply};
use amphora::reshard::holding::{Fenced, Holding, replays};
use amphora::reshard::plan::{GroupLoad, NodeRoom, cut, plan};
use amphora::reshard::{GroupId, NodeId, OpId, Pos, Seq, Span, position};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next() % n }
    }
    fn one_in(&mut self, n: u64) -> bool {
        self.below(n) == 0
    }
}

#[derive(Clone, Copy)]
struct Row {
    val: u64,
    version: Seq,
}

#[derive(Clone, Copy)]
enum Entry {
    Put { pos: Pos, key: u32, val: u64 },
    Del { pos: Pos, key: u32 },
    Fence,
}

#[derive(Default)]
struct Group {
    holding: Holding,
    rows: BTreeMap<(Pos, u32), Row>,
    log: Vec<Entry>,
}

impl Group {
    fn head(&self) -> Seq {
        self.log.len() as Seq
    }
    fn rows_in(&self, span: Span) -> impl Iterator<Item = (&(Pos, u32), &Row)> {
        self.rows.range((span.lo(), 0)..=(span.hi(), u32::MAX))
    }
}

/// The test's parameters (not the product's): how many keys, how big a batch the shell moves.
const KEYS: u32 = 512;
const BATCH: u64 = 6;

struct Sim {
    rng: Rng,
    keys: Vec<(Pos, u32)>,
    groups: BTreeMap<GroupId, Group>,
    retired: BTreeSet<GroupId>,
    queues: BTreeMap<GroupId, VecDeque<Action>>,
    inbox: Vec<Answer>,
    durable: Directory,
    driver: Driver,
    epoch: u64,
    router: Directory,
    oracle: BTreeMap<u32, u64>,
    nodes: BTreeMap<NodeId, u64>,
    window: BTreeMap<GroupId, BTreeMap<Pos, u64>>,
    window_writes: u64,
    log_gone: BTreeSet<OpId>,
    next_val: u64,
    hot: usize,
    deletes: u64,
    // What it counted.
    ended: u64,
    flipped: BTreeMap<&'static str, u64>,
    aborted: u64,
    fence_lags: Vec<u64>,
    refused_fenced: u64,
    restarts: u64,
    acked: u64,
}

impl Sim {
    fn new(seed: u64) -> Self {
        let mut keys: Vec<(Pos, u32)> =
            (0..KEYS).map(|k| (position(&format!("k{k}")), k)).collect();
        keys.sort_unstable();
        let durable = Directory::new(NodeId(1));
        let (driver, out) = Driver::recover(durable.clone(), 0).unwrap();
        assert!(out.is_empty());
        let mut groups = BTreeMap::new();
        groups.insert(
            GroupId(0),
            Group {
                holding: Holding::owning(Some(Span::ALL)),
                ..Group::default()
            },
        );
        Self {
            rng: Rng(seed),
            keys,
            groups,
            retired: BTreeSet::new(),
            queues: BTreeMap::new(),
            inbox: Vec::new(),
            router: durable.clone(),
            durable,
            driver,
            epoch: 0,
            oracle: BTreeMap::new(),
            nodes: BTreeMap::from([(NodeId(1), 2)]),
            window: BTreeMap::new(),
            window_writes: 0,
            log_gone: BTreeSet::new(),
            next_val: 1,
            hot: 0,
            deletes: 10,
            ended: 0,
            flipped: BTreeMap::new(),
            aborted: 0,
            fence_lags: Vec::new(),
            refused_fenced: 0,
            restarts: 0,
            acked: 0,
        }
    }

    /// Carry out `out` in order; the driver may crash between any two items (`nemesis`).
    fn carry(&mut self, out: Vec<Out>, nemesis: bool) {
        let mut pending: VecDeque<Out> = out.into();
        while let Some(o) = pending.pop_front() {
            if nemesis && self.rng.one_in(60) {
                pending = self.crash_driver().into();
                continue;
            }
            match o {
                Out::Commit(c) => self.durable.apply(&c).unwrap(),
                Out::Send(a) => self.queues.entry(a.what.group()).or_default().push_back(a),
                Out::Ended(e) => {
                    self.ended += 1;
                    if e.done {
                        let k = match e.kind {
                            Kind::Split => "split",
                            Kind::Merge => "merge",
                            Kind::Move => "move",
                        };
                        *self.flipped.entry(k).or_default() += 1;
                        if let Some(l) = e.fence_lag {
                            self.fence_lags.push(l);
                        }
                    } else {
                        self.aborted += 1;
                    }
                }
            }
        }
    }

    /// The driver restarts from the directory as it is durable; answers on their way to it are
    /// lost, actions on their way to groups are not.
    fn crash_driver(&mut self) -> Vec<Out> {
        self.restarts += 1;
        self.epoch += 1;
        self.inbox.clear();
        let (driver, out) = Driver::recover(self.durable.clone(), self.epoch).unwrap();
        self.driver = driver;
        out
    }

    /// A group restarts: what it was asked and hadn't done is lost.
    fn crash_group(&mut self) {
        let ids: Vec<GroupId> = self.groups.keys().copied().collect();
        let g = ids[self.rng.below(ids.len() as u64) as usize];
        self.queues.remove(&g);
        let out = self.driver.restarted(g).unwrap();
        self.carry(out, true);
    }

    fn pick_key(&mut self) -> (Pos, u32) {
        // Most writes go to a hot run of keys (by position) that moves now and then.
        let n = self.keys.len();
        let i = if self.rng.below(10) < 8 {
            (self.hot + self.rng.below((n / 16) as u64) as usize) % n
        } else {
            self.rng.below(n as u64) as usize
        };
        self.keys[i]
    }

    fn write(&mut self) {
        let (pos, key) = self.pick_key();
        let Some(g) = self.router.owner(pos) else {
            panic!("the router's directory has a gap")
        };
        let Some(group) = self.groups.get_mut(&g) else {
            self.router = self.durable.clone();
            return;
        };
        match group.holding.answers(pos) {
            Ok(()) => {}
            Err(amphora::reshard::holding::Away::Fenced(_)) => {
                self.refused_fenced += 1;
                return;
            }
            Err(amphora::reshard::holding::Away::Elsewhere) => {
                self.router = self.durable.clone();
                return;
            }
        }
        assert_eq!(
            self.durable.owner(pos),
            Some(g),
            "a group took a write for a key it doesn't own"
        );
        let seq = group.head() + 1;
        if self.rng.below(100) < self.deletes {
            group.log.push(Entry::Del { pos, key });
            group.rows.remove(&(pos, key));
            self.oracle.remove(&key);
        } else {
            let val = self.next_val;
            self.next_val += 1;
            group.log.push(Entry::Put { pos, key, val });
            group.rows.insert((pos, key), Row { val, version: seq });
            self.oracle.insert(key, val);
        }
        self.acked += 1;
        *self.window.entry(g).or_default().entry(pos).or_default() += 1;
        self.window_writes += 1;
    }

    fn read(&mut self) {
        let (pos, key) = self.pick_key();
        let g = self.router.owner(pos).unwrap();
        let Some(group) = self.groups.get(&g) else {
            return;
        };
        if group.holding.answers(pos).is_err() {
            return;
        }
        let got = group.rows.get(&(pos, key)).map(|r| r.val);
        assert_eq!(
            got,
            self.oracle.get(&key).copied(),
            "read of k{key} at group {g:?}"
        );
    }

    /// The shell: `action` carried out at its group.
    fn execute(&mut self, a: Action) -> Reply {
        let op = a.op;
        let failed = |e| Reply::Failed(Failure::Refused(e));
        match a.what {
            Do::Create { group, .. } => {
                if self.retired.contains(&group) {
                    return failed(amphora::reshard::Refused::Ended(op));
                }
                self.groups.entry(group).or_default();
                Reply::Created
            }
            Do::Copy { from, to, span } => {
                let Some(src) = self.groups.get(&from) else {
                    return Reply::Failed(Failure::NoGroup);
                };
                let at = src.head();
                let Some(dst) = self.groups.get(&to) else {
                    return Reply::Failed(Failure::NoGroup);
                };
                let fresh = dst.holding.incoming().is_none_or(|i| i.op != op);
                let mut holding = dst.holding;
                let progress = match holding.receive(op, span) {
                    Ok(p) => p,
                    Err(e) => return failed(e),
                };
                if fresh {
                    assert_eq!(
                        dst.rows_in(span).count(),
                        0,
                        "a span taken in over rows left behind"
                    );
                }
                // The next batch: every row at up to BATCH positions past the cursor.
                let start = match progress.copied {
                    Some(p) if p >= span.hi() => None,
                    Some(p) => Some(p + 1),
                    None => Some(span.lo()),
                };
                let mut batch: Vec<((Pos, u32), Row)> = Vec::new();
                let mut positions = 0;
                let mut last = None;
                let mut done = true;
                if let Some(start) = start {
                    for (k, r) in src.rows_in(Span::new(start, span.hi()).unwrap()) {
                        if last != Some(k.0) {
                            if positions == BATCH {
                                done = false;
                                break;
                            }
                            positions += 1;
                            last = Some(k.0);
                        }
                        batch.push((
                            *k,
                            Row {
                                val: r.val,
                                version: at,
                            },
                        ));
                    }
                }
                let progress = holding.copied(op, last, done, at).unwrap();
                let dst = self.groups.get_mut(&to).unwrap();
                dst.holding = holding;
                dst.rows.extend(batch);
                let head = self.groups[&from].head();
                Reply::Copied { progress, head }
            }
            Do::Replay {
                from,
                to,
                span,
                upto,
            } => {
                if self.log_gone.contains(&op) {
                    return Reply::Failed(Failure::LogGone);
                }
                let Some(src) = self.groups.get(&from) else {
                    return Reply::Failed(Failure::NoGroup);
                };
                let head = src.head();
                let Some(dst) = self.groups.get(&to) else {
                    return Reply::Failed(Failure::NoGroup);
                };
                let Some(incoming) = dst.holding.incoming().filter(|i| i.op == op) else {
                    return failed(amphora::reshard::Refused::Ended(op));
                };
                let from_seq = incoming.progress.replayed;
                let through = upto.min(head).min(from_seq + BATCH).max(from_seq);
                let entries: Vec<(Seq, Entry)> = ((from_seq + 1)..=through)
                    .map(|s| (s, src.log[(s - 1) as usize]))
                    .collect();
                let dst = self.groups.get_mut(&to).unwrap();
                for (seq, e) in entries {
                    match e {
                        Entry::Put { pos, key, val } if span.contains(pos) => {
                            let stored = dst.rows.get(&(pos, key)).map(|r| r.version);
                            if replays(stored, seq) {
                                dst.rows.insert((pos, key), Row { val, version: seq });
                            }
                        }
                        Entry::Del { pos, key } if span.contains(pos) => {
                            let stored = dst.rows.get(&(pos, key)).map(|r| r.version);
                            if stored.is_some() && replays(stored, seq) {
                                dst.rows.remove(&(pos, key));
                            }
                        }
                        _ => {}
                    }
                }
                match dst.holding.replayed(op, through) {
                    Ok(progress) => Reply::Replayed { progress, head },
                    Err(e) => failed(e),
                }
            }
            Do::Fence { group, span } => {
                let Some(g) = self.groups.get_mut(&group) else {
                    return Reply::Failed(Failure::NoGroup);
                };
                let next = g.head() + 1;
                match g.holding.fence(op, span, next) {
                    Ok(Fenced::New(at)) => {
                        g.log.push(Entry::Fence);
                        Reply::Fenced { at }
                    }
                    Ok(Fenced::Already(at)) => Reply::Fenced { at },
                    Err(e) => failed(e),
                }
            }
            Do::Activate { group } => {
                match self.groups.get_mut(&group).map(|g| g.holding.activate(op)) {
                    Some(Ok(())) => Reply::Activated,
                    Some(Err(e)) => failed(e),
                    None => Reply::Failed(Failure::NoGroup),
                }
            }
            Do::Release { group, span } | Do::Discard { group, span } => {
                let release = matches!(a.what, Do::Release { .. });
                let Some(g) = self.groups.get_mut(&group) else {
                    // A retired group held nothing when it was retired: a release asked of it
                    // again (the driver restarted between the retire and the operation's end) is
                    // done. A group never made has nothing to discard.
                    return match (release, self.retired.contains(&group)) {
                        (true, true) => Reply::Released { done: true },
                        (true, false) => Reply::Failed(Failure::NoGroup),
                        (false, _) => Reply::Discarded { done: true },
                    };
                };
                let r = if release {
                    g.holding.release(op)
                } else {
                    g.holding.discard(op)
                };
                if let Err(e) = r {
                    return failed(e);
                }
                let holding = g.holding;
                let garbage: Vec<(Pos, u32)> = g
                    .rows_in(span)
                    .map(|(k, _)| *k)
                    .filter(|k| holding.garbage(k.0))
                    .collect();
                let done = garbage.len() as u64 <= BATCH;
                for k in garbage.into_iter().take(BATCH as usize) {
                    g.rows.remove(&k);
                }
                if release {
                    Reply::Released { done }
                } else {
                    Reply::Discarded { done }
                }
            }
            Do::Unfence { group } => {
                match self.groups.get_mut(&group).map(|g| g.holding.unfence(op)) {
                    Some(Ok(())) => Reply::Unfenced,
                    Some(Err(e)) => failed(e),
                    None => Reply::Unfenced,
                }
            }
            Do::Retire { group } => {
                if let Some(g) = self.groups.get(&group) {
                    assert!(
                        g.holding.owned().is_none()
                            && g.holding.incoming().is_none()
                            && g.holding.fenced().is_none()
                    );
                    assert!(g.rows.is_empty(), "a group retired with rows");
                    self.groups.remove(&group);
                }
                // Its queue stays: a retired group still answers what was asked of it (above), and
                // a driver that restarted after asking this retire may have queued its own asks
                // behind it (the release again). Dropping them would leave that driver waiting
                // on an answer that never comes.
                self.retired.insert(group);
                Reply::Retired
            }
        }
    }

    /// One action carried out, at a group picked at random.
    fn deliver_action(&mut self) -> bool {
        let busy: Vec<GroupId> = self
            .queues
            .iter()
            .filter(|(_, q)| !q.is_empty())
            .map(|(g, _)| *g)
            .collect();
        if busy.is_empty() {
            return false;
        }
        let g = busy[self.rng.below(busy.len() as u64) as usize];
        self.deliver_at(g).is_some()
    }

    /// The next action queued at `g` carried out, if any; its answer goes to the inbox.
    fn deliver_at(&mut self, g: GroupId) -> Option<Do> {
        let a = self.queues.get_mut(&g)?.pop_front()?;
        let reply = self.execute(a);
        self.inbox.push(Answer {
            ask: a.ask,
            op: a.op,
            reply,
        });
        Some(a.what)
    }

    /// One answer taken by the driver, picked at random.
    fn deliver_answer(&mut self, nemesis: bool) -> bool {
        if self.inbox.is_empty() {
            return false;
        }
        let i = self.rng.below(self.inbox.len() as u64) as usize;
        let a = self.inbox.swap_remove(i);
        let out = self.driver.answer(a).unwrap();
        self.carry(out, nemesis);
        true
    }

    /// The planner on what was measured this window, and its proposals begun.
    fn plan(&mut self) {
        let dir = self.driver.directory().clone();
        let mut loads = Vec::new();
        for (g, _) in dir.groups() {
            // A group made by an operation whose Create hasn't come yet holds nothing.
            let rows: Vec<(Pos, u64)> = match self
                .groups
                .get(&g)
                .and_then(|group| group.holding.owned().map(|s| (group, s)))
            {
                Some((group, s)) => group.rows_in(s).map(|(k, _)| (k.0, 1)).collect(),
                None => Vec::new(),
            };
            let w: Vec<(Pos, u64)> = self
                .window
                .get(&g)
                .map(|m| m.iter().map(|(p, c)| (*p, *c)).collect())
                .unwrap_or_default();
            loads.push(GroupLoad {
                group: g,
                bytes: rows.len() as u64,
                load: w.iter().map(|(_, c)| c).sum(),
                by_size: cut(&rows),
                by_load: cut(&w),
            });
        }
        let nodes: Vec<NodeRoom> = self
            .nodes
            .iter()
            .map(|(n, c)| NodeRoom {
                node: *n,
                cores: *c,
            })
            .collect();
        for p in plan(&dir, &nodes, &loads) {
            if let Ok(out) = self.driver.begin(&p) {
                self.carry(out, true);
            }
        }
        self.window.clear();
        self.window_writes = 0;
    }

    /// After every step: the directory covers every position once; no key is answered by two
    /// groups; the group answering a key is the directory's and holds its last acknowledged write.
    fn check(&self, every_key: bool) {
        self.durable.check().unwrap();
        assert_eq!(
            self.driver.directory(),
            &self.durable,
            "the driver's directory isn't the durable one"
        );
        let n = if every_key { self.keys.len() } else { 8 };
        for i in 0..n {
            let (pos, key) = self.keys[(i * 61 + self.ended as usize) % self.keys.len()];
            let owner = self.durable.owner(pos).unwrap();
            let answering: Vec<GroupId> = self
                .groups
                .iter()
                .filter(|(_, g)| g.holding.answers(pos).is_ok())
                .map(|(id, _)| *id)
                .collect();
            assert!(answering.len() <= 1, "k{key} answered by {answering:?}");
            if let Some(g) = answering.first() {
                assert_eq!(
                    *g, owner,
                    "k{key} answered by a group the directory doesn't name"
                );
                let got = self.groups[g].rows.get(&(pos, key)).map(|r| r.val);
                assert_eq!(got, self.oracle.get(&key).copied(), "k{key} at its group");
            }
            let holders = self
                .groups
                .values()
                .filter(|g| g.holding.owned().is_some_and(|s| s.contains(pos)))
                .count();
            assert!(holders <= 2, "k{key} held by {holders} groups");
        }
    }

    /// Run everything under way to its end, with no new work and no nemesis; then every key's row
    /// is in exactly one group, the directory's, with its last acknowledged write.
    fn quiesce(&mut self) -> u64 {
        let mut steps = 0u64;
        while !self.driver.idle()
            || !self.inbox.is_empty()
            || self.queues.values().any(|q| !q.is_empty())
        {
            steps += 1;
            assert!(
                steps < 10_000_000,
                "quiescing didn't end: {:?}\n{:?}",
                self.driver,
                self.groups
                    .iter()
                    .map(|(id, g)| (*id, g.holding))
                    .collect::<Vec<_>>()
            );
            if self.rng.one_in(2) {
                if !self.deliver_action() {
                    self.deliver_answer(false);
                }
            } else if !self.deliver_answer(false) {
                self.deliver_action();
            }
            self.check(false);
        }
        assert_eq!(self.durable.ops().count(), 0);
        let placed: BTreeSet<GroupId> = self.durable.groups().map(|(g, _)| g).collect();
        let live: BTreeSet<GroupId> = self.groups.keys().copied().collect();
        assert_eq!(
            placed, live,
            "groups the directory doesn't know, or groups lost"
        );
        let mut lost = 0u64;
        for (id, g) in &self.groups {
            assert!(g.holding.fenced().is_none() && g.holding.incoming().is_none());
            assert_eq!(
                g.holding.owned(),
                self.durable.span_of(*id),
                "group {id:?}'s holding"
            );
            for (pos, _) in g.rows.keys() {
                assert!(
                    g.holding.owned().is_some_and(|s| s.contains(*pos)),
                    "a row left outside its group's span"
                );
            }
        }
        for (pos, key) in &self.keys {
            let holders: Vec<GroupId> = self
                .groups
                .iter()
                .filter(|(_, g)| g.rows.contains_key(&(*pos, *key)))
                .map(|(id, _)| *id)
                .collect();
            let owner = self.durable.owner(*pos).unwrap();
            match self.oracle.get(key) {
                Some(v) => {
                    assert!(holders.len() <= 1, "k{key} in {holders:?}");
                    if self.groups[&owner].rows.get(&(*pos, *key)).map(|r| r.val) != Some(*v) {
                        lost += 1;
                    }
                }
                None => assert!(holders.is_empty(), "k{key}, deleted, in {holders:?}"),
            }
        }
        lost
    }

    /// The cluster changes now and then: a node joins, one is removed (no cores), one comes back,
    /// cores change; the hot keys move; the mix leans to inserts or to deletes.
    fn weather(&mut self) {
        self.hot = self.rng.below(self.keys.len() as u64) as usize;
        self.deletes = [5, 10, 45][self.rng.below(3) as usize];
        match self.rng.below(4) {
            0 if self.nodes.len() < 5 => {
                let id = NodeId(self.nodes.keys().last().unwrap().0 + 1);
                self.nodes.insert(id, 1 + self.rng.below(4));
            }
            1 if self.nodes.values().filter(|c| **c > 0).count() > 1 => {
                let live: Vec<NodeId> = self
                    .nodes
                    .iter()
                    .filter(|(_, c)| **c > 0)
                    .map(|(n, _)| *n)
                    .collect();
                let n = live[self.rng.below(live.len() as u64) as usize];
                self.nodes.insert(n, 0);
            }
            _ => {
                let all: Vec<NodeId> = self.nodes.keys().copied().collect();
                let n = all[self.rng.below(all.len() as u64) as usize];
                self.nodes.insert(n, 1 + self.rng.below(4));
            }
        }
    }
}

fn run(seed: u64, target: u64) {
    let mut sim = Sim::new(seed);
    let mut step = 0u64;
    let mut next_quiet = 50;
    while sim.ended < target {
        step += 1;
        assert!(
            step < 50_000_000,
            "seed {seed}: {} operations ended in {step} steps",
            sim.ended
        );
        match sim.rng.below(100) {
            0..=44 => sim.write(),
            45..=54 => sim.read(),
            55..=74 => {
                sim.deliver_action();
            }
            75..=94 => {
                sim.deliver_answer(true);
            }
            95 if sim.rng.one_in(8) => sim.crash_group(),
            96 if sim.rng.one_in(8) => {
                let out = sim.crash_driver();
                sim.carry(out, true);
            }
            97 if sim.rng.one_in(30) => {
                let copying: Vec<OpId> = sim
                    .durable
                    .ops()
                    .filter(|o| o.phase == amphora::reshard::directory::Phase::Copying)
                    .map(|o| o.id)
                    .collect();
                if let Some(id) = copying.first() {
                    sim.log_gone.insert(*id);
                }
            }
            98 if sim.rng.one_in(50) => sim.weather(),
            _ => {}
        }
        if sim.window_writes >= 200 {
            sim.plan();
        }
        sim.check(step.is_multiple_of(64));
        if sim.ended >= next_quiet {
            next_quiet += 50;
            assert_eq!(sim.quiesce(), 0, "seed {seed}: writes lost");
        }
    }
    let lost = sim.quiesce();
    let mut lags = sim.fence_lags.clone();
    lags.sort_unstable();
    let p99 = lags
        .get(lags.len().saturating_sub(1) * 99 / 100)
        .copied()
        .unwrap_or(0);
    println!(
        "seed {seed}: {} operations ({:?} flipped, {} given up), {} writes acknowledged, {} lost, {} refused while fenced, {} driver restarts, {} groups, fence lag p99 {p99} max {} entries",
        sim.ended,
        sim.flipped,
        sim.aborted,
        sim.acked,
        lost,
        sim.refused_fenced,
        sim.restarts,
        sim.groups.len(),
        lags.last().copied().unwrap_or(0),
    );
    assert_eq!(lost, 0, "seed {seed}: writes lost");
    for k in ["split", "merge", "move"] {
        assert!(
            sim.flipped.get(k).copied().unwrap_or(0) > 0,
            "seed {seed}: no {k} done (non-vacuity)"
        );
    }
    assert!(
        sim.aborted > 0,
        "seed {seed}: no operation given up (non-vacuity)"
    );
}

fn env(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// T-T1.9.S1 (AC-T1.9.3): automatic splits, merges and moves under writes, restarts and lost logs
/// leave every key in exactly one group and lose no acknowledged write.
#[test]
fn t_t1_9_s1_every_key_in_exactly_one_group_through_automatic_operations() {
    let ops = env("AMPHORA_RESHARD_OPS", 300);
    let seeds = env("AMPHORA_RESHARD_SEEDS", 3);
    let first = env("AMPHORA_RESHARD_SEED", 1);
    for seed in first..first + seeds {
        run(seed, ops);
    }
}

/// T-T1.9.S1, the hang it found: the driver restarts after asking the old group's retire, and asks
/// the release again; the old retire is carried out first, with the new release queued behind it.
/// The retired group must still answer that release (done), so the operation ends.
#[test]
fn t_t1_9_s1_a_release_queued_behind_a_retire_is_still_answered() {
    let mut sim = Sim::new(7);
    for _ in 0..64 {
        sim.write();
    }
    let out = sim
        .driver
        .begin(&Proposal::Move {
            group: GroupId(0),
            to: NodeId(2),
        })
        .unwrap();
    sim.carry(out, false);
    let retiring = |sim: &Sim| {
        sim.queues
            .get(&GroupId(0))
            .is_some_and(|q| q.iter().any(|a| matches!(a.what, Do::Retire { .. })))
    };
    while !retiring(&sim) {
        assert!(
            sim.deliver_action() || sim.deliver_answer(false),
            "the move stalled"
        );
        sim.check(true);
    }
    // The driver restarts before the retire is carried out; past the flip it asks the new group to
    // take the span up again, then the old one to release it, behind the old retire.
    let out = sim.crash_driver();
    sim.carry(out, false);
    let to = sim.driver.directory().owner(0).unwrap();
    assert!(matches!(sim.deliver_at(to), Some(Do::Activate { .. })));
    assert!(sim.deliver_answer(false));
    assert_eq!(
        sim.queues[&GroupId(0)].len(),
        2,
        "the old retire, then the release"
    );
    assert!(matches!(
        sim.deliver_at(GroupId(0)),
        Some(Do::Retire { .. })
    ));
    assert!(matches!(
        sim.deliver_at(GroupId(0)),
        Some(Do::Release { .. })
    ));
    assert_eq!(sim.quiesce(), 0);
    assert!(sim.driver.idle());
    assert_eq!(sim.ended, 1);
    assert_eq!(sim.flipped.get("move"), Some(&1));
}
