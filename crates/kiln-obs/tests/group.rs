//! The observe group end to end in one process: replicas of the store on real hyper-log files,
//! hyper-raft's messages carried between them as kiln's lanes carry them, crashes (a member's
//! replica and log dropped, its files left as they were), restarts from disk, partitions, members
//! that join late and take an image whose files are fetched before its snapshot is stepped.
//! Senders write as kiln's nodes do: one write in flight each, sent again to whoever leads until
//! it is answered. What every scenario proves: every member answers alike, as a store that applied
//! the committed log answers, and every write answered is applied once.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Wake, Waker};
use std::time::{Duration, Instant};

use hyper_block::file::DeviceFile;
use hyper_durable::{GroupStore, LogStore as _, Output, Replica, Unbounded};
use hyper_log::Log;
use hyper_raft::proto::{ConfChangeType, ConfState, Message, MessageType};
use kiln_obs::batch::{Builder, Kind, SeriesKey};
use kiln_obs::block::Block;
use kiln_obs::group;
use kiln_obs::hash::Keyed;
use kiln_obs::image::{FileKind, Image};
use kiln_obs::index::Selector;
use kiln_obs::logs::file::LogFile;
use kiln_obs::logs::record::Record;
use kiln_obs::machine::Machine;
use kiln_obs::manifest::{Point, Session};
use kiln_obs::store::{Answer, Store, Written};

type Member = Replica<GroupStore<DeviceFile>, Machine>;

/// A test's tick: the group's elections in ticks, at a test's pace.
const TICK: Duration = Duration::from_millis(5);
/// A write not answered by then is sent again (to whoever leads now).
const RESEND: Duration = Duration::from_millis(400);

struct Flag(AtomicBool);
impl Wake for Flag {
    fn wake(self: Arc<Self>) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// A member's process: its replica, then its log (dropped in that order, as a replica must be).
struct Live {
    replica: Member,
    _log: Log<DeviceFile>,
    out: Output<(u64, Answer)>,
    compacted: u64,
}

struct Node {
    dir: PathBuf,
    live: Option<Live>,
}

impl Node {
    fn store_dir(dir: &Path) -> PathBuf {
        dir.join("store")
    }
}

/// One sender: a node's process writing, one write in flight.
struct Sender {
    id: u64,
    next: u64,
    total: u64,
    pending: Option<(u64, Vec<u8>, Option<Instant>)>,
    /// The member it sent its write to: only that member's answer reaches it, as only the server's
    /// reaches a runner that wrote through it.
    to: Option<u64>,
    acked: BTreeSet<u64>,
}

struct Cluster {
    _root: tempfile::TempDir,
    nodes: BTreeMap<u64, Node>,
    founding: ConfState,
    /// Links that carry nothing, each way named.
    cut: BTreeSet<(u64, u64)>,
    flag: Arc<Flag>,
    waker: Waker,
    start: Instant,
    ticked: Instant,
    /// What each member answered, by index: (sender, seq, outcome applied), the first answer
    /// each member gave at an index.
    answers: BTreeMap<u64, BTreeMap<u64, Answer>>,
    /// Every write any sender made, by sender and place.
    writes: HashMap<(u64, u64), Vec<u8>>,
    senders: Vec<Sender>,
    snapshots_installed: u64,
    /// Writes sent again, and those a session found applied already.
    resent: u64,
    duplicates: u64,
    /// Every member that led, and in which term.
    leaders: BTreeSet<(u64, u64)>,
    /// Armed: the leader's process ends the moment a follower says it holds the leader's
    /// entries, before the leader hears it — its writes are on the followers, unanswered.
    crash_on_append: bool,
    crashed: Vec<u64>,
}

fn batch(sender: u64, seq: u64) -> Vec<u8> {
    // Disjoint series a sender, times rising with its order: the committed order of senders does
    // not change what is held, and the reference need not know it.
    let mut b = Builder::new();
    let t0 = seq as i64 * 1_000;
    for m in 0..3 {
        for h in 0..4 {
            let host = format!("s{sender:x}-h{h}");
            let k = SeriesKey::new(&format!("m{m}"), [("host", host.as_str())], Kind::Gauge).unwrap();
            b.define(&k);
            b.run(k.fp, (0..5).map(|j| (t0 + j * 100, ((sender * 7 + seq * 13 + j as u64 * 3) % 1000) as f64 / 4.0)));
        }
    }
    let recs: Vec<Record> = (0..4)
        .map(|k| Record { ts: t0 * 1_000_000 + k, severity: [9, 13, 17][(seq % 3) as usize], service: format!("svc{sender:x}"), host: "h".into(), source: "test".into(), message: format!("write {seq} line {k}"), ..Record::default() })
        .collect();
    b.logs(&recs);
    b.finish_write(sender, seq, t0)
}

impl Cluster {
    fn new(voters: &[u64]) -> Self {
        let root = tempfile::tempdir().unwrap();
        let flag = Arc::new(Flag(AtomicBool::new(false)));
        let waker = Waker::from(flag.clone());
        let mut c = Self {
            nodes: BTreeMap::new(),
            founding: group::founding(voters),
            cut: BTreeSet::new(),
            flag,
            waker,
            start: Instant::now(),
            ticked: Instant::now(),
            answers: BTreeMap::new(),
            writes: HashMap::new(),
            senders: Vec::new(),
            snapshots_installed: 0,
            resent: 0,
            duplicates: 0,
            leaders: BTreeSet::new(),
            crash_on_append: false,
            crashed: Vec::new(),
            _root: root,
        };
        for &id in voters {
            c.add_node(id);
        }
        c
    }

