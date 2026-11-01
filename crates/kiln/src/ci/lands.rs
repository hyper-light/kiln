//! Each land, signed as the queue that tested it (docs/ci.md W27-R07). Once a land queue moves a
//! trunk (its compare-and-swap), kiln signs the land as its server, whose queue it is ([`sign`]).
//! The statement (`kiln_ci::identity::landing`) is about the trunk at the commit it moved to, from
//! the tip it was applied onto, and names the candidate the queue tested (that commit, unless the
//! batch was applied again onto a tip that moved meanwhile) and its base. It names the batch's
//! diffs, each at the version that landed with the commit that carries it and the signed accepts
//! that hold on it, and the checks the queue ran on the candidate, each with its conclusion and its
//! legs' signed results. It's kept in `land_signatures`, and `GET /api/repos/:repo/lands/:commit`
//! reads it back ([`get`]).

use std::time::Duration;

use http::StatusCode;
use kiln_ci::identity::landing::{self, Land, Landed, Ran};
use kiln_ci::identity::statement;
use rusqlite::{OptionalExtension as _, params};
use serde_json::{Value, json};

use super::Fail;
use crate::hub::Hub;
use crate::store::Repo;

/// How long a land's signing waits for the signed results of the legs that sign. A leg sends them
/// at its end, after the events its conclusion comes from, so the queue can see a check pass first:
/// a moment first, or longer on a loaded machine (10 seconds wasn't enough under a gate's load,
/// 10-31). Bounded, for a leg whose records never come (its runner gone).
const RESULTS_WAIT: Duration = Duration::from_secs(60);

fn fail(e: anyhow::Error) -> Fail {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}

/// The checks run on `candidate` in repository `repo_id`: each run's name and conclusion, and its
/// jobs' latest attempts' signed results (their statements' digests).
async fn checks_on(hub: &Hub, repo_id: &str, candidate: &str) -> anyhow::Result<Vec<Ran>> {
    let (repo, sha) = (repo_id.to_owned(), candidate.to_owned());
    hub.store
        .call(move |c| {
            let mut runs = c.prepare("SELECT id, COALESCE(NULLIF(run_name, ''), pipeline), COALESCE(conclusion, status) FROM runs WHERE repo_id = ?1 AND sha = ?2 ORDER BY created_at, number")?;
            let rows: Vec<(String, String, String)> = runs.query_map(params![repo, sha], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<_, _>>()?;
            let mut results = c.prepare(
                "SELECT l.digest FROM jobs j JOIN ci_leg_records l ON l.job_id = j.id AND l.attempt = j.run_attempt
                 WHERE j.run_id = ?1 AND l.payload_type = ?2 AND l.refused IS NULL
                   AND j.run_attempt = (SELECT MAX(k.run_attempt) FROM jobs k WHERE k.run_id = j.run_id AND k.key = j.key)
                 ORDER BY j.key, l.digest",
            )?;
            let mut out = Vec::new();
            for (run, name, conclusion) in rows {
                let digests = results.query_map(params![run, kiln_ci::attest::leg::PAYLOAD_RESULTS], |r| r.get(0))?.collect::<Result<Vec<String>, _>>()?;
                out.push(Ran { name, conclusion, results: digests });
            }
            Ok(out)
        })
        .await
}

/// Whether a leg of a run on `candidate` that signs (its delegation says so) hasn't had its signed
/// results taken yet: its job's latest attempt.
async fn awaits_results(hub: &Hub, repo_id: &str, candidate: &str) -> anyhow::Result<bool> {
    let (repo, sha) = (repo_id.to_owned(), candidate.to_owned());
    hub.store
        .call(move |c| {
            let sql = "SELECT EXISTS (SELECT 1 FROM runs r JOIN jobs j ON j.run_id = r.id JOIN ci_leg_delegations d ON d.job_id = j.id
                        WHERE r.repo_id = ?1 AND r.sha = ?2 AND d.signs = 1
                          AND j.run_attempt = (SELECT MAX(k.run_attempt) FROM jobs k WHERE k.run_id = j.run_id AND k.key = j.key)
                          AND NOT EXISTS (SELECT 1 FROM ci_leg_records l WHERE l.job_id = j.id AND l.payload_type = ?3 AND l.refused IS NULL))";
            Ok(c.query_row(sql, params![repo, sha, kiln_ci::attest::leg::PAYLOAD_RESULTS], |r| r.get(0))?)
        })
        .await
}

