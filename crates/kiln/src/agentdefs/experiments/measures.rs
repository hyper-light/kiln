//! What an experiment's sessions measure (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d P7.1), as the Agents page shows
//! it: each variant's `measures` and `trend`, and each criterion's `values`.
//!
//! - **A session's facts** ([`Reading`]), kept with its draw as kiln first reads them: its warm start, where its first
//!   start was a warm one (a warm host's microVM, ready before its run asked: Shards' `warm`), and how long that start
//!   took; and once it's over (it ended, or kiln refused its start), whether it failed (it ended in error, never ran,
//!   or was refused) and its microVM's peak memory. kiln reads them as an experiment is answered, as a session goes
//!   ([`closed`]) and as a start is refused.
//! - **Its metrics**: each fact written once, as it's kept, to kiln's observability (`docs/observe.md`), a value of a
//!   distribution at the time it happened, tagged with its `experiment` and `variant` (their ids):
//!   `kiln.session.warm_ms`, `kiln.session.peak_memory_gib` and `kiln.session.failed_pct` (100 a failed session, 0
//!   another). A query reads them as it reads any of Datadog's distributions (`avg:`, `p50:`, `count:`, `by
//!   {variant}`).
//! - **A variant's measures** ([`measures`]): the median of its warm starts and of its peaks, and the rate of its
//!   failures, each with its 95% confidence sequence (`kiln_ci::stats::sequence`, its sessions read in the order they
//!   were drawn), so a reading taken at any time holds; `[low, mid, high]`, given once each of the three is bounded on
//!   both sides.
//! - **Its trend** ([`trends`]): each measure as it stood at the end of each day (UTC), from the first day on which
//!   every variant that has it had one to today, so every variant's line covers the same days.
//! - **A criterion's values** ([`judge`]): its query and bound evaluated as kiln's monitors evaluate a monitor of them
//!   (`kiln_obs::monitor::evaluate_shared`), as of now, over kiln's observability: a value for each variant its query
//!   groups by (`by {variant}`), and whether it passes the bound.

use std::collections::BTreeMap;
use std::sync::Arc;

use kiln_ci::stats::sequence::{self, Sequence};
use kiln_obs::batch::SeriesKey;
use rusqlite::{OptionalExtension as _, params};
use serde::Serialize;

use super::{Criterion, Experiment};
use crate::harness::model::Status;
use crate::harness::session::Info;
use crate::hub::Hub;

/// The level of each variant's intervals: the Agents page's "95%" interval of a measure.
pub const LEVEL: f64 = 0.95;

/// A day of Unix time, in milliseconds: 86,400 s, as Unix time counts no leap seconds.
const DAY_MS: i64 = 86_400_000;

/// A GiB (2^30 bytes).
const GIB: f64 = 1_073_741_824.0;

/// A measure of a session: the three the Agents page names (each variant's `measures` and `trend`), and the rest of
/// P7.1's deployment measures, which its criteria read in kiln's monitoring syntax (kiln plan
/// 01a1dd2ea7a777a28970a43fc5f766d0 P7.1: "start cold/template/warm, footprint peak and steady, host CPU, failures,
/// session outcome").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Measure {
    WarmMs,
    PeakGib,
    FailedPct,
    /// Its first start's time, tagged with its kind (`start:cold|template|warm|swapped|other`).
    StartMs,
    /// Its microVM's footprint as its last start ended.
    SteadyGib,
    /// The host CPU its microVMs used, all its starts'.
    CpuMs,
    /// 1 a session, tagged with how it ended (`outcome:ended|failed|refused`): `count:` of it counts them.
    Outcome,
}

impl Measure {
    /// The Agents page's.
    const SHOWN: [Measure; 3] = [Measure::WarmMs, Measure::PeakGib, Measure::FailedPct];
    const ALL: [Measure; 7] = [Measure::WarmMs, Measure::PeakGib, Measure::FailedPct, Measure::StartMs, Measure::SteadyGib, Measure::CpuMs, Measure::Outcome];

    /// Its metric in kiln's observability.
    fn metric(self) -> &'static str {
        match self {
            Measure::WarmMs => "kiln.session.warm_ms",
            Measure::PeakGib => "kiln.session.peak_memory_gib",
            Measure::FailedPct => "kiln.session.failed_pct",
            Measure::StartMs => "kiln.session.start_ms",
            Measure::SteadyGib => "kiln.session.steady_memory_gib",
            Measure::CpuMs => "kiln.session.cpu_ms",
            Measure::Outcome => "kiln.session.outcome",
        }
    }

    /// Its value in `r` and when it was taken (Unix ms): none where `r` has none yet.
    fn of(self, r: &Reading) -> Option<(i64, f64)> {
        match self {
            Measure::WarmMs => r.warm.map(|w| (w.at, w.ms)),
            Measure::PeakGib => r.end.and_then(|e| Some((e.at, e.peak? as f64 / GIB))),
            Measure::FailedPct => r.end.map(|e| (e.at, if e.failed { 100.0 } else { 0.0 })),
            Measure::StartMs => r.first.map(|f| (f.at, f.ms)),
            Measure::SteadyGib => r.end.and_then(|e| Some((e.at, e.steady? as f64 / GIB))),
            Measure::CpuMs => r.end.and_then(|e| Some((e.at, e.cpu_ns? as f64 / 1e6))),
            Measure::Outcome => r.end.map(|e| (e.at, 1.0)),
        }
    }

    /// The tag a fact of it carries besides its experiment's and its variant's.
    fn tag(self, r: &Reading) -> Option<(&'static str, &'static str)> {
        match self {
            Measure::StartMs => r.first.map(|f| ("start", f.kind)),
            Measure::Outcome => r.end.map(|e| ("outcome", e.outcome.as_str())),
            _ => None,
        }
    }

    /// What `values` say of it: their median, or, of failures, their rate as a percent; of outcomes, nothing.
    fn point(self, values: &[f64]) -> Option<f64> {
        match self {
            Measure::WarmMs | Measure::PeakGib | Measure::StartMs | Measure::SteadyGib | Measure::CpuMs => kiln_ci::stats::median(&mut values.to_vec()),
            Measure::FailedPct => (!values.is_empty()).then(|| values.iter().sum::<f64>() / values.len() as f64),
            Measure::Outcome => None,
        }
    }

    /// [`Measure::point`] within its 95% confidence sequence, `[low, mid, high]`, of `values` in the order they came;
    /// none until both its ends are bounded.
    fn interval(self, values: &[f64]) -> Option<[f64; 3]> {
        let alpha = 1.0 - LEVEL;
        let bounded = |s: Sequence| Some([s.low?, s.mid, s.high?]);
        match self {
            Measure::WarmMs | Measure::PeakGib | Measure::StartMs | Measure::SteadyGib | Measure::CpuMs => bounded(sequence::median(values, alpha)?),
            Measure::FailedPct => {
                let failed: Vec<f64> = values.iter().map(|v| v / 100.0).collect();
                bounded(sequence::rate(&failed, alpha)?).map(|i| i.map(|x| x * 100.0))
            }
            Measure::Outcome => None,
        }
    }
}

