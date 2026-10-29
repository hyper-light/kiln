//! The work ledger's endpoints for runners (docs/ci.md W24-R07, R02, R04): steps looked up before
//! they run and their successes kept, at `/_kiln/work/v1/`, with a leg's runtime token, as
//! `kiln/cache@v1` reaches `/_kiln/cache` (`chunks`). Entries are the ledger's (`crate::ci::work`),
//! in the leg's scope `owner/repo|domain|`: its repository, and its run's trust as the server has
//! it (`runs.trust`, what its assignment's `trusted` says), never as a request says it, so an
//! untrusted leg's results never reach a trusted one (W24-R21).
//!
//! - `POST lookup` `{ unit, step, max_age }`: the step's unit (`kiln_ci::work::key::Unit`), its
//!   trust the leg's as the server has it, keyed and found on the ledger's one path
//!   (`work::lookup`, W24-R30). 204 when nothing stands for it; else 200 with the strong key it was
//!   found under, the step's recorded outputs (`kiln_ci::work::memo::Replay`, encoded) and its
//!   source (run, its number, leg, step, runner, when, how long it took). The leg's step takes the
//!   source step's log, summary, annotations and test reports over, as a reused leg takes its
//!   source's (W08-R10). Only a success stands: one within `max_age`, whose leg is still kept and
//!   whose files are still kept.
//!   With `flight: true` (W28-R72 to R75), a miss is a turn in the table of steps in flight
//!   (`kiln_ci::work::flight`, [`Flights`]): 201 `{ lease }` (execute it, renewing the lease) or
//!   202 `{ in_flight }` (another leg's step executes it: wait).
//! - `POST wait` `{ unit, step, max_age }`: a waiter asks again, held up to 20 s until the ledger
//!   has the result (200, as `lookup`), its turn comes (201) or still 202.
//! - `POST renew` `{ lease }` → `{ held }`; `POST release` `{ lease }`: a lease let go without a
//!   success (a failure, a cancel), so the next waiter takes it.
//! - `POST record` `{ unit, step, ms, max_age, outputs, lease? }`: a step's success, kept on the
//!   same path (`work::record`): 201 `{ key }`; its lease, if it held one, let go after.
//!   Its files' tree (`outputs.files`) and every file in it must be in the store already; they're
//!   kept while a lookup could reuse it (`work.object`, until its `max_age` is up), then
//!   [`sweep`] lets them go and the store collects them.
//! - `POST missing` `{ digests }`: those to upload: all but what the scope's entries hold already
//!   (what other repositories or domains hold is never said).
//! - `PUT blobs/<sha256>`: an object, checked against its digest.
//! - `GET blobs/<sha256>`: an object the scope's entries refer to.
//! - `POST tests/select` `{ binaries, context, files, affected? }`: which of a build's test
//!   binaries run and which replay a recorded pass, each saying why (W24-R10); `files` are the
//!   files they may have read, as they are now.
//! - `POST tests/record` `{ binary, context, reads, ran }`: a test binary's run, a pass kept for
//!   the runs after it, every run counted in its key's flaky marks (W24-R10).
//! - `POST ops/lookup` `{ op, inputs }` and `POST ops/record` `{ op, inputs, outputs, ms? }`: a
//!   container operation (`work::ops`, its JSON in `docs/work-ops.schema.json`), its inputs by role
//!   as their outputs' digests, looked up and recorded on the ledger's one path, in the leg's own
//!   trust domain whatever the request says (W24-R29, R21; W25's engine).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use http::{Method, StatusCode, header};
use kiln_ci::work::key::{Key, PathSet, Read, Unit};
use kiln_ci::work::tests::{self as test_results, Choice};
use kiln_ci::work::flight;
use kiln_ci::work::memo::{self, Digest, Replay};
use rusqlite::{OptionalExtension as _, params};
use serde::Deserialize;
use serde_json::json;

use super::{blob, cas};
use crate::ci::work::{self, Family, Provenance, Recorded};
use crate::ci::{Fail, token};
use crate::http::server::{self as srv, Request, Response};
use crate::hub::Hub;

/// The largest tree a step's files make.
const MAX_TREE: u64 = 64 << 20;
/// The most digests one `missing` asks about.
const MAX_ASKED: usize = 200_000;

fn msg(status: StatusCode, message: &str) -> Response {
    srv::json(status, &json!({ "message": message }))
}

fn internal(e: anyhow::Error) -> Fail {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}

fn bad(e: impl std::fmt::Display) -> Fail {
    (StatusCode::BAD_REQUEST, e.to_string())
}

/// A SHA-256 as hex, lowercase.
fn is_digest(d: &str) -> bool {
    d.len() == 64 && d.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The leg a token is for, as the server has it.
struct LegOf {
    id: String,
    run: String,
    repo: String,
    /// Its scope in the ledger: `owner/repo|domain|`.
    scope: String,
    domain: &'static str,
    attempt: u32,
    runner: String,
    sha: String,
    key: String,
    branch: String,
    run_attempt: i64,
    /// The run's number and workflow, as a waiter is told of the step it waits on (W28-R75).
    number: i64,
    workflow: String,
}

impl LegOf {
    /// A unit a request sends, in this leg's trust domain as the server has it, whatever the
    /// request says (W24-R21): what the ledger keys (W24-R30).
    fn unit(&self, sent: Unit) -> Unit {
        Unit { trust: self.domain.to_owned(), ..sent }
    }
}

async fn leg_of(hub: &Hub, claims: &token::Claims) -> anyhow::Result<Option<LegOf>> {
    let (leg, run) = (claims.sub.clone(), claims.run.clone());
    let row: Option<(String, String, String, String, i64, String, String, String, String, i64, String, i64, String)> = hub
        .store
        .call(move |c| {
            Ok(c.query_row(
                "SELECT j.id, j.run_id, p.name, r.trust, j.attempt, COALESCE(j.agent, ''), r.sha, COALESCE(j.key, j.id), r.branch, COALESCE(j.run_attempt, 1), COALESCE(r.event, ''),
                        COALESCE(r.number, 0), COALESCE(r.workflow_id, '')
                 FROM jobs j JOIN runs r ON r.id = j.run_id JOIN repos p ON p.id = r.repo_id WHERE j.id = ?1 AND j.run_id = ?2",
                params![leg, run],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?, r.get(9)?, r.get(10)?, r.get(11)?, r.get(12)?)),
            )
            .optional()?)
        })
        .await?;
    Ok(row.map(|(id, run, repo, trust, attempt, runner, sha, key, branch, run_attempt, event, number, workflow)| {
        // A diff's own work, as it's reviewed, is untrusted: the land queue never takes it (W24-R20).
        let domain = crate::ci::reuse::domain(crate::ci::reuse::work_trusted(trust == "trusted", &event));
        LegOf { scope: work::scope(&repo, domain), id, run, repo, domain, attempt: u32::try_from(attempt).unwrap_or(0), runner, sha, key, branch, run_attempt, number, workflow }
    }))
}

