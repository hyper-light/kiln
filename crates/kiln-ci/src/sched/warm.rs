//! Warm-aware placement (docs/ci.md W24-R18, W05-R23): where a leg is expected to finish first,
//! from what each runner holds warm for it, and how long it may wait for a busy runner that would
//! finish it sooner. Pure: what each runner holds ([`Held`]) comes from the caller (the runners'
//! reports of their workspaces, W24-R11 and R12, and the ledger's history), and none of it counts
//! for a leg whose preferences don't name `warm` or `affinity` (opt-in, §0.3).
//!
//! On a runner, a leg is expected to finish after
//!
//! ```text
//! queue wait + cold × (1 − warmth) + missing bytes ÷ bandwidth
//! ```
//!
//! seconds. `cold` is its cold duration; `warmth`, from 0 to 1, the share of it that the runner's
//! best workspace for it saves; the missing bytes, what the runner lacks to start from that
//! workspace (a snapshot's chunks not there yet, W24-R12), fetched at its bandwidth. A workspace's
//! warmth is what warm runs saved there (`1 − warm ÷ cold`), or [`Model::prior`] before any did,
//! shrunk as the workspace falls behind: × `h ÷ (h + files changed)`, `h` being
//! [`Model::half_files`]. A runner where no workspace saves more than its fetch costs runs the leg
//! cold. A workspace of the leg's lane counts only where the leg would run with the toolchain it
//! was made with (the executor and image the runner gives it, and its job's container).
//!
//! **Patience** ([`patience`]). A leg that could start now may wait instead for a busy runner
//! where it would finish sooner: when that runner saves it at least a second, finishes it sooner
//! even after the wait, and is expected to have room within the leg's share
//! ([`Model::patience`]) of what it saves. It waits for that share at most, and once per attempt.
//!
//! The model counts in milliseconds and millionths ([`super::units`]): what runners report in
//! seconds and bytes a second is converted as it's read, and explanations say seconds again.

use serde::{Deserialize, Serialize};

use super::units::{self, Ms, Ppm, WHOLE};

/// One workspace a runner holds warm (W24-R11), or could make warm from a snapshot (W24-R12).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Workspace {
    /// Its lane as placement matches lanes (`work::slots::placement`): W24-R11's hash of the
    /// repository, the job, the matrix values that matter and the profile, its toolchain apart.
    pub lane: String,
    /// The toolchain it was made with (`work::slots::toolchain`: the executor, its image, the
    /// job's container): a leg's lane is this workspace's only where the leg would run with the
    /// same one here. Empty when it isn't known, matching any.
    pub toolchain: String,
    /// The repository, workflow (its file) and job whose lane it is (`LegIn::repo`,
    /// `workflow_file`, `route.job`): what `affinity: { job }` goes by, and `warm` for a leg
    /// without a lane.
    pub repo: String,
    pub workflow: String,
    pub job: String,
    /// How far it is from the commit the lane's legs run at: commits behind, and files changed
    /// between the two, which its warmth shrinks with.
    pub behind: u32,
    pub files: u32,
    /// History: how long the lane's warm runs take here, in seconds (their median).
    pub warm_s: Option<f64>,
    /// The bytes it lacks to start from it: a snapshot's chunks not here yet (W24-R12).
    pub missing: u64,
}

/// What a runner holds warm, as it said last (`Input::Warm`, W24-R18).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Held {
    pub workspaces: Vec<Workspace>,
    /// The bytes a second it fetches what it lacks at; [`Model::bandwidth`] when it doesn't say.
    pub bandwidth: Option<f64>,
}

/// The model's settings (`[ci.scheduler]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Model {
    /// The share of what a busy warm runner would save that a leg waits for it at most, unless its
    /// term says otherwise (`patience:`).
    pub patience: Ppm,
    /// The share of a cold run that a workspace no warm run has used yet is taken to save.
    pub prior: Ppm,
    /// The files changed at which a workspace keeps half its warmth.
    pub half_files: u32,
    /// The bytes a second a runner that doesn't say fetches at.
    pub bandwidth: u64,
}

impl Default for Model {
    fn default() -> Self {
        Model { patience: WHOLE / 2, prior: WHOLE / 2, half_files: 100, bandwidth: 100 << 20 }
    }
}

/// The workspaces one term looks at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wants<'a> {
    /// The leg's own lane (`warm`).
    Lane(&'a str),
    /// Any lane of a job (`affinity: { job }`, and `warm` for a leg without a lane).
    Job { repo: &'a str, workflow: &'a str, job: &'a str },
}

impl Wants<'_> {
    pub fn matches(&self, w: &Workspace) -> bool {
        match self {
            Wants::Lane(lane) => w.lane == *lane,
            Wants::Job { repo, workflow, job } => w.repo == *repo && w.workflow == *workflow && w.job == *job,
        }
    }
}

