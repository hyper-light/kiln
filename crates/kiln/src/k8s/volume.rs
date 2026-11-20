//! An agent's pod's workspace on kiln's Kubernetes engine (docs/agentic.md W10): the PersistentVolume, the claim and the
//! pod's volume through which kubelet mounts its home runner's Slates export of the agent's volume, over RPC-with-TLS,
//! never a copy of the folder on a node. Pure builders, as a job's pod is (`k8s::job_pod`): what the server sends the
//! cluster, made from the export the home answered ([`PodExport`]: `slates_api::Instance::export`'s path, the host and
//! base port `[slates_fleet.nodes]` lists for the home, and the uid and gid that own the folder there).
//!
//! **Why a PersistentVolume.** A pod's own `nfs` volume takes no mount options, so it can't ask for `xprtsec=mtls`; a CSI
//! driver's node plugin needs a privileged container. kubelet mounts a PersistentVolume with the options it names:
//! `nfsvers=4.2` (Slates serves NFSv4.2), `xprtsec=mtls` (RFC 9289: the node's `tlshd` presents its certificate from
//! kiln's CA, the export's half of the capability), `port=` the home's listed base port, `nosuid` and `nodev`.
//!
//! **One claim, reserved.** The claim binds the PersistentVolume by name (`volumeName`, `storageClassName: ""`, so no
//! provisioner answers it), and the PersistentVolume names the claim back (`claimRef`), so no other claim can bind it
//! (Kubernetes, "Reserving a PersistentVolume"). Its reclaim policy is `Retain`: kiln deletes both at the agent's end,
//! and its home detaches the export's attachment, after which its path reaches nothing. Its capacity is a formality NFS
//! never enforces ([`CAPACITY`]): the volume's bounds are Slates' own.
//!
//! **As the folder's owner.** The export's attachment serves one uid (Slates' A-115), so the pod runs as the folder's
//! owner (`runAsUser`, `runAsGroup`), and permission bits mean in the pod what they mean in the folder. The pod mounts the
//! claim at the folder's own path, as an isolated session's guest has it (W8), so nothing is translated.
//!
//! The path carries the export's token, a bearer secret: [`PodExport`]'s `Debug` hides it, and reading the
//! PersistentVolume takes the cluster-wide right to read PersistentVolumes.

use std::collections::BTreeMap;
use std::fmt;

use k8s_openapi::api::core::v1::{
    Container, NFSVolumeSource, ObjectReference, PersistentVolume, PersistentVolumeClaim, PersistentVolumeClaimSpec, PersistentVolumeClaimVolumeSource, PersistentVolumeSpec, Pod,
    PodSecurityContext, PodSpec, ResourceRequirements, Volume, VolumeMount, VolumeResourceRequirements,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

/// Format: the label every object kiln makes for an agent's workspace carries, naming the agent.
pub const AGENT_LABEL: &str = "kiln.dev/agent";
/// Format: the capacity the PersistentVolume states and the claim asks: required by the API, never enforced over NFS.
pub const CAPACITY: &str = "1Gi";
/// Format: the volume's name in the pod.
pub const POD_VOLUME: &str = "workspace";
/// Format: the access the PersistentVolume offers and the claim asks: one node's pods, as one pod is.
const ACCESS: &str = "ReadWriteOnce";

/// What the home answered for a pod's export of its agent's volume (W10, step 2).
#[derive(Clone, PartialEq, Eq)]
pub struct PodExport {
    /// The home's host as `[slates_fleet.nodes]` lists it, which the node's client verifies the export's certificate by.
    pub host: String,
    /// The home's listed base port, where its export answers.
    pub port: u16,
    /// The export's capability path, `/<name>@<attachment>.<token>` (`slates_api::mount::export_path`).
    pub path: String,
    /// Read-only: the export refuses the client's changes.
    pub read_only: bool,
    /// The uid and gid that own the folder at home, which the export serves and the pod runs as.
    pub uid: u32,
    pub gid: u32,
    /// The folder's own path, where the pod mounts the claim.
    pub folder: String,
}

/// Shows the path without its token, a bearer secret.
impl fmt::Debug for PodExport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let named = self.path.rsplit_once('.').map_or("<a path>", |(name, _)| name);
        f.debug_struct("PodExport")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("path", &format_args!("{named}.<token>"))
            .field("read_only", &self.read_only)
            .field("uid", &self.uid)
            .field("gid", &self.gid)
            .field("folder", &self.folder)
            .finish()
    }
}

