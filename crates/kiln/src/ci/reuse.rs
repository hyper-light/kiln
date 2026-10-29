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
    // Where its job is defined (W06-R03's `job.workflow_*`) names the run's own commit for the
    // run's own workflow, as `github.sha` does: in the hash, every push's inputs were new and
    // nothing was reused. The job's definition itself is in it, below.
    let mut planned = json(serde_json::to_value(leg))?;
    if let Some(o) = planned.as_object_mut() {
        o.remove("defined_in");
    }
    let definition = json!({
        "leg": planned,
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
/// The trust domain a leg's results are kept in (W24-R21): untrusted ones never meet trusted ones.
pub fn domain(trusted: bool) -> &'static str {
    if trusted { "trusted" } else { "untrusted" }
}

/// Whether a run's work is trusted (W24-R20, R21): a trusted run's, except a change's own as it's
/// reviewed (a diff, a pull request), which writes untrusted work and reads trusted. The land queue,
/// trusted, never takes a diff's own results: its candidate runs what the diffs touched itself.
pub fn work_trusted(trusted: bool, event: &str) -> bool {
    trusted && !matches!(event, "diff" | "pull_request" | "pull_request_target")
}

/// A leg's work unit (docs/ci.md W24-R06): a leg whose definition is its input hash, which covers
/// its definition, pins, images, platform, listed variables and listed paths' trees. The work
/// ledger keys it, on the one path its records and lookups share (W24-R30).
pub fn work_unit(hash: &str, trusted: bool) -> kiln_ci::work::key::Unit {
    use kiln_ci::work::key::{Kind, Platform, Unit};
    Unit {
        kind: Kind::Leg,
        definition: format!("w08-r10 {hash}"),
        secrets: Default::default(),
        env: Default::default(),
        platform: Platform::default(),
        inputs: Default::default(),
        upstream: Default::default(),
        trust: domain(trusted).to_owned(),
    }
}

/// Whether a succeeded leg can still be taken over: it's there, succeeded, and its artifacts all are.
fn stands(c: &rusqlite::Connection, leg: &str, now: i64) -> rusqlite::Result<bool> {
    c.query_row(
        "SELECT EXISTS(SELECT 1 FROM jobs j WHERE j.id = ?1 AND j.status = 'succeeded'
           AND NOT EXISTS (SELECT 1 FROM ci_artifacts a WHERE a.leg_id = j.id AND (a.finalized = 0 OR (a.expires_at IS NOT NULL AND a.expires_at <= ?2))))",
        params![leg, now],
        |r| r.get(0),
    )
}

/// The newest succeeded leg for `hash` within `max_age` whose artifacts are all still there, with
/// its report: from the work ledger alone, on its one path (W24-R06, R30). The newest run that
/// reproduced the entry is the one taken, so a rerun past `max_age` makes it fresh again.
pub async fn lookup(hub: &Hub, repo: &str, trusted: bool, hash: &str, max_age: u64) -> anyhow::Result<Option<(String, String, Option<String>)>> {
    let (hash, since) = (hash.to_owned(), crate::util::now_unix().saturating_sub(i64::try_from(max_age).unwrap_or(i64::MAX)));
    let (scope, unit) = (crate::ci::work::scope(repo, domain(trusted)), work_unit(&hash, trusted));
    let found = hub
        .store
        .call(move |c| {
            let now = crate::util::now_unix();
            let same = |p: &kiln_ci::work::key::PathSet| p.clone();
            if let Some(hit) = crate::ci::work::lookup(c, &scope, &unit, &same)?
                && hit.recorded.provenance.finished_at >= since
                && stands(c, &hit.recorded.provenance.leg, now)?
            {
                if let Some(by) = &hit.class {
                    crate::metrics::global().work_classes.inc(&["leg", "hit"]);
                    tracing::info!(leg = %hit.recorded.provenance.leg, class = ?by.class, why = ?by.why, "a leg's result is reused by its class (W24-R30)");
                }
                // Its report is among its notes; an entry kept before them has it among its outputs.
                let report = hit.recorded.notes.get("report").or_else(|| hit.recorded.outputs.get("report")).cloned().unwrap_or_default();
                return Ok(Some((hit.recorded.provenance.leg.clone(), report, hit.class.as_ref().map(|by| by.class.rep()))));
            }
            Ok(None)
        })
        .await;
    // Every lookup counted, by layer and outcome (W24-R28).
    let outcome = match &found {
        Ok(Some(_)) => "hit",
        Ok(None) => "miss",
        Err(_) => "error",
    };
    crate::metrics::global().work_lookups.inc(&["leg", outcome]);
    found
}

