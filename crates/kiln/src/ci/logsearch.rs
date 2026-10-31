//! Log search (docs/ci.md W19-R04, W17-R07, W20's `ci.legs.search`, `ci.legs.failure_excerpt`).
//!
//! U05: finished legs' step logs go into ordinary Amphora SQL documents and Unicode trigram
//! bitmaps (literal, case-insensitive substrings of three characters or more) in segments of
//! [`SEGMENT_LINES`] lines, escape codes stripped. The same indexer advances durable finite
//! posting, fresh completion, historical reconstruction and pruning queues. Search orders by
//! captured job completion, independently of indexing interleaving. Ready means this received
//! prefix was indexed, not proof that the separate log delivery lane delivered a closed stream.
//!
//! - `GET /api/ci/logs/search?q=&repo=&workflow=&branch=&conclusion=&since=&until=&limit=`:
//!   matching lines across runs, newest first.
//! - `GET /api/ci/legs/:id/logs/search?q=`: one leg's matching lines, read from its files (so a
//!   running leg's too).
//! - `GET /api/ci/legs/:id/logs/failure`: the failed step's excerpt, around its first error.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use futures::FutureExt as _;
use http::StatusCode;
use crate::sql::{Connection, OptionalExtension, params};
use kiln_ci::logsearch::{lines_with, trigrams};
use serde::Serialize;
use serde_json::{Value, json};

use super::Fail;
use crate::http::server::{self as srv, Response};
use crate::hub::Hub;

/// Lines per indexed segment: a hit's place is its segment, then the line within it.
pub const SEGMENT_LINES: usize = 100;
/// A step's log past this isn't indexed beyond it (the log keeps it).
const MAX_INDEXED: usize = 8 << 20;

fn internal(e: anyhow::Error) -> Fail {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}

/// A log's text as read: escape codes gone, `\r\n` as `\n`, a line rewritten with `\r` as its
/// last version (what a terminal shows).
pub fn plain(raw: &[u8]) -> String {
    let text = crate::harness::claude::strip_ansi(&String::from_utf8_lossy(raw));
    text.split('\n').map(|l| l.trim_end_matches('\r').rsplit('\r').next().unwrap_or_default()).collect::<Vec<_>>().join("\n")
}

mod index;
pub(crate) use index::plain_reader;
#[cfg(test)]
use index::index_step;

/// What to look for, and where.
#[derive(Debug, Clone, Default)]
pub struct Query {
    pub q: String,
    pub repo: Option<String>,
    pub workflow: Option<String>,
    pub branch: Option<String>,
    pub conclusion: Option<String>,
    pub since: Option<i64>,
    pub until: Option<i64>,
    pub limit: usize,
}

/// A matching line.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Hit {
    pub run: String,
    pub number: i64,
    pub repo: String,
    pub workflow: String,
    pub branch: String,
    pub conclusion: Option<String>,
    pub created_at: i64,
    pub leg: String,
    pub job: String,
    pub step: u32,
    pub step_name: Option<String>,
    /// 1-based, within the step's log.
    pub line: usize,
    pub text: String,
    /// Where the match is in `text` (bytes).
    pub at: usize,
}

/// Matching lines across runs, newest first (at most five a segment, `limit` in all).
pub fn search(c: &Connection, q: &Query) -> anyhow::Result<Vec<Hit>> {
    anyhow::ensure!(q.q.len() <= 4096, "log search query exceeds 4096 bytes");
    let mut ranked = Vec::new();
    for gram in trigrams(&q.q).into_iter().take(64) {
        let count: Option<i64> = c.query_row("SELECT documents FROM ci_log_terms WHERE gram=?1", [gram], |r| r.get(0)).optional()?;
        let Some(count) = count else { return Ok(Vec::new()) };
        ranked.push((count, gram));
    }
    ranked.sort();
    let Some((_, anchor)) = ranked.first() else { return Ok(Vec::new()) };
    let other = |n| ranked.get(n).map(|(_, g)| *g);
    const BLOCKS: &str = "SELECT p.finished,p.leg_order,p.leg,p.block,p.bits,
        (SELECT bits FROM ci_log_grams g WHERE g.gram=?2 AND g.finished=p.finished AND g.leg_order=p.leg_order AND g.leg=p.leg AND g.block=p.block),
        (SELECT bits FROM ci_log_grams g WHERE g.gram=?3 AND g.finished=p.finished AND g.leg_order=p.leg_order AND g.leg=p.leg AND g.block=p.block),
        (SELECT bits FROM ci_log_grams g WHERE g.gram=?4 AND g.finished=p.finished AND g.leg_order=p.leg_order AND g.leg=p.leg AND g.block=p.block)
        FROM ci_log_grams p WHERE p.gram=?1";
    let initial = format!("{BLOCKS} ORDER BY p.finished DESC,p.leg_order DESC,p.leg DESC,p.block DESC LIMIT 8");
    let continued = format!("{BLOCKS} AND (p.finished,p.leg_order,p.leg,p.block)<(?5,?6,?7,?8) ORDER BY p.finished DESC,p.leg_order DESC,p.leg DESC,p.block DESC LIMIT 8");
    let mut first = c.prepare_cached(&initial)?;
    let mut next = c.prepare_cached(&continued)?;
    let mut document = c.prepare_cached(
        "SELECT s.leg,s.step,s.line,s.text,j.run_id,j.key,r.number,r.repo_id,COALESCE(r.workflow_id,r.pipeline,''),r.branch,r.conclusion,r.created_at,
        (SELECT name FROM steps WHERE job_id=j.id AND idx=s.step)
        FROM ci_log_doc_order o JOIN ci_log_docs s ON s.id=o.doc AND s.index_version=o.version JOIN ci_log_search_ready ready ON ready.doc=s.id AND ready.version=s.index_version
        JOIN jobs j ON j.id=s.leg JOIN runs r ON r.id=j.run_id
        WHERE o.ordinal=?1 AND o.leg=?2
          AND r.provider='kiln' AND j.status IN ('succeeded','failed','cancelled','skipped','timed_out')
          AND j.attempt=s.source_attempt AND j.finished_at=s.order_finished AND j.rowid=s.order_leg
          AND s.order_finished=?9 AND s.order_leg=?10
          AND NOT EXISTS(SELECT 1 FROM ci_log_index_progress x WHERE x.leg=s.leg AND x.pruning=1)
          AND (?3 IS NULL OR r.repo_id=?3) AND (?4 IS NULL OR r.workflow_id=?4) AND (?5 IS NULL OR r.branch=?5)
          AND (?6 IS NULL OR r.conclusion=?6) AND (?7 IS NULL OR r.created_at>=?7) AND (?8 IS NULL OR r.created_at<?8)",
    )?;
    type Block = (i64, i64, String, i64, Vec<u8>, Option<Vec<u8>>, Option<Vec<u8>>, Option<Vec<u8>>);
    let map = |r: &crate::sql::Row<'_>| -> crate::sql::Result<Block> { Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?)) };
    let mut cursor: Option<(i64, i64, String, i64)> = None;
    let (mut blocks, mut examined, mut bytes) = (0usize, 0usize, 0usize);
    let mut out = Vec::new();
    let limit = q.limit.clamp(1, 1000);
    loop {
        let page: Vec<Block> = match &cursor {
            None => first.query_map(params![anchor, other(1), other(2), other(3)], map)?.collect::<crate::sql::Result<_>>()?,
            Some((finished, order, leg, block)) => next
                .query_map(params![anchor, other(1), other(2), other(3), finished, order, leg, block], map)?
                .collect::<crate::sql::Result<_>>()?,
        };
        let n = page.len();
        for (finished, order, leg, block, raw, b, c, d) in page {
            cursor = Some((finished, order, leg.clone(), block));
            blocks += 1;
            anyhow::ensure!(blocks <= 10_000, "log search candidate budget exhausted; narrow the query or filters");
            let mut bits = index::bitmap(&raw)?;
            for (gram, raw) in [other(1), other(2), other(3)].into_iter().zip([b, c, d]) {
                if gram.is_some() {
                    bits &= raw.as_deref().map(index::bitmap).transpose()?.unwrap_or(0)
                }
            }
            for bit in (0..64u32).rev() {
                if bits & (1u64 << bit) == 0 {
                    continue;
                }
                examined += 1;
                anyhow::ensure!(examined <= 10_000, "log search candidate budget exhausted; narrow the query or filters");
                let id = index::doc_id(block, bit)?;
                let row = document
                    .query_row(params![id, leg, q.repo, q.workflow, q.branch, q.conclusion, q.since, q.until, finished, order], |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, u32>(1)?,
                            usize::try_from(r.get::<_, i64>(2)?).unwrap_or(1),
                            r.get::<_, String>(3)?,
                            r.get::<_, String>(4)?,
                            r.get::<_, String>(5)?,
                            r.get::<_, i64>(6)?,
                            r.get::<_, String>(7)?,
                            r.get::<_, String>(8)?,
                            r.get::<_, String>(9)?,
                            r.get::<_, Option<String>>(10)?,
                            r.get::<_, i64>(11)?,
                            r.get::<_, Option<String>>(12)?,
                        ))
                    })
                    .optional()?;
                let Some((leg, step, line, body, run, job, number, repo, workflow, branch, conclusion, created_at, step_name)) = row else {
                    continue;
                };
                bytes = bytes.saturating_add(body.len());
                anyhow::ensure!(bytes <= 64 << 20, "log search candidate budget exhausted; narrow the query or filters");
                for (line, text, at) in lines_with(&body, line, &q.q).into_iter().take(5) {
                    out.push(Hit {
                        run: run.clone(),
                        number,
                        repo: repo.clone(),
                        workflow: workflow.clone(),
                        branch: branch.clone(),
                        conclusion: conclusion.clone(),
                        created_at,
                        leg: leg.clone(),
                        job: job.clone(),
                        step,
                        step_name: step_name.clone(),
                        line,
                        text,
                        at,
                    });
                    if out.len() >= limit {
                        return Ok(out);
                    }
                }
            }
        }
        if n < 8 {
            return Ok(out);
        }
    }
}

