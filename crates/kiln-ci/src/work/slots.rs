//! Warm workspaces, decided (docs/ci.md W24-R11, R15). A runner keeps each lane's workspaces
//! ("slots") at stable paths, `<work>/warm/<repo>/<lane>/<n>`, each leased to one leg at a time.
//! From what the runner knows (its slots, how far each one's commit is from the leg's, its volume)
//! this says which slot a leg takes, when another is cloned or made, which go when the runner is
//! over its budget, and what becomes of one whose holder died. The runner carries it out
//! (`crates/kiln/src/runner/warm.rs`).
//!
//! - A [`lane`] is a hash of what lets workspaces stand in for one another: the repository, the
//!   job, the matrix values that matter, the toolchain and the profile.
//! - A trusted leg takes the free slot closest to its commit, ties going to the most recently
//!   used ([`choose`]). Taking a lane's last free slot also makes a spare: a copy-on-write clone
//!   of it, made before the leg writes, so a second leg of the lane that comes meanwhile gets a
//!   clone of a workspace no one is writing. A clone of one being written could pair a fingerprint
//!   its build has just written with an output copied before it was: a warm build that isn't a
//!   cold one (W24-AC02). With every slot leased and no spare, or where the volume can't clone, a
//!   leg gets a new, empty slot while the budget allows, and else runs cold.
//! - An untrusted leg works on a throwaway clone of the free slot closest to its commit, or cold
//!   ([`throwaway`]): nothing it writes comes back.
//! - [`evict`] drops the least recently used slots first, never a leased one, until each lane is
//!   within its budget, the runner within its own and the volume above its floor (W24-R15).
//! - A slot whose holder died is dirty: [`recover`] resets it to its repository, or drops it.
//! - What a runner holds warm it tells the server ([`WorkState`]), with how long each lane's legs
//!   took there ([`Runs`]); placement matches lanes but for their toolchain ([`placement`],
//!   [`held`], W24-R18).

use std::cmp::Reverse;
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use super::key::{Key, VERSION};
use crate::sched::warm::{Held, Workspace};
use crate::wf::ir::YVal;
use crate::wire::Warm;

/// A job's `work: { warm }` (W24-R11, R15), as its legs' assignments carry it: `warm: [target]`,
/// or `warm: { paths: [target], budget: 60Gi }`. `None` when it keeps no folders warm.
pub fn warm(work: Option<&YVal>) -> Option<Warm> {
    let w = work?.get("warm")?;
    let list = |v: &YVal| -> Vec<String> {
        match v {
            YVal::Seq(items) => items.iter().filter_map(YVal::as_str).map(str::to_owned).collect(),
            YVal::Str(t) => vec![t.src.clone()],
            _ => Vec::new(),
        }
    };
    let (paths, budget) = match w {
        YVal::Map(_) => (w.get("paths").map(list).unwrap_or_default(), w.get("budget").and_then(bytes)),
        other => (list(other), None),
    };
    (!paths.is_empty()).then_some(Warm { paths, budget })
}

/// A size as written: bytes, or with a unit (`60Gi`, `500M`).
fn bytes(v: &YVal) -> Option<u64> {
    let n = match v {
        YVal::Num(n) => *n,
        YVal::Str(t) => crate::sched::routing::number(t.src.trim())?,
        _ => return None,
    };
    (n.is_finite() && n >= 0.0).then_some(n as u64)
}

/// What a lane is made of (W24-R11): the workspaces of one lane can stand in for one another.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LaneOf {
    /// `owner/name`.
    pub repo: String,
    /// The job: its workflow's file and its id (`.kiln/ci.yaml#rust`).
    pub job: String,
    /// The matrix values that matter, by name.
    pub matrix: BTreeMap<String, String>,
    /// The toolchain's identity: the executor and its image, a hash of `rustc -vV` where known.
    pub toolchain: String,
    /// The build profile, when the job names one (`release`).
    pub profile: String,
}

/// One field into the hash: its length, then its bytes, so no two lanes encode alike.
fn put(h: &mut Sha256, s: &str) {
    h.update((s.len() as u64).to_be_bytes());
    h.update(s.as_bytes());
}

/// A lane's key: SHA-256 over [`VERSION`], `lane` and each component, tagged and length-prefixed
/// as [`super::key`]'s fingerprints are.
pub fn lane(l: &LaneOf) -> Key {
    key_of("lane", l, &l.toolchain)
}

/// A lane as placement matches it (W24-R18): [`lane`] but for its toolchain, which the runner a
/// leg goes to decides (the executor it runs the leg on, and that executor's image). Placement
/// compares toolchains apart, runner by runner.
pub fn placement(l: &LaneOf) -> Key {
    key_of("placement", l, "")
}

fn key_of(tag: &str, l: &LaneOf, toolchain: &str) -> Key {
    let mut h = Sha256::new();
    put(&mut h, VERSION);
    put(&mut h, tag);
    h.update([1]);
    put(&mut h, &l.repo);
    h.update([2]);
    put(&mut h, &l.job);
    h.update([3]);
    h.update((l.matrix.len() as u64).to_be_bytes());
    for (k, v) in &l.matrix {
        put(&mut h, k);
        put(&mut h, v);
    }
    h.update([4]);
    put(&mut h, toolchain);
    h.update([5]);
    put(&mut h, &l.profile);
    Key(h.finalize().into())
}

