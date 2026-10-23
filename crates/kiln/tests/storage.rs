//! W08-AC01 (docs/ci.md): the published `@actions/cache` and `@actions/artifact` clients (bundled
//! in `fixtures/storage-action`, pinned) against a test server's runtime services. Needs Node 24 on
//! `PATH`; the 2 GiB upload runs when `KILN_TEST_BIG=1`.

#![cfg(unix)]

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use common::{Options, Server, http, start};
use serde_json::{Value, json};

fn git(home: &Path, dir: &Path, args: &[&str]) -> anyhow::Result<String> {
    let o = Command::new("git")
        .args(["-c", "user.name=Test", "-c", "user.email=test@example.com", "-c", "init.defaultBranch=main", "-c", "credential.helper="])
        .args(args)
        .current_dir(dir)
        .env("HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()?;
    let out = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    anyhow::ensure!(o.status.success(), "git {args:?}: {out}");
    Ok(out)
}

fn until(s: &Server, secs: u64, what: &str, mut f: impl FnMut() -> anyhow::Result<Option<String>>) -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        match f()? {
            None => return Ok(()),
            Some(last) => anyhow::ensure!(Instant::now() < deadline, "{what}: {last}\n{}", s.log()),
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Node 24, as actions' `node24` runs on.
fn node() -> Option<PathBuf> {
    let node = std::env::var_os("PATH").and_then(|p| std::env::split_paths(&p).map(|d| d.join("node")).find(|n| n.is_file()))?;
    let version = Command::new(&node).arg("--version").output().ok()?;
    String::from_utf8_lossy(&version.stdout).starts_with("v24.").then_some(node)
}

/// The newest run of `branch` once it's finished: its leg's conclusion and its steps' logs.
fn finished(s: &Server, branch: &str, secs: u64) -> anyhow::Result<(String, String)> {
    let mut leg = String::new();
    until(s, secs, &format!("the run on {branch}"), || {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/storage&limit=20", &s.admin, None)?;
        let runs = serde_json::from_str::<Value>(&body)?.get("runs").and_then(Value::as_array).cloned().unwrap_or_default();
        let Some(run) = runs.iter().find(|r| r.get("branch").and_then(Value::as_str) == Some(branch)) else { return Ok(Some(body)) };
        if run.get("finished_at").is_none_or(Value::is_null) {
            return Ok(Some(body));
        }
        let id = run.get("id").and_then(Value::as_str).unwrap_or_default();
        let (_, detail) = http(s.port, "GET", &format!("/api/ci/runs/{id}"), &s.admin, None)?;
        leg = serde_json::from_str::<Value>(&detail)?.pointer("/legs/0/id").and_then(Value::as_str).unwrap_or_default().to_owned();
        Ok(leg.is_empty().then_some(detail))
    })?;
    let (_, about) = http(s.port, "GET", &format!("/api/ci/legs/{leg}"), &s.admin, None)?;
    let about: Value = serde_json::from_str(&about)?;
    let mut log = String::new();
    for step in about.get("steps").and_then(Value::as_array).into_iter().flatten().filter_map(|x| x.get("idx").and_then(Value::as_u64)) {
        let (_, text) = http(s.port, "GET", &format!("/api/ci/legs/{leg}/logs?step={step}"), &s.admin, None)?;
        log.push_str(&text);
    }
    Ok((about.pointer("/leg/conclusion").and_then(Value::as_str).unwrap_or_default().to_owned(), log))
}

/// The steps that use the clients, on the default branch: the cache's save (twice: the second
/// finds it there), restore by key, by prefix and a miss; artifacts of one file and of a thousand
/// (and of 2 GiB, when `big`), listed, downloaded by name and by id, one deleted.
fn on_main(fixture: &Path, big: bool) -> String {
    let use_ = |with: &str| format!("      - uses: ./.github/actions/storage\n        with: {{ {with} }}\n");
    let mut w = format!(
        "on:\n  push: {{ branches: [main] }}\njobs:\n  storage:\n    runs-on: local\n    steps:\n      - run: mkdir -p .github/actions && cp -r \"{}\" .github/actions/storage && mkdir -p data && echo one > data/a\n",
        fixture.display()
    );
    for with in ["op: save, key: k-1, paths: data", "op: save, key: k-1, paths: data"] {
        w.push_str(&use_(with));
    }
    w.push_str("      - run: rm -rf data\n");
    for with in ["op: restore, key: k-1, paths: data", "op: restore, key: k-nope, restore-keys: k-, paths: data", "op: restore, key: other, paths: data"] {
        w.push_str(&use_(with));
    }
    w.push_str("      - run: |\n          test \"$(cat data/a)\" = one && echo restored-contents-ok\n          mkdir -p one many && echo hello > one/file.txt\n          for i in $(seq 1 1000); do echo \"$i\" > many/f$i.txt; done\n");
    if big {
        w.push_str("          mkdir -p big && head -c 2147483648 /dev/urandom > big/blob\n");
    }
    w.push_str("      - id: one\n        uses: ./.github/actions/storage\n        with: { op: upload, name: one, root: one }\n");
    w.push_str(&use_("op: upload, name: many, root: many"));
    if big {
        w.push_str(&use_("op: upload, name: big, root: big"));
        w.push_str(&use_("op: download, name: big, to: got-big"));
    }
    for with in ["op: list", "op: download, name: many, to: got-many", "op: download, id: \"${{ steps.one.outputs.id }}\", to: got-one", "op: delete, name: one", "op: list"] {
        w.push_str(&use_(with));
    }
    w
}

/// W08-AC01 with the published clients.
#[test]
fn w08_ac01_the_published_storage_clients() -> anyhow::Result<()> {
    let Some(node) = node() else {
        eprintln!("skipped: no Node 24 on PATH");
        return Ok(());
    };
    let big = std::env::var("KILN_TEST_BIG").is_ok_and(|v| v == "1");
    // Beside the build, not tmpfs: the 2 GiB upload is written twice.
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))?;
    // `node24` actions run on this Node, not one downloaded.
    let bin = dir.path().join("work/_externals/node24/bin");
    std::fs::create_dir_all(&bin)?;
    std::os::unix::fs::symlink(&node, bin.join("node"))?;
    let s = start(dir.path(), &Options { agent: true, ci: "", extra: "" })?;
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/storage", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let (home, work) = (dir.path().join("githome"), dir.path().join("tree"));
    std::fs::create_dir_all(&home)?;
    std::fs::create_dir_all(work.join(".github/workflows"))?;
    let push = format!("http://x:{}@127.0.0.1:{}/git/acme/storage.git", s.admin, s.port);
    git(&home, &work, &["init", "-q", "-b", "main"])?;
    std::fs::write(work.join("README.md"), "storage\n")?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "first"])?;
    git(&home, &work, &["push", "-q", &push, "HEAD:main"])?;
    until(&s, 60, "the first poll", || {
        let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        Ok(body.contains("\"last_polled_at\":null").then_some(body))
    })?;
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/storage-action");
    std::fs::write(work.join(".github/workflows/storage.yml"), on_main(&fixture, big))?;
    // On another branch: the default branch's cache, by scope fallback.
    let fallback = format!(
        "on:\n  push: {{ branches: [feature] }}\njobs:\n  fallback:\n    runs-on: local\n    steps:\n      - run: mkdir -p .github/actions && cp -r \"{}\" .github/actions/storage\n      - uses: ./.github/actions/storage\n        with: {{ op: restore, key: k-1, paths: data }}\n",
        fixture.display()
    );
    std::fs::write(work.join(".github/workflows/fallback.yml"), fallback)?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "storage"])?;
    git(&home, &work, &["push", "-q", &push, "HEAD:main"])?;
    let (conclusion, log) = finished(&s, "main", if big { 1800 } else { 300 })?;
    assert_eq!(conclusion, "success", "{log}\n{}", s.log());
    let said = |what: &str| log.contains(what);
    let first_save = log.lines().find_map(|l| l.split("saved k-1: ").nth(1)).unwrap_or_default().trim().to_owned();
    assert!(!first_save.is_empty() && first_save != "-1", "saved: {log}");
    assert!(said("saved k-1: -1"), "the second save finds it there: {log}");
    assert!(said("restored k-1: k-1") && said("restored k-nope: k-1") && said("restored other: miss") && said("restored-contents-ok"), "restores by key, by prefix, a miss: {log}");
    assert!(said("uploaded one: 1 files") && said("uploaded many: 1000 files"), "{log}");
    let lists: Vec<&str> = log.lines().filter_map(|l| l.split("listed: ").nth(1)).map(str::trim).collect();
    let (before, after) = if big { ("big,many,one", "big,many") } else { ("many,one", "many") };
    assert_eq!((lists.first(), lists.last()), (Some(&before), Some(&after)), "listed, then the deleted one gone: {log}");
    assert!(said("downloaded by name: 1000 files") && said("downloaded by id: 1 files") && said("deleted one: true"), "{log}");
    if big {
        assert!(said("uploaded big: 1 files, 2147483648 bytes") || said("uploaded big: 1 files"), "{log}");
        assert!(said("downloaded by name: 1 files, 2147483648 bytes"), "{log}");
    }
    // The other branch restores the default branch's entry.
    git(&home, &work, &["checkout", "-q", "-b", "feature"])?;
    std::fs::write(work.join("README.md"), "storage, on a branch\n")?;
    git(&home, &work, &["commit", "-qam", "feature"])?;
    git(&home, &work, &["push", "-q", &push, "HEAD:feature"])?;
    let (conclusion, log) = finished(&s, "feature", 300)?;
    assert_eq!(conclusion, "success", "{log}");
    assert!(log.contains("restored k-1: k-1"), "a branch restores its default branch's cache: {log}");
    Ok(())
}

