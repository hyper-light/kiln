//! U16: one durable runner lifecycle, consuming CI artifacts and publishing CD outcomes.
//! Native install/supervisor handover is an execution backend, never a build scheduler.

use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::json;
use crate::sql::OptionalExtension as _;

use super::{Binary, Hub, Job, JobState, RunnerInfo};
use crate::deploy::model::{Artifact, Kind, Origin, Release, State};

const JOBS: &str = "updates/lifecycle/jobs/";
const INTENTS: &str = "updates/lifecycle/intents/";

#[derive(Default)]
pub(crate) struct Coordinator {
    admission: async_lock::Mutex<()>,
    pub(super) build_start: async_lock::Mutex<()>,
    recovered: Mutex<bool>,
    active: Mutex<HashSet<String>>,
    /// Peer ids are connection-local. Legacy runners without instance ids are trusted only
    /// on the exact connection in this coordinator incarnation, never after recovery.
    incarnation: OnceLock<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct DispatchOwner {
    peer: crate::instances::PeerId,
    instance: Option<String>,
    incarnation: String,
    pub(super) blue_green: bool,
}

impl DispatchOwner {
    fn accepts(&self, hub: &Hub, peer: crate::instances::PeerId, instance: Option<&str>) -> bool {
        match self.instance.as_deref() {
            Some(expected) => !expected.is_empty() && instance == Some(expected),
            None => self.peer == peer && instance.is_none() && self.incarnation == incarnation(hub),
        }
    }
}

fn incarnation(hub: &Hub) -> &str { hub.updates.lifecycle.incarnation.get_or_init(crate::util::new_id) }

#[cfg(test)]
fn dispatch_owner(hub: &Hub, machine: &str) -> anyhow::Result<DispatchOwner> {
    let instances = hub.instances.lock();
    let peer = instances.newest(machine).ok_or_else(|| anyhow::anyhow!("{machine} disconnected before upgrade dispatch"))?;
    let runner = instances.get(peer).ok_or_else(|| anyhow::anyhow!("upgrade dispatch instance disappeared"))?;
    anyhow::ensure!(runner.name == machine && runner.caps & crate::proto::CAP_UPGRADE != 0, "U16 compatibility refusal: selected runner instance does not accept native upgrades");
    anyhow::ensure!(runner.id.as_deref() != Some(""), "runner announced an empty upgrade instance identity");
    Ok(DispatchOwner { peer, instance: runner.id.clone(), incarnation: incarnation(hub).to_owned(), blue_green: runner.caps & crate::proto::CAP_BLUE_GREEN != 0 })
}

/// Resolve only the recorded process, including its reconnect. Newest never grants authority.
pub(super) fn dispatch_peer(hub: &Hub, machine: &str, owner: &DispatchOwner) -> anyhow::Result<crate::instances::PeerId> {
    let instances = hub.instances.lock();
    instances.of(machine).into_iter().find(|peer| instances.get(*peer).is_some_and(|runner| runner.name == machine && owner.accepts(hub, *peer, runner.id.as_deref())))
        .ok_or_else(|| anyhow::anyhow!("the recorded upgrade dispatch instance of {machine} is disconnected"))
}

/// U16: admission fixes one authenticated process and TLS connection. Recovery never
/// recaptures newest, and a reconnect needs a fresh lifecycle request, not renewed authority.
#[derive(Clone, Serialize, Deserialize)]
struct ActivationOwner {
    peer: crate::instances::PeerId,
    process_nonce: String,
    connection: String,
}
impl ActivationOwner {
    fn admitted(hub: &Hub, machine: &str) -> anyhow::Result<Self> {
        let captured = hub.capture_newest_runner(machine, u64::from(crate::proto::CAP_UPGRADE) | crate::proto::CAP_EXPLICIT_ACTIVATION)
            .map_err(|_| super::UpgradeRefusal::Backend)?;
        Ok(Self { peer: captured.peer(), process_nonce: captured.process_nonce().to_owned(), connection: captured.connection_id() })
    }
    fn capture(&self, hub: &Hub, machine: &str) -> anyhow::Result<crate::instances::CapturedRunner> {
        let captured = hub.capture_original_runner(machine, self.peer, &self.process_nonce, u64::from(crate::proto::CAP_UPGRADE) | crate::proto::CAP_EXPLICIT_ACTIVATION)
            .map_err(|_| super::UpgradeRefusal::Connection)?;
        anyhow::ensure!(captured.connection_id() == self.connection, super::UpgradeRefusal::Connection);
        Ok(captured)
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct Intent {
    job: String,
    machine: String,
    target: String,
    commit: String,
    by: String,
    run: Option<String>,
    binary: Option<Binary>,
    release: Option<i64>,
    /// Persisted before sending: recovery observes; it never repeats an ambiguous command.
    sent: bool,
    #[serde(default)]
    rollback: Option<i64>,
    /// U16: installed atomically with `sent`, before the command can leave the server.
    /// An old blue/green process keeps this authority after its replacement is promoted.
    #[serde(default)]
    owner: Option<DispatchOwner>,
    /// Older unsent intents are readable but cannot invent activation authority.
    #[serde(default)]
    activation: Option<ActivationOwner>,
}

pub(super) async fn authenticate(hub: &Hub, machine: &str, peer: crate::instances::PeerId, id: &str) -> anyhow::Result<DispatchOwner> {
    let job = hub.updates.job(id).ok_or_else(|| anyhow::anyhow!("unknown upgrade job {id}"))?;
    anyhow::ensure!(job.kind == "runner" && job.target == machine, "upgrade evidence belongs to another runner or a server job");
    let value = hub.store.kv_get(format!("{INTENTS}{id}")).await?.ok_or_else(|| anyhow::anyhow!("upgrade has no lifecycle dispatch intent"))?;
    let intent: Intent = serde_json::from_str(&value)?;
    anyhow::ensure!(intent.job == id && intent.machine == machine && intent.sent, "upgrade was not dispatched to this runner");
    let owner = intent.owner.ok_or_else(|| anyhow::anyhow!("historical upgrade dispatch has no recorded process owner; runner evidence cannot establish its handover"))?;
    let instances = hub.instances.lock();
    let instance = instances.get(peer).ok_or_else(|| anyhow::anyhow!("upgrade evidence came from a disconnected runner process"))?;
    anyhow::ensure!(instance.name == machine && owner.accepts(hub, peer, instance.id.as_deref()), "upgrade evidence came from a different runner instance than its recorded dispatch owner");
    Ok(owner)
}

/// U16: persistence owns its strings; the wire view's finite vocabulary is reconstructed only
/// from program literals. Deserialization never borrows the Store buffer or invents a lifetime.
#[derive(Deserialize)]
struct PersistedJob {
    id: String,
    kind: String,
    target: String,
    method: String,
    state: JobState,
    started_at: i64,
    ended_at: Option<i64>,
    from_build: Option<String>,
    to_build: Option<String>,
    commit: Option<String>,
    on: Option<String>,
    reached: bool,
    log: Vec<String>,
    error: Option<String>,
    timed_out: Option<String>,
    #[serde(default)]
    blue_green: bool,
    #[serde(default)]
    handed_over: bool,
    #[serde(default)]
    revision: u64,
    #[serde(default)]
    dispatch_at: Option<i64>,
    #[serde(default)]
    idle_at: Option<i64>,
}

impl<'de> Deserialize<'de> for Job {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let stored = PersistedJob::deserialize(d)?;
        let kind = match stored.kind.as_str() {
            "runner" => "runner", "server" => "server",
            _ => return Err(serde::de::Error::custom("unknown update kind")),
        };
        let method = match stored.method.as_str() {
            "native" => "native", "build" => "build", "restart" => "restart", "replace" => "replace", "terminal" => "terminal",
            _ => return Err(serde::de::Error::custom("unknown update method")),
        };
        let timed_out = match stored.timed_out.as_deref() {
            None => None, Some("upgrade") => Some("upgrade"), Some("jobs") => Some("jobs"), Some("build") => Some("build"),
            _ => return Err(serde::de::Error::custom("unknown update timeout")),
        };
        Ok(Job {
            id: stored.id, kind, target: stored.target, method, state: stored.state,
            started_at: stored.started_at, ended_at: stored.ended_at, from_build: stored.from_build, to_build: stored.to_build,
            commit: stored.commit, on: stored.on, reached: stored.reached, log: stored.log, error: stored.error,
            timed_out, progress: false, blue_green: stored.blue_green, handed_over: stored.handed_over,
            revision: stored.revision, dispatch_at: stored.dispatch_at, idle_at: stored.idle_at,
        })
    }
}

/// U16: revisions are immutable accepted snapshots. Older callbacks cannot overwrite a
/// handover, and an equal revision cannot replace the payload its control already accepted.
pub(super) async fn save_job(store: &crate::store::Store, job: &Job) -> anyhow::Result<()> {
    let key = format!("{JOBS}{}", job.id);
    let value = serde_json::to_string(job)?;
    let revision = job.revision;
    let terminal = match job.state { JobState::Running => None, JobState::Done => Some("done"), JobState::Failed => Some("failed") };
    let control_key = terminal.map(|state| format!("upgrade:{}:{state}", job.id));
    let descriptor = json!({"consumer":"upgrade", "job":job});
    store.call(move |c| {
        let tx = crate::store::tx(c)?;
        let previous: Option<String> = tx.query_row("SELECT v FROM kv WHERE k=?1", [&key], |row| row.get(0)).optional()?;
        if let Some(previous) = previous {
            let previous: Job = serde_json::from_str(&previous)?;
            if previous.revision > revision { return Ok(()); }
            if previous.revision == revision {
                let previous = serde_json::to_value(&previous)?;
                anyhow::ensure!(descriptor.get("job") == Some(&previous), "conflicting upgrade payload for {key} at revision {revision}");
                return Ok(());
            }
        }
        let accepted = tx.execute("INSERT INTO kv (k, v) VALUES (?1, ?2) ON CONFLICT(k) DO UPDATE SET v = excluded.v WHERE COALESCE(json_extract(kv.v, '$.revision'), 0) < ?3", crate::sql::params![key, value, revision])?;
        if accepted == 1 && let Some(key) = control_key { crate::ci::schedule::outbox::control(&tx, &key, descriptor)?; }
        tx.commit()?;
        Ok(())
    }).await
}

pub(super) fn changed(hub: &Hub, job: Job) {
    // Server update jobs remain outside the runner lifecycle.
    if job.kind != "runner" { return; }
    let store = hub.store.clone();
    let records = Arc::clone(&hub.records);
    crate::rt::spawn(async move {
        if let Err(e) = save_job(&store, &job).await {
            tracing::error!(job = %job.id, error = %e, "U16: upgrade status persistence failed");
            return;
        }
        let result = async {
            if let Some(value) = store.kv_get(format!("{INTENTS}{}", job.id)).await? {
                let i: Intent = serde_json::from_str(&value)?;
                let latest = store.kv_get(format!("{JOBS}{}", job.id)).await?.ok_or_else(|| anyhow::anyhow!("upgrade snapshot vanished"))?;
                let snapshot: Job = serde_json::from_str(&latest)?;
                outcome_for(&*records, &snapshot, &i).await?;
            }
            anyhow::Ok(())
        }.await;
        if let Err(e) = result { tracing::error!(job = %job.id, error = %e, "U16: CD outcome reconciliation failed"); }
    });
}

async fn save(hub: &Hub, i: &Intent) -> anyhow::Result<()> {
    let (key, value) = (format!("{INTENTS}{}", i.job), serde_json::to_string(i)?);
    hub.store.call(move |c| {
        c.execute("INSERT INTO kv(k,v) VALUES (?1,?2) ON CONFLICT(k) DO UPDATE SET v=excluded.v WHERE NOT COALESCE(json_extract(kv.v, '$.sent'), 0)", crate::sql::params![key,value])?;
        Ok(())
    }).await
}

async fn claim_dispatch(hub: &Hub, i: &Intent) -> anyhow::Result<bool> {
    anyhow::ensure!(i.sent && i.owner.is_some(), "upgrade dispatch needs an explicit process owner");
    let (key, value) = (format!("{INTENTS}{}", i.job), serde_json::to_string(i)?);
    hub.store.call(move |c| {
        Ok(c.execute("UPDATE kv SET v=?2 WHERE k=?1 AND NOT COALESCE(json_extract(v, '$.sent'), 0)", crate::sql::params![key,value])? == 1)
    }).await
}

fn compatible(r: &RunnerInfo, target: &str) -> anyhow::Result<()> {
    anyhow::ensure!(r.method == "native", "U16 compatibility refusal: {} predates native upgrade commands; bootstrap a compatible runner with the person's authorization before requesting a CI/CD upgrade", r.name);
    anyhow::ensure!(r.target.as_deref() == Some(target) && super::known_target(target), "U16: artifact platform {target} does not match {} on {}", r.os, r.arch);
    anyhow::ensure!(r.connected, "{} is not connected", r.name);
    anyhow::ensure!(!r.locked && r.policy != super::Policy::Off, "{} refuses upgrades or its update policy is off", r.name);
    Ok(())
}

pub async fn request(hub: &Arc<Hub>, r: &RunnerInfo, commit: &str, by: &str, binary: Option<Binary>) -> anyhow::Result<Job> {
    request_kind(hub, r, commit, by, binary, None).await
}

async fn request_kind(hub: &Arc<Hub>, r: &RunnerInfo, commit: &str, by: &str, binary: Option<Binary>, rollback: Option<i64>) -> anyhow::Result<Job> {
    let _admission = hub.updates.lifecycle.admission.lock().await;
    recover_locked(hub).await?;
    if let Some(j) = hub.updates.running_for("runner", &r.name) { return Ok(j); }
    let target = r.target.clone().ok_or_else(|| anyhow::anyhow!("no release platform for {} on {}", r.os, r.arch))?;
    compatible(r, &target)?;
    let activation = ActivationOwner::admitted(hub, &r.name)?;
    anyhow::ensure!(commit.len() == 40 && commit.bytes().all(|b| b.is_ascii_hexdigit()), "U16: a full release commit is required");
    let mut job = super::new_job("runner", &r.name, "native", r.build.clone(), binary.as_ref().map(|b| b.build.clone()));
    job.commit = Some(commit.to_ascii_lowercase());
    let binary = binary.or_else(|| super::stored(hub, &target).filter(|b| b.origin == "built" && b.sha.as_deref() == Some(commit)));
    let i = Intent { job: job.id.clone(), machine: r.name.clone(), target, commit: commit.to_ascii_lowercase(), by: by.to_owned(), run: None, binary, release: None, sent: false, rollback, owner: None, activation: Some(activation) };
    // U16: admission is atomic across server processes sharing the Store. The slot fences
    // overlapping requests even if another server has not received the view's event yet.
    let (job_key, intent_key, slot) = (format!("{JOBS}{}", job.id), format!("{INTENTS}{}", job.id), format!("updates/lifecycle/machines/{}", r.name));
    let (job_json, intent_json, candidate) = (serde_json::to_string(&job)?, serde_json::to_string(&i)?, job.clone());
    let admitted = hub.store.call(move |c| {
        let tx = crate::store::tx(c)?;
        let prior: Option<String> = tx.query_row("SELECT v FROM kv WHERE k = ?1", [&slot], |r| r.get(0)).optional()?;
        if let Some(id) = prior {
            let value: Option<String> = tx.query_row("SELECT v FROM kv WHERE k = ?1", [format!("{JOBS}{id}")], |r| r.get(0)).optional()?;
            let old: Job = serde_json::from_str(&value.ok_or_else(|| anyhow::anyhow!("upgrade machine slot lost its job"))?)?;
            if old.state == JobState::Running { return Ok(old); }
        }
        for (k, v) in [(job_key, job_json), (intent_key, intent_json), (slot, candidate.id.clone())] {
            tx.execute("INSERT INTO kv(k,v) VALUES (?1,?2) ON CONFLICT(k) DO UPDATE SET v=excluded.v", crate::sql::params![k,v])?;
        }
        tx.commit()?;
        Ok(candidate)
    }).await?;
    if admitted.id != job.id {
        if hub.updates.job(&admitted.id).is_none() { hub.updates.insert(admitted.clone()); }
        return Ok(admitted);
    }
    hub.updates.insert(job.clone());
    hub.store.audit(by.into(), "upgrade.runner.request".into(), format!("{} through CI/CD at {}", r.name, i.commit)).await;
    spawn(hub, i);
    Ok(job)
}

/// U16: an explicit retry keeps the requested commit and pinned artifact, never runs a shell.
pub async fn retry(hub: &Arc<Hub>, machine: &str, job: &str, by: &str) -> anyhow::Result<Job> {
    let value = hub.store.kv_get(format!("{INTENTS}{job}")).await?.ok_or_else(|| anyhow::anyhow!("no upgrade intent {job}"))?;
    let i: Intent = serde_json::from_str(&value)?;
    anyhow::ensure!(i.machine == machine, "upgrade {job} belongs to another machine");
    let value = hub.store.kv_get(format!("{JOBS}{job}")).await?.ok_or_else(|| anyhow::anyhow!("upgrade snapshot vanished"))?;
    let previous: Job = serde_json::from_str(&value)?;
    anyhow::ensure!(previous.state == JobState::Failed, "only a failed upgrade can be retried");
    let r = super::runners(hub).await.into_iter().find(|r| r.name == i.machine).ok_or_else(|| anyhow::anyhow!("runner is not registered"))?;
    request_kind(hub, &r, &i.commit, by, i.binary, i.rollback).await
}

/// U16: rollback is another authorized deployment of a retained, verified CI artifact.
pub async fn rollback(hub: &Arc<Hub>, machine: &str, release: i64, by: &str) -> anyhow::Result<Job> {
    let r = crate::deploy::store::release(&*hub.records, release).await?.ok_or_else(|| anyhow::anyhow!("no CD release {release}"))?;
    anyhow::ensure!(r.app == "kiln-runner" && r.environment == format!("runner/{machine}") && r.provider.get("backend").and_then(serde_json::Value::as_str) == Some("runner-native"), "release is not this runner's native deployment");
    anyhow::ensure!(r.state == State::Succeeded, "rollback target did not succeed");
    let artifact = r.artifacts.get("binary").ok_or_else(|| anyhow::anyhow!("release lost its pinned binary"))?;
    let target = r.provider.get("platform").and_then(serde_json::Value::as_str).ok_or_else(|| anyhow::anyhow!("release lost its platform"))?;
    let value = hub.store.kv_get(format!("updates/artifacts/{target}/{}", artifact.tag)).await?.ok_or_else(|| anyhow::anyhow!("rollback artifact is no longer retained"))?;
    let binary: Binary = serde_json::from_str(&value)?;
    anyhow::ensure!(artifact.digest == format!("sha256:{}", binary.sha256), "rollback artifact digest does not match CD release");
    let runner = super::runners(hub).await.into_iter().find(|r| r.name == machine).ok_or_else(|| anyhow::anyhow!("runner is not registered"))?;
    let commit = r.version.ok_or_else(|| anyhow::anyhow!("release lost its version"))?;
    request_kind(hub, &runner, &commit, by, Some(binary), Some(release)).await
}

/// Called by the existing fleet worker and on admission; errors block new work until readable.
pub async fn recover(hub: &Arc<Hub>) -> anyhow::Result<()> {
    let _admission = hub.updates.lifecycle.admission.lock().await;
    recover_locked(hub).await
}

async fn recover_locked(hub: &Arc<Hub>) -> anyhow::Result<()> {
    if *hub.updates.lifecycle.recovered.lock() { return Ok(()); }
    let jobs = hub.store.kv_prefix(JOBS.into()).await?;
    for (_, value) in jobs {
        let job: Job = serde_json::from_str(&value)?;
        if hub.updates.job(&job.id).is_none() { hub.updates.insert(job); }
    }
    for (_, value) in hub.store.kv_prefix(INTENTS.into()).await? {
        let i: Intent = serde_json::from_str(&value)?;
        if hub.updates.job(&i.job).is_none() {
            if let Some(value) = hub.store.kv_get(format!("{JOBS}{}", i.job)).await? {
                let job: Job = serde_json::from_str(&value)?;
                if job.state != JobState::Running { outcome_for(&*hub.records, &job, &i).await?; continue; }
                hub.updates.insert(job);
                spawn(hub, i);
                continue;
            }
            let mut job = super::new_job("runner", &i.machine, "native", None, i.binary.as_ref().map(|b| b.build.clone()));
            job.id.clone_from(&i.job);
            job.commit = Some(i.commit.clone());
            save_job(&hub.store, &job).await?;
            hub.updates.insert(job);
        }
        spawn(hub, i);
    }
    *hub.updates.lifecycle.recovered.lock() = true;
    Ok(())
}

fn spawn(hub: &Arc<Hub>, i: Intent) {
    if !hub.updates.lifecycle.active.lock().insert(i.job.clone()) { return; }
    let h = Arc::clone(hub);
    crate::rt::spawn(async move {
        let id = i.job.clone();
        if let Err(e) = drive(&h, i.clone()).await
            && !h.shutdown.is_cancelled()
        {
            h.updates.finish(&h, &id, Err(format!("{e:#}")));
            // Read the latest intent: delivery may have recorded its release before failing.
            if let Ok(Some(value)) = h.store.kv_get(format!("{INTENTS}{id}")).await
                && let Ok(latest) = serde_json::from_str::<Intent>(&value)
                && let Err(error) = outcome(&h, &latest).await
            { tracing::error!(job = %id, error = %error, "U16: failed deployment outcome could not persist"); }
        }
        h.updates.lifecycle.active.lock().remove(&id);
    });
}

async fn build(hub: &Arc<Hub>, i: &mut Intent) -> anyhow::Result<Binary> {
    let repo = crate::fleet::release_repo(hub).await.ok_or_else(|| anyhow::anyhow!("release repository {} is not registered with CI", hub.cfg.updates.release_repo))?;
    if i.run.is_none() && i.binary.is_none() {
        let run = super::build_commit(hub, &repo, &i.commit).await?;
        i.run = Some(run.id);
        save(hub, i).await?;
        hub.updates.say(hub, &i.job, format!("waiting for canonical CI release run {}", i.run.as_deref().unwrap_or_default()));
    }
    loop {
        if hub.shutdown.is_cancelled() { anyhow::bail!("server stopped while following CI release"); }
        if let Some(b) = i.binary.clone().or_else(|| super::stored(hub, &i.target).filter(|b| b.sha.as_deref() == Some(i.commit.as_str()))) {
            anyhow::ensure!(b.origin == "built" && b.run.is_some() && b.target == i.target && b.sha.as_deref() == Some(i.commit.as_str()), "U16: artifact does not match canonical CI release");
            let (repo_id, number) = (repo.id.clone(), b.run);
            let id = hub.store.call(move |c| {
                Ok(c.query_row("SELECT id FROM runs WHERE repo_id=?1 AND number=?2", crate::sql::params![repo_id, number], |r| r.get::<_, String>(0)).optional()?)
            }).await?.ok_or_else(|| anyhow::anyhow!("artifact's canonical CI run vanished"))?;
            if i.run.is_none() { i.run = Some(id); save(hub, i).await?; }
            let run = hub.store.run(i.run.clone().ok_or_else(|| anyhow::anyhow!("missing CI run"))?).await?.ok_or_else(|| anyhow::anyhow!("CI run vanished"))?;
            if run.status == crate::store::Status::Succeeded {
                verify(hub, i, &b).await?;
                return Ok(b);
            }
            anyhow::ensure!(!run.status.is_terminal(), "canonical CI release {} ended {:?}", run.id, run.status);
        }
        let run = hub.store.run(i.run.clone().ok_or_else(|| anyhow::anyhow!("missing CI run"))?).await?.ok_or_else(|| anyhow::anyhow!("CI run vanished"))?;
        anyhow::ensure!(run.sha.eq_ignore_ascii_case(&i.commit) && run.repo_id == repo.id && run.pipeline == hub.cfg.updates.release_pipeline, "CI run does not match requested release");
        if run.status.is_terminal() && run.status != crate::store::Status::Succeeded { anyhow::bail!("CI release {} ended {:?}: {}", run.id, run.status, run.error.unwrap_or_default()); }
        let started = hub.updates.job(&i.job).map_or(crate::util::now_unix(), |j| j.started_at);
        if crate::util::now_unix().saturating_sub(started) > super::waits(hub).await.build().as_secs() as i64 {
            hub.updates.time_out(hub, &i.job, "build", "canonical CI release did not supply a verified platform artifact before the build deadline".into());
            anyhow::bail!("CI release build timed out");
        }
        crate::rt::sleep(Duration::from_secs(2)).await;
    }
}

async fn verify(hub: &Hub, i: &Intent, b: &Binary) -> anyhow::Result<()> {
    anyhow::ensure!(b.target == i.target && b.sha.as_deref() == Some(i.commit.as_str()) && b.origin == "built" && b.run.is_some(), "U16: artifact platform, commit or canonical CI origin does not match the requested release");
    anyhow::ensure!(b.sha256.len() == 64 && b.sha256.bytes().all(|c| c.is_ascii_hexdigit()) && b.build == super::build_id(&b.sha256) && b.size > 0 && b.size <= super::MAX_BINARY as u64, "U16: invalid artifact digest, size or build id");
    let req = crate::ci::attest::Requirement { workflows: vec![format!(".kiln/{}.yaml", hub.cfg.updates.release_pipeline)], branches: vec![hub.cfg.updates.release_branch.clone()], ..Default::default() };
    let id = crate::ci::attest::verify_provenance(hub, &format!("sha256:{}", b.sha256), &req).await?.map_err(|why| anyhow::anyhow!("U16: release provenance refused: {why}"))?;
    let (repo, pipeline, commit, number, run_id) = (hub.cfg.updates.release_repo.clone(), hub.cfg.updates.release_pipeline.clone(), i.commit.clone(), b.run, i.run.clone());
    let matches = hub.store.call(move |c| {
        Ok(c.query_row("SELECT EXISTS(SELECT 1 FROM ci_attestations a JOIN runs r ON r.id = a.run_id JOIN repos p ON p.id = r.repo_id WHERE a.id = ?1 AND p.name = ?2 AND r.pipeline = ?3 AND lower(r.sha) = ?4 AND r.number = ?5 AND r.status = 'succeeded' AND (?6 IS NULL OR r.id = ?6))", crate::sql::params![id, repo, pipeline, commit, number, run_id], |r| r.get::<_, bool>(0))?)
    }).await?;
    anyhow::ensure!(matches, "U16: verified provenance belongs to another CI run, repository or commit");
    Ok(())
}

async fn deployment(hub: &Hub, i: &Intent, b: &Binary) -> anyhow::Result<Release> {
    // Canonical CD record/outcome adapter for a native runner; no Helm/Kubernetes side effect.
    crate::deploy::store::adopt(&*hub.records, &hub.store).await?;
    let r = Release {
        id: 0, app: "kiln-runner".into(), environment: format!("runner/{}", i.machine), kind: if i.rollback.is_some() { Kind::Rollback } else { Kind::Deploy }, state: State::Delivering, origin: Origin::Api,
        version: Some(i.commit.clone()), artifacts: BTreeMap::from([("binary".into(), Artifact { repository: format!("/dl/kiln/{}/{}", b.target, b.build), tag: b.build.clone(), digest: format!("sha256:{}", b.sha256) })]),
        values: None, commit: None, requested_by: i.by.clone(), message: Some("native runner handover".into()), rollback_of: i.rollback, rolled_back_by: None, keep: false,
        provider: json!({"backend":"runner-native", "upgrade_job": i.job, "platform": i.target, "ci_run": i.run, "previous_build": hub.updates.job(&i.job).and_then(|j| j.from_build)}),
        gates: Vec::new(), approvals: Vec::new(), promoted_from: None, idempotency_key: Some(format!("upgrade:{}", i.job)), created_at: crate::util::now_unix(), committed_at: None, ended_at: None,
        secrets: None, no_revert: true, reverts: None, reverted_by: None, irreversible: None,
    };
    Ok(crate::deploy::store::insert_release(&*hub.records, &r).await?.0)
}

async fn drive(hub: &Arc<Hub>, mut i: Intent) -> anyhow::Result<()> {
    if !hub.updates.running(&i.job) { return outcome(hub, &i).await; }
    // Check before even building: unsupported or recovered unbound jobs fail loudly and
    // cannot silently target a newly connected process after the person's admission.
    let captured = if i.sent { None } else {
        Some(i.activation.as_ref().ok_or(super::UpgradeRefusal::Unbound)?.capture(hub, &i.machine)?)
    };
    let b = if i.sent {
        i.binary.clone().ok_or_else(|| anyhow::anyhow!("dispatched upgrade lost its pinned artifact"))?
    } else { build(hub, &mut i).await? };
    if !i.sent {
        let r = super::runners(hub).await.into_iter().find(|r| r.name == i.machine).ok_or_else(|| anyhow::anyhow!("runner disappeared before deployment"))?;
        compatible(&r, &i.target)?;
        let release = deployment(hub, &i, &b).await?;
        i.release = Some(release.id);
        hub.updates.say(hub, &i.job, format!("native deployment recorded as CD release #{}", release.id));
        i.binary = Some(b.clone());
        i.sent = true;
        let captured = captured.as_ref().ok_or(super::UpgradeRefusal::Unbound)?;
        let owner = {
            let instances = hub.instances.lock();
            anyhow::ensure!(instances.still_captured(captured), super::UpgradeRefusal::Connection);
            let runner = instances.get(captured.peer()).ok_or(super::UpgradeRefusal::Connection)?;
            DispatchOwner { peer: captured.peer(), instance: runner.id.clone(), incarnation: incarnation(hub).to_owned(), blue_green: true }
        };
        let blue_green = owner.blue_green;
        i.owner = Some(owner.clone());
        hub.updates.update(hub, &i.job, |j| { j.to_build = Some(b.build.clone()); j.from_build.clone_from(&r.build); j.blue_green = blue_green; j.dispatch_at = Some(crate::util::now_unix()); j.idle_at = None; });
        save_job(&hub.store, &hub.updates.job(&i.job).ok_or_else(|| anyhow::anyhow!("upgrade view vanished"))?).await?;
        if claim_dispatch(hub, &i).await? {
            super::native(hub, &i.job, &i.machine, &b, &owner, captured).await;
        } else {
            // A second coordinator won the durable dispatch fence. Observe its result only.
            if let Some(value) = hub.store.kv_get(format!("{INTENTS}{}", i.job)).await? {
                i = serde_json::from_str(&value)?;
                if let Some(owner) = &i.owner { hub.updates.update(hub, &i.job, |j| j.blue_green = owner.blue_green); }
            }
            super::monitor(hub, &i.job, &i.machine).await;
        }
    } else {
        // Reconcile evidence already reported while this coordinator was down, without reinstalling.
        if let Some(owner) = &i.owner { hub.updates.update(hub, &i.job, |j| j.blue_green = owner.blue_green); }
        hub.updates.on_build(hub, &i.machine, crate::fleet::newest_build(hub, &i.machine).as_deref().unwrap_or_default());
        super::monitor(hub, &i.job, &i.machine).await;
    }
    outcome(hub, &i).await
}

async fn outcome(hub: &Hub, i: &Intent) -> anyhow::Result<()> {
    let Some(job) = hub.updates.job(&i.job) else { anyhow::bail!("upgrade view vanished") };
    save_job(&hub.store, &job).await?;
    outcome_for(&*hub.records, &job, i).await
}

async fn outcome_for(records: &dyn kiln_seams::Records, job: &Job, i: &Intent) -> anyhow::Result<()> {
    let Some(id) = i.release else { return Ok(()) };
    let Some(mut release) = crate::deploy::store::release(records, id).await? else { anyhow::bail!("runner CD release {id} vanished") };
    if job.state == JobState::Running { return Ok(()); }
    let was = release.state;
    if was == State::Succeeded && job.state != JobState::Done { return Ok(()); }
    anyhow::ensure!(job.state != JobState::Done || (job.reached && (!job.blue_green || job.handed_over)), "runner deployment cannot succeed without build and handover evidence");
    release.state = if job.state == JobState::Done { State::Succeeded } else { State::Failed };
    release.message.clone_from(&job.error);
    release.ended_at = job.ended_at;
    let provider = release.provider.as_object_mut().ok_or_else(|| anyhow::anyhow!("runner provider status is not an object"))?;
    provider.insert("reached".into(), json!(job.reached));
    provider.insert("handed_over".into(), json!(job.handed_over));
    if job.state == JobState::Done { provider.insert("landed_at".into(), json!(job.ended_at)); }
    anyhow::ensure!(crate::deploy::store::update_release(records, &release, was).await?, "runner CD release changed while recording outcome");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

    fn binary() -> Binary {
        Binary { target: crate::api::TARGET.into(), sha256: "a".repeat(64), size: 2 << 20, origin: "built".into(), at: 1, build: "a".repeat(12), run: Some(1), sha: Some(COMMIT.into()), by: None }
    }

    fn intent(job: &Job) -> Intent {
        Intent { job: job.id.clone(), machine: job.target.clone(), target: crate::api::TARGET.into(), commit: COMMIT.into(), by: "person".into(), run: Some("run".into()), binary: Some(binary()), release: None, sent: false, rollback: None, owner: None, activation: None }
    }

    fn connect(hub: &Hub, machine: &str, peer: u64, id: Option<&str>, created_at: i64) {
        let hello = crate::proto::Hello { protocol: crate::proto::VERSION, name: machine.into(), labels: Vec::new(), slots: 1, hostname: machine.into(), os: "linux".into(), arch: "x86_64".into(), version: String::new(), ephemeral: false, dedicated_job: None, running: Vec::new(), terminal: false };
        let mut instances = hub.instances.lock();
        if let Some(id) = id { instances.announce(peer, crate::proto::InstanceHello { id: id.into(), created_at, pid: 7 }); }
        instances.join(peer, machine, hello, created_at);
        instances.set_build(peer, "old".into(), crate::proto::CAP_UPGRADE | crate::proto::CAP_BLUE_GREEN);
    }

    #[test]
    fn u16_wrong_runner_and_stale_instance_cannot_log_or_complete_an_upgrade() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            connect(&hub, "fixture", 1, Some("dispatch-owner"), 1);
            let mut job = super::super::new_job("runner", "fixture", "native", Some("old".into()), Some(binary().build));
            job.blue_green = true;
            job.reached = true;
            job.state = JobState::Failed;
            job.timed_out = Some("upgrade");
            job.error = Some("timed out before handover".into());
            save_job(&hub.store, &job).await?;
            let mut sent = intent(&job);
            sent.sent = true;
            sent.owner = Some(dispatch_owner(&hub, "fixture")?);
            save(&hub, &sent).await?;
            hub.updates.insert(job.clone());
            // A replacement is now newest, but the old process retains completion authority.
            connect(&hub, "fixture", 2, Some("newest-process"), 2);
            connect(&hub, "other-machine", 3, Some("dispatch-owner"), 3);
            for (machine, peer) in [("other-machine", 3), ("fixture", 2)] {
                assert!(hub.updates.on_log(&hub, machine, peer, &job.id, "spoofed log".into()).await.is_err());
                assert!(hub.updates.on_done(&hub, machine, peer, &job.id, true, None).await.is_err());
                assert!(hub.updates.on_done(&hub, machine, peer, &job.id, false, Some("spoofed failure".into())).await.is_err());
            }
            assert_eq!(serde_json::to_value(hub.updates.job(&job.id))?, serde_json::to_value(Some(&job))?, "refused evidence leaves job and revision unchanged");
            let stored: Job = serde_json::from_str(&hub.store.kv_get(format!("{JOBS}{}", job.id)).await?.ok_or_else(|| anyhow::anyhow!("snapshot vanished"))?)?;
            assert_eq!(serde_json::to_value(&stored)?, serde_json::to_value(&job)?, "refused evidence does not publish durable progress");
            assert_eq!(dispatch_peer(&hub, "fixture", sent.owner.as_ref().ok_or_else(|| anyhow::anyhow!("missing owner"))?)?, 1, "dispatch never moves to newest");
            hub.updates.on_log(&hub, "fixture", 1, &job.id, "owner handover".into()).await?;
            hub.updates.on_done(&hub, "fixture", 1, &job.id, true, None).await?;
            let landed = hub.updates.job(&job.id).ok_or_else(|| anyhow::anyhow!("job vanished"))?;
            assert_eq!(landed.state, JobState::Done);
            assert!(landed.handed_over && landed.reached, "legitimate old blue/green owner can complete a late handover");
            save_job(&hub.store, &landed).await?;
            let restored: Job = serde_json::from_str(&hub.store.kv_get(format!("{JOBS}{}", job.id)).await?.ok_or_else(|| anyhow::anyhow!("snapshot vanished"))?)?;
            assert_eq!(restored.state, JobState::Done);
            assert!(restored.handed_over);
            hub.shutdown.cancel();
            Ok(())
        })
    }

    #[test]
    fn u16_dispatch_owner_recovers_on_same_instance_reconnect_and_refuses_unowned_history() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            connect(&hub, "fixture", 1, Some("recorded-process"), 1);
            let job = super::super::new_job("runner", "fixture", "native", None, Some(binary().build));
            let mut sent = intent(&job);
            sent.sent = true;
            let mut owner = dispatch_owner(&hub, "fixture")?;
            owner.incarnation = "previous-server-incarnation".into();
            sent.owner = Some(owner);
            save(&hub, &sent).await?;
            save_job(&hub.store, &job).await?;
            hub.updates.insert(job.clone());
            // U16: admission requires the live process/capability declaration; reconnect never
            // renews command authority, although an already-sent owner may report its result.
            assert!(ActivationOwner::admitted(&hub, "fixture").is_err());
            let connection = crate::net::Connection::new(1, 1, [7; 32]);
            {
                let mut instances = hub.instances.lock();
                instances.connected(1, 1, Some([7; 32]));
                instances.announce(1, crate::proto::InstanceHello { id: "recorded-process".into(), created_at: 1, pid: 7 });
                instances.join(1, "fixture", crate::proto::Hello { protocol: crate::proto::VERSION, name: "fixture".into(), labels: Vec::new(), slots: 1, hostname: "fixture".into(), os: "linux".into(), arch: "x86_64".into(), version: String::new(), ephemeral: false, dedicated_job: None, running: Vec::new(), terminal: false }, 1);
                instances.set_build(1, "old".into(), crate::proto::CAP_UPGRADE | crate::proto::CAP_BLUE_GREEN);
                instances.set_wide(1, crate::proto::CAP_WIRE | crate::proto::CAP_UNIFIED_LAYOUT | crate::proto::CAP_EXPLICIT_ACTIVATION);
                instances.set_process(1, "a".repeat(43), Some(connection))?;
            }
            let activation = ActivationOwner::admitted(&hub, "fixture")?;
            assert!(activation.capture(&hub, "fixture").is_ok());
            {
                let mut instances = hub.instances.lock();
                instances.set_wide(1, crate::proto::CAP_WIRE | crate::proto::CAP_UNIFIED_LAYOUT);
            }
            assert!(activation.capture(&hub, "fixture").is_err(), "capability loss refuses dispatch");
            {
                let mut instances = hub.instances.lock();
                instances.connected(1, 2, Some([8; 32]));
                instances.announce(1, crate::proto::InstanceHello { id: "recorded-process".into(), created_at: 1, pid: 7 });
                instances.join(1, "fixture", crate::proto::Hello { protocol: crate::proto::VERSION, name: "fixture".into(), labels: Vec::new(), slots: 1, hostname: "fixture".into(), os: "linux".into(), arch: "x86_64".into(), version: String::new(), ephemeral: false, dedicated_job: None, running: Vec::new(), terminal: false }, 2);
                instances.set_build(1, "old".into(), crate::proto::CAP_UPGRADE | crate::proto::CAP_BLUE_GREEN);
                instances.set_wide(1, crate::proto::CAP_WIRE | crate::proto::CAP_UNIFIED_LAYOUT | crate::proto::CAP_EXPLICIT_ACTIVATION);
                instances.set_process(1, "a".repeat(43), Some(crate::net::Connection::new(1, 2, [8; 32])))?;
            }
            assert!(activation.capture(&hub, "fixture").is_err(), "same peer and process on a fresh capable connection cannot replace admission");
            connect(&hub, "fixture", 4, Some("recorded-process"), 1);
            assert!(activation.capture(&hub, "fixture").is_err(), "no newest/reconnect substitution");
            assert!(hub.updates.on_done(&hub, "fixture", 1, &job.id, true, None).await.is_err(), "stale connection is gone");
            assert!(hub.updates.on_done(&hub, "fixture", 4, &job.id, true, None).await.is_ok(), "persisted instance authority survives a new connection and server incarnation");
            let server = super::super::new_job("server", "fixture", "replace", None, None);
            hub.updates.insert(server.clone());
            assert!(hub.updates.on_done(&hub, "fixture", 4, &server.id, true, None).await.is_err());
            let legacy = super::super::new_job("runner", "fixture", "native", None, None);
            let mut history = intent(&legacy); history.sent = true;
            save(&hub, &history).await?; hub.updates.insert(legacy.clone());
            assert!(hub.updates.on_log(&hub, "fixture", 4, &legacy.id, "unowned".into()).await.is_err(), "old sent records without an owner cannot invent handover authority");
            let waiting = super::super::new_job("runner", "fixture", "native", None, None);
            let mut unsent = intent(&waiting); unsent.owner = Some(dispatch_owner(&hub, "fixture")?);
            save(&hub, &unsent).await?; hub.updates.insert(waiting.clone());
            let refusal = drive(&hub, unsent.clone()).await.err().ok_or_else(|| anyhow::anyhow!("unbound history accepted activation"))?;
            assert!(matches!(refusal.downcast_ref::<super::super::UpgradeRefusal>(), Some(super::super::UpgradeRefusal::Unbound)), "old pending intents fail before a build or command");
            assert!(hub.updates.on_done(&hub, "fixture", 4, &waiting.id, true, None).await.is_err(), "an undispatched job has no completion authority");
            connect(&hub, "legacy", 5, None, 1);
            let legacy_job = super::super::new_job("runner", "legacy", "native", None, None);
            let mut legacy_intent = intent(&legacy_job); legacy_intent.sent = true;
            let mut legacy_owner = dispatch_owner(&hub, "legacy")?;
            legacy_owner.incarnation = "previous-server-incarnation".into(); legacy_intent.owner = Some(legacy_owner);
            save(&hub, &legacy_intent).await?; hub.updates.insert(legacy_job.clone());
            assert!(hub.updates.on_done(&hub, "legacy", 5, &legacy_job.id, true, None).await.is_err(), "a recycled peer id cannot recover an unidentified process owner");
            hub.shutdown.cancel();
            Ok(())
        })
    }

    fn runner() -> RunnerInfo {
        RunnerInfo {
            name: "fixture".into(), hostname: "fixture".into(), os: std::env::consts::OS.into(), arch: std::env::consts::ARCH.into(), connected: true, version: "fixture".into(),
            target: Some(crate::api::TARGET.into()), target_build: None, build: Some("old".into()), up_to_date: false, commit: None, standing: None, next: None, how: None, waits_on: None,
            checkout: None, method: "native", terminal: true, policy: super::super::Policy::Notify, locked: false, from: None, ways: Vec::new(), instances: Vec::new(),
        }
    }

    #[test]
    fn u16_persisted_job_outlives_its_store_buffer_and_refuses_unknown_vocabulary() -> anyhow::Result<()> {
        fn owned<T: serde::de::DeserializeOwned>() {}
        owned::<Job>();
        let restored = {
            let mut job = super::super::new_job("runner", "fixture", "native", Some("old".into()), None);
            job.timed_out = Some("jobs");
            job.blue_green = true;
            let buffer = serde_json::to_string(&job)?;
            serde_json::from_str::<Job>(&buffer)?
        };
        assert_eq!((restored.kind, restored.method, restored.timed_out), ("runner", "native", Some("jobs")));
        assert!(restored.blue_green);
        let original = serde_json::to_value(&restored)?;
        for field in ["kind", "method", "timed_out"] {
            let mut bad = original.clone();
            bad.as_object_mut().ok_or_else(|| anyhow::anyhow!("job is not an object"))?.insert(field.into(), json!("unknown"));
            assert!(serde_json::from_value::<Job>(bad).is_err(), "unknown {field} accepted");
        }
        Ok(())
    }

    #[test]
    fn u16_old_runner_is_refused_even_with_terminals_and_platforms_must_match() -> anyhow::Result<()> {
        let mut r = runner();
        r.method = "incompatible";
        let e = compatible(&r, crate::api::TARGET).err().ok_or_else(|| anyhow::anyhow!("old runner accepted"))?;
        assert!(e.to_string().contains("compatibility refusal"));
        r.method = "native";
        assert!(compatible(&r, crate::api::TARGET).is_ok());
        assert!(compatible(&r, "another-platform").is_err());
        Ok(())
    }

    #[test]
    fn u16_recovery_keeps_handover_evidence_and_older_writes_cannot_erase_it() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let old = super::super::new_job("runner", "fixture", "native", Some("old".into()), Some(binary().build));
            let mut landed = old.clone();
            landed.state = JobState::Done;
            landed.reached = true;
            landed.blue_green = true;
            landed.handed_over = true;
            landed.revision = 9;
            save_job(&hub.store, &landed).await?;
            save_job(&hub.store, &old).await?;
            let mut i = intent(&landed);
            i.sent = true;
            // A dispatched intent is normally installed by claim_dispatch, not save.
            hub.store.kv_set(format!("{INTENTS}{}", i.job), serde_json::to_string(&i)?).await?;
            recover(&hub).await?;
            let restored = hub.updates.job(&landed.id).ok_or_else(|| anyhow::anyhow!("snapshot not recovered"))?;
            assert_eq!(restored.state, JobState::Done);
            assert!(restored.reached && restored.blue_green && restored.handed_over);
            assert_eq!(restored.revision, 9);
            hub.shutdown.cancel();
            Ok(())
        })
    }

    #[test]
    fn u16_accepted_job_revision_and_control_intent_commit_together_without_stale_triggers() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let mut accepted = super::super::new_job("runner", "fixture", "native", Some("old".into()), Some(binary().build));
            save_job(&hub.store, &accepted).await?;
            let running_controls: i64 = hub.store.call(|c| Ok(c.query_row("SELECT count(*) FROM ci_trigger_outbox WHERE json_extract(payload,'$.descriptor.consumer')='upgrade'", [], |r| r.get(0))?)).await?;
            assert_eq!(running_controls, 0, "running progress is not a terminal control transition");
            accepted.revision = 3;
            accepted.state = JobState::Failed;
            accepted.error = Some("accepted failure".into());
            save_job(&hub.store, &accepted).await?;
            save_job(&hub.store, &accepted).await?;
            let mut conflicting = accepted.clone(); conflicting.error = Some("different same-revision failure".into());
            let refusal = save_job(&hub.store, &conflicting).await.err().ok_or_else(|| anyhow::anyhow!("conflicting equal revision was accepted"))?;
            assert!(refusal.to_string().contains("conflicting upgrade payload"));
            let unchanged: Job = serde_json::from_str(&hub.store.kv_get(format!("{JOBS}{}", accepted.id)).await?.ok_or_else(|| anyhow::anyhow!("accepted revision disappeared"))?)?;
            assert_eq!(serde_json::to_value(&unchanged)?,serde_json::to_value(&accepted)?,"equal-revision conflict cannot replace the accepted canonical payload");
            let mut stale = accepted.clone(); stale.revision = 2; stale.error = Some("stale failure".into());
            save_job(&hub.store, &stale).await?;
            let mut diagnostic = accepted.clone(); diagnostic.revision = 4; diagnostic.error = Some("later failure detail".into());
            save_job(&hub.store, &diagnostic).await?;
            let controls: Vec<String> = hub.store.call(|c| {
                Ok(c.prepare("SELECT payload FROM ci_trigger_outbox WHERE kind='control' AND json_extract(payload,'$.descriptor.consumer')='upgrade'")?.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?)
            }).await?;
            assert_eq!(controls.len(), 1, "duplicate, conflicting, stale and later diagnostic revisions cannot enqueue another terminal transition");
            let descriptor: serde_json::Value = serde_json::from_str(controls.first().ok_or_else(|| anyhow::anyhow!("missing upgrade control"))?)?;
            assert_eq!(descriptor.pointer("/descriptor/job/revision"), Some(&json!(3)));
            assert_eq!(descriptor.pointer("/descriptor/job/error"), Some(&json!("accepted failure")));
            let mut done = diagnostic.clone(); done.state = JobState::Done; done.revision = 5;
            save_job(&hub.store, &done).await?;
            let count: i64 = hub.store.call(|c| Ok(c.query_row("SELECT count(*) FROM ci_trigger_outbox WHERE json_extract(payload,'$.descriptor.consumer')='upgrade'", [], |r| r.get(0))?)).await?;
            assert_eq!(count, 2, "a distinct accepted terminal state has its own immutable receipt");
            // A refused control write must roll back the state revision in the same SQL tx.
            hub.store.call(|c| { c.execute("DROP TABLE ci_trigger_outbox", [])?; Ok(()) }).await?;
            let mut blocked = accepted.clone(); blocked.revision = 6; blocked.error = Some("not durably accepted".into());
            assert!(save_job(&hub.store, &blocked).await.is_err());
            let restored: Job = serde_json::from_str(&hub.store.kv_get(format!("{JOBS}{}", accepted.id)).await?.ok_or_else(|| anyhow::anyhow!("job disappeared"))?)?;
            assert_eq!(restored.revision, 5);
            assert_eq!(restored.state, JobState::Done);
            assert_eq!(restored.error, done.error);
            hub.shutdown.cancel();
            Ok(())
        })
    }

    #[test]
    fn u16_competing_coordinators_dispatch_once_and_stale_progress_cannot_clear_the_fence() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let job = super::super::new_job("runner", "fixture", "native", None, None);
            let waiting = intent(&job);
            save(&hub, &waiting).await?;
            let mut sent = waiting.clone();
            sent.sent = true;
            sent.owner = Some(DispatchOwner { peer: 1, instance: Some("owner".into()), incarnation: incarnation(&hub).into(), blue_green: true });
            let mut other = sent.clone();
            other.owner = Some(DispatchOwner { peer: 2, instance: Some("other".into()), incarnation: incarnation(&hub).into(), blue_green: true });
            let (a, b) = futures::join!(claim_dispatch(&hub, &sent), claim_dispatch(&hub, &other));
            let first = a?;
            assert_ne!(first, b?, "exactly one coordinator owns dispatch");
            save(&hub, &waiting).await?;
            let value = hub.store.kv_get(format!("{INTENTS}{}", job.id)).await?.ok_or_else(|| anyhow::anyhow!("intent vanished"))?;
            let restored: Intent = serde_json::from_str(&value)?;
            assert!(restored.sent, "recovery must observe, never send another command");
            assert_eq!(restored.owner.as_ref().and_then(|owner| owner.instance.as_deref()), Some(if first { "owner" } else { "other" }), "a losing claim or stale save cannot overwrite the dispatch owner");
            Ok(())
        })
    }

    #[test]
    fn u16_cd_success_requires_build_and_handover_and_survives_stale_failure() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let mut job = super::super::new_job("runner", "fixture", "native", Some("old".into()), Some(binary().build));
            let mut i = intent(&job);
            let release = deployment(&hub, &i, &binary()).await?;
            assert_eq!(deployment(&hub, &i, &binary()).await?.id, release.id, "recovery reuses the CD release");
            i.release = Some(release.id);
            job.state = JobState::Done;
            job.blue_green = true;
            assert!(outcome_for(&*hub.records, &job, &i).await.is_err());
            job.reached = true;
            assert!(outcome_for(&*hub.records, &job, &i).await.is_err());
            job.handed_over = true;
            job.ended_at = Some(30);
            outcome_for(&*hub.records, &job, &i).await?;
            job.state = JobState::Failed;
            outcome_for(&*hub.records, &job, &i).await?;
            let landed = crate::deploy::store::release(&*hub.records, release.id).await?.ok_or_else(|| anyhow::anyhow!("release vanished"))?;
            assert_eq!(landed.state, State::Succeeded);
            assert_eq!(landed.provider.get("landed_at"), Some(&json!(30)));
            assert_eq!(landed.artifacts.get("binary").map(|a| a.digest.as_str()), Some(format!("sha256:{}", binary().sha256).as_str()));
            Ok(())
        })
    }

    #[test]
    fn u16_artifacts_keep_their_digest_after_latest_changes_and_history_is_recovered() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let target = "x86_64-unknown-linux-musl";
            let bytes = |fill| { let mut b = b"\x7fELF".to_vec(); b.resize(2 << 20, fill); b };
            let first = super::super::keep(&hub, target, bytes(1), "built", Some((1, COMMIT.into())), None).await?;
            let second = super::super::keep(&hub, target, bytes(2), "built", Some((2, COMMIT.into())), None).await?;
            assert_ne!(first.sha256, second.sha256);
            hub.updates.history.lock().clear();
            assert!(super::super::load(&hub).await);
            let (meta, rx) = super::super::stream_build(&hub, target, &first.build).ok_or_else(|| anyhow::anyhow!("pinned artifact unavailable"))?;
            let mut got = Vec::new();
            while let Ok(chunk) = rx.recv().await { got.extend_from_slice(&chunk); }
            assert_eq!(meta.sha256, crate::util::sha256_hex(&got));
            assert_eq!(got, bytes(1));
            Ok(())
        })
    }

    #[test]
    fn u16_non_ci_platform_commit_and_unsigned_artifacts_are_refused() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let job = super::super::new_job("runner", "fixture", "native", None, None);
            let i = intent(&job);
            let b = binary();
            let mut upload = b.clone(); upload.origin = "upload".into();
            assert!(verify(&hub, &i, &upload).await.is_err());
            let mut wrong = b.clone(); wrong.sha = Some("f".repeat(40));
            assert!(verify(&hub, &i, &wrong).await.is_err());
            let mut wrong = b.clone(); wrong.target = "another-platform".into();
            assert!(verify(&hub, &i, &wrong).await.is_err());
            let e = verify(&hub, &i, &b).await.err().ok_or_else(|| anyhow::anyhow!("unsigned artifact accepted"))?;
            assert!(e.to_string().contains("provenance refused"));
            Ok(())
        })
    }
}