/// How a land moved its trunk: from the tip it was applied onto to the commit it moved it to; the
/// candidate the queue tested and its base; and whether the batch was applied again onto `from`
/// (`to` is then a commit of its own, `candidate` where the checks ran).
pub struct Moved<'a> {
    pub from: &'a str,
    pub to: &'a str,
    pub candidate: &'a str,
    pub base: &'a str,
    pub rebased: bool,
}

/// The land of `carried` (each diff's id and the commit that carries it on `moved.to`) on `trunk`
/// of `repo`, as it `moved` it, signed as kiln's server and kept. Its digest.
pub async fn sign(hub: &Hub, repo: &Repo, trunk: &str, moved: &Moved<'_>, carried: &[(String, String)]) -> anyhow::Result<String> {
    let candidate = moved.candidate;
    let mut batch = Vec::new();
    for (id, commit) in carried {
        let Some(d) = super::diffs::find(hub, id).await? else { continue };
        let versions = super::diffs::versions(hub, &d).await?;
        let version = versions.last().map_or(0, |v| v.n);
        let verdicts = super::verdicts::holding_accepts(hub, &d, &versions).await.map_err(|(_, e)| anyhow::anyhow!(e))?;
        batch.push(Landed { diff: d.name.clone(), version, commit: commit.clone(), verdicts });
    }
    // Its checks' signed results, waited for a little where a leg that signs hasn't sent them yet.
    let waited = std::time::Instant::now();
    while waited.elapsed() < RESULTS_WAIT && awaits_results(hub, &repo.id, candidate).await? {
        crate::rt::sleep(Duration::from_millis(250)).await;
    }
    let land = Land {
        repo: repo.name.clone(),
        trunk: trunk.to_owned(),
        from: moved.from.to_owned(),
        to: moved.to.to_owned(),
        candidate: candidate.to_owned(),
        base: moved.base.to_owned(),
        rebased: moved.rebased,
        batch,
        checks: checks_on(hub, &repo.id, candidate).await?,
        via: String::new(),
    };
    keep(hub, &repo.id, &land).await
}

/// A land through a host's own queue (docs/ci.md W26-R19, W27-R07), signed as kiln's server for
/// what it saw: `via`, the host's queue, tested and merged `carried` (each change as `#n`, the
/// version of it Review cleared, and the commit it merged as) onto `trunk`, the last its head now.
/// The tip it went onto is named where kiln's mirror has the commit; its checks are the runs kiln
/// knows of on it. Where kiln's queue tested it and the host only merged it, `tested` is the
/// candidate and its base, onto which the host merged it: its checks are the candidate's. Kept and
/// taken into the trail as [`sign`]'s are. Its digest.
pub async fn sign_host(hub: &Hub, repo: &Repo, trunk: &str, via: &str, tested: Option<(&str, &str)>, carried: &[(String, u32, String)]) -> anyhow::Result<String> {
    let to = carried.last().map(|(_, _, c)| c.clone()).ok_or_else(|| anyhow::anyhow!("no change landed"))?;
    if let Some((candidate, base)) = tested {
        let batch = carried.iter().map(|(diff, version, commit)| Landed { diff: diff.clone(), version: *version, commit: commit.clone(), verdicts: Vec::new() }).collect();
        let checks = checks_on(hub, &repo.id, candidate).await?;
        let land = Land { repo: repo.name.clone(), trunk: trunk.to_owned(), from: base.to_owned(), to, candidate: candidate.to_owned(), base: base.to_owned(), rebased: false, batch, checks, via: via.to_owned() };
        return keep(hub, &repo.id, &land).await;
    }
    let from = match crate::gitops::ensure_commit(hub, repo, &to).await {
        Ok(mirror) => {
            let (dir, parent) = (mirror.to_string_lossy().into_owned(), format!("{to}^1"));
            crate::gitops::run_git(hub, &["-C", &dir, "rev-parse", "--verify", &parent], crate::gitops::LS_REMOTE_TIMEOUT).await.map(|o| String::from_utf8_lossy(&o).trim().to_owned()).unwrap_or_default()
        }
        Err(e) => {
            tracing::info!(repo = %repo.name, commit = %to, error = %format!("{e:#}"), "a host's land is signed without the tip it went onto: kiln's mirror hasn't its commit");
            String::new()
        }
    };
    let batch = carried.iter().map(|(diff, version, commit)| Landed { diff: diff.clone(), version: *version, commit: commit.clone(), verdicts: Vec::new() }).collect();
    let land = Land { repo: repo.name.clone(), trunk: trunk.to_owned(), from: from.clone(), to: to.clone(), candidate: to.clone(), base: from, rebased: false, batch, checks: checks_on(hub, &repo.id, &to).await?, via: via.to_owned() };
    keep(hub, &repo.id, &land).await
}

