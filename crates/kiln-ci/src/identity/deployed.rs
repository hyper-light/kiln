//! Custody to production, the pure part (docs/ci.md W27-R08; the Transfers plan's step 14, kiln
//! plan `01a17856b1b477eda14a574ae312a123`): what CD hands a cluster as it deploys, what the cluster
//! hands the nodes it schedules pods on, and what each pod runs, as custody records
//! ([`super::custody`]) by digest.
//!
//! - **CD's hand-on** ([`hand_on`]) of a sync's manifests (by the SHA-256 of the objects as CD sent
//!   them, [`manifests`]) and the images they name by digest ([`pinned`]) to the cluster, following
//!   CD's receipt of each image ([`registry_hand_on`], [`cd_receipt`]).
//! - **The cluster's receipt** ([`cluster_receipt`]): the cluster holds no identity of kiln's, so
//!   kiln's server signs it for the cluster, on the strength of its API server's answer to each apply:
//!   the manifests by the digest of what the API server holds of what CD sent.
//! - **To each node** ([`to_node`], [`node_receipt`]): the images of the pods the cluster scheduled
//!   there, kiln's for the cluster and for the node.
//! - **The index's resolution** ([`Resolution`], [`to_pod`]): a node hands each pod the manifest of
//!   the image for its own platform (an index's entry, or the image itself), made from what it got.
//! - **What a pod runs** ([`runs`], [`pod_receipt`]): its status's image ID, read through the same
//!   resolution: the image, that manifest or that manifest's config runs the manifest; anything else
//!   runs something else, by its own digest, and the pod's receipt breaks the chain there.
//!
//! Clusters, nodes and pods are named `kubernetes://<cluster>`, `…/node/<name>` and
//! `…/<namespace>/pod/<name>` ([`cluster_uri`], [`node_uri`], [`pod_uri`]). No clock, disk or
//! network: the answers, the platforms and the times are arguments.

use std::collections::BTreeMap;

use serde_json::Value;

use super::custody::{Body, Kind, to_value};
use super::statement::{Statement, Subject};

/// A cluster kiln knows as `cluster`, as custody records name it.
pub fn cluster_uri(cluster: &str) -> String {
    format!("kubernetes://{cluster}")
}

/// Node `node` of cluster `cluster`.
pub fn node_uri(cluster: &str, node: &str) -> String {
    format!("kubernetes://{cluster}/node/{node}")
}

/// Pod `pod` of namespace `namespace` in cluster `cluster`.
pub fn pod_uri(cluster: &str, namespace: &str, pod: &str) -> String {
    format!("kubernetes://{cluster}/{namespace}/pod/{pod}")
}

/// Whether `hex` is a SHA-256 in hex.
fn sha256_hex(hex: &str) -> bool {
    hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit())
}

/// An image reference pinned by digest (`ghcr.io/acme/web:1.2@sha256:…`): its name without the tag
/// (`ghcr.io/acme/web`) and its SHA-256 in hex, lower case. None for a reference by tag alone.
pub fn pinned(reference: &str) -> Option<(String, String)> {
    let (name, digest) = reference.trim().rsplit_once('@')?;
    let hex = digest.strip_prefix("sha256:").filter(|h| sha256_hex(h))?.to_ascii_lowercase();
    // A tag follows the last `:` after the last `/` (a registry's port comes before a `/`).
    let slash = name.rfind('/').map_or(0, |i| i + 1);
    let name = match name.get(slash..).and_then(|last| last.rfind(':')) {
        Some(i) => name.get(..slash + i).unwrap_or(name),
        None => name,
    };
    (!name.is_empty()).then(|| (name.to_owned(), hex))
}

/// The SHA-256 (hex) a container status's image ID names, as each runtime writes it:
/// `docker-pullable://<repository>@sha256:…` and `docker://sha256:…` (Docker's), `<repository>@sha256:…`
/// and `sha256:…` (containerd's, CRI-O's). None while it has none (the image still pulling).
pub fn image_id(id: &str) -> Option<String> {
    let (_, hex) = id.trim().rsplit_once("sha256:")?;
    sha256_hex(hex).then(|| hex.to_ascii_lowercase())
}

/// An image a deploy names, for one platform, as a node of that platform pulls it: the image (an
/// index, or one manifest), the manifest the node pulls (the index's entry for its platform, or the
/// image itself) and that manifest's config, each a SHA-256 in hex.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolution {
    pub image: String,
    /// `linux/amd64`, `linux/arm64/v8`.
    pub platform: String,
    pub manifest: String,
    pub config: String,
}

