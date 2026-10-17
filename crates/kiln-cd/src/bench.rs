//! `kiln cd bench`: the engine at fleet scale, against simulated clusters.
//!
//! Four phases, each measured in simulated time (what a user waits) and in the CPU time spent
//! inside the engine (what kiln costs):
//!
//! 1. **cold start**: every app deploys from nothing;
//! 2. **steady**: ten simulated minutes in which controllers keep updating status, as they do;
//! 3. **rollout**: a share of the apps get a new revision;
//! 4. **drift**: someone edits a share of the Deployments by hand, and self-heal undoes it.
//!
//! Clusters answer within `latency_ms` and admit `qps` requests a second each, so rollout
//! times are bounded the way real ones are: by the clusters, not by kiln.

use std::time::Instant;

use serde_json::{Value, json};

use crate::engine::{Config, Engine, Input};
use crate::harness::Harness;
use crate::model::{AppConfig, SyncPolicy};
use crate::obj::{self, ResKey};
use crate::sim;

/// What to simulate.
#[derive(Clone, Debug)]
pub struct Bench {
    /// Apps.
    pub apps: usize,
    /// Clusters (apps are spread over them).
    pub clusters: usize,
    /// Objects per app: a Deployment, a Service, then ConfigMaps.
    pub resources: usize,
    /// Distinct renders: apps `i` and `j` render the same objects when `i % distinct == j %
    /// distinct` (a fleet deploying one app to many clusters). Equal to `apps` for none shared.
    pub distinct: usize,
    /// Share of apps that get a new revision in the rollout phase.
    pub churn: f64,
    /// Share of Deployments edited by hand in the drift phase.
    pub drift: f64,
    /// Share of Deployments whose status changes each simulated minute in the steady phase.
    pub status_rate: f64,
    /// Requests a second each cluster admits.
    pub qps: f64,
    /// API latency.
    pub latency_ms: u64,
    /// Rollout time of a Deployment.
    pub rollout_ms: u64,
    /// Dry-run before applying.
    pub dry_run: bool,
    /// Engines run in parallel, each owning a share of the clusters.
    pub shards: usize,
}

impl Default for Bench {
    fn default() -> Self {
        Self {
            apps: 10_000,
            clusters: 10,
            resources: 3,
            distinct: 10_000,
            churn: 0.01,
            drift: 0.001,
            status_rate: 0.1,
            qps: 500.0,
            latency_ms: 5,
            rollout_ms: 3_000,
            dry_run: true,
            shards: 1,
        }
    }
}

/// One phase's measurements.
#[derive(Clone, Debug, Default)]
pub struct Phase {
    /// Its name.
    pub name: &'static str,
    /// Simulated time it took.
    pub sim_ms: u64,
    /// Wall time it took.
    pub wall_ms: u128,
    /// Wall time spent inside the engine.
    pub engine_ms: u128,
    /// Inputs the engine took.
    pub inputs: u64,
    /// Whether it finished (every app settled) within its limit.
    pub done: bool,
    /// Resident memory after it, in MiB.
    pub rss_mib: Option<f64>,
}

/// A benchmark's results.
#[derive(Clone, Debug, Default)]
pub struct Report {
    /// The phases, in order.
    pub phases: Vec<Phase>,
    /// Engine counters at the end.
    pub stats: crate::engine::Stats,
    /// Objects across clusters at the end.
    pub objects: usize,
    /// What the engine holds at the end.
    pub footprint: crate::engine::Footprint,
}

/// The process's peak resident memory so far (Linux), in MiB.
fn rss_mib() -> Option<f64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|l| l.starts_with("VmHWM:"))?;
    let kib: f64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kib / 1024.0)
}