/// A leg's lane from what both its runner and the server know of it (W24-R11): its repository,
/// its workflow's file and its job's id (`.kiln/ci.yaml#rust`), its matrix values as text, and
/// `toolchain` ([`toolchain`]; empty for placement, which compares toolchains apart).
pub fn lane_for(repo: &str, workflow: &str, job: &str, matrix: &serde_json::Value, toolchain: String) -> LaneOf {
    let matrix = matrix.as_object().map(|m| m.iter().map(|(k, v)| (k.clone(), v.as_str().map_or_else(|| v.to_string(), str::to_owned))).collect()).unwrap_or_default();
    LaneOf { repo: repo.to_owned(), job: format!("{workflow}#{job}"), matrix, toolchain, profile: String::new() }
}

/// A lane's toolchain as a runner keys it (W24-R11): the executor that runs the leg (`host` when
/// none is named), the image it runs in where the executor runs one (a container's or a
/// microVM's), and the job's container, `|` between.
pub fn toolchain(executor: &str, image: Option<&str>, container: Option<&str>) -> String {
    let executor = if executor.is_empty() { "host" } else { executor };
    [executor, image.unwrap_or_default(), container.unwrap_or_default()].join("|")
}

/// Where a slot stands.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    /// No leg holds it.
    #[default]
    Free,
    /// One leg works in it.
    Leased,
    /// Its holder died while it held it: what it holds is unknown until it's reset.
    Dirty,
    /// It is never leased again and goes at the next eviction: it couldn't be cleared or checked
    /// out in place.
    Quarantined,
}

/// One of a lane's workspaces, as the runner records it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Slot {
    /// Its number, its path's last component: stable, so what tools record of its path holds.
    pub n: u32,
    pub state: State,
    /// The commit its repository has checked out; `None` before its first checkout and after a
    /// reset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// When a leg last let it go (Unix seconds).
    pub used: i64,
    /// The bytes it holds on its own: what removing it frees.
    pub bytes: u64,
    /// The bytes it shares copy-on-write with the lane's other slots, where the volume says
    /// (W24-R15); 0 where it doesn't.
    pub shared: u64,
    /// The leg that holds it, while one does.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub holder: Option<String>,
}

/// What a lane's slots hold together: each one's own bytes, and what they share counted once
/// (copy-on-write clones of one workspace share its extents).
pub fn lane_bytes(slots: &[Slot]) -> u64 {
    let own = slots.iter().fold(0u64, |a, s| a.saturating_add(s.bytes));
    own.saturating_add(slots.iter().map(|s| s.shared).max().unwrap_or(0))
}

/// The most runs of each kind a lane's history keeps ([`Runs`]).
pub const RUNS: usize = 9;

/// How long a lane's legs took on a runner, for placement's expected finish (W24-R18): its trusted
/// legs that succeeded, in milliseconds, the newest last, at most [`RUNS`] of each kind.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Runs {
    /// Those that started from one of its workspaces.
    pub warm_ms: Vec<u64>,
    /// Those that started from none: its first leg there, one that found every workspace held
    /// and no room for another, a cold rerun.
    pub cold_ms: Vec<u64>,
}

impl Runs {
    /// One more run, warm or cold, of `ms`: past [`RUNS`] of its kind the oldest goes.
    pub fn add(&mut self, warm: bool, ms: u64) {
        let list = if warm { &mut self.warm_ms } else { &mut self.cold_ms };
        list.push(ms);
        let over = list.len().saturating_sub(RUNS);
        list.drain(..over);
    }
}

/// The median of `xs`, the lower of the middle two for an even count; `None` for none.
pub fn median(xs: &[u64]) -> Option<u64> {
    let mut v = xs.to_vec();
    v.sort_unstable();
    v.get(v.len().saturating_sub(1) / 2).copied()
}

/// What a runner holds warm (W24-R11), as it tells the server (`AgentMsg::WorkState`'s JSON) for
/// placement (W24-R18): each of its lanes, with its workspaces and how long its legs took there.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkState {
    pub lanes: Vec<LaneState>,
}

/// One lane on a runner.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LaneState {
    /// Its key ([`lane`]), and what it's made of.
    pub key: String,
    pub lane: LaneOf,
    pub slots: Vec<Slot>,
    pub runs: Runs,
}

impl LaneState {
    /// The workspace the lane's next leg there would most likely start from: of those holding a
    /// checkout, free or leased (a leased one is free again once its runner has room for the
    /// leg), the one let go last.
    pub fn freshest(&self) -> Option<&Slot> {
        self.slots.iter().filter(|s| s.base.is_some() && matches!(s.state, State::Free | State::Leased)).max_by_key(|s| (s.used, Reverse(s.n)))
    }
}

/// What a runner holds warm, for placement (W24-R18): a workspace for each lane that has one
/// holding a checkout ([`LaneState::freshest`]), keyed as placement matches lanes ([`placement`])
/// with its toolchain beside it, the median of the lane's warm runs there as its history, and how
/// far its checkout is from the lane's next commit as `distance` says for its repository and
/// checkout (commits behind, files changed; none counted where it can't say).
pub fn held(state: &WorkState, distance: &dyn Fn(&str, &str) -> Option<(u32, u32)>) -> Held {
    let workspaces = state
        .lanes
        .iter()
        .filter_map(|l| {
            let base = l.freshest()?.base.as_deref()?;
            let (workflow, job) = l.lane.job.rsplit_once('#').unwrap_or_default();
            let (behind, files) = distance(&l.lane.repo, base).unwrap_or_default();
            Some(Workspace {
                lane: placement(&l.lane).hex(),
                toolchain: l.lane.toolchain.clone(),
                repo: l.lane.repo.clone(),
                workflow: workflow.to_owned(),
                job: job.to_owned(),
                behind,
                files,
                warm_s: median(&l.runs.warm_ms).map(|ms| ms as f64 / 1000.0),
                missing: 0,
            })
        })
        .collect();
    Held { workspaces, bandwidth: None }
}

