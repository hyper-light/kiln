//! A pool's executors on this runner (docs/ci.md W13-R09): started when the server asks, ready
//! before a leg comes, each taking one leg and then discarded, so every leg starts clean.
//!
//! A `docker` executor is a started container of the pool's image, laid out as the `docker`
//! executor lays out a leg's (W13-R04) under a directory of its own (`<work>/p/<id>`), so a leg
//! that comes to it runs there at once. A leg it can't take as it is (one with `container:`,
//! `services:` or a contained network) runs the usual way, and the executor is discarded all the
//! same. A `firecracker` executor is a microVM of the pool's image, booted and waiting for its
//! leg (W13-R05). `shards` is refused until this runner has it; `k8s`'s executors are the
//! server's own pods.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::docker::{self, Cli, Containers, PathMap, Spec};

/// What the server asks (`ServerMsg::Executor`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Ask {
    /// `start` or `stop`.
    pub op: String,
    pub executor: String,
    pub pool: String,
    pub kind: String,
    pub image: Option<String>,
    /// kiln's registry, and a token that pulls the image from it.
    pub registry: Option<String>,
    pub pull_token: Option<String>,
    /// U03: the scheduler's durable existing startup reservation, never a new host claim.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allocation: Option<String>,
    /// U01: the exact backend capacity persisted with this same outer startup grant.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shape: Option<kiln_ci::sched::pools::ExecutorShape>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub process_nonce: Option<String>,
}

impl Ask {
    pub(super) fn firecracker_shape(&self) -> anyhow::Result<&kiln_ci::sched::vm::FirecrackerShape> {
        let Some(kiln_ci::sched::pools::ExecutorShape::Firecracker(shape)) = &self.shape else {
            anyhow::bail!("U01: Firecracker startup needs its persisted outer grant and explicit overhead shape; update the pool resources/overhead and request a new executor");
        };
        shape.validate().map_err(anyhow::Error::msg)?;
        Ok(shape)
    }
}

/// Physical retirement and producer ordering are distinct authorities.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopProof {
    /// Earlier accepted starts quiesced in this FIFO and its exact Gate is stopped.
    OrderedBarrier,
    /// The existing Gate proves physical retirement; this does not retire a live producer.
    PhysicalRecovery,
}

/// What this runner says of one (`AgentMsg::Executor`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Said {
    pub executor: String,
    /// `ready`, `failed`, `stopping` (proof pending), or `stopped`.
    pub state: String,
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allocation: Option<String>,
    /// U03: the actual startup execution; Ready cannot invent its physical owner.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub physical_execution: Option<String>,
    /// U01: Ready reports the actual backend shape bound to the original physical owner.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shape: Option<kiln_ci::sched::pools::ExecutorShape>,
    /// The existing ordered command owner or durable physical recovery supplied this proof.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_proof: Option<StopProof>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub process_nonce: Option<String>,
}

/// One ready executor.
pub struct Warm {
    pub(crate) execution: Arc<super::processes::Execution>,
    generation: String,
    allocation: String,
    pub id: String,
    pub pool: String,
    pub image: String,
    /// Its directory: the leg's runner workspace when it runs here.
    pub dir: PathBuf,
    pub containers: Containers,
    /// The tools' directory it mounted, leased as a leg leases one (the leg takes it over).
    pub tools: Option<super::workspaces::ToolCache>,
}

/// This runner's ready executors.
#[derive(Clone)]
pub struct Executors { admission: super::processes::Admission, warm: Arc<parking_lot::Mutex<HashMap<String, Warm>>>, intents: Arc<parking_lot::Mutex<Intents>>, owners: Arc<parking_lot::Mutex<HashMap<String, Startup>>>, process_nonce: Option<Arc<str>> }

impl Default for Executors {
    fn default() -> Self { Self { admission: super::processes::Admission::production(), warm: Default::default(), intents: Default::default(), owners: Default::default(), process_nonce: None } }
}

struct Startup { generation: String, allocation: String, kind: String, image: Option<String>, shape: Option<kiln_ci::sched::pools::ExecutorShape>, ready: bool, physically_stopped: bool, stop_proven: bool, execution: Arc<super::processes::Execution> }

#[derive(Default)]
struct Intents { pending: VecDeque<Intent>, bytes: usize, running: bool }
struct Intent { work: PathBuf, ask: Ask, ordered: bool, bytes: usize, answer: futures::channel::oneshot::Sender<Option<Said>> }

impl Executors {
    pub fn for_process(nonce: String) -> Self { Self::for_process_in(nonce, super::processes::Admission::production()) }
    pub(crate) fn for_process_in(nonce: String, admission: super::processes::Admission) -> Self {
        Self { admission, process_nonce: Some(Arc::from(nonce)), ..Self::default() }
    }

    pub fn requires_order(&self, ask: &Ask) -> bool {
        ask.kind == "firecracker" || matches!(&ask.shape, Some(kiln_ci::sched::pools::ExecutorShape::Firecracker(_)))
            || self.owners.lock().get(&ask.executor).is_some_and(|owner| owner.kind == "firecracker")
    }

