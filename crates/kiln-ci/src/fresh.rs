//! Keeping open changes fresh as main moves (docs/ci.md W26-R16), the pure part. When a trunk moves,
//! kiln replays each open change onto it in its own copy, stack by stack, bottom first: a change
//! goes onto the trunk's new tip, or onto the replay of the open change below it. A burst of moves
//! is one move, from where the trunk was to where it is now ([`collapse`]); a change already on the
//! tip, with nothing below it replayed, stays as it is ([`plan`]). A clean replay is a rebase-only
//! revision (why: [`WHY`]), so approvals stay; a conflicted one is kept as a conflicted revision
//! and goes up the conflict ladder (W26-R17), and the changes above it replay onto that revision,
//! as jj's do: a conflict never holds up the stack above it ([`Plan::above_conflict`] says which
//! conflict each sits on). Nothing is pushed and no agent is sent unless `.kiln/review.yaml`'s
//! `fresh:` says so ([`then`]), and turning automatic
//! pushes on says plainly what it costs ([`auto_push_warning`]). The collision radar names two open
//! changes that change the same lines of a file, on the trunk's lines, once for each pair and file
//! ([`collisions`]); and a change says how fresh it is in words ([`Freshness::words`]). The server
//! reads git and the records, replays, and does what this says.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

/// The why of a revision kiln replays (W26-R04).
pub const WHY: &str = "replayed";

/// The why of a conflicted replay kept as a revision (its conflict markers in), for the changes above
/// it to replay onto and for the conflict ladder (W26-R17) to resolve.
pub const WHY_CONFLICTED: &str = "replayed with conflicts";

/// The most changes one plan walks through a stack; a stack deeper than this (or one that loops)
/// is refused, never followed for ever.
const MAX_DEPTH: usize = 1000;

/// `fresh.push` of `.kiln/review.yaml`: whether replays are pushed to the changes' branches.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Push {
    /// Only when someone presses Update.
    #[default]
    Manual,
    /// Each time the trunk moves, every open change's replay.
    Auto,
}

/// `fresh:` of `.kiln/review.yaml`. Replays in kiln's own copy are on; pushing them and sending
/// agents are off until a repository declares them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct Settings {
    /// Open changes are replayed onto the trunk as it moves, in kiln's own copy (`false`: they stay
    /// where their authors put them, until they rebase).
    pub replay: bool,
    pub push: Push,
    /// A conflicting replay sends an agent to a runner that has the repository, to resolve it there
    /// (its resolution still goes up the conflict ladder, W26-R17).
    pub conflict_agents: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { replay: true, push: Push::Manual, conflict_agents: false }
    }
}

/// What turning automatic pushes on means, in plain words: shown on the change that turns it on and
/// on the repository's page while it's on.
pub fn auto_push_warning(trunk: &str) -> String {
    format!(
        "Automatic pushes are on: every open change's branch is pushed each time {trunk} moves. On GitHub, GitLab and Bitbucket that means heavy API use and many more CI runs, and everyone with one of those branches checked out has to pull or rebase before they push."
    )
}

/// Whether a change turns automatic pushes on: its `.kiln/review.yaml` says `push: auto` where the
/// trunk's doesn't.
pub fn turns_auto_push_on(trunk: &Settings, change: &Settings) -> bool {
    trunk.push != Push::Auto && change.push == Push::Auto
}

/// The gate on a change that turns automatic pushes on (W26-R11's, with W26-R16's warning): approving
/// it warns, in plain words, what turning them on means; a gate that holds still holds.
pub fn warn(mut gate: crate::review::Gate, warning: &str) -> crate::review::Gate {
    use crate::review::Verdict;
    if matches!(gate.verdict, Verdict::Clear | Verdict::Inform) {
        gate.verdict = Verdict::Warn;
    }
    gate.warning = Some(match gate.warning.take() {
        Some(w) => format!("{w}; {warning}"),
        None => warning.to_owned(),
    });
    gate
}

/// A trunk moved: from `before` to `after`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Move {
    pub trunk: String,
    pub before: String,
    pub after: String,
}

