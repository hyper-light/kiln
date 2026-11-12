//! AC-T7.4.1: control population ceilings from the minimum supported finite reservation.
//!
//! Placement validates explicit CPU/memory caps and reservations covering them. Therefore each
//! physical box consumes at least MIN_CPU_MILLI and MIN_MEMORY of its node's admission capacity.
//! These counts bound control records; they neither reserve memory nor authorize creation. Unknown
//! facts still admit one pending owner intent per configured endpoint, subject to receiver admission.
//! Recovery must retain its persisted read ceiling/held rows before applying current admission, and
//! the shell must update admission when facts change. This helper does not shrink or discard state.

use hex_api::{MIN_CPU_MILLI, MIN_MEMORY, Refusal};
use hex_sim::model::{Classes, NodeId, NodeSpec};
use hex_sim::peer::Budget;

fn invalid(why: &'static str) -> Refusal {
    Refusal::Invalid {
        what: "placement budget",
        why: why.into(),
    }
}

fn population(node: &NodeSpec) -> Result<usize, Refusal> {
    if !node.classes.meets(Classes::ALL) {
        return Ok(0);
    }
    let cpu = node
        .capacity
        .cpu_milli
        .checked_div(u64::from(MIN_CPU_MILLI))
        .ok_or_else(|| invalid("minimum CPU reservation is zero"))?;
    let memory = node
        .capacity
        .mem_bytes
        .checked_div(MIN_MEMORY)
        .ok_or_else(|| invalid("minimum memory reservation is zero"))?;
    usize::try_from(cpu.min(memory)).map_err(|_| invalid("node population exceeds usize"))
}

fn sum(left: usize, right: usize) -> Result<usize, Refusal> {
    left.checked_add(right)
        .ok_or_else(|| invalid("population or record count exceeds usize"))
}

fn checked_budget(owned: usize, ran: usize) -> Result<Budget, Refusal> {
    // One owner row per intent, one runner row per physical box, and the existing epoch row.
    let _records = sum(sum(owned, ran)?, 1)?;
    Ok(Budget { owned, ran })
}