/// `/_kiln/work/v1/<rest>`, for a leg's token.
pub async fn route(hub: &Arc<Hub>, rest: &[&str], req: Request) -> Result<Response, Fail> {
    let Some(claims) = token::of_request(hub, req.headers()) else {
        return Ok(msg(StatusCode::UNAUTHORIZED, "a leg's ACTIONS_RUNTIME_TOKEN is needed"));
    };
    let Some(leg) = leg_of(hub, &claims).await.map_err(internal)? else { return Ok(msg(StatusCode::NOT_FOUND, "the leg is gone")) };
    let method = req.method().clone();
    match (method, rest) {
        (Method::POST, ["lookup"]) => lookup(hub, &leg, req).await,
        (Method::POST, ["record"]) => record(hub, &leg, req).await,
        (Method::POST, ["wait"]) => wait(hub, &leg, req).await,
        (Method::POST, ["renew"]) => renew(hub, req).await,
        (Method::POST, ["release"]) => release(hub, req).await,
        (Method::POST, ["missing"]) => missing(hub, &leg, req).await,
        (Method::POST, ["tests", "select"]) => tests_select(hub, &leg, req).await,
        (Method::POST, ["tests", "record"]) => tests_record(hub, &leg, req).await,
        (Method::POST, ["ops", "lookup"]) => ops_lookup(hub, &leg, req).await,
        (Method::POST, ["ops", "record"]) => ops_record(hub, &leg, req).await,
        (Method::PUT, ["blobs", digest]) => put_blob(hub, digest, req).await,
        (Method::GET, ["blobs", digest]) => get_blob(hub, &leg, digest).await,
        _ => Ok(msg(StatusCode::NOT_FOUND, "not found")),
    }
}

#[derive(Deserialize)]
struct TestsSelectIn {
    binaries: Vec<test_results::Binary>,
    context: test_results::Context,
    /// The files the binaries may have read, as they are now, by path from the workspace.
    #[serde(default)]
    files: BTreeMap<String, Read>,
    /// The packages the change can affect (W24-R17); every binary's when none.
    #[serde(default)]
    affected: Option<BTreeSet<String>>,
}

/// Which of a build's test binaries run and which replay a recorded pass (W24-R10), each saying
/// why: what the binaries read checked against `files`, a path not among them counting as gone.
async fn tests_select(hub: &Arc<Hub>, leg: &LegOf, req: Request) -> Result<Response, Fail> {
    let t: TestsSelectIn = srv::body_json(req.into_body(), 64 << 20).await.map_err(bad)?;
    let cx = test_results::Context { trust: leg.domain.to_owned(), ..t.context };
    let affected = t.affected.unwrap_or_else(|| t.binaries.iter().map(|b| b.package.clone()).collect());
    let (scope, binaries, files) = (leg.scope.clone(), t.binaries, t.files);
    let out: Vec<serde_json::Value> = hub
        .store
        .call(move |c| {
            let now = |p: &PathSet| p.keys().map(|path| (path.clone(), files.get(path).cloned().unwrap_or(Read::Absent))).collect::<PathSet>();
            let mut out = Vec::new();
            for (b, choice) in work::select_tests(c, &scope, &binaries, &cx, &affected, &now)? {
                out.push(match choice {
                    Choice::Run { why } => json!({ "package": b.package, "name": b.name, "run": true, "why": why }),
                    Choice::Replay { from, why } => {
                        let number: Option<i64> = c.query_row("SELECT number FROM runs WHERE id = ?1", [&from.run], |r| r.get(0)).optional()?;
                        json!({ "package": b.package, "name": b.name, "run": false, "why": why, "from": { "run": from.run, "number": number, "leg": from.leg, "ran": from.ran, "flaky": from.flaky } })
                    }
                });
            }
            Ok(out)
        })
        .await
        .map_err(internal)?;
    Ok(srv::json(StatusCode::OK, &json!({ "binaries": out })))
}

#[derive(Deserialize)]
struct OpIn {
    op: kiln_ci::work::ops::Op,
    /// Its inputs by role, each its output's digest.
    #[serde(default)]
    inputs: BTreeMap<String, String>,
    /// For `record`: what it made, by name (`tree`, `config`, …), each a stored object's digest.
    #[serde(default)]
    outputs: BTreeMap<String, String>,
    #[serde(default)]
    ms: u64,
}

/// A container operation's result, if one stands for it in the leg's domain (W24-R29, W25).
async fn ops_lookup(hub: &Arc<Hub>, leg: &LegOf, req: Request) -> Result<Response, Fail> {
    let o: OpIn = srv::body_json(req.into_body(), 16 << 20).await.map_err(bad)?;
    let unit = leg.unit(kiln_ci::work::ops::unit_of(&o.op, o.inputs));
    let scope = leg.scope.clone();
    let found = hub
        .store
        .call(move |c| {
            let same = |p: &PathSet| p.clone();
            let Some(work::Hit { key, recorded, .. }) = work::lookup(c, &scope, &unit, &same)? else { return Ok(None) };
            let number: Option<i64> = c.query_row("SELECT number FROM runs WHERE id = ?1", [&recorded.provenance.run], |r| r.get(0)).optional()?;
            Ok(Some(json!({ "hit": true, "key": key.hex(), "outputs": recorded.outputs, "source": { "run": recorded.provenance.run, "number": number, "leg": recorded.provenance.leg } })))
        })
        .await
        .map_err(internal)?;
    Ok(srv::json(StatusCode::OK, &found.unwrap_or_else(|| json!({ "hit": false }))))
}

