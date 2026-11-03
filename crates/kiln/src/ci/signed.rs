//! A leg's signed records, as kiln takes and judges them (docs/ci.md W27-R05). The runner sends
//! them at the leg's end ([`crate::proto::AgentMsg::LegResults`]): its results, and its custody
//! records, a receipt of each thing it received and its hand-on of what it made.
//!
//! - **Handed** ([`handed`]): as kiln sends a leg its assignment it keeps the digest the leg's
//!   receipt of it will name, and the server's delegation of the leg names it
//!   (`crate::identities`), with the commit.
//! - **Taken** ([`taken`]): each record checked as kiln takes it, at that time, against kiln's root
//!   and the revocations, since the leg's certificate is short-lived. It must be the leg's (its
//!   results or its custody), its signer the leg kiln gave that job (its run, attempt and key, on
//!   the runner that sent it), acting under the delegation kiln's server gave it as it certified
//!   it, which is checked through kiln's records ([`statement::verify_under`]). Each is kept with
//!   when kiln took it, or why it was refused. kiln signs its own receipt of the leg's hand-on as
//!   the server, following it: it took custody of those things, by those digests.
//! - **Judged** ([`judged`]) whenever they're read, against what kiln keeps of the leg: each step's
//!   end, each log by its chain ([`crate::logs::Logs::check`]), each report by its digest, and the
//!   leg's end, so a log's chunks still on their way when the leg ended count once they're in;
//!   and its custody ([`leg::judge_custody`]): its stream whole, its hand-on and kiln's receipt of
//!   it, and its receipts of what kiln handed it (its assignment, its commit).
//! - `GET /api/ci/legs/:id/signed` says who signed and when kiln took it, the delegation they acted
//!   under, each record (its bundle, to check apart from kiln) and each break (none: they hold). A
//!   leg that sent none is `unsigned`, saying why: its runner signs no legs' records (a kiln from
//!   before them), kiln gave it no identity (a v1 job's) or gave it only after the leg had ended,
//!   or what it sent hasn't come.

use http::StatusCode;
use kiln_ci::attest::leg;
use kiln_ci::identity::chain::{Actor, Identity, Verified};
use kiln_ci::identity::statement;
use rusqlite::{OptionalExtension as _, params};
use serde_json::{Value, json};

use super::Fail;
use crate::http::server::{self as srv, Response};
use crate::hub::Hub;

/// What kiln hands attempt `attempt` of leg `job` as its assignment (`AssignmentV2`'s JSON, as
/// sent), kept by the digest the leg's receipt of it names ([`leg::assignment`]).
pub async fn handed(hub: &Hub, job: &str, attempt: u32, json: &str) -> anyhow::Result<()> {
    let (digest, _) = leg::assignment(&serde_json::from_str(json)?);
    let j = job.to_owned();
    hub.store
        .call(move |c| {
            c.execute("INSERT OR REPLACE INTO ci_leg_assignments (job_id, attempt, digest) VALUES (?1, ?2, ?3)", params![j, attempt, digest])?;
            Ok(())
        })
        .await
}

/// What kiln handed attempt `attempt` of leg `job` as its assignment, by its receipt's digest.
pub async fn handed_digest(store: &crate::store::Store, job: &str, attempt: u32) -> anyhow::Result<Option<String>> {
    let j = job.to_owned();
    store.call(move |c| Ok(c.query_row("SELECT digest FROM ci_leg_assignments WHERE job_id = ?1 AND attempt = ?2", params![j, attempt], |r| r.get(0)).optional()?)).await
}

/// Whether leg `job`'s runner said, as it asked for the leg's identity, that the leg signs (its
/// records, and the provenance kiln writes when asked): of its latest delegation.
pub async fn leg_signs(store: &crate::store::Store, job: &str) -> anyhow::Result<bool> {
    let j = job.to_owned();
    store.call(move |c| Ok(c.query_row("SELECT signs FROM ci_leg_delegations WHERE job_id = ?1 ORDER BY taken_at DESC, rowid DESC LIMIT 1", [&j], |r| r.get::<_, bool>(0)).optional()?.unwrap_or(false))).await
}

/// The commit's tarball kiln served leg `job` (the facade's `GET …/tarball/{ref}`, under the
/// leg's token), by the SHA-256 of the bytes it sent, for its attempt now: what the leg's receipt
/// of its checkout is held to.
pub async fn served(hub: &Hub, job: &str, commit: &str, sha256: &str) -> anyhow::Result<()> {
    let (j, c, d, at) = (job.to_owned(), commit.to_owned(), sha256.to_owned(), crate::util::now_unix());
    hub.store
        .call(move |db| {
            db.execute(
                "INSERT OR IGNORE INTO ci_leg_served (job_id, attempt, commit_sha, sha256, served_at) SELECT ?1, attempt, ?2, ?3, ?4 FROM jobs WHERE id = ?1",
                params![j, c, d, at],
            )?;
            Ok(())
        })
        .await
}

