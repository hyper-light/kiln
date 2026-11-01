//! Custody to production (docs/ci.md W27-R08; the Transfers plan's step 14, kiln plan
//! `01a17856b1b477eda14a574ae312a123`; migration `w27_cd_custody`): what a KilnCD sync hands its
//! cluster, the images the cluster hands its nodes, and what each pod runs, as custody records
//! ([`kiln_ci::identity::deployed`]) kept as every other ([`crate::custody`]).
//!
//! - **A sync** of an app to a Kubernetes cluster that succeeded: CD hands on the manifests it
//!   applied (by the SHA-256 of the objects as it sent them) and the images their pod templates name
//!   by digest, following its receipt of each image (from kiln's registry, which holds what a leg
//!   pushed, or from outside kiln, where its chain begins). kiln's server receives them for the
//!   cluster, which holds no identity of kiln's, on the strength of its API server's answers: the
//!   manifests by the digest of what the answers hold of what CD sent, so an apply a webhook
//!   changed breaks the chain there.
//! - **Its pods**, looked at as the sync's rollout goes on, every [`EVERY`] after, and when a release
//!   asks: for each container the sync's manifests name, of each running pod, the cluster hands its
//!   image on to the pod's node and the node receives it; the node hands the pod its platform's
//!   manifest (the index's resolution: the index as kiln's registry holds it by digest, else as its
//!   own registry serves it); and the pod's receipt is of what its status's image ID says it runs,
//!   read through that resolution. A pod running anything else breaks the chain at its receipt, as
//!   kiln takes it, and the break is recorded.
//! - **The verdict** ([`verdict`]): whether every pod runs what its deploy handed on, by which an
//!   environment requiring a verified chain (W27-R11) counts a deploy healthy.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use kiln_cd::engine::Record;
use kiln_cd::obj::{self, GroupKind, ResKey};
use kiln_ci::identity::deployed::{self, Resolution, Runs};
use kiln_ci::identity::statement::Statement;
use parking_lot::Mutex;
use rusqlite::{OptionalExtension as _, params};
use serde_json::{Value, json};

use super::Cd;
use crate::hub::Hub;

/// How often an app's pods are looked at again once its rollout has settled.
pub const EVERY: Duration = Duration::from_secs(60);
/// How long a look at an app's pods is reused (a release asks every few seconds).
const FRESH: Duration = Duration::from_secs(4);
/// How long a sync's rollout is watched for its pods, a look every few seconds.
const ROLLOUT: Duration = Duration::from_secs(600);
/// The syncs of an app a pod's image is looked for in, newest first: one of an earlier rollout still
/// runs what its sync handed on.
const SYNCS: usize = 10;
/// The most rollouts watched at once: a fleet's syncs past it are looked at by [`run`]'s next round.
const WATCHES: usize = 16;

/// One object a sync applied: as CD sent it, as its API server answered, and the field manager.
struct Applied {
    sent: Value,
    answer: Value,
    manager: String,
}

/// Where a look at an app's pods leaves its deploy.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Verdict {
    /// Every running pod's container its deploy names runs what the deploy handed on.
    Verified { pods: usize, containers: usize },
    /// Not known yet: why (no sync with custody yet, pods still starting).
    Waiting { why: String },
    /// A pod runs what its deploy didn't hand on: why; the break is recorded.
    Broken { why: String },
    /// Its deploy can't be followed to its pods: why (an image named by tag alone).
    Unverifiable { why: String },
}

/// What KilnCD keeps for custody between syncs and looks.
#[derive(Default)]
pub struct Custody {
    /// Each sync under way: what it applied so far, by the app it's tracked as, then by object.
    syncing: Mutex<HashMap<String, BTreeMap<String, Applied>>>,
    /// Images resolved for platforms, by (image, platform): by digest, so never stale.
    resolved: Mutex<HashMap<(String, String), Resolution>>,
    /// Nodes' platforms, by (cluster, node).
    platforms: Mutex<HashMap<(String, String), String>>,
    /// The last look at each app's pods, and when.
    looked: Mutex<HashMap<String, (Instant, Verdict)>>,
    /// Apps whose rollout is being watched.
    watching: Mutex<std::collections::BTreeSet<String>>,
}

/// The app an applied object is tracked as, and the object's key, from kiln's tracking (or Argo
/// CD's, for an app adopted from it): `<app>:<group>/<kind>:<namespace>/<name>`.
fn tracked(object: &Value) -> Option<(String, String)> {
    let t = obj::annotation(object, kiln_cd::engine::TRACKING).or_else(|| obj::annotation(object, kiln_cd::engine::ARGO_TRACKING))?;
    let (app, key) = t.split_once(':')?;
    Some((app.to_owned(), key.to_owned()))
}

impl Custody {
    /// An object a sync applied for real, and what the API server answered.
    pub fn applied(&self, object: &Value, answer: &Value, manager: &str) {
        let Some((app, key)) = tracked(object) else { return };
        let a = Applied { sent: object.clone(), answer: answer.clone(), manager: manager.to_owned() };
        self.syncing.lock().entry(app).or_default().insert(key, a);
    }

    /// What a sync of the app tracked as `app` applied, taken.
    fn take_applied(&self, app: &str) -> BTreeMap<String, Applied> {
        self.syncing.lock().remove(app).unwrap_or_default()
    }
}