/// Why an export can't be made into a pod's workspace ([`objects`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unbuildable {
    /// No host to mount from.
    NoHost,
    /// A port of 0: the home's export listens on its listed base port.
    NoPort,
    /// A path no export answers: not absolute, or holding a comma, a space or a control character, which a mount option
    /// list or kubelet's mount would split or mangle.
    Path,
    /// A folder the pod can't mount the claim at: not absolute, or holding a `..`.
    Folder { folder: String },
    /// A name for the objects that isn't a DNS label.
    Name { name: String },
}

impl fmt::Display for Unbuildable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoHost => f.write_str("the export names no host to mount it from"),
            Self::NoPort => f.write_str("the export names port 0"),
            Self::Path => f.write_str("the export's path isn't one a mount can carry"),
            Self::Folder { folder } => write!(f, "{folder:?} isn't a folder a pod can mount its workspace at"),
            Self::Name { name } => write!(f, "{name:?} isn't a DNS label (lowercase letters, digits and '-', at most 63)"),
        }
    }
}

impl std::error::Error for Unbuildable {}

/// What the server sends the cluster for a pod's workspace: the PersistentVolume, its claim, and the pod's part (its
/// volume, its container's mount, and the identity it runs as).
#[derive(Debug, Clone, PartialEq)]
pub struct Objects {
    pub volume: PersistentVolume,
    pub claim: PersistentVolumeClaim,
    pub pod_volume: Volume,
    pub mount: VolumeMount,
    pub security: PodSecurityContext,
}

/// Whether `name` is a DNS label (RFC 1123), as Kubernetes names these objects.
fn label(name: &str) -> bool {
    !name.is_empty() && name.len() <= 63 && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') && !name.starts_with('-') && !name.ends_with('-')
}

/// The objects for agent `agent`'s pod's workspace over `export` (see the module's notes): the PersistentVolume and the
/// claim both named `name` (a DNS label), the claim in `namespace`, kiln's jobs namespace; or why they can't be built.
pub fn objects(name: &str, namespace: &str, agent: &str, export: &PodExport) -> Result<Objects, Unbuildable> {
    if !label(name) {
        return Err(Unbuildable::Name { name: name.to_owned() });
    }
    if export.host.trim().is_empty() {
        return Err(Unbuildable::NoHost);
    }
    if export.port == 0 {
        return Err(Unbuildable::NoPort);
    }
    if !export.path.starts_with('/') || export.path.chars().any(|c| c == ',' || c.is_whitespace() || c.is_control()) {
        return Err(Unbuildable::Path);
    }
    let folder = std::path::Path::new(&export.folder);
    if !folder.is_absolute() || folder.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
        return Err(Unbuildable::Folder { folder: export.folder.clone() });
    }
    let labels = BTreeMap::from([("app.kubernetes.io/managed-by".to_owned(), "kiln".to_owned()), (AGENT_LABEL.to_owned(), agent.to_owned())]);
    let capacity = BTreeMap::from([("storage".to_owned(), Quantity(CAPACITY.to_owned()))]);
    let volume = PersistentVolume {
        metadata: ObjectMeta { name: Some(name.to_owned()), labels: Some(labels.clone()), ..ObjectMeta::default() },
        spec: Some(PersistentVolumeSpec {
            access_modes: Some(vec![ACCESS.to_owned()]),
            capacity: Some(capacity.clone()),
            claim_ref: Some(ObjectReference { kind: Some("PersistentVolumeClaim".to_owned()), namespace: Some(namespace.to_owned()), name: Some(name.to_owned()), ..ObjectReference::default() }),
            mount_options: Some(["nfsvers=4.2".to_owned(), "xprtsec=mtls".to_owned(), format!("port={}", export.port), "nosuid".to_owned(), "nodev".to_owned()].to_vec()),
            nfs: Some(NFSVolumeSource { server: export.host.trim().to_owned(), path: export.path.clone(), read_only: Some(export.read_only) }),
            persistent_volume_reclaim_policy: Some("Retain".to_owned()),
            storage_class_name: Some(String::new()),
            ..PersistentVolumeSpec::default()
        }),
        ..PersistentVolume::default()
    };
    let claim = PersistentVolumeClaim {
        metadata: ObjectMeta { name: Some(name.to_owned()), namespace: Some(namespace.to_owned()), labels: Some(labels), ..ObjectMeta::default() },
        spec: Some(PersistentVolumeClaimSpec {
            access_modes: Some(vec![ACCESS.to_owned()]),
            resources: Some(VolumeResourceRequirements { requests: Some(capacity), ..VolumeResourceRequirements::default() }),
            storage_class_name: Some(String::new()),
            volume_name: Some(name.to_owned()),
            ..PersistentVolumeClaimSpec::default()
        }),
        ..PersistentVolumeClaim::default()
    };
    let pod_volume = Volume {
        name: POD_VOLUME.to_owned(),
        persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource { claim_name: name.to_owned(), read_only: Some(export.read_only) }),
        ..Volume::default()
    };
    let mount = VolumeMount { name: POD_VOLUME.to_owned(), mount_path: export.folder.clone(), read_only: Some(export.read_only), ..VolumeMount::default() };
    let security = PodSecurityContext { run_as_user: Some(i64::from(export.uid)), run_as_group: Some(i64::from(export.gid)), ..PodSecurityContext::default() };
    Ok(Objects { volume, claim, pod_volume, mount, security })
}