/// The indexer's wake-up: a leg finished.
static KICK: LazyLock<(async_channel::Sender<()>, async_channel::Receiver<()>)> = LazyLock::new(|| async_channel::bounded(1));

/// U05: terminal source intent belongs to the same SQL transaction as the job mutation.
pub(crate) fn stage_finished(c: &Connection, leg: &str) -> anyhow::Result<()> {
    index::enqueue(c, leg, false)
}
/// U05: deletion and its derived-index cleanup intent commit together.
pub(crate) fn stage_pruned(c: &Connection, leg: &str) -> anyhow::Result<()> {
    index::request_prune(c, leg)
}

/// Indexes legs as they finish, for as long as the server runs.
pub fn spawn(hub: Arc<Hub>) {
    crate::rt::spawn(async move {
        let mut scan = Scan::default();
        loop {
            match pass(&hub, &mut scan).await {
                Ok(true) => continue,
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %format!("{e:#}"), "logs could not be indexed"),
            }
            futures::select_biased! {
                _ = KICK.1.recv().fuse() => {}
                () = crate::rt::sleep(Duration::from_secs(30)).fuse() => {}
            }
        }
    });
}

/// Wakes the indexer: a leg finished.
pub fn kick() {
    let _ = KICK.0.try_send(());
}

/// Only transient preparation belongs here: all authoritative progress/publication is SQL.
/// At most four normalized step prefixes and four prepared gram vectors remain pinned.
/// One posting quantum per admitted document per turn; queued documents keep canonical order.
#[derive(Default)]
struct Scan {
    leg: Option<String>,
    doc: Option<i64>,
    prune: Option<String>,
    background_leg: Option<String>,
    texts: std::collections::VecDeque<(String, String, u32, Arc<StepText>, u64)>,
    grams: std::collections::VecDeque<(index::Document, Arc<Vec<i64>>)>,
    legacy_done: bool,
    serviced: std::collections::BTreeSet<i64>,
}

/// Segment boundaries are prepared once with the normalized prefix. Later segments do not
/// rescan all preceding lines (including an 8 MiB many-line step).
struct StepText {
    text: String,
    starts: Vec<usize>,
}
impl StepText {
    fn new(text: String) -> Self {
        let mut starts = vec![0];
        let mut lines = 0usize;
        for (at, b) in text.bytes().enumerate() {
            if b == b'\n' {
                lines += 1;
                if lines.is_multiple_of(SEGMENT_LINES) {
                    starts.push(at + 1)
                }
            }
        }
        Self { text, starts }
    }
    fn segment(&self, n: usize) -> anyhow::Result<(String, bool)> {
        let from = *self.starts.get(n).ok_or_else(|| anyhow::anyhow!("log-index segment cursor exceeds prepared prefix"))?;
        let next = n.checked_add(1).and_then(|n| self.starts.get(n)).copied();
        let end = next.map(|at| at.saturating_sub(1)).unwrap_or(self.text.len());
        Ok((
            self.text.get(from..end).ok_or_else(|| anyhow::anyhow!("invalid log-index segment boundary"))?.to_owned(),
            next.is_none(),
        ))
    }
}

async fn yield_indexer() {
    let mut yielded = false;
    futures::future::poll_fn(|cx| {
        if std::mem::replace(&mut yielded, true) {
            return std::task::Poll::Ready(());
        }
        cx.waker().wake_by_ref();
        std::task::Poll::Pending
    })
    .await;
}

async fn post_document(hub: &Hub, scan: &mut Scan, id: i64) -> anyhow::Result<bool> {
    if scan.serviced.contains(&id) {
        return Ok(false);
    }
    let cached = scan.grams.iter().find(|(d, _)| d.id == id).cloned();
    let mut owner = cached.as_ref().map(|(d, _)| d.leg.clone());
    if cached.is_none() && scan.grams.len() >= 4 {
        return Ok(false);
    }
    scan.serviced.insert(id);
    let result = async {
        let (doc, grams) = if let Some((d, grams)) = cached {
            let check = d.clone();
            let next = hub.store.call_local(move |c| index::refresh(c, &check)).await?;
            let Some(next) = next else {
                scan.grams.retain(|(d, _)| d.id != id);
                return Ok(false);
            };
            (next, grams)
        } else {
            owner = hub.store.call_local(move |c| Ok(c.query_row("SELECT leg FROM ci_log_docs WHERE id=?1", [id], |r| r.get::<_, String>(0)).optional()?)).await?;
            let Some((doc, body)) = hub.store.call_local(move |c| index::load(c, id)).await? else {
                return Ok(false);
            };
            owner = Some(doc.leg.clone());
            let grams = Arc::new(crate::rt::blocking(move || trigrams(&body).into_iter().collect::<Vec<_>>()).await?);
            scan.grams.push_back((doc.clone(), grams.clone()));
            (doc, grams)
        };
        let from = grams.partition_point(|g| doc.after.is_some_and(|a| *g <= a));
        let to = from.saturating_add(index::GRAMS_PER_BATCH).min(grams.len());
        let done = to == grams.len();
        let changed = hub.store.call_local(move |c| index::post(c, &doc, grams.get(from..to).unwrap_or_default(), done)).await?;
        if done {
            scan.grams.retain(|(d, _)| d.id != id)
        }
        Ok(changed)
    }
    .await;
    if result.is_err() {
        scan.grams.retain(|(d, _)| d.id != id);
        if let Some(leg) = owner { scan.texts.retain(|(l, _, _, _, _)| l != &leg); }
    }
    result
}

async fn visit_leg(hub: &Hub, scan: &mut Scan, leg: String) -> anyhow::Result<bool> {
    let key = leg.clone();
    let result = visit_leg_work(hub, scan, leg).await;
    if result.is_err() {
        scan.texts.retain(|(l, _, _, _, _)| l != &key);
        scan.grams.retain(|(d, _)| d.leg != key);
    }
    result
}

