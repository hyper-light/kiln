//! Every runner on the newest kiln (`[updates] follow`).
//!
//! The newest kiln is a commit of the release branch (`[updates] release_branch`): of the commits
//! the server and the connected runners run, the one the others lead to, as the server's mirror of
//! the release repository has them. When it moves (the server was deployed, a runner was upgraded
//! or built itself) every other connected runner is told, and catches up however it can:
//!
//! 1. It downloads a binary of that commit for its platform, when kiln has one: the server's own,
//!    one the release pipeline built, or one a machine built for its platform.
//! 2. Else a machine builds the commit from the server's archive of it, in a directory of its own
//!    (`~/.cache/kiln/self-build`, a source tree per build beside one cache of compiled crates). No
//!    checkout of kiln is needed anywhere: who builds is decided by the tools a machine has (cargo
//!    and node, as its facts report them). A runner that has them builds the commit itself and
//!    installs that build. A Mac hands its binary back, so the other Macs download it instead of
//!    building; a Linux build links the machine's own C library, so it is that machine's alone.
//! 3. A Mac that can't build kiln itself (one without developer tools) gets its build over the
//!    wire: another Mac with the tools builds the commit natively, else a Linux machine cross-builds
//!    it (`ci/release/build.sh`: zig, against the macOS stubs). The binary comes back to the server
//!    and the Mac downloads it, in the same upgrade. One build of a commit for a platform serves
//!    every machine of it: a runner that needs a build already under way waits for that one.
//! 4. Else it waits for a build of the commit, and says what for.
//!
//! Nothing a runner runs is stopped: it takes a new binary in place once it is idle. A machine's
//! commit is the one it reports ([`crate::proto::AgentMsg::Facts`]), else the one kiln knows its
//! build by: every binary kiln has built, kept or seen built is remembered with its commit
//! (`<data>/binaries/builds.json`). A runner on a commit off the release branch, or on a local
//! build that knows no commit, runs something on purpose and is left alone.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::hub::Hub;
use crate::updates::{Binary, Job, RunnerInfo};

/// How often the fleet is looked at when nothing wakes it.
const EVERY: Duration = Duration::from_secs(60);
/// How long a commit not found on the release branch is believed to be off it.
const OFF_BRANCH_TTL: Duration = Duration::from_secs(10 * 60);
/// How long a commit the mirror couldn't fetch (not pushed yet, or GitHub out of reach) is left
/// before it is looked for again.
const UNKNOWN_TTL: Duration = Duration::from_secs(2 * 60);
/// Tries of one commit on one machine (a download or a build) before it is left to wait.
const TRIES: u32 = 2;
/// Between two tries.
const RETRY: Duration = Duration::from_secs(30 * 60);
/// A machine whose build found it lacks a tool to build kiln isn't asked to build again for this
/// long.
const CANT_BUILD_TTL: Duration = Duration::from_secs(24 * 60 * 60);
/// How often a runner waiting for a build another machine makes looks for its binary.
const JOIN_POLL: Duration = Duration::from_secs(2);
/// How long the new runner has to report in after its install finished.
const REPORT_IN: Duration = Duration::from_secs(90);
/// Builds remembered with their commit.
const KEEP_BUILDS: usize = 400;
/// Git questions to the mirror.
const GIT_TIMEOUT: Duration = Duration::from_secs(60);

/// What a build script prints for the server to read (`OK_MARK` and `FAIL_MARK` as an upgrade
/// through a terminal prints them): the SHA-256 of what it built, and that the machine can't
/// build kiln (a tool missing), with why.
const BUILT_MARK: &str = "KILN-BUILT";
const CANT_BUILD_MARK: &str = "KILN-CANT-BUILD";

/// The tools a machine builds kiln with natively, as its facts name them (npm comes with node).
const NATIVE_TOOLS: [&str; 2] = ["cargo", "node"];
/// The tools a Linux machine cross-builds kiln for a Mac with: `ci/release/build.sh` fetches its
/// own zig with python3.
const CROSS_TOOLS: [&str; 3] = ["cargo", "node", "python3"];
/// What building natively counts for when a machine is picked to build for another: more than any
/// room a machine has, less than its being overloaded (`crate::reach::directory::OVERLOADED`).
const NATIVE: f32 = 1.0e3;

/// The usual homes of a checkout of kiln, which a runner reports where it has one
/// (`crate::updates::kiln_checkout`). Nothing needs one to build kiln.
pub(crate) const USUAL: [&str; 12] =
    ["Projects/kiln", "projects/kiln", "src/kiln", "code/kiln", "Code/kiln", "dev/kiln", "Developer/kiln", "git/kiln", "repos/kiln", "workspace/kiln", "work/kiln", "kiln"];

/// The commit every runner is brought to.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Target {
    pub commit: String,
    /// Who ran it first: "server", or a machine's name.
    pub from: String,
    /// Since when it is the newest (unix seconds).
    pub since: i64,
}

/// A build kiln knows the commit of.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Known {
    commit: String,
    target: String,
    at: i64,
}

/// A build of the newest kiln a machine is making: of itself, or for another machine's platform.
#[derive(Debug, Clone)]
struct Building {
    /// Where it's built.
    machine: String,
    /// The platform it's built for.
    target: String,
    commit: String,
    /// Its binary is kept here for the machines of its platform.
    keep: bool,
}

/// Tries of one commit on one machine.
#[derive(Debug, Clone)]
struct Tries {
    n: u32,
    last: Instant,
    error: Option<String>,
    /// The last one installed it: the runner takes it once it is idle.
    installed: bool,
}

#[derive(Default)]
pub struct Fleet {
    target: Mutex<Option<Target>>,
    wake: crate::rt::Notify,
    /// Loaded on first use.
    builds: Mutex<Option<BTreeMap<String, Known>>>,
    /// Upgrade job → the build it is.
    building: Mutex<HashMap<String, Building>>,
    /// (machine, commit) → tries so far.
    tries: Mutex<HashMap<(String, String), Tries>>,
    /// Machines whose build found they lack a tool to build kiln: when, and why.
    cant_build: Mutex<HashMap<String, (Instant, String)>>,
    /// Commits looked up on the release branch.
    on_branch: Mutex<HashMap<String, Branch>>,
    /// Machines whose own kiln config refuses upgrades (`[updates] accept = false`).
    refusing: Mutex<std::collections::HashSet<String>>,
    /// (machine, commit): told a newer kiln is available.
    told: Mutex<std::collections::HashSet<(String, String)>>,
}

/// A commit against the release branch, as last looked up.
#[derive(Debug, Clone, Copy)]
enum Branch {
    /// On it, for good.
    On,
    /// Off it, as of then.
    Off(Instant),
    /// The mirror couldn't fetch it, as of then.
    Unknown(Instant),
}

impl Fleet {
    /// Look at the fleet again now: a runner reported in, a binary arrived, an upgrade ended.
    pub fn wake(&self) {
        self.wake.notify();
    }

    /// What `machine`'s own kiln config says of upgrades ([`crate::proto::AgentMsg::UpdatePrefs`]).
    pub fn set_accept(&self, machine: &str, accept: bool) {
        let mut g = self.refusing.lock();
        if accept {
            g.remove(machine);
        } else {
            g.insert(machine.to_owned());
        }
    }

    /// `machine`'s own kiln config refuses upgrades.
    pub fn refuses(&self, machine: &str) -> bool {
        self.refusing.lock().contains(machine)
    }

    /// The newest kiln, as last decided.
    pub fn target(&self) -> Option<Target> {
        self.target.lock().clone()
    }

    fn entry<R>(&self, machine: &str, commit: &str, f: impl FnOnce(&mut Tries) -> R) -> R {
        let mut g = self.tries.lock();
        f(g.entry((machine.to_owned(), commit.to_owned())).or_insert(Tries { n: 0, last: Instant::now(), error: None, installed: false }))
    }

    /// A try of `commit` on `machine` starts.
    fn tried(&self, machine: &str, commit: &str) {
        self.entry(machine, commit, |t| {
            t.n += 1;
            t.last = Instant::now();
            t.error = None;
            t.installed = false;
        });
    }

    /// That try failed (`Some`), or installed the commit (`None`).
    fn ended(&self, machine: &str, commit: &str, error: Option<String>) {
        self.entry(machine, commit, |t| {
            t.installed = error.is_none();
            t.error = error;
        });
    }

    /// Whether `commit` may be tried on `machine` now; else why not, as the end of a sentence
    /// about the machine.
    fn may_try(&self, machine: &str, commit: &str) -> Result<(), String> {
        let g = self.tries.lock();
        let Some(t) = g.get(&(machine.to_owned(), commit.to_owned())) else { return Ok(()) };
        let s = short(commit);
        let waited = t.last.elapsed();
        if t.installed && waited < RETRY {
            return Err(format!("has {s} installed, and takes it once it is idle"));
        }
        let why = t.error.clone().unwrap_or_else(|| "the last try didn't finish".to_owned());
        if t.n >= TRIES {
            return Err(format!("couldn't reach {s} in {} tries: {why}", t.n));
        }
        if waited < RETRY {
            return Err(format!("tries {s} again in {} min: {why}", (RETRY - waited).as_secs() / 60 + 1));
        }
        Ok(())
    }
}

pub(crate) fn short(commit: &str) -> &str {
    commit.get(..12).unwrap_or(commit)
}