/// Format: the label an agent's pod carries (`app.kubernetes.io/name`), beside a job's `kiln-job`.
pub const AGENT_POD: &str = "kiln-agent";

/// Agent session `session`'s pod (docs/agentic.md W10), named `name` (a DNS label) in kiln's jobs namespace: its harness's
/// image (`image`) running kiln's own runner for that one session (`kiln agent --ephemeral --session`, copied in by the
/// init container a job's pod has, enrolling with the one-time `token` and connecting back to the server at its URLs, as
/// a job's pod does), its workspace mounted at the folder's own path from the claim `objects` names, run as the folder's
/// owner, as `objects` says; the server's default CPU and memory (memory its limit too); and no deadline, as an agent's
/// session has none: it ends as its session does (the runner leaves, and its pod's container with it).
pub fn agent_pod(cfg: &crate::config::Kubernetes, session: &str, name: &str, image: &str, token: &str, (server_url, server_quic): (&str, &str), objects: &Objects) -> Pod {
    let env = vec![super::env_var("KILN_JOIN_TOKEN", token), super::env_var("KILN_SERVER", server_url), super::env_var("KILN_QUIC", server_quic), super::field_env("KILN_POD_UID", "metadata.uid")];
    let q = |s: &str| Quantity(s.to_owned());
    let resources = ResourceRequirements {
        requests: Some(BTreeMap::from([("cpu".to_owned(), q(&cfg.default_cpu)), ("memory".to_owned(), q(&cfg.default_memory))])),
        limits: Some(BTreeMap::from([("memory".to_owned(), q(&cfg.default_memory))])),
        ..ResourceRequirements::default()
    };
    let labels = BTreeMap::from([("app.kubernetes.io/managed-by".to_owned(), "kiln".to_owned()), ("app.kubernetes.io/name".to_owned(), AGENT_POD.to_owned()), (AGENT_LABEL.to_owned(), session.to_owned())]);
    Pod {
        metadata: ObjectMeta { name: Some(name.to_owned()), namespace: Some(cfg.namespace.clone()), labels: Some(labels), ..ObjectMeta::default() },
        spec: Some(PodSpec {
            restart_policy: Some("Never".into()),
            priority_class_name: cfg.priority_class.clone(),
            service_account_name: Some(cfg.service_account.clone()),
            automount_service_account_token: Some(false),
            termination_grace_period_seconds: Some(20),
            node_selector: (!cfg.node_selector.is_empty()).then(|| cfg.node_selector.clone()),
            security_context: Some(objects.security.clone()),
            init_containers: Some(vec![super::kiln_bin(cfg)]),
            containers: vec![Container {
                name: "agent".into(),
                image: Some(image.to_owned()),
                image_pull_policy: Some("IfNotPresent".into()),
                command: Some(["/kiln-bin/kiln", "agent", "--ephemeral", "--session", session, "--name", name, "--work-dir", "/work", "--slots", "1"].map(str::to_owned).to_vec()),
                env: Some(env),
                resources: Some(resources),
                volume_mounts: Some(vec![super::mount("kiln-bin", "/kiln-bin"), super::mount("work", "/work"), objects.mount.clone()]),
                working_dir: Some(objects.mount.mount_path.clone()),
                ..Container::default()
            }],
            volumes: Some(vec![super::empty_dir("kiln-bin"), super::empty_dir("work"), objects.pod_volume.clone()]),
            ..PodSpec::default()
        }),
        ..Pod::default()
    }
}

