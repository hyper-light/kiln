//! The engine end to end, against simulated clusters: syncs, waves, hooks, prunes, self-heal,
//! retries, shared renders and request budgets, each watched happening.

use std::cell::RefCell;
use std::rc::Rc;

use rustc_hash::FxHashMap;
use serde_json::{Value, json};

use crate::engine::{Config, Engine, Input, OpPhase, Record, SyncRequest};
use crate::harness::Harness;
use crate::model::{AppConfig, Health, Retry, Sync, SyncPolicy};
use crate::obj::{self, ResKey};
use crate::sim;

type Git = Rc<RefCell<FxHashMap<String, Vec<Value>>>>;

fn deployment(name: &str, image: &str, annotations: Value) -> Value {
    json!({"apiVersion": "apps/v1", "kind": "Deployment", "metadata": {"name": name, "annotations": annotations},
        "spec": {"replicas": 2, "selector": {"matchLabels": {"app": name}}, "template": {"metadata": {"labels": {"app": name}},
            "spec": {"containers": [{"name": "app", "image": image, "ports": [{"containerPort": 8080}]}]}}}})
}

fn config_map(name: &str, data: &str, annotations: Value) -> Value {
    json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": name, "annotations": annotations}, "data": {"v": data}})
}

fn service(name: &str) -> Value {
    json!({"apiVersion": "v1", "kind": "Service", "metadata": {"name": name}, "spec": {"selector": {"app": name}, "ports": [{"port": 80}]}})
}

fn job(name: &str, hook: &str) -> Value {
    json!({"apiVersion": "batch/v1", "kind": "Job", "metadata": {"name": name, "annotations": {"argocd.argoproj.io/hook": hook}},
        "spec": {"template": {"spec": {"restartPolicy": "Never", "containers": [{"name": "j", "image": "busybox"}]}}}})
}

/// A harness whose renders read "git": revision → objects.
fn harness(cfg: Config) -> (Harness, Git) {
    let git: Git = Rc::new(RefCell::new(FxHashMap::default()));
    let g = git.clone();
    let renderer = Box::new(move |_app: &str, rev: &str, _key: u64| {
        g.borrow().get(rev).cloned().ok_or_else(|| format!("no revision {rev}"))
    });
    let mut h = Harness::new(Engine::new(cfg), renderer);
    h.add_cluster(sim::Cluster::new("dev"), None);
    (h, git)
}

fn fast() -> Config {
    Config { wave_delay_ms: 0, ..Config::default() }
}

fn auto(id: &str) -> AppConfig {
    let mut a = AppConfig::new(id, "dev", "web");
    a.sync = SyncPolicy { auto: true, prune: true, self_heal: true, ..SyncPolicy::default() };
    a
}

fn deploy(h: &mut Harness, git: &Git, app: &str, rev: &str, objects: Vec<Value>) {
    git.borrow_mut().insert(rev.to_owned(), objects);
    let key = obj::hash_bytes(rev.as_bytes());
    h.input(Input::Inputs { app: app.into(), revision: rev.into(), key, content: key });
}

fn status<'a>(h: &'a Harness, app: &str) -> Option<&'a crate::engine::Summary> {
    h.statuses.get(app)
}

fn settled(h: &Harness, app: &str) -> bool {
    status(h, app).is_some_and(|s| {
        s.sync == Sync::Synced && s.health == Health::Healthy && s.operation.as_ref().is_some_and(|o| o.phase == OpPhase::Succeeded)
    })
}

#[test]
fn an_automatic_app_deploys_and_turns_healthy() {
    let (mut h, git) = harness(fast());
    h.input(Input::UpsertApp(auto("web")));
    deploy(&mut h, &git, "web", "r1", vec![config_map("cfg", "1", json!({})), deployment("web", "web:1", json!({})), service("web")]);
    assert!(h.run_until(60_000, |h| settled(h, "web")), "{:?}", status(&h, "web"));
    let finished: Vec<bool> = h.records.iter().filter_map(|r| match r { Record::Finished { succeeded, .. } => Some(*succeeded), _ => None }).collect();
    assert_eq!(finished, [true]);
    // The API server's defaults (imagePullPolicy, protocols, clusterIP) are not drift.
    assert_eq!(status(&h, "web").map(|s| (s.resources, s.out_of_sync)), Some((3, 0)));
    let dep = ResKey::new("apps", "Deployment", "web", "web");
    let live = h.clusters.get("dev").and_then(|c| c.get(&dep)).unwrap_or_default();
    assert_eq!(obj::label(&live, crate::engine::APP_LABEL), Some(crate::engine::app_label("web").as_str()));
}

#[test]
fn drift_is_healed_only_with_self_heal() {
    for heal in [true, false] {
        let (mut h, git) = harness(fast());
        let mut a = auto("web");
        a.sync.self_heal = heal;
        h.input(Input::UpsertApp(a));
        deploy(&mut h, &git, "web", "r1", vec![deployment("web", "web:1", json!({}))]);
        assert!(h.run_until(60_000, |h| settled(h, "web")));
        let key = ResKey::new("apps", "Deployment", "web", "web");
        h.edit("dev", &key, |v| {
            if let Some(c) = v.pointer_mut("/spec/template/spec/containers/0").and_then(Value::as_object_mut) {
                c.insert("image".into(), json!("evil:1"));
            }
        });
        h.run_for(60_000);
        let heals = h.records.iter().filter(|r| matches!(r, Record::Started { trigger: crate::engine::Trigger::SelfHeal, .. })).count();
        assert_eq!(heals, usize::from(heal));
        let live_image = h
            .clusters
            .get("dev")
            .and_then(|c| c.get(&key))
            .and_then(|v| v.pointer("/spec/template/spec/containers/0/image").and_then(Value::as_str).map(str::to_owned));
        if heal {
            assert_eq!(live_image.as_deref(), Some("web:1"));
            assert_eq!(status(&h, "web").map(|s| s.sync), Some(Sync::Synced));
        } else {
            assert_eq!(live_image.as_deref(), Some("evil:1"));
            assert_eq!(status(&h, "web").map(|s| s.sync), Some(Sync::OutOfSync));
        }
    }
}

