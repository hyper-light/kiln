//! Job-result reuse on the server (docs/ci.md W08-R10). A job that opts in (`reuse:`, kiln) gets
//! an input hash before its leg is queued: its definition, its actions' pins, its images'
//! digests, its platform, the variables it lists, its trust and the git trees of the paths it
//! lists. A leg that succeeded is kept under its hash; a later leg with the same hash, within
//! `max-age`, whose source's artifacts are all still there, takes that result over instead of
//! running: its outputs, steps, logs and artifacts. Untrusted legs hash differently from trusted
//! ones, so neither takes the other's. "Run anyway" (`no_reuse` on a rerun) skips the lookup.

use std::collections::BTreeMap;
use std::time::Duration;

use kiln_ci::plan::LegSpec;
use kiln_ci::plan::reuse::{Inputs, Spec};
use kiln_ci::wire::ActionRef;
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use sha2::Digest as _;

use crate::ci::integrations::{self, oci};
use crate::gitops;
use crate::hub::Hub;
use crate::store::Repo;

/// A git listing's hash: what the listed paths hold at the commit.
fn digest(bytes: &[u8]) -> String {
    hex::encode(sha2::Sha256::digest(bytes))
}

/// The git trees of `patterns` at `sha` in the repository's mirror, or of the whole tree.
async fn trees(hub: &Hub, repo: &Repo, sha: &str, patterns: &[String]) -> Result<BTreeMap<String, String>, String> {
    let mirror = super::events::facts(hub, repo).await.mirror;
    let mut out = BTreeMap::new();
    if patterns.is_empty() {
        let tree = gitops::run_git(hub, &["-C", &mirror, "rev-parse", &format!("{sha}^{{tree}}")], gitops::LS_REMOTE_TIMEOUT).await.map_err(|e| format!("the commit's tree: {e:#}"))?;
        out.insert("/".to_owned(), String::from_utf8_lossy(&tree).trim().to_owned());
        return Ok(out);
    }
    let listing = gitops::run_git(hub, &["-C", &mirror, "ls-tree", "-r", "--full-tree", sha], gitops::LS_REMOTE_TIMEOUT).await.map_err(|e| format!("the commit's files: {e:#}"))?;
    let listing = String::from_utf8_lossy(&listing).into_owned();
    for p in patterns {
        out.insert(p.clone(), digest(matching(&listing, p).as_bytes()));
    }
    Ok(out)
}

