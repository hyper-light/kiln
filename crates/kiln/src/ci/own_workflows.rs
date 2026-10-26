//! W26 step 3 (docs/ci.md W02-R08, W05-R21, W13-R09): kiln's own workflows on the engine. Each
//! `.kiln/*.yaml` is checked in the kiln dialect, matched against its own trigger, planned for it
//! as the server plans a run, and each of its legs routed as the scheduler routes one: to the
//! `k8s` pool production is to have, else to one of production's runners
//! (`tests/fixtures/production-runners.json`: `ci.runners.list` on 2026-10-25, their labels and
//! facts, without v1's pods for one job each, which come and go). Its `runs-on` labels are free
//! of `KCI6007` against the same. `deploy.yaml` and `environments.yaml` are KilnCD's files.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use kiln_ci::expr::Dialect;
use kiln_ci::facts::RunnerFacts;
use kiln_ci::plan::{NoSource, RunInput, RunPlan, Settings};
use kiln_ci::sched::pools::PoolSpec;
use kiln_ci::sched::routing::{self, RunnerView};
use kiln_ci::trigger::{Event, Source, context};
use kiln_ci::wf::{self, FileKind};
use serde::Deserialize;
use serde_json::json;

use crate::store::AgentRow;

const REPO: &str = "hyper-light/kiln";
const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

/// Each workflow, and the event and branch it's planned for: its own trigger.
const WORKFLOWS: &[(&str, &str, &str)] = &[
    ("bench.yaml", "push", "main"),
    ("ci.yaml", "push", "main"),
    ("e2e.yaml", "push", "e2e/check"),
    ("image.yaml", "push", "main"),
    ("release.yaml", "push", "main"),
    ("sim.yaml", "workflow_dispatch", "main"),
    ("stress.yaml", "workflow_dispatch", "main"),
    ("wire-lab.yaml", "workflow_dispatch", "main"),
];

/// KilnCD's files in `.kiln`, not workflows.
const CD_FILES: &[&str] = &["deploy.yaml", "environments.yaml"];

/// A runner as `ci.runners.list` gives it.
#[derive(Deserialize)]
struct Runner {
    name: String,
    labels: Vec<String>,
    os: String,
    arch: String,
    slots: u32,
    connected: bool,
    facts: Option<RunnerFacts>,
}

fn production_runners() -> Result<Vec<Runner>, String> {
    serde_json::from_str(include_str!("../../tests/fixtures/production-runners.json")).map_err(|e| format!("production-runners.json: {e}"))
}

/// The pool production's change adds (W26 step 3's plan): Kubernetes pods for `[k8s]` legs.
fn k8s_pool() -> PoolSpec {
    PoolSpec { name: "k8s".into(), executor: "k8s".into(), labels: vec!["k8s".into()], max: Some(64), ..PoolSpec::default() }
}

/// A runner as the scheduler sees it (`runner_view`): its facts, and what this build's runner
/// announces, Docker where it has it.
fn view(r: &Runner) -> RunnerView {
    let mut facts = r.facts.clone().unwrap_or_else(|| RunnerFacts { os: r.os.clone(), arch: r.arch.clone(), ..RunnerFacts::default() });
    facts.executors.entry("host".to_owned()).or_default();
    let docker = if facts.executors.contains_key("docker") { crate::proto::CAP_DOCKER } else { 0 };
    RunnerView { name: r.name.clone(), labels: r.labels.clone(), pool: None, facts, caps: u64::from(crate::updates::RUNNER_CAPS | docker) }
}

/// A runner as the store keeps it, for `KCI6007`.
fn row(r: &Runner) -> AgentRow {
    AgentRow { name: r.name.clone(), labels: r.labels.clone(), slots: r.slots, hostname: r.name.clone(), os: r.os.clone(), arch: r.arch.clone(), version: String::new(), ephemeral: false, connected: r.connected, last_seen: 0, enrolled_at: 0 }
}

