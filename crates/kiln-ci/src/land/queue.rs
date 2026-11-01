//! The land queue by component (docs/ci.md W28-R100 to R109), the pure part: what the server's
//! queue (`crates/kiln/src/ci/land.rs`) does next with a trunk's entries, each decision with why.
//! Step 9's third step: the conflict graph ([`super::conflict`], R30 to R34) and the order within
//! components ([`super::order`], R35 to R37) wired into the queue, and what Review needs of it.
//!
//! - **Components** (R100). The entries queued and under test make a conflict graph over the
//!   repository's units ([`units`]); each of its components is a queue of its own with a candidate
//!   of its own, tested beside every other component's. Entries of different components never
//!   wait for each other.
//! - **Candidates** (R101). A component's candidate holds the first of its entries, as many as its
//!   window lets: admission's AIMD window ([`super::order::Speculation`], R36 and R68), capped by
//!   the trunk's `queue.batch`, and a bisection's bound. An entry alone is its own change's
//!   result. Two or more make a joint candidate, which names where they meet; entries that meet no
//!   other change never share one.
//! - **Results stand; bisection stays inside** (R102). A candidate that passed after the trunk
//!   moved for another component lands as tested, applied again onto the trunk's tip, when what
//!   moved the trunk shares no file, unit or target with it; else it's tested again. A failure is
//!   bisected inside its component: alone, its entry is the culprit; else the window halves and
//!   the component's next candidate ends at the middle suspect.
//! - **One move at a time** (R103). The trunk moves by compare-and-swap, one move at a time, each
//!   taking a ready candidate's every entry at once. Lands go in queue order; new candidates in
//!   R35's (the critical path, then the shortest expected service, aged).
//! - **What Review needs**: its gate ([`Gate`], R104), revisions pinned ([`Pin`], R105), a stack's
//!   clear prefix ([`prefix`], R106), a conflict sent back to Review ([`Gate::Conflict`], R107), the
//!   land record ([`Record`], R108), and a host's own queue ([`Route`], [`on_host`], R109).

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::conflict::{Change, Conflicts, Decision, Unit, Units};
use super::order::{Order, Speculation, Waiting};
use crate::sched::admit;
use crate::work::graph::{Ecosystem, Graph};

#[cfg(test)]
mod tests;

/// How decisions name what moved the trunk since a candidate was built (R102).
const SINCE: &str = "what landed since";

// ------------------------------------------------------------------ units

/// The conflict graph's units of package graphs (W28-R100, W24-R16): every package by its name
/// (`npm:ui` where two ecosystems share one), its folder, the packages it depends on, and the
/// files every package of its workspace reads. Give the trunk's graph and each entry's own: a
/// name given twice is one unit, so a dependency one entry adds links it (R30).
pub fn units<'a>(graphs: impl IntoIterator<Item = &'a Graph>) -> Units {
    let graphs: Vec<&Graph> = graphs.into_iter().collect();
    let mut ecosystems: BTreeMap<&str, BTreeSet<Ecosystem>> = BTreeMap::new();
    for p in graphs.iter().flat_map(|g| g.packages.iter()) {
        ecosystems.entry(p.name.as_str()).or_default().insert(p.ecosystem);
    }
    let name = |eco: Ecosystem, n: &str| if ecosystems.get(n).is_some_and(|e| e.len() > 1) { format!("{eco}:{n}") } else { n.to_owned() };
    let list: Vec<Unit> = graphs
        .iter()
        .flat_map(|g| g.packages.iter())
        .map(|p| Unit { name: name(p.ecosystem, &p.name), ecosystem: p.ecosystem.name().to_owned(), dir: p.dir.clone(), reads: p.workspace_files().into_iter().collect(), deps: p.deps.iter().map(|d| name(p.ecosystem, d)).collect() })
        .collect();
    Units::new(list)
}

// ------------------------------------------------------------------ the planner

/// An entry the queue has, queued or under test, as the planner sees it (W28-R100).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Item {
    /// The server's id for it.
    pub id: String,
    /// What it changes, for the conflict graph. Its `id` is how decisions name it (`D3`,
    /// `D1, D2`): one per entry.
    pub change: Change,
    /// The candidate it's in, while it's under test.
    pub batch: Option<String>,
    /// Its component's window, as last kept on it.
    pub window: Option<u32>,
    /// A bisection's bound on its component's next candidate, kept on the component's head.
    pub bisect: Option<u32>,
    /// How long it has waited since it was queued (seconds).
    pub waited_s: i64,
    /// Its check's expected service (seconds; 0 when nothing says).
    pub service_s: i64,
}

