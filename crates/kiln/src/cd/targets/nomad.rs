//! `nomad`'s I/O half (docs/cd-targets.md §3): Nomad's HTTP API, at the integration's address,
//! with its ACL token. The pure half, `kiln_cd::providers::nomad`, renders the objects.
//!
//! A release registers the job (its HCL parsed by Nomad first), a version Nomad rolls out by its
//! update block, and kiln waits for that version's deployment: successful, failed, or paused for a
//! person to promote its canaries. kiln keeps the versions it registered by release, so a rollback
//! to one of them reverts the job to it.

use std::sync::OnceLock;

use anyhow::{Context as _, Result, bail};
use kiln_cd::obj::{self, ResKey};
use kiln_cd::providers::nomad::{self as p, GROUP};
use serde_json::{Map, Value, json};

use super::{Cx, applied, until};
use crate::ci::integrations::aws::encode;
use crate::ci::integrations::hosting::{HostingError, error_of};
use crate::http::client::{Client, Method, Roots};

/// The longest a deployment takes.
const DEPLOYED: std::time::Duration = std::time::Duration::from_secs(1800);
/// How many of its versions kiln keeps to revert to.
const KEEP: usize = 20;

fn client() -> Result<&'static Client> {
    static C: OnceLock<Client> = OnceLock::new();
    if let Some(c) = C.get() {
        return Ok(c);
    }
    let c = Client::new(Roots::WebPki, None)?;
    Ok(C.get_or_init(|| c))
}