/// The objects app `i` renders at `version`.
fn objects(b: &Bench, i: usize, version: u64) -> Vec<Value> {
    let shared = i % b.distinct.max(1);
    let name = format!("app-{shared}");
    let mut out = vec![
        json!({"apiVersion": "apps/v1", "kind": "Deployment", "metadata": {"name": name, "labels": {"app": name, "team": "t"}},
            "spec": {"replicas": 2, "selector": {"matchLabels": {"app": name}}, "template": {"metadata": {"labels": {"app": name}},
                "spec": {"containers": [{"name": "app", "image": format!("ghcr.io/hyper-light/app:{version}"), "ports": [{"containerPort": 8080}],
                    "env": [{"name": "MODE", "value": "prod"}], "resources": {"requests": {"cpu": "100m", "memory": "128Mi"}}}]}}}}),
        json!({"apiVersion": "v1", "kind": "Service", "metadata": {"name": name}, "spec": {"selector": {"app": name}, "ports": [{"port": 80, "targetPort": 8080}]}}),
    ];
    for c in 2..b.resources {
        out.push(json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": format!("{name}-cfg-{c}")},
            "data": {"config.yaml": format!("version: {version}\nreplicas: 2\nfeature-x: true\n")}}));
    }
    out
}

fn app_id(i: usize) -> String {
    format!("app-{i}")
}

fn cluster_of(b: &Bench, i: usize) -> String {
    format!("cluster-{}", i % b.clusters.max(1))
}