/// One fact of a session: its measure, when it was taken (Unix ms), its value, and the tag it carries besides.
type Fact = (Measure, i64, f64, Option<(&'static str, &'static str)>);

/// A session's warm start: when its first start began (Unix ms), and how long it took (ms).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Warm {
    pub at: i64,
    pub ms: f64,
}

/// A session's first start: when it began (Unix ms), how long it took (ms), and its kind ([`kind_of`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct First {
    pub at: i64,
    pub ms: f64,
    pub kind: &'static str,
}

/// How a drawn session ended: as it should (`ended`), in error or never having run (`failed`), or with its start refused
/// (`refused`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Ended,
    Failed,
    Refused,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Ended => "ended",
            Outcome::Failed => "failed",
            Outcome::Refused => "refused",
        }
    }
}

/// How a session ended: when (Unix ms), whether it failed, its microVM's peak memory and its footprint as its last start
/// ended (bytes), and the host CPU its microVMs used (ns, all its starts'), where Hex measured them; and its outcome.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct End {
    pub at: i64,
    pub failed: bool,
    pub peak: Option<u64>,
    pub steady: Option<u64>,
    pub cpu_ns: Option<u64>,
    pub outcome: Outcome,
}

/// What a draw's session came to, as kept with its draw: its warm start, where it had one, and its first start of any
/// kind; its end, once it's over.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Reading {
    pub warm: Option<Warm>,
    pub first: Option<First>,
    pub end: Option<End>,
}

impl Reading {
    /// Session `i` as kiln has it now (`now`, Unix ms: when its end was seen, where it says no time of its own).
    pub fn of(i: &Info, now: i64) -> Reading {
        Reading { warm: warm_of(i), first: first_of(i), end: (i.status == Status::Exited).then(|| end_of(i, now)) }
    }

    /// Session `i` as it goes: over, whatever it last said.
    pub fn over(i: &Info, now: i64) -> Reading {
        Reading { warm: warm_of(i), first: first_of(i), end: Some(end_of(i, now)) }
    }

    /// A start kiln refused at `at`: a failure then.
    pub fn refused(at: i64) -> Reading {
        Reading { end: Some(End { at, failed: true, peak: None, steady: None, cpu_ns: None, outcome: Outcome::Refused }), ..Reading::default() }
    }

    /// `self` with what `now` says that it doesn't yet: a fact, once kept, stays as it was kept.
    pub fn and(self, now: Reading) -> Reading {
        Reading { warm: self.warm.or(now.warm), first: self.first.or(now.first), end: self.end.or(now.end) }
    }

    /// Its facts.
    fn facts(&self) -> Vec<Fact> {
        Measure::ALL.into_iter().filter_map(|m| m.of(self).map(|(at, v)| (m, at, v, m.tag(self)))).collect()
    }
}

/// A start's kind as the Agents page's starts count it (`crate::agentdefs::sessions::start_kind` of Shards' word), a
/// word kiln doesn't know yet `other`: a tag's values stay these few, whatever a microVM says.
fn kind_of(started_from: &str) -> &'static str {
    match crate::agentdefs::sessions::start_kind(started_from).as_str() {
        "cold" => "cold",
        "template" => "template",
        "warm" => "warm",
        "swapped" => "swapped",
        _ => "other",
    }
}

/// Session `i`'s first start Hex measured: when it began, and its kind and time where Shards says them.
fn first_of(i: &Info) -> Option<First> {
    let (at, m) = i.starts.iter().find_map(|s| Some((s.at, s.measured.as_ref()?)))?;
    Some(First { at, ms: m.start_us? as f64 / 1000.0, kind: kind_of(m.started_from.as_deref()?) })
}

/// Session `i`'s warm start: its first start Hex measured, where Shards says it started warm and timed it.
fn warm_of(i: &Info) -> Option<Warm> {
    first_of(i).filter(|f| f.kind == "warm").map(|f| Warm { at: f.at, ms: f.ms })
}