/// A call to Nomad's API: its answer, or Nomad's error.
async fn call(cx: &Cx<'_>, method: Method, path: &str, body: Option<&Value>) -> Result<Value> {
    let (token, config) = cx.token()?;
    let address = config.get("address").and_then(Value::as_str).filter(|a| !a.is_empty()).context("the Nomad integration has no address")?.trim_end_matches('/').to_owned();
    let base = crate::ci::integrations::endpoint("nomad", || address);
    cx.conn.budget().await;
    let mut req = client()?.request(method, &format!("{base}{path}")).timeout(std::time::Duration::from_secs(60)).header("X-Nomad-Token", token);
    if let Some(b) = body {
        req = req.json(b);
    }
    let resp = req.send().await.with_context(|| format!("Nomad {path}"))?;
    let status = resp.status();
    let bytes = resp.bytes(32 << 20).await?;
    if !(200..300).contains(&status) {
        return Err(error_of(status, &bytes).into());
    }
    Ok(serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

fn missing<T>(r: Result<T>) -> Result<Option<T>> {
    match r {
        Ok(v) => Ok(Some(v)),
        Err(e) if HostingError::is_not_found(&e) => Ok(None),
        Err(e) => Err(e),
    }
}

fn job_path(ns: &str, id: &str, rest: &str) -> String {
    format!("/v1/job/{}{rest}?namespace={}", encode(id, false), encode(ns, false))
}

async fn job(cx: &Cx<'_>, ns: &str, id: &str) -> Result<Option<Value>> {
    missing(call(cx, Method::Get, &job_path(ns, id, ""), None).await)
}

async fn deployment(cx: &Cx<'_>, ns: &str, id: &str) -> Result<Option<Value>> {
    Ok(missing(call(cx, Method::Get, &job_path(ns, id, "/deployment"), None).await)?.filter(|d| !d.is_null()))
}

async fn live(cx: &Cx<'_>, ns: &str, id: &str) -> Result<Option<Value>> {
    let Some(j) = job(cx, ns, id).await? else { return Ok(None) };
    let d = deployment(cx, ns, id).await?;
    let rec = cx.applied(GROUP, "Job", ns, id);
    let ours = rec.as_ref().is_some_and(|r| r.made.get("version").is_some_and(|v| !v.is_null() && Some(v) == j.get("Version")));
    Ok(p::live_job(&j, d.as_ref(), rec.as_ref().map(|r| &r.object), ours))
}

pub async fn list(cx: &Cx<'_>, kind: &str, names: &[(String, String)]) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    for (ns, id) in names {
        if kind == "Job" {
            out.extend(live(cx, if ns.is_empty() { "default" } else { ns }, id).await?);
        }
    }
    Ok(out)
}

/// What the cluster runs: its jobs, in every namespace (§8.2, §8.3).
pub async fn discover(cx: &Cx<'_>) -> Result<Value> {
    let jobs = call(cx, Method::Get, "/v1/jobs?namespace=*", None).await?;
    let out: Vec<Value> = jobs.as_array().into_iter().flatten().map(|j| json!({ "kind": "Job", "name": j.get("ID"), "scope": j.get("Namespace"), "summary": format!("{} · {}", j.get("Type").and_then(Value::as_str).unwrap_or_default(), j.get("Status").and_then(Value::as_str).unwrap_or_default()) })).collect();
    Ok(json!({ "resources": out }))
}

/// A job as it runs now, as the definition that adopts it (§8.2 step 2).
pub async fn adopt(cx: &Cx<'_>, scope: &str, id: &str) -> Result<Value> {
    let ns = if scope.is_empty() { "default" } else { scope };
    let Some(mut j) = job(cx, ns, id).await? else { bail!("no job {id} in {ns}") };
    if let Some(m) = j.as_object_mut() {
        for k in ["Status", "StatusDescription", "Version", "Stable", "SubmitTime", "CreateIndex", "ModifyIndex", "JobModifyIndex"] {
            m.remove(k);
        }
    }
    Ok(json!({ "definition": { "job": j, "namespace": ns }, "note": "kept in the source as the job's JSON (or its HCL), the job reads from there" }))
}

/// Whether a job rolls out through deployments: it has an update block, its own or a group's.
fn deploys(job: &Value) -> bool {
    let has = |u: Option<&Value>| u.is_some_and(|u| !u.is_null() && u.get("MaxParallel").and_then(Value::as_i64) != Some(0));
    job.get("Type").and_then(Value::as_str).is_none_or(|t| t == "service") && (has(job.get("Update")) || job.get("TaskGroups").and_then(Value::as_array).into_iter().flatten().any(|g| has(g.get("Update"))))
}

/// Waits for `version`'s deployment to end: why, if it failed. A deployment paused for a person
/// to promote its canaries is left to them. Its own deployment, found among the job's: one Nomad
/// made afterwards (an auto-revert) is another version's.
async fn deployed(cx: &Cx<'_>, ns: &str, id: &str, version: &Value, what: &str) -> Result<()> {
    let d = until(cx.conn, DEPLOYED, what, || async {
        let all = call(cx, Method::Get, &job_path(ns, id, "/deployments"), None).await?;
        let ours = all.as_array().into_iter().flatten().find(|d| d.get("JobVersion") == Some(version)).cloned();
        Ok(ours.filter(|d| !matches!(d.get("Status").and_then(Value::as_str), Some("running" | "pending" | "initializing"))))
    })
    .await?;
    match d.get("Status").and_then(Value::as_str) {
        Some("successful" | "paused" | "blocked") => Ok(()),
        Some(s) => bail!("{what}: its deployment {s}: {}", d.get("StatusDescription").and_then(Value::as_str).unwrap_or_default()),
        None => bail!("{what}: Nomad answered a deployment without its status"),
    }
}

/// The job JSON a Job object registers: its own, or its HCL as Nomad parses it, `set:` applied.
async fn job_json(cx: &Cx<'_>, desired: &Value) -> Result<Value> {
    let spec = desired.get("spec").cloned().unwrap_or(Value::Null);
    if let Some(j) = spec.get("job") {
        return Ok(j.clone());
    }
    let hcl = spec.get("hcl").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("job {} has no HCL or JSON", obj::name(desired)))?;
    let mut j = call(cx, Method::Post, "/v1/jobs/parse", Some(&json!({ "JobHCL": hcl, "Canonicalize": false }))).await?;
    kiln_cd::providers::apply_sets(&mut j, spec.get("set")).map_err(|e| anyhow::anyhow!("set: {e}"))?;
    if let Some(m) = j.as_object_mut() {
        m.insert("Namespace".into(), json!(obj::namespace(desired)));
    }
    Ok(j)
}