/// The tarballs kiln served attempt `attempt` of leg `job`, each a `checkout` by the SHA-256 of
/// what it sent and the commit it was of.
async fn served_of(hub: &Hub, job: &str, attempt: i64) -> anyhow::Result<Vec<statement::Subject>> {
    let j = job.to_owned();
    let rows: Vec<(String, String)> = hub
        .store
        .call(move |c| {
            let mut st = c.prepare("SELECT commit_sha, sha256 FROM ci_leg_served WHERE job_id = ?1 AND attempt = ?2")?;
            let rows = st.query_map(params![j, attempt], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await?;
    Ok(rows
        .into_iter()
        .map(|(commit, sha256)| statement::Subject { name: "checkout".into(), digest: [("sha256".to_owned(), sha256), ("gitCommit".to_owned(), commit)].into_iter().collect(), ..statement::Subject::default() })
        .collect())
}

/// The header kiln's hand-on of what it serves a leg comes in (a tarball's), its bundle in base64.
pub const HAND_ON_HEADER: &str = "kiln-custody-hand-on";

/// A thing named `name` by one digest (`algorithm`, its hex).
fn named(name: &str, algorithm: &str, hex: &str) -> statement::Subject {
    statement::Subject { name: name.to_owned(), digest: [(algorithm.to_owned(), hex.to_ascii_lowercase())].into_iter().collect(), ..statement::Subject::default() }
}

/// The leg kiln's server last delegated `job` to: its identity's URI, the delegation's delegate.
async fn leg_of(store: &crate::store::Store, job: &str) -> anyhow::Result<Option<String>> {
    let j = job.to_owned();
    let bundle: Option<String> = store.call(move |c| Ok(c.query_row("SELECT bundle FROM ci_leg_delegations WHERE job_id = ?1 ORDER BY taken_at DESC, rowid DESC LIMIT 1", [&j], |r| r.get(0)).optional()?)).await?;
    Ok(bundle.and_then(|b| crate::custody::statement_in(&b)).and_then(|s| s.body.get("delegate").and_then(Value::as_str).map(str::to_owned)))
}

/// The land that put `commit` at the head of the trunk of leg `job`'s repository, by its digest,
/// when kiln's queue landed it (`land_signatures`).
async fn land_of(hub: &Hub, job: &str, commit: &str) -> anyhow::Result<Option<String>> {
    let (j, c) = (job.to_owned(), commit.to_owned());
    hub.store
        .call(move |db| {
            Ok(db
                .query_row(
                    "SELECT l.digest FROM land_signatures l JOIN runs r ON r.repo_id = l.repo_id JOIN jobs j ON j.run_id = r.id WHERE j.id = ?1 AND l.head = ?2 ORDER BY l.signed_at DESC LIMIT 1",
                    params![j, c],
                    |r| r.get(0),
                )
                .optional()?)
        })
        .await
}

/// kiln's server hands `things` of leg `job` to the runner process running it, its leg identity's
/// (`leg`) machine and instance (docs/ci.md W27-R08; the Transfers plan's step 11): a hand-on of
/// each, signed as the server and kept, which that process's receipt follows. The bundles.
async fn hand_to_runner(hub: &Hub, job: &str, leg: &str, things: Vec<statement::Subject>, why: &str) -> anyhow::Result<Vec<Value>> {
    use kiln_ci::identity::custody::{Body, Kind, to_value};
    let runner = leg::runner_of(leg).map_err(|e| anyhow::anyhow!("{e}"))?;
    let (server, org, at) = (server_uri(hub)?, hub.pki.organization(), crate::util::now_unix());
    let mut out = Vec::new();
    for t in things {
        let body = Body { to: Some(runner.clone()), why: format!("leg {job}'s {}, {why}", t.name), ..Body::new(Kind::HandedOn, &server) };
        let s = statement::Statement { subject: vec![t], ..statement::Statement::new(to_value(&body)) };
        let bundle = crate::custody::server_signed(&hub.pki, &s)?;
        match crate::custody::take(&hub.store, &hub.pki.ca_der, &org, &bundle, "kiln's server", at).await? {
            Ok(_) => out.push(bundle),
            Err(why) => tracing::warn!(target: "kiln.custody", job, %why, "kiln refused its own hand-on of a leg's input"),
        }
    }
    Ok(out)
}

/// kiln's server hands attempt `attempt` of leg `job`, certified as `leg`, its inputs (docs/ci.md
/// W27-R08; the Transfers plan's step 11), to the runner process `peer` running it: its
/// assignment as kiln sent it, and its commit, naming the land that put it at its trunk's head
/// when kiln's queue landed it ("landed (kiln)"). Each hand-on kept, and sent to that process
/// (`ServerMsg::Custody`) when it signs custody, which signs its receipts of them at the leg's end.
pub async fn give_inputs(hub: &Hub, peer: crate::instances::PeerId, job: &str, attempt: u32, leg: &str, commit: Option<&str>) {
    let mut things = Vec::new();
    match handed_digest(&hub.store, job, attempt).await {
        Ok(Some(a)) => things.push(named("assignment", "sha256", &a)),
        Ok(None) => {}
        Err(e) => tracing::warn!(target: "kiln.custody", job, error = %format!("{e:#}"), "a leg's assignment wasn't found to hand on"),
    }
    if let Some(c) = commit {
        let mut t = named("commit", "gitCommit", c);
        match land_of(hub, job, c).await {
            Ok(Some(land)) => {
                t.annotations.insert("land".into(), land.into());
            }
            Ok(None) => {}
            Err(e) => tracing::warn!(target: "kiln.custody", job, error = %format!("{e:#}"), "a leg's commit's land wasn't found"),
        }
        things.push(t);
    }
    let bundles = match hand_to_runner(hub, job, leg, things, "as kiln gave it the leg").await {
        Ok(b) => b,
        Err(e) => return tracing::warn!(target: "kiln.custody", job, error = %format!("{e:#}"), "a leg's inputs weren't handed on signed"),
    };
    let signs = hub.instances.lock().get(peer).is_some_and(|i| (u64::from(i.caps) | i.wide) & crate::proto::CAP_CUSTODY != 0);
    if signs && !bundles.is_empty() {
        hub.send_peer(peer, &crate::proto::ServerMsg::Custody { json: json!({ "leg": job, "hand_on": bundles }).to_string() }).await;
    }
}

/// The tarball of commit `sha` kiln serves leg `job` (by its bytes' SHA-256), handed on to the
/// runner process running it (docs/ci.md W27-R08): kiln's hand-on, kept, to send with the bytes,
/// which that process checks them against before the leg runs on them. None for a leg kiln
/// delegated to no one (a v1 job's).
pub async fn served_hand_on(hub: &Hub, job: &str, sha: &str, sha256: &str) -> anyhow::Result<Option<Value>> {
    let mut t = named("checkout", "sha256", sha256);
    t.annotations.insert("commit".into(), sha.into());
    hand_leg(hub, job, t, "as kiln served it").await
}

/// kiln's server hands `thing`, which it serves leg `job` (a tarball, a cache, an artifact, an
/// action), to the runner process running it (docs/ci.md W27-R08): its hand-on, kept, to say with
/// what it serves. None for a leg kiln delegated to no one (a v1 job's).
async fn hand_leg(hub: &Hub, job: &str, thing: statement::Subject, why: &str) -> anyhow::Result<Option<Value>> {
    let Some(leg) = leg_of(&hub.store, job).await? else { return Ok(None) };
    Ok(hand_to_runner(hub, job, &leg, vec![thing], why).await?.into_iter().next())
}

/// Action `repo` (`owner/name`) at commit `sha`, its tarball's SHA-256 `sha256`, which kiln serves
/// leg `job`, handed on to the runner process running it ([`hand_leg`]).
pub async fn action_hand_on(hub: &Hub, job: &str, repo: &str, sha: &str, sha256: &str) -> anyhow::Result<Option<Value>> {
    let mut t = named(&format!("action/{repo}"), "gitCommit", sha);
    t.digest.insert("sha256".into(), sha256.to_ascii_lowercase());
    hand_leg(hub, job, t, "as kiln served it").await
}

/// Artifact `name` of leg `job`'s run (its zip's SHA-256), which kiln serves the leg, handed on to
/// the runner process running it ([`hand_leg`]).
pub async fn artifact_hand_on(hub: &Hub, job: &str, name: &str, sha256: &str) -> anyhow::Result<Option<Value>> {
    hand_leg(hub, job, named(&format!("artifact/{name}"), "sha256", sha256), "as kiln served it").await
}

/// The cache of `key` kiln found leg `job` (its content's SHA-256, hex), handed on to the runner
/// process running it (docs/ci.md W27-R08): kiln's hand-on, kept, to send with what it found,
/// which that process keeps once the cache has come whole by that digest. None for a leg kiln
/// delegated to no one.
pub async fn found_hand_on(hub: &Hub, job: &str, key: &str, sha256: &str) -> anyhow::Result<Option<Value>> {
    hand_leg(hub, job, named(&format!("cache/{key}"), "sha256", sha256), "as kiln found it for the leg").await
}

/// Leg `job`'s token pushed manifest `digest` to repository `repo` of kiln's registry (docs/ci.md
/// W27-R08; the Transfers plan's step 11): kept as its current attempt's, so the leg's hand-on of
/// what it made names the image, and kiln's receipt of that takes custody of it.
pub async fn pushed(hub: &Hub, job: &str, repo: &str, digest: &str) {
    let (j, r, d, at) = (job.to_owned(), repo.to_owned(), digest.to_owned(), crate::util::now_unix());
    let kept = hub
        .store
        .call(move |c| {
            c.execute("INSERT OR IGNORE INTO ci_leg_pushed (job_id, attempt, repo, digest, pushed_at) SELECT ?1, attempt, ?2, ?3, ?4 FROM jobs WHERE id = ?1", params![j, r, d, at])?;
            Ok(())
        })
        .await;
    if let Err(e) = kept {
        tracing::warn!(target: "kiln.custody", job, error = %format!("{e:#}"), "an image a leg pushed wasn't recorded");
    }
}

/// `GET /_kiln/pushed`, for a leg's runtime token: the images its token pushed to kiln's registry
/// this attempt ([`pushed`]), each by its repository and digest, which its runner names in the
/// leg's hand-on of what it made (docs/ci.md W27-R08).
pub async fn pushed_by_leg(hub: &Hub, req: &crate::http::server::Request) -> Result<Response, Fail> {
    let Some(claims) = super::token::of_request(hub, req.headers()) else { return Err((StatusCode::UNAUTHORIZED, "a leg's runtime token is needed".into())) };
    let j = claims.sub.clone();
    let rows: Vec<(String, String)> = hub
        .store
        .call(move |c| {
            let mut q = c.prepare("SELECT p.repo, p.digest FROM ci_leg_pushed p JOIN jobs j ON j.id = p.job_id AND j.attempt = p.attempt WHERE p.job_id = ?1 ORDER BY p.pushed_at, p.repo, p.digest")?;
            let rows = q.query_map([&j], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    Ok(srv::json(StatusCode::OK, &json!({ "pushed": rows.iter().map(|(repo, digest)| json!({ "repo": repo, "digest": digest })).collect::<Vec<_>>() })))
}

/// A leg's record as kiln keeps it.
struct Row {
    attempt: i64,
    digest: String,
    payload_type: String,
    bundle: String,
    signer: String,
    uri: String,
    taken_at: i64,
    refused: Option<String>,
}

/// Takes leg `job`'s signed records `bundles` (a JSON array of bundles), sent by the runner
/// `machine` that ran it, then signs kiln's receipt of its hand-on.
pub async fn taken(hub: &Hub, machine: &str, job: &str, bundles: Vec<Value>) {
    if let Err(e) = take(hub, machine, job, bundles).await {
        tracing::warn!(job, error = %format!("{e:#}"), "a leg's signed records couldn't be kept");
    }
}

async fn take(hub: &Hub, machine: &str, job: &str, all: Vec<Value>) -> anyhow::Result<()> {
    let at = crate::util::now_unix();
    let id = job.to_owned();
    let current: i64 = hub.store.call(move |c| Ok(c.query_row("SELECT attempt FROM jobs WHERE id = ?1", [&id], |r| r.get(0)).optional()?.unwrap_or(0))).await?;
    let server = server_uri(hub)?;
    let against = Against::of(hub, job).await?;
    let mut rows = Vec::new();
    let mut hand_on = None;
    // Its custody records, kept in kiln's custody too when its receipts follow its runner's
    // hand-ons (docs/ci.md W27-R08), so a walk from what it made goes back through it to what kiln
    // handed its runner; a runner from before them signs receipts following only the delegation.
    let (mut custody, mut follows_runner) = (Vec::new(), true);
    for b in all {
        let (row, checked) = match against.check(hub, &b, at) {
            Ok(u) => {
                let c = &u.statement;
                let why = match whose(hub, machine, job, c).await? {
                    Some(why) => Some(why),
                    None => leg::under_kilns_delegation(&u, &server).err(),
                };
                let attempt = match &c.signer.identity.actor {
                    Actor::Leg { attempt, .. } => i64::from(*attempt),
                    _ => current,
                };
                let row = Row { attempt, digest: c.digest.clone(), payload_type: c.payload_type.clone(), bundle: b.to_string(), signer: c.signer.to_string(), uri: c.custodian.clone(), taken_at: at, refused: why };
                (row, Some(u.statement))
            }
            Err(e) => {
                let text = b.to_string();
                let row = Row { attempt: current, digest: format!("bundle:{}", crate::util::sha256_hex(text.as_bytes())), payload_type: String::new(), bundle: text, signer: String::new(), uri: String::new(), taken_at: at, refused: Some(e.to_string()) };
                (row, None)
            }
        };
        if let Some(why) = &row.refused {
            tracing::warn!(job, machine, why = %why, "a leg's signed record was refused");
        } else if let Some(c) = checked.filter(|c| c.payload_type == statement::PAYLOAD_CUSTODY) {
            if leg::kind(&c) == "received" && c.statement.follows.iter().any(|f| against.delegations.contains_key(f)) {
                follows_runner = false;
            }
            custody.push(b.clone());
            if leg::kind(&c) == "handed-on" {
                hand_on = Some((row.attempt, c));
            }
        }
        rows.push(row);
    }
    // Taken once: sent again (a reconnect), a record keeps when kiln first took it.
    let j = job.to_owned();
    hub.store
        .call(move |c| {
            let tx = crate::store::tx(c)?;
            for r in rows {
                let n = tx.execute(
                    "INSERT OR IGNORE INTO ci_leg_records (job_id, attempt, digest, payload_type, bundle, signer, uri, taken_at, refused) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    params![j, r.attempt, r.digest, r.payload_type, r.bundle, r.signer, r.uri, r.taken_at, r.refused],
                )?;
                // In the trail as taken: what was refused wasn't.
                if n > 0 && r.refused.is_none() {
                    crate::trail::take(&tx, &r.bundle, r.taken_at)?;
                }
            }
            tx.commit()?;
            Ok(())
        })
        .await?;
    if let Some((attempt, h)) = hand_on
        && let Some(kilns) = receipt(hub, job, attempt, &h, &server).await?
    {
        custody.push(kilns);
    }
    if follows_runner {
        let (org, came_from) = (hub.pki.organization(), format!("runner {machine}"));
        for b in &custody {
            if let Err(e) = crate::custody::take(&hub.store, &hub.pki.ca_der, &org, b, &came_from, at).await {
                tracing::warn!(target: "kiln.custody", job, error = %format!("{e:#}"), "a leg's custody record wasn't kept in kiln's custody");
            }
        }
    }
    Ok(())
}

/// kiln's receipt of the leg's hand-on `h` of what it made ([`leg::receipt`]), signed as the server
/// and kept as taken now, once: its bundle, the first time.
async fn receipt(hub: &Hub, job: &str, attempt: i64, h: &statement::Checked, server: &str) -> anyhow::Result<Option<Value>> {
    let payload = statement::payload(&leg::receipt(server, h, &format!("leg/{job}/{attempt}")))?;
    let digest = statement::digest(statement::PAYLOAD_CUSTODY, &payload);
    let (j, d) = (job.to_owned(), digest.clone());
    let had: bool = hub.store.call(move |c| Ok(c.query_row("SELECT 1 FROM ci_leg_records WHERE job_id = ?1 AND attempt = ?2 AND digest = ?3", params![j, attempt, d], |_| Ok(())).optional()?.is_some())).await?;
    if had {
        return Ok(None);
    }
    let signed = crate::identities::sign_as(&hub.pki, "server", statement::PAYLOAD_CUSTODY, &payload)?;
    let bundle = signed.to_string();
    let words = Identity::parse(server).map(|i| i.to_string()).unwrap_or_else(|_| server.to_owned());
    let (j, s, at) = (job.to_owned(), server.to_owned(), crate::util::now_unix());
    hub.store
        .call(move |c| {
            let tx = crate::store::tx(c)?;
            let n = tx.execute(
                "INSERT OR IGNORE INTO ci_leg_records (job_id, attempt, digest, payload_type, bundle, signer, uri, taken_at, refused) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL)",
                params![j, attempt, digest, statement::PAYLOAD_CUSTODY, bundle, words, s, at],
            )?;
            if n > 0 {
                crate::trail::take(&tx, &bundle, at)?;
            }
            tx.commit()?;
            Ok(())
        })
        .await?;
    Ok(Some(signed))
}

/// What a leg's records are checked against: the revocations as they stand, and the delegations
/// kiln's server gave the leg, by digest.
struct Against {
    revocations: Vec<kiln_ci::identity::chain::Revocation>,
    delegations: std::collections::HashMap<String, (Value, i64)>,
}

impl Against {
    async fn of(hub: &Hub, job: &str) -> anyhow::Result<Self> {
        Ok(Self { revocations: crate::identities::revocations(&hub.store).await?, delegations: crate::identities::delegations_of(&hub.store, job).await? })
    }

    /// A leg's statement `bundle` checked as kiln took it at `at`, against kiln's root, through
    /// the leg's delegations: who signed it, under which, or why it isn't taken.
    fn check(&self, hub: &Hub, bundle: &Value, at: i64) -> Result<statement::Under, statement::Refusal> {
        statement::verify_under(bundle, &hub.pki.ca_der, &self.revocations, at, |d| self.delegations.get(d).cloned())
    }
}

/// kiln's server's identity URI (`kiln://<org>/server`): the one a leg's delegation is from.
fn server_uri(hub: &Hub) -> anyhow::Result<String> {
    Identity::new(hub.pki.organization(), Actor::Server).uri().map_err(|e| anyhow::anyhow!("{e}"))
}

/// Why a checked statement isn't one of leg `job`'s records from runner `machine`, if it isn't:
/// another payload type than its results' or custody's, or a signer other than that leg.
async fn whose(hub: &Hub, machine: &str, job: &str, c: &statement::Checked) -> anyhow::Result<Option<String>> {
    if c.payload_type != leg::PAYLOAD_RESULTS && c.payload_type != statement::PAYLOAD_CUSTODY {
        return Ok(Some(format!("it's a {}, not a leg's results or custody", c.payload_type)));
    }
    not_the_leg(hub, machine, job, &c.signer).await
}

/// Why `signer` isn't the leg kiln gave `job` (its run, attempt and key) on runner `machine`, if it
/// isn't.
pub(crate) async fn not_the_leg(hub: &Hub, machine: &str, job: &str, signer: &Verified) -> anyhow::Result<Option<String>> {
    let Actor::Leg { machine: on, run, attempt, leg: key, .. } = &signer.identity.actor else {
        return Ok(Some(format!("it's signed by {signer}, not by a leg")));
    };
    let id = job.to_owned();
    let row: Option<(i64, i64, String)> = hub
        .store
        .call(move |db| Ok(db.query_row("SELECT r.number, j.attempt, j.key FROM jobs j JOIN runs r ON r.id = j.run_id WHERE j.id = ?1", [&id], |x| Ok((x.get(0)?, x.get(1)?, x.get(2)?))).optional()?))
        .await?;
    let Some((number, tried, k)) = row else { return Ok(Some(format!("kiln knows no leg {job}"))) };
    let (want_run, want_attempt, want_leg) = (u64::try_from(number).unwrap_or_default(), u32::try_from(tried.max(1)).unwrap_or(1), crate::identities::leg_name(&k));
    if *run != want_run || *attempt != want_attempt || *key != want_leg || on != machine {
        return Ok(Some(format!("it's signed by {signer}, not by leg {want_leg} of run {want_run} (attempt {want_attempt}) on {machine}")));
    }
    Ok(None)
}

/// A log's verdict, as the API says it.
fn verdict(v: &kiln_ci::logs::Verdict) -> Value {
    use kiln_ci::logs::Verdict;
    match v {
        Verdict::Verified { kept, dropped } => json!({ "state": "verified", "kept_bytes": kept, "dropped_chunks": dropped }),
        Verdict::Altered { piece, at } => json!({ "state": "altered", "piece": piece, "at": at }),
        Verdict::Incomplete { have, want } => json!({ "state": "incomplete", "have": have, "want": want }),
        Verdict::Differs => json!({ "state": "differs" }),
    }
}

/// What kiln keeps of leg `job`, for what it `signed`: its steps' ends, each signed log's verdict,
/// its reports, its end.
async fn kept(hub: &Hub, job: &str, signed: &leg::Results) -> anyhow::Result<leg::Kept> {
    let id = job.to_owned();
    let (steps, reports, conclusion): (Vec<(i64, String, Option<i32>)>, Vec<(i64, String, String)>, Option<(String, Option<i32>)>) = hub
        .store
        .call(move |c| {
            let mut st = c.prepare("SELECT idx, status, exit_code FROM steps WHERE job_id = ?1")?;
            let steps = st.query_map([&id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<Vec<_>, _>>()?;
            let mut st = c.prepare("SELECT step_idx, path, object FROM test_reports WHERE leg = ?1")?;
            let reports = st.query_map([&id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<Vec<_>, _>>()?;
            let end = c.query_row("SELECT status, exit_code FROM jobs WHERE id = ?1", [&id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
            Ok((steps, reports, end))
        })
        .await?;
    let mut logs = std::collections::BTreeMap::new();
    for (idx, chain) in &signed.logs {
        logs.insert(*idx, hub.logs.check(job, *idx, chain).await);
    }
    // What kiln keeps as taken from the leg of a step it signed no log of.
    let mut unsigned_logs = std::collections::BTreeMap::new();
    for idx in hub.logs.kept_steps(job).await.into_iter().filter(|idx| !signed.logs.contains_key(idx)) {
        let chunks = hub.logs.pieces(job, idx).await.last().map_or(0, |p| p.len() as u64);
        if chunks > 0 {
            unsigned_logs.insert(idx, chunks);
        }
    }
    Ok(leg::Kept {
        steps: steps.into_iter().filter_map(|(idx, status, exit)| Some((u32::try_from(idx).ok()?, (status, exit)))).collect(),
        logs,
        reports: reports.into_iter().filter_map(|(idx, path, object)| Some(leg::Report { idx: u32::try_from(idx).ok()?, path, sha256: object.trim_start_matches("sha256:").to_owned() })).collect(),
        conclusion,
        unsigned_logs,
        reused: std::collections::BTreeSet::new(),
    })
}

/// Leg `job`'s signed records (its latest attempt's) as kiln holds them now: who signed its
/// results and when kiln took them, the delegation they acted under, each record, each log's
/// verdict and each break between them and what kiln keeps (none: they hold); `None` when the leg
/// sent none.
pub async fn judged(hub: &Hub, job: &str) -> anyhow::Result<Option<Value>> {
    let id = job.to_owned();
    let rows: Vec<Row> = hub
        .store
        .call(move |c| {
            let mut st = c.prepare(
                "SELECT attempt, digest, payload_type, bundle, signer, uri, taken_at, refused FROM ci_leg_records WHERE job_id = ?1 AND attempt = (SELECT MAX(attempt) FROM ci_leg_records WHERE job_id = ?1) ORDER BY taken_at, rowid",
            )?;
            let rows = st
                .query_map([&id], |x| Ok(Row { attempt: x.get(0)?, digest: x.get(1)?, payload_type: x.get(2)?, bundle: x.get(3)?, signer: x.get(4)?, uri: x.get(5)?, taken_at: x.get(6)?, refused: x.get(7)? }))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await?;
    if rows.is_empty() {
        return Ok(None);
    }
    let server = server_uri(hub)?;
    // Each checked again as kiln took it: at the time it did, against the revocations as they
    // stand, the leg's under the delegation kiln's server gave it.
    let against = Against::of(hub, job).await?;
    let mut took: Vec<(&Row, Result<statement::Under, String>)> = Vec::new();
    for r in &rows {
        let u = match &r.refused {
            Some(why) => Err(why.clone()),
            None => match against.check(hub, &serde_json::from_str(&r.bundle)?, r.taken_at) {
                Ok(u) if u.statement.custodian == server => Ok(u),
                Ok(u) => leg::under_kilns_delegation(&u, &server).map(|()| u),
                Err(e) => Err(e.to_string()),
            },
        };
        took.push((r, u));
    }
    let records: Vec<Value> = took
        .iter()
        .map(|(r, u)| {
            let c = u.as_ref().ok().map(|u| &u.statement);
            json!({
                "digest": r.digest, "payload_type": r.payload_type, "kind": c.map(leg::kind), "stream": c.and_then(|c| c.statement.stream.clone()), "seq": c.and_then(|c| c.statement.seq),
                "signer": r.signer, "uri": r.uri, "taken_at": r.taken_at, "refused": u.as_ref().err(), "bundle": serde_json::from_str::<Value>(&r.bundle).unwrap_or(Value::Null),
            })
        })
        .collect();
    let Some((results_row, results)) = took.iter().find(|(r, _)| r.payload_type == leg::PAYLOAD_RESULTS) else {
        let why = took.iter().find_map(|(_, u)| u.as_ref().err().cloned()).unwrap_or_else(|| "it sent no results".into());
        return Ok(Some(json!({ "state": "refused", "why": why, "records": records })));
    };
    let refusal = |why: String| Some(json!({ "state": "refused", "why": why, "signer": results_row.signer, "taken_at": results_row.taken_at, "records": records }));
    let u = match results {
        Ok(u) => u,
        Err(why) => return Ok(refusal(why.clone())),
    };
    let signed = match leg::read(&u.statement.statement) {
        Ok(s) => s,
        Err(why) => return Ok(refusal(why)),
    };
    // Its custody: the leg's records, kiln's receipts, held to the delegation that gave it the leg.
    let fine: Vec<&statement::Checked> = took.iter().filter_map(|(_, u)| u.as_ref().ok()).map(|u| &u.statement).collect();
    let custody = |of_kiln: bool| fine.iter().copied().filter(|c| c.payload_type == statement::PAYLOAD_CUSTODY && (c.custodian == server) == of_kiln).collect::<Vec<_>>();
    let mut kept = kept(hub, job, &signed).await?;
    // The steps it says it received reused: what kiln copied into them is their source leg's.
    kept.reused = leg::reused(&custody(false));
    let mut breaks = leg::judge(&signed, &kept);
    let delegation = u.through.first();
    let served = served_of(hub, job, results_row.attempt).await?;
    breaks.extend(leg::judge_custody(&leg::Taken { legs: custody(false), kilns: custody(true), results: Some(&u.statement), delegation, served }));
    breaks.extend(took.iter().filter_map(|(r, u)| u.as_ref().err().map(|why| format!("its record {} was refused: {why}", r.digest))));
    let under = delegation.map(|d| json!({ "digest": d.digest, "signer": d.signer.to_string(), "uri": d.custodian, "taken_at": d.at, "bundle": against.delegations.get(&d.digest).map(|(b, _)| b.clone()) }));
    Ok(Some(json!({
        "state": if breaks.is_empty() { "verified" } else { "broken" },
        "signer": results_row.signer,
        "uri": results_row.uri,
        "digest": results_row.digest,
        "attempt": results_row.attempt,
        "taken_at": results_row.taken_at,
        "under": under,
        "breaks": breaks,
        "logs": kept.logs.iter().map(|(idx, v)| json!({ "step": idx, "verdict": verdict(v) })).collect::<Vec<_>>(),
        "steps": signed.steps.values().collect::<Vec<_>>(),
        "reports": signed.reports.iter().map(|r| json!({ "step": r.idx, "path": r.path, "sha256": r.sha256 })).collect::<Vec<_>>(),
        "conclusion": signed.conclusion,
        "records": records,
    })))
}

/// Why leg `job` sent no signed records, as far as kiln can tell; `None`: kiln knows no such leg.
async fn unsigned(hub: &Hub, job: &str) -> anyhow::Result<Option<String>> {
    let id = job.to_owned();
    let agent: Option<Option<String>> = hub.store.call(move |c| Ok(c.query_row("SELECT agent FROM jobs WHERE id = ?1", [&id], |r| r.get::<_, Option<String>>(0)).optional()?)).await?;
    let Some(agent) = agent else { return Ok(None) };
    let Some(a) = agent else { return Ok(Some("it hasn't run on a runner yet".to_owned())) };
    // Whether kiln gave the leg an identity, and whether the leg signs (its runner said so of
    // itself, or of the leg as it asked for that identity).
    let delegated = !crate::identities::delegations_of(&hub.store, job).await?.is_empty();
    let signs = hub.runner_wide_caps(&a) & crate::proto::CAP_LEG_SIGNS != 0 || leg_signs(&hub.store, job).await?;
    // Its first step waits for its identity a while, then runs without it (W27-R05).
    let late = delegated && signs && certified_after_its_end(&hub.store, job).await?;
    // Its runner by the name people see (docs/machines.md).
    let a = hub.shown(&a);
    Ok(Some(match (delegated, signs) {
        (true, true) if late => format!("kiln certified its identity only after the leg had ended: its runner {a} had none to sign its records with"),
        (true, true) => format!("its runner {a} sent none: the leg hasn't ended, or what it sent was lost on the way"),
        (true, false) => format!("its runner {a} signs no legs' records: a kiln from before them"),
        (false, true) => format!("kiln gave it no identity: its runner {a} asked for none for it (a kiln from before v1 jobs signed theirs), or kiln couldn't give one"),
        (false, false) => format!("kiln gave it no identity: its runner {a} asked for none (a kiln from before them)"),
    }))
}

/// Whether kiln's latest delegation of leg `job` (as it certified its identity) came after the leg
/// had ended: kiln answered too late for the leg to sign with it (docs/ci.md W27-R05).
async fn certified_after_its_end(store: &crate::store::Store, job: &str) -> anyhow::Result<bool> {
    let j = job.to_owned();
    store
        .call(move |c| {
            let sql = "SELECT j.finished_at IS NOT NULL AND (SELECT MAX(d.taken_at) FROM ci_leg_delegations d WHERE d.job_id = j.id) > j.finished_at FROM jobs j WHERE j.id = ?1";
            Ok(c.query_row(sql, [&j], |r| r.get::<_, Option<bool>>(0)).optional()?.flatten().unwrap_or(false))
        })
        .await
}

/// `GET /api/ci/legs/:id/signed`.
pub async fn get(hub: &Hub, id: &str) -> Result<Response, Fail> {
    let internal = |e: anyhow::Error| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"));
    match judged(hub, id).await.map_err(internal)? {
        Some(v) => Ok(srv::json(StatusCode::OK, &v)),
        None => match unsigned(hub, id).await.map_err(internal)? {
            Some(why) => Ok(srv::json(StatusCode::OK, &json!({ "state": "unsigned", "why": why }))),
            None => Err((StatusCode::NOT_FOUND, format!("kiln knows no leg {id}"))),
        },
    }
}
