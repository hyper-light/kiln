//! The run graph's data (docs/ci.md W21-R04): every job of the workflow, its legs made or not (a
//! job's are made once what it needs is done), with what it needs, whether its matrix waits on
//! what it needs, its steps and how long it typically takes; each matrix leg's values, where it
//! sits in its job's grid; and the run's data flow (W04-R23).

#![cfg(unix)]

mod common;

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use common::{Options, http, start};
use serde_json::Value;

fn git(dir: &Path, args: &[&str]) -> anyhow::Result<()> {
    let o = Command::new("git").arg("-C").arg(dir).args(args).env("GIT_AUTHOR_NAME", "T").env("GIT_AUTHOR_EMAIL", "t@example.com").env("GIT_COMMITTER_NAME", "T").env("GIT_COMMITTER_EMAIL", "t@example.com").output()?;
    anyhow::ensure!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
    Ok(())
}

/// `fan`'s matrix comes from `plan`'s outputs; `last` needs `fan`, and takes `plan`'s exported
/// environment. `plan` takes a while, so the graph is read while the other two have no legs.
const WORKFLOW: &str = r#"on: push
jobs:
  plan:
    runs-on: local
    outputs:
      matrix: ${{ steps.m.outputs.matrix }}
    exports: { env: [PLAN_DIR] }
    steps:
      - id: m
        run: |
          sleep 4
          echo 'matrix={"n":[1,2],"os":["a"]}' >> "$GITHUB_OUTPUT"
          echo "PLAN_DIR=/tmp/plan" >> "$GITHUB_ENV"
  fan:
    needs: plan
    runs-on: local
    strategy:
      matrix: ${{ fromJSON(needs.plan.outputs.matrix) }}
    steps:
      - run: echo "fan ${{ matrix.n }} ${{ matrix.os }}"
  last:
    name: The last one
    needs: [fan, plan]
    env-from: [plan]
    runs-on: local
    steps:
      - run: test "$PLAN_DIR" = /tmp/plan
      - run: 'true'
"#;

fn get(port: u16, admin: &str, path: &str) -> anyhow::Result<Value> {
    let (st, body) = http(port, "GET", path, admin, None)?;
    anyhow::ensure!(st == 200, "{path}: {st} {body}");
    Ok(serde_json::from_str(&body)?)
}

/// The plan's job `id` in a graph.
fn job<'a>(graph: &'a Value, id: &str) -> &'a Value {
    graph.get("jobs").and_then(Value::as_array).and_then(|l| l.iter().find(|j| j.get("job").and_then(Value::as_str) == Some(id))).unwrap_or(&Value::Null)
}

