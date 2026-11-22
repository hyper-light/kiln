//! CP-T1.5 (AC-T1.5.3): the Records suite (`amphora_api::conformance`: kiln-seams' RECORDS-1 to 14
//! ported, and Amphora's own cases) on Amphora's `Namespace`, in memory, on a file and through the
//! local owner's transport; and the simulator against Amphora, request by request, over seeded
//! histories: every answer equal, refusals included.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros
)]

use std::time::{Duration, Instant};

use amphora::database::Database;
use amphora::transport::Client;
use amphora_api::conformance::{Case, cases, run};
use amphora_api::sim::{SimLimits, SimRecords};
use amphora_api::{Cond, Consistency, Records, Rev, Scan, TX_MAX, Tx, Write};

/// The tests' hang guard (local-owner-transport.md, "The tests' budgets"); no verdict rests on it.
const HANG: Duration = Duration::from_secs(120);

fn until() -> Instant {
    Instant::now() + HANG
}

fn passes_every_case<R: Records>(
    what: &str,
    cases: &[Case],
    make: impl FnMut() -> Result<R, amphora_api::Refusal>,
) {
    let report = run(cases, make, HANG);
    assert!(
        report.is_ok() && report.passed.len() == cases.len(),
        "{what}: {:?}",
        report.failed
    );
}

/// AC-T1.5.3: Amphora in memory passes every case, a namespace of its own for each.
#[test]
fn amphora_in_memory_passes_the_records_suite() {
    let db = Database::in_memory(until()).unwrap();
    let mut n = 0;
    passes_every_case("in memory", &cases(), || {
        n += 1;
        db.namespace(&format!("case-{n}"))
    });
}

/// AC-T1.5.3: Amphora on a file passes every case.
#[test]
fn amphora_on_a_file_passes_the_records_suite() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(&dir.path().join("records.db"), until()).unwrap();
    let mut n = 0;
    passes_every_case("on a file", &cases(), || {
        n += 1;
        db.namespace(&format!("case-{n}"))
    });
}

/// AC-T1.5.3: a client of the local owner passes every case, through the transport.
#[test]
fn amphora_through_the_local_owner_passes_the_records_suite() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("records.db");
    let _owner = Database::open(&path, until()).unwrap();
    let client = Client::connect(&path, until()).unwrap();
    let mut n = 0;
    passes_every_case("through the transport", &cases(), || {
        n += 1;
        client.namespace(&format!("case-{n}"))
    });
}

/// A seeded source of choices (xorshift64*, Vigna 2016): the same seed, the same history.
struct Choices(u64);

impl Choices {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d) % n.max(1)
    }
    /// A choice below `n`, as an index.
    fn index(&mut self, n: usize) -> usize {
        usize::try_from(self.below(u64::try_from(n).unwrap())).unwrap()
    }
    fn pick<'a, T>(&mut self, of: &'a [T]) -> &'a T {
        &of[self.index(of.len())]
    }
}

const FAMILIES: [&str; 2] = ["f", "g"];
const KEYS: [&[u8]; 6] = [b"a", b"a\x00", b"ab", b"b", b"b\xff", b"c"];

