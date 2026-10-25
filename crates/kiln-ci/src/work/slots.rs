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

use std::cmp::Reverse;
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use super::key::{Key, VERSION};

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
    let mut h = Sha256::new();
    put(&mut h, VERSION);
    put(&mut h, "lane");
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
    put(&mut h, &l.toolchain);
    h.update([5]);
    put(&mut h, &l.profile);
    Key(h.finalize().into())
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
}
