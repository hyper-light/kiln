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

use serde::{Deserialize, Serialize};

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
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Model {
    /// The share (0 to 1) of what a busy warm runner would save that a leg waits for it at most,
    /// unless its term says otherwise (`patience:`).
    pub patience: f64,
    /// The share of a cold run that a workspace no warm run has used yet is taken to save.
    pub prior: f64,
    /// The files changed at which a workspace keeps half its warmth.
    pub half_files: f64,
    /// The bytes a second a runner that doesn't say fetches at.
    pub bandwidth: f64,
}

impl Default for Model {
    fn default() -> Self {
        Model { patience: 0.5, prior: 0.5, half_files: 100.0, bandwidth: 100.0 * 1024.0 * 1024.0 }
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
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Estimate {
    /// The workspace it would start from: its lane, how many commits behind, how many files
    /// changed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lane: Option<String>,
    pub behind: u32,
    pub files: u32,
    pub warmth: f64,
    /// Seconds running (cold × (1 − warmth)) and fetching what it lacks (missing ÷ bandwidth).
    pub run_s: f64,
    pub fetch_s: f64,
}

impl Estimate {
    /// A cold run of `cold_s` seconds.
    pub fn cold(cold_s: f64) -> Estimate {
        Estimate { lane: None, behind: 0, files: 0, warmth: 0.0, run_s: cold_s, fetch_s: 0.0 }
    }

    /// Seconds from its start to its end: running and fetching.
    pub fn busy_s(&self) -> f64 {
        self.run_s + self.fetch_s
    }
}

fn share(x: f64) -> f64 {
    if x.is_nan() { 0.0 } else { x.clamp(0.0, 1.0) }
}

/// A workspace's warmth for a leg whose cold run takes `cold_s` seconds: what warm runs saved
/// there, else the prior, × `h ÷ (h + files changed)`.
pub fn warmth(w: &Workspace, cold_s: f64, m: &Model) -> f64 {
    let saved = match w.warm_s {
        Some(warm) if cold_s > 0.0 && warm.is_finite() => 1.0 - warm / cold_s,
        _ => m.prior,
    };
    let half = if m.half_files.is_nan() { 0.0 } else { m.half_files.max(0.0) };
    let files = f64::from(w.files);
    let near = if half > 0.0 {
        half / (half + files)
    } else if w.files == 0 {
        1.0
    } else {
        0.0
    };
    share(saved) * near
}

/// The estimate on a runner holding `held` (`None`: nothing): of the workspaces `wants` names
/// there, the one that would finish a leg of `cold_s` seconds first, and cold when none would
/// finish it sooner than a cold run. `toolchain` is the one the leg would run with there: a
/// workspace of its lane made with another isn't its lane's (W24-R11); `None` matches any.
pub fn estimate(held: Option<&Held>, wants: &[Wants], cold_s: f64, m: &Model, toolchain: Option<&str>) -> Estimate {
    let cold = Estimate::cold(cold_s);
    let Some(h) = held.filter(|_| !wants.is_empty()) else { return cold };
    let bandwidth = h.bandwidth.filter(|b| b.is_finite() && *b > 0.0).unwrap_or(m.bandwidth);
    let made_alike = |w: &Workspace| w.toolchain.is_empty() || toolchain.is_none_or(|t| t == w.toolchain);
    h.workspaces
        .iter()
        .filter(|w| wants.iter().any(|x| x.matches(w) && (!matches!(x, Wants::Lane(_)) || made_alike(w))))
        .map(|w| {
            let warmth = warmth(w, cold_s, m);
            let fetch_s = if bandwidth > 0.0 { w.missing as f64 / bandwidth } else { f64::INFINITY };
            Estimate { lane: Some(w.lane.clone()), behind: w.behind, files: w.files, warmth, run_s: cold_s * (1.0 - warmth), fetch_s }
        })
        .fold(cold, |best, e| if e.busy_s() < best.busy_s() { e } else { best })
}

/// One term over runners' expected finishes, normalized as W05-R23's are, sooner better: 1 for
/// the soonest, 0 for the latest, the rest in between; 0 for all when they finish alike, and for
/// a runner with none (`None`: it can't take the leg).
pub fn sooner(finish: &[Option<f64>]) -> Vec<f64> {
    let known: Vec<f64> = finish.iter().flatten().copied().filter(|f| f.is_finite()).collect();
    let (lo, hi) = (known.iter().copied().fold(f64::MAX, f64::min), known.iter().copied().fold(f64::MIN, f64::max));
    finish
        .iter()
        .map(|f| match f {
            Some(f) if f.is_finite() && hi > lo => (hi - f) / (hi - lo),
            _ => 0.0,
        })
        .collect()
}

/// Patience: whether a leg that would take `here_s` seconds where it could start now (unix ms
/// `now`) waits instead for a busy runner where it takes `there_s`, expected to have room in
/// `wait_s`, given its `share` of what that saves; and if so, the whole seconds it saves and the
/// time (unix ms) it waits until at most. It waits only when that runner saves at least a second,
/// finishes it sooner wait included, and is expected to have room within its share of what it
/// saves; and never longer than that share.
pub fn patience(here_s: f64, there_s: f64, wait_s: f64, share_of: f64, now: i64) -> Option<(u64, i64)> {
    let saves = here_s - there_s;
    if !saves.is_finite() || saves < 1.0 || !wait_s.is_finite() {
        return None;
    }
    let saves_s = saves.floor() as u64;
    let bound_ms = (share(share_of) * saves_s as f64 * 1000.0).floor();
    let wait_ms = (wait_s.max(0.0) * 1000.0).ceil();
    if bound_ms <= 0.0 || wait_ms > bound_ms || wait_s.max(0.0) + there_s >= here_s {
        return None;
    }
    Some((saves_s, now.saturating_add(bound_ms as i64)))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: f64 = 1024.0 * 1024.0;

    fn ws(lane: &str, warm_s: Option<f64>, files: u32, missing: u64) -> Workspace {
        Workspace { lane: lane.into(), toolchain: String::new(), repo: "acme/app".into(), workflow: ".kiln/ci.yaml".into(), job: "rust".into(), behind: files / 2, files, warm_s, missing }
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    /// W24-R18's expected finish, by hand: a 120 s cold leg whose warm runs here take 30 s saves
    /// 75 % at no distance, half that 100 files behind; 1 GiB missing at 100 MiB/s costs 10.24 s.
    #[test]
    fn w24_r18_expected_finish_by_hand() {
        let m = Model::default();
        let near = ws("l1", Some(30.0), 0, 0);
        assert!(close(warmth(&near, 120.0, &m), 0.75));
        let behind = ws("l1", Some(30.0), 100, 1 << 30);
        assert!(close(warmth(&behind, 120.0, &m), 0.375));
        let held = Held { workspaces: vec![behind], bandwidth: Some(100.0 * MIB) };
        let e = estimate(Some(&held), &[Wants::Lane("l1")], 120.0, &m, None);
        assert!(close(e.run_s, 75.0) && close(e.fetch_s, 10.24), "{e:?}");
        assert_eq!((e.lane.as_deref(), e.behind, e.files), (Some("l1"), 50, 100));
        // No warm run yet: the prior's half, shrunk by distance alike.
        assert!(close(warmth(&ws("l1", None, 100, 0), 120.0, &m), 0.25));
        // Warm runs that took longer than cold ones save nothing; warmth stays within 0 and 1.
        assert!(close(warmth(&ws("l1", Some(500.0), 0, 0), 120.0, &m), 0.0));
        assert!(close(warmth(&ws("l1", Some(-5.0), 0, 0), 120.0, &m), 1.0));
    }

    /// The best of a runner's workspaces counts, and only those the term names; one whose fetch
    /// costs more than it saves leaves the leg cold there.
    #[test]
    fn w24_r18_the_best_workspace_named_counts() {
        let m = Model::default();
        let held = Held { workspaces: vec![ws("l1", Some(60.0), 0, 0), ws("l1", Some(20.0), 0, 0), ws("l2", Some(1.0), 0, 0)], bandwidth: None };
        let e = estimate(Some(&held), &[Wants::Lane("l1")], 120.0, &m, None);
        assert!(close(e.busy_s(), 20.0), "{e:?}");
        let job = estimate(Some(&held), &[Wants::Job { repo: "acme/app", workflow: ".kiln/ci.yaml", job: "rust" }], 120.0, &m, None);
        assert!(close(job.busy_s(), 1.0), "any lane of the job: {job:?}");
        assert_eq!(estimate(Some(&held), &[Wants::Job { repo: "other/app", workflow: ".kiln/ci.yaml", job: "rust" }], 120.0, &m, None), Estimate::cold(120.0));
        assert_eq!(estimate(Some(&held), &[], 120.0, &m, None), Estimate::cold(120.0), "no term, no warmth");
        assert_eq!(estimate(None, &[Wants::Lane("l1")], 120.0, &m, None), Estimate::cold(120.0));
        let far = Held { workspaces: vec![ws("l1", Some(20.0), 0, 500 << 30)], bandwidth: Some(MIB) };
        assert_eq!(estimate(Some(&far), &[Wants::Lane("l1")], 120.0, &m, None), Estimate::cold(120.0), "fetching costs more than it saves");
    }

    /// W24-R11's lanes are per toolchain: a workspace of the leg's lane made with another counts
    /// for nothing there, one whose toolchain isn't known counts, and `affinity`'s job terms look
    /// past toolchains.
    #[test]
    fn w24_r18_a_lanes_workspace_counts_only_made_with_the_legs_toolchain() {
        let m = Model::default();
        let made = |toolchain: &str| Held { workspaces: vec![Workspace { toolchain: toolchain.into(), ..ws("l1", Some(20.0), 0, 0) }], bandwidth: None };
        let docker = made("docker|kiln/runner-ubuntu-24.04|");
        assert!(close(estimate(Some(&docker), &[Wants::Lane("l1")], 120.0, &m, Some("docker|kiln/runner-ubuntu-24.04|")).busy_s(), 20.0));
        assert_eq!(estimate(Some(&docker), &[Wants::Lane("l1")], 120.0, &m, Some("host||")), Estimate::cold(120.0), "made in a container, the leg would run on the host");
        assert!(close(estimate(Some(&made("")), &[Wants::Lane("l1")], 120.0, &m, Some("host||")).busy_s(), 20.0), "not known: it counts");
        assert!(close(estimate(Some(&docker), &[Wants::Lane("l1")], 120.0, &m, None).busy_s(), 20.0), "no toolchain asked");
        let job = Wants::Job { repo: "acme/app", workflow: ".kiln/ci.yaml", job: "rust" };
        assert!(close(estimate(Some(&docker), &[job], 120.0, &m, Some("host||")).busy_s(), 20.0), "a job's workspaces, whatever made them");
    }

    #[test]
    fn w24_r18_sooner_is_normalized_lower_better() {
        assert_eq!(sooner(&[Some(10.0), Some(30.0), Some(20.0), None]), vec![1.0, 0.0, 0.5, 0.0]);
        assert_eq!(sooner(&[Some(5.0), Some(5.0)]), vec![0.0, 0.0], "alike: no preference");
        assert_eq!(sooner(&[None, Some(5.0)]), vec![0.0, 0.0]);
        assert!(sooner(&[]).is_empty());
    }

    /// Patience by hand: 100 s saved with half of it to wait waits up to 50 s, only for a runner
    /// expected to have room by then, never for under a second saved or a finish no sooner.
    #[test]
    fn w24_r18_patience_by_hand() {
        assert_eq!(patience(120.0, 20.0, 19.0, 0.5, 1_000), Some((100, 51_000)));
        assert_eq!(patience(120.0, 20.0, 50.0, 0.5, 0), Some((100, 50_000)), "expected just within its share");
        assert_eq!(patience(120.0, 20.0, 50.001, 0.5, 0), None, "expected after its share");
        assert_eq!(patience(120.0, 20.0, 0.0, 0.0, 0), None, "no patience");
        assert_eq!(patience(20.5, 20.0, 0.0, 1.0, 0), None, "under a second saved");
        assert_eq!(patience(120.0, 20.0, 100.0, 1.0, 0), None, "no sooner, the wait counted");
        assert_eq!(patience(120.0, 20.0, 99.0, 1.0, 0), Some((100, 100_000)));
        assert_eq!(patience(120.0, 20.0, f64::NAN, 1.0, 0), None);
        assert_eq!(patience(120.7, 20.0, 0.0, 0.5, 0), Some((100, 50_000)), "whole seconds, rounded down");
    }
}