async fn visit_leg_work(hub: &Hub, scan: &mut Scan, leg: String) -> anyhow::Result<bool> {
    // A live viewer/late Bulk chunk is not a closed-stream authority. Canonical terminal
    // source eligibility is checked in SQL; this index publishes only its captured prefix.
    let key = leg.clone();
    let work = hub.store.call_local(move |c| index::leg(c, &leg)).await?;
    match work {
        index::Work::Complete => {
            scan.texts.retain(|(l, _, _, _, _)| l != &key);
            Ok(true)
        }
        index::Work::Idle => {
            scan.texts.retain(|(l, _, _, _, _)| l != &key);
            Ok(false)
        }
        index::Work::Post(id) => post_document(hub, scan, id).await,
        index::Work::Read(plan) => {
            let failed_key = (plan.leg.clone(), plan.version.clone(), plan.actual_step);
            let result = async {
                scan.texts.retain(|(l, v, step, _, _)| l != &plan.leg || (v == &plan.version && *step == plan.actual_step));
                let cached = scan
                    .texts
                    .iter()
                    .find(|(leg, v, step, _, _)| leg == &plan.leg && v == &plan.version && *step == plan.actual_step)
                    .cloned();
                let (text, bytes) = if let Some((_, _, _, text, bytes)) = cached {
                    (text, bytes)
                } else {
                    let transient = scan.texts.len() >= 4;
                    let Some((text, bytes)) = hub.logs.index_text(&plan.leg, plan.actual_step, MAX_INDEXED, if transient { 64 << 10 } else { u64::MAX }).await? else {
                        return Ok(false);
                    };
                    let text = Arc::new(crate::rt::blocking(move || StepText::new(text)).await?);
                    if !transient {
                        scan.texts.push_back((plan.leg.clone(), plan.version.clone(), plan.actual_step, text.clone(), bytes));
                    }
                    (text, bytes)
                };
                let (body, last) = text.segment(plan.segment)?;
                let evict = if last { Some((plan.leg.clone(), plan.version.clone(), plan.actual_step)) } else { None };
                let changed = hub.store.call_local(move |c| index::stage(c, &plan, &body, last, bytes)).await?;
                if let Some((leg, v, step)) = evict {
                    scan.texts.retain(|(l, version, s, _, _)| l != &leg || version != &v || *s != step)
                }
                Ok(changed)
            }
            .await;
            if result.is_err() {
                let (leg, version, step) = failed_key;
                scan.texts.retain(|(l, v, s, _, _)| l != &leg || v != &version || *s != step)
            }
            result
        }
    }
}

/// Round-robin each work class and each item within it. Errors never publish completion and
/// never prevent a later leg/document/prune turn. No transaction contains a whole leg/rebuild.
async fn pass(hub: &Hub, scan: &mut Scan) -> anyhow::Result<bool> {
    let mut changed = false;
    let mut failed = false;
    scan.serviced.clear();
    // Finish the existing prepared package without re-reading/re-sorting its body when
    // a pending-queue page happens not to contain it.
    let prepared: Vec<_> = scan.grams.iter().map(|(d, _)| d.id).collect();
    for id in prepared {
        match post_document(hub, scan, id).await {
            Ok(done) => changed |= done,
            Err(e) => {
                failed = true;
                tracing::warn!(doc=id,error=%format!("{e:#}"),"prepared log document remains pending")
            }
        }
        yield_indexer().await;
    }
    if !scan.legacy_done {
        match hub.store.call_local(|c| index::retire_legacy(c)).await {
            Ok(more) => {
                scan.legacy_done = !more;
                changed |= more
            }
            Err(e) => {
                failed = true;
                tracing::warn!(error=%format!("{e:#}"),"legacy log index retirement remains pending")
            }
        }
        yield_indexer().await;
    }
    let after = scan.prune.clone();
    let prunes: Vec<(String, String)> = hub
        .store
        .call_local(move |c| {
            let sql = if after.is_some() {
                "SELECT leg,version FROM ci_log_index_progress WHERE pruning=1 AND leg>?1 ORDER BY leg LIMIT 8"
            } else {
                "SELECT leg,version FROM ci_log_index_progress WHERE pruning=1 ORDER BY leg LIMIT 8"
            };
            let mut st = c.prepare(sql)?;
            let map = |r: &crate::sql::Row<'_>| Ok((r.get(0)?, r.get(1)?));
            let mut rows: Vec<(String, String)> = if let Some(after) = &after {
                st.query_map([after], map)?.collect::<crate::sql::Result<_>>()?
            } else {
                st.query_map([], map)?.collect::<crate::sql::Result<_>>()?
            };
            if rows.is_empty() && after.is_some() {
                rows = c
                    .prepare("SELECT leg,version FROM ci_log_index_progress WHERE pruning=1 ORDER BY leg LIMIT 8")?
                    .query_map([], map)?
                    .collect::<crate::sql::Result<_>>()?;
            }
            Ok(rows)
        })
        .await?;
    if prunes.is_empty() {
        scan.prune = None
    }
    for (leg, v) in prunes {
        scan.texts.retain(|(l, _, _, _, _)| l != &leg);
        scan.grams.retain(|(d, _)| d.leg != leg);
        scan.prune = Some(leg.clone());
        match hub.store.call_local(move |c| index::prune(c, &leg, &v)).await {
            Ok(done) => changed |= done,
            Err(e) => {
                failed = true;
                tracing::warn!(error=%format!("{e:#}"),"log index pruning remains pending")
            }
        }
        yield_indexer().await;
    }
    let after = scan.doc;
    let docs: Vec<i64> = hub
        .store
        .call_local(move |c| {
            let mut st = c.prepare(if after.is_some() {
                "SELECT doc FROM ci_log_search_pending WHERE doc>?1 ORDER BY doc LIMIT 8"
            } else {
                "SELECT doc FROM ci_log_search_pending ORDER BY doc LIMIT 8"
            })?;
            let mut rows: Vec<i64> = if let Some(after) = after {
                st.query_map([after], |r| r.get(0))?.collect::<crate::sql::Result<_>>()?
            } else {
                st.query_map([], |r| r.get(0))?.collect::<crate::sql::Result<_>>()?
            };
            if rows.is_empty() && after.is_some() {
                rows = c
                    .prepare("SELECT doc FROM ci_log_search_pending ORDER BY doc LIMIT 8")?
                    .query_map([], |r| r.get(0))?
                    .collect::<crate::sql::Result<_>>()?;
            }
            Ok(rows)
        })
        .await?;
    if docs.is_empty() {
        scan.doc = None
    }
    for id in docs {
        scan.doc = Some(id);
        match post_document(hub, scan, id).await {
            Ok(done) => changed |= done,
            Err(e) => {
                failed = true;
                tracing::warn!(doc=id,error=%format!("{e:#}"),"log document remains unindexed")
            }
        }
        yield_indexer().await;
    }
    match hub.store.call_local(|c| index::backfill(c)).await {
        Ok(more) => changed |= more,
        Err(e) => {
            failed = true;
            tracing::warn!(error=%format!("{e:#}"),"historical log discovery remains pending")
        }
    }
    yield_indexer().await;
    // Fresh terminal intents have their own bounded turn, independent of historical depth.
    for background in [false, true] {
        let after = if background { scan.background_leg.clone() } else { scan.leg.clone() };
        let legs: Vec<String> = hub
            .store
            .call_local(move |c| {
                let initial = "SELECT leg FROM ci_log_index_progress WHERE pruning=0 AND background=?1 ORDER BY leg LIMIT 8";
                let continued = "SELECT leg FROM ci_log_index_progress WHERE pruning=0 AND background=?1 AND leg>?2 ORDER BY leg LIMIT 8";
                let mut st = c.prepare(if after.is_some() { continued } else { initial })?;
                let mut rows: Vec<String> = if let Some(after) = &after {
                    st.query_map(params![background, after], |r| r.get(0))?.collect::<crate::sql::Result<_>>()?
                } else {
                    st.query_map([background], |r| r.get(0))?.collect::<crate::sql::Result<_>>()?
                };
                if rows.is_empty() && after.is_some() {
                    rows = c.prepare(initial)?.query_map([background], |r| r.get(0))?.collect::<crate::sql::Result<_>>()?;
                }
                Ok(rows)
            })
            .await?;
        if legs.is_empty() {
            if background { scan.background_leg = None } else { scan.leg = None }
        }
        for leg in legs {
            if background {
                scan.background_leg = Some(leg.clone())
            } else {
                scan.leg = Some(leg.clone())
            }
            match visit_leg(hub, scan, leg.clone()).await {
                Ok(done) => changed |= done,
                Err(e) => {
                    failed = true;
                    tracing::warn!(leg,error=%format!("{e:#}"),"leg logs remain unindexed")
                }
            }
            yield_indexer().await;
        }
    }
    // Preserve the existing retry cadence after an error, after giving every work class
    // its turn. A failed atom remains backlog and cannot spin on the writer.
    Ok(changed && !failed)
}