fn is_commit(s: &str) -> bool {
    (7..=64).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

// ------------------------------------------------------------------ which commit a build is

fn builds_path(hub: &Hub) -> PathBuf {
    hub.cfg.server.data_dir.join("binaries").join("builds.json")
}

fn load_builds(hub: &Hub) -> BTreeMap<String, Known> {
    std::fs::read(builds_path(hub)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

/// Remember that `build` (a binary for `target`) is `commit`.
pub fn remember(hub: &Hub, build: &str, target: &str, commit: &str) {
    if build.is_empty() || build == "unknown" || !is_commit(commit) {
        return;
    }
    let commit = commit.to_ascii_lowercase();
    let mut g = hub.updates.fleet.builds.lock();
    let map = g.get_or_insert_with(|| load_builds(hub));
    if map.get(build).is_some_and(|k| k.commit == commit) {
        return;
    }
    map.insert(build.to_owned(), Known { commit, target: target.to_owned(), at: crate::util::now_unix() });
    while map.len() > KEEP_BUILDS {
        let Some(oldest) = map.iter().min_by_key(|(_, k)| k.at).map(|(b, _)| b.clone()) else { break };
        map.remove(&oldest);
    }
    let path = builds_path(hub);
    let write = || -> anyhow::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.part");
        std::fs::write(&tmp, serde_json::to_vec(&*map)?)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    };
    if let Err(e) = write() {
        tracing::warn!(error = %format!("{e:#}"), "couldn't remember a build's commit");
    }
}

/// The commit `build` was made from, as far as kiln knows.
pub fn commit_of(hub: &Hub, build: &str) -> Option<String> {
    if let Some(c) = crate::updates::binaries(hub).into_iter().find(|b| b.build == build).and_then(|b| b.sha) {
        return Some(c);
    }
    let mut g = hub.updates.fleet.builds.lock();
    g.get_or_insert_with(|| load_builds(hub)).get(build).map(|k| k.commit.clone())
}

/// What a machine's runner says of itself ([`crate::proto::AgentMsg::Facts`]): `None` from
/// runners from before.
pub fn reported(hub: &Hub, machine: &str) -> Option<kiln_ci::facts::KilnFacts> {
    hub.runner_kiln(machine)
}

/// A binary of `commit` for `target` that kiln can hand out.
pub fn binary_for(hub: &Hub, target: &str, commit: &str) -> Option<Binary> {
    crate::updates::binaries(hub).into_iter().find(|b| b.target == target && b.sha.as_deref().is_some_and(|s| s.eq_ignore_ascii_case(commit)))
}

// ------------------------------------------------------------------ the newest commit

async fn release_repo(hub: &Hub) -> Option<crate::store::Repo> {
    let name = &hub.cfg.updates.release_repo;
    hub.store.repos().await.ok()?.into_iter().find(|r| &r.name == name)
}

/// Whether `commit` is on the release branch (fetched into the mirror first when it isn't there
/// yet); `None` when the mirror couldn't fetch it: not pushed yet, or GitHub out of reach.
async fn on_branch(hub: &Hub, repo: &crate::store::Repo, commit: &str) -> Option<bool> {
    match hub.updates.fleet.on_branch.lock().get(commit) {
        Some(Branch::On) => return Some(true),
        Some(Branch::Off(at)) if at.elapsed() < OFF_BRANCH_TTL => return Some(false),
        Some(Branch::Unknown(at)) if at.elapsed() < UNKNOWN_TTL => return None,
        _ => {}
    }
    let (seen, answer) = match crate::gitops::ensure_commit(hub, repo, commit).await {
        Ok(path) => {
            let p = path.to_string_lossy().to_string();
            let branch = format!("refs/heads/{}", hub.cfg.updates.release_branch);
            let on = crate::gitops::run_git(hub, &["-C", &p, "merge-base", "--is-ancestor", commit, &branch], GIT_TIMEOUT).await.is_ok();
            (if on { Branch::On } else { Branch::Off(Instant::now()) }, Some(on))
        }
        Err(_) => (Branch::Unknown(Instant::now()), None),
    };
    hub.updates.fleet.on_branch.lock().insert(commit.to_owned(), seen);
    answer
}

async fn is_ancestor(hub: &Hub, mirror: &str, a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b) || crate::gitops::run_git(hub, &["-C", mirror, "merge-base", "--is-ancestor", a, b], GIT_TIMEOUT).await.is_ok()
}

async fn commit_time(hub: &Hub, mirror: &str, commit: &str) -> i64 {
    crate::gitops::run_git(hub, &["-C", mirror, "show", "-s", "--format=%ct", commit], GIT_TIMEOUT)
        .await
        .ok()
        .and_then(|o| String::from_utf8_lossy(&o).trim().parse().ok())
        .unwrap_or(0)
}

/// The build a machine's newest runner process runs.
pub(crate) fn newest_build(hub: &Hub, machine: &str) -> Option<String> {
    hub.runner_processes(machine).into_iter().find(|i| i.newest).and_then(|i| i.build)
}

/// The commit a machine's runner runs: what it reports, else what kiln knows its build as.
pub fn runner_commit(hub: &Hub, machine: &str, build: Option<&str>) -> Option<String> {
    if let Some(k) = reported(hub, machine)
        && is_commit(&k.commit)
    {
        return Some(k.commit.to_ascii_lowercase());
    }
    build.and_then(|b| commit_of(hub, b))
}

/// Decide the newest commit again, and say so when it moved.
pub async fn refresh(hub: &Arc<Hub>) -> Option<Target> {
    let repo = release_repo(hub).await?;
    let mirror = crate::gitops::mirror_path(hub, &repo.name).to_string_lossy().to_string();
    // The server first: of two machines on the same commit, the server is named.
    let mut seen: Vec<(String, String)> = Vec::new();
    if is_commit(crate::api::COMMIT) {
        seen.push((crate::api::COMMIT.to_owned(), "server".to_owned()));
    }
    for a in hub.agent_list().await.into_iter().filter(|a| a.connected && !a.ephemeral && !a.outpost) {
        let build = newest_build(hub, &a.name);
        if let Some(c) = runner_commit(hub, &a.name, build.as_deref())
            && !seen.iter().any(|(s, _)| s.eq_ignore_ascii_case(&c))
        {
            seen.push((c, a.name));
        }
    }
    let mut best: Option<(String, String)> = None;
    for (c, who) in seen {
        if on_branch(hub, &repo, &c).await != Some(true) {
            continue;
        }
        best = Some(match best {
            None => (c, who),
            Some((b, bw)) => {
                if is_ancestor(hub, &mirror, &b, &c).await {
                    (c, who)
                } else if is_ancestor(hub, &mirror, &c, &b).await {
                    (b, bw)
                } else if commit_time(hub, &mirror, &c).await > commit_time(hub, &mirror, &b).await {
                    // Both on the branch, neither leading to the other (one came in by a merge):
                    // the later one.
                    (c, who)
                } else {
                    (b, bw)
                }
            }
        });
    }
    // Nothing qualifying right now (the machine on the newest went away): the newest stays.
    let moved = {
        let mut g = hub.updates.fleet.target.lock();
        match best {
            Some((c, who)) if g.as_ref().is_none_or(|t| !t.commit.eq_ignore_ascii_case(&c)) => {
                let t = Target { commit: c, from: who, since: crate::util::now_unix() };
                *g = Some(t.clone());
                Some(t)
            }
            _ => None,
        }
    };
    if let Some(t) = &moved {
        tracing::info!(commit = short(&t.commit), from = %t.from, "the newest kiln: every runner follows it");
        hub.store.audit("kiln".into(), "upgrade.target".into(), format!("{} runs {}, the newest kiln: every runner follows it", t.from, short(&t.commit))).await;
        hub.updates.wake_in_step();
        hub.emit(&serde_json::json!({ "type": "updates" }));
    }
    hub.updates.fleet.target()
}

// ------------------------------------------------------------------ where each runner stands

/// Where a runner stands against the newest kiln.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Standing {
    /// It runs the newest commit.
    Current,
    /// It runs an older commit of the release branch.
    Behind,
    /// kiln doesn't know which commit it runs (a runner from before, on a build kiln never saw):
    /// it is brought to the newest.
    Unknown,
    /// It runs a commit off the release branch: something on purpose, left alone.
    Branch,
    /// A local build that knows no commit (a tree with changes): left alone.
    Local,
}

/// Where `r` stands against `t`.
pub async fn standing(hub: &Hub, r: &RunnerInfo, t: &Target) -> Standing {
    if let Some(c) = &r.commit {
        if c.eq_ignore_ascii_case(&t.commit) {
            return Standing::Current;
        }
        // A commit the mirror can't find (not pushed yet) is something on purpose too, until it can.
        let on = match release_repo(hub).await {
            Some(repo) => on_branch(hub, &repo, c).await == Some(true),
            None => true,
        };
        return if on { Standing::Behind } else { Standing::Branch };
    }
    if reported(hub, &r.name).is_some() {
        return Standing::Local;
    }
    // A build kiln has of the newest commit, and the machine runs it.
    let target = r.target.as_deref().unwrap_or_default();
    if r.build.is_some() && binary_for(hub, target, &t.commit).is_some_and(|b| Some(&b.build) == r.build.as_ref()) {
        return Standing::Current;
    }
    Standing::Unknown
}

/// How a runner behind catches up.
#[derive(Debug, Clone)]
pub enum Plan {
    /// It downloads this binary of the newest commit.
    Download(Binary),
    /// A machine builds the newest commit for it, from the server's source ([`Builder`]).
    Build(Builder),
    /// A build of the newest commit for its platform is under way (upgrade job `job`, on machine
    /// `on`): it downloads that build once it's kept here.
    Join { job: String, on: String },
    /// Nothing it can do now: why, as the end of a sentence about the machine ("waits for …").
    Wait(String),
}

/// The machine that builds the newest kiln for a runner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Builder {
    pub machine: String,
    /// The runner itself, which installs what it built; else another machine, which hands the
    /// binary back to the server for the runner to download.
    pub itself: bool,
    /// Cross-built on Linux (`ci/release/build.sh`), rather than natively.
    pub cross: bool,
    /// Why the runner doesn't build it itself, when another machine does.
    pub why: String,
}