#[test]
fn a_diff_shows_only_what_a_sync_changes() {
    let (mut h, git) = harness(fast());
    let mut a = auto("web");
    a.sync.self_heal = false;
    h.input(Input::UpsertApp(a));
    deploy(&mut h, &git, "web", "r1", vec![deployment("web", "web:1", json!({}))]);
    assert!(h.run_until(60_000, |h| settled(h, "web")));
    let key = ResKey::new("apps", "Deployment", "web", "web");
    h.edit("dev", &key, |v| {
        if let Some(c) = v.pointer_mut("/spec/template/spec/containers/0").and_then(Value::as_object_mut) {
            c.insert("image".into(), json!("evil:1"));
        }
    });
    h.run_for(5_000);
    let change = h.engine.change("web", &key);
    let image = |v: Option<&Value>| v.and_then(|v| v.pointer("/spec/template/spec/containers/0/image")).cloned();
    assert_eq!(
        change.as_ref().map(|c| (image(c.before.as_ref()), image(c.after.as_ref()))),
        Some((Some(json!("evil:1")), Some(json!("web:1"))))
    );
    // Everything else, the cluster's defaults included, is the same on both sides.
    let mut healed = change.as_ref().and_then(|c| c.before.clone());
    if let Some(c) = healed.as_mut().and_then(|v| v.pointer_mut("/spec/template/spec/containers/0")).and_then(Value::as_object_mut) {
        c.insert("image".into(), json!("web:1"));
    }
    assert!(healed.as_ref().and_then(|v| v.pointer("/spec/template/spec/containers/0/imagePullPolicy")).is_some());
    assert_eq!(healed, change.and_then(|c| c.after));
}

/// A second engine over the same simulated cluster: kiln restarted.
fn restart(h: &mut Harness) -> (Harness, Git) {
    let (mut h2, git2) = harness(fast());
    if let Some(c) = h.clusters.remove("dev") {
        h2.clusters.insert("dev".into(), c);
    }
    (h2, git2)
}

#[test]
fn after_a_restart_nothing_syncs_before_the_cluster_is_read() {
    let (mut h, git) = harness(fast());
    h.input(Input::UpsertApp(auto("web")));
    let objects = vec![config_map("cfg", "1", json!({})), deployment("web", "web:1", json!({}))];
    deploy(&mut h, &git, "web", "r1", objects.clone());
    assert!(h.run_until(60_000, |h| settled(h, "web")));
    // The cluster's lists are slow to come back; the render is not.
    let (mut h2, git2) = restart(&mut h);
    h2.latency_ms = 2_000;
    h2.input(Input::UpsertApp(auto("web")));
    deploy(&mut h2, &git2, "web", "r1", objects);
    h2.run_for(1_000);
    assert_eq!(status(&h2, "web").map(|s| s.sync), Some(Sync::Unknown));
    h2.run_for(10_000);
    assert!(!h2.records.iter().any(|r| matches!(r, Record::Started { .. })), "{:?}", h2.records);
    assert_eq!(status(&h2, "web").map(|s| s.sync), Some(Sync::Synced));
}

#[test]
fn a_restart_remembers_the_last_sync_and_leaves_drift_without_self_heal() {
    let no_heal = || {
        let mut a = auto("web");
        a.sync.self_heal = false;
        a
    };
    let (mut h, git) = harness(fast());
    h.input(Input::UpsertApp(no_heal()));
    let objects = vec![deployment("web", "web:1", json!({}))];
    deploy(&mut h, &git, "web", "r1", objects.clone());
    assert!(h.run_until(60_000, |h| settled(h, "web")));
    let last = h.records.iter().rev().find_map(|r| match r {
        Record::Finished { content: Some(c), succeeded, .. } => Some((*c, *succeeded)),
        _ => None,
    });
    assert!(last.is_some());
    let key = ResKey::new("apps", "Deployment", "web", "web");
    h.edit("dev", &key, |v| {
        if let Some(s) = v.pointer_mut("/spec").and_then(Value::as_object_mut) {
            s.insert("replicas".into(), json!(1));
        }
    });
    let (mut h2, git2) = restart(&mut h);
    h2.input(Input::UpsertApp(no_heal()));
    if let Some((content, succeeded)) = last {
        h2.input(Input::Remember { app: "web".into(), content, succeeded });
    }
    deploy(&mut h2, &git2, "web", "r1", objects);
    h2.run_for(30_000);
    assert!(!h2.records.iter().any(|r| matches!(r, Record::Started { .. })), "{:?}", h2.records);
    assert_eq!(status(&h2, "web").map(|s| s.sync), Some(Sync::OutOfSync));
}

#[test]
fn a_custom_resource_takes_its_scope_from_a_crd_in_the_same_render() {
    let (mut h, git) = harness(fast());
    let crd = json!({"apiVersion": "apiextensions.k8s.io/v1", "kind": "CustomResourceDefinition", "metadata": {"name": "issuers.example.com"},
        "spec": {"group": "example.com", "names": {"kind": "ClusterIssuer", "plural": "clusterissuers"}, "scope": "Cluster",
            "versions": [{"name": "v1", "served": true, "storage": true}]}});
    let issuer = json!({"apiVersion": "example.com/v1", "kind": "ClusterIssuer", "metadata": {"name": "letsencrypt"}, "spec": {}});
    h.input(Input::UpsertApp(AppConfig::new("web", "dev", "web")));
    deploy(&mut h, &git, "web", "r1", vec![crd, issuer]);
    h.run_for(1_000);
    let keys: Vec<ResKey> = h.engine.resources("web").into_iter().map(|r| r.key).collect();
    assert!(keys.contains(&ResKey::new("example.com", "ClusterIssuer", "", "letsencrypt")), "{keys:?}");
}

#[test]
fn a_sync_asked_for_while_a_render_is_in_flight_deploys_that_render() {
    let (mut h, git) = harness(fast());
    h.input(Input::UpsertApp(AppConfig::new("web", "dev", "web")));
    deploy(&mut h, &git, "web", "r1", vec![deployment("web", "web:1", json!({}))]);
    h.run_for(1_000);
    h.input(Input::Sync { app: "web".into(), request: SyncRequest::default() });
    assert!(h.run_until(60_000, |h| settled(h, "web")));
    // A new commit, and a sync asked for before its render is back.
    deploy(&mut h, &git, "web", "r2", vec![deployment("web", "web:2", json!({}))]);
    h.input(Input::Sync { app: "web".into(), request: SyncRequest::default() });
    h.run_for(30_000);
    let key = ResKey::new("apps", "Deployment", "web", "web");
    let image = h.clusters.get("dev").and_then(|c| c.get(&key)).and_then(|v| v.pointer("/spec/template/spec/containers/0/image").cloned());
    assert_eq!(image, Some(json!("web:2")));
    assert_eq!(status(&h, "web").map(|s| s.sync), Some(Sync::Synced));
}

#[test]
fn a_render_key_that_moves_only_with_the_cluster_syncs_nothing_again() {
    let (mut h, git) = harness(fast());
    let mut a = auto("web");
    a.sync.self_heal = false;
    h.input(Input::UpsertApp(a));
    deploy(&mut h, &git, "web", "r1", vec![deployment("web", "web:1", json!({}))]);
    assert!(h.run_until(60_000, |h| settled(h, "web")));
    let key = ResKey::new("apps", "Deployment", "web", "web");
    h.edit("dev", &key, |v| {
        if let Some(s) = v.pointer_mut("/spec").and_then(Value::as_object_mut) {
            s.insert("replicas".into(), json!(1));
        }
    });
    h.run_for(5_000);
    let started = |h: &Harness| h.records.iter().filter(|r| matches!(r, Record::Started { .. })).count();
    let before = started(&h);
    // The cluster's API versions moved (a CRD installed somewhere): a new key, the same content.
    let content = obj::hash_bytes(b"r1");
    h.input(Input::Inputs { app: "web".into(), revision: "r1".into(), key: content ^ 0xC1, content });
    h.run_for(30_000);
    assert_eq!(started(&h), before, "{:?}", h.records);
    assert_eq!(status(&h, "web").map(|s| s.sync), Some(Sync::OutOfSync));
    // A new commit still syncs.
    deploy(&mut h, &git, "web", "r2", vec![deployment("web", "web:2", json!({}))]);
    assert!(h.run_until(60_000, |h| started(h) > before));
}

