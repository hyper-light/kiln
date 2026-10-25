//! Plan tests (docs/ci.md W04-AC03 to W04-AC06, W04-S01 to S05; AC01 and AC02 are in
//! `matrix.rs` and `tests/plan_props.rs`). Each case is written from GitHub's documentation of the
//! behaviour, or from the requirement for kiln's.

use std::collections::HashMap;

use super::*;
use crate::wf::parse_workflow;

fn workflow(src: &str, dialect: Dialect) -> Arc<Workflow> {
    let path = if dialect == Dialect::Gha { ".github/workflows/t.yml" } else { ".kiln/t.yaml" };
    let (w, d) = parse_workflow(path, src, dialect);
    let errors: Vec<String> = d.iter().filter(|x| x.is_error()).map(|x| x.render(path)).collect();
    assert!(errors.is_empty(), "{errors:?}");
    Arc::new(w.unwrap_or_default())
}

#[derive(Default)]
struct Source {
    workflows: HashMap<String, Arc<Workflow>>,
    durations: HashMap<String, f64>,
    files: Vec<String>,
    graph: Option<Arc<Graph>>,
}

impl PlanSource for Source {
    fn workflow(&self, uses: &str) -> Result<Arc<Workflow>, String> {
        self.workflows.get(uses).cloned().ok_or_else(|| format!("no workflow `{uses}`"))
    }
    fn durations(&self, _job: &str, items: &[String]) -> Vec<Option<f64>> {
        items.iter().map(|i| self.durations.get(i).copied()).collect()
    }
    fn files(&self, pattern: &str) -> Vec<String> {
        let p = Patterns::new(&[pattern]).unwrap_or_default();
        self.files.iter().filter(|f| p.matches(f)).cloned().collect()
    }
    fn graph(&self) -> Option<Arc<Graph>> {
        self.graph.clone()
    }
}

/// A run driven by hand: legs are finished as each test says.
struct Run {
    plan: RunPlan,
    src: Source,
    specs: HashMap<String, LegSpec>,
    /// Keys queued, in order.
    queued: Vec<String>,
    cancels: Vec<String>,
    log: Vec<String>,
    warnings: Vec<String>,
    finished: Option<Conclusion>,
}

impl Run {
    fn with(src: &str, dialect: Dialect, input: RunInput, source: Source) -> Run {
        Run::with_settings(src, dialect, input, source, Settings::default())
    }

    fn with_settings(src: &str, dialect: Dialect, input: RunInput, source: Source, settings: Settings) -> Run {
        let started = RunPlan::start(workflow(src, dialect), input, settings, &source);
        assert!(started.is_ok(), "start: {:?}", started.as_ref().err());
        let (plan, acts) = started.unwrap_or_else(|_| (empty_plan(), Vec::new()));
        let mut r = Run { plan, src: source, specs: HashMap::new(), queued: Vec::new(), cancels: Vec::new(), log: Vec::new(), warnings: Vec::new(), finished: None };
        r.apply(acts);
        r
    }

    fn gha(src: &str) -> Run {
        Run::with(src, Dialect::Gha, RunInput { trusted: true, ..RunInput::default() }, Source::default())
    }

    fn kiln(src: &str) -> Run {
        Run::with(src, Dialect::Kiln, RunInput { trusted: true, ..RunInput::default() }, Source::default())
    }

    fn apply(&mut self, acts: Vec<PlanAction>) {
        for a in acts {
            match a {
                PlanAction::Queue(spec) => {
                    self.queued.push(spec.key.clone());
                    self.specs.insert(spec.key.clone(), *spec);
                }
                PlanAction::Settle { leg, conclusion, reason } => {
                    self.log.push(format!("{} {}: {reason}", leg.key, conclusion.result()));
                    self.specs.insert(leg.key.clone(), *leg);
                }
                PlanAction::Job { job, conclusion, reason } => self.log.push(format!("{job} {}: {reason}", conclusion.result())),
                PlanAction::Cancel { leg, .. } => self.cancels.push(leg),
                PlanAction::Warn { message, .. } => self.warnings.push(message),
                PlanAction::Finish(c) => self.finished = Some(c),
            }
        }
    }

    fn finish(&mut self, key: &str, c: Conclusion) {
        self.report(key, LegReport::of(c));
    }

    fn report(&mut self, key: &str, r: LegReport) {
        let acts = self.plan.on_leg_finished(key, r, &self.src);
        self.apply(acts);
    }

    fn outputs(&mut self, key: &str, outputs: &[(&str, &str)]) {
        self.report(key, LegReport { outputs: outputs.iter().map(|(a, b)| ((*a).to_owned(), (*b).to_owned())).collect(), ..LegReport::of(Conclusion::Success) });
    }

    fn cancel(&mut self) {
        let acts = self.plan.on_cancel(&self.src);
        self.apply(acts);
    }

    fn spec(&self, key: &str) -> LegSpec {
        let s = self.specs.get(key).cloned();
        assert!(s.is_some(), "no leg `{key}`: {:?}", self.specs.keys().collect::<Vec<_>>());
        s.unwrap_or_else(dead_spec)
    }

    fn logged(&self, needle: &str) -> bool {
        self.log.iter().any(|l| l.contains(needle))
    }
}

fn empty_plan() -> RunPlan {
    RunPlan {
        name: None,
        concurrency: None,
        input: RunInput::default(),
        github: Value::Null,
        vars: Value::Null,
        settings: Settings::default(),
        scopes: Vec::new(),
        nodes: Vec::new(),
        keys: HashMap::new(),
        called: Vec::new(),
        cancelled: false,
        finished: None,
        seq: 0,
        output_bytes: 0,
        run_env: Vec::new(),
        removed: BTreeMap::new(),
        graph: None,
    }
}

fn dead_spec() -> LegSpec {
    LegSpec {
        key: String::new(),
        job: String::new(),
        job_name: String::new(),
        name: String::new(),
        matrix: serde_json::Value::Null,
        strategy: serde_json::Value::Null,
        needs: serde_json::Value::Null,
        inputs: serde_json::Value::Null,
        runs_on: RunsOnSpec::default(),
        requires: Vec::new(),
        timeout_minutes: 0,
        continue_on_error: false,
        environment: None,
        concurrency: None,
        permissions: perms::Resolved::default(),
        container: None,
        services: Vec::new(),
        secrets: SecretsSpec::None,
        secret_outputs: Vec::new(),
        env_from: Vec::new(),
        run_env: Vec::new(),
        items: Vec::new(),
        emulated: false,
        reads: Vec::new(),
        cache_cap: None,
        unaffected: None,
        defined_in: DefinedIn::default(),
        workflow: Arc::new(Workflow::default()),
        job_index: 0,
    }
}

/// A JSON value by pointer (`/a/b/0`), null when absent.
fn at(v: &serde_json::Value, p: &str) -> serde_json::Value {
    v.pointer(p).cloned().unwrap_or_default()
}

const S: Conclusion = Conclusion::Success;
const F: Conclusion = Conclusion::Failure;