/// A release run building what a runner waits for, and how far it is: what the runner's row says
/// and links to.
#[derive(Debug, Clone, Serialize)]
pub struct ReleaseRun {
    /// The run's id (its page) and number.
    pub run: String,
    pub number: i64,
    /// Its job's step under way.
    pub step: Option<String>,
    /// When that step (else the job) started, in seconds.
    pub since: Option<i64>,
    /// The machine its job runs on.
    pub on: Option<String>,
}

impl ReleaseRun {
    /// "run #232: its build step, on adacachy, 11 min in".
    fn words(&self, now: i64) -> String {
        let mut w = format!("run #{}", self.number);
        let mut parts: Vec<String> = Vec::new();
        if let Some(s) = &self.step {
            parts.push(format!("its {s} step"));
        }
        if let Some(o) = &self.on {
            parts.push(format!("on {o}"));
        }
        if let Some(t) = self.since {
            parts.push(format!("{} min in", (now - t).max(0) / 60));
        }
        if !parts.is_empty() {
            w.push_str(": ");
            w.push_str(&parts.join(", "));
        }
        w
    }
}

/// A run of the release pipeline building `commit` for `target` right now (its job for that
/// platform running), and where it is. With `queued`, one whose job for it is yet to start counts
/// too; never one whose job for it has ended (a run whose macOS job failed builds no Mac's binary,
/// however long its other jobs go on).
async fn release_building(hub: &Hub, commit: &str, target: &str, queued: bool) -> Option<ReleaseRun> {
    let u = &hub.cfg.updates;
    let repo = release_repo(hub).await?;
    let runs = hub.store.runs(Some(repo.id.clone()), 50, None).await.ok()?;
    for run in runs.into_iter().filter(|r| r.pipeline == u.release_pipeline && r.sha.eq_ignore_ascii_case(commit) && !r.status.is_terminal()) {
        let jobs = hub.store.jobs(run.id.clone(), false).await.unwrap_or_default();
        let Some(j) = jobs.iter().find(|j| j.key == platform_job(target)) else { continue };
        if j.status == crate::store::Status::Running || (queued && !j.status.is_terminal()) {
            let step = j.steps.iter().find(|s| s.status == crate::store::Status::Running);
            return Some(ReleaseRun {
                run: run.id.clone(),
                number: run.number,
                step: step.map(|s| s.name.clone()),
                since: step.and_then(|s| s.started_at).or(j.started_at),
                on: j.agent.clone(),
            });
        }
    }
    None
}

/// The release run `r` waits for to reach `t`, when it waits for one (as [`plan`] decides).
pub async fn waits_on(hub: &Hub, r: &RunnerInfo, t: &Target) -> Option<ReleaseRun> {
    let target = r.target.as_deref()?;
    if binary_for(hub, target, &t.commit).is_some() {
        return None;
    }
    match release_building(hub, &t.commit, target, false).await {
        Some(b) => Some(b),
        None if build_plan(hub, r, target, &t.commit).is_err() => release_building(hub, &t.commit, target, true).await,
        None => None,
    }
}

/// The release pipeline's job that builds `target`'s binary, on a machine of that platform
/// (`.kiln/release.yaml`).
pub fn platform_job(target: &str) -> &'static str {
    if target.ends_with("-apple-darwin") {
        "macos"
    } else if target.contains("-windows-") {
        "windows"
    } else {
        "linux"
    }
}

/// Whether a build for `target` serves every machine of that platform, so it's kept here and can be
/// made on another machine: a Mac's does (natively on a Mac, or cross-built on Linux); a Linux
/// machine's links its own C library, so it is that machine's alone.
fn shared(target: &str) -> bool {
    target.ends_with("-apple-darwin")
}

/// Which of `need` aren't among `tools` (a machine's facts: name → version).
fn lacks<'a>(tools: &BTreeMap<String, String>, need: &[&'a str]) -> Vec<&'a str> {
    need.iter().copied().filter(|t| !tools.contains_key(*t)).collect()
}

/// Why `machine` can't build kiln, as its last build found (a tool missing), while that holds.
fn cant_build(hub: &Hub, machine: &str) -> Option<String> {
    hub.updates.fleet.cant_build.lock().get(machine).filter(|(at, _)| at.elapsed() < CANT_BUILD_TTL).map(|(_, why)| why.clone())
}

/// Whether `r` builds kiln itself: it has the tools to (its facts say so; no checkout of kiln is
/// needed: the source comes from the server); else why not.
pub(crate) fn builds_itself(hub: &Hub, r: &RunnerInfo) -> Result<(), String> {
    if r.os != "linux" && r.os != "macos" {
        return Err("kiln builds itself only on Linux and macOS".to_owned());
    }
    if !r.terminal {
        return Err("terminals are turned off on it, and a build runs in one".to_owned());
    }
    if let Some(why) = cant_build(hub, &r.name) {
        return Err(why);
    }
    let missing = hub.agents.lock().get(&r.name).and_then(|a| a.facts.as_ref().map(|f| lacks(&f.tools, &NATIVE_TOOLS)));
    match missing {
        Some(m) if m.is_empty() => Ok(()),
        Some(m) => Err(format!("it has no {} to build kiln with", m.join(" or "))),
        None => Err("it hasn't said which tools it has".to_owned()),
    }
}

/// A machine that could build kiln for another, as [`pick`] weighs it.
#[derive(Debug, Clone)]
struct Candidate {
    name: String,
    os: String,
    /// The platform its own builds are for.
    target: Option<String>,
    tools: BTreeMap<String, String>,
    /// Its room for more work (`crate::reach::directory::room`).
    room: f32,
}

/// Of `candidates`, the one to build kiln for `target`, and whether it cross-builds: a machine of
/// that platform with the tools builds natively; a Linux machine with the tools cross-builds a
/// Mac's (`ci/release/build.sh`). Natively first, unless that machine is overloaded; then the one
/// with the most room.
fn pick(target: &str, candidates: &[Candidate]) -> Option<(String, bool)> {
    candidates
        .iter()
        .filter_map(|c| {
            if c.target.as_deref() == Some(target) && lacks(&c.tools, &NATIVE_TOOLS).is_empty() {
                Some((c, false, c.room + NATIVE))
            } else if shared(target) && c.os == "linux" && lacks(&c.tools, &CROSS_TOOLS).is_empty() {
                Some((c, true, c.room))
            } else {
                None
            }
        })
        .max_by(|a, b| a.2.total_cmp(&b.2))
        .map(|(c, cross, _)| (c.name.clone(), cross))
}

/// Who builds the newest kiln for `r` (whose platform is `target`): `r` itself when it can; else,
/// for a platform whose builds serve all its machines, another connected machine that can build
/// it (natively, else cross-built on Linux), when `r` can take a download (a runner too old to
/// upgrade itself takes one only through a terminal); else why not.
pub(crate) fn builder(hub: &Hub, r: &RunnerInfo, target: &str) -> Result<Builder, String> {
    let why = match builds_itself(hub, r) {
        Ok(()) => return Ok(Builder { machine: r.name.clone(), itself: true, cross: false, why: String::new() }),
        Err(why) => why,
    };
    if !shared(target) || (r.method != "native" && !r.terminal) {
        return Err(why);
    }
    let candidates: Vec<Candidate> = hub
        .agents
        .lock()
        .values()
        .filter(|a| a.connected && !a.ephemeral && !a.outpost && !a.draining && a.terminal && a.name != r.name)
        .filter_map(|a| {
            let strain = a.sample.as_ref().map(|s| crate::reach::directory::strain(s.cpus, s.cpu_pct, s.load1, s.mem_total, s.mem_used)).unwrap_or_default();
            Some(Candidate {
                name: a.name.clone(),
                os: a.os.clone(),
                target: crate::updates::target_for(&a.os, &a.arch),
                tools: a.facts.as_ref()?.tools.clone(),
                room: crate::reach::directory::room(strain, a.busy, a.slots),
            })
        })
        .collect();
    let candidates: Vec<Candidate> = candidates.into_iter().filter(|c| cant_build(hub, &c.name).is_none()).collect();
    match pick(target, &candidates) {
        Some((machine, cross)) => Ok(Builder { machine, itself: false, cross, why }),
        None => Err(format!("{why}, and no connected machine can build kiln for {target}")),
    }
}

/// A build of `commit` for `target` under way whose binary will be kept here: its upgrade job and
/// the machine building it.
fn under_way(hub: &Hub, target: &str, commit: &str) -> Option<(String, String)> {
    hub.updates.fleet.building.lock().iter().find(|(_, b)| b.keep && b.target == target && b.commit.eq_ignore_ascii_case(commit)).map(|(job, b)| (job.clone(), b.machine.clone()))
}

/// Whether `machine` is building kiln right now (for itself or another machine): its self-build
/// cache is in use.
pub fn builds_on(hub: &Hub, machine: &str) -> bool {
    hub.updates.fleet.building.lock().values().any(|b| b.machine == machine)
}

/// How a runner gets the newest commit built ([`build_plan`]): [`Plan::Join`] or [`Plan::Build`].
enum BuildPlan {
    Join { job: String, on: String },
    Build(Builder),
}

impl From<BuildPlan> for Plan {
    fn from(p: BuildPlan) -> Self {
        match p {
            BuildPlan::Join { job, on } => Plan::Join { job, on },
            BuildPlan::Build(b) => Plan::Build(b),
        }
    }
}

/// How `r` gets the newest commit (`commit`, for its platform `target`) built: by joining a build of
/// it for its platform already under way, else by [`builder`]; else why not. Once kiln has a binary
/// of it, only a build of the runner's own is worth making (a download that keeps failing).
fn build_plan(hub: &Hub, r: &RunnerInfo, target: &str, commit: &str) -> Result<BuildPlan, String> {
    let have = binary_for(hub, target, commit).is_some();
    if !have && let Some((job, on)) = under_way(hub, target, commit) {
        return Ok(BuildPlan::Join { job, on });
    }
    let b = builder(hub, r, target)?;
    if have && !b.itself {
        return Err(format!("{}; kiln has a build of {} for {target} to download", b.why, short(commit)));
    }
    Ok(BuildPlan::Build(b))
}