#[test]
fn a_rollback_syncs_as_one_step_and_automation_cannot_cut_in() {
    let (mut h, git) = harness(fast());
    let mut a = auto("web");
    a.sync.self_heal = false;
    h.input(Input::UpsertApp(a));
    deploy(&mut h, &git, "web", "r1", vec![deployment("web", "web:1", json!({}))]);
    assert!(h.run_until(60_000, |h| settled(h, "web")));
    deploy(&mut h, &git, "web", "r2", vec![deployment("web", "web:2", json!({}))]);
    assert!(h.run_until(60_000, |h| h.records.iter().filter(|r| matches!(r, Record::Finished { succeeded: true, .. })).count() == 2));
    // Back to r1, in one step: its render was kept, so the sync starts at once, as a rollback.
    let key = obj::hash_bytes(b"r1");
    let request = SyncRequest { trigger: Some(crate::engine::Trigger::Rollback), ..SyncRequest::default() };
    h.input(Input::SyncTo { app: "web".into(), revision: "r1".into(), key, content: key, request });
    assert!(h.run_until(60_000, |h| settled(h, "web")));
    let triggers: Vec<crate::engine::Trigger> = h.records.iter().filter_map(|r| match r { Record::Started { trigger, .. } => Some(*trigger), _ => None }).collect();
    assert_eq!(triggers.last(), Some(&crate::engine::Trigger::Rollback), "{triggers:?}");
    let dep = ResKey::new("apps", "Deployment", "web", "web");
    let image = h.clusters.get("dev").and_then(|c| c.get(&dep)).and_then(|v| v.pointer("/spec/template/spec/containers/0/image").cloned());
    assert_eq!(image, Some(json!("web:1")));
}

#[test]
fn a_wave_starts_only_when_the_one_before_is_healthy() {
    let (mut h, git) = harness(fast());
    if let Some(c) = h.clusters.get_mut("dev") {
        c.rollout_ms = 10_000;
    }
    h.input(Input::UpsertApp(auto("web")));
    deploy(&mut h, &git, "web", "r1", vec![
        deployment("db", "db:1", json!({"argocd.argoproj.io/sync-wave": "0"})),
        config_map("after-db", "1", json!({"argocd.argoproj.io/sync-wave": "1"})),
    ]);
    let cm = ResKey::new("", "ConfigMap", "web", "after-db");
    h.run_for(5_000);
    assert!(h.clusters.get("dev").and_then(|c| c.get(&cm)).is_none(), "wave 1 ran before wave 0 was healthy");
    assert!(h.run_until(60_000, |h| settled(h, "web")));
    assert!(h.clusters.get("dev").and_then(|c| c.get(&cm)).is_some());
}

#[test]
fn a_failing_pre_sync_hook_fails_the_sync_and_runs_sync_fail_hooks() {
    let (mut h, git) = harness(fast());
    if let Some(c) = h.clusters.get_mut("dev") {
        c.failing_jobs.insert("migrate".into());
    }
    h.input(Input::UpsertApp(auto("web")));
    deploy(&mut h, &git, "web", "r1", vec![job("migrate", "PreSync"), job("page-someone", "SyncFail"), deployment("web", "web:1", json!({}))]);
    let failed = |h: &Harness| h.records.iter().any(|r| matches!(r, Record::Finished { succeeded: false, .. }));
    assert!(h.run_until(60_000, failed));
    let dep = ResKey::new("apps", "Deployment", "web", "web");
    let page = ResKey::new("batch", "Job", "web", "page-someone");
    let c = h.clusters.get("dev");
    assert!(c.and_then(|c| c.get(&dep)).is_none(), "the Deployment was applied after its PreSync hook failed");
    assert!(c.and_then(|c| c.get(&page)).is_some(), "the SyncFail hook didn't run");
    assert_eq!(status(&h, "web").and_then(|s| s.operation.as_ref().map(|o| o.phase)), Some(OpPhase::Failed));
}

#[test]
fn hooks_run_in_their_phase_and_succeed() {
    let (mut h, git) = harness(fast());
    h.input(Input::UpsertApp(auto("web")));
    deploy(&mut h, &git, "web", "r1", vec![job("migrate", "PreSync"), deployment("web", "web:1", json!({})), job("smoke", "PostSync")]);
    assert!(h.run_until(60_000, |h| settled(h, "web")), "{:?}", status(&h, "web"));
    let c = h.clusters.get("dev");
    assert!(c.and_then(|c| c.get(&ResKey::new("batch", "Job", "web", "smoke"))).is_some());
    // Hooks don't count as the app's resources.
    assert_eq!(status(&h, "web").map(|s| s.resources), Some(1));
}

#[test]
fn objects_git_drops_are_pruned() {
    let (mut h, git) = harness(fast());
    h.input(Input::UpsertApp(auto("web")));
    deploy(&mut h, &git, "web", "r1", vec![config_map("a", "1", json!({})), config_map("b", "1", json!({}))]);
    assert!(h.run_until(60_000, |h| settled(h, "web")));
    deploy(&mut h, &git, "web", "r2", vec![config_map("a", "1", json!({}))]);
    let b = ResKey::new("", "ConfigMap", "web", "b");
    assert!(h.run_until(60_000, |h| h.clusters.get("dev").and_then(|c| c.get(&b)).is_none() && settled(h, "web")));
    assert_eq!(status(&h, "web").map(|s| s.resources), Some(1));
}

#[test]
fn a_failed_automatic_sync_is_not_repeated_until_git_moves() {
    let (mut h, git) = harness(fast());
    if let Some(c) = h.clusters.get_mut("dev") {
        c.fail_names.insert("web".into());
    }
    h.input(Input::UpsertApp(auto("web")));
    deploy(&mut h, &git, "web", "r1", vec![deployment("web", "web:1", json!({}))]);
    h.run_for(120_000);
    let attempts = |h: &Harness| h.records.iter().filter(|r| matches!(r, Record::Started { .. })).count();
    assert_eq!(attempts(&h), 1);
    if let Some(c) = h.clusters.get_mut("dev") {
        c.fail_names.clear();
    }
    deploy(&mut h, &git, "web", "r2", vec![deployment("web", "web:2", json!({}))]);
    assert!(h.run_until(60_000, |h| settled(h, "web")));
    assert_eq!(attempts(&h), 2);
}