/// One request, made to both stores; their answers must be equal.
fn step(c: &mut Choices, amphora: &dyn Records, sim: &dyn Records) -> String {
    let head = sim.head(until()).unwrap();
    let rev = c.below(head + 2);
    let family = *c.pick(&FAMILIES);
    match c.below(7) {
        0 | 1 => {
            let mut tx = Tx::default();
            for _ in 0..c.below(3) {
                let (f, k) = (*c.pick(&FAMILIES), c.pick(&KEYS).to_vec());
                tx.conds.push(if c.below(2) == 0 {
                    Cond::Absent {
                        family: f.into(),
                        key: k,
                    }
                } else {
                    Cond::At {
                        family: f.into(),
                        key: k,
                        rev,
                    }
                });
            }
            for _ in 0..c.below(4) {
                let (f, k) = (*c.pick(&FAMILIES), c.pick(&KEYS).to_vec());
                tx.writes.push(if c.below(3) == 0 {
                    Write::Delete {
                        family: f.into(),
                        key: k,
                    }
                } else {
                    Write::Put {
                        family: f.into(),
                        key: k,
                        value: c.below(4).to_be_bytes().to_vec(),
                    }
                });
            }
            let (a, s) = (
                amphora.commit(tx.clone(), until()),
                sim.commit(tx.clone(), until()),
            );
            assert_eq!(a, s, "commit {tx:?}");
            format!("commit {tx:?}: {s:?}")
        }
        2 => {
            let key = c.pick(&KEYS);
            let at = if c.below(2) == 0 {
                Consistency::Linearizable
            } else {
                Consistency::Snapshot(rev)
            };
            let (a, s) = (
                amphora.get(family, key, at, until()),
                sim.get(family, key, at, until()),
            );
            assert_eq!(a, s, "get {family} {key:?} {at:?}");
            format!("get {family} {key:?} {at:?}: {s:?}")
        }
        3 => {
            let prefix = c.pick(&KEYS)[..c.index(2)].to_vec();
            let after = (c.below(2) == 0).then(|| c.pick(&KEYS).to_vec());
            let at = if c.below(2) == 0 {
                Consistency::Linearizable
            } else {
                Consistency::Snapshot(rev)
            };
            let scan = Scan {
                prefix,
                after,
                limit: c.index(4),
                at,
            };
            let (a, s) = (
                amphora.scan(family, scan.clone(), until()),
                sim.scan(family, scan.clone(), until()),
            );
            assert_eq!(a, s, "scan {family} {scan:?}");
            format!("scan {family} {scan:?}: {s:?}")
        }
        4 => {
            let limit = c.index(5);
            let (a, s) = (
                amphora.changes(family, rev, limit, until()),
                sim.changes(family, rev, limit, until()),
            );
            assert_eq!(a, s, "changes {family} after {rev} limit {limit}");
            format!("changes {family} {rev} {limit}: {s:?}")
        }
        5 => {
            let (a, s) = (
                amphora.changed(family, rev, until()),
                sim.changed(family, rev, until()),
            );
            assert_eq!(a, s, "changed {family} after {rev}");
            format!("changed {family} {rev}: {s:?}")
        }
        _ => {
            let (a, s) = (
                amphora.compact_step(rev, until()),
                sim.compact_step(rev, until()),
            );
            assert_eq!(a, s, "compact_step {rev}");
            format!("compact_step {rev}: {s:?}")
        }
    }
}

/// CP-T1.5: the simulator answers as Amphora does. Seeded histories of commits (conditions met
/// and failed, puts, deletes, a key twice), reads at snapshots and at the head, scans, change
/// pages, readiness checks and compaction steps go to both; every answer is equal. The seeds are
/// `AMPHORA_RECORDS_SEEDS` (default 16), each `AMPHORA_RECORDS_STEPS` requests (default 400).
#[test]
fn the_simulator_answers_as_amphora_does() {
    let seeds: u64 = std::env::var("AMPHORA_RECORDS_SEEDS").map_or(16, |v| v.parse().unwrap());
    let steps: u64 = std::env::var("AMPHORA_RECORDS_STEPS").map_or(400, |v| v.parse().unwrap());
    let db = Database::in_memory(until()).unwrap();
    let mut kinds = std::collections::BTreeMap::<String, u64>::new();
    for seed in 0..seeds {
        let amphora = db.namespace(&format!("seed-{seed}")).unwrap();
        let sim = SimRecords::new(SimLimits {
            rows: TX_MAX,
            versions: TX_MAX * TX_MAX,
        });
        let mut c = Choices(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
        for _ in 0..steps {
            let said = step(&mut c, &amphora, &sim);
            let kind = said.split([' ', ':']).next().unwrap_or("").to_owned();
            let refused = said.contains("Err(");
            *kinds
                .entry(format!("{kind}{}", if refused { " refused" } else { "" }))
                .or_default() += 1;
        }
        let head: Rev = sim.head(until()).unwrap();
        assert_eq!(amphora.head(until()).unwrap(), head);
    }
    // Not vacuous: every kind of request was made, answered and refused.
    for kind in [
        "commit",
        "get",
        "scan",
        "changes",
        "changed",
        "compact_step",
    ] {
        assert!(
            kinds.get(kind).copied().unwrap_or(0) > 0,
            "no {kind} answered: {kinds:?}"
        );
    }
    for kind in ["commit refused", "get refused", "changes refused"] {
        assert!(
            kinds.get(kind).copied().unwrap_or(0) > 0,
            "no {kind}: {kinds:?}"
        );
    }
}

/// A group of `voters` voters on this host (CP-T1.8, G12 step 2: one code path at 1 voter and 3),
/// each a replicated database of its own whose raft log is its write-ahead log, their records
/// relayed in-process until the group is dropped.
struct Group {
    databases: Vec<Database>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    relay: Option<std::thread::JoinHandle<()>>,
    _dir: tempfile::TempDir,
}

impl Group {
    fn new(voters: u64) -> Self {
        use std::sync::atomic::{AtomicBool, Ordering};
        let dir = tempfile::tempdir().unwrap();
        let databases: Vec<Database> = (1..=voters)
            .map(|node_id| {
                Database::open_replicated(
                    &dir.path().join(format!("state-{node_id}.db")),
                    &dir.path().join(format!("raft-{node_id}.log")),
                    amphora_api::replication::ReplicaConfig {
                        node_id,
                        voters: (1..=voters).collect(),
                        learners: Vec::new(),
                        bootstrap: None,
                    },
                    until(),
                )
                .unwrap()
            })
            .collect();
        let handles: Vec<_> = databases.iter().map(|d| d.replica().unwrap()).collect();
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let relay = {
            let stop = std::sync::Arc::clone(&stop);
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let mut moved = 0;
                    for (from, handle) in (1u64..).zip(&handles) {
                        for record in handle.drain(1024).unwrap_or_default() {
                            let to = usize::try_from(record.to - 1).unwrap();
                            // A record a member can't take now is lost, as on a network: Raft
                            // sends it again.
                            let _ = handles[to].receive(from, record.bytes, until());
                            moved += 1;
                        }
                    }
                    if moved == 0 {
                        std::thread::park_timeout(Duration::from_millis(1));
                    }
                }
            })
        };
        Self {
            databases,
            stop,
            relay: Some(relay),
            _dir: dir,
        }
    }

    /// The member that leads, once one does and its group's seed is in.
    fn leader(&self) -> &Database {
        let deadline = until();
        loop {
            for database in &self.databases {
                let status = database.replica_status(deadline).unwrap().unwrap();
                assert!(status.fenced.is_none(), "fenced: {:?}", status.fenced);
                if status.leader == status.node_id && !status.bootstrapping {
                    return database;
                }
            }
            assert!(Instant::now() < deadline, "no member led");
            std::thread::park_timeout(Duration::from_millis(10));
        }
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(relay) = self.relay.take() {
            let _ = relay.join();
        }
    }
}