/// Runs one shard of the benchmark: the clusters `c` with `c % shards == shard`, and their apps.
fn run_shard(b: &Bench, shard: usize, shards: usize, mut progress: impl FnMut(&Phase)) -> Report {
    let mine = |i: usize| (i % b.clusters.max(1)) % shards.max(1) == shard;
    let my_apps: Vec<usize> = (0..b.apps).filter(|i| mine(*i)).collect();
    let cfg = Config { dry_run_first: b.dry_run, cluster_qps: b.qps, cluster_burst: b.qps, cluster_inflight: 256, ..Config::default() };
    let bb = b.clone();
    let renderer = Box::new(move |app: &str, rev: &str, _key: u64| {
        let i: usize = app.trim_start_matches("app-").parse().map_err(|_| format!("bad app {app}"))?;
        let version: u64 = rev.trim_start_matches('v').parse().unwrap_or(1);
        Ok(objects(&bb, i, version))
    });
    let mut h = Harness::new(Engine::new(cfg), renderer);
    h.latency_ms = b.latency_ms;
    for c in (0..b.clusters.max(1)).filter(|c| c % shards.max(1) == shard) {
        let mut cl = sim::Cluster::new(&format!("cluster-{c}"));
        cl.rollout_ms = b.rollout_ms;
        h.add_cluster(cl, Some(b.qps));
    }
    let mut report = Report::default();
    let phase = |h: &mut Harness, name: &'static str, limit_ms: u64, setup: &mut dyn FnMut(&mut Harness)| -> Phase {
        let (wall, engine0, inputs0, sim0) = (Instant::now(), h.engine_ns, h.inputs, h.now);
        setup(h);
        let done = h.run_until(limit_ms, Harness::all_settled);
        Phase {
            name,
            sim_ms: h.now - sim0,
            wall_ms: wall.elapsed().as_millis(),
            engine_ms: (h.engine_ns - engine0) / 1_000_000,
            inputs: h.inputs - inputs0,
            done,
            rss_mib: rss_mib(),
        }
    };

    // 1. Cold start.
    let p = phase(&mut h, "cold start", 3_600_000, &mut |h| {
        for &i in &my_apps {
            let mut a = AppConfig::new(&app_id(i), &cluster_of(b, i), &format!("ns-{}", i % 100));
            a.sync = SyncPolicy { auto: true, prune: true, self_heal: true, ..SyncPolicy::default() };
            h.input(Input::UpsertApp(a));
            let shared = (i % b.distinct.max(1)) as u64;
            h.input(Input::Inputs { app: app_id(i).into(), revision: "v1".into(), key: obj::hash_bytes(&[shared.to_le_bytes(), 1u64.to_le_bytes()].concat()) });
        }
    });
    progress(&p);
    report.phases.push(p);

    // 2. Steady: status-only churn, ten simulated minutes.
    let (wall, engine0, inputs0, sim0) = (Instant::now(), h.engine_ns, h.inputs, h.now);
    let per_minute = ((my_apps.len() as f64) * b.status_rate) as usize;
    let mut cursor = 0usize;
    for minute in 0..10u64 {
        for _ in 0..per_minute {
            let Some(&i) = my_apps.get(cursor % my_apps.len().max(1)) else { break };
            cursor += 1;
            let key = ResKey::new("apps", "Deployment", &format!("ns-{}", i % 100), &format!("app-{}", i % b.distinct.max(1)));
            h.touch(&cluster_of(b, i), &key, |s| {
                s.insert("readyReplicas".into(), json!(2));
                s.insert("observedAt".into(), json!(minute));
            });
        }
        h.run_for(60_000);
    }
    let p = Phase {
        name: "steady (10 min)",
        sim_ms: h.now - sim0,
        wall_ms: wall.elapsed().as_millis(),
        engine_ms: (h.engine_ns - engine0) / 1_000_000,
        inputs: h.inputs - inputs0,
        done: h.all_settled(),
        rss_mib: rss_mib(),
    };
    progress(&p);
    report.phases.push(p);

    // 3. Rollout: a share of the apps move to v2.
    let changed = ((my_apps.len() as f64) * b.churn).ceil() as usize;
    let (wall, engine0, inputs0, sim0) = (Instant::now(), h.engine_ns, h.inputs, h.now);
    let records0 = h.records.len();
    for &i in my_apps.iter().take(changed) {
        let shared = (i % b.distinct.max(1)) as u64;
        h.input(Input::Inputs { app: app_id(i).into(), revision: "v2".into(), key: obj::hash_bytes(&[shared.to_le_bytes(), 2u64.to_le_bytes()].concat()) });
    }
    let target = changed.min(my_apps.len());
    let done = h.run_until(3_600_000, |h| {
        let synced = h.records.get(records0..).map(|r| {
            r.iter().filter(|r| matches!(r, crate::engine::Record::Finished { succeeded: true, revision: Some(rev), .. } if rev.as_ref() == "v2")).count()
        });
        synced.unwrap_or(0) >= target && h.all_settled()
    });
    let p = Phase {
        name: "rollout",
        sim_ms: h.now - sim0,
        wall_ms: wall.elapsed().as_millis(),
        engine_ms: (h.engine_ns - engine0) / 1_000_000,
        inputs: h.inputs - inputs0,
        done,
        rss_mib: rss_mib(),
    };
    progress(&p);
    report.phases.push(p);

    // 4. Drift: hand edits to a share of the Deployments; self-heal undoes them.
    let drifted = ((my_apps.len() as f64) * b.drift).ceil() as usize;
    let p = phase(&mut h, "drift + self-heal", 3_600_000, &mut |h| {
        let step = (my_apps.len() / drifted.max(1)).max(1);
        for n in 0..drifted {
            let Some(&i) = my_apps.get((n * step) % my_apps.len().max(1)) else { break };
            let key = ResKey::new("apps", "Deployment", &format!("ns-{}", i % 100), &format!("app-{}", i % b.distinct.max(1)));
            h.edit(&cluster_of(b, i), &key, |v| {
                if let Some(spec) = v.get_mut("spec").and_then(Value::as_object_mut) {
                    spec.insert("replicas".into(), json!(7));
                }
            });
        }
        // Self-heal waits out its backoff; give the drift time to be seen first.
        h.run_for(100);
    });
    progress(&p);
    report.phases.push(p);

    report.stats = h.engine.stats();
    report.footprint = h.engine.footprint();
    report.objects = h.clusters.values().map(sim::Cluster::len).sum();
    report
}