/// A candidate under test (W28-R101).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub batch: String,
    /// Its entries' ids, in the order they're applied.
    pub entries: Vec<String>,
    pub outcome: Outcome,
    /// What the trunk changed since the candidate was built on it (R102): `Ok(None)` while the
    /// trunk is where it was, `Ok(Some(files))` once it moved, by those files, and `Err(why)` when
    /// the candidate's base isn't in the trunk's history any more.
    pub since: Result<Option<BTreeSet<String>>, String>,
}

/// How a candidate's checks stand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Running,
    Passed,
    /// A check failed: which, in words (``gate` failure on the candidate`).
    Failed(String),
}

/// One of the planner's decisions; the server carries them out in the order given.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "step", rename_all = "kebab-case")]
pub enum Step {
    /// The candidate passed: its entries land in one compare-and-swap move of the trunk;
    /// `rebased`, applied again onto the trunk's tip, their result standing (R102, R103).
    Land { batch: String, entries: Vec<String>, rebased: bool, window: u32, why: String },
    /// The candidate passed, but what moved the trunk since meets it, or the trunk was rewritten:
    /// its entries are queued again, to be tested on the trunk as it is (R102).
    Retest { batch: String, entries: Vec<String>, why: String },
    /// The candidate failed with one entry: the culprit, out of the queue (R102).
    Eject { batch: String, entry: String, window: u32, why: String },
    /// The candidate failed with more: its entries are queued again, the window halved, and the
    /// component's next candidate holds `next` of them from `head`, ending at the middle suspect.
    Split { batch: String, entries: Vec<String>, head: String, next: u32, window: u32, why: String },
    /// A new candidate of `entries`, a component's first, on the trunk's tip (R100, R101).
    Build { entries: Vec<String>, window: u32, why: String },
}

/// Where an entry stands in its component, kept on it and shown (W28-R108).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Standing {
    /// Its component, by its head's id.
    pub component: String,
    /// Its place in its component, from 1.
    pub position: u32,
    /// Its component's window, after this plan's decisions.
    pub window: u32,
    /// Its decision against each entry ahead of it in the queue (R34).
    pub why: Vec<String>,
}

/// What the queue does next, and where each entry stands.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    pub steps: Vec<Step>,
    /// By entry id.
    pub standings: BTreeMap<String, Standing>,
}

/// The queue's knobs: each component's window and the order of new candidates (R35, R36).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Knobs {
    pub window: admit::Window,
    pub order: Order,
}

impl Knobs {
    /// A trunk's knobs by its `queue.batch` (at least 1): each component's window starts there and
    /// never passes it, so a trunk that tests one change at a time still does, each component
    /// beside the others.
    pub fn of_batch(batch: u32) -> Knobs {
        let cap = batch.max(1);
        Knobs { window: admit::Window { initial: cap, floor: 1, cap }, order: Order::default() }
    }
}

