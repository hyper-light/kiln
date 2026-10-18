//! Every runner on the newest kiln (`[updates] follow`).
//!
//! The newest kiln is a commit of the release branch (`[updates] release_branch`): of the commits
//! the server and the connected runners run, the one the others lead to, as the server's mirror of
//! the release repository has them. When it moves (the server was deployed, a runner was upgraded
//! or built itself) every other connected runner is told, and catches up however it can:
//!
//! 1. It downloads a binary of that commit for its platform, when kiln has one: the server's own,
//!    one the release pipeline built, or one another machine of its platform built itself.
//! 2. Else, on a machine with a checkout of kiln (the sign it builds kiln), it builds the commit
//!    itself, from the server's archive of it, in a directory of its own
//!    (`~/.cache/kiln/self-build`, a source tree per build beside one cache of compiled crates; the
//!    checkout is never touched), and installs that build. A Mac
//!    hands its binary back, so other Macs download it instead of building; a Linux build links
//!    the machine's own C library, so it is that machine's alone.
//! 3. Else it waits for a build of the commit, and says what for.
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
/// A machine found without a kiln checkout isn't asked to build again for this long.
const NO_CHECKOUT_TTL: Duration = Duration::from_secs(24 * 60 * 60);
/// How long a build on a machine may take, install included.
const BUILD_TIMEOUT: Duration = Duration::from_secs(60 * 60);
/// How long the new runner has to report in after its install finished.
const REPORT_IN: Duration = Duration::from_secs(90);
/// Builds remembered with their commit.
const KEEP_BUILDS: usize = 400;
/// Git questions to the mirror.
const GIT_TIMEOUT: Duration = Duration::from_secs(60);

/// What a build script prints for the server to read (`OK_MARK` and `FAIL_MARK` as an upgrade
/// through a terminal prints them).
const BUILT_MARK: &str = "KILN-BUILT";
const NO_CHECKOUT_MARK: &str = "KILN-NO-CHECKOUT";

/// Where a machine looks for a checkout of kiln, besides where its agents work and its synced
/// copies: the usual homes of source code.
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

