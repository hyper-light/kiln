//! Where a commit stands on its way from landing to replanting, as Ship shows it: a stand in
//! Mission control's words (needs you, failing, landing, checking, building, in staging, waiting,
//! shipping, replanting, shipped; or fixed and rolled back once past them), the one line that says
//! what it's doing now, and its six pips (land, checks, build, staging, production, replant).
//!
//! The server reads the records (plan 01a175b3a21174bc8769f5e784b90ecb, "Sources by commit": CI
//! runs and their legs' attempts, the release pipeline's builds and image, staging, the production
//! release with its approval and guard, each runner's replant) into a [`Journey`]; [`standing`]
//! says where it stands. Pure: the time is passed in, nothing is read or written here.
//!
//! A code failure, CI's own error and a flake stay apart: a leg that failed and passed on its retry
//! is a flake; one that kiln itself failed (its runner lost, its pod evicted) and passed on retry is
//! a pass.

use serde::Serialize;

/// Who made a commit: a person, or an agent acting for one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Author {
    Person(String),
    Agent { name: String, for_person: String },
}

/// The land queue's word on a commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Land {
    Queued { ahead: u32 },
    Testing,
    Landed,
    Ejected { why: String },
}

/// How one attempt at a leg went (`ci_attempts`: its conclusion and failure class).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attempt {
    Queued,
    Running,
    Passed,
    /// Nothing it reads changed: the result before it holds (W08).
    Reused,
    /// The code failed it.
    Failed,
    /// Kiln failed it (a runner lost, a pod evicted): it's retried, and isn't the code's fault.
    KilnError,
    Cancelled,
}

/// One leg of a check, its attempts oldest first, and what its last failure said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Leg {
    pub name: String,
    pub attempts: Vec<Attempt>,
    pub note: String,
}

/// One pipeline's run on the commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub pipeline: String,
    pub legs: Vec<Leg>,
}

/// How a piece of the build (a platform's binary, the image) stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    Queued,
    Running,
    Passed,
    Failed,
    /// A newer push replaced it before it finished (binaries only).
    Superseded,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Platform {
    pub name: String,
    pub state: Part,
}

/// The release pipeline's build of the commit: each platform's binary and the image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Build {
    pub platforms: Vec<Platform>,
    pub image: Part,
}

/// Staging and the commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Staging {
    NotYet,
    Syncing,
    /// Healthy, and soaking before production may take it (times in ms).
    Soaking { since: i64, until: i64 },
    Healthy,
    /// In staging inside a later commit's sync, never on its own.
    Rode { with: String },
    /// It was in staging; staging has moved on.
    Moved,
    /// Staging skips a commit whose checks failed.
    Skipped,
}

/// Production and the commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Production {
    NotYet,
    /// It ships with the commit production waits on, once that's approved.
    Rides { with: String },
    /// Production waits for a person to approve it, `commits` shipping with it (itself included).
    WaitsForApproval { commits: u32 },
    /// Approved: the release is committed and kiln restarts into it.
    Delivering,
    /// The guard watches production run it, ready to put the build before back (times in ms).
    Guard { since: i64, until: i64 },
    /// Production runs it.
    Runs,
    /// Shipped inside a later commit's release, never on its own.
    Rode { with: String },
    /// It ran in production; production has moved on.
    Moved,
    /// It didn't hold: production went back to `to`, for `why`; `acknowledged` once a person saw it.
    RolledBack { to: String, why: String, acknowledged: bool },
}

/// One runner's replant onto the build production runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Runner {
    Waiting,
    Switching,
    On,
    Failed,
    Offline,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerReplant {
    pub name: String,
    pub state: Runner,
    /// Why it failed, as the upgrade said.
    pub why: String,
}

/// The server, then each runner, onto the build production runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Replant {
    pub server: Part,
    pub runners: Vec<RunnerReplant>,
}

/// A commit's records, as the server read them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Journey {
    pub author: Author,
    pub land: Land,
    pub checks: Vec<Check>,
    pub build: Build,
    pub staging: Staging,
    pub production: Production,
    /// The replant of the build production runs, when this commit is in it (its own, or the one it
    /// rode with).
    pub replant: Option<Replant>,
    /// Production runs this very commit: a runner that didn't take it needs you here.
    pub in_production: bool,
    /// Who took its failure on (the agent fixing it).
    pub handled_by: Option<String>,
    /// The commit that fixed its failure.
    pub fixed_by: Option<String>,
}

