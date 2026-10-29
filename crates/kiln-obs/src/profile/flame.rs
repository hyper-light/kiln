//! Profiles summed into a flame graph (`docs/observe.md` §8.11): every sample's stack, root first,
//! a node a function under its caller's node; each node its total (its own and its callees'
//! values) and its self (its own), summed over every profile added. Functions and their files
//! interned; a node found under its parent by a table of node ids hashed by (parent, function),
//! the node itself the key: four bytes a node.
//!
//! Flame graphs summed apart (a query's threads') are merged ([`Flame::merge`]); a graph is drawn
//! root first, each node's children after it by their functions' names (as Brendan Gregg's
//! flame graphs order them, and pprof's), nodes too small to see left out ([`Flame::drawn`]); and
//! each function's self and total summed, a recursive function's total counted once
//! ([`Flame::by_function`]).

use hashbrown::HashTable;

use super::pprof::Profile;
use crate::hash::Keyed;

/// A node: its function (`Flame::functions`' place), its parent's place (the root's own: 0), its
/// total and self.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Node {
    pub function: u32,
    pub parent: u32,
    pub total: i64,
    pub own: i64,
}

/// A flame graph being summed.
#[derive(Debug)]
pub struct Flame {
    keyed: Keyed,
    /// Strings interned: functions' names and files.
    strings: Vec<Box<str>>,
    string_of: HashTable<u32>,
    /// Functions: their name and file (`strings`' places).
    pub functions: Vec<(u32, u32)>,
    function_of: HashTable<u32>,
    /// The root first (place 0: no function, `u32::MAX`); a node after its parent.
    pub nodes: Vec<Node>,
    child_of: HashTable<u32>,
    /// Profiles and samples added.
    pub profiles: u32,
    pub samples: u64,
    /// A profile's functions as interned here, kept from one profile to the next.
    mine: Vec<u32>,
}

/// Bound on a flame graph's nodes (a node past it is summed into its parent's own: the graph's
/// totals stay whole).
pub const MAX_NODES: usize = 1 << 20;

/// A node as drawn: its function, its parent's place among the nodes drawn (the root's own: 0),
/// its total and self.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Drawn {
    pub function: u32,
    pub parent: u32,
    pub total: i64,
    pub own: i64,
}

fn pair(a: u32, b: u32) -> u64 {
    (u64::from(a) << 32) | u64::from(b)
}

impl Default for Flame {
    fn default() -> Self {
        Self::new()
    }
}

impl Flame {
    pub fn new() -> Self {
        Self {
            keyed: Keyed::new(),
            strings: Vec::new(),
            string_of: HashTable::new(),
            functions: Vec::new(),
            function_of: HashTable::new(),
            nodes: vec![Node { function: u32::MAX, parent: 0, total: 0, own: 0 }],
            child_of: HashTable::new(),
            profiles: 0,
            samples: 0,
            mine: Vec::new(),
        }
    }

    fn intern(&mut self, s: &str) -> u32 {
        let h = self.keyed.bytes(s.as_bytes());
        let strings = &self.strings;
        if let Some(&i) = self.string_of.find(h, |&i| strings.get(i as usize).is_some_and(|x| **x == *s)) {
            return i;
        }
        let i = u32::try_from(self.strings.len()).unwrap_or(u32::MAX);
        self.strings.push(s.into());
        let (keyed, strings) = (self.keyed, &self.strings);
        self.string_of.insert_unique(h, i, |&j| keyed.bytes(strings.get(j as usize).map_or(&b""[..], |x| x.as_bytes())));
        i
    }

    /// The function `name` of `file`, interned.
    fn function(&mut self, name: &str, file: &str) -> u32 {
        let key = (self.intern(name), self.intern(file));
        let h = self.keyed.word(pair(key.0, key.1));
        let functions = &self.functions;
        if let Some(&i) = self.function_of.find(h, |&i| functions.get(i as usize) == Some(&key)) {
            return i;
        }
        let i = u32::try_from(self.functions.len()).unwrap_or(u32::MAX);
        self.functions.push(key);
        let (keyed, functions) = (self.keyed, &self.functions);
        self.function_of.insert_unique(h, i, |&j| functions.get(j as usize).map_or(0, |&(a, b)| keyed.word(pair(a, b))));
        i
    }

