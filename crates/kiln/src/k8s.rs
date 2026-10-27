//! Kubernetes executor. A queued `runs-on: [k8s]` job gets its own pod:
//!
//! * an initContainer from the kiln image copies the static `kiln` binary into an `emptyDir`,
//! * the job container (any image: the toolchain preset or `image:`) runs
//!   `kiln agent --ephemeral --job <id>`, which enrolls with a one-time token, connects back over
//!   QUIC/mTLS exactly like a bare-metal agent, runs the one job and exits,
//! * a reaper deletes finished pods and fails jobs whose pods cannot start (bad image, quota).
//!
//! The pod needs no git and no GitHub access: source comes from the server's mirror as a tarball.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use k8s_openapi::api::core::v1::{
    Affinity, Container, ContainerPort, EnvVar, ExecAction, HostAlias, NodeAffinity, NodeSelectorRequirement, NodeSelectorTerm, PersistentVolumeClaimVolumeSource, Pod, PodSpec,
    PreferredSchedulingTerm, Probe, ResourceRequirements, SecurityContext, Volume, VolumeMount,
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
pub struct ExecutorPod {
    pub name: String,
    pub executor: String,
    pub phase: String,
    /// Why it can't start, when it can't.
    pub stuck: Option<String>,
    pub deleting: bool,
}

pub struct K8s {
    api: KubeApi,
    cfg: KCfg,
}

impl K8s {
    pub async fn connect(cfg: &KCfg) -> anyhow::Result<Self> {
        let api = KubeApi::discover(&cfg.namespace)?;
        // Probe permissions now so a broken RBAC setup is reported at startup.
        api.list_pods(MANAGED).await?;
        Ok(Self { api, cfg: cfg.clone() })
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
    pub async fn launch_executor(&self, hub: &Hub, executor: &str, pool: &kiln_ci::sched::pools::PoolSpec, pod: Option<&crate::ci::pools::LegPod>) -> anyhow::Result<String> {
        let name = executor_pod(executor);
        let token = crate::util::random_token();
        let ttl = Duration::from_secs(u64::from(self.cfg.start_timeout_seconds) + 120);
        hub.executor_tokens.lock().insert(token.clone(), (executor.to_owned(), name.clone(), Instant::now() + ttl));
        let server_url = self.cfg.server_url.clone().unwrap_or_else(|| hub.cfg.server.public_url.clone());
        let server_quic = self.cfg.server_quic.clone().unwrap_or_else(|| hub.cfg.server.quic_advertise.clone());
        let made = pool_pod(&self.cfg, executor, &name, &token, (&server_url, &server_quic), pool, pod);
        self.api.create_pod(&made).await?;
        Ok(name)
    }

    /// The `k8s` pools' executor pods there are.
    pub async fn executor_pods(&self) -> anyhow::Result<Vec<ExecutorPod>> {
        let list = self.api.list_pods(EXECUTOR_PODS).await?;
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
                Some(ExecutorPod { name, executor, phase: status.phase.unwrap_or_default(), stuck, deleting: p.metadata.deletion_timestamp.is_some() })
            })
            .collect())
    }

    pub async fn delete_pod(&self, name: &str) -> anyhow::Result<()> {
        self.api.delete_pod(name).await
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
                    if let Some(n) = p.metadata.name
                        && let Err(e) = self.api.delete_pod(&n).await {
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
                    if let Err(e) = self.api.delete_pod(&name).await {
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
                        let _ = self.api.delete_pod(&name).await;
                        hub.launching.lock().remove(&job_id);
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
    use kiln_ci::sched::pools::PoolSpec;

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
}
