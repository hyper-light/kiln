//! The land queue (docs/ci.md W14-R07, R08; by component, W28-R100 to R109). "Land D3" lands D3
//! and every diff below it, in order, each on the version Review's gate cleared; the strategy is
//! chosen then (`stack`: one commit each, `stack-squash`: one for all, `ff-only`: the trunk moved
//! to the stack as it is), within `land.strategies`. Each step of a trunk's queue ([`step`])
//! carries out what `kiln_ci::land::queue::plan` decides:
//!
//! 1. Review's gate (`review::readiness`, W28-R104) is asked first: held and returned entries it
//!    clears join the queue again, their approved revisions pinned anew (R105), a stack's clear
//!    prefix ahead of the rest (R106); a queued entry whose pin a newer revision invalidated holds.
//! 2. The entries queued and under test make a conflict graph over the repository's units: the
//!    trunk's package graph and each entry's own (R100). Each component has a candidate of its own,
//!    tested beside every other component's: its first entries, as many as its window lets (R101),
//!    applied onto the trunk's tip without a working tree (`git merge-tree`; a temporary worktree
//!    where git is older than 2.40), each landed commit with `Differential-Revision:` and
//!    `Reviewed-by:` trailers. An entry that conflicts with the trunk goes back to Review (R107).
//! 3. Each candidate is kept at `refs/kiln/land/<trunk>/<batch>` and starts its `land` and
//!    `merge_group` workflows; a required check (`land.checks`) none of them runs ejects its
//!    entries.
//! 4. A candidate that passed lands once Review's gate, asked again, clears each of its entries: the
//!    trunk moves to it in one compare-and-swap update, or, when the trunk moved since only for
//!    changes it shares nothing with, to its entries applied again onto the tip, its result standing
//!    (R102, R103). A failure ejects a lone entry with its reason, or is bisected inside its
//!    component.
//! 5. The landed diffs are marked, said (`pull_request` `closed`), recorded (R108), and the open
//!    diffs above them restacked onto the new trunk as rebase-only versions.
//!
//! A repository whose host has a queue of its own lands there, through the forge seam (R109).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use http::StatusCode;
use kiln_ci::land::conflict::{Change, Units};
use kiln_ci::land::queue::{self, Candidate, Gate, Item, Outcome, Pin, Step};
use kiln_ci::land::{self, Strategy};
use kiln_ci::review::State;
use kiln_ci::trigger::{Event, Source};
use kiln_seams::forge::{self as seam, Forge};
use rusqlite::{OptionalExtension as _, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::Fail;
use super::diffs::{self, Diff};
use crate::authz::Caller;
use crate::gitops::{self, LS_REMOTE_TIMEOUT};
use crate::hub::Hub;
use crate::store::Repo;

fn fail(e: anyhow::Error) -> Fail {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}

fn kicker() -> &'static crate::rt::Notify {
    static KICK: OnceLock<crate::rt::Notify> = OnceLock::new();
    KICK.get_or_init(crate::rt::Notify::new)
}

/// Wakes the queue: an entry came, a run finished.
pub fn kick() {
    kicker().notify();
}

/// How often an entry that waits on Review is asked again, at most, unless a review or Review's
/// word asks it at once ([`recheck`]).
const RECHECK: Duration = Duration::from_secs(10);

/// When each entry that waits on Review (or on its host's queue) was last asked about.
fn rechecked() -> &'static parking_lot::Mutex<HashMap<String, Instant>> {
    static R: OnceLock<parking_lot::Mutex<HashMap<String, Instant>>> = OnceLock::new();
    R.get_or_init(Default::default)
}

fn due(key: &str, every: Duration) -> bool {
    rechecked().lock().get(key).is_none_or(|t| t.elapsed() >= every)
}

/// What waits on Review asked again at once (docs/ci.md W28-R104): a review came, or Review's word.
pub fn recheck() {
    rechecked().lock().clear();
    kick();
}

/// The queue, for as long as the server runs: each trunk with entries, every few seconds or when
/// kicked.
pub async fn run_loop(hub: Arc<Hub>) -> anyhow::Result<()> {
    use futures::FutureExt as _;
    loop {
        if let Err(e) = tick(&hub).await {
            tracing::warn!(error = %format!("{e:#}"), "the land queue stumbled");
        }
        futures::select_biased! {
            () = hub.shutdown.cancelled().fuse() => return Ok(()),
            () = kicker().notified().fuse() => {}
            () = crate::rt::sleep(Duration::from_secs(3)).fuse() => {}
        }
    }
}

/// An entry of the queue, as kept: `queued`, `testing`, `held` (by Review's gate or Review),
/// `returned` (to Review, for a conflict with the trunk), or done: `landed`, `ejected`,
/// `cancelled`, `withdrawn` (by Review).
#[derive(Debug, Clone)]
struct Entry {
    id: String,
    repo_id: String,
    trunk: String,
    diffs: Vec<String>,
    strategy: Strategy,
    state: String,
    batch: Option<String>,
    base: Option<String>,
    candidate: Option<String>,
    /// The workflows started on the candidate (their paths).
    started: Vec<String>,
    /// The commit that carries each diff on the candidate.
    commits: BTreeMap<String, String>,
    enqueued_by: String,
    /// Who landed it, which its candidate's runs are held to (W10-R02's `secrets.use`).
    started_by: Option<Value>,
    /// Its component's window as last kept (docs/ci.md W28-R101), and a bisection's bound on its
    /// component's next candidate, kept on the component's head (R102).
    window: Option<u32>,
    bisect: Option<u32>,
    /// The revisions it lands, bottom first: each diff's approved version, by its commit and
    /// patch-id (R105).
    pinned: Vec<Pin>,
    /// Who holds it, `gate` (Review's gate) or `review` (Review did), and why (R104).
    hold: Option<String>,
    reason: Option<String>,
    /// When it last joined the queue: its place there (R100).
    queued_at: i64,
    /// Which queue lands it, `kiln` or its host's (R109); there, its changes.
    route: Option<String>,
    host: Option<HostEntry>,
}

fn entry_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Entry> {
    let diffs: String = r.get(1)?;
    let strategy: String = r.get(2)?;
    let text = |i: usize| -> rusqlite::Result<Option<String>> { r.get::<_, Option<String>>(i) };
    let number = |i: usize| -> rusqlite::Result<Option<u32>> { Ok(r.get::<_, Option<i64>>(i)?.and_then(|n| u32::try_from(n).ok())) };
    Ok(Entry {
        id: r.get(0)?,
        diffs: serde_json::from_str(&diffs).unwrap_or_default(),
        strategy: Strategy::parse(&strategy).unwrap_or(Strategy::Stack),
        state: r.get(3)?,
        batch: r.get(4)?,
        base: r.get(5)?,
        candidate: r.get(6)?,
        started: text(7)?.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default(),
        commits: text(8)?.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default(),
        enqueued_by: r.get(9)?,
        started_by: text(10)?.and_then(|t| serde_json::from_str(&t).ok()),
        window: number(11)?,
        bisect: number(12)?,
        pinned: text(13)?.and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default(),
        hold: r.get(14)?,
        reason: r.get(15)?,
        queued_at: r.get(16)?,
        route: r.get(17)?,
        host: text(18)?.and_then(|t| serde_json::from_str(&t).ok()),
        repo_id: r.get(19)?,
        trunk: r.get(20)?,
    })
}

const ENTRY_COLUMNS: &str = "id, diffs, strategy, state, batch, base, candidate, checks, commits, enqueued_by, started_by, window_size, bisect, pinned, hold, reason, COALESCE(queued_at, created_at), route, host, repo_id, trunk";