/// Retention installs only an idempotent prune intent for jobs already deleted. The same
/// indexer owns physical index cleanup, in finite reverse pages alongside normal indexing.
pub async fn forget(hub: &Hub, legs: Vec<String>) {
    for chunk in legs.chunks(128) {
        let chunk = chunk.to_vec();
        if let Err(e) = hub
            .store
            .call_local(move |c| {
                for leg in chunk {
                    index::request_prune(c, &leg)?
                }
                Ok(())
            })
            .await
        {
            tracing::warn!(error=%format!("{e:#}"),"log index prune intent has unknown outcome");
        }
        yield_indexer().await;
    }
    kick();
}

#[cfg(test)]
fn rebuild(c: &Connection) -> anyhow::Result<usize> {
    let ids: Vec<i64> = c
        .prepare("SELECT doc FROM ci_log_search_pending ORDER BY doc")?
        .query_map([], |r| r.get(0))?
        .collect::<crate::sql::Result<_>>()?;
    for id in &ids {
        loop {
            let Some((d, body)) = index::load(c, *id)? else { break };
            let all = trigrams(&body);
            let page: Vec<_> = all.into_iter().filter(|g| d.after.is_none_or(|a| *g > a)).take(index::GRAMS_PER_BATCH + 1).collect();
            index::post(c, &d, page.get(..page.len().min(index::GRAMS_PER_BATCH)).unwrap_or_default(), page.len() <= index::GRAMS_PER_BATCH)?;
        }
    }
    Ok(ids.len())
}

#[cfg(test)]
fn forget_rows(c: &Connection, legs: &[String]) -> anyhow::Result<()> {
    for leg in legs {
        let v = crate::util::new_id();
        c.execute(
            "INSERT INTO ci_log_index_progress (leg,version,pruning) VALUES (?1,?2,1) ON CONFLICT(leg) DO UPDATE SET pruning=1",
            params![leg, v],
        )?;
        let v: String = c.query_row("SELECT version FROM ci_log_index_progress WHERE leg=?1", [leg], |r| r.get(0))?;
        while c.query_row("SELECT EXISTS(SELECT 1 FROM ci_log_index_progress WHERE leg=?1)", [leg], |r| r.get::<_, bool>(0))? {
            index::prune(c, leg, &v)?;
        }
    }
    Ok(())
}

async fn repo_id(hub: &Hub, repo: &str) -> Result<String, Fail> {
    let rs = hub.store.repos().await.map_err(internal)?;
    rs.into_iter().find(|r| r.id == repo || r.name == repo).map(|r| r.id).ok_or_else(|| (StatusCode::NOT_FOUND, format!("no repository `{repo}`")))
}

/// `GET /api/ci/logs/search`.
pub async fn route(hub: &Hub, q: &HashMap<String, String>) -> Result<Response, Fail> {
    let text = q.get("q").map(|s| s.trim().to_owned()).unwrap_or_default();
    if text.chars().count() < 3 {
        return Err((StatusCode::BAD_REQUEST, "search for three characters or more".into()));
    }
    let some = |k: &str| q.get(k).map(|v| v.trim().to_owned()).filter(|v| !v.is_empty());
    let repo = match some("repo") {
        Some(r) => Some(repo_id(hub, &r).await?),
        None => None,
    };
    let time = |k: &str| some(k).and_then(|v| v.parse::<i64>().ok());
    let since = time("since").or_else(|| some("days").and_then(|d| d.parse::<i64>().ok()).map(|d| crate::util::now_unix() - d.clamp(1, 3650) * 86_400));
    let query = Query { q: text, repo, workflow: some("workflow"), branch: some("branch"), conclusion: some("conclusion"), since, until: time("until"), limit: some("limit").and_then(|l| l.parse().ok()).unwrap_or(100) };
    let started = std::time::Instant::now();
    let hits = hub.store.call(move |c| search(c, &query)).await.map_err(internal)?;
    let names: HashMap<String, String> = hub.store.repos().await.map_err(internal)?.into_iter().map(|r| (r.id, r.name)).collect();
    let behind: i64 = hub.store.call(|c| Ok(c.query_row("SELECT COUNT(*) FROM jobs j JOIN runs r ON r.id=j.run_id WHERE r.provider='kiln' AND j.finished_at IS NOT NULL AND (NOT EXISTS (SELECT 1 FROM ci_log_indexed x WHERE x.leg=j.id AND x.source_attempt=j.attempt AND x.source_finished=j.finished_at AND x.source_order=j.rowid) OR EXISTS (SELECT 1 FROM ci_log_docs d WHERE d.leg = j.id AND NOT EXISTS (SELECT 1 FROM ci_log_search_ready z WHERE z.doc = d.id AND z.version=d.index_version)))", [], |x| x.get(0))?)).await.map_err(internal)?;
    let hits: Vec<Value> = hits
        .into_iter()
        .map(|h| {
            let mut v = serde_json::to_value(&h).unwrap_or_default();
            if let Some(o) = v.as_object_mut() {
                o.insert("repo_name".into(), json!(names.get(&h.repo)));
            }
            v
        })
        .collect();
    Ok(srv::json(StatusCode::OK, &json!({ "hits": hits, "took_ms": started.elapsed().as_millis(), "unindexed_legs": behind })))
}