    /// The node of `function` under `parent`, made where there is none; none past [`MAX_NODES`].
    fn child(&mut self, parent: u32, function: u32) -> Option<u32> {
        let h = self.keyed.word(pair(parent, function));
        let nodes = &self.nodes;
        if let Some(&n) = self.child_of.find(h, |&n| nodes.get(n as usize).is_some_and(|x| x.parent == parent && x.function == function)) {
            return Some(n);
        }
        if self.nodes.len() >= MAX_NODES {
            return None;
        }
        let n = u32::try_from(self.nodes.len()).unwrap_or(u32::MAX);
        self.nodes.push(Node { function, parent, total: 0, own: 0 });
        let (keyed, nodes) = (self.keyed, &self.nodes);
        self.child_of.insert_unique(h, n, |&m| nodes.get(m as usize).map_or(0, |x| keyed.word(pair(x.parent, x.function))));
        Some(n)
    }

    pub fn string(&self, i: u32) -> &str {
        self.strings.get(i as usize).map_or("", |s| s)
    }

    /// Function `f`'s name and file.
    pub fn name(&self, f: u32) -> (&str, &str) {
        self.functions.get(f as usize).map_or(("", ""), |&(n, file)| (self.string(n), self.string(file)))
    }

    /// Every sample of `p`, its values of the type at `value` (a profile without it adds nothing):
    /// as the profile has them (a sampled profile's values made whole are the writer's to make:
    /// pprof keeps them so).
    pub fn add(&mut self, p: &Profile<'_>, value: usize) {
        self.profiles += 1;
        // Each of the profile's functions, interned once.
        let mut mine = std::mem::take(&mut self.mine);
        mine.clear();
        mine.extend(p.functions.iter().map(|f| self.function(f.name, f.file)));
        for s in &p.samples {
            let Some(&v) = p.values_of(s).get(value) else { continue };
            if v == 0 {
                continue;
            }
            self.samples += 1;
            let stack = p.stacks.get(s.stack.0 as usize..s.stack.1 as usize).unwrap_or_default();
            if let Some(root) = self.nodes.first_mut() {
                root.total = root.total.saturating_add(v);
            }
            let mut at = 0u32;
            // Root first: the stack's last frame is the outermost caller.
            for &f in stack.iter().rev() {
                let Some(&function) = mine.get(f as usize) else { continue };
                // Past the bound: the rest of the stack its parent's own.
                let Some(next) = self.child(at, function) else { break };
                at = next;
                if let Some(node) = self.nodes.get_mut(at as usize) {
                    node.total = node.total.saturating_add(v);
                }
            }
            if let Some(node) = self.nodes.get_mut(at as usize) {
                node.own = node.own.saturating_add(v);
            }
        }
        self.mine = mine;
    }

    /// `other` summed into this one, node by node at the same path; a node past the bound summed
    /// into its nearest ancestor's own.
    pub fn merge(&mut self, other: &Flame) {
        self.profiles += other.profiles;
        self.samples += other.samples;
        let functions: Vec<u32> = other.functions.iter().map(|&(n, f)| self.function(other.string(n), other.string(f))).collect();
        // Each of its nodes' node here, and whether it was folded into it (past the bound).
        let mut here: Vec<(u32, bool)> = Vec::with_capacity(other.nodes.len());
        here.push((0, false));
        if let (Some(root), Some(o)) = (self.nodes.first_mut(), other.nodes.first()) {
            root.total = root.total.saturating_add(o.total);
            root.own = root.own.saturating_add(o.own);
        }
        for o in other.nodes.iter().skip(1) {
            // A node comes after its parent.
            let (parent, folded) = here.get(o.parent as usize).copied().unwrap_or((0, true));
            let made = if folded { None } else { functions.get(o.function as usize).and_then(|&f| self.child(parent, f)) };
            match made {
                Some(n) => {
                    if let Some(node) = self.nodes.get_mut(n as usize) {
                        node.total = node.total.saturating_add(o.total);
                        node.own = node.own.saturating_add(o.own);
                    }
                    here.push((n, false));
                }
                None => {
                    if let Some(node) = self.nodes.get_mut(parent as usize) {
                        node.own = node.own.saturating_add(o.own);
                    }
                    here.push((parent, true));
                }
            }
        }
    }

