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
pub(crate) const MAX_INDEXED: usize = 8 << 20;

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
pub(crate) use index::{plain_reader,Plain};
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
    /// Legacy producer delivery is unsequenced: this qualifies the observed search view,
    /// never an attempt-bound close or producer completeness.
    pub source_qualification:String,
    pub accepted_revision:Option<i64>,
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
        (SELECT name FROM steps WHERE job_id=j.id AND idx=s.step),s.source_view,s.source_revision
        FROM ci_log_doc_order o JOIN ci_log_docs s ON s.id=o.doc AND s.index_version=o.version JOIN ci_log_search_ready ready ON ready.doc=s.id AND ready.version=s.index_version
        JOIN jobs j ON j.id=s.leg JOIN runs r ON r.id=j.run_id LEFT JOIN ci_log_accepted_views a ON a.job=j.id AND a.revision>0 AND a.retired=0
        WHERE o.ordinal=?1 AND o.leg=?2
          AND r.provider='kiln' AND j.status IN ('succeeded','failed','cancelled','skipped','timed_out')
          AND j.attempt=s.source_attempt AND j.finished_at=s.order_finished AND j.rowid=s.order_leg
          AND s.source_view IS a.view AND s.source_revision IS a.revision AND s.source_root IS a.root
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
                            r.get::<_, Option<String>>(13)?,r.get::<_, Option<i64>>(14)?,
                        ))
                    })
                    .optional()?;
                let Some((leg, step, line, body, run, job, number, repo, workflow, branch, conclusion, created_at, step_name,view,accepted_revision)) = row else {
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
                        source_qualification:if view.is_some(){crate::logs::accepted::QUALIFICATION.into()}else{"historical_file_delivery_unqualified".into()},
                        accepted_revision,
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
    c.execute("UPDATE ci_log_accepted_views SET retired=1 WHERE job=?1 AND NOT EXISTS(SELECT 1 FROM jobs WHERE id=?1)",[leg])?;
    c.execute("INSERT OR IGNORE INTO ci_log_accepted_retired(view) SELECT view FROM ci_log_accepted_views WHERE job=?1 AND retired=1",[leg])?;
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
    retired_view:Option<String>,
    unpublished_view:Option<String>,
    deleted_file:Option<String>,
    accepted_reads:std::collections::VecDeque<(String,String,u32,crate::logs::accepted::Reader)>,
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
        scan.accepted_reads.retain(|(l,_,_,_)|l!=&key);
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
        scan.accepted_reads.retain(|(l,_,_,_)|l!=&key);
            Ok(true)
        }
        index::Work::Idle => {
            scan.texts.retain(|(l, _, _, _, _)| l != &key);
        scan.accepted_reads.retain(|(l,_,_,_)|l!=&key);
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
                    let transient = scan.texts.len()+scan.accepted_reads.len()>=4;
                    let (text,bytes)=if let Some(prefix)=&plan.prefix {
                        scan.accepted_reads.retain(|(leg,version,step,_)|leg!=&plan.leg || (version==&plan.version && *step==plan.actual_step));
                        let pending=scan.accepted_reads.iter().position(|(leg,version,step,_)|leg==&plan.leg && version==&plan.version && *step==plan.actual_step).and_then(|at|scan.accepted_reads.remove(at));
                        let reader=if let Some((_,_,_,reader))=pending {reader}else{
                            if transient {return Ok(false)}
                            crate::logs::accepted::Reader::open(&hub.store,prefix.clone(),plan.actual_step,MAX_INDEXED).await?
                        };
                        match reader.advance(&hub.store,&hub.blobs.blobs).await? {
                            crate::logs::accepted::ReadTurn::Pending(reader)=>{
                                scan.accepted_reads.push_back((plan.leg.clone(),plan.version.clone(),plan.actual_step,*reader));return Ok(true)
                            }
                            crate::logs::accepted::ReadTurn::Ready(text,bytes)=>(text,bytes),
                        }
                    }else{
                        let Some(text)=hub.logs.index_text(&plan.leg,plan.actual_step,MAX_INDEXED,if transient {64<<10}else{u64::MAX}).await? else {return Ok(false)};text
                    };
                    let text = Arc::new(crate::rt::blocking(move || StepText::new(text)).await?);
                    if scan.texts.len()+scan.accepted_reads.len()<4 {
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
                scan.texts.retain(|(l, v, s, _, _)| l != &leg || v != &version || *s != step);
                scan.accepted_reads.retain(|(l,v,s,_)|l!=&leg || v!=&version || *s!=step)
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
    match crate::logs::accepted::repair_unpublished(&hub.store,&mut scan.unpublished_view).await {
        Ok(more)=>changed|=more,
        Err(error)=>{failed=true;tracing::warn!(error=%format!("{error:#}"),"original unpublished log suffix remains fenced; later work still receives a turn");}
    }
    match crate::logs::accepted::retirement_view(&hub.store,scan.retired_view.clone()).await {
        Ok(Some(view))=>{
            scan.retired_view=Some(view.clone());
            match crate::logs::accepted::retire_turn(&hub.logs,&hub.store,&hub.blobs.blobs,view).await {
                Ok(more)=>changed|=more,
                Err(error)=>{failed=true;tracing::warn!(error=%format!("{error:#}"),"accepted log key/pin retention incomplete; next view remains eligible");}
            }
        }
        Ok(None)=>scan.retired_view=None,
        Err(error)=>{failed=true;tracing::warn!(error=%format!("{error:#}"),"accepted log retirement cursor unavailable");}
    }
    match crate::logs::accepted::retire_file_turn(&hub.logs,&mut scan.deleted_file).await {
        Ok(more)=>changed|=more,
        Err(error)=>{failed=true;tracing::warn!(error=%format!("{error:#}"),"owned log-file retention remains incomplete");}
    }
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
        scan.accepted_reads.retain(|(l,_,_,_)|l!=&leg);
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
                    stage_pruned(c, &leg)?
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
        c.execute_batch(include_str!("../store/migrations/20261031-0100_u05_accepted_log_prefix.sql"))?;
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
        // Exercise the actual borrowed decoder with the existing encoder, including
        // empty application chunks and a legal frame exactly at the transport bound.
        let mut largest=vec![b'x';crate::logs::accepted::CHUNK];
        let oversize=crate::proto::encode(&crate::proto::LogChunk{job:"accepted-prefix".into(),idx:u32::MAX,data:largest.clone()})?;
        let overhead=oversize.len()-crate::logs::accepted::CHUNK;largest.truncate(largest.len()-overhead);
        for raw in [Vec::new(),b"legacy observed bytes\n".to_vec(),largest] {
            let encoded=crate::proto::encode(&crate::proto::LogChunk{job:"accepted-prefix".into(),idx:u32::MAX,data:raw.clone()})?;
            assert!(encoded.len()<=crate::logs::accepted::CHUNK);
            let borrowed=crate::proto::read_whole::<crate::logs::accepted::LegacyChunk<'_>>(&encoded).map_err(|e|anyhow::anyhow!("legacy borrowed fixture: {}",e.why("LogChunk")))?;
            assert_eq!((borrowed.job,borrowed.idx,borrowed.data),("accepted-prefix",u32::MAX,raw.as_slice()));
        }
        // One local fault decorator inside this existing fixture: the real configured
        // Blobs commits input, loses its PUT acknowledgment, then temporarily cannot read.
        // Every unrelated API delegates unchanged; no alternative journal/controller.
        use kiln_seams::blobs::{self,Blobs};
        use std::sync::atomic::{AtomicBool,AtomicU8,AtomicUsize,Ordering};
        struct LostPut {
            inner:Arc<dyn Blobs>,mode:Arc<AtomicU8>,attempts:Arc<AtomicUsize>,
            original:parking_lot::Mutex<Option<(String,String,usize)>>,exact:Arc<AtomicBool>,read_bytes:Arc<AtomicUsize>,blocked_key:Arc<parking_lot::Mutex<Option<String>>>,
        }
        impl Blobs for LostPut {
            fn put<'a>(&'a self,key:&'a str,bytes:Vec<u8>,put:blobs::Put)->futures::future::BoxFuture<'a,Result<blobs::Meta,blobs::Refusal>> {
                Box::pin(async move {
                    let mode=self.mode.load(Ordering::SeqCst);
                    if mode!=0 {
                        self.attempts.fetch_add(1,Ordering::SeqCst);
                        let mut original=self.original.lock();
                        let descriptor=(key.to_owned(),crate::util::sha256_hex(&bytes),bytes.len());
                        if let Some(first)=original.as_ref() {
                            if first.0==key && (first!=&descriptor || !matches!(&put.cond,blobs::Cond::IfNoneMatch) || put.sha256.as_deref()!=Some(first.1.as_str())) {self.exact.store(false,Ordering::SeqCst);}
                        }else{*original=Some(descriptor)}
                    }
                    if mode==2 {return Err(blobs::Refusal::Unavailable("fixture retains original bytes while PUT outcome is unknown".into()))}
                    let result=self.inner.put(key,bytes,put).await;
                    if mode==1 && result.is_ok() {self.mode.store(2,Ordering::SeqCst);return Err(blobs::Refusal::Unavailable("fixture lost committed PUT acknowledgment".into()))}
                    result
                })
            }
            fn get<'a>(&'a self,key:&'a str,range:Option<blobs::Range>)->futures::future::BoxFuture<'a,Result<(blobs::Meta,Vec<u8>),blobs::Refusal>> {
                Box::pin(async move {
                    let mode=self.mode.load(Ordering::SeqCst);
                    if mode==2 || (mode==4 && self.blocked_key.lock().as_deref()==Some(key)) {return Err(blobs::Refusal::Unavailable("fixture retains original operation while selected immutable input is unavailable".into()))}
                    let result=self.inner.get(key,range).await;
                    if let Ok((_,bytes))=&result {self.read_bytes.fetch_add(bytes.len(),Ordering::SeqCst);}
                    result
                })
            }
            fn head<'a>(&'a self,key:&'a str)->futures::future::BoxFuture<'a,Result<blobs::Meta,blobs::Refusal>> {self.inner.head(key)}
            fn delete<'a>(&'a self,key:&'a str)->futures::future::BoxFuture<'a,Result<(),blobs::Refusal>> {self.inner.delete(key)}
            fn list<'a>(&'a self,prefix:&'a str,after:Option<&'a str>,limit:usize)->futures::future::BoxFuture<'a,Result<blobs::Listing,blobs::Refusal>> {self.inner.list(prefix,after,limit)}
            fn start_upload<'a>(&'a self,key:&'a str)->futures::future::BoxFuture<'a,Result<String,blobs::Refusal>> {self.inner.start_upload(key)}
            fn upload_part<'a>(&'a self,upload:&'a str,part:u32,bytes:Vec<u8>)->futures::future::BoxFuture<'a,Result<String,blobs::Refusal>> {self.inner.upload_part(upload,part,bytes)}
            fn complete_upload<'a>(&'a self,upload:&'a str,parts:Vec<(u32,String)>)->futures::future::BoxFuture<'a,Result<blobs::Meta,blobs::Refusal>> {self.inner.complete_upload(upload,parts)}
            fn abort_upload<'a>(&'a self,upload:&'a str)->futures::future::BoxFuture<'a,Result<(),blobs::Refusal>> {self.inner.abort_upload(upload)}
            fn presign<'a>(&'a self,key:&'a str,access:blobs::Access,expires:Duration)->futures::future::BoxFuture<'a,Result<String,blobs::Refusal>> {self.inner.presign(key,access,expires)}
        }
        let read_bytes=Arc::new(AtomicUsize::new(0));
        let blocked_key=Arc::new(parking_lot::Mutex::new(None));
        let fault=Arc::new(AtomicU8::new(0));let attempts=Arc::new(AtomicUsize::new(0));let exact=Arc::new(AtomicBool::new(true));
        let mut hub = crate::hub::tests::scratch_hub().await?;
        let logs = crate::logs::Logs::new(hub.cfg.data("logs"), u64::try_from(MAX_INDEXED)? + 4096);
        let fault_blobs:Arc<dyn Blobs>=Arc::new(LostPut{inner:hub.blobs.blobs.clone(),mode:fault.clone(),attempts:attempts.clone(),exact:exact.clone(),read_bytes:read_bytes.clone(),blocked_key:blocked_key.clone(),original:parking_lot::Mutex::new(None)});
        logs.bind_accepted(hub.store.clone(),fault_blobs,"bounded-indexing".into());
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
        hub.logs.historical_fixture_append("older", 1, body.as_bytes());
        hub.logs.historical_fixture_append("older", 2, b"chronologyneedle older last segment");
        hub.logs.finish("older");
        hub.logs.historical_fixture_append("newer", 1, later.as_bytes());
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
        hub.logs.historical_fixture_append("backdated", 1, b"fresh backdated needle");
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
        // U05: extend this SAME product fixture through the real retained Logs owner.
        // R1 captures complete old file history; late R2 must invalidate ready R1 without
        // changing its terminal tuple or claiming a producer/attempt close frontier.
        hub.store.call(|c|{
            c.execute("INSERT INTO jobs(id,run_id,key,display,status,labels,needs,spec,finished_at,attempt) VALUES('accepted-prefix','log-run','accepted','accepted','failed','[]','[]','{}',300,1)",[])?;
            c.execute("INSERT INTO steps(job_id,idx,name,status) VALUES('accepted-prefix',1,'historical','failed')",[])?;
            stage_finished(c,"accepted-prefix")?;Ok(())
        }).await?;
        hub.logs.historical_fixture_append("accepted-prefix",1,b"historicalneedle retained before migration\n");
        hub.logs.finish("accepted-prefix");
        let mut accepted_scan=Scan::default();
        for _ in 0..16 {pass(&hub,&mut accepted_scan).await?;}
        fault.store(1,Ordering::SeqCst);
        hub.logs.observe_server("accepted-prefix".into(),2,b"firstacceptedneedle R1\n".to_vec())?;
        for _ in 0..128 {
            if fault.load(Ordering::SeqCst)==2 {break}
            // Cancellation does not discard the original future/raw body: it lives in
            // Logs, while unrelated Hub controls may continue polling.
            let _=crate::rt::timeout(Duration::from_millis(50),hub.logs.accepted_completion()).await;
        }
        assert_eq!(fault.load(Ordering::SeqCst),2,"fixture did not reach its ambiguous committed PUT");
        hub.store.call(|c|{
            assert!(crate::logs::accepted::prefix(c,"accepted-prefix")?.is_none(),"ambiguous PUT alone cannot publish accepted head");
            assert_eq!(c.query_row("SELECT COUNT(*) FROM ci_log_accepted_ops WHERE job='accepted-prefix' AND status='pending'",[],|r|r.get::<_,i64>(0))?,1,"same original operation retained");Ok(())
        }).await?;
        fault.store(3,Ordering::SeqCst);
        let r1=hub.logs.accepted_completion().await?;
        assert!(r1.accepted,"first observed append includes historical prefix: {r1:?}");
        assert!(attempts.load(Ordering::SeqCst)>=2 && exact.load(Ordering::SeqCst),"ambiguous immutable PUT retries preserve exact original key/body/digest/condition");
        fault.store(0,Ordering::SeqCst);
        let original=r1.clone();
        hub.store.call(move |c|{
            let before=crate::logs::accepted::prefix(c,"accepted-prefix")?;
            // Discard an exact own CAS receipt and ask for the SAME operation again.
            // This checks transition idempotence, not a synthetic shared-actor reset.
            let _=crate::logs::accepted::publish(c,&original.operation)?;
            assert_eq!(crate::logs::accepted::publish(c,&original.operation)?,original);
            assert_eq!(crate::logs::accepted::completed(c,&original.operation)?,Some(original));
            assert_eq!(crate::logs::accepted::prefix(c,"accepted-prefix")?,before);Ok(())
        }).await?;
        let mut stale_read=None;let mut stale_post=None;
        for _ in 0..64 {
            // A whole pass can finish pruning and consume the first Read in its same
            // fresh-leg turn. Advance one real prune atom before sampling that Read.
            let Some(work)=hub.store.call(|c|{
                let pruning:Option<String>=c.query_row("SELECT version FROM ci_log_index_progress WHERE leg='accepted-prefix' AND pruning=1",[],|r|r.get(0)).optional()?;
                if let Some(version)=pruning {index::prune(c,"accepted-prefix",&version)?;return Ok(None)}
                Ok(Some(index::leg(c,"accepted-prefix")?))
            }).await? else {continue};
            match work {
                index::Work::Read(plan)=>{if stale_read.is_none(){stale_read=Some(plan)}},
                index::Work::Post(id) if stale_post.is_none()=>{
                    stale_post=hub.store.call(move |c|Ok(index::load(c,id)?.map(|(doc,body)|(doc,trigrams(&body).into_iter().take(index::GRAMS_PER_BATCH).collect::<Vec<_>>())))).await?;
                }
                _=>{}
            }
            pass(&hub,&mut accepted_scan).await?;
        }
        let captured=(stale_read.ok_or_else(||anyhow::anyhow!("R1 did not expose read work"))?,stale_post.ok_or_else(||anyhow::anyhow!("R1 did not expose posting work"))?);
        assert_eq!(captured.0.prefix.as_ref(),Some(&r1.prefix),"captured read belongs to R1");
        assert_eq!(captured.1.0.prefix.as_ref(),Some(&r1.prefix),"captured posting belongs to R1");
        hub.store.call(|c|{
            let hits=search(c,&Query{q:"historicalneedle".into(),..Query::default()})?;
            assert_eq!(hits.iter().filter(|h|h.leg=="accepted-prefix").count(),1,"first append cannot shadow historical file lines");
            assert!(hits.iter().all(|h|h.source_qualification==crate::logs::accepted::QUALIFICATION));Ok(())
        }).await?;
        hub.logs.observe_server("accepted-prefix".into(),u32::MAX,b"lateacceptedneedle R2\n".to_vec())?;
        let r2=hub.logs.accepted_completion().await?;
        assert!(r2.accepted && r2.prefix.view==r1.prefix.view && r2.prefix.revision>r1.prefix.revision);
        let expected=r2.prefix.clone();
        hub.store.call(move |c|{
            assert_eq!(crate::logs::accepted::prefix(c,"accepted-prefix")?,Some(expected));
            assert!(search(c,&Query{q:"historicalneedle".into(),..Query::default()})?.iter().all(|h|h.leg!="accepted-prefix"),"old readiness hides immediately at R2 commit");
            assert!(!index::stage(c,&captured.0,"stale R1",true,0)?,"stale R1 read cannot publish into R2");
            assert!(!index::post(c,&captured.1.0,&captured.1.1,false)?,"stale R1 posting cannot publish into R2");Ok(())
        }).await?;
        // Discard transient readers/grams as on indexer restart; SQL head/cursors retain R2.
        accepted_scan=Scan::default();
        for _ in 0..128 {pass(&hub,&mut accepted_scan).await?;}
        let revision=r2.prefix.revision;
        hub.store.call(move |c|{
            for query in ["historicalneedle","firstacceptedneedle","lateacceptedneedle"] {
                let hits=search(c,&Query{q:query.into(),..Query::default()})?;
                let own=hits.iter().filter(|h|h.leg=="accepted-prefix").collect::<Vec<_>>();
                assert_eq!(own.len(),1,"no duplicate or missing old/new lines: {query}");
                assert_eq!(own.first().and_then(|h|h.accepted_revision),Some(revision));
            }
            let hit=search(c,&Query{q:"lateacceptedneedle".into(),..Query::default()})?;
            assert_eq!(hit.first().map(|h|h.step),Some(u32::MAX),"legal legacy maximum step advances without overflow");Ok(())
        }).await?;
        // One healthy history verification reads each captured step once, even when the
        // OLD ready index has many segments. Count observed Blob bytes, not internal loops
        // or wall time; the former whole-step-per-doc algorithm exceeds this allowance.
        hub.store.call(|c|{
            c.execute("INSERT INTO jobs(id,run_id,key,display,status,labels,needs,spec,finished_at,attempt) VALUES('history-linear','log-run','linear','linear','failed','[]','[]','{}',301,1)",[])?;
            c.execute("INSERT INTO steps(job_id,idx,name,status) VALUES('history-linear',1,'old history','failed')",[])?;stage_finished(c,"history-linear")
        }).await?;
        let history=(0..1000).map(|line|format!("linearityneedle {line:04} {}\n","z".repeat(480))).collect::<String>();
        let history_size=history.len();hub.logs.historical_fixture_append("history-linear",1,history.as_bytes());hub.logs.finish("history-linear");
        let mut linear_scan=Scan::default();for _ in 0..1024{pass(&hub,&mut linear_scan).await?;}
        hub.store.call(|c|{assert!(c.query_row("SELECT COUNT(*) FROM ci_log_docs WHERE leg='history-linear'",[],|r|r.get::<_,u64>(0))?>=8);Ok(())}).await?;
        let before=read_bytes.load(Ordering::SeqCst);
        let linear=hub.logs.append_server("history-linear".into(),2,b"afterhistoryneedle\n".to_vec()).await?;
        assert!(linear.accepted);assert!(read_bytes.load(Ordering::SeqCst).saturating_sub(before)<=history_size*3+4096,"historical verification reread the step per old document");

        // A >4MiB canonical raw source (including bytes Plain would change) copies through
        // the SAME retained owner into a target that already has accepted live output.
        hub.store.call(|c|{
            for job in ["large-source","large-copy","fifth-reader"] {
                c.execute("INSERT INTO jobs(id,run_id,key,display,status,labels,needs,spec,finished_at,attempt) VALUES(?1,'log-run','raw','raw','failed','[]','[]','{}',302,1)",[job])?;
            }Ok(())
        }).await?;
        let mut raw=vec![b'x';crate::logs::accepted::CHUNK];
        for (at,byte) in [0xff,0,27,b'[',b'3',b'1',b'm'].into_iter().enumerate(){if let Some(slot)=raw.get_mut(at){*slot=byte;}}
        let tail=[b"rawcopytailneedle\n".as_slice(),vec![b'y';1024].as_slice()].concat();
        hub.logs.append_server("large-source".into(),1,raw.clone()).await?;
        hub.logs.append_server("large-source".into(),1,tail.clone()).await?;
        hub.logs.append_server("large-copy".into(),1,b"beforecopyneedle\n".to_vec()).await?;
        let effect=crate::util::sha256_hex(b"large-original-memo-effect");
        let first=b"FIRST SavedHit fixture payload".to_vec();
        let result=hub.logs.copy_step(crate::logs::accepted::CopyStep{job:"large-copy".into(),step:2,target_attempt:1,effect:effect.clone(),source_job:"large-source".into(),source_step:1,result:first.clone(),output_tree:None}).await?;
        assert_eq!(result,first);
        let check=effect.clone();let target_head=hub.store.call(move|c|{
            let op=crate::logs::accepted::copy_metadata_operation(c,"large-copy",2,1,&check)?.ok_or_else(||anyhow::anyhow!("first metadata unexpectedly applied"))?;
            assert!(c.query_row("SELECT active FROM ci_log_accepted_copy WHERE operation=?1",[op],|r|r.get::<_,bool>(0))?,"source lease must survive target head until metadata receipt");
            crate::logs::accepted::prefix(c,"large-copy")
        }).await?;
        hub.logs.append_server("large-source".into(),1,b"later source must not duplicate target copy\n".to_vec()).await?;
        let replay=hub.logs.copy_step(crate::logs::accepted::CopyStep{job:"large-copy".into(),step:2,target_attempt:1,effect:effect.clone(),source_job:"large-source".into(),source_step:1,result:b"NEWER selected result must not replace FIRST".to_vec(),output_tree:None}).await?;
        assert_eq!(replay,first);
        hub.store.call(move|c|{assert_eq!(crate::logs::accepted::prefix(c,"large-copy")?,target_head);Ok(())}).await?;
        // Real existing Store/provider retention paths honor the original source SQL
        // lease, while unrelated eligible work proceeds. Release in the metadata tx then
        // permits the next ordinary pass; no synthetic second retention owner is used.
        hub.store.call(|c|{
            c.execute("INSERT INTO runs(id,repo_id,number,pipeline,event,branch,sha,status,created_at,provider,external_id,attempt) VALUES('lease-provider-run','log-repo',2,'ci','push','main','abc','failed',1,'kiln','lease-provider-ext',1)",[])?;
            c.execute("INSERT INTO runs(id,repo_id,number,pipeline,event,branch,sha,status,created_at) VALUES('eligible-retention-run','log-repo',3,'ci','push','main','abc','failed',1)",[])?;
            for (job,run) in [("lease-provider-source","lease-provider-run"),("lease-provider-unheld","lease-provider-run"),("eligible-retention-job","eligible-retention-run")] {
                c.execute("INSERT INTO jobs(id,run_id,key,display,status,labels,needs,spec,finished_at,attempt,run_attempt) VALUES(?1,?2,'lease','lease','failed','[]','[]','{}',304,1,1)",params![job,run])?;
            }Ok(())
        }).await?;
        hub.logs.append_server("lease-provider-source".into(),1,b"leased provider source\n".to_vec()).await?;
        let lease_effect=crate::util::sha256_hex(b"retention-source-effect");
        hub.logs.copy_step(crate::logs::accepted::CopyStep{job:"large-copy".into(),step:3,target_attempt:1,effect:lease_effect.clone(),source_job:"lease-provider-source".into(),source_step:1,result:b"original retention result".to_vec(),output_tree:None}).await?;
        let deleted=hub.store.prune(0).await?;
        assert!(deleted.iter().any(|id|id=="eligible-retention-job"),"unheld old run progresses while original source is leased");
        hub.store.call(|c|{assert!(c.query_row("SELECT EXISTS(SELECT 1 FROM jobs WHERE id='lease-provider-source')",[],|r|r.get::<_,bool>(0))?);Ok(())}).await?;
        assert!(hub.store.delete_repo("log-repo".into()).await.is_err(),"repo cascade must refuse before deleting leased source metadata");
        let repository=hub.store.repos().await?.into_iter().find(|repo|repo.id=="log-repo").ok_or_else(||anyhow::anyhow!("leased repository disappeared"))?;
        let provider_run=crate::ci::providers::ExtRun{external_id:"lease-provider-ext".into(),status:"failed".into(),attempt:1,created_at:1,..Default::default()};
        crate::ci::providers::record(&hub,&repository,"kiln",&provider_run,Some(&[])).await?;
        hub.store.call(|c|{
            assert!(c.query_row("SELECT EXISTS(SELECT 1 FROM jobs WHERE id='lease-provider-source')",[],|r|r.get::<_,bool>(0))?);
            assert!(!c.query_row("SELECT EXISTS(SELECT 1 FROM jobs WHERE id='lease-provider-unheld')",[],|r|r.get::<_,bool>(0))?,"unheld omitted provider job progresses");Ok(())
        }).await?;
        let release=lease_effect.clone();hub.store.call(move|c|{let tx=crate::store::tx(c)?;let op=crate::logs::accepted::copy_metadata_operation(&tx,"large-copy",3,1,&release)?.ok_or_else(||anyhow::anyhow!("retention metadata receipt missing"))?;crate::logs::accepted::mark_copy_metadata(&tx,&op)?;tx.commit()?;Ok(())}).await?;
        crate::ci::providers::record(&hub,&repository,"kiln",&provider_run,Some(&[])).await?;
        // Preserve this fixture's still-active main run while proving a released source run
        // is now eligible. This is fixture chronology, not a retention flag/default waiver.
        hub.store.call(|c|{c.execute("UPDATE runs SET created_at=?1 WHERE id='log-run'",[crate::util::now_unix()+86_400])?;Ok(())}).await?;
        hub.store.prune(0).await?;
        hub.store.call(|c|{assert!(!c.query_row("SELECT EXISTS(SELECT 1 FROM runs WHERE id='lease-provider-run')",[],|r|r.get::<_,bool>(0))?);Ok(())}).await?;
        let check=effect.clone();hub.store.call(move|c|{
            let tx=crate::store::tx(c)?;let op=crate::logs::accepted::copy_metadata_operation(&tx,"large-copy",2,1,&check)?.ok_or_else(||anyhow::anyhow!("missing metadata operation"))?;
            crate::logs::accepted::mark_copy_metadata(&tx,&op)?;
            assert!(crate::logs::accepted::copy_metadata_operation(&tx,"large-copy",2,1,&check)?.is_none());tx.commit()?;Ok(())
        }).await?;
        let keys=hub.store.call(|c|Ok(c.prepare("SELECT x.key FROM ci_log_accepted_chunks x JOIN ci_log_accepted_views v ON v.view=x.view WHERE v.job='large-copy' AND x.step=2 AND x.revision<=v.revision ORDER BY x.step_ordinal")?.query_map([],|r|r.get::<_,String>(0))?.collect::<crate::sql::Result<Vec<_>>>()?)).await?;
        let mut copied=Vec::new();for key in keys{let (_,bytes)=hub.blobs.blobs.get(&key,None).await?;copied.extend(bytes);}
        let mut expected=raw.clone();expected.extend(tail.clone());assert_eq!(copied,expected,"raw copy must retain bytes beyond a frame and bytes Plain would change");
        hub.logs.mark("large-copy",2,"attempt 2 boundaryneedle").await;
        let mut copy_scan=Scan::default();for _ in 0..128{pass(&hub,&mut copy_scan).await?;}
        hub.store.call(|c|{assert!(search(c,&Query{q:"boundaryneedle".into(),..Query::default()})?.iter().any(|h|h.leg=="large-copy"));Ok(())}).await?;

        // Four forgotten partial accepted readers cannot permanently consume all four
        // bounded indexer slots. Exercise real large-copy/read/prune work, then a fifth leg.
        let mut reader_scan=Scan::default();
        for n in 0..4u32 {
            let job=format!("forgotten-reader-{n}");let own=job.clone();
            hub.store.call(move|c|{c.execute("INSERT INTO jobs(id,run_id,key,display,status,labels,needs,spec,finished_at,attempt) VALUES(?1,'log-run','reader','reader','failed','[]','[]','{}',303,1)",[own])?;Ok(())}).await?;
            let effect=crate::util::sha256_hex(job.as_bytes());
            hub.logs.copy_step(crate::logs::accepted::CopyStep{job:job.clone(),step:1,target_attempt:1,effect:effect.clone(),source_job:"large-source".into(),source_step:1,result:b"reader-result".to_vec(),output_tree:None}).await?;
            let id=job.clone();hub.store.call(move|c|{let tx=crate::store::tx(c)?;let op=crate::logs::accepted::copy_metadata_operation(&tx,&id,1,1,&effect)?.ok_or_else(||anyhow::anyhow!("reader copy metadata"))?;crate::logs::accepted::mark_copy_metadata(&tx,&op)?;tx.commit()?;Ok(())}).await?;
            visit_leg(&hub,&mut reader_scan,job).await?;
        }
        assert_eq!(reader_scan.accepted_reads.len(),4,"fixture must retain four genuinely partial readers");
        hub.store.call(|c|{
            let tx=crate::store::tx(c)?;for n in 0..4{let job=format!("forgotten-reader-{n}");tx.execute("DELETE FROM jobs WHERE id=?1",[&job])?;stage_pruned(&tx,&job)?;tx.execute("INSERT INTO ci_log_deferred_delete(job) VALUES(?1) ON CONFLICT(job) DO NOTHING",[job])?;}tx.commit()?;Ok(())
        }).await?;
        for _ in 0..64{pass(&hub,&mut reader_scan).await?;}
        assert!(reader_scan.accepted_reads.is_empty(),"pruning forgotten readers releases transient capacity");
        hub.logs.append_server("fifth-reader".into(),1,b"fifthreaderneedle\n".to_vec()).await?;
        for _ in 0..128{pass(&hub,&mut reader_scan).await?;}
        hub.store.call(|c|{assert!(search(c,&Query{q:"fifthreaderneedle".into(),..Query::default()})?.iter().any(|h|h.leg=="fifth-reader"));Ok(())}).await?;

        // Revocation during actual retained copy I/O cannot publish the old effect into
        // a replacement target attempt, or turn its Unknown into a fresh cache selection.
        hub.store.call(|c|{c.execute("INSERT INTO jobs(id,run_id,key,display,status,labels,needs,spec,finished_at,attempt) VALUES('stale-copy','log-run','stale','stale','failed','[]','[]','{}',305,1)",[])?;Ok(())}).await?;
        // End is a compatibility projection boundary after already-observed FIFO inputs,
        // not producer delivery closure. No Hub storage await/CPU lease is introduced.
        hub.store.call(|c|{c.execute("INSERT INTO jobs(id,run_id,key,display,status,labels,needs,spec,finished_at,attempt) VALUES('finish-order','log-run','finish','finish','failed','[]','[]','{}',305,1)",[])?;Ok(())}).await?;
        let mut viewer=hub.logs.subscribe("finish-order");
        hub.logs.observe_server("finish-order".into(),1,b"first observed\n".to_vec())?;
        hub.logs.observe_server("finish-order".into(),1,b"second observed\n".to_vec())?;
        hub.logs.finish("finish-order");hub.logs.finish("finish-order");
        assert!(hub.logs.is_live("finish-order"),"queued original inputs must keep the existing viewer channel open");
        // Own reply also drives unrelated successful append completions without relying
        // on a concurrently polling Hub; the whole exercise has a real finite timeout.
        crate::rt::timeout(Duration::from_secs(10),hub.logs.append_server("stale-copy".into(),1,b"current original prefix\n".to_vec())).await??;
        let shown=crate::rt::timeout(Duration::from_secs(10),async {
            use futures::FutureExt as _;
            let mut shown=Vec::new();
            loop {
                futures::select_biased! {
                    event=viewer.recv().fuse()=>match event? {
                        crate::logs::LogEvent::Data{data,..}=>shown.extend_from_slice(&data),
                        crate::logs::LogEvent::End=>return Ok::<_,anyhow::Error>(shown),
                        crate::logs::LogEvent::Step{..}=>{},
                    },
                    completion=hub.logs.accepted_completion().fuse()=>{completion?;},
                }
            }
        }).await??;
        assert_eq!(shown,b"first observed\nsecond observed\n");
        assert!(!hub.logs.is_live("finish-order"));
        assert!(viewer.recv().await.is_err(),"duplicate finish emits only one End on the original channel");
        let original_head=hub.store.call(|c|crate::logs::accepted::prefix(c,"stale-copy")).await?;
        let stale_effect=crate::util::sha256_hex(b"stale-copy-effect");
        *blocked_key.lock()=Some(hub.store.call(|c|Ok(c.query_row("SELECT x.key FROM ci_log_accepted_chunks x JOIN ci_log_accepted_views v ON v.view=x.view WHERE v.job='large-source' AND x.step=1 AND x.step_ordinal=2",[],|r|r.get::<_,String>(0))?)).await?);
        fault.store(4,Ordering::SeqCst);
        let pending=hub.logs.copy_step(crate::logs::accepted::CopyStep{job:"stale-copy".into(),step:2,target_attempt:1,effect:stale_effect.clone(),source_job:"large-source".into(),source_step:1,result:b"original stale result".to_vec(),output_tree:None});
        futures::pin_mut!(pending);
        let mut admitted=false;
        for _ in 0..128 {
            let result=crate::rt::timeout(Duration::from_millis(50),pending.as_mut()).await;
            assert!(result.is_err(),"unavailable original source bytes cannot produce success/refusal before target authority changes");
            admitted=hub.store.call(|c|Ok(c.query_row("SELECT EXISTS(SELECT 1 FROM ci_log_accepted_ops WHERE job='stale-copy' AND status='pending' AND staged_revision>expected_revision)",[],|r|r.get::<_,bool>(0))?)).await?;
            if admitted{break}
        }
        assert!(admitted,"copy fixture never reached its retained original operation");
        hub.store.call(|c|{c.execute("UPDATE jobs SET attempt=2 WHERE id='stale-copy'",[])?;Ok(())}).await?;
        fault.store(0,Ordering::SeqCst);
        assert!(crate::rt::timeout(Duration::from_secs(10),pending).await?.is_err(),"revoked target attempt cannot receive copied bytes/result200");
        hub.store.call(move|c|{
            assert_eq!(crate::logs::accepted::prefix(c,"stale-copy")?,original_head,"old effect cannot advance the new attempt's job view");
            assert_eq!(c.query_row("SELECT status FROM ci_log_accepted_ops WHERE job='stale-copy' AND origin='server-memo-copy'",[],|r|r.get::<_,String>(0))?,"refused");
            assert!(!c.query_row("SELECT active FROM ci_log_accepted_copy x JOIN ci_log_accepted_ops o ON o.operation=x.operation WHERE o.job='stale-copy'",[],|r|r.get::<_,bool>(0))?,"own refusal releases source use while retaining its target pins");Ok(())
        }).await?;
        assert!(hub.logs.resume_copy("stale-copy",2,1,&stale_effect).await.is_err(),"old attempt cannot be mistaken for no original slot");
        let retained_pins=hub.store.call(|c|Ok(c.query_row("SELECT COUNT(*) FROM ci_log_accepted_pins p JOIN ci_log_accepted_ops o ON o.operation=p.operation WHERE o.job='stale-copy' AND o.status='refused'",[],|r|r.get::<_,u64>(0))?)).await?;
        assert!(retained_pins>0,"staged refusal keeps original immutable custody");
        let successor=hub.logs.append_server("stale-copy".into(),2,b"afterrefusedcopyneedle\n".to_vec());
        futures::pin_mut!(successor);
        assert!(crate::rt::timeout(Duration::from_millis(50),successor.as_mut()).await.is_err(),"recoverable suffix fence retains an already-admitted successor instead of refusing/dropping it");
        let mut suffix_scan=Scan::default();let mut progressed=false;
        for _ in 0..128 {
            pass(&hub,&mut suffix_scan).await?;
            if let Ok(result)=crate::rt::timeout(Duration::from_millis(50),successor.as_mut()).await {result?;progressed=true;break}
        }
        assert!(progressed,"same owner/indexer must unlink refused unpublished suffix and admit the retained successor");
        for _ in 0..128 {pass(&hub,&mut suffix_scan).await?;}
        hub.store.call(move|c|{
            assert_eq!(c.query_row("SELECT COUNT(*) FROM ci_log_accepted_pins p JOIN ci_log_accepted_ops o ON o.operation=p.operation WHERE o.job='stale-copy' AND o.status='refused'",[],|r|r.get::<_,u64>(0))?,retained_pins,"repair must not release any original Blob pin");
            assert!(!c.query_row("SELECT EXISTS(SELECT 1 FROM ci_log_accepted_chunks x JOIN ci_log_accepted_ops o ON o.operation=x.operation WHERE o.job='stale-copy' AND o.status='refused')",[],|r|r.get::<_,bool>(0))?);
            assert!(!c.query_row("SELECT EXISTS(SELECT 1 FROM ci_log_accepted_unpublished u JOIN ci_log_accepted_views v ON v.view=u.view WHERE v.job='stale-copy')",[],|r|r.get::<_,bool>(0))?);
            assert_eq!(search(c,&Query{q:"afterrefusedcopyneedle".into(),..Query::default()})?.iter().filter(|h|h.leg=="stale-copy").count(),1);
            assert!(search(c,&Query{q:"current original prefix".into(),..Query::default()})?.iter().any(|h|h.leg=="stale-copy"),"published prefix survives refused suffix cleanup");Ok(())
        }).await?;

        // Known admission failure and missing immutable bytes never acquire readiness.
        assert!(hub.logs.observe_server("accepted-prefix".into(),3,vec![0;crate::logs::accepted::CHUNK+1]).is_err());
        let missing=hub.store.call(|c|Ok(c.query_row("SELECT key FROM ci_log_accepted_chunks x JOIN ci_log_accepted_views v ON v.view=x.view WHERE v.job='accepted-prefix' ORDER BY x.revision DESC LIMIT 1",[],|r|r.get::<_,String>(0))?)).await?;
        hub.blobs.blobs.delete(&missing).await?;
        hub.store.call(|c|{c.execute("DELETE FROM ci_log_indexed WHERE leg='accepted-prefix'",[])?;stage_finished(c,"accepted-prefix")}).await?;
        // This fixture's explicit corruption is NOT a production retry policy. The same
        // bounded indexer refuses missing owned bytes and keeps derived work incomplete.
        accepted_scan=Scan::default();
        let mut refused_read=false;
        for _ in 0..128 {if pass(&hub,&mut accepted_scan).await.is_err(){refused_read=true;break}}
        assert!(refused_read,"missing accepted object must fail the read instead of accepted empty data");
        hub.store.call(|c|{assert!(!c.query_row("SELECT EXISTS(SELECT 1 FROM ci_log_indexed WHERE leg='accepted-prefix')",[],|r|r.get::<_,bool>(0))?);Ok(())}).await?;
        // Ambiguous PUT lifetime remains pinned on retirement; later settled views still
        // get their own turn. Tombstoned IDs cannot be INSERT OR IGNORE/upserted back.
        let held=hub.store.call(|c|Ok(c.query_row("SELECT key FROM ci_log_accepted_pins p JOIN ci_log_accepted_views v ON v.view=p.view WHERE v.job='accepted-prefix' AND p.ambiguous=1 LIMIT 1",[],|r|r.get::<_,String>(0))?)).await?;
        hub.store.call(|c|{let tx=crate::store::tx(c)?;for job in ["accepted-prefix","fifth-reader"]{tx.execute("DELETE FROM jobs WHERE id=?1",[job])?;stage_pruned(&tx,job)?;tx.execute("INSERT INTO ci_log_deferred_delete(job) VALUES(?1) ON CONFLICT(job) DO NOTHING",[job])?;}tx.commit()?;Ok(())}).await?;
        let mut retirement=Scan::default();for _ in 0..128{let _=pass(&hub,&mut retirement).await?;}
        let retained=held.clone();hub.store.call(move|c|{
            assert!(c.query_row("SELECT EXISTS(SELECT 1 FROM ci_log_accepted_pins WHERE key=?1 AND ambiguous=1)",[retained],|r|r.get::<_,bool>(0))?);
            assert_eq!(c.query_row("SELECT COUNT(*) FROM ci_log_accepted_pins p JOIN ci_log_accepted_views v ON v.view=p.view WHERE v.job='fifth-reader'",[],|r|r.get::<_,u64>(0))?,0,"later eligible retirement must not starve behind held oldest view");
            assert!(c.execute("INSERT OR IGNORE INTO jobs(id,run_id,key,display,status,labels,needs,spec) VALUES('fifth-reader','log-run','reuse','reuse','queued','[]','[]','{}')",[]).is_err(),"retired ID cannot bypass fence with INSERT OR IGNORE");
            assert!(c.execute("INSERT INTO jobs(id,run_id,key,display,status,labels,needs,spec) VALUES('fifth-reader','log-run','reuse','reuse','queued','[]','[]','{}') ON CONFLICT(id) DO UPDATE SET status='queued'",[]).is_err(),"retired ID cannot bypass fence with upsert");Ok(())
        }).await?;
        assert!(hub.blobs.blobs.head(&held).await.is_ok(),"ambiguous retained key cannot be physically removed");
        hub.store.delete_repo("log-repo".into()).await?;
        hub.store.call(|c|{assert!(!c.query_row("SELECT EXISTS(SELECT 1 FROM repos WHERE id='log-repo')",[],|r|r.get::<_,bool>(0))?,"released source lease permits later explicit repo deletion");Ok(())}).await?;
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