/// Every leg of `run`, once the run has finished: its job's key, conclusion and steps' logs.
fn legs(s: &Server, run: &str) -> anyhow::Result<Vec<(String, String, String)>> {
    let (_, detail) = http(s.port, "GET", &format!("/api/ci/runs/{run}"), &s.admin, None)?;
    let mut out = Vec::new();
    for l in serde_json::from_str::<Value>(&detail)?.get("legs").and_then(Value::as_array).cloned().unwrap_or_default() {
        let id = l.get("id").and_then(Value::as_str).unwrap_or_default();
        let (_, about) = http(s.port, "GET", &format!("/api/ci/legs/{id}"), &s.admin, None)?;
        let about: Value = serde_json::from_str(&about)?;
        let mut log = String::new();
        for step in about.get("steps").and_then(Value::as_array).into_iter().flatten().filter_map(|x| x.get("idx").and_then(Value::as_u64)) {
            let (_, text) = http(s.port, "GET", &format!("/api/ci/legs/{id}/logs?step={step}"), &s.admin, None)?;
            log.push_str(&text);
        }
        let key = about.pointer("/leg/key").and_then(Value::as_str).unwrap_or_default().to_owned();
        out.push((key, about.pointer("/leg/conclusion").and_then(Value::as_str).unwrap_or_default().to_owned(), log));
    }
    Ok(out)
}

