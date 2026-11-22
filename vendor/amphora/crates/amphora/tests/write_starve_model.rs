//! The follower-write candidates on a deterministic model (goal 9c63f7d4, kiln plan 01a1b8d174dc step 3;
//! docs/research/follower-write-starvation.md). A leader and a writing follower, in simulated
//! microseconds: the follower's ReadIndex, its apply, its preparation and its forward; the leader's
//! own preparations, one at a time until each resolves, as the owner serializes them
//! (replication.md, "Resolved transactions"); every entry appended, a refused one included, moving
//! the point (`on_base`). What each transaction read, wrote and allocated comes from the recorded
//! workload traces (bench/records/2026-11-07-studio-local2-write-starve-traces). Each candidate is
//! a validation or admission rule: A0 today's exact base; A1 page read-set validation; A1m with page
//! 1's size merged (two allocating writers still collide); A1r with allocation renumbered too; B
//! named commands run at the leader, queued behind its own writes; Bp ahead of them; C a write-slot lease; D A1m then C after two refusals; E
//! row-level rebase at the leader; EC E then C after two refusals; F a remote session at the
//! leader; G A0 then C after one refusal.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    clippy::string_slice
)]

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};
use std::fmt::Write as _;

/// One transaction of a trace: pages and keys, interned.
#[derive(Clone, Debug, Default)]
struct Tx {
    read: Vec<u32>,
    written: Vec<u32>,
    allocated: Vec<u32>,
    /// Page 1 written with only its size and checksum changed (every page-1 write in the traces).
    page1_alloc_only: bool,
    /// Keys read and written, strictly: an append reads its table's `max` and writes it.
    strict_read: Vec<u32>,
    strict_written: Vec<u32>,
    /// Keys a row-level rebase must keep: an append's `max` and new key dropped, as the rebase
    /// takes the next key on the newer state.
    rebase_read: Vec<u32>,
    rebase_written: Vec<u32>,
    statements: u64,
}

fn intersects(a: &[u32], b: &[u32]) -> bool {
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => return true,
        }
    }
    false
}

/// A workload's two streams, as the trace recorded them.
#[derive(Clone, Default)]
struct Streams {
    leader: Vec<Tx>,
    follower: Vec<Tx>,
}

/// Reads the traces' JSON lines: only the fields and shapes `workload_traces` writes.
fn load(text: &str) -> Vec<(String, Streams)> {
    let mut keys: HashMap<String, u32> = HashMap::new();
    let mut intern = |key: &str| -> u32 {
        let next = keys.len() as u32;
        *keys.entry(key.to_string()).or_insert(next)
    };
    let mut out: Vec<(String, Streams)> = Vec::new();
    for line in text.lines() {
        if line.starts_with("{\"summary\"") || line.is_empty() {
            continue;
        }
        let workload = string_field(line, "workload");
        let role = string_field(line, "role");
        let numbers = |name: &str| -> Vec<u32> {
            let mut v: Vec<u32> = list_field(line, name)
                .iter()
                .map(|n| n.parse().unwrap())
                .collect();
            v.sort_unstable();
            v
        };
        let page1 = list_field(line, "page1");
        let keys_read = list_field(line, "keys_read");
        let keys_written = list_field(line, "keys_written");
        let appended: HashSet<String> = keys_read
            .iter()
            .filter_map(|k| k.strip_suffix("#max").map(str::to_string))
            .collect();
        let table = |k: &str| k.split('#').next().unwrap().to_string();
        let is_append =
            |k: &str| appended.contains(&table(k)) && !keys_read.contains(&k.to_string());
        let mut strict_read: Vec<u32> = keys_read.iter().map(|k| intern(k)).collect();
        let mut strict_written: Vec<u32> = keys_written
            .iter()
            .map(|k| {
                if is_append(k) {
                    intern(&format!("{}#max", table(k)))
                } else {
                    intern(k)
                }
            })
            .collect();
        let mut rebase_read: Vec<u32> = keys_read
            .iter()
            .filter(|k| !k.ends_with("#max"))
            .map(|k| intern(k))
            .collect();
        let mut rebase_written: Vec<u32> = keys_written
            .iter()
            .filter(|k| !is_append(k))
            .map(|k| intern(k))
            .collect();
        for v in [
            &mut strict_read,
            &mut strict_written,
            &mut rebase_read,
            &mut rebase_written,
        ] {
            v.sort_unstable();
            v.dedup();
        }
        let tx = Tx {
            read: numbers("read"),
            written: numbers("written"),
            allocated: numbers("allocated"),
            page1_alloc_only: !page1.is_empty()
                && page1.iter().all(|p| p == "size" || p == "reserved"),
            strict_read,
            strict_written,
            rebase_read,
            rebase_written,
            statements: (keys_written.len() as u64).max(1),
        };
        if out.last().is_none_or(|(name, _)| *name != workload) {
            out.push((workload.clone(), Streams::default()));
        }
        let streams = &mut out.last_mut().unwrap().1;
        if role == "F" {
            streams.follower.push(tx);
        } else {
            streams.leader.push(tx);
        }
    }
    out
}