/// How session `i` ended (seen at `now`, where it says no time): in error, or never having run, a failure; its peak the
/// most any of its starts held, its steady footprint its last start's, its host CPU all its starts'.
fn end_of(i: &Info, now: i64) -> End {
    let exit = i.exit.as_ref();
    let at = exit.map(|x| x.at).filter(|at| *at > 0).or_else(|| i.starts.last().and_then(|s| s.ended.as_ref()).map(|e| e.at)).unwrap_or(now);
    let failed = exit.is_some_and(|x| x.error.is_some()) || i.starts.is_empty();
    let measured = || i.starts.iter().filter_map(|s| s.measured.as_ref());
    let cpu = measured().filter_map(|m| m.cpu_ns).reduce(u64::saturating_add);
    End {
        at,
        failed,
        peak: measured().filter_map(|m| m.footprint_peak_bytes).max(),
        steady: measured().filter_map(|m| m.footprint_steady_bytes).last(),
        cpu_ns: cpu,
        outcome: if failed { Outcome::Failed } else { Outcome::Ended },
    }
}

/// A variant's measures, each `[low, mid, high]` within its 95% interval, once its sessions bound it.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Measures {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warm_ms: Option<[f64; 3]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peak_gib: Option<[f64; 3]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failed_pct: Option<[f64; 3]>,
}

/// A variant's trend: each measure's value at the end of each day, oldest first, today's last.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Trend {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warm_ms: Option<Vec<f64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peak_gib: Option<Vec<f64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failed_pct: Option<Vec<f64>>,
}

impl Trend {
    /// Its line of `m`, one of the Agents page's measures (`Measure::SHOWN`).
    fn line(&mut self, m: Measure) -> Option<&mut Option<Vec<f64>>> {
        match m {
            Measure::WarmMs => Some(&mut self.warm_ms),
            Measure::PeakGib => Some(&mut self.peak_gib),
            Measure::FailedPct => Some(&mut self.failed_pct),
            Measure::StartMs | Measure::SteadyGib | Measure::CpuMs | Measure::Outcome => None,
        }
    }
}

/// A criterion's value for a variant, and whether it passes its bound.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Judged {
    pub value: f64,
    pub passes: bool,
}

/// What `readings` (a variant's sessions, in the order they were drawn) measure: each measure once they bound it (a
/// host's sessions have no warm start or peak, and a microVM's no warm start unless a warm host started it); none while
/// they bound none.
pub fn measures(readings: &[Reading]) -> Option<Measures> {
    let interval = |m: Measure| m.interval(&readings.iter().filter_map(|r| Some(m.of(r)?.1)).collect::<Vec<f64>>());
    let m = Measures { warm_ms: interval(Measure::WarmMs), peak_gib: interval(Measure::PeakGib), failed_pct: interval(Measure::FailedPct) };
    if m.warm_ms.is_none() && m.peak_gib.is_none() && m.failed_pct.is_none() { None } else { Some(m) }
}

/// Each variant's trend (`variants`: each one's readings, in the order they were drawn), over the days (UTC) from its
/// experiment's start (`started_at`) to `now`'s, a value taken outside them counted on the nearest: each measure as it
/// stood at the end of each day ([`Measure::point`] of its values up to then), from the first day on which every variant
/// that has the measure had one. A variant without it has none.
pub fn trends(variants: &[Vec<Reading>], started_at: i64, now: i64) -> Vec<Trend> {
    let (first, today) = (started_at.div_euclid(DAY_MS), now.div_euclid(DAY_MS));
    let mut out = vec![Trend::default(); variants.len()];
    if today < first {
        return out;
    }
    for m in Measure::SHOWN {
        // Each variant's values by day, those of one day in the order they were drawn.
        let days: Vec<Vec<(i64, f64)>> = variants
            .iter()
            .map(|rs| {
                let mut v: Vec<(i64, f64)> = rs.iter().filter_map(|r| m.of(r)).map(|(at, x)| (at.div_euclid(DAY_MS).clamp(first, today), x)).collect();
                v.sort_by_key(|(day, _)| *day);
                v
            })
            .collect();
        let Some(from) = days.iter().filter_map(|v| v.first().map(|(day, _)| *day)).max() else { continue };
        for (t, v) in out.iter_mut().zip(&days) {
            if v.is_empty() {
                continue;
            }
            let line: Vec<f64> = (from..=today).filter_map(|d| m.point(&v.iter().take_while(|(day, _)| *day <= d).map(|(_, x)| *x).collect::<Vec<f64>>())).collect();
            if let Some(l) = t.line(m) {
                *l = Some(line);
            }
        }
    }
    out
}

/// What criterion `c`'s query and bound say of each of `variants` (their ids), evaluated as kiln's monitors evaluate a
/// monitor of them as of `now` (`kiln_obs::monitor::evaluate_shared`, no evaluation delay): for each group its query
/// gives whose `variant` tag names one of them, its value and whether it passes. None where it gives none, or can't be
/// evaluated (said in the log, as a monitor's is).
pub fn judge(store: &kiln_obs::lock::RwLock<kiln_obs::store::Store>, c: &Criterion, variants: &[String], now: i64) -> Option<BTreeMap<String, Judged>> {
    let q = match kiln_obs::monitor::parse(&format!("{} {} {}", c.query, c.op.as_str(), c.bound)) {
        Ok(q) => q,
        Err(e) => {
            tracing::debug!(criterion = %c.id, error = %e, "an experiment's criterion isn't one kiln's monitors read");
            return None;
        }
    };
    let values = match kiln_obs::monitor::evaluate_shared(store, &q, now, 0) {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!(criterion = %c.id, error = %e, "an experiment's criterion did not evaluate");
            return None;
        }
    };
    let judged: BTreeMap<String, Judged> = values
        .into_iter()
        .filter_map(|v| {
            let (_, variant) = v.group.iter().find(|(k, _)| k == "variant")?;
            variants.contains(variant).then(|| (variant.clone(), Judged { value: v.value, passes: q.op.passes(v.value, q.threshold) }))
        })
        .collect();
    (!judged.is_empty()).then_some(judged)
}