/// A leg's steps: (idx, name, status, exit code), in order.
async fn steps_of(hub: &Hub, leg: &str) -> Result<Vec<(u32, String, String, Option<i64>)>, Fail> {
    let leg = leg.to_owned();
    hub.store
        .call(move |c| {
            let mut st = c.prepare("SELECT idx, name, COALESCE(conclusion, status), exit_code FROM steps WHERE job_id = ?1 ORDER BY COALESCE(started_seq, 1000000000 + idx)")?;
            let rows = st.query_map([&leg], |x| Ok((x.get(0)?, x.get(1)?, x.get(2)?, x.get(3)?)))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
        .map_err(internal)
}

/// `GET /api/ci/legs/:id/logs/search?q=` and `/logs/failure`.
pub async fn leg(hub: &Hub, id: &str, what: &str, q: &HashMap<String, String>) -> Result<Response, Fail> {
    let steps = steps_of(hub, id).await?;
    if steps.is_empty() && hub.store.job(id.to_owned()).await.map_err(internal)?.is_none() {
        return Err((StatusCode::NOT_FOUND, format!("no leg `{id}`")));
    }
    match what {
        "search" => {
            let text = q.get("q").map(|s| s.trim().to_owned()).unwrap_or_default();
            if text.is_empty() {
                return Err((StatusCode::BAD_REQUEST, "say what to search for: ?q=".into()));
            }
            let mut hits = Vec::new();
            for (idx, name, ..) in &steps {
                let body = plain(&hub.logs.read(id, *idx).await);
                for (line, t, at) in lines_with(&body, 1, &text).into_iter().take(200) {
                    hits.push(json!({ "step": idx, "step_name": name, "line": line, "text": t, "at": at }));
                }
            }
            Ok(srv::json(StatusCode::OK, &json!({ "leg": id, "hits": hits })))
        }
        _ => {
            let lines: usize = q.get("lines").and_then(|l| l.parse().ok()).unwrap_or(40).clamp(5, 400);
            Ok(srv::json(StatusCode::OK, &failure_of(hub, id, &steps, lines).await))
        }
    }
}

/// What to read first when leg `id` failed (`lines` of it): `{ leg, step, step_name, status,
/// exit_code, total_lines, lines: [{ line, text }] }`. For whoever follows a failure (an agent
/// whose change it is, `crate::workspace::follow`).
pub async fn failure(hub: &Hub, id: &str, lines: usize) -> Result<Value, Fail> {
    let steps = steps_of(hub, id).await?;
    Ok(failure_of(hub, id, &steps, lines.clamp(5, 400)).await)
}

async fn failure_of(hub: &Hub, id: &str, steps: &[(u32, String, String, Option<i64>)], lines: usize) -> Value {
    // The step that failed (else the last that ran), around its first error line, else its end.
    let failed = steps.iter().find(|s| matches!(s.2.as_str(), "failure" | "failed" | "timed_out")).or_else(|| steps.iter().rev().find(|s| s.2 != "skipped" && s.2 != "pending"));
    let Some((idx, name, status, exit_code)) = failed.cloned() else { return json!({ "leg": id, "step": null, "lines": [] }) };
    let body = plain(&hub.logs.read(id, idx).await);
    let body = body.trim_end();
    let all: Vec<&str> = if body.is_empty() { Vec::new() } else { body.split('\n').collect() };
    let (from, to) = excerpt(&all, lines);
    let shown: Vec<Value> = all.get(from..to).unwrap_or_default().iter().enumerate().map(|(i, l)| json!({ "line": from + i + 1, "text": crate::util::truncate(l, 2000) })).collect();
    json!({ "leg": id, "step": idx, "step_name": name, "status": status, "exit_code": exit_code, "total_lines": all.len(), "lines": shown })
}

/// Words that mark a line as an error's.
const ERROR_MARKS: &[&str] = &["##[error]", "error:", "error[", "fatal:", "panicked at", "exception", "traceback (most recent call last)", "failed", "fail:", "--- fail", "assertionerror", "npm err!"];

/// How a line that surely is an error's starts (a compiler's, a test's, a tool's), its indent
/// aside; and what it has wherever.
const ERROR_STARTS: &[&str] = &["##[error]", "error:", "error[", "fatal:", "fail:", "--- fail", "fail\t", "npm err!", "traceback (most recent call last)"];
const ERROR_ANYWHERE: &[&str] = &["##[error]", "panicked at", "assertionerror"];

/// A line (lower case, its indent aside) that says it isn't an error's though it has one of their
/// words: a log record at a lower level (`level=info msg="fetch failed …"`, a JSON record's
/// `"level":"debug"`, `[INFO]`, `warning:`), or a test runner's progress (Go's `=== RUN`,
/// `--- PASS`, `ok`).
fn lesser(l: &str) -> bool {
    const BELOW: &[&str] = &["info", "debug", "trace", "warning", "warn", "notice"];
    let level = ["level=", "\"level\":", "lvl="]
        .iter()
        .any(|k| l.match_indices(k).any(|(at, _)| l.get(at + k.len()..).is_some_and(|rest| BELOW.iter().any(|b| rest.trim_start_matches([' ', '"']).starts_with(b)))));
    let tag = l.strip_prefix('[').unwrap_or(l);
    let tagged = BELOW.iter().any(|b| tag.strip_prefix(b).is_some_and(|after| after.starts_with([':', ' ', ']'])));
    let progress = ["=== run", "=== pause", "=== cont", "=== name", "--- pass", "--- skip", "ok ", "ok\t"].iter().any(|p| l.starts_with(p));
    level || tagged || progress
}

/// The excerpt of `lines` (`n` of them) to show for a failure: from a few lines before the first
/// line that surely reads like an error, else the first that may, else the last `n`. A line that
/// says it's a lesser one's (`lesser`) is neither.
pub fn excerpt(lines: &[&str], n: usize) -> (usize, usize) {
    let lower: Vec<String> = lines.iter().map(|l| l.trim_start().to_lowercase()).collect();
    let sure = lower.iter().position(|l| !lesser(l) && (ERROR_STARTS.iter().any(|m| l.starts_with(m)) || ERROR_ANYWHERE.iter().any(|m| l.contains(m))));
    let first = sure.or_else(|| lower.iter().position(|l| !lesser(l) && ERROR_MARKS.iter().any(|m| l.contains(m))));
    match first {
        // A few lines before it (fewer in a short excerpt, so it's in it), and no fewer than `n`
        // when it's near the end.
        Some(i) => {
            let from = i.saturating_sub(5.min(n.saturating_sub(1) / 2)).min(lines.len().saturating_sub(n));
            (from, (from + n).min(lines.len()))
        }
        None => (lines.len().saturating_sub(n), lines.len()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A database with the index and just enough of runs, jobs and steps.
    fn db(c: &mut Connection<'_>) -> anyhow::Result<()> {
        c.execute_batch(
            "CREATE TABLE runs (id TEXT PRIMARY KEY, number INTEGER, repo_id TEXT, workflow_id TEXT, pipeline TEXT, branch TEXT, conclusion TEXT, status TEXT, created_at INTEGER, provider TEXT NOT NULL DEFAULT 'kiln');
             CREATE TABLE jobs (id TEXT PRIMARY KEY, run_id TEXT, key TEXT, finished_at INTEGER, attempt INTEGER NOT NULL DEFAULT 1, status TEXT NOT NULL DEFAULT 'succeeded');
             CREATE TABLE steps (job_id TEXT, idx INTEGER, name TEXT);
             CREATE TABLE ci_log_indexed (leg TEXT PRIMARY KEY, at INTEGER, bytes INTEGER, first_row INTEGER, last_row INTEGER);",
        )?;
        c.execute_batch(include_str!("../store/migrations/20261029-1110_u05_log_search.sql"))?;
        c.execute_batch(include_str!("../store/migrations/20261030-0200_u05_log_index_progress.sql"))?;
        Ok(())
    }

    #[test]
    fn w19_r04_lines() -> anyhow::Result<()> {
        assert_eq!(plain(b"\x1b[31mred\x1b[0m\r\nprogress 10%\rprogress 100%\nlast"), "red\nprogress 100%\nlast");
        struct Fragments<'a>(&'a [u8],usize);
        impl std::io::Read for Fragments<'_> {
            fn read(&mut self,buf:&mut[u8])->std::io::Result<usize>{
                let n=buf.len().min(self.1);self.0.read(buf.get_mut(..n).unwrap_or_default())
            }
        }
        let raw=b"\x1b[31m\xce\xa3red\x1b[0m\r\nold\rrewritten\r\r\ninvalid\xf0\x80x\r\n\x1b[unfinished";
        for chunk in [1,2,7] {for limit in [0,1,2,3,17,64,4096] {
            assert_eq!(plain_reader(Fragments(raw,chunk),limit)?,crate::util::truncate(&plain(raw),limit),"streamed normalization/prefix: {chunk}/{limit}");
        }}
        let lines = ["ok", "compiling", "error[E0308]: mismatched types", "  --> src/a.rs:3", "done"];
        assert_eq!(excerpt(&lines, 3), (1, 4));
        assert_eq!(excerpt(&["a", "b", "c", "d"], 2), (2, 4), "no error line: the end");
        // A log record at a lower level, or a test's progress, isn't an error's, and a line that
        // starts as one comes before one that only has its words (buildx's integration tests).
        let go = [
            "time=\"2026-10-23T20:22:41Z\" level=info msg=\"fetch failed after status: 404 Not Found\"",
            "=== RUN   TestIntegration/TestBuildFailedNoParallel",
            "--- PASS: TestIntegration/TestBuildFailedNoParallel (2.22s)",
            "{\"level\":\"debug\",\"msg\":\"retry failed\"}",
            "[INFO] a step failed, retrying",
            "    dap_build.go:937:",
            "        \tError Trace:\t/src/tests/dap_build.go:937",
            "        \tError:      \ttimeout reached",
            "--- FAIL: TestIntegration (70.54s)",
            "FAIL",
        ];
        assert_eq!(excerpt(&go, 3), (6, 9), "{:?}", go.get(6..9));
        assert_eq!(excerpt(&["x"; 60].iter().copied().chain(["error: no such file"]).collect::<Vec<_>>(), 40), (21, 61), "near the end: the last 40");
        assert_eq!(excerpt(&["ok", "Build failed: 2 errors", "done"], 2), (1, 3), "only a mark: the first");
        assert_eq!(excerpt(&["warning: the build failed for an optional part", "information: failed", "x"], 1), (1, 2), "a warning is lesser; information isn't");
        crate::sql::with_memory(|c| {
        db(c)?;
        c.execute("INSERT INTO runs (id,number,repo_id,workflow_id,pipeline,branch,conclusion,status,created_at) VALUES ('r1', 7, 'repo', 'ci.yml', '', 'main', 'failure', 'failed', 100)", [])?;
        c.execute("INSERT INTO jobs (id,run_id,key,finished_at) VALUES ('l1', 'r1', 'test', 100)", [])?;
        c.execute("INSERT INTO steps VALUES ('l1', 1, 'Run tests')", [])?;
        let text: String = (1..=250).map(|i| if i == 175 { "thread 'main' panicked at Needle-42 here".to_owned() } else { format!("line {i}") }).collect::<Vec<_>>().join("\n");
        assert_eq!(index_step(c, "l1", 1, &text)?.map(|(a, b)| b - a), Some(2), "250 lines: three segments");
        let hits = search(c, &Query { q: "needle-42".into(), limit: 10, ..Query::default() })?;
        assert_eq!(hits.iter().map(|h| (h.line, h.step, h.step_name.as_deref(), h.number)).collect::<Vec<_>>(), vec![(175, 1, Some("Run tests"), 7)]);
        assert_eq!(hits.first().map(|h| h.at), Some(26));
        // Filters narrow it; quotes are literal.
        assert!(search(c, &Query { q: "needle-42".into(), branch: Some("dev".into()), limit: 10, ..Query::default() })?.is_empty());
        assert!(search(c, &Query { q: "a \"quoted\" thing".into(), limit: 10, ..Query::default() }).is_ok());
        // U05: one literal oracle, reusing this fixture, covers candidate false positives,
        // Unicode lowercase, punctuation, snippets, segment caps, ordering and result limits.
        let literal = "A \"quoted\" thing Σύνδεση needle-42";
        index_step(c, "l1", 1, &format!("{}\n{}", literal, "needle-42\n".repeat(9)))?;
        index_step(c, "l1", 1, "literal abcdef hit")?;
        for _ in 0..12 { index_step(c, "l1", 1, "abc bcd cde def -- scattered triples")?; }
        let long = format!("{} NEEDLE-42", "x".repeat(2100));
        index_step(c, "l1", 1, &long)?;
        for query in ["needle-42", "a \"quoted\" thing", "ΣΎΝΔΕΣΗ", "abcdef", "absent"] {
            for limit in [1, 3, 1001] {
                let docs: Vec<(usize, String)> = c.prepare("SELECT line, text FROM ci_log_docs ORDER BY id DESC")?
                    .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<crate::sql::Result<_>>()?;
                let expected: Vec<_> = docs.iter().flat_map(|(line, text)| lines_with(text, *line, query).into_iter().take(5)).take(limit.clamp(1, 1000)).collect();
                let actual = search(c, &Query { q: query.into(), limit, ..Query::default() })?;
                assert_eq!(actual.into_iter().map(|h| (h.line, h.text, h.at)).collect::<Vec<_>>(), expected, "U05 literal oracle: {query}");
            }
        }
        // A canonical imported document with its exact source keeps its id while postings rebuild.
        c.execute("INSERT INTO ci_log_docs (id,text,leg,step,line,index_version,order_doc,source_attempt,order_finished,order_leg) SELECT 900,'imported Needle-42','l1',1,701,?1,900,attempt,finished_at,rowid FROM jobs WHERE id='l1'",[crate::util::new_id()])?;
        c.execute("INSERT INTO ci_log_search_pending (doc) VALUES (900)", [])?;
        assert_eq!(rebuild(c)?, 1);
        assert_eq!(rebuild(c)?, 0);
        let imported = search(c, &Query { q: "needle-42".into(), limit: 1, ..Query::default() })?;
        assert_eq!(imported.first().map(|h| h.line), Some(701));
        forget_rows(c, &["l1".to_owned()])?;
        assert!(search(c, &Query { q: "needle-42".into(), limit: 10, ..Query::default() })?.is_empty());
        let postings: i64 = c.query_row("SELECT COUNT(*) FROM ci_log_grams", [], |r| r.get(0))?;
        let terms: i64 = c.query_row("SELECT COUNT(*) FROM ci_log_terms", [], |r| r.get(0))?;
        assert_eq!((postings, terms), (0, 0));
        Ok(())
        })?;
        crate::rt::block_on(bounded_indexing())
    }


    async fn bounded_indexing() -> anyhow::Result<()> {
        let mut hub = crate::hub::tests::scratch_hub().await?;
        let logs = crate::logs::Logs::new(hub.cfg.data("logs"), u64::try_from(MAX_INDEXED)? + 4096);
        Arc::get_mut(&mut hub).ok_or_else(|| anyhow::anyhow!("fresh scratch Hub is already shared"))?.logs = logs;
        hub.store.call(|c|{
                c.execute("INSERT INTO repos (id,name,clone_url,created_at) VALUES ('log-repo','log-repo','file:///logs',1)",[])?;
                c.execute("INSERT INTO runs (id,repo_id,number,pipeline,event,branch,sha,status,created_at,workflow_id,conclusion) VALUES ('log-run','log-repo',1,'ci','push','main','abc','succeeded',1,'ci.yml','success')",[])?;
                for (leg,finished) in [("older",100),("newer",200)] {
                    c.execute("INSERT INTO jobs (id,run_id,key,display,status,labels,needs,spec,finished_at,attempt) VALUES (?1,'log-run','build','build','succeeded','[]','[]','{}',?2,1)",params![leg,finished])?;
                    c.execute("INSERT INTO steps (job_id,idx,name,status) VALUES (?1,1,'first','succeeded')",[leg])?;
                }
                c.execute("INSERT INTO steps (job_id,idx,name,status) VALUES ('older',2,'second','succeeded')",[])?;
                // Unknown legacy content/marker must be pruned, never stamped with this attempt.
                c.execute("INSERT INTO ci_log_docs (text,leg,step,line) VALUES ('obsolete legacy prefix','older',1,1)",[])?;
                c.execute("INSERT INTO ci_log_indexed (leg,at,bytes) VALUES ('older',1,22)",[])?;
                stage_finished(c,"older")?;stage_finished(c,"newer")?;
                Ok(())
            }).await?;
        let distinct = |count| (0..count).filter_map(|n| char::from_u32(0x10000 + n)).collect::<String>();
        let mut body = format!("chronologyneedle {}", distinct(5000));
        let later = format!("chronologyneedle newer {}", distinct(500));
        assert!(
            later.len() > 1024 && trigrams(&later).len() > index::GRAMS_PER_BATCH * 3,
            "later ordinary log must need multiple posting pages"
        );
        assert!(
            trigrams(&body).len() > trigrams(&later).len() * 4,
            "older preparation must stay pending while ordinary later work progresses"
        );
        body.extend(std::iter::repeat_n('x', MAX_INDEXED - body.len()));
        assert_eq!(body.len(), MAX_INDEXED, "real largest permitted single-line document");
        hub.logs.append("older", 1, body.as_bytes());
        hub.logs.append("older", 2, b"chronologyneedle older last segment");
        hub.logs.finish("older");
        hub.logs.append("newer", 1, later.as_bytes());
        hub.logs.finish("newer");
        let mut scan = Scan::default();
        pass(&hub, &mut scan).await?;
        pass(&hub, &mut scan).await?;
        pass(&hub, &mut scan).await?;
        hub.store
            .call(|c| {
                for leg in ["older", "newer"] {
                    assert!(
                        c.query_row(
                            "SELECT EXISTS(SELECT 1 FROM ci_log_search_pending p JOIN ci_log_docs d ON d.id=p.doc WHERE d.leg=?1 AND p.after_gram IS NOT NULL)",
                            [leg],
                            |r| r.get::<_, bool>(0)
                        )?,
                        "ordinary documents both receive posting quanta"
                    );
                }
                Ok(())
            })
            .await?;
        for _ in 0..8 {
            pass(&hub, &mut scan).await?;
            if hub
                .store
                .call(|c| Ok(c.query_row("SELECT EXISTS(SELECT 1 FROM ci_log_indexed WHERE leg='newer')", [], |r| r.get::<_, bool>(0))?))
                .await?
            {
                break;
            }
        }
        hub.store
            .call(|c| {
                assert_eq!(
                    c.query_row("SELECT COUNT(*) FROM ci_log_indexed WHERE leg='newer'", [], |r| r.get::<_, i64>(0))?,
                    1,
                    "later multi-page leg completes while oldest is partial"
                );
                assert_eq!(c.query_row("SELECT COUNT(*) FROM ci_log_indexed WHERE leg='older'", [], |r| r.get::<_, i64>(0))?, 0);
                assert_eq!(c.query_row("SELECT 1", [], |r| r.get::<_, i64>(0))?, 1, "the same writer is available between quanta");
                Ok(())
            })
            .await?;
        let (saved, all) = hub
            .store
            .call(|c| {
                let id = c.query_row("SELECT id FROM ci_log_docs WHERE leg='older' ORDER BY id LIMIT 1", [], |r| r.get(0))?;
                let (doc, body) = index::load(c, id)?.ok_or_else(|| anyhow::anyhow!("old document missing"))?;
                Ok((doc, trigrams(&body).into_iter().collect::<Vec<_>>()))
            })
            .await?;
        let first = *all.first().ok_or_else(|| anyhow::anyhow!("fixture has no grams"))?;
        let chars = [first >> 42, (first >> 21) & 0x1f_ffff, first & 0x1f_ffff];
        let query = chars
            .into_iter()
            .map(|c| u32::try_from(c).ok().and_then(char::from_u32).ok_or_else(|| anyhow::anyhow!("bad fixture gram")))
            .collect::<anyhow::Result<String>>()?;
        let query2 = query.clone();
        hub.store
            .call(move |c| {
                assert!(
                    search(
                        c,
                        &Query {
                            q: query2,
                            limit: 100,
                            ..Query::default()
                        }
                    )?
                    .iter()
                    .all(|h| h.leg != "older"),
                    "even a posted literal gram stays invisible before ready"
                );
                Ok(())
            })
            .await?;
        let from = all.partition_point(|g| saved.after.is_some_and(|a| *g <= a));
        let page = all.get(from..from.saturating_add(index::GRAMS_PER_BATCH).min(all.len())).unwrap_or_default().to_vec();
        let d = saved.clone();
        let pg = page.clone();
        hub.store
            .call(move |c| {
                // Lose a local acknowledgment after a commit, then retry the exact captured page.
                assert!(index::post(c, &d, &pg, false)?);
                let count: i64 = c.query_row("SELECT SUM(documents) FROM ci_log_terms", [], |r| r.get(0))?;
                assert!(!index::post(c, &d, &pg, false)?);
                assert_eq!(
                    c.query_row("SELECT SUM(documents) FROM ci_log_terms", [], |r| r.get::<_, i64>(0))?,
                    count,
                    "replay cannot double-count terms"
                );
                Ok(())
            })
            .await?;
        let d = saved.clone();
        hub.store
            .call(move |c| {
                let (next, body) = index::load(c, d.id)?.ok_or_else(|| anyhow::anyhow!("pending document missing"))?;
                let page: Vec<_> = trigrams(&body).into_iter().filter(|g| next.after.is_none_or(|a| *g > a)).take(index::GRAMS_PER_BATCH).collect();
                {
                    let tx = crate::store::tx(c)?;
                    index::post(&tx, &next, &page, false)?;
                }
                assert_eq!(index::load(c, d.id)?.map(|(d, _)| d.after), Some(next.after), "rollback preserves the same restart cursor");
                Ok(())
            })
            .await?;
        for _ in 0..128 {
            pass(&hub, &mut scan).await?;
            if hub
                .store
                .call(|c| Ok(c.query_row("SELECT EXISTS(SELECT 1 FROM ci_log_indexed WHERE leg='older')", [], |r| r.get::<_, bool>(0))?))
                .await?
            {
                break;
            }
        }
        hub.store
            .call(|c| {
                assert_eq!(c.query_row("SELECT COUNT(*) FROM ci_log_indexed WHERE leg IN ('older','newer')", [], |r| r.get::<_, i64>(0))?, 2);
                let hits = search(
                    c,
                    &Query {
                        q: "chronologyneedle".into(),
                        limit: 100,
                        ..Query::default()
                    },
                )?;
                assert_eq!(
                    hits.iter().map(|h| (h.leg.as_str(), h.step, h.line)).collect::<Vec<_>>(),
                    vec![("newer", 1, 1), ("older", 2, 1), ("older", 1, 1)],
                    "finished order survives older segments inserted after a newer leg"
                );
                Ok(())
            })
            .await?;
        hub.store
            .call(|c| {
                // A retry hides the preceding received prefix immediately, before another finish.
                c.execute("UPDATE jobs SET attempt=attempt+1,status='queued' WHERE id='older'", [])?;
                let hits = search(
                    c,
                    &Query {
                        q: "chronologyneedle".into(),
                        limit: 100,
                        ..Query::default()
                    },
                )?;
                assert_eq!(hits.iter().map(|h| h.leg.as_str()).collect::<Vec<_>>(), vec!["newer"]);
                c.execute("UPDATE jobs SET status='succeeded' WHERE id='older'", [])?;
                stage_finished(c, "older")?;
                let version: String = c.query_row("SELECT version FROM ci_log_index_progress WHERE leg='older'", [], |r| r.get(0))?;
                stage_finished(c, "older")?;
                assert_eq!(
                    c.query_row("SELECT version FROM ci_log_index_progress WHERE leg='older'", [], |r| r.get::<_, String>(0))?,
                    version,
                    "duplicate exact finish preserves resumable progress"
                );
                Ok(())
            })
            .await?;
        // Recreate the transient pump state: the canonical retry/prune cursor survives it.
        scan = Scan::default();
        for _ in 0..256 {
            pass(&hub, &mut scan).await?;
            if hub
                .store
                .call(|c| Ok(c.query_row("SELECT EXISTS(SELECT 1 FROM ci_log_indexed WHERE leg='older' AND source_attempt=2)", [], |r| r.get::<_, bool>(0))?))
                .await?
            {
                break;
            }
        }
        hub.store
            .call(|c| {
                assert!(c.query_row("SELECT EXISTS(SELECT 1 FROM ci_log_indexed WHERE leg='older' AND source_attempt=2)", [], |r| r.get::<_, bool>(0))?);
                let hits = search(
                    c,
                    &Query {
                        q: "chronologyneedle".into(),
                        limit: 100,
                        ..Query::default()
                    },
                )?;
                assert_eq!(
                    hits.iter().map(|h| (h.leg.as_str(), h.step)).collect::<Vec<_>>(),
                    vec![("newer", 1), ("older", 2), ("older", 1)],
                    "attempt replacement neither mixes nor double-publishes old documents"
                );
                Ok(())
            })
            .await?;
        hub.store
            .call(|c| {
                c.execute("DELETE FROM jobs WHERE id='older'", [])?;
                index::request_prune(c, "older")?;
                Ok(())
            })
            .await?;
        let prune_version: String = hub
            .store
            .call(|c| Ok(c.query_row("SELECT version FROM ci_log_index_progress WHERE leg='older'", [], |r| r.get(0))?))
            .await?;
        for _ in 0..128 {
            pass(&hub, &mut scan).await?;
            if hub
                .store
                .call(|c| Ok(!c.query_row("SELECT EXISTS(SELECT 1 FROM ci_log_index_progress WHERE leg='older')", [], |r| r.get::<_, bool>(0))?))
                .await?
            {
                break;
            }
        }
        let old = saved.clone();
        hub.store.call(move|c|{
                assert_eq!(c.query_row("SELECT COUNT(*) FROM ci_log_docs WHERE leg='older'",[],|r|r.get::<_,i64>(0))?,0,"prune drains all reverse pages");
                for gram in trigrams("chronologyneedle") {
                    assert_eq!(c.query_row("SELECT documents FROM ci_log_terms WHERE gram=?1",[gram],|r|r.get::<_,i64>(0))?,1,"pruned docs leave exactly the newer document count");
                }
                let version=crate::util::new_id();
                c.execute("INSERT INTO ci_log_docs (id,text,leg,step,line,index_version,order_finished,order_leg,order_doc,source_attempt) SELECT ?1,'replacement','newer',1,400,?2,finished_at,rowid,999999,attempt FROM jobs WHERE id='newer'",params![old.id,version])?;
                c.execute("INSERT INTO ci_log_search_pending (doc) VALUES (?1)",[old.id])?;
                let count:i64=c.query_row("SELECT SUM(documents) FROM ci_log_terms",[],|r|r.get(0))?;
                assert!(!index::post(c,&old,&page,false)?,"stale prepared page refuses a reused integer document id");
                assert!(!index::prune(c,"older",&prune_version)?,"retired prune authority cannot touch replacement work");
                assert_eq!(c.query_row("SELECT SUM(documents) FROM ci_log_terms",[],|r|r.get::<_,i64>(0))?,count);
                assert_eq!(c.query_row("SELECT index_version FROM ci_log_docs WHERE id=?1",[old.id],|r|r.get::<_,String>(0))?,version);
                Ok(())
            }).await?;
        hub.store
            .call(|c| {
                c.execute(
                    "UPDATE ci_log_index_backfill SET after_finished=NULL,after_order=NULL,end_finished=200,end_order=9223372036854775807,done=0 WHERE id=1",
                    [],
                )?;
                // Historical finished rows must not delay a freshly committed, backdated leg.
                for n in 0..64 {
                    let id = format!("history-{n:03}");
                    c.execute(
                        "INSERT INTO jobs (id,run_id,key,display,status,labels,needs,spec,finished_at) VALUES (?1,'log-run','old','old','succeeded','[]','[]','{}',100)",
                        [&id],
                    )?;
                    c.execute("INSERT INTO ci_log_indexed (leg,at,bytes) VALUES (?1,1,0)", [&id])?;
                }
                assert!(index::backfill(c)?);
                Ok(())
            })
            .await?;
        // The scan restarts from its committed historical cursor, not from the prefix.
        let before: (i64, i64) = hub
            .store
            .call(|c| Ok(c.query_row("SELECT after_finished,after_order FROM ci_log_index_backfill WHERE id=1", [], |r| Ok((r.get(0)?, r.get(1)?)))?))
            .await?;
        hub.logs.append("backdated", 1, b"fresh backdated needle");
        hub.logs.finish("backdated");
        hub.store
            .call(|c| {
                c.execute(
                    "INSERT INTO jobs (id,run_id,key,display,status,labels,needs,spec,finished_at) VALUES ('backdated','log-run','fresh','fresh','succeeded','[]','[]','{}',50)",
                    [],
                )?;
                c.execute("INSERT INTO steps (job_id,idx,name,status) VALUES ('backdated',1,'first','succeeded')", [])?;
                stage_finished(c, "backdated")?;
                Ok(())
            })
            .await?;
        scan = Scan::default();
        pass(&hub, &mut scan).await?;
        pass(&hub, &mut scan).await?;
        hub.store
            .call(move |c| {
                assert!(
                    c.query_row("SELECT EXISTS(SELECT 1 FROM ci_log_indexed WHERE leg='backdated')", [], |r| r.get::<_, bool>(0))?,
                    "fresh backdated completion bypasses the historical cursor"
                );
                let after: (i64, i64) = c.query_row("SELECT after_finished,after_order FROM ci_log_index_backfill WHERE id=1", [], |r| Ok((r.get(0)?, r.get(1)?)))?;
                assert!(after > before, "historical reconstruction resumes after its committed cursor");
                assert_eq!(
                    search(
                        c,
                        &Query {
                            q: "fresh backdated needle".into(),
                            ..Query::default()
                        }
                    )?
                    .len(),
                    1
                );
                Ok(())
            })
            .await?;
        Ok(())
    }

    /// W19-AC06 (a bench; `cargo test -p kiln --release -- --ignored w19_ac06`): a string one step
    /// printed, among 10,000 runs' logs, is found in under 500 ms.
    #[test]
    #[ignore]
    fn w19_ac06_search_bench() -> anyhow::Result<()> {
        // The ignored scale benchmark seeds 60,000 logs; retain its explicit setup budget.
        let database = amphora::database::Database::in_memory(crate::store::deadline())?;
        database.call(std::time::Instant::now() + std::time::Duration::from_secs(3600), |c| {
        db(c)?;
        let words = ["compiling", "running", "test", "ok", "warning", "downloaded", "finished", "cargo", "serde", "tokio"];
        let mut seed = 7u64;
        for r in 0..10_000i64 {
            c.execute("INSERT INTO runs (id,number,repo_id,workflow_id,pipeline,branch,conclusion,status,created_at) VALUES (?1, ?2, 'repo', 'ci.yml', '', 'main', 'success', 'succeeded', ?2)", params![format!("r{r}"), r])?;
            for leg in 0..2i64 {
                let id = format!("r{r}l{leg}");
                c.execute("INSERT INTO jobs (id,run_id,key,finished_at) VALUES (?1, ?2, 'build', ?3)", params![id, format!("r{r}"), r])?;
                for step in 0..3u32 {
                    let text: String = (0..40)
                        .map(|i| {
                            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
                            let w = |k: u64| words.get(usize::try_from((seed >> k) % 10).unwrap_or(0)).copied().unwrap_or("x");
                            if r == 4321 && leg == 1 && step == 2 && i == 17 { "the unique-marker-9f3c string".to_owned() } else { format!("{} {} v{}.{} {}", w(13), w(21), (seed >> 7) % 40, (seed >> 29) % 10, w(37)) }
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    index_step(c, &id, step, &text)?;
                }
            }
        }
        let t = std::time::Instant::now();
        let hits = search(c, &Query { q: "unique-marker-9f3c".into(), limit: 50, ..Query::default() })?;
        let took = t.elapsed();
        eprintln!("10,000 runs, 60,000 step logs: found {} in {took:?}", hits.len());
        assert_eq!(hits.iter().map(|h| (h.run.as_str(), h.leg.as_str(), h.step, h.line)).collect::<Vec<_>>(), vec![("r4321", "r4321l1", 2, 18)]);
        assert!(took < std::time::Duration::from_millis(500), "{took:?}");
        Ok(())
        }).map_err(Into::into)
    }
}