fn jobs(body: &str) -> String {
    format!("on: push\njobs:\n{body}")
}

// ------------------------------------------------------------------ the graph (W04-AC03)

#[test]
fn needs_run_in_order_and_a_failure_skips_dependents() {
    let mut r = Run::gha(&jobs("  a:\n    runs-on: x\n    steps: [{run: a}]\n  b:\n    needs: a\n    runs-on: x\n    steps: [{run: b}]\n  c:\n    needs: b\n    runs-on: x\n    steps: [{run: c}]\n"));
    assert_eq!(r.queued, vec!["a"]);
    r.finish("a", S);
    assert_eq!(r.queued, vec!["a", "b"]);
    r.finish("b", F);
    assert!(r.logged("c skipped: skipped: a job it needs failed"), "{:?}", r.log);
    assert_eq!(r.finished, Some(F));
}

#[test]
fn status_functions_decide_after_a_failure() {
    let body = "  a:\n    runs-on: x\n    steps: [{run: a}]\n  always:\n    needs: a\n    if: always()\n    runs-on: x\n    steps: [{run: b}]\n  failed:\n    needs: a\n    if: failure()\n    runs-on: x\n    steps: [{run: b}]\n  ok:\n    needs: a\n    if: success()\n    runs-on: x\n    steps: [{run: b}]\n  notfailed:\n    needs: a\n    if: ${{ !failure() }}\n    runs-on: x\n    steps: [{run: b}]\n";
    let mut r = Run::gha(&jobs(body));
    r.finish("a", F);
    assert!(r.queued.contains(&"always".to_owned()) && r.queued.contains(&"failed".to_owned()));
    assert!(r.logged("ok skipped") && r.logged("notfailed skipped"), "{:?}", r.log);
}

#[test]
fn failure_is_seen_through_transitive_needs() {
    let body = "  a:\n    runs-on: x\n    steps: [{run: a}]\n  b:\n    needs: a\n    if: always()\n    runs-on: x\n    steps: [{run: b}]\n  c:\n    needs: b\n    if: failure()\n    runs-on: x\n    steps: [{run: c}]\n";
    let mut r = Run::gha(&jobs(body));
    r.finish("a", F);
    r.finish("b", S);
    assert!(r.queued.contains(&"c".to_owned()), "c sees a's failure through b: {:?}", r.log);
}

#[test]
fn a_skipped_need_skips_unless_the_condition_lets_it_run() {
    let body = "  a:\n    if: false\n    runs-on: x\n    steps: [{run: a}]\n  plain:\n    needs: a\n    runs-on: x\n    steps: [{run: b}]\n  notfailed:\n    needs: a\n    if: ${{ !failure() && !cancelled() }}\n    runs-on: x\n    steps: [{run: b}]\n  always:\n    needs: a\n    if: always()\n    runs-on: x\n    steps: [{run: b}]\n";
    let mut r = Run::gha(&jobs(body));
    assert!(r.logged("a skipped") && r.logged("plain skipped: skipped: a job it needs was skipped"), "{:?}", r.log);
    assert!(r.queued.contains(&"notfailed".to_owned()) && r.queued.contains(&"always".to_owned()));
    r.finish("notfailed", S);
    r.finish("always", S);
    assert_eq!(r.finished, Some(S), "skipped jobs don't fail a run");
}

#[test]
fn continue_on_error_keeps_the_run_and_dependents_going() {
    let mut r = Run::gha(&jobs("  a:\n    continue-on-error: true\n    runs-on: x\n    steps: [{run: a}]\n  b:\n    needs: a\n    runs-on: x\n    steps: [{run: b}]\n"));
    r.finish("a", F);
    assert!(r.queued.contains(&"b".to_owned()));
    r.finish("b", S);
    assert_eq!(r.finished, Some(S));
}

#[test]
fn cancelling_runs_only_what_still_holds() {
    let body = "  a:\n    runs-on: x\n    steps: [{run: a}]\n  b:\n    needs: a\n    runs-on: x\n    steps: [{run: b}]\n  cleanup:\n    needs: a\n    if: always()\n    runs-on: x\n    steps: [{run: c}]\n  oncancel:\n    needs: a\n    if: cancelled()\n    runs-on: x\n    steps: [{run: c}]\n";
    let mut r = Run::gha(&jobs(body));
    r.cancel();
    assert_eq!(r.cancels, vec!["a"]);
    r.finish("a", Conclusion::Cancelled);
    assert!(r.logged("b skipped"));
    assert!(r.queued.contains(&"cleanup".to_owned()) && r.queued.contains(&"oncancel".to_owned()), "{:?}", r.log);
    r.finish("cleanup", S);
    r.finish("oncancel", S);
    assert_eq!(r.finished, Some(Conclusion::Cancelled));
}

#[test]
fn a_timeout_is_a_failure_to_dependents() {
    let mut r = Run::gha(&jobs("  a:\n    runs-on: x\n    steps: [{run: a}]\n  b:\n    needs: a\n    if: failure()\n    runs-on: x\n    steps: [{run: b}]\n"));
    r.finish("a", Conclusion::TimedOut);
    assert!(r.queued.contains(&"b".to_owned()));
    assert_eq!(at(&r.spec("b").needs, "/a/result"), "failure");
}

#[test]
fn fail_fast_and_max_parallel() {
    // fail-fast (default): the first failure cancels the job's other legs.
    let mut r = Run::gha(&jobs("  t:\n    runs-on: x\n    strategy:\n      matrix:\n        v: [1, 2, 3]\n    steps: [{run: t}]\n"));
    assert_eq!(r.queued, vec!["t (1)", "t (2)", "t (3)"]);
    r.finish("t (2)", F);
    assert_eq!(r.cancels, vec!["t (1)", "t (3)"]);
    // fail-fast: false cancels nothing.
    let mut r = Run::gha(&jobs("  t:\n    runs-on: x\n    strategy:\n      fail-fast: false\n      matrix:\n        v: [1, 2, 3]\n    steps: [{run: t}]\n"));
    r.finish("t (2)", F);
    assert!(r.cancels.is_empty());
    // max-parallel: one at a time, in order.
    let mut r = Run::gha(&jobs("  t:\n    runs-on: x\n    strategy:\n      max-parallel: 1\n      matrix:\n        v: [1, 2, 3]\n    steps: [{run: t}]\n"));
    assert_eq!(r.queued, vec!["t (1)"]);
    r.finish("t (1)", S);
    assert_eq!(r.queued, vec!["t (1)", "t (2)"]);
    assert_eq!(at(&r.spec("t (2)").strategy, "/max-parallel"), 1.0);
    // max-parallel: the held legs are cancelled by fail-fast without being queued.
    let mut r = Run::gha(&jobs("  t:\n    runs-on: x\n    strategy:\n      max-parallel: 1\n      matrix:\n        v: [1, 2, 3]\n    steps: [{run: t}]\n"));
    r.finish("t (1)", F);
    assert!(r.logged("t (2) cancelled: fail-fast: t (1) failed") && r.logged("t (3) cancelled"), "{:?}", r.log);
    assert_eq!(r.finished, Some(F));
}