/// How `r` catches up with `t`.
pub async fn plan(hub: &Hub, r: &RunnerInfo, t: &Target) -> Plan {
    let s = short(&t.commit);
    let Some(target) = r.target.as_deref() else {
        return Plan::Wait(format!("kiln has no build for {} on {}", r.os, r.arch));
    };
    if let Some(b) = binary_for(hub, target, &t.commit) {
        return Plan::Download(b);
    }
    let now = crate::util::now_unix();
    // The release build's job for its platform is under way: that binary comes soon.
    if let Some(b) = release_building(hub, &t.commit, target, false).await {
        return Plan::Wait(format!("waits for the release build of {s} ({})", b.words(now)));
    }
    // Otherwise a machine builds it now (one already building it for its platform serves it too),
    // rather than wait on a build of every platform at once on another machine, which can take
    // hours.
    match build_plan(hub, r, target, &t.commit) {
        Ok(p) => p.into(),
        Err(why) => match release_building(hub, &t.commit, target, true).await {
            Some(b) => Plan::Wait(format!("waits for the release build of {s} ({})", b.words(now))),
            None => Plan::Wait(format!("waits for a build of {s} for {target}: {why}")),
        },
    }
}

/// The ways a person can have `r` upgraded to `t` now, each with what it does or why it can't.
pub async fn ways(hub: &Hub, r: &RunnerInfo, t: &Target) -> Vec<crate::updates::WayOpen> {
    use crate::updates::{Way, WayOpen};
    let s = short(&t.commit);
    let Some(target) = r.target.as_deref() else { return Vec::new() };
    let way = |way, ok, why: String, on: Option<String>| WayOpen { way, ok, why, on };
    let download = match binary_for(hub, target, &t.commit) {
        Some(_) if r.method != "native" && !r.terminal => way(Way::Download, false, "its runner is too old to upgrade itself, and terminals are off on it".to_owned(), None),
        Some(b) => way(Way::Download, true, format!("downloads build {} of {s}", b.build), None),
        None => way(
            Way::Download,
            false,
            match release_building(hub, &t.commit, target, false).await {
                Some(b) => format!("no binary of {s} for {target} yet: release run #{} is building one", b.number),
                None => format!("no binary of {s} for {target} yet"),
            },
            None,
        ),
    };
    let build = match build_plan(hub, r, target, &t.commit) {
        Ok(BuildPlan::Join { on, .. }) => way(Way::Build, true, format!("{on} is building {s} for {target} now: it downloads that build"), Some(on)),
        Ok(BuildPlan::Build(b)) if b.itself => way(Way::Build, true, format!("builds {s} on the machine, from the server's source"), None),
        Ok(BuildPlan::Build(b)) => {
            let how = if b.cross { "cross-builds" } else { "builds" };
            way(Way::Build, true, format!("{} {how} {s} for {target} from the server's source, and it downloads that build", b.machine), Some(b.machine))
        }
        Err(why) => way(Way::Build, false, why, None),
    };
    vec![download, build]
}

/// How `r` catches up with `t` the way a person chose.
pub fn plan_by(hub: &Hub, r: &RunnerInfo, t: &Target, way: crate::updates::Way) -> anyhow::Result<Plan> {
    let target = r.target.as_deref().ok_or_else(|| anyhow::anyhow!("kiln has no build for {} on {}", r.os, r.arch))?;
    Ok(match way {
        crate::updates::Way::Download => {
            Plan::Download(binary_for(hub, target, &t.commit).ok_or_else(|| anyhow::anyhow!("no binary of {} for {target} yet", short(&t.commit)))?)
        }
        crate::updates::Way::Build => build_plan(hub, r, target, &t.commit).map_err(|why| anyhow::anyhow!("kiln can't be built for {} now: {why}", r.name))?.into(),
    })
}

/// What `r` does next to reach `t`: how ("download", "build", "wait", "upgrading" while it does,
/// or "left" when it is left alone), and that said as the end of a sentence about the machine.
/// `None` when it runs `t`.
pub async fn next(hub: &Hub, r: &RunnerInfo, t: &Target, standing: Standing) -> Option<(&'static str, String)> {
    let s = short(&t.commit);
    match standing {
        Standing::Current => return None,
        Standing::Branch => return Some(("left", format!("runs {}, off {}: left alone", short(r.commit.as_deref().unwrap_or_default()), hub.cfg.updates.release_branch))),
        Standing::Local => return Some(("left", "runs a local build: left alone".to_owned())),
        Standing::Behind | Standing::Unknown => {}
    }
    if let Some(j) = hub.updates.running_for("runner", &r.name) {
        return Some((
            "upgrading",
            match (j.method, j.on) {
                ("build", Some(on)) => format!("is upgrading to {s}, built by {on}"),
                ("build", None) => format!("is building {s} itself"),
                _ => format!("is upgrading to {s}"),
            },
        ));
    }
    let auto = r.policy == crate::updates::Policy::Auto;
    if r.locked {
        return Some(("off", format!("{s} is available; its machine refuses upgrades (its kiln config says [updates] accept = false)")));
    }
    if r.policy == crate::updates::Policy::Off {
        return Some(("off", format!("{s} is available; its updates are off, so it stays as it is")));
    }
    if auto && let Err(why) = hub.updates.fleet.may_try(&r.name, &t.commit) {
        return Some(("wait", why));
    }
    // Not upgraded by itself: what its Upgrade does.
    let when = if auto { "" } else { " when you upgrade it" };
    Some(match plan(hub, r, t).await {
        Plan::Download(b) => ("download", format!("downloads build {} ({s}){when}", b.build)),
        Plan::Build(b) if b.itself => ("build", format!("builds {s} itself{when}")),
        Plan::Build(b) => ("build", format!("gets {s} {} by {}{when}", if b.cross { "cross-built" } else { "built" }, b.machine)),
        Plan::Join { on, .. } if auto => ("wait", format!("waits for the build of {s} {on} is making, then downloads it")),
        Plan::Join { on, .. } => ("build", format!("gets the build of {s} {on} is making{when}")),
        Plan::Wait(why) => ("wait", why),
    })
}

// ------------------------------------------------------------------ following

/// Keep every runner on the newest kiln: decide it, and bring each runner behind to it.
pub async fn follow_loop(hub: Arc<Hub>) -> anyhow::Result<()> {
    use futures::FutureExt as _;
    // This server's build is its binary's hash, read whole the first time it is asked
    // (`updates::own_binary`): read on the blocking pool before this loop asks it on a runtime
    // thread (`updates::binaries`, `updates::runners`), where a debug build's gigabyte held the
    // shard, and the HTTP listener on it, back 30 to 60 s.
    let build = crate::rt::blocking(crate::updates::own_build).await?;
    // What this server runs is remembered, so runners on it are known after it is replaced.
    if is_commit(crate::api::COMMIT) {
        remember(&hub, &build, crate::api::TARGET, crate::api::COMMIT);
    }
    let mut tick = crate::rt::Interval::new(EVERY);
    loop {
        futures::select_biased! {
            () = hub.shutdown.cancelled().fuse() => return Ok(()),
            () = hub.updates.fleet.wake.notified().fuse() => {}
            () = tick.tick().fuse() => {}
        }
        // Binaries kept here are remembered too, so they're known after they're replaced.
        for b in crate::updates::binaries(&hub) {
            if let Some(c) = &b.sha {
                remember(&hub, &b.build, &b.target, c);
            }
        }
        let target = refresh(&hub).await;
        crate::updates::measure(&hub).await;
        let Some(t) = target else { continue };
        tell(&hub, &t).await;
        if hub.cfg.updates.follow {
            follow(&hub, &t).await;
        }
    }
}

/// Tell each runner behind the newest kiln that it is available, and where its build comes from,
/// once a commit, whatever its policy (only runners that can hear it).
async fn tell(hub: &Arc<Hub>, t: &Target) {
    for r in crate::updates::runners(hub).await {
        if !r.connected || !matches!(r.standing, Some(Standing::Behind | Standing::Unknown)) {
            continue;
        }
        let key = (r.name.clone(), t.commit.clone());
        if hub.updates.fleet.told.lock().contains(&key) {
            continue;
        }
        let Some(peer) = hub.instances.lock().newest_with(&r.name, crate::proto::CAP_UPDATES) else { continue };
        let from = r.from.clone().unwrap_or_else(|| format!("no build of it for {} yet", r.target.as_deref().unwrap_or("its platform")));
        let msg = crate::proto::ServerMsg::UpdateAvailable { commit: t.commit.clone(), build: r.target_build.clone().unwrap_or_default(), from };
        hub.command_peers(&r.name, vec![peer], &msg).await.log_when_answered(crate::hub::LOG_WITHIN);
        hub.updates.fleet.told.lock().insert(key);
    }
}

async fn follow(hub: &Arc<Hub>, t: &Target) {
    for r in crate::updates::runners(hub).await {
        // Only runners whose policy is Automatic: the rest wait for their Upgrade, or are left alone.
        if r.policy != crate::updates::Policy::Auto || r.locked || !r.connected || !matches!(r.standing, Some(Standing::Behind | Standing::Unknown)) || hub.updates.running_for("runner", &r.name).is_some() {
            continue;
        }
        if hub.updates.fleet.may_try(&r.name, &t.commit).is_err() {
            continue;
        }
        let started = match plan(hub, &r, t).await {
            Plan::Download(b) => Some(crate::updates::upgrade_with(hub, &r, &b, "kiln").await),
            Plan::Build(b) => Some(build(hub, &r, &t.commit, &b, "kiln").await),
            Plan::Join { .. } | Plan::Wait(_) => None,
        };
        match started {
            Some(Ok(job)) => {
                hub.updates.fleet.tried(&r.name, &t.commit);
                tracing::info!(machine = %r.name, commit = short(&t.commit), method = job.method, "bringing a runner to the newest kiln");
            }
            Some(Err(e)) => {
                hub.updates.fleet.tried(&r.name, &t.commit);
                hub.updates.fleet.ended(&r.name, &t.commit, Some(format!("{e:#}")));
                tracing::warn!(machine = %r.name, commit = short(&t.commit), error = %format!("{e:#}"), "couldn't bring a runner to the newest kiln");
            }
            None => {}
        }
    }
}