    fn add_node(&mut self, id: u64) {
        let dir = self._root.path().join(format!("node-{id}"));
        self.nodes.insert(id, Node { dir, live: None });
        self.restart(id);
    }

    /// The member's process starts (again): its log and store reopen as they are on disk.
    fn restart(&mut self, id: u64) {
        let node = self.nodes.get_mut(&id).unwrap();
        assert!(node.live.is_none());
        let (log, store) = group::open_log(&node.dir.join("obs.log")).unwrap();
        let depth = store.depth();
        let machine = Machine::open(&Node::store_dir(&node.dir), Keyed::new(), &self.founding).unwrap();
        let replica = Replica::open(&group::settings(id, depth).unwrap(), store, machine, Unbounded).unwrap();
        let compacted = replica.applied().index;
        node.live = Some(Live { replica, _log: log, out: Output::default(), compacted });
    }

    /// The member's process ends at once: nothing more is written, what is on disk stays.
    fn crash(&mut self, id: u64) {
        self.nodes.get_mut(&id).unwrap().live = None;
    }

    fn now(&self) -> u64 {
        u64::try_from(self.start.elapsed().as_nanos()).unwrap()
    }

    fn leader(&self) -> Option<u64> {
        let leaders: Vec<(u64, u64)> = self.nodes.iter().filter_map(|(&id, n)| n.live.as_ref().filter(|l| l.replica.is_leader()).map(|l| (l.replica.term(), id))).collect();
        leaders.into_iter().max().map(|(_, id)| id)
    }

    fn member(&mut self, id: u64) -> &mut Member {
        &mut self.nodes.get_mut(&id).unwrap().live.as_mut().unwrap().replica
    }