/// `land` signed as kiln's server, kept in `land_signatures` by the commit its trunk moved to and
/// taken into the trail (W27-R09). Its digest.
async fn keep(hub: &Hub, repo_id: &str, land: &Land) -> anyhow::Result<String> {
    let payload = statement::payload(&landing::statement(land)).map_err(|e| anyhow::anyhow!("{e}"))?;
    let bundle = crate::identities::sign_as(&hub.pki, "server", landing::PAYLOAD_LAND, &payload)?;
    let digest = statement::digest(landing::PAYLOAD_LAND, &payload);
    let signer = kiln_ci::identity::chain::Identity::new(hub.pki.organization(), kiln_ci::identity::chain::Actor::Server).uri().map_err(|e| anyhow::anyhow!("{e}"))?;
    let (repo_id, trunk, head, candidate, base, d, at) = (repo_id.to_owned(), land.trunk.clone(), land.to.clone(), land.candidate.clone(), land.base.clone(), digest.clone(), crate::util::now_unix());
    hub.store
        .call(move |c| {
            let (tx, text) = (crate::store::tx(c)?, bundle.to_string());
            tx.execute(
                "INSERT INTO land_signatures (repo_id, trunk, head, candidate, base, digest, signer, signed_at, bundle) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT (repo_id, trunk, head) DO UPDATE SET candidate = excluded.candidate, base = excluded.base, digest = excluded.digest, signer = excluded.signer, signed_at = excluded.signed_at, bundle = excluded.bundle",
                params![repo_id, trunk, head, candidate, base, d, signer, at, text],
            )?;
            // In the trail as taken (W27-R09).
            crate::trail::take(&tx, &text, at)?;
            tx.commit()?;
            Ok(())
        })
        .await?;
    Ok(digest)
}

/// `GET /api/repos/:repo/lands/:commit`: the land that put `commit` at the head of a trunk of
/// `repo`, as its queue signed it: its trunk, the candidate it tested and that candidate's base,
/// its signer, digest, when, what it says, and its envelope.
pub async fn get(hub: &Hub, repo: &str, commit: &str) -> Result<Value, Fail> {
    let repos = hub.store.repos().await.map_err(fail)?;
    let r = repos.into_iter().find(|x| x.name == repo || x.id == repo).ok_or((StatusCode::NOT_FOUND, format!("no repository {repo}")))?;
    let (id, sha) = (r.id.clone(), commit.to_owned());
    type Row = (String, String, String, String, String, i64, String);
    let row: Option<Row> = hub
        .store
        .call(move |c| {
            let sql = "SELECT trunk, candidate, base, digest, signer, signed_at, bundle FROM land_signatures WHERE repo_id = ?1 AND head = ?2 ORDER BY signed_at DESC LIMIT 1";
            Ok(c.query_row(sql, params![id, sha], |x| Ok((x.get(0)?, x.get(1)?, x.get(2)?, x.get(3)?, x.get(4)?, x.get(5)?, x.get(6)?))).optional()?)
        })
        .await
        .map_err(fail)?;
    let (trunk, candidate, base, digest, signer, signed_at, bundle) = row.ok_or((StatusCode::NOT_FOUND, format!("no land of {repo} put {commit} at a trunk's head")))?;
    let bundle: Value = serde_json::from_str(&bundle).unwrap_or(Value::Null);
    let said = bundle
        .pointer("/dsseEnvelope/payload")
        .and_then(Value::as_str)
        .and_then(|p| base64::Engine::decode(&base64::engine::general_purpose::STANDARD, p).ok())
        .and_then(|p| serde_json::from_slice::<statement::Statement>(&p).ok())
        .map(|s| s.body)
        .unwrap_or(Value::Null);
    Ok(json!({ "repo": r.name, "trunk": trunk, "head": commit, "candidate": candidate, "base": base, "signer": signer, "digest": digest, "signed_at": signed_at, "land": said, "bundle": bundle }))
}
