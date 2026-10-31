//! U15, CP-T1.8: resolved SQL output, durable apply fencing and snapshot behavior.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros
)]

use amphora::replication::Machine;
use hyper_durable::{EntryRef, Point, StateMachine};
use hyper_raft::proto::{ConfState, EntryType};

fn configuration() -> ConfState {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::ERROR)
        .try_init();
    ConfState {
        voters: vec![1, 2, 3],
        ..ConfState::default()
    }
}
fn value(machine: &mut Machine) -> Vec<u8> {
    machine
        .read(|c| {
            Ok(
                c.query_row("SELECT value FROM items WHERE id = 1", [], |r| {
                    r.get::<_, Vec<u8>>(0usize)
                })?,
            )
        })
        .unwrap()
}
fn apply(machine: &mut Machine, index: u64, data: &[u8]) -> Vec<amphora::replication::Applied> {
    let mut answers = Vec::new();
    machine
        .apply(
            &EntryRef {
                index,
                term: 1,
                kind: EntryType::EntryNormal,
                context: b"request",
                data,
            },
            &mut answers,
        )
        .unwrap();
    answers
}

#[test]
fn prepared_sql_is_not_visible_until_commit_and_random_defaults_replicate_as_chosen() {
    let dir = tempfile::tempdir().unwrap();
    let mut leader = Machine::open(&dir.path().join("leader.db"), configuration()).unwrap();
    let mut follower = Machine::open(&dir.path().join("follower.db"), configuration()).unwrap();
    let (prepared, chosen) = leader.prepare(|c| {
        c.execute_batch("CREATE TABLE items(id INTEGER PRIMARY KEY, value BLOB DEFAULT(randomblob(32))); INSERT INTO items(id) VALUES (1)")?;
        Ok(c.query_row("SELECT value FROM items WHERE id = 1", [], |r| r.get::<_, Vec<u8>>(0usize))?)
    }).unwrap();
    let bytes = prepared.encode().unwrap();
    assert!(
        leader
            .read(|c| Ok(c.query_row("SELECT value FROM items", [], |r| r
                .get::<_, Vec<u8>>(0usize))?))
            .is_err()
    );
    assert!(apply(&mut leader, 1, &bytes)[0].result.is_ok());
    assert!(apply(&mut follower, 1, &bytes)[0].result.is_ok());
    assert_eq!(value(&mut leader), chosen);
    assert_eq!(value(&mut follower), chosen);
    drop(leader);
    let mut reopened = Machine::open(&dir.path().join("leader.db"), configuration()).unwrap();
    assert_eq!(reopened.durable(), Point { index: 1, term: 1 });
    assert_eq!(value(&mut reopened), chosen);
}

#[test]
fn stale_preparations_are_refused_and_snapshots_restore_rows_and_membership_together() {
    let dir = tempfile::tempdir().unwrap();
    let mut source = Machine::open(&dir.path().join("source.db"), configuration()).unwrap();
    let (first, _) = source.prepare(|c| { c.execute_batch("CREATE TABLE items(id INTEGER PRIMARY KEY, value BLOB); INSERT INTO items VALUES (1, x'01')")?; Ok(()) }).unwrap();
    let (stale, _) = source
        .prepare(|c| {
            c.execute_batch("CREATE TABLE discarded(x)")?;
            Ok(())
        })
        .unwrap();
    assert!(
        apply(&mut source, 1, &first.encode().unwrap())[0]
            .result
            .is_ok()
    );
    assert!(
        apply(&mut source, 2, &stale.encode().unwrap())[0]
            .result
            .is_err()
    );
    assert_eq!(source.durable(), Point { index: 2, term: 1 });
    assert_eq!(value(&mut source), vec![1]);
    let mut image = Vec::new();
    let (at, membership) = source.image(&mut image).unwrap();
    let pages: Vec<(u32, Vec<u8>)> = postcard::from_bytes(&image).unwrap();
    assert!(
        pages[0].1.windows(b"items".len()).any(|w| w == b"items"),
        "snapshot must read the current catalog page"
    );
    let path = dir.path().join("restored.db");
    let mut restored = Machine::open(&path, configuration()).unwrap();
    restored.install(&image, at, &membership).unwrap();
    assert_eq!(value(&mut restored), vec![1], "installed image");
    drop(restored);
    let mut reopened = Machine::open(&path, ConfState::default()).unwrap();
    assert_eq!(reopened.durable(), at);
    assert_eq!(reopened.configuration(), &membership);
    assert_eq!(value(&mut reopened), vec![1], "reopened image");
}

// Exercise the existing durable shell and real device log, with transport delivery under this
// test's control. Each turn waits for the actual oldest disk write, never a timer guess.
type Replica = hyper_durable::Replica<
    hyper_durable::GroupStore<hyper_block::file::DeviceFile>,
    Machine,
    hyper_durable::Bytes,