/// Whether a manifest (its media type and JSON) is an index: OCI's, or Docker's manifest list.
pub fn is_index(media_type: &str, manifest: &Value) -> bool {
    media_type.contains("index") || media_type.contains("manifest.list") || manifest.get("manifests").is_some_and(Value::is_array)
}

/// The SHA-256 (hex) of the manifest index `index` lists for `platform` (`linux/amd64`,
/// `linux/arm64/v8`), as a runtime picks it: the same OS and architecture, the variant given (an
/// `arm64` without one takes `v8` or none); attestations (`unknown/unknown`) never.
pub fn platform_entry(index: &Value, platform: &str) -> Option<String> {
    let mut parts = platform.split('/');
    let (os, arch, variant) = (parts.next()?, parts.next()?, parts.next());
    let entries: Vec<&Value> = index.get("manifests")?.as_array()?.iter().filter(|m| m.pointer("/platform/os").and_then(Value::as_str) == Some(os) && m.pointer("/platform/architecture").and_then(Value::as_str) == Some(arch)).collect();
    let of = |m: &Value| m.pointer("/platform/variant").and_then(Value::as_str).map(str::to_owned);
    let chosen = match variant {
        Some(v) => entries.iter().find(|m| of(m).as_deref() == Some(v)),
        None if arch == "arm64" => entries.iter().find(|m| matches!(of(m).as_deref(), Some("v8") | None)),
        None => entries.iter().find(|m| of(m).is_none()).or_else(|| entries.first()),
    }?;
    let hex = chosen.get("digest")?.as_str()?.strip_prefix("sha256:")?;
    sha256_hex(hex).then(|| hex.to_ascii_lowercase())
}

/// The SHA-256 (hex) of an image manifest's config.
pub fn config_of(manifest: &Value) -> Option<String> {
    let hex = manifest.pointer("/config/digest")?.as_str()?.strip_prefix("sha256:")?;
    sha256_hex(hex).then(|| hex.to_ascii_lowercase())
}

/// What a pod's container runs, by what its status reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Runs {
    /// The manifest the image resolves to on its node's platform: reported as the image, that
    /// manifest or its config (Docker reports the index a tag or digest named; CRI-O the manifest;
    /// an image loaded with no registry's digest, its config).
    Follows,
    /// Something else, by the SHA-256 its status reports.
    Other(String),
    /// Nothing yet: its status names no digest.
    Unknown,
}

/// What a container whose status reports image ID `reported` runs, for image `r` on its node's
/// platform.
pub fn runs(r: &Resolution, reported: &str) -> Runs {
    match image_id(reported) {
        None => Runs::Unknown,
        Some(hex) if hex == r.image || hex == r.manifest || hex == r.config => Runs::Follows,
        Some(hex) => Runs::Other(hex),
    }
}

/// A SHA-256 (hex) as one digest of a thing.
fn sha256(hex: &str) -> BTreeMap<String, String> {
    BTreeMap::from([("sha256".to_owned(), hex.to_owned())])
}

/// Image `name` by its SHA-256 (`hex`): what a deploy names, and its custodians hand on.
pub fn image(name: &str, hex: &str) -> Subject {
    Subject { name: format!("image {name}"), digest: sha256(hex), ..Subject::default() }
}

/// The manifests a sync of app `app` at `revision` applied, `objects` of them, by the SHA-256
/// (`hex`) of the objects as CD sent them ([`digest_of_objects`]).
pub fn manifests(app: &str, revision: &str, hex: &str, objects: usize) -> Subject {
    let annotations = BTreeMap::from([("objects".to_owned(), Value::from(objects))]);
    let at = if revision.is_empty() { String::new() } else { format!(" at {revision}") };
    Subject { name: format!("manifests of {app}{at}"), digest: sha256(hex), size: None, annotations }
}

/// The SHA-256 (hex) of `objects`, each as canonical JSON (every object's keys sorted), in the
/// order given: the caller sorts them (by kind, namespace and name).
pub fn digest_of_objects(objects: &[Value]) -> String {
    use sha2::Digest as _;
    let mut h = sha2::Sha256::new();
    for o in objects {
        let bytes = serde_json::to_vec(&sorted(o)).unwrap_or_default();
        h.update(u64::try_from(bytes.len()).unwrap_or(u64::MAX).to_be_bytes());
        h.update(&bytes);
    }
    super::chain::hex(&h.finalize())
}