/// What the queue does next with a trunk's entries (W28-R100 to R103): `items` are its entries
/// queued and under test, in queue order; `candidates` those under test, each with how its checks
/// stand and what moved the trunk since it was built. Every decision says why.
pub fn plan(units: &Units, items: &[Item], candidates: &[Candidate], knobs: &Knobs) -> Plan {
    let changes: Vec<Change> = items.iter().map(|i| i.change.clone()).collect();
    let g = Conflicts::new(units, &changes);
    let by_id: BTreeMap<&str, (usize, &Item)> = items.iter().enumerate().map(|(n, i)| (i.id.as_str(), (n, i))).collect();
    let component_of = |id: &str| by_id.get(id).and_then(|(n, _)| g.changes.get(*n)).map(|p| p.component);
    let label = |id: &str| by_id.get(id).map_or_else(|| id.to_owned(), |(_, i)| i.change.id.clone());
    let labels = |ids: &[String]| ids.iter().map(|i| label(i)).collect::<Vec<_>>().join(", ");
    // Each component's window: its oldest entry's that has one (R36).
    let mut windows: Vec<Speculation> = g
        .components
        .iter()
        .map(|k| {
            let kept = k.changes.iter().filter_map(|l| items.iter().find(|i| &i.change.id == l)).find_map(|i| i.window);
            Speculation::new(admit::Window { initial: kept.unwrap_or(knobs.window.initial), ..knobs.window })
        })
        .collect();

    let mut steps = Vec::new();
    let mut busy: BTreeSet<usize> = BTreeSet::new();
    let mut ordered: Vec<&Candidate> = candidates.iter().collect();
    ordered.sort_by_key(|c| c.entries.first().and_then(|e| by_id.get(e.as_str())).map_or(usize::MAX, |(n, _)| *n));
    for c in ordered {
        busy.extend(c.entries.iter().filter_map(|e| component_of(e)));
        let present: Vec<String> = c.entries.iter().filter(|e| by_id.contains_key(e.as_str())).cloned().collect();
        if present.len() < c.entries.len() {
            steps.push(Step::Retest { batch: c.batch.clone(), entries: present.clone(), why: format!("{}: tested again, without what left the queue", labels(&present)) });
            continue;
        }
        let k = c.entries.first().and_then(|e| component_of(e));
        match &c.outcome {
            Outcome::Running => {}
            Outcome::Passed => {
                // R31: an entry lands once every entry ahead of it that it meets has, or lands with it.
                let landable = c.entries.iter().enumerate().all(|(n, e)| {
                    let ahead: BTreeSet<String> = c.entries.iter().take(n).map(|x| label(x)).collect();
                    g.verdict(&label(e)).is_none_or(|v| v.waits_for().iter().all(|w| ahead.contains(*w)))
                });
                if !landable {
                    continue;
                }
                match stands(units, &c.since, c.entries.iter().filter_map(|e| by_id.get(e.as_str()).map(|(_, i)| i.change.clone())).collect()) {
                    Ok(rebased) => {
                        let mut window = 0;
                        if let Some(w) = k.and_then(|k| windows.get_mut(k)) {
                            for _ in &c.entries {
                                w.pass();
                            }
                            window = w.open();
                        }
                        let how = if rebased { "passed, and the trunk moved since only for changes it shares nothing with: its result stands, and it lands as tested" } else { "passed on the trunk's tip: it lands" };
                        steps.push(Step::Land { batch: c.batch.clone(), entries: c.entries.clone(), rebased, window, why: format!("{}: {how}", labels(&c.entries)) });
                    }
                    Err(why) => steps.push(Step::Retest { batch: c.batch.clone(), entries: c.entries.clone(), why: format!("{}: passed, but {why}: tested again on the trunk as it is", labels(&c.entries)) }),
                }
            }
            Outcome::Failed(what) => {
                let w = k.and_then(|k| windows.get_mut(k));
                let window = w.map_or(1, |w| {
                    w.fail();
                    w.open()
                });
                match c.entries.as_slice() {
                    [] => {}
                    [only] => steps.push(Step::Eject { batch: c.batch.clone(), entry: only.clone(), window, why: format!("{}: {what}, alone: the culprit, out of the queue; its component's window is {window}", label(only)) }),
                    [head, ..] => {
                        // R36: every entry of it is a suspect (a check doesn't say which units it
                        // tests), so the next candidate ends at the middle one.
                        let next = u32::try_from((c.entries.len() - 1) / 2 + 1).unwrap_or(1);
                        let why = format!("{}: {what}: bisected inside their component, its next candidate the first {next}; its window halves to {window}", labels(&c.entries));
                        steps.push(Step::Split { batch: c.batch.clone(), entries: c.entries.clone(), head: head.clone(), next, window, why });
                    }
                }
            }
        }
    }

    // New candidates, a component's first entries each, for every component with none under test.
    let mut builds: Vec<(usize, Vec<&Item>)> = Vec::new();
    for (k, comp) in g.components.iter().enumerate() {
        if busy.contains(&k) {
            continue;
        }
        let members: Vec<&Item> = comp.changes.iter().filter_map(|l| items.iter().find(|i| &i.change.id == l)).collect();
        let Some(head) = members.first() else { continue };
        let open = windows.get(k).map_or(1, Speculation::open);
        let n = usize::try_from(open.min(head.bisect.unwrap_or(u32::MAX)).max(1)).unwrap_or(1).min(members.len());
        builds.push((k, members.into_iter().take(n).collect()));
    }
    // R35: the critical path first (each candidate is its component's head's check, so on its
    // critical path), then the shortest expected service, aged by how long its head has waited.
    let waiting: Vec<Waiting> = builds.iter().map(|(_, b)| Waiting { critical: true, service_s: b.iter().map(|i| i.service_s).max().unwrap_or(0), waited_s: b.first().map_or(0, |i| i.waited_s) }).collect();
    for r in knobs.order.rank(&waiting) {
        let Some((k, batch)) = builds.get(r) else { continue };
        let window = windows.get(*k).map_or(1, Speculation::open);
        let ids: Vec<String> = batch.iter().map(|i| i.id.clone()).collect();
        let alone = g.components.get(*k).is_some_and(|c| c.changes.len() == 1);
        let bound = batch.first().and_then(|h| h.bisect).map(|b| format!(", bisecting (the first {b})")).unwrap_or_default();
        let why = match batch.as_slice() {
            [one] if alone => format!("{}: a candidate of its own on the trunk: it meets no other change in the queue", one.change.id),
            [one] => format!("{}: a candidate of its own on the trunk, its component's head (window {window}{bound})", one.change.id),
            _ => format!("{}: one candidate on the trunk (window {window}{bound}), where they meet: {}", labels(&ids), meets(&g, batch)),
        };
        steps.push(Step::Build { entries: ids, window, why });
    }

    let mut standings = BTreeMap::new();
    for (k, comp) in g.components.iter().enumerate() {
        let window = windows.get(k).map_or(1, Speculation::open);
        let members: Vec<&Item> = comp.changes.iter().filter_map(|l| items.iter().find(|i| &i.change.id == l)).collect();
        let head = members.first().map(|i| i.id.clone()).unwrap_or_default();
        for (p, i) in members.iter().enumerate() {
            let why = g.verdict(&i.change.id).map(|v| v.why()).unwrap_or_default();
            standings.insert(i.id.clone(), Standing { component: head.clone(), position: u32::try_from(p + 1).unwrap_or(u32::MAX), window, why });
        }
    }
    Plan { steps, standings }
}

