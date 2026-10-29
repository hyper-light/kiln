//! Placement by headroom (docs/ci.md W05-R03, docs/runners.md §5), shaped as kiln-seams'
//! `Placement` (docs/seams.md): what work needs and the nodes in, a node or a refusal saying why
//! out. The engine and the v1 dispatcher place through it ([`place`], [`fits`], [`before`]), and
//! reach ranks by the same [`Headroom::fullness`], so Hex's placement (CP-T7.1) can take the
//! rule's place without any of them changing.

use std::cmp::Ordering;

use super::state::{Headroom, Resources};

/// A node as placement weighs it (kiln-seams' `NodeUse`, its use live): its name, and its
/// headroom by its latest sample, what was placed there since counted in use (`None`: no fresh
/// sample, and it's placed as before samples).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Node<'a> {
    pub name: &'a str,
    pub headroom: Option<Headroom>,
}

/// No node takes the work (kiln-seams' `Refusal::Unplaceable`): why, in words, and the nodes it
/// would leave past the target, by name.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("{why}")]
pub struct Unplaceable {
    pub why: String,
    pub past: Vec<String>,
}

/// Whether work needing `needs` leaves a node within `target` of its CPUs and of its memory. One
/// without a fresh sample is placed as before samples were: within it.
pub fn fits(needs: &Resources, headroom: Option<&Headroom>, target: f64) -> bool {
    headroom.is_none_or(|h| h.within(needs, target))
}

/// Which of two nodes work needing `needs` goes to first: the one it leaves the more headroom
/// ([`Headroom::fullness`]); nodes without a fresh sample after those, in the caller's order.
pub fn before(needs: &Resources, a: Option<&Headroom>, b: Option<&Headroom>) -> Ordering {
    let full = |h: Option<&Headroom>| h.map_or(f64::INFINITY, |h| h.fullness(needs));
    full(a).total_cmp(&full(b))
}

/// Where work needing `needs` goes, of `nodes` (W05): of those it leaves within `target`
/// ([`fits`]), the one it leaves the most headroom ([`before`]), a tie going to the one given
/// first. None within the target: why, what each would have in use with it.
pub fn place<'a>(needs: &Resources, nodes: &[Node<'a>], target: f64) -> Result<&'a str, Unplaceable> {
    if let Some(n) = nodes.iter().filter(|n| fits(needs, n.headroom.as_ref(), target)).min_by(|a, b| before(needs, a.headroom.as_ref(), b.headroom.as_ref())) {
        return Ok(n.name);
    }
    if nodes.is_empty() {
        return Err(Unplaceable { why: "there are no nodes".into(), past: Vec::new() });
    }
    // Every node has a fresh sample here (one without fits): what each would pass the target by.
    let mut each: Vec<(&str, String)> = nodes
        .iter()
        .filter_map(|n| {
            let (cpu, mem) = n.headroom?.use_with(needs);
            let (what, share) = if mem > cpu { ("memory", mem) } else { ("CPUs", cpu) };
            Some((n.name, format!("{}'s {what} would be at {:.0}%", n.name, share * 100.0)))
        })
        .collect();
    each.sort();
    let why = format!("no node stays within {:.0}% with it: {}", target * 100.0, each.iter().map(|(_, w)| w.as_str()).collect::<Vec<_>>().join(", "));
    Err(Unplaceable { why, past: each.into_iter().map(|(n, _)| n.to_owned()).collect() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sched::state::Sample;

    const GIB: u64 = 1 << 30;

    fn node(name: &str, busy: f64, used_gib: u64, io: f64) -> Node<'_> {
        Node { name, headroom: Some(Headroom::of(&Sample { cpus: 32, busy, load1: 0.0, mem_total: 64 * GIB, mem_used: used_gib * GIB, io_full: Some(io) })) }
    }

    /// W05 step 5: placement's shape, kiln-seams' `Placement`'s: needs and nodes in, a node or why
    /// not out. The node left the most headroom within the target; I/O pressure counting against
    /// one; nodes without a sample after those, in the order given; none within, why, naming what
    /// each would pass it by.
    #[test]
    fn needs_and_nodes_in_a_node_or_why_not_out() {
        let four = Resources { cpu: 4.0, mem: 2 * GIB, ..Resources::default() };
        let (a, b, c) = (node("a", 0.8, 16, 0.0), node("b", 0.3, 16, 0.0), node("c", 0.3, 16, 30.0));
        // a would pass the target; c's disk counts against it, whichever comes first.
        assert_eq!(place(&four, &[a, b, c], 0.85), Ok("b"));
        assert_eq!(place(&four, &[c, b], 0.85), Ok("b"));
        // Even, the first given; one without a sample only once no sampled one fits.
        assert_eq!(place(&four, &[node("d", 0.3, 16, 0.0), b], 0.85), Ok("d"));
        let unknown = Node { name: "e", headroom: None };
        assert_eq!(place(&four, &[unknown, a, b], 0.85), Ok("b"));
        assert_eq!(place(&four, &[unknown, a], 0.85), Ok("e"));
        // None within the target: why, a short of CPUs, b of memory.
        let big = Resources { cpu: 5.0, mem: 42 * GIB, ..Resources::default() };
        assert_eq!(
            place(&big, &[b, a], 0.85),
            Err(Unplaceable { why: "no node stays within 85% with it: a's CPUs would be at 96%, b's memory would be at 91%".into(), past: vec!["a".into(), "b".into()] })
        );
        assert_eq!(place(&four, &[], 0.85), Err(Unplaceable { why: "there are no nodes".into(), past: Vec::new() }));
        assert!(fits(&four, None, 0.85) && !fits(&four, a.headroom.as_ref(), 0.85));
        assert_eq!(before(&four, b.headroom.as_ref(), None), Ordering::Less);
    }
}
