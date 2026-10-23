//! Profiles summed into a flame graph (`docs/observe.md` §8.11): every sample's stack, root first,
//! a node a function under its caller's node; each node its total (its own and its callees'
//! values) and its self (its own), summed over every profile added. Functions and files interned;
//! a node found under its parent by a map of (parent, function).

use std::collections::HashMap;

use super::pprof::Profile;

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
#[derive(Debug, Default)]
pub struct Flame {
    /// Strings interned: functions' names and files.
    strings: Vec<Box<str>>,
    string_of: HashMap<Box<str>, u32>,
    /// Functions: their name and file (`strings`' places).
    pub functions: Vec<(u32, u32)>,
    function_of: HashMap<(u32, u32), u32>,
    /// The root first (place 0: no function, `u32::MAX`).
    pub nodes: Vec<Node>,
    child_of: HashMap<(u32, u32), u32>,
    /// Profiles and samples added.
    pub profiles: u32,
    pub samples: u64,
}

/// Bound on a flame graph's nodes (a node past it is summed into its parent's own: the graph's
/// totals stay whole).
pub const MAX_NODES: usize = 1 << 20;

impl Flame {
    pub fn new() -> Self {
        Self { nodes: vec![Node { function: u32::MAX, parent: 0, total: 0, own: 0 }], ..Self::default() }
    }

    fn intern(&mut self, s: &str) -> u32 {
        if let Some(&i) = self.string_of.get(s) {
            return i;
        }
        let i = u32::try_from(self.strings.len()).unwrap_or(u32::MAX);
        self.strings.push(s.into());
        self.string_of.insert(s.into(), i);
        i
    }

    pub fn string(&self, i: u32) -> &str {
        self.strings.get(i as usize).map_or("", |s| s)
    }

    /// Every sample of `p`, its values of the type at `value` (a profile without it adds nothing)
    /// times `scale` (a sampled profile's values made whole are the writer's to make: pprof keeps
    /// them so).
    pub fn add(&mut self, p: &Profile<'_>, value: usize) {
        self.profiles += 1;
        // Each of the profile's functions, interned once.
        let mut mine: Vec<u32> = Vec::with_capacity(p.functions.len());
        for f in &p.functions {
            let key = (self.intern(f.name), self.intern(f.file));
            let next = u32::try_from(self.functions.len()).unwrap_or(u32::MAX);
            let id = *self.function_of.entry(key).or_insert_with(|| {
                self.functions.push(key);
                next
            });
            mine.push(id);
        }
        for s in &p.samples {
            let Some(&v) = p.values_of(s).get(value) else { continue };
            if v == 0 {
                continue;
            }
            self.samples += 1;
            let stack = p.stacks.get(s.stack.0 as usize..s.stack.1 as usize).unwrap_or_default();
            let mut at = 0u32;
            if let Some(root) = self.nodes.get_mut(0) {
                root.total += v;
            }
            // Root first: the stack's last frame is the outermost caller.
            for &f in stack.iter().rev() {
                let Some(&function) = mine.get(f as usize) else { continue };
                let next = match self.child_of.get(&(at, function)) {
                    Some(&n) => n,
                    None if self.nodes.len() < MAX_NODES => {
                        let n = u32::try_from(self.nodes.len()).unwrap_or(u32::MAX);
                        self.nodes.push(Node { function, parent: at, total: 0, own: 0 });
                        self.child_of.insert((at, function), n);
                        n
                    }
                    // Past the bound: the rest of the stack its parent's own.
                    None => break,
                };
                at = next;
                if let Some(node) = self.nodes.get_mut(at as usize) {
                    node.total += v;
                }
            }
            if let Some(node) = self.nodes.get_mut(at as usize) {
                node.own += v;
            }
        }
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
            at = (1..g.nodes.len()).find(|&n| g.nodes[n].parent as usize == at && g.string(g.functions[g.nodes[n].function as usize].0) == *name).unwrap();
        }
        g.nodes[at]
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
}