/// Whether a candidate's result stands on the trunk as it is now (W28-R102): `Ok(false)` where it
/// was built, `Ok(true)` when the trunk moved since only for changes it shares nothing with, else
/// why it doesn't. The server asks it again just before a move, the trunk perhaps moved since the
/// plan by another component's.
pub fn stands(units: &Units, since: &Result<Option<BTreeSet<String>>, String>, entries: Vec<Change>) -> Result<bool, String> {
    let files = match since {
        Err(why) => return Err(why.clone()),
        Ok(None) => return Ok(false),
        Ok(Some(files)) => files.clone(),
    };
    let mut queue = vec![Change { id: SINCE.to_owned(), files, targets: BTreeSet::new() }];
    queue.extend(entries);
    let g = Conflicts::new(units, &queue);
    let met: Vec<String> = g.verdicts.iter().skip(1).filter_map(|v| v.decisions.first().filter(|d| matches!(d, Decision::Waits { .. })).map(|d| format!("{} {d}", v.change))).collect();
    if met.is_empty() { Ok(true) } else { Err(format!("the trunk moved since it was built, and {}", met.join("; "))) }
}

/// Where a joint candidate's entries meet: each one's decisions against those ahead of it in the
/// candidate that it waits for (R34's words).
fn meets(g: &Conflicts, batch: &[&Item]) -> String {
    let mut said = Vec::new();
    for (n, i) in batch.iter().enumerate() {
        let ahead: BTreeSet<&str> = batch.iter().take(n).map(|x| x.change.id.as_str()).collect();
        for d in g.verdict(&i.change.id).map(|v| v.decisions.as_slice()).unwrap_or_default() {
            if let Decision::Waits { other, .. } = d
                && ahead.contains(other.as_str())
            {
                said.push(format!("{} {d}", i.change.id));
            }
        }
    }
    if said.is_empty() { "they're linked through the rest of their component".to_owned() } else { said.join("; ") }
}

// ------------------------------------------------------------------ Review's gate

/// Review's word on a diff, or an entry, for landing (W28-R104): the single gate `land`,
/// `land_when_ready` and the queue's re-check before a move ask.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "gate", rename_all = "kebab-case")]
pub enum Gate {
    /// Nothing keeps it: it lands.
    Ready,
    /// It holds, these the reasons.
    Hold { why: Vec<String> },
    /// It conflicts with the trunk in these files: back to Review's conflict ladder (R107), with
    /// Review's own reasons beside them (W26's, each conflict where it falls).
    Conflict {
        files: Vec<String>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        why: Vec<String>,
    },
}

impl Gate {
    /// A diff's gate from its blockers and the files it conflicts with the trunk in: a conflict
    /// keeps the blockers beside it, so Review's words for each conflict are said too.
    pub fn of(blockers: Vec<String>, conflicts: Vec<String>) -> Gate {
        if !conflicts.is_empty() {
            Gate::Conflict { files: conflicts, why: blockers }
        } else if !blockers.is_empty() {
            Gate::Hold { why: blockers }
        } else {
            Gate::Ready
        }
    }

