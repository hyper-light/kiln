//! When to split, merge or move (docs/design/reshard.md, "When: from what is measured"). No
//! threshold is picked: each rule compares a group's measured share of the work against the share
//! one core can take, which is the nodes' measured cores.
//!
//! - **Split** a group whose load, or size, is more than one core's share of the whole (`× C >
//!   total`, `C` the cores of every node): one group has one writer, so past that share it holds back
//!   a core another group could use (docs/design/store.md, decision 10). It is cut where its
//!   measured load, or its rows, fall half each side.
//! - **Merge** two groups side by side whose load and size together are at most half a core's
//!   share. The factor of two is the split's own (a split halves), which keeps a merge from undoing
//!   a split while the work stands still: the dynamic table's rule of growing when full and
//!   shrinking at a quarter, not at a half, so no sequence of changes thrashes (Cormen, Leiserson,
//!   Rivest and Stein, Introduction to Algorithms, 3rd ed., section 17.4.2).
//! - **Move** a group from the node with the most load per core to the one with the least only when
//!   the move leaves the second no busier per core than the first: a move never overshoots, so a
//!   group never goes back and forth. A node that has no cores left (being removed) gives up every
//!   group, as far as the others will take them.

use std::collections::{BTreeMap, BTreeSet};

use super::directory::{Directory, Proposal};
use super::{GroupId, NodeId, Pos};

/// A cut of a span: before `at`, with the weight measured on each side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cut {
    pub at: Pos,
    pub left: u64,
    pub right: u64,
}

/// The cut nearest the middle of `points` (positions in order, each with its weight: rows, bytes
/// or load sampled there) that leaves weight on both sides; none if there is no such cut, or the
/// positions aren't in order.
#[must_use]
pub fn cut(points: &[(Pos, u64)]) -> Option<Cut> {
    let mut total: u128 = 0;
    let mut last: Option<Pos> = None;
    for (pos, weight) in points {
        if last.is_some_and(|l| *pos < l) {
            return None;
        }
        last = Some(*pos);
        total = total.checked_add(u128::from(*weight))?;
    }
    let mut best: Option<(u128, Pos, u128)> = None;
    let mut left: u128 = 0;
    let mut previous: Option<Pos> = None;
    for (pos, weight) in points {
        if previous.is_some_and(|p| p != *pos) && left > 0 && left < total {
            let right = total.checked_sub(left)?;
            let gap = left.abs_diff(right);
            if best.is_none_or(|(g, _, _)| gap < g) {
                best = Some((gap, *pos, left));
            }
        }
        previous = Some(*pos);
        left = left.checked_add(u128::from(*weight))?;
    }
    let (_, at, left) = best?;
    Some(Cut {
        at,
        left: u64::try_from(left).ok()?,
        right: u64::try_from(total.checked_sub(left)?).ok()?,
    })
}

/// What was measured of one group over the shell's last window: its bytes, its load (any measure
/// that adds up over groups: writes, or its writer's busy time), and where its rows and its load
/// fall half each side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GroupLoad {
    pub group: GroupId,
    pub bytes: u64,
    pub load: u64,
    pub by_size: Option<Cut>,
    pub by_load: Option<Cut>,
}

/// A node's cores for groups' writers, as the node measures them (none: it is being removed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodeRoom {
    pub node: NodeId,
    pub cores: u64,
}

/// `a × b` against `c × d`, without overflow.
fn more(a: u128, b: u128, c: u128, d: u128) -> bool {
    a.saturating_mul(b) > c.saturating_mul(d)
}

