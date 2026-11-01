//! Kubernetes executor. A queued `runs-on: [k8s]` job gets its own pod:
//!
//! * an initContainer from the kiln image copies the static `kiln` binary into an `emptyDir`,
//! * the job container (any image: the toolchain preset or `image:`) runs
//!   `kiln agent --ephemeral --job <id>`, which enrolls with a one-time token, connects back over
//!   QUIC/mTLS exactly like a bare-metal agent, runs the one job and exits,
//! * a reaper deletes finished pods and fails jobs whose pods cannot start (bad image, quota).
//!
//! The pod needs no git and no GitHub access: source comes from the server's mirror as a tarball.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use k8s_openapi::api::core::v1::{
    Affinity, Container, ContainerPort, EnvVar, ExecAction, HostAlias, HostPathVolumeSource, NodeAffinity, NodeSelector, NodeSelectorRequirement, NodeSelectorTerm, PersistentVolumeClaimVolumeSource,
    Pod, PodSpec, PreferredSchedulingTerm, Probe, ResourceRequirements, SecurityContext, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use crate::kubeapi::KubeApi;

use crate::config::Kubernetes as KCfg;
use crate::hub::Hub;
use crate::store::{Job, Run, Status};

pub const MANAGED: &str = "app.kubernetes.io/managed-by=kiln";
const JOB_LABEL: &str = "kiln.dev/job";
/// A `k8s` pool's executor pods (docs/ci.md W13-R07): their label, and their executor's id.
pub const EXECUTOR_PODS: &str = "app.kubernetes.io/managed-by=kiln,app.kubernetes.io/name=kiln-executor";
const EXECUTOR_ANNOTATION: &str = "kiln.dev/executor";
const EXECUTOR_GENERATION: &str = "kiln.dev/executor-generation";
const EXECUTOR_ALLOCATION: &str = "kiln.dev/executor-allocation";

/// U03: captured from the authenticated API, then bound to the executor's enrollment and
/// existing allocation. A pod name alone can be reused after force deletion.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ExecutorIdentity {
    pub namespace: String,
    pub name: String,
    pub uid: String,
    pub node: String,
    pub executor: String,
    pub generation: String,
    pub allocation: String,
}

/// Capacity is separate from ExecutorIdentity: adding it to that JSON would change the
/// physical execution hash used by existing stop and retained-ownership proofs.
pub(crate) struct ExecutorBootstrap {
    pub identity: ExecutorIdentity,
    pub capacity: kiln_ci::sched::pools::PodCapacity,
}

/// Parse the API's quantities once. Missing values stay unknown; malformed or nonfinite
/// values are refused rather than turning into zero or an unlimited allowance.
fn pod_quantity(value: Option<&Quantity>) -> anyhow::Result<Option<f64>> {
    value.map(|q| {
        let n = kiln_cd::diff::quantity(&serde_json::Value::String(q.0.clone())).ok_or_else(|| anyhow::anyhow!("U01 invalid pod resource quantity"))?;
        anyhow::ensure!(n.is_finite() && n >= 0.0, "U01 invalid pod resource quantity");
        Ok(n)
    }).transpose()
}

fn pod_bytes(value: Option<&Quantity>) -> anyhow::Result<Option<u64>> {
    pod_quantity(value)?.map(|n| {
        // Integers through 2^53 are exactly representable by the shared quantity parser.
        // Refuse larger byte counts instead of saturating or inventing rounded-up capacity.
        anyhow::ensure!(n <= 9_007_199_254_740_992.0, "U01 pod byte quantity exceeds exact representation");
        Ok(n.floor() as u64)
    }).transpose()
}

fn pod_capacity(pod: &Pod) -> anyhow::Result<kiln_ci::sched::pools::PodCapacity> {
    let spec = pod.spec.as_ref().ok_or_else(|| anyhow::anyhow!("U01 pod spec is absent"))?;
    let mut jobs = spec.containers.iter().filter(|c| c.name == "job");
    let job = jobs.next().ok_or_else(|| anyhow::anyhow!("U01 pod job container is absent"))?;
    anyhow::ensure!(jobs.next().is_none(), "U01 duplicate pod job containers");
    let requests = job.resources.as_ref().and_then(|r| r.requests.as_ref());
    let limits = job.resources.as_ref().and_then(|r| r.limits.as_ref());
    let capacity = kiln_ci::sched::pools::PodCapacity {
        resource_version: pod.metadata.resource_version.clone().unwrap_or_default(),
        cpu_request: pod_quantity(requests.and_then(|r| r.get("cpu")))?, cpu_limit: pod_quantity(limits.and_then(|r| r.get("cpu")))?,
        memory_request: pod_bytes(requests.and_then(|r| r.get("memory")))?, memory_limit: pod_bytes(limits.and_then(|r| r.get("memory")))?,
        ephemeral_storage_request: pod_bytes(requests.and_then(|r| r.get("ephemeral-storage")))?, ephemeral_storage_limit: pod_bytes(limits.and_then(|r| r.get("ephemeral-storage")))?,
    };
    capacity.validate().map_err(anyhow::Error::msg)?;
    Ok(capacity)
}

impl ExecutorIdentity {
    /// U03: bounded immutable physical identity for ParentBinding; retain the full API identity
    /// alongside the grant for enrollment and stop-proof verification.
    pub fn physical_execution(&self) -> anyhow::Result<String> {
        Ok(format!("pod-{}", crate::util::sha256_hex(&serde_json::to_vec(self)?)))
    }
}

/// U03/U11: latest reported container exits, not whole-UID retirement authority.
/// Neither terminal phase nor restartCount=0 closes the kubelet worker or historical sandboxes.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ExecutorStopped {
    pub owner: ExecutorIdentity,
    pub resource_version: String,
    pub containers: BTreeMap<String, String>,
}

impl ExecutorStopped {
    pub(crate) fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(!self.resource_version.is_empty() && self.resource_version.len() <= 512 && !self.containers.is_empty() && self.containers.len() <= 256, "U03: invalid pod stopped evidence");
        for (name, id) in &self.containers {
            anyhow::ensure!(!name.is_empty() && name.len() <= 253 && !id.is_empty() && id.len() <= 512 && !name.chars().chain(id.chars()).any(char::is_control), "U03: invalid stopped container identity");
        }
        Ok(())
    }
}

fn executor_identity(pod: &Pod, namespace: &str, executor: &str, generation: &str, allocation: &str) -> Option<ExecutorIdentity> {
    let annotations = pod.metadata.annotations.as_ref()?;
    let spec = pod.spec.as_ref()?;
    if annotations.get(EXECUTOR_ANNOTATION)?.as_str() != executor || annotations.get(EXECUTOR_GENERATION)?.as_str() != generation || annotations.get(EXECUTOR_ALLOCATION)?.as_str() != allocation || pod.metadata.namespace.as_deref()? != namespace || spec.restart_policy.as_deref() != Some("Never") { return None }
    let owner = ExecutorIdentity { namespace: namespace.into(), name: pod.metadata.name.clone()?, uid: pod.metadata.uid.clone()?, node: spec.node_name.clone()?, executor: executor.into(), generation: generation.into(), allocation: allocation.into() };
    [&owner.namespace, &owner.name, &owner.uid, &owner.node, &owner.executor, &owner.generation, &owner.allocation].iter().all(|s| !s.is_empty() && s.len() <= 512 && !s.chars().any(char::is_control)).then_some(owner)
}

fn executor_stopped(pod: &Pod, owner: &ExecutorIdentity) -> Option<ExecutorStopped> {
    if executor_identity(pod, &owner.namespace, &owner.executor, &owner.generation, &owner.allocation).as_ref() != Some(owner) { return None }
    let status = pod.status.as_ref()?;
    if !matches!(status.phase.as_deref(), Some("Succeeded" | "Failed")) || matches!(status.reason.as_deref(), Some("NodeLost" | "Shutdown")) { return None }
    let spec = pod.spec.as_ref()?;
    // A restart means this snapshot cannot enumerate every previously owned container ID.
    // Keep it fenced until the node supplies complete evidence; never infer death from phase.
    let mut containers = BTreeMap::new();
    let sets = [
        (spec.containers.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), status.container_statuses.as_deref().unwrap_or_default()),
        (spec.init_containers.iter().flatten().map(|c| c.name.as_str()).collect(), status.init_container_statuses.as_deref().unwrap_or_default()),
        (spec.ephemeral_containers.iter().flatten().map(|c| c.name.as_str()).collect(), status.ephemeral_container_statuses.as_deref().unwrap_or_default()),
    ];
    for (names, states) in sets {
        if names.len() != states.len() { return None }
        for name in names {
            let state = states.iter().find(|s| s.name == name)?;
            if state.restart_count != 0 { return None }
            let terminated = state.state.as_ref()?.terminated.as_ref()?;
            let id = state.container_id.as_ref()?;
            if id.is_empty() || terminated.container_id.as_ref() != Some(id) || terminated.finished_at.is_none() || containers.insert(name.into(), id.clone()).is_some() { return None }
        }
    }
    if containers.is_empty() { return None }
    let resource_version = pod.metadata.resource_version.clone().filter(|v| !v.is_empty())?;
    let proof = ExecutorStopped { owner: owner.clone(), resource_version, containers };
    proof.validate().ok()?;
    Some(proof)
}
/// Where a pod's state stops it starting: its image can't be pulled, or its container made.
const FATAL: &[&str] = &["ErrImagePull", "ImagePullBackOff", "InvalidImageName", "CreateContainerConfigError", "CreateContainerError"];