fn string_field(line: &str, name: &str) -> String {
    let start = line.find(&format!("\"{name}\":\"")).unwrap() + name.len() + 4;
    let end = line[start..].find('"').unwrap();
    line[start..start + end].to_string()
}

fn list_field(line: &str, name: &str) -> Vec<String> {
    let start = line.find(&format!("\"{name}\":[")).unwrap() + name.len() + 4;
    let end = line[start..].find(']').unwrap();
    line[start..start + end]
        .split(',')
        .filter(|s| !s.is_empty())
        .map(|s| s.trim_matches('"').to_string())
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Candidate {
    A0,
    A1,
    A1m,
    A1r,
    B,
    Bp,
    C,
    D,
    E,
    EC,
    F,
    G,
}
const CANDIDATES: [Candidate; 12] = [
    Candidate::A0,
    Candidate::A1,
    Candidate::A1m,
    Candidate::A1r,
    Candidate::B,
    Candidate::Bp,
    Candidate::C,
    Candidate::D,
    Candidate::E,
    Candidate::EC,
    Candidate::F,
    Candidate::G,
];

/// How a follower's attempt reaches the log.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Path {
    /// ReadIndex, local preparation, forward.
    Optimistic,
    /// The write slot first (C), then as Optimistic.
    Lease,
    /// The command to the leader (B).
    Command,
    /// The write slot, then each statement a round trip to the leader (F).
    Remote,
}

impl Candidate {
    fn path(self, refusals: u32) -> Path {
        match self {
            Self::B | Self::Bp => Path::Command,
            Self::C => Path::Lease,
            Self::F => Path::Remote,
            Self::D | Self::EC if refusals >= 2 => Path::Lease,
            Self::G if refusals >= 1 => Path::Lease,
            _ => Path::Optimistic,
        }
    }
}

/// The model's fixed costs, in microseconds: model parameters, not measured tuning constants. The
/// A/B's conclusions are checked against them by `sensitivity` before they are reported.
#[derive(Clone, Copy)]
struct Costs {
    rtt: u64,
    /// A preparation: a fixed part and a part per page read.
    prepare_fixed: u64,
    prepare_per_page: u64,
    /// The follower's think time between its writes, and its write's deadline.
    think: u64,
    deadline: u64,
    /// Simulated time.
    duration: u64,
    /// The leader's queue of its own writes; past it an arrival is refused.
    leader_queue: usize,
    /// Kung and Robinson's finite window of recent write sets (f4): an older base is refused.
    window: usize,
    /// How long a granted write slot waits for the follower's forward.
    lease: u64,
}