    /// Every node's values times `num` over `den` (rounded down): snapshots summed over uploads made
    /// their average (`den` uploads of `num` processes).
    pub fn scale(&mut self, num: i64, den: i64) {
        if den <= 0 || num == den {
            return;
        }
        let by = |v: i64| i64::try_from(i128::from(v) * i128::from(num) / i128::from(den)).unwrap_or(i64::MAX);
        for n in &mut self.nodes {
            (n.total, n.own) = (by(n.total), by(n.own));
        }
    }

    /// Each node's children, by their functions' names (then files): where each node's begin in
    /// the list, and the list.
    fn children(&self) -> (Vec<u32>, Vec<u32>) {
        let n = self.nodes.len();
        let mut start = vec![0u32; n + 1];
        for node in self.nodes.iter().skip(1) {
            if let Some(c) = start.get_mut(node.parent as usize + 1) {
                *c += 1;
            }
        }
        for i in 1..=n {
            let before = start.get(i - 1).copied().unwrap_or(0);
            if let Some(c) = start.get_mut(i) {
                *c += before;
            }
        }
        let mut fill: Vec<u32> = start.clone();
        let mut list = vec![0u32; n.saturating_sub(1)];
        for (i, node) in self.nodes.iter().enumerate().skip(1) {
            if let Some(at) = fill.get_mut(node.parent as usize) {
                if let Some(slot) = list.get_mut(*at as usize) {
                    *slot = u32::try_from(i).unwrap_or(u32::MAX);
                }
                *at += 1;
            }
        }
        // Functions ranked by name, then file.
        let mut order: Vec<u32> = (0..u32::try_from(self.functions.len()).unwrap_or(u32::MAX)).collect();
        order.sort_by(|&a, &b| self.name(a).cmp(&self.name(b)));
        let mut rank = vec![0u32; self.functions.len()];
        for (r, &f) in order.iter().enumerate() {
            if let Some(x) = rank.get_mut(f as usize) {
                *x = u32::try_from(r).unwrap_or(u32::MAX);
            }
        }
        let rank_of = |node: u32| self.nodes.get(node as usize).and_then(|x| rank.get(x.function as usize)).copied().unwrap_or(u32::MAX);
        for i in 0..n {
            let (a, b) = (start.get(i).copied().unwrap_or(0) as usize, start.get(i + 1).copied().unwrap_or(0) as usize);
            if let Some(kids) = list.get_mut(a..b) {
                kids.sort_unstable_by_key(|&c| rank_of(c));
            }
        }
        (start, list)
    }

    /// Its nodes as a flame graph draws them: root first, each node's children after it by their
    /// functions' names; a node whose total is under `min_total` left out with all under it (its
    /// parent's width keeps its share).
    pub fn drawn(&self, min_total: i64) -> Vec<Drawn> {
        self.draw(&|n| self.nodes.get(n).map_or(0, |x| x.total), min_total)
            .into_iter()
            .filter_map(|(node, parent)| {
                let x = self.nodes.get(node as usize)?;
                Some(Drawn { function: if node == 0 { u32::MAX } else { x.function }, parent, total: x.total, own: x.own })
            })
            .collect()
    }

    /// The nodes drawn as `width` sizes them: each node and its parent's place among them, root
    /// first, each node's children after it by name, depth first; a node under `min_total`, or of
    /// none, left out with all under it.
    fn draw(&self, width: &dyn Fn(usize) -> i64, min_total: i64) -> Vec<(u32, u32)> {
        let (start, list) = self.children();
        let mut out = vec![(0u32, 0u32)];
        let mut stack: Vec<(u32, u32)> = Vec::new();
        let push_kids = |node: u32, at: u32, stack: &mut Vec<(u32, u32)>| {
            let (a, b) = (start.get(node as usize).copied().unwrap_or(0) as usize, start.get(node as usize + 1).copied().unwrap_or(0) as usize);
            for &c in list.get(a..b).unwrap_or_default().iter().rev() {
                let w = width(c as usize);
                if w >= min_total && w != 0 {
                    stack.push((c, at));
                }
            }
        };
        push_kids(0, 0, &mut stack);
        while let Some((node, parent)) = stack.pop() {
            let at = u32::try_from(out.len()).unwrap_or(u32::MAX);
            out.push((node, parent));
            push_kids(node, at, &mut stack);
        }
        out
    }

