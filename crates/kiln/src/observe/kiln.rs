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
//! - kiln CD's deploys (`kiln.cd.deploys` by app, trigger and result; objects applied and pruned)
//!   and every change of an app's health or sync, as events (service `kiln.cd`); its apps counted
//!   by health and sync each interval (`kiln.cd.apps`).
//! - The registry's families keep their Prometheus names (`kiln_jobs_queued`, ...), but for the
//!   runners' host samples and the server process's figures, which observe reports itself.

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
    o.log(vec![Record { ts: ts.saturating_mul(1_000_000), severity: if failed(status) { 17 } else { 9 }, service: "kiln.ci".into(), host: runner.to_owned(), source: "kiln.event".into(), message, attrs, trace_id: None, span_id: None }]);
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
    o.log(vec![Record { ts: ts.saturating_mul(1_000_000), severity: if failed(status) { 17 } else { 9 }, service: "kiln.ci".into(), host: String::new(), source: "kiln.event".into(), message, attrs, trace_id: None, span_id: None }]);
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

/// The server's registry, every `every`, as Prometheus would scrape `/metrics`, tagged with the
/// server's machine; and kiln CD's apps counted by health and sync (`kiln.cd.apps`).
pub fn start_registry(o: Observe, host: String, every: Duration) {
    if every.is_zero() {
        return;
    }
    let spawned = std::thread::Builder::new().name("kiln-observe-registry".into()).spawn(move || {
        loop {
            let started = Instant::now();
            let text = crate::metrics::render();
            let mut scraped = kiln_obs::prom::parse(&text, crate::util::now_ms(), &[("host", host.as_str())]);
            scraped.series.retain(|(k, _)| kiln_obs::batch::parse_key(&k.key).is_ok_and(|p| !p.metric.starts_with("kiln_agent_") && !p.metric.starts_with("process_")));
            if let Some(cd) = crate::cd::get() {
                let mut counts: std::collections::BTreeMap<(String, String), u64> = std::collections::BTreeMap::new();
                for s in cd.statuses.read().values() {
                    *counts.entry((s.health.as_str().to_owned(), s.sync.as_str().to_owned())).or_default() += 1;
                }
                let ts = crate::util::now_ms();
                for ((health, sync), n) in counts {
                    point(&mut scraped.series, "kiln.cd.apps", Kind::Gauge, &[("host", host.as_str()), ("health", health.as_str()), ("sync", sync.as_str())], ts, n as f64);
                }
            }
            o.write(scraped.series);
            std::thread::sleep(every.saturating_sub(started.elapsed()));
        }
    });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "observe's scrape of kiln's registry did not start");
    }
}