#[cfg(test)]
mod signed_release_tests {
    use super::*;

    /// U16: an authorized request can consume an actual signed canonical CI build without
    /// executing an installer; a different run/commit and a damaged signature are refused.
    #[test]
    fn u16_signed_ci_artifact_enters_the_native_deployment_adapter() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let commit = "0123456789abcdef0123456789abcdef01234567";
            let repo = hub.store.upsert_repo(crate::store::Repo {
                id: "release-repo".into(), name: hub.cfg.updates.release_repo.clone(), clone_url: String::new(), branches: vec!["main".into()], poll_seconds: 0, enabled: true,
                created_at: 0, last_polled_at: None, last_error: None, mode: "upstream".into(), default_branch: None, mirror: None,
            }).await?;
            let run = hub.store.create_run(crate::store::Run {
                id: "run".into(), repo_id: repo.id, repo_name: repo.name, number: 0, pipeline: hub.cfg.updates.release_pipeline.clone(), event: "workflow_dispatch".into(),
                branch: hub.cfg.updates.release_branch.clone(), sha: commit.into(), message: String::new(), author: "person".into(), status: crate::store::Status::Succeeded,
                created_at: 0, started_at: None, finished_at: None, error: None, concurrency_group: None,
            }, Vec::new()).await?;
            hub.store.call(|c| { c.execute("UPDATE runs SET status='succeeded' WHERE id='run'", [])?; Ok(()) }).await?;
            let mut bytes = b"\x7fELF".to_vec(); bytes.resize(2 << 20, 7);
            let b = super::super::keep(&hub, "x86_64-unknown-linux-musl", bytes, "built", Some((run.number, commit.into())), None).await?;
            let subject = kiln_ci::attest::Subject { name: format!("dist/kiln-{}", b.target), digest: BTreeMap::from([("sha256".into(), b.sha256.clone())]) };
            let facts = kiln_ci::attest::Facts {
                builder_id: format!("{}/ci@v1", hub.cfg.server.public_url.trim_end_matches('/')), repository: format!("{}/{}", hub.cfg.server.public_url.trim_end_matches('/'), hub.cfg.updates.release_repo), workflow_path: format!(".kiln/{}.yaml", hub.cfg.updates.release_pipeline),
                git_ref: format!("refs/heads/{}", hub.cfg.updates.release_branch), sha: commit.into(), ..Default::default()
            };
            crate::ci::attest::keep_provenance(&hub, &[subject], &facts).await?;
            let job = super::super::new_job("runner", "fixture", "native", Some("old".into()), None);
            let mut i = Intent { job: job.id.clone(), machine: "fixture".into(), target: b.target.clone(), commit: commit.into(), by: "person".into(), run: Some("run".into()), binary: Some(b.clone()), release: None, sent: false, rollback: None, owner: None, activation: None };
            hub.updates.insert(job);
            let ready = build(&hub, &mut i).await?;
            assert_eq!(ready.sha256, b.sha256);
            let release = deployment(&hub, &i, &ready).await?;
            assert_eq!(release.state, State::Delivering);
            assert_eq!(release.provider.get("ci_run"), Some(&json!("run")));
            let mut wrong = ready.clone(); wrong.run = Some(run.number + 1);
            assert!(verify(&hub, &i, &wrong).await.is_err());
            hub.store.call(|c| { c.execute("UPDATE ci_attestations SET bundle='{}'", [])?; Ok(()) }).await?;
            assert!(verify(&hub, &i, &ready).await.is_err());
            Ok(())
        })
    }
}