#[test]
fn githubs_fail_fast_example_with_an_experimental_leg() {
    let body = "  test:\n    runs-on: ubuntu-latest\n    continue-on-error: ${{ matrix.experimental }}\n    strategy:\n      fail-fast: true\n      matrix:\n        version: [6, 7, 8]\n        experimental: [false]\n        include:\n          - version: 9\n            experimental: true\n    steps: [{run: t}]\n";
    let mut r = Run::gha(&jobs(body));
    assert_eq!(r.queued.len(), 4);
    assert!(r.spec("test (9, true)").continue_on_error && !r.spec("test (6, false)").continue_on_error);
    r.finish("test (9, true)", F);
    assert!(r.cancels.is_empty(), "the experimental leg failing affects no one");
    r.finish("test (6, false)", F);
    assert_eq!(r.cancels, vec!["test (7, false)", "test (8, false)"]);
}

#[test]
fn strategy_context_and_leg_names() {
    let r = Run::gha(&jobs("  build:\n    name: Build ${{ matrix.os }}\n    runs-on: ${{ matrix.os }}\n    strategy:\n      matrix:\n        os: [ubuntu-latest, windows-latest]\n    steps: [{run: b}]\n"));
    // The name references matrix: no suffix; the key keeps the values.
    let s = r.spec("build (windows-latest)");
    assert_eq!(s.name, "Build windows-latest");
    assert_eq!(s.runs_on.labels, vec!["windows-latest"]);
    assert_eq!((at(&s.strategy, "/job-index").as_f64(), at(&s.strategy, "/job-total").as_f64()), (Some(1.0), Some(2.0)));
}

/// W17-R06: `kiln workflow run --matrix os=windows-latest` runs those combinations; a job whose
/// matrix lacks the key runs them all, and one with none that match is skipped, saying why.
#[test]
fn w17_r06_only_matrix() {
    let body = "  test:\n    runs-on: ${{ matrix.os }}\n    strategy:\n      matrix:\n        os: [macos-latest, windows-latest]\n        version: [12, 14]\n    steps: [{run: t}]\n  lint:\n    runs-on: x\n    strategy:\n      matrix:\n        node: [20, 22]\n    steps: [{run: l}]\n  pick:\n    runs-on: x\n    strategy:\n      matrix:\n        os: [linux]\n    steps: [{run: p}]\n";
    let only = Settings { only_matrix: vec![("os".into(), "windows-latest".into())], ..Settings::default() };
    let r = Run::with_settings(&jobs(body), Dialect::Gha, RunInput { trusted: true, ..RunInput::default() }, Source::default(), only);
    assert_eq!(r.queued, vec!["test (windows-latest, 12)", "test (windows-latest, 14)", "lint (20)", "lint (22)"]);
    assert!(r.logged("pick skipped: no combination of the matrix has os=windows-latest"), "{:?}", r.log);
    assert_eq!(at(&r.spec("test (windows-latest, 14)").strategy, "/job-total").as_f64(), Some(2.0), "counted over what runs");
}

/// W04-S01: GitHub's documented example.
#[test]
fn w04_s01_documented_legs() {
    let body = "  test:\n    runs-on: ${{ matrix.os }}\n    strategy:\n      matrix:\n        os: [macos-latest, windows-latest]\n        version: [12, 14, 16]\n        include:\n          - os: windows-latest\n            version: 17\n    steps: [{run: t}]\n";
    let r = Run::gha(&jobs(body));
    assert_eq!(
        r.queued,
        vec!["test (macos-latest, 12)", "test (macos-latest, 14)", "test (macos-latest, 16)", "test (windows-latest, 12)", "test (windows-latest, 14)", "test (windows-latest, 16)", "test (windows-latest, 17)"]
    );
}