    /// Register in arrival order before the command is acknowledged. The one existing executor
    /// owner, rather than the scheduling order of per-request tasks, runs every admitted intent.
    pub fn enqueue(&self, work: &Path, ask: Ask, ordered: bool, bytes: usize) -> anyhow::Result<futures::channel::oneshot::Receiver<Option<Said>>> {
        anyhow::ensure!(matches!(ask.op.as_str(), "start" | "stop"), "executor operation must be start or stop");
        if self.requires_order(&ask) {
            anyhow::ensure!(ordered, "Firecracker requires a current, authenticated live command item");
            anyhow::ensure!(self.process_nonce.as_deref().is_some_and(|nonce| crate::proto::valid_process_nonce(nonce) && ask.process_nonce.as_deref() == Some(nonce)), "Firecracker request belongs to another or unverified agent process");
            startup_identity(&ask)?;
            if ask.op == "start" { ask.firecracker_shape()?; }
        }
        let (answer, result) = futures::channel::oneshot::channel();
        let launch = {
            let mut intents = self.intents.lock();
            let bytes = bytes.max(1);
            let held = intents.bytes.checked_add(bytes).filter(|held| *held <= crate::wire::commands::ROOM).ok_or_else(|| anyhow::anyhow!("executor intent queue exceeds the existing command byte allowance"))?;
            intents.pending.push_back(Intent { work: work.into(), ask, ordered, bytes, answer });
            intents.bytes = held;
            let launch = !intents.running;
            intents.running = true;
            launch
        };
        if launch {
            let execs = self.clone();
            crate::rt::spawn(async move {
                loop {
                    let intent = {
                        let mut intents = execs.intents.lock();
                        match intents.pending.pop_front() {
                            Some(intent) => { intents.bytes -= intent.bytes; intent }
                            None => { intents.running = false; return; }
                        }
                    };
                    let said = handle(&execs, &intent.work, intent.ask, intent.ordered).await;
                    let _ = intent.answer.send(said);
                }
            });
        }
        Ok(result)
    }

    /// The agent calls this only after its existing Amphora outbox accepted this exact receipt.
    /// Missing/replayed receipts and physical recovery cannot reclaim a live producer's owner.
    pub fn published(&self, said: &Said) {
        if said.state != "stopped" || said.stop_proof != Some(StopProof::OrderedBarrier) { return; }
        let mut owners = self.owners.lock();
        let remove = owners.get(&said.executor).is_some_and(|owner| owner.stop_proven
            && said.generation.as_ref() == Some(&owner.generation) && said.allocation.as_ref() == Some(&owner.allocation)
            && said.physical_execution.as_deref() == Some(owner.execution.identity())
            && (owner.kind != "firecracker" || said.process_nonce.as_deref() == self.process_nonce.as_deref()));
        if remove { owners.remove(&said.executor); }
    }

    /// A local canonical outbox receipt came from the existing Gate importer. Runtime/adopted
    /// work can retire outside this FIFO; remember that exact physical result without treating
    /// it as producer retirement or permission to reclaim this owner.
    pub fn recovered(&self, said: &Said) {
        if said.state != "stopped" || said.stop_proof != Some(StopProof::PhysicalRecovery) { return; }
        if let Some(owner) = self.owners.lock().get_mut(&said.executor)
            && said.generation.as_ref() == Some(&owner.generation) && said.allocation.as_ref() == Some(&owner.allocation)
            && said.physical_execution.as_deref() == Some(owner.execution.identity())
        { owner.physically_stopped = true; }
    }

    /// Reissue only an actual completed physical stop through the existing retained outbox.
    /// The server may use its exact durable receipt history solely to order cleanup Stop.
    pub fn closed_owner_hints(&self) -> Vec<Said> {
        self.owners.lock().iter().filter(|(_, owner)| owner.physically_stopped && owner.kind == "firecracker")
            .take(128).map(|(executor, owner)| Said { executor: executor.clone(), state: "stopped".into(),
                generation: Some(owner.generation.clone()), allocation: Some(owner.allocation.clone()),
                physical_execution: Some(owner.execution.identity().into()), process_nonce: self.process_nonce.as_deref().map(str::to_owned),
                ..Said::default() }).collect()
    }

    /// U03: a stale or incompatible assignment must leave the original warm executor owned.
    pub(crate) fn take_for(&self, a: &kiln_ci::wire::AssignmentV2) -> anyhow::Result<Option<Warm>> {
        let Some(slot) = a.executor.slot.as_deref() else { return Ok(None) };
        let mut warm = self.warm.lock();
        if let Some(w) = warm.get(slot) {
            anyhow::ensure!(adoptable(a, w), "U03: the assignment cannot adopt this exact reserved Docker executor");
        }
        Ok(warm.remove(slot))
    }

    pub fn take(&self, id: &str) -> Option<Warm> {
        self.warm.lock().remove(id)
    }

    /// U03: restore a taken executor after binding failed; never replace another owner.
    /// Stale or incompatible assignments should use take_for, which leaves it in place.
    pub(crate) fn restore(&self, w: Warm) -> anyhow::Result<()> {
        let mut warm = self.warm.lock();
        anyhow::ensure!(!warm.contains_key(&w.id), "U03: restoring a warm executor would replace its existing owner");
        warm.insert(w.id.clone(), w);
        Ok(())
    }

    fn put(&self, w: Warm) {
        self.warm.lock().insert(w.id.clone(), w);
    }

}

/// Removes what executors of an earlier process left (`<work>/p`): this one has none yet.
pub fn sweep(work: &Path) {
    let _ = std::fs::remove_dir_all(work.join("p"));
}

/// Where a leg runs in executor `id`: `<work>/p/<id>`.
pub fn dir_of(work: &Path, id: &str) -> PathBuf {
    work.join("p").join(id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect::<String>())
}

/// The tools' directory of the `docker` executor's image, as a leg's layout has it.
pub fn tool_cache(work: &Path, image: &str) -> PathBuf {
    work.join("_tool").join(image.chars().map(|c| if c.is_ascii_alphanumeric() || c == '.' { c } else { '-' }).collect::<String>())
}

