//! Sites (LB08): groups of nodes near one another, so a rate measured between two sites stands for
//! other pairs between them ([`super::rate`]), and placement can tell near from far.
//!
//! Two sources, joined: nodes on one IP subnet are on one network; and nodes close in coordinate
//! space, by single-linkage clustering (Zahn, "Graph-Theoretical Methods for Detecting and
//! Describing Gestalt Clusters", IEEE Trans. Computers C-20(1), 1971: the minimum spanning tree
//! cut at its inconsistent edges). The tree's edges, sorted, are cut at the largest ratio between
//! one and the next whose ranges, each its predicted round trip give or take its uncertainty,
//! don't overlap: a gap the estimates can tell from their own error. No distance is picked. A pair
//! whose prediction no out-of-sample error vouches for takes no part (see [`super::coord`]).

use std::collections::BTreeMap;

/// A node to group: its name, its subnets, and the edges known to the others (by index): their
/// round trip and its uncertainty, ns.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Member {
    pub name: String,
    pub subnets: Vec<String>,
}

/// The site of each member, by its first member's name: `edge(i, j)` the predicted round trip
/// between members `i` and `j` and its uncertainty, ns, or none where unknown.
pub fn group(members: &[Member], edge: impl Fn(usize, usize) -> Option<(u64, u64)>) -> BTreeMap<String, String> {
    let n = members.len();
    let mut parent: Vec<usize> = (0..n).collect();
    let root = |p: &Vec<usize>, mut i: usize| {
        for _ in 0..p.len() {
            match p.get(i) {
                Some(&up) if up != i => i = up,
                _ => break,
            }
        }
        i
    };
    let join = |p: &mut Vec<usize>, a: usize, b: usize| {
        let (ra, rb) = (root(p, a), root(p, b));
        if ra != rb {
            let (lo, hi) = if ra < rb { (ra, rb) } else { (rb, ra) };
            if let Some(slot) = p.get_mut(hi) {
                *slot = lo;
            }
        }
    };
    // Prim's tree over the known edges: (weight, uncertainty, i, j).
    let mut tree: Vec<(u64, u64, usize, usize)> = Vec::new();
    let mut inside = vec![false; n];
    let mut best: Vec<Option<(u64, u64, usize)>> = vec![None; n];
    for start in 0..n {
        if inside.get(start).copied().unwrap_or(true) {
            continue;
        }
        let mut next = Some(start);
        while let Some(v) = next {
            if let Some(s) = inside.get_mut(v) {
                *s = true;
            }
            for u in 0..n {
                if inside.get(u).copied().unwrap_or(true) {
                    continue;
                }
                if let Some((w, e)) = edge(v, u) {
                    let better = best.get(u).copied().flatten().is_none_or(|(bw, _, _)| w < bw);
                    if better && let Some(b) = best.get_mut(u) {
                        *b = Some((w, e, v));
                    }
                }
            }
            next = None;
            let mut least: Option<(u64, usize)> = None;
            for u in 0..n {
                if inside.get(u).copied().unwrap_or(true) {
                    continue;
                }
                if let Some(Some((w, _, _))) = best.get(u)
                    && least.is_none_or(|(lw, _)| *w < lw)
                {
                    least = Some((*w, u));
                }
            }
            if let Some((_, u)) = least
                && let Some(Some((w, e, from))) = best.get(u).copied()
            {
                tree.push((w, e, from, u));
                next = Some(u);
            }
        }
    }
    tree.sort_unstable();
    // The cut: the largest ratio (next − its error)/(this + its error) above 1.
    let mut cut: Option<(u128, u128, u64)> = None;
    for pair in tree.windows(2) {
        let (Some(a), Some(b)) = (pair.first(), pair.get(1)) else { continue };
        let lo = u128::from(a.0.saturating_add(a.1)).max(1);
        let hi = u128::from(b.0.saturating_sub(b.1));
        #[cfg(test)]
        let hi = if super::mutant() == Some(super::Mutant::CutInsideNoise) { u128::from(b.0) } else { hi };
        if hi <= lo {
            continue;
        }
        // Compare hi/lo against the best so far without division: hi·blo > bhi·lo.
        if cut.is_none_or(|(bh, bl, _)| hi * bl > bh * lo) {
            cut = Some((hi, lo, b.0));
        }
    }
    for (w, _, i, j) in &tree {
        if cut.is_none_or(|(_, _, at)| *w < at) {
            join(&mut parent, *i, *j);
        }
    }
    // One subnet, one site.
    let mut by_subnet: BTreeMap<&str, usize> = BTreeMap::new();
    for (i, m) in members.iter().enumerate() {
        for s in &m.subnets {
            match by_subnet.get(s.as_str()) {
                Some(&first) => join(&mut parent, first, i),
                None => {
                    by_subnet.insert(s.as_str(), i);
                }
            }
        }
    }
    let name_of = |i: usize| members.get(i).map(|m| m.name.clone()).unwrap_or_default();
    // A site is named by its least member name.
    let mut least: BTreeMap<usize, String> = BTreeMap::new();
    for i in 0..n {
        let r = root(&parent, i);
        let nm = name_of(i);
        let e = least.entry(r).or_insert_with(|| nm.clone());
        if nm < *e {
            *e = nm;
        }
    }
    (0..n).map(|i| (name_of(i), least.get(&root(&parent, i)).cloned().unwrap_or_default())).collect()
}