/// `v` with every object's keys in sorted order, whatever order serde_json keeps a map's keys in: a
/// build with its `preserve_order` (one crate of the build asking for it is enough) keeps them as
/// inserted.
fn sorted(v: &Value) -> Value {
    match v {
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            Value::Object(keys.into_iter().filter_map(|k| m.get(k).map(|x| (k.clone(), sorted(x)))).collect())
        }
        Value::Array(a) => Value::Array(a.iter().map(sorted).collect()),
        other => other.clone(),
    }
}

/// A statement of `body` about `subject`, following `follows`.
fn statement(body: &Body, subject: Vec<Subject>, follows: Vec<String>) -> Statement {
    Statement { subject, follows, ..Statement::new(to_value(body)) }
}

/// kiln's server (`server`) hands `thing`, an image it holds (by its receipt `held`), on to CD (`cd`)
/// for app `app`'s deploys, from its registry: the same for every deploy of it, kept once.
pub fn registry_hand_on(server: &str, cd: &str, thing: Subject, held: &str, app: &str) -> Statement {
    let b = Body { to: Some(cd.to_owned()), component: Some("registry".into()), why: format!("an image kiln holds, for {app}'s deploys"), ..Body::new(Kind::HandedOn, server) };
    statement(&b, vec![thing], vec![held.to_owned()])
}

/// CD's receipt of image `thing` for app `app`: from kiln's server, following its hand-on
/// ([`registry_hand_on`]); or, with no hand-on, from where kiln has no identity (`from`:
/// `docker://<reference>`), where its chain begins.
pub fn cd_receipt(cd: &str, from: &str, thing: Subject, hand_on: Option<&str>, app: &str) -> Statement {
    let b = Body { from: Some(from.to_owned()), component: Some(format!("app {app}")), why: format!("an image {app} deploys"), ..Body::new(Kind::Received, cd) };
    statement(&b, vec![thing], hand_on.map(str::to_owned).into_iter().collect())
}

/// CD's hand-on of `things` (a sync's [`manifests`] and the images they name) to cluster `cluster`,
/// following CD's receipts of those images (`holding`), at `at` (unix seconds: each sync's own).
pub fn hand_on(cd: &str, cluster: &str, things: Vec<Subject>, holding: Vec<String>, why: &str, at: i64) -> Statement {
    let b = Body { to: Some(cluster_uri(cluster)), why: why.to_owned(), said_at: Some(at), ..Body::new(Kind::HandedOn, cd) };
    statement(&b, things, holding)
}

/// kiln's server's receipt for cluster `cluster` of `things` from CD (`cd`), following CD's hand-on
/// `hand_on`: the manifests by the digest of what its API server holds of what CD sent, saying so
/// (`why`).
pub fn cluster_receipt(server: &str, cluster: &str, cd: &str, things: Vec<Subject>, hand_on: &str, why: &str) -> Statement {
    let b = Body { from: Some(cd.to_owned()), taken_for: Some(cluster_uri(cluster)), why: why.to_owned(), ..Body::new(Kind::Received, server) };
    statement(&b, things, vec![hand_on.to_owned()])
}

/// kiln for cluster `cluster` hands `images` on to node `node`, the images of the pods it scheduled
/// there (`why`), following the cluster's receipt `receipt`.
pub fn to_node(server: &str, cluster: &str, node: &str, images: Vec<Subject>, receipt: &str, why: &str) -> Statement {
    let b = Body { to: Some(node_uri(cluster, node)), taken_for: Some(cluster_uri(cluster)), why: why.to_owned(), ..Body::new(Kind::Taken, server) };
    statement(&b, images, vec![receipt.to_owned()])
}

/// kiln's receipt for node `node` of cluster `cluster` of `images`, following the cluster's hand-on
/// to it, `take`.
pub fn node_receipt(server: &str, cluster: &str, node: &str, images: Vec<Subject>, take: &str, why: &str) -> Statement {
    let b = Body { from: Some(cluster_uri(cluster)), taken_for: Some(node_uri(cluster, node)), why: why.to_owned(), ..Body::new(Kind::Received, server) };
    statement(&b, images, vec![take.to_owned()])
}

/// Image `name` for platform `r.platform`, by the manifest a node of it pulls (`hex`).
fn for_platform(name: &str, r: &Resolution, hex: &str) -> Subject {
    let mut annotations = BTreeMap::new();
    if r.image != hex {
        annotations.insert("image".to_owned(), Value::from(format!("sha256:{}", r.image)));
    }
    Subject { name: format!("image {name} for {}", r.platform), digest: sha256(hex), size: None, annotations }
}