/// Retention may deliberately outlive stop(): only the existing exact Gate's durable stopped
/// phase proves all groups and future debug admission ended. Missing evidence remains unknown.
async fn stopped_reply(execs: &Executors, ask: &Ask, execution: &Arc<super::processes::Execution>, error: Option<String>, barrier: bool) -> Said {
    let proof = execution.stopped_proven().await;
    let (stopped, pending) = match proof {
        Ok(true) => (true, None),
        Ok(false) => (false, Some("U03: this physical owner is still retained or awaiting group stop proof; its grant remains fenced until exact release".to_owned())),
        Err(error) => (false, Some(format!("U03: physical stop evidence is unknown; retain the grant and reconcile the exact execution: {error:#}"))),
    };
    if stopped && let Some(owner) = execs.owners.lock().get_mut(&ask.executor)
        && owner.execution.identity() == execution.identity()
    { owner.physically_stopped = true; if barrier { owner.stop_proven = true; } }
    let error = match (error, pending) { (Some(error), Some(pending)) => Some(format!("{error}; {pending}")), (error, None) => error, (None, pending) => pending };
    Said { stop_proof: if stopped && barrier { Some(StopProof::OrderedBarrier) } else { None }, physical_execution: Some(execution.identity().into()),
        ..reply(ask, if stopped { "stopped" } else { "stopping" }, error) }
}

/// Runs one synchronously admitted intent inside this executor owner's FIFO.
async fn handle(execs: &Executors, work: &Path, ask: Ask, ordered: bool) -> Option<Said> {
    // Recheck against the actual owner: an earlier queued Start may have established it after
    // this intent was registered, including when a legacy envelope omitted its backend kind.
    if execs.requires_order(&ask) && (!ordered || execs.process_nonce.as_deref().is_none_or(|nonce| !crate::proto::valid_process_nonce(nonce) || ask.process_nonce.as_deref() != Some(nonce))) {
        return Some(reply(&ask, "failed", Some("Firecracker operation has no current ordered process owner".into())));
    }
    if ask.op == "start" {
        if ask.kind == "firecracker" && let Err(error) = ask.firecracker_shape() {
            return Some(reply(&ask, "failed", Some(format!("{error:#}"))));
        }
        let owner = execs.owners.lock().get(&ask.executor).map(|owner| (owner.generation.clone(), owner.allocation.clone(), owner.kind.clone(), owner.image.clone(), owner.shape.clone(), owner.ready, Arc::clone(&owner.execution)));
        if let Some((generation, allocation, kind, image, shape, ready, execution)) = owner {
            if ask.generation.as_ref() != Some(&generation) || ask.allocation.as_ref() != Some(&allocation) || ask.kind != kind || ask.image != image || ask.shape != shape {
                return Some(reply(&ask, "failed", Some("U03: executor startup changed its existing grant".into())));
            }
            // The warm object may already have been adopted. Keep its original owner indexed
            // so a replay cannot boot a second physical executor under the same reservation.
            if !execution.accepts_work() {
                execution.stop().await;
                let mut outcome = stopped_reply(execs, &ask, &execution, None, false).await;
                // Replay can be the server's first observed failure. Persist its Stop intent
                // before a later actual Stop reports proof pending, including retained roots.
                if outcome.state == "stopping" { outcome.state = "failed".into(); }
                return Some(outcome);
            }
            if !ready {
                return Some(reply(&ask, "failed", Some("U01: this startup has no verified native Ready outcome; stop/reconcile its original physical owner before replacement".into())));
            }
            return Some(Said { shape, physical_execution: Some(execution.identity().into()), ..reply(&ask, "ready", None) });
        }
        if execs.owners.lock().len() >= 4096 {
            return Some(reply(&ask, "failed", Some("U03: the concurrent executor ownership limit is full; finish existing executors before startup".into())));
        }
        let (Some(generation), Some(allocation)) = (ask.generation.clone(), ask.allocation.clone()) else {
            return Some(reply(&ask, "failed", Some("U03: executor startup requires its existing allocation and generation".into())));
        };
        let execution = match super::processes::Execution::allocate_executor(execs.admission.clone(), work.to_path_buf(), ask.executor.clone(), generation, allocation) {
            Ok(execution) => execution,
            Err(error) => return Some(reply(&ask, "failed", Some(format!("U03: executor identity refused: {error:#}")))),
        };
        // ctl-9: preserve this SAME immutable owner before host/Gate I/O can fail or have an
        // unknown outcome. No backend may run until its preparation positively succeeds.
        execs.owners.lock().insert(ask.executor.clone(), Startup { generation: ask.generation.clone().unwrap_or_default(), allocation: ask.allocation.clone().unwrap_or_default(), kind: ask.kind.clone(), image: ask.image.clone(), shape: ask.shape.clone(), ready: false, physically_stopped: false, stop_proven: false, execution: Arc::clone(&execution) });
        let preparing = Arc::clone(&execution);
        if let Err(error) = crate::rt::blocking(move || preparing.prepare_executor_root()).await.flatten() {
            execution.stop().await;
            let mut outcome = stopped_reply(execs, &ask, &execution, Some(format!("U03: physical executor preparation refused: {error:#}")), false).await;
            if outcome.state == "stopping" { outcome.state = "failed".into(); }
            return Some(outcome);
        }
        let bound = if ask.kind == "firecracker" {
            match ask.firecracker_shape() {
                Ok(shape) => {
                    let (execution, shape) = (Arc::clone(&execution), shape.clone());
                    crate::rt::blocking(move || execution.bind_vm_shape(shape)).await.flatten()
                }
                Err(error) => Err(error),
            }
        } else { Ok(()) };
        if let Err(error) = bound {
            execution.stop().await;
            let mut outcome = stopped_reply(execs, &ask, &execution, Some(format!("U01: {error:#}")), false).await;
            if outcome.state == "stopping" { outcome.state = "failed".into(); }
            return Some(outcome);
        }
        let asked = ask.clone();
        let ask_executor = ask.executor.clone();
        let expected_shape = ask.shape.clone();
        let shaped = ask.kind == "firecracker";
        let mut said = execution.scope(handle_owned(execs, work, ask)).await;
        if shaped && let Some(response) = &mut said
            && response.state == "ready" && response.shape != expected_shape {
            response.state = "failed".into();
            response.error = Some("U01: Firecracker Ready has no matching original physical shape".into());
        }
        if said.as_ref().is_some_and(|said| said.state == "ready")
            && let Some(owner) = execs.owners.lock().get_mut(&ask_executor) { owner.ready = true; }
        if said.as_ref().is_none_or(|said| said.state != "ready") {
            execution.stop().await;
            // Failure is not an ordering barrier: a replay with another sequence may already
            // be in flight. Keep the closed owner until an ordered Stop is durably reported.
            let error = said.and_then(|said| said.error);
            let mut outcome = stopped_reply(execs, &asked, &execution, error, false).await;
            // A first startup failure must durably enter the server's Stop phase even when
            // its physical evidence is still unknown. It supplies no stopped certificate.
            if outcome.state == "stopping" { outcome.state = "failed".into(); }
            said = Some(outcome);
        }
        return said;
    }
    if ask.op == "stop" {
        let owner = execs.owners.lock().get(&ask.executor).map(|owner| (owner.generation.clone(), owner.allocation.clone(), Arc::clone(&owner.execution)));
        if let Some((generation, allocation, execution)) = owner {
            if ask.generation.as_ref() != Some(&generation) || ask.allocation.as_ref() != Some(&allocation) {
                return Some(reply(&ask, "failed", Some("U03: stale executor stop grant".into())));
            }
            let result = handle_owned(execs, work, ask.clone()).await;
            execution.stop().await;
            let said = stopped_reply(execs, &ask, &execution, result.and_then(|said| said.error), ordered).await;
            if !ordered && said.state == "stopped" {
                // Preserve legacy unsequenced non-VM behavior only after actual physical proof.
                execs.owners.lock().remove(&ask.executor);
            }
            return Some(said);
        }
    }
    handle_owned(execs, work, ask).await
}