/// The pod a `k8s` pool's executor is: `kiln-x-` and its id, as a DNS label (lowercase, at most
/// 63 characters), unique by a hash of the id.
pub fn executor_pod(executor: &str) -> String {
    let clean: String = executor.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' }).collect();
    let hash = crate::util::sha256_hex(executor.as_bytes());
    let short: String = clean.chars().take(40).collect();
    format!("kiln-x-{}-{}", short.trim_matches('-'), hash.get(..8).unwrap_or(&hash))
}

/// One executor pod, as the reaper sees it.
#[derive(Clone, Debug)]
pub struct ExecutorPod {
    pub namespace: String,
    pub name: String,
    pub uid: String,
    pub resource_version: String,
    pub executor: String,
    pub generation: String,
    pub allocation: String,
    pub phase: String,
    /// Why it can't start, when it can't.
    pub stuck: Option<String>,
    pub deleting: bool,
}

impl ExecutorPod {
    /// U11-F03: labels and the reusable name select diagnostics, never cleanup authority.
    /// resourceVersion is retained evidence, not an ordering number or immutable identity.
    pub(crate) fn valid_owner(&self) -> bool {
        [&self.namespace, &self.name, &self.uid, &self.resource_version, &self.executor, &self.generation, &self.allocation]
            .iter().all(|s| !s.is_empty() && s.len() <= 512 && !s.chars().any(char::is_control))
    }

    pub(crate) fn observes(&self, owner: &ExecutorIdentity) -> bool {
        self.valid_owner() && self.namespace == owner.namespace && self.name == owner.name && self.uid == owner.uid
            && self.executor == owner.executor && self.generation == owner.generation && self.allocation == owner.allocation
    }

    pub(crate) fn stuck_key(&self) -> (String, String, String, String) {
        (self.namespace.clone(), self.uid.clone(), self.generation.clone(), self.allocation.clone())
    }
}

/// U11-F05: one atomic API snapshot's binding shape, never an actionable NeverBound
/// receipt. Original Create-UID custody, reviewed provider/version, producer closure and
/// (for an unbound nondeleting snapshot) acknowledged UID+RV deletion are separate premises.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PodBindingObservation {
    Unknown,
    ExcludedMirror,
    Bound,
    Unbound { deleting: bool },
}

pub(crate) struct ExecutorPodObservation {
    pub pod: ExecutorPod,
    pub binding: PodBindingObservation,
}

fn pod_binding_observation(pod: &Pod) -> PodBindingObservation {
    if pod.metadata.annotations.as_ref().is_some_and(|annotations| annotations.contains_key("kubernetes.io/config.mirror")) {
        return PodBindingObservation::ExcludedMirror;
    }
    let Some(spec) = &pod.spec else { return PodBindingObservation::Unknown; };
    // Keep absence/incomplete identity unknown. ResourceVersion is an opaque equality token,
    // not a numeric order; the same snapshot's exact UID+RV remain in ExecutorPod.
    if [&pod.metadata.namespace, &pod.metadata.name, &pod.metadata.uid, &pod.metadata.resource_version].iter()
        .any(|value| value.as_deref().is_none_or(|value| value.is_empty() || value.len() > 512 || value.chars().any(char::is_control))) {
        return PodBindingObservation::Unknown;
    }
    if spec.node_name.as_deref().is_some_and(|node| !node.is_empty()) { PodBindingObservation::Bound }
    else { PodBindingObservation::Unbound { deleting: pod.metadata.deletion_timestamp.is_some() } }
}

pub struct K8s {
    api: KubeApi,
    cfg: KCfg,
    /// The nodes each warm pool's pods ran on, newest first (W24-R14).
    warm_nodes: parking_lot::Mutex<HashMap<String, Vec<String>>>,
}

impl K8s {
    /// U03: use this API identity at enrollment; the token's pod name is insufficient.
    pub(crate) async fn executor_bootstrap(&self, executor: &str, generation: &str, allocation: &str) -> anyhow::Result<Option<ExecutorBootstrap>> {
        let Some(pod) = self.api.get_pod(&executor_pod(executor)).await? else { return Ok(None) };
        let Some(identity) = executor_identity(&pod, &self.api.namespace, executor, generation, allocation) else { return Ok(None) };
        Ok(Some(ExecutorBootstrap { identity, capacity: pod_capacity(&pod)? }))
    }

    /// U03/U11: this reports current exact-UID instance exits only. API 404, replaced UID,
    /// terminal phase and complete latest statuses never prove provider/producer closure.
    pub async fn executor_stopped(&self, owner: &ExecutorIdentity) -> anyhow::Result<Option<ExecutorStopped>> {
        anyhow::ensure!(owner.namespace == self.api.namespace, "the executor belongs to another namespace");
        let Some(pod) = self.api.get_pod(&owner.name).await? else { return Ok(None) };
        Ok(executor_stopped(&pod, owner))
    }

    pub async fn delete_executor(&self, owner: &ExecutorIdentity) -> anyhow::Result<()> {
        anyhow::ensure!(owner.namespace == self.api.namespace, "the executor belongs to another namespace");
        self.api.delete_pod_uid(&owner.name, &owner.uid).await
    }

    pub async fn connect(cfg: &KCfg) -> anyhow::Result<Self> {
        let api = KubeApi::discover(&cfg.namespace)?;
        // Probe permissions now so a broken RBAC setup is reported at startup.
        api.list_pods(MANAGED).await?;
        Ok(Self { api, cfg: cfg.clone(), warm_nodes: parking_lot::Mutex::default() })
    }

    pub async fn launch(&self, hub: &Hub, job: &Job, run: &Run) -> anyhow::Result<String> {
        let spec = job.spec.as_ref().ok_or_else(|| anyhow::anyhow!("job has no spec"))?;
        let short = job.id.get(job.id.len().saturating_sub(10)..).unwrap_or(&job.id);
        let name = format!("kiln-{}-{}", short, job.attempt);
        let token = crate::util::random_token();
        let ttl = Duration::from_secs(u64::from(self.cfg.start_timeout_seconds) + 120);
        hub.ephemeral_tokens.lock().insert(token.clone(), (job.id.clone(), Instant::now() + ttl));
        let server_url = self.cfg.server_url.clone().unwrap_or_else(|| hub.cfg.server.public_url.clone());
        let server_quic = self.cfg.server_quic.clone().unwrap_or_else(|| hub.cfg.server.quic_advertise.clone());
        let pod = job_pod(&self.cfg, job, spec, run, &name, &token, (&server_url, &server_quic));
        self.api.create_pod(&pod).await?;
        Ok(name)
    }

    /// A `k8s` pool's executor (docs/ci.md W13-R07): [`pool_pod`], whose own runner enrolls with
    /// a one-time token bound to the executor and the pod's name.
    ///
    /// A pool with `warm` keeps its runners' warm workspaces on their nodes ([`warm`], W24-R14).
    pub(crate) async fn launch_executor(&self, hub: &Hub, grant: &crate::ci::dispatch::ExecutorGrant, pool: &kiln_ci::sched::pools::PoolSpec, pod: Option<&crate::ci::pools::LegPod>) -> anyhow::Result<String> {
        let executor = grant.executor.as_str();
        anyhow::ensure!(grant.snapshot.runner.is_empty() && grant.phase == "starting" && grant.physical_execution.is_none(), "U03: pod startup does not own an unstarted server executor grant");
        let warm = self.warm_for(hub, pool);
        let name = executor_pod(executor);
        let ttl = Duration::from_secs(u64::from(self.cfg.start_timeout_seconds) + 120);
        let token = hub.mint_executor_token(grant, &name, ttl).await?;
        let server_url = self.cfg.server_url.clone().unwrap_or_else(|| hub.cfg.server.public_url.clone());
        let server_quic = self.cfg.server_quic.clone().unwrap_or_else(|| hub.cfg.server.quic_advertise.clone());
        let mut made = pool_pod(&self.cfg, executor, &name, &token, (&server_url, &server_quic), pool, pod);
        let annotations = made.metadata.annotations.get_or_insert_with(BTreeMap::new);
        annotations.insert(EXECUTOR_GENERATION.into(), grant.generation.clone());
        annotations.insert(EXECUTOR_ALLOCATION.into(), grant.allocation.clone());
        if let Some(w) = &warm {
            apply_warm(&mut made, w);
        }
        self.api.create_pod(&made).await?;
        Ok(name)
    }

    /// What a pool's next pod gets of `warm` (W24-R14): none when the pool keeps no workspaces, or
    /// while untrusted legs may run in its pods (`ci.untrusted.executors` naming `k8s`), which
    /// would write where trusted legs read (W24-R21).
    fn warm_for(&self, hub: &Hub, pool: &kiln_ci::sched::pools::PoolSpec) -> Option<Warm> {
        let w = pool.warm.as_ref()?;
        if hub.cfg.ci.untrusted.executors.iter().any(|e| e == "k8s") {
            tracing::warn!(pool = %pool.name, "a k8s pool's pod starts cold: untrusted legs may run in its pods (`ci.untrusted.executors` names k8s), and they'd write warm workspaces trusted legs read");
            return None;
        }
        let nodes = self.warm_nodes.lock().get(&pool.name).cloned().unwrap_or_default();
        Some(warm(w, &pool.name, &nodes))
    }

