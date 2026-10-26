//! W04-AC07 (docs/ci.md): for the plan fixtures F-10 to F-15 (`tests/fixtures/plan`), the plan
//! preview's legs, decided conditions and removed combinations are the legs a run of each
//! creates. The server has three runners, each of this test build posing as a runner of one of
//! F-10's platforms (`kiln agent --chaos platform=…`), so every leg runs on whatever machine this
//! is. Every run must succeed too, which checks the rest of W04 end to end: S04's shards balanced
//! by recorded times (F-11), AC04's reusable workflows three deep with typed inputs, inherited
//! secrets and outputs (F-14), and AC06's typed, plain and secret outputs, `exports` and
//! `env-from`, `run.env` and `legs` (F-15).

#![cfg(all(unix, feature = "chaos"))]

mod common;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use common::{Options, Server, http, start};
use serde_json::{Value, json};

/// F-10's platforms, and the runner posing as each.
const PLATFORMS: [(&str, &str, &str); 3] = [("linux/amd64", "linux", "x86_64"), ("linux/arm64", "linux", "aarch64"), ("darwin/arm64", "macos", "aarch64")];

fn runner_name(platform: &str) -> String {
    format!("posing-{}", platform.replace('/', "-"))
}

/// A runner, killed when the test ends.
struct Runner(Child);

impl Drop for Runner {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn git(dir: &Path, args: &[&str]) -> anyhow::Result<String> {
    let o = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()?;
    anyhow::ensure!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
    Ok(String::from_utf8_lossy(&o.stdout).trim().to_owned())
}

/// `from`'s files into `to`, directories and all.
fn copy_tree(from: &Path, to: &Path) -> anyhow::Result<()> {
    for e in std::fs::read_dir(from)? {
        let e = e?;
        let target = to.join(e.file_name());
        if e.file_type()?.is_dir() {
            std::fs::create_dir_all(&target)?;
            copy_tree(&e.path(), &target)?;
        } else {
            std::fs::copy(e.path(), &target)?;
        }
    }
    Ok(())
}

fn until<T>(secs: u64, what: &str, mut f: impl FnMut() -> anyhow::Result<Result<T, String>>) -> anyhow::Result<T> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        match f()? {
            Ok(t) => return Ok(t),
            Err(why) if Instant::now() > deadline => anyhow::bail!("{what}: {why}"),
            Err(_) => std::thread::sleep(Duration::from_millis(500)),
        }
    }
}

fn get(s: &Server, path: &str) -> anyhow::Result<Value> {
    let (st, body) = http(s.port, "GET", path, &s.admin, None)?;
    anyhow::ensure!(st == 200, "GET {path}: {st} {body}");
    Ok(serde_json::from_str(&body)?)
}

fn text<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or_default()
}

/// The finished run of `path` at `sha`, and its detail.
fn finished_run(s: &Server, sha: &str, path: &str, secs: u64) -> anyhow::Result<Value> {
    let id = until(secs, &format!("{path} finishes at {sha}"), || {
        let runs = get(s, "/api/ci/runs?repo=acme/plans&limit=100")?;
        let run = runs.get("runs").and_then(Value::as_array).into_iter().flatten().find(|r| text(r, "sha") == sha && text(r, "workflow").ends_with(path)).cloned();
        Ok(match run {
            Some(r) if r.get("finished_at").is_some_and(|f| !f.is_null()) => Ok(text(&r, "id").to_owned()),
            Some(r) => Err(format!("{r}\n{}", s.log())),
            None => Err(format!("no run yet: {runs}\n{}", s.log())),
        })
    })?;
    get(s, &format!("/api/ci/runs/{id}"))
}

/// What the preview says of `path` at `sha`, for a push.
fn preview(s: &Server, sha: &str, path: &str) -> anyhow::Result<Value> {
    let (st, body) = http(s.port, "POST", "/api/ci/plan-preview", &s.admin, Some(&json!({ "repo": "acme/plans", "ref": "main", "sha": sha, "path": path, "event": "push" })))?;
    anyhow::ensure!(st == 200, "preview of {path}: {st} {body}");
    let v: Value = serde_json::from_str(&body)?;
    anyhow::ensure!(v.get("runs") == Some(&json!(true)), "the push runs {path}: {v}");
    Ok(v.get("plan").cloned().unwrap_or_default())
}