/// `e`'s variants measured from its draws (`draws`: each one's variant and reading, in the order drawn), as of `now`.
fn fill(e: &mut Experiment, draws: &[Kept], now: i64) {
    let per: Vec<Vec<Reading>> = e.variants.iter().map(|v| draws.iter().filter(|d| d.variant == v.id).map(|d| d.reading).collect()).collect();
    let lines = trends(&per, e.started_at, now);
    for ((v, readings), t) in e.variants.iter_mut().zip(&per).zip(lines) {
        v.measures = measures(readings);
        v.trend = (t != Trend::default()).then_some(t);
    }
}

/// `es` as their operations answer them: each one's draws' sessions read ([`settled`]), its variants measured, and its
/// criteria judged over kiln's observability, where it runs. What can't be read is said in the log, and answered
/// without.
pub async fn measure(hub: &Hub, es: &mut [Experiment]) {
    measure_over(hub, es, crate::observe::api::get().and_then(|o| o.store_replayed())).await;
}

/// [`measure`], its criteria judged over `store` (none: not judged).
pub(super) async fn measure_over(hub: &Hub, es: &mut [Experiment], store: Option<kiln_obs::machine::Shared>) {
    let now = crate::util::now_ms();
    for e in es.iter_mut() {
        match settled(hub, &e.id, now).await {
            Ok(draws) => fill(e, &draws, now),
            Err(err) => tracing::warn!(experiment = %e.id, error = %format!("{err:#}"), "an experiment's sessions weren't read: it's answered without its measures"),
        }
    }
    let Some(store) = store else { return };
    let asked: Vec<(Vec<Criterion>, Vec<String>)> = es.iter().map(|e| (e.criteria.clone(), e.variants.iter().map(|v| v.id.clone()).collect())).collect();
    if asked.iter().all(|(criteria, _)| criteria.is_empty()) {
        return;
    }
    // On the blocking pool, as kiln's monitors evaluate theirs: a query holds the store's lock a part at a time.
    let judged = crate::rt::blocking(move || asked.iter().map(|(criteria, variants)| criteria.iter().map(|c| judge(&store, c, variants, now)).collect::<Vec<_>>()).collect::<Vec<_>>()).await;
    match judged {
        Ok(judged) => {
            for (e, values) in es.iter_mut().zip(judged) {
                for (c, v) in e.criteria.iter_mut().zip(values) {
                    c.values = v;
                }
            }
        }
        Err(err) => tracing::warn!(error = %format!("{err:#}"), "experiments' criteria weren't judged: they're answered without their values"),
    }
}

/// A draw as its measures read it.
#[derive(Debug, Clone)]
struct Kept {
    n: i64,
    variant: String,
    session: Option<String>,
    reading: Reading,
}

const KEPT: &str = "draw, variant_id, session_id, warm_at, warm_ms, ended_at, failed, peak_bytes, first_at, first_ms, first_kind";

/// A draw's row ([`KEPT`]'s columns). Of an end, the facts written as it was kept and not read again (its steady
/// footprint, its CPU, a refusal apart from a failure) are as they were written; read back, none, and an outcome of its
/// `failed` alone.
fn kept(r: &rusqlite::Row<'_>) -> rusqlite::Result<Kept> {
    let (warm_at, warm_ms): (Option<i64>, Option<f64>) = (r.get(3)?, r.get(4)?);
    let (ended_at, failed, peak): (Option<i64>, Option<bool>, Option<i64>) = (r.get(5)?, r.get(6)?, r.get(7)?);
    let (first_at, first_ms, first_kind): (Option<i64>, Option<f64>, Option<String>) = (r.get(8)?, r.get(9)?, r.get(10)?);
    let warm = warm_at.zip(warm_ms).map(|(at, ms)| Warm { at, ms });
    let first = first_at.zip(first_ms).map(|(at, ms)| First { at, ms, kind: kind_of(first_kind.as_deref().unwrap_or("")) });
    let end = ended_at.map(|at| {
        let failed = failed.unwrap_or(false);
        End { at, failed, peak: peak.and_then(|p| u64::try_from(p).ok()), steady: None, cpu_ns: None, outcome: if failed { Outcome::Failed } else { Outcome::Ended } }
    });
    Ok(Kept { n: r.get(0)?, variant: r.get(1)?, session: r.get(2)?, reading: Reading { warm, first, end } })
}