#[test]
fn failed_syncs_retry_with_backoff_up_to_the_limit() {
    let (mut h, git) = harness(fast());
    if let Some(c) = h.clusters.get_mut("dev") {
        c.fail_names.insert("web".into());
    }
    let mut a = auto("web");
    a.sync.retry = Some(Retry { limit: 2, backoff_ms: 1_000, factor: 2, max_ms: 60_000 });
    h.input(Input::UpsertApp(a));
    deploy(&mut h, &git, "web", "r1", vec![deployment("web", "web:1", json!({}))]);
    h.run_for(1_500);
    let starts = |h: &Harness| h.records.iter().filter(|r| matches!(r, Record::Started { .. })).count();
    assert_eq!(starts(&h), 2, "the first retry comes after 1 s");
    h.run_for(60_000);
    assert_eq!(starts(&h), 3, "two retries, then it stops");
    assert_eq!(h.records.iter().filter(|r| matches!(r, Record::Finished { succeeded: false, .. })).count(), 1);
}

#[test]
fn apps_with_the_same_inputs_share_one_render() {
    let renders = Rc::new(RefCell::new(0u32));
    let r = renders.clone();
    let renderer = Box::new(move |_app: &str, _rev: &str, _key: u64| {
        *r.borrow_mut() += 1;
        Ok(vec![config_map("shared", "1", json!({}))])
    });
    let mut h = Harness::new(Engine::new(fast()), renderer);
    for c in ["a", "b", "c"] {
        h.add_cluster(sim::Cluster::new(c), None);
        let mut app = AppConfig::new(&format!("web-{c}"), c, "web");
        app.sync.auto = true;
        h.input(Input::UpsertApp(app));
        h.input(Input::Inputs { app: format!("web-{c}").into(), revision: "r1".into(), key: 7, content: 7 });
    }
    assert!(h.run_until(60_000, Harness::all_settled));
    assert_eq!(*renders.borrow(), 1);
}

#[test]
fn status_only_events_cost_no_comparison() {
    let (mut h, git) = harness(fast());
    h.input(Input::UpsertApp(auto("web")));
    deploy(&mut h, &git, "web", "r1", vec![deployment("web", "web:1", json!({}))]);
    assert!(h.run_until(60_000, |h| settled(h, "web")));
    let before = h.engine.stats();
    let key = ResKey::new("apps", "Deployment", "web", "web");
    for i in 0..100 {
        h.touch("dev", &key, |s| {
            s.insert("readyReplicas".into(), json!(2));
            s.insert("probe".into(), json!(i));
        });
        h.run_for(10);
    }
    let after = h.engine.stats();
    assert_eq!(after.comparisons, before.comparisons);
    assert_eq!(after.live_unchanged - before.live_unchanged, 100);
}

#[test]
fn a_cluster_budget_paces_requests() {
    let (mut h, git) = harness(Config { dry_run_first: false, ..fast() });
    h.input(Input::UpsertCluster { name: "dev".into(), qps: Some(10.0), inflight: None });
    h.input(Input::UpsertApp(auto("web")));
    let objects: Vec<Value> = (0..100).map(|i| config_map(&format!("c{i}"), "1", json!({}))).collect();
    deploy(&mut h, &git, "web", "r1", objects);
    let start = h.now;
    assert!(h.run_until(120_000, |h| settled(h, "web")));
    // 100 applies at 10 a second, after a burst of 20: about 8 s.
    assert!(h.now - start >= 7_000, "took {} ms", h.now - start);
}

#[test]
fn a_manual_sync_applies_and_a_dry_run_changes_nothing() {
    let (mut h, git) = harness(fast());
    h.input(Input::UpsertApp(AppConfig::new("web", "dev", "web")));
    deploy(&mut h, &git, "web", "r1", vec![config_map("cfg", "1", json!({}))]);
    h.run_for(1_000);
    assert_eq!(status(&h, "web").map(|s| s.sync), Some(Sync::OutOfSync));
    h.input(Input::Sync { app: "web".into(), request: SyncRequest { dry_run: true, ..SyncRequest::default() } });
    h.run_for(1_000);
    assert_eq!(h.clusters.get("dev").map(sim::Cluster::len), Some(0));
    h.input(Input::Sync { app: "web".into(), request: SyncRequest::default() });
    assert!(h.run_until(10_000, |h| settled(h, "web")));
}

#[test]
fn create_namespace_makes_one_that_outlives_prunes() {
    let (mut h, git) = harness(fast());
    let mut a = auto("web");
    a.create_namespace = Some((vec![("team".into(), "web".into())], Vec::new()));
    h.input(Input::UpsertApp(a));
    deploy(&mut h, &git, "web", "r1", vec![config_map("cfg", "1", json!({}))]);
    assert!(h.run_until(60_000, |h| settled(h, "web")), "{:?}", status(&h, "web"));
    let ns = ResKey::new("", "Namespace", "", "web");
    let live = h.clusters.get("dev").and_then(|c| c.get(&ns));
    assert_eq!(live.as_ref().and_then(|v| obj::label(v, "team").map(str::to_owned)), Some("web".into()));
    h.input(Input::RemoveApp { id: "web".into(), cascade: Some(crate::model::Propagation::Foreground) });
    h.run_for(5_000);
    assert!(h.clusters.get("dev").and_then(|c| c.get(&ns)).is_some(), "the namespace was deleted with the app");
}

/// The namespace "web", as someone else made it.
fn someone_elses_namespace(h: &mut Harness) {
    let ns = json!({"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": "web", "labels": {"keep": "1"},
        "annotations": {"argocd.argoproj.io/tracking-id": "platform:/Namespace:/web"}}});
    if let Some(c) = h.clusters.get_mut("dev") {
        let _ = c.apply(0, ns, "someone", false);
    }
}

fn web_namespace(h: &Harness) -> Option<Value> {
    h.clusters.get("dev").and_then(|c| c.get(&ResKey::new("", "Namespace", "", "web")))
}

/// Argo CD's CreateNamespace: the namespace is no object of the app's. Already there, it is
/// neither compared nor counted, and a sync leaves it as it is.
#[test]
fn a_namespace_an_app_makes_is_never_compared() {
    let (mut h, git) = harness(fast());
    someone_elses_namespace(&mut h);
    let before = web_namespace(&h);
    let mut a = auto("web");
    a.create_namespace = Some((Vec::new(), Vec::new()));
    h.input(Input::UpsertApp(a));
    deploy(&mut h, &git, "web", "r1", vec![config_map("cfg", "1", json!({}))]);
    assert!(h.run_until(60_000, |h| settled(h, "web")), "{:?}", status(&h, "web"));
    let s = status(&h, "web");
    assert_eq!(s.map(|s| (s.resources, s.out_of_sync, s.shared)), Some((1, 0, 0)), "{s:?}");
    assert_eq!(web_namespace(&h).map(|v| (v.pointer("/metadata/labels").cloned(), v.pointer("/metadata/annotations").cloned())), before.map(|v| (v.pointer("/metadata/labels").cloned(), v.pointer("/metadata/annotations").cloned())), "the sync left it alone");
    assert!(h.engine.resources("web").iter().all(|r| &*r.key.kind != "Namespace"), "not one of its objects");
}