/// A container operation's result, kept for the operations after it: each output a stored object.
async fn ops_record(hub: &Arc<Hub>, leg: &LegOf, req: Request) -> Result<Response, Fail> {
    let o: OpIn = srv::body_json(req.into_body(), 16 << 20).await.map_err(bad)?;
    if o.outputs.is_empty() {
        return Ok(msg(StatusCode::BAD_REQUEST, "an operation's result names what it made (`outputs`)"));
    }
    let unit = leg.unit(kiln_ci::work::ops::unit_of(&o.op, o.inputs));
    let now = crate::util::now_unix();
    let objects: Vec<String> = o.outputs.values().filter_map(|d| d.strip_prefix("sha256:").map(str::to_owned)).collect();
    let result = Recorded {
        outcome: "success".into(),
        exit: Some(0),
        outputs: o.outputs,
        provenance: Provenance { repo: leg.repo.clone(), run: leg.run.clone(), leg: leg.id.clone(), attempt: leg.attempt, runner: leg.runner.clone(), commit: leg.sha.clone(), ms: o.ms, finished_at: now, ..Provenance::default() },
        domain: leg.domain.to_owned(),
        ..Recorded::default()
    };
    let scope = leg.scope.clone();
    let kept: Result<Key, String> = hub
        .store
        .call(move |c| {
            for d in &objects {
                let stored: bool = c.query_row("SELECT EXISTS(SELECT 1 FROM cas_index WHERE digest = ?1)", [d], |r| r.get(0))?;
                if !stored {
                    return Ok(Err(format!("sha256:{d} isn't stored: upload it first")));
                }
            }
            let tx = crate::store::tx(c)?;
            let key = work::record(&tx, &scope, &unit, &PathSet::new(), &result)?;
            tx.commit()?;
            Ok(Ok(key))
        })
        .await
        .map_err(internal)?;
    match kept {
        Ok(key) => Ok(srv::json(StatusCode::CREATED, &json!({ "key": key.hex() }))),
        Err(e) => Ok(msg(StatusCode::BAD_REQUEST, &e)),
    }
}

#[derive(Deserialize)]
struct TestsRecordIn {
    binary: test_results::Binary,
    context: test_results::Context,
    #[serde(default)]
    reads: PathSet,
    ran: test_results::Ran,
}

/// A test binary's run (W24-R10): a pass kept for the runs after it; every run counted in its
/// key's flaky marks. The leg's own domain, whatever the runner sent.
async fn tests_record(hub: &Arc<Hub>, leg: &LegOf, req: Request) -> Result<Response, Fail> {
    let t: TestsRecordIn = srv::body_json(req.into_body(), 64 << 20).await.map_err(bad)?;
    let cx = test_results::Context { trust: leg.domain.to_owned(), ..t.context };
    let provenance = Provenance { repo: leg.repo.clone(), run: leg.run.clone(), leg: leg.id.clone(), attempt: leg.attempt, runner: leg.runner.clone(), commit: leg.sha.clone(), ms: t.ran.ms, finished_at: crate::util::now_unix(), ..Provenance::default() };
    let scope = leg.scope.clone();
    let kept = hub.store.call(move |c| work::record_test(c, &scope, &t.binary, &cx, &t.reads, &t.ran, &provenance)).await.map_err(internal)?;
    Ok(srv::json(StatusCode::CREATED, &json!({ "kept": kept.map(|k| k.hex()) })))
}

#[derive(Deserialize)]
struct LookupIn {
    /// The step's unit (W24-R01): the ledger keys it (W24-R30).
    unit: Unit,
    step: u32,
    #[serde(default)]
    max_age: Option<u64>,
    /// A miss is a turn in the table of steps in flight (W28-R72): 201 or 202 rather than 204.
    #[serde(default)]
    flight: bool,
}

/// What stands for a step's unit: the newest success in the scope, within `max_age`, its leg still
/// kept (a pruned run takes its logs with it), and its files still kept: its tree's `work.object`
/// not yet due (each file in it is kept at least as long, [`keep_object`]) and in the store.
async fn lookup(hub: &Arc<Hub>, leg: &LegOf, req: Request) -> Result<Response, Fail> {
    let l: LookupIn = srv::body_json(req.into_body(), 16 << 20).await.map_err(bad)?;
    let unit = leg.unit(l.unit);
    if let Some(found) = found(hub, leg, &unit, l.max_age).await.map_err(internal)? {
        return hit(hub, leg, l.step, found).await;
    }
    if !l.flight {
        return Ok(srv::status(StatusCode::NO_CONTENT));
    }
    turn(hub, leg, &unit, l.step).await
}

/// A waiter asks again (W28-R75): held until the ledger has the result, its turn comes, or 20 s
/// pass (the runner asks again, renewing its place).
async fn wait(hub: &Arc<Hub>, leg: &LegOf, req: Request) -> Result<Response, Fail> {
    let l: LookupIn = srv::body_json(req.into_body(), 16 << 20).await.map_err(bad)?;
    let unit = leg.unit(l.unit);
    let until = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        if let Some(found) = found(hub, leg, &unit, l.max_age).await.map_err(internal)? {
            return hit(hub, leg, l.step, found).await;
        }
        let answer = turn(hub, leg, &unit, l.step).await?;
        if answer.status() != StatusCode::ACCEPTED || std::time::Instant::now() >= until {
            return Ok(answer);
        }
        crate::rt::sleep(std::time::Duration::from_millis(500)).await;
    }
}

/// What stands for a step's unit: the newest success in the scope, within `max_age`, its leg still
/// kept (a pruned run takes its logs with it), and its files still kept: its tree's `work.object`
/// not yet due (each file in it is kept at least as long, [`keep_object`]) and in the store.
async fn found(hub: &Arc<Hub>, leg: &LegOf, unit: &Unit, max_age: Option<u64>) -> anyhow::Result<Option<(Key, Recorded, i64, Option<String>)>> {
    let now = crate::util::now_unix();
    let since = now.saturating_sub(i64::try_from(max_age.unwrap_or(memo::MAX_AGE)).unwrap_or(i64::MAX));
    let (scope, unit) = (leg.scope.clone(), unit.clone());
    hub.store
        .call(move |c| {
            let same = |p: &PathSet| p.clone();
            let Some(work::Hit { key, recorded: rec, class, .. }) = work::lookup(c, &scope, &unit, &same)? else { return Ok(None) };
            if rec.outcome != "success" || rec.provenance.finished_at < since {
                return Ok(None);
            }
            if let Some(tree) = rec.outputs.get("files") {
                let Some(d) = Digest::parse(tree) else { return Ok(None) };
                let due: Option<i64> = c.query_row("SELECT at FROM work_records WHERE family = ?1 AND key = ?2", params![Family::Object.name(), format!("{scope}{}", d.hash)], |r| r.get(0)).optional()?;
                let stored: bool = c.query_row("SELECT EXISTS(SELECT 1 FROM cas_index WHERE digest = ?1)", [&d.hash], |r| r.get(0))?;
                if !due.is_some_and(|at| at >= now) || !stored {
                    return Ok(None);
                }
            }
            let number: Option<i64> = c.query_row("SELECT r.number FROM jobs j JOIN runs r ON r.id = j.run_id WHERE j.id = ?1", [&rec.provenance.leg], |r| r.get(0)).optional()?;
            Ok(number.map(|n| (key, rec, n, class.map(|b| b.class.rep()))))
        })
        .await
}

