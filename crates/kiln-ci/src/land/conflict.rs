//! The land queue's conflict graph (docs/ci.md W26-R30 to R34), the pure part: which of the
//! changes pending in the queue commute, and so land side by side, and which wait for which, each
//! decision with why. The server gives the changes' files and deploy targets and the repository's
//! units (W24-R16's package graph), and carries out what this says.
//!
//! - A [`Change`] is the files it changes and the deploy targets it changes; [`Units`] says which
//!   units its files are inputs of, and which units it affects, as W24-R17 has it (R30).
//! - Two changes conflict when they share a changed file, an affected unit or a deploy target. The
//!   conflict graph's connected components ([`Component`]) come from a union-find: changes in
//!   different components never wait for each other (R31).
//! - A component's interaction units take files from two or more of its changes; its joint units
//!   are affected by two or more. Only joint units get keys no change gives alone (R32).
//! - Changes no two of which conflict commute: any order of them gives the same tree, and every
//!   unit the key it has with its own change alone. They land as one class ([`Class`]), the sorted
//!   set of their ids (R33).
//! - Each change is decided against each change ahead of it in the queue: it waits for it, naming
//!   what they share, or lands beside it ([`Verdict::why`], R34).
//!
//! W24-R30's e-graph is a union-find over result keys within a trust domain. Changes aren't keys,
//! so the components here come from a union-find of their own.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Serialize};

/// A unit of the repository: a package (W24-R16), by its folder.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Unit {
    /// Its name, one per unit (`kiln-ci`; `npm:ui` where two ecosystems share a name).
    pub name: String,
    /// Its ecosystem (`cargo`, `npm`): of each ecosystem's units, a file is an input of the one whose
    /// folder holds it most closely (W24-R17's package of a file).
    pub ecosystem: String,
    /// Its folder from the repository's root (`crates/kiln-ci`); empty for the root.
    pub dir: String,
    /// Files it reads besides those its folder holds: its workspace's root manifest and lockfile
    /// (`Cargo.toml`, `Cargo.lock`), which W24-R17 has every package of the workspace read.
    pub reads: BTreeSet<String>,
    /// The units it depends on, by name.
    pub deps: BTreeSet<String>,
}

/// The repository's units and what each depends on (W26-R30). A name given more than once is one
/// unit, holding the files of each folder given, reading every file and depending on every unit
/// any of them names: the trunk's graph and each pending change's own (W24-R16 read with the change
/// applied), given together, are their union, so a dependency one change adds links it to the
/// changes of what it depends on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Units {
    /// Each unit by name: as first given, with every one's reads and dependencies.
    units: BTreeMap<String, Unit>,
    /// The units each folder holds, by ecosystem.
    folders: BTreeMap<String, BTreeMap<String, BTreeSet<String>>>,
    /// The units that read each file.
    readers: BTreeMap<String, BTreeSet<String>>,
    /// The units that depend on each unit.
    dependents: BTreeMap<String, BTreeSet<String>>,
}

impl Units {
    pub fn new(units: impl IntoIterator<Item = Unit>) -> Units {
        let mut g = Units::default();
        for u in units {
            g.folders.entry(clean(&u.dir).to_owned()).or_default().entry(u.ecosystem.clone()).or_default().insert(u.name.clone());
            for f in &u.reads {
                g.readers.entry(clean(f).to_owned()).or_default().insert(u.name.clone());
            }
            for d in &u.deps {
                g.dependents.entry(d.clone()).or_default().insert(u.name.clone());
            }
            match g.units.entry(u.name.clone()) {
                Entry::Vacant(e) => {
                    e.insert(u);
                }
                Entry::Occupied(mut e) => {
                    let had = e.get_mut();
                    had.reads.extend(u.reads);
                    had.deps.extend(u.deps);
                }
            }
        }
        g
    }

    /// A unit, by its name.
    pub fn unit(&self, name: &str) -> Option<&Unit> {
        self.units.get(name)
    }

    /// The units `file` is an input of: of each ecosystem's units, those whose folder holds it most
    /// closely, and every unit that reads it.
    pub fn takes(&self, file: &str) -> BTreeSet<&str> {
        let file = clean(file);
        let mut out: BTreeSet<&str> = self.readers.get(file).into_iter().flatten().map(String::as_str).collect();
        let mut held: BTreeSet<&str> = BTreeSet::new();
        let mut at = file;
        while !at.is_empty() {
            at = at.rsplit_once('/').map_or("", |(up, _)| up);
            for (eco, names) in self.folders.get(at).into_iter().flatten() {
                if held.insert(eco) {
                    out.extend(names.iter().map(String::as_str));
                }
            }
        }
        out
    }