/// A sync started or finished (KilnCD's engine's record): what it applied is kept from its start,
/// and once it has succeeded, its custody is recorded and its rollout watched.
pub fn on_record(cd: &Arc<Cd>, r: &Record) {
    let (app, tracked_as) = match r {
        Record::Started { app, .. } | Record::Finished { app, .. } => {
            let tracked_as = cd.apps.read().get(app.as_ref()).map(|e| e.def.config().tracked_as().to_owned()).unwrap_or_else(|| app.to_string());
            (app.to_string(), tracked_as)
        }
    };
    match r {
        Record::Started { .. } => {
            cd.custody.take_applied(&tracked_as);
        }
        Record::Finished { op, revision, succeeded, at_ms, .. } => {
            let applied = cd.custody.take_applied(&tracked_as);
            if !*succeeded || applied.is_empty() {
                return;
            }
            let (cd, op, revision, at) = (cd.clone(), *op, revision.as_ref().map(|r| r.to_string()).unwrap_or_default(), i64::try_from(*at_ms / 1000).unwrap_or_default());
            crate::rt::spawn(async move {
                match synced(&cd, &app, op, &revision, at, applied).await {
                    Ok(true) => watch(cd, app),
                    Ok(false) => {}
                    Err(e) => tracing::warn!(target: "kiln.custody", %app, op, error = %format!("{e:#}"), "a sync's custody to its cluster wasn't recorded"),
                }
            });
        }
    }
}

/// A container a sync's manifests name: its workload (`group/kind:namespace/name`), its name, and
/// its image as the manifests name it.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct Named {
    workload: String,
    container: String,
    image: String,
}

/// An object's key as kiln's tracking writes it: `group/kind:namespace/name`.
fn workload_key(o: &Value) -> String {
    format!("{}/{}:{}/{}", obj::group(o), obj::kind(o), obj::namespace(o), obj::name(o))
}

/// The pod spec of a workload (Deployment, StatefulSet, DaemonSet, ReplicaSet, Job, a CronJob's
/// jobs, a Pod, or anything shaped like one: an Argo Rollout).
fn pod_spec(o: &Value) -> Option<&Value> {
    if obj::kind(o) == "Pod" && obj::group(o).is_empty() {
        return o.get("spec");
    }
    o.pointer("/spec/template/spec").or_else(|| o.pointer("/spec/jobTemplate/spec/template/spec"))
}

/// Each container a manifest names, init containers too.
fn containers_of(key: &str, o: &Value) -> Vec<Named> {
    let Some(spec) = pod_spec(o) else { return Vec::new() };
    ["initContainers", "containers"]
        .iter()
        .flat_map(|list| spec.get(*list).and_then(Value::as_array).into_iter().flatten())
        .filter_map(|c| Some(Named { workload: key.to_owned(), container: c.get("name")?.as_str()?.to_owned(), image: c.get("image")?.as_str()?.to_owned() }))
        .collect()
}

/// A Secret with its values left out (its keys kept): custody names what was deployed by digest,
/// and a digest over a secret's value is a way to guess it.
fn masked(o: &mut Value) {
    if obj::kind(o) != "Secret" || !obj::group(o).is_empty() {
        return;
    }
    for field in ["data", "stringData"] {
        if let Some(m) = o.get_mut(field).and_then(Value::as_object_mut) {
            for v in m.values_mut() {
                *v = Value::String(String::new());
            }
        }
    }
}

/// What `answer` holds of what `sent` set (both prepared for comparing): `sent` itself when every
/// field it sets is there with its value, else the answer, and where it differs.
fn held_of(sent: &Value, answer: &Value, manager: &str) -> (Value, Option<String>) {
    let live = kiln_cd::diff::LiveView::new(answer.clone(), manager);
    match kiln_cd::diff::compare(sent, &live.object, live.owned(manager).as_ref()) {
        kiln_cd::diff::Verdict::Synced => (sent.clone(), None),
        kiln_cd::diff::Verdict::OutOfSync { path, .. } => (live.object, Some(path)),
    }
}

/// `s` signed by kiln's CD, taken: its digest, and the break its hop recorded, if any. An error if
/// kiln doesn't take its own.
async fn as_cd(hub: &Hub, s: &Statement, came_from: &str) -> anyhow::Result<(String, Option<String>)> {
    let bundle = crate::custody::cd_signed(&hub.pki, s)?;
    taken(hub, &bundle, came_from).await
}

/// `s` signed by kiln's server, taken: its digest, and the break its hop recorded, if any.
async fn as_server(hub: &Hub, s: &Statement, came_from: &str) -> anyhow::Result<(String, Option<String>)> {
    let bundle = crate::custody::server_signed(&hub.pki, s)?;
    taken(hub, &bundle, came_from).await
}

async fn taken(hub: &Hub, bundle: &Value, came_from: &str) -> anyhow::Result<(String, Option<String>)> {
    match crate::custody::take(&hub.store, &hub.pki.ca_der, &hub.pki.organization(), bundle, came_from, crate::util::now_unix()).await? {
        Ok(t) => Ok((t.digest, t.broke)),
        Err(why) => anyhow::bail!("kiln refused its own custody record: {why}"),
    }
}

/// CD's receipt of image `name` (`hex`) for app `app`: from kiln's registry when kiln holds it (a
/// leg pushed it), through the registry's hand-on to CD; else from outside kiln, where its chain
/// begins. The same for each sync of it.
async fn holding(hub: &Hub, app: &str, name: &str, hex: &str) -> anyhow::Result<String> {
    let org = hub.pki.organization();
    let (server, cd_uri) = (crate::custody::server_uri(&org), crate::custody::cd_uri(&org));
    let thing = deployed::image(name, hex);
    let came_from = format!("KilnCD's deploys of {app}");
    let receipt = match crate::custody::held_with(&hub.store, &org, &server, &thing).await? {
        Some(held) => {
            let (to_cd, _) = as_server(hub, &deployed::registry_hand_on(&server, &cd_uri, thing.clone(), &held, app), &came_from).await?;
            deployed::cd_receipt(&cd_uri, &server, thing, Some(&to_cd), app)
        }
        None => deployed::cd_receipt(&cd_uri, &format!("docker://{name}@sha256:{hex}"), thing, None, app),
    };
    Ok(as_cd(hub, &receipt, &came_from).await?.0)
}