async fn handle_owned(execs: &Executors, work: &Path, ask: Ask) -> Option<Said> {
    // U03: warm startup precedes Leg::run. It must already be scoped to the scheduler's
    // persisted existing grant; never fall through to an unowned raw host spawn.
    if ask.op == "start" && let Err(e) = startup_owned(&ask) {
        return Some(reply(&ask, "failed", Some(format!("{e:#}"))));
    }
    match ask.op.as_str() {
        "stop" => {
            if let Some(w) = execs.take(&ask.executor) {
                if ask.generation.as_deref() != Some(w.generation.as_str()) || ask.allocation.as_deref() != Some(w.allocation.as_str()) {
                    execs.put(w);
                    return Some(reply(&ask, "failed", Some("U03: stale executor stop grant".into())));
                }
                let physical = w.execution.identity().to_owned();
                discard(w).await;
                return Some(Said { physical_execution: Some(physical), ..reply(&ask, "stopped", None) });
            }
            #[cfg(target_os = "linux")]
            match super::firecracker::discard_warm(&ask).await {
                Ok(Some(physical)) => return Some(Said { physical_execution: Some(physical), ..reply(&ask, "stopped", None) }),
                Err(e) => return Some(reply(&ask, "failed", Some(format!("{e:#}")))),
                Ok(None) => {},
            }
            None
        }
        "start" if ask.kind == "firecracker" => Some(start_microvm(work, &ask).await),
        "start" => Some(match start(work, &ask).await {
            Ok(w) => {
                let said = reply(&ask, "ready", None);
                execs.put(w);
                said
            }
            Err(e) => reply(&ask, "failed", Some(format!("{e:#}"))),
        }),
        other => Some(reply(&ask, "failed", Some(format!("`{other}`: start or stop")))),
    }
}

fn startup_identity(ask: &Ask) -> anyhow::Result<()> {
    for value in [Some(ask.executor.as_str()), ask.generation.as_deref(), ask.allocation.as_deref()] {
        let value = value.ok_or_else(|| anyhow::anyhow!("U03: executor startup requires its existing allocation and generation"))?;
        anyhow::ensure!(!value.is_empty() && value.len() <= 512 && !value.chars().any(char::is_control), "U03: invalid executor startup ownership");
    }
    Ok(())
}

fn startup_owned(ask: &Ask) -> anyhow::Result<()> {
    startup_identity(ask)?;
    let execution = super::processes::current().ok_or_else(|| anyhow::anyhow!("U03: executor startup has no physical execution under its existing grant"))?;
    anyhow::ensure!(execution.accepts_work(), "U03: executor startup has been fenced");
    Ok(())
}

/// A `firecracker` pool's executor: a microVM booted and waiting for its leg.
async fn start_microvm(work: &Path, ask: &Ask) -> Said {
    #[cfg(target_os = "linux")]
    let started = super::firecracker::start_warm(work, ask).await;
    #[cfg(not(target_os = "linux"))]
    let started: anyhow::Result<()> = {
        let _ = work;
        Err(anyhow::anyhow!("microVMs run on Linux hosts with KVM"))
    };
    match started {
        Ok(()) => reply(ask, "ready", None),
        Err(e) => reply(ask, "failed", Some(format!("{e:#}"))),
    }
}