/// An upgrade job ended (`error` when it failed): what it means for the next try.
pub fn on_job_end(hub: &Hub, job: &Job, error: Option<&str>) {
    let building = hub.updates.fleet.building.lock().remove(&job.id);
    if let Some(c) = building.as_ref().map(|b| b.commit.clone()).or_else(|| job.commit.clone()) {
        hub.updates.fleet.ended(&job.target, &c, error.map(str::to_owned));
    }
    hub.updates.fleet.wake();
}

// ------------------------------------------------------------------ a machine builds kiln

/// The token a build job's machine fetches the source and hands back its binary with.
pub fn token(hub: &Hub, job: &str) -> String {
    crate::api::job_token(&hub.admin_token, &format!("upgrade:{job}"))
}

/// The commit build job `job` builds, while it runs.
pub fn building_commit(hub: &Hub, job: &str) -> Option<String> {
    hub.updates.fleet.building.lock().get(job).map(|b| b.commit.clone())
}

/// The source of the commit build job `job` builds: the server's archive of it (tar).
pub async fn source(hub: &Hub, job: &str) -> anyhow::Result<async_channel::Receiver<bytes::Bytes>> {
    let commit = building_commit(hub, job).ok_or_else(|| anyhow::anyhow!("no such build"))?;
    let repo = release_repo(hub).await.ok_or_else(|| anyhow::anyhow!("kiln doesn't build {}", hub.cfg.updates.release_repo))?;
    crate::gitops::archive(hub, &repo, &commit).await
}

/// The machine of build job `job` built a binary with this SHA-256; `bytes` is the binary when
/// it is kept for its platform.
pub async fn on_built(hub: &Arc<Hub>, job: &str, sha256: &str, bytes: Option<Vec<u8>>) -> anyhow::Result<()> {
    use sha2::Digest as _;
    let b = hub.updates.fleet.building.lock().get(job).cloned().ok_or_else(|| anyhow::anyhow!("no such build"))?;
    anyhow::ensure!(sha256.len() == 64 && sha256.bytes().all(|c| c.is_ascii_hexdigit()), "not a SHA-256: {sha256}");
    let sha256 = sha256.to_ascii_lowercase();
    let build = crate::updates::build_id(&sha256);
    remember(hub, &build, &b.target, &b.commit);
    hub.updates.set_to_build(hub, job, &build);
    let Some(bytes) = bytes.filter(|v| !v.is_empty()) else { return Ok(()) };
    anyhow::ensure!(b.keep, "a build of {} isn't kept here", b.target);
    let got = hex::encode(sha2::Sha256::digest(&bytes));
    anyhow::ensure!(got == sha256, "the binary's SHA-256 is {got}, not {sha256}");
    let kept = crate::updates::keep_built(hub, &b.target, bytes, &b.machine, &b.commit).await?;
    hub.store.audit("kiln".into(), "upgrade.binary".into(), format!("built by {} ({}): {} for {}", b.machine, short(&b.commit), kept.build, b.target)).await;
    hub.emit(&serde_json::json!({ "type": "updates" }));
    hub.updates.fleet.wake();
    Ok(())
}

/// What a build script builds, and what becomes of its binary ([`build_script`]).
pub struct Script<'a> {
    /// The upgrade job it is.
    pub job: &'a str,
    /// Where the machine can reach this server ([`crate::updates::server_bases`]).
    pub bases: &'a [String],
    /// What it fetches the source and hands the binary back with ([`token`]).
    pub token: &'a str,
    pub commit: &'a str,
    /// The platform it builds for.
    pub target: &'a str,
    /// Cross-built on Linux (`ci/release/build.sh`), rather than natively.
    pub cross: bool,
    /// Its binary goes back to the server, kept for the machines of its platform.
    pub keep: bool,
    /// The machine installs what it built, for its runner process (`crate::updates::instance8`:
    /// empty for the machine's usual one); else it builds for another machine, and handing the
    /// binary back is the point.
    pub install: Option<&'a str>,
}