    /// Each function with a value: its self (its nodes' own) and its total (its nodes' totals, a
    /// node under another of the same function not counted again, so recursion counts once), as
    /// `(function, self, total)`, in no order.
    pub fn by_function(&self) -> Vec<(u32, i64, i64)> {
        self.by_function_of(&|n| self.nodes.get(n).map_or((0, 0), |x| (x.total, x.own)))
    }

    /// As [`Flame::by_function`], of each node's `(total, self)` as `values` gives them.
    fn by_function_of(&self, values: &dyn Fn(usize) -> (i64, i64)) -> Vec<(u32, i64, i64)> {
        let (start, list) = self.children();
        let (mut own, mut total) = (vec![0i64; self.functions.len()], vec![0i64; self.functions.len()]);
        let mut on_path = vec![0u32; self.functions.len()];
        // Depth first: (node, entering).
        let mut stack: Vec<(u32, bool)> = vec![(0, true)];
        while let Some((node, entering)) = stack.pop() {
            let Some(x) = self.nodes.get(node as usize) else { continue };
            let f = x.function as usize;
            if !entering {
                if let Some(p) = on_path.get_mut(f) {
                    *p -= 1;
                }
                continue;
            }
            if node != 0 {
                let (t_v, o_v) = values(node as usize);
                if let (Some(o), Some(t), Some(p)) = (own.get_mut(f), total.get_mut(f), on_path.get_mut(f)) {
                    *o = o.saturating_add(o_v);
                    if *p == 0 {
                        *t = t.saturating_add(t_v);
                    }
                    *p += 1;
                }
                stack.push((node, false));
            }
            let (a, b) = (start.get(node as usize).copied().unwrap_or(0) as usize, start.get(node as usize + 1).copied().unwrap_or(0) as usize);
            for &c in list.get(a..b).unwrap_or_default() {
                stack.push((c, true));
            }
        }
        own.iter().zip(&total).enumerate().filter(|(_, (o, t))| **o != 0 || **t != 0).map(|(f, (o, t))| (u32::try_from(f).unwrap_or(u32::MAX), *o, *t)).collect()
    }

    /// This graph and `second` side by side (Datadog's comparison; Brendan Gregg's differential
    /// flame graphs): a graph of both's paths, this one's values in its nodes and `second`'s beside
    /// each; a path only one has, of none in the other.
    pub fn compare(&self, second: &Flame) -> Compared {
        let mut g = Flame::new();
        g.merge(self);
        let functions: Vec<u32> = second.functions.iter().map(|&(n, f)| g.function(second.string(n), second.string(f))).collect();
        let mut values = vec![(0i64, 0i64); g.nodes.len()];
        if let (Some(v), Some(r)) = (values.first_mut(), second.nodes.first()) {
            *v = (r.total, r.own);
        }
        // Each of its nodes' node in `g`, and whether it was folded into it (past the bound).
        let mut here: Vec<(u32, bool)> = Vec::with_capacity(second.nodes.len());
        here.push((0, false));
        for o in second.nodes.iter().skip(1) {
            let (parent, folded) = here.get(o.parent as usize).copied().unwrap_or((0, true));
            let made = if folded { None } else { functions.get(o.function as usize).and_then(|&f| g.child(parent, f)) };
            values.resize(g.nodes.len(), (0, 0));
            match made {
                Some(n) => {
                    if let Some(v) = values.get_mut(n as usize) {
                        *v = (v.0.saturating_add(o.total), v.1.saturating_add(o.own));
                    }
                    here.push((n, false));
                }
                None => {
                    if let Some(v) = values.get_mut(parent as usize) {
                        v.1 = v.1.saturating_add(o.own);
                    }
                    here.push((parent, true));
                }
            }
        }
        values.resize(g.nodes.len(), (0, 0));
        Compared { flame: g, second: values, second_profiles: second.profiles, second_samples: second.samples }
    }
}