/// W04-S02: a matrix read from a need's output expands when the need finishes.
#[test]
fn w04_s02_a_matrix_from_outputs() {
    let body = "  plan:\n    runs-on: x\n    outputs:\n      shards: ${{ steps.p.outputs.shards }}\n    steps: [{id: p, run: p}]\n  test:\n    needs: plan\n    runs-on: x\n    strategy:\n      matrix:\n        shard: ${{ fromJSON(needs.plan.outputs.shards) }}\n    steps: [{run: t}]\n";
    let mut r = Run::gha(&jobs(body));
    assert_eq!(r.queued, vec!["plan"]);
    r.outputs("plan", &[("shards", r#"["core","cli","ui"]"#)]);
    assert_eq!(r.queued, vec!["plan", "test (core)", "test (cli)", "test (ui)"]);
    assert_eq!(at(&r.spec("test (cli)").needs, "/plan/outputs/shards"), r#"["core","cli","ui"]"#);
}

#[test]
fn a_matrix_that_cant_expand_fails_its_job() {
    let mut r = Run::gha(&jobs("  a:\n    runs-on: x\n    outputs: { m: x }\n    steps: [{run: a}]\n  t:\n    needs: a\n    runs-on: x\n    strategy:\n      matrix:\n        v: ${{ fromJSON(needs.a.outputs.m) }}\n    steps: [{run: t}]\n"));
    r.outputs("a", &[("m", "not json")]);
    assert!(r.logged("t failure: strategy.matrix.v: Error parsing JSON"), "{:?}", r.log);
    assert_eq!(r.finished, Some(F));
}

#[test]
fn runs_on_timeouts_environment_concurrency_and_permissions() {
    let src = "on: workflow_dispatch\nrun-name: Deploy ${{ inputs.env }}\nconcurrency:\n  group: deploy-${{ inputs.env }}\n  cancel-in-progress: ${{ inputs.env != 'prod' }}\npermissions:\n  contents: write\njobs:\n  d:\n    runs-on: [self-hosted, '${{ inputs.env }}']\n    timeout-minutes: 30\n    environment:\n      name: ${{ inputs.env }}\n      url: https://${{ inputs.env }}.example\n    concurrency:\n      group: d-${{ inputs.env }}\n      queue: max\n    container: node:${{ inputs.node }}\n    services:\n      db: { image: postgres }\n    steps: [{run: d}]\n";
    let input = RunInput { inputs: serde_json::json!({"env": "staging", "node": 20}), trusted: true, ..RunInput::default() };
    let r = Run::with(src, Dialect::Gha, input.clone(), Source::default());
    assert_eq!(r.plan.name.as_deref(), Some("Deploy staging"));
    assert_eq!(r.plan.concurrency, Some(GroupSpec { group: "deploy-staging".into(), cancel_in_progress: true, queue_max: false, limit: None }));
    let d = r.spec("d");
    assert_eq!(d.runs_on.labels, vec!["self-hosted", "staging"]);
    assert_eq!(d.timeout_minutes, 30);
    assert_eq!(d.environment, Some(EnvironmentSpec { name: "staging".into(), url: Some("https://${{ inputs.env }}.example".into()), deployment: true, preview: None }));
    assert!(d.concurrency.as_ref().is_some_and(|c| c.group == "d-staging" && c.queue_max));
    assert_eq!(d.container.as_deref(), Some("node:20"));
    assert_eq!(d.services, vec![("db".to_owned(), "postgres".to_owned())]);
    assert_eq!(d.permissions.level("contents"), perms::Level::Write);
    assert_eq!(d.permissions.level("issues"), perms::Level::None);
    // Untrusted: read at most.
    let r = Run::with(src, Dialect::Gha, RunInput { trusted: false, ..input }, Source::default());
    assert_eq!(r.spec("d").permissions.level("contents"), perms::Level::Read);
    // GitHub's default timeout, and kiln's v1 default.
    assert_eq!(Run::gha(&jobs("  a:\n    runs-on: x\n    steps: [{run: a}]\n")).spec("a").timeout_minutes, 360);
    assert_eq!(Run::kiln("name: k\non: push\njobs:\n  a:\n    steps: [{run: a}]\n").spec("a").timeout_minutes, 60);
}

#[test]
fn an_expression_that_fails_fails_the_leg_not_the_plan() {
    let mut r = Run::gha(&jobs("  a:\n    runs-on: ${{ fromJSON('nope') }}\n    steps: [{run: a}]\n  b:\n    needs: a\n    if: always()\n    runs-on: x\n    steps: [{run: b}]\n"));
    assert!(r.logged("a failure: runs-on"), "{:?}", r.log);
    assert!(r.queued.contains(&"b".to_owned()));
    r.finish("b", S);
    assert_eq!(r.finished, Some(F));
}

// ------------------------------------------------------------------ reusable workflows (W04-AC04)

const CALLED: &str = "on:\n  workflow_call:\n    inputs:\n      n: { type: number, required: true }\n      flag: { type: boolean }\n      s: { type: string, default: d }\n    secrets:\n      token: { required: false }\n    outputs:\n      out: { value: '${{ jobs.inner.outputs.x }}' }\npermissions: write-all\njobs:\n  inner:\n    runs-on: x\n    outputs: { x: '${{ steps.a.outputs.x }}' }\n    steps: [{id: a, run: a}]\n";

fn with_called(extra: &[(&str, &str)]) -> Source {
    let mut s = Source::default();
    s.workflows.insert("./.github/workflows/called.yml".into(), workflow(CALLED, Dialect::Gha));
    for (k, v) in extra {
        s.workflows.insert((*k).to_owned(), workflow(v, Dialect::Gha));
    }
    s
}

#[test]
fn w04_ac04_inputs_outputs_secrets_permissions() {
    let caller = "on: push\npermissions:\n  contents: read\njobs:\n  call:\n    uses: ./.github/workflows/called.yml\n    with: { n: 3, flag: true }\n    secrets: inherit\n  after:\n    needs: call\n    runs-on: x\n    steps: [{run: 'echo ${{ needs.call.outputs.out }}'}]\n";
    let mut r = Run::with(caller, Dialect::Gha, RunInput { trusted: true, ..RunInput::default() }, with_called(&[]));
    assert_eq!(r.queued, vec!["call / inner"]);
    let inner = r.spec("call / inner");
    assert_eq!(inner.job, "call/inner");
    assert_eq!(inner.inputs, serde_json::json!({"n": 3, "flag": true, "s": "d"}));
    assert_eq!(inner.secrets, SecretsSpec::Run);
    // write-all in the callee is cut down to the caller's.
    assert_eq!((inner.permissions.level("contents"), inner.permissions.level("issues")), (perms::Level::Read, perms::Level::None));
    r.outputs("call / inner", &[("x", "42")]);
    assert_eq!(r.queued, vec!["call / inner", "after"]);
    assert_eq!(at(&r.spec("after").needs, "/call/outputs/out"), "42");
    assert_eq!(at(&r.spec("after").needs, "/call/result"), "success");
    r.finish("after", S);
    assert_eq!(r.finished, Some(S));
}

/// W08-R14 (AC09's last part): a job's cache mode caps the workflow it calls. A called workflow
/// or job asking for more doesn't start; legs that ask nothing carry the cap to the dispatcher.
#[test]
fn w08_r14_the_callers_cache_mode_caps_the_called() {
    let greedy = "on: workflow_call\njobs:\n  inner:\n    runs-on: x\n    cache-mode: write\n    steps: [{run: a}]\n";
    let modest = "on: workflow_call\njobs:\n  inner:\n    runs-on: x\n    steps: [{run: a}]\n";
    let src = || with_called(&[("./.github/workflows/greedy.yml", greedy), ("./.github/workflows/modest.yml", modest)]);
    let caller = |called: &str, mode: &str| format!("on: push\njobs:\n  call:\n    uses: ./.github/workflows/{called}.yml\n{mode}");
    let r = Run::with(&caller("greedy", "    cache-mode: read\n"), Dialect::Gha, RunInput { trusted: true, ..RunInput::default() }, src());
    assert!(r.logged("asks for `cache-mode: write` (its job `inner`), more than its caller's `cache-mode: read` allows"), "{:?}", r.log);
    assert!(r.queued.is_empty());
    let r = Run::with(&caller("modest", "    cache-mode: read\n"), Dialect::Gha, RunInput { trusted: true, ..RunInput::default() }, src());
    assert_eq!(r.spec("call / inner").cache_cap.as_deref(), Some("read"));
    // Without a mode of its own, the caller caps nothing.
    let r = Run::with(&caller("greedy", ""), Dialect::Gha, RunInput { trusted: true, ..RunInput::default() }, src());
    assert_eq!(r.spec("call / inner").cache_cap, None);
    assert_eq!(crate::store::cache::Mode::Write.min(crate::store::cache::Mode::Read), crate::store::cache::Mode::Read);
    assert_eq!(crate::store::cache::Mode::WriteOnly.min(crate::store::cache::Mode::Read), crate::store::cache::Mode::None);
}

#[test]
fn w04_ac04_inputs_and_secrets_are_checked() {
    let run = |with: &str, secrets: &str| {
        let caller = format!("on: push\njobs:\n  call:\n    uses: ./.github/workflows/called.yml\n{with}{secrets}");
        Run::with(&caller, Dialect::Gha, RunInput { trusted: true, ..RunInput::default() }, with_called(&[]))
    };
    assert!(run("", "").logged("requires the input `n`"));
    assert!(run("    with: { n: 1, nope: 2 }\n", "").logged("has no input `nope`"));
    assert!(run("    with: { n: 1, flag: maybe }\n", "").logged("is a boolean"));
    let r = run("    with: { n: 1 }\n", "    secrets: { token: '${{ secrets.T }}' }\n");
    assert!(matches!(r.spec("call / inner").secrets, SecretsSpec::Mapped { ref map, .. } if map == &vec![("token".to_owned(), "${{ secrets.T }}".to_owned())]));
    assert_eq!(run("    with: { n: 1 }\n", "").spec("call / inner").secrets, SecretsSpec::None);
}

/// W06-R03: a leg knows the workflow file that defines its job, as `job.workflow_*` say it: the
/// run's own; a `./` one's (the run's repository and commit, at its ref); a remote one's (its
/// repository, its ref as GitHub writes it, and the commit it was pinned to).
#[test]
fn w06_r03_a_legs_job_knows_its_workflow() {
    let caller = "on: push\njobs:\n  top:\n    runs-on: x\n    steps: [{run: a}]\n  local:\n    uses: ./.github/workflows/called.yml\n    with: { n: 1 }\n  remote:\n    uses: octo-org/automation/.github/workflows/deploy.yml@v1\n";
    let at = |src: &str, path: &str, origin: Option<crate::wf::ir::Origin>| Arc::new(Workflow { path: path.into(), origin, ..(*workflow(src, Dialect::Gha)).clone() });
    let mut src = Source::default();
    src.workflows.insert("./.github/workflows/called.yml".into(), at(CALLED, ".github/workflows/called.yml", None));
    let far = "on: workflow_call\njobs:\n  far:\n    runs-on: x\n    steps: [{run: b}]\n";
    let pinned = crate::wf::ir::Origin { repository: "octo-org/automation".into(), git_ref: "refs/tags/v1".into(), sha: "f".repeat(40) };
    src.workflows.insert("octo-org/automation/.github/workflows/deploy.yml@v1".into(), at(far, ".github/workflows/deploy.yml", Some(pinned)));
    let github = serde_json::json!({ "repository": "acme/app", "ref": "refs/heads/main", "sha": "a".repeat(40) });
    let r = Run::with(caller, Dialect::Gha, RunInput { trusted: true, github, ..RunInput::default() }, src);
    let defined = |repo: &str, path: &str, git_ref: &str, sha: String| DefinedIn {
        workflow_ref: format!("{repo}/{path}@{git_ref}"),
        workflow_sha: sha,
        workflow_repository: repo.into(),
        workflow_file_path: path.into(),
    };
    assert_eq!(r.spec("top").defined_in, defined("acme/app", ".github/workflows/t.yml", "refs/heads/main", "a".repeat(40)), "the run's own");
    assert_eq!(r.spec("local / inner").defined_in, defined("acme/app", ".github/workflows/called.yml", "refs/heads/main", "a".repeat(40)), "a `./` one, at the run's commit");
    assert_eq!(r.spec("remote / far").defined_in, defined("octo-org/automation", ".github/workflows/deploy.yml", "refs/tags/v1", "f".repeat(40)), "a remote one, pinned");
}

#[test]
fn w04_ac04_nesting_limit() {
    // w0 calls w1, …: ten levels nest, eleven don't.
    let level = |i: usize, last: bool| {
        if last {
            "on: workflow_call\njobs:\n  leaf:\n    runs-on: x\n    steps: [{run: l}]\n".to_owned()
        } else {
            format!("on: workflow_call\njobs:\n  next:\n    uses: ./.github/workflows/w{}.yml\n", i + 1)
        }
    };
    for (depth, ok) in [(10usize, true), (11, false)] {
        let mut src = Source::default();
        for i in 1..=depth {
            src.workflows.insert(format!("./.github/workflows/w{i}.yml"), workflow(&level(i, i == depth), Dialect::Gha));
        }
        let root = "on: push\njobs:\n  start:\n    uses: ./.github/workflows/w1.yml\n";
        let r = Run::with(root, Dialect::Gha, RunInput { trusted: true, ..RunInput::default() }, src);
        assert_eq!(r.queued.len() == 1, ok, "depth {depth}: {:?} {:?}", r.queued, r.log);
        assert_eq!(r.logged("nest at most 10 deep"), !ok);
    }
}

#[test]
fn w04_ac04_a_matrix_of_calls() {
    let caller = "on: push\njobs:\n  call:\n    strategy:\n      matrix:\n        v: [a, b]\n    uses: ./.github/workflows/called.yml\n    with: { n: 1 }\n";
    let mut r = Run::with(caller, Dialect::Gha, RunInput { trusted: true, ..RunInput::default() }, with_called(&[]));
    assert_eq!(r.queued, vec!["call (a) / inner", "call (b) / inner"]);
    r.finish("call (a) / inner", S);
    assert_eq!(r.finished, None);
    r.finish("call (b) / inner", F);
    assert_eq!(r.finished, Some(F));
}

// ------------------------------------------------------------------ kiln matrices (W04-AC05)

#[test]
fn w04_ac05_where_and_when() {
    let src = "name: k\non: push\njobs:\n  t:\n    strategy:\n      matrix:\n        os: [linux, macos, windows]\n        profile: [dev, release, asan]\n        where: matrix.profile != 'asan' || matrix.os == 'linux'\n      when: ${{ matrix.os != 'windows' }}\n    steps: [{run: t}]\n";
    let r = Run::kiln(src);
    let removed = r.plan.removed().get("t").cloned().unwrap_or_default();
    assert_eq!(removed.len(), 2);
    assert!(removed.iter().all(|(m, why)| at(m, "/profile") == "asan" && why == "removed by where"));
    // 7 legs: windows' two are skipped by when, the rest queued.
    assert_eq!(r.queued.len(), 5);
    assert!(r.logged("t (windows, dev) skipped: skipped by when"), "{:?}", r.log);
}

#[test]
fn w04_ac05_platforms_and_emulation() {
    let src = "name: k\non: push\njobs:\n  b:\n    runs-on: native\n    strategy:\n      matrix:\n        platforms: [linux/amd64, linux/arm64, darwin/arm64]\n        profile: [release]\n        emulate: [linux/arm64]\n    steps: [{run: b}]\n";
    let r = Run::kiln(src);
    assert_eq!(r.queued, vec!["b (linux/amd64, linux, amd64, release)", "b (linux/arm64, linux, arm64, release)", "b (darwin/arm64, darwin, arm64, release)"]);
    let mac = r.spec("b (darwin/arm64, darwin, arm64, release)");
    assert_eq!(mac.requires, vec!["os == 'macos' && arch == 'aarch64'"]);
    assert_eq!(at(&mac.matrix, "/os"), "darwin");
    let arm = r.spec("b (linux/arm64, linux, arm64, release)");
    assert!(arm.emulated && arm.requires.first().is_some_and(|q| q.contains("binfmt contains 'linux/arm64'")));
}

/// W04-S03: `matrix: same` waits only for the matching leg.
#[test]
fn w04_s03_leg_level_needs() {
    let src = "name: k\non: push\njobs:\n  build:\n    strategy:\n      matrix:\n        platforms: [linux/amd64, linux/arm64]\n    steps: [{run: b}]\n  test:\n    needs: [{ job: build, matrix: same }]\n    strategy:\n      matrix:\n        platforms: [linux/amd64, linux/arm64]\n    steps: [{run: t}]\n";
    let mut r = Run::kiln(src);
    assert_eq!(r.queued, vec!["build (linux/amd64, linux, amd64)", "build (linux/arm64, linux, arm64)"]);
    r.finish("build (linux/arm64, linux, arm64)", S);
    assert_eq!(r.queued.last().map(String::as_str), Some("test (linux/arm64, linux, arm64)"), "arm64's test starts without amd64's build");
    assert_eq!(r.queued.len(), 3);
    r.finish("build (linux/amd64, linux, amd64)", F);
    assert!(r.logged("test (linux/amd64, linux, amd64) skipped"), "{:?}", r.log);
}

#[test]
fn leg_level_needs_by_where() {
    let src = "name: k\non: push\njobs:\n  build:\n    strategy:\n      matrix:\n        os: [linux, macos]\n      fail-fast: false\n    steps: [{run: b}]\n  pkg:\n    needs: [{ job: build, where: \"matrix.os == 'linux'\" }]\n    steps: [{run: p}]\n";
    let mut r = Run::kiln(src);
    r.finish("build (macos)", F);
    assert!(!r.queued.contains(&"pkg".to_owned()));
    r.finish("build (linux)", S);
    assert!(r.queued.contains(&"pkg".to_owned()), "macos failing doesn't hold pkg: {:?}", r.log);
}

#[test]
fn w04_ac05_shard_and_chunk() {
    let mut src = Source::default();
    for (i, d) in [300.0, 240.0, 180.0, 120.0, 60.0, 60.0, 30.0, 30.0].iter().enumerate() {
        src.durations.insert(format!("t{i}"), *d);
    }
    let shard = "name: k\non: push\njobs:\n  t:\n    strategy:\n      shard:\n        items: ['t0', 't1', 't2', 't3', 't4', 't5', 't6', 't7']\n        count: 3\n    steps: [{run: t}]\n";
    let r = Run::with(shard, Dialect::Kiln, RunInput { trusted: true, ..RunInput::default() }, src);
    assert_eq!(r.queued.len(), 3);
    let items: Vec<Vec<String>> = r.queued.iter().map(|k| r.spec(k).items).collect();
    assert_eq!(items.iter().map(Vec::len).sum::<usize>(), 8);
    assert_eq!(at(&r.spec(r.queued.first().map_or("", String::as_str)).matrix, "/shard"), serde_json::json!({"index": 0, "total": 3}));
    // chunk: 10 items, 4 a leg.
    let chunk = "name: k\non: push\njobs:\n  t:\n    strategy:\n      chunk: { items: '${{ fromJSON(''[1,2,3,4,5,6,7,8,9,10]'') }}', size: 4 }\n    steps: [{run: t}]\n";
    let r = Run::kiln(chunk);
    assert_eq!(r.queued.iter().map(|k| r.spec(k).items.len()).collect::<Vec<_>>(), vec![4, 4, 2]);
}

/// W04-S04: `target` with history: legs of about the target each.
#[test]
fn w04_s04_shard_to_a_target() {
    let mut src = Source::default();
    let mut files = Vec::new();
    for i in 0..40 {
        let f = format!("tests/t{i}_test.py");
        src.durations.insert(f.clone(), 60.0);
        files.push(f);
    }
    src.files = files;
    let s = "name: k\non: push\njobs:\n  t:\n    strategy:\n      shard: { files: 'tests/**/*_test.py', target: 5m, balance: timing }\n    steps: [{run: t}]\n";
    let r = Run::with(s, Dialect::Kiln, RunInput { trusted: true, ..RunInput::default() }, src);
    assert_eq!(r.queued.len(), 8);
    assert!(r.queued.iter().all(|k| r.spec(k).items.len() == 5));
}

#[test]
fn w04_ac05_finer_parallelism_and_fail_fast_scope() {
    let src = "name: k\non: push\njobs:\n  t:\n    strategy:\n      max-parallel: { total: 8, per: { os: { macos: 1 } } }\n      fail-fast: { scope: 'axis:os' }\n      matrix:\n        os: [linux, macos]\n        n: [1, 2]\n    steps: [{run: t}]\n";
    let mut r = Run::kiln(src);
    assert_eq!(r.queued, vec!["t (linux, 1)", "t (linux, 2)", "t (macos, 1)"]);
    r.finish("t (macos, 1)", S);
    assert_eq!(r.queued.last().map(String::as_str), Some("t (macos, 2)"));
    r.finish("t (linux, 1)", F);
    assert_eq!(r.cancels, vec!["t (linux, 2)"], "only linux legs are cancelled");
}

// ------------------------------------------------------------------ outputs (W04-AC06)

#[test]
fn w04_ac06_last_writer_and_legs() {
    let src = "name: k\non: push\njobs:\n  m:\n    strategy:\n      matrix:\n        v: [a, b]\n    outputs: { x: '${{ steps.s.outputs.x }}' }\n    steps: [{id: s, run: s}]\n  after:\n    needs: m\n    steps: [{run: a}]\n";
    let mut r = Run::kiln(src);
    r.outputs("m (b)", &[("x", "b")]);
    r.outputs("m (a)", &[("x", "a")]);
    let after = r.spec("after");
    assert_eq!(at(&after.needs, "/m/outputs/x"), "a", "the leg that finished last wins");
    assert_eq!(at(&after.needs, "/m/legs").as_array().map(Vec::len), Some(2));
    assert_eq!(at(&after.needs, "/m/legs/1/outputs/x"), "b");
}

#[test]
fn w04_ac06_limits_typed_env_and_secret_outputs() {
    // Over 1 MiB: none kept, with a warning.
    let mut r = Run::gha(&jobs("  a:\n    runs-on: x\n    steps: [{run: a}]\n  b:\n    needs: a\n    runs-on: x\n    steps: [{run: b}]\n"));
    r.outputs("a", &[("big", &"x".repeat(MAX_JOB_OUTPUTS + 1))]);
    assert!(r.warnings.iter().any(|w| w.contains("1 MiB")));
    assert_eq!(at(&r.spec("b").needs, "/a/outputs"), serde_json::json!({}));
    // Typed outputs: kiln consumers see the JSON value, GitHub's its text.
    let src = "name: k\non: push\njobs:\n  a:\n    steps: [{run: a}]\n  b:\n    needs: a\n    env-from: [a]\n    steps: [{run: b}]\n  c:\n    needs: b\n    steps: [{run: c}]\n";
    let mut r = Run::kiln(src);
    r.report(
        "a",
        LegReport {
            json_outputs: vec![("n".into(), serde_json::json!(3))],
            env: vec![("IMAGE_TAG".into(), "abc".into())],
            run_env: vec![("RELEASE".into(), "1".into())],
            secret_outputs: vec!["token".into()],
            ..LegReport::of(S)
        },
    );
    let b = r.spec("b");
    assert_eq!(at(&b.needs, "/a/outputs/n"), 3.0);
    assert_eq!(at(&b.needs, "/a/env/IMAGE_TAG"), "abc");
    // W04-S05: env-from.
    assert_eq!(b.env_from, vec![("IMAGE_TAG".to_owned(), "abc".to_owned())]);
    assert_eq!(b.run_env, vec![("RELEASE".to_owned(), "1".to_owned())]);
    assert_eq!(b.secret_outputs, vec![SecretOutput { job: "a".into(), name: "token".into(), leg: "a".into() }]);
    assert!(at(&b.needs, "/a/outputs").get("token").is_none(), "secret outputs never enter contexts");
    r.finish("b", S);
    assert_eq!(r.spec("c").run_env, vec![("RELEASE".to_owned(), "1".to_owned())], "run.env reaches later jobs that need the writer");
    // A GitHub consumer sees the typed output as JSON text.
    let mut r = Run::gha(&jobs("  a:\n    runs-on: x\n    steps: [{run: a}]\n  b:\n    needs: a\n    runs-on: x\n    steps: [{run: b}]\n"));
    r.report("a", LegReport { json_outputs: vec![("n".into(), serde_json::json!({"k": [1]}))], ..LegReport::of(S) });
    assert_eq!(at(&r.spec("b").needs, "/a/outputs/n"), r#"{"k":[1]}"#);
}

#[test]
fn reads_are_recorded_for_the_flow() {
    let mut r = Run::gha(&jobs("  a:\n    runs-on: x\n    steps: [{run: a}]\n  b:\n    needs: a\n    if: needs.a.result == 'success'\n    runs-on: x\n    steps: [{run: 'echo ${{ needs.a.outputs.digest }}'}]\n"));
    r.finish("a", S);
    assert_eq!(r.spec("b").reads, vec!["needs.a.result", "needs.a.outputs.digest"]);
}

#[test]
fn reports_for_unknown_or_finished_legs_change_nothing() {
    let mut r = Run::gha(&jobs("  a:\n    runs-on: x\n    steps: [{run: a}]\n"));
    let before = r.queued.clone();
    r.finish("nope", S);
    r.finish("a", S);
    assert_eq!(r.finished, Some(S));
    let acts = r.plan.on_leg_finished("a", LegReport::of(F), &r.src);
    assert!(acts.is_empty());
    assert_eq!(r.queued, before);
}

// ------------------------------------------------------------------ preview (W04-R29)

#[test]
fn preview_matches_what_a_run_queues() -> Result<(), String> {
    let src = "on: push\njobs:\n  a:\n    runs-on: x\n    strategy:\n      matrix:\n        v: [1, 2]\n    steps: [{run: a}]\n  b:\n    needs: a\n    if: needs.a.result == 'success'\n    runs-on: x\n    steps: [{run: b}]\n  c:\n    if: false\n    runs-on: x\n    steps: [{run: c}]\n";
    let p = RunPlan::preview(workflow(src, Dialect::Gha), RunInput { trusted: true, ..RunInput::default() }, Settings::default(), &NoSource)?;
    assert_eq!(p.legs.iter().map(|l| l.key.as_str()).collect::<Vec<_>>(), vec!["a (1)", "a (2)", "b"]);
    assert!(p.decided.iter().any(|d| d.job == "c" && d.conclusion == Conclusion::Skipped));
    assert_eq!(p.later, vec!["b"]);
    Ok(())
}

/// W05-R25: a restarted dispatcher takes legs back with what the plan made of them, the
/// environment (its protection holds them), permissions and concurrency included.
#[test]
fn a_leg_s_spec_comes_back_whole_from_the_plan() -> Result<(), String> {
    let src = "on: push\njobs:\n  deploy:\n    runs-on: x\n    environment: { name: production, url: 'https://example.com' }\n    permissions: { contents: write }\n    concurrency: deploy\n    steps: [{run: d}]\n";
    let (plan, _) = RunPlan::start(workflow(src, Dialect::Gha), RunInput { trusted: true, ..RunInput::default() }, Settings::default(), &NoSource)?;
    let spec = plan.spec_of("deploy").ok_or("the leg")?;
    assert_eq!(spec.environment.as_ref().map(|e| e.name.as_str()), Some("production"));
    assert_eq!(spec.permissions.level("contents"), perms::Level::Write);
    assert_eq!(spec.concurrency.as_ref().map(|g| g.group.as_str()), Some("deploy"));
    assert!(spec.def().is_some_and(|j| j.id == "deploy"), "its job, to run");
    assert!(plan.spec_of("nope").is_none());
    Ok(())
}

/// A matrix from a job's outputs is decided when that job finishes: the preview's empty outputs
/// don't make it a failure, nor skip the jobs after it.
#[test]
fn preview_leaves_what_needs_outputs_decide_for_later() -> Result<(), String> {
    let src = "on: push\njobs:\n  list:\n    runs-on: x\n    outputs: {targets: '${{ steps.l.outputs.t }}'}\n    steps: [{id: l, run: x}]\n  fuzz:\n    needs: list\n    runs-on: x\n    strategy:\n      matrix:\n        target: ${{ fromJSON(needs.list.outputs.targets) }}\n    steps: [{run: f}]\n  report:\n    needs: fuzz\n    runs-on: x\n    steps: [{run: r}]\n";
    let p = RunPlan::preview(workflow(src, Dialect::Gha), RunInput { trusted: true, ..RunInput::default() }, Settings::default(), &NoSource)?;
    assert_eq!(p.legs.iter().map(|l| l.key.as_str()).collect::<Vec<_>>(), vec!["list"]);
    assert!(p.decided.is_empty(), "{:?}", p.decided);
    assert_eq!(p.later, vec!["fuzz", "report"]);
    Ok(())
}

#[test]
fn preview_lists_the_combinations_exclude_removed() -> Result<(), String> {
    let src = "on: push\njobs:\n  t:\n    runs-on: x\n    strategy:\n      matrix:\n        os: [linux, mac]\n        node: [20, 22]\n        exclude:\n          - {os: linux, node: 99}\n          - {os: mac, node: 20}\n    steps: [{run: t}]\n";
    let p = RunPlan::preview(workflow(src, Dialect::Gha), RunInput { trusted: true, ..RunInput::default() }, Settings::default(), &NoSource)?;
    assert_eq!(p.legs.len(), 3);
    assert_eq!(p.removed.get("t").cloned().unwrap_or_default(), vec![(serde_json::json!({"os": "mac", "node": 20}), "removed by exclude entry 2".to_owned())]);
    Ok(())
}

// ------------------------------------------------------------------ affected() (W24-R17)

/// A Cargo workspace of three crates, where `cli` depends on `core`.
fn crates() -> Arc<Graph> {
    let files: HashMap<&str, &str> = HashMap::from([
        ("Cargo.toml", "[workspace]\nmembers = [\"crates/*\"]\n"),
        ("crates/core/Cargo.toml", "[package]\nname = \"core\"\n"),
        ("crates/cli/Cargo.toml", "[package]\nname = \"cli\"\n\n[dependencies]\ncore = { path = \"../core\" }\n"),
        ("crates/docs/Cargo.toml", "[package]\nname = \"docs\"\n"),
    ]);
    let paths: Vec<String> = files.keys().map(|p| (*p).to_owned()).collect();
    Arc::new(graph::read(&|p| files.get(p).map(|t| (*t).to_owned()), &paths))
}

/// A push that changed `files`.
fn changed(files: &[&str]) -> RunInput {
    RunInput { trusted: true, changed: Some(Arc::new(files.iter().map(|f| (*f).to_owned()).collect())), ..RunInput::default() }
}

/// W24-R17: `affected()` in a dynamic matrix and in a condition, from the run's changed files and
/// the commit's package graph.
#[test]
fn w24_r17_affected_in_matrices_and_conditions() {
    let wf = "name: k\non: push\njobs:\n  test:\n    strategy:\n      matrix:\n        crate: ${{ affected('cargo') }}\n    steps: [{run: t}]\n  docs:\n    if: contains(affected('cargo'), 'docs')\n    steps: [{run: d}]\n";
    let r = Run::with(wf, Dialect::Kiln, changed(&["crates/core/src/lib.rs", "README.md"]), Source { graph: Some(crates()), ..Source::default() });
    assert_eq!(r.queued, ["test (cli)", "test (core)"], "core, and cli, which depends on it: {:?}", r.log);
    assert!(r.logged("docs skipped"), "{:?}", r.log);
    let r = Run::with(wf, Dialect::Kiln, changed(&["crates/docs/book.md"]), Source { graph: Some(crates()), ..Source::default() });
    let mut queued = r.queued.clone();
    queued.sort();
    assert_eq!(queued, ["docs", "test (docs)"]);
}

/// What `affected()` can't know is an error that says why, never an empty list.
#[test]
fn w24_r17_affected_says_what_it_lacks() {
    let wf = |eco: &str| format!("name: k\non: push\njobs:\n  test:\n    strategy:\n      matrix:\n        crate: ${{{{ affected('{eco}') }}}}\n    steps: [{{run: t}}]\n");
    let failed = |r: &Run, why: &str| r.logged(&format!("test failure: strategy.matrix.crate: {why}"));
    let r = Run::with(&wf("cargo"), Dialect::Kiln, changed(&["x"]), Source::default());
    assert!(failed(&r, "affected() needs the repository's package graph"), "{:?}", r.log);
    let r = Run::with(&wf("cargo"), Dialect::Kiln, RunInput { trusted: true, ..RunInput::default() }, Source { graph: Some(crates()), ..Source::default() });
    assert!(failed(&r, "affected() needs the event's changed files"), "{:?}", r.log);
    let r = Run::with(&wf("maven"), Dialect::Kiln, changed(&["x"]), Source { graph: Some(crates()), ..Source::default() });
    assert!(failed(&r, "affected(): `maven` is not an ecosystem"), "{:?}", r.log);
    let broken = Arc::new(graph::read(&|_| Some("[workspace\n".to_owned()), &["Cargo.toml".to_owned()]));
    let r = Run::with(&wf("cargo"), Dialect::Kiln, changed(&["x"]), Source { graph: Some(broken), ..Source::default() });
    assert!(failed(&r, "affected('cargo'): the cargo package graph couldn't be read: Cargo.toml: isn't valid TOML"), "{:?}", r.log);
}

// ------------------------------------------------------------------ unaffected legs (W24-R06)

/// W24-R06: a job that opts in to reuse, whose `if:` is false only for what the change didn't
/// touch, is queued to finish reused (`unaffected` saying why it would have been skipped); one whose
/// `if:` is false for another reason, or that doesn't opt in, is skipped as before; one the change
/// affects runs.
#[test]
fn w24_r06_legs_the_change_cannot_affect_are_queued_to_be_reused() {
    let wf = "name: k\non: push\njobs:\n  core:\n    if: contains(affected('cargo'), 'core')\n    work: { memo: { paths: [crates/core] } }\n    steps: [{run: t}]\n  plain:\n    if: contains(affected('cargo'), 'core')\n    steps: [{run: t}]\n  prs:\n    if: github.event_name == 'pull_request' && contains(affected('cargo'), 'core')\n    work: { memo: true }\n    steps: [{run: t}]\n  docs:\n    if: changed('docs/**')\n    reuse: true\n    steps: [{run: d}]\n  cli:\n    if: contains(affected('cargo'), 'cli')\n    reuse: true\n    steps: [{run: c}]\n";
    let r = Run::with(wf, Dialect::Kiln, changed(&["crates/cli/src/main.rs"]), Source { graph: Some(crates()), ..Source::default() });
    let mut queued = r.queued.clone();
    queued.sort();
    assert_eq!(queued, ["cli", "core", "docs"], "{:?}", r.log);
    assert_eq!(r.spec("core").unaffected.as_deref(), Some("skipped: `if: contains(affected('cargo'), 'core')` was false"));
    assert_eq!(r.spec("docs").unaffected.as_deref(), Some("skipped: `if: changed('docs/**')` was false"));
    assert_eq!(r.spec("cli").unaffected, None, "the change affects it: it runs");
    assert!(r.logged("plain skipped") && r.logged("prs skipped"), "no reuse, or false whatever changed: {:?}", r.log);
    // Not part of what the leg is: its reuse key is the same either way.
    let json = serde_json::to_value(r.spec("core")).unwrap_or_default();
    assert!(json.get("unaffected").is_none(), "{json}");
}

/// W24-R06 with `when:`: a matrix's legs the change can't affect are queued to be reused, the
/// others run.
#[test]
fn w24_r06_when_over_affected() {
    let wf = "name: k\non: push\njobs:\n  test:\n    strategy:\n      matrix:\n        crate: [core, cli, docs]\n      when: contains(affected('cargo'), matrix.crate)\n    work: { memo: true }\n    steps: [{run: t}]\n";
    let r = Run::with(wf, Dialect::Kiln, changed(&["crates/cli/src/main.rs"]), Source { graph: Some(crates()), ..Source::default() });
    assert_eq!(r.queued, ["test (core)", "test (cli)", "test (docs)"], "{:?}", r.log);
    assert_eq!(r.spec("test (cli)").unaffected, None);
    assert_eq!(r.spec("test (core)").unaffected.as_deref(), Some("skipped by when"));
    assert_eq!(r.spec("test (docs)").unaffected.as_deref(), Some("skipped by when"));
}

/// W24-R06: what the dispatcher makes of an unaffected leg is what follows it: reused, its
/// dependents run on its outputs; skipped (nothing to reuse), they're skipped as they'd have been.
#[test]
fn w24_r06_an_unaffected_legs_end_is_its_dependents_need() {
    let wf = "name: k\non: push\njobs:\n  core:\n    if: contains(affected('cargo'), 'core')\n    reuse: true\n    steps: [{run: t}]\n  after:\n    needs: core\n    steps: [{run: a}]\n";
    let input = || changed(&["crates/cli/src/main.rs"]);
    let mut r = Run::with(wf, Dialect::Kiln, input(), Source { graph: Some(crates()), ..Source::default() });
    assert_eq!(r.queued, ["core"]);
    r.outputs("core", &[("v", "1")]);
    assert_eq!(r.queued, ["core", "after"], "reused: {:?}", r.log);
    assert_eq!(at(&r.spec("after").needs, "/core/outputs/v"), serde_json::json!("1"));
    let mut r = Run::with(wf, Dialect::Kiln, input(), Source { graph: Some(crates()), ..Source::default() });
    r.finish("core", Conclusion::Skipped);
    assert!(r.logged("after skipped"), "{:?}", r.log);
    assert_eq!(r.finished, Some(Conclusion::Success));
}