/// `moves` as one per trunk, in the order each trunk first moved: from where it was before its first
/// to where it is after its last. A trunk that ended where it began didn't move.
pub fn collapse(moves: &[Move]) -> Vec<Move> {
    let mut out: Vec<Move> = Vec::new();
    for m in moves {
        match out.iter_mut().find(|o| o.trunk == m.trunk) {
            Some(o) => o.after.clone_from(&m.after),
            None => out.push(m.clone()),
        }
    }
    out.retain(|m| m.before != m.after);
    out
}

/// What kind of change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A kiln diff: one commit, maybe on another open diff.
    Diff,
    /// A kiln change request: a branch's commits.
    Request,
    /// A host's pull request (GitHub, GitLab, Bitbucket).
    Pull,
}

/// An open change as the planner sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// How kiln names it: `D40`, `pull:github:488`.
    pub id: String,
    pub kind: Kind,
    pub trunk: String,
    /// The open change below it in its stack, if any.
    pub below: Option<String>,
    /// The trunk commit its latest revision (or the last replay kept of it) is on.
    pub on: String,
    /// Its latest revision is on the latest revision of the change below it (none below: true). A
    /// change whose below got a new revision since (a resolution, a push) is replayed onto it.
    pub on_below: bool,
}

/// Where a replay goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Onto {
    /// The trunk's tip.
    Tip(String),
    /// The replay of the open change below it.
    Below(String),
}

/// One change to replay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Replay {
    pub id: String,
    pub onto: Onto,
}

/// What to replay after a move, in order, and what's left as it is.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    /// Stack by stack, bottom first: each change after the one it goes onto.
    pub replays: Vec<Replay>,
    /// On the tip already, with nothing below it replayed.
    pub current: Vec<String>,
    /// Changes that can't be planned, and why (a stack that loops, or deeper than kiln follows).
    pub refused: Vec<(String, String)>,
}

impl Plan {
    /// The conflicted replay under change `id` in its stack, given the replays that conflicted: the
    /// nearest one below it (`None`: it sits on clean replays, or on the tip). It still replays, onto
    /// that kept conflicted revision; this is what it says it sits on.
    pub fn above_conflict(&self, id: &str, conflicted: &BTreeSet<String>) -> Option<String> {
        let below: BTreeMap<&str, &str> = self
            .replays
            .iter()
            .filter_map(|r| match &r.onto {
                Onto::Below(b) => Some((r.id.as_str(), b.as_str())),
                Onto::Tip(_) => None,
            })
            .collect();
        let mut at = below.get(id).copied();
        let mut steps = 0usize;
        while let Some(c) = at {
            steps += 1;
            if steps > MAX_DEPTH {
                break;
            }
            if conflicted.contains(c) {
                return Some(c.to_owned());
            }
            at = below.get(c).copied();
        }
        None
    }
}