/// A sync's records, kept: CD's hand-on and kiln's receipt for the cluster, the SHA-256 of what CD
/// sent and of what the cluster holds of it, how many objects, the containers its manifests name,
/// the images they name by digest, where the cluster holds otherwise (if it does), and the break
/// the receipt's hop recorded.
struct Recorded {
    hand_on: String,
    receipt: String,
    sent: String,
    held: String,
    objects: usize,
    named: Vec<Named>,
    images: Vec<(String, String)>,
    otherwise: Option<String>,
    broke: Option<String>,
}

/// The custody of a sync of `app` to `cluster` that succeeded (op `op`, at `revision`, finished at
/// `at`), what it `applied`: CD's holding of each image its manifests name by digest, CD's hand-on of
/// what it applied to the cluster, and kiln's receipt for the cluster, signed and kept.
async fn record_sync(hub: &Hub, cluster: &str, app: &str, op: u64, revision: &str, at: i64, applied: &BTreeMap<String, Applied>) -> anyhow::Result<Recorded> {
    let (mut sent, mut held, mut answered, mut named) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut otherwise: Option<String> = None;
    for (key, a) in applied {
        let (mut s, mut answer) = (a.sent.clone(), a.answer.clone());
        kiln_cd::diff::normalize_desired(&mut s);
        masked(&mut s);
        masked(&mut answer);
        let (h, differs) = held_of(&s, &answer, &a.manager);
        if otherwise.is_none()
            && let Some(path) = differs
        {
            otherwise = Some(format!("it holds {key} otherwise: {path}"));
        }
        named.extend(containers_of(&workload_key(&s), &s));
        sent.push(s);
        held.push(h);
        answered.push(answer);
    }
    let n = sent.len();
    let (sent_hex, held_hex) = (deployed::digest_of_objects(&sent), deployed::digest_of_objects(&held));
    let mut images: Vec<(String, String)> = named.iter().filter_map(|c| deployed::pinned(&c.image)).collect();
    images.sort();
    images.dedup();
    let mut holdings = Vec::new();
    for (name, hex) in &images {
        holdings.push(holding(hub, app, name, hex).await?);
    }
    let org = hub.pki.organization();
    let (server, cd_uri) = (crate::custody::server_uri(&org), crate::custody::cd_uri(&org));
    let came_from = format!("KilnCD's sync {op} of {app}");
    let mut things = vec![deployed::manifests(app, revision, &sent_hex, n)];
    things.extend(images.iter().map(|(name, hex)| deployed::image(name, hex)));
    let why = format!("sync {op} of {app}{}: the {n} objects it applied, to cluster {cluster}", if revision.is_empty() { String::new() } else { format!(" at {revision}") });
    let (hand_on, _) = as_cd(hub, &deployed::hand_on(&cd_uri, cluster, things.clone(), holdings, &why, at), &came_from).await?;
    let mut answer = deployed::manifests(app, revision, &held_hex, n);
    answer.annotations.insert("applied".into(), json!(format!("sha256:{}", deployed::digest_of_objects(&answered))));
    let mut got = vec![answer];
    got.extend(things.into_iter().skip(1));
    let said = match &otherwise {
        Some(d) => format!("as its API server answered each of the {n} applies: {d}"),
        None => format!("as its API server answered each of the {n} applies"),
    };
    let (receipt, broke) = as_server(hub, &deployed::cluster_receipt(&server, cluster, &cd_uri, got, &hand_on, &said), &came_from).await?;
    if let Some(b) = &broke {
        tracing::warn!(target: "kiln.custody", %app, op, %cluster, broke = %b, "the cluster holds other manifests than CD handed on");
    }
    Ok(Recorded { hand_on, receipt, sent: sent_hex, held: held_hex, objects: n, named, images, otherwise, broke })
}

/// Records the custody of a sync of `app` that succeeded ([`record_sync`]) and keeps it as the app's
/// latest: whether its manifests name an image by digest, whose pods are then worth watching.
async fn synced(cd: &Arc<Cd>, app: &str, op: u64, revision: &str, at: i64, applied: BTreeMap<String, Applied>) -> anyhow::Result<bool> {
    let cluster = cd.apps.read().get(app).map(|e| e.def.destination.cluster.clone()).unwrap_or_default();
    let r = record_sync(&cd.hub, &cluster, app, op, revision, at, &applied).await?;
    let (app_s, revision_s, containers) = (app.to_owned(), revision.to_owned(), serde_json::to_string(&r.named)?);
    let (why_s, now) = (r.otherwise.clone().or_else(|| r.broke.clone()).unwrap_or_default(), crate::util::now_unix());
    let op_i = i64::try_from(op).unwrap_or(i64::MAX);
    let n_i = i64::try_from(r.objects).unwrap_or(i64::MAX);
    let (hand_on, receipt, sent, held) = (r.hand_on.clone(), r.receipt.clone(), r.sent.clone(), r.held.clone());
    cd.hub
        .store
        .call(move |c| {
            c.execute(
                "INSERT INTO cd_custody_syncs (app, cluster, revision, op, hand_on, receipt, manifests, held, objects, containers, why, at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![app_s, cluster, revision_s, op_i, hand_on, receipt, sent, held, n_i, containers, why_s, now],
            )?;
            Ok(())
        })
        .await?;
    cd.custody.looked.lock().remove(app);
    Ok(!r.images.is_empty())
}