/// The lines of a `git ls-tree -r` listing (`<mode> <type> <object>\t<path>`) whose path the
/// pattern names: a glob (`src/**/*.rs`), or a file or a directory as it is.
fn matching(listing: &str, pattern: &str) -> String {
    let pattern = pattern.trim().trim_start_matches("./").trim_end_matches('/');
    let glob = pattern.contains(['*', '?', '[']);
    listing
        .lines()
        .filter(|line| {
            let path = line.split_once('\t').map_or("", |(_, p)| p);
            if glob { crate::pipeline::glob_match(pattern, path) } else { path == pattern || path.strip_prefix(pattern).is_some_and(|rest| rest.starts_with('/')) }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// An image's digest: a reference's own, else its registry's answer for the tag.
async fn image_digest(hub: &Hub, image: &str) -> Option<String> {
    let r = oci::parse_reference(image).ok()?;
    if let Some(d) = &r.digest {
        return Some(d.clone());
    }
    let registry = integrations::registry_for(hub, &r.registry).await.ok().flatten().unwrap_or_else(|| oci::Registry::for_name(&r.registry, None, false));
    crate::rt::timeout(Duration::from_secs(10), oci::resolve(&registry, &r)).await.ok()?.ok()?.map(|x| x.digest)
}

/// Whether the leg opts in, and how.
pub fn spec_of(leg: &LegSpec) -> Option<Spec> {
    let v = leg.def()?.kiln.reuse.as_ref()?;
    match Spec::from_yval(v) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(leg = %leg.key, error = %e, "the job's reuse doesn't read; it runs");
            None
        }
    }
}

/// The leg's inputs, gathered; why not when one can't be known (then it runs).
#[allow(clippy::too_many_arguments)]
pub async fn inputs(hub: &Hub, repo: &Repo, sha: &str, leg: &LegSpec, reuse: &Spec, actions: &BTreeMap<String, ActionRef>, vars: &Value, trusted: bool) -> Result<Inputs, String> {
    let job = leg.def().ok_or("the job isn't in the workflow")?;
    let wf = &leg.workflow;
    let json = |v: serde_json::Result<Value>| v.map_err(|e| format!("its definition: {e}"));
    let definition = json!({
        "leg": json(serde_json::to_value(leg))?,
        "job": json(serde_json::to_value(job))?,
        "workflow": { "env": json(serde_json::to_value(&wf.env))?, "defaults": json(serde_json::to_value(&wf.defaults))? },
    });
    let pins: BTreeMap<String, String> = job.steps.iter().filter_map(|s| s.uses.as_deref()).filter_map(|u| actions.get(u).map(|a| (u.to_owned(), a.sha.clone()))).collect();
    let mut images = BTreeMap::new();
    for image in leg.container.iter().chain(leg.services.iter().map(|(_, i)| i)).filter(|i| !i.is_empty()) {
        images.insert(image.clone(), image_digest(hub, image).await.ok_or_else(|| format!("{image}'s digest"))?);
    }
    let platform = json!({ "runs_on": leg.runs_on, "requires": leg.requires, "emulated": leg.emulated });
    // The listed variables: a `vars` value, else the workflow's or the job's `env` as written.
    let mut env = BTreeMap::new();
    for name in &reuse.env {
        let v = vars.get(name).and_then(Value::as_str).map(str::to_owned).or_else(|| job.env.iter().chain(wf.env.iter()).find(|(k, _)| k == name).map(|(_, t)| t.src.clone()));
        if let Some(v) = v {
            env.insert(name.clone(), v);
        }
    }
    let trees = trees(hub, repo, sha, &reuse.paths).await?;
    Ok(Inputs { definition, actions: pins, images, platform, env, trusted, trees })
}

/// A leg that succeeded under `hash` within `max_age` seconds, whose artifacts are all still
/// there: it and the report it gave.
pub async fn lookup(hub: &Hub, hash: &str, max_age: u64) -> anyhow::Result<Option<(String, String)>> {
    let (hash, since) = (hash.to_owned(), crate::util::now_unix().saturating_sub(i64::try_from(max_age).unwrap_or(i64::MAX)));
    hub.store
        .call(move |c| {
            Ok(c.query_row(
                "SELECT r.leg_id, r.report FROM ci_reuse r JOIN jobs j ON j.id = r.leg_id
                 WHERE r.input_hash = ?1 AND r.created_at >= ?2 AND j.status = 'succeeded'
                   AND NOT EXISTS (SELECT 1 FROM ci_artifacts a WHERE a.leg_id = r.leg_id AND (a.finalized = 0 OR (a.expires_at IS NOT NULL AND a.expires_at <= ?3)))
                 ORDER BY r.created_at DESC LIMIT 1",
                params![hash, since, crate::util::now_unix()],
                |x| Ok((x.get(0)?, x.get(1)?)),
            )
            .optional()?)
        })
        .await
}

/// A leg that succeeded under `hash`: kept for those that come after.
pub async fn remember(hub: &Hub, leg: &str, run: &str, hash: &str, report: &str) -> anyhow::Result<()> {
    let (leg, run, hash, report) = (leg.to_owned(), run.to_owned(), hash.to_owned(), report.to_owned());
    hub.store
        .call(move |c| {
            c.execute("INSERT OR REPLACE INTO ci_reuse (input_hash, leg_id, run_id, report, created_at) VALUES (?1, ?2, ?3, ?4, ?5)", params![hash, leg, run, report, crate::util::now_unix()])?;
            Ok(())
        })
        .await
}

/// A leg of `run` that takes `source`'s result over: a copy of it, its steps and its artifacts,
/// `reused_from` naming it, `input_hash` its hash.
pub async fn take_over(hub: &Hub, run: &str, source: &str, id: &str, hash: &str) -> anyhow::Result<()> {
    let (run, source, id, hash) = (run.to_owned(), source.to_owned(), id.to_owned(), hash.to_owned());
    hub.store
        .call(move |c| {
            let tx = crate::store::tx(c)?;
            tx.execute(
                "INSERT INTO jobs (id, run_id, key, display, status, labels, needs, spec, agent, executor, attempt, queued_at, started_at, finished_at, exit_code, error,
                                   job_id, matrix, leg_index, outputs, conclusion, requirements, routing, concurrency_group, priority, input_hash, reused_from, environment, leg, run_attempt)
                 SELECT ?1, ?3, key, display, status, labels, needs, spec, agent, executor, attempt, queued_at, started_at, finished_at, exit_code, error,
                        job_id, matrix, leg_index, outputs, conclusion, requirements, routing, concurrency_group, priority, ?4, COALESCE(reused_from, id), environment, leg,
                        (SELECT attempt FROM runs WHERE id = ?3)
                 FROM jobs WHERE id = ?2",
                params![id, source, run, hash],
            )?;
            tx.execute(
                "INSERT INTO steps (job_id, idx, name, status, started_at, finished_at, exit_code, kind, step_id, action, outcome, conclusion, outputs, attempt)
                 SELECT ?1, idx, name, status, started_at, finished_at, exit_code, kind, step_id, action, outcome, conclusion, outputs, attempt FROM steps WHERE job_id = ?2",
                params![id, source],
            )?;
            // Its artifacts are this run's too, the same blobs under the same names.
            tx.execute(
                "INSERT OR IGNORE INTO ci_artifacts (run_id, leg_id, name, object, size, digest, mime_type, created_at, expires_at, finalized)
                 SELECT ?1, ?2, name, object, size, digest, mime_type, created_at, expires_at, finalized FROM ci_artifacts WHERE leg_id = ?3",
                params![run, id, source],
            )?;
            tx.commit()?;
            Ok(())
        })
        .await
}