impl Costs {
    fn prepare(&self, tx: &Tx) -> u64 {
        self.prepare_fixed + self.prepare_per_page * tx.read.len() as u64
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Ev {
    LeaderArrival,
    LeaderTry,
    LeaderAppendOwn,
    LeaderCommitted { entry: usize },
    FollowerStart,
    LeaderReadIndex,
    LeaderIntent,
    LeaderCommand,
    FollowerReadIndex { index: usize },
    FollowerPrepare,
    LeaderForward { base: usize },
    LeaderRemote,
    FollowerOutcome { entry: usize },
    LeaseExpire { generation: u64 },
}

/// What the leader's slot runs.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Work {
    Own {
        tx: usize,
        arrived: u64,
    },
    /// A follower's write run at the leader: B's command, or E's rebase.
    ForFollower,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Origin {
    Leader { tx: usize },
    Follower { tx: usize },
}

struct Entry {
    origin: Origin,
    ok: bool,
    commit: u64,
    learned: u64,
}

/// What one run counted.
#[derive(Default, Clone)]
struct Counts {
    follower_latencies: Vec<u64>,
    follower_misses: u64,
    follower_attempts: u64,
    refusals: u64,
    false_refusals: u64,
    /// Refusals where no row conflicted once appends are rebased (`rebase_*` keys).
    false_refusals_rows: u64,
    /// Forwards that met no entry since their base, and that met some but read none of their pages.
    forwards_alone: u64,
    forwards_disjoint: u64,
    leader_offered: u64,
    leader_done: u64,
    leader_refused_admission: u64,
    leader_refused_stale: u64,
    leader_latencies: Vec<u64>,
    leader_stall: u64,
}

struct Sim<'a> {
    cand: Candidate,
    costs: Costs,
    streams: &'a Streams,
    follower_on: bool,
    rate: f64,
    rng: u64,
    now: u64,
    seq: u64,
    events: BinaryHeap<Reverse<(u64, u64, Ev)>>,
    log: Vec<Entry>,
    committed: usize,
    leader_busy: bool,
    leader_queue: VecDeque<Work>,
    leader_next_tx: usize,
    // The leader's own transaction last begun: the trace position the follower's meets.
    leader_exec_tx: usize,
    // The leader's one preparation under way: what, on which base, and its entry once appended.
    in_flight: Option<(Work, usize, Option<usize>)>,
    lease: Option<u64>,
    // The follower's entry appended under the slot: the slot ends at its commit.
    lease_entry: Option<usize>,
    lease_generation: u64,
    lease_since: u64,
    intent_waiting: bool,
    // The follower's one write under way.
    follower_tx: usize,
    follower_began: u64,
    follower_refusals: u32,
    follower_base: usize,
    counts: Counts,
}

impl<'a> Sim<'a> {
    fn new(
        cand: Candidate,
        costs: Costs,
        streams: &'a Streams,
        rate: f64,
        seed: u64,
        follower_on: bool,
    ) -> Self {
        Self {
            cand,
            costs,
            streams,
            follower_on,
            rate,
            rng: seed,
            now: 0,
            seq: 0,
            events: BinaryHeap::new(),
            log: Vec::new(),
            committed: 0,
            leader_busy: false,
            leader_queue: VecDeque::new(),
            leader_next_tx: 0,
            leader_exec_tx: 0,
            in_flight: None,
            lease: None,
            lease_entry: None,
            lease_generation: 0,
            lease_since: 0,
            intent_waiting: false,
            follower_tx: 0,
            follower_began: 0,
            follower_refusals: 0,
            follower_base: 0,
            counts: Counts::default(),
        }
    }

    fn random(&mut self) -> f64 {
        self.rng = self.rng.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.rng;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        ((z ^ (z >> 31)) >> 11) as f64 / (1u64 << 53) as f64
    }

    /// One way across the network: half a round trip, up to 20% more.
    fn hop(&mut self) -> u64 {
        let half = self.costs.rtt / 2;
        half + (half as f64 * 0.2 * self.random()) as u64
    }

    fn at(&mut self, delay: u64, ev: Ev) {
        self.seq += 1;
        self.events.push(Reverse((self.now + delay, self.seq, ev)));
    }

    fn follower_tx(&self) -> &'a Tx {
        let streams: &'a Streams = self.streams;
        &streams.follower[self.follower_tx % streams.follower.len()]
    }