/// What a leg is expected to take on one runner, from the workspace there that would finish it
/// first: cold, from none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Estimate {
    /// The workspace it would start from: its lane, how many commits behind, how many files
    /// changed.
    pub lane: Option<String>,
    pub behind: u32,
    pub files: u32,
    pub warmth: Ppm,
    /// Running (cold × (1 − warmth)) and fetching what it lacks (missing ÷ bandwidth); `Ms::MAX`
    /// fetching at no bandwidth, never.
    pub run_ms: Ms,
    pub fetch_ms: Ms,
}

/// As explanations have always said it: warmth from 0 to 1, and seconds.
impl Serialize for Estimate {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Said<'a> {
            #[serde(skip_serializing_if = "Option::is_none")]
            lane: Option<&'a str>,
            behind: u32,
            files: u32,
            warmth: f64,
            run_s: f64,
            fetch_s: f64,
        }
        Said { lane: self.lane.as_deref(), behind: self.behind, files: self.files, warmth: units::share(self.warmth), run_s: units::secs(self.run_ms), fetch_s: units::secs_or_never(self.fetch_ms) }.serialize(s)
    }
}

impl Estimate {
    /// A cold run of `cold_ms`.
    pub fn cold(cold_ms: Ms) -> Estimate {
        Estimate { lane: None, behind: 0, files: 0, warmth: 0, run_ms: cold_ms, fetch_ms: 0 }
    }

    /// From its start to its end: running and fetching.
    pub fn busy_ms(&self) -> Ms {
        self.run_ms.saturating_add(self.fetch_ms)
    }
}

/// `a × b ÷ c`, rounded down, in 128 bits so nothing overflows on the way; `c` of 0 is none.
pub fn scale(a: u64, b: u64, c: u64) -> u64 {
    if c == 0 {
        return 0;
    }
    u64::try_from(u128::from(a) * u128::from(b) / u128::from(c)).unwrap_or(u64::MAX)
}

/// A workspace's warmth for a leg whose cold run takes `cold_ms`: what warm runs saved there,
/// else the prior, × `h ÷ (h + files changed)`.
pub fn warmth(w: &Workspace, cold_ms: Ms, m: &Model) -> Ppm {
    let saved = match w.warm_s.and_then(units::ms_of_secs) {
        Some(warm) if cold_ms > 0 => u32::try_from(scale(cold_ms.saturating_sub(warm), u64::from(WHOLE), cold_ms)).unwrap_or(WHOLE),
        _ => m.prior,
    };
    let near = if m.half_files > 0 {
        scale(u64::from(m.half_files), u64::from(WHOLE), u64::from(m.half_files) + u64::from(w.files))
    } else if w.files == 0 {
        u64::from(WHOLE)
    } else {
        0
    };
    u32::try_from(scale(u64::from(saved.min(WHOLE)), near, u64::from(WHOLE))).unwrap_or(WHOLE)
}

/// The estimate on a runner holding `held` (`None`: nothing): of the workspaces `wants` names
/// there, the one that would finish a leg of `cold_ms` first, and cold when none would finish it
/// sooner than a cold run. `toolchain` is the one the leg would run with there: a workspace of its
/// lane made with another isn't its lane's (W24-R11); `None` matches any.
pub fn estimate(held: Option<&Held>, wants: &[Wants], cold_ms: Ms, m: &Model, toolchain: Option<&str>) -> Estimate {
    let cold = Estimate::cold(cold_ms);
    let Some(h) = held.filter(|_| !wants.is_empty()) else { return cold };
    let bandwidth = h.bandwidth.and_then(units::bytes_per_s).unwrap_or(m.bandwidth);
    let made_alike = |w: &Workspace| w.toolchain.is_empty() || toolchain.is_none_or(|t| t == w.toolchain);
    h.workspaces
        .iter()
        .filter(|w| wants.iter().any(|x| x.matches(w) && (!matches!(x, Wants::Lane(_)) || made_alike(w))))
        .map(|w| {
            let warmth = warmth(w, cold_ms, m);
            // Missing bytes at the bandwidth, to the nearest millisecond.
            let fetch_ms = if bandwidth > 0 { u64::try_from((u128::from(w.missing) * 1000 + u128::from(bandwidth / 2)) / u128::from(bandwidth)).unwrap_or(Ms::MAX) } else { Ms::MAX };
            let run_ms = scale(cold_ms, u64::from(WHOLE.saturating_sub(warmth)), u64::from(WHOLE));
            Estimate { lane: Some(w.lane.clone()), behind: w.behind, files: w.files, warmth, run_ms, fetch_ms }
        })
        .fold(cold, |best, e| if e.busy_ms() < best.busy_ms() { e } else { best })
}