    /// One round: every live member driven, its messages carried (a snapshot's files fetched
    /// first), the clock ticked, senders' writes sent or sent again.
    fn round(&mut self) -> bool {
        self.flag.0.store(false, Ordering::SeqCst);
        let now = self.now();
        let mut messages = Vec::new();
        let ids: Vec<u64> = self.nodes.keys().copied().collect();
        for &id in &ids {
            let Some(live) = self.nodes.get_mut(&id).and_then(|n| n.live.as_mut()) else { continue };
            let mut answers = Vec::new();
            let _ = group::pump(&mut live.replica, now, &self.waker, &mut live.out, &mut messages, &mut answers).unwrap_or_else(|e| panic!("member {id}: {e:?} ({:?})", live.replica.machine().fault));
            let seen = self.answers.entry(id).or_default();
            if live.replica.is_leader() {
                self.leaders.insert((live.replica.term(), id));
            }
            for (index, a) in answers {
                if matches!(a, Answer::Write { outcome: Written::Duplicate, .. }) && !seen.contains_key(&index) {
                    self.duplicates += 1;
                }
                seen.entry(index).or_insert(a.clone());
                if let Answer::Write { sender, seq, .. } = a
                    && let Some(s) = self.senders.iter_mut().find(|s| s.id == sender)
                    && s.to == Some(id)
                    && s.pending.as_ref().is_some_and(|p| p.0 == seq)
                {
                    s.acked.insert(seq);
                    s.pending = None;
                }
            }
            if group::due(&live.replica) {
                let keep = group::keep(&live.replica, live.compacted);
                if live.replica.compact(keep, now, &self.waker).unwrap() {
                    live.compacted = live.replica.applied().index;
                }
            }
        }
        let moved = !messages.is_empty();
        for m in messages {
            self.deliver(m);
        }
        if self.ticked.elapsed() >= TICK {
            self.ticked = Instant::now();
            for n in self.nodes.values_mut() {
                if let Some(l) = n.live.as_mut() {
                    l.replica.tick().unwrap();
                }
            }
        }
        self.send_writes();
        moved
    }

    fn deliver(&mut self, m: Message) {
        let (from, to) = (m.from, m.to);
        if self.crash_on_append && m.msg_type == MessageType::MsgAppendResponse && !m.reject && self.leader() == Some(to) && self.nodes.get(&to).is_some_and(|n| n.live.is_some()) {
            self.crash_on_append = false;
            self.crash(to);
            self.crashed.push(to);
            return;
        }
        let up = self.nodes.get(&to).is_some_and(|n| n.live.is_some());
        if !up || self.cut.contains(&(from, to)) {
            if let Some(l) = self.nodes.get_mut(&from).and_then(|n| n.live.as_mut()) {
                let _ = l.replica.report_unreachable(to);
                if m.msg_type == MessageType::MsgSnapshot {
                    let _ = l.replica.report_snapshot(to, false);
                }
            }
            return;
        }
        if m.msg_type == MessageType::MsgSnapshot {
            // What kiln's driver does before stepping a snapshot: every file the image names,
            // fetched from the sender's snapshot of it into the member's staging folder and checked
            // whole there.
            let fetched = self.fetch_snapshot(&m);
            if let Some(l) = self.nodes.get_mut(&from).and_then(|n| n.live.as_mut()) {
                let _ = l.replica.report_snapshot(to, fetched);
            }
            if !fetched {
                return;
            }
            self.snapshots_installed += 1;
        }
        let _ = self.member(to).step(m);
    }

    fn fetch_snapshot(&mut self, m: &Message) -> bool {
        let Some(snap) = &m.snapshot else { return false };
        let Some(meta) = &snap.metadata else { return false };
        let Ok(image) = Image::decode(&snap.data) else { return false };
        let Some(source) = self.nodes.get(&m.from).and_then(|n| n.live.as_ref()) else { return false };
        let from_dir = source.replica.machine().snapshot_dir(Point { index: meta.index, term: meta.term });
        let Some(target) = self.nodes.get(&m.to).and_then(|n| n.live.as_ref()) else { return false };
        let staging = target.replica.machine().staging().to_owned();
        let _ = std::fs::remove_dir_all(&staging);
        std::fs::create_dir_all(&staging).unwrap();
        for f in &image.files {
            let to = staging.join(&f.name);
            if std::fs::copy(from_dir.join(&f.name), &to).is_err() {
                return false;
            }
            let ok = match f.kind {
                FileKind::Block => Block::open(&to).is_ok_and(|b| b.bytes == f.bytes && b.crc == f.crc && b.verify().is_ok()),
                FileKind::Logs => LogFile::open(&to).is_ok_and(|l| l.bytes == f.bytes && l.crc == f.crc && l.verify().is_ok()),
            };
            if !ok {
                return false;
            }
        }
        true
    }