/// What to replay now that each trunk is at `tips` (trunk → its tip): every open change of a trunk
/// that moved, stack by stack, bottom first, each in the order `changes` gives the stacks' bottoms.
/// A stack's bottom goes onto the tip unless it's on it already; a change above goes onto the one
/// below it (its replay this round, else its latest revision) when that one was replayed or it isn't
/// on its latest revision, and otherwise stays as it is. A change whose trunk has no tip here isn't
/// planned.
pub fn plan(tips: &BTreeMap<String, String>, changes: &[Change]) -> Plan {
    let by_id: BTreeMap<&str, &Change> = changes.iter().map(|c| (c.id.as_str(), c)).collect();
    let mut above: BTreeMap<&str, Vec<&Change>> = BTreeMap::new();
    for c in changes {
        if let Some(b) = c.below.as_deref().filter(|b| by_id.contains_key(b)) {
            above.entry(b).or_default().push(c);
        }
    }
    let mut out = Plan::default();
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    // Bottoms first, in the order given: a change whose below isn't open is a stack's bottom.
    for bottom in changes.iter().filter(|c| c.below.as_deref().is_none_or(|b| !by_id.contains_key(b))) {
        let Some(tip) = tips.get(&bottom.trunk) else { continue };
        // (the change, whether the change below it is replayed, depth)
        let mut work: Vec<(&Change, bool, usize)> = vec![(bottom, false, 0)];
        while let Some((c, below_replayed, depth)) = work.pop() {
            if !seen.insert(c.id.as_str()) {
                out.refused.push((c.id.clone(), format!("{} is in its stack twice: the stack loops", c.id)));
                continue;
            }
            if depth >= MAX_DEPTH {
                out.refused.push((c.id.clone(), format!("{}'s stack is deeper than {MAX_DEPTH} changes", c.id)));
                continue;
            }
            let below = c.below.clone().filter(|b| by_id.contains_key(b.as_str()));
            let replayed = if let Some(b) = below.filter(|_| below_replayed || !c.on_below) {
                out.replays.push(Replay { id: c.id.clone(), onto: Onto::Below(b) });
                true
            } else if c.below.as_deref().is_none_or(|b| !by_id.contains_key(b)) && c.on != *tip {
                out.replays.push(Replay { id: c.id.clone(), onto: Onto::Tip(tip.clone()) });
                true
            } else {
                out.current.push(c.id.clone());
                false
            };
            // Pushed in reverse so the first one above is walked first.
            for a in above.get(c.id.as_str()).into_iter().flatten().rev() {
                work.push((a, replayed, depth + 1));
            }
        }
    }
    // Whatever wasn't reached from a bottom sits on a loop.
    for c in changes {
        if tips.contains_key(&c.trunk) && !seen.contains(c.id.as_str()) {
            out.refused.push((c.id.clone(), format!("{}'s stack loops: no change in it is a bottom", c.id)));
        }
    }
    out
}

/// How a replay went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Clean,
    Conflicted,
}

/// What follows a replay, as `settings` say. Either way it's a new revision, which the changes above
/// it replay onto.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Then {
    /// The revision is rebase-only, its why [`WHY`] (a clean replay), so approvals stay; a
    /// conflicted one is kept as a conflicted revision, which they don't.
    pub rebase_only: bool,
    /// Push it to the change's branch now (`push: auto`, a clean replay of a change with a branch).
    pub push: bool,
    /// It goes up the conflict ladder (W26-R17).
    pub ladder: bool,
    /// Send an agent to a runner that has the repository (`conflict-agents`, a conflicted replay).
    pub agent: bool,
}

/// What follows `outcome` for a change with a branch to push (`branch`: a change request's, a
/// pull request's; a kiln diff has none).
pub fn then(settings: Settings, outcome: Outcome, branch: bool) -> Then {
    match outcome {
        Outcome::Clean => Then { rebase_only: true, push: branch && settings.push == Push::Auto, ladder: false, agent: false },
        Outcome::Conflicted => Then { rebase_only: false, push: false, ladder: true, agent: settings.conflict_agents },
    }
}

/// Lines of a file an open change changes, on the trunk's lines (`start..=end`; a change that only
/// adds lines touches the line it adds them after).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Touch {
    pub change: String,
    /// Its stack's bottom: changes in one stack overlap by design, and aren't a collision.
    pub stack: String,
    pub file: String,
    pub start: u32,
    pub end: u32,
}

/// Two open changes that change the same lines of `file` (`a` before `b`), and the lines both change.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Collision {
    pub a: String,
    pub b: String,
    pub file: String,
    pub start: u32,
    pub end: u32,
}

impl Collision {
    /// What a collision is told by: its pair and file, so lines that shift as the trunk moves don't
    /// tell anyone twice.
    pub fn key(&self) -> (String, String, String) {
        (self.a.clone(), self.b.clone(), self.file.clone())
    }
}