/// The operations the measures call for, on groups in none, each group in one at most: splits,
/// then merges, then moves. Nothing if a group of the directory wasn't measured: a decision on
/// part of the picture could be wrong.
#[must_use]
pub fn plan(dir: &Directory, nodes: &[NodeRoom], groups: &[GroupLoad]) -> Vec<Proposal> {
    let measured: BTreeMap<GroupId, GroupLoad> = groups.iter().map(|g| (g.group, *g)).collect();
    if dir.groups().any(|(g, _)| !measured.contains_key(&g)) {
        return Vec::new();
    }
    let cores: u128 = nodes.iter().map(|n| u128::from(n.cores)).sum();
    if cores == 0 {
        return Vec::new();
    }
    let load: u128 = measured.values().map(|g| u128::from(g.load)).sum();
    let bytes: u128 = measured.values().map(|g| u128::from(g.bytes)).sum();
    let mut used: BTreeSet<GroupId> = dir
        .groups()
        .map(|(g, _)| g)
        .filter(|g| dir.busy(*g))
        .collect();
    let mut out = Vec::new();

    for (span, group) in dir.spans() {
        let Some(m) = measured.get(group) else {
            continue;
        };
        if used.contains(group) {
            continue;
        }
        let fits =
            |c: &Option<Cut>| c.filter(|c| c.left > 0 && c.right > 0 && span.cut(c.at).is_some());
        let at = if more(u128::from(m.load), cores, load, 1) {
            fits(&m.by_load).or_else(|| fits(&m.by_size))
        } else if more(u128::from(m.bytes), cores, bytes, 1) {
            fits(&m.by_size)
        } else {
            None
        };
        if let Some(c) = at {
            used.insert(*group);
            out.push(Proposal::Split {
                group: *group,
                at: c.at,
            });
        }
    }

    let twice = cores.saturating_mul(2);
    for pair in dir.spans().windows(2) {
        let [(_, a), (_, b)] = pair else { continue };
        if used.contains(a) || used.contains(b) {
            continue;
        }
        let (Some(ma), Some(mb)) = (measured.get(a), measured.get(b)) else {
            continue;
        };
        let pair_load = u128::from(ma.load).saturating_add(u128::from(mb.load));
        let pair_bytes = u128::from(ma.bytes).saturating_add(u128::from(mb.bytes));
        if more(pair_load, twice, load, 1) || more(pair_bytes, twice, bytes, 1) {
            continue;
        }
        // The smaller is copied into the larger.
        let (from, into) = if (ma.bytes, a) <= (mb.bytes, b) {
            (*a, *b)
        } else {
            (*b, *a)
        };
        used.insert(*a);
        used.insert(*b);
        out.push(Proposal::Merge { from, into });
    }

    moves(dir, nodes, &measured, &mut used, &mut out);
    out
}

#[derive(Debug, Clone, Copy)]
struct Node {
    cores: u128,
    load: u128,
}