/// Where a commit stands, in Mission control's words.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Stand {
    NeedsYou,
    Failing,
    Landing,
    Checking,
    Building,
    InStaging,
    Waiting,
    Shipping,
    Replanting,
    Shipped,
    Fixed,
    RolledBack,
}

/// The six stages, land to replant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Land,
    Checks,
    Build,
    Staging,
    Production,
    Replant,
}

/// A stage's state, as the UI's glyph for it is named (ui/src/components/ui/status-glyph-spec.ts).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Glyph {
    None,
    Queued,
    Running,
    Passed,
    Reused,
    Flaky,
    Failed,
    Error,
    Superseded,
    Needs,
    Soaking,
    Guard,
    Healthy,
    Rode,
    #[serde(rename = "rolledback")]
    RolledBack,
    Replanting,
    Replanted,
    Partial,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Pip {
    pub stage: Stage,
    pub state: Glyph,
    /// How far a wait has come (soaking, the guard), 0 to 1.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub p: Option<f64>,
}

/// Where a commit stands, what it's doing now, and its six pips.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Standing {
    pub stand: Stand,
    pub line: String,
    pub pips: Vec<Pip>,
}

/// A leg's result: its last attempt; a pass after the code failed an earlier one is a flake; a
/// pass after kiln's own error is a pass.
pub fn leg_glyph(leg: &Leg) -> Glyph {
    match leg.attempts.last() {
        None | Some(Attempt::Queued) => Glyph::Queued,
        Some(Attempt::Running) => Glyph::Running,
        Some(Attempt::Reused) => Glyph::Reused,
        Some(Attempt::Failed) => Glyph::Failed,
        Some(Attempt::KilnError) => Glyph::Error,
        Some(Attempt::Cancelled) => Glyph::Superseded,
        Some(Attempt::Passed) => {
            let before = leg.attempts.len().saturating_sub(1);
            if leg.attempts.iter().take(before).any(|a| *a == Attempt::Failed) {
                Glyph::Flaky
            } else {
                Glyph::Passed
            }
        }
    }
}

/// Every check's legs together: failed if any failed, under way while any runs (kiln's errors are
/// retried, so still under way), reused if every one was, flaky if any flaked.
fn checks_glyph(checks: &[Check]) -> Glyph {
    let legs: Vec<Glyph> = checks.iter().flat_map(|c| c.legs.iter().map(leg_glyph)).collect();
    if legs.is_empty() {
        return Glyph::None;
    }
    if legs.contains(&Glyph::Failed) {
        Glyph::Failed
    } else if legs.iter().any(|g| matches!(g, Glyph::Running | Glyph::Queued | Glyph::Error)) {
        Glyph::Running
    } else if legs.iter().all(|g| *g == Glyph::Reused) {
        Glyph::Reused
    } else if legs.contains(&Glyph::Flaky) {
        Glyph::Flaky
    } else {
        Glyph::Passed
    }
}

fn build_glyph(build: &Build) -> Glyph {
    let parts = build.platforms.iter().map(|p| p.state).chain(std::iter::once(build.image));
    let parts: Vec<Part> = parts.collect();
    if build.image == Part::Failed {
        Glyph::Failed
    } else if parts.iter().any(|p| matches!(p, Part::Queued | Part::Running)) {
        Glyph::Running
    } else if build.platforms.iter().any(|p| p.state == Part::Failed) {
        Glyph::Partial
    } else {
        Glyph::Passed
    }
}

/// How far through `since..until` `now` is.
fn share(now: i64, since: i64, until: i64) -> f64 {
    if until <= since {
        return 1.0;
    }
    ((now - since) as f64 / (until - since) as f64).clamp(0.0, 1.0)
}

fn live(replant: &Replant) -> (usize, usize) {
    let live = replant.runners.iter().filter(|r| r.state != Runner::Offline);
    let total = live.clone().count();
    let on = live.filter(|r| r.state == Runner::On).count();
    (on, total)
}

fn replant_glyph(j: &Journey) -> Glyph {
    let shipped = matches!(j.production, Production::Runs | Production::Rode { .. } | Production::Moved);
    match (&j.replant, shipped) {
        (Some(r), true) if r.runners.iter().any(|x| x.state == Runner::Failed) => Glyph::Partial,
        (Some(r), true) if r.server == Part::Running || r.runners.iter().any(|x| matches!(x.state, Runner::Waiting | Runner::Switching)) => Glyph::Replanting,
        (Some(_), true) => Glyph::Replanted,
        _ => Glyph::None,
    }
}