/// What's wrong with `text` as `.kiln/{file}`, planned for `event` on `branch` against `runners`
/// and `pools`: its errors and `KCI6007`s, a trigger it doesn't run on, a plan that can't be made
/// or a job it fails, and each leg no pool serves and no runner may take.
fn problems(file: &str, text: &str, event: &str, branch: &str, runners: &[Runner], pools: &[PoolSpec]) -> Vec<String> {
    let path = format!(".kiln/{file}");
    let mut out = Vec::new();
    let (checked, model) = wf::check_model(&path, text, None);
    if checked.kind != "workflow" || checked.dialect != Dialect::Kiln {
        out.push(format!("{path}: read as a {} in {:?}, not a kiln-dialect workflow", checked.kind, checked.dialect));
    }
    out.extend(checked.diagnostics.iter().filter(|d| d.is_error()).map(|d| d.render(&path)));
    let Some(w) = model else {
        out.push(format!("{path}: no model"));
        return out;
    };
    let rules = super::routing::defaults(false);
    let rows: Vec<AgentRow> = runners.iter().map(row).collect();
    out.extend(super::workflows::unknown_in(&w, REPO, &rules, &rows, pools).iter().map(|d| d.render(&path)));
    let e = Event {
        name: event.into(),
        repo: REPO.into(),
        git_ref: format!("refs/heads/{branch}"),
        sha: SHA.into(),
        actor: "adalundhe".into(),
        trusted: true,
        source: Source::Webhook,
        payload: json!({ "repository": { "full_name": REPO, "default_branch": "main" } }),
        changed: Some(vec!["crates/kiln/src/lib.rs".into()]),
        ..Event::default()
    };
    if let Err(why) = kiln_ci::trigger::matches(&w, &e) {
        out.push(format!("{path}: doesn't run on {event} to {branch}: {why}"));
    }
    let repo = context::Repo { full_name: REPO.into(), default_branch: "main".into(), server_url: "https://github.com".into(), ..context::Repo::default() };
    let run = context::RunFacts { run_id: 1, run_number: 1, run_attempt: 1, workflow: w.name.clone().unwrap_or_else(|| path.clone()), ..context::RunFacts::default() };
    let inputs = match w.on.iter().find(|t| t.event == event && event == "workflow_dispatch") {
        Some(t) => match kiln_ci::trigger::dispatch::inputs(&t.inputs, &serde_json::Map::new()) {
            Ok((typed, _)) => typed,
            Err(why) => {
                out.push(format!("{path}: its inputs: {why}"));
                json!({})
            }
        },
        None => json!({}),
    };
    let input = RunInput { github: context::github(&e, &repo, &run), inputs, vars: json!({}), changed: e.changed.clone().map(Arc::new), trusted: true };
    // As the server plans (`Dispatcher::settings`).
    let settings = Settings { default_runs_on: vec!["k8s".into()], ..Settings::default() };
    let plan = match RunPlan::preview(Arc::new(w), input, settings, &NoSource) {
        Ok(p) => p,
        Err(why) => {
            out.push(format!("{path} ({event}): no plan: {why}"));
            return out;
        }
    };
    out.extend(plan.decided.iter().filter(|d| d.conclusion.failed()).map(|d| format!("{path} ({event}) {}: {}", d.job, d.reason)));
    out.extend(plan.later.iter().map(|j| format!("{path} ({event}) {j}: decided only when its needs finish, so not routed here")));
    if plan.legs.is_empty() {
        out.push(format!("{path} ({event}): plans no legs"));
    }
    let pools: Vec<_> = pools.iter().filter_map(|p| p.pool().map_err(|why| out.push(why)).ok()).collect();
    let views: Vec<RunnerView> = runners.iter().map(view).collect();
    for leg in &plan.legs {
        let route = super::dispatch::route_of(leg, &path, REPO, event, false, None);
        // The scheduler's `pool_for`: the first pool that serves its labels runs it.
        if pools.iter().any(|p| p.serves(&route.labels)) {
            continue;
        }
        match routing::resolve(&route, &rules) {
            Ok(req) if views.iter().any(|v| routing::eligible(&req, v)) => {}
            Ok(req) => {
                let why: Vec<String> = views.iter().map(|v| format!("{} lacks {}", v.name, routing::checks(&req, v).iter().filter(|c| !c.pass).map(|c| c.requirement.as_str()).collect::<Vec<_>>().join(", "))).collect();
                out.push(format!("{path} ({event}): leg `{}` has no pool and no runner: {}", leg.key, why.join("; ")));
            }
            Err(why) => out.push(format!("{path} ({event}): leg `{}` doesn't route: {why}", leg.key)),
        }
    }
    out
}