/// A node's runner ceiling and its cluster-wide owner ceiling, with no picked box-count cap.
pub fn runtime_budget(id: NodeId, nodes: &[NodeSpec]) -> Result<Budget, Refusal> {
    // The deterministic duplicate check stores exactly one ID per supplied node, fallibly reserved.
    let mut ids = Vec::new();
    ids.try_reserve_exact(nodes.len())
        .map_err(|_| Refusal::Exhausted {
            what: "placement node IDs",
            bound: nodes.len(),
        })?;
    ids.extend(nodes.iter().map(|node| node.id));
    ids.sort_unstable();
    if ids.windows(2).any(|pair| pair.first() == pair.last()) {
        return Err(invalid("duplicate node ID"));
    }
    let local = nodes
        .iter()
        .find(|node| node.id == id)
        .ok_or_else(|| invalid("local node ID is absent"))?;
    let ran = population(local)?;
    let owned = nodes
        .iter()
        .try_fold(0, |total, node| sum(total, population(node)?))?
        .max(nodes.len());
    checked_budget(owned, ran)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hex_sim::model::{Class, Resources};

    fn node(id: u32, cpu: u64, memory: u64, classes: Classes) -> NodeSpec {
        let mut node = NodeSpec::new(
            NodeId(id),
            Resources {
                cpu_milli: cpu,
                mem_bytes: memory,
                ..Resources::default()
            },
        );
        node.classes = classes;
        node
    }

    fn counts(id: u32, nodes: &[NodeSpec]) -> (usize, usize) {
        let budget = runtime_budget(NodeId(id), nodes).unwrap();
        (budget.owned, budget.ran)
    }

    /// AC-T7.4.1: fractional minimum reservations cannot create an extra control slot.
    #[test]
    fn cp_t7_4_fractional_cpu_and_memory_populations_round_down_independently() {
        let cpu = u64::from(MIN_CPU_MILLI);
        let classes = Classes::NONE.with(Class::Oci);
        let cpu_limited = node(0, 2 * cpu + cpu - 1, 5 * MIN_MEMORY, classes);
        assert_eq!(counts(0, &[cpu_limited]), (2, 2));
        let memory_limited = node(0, 5 * cpu, 3 * MIN_MEMORY - 1, classes);
        assert_eq!(counts(0, &[memory_limited]), (2, 2));
        for too_small in [
            node(0, cpu - 1, 5 * MIN_MEMORY, classes),
            node(0, 5 * cpu, MIN_MEMORY - 1, classes),
        ] {
            assert_eq!(counts(0, &[too_small]), (1, 0));
        }
    }

    /// AC-T7.4.1: only a supported advertised class gives physical population; class count is no multiplier.
    #[test]
    fn cp_t7_4_classes_filter_capacity_without_multiplying_shared_host_room() {
        let cpu = u64::from(MIN_CPU_MILLI);
        let unavailable = node(0, 7 * cpu, 7 * MIN_MEMORY, Classes::NONE);
        let available = node(1, 3 * cpu, 3 * MIN_MEMORY, Classes::ALL);
        assert_eq!(counts(0, &[unavailable.clone(), available.clone()]), (3, 0));
        assert_eq!(counts(1, &[unavailable, available]), (3, 3));
        let unknown_only: Classes = serde_json::from_str("128").unwrap();
        assert_eq!(
            counts(0, &[node(0, 7 * cpu, 7 * MIN_MEMORY, unknown_only)]),
            (1, 0)
        );
    }

    /// AC-T7.4.1: absent facts preserve pending intents, without granting a receiver any physical capacity.
    #[test]
    fn cp_t7_4_unknown_facts_keep_one_pending_intent_per_configured_endpoint() {
        let nodes = [
            node(0, 0, 0, Classes::NONE),
            node(1, 0, 0, Classes::NONE),
            node(2, 0, 0, Classes::NONE),
        ];
        assert_eq!(counts(0, &nodes), (nodes.len(), 0));
    }

    /// AC-T7.4.1: an owner may place across every endpoint; its runner ceiling remains its own node's.
    #[test]
    fn cp_t7_4_owner_population_sums_nodes_and_runner_population_is_local() {
        let cpu = u64::from(MIN_CPU_MILLI);
        let nodes = [
            node(0, 5 * cpu, 9 * MIN_MEMORY, Classes::ALL),
            node(
                1,
                8 * cpu,
                2 * MIN_MEMORY,
                Classes::NONE.with(Class::Shards),
            ),
        ];
        assert_eq!(counts(0, &nodes), (7, 5));
        assert_eq!(counts(1, &nodes), (7, 2));
    }

    /// AC-T7.4.1: capacities are counts only; even enormous ceilings allocate no per-box state here.
    #[test]
    fn cp_t7_4_large_capacity_is_not_clipped_to_an_unrelated_default_box_count() {
        let node = node(0, u64::MAX, u64::MAX, Classes::ALL);
        let ceiling = (u64::MAX / u64::from(MIN_CPU_MILLI)).min(u64::MAX / MIN_MEMORY);
        match usize::try_from(ceiling) {
            Ok(ceiling)
                if ceiling
                    .checked_add(ceiling)
                    .and_then(|n| n.checked_add(1))
                    .is_some() =>
            {
                assert_eq!(counts(0, &[node]), (ceiling, ceiling));
            }
            _ => assert!(runtime_budget(NodeId(0), &[node]).is_err()),
        }
    }

    /// AC-T7.4.1: ambiguous endpoint tables cannot silently choose a local node or double-count capacity.
    #[test]
    fn cp_t7_4_unknown_local_and_duplicate_node_ids_are_refused() {
        let local = node(0, 0, 0, Classes::NONE);
        assert!(matches!(
            runtime_budget(NodeId(0), &[]),
            Err(Refusal::Invalid { .. })
        ));
        assert!(matches!(
            runtime_budget(NodeId(1), std::slice::from_ref(&local)),
            Err(Refusal::Invalid { .. })
        ));
        assert!(matches!(
            runtime_budget(NodeId(0), &[local.clone(), local]),
            Err(Refusal::Invalid { .. })
        ));
    }

    /// AC-T7.4.1: test representational overflow without allocating a million-node capacity table.
    #[test]
    fn cp_t7_4_population_sum_and_combined_epoch_record_count_overflow_are_refused() {
        assert!(sum(usize::MAX, 1).is_err());
        assert!(checked_budget(usize::MAX, 0).is_err());
        assert!(checked_budget(usize::MAX - 1, 1).is_err());
        let exact = checked_budget(usize::MAX - 1, 0).unwrap();
        assert_eq!((exact.owned, exact.ran), (usize::MAX - 1, 0));
    }
}