/// Watches `app`'s rollout: its pods looked at every few seconds until each runs what its sync handed
/// on, or one breaks the chain, or [`ROLLOUT`] passes; once at a time.
fn watch(cd: Arc<Cd>, app: String) {
    {
        let mut watching = cd.custody.watching.lock();
        if watching.len() >= WATCHES || !watching.insert(app.clone()) {
            return;
        }
    }
    crate::rt::spawn(async move {
        let until = Instant::now() + ROLLOUT;
        while Instant::now() < until && cd.hub.shutdown.sleep(Duration::from_secs(3)).await {
            match verdict(&cd, &app, true).await {
                Verdict::Waiting { .. } => continue,
                Verdict::Broken { why } => {
                    crate::deploy::kilncd::custody_broke(&app, &why).await;
                    break;
                }
                _ => break,
            }
        }
        cd.custody.watching.lock().remove(&app);
    });
}

/// Looks at every app with custody to its cluster every [`EVERY`]: a pod whose image was swapped
/// after its deploy breaks the chain however long after.
pub async fn run(cd: Arc<Cd>) {
    while cd.hub.shutdown.sleep(EVERY).await {
        let apps: Vec<String> = cd.hub.store.call(|c| Ok(c.prepare("SELECT DISTINCT app FROM cd_custody_syncs")?.query_map([], |r| r.get(0))?.collect::<Result<Vec<String>, _>>()?)).await.unwrap_or_default();
        for app in apps.into_iter().filter(|a| cd.apps.read().contains_key(a)) {
            if let Verdict::Broken { why } = verdict(&cd, &app, true).await {
                crate::deploy::kilncd::custody_broke(&app, &why).await;
            }
        }
    }
}