/// Missing, it is made, with none of kiln's marks: no tracking, no wave, no options.
#[test]
fn a_missing_namespace_is_made_without_kilns_marks() {
    let (mut h, git) = harness(fast());
    let mut a = auto("web");
    a.create_namespace = Some((Vec::new(), Vec::new()));
    h.input(Input::UpsertApp(a));
    deploy(&mut h, &git, "web", "r1", vec![config_map("cfg", "1", json!({}))]);
    assert!(h.run_until(60_000, |h| settled(h, "web")), "{:?}", status(&h, "web"));
    let ns = web_namespace(&h);
    assert!(ns.is_some(), "made");
    let annotations = ns.as_ref().and_then(|v| v.pointer("/metadata/annotations")).and_then(Value::as_object).map(|m| m.keys().cloned().collect::<Vec<_>>()).unwrap_or_default();
    assert!(annotations.iter().all(|k| !k.contains("tracking") && !k.contains("sync-options") && !k.contains("sync-wave")), "{annotations:?}");
    assert_eq!(status(&h, "web").map(|s| s.resources), Some(1));
}

/// managedNamespaceMetadata: applied to the namespace at every sync, beside what others set.
#[test]
fn managed_namespace_metadata_is_applied_to_one_already_there() {
    let (mut h, git) = harness(fast());
    someone_elses_namespace(&mut h);
    let mut a = auto("web");
    a.create_namespace = Some((vec![("team".into(), "web".into())], vec![("owner".into(), "ada".into())]));
    a.manage_namespace = true;
    h.input(Input::UpsertApp(a));
    deploy(&mut h, &git, "web", "r1", vec![config_map("cfg", "1", json!({}))]);
    assert!(h.run_until(60_000, |h| settled(h, "web")), "{:?}", status(&h, "web"));
    let ns = web_namespace(&h).unwrap_or_default();
    assert_eq!((obj::label(&ns, "team"), obj::label(&ns, "keep"), obj::annotation(&ns, "owner")), (Some("web"), Some("1"), Some("ada")));
    assert!(obj::annotation(&ns, "argocd.argoproj.io/sync-options").is_none());
}

/// Another app deploys the namespace as one of its objects: it is that app's alone, not shared
/// with the app that makes it, and that app prunes it once git drops it.
#[test]
fn a_namespace_another_app_deploys_is_not_shared_with_one_that_makes_it() {
    let (mut h, git) = harness(fast());
    h.input(Input::UpsertApp(auto("namespaces")));
    deploy(&mut h, &git, "namespaces", "n1", vec![json!({"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": "web"}})]);
    assert!(h.run_until(60_000, |h| settled(h, "namespaces")), "{:?}", status(&h, "namespaces"));
    let mut a = auto("web");
    a.create_namespace = Some((Vec::new(), Vec::new()));
    h.input(Input::UpsertApp(a));
    deploy(&mut h, &git, "web", "w1", vec![config_map("cfg", "1", json!({}))]);
    assert!(h.run_until(60_000, |h| settled(h, "web")), "{:?}", status(&h, "web"));
    assert_eq!((status(&h, "web").map(|s| s.shared), status(&h, "namespaces").map(|s| s.shared)), (Some(0), Some(0)));
    assert!(h.engine.resources("namespaces").iter().all(|r| r.shared_with.is_empty()));
    // Dropped from git: the app that deploys it prunes it; the one that makes it doesn't hold it.
    deploy(&mut h, &git, "namespaces", "n2", vec![config_map("marker", "1", json!({}))]);
    assert!(h.run_until(30_000, |h| web_namespace(h).is_none()), "{:?}", h.engine.resources("namespaces"));
}

#[test]
fn shadow_mode_compares_and_never_writes() {
    let (mut h, git) = harness(fast());
    let mut a = auto("web");
    a.read_only = true;
    h.input(Input::UpsertApp(a));
    deploy(&mut h, &git, "web", "r1", vec![config_map("cfg", "1", json!({}))]);
    h.run_for(10_000);
    assert_eq!(status(&h, "web").map(|s| s.sync), Some(Sync::OutOfSync));
    h.input(Input::Sync { app: "web".into(), request: SyncRequest::default() });
    h.run_for(10_000);
    assert_eq!(h.clusters.get("dev").map(sim::Cluster::len), Some(0));
}

#[test]
fn objects_argo_cd_deployed_are_found_by_its_tracking_name() {
    let (mut h, git) = harness(fast());
    // Argo CD deployed "web" (tracking-id names the Application, not kiln's project/name id).
    let cm = json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "cfg", "namespace": "web",
        "annotations": {"argocd.argoproj.io/tracking-id": "web:/ConfigMap:web/cfg"}}, "data": {"v": "1"}});
    if let Some(c) = h.clusters.get_mut("dev") {
        let _ = c.apply(0, cm, "argocd-controller", false);
    }
    let mut a = AppConfig::new("default/web", "dev", "web");
    a.tracking = crate::model::Tracking::ArgoAnnotation;
    a.tracking_name = Some("web".into());
    a.read_only = true;
    h.input(Input::UpsertApp(a));
    deploy(&mut h, &git, "default/web", "r1", vec![config_map("cfg", "1", json!({}))]);
    h.run_for(5_000);
    let s = status(&h, "default/web");
    assert_eq!(s.map(|s| (s.sync, s.resources, s.out_of_sync)), Some((Sync::Synced, 1, 0)), "{s:?}");
}

/// A shadow compares what Argo CD deployed: removing it, even "with its objects", deletes none.
#[test]
fn removing_a_shadow_deletes_nothing_it_compared() {
    let (mut h, git) = harness(fast());
    let cm = json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "cfg", "namespace": "web",
        "annotations": {"argocd.argoproj.io/tracking-id": "web:/ConfigMap:web/cfg"}}, "data": {"v": "1"}});
    if let Some(c) = h.clusters.get_mut("dev") {
        let _ = c.apply(0, cm, "argocd-controller", false);
    }
    let mut a = AppConfig::new("default/web", "dev", "web");
    a.tracking = crate::model::Tracking::ArgoAnnotation;
    a.tracking_name = Some("web".into());
    a.read_only = true;
    h.input(Input::UpsertApp(a));
    deploy(&mut h, &git, "default/web", "r1", vec![config_map("cfg", "1", json!({}))]);
    h.run_for(5_000);
    assert_eq!(status(&h, "default/web").map(|s| s.sync), Some(Sync::Synced), "its tracking names it: the object is its own");
    h.input(Input::RemoveApp { id: "default/web".into(), cascade: Some(crate::model::Propagation::Foreground) });
    h.run_for(10_000);
    assert!(present(&h, "cfg"), "a shadow never writes, also as it goes");
}