    fn send_writes(&mut self) {
        let Some(leader) = self.leader() else { return };
        for i in 0..self.senders.len() {
            let s = &mut self.senders[i];
            if s.pending.is_none() && s.next <= s.total {
                let e = batch(s.id, s.next);
                self.writes.insert((s.id, s.next), e.clone());
                s.pending = Some((s.next, e, None));
                s.next += 1;
            }
            let Some((_, entry, sent)) = &mut s.pending else { continue };
            if sent.is_some_and(|t| t.elapsed() < RESEND) {
                continue;
            }
            let entry = entry.clone();
            if sent.is_some() {
                self.resent += 1;
            }
            *sent = Some(Instant::now());
            s.to = Some(leader);
            let live = self.nodes.get_mut(&leader).and_then(|n| n.live.as_mut()).unwrap();
            let _ = live.replica.propose(Vec::new(), entry);
        }
    }

    fn add_senders(&mut self, n: u64, writes: u64) {
        for i in 0..n {
            self.senders.push(Sender { id: 0x5e00 + i, next: 1, total: writes, pending: None, to: None, acked: BTreeSet::new() });
        }
    }

    /// Rounds until `done` holds, or panics after `within`.
    fn until(&mut self, within: Duration, what: &str, done: impl Fn(&Self) -> bool) {
        let start = Instant::now();
        while start.elapsed() < within {
            if done(self) {
                return;
            }
            if !self.round() && !self.flag.0.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_micros(200));
            }
        }
        panic!("{what}: not within {within:?}");
    }

    /// Every live member compacts its log behind what it applied, keeping `keep` entries, once its
    /// writes are in.
    fn compact_all(&mut self, keep: u64) {
        let ids: Vec<u64> = self.nodes.iter().filter(|(_, n)| n.live.is_some()).map(|(&id, _)| id).collect();
        for id in ids {
            self.until(Duration::from_secs(10), "no write out", |c| c.nodes[&id].live.as_ref().is_none_or(|l| l.replica.in_flight() == 0));
            let (now, waker) = (self.now(), self.waker.clone());
            if let Some(l) = self.nodes.get_mut(&id).and_then(|n| n.live.as_mut())
                && l.replica.compact(keep, now, &waker).unwrap()
            {
                l.compacted = l.replica.applied().index;
            }
        }
    }

    fn acked(&self) -> u64 {
        self.senders.iter().map(|s| s.acked.len() as u64).sum()
    }

    fn all_written(&self) -> bool {
        self.senders.iter().all(|s| s.acked.len() as u64 == s.total)
    }

    /// Every live member applied all the leader committed, and none has a write out.
    fn settled(&self) -> bool {
        let Some(leader) = self.leader() else { return false };
        let commit = self.nodes[&leader].live.as_ref().unwrap().replica.applied().index;
        self.nodes.values().filter_map(|n| n.live.as_ref()).all(|l| l.replica.applied().index == commit && l.replica.in_flight() == 0)
    }

    fn store(&self, id: u64) -> std::sync::RwLockReadGuard<'_, Store> {
        self.nodes[&id].live.as_ref().unwrap().replica.machine().read()
    }

    /// What every live member answers, alike and as a store that applied the committed log does;
    /// every write answered applied once.
    fn verify(&self) {
        // The committed log, as the members applied it: one entry an index, the same on all.
        let mut log: BTreeMap<u64, Option<(u64, u64)>> = BTreeMap::new();
        for (member, seen) in &self.answers {
            for (&index, a) in seen {
                let entry = match a {
                    Answer::Write { sender, seq, .. } => Some((*sender, *seq)),
                    _ => None,
                };
                if let Some(other) = log.insert(index, entry) {
                    assert_eq!(other, entry, "member {member} applied another entry at {index}");
                }
            }
        }
        let mut reference = Store::new(Keyed::new());
        let mut applied: BTreeMap<(u64, u64), u32> = BTreeMap::new();
        for (&index, entry) in &log {
            let Some(key) = entry else { continue };
            let a = reference.apply_entry(index, &self.writes[key]).unwrap();
            if let Answer::Write { outcome: Written::Applied(_), .. } = a {
                *applied.entry(*key).or_default() += 1;
            }
        }
        for s in &self.senders {
            for seq in &s.acked {
                assert_eq!(applied.get(&(s.id, *seq)), Some(&1), "sender {:x}'s write {seq} applied once", s.id);
            }
        }
        let want = view(&reference);
        for (&id, n) in &self.nodes {
            if n.live.is_some() {
                assert_eq!(view(&self.store(id)), want, "member {id}");
            }
        }
    }
}

