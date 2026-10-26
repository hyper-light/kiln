//! Quiver's node passes `quiver-api`'s conformance suite (QUIVER-1 to QUIVER-19), the clauses the
//! simulator passes, serving the ephemeral class and refusing the others typed (kiln's CP-T2.1,
//! AC-T2.1.1).

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

use quiver::{Node, NodeConfig};
use quiver_api::conformance::{TopicsFixture, cases, run};
use quiver_api::{Class, Topics};

struct NodeFixture(Node);

impl TopicsFixture for NodeFixture {
    fn topics(&self) -> &dyn Topics {
        &self.0
    }

    fn serves(&self, class: Class) -> bool {
        class == Class::Ephemeral
    }
}

fn node() -> NodeFixture {
    NodeFixture(
        Node::new(NodeConfig {
            memory: 64 << 20,
            topics: 100,
            subscriptions: 100,
        })
        .unwrap(),
    )
}

/// AC-T2.1.1: do the suite's every clause against a node, expect every one to pass.
#[test]
fn the_node_passes_the_topics_suite() -> Result<(), String> {
    let passed =
        futures::executor::block_on(run("Topics", &cases::<NodeFixture>(), || async { node() }))
            .into_result()?;
    assert_eq!(passed, cases::<NodeFixture>().len());
    Ok(())
}