/// Two graphs side by side ([`Flame::compare`]): the union of their paths, the first's values in
/// its nodes, the second's beside, a node each.
#[derive(Debug)]
pub struct Compared {
    pub flame: Flame,
    /// The second's total and self, a node each.
    pub second: Vec<(i64, i64)>,
    pub second_profiles: u32,
    pub second_samples: u64,
}

/// A node as a comparison draws it: its function, its parent's place among those drawn, its total
/// and self in the first graph and in the second.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DrawnPair {
    pub function: u32,
    pub parent: u32,
    pub total: [i64; 2],
    pub own: [i64; 2],
}

impl Compared {
    fn values(&self, n: usize) -> [(i64, i64); 2] {
        [self.flame.nodes.get(n).map_or((0, 0), |x| (x.total, x.own)), self.second.get(n).copied().unwrap_or((0, 0))]
    }

    /// Drawn as the second graph's totals size it (`by_second`), or the first's: a node under
    /// `min_total` there, or of none, left out with all under it.
    pub fn drawn(&self, by_second: bool, min_total: i64) -> Vec<DrawnPair> {
        let side = usize::from(by_second);
        self.flame
            .draw(&|n| self.values(n).get(side).map_or(0, |v| v.0), min_total)
            .into_iter()
            .map(|(node, parent)| {
                let [a, b] = self.values(node as usize);
                let function = if node == 0 { u32::MAX } else { self.flame.nodes.get(node as usize).map_or(u32::MAX, |x| x.function) };
                DrawnPair { function, parent, total: [a.0, b.0], own: [a.1, b.1] }
            })
            .collect()
    }

    /// Each function with a value in either: `(function, [self, self], [total, total])`, the first
    /// graph's then the second's (recursion counted once), in no order.
    pub fn by_function(&self) -> Vec<(u32, [i64; 2], [i64; 2])> {
        let first = self.flame.by_function_of(&|n| self.values(n)[0]);
        let second = self.flame.by_function_of(&|n| self.values(n)[1]);
        let mut by: std::collections::BTreeMap<u32, ([i64; 2], [i64; 2])> = std::collections::BTreeMap::new();
        for (f, o, t) in first {
            let e = by.entry(f).or_default();
            (e.0[0], e.1[0]) = (o, t);
        }
        for (f, o, t) in second {
            let e = by.entry(f).or_default();
            (e.0[1], e.1[1]) = (o, t);
        }
        by.into_iter().map(|(f, (o, t))| (f, o, t)).collect()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::super::pprof::{Function, Profile, Sample, ValueType};
    use super::*;

    /// `main` → `work` → `hash` twice (10 and 5 ns) and `main` → `idle` (20 ns), of a profile's
    /// second type; another profile alike.
    fn profile() -> Profile<'static> {
        let f = |name| Function { name, file: "app/main.go" };
        let sample = |stack: (u32, u32), values: (u32, u32)| Sample { stack, values, labels: (0, 0) };
        Profile {
            sample_types: vec![ValueType { kind: "samples", unit: "count" }, ValueType { kind: "cpu", unit: "nanoseconds" }],
            functions: vec![f("main"), f("work"), f("hash"), f("idle")],
            stacks: vec![2, 1, 0, 3, 0, 2, 1, 0],
            values: vec![1, 10, 2, 20, 1, 5],
            samples: vec![sample((0, 3), (0, 2)), sample((3, 5), (2, 4)), sample((5, 8), (4, 6))],
            ..Profile::default()
        }
    }

    /// The node at `path` (functions' names from the root).
    fn node(g: &Flame, path: &[&str]) -> Node {
        let mut at = 0usize;
        for name in path {
            at = (1..g.nodes.len()).find(|&n| g.nodes[n].parent as usize == at && g.name(g.nodes[n].function).0 == *name).unwrap();
        }
        g.nodes[at]
    }

