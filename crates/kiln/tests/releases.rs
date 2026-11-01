//! Releases by tag and update drafts (docs/ci.md W29-R21, R22), against a server of its own with
//! real `git`: what a push may do to a release's tag in a repository kiln hosts.

#![cfg(unix)]

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use common::{Options, Server, http, start};
use serde_json::{Value, json};

/// `git` with no configuration but this test's, in `dir`: its output, or why it failed.
fn git(home: &Path, dir: &Path, args: &[&str]) -> Result<String, String> {
    let o = Command::new("git")
        .args(["-c", "user.name=Ada", "-c", "user.email=ada@example.com", "-c", "init.defaultBranch=main", "-c", "credential.helper=", "-c", "commit.gpgsign=false", "-c", "tag.gpgsign=false"])
        .args(args)
        .current_dir(dir)
        .env("HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GIT_ASKPASS")
        .env_remove("SSH_ASKPASS")
        .output()
        .map_err(|e| e.to_string())?;
    let out = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    if o.status.success() { Ok(out) } else { Err(out) }
}

/// [`git`] that must work.
fn must(home: &Path, dir: &Path, args: &[&str]) -> anyhow::Result<String> {
    git(home, dir, args).map_err(|e| anyhow::anyhow!("git {args:?}: {e}"))
}

/// A repository kiln hosts, cloned under `dir` (its `/` a `_`), with a first commit on main.
fn hosted(s: &Server, home: &Path, dir: &Path, name: &str) -> anyhow::Result<PathBuf> {
    hosted_with(s, home, dir, name, &[])
}

/// [`hosted`], `files` in its first commit too: a setting read from the trunk is there before
/// anything reads it.
fn hosted_with(s: &Server, home: &Path, dir: &Path, name: &str, files: &[(&str, &str)]) -> anyhow::Result<PathBuf> {
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": name, "mode": "native" })))?;
    anyhow::ensure!(st == 200, "{name}: {st} {body}");
    let leaf = name.replace('/', "_");
    must(home, dir, &["clone", "-q", &format!("http://x:{}@127.0.0.1:{}/git/{name}.git", s.admin, s.port), &leaf])?;
    let work = dir.join(&leaf);
    std::fs::write(work.join("README.md"), format!("{name}\n"))?;
    for (path, text) in files {
        let p = work.join(path);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(p, text)?;
    }
    must(home, &work, &["add", "."])?;
    must(home, &work, &["commit", "-qm", "first"])?;
    must(home, &work, &["push", "-q", "origin", "main"])?;
    Ok(work)
}

/// A refused push's words: it must fail, saying `why`.
fn refused(home: &Path, work: &Path, args: &[&str], why: &str) {
    let got = git(home, work, args);
    assert!(got.as_ref().is_err_and(|out| out.contains(why)), "git {args:?} should be refused, saying {why:?}: {got:?}");
}

/// W29-R21: in a repository kiln hosts, a full version pushed is a release. Its tag isn't moved
/// or deleted, and its name isn't used again, even once its tag is gone from the disk; a floating
/// version isn't pushed (Release moves it); another full version and other tags push as ever.
#[test]
fn w29_r21_released_tags_dont_move() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { agent: false, ci: "", extra: "" })?;
    let home = dir.path().join("githome");
    std::fs::create_dir_all(&home)?;
    let work = hosted(&s, &home, dir.path(), "acme/kit")?;
    must(&home, &work, &["tag", "v1.0.0"])?;
    let pushed = git(&home, &work, &["push", "-q", "origin", "v1.0.0"]);
    anyhow::ensure!(pushed.is_ok(), "{pushed:?}\n{}", s.log());

    std::fs::write(work.join("README.md"), "acme/kit, changed\n")?;
    must(&home, &work, &["commit", "-qam", "changed"])?;
    must(&home, &work, &["push", "-q", "origin", "main"])?;
    must(&home, &work, &["tag", "-f", "v1.0.0"])?;
    refused(&home, &work, &["push", "-q", "--force", "origin", "v1.0.0"], "`v1.0.0` is a release: it isn't moved");
    refused(&home, &work, &["push", "-q", "origin", ":refs/tags/v1.0.0"], "`v1.0.0` is a release: it isn't deleted");
    must(&home, &work, &["tag", "v1"])?;
    refused(&home, &work, &["push", "-q", "origin", "v1"], "`v1` is a floating version: only Release moves it");

    // Gone from the disk, its name is still a release's.
    let bare = dir.path().join("data").join("hosted").join("acme").join("kit.git");
    must(&home, &bare, &["tag", "-d", "v1.0.0"])?;
    refused(&home, &work, &["push", "-q", "origin", "v1.0.0"], "`v1.0.0` was released before: a release's name isn't used again");

    // Another release, and another kind of tag, push as ever.
    must(&home, &work, &["tag", "v1.0.1"])?;
    must(&home, &work, &["push", "-q", "origin", "v1.0.1"])?;
    must(&home, &work, &["tag", "nightly"])?;
    must(&home, &work, &["push", "-q", "origin", "nightly"])?;
    must(&home, &work, &["tag", "-f", "nightly", "HEAD~1"])?;
    must(&home, &work, &["push", "-q", "--force", "origin", "nightly"])?;
    Ok(())
}

