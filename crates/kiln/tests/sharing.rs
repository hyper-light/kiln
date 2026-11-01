//! Sharing across repositories (docs/ci.md W29-R17 to R20), against a server of its own with its
//! own runner and real `git`: who may use a repository's actions and reusable workflows, as its
//! Access says.

#![cfg(unix)]

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use common::{Options, Server, http, start};
use serde_json::{Value, json};

/// `git` with no configuration but this test's, in `dir`.
fn git(home: &Path, dir: &Path, args: &[&str]) -> anyhow::Result<String> {
    let o = Command::new("git")
        .args(["-c", "user.name=Ada", "-c", "user.email=ada@example.com", "-c", "init.defaultBranch=main", "-c", "credential.helper=", "-c", "commit.gpgsign=false"])
        .args(args)
        .current_dir(dir)
        .env("HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GIT_ASKPASS")
        .env_remove("SSH_ASKPASS")
        .output()?;
    let out = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    anyhow::ensure!(o.status.success(), "git {args:?}: {out}");
    Ok(out)
}

fn get(s: &Server, path: &str) -> anyhow::Result<Value> {
    let (st, text) = http(s.port, "GET", path, &s.admin, None)?;
    anyhow::ensure!(st == 200, "GET {path}: {st} {text}");
    Ok(serde_json::from_str(&text)?)
}

fn at<'a>(v: &'a Value, pointer: &str) -> &'a str {
    v.pointer(pointer).and_then(Value::as_str).unwrap_or_default()
}

/// Until `name`'s first poll is done: it takes the branch as it is.
fn polled(s: &Server, name: &str) -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let v = get(s, "/api/repos")?;
        let list = v.as_array().cloned().or_else(|| v.get("repos").and_then(Value::as_array).cloned()).unwrap_or_default();
        if list.iter().any(|r| r.get("name").and_then(Value::as_str) == Some(name) && r.get("last_polled_at").is_some_and(|t| !t.is_null())) {
            return Ok(());
        }
        anyhow::ensure!(Instant::now() < deadline, "{name} wasn't polled: {v}\n{}", s.log());
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Writes `files` under `work`.
fn write(work: &Path, files: &[(&str, &str)]) -> anyhow::Result<()> {
    for (path, text) in files {
        let p = work.join(path);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(p, text)?;
    }
    Ok(())
}

/// A repository kiln hosts, cloned under `dir` (its `/` a `_`), its first commit (a README and
/// `files`) pushed and polled.
fn hosted(s: &Server, home: &Path, dir: &Path, name: &str, files: &[(&str, &str)]) -> anyhow::Result<PathBuf> {
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": name, "mode": "native" })))?;
    anyhow::ensure!(st == 200, "{name}: {st} {body}");
    let leaf = name.replace('/', "_");
    git(home, dir, &["clone", "-q", &format!("http://x:{}@127.0.0.1:{}/git/{name}.git", s.admin, s.port), &leaf])?;
    let work = dir.join(&leaf);
    write(&work, &[("README.md", &format!("{name}\n"))])?;
    write(&work, files)?;
    git(home, &work, &["add", "."])?;
    git(home, &work, &["commit", "-qm", "first"])?;
    git(home, &work, &["push", "-q", "origin", "main"])?;
    polled(s, name)?;
    Ok(work)
}

/// Writes `files` in `work`, commits them as `message` and pushes main: the commit.
fn push(home: &Path, work: &Path, message: &str, files: &[(&str, &str)]) -> anyhow::Result<String> {
    write(work, files)?;
    git(home, work, &["add", "."])?;
    git(home, work, &["commit", "-qm", message])?;
    git(home, work, &["push", "-q", "origin", "main"])?;
    Ok(git(home, work, &["rev-parse", "HEAD"])?.trim().to_owned())
}