    /// Every node's path (its functions' names from the root) and its total and self.
    fn paths(g: &Flame) -> std::collections::BTreeMap<String, (i64, i64)> {
        let mut out = std::collections::BTreeMap::new();
        for (i, n) in g.nodes.iter().enumerate().skip(1) {
            let mut path = vec![g.name(n.function).0.to_owned()];
            let mut at = n.parent as usize;
            while at != 0 {
                path.push(g.name(g.nodes[at].function).0.to_owned());
                at = g.nodes[at].parent as usize;
            }
            path.reverse();
            assert!(out.insert(path.join(";"), (n.total, n.own)).is_none(), "node {i}: one node a path");
        }
        out
    }

    /// Each stack root first, a node a function under its caller's: totals its subtree's, self its
    /// own; a second profile summed into the same nodes.
    #[test]
    fn stacks_are_summed_root_first() {
        let mut g = Flame::new();
        let p = profile();
        g.add(&p, 1);
        assert_eq!((g.nodes[0].total, g.profiles, g.samples), (35, 1, 3));
        assert_eq!((node(&g, &["main"]).total, node(&g, &["main"]).own), (35, 0));
        assert_eq!((node(&g, &["main", "work"]).total, node(&g, &["main", "work", "hash"]).own), (15, 15));
        assert_eq!((node(&g, &["main", "idle"]).total, node(&g, &["main", "idle"]).own), (20, 20));
        let nodes = g.nodes.len();
        g.add(&p, 1);
        assert_eq!((g.nodes.len(), g.nodes[0].total, node(&g, &["main", "work", "hash"]).total), (nodes, 70, 30), "the same nodes, summed");
        let mut h = Flame::new();
        h.add(&p, 7);
        assert_eq!(h.nodes[0].total, 0, "a type the profile has not: nothing");
    }

    /// Go's own profiles summed apart and merged are the graph of them summed together, path by
    /// path; and merged into a graph of other functions, each path its own.
    #[test]
    fn graphs_summed_apart_and_merged_are_the_graph_summed_together() {
        let mut cpu = Vec::new();
        std::io::Read::read_to_end(&mut flate2::read::GzDecoder::new(&include_bytes!("../fixtures/pprof/go-cpu.pprof")[..]), &mut cpu).unwrap();
        let p = super::super::pprof::read(&cpu).unwrap();
        let (mut whole, mut a, mut b) = (Flame::new(), Flame::new(), Flame::new());
        whole.add(&p, 1);
        whole.add(&p, 1);
        whole.add(&profile(), 1);
        a.add(&p, 1);
        b.add(&profile(), 1);
        b.add(&p, 1);
        a.merge(&b);
        assert_eq!(paths(&a), paths(&whole));
        assert_eq!((a.nodes[0].total, a.profiles, a.samples), (whole.nodes[0].total, 3, whole.samples));
    }

    /// Drawn root first, each node's children after it by name, each subtree whole before the
    /// next; a node too small left out with what is under it; every node's parent drawn before it.
    #[test]
    fn a_graph_is_drawn_root_first_its_children_by_name() {
        let mut g = Flame::new();
        g.add(&profile(), 1);
        let drawn = g.drawn(0);
        let names: Vec<(&str, u32)> = drawn.iter().map(|d| (if d.function == u32::MAX { "" } else { g.name(d.function).0 }, d.parent)).collect();
        assert_eq!(names, [("", 0), ("main", 0), ("idle", 1), ("work", 1), ("hash", 3)]);
        assert_eq!((drawn[4].total, drawn[4].own), (15, 15));
        let small = g.drawn(16);
        assert_eq!(small.iter().map(|d| if d.function == u32::MAX { "" } else { g.name(d.function).0 }).collect::<Vec<_>>(), ["", "main", "idle"], "work (15) and hash under it left out");
        assert_eq!(small[1].total, 35, "main's width keeps their share");
    }