/// AC07: the preview's legs, decisions and removed combinations are the run's. Its legs are the
/// run's (key, job, name and matrix); each decision is a leg the run settled the same way, for the
/// same reason; the jobs it leaves for later are the rest of the run's legs; and nothing else.
fn same_as_run(path: &str, plan: &Value, detail: &Value) -> Result<(), String> {
    let legs: Vec<Value> = detail.get("legs").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let find = |key: &str| legs.iter().find(|l| text(l, "key") == key);
    for p in plan.get("legs").and_then(Value::as_array).into_iter().flatten() {
        let key = text(p, "key");
        let Some(l) = find(key) else { return Err(format!("{path}: the preview's leg `{key}` isn't the run's: {legs:?}")) };
        for (a, b) in [("job", "job"), ("name", "name"), ("matrix", "matrix")] {
            let (want, got) = (p.get(a).cloned().unwrap_or(Value::Null), l.get(b).cloned().unwrap_or(Value::Null));
            if want != got && !(want.is_null() && got == json!({})) && !(got.is_null() && want == json!({})) {
                return Err(format!("{path}: `{key}`'s {a} is {want} in the preview and {got} in the run"));
            }
        }
        seen.insert(key.to_owned());
    }
    for d in plan.get("decided").and_then(Value::as_array).into_iter().flatten() {
        let key = d.get("leg").and_then(Value::as_str).unwrap_or_else(|| text(d, "job"));
        let Some(l) = find(key) else { return Err(format!("{path}: the preview decided `{key}`, which the run hasn't: {legs:?}")) };
        if text(l, "status") != text(d, "conclusion") || text(l, "error") != text(d, "reason") {
            return Err(format!("{path}: the preview decided `{key}` {} ({}); the run, {} ({})", text(d, "conclusion"), text(d, "reason"), text(l, "status"), text(l, "error")));
        }
        seen.insert(key.to_owned());
    }
    let later: Vec<&str> = plan.get("later").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
    for l in &legs {
        let key = text(l, "key");
        if !seen.contains(key) && !later.contains(&text(l, "job")) {
            return Err(format!("{path}: the run's leg `{key}` isn't in the preview: {plan}"));
        }
    }
    for job in &later {
        if !legs.iter().any(|l| text(l, "job") == *job) {
            return Err(format!("{path}: the preview left `{job}` for later, and the run never made it"));
        }
    }
    let removed = plan.get("removed").cloned().unwrap_or(Value::Null);
    let ran = detail.get("removed").cloned().unwrap_or(Value::Null);
    let none = |v: &Value| v.is_null() || v.as_object().is_some_and(serde_json::Map::is_empty);
    if removed != ran && !(none(&removed) && none(&ran)) {
        return Err(format!("{path}: the preview removed {removed}, the run {ran}"));
    }
    Ok(())
}

fn names_of(plan: &Value, job: &str) -> Vec<String> {
    plan.get("legs").and_then(Value::as_array).into_iter().flatten().filter(|l| text(l, "job") == job).map(|l| text(l, "name").to_owned()).collect()
}