/// `repo`'s runs at `sha`, once `n` of them have finished.
fn finished(s: &Server, repo: &str, sha: &str, n: usize) -> anyhow::Result<Vec<Value>> {
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        let v = get(s, &format!("/api/ci/runs?repo={repo}&limit=100"))?;
        let all: Vec<Value> = v.get("runs").and_then(Value::as_array).into_iter().flatten().filter(|r| at(r, "/sha") == sha).cloned().collect();
        let done = all.iter().filter(|r| r.get("finished_at").is_some_and(|f| !f.is_null())).count();
        if done >= n {
            return Ok(all);
        }
        anyhow::ensure!(Instant::now() < deadline, "{repo} at {sha}: {done} of {n} runs finished: {all:?}\n{}", s.log());
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// A run refused as it starts (W29-R19): KCI4022, naming who it isn't shared with, and no job.
fn refused(s: &Server, run: &Value, caller: &str, what: &str) -> anyhow::Result<()> {
    assert_eq!(at(run, "/conclusion"), "startup_failure", "{what}: {run}");
    let why = at(run, "/error");
    assert!(why.contains("KCI4022") && why.contains(&format!("isn't shared with `{caller}`")), "{what}: {run}");
    let detail = get(s, &format!("/api/ci/runs/{}", at(run, "/id")))?;
    assert_eq!(detail.get("legs").and_then(Value::as_array).map_or(0, Vec::len), 0, "{what}: no job ran: {detail}");
    Ok(())
}

/// Code mode's answer to `program`, as the admin.
fn code(s: &Server, program: &str) -> anyhow::Result<Value> {
    let (st, body) = http(s.port, "POST", "/api/mcp", &s.admin, Some(&json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": "code_run", "arguments": { "program": program } } })))?;
    anyhow::ensure!(st == 200, "code_run: {st} {body}");
    let v: Value = serde_json::from_str(&body)?;
    let text = v.pointer("/result/content/0/text").and_then(Value::as_str).unwrap_or_default();
    anyhow::ensure!(v.pointer("/result/isError") == Some(&json!(false)), "{program}: {text}");
    Ok(serde_json::from_str::<Value>(text)?.get("result").cloned().unwrap_or_default())
}

/// The catalog's entries, as `GET route` lists them.
fn listed(s: &Server, route: &str) -> anyhow::Result<Vec<Value>> {
    Ok(get(s, route)?.get("actions").and_then(Value::as_array).cloned().unwrap_or_default())
}

/// acme/kit's composite action and reusable workflow.
const ACTION: &str = "name: x\nruns:\n  using: composite\n  steps:\n    - run: echo shared\n      shell: bash\n";
const BUILD: &str = "name: build\non: workflow_call\njobs:\n  b:\n    runs-on: local\n    steps:\n      - run: echo built\n";

/// A caller's workflows, one use of acme/kit each: a step's action and a job's reusable workflow,
/// through `kiln://` and through a plain name (`[[ci.actions.sources]]` sends acme's to kiln).
fn callers_workflows() -> Vec<(&'static str, String)> {
    let step = |u: &str| format!("on: push\njobs:\n  a:\n    runs-on: local\n    steps:\n      - uses: {u}\n");
    let call = |u: &str| format!("on: push\njobs:\n  c:\n    uses: {u}\n");
    vec![
        (".kiln/step-kiln.yaml", step("kiln://acme/kit/.github/actions/x@main")),
        (".kiln/step-plain.yaml", step("acme/kit/.github/actions/x@main")),
        (".kiln/call-kiln.yaml", call("kiln://acme/kit/.github/workflows/build.yml@main")),
        (".kiln/call-plain.yaml", call("acme/kit/.github/workflows/build.yml@main")),
    ]
}

/// W29-R19: one check, for actions and reusable workflows alike. acme/kit's action and reusable
/// workflow, used from acme/app and from other/app through `kiln://` and through a plain name a
/// native source sends to kiln: not shared, only acme/kit's own workflows may use them; `share:
/// owner`, acme's other repositories too; `share: everyone`, everyone's. A refused use fails its
/// run as it starts, before any job, with KCI4022, the reusable workflow as the action.
#[test]
fn w29_r19_actions_and_reusable_workflows_alike() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let ci = "kiln_workflows = true\n[[ci.actions.sources]]\nowners = [\"acme\"]\nnative = true\n";
    let s = start(dir.path(), &Options { agent: true, ci, extra: "" })?;
    let home = dir.path().join("githome");
    std::fs::create_dir_all(&home)?;
    let kit = hosted(&s, &home, dir.path(), "acme/kit", &[(".github/actions/x/action.yml", ACTION), (".github/workflows/build.yml", BUILD)])?;
    let callers = ["acme/app", "other/app"];
    let mut works = Vec::new();
    for name in callers {
        works.push(hosted(&s, &home, dir.path(), name, &[])?);
    }

    // Its own workflows always may, not shared as it is.
    let own = "on: push\njobs:\n  a:\n    runs-on: local\n    steps:\n      - uses: kiln://acme/kit/.github/actions/x@main\n      - uses: acme/kit/.github/actions/x@main\n  c:\n    uses: kiln://acme/kit/.github/workflows/build.yml@main\n";
    let sha = push(&home, &kit, "its own", &[(".kiln/own.yaml", own)])?;
    let runs = finished(&s, "acme/kit", &sha, 1)?;
    assert_eq!(runs.iter().map(|r| at(r, "/conclusion")).collect::<Vec<_>>(), ["success"], "{runs:?}");

    let workflows = callers_workflows();
    // Each level, and the callers it lets use acme/kit.
    let levels: [(&str, &[&str]); 3] = [("none", &[]), ("owner", &["acme/app"]), ("everyone", &["acme/app", "other/app"])];
    for (level, may) in levels {
        if level != "none" {
            push(&home, &kit, &format!("share {level}"), &[(".kiln/config.yaml", &format!("share: {level}\n"))])?;
            // acme/kit's settings are read from its trunk and kept 20 seconds (repo_config): the
            // change is waited out.
            std::thread::sleep(Duration::from_secs(21));
        }
        for (name, work) in callers.iter().zip(&works) {
            let readme = format!("{name}, acme/kit shared {level}\n");
            let mut files: Vec<(&str, &str)> = workflows.iter().map(|(p, t)| (*p, t.as_str())).collect();
            files.push(("README.md", &readme));
            let sha = push(&home, work, &format!("uses acme/kit, shared {level}"), &files)?;
            let runs = finished(&s, name, &sha, workflows.len())?;
            for (path, _) in &workflows {
                let what = format!("{name}'s {path}, acme/kit shared {level}");
                let run = runs.iter().find(|r| at(r, "/workflow") == *path).cloned().unwrap_or_default();
                if may.contains(name) {
                    assert_eq!(at(&run, "/conclusion"), "success", "{what}: {run}\n{}", s.log());
                } else {
                    refused(&s, &run, name, &what)?;
                }
            }
        }
    }
    Ok(())
}

/// W29-R20: the catalog. With `repo`, it lists the actions and reusable workflows kiln's
/// repositories share that the repository's workflows may use, each with its kind, interface and
/// access, written as the plain name for a native owner and `kiln://` otherwise, at its newest tag;
/// not one it may not use. `ci.actions.get` gives an action's tags, its access and which
/// repositories' runs pinned which version. Code mode answers as the routes do.
#[test]
fn w29_r20_the_catalog() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let ci = "kiln_workflows = true\n[[ci.actions.sources]]\nowners = [\"acme\"]\nnative = true\n";
    let s = start(dir.path(), &Options { agent: true, ci, extra: "" })?;
    let home = dir.path().join("githome");
    std::fs::create_dir_all(&home)?;
    let hello = "name: Hello\ndescription: Says hello\ninputs:\n  who:\n    description: Whom to greet\n    required: true\noutputs:\n  said:\n    description: What it said\n    value: ${{ steps.s.outputs.said }}\nruns:\n  using: composite\n  steps:\n    - id: s\n      run: echo \"said=hello ${{ inputs.who }}\" >> \"$GITHUB_OUTPUT\"\n      shell: bash\n";
    let build = "name: build\non:\n  workflow_call:\n    inputs:\n      target:\n        type: string\n        required: true\n    outputs:\n      digest:\n        description: The image's digest\n        value: ${{ jobs.b.outputs.digest }}\n    secrets:\n      token:\n        required: true\njobs:\n  b:\n    runs-on: local\n    outputs:\n      digest: sha256:0\n    steps:\n      - run: echo built\n";
    // acme/kit shares with acme's repositories, tagged v1; other/tools shares nothing, untagged.
    let kit = hosted(&s, &home, dir.path(), "acme/kit", &[(".kiln/config.yaml", "share: owner\n"), (".github/actions/hello/action.yml", hello), (".github/workflows/build.yml", build)])?;
    git(&home, &kit, &["tag", "v1"])?;
    git(&home, &kit, &["push", "-q", "origin", "v1"])?;
    let v1 = git(&home, &kit, &["rev-parse", "v1^{commit}"])?.trim().to_owned();
    let tools = hosted(&s, &home, dir.path(), "other/tools", &[(".github/actions/lint/action.yml", "name: Lint\nruns:\n  using: composite\n  steps:\n    - run: echo linted\n      shell: bash\n")])?;
    let tools_head = git(&home, &tools, &["rev-parse", "HEAD"])?.trim().to_owned();

    // What acme/app may use: acme/kit's action and reusable workflow, not other/tools' action.
    let mine = listed(&s, "/api/ci/actions?repo=acme/app")?;
    let entry = |list: &[Value], uses: &str| list.iter().find(|e| at(e, "/uses") == uses).cloned();
    let action = entry(&mine, "acme/kit/.github/actions/hello@v1").unwrap_or_default();
    assert_eq!((at(&action, "/kind"), at(&action, "/owner"), at(&action, "/repo"), at(&action, "/path"), at(&action, "/access")), ("composite", "acme", "kit", ".github/actions/hello", "owner"), "{mine:?}");
    assert_eq!(at(&action, "/description"), "Says hello");
    assert_eq!(action.pointer("/inputs/0"), Some(&json!({ "name": "who", "description": "Whom to greet", "required": true, "default": null, "type": null })), "{action}");
    assert_eq!(action.pointer("/outputs/0/name"), Some(&json!("said")), "{action}");
    let workflow = entry(&mine, "acme/kit/.github/workflows/build.yml@v1").unwrap_or_default();
    assert_eq!((at(&workflow, "/kind"), at(&workflow, "/path"), at(&workflow, "/access")), ("workflow", ".github/workflows/build.yml", "owner"), "{mine:?}");
    assert_eq!(workflow.pointer("/inputs/0"), Some(&json!({ "name": "target", "description": null, "required": true, "default": null, "type": "string" })), "{workflow}");
    assert_eq!((workflow.pointer("/outputs/0/name"), workflow.pointer("/outputs/0/description")), (Some(&json!("digest")), Some(&json!("The image's digest"))), "{workflow}");
    assert_eq!(workflow.pointer("/secrets/0"), Some(&json!({ "name": "token", "description": null, "required": true, "default": null, "type": null })), "{workflow}");
    assert!(!mine.iter().any(|e| at(e, "/owner") == "other"), "other/tools shares nothing with acme/app: {mine:?}");
    // A native owner's repositories are listed as kiln's own, never as mirrors.
    assert!(mine.iter().filter(|e| at(e, "/uses").starts_with("acme/")).all(|e| e.get("kind").is_some()), "{mine:?}");

    // Another owner's repository may use neither of acme/kit's; other/tools' own workflows may
    // use its action, named by `kiln://` at its commit (no tag, and not a native owner).
    let theirs = listed(&s, "/api/ci/actions?repo=other/tools")?;
    assert!(!theirs.iter().any(|e| at(e, "/owner") == "acme"), "{theirs:?}");
    let lint = entry(&theirs, &format!("kiln://other/tools/.github/actions/lint@{tools_head}")).unwrap_or_default();
    assert_eq!((at(&lint, "/kind"), at(&lint, "/access")), ("composite", "none"), "{theirs:?}");
    // `kind=` and `owner=`.
    let workflows = listed(&s, "/api/ci/actions?repo=acme/app&kind=workflow")?;
    assert_eq!(workflows.iter().map(|e| at(e, "/uses")).collect::<Vec<_>>(), ["acme/kit/.github/workflows/build.yml@v1"], "{workflows:?}");
    let others = listed(&s, "/api/ci/actions?owner=other")?;
    assert_eq!(others.iter().map(|e| at(e, "/uses").to_owned()).collect::<Vec<_>>(), [format!("kiln://other/tools/.github/actions/lint@{tools_head}")], "{others:?}");

    // acme/app runs acme/kit's action at v1: its consumers say so.
    let app = hosted(&s, &home, dir.path(), "acme/app", &[])?;
    let greet = "on: push\njobs:\n  a:\n    runs-on: local\n    steps:\n      - uses: acme/kit/.github/actions/hello@v1\n        with:\n          who: acme/app\n";
    let sha = push(&home, &app, "greets", &[(".kiln/greet.yaml", greet)])?;
    let runs = finished(&s, "acme/app", &sha, 1)?;
    assert_eq!(runs.iter().map(|r| at(r, "/conclusion")).collect::<Vec<_>>(), ["success"], "{runs:?}\n{}", s.log());
    let one = get(&s, "/api/ci/actions/acme/kit?path=.github/actions/hello")?;
    assert_eq!((one.get("hosted"), at(&one, "/access")), (Some(&json!(true)), "owner"), "{one}");
    assert_eq!(one.pointer("/entries/0/uses"), Some(&json!("acme/kit/.github/actions/hello@v1")), "{one}");
    let versions: Vec<(String, String)> = one.get("versions").and_then(Value::as_array).into_iter().flatten().map(|v| (at(v, "/ref").to_owned(), at(v, "/sha").to_owned())).collect();
    assert_eq!(versions, [("v1".to_owned(), v1.clone())], "{one}");
    assert!(one.pointer("/versions/0/checks").is_some_and(Value::is_array), "{one}");
    let consumers: Vec<(String, String, String, String)> = one.get("consumers").and_then(Value::as_array).into_iter().flatten().map(|c| (at(c, "/repo").to_owned(), at(c, "/kind").to_owned(), at(c, "/ref").to_owned(), at(c, "/sha").to_owned())).collect();
    assert_eq!(consumers, [("acme/app".to_owned(), "action".to_owned(), "v1".to_owned(), v1.clone())], "{one}");
    // The repository as a whole: both of its, and no one consumes its reusable workflow yet.
    let whole = get(&s, "/api/ci/actions/acme/kit")?;
    assert_eq!(whole.get("entries").and_then(Value::as_array).map(Vec::len), Some(2), "{whole}");
    let (st, _) = http(s.port, "GET", "/api/ci/actions/nobody/nothing", &s.admin, None)?;
    assert_eq!(st, 404);

    // Code mode answers as the routes do.
    for (program, route) in [
        ("return await kiln.ci.actions.list({ repo: 'acme/app' })", "/api/ci/actions?repo=acme/app"),
        ("return await kiln.ci.actions.list({ owner: 'other' })", "/api/ci/actions?owner=other"),
        ("return await kiln.ci.actions.get({ repo: 'acme/kit', path: '.github/actions/hello' })", "/api/ci/actions/acme/kit?path=.github/actions/hello"),
    ] {
        assert_eq!(code(&s, program)?, get(&s, route)?, "{program} answers as GET {route}");
    }
    Ok(())
}