    /// An entry's gate from its diffs', bottom first: a conflict in any sends it back to Review;
    /// else any diff's blockers hold it, each named by its diff.
    pub fn of_entry(diffs: &[(String, Gate)]) -> Gate {
        let files: BTreeSet<String> = diffs.iter().flat_map(|(_, g)| if let Gate::Conflict { files, .. } = g { files.clone() } else { Vec::new() }).collect();
        let why: Vec<String> = diffs
            .iter()
            .flat_map(|(d, g)| match g {
                Gate::Hold { why } | Gate::Conflict { why, .. } => why.iter().map(|w| format!("{d}: {w}")).collect(),
                Gate::Ready => Vec::new(),
            })
            .collect();
        if !files.is_empty() {
            return Gate::Conflict { files: files.into_iter().collect(), why };
        }
        if why.is_empty() { Gate::Ready } else { Gate::Hold { why } }
    }

    pub fn ready(&self) -> bool {
        *self == Gate::Ready
    }

    /// Why it doesn't land, in words.
    pub fn reason(&self) -> Option<String> {
        match self {
            Gate::Ready => None,
            Gate::Hold { why } => Some(why.join("; ")),
            Gate::Conflict { files, why } => Some(std::iter::once(conflicts_in(files)).chain(why.iter().cloned()).collect::<Vec<_>>().join("; ")),
        }
    }
}

/// A conflict with the trunk in words: the first five files, and how many more.
pub fn conflicts_in(files: &[String]) -> String {
    let shown: Vec<&str> = files.iter().take(5).map(String::as_str).collect();
    let more = if files.len() > 5 { format!(" and {} more", files.len() - 5) } else { String::new() };
    format!("needs rebase: conflicts with the trunk in {}{more}", shown.join(", "))
}

// ------------------------------------------------------------------ pinned revisions

/// A revision an entry lands (W28-R105): the version of a diff Review approved, by its commit,
/// the commit below it, and its patch-id. The candidate tested is this revision onto the trunk.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Pin {
    /// `D3`, `CR4`.
    pub diff: String,
    pub version: u32,
    pub commit: String,
    pub parent: String,
    pub patch_id: String,
}

/// A diff's version as kept.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Revision {
    pub version: u32,
    pub commit: String,
    pub parent: String,
    pub patch_id: String,
    /// It only rebased the one before it.
    pub rebase_only: bool,
}

/// Whether `pin` still names what its entry lands, given its diff's revisions (W28-R105): a newer
/// revision invalidates it unless it only rebased, with the same patch-id. Else why not (said of
/// the diff: `it has v2 since v1 was approved, …`).
pub fn pin_holds(pin: &Pin, revisions: &[Revision]) -> Result<(), String> {
    if !revisions.iter().any(|r| r.version == pin.version) {
        return Err(format!("its v{}, approved, is gone", pin.version));
    }
    for r in revisions.iter().filter(|r| r.version > pin.version) {
        let same = r.patch_id.is_empty() || pin.patch_id.is_empty() || r.patch_id == pin.patch_id;
        if !(r.rebase_only && same) {
            return Err(format!("it has v{} since v{} was approved, which changes its patch: v{} lands once Review approves it", r.version, pin.version, r.version));
        }
    }
    Ok(())
}

// ------------------------------------------------------------------ a stack's clear prefix

/// What of a stack lands now (W28-R106): its clear diffs from the bottom, in order, and the rest,
/// each holding with why.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Prefix {
    pub clear: Vec<String>,
    pub held: Vec<(String, Vec<String>)>,
}

/// A stack's diffs, bottom first, each with its own gate (W28-R106): the clear lower diffs land in
/// order; the first that isn't holds for its reasons, and every diff above it holds for it too.
pub fn prefix(stack: &[(String, Gate)]) -> Prefix {
    let mut out = Prefix::default();
    let mut below: Option<String> = None;
    for (d, g) in stack {
        // Each reason its own, so each is said with its diff's name (`D1: …; D1: …`), as Review
        // says a diff's blockers.
        let why: Vec<String> = match g {
            Gate::Ready => Vec::new(),
            Gate::Hold { why } => why.clone(),
            Gate::Conflict { files, why } => std::iter::once(conflicts_in(files)).chain(why.iter().cloned()).collect(),
        };
        match &below {
            None if g.ready() => out.clear.push(d.clone()),
            None => {
                out.held.push((d.clone(), why));
                below = Some(d.clone());
            }
            Some(b) => out.held.push((d.clone(), std::iter::once(format!("{b}, below it, holds")).chain(why).collect())),
        }
    }
    out
}