#[test]
fn w04_ac07_previews_are_runs() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { agent: false, ci: "kiln_workflows = true\ngithub_workflows = \"on\"", extra: "" })?;
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/plans", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "native repository: {st} {body}");
    // F-14 inherits a repository secret three levels down.
    let (st, body) = http(s.port, "PUT", "/api/ci/secrets/F14_SECRET?scope=repo:acme/plans", &s.admin, Some(&json!({ "value": "inherited-three-deep" })))?;
    anyhow::ensure!(st == 200 || st == 204, "the secret: {st} {body}");

    // Three runners, one posing as each of F-10's platforms. The toolchain stays where it is
    // though each runner has a home of its own (F-10 builds with `rustc`).
    let join = std::fs::read_to_string(dir.path().join("data/join.token"))?.trim().to_owned();
    let real_home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    let rustup = std::env::var_os("RUSTUP_HOME").map_or_else(|| real_home.join(".rustup"), PathBuf::from);
    let cargo = std::env::var_os("CARGO_HOME").map_or_else(|| real_home.join(".cargo"), PathBuf::from);
    let mut runners = Vec::new();
    for (platform, _, _) in PLATFORMS {
        let name = runner_name(platform);
        let base = dir.path().join(&name);
        std::fs::create_dir_all(base.join("home"))?;
        let log = std::fs::File::create(base.join("agent.log"))?;
        let child = common::kiln()
            .args(["agent", "--server", &format!("http://127.0.0.1:{}", s.port), "--token", &join, "--name", &name, "--labels", "posing", "--slots", "3", "--no-terminal", "--chaos", &format!("platform={platform}")])
            .arg("--work-dir")
            .arg(base.join("work"))
            .arg("--state-dir")
            .arg(base.join("state"))
            .env("HOME", base.join("home"))
            .env("RUSTUP_HOME", &rustup)
            .env("CARGO_HOME", &cargo)
            .env("NO_COLOR", "1")
            .env_remove("KILN_CONFIG")
            .env_remove("KILN_URL")
            .env_remove("KILN_ADMIN_TOKEN")
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log))
            .spawn()?;
        runners.push(Runner(child));
    }
    let agent_logs = || PLATFORMS.iter().map(|(p, _, _)| std::fs::read_to_string(dir.path().join(runner_name(p)).join("agent.log")).unwrap_or_default()).collect::<Vec<_>>().join("\n---\n");
    until(180, "the runners connect with the platforms they pose as", || {
        let agents = get(&s, "/api/agents")?;
        let list = agents.as_array().cloned().or_else(|| agents.get("agents").and_then(Value::as_array).cloned()).unwrap_or_default();
        let posed = PLATFORMS.iter().all(|(p, os, arch)| {
            list.iter().any(|a| text(a, "name") == runner_name(p) && a.get("connected") == Some(&json!(true)) && a.pointer("/facts/os") == Some(&json!(os)) && a.pointer("/facts/arch") == Some(&json!(arch)))
        });
        Ok(if posed { Ok(()) } else { Err(format!("{agents}\n{}", agent_logs())) })
    })?;

    // The repository: a first commit kiln takes as it is, then F-11 alone, whose run records the
    // times the next one shards by, then every fixture.
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/plan");
    let work = dir.path().join("work-tree");
    std::fs::create_dir_all(&work)?;
    git(&work, &["init", "-q", "-b", "main"])?;
    std::fs::write(work.join("README.md"), "plans\n")?;
    git(&work, &["add", "."])?;
    git(&work, &["commit", "-qm", "first"])?;
    let url = format!("http://x:{}@127.0.0.1:{}/git/acme/plans.git", s.admin, s.port);
    git(&work, &["push", "-q", &url, "main"])?;
    until(60, "the first poll", || {
        let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        Ok(if body.contains("\"last_polled_at\":null") { Err(body) } else { Ok(()) })
    })?;
    copy_tree(&fixtures.join("f11-timing-shards"), &work)?;
    std::fs::create_dir_all(work.join("tests"))?;
    for i in 0..40 {
        std::fs::write(work.join(format!("tests/t{i:02}_test.py")), format!("def test_{i}():\n    pass\n"))?;
    }
    git(&work, &["add", "."])?;
    git(&work, &["commit", "-qm", "F-11"])?;
    git(&work, &["push", "-q", &url, "main"])?;
    let first = git(&work, &["rev-parse", "HEAD"])?;
    let plan = preview(&s, &first, ".kiln/shards.yaml")?;
    let run = finished_run(&s, &first, ".kiln/shards.yaml", 600)?;
    anyhow::ensure!(run.pointer("/run/conclusion") == Some(&json!("success")), "F-11's first run: {run}\n{}", s.log());
    same_as_run(".kiln/shards.yaml (no history)", &plan, &run).map_err(anyhow::Error::msg)?;
    assert_eq!(names_of(&plan, "test").len(), 1, "no history: one leg of all forty");

    for f in ["f10-cross-arch", "f12-matrices", "f13-dynamic", "f14-reusable", "f15-outputs"] {
        copy_tree(&fixtures.join(f), &work)?;
    }
    git(&work, &["add", "."])?;
    git(&work, &["commit", "-qm", "F-10 to F-15"])?;
    git(&work, &["push", "-q", &url, "main"])?;
    let sha = git(&work, &["rev-parse", "HEAD"])?;
    let paths = [".kiln/cross-arch.yaml", ".kiln/shards.yaml", ".github/workflows/matrices.yml", ".github/workflows/dynamic.yml", ".github/workflows/reusable.yml", ".kiln/outputs.yaml"];
    let mut failures: Vec<String> = Vec::new();
    let mut check = |ok: bool, what: String| {
        if !ok {
            failures.push(what);
        }
    };
    let began = Instant::now();
    for path in paths {
        let plan = preview(&s, &sha, path)?;
        let run = finished_run(&s, &sha, path, 1200)?;
        eprintln!("{path}: {} after {:?}", run.pointer("/run/conclusion").and_then(Value::as_str).unwrap_or("?"), began.elapsed());
        if run.pointer("/run/conclusion") != Some(&json!("success")) {
            check(false, format!("{path} didn't succeed: {run}"));
            continue;
        }
        if let Err(e) = same_as_run(path, &plan, &run) {
            check(false, e);
        }
        match path {
            // F-10: each build and test on the runner of its platform; one collect.
            ".kiln/cross-arch.yaml" => {
                for l in run.get("legs").and_then(Value::as_array).into_iter().flatten().filter(|l| text(l, "job") != "collect") {
                    let platform = l.pointer("/matrix/platform").and_then(Value::as_str).unwrap_or_default();
                    check(text(l, "runner") == runner_name(platform), format!("F-10's `{}` ran on {}", text(l, "key"), text(l, "runner")));
                }
                check((names_of(&plan, "build").len(), names_of(&plan, "test").len(), names_of(&plan, "collect").len()) == (3, 3, 1), format!("F-10's legs: {plan}"));
            }
            // F-11 with history: forty minutes in legs of five.
            ".kiln/shards.yaml" => {
                let legs = plan.get("legs").and_then(Value::as_array).cloned().unwrap_or_default();
                check(legs.len() == 8 && legs.iter().all(|l| l.get("items").and_then(Value::as_array).map(Vec::len) == Some(5)), format!("F-11 with history, eight legs of five: {plan}"));
            }
            // F-12: the legs GitHub documents (the preview lists them by key; AC01's unit tests
            // check their order).
            ".github/workflows/matrices.yml" => {
                let want: [(&str, &[&str]); 9] = [
                    ("single", &["single (10)", "single (12)", "single (14)"]),
                    ("multi", &["multi (ubuntu-22.04, 10)", "multi (ubuntu-22.04, 12)", "multi (ubuntu-22.04, 14)", "multi (ubuntu-20.04, 10)", "multi (ubuntu-20.04, 12)", "multi (ubuntu-20.04, 14)"]),
                    ("expand", &["expand (apple, cat, pink, circle)", "expand (apple, dog, green, circle)", "expand (pear, cat, pink)", "expand (pear, dog, green)", "expand (banana)", "expand (banana, cat)"]),
                    ("extend", &["extend (windows-latest, 14)", "extend (windows-latest, 16, 6)", "extend (ubuntu-latest, 14)", "extend (ubuntu-latest, 16)"]),
                    (
                        "add",
                        &[
                            "add (macos-latest, 12)",
                            "add (macos-latest, 14)",
                            "add (macos-latest, 16)",
                            "add (windows-latest, 12)",
                            "add (windows-latest, 14)",
                            "add (windows-latest, 16)",
                            "add (ubuntu-latest, 12)",
                            "add (ubuntu-latest, 14)",
                            "add (ubuntu-latest, 16)",
                            "add (windows-latest, 17)",
                        ],
                    ),
                    ("only-include", &["only-include (production, site-a)", "only-include (staging, site-b)"]),
                    (
                        "exclude",
                        &[
                            "exclude (macos-latest, 12, staging)",
                            "exclude (macos-latest, 14, staging)",
                            "exclude (macos-latest, 14, production)",
                            "exclude (macos-latest, 16, staging)",
                            "exclude (macos-latest, 16, production)",
                            "exclude (windows-latest, 12, staging)",
                            "exclude (windows-latest, 12, production)",
                            "exclude (windows-latest, 14, staging)",
                            "exclude (windows-latest, 14, production)",
                        ],
                    ),
                    (
                        "objects",
                        &[
                            r#"objects (ubuntu-latest, {"version":14})"#,
                            r#"objects (ubuntu-latest, {"version":20,"env":"NODE_OPTIONS=--openssl-legacy-provider"})"#,
                            r#"objects (macos-latest, {"version":14})"#,
                            r#"objects (macos-latest, {"version":20,"env":"NODE_OPTIONS=--openssl-legacy-provider"})"#,
                        ],
                    ),
                    ("experimental", &["experimental (6, false)", "experimental (7, false)", "experimental (8, false)", "experimental (9, true)"]),
                ];
                for (job, legs) in want {
                    let (mut got, mut want) = (names_of(&plan, job), legs.iter().map(|l| (*l).to_owned()).collect::<Vec<_>>());
                    got.sort();
                    want.sort();
                    check(got == want, format!("F-12's {job}: {got:?}, not {want:?}"));
                }
                check(plan.pointer("/removed/exclude").and_then(Value::as_array).map(Vec::len) == Some(3), format!("F-12's removed: {plan}"));
            }
            // F-13: `test`'s legs once `plan` said which; `docs` skipped from the start.
            ".github/workflows/dynamic.yml" => {
                check(plan.get("later") == Some(&json!(["test"])), format!("F-13's later: {plan}"));
                let mut tests: Vec<&str> = run.get("legs").and_then(Value::as_array).into_iter().flatten().filter(|l| text(l, "job") == "test").map(|l| text(l, "name")).collect();
                tests.sort_unstable();
                check(tests == ["test (cli)", "test (core)", "test (ui)"], format!("F-13's tests: {tests:?}"));
                check(plan.get("decided").and_then(Value::as_array).is_some_and(|d| d.iter().any(|d| text(d, "job") == "docs" && text(d, "conclusion") == "skipped")), format!("F-13's docs: {plan}"));
            }
            // F-14: three levels down and back.
            ".github/workflows/reusable.yml" => check(names_of(&plan, "call/down/leaf/work") == ["call / down / leaf / work"], format!("F-14's legs: {plan}")),
            _ => {}
        }
    }
    drop(check);
    drop(runners);
    assert!(failures.is_empty(), "{}\n{}", failures.join("\n"), s.log());
    Ok(())
}