/// The input hash a leg was queued with, if it opted in.
pub async fn hash_of(hub: &Hub, leg: &str) -> anyhow::Result<Option<String>> {
    let leg = leg.to_owned();
    hub.store.call(move |c| Ok(c.query_row("SELECT input_hash FROM jobs WHERE id = ?1", [&leg], |r| r.get::<_, Option<String>>(0)).optional()?.flatten())).await
}

/// Marks the leg with its input hash.
pub async fn set_hash(hub: &Hub, leg: &str, hash: &str) -> anyhow::Result<()> {
    let (leg, hash) = (leg.to_owned(), hash.to_owned());
    hub.store.call(move |c| Ok(c.execute("UPDATE jobs SET input_hash = ?2 WHERE id = ?1", params![leg, hash]).map(|_| ())?)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w08_r10_paths() {
        let listing = "100644 blob a1\tlib/a.txt\n100644 blob b1\tlib/x/b.rs\n100644 blob c1\tREADME.md\n100644 blob d1\tlibrary/z\n";
        assert_eq!(matching(listing, "lib/**"), "100644 blob a1\tlib/a.txt\n100644 blob b1\tlib/x/b.rs");
        assert_eq!(matching(listing, "lib"), "100644 blob a1\tlib/a.txt\n100644 blob b1\tlib/x/b.rs", "a directory, not every name it starts");
        assert_eq!(matching(listing, "README.md"), "100644 blob c1\tREADME.md");
        assert_eq!(matching(listing, "**/*.rs"), "100644 blob b1\tlib/x/b.rs");
        assert_eq!(matching(listing, "nothing"), "");
    }
}