fn at<'a>(v: &'a Value, pointer: &str) -> &'a str {
    v.pointer(pointer).and_then(Value::as_str).unwrap_or_default()
}

/// The conclusion of `repo`'s run at `sha` once it has finished.
fn concluded(s: &Server, repo: &str, sha: &str) -> anyhow::Result<String> {
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        let (_, body) = http(s.port, "GET", &format!("/api/ci/runs?repo={repo}&limit=50"), &s.admin, None)?;
        let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
        let run = v.get("runs").and_then(Value::as_array).into_iter().flatten().find(|r| at(r, "/sha") == sha && r.get("finished_at").is_some_and(|f| !f.is_null())).cloned();
        if let Some(r) = run {
            return Ok(at(&r, "/conclusion").to_owned());
        }
        anyhow::ensure!(Instant::now() < deadline, "no finished run of {repo} at {sha}: {v}\n{}", s.log());
        std::thread::sleep(Duration::from_millis(300));
    }
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

/// What `refs/tags/<tag>` names on the server, peeled to its commit.
fn tagged(home: &Path, work: &Path, tag: &str) -> anyhow::Result<String> {
    let out = must(home, work, &["ls-remote", "origin", &format!("refs/tags/{tag}"), &format!("refs/tags/{tag}^{{}}")])?;
    let lines: Vec<(&str, &str)> = out.lines().filter_map(|l| l.split_once('\t')).collect();
    let peeled = lines.iter().find(|(_, r)| r.ends_with("^{}")).or_else(|| lines.first()).map(|(sha, _)| (*sha).to_owned());
    peeled.ok_or_else(|| anyhow::anyhow!("no refs/tags/{tag}: {out}"))
}