/// A leg that succeeded under `hash`: kept for those that come after, in the work ledger as its
/// work unit's (W24-R06). Its report is a note, not an output: every succeeded run of the unit
/// reproduces the entry, and the newest is the one a lookup takes (W24-R30).
pub async fn remember(hub: &Hub, repo: &str, trusted: bool, leg: &str, run: &str, hash: &str, report: &str) -> anyhow::Result<()> {
    let (repo, leg, run, hash, report) = (repo.to_owned(), leg.to_owned(), run.to_owned(), hash.to_owned(), report.to_owned());
    hub.store
        .call(move |c| {
            let now = crate::util::now_unix();
            let result = crate::ci::work::Recorded {
                outcome: "success".into(),
                // What it produced, as its class knows it: its inputs' hash, so no two jobs' legs
                // share a class (W24-R30), and every rerun of one reproduces it.
                outputs: BTreeMap::from([("inputs".to_owned(), hash.clone())]),
                notes: BTreeMap::from([("report".to_owned(), report)]),
                provenance: crate::ci::work::Provenance { repo: repo.clone(), run, leg, finished_at: now, ..Default::default() },
                domain: domain(trusted).to_owned(),
                ..Default::default()
            };
            crate::ci::work::record(c, &crate::ci::work::scope(&repo, domain(trusted)), &work_unit(&hash, trusted), &kiln_ci::work::key::PathSet::new(), &result)?;
            Ok(())
        })
        .await
}

/// A leg of `run` that takes `source`'s result over: a copy of it, its steps and its artifacts,
/// `reused_from` naming it, `input_hash` its hash.
pub async fn take_over(hub: &Hub, run: &str, source: &str, id: &str, hash: &str, class: Option<String>) -> anyhow::Result<()> {
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
            // Found through its class, it says which (W24-R30, R28).
            if let Some(c) = &class {
                tx.execute("UPDATE jobs SET reused_class = ?2 WHERE id = ?1", params![id, c])?;
            }
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
            // The time it saved: what its source ran (W24-R28).
            let ran: Option<i64> = tx.query_row("SELECT finished_at - started_at FROM jobs WHERE id = ?1", [&id], |r| r.get(0)).optional()?.flatten();
            tx.commit()?;
            if let Some(s) = ran.and_then(|s| u64::try_from(s).ok()) {
                crate::metrics::global().work_saved_seconds.add(s);
            }
            Ok(())
        })
        .await
}