    fn tx(&self, origin: Origin) -> &'a Tx {
        let streams: &'a Streams = self.streams;
        match origin {
            Origin::Leader { tx } => &streams.leader[tx % streams.leader.len()],
            Origin::Follower { tx } => &streams.follower[tx % streams.follower.len()],
        }
    }

    /// The followers' applied prefix now: entries whose commit they have heard of.
    fn follower_applied(&self) -> usize {
        self.log.partition_point(|e| e.learned <= self.now)
    }

    /// Whether `tx` prepared on `base` applies at the log's end, by the candidate's rule; and,
    /// when refused, whether a row it read was written in between (false when not).
    fn validate(&self, tx: &Tx, base: usize, rule: Candidate) -> Result<(), bool> {
        let pos = self.log.len();
        let between = &self.log[base..pos];
        let truly = || {
            between
                .iter()
                .filter(|e| e.ok)
                .any(|e| intersects(&self.tx(e.origin).strict_written, &tx.strict_read))
        };
        let ok = match rule {
            Candidate::A0
            | Candidate::B
            | Candidate::Bp
            | Candidate::C
            | Candidate::F
            | Candidate::G => base == pos,
            _ if between.len() > self.costs.window => false,
            Candidate::A1 => !between
                .iter()
                .filter(|e| e.ok)
                .any(|e| intersects(&self.tx(e.origin).written, &tx.read)),
            Candidate::A1m | Candidate::D | Candidate::A1r => {
                let renumber = rule == Candidate::A1r;
                !between.iter().filter(|e| e.ok).any(|e| {
                    let other = self.tx(e.origin);
                    let rest = |pages: &[u32]| -> Vec<u32> {
                        pages.iter().copied().filter(|p| *p != 1).collect()
                    };
                    let page1 = other.written.first() == Some(&1)
                        && tx.read.first() == Some(&1)
                        && (!other.page1_alloc_only || (!renumber && !tx.allocated.is_empty()));
                    page1 || intersects(&rest(&other.written), &rest(&tx.read))
                })
            }
            Candidate::E | Candidate::EC => !between
                .iter()
                .filter(|e| e.ok)
                .any(|e| intersects(&self.tx(e.origin).rebase_written, &tx.rebase_read)),
        };
        if ok { Ok(()) } else { Err(truly()) }
    }

    /// Whether E's write, valid by rows, also applies as its pages are (A1m), or must be rebased.
    fn pages_apply(&self, tx: &Tx, base: usize) -> bool {
        self.validate(tx, base, Candidate::A1m).is_ok()
    }

    fn append(&mut self, origin: Origin, ok: bool) -> usize {
        let commit = (self.now + self.costs.rtt).max(self.log.last().map_or(0, |e| e.commit));
        let hop = self.hop();
        let learned = (commit + hop).max(self.log.last().map_or(0, |e| e.learned));
        self.log.push(Entry {
            origin,
            ok,
            commit,
            learned,
        });
        let entry = self.log.len() - 1;
        self.at(commit - self.now, Ev::LeaderCommitted { entry });
        entry
    }

    fn release_lease(&mut self) {
        if self.lease.take().is_some() {
            self.counts.leader_stall += self.now - self.lease_since;
            self.at(0, Ev::LeaderTry);
        }
    }

    fn grant(&mut self) {
        self.intent_waiting = false;
        self.lease_generation += 1;
        self.lease = Some(self.lease_generation);
        self.lease_since = self.now;
        let generation = self.lease_generation;
        if self.cand.path(self.follower_refusals) == Path::Remote {
            // The session holds the slot for its round trips, each statement one.
            let n = self.follower_tx().statements;
            let p = self.costs.prepare(self.follower_tx());
            let session = self.costs.rtt * (1 + n) + p;
            self.at(session, Ev::LeaderRemote);
            self.at(session + self.costs.lease, Ev::LeaseExpire { generation });
        } else {
            self.at(self.costs.lease, Ev::LeaseExpire { generation });
            self.at(0, Ev::LeaderReadIndex);
        }
    }

    fn step(&mut self, ev: Ev) {
        match ev {
            Ev::LeaderArrival => {
                self.counts.leader_offered += 1;
                if self.leader_queue.len() >= self.costs.leader_queue {
                    self.counts.leader_refused_admission += 1;
                } else {
                    let tx = self.leader_next_tx;
                    self.leader_next_tx += 1;
                    self.leader_queue.push_back(Work::Own {
                        tx,
                        arrived: self.now,
                    });
                    self.at(0, Ev::LeaderTry);
                }
                let gap = (-self.random().max(1e-12).ln() / self.rate * 1e6) as u64;
                self.at(gap.max(1), Ev::LeaderArrival);
            }
            Ev::LeaderTry => {
                if self.leader_busy || self.lease.is_some() {
                    return;
                }
                // Every candidate but today's prepares at the leader's log tail, as the floor's
                // session does (replication.md, "the floor session's rule"): an entry appended and
                // not yet applied would make its base stale.
                if self.cand != Candidate::A0 && self.committed < self.log.len() {
                    return;
                }
                if self.intent_waiting {
                    self.grant();
                    return;
                }
                let Some(work) = self.leader_queue.pop_front() else {
                    return;
                };
                self.leader_busy = true;
                let base = self.committed;
                let tx = match work {
                    Work::Own { tx, .. } => {
                        self.leader_exec_tx = tx;
                        self.tx(Origin::Leader { tx })
                    }
                    Work::ForFollower => self.follower_tx(),
                };
                let p = self.costs.prepare(tx);
                self.in_flight = Some((work, base, None));
                self.at(p, Ev::LeaderAppendOwn);
            }
            Ev::LeaderAppendOwn => {
                let Some((work, base, None)) = self.in_flight else {
                    panic!("an append with no preparation under way");
                };
                let origin = match work {
                    Work::Own { tx, .. } => Origin::Leader { tx },
                    Work::ForFollower => Origin::Follower {
                        tx: self.follower_tx,
                    },
                };
                // The leader's own preparations meet the same rule as a follower's.
                // E's rows are a follower's; the leader's own write, refused, prepares again on the
                // newer state, which is itself the rebase.
                let rule = match self.cand {
                    Candidate::B | Candidate::Bp | Candidate::C | Candidate::F | Candidate::G => {
                        Candidate::A0
                    }
                    Candidate::D | Candidate::E | Candidate::EC => Candidate::A1m,
                    c => c,
                };
                let ok = self.validate(self.tx(origin), base, rule).is_ok();
                let entry = self.append(origin, ok);
                if let Work::ForFollower = work {
                    let d = self.log[entry].learned - self.now;
                    self.at(d, Ev::FollowerOutcome { entry });
                }
                self.in_flight = Some((work, base, Some(entry)));
            }
            Ev::LeaderCommitted { entry } => {
                self.committed = self.committed.max(entry + 1);
                if self.lease_entry == Some(entry) {
                    self.lease_entry = None;
                    self.release_lease();
                }
                self.at(0, Ev::LeaderTry);
                let Some((work, _, Some(own))) = self.in_flight else {
                    return;
                };
                if own != entry {
                    return;
                }
                self.in_flight = None;
                self.leader_busy = false;
                if let Work::Own { tx, arrived } = work {
                    if self.log[entry].ok {
                        self.counts.leader_done += 1;
                        self.counts.leader_latencies.push(self.now - arrived);
                    } else {
                        self.counts.leader_refused_stale += 1;
                        self.leader_queue.push_front(Work::Own { tx, arrived });
                    }
                }
                self.at(0, Ev::LeaderTry);
            }
            Ev::FollowerStart => {
                self.counts.follower_attempts += 1;
                let hop = self.hop();
                match self.cand.path(self.follower_refusals) {
                    Path::Optimistic => self.at(hop, Ev::LeaderReadIndex),
                    Path::Lease | Path::Remote => self.at(hop, Ev::LeaderIntent),
                    Path::Command => self.at(hop, Ev::LeaderCommand),
                }
            }
            Ev::LeaderIntent => {
                self.intent_waiting = true;
                self.at(0, Ev::LeaderTry);
            }
            Ev::LeaderCommand => {
                if self.cand == Candidate::Bp {
                    self.leader_queue.push_front(Work::ForFollower);
                } else {
                    self.leader_queue.push_back(Work::ForFollower);
                }
                self.at(0, Ev::LeaderTry);
            }
            Ev::LeaderReadIndex => {
                // Raft's ReadIndex (Ongaro §6.4): the commit index, once a heartbeat round shows
                // the leader still leads; with the slot held, the log's end, all of it committed.
                let index = if self.lease.is_some() {
                    self.log.len()
                } else {
                    self.committed
                };
                let hop = self.hop();
                self.at(self.costs.rtt + hop, Ev::FollowerReadIndex { index });
            }
            Ev::FollowerReadIndex { index } => {
                if self.follower_applied() >= index {
                    self.at(0, Ev::FollowerPrepare);
                } else {
                    let wait = self.log[index - 1].learned - self.now;
                    self.at(wait, Ev::FollowerReadIndex { index });
                }
            }
            Ev::FollowerPrepare => {
                // The session runs now, on the state it meets: the trace's transaction recorded
                // beside the leader's position (nine of the leader's to one of the follower's), so
                // a write prepared again after a refusal meets the pages the leader writes then.
                self.follower_tx = self.follower_tx.max(self.leader_exec_tx / 9);
                let base = self.follower_applied();
                let p = self.costs.prepare(self.follower_tx());
                let hop = self.hop();
                self.at(p + hop, Ev::LeaderForward { base });
            }
            Ev::LeaderForward { base } => {
                let tx = self.follower_tx();
                let held = self.lease.is_some();
                let rule = match self.cand {
                    Candidate::D | Candidate::EC if held => Candidate::A0,
                    Candidate::G | Candidate::C => Candidate::A0,
                    c => c,
                };
                let verdict = self.validate(tx, base, rule);
                if base == self.log.len() {
                    self.counts.forwards_alone += 1;
                } else if self.validate(tx, base, Candidate::A1).is_ok() {
                    self.counts.forwards_disjoint += 1;
                }
                if matches!(self.cand, Candidate::E | Candidate::EC)
                    && !held
                    && verdict.is_ok()
                    && !self.pages_apply(tx, base)
                {
                    // Valid by rows, not by pages: the leader rebases the rows on its state, in
                    // its slot, ahead of its own writes.
                    self.leader_queue.push_front(Work::ForFollower);
                    self.at(0, Ev::LeaderTry);
                    return;
                }
                if let Err(truly) = verdict {
                    if !truly {
                        self.counts.false_refusals += 1;
                    }
                    if self.validate(tx, base, Candidate::E).is_ok() {
                        self.counts.false_refusals_rows += 1;
                    }
                }
                let entry = self.append(
                    Origin::Follower {
                        tx: self.follower_tx,
                    },
                    verdict.is_ok(),
                );
                self.follower_base = base;
                if held {
                    self.lease_entry = Some(entry);
                }
                let d = self.log[entry].learned - self.now;
                self.at(d, Ev::FollowerOutcome { entry });
            }
            Ev::LeaderRemote => {
                let entry = self.append(
                    Origin::Follower {
                        tx: self.follower_tx,
                    },
                    true,
                );
                self.lease_entry = Some(entry);
                let d = self.log[entry].learned - self.now;
                self.at(d, Ev::FollowerOutcome { entry });
            }
            Ev::FollowerOutcome { entry } => {
                let ok = self.log[entry].ok;
                let late = self.now > self.follower_began + self.costs.deadline;
                if ok && !late {
                    self.counts
                        .follower_latencies
                        .push(self.now - self.follower_began);
                    self.next_write();
                } else if late {
                    self.counts.follower_misses += 1;
                    self.next_write();
                } else {
                    self.counts.refusals += 1;
                    self.follower_refusals += 1;
                    self.at(0, Ev::FollowerStart);
                }
            }
            Ev::LeaseExpire { generation } => {
                if self.lease == Some(generation) && self.lease_entry.is_none() {
                    self.release_lease();
                }
            }
        }
    }

    fn next_write(&mut self) {
        self.follower_tx += 1;
        self.follower_refusals = 0;
        self.follower_began = self.now + self.costs.think;
        self.at(self.costs.think, Ev::FollowerStart);
    }

    fn run(mut self) -> Counts {
        if self.rate > 0.0 {
            self.at(0, Ev::LeaderArrival);
        }
        if self.follower_on {
            self.follower_began = 0;
            self.at(0, Ev::FollowerStart);
        }
        while let Some(Reverse((t, _, ev))) = self.events.pop() {
            if t > self.costs.duration {
                break;
            }
            self.now = t;
            self.step(ev);
        }
        // A write still under way past its deadline at the end is a miss; one within it is not
        // counted either way.
        if self.follower_on && self.now > self.follower_began + self.costs.deadline {
            self.counts.follower_misses += 1;
        }
        self.counts
    }
}