/// Applies a Job: registered unless it's at the release's version already; a version kiln
/// registered before is reverted to.
pub async fn apply(cx: &Cx<'_>, desired: &Value) -> Result<Value> {
    let (id, ns) = (obj::name(desired), obj::namespace(desired));
    let ns = if ns.is_empty() { "default" } else { ns };
    let want = kiln_cd::obj::hash_value(desired.get("spec").unwrap_or(&Value::Null)).to_string();
    let prev = cx.applied(GROUP, "Job", ns, id).map(|a| a.made).unwrap_or(Value::Null);
    let mut versions: Map<String, Value> = prev.get("versions").and_then(Value::as_object).cloned().unwrap_or_default();
    let current = job(cx, ns, id).await?;
    let at = current.as_ref().and_then(|j| j.get("Version")).cloned();
    let what = format!("rolling job {id} out");
    let version = if prev.get("release").and_then(Value::as_str) == Some(want.as_str()) && at.is_some() && at.as_ref() == prev.get("version") {
        at.unwrap_or(Value::Null)
    } else {
        match versions.get(&want).cloned().filter(|v| current.is_some() && Some(v) != at.as_ref()) {
            // A version kiln registered of this release before: the job reverts to it.
            Some(v) => {
                call(cx, Method::Post, &job_path(ns, id, "/revert"), Some(&json!({ "JobID": id, "JobVersion": v, "Namespace": ns }))).await?;
            }
            None => {
                let j = job_json(cx, desired).await?;
                call(cx, Method::Post, &format!("/v1/jobs?namespace={}", encode(ns, false)), Some(&json!({ "Job": j }))).await?;
            }
        }
        let j = job(cx, ns, id).await?.ok_or_else(|| anyhow::anyhow!("{what}: the job is gone"))?;
        let now = j.get("Version").cloned().unwrap_or(Value::Null);
        cx.record(GROUP, "Job", ns, id, Some(applied(desired, json!({ "release": want, "version": now, "versions": versions })))).await?;
        if deploys(&j) {
            deployed(cx, ns, id, &now, &what).await?;
        }
        now
    };
    versions.insert(want.clone(), version.clone());
    while versions.len() > KEEP {
        let Some(k) = versions.keys().next().cloned() else { break };
        versions.remove(&k);
    }
    cx.record(GROUP, "Job", ns, id, Some(applied(desired, json!({ "release": want, "version": version, "versions": versions })))).await?;
    live(cx, ns, id).await?.ok_or_else(|| anyhow::anyhow!("job {id} is gone"))
}