/// The cluster's calls a pod's workspace takes ([`make`], [`unmake`]): kiln's own client (`crate::kubeapi::KubeApi`), or
/// a stand-in, so the order is kiln's and is tested.
pub trait Cluster {
    fn create_volume(&self, v: &PersistentVolume) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;
    fn create_claim(&self, c: &PersistentVolumeClaim) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;
    fn delete_claim(&self, name: &str) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;
    fn delete_volume(&self, name: &str) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;
}

impl Cluster for crate::kubeapi::KubeApi {
    async fn create_volume(&self, v: &PersistentVolume) -> anyhow::Result<()> {
        self.create_persistent_volume(v).await
    }
    async fn create_claim(&self, c: &PersistentVolumeClaim) -> anyhow::Result<()> {
        crate::kubeapi::KubeApi::create_claim(self, c).await
    }
    async fn delete_claim(&self, name: &str) -> anyhow::Result<()> {
        crate::kubeapi::KubeApi::delete_claim(self, name).await
    }
    async fn delete_volume(&self, name: &str) -> anyhow::Result<()> {
        self.delete_persistent_volume(name).await
    }
}

/// Makes a pod's workspace in the cluster: the PersistentVolume first, reserved for its claim, then the claim, which binds
/// it by name. A claim the cluster refuses takes its PersistentVolume back down: nothing of a refused workspace is left.
pub async fn make(cluster: &impl Cluster, objects: &Objects) -> anyhow::Result<()> {
    cluster.create_volume(&objects.volume).await?;
    if let Err(e) = cluster.create_claim(&objects.claim).await {
        if let Some(name) = objects.volume.metadata.name.as_deref()
            && let Err(gone) = cluster.delete_volume(name).await
        {
            tracing::warn!(volume = name, error = %format!("{gone:#}"), "a refused pod workspace's PersistentVolume stayed");
        }
        return Err(e);
    }
    Ok(())
}