    /// The `k8s` pools' executor pods there are. A warm pool's pods on a node are remembered, the
    /// pool's next pods drawn there (W24-R14).
    pub async fn executor_pods(&self) -> anyhow::Result<Vec<ExecutorPod>> {
        Ok(self.executor_pod_observations().await?.into_iter().map(|observation| observation.pod).collect())
    }

    /// Retain node/deletion shape with the exact same bounded UID/version observation.
    /// The existing reaper consumes it; this adds no client/controller or release authority.
    pub(crate) async fn executor_pod_observations(&self) -> anyhow::Result<Vec<ExecutorPodObservation>> {
        let list = self.api.list_pods(EXECUTOR_PODS).await?;
        {
            let mut nodes = self.warm_nodes.lock();
            for p in &list {
                if let (Some(pool), Some(node)) = (p.metadata.annotations.as_ref().and_then(|a| a.get(WARM_ANNOTATION)), p.spec.as_ref().and_then(|s| s.node_name.as_deref())) {
                    remember(&mut nodes, pool, node);
                }
            }
        }
        Ok(list
            .into_iter()
            .filter_map(|p| {
                let name = p.metadata.name.clone()?;
                let executor = p.metadata.annotations.as_ref().and_then(|a| a.get(EXECUTOR_ANNOTATION)).cloned().unwrap_or_default();
                let status = p.status.clone().unwrap_or_default();
                let stuck = status.container_statuses.iter().flatten().chain(status.init_container_statuses.iter().flatten()).filter_map(|c| c.state.as_ref()?.waiting.as_ref()).find_map(|w| {
                    let r = w.reason.clone().unwrap_or_default();
                    FATAL.contains(&r.as_str()).then(|| format!("{r}: {}", w.message.clone().unwrap_or_default()))
                });
                let annotation = |key: &str| p.metadata.annotations.as_ref().and_then(|a| a.get(key)).cloned().unwrap_or_default();
                let binding = pod_binding_observation(&p);
                let pod = ExecutorPod {
                    namespace: p.metadata.namespace.clone().unwrap_or_default(), name, uid: p.metadata.uid.clone().unwrap_or_default(),
                    resource_version: p.metadata.resource_version.clone().unwrap_or_default(), executor,
                    generation: annotation(EXECUTOR_GENERATION), allocation: annotation(EXECUTOR_ALLOCATION),
                    phase: status.phase.unwrap_or_default(), stuck, deleting: p.metadata.deletion_timestamp.is_some(),
                };
                Some(ExecutorPodObservation { pod, binding })
            })
            .collect())
    }

    /// Is a pod for this job still legitimately on its way (Pending waiting for capacity or
    /// pulling its image, or Running)? Pods stuck on a fatal start error, or already being
    /// deleted, do not count.
    pub async fn job_pod_alive(&self, job: &str) -> bool {
        let Ok(list) = self.api.list_pods(&format!("{MANAGED},{JOB_LABEL}={job}")).await else { return false };
        list.iter().any(|p| {
            let st = p.status.clone().unwrap_or_default();
            let phase = st.phase.unwrap_or_default();
            let fatal = st
                .container_statuses
                .iter()
                .flatten()
                .chain(st.init_container_statuses.iter().flatten())
                .filter_map(|c| c.state.as_ref()?.waiting.as_ref()?.reason.clone())
                .any(|r| matches!(r.as_str(), "ErrImagePull" | "ImagePullBackOff" | "InvalidImageName" | "CreateContainerConfigError" | "CreateContainerError"));
            p.metadata.deletion_timestamp.is_none() && (phase == "Pending" || phase == "Running") && !fatal
        })
    }

    /// Is the pod an ephemeral agent runs in still alive (agents are named after their pod)?
    pub async fn agent_pod_alive(&self, pod: &str) -> bool {
        match self.api.get_pod(pod).await {
            Ok(Some(p)) => {
                let phase = p.status.and_then(|s| s.phase).unwrap_or_default();
                p.metadata.deletion_timestamp.is_none() && (phase == "Pending" || phase == "Running")
            }
            Ok(None) => false,
            // Unknown (API hiccup): assume alive and let the grace period decide.
            Err(_) => true,
        }
    }

    pub async fn delete_job_pods(&self, job: &str) {
        match self.api.list_pods(&format!("{MANAGED},{JOB_LABEL}={job}")).await {
            Ok(list) => {
                for p in list {
                    if let Some(n) = p.metadata.name.as_deref()
                        && let Err(e) = self.api.delete_pod(&p).await {
                            tracing::warn!(pod = %n, error = %e, "pod delete failed");
                        }
                }
            }
            Err(e) => tracing::warn!(error = %e, "pod list failed"),
        }
    }

    /// Delete finished jobs' pods; fail jobs whose pods are stuck unable to start. A pod that carries
    /// no job isn't this loop's: a CI pool's executor (`ci::pools::reap_pods` keeps those), an image
    /// check. Taken for a finished job's, an executor's pod went within 15 s of starting, its leg
    /// lost with it.
    pub async fn reap_loop(self: Arc<Self>, hub: Arc<Hub>) -> anyhow::Result<()> {
        let mut pending_since: std::collections::HashMap<String, Instant> = Default::default();
        loop {
            if !hub.shutdown.sleep(Duration::from_secs(15)).await {
                return Ok(());
            }
            let list = self.api.list_pods(MANAGED).await?;
            let mut seen = std::collections::HashSet::new();
            for pod in list {
                let Some(name) = pod.metadata.name.clone() else { continue };
                seen.insert(name.clone());
                let Some(job_id) = pod.metadata.labels.as_ref().and_then(|l| l.get(JOB_LABEL)).cloned() else { continue };
                let status = pod.status.clone().unwrap_or_default();
                let phase = status.phase.clone().unwrap_or_default();
                let job_terminal = match hub.store.job(job_id.clone()).await? {
                    Some(j) => j.status.is_terminal(),
                    None => true,
                };
                if phase == "Succeeded" || phase == "Failed" || job_terminal {
                    if let Err(e) = self.api.delete_pod(&pod).await {
                        tracing::debug!(pod = %name, error = %e, "reap delete failed");
                    }
                    continue;
                }
                let waiting = status
                    .container_statuses
                    .iter()
                    .flatten()
                    .chain(status.init_container_statuses.iter().flatten())
                    .filter_map(|c| c.state.as_ref()?.waiting.as_ref())
                    .find_map(|w| {
                        let r = w.reason.clone().unwrap_or_default();
                        matches!(
                            r.as_str(),
                            "ErrImagePull" | "ImagePullBackOff" | "InvalidImageName" | "CreateContainerConfigError" | "CreateContainerError"
                        )
                        .then(|| format!("{r}: {}", w.message.clone().unwrap_or_default()))
                    });
                if let Some(reason) = waiting {
                    let since = *pending_since.entry(name.clone()).or_insert_with(Instant::now);
                    if since.elapsed() > Duration::from_secs(120) {
                        tracing::warn!(pod = %name, %reason, "pod cannot start; failing job");
                        let _ = self.api.delete_pod(&pod).await;
                        if let Ok(Some(j)) = hub.store.job(job_id.clone()).await
                            && j.status == Status::Queued {
                                hub.store.claim_job(job_id.clone(), name.clone(), "k8s".into()).await?;
                                let (run, _) = hub
                                    .store
                                    .finish_job(job_id.clone(), Status::Failed, None, Some(format!("pod {name}: {reason}")), None, None)
                                    .await?;
                                hub.emit_job(&job_id).await;
                                hub.emit_run(&run).await;
                            }
                    }
                }
            }
            pending_since.retain(|n, _| seen.contains(n));
        }
    }
}

/// The toolchains' caches a job pod keeps under the cache claim (`[kubernetes] cache_pvc`), shared
/// by every job: v1's, and a kiln-dialect leg's on a `k8s` pool (docs/ci.md W02-R06).
const CACHE_ENV: &[(&str, &str)] = &[
    ("CARGO_HOME", "/cache/cargo"),
    ("GOMODCACHE", "/cache/go/mod"),
    ("GOCACHE", "/cache/go/build"),
    ("UV_CACHE_DIR", "/cache/uv"),
    ("npm_config_cache", "/cache/npm"),
    ("BUN_INSTALL_CACHE_DIR", "/cache/bun"),
    ("KILN_GIT_CACHE", "/cache/git"),
];

