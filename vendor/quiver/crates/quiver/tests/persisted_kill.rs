//! An acknowledged publish survives a kill of any node (kiln's CP-T2.2, step 4), three nodes in one
//! process: the leader, a follower, and the node the publisher publishes through, each killed at
//! every persistence point of a run of publishes, the k-th frame its log writes and flushes for
//! k = 1, 2, … until the run ends before it. A kill here stops the node's host at once and drops
//! everything it holds, as a crash would; `persisted_processes.rs` kills processes with SIGKILL.

#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects,
        clippy::disallowed_macros,
        clippy::disallowed_methods,
        clippy::cast_possible_truncation
    )
)]

mod cluster;

use std::time::{Duration, Instant};

use cluster::Cluster;
use cluster::drill::{Heard, check};
use quiver::Node;
use quiver_api::{Class, Topic, Topics};

const TOPIC: &str = "drill";
/// Publishes in the run a kill lands in.
const RUN: usize = 12;
/// A wait fails only past this: a wedge, not slowness.
const STUCK: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug)]
enum Victim {
    Leader,
    Follower,
    /// The leader, and the node the publisher publishes through.
    PublishersOwn,
}

fn wait_for<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    let began = Instant::now();
    loop {
        if let Some(found) = f() {
            return found;
        }
        assert!(
            began.elapsed() < STUCK,
            "{what} didn't come within {STUCK:?}"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn leader_of(c: &Cluster) -> Option<u64> {
    (1..=3).filter(|id| c.running(*id)).find(|id| {
        c.node(*id)
            .persisted_stats()
            .and_then(|s| s.topic(TOPIC).map(|g| g.leads))
            .unwrap_or(false)
    })
}

fn frames(node: &Node) -> u64 {
    node.persisted_stats().map_or(0, |s| s.frames())
}

/// Every message `node` keeps of the topic, from offset 0.
fn log_of(node: &Node) -> Vec<(u64, String)> {
    let mut out = Vec::new();
    let mut from = 0;
    loop {
        let batch = futures::executor::block_on(node.read(TOPIC, from, 1000)).unwrap();
        if batch.messages.is_empty() {
            return out;
        }
        for m in batch.messages {
            out.push((m.offset, String::from_utf8(m.payload).unwrap()));
        }
        from = batch.next;
    }
}

/// One drill: `victim` killed at its log's `point`-th frame of a run; whether the run ended
/// before the kill came (every later point lands after it too).
fn drill(victim: Victim, point: u64) -> Result<bool, String> {
    let began = Instant::now();
    let phase = |what: &str| eprintln!("{victim:?} {point}: {what} at {:?}", began.elapsed());
    let mut c = Cluster::start(3);
    let topic = Topic {
        class: Class::Persisted,
        retain_messages: 10_000,
        retain_bytes: 1 << 24,
    };
    futures::executor::block_on(c.node(1).create(TOPIC, topic)).unwrap();
    let leader = wait_for("a leader of the topic's group", || leader_of(&c));
    let others: Vec<u64> = (1..=3).filter(|id| *id != leader).collect();
    let (victim_id, through) = match victim {
        Victim::Leader => (leader, others[0]),
        Victim::Follower => (others[0], others[1]),
        Victim::PublishersOwn => (leader, leader),
    };
    let fallback = (1..=3)
        .find(|id| *id != victim_id && *id != through)
        .unwrap();
    phase(&format!(
        "led by {leader}, killing {victim_id}, publishing through {through}"
    ));
    let base = frames(c.node(victim_id));
    let publisher = std::thread::spawn({
        let mut node = c.node(through).clone();
        let fallback = c.node(fallback).clone();
        move || {
            let mut heard = Heard::default();
            for i in 0..RUN {
                let payload = format!("{point}-a{i}");
                match futures::executor::block_on(node.publish(TOPIC, None, payload.as_bytes())) {
                    Ok(offset) => {
                        heard.acked.insert(payload, offset);
                    }
                    Err(_) => {
                        // Its node went: the fate is unknown, and the run goes on through another.
                        heard.unknown.insert(payload);
                        node = fallback.clone();
                    }
                }
            }
            heard
        }
    });
    let mut ended_first = false;
    wait_for("the kill's frame or the run's end", || {
        if frames(c.node(victim_id)) >= base + point {
            return Some(());
        }
        if publisher.is_finished() {
            ended_first = true;
            return Some(());
        }
        None
    });
    c.kill(victim_id);
    phase("killed");
    let mut heard = publisher.join().unwrap();
    phase(&format!(
        "the run ended: {} acknowledged, {} unknown",
        heard.acked.len(),
        heard.unknown.len()
    ));
    c.restart(victim_id);
    phase("restarted");
    // Every node takes publishes again, the one back from the kill among them.
    for i in 0..3 {
        for id in 1..=3 {
            let payload = format!("{point}-b{id}-{i}");
            let offset =
                futures::executor::block_on(c.node(id).publish(TOPIC, None, payload.as_bytes()))
                    .map_err(|e| format!("a publish through node {id} after the kill: {e}"))?;
            heard.acked.insert(payload, offset);
        }
    }
    phase("published after");
    let end = heard.acked.values().max().copied().unwrap_or(0) + 1;
    wait_for("every node to hold every acknowledged publish", || {
        let ends: Vec<u64> = (1..=3)
            .map(|id| futures::executor::block_on(c.node(id).bounds(TOPIC)).map_or(0, |b| b.1))
            .collect();
        (ends.iter().all(|e| *e == ends[0]) && ends[0] >= end).then_some(())
    });
    let logs: Vec<(u64, Vec<(u64, String)>)> = (1..=3).map(|id| (id, log_of(c.node(id)))).collect();
    check(&heard, &logs).map_err(|why| format!("{victim:?} killed at frame {point}: {why}"))?;
    phase("checked");
    Ok(ended_first)
}

fn every_point(victim: Victim) -> Result<(), String> {
    let mut point = 1;
    loop {
        let ended_first = drill(victim, point)?;
        if ended_first {
            assert!(point > 1, "the run ended before the victim's first frame");
            return Ok(());
        }
        point += 1;
    }
}

/// CP-T2.2 step 4: do kill the leader at every frame its log writes in a run of publishes made
/// through a follower, expect nothing acknowledged lost, nothing appended twice, every node alike.
#[test]
fn cp_t2_2_an_acknowledged_publish_survives_the_leaders_kill_at_every_persistence_point()
-> Result<(), String> {
    every_point(Victim::Leader)
}

/// CP-T2.2 step 4: the same, a follower killed.
#[test]
fn cp_t2_2_an_acknowledged_publish_survives_a_followers_kill_at_every_persistence_point()
-> Result<(), String> {
    every_point(Victim::Follower)
}

/// CP-T2.2 step 4: the same, the leader killed with the publisher's own node: what it hadn't
/// acknowledged may be kept or not, never twice.
#[test]
fn cp_t2_2_an_acknowledged_publish_survives_its_own_nodes_kill_at_every_persistence_point()
-> Result<(), String> {
    every_point(Victim::PublishersOwn)
}

/// A node's part port taken while it was down: do kill a follower, hold the port its topic's
/// member kept with a socket of another's, start it again, expect it to bind another, record it,
/// and take publishes like every node (a host that couldn't bind its kept port once ended).
#[test]
fn cp_t2_2_a_node_whose_parts_port_was_taken_while_it_was_down_rebinds_and_catches_up() {
    let mut c = Cluster::start(3);
    let topic = Topic {
        class: Class::Persisted,
        retain_messages: 100,
        retain_bytes: 1 << 20,
    };
    futures::executor::block_on(c.node(1).create(TOPIC, topic)).unwrap();
    let leader = wait_for("a leader", || leader_of(&c));
    let victim = (1..=3).find(|id| *id != leader).unwrap();
    futures::executor::block_on(c.node(victim).publish(TOPIC, None, b"before")).unwrap();
    c.kill(victim);
    let topics = c.dir.join(format!("node-{victim}")).join("topics");
    let part = std::fs::read_dir(&topics)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let kept = u16::from_le_bytes(
        std::fs::read(part.join("port"))
            .unwrap()
            .try_into()
            .unwrap(),
    );
    let _taken = std::net::UdpSocket::bind(("127.0.0.1", kept)).unwrap();
    c.restart(victim);
    for id in 1..=3 {
        let payload = format!("after-{id}");
        futures::executor::block_on(c.node(id).publish(TOPIC, None, payload.as_bytes()))
            .unwrap_or_else(|e| {
                let figures: Vec<_> = (1..=3).map(|n| c.node(n).persisted_stats()).collect();
                panic!("node {id}: {e}; held {kept}; {figures:#?}")
            });
    }
    let now_kept = u16::from_le_bytes(
        std::fs::read(part.join("port"))
            .unwrap()
            .try_into()
            .unwrap(),
    );
    assert_ne!(now_kept, kept, "it kept the port another holds");
    let logs: Vec<Vec<(u64, String)>> = (1..=3)
        .map(|id| {
            wait_for("every node to hold all four", || {
                let log = log_of(c.node(id));
                (log.len() == 4).then_some(log)
            })
        })
        .collect();
    assert!(logs.iter().all(|log| *log == logs[0]), "{logs:?}");
}
