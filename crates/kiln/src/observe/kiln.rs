//! kiln's own signals in observe (`docs/observe.md` §8): CI's jobs and runs as they finish (metrics
//! and events), and the server's registry (`crate::metrics`, what `/metrics` serves Prometheus) every
//! [`super::host::EVERY`], as Prometheus would scrape it.
//!
//! - `kiln.ci.job.duration` and `kiln.ci.job.queue_time` (seconds), `kiln.ci.job.peak_rss_bytes`
//!   and `kiln.ci.job.cpu_seconds`, and `kiln.ci.jobs.finished` (a count, one a job), each tagged
//!   with its `repo`, `pipeline`, `job`, `status`, `runner`, `executor`, `branch` and `event`, so
//!   `p95:kiln.ci.job.duration{repo:kiln} by {job}` and
//!   `sum:kiln.ci.jobs.finished{status:failed}.as_count()` answer what they say;
//!   `kiln.ci.pipeline.duration` and `kiln.ci.pipelines.finished` the same of whole runs.
//! - Each is an event too: a record of source `kiln.event` and service `kiln.ci`, its attributes the
//!   run's (number, sha, author) besides the tags, an error's severity when it failed, so the event
//!   stream and log monitors see it (`source:kiln.event @event.type:ci.job.finished status:error`).
//! - Each run is a trace too (§8.7, Datadog's CI visibility's model): its root span the run (service
//!   `kiln.ci`, its name the pipeline's, a server's span so the trace metrics read each pipeline's
//!   runs, failures and durations), each job a span under it, each step a span under its job; the
//!   ids derived from the run's and the job's (SHA-256 of them), so the spans written as each job
//!   ends and the run's as it ends are one trace, and the same if written twice.
//! - kiln CD's deploys (`kiln.cd.deploys` by app, trigger and result; objects applied and pruned)
//!   and every change of an app's health or sync, as events (service `kiln.cd`); its apps counted
//!   by health and sync each interval (`kiln.cd.apps`), a pair counted once written from then on, 0
//!   when no app is in it, as a Prometheus gauge's labels stay (a monitor on the degraded apps sees
//!   them leave).
//! - The registry's families keep their Prometheus names (`kiln_jobs_queued`, ...), but for the
//!   runners' host samples and the process's figures, which observe reports itself. Every runner's
//!   own registry is read as the server's is, each tagged with its host; a histogram answers
//!   percentiles from its `_bucket`s (`p95:kiln_…_seconds{*}`), as a scrape's does.

use std::time::{Duration, Instant};

use kiln_obs::batch::{Kind, SeriesKey};
use kiln_obs::logs::record::Record;

use super::Observe;
use crate::hub::Hub;
use crate::store::{Job, Run, Status};

fn point(out: &mut Vec<(SeriesKey, Vec<(i64, f64)>)>, metric: &str, kind: Kind, tags: &[(&str, &str)], ts: i64, v: f64) {
    if v.is_finite()
        && let Ok(k) = SeriesKey::new(metric, tags.iter().copied(), kind)
    {
        out.push((k, vec![(ts, v)]));
    }
}

fn failed(s: Status) -> bool {
    matches!(s, Status::Failed)
}

/// An id derived from `what` and `key` (its first bytes of their SHA-256): the same wherever and
/// whenever made; never all zero (OTLP's invalid id) but with a chance of 2⁻⁶⁴ that is taken as it is.
fn derived<const N: usize>(what: &str, key: &str) -> [u8; N] {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(what.as_bytes());
    h.update([0]);
    h.update(key.as_bytes());
    let d = h.finalize();
    let mut out = [0u8; N];
    for (o, b) in out.iter_mut().zip(d.iter()) {
        *o = *b;
    }
    out
}

/// A CI run's trace id, and its root span's id.
pub fn run_trace(run_id: &str) -> ([u8; 16], [u8; 8]) {
    (derived("kiln.ci.run.trace", run_id), derived("kiln.ci.run.span", run_id))
}

fn ns(s: i64) -> i64 {
    s.saturating_mul(1_000_000_000)
}

/// A span's failure: a step, a job or a run that failed or ran out of time.
fn erred(s: Status) -> bool {
    matches!(s, Status::Failed | Status::TimedOut)
}