fn reply(ask: &Ask, state: &str, error: Option<String>) -> Said {
    Said { executor: ask.executor.clone(), state: state.into(), error, generation: ask.generation.clone(), allocation: ask.allocation.clone(), process_nonce: ask.process_nonce.clone(), physical_execution: (state == "ready").then(super::processes::current).flatten().map(|execution| execution.identity().into()),
        shape: if state == "ready" && ask.kind == "firecracker" { super::processes::current().and_then(|execution| execution.vm_shape().ok()).map(kiln_ci::sched::pools::ExecutorShape::Firecracker) } else { None }, stop_proof: None }
}

async fn start(work: &Path, ask: &Ask) -> anyhow::Result<Warm> {
    anyhow::ensure!(ask.kind == "docker", "this runner starts `docker` and `firecracker` pools' executors; `{}` comes with its executor (docs/ci.md W13-R06, R07)", ask.kind);
    let image = ask.image.clone().filter(|i| !i.trim().is_empty()).ok_or_else(|| anyhow::anyhow!("the pool names no image"))?;
    let bin = crate::rt::blocking(docker::find).await?.ok_or_else(|| anyhow::anyhow!("this runner has no working Docker or Podman"))?;
    let dir = dir_of(work, &ask.executor);
    let temp = dir.join("_temp");
    let config = temp.join("_docker_config");
    // Leased as a leg of the `docker` executor leases its image's tools, which the leg takes over.
    let lease = super::workspaces::ToolCache::take(&tool_cache(work, &image));
    let tools = lease.path();
    let dirs = [config.clone(), temp.join("_github_home"), temp.join("_github_workflow"), work.join("_actions"), tools.clone(), work.join("_externals")];
    crate::rt::blocking(move || dirs.iter().try_for_each(std::fs::create_dir_all)).await??;
    // Pulls as the runner's own Docker would, and from kiln's registry with the token the server
    // gave for this image.
    let mut conf = super::credential::runner_config().unwrap_or_else(|| serde_json::json!({}));
    if let (Some(host), Some(token), Some(o)) = (ask.registry.as_deref().map(crate::githost::host_of), ask.pull_token.as_deref(), conf.as_object_mut()) {
        let auths = o.entry("auths").or_insert_with(|| serde_json::json!({}));
        if let Some(a) = auths.as_object_mut() {
            a.insert(host, serde_json::json!({ "registrytoken": token }));
        }
    }
    super::auth::write_private(&config.join("config.json"), conf.to_string().as_bytes())?;
    // The paths a leg's layout will have, at the same paths inside (the `docker` executor's).
    let same = |p: &Path| p.to_string_lossy().into_owned();
    let map = PathMap::new(vec![
        (dir.clone(), same(&dir)),
        (temp.join("_github_home"), "/github/home".into()),
        (temp.join("_github_workflow"), "/github/workflow".into()),
        (work.join("_actions"), same(&work.join("_actions"))),
        (tools, "/opt/hostedtoolcache".into()),
        (work.join("_externals"), "/__e".into()),
    ]);
    let cli = Cli::new(&bin, config).owned_by(work);
    let spec = Spec { image: image.clone(), user: docker::host_user(), seed_tools: true, ..Spec::default() };
    let said = parking_lot::Mutex::new(Vec::<String>::new());
    let containers = docker::Containers::start(cli, &ask.executor, map, &same(&dir), Some(spec), Vec::new(), None, &|m: String| said.lock().push(m)).await?;
    tracing::info!(executor = %ask.executor, pool = %ask.pool, image = %image, "pool executor ready");
    let execution = super::processes::current().ok_or_else(|| anyhow::anyhow!("U03: executor startup lost its physical ownership"))?;
    Ok(Warm { execution, generation: ask.generation.clone().ok_or_else(|| anyhow::anyhow!("U03: executor generation missing"))?, allocation: ask.allocation.clone().ok_or_else(|| anyhow::anyhow!("U03: executor allocation missing"))?, id: ask.executor.clone(), pool: ask.pool.clone(), image, dir, containers, tools: Some(lease) })
}

/// Removes an executor: its containers and its directory.
pub async fn discard(w: Warm) {
    w.containers.stop().await;
    w.execution.stop().await;
    let dir = w.dir.clone();
    let _ = crate::rt::blocking(move || std::fs::remove_dir_all(dir)).await;
}

/// Whether a leg can run in a ready executor as it is: the `docker` executor's own container,
/// of the pool's image, with nothing the executor wasn't started with.
pub fn adoptable(a: &kiln_ci::wire::AssignmentV2, w: &Warm) -> bool {
    a.executor.kind == "docker" && a.executor.image.as_deref() == Some(w.image.as_str()) && a.job_container.is_none() && a.job_services.is_empty() && a.network.is_none()
        && w.execution.accepts_work() && assignment_binding(a, &w.id, &w.generation, &w.allocation, w.execution.identity()).is_ok()
}

/// U03: select this exact startup Arc before Running and its cancellation timers exist.
/// Binding is persisted off-shard before the assignment can launch any work.
pub(crate) async fn adopted_execution(a: &kiln_ci::wire::AssignmentV2, w: &Warm) -> anyhow::Result<Arc<super::processes::Execution>> {
    anyhow::ensure!(adoptable(a, w), "U03: the reserved Docker executor cannot run this assignment as created");
    let parent = assignment_binding(a, &w.id, &w.generation, &w.allocation, w.execution.identity())?;
    let execution = Arc::clone(&w.execution);
    crate::rt::blocking(move || { execution.bind_parent(parent)?; Ok(execution) }).await?
}