/// How long a leg of the lane placement keys `key` ([`placement`]) runs cold: the median of every
/// runner's cold runs of the lane, in milliseconds (W24-R18's cold duration, from history).
pub fn cold_ms<'a>(states: impl IntoIterator<Item = &'a WorkState>, key: &str) -> Option<u64> {
    let runs: Vec<u64> = states.into_iter().flat_map(|s| s.lanes.iter()).filter(|l| placement(&l.lane).hex() == key).flat_map(|l| l.runs.cold_ms.iter().copied()).collect();
    median(&runs)
}

/// Whether the runner's volume clones a folder copy-on-write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Clones {
    /// It can't (ext4, NTFS): workspaces stay in place, and a second leg of a lane gets a new one.
    No,
    /// File by file, sharing their extents: reflink on btrfs and XFS, clonefile on APFS.
    Cow,
}

/// What warm workspaces may keep on a runner (W24-R15).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    /// The most every lane's slots hold together.
    pub runner: u64,
    /// Below this much free on the volume nothing new is kept, and slots go until it's free again.
    pub floor: u64,
    /// The most slots a lane has.
    pub slots: u32,
}

/// The runner's volume as it is now.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Disk {
    /// Bytes free on it.
    pub free: u64,
    /// What every lane's slots hold together ([`lane_bytes`], summed).
    pub kept: u64,
}

/// Why a lane has no room for another slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Full {
    /// The volume is below its floor.
    Floor,
    /// The runner's slots hold its budget.
    Runner,
    /// The lane's slots hold the job's budget.
    Lane,
    /// The lane has its most slots.
    Slots,
}

/// What a trusted leg takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Take {
    /// A free slot, checked out in place.
    Slot(u32),
    /// A new, empty slot under this number: cold this time, warm for the lane's legs after it.
    New(u32),
    /// No slot: none is free and there's no room for another. The leg runs cold in a workspace of
    /// its own.
    Cold(Full),
}

/// What a trusted leg's lease does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Choice {
    pub take: Take,
    /// A copy-on-write clone of the slot taken, made under this number before the leg writes to
    /// it: the lane had no other free slot, so a leg of it that comes while this one runs finds a
    /// clone of a workspace no one is writing.
    pub spare: Option<u32>,
}

/// Whether the lane has room for another slot (one more of what it holds), and if not why.
fn room(slots: &[Slot], lane_budget: Option<u64>, b: &Budget, d: &Disk) -> Result<(), Full> {
    if d.free < b.floor {
        return Err(Full::Floor);
    }
    if d.kept >= b.runner {
        return Err(Full::Runner);
    }
    if lane_budget.is_some_and(|max| lane_bytes(slots) >= max) {
        return Err(Full::Lane);
    }
    if slots.len() >= usize::try_from(b.slots).unwrap_or(usize::MAX) {
        return Err(Full::Slots);
    }
    Ok(())
}

/// The lowest number no slot of the lane has: paths stay few and stable.
fn next(slots: &[Slot]) -> u32 {
    (0u32..).find(|n| !slots.iter().any(|s| s.n == *n)).unwrap_or(u32::MAX)
}

/// The order slots are taken in: the least distance known (an unknown one after every known),
/// then one holding a checkout, then the most recently used, then the lowest number.
fn nearness(s: &Slot, distance: &BTreeMap<u32, u64>) -> (u64, bool, Reverse<i64>, u32) {
    (distance.get(&s.n).copied().unwrap_or(u64::MAX), s.base.is_none(), Reverse(s.used), s.n)
}

/// The free slot closest to the leg's commit.
fn closest<'a>(slots: &'a [Slot], distance: &BTreeMap<u32, u64>) -> Option<&'a Slot> {
    slots.iter().filter(|s| s.state == State::Free).min_by_key(|s| nearness(s, distance))
}

/// What a trusted leg of the lane takes (W24-R11): the free slot closest to its commit (`distance`:
/// commits between each slot's base and the leg's, where the runner knows), with a spare cloned
/// from it when it was the lane's last free one, the volume clones and there's room; else a new
/// slot while there's room; else nothing (cold). `lane_budget` is the job's `warm.budget`.
pub fn choose(slots: &[Slot], distance: &BTreeMap<u32, u64>, clones: Clones, lane_budget: Option<u64>, budget: &Budget, disk: &Disk) -> Choice {
    match closest(slots, distance) {
        Some(s) => {
            let last_free = !slots.iter().any(|o| o.n != s.n && o.state == State::Free);
            let spare = (clones == Clones::Cow && last_free && s.base.is_some() && room(slots, lane_budget, budget, disk).is_ok()).then(|| next(slots));
            Choice { take: Take::Slot(s.n), spare }
        }
        None => match room(slots, lane_budget, budget, disk) {
            Ok(()) => Choice { take: Take::New(next(slots)), spare: None },
            Err(why) => Choice { take: Take::Cold(why), spare: None },
        },
    }
}

/// The slot an untrusted leg's throwaway clone is made from: the free one holding a checkout
/// closest to its commit, where the volume clones and is above its floor. `None`: it runs cold.
/// Nothing it writes comes back (W24-R11, R21).
pub fn throwaway(slots: &[Slot], distance: &BTreeMap<u32, u64>, clones: Clones, budget: &Budget, disk: &Disk) -> Option<u32> {
    if clones != Clones::Cow || disk.free < budget.floor {
        return None;
    }
    slots.iter().filter(|s| s.state == State::Free && s.base.is_some()).min_by_key(|s| nearness(s, distance)).map(|s| s.n)
}