/// The newest run for `event`, once it has finished.
fn finished_event(s: &Server, event: &str, workflow: &str, secs: u64) -> anyhow::Result<Value> {
    let mut found = Value::Null;
    until(s, secs, &format!("a {event} run of {workflow}"), || {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/storage&limit=20", &s.admin, None)?;
        let runs = serde_json::from_str::<Value>(&body)?.get("runs").and_then(Value::as_array).cloned().unwrap_or_default();
        let run = runs.into_iter().find(|r| r.get("event").and_then(Value::as_str) == Some(event) && serde_json::to_string(r).unwrap_or_default().contains(workflow));
        match run {
            Some(r) if r.get("finished_at").is_some_and(|f| !f.is_null()) => {
                found = r;
                Ok(None)
            }
            _ => Ok(Some(body)),
        }
    })?;
    Ok(found)
}

/// W08-AC09: cache access as the published client sees it. A dispatched run's jobs, unset
/// (`write`), `read`, `write-only` and `none`: each has its `ACTIONS_CACHE_MODE`, and the client
/// restores and saves as the mode allows, saying what it skipped. A `pull_request_target` run
/// restores the default branch's entry and can't save there. A called workflow asking for more
/// than its caller allows doesn't start.
#[test]
fn w08_ac09_cache_access_by_event_and_mode() -> anyhow::Result<()> {
    let Some(node) = node() else {
        eprintln!("skipped: no Node 24 on PATH");
        return Ok(());
    };
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))?;
    let bin = dir.path().join("work/_externals/node24/bin");
    std::fs::create_dir_all(&bin)?;
    std::os::unix::fs::symlink(&node, bin.join("node"))?;
    let s = start(dir.path(), &Options { agent: true, ci: "", extra: "" })?;
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/storage", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let (home, work) = (dir.path().join("githome"), dir.path().join("tree"));
    std::fs::create_dir_all(&home)?;
    std::fs::create_dir_all(work.join(".github/workflows"))?;
    let push = format!("http://x:{}@127.0.0.1:{}/git/acme/storage.git", s.admin, s.port);
    git(&home, &work, &["init", "-q", "-b", "main"])?;
    std::fs::write(work.join("README.md"), "storage\n")?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "first"])?;
    git(&home, &work, &["push", "-q", &push, "HEAD:main"])?;
    until(&s, 60, "the first poll", || {
        let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        Ok(body.contains("\"last_polled_at\":null").then_some(body))
    })?;
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/storage-action");
    let prep = format!("      - run: mkdir -p .github/actions && cp -r \"{}\" .github/actions/storage && mkdir -p data && echo one > data/a && echo \"mode=$ACTIONS_CACHE_MODE\"\n", fixture.display());
    let use_ = |with: &str| format!("      - uses: ./.github/actions/storage\n        with: {{ {with} }}\n");
    let job = |name: &str, mode: Option<&str>, ops: &[&str]| {
        let mut j = format!("  {name}:\n    runs-on: local\n");
        if let Some(m) = mode {
            j.push_str(&format!("    cache-mode: {m}\n    needs: write\n"));
        }
        j.push_str("    steps:\n");
        j.push_str(&prep);
        for op in ops {
            j.push_str(&use_(op));
        }
        j
    };
    let modes = [
        "on: workflow_dispatch\njobs:\n".to_owned(),
        job("write", None, &["op: save, key: k-write, paths: data", "op: restore, key: k-write, paths: data"]),
        job("read", Some("read"), &["op: save, key: k-read, paths: data", "op: restore, key: k-write, paths: data"]),
        job("write-only", Some("write-only"), &["op: restore, key: k-write, paths: data", "op: save, key: k-wo, paths: data"]),
        job("none", Some("none"), &["op: restore, key: k-write, paths: data", "op: save, key: k-none, paths: data"]),
    ]
    .concat();
    std::fs::write(work.join(".github/workflows/modes.yml"), modes)?;
    let target =
        format!("on: pull_request_target\njobs:\n  target:\n    runs-on: local\n    steps:\n{prep}{}{}", use_("op: restore, key: k-write, paths: data"), use_("op: save, key: k-target, paths: data"));
    std::fs::write(work.join(".github/workflows/target.yml"), target)?;
    std::fs::write(work.join(".github/workflows/caller.yml"), "on: workflow_dispatch\njobs:\n  call:\n    cache-mode: read\n    uses: ./.github/workflows/called.yml\n")?;
    std::fs::write(work.join(".github/workflows/called.yml"), "on: workflow_call\njobs:\n  greedy:\n    runs-on: local\n    cache-mode: write\n    steps:\n      - run: echo never\n")?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "modes"])?;
    git(&home, &work, &["push", "-q", &push, "HEAD:main"])?;
    // The modes, dispatched once the server has the commit that brings them.
    until(&s, 60, "the dispatch", || {
        let (st, body) = http(s.port, "POST", "/api/ci/repos/acme%2Fstorage/workflows/.github/workflows/modes.yml/dispatches", &s.admin, Some(&json!({ "ref": "main" })))?;
        Ok((!(200..300).contains(&st)).then(|| format!("{st} {body}")))
    })?;
    let run = finished_event(&s, "workflow_dispatch", "modes", 600)?;
    let id = run.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
    let all = legs(&s, &id)?;
    let log_of = |key: &str| all.iter().find(|(k, _, _)| k == key).map(|(_, c, l)| (c.clone(), l.clone())).unwrap_or_default();
    let (c, l) = log_of("write");
    assert!(c == "success" && l.contains("mode=write") && l.contains("restored k-write: k-write") && !l.contains("saved k-write: -1"), "unset on workflow_dispatch: write\n{l}\n{}", s.log());
    let (c, l) = log_of("read");
    assert!(c == "success" && l.contains("mode=read") && l.contains("Cache save skipped") && l.contains("restored k-write: k-write"), "read\n{l}");
    let (c, l) = log_of("write-only");
    assert!(c == "success" && l.contains("mode=write-only") && l.contains("Cache restore skipped") && l.contains("restored k-write: miss") && !l.contains("saved k-wo: -1"), "write-only\n{l}");
    let (c, l) = log_of("none");
    assert!(c == "success" && l.contains("mode=none") && l.contains("Cache restore skipped") && l.contains("Cache save skipped"), "none\n{l}");
    // pull_request_target: read, from the default branch's entries.
    git(&home, &work, &["checkout", "-q", "-b", "feature"])?;
    std::fs::write(work.join("change.txt"), "change\n")?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "a change"])?;
    git(&home, &work, &["push", "-q", &push, "HEAD:feature"])?;
    let (st, cr) = http(s.port, "POST", "/api/repos/acme%2Fstorage/change-requests", &s.admin, Some(&json!({ "branch": "feature" })))?;
    anyhow::ensure!(st == 200, "change request: {st} {cr}");
    let run = finished_event(&s, "pull_request_target", "target", 300)?;
    let id = run.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
    let (c, l) = legs(&s, &id)?.into_iter().next().map(|(_, c, l)| (c, l)).unwrap_or_default();
    assert!(c == "success" && l.contains("mode=read") && l.contains("restored k-write: k-write") && l.contains("Cache save skipped"), "pull_request_target\n{l}");
    // A called workflow that asks for more than its caller allows: the run doesn't start.
    let (st, body) = http(s.port, "POST", "/api/ci/repos/acme%2Fstorage/workflows/.github/workflows/caller.yml/dispatches", &s.admin, Some(&json!({ "ref": "main" })))?;
    if (200..300).contains(&st) {
        let run = finished_event(&s, "workflow_dispatch", "caller", 120)?;
        let id = run.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
        let (_, mut notes) = http(s.port, "GET", &format!("/api/ci/runs/{id}/annotations"), &s.admin, None)?;
        // What its legs say too: their errors and annotations.
        let (_, detail) = http(s.port, "GET", &format!("/api/ci/runs/{id}"), &s.admin, None)?;
        for l in serde_json::from_str::<Value>(&detail)?.get("legs").and_then(Value::as_array).cloned().unwrap_or_default() {
            let leg = l.get("id").and_then(Value::as_str).unwrap_or_default();
            let (_, about) = http(s.port, "GET", &format!("/api/ci/legs/{leg}"), &s.admin, None)?;
            notes.push_str(&about);
        }
        let ran = legs(&s, &id)?.iter().any(|(_, _, l)| l.contains("never"));
        assert!(run.get("conclusion") != Some(&json!("success")) && !ran, "it doesn't start: {run}\n{notes}");
        assert!(notes.contains("cache-mode") || serde_json::to_string(&run).unwrap_or_default().contains("cache-mode"), "and says why: {run}\n{notes}\n{detail}");
    } else {
        assert!(body.contains("cache-mode"), "it doesn't start, and says why: {st} {body}");
    }
    Ok(())
}