/// Prefer worker nodes: the control-plane node keeps room for the platform (kiln, ArgoCD,
/// Prometheus), so restarting those rarely needs to preempt a build.
fn worker_affinity() -> Affinity {
    Affinity {
        node_affinity: Some(NodeAffinity {
            preferred_during_scheduling_ignored_during_execution: Some(vec![PreferredSchedulingTerm {
                weight: 100,
                preference: NodeSelectorTerm {
                    match_expressions: Some(vec![NodeSelectorRequirement { key: "node-role.kubernetes.io/control-plane".into(), operator: "DoesNotExist".into(), values: None }]),
                    ..Default::default()
                },
            }]),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// The init container that copies the static `kiln` into the pod. Explicit requests: a namespace
/// ResourceQuota rejects pods with any container lacking them (init containers included).
fn kiln_bin(cfg: &KCfg) -> Container {
    let q = |s: &str| Quantity(s.to_owned());
    Container {
        name: "kiln-bin".into(),
        image: Some(cfg.agent_image.clone()),
        image_pull_policy: Some(cfg.image_pull_policy.clone()),
        command: Some(vec!["cp".into(), "/usr/local/bin/kiln".into(), "/kiln-bin/kiln".into()]),
        volume_mounts: Some(vec![mount("kiln-bin", "/kiln-bin")]),
        resources: Some(ResourceRequirements {
            requests: Some(BTreeMap::from([("cpu".to_owned(), q("50m")), ("memory".to_owned(), q("32Mi"))])),
            limits: Some(BTreeMap::from([("memory".to_owned(), q("128Mi"))])),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// The cache claim's volume, when the server has one.
fn cache_volume(cfg: &KCfg) -> Option<Volume> {
    cfg.cache_pvc.as_ref().map(|pvc| Volume { name: "cache".into(), persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource { claim_name: pvc.clone(), read_only: Some(false) }), ..Default::default() })
}

/// A v1 `runs-on: [k8s]` job's pod: its job container the job's `image:` (else its toolchain's
/// preset) running `kiln agent --ephemeral --job <id>`, with the job's CPU and memory (memory its
/// limit too), the cache claim and its caches, and a deadline of the job's timeout and 15 minutes.
fn job_pod(cfg: &KCfg, job: &Job, spec: &crate::pipeline::PlannedJob, run: &Run, name: &str, token: &str, (server_url, server_quic): (&str, &str)) -> Pod {
    let image = spec.image.clone().unwrap_or_else(|| crate::pipeline::toolchain_image(spec.toolchain.as_deref()).to_owned());
    let labels: Vec<String> = spec.labels.iter().cloned().chain(["k8s".to_owned()]).collect();
    let mut env = vec![env_var("KILN_JOIN_TOKEN", token), env_var("KILN_SERVER", server_url), env_var("KILN_QUIC", server_quic)];
    let mut mounts = vec![mount("kiln-bin", "/kiln-bin"), mount("work", "/work")];
    let mut volumes = vec![empty_dir("kiln-bin"), empty_dir("work")];
    if let Some(v) = cache_volume(cfg) {
        mounts.push(mount("cache", "/cache"));
        volumes.push(v);
        env.extend(CACHE_ENV.iter().map(|(k, v)| env_var(k, v)));
    }
    let q = |s: &str| Quantity(s.to_owned());
    let cpu = spec.resources.cpu.clone().unwrap_or_else(|| cfg.default_cpu.clone());
    let mem = spec.resources.memory.clone().unwrap_or_else(|| cfg.default_memory.clone());
    let resources = ResourceRequirements {
        requests: Some(BTreeMap::from([("cpu".to_owned(), q(&cpu)), ("memory".to_owned(), q(&mem))])),
        limits: Some(BTreeMap::from([("memory".to_owned(), q(&mem))])),
        ..Default::default()
    };
    Pod {
        metadata: ObjectMeta {
            name: Some(name.to_owned()),
            namespace: Some(cfg.namespace.clone()),
            labels: Some(BTreeMap::from([
                ("app.kubernetes.io/managed-by".to_owned(), "kiln".to_owned()),
                ("app.kubernetes.io/name".to_owned(), "kiln-job".to_owned()),
                (JOB_LABEL.to_owned(), job.id.clone()),
                ("kiln.dev/run".to_owned(), run.id.clone()),
            ])),
            annotations: Some(BTreeMap::from([("kiln.dev/repo".to_owned(), run.repo_name.clone()), ("kiln.dev/job-key".to_owned(), job.key.clone())])),
            ..Default::default()
        },
        spec: Some(PodSpec {
            restart_policy: Some("Never".into()),
            priority_class_name: cfg.priority_class.clone(),
            affinity: Some(worker_affinity()),
            service_account_name: Some(cfg.service_account.clone()),
            automount_service_account_token: Some(false),
            active_deadline_seconds: Some(i64::from(spec.timeout_minutes) * 60 + 900),
            termination_grace_period_seconds: Some(20),
            node_selector: (!cfg.node_selector.is_empty()).then(|| cfg.node_selector.clone()),
            init_containers: Some(vec![kiln_bin(cfg)]),
            containers: vec![Container {
                name: "job".into(),
                image: Some(image),
                image_pull_policy: Some("IfNotPresent".into()),
                command: Some(vec![
                    "/kiln-bin/kiln".into(),
                    "agent".into(),
                    "--ephemeral".into(),
                    "--job".into(),
                    job.id.clone(),
                    "--name".into(),
                    name.to_owned(),
                    "--labels".into(),
                    labels.join(","),
                    "--work-dir".into(),
                    "/work".into(),
                    "--slots".into(),
                    "1".into(),
                ]),
                env: Some(env),
                resources: Some(resources),
                volume_mounts: Some(mounts),
                working_dir: Some("/work".into()),
                ..Default::default()
            }],
            volumes: Some(volumes),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// A `k8s` pool's executor's pod (docs/ci.md W13-R07): its job container the pool's image (else
/// Debian's) running `kiln agent --ephemeral`, copied in by the init container as a job's pod has
/// it, which takes the pool's one leg it's given and leaves; the pool's `resources` as its requests
/// (memory its limit too); the cache claim mounted as a job pod's is; on a worker node where there
/// is one, as a job pod.
///
/// A pod made for a leg (`pod`) is that leg's. Its `container:` is the job container (its image,
/// its environment), and each of its `services:` a native sidecar (an init container that keeps
/// running) with its ports, on the pod's `localhost`, which `hostAliases` name it by too: the
/// runner inside is told so (`KILN_POD_CONTAINERS`, and `KILN_POD_SERVICES` with each service's
/// ports) and starts no containers of its own. A kiln-dialect leg's pod is v1's job pod
/// ([`crate::ci::pools::kiln_pod`], W02-R06): the image of its `image:` or `toolchain:`, its own
/// CPU and memory over the pool's, the toolchains' caches and v1's deadline.
pub(crate) fn pool_pod(cfg: &KCfg, executor: &str, name: &str, token: &str, (server_url, server_quic): (&str, &str), pool: &kiln_ci::sched::pools::PoolSpec, pod: Option<&crate::ci::pools::LegPod>) -> Pod {
    let image = pod.and_then(|p| p.image.clone()).or_else(|| pool.image.clone().filter(|i| !i.trim().is_empty())).unwrap_or_else(|| crate::pipeline::toolchain_image(None).to_owned());
    let mut env = vec![env_var("KILN_JOIN_TOKEN", token), env_var("KILN_SERVER", server_url), env_var("KILN_QUIC", server_quic), env_var("KILN_POD", "1")];
    // The leg's containers are the pod's.
    let services = pod.map(|p| p.services.clone()).unwrap_or_default();
    if let Some(p) = pod {
        env.extend(p.env.iter().map(|(k, v)| env_var(k, v)));
        if p.containers {
            env.push(env_var("KILN_POD_CONTAINERS", "1"));
            let ports: serde_json::Map<String, serde_json::Value> = services.iter().map(|s| (s.id.clone(), serde_json::json!(s.ports))).collect();
            env.push(env_var("KILN_POD_SERVICES", &serde_json::Value::Object(ports).to_string()));
        }
    }
    let label = |id: &str| -> String {
        let l: String = id.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' }).take(50).collect();
        format!("svc-{}", l.trim_matches('-'))
    };
    let sidecars: Vec<Container> = services
        .iter()
        .map(|s| Container {
            name: label(&s.id),
            image: Some(s.image.clone()),
            image_pull_policy: Some("IfNotPresent".into()),
            env: Some(s.env.iter().map(|(k, v)| env_var(k, v)).collect()),
            ports: Some(s.ports.iter().map(|p| ContainerPort { container_port: i32::from(*p), ..Default::default() }).collect()),
            // A native sidecar: started before the job container, kept running beside it.
            restart_policy: Some("Always".into()),
            resources: Some(ResourceRequirements {
                requests: Some(BTreeMap::from([("cpu".to_owned(), Quantity("100m".into())), ("memory".to_owned(), Quantity("128Mi".into()))])),
                ..Default::default()
            }),
            ..Default::default()
        })
        .collect();
    let host_aliases = (!services.is_empty()).then(|| vec![HostAlias { ip: "127.0.0.1".into(), hostnames: Some(services.iter().map(|s| s.id.clone()).collect()) }]);
    let mut mounts = vec![mount("kiln-bin", "/kiln-bin"), mount("work", "/work")];
    let mut volumes = vec![empty_dir("kiln-bin"), empty_dir("work")];
    let docker = pool.docker.as_ref().map(pod_docker);
    if docker.is_some() {
        mounts.push(mount("docker-sock", POD_DOCKER_DIR));
        volumes.extend([empty_dir("docker-sock"), empty_dir("docker-data")]);
        env.push(env_var("DOCKER_HOST", &format!("unix://{POD_DOCKER_DIR}/docker.sock")));
        env.push(env_var("KILN_POD_DOCKER", "/kiln-bin/docker"));
    }
    if let Some(v) = cache_volume(cfg) {
        mounts.push(mount("cache", "/cache"));
        volumes.push(v);
        let caches = pod.is_some_and(|p| p.caches);
        env.extend(CACHE_ENV.iter().filter(|(k, _)| caches || *k == "KILN_GIT_CACHE").map(|(k, v)| env_var(k, v)));
    }
    let q = |s: &str| Quantity(s.to_owned());
    let r = pool.resources.clone().unwrap_or_default();
    let cpu = pod.and_then(|p| p.cpu.clone()).or_else(|| r.cpu.clone()).unwrap_or_else(|| cfg.default_cpu.clone());
    let mem = pod.and_then(|p| p.memory.clone()).or_else(|| r.memory.clone()).unwrap_or_else(|| cfg.default_memory.clone());
    let mut requests = BTreeMap::from([("cpu".to_owned(), q(&cpu)), ("memory".to_owned(), q(&mem))]);
    if let Some(disk) = &r.disk {
        requests.insert("ephemeral-storage".to_owned(), q(disk));
    }
    let resources = ResourceRequirements { requests: Some(requests), limits: Some(BTreeMap::from([("memory".to_owned(), q(&mem))])), ..Default::default() };
    Pod {
        metadata: ObjectMeta {
            name: Some(name.to_owned()),
            namespace: Some(cfg.namespace.clone()),
            labels: Some(BTreeMap::from([
                ("app.kubernetes.io/managed-by".to_owned(), "kiln".to_owned()),
                ("app.kubernetes.io/name".to_owned(), "kiln-executor".to_owned()),
                ("kiln.dev/pool".to_owned(), pool.name.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '-' }).take(63).collect()),
            ])),
            annotations: Some(BTreeMap::from([(EXECUTOR_ANNOTATION.to_owned(), executor.to_owned()), ("kiln.dev/pool".to_owned(), pool.name.clone())])),
            ..Default::default()
        },
        spec: Some(PodSpec {
            restart_policy: Some("Never".into()),
            priority_class_name: cfg.priority_class.clone(),
            affinity: Some(worker_affinity()),
            service_account_name: Some(cfg.service_account.clone()),
            automount_service_account_token: Some(false),
            active_deadline_seconds: pod.and_then(|p| p.deadline_s),
            termination_grace_period_seconds: Some(20),
            node_selector: (!cfg.node_selector.is_empty()).then(|| cfg.node_selector.clone()),
            host_aliases,
            init_containers: Some(std::iter::once(kiln_bin(cfg)).chain(docker.into_iter().flatten()).chain(sidecars).collect()),
            containers: vec![Container {
                name: "job".into(),
                image: Some(image),
                image_pull_policy: Some("IfNotPresent".into()),
                command: Some(vec![
                    "/kiln-bin/kiln".into(),
                    "agent".into(),
                    "--ephemeral".into(),
                    "--name".into(),
                    name.to_owned(),
                    "--labels".into(),
                    pool.labels.join(","),
                    "--work-dir".into(),
                    "/work".into(),
                    "--slots".into(),
                    "1".into(),
                ]),
                env: Some(env),
                resources: Some(resources),
                volume_mounts: Some(mounts),
                working_dir: Some("/work".into()),
                ..Default::default()
            }],
            volumes: Some(volumes),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Where a pod's Docker sidecar (W13-R07) puts its socket, in its job container too.
const POD_DOCKER_DIR: &str = "/run/kiln-docker";

/// A `k8s` pool's Docker for Docker actions (W13-R07): the Docker CLI copied in beside `kiln`, then
/// the engine as a native sidecar answering on a socket the job container shares, started before
/// it (its startup probe waits for the socket). The work directory is at the same path in both,
/// so what a step bind-mounts is there; its images and containers are on a volume of their own.
fn pod_docker(d: &kiln_ci::sched::pools::PodDocker) -> [Container; 2] {
    let socket = format!("unix://{POD_DOCKER_DIR}/docker.sock");
    let small =
        |cpu: &str, mem: &str| ResourceRequirements { requests: Some(BTreeMap::from([("cpu".to_owned(), Quantity(cpu.into())), ("memory".to_owned(), Quantity(mem.into()))])), ..Default::default() };
    let (command, data, env) = match d.engine() {
        // Rootless Docker in Docker, its TLS off: it answers on the pod's socket only.
        "docker" => (vec!["dockerd-entrypoint.sh".to_owned(), format!("--host={socket}")], "/home/rootless/.local/share/docker", vec![env_var("DOCKER_TLS_CERTDIR", "")]),
        _ => (vec!["podman".to_owned(), "system".to_owned(), "service".to_owned(), "--time=0".to_owned(), socket], "/var/lib/containers", Vec::new()),
    };
    let cli = Container {
        name: "docker-cli".into(),
        image: Some(d.cli_image()),
        image_pull_policy: Some("IfNotPresent".into()),
        command: Some(vec!["cp".into(), "/usr/local/bin/docker".into(), "/kiln-bin/docker".into()]),
        volume_mounts: Some(vec![mount("kiln-bin", "/kiln-bin")]),
        resources: Some(small("50m", "32Mi")),
        ..Default::default()
    };
    let engine = Container {
        name: "docker".into(),
        image: Some(d.image()),
        image_pull_policy: Some("IfNotPresent".into()),
        command: Some(command),
        env: Some(env),
        security_context: Some(SecurityContext { privileged: Some(true), ..Default::default() }),
        restart_policy: Some("Always".into()),
        startup_probe: Some(Probe {
            exec: Some(ExecAction { command: Some(vec!["sh".into(), "-c".into(), format!("test -S {POD_DOCKER_DIR}/docker.sock")]) }),
            period_seconds: Some(1),
            failure_threshold: Some(300),
            ..Default::default()
        }),
        volume_mounts: Some(vec![mount("docker-sock", POD_DOCKER_DIR), mount("work", "/work"), mount("docker-data", data)]),
        resources: Some(small("100m", "256Mi")),
        ..Default::default()
    };
    [cli, engine]
}

/// Where a pod's runner keeps its warm workspaces (W24-R11): `<work>/warm`.
const POD_WARM_DIR: &str = "/work/warm";
/// A warm pool's pod's annotation: its pool, whose nodes are remembered (W24-R14).
const WARM_ANNOTATION: &str = "kiln.dev/warm";
/// How many of the nodes a warm pool's pods ran on its next pods are drawn to.
const WARM_NODES: usize = 4;

/// What a pod of a pool with `warm` gets (docs/ci.md W24-R14): the folder `<root>/<pool>` on its
/// node (a `hostPath`, made when it's missing), at `/work/warm`, where its runner keeps its lanes'
/// warm workspaces; a node carrying the pool's `node_label`, when it names one; and a pull toward
/// the nodes the pool's pods ran on last (`nodes`, newest first), whose folders hold their lanes.
#[derive(Debug, Clone, PartialEq)]
pub struct Warm {
    pub pool: String,
    pub volume: Volume,
    pub required: Option<NodeSelectorTerm>,
    pub preferred: Vec<PreferredSchedulingTerm>,
}

pub fn warm(w: &kiln_ci::sched::pools::PodWarm, pool: &str, nodes: &[String]) -> Warm {
    // A pool's name is letters, digits, `-`, `_` and `.`; one of dots alone isn't a folder's.
    let folder = if pool.chars().all(|c| c == '.') { pool.replace('.', "_") } else { pool.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '-' }).collect() };
    let path = format!("{}/{folder}", w.root().trim_end_matches('/'));
    let volume = Volume { name: "warm".into(), host_path: Some(HostPathVolumeSource { path, type_: Some("DirectoryOrCreate".into()) }), ..Default::default() };
    let required = w.node_label().map(|(key, value)| NodeSelectorTerm {
        match_expressions: Some(vec![NodeSelectorRequirement { key, operator: if value.is_some() { "In" } else { "Exists" }.into(), values: value.map(|v| vec![v]) }]),
        ..Default::default()
    });
    let preferred = nodes
        .iter()
        .take(WARM_NODES)
        .zip([100, 80, 60, 40])
        .map(|(node, weight)| PreferredSchedulingTerm {
            weight,
            preference: NodeSelectorTerm { match_fields: Some(vec![NodeSelectorRequirement { key: "metadata.name".into(), operator: "In".into(), values: Some(vec![node.clone()]) }]), ..Default::default() },
        })
        .collect();
    Warm { pool: pool.to_owned(), volume, required, preferred }
}

/// Puts `w` in an executor's `pod`: its volume, mounted at `/work/warm` in the job container and in
/// a Docker sidecar's engine (what a step bind-mounts from a warm workspace is there too); its node
/// terms (the label's joins each required term the pod has); its annotation.
pub fn apply_warm(pod: &mut Pod, w: &Warm) {
    pod.metadata.annotations.get_or_insert_with(BTreeMap::new).insert(WARM_ANNOTATION.to_owned(), w.pool.clone());
    let Some(spec) = pod.spec.as_mut() else { return };
    spec.volumes.get_or_insert_with(Vec::new).push(w.volume.clone());
    for c in spec.containers.iter_mut().chain(spec.init_containers.iter_mut().flatten()).filter(|c| c.name == "job" || c.name == "docker") {
        c.volume_mounts.get_or_insert_with(Vec::new).push(mount(&w.volume.name, POD_WARM_DIR));
    }
    if w.required.is_none() && w.preferred.is_empty() {
        return;
    }
    let nodes = spec.affinity.get_or_insert_with(Affinity::default).node_affinity.get_or_insert_with(NodeAffinity::default);
    if let Some(term) = &w.required {
        let selector = nodes.required_during_scheduling_ignored_during_execution.get_or_insert_with(|| NodeSelector { node_selector_terms: Vec::new() });
        if selector.node_selector_terms.is_empty() {
            selector.node_selector_terms.push(term.clone());
        } else {
            for t in &mut selector.node_selector_terms {
                t.match_expressions.get_or_insert_with(Vec::new).extend(term.match_expressions.iter().flatten().cloned());
            }
        }
    }
    nodes.preferred_during_scheduling_ignored_during_execution.get_or_insert_with(Vec::new).extend(w.preferred.iter().cloned());
}

/// Remembers that a pod of warm pool `pool` ran on `node`: its nodes, newest first, the most kept.
fn remember(nodes: &mut HashMap<String, Vec<String>>, pool: &str, node: &str) {
    let list = nodes.entry(pool.to_owned()).or_default();
    list.retain(|n| n != node);
    list.insert(0, node.to_owned());
    list.truncate(WARM_NODES);
}

fn env_var(k: &str, v: &str) -> EnvVar {
    EnvVar { name: k.to_owned(), value: Some(v.to_owned()), ..Default::default() }
}

fn mount(name: &str, path: &str) -> VolumeMount {
    VolumeMount { name: name.to_owned(), mount_path: path.to_owned(), ..Default::default() }
}

fn empty_dir(name: &str) -> Volume {
    Volume { name: name.to_owned(), empty_dir: Some(Default::default()), ..Default::default() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ci::pools::{LegPod, kiln_pod};
    use kiln_ci::sched::pools::{PodWarm, PoolSpec};
    use serde_json::{Value, json};

    #[test]
    fn u03_executor_proof_requires_exact_uid_and_all_container_terminations() -> anyhow::Result<()> {
        let mut pod: Pod = serde_json::from_value(serde_json::json!({
            "metadata": { "name": "kiln-x-test", "namespace": "jobs", "uid": "uid-one", "resourceVersion": "42", "annotations": { "kiln.dev/executor": "executor-one", "kiln.dev/executor-generation": "generation-one", "kiln.dev/executor-allocation": "existing-grant" } },
            "spec": { "restartPolicy": "Never", "nodeName": "node-one", "containers": [{ "name": "job", "image": "fixture", "resources": { "requests": { "cpu": "500m", "memory": "64Mi", "ephemeral-storage": "2Gi" }, "limits": { "cpu": "250m", "memory": "32Mi", "ephemeral-storage": "4Gi" } } }], "initContainers": [{ "name": "tools", "image": "fixture" }] },
            "status": { "phase": "Succeeded", "containerStatuses": [{ "name": "job", "image": "fixture", "imageID": "fixture", "ready": false, "restartCount": 0, "containerID": "containerd://job-one", "state": { "terminated": { "exitCode": 0, "containerID": "containerd://job-one", "finishedAt": "2026-10-29T00:00:00Z" } } }], "initContainerStatuses": [{ "name": "tools", "image": "fixture", "imageID": "fixture", "ready": false, "restartCount": 0, "containerID": "containerd://tools-one", "state": { "terminated": { "exitCode": 0, "containerID": "containerd://tools-one", "finishedAt": "2026-10-29T00:00:00Z" } } }] }
        }))?;
        let owner = executor_identity(&pod, "jobs", "executor-one", "generation-one", "existing-grant").ok_or_else(|| anyhow::anyhow!("fixture identity missing"))?;
        let capacity = pod_capacity(&pod)?;
        assert_eq!((capacity.cpu_request, capacity.cpu_limit, capacity.memory_request, capacity.memory_limit), (Some(0.5), Some(0.25), Some(64 * 1024 * 1024), Some(32 * 1024 * 1024)));
        assert_eq!((capacity.ephemeral_storage_request, capacity.ephemeral_storage_limit), (Some(2 * 1024 * 1024 * 1024), Some(4 * 1024 * 1024 * 1024)));
        assert!(capacity.fits(&kiln_ci::sched::Resources { cpu: 0.25, mem: 32 * 1024 * 1024, ..Default::default() }));
        assert!(!capacity.fits(&kiln_ci::sched::Resources { cpu: 0.5, ..Default::default() }), "actual limit caps the request");
        assert!(!capacity.fits(&kiln_ci::sched::Resources { disk: 1, ..Default::default() }), "ephemeral placement request does not bound workspace/cache growth");
        assert!(pod_quantity(Some(&Quantity("-1".into()))).is_err());
        assert!(pod_quantity(Some(&Quantity("NaN".into()))).is_err());
        assert!(pod_bytes(Some(&Quantity("1Ei".into()))).is_err());
        let unknown = pod_capacity(&Pod { spec: Some(PodSpec { containers: vec![Container { name: "job".into(), ..Default::default() }], ..Default::default() }), metadata: pod.metadata.clone(), ..Default::default() })?;
        assert_eq!(unknown.cpu_request, None);
        assert!(unknown.short(&kiln_ci::sched::Resources { cpu: 0.1, ..Default::default() }).iter().any(|s| s.contains("unknown")));
        let proof = executor_stopped(&pod, &owner).ok_or_else(|| anyhow::anyhow!("fixture termination proof missing"))?;
        assert_eq!(proof.containers.len(), 2);
        assert_eq!(pod_binding_observation(&pod), PodBindingObservation::Bound, "complete latest exits do not turn a bound UID into NeverBound");
        let mut unbound = pod.clone();
        unbound.spec.as_mut().ok_or_else(|| anyhow::anyhow!("fixture spec missing"))?.node_name = None;
        assert_eq!(pod_binding_observation(&unbound), PodBindingObservation::Unbound { deleting: false });
        unbound.metadata.deletion_timestamp = Some(serde_json::from_value(serde_json::json!("2026-10-29T00:00:01Z"))?);
        assert_eq!(pod_binding_observation(&unbound), PodBindingObservation::Unbound { deleting: true }, "same-snapshot deleting metadata is conditional evidence only, not original custody or closure");
        let incomplete = Pod { spec: None, ..unbound.clone() };
        assert_eq!(pod_binding_observation(&incomplete), PodBindingObservation::Unknown);
        unbound.metadata.annotations.get_or_insert_with(BTreeMap::new).insert("kubernetes.io/config.mirror".into(), "mirror".into());
        assert_eq!(pod_binding_observation(&unbound), PodBindingObservation::ExcludedMirror);
        pod.metadata.uid = Some("uid-replacement".into());
        assert!(executor_stopped(&pod, &owner).is_none());
        pod.metadata.uid = Some(owner.uid.clone());
        let status = pod.status.as_mut().ok_or_else(|| anyhow::anyhow!("fixture status missing"))?;
        status.reason = Some("NodeLost".into());
        assert!(executor_stopped(&pod, &owner).is_none());
        let status = pod.status.as_mut().ok_or_else(|| anyhow::anyhow!("fixture status missing"))?;
        status.reason = None;
        status.init_container_statuses = None;
        assert!(executor_stopped(&pod, &owner).is_none(), "phase alone does not prove the init container stopped");
        Ok(())
    }

    fn cfg() -> KCfg {
        KCfg { cache_pvc: Some("kiln-cache".into()), priority_class: Some("kiln-jobs".into()), ..KCfg::default() }
    }

    fn job(planned: crate::pipeline::PlannedJob) -> Job {
        Job {
            id: "j1".into(),
            run_id: "r1".into(),
            key: planned.key.clone(),
            display: planned.display.clone(),
            status: Status::Queued,
            labels: planned.labels.clone(),
            needs: Vec::new(),
            agent: None,
            executor: None,
            attempt: 0,
            queued_at: None,
            started_at: None,
            finished_at: None,
            exit_code: None,
            error: None,
            peak_rss: None,
            cpu_seconds: None,
            spec: Some(planned),
            steps: Vec::new(),
            matrix: None,
            placement: None,
            paused_at: None,
            paused_ms: 0,
        }
    }

    fn run() -> Run {
        Run {
            id: "r1".into(),
            repo_id: "1".into(),
            repo_name: "hyper-light/kiln".into(),
            number: 1,
            pipeline: "ci".into(),
            event: "push".into(),
            branch: "main".into(),
            sha: "0123456789abcdef0123456789abcdef01234567".into(),
            message: String::new(),
            author: String::new(),
            status: Status::Queued,
            created_at: 0,
            started_at: None,
            finished_at: None,
            error: None,
            concurrency_group: None,
        }
    }

    /// What of a pod makes its leg run as it does: the job container's image, requests and limits,
    /// the toolchains' caches, the volumes, the placement and the deadline.
    fn shape(p: &Pod) -> serde_json::Value {
        let spec = p.spec.clone().unwrap_or_default();
        let job = spec.containers.first().cloned().unwrap_or_default();
        let caches: Vec<(String, Option<String>)> = job.env.clone().unwrap_or_default().into_iter().filter(|e| CACHE_ENV.iter().any(|(k, _)| *k == e.name)).map(|e| (e.name, e.value)).collect();
        let volumes: Vec<String> = spec.volumes.clone().unwrap_or_default().into_iter().map(|v| v.name).collect();
        serde_json::json!({
            "image": job.image, "resources": job.resources, "caches": caches, "volumes": volumes, "working_dir": job.working_dir,
            "affinity": spec.affinity, "deadline": spec.active_deadline_seconds, "priority": spec.priority_class_name,
            "service_account": spec.service_account_name, "init": spec.init_containers.unwrap_or_default().into_iter().map(|c| c.image).collect::<Vec<_>>(),
        })
    }

    /// docs/ci.md W02-R06, W13-R07: each of kiln's own `[k8s]` jobs, on a `k8s` pool, gets the pod
    /// v1 makes for it (its toolchain's or `image:`'s image, its CPU and memory, the caches, the
    /// deadline, a worker node), the pool having no image or resources of its own.
    #[test]
    fn w02_r06_kilns_k8s_jobs_get_v1s_pods_on_a_k8s_pool() -> anyhow::Result<()> {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.kiln");
        let pool = PoolSpec { name: "k8s".into(), executor: "k8s".into(), labels: vec!["k8s".into()], max: Some(64), ..PoolSpec::default() };
        let mut compared = Vec::new();
        for file in ["ci.yaml", "sim.yaml", "stress.yaml", "e2e.yaml"] {
            let src = std::fs::read_to_string(root.join(file))?;
            let v1 = crate::pipeline::parse(&src)?;
            let mut ctx = crate::pipeline::Context::default();
            ctx.set("branch", "main");
            let planned = crate::pipeline::plan(&v1, &ctx)?;
            let (ir, diags) = kiln_ci::wf::parse_workflow(&format!(".kiln/{file}"), &src, kiln_ci::expr::Dialect::Kiln);
            anyhow::ensure!(!diags.iter().any(kiln_ci::wf::Diag::is_error), "{file}: {diags:?}");
            let ir = ir.ok_or_else(|| anyhow::anyhow!("{file} has no workflow"))?;
            let mut seen = std::collections::BTreeSet::new();
            for p in planned.into_iter().filter(|p| p.labels.iter().any(|l| l == "k8s")) {
                if !seen.insert(p.spec_id.clone()) {
                    continue;
                }
                let j = ir.jobs.iter().find(|j| j.id == p.spec_id).ok_or_else(|| anyhow::anyhow!("{file}: no job {}", p.spec_id))?;
                let leg = kiln_pod(j, j.kiln.image.as_ref().map(|t| t.src.clone()), p.timeout_minutes);
                let theirs = job_pod(&cfg(), &job(p.clone()), &p, &run(), "kiln-1-0", "t", ("http://kiln:7070", "kiln:7071"));
                let ours = pool_pod(&cfg(), "x1", "kiln-x-1", "t", ("http://kiln:7070", "kiln:7071"), &pool, Some(&leg));
                assert_eq!(shape(&ours), shape(&theirs), "{file}: {}", p.spec_id);
                compared.push(format!("{file}:{}", p.spec_id));
            }
        }
        // ci.yaml's rust, ui and vendor; sim's nightly; stress's pods; e2e's pod jobs.
        for want in ["ci.yaml:rust", "ci.yaml:ui", "ci.yaml:vendor", "sim.yaml:nightly", "stress.yaml:pods", "e2e.yaml:rust", "e2e.yaml:fanout-pods", "e2e.yaml:gate"] {
            assert!(compared.iter().any(|c| c == want), "{want} not compared: {compared:?}");
        }
        Ok(())
    }

    const SIDE_BY_SIDE: &str = "name: pods\non:\n  push: { branches: [main] }\njobs:\n  rust:\n    runs-on: [k8s]\n    toolchain: rust\n    resources: { cpu: \"2\", memory: 16Gi }\n    steps:\n      - run: cargo test\n  alpine:\n    runs-on: [k8s]\n    image: alpine\n    resources: { cpu: 100m, memory: 64Mi }\n    steps:\n      - run: echo ok\n  plain:\n    runs-on: [k8s]\n    steps:\n      - run: echo ok\n";

    /// docs/ci.md W02-R06, W13-R07: v1's pod and the engine's for the same kiln-dialect job, built
    /// side by side (`toolchain: rust` with 2 CPUs and 16Gi; `image: alpine` with 100m and 64Mi),
    /// have the same image, requests, limits and cache environment, field by field; a job that sets
    /// none of `toolchain:`, `image:` and `resources:` keeps the pool's pod.
    #[test]
    fn w02_r06_v1s_pod_and_the_engines_side_by_side() -> anyhow::Result<()> {
        let pool = PoolSpec {
            name: "k8s".into(),
            executor: "k8s".into(),
            labels: vec!["k8s".into()],
            image: Some("kiln/runner-ubuntu-24.04".into()),
            resources: Some(kiln_ci::sched::pools::ResourceSpec { cpu: Some("4".into()), memory: Some("8Gi".into()), ..Default::default() }),
            ..PoolSpec::default()
        };
        let v1 = crate::pipeline::plan(&crate::pipeline::parse(SIDE_BY_SIDE)?, &crate::pipeline::Context::default())?;
        let (ir, diags) = kiln_ci::wf::parse_workflow(".kiln/pods.yaml", SIDE_BY_SIDE, kiln_ci::expr::Dialect::Kiln);
        anyhow::ensure!(!diags.iter().any(kiln_ci::wf::Diag::is_error), "{diags:?}");
        let ir = ir.ok_or_else(|| anyhow::anyhow!("no workflow"))?;
        let job_of = |id: &str| ir.jobs.iter().find(|j| j.id == id).ok_or_else(|| anyhow::anyhow!("no job {id}"));
        let main = |p: &Pod| p.spec.clone().unwrap_or_default().containers.first().cloned().unwrap_or_default();
        let caches = |c: &Container| -> Vec<(String, Option<String>)> { c.env.clone().unwrap_or_default().into_iter().filter(|e| CACHE_ENV.iter().any(|(k, _)| *k == e.name)).map(|e| (e.name, e.value)).collect() };
        for (id, image, cpu, memory) in [("rust", "docker.io/library/rust:1-bookworm", "2", "16Gi"), ("alpine", "alpine", "100m", "64Mi")] {
            let p = v1.iter().find(|p| p.spec_id == id).ok_or_else(|| anyhow::anyhow!("v1 has no {id}"))?;
            let j = job_of(id)?;
            assert!(crate::ci::pools::sets_pod(j), "{id}");
            let leg = kiln_pod(j, j.kiln.image.as_ref().map(|t| t.src.clone()), p.timeout_minutes);
            let theirs = main(&job_pod(&cfg(), &job(p.clone()), p, &run(), "kiln-1-0", "t", ("http://kiln:7070", "kiln:7071")));
            let ours = main(&pool_pod(&cfg(), "x1", "kiln-x-1", "t", ("http://kiln:7070", "kiln:7071"), &pool, Some(&leg)));
            let (rt, ro) = (theirs.resources.clone().unwrap_or_default(), ours.resources.clone().unwrap_or_default());
            assert_eq!(ours.image, theirs.image, "{id}: image");
            assert_eq!(ours.image.as_deref(), Some(image), "{id}: image");
            assert_eq!(ro.requests, rt.requests, "{id}: requests");
            assert_eq!(ro.requests.as_ref().and_then(|r| r.get("cpu")).map(|q| q.0.as_str()), Some(cpu), "{id}: cpu");
            assert_eq!(ro.requests.as_ref().and_then(|r| r.get("memory")).map(|q| q.0.as_str()), Some(memory), "{id}: memory");
            assert_eq!(ro.limits, rt.limits, "{id}: limits");
            assert_eq!(ro.limits.as_ref().and_then(|r| r.get("memory")).map(|q| q.0.as_str()), Some(memory), "{id}: memory limit");
            assert_eq!(caches(&ours), caches(&theirs), "{id}: caches");
            assert_eq!(caches(&ours).len(), CACHE_ENV.len(), "{id}: caches");
        }
        let plain = job_of("plain")?;
        assert!(!crate::ci::pools::sets_pod(plain), "a job that sets none of them");
        let shared = main(&pool_pod(&cfg(), "x2", "kiln-x-2", "t", ("http://kiln:7070", "kiln:7071"), &pool, None));
        assert_eq!(shared.image.as_deref(), Some("kiln/runner-ubuntu-24.04"));
        let r = shared.resources.unwrap_or_default();
        assert_eq!(r.requests.as_ref().and_then(|r| r.get("cpu")).map(|q| q.0.as_str()), Some("4"));
        assert_eq!(r.limits.as_ref().and_then(|r| r.get("memory")).map(|q| q.0.as_str()), Some("8Gi"));
        Ok(())
    }

    /// A leg that isn't the kiln dialect's keeps the pool's pod: the pool's image and resources,
    /// no toolchain caches but git's, no deadline; one with a `container:` is told its containers
    /// are the pod's.
    #[test]
    fn w13_r07_other_legs_keep_the_pools_pods() {
        let pool = PoolSpec {
            name: "k8s".into(),
            executor: "k8s".into(),
            labels: vec!["k8s".into()],
            image: Some("kiln/runner-ubuntu-24.04".into()),
            resources: Some(kiln_ci::sched::pools::ResourceSpec { cpu: Some("4".into()), memory: Some("8Gi".into()), ..Default::default() }),
            ..PoolSpec::default()
        };
        let env = |p: &Pod| -> Vec<String> { p.spec.clone().unwrap_or_default().containers.first().and_then(|c| c.env.clone()).unwrap_or_default().into_iter().map(|e| e.name).collect() };
        let shared = pool_pod(&cfg(), "x1", "kiln-x-1", "t", ("http://kiln:7070", "kiln:7071"), &pool, None);
        let s = shape(&shared);
        let at = |v: &serde_json::Value, p: &str| v.pointer(p).cloned().unwrap_or_default();
        assert_eq!(at(&s, "/image"), "kiln/runner-ubuntu-24.04");
        assert_eq!(at(&s, "/resources/requests/cpu"), "4");
        assert_eq!(at(&s, "/resources/limits/memory"), "8Gi");
        assert_eq!(at(&s, "/caches"), serde_json::json!([["KILN_GIT_CACHE", "/cache/git"]]));
        assert!(at(&s, "/deadline").is_null());
        assert!(!env(&shared).iter().any(|e| e == "KILN_POD_CONTAINERS"));
        let boxed = LegPod { image: Some("postgres:17".into()), containers: true, ..LegPod::default() };
        let made = pool_pod(&cfg(), "x2", "kiln-x-2", "t", ("http://kiln:7070", "kiln:7071"), &pool, Some(&boxed));
        assert_eq!(at(&shape(&made), "/image"), "postgres:17");
        assert_eq!(at(&shape(&made), "/resources/requests/memory"), "8Gi");
        assert!(env(&made).iter().any(|e| e == "KILN_POD_CONTAINERS"));
    }

    /// An executor's pod as `launch_executor` makes one with a Docker sidecar: the job container and
    /// the engine mount the work folder.
    fn executor() -> Pod {
        let container = |name: &str| Container { name: name.into(), volume_mounts: Some(vec![mount("work", "/work")]), ..Default::default() };
        Pod {
            metadata: ObjectMeta { name: Some("kiln-x-pool-rust-1".into()), annotations: Some(BTreeMap::from([(EXECUTOR_ANNOTATION.to_owned(), "pool-rust-1".to_owned())])), ..Default::default() },
            spec: Some(PodSpec {
                init_containers: Some(vec![Container { name: "kiln-bin".into(), volume_mounts: Some(vec![mount("kiln-bin", "/kiln-bin")]), ..Default::default() }, container("docker")]),
                containers: vec![container("job")],
                volumes: Some(vec![empty_dir("kiln-bin"), empty_dir("work")]),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn mounts(pod: &Value, at: &str) -> Vec<(String, String)> {
        let containers = pod.pointer("/spec/containers").and_then(Value::as_array).into_iter().flatten().chain(pod.pointer("/spec/initContainers").and_then(Value::as_array).into_iter().flatten());
        containers
            .filter_map(|c| {
                let name = c.get("name").and_then(Value::as_str)?;
                let m = c.get("volumeMounts").and_then(Value::as_array)?.iter().find(|m| m.get("mountPath").and_then(Value::as_str) == Some(at))?;
                Some((name.to_owned(), m.get("name").and_then(Value::as_str).unwrap_or_default().to_owned()))
            })
            .collect()
    }

    /// W24-R14: a warm pool's pod keeps its runner's warm workspaces in a folder of the pool's on its
    /// node, `/work/warm` in the job container and the Docker engine both; goes only to a node with
    /// the pool's label; is drawn to the nodes the pool's pods ran on last, the newest most; and is
    /// marked, so its node is remembered.
    #[test]
    fn w24_r14_a_warm_pools_pod_keeps_its_workspaces_on_its_node() -> anyhow::Result<()> {
        let w = warm(&PodWarm { root: Some("/mnt/nvme/kiln/".into()), node_label: Some("kiln.dev/warm=true".into()) }, "rust", &["n2".into(), "n1".into()]);
        let mut pod = executor();
        apply_warm(&mut pod, &w);
        let v = serde_json::to_value(&pod)?;
        assert_eq!(v.pointer("/spec/volumes/2"), Some(&json!({ "name": "warm", "hostPath": { "path": "/mnt/nvme/kiln/rust", "type": "DirectoryOrCreate" } })), "{v}");
        assert_eq!(mounts(&v, "/work/warm"), [("job".to_owned(), "warm".to_owned()), ("docker".to_owned(), "warm".to_owned())], "{v}");
        assert_eq!(mounts(&v, "/kiln-bin"), [("kiln-bin".to_owned(), "kiln-bin".to_owned())], "the binary's copy keeps none: {v}");
        let affinity = v.pointer("/spec/affinity/nodeAffinity").cloned().unwrap_or_default();
        assert_eq!(
            affinity.pointer("/requiredDuringSchedulingIgnoredDuringExecution/nodeSelectorTerms"),
            Some(&json!([{ "matchExpressions": [{ "key": "kiln.dev/warm", "operator": "In", "values": ["true"] }] }])),
            "{affinity}"
        );
        assert_eq!(
            affinity.pointer("/preferredDuringSchedulingIgnoredDuringExecution"),
            Some(&json!([
                { "weight": 100, "preference": { "matchFields": [{ "key": "metadata.name", "operator": "In", "values": ["n2"] }] } },
                { "weight": 80, "preference": { "matchFields": [{ "key": "metadata.name", "operator": "In", "values": ["n1"] }] } },
            ])),
            "{affinity}"
        );
        assert_eq!(v.pointer("/metadata/annotations/kiln.dev~1warm"), Some(&json!("rust")));
        assert_eq!(v.pointer("/metadata/annotations/kiln.dev~1executor"), Some(&json!("pool-rust-1")), "its own annotations stay");
        Ok(())
    }

    /// W24-R14: with no label and no nodes yet, a warm pool's pod gets its folder and no node terms;
    /// a label alone is a key that must be there; a label's requirement joins a required term the
    /// pod has; a pool's name of dots is no folder's.
    #[test]
    fn w24_r14_a_warm_pods_node_terms_only_as_the_pool_says() -> anyhow::Result<()> {
        let mut pod = executor();
        apply_warm(&mut pod, &warm(&PodWarm::default(), "pods", &[]));
        let v = serde_json::to_value(&pod)?;
        assert_eq!(v.pointer("/spec/volumes/2/hostPath/path"), Some(&json!("/var/lib/kiln/warm/pods")));
        assert!(v.pointer("/spec/affinity").is_none(), "{v}");
        let label = warm(&PodWarm { node_label: Some("nvme".into()), ..PodWarm::default() }, "pods", &[]);
        assert_eq!(serde_json::to_value(&label.required)?, json!({ "matchExpressions": [{ "key": "nvme", "operator": "Exists" }] }));
        let mut held = executor();
        if let Some(spec) = held.spec.as_mut() {
            let zone = NodeSelectorTerm { match_expressions: Some(vec![NodeSelectorRequirement { key: "zone".into(), operator: "In".into(), values: Some(vec!["a".into()]) }]), ..Default::default() };
            spec.affinity = Some(Affinity { node_affinity: Some(NodeAffinity { required_during_scheduling_ignored_during_execution: Some(NodeSelector { node_selector_terms: vec![zone] }), ..Default::default() }), ..Default::default() });
        }
        apply_warm(&mut held, &label);
        let v = serde_json::to_value(&held)?;
        assert_eq!(
            v.pointer("/spec/affinity/nodeAffinity/requiredDuringSchedulingIgnoredDuringExecution/nodeSelectorTerms"),
            Some(&json!([{ "matchExpressions": [{ "key": "zone", "operator": "In", "values": ["a"] }, { "key": "nvme", "operator": "Exists" }] }])),
            "{v}"
        );
        assert_eq!(warm(&PodWarm::default(), "..", &[]).volume.host_path.map(|h| h.path).as_deref(), Some("/var/lib/kiln/warm/__"));
        Ok(())
    }

    /// W24-R14 on the pod [`pool_pod`] makes: a warm pool's pod with a Docker engine mounts its
    /// node's folder at `/work/warm` in the job container and in the engine, beside the pool's own
    /// volumes, and is drawn to the node its pods ran on last.
    #[test]
    fn w24_r14_a_warm_pools_pod_from_pool_pod() -> anyhow::Result<()> {
        let pool = PoolSpec {
            name: "rust".into(),
            executor: "k8s".into(),
            labels: vec!["rust".into()],
            docker: Some(kiln_ci::sched::pools::PodDocker::default()),
            warm: Some(PodWarm::default()),
            ..PoolSpec::default()
        };
        let mut pod = pool_pod(&cfg(), "x1", "kiln-x-1", "t", ("http://kiln:7070", "kiln:7071"), &pool, None);
        let preferred = |v: &Value| v.pointer("/spec/affinity/nodeAffinity/preferredDuringSchedulingIgnoredDuringExecution").and_then(Value::as_array).cloned().unwrap_or_default();
        let before = preferred(&serde_json::to_value(&pod)?);
        apply_warm(&mut pod, &warm(&PodWarm::default(), &pool.name, &["n1".into()]));
        let v = serde_json::to_value(&pod)?;
        assert_eq!(mounts(&v, "/work/warm"), [("job".to_owned(), "warm".to_owned()), ("docker".to_owned(), "warm".to_owned())], "{v}");
        let volumes = v.pointer("/spec/volumes").and_then(Value::as_array).cloned().unwrap_or_default();
        let named = |n: &str| volumes.iter().find(|x| x.get("name").and_then(Value::as_str) == Some(n)).cloned();
        assert_eq!(named("warm"), Some(json!({ "name": "warm", "hostPath": { "path": "/var/lib/kiln/warm/rust", "type": "DirectoryOrCreate" } })), "{v}");
        assert!(named("work").is_some() && named("cache").is_some(), "the pool's own volumes stay: {v}");
        let after = preferred(&v);
        assert!(before.iter().all(|t| after.contains(t)), "the pod's own node preferences stay: {v}");
        assert!(after.iter().any(|t| t.pointer("/preference/matchFields/0/values") == Some(&json!(["n1"]))), "drawn to the node it ran on last: {v}");
        Ok(())
    }

    /// W24-R14: a warm pool remembers the nodes its pods ran on, newest first, each once, the four
    /// newest kept.
    #[test]
    fn w24_r14_a_warm_pool_remembers_its_nodes_newest_first() {
        let mut nodes = HashMap::new();
        for n in ["a", "b", "c", "a", "d", "e"] {
            remember(&mut nodes, "rust", n);
        }
        remember(&mut nodes, "go", "z");
        assert_eq!(nodes.get("rust").cloned().unwrap_or_default(), ["e", "d", "a", "c"]);
        assert_eq!(nodes.get("go").cloned().unwrap_or_default(), ["z"]);
    }
}