#[allow(clippy::too_many_arguments)]
fn span(trace: [u8; 16], id: [u8; 8], parent: Option<[u8; 8]>, name: &str, kind: &str, start_s: i64, end_s: i64, host: &str, failed: bool, mut attrs: Vec<(String, String)>) -> Record {
    let mut own = vec![("span.kind".to_owned(), kind.to_owned()), ("duration".to_owned(), ns((end_s - start_s).max(0)).to_string())];
    if let Some(p) = parent {
        own.push(("parent_id".to_owned(), hex::encode(p)));
    }
    if failed {
        own.push(("otel.status_code".to_owned(), "ERROR".to_owned()));
    }
    own.append(&mut attrs);
    Record { ts: ns(start_s), severity: if failed { 17 } else { 9 }, service: "kiln.ci".into(), host: host.to_owned(), source: "kiln.ci".into(), message: name.to_owned(), attrs: own, trace_id: Some(trace), span_id: Some(id) }
}

/// A job's spans: itself under its run's root, and its steps under it (those that started).
pub fn job_spans(job: &Job, run: Option<&Run>, status: Status, finished: i64) -> Vec<Record> {
    let (trace, root) = run_trace(&job.run_id);
    let id = derived::<8>("kiln.ci.job.span", &job.id);
    let start = job.started_at.or(job.queued_at).unwrap_or(finished);
    let runner = job.agent.as_deref().unwrap_or("");
    let mut attrs: Vec<(String, String)> = vec![("ci.job.name".into(), job.display.clone()), ("ci.job.id".into(), job.id.clone()), ("ci.status".into(), status.as_str().to_owned()), ("ci.job.attempt".into(), job.attempt.to_string())];
    for (k, v) in [("runner", runner), ("executor", job.executor.as_deref().unwrap_or(""))] {
        if !v.is_empty() {
            attrs.push((k.into(), v.into()));
        }
    }
    if let (Some(q), Some(s)) = (job.queued_at, job.started_at) {
        attrs.push(("ci.job.queue_s".into(), (s - q).max(0).to_string()));
    }
    if let Some(r) = run {
        attrs.push(("ci.pipeline.name".into(), r.pipeline.clone()));
        attrs.push(("git.repository".into(), r.repo_name.clone()));
    }
    if let Some(c) = job.exit_code {
        attrs.push(("exit_code".into(), c.to_string()));
    }
    if let Some(e) = &job.error {
        attrs.push(("error.message".into(), e.clone()));
    }
    let mut out = vec![span(trace, id, Some(root), &job.key, "internal", start, finished.max(start), runner, erred(status), attrs)];
    for st in &job.steps {
        let Some(s0) = st.started_at else { continue };
        let s1 = st.finished_at.unwrap_or(finished).max(s0);
        let mut attrs = vec![("ci.step.index".to_owned(), st.idx.to_string()), ("ci.status".to_owned(), st.status.as_str().to_owned())];
        if let Some(c) = st.exit_code {
            attrs.push(("exit_code".into(), c.to_string()));
        }
        out.push(span(trace, derived("kiln.ci.step.span", &format!("{}\0{}", job.id, st.idx)), Some(id), &st.name, "internal", s0, s1, runner, erred(st.status), attrs));
    }
    out
}

/// A run's root span: the pipeline's run, from its start to its end.
pub fn run_span(run: &Run, status: Status, finished: i64) -> Record {
    let (trace, root) = run_trace(&run.id);
    let start = run.started_at.unwrap_or(run.created_at);
    let attrs = vec![
        ("ci.provider.name".into(), "kiln".into()),
        ("ci.pipeline.name".into(), run.pipeline.clone()),
        ("ci.pipeline.number".into(), run.number.to_string()),
        ("ci.pipeline.id".into(), run.id.clone()),
        ("ci.status".into(), status.as_str().to_owned()),
        ("ci.event".into(), run.event.clone()),
        ("git.repository".into(), run.repo_name.clone()),
        ("git.branch".into(), run.branch.clone()),
        ("git.commit.sha".into(), run.sha.clone()),
        ("git.commit.author".into(), run.author.clone()),
    ];
    let mut r = span(trace, root, None, &run.pipeline, "server", start, finished.max(start), "", erred(status), attrs);
    if let Some(e) = &run.error {
        r.attrs.push(("error.message".into(), e.clone()));
    }
    r
}