/// Each pair of open changes from different stacks that change the same lines of a file, not yet
/// told (`told`: keys already told): one per pair and file, with the first lines both change.
pub fn collisions(touches: &[Touch], told: &BTreeSet<(String, String, String)>) -> Vec<Collision> {
    let mut by_file: BTreeMap<&str, Vec<&Touch>> = BTreeMap::new();
    for t in touches {
        by_file.entry(t.file.as_str()).or_default().push(t);
    }
    let mut found: BTreeMap<(String, String, String), Collision> = BTreeMap::new();
    for (file, ts) in by_file {
        for (i, x) in ts.iter().enumerate() {
            for y in ts.iter().skip(i + 1) {
                if x.change == y.change || x.stack == y.stack || x.start > y.end || y.start > x.end {
                    continue;
                }
                let (a, b) = if x.change < y.change { (x, y) } else { (y, x) };
                let c = Collision { a: a.change.clone(), b: b.change.clone(), file: file.to_owned(), start: x.start.max(y.start), end: x.end.min(y.end) };
                if told.contains(&c.key()) {
                    continue;
                }
                found.entry(c.key()).and_modify(|k| if (c.start, c.end) < (k.start, k.end) { *k = c.clone() }).or_insert(c);
            }
        }
    }
    found.into_values().collect()
}

/// The lines each file's hunks change on the old side of a unified diff (`git diff -U0`): `(file,
/// start, end)`. A hunk that only adds lines touches the line it adds them after (line 1 for the
/// top of a file); a file that's new is named by its new path.
pub fn old_lines(unified: &str) -> Vec<(String, u32, u32)> {
    let mut out = Vec::new();
    let mut file: Option<String> = None;
    let mut old: Option<String> = None;
    for line in unified.lines() {
        if let Some(p) = line.strip_prefix("--- ") {
            old = (p != "/dev/null").then(|| p.strip_prefix("a/").unwrap_or(p).to_owned());
            file = None;
        } else if let Some(p) = line.strip_prefix("+++ ") {
            file = old.take().or_else(|| (p != "/dev/null").then(|| p.strip_prefix("b/").unwrap_or(p).to_owned()));
        } else if let Some(rest) = line.strip_prefix("@@ -")
            && let Some(f) = &file
        {
            let range = rest.split_whitespace().next().unwrap_or_default();
            let (start, count) = match range.split_once(',') {
                Some((s, c)) => (s.parse::<u32>().ok(), c.parse::<u32>().ok()),
                None => (range.parse::<u32>().ok(), Some(1)),
            };
            let (Some(start), Some(count)) = (start, count) else { continue };
            let start = start.max(1);
            out.push((f.clone(), start, start.saturating_add(count.saturating_sub(1))));
        }
    }
    out
}

/// How fresh a change is on its trunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Freshness {
    /// Its latest revision is on the tip.
    Current { tip: String },
    /// Replayed onto the tip cleanly.
    Clean { tip: String },
    /// Its replay onto the tip conflicts.
    Conflicted { tip: String },
    /// Being replayed onto the tip.
    Replaying { tip: String },
    /// Replayed cleanly onto the conflicted revision of the change below it.
    AboveConflict { below: String, tip: String },
}

fn short(sha: &str) -> &str {
    sha.get(..6).unwrap_or(sha)
}