#[test]
fn crds_deployed_by_argo_cd_carry_no_tracking_and_still_compare_synced() {
    let (mut h, git) = harness(fast());
    let crd = json!({"apiVersion": "apiextensions.k8s.io/v1", "kind": "CustomResourceDefinition", "metadata": {"name": "widgets.example.com"},
        "spec": {"group": "example.com", "names": {"kind": "Widget", "plural": "widgets"}, "scope": "Namespaced", "versions": [{"name": "v1", "served": true, "storage": true}]}});
    if let Some(c) = h.clusters.get_mut("dev") {
        let _ = c.apply(0, crd.clone(), "argocd-controller", false);
    }
    let mut a = AppConfig::new("default/crds", "dev", "web");
    a.tracking = crate::model::Tracking::ArgoAnnotation;
    a.tracking_name = Some("crds".into());
    a.read_only = true;
    h.input(Input::UpsertApp(a));
    deploy(&mut h, &git, "default/crds", "r1", vec![crd]);
    h.run_for(5_000);
    let s = status(&h, "default/crds");
    assert_eq!(s.map(|s| (s.sync, s.out_of_sync)), Some((Sync::Synced, 0)), "{s:?}");
}

#[test]
fn a_sync_creates_its_namespace_before_validating_what_goes_in_it() {
    let (mut h, git) = harness(Config { wave_delay_ms: 0, ..Config::default() });
    if let Some(c) = h.clusters.get_mut("dev") {
        c.strict_namespaces = true;
    }
    let mut a = auto("web");
    a.create_namespace = Some((Vec::new(), Vec::new()));
    h.input(Input::UpsertApp(a));
    deploy(&mut h, &git, "web", "r1", vec![config_map("cfg", "1", json!({})), deployment("web", "web:1", json!({}))]);
    assert!(h.run_until(60_000, |h| settled(h, "web")), "{:?}", status(&h, "web"));
}

/// The steps of a plan, as `kind phase name`.
fn steps(plan: &crate::engine::Plan) -> Vec<String> {
    plan.steps.iter().map(|s| format!("{} {} {}", s.kind.as_str(), s.phase.as_str(), s.key.name)).collect()
}

#[test]
fn a_plan_says_what_a_sync_of_another_render_would_do_and_changes_nothing() {
    let (mut h, git) = harness(fast());
    h.input(Input::UpsertApp(auto("web")));
    deploy(&mut h, &git, "web", "r1", vec![config_map("cfg", "1", json!({})), deployment("web", "web:1", json!({})), service("web")]);
    assert!(h.run_until(60_000, |h| settled(h, "web")));
    let records = h.records.len();
    // The next commit changes the ConfigMap, adds one, leaves the Deployment, drops the Service,
    // and runs a migration first.
    let next = vec![config_map("cfg", "2", json!({})), config_map("extra", "1", json!({})), deployment("web", "web:1", json!({})), job("migrate", "PreSync")];
    let plan = h.engine.plan("web", Some(next), &FxHashMap::default()).unwrap_or_default();
    assert_eq!(steps(&plan), ["hook PreSync migrate", "update Sync cfg", "create Sync extra", "prune Sync web"]);
    assert_eq!((plan.unchanged, plan.held.len(), plan.unread.len(), plan.shared.len()), (1, 0, 0, 0));
    let cfg = plan.steps.iter().find(|s| &*s.key.name == "cfg").map(|s| &s.change);
    let data = |v: Option<&Value>| v.and_then(|v| v.pointer("/data/v")).cloned();
    assert_eq!(cfg.map(|c| (data(c.before.as_ref()), data(c.after.as_ref()))), Some((Some(json!("1")), Some(json!("2")))));
    assert!(plan.steps.iter().find(|s| s.kind == crate::engine::StepKind::Prune).is_some_and(|s| s.change.after.is_none()));
    // Every object as the app applies it: in the app's namespace, with its tracking.
    let names: Vec<(&str, &str, bool)> = plan.objects.iter().map(|p| (&*p.key.name, &*p.key.namespace, p.hook)).collect();
    assert_eq!(names, [("cfg", "web", false), ("extra", "web", false), ("web", "web", false), ("migrate", "web", true)]);
    assert!(plan.objects.iter().all(|p| obj::label(&p.object, crate::engine::APP_LABEL) == Some(crate::engine::app_label("web").as_str())));
    // Planning asked nothing of the cluster and started nothing.
    h.run_for(5_000);
    assert_eq!(h.records.len(), records);
    assert!(settled(&h, "web"));
}

#[test]
fn a_plan_keeps_what_prune_false_keeps_and_shows_drift_for_the_render_deployed() {
    let (mut h, git) = harness(fast());
    let mut a = auto("web");
    a.sync.self_heal = false;
    h.input(Input::UpsertApp(a));
    let keep = json!({"argocd.argoproj.io/sync-options": "Prune=false"});
    deploy(&mut h, &git, "web", "r1", vec![config_map("cfg", "1", keep), deployment("web", "web:1", json!({}))]);
    assert!(h.run_until(60_000, |h| settled(h, "web")));
    let plan = h.engine.plan("web", Some(vec![deployment("web", "web:1", json!({}))]), &FxHashMap::default()).unwrap_or_default();
    assert!(plan.steps.is_empty(), "{:?}", steps(&plan));
    let held: Vec<(&str, crate::model::PruneMode)> = plan.held.iter().map(|(k, m)| (&*k.name, *m)).collect();
    assert_eq!(held, [("cfg", crate::model::PruneMode::Never)]);
    // The render deployed now, planned: nothing to do until the cluster drifts.
    assert!(h.engine.plan("web", None, &FxHashMap::default()).is_some_and(|p| p.steps.is_empty() && p.unchanged == 2));
    let key = ResKey::new("apps", "Deployment", "web", "web");
    h.edit("dev", &key, |v| {
        if let Some(c) = v.pointer_mut("/spec/template/spec/containers/0").and_then(Value::as_object_mut) {
            c.insert("image".into(), json!("evil:1"));
        }
    });
    h.run_for(5_000);
    let drift = h.engine.plan("web", None, &FxHashMap::default()).unwrap_or_default();
    assert_eq!(steps(&drift), ["update Sync web"]);
    assert_eq!(drift.steps.first().and_then(|s| s.why.clone()).as_deref(), Some(".spec.template.spec.containers[name=app].image differs"));
    assert!(h.engine.plan("nope", None, &FxHashMap::default()).is_none());
}