/// A hit: the leg's step takes its source's log over, and is told what it left and where from,
/// and through which class when equivalence found it (W24-R30, R28).
async fn hit(hub: &Arc<Hub>, leg: &LegOf, step: u32, (key, rec, number, class): (Key, Recorded, i64, Option<String>)) -> Result<Response, Fail> {
    // A hit through equivalence says so, and counts by layer (W24-R30, R28).
    crate::metrics::global().work_lookups.inc(&["step", if class.is_some() { "class" } else { "hit" }]);
    if class.is_some() {
        crate::metrics::global().work_classes.inc(&["step", "hit"]);
    }
    take_over(hub, leg, step, &rec).await.map_err(internal)?;
    let p = &rec.provenance;
    let workflow: Option<String> = {
        let run = p.run.clone();
        hub.store.call(move |c| Ok(c.query_row("SELECT workflow_id FROM runs WHERE id = ?1", [&run], |r| r.get::<_, Option<String>>(0)).optional()?.flatten())).await.map_err(internal)?
    };
    Ok(srv::json(
        StatusCode::OK,
        &json!({
            "key": key.hex(),
            "outputs": rec.outputs,
            "class": class,
            "source": { "run": p.run, "number": number, "workflow": workflow, "leg": p.leg, "step": p.step, "attempt": p.attempt, "runner": p.runner, "commit": p.commit, "finished_at": p.finished_at, "ms": p.ms },
        }),
    ))
}

/// The unit's key in the table of steps in flight: its weak key whatever its domain, so a leg may
/// wait on an execution of a domain it reads, within its repository.
fn flight_key(leg: &LegOf, unit: &Unit) -> String {
    format!("{}|{}", leg.repo, kiln_ci::work::key::weak(&Unit { trust: String::new(), ..unit.clone() }).hex())
}

fn holder(leg: &LegOf, step: u32) -> flight::Holder {
    flight::Holder { run: leg.run.clone(), number: leg.number, workflow: leg.workflow.clone(), leg: leg.id.clone(), step, attempt: leg.attempt, runner: leg.runner.clone() }
}

/// A miss, as a turn in the table of steps in flight (W28-R72 to R74).
async fn turn(hub: &Arc<Hub>, leg: &LegOf, unit: &Unit, step: u32) -> Result<Response, Fail> {
    let domain = kiln_ci::work::trust::Domain::parse(leg.domain).map_err(bad)?;
    let (key, who) = (flight_key(leg, unit), holder(leg, step));
    let answer = hub.flight.with(hub, move |f, now| f.ask(&key, &domain, &who, now)).await.map_err(internal)?;
    Ok(match answer {
        flight::Answer::Execute { lease } => srv::json(StatusCode::CREATED, &json!({ "lease": lease })),
        flight::Answer::Alone => srv::status(StatusCode::NO_CONTENT),
        flight::Answer::Wait { holder, domain, since, ahead, turn } => srv::json(
            StatusCode::ACCEPTED,
            &json!({ "in_flight": { "holder": holder, "said": holder.said(), "domain": domain, "since": since, "ahead": ahead, "turn": turn } }),
        ),
    })
}

#[derive(Deserialize)]
struct LeaseIn {
    lease: String,
}

/// The holder of a lease is still at it.
async fn renew(hub: &Arc<Hub>, req: Request) -> Result<Response, Fail> {
    let l: LeaseIn = srv::body_json(req.into_body(), 1 << 16).await.map_err(bad)?;
    let held = hub.flight.with(hub, move |f, now| f.renew(&l.lease, now)).await.map_err(internal)?;
    Ok(srv::json(StatusCode::OK, &json!({ "held": held })))
}

/// A lease let go without a success: the next waiter's turn (W28-R73).
async fn release(hub: &Arc<Hub>, req: Request) -> Result<Response, Fail> {
    let l: LeaseIn = srv::body_json(req.into_body(), 1 << 16).await.map_err(bad)?;
    let gone = hub.flight.with(hub, move |f, now| f.end(&l.lease, now).is_some()).await.map_err(internal)?;
    Ok(srv::json(StatusCode::OK, &json!({ "released": gone })))
}

/// The table of steps in flight (W28-R72 to R74), kept in memory and its leases in SQLite
/// (`work_flight`), so a restart rebuilds it: their holders renew them or they lapse.
#[derive(Default)]
pub struct Flights {
    table: async_lock::Mutex<Option<flight::Flight>>,
}

impl Flights {
    /// Runs `op` on the table (loaded once from SQLite), then keeps its leases as they stand.
    async fn with<T>(&self, hub: &Hub, op: impl FnOnce(&mut flight::Flight, i64) -> T) -> anyhow::Result<T> {
        let mut table = self.table.lock().await;
        if table.is_none() {
            let rows: Vec<String> = hub.store.call(|c| Ok(c.prepare("SELECT lease FROM work_flight")?.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?)).await?;
            let mut f = flight::Flight::new();
            for json in rows {
                match serde_json::from_str::<flight::Lease>(&json) {
                    Ok(l) => f.restore(l),
                    Err(e) => tracing::warn!(error = %e, "a kept lease of a step in flight doesn't read; it's let go"),
                }
            }
            *table = Some(f);
        }
        let Some(f) = table.as_mut() else { anyhow::bail!("the table of steps in flight isn't loaded") };
        let now = crate::util::now_ms();
        let before: BTreeMap<String, flight::Lease> = f.leases().map(|l| (l.id.clone(), l.clone())).collect();
        let out = op(f, now);
        let after: BTreeMap<String, flight::Lease> = f.leases().filter(|l| now.saturating_sub(l.renewed) <= flight::LEASE_TTL_MS).map(|l| (l.id.clone(), l.clone())).collect();
        if before != after {
            let gone: Vec<String> = before.keys().filter(|k| !after.contains_key(*k)).cloned().collect();
            let put: Vec<(String, String)> = after.iter().filter(|(k, l)| before.get(*k) != Some(*l)).map(|(k, l)| (k.clone(), serde_json::to_string(l).unwrap_or_default())).collect();
            hub.store
                .call(move |c| {
                    let tx = crate::store::tx(c)?;
                    for id in &gone {
                        tx.execute("DELETE FROM work_flight WHERE id = ?1", [id])?;
                    }
                    for (id, json) in &put {
                        tx.execute("INSERT INTO work_flight (id, lease) VALUES (?1, ?2) ON CONFLICT (id) DO UPDATE SET lease = excluded.lease", params![id, json])?;
                    }
                    tx.commit()?;
                    Ok(())
                })
                .await?;
        }
        Ok(out)
    }
}