    /// The units a change to `files` can affect (W24-R17): those the files are inputs of, and every
    /// unit that depends on one of those, transitively.
    pub fn affected<S: AsRef<str>>(&self, files: impl IntoIterator<Item = S>) -> BTreeSet<String> {
        let mut hit: BTreeSet<String> = BTreeSet::new();
        for f in files {
            hit.extend(self.takes(f.as_ref()).into_iter().map(str::to_owned));
        }
        self.with_dependents(hit)
    }

    /// `hit` and every unit that depends on one of them, transitively.
    fn with_dependents(&self, mut hit: BTreeSet<String>) -> BTreeSet<String> {
        let mut todo: Vec<String> = hit.iter().cloned().collect();
        while let Some(u) = todo.pop() {
            for d in self.dependents.get(&u).into_iter().flatten() {
                if hit.insert(d.clone()) {
                    todo.push(d.clone());
                }
            }
        }
        hit
    }
}

/// A change pending in the land queue (W26-R30): a diff, or a change request.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Change {
    /// The queue's id for it (`D12`), one per change.
    pub id: String,
    /// The files it changes (adds, edits or removes), from the repository's root; a rename is both
    /// of its paths.
    pub files: BTreeSet<String>,
    /// The deploy targets it changes.
    pub targets: BTreeSet<String>,
}

/// A change as the conflict graph has it (W26-R30).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Pending {
    pub id: String,
    pub files: BTreeSet<String>,
    /// The units its files are inputs of.
    pub takes: BTreeSet<String>,
    /// The units it affects: those, and every unit that depends on one of them, transitively.
    pub units: BTreeSet<String>,
    pub targets: BTreeSet<String>,
    /// Its component, by its place in [`Conflicts::components`].
    pub component: usize,
}

/// A connected component of the conflict graph (W26-R31, R32): a queue of its own.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Component {
    /// Its changes, in queue order.
    pub changes: Vec<String>,
    /// Its interaction units: each unit whose inputs take files from two or more of its changes,
    /// with those changes, in queue order. They are where its changes meet.
    pub interaction: BTreeMap<String, Vec<String>>,
    /// Its joint units: each unit two or more of its changes affect, with those changes. They are
    /// the interaction units, what depends on them, and what depends on units different changes
    /// affect: the units whose keys no change gives alone, so the only ones built and tested
    /// together. Every other unit keeps the key it has with its own change alone, or the trunk's.
    pub joint: BTreeMap<String, Vec<String>>,
}

/// What two changes share, so that the later waits for the earlier (W26-R31, R34).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Conflict {
    /// Both change the file.
    File { path: String },
    /// Both affect the unit, whose folder is `dir`.
    Unit { name: String, dir: String },
    /// Both change the deploy target.
    Target { name: String },
}

impl fmt::Display for Conflict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Conflict::File { path } => write!(f, "both change {path}"),
            Conflict::Unit { name, dir } if dir.is_empty() => write!(f, "both affect the root (unit {name})"),
            Conflict::Unit { name, dir } => write!(f, "both affect {dir} (unit {name})"),
            Conflict::Target { name } => write!(f, "both change deploy target {name}"),
        }
    }
}

/// A change's decision against one change ahead of it in the queue (W26-R34).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "decision", rename_all = "kebab-case")]
pub enum Decision {
    /// They share nothing: it lands beside `other`, in either order.
    Beside { other: String },
    /// It lands after `other`: they share `conflicts`, files first, then the units where they meet
    /// (those none of whose dependencies both affect), then deploy targets.
    Waits { other: String, conflicts: Vec<Conflict> },
}

impl fmt::Display for Decision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Decision::Beside { other } => write!(f, "lands beside {other}: no shared unit, file or target"),
            Decision::Waits { other, conflicts } => write!(f, "waits for {other}: {}", said(conflicts)),
        }
    }
}

/// What the queue does with a change (W26-R34): its decision against each change ahead of it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Verdict {
    pub change: String,
    /// One for each change ahead of it, in queue order.
    pub decisions: Vec<Decision>,
}

impl Verdict {
    /// Whether it lands at once: no change ahead of it conflicts with it.
    pub fn at_once(&self) -> bool {
        !self.decisions.iter().any(|d| matches!(d, Decision::Waits { .. }))
    }