/// A job (a leg) that finished with `status` at `finished` (unix s): its points and its event.
pub fn job_finished(job: &Job, run: Option<&Run>, status: Status, finished: i64) {
    let Some(o) = super::api::get() else { return };
    let ts = finished.saturating_mul(1000);
    let (repo, pipeline, branch, event) = run.map_or(("", "", "", ""), |r| (r.repo_name.as_str(), r.pipeline.as_str(), r.branch.as_str(), r.event.as_str()));
    let runner = job.agent.as_deref().unwrap_or("");
    let executor = job.executor.as_deref().unwrap_or("");
    let tags = [("repo", repo), ("pipeline", pipeline), ("job", job.key.as_str()), ("status", status.as_str()), ("runner", runner), ("executor", executor), ("branch", branch), ("event", event)];
    let mut out = Vec::new();
    let duration = job.started_at.map(|s| (finished - s).max(0) as f64);
    let queued = job.queued_at.zip(job.started_at).map(|(q, s)| (s - q).max(0) as f64);
    if let Some(d) = duration {
        point(&mut out, "kiln.ci.job.duration", Kind::Gauge, &tags, ts, d);
    }
    if let Some(q) = queued {
        point(&mut out, "kiln.ci.job.queue_time", Kind::Gauge, &tags, ts, q);
    }
    if let Some(r) = job.peak_rss {
        point(&mut out, "kiln.ci.job.peak_rss_bytes", Kind::Gauge, &tags, ts, r as f64);
    }
    if let Some(c) = job.cpu_seconds {
        point(&mut out, "kiln.ci.job.cpu_seconds", Kind::Gauge, &tags, ts, c);
    }
    point(&mut out, "kiln.ci.jobs.finished", Kind::Count, &tags, ts, 1.0);
    o.write(out);
    let mut attrs: Vec<(String, String)> = tags.iter().filter(|(_, v)| !v.is_empty()).map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect();
    attrs.push(("event.type".into(), "ci.job.finished".into()));
    attrs.push(("job.id".into(), job.id.clone()));
    if let Some(d) = duration {
        attrs.push(("duration_s".into(), format!("{d}")));
    }
    if let Some(q) = queued {
        attrs.push(("queue_s".into(), format!("{q}")));
    }
    if let Some(r) = run {
        attrs.push(("run.number".into(), r.number.to_string()));
        attrs.push(("git.sha".into(), r.sha.clone()));
        attrs.push(("git.author".into(), r.author.clone()));
    }
    if let Some(e) = &job.error {
        attrs.push(("error".into(), e.clone()));
    }
    let message = format!("{} {} of {repo} #{} on {}{}", job.display, status.as_str(), run.map_or(0, |r| r.number), if runner.is_empty() { "—" } else { runner }, duration.map_or_else(String::new, |d| format!(" in {d} s")));
    // The event carries the run's trace: its job's span a click away.
    let (trace, _) = run_trace(&job.run_id);
    o.log(vec![Record { ts: ts.saturating_mul(1_000_000), severity: if failed(status) { 17 } else { 9 }, service: "kiln.ci".into(), host: runner.to_owned(), source: "kiln.event".into(), message, attrs, trace_id: Some(trace), span_id: Some(derived("kiln.ci.job.span", &job.id)) }]);
    o.spans(&job_spans(job, run, status, finished));
}

/// A run that finished: its points and its event.
pub fn run_finished(run: &Run, status: Status, finished: i64) {
    let Some(o) = super::api::get() else { return };
    let ts = finished.saturating_mul(1000);
    let tags = [("repo", run.repo_name.as_str()), ("pipeline", run.pipeline.as_str()), ("status", status.as_str()), ("branch", run.branch.as_str()), ("event", run.event.as_str())];
    let mut out = Vec::new();
    let duration = run.started_at.map(|s| (finished - s).max(0) as f64);
    if let Some(d) = duration {
        point(&mut out, "kiln.ci.pipeline.duration", Kind::Gauge, &tags, ts, d);
    }
    point(&mut out, "kiln.ci.pipelines.finished", Kind::Count, &tags, ts, 1.0);
    o.write(out);
    let mut attrs: Vec<(String, String)> = tags.iter().filter(|(_, v)| !v.is_empty()).map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect();
    attrs.extend([("event.type".into(), "ci.pipeline.finished".into()), ("run.id".into(), run.id.clone()), ("run.number".into(), run.number.to_string()), ("git.sha".into(), run.sha.clone()), ("git.author".into(), run.author.clone())]);
    if let Some(d) = duration {
        attrs.push(("duration_s".into(), format!("{d}")));
    }
    let message = format!("{} #{} of {} {}{}", run.pipeline, run.number, run.repo_name, status.as_str(), duration.map_or_else(String::new, |d| format!(" in {d} s")));
    let (trace, root) = run_trace(&run.id);
    o.log(vec![Record { ts: ts.saturating_mul(1_000_000), severity: if failed(status) { 17 } else { 9 }, service: "kiln.ci".into(), host: String::new(), source: "kiln.event".into(), message, attrs, trace_id: Some(trace), span_id: Some(root) }]);
    o.spans(&[run_span(run, status, finished)]);
}

