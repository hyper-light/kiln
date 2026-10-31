//! U16: fleet discovery and opt-in rollout policy. Builds and deployments enter the durable
//! `updates::coordinator`; CI owns build placement and the native backend owns handover.

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
/// Builds remembered with their commit.
const KEEP_BUILDS: usize = 400;
/// Git questions to the mirror.
const GIT_TIMEOUT: Duration = Duration::from_secs(60);

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
    /// (machine, commit) → tries so far.
    tries: Mutex<HashMap<(String, String), Tries>>,
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
    crate::updates::binaries(hub).into_iter().find(|b| b.origin == "built" && b.target == target && b.sha.as_deref().is_some_and(|s| s.eq_ignore_ascii_case(commit)))
}

// ------------------------------------------------------------------ the newest commit

pub(crate) async fn release_repo(hub: &Hub) -> Option<crate::store::Repo> {
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

pub(crate) async fn is_ancestor(hub: &Hub, mirror: &str, a: &str, b: &str) -> bool {
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
        hub.emit(&crate::wire::events::ServerEvent::Updates);
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
    /// CI builds the requested release; the lifecycle coordinator waits for its artifact.
    Build,
    /// Nothing it can do now: why, as the end of a sentence about the machine ("waits for …").
    Wait(String),
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
    release_building(hub, &t.commit, target, true).await
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
/// U16: upgrades no longer create host build terminals (doctor compatibility view).
pub fn builds_on(_hub: &Hub, _machine: &str) -> bool { false }

/// U16: CI owns placement, joining and build deduplication.
pub async fn plan(hub: &Hub, r: &RunnerInfo, t: &Target) -> Plan {
    if r.method != "native" { return Plan::Wait("U16 compatibility refusal: the runner predates native upgrade commands".into()); }
    let Some(target) = r.target.as_deref() else { return Plan::Wait(format!("kiln has no build for {} on {}", r.os, r.arch)); };
    binary_for(hub, target, &t.commit).map_or(Plan::Build, Plan::Download)
}

/// The ways a person can have `r` upgraded to `t` now, each with what it does or why it can't.
pub async fn ways(hub: &Hub, r: &RunnerInfo, t: &Target) -> Vec<crate::updates::WayOpen> {
    use crate::updates::{Way, WayOpen};
    let s = short(&t.commit);
    let Some(target) = r.target.as_deref() else { return Vec::new() };
    let way = |way, ok, why: String, on: Option<String>| WayOpen { way, ok, why, on };
    let download = match binary_for(hub, target, &t.commit) {
        Some(_) if r.method != "native" => way(Way::Download, false, "U16 compatibility refusal: its runner predates native upgrade commands".to_owned(), None),
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
    let build = way(Way::Build, r.method == "native", if r.method == "native" {
        format!("builds {s} through the canonical CI release workflow, then deploys its verified artifact")
    } else { "U16 compatibility refusal: the runner predates native upgrade commands".into() }, None);
    vec![download, build]
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
        Plan::Build => ("build", format!("builds {s} through the canonical CI release workflow{when}")),
        Plan::Wait(why) => ("wait", why),
    })
}

// ------------------------------------------------------------------ following

/// Keep every runner on the newest kiln: decide it, and bring each runner behind to it.
pub async fn follow_loop(hub: Arc<Hub>) -> anyhow::Result<()> {
    use futures::FutureExt as _;
    crate::updates::coordinator::recover(&hub).await?;
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
            Plan::Build => Some(crate::updates::coordinator::request(hub, &r, &t.commit, "kiln", None).await),
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
    if let Some(c) = job.commit.clone() {
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
pub fn building_commit(_hub: &Hub, _job: &str) -> Option<String> { None }

/// U16: old source endpoints are read-compatible refusals; builds use CI checkout/artifacts.
pub async fn source(_hub: &Hub, _job: &str) -> anyhow::Result<async_channel::Receiver<bytes::Bytes>> {
    anyhow::bail!("legacy terminal builds are retired; request a canonical CI release workflow")
}

pub async fn on_built(_hub: &Arc<Hub>, _job: &str, _sha256: &str, _bytes: Option<Vec<u8>>) -> anyhow::Result<()> {
    anyhow::bail!("legacy terminal build uploads are retired; publish artifacts through the canonical CI release workflow")
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

}
