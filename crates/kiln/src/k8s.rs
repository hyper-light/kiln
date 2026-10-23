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

        let image = spec
            .image
            .clone()
            .unwrap_or_else(|| crate::pipeline::toolchain_image(spec.toolchain.as_deref()).to_owned());
        let server_url = self.cfg.server_url.clone().unwrap_or_else(|| hub.cfg.server.public_url.clone());
        let server_quic = self.cfg.server_quic.clone().unwrap_or_else(|| hub.cfg.server.quic_advertise.clone());
        let labels: Vec<String> = spec.labels.iter().cloned().chain(["k8s".to_owned()]).collect();

        let mut env = vec![
            env_var("KILN_JOIN_TOKEN", &token),
            env_var("KILN_SERVER", &server_url),
            env_var("KILN_QUIC", &server_quic),
        ];
        let mut mounts = vec![mount("kiln-bin", "/kiln-bin"), mount("work", "/work")];
        let mut volumes = vec![empty_dir("kiln-bin"), empty_dir("work")];
        if let Some(pvc) = &self.cfg.cache_pvc {
            mounts.push(mount("cache", "/cache"));
            volumes.push(Volume {
                name: "cache".into(),
                persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                    claim_name: pvc.clone(),
                    read_only: Some(false),
                }),
                ..Default::default()
            });
            for (k, v) in [
                ("CARGO_HOME", "/cache/cargo"),
                ("GOMODCACHE", "/cache/go/mod"),
                ("GOCACHE", "/cache/go/build"),
                ("UV_CACHE_DIR", "/cache/uv"),
                ("npm_config_cache", "/cache/npm"),
                ("BUN_INSTALL_CACHE_DIR", "/cache/bun"),
                ("KILN_GIT_CACHE", "/cache/git"),
            ] {
                env.push(env_var(k, v));
            }
        }

        let q = |s: &str| Quantity(s.to_owned());
        let cpu = spec.resources.cpu.clone().unwrap_or_else(|| self.cfg.default_cpu.clone());
        let mem = spec.resources.memory.clone().unwrap_or_else(|| self.cfg.default_memory.clone());
        let resources = ResourceRequirements {
            requests: Some(BTreeMap::from([("cpu".to_owned(), q(&cpu)), ("memory".to_owned(), q(&mem))])),
            limits: Some(BTreeMap::from([("memory".to_owned(), q(&mem))])),
            ..Default::default()
        };

        let pod = Pod {
            metadata: ObjectMeta {
                name: Some(name.clone()),
                namespace: Some(self.cfg.namespace.clone()),
                labels: Some(BTreeMap::from([
                    ("app.kubernetes.io/managed-by".to_owned(), "kiln".to_owned()),
                    ("app.kubernetes.io/name".to_owned(), "kiln-job".to_owned()),
                    (JOB_LABEL.to_owned(), job.id.clone()),
                    ("kiln.dev/run".to_owned(), run.id.clone()),
                ])),
                annotations: Some(BTreeMap::from([
                    ("kiln.dev/repo".to_owned(), run.repo_name.clone()),
                    ("kiln.dev/job-key".to_owned(), job.key.clone()),
                ])),
                ..Default::default()
            },
            spec: Some(PodSpec {
                restart_policy: Some("Never".into()),
                priority_class_name: self.cfg.priority_class.clone(),
                // Prefer worker nodes: the control-plane node keeps room for the platform (kiln,
                // ArgoCD, Prometheus), so restarting those rarely needs to preempt a build.
                affinity: Some(Affinity {
                    node_affinity: Some(NodeAffinity {
                        preferred_during_scheduling_ignored_during_execution: Some(vec![PreferredSchedulingTerm {
                            weight: 100,
                            preference: NodeSelectorTerm {
                                match_expressions: Some(vec![NodeSelectorRequirement {
                                    key: "node-role.kubernetes.io/control-plane".into(),
                                    operator: "DoesNotExist".into(),
                                    values: None,
                                }]),
                                ..Default::default()
                            },
                        }]),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                service_account_name: Some(self.cfg.service_account.clone()),
                automount_service_account_token: Some(false),
                active_deadline_seconds: Some(i64::from(spec.timeout_minutes) * 60 + 900),
                termination_grace_period_seconds: Some(20),
                node_selector: (!self.cfg.node_selector.is_empty()).then(|| self.cfg.node_selector.clone()),
                init_containers: Some(vec![Container {
                    name: "kiln-bin".into(),
                    image: Some(self.cfg.agent_image.clone()),
                    image_pull_policy: Some(self.cfg.image_pull_policy.clone()),
                    command: Some(vec!["cp".into(), "/usr/local/bin/kiln".into(), "/kiln-bin/kiln".into()]),
                    volume_mounts: Some(vec![mount("kiln-bin", "/kiln-bin")]),
                    // Explicit requests: a namespace ResourceQuota rejects pods with any container
                    // lacking them (init containers included).
                    resources: Some(ResourceRequirements {
                        requests: Some(BTreeMap::from([("cpu".to_owned(), q("50m")), ("memory".to_owned(), q("32Mi"))])),
                        limits: Some(BTreeMap::from([("memory".to_owned(), q("128Mi"))])),
                        ..Default::default()
                    }),
                    ..Default::default()
                }]),
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
                        name.clone(),
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
        };
        self.api.create_pod(&pod).await?;
        Ok(name)
    }

    /// A `k8s` pool's executor (docs/ci.md W13-R07): a pod of the pool's image (else the
    /// toolchain's) whose own runner (`kiln agent --ephemeral`, copied in by the init container as
    /// a job's pod has it) enrolls with a one-time token bound to the executor and the pod's name,
    /// takes the pool's one leg it's given, and leaves. The pool's `resources` are its requests
    /// (memory its limit too), and the cache claim is mounted as a job pod's is.
    ///
    /// A pod made for a leg (`pod`) holds that leg's containers: its `container:` is the job
    /// container (its image, its environment), and each of its `services:` a native sidecar (an
    /// init container that keeps running) with its ports, on the pod's `localhost`, which
    /// `hostAliases` name it by too. The runner inside is told so (`KILN_POD_CONTAINERS`, and
    /// `KILN_POD_SERVICES` with each service's ports) and starts no containers of its own.
    pub async fn launch_executor(&self, hub: &Hub, executor: &str, pool: &kiln_ci::sched::pools::PoolSpec, pod: Option<&crate::ci::pools::LegPod>) -> anyhow::Result<String> {
        let name = executor_pod(executor);
        let token = crate::util::random_token();
        let ttl = Duration::from_secs(u64::from(self.cfg.start_timeout_seconds) + 120);
        hub.executor_tokens.lock().insert(token.clone(), (executor.to_owned(), name.clone(), Instant::now() + ttl));
        let image = pod.and_then(|p| p.image.clone()).or_else(|| pool.image.clone().filter(|i| !i.trim().is_empty())).unwrap_or_else(|| crate::pipeline::toolchain_image(None).to_owned());
        let server_url = self.cfg.server_url.clone().unwrap_or_else(|| hub.cfg.server.public_url.clone());
        let server_quic = self.cfg.server_quic.clone().unwrap_or_else(|| hub.cfg.server.quic_advertise.clone());
        let mut env = vec![env_var("KILN_JOIN_TOKEN", &token), env_var("KILN_SERVER", &server_url), env_var("KILN_QUIC", &server_quic), env_var("KILN_POD", "1")];
        // The leg's containers are the pod's.
        let services = pod.map(|p| p.services.clone()).unwrap_or_default();
        if let Some(p) = pod {
            env.extend(p.env.iter().map(|(k, v)| env_var(k, v)));
            env.push(env_var("KILN_POD_CONTAINERS", "1"));
            let ports: serde_json::Map<String, serde_json::Value> = services.iter().map(|s| (s.id.clone(), serde_json::json!(s.ports))).collect();
            env.push(env_var("KILN_POD_SERVICES", &serde_json::Value::Object(ports).to_string()));
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
        if let Some(pvc) = &self.cfg.cache_pvc {
            mounts.push(mount("cache", "/cache"));
            volumes.push(Volume { name: "cache".into(), persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource { claim_name: pvc.clone(), read_only: Some(false) }), ..Default::default() });
            env.push(env_var("KILN_GIT_CACHE", "/cache/git"));
        }
        let q = |s: &str| Quantity(s.to_owned());
        let r = pool.resources.clone().unwrap_or_default();
        let cpu = r.cpu.clone().unwrap_or_else(|| self.cfg.default_cpu.clone());
        let mem = r.memory.clone().unwrap_or_else(|| self.cfg.default_memory.clone());
        let mut requests = BTreeMap::from([("cpu".to_owned(), q(&cpu)), ("memory".to_owned(), q(&mem))]);
        if let Some(disk) = &r.disk {
            requests.insert("ephemeral-storage".to_owned(), q(disk));
        }
        let resources = ResourceRequirements { requests: Some(requests), limits: Some(BTreeMap::from([("memory".to_owned(), q(&mem))])), ..Default::default() };
        let pod = Pod {
            metadata: ObjectMeta {
                name: Some(name.clone()),
                namespace: Some(self.cfg.namespace.clone()),
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
                priority_class_name: self.cfg.priority_class.clone(),
                service_account_name: Some(self.cfg.service_account.clone()),
                automount_service_account_token: Some(false),
                termination_grace_period_seconds: Some(20),
                node_selector: (!self.cfg.node_selector.is_empty()).then(|| self.cfg.node_selector.clone()),
                host_aliases,
                init_containers: Some(
                    std::iter::once(Container {
                        name: "kiln-bin".into(),
                        image: Some(self.cfg.agent_image.clone()),
                        image_pull_policy: Some(self.cfg.image_pull_policy.clone()),
                        command: Some(vec!["cp".into(), "/usr/local/bin/kiln".into(), "/kiln-bin/kiln".into()]),
                        volume_mounts: Some(vec![mount("kiln-bin", "/kiln-bin")]),
                        resources: Some(ResourceRequirements {
                            requests: Some(BTreeMap::from([("cpu".to_owned(), q("50m")), ("memory".to_owned(), q("32Mi"))])),
                            limits: Some(BTreeMap::from([("memory".to_owned(), q("128Mi"))])),
                            ..Default::default()
                        }),
                        ..Default::default()
                    })
                    .chain(docker.into_iter().flatten())
                    .chain(sidecars)
                    .collect(),
                ),
                containers: vec![Container {
                    name: "job".into(),
                    image: Some(image),
                    image_pull_policy: Some("IfNotPresent".into()),
                    command: Some(vec![
                        "/kiln-bin/kiln".into(),
                        "agent".into(),
                        "--ephemeral".into(),
                        "--name".into(),
                        name.clone(),
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
        };
        self.api.create_pod(&pod).await?;
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

    /// Delete finished pods; fail jobs whose pods are stuck unable to start.
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
                let job_id = pod.metadata.labels.as_ref().and_then(|l| l.get(JOB_LABEL)).cloned().unwrap_or_default();
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