/// The leg's step `idx` takes its source's over: its log, summary, annotations and test reports.
async fn take_over(hub: &Arc<Hub>, leg: &LegOf, idx: u32, rec: &Recorded) -> anyhow::Result<()> {
    let (source, from) = (rec.provenance.leg.clone(), rec.provenance.step.unwrap_or(0));
    let log = hub.logs.read(&source, from).await;
    if !log.is_empty() {
        let (logs, id) = (hub.logs.clone(), leg.id.clone());
        crate::rt::blocking(move || {
            for piece in log.chunks(crate::proto::LOG_CHUNK) {
                logs.append(&id, idx, piece);
            }
        })
        .await?;
    }
    let (id, run, key, attempt, branch, sha) = (leg.id.clone(), leg.run.clone(), leg.key.clone(), leg.run_attempt, leg.branch.clone(), leg.sha.clone());
    hub.store
        .call(move |c| {
            let tx = crate::store::tx(c)?;
            let now = crate::util::now_unix();
            tx.execute("INSERT OR REPLACE INTO ci_summaries (leg_id, step_idx, markdown) SELECT ?1, ?2, markdown FROM ci_summaries WHERE leg_id = ?3 AND step_idx = ?4", params![id, idx, source, from])?;
            tx.execute(
                "INSERT INTO ci_annotations (leg_id, step_idx, level, title, message, file, line, end_line, col, end_col, log_offset)
                 SELECT ?1, ?2, level, title, message, file, line, end_line, col, end_col, log_offset FROM ci_annotations WHERE leg_id = ?3 AND step_idx = ?4 ORDER BY id",
                params![id, idx, source, from],
            )?;
            let reports: Vec<i64> = {
                let mut q = tx.prepare("SELECT id FROM test_reports WHERE leg = ?1 AND step_idx = ?2 ORDER BY id")?;
                q.query_map(params![source, from], |r| r.get(0))?.collect::<Result<_, _>>()?
            };
            for report in reports {
                tx.execute("INSERT INTO test_reports (leg, step_idx, path, format, object, cases, failed, ts) SELECT ?1, ?2, path, format, object, cases, failed, ?3 FROM test_reports WHERE id = ?4", params![id, idx, now, report])?;
                let copy = tx.last_insert_rowid();
                tx.execute(
                    "INSERT INTO test_results (repo_id, case_id, run_id, leg, leg_key, attempt, branch, sha, report, outcome, duration_ms, message, trace, line, retried, quarantined, ts)
                     SELECT repo_id, case_id, ?1, ?2, ?3, ?4, ?5, ?6, ?7, outcome, duration_ms, message, trace, line, retried, quarantined, ?8 FROM test_results WHERE report = ?9 ORDER BY id",
                    params![run, id, key, attempt, branch, sha, copy, now, report],
                )?;
            }
            tx.commit()?;
            Ok(())
        })
        .await?;
    hub.emit_job(&leg.id).await;
    Ok(())
}

#[derive(Deserialize)]
struct RecordIn {
    /// The step's unit (W24-R01): the ledger keys it (W24-R30).
    unit: Unit,
    step: u32,
    #[serde(default)]
    ms: u64,
    /// How long the result stays reusable (the step's `memo.max-age`): its files are kept as long.
    #[serde(default)]
    max_age: Option<u64>,
    #[serde(default)]
    outputs: BTreeMap<String, String>,
    /// The lease it executed under (W28-R72), let go once its result is kept.
    #[serde(default)]
    lease: Option<String>,
}

/// An object an entry refers to, kept in the scope until `until` at least: of every result that
/// refers to it, the one reusable longest says (so a tree's files are kept as long as it is).
fn keep_object(c: &rusqlite::Connection, scope: &str, digest: &str, key: &Key, until: i64) -> rusqlite::Result<()> {
    c.execute(
        "INSERT INTO work_records (family, key, value, at) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (family, key) DO UPDATE SET value = excluded.value, at = MAX(at, excluded.at)",
        params![Family::Object.name(), format!("{scope}{digest}"), key.hex().as_bytes(), until],
    )?;
    Ok(())
}

/// Lets go of the objects no step's result can be reused for any more (their `at` has passed):
/// the content store collects each once nothing else refers to it (`cas::collect`). How many.
pub async fn sweep(hub: &Hub) -> anyhow::Result<usize> {
    let now = crate::util::now_unix();
    let n = hub.store.call(move |c| Ok(c.execute("DELETE FROM work_records WHERE family = ?1 AND at < ?2", params![Family::Object.name(), now])?)).await?;
    if n > 0 {
        tracing::debug!(objects = n, "steps' kept files let go: no result of theirs is reusable any more");
    }
    Ok(n)
}

/// The bytes of a stored object up to `max`, when it's there.
async fn read_object(hub: &Hub, digest: &str, max: u64) -> anyhow::Result<Option<Vec<u8>>> {
    let Some(obj) = cas::object(hub, digest).await? else { return Ok(None) };
    anyhow::ensure!(obj.size <= max, "{digest} is {} bytes, more than {max}", obj.size);
    let rx = cas::read(hub, &obj, 0, obj.size);
    let mut out = Vec::with_capacity(usize::try_from(obj.size).unwrap_or(0));
    while let Ok(b) = rx.recv().await {
        out.extend_from_slice(&b);
    }
    anyhow::ensure!(out.len() as u64 == obj.size, "{digest} read back short");
    Ok(Some(out))
}