    /// A function's self summed over its nodes; its total its outermost nodes' (recursion counted
    /// once).
    #[test]
    fn a_recursive_function_s_total_is_counted_once() {
        // main → fib → fib → fib (7), main → fib (3), main → log (2).
        let f = |name| Function { name, file: "f.go" };
        let sample = |stack: (u32, u32), values: (u32, u32)| Sample { stack, values, labels: (0, 0) };
        let p = Profile {
            sample_types: vec![ValueType { kind: "cpu", unit: "nanoseconds" }],
            functions: vec![f("main"), f("fib"), f("log")],
            stacks: vec![1, 1, 1, 0, 1, 0, 2, 0],
            values: vec![7, 3, 2],
            samples: vec![sample((0, 4), (0, 1)), sample((4, 6), (1, 2)), sample((6, 8), (2, 3))],
            ..Profile::default()
        };
        let mut g = Flame::new();
        g.add(&p, 0);
        let by: std::collections::BTreeMap<&str, (i64, i64)> = g.by_function().into_iter().map(|(f, o, t)| (g.name(f).0, (o, t))).collect();
        assert_eq!(by["fib"], (10, 10), "its nodes' totals 10, 7, 7: counted once, as 10");
        assert_eq!((by["main"], by["log"]), ((0, 12), (2, 2)));
    }

    /// Two graphs side by side: every path of either, each node its values in both (one only the
    /// first has, none in the second; one only the second has, none in the first); drawn as either
    /// sizes it; each function's self and total in both.
    #[test]
    fn two_graphs_are_compared_path_by_path() {
        let mut before = Flame::new();
        before.add(&profile(), 1);
        // After: `hash` gone from `work` (its 15 now `work`'s own), `idle` doubled, `gc` new.
        let f = |name| Function { name, file: "app/main.go" };
        let sample = |stack: (u32, u32), values: (u32, u32)| Sample { stack, values, labels: (0, 0) };
        let after_p = Profile {
            sample_types: vec![ValueType { kind: "cpu", unit: "nanoseconds" }],
            functions: vec![f("main"), f("work"), f("idle"), f("gc")],
            stacks: vec![1, 0, 2, 0, 3],
            values: vec![15, 40, 5],
            samples: vec![sample((0, 2), (0, 1)), sample((2, 4), (1, 2)), sample((4, 5), (2, 3))],
            ..Profile::default()
        };
        let mut after = Flame::new();
        after.add(&after_p, 0);
        let c = before.compare(&after);
        let at = |path: &[&str]| {
            let n = (0..c.flame.nodes.len()).find(|&i| {
                let mut names = Vec::new();
                let mut at = i;
                while at != 0 {
                    names.push(c.flame.name(c.flame.nodes[at].function).0);
                    at = c.flame.nodes[at].parent as usize;
                }
                names.reverse();
                names == path
            });
            n.map(|n| c.values(n))
        };
        assert_eq!(at(&["main", "work", "hash"]), Some([(15, 15), (0, 0)]), "only before");
        assert_eq!(at(&["main", "work"]), Some([(15, 0), (15, 15)]));
        assert_eq!(at(&["main", "idle"]), Some([(20, 20), (40, 40)]));
        assert_eq!(at(&["gc"]), Some([(0, 0), (5, 5)]), "only after, at the root");
        assert_eq!(c.values(0), [(35, 0), (60, 0)]);
        let by_after = c.drawn(true, 0);
        let names: Vec<&str> = by_after.iter().map(|d| if d.function == u32::MAX { "" } else { c.flame.name(d.function).0 }).collect();
        assert_eq!(names, ["", "gc", "main", "idle", "work"], "as the second sizes it: hash, none there, left out");
        assert_eq!(c.drawn(false, 0).len(), 5, "as the first: gc, none there, left out");
        let by: std::collections::BTreeMap<&str, ([i64; 2], [i64; 2])> = c.by_function().into_iter().map(|(f, o, t)| (c.flame.name(f).0, (o, t))).collect();
        assert_eq!(by["hash"], ([15, 0], [15, 0]));
        assert_eq!(by["work"], ([0, 15], [15, 15]));
        assert_eq!(by["gc"], ([0, 5], [0, 5]));
    }
}