fn percentile(sorted: &[u64], q: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let i = ((sorted.len() - 1) as f64 * q).round() as usize;
    sorted[i]
}

fn costs(rtt: u64) -> Costs {
    Costs {
        rtt,
        prepare_fixed: 100,
        prepare_per_page: 5,
        think: 100_000,
        deadline: 30_000_000,
        duration: 120_000_000,
        leader_queue: 4096,
        window: 4096,
        lease: 4 * rtt + 50_000,
    }
}

/// One cell: a candidate on a workload at a rate and round trip, over seeds, as one JSON line.
fn cell(
    (name, variant): (&str, &str),
    streams: &Streams,
    cand: Candidate,
    rate: f64,
    costs: Costs,
    seeds: &[u64],
) -> String {
    let mut all = Counts::default();
    let mut baseline = Counts::default();
    for &seed in seeds {
        let c = Sim::new(cand, costs, streams, rate, seed, true).run();
        let b = Sim::new(Candidate::A0, costs, streams, rate, seed, false).run();
        all.follower_latencies.extend(c.follower_latencies);
        all.follower_misses += c.follower_misses;
        all.follower_attempts += c.follower_attempts;
        all.refusals += c.refusals;
        all.false_refusals += c.false_refusals;
        all.false_refusals_rows += c.false_refusals_rows;
        all.forwards_alone += c.forwards_alone;
        all.forwards_disjoint += c.forwards_disjoint;
        all.leader_offered += c.leader_offered;
        all.leader_done += c.leader_done;
        all.leader_refused_admission += c.leader_refused_admission;
        all.leader_refused_stale += c.leader_refused_stale;
        all.leader_latencies.extend(c.leader_latencies);
        all.leader_stall += c.leader_stall;
        baseline.leader_done += b.leader_done;
        baseline.leader_latencies.extend(b.leader_latencies);
    }
    all.follower_latencies.sort_unstable();
    all.leader_latencies.sort_unstable();
    baseline.leader_latencies.sort_unstable();
    let ms = |us: u64| us as f64 / 1000.0;
    let f = &all.follower_latencies;
    let writes = f.len() as u64 + all.follower_misses;
    let seconds = costs.duration as f64 / 1e6 * seeds.len() as f64;
    format!(
        "{{\"workload\":\"{name}\",\"variant\":\"{variant}\",\"candidate\":\"{cand:?}\",\"rate\":{rate},\"rtt_ms\":{},\"seeds\":{},\"follower_done\":{},\"follower_misses\":{},\"miss_rate\":{:.4},\"p50_ms\":{:.2},\"p99_ms\":{:.2},\"max_ms\":{:.2},\"attempts\":{},\"refusals\":{},\"false_refusals\":{},\"false_refusals_rows\":{},\"forwards_alone\":{},\"forwards_disjoint\":{},\"leader_per_s\":{:.1},\"leader_baseline_per_s\":{:.1},\"leader_p99_ms\":{:.2},\"leader_baseline_p99_ms\":{:.2},\"leader_refused_stale\":{},\"leader_refused_admission\":{},\"leader_stall_s\":{:.3}}}",
        ms(costs.rtt),
        seeds.len(),
        f.len(),
        all.follower_misses,
        if writes == 0 {
            0.0
        } else {
            all.follower_misses as f64 / writes as f64
        },
        ms(percentile(f, 0.5)),
        ms(percentile(f, 0.99)),
        ms(f.last().copied().unwrap_or(0)),
        all.follower_attempts,
        all.refusals,
        all.false_refusals,
        all.false_refusals_rows,
        all.forwards_alone,
        all.forwards_disjoint,
        all.leader_done as f64 / seconds,
        baseline.leader_done as f64 / seconds,
        ms(percentile(&all.leader_latencies, 0.99)),
        ms(percentile(&baseline.leader_latencies, 0.99)),
        all.leader_refused_stale,
        all.leader_refused_admission,
        all.leader_stall as f64 / 1e6,
    )
}

