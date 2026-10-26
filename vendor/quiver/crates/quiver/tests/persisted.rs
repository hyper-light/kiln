//! The persisted class (kiln's CP-T2.2, step 4): quiver-api's conformance suite passes for it on a
//! node that is a cluster of one, and on three nodes in one process, each topic's part on all
//! three.

#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects,
        clippy::disallowed_macros
    )
)]

mod cluster;

use std::time::Duration;

use cluster::Cluster;
use quiver_api::conformance::{TopicsFixture, cases, run_within};
use quiver_api::{Class, Topics};

/// A case's cluster: the suite reaches it through its first node.
struct Fixture(Cluster);

impl TopicsFixture for Fixture {
    fn topics(&self) -> &dyn Topics {
        self.0.node(1)
    }

    fn serves(&self, class: Class) -> bool {
        matches!(class, Class::Ephemeral | Class::Persisted)
    }

    fn class(&self) -> Class {
        Class::Persisted
    }
}

/// A case that never ends fails here: a wedge, not slowness (each case's cluster elects and
/// commits in milliseconds).
const CASE_DEADLINE: Duration = Duration::from_secs(60);

fn suite(nodes: u64) -> Result<usize, String> {
    let report = futures::executor::block_on(run_within(
        "Topics",
        &cases::<Fixture>(),
        || async move { Fixture(Cluster::start(nodes)) },
        || cluster::after(CASE_DEADLINE),
    ));
    report.into_result()
}

/// CP-T2.2 step 4: do the suite's every clause against the persisted class on a node that is a
/// cluster of one (kiln on one server: its events on a group of one, durable on its disk), expect
/// every one to pass.
#[test]
fn cp_t2_2_the_persisted_class_passes_the_topics_suite_on_one_node() -> Result<(), String> {
    assert_eq!(suite(1)?, cases::<Fixture>().len());
    Ok(())
}

/// CP-T2.2 step 4: do the suite's every clause against the persisted class on three nodes in one
/// process, through the first, expect every one to pass.
#[test]
fn cp_t2_2_the_persisted_class_passes_the_topics_suite_on_three_nodes() -> Result<(), String> {
    assert_eq!(suite(3)?, cases::<Fixture>().len());
    Ok(())
}

/// Every node's publishes are its own: do publish from all three nodes at once into one topic,
/// expect each publish answered with the offset its own payload took, every offset once, on every
/// node alike. (Every node's publisher once took the same session in a fresh cluster, and a node
/// answered its publish with another node's offset, and never sent its own.)
#[test]
fn cp_t2_2_each_nodes_publishes_are_answered_at_their_own_offsets() {
    let c = Cluster::start(3);
    let topic = quiver_api::Topic {
        class: Class::Persisted,
        retain_messages: 10_000,
        retain_bytes: 1 << 24,
    };
    futures::executor::block_on(c.node(1).create("shared", topic)).unwrap();
    let threads: Vec<_> = (1..=3)
        .map(|id| {
            let node = c.node(id).clone();
            std::thread::spawn(move || {
                (0..30)
                    .map(|i| {
                        let payload = format!("n{id}-{i}");
                        let offset = futures::executor::block_on(node.publish(
                            "shared",
                            None,
                            payload.as_bytes(),
                        ))
                        .unwrap();
                        (offset, payload)
                    })
                    .collect::<Vec<_>>()
            })
        })
        .collect();
    let mut answered: Vec<(u64, String)> = threads
        .into_iter()
        .flat_map(|t| t.join().unwrap())
        .collect();
    answered.sort();
    assert_eq!(
        answered.iter().map(|(o, _)| *o).collect::<Vec<_>>(),
        (0..90).collect::<Vec<u64>>(),
        "every offset once"
    );
    for id in 1..=3 {
        let mut read = Vec::new();
        let began = std::time::Instant::now();
        while read.len() < 90 {
            assert!(
                began.elapsed() < CASE_DEADLINE,
                "node {id} holds {} of 90",
                read.len()
            );
            let from = u64::try_from(read.len()).unwrap();
            let batch = futures::executor::block_on(c.node(id).read("shared", from, 1000)).unwrap();
            read.extend(
                batch
                    .messages
                    .into_iter()
                    .map(|m| (m.offset, String::from_utf8(m.payload).unwrap())),
            );
        }
        assert_eq!(read, answered, "node {id}'s log");
    }
}

/// A cluster of one may name port 0 (kiln's server on one machine, beside the many servers its
/// tests start at once): do start one so, publish, restart it (at another port the system picks)
/// and publish again, expect each publish answered at its offset and both read back; and a cluster
/// of three naming a port 0 refused, typed, since its peers dial every node where it says.
#[test]
fn cp_t2_2_a_cluster_of_one_listens_where_the_system_picks() {
    let mut c = Cluster::start_at(vec![(1, std::net::SocketAddr::from(([127, 0, 0, 1], 0)))]);
    let topic = quiver_api::Topic {
        class: Class::Persisted,
        retain_messages: 100,
        retain_bytes: 1 << 20,
    };
    futures::executor::block_on(c.node(1).create("events", topic)).unwrap();
    let first = futures::executor::block_on(c.node(1).publish("events", None, b"a")).unwrap();
    c.kill(1);
    c.restart(1);
    let second = futures::executor::block_on(c.node(1).publish("events", None, b"b")).unwrap();
    assert_eq!((first, second), (0, 1));
    let read = futures::executor::block_on(c.node(1).read("events", 0, 10)).unwrap();
    let held: Vec<(u64, Vec<u8>)> = read
        .messages
        .into_iter()
        .map(|m| (m.offset, m.payload))
        .collect();
    assert_eq!(held, vec![(0, b"a".to_vec()), (1, b"b".to_vec())]);

    let mut three = c.config(1);
    three.nodes = (1..=3)
        .map(|i| (i, std::net::SocketAddr::from(([127, 0, 0, 1], 0))))
        .collect();
    three.data = c.dir.join("three");
    let refused = quiver::Node::with_persisted(cluster::NODE, three).err();
    assert!(
        matches!(&refused, Some(quiver_api::Refusal::Invalid { why, .. }) if why.contains("port is 0")),
        "{refused:?}"
    );
}