/// Why a slot goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    /// It's quarantined.
    Quarantined,
    /// Its lane holds more than the job's budget.
    Lane,
    /// The runner's slots hold more than its budget.
    Runner,
    /// The volume is below its floor.
    Floor,
}

/// A lane, for [`evict`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Lane {
    pub slots: Vec<Slot>,
    /// The job's `warm.budget`.
    pub budget: Option<u64>,
}

/// Whether eviction may take a slot: no leg holds it (a dirty one's holder died).
fn evictable(s: &Slot) -> bool {
    matches!(s.state, State::Free | State::Dirty)
}

/// The order slots go in: the least recently used first, then the highest number (spares and
/// newer slots before the lane's first).
fn age(s: &Slot) -> (i64, Reverse<u32>) {
    (s.used, Reverse(s.n))
}

/// Takes slot `n` out of `slots`: the bytes that frees.
fn take_out(slots: &mut Vec<Slot>, n: u32) -> u64 {
    let before = lane_bytes(slots);
    slots.retain(|s| s.n != n);
    before.saturating_sub(lane_bytes(slots))
}

/// Which slots go (W24-R15), as `(lane, slot, why)` in the order they go. Quarantined ones always;
/// then, least recently used first and never a leased one, each lane's until it's within its
/// budget, any lane's until the runner is within its own, and more until the volume (`free` bytes
/// now) is above its floor, each slot freeing what it holds on its own.
pub fn evict(lanes: &[Lane], budget: &Budget, free: u64) -> Vec<(usize, u32, Why)> {
    let mut left: Vec<Vec<Slot>> = lanes.iter().map(|l| l.slots.clone()).collect();
    let mut out = Vec::new();
    let mut free = free;
    for (i, slots) in left.iter_mut().enumerate() {
        let quarantined: Vec<u32> = slots.iter().filter(|s| s.state == State::Quarantined).map(|s| s.n).collect();
        for n in quarantined {
            free = free.saturating_add(take_out(slots, n));
            out.push((i, n, Why::Quarantined));
        }
    }
    for (i, (lane, slots)) in lanes.iter().zip(left.iter_mut()).enumerate() {
        let Some(max) = lane.budget else { continue };
        while lane_bytes(slots) > max {
            let Some(n) = slots.iter().filter(|s| evictable(s)).min_by_key(|s| age(s)).map(|s| s.n) else { break };
            free = free.saturating_add(take_out(slots, n));
            out.push((i, n, Why::Lane));
        }
    }
    loop {
        let total = left.iter().fold(0u64, |a, s| a.saturating_add(lane_bytes(s)));
        let why = if total > budget.runner {
            Why::Runner
        } else if free < budget.floor {
            Why::Floor
        } else {
            break;
        };
        let oldest = left.iter().enumerate().flat_map(|(i, slots)| slots.iter().filter(|s| evictable(s)).map(move |s| (i, s))).min_by_key(|(_, s)| age(s)).map(|(i, s)| (i, s.n));
        let Some((i, n)) = oldest else { break };
        let Some(slots) = left.get_mut(i) else { break };
        free = free.saturating_add(take_out(slots, n));
        out.push((i, n, why));
    }
    out
}

/// What becomes of a slot a lease finds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recovery {
    /// It stays as it is: free, or leased by a leg that's alive.
    Keep,
    /// Its holder died: everything but its repository goes, and it's free again (cold, but for
    /// the history git needn't fetch again).
    Reset,
    /// It goes: its holder died before it held a repository, or it's quarantined.
    Drop,
}