/// A trace-free stand-in: every transaction reads and writes one leaf of one table, and page 1.
fn one_leaf() -> Streams {
    let tx = Tx {
        read: vec![1, 2, 3],
        written: vec![3],
        strict_read: vec![0],
        strict_written: vec![0],
        rebase_read: vec![],
        rebase_written: vec![],
        statements: 1,
        ..Tx::default()
    };
    Streams {
        leader: vec![tx.clone()],
        follower: vec![tx],
    }
}

/// Goal 9c63f7d4, plan 01a1b8d174dc step 3, the control (f1, f3). Do: today's exact base, the
/// leader writing 100 a second (kiln's server wrote every 10 to 20 ms), a 10 ms round trip, the
/// follower re-running each refused write as kiln's `store::write` does. Expect: the follower's
/// writes are refused round after round until their 30 s deadline, as kiln's runner's were.
#[test]
fn today_a_followers_write_starves_under_a_steady_leader() {
    let streams = one_leaf();
    let mut c = costs(10_000);
    c.duration = 90_000_000;
    let counts = Sim::new(Candidate::A0, c, &streams, 100.0, 1, true).run();
    assert!(
        counts.follower_misses >= 2,
        "misses {}",
        counts.follower_misses
    );
    assert!(
        counts.follower_latencies.is_empty(),
        "a write got through: {:?}",
        counts.follower_latencies
    );
    assert!(counts.refusals > 100, "refusals {}", counts.refusals);
}