fn moves(
    dir: &Directory,
    nodes: &[NodeRoom],
    measured: &BTreeMap<GroupId, GroupLoad>,
    used: &mut BTreeSet<GroupId>,
    out: &mut Vec<Proposal>,
) {
    let mut at: BTreeMap<NodeId, Node> = nodes
        .iter()
        .map(|n| {
            (
                n.node,
                Node {
                    cores: u128::from(n.cores),
                    load: 0,
                },
            )
        })
        .collect();
    let mut placed: BTreeMap<GroupId, NodeId> = BTreeMap::new();
    for (group, node) in dir.groups() {
        let load = measured.get(&group).map_or(0, |m| u128::from(m.load));
        let n = at.entry(node).or_insert(Node { cores: 0, load: 0 });
        n.load = n.load.saturating_add(load);
        placed.insert(group, node);
    }
    // Each round moves one group and updates the loads: at most one round a group.
    for _ in 0..placed.len() {
        let Some((cold, c)) = at
            .iter()
            .filter(|(_, n)| n.cores > 0)
            .min_by(|(_, x), (_, y)| {
                x.load
                    .saturating_mul(y.cores)
                    .cmp(&y.load.saturating_mul(x.cores))
            })
            .map(|(id, n)| (*id, *n))
        else {
            return;
        };
        // A node being removed gives up its groups first, whatever they weigh.
        let draining = at
            .iter()
            .filter(|(id, n)| {
                n.cores == 0 && free_on(&placed, measured, used, **id).next().is_some()
            })
            .map(|(id, n)| (*id, *n))
            .next();
        let chosen = if let Some((hot, _)) = draining {
            free_on(&placed, measured, used, hot)
                .max_by_key(|m| {
                    (
                        m.load,
                        std::cmp::Reverse(m.bytes),
                        std::cmp::Reverse(m.group),
                    )
                })
                .map(|m| (hot, *m))
        } else {
            let Some((hot, h)) = at
                .iter()
                .filter(|(_, n)| n.cores > 0)
                .max_by(|(_, x), (_, y)| {
                    x.load
                        .saturating_mul(y.cores)
                        .cmp(&y.load.saturating_mul(x.cores))
                })
                .map(|(id, n)| (*id, *n))
            else {
                return;
            };
            if hot == cold {
                return;
            }
            // Never overshoot: after the move the cold node is no busier per core than the hot.
            free_on(&placed, measured, used, hot)
                .filter(|m| m.load > 0)
                .filter(|m| {
                    let g = u128::from(m.load);
                    let after_hot = h.load.saturating_sub(g);
                    !more(c.load.saturating_add(g), h.cores, after_hot, c.cores)
                })
                .max_by_key(|m| {
                    (
                        m.load,
                        std::cmp::Reverse(m.bytes),
                        std::cmp::Reverse(m.group),
                    )
                })
                .map(|m| (hot, *m))
        };
        let Some((hot, m)) = chosen else { return };
        let g = u128::from(m.load);
        if let Some(n) = at.get_mut(&hot) {
            n.load = n.load.saturating_sub(g);
        }
        if let Some(n) = at.get_mut(&cold) {
            n.load = n.load.saturating_add(g);
        }
        used.insert(m.group);
        out.push(Proposal::Move {
            group: m.group,
            to: cold,
        });
    }
}

/// The measured groups on `node` in no operation yet.
fn free_on<'a>(
    placed: &'a BTreeMap<GroupId, NodeId>,
    measured: &'a BTreeMap<GroupId, GroupLoad>,
    used: &'a BTreeSet<GroupId>,
    node: NodeId,
) -> impl Iterator<Item = &'a GroupLoad> + 'a {
    placed
        .iter()
        .filter(move |(g, n)| **n == node && !used.contains(*g))
        .filter_map(|(g, _)| measured.get(g))
}

#[cfg(test)]
mod tests {
    use super::super::directory::Change;
    use super::*;

    fn measure(group: u64, bytes: u64, load: u64) -> GroupLoad {
        GroupLoad {
            group: GroupId(group),
            bytes,
            load,
            by_size: Some(Cut {
                at: 1 << 62,
                left: bytes / 2,
                right: bytes - bytes / 2,
            }),
            by_load: Some(Cut {
                at: 1 << 61,
                left: load / 2,
                right: load - load / 2,
            }),
        }
    }

    fn room(node: u64, cores: u64) -> NodeRoom {
        NodeRoom {
            node: NodeId(node),
            cores,
        }
    }

    /// The directory with `cuts` made, every group on node 1.
    fn split_at(cuts: &[Pos]) -> Directory {
        let mut d = Directory::new(NodeId(1));
        for (i, at) in cuts.iter().enumerate() {
            let c = d
                .propose(&Proposal::Split {
                    group: GroupId(i as u64),
                    at: *at,
                })
                .unwrap();
            d.apply(&c).unwrap();
            let id = super::super::OpId(i as u64 + 1);
            d.apply(&Change::Cutover(id)).unwrap();
            d.apply(&Change::Flip(id)).unwrap();
            d.apply(&Change::End(id)).unwrap();
        }
        d
    }