/// A CI leg done for good (`crate::ci::runs::finish_leg`): read back and written.
pub async fn leg_finished(hub: &Hub, leg: &str) {
    if super::api::get().is_none() {
        return;
    }
    let Ok(Some(job)) = hub.store.job(leg.to_owned()).await else { return };
    let run = hub.store.run(job.run_id.clone()).await.ok().flatten();
    job_finished(&job, run.as_ref(), job.status, job.finished_at.unwrap_or_else(crate::util::now_unix));
}

/// A CI run's end (`crate::ci::runs::finish_run`): read back and written.
pub async fn run_ended(hub: &Hub, run: &str) {
    if super::api::get().is_none() {
        return;
    }
    let Ok(Some(r)) = hub.store.run(run.to_owned()).await else { return };
    run_finished(&r, r.status, r.finished_at.unwrap_or_else(crate::util::now_unix));
}

/// A deployment kiln CD recorded: `kiln.cd.deploys` (a count, one a deploy) tagged with its `app`,
/// `trigger` and `result`, the objects it applied and pruned, and an event (service `kiln.cd`).
pub fn deployed(d: &kiln_cd::state::Deployment) {
    let Some(o) = super::api::get() else { return };
    let ts = i64::try_from(d.at_ms).unwrap_or(i64::MAX);
    let result = if d.succeeded { "succeeded" } else { "failed" };
    let tags = [("app", d.app.as_str()), ("trigger", d.trigger.as_str()), ("result", result)];
    let mut out = Vec::new();
    point(&mut out, "kiln.cd.deploys", Kind::Count, &tags, ts, 1.0);
    point(&mut out, "kiln.cd.deploy.applied", Kind::Gauge, &tags, ts, f64::from(d.applied));
    point(&mut out, "kiln.cd.deploy.pruned", Kind::Gauge, &tags, ts, f64::from(d.pruned));
    o.write(out);
    let mut attrs: Vec<(String, String)> = tags.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect();
    attrs.extend([("event.type".into(), "cd.deploy".into()), ("op".into(), d.op.to_string()), ("applied".into(), d.applied.to_string()), ("pruned".into(), d.pruned.to_string())]);
    if let Some(r) = &d.revision {
        attrs.push(("revision".into(), r.clone()));
    }
    if let Some(by) = &d.by {
        attrs.push(("by".into(), by.clone()));
    }
    let message = format!("deploy of {} {}{}: {}", d.app, result, d.revision.as_deref().map_or_else(String::new, |r| format!(" at {}", r.get(..12).unwrap_or(r))), d.message);
    o.log(vec![Record { ts: ts.saturating_mul(1_000_000), severity: if d.succeeded { 9 } else { 17 }, service: "kiln.cd".into(), host: String::new(), source: "kiln.event".into(), message, attrs, trace_id: None, span_id: None }]);
}