/// Experiment `id`'s draws, in the order drawn, each with what its session came to: one not over yet read again now
/// from kiln's sessions (`now`: when an end is seen that says no time), and what's new kept and written ([`keep`]).
async fn settled(hub: &Hub, id: &str, now: i64) -> anyhow::Result<Vec<Kept>> {
    let experiment = id.to_owned();
    let mut draws: Vec<Kept> = hub
        .store
        .read(move |c| {
            let mut st = c.prepare_cached(&format!("SELECT {KEPT} FROM agent_def_experiment_draws WHERE experiment_id = ?1 ORDER BY draw"))?;
            let rows = st.query_map(params![experiment], kept)?.collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await?;
    let mut changed = Vec::new();
    for d in &mut draws {
        let (Some(session), None) = (&d.session, d.reading.end) else { continue };
        let Some(info) = hub.harness.info(session) else { continue };
        let read = d.reading.and(Reading::of(&info, now));
        if read != d.reading {
            changed.push((d.n, d.variant.clone(), d.reading, read));
            d.reading = read;
        }
    }
    keep(hub, id, changed).await;
    Ok(draws)
}

/// What's new of experiment `experiment`'s draws (each its draw, its variant, what was kept and what's read now) kept,
/// each fact once (one another read kept first is neither kept nor written again), and written ([`write`]).
async fn keep(hub: &Hub, experiment: &str, changed: Vec<(i64, String, Reading, Reading)>) {
    if changed.is_empty() {
        return;
    }
    let e = experiment.to_owned();
    let done = hub
        .store
        .call(move |c| {
            let tx = crate::store::tx(c)?;
            let mut kept: Vec<(String, Vec<Reading>)> = Vec::new();
            for (n, variant, was, read) in &changed {
                let mut new = Vec::new();
                if let (None, Some(w)) = (was.warm, read.warm)
                    && tx.execute("UPDATE agent_def_experiment_draws SET warm_at = ?3, warm_ms = ?4 WHERE experiment_id = ?1 AND draw = ?2 AND warm_at IS NULL", params![e, n, w.at, w.ms])? == 1
                {
                    new.push(Reading { warm: Some(w), ..Reading::default() });
                }
                if let (None, Some(f)) = (was.first, read.first)
                    && tx.execute(
                        "UPDATE agent_def_experiment_draws SET first_at = ?3, first_ms = ?4, first_kind = ?5 WHERE experiment_id = ?1 AND draw = ?2 AND first_at IS NULL",
                        params![e, n, f.at, f.ms, f.kind],
                    )? == 1
                {
                    new.push(Reading { first: Some(f), ..Reading::default() });
                }
                if let (None, Some(end)) = (was.end, read.end)
                    && tx.execute(
                        "UPDATE agent_def_experiment_draws SET ended_at = ?3, failed = ?4, peak_bytes = ?5 WHERE experiment_id = ?1 AND draw = ?2 AND ended_at IS NULL",
                        params![e, n, end.at, end.failed, end.peak.and_then(|p| i64::try_from(p).ok())],
                    )? == 1
                {
                    new.push(Reading { end: Some(end), ..Reading::default() });
                }
                if !new.is_empty() {
                    kept.push((variant.clone(), new));
                }
            }
            tx.commit()?;
            Ok(kept)
        })
        .await;
    match done {
        Ok(kept) => {
            for (variant, readings) in kept {
                write(experiment, &variant, &readings);
            }
        }
        Err(err) => tracing::warn!(%experiment, error = %format!("{err:#}"), "what an experiment's sessions came to wasn't kept"),
    }
}

/// A start of variant `variant` of experiment `experiment` that kiln refused at `at`: its failure, written.
pub(super) fn refused(experiment: &str, variant: &str, at: i64) {
    write(experiment, variant, &[Reading::refused(at)]);
}

/// Session `info` is going (`crate::harness::registry`, as it's closed): where a definition's experiment drew it, what
/// it came to is kept with its draw and written now, as it's over and kiln won't have it to read again.
pub fn closed(hub: &Arc<Hub>, info: &Arc<Info>) {
    if info.experiment.is_none() {
        return;
    }
    let (hub, info) = (Arc::clone(hub), Arc::clone(info));
    drop(crate::rt::spawn(async move { kept_as_closed(&hub, &info).await }));
}

/// [`closed`]'s keeping: session `info`'s draw, where it has one not over yet, kept over as of now.
pub(super) async fn kept_as_closed(hub: &Hub, info: &Info) {
    let session = info.id.clone();
    let found = hub
        .store
        .read(move |c| Ok(c.query_row(&format!("SELECT {KEPT}, experiment_id FROM agent_def_experiment_draws WHERE session_id = ?1"), params![session], |r| Ok((kept(r)?, r.get::<_, String>(11)?))).optional()?))
        .await;
    match found {
        Ok(Some((d, experiment))) if d.reading.end.is_none() => {
            let read = d.reading.and(Reading::over(info, crate::util::now_ms()));
            keep(hub, &experiment, vec![(d.n, d.variant, d.reading, read)]).await;
        }
        Ok(_) => {}
        Err(e) => tracing::warn!(session = %info.id, error = %format!("{e:#}"), "what a closed session of an experiment came to wasn't kept"),
    }
}

/// The facts of `readings`, of variant `variant` of experiment `experiment`, written to kiln's observability, where it
/// runs.
fn write(experiment: &str, variant: &str, readings: &[Reading]) {
    let Some(o) = crate::observe::api::get() else { return };
    let points = points(experiment, variant, readings);
    if !points.is_empty() {
        o.write(points);
    }
}

/// [`write`]'s points: each measure's metric a distribution (`kiln_obs::dist::series_of`), tagged with the experiment
/// and the variant (and a fact's own tag, a start's kind or an outcome), its values at their times (those taken at one
/// time one point of it).
pub(super) fn points(experiment: &str, variant: &str, readings: &[Reading]) -> Vec<(SeriesKey, Vec<(i64, f64)>)> {
    let facts: Vec<Fact> = readings.iter().flat_map(Reading::facts).collect();
    let mut out = Vec::new();
    for m in Measure::ALL {
        let mut by: BTreeMap<Option<(&str, &str)>, BTreeMap<i64, Vec<f64>>> = BTreeMap::new();
        for (_, t, v, tag) in facts.iter().filter(|(fm, ..)| *fm == m) {
            by.entry(*tag).or_default().entry(*t).or_default().push(*v);
        }
        for (tag, at) in by {
            let mut tags = vec![("experiment", experiment), ("variant", variant)];
            tags.extend(tag);
            match kiln_obs::dist::series_of(m.metric(), &tags, &at.into_iter().collect::<Vec<_>>()) {
                Ok(s) => out.extend(s),
                Err(e) => tracing::warn!(metric = m.metric(), error = %e, "an experiment's session metric wasn't written"),
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::model::ExitInfo;
    use crate::harness::session::{Ended, Measured, Runs, Start};

    const DAY: i64 = DAY_MS;

    fn warm(at: i64, ms: f64) -> Reading {
        Reading { warm: Some(Warm { at, ms }), ..Reading::default() }
    }

    /// An end as `end_of` reads one: failed or ended, its peak `peak` bytes, nothing else measured.
    fn end(at: i64, failed: bool, peak: Option<u64>) -> End {
        End { at, failed, peak, steady: None, cpu_ns: None, outcome: if failed { Outcome::Failed } else { Outcome::Ended } }
    }

    fn ended(at: i64, failed: bool, peak_gib: f64) -> Reading {
        Reading { end: Some(end(at, failed, Some((peak_gib * GIB) as u64))), ..Reading::default() }
    }

    /// A microVM start at `at` that Shards says started `from`, timed `us`, its peak `peak` bytes.
    fn start(at: i64, from: &str, us: u64, peak: Option<u64>) -> Start {
        Start { at, runs: Runs::Host { pid: 1 }, ended: None, measured: Some(Measured { start_us: Some(us), started_from: Some(from.into()), footprint_peak_bytes: peak, ..Measured::default() }) }
    }

    /// P7.1 (the Agents page's measures): a session's facts as kiln reads them. Its warm start is its first measured
    /// start's, where Shards says `warm`, in ms; a cold first start gives none. Running, it has no end; ended, its end is
    /// its exit's time, failed where it ended in error or never ran, its peak the most any start held; going while it
    /// runs, it's over then. Once kept, a fact stays: a later reading adds only what was missing.
    #[test]
    fn p7_1_a_sessions_facts_are_read_from_its_starts_and_its_end() {
        let mut i = Info { id: "s".into(), status: Status::Running, starts: vec![start(1_000, "warm", 4_500, Some(2 << 30)), start(9_000, "booted", 300_000, Some(3 << 30))], ..Info::default() };
        let running = Reading::of(&i, 50_000);
        assert_eq!(running, Reading { warm: Some(Warm { at: 1_000, ms: 4.5 }), first: Some(First { at: 1_000, ms: 4.5, kind: "warm" }), end: None });
        assert_eq!(Reading::over(&i, 50_000).end, Some(end(50_000, false, Some(3 << 30))), "going while it runs: over then");
        i.status = Status::Exited;
        i.exit = Some(ExitInfo { at: 40_000, error: Some("claude exited with 1".into()), ..ExitInfo::default() });
        assert_eq!(Reading::of(&i, 50_000).end, Some(end(40_000, true, Some(3 << 30))));
        i.exit = Some(ExitInfo::default());
        if let Some(s) = i.starts.last_mut() {
            s.ended = Some(Ended { at: 30_000, code: Some(0) });
        }
        assert_eq!(Reading::of(&i, 50_000).end, Some(end(30_000, false, Some(3 << 30))), "no time of its exit's: its last start's end");
        let cold = Info { id: "c".into(), status: Status::Exited, starts: vec![start(1_000, "booted", 250_000, None)], ..Info::default() };
        assert_eq!(
            Reading::of(&cold, 5_000),
            Reading { warm: None, first: Some(First { at: 1_000, ms: 250.0, kind: "cold" }), end: Some(end(5_000, false, None)) },
            "a cold first start is no warm start, and is its first"
        );
        let never = Info { id: "n".into(), status: Status::Exited, ..Info::default() };
        assert!(Reading::of(&never, 5_000).end.is_some_and(|e| e.failed), "one that never ran failed");
        let kept = running.and(Reading { warm: Some(Warm { at: 7, ms: 1.0 }), first: None, end: Some(end(60_000, false, None)) });
        assert_eq!(kept, Reading { warm: Some(Warm { at: 1_000, ms: 4.5 }), first: running.first, end: Some(end(60_000, false, None)) }, "a kept fact stays");
    }

    /// P7.1: a variant's measures are its 95% confidence sequences, `[low, mid, high]`: the median of its warm starts and
    /// of its peaks (GiB), and its failures' rate as a percent, the middle the point and the ends around it. Each is given
    /// once its sessions bound it: too few warm starts leave the median's sequence open and that measure out, the others
    /// shown (P7.2); none while none is bounded.
    #[test]
    fn p7_1_a_variants_measures_are_bounded_intervals_of_its_sessions() -> anyhow::Result<()> {
        let mut rs = Vec::new();
        for k in 0..200i64 {
            let ms = 4.0 + (k % 7) as f64;
            rs.push(warm(k * 1_000, ms).and(ended(k * 1_000 + 500, k % 10 == 0, 2.0 + (k % 3) as f64 * 0.25)));
        }
        let m = measures(&rs).ok_or_else(|| anyhow::anyhow!("200 sessions bound all three"))?;
        let (Some(w), Some(p), Some(f)) = (m.warm_ms, m.peak_gib, m.failed_pct) else { anyhow::bail!("200 sessions bound all three: {m:?}") };
        let ([_, warm_mid, _], [_, peak_mid, _], [failed_low, failed_mid, failed_high]) = (w, p, f);
        anyhow::ensure!(warm_mid == 7.0 && peak_mid == 2.25 && (failed_mid - 10.0).abs() < 1e-9, "{m:?}");
        for [low, mid, high] in [w, p, f] {
            anyhow::ensure!(low <= mid && mid <= high, "{m:?}");
        }
        anyhow::ensure!(failed_low >= 0.0 && failed_high <= 100.0, "{m:?}");
        let two = measures(rs.get(..2).unwrap_or_default()).ok_or_else(|| anyhow::anyhow!("two sessions' failures are bounded"))?;
        anyhow::ensure!(two.warm_ms.is_none() && two.peak_gib.is_none() && two.failed_pct.is_some(), "two sessions leave each median unbounded: {two:?}");
        // No warm start (a host's sessions, or microVMs no warm host started): the rest shown as they were.
        let no_warm: Vec<Reading> = rs.iter().map(|r| Reading { warm: None, ..*r }).collect();
        let rest = measures(&no_warm).ok_or_else(|| anyhow::anyhow!("its peaks and failures"))?;
        anyhow::ensure!(rest == Measures { warm_ms: None, ..m }, "a measure none has left out, the rest shown: {rest:?}");
        anyhow::ensure!(measures(&[]).is_none(), "no session: none");
        Ok(())
    }

    /// P7.1: each variant's trend is each measure as it stood at the end of each day (UTC), today's last, its last value
    /// the measure's middle now; every variant's line starts on the first day all that have the measure had one, so
    /// lines cover the same days; a value from before the experiment's start counts on its first day; a variant
    /// without the measure has no line.
    #[test]
    fn p7_1_a_trend_is_each_day_as_it_stood_from_the_day_all_had_one() {
        let start = 100 * DAY;
        let now = 103 * DAY + 5;
        let a = vec![warm(start - 3 * DAY, 10.0), warm(start + 10, 20.0), warm(start + DAY + 10, 30.0), warm(start + 3 * DAY + 1, 40.0)];
        let b = vec![warm(start + DAY + 20, 5.0), warm(start + 2 * DAY + 20, 7.0)];
        let c = vec![ended(start + 2 * DAY, true, 1.0), ended(start + 2 * DAY + 1, false, 1.0)];
        let t = trends(&[a, b, c], start, now);
        let line = |k: usize| t.get(k).and_then(|x| x.warm_ms.clone());
        // `a` and `b` both have warm starts from day 101: days 101 to 103.
        assert_eq!(line(0), Some(vec![20.0, 20.0, 25.0]), "{t:?}");
        assert_eq!(line(1), Some(vec![5.0, 6.0, 6.0]), "{t:?}");
        assert_eq!(line(2), None, "no warm start: no line");
        assert_eq!(t.get(2).and_then(|x| x.failed_pct.clone()), Some(vec![50.0, 50.0]), "{t:?}");
        assert_eq!(trends(&[vec![warm(start, 1.0)]], now, start).first(), Some(&Trend::default()), "an experiment from after now: none");
    }

    /// P7.1: a session's facts are points of their measures' distributions, tagged with the experiment and the variant:
    /// a fact a measure (a failure 100, a success 0), facts taken at one time one point of it.
    #[test]
    fn p7_1_facts_are_distribution_points_tagged_by_experiment_and_variant() -> anyhow::Result<()> {
        let points = points("e1", "v1", &[Reading::refused(5_000), warm(1_000, 4.0).and(ended(5_000, false, 2.0))]);
        let mut read = Vec::new();
        for (k, p) in &points {
            let key = kiln_obs::batch::parse_key(&k.key).map_err(|e| anyhow::anyhow!("{e}"))?;
            anyhow::ensure!(key.tags.contains(&("experiment", "e1")) && key.tags.contains(&("variant", "v1")), "{:?}", key.tags);
            read.push((key.metric.to_owned(), p.clone()));
        }
        let metric = |name: &str| read.iter().filter(|(m, _)| m == name).flat_map(|(_, p)| p.clone()).collect::<Vec<_>>();
        anyhow::ensure!(metric("kiln.session.failed_pct.count") == vec![(5_000, 2.0)], "two at one time: one point of two: {read:?}");
        anyhow::ensure!(metric("kiln.session.failed_pct.sum") == vec![(5_000, 100.0)], "{read:?}");
        anyhow::ensure!(metric("kiln.session.warm_ms.sum") == vec![(1_000, 4.0)], "{read:?}");
        anyhow::ensure!(metric("kiln.session.peak_memory_gib.max") == vec![(5_000, 2.0)], "{read:?}");
        Ok(())
    }

    /// P7.1's deployment measures beside the Agents page's three ("start cold/template/warm, footprint peak and steady,
    /// host CPU, failures, session outcome"): a session's first start of any kind, tagged with its kind; its steady
    /// footprint (its last start's); the host CPU of all its starts; its outcome, tagged; each a point of its
    /// distribution, so a criterion reads them by variant (`p50:kiln.session.start_ms{experiment:e,start:cold} by
    /// {variant}`, `count:kiln.session.outcome{experiment:e,outcome:refused} by {variant}`).
    #[test]
    fn p7_1_deployment_measures_are_points_tagged_by_kind_and_outcome() -> anyhow::Result<()> {
        let mut first = start(1_000, "restored", 80_000, Some(3 << 30));
        let mut second = start(9_000, "booted", 300_000, Some(2 << 30));
        for (s, steady, cpu) in [(&mut first, 1u64 << 30, 2_000_000_000u64), (&mut second, 1 << 29, 500_000_000)] {
            if let Some(m) = s.measured.as_mut() {
                m.footprint_steady_bytes = Some(steady);
                m.cpu_ns = Some(cpu);
            }
        }
        let i = Info { id: "s".into(), status: Status::Exited, starts: vec![first, second], exit: Some(ExitInfo { at: 20_000, ..ExitInfo::default() }), ..Info::default() };
        let r = Reading::of(&i, 30_000);
        anyhow::ensure!(r.first == Some(First { at: 1_000, ms: 80.0, kind: "template" }) && r.warm.is_none(), "{r:?}");
        let e = r.end.ok_or_else(|| anyhow::anyhow!("it ended"))?;
        anyhow::ensure!(e.steady == Some(1 << 29) && e.cpu_ns == Some(2_500_000_000) && e.peak == Some(3 << 30) && e.outcome == Outcome::Ended, "{e:?}");
        let mut read = Vec::new();
        for (k, p) in &points("e", "v", &[r, Reading::refused(40_000)]) {
            let key = kiln_obs::batch::parse_key(&k.key).map_err(|e| anyhow::anyhow!("{e}"))?;
            let tag = |name: &str| key.tags.iter().find(|(t, _)| *t == name).map(|(_, v)| (*v).to_owned());
            read.push((key.metric.to_owned(), tag("start"), tag("outcome"), p.clone()));
        }
        let of = |metric: &str| read.iter().filter(|(m, ..)| m == metric).cloned().collect::<Vec<_>>();
        anyhow::ensure!(of("kiln.session.start_ms.sum").iter().any(|(_, kind, _, p)| kind.as_deref() == Some("template") && *p == vec![(1_000, 80.0)]), "{read:?}");
        anyhow::ensure!(of("kiln.session.steady_memory_gib.max").iter().any(|(.., p)| *p == vec![(20_000, 0.5)]), "{read:?}");
        anyhow::ensure!(of("kiln.session.cpu_ms.sum").iter().any(|(.., p)| *p == vec![(20_000, 2_500.0)]), "{read:?}");
        let outcomes: Vec<(Option<String>, Vec<(i64, f64)>)> = of("kiln.session.outcome.count").into_iter().map(|(_, _, o, p)| (o, p)).collect();
        anyhow::ensure!(outcomes.contains(&(Some("ended".into()), vec![(20_000, 1.0)])) && outcomes.contains(&(Some("refused".into()), vec![(40_000, 1.0)])), "{outcomes:?}");
        Ok(())
    }

    /// P7.1 (criteria in kiln's monitoring syntax, "in full"): a criterion's query and bound are evaluated as a monitor of
    /// them is, over kiln's observability: `avg(last_7d):avg:kiln.session.warm_ms{experiment:e} by {variant}` gives each
    /// variant its mean, and `<= 6` passes the faster; a formula of a session metric and another of kiln's metrics
    /// evaluates too; a query that isn't by variant, or names no variant of the experiment, gives none.
    #[test]
    fn p7_1_a_criterion_is_evaluated_per_variant_as_a_monitor_is() -> anyhow::Result<()> {
        use kiln_obs::batch::{Builder, Kind};
        let now = 20 * DAY;
        let mut store = kiln_obs::store::Store::new(kiln_obs::hash::Keyed::with_keys(1, 2));
        let mut b = Builder::new();
        for (variant, ms) in [("fast", 4.0), ("slow", 9.0)] {
            let readings: Vec<Reading> = (1..=6).map(|k| warm(now - k * DAY / 2, ms)).collect();
            for (key, pts) in points("e", variant, &readings) {
                b.define(&key);
                b.run(key.fp, pts);
            }
        }
        let scale = SeriesKey::new("kiln.scale", [("host", "a")], Kind::Gauge).map_err(|e| anyhow::anyhow!("{e}"))?;
        b.define(&scale);
        b.run(scale.fp, (1..=6).map(|k| (now - k * DAY / 2, 2.0)));
        store.apply(&b.finish()).map_err(|e| anyhow::anyhow!("{e}"))?;
        let store = kiln_obs::lock::RwLock::new(store);
        let variants = ["fast".to_owned(), "slow".to_owned()];
        let crit = |query: &str, op: super::super::Comparison, bound: f64| Criterion { id: "c".into(), name: "Warm".into(), query: query.into(), op, bound, values: None };
        let warm = crit("avg(last_7d):avg:kiln.session.warm_ms{experiment:e} by {variant}", super::super::Comparison::AtMost, 6.0);
        let got = judge(&store, &warm, &variants, now).ok_or_else(|| anyhow::anyhow!("no values"))?;
        anyhow::ensure!(got.get("fast") == Some(&Judged { value: 4.0, passes: true }) && got.get("slow") == Some(&Judged { value: 9.0, passes: false }), "{got:?}");
        let formula = crit("avg(last_7d):avg:kiln.session.warm_ms{experiment:e} by {variant} / avg:kiln.scale{*}", super::super::Comparison::Below, 3.0);
        let got = judge(&store, &formula, &variants, now).ok_or_else(|| anyhow::anyhow!("no formula values"))?;
        anyhow::ensure!(got.get("fast").is_some_and(|j| (j.value - 2.0).abs() < 1e-9 && j.passes) && got.get("slow").is_some_and(|j| !j.passes), "{got:?}");
        anyhow::ensure!(judge(&store, &crit("avg(last_7d):avg:kiln.session.warm_ms{experiment:e}", super::super::Comparison::Below, 6.0), &variants, now).is_none(), "not by variant: none");
        anyhow::ensure!(judge(&store, &warm, &["other".to_owned()], now).is_none(), "no variant of its: none");
        Ok(())
    }
}