/// Non-vacuity of the model (goal 9c63f7d4). Do: the same follower with no leader writes. Expect:
/// every write applies first time, in four one-way hops and two round trips plus its preparation.
#[test]
fn with_no_leader_writes_every_follower_write_applies_first_time() {
    let streams = one_leaf();
    let c = costs(10_000);
    let counts = Sim::new(Candidate::A0, c, &streams, 0.0, 1, true).run();
    assert_eq!(counts.refusals, 0);
    assert_eq!(counts.follower_misses, 0);
    let mut l = counts.follower_latencies.clone();
    l.sort_unstable();
    assert!(l.len() > 500, "{}", l.len());
    let p50 = percentile(&l, 0.5);
    assert!((40_000..46_000).contains(&p50), "p50 {p50}");
}

/// Each candidate on the control's own case (goal 9c63f7d4). Expect: the pessimistic and
/// leader-run candidates never miss; the control does.
#[test]
fn the_lease_and_the_leader_run_candidates_do_not_starve_on_the_control() {
    let streams = one_leaf();
    let mut c = costs(10_000);
    c.duration = 60_000_000;
    for cand in [
        Candidate::B,
        Candidate::Bp,
        Candidate::C,
        Candidate::F,
        Candidate::G,
    ] {
        let counts = Sim::new(cand, c, &streams, 100.0, 1, true).run();
        assert_eq!(counts.follower_misses, 0, "{cand:?}");
        assert!(counts.follower_latencies.len() > 100, "{cand:?}");
    }
}