#[derive(Debug, PartialEq)]
struct View {
    series: Vec<(String, Vec<(i64, u64)>)>,
    names: Vec<String>,
    sessions: BTreeMap<u64, Session>,
    logs: Vec<Record>,
    sums: Vec<(Vec<(String, String)>, Vec<u64>)>,
}

fn view(s: &Store) -> View {
    let mut series = Vec::new();
    for name in s.metric_names() {
        let mut ids = Vec::new();
        s.select(&Selector::metric(name), &mut ids);
        for id in ids {
            let r = s.series(id).unwrap();
            let (mut ts, mut vs) = (Vec::new(), Vec::new());
            s.read(id, i64::MIN, i64::MAX, &mut ts, &mut vs).unwrap();
            series.push((format!("{}{:?}", r.metric(), r.tags().collect::<Vec<_>>()), ts.into_iter().zip(vs.into_iter().map(f64::to_bits)).collect()));
        }
    }
    series.sort();
    let all = kiln_obs::logs::search::parse("*").unwrap();
    let answer = kiln_obs::query::eval::eval(s, &kiln_obs::query::eval::Request::new("sum:m1{*} by {host}", 0, 2_000_000)).unwrap();
    View {
        series,
        names: s.metric_names().into_iter().map(str::to_owned).collect(),
        sessions: s.sessions().clone(),
        logs: s.search_logs(&all, i64::MIN, i64::MAX, usize::MAX).unwrap().records,
        sums: answer.series.into_iter().map(|x| (x.tags, x.values.into_iter().map(f64::to_bits).collect())).collect(),
    }
}

#[test]
fn three_voters_answer_alike_through_a_leader_crash_restarts_and_a_partition() {
    let mut c = Cluster::new(&[1, 2, 3]);
    c.until(Duration::from_secs(10), "a leader", |c| c.leader().is_some());
    c.add_senders(3, 120);
    c.until(Duration::from_secs(20), "40 writes", |c| c.acked() >= 40);
    // The leader's process ends mid-stream: the others elect, the senders send again.
    let first = c.leader().unwrap();
    c.crash(first);
    c.until(Duration::from_secs(20), "a new leader", |c| c.leader().is_some_and(|l| l != first));
    c.until(Duration::from_secs(20), "120 writes", |c| c.acked() >= 120);
    c.restart(first);
    // The leader now is cut off from both others: it cannot commit; they elect and go on.
    let second = c.leader().unwrap();
    for &other in c.nodes.keys().filter(|&&id| id != second).collect::<Vec<_>>() {
        c.cut.insert((second, other));
        c.cut.insert((other, second));
    }
    let before = c.acked();
    c.until(Duration::from_secs(20), "writes past the partition", |c| c.acked() >= before + 60 && c.leader().is_some_and(|l| l != second));
    c.cut.clear();
    // A follower's process ends and starts again while writes go on.
    let follower = *c.nodes.keys().find(|&&id| Some(id) != c.leader()).unwrap();
    c.crash(follower);
    c.until(Duration::from_secs(20), "more writes", |c| c.acked() >= before + 120);
    c.restart(follower);
    c.until(Duration::from_secs(60), "every write", Cluster::all_written);
    c.until(Duration::from_secs(20), "the group settled", Cluster::settled);
    c.verify();
    eprintln!("leaders {:?}, resent {}, duplicates {}", c.leaders, c.resent, c.duplicates);
    assert!(c.leaders.len() >= 3, "a leader for each failure: {:?}", c.leaders);
    assert!(c.resent > 0, "writes were sent again");
}