/// An app's status changed: when its health or its sync did, an event (warn when it turned
/// unhealthy, info when it came back).
pub fn cd_status(before: Option<&kiln_cd::engine::Summary>, now: &kiln_cd::engine::Summary) {
    let Some(o) = super::api::get() else { return };
    let (health, sync) = (now.health.as_str(), now.sync.as_str());
    let (was_health, was_sync) = before.map_or(("", ""), |b| (b.health.as_str(), b.sync.as_str()));
    if (health, sync) == (was_health, was_sync) {
        return;
    }
    let ts = crate::util::now_ms();
    let app = now.app.to_string();
    let attrs: Vec<(String, String)> = vec![
        ("event.type".into(), "cd.status".into()),
        ("app".into(), app.clone()),
        ("health".into(), health.to_owned()),
        ("sync".into(), sync.to_owned()),
        ("health.before".into(), was_health.to_owned()),
        ("sync.before".into(), was_sync.to_owned()),
    ];
    // kiln CD's health (Argo CD's): Degraded, Missing and Unknown are trouble; Healthy, Progressing
    // and Suspended (paused, by intent) are not.
    let bad = matches!(health, "Degraded" | "Missing" | "Unknown");
    let message = format!("{app} is {health} and {sync}{}{}", if was_health.is_empty() { String::new() } else { format!(" (was {was_health} and {was_sync})") }, now.message.as_deref().map_or_else(String::new, |m| format!(": {m}")));
    o.log(vec![Record { ts: ts.saturating_mul(1_000_000), severity: if bad { 13 } else { 9 }, service: "kiln.cd".into(), host: String::new(), source: "kiln.event".into(), message, attrs, trace_id: None, span_id: None }]);
}

/// Apps (each its health and sync) counted by health and sync into `counts`: a pair counted before
/// kept, 0 when no app is in it now.
fn count_apps(counts: &mut std::collections::BTreeMap<(&'static str, &'static str), u64>, apps: impl Iterator<Item = (kiln_cd::model::Health, kiln_cd::model::Sync)>) {
    counts.values_mut().for_each(|n| *n = 0);
    for (health, sync) in apps {
        *counts.entry((health.as_str(), sync.as_str())).or_default() += 1;
    }
}

/// How long the registry waits for the QUIC endpoint's sample: its thread answers on its next turn,
/// a turn some microseconds unless a flood of frames is ahead of it.
const QUIC_SAMPLE_WAIT: Duration = Duration::from_millis(500);

/// What a node's QUIC endpoint measures ([`crate::net::QuicSample`]), as metrics under `kiln.quic`,
/// tagged with the node (`host`) and, a connection's, its peer (`peer`: a runner's name on the
/// server, `server` on a runner). Each connection's path now, as gauges (seconds and bytes; rates
/// in bytes a second): its smoothed and least round trips (RFC 9002 §5), its probe timeout, its
/// congestion window, slow-start threshold and pacing rate where the controller keeps them, its
/// delivery rate, receive window and largest datagram. What the connection counted since it
/// began, as counters (a reconnection's reset found at query time): packets sent and lost, bytes
/// lost and acknowledged, congestion events, black holes, UDP datagrams and bytes each way, STREAM
/// frames each way, RESET_STREAM frames each way, and frames that say this side or the peer was
/// held by flow control (DATA_BLOCKED, STREAM_DATA_BLOCKED, STREAMS_BLOCKED). The endpoint's own:
/// connections and exchanges open (gauges), streams refused, lane frames dropped, responses dropped
/// (counters). Names follow qlog's recovery metrics (`draft-ietf-quic-qlog-quic-events`,
/// `recovery:metrics_updated`: `smoothed_rtt`, `min_rtt`, `congestion_window`, `ssthresh`,
/// `pacing_rate`).
pub fn quic_points(out: &mut Vec<(SeriesKey, Vec<(i64, f64)>)>, host: &str, s: &crate::net::QuicSample, ts: i64) {
    let own = [("host", host)];
    point(out, "kiln.quic.connections", Kind::Gauge, &own, ts, s.connections as f64);
    point(out, "kiln.quic.exchanges", Kind::Gauge, &own, ts, s.exchanges as f64);
    point(out, "kiln.quic.streams_refused", Kind::Counter, &own, ts, s.streams_refused as f64);
    point(out, "kiln.quic.frames_dropped", Kind::Counter, &own, ts, s.frames_dropped as f64);
    point(out, "kiln.quic.responses_dropped", Kind::Counter, &own, ts, s.responses_dropped as f64);
    for p in &s.peers {
        let tags = [("host", host), ("peer", p.name.as_str())];
        let (f, c) = (&p.path, &p.conn);
        for (metric, v) in [
            ("kiln.quic.smoothed_rtt", f.rtt.as_secs_f64()),
            ("kiln.quic.min_rtt", f.min_rtt.as_secs_f64()),
            ("kiln.quic.pto", f.probe_timeout.as_secs_f64()),
            ("kiln.quic.congestion_window", f.cwnd as f64),
            ("kiln.quic.delivery_rate", f.delivery_rate as f64),
            ("kiln.quic.receive_window", f.receive_window as f64),
            ("kiln.quic.max_datagram", f64::from(f.max_datagram)),
        ] {
            point(out, metric, Kind::Gauge, &tags, ts, v);
        }
        for (metric, v) in [("kiln.quic.ssthresh", f.ssthresh), ("kiln.quic.pacing_rate", f.pacing_rate)] {
            if let Some(v) = v {
                point(out, metric, Kind::Gauge, &tags, ts, v as f64);
            }
        }
        let blocked = |x: &hyper_quic::FrameStats| x.data_blocked + x.stream_data_blocked + x.streams_blocked_bidi + x.streams_blocked_uni;
        for (metric, v) in [
            ("kiln.quic.packets_sent", c.path.sent_packets),
            ("kiln.quic.packets_lost", c.path.lost_packets),
            ("kiln.quic.bytes_lost", c.path.lost_bytes),
            ("kiln.quic.bytes_acked", c.path.acked_bytes),
            ("kiln.quic.congestion_events", c.path.congestion_events),
            ("kiln.quic.black_holes", c.path.black_holes_detected),
            ("kiln.quic.datagrams_sent", c.udp_tx.datagrams),
            ("kiln.quic.datagrams_received", c.udp_rx.datagrams),
            ("kiln.quic.bytes_sent", c.udp_tx.bytes),
            ("kiln.quic.bytes_received", c.udp_rx.bytes),
            ("kiln.quic.stream_frames_sent", c.frame_tx.stream),
            ("kiln.quic.stream_frames_received", c.frame_rx.stream),
            ("kiln.quic.resets_sent", c.frame_tx.reset_stream),
            ("kiln.quic.resets_received", c.frame_rx.reset_stream),
            ("kiln.quic.blocked_sent", blocked(&c.frame_tx)),
            ("kiln.quic.blocked_received", blocked(&c.frame_rx)),
        ] {
            point(out, metric, Kind::Counter, &tags, ts, v as f64);
        }
    }
}