/// The A/B (goal 9c63f7d4, plan 01a1b8d174dc step 3): every candidate on every workload, at leader
/// rates of 10, 100 and 1,000 a second and round trips of 1, 10 and 100 ms, twenty seeds each.
/// Reads the traces from `$AMPHORA_TRACES` (uncompressed), writes JSON lines to
/// `$AMPHORA_MODEL_OUT`. Run in release with `--ignored`.
#[test]
#[ignore = "the A/B sweep: run in release with AMPHORA_TRACES and AMPHORA_MODEL_OUT set"]
fn the_ab_sweep() {
    let text = std::fs::read_to_string(std::env::var_os("AMPHORA_TRACES").unwrap()).unwrap();
    let workloads = load(&text);
    let seeds: [u64; 20] = std::array::from_fn(|i| i as u64 + 1);
    let lines: Vec<String> = std::thread::scope(|scope| {
        let handles: Vec<_> = workloads
            .iter()
            .map(|(name, streams)| {
                scope.spawn(move || {
                    let mut out = String::new();
                    for rate in [10.0, 100.0, 1000.0] {
                        for rtt in [1_000, 10_000, 100_000] {
                            for cand in CANDIDATES {
                                let line =
                                    cell((name, "base"), streams, cand, rate, costs(rtt), &seeds);
                                writeln!(out, "{line}").unwrap();
                            }
                        }
                    }
                    // Sensitivity: preparations four times slower per page and a follower
                    // writing once a second instead of ten times, at the middle round trip.
                    for rate in [10.0, 100.0] {
                        for cand in CANDIDATES {
                            let mut c = costs(10_000);
                            c.prepare_per_page = 20;
                            c.think = 1_000_000;
                            let line = cell((name, "slow"), streams, cand, rate, c, &seeds);
                            writeln!(out, "{line}").unwrap();
                        }
                    }
                    out
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    std::fs::write(
        std::env::var_os("AMPHORA_MODEL_OUT").unwrap(),
        lines.concat(),
    )
    .unwrap();
}