pub async fn delete(cx: &Cx<'_>, key: &ResKey) -> Result<()> {
    let ns = if key.namespace.is_empty() { "default" } else { &key.namespace };
    missing(call(cx, Method::Delete, &job_path(ns, &key.name, ""), None).await)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use kiln_cd::model::Health;

    use super::*;
    use crate::cd::targets::fake::{MemFiles, MemRecords};
    use crate::cd::targets::fake_gcp::synced;
    use crate::cd::targets::fake_nomad::{self, FakeNomad};
    use crate::cd::targets::fake_query::released_to;
    use crate::cd::targets::{Conn, Creds, key_of};

    const HCL: &str = "job \"api\" {\n  type = \"service\"\n  group \"api\" {\n    count = 2\n    update {\n      max_parallel = 1\n      auto_revert = true\n    }\n    task \"api\" {\n      driver = \"docker\"\n      config {\n        image = \"ghcr.io/acme/api:placeholder\"\n      }\n    }\n  }\n}\n";

    /// A job on a faked Nomad, release by release: the first parses its HCL through Nomad,
    /// registers it with the release's image, and its deployment succeeds; the same release
    /// again registers nothing; the next registers its own version; one whose tasks don't start
    /// fails, and Nomad reverts the job to its last stable version; the release before, rolled
    /// back to, reverts the job to the version kiln registered of it; then the job stops.
    #[test]
    fn jobs_roll_out_each_releases_version() -> anyhow::Result<()> {
        let _one = crate::cd::targets::fake_hosting::HOSTING.lock();
        crate::rt::block_on(async {
            let fake = Arc::new(FakeNomad::default());
            fake_nomad::start(fake.clone()).await?;
            let place = json!({ "account": "nomad-test" });
            let (target, _) = kiln_cd::providers::target_of("nomad", place.as_object().ok_or_else(|| anyhow::anyhow!("place"))?).map_err(|e| anyhow::anyhow!("{e}"))?;
            let def = target.def().ok_or_else(|| anyhow::anyhow!("provider"))?;
            assert!(def.built);
            let conn = Conn::quick(target, def);
            let (records, files) = (MemRecords::default(), MemFiles::default());
            let cx = Cx { conn: &conn, creds: Creds::Token { token: fake_nomad::TOKEN.into(), config: json!({ "address": "https://nomad.example.com:4646" }) }, records: &records, files: &files };
            let spec = json!({ "job": "deploy/api.nomad.hcl", "set": { "TaskGroups[Name=api].Tasks[Name=api].Config.image": "ghcr.io/acme/api@${{ release.artifacts.image.digest }}" } });
            let release = |version: &str, digest: &str| -> anyhow::Result<Value> {
                released_to("nomad", &place, spec.clone(), version, &[("deploy/api.nomad.hcl", HCL)], &[("image", digest)])?.into_iter().next().ok_or_else(|| anyhow::anyhow!("no job"))
            };
            let names = [("default".to_owned(), "api".to_owned())];
            let h = |o: &Value| kiln_cd::providers::health(o).map(|s| s.health);
            let image = || fake.state.lock().jobs.get(&("default".to_owned(), "api".to_owned())).and_then(|j| j.versions.last().and_then(|(v, _)| v.pointer("/TaskGroups/0/Tasks/0/Config/image").and_then(Value::as_str).map(str::to_owned))).unwrap_or_default();
            let last_call = || fake.state.lock().calls.last().cloned().unwrap_or_default();

            let j1 = release("1.0.0", "sha256:aaaa")?;
            assert_eq!((obj::name(&j1), obj::namespace(&j1)), ("api", "default"));
            let up = apply(&cx, &j1).await?;
            assert_eq!(h(&up), Some(Health::Healthy), "{up}");
            assert!(synced(&j1, &cx.with_applied(up.clone())), "in sync: {up}");
            assert_eq!(fake.state.lock().calls, ["Register api v0"]);
            assert_eq!(image(), "ghcr.io/acme/api@sha256:aaaa", "set: applied to the job Nomad parsed");
            apply(&cx, &release("1.0.0", "sha256:aaaa")?).await?;
            assert_eq!(fake.state.lock().calls.len(), 1, "the same release again registers nothing");

            let j2 = release("1.1.0", "sha256:bbbb")?;
            let up2 = apply(&cx, &j2).await?;
            assert_eq!((h(&up2), last_call()), (Some(Health::Healthy), "Register api v1".to_owned()));

            // One whose tasks don't start: its deployment fails, and Nomad reverts the job.
            let failed = apply(&cx, &release("1.2.0", "sha256:broken")?).await.err().map(|e| format!("{e:#}")).unwrap_or_default();
            assert!(failed.contains("deployment failed"), "{failed}");
            assert_eq!(image(), "ghcr.io/acme/api@sha256:bbbb", "auto-reverted to the last stable version");

            // Rolled back to the release before that: the job reverted to kiln's version of it.
            let back = apply(&cx, &j1).await?;
            assert!(last_call().starts_with("Revert to v0:"), "{}", last_call());
            assert_eq!(image(), "ghcr.io/acme/api@sha256:aaaa");
            assert!(synced(&j1, &cx.with_applied(back.clone())) && h(&back) == Some(Health::Healthy), "{back}");

            let found = discover(&cx).await?;
            assert_eq!(found.pointer("/resources/0/name"), Some(&json!("api")));
            let adopted = adopt(&cx, "default", "api").await?;
            assert_eq!(adopted.pointer("/definition/job/ID"), Some(&json!("api")), "{adopted}");
            delete(&cx, &key_of(&j1)).await?;
            assert_eq!(last_call(), "Stop api");
            assert_eq!(h(&list(&cx, "Job", &names).await?.into_iter().next().unwrap_or_default()), Some(Health::Degraded), "a stopped job is dead");
            Ok(())
        })
    }
}