    #[test]
    fn t_t1_9_the_cut_falls_nearest_the_middle_between_positions() {
        assert_eq!(
            cut(&[(1, 1), (2, 1), (3, 1), (4, 1)]),
            Some(Cut {
                at: 3,
                left: 2,
                right: 2
            })
        );
        assert_eq!(
            cut(&[(1, 5), (1, 5), (9, 1)]),
            Some(Cut {
                at: 9,
                left: 10,
                right: 1
            })
        );
        assert_eq!(cut(&[(7, 3), (7, 4)]), None, "one position can't be cut");
        assert_eq!(cut(&[(2, 1), (1, 1)]), None, "out of order");
        assert_eq!(cut(&[(1, 0), (2, 5)]), None, "no weight on one side");
    }

    /// AC-T1.9.1: one group on one node with one core never splits; with 4 cores a group with
    /// all the load splits at its load's middle.
    #[test]
    fn t_t1_9_a_group_splits_past_one_cores_share() {
        let d = Directory::new(NodeId(1));
        assert!(plan(&d, &[room(1, 1)], &[measure(0, 100, 100)]).is_empty());
        assert_eq!(
            plan(&d, &[room(1, 4)], &[measure(0, 100, 100)]),
            vec![Proposal::Split {
                group: GroupId(0),
                at: 1 << 61
            }]
        );
        // No load: it splits by size.
        assert_eq!(
            plan(&d, &[room(1, 4)], &[measure(0, 100, 0)]),
            vec![Proposal::Split {
                group: GroupId(0),
                at: 1 << 62
            }]
        );
        // Nothing at all: nothing to do.
        assert!(plan(&d, &[room(1, 4)], &[measure(0, 0, 0)]).is_empty());
    }

    /// Two groups side by side merge only at half a core's share together, never right after
    /// the split that made them.
    #[test]
    fn t_t1_9_groups_merge_at_half_a_cores_share_not_at_one() {
        // Three groups, 4 cores: share 1/4, merge when a pair holds at most 1/8. The third's
        // measured cuts fall outside its span, so it can't split.
        let d = split_at(&[1 << 62, 1 << 63]);
        let ms = |a, b, c| vec![measure(0, a, a), measure(1, b, b), measure(2, c, c)];
        let p = plan(&d, &[room(1, 4)], &ms(6, 6, 188));
        assert!(
            p.contains(&Proposal::Merge {
                from: GroupId(0),
                into: GroupId(1)
            }),
            "{p:?}"
        );
        // At 1/5 together (above 1/8): no merge.
        let p = plan(&d, &[room(1, 4)], &ms(10, 10, 80));
        assert!(
            !p.iter().any(|p| matches!(p, Proposal::Merge { .. })),
            "{p:?}"
        );
    }

    /// A move leaves the cold node no busier per core than the hot; a node with no cores gives
    /// up its groups.
    #[test]
    fn t_t1_9_moves_never_overshoot_and_a_removed_node_is_emptied() {
        let d = split_at(&[1 << 62, 1 << 63]);
        // Three groups on node 1 (2 cores), node 2 empty (2 cores), 20 load each; no cut
        // measured, so none splits.
        let ms = vec![
            GroupLoad {
                by_load: None,
                by_size: None,
                ..measure(0, 1, 20)
            },
            GroupLoad {
                by_load: None,
                by_size: None,
                ..measure(1, 1, 20)
            },
            GroupLoad {
                by_load: None,
                by_size: None,
                ..measure(2, 1, 20)
            },
        ];
        let p = plan(&d, &[room(1, 2), room(2, 2)], &ms);
        // 60 on node 1: one 20 moves (40 vs 20); a second would overshoot (20 vs 40).
        assert_eq!(
            p.iter()
                .filter(|p| matches!(p, Proposal::Move { .. }))
                .count(),
            1,
            "{p:?}"
        );
        let p = plan(&d, &[room(1, 0), room(2, 2)], &ms);
        assert_eq!(
            p.iter()
                .filter(|p| matches!(p, Proposal::Move { to: NodeId(2), .. }))
                .count(),
            3
        );
    }
}