#[test]
fn a_write_committed_as_its_leader_dies_is_applied_once() {
    let mut c = Cluster::new(&[1, 2, 3]);
    c.until(Duration::from_secs(10), "a leader", |c| c.leader().is_some());
    c.add_senders(3, 200);
    for round in 0..6 {
        let target = 20 * (round + 1);
        c.until(Duration::from_secs(20), "writes", |c| c.acked() >= target);
        c.crash_on_append = true;
        c.until(Duration::from_secs(20), "the leader gone as a follower took its entries", |c| !c.crash_on_append);
        c.until(Duration::from_secs(20), "a new leader", |c| c.leader().is_some());
        let gone = *c.crashed.last().unwrap();
        c.until(Duration::from_secs(20), "writes after", |c| c.acked() >= target + 10);
        c.restart(gone);
    }
    c.until(Duration::from_secs(60), "every write", Cluster::all_written);
    c.until(Duration::from_secs(20), "the group settled", Cluster::settled);
    c.verify();
    eprintln!("crashed {:?}, resent {}, duplicates {}", c.crashed, c.resent, c.duplicates);
    assert!(c.duplicates > 0, "a write committed by the leader that died was sent again and found applied");
}

#[test]
fn a_voter_away_past_a_compaction_catches_up_by_image() {
    let mut c = Cluster::new(&[1, 2, 3]);
    c.until(Duration::from_secs(10), "a leader", |c| c.leader().is_some());
    c.add_senders(3, 150);
    c.until(Duration::from_secs(20), "30 writes", |c| c.acked() >= 30);
    let away = *c.nodes.keys().find(|&&id| Some(id) != c.leader()).unwrap();
    c.crash(away);
    c.until(Duration::from_secs(20), "120 writes", |c| c.acked() >= 120);
    // The two left compact twice, past everything the one away holds.
    c.compact_all(0);
    c.until(Duration::from_secs(20), "130 writes", |c| c.acked() >= 130);
    c.compact_all(0);
    c.restart(away);
    c.until(Duration::from_secs(60), "every write", Cluster::all_written);
    c.until(Duration::from_secs(30), "the group settled", Cluster::settled);
    assert!(c.snapshots_installed > 0, "the member came back by an image");
    c.verify();
    // And it serves its installed image in turn: it leads once the others are gone.
    let now_leader = c.leader().unwrap();
    if now_leader != away {
        c.member(now_leader).transfer(away).unwrap();
        c.until(Duration::from_secs(20), "the member back leads", |c| c.leader() == Some(away));
    }
    c.verify();
}

#[test]
fn a_learner_that_joins_after_compaction_takes_an_image_and_its_files() {
    // kiln's shape: the server the one voter, a runner a learner.
    let mut c = Cluster::new(&[1]);
    c.member(1).campaign().unwrap();
    c.until(Duration::from_secs(10), "the voter leads", |c| c.leader() == Some(1));
    c.add_senders(2, 150);
    c.until(Duration::from_secs(30), "100 writes", |c| c.acked() >= 100);
    // Compacted past the start: a member that joins now cannot take the log from its beginning.
    let (now, waker) = (c.now(), c.waker.clone());
    c.until(Duration::from_secs(10), "no write out", |c| c.nodes[&1].live.as_ref().unwrap().replica.in_flight() == 0);
    assert!(c.member(1).compact(0, now, &waker).unwrap(), "a compaction's start went out");
    c.until(Duration::from_secs(10), "the compaction durable", |c| c.nodes[&1].live.as_ref().unwrap().replica.in_flight() == 0);
    c.add_node(2);
    c.member(1).change(Vec::new(), &group::change(2, ConfChangeType::AddLearnerNode)).unwrap();
    c.until(Duration::from_secs(30), "the learner caught up", |c| c.snapshots_installed > 0 && c.nodes[&2].live.as_ref().unwrap().replica.applied().index >= 100);
    c.until(Duration::from_secs(60), "every write", Cluster::all_written);
    c.until(Duration::from_secs(20), "the group settled", Cluster::settled);
    c.verify();
    // The learner's process starts again: it opens on the image it installed and what came after.
    c.crash(2);
    c.restart(2);
    c.until(Duration::from_secs(20), "the group settled again", Cluster::settled);
    c.verify();
    assert!(c.store(2).block_count() > 0, "the learner holds the image's blocks");
}