fn read(file: &str) -> Result<String, String> {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.kiln").join(file);
    std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))
}

/// Every `.kiln` workflow has no errors and no `KCI6007`, runs on its own trigger, plans, and has
/// a pool or one of production's runners for each leg; the CD files are CD's.
#[test]
fn w26_kilns_own_workflows_check_plan_and_route() -> Result<(), String> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.kiln");
    let mut found: Vec<String> = std::fs::read_dir(&dir)
        .map_err(|e| e.to_string())?
        .filter_map(Result::ok)
        .filter(|e| e.path().is_file())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".yaml") || n.ends_with(".yml"))
        .collect();
    found.sort();
    let mut known: Vec<String> = WORKFLOWS.iter().map(|(f, _, _)| (*f).to_owned()).chain(CD_FILES.iter().map(|f| (*f).to_owned())).collect();
    known.sort();
    assert_eq!(found, known, "each file of .kiln is a workflow here with its trigger, or a CD file");
    let runners = production_runners()?;
    assert!(runners.len() >= 8, "production's runners: {}", runners.len());
    let pools = [k8s_pool()];
    let mut all = Vec::new();
    for (file, event, branch) in WORKFLOWS {
        all.extend(problems(file, &read(file)?, event, branch, &runners, &pools));
    }
    for file in CD_FILES {
        let path = format!(".kiln/{file}");
        assert_eq!(wf::file_kind(&path), Some(FileKind::Policy), "{path}");
        let (checked, model) = wf::check_model(&path, &read(file)?, None);
        assert!(checked.kind == "policy" && model.is_none() && checked.diagnostics.is_empty(), "{path}: {checked:?}");
    }
    let envs: BTreeMap<String, kiln_cd::apps::EnvironmentSpec> = serde_yaml::from_str(&read("environments.yaml")?).map_err(|e| format!("environments.yaml: {e}"))?;
    let apps = kiln_cd::apps::from_deploy_file(&read("deploy.yaml")?, "https://github.com/hyper-light/kiln.git", "main", &envs, &kiln_cd::apps::Scope::default()).map_err(|e| format!("deploy.yaml: {}", e.0))?;
    assert!(!apps.is_empty(), "deploy.yaml deploys apps");
    if all.is_empty() { Ok(()) } else { Err(all.join("\n")) }
}

/// The test above fails a workflow that asks for a label no runner or pool serves, on a pod leg
/// and on a bare-metal one, and the `[k8s]` legs without the pool.
#[test]
fn w26_a_label_nothing_serves_fails() -> Result<(), String> {
    let runners = production_runners()?;
    let pools = [k8s_pool()];
    let ci = read("ci.yaml")?;
    for (from, to) in [("runs-on: [k8s]", "runs-on: [k8s, tpu]"), ("runs-on: [baremetal, linux]", "runs-on: [baremetal, linux, tpu]")] {
        assert!(ci.contains(from), "ci.yaml has `{from}`");
        let p = problems("ci.yaml", &ci.replacen(from, to, 1), "push", "main", &runners, &pools);
        assert!(p.iter().any(|x| x.contains("KCI6007") && x.contains("`tpu`")), "{to}: {p:#?}");
        assert!(p.iter().any(|x| x.contains("has no pool and no runner")), "{to}: {p:#?}");
    }
    let p = problems("ci.yaml", &ci, "push", "main", &runners, &[]);
    assert!(p.iter().any(|x| x.contains("KCI6007") && x.contains("`k8s`")) && p.iter().any(|x| x.contains("has no pool and no runner")), "without the pool: {p:#?}");
    Ok(())
}