/// W29-R21: Release by tag on a repository kiln hosts. At a commit whose checks passed it makes the
/// annotated tag and moves `v1` and `v1.1` there; the same name again is refused; at a commit with
/// a failed check it's refused, naming it, unless confirmed; an older patch moves only its minor.
/// Code mode releases as the route does.
#[test]
fn w29_r21_release_by_tag() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { agent: true, ci: "", extra: "" })?;
    let home = dir.path().join("githome");
    std::fs::create_dir_all(&home)?;
    let work = hosted(&s, &home, dir.path(), "acme/kit")?;
    std::fs::create_dir_all(work.join(".github/workflows"))?;
    std::fs::write(work.join(".github/workflows/check.yml"), "name: check\non: push\njobs:\n  c:\n    runs-on: local\n    steps:\n      - run: echo checked\n")?;
    must(&home, &work, &["add", "."])?;
    must(&home, &work, &["commit", "-qm", "a check"])?;
    must(&home, &work, &["push", "-q", "origin", "main"])?;
    let a = must(&home, &work, &["rev-parse", "HEAD"])?.trim().to_owned();
    assert_eq!(concluded(&s, "acme/kit", &a)?, "success");

    let release = |body: Value| -> anyhow::Result<(u16, Value)> {
        let (st, text) = http(s.port, "POST", "/api/ci/repos/acme%2Fkit/releases", &s.admin, Some(&body))?;
        Ok((st, serde_json::from_str(&text).unwrap_or(Value::Null)))
    };
    let (st, out) = release(json!({ "sha": a, "tag": "v1.1.0" }))?;
    assert_eq!(st, 200, "{out}");
    assert_eq!(out.get("moved"), Some(&json!(["v1", "v1.1"])), "{out}");
    for tag in ["v1.1.0", "v1", "v1.1"] {
        assert_eq!(tagged(&home, &work, tag)?, a, "{tag}");
    }
    let annotated = must(&home, &work, &["ls-remote", "origin", "refs/tags/v1.1.0"])?;
    assert!(!annotated.starts_with(&a), "an annotated tag: its own object: {annotated}");
    let (st, again) = release(json!({ "sha": a, "tag": "v1.1.0" }))?;
    assert_eq!(st, 409, "{again}");

    // A commit with a failed check asks again, naming it.
    std::fs::write(work.join(".github/workflows/check.yml"), "name: check\non: push\njobs:\n  c:\n    runs-on: local\n    steps:\n      - run: exit 1\n")?;
    must(&home, &work, &["add", "."])?;
    must(&home, &work, &["commit", "-qm", "breaks the check"])?;
    must(&home, &work, &["push", "-q", "origin", "main"])?;
    let b = must(&home, &work, &["rev-parse", "HEAD"])?.trim().to_owned();
    assert_eq!(concluded(&s, "acme/kit", &b)?, "failure");
    let (st, asked) = release(json!({ "sha": b, "tag": "v1.2.0" }))?;
    assert_eq!(st, 409, "{asked}");
    assert!(asked.to_string().contains(".github/workflows/check.yml"), "it names the failed check: {asked}");
    let (st, out) = release(json!({ "sha": b, "tag": "v1.2.0", "confirm": true }))?;
    assert_eq!((st, out.get("moved")), (200, Some(&json!(["v1", "v1.2"]))), "{out}");
    assert_eq!(tagged(&home, &work, "v1")?, b);

    // An older patch moves its minor, not its major; code mode releases as the route does.
    let (st, out) = release(json!({ "sha": a, "tag": "v1.1.1" }))?;
    assert_eq!((st, out.get("moved")), (200, Some(&json!(["v1.1"]))), "{out}");
    assert_eq!(tagged(&home, &work, "v1")?, b, "v1 stays at the newest release");
    let by_code = code(&s, &format!("return await kiln.ci.releases.tag({{ repo: 'acme/kit', sha: '{a}', tag: 'v1.1.2' }})"))?;
    assert_eq!((at(&by_code, "/tag"), at(&by_code, "/sha"), by_code.get("moved")), ("v1.1.2", a.as_str(), Some(&json!(["v1.1"]))), "{by_code}");
    Ok(())
}

/// Commits `files` in `work` and pushes main: the commit.
fn push(home: &Path, work: &Path, files: &[(&str, &str)]) -> anyhow::Result<String> {
    for (path, text) in files {
        let p = work.join(path);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(p, text)?;
    }
    must(home, work, &["add", "."])?;
    must(home, work, &["commit", "-qm", "files"])?;
    must(home, work, &["push", "-q", "origin", "main"])?;
    Ok(must(home, work, &["rev-parse", "HEAD"])?.trim().to_owned())
}

/// The drafts in `repo`'s rows that kiln's updates opened, once `n` of them are there (and still
/// after 2 s): each its id and its file's text at its head.
fn update_drafts(s: &Server, repo: &str, path: &str, n: usize) -> anyhow::Result<Vec<(String, String)>> {
    let read = || -> anyhow::Result<Vec<(String, String)>> {
        let (_, body) = http(s.port, "GET", &format!("/api/ship/pipelines?repo={repo}"), &s.admin, None)?;
        let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
        let rows: Vec<Value> = v.pointer("/repos/0/rows").and_then(Value::as_array).cloned().unwrap_or_default();
        let mut out = Vec::new();
        for r in rows.iter().filter(|r| at(r, "/path") == path && at(r, "/draft/author").starts_with("kiln-updates:")) {
            let id = at(r, "/draft/id").to_owned();
            let (_, pane) = http(s.port, "GET", &format!("/api/ship/pipelines/{repo}?path={path}&draft={id}"), &s.admin, None)?;
            let pane: Value = serde_json::from_str(&pane).unwrap_or(Value::Null);
            out.push((id, at(&pane, "/detail/text").to_owned()));
        }
        Ok(out)
    };
    let deadline = Instant::now() + Duration::from_secs(120);
    while read()?.len() < n {
        anyhow::ensure!(Instant::now() < deadline, "{repo} has fewer than {n} update drafts of {path}: {:?}\n{}", read()?, s.log());
        std::thread::sleep(Duration::from_millis(500));
    }
    std::thread::sleep(Duration::from_secs(2));
    read()
}