/// A trunk's entries in `states`, in queue order: when each last joined the queue (W28-R100).
async fn entries(hub: &Hub, repo_id: &str, trunk: &str, states: &'static [&'static str]) -> anyhow::Result<Vec<Entry>> {
    let (repo_id, trunk) = (repo_id.to_owned(), trunk.to_owned());
    hub.store
        .call(move |c| {
            let placeholders = states.iter().map(|s| format!("'{s}'")).collect::<Vec<_>>().join(", ");
            let sql = format!("SELECT {ENTRY_COLUMNS} FROM land_queue WHERE repo_id = ?1 AND trunk = ?2 AND state IN ({placeholders}) ORDER BY COALESCE(queued_at, created_at), created_at, rowid");
            let mut st = c.prepare(&sql)?;
            let rows = st.query_map(params![repo_id, trunk], entry_row)?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
}

async fn entry_by_id(hub: &Hub, id: &str) -> anyhow::Result<Option<Entry>> {
    let key = id.to_owned();
    hub.store.call(move |c| Ok(c.query_row(&format!("SELECT {ENTRY_COLUMNS} FROM land_queue WHERE id = ?1"), [&key], entry_row).optional()?)).await
}

/// The entry that holds diff `diff_id` while it's live (queued, under test, held or returned).
async fn live_of(hub: &Hub, diff_id: &str) -> anyhow::Result<Option<Entry>> {
    let id = diff_id.to_owned();
    hub.store
        .call(move |c| {
            let sql = format!("SELECT {ENTRY_COLUMNS} FROM land_queue q WHERE q.state IN ('queued', 'testing', 'held', 'returned') AND EXISTS (SELECT 1 FROM json_each(q.diffs) WHERE value = ?1) ORDER BY q.created_at DESC LIMIT 1");
            Ok(c.query_row(&sql, [&id], entry_row).optional()?)
        })
        .await
}

/// The live entry that holds diff `diff_id` (docs/ci.md W28-R104): its id, state and reason, as
/// Review shows them.
pub(crate) async fn entry_of(hub: &Hub, diff_id: &str) -> Option<(String, String, Option<String>)> {
    live_of(hub, diff_id).await.ok().flatten().map(|e| (e.id, e.state, e.reason))
}

/// An entry to keep (docs/ci.md W28-R100 to R109).
struct New {
    repo_id: String,
    trunk: String,
    diffs: Vec<String>,
    strategy: Strategy,
    by: String,
    started_by: Option<String>,
    state: &'static str,
    hold: Option<String>,
    reason: Option<String>,
    conflicts: Vec<String>,
    pinned: Vec<Pin>,
    route: String,
    host: Option<HostEntry>,
    queued_at: i64,
}

async fn insert(hub: &Hub, n: New) -> anyhow::Result<String> {
    let id = crate::util::new_id();
    let key = id.clone();
    hub.store
        .call(move |c| {
            let now = crate::util::now_unix();
            let diffs = serde_json::to_string(&n.diffs)?;
            let pinned = serde_json::to_string(&n.pinned)?;
            let conflicts = if n.conflicts.is_empty() { None } else { Some(serde_json::to_string(&n.conflicts)?) };
            let host = match &n.host {
                Some(h) => Some(serde_json::to_string(h)?),
                None => None,
            };
            c.execute(
                "INSERT INTO land_queue (id, repo_id, trunk, diffs, strategy, state, enqueued_by, created_at, updated_at, started_by, hold, reason, conflicts, pinned, route, host, queued_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
                params![key, n.repo_id, n.trunk, diffs, n.strategy.as_str(), n.state, n.by, now, n.started_by, n.hold, n.reason, conflicts, pinned, n.route, host, n.queued_at],
            )?;
            Ok(())
        })
        .await?;
    Ok(id)
}

/// Sets each of `ids`' diffs' state.
async fn set_diff_state(hub: &Hub, ids: &[String], state: State) -> anyhow::Result<()> {
    let ids = ids.to_vec();
    hub.store
        .call(move |c| {
            let now = crate::util::now_unix();
            for id in &ids {
                c.execute("UPDATE diffs SET state = ?2, updated_at = ?3 WHERE id = ?1", params![id, state.as_str(), now])?;
            }
            Ok(())
        })
        .await
}

/// Diffs the queue gives back to Review (docs/ci.md W28-R104, R107): each that was landing in the
/// state its reviews give it now.
async fn to_review(hub: &Hub, ids: &[String]) -> anyhow::Result<()> {
    for id in ids {
        let Some(d) = diffs::find(hub, id).await? else { continue };
        if d.state != State::Landing.as_str() {
            continue;
        }
        let now = super::review::state_now(hub, &d).await.unwrap_or(State::Accepted);
        set_diff_state(hub, std::slice::from_ref(id), now).await?;
    }
    Ok(())
}

/// An entry's diffs, bottom first (those gone left out).
async fn diffs_of(hub: &Hub, e: &Entry) -> anyhow::Result<Vec<Diff>> {
    let mut out = Vec::new();
    for id in &e.diffs {
        if let Some(d) = diffs::find(hub, id).await? {
            out.push(d);
        }
    }
    Ok(out)
}

/// Says where an entry is (`queue`: `queued`, `testing`, `held`, `returned`, `landed`, `ejected`,
/// `cancelled`, `withdrawn`), with its diffs and whose they are, for whoever follows them.
async fn tell(hub: &Hub, e: &Entry, state: &str, reason: Option<&str>) {
    let mut list = Vec::new();
    for id in &e.diffs {
        if let Ok(Some(d)) = diffs::find(hub, id).await {
            list.push(crate::wire::events::LandDiff { diff: d.name, repo: d.repo, author: d.author, author_for: d.author_for });
        }
    }
    hub.emit(&crate::wire::events::ServerEvent::Queue(crate::wire::events::Queue::Entry(crate::wire::events::LandEntry {
        entry: e.id.clone(),
        state: state.to_owned(),
        reason: reason.map(str::to_owned),
        strategy: e.strategy.as_str().to_owned(),
        diffs: list,
        enqueued_by: e.enqueued_by.clone(),
    })));
}

/// GitHub's merge queue events for diffs (W03-R02's native table): `pull_request` `enqueued`, or
/// `dequeued` with GitHub's `reason` (`MERGE`, `MANUAL`, `CI_FAILURE`, `INVALID_MERGE_COMMIT`,
/// `BRANCH_PROTECTIONS`, `UNKNOWN_REMOVAL_REASON`), sent by `by`.
async fn queue_event(hub: &Hub, ids: &[String], action: &str, reason: Option<&str>, by: &str) {
    for id in ids {
        let Ok(Some(d)) = diffs::find(hub, id).await else { continue };
        let Some(repo) = hub.store.repos().await.ok().and_then(|r| r.into_iter().find(|r| r.name == d.repo)) else { continue };
        if let Err(err) = diffs::activity(hub, &repo, &d, action, by, |p| {
            if let Some(r) = reason {
                p.insert("reason".into(), json!(r));
            }
        })
        .await
        {
            tracing::warn!(diff = %d.name, %action, error = %format!("{err:#}"), "its workflows didn't start");
        }
    }
}

/// Takes an entry out of the queue, saying why (`code`: GitHub's reason, for `dequeued`); its
/// diffs go back to their review state.
async fn eject(hub: &Arc<Hub>, e: &Entry, reason: &str, code: &str) -> anyhow::Result<()> {
    let (id, why) = (e.id.clone(), reason.to_owned());
    hub.store.call(move |c| Ok(c.execute("UPDATE land_queue SET state = 'ejected', reason = ?2, hold = NULL, updated_at = ?3 WHERE id = ?1", params![id, why, crate::util::now_unix()]).map(|_| ())?)).await?;
    set_diff_state(hub, &e.diffs, State::Accepted).await?;
    hub.store.audit("kiln".into(), "queue.eject".into(), format!("{}: {reason}", e.id)).await;
    tell(hub, e, "ejected", Some(reason)).await;
    queue_event(hub, &e.diffs, "dequeued", Some(code), "kiln").await;
    crate::rt::spawn(super::notify::land_ejected(Arc::clone(hub), e.diffs.clone(), reason.to_owned()));
    tracing::info!(entry = %e.id, %reason, "ejected from the land queue");
    Ok(())
}

// ------------------------------------------------------------------ Review's gate

/// What holds a stack's diffs, in words: `D3: 0 of 1 accepts; D4: D3, below it, holds`.
fn said(held: &[(String, Vec<String>)]) -> String {
    held.iter().flat_map(|(d, why)| why.iter().map(move |w| format!("{d}: {w}"))).collect::<Vec<_>>().join("; ")
}

/// A diff's versions as pins read them (docs/ci.md W28-R105): each with its patch-id, and whether
/// it only rebased the one before it.
async fn revisions(hub: &Hub, diff_id: &str) -> anyhow::Result<Vec<queue::Revision>> {
    let id = diff_id.to_owned();
    hub.store
        .call(move |c| {
            let mut st = c.prepare("SELECT n, commit_sha, parent, patch_id, rebase_only FROM diff_versions WHERE diff_id = ?1 ORDER BY n")?;
            let rows = st.query_map([&id], |r| Ok(queue::Revision { version: r.get(0)?, commit: r.get(1)?, parent: r.get(2)?, patch_id: r.get(3)?, rebase_only: r.get(4)? }))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
}

/// Each diff's latest version, pinned (docs/ci.md W28-R105): what Review's gate cleared, by its
/// commit, the commit below it and its patch-id. Bottom first.
async fn pins_of(hub: &Hub, ds: &[Diff]) -> anyhow::Result<Vec<Pin>> {
    let mut out = Vec::new();
    for d in ds {
        let r = revisions(hub, &d.id).await?.pop().ok_or_else(|| anyhow::anyhow!("{} has no version", d.name))?;
        out.push(Pin { diff: d.name.clone(), version: r.version, commit: r.commit, parent: r.parent, patch_id: r.patch_id });
    }
    Ok(out)
}

/// What Review's gate says of an entry's diffs now (docs/ci.md W28-R104), bottom first, each on top
/// of the open diffs below it; and, for each diff whose pin a newer revision invalidated, why
/// (R105).
async fn gate_entry(hub: &Hub, e: &Entry, ds: &[Diff]) -> anyhow::Result<(Vec<(String, Gate)>, BTreeMap<String, String>)> {
    let Some(top) = ds.last() else { return Ok((Vec::new(), BTreeMap::new())) };
    let chain = stack_below(hub, top).await.map_err(|(_, m)| anyhow::anyhow!(m))?;
    let all = super::review::readiness_of(hub, &chain).await.map_err(|(_, m)| anyhow::anyhow!(m))?;
    let mut gates = Vec::new();
    let mut fails = BTreeMap::new();
    for d in ds {
        let g = chain.iter().position(|c| c.id == d.id).and_then(|i| all.get(i)).map(|r| r.gate.clone()).unwrap_or_else(|| Gate::Hold { why: vec!["it isn't open any more".into()] });
        gates.push((d.name.clone(), g));
        if let Some(p) = e.pinned.iter().find(|p| p.diff == d.name)
            && let Err(why) = queue::pin_holds(p, &revisions(hub, &d.id).await?)
        {
            fails.insert(d.name.clone(), why);
        }
    }
    Ok((gates, fails))
}

/// `g`, holding for `why` too (a pin a newer revision invalidated: W28-R105).
fn holding(g: Gate, why: &str) -> Gate {
    match g {
        Gate::Ready => Gate::Hold { why: vec![why.to_owned()] },
        Gate::Hold { why: mut rest } => {
            rest.insert(0, why.to_owned());
            Gate::Hold { why: rest }
        }
        conflict => conflict,
    }
}

/// The files of the first diff that isn't clear when it's a conflict with the trunk (W28-R107).
fn first_conflict(gates: &[(String, Gate)]) -> Option<Vec<String>> {
    match gates.iter().map(|(_, g)| g).find(|g| !g.ready()) {
        Some(Gate::Conflict { files }) => Some(files.clone()),
        _ => None,
    }
}

/// An entry held (docs/ci.md W28-R104): by Review's gate (`who`: `gate`), its blockers the reason,
/// or by Review (`review`). Out of any candidate; its diffs back with Review (`code`: GitHub's
/// reason for `dequeued`).
async fn hold_entry(hub: &Arc<Hub>, e: &Entry, who: &str, why: &str, code: &str) -> anyhow::Result<()> {
    let (id, who2, why2) = (e.id.clone(), who.to_owned(), why.to_owned());
    hub.store
        .call(move |c| {
            c.execute(
                "UPDATE land_queue SET state = 'held', hold = ?2, reason = ?3, batch = NULL, base = NULL, candidate = NULL, checks = NULL, commits = NULL, conflicts = NULL, updated_at = ?4 WHERE id = ?1",
                params![id, who2, why2, crate::util::now_unix()],
            )?;
            Ok(())
        })
        .await?;
    if matches!(e.state.as_str(), "queued" | "testing") {
        to_review(hub, &e.diffs).await?;
        queue_event(hub, &e.diffs, "dequeued", Some(code), "kiln").await;
    }
    hub.store.audit("kiln".into(), "queue.hold".into(), format!("{} ({who}): {why}", e.id)).await;
    tell(hub, e, "held", Some(why)).await;
    tracing::info!(entry = %e.id, %who, %why, "a land queue entry holds");
    Ok(())
}

/// An entry that conflicts with the trunk goes back to Review (docs/ci.md W28-R107), not out:
/// `returned`, its diffs in their review state, the files kept. It joins the queue again once
/// Review's gate clears it: a version that applies, approved.
async fn return_to_review(hub: &Arc<Hub>, e: &Entry, files: &[String]) -> anyhow::Result<()> {
    let reason = queue::conflicts_in(files);
    let (id, why, list) = (e.id.clone(), reason.clone(), serde_json::to_string(files)?);
    hub.store
        .call(move |c| {
            c.execute(
                "UPDATE land_queue SET state = 'returned', hold = NULL, reason = ?2, conflicts = ?3, batch = NULL, base = NULL, candidate = NULL, checks = NULL, commits = NULL, updated_at = ?4 WHERE id = ?1",
                params![id, why, list, crate::util::now_unix()],
            )?;
            Ok(())
        })
        .await?;
    if matches!(e.state.as_str(), "queued" | "testing") {
        to_review(hub, &e.diffs).await?;
        queue_event(hub, &e.diffs, "dequeued", Some("INVALID_MERGE_COMMIT"), "kiln").await;
        crate::rt::spawn(super::notify::land_ejected(Arc::clone(hub), e.diffs.clone(), reason.clone()));
    }
    hub.store.audit("kiln".into(), "queue.return".into(), format!("{}: {reason}", e.id)).await;
    tell(hub, e, "returned", Some(reason.as_str())).await;
    tracing::info!(entry = %e.id, %reason, "back to Review from the land queue");
    Ok(())
}

/// An entry Review's gate clears joins the queue again (docs/ci.md W28-R104, R107): at its end,
/// its diffs' approved versions pinned anew (R105).
async fn rejoin(hub: &Arc<Hub>, e: &Entry, ds: &[Diff]) -> anyhow::Result<()> {
    let pins = serde_json::to_string(&pins_of(hub, ds).await?)?;
    let ids: Vec<String> = ds.iter().map(|d| d.id.clone()).collect();
    let (id, list) = (e.id.clone(), serde_json::to_string(&ids)?);
    let changed = hub
        .store
        .call(move |c| {
            let now = crate::util::now_unix();
            Ok(c.execute(
                "UPDATE land_queue SET state = 'queued', hold = NULL, reason = NULL, conflicts = NULL, diffs = ?2, pinned = ?3, queued_at = ?4, updated_at = ?4 WHERE id = ?1 AND state IN ('held', 'returned')",
                params![id, list, pins, now],
            )?)
        })
        .await?;
    if changed == 0 {
        return Ok(());
    }
    set_diff_state(hub, &ids, State::Landing).await?;
    hub.store.audit("kiln".into(), "queue.rejoin".into(), format!("{}: Review's gate cleared it", e.id)).await;
    tell(hub, e, "queued", None).await;
    queue_event(hub, &ids, "enqueued", None, &e.enqueued_by).await;
    kick();
    Ok(())
}

/// A stack's entry split at its clear prefix (docs/ci.md W28-R106): it keeps its first `k` diffs
/// and their pins; the rest go to an entry of their own, `state` (`held` by Review's gate, or
/// `returned` for a conflict), saying why. That entry's id.
async fn split_off(hub: &Arc<Hub>, e: &Entry, k: usize, state: &'static str, why: &str, conflicts: &[String]) -> anyhow::Result<String> {
    let keep: Vec<String> = e.diffs.iter().take(k).cloned().collect();
    let rest: Vec<String> = e.diffs.iter().skip(k).cloned().collect();
    let (keep_pins, rest_pins): (Vec<Pin>, Vec<Pin>) = (e.pinned.iter().take(k).cloned().collect(), e.pinned.iter().skip(k).cloned().collect());
    let (id, kept, pins) = (e.id.clone(), serde_json::to_string(&keep)?, serde_json::to_string(&keep_pins)?);
    hub.store.call(move |c| Ok(c.execute("UPDATE land_queue SET diffs = ?2, pinned = ?3, updated_at = ?4 WHERE id = ?1", params![id, kept, pins, crate::util::now_unix()]).map(|_| ())?)).await?;
    let new = insert(
        hub,
        New {
            repo_id: e.repo_id.clone(),
            trunk: e.trunk.clone(),
            diffs: rest.clone(),
            strategy: e.strategy,
            by: e.enqueued_by.clone(),
            started_by: e.started_by.as_ref().map(Value::to_string),
            state,
            hold: (state == "held").then(|| "gate".to_owned()),
            reason: Some(why.to_owned()),
            conflicts: conflicts.to_vec(),
            pinned: rest_pins,
            route: e.route.clone().unwrap_or_else(|| "kiln".into()),
            host: None,
            queued_at: e.queued_at,
        },
    )
    .await?;
    if matches!(e.state.as_str(), "queued" | "testing") {
        to_review(hub, &rest).await?;
        queue_event(hub, &rest, "dequeued", Some(if state == "returned" { "INVALID_MERGE_COMMIT" } else { "BRANCH_PROTECTIONS" }), "kiln").await;
    }
    hub.store.audit("kiln".into(), "queue.split".into(), format!("{}: its last {} diffs into {new}, {state}: {why}", e.id, rest.len())).await;
    if let Some(n) = entry_by_id(hub, &new).await? {
        tell(hub, &n, state, Some(why)).await;
    }
    Ok(new)
}

/// An entry Review's gate doesn't clear (docs/ci.md W28-R104 to R107), each diff's pin failing
/// holding it first: with no clear prefix it's returned to Review for a conflict, else held; with
/// one, the prefix keeps the entry, back in the queue (`again`), and the rest goes on its own. Whether
/// the entry kept a prefix.
async fn not_cleared(hub: &Arc<Hub>, e: &Entry, gates: Vec<(String, Gate)>, fails: &BTreeMap<String, String>) -> anyhow::Result<bool> {
    let gates: Vec<(String, Gate)> = gates.into_iter().map(|(d, g)| if let Some(why) = fails.get(&d) { (d.clone(), holding(g, why)) } else { (d, g) }).collect();
    let p = queue::prefix(&gates);
    let conflict = first_conflict(&gates);
    let why = said(&p.held);
    if p.clear.is_empty() {
        match conflict {
            Some(files) => return_to_review(hub, e, &files).await?,
            None => hold_entry(hub, e, "gate", &why, "BRANCH_PROTECTIONS").await?,
        }
        return Ok(false);
    }
    let files = conflict.unwrap_or_default();
    split_off(hub, e, p.clear.len(), if files.is_empty() { "held" } else { "returned" }, &why, &files).await?;
    Ok(true)
}

/// A held or returned entry's state and reason, updated when Review's gate says otherwise now.
async fn wait_on(hub: &Arc<Hub>, e: &Entry, state: &'static str, why: &str, files: &[String]) -> anyhow::Result<()> {
    if e.state == state && e.reason.as_deref() == Some(why) {
        return Ok(());
    }
    let (id, hold, why2, list) = (e.id.clone(), (state == "held").then_some("gate"), why.to_owned(), (!files.is_empty()).then(|| serde_json::to_string(files)).transpose()?);
    hub.store.call(move |c| Ok(c.execute("UPDATE land_queue SET state = ?2, hold = ?3, reason = ?4, conflicts = ?5, updated_at = ?6 WHERE id = ?1", params![id, state, hold, why2, list, crate::util::now_unix()]).map(|_| ())?)).await?;
    tell(hub, e, state, Some(why)).await;
    Ok(())
}

/// Why the diff below an entry's bottom keeps it waiting (docs/ci.md W28-R106): it isn't landed,
/// and no entry ahead of it in the queue lands it.
async fn below(hub: &Hub, ds: &[Diff]) -> anyhow::Result<Option<String>> {
    let Some(name) = ds.first().and_then(|d| d.parent.clone()) else { return Ok(None) };
    let Some(d) = diffs::find(hub, &name).await? else { return Ok(None) };
    if d.state == State::Landed.as_str() {
        return Ok(None);
    }
    Ok(match live_of(hub, &d.id).await? {
        Some(e) if matches!(e.state.as_str(), "queued" | "testing") => None,
        Some(e) => Some(format!("{name}, below it, waits on Review: {}", e.reason.unwrap_or_default())),
        None => Some(format!("{name}, below it, isn't in the land queue")),
    })
}

/// An entry that waits on Review, asked again (docs/ci.md W28-R104 to R107): once Review's gate
/// clears it, it joins the queue, re-pinned; a stack's clear prefix joins and the rest waits on.
/// Otherwise it says why it waits: a conflict with the trunk keeps it returned.
async fn recheck_entry(hub: &Arc<Hub>, e: &Entry) -> anyhow::Result<()> {
    rechecked().lock().insert(e.id.clone(), Instant::now());
    let all = diffs_of(hub, e).await?;
    if let Some(gone) = all.iter().find(|d| d.state == State::Abandoned.as_str()) {
        return finish(hub, e, "withdrawn", &format!("{} was abandoned", gone.name)).await;
    }
    let ds: Vec<Diff> = all.into_iter().filter(|d| d.state != State::Landed.as_str()).collect();
    if ds.is_empty() {
        return finish(hub, e, "cancelled", "its diffs landed otherwise").await;
    }
    // Diffs that landed otherwise, or are gone, leave it: what's left lines up with its pins.
    let kept = if ds.len() == e.diffs.len() {
        e.clone()
    } else {
        let ids: Vec<String> = ds.iter().map(|d| d.id.clone()).collect();
        let pinned: Vec<Pin> = e.pinned.iter().filter(|p| ds.iter().any(|d| d.name == p.diff)).cloned().collect();
        let (id, list, pins) = (e.id.clone(), serde_json::to_string(&ids)?, serde_json::to_string(&pinned)?);
        hub.store.call(move |c| Ok(c.execute("UPDATE land_queue SET diffs = ?2, pinned = ?3 WHERE id = ?1", params![id, list, pins]).map(|_| ())?)).await?;
        Entry { diffs: ids, pinned, ..e.clone() }
    };
    let e = &kept;
    if let Some(why) = below(hub, &ds).await? {
        return wait_on(hub, e, "held", &why, &[]).await;
    }
    let (gates, fails) = gate_entry(hub, e, &ds).await?;
    let p = queue::prefix(&gates);
    if p.held.is_empty() {
        return rejoin(hub, e, &ds).await;
    }
    let files = first_conflict(&gates).unwrap_or_default();
    let state = if files.is_empty() { "held" } else { "returned" };
    let pinned: Vec<String> = fails.iter().map(|(d, w)| format!("{d}: {w}")).collect();
    let why = pinned.into_iter().chain(std::iter::once(said(&p.held))).collect::<Vec<_>>().join("; ");
    let k = p.clear.len();
    if k == 0 {
        return wait_on(hub, e, state, &why, &files).await;
    }
    split_off(hub, e, k, state, &why, &files).await?;
    let kept: Vec<Diff> = ds.into_iter().take(k).collect();
    match entry_by_id(hub, &e.id).await? {
        Some(now) => rejoin(hub, &now, &kept).await,
        None => Ok(()),
    }
}

/// A live entry done without landing: `withdrawn` or `cancelled`, saying why; diffs the queue had
/// go back to Review.
async fn finish(hub: &Arc<Hub>, e: &Entry, state: &'static str, why: &str) -> anyhow::Result<()> {
    let (id, why2) = (e.id.clone(), why.to_owned());
    hub.store
        .call(move |c| {
            c.execute("UPDATE land_queue SET state = ?2, reason = ?3, hold = NULL, batch = NULL, base = NULL, candidate = NULL, updated_at = ?4 WHERE id = ?1", params![id, state, why2, crate::util::now_unix()])?;
            Ok(())
        })
        .await?;
    if matches!(e.state.as_str(), "queued" | "testing") {
        to_review(hub, &e.diffs).await?;
        queue_event(hub, &e.diffs, "dequeued", Some("MANUAL"), "kiln").await;
    }
    tell(hub, e, state, Some(why)).await;
    Ok(())
}

// ------------------------------------------------------------------ enqueueing

/// The open diffs `d` stands on, bottom first, and `d`.
pub(crate) async fn stack_below(hub: &Hub, d: &Diff) -> Result<Vec<Diff>, Fail> {
    let mut chain = vec![d.clone()];
    let mut at = d.parent.clone();
    while let Some(name) = at {
        let Some(below) = diffs::find(hub, &name).await.map_err(fail)? else { break };
        if below.state == State::Landed.as_str() {
            break;
        }
        if chain.iter().any(|x| x.id == below.id) || chain.len() > kiln_ci::review::MAX_STACK {
            break;
        }
        at = below.parent.clone();
        chain.push(below);
    }
    chain.reverse();
    Ok(chain)
}

/// `POST /api/diffs/:id/land` `{ strategy }`: the diff and those below it, into the queue.
pub async fn enqueue(hub: &Arc<Hub>, d: &Diff, caller: &Caller, body: &Value) -> Result<Value, Fail> {
    enqueue_as(hub, d, &caller.name, caller.started_by(), body).await
}

/// [`enqueue`] as `by` (the audit's name), its candidate's runs held to `started_by`. Review's gate
/// decides (docs/ci.md W28-R104): a stack's clear prefix lands, in order, its approved revisions
/// pinned (R105); the rest holds, each diff saying why (R106); with nothing clear, nothing goes,
/// and the answer says why. A diff that waits on Review in the queue already is asked again.
pub(crate) async fn enqueue_as(hub: &Arc<Hub>, d: &Diff, by: &str, started_by: Option<Value>, body: &Value) -> Result<Value, Fail> {
    let (repo, path) = diffs::home(hub, d).await?;
    let (policy, _) = super::review::policy(hub, &path, &d.trunk).await;
    let asked = match body.get("strategy").and_then(Value::as_str) {
        Some(s) => Some(Strategy::parse(s).ok_or_else(|| (StatusCode::BAD_REQUEST, format!("no strategy `{s}`: stack, stack-squash, squash, rebase, merge or ff-only")))?),
        None => None,
    };
    let strategy = policy.strategy(asked, d.kind != "diff").map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e))?;
    // A squash of a stack is the stack as one commit.
    let strategy = if strategy == Strategy::Squash && d.kind == "diff" { Strategy::StackSquash } else { strategy };
    if let Some(e) = live_of(hub, &d.id).await.map_err(fail)? {
        if matches!(e.state.as_str(), "held" | "returned") && e.hold.as_deref() != Some("review") {
            recheck_entry(hub, &e).await.map_err(fail)?;
            let now = entry_by_id(hub, &e.id).await.map_err(fail)?.ok_or((StatusCode::CONFLICT, format!("{} left the land queue", d.name)))?;
            if now.state == "queued" {
                return Ok(json!({ "entry": now.id, "diffs": names_of(hub, &now.diffs).await, "trunk": d.trunk, "strategy": now.strategy.as_str(), "state": "queued" }));
            }
            return Err((StatusCode::CONFLICT, format!("{} can't land yet: {}", d.name, now.reason.unwrap_or_default())));
        }
        let held = e.reason.filter(|_| e.state == "held").map(|r| format!(", held: {r}")).unwrap_or_default();
        return Err((StatusCode::CONFLICT, format!("{} can't land yet: {}: it's in the land queue{held}", d.name, d.name)));
    }
    // The stack from the first diff above those the queue has already, bottom first.
    let full = stack_below(hub, d).await?;
    let mut start = 0;
    for (i, x) in full.iter().enumerate() {
        if x.id != d.id && live_of(hub, &x.id).await.map_err(fail)?.is_some() {
            start = i + 1;
        }
    }
    let rs = super::review::readiness_of(hub, &full).await?;
    let gates: Vec<(String, Gate)> = full.iter().zip(&rs).skip(start).map(|(x, r)| (x.name.clone(), r.gate.clone())).collect();
    let p = queue::prefix(&gates);
    if p.clear.is_empty() {
        return Err((StatusCode::CONFLICT, format!("{} can't land yet: {}", d.name, said(&p.held))));
    }
    let chain: Vec<Diff> = full.into_iter().skip(start).collect();
    let clear: Vec<Diff> = chain.iter().take(p.clear.len()).cloned().collect();
    let rest: Vec<Diff> = chain.iter().skip(p.clear.len()).cloned().collect();
    let ids: Vec<String> = clear.iter().map(|x| x.id.clone()).collect();
    let names: Vec<String> = clear.iter().map(|x| x.name.clone()).collect();
    let now = crate::util::now_unix();
    let started = started_by.map(|v| v.to_string());
    let new = |diffs: Vec<String>, state: &'static str, hold: Option<String>, reason: Option<String>, pinned: Vec<Pin>| New {
        repo_id: repo.id.clone(),
        trunk: d.trunk.clone(),
        diffs,
        strategy,
        by: by.to_owned(),
        started_by: started.clone(),
        state,
        hold,
        reason,
        conflicts: Vec::new(),
        pinned,
        route: queue::Route::Kiln.name().to_owned(),
        host: None,
        queued_at: now,
    };
    let pins = pins_of(hub, &clear).await.map_err(fail)?;
    let entry = insert(hub, new(ids.clone(), "queued", None, None, pins)).await.map_err(fail)?;
    set_diff_state(hub, &ids, State::Landing).await.map_err(fail)?;
    hub.store.audit(by.to_owned(), "diffs.land".into(), format!("{} {} ({}) by {}", repo.name, names.join(" "), d.trunk, strategy.as_str())).await;
    queue_event(hub, &ids, "enqueued", None, by).await;
    let mut held_entry = None;
    if !rest.is_empty() {
        let pins = pins_of(hub, &rest).await.map_err(fail)?;
        let why = said(&p.held);
        let id = insert(hub, new(rest.iter().map(|x| x.id.clone()).collect(), "held", Some("gate".into()), Some(why.clone()), pins)).await.map_err(fail)?;
        hub.store.audit(by.to_owned(), "queue.hold".into(), format!("{id} (gate): {why}")).await;
        held_entry = Some(id);
    }
    kick();
    let held: Vec<Value> = p.held.iter().map(|(diff, why)| json!({ "diff": diff, "why": why })).collect();
    Ok(json!({ "entry": entry, "diffs": names, "trunk": d.trunk, "strategy": strategy.as_str(), "state": "queued", "held": held, "held_entry": held_entry }))
}

/// The names (`D3`) of diffs by their ids.
async fn names_of(hub: &Hub, ids: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for id in ids {
        if let Ok(Some(d)) = diffs::find(hub, id).await {
            out.push(d.name);
        }
    }
    out
}

/// `GET /api/repos/:repo/queue`: its entries, newest first (the last 100 that finished too), each
/// with where it stands in its component (docs/ci.md W28-R100), its pinned revisions (R105), who
/// holds it and why (R104), the files it conflicts with the trunk in (R107), and, landed, its land
/// record (R108).
pub async fn list(hub: &Hub, repo: &str) -> Result<Value, Fail> {
    let key = repo.to_owned();
    let mut rows: Vec<Value> = hub
        .store
        .call(move |c| {
            let mut st = c.prepare(
                "SELECT q.id, q.trunk, q.diffs, q.strategy, q.state, q.batch, q.candidate, q.reason, q.enqueued_by, q.created_at, q.updated_at,
                        q.component, q.position, q.window_size, q.verdict, q.hold, q.pinned, q.conflicts, q.route, COALESCE(q.queued_at, q.created_at), q.host, COALESCE(q.tested, 0)
                 FROM land_queue q JOIN repos p ON p.id = q.repo_id
                 WHERE p.name = ?1 OR p.id = ?1 ORDER BY q.state IN ('queued', 'testing', 'held', 'returned') DESC, q.created_at DESC LIMIT 200",
            )?;
            let rows = st.query_map([&key], |r| {
                let ids: Vec<String> = serde_json::from_str(&r.get::<_, String>(2)?).unwrap_or_default();
                let parsed = |i: usize| -> rusqlite::Result<Value> { Ok(r.get::<_, Option<String>>(i)?.and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(Value::Null)) };
                let row = json!({
                    "id": r.get::<_, String>(0)?, "trunk": r.get::<_, String>(1)?, "strategy": r.get::<_, String>(3)?, "state": r.get::<_, String>(4)?,
                    "batch": r.get::<_, Option<String>>(5)?, "candidate": r.get::<_, Option<String>>(6)?, "reason": r.get::<_, Option<String>>(7)?,
                    "enqueued_by": r.get::<_, String>(8)?, "created_at": r.get::<_, i64>(9)?, "updated_at": r.get::<_, i64>(10)?,
                    "component": r.get::<_, Option<String>>(11)?, "position": r.get::<_, Option<i64>>(12)?, "window": r.get::<_, Option<i64>>(13)?, "why": parsed(14)?,
                    "hold": r.get::<_, Option<String>>(15)?, "pinned": parsed(16)?, "conflicts": parsed(17)?, "route": r.get::<_, Option<String>>(18)?,
                    "queued_at": r.get::<_, i64>(19)?, "host": parsed(20)?, "tested": r.get::<_, i64>(21)?,
                });
                Ok((ids, row))
            })?;
            let mut out = Vec::new();
            let mut name = c.prepare("SELECT 'D' || number FROM diffs WHERE id = ?1")?;
            let mut record = c.prepare(&format!("SELECT {RECORD_COLUMNS} FROM land_records WHERE entry_id = ?1"))?;
            for (ids, mut row) in rows.filter_map(Result::ok) {
                let names: Vec<String> = ids.iter().filter_map(|i| name.query_row([i], |r| r.get::<_, String>(0)).ok()).collect();
                let id = row.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
                let kept = record.query_row([&id], record_row).optional()?;
                if let Some(o) = row.as_object_mut() {
                    o.insert("diffs".into(), json!(names));
                    o.insert("record".into(), kept.map(|r| json!(r)).unwrap_or(Value::Null));
                }
                out.push(row);
            }
            Ok(out)
        })
        .await
        .map_err(fail)?;
    // Each candidate's `bench-gate` results (W16-R09).
    let candidates: Vec<String> = rows.iter().filter_map(|r| r.get("candidate").and_then(Value::as_str).map(str::to_owned)).collect();
    let gates = super::review::gates_on(hub, repo, candidates).await.map_err(fail)?;
    for r in &mut rows {
        let g = r.get("candidate").and_then(Value::as_str).and_then(|c| gates.get(c)).cloned().unwrap_or_default();
        if let Some(o) = r.as_object_mut() {
            o.insert("gates".into(), json!(g));
        }
    }
    Ok(json!({ "entries": rows }))
}

/// An entry under test leaves its candidate: the candidate is dropped and its other entries are
/// tested again without it.
async fn leave_candidate(hub: &Hub, e: &Entry) -> anyhow::Result<()> {
    let Some(batch) = e.batch.clone().filter(|_| e.state == "testing") else { return Ok(()) };
    let (b, id) = (batch.clone(), e.id.clone());
    hub.store
        .call(move |c| {
            c.execute(
                "UPDATE land_queue SET state = 'queued', batch = NULL, candidate = NULL, base = NULL, checks = NULL, commits = NULL, reason = 'tested again, without an entry that left its candidate', updated_at = ?3 WHERE batch = ?1 AND state = 'testing' AND id != ?2",
                params![b, id, crate::util::now_unix()],
            )?;
            Ok(())
        })
        .await?;
    if let Some(repo) = hub.store.repos().await?.into_iter().find(|r| r.id == e.repo_id)
        && let Some(path) = super::hosting::native_path(hub, &repo)
    {
        drop_candidate(hub, &path.to_string_lossy(), &e.trunk, &batch).await;
    }
    kick();
    Ok(())
}

/// `DELETE /api/queue/:entry`: out of the queue (not once it landed).
pub async fn cancel(hub: &Arc<Hub>, entry: &str, caller: &Caller) -> Result<Value, Fail> {
    let Some(e) = entry_by_id(hub, entry).await.map_err(fail)? else { return Err((StatusCode::NOT_FOUND, "no such entry".into())) };
    if !matches!(e.state.as_str(), "queued" | "testing" | "held" | "returned") {
        return Err((StatusCode::CONFLICT, format!("the entry is {}", e.state)));
    }
    let id = e.id.clone();
    hub.store.call(move |c| Ok(c.execute("UPDATE land_queue SET state = 'cancelled', reason = 'cancelled', hold = NULL, updated_at = ?2 WHERE id = ?1", params![id, crate::util::now_unix()]).map(|_| ())?)).await.map_err(fail)?;
    if matches!(e.state.as_str(), "queued" | "testing") {
        set_diff_state(hub, &e.diffs, State::Accepted).await.map_err(fail)?;
    }
    // The rest of its candidate is tested again without it.
    leave_candidate(hub, &e).await.map_err(fail)?;
    off_host(hub, &e).await;
    hub.store.audit(caller.name.clone(), "queue.cancel".into(), e.id.clone()).await;
    queue_event(hub, &e.diffs, "dequeued", Some("MANUAL"), &caller.name).await;
    kick();
    Ok(json!({ "entry": e.id, "state": "cancelled" }))
}

/// The queue entry `d` is in, if it's in one: `(entry, state, position in its trunk's queue,
/// when, by whom)`.
pub async fn queued_of(hub: &Hub, d: &Diff) -> Option<(String, String, i64, i64, String)> {
    let id = d.id.clone();
    hub.store
        .call(move |c| {
            Ok(c.query_row(
                "SELECT q.id, q.state, (SELECT COUNT(*) FROM land_queue o WHERE o.repo_id = q.repo_id AND o.trunk = q.trunk AND o.state IN ('queued', 'testing') AND o.created_at <= q.created_at), q.created_at, q.enqueued_by
                 FROM land_queue q WHERE q.state IN ('queued', 'testing') AND EXISTS (SELECT 1 FROM json_each(q.diffs) WHERE value = ?1) ORDER BY q.created_at DESC LIMIT 1",
                [&id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .optional()?)
        })
        .await
        .ok()
        .flatten()
}

/// `d` out of the queue it's in, as [`cancel`] takes an entry out.
pub async fn dequeue(hub: &Arc<Hub>, d: &Diff, caller: &Caller) -> Result<Value, Fail> {
    let Some((entry, ..)) = queued_of(hub, d).await else { return Err((StatusCode::UNPROCESSABLE_ENTITY, format!("{} isn't in the land queue", d.name))) };
    cancel(hub, &entry, caller).await
}

// ------------------------------------------------------------------ Review's word on an entry

/// `POST /api/queue/:entry/hold` `{ reason }` (docs/ci.md W28-R104): Review holds an entry, queued,
/// under test or waiting on its gate, until Review lets it go: out of its candidate, whose other
/// entries are tested again without it, and out of its host's queue.
pub async fn hold(hub: &Arc<Hub>, entry: &str, by: &str, reason: Option<&str>) -> Result<Value, Fail> {
    let e = entry_by_id(hub, entry).await.map_err(fail)?.ok_or((StatusCode::NOT_FOUND, "no such entry".to_owned()))?;
    if !matches!(e.state.as_str(), "queued" | "testing" | "held" | "returned") {
        return Err((StatusCode::CONFLICT, format!("the entry is {}", e.state)));
    }
    let why = match reason {
        Some(r) => format!("Review holds it ({by}): {r}"),
        None => format!("Review holds it ({by})"),
    };
    leave_candidate(hub, &e).await.map_err(fail)?;
    off_host(hub, &e).await;
    hold_entry(hub, &e, "review", &why, "MANUAL").await.map_err(fail)?;
    hub.store.audit(by.to_owned(), "queue.hold".into(), format!("{}: {why}", e.id)).await;
    Ok(json!({ "entry": e.id, "state": "held", "hold": "review", "reason": why }))
}

/// `POST /api/queue/:entry/release` (docs/ci.md W28-R104): an entry Review held goes on as
/// Review's gate says, asked at once.
pub async fn release(hub: &Arc<Hub>, entry: &str, by: &str) -> Result<Value, Fail> {
    let e = entry_by_id(hub, entry).await.map_err(fail)?.ok_or((StatusCode::NOT_FOUND, "no such entry".to_owned()))?;
    if !(e.state == "held" && e.hold.as_deref() == Some("review")) {
        return Err((StatusCode::CONFLICT, format!("Review doesn't hold it: it's {}", e.state)));
    }
    let why = format!("Review let it go ({by}): its gate decides");
    let (id, why2) = (e.id.clone(), why.clone());
    // On its host's queue, it goes in again; else Review's gate decides.
    let (state, hold) = if e.host.is_some() { ("queued", None) } else { ("held", Some("gate")) };
    hub.store.call(move |c| Ok(c.execute("UPDATE land_queue SET state = ?2, hold = ?3, reason = ?4, updated_at = ?5 WHERE id = ?1", params![id, state, hold, why2, crate::util::now_unix()]).map(|_| ())?)).await.map_err(fail)?;
    rechecked().lock().remove(&e.id);
    hub.store.audit(by.to_owned(), "queue.release".into(), format!("{}: {why}", e.id)).await;
    tell(hub, &e, state, Some(why.as_str())).await;
    kick();
    Ok(json!({ "entry": e.id, "state": state, "reason": why }))
}

/// `POST /api/queue/:entry/withdraw` `{ reason }` (docs/ci.md W28-R104): Review takes an entry out
/// of the queue; its diffs are Review's again.
pub async fn withdraw(hub: &Arc<Hub>, entry: &str, by: &str, reason: Option<&str>) -> Result<Value, Fail> {
    let e = entry_by_id(hub, entry).await.map_err(fail)?.ok_or((StatusCode::NOT_FOUND, "no such entry".to_owned()))?;
    if !matches!(e.state.as_str(), "queued" | "testing" | "held" | "returned") {
        return Err((StatusCode::CONFLICT, format!("the entry is {}", e.state)));
    }
    let why = match reason {
        Some(r) => format!("withdrawn by Review ({by}): {r}"),
        None => format!("withdrawn by Review ({by})"),
    };
    leave_candidate(hub, &e).await.map_err(fail)?;
    off_host(hub, &e).await;
    finish(hub, &e, "withdrawn", &why).await.map_err(fail)?;
    hub.store.audit(by.to_owned(), "queue.withdraw".into(), format!("{}: {why}", e.id)).await;
    kick();
    Ok(json!({ "entry": e.id, "state": "withdrawn", "reason": why }))
}

// ------------------------------------------------------------------ landing when ready

/// A diff waiting to land as soon as its trunk's policy lets it: GitHub's auto-merge as a branch
/// that requires a merge queue has it (every kiln trunk lands through its queue), into the queue
/// then, as whoever asked.
#[derive(Debug, Clone)]
pub struct WhenReady {
    /// The strategy it lands by, as asked or else its trunk's policy's own.
    pub strategy: String,
    pub by: String,
    started_by: Option<Value>,
    pub at: i64,
    pub title: Option<String>,
    pub message: Option<String>,
}

/// What `d` waits to land by, if it does.
pub async fn when_ready_of(hub: &Hub, d: &Diff) -> Option<WhenReady> {
    let id = d.id.clone();
    hub.store
        .call(move |c| {
            Ok(c.query_row("SELECT strategy, enabled_by, started_by, enabled_at, commit_title, commit_message FROM land_when_ready WHERE diff_id = ?1", [&id], |r| {
                let started: Option<String> = r.get(2)?;
                Ok(WhenReady { strategy: r.get(0)?, by: r.get(1)?, started_by: started.and_then(|s| serde_json::from_str(&s).ok()), at: r.get(3)?, title: r.get(4)?, message: r.get(5)? })
            })
            .optional()?)
        })
        .await
        .ok()
        .flatten()
}

/// `d` (and the diffs below it) to land as soon as its trunk's policy lets it, as `by` asks (the
/// audit's name), held to `started_by` as [`enqueue_as`] holds an entry: by `strategy` if asked
/// (one the trunk allows), else the policy's. Into the queue at once if Review's gate clears it
/// now, by the same gate [`enqueue_as`] asks (docs/ci.md W28-R104); whether it went.
pub async fn land_when_ready(hub: &Arc<Hub>, d: &Diff, by: &str, started_by: Option<Value>, strategy: Option<&str>, title: Option<String>, message: Option<String>) -> Result<bool, Fail> {
    match d.state.as_str() {
        "landed" | "abandoned" => return Err((StatusCode::UNPROCESSABLE_ENTITY, format!("{} is {}", d.name, d.state))),
        "landing" => return Err((StatusCode::CONFLICT, format!("{} is in the land queue already", d.name))),
        _ => {}
    }
    let (_, path) = diffs::home(hub, d).await?;
    let (policy, _) = super::review::policy(hub, &path, &d.trunk).await;
    let asked = match strategy {
        Some(s) => Some(Strategy::parse(s).ok_or_else(|| (StatusCode::BAD_REQUEST, format!("no strategy `{s}`: stack, stack-squash, squash, rebase, merge or ff-only")))?),
        None => None,
    };
    let strategy = policy.strategy(asked, d.kind != "diff").map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e))?;
    let (id, s, who, started, now) = (d.id.clone(), strategy.as_str().to_owned(), by.to_owned(), started_by.map(|v| v.to_string()), crate::util::now_unix());
    hub.store
        .call(move |c| {
            c.execute(
                "INSERT INTO land_when_ready (diff_id, strategy, enabled_by, started_by, enabled_at, commit_title, commit_message) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT (diff_id) DO UPDATE SET strategy = ?2, enabled_by = ?3, started_by = ?4, enabled_at = ?5, commit_title = ?6, commit_message = ?7",
                params![id, s, who, started, now, title, message],
            )?;
            Ok(())
        })
        .await
        .map_err(fail)?;
    hub.store.audit(by.to_owned(), "diffs.land_when_ready".into(), format!("{} {} ({}) by {}", d.repo, d.name, d.trunk, strategy.as_str())).await;
    queue_event(hub, std::slice::from_ref(&d.id), "auto_merge_enabled", None, by).await;
    Ok(ready(hub, d).await)
}

/// `d` no longer waiting to land, as `by` asks. Whether it was.
pub async fn not_when_ready(hub: &Hub, d: &Diff, by: &str) -> Result<bool, Fail> {
    let id = d.id.clone();
    let was = hub.store.call(move |c| Ok(c.execute("DELETE FROM land_when_ready WHERE diff_id = ?1", [&id])? > 0)).await.map_err(fail)?;
    if was {
        hub.store.audit(by.to_owned(), "diffs.land_when_ready".into(), format!("{} {} no longer", d.repo, d.name)).await;
        queue_event(hub, std::slice::from_ref(&d.id), "auto_merge_disabled", None, by).await;
    }
    Ok(was)
}

async fn forget(hub: &Hub, diff: &str) {
    let id = diff.to_owned();
    if let Err(e) = hub.store.call(move |c| Ok(c.execute("DELETE FROM land_when_ready WHERE diff_id = ?1", [&id]).map(|_| ())?)).await {
        tracing::warn!(diff = %diff, error = %format!("{e:#}"), "a diff's wait to land couldn't end");
    }
}

/// Into the queue if `d` may land now, as whoever asked it to land when ready, its wait over then.
/// One its trunk won't take any more (its strategy no longer allowed, say) stops waiting.
async fn ready(hub: &Arc<Hub>, d: &Diff) -> bool {
    let Some(w) = when_ready_of(hub, d).await else { return false };
    match enqueue_as(hub, d, &w.by, w.started_by.clone(), &json!({ "strategy": w.strategy })).await {
        Ok(_) => {
            forget(hub, &d.id).await;
            true
        }
        // Not yet: what keeps it from landing (Review's gate holds it, or sends it back).
        Err((StatusCode::CONFLICT, _)) => false,
        Err((code, why)) if code.is_client_error() => {
            tracing::info!(diff = %d.name, %why, "a diff waiting to land can't any more: its wait ends");
            forget(hub, &d.id).await;
            queue_event(hub, std::slice::from_ref(&d.id), "auto_merge_disabled", Some(why.as_str()), "kiln").await;
            false
        }
        Err((_, why)) => {
            tracing::warn!(diff = %d.name, %why, "a diff waiting to land couldn't be put in the queue");
            false
        }
    }
}

/// The diffs waiting to land when ready: each into the queue once it may, its wait over once it's
/// in one (whoever put it there), landed or abandoned.
async fn waiting(hub: &Arc<Hub>) {
    let ids: Vec<String> = hub
        .store
        .call(|c| {
            let mut st = c.prepare("SELECT diff_id FROM land_when_ready ORDER BY enabled_at")?;
            let rows = st.query_map([], |r| r.get::<_, String>(0))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
        .unwrap_or_default();
    for id in ids {
        match diffs::find(hub, &id).await {
            Ok(Some(d)) if matches!(d.state.as_str(), "landing" | "landed" | "abandoned") => forget(hub, &id).await,
            Ok(Some(d)) if d.state != "draft" => {
                ready(hub, &d).await;
            }
            Ok(Some(_)) => {}
            Ok(None) => forget(hub, &id).await,
            Err(e) => tracing::warn!(diff = %id, error = %format!("{e:#}"), "a diff waiting to land couldn't be read"),
        }
    }
}

// ------------------------------------------------------------------ building candidates

/// The git version, `(major, minor)`.
async fn git_version(hub: &Hub) -> (u32, u32) {
    static V: OnceLock<(u32, u32)> = OnceLock::new();
    if let Some(v) = V.get() {
        return *v;
    }
    let out = gitops::run_git(hub, &["version"], LS_REMOTE_TIMEOUT).await.map(|o| String::from_utf8_lossy(&o).into_owned()).unwrap_or_default();
    let mut nums = out.split_whitespace().nth(2).unwrap_or_default().split('.').map(|n| n.parse::<u32>().unwrap_or(0));
    let v = (nums.next().unwrap_or(0), nums.next().unwrap_or(0));
    *V.get_or_init(|| v)
}

/// Why a commit didn't go onto the candidate.
enum Refusal {
    /// It conflicts there: the files.
    Conflict(Vec<String>),
    Other(String),
}

/// The tree of `commit` (whose parent is `parent`) applied onto `onto`: a cherry-pick without a
/// working tree.
async fn pick_tree(hub: &Hub, path: &str, onto: &str, commit: &str, parent: &str) -> Result<String, Refusal> {
    if git_version(hub).await >= (2, 40) {
        let base = format!("--merge-base={parent}");
        let r = diffs::git_run(hub, &["-C", path, "merge-tree", "--write-tree", "--no-messages", "--name-only", &base, onto, commit], &[], Vec::new()).await.map_err(|e| Refusal::Other(format!("{e:#}")))?;
        let text = String::from_utf8_lossy(&r.out).into_owned();
        let mut lines = text.lines();
        let tree = lines.next().unwrap_or_default().trim().to_owned();
        if r.ok && !tree.is_empty() {
            return Ok(tree);
        }
        let files: Vec<String> = lines.filter(|l| !l.is_empty()).map(str::to_owned).collect();
        if !files.is_empty() {
            return Err(Refusal::Conflict(files));
        }
        return Err(Refusal::Other(String::from_utf8_lossy(&r.err).trim().to_owned()));
    }
    // Older git: a temporary worktree of the bare repository.
    let tmp = std::env::temp_dir().join(format!("kiln-land-{}", crate::util::new_id()));
    let t = tmp.to_string_lossy().into_owned();
    let made = gitops::run_git(hub, &["-C", path, "worktree", "add", "--quiet", "--detach", &t, onto], LS_REMOTE_TIMEOUT).await;
    let result = match made {
        Err(e) => Err(Refusal::Other(format!("{e:#}"))),
        Ok(_) => {
            let r = diffs::git_run(hub, &["-C", &t, "-c", "user.name=kiln", "-c", "user.email=kiln@localhost", "cherry-pick", "--no-commit", "--allow-empty", commit], &[], Vec::new()).await.map_err(|e| Refusal::Other(format!("{e:#}")));
            match r {
                Ok(r) if r.ok => gitops::run_git(hub, &["-C", &t, "write-tree"], LS_REMOTE_TIMEOUT).await.map(|o| String::from_utf8_lossy(&o).trim().to_owned()).map_err(|e| Refusal::Other(format!("{e:#}"))),
                Ok(_) => {
                    let files = gitops::run_git(hub, &["-C", &t, "diff", "--name-only", "--diff-filter=U"], LS_REMOTE_TIMEOUT).await.map(|o| String::from_utf8_lossy(&o).lines().map(str::to_owned).collect()).unwrap_or_default();
                    Err(Refusal::Conflict(files))
                }
                Err(e) => Err(e),
            }
        }
    };
    let _ = gitops::run_git(hub, &["-C", path, "worktree", "remove", "--force", &t], LS_REMOTE_TIMEOUT).await;
    let _ = std::fs::remove_dir_all(&tmp);
    result
}

/// The tree of `theirs` merged into `ours` (their merge base found by git), without a working
/// tree.
async fn merge_tree(hub: &Hub, path: &str, ours: &str, theirs: &str) -> Result<String, Refusal> {
    if git_version(hub).await >= (2, 38) {
        let r = diffs::git_run(hub, &["-C", path, "merge-tree", "--write-tree", "--no-messages", "--name-only", ours, theirs], &[], Vec::new()).await.map_err(|e| Refusal::Other(format!("{e:#}")))?;
        let text = String::from_utf8_lossy(&r.out).into_owned();
        let mut lines = text.lines();
        let tree = lines.next().unwrap_or_default().trim().to_owned();
        if r.ok && !tree.is_empty() {
            return Ok(tree);
        }
        let files: Vec<String> = lines.filter(|l| !l.is_empty()).map(str::to_owned).collect();
        return Err(if files.is_empty() { Refusal::Other(String::from_utf8_lossy(&r.err).trim().to_owned()) } else { Refusal::Conflict(files) });
    }
    let tmp = std::env::temp_dir().join(format!("kiln-land-{}", crate::util::new_id()));
    let t = tmp.to_string_lossy().into_owned();
    let result = match gitops::run_git(hub, &["-C", path, "worktree", "add", "--quiet", "--detach", &t, ours], LS_REMOTE_TIMEOUT).await {
        Err(e) => Err(Refusal::Other(format!("{e:#}"))),
        Ok(_) => match diffs::git_run(hub, &["-C", &t, "-c", "user.name=kiln", "-c", "user.email=kiln@localhost", "merge", "--no-commit", "--no-ff", "--quiet", theirs], &[], Vec::new()).await {
            Ok(r) if r.ok => gitops::run_git(hub, &["-C", &t, "write-tree"], LS_REMOTE_TIMEOUT).await.map(|o| String::from_utf8_lossy(&o).trim().to_owned()).map_err(|e| Refusal::Other(format!("{e:#}"))),
            Ok(_) => Err(Refusal::Conflict(gitops::run_git(hub, &["-C", &t, "diff", "--name-only", "--diff-filter=U"], LS_REMOTE_TIMEOUT).await.map(|o| String::from_utf8_lossy(&o).lines().map(str::to_owned).collect()).unwrap_or_default())),
            Err(e) => Err(Refusal::Other(format!("{e:#}"))),
        },
    };
    let _ = gitops::run_git(hub, &["-C", path, "worktree", "remove", "--force", &t], LS_REMOTE_TIMEOUT).await;
    let _ = std::fs::remove_dir_all(&tmp);
    result
}

/// A change request applied onto `tip` by its strategy (W14-R07): `squash` (one commit), `merge`
/// (a merge commit), `rebase` (each commit replayed), `ff-only` (the branch's head, when the
/// trunk is its ancestor).
async fn apply_change_request(hub: &Hub, path: &str, tip: &str, strategy: Strategy, d: &Diff, v: &diffs::Version, reviewers: &[String]) -> Result<Applied, Refusal> {
    let other = |x: anyhow::Error| Refusal::Other(format!("{x:#}"));
    let head = v.commit.clone();
    let mut commits = BTreeMap::new();
    let message = format!("{}\n\n{}", d.title, d.summary);
    match strategy {
        Strategy::FfOnly => {
            if gitops::run_git(hub, &["-C", path, "merge-base", "--is-ancestor", tip, &head], LS_REMOTE_TIMEOUT).await.is_err() {
                return Err(Refusal::Other(format!("ff-only: {} has moved past the branch; rebase it onto {} or land with another strategy", d.trunk, d.trunk)));
            }
            commits.insert(d.id.clone(), head.clone());
            Ok(Applied { tip: head, commits })
        }
        Strategy::Merge => {
            let tree = merge_tree(hub, path, tip, &head).await?;
            let who = author(hub, path, &head).await.map_err(other)?;
            let msg = land::landed_message(&format!("Merge {}: {}\n\n{}", d.name, d.title, d.summary), &d.name, reviewers, &changes_of(&[d]));
            let c = commit_tree(hub, path, &tree, &[tip, &head], &who, &msg).await.map_err(other)?;
            commits.insert(d.id.clone(), c.clone());
            Ok(Applied { tip: c, commits })
        }
        Strategy::Rebase => {
            let list = gitops::run_git(hub, &["-C", path, "rev-list", "--reverse", "--topo-order", "--no-merges", &format!("{}..{head}", v.base)], LS_REMOTE_TIMEOUT).await.map(|o| String::from_utf8_lossy(&o).lines().map(str::to_owned).collect::<Vec<_>>()).map_err(other)?;
            let mut at = tip.to_owned();
            for c in &list {
                let parent = gitops::run_git(hub, &["-C", path, "rev-parse", &format!("{c}^")], LS_REMOTE_TIMEOUT).await.map(|o| String::from_utf8_lossy(&o).trim().to_owned()).map_err(other)?;
                let tree = pick_tree(hub, path, &at, c, &parent).await?;
                let who = author(hub, path, c).await.map_err(other)?;
                let msg = gitops::run_git(hub, &["-C", path, "log", "-1", "--format=%B", c], LS_REMOTE_TIMEOUT).await.map(|o| String::from_utf8_lossy(&o).into_owned()).unwrap_or_default();
                at = commit_tree(hub, path, &tree, &[&at], &who, &land::landed_message(&msg, &d.name, reviewers, &changes_of(&[d]))).await.map_err(other)?;
            }
            commits.insert(d.id.clone(), at.clone());
            Ok(Applied { tip: at, commits })
        }
        // `squash`, the default.
        _ => {
            let tree = merge_tree(hub, path, tip, &head).await?;
            let who = author(hub, path, &head).await.map_err(other)?;
            let c = commit_tree(hub, path, &tree, &[tip], &who, &land::landed_message(&message, &d.name, reviewers, &changes_of(&[d]))).await.map_err(other)?;
            commits.insert(d.id.clone(), c.clone());
            Ok(Applied { tip: c, commits })
        }
    }
}

/// The changes' identities `diffs` land, for their commits' `Kiln-Change` lines (W26-R03).
fn changes_of(diffs: &[&Diff]) -> Vec<String> {
    diffs.iter().filter_map(|d| d.change.clone()).collect()
}

/// The author of `commit`: name, email, date (raw).
async fn author(hub: &Hub, path: &str, commit: &str) -> anyhow::Result<(String, String, String)> {
    let o = gitops::run_git(hub, &["-C", path, "log", "-1", "--format=%an%x00%ae%x00%ad", "--date=raw", commit], LS_REMOTE_TIMEOUT).await?;
    let text = String::from_utf8_lossy(&o).trim().to_owned();
    let mut f = text.split('\0');
    Ok((f.next().unwrap_or_default().to_owned(), f.next().unwrap_or_default().to_owned(), f.next().unwrap_or_default().to_owned()))
}

/// A commit of `tree` on `parents`, by `who` (committed by kiln, signed when
/// `[ci.land_signing]` says so), saying `message`.
async fn commit_tree(hub: &Hub, path: &str, tree: &str, parents: &[&str], who: &(String, String, String), message: &str) -> anyhow::Result<String> {
    let signing = hub.cfg.ci.land_signing.as_ref().filter(|s| !s.key.is_empty());
    let (fmt, key) = signing.map(|s| (format!("gpg.format={}", s.format.as_deref().unwrap_or("ssh")), format!("user.signingkey={}", s.key))).unwrap_or_default();
    let mut args: Vec<&str> = Vec::new();
    if signing.is_some() {
        args.extend(["-c", &fmt, "-c", &key]);
    }
    args.extend(["-C", path, "commit-tree", tree]);
    for p in parents {
        args.extend(["-p", p]);
    }
    if signing.is_some() {
        args.push("-S");
    }
    args.extend(["-F", "-"]);
    let env = [("GIT_AUTHOR_NAME", who.0.as_str()), ("GIT_AUTHOR_EMAIL", who.1.as_str()), ("GIT_AUTHOR_DATE", who.2.as_str()), ("GIT_COMMITTER_NAME", "kiln"), ("GIT_COMMITTER_EMAIL", "kiln@localhost")];
    let r = diffs::git_run(hub, &args, &env, message.as_bytes().to_vec()).await?;
    anyhow::ensure!(r.ok, "git commit-tree: {}", String::from_utf8_lossy(&r.err).trim());
    Ok(String::from_utf8_lossy(&r.out).trim().to_owned())
}

/// A commit of `tree` on `parent` that only a conflict check stands on: kiln's, never signed, no
/// ref keeping it.
async fn scratch_commit(hub: &Hub, path: &str, tree: &str, parent: &str) -> anyhow::Result<String> {
    let date = format!("{} +0000", crate::util::now_unix());
    let env = [("GIT_AUTHOR_NAME", "kiln"), ("GIT_AUTHOR_EMAIL", "kiln@localhost"), ("GIT_AUTHOR_DATE", date.as_str()), ("GIT_COMMITTER_NAME", "kiln"), ("GIT_COMMITTER_EMAIL", "kiln@localhost"), ("GIT_COMMITTER_DATE", date.as_str())];
    let r = diffs::git_run(hub, &["-C", path, "commit-tree", tree, "-p", parent, "-m", "scratch"], &env, Vec::new()).await?;
    anyhow::ensure!(r.ok, "git commit-tree: {}", String::from_utf8_lossy(&r.err).trim());
    Ok(String::from_utf8_lossy(&r.out).trim().to_owned())
}

/// The files each of `picks` (bottom first: a commit and its parent; none for a diff without a
/// version) conflicts with `trunk`'s tip in, each applied on top of those below it (docs/ci.md
/// W28-R104, R107). None above a pick that conflicts or can't be made, none at all when the trunk
/// can't be read.
pub(crate) async fn conflicts_on(hub: &Hub, path: &str, trunk: &str, picks: &[Option<(String, String)>]) -> Vec<Vec<String>> {
    let mut out: Vec<Vec<String>> = picks.iter().map(|_| Vec::new()).collect();
    let Ok(mut at) = rev_parse(hub, path, &format!("refs/heads/{trunk}^{{commit}}")).await else { return out };
    for (i, pick) in picks.iter().enumerate() {
        let Some((commit, parent)) = pick else { break };
        match pick_tree(hub, path, &at, commit, parent).await {
            Ok(tree) => match scratch_commit(hub, path, &tree, &at).await {
                Ok(c) => at = c,
                Err(_) => break,
            },
            Err(Refusal::Conflict(files)) => {
                if let Some(slot) = out.get_mut(i) {
                    *slot = files;
                }
                break;
            }
            Err(Refusal::Other(_)) => break,
        }
    }
    out
}

/// What an entry put on the candidate: its new tip, and the commit that carries each diff.
struct Applied {
    tip: String,
    commits: BTreeMap<String, String>,
}

/// An entry's diffs applied onto `tip` by its strategy, each at the version its pin names
/// (docs/ci.md W28-R105): the candidate tested is the approved revision onto the trunk.
async fn apply(hub: &Hub, path: &str, tip: &str, e: &Entry) -> Result<Applied, Refusal> {
    let mut chain = Vec::new();
    for id in &e.diffs {
        let d = diffs::find(hub, id).await.map_err(|x| Refusal::Other(format!("{x:#}")))?.ok_or_else(|| Refusal::Other("a diff of it is gone".into()))?;
        let versions = diffs::versions(hub, &d).await.map_err(|x| Refusal::Other(format!("{x:#}")))?;
        let v = match e.pinned.iter().find(|p| p.diff == d.name) {
            Some(p) => versions.iter().find(|v| v.n == p.version && v.commit == p.commit).cloned().ok_or_else(|| Refusal::Other(format!("{}'s v{}, approved, is gone", d.name, p.version)))?,
            None => versions.last().cloned().ok_or_else(|| Refusal::Other(format!("{} has no version", d.name)))?,
        };
        let f = super::review::facts(hub, &d, &versions).await.map_err(|(_, m)| Refusal::Other(m))?;
        let (policy, _) = super::review::policy(hub, path, &d.trunk).await;
        let reviewers: Vec<String> = f.accepting(&policy.land).into_iter().collect();
        chain.push((d, v, reviewers));
    }
    // A change request lands whole, by its own strategies.
    if let [(d, v, reviewers)] = chain.as_slice()
        && d.kind == "change_request"
    {
        return apply_change_request(hub, path, tip, e.strategy, d, v, reviewers).await;
    }
    let msg_of = |sha: &str| {
        let s = sha.to_owned();
        async move { gitops::run_git(hub, &["-C", path, "log", "-1", "--format=%B", &s], LS_REMOTE_TIMEOUT).await.map(|o| String::from_utf8_lossy(&o).into_owned()).unwrap_or_default() }
    };
    let mut commits = BTreeMap::new();
    match e.strategy {
        Strategy::FfOnly => {
            // The stack exactly as it is, on the trunk's tip.
            let shas: Vec<String> = chain.iter().map(|(_, v, _)| v.commit.clone()).collect();
            let Some(top) = shas.last().cloned() else { return Err(Refusal::Other("nothing to land".into())) };
            let between = gitops::run_git(hub, &["-C", path, "rev-list", "--reverse", "--topo-order", &format!("{tip}..{top}")], LS_REMOTE_TIMEOUT).await.map(|o| String::from_utf8_lossy(&o).lines().map(str::to_owned).collect::<Vec<_>>()).unwrap_or_default();
            let on_tip = gitops::run_git(hub, &["-C", path, "merge-base", "--is-ancestor", tip, &top], LS_REMOTE_TIMEOUT).await.is_ok();
            if !on_tip || between != shas {
                return Err(Refusal::Other("ff-only: the stack isn't on the trunk's tip as it is; `kiln sync` and submit again, or land with another strategy".into()));
            }
            for (d, v, _) in &chain {
                commits.insert(d.id.clone(), v.commit.clone());
            }
            Ok(Applied { tip: top, commits })
        }
        Strategy::StackSquash | Strategy::Squash => {
            let mut scratch = tip.to_owned();
            let mut tree = String::new();
            for (_, v, _) in &chain {
                tree = pick_tree(hub, path, &scratch, &v.commit, &v.parent).await?;
                let who = author(hub, path, &v.commit).await.map_err(|x| Refusal::Other(format!("{x:#}")))?;
                scratch = commit_tree(hub, path, &tree, &[&scratch], &who, "scratch").await.map_err(|x| Refusal::Other(format!("{x:#}")))?;
            }
            let Some((top, top_v, _)) = chain.last() else { return Err(Refusal::Other("nothing to land".into())) };
            let mut message = format!("{}\n\n", top.title);
            for (d, _, _) in &chain {
                message.push_str(&format!("- {}: {}\n", d.name, d.title));
            }
            let mut reviewers: Vec<String> = chain.iter().flat_map(|(_, _, r)| r.clone()).collect();
            reviewers.sort();
            reviewers.dedup();
            let revisions: Vec<String> = chain.iter().map(|(d, _, _)| d.name.clone()).collect();
            let mut message = land::landed_message(&message, &revisions.join(", "), &reviewers, &changes_of(&chain.iter().map(|(d, _, _)| d).collect::<Vec<_>>()));
            if revisions.len() > 1 {
                message = message.replace(&format!("Differential-Revision: {}", revisions.join(", ")), &revisions.iter().map(|r| format!("Differential-Revision: {r}")).collect::<Vec<_>>().join("\n"));
            }
            let who = author(hub, path, &top_v.commit).await.map_err(|x| Refusal::Other(format!("{x:#}")))?;
            let c = commit_tree(hub, path, &tree, &[tip], &who, &message).await.map_err(|x| Refusal::Other(format!("{x:#}")))?;
            for (d, _, _) in &chain {
                commits.insert(d.id.clone(), c.clone());
            }
            Ok(Applied { tip: c, commits })
        }
        // `stack` (and anything else a diff lands by): one commit each, in order.
        _ => {
            let mut at = tip.to_owned();
            for (d, v, reviewers) in &chain {
                let tree = pick_tree(hub, path, &at, &v.commit, &v.parent).await?;
                let who = author(hub, path, &v.commit).await.map_err(|x| Refusal::Other(format!("{x:#}")))?;
                let message = land::landed_message(&msg_of(&v.commit).await, &d.name, reviewers, &changes_of(&[d]));
                at = commit_tree(hub, path, &tree, &[&at], &who, &message).await.map_err(|x| Refusal::Other(format!("{x:#}")))?;
                commits.insert(d.id.clone(), at.clone());
            }
            Ok(Applied { tip: at, commits })
        }
    }
}

/// A commit as payloads describe it: its tree, message, and who and when.
async fn commit_of(hub: &Hub, path: &str, sha: &str) -> kiln_ci::trigger::context::Commit {
    use kiln_ci::trigger::context::{Commit, Person};
    let o = gitops::run_git(hub, &["-C", path, "show", "-s", "--format=%T%x00%an%x00%ae%x00%aI%x00%cn%x00%ce%x00%B", sha], LS_REMOTE_TIMEOUT).await.unwrap_or_default();
    let text = String::from_utf8_lossy(&o).into_owned();
    let f: Vec<&str> = text.splitn(7, '\0').collect();
    let get = |i: usize| f.get(i).copied().unwrap_or_default().trim_end_matches('\n').to_owned();
    let person = |name: String, email: String| Person { name, email, username: None };
    Commit { id: sha.to_owned(), tree_id: get(0), message: get(6), timestamp: get(3), author: person(get(1), get(2)), committer: person(get(4), get(5)) }
}

/// The candidate's `land` and `merge_group` events: GitHub's payload, with kiln's own part
/// (`kiln.land`). Held to whoever landed its entries (`started_by`).
#[allow(clippy::too_many_arguments)]
fn candidate_events(ctx: &kiln_ci::trigger::context::Repo, repo: &Repo, trunk: &str, base: &str, tip: &kiln_ci::trigger::context::Commit, batch: &str, names: &[String], started_by: Option<Value>) -> (Event, Event) {
    let head_ref = format!("refs/kiln/land/{trunk}/{batch}");
    let kiln = kiln_ci::trigger::context::Person { name: "kiln".into(), email: String::new(), username: Some("kiln[bot]".into()) };
    let mut payload = kiln_ci::trigger::context::merge_group(ctx, trunk, base, tip, &head_ref, &kiln);
    if let Some(p) = payload.as_object_mut() {
        p.insert("kiln".into(), json!({ "land": { "trunk": trunk, "batch": batch, "diffs": names } }));
    }
    let tip = tip.id.as_str();
    let mg = Event { name: "merge_group".into(), action: "checks_requested".into(), repo: repo.name.clone(), git_ref: head_ref.clone(), sha: tip.to_owned(), base_ref: Some(trunk.to_owned()), actor: "kiln".into(), trusted: true, source: Source::Native, payload, started_by, ..Event::default() };
    let landing = Event { name: "land".into(), action: String::new(), ..mg.clone() };
    (mg, landing)
}

/// Starts the candidate's workflows: those whose `on` takes `merge_group` or `land`. Their paths.
async fn start_checks(hub: &Hub, repo: &Repo, events: &(Event, Event)) -> anyhow::Result<Vec<String>> {
    if !hub.ci.enabled() {
        return Ok(Vec::new());
    }
    let commit = super::load::load(hub, repo, &events.0.sha).await?;
    let mut started = Vec::new();
    for l in commit.workflows.iter().filter(|l| l.runs && (l.dialect == kiln_ci::expr::Dialect::Gha || hub.cfg.ci.kiln_workflows)) {
        for e in [&events.0, &events.1] {
            if super::events::start(hub, repo, e, l, "land candidate", "kiln", Value::Null).await? {
                started.push(l.path.clone());
                break;
            }
        }
    }
    Ok(started)
}

/// A required check the candidate's workflows don't cover.
fn uncovered(required: &[String], started: &[String], names: &BTreeMap<String, Option<String>>) -> Vec<String> {
    let stem = |p: &str| p.rsplit('/').next().unwrap_or(p).trim_end_matches(".yml").trim_end_matches(".yaml").to_owned();
    required
        .iter()
        .filter(|c| {
            let head = c.split('/').next().unwrap_or(c);
            !(started.iter().any(|s| s == c.as_str() || stem(s) == head || s == head) || names.contains_key(c.as_str()) || names.keys().any(|k| k.split('/').next() == Some(head)))
        })
        .cloned()
        .collect()
}

async fn rev_parse(hub: &Hub, path: &str, rev: &str) -> anyhow::Result<String> {
    gitops::run_git(hub, &["-C", path, "rev-parse", "--verify", rev], LS_REMOTE_TIMEOUT).await.map(|o| String::from_utf8_lossy(&o).trim().to_owned())
}

/// The files that differ between two commits, a rename both its paths.
async fn files_between(hub: &Hub, path: &str, from: &str, to: &str) -> anyhow::Result<BTreeSet<String>> {
    let out = gitops::run_git(hub, &["-C", path, "diff", "--name-only", "--no-renames", "-z", from, to], LS_REMOTE_TIMEOUT).await?;
    Ok(out.split(|b| *b == 0).filter_map(|p| std::str::from_utf8(p).ok()).filter(|p| !p.is_empty()).map(str::to_owned).collect())
}

/// What a pinned revision changes (docs/ci.md W28-R100): the files between its parent and its
/// commit, kept, as a commit's never change.
async fn files_of(hub: &Hub, path: &str, parent: &str, commit: &str) -> BTreeSet<String> {
    static KEPT: OnceLock<parking_lot::Mutex<HashMap<(String, String), BTreeSet<String>>>> = OnceLock::new();
    let kept = KEPT.get_or_init(Default::default);
    let key = (parent.to_owned(), commit.to_owned());
    if let Some(f) = kept.lock().get(&key) {
        return f.clone();
    }
    match files_between(hub, path, parent, commit).await {
        Ok(files) => {
            let mut k = kept.lock();
            if k.len() >= 4096 {
                k.clear();
            }
            k.insert(key, files.clone());
            files
        }
        Err(e) => {
            tracing::warn!(commit, error = %format!("{e:#}"), "what a land queue entry changes couldn't be read");
            BTreeSet::new()
        }
    }
}

// ------------------------------------------------------------------ the queue's step

async fn tick(hub: &Arc<Hub>) -> anyhow::Result<()> {
    waiting(hub).await;
    let groups: Vec<(String, String)> = hub
        .store
        .call(|c| {
            let mut st = c.prepare("SELECT DISTINCT repo_id, trunk FROM land_queue WHERE state IN ('queued', 'testing', 'held', 'returned')")?;
            let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await?;
    if groups.is_empty() {
        return Ok(());
    }
    let repos = hub.store.repos().await?;
    for (repo_id, trunk) in groups {
        let Some(repo) = repos.iter().find(|r| r.id == repo_id) else { continue };
        if let Err(e) = step(hub, repo, &trunk).await {
            tracing::warn!(repo = %repo.name, %trunk, error = %format!("{e:#}"), "the land queue couldn't move");
        }
    }
    Ok(())
}

/// One step of a trunk's queue (docs/ci.md W28-R100 to R109): what waits on Review asked again,
/// pins held to, then what `queue::plan` decides for the entries queued and under test, carried out
/// in order, and where each entry stands kept on it.
async fn step(hub: &Arc<Hub>, repo: &Repo, trunk: &str) -> anyhow::Result<()> {
    let Some(path) = super::hosting::native_path(hub, repo) else {
        return host_step(hub, repo, trunk).await;
    };
    let path = path.to_string_lossy().into_owned();
    let (policy, _) = super::review::policy(hub, &path, trunk).await;
    // R104 to R107: what waits on Review's gate, asked again (what Review holds waits for Review).
    for e in entries(hub, &repo.id, trunk, &["held", "returned"]).await? {
        if e.hold.as_deref() == Some("review") || !due(&e.id, RECHECK) {
            continue;
        }
        if let Err(err) = recheck_entry(hub, &e).await {
            tracing::warn!(entry = %e.id, error = %format!("{err:#}"), "an entry waiting on Review couldn't be asked again");
        }
    }
    // R105: what Review approved, still.
    pins_step(hub, trunk, &repo.id).await?;
    let live = entries(hub, &repo.id, trunk, &["queued", "testing"]).await?;
    if live.is_empty() {
        return Ok(());
    }
    let broken: Vec<Entry> = live.iter().filter(|e| e.state == "testing" && (e.candidate.is_none() || e.base.is_none() || e.batch.is_none())).cloned().collect();
    if !broken.is_empty() {
        return requeue(hub, &broken).await;
    }
    let tip = rev_parse(hub, &path, &format!("refs/heads/{trunk}^{{commit}}")).await?;
    let seen = see(hub, repo, &path, &policy, &tip, &live).await;
    let plan = queue::plan(&seen.units, &seen.items, &seen.candidates, &queue::Knobs::of_batch(policy.queue.batch));
    for s in &plan.steps {
        let done = match s {
            Step::Land { batch, entries, why, .. } => {
                tracing::info!(repo = %repo.name, %trunk, %why, "a land candidate passed");
                land_step(hub, repo, &path, trunk, &policy, &seen, batch, entries).await
            }
            Step::Retest { batch, entries, why } => {
                drop_candidate(hub, &path, trunk, batch).await;
                requeue_with(hub, &by_ids(&live, entries), Some(why.as_str())).await
            }
            Step::Eject { batch, entry, why, .. } => {
                drop_candidate(hub, &path, trunk, batch).await;
                match live.iter().find(|e| &e.id == entry) {
                    Some(e) => eject(hub, e, why, "CI_FAILURE").await,
                    None => Ok(()),
                }
            }
            Step::Split { batch, entries, head, next, why, .. } => {
                drop_candidate(hub, &path, trunk, batch).await;
                tracing::info!(repo = %repo.name, %trunk, %why, "a land candidate failed: bisected inside its component");
                match requeue_with(hub, &by_ids(&live, entries), Some(why.as_str())).await {
                    Ok(()) => set_bisect(hub, head, *next).await,
                    Err(e) => Err(e),
                }
            }
            Step::Build { entries, why, .. } => build_step(hub, repo, &path, trunk, &policy, entries, why).await,
        };
        if let Err(e) = done {
            tracing::warn!(repo = %repo.name, %trunk, error = %format!("{e:#}"), "a land queue step couldn't be carried out");
        }
    }
    keep(hub, &plan.standings).await
}

fn by_ids(live: &[Entry], ids: &[String]) -> Vec<Entry> {
    ids.iter().filter_map(|id| live.iter().find(|e| &e.id == id).cloned()).collect()
}

/// Entries queued and under test whose pin a newer revision invalidated hold (docs/ci.md
/// W28-R105): what they'd land isn't what Review approved any more. A stack's diffs below the first
/// such go on (R106). An entry from before pins is pinned to what it has.
async fn pins_step(hub: &Arc<Hub>, trunk: &str, repo_id: &str) -> anyhow::Result<()> {
    for e in entries(hub, repo_id, trunk, &["queued", "testing"]).await? {
        if e.host.is_some() {
            continue;
        }
        let ds = diffs_of(hub, &e).await?;
        if e.pinned.is_empty() {
            let pins = serde_json::to_string(&pins_of(hub, &ds).await?)?;
            let id = e.id.clone();
            hub.store.call(move |c| Ok(c.execute("UPDATE land_queue SET pinned = ?2 WHERE id = ?1", params![id, pins]).map(|_| ())?)).await?;
            continue;
        }
        let mut gates = Vec::new();
        let mut failing = false;
        for d in &ds {
            let g = match e.pinned.iter().find(|p| p.diff == d.name) {
                Some(p) => match queue::pin_holds(p, &revisions(hub, &d.id).await?) {
                    Ok(()) => Gate::Ready,
                    Err(why) => {
                        failing = true;
                        Gate::Hold { why: vec![why] }
                    }
                },
                None => Gate::Ready,
            };
            gates.push((d.name.clone(), g));
        }
        if !failing {
            continue;
        }
        leave_candidate(hub, &e).await?;
        if not_cleared(hub, &e, gates, &BTreeMap::new()).await? {
            requeue_with(hub, std::slice::from_ref(&e), Some("tested again without the diffs above it that hold")).await?;
        }
    }
    Ok(())
}

/// What the planner sees of a trunk's queue (docs/ci.md W28-R100).
struct Seen {
    units: Units,
    items: Vec<Item>,
    candidates: Vec<Candidate>,
    /// Each entry's change, by its id.
    changes: HashMap<String, Change>,
}

/// What the planner sees of a trunk's queue (docs/ci.md W28-R100 to R102): each entry queued or
/// under test as an item, its change the files its pinned revisions change, and those of the diffs
/// below it that entries ahead of it land (a stack's later entry waits for them); the repository's
/// units, the trunk's package graph and each entry's own; and each candidate under test, with how
/// its checks stand and what moved the trunk since it was built.
async fn see(hub: &Hub, repo: &Repo, path: &str, policy: &land::Policy, tip: &str, live: &[Entry]) -> Seen {
    let now = crate::util::now_unix();
    let mut by_diff: HashMap<String, BTreeSet<String>> = HashMap::new();
    for p in live.iter().flat_map(|e| e.pinned.iter()) {
        if !by_diff.contains_key(&p.diff) {
            let files = files_of(hub, path, &p.parent, &p.commit).await;
            by_diff.insert(p.diff.clone(), files);
        }
    }
    let mut items = Vec::new();
    let mut changes = HashMap::new();
    let mut labels: BTreeSet<String> = BTreeSet::new();
    for e in live {
        let names: Vec<String> = e.pinned.iter().map(|p| p.diff.clone()).collect();
        let mut label = names.join(", ");
        if label.is_empty() || !labels.insert(label.clone()) {
            label = e.id.clone();
            labels.insert(label.clone());
        }
        let mut files: BTreeSet<String> = names.iter().filter_map(|n| by_diff.get(n)).flatten().cloned().collect();
        files.extend(below_files(hub, e, &by_diff).await);
        let change = Change { id: label, files, targets: BTreeSet::new() };
        changes.insert(e.id.clone(), change.clone());
        items.push(Item { id: e.id.clone(), change, batch: e.batch.clone(), window: e.window, bisect: e.bisect, waited_s: now.saturating_sub(e.queued_at).max(0), service_s: 0 });
    }
    let mut graphs = Vec::new();
    if let Some(g) = super::graphs::at_in(hub, repo, path, tip).await {
        graphs.push(g);
    }
    for e in live {
        if let Some(top) = e.pinned.last()
            && let Some(g) = super::graphs::at_in(hub, repo, path, &top.commit).await
        {
            graphs.push(g);
        }
    }
    let units = queue::units(graphs.iter().map(|g| &**g));
    let mut batches: Vec<(String, Vec<&Entry>)> = Vec::new();
    for e in live.iter().filter(|e| e.state == "testing") {
        let Some(b) = &e.batch else { continue };
        match batches.iter_mut().find(|(x, _)| x == b) {
            Some((_, list)) => list.push(e),
            None => batches.push((b.clone(), vec![e])),
        }
    }
    let mut candidates = Vec::new();
    for (batch, es) in batches {
        let Some(first) = es.first() else { continue };
        let outcome = outcome_of(hub, repo, policy, first).await;
        let since = since_of(hub, path, first.base.as_deref().unwrap_or_default(), tip).await;
        candidates.push(Candidate { batch, entries: es.iter().map(|e| e.id.clone()).collect(), outcome, since });
    }
    Seen { units, items, candidates, changes }
}

/// The files of the diffs below an entry's bottom that entries ahead of it land (docs/ci.md
/// W28-R106): a stack split at its clear prefix stays one component, its later part after the rest.
async fn below_files(hub: &Hub, e: &Entry, by_diff: &HashMap<String, BTreeSet<String>>) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let Some(bottom) = e.diffs.first() else { return out };
    let mut at = diffs::find(hub, bottom).await.ok().flatten().and_then(|d| d.parent);
    let mut n = 0;
    while let Some(name) = at.take() {
        n += 1;
        if n > kiln_ci::review::MAX_STACK || e.pinned.iter().any(|p| p.diff == name) {
            break;
        }
        let Some(files) = by_diff.get(&name) else { break };
        out.extend(files.iter().cloned());
        at = diffs::find(hub, &name).await.ok().flatten().and_then(|d| d.parent);
    }
    out
}

/// How a candidate's checks stand: every workflow it started and every required check must pass.
async fn outcome_of(hub: &Hub, repo: &Repo, policy: &land::Policy, e: &Entry) -> Outcome {
    let Some(candidate) = e.candidate.clone() else { return Outcome::Running };
    let Ok(checks) = super::review::checks_on(hub, &repo.name, &candidate).await else { return Outcome::Running };
    let stem = |p: &str| p.rsplit('/').next().unwrap_or(p).trim_end_matches(".yml").trim_end_matches(".yaml").to_owned();
    let mut gates: Vec<String> = e.started.clone();
    gates.extend(policy.land.checks.iter().cloned());
    let mut failed: Vec<String> = Vec::new();
    let mut waiting = false;
    for g in &gates {
        match checks.get(g).or_else(|| checks.get(&stem(g))) {
            Some(Some(c)) if land::passed(c) => {}
            Some(Some(c)) => failed.push(format!("`{g}` {c}")),
            _ => waiting = true,
        }
    }
    if !failed.is_empty() {
        // A failing bench-gate says which metrics got worse (W16-R09).
        let said: Vec<String> = super::review::gates_on(hub, &repo.name, vec![candidate.clone()])
            .await
            .unwrap_or_default()
            .remove(&candidate)
            .unwrap_or_default()
            .iter()
            .filter(|g| g.get("level").and_then(Value::as_str) == Some("error"))
            .filter_map(|g| g.get("message").and_then(Value::as_str).map(str::to_owned))
            .collect();
        return Outcome::Failed(format!("{} on the candidate{}", failed.join(", "), said.iter().map(|m| format!("; {m}")).collect::<String>()));
    }
    if waiting { Outcome::Running } else { Outcome::Passed }
}

/// What the trunk changed since a candidate was built on `base` (docs/ci.md W28-R102): nothing
/// while it's there, else the files of what moved it, or why that can't be said (its base dropped
/// from the trunk's history).
async fn since_of(hub: &Hub, path: &str, base: &str, tip: &str) -> Result<Option<BTreeSet<String>>, String> {
    if base == tip {
        return Ok(None);
    }
    if gitops::run_git(hub, &["-C", path, "merge-base", "--is-ancestor", base, tip], LS_REMOTE_TIMEOUT).await.is_err() {
        return Err("the trunk was rewritten since it was built".into());
    }
    files_between(hub, path, base, tip).await.map(Some).map_err(|e| format!("what moved the trunk since couldn't be read ({e:#})"))
}

/// How the trunk moved for a candidate (docs/ci.md W28-R102, R103, R108).
struct Move {
    from: String,
    to: String,
    /// Its entries were applied again onto a trunk that moved since, their result standing.
    rebased: bool,
    candidate: String,
    base: String,
}

/// A candidate that passed lands (docs/ci.md W28-R102 to R104): Review's gate is asked again of each
/// entry just before the move, and one it doesn't clear leaves (held, its clear prefix tested on
/// its own, or back to Review), the rest tested again without it. Then the trunk moves once, by
/// compare-and-swap, taking every entry: to the candidate where it was built, or, when the trunk
/// moved since only for changes the candidate shares nothing with, to its entries applied again
/// onto the tip, its result standing; else it's tested again.
#[allow(clippy::too_many_arguments)]
async fn land_step(hub: &Arc<Hub>, repo: &Repo, path: &str, trunk: &str, policy: &land::Policy, seen: &Seen, batch: &str, ids: &[String]) -> anyhow::Result<()> {
    let mut es = Vec::new();
    for id in ids {
        if let Some(e) = entry_by_id(hub, id).await?
            && e.state == "testing"
            && e.batch.as_deref() == Some(batch)
        {
            es.push(e);
        }
    }
    let labels = |list: &[Entry]| list.iter().filter_map(|e| seen.changes.get(&e.id).map(|c| c.id.clone())).collect::<Vec<_>>().join(", ");
    if es.len() != ids.len() {
        drop_candidate(hub, path, trunk, batch).await;
        return requeue_with(hub, &es, Some("tested again, without what left the queue")).await;
    }
    let Some((candidate, base)) = es.first().and_then(|e| e.candidate.clone().zip(e.base.clone())) else {
        return requeue(hub, &es).await;
    };
    let mut left: Vec<Entry> = Vec::new();
    for e in &es {
        let ds = diffs_of(hub, e).await?;
        let (gates, fails) = gate_entry(hub, e, &ds).await?;
        if fails.is_empty() && gates.iter().all(|(_, g)| g.ready()) {
            continue;
        }
        if not_cleared(hub, e, gates, &fails).await? {
            requeue_with(hub, std::slice::from_ref(e), Some("tested again without the diffs above it that hold")).await?;
        }
        left.push(e.clone());
    }
    if !left.is_empty() {
        drop_candidate(hub, path, trunk, batch).await;
        let rest: Vec<Entry> = es.into_iter().filter(|e| !left.iter().any(|l| l.id == e.id)).collect();
        let why = format!("tested again, without {}: Review's gate didn't clear it just before the move", labels(&left));
        return requeue_with(hub, &rest, Some(why.as_str())).await;
    }
    let tip = rev_parse(hub, path, &format!("refs/heads/{trunk}^{{commit}}")).await?;
    let mut to = candidate.clone();
    let mut commits: Vec<BTreeMap<String, String>> = es.iter().map(|e| e.commits.clone()).collect();
    let mut rebased = false;
    if tip != base {
        let since = since_of(hub, path, &base, &tip).await;
        let changes: Vec<Change> = es.iter().filter_map(|e| seen.changes.get(&e.id).cloned()).collect();
        if let Err(why) = queue::stands(&seen.units, &since, changes) {
            drop_candidate(hub, path, trunk, batch).await;
            let why = format!("{}: passed, but {why}: tested again on the trunk as it is", labels(&es));
            return requeue_with(hub, &es, Some(why.as_str())).await;
        }
        match reapply(hub, path, &tip, &es).await {
            Ok((t, c)) => {
                to = t;
                commits = c;
                rebased = true;
            }
            Err(why) => {
                drop_candidate(hub, path, trunk, batch).await;
                return requeue_with(hub, &es, Some(why.as_str())).await;
            }
        }
    }
    let moved = gitops::run_git(hub, &["-C", path, "update-ref", "-m", "kiln land", &format!("refs/heads/{trunk}"), &to, &tip], LS_REMOTE_TIMEOUT).await;
    drop_candidate(hub, path, trunk, batch).await;
    if moved.is_err() {
        tracing::info!(repo = %repo.name, %trunk, "the trunk moved while its candidate landed: building another");
        return requeue_with(hub, &es, Some("the trunk moved as it landed: built again")).await;
    }
    let mv = Move { from: tip, to, rebased, candidate, base };
    let batch: Vec<(Entry, BTreeMap<String, String>)> = es.into_iter().zip(commits).collect();
    landed(hub, repo, path, trunk, &batch, &mv, policy).await
}

/// A candidate's entries applied again onto the trunk's tip, as tested (docs/ci.md W28-R102): the
/// new tip and the commit carrying each entry's diffs, or why not.
async fn reapply(hub: &Hub, path: &str, tip: &str, es: &[Entry]) -> Result<(String, Vec<BTreeMap<String, String>>), String> {
    let mut at = tip.to_owned();
    let mut all = Vec::new();
    for e in es {
        match apply(hub, path, &at, e).await {
            Ok(a) => {
                at = a.tip;
                all.push(a.commits);
            }
            Err(Refusal::Conflict(files)) => return Err(format!("applied again onto the trunk's tip, it conflicts in {}: tested again", files.join(", "))),
            Err(Refusal::Other(why)) => return Err(format!("applied again onto the trunk's tip: {why}")),
        }
    }
    Ok((at, all))
}

/// A component's new candidate (docs/ci.md W28-R100, R101): its entries applied onto the trunk's tip
/// in order. One that conflicts with the trunk itself goes back to Review (R107); one that
/// conflicts only with an entry ahead of it waits for that one to land, the candidate ending before
/// it. Kept at its ref with its checks started, its entries under test.
#[allow(clippy::too_many_arguments)]
async fn build_step(hub: &Arc<Hub>, repo: &Repo, path: &str, trunk: &str, policy: &land::Policy, ids: &[String], why: &str) -> anyhow::Result<()> {
    let mut es = Vec::new();
    for id in ids {
        match entry_by_id(hub, id).await? {
            Some(e) if e.state == "queued" => es.push(e),
            _ => break,
        }
    }
    if es.is_empty() {
        return Ok(());
    }
    let base = rev_parse(hub, path, &format!("refs/heads/{trunk}^{{commit}}")).await?;
    let mut tip = base.clone();
    let mut built: Vec<(Entry, Applied)> = Vec::new();
    for e in es {
        match apply(hub, path, &tip, &e).await {
            Ok(a) => {
                tip = a.tip.clone();
                built.push((e, a));
            }
            Err(Refusal::Conflict(files)) if built.is_empty() => {
                return_to_review(hub, &e, &files).await?;
                break;
            }
            Err(Refusal::Conflict(files)) => {
                let ahead = names_of(hub, &built.iter().flat_map(|(b, _)| b.diffs.clone()).collect::<Vec<_>>()).await.join(", ");
                let note = format!("conflicts with {ahead}, ahead of it, in {}: its candidate comes once they land", files.join(", "));
                let id = e.id.clone();
                hub.store.call(move |c| Ok(c.execute("UPDATE land_queue SET reason = ?2 WHERE id = ?1", params![id, note]).map(|_| ())?)).await?;
                break;
            }
            Err(Refusal::Other(why)) => {
                eject(hub, &e, &why, "UNKNOWN_REMOVAL_REASON").await?;
                break;
            }
        }
    }
    if built.is_empty() {
        kick();
        return Ok(());
    }
    let batch = crate::util::new_id();
    gitops::run_git(hub, &["-C", path, "update-ref", &format!("refs/kiln/land/{trunk}/{batch}"), &tip], LS_REMOTE_TIMEOUT).await?;
    let mut names = Vec::new();
    for (e, _) in &built {
        names.extend(names_of(hub, &e.diffs).await);
    }
    let ctx = super::events::facts(hub, repo).await.repo;
    let head = commit_of(hub, path, &tip).await;
    // A candidate is held to each of its landers: a secret only every one of them may use.
    let landers: Vec<Value> = built.iter().filter_map(|(e, _)| e.started_by.clone()).collect();
    let started_by = (!landers.is_empty()).then(|| json!({ "name": "the batch's landers", "all": landers }));
    let events = candidate_events(&ctx, repo, trunk, &base, &head, &batch, &names, started_by);
    let started = start_checks(hub, repo, &events).await.unwrap_or_else(|e| {
        tracing::warn!(repo = %repo.name, error = %format!("{e:#}"), "the candidate's workflows didn't start");
        Vec::new()
    });
    let missing = uncovered(&policy.land.checks, &started, &BTreeMap::new());
    if !missing.is_empty() {
        drop_candidate(hub, path, trunk, &batch).await;
        for (e, _) in &built {
            eject(hub, e, &format!("{} must pass on the candidate, and no workflow runs it there: add `on: merge_group` (or kiln's `on: land`)", missing.iter().map(|m| format!("`{m}`")).collect::<Vec<_>>().join(", ")), "BRANCH_PROTECTIONS").await?;
        }
        return Ok(());
    }
    let started_json = serde_json::to_string(&started).unwrap_or_default();
    for (e, a) in &built {
        let (id, b, base2, cand, st, commits) = (e.id.clone(), batch.clone(), base.clone(), tip.clone(), started_json.clone(), serde_json::to_string(&a.commits).unwrap_or_default());
        hub.store
            .call(move |c| {
                c.execute(
                    "UPDATE land_queue SET state = 'testing', batch = ?2, base = ?3, candidate = ?4, checks = ?5, commits = ?6, bisect = NULL, reason = NULL, tested = COALESCE(tested, 0) + 1, updated_at = ?7 WHERE id = ?1",
                    params![id, b, base2, cand, st, commits, crate::util::now_unix()],
                )?;
                Ok(())
            })
            .await?;
    }
    tracing::info!(repo = %repo.name, %trunk, candidate = %tip, diffs = %names.join(" "), checks = started.len(), %why, "a land candidate is under test");
    for (e, _) in &built {
        tell(hub, e, "testing", None).await;
    }
    // Nothing to wait for: the next step lands it.
    if started.is_empty() {
        kick();
    }
    Ok(())
}

/// A candidate's ref, gone.
async fn drop_candidate(hub: &Hub, path: &str, trunk: &str, batch: &str) {
    let _ = gitops::run_git(hub, &["-C", path, "update-ref", "-d", &format!("refs/kiln/land/{trunk}/{batch}")], LS_REMOTE_TIMEOUT).await;
}

/// Entries back in the queue, to be built again.
async fn requeue(hub: &Hub, list: &[Entry]) -> anyhow::Result<()> {
    requeue_with(hub, list, None).await
}

/// Entries back in the queue, to be built again, saying why.
async fn requeue_with(hub: &Hub, list: &[Entry], reason: Option<&str>) -> anyhow::Result<()> {
    let ids: Vec<String> = list.iter().map(|e| e.id.clone()).collect();
    let why = reason.map(str::to_owned);
    hub.store
        .call(move |c| {
            for id in &ids {
                c.execute(
                    "UPDATE land_queue SET state = 'queued', batch = NULL, base = NULL, candidate = NULL, checks = NULL, commits = NULL, reason = ?3, updated_at = ?2 WHERE id = ?1 AND state IN ('queued', 'testing')",
                    params![id, crate::util::now_unix(), why],
                )?;
            }
            Ok(())
        })
        .await?;
    kick();
    Ok(())
}

/// A bisection's bound on a component's next candidate, kept on its head (docs/ci.md W28-R102).
async fn set_bisect(hub: &Hub, head: &str, next: u32) -> anyhow::Result<()> {
    let id = head.to_owned();
    hub.store.call(move |c| Ok(c.execute("UPDATE land_queue SET bisect = ?2 WHERE id = ?1", params![id, next]).map(|_| ())?)).await
}

/// Where each entry stands in its component, kept on it (docs/ci.md W28-R100, R101, R108): its
/// component (by its head), its place there, its component's window, and its decision against each
/// entry ahead of it.
async fn keep(hub: &Hub, standings: &BTreeMap<String, queue::Standing>) -> anyhow::Result<()> {
    let rows: Vec<(String, String, u32, u32, String)> = standings.iter().map(|(id, s)| (id.clone(), s.component.clone(), s.position, s.window, serde_json::to_string(&s.why).unwrap_or_default())).collect();
    hub.store
        .call(move |c| {
            for (id, component, position, window, why) in &rows {
                c.execute("UPDATE land_queue SET component = ?2, position = ?3, window_size = ?4, verdict = ?5 WHERE id = ?1 AND state IN ('queued', 'testing')", params![id, component, position, window, why])?;
            }
            Ok(())
        })
        .await
}

/// The trunk moved (`mv`): the entries' diffs landed, each by its commit, each entry's land kept
/// (docs/ci.md W28-R108); the open diffs above them restacked.
async fn landed(hub: &Arc<Hub>, repo: &Repo, path: &str, trunk: &str, batch: &[(Entry, BTreeMap<String, String>)], mv: &Move, policy: &land::Policy) -> anyhow::Result<()> {
    let tip = mv.to.as_str();
    let runs = runs_on(hub, &repo.id, &mv.candidate).await.unwrap_or_default();
    let mut all = Vec::new();
    // Each diff's closing event is sent by whoever asked it to land.
    let mut landed_by: HashMap<String, String> = HashMap::new();
    for (e, commits) in batch {
        let (id, commits2, list) = (e.id.clone(), commits.clone(), serde_json::to_string(commits).unwrap_or_default());
        hub.store
            .call(move |c| {
                let now = crate::util::now_unix();
                c.execute("UPDATE land_queue SET state = 'landed', commits = ?3, reason = NULL, updated_at = ?2 WHERE id = ?1", params![id, now, list])?;
                for (diff, commit) in &commits2 {
                    c.execute("UPDATE diffs SET state = 'landed', landed_commit = ?2, landed_at = ?3, updated_at = ?3 WHERE id = ?1", params![diff, commit, now])?;
                }
                Ok(())
            })
            .await?;
        if let Err(err) = record(hub, &repo.id, trunk, e, commits, Some(mv), policy, &runs, queue::Route::Kiln.name()).await {
            tracing::warn!(entry = %e.id, error = %format!("{err:#}"), "a land's record couldn't be kept");
        }
        all.extend(e.diffs.iter().cloned());
        landed_by.extend(e.diffs.iter().map(|d| (d.clone(), e.enqueued_by.clone())));
        hub.store.audit("kiln".into(), "queue.land".into(), format!("{} {trunk} at {tip}: entry {} by {}{}", repo.name, e.id, e.enqueued_by, if mv.rebased { ", as tested onto a trunk that moved since" } else { "" })).await;
        tell(hub, e, "landed", None).await;
        // For rules on `diff.landed`: the issues a diff names hear it landed (W11-R07).
        for (diff, commit) in commits {
            crate::rt::spawn(super::notify::diff_landed(Arc::clone(hub), diff.clone(), commit.clone()));
        }
        queue_event(hub, &e.diffs, "dequeued", Some("MERGE"), &e.enqueued_by).await;
    }
    tracing::info!(repo = %repo.name, %trunk, %tip, diffs = all.len(), rebased = mv.rebased, "landed");
    // For whoever follows lands (deploy's releases of origin `git`, docs/deploy-api.md).
    let names = names_of(hub, &all).await;
    let by: Vec<String> = batch.iter().map(|(e, _)| e.enqueued_by.clone()).collect();
    hub.emit(&crate::wire::events::ServerEvent::Landed(Box::new(crate::wire::events::Landed { repo: repo.id.clone(), repo_name: repo.name.clone(), trunk: trunk.to_string(), tip: tip.to_string(), diffs: names, by })));
    super::hosting::mirror_trunk(hub, repo, path, trunk);
    // The trunk's push runs (the poller sees it now).
    crate::gitops::want(hub, &repo.id);
    for id in &all {
        if let Ok(Some(d)) = diffs::find(hub, id).await
            && let Err(e) = diffs::closed(hub, repo, &d, true, landed_by.get(id).map_or("kiln", String::as_str)).await
        {
            tracing::warn!(diff = %d.name, error = %format!("{e:#}"), "a landed diff's closing event didn't start");
        }
    }
    restack(hub, repo, path, trunk, &all, tip).await;
    // What waits on Review may apply now, or not.
    recheck();
    Ok(())
}

// ------------------------------------------------------------------ the land record

const RECORD_COLUMNS: &str = "entry_id, changes, kind, strategy, revisions, candidate, base, runs, rebased, moved_from, moved_to, landed, policy, landed_by, via";

fn record_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<queue::Record> {
    let json_of = |i: usize| -> rusqlite::Result<Value> { Ok(r.get::<_, Option<String>>(i)?.and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(Value::Null)) };
    Ok(queue::Record {
        changes: serde_json::from_value(json_of(1)?).unwrap_or_default(),
        kind: r.get(2)?,
        strategy: r.get(3)?,
        revisions: serde_json::from_value(json_of(4)?).unwrap_or_default(),
        candidate: r.get(5)?,
        base: r.get(6)?,
        runs: serde_json::from_value(json_of(7)?).unwrap_or_default(),
        rebased: r.get(8)?,
        from: r.get(9)?,
        to: r.get(10)?,
        landed: serde_json::from_value(json_of(11)?).unwrap_or_default(),
        policy: serde_json::from_value(json_of(12)?).unwrap_or_default(),
        by: r.get(13)?,
        queue: r.get(14)?,
    })
}

/// The runs of a candidate's checks (docs/ci.md W28-R108).
async fn runs_on(hub: &Hub, repo_id: &str, sha: &str) -> anyhow::Result<Vec<queue::CheckRun>> {
    let (repo_id, sha) = (repo_id.to_owned(), sha.to_owned());
    hub.store
        .call(move |c| {
            let mut st = c.prepare("SELECT id, pipeline, conclusion FROM runs WHERE repo_id = ?1 AND sha = ?2 ORDER BY created_at, number")?;
            let rows = st.query_map(params![repo_id, sha], |r| Ok(queue::CheckRun { run: r.get(0)?, workflow: r.get(1)?, conclusion: r.get(2)? }))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
}

/// What a land keeps (docs/ci.md W28-R108), one row per entry: its changes and their kind, the
/// revisions approved with their patch-ids, the candidate tested and the trunk it was built on, its
/// checks' runs, whether it landed as tested onto a trunk that moved since, the trunk's move, the
/// commit carrying each change, the trunk's policy, who asked, and which queue landed it.
#[allow(clippy::too_many_arguments)]
async fn record(hub: &Hub, repo_id: &str, trunk: &str, e: &Entry, landed: &BTreeMap<String, String>, mv: Option<&Move>, policy: &land::Policy, runs: &[queue::CheckRun], via: &str) -> anyhow::Result<()> {
    let ds = diffs_of(hub, e).await?;
    let changes: Vec<String> = match &e.host {
        Some(h) => h.changes.iter().map(|c| format!("#{}", c.number)).collect(),
        None => ds.iter().map(|d| d.name.clone()).collect(),
    };
    let kind = match (&e.host, ds.as_slice()) {
        (Some(_), _) => "host".to_owned(),
        (None, [one]) => one.kind.clone(),
        _ => "stack".to_owned(),
    };
    let by_name: BTreeMap<String, String> = landed.iter().map(|(id, c)| (ds.iter().find(|d| &d.id == id).map_or_else(|| id.clone(), |d| d.name.clone()), c.clone())).collect();
    let r = queue::Record {
        changes,
        kind,
        strategy: e.strategy.as_str().to_owned(),
        revisions: e.pinned.clone(),
        candidate: mv.map(|m| m.candidate.clone()),
        base: mv.map(|m| m.base.clone()),
        runs: runs.to_vec(),
        rebased: mv.is_some_and(|m| m.rebased),
        from: mv.map(|m| m.from.clone()).unwrap_or_default(),
        to: mv.map(|m| m.to.clone()).unwrap_or_default(),
        landed: by_name,
        policy: policy.clone(),
        by: e.enqueued_by.clone(),
        queue: via.to_owned(),
    };
    let (id, repo_id, trunk) = (e.id.clone(), repo_id.to_owned(), trunk.to_owned());
    hub.store
        .call(move |c| {
            c.execute(
                "INSERT OR REPLACE INTO land_records (entry_id, repo_id, trunk, changes, kind, strategy, revisions, candidate, base, runs, rebased, moved_from, moved_to, landed, policy, landed_by, via, landed_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
                params![
                    id,
                    repo_id,
                    trunk,
                    serde_json::to_string(&r.changes)?,
                    r.kind,
                    r.strategy,
                    serde_json::to_string(&r.revisions)?,
                    r.candidate,
                    r.base,
                    serde_json::to_string(&r.runs)?,
                    r.rebased,
                    r.from,
                    r.to,
                    serde_json::to_string(&r.landed)?,
                    serde_json::to_string(&r.policy)?,
                    r.by,
                    r.queue,
                    crate::util::now_unix()
                ],
            )?;
            Ok(())
        })
        .await
}

/// The open diffs standing on `landed` ones, onto `tip` as rebase-only versions, each on the one
/// below it; a chain that conflicts stays as it was (its author syncs it).
async fn restack(hub: &Arc<Hub>, repo: &Repo, path: &str, trunk: &str, landed: &[String], tip: &str) {
    let children = |parents: Vec<String>| {
        let hub = hub.clone();
        async move {
            hub.store
                .call(move |c| {
                    let mut out = Vec::new();
                    let mut st = c.prepare("SELECT id FROM diffs WHERE parent_diff = ?1 AND state NOT IN ('landed', 'abandoned', 'landing')")?;
                    for p in &parents {
                        out.extend(st.query_map([p], |r| r.get::<_, String>(0))?.filter_map(Result::ok));
                    }
                    Ok(out)
                })
                .await
                .unwrap_or_default()
        }
    };
    // (diff, the commit it goes on, the diff below it now).
    let mut work: Vec<(String, String, Option<String>)> = children(landed.to_vec()).await.into_iter().map(|id| (id, tip.to_owned(), None)).collect();
    let mut seen = 0usize;
    while let Some((id, onto, below)) = work.pop() {
        seen += 1;
        if seen > 500 {
            break;
        }
        let Ok(Some(d)) = diffs::find(hub, &id).await else { continue };
        let Some(v) = diffs::versions(hub, &d).await.ok().and_then(|mut v| v.pop()) else { continue };
        let tree = match pick_tree(hub, path, &onto, &v.commit, &v.parent).await {
            Ok(t) => t,
            Err(_) => {
                tracing::info!(diff = %d.name, "a diff above a landed one doesn't restack cleanly; its author syncs it");
                continue;
            }
        };
        let Ok(who) = author(hub, path, &v.commit).await else { continue };
        let message = gitops::run_git(hub, &["-C", path, "log", "-1", "--format=%B", &v.commit], LS_REMOTE_TIMEOUT).await.map(|o| String::from_utf8_lossy(&o).into_owned()).unwrap_or_default();
        // The commit kiln makes carries its change's `Kiln-Change` (W26-R03).
        let message = kiln_ci::identity::with_kiln_changes(&message, &changes_of(&[&d]));
        let Ok(c) = commit_tree(hub, path, &tree, &[&onto], &who, &message).await else { continue };
        let patch = diffs::git_run(hub, &["-C", path, "show", "--no-color", &c], &[], Vec::new()).await.ok().map(|r| r.out).unwrap_or_default();
        let patch_id = diffs::git_with_input(hub, &["-C", path, "patch-id", "--stable"], patch).await.ok().and_then(|o| String::from_utf8_lossy(&o).split_whitespace().next().map(str::to_owned)).unwrap_or_default();
        let nv = diffs::NewVersion { commit: c.clone(), parent: onto.clone(), base: tip.to_owned(), patch_id, message, note: Some(format!("restacked onto {trunk} after a land")), rebase_only: true, pushed_by: "kiln".into(), parent_diff: below.clone() };
        match diffs::add_version(hub, repo, path, &d, nv).await {
            Ok(n) => tracing::info!(diff = %d.name, version = n, "restacked onto the trunk"),
            Err(e) => {
                tracing::warn!(diff = %d.name, error = %format!("{e:#}"), "a restack's version couldn't be kept");
                continue;
            }
        }
        for child in children(vec![d.id.clone()]).await {
            work.push((child, c.clone(), Some(d.id.clone())));
        }
    }
}

// ------------------------------------------------------------------ a host's own queue

/// An entry on its host's own queue (docs/ci.md W28-R109): the repository's name there and each of
/// its changes, bottom first.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
struct HostEntry {
    repo: String,
    changes: Vec<HostChange>,
}

/// One of an entry's changes on its host, as last read through the forge seam.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
struct HostChange {
    number: u64,
    /// The head it lands at, as Review asked.
    head: String,
    /// Its last read's ETag: a read of what hasn't changed since costs nothing.
    etag: Option<String>,
    /// Put in its host's queue (or seen there), and whether the queue holds it now.
    entered: bool,
    queued: bool,
    /// `open`, `merged` or `closed`, and the commit it merged as.
    state: String,
    commit: Option<String>,
}

impl HostChange {
    fn on_host(&self) -> queue::OnHost {
        let state = match self.state.as_str() {
            "merged" => queue::HostState::Merged { commit: self.commit.clone().unwrap_or_default() },
            "closed" => queue::HostState::Closed,
            _ => queue::HostState::Open,
        };
        queue::OnHost { number: self.number, entered: self.entered, queued: self.queued, state }
    }
}

/// A forge, and a repository's name on its host.
type Reached = (Arc<dyn Forge>, String);

/// Forges set for repositories (by id) in place of their hosts': a test's simulator.
fn forges() -> &'static parking_lot::Mutex<HashMap<String, Reached>> {
    static F: OnceLock<parking_lot::Mutex<HashMap<String, Reached>>> = OnceLock::new();
    F.get_or_init(Default::default)
}

/// The forge seam a repository's changes are reached through (docs/ci.md W28-R109), and its name on
/// its host: one set for it, else its host's (githost implements the seam). Kiln's own git server
/// has none: its repositories land on kiln's queue.
async fn forge_of(hub: &Arc<Hub>, repo: &Repo) -> Option<Reached> {
    if let Some(f) = forges().lock().get(&repo.id).cloned() {
        return Some(f);
    }
    let (f, name) = crate::githost::forge::for_repo(hub, repo).await?;
    Some((Arc::new(f) as Arc<dyn Forge>, name))
}

/// A host queue's name, as the land record and the queue show it.
fn queue_name(k: seam::QueueKind) -> &'static str {
    match k {
        seam::QueueKind::MergeQueue => "merge queue",
        seam::QueueKind::MergeTrain => "merge train",
    }
}

/// Which queue lands `repo`'s changes (docs/ci.md W28-R109): its host's, where the forge seam says it
/// has one on (`abilities(repo).queue`), else kiln's.
async fn route_of(hub: &Arc<Hub>, repo: &Repo) -> (queue::Route, Option<Reached>) {
    let Some((forge, name)) = forge_of(hub, repo).await else { return (queue::Route::Kiln, None) };
    match forge.abilities(&name).await {
        Ok(a) => (queue::Route::of(a.queue.map(queue_name)), Some((forge, name))),
        Err(e) => {
            tracing::warn!(repo = %repo.name, error = %e, "the repository's abilities on its host couldn't be read: kiln's queue lands it");
            (queue::Route::Kiln, Some((forge, name)))
        }
    }
}

/// Changes on a repository's host into its host's own queue (docs/ci.md W28-R109), as `by` asks:
/// bottom first, each by its number and the head Review cleared. Refused where the forge seam says
/// its host has no queue for it (kiln's queue lands the diffs of repositories it hosts).
pub async fn enqueue_host(hub: &Arc<Hub>, repo: &Repo, trunk: &str, changes: &[(u64, String)], by: &str) -> Result<Value, Fail> {
    if changes.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "no changes to land".into()));
    }
    let (route, reached) = route_of(hub, repo).await;
    let (queue::Route::Host { queue: q }, Some((_, name))) = (route, reached) else {
        return Err((StatusCode::UNPROCESSABLE_ENTITY, format!("{} has no queue on its host: kiln's queue lands the diffs of repositories it hosts", repo.name)));
    };
    let host = HostEntry { repo: name, changes: changes.iter().map(|(n, head)| HostChange { number: *n, head: head.clone(), state: "open".into(), ..HostChange::default() }).collect() };
    let pinned: Vec<Pin> = changes.iter().map(|(n, head)| Pin { diff: format!("#{n}"), commit: head.clone(), ..Pin::default() }).collect();
    let id = insert(
        hub,
        New {
            repo_id: repo.id.clone(),
            trunk: trunk.to_owned(),
            diffs: Vec::new(),
            strategy: Strategy::Merge,
            by: by.to_owned(),
            started_by: None,
            state: "queued",
            hold: None,
            reason: None,
            conflicts: Vec::new(),
            pinned,
            route: q.clone(),
            host: Some(host),
            queued_at: crate::util::now_unix(),
        },
    )
    .await
    .map_err(fail)?;
    let numbers: Vec<u64> = changes.iter().map(|(n, _)| *n).collect();
    hub.store.audit(by.to_owned(), "queue.host".into(), format!("{} {numbers:?} into its host's {q}: entry {id}", repo.name)).await;
    kick();
    Ok(json!({ "entry": id, "route": q, "changes": numbers, "state": "queued" }))
}

/// How often what's on a host's queue is read back unasked, at most (a read of the queue counts
/// against the host's rate limit; a change's conditional read costs nothing while it's unchanged).
const HOST_QUEUE_EVERY: Duration = Duration::from_secs(30);

/// One step of a repository's entries on its host's own queue (docs/ci.md W28-R109), through the
/// forge seam alone: each change put in (`enqueue`, the queue read back after, as a stacked change
/// takes those below it), followed (`change`, read conditionally; `queued`, read back when one
/// moved), and the entry landed once every change merged, out once one left the queue unmerged. An
/// entry kiln's queue would land, of a repository kiln doesn't host, is ejected, as before.
async fn host_step(hub: &Arc<Hub>, repo: &Repo, trunk: &str) -> anyhow::Result<()> {
    let live = entries(hub, &repo.id, trunk, &["queued", "testing"]).await?;
    let (on_host, rest): (Vec<Entry>, Vec<Entry>) = live.into_iter().partition(|e| e.host.is_some());
    for e in rest {
        eject(hub, &e, "the repository isn't hosted here", "UNKNOWN_REMOVAL_REASON").await?;
    }
    if on_host.is_empty() {
        return Ok(());
    }
    let Some((forge, _)) = forge_of(hub, repo).await else {
        for e in on_host {
            eject(hub, &e, "its host can't be reached through the forge seam", "UNKNOWN_REMOVAL_REASON").await?;
        }
        return Ok(());
    };
    for e in on_host {
        if let Err(err) = follow_host(hub, repo, trunk, forge.as_ref(), &e).await {
            tracing::warn!(entry = %e.id, error = %format!("{err:#}"), "an entry on its host's queue couldn't be followed");
        }
    }
    Ok(())
}

/// What the host's queue holds, read back onto an entry's changes.
async fn read_queued(forge: &dyn Forge, h: &mut HostEntry) -> Result<(), seam::Refusal> {
    let q = forge.queued(&h.repo).await?;
    for c in &mut h.changes {
        c.queued = q.contains(&c.number);
        if c.queued {
            c.entered = true;
        }
    }
    Ok(())
}

async fn follow_host(hub: &Arc<Hub>, repo: &Repo, trunk: &str, forge: &dyn Forge, e: &Entry) -> anyhow::Result<()> {
    let Some(mut h) = e.host.clone() else { return Ok(()) };
    let before = h.clone();
    let name = h.repo.clone();
    let mut read_back = due(&format!("host:{}", e.id), HOST_QUEUE_EVERY);
    let ons: Vec<queue::OnHost> = h.changes.iter().map(HostChange::on_host).collect();
    if let Some(n) = queue::next_to_enqueue(&ons) {
        let head = h.changes.iter().find(|c| c.number == n).map(|c| c.head.clone()).unwrap_or_default();
        match forge.enqueue(&name, n, &head).await {
            Ok(_) => {
                if let Some(c) = h.changes.iter_mut().find(|c| c.number == n) {
                    c.entered = true;
                }
                read_back = true;
            }
            Err(seam::Refusal::Moved { head }) => return eject(hub, e, &format!("#{n} moved on its host (its head is {head}), past what Review cleared"), "INVALID_MERGE_COMMIT").await,
            // Merged or closed already: its read says which.
            Err(seam::Refusal::Closed { .. }) => {}
            Err(other) => anyhow::bail!("#{n} couldn't go in its host's queue: {other}"),
        }
    }
    for c in &mut h.changes {
        match forge.change(&name, c.number, c.etag.as_deref()).await {
            Ok(seam::Fresh::Changed { value, etag }) => {
                c.etag = Some(etag);
                read_back = true;
                match value.state {
                    seam::State::Open => c.state = "open".into(),
                    seam::State::Merged { commit } => {
                        c.state = "merged".into();
                        c.commit = Some(commit);
                    }
                    seam::State::Closed => c.state = "closed".into(),
                }
            }
            Ok(seam::Fresh::NotModified) => {}
            Err(err) => tracing::warn!(entry = %e.id, number = c.number, error = %err, "a change on its host couldn't be read"),
        }
    }
    if read_back {
        rechecked().lock().insert(format!("host:{}", e.id), Instant::now());
        if let Err(err) = read_queued(forge, &mut h).await {
            tracing::warn!(entry = %e.id, error = %err, "its host's queue couldn't be read back");
        }
    }
    if h != before {
        let (id, text) = (e.id.clone(), serde_json::to_string(&h)?);
        hub.store.call(move |c| Ok(c.execute("UPDATE land_queue SET host = ?2, state = 'testing', updated_at = ?3 WHERE id = ?1", params![id, text, crate::util::now_unix()]).map(|_| ())?)).await?;
    }
    let ons: Vec<queue::OnHost> = h.changes.iter().map(HostChange::on_host).collect();
    match queue::on_host(&ons) {
        queue::HostVerdict::Landed { commits } => {
            let landed: BTreeMap<String, String> = commits.into_iter().map(|(n, c)| (format!("#{n}"), c)).collect();
            let (id, list) = (e.id.clone(), serde_json::to_string(&landed)?);
            hub.store.call(move |c| Ok(c.execute("UPDATE land_queue SET state = 'landed', commits = ?2, updated_at = ?3 WHERE id = ?1", params![id, list, crate::util::now_unix()]).map(|_| ())?)).await?;
            let via = e.route.clone().unwrap_or_default();
            let policy = land::Policy::default();
            if let Err(err) = record(hub, &repo.id, trunk, e, &landed, None, &policy, &[], &via).await {
                tracing::warn!(entry = %e.id, error = %format!("{err:#}"), "a land's record couldn't be kept");
            }
            hub.store.audit("kiln".into(), "queue.land".into(), format!("{} {trunk}: entry {} by {}, through its host's {via}", repo.name, e.id, e.enqueued_by)).await;
            tell(hub, e, "landed", None).await;
            Ok(())
        }
        queue::HostVerdict::Out { why, .. } => eject(hub, e, &why, "UNKNOWN_REMOVAL_REASON").await,
        queue::HostVerdict::Waiting => Ok(()),
    }
}

/// An entry on its host's queue taken out of it (docs/ci.md W28-R109): its lowest change the queue
/// holds dequeued (those stacked above it go with it), the queue read back after.
async fn off_host(hub: &Arc<Hub>, e: &Entry) {
    let Some(mut h) = e.host.clone() else { return };
    let Some(repo) = hub.store.repos().await.ok().and_then(|r| r.into_iter().find(|r| r.id == e.repo_id)) else { return };
    let Some((forge, _)) = forge_of(hub, &repo).await else { return };
    let Some(n) = h.changes.iter().find(|c| c.queued).map(|c| c.number) else { return };
    if let Err(err) = forge.dequeue(&h.repo, n).await {
        tracing::warn!(entry = %e.id, number = n, error = %err, "a change couldn't leave its host's queue");
    }
    if read_queued(forge.as_ref(), &mut h).await.is_ok() {
        for c in &mut h.changes {
            c.entered = c.queued;
        }
        if let Ok(text) = serde_json::to_string(&h) {
            let id = e.id.clone();
            let _ = hub.store.call(move |c| Ok(c.execute("UPDATE land_queue SET host = ?2 WHERE id = ?1", params![id, text]).map(|_| ())?)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w14_r08_uncovered() {
        let started = vec![".github/workflows/ci.yml".to_owned()];
        let none = BTreeMap::new();
        assert!(uncovered(&["ci".into(), "ci/test".into(), ".github/workflows/ci.yml".into()], &started, &none).is_empty());
        assert_eq!(uncovered(&["lint".into(), "ci".into()], &started, &none), ["lint"]);
    }

    fn repo(id: &str, name: &str) -> Repo {
        Repo {
            id: id.into(),
            name: name.into(),
            clone_url: format!("https://github.com/{name}.git"),
            branches: Vec::new(),
            poll_seconds: 0,
            enabled: true,
            created_at: 0,
            last_polled_at: None,
            last_error: None,
            mode: "upstream".into(),
            default_branch: None,
            mirror: None,
        }
    }

    async fn state_of(hub: &Hub, entry: &str) -> anyhow::Result<(String, HostEntry)> {
        let e = entry_by_id(hub, entry).await?.ok_or_else(|| anyhow::anyhow!("no entry {entry}"))?;
        Ok((e.state, e.host.unwrap_or_default()))
    }

    /// docs/ci.md W28-R109: a repository whose host has a queue on lands there, through the forge
    /// seam alone: each change put in at the head Review cleared and the queue read back, followed
    /// until it merged, the land recorded with the host's queue; Review's hold takes it out of the
    /// host's queue and its release puts it back. Where the host has none, kiln's queue is the
    /// route, and the host's isn't asked.
    #[test]
    fn w28_r109_a_hosts_queue_through_the_forge_seam() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let sim = seam::SimForge::github();
            let (name, r) = ("acme/app", hub.store.upsert_repo(repo("r-host", "acme/app")).await?);
            sim.add_repo(name);
            sim.push(name, "one", seam::TRUNK, "one")?;
            sim.push(name, "two", seam::TRUNK, "two")?;
            let open = |head: &str| seam::Open { head: head.into(), base: seam::TRUNK.into(), title: format!("change {head}"), by: "ada".into() };
            let one = sim.open(name, open("one")).await?;
            let two = sim.open(name, open("two")).await?;
            forges().lock().insert(r.id.clone(), (Arc::new(sim.clone()) as Arc<dyn Forge>, name.to_owned()));
            // No queue on the host: kiln's is the route, and the host's queue isn't asked.
            let refused = enqueue_host(&hub, &r, seam::TRUNK, &[(one.number, one.head_sha.clone())], "user:ada").await;
            assert!(matches!(&refused, Err((StatusCode::UNPROCESSABLE_ENTITY, why)) if why.contains("no queue on its host")), "{refused:?}");
            sim.set_queue(name, true);
            let a = enqueue_host(&hub, &r, seam::TRUNK, &[(one.number, one.head_sha.clone())], "user:ada").await.map_err(|(_, m)| anyhow::anyhow!(m))?;
            let b = enqueue_host(&hub, &r, seam::TRUNK, &[(two.number, two.head_sha.clone())], "user:ada").await.map_err(|(_, m)| anyhow::anyhow!(m))?;
            assert_eq!(a.get("route").and_then(Value::as_str), Some("merge queue"));
            let (ea, eb) = (a.get("entry").and_then(Value::as_str).unwrap_or_default().to_owned(), b.get("entry").and_then(Value::as_str).unwrap_or_default().to_owned());
            host_step(&hub, &r, seam::TRUNK).await?;
            assert_eq!(sim.queued(name).await?, vec![one.number, two.number], "each put in, in queue order");
            let (_, h) = state_of(&hub, &ea).await?;
            assert!(h.changes.iter().all(|c| c.entered && c.queued), "read back: {h:?}");
            // Review holds the second: out of the host's queue, read back; let go, it goes in again.
            hold(&hub, &eb, "user:rev", Some("not today")).await.map_err(|(_, m)| anyhow::anyhow!(m))?;
            assert_eq!(sim.queued(name).await?, vec![one.number]);
            assert_eq!(state_of(&hub, &eb).await?.0, "held");
            release(&hub, &eb, "user:rev").await.map_err(|(_, m)| anyhow::anyhow!(m))?;
            host_step(&hub, &r, seam::TRUNK).await?;
            assert_eq!(sim.queued(name).await?, vec![one.number, two.number], "released, it's queued again");
            // The host's queue lands both: each entry lands, its record naming the host's queue.
            assert_eq!(sim.land_queue(name)?, vec![one.number, two.number]);
            host_step(&hub, &r, seam::TRUNK).await?;
            for e in [&ea, &eb] {
                assert_eq!(state_of(&hub, e).await?.0, "landed", "{e}");
            }
            let id = ea.clone();
            let kept = hub.store.call(move |c| Ok(c.query_row(&format!("SELECT {RECORD_COLUMNS} FROM land_records WHERE entry_id = ?1"), [&id], record_row)?)).await?;
            assert_eq!((kept.kind.as_str(), kept.queue.as_str(), kept.changes.clone()), ("host", "merge queue", vec![format!("#{}", one.number)]));
            assert!(kept.landed.get(&format!("#{}", one.number)).is_some_and(|c| sim.contains(name, seam::TRUNK, c)), "{kept:?}");
            Ok(())
        })
    }
}