    /// The changes ahead of it that it waits for, in queue order.
    pub fn waits_for(&self) -> Vec<&str> {
        self.decisions.iter().filter_map(|d| if let Decision::Waits { other, .. } = d { Some(other.as_str()) } else { None }).collect()
    }

    /// Its decisions in words, in queue order: `waits for D9: both affect crates/kiln-ci (unit
    /// kiln-ci)`, `lands beside D12: no shared unit, file or target`; with no change ahead of it,
    /// `lands at once: nothing is ahead of it in the queue`.
    pub fn why(&self) -> Vec<String> {
        if self.decisions.is_empty() {
            return vec!["lands at once: nothing is ahead of it in the queue".to_owned()];
        }
        self.decisions.iter().map(ToString::to_string).collect()
    }
}

/// A landing class (W26-R33): changes no two of which conflict, which land in any order with one
/// tree and one key for every unit. Its canonical form is the sorted set of their ids, so every
/// order of them is the same class.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Class(pub BTreeSet<String>);

/// The conflict graph of the changes pending in a land queue (W26-R30 to R34).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Conflicts {
    /// Each change, in queue order.
    pub changes: Vec<Pending>,
    /// The components, in the queue order of their first changes.
    pub components: Vec<Component>,
    /// Each change's verdict, in queue order.
    pub verdicts: Vec<Verdict>,
    /// What each pair of conflicting changes shares, by their places in the queue (earlier first).
    pairs: BTreeMap<(usize, usize), Vec<Conflict>>,
}

impl Conflicts {
    /// The conflict graph of `queue` (in queue order) over `units`, which should be the union of
    /// the trunk's graph and each change's own (W26-R30).
    pub fn new(units: &Units, queue: &[Change]) -> Conflicts {
        let mut changes: Vec<Pending> = queue
            .iter()
            .map(|c| {
                let files: BTreeSet<String> = c.files.iter().map(String::as_str).map(clean).filter(|f| !f.is_empty()).map(str::to_owned).collect();
                let takes: BTreeSet<String> = files.iter().flat_map(|f| units.takes(f)).map(str::to_owned).collect();
                let affected = units.with_dependents(takes.clone());
                Pending { id: c.id.clone(), files, takes, units: affected, targets: c.targets.clone(), component: 0 }
            })
            .collect();

        // R31: one set per change; a file, unit or target two changes share joins their sets.
        let mut sets = Sets::new(changes.len());
        {
            let mut first: BTreeMap<(u8, &str), usize> = BTreeMap::new();
            for (i, c) in changes.iter().enumerate() {
                let items = c.files.iter().map(|f| (0, f.as_str())).chain(c.units.iter().map(|u| (1, u.as_str()))).chain(c.targets.iter().map(|t| (2, t.as_str())));
                for item in items {
                    match first.entry(item) {
                        Entry::Vacant(e) => {
                            e.insert(i);
                        }
                        Entry::Occupied(e) => sets.union(*e.get(), i),
                    }
                }
            }
        }
        let mut components: Vec<Component> = Vec::new();
        let mut numbered: BTreeMap<usize, usize> = BTreeMap::new();
        for (i, c) in changes.iter_mut().enumerate() {
            let root = sets.find(i);
            let n = *numbered.entry(root).or_insert_with(|| {
                components.push(Component::default());
                components.len() - 1
            });
            c.component = n;
        }

        // R32: the units that take files from, or that are affected by, two or more of a
        // component's changes.
        for c in &changes {
            if let Some(k) = components.get_mut(c.component) {
                k.changes.push(c.id.clone());
                for u in &c.takes {
                    k.interaction.entry(u.clone()).or_default().push(c.id.clone());
                }
                for u in &c.units {
                    k.joint.entry(u.clone()).or_default().push(c.id.clone());
                }
            }
        }
        for k in &mut components {
            k.interaction.retain(|_, by| by.len() > 1);
            k.joint.retain(|_, by| by.len() > 1);
        }

        // R34: each change against each change ahead of it.
        let mut pairs: BTreeMap<(usize, usize), Vec<Conflict>> = BTreeMap::new();
        let mut verdicts: Vec<Verdict> = Vec::with_capacity(changes.len());
        for (j, b) in changes.iter().enumerate() {
            let mut decisions = Vec::with_capacity(j);
            for (i, a) in changes.iter().enumerate().take(j) {
                let conflicts = if a.component == b.component { shared(units, a, b) } else { Vec::new() };
                if conflicts.is_empty() {
                    decisions.push(Decision::Beside { other: a.id.clone() });
                } else {
                    pairs.insert((i, j), conflicts.clone());
                    decisions.push(Decision::Waits { other: a.id.clone(), conflicts });
                }
            }
            verdicts.push(Verdict { change: b.id.clone(), decisions });
        }
        Conflicts { changes, components, verdicts, pairs }
    }

