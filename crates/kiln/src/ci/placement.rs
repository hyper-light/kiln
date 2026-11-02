//! kiln's headroom rule ([`kiln_ci::sched::place`], docs/runners.md §5) behind kiln-seams'
//! `Placement`, held to the seam's conformance suite (docs/seams.md, PLACEMENT-1 to 7): its inputs
//! and outputs are Placement's, so Hex's placement (CP-T7.1) can take its place. A node's use here
//! is what's placed on it, as the seam counts it (no samples), and the target is all of a node.

use std::collections::BTreeMap;

use futures::future::BoxFuture;
use kiln_ci::sched::state::Headroom;
use kiln_seams::placement::{Ask, Node, NodeUse, Placed, Placement, PlacementFixture, Refusal, Resources, cases, conformance_nodes};
use parking_lot::Mutex;

/// The rule as a `Placement`: its nodes, and what's placed where.
struct Headroomed {
    nodes: Vec<Node>,
    placed: Mutex<BTreeMap<String, (String, Resources)>>,
}

/// What's placed on a node.
fn used(placed: &BTreeMap<String, (String, Resources)>, node: &str) -> Resources {
    placed.values().filter(|(n, _)| n == node).fold(Resources::default(), |a, (_, r)| Resources { cpu_milli: a.cpu_milli.saturating_add(r.cpu_milli), memory_bytes: a.memory_bytes.saturating_add(r.memory_bytes) })
}

/// The seam's resources as the engine counts them.
fn engine(r: Resources) -> kiln_ci::sched::state::Resources {
    kiln_ci::sched::state::Resources { cpu: r.cpu_milli as f64 / 1000.0, mem: r.memory_bytes, ..Default::default() }
}

impl Placement for Headroomed {
    fn place(&self, ask: Ask) -> BoxFuture<'_, Result<Placed, Refusal>> {
        let out = (|| {
            if ask.id.is_empty() || ask.id.len() > 256 {
                return Err(Refusal::Invalid { what: "an ask's id", why: format!("1 to 256 bytes ({})", ask.id.len()) });
            }
            let mut placed = self.placed.lock();
            if let Some((node, _)) = placed.get(&ask.id) {
                return Ok(Placed { id: ask.id.clone(), node: node.clone() });
            }
            let labelled: Vec<&Node> = self.nodes.iter().filter(|n| ask.selector.iter().all(|(k, v)| n.labels.get(k) == Some(v))).collect();
            if labelled.is_empty() {
                let wanted: Vec<String> = ask.selector.iter().map(|(k, v)| format!("{k}={v}")).collect();
                return Err(Refusal::Unplaceable { id: ask.id.clone(), why: format!("no node has the labels {}", wanted.join(", ")) });
            }
            // Each node's headroom: what it has, less what's placed on it.
            let nodes: Vec<kiln_ci::sched::place::Node<'_>> = labelled
                .iter()
                .map(|n| {
                    let u = used(&placed, &n.name);
                    let free = Resources { cpu_milli: n.capacity.cpu_milli.saturating_sub(u.cpu_milli), memory_bytes: n.capacity.memory_bytes.saturating_sub(u.memory_bytes) };
                    let headroom = Headroom { cpus: engine(free).cpu, mem: free.memory_bytes, io_full: None, of: engine(n.capacity) };
                    kiln_ci::sched::place::Node { name: &n.name, headroom: Some(headroom) }
                })
                .collect();
            match kiln_ci::sched::place::place(&engine(ask.needs), &nodes, 1.0) {
                Ok(node) => {
                    let node = node.to_owned();
                    placed.insert(ask.id.clone(), (node.clone(), ask.needs));
                    Ok(Placed { id: ask.id, node })
                }
                Err(no) => Err(Refusal::Unplaceable { id: ask.id, why: no.why }),
            }
        })();
        Box::pin(async move { out })
    }

    fn release<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<(), Refusal>> {
        self.placed.lock().remove(id);
        Box::pin(async { Ok(()) })
    }

    fn nodes(&self) -> BoxFuture<'_, Result<Vec<NodeUse>, Refusal>> {
        let placed = self.placed.lock();
        let out = self.nodes.iter().map(|n| NodeUse { name: n.name.clone(), capacity: n.capacity, used: used(&placed, &n.name) }).collect();
        Box::pin(async move { Ok(out) })
    }
}

struct Fixture(Headroomed);

impl PlacementFixture for Fixture {
    fn placement(&self) -> &dyn Placement {
        &self.0
    }
}

/// W05 step 5: kiln's headroom rule passes the `Placement` suite, every clause, on the seam's two
/// nodes: never over-committed, refused with what's short, releasing exactly what was placed.
#[test]
fn the_headroom_rule_passes_the_placement_suite() -> Result<(), String> {
    let make = || async { Fixture(Headroomed { nodes: conformance_nodes(), placed: Mutex::new(BTreeMap::new()) }) };
    futures::executor::block_on(kiln_seams::conformance::run("Placement (kiln's headroom rule)", &cases::<Fixture>(), make)).into_result().map(|_| ())
}