>;
struct Node {
    now: u64,
    replica: Option<Replica>,
    log: hyper_log::Log<hyper_block::file::DeviceFile>,
}
fn settings(id: u64) -> hyper_durable::Settings {
    let limits = hyper_raft::Limits::derive(hyper_raft::Stated {
        message: 64 << 10,
        members: 3,
        memory: 64 << 20,
        depth: 1,
    })
    .unwrap();
    hyper_durable::Settings {
        core: hyper_raft::Config {
            check_quorum: true,
            pre_vote: true,
            ..hyper_raft::Config::new(id, limits)
        },
        // CP-T2.2: preserve flush-proved group durability during API reconciliation.
        disk: hyper_durable::Disk::Flushed,
        elections: hyper_raft::Elections::Suspicion,
        quiet: std::time::Duration::from_millis(50),
    }
}
fn node(dir: &std::path::Path, id: u64) -> Node {
    let file = hyper_block::file::DeviceFile::open(
        &dir.join(format!("raft-{id}.log")),
        true,
        hyper_block::file::CachingRequest::Buffered,
        hyper_block::buf::Alignment::new(4096).unwrap(),
    )
    .unwrap();
    let log = hyper_log::Log::create(
        file,
        hyper_log::Config {
            segment_bytes: 1 << 20,
            max_segments: 8,
            max_groups: 1,
            group_entries: 1024,
            group_bytes: 4 << 20,
            group_cache: 1 << 20,
            queue_submissions: 16,
            waits: hyper_log::Waits::Never,
        },
        u128::from(id),
    )
    .unwrap();
    let group = hyper_durable::GroupStore::claim(&log, 1).unwrap();
    let machine = Machine::open(&dir.join(format!("state-{id}.db")), configuration()).unwrap();
    let replica = hyper_durable::Replica::open(
        &settings(id),
        group,
        machine,
        hyper_durable::Bytes::new(64 << 20),
    )
    .unwrap();
    Node {
        now: 0,
        replica: Some(replica),
        log,
    }
}
fn turns(
    nodes: &mut [Node],
    rounds: u64,
    connected: bool,
) -> Vec<(u64, amphora::replication::Applied)> {
    let mut answers = Vec::new();
    for _ in 0..rounds {
        let mut messages = Vec::new();
        for node in nodes.iter_mut() {
            let Some(replica) = &mut node.replica else {
                continue;
            };
            node.now += 1_000_000;
            replica.log_mut().wait();
            let mut output = hyper_durable::Output::default();
            replica
                .drive(node.now, std::task::Waker::noop(), &mut output)
                .unwrap();
            messages.extend(output.messages);
            answers.extend(output.answers.into_iter().map(|a| (replica.id(), a)));
        }
        for message in messages {
            if !connected && (message.from == 1 || message.to == 1) {
                continue;
            }
            if let Some(replica) = nodes
                .get_mut(usize::try_from(message.to - 1).unwrap())
                .and_then(|n| n.replica.as_mut())
            {
                replica.step(message).unwrap();
            }
        }
    }
    answers
}

#[test]
fn real_raft_log_requires_a_quorum_and_reopens_the_applied_rows() {
    let dir = tempfile::tempdir().unwrap();
    let mut nodes: Vec<_> = (1..=3).map(|id| node(dir.path(), id)).collect();
    nodes[0].replica.as_mut().unwrap().campaign().unwrap();
    for _ in 0..100 {
        turns(&mut nodes, 1, true);
        if nodes
            .iter()
            .all(|n| n.replica.as_ref().unwrap().applied().index >= 1)
        {
            break;
        }
    }
    assert!(nodes[0].replica.as_ref().unwrap().is_leader());
    assert!(
        nodes
            .iter()
            .all(|n| n.replica.as_ref().unwrap().applied().index >= 1)
    );
    let (prepared, chosen) = nodes[0].replica.as_mut().unwrap().machine_mut().prepare(|c| {
        c.execute_batch("CREATE TABLE items(id INTEGER PRIMARY KEY, value BLOB DEFAULT(randomblob(32))); INSERT INTO items(id) VALUES (1)")?;
        Ok(c.query_row("SELECT value FROM items WHERE id = 1", [], |r| r.get::<_, Vec<u8>>(0usize))?)
    }).unwrap();
    nodes[0]
        .replica
        .as_mut()
        .unwrap()
        .propose(b"client".to_vec(), prepared.encode().unwrap())
        .unwrap();
    assert!(
        turns(&mut nodes, 20, false).is_empty(),
        "a minority must not acknowledge a write"
    );
    nodes[0].replica.as_mut().unwrap().beat().unwrap();
    for _ in 0..100 {
        turns(&mut nodes, 1, true);
        if nodes
            .iter()
            .all(|n| n.replica.as_ref().unwrap().applied().index >= 2)
        {
            break;
        }
    }
    assert!(
        nodes
            .iter()
            .all(|n| n.replica.as_ref().unwrap().applied().index >= 2)
    );
    for node in &mut nodes {
        assert_eq!(value(node.replica.as_mut().unwrap().machine_mut()), chosen);
    }
    let mut first = nodes.remove(0);
    let at = first.replica.as_ref().unwrap().applied();
    drop(first.replica.take());
    let group = hyper_durable::GroupStore::claim(&first.log, 1).unwrap();
    let machine = Machine::open(&dir.path().join("state-1.db"), ConfState::default()).unwrap();
    let mut reopened = hyper_durable::Replica::open(
        &settings(1),
        group,
        machine,
        hyper_durable::Bytes::new(64 << 20),
    )
    .unwrap();
    assert_eq!(reopened.applied(), at);
    assert_eq!(value(reopened.machine_mut()), chosen);
}