/// kiln for node `node` of cluster `cluster` hands pod `pod` (its URI) the manifest image `name`
/// resolves to on its platform (`r`), made from its receipt of the image, `receipt`: the index's
/// resolution, as its runtime makes it.
#[allow(clippy::too_many_arguments)]
pub fn to_pod(server: &str, cluster: &str, node: &str, pod: &str, name: &str, r: &Resolution, receipt: &str, why: &str) -> Statement {
    let mut thing = for_platform(name, r, &r.manifest);
    thing.annotations.insert("config".into(), Value::from(format!("sha256:{}", r.config)));
    let b = Body { to: Some(pod.to_owned()), taken_for: Some(node_uri(cluster, node)), why: why.to_owned(), ..Body::new(Kind::Taken, server) };
    statement(&b, vec![thing], vec![receipt.to_owned()])
}

/// kiln's receipt for pod `pod` (its URI) on node `node` of what it runs as image `name` for its
/// platform (`r`): `runs`, the SHA-256 its status reports read through the resolution (the manifest
/// when it follows, else its own), its status's own words (`reported`) beside it; following the
/// node's hand-on `take`. Another digest than the hand-on's breaks the chain here.
#[allow(clippy::too_many_arguments)]
pub fn pod_receipt(server: &str, cluster: &str, node: &str, pod: &str, name: &str, r: &Resolution, runs: &str, reported: &str, take: &str, why: &str) -> Statement {
    let mut thing = for_platform(name, r, runs);
    thing.annotations.insert("reported".into(), Value::from(reported));
    let b = Body { from: Some(node_uri(cluster, node)), taken_for: Some(pod.to_owned()), why: why.to_owned(), ..Body::new(Kind::Received, server) };
    statement(&b, vec![thing], vec![take.to_owned()])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(c: char) -> String {
        c.to_string().repeat(64)
    }

    #[test]
    fn w27_r08_an_image_pinned_by_digest_is_named_without_its_tag() {
        let d = format!("sha256:{}", hex('A'));
        assert_eq!(pinned(&format!("ghcr.io/acme/web:1.2@{d}")), Some(("ghcr.io/acme/web".into(), hex('a'))));
        assert_eq!(pinned(&format!("localhost:5000/acme/web@{d}")), Some(("localhost:5000/acme/web".into(), hex('a'))), "a port isn't a tag");
        assert_eq!(pinned(&format!("localhost:5000/web:v1@{d}")), Some(("localhost:5000/web".into(), hex('a'))));
        assert_eq!(pinned("ghcr.io/acme/web:1.2"), None, "by tag alone: nothing to compare by digest");
        assert_eq!(pinned("ghcr.io/acme/web@sha256:abc"), None, "not a SHA-256");
        assert_eq!(pinned(&format!("@{d}")), None);
    }

    #[test]
    fn w27_r08_each_runtimes_image_id_names_its_digest() {
        let (a, b) = (hex('a'), hex('b'));
        assert_eq!(image_id(&format!("docker-pullable://localhost:5000/acme/web@sha256:{a}")), Some(a.clone()));
        assert_eq!(image_id(&format!("docker://sha256:{b}")), Some(b.clone()));
        assert_eq!(image_id(&format!("docker.io/library/nginx@sha256:{a}")), Some(a.clone()));
        assert_eq!(image_id(&format!("sha256:{}", a.to_ascii_uppercase())), Some(a.clone()));
        assert_eq!(image_id(""), None, "still pulling");
        assert_eq!(image_id("docker://sha256:abc"), None);
    }

    /// W27-R08 (the Transfers plan's step 14): a pod runs what its deploy named on its node's
    /// platform however its runtime reports it: the index (Docker, containerd), the platform's
    /// manifest (CRI-O) or that manifest's config (an image loaded without a registry); anything
    /// else is another image, by its own digest.
    #[test]
    fn w27_r08_a_pods_image_follows_through_the_indexs_resolution() {
        let r = Resolution { image: hex('1'), platform: "linux/amd64".into(), manifest: hex('2'), config: hex('3') };
        for id in [format!("docker-pullable://localhost:5000/acme/web@sha256:{}", hex('1')), format!("sha256:{}", hex('2')), format!("docker://sha256:{}", hex('3'))] {
            assert_eq!(runs(&r, &id), Runs::Follows, "{id}");
        }
        assert_eq!(runs(&r, &format!("docker-pullable://registry.k8s.io/pause@sha256:{}", hex('9'))), Runs::Other(hex('9')));
        assert_eq!(runs(&r, ""), Runs::Unknown);
        // The arm64 node's manifest isn't the amd64 node's: run there, it's another image.
        let arm = Resolution { platform: "linux/arm64".into(), manifest: hex('4'), config: hex('5'), ..r.clone() };
        assert_eq!(runs(&arm, &format!("sha256:{}", hex('2'))), Runs::Other(hex('2')));
        assert_eq!(runs(&arm, &format!("sha256:{}", hex('4'))), Runs::Follows);
    }

    #[test]
    fn w27_r08_an_index_resolves_to_its_platforms_manifest() {
        let entry = |os: &str, arch: &str, variant: Option<&str>, c: char| {
            let mut p = serde_json::json!({"os": os, "architecture": arch});
            if let (Some(v), Some(m)) = (variant, p.as_object_mut()) {
                m.insert("variant".into(), Value::from(v));
            }
            serde_json::json!({"mediaType": "application/vnd.oci.image.manifest.v1+json", "digest": format!("sha256:{}", hex(c)), "platform": p})
        };
        let index = serde_json::json!({"schemaVersion": 2, "mediaType": "application/vnd.oci.image.index.v1+json", "manifests": [
            entry("linux", "amd64", None, '1'), entry("linux", "arm64", Some("v8"), '2'), entry("linux", "arm", Some("v7"), '3'), entry("unknown", "unknown", None, '4'),
        ]});
        assert!(is_index("application/vnd.oci.image.index.v1+json", &index));
        assert!(is_index("", &index), "an index by its shape");
        assert!(!is_index("application/vnd.oci.image.manifest.v1+json", &serde_json::json!({"config": {}})));
        assert_eq!(platform_entry(&index, "linux/amd64"), Some(hex('1')));
        assert_eq!(platform_entry(&index, "linux/arm64"), Some(hex('2')), "arm64 takes v8");
        assert_eq!(platform_entry(&index, "linux/arm64/v8"), Some(hex('2')));
        assert_eq!(platform_entry(&index, "linux/arm/v7"), Some(hex('3')));
        assert_eq!(platform_entry(&index, "windows/amd64"), None);
        assert_eq!(platform_entry(&index, "unknown"), None);
        let manifest = serde_json::json!({"config": {"digest": format!("sha256:{}", hex('5'))}, "layers": []});
        assert_eq!(config_of(&manifest), Some(hex('5')));
        assert_eq!(config_of(&index), None);
    }

    #[test]
    fn w27_r08_a_syncs_manifests_are_named_by_what_cd_sent() {
        let (deployment, service) = (serde_json::json!({"kind": "Deployment", "metadata": {"name": "web"}}), serde_json::json!({"kind": "Service", "metadata": {"name": "web"}}));
        let d = digest_of_objects(&[deployment.clone(), service.clone()]);
        assert_eq!(d.len(), 64);
        assert_ne!(d, digest_of_objects(&[service.clone(), deployment]), "in order");
        let reordered = serde_json::json!({"metadata": {"name": "web"}, "kind": "Deployment"});
        assert_eq!(digest_of_objects(&[reordered, service]), d, "a map's keys in any order are one object");
        let m = manifests("shop/web", "4c1e2a", &d, 2);
        assert_eq!((m.name.as_str(), m.annotations.get("objects")), ("manifests of shop/web at 4c1e2a", Some(&Value::from(2))));
    }

    /// W27-R08: the objects' JSON is canonical whatever order serde_json keeps a map's keys in (a
    /// build with its `preserve_order`, the workspace's, keeps them as inserted): every object's
    /// keys sorted, at every depth.
    #[test]
    fn w27_r08_a_syncs_objects_are_hashed_with_their_keys_sorted() {
        let o = serde_json::json!({"spec": {"replicas": 2, "selector": {}}, "kind": "Deployment", "metadata": {"name": "web", "labels": [{"z": 1, "a": 2}]}});
        assert_eq!(serde_json::to_string(&sorted(&o)).unwrap_or_default(), r#"{"kind":"Deployment","metadata":{"labels":[{"a":2,"z":1}],"name":"web"},"spec":{"replicas":2,"selector":{}}}"#);
    }
}
