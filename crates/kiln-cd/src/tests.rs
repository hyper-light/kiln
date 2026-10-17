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
    h.input(Input::Inputs { app: app.into(), revision: rev.into(), key });
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
        Record::Finished { key: Some(k), succeeded, .. } => Some((*k, *succeeded)),
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
    if let Some((key, succeeded)) = last {
        h2.input(Input::Remember { app: "web".into(), key, succeeded });
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
        h.input(Input::Inputs { app: format!("web-{c}").into(), revision: "r1".into(), key: 7 });
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