/// The script a machine builds kiln with, through a terminal kiln opens on it (so a runner from
/// before this builds too). No checkout of kiln is needed: the source comes from the server, and
/// the build goes to `~/.cache/kiln/self-build`. It needs cargo, npm, tar and curl (a Mac building
/// natively, Apple's developer tools too; a Linux machine cross-building, rustup and python3), and
/// says which it lacks ([`CANT_BUILD_MARK`]). Free of single quotes and backslashes, as
/// [`crate::updates::terminal_script`].
pub fn build_script(sc: &Script<'_>) -> String {
    let ok = crate::updates::OK_MARK;
    let fail = crate::updates::FAIL_MARK;
    let hidden = crate::updates::HIDDEN_MARK;
    let (job, token, commit, target) = (sc.job, sc.token, sc.commit, sc.target);
    let s = short(commit);
    let bases = sc.bases.join(" ");
    let (tools, what) = if sc.cross { ("cargo npm tar curl rustup python3", "ci/release/build.sh") } else { ("cargo npm tar curl", "cargo") };
    let mac_tools = if sc.cross {
        ""
    } else {
        r#"if [ "$(uname -s)" = Darwin ] && ! xcode-select -p >/dev/null 2>&1; then
  cant "this Mac has no developer tools (xcode-select --install installs them), so it cannot build kiln"
fi
"#
    };
    let svc = match sc.install {
        Some(inst8) => {
            let strip = if inst8.is_empty() { String::new() } else { format!("case \"$svc\" in *-{inst8}) svc=${{svc%-{inst8}}} ;; esac\n") };
            format!(
                r#"svc=kiln-agent
if [ -r /proc/self/cgroup ]; then
  cg=$(grep -m1 -E "^0::|name=systemd" /proc/self/cgroup | cut -d: -f3)
  case "$cg" in
    */kiln*.service) svc=$(basename "$cg" .service) ;;
  esac
fi
{strip}"#
            )
        }
        None => String::new(),
    };
    let (build, bin) = if sc.cross {
        (
            format!(
                r#"if [ "$cargo" = "cargo +stable" ]; then export RUSTUP_TOOLCHAIN=stable; fi
echo "cross-building kiln {s} for {target} with $(cargo --version 2>/dev/null): ci/release/build.sh, zig against the macOS stubs"
{{ KILN_COMMIT={commit} CARGO_TARGET_DIR="$dir/target" KILN_DIST="$dir/dist-{job}" nice -n 10 ci/release/build.sh {target}; echo "$?" >"$dir/rc-{job}"; }} 2>&1 | tee "$log""#
            ),
            format!("$dir/dist-{job}/kiln-{target}"),
        )
    } else {
        (
            format!(
                r#"echo "building kiln {s} with $($cargo --version 2>/dev/null)"
{{ MACOSX_DEPLOYMENT_TARGET=11.0 KILN_COMMIT={commit} CARGO_TARGET_DIR="$dir/target" nice -n 10 $cargo build --release --locked -p kiln; echo "$?" >"$dir/rc-{job}"; }} 2>&1 | tee "$log""#
            ),
            "$dir/target/release/kiln".to_owned(),
        )
    };
    let post = format!(r#"curl -fsS -X POST -H "Authorization: Bearer {token}" -H "Content-Type: application/octet-stream" --data-binary @"$bin" "$base/api/updates/jobs/{job}/binary?sha256=$sum" >/dev/null"#);
    let hand_back = match (sc.keep, sc.install) {
        (true, Some(_)) => format!(
            r#"if {post}; then
  echo "handed the binary to the server, for the other machines of {target}"
else
  echo "could not hand the binary to the server: the other machines of {target} need a build of their own"
fi"#
        ),
        (true, None) => format!(
            r#"{post} || fail "could not hand the binary to the server"
echo "handed the binary to the server: the machines of {target} download it from there""#
        ),
        (false, _) => format!(r#"curl -fsS -X POST -H "Authorization: Bearer {token}" "$base/api/updates/jobs/{job}/binary?sha256=$sum" >/dev/null || true"#),
    };
    let end = match sc.install {
        Some(_) => format!(
            r#"echo "kiln install --agent --upgrade --service $svc"
"$bin" install --agent --upgrade --service "$svc"
rc=$?
if [ "$rc" = 0 ]; then echo "{ok}"; else echo "{fail} install --agent --upgrade exited with $rc"; fi
exit "$rc""#
        ),
        None => format!(
            r#"rm -rf "$dir/dist-{job}"
echo "{ok}""#
        ),
    };
    format!(
        r#"{hidden}{job}
set -u
fail() {{ echo "{fail} $*"; exit 1; }}
cant() {{ echo "{CANT_BUILD_MARK} $*"; exit 10; }}
le=$("${{SHELL:-/bin/sh}}" -lic env </dev/null 2>/dev/null)
lp=$(echo "$le" | sed -n "s/^PATH=//p" | tail -n 1)
if [ -n "$lp" ]; then PATH="$lp:$PATH"; fi
for v in RUSTUP_HOME CARGO_HOME; do
  val=$(echo "$le" | sed -n "s/^$v=//p" | tail -n 1)
  if [ -n "$val" ]; then export "$v=$val"; fi
done
PATH="$PATH:$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin"
export PATH
for t in {tools}; do
  command -v "$t" >/dev/null 2>&1 || cant "this machine has no $t, so it cannot build kiln"
done
{mac_tools}{svc}dir="$HOME/.cache/kiln/self-build"
mkdir -p "$dir" || fail "cannot make $dir"
base=""
for b in {bases}; do
  echo "fetching the source of {s} from $b"
  if curl -fsS -H "Authorization: Bearer {token}" -o "$dir/source-{job}.tar" "$b/api/updates/jobs/{job}/source"; then base="$b"; break; fi
done
[ -n "$base" ] || fail "could not fetch the source of {s} from the server"
find "$dir" -maxdepth 1 -name "src*" -mmin +120 -exec rm -rf {{}} + 2>/dev/null
find "$dir" -maxdepth 1 -name "dist-*" -mmin +120 -exec rm -rf {{}} + 2>/dev/null
find "$dir" -maxdepth 1 -name "build-*.log" -mtime +7 -exec rm -f {{}} + 2>/dev/null
src="$dir/src-{job}"
rm -rf "$src" && mkdir -p "$src" && tar -xf "$dir/source-{job}.tar" -C "$src" || fail "could not unpack the source"
rm -f "$dir/source-{job}.tar"
cd "$src" || fail "no $src"
echo "building the UI (node $(node --version 2>/dev/null), npm $(npm --version 2>/dev/null))"
npm ci --prefix ui --no-audit --no-fund --loglevel=error || fail "npm ci failed"
if [ ! -x ui/node_modules/.bin/tsc ]; then
  echo "npm ci left no tsc in ui/node_modules/.bin: installing again"
  npm ci --prefix ui --no-audit --no-fund --loglevel=error || fail "npm ci failed"
  [ -x ui/node_modules/.bin/tsc ] || fail "npm ci did not install the UI tools (no tsc in ui/node_modules/.bin)"
fi
npm run --prefix ui build || fail "the UI did not build"
cargo=cargo
if command -v rustup >/dev/null 2>&1; then
  echo "bringing the stable Rust toolchain up to date: kiln needs a recent one"
  if rustup toolchain install stable --profile minimal --no-self-update >"$dir/rustup.log" 2>&1 || rustup update stable --no-self-update >>"$dir/rustup.log" 2>&1; then
    cargo="cargo +stable"
  else
    echo "rustup could not update stable:"
    tail -n 6 "$dir/rustup.log"
    echo "building with the Rust there is"
  fi
fi
log="$dir/build-{job}.log"
{build}
rc=$(cat "$dir/rc-{job}" 2>/dev/null || echo 1)
rm -f "$dir/rc-{job}"
if [ "$rc" != 0 ]; then
  echo "{what} exited with $rc. What it said that went wrong:"
  grep -E "^error|could not compile|Killed|signal: |No space left" "$log" | tail -n 40
  grep -q -E "^error" "$log" || echo "{what} stopped without an error message: the machine may have run out of memory or slept, or the build was stopped"
  fail "kiln did not build (its full output is $log on this machine)"
fi
bin="{bin}"
if command -v sha256sum >/dev/null 2>&1; then sum=$(sha256sum "$bin" | cut -d" " -f1); else sum=$(shasum -a 256 "$bin" | cut -d" " -f1); fi
echo "{BUILT_MARK} $sum"
{hand_back}
{end}
"#
    )
}

/// Start upgrading `r` to `commit` by a build of it (`by`: who asked, for the audit log): `b`
/// makes it from the server's source, on `r` itself, which installs it; or on another machine,
/// which hands the binary back to the server for `r` to download, in the same job. Nothing either
/// machine runs is stopped.
pub async fn build(hub: &Arc<Hub>, r: &RunnerInfo, commit: &str, b: &Builder, by: &str) -> anyhow::Result<Job> {
    let target = r.target.clone().ok_or_else(|| anyhow::anyhow!("kiln has no build for {} on {}", r.os, r.arch))?;
    let bases = crate::updates::server_bases(hub);
    anyhow::ensure!(!bases.is_empty(), "kiln has no public URL (server.public_url) the machine could fetch the source from");
    let s = short(commit);
    let keep = shared(&target);
    let mut job = crate::updates::new_job("runner", &r.name, "build", r.build.clone(), None);
    job.commit = Some(commit.to_owned());
    job.on = (!b.itself).then(|| b.machine.clone());
    let id = job.id.clone();
    hub.updates.insert(job.clone());
    hub.updates.fleet.building.lock().insert(id.clone(), Building { machine: b.machine.clone(), target: target.clone(), commit: commit.to_owned(), keep });
    if b.itself {
        hub.store.audit(by.into(), "upgrade.runner".into(), format!("{} builds {s} itself ({target})", r.name)).await;
        hub.updates.say(hub, &id, format!("{} builds kiln {s} itself: no binary of it for {target} yet; nothing it runs is stopped", r.name));
    } else {
        let how = if b.cross { "cross-builds" } else { "builds" };
        hub.store.audit(by.into(), "upgrade.runner".into(), format!("{} {how} {s} for {} ({target})", b.machine, r.name)).await;
        hub.updates.say(
            hub,
            &id,
            format!("{} {how} kiln {s} for {target}: no binary of it yet, and {} can't build it itself ({}). It downloads the build once it's back here; nothing either machine runs is stopped", b.machine, r.name, b.why),
        );
    }
    let inst8 = crate::updates::instance8(r);
    let script = build_script(&Script { job: &id, bases: &bases, token: &token(hub, &id), commit, target: &target, cross: b.cross, keep, install: b.itself.then_some(inst8.as_str()) });
    let (h, machine, runner) = (Arc::clone(hub), b.machine.clone(), (!b.itself).then(|| r.name.clone()));
    crate::rt::spawn(async move { run_build(&h, &id, &machine, &script, runner.as_deref()).await });
    Ok(job)
}

/// Start upgrading `r` to `commit` with the build of it for its platform that machine `on` is
/// making already (upgrade job `building`): `r` downloads that build once it's kept here, in this
/// job (`by`: who asked, for the audit log).
pub async fn join(hub: &Arc<Hub>, r: &RunnerInfo, commit: &str, building: &str, on: &str, by: &str) -> anyhow::Result<Job> {
    let target = r.target.clone().ok_or_else(|| anyhow::anyhow!("kiln has no build for {} on {}", r.os, r.arch))?;
    let s = short(commit);
    let mut job = crate::updates::new_job("runner", &r.name, "build", r.build.clone(), None);
    job.commit = Some(commit.to_owned());
    job.on = Some(on.to_owned());
    let id = job.id.clone();
    hub.updates.insert(job.clone());
    hub.store.audit(by.into(), "upgrade.runner".into(), format!("{} gets the build of {s} {on} is making ({target})", r.name)).await;
    hub.updates.say(hub, &id, format!("{on} is building kiln {s} for {target} already: {} downloads that build once it's back here; nothing it runs is stopped", r.name));
    let (h, machine, commit, building) = (Arc::clone(hub), r.name.clone(), commit.to_owned(), building.to_owned());
    crate::rt::spawn(async move { await_build(&h, &id, &machine, &target, &commit, &building).await });
    Ok(job)
}

/// Job `id` waits for the binary of `commit` for `target` that upgrade job `building` makes, then
/// upgrades `machine` to it.
async fn await_build(hub: &Arc<Hub>, id: &str, machine: &str, target: &str, commit: &str, building: &str) {
    let u = &hub.updates;
    let started = Instant::now();
    loop {
        if !u.running(id) {
            return;
        }
        if let Some(b) = binary_for(hub, target, commit) {
            crate::updates::upgrade_into(hub, id, machine, &b).await;
            return;
        }
        match u.job(building) {
            Some(j) if j.state == crate::updates::JobState::Running => {}
            j => {
                let why = j.and_then(|j| j.error).unwrap_or_else(|| "its machine didn't hand a binary back".to_owned());
                u.finish(hub, id, Err(format!("the build it waited for ended without a binary of {}: {why}", short(commit))));
                return;
            }
        }
        let limit = crate::updates::waits(hub).await.build();
        if started.elapsed() > limit {
            u.time_out(hub, id, "build", format!("the build it waited for didn't finish in {}", crate::updates::said(limit)));
            return;
        }
        crate::rt::sleep(JOIN_POLL).await;
    }
}

/// Runs a build script in a terminal on `machine` and follows it: a build the machine installs,
/// or one for runner `then`, which downloads it once it's kept here.
async fn run_build(hub: &Arc<Hub>, id: &str, machine: &str, script: &str, then: Option<&str>) {
    use crate::termsess::Frame;
    let u = &hub.updates;
    let session = crate::termsess::open_agent(hub, machine.to_owned(), Some(format!("sh -c '{script}'")), 200, 50);
    let mut att = session.attach();
    let mut screen = crate::updates::Screen::default();
    let (mut ok_marked, mut cant) = (false, None::<String>);
    let mut lost = false;
    let mut feed = |text: &str| {
        for shown in screen.feed(text) {
            let line = match shown {
                crate::updates::Shown::Line(line) => line,
                // cargo's progress bar, as it is now (it draws over itself, never ending its line).
                crate::updates::Shown::Progress(p) => {
                    if !crate::updates::marked(&p) && !p.starts_with(BUILT_MARK) && !p.starts_with(CANT_BUILD_MARK) {
                        u.progress(hub, id, p.trim());
                    }
                    continue;
                }
            };
            let line = line.trim_end();
            if line.is_empty() {
                continue;
            }
            if line.starts_with(crate::updates::HIDDEN_MARK) {
                u.end_progress(hub, id);
                continue;
            }
            if line.contains(crate::updates::OK_MARK) {
                ok_marked = true;
                u.end_progress(hub, id);
                continue;
            }
            if let Some(sum) = line.strip_prefix(BUILT_MARK) {
                u.set_to_build(hub, id, &crate::updates::build_id(sum.trim()));
                u.end_progress(hub, id);
                continue;
            }
            if let Some(why) = line.strip_prefix(CANT_BUILD_MARK) {
                cant = Some(why.trim().to_owned());
            }
            let line = line.strip_prefix(crate::updates::FAIL_MARK).or_else(|| line.strip_prefix(CANT_BUILD_MARK)).map_or(line, str::trim);
            u.say(hub, id, line);
        }
    };
    feed(&String::from_utf8_lossy(&att.replay));
    let started = Instant::now();
    let mut timed_out = false;
    let result = loop {
        // Looked at again at least every minute: a longer wait chosen meanwhile applies.
        let limit = crate::updates::waits(hub).await.build();
        let left = limit.saturating_sub(started.elapsed());
        if left.is_zero() {
            timed_out = true;
            break Err(format!("the build didn't finish in {}", crate::updates::said(limit)));
        }
        let Ok(frame) = crate::rt::timeout(left.min(Duration::from_secs(60)), att.live.recv()).await else {
            continue;
        };
        match frame {
            Ok(Frame::Data(d)) => feed(&String::from_utf8_lossy(&d)),
            Ok(Frame::Exit { code, error }) => {
                feed("\n");
                break match (code, error) {
                    (Some(0), _) => Ok(()),
                    (_, Some(e)) if e == "__reconnect__" => Err(format!("{machine} disconnected during the build")),
                    (_, Some(e)) => Err(e),
                    (c, None) => Err(format!("the build exited with {}", c.map_or_else(|| "a signal".to_owned(), |c| c.to_string()))),
                };
            }
            Ok(Frame::Size { .. }) => {}
            // Faster than kept here: the build's own log on the machine has all of it.
            Err(async_broadcast::RecvError::Overflowed(_)) => {
                if !std::mem::replace(&mut lost, true) {
                    u.say(hub, id, "(some of the build's output came faster than kiln kept it: the build log on the machine has all of it)");
                }
            }
            Err(async_broadcast::RecvError::Closed) => break Err("the build's terminal closed".to_owned()),
        }
    };
    session.detach();
    if let Some(why) = &cant {
        hub.updates.fleet.cant_build.lock().insert(machine.to_owned(), (Instant::now(), why.clone()));
    }
    if let Err(e) = result
        && !ok_marked
    {
        if timed_out {
            session.close();
            u.time_out(hub, id, "build", e);
        } else {
            u.finish(hub, id, Err(cant.unwrap_or(e)));
        }
        return;
    }
    if let Some(runner) = then {
        let built = hub.updates.fleet.building.lock().remove(id);
        match built.and_then(|b| binary_for(hub, &b.target, &b.commit)) {
            Some(bin) => crate::updates::upgrade_into(hub, id, runner, &bin).await,
            None => u.finish(hub, id, Err(format!("{machine} built it, but its binary didn't reach this server"))),
        }
        return;
    }
    // Installed: the new runner reports its build when it connects, which completes the job.
    let until = Instant::now() + REPORT_IN;
    while Instant::now() < until {
        if !u.running(id) {
            return;
        }
        u.reached_commit(hub, id, machine);
        if u.reached(id) {
            u.finish(hub, id, Ok(String::new()));
            return;
        }
        crate::rt::sleep(Duration::from_secs(1)).await;
    }
    u.finish(hub, id, Ok("installed; the runner takes it once it is idle (it shows under Runners when it does)".to_owned()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_release_run_says_how_far_it_is() {
        let r = ReleaseRun { run: "01a1".into(), number: 232, step: Some("build".into()), since: Some(1_000), on: Some("adacachy".into()) };
        assert_eq!(r.words(1_000 + 11 * 60 + 5), "run #232: its build step, on adacachy, 11 min in");
        let bare = ReleaseRun { run: "01a1".into(), number: 7, step: None, since: None, on: None };
        assert_eq!(bare.words(0), "run #7");
    }

    #[test]
    fn short_and_commits() {
        assert_eq!(short("0123456789abcdef0123"), "0123456789ab");
        assert_eq!(short("abc"), "abc");
        assert!(is_commit("e8c17e544623c019c78fce08ee0931a79d5d09db"));
        assert!(!is_commit(""));
        assert!(!is_commit("main"));
        assert!(!is_commit("e8c17e5 "));
    }

    fn script<'a>(target: &'a str, cross: bool, keep: bool, install: Option<&'a str>) -> String {
        let bases = ["https://kiln.example".to_owned()];
        build_script(&Script { job: "job1", bases: &bases, token: "tok", commit: "e8c17e544623c019c78fce08ee0931a79d5d09db", target, cross, keep, install })
    }

    #[test]
    fn the_build_script_quotes_nothing_it_shouldnt() {
        let mac = script("aarch64-apple-darwin", false, true, Some(""));
        let linux = script("x86_64-unknown-linux-gnu", false, false, Some("01a14794"));
        let for_another = script("aarch64-apple-darwin", false, true, None);
        let cross = script("aarch64-apple-darwin", true, true, None);
        for s in [&mac, &linux, &for_another, &cross] {
            // It goes inside `sh -c '…'`: no single quotes, no backslashes.
            assert!(!s.contains('\'') && !s.contains('\\'), "{s}");
            assert!(s.starts_with(crate::updates::HIDDEN_MARK));
            assert!(s.contains("/api/updates/jobs/job1/source"));
            assert!(s.contains("KILN_COMMIT=e8c17e544623c019c78fce08ee0931a79d5d09db"));
            // A recent Rust: the machine's may be older than kiln's dependencies need.
            assert!(s.contains("rustup toolchain install stable"));
            assert!(!s.contains("checkout"), "no checkout of kiln is looked for: {s}");
        }
        assert!(mac.contains("--data-binary @\"$bin\"") && mac.contains("$cargo build") && mac.contains("install --agent --upgrade"));
        assert!(mac.contains("xcode-select -p"), "a Mac building natively needs Apple's developer tools");
        assert!(!linux.contains("--data-binary"), "a Linux build isn't handed back");
        assert!(linux.contains("*-01a14794) svc=${svc%-01a14794}"));
        assert!(for_another.contains("--data-binary @\"$bin\"") && for_another.contains("|| fail \"could not hand the binary to the server\""));
        assert!(!for_another.contains("install --agent") && !for_another.contains("svc="));
        assert!(cross.contains("ci/release/build.sh aarch64-apple-darwin") && cross.contains("KILN_DIST=\"$dir/dist-job1\""));
        assert!(cross.contains("bin=\"$dir/dist-job1/kiln-aarch64-apple-darwin\"") && cross.contains("export RUSTUP_TOOLCHAIN=stable"));
        assert!(cross.contains("for t in cargo npm tar curl rustup python3") && !cross.contains("xcode-select"));
    }

    /// The script under sh: with no checkout of kiln anywhere it goes on to the tools, then to
    /// fetching the source from the server (refused here), never looking for one.
    #[cfg(unix)]
    #[test]
    fn the_build_script_needs_no_checkout() -> anyhow::Result<()> {
        use std::os::unix::fs::PermissionsExt as _;
        let (home, tools) = (tempfile::tempdir()?, tempfile::tempdir()?);
        for (name, body) in [("cargo", "exit 0"), ("npm", "exit 0"), ("curl", "exit 22")] {
            let p = tools.path().join(name);
            std::fs::write(&p, format!("#!/bin/sh\n{body}\n"))?;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755))?;
        }
        let s = script("aarch64-apple-darwin", false, true, None);
        let path = format!("{}:/usr/bin:/bin", tools.path().display());
        let out = std::process::Command::new("/bin/sh").arg("-c").arg(&s).env("HOME", home.path()).env("SHELL", "/bin/sh").env("PATH", path).output()?;
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        assert_eq!(out.status.code(), Some(1), "{text}");
        assert!(text.contains("fetching the source of e8c17e544623 from https://kiln.example"), "{text}");
        assert!(text.contains(&format!("{} could not fetch the source", crate::updates::FAIL_MARK)), "{text}");
        assert!(!text.contains(CANT_BUILD_MARK) && !text.contains("checkout"), "{text}");
        Ok(())
    }

    fn candidate(name: &str, os: &str, target: &str, tools: &[&str], room: f32) -> Candidate {
        Candidate { name: name.into(), os: os.into(), target: Some(target.into()), tools: tools.iter().map(|t| ((*t).to_owned(), "1".to_owned())).collect(), room }
    }

    /// A Mac with no developer tools gets its build from a Mac that has them, natively, before a
    /// Linux machine that cross-builds it; from Linux when that Mac is overloaded or there is none;
    /// from nowhere when nothing has the tools.
    #[test]
    fn a_mac_s_build_is_made_where_the_tools_are() {
        let mac = "aarch64-apple-darwin";
        let laptop = candidate("laptop", "macos", mac, &["cargo", "node", "git"], 10.0);
        let linux = candidate("linux", "linux", "x86_64-unknown-linux-gnu", &["cargo", "node", "python3"], 60.0);
        let studio = candidate("studio", "macos", mac, &["git", "docker"], 80.0);
        assert_eq!(pick(mac, &[studio.clone(), linux.clone(), laptop.clone()]), Some(("laptop".to_owned(), false)), "natively, on the Mac with the tools");
        let pressed = Candidate { room: 10.0 - crate::reach::directory::OVERLOADED, ..laptop.clone() };
        assert_eq!(pick(mac, &[pressed, linux.clone()]), Some(("linux".to_owned(), true)), "an overloaded Mac gives way to a Linux machine");
        assert_eq!(pick(mac, &[studio.clone(), linux.clone()]), Some(("linux".to_owned(), true)), "cross-built when no Mac can");
        let roomier = Candidate { name: "linux-2".into(), room: 90.0, ..linux.clone() };
        assert_eq!(pick(mac, &[linux.clone(), roomier]), Some(("linux-2".to_owned(), true)), "the Linux machine with the most room");
        let no_python = candidate("linux", "linux", "x86_64-unknown-linux-gnu", &["cargo", "node"], 60.0);
        assert_eq!(pick(mac, &[studio.clone(), no_python]), None, "cross-building needs python3 for zig");
        assert_eq!(pick("x86_64-pc-windows-msvc", &[linux, studio]), None, "only a Mac's build is cross-built for it");
    }

    fn live(name: &str, os: &str, arch: &str, tools: &[&str]) -> crate::hub::AgentLive {
        crate::hub::AgentLive {
            name: name.into(),
            peer: None,
            labels: Vec::new(),
            slots: 2,
            hostname: name.into(),
            os: os.into(),
            arch: arch.into(),
            version: "0.2.0".into(),
            ephemeral: false,
            connected: true,
            last_seen: 0,
            enrolled_at: 0,
            running: Vec::new(),
            busy: 0,
            draining: false,
            sample: None,
            battery: None,
            dropped: Vec::new(),
            dropped_toward: Vec::new(),
            terminal: true,
            outpost: false,
            dedicated_job: None,
            exclusive: false,
            lost_at: None,
            facts: Some(Box::new(kiln_ci::facts::RunnerFacts {
                os: os.into(),
                arch: arch.into(),
                tools: tools.iter().map(|t| ((*t).to_owned(), "1".to_owned())).collect(),
                ..Default::default()
            })),
            instances: Vec::new(),
            transport: None,
        }
    }

    const NEWEST: &str = "e511eabdeab2118ab480c14c27ca460807e0c372";

    /// A hub with the fleet of 2026-10-24: a Mac Studio with no developer tools, a MacBook with
    /// cargo and node, and a Linux machine with what cross-building takes.
    async fn fleet() -> anyhow::Result<Arc<Hub>> {
        let hub = crate::hub::tests::scratch_hub().await?;
        for a in [live("studio", "macos", "aarch64", &["git", "docker"]), live("macbook", "macos", "aarch64", &["cargo", "node", "git"]), live("linux", "linux", "x86_64", &["cargo", "node", "python3"])] {
            hub.agents.lock().insert(a.name.clone(), a);
        }
        Ok(hub)
    }

    async fn runner(hub: &Arc<Hub>, name: &str) -> anyhow::Result<RunnerInfo> {
        crate::updates::runners(hub).await.into_iter().find(|r| r.name == name).ok_or_else(|| anyhow::anyhow!("no runner {name}"))
    }

    fn newest() -> Target {
        Target { commit: NEWEST.into(), from: "server".into(), since: 0 }
    }

    fn built_by(p: &Plan) -> Option<(String, bool, bool)> {
        match p {
            Plan::Build(b) => Some((b.machine.clone(), b.itself, b.cross)),
            _ => None,
        }
    }

    /// No checkout of kiln decides who builds: the MacBook builds itself (it has cargo and node),
    /// the Studio (no developer tools) gets the MacBook's build over the wire, and with no Mac to
    /// build it, a Linux machine cross-builds it. A Linux machine without the tools waits: its build
    /// would be its own alone.
    #[test]
    fn who_builds_is_decided_by_the_tools_not_a_checkout() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = fleet().await?;
            let t = newest();
            let studio = runner(&hub, "studio").await?;
            assert_eq!(built_by(&plan(&hub, &runner(&hub, "macbook").await?, &t).await), Some(("macbook".to_owned(), true, false)));
            match plan(&hub, &studio, &t).await {
                Plan::Build(b) => {
                    assert_eq!((b.machine.as_str(), b.itself, b.cross), ("macbook", false, false));
                    assert_eq!(b.why, "it has no cargo or node to build kiln with");
                }
                other => anyhow::bail!("the Studio's plan: {other:?}"),
            }
            let ways = ways(&hub, &studio, &t).await;
            let build = ways.iter().find(|w| w.way == crate::updates::Way::Build).ok_or_else(|| anyhow::anyhow!("no build way"))?;
            assert!(build.ok && build.on.as_deref() == Some("macbook"), "{ways:?}");
            assert_eq!(build.why, "macbook builds e511eabdeab2 for aarch64-apple-darwin from the server's source, and it downloads that build");
            assert!(ways.iter().any(|w| w.way == crate::updates::Way::Download && !w.ok));
            if let Some(m) = hub.agents.lock().get_mut("macbook") {
                m.draining = true;
            }
            assert_eq!(built_by(&plan(&hub, &studio, &t).await), Some(("linux".to_owned(), false, true)), "no Mac to build it: cross-built");
            if let Some(f) = hub.agents.lock().get_mut("linux").and_then(|l| l.facts.as_mut()) {
                f.tools.remove("python3");
            }
            match plan(&hub, &studio, &t).await {
                Plan::Wait(why) => assert_eq!(
                    why,
                    "waits for a build of e511eabdeab2 for aarch64-apple-darwin: it has no cargo or node to build kiln with, and no connected machine can build kiln for aarch64-apple-darwin"
                ),
                other => anyhow::bail!("nowhere to build: {other:?}"),
            }
            hub.agents.lock().insert("box".into(), live("box", "linux", "x86_64", &["git"]));
            assert!(matches!(plan(&hub, &runner(&hub, "box").await?, &t).await, Plan::Wait(_)), "a Linux build is its machine's own: never made elsewhere");
            Ok(())
        })
    }

    /// One build of a commit for a platform serves every machine of it: while one is under way,
    /// the others (the machine building it too) wait for it rather than build again. A machine
    /// whose build found it lacks a tool isn't asked again.
    #[test]
    fn a_build_under_way_serves_the_whole_platform() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = fleet().await?;
            let t = newest();
            let building = Building { machine: "macbook".into(), target: "aarch64-apple-darwin".into(), commit: NEWEST.into(), keep: true };
            hub.updates.fleet.building.lock().insert("job-1".into(), building);
            for name in ["studio", "macbook"] {
                match plan(&hub, &runner(&hub, name).await?, &t).await {
                    Plan::Join { job, on } => assert_eq!((job.as_str(), on.as_str()), ("job-1", "macbook")),
                    other => anyhow::bail!("{name}: {other:?}"),
                }
            }
            let ways = ways(&hub, &runner(&hub, "studio").await?, &t).await;
            assert!(ways.iter().any(|w| w.way == crate::updates::Way::Build && w.ok && w.why == "macbook is building e511eabdeab2 for aarch64-apple-darwin now: it downloads that build"), "{ways:?}");
            assert!(builds_on(&hub, "macbook") && !builds_on(&hub, "studio"));
            hub.updates.fleet.building.lock().clear();
            hub.updates.fleet.cant_build.lock().insert("macbook".into(), (Instant::now(), "this Mac has no developer tools".into()));
            let macbook = runner(&hub, "macbook").await?;
            assert_eq!(builds_itself(&hub, &macbook), Err("this Mac has no developer tools".to_owned()));
            assert_eq!(built_by(&plan(&hub, &macbook, &t).await), Some(("linux".to_owned(), false, true)));
            assert_eq!(built_by(&plan(&hub, &runner(&hub, "studio").await?, &t).await), Some(("linux".to_owned(), false, true)));
            Ok(())
        })
    }

    /// A Mac's build kept for its platform: what a runner waiting for it then downloads.
    async fn keep_mac_build(hub: &Arc<Hub>) -> anyhow::Result<Binary> {
        let mut bytes = vec![0u8; 2 << 20];
        if let Some(head) = bytes.get_mut(..4) {
            head.copy_from_slice(&[0xcf, 0xfa, 0xed, 0xfe]);
        }
        crate::updates::keep_built(hub, "aarch64-apple-darwin", bytes, "macbook", NEWEST).await
    }

    async fn ended(hub: &Arc<Hub>, id: &str) -> anyhow::Result<Job> {
        for _ in 0..200 {
            if let Some(j) = hub.updates.job(id).filter(|j| j.state != crate::updates::JobState::Running) {
                return Ok(j);
            }
            crate::rt::sleep(Duration::from_millis(50)).await;
        }
        anyhow::bail!("job {id} didn't end")
    }

    /// A runner that joins a build under way goes on, in its own job, to download what came back:
    /// here it gets as far as asking its runner (whose terminals are off, so that's where it stops).
    /// One whose build ended without a binary ends saying why.
    #[test]
    fn a_runner_joining_a_build_downloads_it_once_kept() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = fleet().await?;
            if let Some(s) = hub.agents.lock().get_mut("studio") {
                s.terminal = false;
            }
            let studio = runner(&hub, "studio").await?;
            let other = crate::updates::new_job("runner", "studio-2", "build", None, None);
            hub.updates.insert(other.clone());
            hub.updates.fleet.building.lock().insert(other.id.clone(), Building { machine: "macbook".into(), target: "aarch64-apple-darwin".into(), commit: NEWEST.into(), keep: true });
            let job = join(&hub, &studio, NEWEST, &other.id, "macbook", "test").await?;
            assert_eq!((job.on.as_deref(), job.method), (Some("macbook"), "build"));
            crate::rt::sleep(Duration::from_millis(300)).await;
            assert!(hub.updates.running(&job.id), "it waits for the build");
            let kept = keep_mac_build(&hub).await?;
            let j = ended(&hub, &job.id).await?;
            assert!(j.log.iter().any(|l| l.starts_with("macbook is building kiln e511eabdeab2 for aarch64-apple-darwin already")), "{:?}", j.log);
            assert!(j.error.as_deref().is_some_and(|e| e.contains("terminals turned off")), "it went on to download {}: {:?}", kept.build, j.error);
            let failed = crate::updates::new_job("runner", "studio-3", "build", None, None);
            hub.updates.insert(failed.clone());
            let other = Target { commit: "0123456789abcdef0123456789abcdef01234567".into(), ..newest() };
            let job = join(&hub, &studio, &other.commit, &failed.id, "macbook", "test").await?;
            hub.updates.finish(&hub, &failed.id, Err("kiln did not build".into()));
            let j = ended(&hub, &job.id).await?;
            assert_eq!(j.error.as_deref(), Some("the build it waited for ended without a binary of 0123456789ab: kiln did not build"));
            Ok(())
        })
    }
}