/// One term over runners' expected finishes, normalized as W05-R23's are, sooner better: all of it
/// for the soonest, none for the latest, the rest in between; none for all when they finish alike,
/// and for a runner with none (`None`: it can't take the leg) or that never finishes (`Ms::MAX`).
pub fn sooner(finish: &[Option<Ms>]) -> Vec<Ppm> {
    let known = || finish.iter().flatten().copied().filter(|f| *f < Ms::MAX);
    let (lo, hi) = (known().min().unwrap_or(0), known().max().unwrap_or(0));
    finish
        .iter()
        .map(|f| match f {
            Some(f) if *f < Ms::MAX && hi > lo => u32::try_from(scale(hi - f, u64::from(WHOLE), hi - lo)).unwrap_or(WHOLE),
            _ => 0,
        })
        .collect()
}

/// Patience: whether a leg that would take `here_ms` where it could start now (unix ms `now`)
/// waits instead for a busy runner where it takes `there_ms`, expected to have room in `wait_ms`,
/// given its `share` of what that saves; and if so, the whole seconds it saves and the time (unix
/// ms) it waits until at most. It waits only when that runner saves at least a second, finishes it
/// sooner wait included, and is expected to have room within its share of what it saves; and never
/// longer than that share.
pub fn patience(here_ms: Ms, there_ms: Ms, wait_ms: Ms, share: Ppm, now: i64) -> Option<(u64, i64)> {
    let saves_ms = here_ms.checked_sub(there_ms).filter(|s| *s >= 1000)?;
    let saves_s = saves_ms / 1000;
    let bound_ms = scale(u64::from(share.min(WHOLE)), saves_s.saturating_mul(1000), u64::from(WHOLE));
    if bound_ms == 0 || wait_ms > bound_ms || wait_ms.saturating_add(there_ms) >= here_ms {
        return None;
    }
    Some((saves_s, now.saturating_add(i64::try_from(bound_ms).unwrap_or(i64::MAX))))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: f64 = 1024.0 * 1024.0;

    fn ws(lane: &str, warm_s: Option<f64>, files: u32, missing: u64) -> Workspace {
        Workspace { lane: lane.into(), toolchain: String::new(), repo: "acme/app".into(), workflow: ".kiln/ci.yaml".into(), job: "rust".into(), behind: files / 2, files, warm_s, missing }
    }

    /// W24-R18's expected finish, by hand: a 120 s cold leg whose warm runs here take 30 s saves
    /// 75 % at no distance, half that 100 files behind; 1 GiB missing at 100 MiB/s costs 10.24 s.
    #[test]
    fn w24_r18_expected_finish_by_hand() {
        let m = Model::default();
        let near = ws("l1", Some(30.0), 0, 0);
        assert_eq!(warmth(&near, 120_000, &m), 750_000);
        let behind = ws("l1", Some(30.0), 100, 1 << 30);
        assert_eq!(warmth(&behind, 120_000, &m), 375_000);
        let held = Held { workspaces: vec![behind], bandwidth: Some(100.0 * MIB) };
        let e = estimate(Some(&held), &[Wants::Lane("l1")], 120_000, &m, None);
        assert_eq!((e.run_ms, e.fetch_ms), (75_000, 10_240), "{e:?}");
        assert_eq!((e.lane.as_deref(), e.behind, e.files), (Some("l1"), 50, 100));
        // No warm run yet: the prior's half, shrunk by distance alike.
        assert_eq!(warmth(&ws("l1", None, 100, 0), 120_000, &m), 250_000);
        // Warm runs that took longer than cold ones save nothing; warmth stays within 0 and 1.
        assert_eq!(warmth(&ws("l1", Some(500.0), 0, 0), 120_000, &m), 0);
        assert_eq!(warmth(&ws("l1", Some(-5.0), 0, 0), 120_000, &m), WHOLE);
        // Explanations say it as they always have: a share and seconds.
        let said = serde_json::to_value(&e).map_err(|err| err.to_string());
        assert_eq!(said, Ok(serde_json::json!({ "lane": "l1", "behind": 50, "files": 100, "warmth": 0.375, "run_s": 75.0, "fetch_s": 10.24 })));
    }

    /// The best of a runner's workspaces counts, and only those the term names; one whose fetch
    /// costs more than it saves leaves the leg cold there.
    #[test]
    fn w24_r18_the_best_workspace_named_counts() {
        let m = Model::default();
        let held = Held { workspaces: vec![ws("l1", Some(60.0), 0, 0), ws("l1", Some(20.0), 0, 0), ws("l2", Some(1.0), 0, 0)], bandwidth: None };
        let e = estimate(Some(&held), &[Wants::Lane("l1")], 120_000, &m, None);
        assert_eq!(e.busy_ms(), 20_000, "{e:?}");
        let job = estimate(Some(&held), &[Wants::Job { repo: "acme/app", workflow: ".kiln/ci.yaml", job: "rust" }], 120_000, &m, None);
        assert_eq!(job.busy_ms(), 1_000, "any lane of the job: {job:?}");
        assert_eq!(estimate(Some(&held), &[Wants::Job { repo: "other/app", workflow: ".kiln/ci.yaml", job: "rust" }], 120_000, &m, None), Estimate::cold(120_000));
        assert_eq!(estimate(Some(&held), &[], 120_000, &m, None), Estimate::cold(120_000), "no term, no warmth");
        assert_eq!(estimate(None, &[Wants::Lane("l1")], 120_000, &m, None), Estimate::cold(120_000));
        let far = Held { workspaces: vec![ws("l1", Some(20.0), 0, 500 << 30)], bandwidth: Some(MIB) };
        assert_eq!(estimate(Some(&far), &[Wants::Lane("l1")], 120_000, &m, None), Estimate::cold(120_000), "fetching costs more than it saves");
    }

    /// W24-R11's lanes are per toolchain: a workspace of the leg's lane made with another counts
    /// for nothing there, one whose toolchain isn't known counts, and `affinity`'s job terms look
    /// past toolchains.
    #[test]
    fn w24_r18_a_lanes_workspace_counts_only_made_with_the_legs_toolchain() {
        let m = Model::default();
        let made = |toolchain: &str| Held { workspaces: vec![Workspace { toolchain: toolchain.into(), ..ws("l1", Some(20.0), 0, 0) }], bandwidth: None };
        let docker = made("docker|kiln/runner-ubuntu-24.04|");
        assert_eq!(estimate(Some(&docker), &[Wants::Lane("l1")], 120_000, &m, Some("docker|kiln/runner-ubuntu-24.04|")).busy_ms(), 20_000);
        assert_eq!(estimate(Some(&docker), &[Wants::Lane("l1")], 120_000, &m, Some("host||")), Estimate::cold(120_000), "made in a container, the leg would run on the host");
        assert_eq!(estimate(Some(&made("")), &[Wants::Lane("l1")], 120_000, &m, Some("host||")).busy_ms(), 20_000, "not known: it counts");
        assert_eq!(estimate(Some(&docker), &[Wants::Lane("l1")], 120_000, &m, None).busy_ms(), 20_000, "no toolchain asked");
        let job = Wants::Job { repo: "acme/app", workflow: ".kiln/ci.yaml", job: "rust" };
        assert_eq!(estimate(Some(&docker), &[job], 120_000, &m, Some("host||")).busy_ms(), 20_000, "a job's workspaces, whatever made them");
    }

    #[test]
    fn w24_r18_sooner_is_normalized_lower_better() {
        assert_eq!(sooner(&[Some(10_000), Some(30_000), Some(20_000), None]), vec![WHOLE, 0, WHOLE / 2, 0]);
        assert_eq!(sooner(&[Some(5_000), Some(5_000)]), vec![0, 0], "alike: no preference");
        assert_eq!(sooner(&[None, Some(5_000)]), vec![0, 0]);
        assert_eq!(sooner(&[Some(5_000), Some(Ms::MAX)]), vec![0, 0], "never finishing isn't a finish");
        assert!(sooner(&[]).is_empty());
    }

    /// Patience by hand: 100 s saved with half of it to wait waits up to 50 s, only for a runner
    /// expected to have room by then, never for under a second saved or a finish no sooner.
    #[test]
    fn w24_r18_patience_by_hand() {
        let half = WHOLE / 2;
        assert_eq!(patience(120_000, 20_000, 19_000, half, 1_000), Some((100, 51_000)));
        assert_eq!(patience(120_000, 20_000, 50_000, half, 0), Some((100, 50_000)), "expected just within its share");
        assert_eq!(patience(120_000, 20_000, 50_001, half, 0), None, "expected after its share");
        assert_eq!(patience(120_000, 20_000, 0, 0, 0), None, "no patience");
        assert_eq!(patience(20_500, 20_000, 0, WHOLE, 0), None, "under a second saved");
        assert_eq!(patience(120_000, 20_000, 100_000, WHOLE, 0), None, "no sooner, the wait counted");
        assert_eq!(patience(120_000, 20_000, 99_000, WHOLE, 0), Some((100, 100_000)));
        assert_eq!(patience(20_000, 120_000, 0, WHOLE, 0), None, "slower there");
        assert_eq!(patience(120_700, 20_000, 0, half, 0), Some((100, 50_000)), "whole seconds, rounded down");
    }
}