/// A build a machine is doing of itself.
#[derive(Debug, Clone)]
struct Building {
    machine: String,
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
    /// Machines found without a kiln checkout, and when.
    no_checkout: Mutex<HashMap<String, Instant>>,
    /// Commits looked up on the release branch.
    on_branch: Mutex<HashMap<String, Branch>>,
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
fn newest_build(hub: &Hub, machine: &str) -> Option<String> {
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
    /// It builds the newest commit itself (where its kiln checkout may be).
    Build(Vec<String>),
    /// Nothing it can do now: why, as the end of a sentence about the machine ("waits for …").
    Wait(String),
}

/// A run of the release pipeline building `commit` right now (a job of it running): its number.
async fn release_building(hub: &Hub, commit: &str) -> Option<i64> {
    let u = &hub.cfg.updates;
    let repo = release_repo(hub).await?;
    let runs = hub.store.runs(Some(repo.id.clone()), 50, None).await.ok()?;
    for run in runs.into_iter().filter(|r| r.pipeline == u.release_pipeline && r.sha.eq_ignore_ascii_case(commit) && !r.status.is_terminal()) {
        let jobs = hub.store.jobs(run.id.clone(), false).await.unwrap_or_default();
        if jobs.iter().any(|j| j.status == crate::store::Status::Running) {
            return Some(run.number);
        }
    }
    None
}

/// Where a machine's kiln checkout may be: its synced copies of kiln, where its agents work (the
/// build script climbs to the top of the repository), then the usual places (relative to its home).
fn checkouts(hub: &Hub, machine: &str) -> Vec<String> {
    let mut v: Vec<String> = Vec::new();
    let mut add = |p: &str| {
        let p = p.trim();
        // Each goes in double quotes in the script: nothing there may end the quote or expand.
        if !p.is_empty() && !p.contains(|c: char| "\"'`$\\\n\r".contains(c)) && !v.iter().any(|x| x == p) && v.len() < 24 {
            v.push(p.to_owned());
        }
    };
    if let Some(k) = reported(hub, machine)
        && let Some(c) = &k.checkout
    {
        add(c);
    }
    let name = &hub.cfg.updates.release_repo;
    let short_name = name.rsplit('/').next().unwrap_or(name);
    for repo in hub.sync.view().repos {
        if repo.name == *name || repo.name == short_name {
            for c in repo.copies.iter().filter(|c| c.machine == machine) {
                add(&c.path);
            }
        }
    }
    let at = format!("agent:{machine}");
    for s in hub.harness.list().into_iter().filter(|s| s.target == at) {
        add(&s.cwd);
    }
    v
}

/// Whether `r` can build kiln itself; else why not.
fn can_build(hub: &Hub, r: &RunnerInfo) -> Result<Vec<String>, String> {
    if r.os != "linux" && r.os != "macos" {
        return Err("kiln builds itself only on Linux and macOS".to_owned());
    }
    if !r.terminal {
        return Err("terminals are turned off on it, and a build runs in one".to_owned());
    }
    if hub.updates.fleet.no_checkout.lock().get(&r.name).is_some_and(|at| at.elapsed() < NO_CHECKOUT_TTL) {
        return Err("no kiln checkout was found on it".to_owned());
    }
    Ok(checkouts(hub, &r.name))
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
    if let Some(n) = release_building(hub, &t.commit).await {
        return Plan::Wait(format!("waits for the release build of {s} (run #{n})"));
    }
    match can_build(hub, r) {
        Ok(c) => Plan::Build(c),
        Err(why) => Plan::Wait(format!("waits for a build of {s} for {target}: {why}")),
    }
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
        return Some(("upgrading", if j.method == "build" { format!("is building {s} itself") } else { format!("is upgrading to {s}") }));
    }
    if let Err(why) = hub.updates.fleet.may_try(&r.name, &t.commit) {
        return Some(("wait", why));
    }
    Some(match plan(hub, r, t).await {
        Plan::Download(b) => ("download", format!("downloads build {} ({s})", b.build)),
        Plan::Build(_) => ("build", format!("builds {s} itself")),
        Plan::Wait(why) => ("wait", why),
    })
}

// ------------------------------------------------------------------ following

/// Keep every runner on the newest kiln: decide it, and bring each runner behind to it.
pub async fn follow_loop(hub: Arc<Hub>) -> anyhow::Result<()> {
    use futures::FutureExt as _;
    // What this server runs is remembered, so runners on it are known after it is replaced.
    if is_commit(crate::api::COMMIT) {
        remember(&hub, &crate::updates::own_build(), crate::api::TARGET, crate::api::COMMIT);
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
        let Some(t) = refresh(&hub).await else { continue };
        if hub.cfg.updates.follow {
            follow(&hub, &t).await;
        }
    }
}