impl Freshness {
    /// In words, as the change page and its row say it: `replays clean on main@4c1e2a`.
    pub fn words(&self, trunk: &str) -> String {
        match self {
            Freshness::Current { tip } => format!("on {trunk}@{}", short(tip)),
            Freshness::Clean { tip } => format!("replays clean on {trunk}@{}", short(tip)),
            Freshness::Conflicted { tip } => format!("conflicts on {trunk}@{}", short(tip)),
            Freshness::Replaying { tip } => format!("replaying onto {trunk}@{}", short(tip)),
            Freshness::AboveConflict { below, tip } => format!("replays clean on {below}'s conflicts on {trunk}@{}", short(tip)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OLD: &str = "9b1f00000000";
    const TIP: &str = "5d0e7b000000";

    fn diff(id: &str, below: Option<&str>, on: &str) -> Change {
        Change { id: id.into(), kind: Kind::Diff, trunk: "main".into(), below: below.map(Into::into), on: on.into(), on_below: true }
    }

    fn tips() -> BTreeMap<String, String> {
        BTreeMap::from([("main".to_owned(), TIP.to_owned())])
    }

    fn order(p: &Plan) -> Vec<(&str, Onto)> {
        p.replays.iter().map(|r| (r.id.as_str(), r.onto.clone())).collect()
    }

    #[test]
    fn w26_r16_replays_stack_by_stack_bottom_first() {
        // D40 ← D41 ← D42, and #488 beside them; given top first, so the order isn't the input's.
        let changes = [
            diff("D42", Some("D41"), OLD),
            diff("D41", Some("D40"), OLD),
            diff("D40", None, OLD),
            Change { id: "pull:github:488".into(), kind: Kind::Pull, trunk: "main".into(), below: None, on: OLD.into(), on_below: true },
        ];
        let p = plan(&tips(), &changes);
        assert_eq!(
            order(&p),
            [
                ("D40", Onto::Tip(TIP.into())),
                ("D41", Onto::Below("D40".into())),
                ("D42", Onto::Below("D41".into())),
                ("pull:github:488", Onto::Tip(TIP.into())),
            ]
        );
        assert!(p.current.is_empty() && p.refused.is_empty());
    }

    #[test]
    fn w26_r16_a_branching_stack_replays_each_branch_after_its_base() {
        let changes = [diff("D40", None, OLD), diff("D41", Some("D40"), OLD), diff("D50", Some("D40"), OLD), diff("D42", Some("D41"), OLD)];
        let p = plan(&tips(), &changes);
        let ids: Vec<&str> = p.replays.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["D40", "D41", "D42", "D50"]);
    }

    #[test]
    fn w26_r16_a_change_on_the_tip_stays_unless_below_it_moved() {
        // D40 is on the tip already; D41 above it too: nothing to replay. D60 on the old tip moves.
        let changes = [diff("D40", None, TIP), diff("D41", Some("D40"), TIP), diff("D60", None, OLD)];
        let p = plan(&tips(), &changes);
        assert_eq!(order(&p), [("D60", Onto::Tip(TIP.into()))]);
        assert_eq!(p.current, ["D40", "D41"]);
        // D40 moved: D41 above it replays onto it, though D41 was on the tip.
        let changes = [diff("D40", None, OLD), diff("D41", Some("D40"), TIP)];
        assert_eq!(order(&plan(&tips(), &changes)), [("D40", Onto::Tip(TIP.into())), ("D41", Onto::Below("D40".into()))]);
    }

    #[test]
    fn w26_r16_a_change_whose_below_moved_replays_onto_it() {
        // D40 got a new revision (a resolution) and is on the tip; D41 is still on D40's old one.
        let mut d41 = diff("D41", Some("D40"), TIP);
        d41.on_below = false;
        let p = plan(&tips(), &[diff("D40", None, TIP), d41, diff("D42", Some("D41"), TIP)]);
        assert_eq!(order(&p), [("D41", Onto::Below("D40".into())), ("D42", Onto::Below("D41".into()))]);
        assert_eq!(p.current, ["D40"]);
    }

    #[test]
    fn w26_r16_a_change_of_another_trunk_isnt_planned() {
        let mut other = diff("D70", None, OLD);
        other.trunk = "release".into();
        let p = plan(&tips(), &[other]);
        assert_eq!(p, Plan::default());
    }

    #[test]
    fn w26_r16_a_looping_stack_is_refused_not_followed() {
        let changes = [diff("D1", Some("D2"), OLD), diff("D2", Some("D1"), OLD)];
        let p = plan(&tips(), &changes);
        assert!(p.replays.is_empty());
        assert_eq!(p.refused.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(), ["D1", "D2"]);
        assert!(p.refused.iter().all(|(_, why)| why.contains("loops")));
    }

    #[test]
    fn w26_r16_a_burst_of_moves_replays_once() {
        let m = |t: &str, b: &str, a: &str| Move { trunk: t.into(), before: b.into(), after: a.into() };
        let moves = [m("main", "a", "b"), m("release", "r", "s"), m("main", "b", "c"), m("main", "c", "d"), m("release", "s", "r")];
        assert_eq!(collapse(&moves), [m("main", "a", "d")]);
        assert!(collapse(&[]).is_empty());
    }

    #[test]
    fn w26_r16_nothing_pushed_unless_auto() {
        let manual = Settings::default();
        let auto = Settings { push: Push::Auto, ..Settings::default() };
        assert!(!then(manual, Outcome::Clean, true).push);
        assert!(then(auto, Outcome::Clean, true).push);
        // A kiln diff has no branch to push; a conflicted replay is never pushed.
        assert!(!then(auto, Outcome::Clean, false).push);
        assert!(!then(auto, Outcome::Conflicted, true).push);
        // A clean replay is rebase-only; a conflicted one isn't.
        assert!(then(manual, Outcome::Clean, false).rebase_only);
        assert!(!then(manual, Outcome::Conflicted, false).rebase_only);
    }

    #[test]
    fn w26_r16_conflict_agents_off_by_default() {
        let off = then(Settings::default(), Outcome::Conflicted, true);
        assert_eq!(off, Then { rebase_only: false, push: false, ladder: true, agent: false });
        let on = Settings { conflict_agents: true, ..Settings::default() };
        assert!(then(on, Outcome::Conflicted, false).agent);
        assert!(!then(on, Outcome::Clean, false).agent);
    }

    #[test]
    fn w26_r16_above_a_conflict_still_replays_onto_it() {
        let changes = [diff("D40", None, OLD), diff("D41", Some("D40"), OLD), diff("D42", Some("D41"), OLD), diff("D43", Some("D42"), OLD)];
        let p = plan(&tips(), &changes);
        // The plan is the same whatever conflicts: each change onto the one below's revision.
        assert_eq!(p.replays.len(), 4);
        let conflicted = BTreeSet::from(["D41".to_owned()]);
        assert_eq!(p.above_conflict("D40", &conflicted), None);
        assert_eq!(p.above_conflict("D41", &conflicted), None, "D41 itself sits on D40's clean replay");
        assert_eq!(p.above_conflict("D42", &conflicted).as_deref(), Some("D41"));
        assert_eq!(p.above_conflict("D43", &conflicted).as_deref(), Some("D41"));
        // Two conflicts: a change sits on the nearest.
        let both = BTreeSet::from(["D41".to_owned(), "D42".to_owned()]);
        assert_eq!(p.above_conflict("D43", &both).as_deref(), Some("D42"));
    }

    fn touch(change: &str, stack: &str, file: &str, start: u32, end: u32) -> Touch {
        Touch { change: change.into(), stack: stack.into(), file: file.into(), start, end }
    }

    #[test]
    fn w26_r16_collision_radar_names_both_once() {
        let touches = [
            touch("D44", "D44", "sched/fair.rs", 40, 58),
            touch("pull:github:488", "pull:github:488", "sched/fair.rs", 45, 70),
            touch("pull:github:488", "pull:github:488", "sched/fair.rs", 50, 52),
            // Another file; and lines next to but not on #488's.
            touch("D45", "D45", "sched/core.rs", 1, 9),
            touch("D46", "D46", "sched/fair.rs", 71, 72),
        ];
        let found = collisions(&touches, &BTreeSet::new());
        assert_eq!(found, [Collision { a: "D44".into(), b: "pull:github:488".into(), file: "sched/fair.rs".into(), start: 45, end: 58 }]);
        // Once told, the same pair and file isn't told again, even as its lines shift.
        let told: BTreeSet<_> = found.iter().map(Collision::key).collect();
        let shifted = [touch("D44", "D44", "sched/fair.rs", 42, 60), touch("pull:github:488", "pull:github:488", "sched/fair.rs", 47, 72)];
        assert!(collisions(&shifted, &told).is_empty());
        // A new file both change is a new overlap.
        let more = [touch("D44", "D44", "sched/rt.rs", 3, 3), touch("pull:github:488", "pull:github:488", "sched/rt.rs", 3, 4)];
        assert_eq!(collisions(&more, &told).len(), 1);
    }

    #[test]
    fn w26_r16_one_stack_isnt_a_collision() {
        let touches = [touch("D40", "D40", "a.rs", 1, 10), touch("D41", "D40", "a.rs", 5, 6)];
        assert!(collisions(&touches, &BTreeSet::new()).is_empty());
    }

    #[test]
    fn w26_r16_old_lines_reads_each_hunk_on_the_trunks_lines() {
        let diff = "diff --git a/sched/fair.rs b/sched/fair.rs\n--- a/sched/fair.rs\n+++ b/sched/fair.rs\n@@ -40,19 +40,20 @@ fn pick()\n-x\n+y\n@@ -90 +91 @@\n-a\n+b\n@@ -100,0 +102,2 @@\n+c\ndiff --git a/new.rs b/new.rs\nnew file mode 100644\n--- /dev/null\n+++ b/new.rs\n@@ -0,0 +1,3 @@\n+n\n";
        assert_eq!(
            old_lines(diff),
            [("sched/fair.rs".to_owned(), 40, 58), ("sched/fair.rs".to_owned(), 90, 90), ("sched/fair.rs".to_owned(), 100, 100), ("new.rs".to_owned(), 1, 1)]
        );
        assert!(old_lines("").is_empty());
        assert!(old_lines("--- a/x\n+++ b/x\n@@ -nonsense @@\n").is_empty());
    }

    #[test]
    fn w26_r16_freshness_words() {
        let tip = "4c1e2a9f0b".to_owned();
        assert_eq!(Freshness::Clean { tip: tip.clone() }.words("main"), "replays clean on main@4c1e2a");
        assert_eq!(Freshness::Conflicted { tip: tip.clone() }.words("main"), "conflicts on main@4c1e2a");
        assert_eq!(Freshness::Replaying { tip: tip.clone() }.words("main"), "replaying onto main@4c1e2a");
        assert_eq!(Freshness::Current { tip: tip.clone() }.words("main"), "on main@4c1e2a");
        assert_eq!(Freshness::AboveConflict { below: "D42".into(), tip: tip.clone() }.words("main"), "replays clean on D42's conflicts on main@4c1e2a");
        assert_eq!(Freshness::Clean { tip: "ab".into() }.words("main"), "replays clean on main@ab");
    }

    #[test]
    fn w26_r16_policy_parses_and_defaults_off() -> Result<(), String> {
        use crate::land::Policy;
        assert_eq!(Policy::parse("")?.fresh, Settings { replay: true, push: Push::Manual, conflict_agents: false });
        assert_eq!(Policy::parse("land:\n  approvals: 2\n")?.fresh, Settings::default());
        let on = Policy::parse("fresh:\n  push: auto\n  conflict-agents: true\n")?;
        assert_eq!(on.fresh, Settings { replay: true, push: Push::Auto, conflict_agents: true });
        assert!(!Policy::parse("fresh:\n  replay: false\n")?.fresh.replay);
        assert!(Policy::parse("fresh:\n  push: sometimes\n").is_err());
        Ok(())
    }

    #[test]
    fn w26_r16_turning_auto_push_on_warns_on_approval() {
        let manual = Settings::default();
        let auto = Settings { push: Push::Auto, ..Settings::default() };
        assert!(turns_auto_push_on(&manual, &auto));
        assert!(!turns_auto_push_on(&auto, &auto), "already on: nothing turns it on");
        assert!(!turns_auto_push_on(&manual, &manual));
        let w = auto_push_warning("main");
        let clear = crate::review::gate(&[], &[], None);
        let g = warn(clear, &w);
        assert_eq!(g.verdict, crate::review::Verdict::Warn);
        assert_eq!(g.approve(false), Err(format!("{w}: approve anyway with `force`")));
        assert!(g.approve(true).is_ok_and(|said| said.is_some_and(|s| s.contains("pull or rebase"))));
        // A gate that holds still holds.
        let held = crate::review::gate(&[], &[crate::review::Conflict { file: "a.rs".into(), line: Some(3), taken: false }], None);
        assert_eq!(warn(held, &w).verdict, crate::review::Verdict::Hold);
    }

    #[test]
    fn w26_r16_the_warning_says_what_auto_push_costs() {
        let w = auto_push_warning("main");
        for words in ["every open change's branch is pushed each time main moves", "GitHub, GitLab and Bitbucket", "API use", "CI runs", "pull or rebase before they push"] {
            assert!(w.contains(words), "{words}");
        }
    }
}