#[test]
fn w21_r04_the_run_graph_from_the_plan() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo)?;
    std::fs::write(repo.join("README.md"), "graph\n")?;
    git(&repo, &["init", "-q", "-b", "main"])?;
    git(&repo, &["add", "."])?;
    git(&repo, &["commit", "-qm", "start"])?;
    let extra = format!("[[repos]]\nname = \"acme/graph\"\nclone_url = \"file://{}\"\nbranches = [\"main\"]\npoll_seconds = 1\n", repo.display());
    let s = start(dir.path(), &Options { agent: true, ci: "kiln_workflows = true", extra: &extra })?;
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        if body.contains("acme/graph") && !body.contains("\"last_polled_at\":null") {
            break;
        }
        anyhow::ensure!(Instant::now() < deadline, "never polled: {body}");
        std::thread::sleep(Duration::from_millis(300));
    }
    std::fs::create_dir_all(repo.join(".kiln"))?;
    std::fs::write(repo.join(".kiln/graph.yaml"), WORKFLOW)?;
    git(&repo, &["add", "."])?;
    git(&repo, &["commit", "-qm", "graph"])?;

    // While `plan` runs: its node has its leg, and the plan says what the others wait for.
    let deadline = Instant::now() + Duration::from_secs(120);
    let (first, graph) = loop {
        let runs = get(s.port, &s.admin, "/api/ci/runs?repo=acme/graph&limit=5")?;
        if let Some(id) = runs.pointer("/runs/0/id").and_then(Value::as_str) {
            let graph = get(s.port, &s.admin, &format!("/api/ci/runs/{id}/graph"))?;
            let nodes: Vec<&str> = graph.get("nodes").and_then(Value::as_array).map(|l| l.iter().filter_map(|n| n.get("job").and_then(Value::as_str)).collect()).unwrap_or_default();
            if nodes == ["plan"] {
                break (id.to_owned(), graph);
            }
            anyhow::ensure!(!nodes.contains(&"fan"), "`plan` finished before the graph was read: {graph}");
        }
        anyhow::ensure!(Instant::now() < deadline, "no run: {runs}\n{}", s.log());
        std::thread::sleep(Duration::from_millis(200));
    };
    let ids: Vec<&str> = graph.get("jobs").and_then(Value::as_array).map(|l| l.iter().filter_map(|j| j.get("job").and_then(Value::as_str)).collect()).unwrap_or_default();
    assert_eq!(ids, ["plan", "fan", "last"], "every job, as written: {graph}");
    assert_eq!(job(&graph, "fan").get("matrix_waits"), Some(&Value::Bool(true)), "its matrix comes from plan's outputs: {graph}");
    assert_eq!(job(&graph, "fan").get("needs"), Some(&serde_json::json!(["plan"])), "{graph}");
    assert_eq!(job(&graph, "last").get("matrix_waits"), Some(&Value::Bool(false)), "{graph}");
    assert_eq!(job(&graph, "last").get("name").and_then(Value::as_str), Some("The last one"), "{graph}");
    assert_eq!(job(&graph, "last").get("steps").and_then(Value::as_u64), Some(2), "{graph}");
    assert_eq!(job(&graph, "plan").get("calls"), Some(&Value::Bool(false)), "{graph}");
    assert!(job(&graph, "plan").get("typical").is_some_and(Value::is_null), "no run before it to say: {graph}");

    // The run's end: `fan`'s two legs, each with its values.
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        let run = get(s.port, &s.admin, &format!("/api/ci/runs/{first}"))?;
        if run.pointer("/run/finished_at").is_some_and(|f| !f.is_null()) {
            assert_eq!(run.pointer("/run/conclusion").and_then(Value::as_str), Some("success"), "{run}\n{}", s.log());
            break;
        }
        anyhow::ensure!(Instant::now() < deadline, "the first run didn't finish: {run}\n{}", s.log());
        std::thread::sleep(Duration::from_millis(500));
    }
    let v1 = get(s.port, &s.admin, &format!("/api/runs/{first}"))?;
    let mut fan: Vec<(f64, String)> = v1
        .get("jobs")
        .and_then(Value::as_array)
        .map(|l| {
            l.iter()
                .filter(|j| j.get("key").and_then(Value::as_str).is_some_and(|k| k.starts_with("fan")))
                .map(|j| (j.pointer("/matrix/n").and_then(Value::as_f64).unwrap_or(-1.0), j.pointer("/matrix/os").and_then(Value::as_str).unwrap_or_default().to_owned()))
                .collect()
        })
        .unwrap_or_default();
    fan.sort_by(|a, b| a.0.total_cmp(&b.0));
    assert_eq!(fan, [(1.0, "a".to_owned()), (2.0, "a".to_owned())], "each leg's values, as the run page reads them: {v1}");
    assert!(v1.get("jobs").and_then(Value::as_array).is_some_and(|l| l.iter().filter(|j| j.get("key").and_then(Value::as_str) == Some("plan")).all(|j| j.get("matrix").is_none())), "a leg of no matrix has none: {v1}");

    // Its data flow (W04-R23): plan's output, from its step, read by fan's matrix in both of fan's
    // legs; plan's exported environment, set by its leg and taken by last.
    let flow = get(s.port, &s.admin, &format!("/api/ci/runs/{first}/flow"))?;
    let out = flow.pointer("/outputs/0").cloned().unwrap_or_default();
    assert_eq!(out.get("name").and_then(Value::as_str), Some("matrix"), "{flow}");
    assert_eq!(out.pointer("/step/id").and_then(Value::as_str), Some("m"), "{flow}");
    assert_eq!(out.pointer("/values/0/value").and_then(Value::as_str), Some(r#"{"n":[1,2],"os":["a"]}"#), "{flow}");
    let fan = out.get("readers").and_then(Value::as_array).and_then(|r| r.iter().find(|r| r.get("job").and_then(Value::as_str) == Some("fan"))).cloned().unwrap_or_default();
    assert!(fan.get("at").is_some_and(|a| a.to_string().contains("strategy.matrix")), "{flow}");
    assert_eq!(fan.get("legs").and_then(Value::as_array).map(Vec::len), Some(2), "the legs that read it: {flow}");
    let exported = flow.pointer("/exports/0").cloned().unwrap_or_default();
    assert_eq!(exported.get("names"), Some(&serde_json::json!(["PLAN_DIR"])), "{flow}");
    assert_eq!(exported.pointer("/legs/0/names"), Some(&serde_json::json!(["PLAN_DIR"])), "set by its leg: {flow}");
    assert_eq!(exported.pointer("/readers/0/job").and_then(Value::as_str), Some("last"), "{flow}");

    // The next run: how long each job took the last time it succeeded.
    std::fs::write(repo.join("README.md"), "graph, again\n")?;
    git(&repo, &["commit", "-qam", "again"])?;
    let deadline = Instant::now() + Duration::from_secs(120);
    let graph = loop {
        let runs = get(s.port, &s.admin, "/api/ci/runs?repo=acme/graph&limit=5")?;
        if let Some(id) = runs.pointer("/runs/0/id").and_then(Value::as_str)
            && id != first
        {
            break get(s.port, &s.admin, &format!("/api/ci/runs/{id}/graph"))?;
        }
        anyhow::ensure!(Instant::now() < deadline, "no second run: {runs}\n{}", s.log());
        std::thread::sleep(Duration::from_millis(300));
    };
    let typical = |id: &str| job(&graph, id).get("typical").and_then(Value::as_f64);
    assert!(typical("plan").is_some_and(|t| t >= 3.0), "plan sleeps 4 s: {graph}");
    assert!(typical("fan").is_some() && typical("last").is_some(), "{graph}");
    Ok(())
}