/// This node's registry (the server's, or a runner's), every `every`, as Prometheus would scrape
/// `/metrics`, tagged with its machine; where kiln CD runs (the server), its apps counted by health
/// and sync (`kiln.cd.apps`); and its QUIC endpoint's transport ([`quic_points`]), when it has one.
pub fn start_registry(o: Observe, host: String, every: Duration, quic: Option<crate::net::NetHandle>) {
    if every.is_zero() {
        return;
    }
    let spawned = std::thread::Builder::new().name("kiln-observe-registry".into()).spawn(move || {
        // The apps by health and sync: every pair counted since kiln started, 0 when none is in it.
        let mut counts: std::collections::BTreeMap<(&'static str, &'static str), u64> = std::collections::BTreeMap::new();
        loop {
            let started = Instant::now();
            let text = crate::metrics::render();
            let mut scraped = kiln_obs::prom::parse(&text, crate::util::now_ms(), &[("host", host.as_str())]);
            scraped.series.retain(|(k, _)| kiln_obs::batch::parse_key(&k.key).is_ok_and(|p| !p.metric.starts_with("kiln_agent_") && !p.metric.starts_with("process_")));
            if let Some(cd) = crate::cd::get() {
                count_apps(&mut counts, cd.statuses.read().values().map(|s| (s.health, s.sync)));
                let ts = crate::util::now_ms();
                for (&(health, sync), &n) in &counts {
                    point(&mut scraped.series, "kiln.cd.apps", Kind::Gauge, &[("host", host.as_str()), ("health", health), ("sync", sync)], ts, n as f64);
                }
            }
            if let Some(s) = quic.as_ref().and_then(|q| q.sample(QUIC_SAMPLE_WAIT)) {
                quic_points(&mut scraped.series, &host, &s, crate::util::now_ms());
            }
            o.write(scraped.series);
            std::thread::sleep(every.saturating_sub(started.elapsed()));
        }
    });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "observe's scrape of kiln's registry did not start");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Step;

    /// A sample of the endpoint and one peer, as the metrics observe writes: each a gauge or a
    /// counter as it is, tagged with the node and the peer, in seconds and bytes; a threshold the
    /// controller keeps none of is not written.
    #[test]
    fn a_quic_sample_is_its_metrics_each_of_its_peer() {
        use std::time::Duration;
        let mut conn = hyper_quic::ConnectionStats::default();
        conn.path.sent_packets = 120;
        conn.path.lost_packets = 3;
        conn.path.lost_bytes = 3_600;
        conn.udp_tx.bytes = 140_000;
        conn.udp_rx.datagrams = 90;
        conn.frame_tx.stream = 77;
        conn.frame_tx.data_blocked = 1;
        conn.frame_tx.streams_blocked_bidi = 2;
        let path = hyper_transport::PathFacts {
            rtt: Duration::from_micros(12_500),
            min_rtt: Duration::from_micros(9_000),
            cwnd: 64_000,
            max_datagram: 1_452,
            delivery_rate: 5_120_000,
            receive_window: 8 << 20,
            pacing_rate: None,
            ssthresh: Some(48_000),
            warm_up_bytes: 0,
            warming_up: false,
            probe_timeout: Duration::from_millis(60),
        };
        let s =
            crate::net::QuicSample { connections: 1, exchanges: 4, streams_refused: 2, frames_dropped: 0, responses_dropped: 0, peers: vec![crate::net::PeerSample { name: "r1".into(), path, conn }] };
        let mut out = Vec::new();
        quic_points(&mut out, "server-1", &s, 1_000);
        let got: std::collections::BTreeMap<String, (Kind, Vec<(String, String)>, f64)> = out
            .iter()
            .filter_map(|(k, pts)| {
                let p = kiln_obs::batch::parse_key(&k.key).ok()?;
                let tags = p.tags.iter().map(|(a, b)| ((*a).to_owned(), (*b).to_owned())).collect();
                Some((p.metric.to_owned(), (k.kind, tags, pts.first()?.1)))
            })
            .collect();
        let peer = vec![("host".to_owned(), "server-1".to_owned()), ("peer".to_owned(), "r1".to_owned())];
        let own = vec![("host".to_owned(), "server-1".to_owned())];
        for (metric, kind, tags, v) in [
            ("kiln.quic.smoothed_rtt", Kind::Gauge, &peer, 0.0125),
            ("kiln.quic.min_rtt", Kind::Gauge, &peer, 0.009),
            ("kiln.quic.pto", Kind::Gauge, &peer, 0.06),
            ("kiln.quic.congestion_window", Kind::Gauge, &peer, 64_000.0),
            ("kiln.quic.ssthresh", Kind::Gauge, &peer, 48_000.0),
            ("kiln.quic.max_datagram", Kind::Gauge, &peer, 1_452.0),
            ("kiln.quic.packets_sent", Kind::Counter, &peer, 120.0),
            ("kiln.quic.packets_lost", Kind::Counter, &peer, 3.0),
            ("kiln.quic.bytes_lost", Kind::Counter, &peer, 3_600.0),
            ("kiln.quic.bytes_sent", Kind::Counter, &peer, 140_000.0),
            ("kiln.quic.datagrams_received", Kind::Counter, &peer, 90.0),
            ("kiln.quic.stream_frames_sent", Kind::Counter, &peer, 77.0),
            ("kiln.quic.blocked_sent", Kind::Counter, &peer, 3.0),
            ("kiln.quic.connections", Kind::Gauge, &own, 1.0),
            ("kiln.quic.exchanges", Kind::Gauge, &own, 4.0),
            ("kiln.quic.streams_refused", Kind::Counter, &own, 2.0),
        ] {
            let g = got.get(metric);
            assert!(g.is_some(), "{metric} not written: {:?}", got.keys().collect::<Vec<_>>());
            let Some(g) = g else { continue };
            assert_eq!((g.0, &g.1), (kind, tags), "{metric}");
            assert!((g.2 - v).abs() < 1e-12, "{metric}: {} not {v}", g.2);
        }
        assert!(!got.contains_key("kiln.quic.pacing_rate"), "a rate the controller keeps none of is not written");
    }

    #[test]
    fn a_health_once_counted_is_counted_0_when_no_app_is_in_it() {
        use kiln_cd::model::{Health, Sync};
        let mut counts = std::collections::BTreeMap::new();
        count_apps(&mut counts, [(Health::Degraded, Sync::Synced), (Health::Healthy, Sync::Synced), (Health::Healthy, Sync::Synced)].into_iter());
        assert_eq!(counts.iter().map(|(k, n)| (*k, *n)).collect::<Vec<_>>(), [(("Degraded", "Synced"), 1), (("Healthy", "Synced"), 2)]);
        count_apps(&mut counts, [(Health::Healthy, Sync::Synced), (Health::Healthy, Sync::OutOfSync), (Health::Healthy, Sync::Synced)].into_iter());
        assert_eq!(counts.iter().map(|(k, n)| (*k, *n)).collect::<Vec<_>>(), [(("Degraded", "Synced"), 0), (("Healthy", "OutOfSync"), 1), (("Healthy", "Synced"), 2)]);
    }

    fn step(idx: u32, name: &str, from: i64, to: i64, status: Status) -> Step {
        Step { idx, name: name.into(), status, started_at: Some(from), finished_at: Some(to), exit_code: Some(i32::from(status == Status::Failed)), seq: None }
    }

    fn job(id: &str, key: &str, from: i64, to: i64, status: Status, steps: Vec<Step>) -> Job {
        Job {
            id: id.into(),
            run_id: "run-1".into(),
            key: key.into(),
            display: key.into(),
            status,
            labels: Vec::new(),
            needs: Vec::new(),
            agent: Some("fedora".into()),
            executor: Some("shell".into()),
            attempt: 1,
            queued_at: Some(from - 3),
            started_at: Some(from),
            finished_at: Some(to),
            exit_code: None,
            error: None,
            peak_rss: None,
            cpu_seconds: None,
            spec: None,
            steps,
            matrix: None,
        }
    }

    /// A run of two jobs, the second failing at its test step: one trace, the run its root, each job
    /// under it and each step under its job, as the waterfall draws them; written again (a leg read
    /// back twice), the same spans.
    #[test]
    fn a_run_is_a_trace_of_its_jobs_and_their_steps() -> anyhow::Result<()> {
        let run = Run {
            id: "run-1".into(),
            repo_id: "r".into(),
            repo_name: "kiln".into(),
            number: 42,
            pipeline: "ci".into(),
            event: "push".into(),
            branch: "main".into(),
            sha: "abc123".into(),
            message: "m".into(),
            author: "ada".into(),
            status: Status::Failed,
            created_at: 1_000,
            started_at: Some(1_002),
            finished_at: Some(1_100),
            error: None,
            concurrency_group: None,
        };
        let build = job("job-b", "build", 1_005, 1_050, Status::Succeeded, vec![step(0, "checkout", 1_005, 1_008, Status::Succeeded), step(1, "cargo build", 1_008, 1_050, Status::Succeeded)]);
        let test = job("job-t", "test", 1_051, 1_100, Status::Failed, vec![step(0, "checkout", 1_051, 1_053, Status::Succeeded), step(1, "cargo test", 1_053, 1_100, Status::Failed)]);
        let mut spans = job_spans(&build, Some(&run), Status::Succeeded, 1_050);
        anyhow::ensure!(job_spans(&build, Some(&run), Status::Succeeded, 1_050) == spans, "the same spans written again");
        spans.extend(job_spans(&test, Some(&run), Status::Failed, 1_100));
        spans.push(run_span(&run, Status::Failed, 1_100));
        let (trace, _) = run_trace("run-1");
        anyhow::ensure!(spans.iter().all(|s| s.trace_id == Some(trace)));
        let t = kiln_obs::apm::assemble(spans);
        let shape: Vec<(&str, u32)> = t.spans.iter().map(|p| (p.span.message.as_str(), p.depth)).collect();
        anyhow::ensure!(shape == vec![("ci", 0), ("build", 1), ("checkout", 2), ("cargo build", 2), ("test", 1), ("checkout", 2), ("cargo test", 2)], "{shape:?}");
        anyhow::ensure!(t.roots == 1 && t.errors == 3, "the run, its test job and step failed: {}", t.errors);
        anyhow::ensure!((t.start, t.end) == (1_002_000_000_000, 1_100_000_000_000));
        let Some(root) = t.spans.first() else { anyhow::bail!("a root") };
        let attr = |k: &str| root.span.attrs.iter().find(|(a, _)| a == k).map(|(_, v)| v.as_str());
        anyhow::ensure!(attr("span.kind") == Some("server") && attr("ci.pipeline.number") == Some("42") && attr("git.commit.sha") == Some("abc123") && attr("duration") == Some("98000000000"));
        Ok(())
    }
}
