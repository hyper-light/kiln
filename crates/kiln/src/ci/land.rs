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
        Some(Gate::Conflict { files, .. }) => Some(files.clone()),
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

/// A commit of `tree` on `parents`, by `who`, committed and signed by kiln (W27-R03: as kiln's
/// server; with `[ci.land_signing]`'s key instead when it names one), saying `message`.
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
    args.push(if signing.is_some() { "-S" } else { "--no-gpg-sign" });
    args.extend(["-F", "-"]);
    let env = [("GIT_AUTHOR_NAME", who.0.as_str()), ("GIT_AUTHOR_EMAIL", who.1.as_str()), ("GIT_AUTHOR_DATE", who.2.as_str()), ("GIT_COMMITTER_NAME", "kiln"), ("GIT_COMMITTER_EMAIL", "kiln@localhost")];
    let r = diffs::git_run(hub, &args, &env, message.as_bytes().to_vec()).await?;
    anyhow::ensure!(r.ok, "git commit-tree: {}", String::from_utf8_lossy(&r.err).trim());
    let sha = String::from_utf8_lossy(&r.out).trim().to_owned();
    if signing.is_some() {
        return Ok(sha);
    }
    match super::revision::sign_as_kiln(hub, path, &sha, "server").await {
        Ok((signed, _)) => Ok(signed),
        Err(e) => {
            tracing::warn!(commit = %sha, error = %format!("{e:#}"), "a commit the land queue made isn't signed");
            Ok(sha)
        }
    }
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
    // A host's changes apply as their host would merge them (docs/ci.md W26-R19): each head, bottom
    // first, merged onto the tip in kiln's mirror, the candidate a commit of kiln's.
    if let Some(h) = &e.host {
        let mut at = tip.to_owned();
        let mut commits = BTreeMap::new();
        for c in h.changes.iter().filter(|c| c.state == "open") {
            let tree = merge_tree(hub, path, &at, &c.head).await?;
            let who = author(hub, path, &c.head).await.map_err(|x| Refusal::Other(format!("{x:#}")))?;
            let made = commit_tree(hub, path, &tree, &[&at, &c.head], &who, &format!("Merge #{} into {}", c.number, e.trunk)).await.map_err(|x| Refusal::Other(format!("{x:#}")))?;
            commits.insert(format!("#{}", c.number), made.clone());
            at = made;
        }
        return Ok(Applied { tip: at, commits });
    }
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
    // kiln's queue tests the changes of kiln's own repositories, and of a host's whose host has no
    // queue (docs/ci.md W26-R19): those in kiln's mirror, their host merging each at the head
    // tested. The rest go through their host's own queue.
    let (path, merges) = match super::hosting::native_path(hub, repo) {
        Some(p) => (p.to_string_lossy().into_owned(), None),
        // A host's queue lands the entries routed to it; those routed to kiln's go on below, by
        // each entry's own route (W26-R19).
        None => {
            if let Err(err) = host_step(hub, repo, trunk).await {
                tracing::warn!(repo = %repo.name, error = %format!("{err:#}"), "the entries on its host's queue couldn't be stepped: those on kiln's go on");
            }
            match host_merges(hub, repo, trunk).await? {
                Some(reached) => (gitops::mirror_path(hub, &repo.name).to_string_lossy().into_owned(), Some(reached)),
                None => return Ok(()),
            }
        }
    };
    if merges.is_some() {
        // Each change's head in kiln's mirror, for its candidate to be built there.
        for e in entries(hub, &repo.id, trunk, &["queued"]).await? {
            for (n, head) in open_on_host(&e) {
                if let Err(err) = gitops::ensure_commit(hub, repo, &head).await {
                    tracing::warn!(entry = %e.id, number = n, error = %format!("{err:#}"), "a host's change isn't in kiln's mirror");
                }
            }
        }
    }
    let (policy, _) = super::review::policy(hub, &path, trunk).await;
    // R104 to R107: what waits on Review's gate, asked again (what Review holds waits for Review;
    // a host's change, [`host_recheck`]).
    for e in entries(hub, &repo.id, trunk, &["held", "returned"]).await? {
        if e.host.is_some() || e.hold.as_deref() == Some("review") || !due(&e.id, RECHECK) {
            continue;
        }
        if let Err(err) = recheck_entry(hub, &e).await {
            tracing::warn!(entry = %e.id, error = %format!("{err:#}"), "an entry waiting on Review couldn't be asked again");
        }
    }
    // R105: what Review approved, still.
    pins_step(hub, trunk, &repo.id).await?;
    let live = entries(hub, &repo.id, trunk, &["queued", "testing"]).await?;
    let live: Vec<Entry> = if merges.is_some() { live.into_iter().filter(|e| e.route.as_deref() == Some(queue::HOST_MERGES)).collect() } else { live };
    if live.is_empty() {
        return Ok(());
    }
    let broken: Vec<Entry> = live.iter().filter(|e| e.state == "testing" && (e.candidate.is_none() || e.base.is_none() || e.batch.is_none())).cloned().collect();
    if !broken.is_empty() {
        return requeue(hub, &broken).await;
    }
    let tip = rev_parse(hub, &path, &format!("refs/heads/{trunk}^{{commit}}")).await?;
    let mut seen = see(hub, repo, &path, &policy, &tip, &live).await;
    // A host-merges batch whose merges began is finished as it was decided (docs/ci.md W26-R19):
    // the trunk its own merges moved is no reason to test it again. The plan sees it still running,
    // its component busy, and it's finished here.
    let started = started_batches(&live);
    if merges.is_some() {
        for c in seen.candidates.iter_mut().filter(|c| started.iter().any(|(b, _)| *b == c.batch)) {
            c.outcome = Outcome::Running;
        }
    }
    let plan = queue::plan(&seen.units, &seen.items, &seen.candidates, &queue::Knobs::of_batch(policy.queue.batch));
    if let Some((forge, _)) = &merges {
        for (batch, ids) in &started {
            if let Err(err) = Box::pin(host_land_step(hub, repo, &path, trunk, &policy, forge.as_ref(), &seen, batch, ids)).await {
                tracing::warn!(repo = %repo.name, %batch, error = %format!("{err:#}"), "a batch its host is merging couldn't be finished this step");
            }
        }
    }
    for s in &plan.steps {
        let done = match s {
            Step::Land { batch, entries, why, .. } => {
                tracing::info!(repo = %repo.name, %trunk, %why, "a land candidate passed");
                match &merges {
                    Some((forge, _)) => Box::pin(host_land_step(hub, repo, &path, trunk, &policy, forge.as_ref(), &seen, batch, entries)).await,
                    None => land_step(hub, repo, &path, trunk, &policy, &seen, batch, entries).await,
                }
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
    for e in live {
        for p in &e.pinned {
            if by_diff.contains_key(&p.diff) {
                continue;
            }
            // A host's change pins no parent: its files are what it changes since it left the trunk,
            // so host changes share components as kiln's do (docs/ci.md W26-R19, W28-R35).
            let parent = if p.parent.is_empty() && e.host.is_some() { merge_base(hub, path, tip, &p.commit).await } else { p.parent.clone() };
            let files = files_of(hub, path, &parent, &p.commit).await;
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

/// Where `head` left `tip`'s history in kiln's mirror at `path`, or nothing where that can't be read.
async fn merge_base(hub: &Hub, path: &str, tip: &str, head: &str) -> String {
    gitops::run_git(hub, &["-C", path, "merge-base", tip, head], LS_REMOTE_TIMEOUT).await.map(|o| String::from_utf8_lossy(&o).trim().to_owned()).unwrap_or_default()
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
    // Signed as the queue that tested it, as it moved the trunk (W27-R07): a land kiln can't sign
    // stands, unsigned, and says so.
    let carried: Vec<(String, String)> = batch.iter().flat_map(|(e, commits)| e.diffs.iter().filter_map(|d| commits.get(d).map(|c| (d.clone(), c.clone())))).collect();
    let moved = super::lands::Moved { from: &mv.from, to: &mv.to, candidate: &mv.candidate, base: &mv.base, rebased: mv.rebased };
    if let Err(e) = Box::pin(super::lands::sign(hub, repo, trunk, &moved, &carried)).await {
        tracing::warn!(repo = %repo.name, %trunk, candidate = %mv.candidate, error = %format!("{e:#}"), "a land wasn't signed");
    }
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
    /// The background merge kiln asked its host for and hasn't seen end (W26-R19): read again on
    /// the next step, never waited on.
    #[serde(skip_serializing_if = "Option::is_none")]
    merging: Option<String>,
    /// Landed by kiln already, recorded and signed into the trail (W26-R19): its entry, back in the
    /// queue part-merged, signs only what merged since.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    landed: bool,
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
/// has one on (`abilities(repo).queue`), else kiln's, its host merging what kiln tested (W26-R19).
async fn route_of(hub: &Arc<Hub>, repo: &Repo) -> (queue::Route, Option<Reached>) {
    let Some((forge, name)) = forge_of(hub, repo).await else { return (queue::Route::Kiln, None) };
    match forge.abilities(&name).await {
        Ok(a) => (queue::Route::of_host(a.queue.map(queue_name)), Some((forge, name))),
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
    // Its host's queue, or kiln's with its host merging what kiln tested (W26-R19).
    let (q, name) = match (route, reached) {
        (queue::Route::Host { queue: q }, Some((_, name))) => (q, name),
        (queue::Route::HostMerges, Some((_, name))) => (queue::HOST_MERGES.to_owned(), name),
        _ => return Err((StatusCode::UNPROCESSABLE_ENTITY, format!("{} isn't reached through its host's forge: kiln's queue lands the diffs of repositories it hosts", repo.name))),
    };
    // Review hands a host's changes to its queue only once its gate clears each (W26-R19).
    let held = super::review::host_held(hub, repo, trunk, changes).await?;
    if !held.is_empty() {
        return Err((StatusCode::CONFLICT, format!("Review's gate holds {}", held.join("; "))));
    }
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

/// `POST /api/repos/:repo/queue` (docs/ci.md W28-R109): `{ trunk?, changes: [{ number, head }] }`,
/// a host's changes into its host's own queue, bottom first, each at the head Review cleared (the
/// one Ship shows). Each change is read on its host first: one whose head moved since is refused
/// (409, naming the head it's at), as is one merged or closed already; a repository whose host has
/// no queue for it is refused as [`enqueue_host`] refuses it (422). `trunk` is the repository's
/// default branch when left out.
pub async fn enqueue_route(hub: &Arc<Hub>, repo: &str, body: &Value, by: &str) -> Result<Value, Fail> {
    let found = hub.store.repos().await.map_err(fail)?.into_iter().find(|r| r.name == repo || r.id == repo);
    let r = found.ok_or_else(|| (StatusCode::NOT_FOUND, format!("no repository {repo}")))?;
    let mut changes = Vec::new();
    for c in body.get("changes").and_then(Value::as_array).into_iter().flatten() {
        let (Some(n), Some(head)) = (c.get("number").and_then(Value::as_u64), c.get("head").and_then(Value::as_str).filter(|h| !h.is_empty())) else {
            return Err((StatusCode::BAD_REQUEST, "each change is `{ number, head }`".into()));
        };
        changes.push((n, head.to_owned()));
    }
    let trunk = body.get("trunk").and_then(Value::as_str).map(str::to_owned).or_else(|| r.default_branch.clone()).unwrap_or_else(|| "main".into());
    if let (queue::Route::Host { .. } | queue::Route::HostMerges, Some((forge, name))) = route_of(hub, &r).await {
        for (n, head) in &changes {
            match forge.change(&name, *n, None).await {
                Ok(seam::Fresh::Changed { value, .. }) if !matches!(value.state, seam::State::Open) => {
                    return Err((StatusCode::CONFLICT, format!("#{n} is merged or closed on its host")));
                }
                Ok(seam::Fresh::Changed { value, .. }) if value.head_sha != *head => {
                    return Err((StatusCode::CONFLICT, format!("#{n} moved on its host: its head is {}, not {head}, the head Review cleared", value.head_sha)));
                }
                Ok(_) => {}
                Err(seam::Refusal::NotFound { .. }) => return Err((StatusCode::NOT_FOUND, format!("no #{n} on {name}'s host"))),
                Err(e) => return Err((StatusCode::BAD_GATEWAY, format!("#{n} couldn't be read on its host: {e}"))),
            }
        }
    }
    enqueue_host(hub, &r, &trunk, &changes, by).await
}

/// Whether `repo`'s entries on `trunk` land through kiln's queue, their host merging what kiln
/// tested (docs/ci.md W26-R19), and its forge: as their route says, so the host isn't asked each
/// step.
async fn host_merges(hub: &Arc<Hub>, repo: &Repo, trunk: &str) -> anyhow::Result<Option<Reached>> {
    let live = entries(hub, &repo.id, trunk, &["queued", "testing", "held"]).await?;
    if !live.iter().any(|e| e.route.as_deref() == Some(queue::HOST_MERGES)) {
        return Ok(None);
    }
    Ok(forge_of(hub, repo).await)
}

/// The batches under test of `live` whose host began merging them: a change merged and not landed by
/// kiln, or a background merge under way (docs/ci.md W26-R19). Each with its entries' ids.
fn started_batches(live: &[Entry]) -> Vec<(String, Vec<String>)> {
    let mut out: Vec<(String, Vec<String>)> = Vec::new();
    for e in live.iter().filter(|e| e.state == "testing") {
        let Some(b) = &e.batch else { continue };
        match out.iter_mut().find(|(x, _)| x == b) {
            Some((_, ids)) => ids.push(e.id.clone()),
            None => out.push((b.clone(), vec![e.id.clone()])),
        }
    }
    let begun = |id: &String| live.iter().find(|e| &e.id == id).is_some_and(|e| e.host.iter().flat_map(|h| h.changes.iter()).any(|c| (c.state == "merged" && !c.landed) || c.merging.is_some()));
    out.retain(|(_, ids)| ids.iter().any(begun));
    out
}

/// The merge method a host merges by, for a strategy.
fn method_of(s: Strategy) -> seam::Method {
    match s {
        Strategy::Squash => seam::Method::Squash,
        Strategy::Rebase => seam::Method::Rebase,
        Strategy::FfOnly => seam::Method::FastForward,
        _ => seam::Method::Merge,
    }
}

/// The first seven characters of a commit, as kiln says one.
fn short(sha: &str) -> &str {
    sha.get(..7).unwrap_or(sha)
}

/// Where a change kiln has its host merge stands now (docs/ci.md W26-R19), read before asking
/// again: merged (the change keeps its commit), its background merge not done yet, open at the
/// head its host has, or out (closed, or a merge its host didn't do).
enum Standing {
    Merged,
    Merging,
    Open { head: String },
    Out(String),
}

/// Change `c` as its host has it now: the background merge kiln asked for, read once and never
/// waited on, else the change itself. `c` keeps what's read.
async fn standing_of(forge: &dyn Forge, name: &str, c: &mut HostChange) -> anyhow::Result<Standing> {
    if let Some(id) = c.merging.clone() {
        match forge.merge_result(name, c.number, &id).await {
            Ok(seam::MergeResult::Pending) => return Ok(Standing::Merging),
            Ok(seam::MergeResult::Merged { commit, .. }) => {
                (c.merging, c.state, c.commit) = (None, "merged".into(), Some(commit));
                return Ok(Standing::Merged);
            }
            Ok(seam::MergeResult::Failed { why }) => {
                c.merging = None;
                return Ok(Standing::Out(format!("#{}: its host didn't merge it: {why}", c.number)));
            }
            // A merge its host no longer knows: the change itself says.
            Err(_) => c.merging = None,
        }
    }
    let seam::Fresh::Changed { value, .. } = forge.change(name, c.number, None).await? else { return Ok(Standing::Open { head: c.head.clone() }) };
    Ok(match value.state {
        seam::State::Merged { commit } => {
            (c.state, c.commit) = ("merged".into(), Some(commit));
            Standing::Merged
        }
        seam::State::Closed => {
            c.state = "closed".into();
            Standing::Out(format!("#{} was closed on its host", c.number))
        }
        seam::State::Open => Standing::Open { head: value.head_sha },
    })
}

/// The commit the last change its host merged of batch `es` merged as (bottom first, those kiln
/// landed before left out).
fn last_merged(es: &[Entry]) -> Option<String> {
    es.iter().flat_map(|e| e.host.iter().flat_map(|h| h.changes.iter())).filter(|c| c.state == "merged" && !c.landed).filter_map(|c| c.commit.clone()).next_back()
}

/// Entry `e`'s host changes, as `e` has them now, kept on its row.
async fn save_host(hub: &Hub, e: &Entry) -> anyhow::Result<()> {
    let Some(h) = &e.host else { return Ok(()) };
    let (id, text) = (e.id.clone(), serde_json::to_string(h)?);
    hub.store.call(move |c| Ok(c.execute("UPDATE land_queue SET host = ?2, updated_at = ?3 WHERE id = ?1", params![id, text, crate::util::now_unix()]).map(|_| ())?)).await
}

/// The changes of entry `e` its host merged that kiln hasn't landed yet, bottom first, each its
/// number and commit.
fn merged_of(e: &Entry) -> Vec<(u64, String)> {
    e.host.iter().flat_map(|h| h.changes.iter().filter(|c| c.state == "merged" && !c.landed).map(|c| (c.number, c.commit.clone().unwrap_or_default()))).collect()
}

/// Every change of entry `e` its host merged, landed before or not, as `#n` by the commit it merged
/// as: what its record and its row say it carries.
fn carried_by(e: &Entry) -> BTreeMap<String, String> {
    e.host.iter().flat_map(|h| h.changes.iter().filter(|c| c.state == "merged").map(|c| (format!("#{}", c.number), c.commit.clone().unwrap_or_default()))).collect()
}

/// Entry `e`, every change of it merged, landed: its row says so, with the commits it carries and
/// its changes as its host has them, and whoever follows it is told.
async fn set_landed(hub: &Hub, e: &Entry) -> anyhow::Result<()> {
    save_host(hub, e).await?;
    let (id, list) = (e.id.clone(), serde_json::to_string(&carried_by(e))?);
    hub.store.call(move |c| Ok(c.execute("UPDATE land_queue SET state = 'landed', commits = ?2, updated_at = ?3 WHERE id = ?1", params![id, list, crate::util::now_unix()]).map(|_| ())?)).await?;
    tell(hub, e, "landed", None).await;
    Ok(())
}

/// The tip `merged` (`k` changes, each one commit by `method`) went onto: `to`'s `k`th first
/// parent, as kiln's mirror has it; none where it hasn't `to`, or a rebase made more commits.
async fn went_onto(hub: &Hub, repo: &Repo, path: &str, to: &str, k: usize, method: seam::Method) -> Option<String> {
    gitops::ensure_commit(hub, repo, to).await.ok()?;
    if matches!(method, seam::Method::Rebase | seam::Method::FastForward) {
        return None;
    }
    rev_parse(hub, path, &format!("{to}{}", "^".repeat(k))).await.ok()
}

/// What an entry's merges on its host put on the trunk, as kiln signs it (docs/ci.md W26-R19,
/// W27-R07): the candidate kiln tested and its base, the tip the merges went onto, and whether
/// they landed as tested (onto the base, the batch's tree the candidate's).
struct Landing {
    candidate: String,
    base: String,
    from: String,
    tested: bool,
}

/// Entry `e`'s changes its host merged since kiln last landed any of it, landed: recorded under the
/// trunk's policy with the candidate's runs and the move they made (the record naming all the entry
/// carries), signed into the trail as kiln's queue tested them, or as applied again onto the tip
/// they went onto where they didn't land as tested, and marked landed on `e`.
async fn land_merged(hub: &Arc<Hub>, repo: &Repo, trunk: &str, policy: &land::Policy, e: &mut Entry, how: &Landing) -> anyhow::Result<()> {
    let merged = merged_of(e);
    let Some((_, to)) = merged.last().cloned() else { return Ok(()) };
    let mv = Move { from: how.from.clone(), to: to.clone(), rebased: !how.tested, candidate: how.candidate.clone(), base: how.base.clone() };
    let runs = runs_on(hub, &repo.id, &how.candidate).await.unwrap_or_default();
    if let Err(err) = record(hub, &repo.id, trunk, e, &carried_by(e), Some(&mv), policy, &runs, queue::HOST_MERGES).await {
        tracing::warn!(entry = %e.id, error = %format!("{err:#}"), "a land's record couldn't be kept");
    }
    let mut carried = Vec::new();
    for (n, commit) in &merged {
        let head = e.host.iter().flat_map(|h| h.changes.iter()).find(|c| c.number == *n).map(|c| c.head.clone()).unwrap_or_default();
        carried.push((format!("#{n}"), host_version(hub, &repo.id, *n, &head).await, commit.clone()));
    }
    let tested = super::lands::Tested { candidate: &how.candidate, base: &how.base, from: &how.from, rebased: !how.tested };
    if let Err(err) = Box::pin(super::lands::sign_host(hub, repo, trunk, queue::HOST_MERGES, Some(tested), &carried)).await {
        tracing::warn!(entry = %e.id, error = %format!("{err:#}"), "a land kiln tested and its host merged wasn't signed");
    }
    hub.store.audit("kiln".into(), "queue.land".into(), format!("{} {trunk}: entry {} by {}, tested as {} and merged by its host{}", repo.name, e.id, e.enqueued_by, how.candidate, if how.tested { "" } else { ", not as tested" })).await;
    for c in e.host.iter_mut().flat_map(|h| h.changes.iter_mut()).filter(|c| c.state == "merged") {
        c.landed = true;
    }
    Ok(())
}

/// Why a batch kiln's queue tested for its host to merge stops before it merged whole (docs/ci.md
/// W26-R19).
enum Stop {
    /// Out of the queue.
    Out(String),
    /// Tested again: the trunk moved since it was built for something it meets (W28-R102).
    Again(String),
    /// Review's gate holds it.
    Held(String),
    /// Its change `number` moved to `head` after it was tested: content Review hasn't cleared, back
    /// to Review's gate at that head (W28-R105).
    Back { why: String, number: u64, head: String },
}

/// Change `number` of entry `e` at `head` now, its host change and its pin both: what Review's gate
/// is asked next, and where its candidate is built once the gate clears (docs/ci.md W26-R19).
async fn move_head(hub: &Hub, e: &mut Entry, number: u64, head: &str) -> anyhow::Result<()> {
    for c in e.host.iter_mut().flat_map(|h| h.changes.iter_mut()).filter(|c| c.number == number) {
        (c.head, c.merging, c.etag) = (head.to_owned(), None, None);
    }
    let pin = format!("#{number}");
    for p in e.pinned.iter_mut().filter(|p| p.diff == pin) {
        p.commit = head.to_owned();
    }
    save_host(hub, e).await?;
    let (id, pins) = (e.id.clone(), serde_json::to_string(&e.pinned)?);
    hub.store.call(move |c| Ok(c.execute("UPDATE land_queue SET pinned = ?2 WHERE id = ?1", params![id, pins]).map(|_| ())?)).await
}

/// A batch kiln's queue tested, of a repository whose host has no queue (docs/ci.md W26-R19),
/// merged by its host, all of it or none.
/// - Without the host's trunk as it is now (kiln's mirror fetched), nothing merges this step.
/// - Before anything of the batch merged, R102 is asked again, as kiln's own land asks it just
///   before its move: a trunk that moved since the candidate was built stands only where it moved
///   for what the batch shares nothing with (its result standing, applied again), else the batch is
///   tested again. kiln never builds a candidate of its own on the trunk's tip.
/// - Resumed after its host merged part of it, R102 is asked again of what's still to merge: what
///   moved the trunk since the batch's last merge (another land among it) against those changes.
///   Whichever lands second is so checked against the first, both ways round.
/// - Every entry is decided first: each change as its host has it (`standing_of`, its head through
///   `queue::host_merge`) and Review's gate on those still open. One that's out, held or whose head
///   moved (back to Review, at its new head) stops the batch, once no merge of it is under way:
///   whatever merged lands, and the rest go back to the queue.
/// - Then each change merges at the head tested, bottom first. A background merge is read on the
///   next step, never waited on. A refusal that won't change (a moved head, or a merge its host won't
///   do) stops the batch the same way; one that may (busy, rate-limited, unreachable) is tried again
///   on the next step.
/// - Merged whole, each entry lands, checked against what kiln tested ([`land_merged`]).
///
/// A read that fails leaves the batch for the next step.
#[allow(clippy::too_many_arguments)]
async fn host_land_step(hub: &Arc<Hub>, repo: &Repo, path: &str, trunk: &str, policy: &land::Policy, forge: &dyn Forge, seen: &Seen, batch: &str, ids: &[String]) -> anyhow::Result<()> {
    if let Err(err) = gitops::sync_mirror(hub, repo).await {
        tracing::warn!(repo = %repo.name, error = %format!("{err:#}"), "kiln's mirror couldn't be fetched from its host: nothing merges this step");
        return Ok(());
    }
    let trunk_now = rev_parse(hub, path, &format!("refs/heads/{trunk}^{{commit}}")).await?;
    let mut es = by_ids(&entries(hub, &repo.id, trunk, &["testing"]).await?, ids);
    // What kiln landed of an entry before it came back to the queue isn't this batch's.
    let started = es.iter().any(|e| e.host.iter().flat_map(|h| h.changes.iter()).any(|c| (c.state == "merged" && !c.landed) || c.merging.is_some()));
    let (Some(candidate), Some(base)) = (es.first().and_then(|e| e.candidate.clone()), es.first().and_then(|e| e.base.clone())) else { return Ok(()) };
    // R102 just before the move: what moved the trunk since the candidate was built, against the batch.
    if !started && trunk_now != base {
        let since = since_of(hub, path, &base, &trunk_now).await;
        let changes: Vec<Change> = es.iter().filter_map(|e| seen.changes.get(&e.id).cloned()).collect();
        if let Err(why) = queue::stands(&seen.units, &since, changes) {
            let why = format!("passed, but {why}: tested again on the trunk as it is");
            let stops = es.iter().map(|_| Some(Stop::Again(why.clone()))).collect();
            return stop_batch(hub, repo, path, trunk, policy, batch, &mut es, stops, &candidate, &base).await;
        }
    }
    // Every entry decided before anything merges.
    let mut stops: Vec<Option<Stop>> = Vec::new();
    for e in &mut es {
        let Some(h) = e.host.as_mut() else {
            stops.push(Some(Stop::Out("it isn't a host's change".to_owned())));
            continue;
        };
        let name = h.repo.clone();
        let mut stop = None;
        for c in h.changes.iter_mut().filter(|c| c.state == "open") {
            let standing = match standing_of(forge, &name, c).await {
                Ok(s) => s,
                Err(err) => {
                    tracing::warn!(number = c.number, error = %format!("{err:#}"), "a change couldn't be read on its host before a merge: the next step reads it again");
                    return Ok(());
                }
            };
            match standing {
                Standing::Out(why) => stop = Some(Stop::Out(why)),
                Standing::Open { head } => {
                    if let queue::HostMerge::Back { why } = queue::host_merge(&c.head, &head) {
                        stop = Some(Stop::Back { why: format!("#{}: {why}", c.number), number: c.number, head });
                    }
                }
                Standing::Merged | Standing::Merging => {}
            }
            if stop.is_some() {
                break;
            }
        }
        if stop.is_none() {
            match super::review::host_held(hub, repo, trunk, &open_on_host(e)).await {
                Ok(held) if !held.is_empty() => stop = Some(Stop::Held(held.join("; "))),
                Ok(_) => {}
                Err((_, why)) => {
                    tracing::warn!(entry = %e.id, error = %why, "Review's gate couldn't be asked before a merge: the next step asks again");
                    return Ok(());
                }
            }
        }
        stops.push(stop);
    }
    if stops.iter().any(Option::is_some) {
        // A merge under way says what merged before the batch stops: read again next step.
        if es.iter().any(|e| e.host.iter().flat_map(|h| h.changes.iter()).any(|c| c.merging.is_some())) {
            return Ok(());
        }
        return stop_batch(hub, repo, path, trunk, policy, batch, &mut es, stops, &candidate, &base).await;
    }
    // Resumed: what moved the trunk since the batch's last merge, against what's still to merge.
    if let Some(last) = started.then(|| last_merged(&es)).flatten() {
        let mut rest = Change { id: "what's still to merge of the batch".into(), files: BTreeSet::new(), targets: BTreeSet::new() };
        for (_, head) in es.iter().flat_map(open_on_host) {
            let from = merge_base(hub, path, &trunk_now, &head).await;
            rest.files.extend(files_of(hub, path, &from, &head).await);
        }
        if !rest.files.is_empty() {
            let since = since_of(hub, path, &last, &trunk_now).await;
            if let Err(why) = queue::stands(&seen.units, &since, vec![rest]) {
                let why = format!("passed, but part of its batch merged and {why}: tested again on the trunk as it is");
                let stops = es.iter().map(|e| (!open_on_host(e).is_empty()).then(|| Stop::Again(why.clone()))).collect();
                return stop_batch(hub, repo, path, trunk, policy, batch, &mut es, stops, &candidate, &base).await;
            }
        }
    }
    // Then each change merges, at the head tested, bottom first.
    let mut failed: Option<(usize, Stop)> = None;
    let mut waiting = false;
    for (i, e) in es.iter_mut().enumerate() {
        let method = method_of(e.strategy);
        let Some(h) = e.host.as_mut() else { continue };
        let name = h.repo.clone();
        for c in h.changes.iter_mut().filter(|c| c.state == "open") {
            if c.merging.is_some() {
                waiting = true;
                break;
            }
            match forge.merge(&name, c.number, seam::Merge { method, expected: c.head.clone() }).await {
                Ok(seam::Merging::Merged { commit, .. }) => (c.state, c.commit) = ("merged".into(), Some(commit)),
                Ok(seam::Merging::Started { id }) => {
                    c.merging = Some(id);
                    waiting = true;
                    break;
                }
                Err(seam::Refusal::Moved { head }) => {
                    let why = format!("#{}: its head moved to {} on its host after {} was tested, past what Review cleared", c.number, short(&head), short(&c.head));
                    failed = Some((i, Stop::Back { why, number: c.number, head }));
                    break;
                }
                // What its host won't do on any later step either takes the entry out.
                Err(err @ (seam::Refusal::Unsupported { .. } | seam::Refusal::Invalid { .. } | seam::Refusal::NotLinear { .. } | seam::Refusal::StackLimit { .. } | seam::Refusal::NotFound { .. })) => {
                    failed = Some((i, Stop::Out(format!("#{}: its host won't merge it: {err}", c.number))));
                    break;
                }
                Err(err) => {
                    tracing::warn!(number = c.number, error = %err, "its host didn't take a merge: the next step reads the change again");
                    waiting = true;
                    break;
                }
            }
        }
        save_host(hub, e).await?;
        if waiting || failed.is_some() {
            break;
        }
    }
    if let Some((i, stop)) = failed {
        let mut stops: Vec<Option<Stop>> = (0..es.len()).map(|_| None).collect();
        if let Some(s) = stops.get_mut(i) {
            *s = Some(stop);
        }
        return stop_batch(hub, repo, path, trunk, policy, batch, &mut es, stops, &candidate, &base).await;
    }
    if waiting {
        return Ok(());
    }
    // Merged whole: each entry lands, checked against what kiln tested. The first went onto the
    // candidate's base, by its tree; each later one onto the one before; and each one's tree is the
    // candidate's after it (its applied commits).
    let tree = |c: String| async move { if c.is_empty() { None } else { rev_parse(hub, path, &format!("{c}^{{tree}}")).await.ok() } };
    let base_tree = tree(base.clone()).await;
    let mut before: Option<String> = None;
    let mut landings = Vec::new();
    let mut tested = base_tree.is_some();
    for e in &es {
        let merged = merged_of(e);
        let to = merged.last().map(|(_, c)| c.clone()).unwrap_or_default();
        let from = went_onto(hub, repo, path, &to, merged.len(), method_of(e.strategy)).await.unwrap_or_default();
        let point = merged.last().and_then(|(n, _)| e.commits.get(&format!("#{n}"))).cloned().unwrap_or_default();
        let onto = match &before {
            None => tree(from.clone()).await.is_some_and(|t| Some(t) == base_tree),
            Some(b) => !from.is_empty() && &from == b,
        };
        let (at, tested_at) = (tree(to.clone()).await, tree(point).await);
        tested &= onto && at.is_some() && at == tested_at;
        landings.push(from);
        before = Some(to);
    }
    drop_candidate(hub, path, trunk, batch).await;
    for (e, from) in es.iter_mut().zip(landings) {
        land_merged(hub, repo, trunk, policy, e, &Landing { candidate: candidate.clone(), base: base.clone(), from, tested }).await?;
        set_landed(hub, e).await?;
    }
    Ok(())
}

/// A batch stopped before it merged whole (docs/ci.md W26-R19): its candidate dropped, and each
/// entry's changes its host merged landed (not as tested: the candidate was the whole batch). An
/// entry merged whole is landed; each other is out, held, back with Review's gate at its new head,
/// or queued again by `stops` (its own, or "an entry beside it didn't merge").
#[allow(clippy::too_many_arguments)]
async fn stop_batch(hub: &Arc<Hub>, repo: &Repo, path: &str, trunk: &str, policy: &land::Policy, batch: &str, es: &mut [Entry], stops: Vec<Option<Stop>>, candidate: &str, base: &str) -> anyhow::Result<()> {
    drop_candidate(hub, path, trunk, batch).await;
    for (e, stop) in es.iter_mut().zip(stops) {
        let merged = merged_of(e);
        if let Some((_, to)) = merged.last() {
            let from = went_onto(hub, repo, path, to, merged.len(), method_of(e.strategy)).await.unwrap_or_default();
            land_merged(hub, repo, trunk, policy, e, &Landing { candidate: candidate.to_owned(), base: base.to_owned(), from, tested: false }).await?;
        }
        if e.host.as_ref().is_some_and(|h| h.changes.iter().all(|c| c.state == "merged")) {
            set_landed(hub, e).await?;
            continue;
        }
        save_host(hub, e).await?;
        let said = |why: &str| if merged.is_empty() { why.to_owned() } else { format!("{why} ({} merged and signed)", merged.iter().map(|(n, _)| format!("#{n}")).collect::<Vec<_>>().join(", ")) };
        match stop {
            Some(Stop::Out(why)) => eject(hub, e, &said(&why), "INVALID_MERGE_COMMIT").await?,
            Some(Stop::Held(why)) => hold_entry(hub, e, "gate", &said(&why), "BRANCH_PROTECTIONS").await?,
            Some(Stop::Again(why)) => requeue_with(hub, std::slice::from_ref(&*e), Some(said(&why).as_str())).await?,
            Some(Stop::Back { why, number, head }) => {
                move_head(hub, e, number, &head).await?;
                hold_entry(hub, e, "gate", &said(&why), "BRANCH_PROTECTIONS").await?;
            }
            None => requeue_with(hub, std::slice::from_ref(&*e), Some(said("an entry beside it in its batch didn't merge").as_str())).await?,
        }
    }
    Ok(())
}

/// A host entry's changes still open there, each at the head Review cleared.
fn open_on_host(e: &Entry) -> Vec<(u64, String)> {
    e.host.iter().flat_map(|h| h.changes.iter().filter(|c| c.state == "open").map(|c| (c.number, c.head.clone()))).collect()
}

/// Which of change `number`'s heads kiln saw `head` as (its version, from 1): the first, where
/// kiln's mirror never saw it.
async fn host_version(hub: &Hub, repo_id: &str, number: u64, head: &str) -> u32 {
    let (id, n, sha) = (repo_id.to_owned(), i64::try_from(number).unwrap_or(i64::MAX), head.to_owned());
    let seen = hub.store.call(move |c| Ok(c.query_row("SELECT MIN(n) FROM host_pull_heads WHERE repo_id = ?1 AND number = ?2 AND sha = ?3", params![id, n, sha], |r| r.get::<_, Option<i64>>(0))?)).await;
    seen.ok().flatten().and_then(|v| u32::try_from(v).ok()).filter(|v| *v > 0).unwrap_or(1)
}

/// Host entries that wait (docs/ci.md W26-R19). One whose changes its host merged meanwhile (a
/// merge that won the race with `off_host`, or one made on the host itself) lands as the queue lands
/// one, the hold said, so the trail misses no merge. One Review's gate holds is asked again, and
/// one it clears is queued, to go into its host's queue on this step. A read that fails is said,
/// and the next entry asked.
async fn host_recheck(hub: &Arc<Hub>, repo: &Repo, trunk: &str) -> anyhow::Result<()> {
    let waiting: Vec<Entry> = entries(hub, &repo.id, trunk, &["held"]).await?.into_iter().filter(|e| e.host.is_some()).collect();
    if waiting.is_empty() {
        return Ok(());
    }
    let forge = forge_of(hub, repo).await;
    for e in waiting {
        if !due(&e.id, RECHECK) {
            continue;
        }
        rechecked().lock().insert(e.id.clone(), Instant::now());
        if let Some((f, _)) = &forge {
            match merged_while_held(hub, repo, trunk, f.as_ref(), &e).await {
                Ok(true) => continue,
                Ok(false) => {}
                Err(err) => tracing::warn!(entry = %e.id, error = %format!("{err:#}"), "a held entry's changes couldn't be read on its host"),
            }
        }
        if e.hold.as_deref() != Some("gate") {
            continue;
        }
        let held = match super::review::host_held(hub, repo, trunk, &open_on_host(&e)).await {
            Ok(held) => held,
            Err((_, why)) => {
                tracing::warn!(entry = %e.id, error = %why, "Review's gate couldn't be asked again of a held entry");
                continue;
            }
        };
        let (id, now) = (e.id.clone(), crate::util::now_unix());
        if held.is_empty() {
            hub.store.call(move |c| Ok(c.execute("UPDATE land_queue SET state = 'queued', hold = NULL, reason = NULL, queued_at = ?2, updated_at = ?2 WHERE id = ?1", params![id, now]).map(|_| ())?)).await?;
            tell(hub, &e, "queued", None).await;
        } else if e.reason.as_deref() != Some(held.join("; ").as_str()) {
            let why = held.join("; ");
            hub.store.call(move |c| Ok(c.execute("UPDATE land_queue SET reason = ?2, updated_at = ?3 WHERE id = ?1", params![id, why, now]).map(|_| ())?)).await?;
        }
    }
    Ok(())
}

/// How often what's on a host's queue is read back unasked, at most (a read of the queue counts
/// against the host's rate limit; a change's conditional read costs nothing while it's unchanged).
const HOST_QUEUE_EVERY: Duration = Duration::from_secs(30);

/// One step of a repository's entries on its host's own queue (docs/ci.md W28-R109), through the
/// forge seam alone: each change put in (`enqueue`, the queue read back after, as a stacked change
/// takes those below it), followed (`change`, read conditionally; `queued`, read back when one
/// moved), and the entry landed once every change merged, out once one left the queue unmerged (what
/// merged before it landed first, W26-R19). An
/// entry kiln's queue would land, of a repository kiln doesn't host, is ejected, as before.
async fn host_step(hub: &Arc<Hub>, repo: &Repo, trunk: &str) -> anyhow::Result<()> {
    if let Err(err) = host_recheck(hub, repo, trunk).await {
        tracing::warn!(repo = %repo.name, error = %format!("{err:#}"), "held entries on a host couldn't be asked again");
    }
    let live = entries(hub, &repo.id, trunk, &["queued", "testing"]).await?;
    // Entries routed to kiln's queue (W26-R19) are kiln's to land, not their host's queue's.
    let (on_host, rest): (Vec<Entry>, Vec<Entry>) = live.into_iter().filter(|e| e.route.as_deref() != Some(queue::HOST_MERGES)).partition(|e| e.host.is_some());
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
    let next = queue::next_to_enqueue(&ons);
    // Review's gate, asked just before a change goes in and again while the host's queue holds it
    // (docs/ci.md W26-R19): one it holds comes out of the host's queue and waits, held, until it
    // clears.
    if next.is_some() || due(&format!("gate:{}", e.id), RECHECK) {
        rechecked().lock().insert(format!("gate:{}", e.id), Instant::now());
        let held = super::review::host_held(hub, repo, trunk, &open_on_host(e)).await.map_err(|(_, m)| anyhow::anyhow!(m))?;
        if !held.is_empty() {
            off_host(hub, e).await;
            return hold_entry(hub, e, "gate", &held.join("; "), "BRANCH_PROTECTIONS").await;
        }
    }
    if let Some(n) = next {
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
            land_from_host(hub, repo, trunk, e, &h, &landed, "").await
        }
        // What its host's queue merged before the rest left it lands first, so the trail misses no
        // merge (W26-R19): recorded and signed, and the entry out, saying which part merged.
        queue::HostVerdict::Out { why, .. } => {
            let merged: Vec<(u64, String)> = h.changes.iter().filter(|c| c.state == "merged" && !c.landed).map(|c| (c.number, c.commit.clone().unwrap_or_default())).collect();
            if merged.is_empty() {
                return eject(hub, e, &why, "UNKNOWN_REMOVAL_REASON").await;
            }
            let landed: BTreeMap<String, String> = merged.iter().map(|(n, c)| (format!("#{n}"), c.clone())).collect();
            keep_host_land(hub, repo, trunk, e, &h, &landed, "").await;
            for c in h.changes.iter_mut().filter(|c| c.state == "merged") {
                c.landed = true;
            }
            let (id, text) = (e.id.clone(), serde_json::to_string(&h)?);
            hub.store.call(move |c| Ok(c.execute("UPDATE land_queue SET host = ?2 WHERE id = ?1", params![id, text]).map(|_| ())?)).await?;
            let said = merged.iter().map(|(n, _)| format!("#{n}")).collect::<Vec<_>>().join(", ");
            eject(hub, e, &format!("{why} ({said} merged and signed)"), "UNKNOWN_REMOVAL_REASON").await
        }
        queue::HostVerdict::Waiting => Ok(()),
    }
}

/// A host entry whose changes its host merged (docs/ci.md W28-R109, W26-R19): landed, and what it
/// carries kept as [`keep_host_land`] keeps it.
async fn land_from_host(hub: &Arc<Hub>, repo: &Repo, trunk: &str, e: &Entry, h: &HostEntry, landed: &BTreeMap<String, String>, note: &str) -> anyhow::Result<()> {
    let (id, list, text) = (e.id.clone(), serde_json::to_string(landed)?, serde_json::to_string(h)?);
    hub.store
        .call(move |c| Ok(c.execute("UPDATE land_queue SET state = 'landed', hold = NULL, commits = ?2, host = ?3, updated_at = ?4 WHERE id = ?1", params![id, list, text, crate::util::now_unix()]).map(|_| ())?))
        .await?;
    keep_host_land(hub, repo, trunk, e, h, landed, note).await;
    tell(hub, e, "landed", None).await;
    Ok(())
}

/// What entry `e`'s host merged (`landed`, each change as `#n` by the commit it merged as), recorded
/// under its trunk's policy as kiln's mirror has it (W28-R108) and signed into the trail as kiln's
/// server saw it land, each change bottom first at the version Review cleared, and those kiln landed
/// before left out of the statement. `note` says how it came to land (", while Review held it"),
/// after its queue's name in the record, the statement and the audit.
async fn keep_host_land(hub: &Arc<Hub>, repo: &Repo, trunk: &str, e: &Entry, h: &HostEntry, landed: &BTreeMap<String, String>, note: &str) {
    let via = format!("{}{note}", e.route.clone().unwrap_or_default());
    let mirror = crate::gitops::mirror_path(hub, &repo.name).to_string_lossy().into_owned();
    let (policy, _) = super::review::policy(hub, &mirror, trunk).await;
    if let Err(err) = record(hub, &repo.id, trunk, e, landed, None, &policy, &[], &via).await {
        tracing::warn!(entry = %e.id, error = %format!("{err:#}"), "a land's record couldn't be kept");
    }
    // What kiln's queue landed of it before is in the trail already (W26-R19).
    let mut carried = Vec::new();
    for c in h.changes.iter().filter(|c| !c.landed) {
        if let Some(commit) = landed.get(&format!("#{}", c.number)) {
            carried.push((format!("#{}", c.number), host_version(hub, &repo.id, c.number, &c.head).await, commit.clone()));
        }
    }
    if let Err(err) = Box::pin(super::lands::sign_host(hub, repo, trunk, &via, None, &carried)).await {
        tracing::warn!(entry = %e.id, error = %format!("{err:#}"), "a land through its host's queue wasn't signed");
    }
    hub.store.audit("kiln".into(), "queue.land".into(), format!("{} {trunk}: entry {} by {}, through its host's {via}", repo.name, e.id, e.enqueued_by)).await;
}

/// Whether held entry `e`'s changes all merged on its host meanwhile, read conditionally (free while
/// unchanged): then it's landed as [`follow_host`] lands one, saying Review held it.
async fn merged_while_held(hub: &Arc<Hub>, repo: &Repo, trunk: &str, forge: &dyn Forge, e: &Entry) -> anyhow::Result<bool> {
    let Some(mut h) = e.host.clone() else { return Ok(false) };
    let (before, name) = (h.clone(), h.repo.clone());
    for c in h.changes.iter_mut().filter(|c| c.state == "open") {
        if let seam::Fresh::Changed { value, etag } = forge.change(&name, c.number, c.etag.as_deref()).await? {
            c.etag = Some(etag);
            match value.state {
                seam::State::Merged { commit } => {
                    c.state = "merged".into();
                    c.commit = Some(commit);
                }
                seam::State::Closed => c.state = "closed".into(),
                seam::State::Open => {}
            }
        }
    }
    if !h.changes.iter().all(|c| c.state == "merged") {
        if h != before {
            let (id, text) = (e.id.clone(), serde_json::to_string(&h)?);
            hub.store.call(move |c| Ok(c.execute("UPDATE land_queue SET host = ?2 WHERE id = ?1", params![id, text]).map(|_| ())?)).await?;
        }
        return Ok(false);
    }
    let landed: BTreeMap<String, String> = h.changes.iter().map(|c| (format!("#{}", c.number), c.commit.clone().unwrap_or_default())).collect();
    land_from_host(hub, repo, trunk, e, &h, &landed, queue::HELD_NOTE).await?;
    Ok(true)
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

    /// What kiln's mirror has of `host`'s pull request `number` (docs/ci.md W26-R01): opened by ada,
    /// at `head`, and its reviews so far, oldest first (who and their verdict).
    async fn mirror(hub: &Hub, repo_id: &str, host: &str, number: u64, head: &str, reviews: &[(&str, &str)]) -> anyhow::Result<()> {
        let said: Vec<Value> = reviews.iter().zip(1_i64..).map(|((by, verdict), at)| json!({ "by": format!("{host}:{by}"), "verdict": verdict, "at": at })).collect();
        let (repo, host, head, n, said) = (repo_id.to_owned(), host.to_owned(), head.to_owned(), i64::try_from(number)?, serde_json::to_string(&said)?);
        hub.store
            .call(move |c| {
                c.execute(
                    "INSERT INTO host_pulls (repo_id, host, number, author, state, reviews, first_seen) VALUES (?1, ?2, ?3, ?4, 'open', ?5, 0)
                     ON CONFLICT (repo_id, host, number) DO UPDATE SET reviews = excluded.reviews",
                    params![repo, host, n, format!("{host}:ada"), said],
                )?;
                c.execute("INSERT OR REPLACE INTO host_pull_heads (repo_id, host, number, n, sha, seen_at) VALUES (?1, ?2, ?3, 1, ?4, 0)", params![repo, host, n, head])?;
                Ok(())
            })
            .await
    }

    /// A repository on a host, its clone where nothing answers: kiln's mirror of it stays empty.
    fn hosted(id: &str, name: &str) -> Repo {
        Repo { clone_url: format!("file:///nonexistent/{id}.git"), ..repo(id, name) }
    }

    /// docs/ci.md W26-R19: a host's pull request reaches its host's queue only once Review's gate is
    /// clear, on GitHub's merge queue and GitLab's merge trains alike. Asked to land while kim asks
    /// for changes, it's refused, naming her; one kiln hasn't mirrored is refused too. Cleared, it
    /// goes in at the head Review cleared. Asked for changes again while it's queued, it comes out,
    /// held by the gate, saying why; approved again, it goes back in.
    #[test]
    fn w26_r19_a_host_pull_enters_its_hosts_queue_only_when_clear() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            for (sim, host, name, route) in [(seam::SimForge::github(), "github", "acme/app", "merge queue"), (seam::SimForge::gitlab(), "gitlab", "acme/lab", "merge train")] {
                let r = hub.store.upsert_repo(hosted(&format!("r-{host}"), name)).await?;
                sim.add_repo(name);
                sim.set_queue(name, true);
                sim.push(name, "one", seam::TRUNK, "one")?;
                let one = sim.open(name, seam::Open { head: "one".into(), base: seam::TRUNK.into(), title: "change one".into(), by: "ada".into() }).await?;
                forges().lock().insert(r.id.clone(), (Arc::new(sim.clone()) as Arc<dyn Forge>, name.to_owned()));
                let ask = json!({ "changes": [{ "number": one.number, "head": one.head_sha }] });
                let unseen = enqueue_route(&hub, name, &ask, "user:ada").await;
                assert!(matches!(&unseen, Err((StatusCode::CONFLICT, why)) if why.contains("Review hasn't seen it")), "{host}: {unseen:?}");
                // A draft is held until it's marked ready, whatever its reviews say.
                mirror(&hub, &r.id, host, one.number, &one.head_sha, &[("kim", "approve")]).await?;
                let (id, n) = (r.id.clone(), i64::try_from(one.number)?);
                let mark = |state: &'static str| {
                    let id = id.clone();
                    hub.store.call(move |c| Ok(c.execute("UPDATE host_pulls SET state = ?3 WHERE repo_id = ?1 AND number = ?2", params![id, n, state]).map(|_| ())?))
                };
                mark("draft").await?;
                let draft = enqueue_route(&hub, name, &ask, "user:ada").await;
                assert!(matches!(&draft, Err((StatusCode::CONFLICT, why)) if why.contains(&format!("#{}: it's a draft", one.number))), "{host}: {draft:?}");
                mark("open").await?;
                mirror(&hub, &r.id, host, one.number, &one.head_sha, &[("kim", "request_changes")]).await?;
                let refused = enqueue_route(&hub, name, &ask, "user:ada").await;
                assert!(matches!(&refused, Err((StatusCode::CONFLICT, why)) if why.contains(&format!("#{}: ", one.number)) && why.contains(&format!("{host}:kim asked for changes"))), "{host}: {refused:?}");
                assert!(sim.queued(name).await?.is_empty(), "{host}: nothing went in");

                mirror(&hub, &r.id, host, one.number, &one.head_sha, &[("kim", "request_changes"), ("kim", "approve")]).await?;
                let a = enqueue_route(&hub, name, &ask, "user:ada").await.map_err(|(_, m)| anyhow::anyhow!(m))?;
                assert_eq!(a.get("route").and_then(Value::as_str), Some(route));
                let entry = a.get("entry").and_then(Value::as_str).unwrap_or_default().to_owned();
                host_step(&hub, &r, seam::TRUNK).await?;
                assert_eq!(sim.queued(name).await?, vec![one.number], "{host}: cleared, it's in");

                mirror(&hub, &r.id, host, one.number, &one.head_sha, &[("kim", "approve"), ("kim", "request_changes")]).await?;
                rechecked().lock().remove(&format!("gate:{entry}"));
                host_step(&hub, &r, seam::TRUNK).await?;
                assert!(sim.queued(name).await?.is_empty(), "{host}: held, it left the host's queue");
                let held = entry_by_id(&hub, &entry).await?.ok_or_else(|| anyhow::anyhow!("no entry {entry}"))?;
                assert_eq!((held.state.as_str(), held.hold.as_deref()), ("held", Some("gate")), "{host}");
                assert!(held.reason.as_deref().is_some_and(|w| w.contains(&format!("{host}:kim asked for changes"))), "{host}: {:?}", held.reason);

                mirror(&hub, &r.id, host, one.number, &one.head_sha, &[("kim", "request_changes"), ("kim", "approve")]).await?;
                rechecked().lock().remove(&entry);
                host_step(&hub, &r, seam::TRUNK).await?;
                assert_eq!(sim.queued(name).await?, vec![one.number], "{host}: approved again, it's back in");
                assert_eq!(state_of(&hub, &entry).await?.0, "testing", "{host}");
            }
            Ok(())
        })
    }

    /// docs/ci.md W26-R19: a change its host merged while Review's gate held its entry (merged on the
    /// host directly, or before `off_host` took it out) still lands, recorded and signed into the
    /// trail, the hold said: the trail misses no merge.
    #[test]
    fn w26_r19_a_change_its_host_merged_while_held_lands_signed() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            crate::identities::own_identities(&hub.store, &hub.pki).await?;
            let sim = seam::SimForge::github();
            let (name, r) = ("acme/held", hub.store.upsert_repo(hosted("r-held", "acme/held")).await?);
            sim.add_repo(name);
            sim.set_queue(name, true);
            sim.push(name, "one", seam::TRUNK, "one")?;
            let one = sim.open(name, seam::Open { head: "one".into(), base: seam::TRUNK.into(), title: "change one".into(), by: "ada".into() }).await?;
            forges().lock().insert(r.id.clone(), (Arc::new(sim.clone()) as Arc<dyn Forge>, name.to_owned()));
            mirror(&hub, &r.id, "github", one.number, &one.head_sha, &[("kim", "approve")]).await?;
            let a = enqueue_route(&hub, name, &json!({ "changes": [{ "number": one.number, "head": one.head_sha }] }), "user:ada").await.map_err(|(_, m)| anyhow::anyhow!(m))?;
            let entry = a.get("entry").and_then(Value::as_str).unwrap_or_default().to_owned();
            host_step(&hub, &r, seam::TRUNK).await?;
            mirror(&hub, &r.id, "github", one.number, &one.head_sha, &[("kim", "approve"), ("kim", "request_changes")]).await?;
            rechecked().lock().remove(&format!("gate:{entry}"));
            host_step(&hub, &r, seam::TRUNK).await?;
            assert_eq!(state_of(&hub, &entry).await?.0, "held");
            // Merged on the host directly while it's held.
            sim.set_queue(name, false);
            // GitHub merges in the background: done once its result is read.
            if let seam::Merging::Started { id } = sim.merge(name, one.number, seam::Merge { method: seam::Method::Merge, expected: one.head_sha.clone() }).await? {
                sim.merge_result(name, one.number, &id).await?;
            }
            rechecked().lock().remove(&entry);
            host_step(&hub, &r, seam::TRUNK).await?;
            assert_eq!(state_of(&hub, &entry).await?.0, "landed");
            let id = entry.clone();
            let kept = hub.store.call(move |c| Ok(c.query_row(&format!("SELECT {RECORD_COLUMNS} FROM land_records WHERE entry_id = ?1"), [&id], record_row)?)).await?;
            assert_eq!(kept.queue, "merge queue, while Review held it");
            let commit = kept.landed.get(&format!("#{}", one.number)).cloned().unwrap_or_default();
            assert!(sim.contains(name, seam::TRUNK, &commit));
            let got = super::super::lands::get(&hub, name, &commit).await.map_err(|(_, m)| anyhow::anyhow!(m))?;
            assert_eq!(got.pointer("/land/via").and_then(Value::as_str), Some("merge queue, while Review held it"), "{got}");
            Ok(())
        })
    }

    /// A repository on a host with no queue (docs/ci.md W26-R19): a real repository as the host,
    /// kiln's mirror fetched from it, and the forge seam's simulator holding the same commits. The
    /// trunk has a change on each of `branches`, each one mirrored with kim's approval.
    struct Lab {
        r: Repo,
        sim: seam::SimForge,
        name: &'static str,
        work: std::path::PathBuf,
        bare: std::path::PathBuf,
        t: String,
        heads: Vec<String>,
        pulls: Vec<seam::Change>,
    }

    fn git_in(dir: &std::path::Path, args: &[&str]) -> anyhow::Result<String> {
        let o = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "ada")
            .env("GIT_AUTHOR_EMAIL", "ada@example.com")
            .env("GIT_COMMITTER_NAME", "ada")
            .env("GIT_COMMITTER_EMAIL", "ada@example.com")
            .output()?;
        anyhow::ensure!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
        Ok(String::from_utf8_lossy(&o.stdout).trim().to_owned())
    }

    async fn merges_lab(hub: &Arc<Hub>, id: &str, name: &'static str, branches: &[&str]) -> anyhow::Result<Lab> {
        let each: Vec<(String, Vec<(String, String)>)> = branches.iter().map(|b| ((*b).to_owned(), vec![(format!("{b}.txt"), format!("{b}.txt\n"))])).collect();
        lab_with(hub, id, name, &[("README.md".to_owned(), "README.md\n".to_owned())], &each).await
    }

    /// [`merges_lab`]'s lab, its trunk holding `files` and each branch writing its own.
    async fn lab_with(hub: &Arc<Hub>, id: &str, name: &'static str, files: &[(String, String)], branches: &[(String, Vec<(String, String)>)]) -> anyhow::Result<Lab> {
        crate::identities::own_identities(&hub.store, &hub.pki).await?;
        let bare = gitops::mirror_path(hub, name);
        let work = bare.with_extension("host");
        std::fs::create_dir_all(&work)?;
        let commit = |files: &[(String, String)], said: &str| -> anyhow::Result<String> {
            for (file, text) in files {
                let at = work.join(file);
                if let Some(dir) = at.parent() {
                    std::fs::create_dir_all(dir)?;
                }
                std::fs::write(at, text)?;
            }
            git_in(&work, &["add", "."])?;
            git_in(&work, &["commit", "-qm", said])?;
            git_in(&work, &["rev-parse", "HEAD"])
        };
        git_in(&work, &["init", "-q", "-b", seam::TRUNK])?;
        let t = commit(files, "the trunk")?;
        let mut heads = Vec::new();
        for (b, writes) in branches {
            git_in(&work, &["checkout", "-qb", b, seam::TRUNK])?;
            heads.push(commit(writes, b)?);
        }
        git_in(&work, &["checkout", "-q", seam::TRUNK])?;
        let r = hub.store.upsert_repo(Repo { clone_url: work.to_string_lossy().into_owned(), ..repo(id, name) }).await?;
        gitops::sync_mirror(hub, &r).await?;
        let sim = seam::SimForge::github();
        sim.add_repo(name);
        sim.adopt(name, seam::TRUNK, &t, &[])?;
        let mut pulls = Vec::new();
        for ((b, _), h) in branches.iter().zip(&heads) {
            sim.adopt(name, b, h, &[&t])?;
            let c = sim.open(name, seam::Open { head: b.clone(), base: seam::TRUNK.into(), title: format!("change {b}"), by: "ada".into() }).await?;
            mirror(hub, &r.id, "github", c.number, &c.head_sha, &[("kim", "approve")]).await?;
            pulls.push(c);
        }
        forges().lock().insert(r.id.clone(), (Arc::new(sim.clone()) as Arc<dyn Forge>, name.to_owned()));
        Ok(Lab { r, sim, name, work, bare, t, heads, pulls })
    }

    /// `changes` (bottom first) into the queue, as Review hands them over: the entry's id.
    async fn hand_over(hub: &Arc<Hub>, lab: &Lab, changes: &[&seam::Change]) -> anyhow::Result<String> {
        let list: Vec<Value> = changes.iter().map(|c| json!({ "number": c.number, "head": c.head_sha })).collect();
        let a = enqueue_route(hub, lab.name, &json!({ "changes": list }), "user:ada").await.map_err(|(_, m)| anyhow::anyhow!(m))?;
        assert_eq!(a.get("route").and_then(Value::as_str), Some(queue::HOST_MERGES));
        Ok(a.get("entry").and_then(Value::as_str).unwrap_or_default().to_owned())
    }

    /// The queue stepped until `entry` is in one of `states`, at most `n` steps: its entry then.
    async fn step_until(hub: &Arc<Hub>, lab: &Lab, entry: &str, states: &[&str], n: usize) -> anyhow::Result<Entry> {
        for _ in 0..n {
            step(hub, &lab.r, seam::TRUNK).await?;
            let e = entry_by_id(hub, entry).await?.ok_or_else(|| anyhow::anyhow!("no entry {entry}"))?;
            if states.contains(&e.state.as_str()) {
                return Ok(e);
            }
        }
        anyhow::bail!("{entry} isn't {states:?} after {n} steps")
    }

    /// docs/ci.md W26-R19, W28-R109: a repository on a host with no queue lands through kiln's queue.
    /// Each change's candidate is built and tested in kiln's mirror, then its host merges it at the
    /// head tested, its background merge read on a later step, never waited on. The land is
    /// recorded and signed into the trail as kiln's queue tested it. A trunk that moves between the
    /// test and the merge only for what the change shares nothing with leaves its result standing:
    /// merged, applied again, never built again (W28-R102). A head that moves goes back to Review's
    /// gate, saying so, and enters again at its new head once its gate clears (W28-R105).
    #[test]
    fn w26_r19_a_host_without_a_queue_lands_through_kilns() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let lab = merges_lab(&hub, "r-merges", "acme/merge", &["one", "two", "three"]).await?;
            let ([h1, h2, h3], [p1, p2, p3]) = (lab.heads.as_slice(), lab.pulls.as_slice()) else { anyhow::bail!("three changes") };
            let (name, t) = (lab.name, lab.t.clone());

            // #1: built on the trunk in kiln's mirror, tested there (nothing asks for a check), then
            // merged by its host at the head tested.
            let ea = hand_over(&hub, &lab, &[p1]).await?;
            let built = step_until(&hub, &lab, &ea, &["testing"], 2).await?;
            let cand = built.candidate.clone().unwrap_or_default();
            assert_eq!(built.base.as_deref(), Some(t.as_str()));
            assert_eq!(git_in(&lab.bare, &["rev-parse", &format!("{cand}^2")])?, *h1, "the candidate merges the head Review cleared");
            let merging = step_until(&hub, &lab, &ea, &["testing", "landed"], 1).await?;
            assert!(merging.host.iter().flat_map(|h| h.changes.iter()).any(|c| c.merging.is_some()), "its background merge is read on a later step");
            step_until(&hub, &lab, &ea, &["landed"], 2).await?;
            assert!(lab.sim.contains(name, seam::TRUNK, h1), "merged on its host");
            let id = ea.clone();
            let kept = hub.store.call(move |c| Ok(c.query_row(&format!("SELECT {RECORD_COLUMNS} FROM land_records WHERE entry_id = ?1"), [&id], record_row)?)).await?;
            assert_eq!(kept.queue, queue::HOST_MERGES);
            let merged = kept.landed.get(&format!("#{}", p1.number)).cloned().unwrap_or_default();
            let got = super::super::lands::get(&hub, name, &merged).await.map_err(|(_, m)| anyhow::anyhow!(m))?;
            let said = |k: &str| got.pointer(&format!("/land/{k}")).and_then(Value::as_str).unwrap_or_default().to_owned();
            assert_eq!((said("via"), said("candidate"), said("base")), (queue::HOST_MERGES.to_owned(), cand.clone(), t.clone()), "{got}");
            // The simulator's merge commit isn't in kiln's mirror: kiln can't show it landed as
            // tested, so it signs it as applied again, onto a tip it couldn't read.
            assert_eq!(got.pointer("/land/rebased").and_then(Value::as_bool), Some(true), "{got}");
            // The host's trunk has #1 in it now, and kiln's mirror fetches it from there.
            git_in(&lab.work, &["merge", "-q", "--no-ff", "-m", "Merge #1", "one"])?;
            let m1 = git_in(&lab.work, &["rev-parse", "HEAD"])?;
            lab.sim.adopt(name, seam::TRUNK, &m1, &[&t, h1])?;

            // #2: built on the trunk kiln's mirror had. The trunk moved before the merge, only for #1,
            // which #2 shares nothing with: its result stands, and it's merged onto the trunk as it
            // is, applied again, never built again.
            let eb = hand_over(&hub, &lab, &[p2]).await?;
            let built = step_until(&hub, &lab, &eb, &["testing"], 2).await?;
            assert_eq!(built.base, Some(t.clone()), "built on the trunk the mirror had");
            let landed = step_until(&hub, &lab, &eb, &["landed"], 5).await?;
            assert_eq!(landed.candidate, built.candidate, "its candidate, never built again");
            assert!(lab.sim.contains(name, seam::TRUNK, h2));
            let id = eb.clone();
            let kept = hub.store.call(move |c| Ok(c.query_row(&format!("SELECT {RECORD_COLUMNS} FROM land_records WHERE entry_id = ?1"), [&id], record_row)?)).await?;
            let m2 = kept.landed.get(&format!("#{}", p2.number)).cloned().unwrap_or_default();
            let got = super::super::lands::get(&hub, name, &m2).await.map_err(|(_, m)| anyhow::anyhow!(m))?;
            assert_eq!(got.pointer("/land/rebased").and_then(Value::as_bool), Some(true), "applied again onto the trunk as it is: {got}");

            // #3: its head moves on its host after its candidate was tested. That's content Review
            // hasn't cleared: back to Review's gate at its new head, merged at neither. Once Review
            // clears the new head, it enters again, is built at it and merges at it.
            let ec = hand_over(&hub, &lab, &[p3]).await?;
            step_until(&hub, &lab, &ec, &["testing"], 2).await?;
            git_in(&lab.work, &["checkout", "-q", "three"])?;
            std::fs::write(lab.work.join("three.txt"), "three again\n")?;
            git_in(&lab.work, &["commit", "-qam", "three again"])?;
            let h3b = git_in(&lab.work, &["rev-parse", "HEAD"])?;
            git_in(&lab.work, &["checkout", "-q", seam::TRUNK])?;
            lab.sim.adopt(name, "three", &h3b, &[h3])?;
            let back = step_until(&hub, &lab, &ec, &["held"], 1).await?;
            assert!(back.reason.as_deref().is_some_and(|w| w.contains("its head moved") && w.contains("past what Review cleared")), "{:?}", back.reason);
            assert!(!lab.sim.contains(name, seam::TRUNK, h3) && !lab.sim.contains(name, seam::TRUNK, &h3b), "merged at neither");
            mirror(&hub, &lab.r.id, "github", p3.number, &h3b, &[("kim", "approve")]).await?;
            rechecked().lock().remove(&ec);
            let again = step_until(&hub, &lab, &ec, &["testing"], 2).await?;
            assert_eq!(git_in(&lab.bare, &["rev-parse", &format!("{}^2", again.candidate.clone().unwrap_or_default())])?, h3b, "built again at its new head");
            step_until(&hub, &lab, &ec, &["landed"], 3).await?;
            assert!(lab.sim.contains(name, seam::TRUNK, &h3b), "merged at the head Review cleared");
            Ok(())
        })
    }

    /// docs/ci.md W26-R19: a batch merges whole or not at all. Two entries tested as one candidate,
    /// the first's head moved on its host: the first goes back to Review's gate at its new head (W28-R105),
    /// and the second goes back to the queue, never merged onto a trunk without the first. Two more:
    /// the first merges whole, then its host refuses the second's merge as moved. The first lands, and
    /// the second goes back to Review's gate at its new head, never merged.
    #[test]
    fn w26_r19_a_batch_merges_whole_or_not_at_all() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let lab = merges_lab(&hub, "r-batch", "acme/batch", &["one", "two"]).await?;
            let ([_, h2], [p1, p2]) = (lab.heads.as_slice(), lab.pulls.as_slice()) else { anyhow::bail!("two changes") };
            let (ea, eb) = (hand_over(&hub, &lab, &[p1]).await?, hand_over(&hub, &lab, &[p2]).await?);
            // Both candidates built in one step, before either merges.
            let a = step_until(&hub, &lab, &ea, &["testing"], 1).await?;
            assert_eq!(entry_by_id(&hub, &eb).await?.map(|e| e.state), Some("testing".to_owned()));
            // One candidate for both, as the queue makes for entries that meet.
            let (batch, cand, base) = (a.batch.clone().unwrap_or_default(), a.candidate.clone().unwrap_or_default(), a.base.clone().unwrap_or_default());
            let (id, b2, c2, s2) = (eb.clone(), batch.clone(), cand, base);
            hub.store.call(move |c| Ok(c.execute("UPDATE land_queue SET batch = ?2, candidate = ?3, base = ?4 WHERE id = ?1", params![id, b2, c2, s2]).map(|_| ())?)).await?;
            let moved = lab.sim.push(lab.name, "one", "one", "one again")?;
            let path = lab.bare.to_string_lossy().into_owned();
            let seen = seen_of(&hub, &lab).await?;
            host_land_step(&hub, &lab.r, &path, seam::TRUNK, &land::Policy::default(), &lab.sim, &seen, &batch, &[ea.clone(), eb.clone()]).await?;
            let (first, second) = (entry_by_id(&hub, &ea).await?.ok_or_else(|| anyhow::anyhow!("no entry"))?, entry_by_id(&hub, &eb).await?.ok_or_else(|| anyhow::anyhow!("no entry"))?);
            assert_eq!((first.state.as_str(), first.hold.as_deref()), ("held", Some("gate")), "back with Review's gate");
            assert!(first.reason.as_deref().is_some_and(|w| w.contains("its head moved")), "{:?}", first.reason);
            assert!(first.host.iter().flat_map(|h| h.changes.iter()).all(|c| c.head == moved), "at its new head: {first:?}");
            assert_eq!(second.state, "queued");
            assert!(second.reason.as_deref().is_some_and(|w| w.contains("an entry beside it in its batch didn't merge")), "{:?}", second.reason);
            assert!(!lab.sim.contains(lab.name, seam::TRUNK, h2), "never merged without the first");

            // The first merged whole, then the second's merge refused as moved.
            let lab = merges_lab(&hub, "r-batch-moved", "acme/batch-moved", &["three", "four"]).await?;
            let ([h3, h4], [p3, p4]) = (lab.heads.as_slice(), lab.pulls.as_slice()) else { anyhow::bail!("two changes") };
            let (ec, ed) = (hand_over(&hub, &lab, &[p3]).await?, hand_over(&hub, &lab, &[p4]).await?);
            step_until(&hub, &lab, &ec, &["testing"], 1).await?;
            assert_eq!(entry_now(&hub, &ed).await?.state, "testing", "both built in one step, before either merges");
            let ids = [ec.clone(), ed.clone()];
            let (batch, path) = one_batch(&hub, &lab, &ids).await?;
            let (refusing, policy) = (Tuned { moved: p4.number.into(), merge_in: Some(lab.work.clone()), ..tuned(&lab.sim) }, land::Policy::default());
            let seen = seen_of(&hub, &lab).await?;
            let land_step = || host_land_step(&hub, &lab.r, &path, seam::TRUNK, &policy, &refusing, &seen, &batch, &ids);
            land_step().await?;
            land_step().await?;
            let (c, d) = (entry_now(&hub, &ec).await?, entry_now(&hub, &ed).await?);
            assert_eq!(c.state, "landed", "merged whole: landed, not queued again");
            assert_eq!((d.state.as_str(), d.hold.as_deref()), ("held", Some("gate")), "back with Review's gate");
            assert!(d.reason.as_deref().is_some_and(|w| w.contains("its head moved")), "{:?}", d.reason);
            assert!(d.host.iter().flat_map(|h| h.changes.iter()).all(|c| c.head == "f".repeat(40)), "at its new head: {d:?}");
            assert!(lab.sim.contains(lab.name, seam::TRUNK, h3) && !lab.sim.contains(lab.name, seam::TRUNK, h4));
            Ok(())
        })
    }

    /// docs/ci.md W26-R19: a stack its host merged part-way still lands what merged. #1 merges in
    /// the background; #2's head moves before it merges: #1 is recorded and signed into the trail,
    /// and the entry goes back to Review's gate at #2's new head, saying so. The trail misses no merge.
    #[test]
    fn w26_r19_a_stack_merged_part_way_lands_what_merged() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let lab = merges_lab(&hub, "r-stack", "acme/stack", &["one", "two"]).await?;
            let ([h1, h2], [p1, p2]) = (lab.heads.as_slice(), lab.pulls.as_slice()) else { anyhow::bail!("two changes") };
            let e = hand_over(&hub, &lab, &[p1, p2]).await?;
            step_until(&hub, &lab, &e, &["testing"], 2).await?;
            // #1's merge is under way on its host; then #2's head moves.
            step(&hub, &lab.r, seam::TRUNK).await?;
            let started = entry_by_id(&hub, &e).await?.ok_or_else(|| anyhow::anyhow!("no entry"))?;
            assert!(started.host.iter().flat_map(|h| h.changes.iter()).any(|c| c.number == p1.number && c.merging.is_some()), "{started:?}");
            lab.sim.push(lab.name, "two", "two", "two again")?;
            let out = step_until(&hub, &lab, &e, &["held"], 1).await?;
            assert!(out.reason.as_deref().is_some_and(|w| w.contains(&format!("#{}", p2.number)) && w.contains("its head moved") && w.contains("merged and signed")), "{:?}", out.reason);
            assert!(lab.sim.contains(lab.name, seam::TRUNK, h1) && !lab.sim.contains(lab.name, seam::TRUNK, h2));
            let id = e.clone();
            let kept = hub.store.call(move |c| Ok(c.query_row(&format!("SELECT {RECORD_COLUMNS} FROM land_records WHERE entry_id = ?1"), [&id], record_row)?)).await?;
            let m1 = kept.landed.get(&format!("#{}", p1.number)).cloned().unwrap_or_default();
            let got = super::super::lands::get(&hub, lab.name, &m1).await.map_err(|(_, m)| anyhow::anyhow!(m))?;
            assert_eq!(got.pointer("/land/via").and_then(Value::as_str), Some(queue::HOST_MERGES), "{got}");
            assert_eq!(got.pointer("/land/rebased").and_then(Value::as_bool), Some(true), "only part of what kiln tested landed: {got}");
            Ok(())
        })
    }

    /// The forge seam's simulator, tuned as a host can be:
    /// - while `pending` is set, its background merges are still under way: merges that take a while;
    /// - change `moved`'s merge is refused as moved, standing for a push between kiln's read of the
    ///   change and its merge;
    /// - change `wont`'s merge is refused for good: one its host won't do on any step;
    /// - with `merge_in`, its merges are real commits in that repository (the one kiln's mirror
    ///   fetches from), which the simulator then has too; with `later`, each comes back as a
    ///   background merge, its result read on the next step.
    struct Tuned {
        sim: seam::SimForge,
        pending: std::sync::atomic::AtomicBool,
        moved: std::sync::atomic::AtomicU64,
        wont: std::sync::atomic::AtomicU64,
        merge_in: Option<std::path::PathBuf>,
        later: std::sync::atomic::AtomicBool,
        made: parking_lot::Mutex<HashMap<String, (u64, String)>>,
    }

    /// `sim`, as it is.
    fn tuned(sim: &seam::SimForge) -> Tuned {
        Tuned { sim: sim.clone(), pending: false.into(), moved: 0u64.into(), wont: 0u64.into(), merge_in: None, later: false.into(), made: Default::default() }
    }

    impl Tuned {
        /// Change `number` merged at `head` in `work` with a merge commit, as its host would: the
        /// commit, which the simulator's trunk then is too.
        fn merge_for_real(&self, work: &std::path::Path, repo: &str, number: u64, head: &str) -> anyhow::Result<String> {
            let before = git_in(work, &["rev-parse", "HEAD"])?;
            git_in(work, &["merge", "-q", "--no-ff", "-m", &format!("Merge #{number}"), head])?;
            let commit = git_in(work, &["rev-parse", "HEAD"])?;
            self.sim.adopt(repo, seam::TRUNK, &commit, &[&before, head])?;
            Ok(commit)
        }
    }

    type Fut<'a, T> = futures::future::BoxFuture<'a, Result<T, seam::Refusal>>;

    impl Forge for Tuned {
        fn abilities<'a>(&'a self, repo: &'a str) -> Fut<'a, seam::Abilities> { self.sim.abilities(repo) }
        fn open<'a>(&'a self, repo: &'a str, open: seam::Open) -> Fut<'a, seam::Change> { self.sim.open(repo, open) }
        fn change<'a>(&'a self, repo: &'a str, number: u64, etag: Option<&'a str>) -> Fut<'a, seam::Fresh<seam::Change>> { self.sim.change(repo, number, etag) }
        fn changes<'a>(&'a self, repo: &'a str, etag: Option<&'a str>) -> Fut<'a, seam::Fresh<Vec<seam::Change>>> { self.sim.changes(repo, etag) }
        fn update<'a>(&'a self, repo: &'a str, number: u64, update: seam::Update) -> Fut<'a, seam::Updated> { self.sim.update(repo, number, update) }
        fn stack<'a>(&'a self, repo: &'a str, changes: Vec<u64>) -> Fut<'a, seam::Stack> { self.sim.stack(repo, changes) }
        fn stack_add<'a>(&'a self, repo: &'a str, stack: u64, changes: Vec<u64>) -> Fut<'a, seam::Stack> { self.sim.stack_add(repo, stack, changes) }
        fn stack_of<'a>(&'a self, repo: &'a str, number: u64) -> Fut<'a, Option<seam::Stack>> { self.sim.stack_of(repo, number) }
        fn unstack<'a>(&'a self, repo: &'a str, stack: u64) -> Fut<'a, Option<seam::Stack>> { self.sim.unstack(repo, stack) }
        fn merge<'a>(&'a self, repo: &'a str, number: u64, merge: seam::Merge) -> Fut<'a, seam::Merging> {
            use std::sync::atomic::Ordering::SeqCst;
            if self.moved.load(SeqCst) == number {
                return Box::pin(async { Err(seam::Refusal::Moved { head: "f".repeat(40) }) });
            }
            if self.wont.load(SeqCst) == number {
                return Box::pin(async { Err(seam::Refusal::NotLinear { why: "its history isn't linear: it needs updating first".into() }) });
            }
            if let Some(work) = &self.merge_in {
                let made = self.merge_for_real(work, repo, number, &merge.expected).map(|commit| {
                    if self.later.load(SeqCst) {
                        let id = format!("real-{number}-{commit}");
                        self.made.lock().insert(id.clone(), (number, commit));
                        seam::Merging::Started { id }
                    } else {
                        seam::Merging::Merged { merged: vec![number], commit }
                    }
                });
                return Box::pin(async move { made.map_err(|e| seam::Refusal::Unavailable(format!("{e:#}"))) });
            }
            self.sim.merge(repo, number, merge)
        }
        fn merge_result<'a>(&'a self, repo: &'a str, number: u64, id: &'a str) -> Fut<'a, seam::MergeResult> {
            if self.pending.load(std::sync::atomic::Ordering::SeqCst) {
                return Box::pin(async { Ok(seam::MergeResult::Pending) });
            }
            if let Some((n, commit)) = self.made.lock().get(id).cloned() {
                return Box::pin(async move { Ok(seam::MergeResult::Merged { merged: vec![n], commit }) });
            }
            self.sim.merge_result(repo, number, id)
        }
        fn enqueue<'a>(&'a self, repo: &'a str, number: u64, expected: &'a str) -> Fut<'a, Vec<u64>> { self.sim.enqueue(repo, number, expected) }
        fn dequeue<'a>(&'a self, repo: &'a str, number: u64) -> Fut<'a, Vec<u64>> { self.sim.dequeue(repo, number) }
        fn queued<'a>(&'a self, repo: &'a str) -> Fut<'a, Vec<u64>> { self.sim.queued(repo) }
        fn draft<'a>(&'a self, repo: &'a str, number: u64, by: &'a str, draft: seam::Draft) -> Fut<'a, u64> { self.sim.draft(repo, number, by, draft) }
        fn drafts<'a>(&'a self, repo: &'a str, number: u64, by: &'a str) -> Fut<'a, Vec<seam::Comment>> { self.sim.drafts(repo, number, by) }
        fn publish<'a>(&'a self, repo: &'a str, number: u64, by: &'a str, verdict: seam::Verdict) -> Fut<'a, seam::Review> { self.sim.publish(repo, number, by, verdict) }
        fn comments<'a>(&'a self, repo: &'a str, number: u64) -> Fut<'a, Vec<seam::Comment>> { self.sim.comments(repo, number) }
        fn rate<'a>(&'a self, repo: &'a str) -> Fut<'a, seam::Rate> { self.sim.rate(repo) }
        fn verifications<'a>(&'a self, repo: &'a str, number: u64) -> Fut<'a, Vec<seam::Verification>> { self.sim.verifications(repo, number) }
    }

    /// Entries `ids`, each built as its own candidate, put in one batch with the first's: as the
    /// queue batches entries that meet. The batch, and kiln's mirror's path.
    async fn one_batch(hub: &Arc<Hub>, lab: &Lab, ids: &[String]) -> anyhow::Result<(String, String)> {
        let first = entry_by_id(hub, ids.first().map(String::as_str).unwrap_or_default()).await?.ok_or_else(|| anyhow::anyhow!("no entry"))?;
        let (batch, cand, base) = (first.batch.unwrap_or_default(), first.candidate.unwrap_or_default(), first.base.unwrap_or_default());
        for id in ids.iter().skip(1) {
            let (id, b, c, s) = (id.clone(), batch.clone(), cand.clone(), base.clone());
            hub.store.call(move |x| Ok(x.execute("UPDATE land_queue SET batch = ?2, candidate = ?3, base = ?4 WHERE id = ?1", params![id, b, c, s]).map(|_| ())?)).await?;
        }
        Ok((batch, lab.bare.to_string_lossy().into_owned()))
    }

    /// What the planner sees of `lab`'s queue now, as `step` gives it to a land.
    async fn seen_of(hub: &Arc<Hub>, lab: &Lab) -> anyhow::Result<Seen> {
        let path = lab.bare.to_string_lossy().into_owned();
        let live = entries(hub, &lab.r.id, seam::TRUNK, &["queued", "testing"]).await?;
        let tip = rev_parse(hub, &path, &format!("refs/heads/{}^{{commit}}", seam::TRUNK)).await?;
        Ok(see(hub, &lab.r, &path, &land::Policy::default(), &tip, &live).await)
    }

    /// Entry `id` as it stands.
    async fn entry_now(hub: &Arc<Hub>, id: &str) -> anyhow::Result<Entry> {
        entry_by_id(hub, id).await?.ok_or_else(|| anyhow::anyhow!("no entry {id}"))
    }

    /// Each candidate of `lab` under test that has no result yet gets one for `ci`, standing for the
    /// trunk's gating workflow (docs/ci.md W26-R19): failing where `fails` says so of its entries,
    /// else passing.
    async fn report(hub: &Arc<Hub>, lab: &Lab, fails: &(dyn Fn(&[Entry]) -> bool + Sync)) -> anyhow::Result<()> {
        let mut batches: BTreeMap<String, Vec<Entry>> = BTreeMap::new();
        for e in entries(hub, &lab.r.id, seam::TRUNK, &["testing"]).await? {
            if let Some(b) = e.batch.clone() {
                batches.entry(b).or_default().push(e);
            }
        }
        for es in batches.into_values() {
            let Some(cand) = es.first().and_then(|e| e.candidate.clone()) else { continue };
            let conclusion = if fails(&es) { "failure" } else { "success" };
            let (repo_id, ids) = (lab.r.id.clone(), es.iter().map(|e| e.id.clone()).collect::<Vec<_>>());
            hub.store
                .call(move |c| {
                    for id in &ids {
                        c.execute("UPDATE land_queue SET checks = '[\"ci\"]' WHERE id = ?1", [id])?;
                    }
                    c.execute(
                        "INSERT OR IGNORE INTO runs (id, repo_id, number, pipeline, event, branch, sha, message, author, status, conclusion, created_at)
                         VALUES (?1, ?2, (SELECT COALESCE(MAX(number), 0) + 1 FROM runs WHERE repo_id = ?2), 'ci', 'merge_group', 'main', ?3, 'ci', 'kiln', 'completed', ?4, 0)",
                        params![format!("ci-{cand}"), repo_id, cand, conclusion],
                    )?;
                    Ok(())
                })
                .await?;
        }
        Ok(())
    }

    /// How many land statements the trail holds.
    async fn lands_in_trail(hub: &Arc<Hub>) -> anyhow::Result<i64> {
        hub.store.call(|c| Ok(c.query_row("SELECT COUNT(*) FROM trail WHERE payload_type = ?1", [kiln_ci::identity::landing::PAYLOAD_LAND], |x| x.get::<_, i64>(0))?)).await
    }

    /// docs/ci.md W26-R19: a batch stops only once no merge of it is under way, and an entry merged
    /// whole beside one that didn't lands. #1 and #2 are tested as one candidate, and #1's merge is
    /// under way on its host when #2's head moves: the batch waits for the merge's result, then #1
    /// lands, signed into the trail once, and #2 goes back to Review's gate, never merged.
    #[test]
    fn w26_r19_a_batch_stops_once_its_merges_are_done() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let lab = merges_lab(&hub, "r-slow", "acme/slow", &["one", "two"]).await?;
            let ([h1, h2], [p1, p2]) = (lab.heads.as_slice(), lab.pulls.as_slice()) else { anyhow::bail!("two changes") };
            let (ea, eb) = (hand_over(&hub, &lab, &[p1]).await?, hand_over(&hub, &lab, &[p2]).await?);
            step_until(&hub, &lab, &ea, &["testing"], 1).await?;
            assert_eq!(entry_now(&hub, &eb).await?.state, "testing", "both built in one step, before either merges");
            let ids = [ea.clone(), eb.clone()];
            let (batch, path) = one_batch(&hub, &lab, &ids).await?;
            let (slow, policy) = (Tuned { pending: true.into(), ..tuned(&lab.sim) }, land::Policy::default());
            let seen = seen_of(&hub, &lab).await?;
            let land_step = || host_land_step(&hub, &lab.r, &path, seam::TRUNK, &policy, &slow, &seen, &batch, &ids);
            land_step().await?;
            lab.sim.push(lab.name, "two", "two", "two again")?;
            land_step().await?;
            let (a, b) = (entry_now(&hub, &ea).await?, entry_now(&hub, &eb).await?);
            assert_eq!((a.state.as_str(), b.state.as_str()), ("testing", "testing"), "#1's merge is under way: the batch waits");
            assert!(a.host.iter().flat_map(|h| h.changes.iter()).any(|c| c.number == p1.number && c.merging.is_some()), "{a:?}");
            slow.pending.store(false, std::sync::atomic::Ordering::SeqCst);
            land_step().await?;
            let (a, b) = (entry_now(&hub, &ea).await?, entry_now(&hub, &eb).await?);
            assert_eq!(a.state, "landed", "merged whole: landed, not queued again");
            assert_eq!(b.state, "held", "back with Review's gate");
            assert!(b.reason.as_deref().is_some_and(|w| w.contains("its head moved")), "{:?}", b.reason);
            assert!(lab.sim.contains(lab.name, seam::TRUNK, h1) && !lab.sim.contains(lab.name, seam::TRUNK, h2));
            assert_eq!(lands_in_trail(&hub).await?, 1, "#1's land, signed once");
            Ok(())
        })
    }

    /// docs/ci.md W26-R19, W28-R102: a batch its host merged part of is checked again before its
    /// next merge. A stack of #1 and #2, its host's merges real commits: #1's is under way when
    /// someone else's change lands on the trunk, touching #2's file. Once #1 has merged, #2 isn't
    /// merged onto that land unchecked: it goes back to be tested again, and #1 lands, signed.
    #[test]
    fn w26_r19_a_resumed_batch_checks_the_trunk_again() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let lab = merges_lab(&hub, "r-resume", "acme/resume", &["one", "two"]).await?;
            let ([_, h2], [p1, p2]) = (lab.heads.as_slice(), lab.pulls.as_slice()) else { anyhow::bail!("two changes") };
            let host = Tuned { merge_in: Some(lab.work.clone()), later: true.into(), ..tuned(&lab.sim) };
            forges().lock().insert(lab.r.id.clone(), (Arc::new(host) as Arc<dyn Forge>, lab.name.to_owned()));
            let e = hand_over(&hub, &lab, &[p1, p2]).await?;
            step_until(&hub, &lab, &e, &["testing"], 2).await?;
            // #1's merge is under way; #2's waits on it.
            step(&hub, &lab.r, seam::TRUNK).await?;
            let mid = entry_now(&hub, &e).await?;
            assert!(mid.host.iter().flat_map(|h| h.changes.iter()).any(|c| c.number == p1.number && c.merging.is_some()), "{mid:?}");
            // Meanwhile someone else's change lands on the trunk, onto #1's merge, touching #2's file.
            let m1 = git_in(&lab.work, &["rev-parse", "HEAD"])?;
            std::fs::write(lab.work.join("two.txt"), "someone else's two\n")?;
            git_in(&lab.work, &["add", "two.txt"])?;
            git_in(&lab.work, &["commit", "-qm", "someone else's two"])?;
            let theirs = git_in(&lab.work, &["rev-parse", "HEAD"])?;
            lab.sim.adopt(lab.name, seam::TRUNK, &theirs, &[&m1])?;
            let back = step_until(&hub, &lab, &e, &["queued"], 1).await?;
            assert!(back.reason.as_deref().is_some_and(|w| w.contains("tested again") && w.contains(&format!("#{} merged and signed", p1.number))), "{:?}", back.reason);
            assert!(back.host.iter().flat_map(|h| h.changes.iter()).any(|c| c.number == p1.number && c.state == "merged" && c.landed), "#1 landed: {back:?}");
            assert_eq!(lands_in_trail(&hub).await?, 1, "#1's land, signed");
            assert!(!lab.sim.contains(lab.name, seam::TRUNK, h2), "#2 never merged onto a land it wasn't checked against");
            Ok(())
        })
    }

    /// docs/ci.md W26-R19: what kiln lands of an entry is signed once. A stack of #1 and #2, and #3
    /// beside it, are tested as one candidate; #1 merges, then #3's head moves (#3 goes back to Review's
    /// gate): #1 lands, signed, and the stack goes back to the queue with #2 alone. When #2 lands, its
    /// statement carries #2 only, and the entry's record names both.
    #[test]
    fn w26_r19_what_kiln_lands_of_an_entry_is_signed_once() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let lab = merges_lab(&hub, "r-once", "acme/once", &["one", "two", "three"]).await?;
            let ([h1, h2, h3], [p1, p2, p3]) = (lab.heads.as_slice(), lab.pulls.as_slice()) else { anyhow::bail!("three changes") };
            let (ea, eb) = (hand_over(&hub, &lab, &[p1, p2]).await?, hand_over(&hub, &lab, &[p3]).await?);
            step_until(&hub, &lab, &ea, &["testing"], 1).await?;
            assert_eq!(entry_now(&hub, &eb).await?.state, "testing", "both built in one step, before either merges");
            let ids = [ea.clone(), eb.clone()];
            let (batch, path) = one_batch(&hub, &lab, &ids).await?;
            let policy = land::Policy::default();
            let seen = seen_of(&hub, &lab).await?;
            let land_step = || host_land_step(&hub, &lab.r, &path, seam::TRUNK, &policy, &lab.sim, &seen, &batch, &ids);
            land_step().await?;
            lab.sim.push(lab.name, "three", "three", "three again")?;
            land_step().await?;
            let back = entry_now(&hub, &ea).await?;
            assert_eq!(back.state, "queued");
            assert!(back.reason.as_deref().is_some_and(|w| w.contains(&format!("#{} merged and signed", p1.number))), "{:?}", back.reason);
            assert_eq!(entry_now(&hub, &eb).await?.state, "held");
            assert_eq!(lands_in_trail(&hub).await?, 1);
            // Back in the queue with #2 alone: built, tested and merged by its host.
            step_until(&hub, &lab, &ea, &["landed"], 6).await?;
            assert!(lab.sim.contains(lab.name, seam::TRUNK, h1) && lab.sim.contains(lab.name, seam::TRUNK, h2) && !lab.sim.contains(lab.name, seam::TRUNK, h3));
            assert_eq!(lands_in_trail(&hub).await?, 2, "#1's land, then #2's: none twice");
            let id = ea.clone();
            let kept = hub.store.call(move |c| Ok(c.query_row(&format!("SELECT {RECORD_COLUMNS} FROM land_records WHERE entry_id = ?1"), [&id], record_row)?)).await?;
            assert!([p1.number, p2.number].iter().all(|n| kept.landed.contains_key(&format!("#{n}"))), "the record names all the entry carries: {:?}", kept.landed);
            let m2 = kept.landed.get(&format!("#{}", p2.number)).cloned().unwrap_or_default();
            let got = super::super::lands::get(&hub, lab.name, &m2).await.map_err(|(_, m)| anyhow::anyhow!(m))?;
            let carried: Vec<&str> = got.pointer("/land/batch").and_then(Value::as_array).into_iter().flatten().filter_map(|d| d.get("diff").and_then(Value::as_str)).collect();
            assert_eq!(carried, [format!("#{}", p2.number).as_str()], "{got}");
            Ok(())
        })
    }

    /// docs/ci.md W26-R19, W28-R35, R36: host changes of a repository whose host has no queue are
    /// kiln's queue's entries like its own, never landed one at a time. Four: #1 and #2 in components
    /// of their own, #3 and #4 sharing one (both edit shared.txt), and #4 failing the trunk's gating
    /// check; the trunk's policy batches up to 4. #1 and #2 land without waiting for each other or
    /// for the shared component; #3 and #4 are tested in one window together, then bisected; #4 is
    /// ejected, saying why, and #3 lands.
    #[test]
    fn w26_r19_host_changes_land_side_by_side() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let lines = |at: usize| -> String { (1..=10).map(|k| if k == at { format!("line {k}, changed\n") } else { format!("line {k}\n") }).collect() };
            let files = [("README.md".to_owned(), "README.md\n".to_owned()), ("shared.txt".to_owned(), lines(0)), (".kiln/review.yaml".to_owned(), "queue:\n  batch: 4\n".to_owned())];
            let branches = [
                ("one".to_owned(), vec![("one.txt".to_owned(), "one\n".to_owned())]),
                ("two".to_owned(), vec![("two.txt".to_owned(), "two\n".to_owned())]),
                ("three".to_owned(), vec![("shared.txt".to_owned(), lines(1))]),
                ("four".to_owned(), vec![("shared.txt".to_owned(), lines(10))]),
            ];
            let lab = lab_with(&hub, "r-sides", "acme/sides", &files, &branches).await?;
            let ([h1, h2, h3, h4], [p1, p2, p3, p4]) = (lab.heads.as_slice(), lab.pulls.as_slice()) else { anyhow::bail!("four changes") };
            let (e1, e2) = (hand_over(&hub, &lab, &[p1]).await?, hand_over(&hub, &lab, &[p2]).await?);
            let (e3, e4) = (hand_over(&hub, &lab, &[p3]).await?, hand_over(&hub, &lab, &[p4]).await?);
            // One step builds them all: #1's candidate and #2's on their own, #3's and #4's together.
            step(&hub, &lab.r, seam::TRUNK).await?;
            let [a, b, c, d] = [entry_now(&hub, &e1).await?, entry_now(&hub, &e2).await?, entry_now(&hub, &e3).await?, entry_now(&hub, &e4).await?];
            assert!([&a, &b, &c, &d].iter().all(|e| e.state == "testing"), "{:?}", [&a.state, &b.state, &c.state, &d.state]);
            assert!(a.batch != b.batch && a.batch != c.batch && b.batch != c.batch, "#1 and #2 side by side, apart from #3 and #4");
            assert_eq!(c.batch, d.batch, "#3 and #4 tested in one window together");
            // #4 fails `ci` wherever it's tested; everything else passes.
            let fails = |es: &[Entry]| es.iter().any(|e| e.id == e4);
            let mut independent_first = false;
            for _ in 0..16 {
                report(&hub, &lab, &fails).await?;
                step(&hub, &lab.r, seam::TRUNK).await?;
                let [a, b, c, d] = [entry_now(&hub, &e1).await?, entry_now(&hub, &e2).await?, entry_now(&hub, &e3).await?, entry_now(&hub, &e4).await?];
                if a.state == "landed" && b.state == "landed" && c.state != "landed" && d.state != "ejected" {
                    independent_first = true;
                }
                if c.state == "landed" && d.state == "ejected" {
                    break;
                }
            }
            let [a, b, c, d] = [entry_now(&hub, &e1).await?, entry_now(&hub, &e2).await?, entry_now(&hub, &e3).await?, entry_now(&hub, &e4).await?];
            assert_eq!([a.state.as_str(), b.state.as_str(), c.state.as_str(), d.state.as_str()], ["landed", "landed", "landed", "ejected"]);
            assert!(independent_first, "#1 and #2 landed while the shared component was still at work");
            assert!(d.reason.as_deref().is_some_and(|w| w.contains("ci")), "says why: {:?}", d.reason);
            for h in [h1, h2, h3] {
                assert!(lab.sim.contains(lab.name, seam::TRUNK, h), "{h} merged");
            }
            assert!(!lab.sim.contains(lab.name, seam::TRUNK, h4), "#4 never merged");
            Ok(())
        })
    }

    /// docs/ci.md W26-R19, W27-R07: a land its host merged as kiln's queue tested it is signed as
    /// tested. The host's merge is a real commit in the repository kiln's mirror fetches from, onto
    /// the candidate's base and with the candidate's tree. The land reads back not applied again,
    /// from the base, and kiln verify says kiln's queue tested the candidate and its host merged it.
    #[test]
    fn w26_r19_a_host_merge_of_what_kiln_tested_is_signed_as_tested() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let lab = merges_lab(&hub, "r-tested", "acme/tested", &["one"]).await?;
            let ([h1], [p1]) = (lab.heads.as_slice(), lab.pulls.as_slice()) else { anyhow::bail!("one change") };
            forges().lock().insert(lab.r.id.clone(), (Arc::new(Tuned { merge_in: Some(lab.work.clone()), ..tuned(&lab.sim) }) as Arc<dyn Forge>, lab.name.to_owned()));
            let e = hand_over(&hub, &lab, &[p1]).await?;
            let cand = step_until(&hub, &lab, &e, &["testing"], 2).await?.candidate.unwrap_or_default();
            step_until(&hub, &lab, &e, &["landed"], 2).await?;
            let merged = git_in(&lab.work, &["rev-parse", "HEAD"])?;
            assert_eq!(git_in(&lab.work, &["rev-parse", "HEAD^1"])?, lab.t, "its host merged it onto the candidate's base");
            assert!(lab.sim.contains(lab.name, seam::TRUNK, h1));
            let got = super::super::lands::get(&hub, lab.name, &merged).await.map_err(|(_, m)| anyhow::anyhow!(m))?;
            let said = |k: &str| got.pointer(&format!("/land/{k}")).and_then(Value::as_str).unwrap_or_default().to_owned();
            assert_eq!(got.pointer("/land/rebased").and_then(Value::as_bool), Some(false), "landed as tested: {got}");
            assert_eq!((said("from"), said("base"), said("candidate"), said("via")), (lab.t.clone(), lab.t.clone(), cand.clone(), queue::HOST_MERGES.to_owned()), "{got}");
            let land: kiln_ci::identity::landing::Land = serde_json::from_value(got.get("land").cloned().unwrap_or_default())?;
            let words = kiln_ci::identity::verify::landed(&merged, &land).map_err(|e| anyhow::anyhow!(e))?;
            assert!(words.contains(&format!("kiln's queue tested candidate {}", short(&cand))) && words.contains("and its host merged it"), "{words}");
            Ok(())
        })
    }

    /// docs/ci.md W26-R19: a merge its host won't do on any step takes its entry out, saying why,
    /// rather than holding its batch for ever. It's never merged.
    #[test]
    fn w26_r19_a_merge_its_host_wont_do_takes_its_entry_out() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let lab = merges_lab(&hub, "r-wont", "acme/wont", &["one"]).await?;
            let ([h1], [p1]) = (lab.heads.as_slice(), lab.pulls.as_slice()) else { anyhow::bail!("one change") };
            forges().lock().insert(lab.r.id.clone(), (Arc::new(Tuned { wont: p1.number.into(), ..tuned(&lab.sim) }) as Arc<dyn Forge>, lab.name.to_owned()));
            let e = hand_over(&hub, &lab, &[p1]).await?;
            step_until(&hub, &lab, &e, &["testing"], 2).await?;
            let out = step_until(&hub, &lab, &e, &["ejected"], 1).await?;
            assert!(out.reason.as_deref().is_some_and(|w| w.contains("its host won't merge it") && w.contains("isn't linear")), "{:?}", out.reason);
            assert!(!lab.sim.contains(lab.name, seam::TRUNK, h1), "never merged");
            Ok(())
        })
    }

    /// docs/ci.md W26-R19, W27-R07: a land through a host's own queue is signed into the trail, as
    /// kiln's server saw it: the statement names the host's queue, the change at the version Review
    /// cleared and the commit it merged as, and reads back from `GET /api/repos/:repo/lands/:commit`.
    /// Its record keeps the trunk's policy.
    #[test]
    fn w26_r19_a_host_queues_land_is_signed_into_the_trail() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            crate::identities::own_identities(&hub.store, &hub.pki).await?;
            let sim = seam::SimForge::github();
            let (name, r) = ("acme/app", hub.store.upsert_repo(hosted("r-signed", "acme/app")).await?);
            sim.add_repo(name);
            sim.set_queue(name, true);
            sim.push(name, "one", seam::TRUNK, "one")?;
            let one = sim.open(name, seam::Open { head: "one".into(), base: seam::TRUNK.into(), title: "change one".into(), by: "ada".into() }).await?;
            forges().lock().insert(r.id.clone(), (Arc::new(sim.clone()) as Arc<dyn Forge>, name.to_owned()));
            mirror(&hub, &r.id, "github", one.number, &one.head_sha, &[("kim", "approve")]).await?;
            let a = enqueue_route(&hub, name, &json!({ "changes": [{ "number": one.number, "head": one.head_sha }] }), "user:ada").await.map_err(|(_, m)| anyhow::anyhow!(m))?;
            let entry = a.get("entry").and_then(Value::as_str).unwrap_or_default().to_owned();
            host_step(&hub, &r, seam::TRUNK).await?;
            assert_eq!(sim.land_queue(name)?, vec![one.number]);
            host_step(&hub, &r, seam::TRUNK).await?;
            assert_eq!(state_of(&hub, &entry).await?.0, "landed");
            let id = entry.clone();
            let kept = hub.store.call(move |c| Ok(c.query_row(&format!("SELECT {RECORD_COLUMNS} FROM land_records WHERE entry_id = ?1"), [&id], record_row)?)).await?;
            let commit = kept.landed.get(&format!("#{}", one.number)).cloned().unwrap_or_default();
            let got = super::super::lands::get(&hub, name, &commit).await.map_err(|(_, m)| anyhow::anyhow!(m))?;
            assert_eq!(got.pointer("/land/via").and_then(Value::as_str), Some("merge queue"), "{got}");
            assert_eq!(got.pointer("/land/batch/0/diff").and_then(Value::as_str), Some(format!("#{}", one.number).as_str()), "{got}");
            assert_eq!(got.pointer("/land/to").and_then(Value::as_str), Some(commit.as_str()), "{got}");
            assert!(got.get("signer").and_then(Value::as_str).is_some_and(|s| s.contains("server")), "signed as kiln's server: {got}");
            let taken = hub.store.call(|c| Ok(c.query_row("SELECT COUNT(*) FROM trail WHERE payload_type = ?1", [kiln_ci::identity::landing::PAYLOAD_LAND], |x| x.get::<_, i64>(0))?)).await?;
            assert_eq!(taken, 1, "in the trail");
            Ok(())
        })
    }

    /// docs/ci.md W26-R19, W28-R109: a stack its host's queue merged part-way still lands what
    /// merged. #1 and #2 go in GitHub's merge queue; the queue merges #1, and #2 leaves it unmerged
    /// (its head moved there). #1 is recorded and signed into the trail, and the entry is out,
    /// saying so. The trail misses no merge.
    #[test]
    fn w26_r19_a_stack_its_hosts_queue_merged_part_way_lands_what_merged() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            crate::identities::own_identities(&hub.store, &hub.pki).await?;
            let sim = seam::SimForge::github();
            let (name, r) = ("acme/part", hub.store.upsert_repo(hosted("r-part", "acme/part")).await?);
            sim.add_repo(name);
            sim.set_queue(name, true);
            let mut pulls = Vec::new();
            for b in ["one", "two"] {
                sim.push(name, b, seam::TRUNK, b)?;
                let c = sim.open(name, seam::Open { head: b.into(), base: seam::TRUNK.into(), title: format!("change {b}"), by: "ada".into() }).await?;
                mirror(&hub, &r.id, "github", c.number, &c.head_sha, &[("kim", "approve")]).await?;
                pulls.push(c);
            }
            let [one, two] = pulls.as_slice() else { anyhow::bail!("two changes") };
            forges().lock().insert(r.id.clone(), (Arc::new(sim.clone()) as Arc<dyn Forge>, name.to_owned()));
            let ask = json!({ "changes": [{ "number": one.number, "head": one.head_sha }, { "number": two.number, "head": two.head_sha }] });
            let a = enqueue_route(&hub, name, &ask, "user:ada").await.map_err(|(_, m)| anyhow::anyhow!(m))?;
            let entry = a.get("entry").and_then(Value::as_str).unwrap_or_default().to_owned();
            // Each goes in its host's queue, a step each.
            host_step(&hub, &r, seam::TRUNK).await?;
            host_step(&hub, &r, seam::TRUNK).await?;
            assert_eq!(sim.queued(name).await?, vec![one.number, two.number]);
            // #2's head moves there, so the queue merges #1 and ejects #2 unmerged.
            sim.push(name, "two", "two", "two again")?;
            assert_eq!(sim.land_queue(name)?, vec![one.number]);
            host_step(&hub, &r, seam::TRUNK).await?;
            let out = entry_now(&hub, &entry).await?;
            assert_eq!(out.state, "ejected");
            assert!(out.reason.as_deref().is_some_and(|w| w.contains(&format!("#{} left its host's queue unmerged", two.number)) && w.contains(&format!("(#{} merged and signed)", one.number))), "{:?}", out.reason);
            let id = entry.clone();
            let kept = hub.store.call(move |c| Ok(c.query_row(&format!("SELECT {RECORD_COLUMNS} FROM land_records WHERE entry_id = ?1"), [&id], record_row)?)).await?;
            assert_eq!(kept.queue, "merge queue");
            let m1 = kept.landed.get(&format!("#{}", one.number)).cloned().unwrap_or_default();
            assert!(sim.contains(name, seam::TRUNK, &m1), "#1 is on its host's trunk");
            let got = super::super::lands::get(&hub, name, &m1).await.map_err(|(_, m)| anyhow::anyhow!(m))?;
            assert_eq!(got.pointer("/land/via").and_then(Value::as_str), Some("merge queue"), "{got}");
            assert_eq!(got.pointer("/land/batch/0/diff").and_then(Value::as_str), Some(format!("#{}", one.number).as_str()), "{got}");
            assert_eq!(lands_in_trail(&hub).await?, 1, "#1's land, signed once");
            Ok(())
        })
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
            let (name, r) = ("acme/app", hub.store.upsert_repo(hosted("r-host", "acme/app")).await?);
            sim.add_repo(name);
            sim.push(name, "one", seam::TRUNK, "one")?;
            sim.push(name, "two", seam::TRUNK, "two")?;
            let open = |head: &str| seam::Open { head: head.into(), base: seam::TRUNK.into(), title: format!("change {head}"), by: "ada".into() };
            let one = sim.open(name, open("one")).await?;
            let two = sim.open(name, open("two")).await?;
            forges().lock().insert(r.id.clone(), (Arc::new(sim.clone()) as Arc<dyn Forge>, name.to_owned()));
            // Review cleared both (W26-R19): a host's queue takes only what Review's gate clears.
            for c in [&one, &two] {
                mirror(&hub, &r.id, "github", c.number, &c.head_sha, &[("kim", "approve")]).await?;
            }
            // No queue on the host: kiln's queue is the route, its host merging what kiln tests
            // (W26-R19), and the host's queue isn't asked.
            let kilns = enqueue_host(&hub, &r, seam::TRUNK, &[(one.number, one.head_sha.clone())], "user:ada").await.map_err(|(_, m)| anyhow::anyhow!(m))?;
            assert_eq!(kilns.get("route").and_then(Value::as_str), Some(queue::HOST_MERGES));
            withdraw(&hub, kilns.get("entry").and_then(Value::as_str).unwrap_or_default(), "user:rev", Some("its host has a queue now")).await.map_err(|(_, m)| anyhow::anyhow!(m))?;
            sim.set_queue(name, true);
            // The route reads each change on its host first: one whose head moved since Review
            // cleared it is refused, naming the head it's at; a change without a head isn't one.
            sim.push(name, "three", seam::TRUNK, "three")?;
            let three = sim.open(name, open("three")).await?;
            let moved = sim.push(name, "three", "three", "three again")?;
            let stale = enqueue_route(&hub, name, &json!({ "changes": [{ "number": three.number, "head": three.head_sha }] }), "user:ada").await;
            assert!(matches!(&stale, Err((StatusCode::CONFLICT, why)) if why.contains(&moved)), "{stale:?}");
            let bad = enqueue_route(&hub, name, &json!({ "changes": [{ "number": three.number }] }), "user:ada").await;
            assert!(matches!(&bad, Err((StatusCode::BAD_REQUEST, _))), "{bad:?}");
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