#[test]
fn a_plan_reads_kinds_the_engine_has_not_listed_from_what_the_owner_read() {
    let (mut h, git) = harness(fast());
    h.input(Input::UpsertApp(auto("web")));
    deploy(&mut h, &git, "web", "r1", vec![deployment("web", "web:1", json!({}))]);
    assert!(h.run_until(60_000, |h| settled(h, "web")));
    // The next commit adds a ConfigMap and a Service: kinds this app never had, so never listed.
    let next = vec![deployment("web", "web:1", json!({})), config_map("cfg", "1", json!({})), service("web")];
    let plan = h.engine.plan("web", Some(next.clone()), &FxHashMap::default()).unwrap_or_default();
    assert_eq!(plan.unread.iter().map(|k| k.kind.to_string()).collect::<Vec<_>>(), ["ConfigMap", "Service"]);
    assert!(plan.steps.is_empty());
    // Read by the owner: the ConfigMap isn't there; a Service someone made by hand is.
    let mut by_hand = service("web");
    if let Some(m) = by_hand.pointer_mut("/metadata").and_then(Value::as_object_mut) {
        m.insert("namespace".into(), json!("web"));
    }
    if let Some(p) = by_hand.pointer_mut("/spec/ports/0").and_then(Value::as_object_mut) {
        p.insert("port".into(), json!(8080));
    }
    let mut read = FxHashMap::default();
    read.insert(ResKey::new("", "ConfigMap", "web", "cfg"), None);
    read.insert(ResKey::new("", "Service", "web", "web"), Some(by_hand));
    let plan = h.engine.plan("web", Some(next), &read).unwrap_or_default();
    assert!(plan.unread.is_empty());
    assert_eq!(steps(&plan), ["create Sync cfg", "update Sync web"]);
    assert_eq!(plan.unchanged, 1);
}

/// A ConfigMap Argo CD deployed for its Application `kps`.
fn argo_config_map(name: &str) -> Value {
    json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": name, "namespace": "web",
        "annotations": {"argocd.argoproj.io/tracking-id": format!("kps:/ConfigMap:web/{name}")}}, "data": {"v": "1"}})
}

/// An app adopted from Argo CD, shadowed: kiln compares it and never writes.
fn shadow(id: &str, tracking: &str) -> AppConfig {
    let mut a = AppConfig::new(id, "dev", "web");
    a.tracking = crate::model::Tracking::ArgoAnnotation;
    a.tracking_name = Some(tracking.into());
    a.read_only = true;
    a
}

/// What an app's status counts: sync, resources, differing, shared.
fn counts(h: &Harness, id: &str) -> Option<(Sync, u32, u32, u32)> {
    status(h, id).map(|s| (s.sync, s.resources, s.out_of_sync, s.shared))
}

fn present(h: &Harness, name: &str) -> bool {
    h.clusters.get("dev").and_then(|c| c.get(&ResKey::new("", "ConfigMap", "web", name))).is_some()
}

/// Taking an app over from Argo CD: Argo CD's app (shadowed) and kiln's own deploy the same
/// objects. Each compares all of them, whichever renders first and however often; kiln's shows
/// them as Argo CD's until a person's sync takes them over; nothing is pruned either way.
#[test]
fn two_apps_deploying_the_same_objects_each_compare_all_of_them() {
    let (mut h, git) = harness(fast());
    let names = ["a", "b", "c"];
    if let Some(c) = h.clusters.get_mut("dev") {
        for n in names {
            let _ = c.apply(0, argo_config_map(n), "argocd-controller", false);
        }
    }
    let objects: Vec<Value> = names.iter().map(|n| config_map(n, "1", json!({}))).collect();
    h.input(Input::UpsertApp(shadow("default/kps", "kps")));
    h.input(Input::UpsertApp(AppConfig::new("default/kps-production", "dev", "web")));
    // Ours renders first, then Argo CD's, then ours again at a new revision.
    deploy(&mut h, &git, "default/kps-production", "p1", objects.clone());
    h.run_for(5_000);
    deploy(&mut h, &git, "default/kps", "s1", objects.clone());
    h.run_for(5_000);
    deploy(&mut h, &git, "default/kps-production", "p2", objects.clone());
    h.run_for(5_000);
    assert_eq!(counts(&h, "default/kps"), Some((Sync::Synced, 3, 0, 0)));
    assert_eq!(counts(&h, "default/kps-production"), Some((Sync::OutOfSync, 3, 3, 3)));
    let views = h.engine.resources("default/kps-production");
    assert!(
        views.iter().all(|r| r.tracked_by.as_deref() == Some("default/kps") && r.shared_with.iter().any(|x| &**x == "default/kps") && !r.requires_pruning),
        "{views:?}"
    );
    assert!(views.iter().all(|r| r.why.as_deref().is_some_and(|w| w.contains("default/kps's tracking"))), "{views:?}");
    assert!(views.iter().all(|r| r.marks_only), "only the tracking marks would change: {views:?}");
    assert_eq!(status(&h, "default/kps-production").map(|s| (s.shared, s.unmarked)), Some((3, 0)), "taken over, not merely unmarked");
    // A real change in one of them isn't a matter of marks.
    deploy(&mut h, &git, "default/kps-production", "p3", vec![config_map("a", "2", json!({})), config_map("b", "1", json!({})), config_map("c", "1", json!({}))]);
    h.run_for(5_000);
    let a = h.engine.resources("default/kps-production").into_iter().find(|r| &*r.key.name == "a");
    assert!(a.as_ref().is_some_and(|r| !r.marks_only && r.tracked_by.is_some()), "{a:?}");

    deploy(&mut h, &git, "default/kps-production", "p4", objects.clone());
    h.run_for(5_000);
    // A person syncs ours: it takes them over. Argo CD's now sees ours on them; nothing is pruned.
    h.input(Input::Sync { app: "default/kps-production".into(), request: SyncRequest::default() });
    h.run_for(10_000);
    assert_eq!(counts(&h, "default/kps-production"), Some((Sync::Synced, 3, 0, 0)));
    assert_eq!(counts(&h, "default/kps"), Some((Sync::OutOfSync, 3, 3, 3)));
    assert_eq!(h.clusters.get("dev").map(sim::Cluster::len), Some(3));
}

/// An app prunes what carries its tracking only while no other app deploys it.
#[test]
fn an_app_never_prunes_what_another_app_deploys() {
    let (mut h, git) = harness(fast());
    h.input(Input::UpsertApp(auto("one")));
    deploy(&mut h, &git, "one", "o1", vec![config_map("x", "1", json!({})), config_map("y", "1", json!({}))]);
    assert!(h.run_until(30_000, |h| settled(h, "one")));
    h.input(Input::UpsertApp(AppConfig::new("two", "dev", "web")));
    deploy(&mut h, &git, "two", "t1", vec![config_map("x", "1", json!({}))]);
    deploy(&mut h, &git, "one", "o2", vec![config_map("y", "1", json!({}))]);
    h.run_for(30_000);
    assert!(present(&h, "x"), "one doesn't prune what two deploys");
    assert_eq!(counts(&h, "one").map(|c| (c.0, c.2)), Some((Sync::Synced, 0)));
    // Two drops it too: now it is one's to prune, and one's automatic sync does.
    deploy(&mut h, &git, "two", "t2", vec![]);
    h.run_for(30_000);
    assert!(!present(&h, "x"), "pruned once nobody deploys it");
    assert!(present(&h, "y"));
}