/// An adopter of `source`'s execution takes its result over in place (W28-R44): its row's runner,
/// times, exit code and outputs, its steps and its artifacts, `reused_from` naming `source`.
/// Returns the report `source` gave its plan, when it logged one.
pub async fn adopt_into(hub: &Hub, leg: &str, source: &str) -> anyhow::Result<Option<String>> {
    let (leg, source) = (leg.to_owned(), source.to_owned());
    hub.store
        .call(move |c| {
            let tx = crate::store::tx(c)?;
            tx.execute(
                "UPDATE jobs SET (agent, executor, started_at, finished_at, exit_code, error, outputs, reused_from) =
                   (SELECT agent, executor, started_at, finished_at, exit_code, error, outputs, COALESCE(reused_from, id) FROM jobs WHERE id = ?2)
                 WHERE id = ?1",
                params![leg, source],
            )?;
            tx.execute("DELETE FROM steps WHERE job_id = ?1", [&leg])?;
            tx.execute(
                "INSERT INTO steps (job_id, idx, name, status, started_at, finished_at, exit_code, kind, step_id, action, outcome, conclusion, outputs, attempt)
                 SELECT ?1, idx, name, status, started_at, finished_at, exit_code, kind, step_id, action, outcome, conclusion, outputs, attempt FROM steps WHERE job_id = ?2",
                params![leg, source],
            )?;
            tx.execute(
                "INSERT OR IGNORE INTO ci_artifacts (run_id, leg_id, name, object, size, digest, mime_type, created_at, expires_at, finalized)
                 SELECT (SELECT run_id FROM jobs WHERE id = ?1), ?1, name, object, size, digest, mime_type, created_at, expires_at, finalized FROM ci_artifacts WHERE leg_id = ?2",
                params![leg, source],
            )?;
            let report: Option<String> = tx
                .query_row(
                    "SELECT l.report FROM ci_plan_log l JOIN jobs j ON j.id = ?1 WHERE l.run_id = j.run_id AND l.leg = j.key AND l.kind = 'leg' ORDER BY l.seq DESC LIMIT 1",
                    [&source],
                    |r| r.get::<_, Option<String>>(0),
                )
                .optional()?
                .flatten();
            tx.commit()?;
            Ok(report)
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

    /// W24-R06: a leg that took an earlier result over, as one the change can't affect does, is a
    /// passing required check: the land policy sees it green, and its row names its source.
    #[test]
    fn w24_r06_a_reused_leg_is_a_passing_required_check() -> anyhow::Result<()> {
        use crate::ci::runs;
        use crate::store::Status;
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let repo = Repo { id: "r1".into(), name: "acme/app".into(), clone_url: String::new(), branches: Vec::new(), poll_seconds: 0, enabled: true, created_at: 0, last_polled_at: None, last_error: None, mode: "native".into(), default_branch: None, mirror: None };
            let repo = hub.store.upsert_repo(repo).await?;
            let (wf, _) = kiln_ci::wf::parse_workflow(".kiln/ci.yaml", "on: push\njobs:\n  a:\n    work: { memo: true }\n    steps: [{run: t}]\n", kiln_ci::expr::Dialect::Kiln);
            let wf = std::sync::Arc::new(wf.ok_or_else(|| anyhow::anyhow!("no workflow"))?);
            let input = kiln_ci::plan::RunInput { trusted: true, ..Default::default() };
            let (_, acts) = kiln_ci::plan::RunPlan::start(wf, input, kiln_ci::plan::Settings::default(), &kiln_ci::plan::NoSource).map_err(|e| anyhow::anyhow!(e))?;
            let spec = acts.into_iter().find_map(|a| if let kiln_ci::plan::PlanAction::Queue(s) = a { Some(*s) } else { None }).ok_or_else(|| anyhow::anyhow!("no leg"))?;
            let new = |id: &str, sha: &str| runs::NewRun {
                id: id.into(),
                workflow: ".kiln/ci.yaml".into(),
                name: "ci".into(),
                event: "push".into(),
                branch: "main".into(),
                sha: sha.into(),
                message: String::new(),
                author: String::new(),
                actor: "ada".into(),
                trigger: Value::Null,
                trusted: true,
                priority: 60,
                payload: Value::Null,
                model: String::new(),
                input: String::new(),
            };
            runs::create(&hub, &repo, new("run1", "s1")).await?;
            runs::create(&hub, &repo, new("run2", "s2")).await?;
            runs::insert_leg(&hub, "run1", "leg1", &spec, Status::Queued, None).await?;
            runs::finish_leg(&hub, "leg1", Status::Succeeded, "success", None, None).await?;
            take_over(&hub, "run2", "leg1", "leg2", "h1", None).await?;
            let checks = crate::ci::review::checks_on(&hub, "acme/app", "s2").await?;
            anyhow::ensure!(checks.get("ci/a") == Some(&Some("success".to_owned())), "{checks:?}");
            let land = kiln_ci::land::Land { checks: vec!["ci/a".into()], approvals: 0, owners: false, resolve_threads: false, ..Default::default() };
            let blockers = kiln_ci::land::blockers(&land, &[], &kiln_ci::land::Facts { checks, ..Default::default() });
            anyhow::ensure!(blockers.is_empty(), "{blockers:?}");
            let from: Option<String> = hub.store.call(|c| Ok(c.query_row("SELECT reused_from FROM jobs WHERE id = 'leg2'", [], |r| r.get(0))?)).await?;
            anyhow::ensure!(from.as_deref() == Some("leg1"), "{from:?}");
            Ok(())
        })
    }
}