/// W29-R22: update drafts. After acme/kit releases v1.1.0, acme/app (updates on, cooldown 0),
/// whose runs used v1.0.0, gets an update draft moving its `uses:` to v1.1.0; acme/other, which
/// didn't ask, gets none. In acme/web, a weekly group gathers acme/kit's and acme/tools' releases
/// in one draft.
#[test]
fn w29_r22_update_drafts() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { agent: true, ci: "", extra: "" })?;
    let home = dir.path().join("githome");
    std::fs::create_dir_all(&home)?;
    let action = "name: x\nruns:\n  using: composite\n  steps:\n    - run: echo x\n      shell: bash\n";
    let release = |repo: &str, sha: &str, tag: &str| -> anyhow::Result<()> {
        let (st, out) = http(s.port, "POST", &format!("/api/ci/repos/{}/releases", repo.replace('/', "%2F")), &s.admin, Some(&json!({ "sha": sha, "tag": tag })))?;
        anyhow::ensure!(st == 200, "{repo} {tag}: {st} {out}");
        Ok(())
    };
    let mut heads = Vec::new();
    for publisher in ["acme/kit", "acme/tools"] {
        let work = hosted_with(&s, &home, dir.path(), publisher, &[(".kiln/config.yaml", "share: owner\n"), (".github/actions/x/action.yml", action)])?;
        let head = must(&home, &work, &["rev-parse", "HEAD"])?.trim().to_owned();
        release(publisher, &head, "v1.0.0")?;
        heads.push((publisher, head));
    }
    let uses = |names: &[&str]| -> String {
        let steps: String = names.iter().map(|n| format!("      - uses: kiln://{n}/.github/actions/x@v1.0.0\n")).collect();
        format!("name: ci\non: push\njobs:\n  a:\n    runs-on: local\n    steps:\n{steps}")
    };
    let consumers = [
        ("acme/app", Some("updates:\n  actions: on\n  cooldown: 0\n"), uses(&["acme/kit"])),
        ("acme/other", None, uses(&["acme/kit"])),
        ("acme/web", Some("updates:\n  actions: on\n  cooldown: 0\n  group: weekly\n"), uses(&["acme/kit", "acme/tools"])),
    ];
    for (name, config, workflow) in &consumers {
        let mut files: Vec<(&str, &str)> = vec![(".github/workflows/ci.yml", workflow.as_str())];
        if let Some(c) = config {
            files.push((".kiln/config.yaml", c));
        }
        let work = hosted(&s, &home, dir.path(), name)?;
        let sha = push(&home, &work, &files)?;
        assert_eq!(concluded(&s, name, &sha)?, "success", "{name}'s run used v1.0.0");
    }

    // acme/kit's v1.1.0: one draft each where it was asked for.
    let kit = heads.first().map(|(_, h)| h.clone()).unwrap_or_default();
    release("acme/kit", &kit, "v1.1.0")?;
    let app = update_drafts(&s, "acme/app", ".github/workflows/ci.yml", 1)?;
    assert_eq!(app.len(), 1, "{app:?}");
    assert!(app.iter().all(|(_, text)| text.contains("kiln://acme/kit/.github/actions/x@v1.1.0\n")), "{app:?}");
    let web = update_drafts(&s, "acme/web", ".github/workflows/ci.yml", 1)?;
    assert_eq!(web.len(), 1, "{web:?}");
    assert_eq!(update_drafts(&s, "acme/other", ".github/workflows/ci.yml", 0)?.len(), 0, "acme/other didn't ask");

    // acme/tools' v1.1.0 joins acme/web's week: one draft, both moved.
    let tools = heads.get(1).map(|(_, h)| h.clone()).unwrap_or_default();
    release("acme/tools", &tools, "v1.1.0")?;
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let web = update_drafts(&s, "acme/web", ".github/workflows/ci.yml", 1)?;
        if web.iter().any(|(_, t)| t.contains("acme/tools/.github/actions/x@v1.1.0")) {
            assert_eq!(web.len(), 1, "one draft for the week: {web:?}");
            assert!(web.iter().all(|(_, t)| t.contains("acme/kit/.github/actions/x@v1.1.0") && t.contains("acme/tools/.github/actions/x@v1.1.0")), "{web:?}");
            break;
        }
        anyhow::ensure!(Instant::now() < deadline, "acme/tools' release didn't join acme/web's draft: {web:?}\n{}", s.log());
        std::thread::sleep(Duration::from_millis(500));
    }
    assert_eq!(update_drafts(&s, "acme/app", ".github/workflows/ci.yml", 1)?.len(), 1, "acme/app doesn't use acme/tools");
    Ok(())
}