/// The six pips, land to replant.
pub fn pips(j: &Journey, now: i64) -> Vec<Pip> {
    let pip = |stage, state| Pip { stage, state, p: None };
    let land = match j.land {
        Land::Queued { .. } => Glyph::Queued,
        Land::Testing => Glyph::Running,
        Land::Landed => Glyph::Passed,
        Land::Ejected { .. } => Glyph::Failed,
    };
    let staging = match &j.staging {
        Staging::NotYet => pip(Stage::Staging, Glyph::None),
        Staging::Syncing => pip(Stage::Staging, Glyph::Running),
        Staging::Soaking { since, until } => Pip { stage: Stage::Staging, state: Glyph::Soaking, p: Some(share(now, *since, *until)) },
        Staging::Healthy => pip(Stage::Staging, Glyph::Healthy),
        Staging::Rode { .. } => pip(Stage::Staging, Glyph::Rode),
        Staging::Moved => pip(Stage::Staging, Glyph::Passed),
        Staging::Skipped => pip(Stage::Staging, Glyph::Superseded),
    };
    let production = match &j.production {
        Production::NotYet => pip(Stage::Production, Glyph::None),
        Production::Rides { .. } => pip(Stage::Production, Glyph::Queued),
        Production::WaitsForApproval { .. } => pip(Stage::Production, Glyph::Needs),
        Production::Delivering => pip(Stage::Production, Glyph::Running),
        Production::Guard { since, until } => Pip { stage: Stage::Production, state: Glyph::Guard, p: Some(share(now, *since, *until)) },
        Production::Runs => pip(Stage::Production, Glyph::Healthy),
        Production::Rode { .. } => pip(Stage::Production, Glyph::Rode),
        Production::Moved => pip(Stage::Production, Glyph::Passed),
        Production::RolledBack { .. } => pip(Stage::Production, Glyph::RolledBack),
    };
    let build = if j.land == Land::Landed { build_glyph(&j.build) } else { Glyph::None };
    vec![
        pip(Stage::Land, land),
        pip(Stage::Checks, checks_glyph(&j.checks)),
        pip(Stage::Build, build),
        staging,
        production,
        pip(Stage::Replant, replant_glyph(j)),
    ]
}

fn first_failure(checks: &[Check]) -> Option<&Leg> {
    checks.iter().flat_map(|c| c.legs.iter()).find(|l| leg_glyph(l) == Glyph::Failed)
}

fn plural(n: u32, one: &str) -> String {
    if n == 1 {
        format!("{n} {one}")
    } else {
        format!("{n} {one}s")
    }
}

/// Where a commit stands, the one line that says what it's doing, and its pips, `now` in ms.
pub fn standing(j: &Journey, now: i64) -> Standing {
    let pips = pips(j, now);
    let (stand, line) = stand_and_line(j);
    Standing { stand, line, pips }
}