/// Keeps a step's success (W24-R07: only successes are recorded, so this is all it takes). Its
/// files, the tree and each file in it, are in the store already: checked as they're referred to,
/// in one transaction, so a collection never takes one in between.
async fn record(hub: &Arc<Hub>, leg: &LegOf, req: Request) -> Result<Response, Fail> {
    let r: RecordIn = srv::body_json(req.into_body(), 16 << 20).await.map_err(bad)?;
    let unit = leg.unit(r.unit);
    let replay = Replay::decode(&r.outputs).map_err(bad)?;
    let mut objects: Vec<String> = Vec::new();
    if let Some(d) = &replay.files {
        let Some(bytes) = read_object(hub, &d.hash, MAX_TREE).await.map_err(|e| bad(format!("{e:#}")))? else {
            return Ok(msg(StatusCode::BAD_REQUEST, &format!("the tree {} isn't stored: upload it first", d.hash)));
        };
        if Digest::of(&bytes) != *d {
            return Ok(msg(StatusCode::BAD_REQUEST, &format!("the tree {} is {} bytes, not {}", d.hash, bytes.len(), d.size_bytes)));
        }
        let tree: memo::Tree = serde_json::from_slice(&bytes).map_err(|e| bad(format!("the tree {}: {e}", d.hash)))?;
        objects.push(d.hash.clone());
        for e in memo::entries(&tree).map_err(bad)? {
            if let memo::Entry::File { digest, .. } = e {
                objects.push(digest.hash);
            }
        }
        objects.sort();
        objects.dedup();
    }
    let now = crate::util::now_unix();
    let until = now.saturating_add(i64::try_from(r.max_age.unwrap_or(memo::MAX_AGE)).unwrap_or(i64::MAX));
    let result = Recorded {
        outcome: "success".into(),
        exit: Some(0),
        outputs: r.outputs,
        provenance: Provenance { repo: leg.repo.clone(), run: leg.run.clone(), leg: leg.id.clone(), step: Some(r.step), attempt: leg.attempt, runner: leg.runner.clone(), commit: leg.sha.clone(), ms: r.ms, finished_at: now, ..Provenance::default() },
        domain: leg.domain.to_owned(),
        ..Recorded::default()
    };
    let scope = leg.scope.clone();
    let kept: Result<Key, String> = hub
        .store
        .call(move |c| {
            let tx = crate::store::tx(c)?;
            for o in &objects {
                let stored: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM cas_index WHERE digest = ?1)", [o], |r| r.get(0))?;
                if !stored {
                    return Ok(Err(format!("{o} isn't stored: upload it first")));
                }
            }
            let key = work::record(&tx, &scope, &unit, &PathSet::new(), &result)?;
            for o in &objects {
                keep_object(&tx, &scope, o, &key, until)?;
            }
            tx.commit()?;
            Ok(Ok(key))
        })
        .await
        .map_err(internal)?;
    let key = match kept {
        Ok(k) => k,
        Err(e) => return Ok(msg(StatusCode::BAD_REQUEST, &e)),
    };
    // Kept first, then the lease goes: its waiters' next lookup is a hit (W28-R72).
    if let Some(lease) = r.lease {
        hub.flight.with(hub, move |f, now| f.end(&lease, now)).await.map_err(internal)?;
    }
    tracing::debug!(leg = %leg.id, step = r.step, key = %key, "a step's result kept for reuse");
    Ok(srv::json(StatusCode::CREATED, &json!({ "key": key.hex() })))
}

#[derive(Deserialize)]
struct MissingIn {
    digests: Vec<String>,
}

/// Which of `digests` to send: all but those the store has and the scope's entries keep (not yet
/// due to go: one that is goes as soon as the next upkeep, so it's sent again).
async fn missing(hub: &Arc<Hub>, leg: &LegOf, req: Request) -> Result<Response, Fail> {
    let m: MissingIn = srv::body_json(req.into_body(), 32 << 20).await.map_err(bad)?;
    if m.digests.len() > MAX_ASKED || !m.digests.iter().all(|d| is_digest(d)) {
        return Ok(msg(StatusCode::BAD_REQUEST, &format!("`digests` lists at most {MAX_ASKED} SHA-256s, as hex")));
    }
    let (scope, asked, now) = (leg.scope.clone(), m.digests, crate::util::now_unix());
    let mut need: Vec<String> = hub
        .store
        .call(move |c| {
            let mut q = c.prepare_cached(
                "SELECT EXISTS(SELECT 1 FROM work_records WHERE family = ?1 AND key = ?2 AND at >= ?3) AND EXISTS(SELECT 1 FROM cas_index WHERE digest = ?4)",
            )?;
            let mut need = Vec::new();
            for d in asked {
                let held: bool = q.query_row(params![Family::Object.name(), format!("{scope}{d}"), now, d], |r| r.get(0))?;
                if !held {
                    need.push(d);
                }
            }
            Ok(need)
        })
        .await
        .map_err(internal)?;
    need.sort();
    need.dedup();
    Ok(srv::json(StatusCode::OK, &json!({ "missing": need })))
}

/// An object, taken only when it is what its digest says.
async fn put_blob(hub: &Arc<Hub>, digest: &str, req: Request) -> Result<Response, Fail> {
    if !is_digest(digest) {
        return Ok(msg(StatusCode::BAD_REQUEST, "an object is named by its SHA-256, as hex"));
    }
    let staged = cas::staging(hub).join(format!("work-{}", crate::util::new_id()));
    if let Some(dir) = staged.parent() {
        std::fs::create_dir_all(dir).map_err(|e| internal(e.into()))?;
    }
    let max = hub.cfg.ci.cache.max_entry_gb.saturating_mul(1 << 30);
    let cleanup = |p: &std::path::Path| {
        let _ = std::fs::remove_file(p);
    };
    if let Err(e) = blob::receive(req, &staged, max).await {
        cleanup(&staged);
        return Ok(msg(StatusCode::BAD_REQUEST, &format!("the object: {e:#}")));
    }
    let p = staged.clone();
    let hashed = crate::rt::blocking(move || crate::sync::blobs::hash_file(&p)).await.map_err(internal)?.map_err(|e| internal(e.into()));
    let got = match hashed {
        Ok(h) => h,
        Err(e) => {
            cleanup(&staged);
            return Err(e);
        }
    };
    if got != digest {
        cleanup(&staged);
        return Ok(msg(StatusCode::BAD_REQUEST, &format!("the body hashes to {got}, not {digest}")));
    }
    let kept = cas::put_file(hub, &staged).await;
    cleanup(&staged);
    let obj = kept.map_err(internal)?;
    Ok(srv::json(StatusCode::OK, &json!({ "size": obj.size })))
}