// ------------------------------------------------------------------ the land record

/// A run of a candidate's checks.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CheckRun {
    pub run: String,
    pub workflow: String,
    pub conclusion: Option<String>,
}

/// What a land keeps (W28-R108): the change and its kind, the revisions approved with their
/// patch-ids, the candidate tested and its checks' runs, the commits landed, and the trunk's
/// policy it landed under.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Record {
    /// Its diffs (`D3`), bottom first, or its change request (`CR4`), or its host's changes.
    pub changes: Vec<String>,
    /// `diff`, `stack`, `change_request`, or `host` for a host's changes.
    pub kind: String,
    pub strategy: String,
    /// The revisions Review approved, each with its patch-id.
    pub revisions: Vec<Pin>,
    /// The candidate tested, the trunk it was built on, and its checks' runs.
    pub candidate: Option<String>,
    pub base: Option<String>,
    pub runs: Vec<CheckRun>,
    /// Landed as tested onto a trunk that moved since, its result standing (R102).
    pub rebased: bool,
    /// The trunk before and after its move, and the commit that carries each change.
    pub from: String,
    pub to: String,
    pub landed: BTreeMap<String, String>,
    /// The trunk's policy it landed under (`.kiln/review.yaml`).
    pub policy: super::Policy,
    /// Who asked it to land, and which queue landed it: `kiln`, or its host's.
    pub by: String,
    pub queue: String,
}

// ------------------------------------------------------------------ a host's own queue

/// Which queue lands a repository's changes (W28-R109): kiln's (W28's, this module's) where the
/// forge seam says its host has no queue for it, else the host's own (GitHub's merge queue,
/// GitLab's merge trains), followed through the seam.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    Kiln,
    Host { queue: String },
}

impl Route {
    /// The route `abilities(repo).queue` gives: its host queue's name, if it has one.
    pub fn of(host_queue: Option<&str>) -> Route {
        match host_queue {
            None => Route::Kiln,
            Some(q) => Route::Host { queue: q.to_owned() },
        }
    }

    pub fn name(&self) -> &str {
        match self {
            Route::Kiln => "kiln",
            Route::Host { queue } => queue,
        }
    }
}

/// Where a host's change is, as read back through the seam.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostState {
    Open,
    Merged { commit: String },
    Closed,
}

/// One of an entry's changes on its host: its number, whether kiln has put it in the host's queue
/// (or seen it there), whether the host's queue holds it now, and where it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OnHost {
    pub number: u64,
    pub entered: bool,
    pub queued: bool,
    pub state: HostState,
}

/// How an entry stands on its host's queue (W28-R109).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostVerdict {
    /// Every change merged: landed, each by its commit.
    Landed { commits: Vec<(u64, String)> },
    /// In the host's queue still, or partly merged.
    Waiting,
    /// Out of the host's queue unmerged: ejected or dequeued there, or closed.
    Out { number: u64, why: String },
}

/// How an entry's changes (bottom first) stand on their host's queue (W28-R109): `Merged` means
/// landed; open and not queued, once it was put there, means the host ejected or dequeued it.
pub fn on_host(changes: &[OnHost]) -> HostVerdict {
    let mut commits = Vec::new();
    for c in changes {
        match &c.state {
            HostState::Merged { commit } => commits.push((c.number, commit.clone())),
            HostState::Closed => return HostVerdict::Out { number: c.number, why: format!("#{} was closed on its host, unmerged", c.number) },
            HostState::Open if c.entered && !c.queued => return HostVerdict::Out { number: c.number, why: format!("#{} left its host's queue unmerged: ejected or dequeued there", c.number) },
            HostState::Open => {}
        }
    }
    if !changes.is_empty() && commits.len() == changes.len() { HostVerdict::Landed { commits } } else { HostVerdict::Waiting }
}

/// The next of an entry's changes (bottom first) to put in its host's queue: the first open one
/// not put there yet. On GitHub a stacked change's enqueue queues the changes below it too, so the
/// server reads the queue back after each, and marks what it holds.
pub fn next_to_enqueue(changes: &[OnHost]) -> Option<u64> {
    changes.iter().find(|c| !c.entered && !c.queued && c.state == HostState::Open).map(|c| c.number)
}