fn stand_and_line(j: &Journey) -> (Stand, String) {
    let checks = checks_glyph(&j.checks);
    let failure = first_failure(&j.checks);
    let failure_line = || failure.map(|l| format!("{} · {}", l.name, l.note)).unwrap_or_else(|| "checks failed".to_string());
    let failed_runner = j.replant.as_ref().and_then(|r| r.runners.iter().find(|x| x.state == Runner::Failed));

    // What needs you first: an approval, a person's own failure no one has taken on, a rollback
    // not yet seen, a runner that didn't take the build production runs.
    if let Production::WaitsForApproval { commits } = j.production {
        return (Stand::NeedsYou, format!("Approve for production · {}", plural(commits, "commit")));
    }
    if checks == Glyph::Failed && matches!(j.author, Author::Person(_)) && j.handled_by.is_none() && j.fixed_by.is_none() {
        return (Stand::NeedsYou, failure_line());
    }
    if let Production::RolledBack { to, why, acknowledged: false } = &j.production {
        return (Stand::NeedsYou, format!("Rolled back to {to} · {why}"));
    }
    if let (true, Some(r)) = (j.in_production, failed_runner) {
        let why = r.why.split(':').next().unwrap_or_default().trim();
        return (Stand::NeedsYou, format!("{} didn't replant · {why}", r.name));
    }

    if checks == Glyph::Failed {
        return match (&j.fixed_by, &j.handled_by) {
            (Some(fix), _) => (Stand::Fixed, format!("Fixed in {fix}")),
            (None, Some(by)) => (Stand::Failing, format!("{by} fixing · {}", failure.map(|l| l.note.as_str()).unwrap_or("checks failed"))),
            (None, None) => (Stand::Failing, failure_line()),
        };
    }
    match &j.land {
        Land::Queued { ahead } => return (Stand::Landing, format!("Queued · {ahead} ahead")),
        Land::Testing => return (Stand::Landing, "Testing against trunk".to_string()),
        Land::Ejected { why } => return (Stand::Failing, format!("Didn't land · {why}")),
        Land::Landed => {}
    }
    if checks == Glyph::Running {
        let retrying = j.checks.iter().flat_map(|c| c.legs.iter()).find(|l| l.attempts.len() > 1 && matches!(l.attempts.last(), Some(Attempt::Running | Attempt::Queued)));
        if let Some(leg) = retrying {
            return (Stand::Checking, format!("Retrying · {}", leg.name));
        }
        let running: Vec<&str> = j.checks.iter().filter(|c| c.legs.iter().any(|l| matches!(leg_glyph(l), Glyph::Running | Glyph::Queued | Glyph::Error))).map(|c| c.pipeline.as_str()).collect();
        return (Stand::Checking, if running.is_empty() { "Starting".to_string() } else { running.join(" · ") });
    }
    match &j.production {
        Production::Delivering => return (Stand::Shipping, "Restarting into it".to_string()),
        Production::Guard { .. } => return (Stand::Shipping, "Guard watching".to_string()),
        _ => {}
    }
    if let (true, Some(r), Glyph::Replanting) = (j.in_production, &j.replant, replant_glyph(j)) {
        let (on, total) = live(r);
        return (Stand::Replanting, format!("Runners {on}/{total}"));
    }
    match &j.production {
        Production::Runs => {
            let runners = j.replant.as_ref().map(live).map(|(on, total)| format!(" · runners {on}/{total}")).unwrap_or_default();
            return (Stand::Shipped, format!("In production{runners}"));
        }
        Production::Rode { with } => return (Stand::Shipped, format!("With {with}")),
        Production::Moved => return (Stand::Shipped, "Superseded".to_string()),
        Production::RolledBack { to, .. } => return (Stand::RolledBack, format!("Rolled back to {to}")),
        Production::Rides { with } => return (Stand::Waiting, format!("Rides with {with}")),
        _ => {}
    }
    match &j.staging {
        Staging::Syncing => return (Stand::InStaging, "Syncing".to_string()),
        Staging::Soaking { .. } => return (Stand::InStaging, "Soaking".to_string()),
        Staging::Healthy => return (Stand::InStaging, "Healthy".to_string()),
        Staging::Rode { with } => return (Stand::InStaging, format!("With {with}")),
        _ => {}
    }
    if matches!(build_glyph(&j.build), Glyph::Running) && j.build.image != Part::Passed {
        let left: Vec<&str> = j.build.platforms.iter().filter(|p| matches!(p.state, Part::Queued | Part::Running)).map(|p| p.name.as_str()).collect();
        return (Stand::Building, if left.is_empty() { "Building the image".to_string() } else { format!("Building {}", left.join(", ")) });
    }
    (Stand::InStaging, if j.staging == Staging::Moved { "Passed".to_string() } else { "Built".to_string() })
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: i64 = 60_000;

    fn leg(name: &str, attempts: &[Attempt], note: &str) -> Leg {
        Leg { name: name.to_string(), attempts: attempts.to_vec(), note: note.to_string() }
    }

    fn built() -> Build {
        Build {
            platforms: ["Linux", "Windows", "macOS"].iter().map(|n| Platform { name: n.to_string(), state: Part::Passed }).collect(),
            image: Part::Passed,
        }
    }

    /// A commit that landed, passed every check and built: each fixture changes what it's about.
    fn landed(author: Author) -> Journey {
        Journey {
            author,
            land: Land::Landed,
            checks: vec![
                Check { pipeline: "ci".into(), legs: vec![leg("rust lint + test", &[Attempt::Passed], ""), leg("ui typecheck + lint + build", &[Attempt::Passed], "")] },
                Check { pipeline: "e2e".into(), legs: vec![leg("playwright", &[Attempt::Passed], "")] },
            ],
            build: built(),
            staging: Staging::NotYet,
            production: Production::NotYet,
            replant: None,
            in_production: false,
            handled_by: None,
            fixed_by: None,
        }
    }

    fn agent() -> Author {
        Author::Agent { name: "Kiln Distro".into(), for_person: "ada".into() }
    }

    fn pip(s: &Standing, stage: Stage) -> Option<Glyph> {
        s.pips.iter().find(|p| p.stage == stage).map(|p| p.state)
    }

    fn runner(name: &str, state: Runner, why: &str) -> RunnerReplant {
        RunnerReplant { name: name.into(), state, why: why.into() }
    }

    #[test]
    fn ship_a_flaky_pass_soaks_in_staging_with_its_checks_flaky() {
        let mut j = landed(agent());
        if let Some(c) = j.checks.get_mut(1) {
            c.legs = vec![leg("playwright", &[Attempt::Failed, Attempt::Passed], "login rings settle failed once (WebKit)")];
        }
        j.staging = Staging::Soaking { since: 0, until: 90_000 };
        let s = standing(&j, 30_000);
        assert_eq!(s.stand, Stand::InStaging);
        assert_eq!(s.line, "Soaking");
        assert_eq!(pip(&s, Stage::Checks), Some(Glyph::Flaky));
        let soak = s.pips.iter().find(|p| p.stage == Stage::Staging).and_then(|p| p.p);
        assert!(matches!(soak, Some(p) if (p - 1.0 / 3.0).abs() < 1e-9), "{soak:?}");
    }

    #[test]
    fn ship_a_pass_after_kilns_own_error_is_a_pass_not_a_flake() {
        let l = leg("rust lint + test", &[Attempt::KilnError, Attempt::Passed], "metal-ryzen-02 lost the leg");
        assert_eq!(leg_glyph(&l), Glyph::Passed);
        assert_eq!(leg_glyph(&leg("x", &[Attempt::KilnError], "")), Glyph::Error);
    }

    #[test]
    fn ship_a_failure_its_agent_fixed_is_failing_while_fixing_then_fixed() {
        let mut j = landed(agent());
        if let Some(c) = j.checks.get_mut(0) {
            c.legs = vec![leg("rust lint + test", &[Attempt::Failed], "assertion failed in sync::reconnect")];
        }
        j.staging = Staging::Skipped;
        j.handled_by = Some("Kiln Distro".into());
        let s = standing(&j, 0);
        assert_eq!(s.stand, Stand::Failing);
        assert_eq!(s.line, "Kiln Distro fixing · assertion failed in sync::reconnect");
        assert_eq!(pip(&s, Stage::Checks), Some(Glyph::Failed));
        assert_eq!(pip(&s, Stage::Staging), Some(Glyph::Superseded));

        j.fixed_by = Some("5966eb9".into());
        let s = standing(&j, 0);
        assert_eq!(s.stand, Stand::Fixed);
        assert_eq!(s.line, "Fixed in 5966eb9");
    }

    #[test]
    fn ship_a_persons_failure_no_one_took_on_needs_them() {
        let mut j = landed(Author::Person("mei".into()));
        if let Some(c) = j.checks.get_mut(0) {
            c.legs = vec![leg("rust lint + test", &[Attempt::Failed], "assertion failed in sync::reconnect")];
        }
        let s = standing(&j, 0);
        assert_eq!(s.stand, Stand::NeedsYou);
        assert_eq!(s.line, "rust lint + test · assertion failed in sync::reconnect");
    }

    #[test]
    fn ship_a_release_waiting_with_riders_needs_you_and_its_riders_wait() {
        let mut j = landed(Author::Person("ada".into()));
        j.staging = Staging::Healthy;
        j.production = Production::WaitsForApproval { commits: 7 };
        let s = standing(&j, 0);
        assert_eq!(s.stand, Stand::NeedsYou);
        assert_eq!(s.line, "Approve for production · 7 commits");
        assert_eq!(pip(&s, Stage::Production), Some(Glyph::Needs));

        let mut rider = landed(agent());
        rider.staging = Staging::Rode { with: "81a6414".into() };
        rider.production = Production::Rides { with: "f51748c".into() };
        let s = standing(&rider, 0);
        assert_eq!(s.stand, Stand::Waiting);
        assert_eq!(s.line, "Rides with f51748c");
        assert_eq!(pip(&s, Stage::Production), Some(Glyph::Queued));
        assert_eq!(pip(&s, Stage::Staging), Some(Glyph::Rode));
    }

    #[test]
    fn ship_a_guard_rollback_needs_you_until_seen() {
        let mut j = landed(agent());
        j.staging = Staging::Moved;
        j.production = Production::Guard { since: 0, until: 10 * MIN };
        let s = standing(&j, 6 * MIN);
        assert_eq!(s.stand, Stand::Shipping);
        assert_eq!(s.line, "Guard watching");
        assert_eq!(pip(&s, Stage::Production), Some(Glyph::Guard));

        j.production = Production::RolledBack { to: "34e18fb".into(), why: "server restarted (OOM)".into(), acknowledged: false };
        let s = standing(&j, 11 * MIN);
        assert_eq!(s.stand, Stand::NeedsYou);
        assert_eq!(s.line, "Rolled back to 34e18fb · server restarted (OOM)");
        assert_eq!(pip(&s, Stage::Production), Some(Glyph::RolledBack));

        j.production = Production::RolledBack { to: "34e18fb".into(), why: "server restarted (OOM)".into(), acknowledged: true };
        assert_eq!(standing(&j, 12 * MIN).stand, Stand::RolledBack);
    }

    #[test]
    fn ship_a_runner_that_did_not_replant_needs_you() {
        let mut j = landed(agent());
        j.staging = Staging::Moved;
        j.production = Production::Runs;
        j.in_production = true;
        j.replant = Some(Replant {
            server: Part::Passed,
            runners: vec![
                runner("metal-ryzen-01", Runner::On, ""),
                runner("metal-epyc-01", Runner::On, ""),
                runner("pi-04", Runner::Failed, "upgrade timed out: no report within 15 minutes"),
                runner("nuc-old", Runner::Offline, ""),
            ],
        });
        let s = standing(&j, 0);
        assert_eq!(s.stand, Stand::NeedsYou);
        assert_eq!(s.line, "pi-04 didn't replant · upgrade timed out");
        assert_eq!(pip(&s, Stage::Replant), Some(Glyph::Partial));

        if let Some(r) = j.replant.as_mut().and_then(|r| r.runners.get_mut(2)) {
            r.state = Runner::Switching;
        }
        let s = standing(&j, 0);
        assert_eq!(s.stand, Stand::Replanting);
        assert_eq!(s.line, "Runners 2/3");
        assert_eq!(pip(&s, Stage::Replant), Some(Glyph::Replanting));

        if let Some(r) = j.replant.as_mut().and_then(|r| r.runners.get_mut(2)) {
            r.state = Runner::On;
        }
        let s = standing(&j, 0);
        assert_eq!(s.stand, Stand::Shipped);
        assert_eq!(s.line, "In production · runners 3/3");
        assert_eq!(pip(&s, Stage::Replant), Some(Glyph::Replanted));
    }

    #[test]
    fn ship_landing_checking_and_building_in_order() {
        let mut j = landed(agent());
        j.land = Land::Queued { ahead: 2 };
        assert_eq!(stand_and_line(&j), (Stand::Landing, "Queued · 2 ahead".to_string()));
        j.land = Land::Landed;
        if let Some(c) = j.checks.get_mut(1) {
            c.legs = vec![leg("playwright", &[Attempt::Running], "")];
        }
        assert_eq!(stand_and_line(&j), (Stand::Checking, "e2e".to_string()));
        j = landed(agent());
        j.build.image = Part::Running;
        if let Some(p) = j.build.platforms.get_mut(2) {
            p.state = Part::Running;
        }
        assert_eq!(stand_and_line(&j), (Stand::Building, "Building macOS".to_string()));
    }

    #[test]
    fn ship_glyph_names_are_the_uis() {
        let names: Vec<String> = [Glyph::RolledBack, Glyph::Needs, Glyph::Replanted, Glyph::None]
            .iter()
            .filter_map(|g| serde_json::to_value(g).ok())
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect();
        assert_eq!(names, vec!["rolledback", "needs", "replanted", "none"]);
        assert_eq!(serde_json::to_value(Stand::InStaging).ok(), Some(serde_json::json!("in_staging")));
    }
}