/// Runs the benchmark over `b.shards` engines in parallel, one thread each, as kiln runs one
/// engine per shard. Phases report the slowest shard's simulated and wall time and the total
/// CPU time spent inside the engines.
pub fn run(b: &Bench, progress: impl Fn(&Phase) + Sync) -> Report {
    let shards = b.shards.max(1);
    if shards == 1 {
        return run_shard(b, 0, 1, &progress);
    }
    let reports: Vec<Report> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..shards).map(|s| scope.spawn(move || run_shard(b, s, shards, |_| {}))).collect();
        handles.into_iter().filter_map(|h| h.join().ok()).collect()
    });
    let mut out = Report::default();
    for r in &reports {
        for (i, p) in r.phases.iter().enumerate() {
            if out.phases.len() <= i {
                out.phases.push(Phase { name: p.name, done: true, ..Phase::default() });
            }
            if let Some(o) = out.phases.get_mut(i) {
                o.sim_ms = o.sim_ms.max(p.sim_ms);
                o.wall_ms = o.wall_ms.max(p.wall_ms);
                o.engine_ms += p.engine_ms;
                o.inputs += p.inputs;
                o.done &= p.done;
                o.rss_mib = rss_mib();
            }
        }
        let (s, t) = (&mut out.stats, r.stats);
        s.live_events += t.live_events;
        s.live_unchanged += t.live_unchanged;
        s.comparisons += t.comparisons;
        s.renders += t.renders;
        s.render_hits += t.render_hits;
        s.applies += t.applies;
        s.deletes += t.deletes;
        s.syncs += t.syncs;
        s.syncs_ok += t.syncs_ok;
        s.syncs_failed += t.syncs_failed;
        s.statuses += t.statuses;
        out.objects += r.objects;
        let (f, g) = (&mut out.footprint, r.footprint);
        f.apps += g.apps;
        f.resources += g.resources;
        f.renders += g.renders;
        f.app_bytes += g.app_bytes;
        f.resource_bytes += g.resource_bytes;
        f.index_bytes += g.index_bytes;
        f.name_bytes += g.name_bytes;
        f.live_bytes += g.live_bytes;
        f.desired_bytes += g.desired_bytes;
    }
    for p in &out.phases {
        progress(p);
    }
    out
}

/// The report as a table.
pub fn table(b: &Bench, r: &Report) -> String {
    let mut out = format!(
        "kiln cd bench: {} apps × {} objects over {} clusters ({} distinct renders), {} engine shards, {} req/s per cluster, {} ms latency\n\n",
        b.apps, b.resources, b.clusters, b.distinct, b.shards, b.qps, b.latency_ms
    );
    out.push_str(&format!("{:<20} {:>12} {:>12} {:>12} {:>12} {:>10} {:>6}\n", "phase", "simulated", "wall", "engine cpu", "inputs", "peak MiB", "done"));
    for p in &r.phases {
        out.push_str(&format!(
            "{:<20} {:>10.1} s {:>10.2} s {:>10.2} s {:>12} {:>10} {:>6}\n",
            p.name,
            p.sim_ms as f64 / 1000.0,
            p.wall_ms as f64 / 1000.0,
            p.engine_ms as f64 / 1000.0,
            p.inputs,
            p.rss_mib.map(|m| format!("{m:.0}")).unwrap_or_else(|| "-".into()),
            if p.done { "yes" } else { "NO" }
        ));
    }
    let s = r.stats;
    out.push_str(&format!(
        "\n{} objects live · {} live events ({} unchanged, no comparison) · {} comparisons · {} renders ({} shared) · {} applies · {} deletes · {} syncs ({} ok, {} failed) · {} status updates\n",
        r.objects, s.live_events, s.live_unchanged, s.comparisons, s.renders, s.render_hits, s.applies, s.deletes, s.syncs, s.syncs_ok, s.syncs_failed, s.statuses
    ));
    let f = r.footprint;
    let mib = |b: usize| b as f64 / (1024.0 * 1024.0);
    out.push_str(&format!(
        "engine holds ≈ {:.0} MiB ({:.0} B per resource): live {:.0} · rendered {:.0} · resources {:.0} · index {:.0} · names {:.0} · apps {:.0} MiB\n",
        mib(f.total()),
        f.total() as f64 / f.resources.max(1) as f64,
        mib(f.live_bytes),
        mib(f.desired_bytes),
        mib(f.resource_bytes),
        mib(f.index_bytes),
        mib(f.name_bytes),
        mib(f.app_bytes)
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_small_fleet_settles_in_every_phase() {
        let b = Bench { apps: 200, clusters: 4, distinct: 200, ..Bench::default() };
        let r = run(&b, |_| {});
        for p in &r.phases {
            assert!(p.done, "{} did not settle: {}", p.name, table(&b, &r));
        }
        assert_eq!(r.objects, 200 * 3);
        // Steady status churn never costs a comparison.
        assert_eq!(r.stats.syncs_failed, 0);
    }
}