/// Takes a pod's workspace `name` down, once its pod is gone: the claim, then the PersistentVolume (kubelet keeps a
/// claim's volume while a pod uses it). Each is asked, the PersistentVolume too where the claim wasn't taken down; one
/// already gone is no error. The first that failed is said.
pub async fn unmake(cluster: &impl Cluster, name: &str) -> anyhow::Result<()> {
    let claim = cluster.delete_claim(name).await;
    let volume = cluster.delete_volume(name).await;
    claim.and(volume)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// `v`'s field `key`, null where it has none.
    fn at(v: &serde_json::Value, key: &str) -> serde_json::Value {
        v.get(key).cloned().unwrap_or_default()
    }

    /// Format: an export's capability path, its token after the last dot.
    const PATH: &str = "/kiln-a@7.4f1c2a9e0b";

    fn export() -> PodExport {
        PodExport { host: "10.0.0.1".to_owned(), port: 20049, path: PATH.to_owned(), read_only: false, uid: 501, gid: 20, folder: "/Users/u/src/kiln".to_owned() }
    }

    /// W10. Do: build a pod's workspace over a home's export. Expect a PersistentVolume kubelet mounts over RPC-with-TLS at
    /// the home's listed host and port with the export's path, reserved for its claim alone and kept until kiln deletes
    /// it; a claim that binds it by name with no provisioner; and the pod mounting the claim at the folder's own path, as
    /// the folder's owner.
    #[test]
    fn w10_a_pods_workspace_is_its_homes_export_mounted_over_rpc_with_tls_reserved_for_its_claim() -> anyhow::Result<()> {
        let built = objects("kiln-ws-a", "kiln-jobs", "01a1f7", &export())?;
        let volume = serde_json::to_value(&built.volume)?;
        assert_eq!(
            at(&volume, "spec"),
            json!({
                "accessModes": ["ReadWriteOnce"],
                "capacity": {"storage": "1Gi"},
                "claimRef": {"kind": "PersistentVolumeClaim", "namespace": "kiln-jobs", "name": "kiln-ws-a"},
                "mountOptions": ["nfsvers=4.2", "xprtsec=mtls", "port=20049", "nosuid", "nodev"],
                "nfs": {"server": "10.0.0.1", "path": PATH, "readOnly": false},
                "persistentVolumeReclaimPolicy": "Retain",
                "storageClassName": "",
            })
        );
        assert_eq!(at(&volume, "metadata"), json!({"name": "kiln-ws-a", "labels": {"app.kubernetes.io/managed-by": "kiln", "kiln.dev/agent": "01a1f7"}}));
        let claim = serde_json::to_value(&built.claim)?;
        assert_eq!(at(&at(&claim, "metadata"), "namespace"), json!("kiln-jobs"));
        assert_eq!(at(&claim, "spec"), json!({"accessModes": ["ReadWriteOnce"], "resources": {"requests": {"storage": "1Gi"}}, "storageClassName": "", "volumeName": "kiln-ws-a"}));
        assert_eq!(serde_json::to_value(&built.pod_volume)?, json!({"name": "workspace", "persistentVolumeClaim": {"claimName": "kiln-ws-a", "readOnly": false}}));
        assert_eq!(serde_json::to_value(&built.mount)?, json!({"name": "workspace", "mountPath": "/Users/u/src/kiln", "readOnly": false}));
        assert_eq!(serde_json::to_value(&built.security)?, json!({"runAsUser": 501, "runAsGroup": 20}));
        let read = objects("kiln-ws-a", "kiln-jobs", "01a1f7", &PodExport { read_only: true, ..export() })?;
        assert_eq!((read.mount.read_only, read.pod_volume.persistent_volume_claim.and_then(|c| c.read_only)), (Some(true), Some(true)));
        Ok(())
    }

    /// W10. Do: build agent session `01a1f7`'s pod over its workspace. Expect kiln's runner for that one session copied in
    /// and run in the harness's image, enrolling with its token, the workspace mounted at the folder's own path from its
    /// claim and worked in there, run as the folder's owner, and no deadline.
    #[test]
    fn w10_an_agents_pod_runs_kilns_runner_for_its_one_session_on_its_workspace() -> anyhow::Result<()> {
        let cfg = crate::config::Kubernetes::default();
        let built = objects("kiln-ws-a", "kiln-jobs", "01a1f7", &export())?;
        let pod = serde_json::to_value(agent_pod(&cfg, "01a1f7", "kiln-ws-a", "ghcr.io/acme/agents:1", "t0ken", ("http://kiln:7070", "kiln:7071"), &built))?;
        let spec = at(&pod, "spec");
        let agent = spec.pointer("/containers/0").cloned().unwrap_or_default();
        assert_eq!(at(&agent, "image"), json!("ghcr.io/acme/agents:1"));
        assert_eq!(at(&agent, "command"), json!(["/kiln-bin/kiln", "agent", "--ephemeral", "--session", "01a1f7", "--name", "kiln-ws-a", "--work-dir", "/work", "--slots", "1"]));
        assert_eq!(at(&agent, "workingDir"), json!("/Users/u/src/kiln"));
        assert!(at(&agent, "volumeMounts").as_array().is_some_and(|m| m.contains(&json!({"name": "workspace", "mountPath": "/Users/u/src/kiln", "readOnly": false}))));
        assert!(at(&agent, "env").as_array().is_some_and(|e| e.contains(&json!({"name": "KILN_JOIN_TOKEN", "value": "t0ken"}))));
        assert_eq!(at(&spec, "securityContext"), json!({"runAsUser": 501, "runAsGroup": 20}));
        assert!(at(&spec, "volumes").as_array().is_some_and(|v| v.contains(&json!({"name": "workspace", "persistentVolumeClaim": {"claimName": "kiln-ws-a", "readOnly": false}}))));
        assert_eq!((at(&spec, "activeDeadlineSeconds"), at(&spec, "restartPolicy")), (serde_json::Value::Null, json!("Never")));
        assert_eq!(pod.pointer("/metadata/labels/kiln.dev~1agent"), Some(&json!("01a1f7")));
        Ok(())
    }

    /// The cluster as a stand-in takes it: each call said in order, and the claim's creation and deletion answering as
    /// asked.
    struct Stand {
        said: std::sync::Mutex<Vec<String>>,
        claim: Result<(), String>,
        unclaim: Result<(), String>,
    }

    impl Stand {
        fn new(claim: Result<(), String>, unclaim: Result<(), String>) -> Self {
            Stand { said: std::sync::Mutex::new(Vec::new()), claim, unclaim }
        }
        fn say(&self, what: String) {
            self.said.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(what);
        }
        fn said(&self) -> Vec<String> {
            self.said.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
        }
    }

    impl Cluster for Stand {
        async fn create_volume(&self, v: &PersistentVolume) -> anyhow::Result<()> {
            self.say(format!("create volume {}", v.metadata.name.clone().unwrap_or_default()));
            Ok(())
        }
        async fn create_claim(&self, c: &PersistentVolumeClaim) -> anyhow::Result<()> {
            self.say(format!("create claim {}", c.metadata.name.clone().unwrap_or_default()));
            self.claim.clone().map_err(anyhow::Error::msg)
        }
        async fn delete_claim(&self, name: &str) -> anyhow::Result<()> {
            self.say(format!("delete claim {name}"));
            self.unclaim.clone().map_err(anyhow::Error::msg)
        }
        async fn delete_volume(&self, name: &str) -> anyhow::Result<()> {
            self.say(format!("delete volume {name}"));
            Ok(())
        }
    }

    /// W10. Do: make a pod's workspace in a cluster that takes it, then in one that refuses its claim; take it down, then
    /// take it down where the claim's deletion fails. Expect the PersistentVolume before the claim, and a refused claim's
    /// PersistentVolume taken back down; the claim before the PersistentVolume as it goes, the PersistentVolume asked to
    /// go even where the claim's deletion failed, that failure said.
    #[test]
    fn w10_a_pods_workspace_is_made_volume_then_claim_and_taken_down_claim_then_volume() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let built = objects("kiln-ws-a", "kiln-jobs", "01a1f7", &export())?;
            let taken = Stand::new(Ok(()), Ok(()));
            make(&taken, &built).await?;
            assert_eq!(taken.said(), ["create volume kiln-ws-a", "create claim kiln-ws-a"]);
            let refused = Stand::new(Err("exceeded quota".to_owned()), Ok(()));
            assert!(make(&refused, &built).await.is_err_and(|e| e.to_string().contains("exceeded quota")));
            assert_eq!(refused.said(), ["create volume kiln-ws-a", "create claim kiln-ws-a", "delete volume kiln-ws-a"]);
            let down = Stand::new(Ok(()), Ok(()));
            unmake(&down, "kiln-ws-a").await?;
            assert_eq!(down.said(), ["delete claim kiln-ws-a", "delete volume kiln-ws-a"]);
            let stuck = Stand::new(Ok(()), Err("forbidden".to_owned()));
            assert!(unmake(&stuck, "kiln-ws-a").await.is_err_and(|e| e.to_string().contains("forbidden")));
            assert_eq!(stuck.said(), ["delete claim kiln-ws-a", "delete volume kiln-ws-a"]);
            Ok(())
        })
    }

    /// W10. Do: build a pod's workspace over exports it can't be made from (no host, port 0, a relative path, a path a mount
    /// option list would split, a folder climbing out by `..`, a relative folder), and objects whose name isn't a DNS
    /// label; then show an export. Expect each refused, typed; and the export shown without its token.
    #[test]
    fn w10_an_export_a_pod_cant_mount_is_refused_typed_and_its_token_never_shown() {
        let refused = |e: PodExport| objects("kiln-ws-a", "kiln-jobs", "a", &e).err();
        assert_eq!(refused(PodExport { host: " ".to_owned(), ..export() }), Some(Unbuildable::NoHost));
        assert_eq!(refused(PodExport { port: 0, ..export() }), Some(Unbuildable::NoPort));
        for path in ["kiln-a@7.4f1c", "/kiln-a@7.4f1c,port=1", "/kiln a@7.4f1c", "/kiln-a@7.4f\n1c"] {
            assert_eq!(refused(PodExport { path: path.to_owned(), ..export() }), Some(Unbuildable::Path), "{path:?}");
        }
        for folder in ["/Users/u/../root", "src/kiln"] {
            assert_eq!(refused(PodExport { folder: folder.to_owned(), ..export() }), Some(Unbuildable::Folder { folder: folder.to_owned() }));
        }
        let long = "k".repeat(64);
        for name in ["Kiln-ws", "-kiln", "kiln-", "", long.as_str(), "kiln.ws"] {
            assert_eq!(objects(name, "kiln-jobs", "a", &export()).err(), Some(Unbuildable::Name { name: name.to_owned() }));
        }
        let shown = format!("{:?}", export());
        assert!(shown.contains("/kiln-a@7.<token>") && !shown.contains("4f1c2a9e0b"), "{shown}");
    }
}