/// Deleting an app with its objects leaves those another app deploys.
#[test]
fn deleting_an_app_leaves_what_another_app_deploys() {
    let (mut h, git) = harness(fast());
    h.input(Input::UpsertApp(auto("one")));
    deploy(&mut h, &git, "one", "o1", vec![config_map("x", "1", json!({})), config_map("y", "1", json!({}))]);
    assert!(h.run_until(30_000, |h| settled(h, "one")));
    h.input(Input::UpsertApp(AppConfig::new("two", "dev", "web")));
    deploy(&mut h, &git, "two", "t1", vec![config_map("x", "1", json!({}))]);
    h.run_for(5_000);
    assert_eq!(counts(&h, "two").map(|c| c.3), Some(1), "x carries one's tracking");
    h.input(Input::RemoveApp { id: "one".into(), cascade: Some(crate::model::Propagation::Foreground) });
    h.run_for(10_000);
    assert!(present(&h, "x"), "two deploys x");
    assert!(!present(&h, "y"), "y was one's alone");
    // The tracking on x names nobody now: x is two's to sync, shared with nobody.
    assert_eq!(counts(&h, "two").map(|c| (c.0, c.3)), Some((Sync::OutOfSync, 0)));
}

/// Argo CD's FailOnSharedResource: a sync fails before it applies anything when another app's
/// tracking is on something it deploys.
#[test]
fn fail_on_shared_resource_fails_a_sync_before_it_applies_anything() {
    let (mut h, git) = harness(fast());
    h.input(Input::UpsertApp(auto("one")));
    deploy(&mut h, &git, "one", "o1", vec![config_map("x", "1", json!({}))]);
    assert!(h.run_until(30_000, |h| settled(h, "one")));
    let mut two = AppConfig::new("two", "dev", "web");
    two.sync.options.fail_on_shared_resource = Some(true);
    h.input(Input::UpsertApp(two));
    deploy(&mut h, &git, "two", "t1", vec![config_map("x", "2", json!({}))]);
    h.run_for(5_000);
    h.input(Input::Sync { app: "two".into(), request: SyncRequest::default() });
    h.run_for(10_000);
    let op = status(&h, "two").and_then(|s| s.operation.clone());
    assert!(op.as_ref().is_some_and(|o| o.phase == OpPhase::Failed && o.message.contains("Shared resource found: ConfigMap/x is part of applications two and one")), "{op:?}");
    let x = h.clusters.get("dev").and_then(|c| c.get(&ResKey::new("", "ConfigMap", "web", "x")));
    assert_eq!(x.as_ref().and_then(|v| v.pointer("/data/v")), Some(&json!("1")), "untouched");
}

/// Two apps syncing automatically never take an object from each other: a person decides.
#[test]
fn automatic_syncs_never_take_an_object_from_another_app() {
    let (mut h, git) = harness(fast());
    h.input(Input::UpsertApp(auto("one")));
    deploy(&mut h, &git, "one", "o1", vec![config_map("x", "1", json!({}))]);
    assert!(h.run_until(30_000, |h| settled(h, "one")));
    h.input(Input::UpsertApp(auto("two")));
    deploy(&mut h, &git, "two", "t1", vec![config_map("x", "1", json!({}))]);
    h.run_for(120_000);
    let syncs = |h: &Harness, app: &str| h.records.iter().filter(|r| matches!(r, Record::Started { app: a, .. } if &**a == app)).count();
    assert_eq!(syncs(&h, "two"), 0, "two waits for a person");
    assert_eq!(counts(&h, "two").map(|c| (c.0, c.3)), Some((Sync::OutOfSync, 1)));
    // A person syncs two: it has x now, and one's self-heal doesn't take it back.
    h.input(Input::Sync { app: "two".into(), request: SyncRequest::default() });
    h.run_for(120_000);
    assert_eq!((syncs(&h, "one"), syncs(&h, "two")), (1, 1));
    assert_eq!(counts(&h, "one").map(|c| (c.0, c.3)), Some((Sync::OutOfSync, 1)));
}

/// An object the app deploys that has no tracking at all (made by hand, or a CRD Argo CD
/// leaves unmarked) differs only by kiln's marks: counted apart from changes.
#[test]
fn objects_without_tracking_differ_only_by_kilns_marks() {
    let (mut h, git) = harness(fast());
    if let Some(c) = h.clusters.get_mut("dev") {
        let _ = c.apply(0, json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "x", "namespace": "web"}, "data": {"v": "1"}}), "kubectl", false);
    }
    h.input(Input::UpsertApp(AppConfig::new("one", "dev", "web")));
    deploy(&mut h, &git, "one", "o1", vec![config_map("x", "1", json!({})), config_map("y", "1", json!({}))]);
    h.run_for(5_000);
    let s = status(&h, "one").map(|s| (s.out_of_sync, s.missing, s.shared, s.unmarked));
    assert_eq!(s, Some((2, 1, 0, 1)), "y missing, x only unmarked");
    let x = h.engine.resources("one").into_iter().find(|r| &*r.key.name == "x");
    assert!(x.is_some_and(|r| r.marks_only && r.tracked_by.is_none()));
}


/// A sync of objects already there and healthy applies every one of them, however many wait in
/// the cluster's queue: a task waits for its own answer, not for the live object it is about to
/// change (which is the one from before, and healthy).
#[test]
fn a_sync_of_objects_already_there_applies_every_one() {
    let (mut h, git) = harness(fast());
    h.input(Input::UpsertCluster { name: "dev".into(), qps: None, inflight: Some(2) });
    let cms: Vec<Value> = (0..12).map(|i| config_map(&format!("cfg{i}"), "1", json!({}))).collect();
    for i in 0..12 {
        let cm = json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": format!("cfg{i}"), "namespace": "web"}, "data": {"v": "1"}});
        if let Some(c) = h.clusters.get_mut("dev") {
            let _ = c.apply(0, cm, "someone", false);
        }
    }
    h.input(Input::UpsertApp(AppConfig::new("web", "dev", "web")));
    deploy(&mut h, &git, "web", "r1", cms);
    h.run_for(5_000);
    let n = h.records.len();
    h.input(Input::Sync { app: "web".into(), request: SyncRequest::default() });
    assert!(h.run_until(60_000, |h| settled(h, "web")), "{:?}", status(&h, "web"));
    let marked = (0..12)
        .filter(|i| h.clusters.get("dev").and_then(|c| c.get(&ResKey::new("", "ConfigMap", "web", &format!("cfg{i}")))).is_some_and(|v| obj::annotation(&v, crate::engine::TRACKING).is_some()))
        .count();
    assert_eq!(marked, 12, "every object applied");
    let applied = h.records.iter().skip(n).find_map(|r| if let Record::Finished { applied, .. } = r { Some(*applied) } else { None });
    assert_eq!(applied, Some(12));
}