/// What the Records suite and seeded histories said on a group of `voters` voters, every answer
/// checked against the simulator's as it came: the cases passed and failed, then each request
/// and its answer. The seeds are `AMPHORA_GROUP_SEEDS` (default 4), each `AMPHORA_GROUP_STEPS`
/// requests (default 200).
fn said_at(voters: u64) -> Vec<String> {
    let seeds: u64 = std::env::var("AMPHORA_GROUP_SEEDS").map_or(4, |v| v.parse().unwrap());
    let steps: u64 = std::env::var("AMPHORA_GROUP_STEPS").map_or(200, |v| v.parse().unwrap());
    let group = Group::new(voters);
    let db = group.leader();
    let mut n = 0;
    let report = run(
        &cases(),
        || {
            n += 1;
            db.namespace(&format!("case-{n}"))
        },
        HANG,
    );
    let mut said: Vec<String> = report
        .passed
        .iter()
        .map(|c| format!("passed {c}"))
        .collect();
    said.extend(
        report
            .failed
            .iter()
            .map(|(c, f)| format!("failed {c}: {f:?}")),
    );
    for seed in 0..seeds {
        let amphora = db.namespace(&format!("seed-{seed}")).unwrap();
        let sim = SimRecords::new(SimLimits {
            rows: TX_MAX,
            versions: TX_MAX * TX_MAX,
        });
        let mut c = Choices(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
        for _ in 0..steps {
            said.push(step(&mut c, &amphora, &sim));
        }
        assert_eq!(amphora.head(until()).unwrap(), sim.head(until()).unwrap());
    }
    said
}

/// AC-T1.8.L2, T-T1.8.L2 (CP-T1.8, G12 step 2: the store's tests at 1 voter and 3): one harness,
/// the group's size its only parameter. Do: the Records suite and seeded histories on a group of
/// one voter, then on a group of three, each at its leader. Expect: every case passes at both,
/// every answer equal to the simulator's, and the two transcripts identical.
#[test]
fn the_records_suite_answers_alike_at_one_voter_and_three() {
    let one = said_at(1);
    assert!(
        one.iter().all(|line| !line.starts_with("failed ")),
        "at one voter: {one:?}"
    );
    assert!(one.len() > cases().len(), "no history ran");
    let three = said_at(3);
    assert_eq!(one.len(), three.len());
    for (a, b) in one.iter().zip(&three) {
        assert_eq!(a, b, "one voter and three answered differently");
    }
}