/// CD's newest receipt of the image whose SHA-256 is `hex` for KilnCD app `app`'s deploys: what a
/// promotion from that app's environment hands on following (docs/ci.md W27-R08; the Transfers
/// plan's steps 8 and 14). None before a sync of it recorded its custody.
pub async fn cd_holding(hub: &Hub, app: &str, hex: &str) -> anyhow::Result<Option<String>> {
    let cd = crate::custody::cd_uri(&hub.pki.organization());
    let (h, component) = (hex.to_ascii_lowercase(), format!("app {app}"));
    let rows: Vec<(String, String)> = hub
        .store
        .call(move |c| {
            let mut q = c.prepare(
                "SELECT c.digest, c.envelope FROM custody c JOIN custody_things t ON t.record = c.digest WHERE c.kind = 'received' AND c.custodian = ?1 AND t.algorithm = 'sha256' AND t.hex = ?2 ORDER BY c.taken_at DESC, c.rowid DESC",
            )?;
            let rows = q.query_map(params![cd, h], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await?;
    let of = |e: &str| crate::custody::statement_in(e).and_then(|s| s.body.get("component").and_then(Value::as_str).map(str::to_owned));
    Ok(rows.into_iter().find(|(_, e)| of(e).as_deref() == Some(component.as_str())).map(|(d, _)| d))
}

/// A sync of an app as kept: its id, cluster, the cluster's receipt and the containers its
/// manifests named.
struct Sync {
    id: i64,
    cluster: String,
    receipt: String,
    named: Vec<Named>,
}

/// `app`'s syncs with custody, newest first, at most [`SYNCS`].
async fn syncs(cd: &Cd, app: &str) -> anyhow::Result<Vec<Sync>> {
    let a = app.to_owned();
    let rows: Vec<(i64, String, String, String)> = cd
        .hub
        .store
        .call(move |c| {
            let mut q = c.prepare("SELECT id, cluster, receipt, containers FROM cd_custody_syncs WHERE app = ?1 ORDER BY id DESC LIMIT ?2")?;
            let rows = q.query_map(params![a, SYNCS as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await?;
    Ok(rows.into_iter().map(|(id, cluster, receipt, named)| Sync { id, cluster, receipt, named: serde_json::from_str(&named).unwrap_or_default() }).collect())
}

/// What kiln's registry holds by digest `hex`: its media type and body.
async fn kilns_manifest(cd: &Cd, hex: &str) -> anyhow::Result<Option<(String, Value)>> {
    let d = format!("sha256:{hex}");
    let row: Option<(String, Vec<u8>)> = cd.hub.store.call(move |c| Ok(c.query_row("SELECT media_type, body FROM registry_manifests WHERE digest = ?1 LIMIT 1", [d], |r| Ok((r.get(0)?, r.get(1)?))).optional()?)).await?;
    Ok(row.and_then(|(t, b)| serde_json::from_slice(&b).ok().map(|v| (t, v))))
}

/// Image `name` (`hex`) for `platform` (`linux/amd64`): the manifest a node of it pulls and its
/// config, read from kiln's registry by digest when kiln holds it (the same bytes wherever they're
/// pulled from), else from the image's registry.
async fn resolve(cd: &Cd, name: &str, hex: &str, platform: &str) -> anyhow::Result<Resolution> {
    let key = (hex.to_owned(), platform.to_owned());
    if let Some(r) = cd.custody.resolved.lock().get(&key) {
        return Ok(r.clone());
    }
    let (manifest, config) = match kilns_manifest(cd, hex).await? {
        Some((media_type, v)) => {
            let manifest = if deployed::is_index(&media_type, &v) {
                deployed::platform_entry(&v, platform).ok_or_else(|| anyhow::anyhow!("{name}@sha256:{hex} has no image for {platform}"))?
            } else {
                hex.to_owned()
            };
            let body = if manifest == hex { Some(v) } else { kilns_manifest(cd, &manifest).await?.map(|(_, b)| b) };
            let config = body.as_ref().and_then(deployed::config_of).ok_or_else(|| anyhow::anyhow!("kiln's registry has no manifest sha256:{manifest} of {name} for {platform}"))?;
            (manifest, config)
        }
        None => {
            use crate::ci::integrations::{self, oci};
            let reference = oci::parse_reference(&format!("{name}@sha256:{hex}"))?;
            let registry = integrations::registry_for(&cd.hub, &reference.registry).await?.unwrap_or_else(|| oci::Registry::for_name(&reference.registry, None, false));
            let (os, arch) = platform.split_once('/').unwrap_or((platform, ""));
            let arch = arch.split('/').next().unwrap_or(arch);
            let p = oci::manifest_for(&registry, &reference, os, arch).await?;
            (p.digest.trim_start_matches("sha256:").to_owned(), p.config.trim_start_matches("sha256:").to_owned())
        }
    };
    let r = Resolution { image: hex.to_owned(), platform: platform.to_owned(), manifest, config };
    cd.custody.resolved.lock().insert(key, r.clone());
    Ok(r)
}

/// Node `node`'s platform (`linux/amd64`), as its status says.
async fn platform_of(cd: &Cd, cluster: &super::kube::Cluster, node: &str) -> anyhow::Result<String> {
    let key = (cluster.name.clone(), node.to_owned());
    if let Some(p) = cd.custody.platforms.lock().get(&key) {
        return Ok(p.clone());
    }
    let n = cluster.get(&ResKey::new("", "Node", "", node)).await?.ok_or_else(|| anyhow::anyhow!("no node {node}"))?;
    let info = |k: &str| obj::str_at(&n, &["status", "nodeInfo", k]).unwrap_or_default().to_owned();
    let p = format!("{}/{}", info("operatingSystem"), info("architecture"));
    cd.custody.platforms.lock().insert(key, p.clone());
    Ok(p)
}

/// One container of one pod, as a look found it.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Seen {
    pub namespace: String,
    pub pod: String,
    pub uid: String,
    pub node: String,
    pub platform: String,
    pub container: String,
    /// Its image as the pod's spec names it now.
    pub image: String,
    /// Its image ID as its status reports it.
    pub image_id: String,
    /// The SHA-256 its deploy handed on for its node's platform, and what it runs.
    pub wants: String,
    pub runs: String,
    /// The pod's receipt.
    pub record: String,
    pub follows: bool,
    pub why: String,
}

/// What a look at `app`'s pods leaves its deploy: from the last few seconds' look, unless `fresh`.
pub async fn verdict(cd: &Arc<Cd>, app: &str, fresh: bool) -> Verdict {
    if !fresh
        && let Some((at, v)) = cd.custody.looked.lock().get(app)
        && at.elapsed() < FRESH
    {
        return v.clone();
    }
    let v = match look(cd, app).await {
        Ok((v, _)) => v,
        Err(e) => Verdict::Waiting { why: format!("its pods couldn't be looked at: {e:#}") },
    };
    cd.custody.looked.lock().insert(app.to_owned(), (Instant::now(), v.clone()));
    v
}

/// `app`'s custody to production for the API: its last sync's records, each pod's container, and
/// the verdict.
pub async fn view(cd: &Arc<Cd>, app: &str) -> anyhow::Result<Value> {
    let (verdict, seen) = look(cd, app).await?;
    cd.custody.looked.lock().insert(app.to_owned(), (Instant::now(), verdict.clone()));
    let a = app.to_owned();
    let last: Option<Value> = cd
        .hub
        .store
        .call(move |c| {
            Ok(c.query_row(
                "SELECT id, cluster, revision, op, hand_on, receipt, manifests, held, objects, containers, why, at FROM cd_custody_syncs WHERE app = ?1 ORDER BY id DESC LIMIT 1",
                [a],
                |r| {
                    let containers: String = r.get(9)?;
                    Ok(json!({
                        "id": r.get::<_, i64>(0)?, "cluster": r.get::<_, String>(1)?, "revision": r.get::<_, String>(2)?, "op": r.get::<_, i64>(3)?,
                        "hand_on": r.get::<_, String>(4)?, "receipt": r.get::<_, String>(5)?, "manifests": format!("sha256:{}", r.get::<_, String>(6)?),
                        "held": format!("sha256:{}", r.get::<_, String>(7)?), "objects": r.get::<_, i64>(8)?,
                        "containers": serde_json::from_str::<Value>(&containers).unwrap_or(Value::Null), "why": r.get::<_, String>(10)?, "at": r.get::<_, i64>(11)?,
                    }))
                },
            )
            .optional()?)
        })
        .await?;
    Ok(json!({ "app": app, "verdict": verdict, "sync": last, "pods": seen }))
}

/// Looks at `app`'s pods now, recording each container's custody from its sync's receipt to what it
/// runs: the verdict, and what was seen.
async fn look(cd: &Arc<Cd>, app: &str) -> anyhow::Result<(Verdict, Vec<Seen>)> {
    let syncs = syncs(cd, app).await?;
    let Some(last) = syncs.first() else { return Ok((Verdict::Waiting { why: format!("no sync of {app} has custody to its cluster yet") }, Vec::new())) };
    // A sync applies only what differs where its app says so (ApplyOutOfSyncOnly): each container
    // is named by the newest sync that applied its workload.
    if syncs.iter().all(|s| s.named.is_empty()) {
        return Ok((Verdict::Verified { pods: 0, containers: 0 }, Vec::new()));
    }
    let cluster = cd.clusters.read().get(&last.cluster).cloned().ok_or_else(|| anyhow::anyhow!("cluster {} isn't connected", last.cluster))?;
    let tree = cd.live.tree(cd, app, true).await.map_err(|(_, why)| anyhow::anyhow!(why))?;
    // Each live pod of the app, with the workload of the app's that owns it, and the images its
    // controller's template gave it where that template is fixed for its pods (a ReplicaSet's, a
    // Job's): what the cluster handed this pod, whatever its own spec says now.
    let mut pods: Vec<(String, String, String, BTreeMap<String, String>)> = Vec::new();
    for n in tree.tree.nodes.iter().filter(|n| n.group.is_empty() && n.kind == "Pod" && n.uid.is_some()) {
        let owners = tree.owners(n);
        let top = std::iter::once(n).chain(owners.iter().copied()).rfind(|o| o.managed);
        let Some(w) = top else { continue };
        let given: BTreeMap<String, String> = owners
            .first()
            .filter(|o| matches!((o.group.as_str(), o.kind.as_str()), ("apps", "ReplicaSet") | ("batch", "Job")))
            .map(|o| o.containers.iter().map(|c| (c.name.clone(), c.image.clone())).collect())
            .unwrap_or_default();
        pods.push((n.namespace.clone(), n.uid.clone().unwrap_or_default(), format!("{}/{}:{}/{}", w.group, w.kind, w.namespace, w.name), given));
    }
    let mut namespaces: Vec<&str> = pods.iter().map(|(ns, ..)| ns.as_str()).collect();
    namespaces.sort_unstable();
    namespaces.dedup();
    let mut live: HashMap<String, Value> = HashMap::new();
    for ns in namespaces {
        for p in cluster.list_in(&GroupKind::new("", "Pod"), Some(ns), None, false).await? {
            if let Some(uid) = obj::str_at(&p, &["metadata", "uid"]) {
                live.insert(uid.to_owned(), p);
            }
        }
    }
    let (mut seen, mut waiting, mut unverifiable, mut broken) = (Vec::new(), None::<String>, None::<String>, None::<String>);
    let mut counted = std::collections::BTreeSet::new();
    for (_, uid, workload, given) in &pods {
        let Some(p) = live.get(uid) else { continue };
        if obj::get(p, &["metadata", "deletionTimestamp"]).is_some_and(|t| !t.is_null()) {
            continue;
        }
        let (ns, name) = (obj::namespace(p).to_owned(), obj::name(p).to_owned());
        let Some(node) = obj::str_at(p, &["spec", "nodeName"]).filter(|n| !n.is_empty()).map(str::to_owned) else {
            waiting.get_or_insert(format!("pod {ns}/{name} isn't scheduled yet"));
            continue;
        };
        let specs: Vec<Named> = containers_of(workload, p);
        let statuses = ["initContainerStatuses", "containerStatuses"].iter().flat_map(|k| obj::list_at(p, &["status", *k]).into_iter().flatten()).cloned().collect::<Vec<Value>>();
        for c in specs {
            // The containers its deploy names; another (a webhook's sidecar) isn't the deploy's.
            let newest = syncs.iter().find_map(|s| s.named.iter().find(|x| x.workload == *workload && x.container == c.container).map(|x| (s, x)));
            let Some((named_by, last_named)) = newest else { continue };
            // What it should run: what its controller's template gave it, else (a StatefulSet's, a
            // DaemonSet's, the app's own pod) its own image; either only when a recorded sync handed
            // that on (a pod of an earlier rollout runs its own sync's), else what the newest sync of
            // its workload names.
            let handed = deployed::pinned(given.get(&c.container).unwrap_or(&c.image));
            let from = syncs.iter().find(|s| s.named.iter().any(|x| x.workload == *workload && x.container == c.container && deployed::pinned(&x.image).is_some_and(|x| Some(&x) == handed.as_ref())));
            let (sync, wanted) = match from {
                Some(s) => (s, handed.clone()),
                None => (named_by, deployed::pinned(&last_named.image)),
            };
            let Some((image_name, image_hex)) = wanted else {
                unverifiable.get_or_insert(format!("{workload}'s container {} runs {}, named by tag alone: what runs can't be compared with it by digest", c.container, last_named.image));
                continue;
            };
            let reported = statuses.iter().find(|s| s.get("name").and_then(Value::as_str) == Some(c.container.as_str())).and_then(|s| s.get("imageID").and_then(Value::as_str)).unwrap_or_default().to_owned();
            if deployed::image_id(&reported).is_none() {
                waiting.get_or_insert(format!("pod {ns}/{name}'s container {} has no image yet", c.container));
                continue;
            }
            let platform = platform_of(cd, &cluster, &node).await?;
            let r = resolve(cd, &image_name, &image_hex, &platform).await?;
            let s = record_pod(cd, app, sync, &node, &ns, &name, uid, &c, &image_name, &r, &reported).await?;
            counted.insert(uid.clone());
            if !s.follows {
                broken.get_or_insert(s.why.clone());
            }
            seen.push(s);
        }
    }
    let containers = seen.len();
    let v = match (broken, unverifiable, waiting) {
        (Some(why), ..) => Verdict::Broken { why },
        (None, Some(why), _) => Verdict::Unverifiable { why },
        (None, None, Some(why)) => Verdict::Waiting { why },
        (None, None, None) if containers == 0 => Verdict::Waiting { why: format!("no pod of {app} runs yet") },
        (None, None, None) => Verdict::Verified { pods: counted.len(), containers },
    };
    Ok((v, seen))
}

/// A pod's container's custody: the cluster's hand-on of its image to its node and the node's
/// receipt, the node's hand-on of its platform's manifest to the pod, and the pod's receipt of what
/// it runs, as `reported`; kept once for each image it reports.
#[allow(clippy::too_many_arguments)]
async fn record_pod(cd: &Cd, app: &str, sync: &Sync, node: &str, ns: &str, pod: &str, uid: &str, c: &Named, name: &str, r: &Resolution, reported: &str) -> anyhow::Result<Seen> {
    let (u, cn, rep) = (uid.to_owned(), c.container.clone(), reported.to_owned());
    let kept: Option<(String, String, i64, String)> = cd
        .hub
        .store
        .call(move |db| Ok(db.query_row("SELECT runs, record, follows, why FROM cd_custody_pods WHERE pod_uid = ?1 AND container = ?2 AND image_id = ?3", params![u, cn, rep], |x| Ok((x.get(0)?, x.get(1)?, x.get(2)?, x.get(3)?))).optional()?))
        .await?;
    let seen = |runs: String, record: String, follows: bool, why: String| Seen {
        namespace: ns.to_owned(),
        pod: pod.to_owned(),
        uid: uid.to_owned(),
        node: node.to_owned(),
        platform: r.platform.clone(),
        container: c.container.clone(),
        image: c.image.clone(),
        image_id: reported.to_owned(),
        wants: r.manifest.clone(),
        runs,
        record,
        follows,
        why,
    };
    if let Some((runs, record, follows, why)) = kept {
        return Ok(seen(runs, record, follows != 0, why));
    }
    let org = cd.hub.pki.organization();
    let server = crate::custody::server_uri(&org);
    let came_from = format!("KilnCD's look at {app}'s pods");
    let image = deployed::image(name, &r.image);
    let scheduled = format!("the pods of {app} it scheduled there run {name}@sha256:{}", r.image);
    let (given, _) = as_server(&cd.hub, &deployed::to_node(&server, &sync.cluster, node, vec![image.clone()], &sync.receipt, &scheduled), &came_from).await?;
    // What the node pulls for its platform, said on its receipt: the index's resolution follows it.
    let mut pulled = image;
    pulled.annotations.insert("pulls".into(), json!(format!("sha256:{}", r.manifest)));
    pulled.annotations.insert("platform".into(), json!(r.platform));
    let (got, _) = as_server(&cd.hub, &deployed::node_receipt(&server, &sync.cluster, node, vec![pulled], &given, &format!("{name}@sha256:{}, as its pods' spec names it", r.image)), &came_from).await?;
    let pod_uri = deployed::pod_uri(&sync.cluster, ns, pod);
    let container = format!("pod {ns}/{pod} (uid {uid})'s container {} on node {node}", c.container);
    let (to_pod, _) = as_server(&cd.hub, &deployed::to_pod(&server, &sync.cluster, node, &pod_uri, name, r, &got, &container), &came_from).await?;
    let (runs, follows, why) = match deployed::runs(r, reported) {
        Runs::Follows => (r.manifest.clone(), true, String::new()),
        Runs::Other(hex) => {
            let why = format!("{container} runs sha256:{hex} ({reported}), which doesn't follow from what its deploy handed on: {name}@sha256:{}, sha256:{} on {}", r.image, r.manifest, r.platform);
            (hex, false, why)
        }
        Runs::Unknown => anyhow::bail!("{container} reports no image yet"),
    };
    let (record, broke) = as_server(&cd.hub, &deployed::pod_receipt(&server, &sync.cluster, node, &pod_uri, name, r, &runs, reported, &to_pod, &container), &came_from).await?;
    if follows && broke.is_some() {
        tracing::warn!(target: "kiln.custody", %app, %pod, broke = broke.as_deref().unwrap_or_default(), "a pod's receipt breaks its chain");
    }
    let follows = follows && broke.is_none();
    let why = if follows { why } else { broke.map_or(why.clone(), |b| if why.is_empty() { b } else { why.clone() }) };
    let row = (app.to_owned(), sync.id, sync.cluster.clone(), ns.to_owned(), pod.to_owned(), uid.to_owned(), node.to_owned(), r.platform.clone(), c.container.clone(), c.image.clone(), reported.to_owned(), r.manifest.clone(), runs.clone(), record.clone(), i64::from(follows), why.clone());
    let now = crate::util::now_unix();
    cd.hub
        .store
        .call(move |db| {
            db.execute(
                "INSERT OR IGNORE INTO cd_custody_pods (app, sync, cluster, namespace, pod, pod_uid, node, platform, container, image, image_id, wants, runs, record, follows, why, seen_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
                params![row.0, row.1, row.2, row.3, row.4, row.5, row.6, row.7, row.8, row.9, row.10, row.11, row.12, row.13, row.14, row.15, now],
            )?;
            Ok(())
        })
        .await?;
    if !follows {
        tracing::warn!(target: "kiln.custody", %app, %pod, %why, "a pod runs what its deploy didn't hand on");
    }
    Ok(seen(runs, record, follows, why))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w27_r08_a_syncs_containers_are_read_from_each_kind_of_workload() {
        let d = json!({"kind": "Deployment", "apiVersion": "apps/v1", "spec": {"template": {"spec": {"initContainers": [{"name": "migrate", "image": "acme/web@sha256:aa"}], "containers": [{"name": "web", "image": "acme/web:1"}]}}}});
        let c = containers_of("apps/Deployment:shop/web", &d);
        assert_eq!(c.iter().map(|x| (x.container.as_str(), x.image.as_str())).collect::<Vec<_>>(), vec![("migrate", "acme/web@sha256:aa"), ("web", "acme/web:1")]);
        let cron = json!({"kind": "CronJob", "apiVersion": "batch/v1", "spec": {"jobTemplate": {"spec": {"template": {"spec": {"containers": [{"name": "tick", "image": "acme/tick"}]}}}}}});
        assert_eq!(containers_of("batch/CronJob:shop/tick", &cron).len(), 1);
        let pod = json!({"kind": "Pod", "apiVersion": "v1", "spec": {"containers": [{"name": "one", "image": "busybox"}]}});
        assert_eq!(containers_of("/Pod:shop/one", &pod).len(), 1);
        assert!(containers_of("/ConfigMap:shop/c", &json!({"kind": "ConfigMap", "data": {}})).is_empty());
    }

    #[test]
    fn w27_r08_an_applied_object_is_kept_by_its_app_and_key() {
        let k = Custody::default();
        let o = json!({"kind": "Service", "metadata": {"name": "web", "annotations": {"kiln.dev/tracking": "shop/web-production:/Service:shop/web"}}});
        k.applied(&o, &o, "kiln");
        k.applied(&json!({"kind": "Service", "metadata": {"name": "untracked"}}), &o, "kiln");
        let got = k.take_applied("shop/web-production");
        assert_eq!(got.keys().collect::<Vec<_>>(), vec!["/Service:shop/web"]);
        assert!(k.take_applied("shop/web-production").is_empty(), "taken once");
    }

    /// W27-R08 (the Transfers plan's step 14): a sync's records, signed and kept. CD hands cluster e2e
    /// what it applied (a Deployment naming an image from outside kiln, whose chain begins at CD's
    /// receipt of it); kiln receives it for the cluster as the API server answered: unbroken. Another
    /// sync, whose answer holds another image (a webhook's), breaks the chain at the cluster's
    /// receipt, as changed, saying where.
    #[test]
    fn w27_r08_a_syncs_records_hold_or_break_at_the_clusters_receipt() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            crate::identities::own_identities(&hub.store, &hub.pki).await?;
            let (org, now) = (hub.pki.organization(), crate::util::now_unix());
            let deployment = |image: &str| json!({"apiVersion": "apps/v1", "kind": "Deployment", "metadata": {"name": "web", "namespace": "shop"}, "spec": {"template": {"spec": {"containers": [{"name": "web", "image": image}]}}}});
            let image = format!("ghcr.io/acme/web@sha256:{}", "a".repeat(64));
            let applied = |answer: Value| BTreeMap::from([("apps/Deployment:shop/web".to_owned(), Applied { sent: deployment(&image), answer, manager: "kiln".into() })]);
            let r = record_sync(&hub, "e2e", "shop/web", 1, "4c1e2a", now - 60, &applied(deployment(&image))).await?;
            anyhow::ensure!(r.sent == r.held && r.broke.is_none() && r.otherwise.is_none(), "held as sent: {:?} {:?}", r.broke, r.otherwise);
            assert_eq!((r.objects, r.named.len(), r.images.len()), (1, 1, 1));
            let chain = crate::custody::chain(&hub.store, &hub.pki.ca_der, &org, &r.receipt, now).await?.ok_or_else(|| anyhow::anyhow!("no chain from the cluster's receipt"))?;
            anyhow::ensure!(chain.get("unbroken") == Some(&json!(true)), "{chain}");
            let hops: Vec<(String, String, String)> = chain
                .get("hops")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(|h| {
                    let t = |k: &str| h.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
                    (t("kind"), t("custodian"), t("taken_for"))
                })
                .collect();
            let (server, cd) = (crate::custody::server_uri(&org), crate::custody::cd_uri(&org));
            let want = [("received", server.as_str(), "kubernetes://e2e"), ("handed-on", cd.as_str(), ""), ("received", cd.as_str(), "")];
            assert_eq!(hops, want.map(|(k, c, f)| (k.to_owned(), c.to_owned(), f.to_owned())).to_vec(), "{chain}");
            // A webhook put another image in what the API server holds: the cluster's receipt breaks.
            let other = deployment(&format!("mirror.example/acme/web@sha256:{}", "b".repeat(64)));
            let r = record_sync(&hub, "e2e", "shop/web", 2, "4c1e2a", now, &applied(other)).await?;
            anyhow::ensure!(r.sent != r.held, "the cluster holds another");
            anyhow::ensure!(r.otherwise.as_deref().is_some_and(|o| o.contains("apps/Deployment:shop/web") && o.contains("image")), "where: {:?}", r.otherwise);
            anyhow::ensure!(r.broke.as_deref().is_some_and(|b| b.contains("changed between") && b.contains(&r.sent) && b.contains(&r.held)), "{:?}", r.broke);
            let breaks = crate::custody::breaks(&hub.store, 10).await?;
            anyhow::ensure!(breaks.iter().any(|b| b.get("record") == Some(&json!(r.receipt)) && b.get("kind") == Some(&json!("changed"))), "{breaks:?}");
            Ok(())
        })
    }

    #[test]
    fn w27_r08_a_secrets_values_never_reach_a_digest() {
        let mut secret = json!({"apiVersion": "v1", "kind": "Secret", "metadata": {"name": "db"}, "data": {"password": "aHVudGVyMg=="}});
        masked(&mut secret);
        assert_eq!(secret.pointer("/data/password"), Some(&json!("")), "its key kept, its value gone");
        let mut map = json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "c"}, "data": {"color": "blue"}});
        masked(&mut map);
        assert_eq!(map.pointer("/data/color"), Some(&json!("blue")));
    }

    #[test]
    fn w27_r08_what_the_api_server_holds_of_what_cd_sent() {
        let sent = json!({"apiVersion": "apps/v1", "kind": "Deployment", "metadata": {"name": "web", "namespace": "shop"}, "spec": {"replicas": 2, "template": {"spec": {"containers": [{"name": "web", "image": "acme/web@sha256:aa"}]}}}});
        let mut answer = sent.clone();
        if let Some(m) = answer.get_mut("metadata").and_then(Value::as_object_mut) {
            m.insert("uid".into(), json!("u-1"));
            m.insert("resourceVersion".into(), json!("42"));
        }
        if let Some(c) = answer.pointer_mut("/spec/template/spec/containers/0").and_then(Value::as_object_mut) {
            c.insert("imagePullPolicy".into(), json!("IfNotPresent"));
        }
        let (held, differs) = held_of(&sent, &answer, "kiln");
        assert_eq!((held, differs), (sent.clone(), None), "defaults and the server's own fields aren't what CD sent");
        if let Some(c) = answer.pointer_mut("/spec/template/spec/containers/0").and_then(Value::as_object_mut) {
            c.insert("image".into(), json!("mirror.example/acme/web@sha256:bb"));
        }
        let (held, differs) = held_of(&sent, &answer, "kiln");
        assert_ne!(held, sent, "a webhook changed the image: the cluster holds another");
        assert!(differs.is_some_and(|p| p.contains("image")));
    }
}