/// What becomes of `slot` (W24-R11): `alive` says whether the leg its record names still holds it
/// (its lock is held, or it's kept to look into).
pub fn recover(slot: &Slot, alive: bool) -> Recovery {
    match slot.state {
        State::Free => Recovery::Keep,
        State::Leased if alive => Recovery::Keep,
        State::Leased | State::Dirty if slot.base.is_some() => Recovery::Reset,
        State::Leased | State::Dirty | State::Quarantined => Recovery::Drop,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn lane_of() -> LaneOf {
        LaneOf { repo: "acme/app".into(), job: ".kiln/ci.yaml#rust".into(), matrix: BTreeMap::from([("os".into(), "linux".into())]), toolchain: "host".into(), profile: String::new() }
    }

    /// W24-R11: the kiln dialect's `work: { warm }` as the assignment carries it.
    #[test]
    fn w24_r11_a_jobs_warm_folders_from_the_dialect() {
        let wf = |job: &str| crate::wf::parse_workflow(".kiln/ci.yaml", &format!("on: push\njobs:\n  build:\n    runs-on: local\n{job}    steps:\n      - run: make\n"), crate::expr::Dialect::Kiln);
        let of = |job: &str| wf(job).0.and_then(|w| w.jobs.first().and_then(|j| warm(j.kiln.work.as_ref())));
        assert_eq!(of("    work: { warm: [target, node_modules] }\n"), Some(Warm { paths: vec!["target".into(), "node_modules".into()], budget: None }));
        assert_eq!(of("    work: { warm: target }\n"), Some(Warm { paths: vec!["target".into()], budget: None }));
        assert_eq!(of("    work:\n      warm: { paths: [target], budget: 60Gi }\n"), Some(Warm { paths: vec!["target".into()], budget: Some(60 << 30) }));
        assert_eq!(of("    work: { warm: { paths: [target], budget: 1048576 } }\n"), Some(Warm { paths: vec!["target".into()], budget: Some(1 << 20) }));
        assert_eq!(of("    work: { warm: [] }\n"), None, "nothing kept, no warm workspace");
        assert_eq!(of("    work: { memo: { inputs: [src/**] } }\n"), None);
        assert_eq!(of(""), None);
        let (_, diags) = wf("    work: { warm: [target] }\n");
        assert!(diags.iter().all(|d| !d.is_error()), "a key the dialect knows: {diags:?}");
        let (_, diags) = crate::wf::parse_workflow(".github/workflows/ci.yml", "on: push\njobs:\n  b:\n    runs-on: x\n    work: { warm: [target] }\n    steps: [{run: make}]\n", crate::expr::Dialect::Gha);
        assert!(diags.iter().any(|d| d.code == "KCI5002"), "and kiln's alone: {diags:?}");
        // An overlay gives a GitHub workflow's job its warm folders (W02-R07, W24-R25).
        let (gha, _) = crate::wf::parse_workflow(".github/workflows/ci.yml", "on: push\njobs:\n  b:\n    runs-on: x\n    steps: [{run: make}]\n", crate::expr::Dialect::Gha);
        let (ov, diags) = crate::wf::parse_overlay("jobs:\n  b:\n    work: { warm: [target] }\n");
        assert!(diags.iter().all(|d| !d.is_error()), "{diags:?}");
        let mut gha = gha.unwrap_or_default();
        let _ = crate::wf::apply_overlay(&mut gha, &ov.unwrap_or_default());
        assert_eq!(gha.jobs.first().and_then(|j| warm(j.kiln.work.as_ref())), Some(Warm { paths: vec!["target".into()], budget: None }));
    }

    #[test]
    fn w24_r11_a_lane_is_its_repository_job_matrix_toolchain_and_profile() -> Result<(), serde_json::Error> {
        let base = lane_of();
        let k = lane(&base);
        let edits: [fn(&mut LaneOf); 6] = [
            |l| l.repo.push('x'),
            |l| l.job = ".kiln/ci.yaml#test".into(),
            |l| {
                l.matrix.insert("os".into(), "macos".into());
            },
            |l| {
                l.matrix.insert("rust".into(), "nightly".into());
            },
            |l| l.toolchain = "docker|kiln/runner-ubuntu-24.04".into(),
            |l| l.profile = "release".into(),
        ];
        for edit in edits {
            let mut l = base.clone();
            edit(&mut l);
            assert_ne!(lane(&l), k, "{l:?}");
        }
        // Each field is its own: a value moved across a boundary is another lane.
        let moved = LaneOf { repo: "acme/ap".into(), job: format!("p{}", base.job), ..base.clone() };
        assert_ne!(lane(&moved), k);
        // The same lane, however it was built or carried.
        let back: LaneOf = serde_json::from_str(&serde_json::to_string(&base)?)?;
        assert_eq!(lane(&back), k);
        assert_eq!(k.hex().len(), 64);
        Ok(())
    }

    fn slot(n: u32, state: State, base: Option<&str>, used: i64, bytes: u64) -> Slot {
        Slot { n, state, base: base.map(str::to_owned), used, bytes, shared: 0, holder: None }
    }

    const ROOMY: Budget = Budget { runner: 1 << 40, floor: 1 << 30, slots: 4 };
    const DISK: Disk = Disk { free: 1 << 40, kept: 0 };

    #[test]
    fn w24_r11_a_leg_takes_the_free_slot_closest_to_its_commit() {
        let slots = [slot(0, State::Free, Some("a"), 10, 1), slot(1, State::Free, Some("b"), 30, 1), slot(2, State::Leased, Some("c"), 40, 1), slot(3, State::Free, Some("d"), 20, 1)];
        let d = BTreeMap::from([(0, 5), (1, 9), (2, 0), (3, 5)]);
        // 0 and 3 are as close; 3 was used more recently. The leased one, closer still, isn't taken.
        assert_eq!(choose(&slots, &d, Clones::Cow, None, &ROOMY, &DISK), Choice { take: Take::Slot(3), spare: None });
        // A known distance comes before an unknown one; among unknown ones the most recently used.
        assert_eq!(choose(&slots, &BTreeMap::from([(0, 50)]), Clones::No, None, &ROOMY, &DISK).take, Take::Slot(0));
        assert_eq!(choose(&slots, &BTreeMap::new(), Clones::No, None, &ROOMY, &DISK).take, Take::Slot(1));
    }

    #[test]
    fn w24_r11_a_second_leg_of_the_lane_gets_a_clone() {
        // The lane's one slot: the first leg takes it, and a spare is cloned from it before it writes.
        let first = choose(&[slot(0, State::Free, Some("a"), 10, 100)], &BTreeMap::new(), Clones::Cow, None, &ROOMY, &DISK);
        assert_eq!(first, Choice { take: Take::Slot(0), spare: Some(1) });
        // The clone shares all its source's bytes, which the lane holds once.
        let slots = [Slot { shared: 100, ..slot(0, State::Leased, Some("a"), 10, 0) }, Slot { shared: 100, ..slot(1, State::Free, Some("a"), 10, 0) }];
        assert_eq!(lane_bytes(&slots), 100);
        // The second, while the first runs, takes the clone (and a spare of its own is made).
        let second = choose(&slots, &BTreeMap::new(), Clones::Cow, None, &ROOMY, &DISK);
        assert_eq!(second, Choice { take: Take::Slot(1), spare: Some(2) });
        // Where the volume can't clone: no spare, and a leg that finds every slot leased gets a new
        // one while there's room, else runs cold.
        let leased = [slot(0, State::Leased, Some("a"), 10, 100)];
        assert_eq!(choose(&[slot(0, State::Free, Some("a"), 10, 100)], &BTreeMap::new(), Clones::No, None, &ROOMY, &DISK).spare, None);
        assert_eq!(choose(&leased, &BTreeMap::new(), Clones::No, None, &ROOMY, &DISK).take, Take::New(1));
        assert_eq!(choose(&leased, &BTreeMap::new(), Clones::Cow, Some(100), &ROOMY, &DISK).take, Take::Cold(Full::Lane));
        // A slot never checked out isn't worth a spare.
        assert_eq!(choose(&[slot(0, State::Free, None, 0, 0)], &BTreeMap::new(), Clones::Cow, None, &ROOMY, &DISK).spare, None);
        // An empty lane: its first slot.
        assert_eq!(choose(&[], &BTreeMap::new(), Clones::Cow, None, &ROOMY, &DISK).take, Take::New(0));
    }

    #[test]
    fn w24_r15_nothing_new_is_kept_past_a_budget_or_below_the_floor() {
        let leased = [slot(0, State::Leased, Some("a"), 10, 100), slot(2, State::Leased, Some("a"), 10, 100)];
        let at = |b: &Budget, d: &Disk, lane: Option<u64>| choose(&leased, &BTreeMap::new(), Clones::Cow, lane, b, d).take;
        assert_eq!(at(&ROOMY, &DISK, None), Take::New(1), "the lowest free number");
        assert_eq!(at(&ROOMY, &Disk { free: (1 << 30) - 1, kept: 0 }, None), Take::Cold(Full::Floor));
        assert_eq!(at(&ROOMY, &Disk { free: 1 << 40, kept: 1 << 40 }, None), Take::Cold(Full::Runner));
        assert_eq!(at(&ROOMY, &DISK, Some(200)), Take::Cold(Full::Lane));
        assert_eq!(at(&Budget { slots: 2, ..ROOMY }, &DISK, None), Take::Cold(Full::Slots));
        // A free slot is still taken in place, but no spare is made for it.
        let free = [slot(0, State::Free, Some("a"), 10, 100)];
        assert_eq!(choose(&free, &BTreeMap::new(), Clones::Cow, None, &ROOMY, &Disk { free: 0, kept: 0 }), Choice { take: Take::Slot(0), spare: None });
    }

    #[test]
    fn w24_r11_untrusted_legs_get_a_throwaway_clone_or_run_cold() {
        let slots = [slot(0, State::Leased, Some("a"), 50, 1), slot(1, State::Free, Some("b"), 10, 1), slot(2, State::Free, None, 60, 1), slot(3, State::Free, Some("c"), 20, 1)];
        assert_eq!(throwaway(&slots, &BTreeMap::new(), Clones::Cow, &ROOMY, &DISK), Some(3), "the most recently used holding a checkout");
        assert_eq!(throwaway(&slots, &BTreeMap::from([(1, 3)]), Clones::Cow, &ROOMY, &DISK), Some(1), "the closest");
        assert_eq!(throwaway(&slots, &BTreeMap::from([(1, 3)]), Clones::No, &ROOMY, &DISK), None, "never a copy");
        assert_eq!(throwaway(&slots, &BTreeMap::from([(1, 3)]), Clones::Cow, &ROOMY, &Disk { free: 0, kept: 0 }), None, "below the floor");
        assert_eq!(throwaway(&[slot(0, State::Leased, Some("a"), 50, 1), slot(1, State::Free, None, 9, 1)], &BTreeMap::new(), Clones::Cow, &ROOMY, &DISK), None, "a leased slot is never cloned, an empty one not worth it");
    }

    #[test]
    fn w24_r15_eviction_goes_least_recently_used_first_never_leased() {
        let lanes = [
            Lane { slots: vec![slot(0, State::Free, Some("a"), 30, 60), slot(1, State::Free, Some("a"), 10, 60), slot(2, State::Leased, Some("a"), 0, 60)], budget: Some(130) },
            Lane { slots: vec![slot(0, State::Free, Some("b"), 20, 50), slot(1, State::Quarantined, None, 40, 5)], budget: None },
        ];
        let b = Budget { runner: 120, floor: 0, slots: 4 };
        // The quarantined slot always; the first lane's oldest free slot for its own budget (180 >
        // 130); then the oldest left anywhere until the runner is within 120 (120 + 50 > 120).
        assert_eq!(evict(&lanes, &b, 1 << 40), [(1, 1, Why::Quarantined), (0, 1, Why::Lane), (1, 0, Why::Runner)]);
        // Under budget, the floor: the oldest goes until the volume is free enough (5 + 60 + 50).
        let roomy = Budget { runner: 1 << 40, floor: 100, slots: 4 };
        assert_eq!(evict(&lanes, &roomy, 0), [(1, 1, Why::Quarantined), (0, 1, Why::Lane), (1, 0, Why::Floor)]);
        // Leased slots stay whatever the budget.
        let held = [Lane { slots: vec![slot(0, State::Leased, Some("a"), 0, 1 << 30)], budget: Some(1) }];
        assert!(evict(&held, &Budget { runner: 0, floor: u64::MAX, slots: 4 }, 0).is_empty());
    }

    #[test]
    fn w24_r11_a_slot_whose_holder_died_is_reset_or_dropped() {
        let checked_out = slot(0, State::Leased, Some("a"), 0, 1);
        assert_eq!(recover(&checked_out, true), Recovery::Keep);
        assert_eq!(recover(&checked_out, false), Recovery::Reset);
        assert_eq!(recover(&slot(0, State::Leased, None, 0, 1), false), Recovery::Drop, "it died before its checkout");
        assert_eq!(recover(&slot(0, State::Dirty, Some("a"), 0, 1), true), Recovery::Reset);
        assert_eq!(recover(&slot(0, State::Free, Some("a"), 0, 1), false), Recovery::Keep);
        assert_eq!(recover(&slot(0, State::Quarantined, Some("a"), 0, 1), true), Recovery::Drop);
    }

    #[test]
    fn w24_r15_a_lane_counts_what_its_clones_share_once() {
        assert_eq!(lane_bytes(&[]), 0);
        let one = [slot(0, State::Free, None, 0, 70)];
        assert_eq!(lane_bytes(&one), 70);
        let clones = [Slot { shared: 40, ..slot(0, State::Free, None, 0, 10) }, Slot { shared: 40, ..slot(1, State::Free, None, 0, 5) }];
        assert_eq!(lane_bytes(&clones), 55);
    }

    fn state() -> impl Strategy<Value = State> {
        prop_oneof![Just(State::Free), Just(State::Leased), Just(State::Dirty), Just(State::Quarantined)]
    }

    /// Up to 7 slots with distinct numbers below 10.
    fn slots() -> impl Strategy<Value = Vec<Slot>> {
        proptest::collection::btree_map(0u32..10, (state(), proptest::option::of("[a-c]"), 0i64..50, 0u64..100, 0u64..20), 0..7)
            .prop_map(|m| m.into_iter().map(|(n, (state, base, used, bytes, shared))| Slot { n, state, base, used, bytes, shared, holder: None }).collect())
    }

    fn budget() -> impl Strategy<Value = Budget> {
        (0u64..1000, 0u64..100, 0u32..6).prop_map(|(runner, floor, slots)| Budget { runner, floor, slots })
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(5_000))]

        /// W24-R11: a leg takes a free slot, none free being closer to its commit (nor as close and
        /// used more recently); a new or spare slot's number is one no slot has; a spare only where
        /// the volume clones, from the lane's last free slot.
        #[test]
        fn w24_r11_choices_take_the_nearest_free_slot_never_a_held_one(
            slots in slots(),
            distance in proptest::collection::btree_map(0u32..10, 0u64..20, 0..10),
            cow in any::<bool>(),
            lane_budget in proptest::option::of(0u64..400),
            b in budget(),
            free in 0u64..200,
            kept in 0u64..1000,
        ) {
            let clones = if cow { Clones::Cow } else { Clones::No };
            let c = choose(&slots, &distance, clones, lane_budget, &b, &Disk { free, kept });
            let unused = |n: u32| !slots.iter().any(|s| s.n == n);
            match c.take {
                Take::Slot(n) => {
                    let s = slots.iter().find(|s| s.n == n);
                    prop_assert!(s.is_some_and(|s| s.state == State::Free));
                    if let Some(s) = s {
                        for o in slots.iter().filter(|o| o.state == State::Free) {
                            prop_assert!(nearness(o, &distance) >= nearness(s, &distance), "{o:?} is nearer than {s:?}");
                        }
                        if let Some(m) = c.spare {
                            prop_assert!(unused(m) && cow && s.base.is_some());
                            prop_assert_eq!(slots.iter().filter(|o| o.state == State::Free).count(), 1);
                        }
                    }
                }
                Take::New(n) => {
                    prop_assert!(unused(n) && c.spare.is_none());
                    prop_assert!(!slots.iter().any(|s| s.state == State::Free));
                }
                Take::Cold(_) => prop_assert!(!slots.iter().any(|s| s.state == State::Free) && c.spare.is_none()),
            }
        }

        /// W24-R15: eviction never takes a leased slot; quarantined ones always go; what's left of
        /// each lane is within its budget, and the runner within its own, unless nothing left could
        /// go; and slots go oldest first.
        #[test]
        fn w24_r15_eviction_keeps_budgets_and_leases(
            lanes in proptest::collection::vec((slots(), proptest::option::of(0u64..300)), 0..4),
            b in budget(),
            free in 0u64..200,
        ) {
            let lanes: Vec<Lane> = lanes.into_iter().map(|(slots, budget)| Lane { slots, budget }).collect();
            let gone = evict(&lanes, &b, free);
            let went = |i: usize, n: u32| gone.iter().any(|(l, m, _)| *l == i && *m == n);
            let mut left_total = 0u64;
            let mut any_evictable_left = false;
            for (i, lane) in lanes.iter().enumerate() {
                for s in &lane.slots {
                    if s.state == State::Leased {
                        prop_assert!(!went(i, s.n), "a leased slot went: {s:?}");
                    }
                    if s.state == State::Quarantined {
                        prop_assert!(went(i, s.n));
                    }
                }
                let left: Vec<Slot> = lane.slots.iter().filter(|s| !went(i, s.n)).cloned().collect();
                let evictable_left = left.iter().any(evictable);
                any_evictable_left |= evictable_left;
                if let Some(max) = lane.budget {
                    prop_assert!(lane_bytes(&left) <= max || !evictable_left, "lane {i} over its budget with slots it could drop");
                }
                left_total = left_total.saturating_add(lane_bytes(&left));
                // Oldest first, lane by lane: whatever went of a lane for its budget is older than
                // whatever of it stays and could have gone.
                for (_, n, _) in gone.iter().filter(|(l, _, w)| *l == i && *w == Why::Lane) {
                    if let Some(g) = lane.slots.iter().find(|s| s.n == *n) {
                        prop_assert!(left.iter().filter(|s| evictable(s)).all(|s| age(s) >= age(g)));
                    }
                }
            }
            prop_assert!(left_total <= b.runner || !any_evictable_left);
            // Across lanes, for the runner's budget and the floor, oldest first too.
            for (i, n, _) in gone.iter().filter(|(_, _, w)| matches!(w, Why::Runner | Why::Floor)) {
                let Some(g) = lanes.get(*i).and_then(|l| l.slots.iter().find(|s| s.n == *n)) else { continue };
                for (j, lane) in lanes.iter().enumerate() {
                    prop_assert!(lane.slots.iter().filter(|s| evictable(s) && !went(j, s.n)).all(|s| age(s) >= age(g)));
                }
            }
        }
    }

    /// W24-R18: placement keys a lane but for its toolchain; the lane's own key keeps it; a leg's
    /// lane from its parts is the one its runner keys.
    #[test]
    fn w24_r18_placement_keys_lanes_but_for_their_toolchain() {
        let l = lane_of();
        let docker = LaneOf { toolchain: "docker|kiln/runner-ubuntu-24.04|".into(), ..l.clone() };
        assert_ne!(lane(&l), lane(&docker));
        assert_eq!(placement(&l), placement(&docker));
        assert_ne!(placement(&l), lane(&LaneOf { toolchain: String::new(), ..l.clone() }), "a key of its own");
        assert_ne!(placement(&l), placement(&LaneOf { matrix: BTreeMap::from([("os".into(), "macos".into())]), ..l.clone() }));
        assert_eq!(lane_for("acme/app", ".kiln/ci.yaml", "rust", &serde_json::json!({ "os": "linux" }), "host".into()), l);
        let m = lane_for("a/b", "w.yml", "j", &serde_json::json!({ "n": 14, "x": true }), String::new()).matrix;
        assert_eq!(m, BTreeMap::from([("n".into(), "14".into()), ("x".into(), "true".into())]));
        assert_eq!(toolchain("", None, None), "host||");
        assert_eq!(toolchain("docker", Some("kiln/runner-ubuntu-24.04"), Some("rust:1.91")), "docker|kiln/runner-ubuntu-24.04|rust:1.91");
    }

    /// W24-R18: what a runner holds warm, as placement takes it: a workspace for each lane with a
    /// checkout (its freshest, free or leased), its history's median, its distance as given (none
    /// counted where it isn't known); and a lane's cold duration from every runner's cold runs.
    #[test]
    fn w24_r18_a_runners_warm_state_for_placement() -> Result<(), String> {
        let slot = |n: u32, state: State, base: Option<&str>, used: i64| Slot { n, state, base: base.map(str::to_owned), used, ..Slot::default() };
        let rust = LaneState {
            key: lane(&lane_of()).hex(),
            lane: lane_of(),
            slots: vec![slot(0, State::Free, Some("aaa"), 10), slot(1, State::Leased, Some("bbb"), 20), slot(2, State::Dirty, Some("ccc"), 30)],
            runs: Runs { warm_ms: vec![30_000, 10_000, 20_000], cold_ms: vec![120_000] },
        };
        let docs = LaneState { key: "docs".into(), lane: LaneOf { job: ".kiln/ci.yaml#docs".into(), ..lane_of() }, slots: vec![slot(0, State::Free, None, 5)], runs: Runs::default() };
        let state = WorkState { lanes: vec![rust.clone(), docs] };
        let h = held(&state, &|repo, base| (repo == "acme/app" && base == "bbb").then_some((2, 7)));
        let [w] = h.workspaces.as_slice() else { return Err(format!("one workspace, the docs lane having no checkout: {h:?}")) };
        assert_eq!((w.lane.clone(), w.toolchain.as_str(), w.repo.as_str(), w.workflow.as_str(), w.job.as_str()), (placement(&lane_of()).hex(), "host", "acme/app", ".kiln/ci.yaml", "rust"));
        assert_eq!((w.behind, w.files, w.warm_s, w.missing), (2, 7, Some(20.0), 0));
        let unknown = held(&state, &|_, _| None);
        assert!(unknown.workspaces.iter().all(|w| (w.behind, w.files) == (0, 0)), "{unknown:?}");
        let other = WorkState { lanes: vec![LaneState { lane: LaneOf { toolchain: "docker|i|".into(), ..lane_of() }, runs: Runs { cold_ms: vec![100_000, 140_000], ..Runs::default() }, ..rust }] };
        assert_eq!(cold_ms([&state, &other], &placement(&lane_of()).hex()), Some(120_000), "every runner's, whatever the toolchain");
        assert_eq!(cold_ms([&state], "another"), None);
        let json = serde_json::to_string(&state).map_err(|e| e.to_string())?;
        assert_eq!(serde_json::from_str::<WorkState>(&json).map_err(|e| e.to_string())?, state, "as the runner says it");
        Ok(())
    }

    /// A lane's history keeps the newest [`RUNS`] of each kind; the median is the lower middle.
    #[test]
    fn w24_r18_runs_keep_the_newest() {
        let mut r = Runs::default();
        for ms in 1..=12u64 {
            r.add(true, ms);
        }
        r.add(false, 99);
        assert_eq!(r.warm_ms, (4..=12).collect::<Vec<u64>>());
        assert_eq!(r.cold_ms, vec![99]);
        assert_eq!((median(&[3, 1, 2]), median(&[4, 1, 3, 2]), median(&[])), (Some(2), Some(2), None));
    }
}