    /// A pending change, by its id.
    pub fn change(&self, id: &str) -> Option<&Pending> {
        self.changes.iter().find(|c| c.id == id)
    }

    /// The component a change is in.
    pub fn component_of(&self, id: &str) -> Option<&Component> {
        self.change(id).and_then(|c| self.components.get(c.component))
    }

    /// A change's verdict.
    pub fn verdict(&self, id: &str) -> Option<&Verdict> {
        self.verdicts.iter().find(|v| v.change == id)
    }

    /// The landing class of `order` (ids, as they would land) when no two of them conflict
    /// (W26-R33): every order of them gives the same class. Else the first two that conflict, as
    /// the later's decision says it: `D12 waits for D9: both change docs/ci.md`.
    pub fn class(&self, order: &[&str]) -> Result<Class, String> {
        let mut at: BTreeMap<usize, &str> = BTreeMap::new();
        for &id in order {
            let i = self.changes.iter().position(|c| c.id == id).ok_or_else(|| format!("{id} isn't in the queue"))?;
            if at.insert(i, id).is_some() {
                return Err(format!("{id} is given twice"));
            }
        }
        for (&j, later) in &at {
            for (&i, earlier) in at.range(..j) {
                if let Some(conflicts) = self.pairs.get(&(i, j)) {
                    return Err(format!("{later} {}", Decision::Waits { other: (*earlier).to_owned(), conflicts: conflicts.clone() }));
                }
            }
        }
        Ok(Class(at.into_values().map(str::to_owned).collect()))
    }
}

/// What two changes share (W26-R34): the files both change; the units where they meet, those both
/// affect none of whose dependencies both affect (every unit both affect, when a cycle of
/// dependencies leaves none); and the deploy targets both change.
fn shared(units: &Units, a: &Pending, b: &Pending) -> Vec<Conflict> {
    let both: BTreeSet<&str> = a.units.intersection(&b.units).map(String::as_str).collect();
    let meet: Vec<&str> = both.iter().copied().filter(|u| !units.unit(u).is_some_and(|x| x.deps.iter().any(|d| both.contains(d.as_str())))).collect();
    let meet = if meet.is_empty() { both.into_iter().collect() } else { meet };
    let files = a.files.intersection(&b.files).map(|p| Conflict::File { path: p.clone() });
    let at = meet.into_iter().map(|u| Conflict::Unit { name: u.to_owned(), dir: units.unit(u).map(|x| x.dir.clone()).unwrap_or_default() });
    let targets = a.targets.intersection(&b.targets).map(|t| Conflict::Target { name: t.clone() });
    files.chain(at).chain(targets).collect()
}

/// Conflicts in words: the first three, and how many more.
fn said(conflicts: &[Conflict]) -> String {
    let mut words: Vec<String> = conflicts.iter().take(3).map(ToString::to_string).collect();
    if conflicts.len() > 3 {
        words.push(format!("and {} more", conflicts.len() - 3));
    }
    words.join("; ")
}

/// A path as the graph keeps it: from the root, without `./` or a slash at either end.
fn clean(path: &str) -> &str {
    path.trim_start_matches("./").trim_matches('/')
}

/// A union-find over the queue's changes, by place: each set's root is its first change in the
/// queue, so components come out in queue order. Path halving keeps finds short.
#[derive(Debug, Clone)]
struct Sets(Vec<usize>);

impl Sets {
    fn new(n: usize) -> Sets {
        Sets((0..n).collect())
    }

    fn find(&mut self, mut i: usize) -> usize {
        loop {
            let up = self.0.get(i).copied().unwrap_or(i);
            if up == i {
                return i;
            }
            let next = self.0.get(up).copied().unwrap_or(up);
            if let Some(slot) = self.0.get_mut(i) {
                *slot = next;
            }
            i = next;
        }
    }

    fn union(&mut self, a: usize, b: usize) {
        let (ra, rb) = (self.find(a), self.find(b));
        if let Some(slot) = self.0.get_mut(ra.max(rb)) {
            *slot = ra.min(rb);
        }
    }
}

#[cfg(test)]
mod tests;