pub(super) fn assignment_binding(a: &kiln_ci::wire::AssignmentV2, executor: &str, generation: &str, allocation: &str, physical: &str) -> anyhow::Result<kiln_ci::wire::ParentBinding> {
    let parent = a.parent_binding.as_ref().ok_or_else(|| anyhow::anyhow!("U03: assignment has no existing executor ownership"))?;
    anyhow::ensure!(a.executor.slot.as_deref() == Some(executor) && parent.executor == executor && parent.generation == generation && parent.allocation == allocation && parent.physical_execution == physical && parent.job == a.job_id && parent.attempt == a.attempt, "U03: assignment does not own this exact executor generation and job attempt");
    super::processes::ParentExecution::verified(parent.clone(), &a.job_id, a.attempt)?;
    Ok(parent.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn u03_adoption_refuses_stale_grant_physical_owner_and_attempt() -> anyhow::Result<()> {
        let parent = kiln_ci::wire::ParentBinding { executor: "executor-one".into(), generation: "generation-one".into(), allocation: "grant-one".into(), physical_execution: "physical-one".into(), job: "job-one".into(), attempt: 2 };
        let a = kiln_ci::wire::AssignmentV2 { job_id: parent.job.clone(), attempt: parent.attempt, parent_binding: Some(parent.clone()), executor: kiln_ci::wire::ExecutorSpec { slot: Some(parent.executor.clone()), ..Default::default() }, ..Default::default() };
        assert_eq!(assignment_binding(&a, &parent.executor, &parent.generation, &parent.allocation, &parent.physical_execution)?, parent);
        for (executor, generation, allocation, physical) in [("other", "generation-one", "grant-one", "physical-one"), ("executor-one", "old", "grant-one", "physical-one"), ("executor-one", "generation-one", "old", "physical-one"), ("executor-one", "generation-one", "grant-one", "old")] {
            assert!(assignment_binding(&a, executor, generation, allocation, physical).is_err());
        }
        let stale = kiln_ci::wire::AssignmentV2 { attempt: 3, ..a };
        assert!(assignment_binding(&stale, &parent.executor, &parent.generation, &parent.allocation, &parent.physical_execution).is_err());
        Ok(())
    }

    #[test]
    fn u03_replayed_start_keeps_adopted_executor_owner_and_refuses_changed_grant() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let nonce = "a".repeat(43);
            let admission = super::super::processes::Admission::private(&tempfile::tempdir()?.keep())?;
            let executors = Executors::for_process_in(nonce.clone(), admission.clone());
            let work = tempfile::tempdir()?;
            let execution = super::super::processes::Execution::allocate_executor(admission.clone(), work.path().into(), "executor-one".into(), "generation-one".into(), "existing-grant".into())?;
            execution.prepare_executor_root()?;
            let outer = kiln_ci::sched::state::Resources { cpu: 3.5, mem: 512 << 20, disk: 1 << 30, gpu: 0 };
            let overhead = kiln_ci::sched::state::Resources { cpu: 0.25, mem: 128 << 20, disk: 64 << 20, gpu: 0 };
            let shape = kiln_ci::sched::pools::ExecutorShape::Firecracker(kiln_ci::sched::vm::FirecrackerShape::derive(outer, overhead).map_err(anyhow::Error::msg)?);
            let ask = Ask { op: "start".into(), executor: "executor-one".into(), kind: "firecracker".into(), process_nonce: Some(nonce.clone()), image: Some("image-one".into()), shape: Some(shape.clone()), generation: Some("generation-one".into()), allocation: Some("existing-grant".into()), ..Default::default() };
            let queue = |ask: Ask| {
                let bytes = serde_json::to_vec(&ask)?.len();
                executors.enqueue(Path::new("."), ask, true, bytes)
            };
            executors.owners.lock().insert(ask.executor.clone(), Startup { generation: "generation-one".into(), allocation: "existing-grant".into(), kind: ask.kind.clone(), image: ask.image.clone(), shape: Some(shape.clone()), ready: true, physically_stopped: false, stop_proven: false, execution: Arc::clone(&execution) });
            // The warm object was already adopted; only the original physical owner remains.
            let first = queue(ask.clone())?.await?.ok_or_else(|| anyhow::anyhow!("missing startup response"))?;
            let replay = queue(ask.clone())?.await?.ok_or_else(|| anyhow::anyhow!("missing replay response"))?;
            assert_eq!(first, replay);
            assert_eq!(first.state, "ready");
            assert_eq!(first.physical_execution.as_deref(), Some(execution.identity()));
            assert_eq!(first.shape, ask.shape, "Ready preserves the actual startup shape after adoption");
            let changed_shape = kiln_ci::sched::pools::ExecutorShape::Firecracker(kiln_ci::sched::vm::FirecrackerShape::derive(kiln_ci::sched::state::Resources { cpu: 4.5, ..outer }, overhead).map_err(anyhow::Error::msg)?);
            for changed in [Ask { generation: Some("generation-two".into()), ..ask.clone() }, Ask { process_nonce: Some("b".repeat(43)), ..ask.clone() }, Ask { process_nonce: None, ..ask.clone() }, Ask { shape: Some(changed_shape), ..ask.clone() }, Ask { shape: None, ..ask.clone() }, Ask { image: Some("image-two".into()), ..ask.clone() }] {
                // Some malformed requests are refused synchronously, before command ACK or I/O.
                if let Ok(response) = queue(changed) {
                    assert_eq!(response.await?.ok_or_else(|| anyhow::anyhow!("missing refusal"))?.state, "failed");
                }
                assert_eq!(executors.owners.lock().len(), 1, "a changed/missing shape never boots a replacement under the existing owner");
            }
            assert!(executors.enqueue(Path::new("."), ask.clone(), false, 1).is_err(), "unsequenced VM startup is refused before I/O");
            assert!(executors.enqueue(Path::new("."), ask.clone(), true, crate::wire::commands::ROOM + 1).is_err());
            if let Some(owner) = executors.owners.lock().get_mut(&ask.executor) { owner.ready = false; }
            let uncertain = queue(ask.clone())?.await?.ok_or_else(|| anyhow::anyhow!("missing pending outcome"))?;
            assert_eq!(uncertain.state, "failed", "accepted startup intent alone never proves native Ready");
            assert!(executors.closed_owner_hints().is_empty(), "unobserved Ready is not physical stop proof");
            let retained = execution.retain()?;
            execution.fence();
            assert!(executors.closed_owner_hints().is_empty(), "fencing alone cannot mint a stopped hint");
            let pending_start = queue(ask.clone())?.await?.ok_or_else(|| anyhow::anyhow!("missing retained replay outcome"))?;
            assert_eq!(pending_start.state, "failed", "a pending Start replay still drives the server's durable Stop phase");
            assert_eq!(pending_start.stop_proof, None);
            let pending = queue(Ask { op: "stop".into(), image: None, shape: None, ..ask.clone() })?.await?.ok_or_else(|| anyhow::anyhow!("missing retained stop outcome"))?;
            assert_eq!(pending.state, "stopping", "retained debug admission outlives ordinary stop completion");
            assert_eq!(pending.stop_proof, None, "no ordered physical barrier exists before the exact retained Gate releases");
            assert!(executors.closed_owner_hints().is_empty());
            executors.published(&pending);
            assert_eq!(executors.owners.lock().len(), 1);
            retained.release().await;
            assert!(executors.closed_owner_hints().is_empty(), "external retained release is observed through its canonical proof receipt");
            let recovery = super::super::processes::recovery_evidence(&admission)?.into_iter().find(|proof| proof.execution == execution.identity()).ok_or_else(|| anyhow::anyhow!("missing externally retired Gate proof"))?;
            let recovered = Said { executor: ask.executor.clone(), state: "stopped".into(), generation: ask.generation.clone(), allocation: ask.allocation.clone(), physical_execution: Some(recovery.execution), stop_proof: Some(StopProof::PhysicalRecovery), ..Default::default() };
            executors.recovered(&Said { physical_execution: Some("another-execution".into()), ..recovered.clone() });
            assert!(executors.closed_owner_hints().is_empty());
            executors.recovered(&recovered);
            assert_eq!(executors.closed_owner_hints().len(), 1);
            assert_eq!(executors.owners.lock().len(), 1, "physical recovery preserves the original producer's ordering owner");
            let stopped = queue(ask.clone())?.await?.ok_or_else(|| anyhow::anyhow!("missing stopped replay"))?;
            assert_eq!(stopped.state, "stopped");
            assert_eq!(stopped.physical_execution.as_deref(), Some(execution.identity()));
            let stopped_again = queue(ask.clone())?.await?.ok_or_else(|| anyhow::anyhow!("missing stopped replay"))?;
            assert_eq!(stopped, stopped_again, "a known stopped owner never authorizes another boot");
            assert_eq!(executors.closed_owner_hints(), vec![stopped.clone()]);
            executors.published(&stopped);
            executors.published(&Said { stop_proof: Some(StopProof::PhysicalRecovery), ..stopped.clone() });
            assert_eq!(executors.owners.lock().len(), 1, "spontaneous/recovery publication cannot reclaim a live producer's ordering owner");
            let stop = Ask { op: "stop".into(), image: None, shape: None, ..ask.clone() };
            assert!(executors.enqueue(Path::new("."), stop.clone(), false, 1).is_err(), "VM stop requires the same ordered owner");
            let barrier = queue(stop)?.await?.ok_or_else(|| anyhow::anyhow!("missing stop barrier"))?;
            assert_eq!(barrier.stop_proof, Some(StopProof::OrderedBarrier));
            assert_eq!(executors.owners.lock().len(), 1, "physical stop precedes durable outbox publication");
            executors.published(&barrier);
            assert!(executors.owners.lock().is_empty());
            let physical = execution.identity().to_owned();
            drop(execution); // The exact Gate permit is retained until the Startup/other owner drops.
            let proof = super::super::processes::recovery_evidence(&admission)?.into_iter().find(|proof| proof.execution == physical).ok_or_else(|| anyhow::anyhow!("stop barrier lacks exact durable physical proof"))?;
            super::super::processes::acknowledge_evidence(&admission, &proof)?;

            let mut receiver = crate::wire::channel::Receiver::new();
            let mut current = crate::wire::commands::Current::new();
            let stream = "l:1.1";
            assert!(matches!(current.of(stream), crate::wire::commands::Stand::Newer(_)));
            let mut seq = 0;
            // Repeated logical lifecycles use the actual FIFO and existing channel dedupe.
            // There is no process-lifetime stopped-map growth and no timing/sleep assumption.
            for cycle in 0..4 {
                let id = format!("executor-cycle-{cycle}");
                let execution = super::super::processes::Execution::allocate_executor(admission.clone(), work.path().into(), id.clone(), "generation-one".into(), "existing-grant".into())?;
                execution.prepare_executor_root()?;
                let physical = execution.identity().to_owned();
                let start = Ask { executor: id.clone(), ..ask.clone() };
                executors.owners.lock().insert(id, Startup { generation: start.generation.clone().unwrap_or_default(), allocation: start.allocation.clone().unwrap_or_default(), kind: start.kind.clone(), image: start.image.clone(), shape: start.shape.clone(), ready: true, physically_stopped: false, stop_proven: false, execution });
                let start_body = serde_json::to_vec(&start)?;
                seq += 1;
                let start_seq = seq;
                assert!(matches!(receiver.on_item(stream, seq, seq - 1, start_body.clone()), crate::wire::channel::Arrival::Take(_)));
                let ready = queue(start.clone())?;
                receiver.took(stream, seq); // ACK only after synchronous queue registration.
                let stop = Ask { op: "stop".into(), image: None, shape: None, ..start };
                seq += 1;
                assert!(matches!(receiver.on_item(stream, seq, seq - 1, serde_json::to_vec(&stop)?), crate::wire::channel::Arrival::Take(_)));
                let stopped = queue(stop)?;
                receiver.took(stream, seq);
                // Both requests were accepted before either operation's completion was awaited.
                assert_eq!(ready.await?.ok_or_else(|| anyhow::anyhow!("missing FIFO Ready"))?.state, "ready");
                let barrier = stopped.await?.ok_or_else(|| anyhow::anyhow!("missing FIFO stop"))?;
                assert_eq!(barrier.stop_proof, Some(StopProof::OrderedBarrier));
                executors.published(&barrier);
                assert!(executors.owners.lock().is_empty());
                let proof = super::super::processes::recovery_evidence(&admission)?.into_iter().find(|proof| proof.execution == physical).ok_or_else(|| anyhow::anyhow!("FIFO barrier lacks exact durable physical proof"))?;
                super::super::processes::acknowledge_evidence(&admission, &proof)?;
                assert!(matches!(current.of(stream), crate::wire::commands::Stand::Current));
                assert!(matches!(receiver.on_item(stream, start_seq, start_seq - 1, start_body), crate::wire::channel::Arrival::Answered(_)), "delayed original Start cannot reenter the owner after its stop barrier");
            }
            assert!(matches!(current.of("l:1.2"), crate::wire::commands::Stand::Newer(_)));
            assert!(matches!(current.of(stream), crate::wire::commands::Stand::Gone));

            // ctl-9: a real deterministic Gate-directory refusal occurs after logical
            // registration, before any backend. Neither failed preparation nor Stop can
            // fabricate positive physical evidence from an absent/unreadable Gate.
            let refused_domain = super::super::processes::Admission::private(&tempfile::tempdir()?.keep())?;
            std::fs::create_dir_all(refused_domain.ci())?;
            let blocker = refused_domain.ci().join("executions");
            std::fs::write(&blocker, b"owned non-directory fixture")?;
            let refused_executors = Executors::for_process_in(nonce, refused_domain.clone());
            let refused_work = refused_domain.root().join("work");
            let refused_ask = Ask { executor: "prepare-refused".into(), ..ask.clone() };
            let request = |ask: Ask| {
                let bytes = serde_json::to_vec(&ask)?.len();
                refused_executors.enqueue(&refused_work, ask, true, bytes)
            };
            let failed = request(refused_ask.clone())?.await?.ok_or_else(|| anyhow::anyhow!("preparation refusal lost its response"))?;
            assert_eq!(failed.state, "failed");
            assert_eq!(failed.stop_proof, None);
            assert!(failed.error.as_deref().is_some_and(|error| error.contains("preparation refused")));
            let physical = failed.physical_execution.clone().ok_or_else(|| anyhow::anyhow!("preparation refusal lost its original physical identity"))?;
            let original = refused_executors.owners.lock().get(&refused_ask.executor).map(|owner| Arc::clone(&owner.execution)).ok_or_else(|| anyhow::anyhow!("preparation refusal lost its Startup owner"))?;
            assert_eq!(original.identity(), physical);
            assert_eq!(original.admission(), &refused_domain);
            assert!(!original.accepts_work());
            assert!(!refused_work.exists(), "a failed preparation cannot enter backend I/O");
            assert!(original.prepare_executor_root().is_err(), "closed ownership cannot be reopened by retry");
            let pending = request(Ask { op: "stop".into(), ..refused_ask.clone() })?.await?.ok_or_else(|| anyhow::anyhow!("exact Stop lost its failed startup owner"))?;
            assert_eq!(pending.state, "stopping");
            assert_eq!(pending.stop_proof, None);
            assert_eq!(pending.physical_execution.as_deref(), Some(physical.as_str()));
            refused_executors.published(&pending);
            assert_eq!(refused_executors.owners.lock().len(), 1, "unknown proof retains the same owner");
            std::fs::remove_file(blocker)?; // Remove only the owned refusal fixture, never Gate evidence.
            let replay = request(refused_ask.clone())?.await?.ok_or_else(|| anyhow::anyhow!("closed replay lost its response"))?;
            assert_eq!(replay.state, "failed");
            assert_eq!(replay.physical_execution.as_deref(), Some(physical.as_str()));
            assert_eq!(replay.stop_proof, None);
            let changed = request(Ask { allocation: Some("foreign-grant".into()), ..refused_ask })?.await?.ok_or_else(|| anyhow::anyhow!("changed grant lost its refusal"))?;
            assert_eq!(changed.state, "failed");
            assert_eq!(refused_executors.owners.lock().len(), 1);
            assert!(!refused_work.exists());
            assert!(super::super::processes::recovery_evidence(&refused_domain)?.is_empty(), "missing Gate never supplies a release certificate");
            Ok(())
        })
    }
}