async fn follow(hub: &Arc<Hub>, t: &Target) {
    for r in crate::updates::runners(hub).await {
        if !r.connected || !matches!(r.standing, Some(Standing::Behind | Standing::Unknown)) || hub.updates.running_for("runner", &r.name).is_some() {
            continue;
        }
        if hub.updates.fleet.may_try(&r.name, &t.commit).is_err() {
            continue;
        }
        let started = match plan(hub, &r, t).await {
            Plan::Download(b) => Some(crate::updates::upgrade_with(hub, &r, &b, "kiln").await),
            Plan::Build(c) => Some(self_build(hub, &r, &t.commit, c, "kiln").await),
            Plan::Wait(_) => None,
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

// ------------------------------------------------------------------ a machine builds itself

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
    let (h, target, machine, commit) = (Arc::clone(hub), b.target.clone(), b.machine.clone(), b.commit.clone());
    let kept = crate::rt::blocking(move || crate::updates::keep_built(&h, &target, &bytes, &machine, &commit)).await??;
    hub.store.audit("kiln".into(), "upgrade.binary".into(), format!("built by {} ({}): {} for {}", b.machine, short(&b.commit), kept.build, b.target)).await;
    hub.emit(&serde_json::json!({ "type": "updates" }));
    hub.updates.fleet.wake();
    Ok(())
}

/// The script a machine builds kiln with, through a terminal kiln opens on it (so a runner from
/// before this builds too). It needs a kiln checkout (only as the sign the machine builds kiln:
/// the source comes from the server, and the build goes to `~/.cache/kiln/self-build`), cargo,
/// npm, tar and curl. Free of single quotes and backslashes, as [`crate::updates::terminal_script`].
pub fn build_script(job: &str, bases: &[String], token: &str, commit: &str, candidates: &[String], keep: bool, inst8: &str) -> String {
    let ok = crate::updates::OK_MARK;
    let fail = crate::updates::FAIL_MARK;
    let hidden = crate::updates::HIDDEN_MARK;
    let s = short(commit);
    let mut dirs: Vec<String> = candidates.iter().map(|c| format!("\"{c}\"")).collect();
    dirs.extend(USUAL.iter().map(|u| format!("\"$HOME/{u}\"")));
    let dirs = dirs.join(" ");
    let bases = bases.join(" ");
    let strip = if inst8.is_empty() { String::new() } else { format!("case \"$svc\" in *-{inst8}) svc=${{svc%-{inst8}}} ;; esac\n") };
    let hand_back = if keep {
        format!(
            r#"if curl -fsS -X POST -H "Authorization: Bearer {token}" -H "Content-Type: application/octet-stream" --data-binary @"$bin" "$base/api/updates/jobs/{job}/binary?sha256=$sum" >/dev/null; then
  echo "handed the binary to the server, for machines like this one"
else
  echo "could not hand the binary to the server: machines like this one build it themselves"
fi"#
        )
    } else {
        format!(r#"curl -fsS -X POST -H "Authorization: Bearer {token}" "$base/api/updates/jobs/{job}/binary?sha256=$sum" >/dev/null || true"#)
    };
    format!(
        r#"{hidden}{job}
set -u
fail() {{ echo "{fail} $*"; exit 1; }}
le=$("${{SHELL:-/bin/sh}}" -lic env </dev/null 2>/dev/null)
lp=$(echo "$le" | sed -n "s/^PATH=//p" | tail -n 1)
if [ -n "$lp" ]; then PATH="$lp:$PATH"; fi
for v in RUSTUP_HOME CARGO_HOME; do
  val=$(echo "$le" | sed -n "s/^$v=//p" | tail -n 1)
  if [ -n "$val" ]; then export "$v=$val"; fi
done
PATH="$PATH:$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin"
export PATH
repo=""
for d in {dirs}; do
  [ -d "$d" ] || continue
  top=$(git -C "$d" rev-parse --show-toplevel 2>/dev/null || echo "$d")
  if [ -f "$top/crates/kiln/Cargo.toml" ] && [ -f "$top/ui/package.json" ]; then repo="$top"; break; fi
done
if [ -z "$repo" ]; then echo "{NO_CHECKOUT_MARK} no kiln checkout on this machine"; exit 10; fi
echo "kiln checkout: $repo (left as it is: the build is of {s}, from the server)"
for t in cargo npm tar curl; do
  command -v "$t" >/dev/null 2>&1 || fail "this machine has no $t, so it cannot build kiln"
done
svc=kiln-agent
if [ -r /proc/self/cgroup ]; then
  cg=$(grep -m1 -E "^0::|name=systemd" /proc/self/cgroup | cut -d: -f3)
  case "$cg" in
    */kiln*.service) svc=$(basename "$cg" .service) ;;
  esac
fi
{strip}dir="$HOME/.cache/kiln/self-build"
mkdir -p "$dir" || fail "cannot make $dir"
base=""
for b in {bases}; do
  echo "fetching the source of {s} from $b"
  if curl -fsS -H "Authorization: Bearer {token}" -o "$dir/source-{job}.tar" "$b/api/updates/jobs/{job}/source"; then base="$b"; break; fi
done
[ -n "$base" ] || fail "could not fetch the source of {s} from the server"
find "$dir" -maxdepth 1 -name "src*" -mmin +120 -exec rm -rf {{}} + 2>/dev/null
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
echo "building kiln {s} with $($cargo --version 2>/dev/null)"
MACOSX_DEPLOYMENT_TARGET=11.0 KILN_COMMIT={commit} CARGO_TARGET_DIR="$dir/target" nice -n 10 $cargo build --release --locked -p kiln || fail "kiln did not build"
bin="$dir/target/release/kiln"
if command -v sha256sum >/dev/null 2>&1; then sum=$(sha256sum "$bin" | cut -d" " -f1); else sum=$(shasum -a 256 "$bin" | cut -d" " -f1); fi
echo "{BUILT_MARK} $sum"
{hand_back}
echo "kiln install --agent --upgrade --service $svc"
"$bin" install --agent --upgrade --service "$svc"
rc=$?
if [ "$rc" = 0 ]; then echo "{ok}"; else echo "{fail} install --agent --upgrade exited with $rc"; fi
exit "$rc"
"#
    )
}

/// Start `r` building `commit` itself and upgrading to it (`by`: who asked, for the audit log).
pub async fn self_build(hub: &Arc<Hub>, r: &RunnerInfo, commit: &str, candidates: Vec<String>, by: &str) -> anyhow::Result<Job> {
    let target = r.target.clone().ok_or_else(|| anyhow::anyhow!("kiln has no build for {} on {}", r.os, r.arch))?;
    let bases = crate::updates::server_bases(hub);
    anyhow::ensure!(!bases.is_empty(), "kiln has no public URL (server.public_url) the machine could fetch the source from");
    // A Mac builds exactly the release target of its platform; a Linux build links glibc, not musl.
    let keep = r.os == "macos" && target.ends_with("-apple-darwin");
    let mut job = crate::updates::new_job("runner", &r.name, "build", r.build.clone(), None);
    job.commit = Some(commit.to_owned());
    let id = job.id.clone();
    hub.updates.fleet.building.lock().insert(id.clone(), Building { machine: r.name.clone(), target: target.clone(), commit: commit.to_owned(), keep });
    hub.updates.insert(job.clone());
    hub.store.audit(by.into(), "upgrade.runner".into(), format!("{} builds {} itself ({target})", r.name, short(commit))).await;
    hub.updates.say(hub, &id, format!("{} builds kiln {} itself: no binary of it for {target} yet; nothing it runs is stopped", r.name, short(commit)));
    let inst8 = crate::updates::instance8(r);
    let script = build_script(&id, &bases, &token(hub, &id), commit, &candidates, keep, &inst8);
    let (h, machine) = (Arc::clone(hub), r.name.clone());
    crate::rt::spawn(async move { run_build(&h, &id, &machine, &script).await });
    Ok(job)
}

/// Runs a build script in a terminal on the machine and follows it.
async fn run_build(hub: &Arc<Hub>, id: &str, machine: &str, script: &str) {
    use crate::termsess::Frame;
    let u = &hub.updates;
    let session = crate::termsess::open_agent(hub, machine.to_owned(), Some(format!("sh -c '{script}'")), 200, 50);
    let mut att = session.attach();
    let mut pending = String::new();
    let (mut ok_marked, mut no_checkout) = (false, false);
    let mut feed = |text: &str| {
        pending.push_str(&crate::updates::plain(text));
        while let Some(i) = pending.find('\n') {
            let line: String = pending.drain(..=i).collect();
            let line = line.trim_end();
            if line.is_empty() || line.starts_with(crate::updates::HIDDEN_MARK) {
                continue;
            }
            if line.contains(crate::updates::OK_MARK) {
                ok_marked = true;
                continue;
            }
            if line.starts_with(NO_CHECKOUT_MARK) {
                no_checkout = true;
            }
            if let Some(sum) = line.strip_prefix(BUILT_MARK) {
                u.set_to_build(hub, id, &crate::updates::build_id(sum.trim()));
                continue;
            }
            let line = line.strip_prefix(crate::updates::FAIL_MARK).or_else(|| line.strip_prefix(NO_CHECKOUT_MARK)).map_or(line, str::trim);
            u.say(hub, id, line);
        }
    };
    feed(&String::from_utf8_lossy(&att.replay));
    let deadline = Instant::now() + BUILD_TIMEOUT;
    let result = loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let Ok(frame) = crate::rt::timeout(left, att.live.recv()).await else {
            break Err("the build didn't finish in an hour".to_owned());
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
            Ok(Frame::Size { .. }) | Err(async_broadcast::RecvError::Overflowed(_)) => {}
            Err(async_broadcast::RecvError::Closed) => break Err("the build's terminal closed".to_owned()),
        }
    };
    session.detach();
    if no_checkout {
        hub.updates.fleet.no_checkout.lock().insert(machine.to_owned(), Instant::now());
    }
    if let Err(e) = result
        && !ok_marked
    {
        if e.contains("an hour") {
            session.close();
        }
        let e = if no_checkout { "no kiln checkout on this machine to build from".to_owned() } else { e };
        u.finish(hub, id, Err(e));
        return;
    }
    // Installed: the new runner reports its build when it connects, which completes the job.
    let until = Instant::now() + REPORT_IN;
    while Instant::now() < until {
        if !u.running(id) {
            return;
        }
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
    fn short_and_commits() {
        assert_eq!(short("0123456789abcdef0123"), "0123456789ab");
        assert_eq!(short("abc"), "abc");
        assert!(is_commit("e8c17e544623c019c78fce08ee0931a79d5d09db"));
        assert!(!is_commit(""));
        assert!(!is_commit("main"));
        assert!(!is_commit("e8c17e5 "));
    }

    #[test]
    fn the_build_script_quotes_nothing_it_shouldnt() {
        let s = build_script("job1", &["https://kiln.example".to_owned()], "tok", "e8c17e544623c019c78fce08ee0931a79d5d09db", &["/home/ada/src/kiln".to_owned()], true, "");
        // It goes inside `sh -c '…'`: no single quotes, no backslashes.
        assert!(!s.contains('\'') && !s.contains('\\'), "{s}");
        assert!(s.starts_with(crate::updates::HIDDEN_MARK));
        assert!(s.contains("\"/home/ada/src/kiln\" \"$HOME/Projects/kiln\""));
        assert!(s.contains("/api/updates/jobs/job1/source"));
        assert!(s.contains("--data-binary @\"$bin\""));
        assert!(s.contains("KILN_COMMIT=e8c17e544623c019c78fce08ee0931a79d5d09db"));
        // A recent Rust: the machine's may be older than kiln's dependencies need.
        assert!(s.contains("rustup toolchain install stable") && s.contains("$cargo build"));
        let linux = build_script("job1", &["https://kiln.example".to_owned()], "tok", "e8c17e5", &[], false, "01a14794");
        assert!(!linux.contains("--data-binary"), "a Linux build isn't handed back");
        assert!(linux.contains("*-01a14794) svc=${svc%-01a14794}"));
    }

    /// The script under sh: with no checkout anywhere it says so and stops before building.
    #[cfg(unix)]
    #[test]
    fn the_build_script_stops_without_a_checkout() -> anyhow::Result<()> {
        let home = tempfile::tempdir()?;
        let s = build_script("j", &["http://127.0.0.1:9".to_owned()], "t", "e8c17e5", &["/nonexistent/kiln".to_owned()], false, "");
        let run = || std::process::Command::new("sh").arg("-c").arg(&s).env("HOME", home.path()).env("SHELL", "/bin/sh").output();
        let out = run()?;
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        assert_eq!(out.status.code(), Some(10), "{text}");
        assert!(text.contains(NO_CHECKOUT_MARK), "{text}");
        // A checkout found: it goes on to check the tools (and fails fetching from a closed port).
        let kiln = home.path().join("Projects/kiln");
        std::fs::create_dir_all(kiln.join("crates/kiln"))?;
        std::fs::create_dir_all(kiln.join("ui"))?;
        std::fs::write(kiln.join("crates/kiln/Cargo.toml"), "")?;
        std::fs::write(kiln.join("ui/package.json"), "{}")?;
        let out = run()?;
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(text.contains("kiln checkout:"), "{text}");
        assert!(!text.contains(NO_CHECKOUT_MARK), "{text}");
        assert_ne!(out.status.code(), Some(0), "{text}");
        Ok(())
    }
}