/// An object the scope's entries refer to; anything else isn't there, as far as the leg is told.
async fn get_blob(hub: &Arc<Hub>, leg: &LegOf, digest: &str) -> Result<Response, Fail> {
    if !is_digest(digest) {
        return Ok(msg(StatusCode::NOT_FOUND, "no such object"));
    }
    let k = format!("{}{digest}", leg.scope);
    let referred = hub.store.call(move |c| Ok(work::get(c, Family::Object, &k)?.is_some())).await.map_err(internal)?;
    if !referred {
        return Ok(msg(StatusCode::NOT_FOUND, "no such object"));
    }
    let Some(obj) = cas::object(hub, digest).await.map_err(internal)? else { return Ok(msg(StatusCode::NOT_FOUND, "the object is gone")) };
    let r = srv::response(StatusCode::OK, "application/octet-stream", srv::channel_body(cas::read(hub, &obj, 0, obj.size)));
    Ok(srv::with_header(r, header::CONTENT_LENGTH, &obj.size.to_string()))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::Value;
    use std::time::Duration;

    /// A repository, a run of it (`trust`) and a leg of the run in `hub`'s store, and the leg's
    /// token: what the endpoints need of a leg.
    pub(crate) async fn leg(hub: &Hub, repo: &str, run: &str, leg: &str, trusted: bool) -> anyhow::Result<String> {
        let r = crate::store::Repo { id: format!("r-{repo}"), name: repo.into(), clone_url: format!("https://example.com/{repo}.git"), branches: Vec::new(), poll_seconds: 60, enabled: true, created_at: 0, last_polled_at: None, last_error: None, mode: "upstream".into(), default_branch: None, mirror: None };
        let repo_id = hub.store.upsert_repo(r).await?.id;
        let (rid, run_id, leg_id) = (repo_id.clone(), run.to_owned(), leg.to_owned());
        hub.store
            .call(move |c| {
                let number: i64 = c.query_row("SELECT COALESCE(MAX(number), 0) + 1 FROM runs WHERE repo_id = ?1", [&rid], |r| r.get(0))?;
                c.execute(
                    "INSERT OR IGNORE INTO runs (id, repo_id, number, pipeline, event, branch, sha, status, created_at, workflow_id, trust) VALUES (?1, ?2, ?3, 'ci', 'push', 'main', 'abc123', 'running', 1, '.kiln/ci.yaml', ?4)",
                    params![run_id, rid, number, if trusted { "trusted" } else { "untrusted" }],
                )?;
                c.execute("INSERT INTO jobs (id, run_id, key, display, status, labels, needs, spec, agent, attempt, job_id) VALUES (?1, ?2, 'build', 'build', 'running', '[]', '[]', '{}', 'runner-a', 1, 'build')", params![leg_id, run_id])?;
                Ok(())
            })
            .await?;
        Ok(token::mint(hub, &token::Leg { leg, run, repo: &repo_id, cache: Vec::new(), cache_mode: "write", ttl: 3600 }))
    }

    /// The endpoints for `hub` on a port of their own, as the runner reaches the server: its base URL.
    pub(crate) fn serve(hub: Arc<Hub>) -> anyhow::Result<String> {
        struct Work(Arc<Hub>);
        impl srv::Handler for Work {
            async fn handle(self: Arc<Self>, req: Request) -> Response {
                let path = req.uri().path().to_owned();
                let seg: Vec<&str> = path.trim_start_matches('/').split('/').collect();
                let r = match seg.as_slice() {
                    ["_kiln", "work", "v1", rest @ ..] => route(&self.0, rest, req).await,
                    _ => Ok(msg(StatusCode::NOT_FOUND, "not found")),
                };
                r.unwrap_or_else(|(s, m)| msg(s, &m))
            }
        }
        crate::ci::integrations::alibaba::tests::serve(Arc::new(Work(hub)))
    }

    /// A step's unit as a runner sends it (W24-R30), told apart by its definition. It says it's
    /// trusted: the server keys it in its leg's domain whatever it says (W24-R21).
    fn step_unit(definition: &str) -> Value {
        use kiln_ci::work::key::{Kind, Platform};
        let u = Unit { kind: Kind::Step, definition: definition.into(), secrets: Default::default(), env: BTreeMap::new(), platform: Platform::default(), inputs: BTreeMap::new(), upstream: BTreeMap::new(), trust: "trusted".into() };
        serde_json::to_value(u).unwrap_or_default()
    }

    async fn post(base: &str, token: &str, what: &str, body: &Value) -> anyhow::Result<(u16, Value)> {
        let c = crate::http::client::Client::public()?;
        let resp = c.post(&format!("{base}/_kiln/work/v1/{what}")).bearer(token).json(body).timeout(std::time::Duration::from_secs(30)).send().await?;
        let status = resp.status();
        Ok((status, resp.json(1 << 20).await.unwrap_or(Value::Null)))
    }

    /// W24-R07, R21: a step's success is kept in its leg's scope and found for the same unit;
    /// another domain, another repository or another unit finds nothing; a lookup takes the source
    /// step's log over; an untrusted leg can't say it's trusted, nor can its unit.
    #[test]
    fn w24_r07_steps_are_kept_and_found_in_their_scope() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let first = leg(&hub, "acme/app", "run1", "leg1", true).await?;
            let second = leg(&hub, "acme/app", "run2", "leg2", true).await?;
            let untrusted = leg(&hub, "acme/app", "run3", "leg3", false).await?;
            let other = leg(&hub, "acme/other", "run4", "leg4", true).await?;
            let base = serve(Arc::clone(&hub))?;
            hub.logs.append("leg1", 2, b"built 1\r\n");
            let unit = step_unit("ab");
            let outputs = Replay { outputs: BTreeMap::from([("v".to_owned(), "1".to_owned())]), ..Replay::default() }.encode();
            let (st, v) = post(&base, &first, "lookup", &json!({ "unit": unit, "step": 2 })).await?;
            anyhow::ensure!(st == 204, "nothing yet: {st} {v}");
            let (st, v) = post(&base, &first, "record", &json!({ "unit": unit, "step": 2, "ms": 1500, "outputs": outputs })).await?;
            anyhow::ensure!(st == 201 && v.get("key").and_then(Value::as_str).is_some_and(|k| k.len() == 64), "{st} {v}");
            let (st, v) = post(&base, &second, "lookup", &json!({ "unit": unit, "step": 0 })).await?;
            anyhow::ensure!(st == 200, "{st} {v}");
            anyhow::ensure!(v.pointer("/source/run") == Some(&json!("run1")) && v.pointer("/source/number") == Some(&json!(1)) && v.pointer("/source/step") == Some(&json!(2)) && v.pointer("/source/ms") == Some(&json!(1500)), "{v}");
            anyhow::ensure!(v.pointer("/outputs/output.v") == Some(&json!("1")), "{v}");
            let log = hub.logs.read("leg2", 0).await;
            anyhow::ensure!(log == b"built 1\r\n", "the source's log: {}", String::from_utf8_lossy(&log));
            for (who, t) in [("an untrusted leg", &untrusted), ("another repository", &other)] {
                let (st, v) = post(&base, t, "lookup", &json!({ "unit": unit, "step": 0 })).await?;
                anyhow::ensure!(st == 204, "{who}: {st} {v}");
            }
            let (st, _) = post(&base, &second, "lookup", &json!({ "unit": step_unit("cd"), "step": 0 })).await?;
            anyhow::ensure!(st == 204, "another key");
            // An untrusted leg's record lands in its own domain, whatever it says.
            let (st, v) = post(&base, &untrusted, "record", &json!({ "unit": step_unit("ef"), "step": 0, "outputs": {}, "trusted": true })).await?;
            anyhow::ensure!(st == 201, "{st} {v}");
            let (st, _) = post(&base, &second, "lookup", &json!({ "unit": step_unit("ef"), "step": 0 })).await?;
            anyhow::ensure!(st == 204, "a trusted leg never reads an untrusted result");
            let (st, _) = post(&base, &untrusted, "lookup", &json!({ "unit": step_unit("ef"), "step": 1 })).await?;
            anyhow::ensure!(st == 200, "an untrusted leg reads its own domain's");
            // A tree that isn't stored can't be recorded; a bad token gets nothing at all.
            let files = Replay { files: Some(memo::Digest::of(b"no such tree")), ..Replay::default() }.encode();
            let (st, v) = post(&base, &first, "record", &json!({ "unit": step_unit("12"), "step": 1, "outputs": files })).await?;
            anyhow::ensure!(st == 400, "{st} {v}");
            let (st, _) = post(&base, "not-a-token", "lookup", &json!({ "unit": unit, "step": 0 })).await?;
            anyhow::ensure!(st == 401, "{st}");
            Ok(())
        })
    }

    async fn put(base: &str, token: &str, digest: &str, body: Vec<u8>) -> anyhow::Result<u16> {
        let c = crate::http::client::Client::public()?;
        Ok(c.put(&format!("{base}/_kiln/work/v1/blobs/{digest}")).bearer(token).body(body).timeout(Duration::from_secs(30)).send().await?.status())
    }

    async fn get(base: &str, token: &str, digest: &str) -> anyhow::Result<(u16, Vec<u8>)> {
        let c = crate::http::client::Client::public()?;
        let resp = c.get(&format!("{base}/_kiln/work/v1/blobs/{digest}")).bearer(token).timeout(Duration::from_secs(30)).send().await?;
        let status = resp.status();
        Ok((status, resp.bytes(1 << 20).await?.to_vec()))
    }

    /// W24-R07: a step's files go up as a tree and the files in it, only what its scope lacks, each
    /// checked against its digest; its result is kept once they're all there, and they read back in
    /// that scope alone. They're kept as long as the result is reusable (the longest `max_age` of
    /// those that refer to them); then a lookup no longer finds it and the upkeep lets them go.
    #[test]
    fn w24_r07_a_steps_files_are_kept_while_its_result_is_reusable() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let first = leg(&hub, "acme/app", "run1", "leg1", true).await?;
            let second = leg(&hub, "acme/app", "run2", "leg2", true).await?;
            let other = leg(&hub, "acme/other", "run3", "leg3", true).await?;
            let base = serve(Arc::clone(&hub))?;
            let (bin, script) = (b"built\n".to_vec(), b"#!/bin/sh\n".to_vec());
            let entries = [memo::Entry::File { path: "out/bin".into(), digest: Digest::of(&bin), executable: false }, memo::Entry::File { path: "out/run.sh".into(), digest: Digest::of(&script), executable: true }];
            let tree = serde_json::to_vec(&memo::tree(&entries).map_err(anyhow::Error::msg)?)?;
            let td = Digest::of(&tree);
            let objects = [(Digest::of(&bin), bin), (Digest::of(&script), script), (td.clone(), tree.clone())];
            let digests: Vec<&String> = objects.iter().map(|(d, _)| &d.hash).collect();
            let missing = |v: &Value| v.get("missing").and_then(Value::as_array).map_or(0, Vec::len);
            let (st, v) = post(&base, &first, "missing", &json!({ "digests": digests })).await?;
            anyhow::ensure!(st == 200 && missing(&v) == 3, "all three to send: {st} {v}");
            let files = Replay { outputs: BTreeMap::from([("made".to_owned(), "yes".to_owned())]), files: Some(td.clone()), ..Replay::default() }.encode();
            let (unit, step) = (step_unit("ab"), json!(0));
            let (st, v) = post(&base, &first, "record", &json!({ "unit": unit, "step": step, "outputs": files })).await?;
            anyhow::ensure!(st == 400, "its files aren't there yet: {st} {v}");
            anyhow::ensure!(put(&base, &first, &td.hash, b"not the tree".to_vec()).await? == 400, "a body that isn't its digest's");
            for (d, body) in &objects {
                anyhow::ensure!(put(&base, &first, &d.hash, body.clone()).await? == 200, "{}", d.hash);
            }
            let (st, v) = post(&base, &first, "record", &json!({ "unit": unit, "step": step, "max_age": 3600, "outputs": files })).await?;
            anyhow::ensure!(st == 201, "{st} {v}");
            // Kept in its scope: nothing more to send there; everything elsewhere, as if never seen.
            let (_, v) = post(&base, &second, "missing", &json!({ "digests": digests })).await?;
            anyhow::ensure!(missing(&v) == 0, "{v}");
            let (_, v) = post(&base, &other, "missing", &json!({ "digests": digests })).await?;
            anyhow::ensure!(missing(&v) == 3, "another repository is never told: {v}");
            anyhow::ensure!(get(&base, &second, &td.hash).await? == (200, tree.clone()), "its tree reads back in its scope");
            anyhow::ensure!(get(&base, &other, &td.hash).await?.0 == 404, "and nowhere else");
            let (st, v) = post(&base, &second, "lookup", &json!({ "unit": unit, "step": 0, "max_age": 3600 })).await?;
            anyhow::ensure!(st == 200 && v.pointer("/outputs/files") == Some(&json!(td.to_string())) && v.pointer("/outputs/output.made") == Some(&json!("yes")), "{st} {v}");
            // The longest `max_age` of the results that refer to a file keeps it.
            let (st, _) = post(&base, &first, "record", &json!({ "unit": step_unit("cd"), "step": 1, "max_age": 30 * 86_400, "outputs": files })).await?;
            anyhow::ensure!(st == 201, "{st}");
            let (st, _) = post(&base, &first, "record", &json!({ "unit": step_unit("ef"), "step": 2, "max_age": 60, "outputs": files })).await?;
            anyhow::ensure!(st == 201, "{st}");
            let due: Option<i64> = hub.store.call(|c| Ok(c.query_row("SELECT MIN(at) FROM work_records WHERE family = 'work.object'", [], |r| r.get(0))?)).await?;
            anyhow::ensure!(due.is_some_and(|at| at > crate::util::now_unix() + 29 * 86_400), "kept for the longest: {due:?}");
            // Once no result can be reused, the lookup finds nothing and the upkeep lets the files go.
            hub.store.call(|c| Ok(c.execute("UPDATE work_records SET at = 0 WHERE family = 'work.object'", [])?)).await?;
            let (st, _) = post(&base, &second, "lookup", &json!({ "unit": unit, "step": 0 })).await?;
            anyhow::ensure!(st == 204, "its files are due to go: {st}");
            anyhow::ensure!(sweep(&hub).await? == 3, "the tree and its two files");
            let (_, v) = post(&base, &second, "missing", &json!({ "digests": digests })).await?;
            anyhow::ensure!(missing(&v) == 3, "{v}");
            anyhow::ensure!(get(&base, &second, &td.hash).await?.0 == 404, "no longer the scope's");
            Ok(())
        })
    }
}
