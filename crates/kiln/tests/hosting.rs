//! Native code hosting (docs/ci.md W14-R01, R02), against a server of its own with real `git`:
//! a native repository is made, cloned and pushed over HTTPS with people's tokens; the push
//! policy holds (trunks only for those allowed, branches for developers, tags for those who land,
//! nothing for viewers or for tokens that may do nothing); and a push starts the repository's
//! workflows.

#![cfg(unix)]

mod common;

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use common::{Options, Server, http, start};
use serde_json::{Value, json};

/// `git` with no configuration but this test's, in `dir`.
fn git(home: &Path, dir: &Path, args: &[&str]) -> anyhow::Result<(bool, String)> {
    let o = Command::new("git")
        .args(["-c", "user.name=Test", "-c", "user.email=test@example.com", "-c", "init.defaultBranch=main", "-c", "credential.helper="])
        .args(args)
        .current_dir(dir)
        .env("HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GIT_ASKPASS")
        .env_remove("SSH_ASKPASS")
        .output()?;
    Ok((o.status.success(), format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))))
}

fn must(r: anyhow::Result<(bool, String)>, what: &str) -> anyhow::Result<String> {
    let (ok, out) = r?;
    anyhow::ensure!(ok, "{what}: {out}");
    Ok(out)
}

/// A user with `grants`, and a personal access token that may do what they may.
fn person(s: &Server, name: &str, grants: &[&str]) -> anyhow::Result<String> {
    let (st, body) = http(s.port, "POST", "/api/users", &s.admin, Some(&json!({ "name": name, "grants": grants })))?;
    anyhow::ensure!((200..300).contains(&st), "{name}: {st} {body}");
    let (st, body) = http(s.port, "POST", "/api/tokens", &s.admin, Some(&json!({ "kind": "pat", "user": name, "grants": ["owner"] })))?;
    anyhow::ensure!((200..300).contains(&st), "{name}'s token: {st} {body}");
    serde_json::from_str::<Value>(&body)?.get("token").and_then(Value::as_str).map(str::to_owned).ok_or_else(|| anyhow::anyhow!("no token in {body}"))
}

#[test]
fn w14_r02_native_repository() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options::default())?;
    let home = dir.path().join("githome");
    std::fs::create_dir_all(&home)?;
    let url = |token: &str| format!("http://x:{token}@127.0.0.1:{}/git/acme/app.git", s.port);

    // A native repository, empty on `main`.
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/app", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "native repository: {st} {body}");
    let repo: Value = serde_json::from_str(&body)?;
    let clone_url = repo.get("clone_url").and_then(Value::as_str).unwrap_or_default();
    assert!(clone_url.starts_with("file://") && clone_url.ends_with("/hosted/acme/app.git"), "{clone_url}");
    assert_eq!((repo.get("mode").and_then(Value::as_str), repo.get("default_branch").and_then(Value::as_str)), (Some("native"), Some("main")));
    let (st, _) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/other", "import": "/etc" })))?;
    assert_eq!(st, 400, "an import copies from the network, not the server's disks");

    let dev = person(&s, "dev", &["developer@repo:acme/app"])?;
    let viewer = person(&s, "viewer", &["viewer@repo:acme/app"])?;
    let nobody = person(&s, "nobody", &[])?;

    // Without a token, or with one that may do nothing: no clone.
    let (ok, out) = git(&home, dir.path(), &["clone", &format!("http://127.0.0.1:{}/git/acme/app.git", s.port), "anon"])?;
    assert!(!ok, "{out}");
    let (ok, out) = git(&home, dir.path(), &["clone", &url(&nobody), "nobody"])?;
    assert!(!ok && out.contains("403"), "{out}");

    // The developer clones the empty repository and pushes its first trunk.
    must(git(&home, dir.path(), &["clone", &url(&dev), "dev"]), "the developer's clone")?;
    let work = dir.path().join("dev");
    std::fs::create_dir_all(work.join(".github/workflows"))?;
    std::fs::write(work.join(".github/workflows/ci.yml"), "on: push\njobs:\n  build:\n    runs-on: local\n    steps:\n      - run: echo built\n")?;
    std::fs::write(work.join("README.md"), "app\n")?;
    must(git(&home, &work, &["add", "."]), "add")?;
    must(git(&home, &work, &["commit", "-qm", "first"]), "commit")?;
    must(git(&home, &work, &["push", "-q", "origin", "main"]), "the first trunk")?;
    let first = must(git(&home, &work, &["rev-parse", "HEAD"]), "rev-parse")?.trim().to_owned();

    // After that, trunk is the land queue's: refused, and said so.
    std::fs::write(work.join("README.md"), "app, again\n")?;
    must(git(&home, &work, &["commit", "-qam", "second"]), "commit")?;
    let (ok, out) = git(&home, &work, &["push", "origin", "main"])?;
    assert!(!ok && out.contains("main is a trunk") && out.contains("remote: kiln:"), "{out}");
    // A branch is the developer's to push; a tag isn't.
    must(git(&home, &work, &["push", "-q", "origin", "HEAD:refs/heads/topic"]), "a branch")?;
    let second = must(git(&home, &work, &["rev-parse", "HEAD"]), "rev-parse")?.trim().to_owned();
    must(git(&home, &work, &["tag", "v1"]), "tag")?;
    let (ok, out) = git(&home, &work, &["push", "origin", "v1"])?;
    assert!(!ok && out.contains("diffs.land"), "{out}");
    // Nor are kiln's own refs, or diffs before W14-R03.
    let (ok, out) = git(&home, &work, &["push", "origin", "HEAD:refs/kiln/diffs/D1/v1"])?;
    assert!(!ok && out.contains("kiln's own refs"), "{out}");
    // A push is refused whole: an allowed branch waits with the trunk that isn't.
    let (ok, out) = git(&home, &work, &["push", "origin", "HEAD:refs/heads/other", "HEAD:refs/heads/main"])?;
    assert!(!ok && out.contains("another of its updates"), "{out}");

    // A viewer reads, and pushes nothing.
    must(git(&home, dir.path(), &["clone", "-q", &url(&viewer), "viewer"]), "the viewer's clone")?;
    let theirs = dir.path().join("viewer");
    let (ok, out) = git(&home, &theirs, &["push", "origin", "HEAD:refs/heads/mine"])?;
    assert!(!ok && out.contains("diffs.submit"), "{out}");

    // The owner may write trunk directly.
    must(git(&home, &work, &["push", "-q", &url(&s.admin), "HEAD:main", "v1"]), "the owner's push")?;
    let refs = must(git(&home, dir.path(), &["ls-remote", &url(&viewer)]), "ls-remote")?;
    for (r, sha) in [("refs/heads/main", &second), ("refs/heads/topic", &second), ("refs/tags/v1", &second)] {
        assert!(refs.lines().any(|l| l == format!("{sha}\t{r}")), "{r} at {sha}: {refs}");
    }
    assert!(!refs.contains("refs/heads/other") && !refs.contains("refs/kiln/"), "{refs}");

    // The push started the repository's workflow, on the commit pushed.
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let (_, runs) = http(s.port, "GET", "/api/runs?limit=50", &s.admin, None)?;
        if runs.contains(&first) || runs.contains(&second) {
            break;
        }
        anyhow::ensure!(Instant::now() < deadline, "no run for the pushes: {runs}");
        std::thread::sleep(Duration::from_millis(500));
    }
    // The audit names who pushed what.
    let (_, audit) = http(s.port, "GET", "/api/audit?limit=200", &s.admin, None)?;
    assert!(audit.contains("repo.push") && audit.contains("user:dev"), "{audit}");
    Ok(())
}

/// A commit of `file` holding `text`, with Change-Id `id` (as kiln's hook adds it): its SHA.
fn commit(home: &Path, work: &Path, file: &str, text: &str, title: &str, id: &str) -> anyhow::Result<String> {
    std::fs::write(work.join(file), text)?;
    must(git(home, work, &["add", file]), "add")?;
    must(git(home, work, &["commit", "-qm", &format!("{title}\n\nChange-Id: {id}")]), "commit")?;
    Ok(must(git(home, work, &["rev-parse", "HEAD"]), "rev-parse")?.trim().to_owned())
}

fn api(s: &Server, token: &str, method: &str, path: &str) -> anyhow::Result<Value> {
    let empty = json!({});
    let (st, body) = http(s.port, method, path, token, (method != "GET").then_some(&empty))?;
    anyhow::ensure!(st == 200, "{method} {path}: {st} {body}");
    Ok(serde_json::from_str(&body)?)
}

/// The versions of a diff: (n, rebase_only).
fn versions(d: &Value) -> Vec<(u64, bool)> {
    d.get("versions").and_then(Value::as_array).into_iter().flatten().map(|v| (v.get("n").and_then(Value::as_u64).unwrap_or(0), v.get("rebase_only").and_then(Value::as_bool).unwrap_or(false))).collect()
}

fn text<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or_default()
}

/// W14-AC01: submitting three commits makes D1–D3, a stack; amending D2 adds D2 v2 only; the
/// stack rebased onto a moved trunk gets rebase-only versions, marked; reordering is seen; an
/// interdiff of D2 v1..v2 shows the amend; every version is kept at its ref; and the push's
/// refusals (no Change-Id, not a trunk, an abandoned diff, a viewer) say why. With real git.
#[test]
fn w14_ac01_stacks() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options::default())?;
    let home = dir.path().join("githome");
    std::fs::create_dir_all(&home)?;
    let url = |token: &str| format!("http://x:{token}@127.0.0.1:{}/git/acme/app.git", s.port);
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/app", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "native repository: {st} {body}");
    let repo_id = serde_json::from_str::<Value>(&body)?.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
    let dev = person(&s, "dev", &["developer@repo:acme/app"])?;
    let viewer = person(&s, "viewer", &["viewer@repo:acme/app"])?;

    must(git(&home, dir.path(), &["clone", "-q", &url(&dev), "work"]), "clone")?;
    let work = dir.path().join("work");
    std::fs::create_dir_all(work.join(".github/workflows"))?;
    std::fs::write(work.join(".github/workflows/ci.yml"), "on: pull_request\njobs:\n  test:\n    runs-on: local\n    steps:\n      - run: echo tested\n")?;
    std::fs::write(work.join("README.md"), "app\n")?;
    must(git(&home, &work, &["add", "."]), "add")?;
    must(git(&home, &work, &["commit", "-qm", "first"]), "commit")?;
    must(git(&home, &work, &["push", "-q", "origin", "main"]), "the first trunk")?;
    must(git(&home, &work, &["checkout", "-qb", "stack"]), "branch")?;

    // Three commits: D1–D3, each on the one below.
    let (i1, i2, i3) = ("I1111111111111111111111111111111111111111", "I2222222222222222222222222222222222222222", "I3333333333333333333333333333333333333333");
    let c1 = commit(&home, &work, "a.txt", "one\n", "one: a", i1)?;
    let c2 = commit(&home, &work, "b.txt", "two\n", "two: b", i2)?;
    let c3 = commit(&home, &work, "c.txt", "three\n", "three: c", i3)?;
    let out = must(git(&home, &work, &["push", "origin", "HEAD:refs/for/main"]), "submit")?;
    for want in ["D1 v1 (new)", "D2 v1 (new)", "D3 v1 (new)", "/diffs/D1", "two: b"] {
        assert!(out.contains(want), "{want} in {out}");
    }
    let listed = api(&s, &dev, "GET", &format!("/api/repos/{repo_id}/diffs"))?;
    assert_eq!(listed.get("diffs").and_then(Value::as_array).map(Vec::len), Some(3), "{listed}");
    let d2 = api(&s, &dev, "GET", "/api/diffs/D2")?;
    assert_eq!((text(&d2, "parent"), text(&d2, "state"), text(&d2, "author"), text(&d2, "author_kind")), ("D1", "needs-review", "user:dev", "human"), "{d2}");
    assert_eq!(versions(&d2), [(1, false)]);
    let stack = api(&s, &dev, "GET", &format!("/api/stacks/{}", text(&d2, "stack")))?;
    let order: Vec<&str> = stack.get("diffs").and_then(Value::as_array).into_iter().flatten().map(|d| text(d, "name")).collect();
    assert_eq!(order, ["D1", "D2", "D3"]);
    let refs = must(git(&home, &work, &["ls-remote", &url(&dev)]), "ls-remote")?;
    for (r, sha) in [("refs/kiln/diffs/D1/v1", &c1), ("refs/kiln/diffs/D2/v1", &c2), ("refs/kiln/diffs/D3/v1", &c3)] {
        assert!(refs.lines().any(|l| l == format!("{sha}\t{r}")), "{r} at {sha}: {refs}");
    }
    assert!(!refs.contains("refs/for/"), "the push's way in isn't kept: {refs}");

    // Amending D2 adds D2 v2 only; D3, restacked on it, keeps its version.
    must(git(&home, &work, &["checkout", "-q", &c2]), "checkout")?;
    std::fs::write(work.join("b.txt"), "two, amended\n")?;
    must(git(&home, &work, &["commit", "-q", "--amend", "--no-edit", "-a"]), "amend")?;
    must(git(&home, &work, &["cherry-pick", &c3]), "restack")?;
    let out = must(git(&home, &work, &["push", "origin", "HEAD:refs/for/main"]), "the amend")?;
    for want in ["D1 v1 (unchanged)", "D2 v2 (updated)", "D3 v1 (unchanged)"] {
        assert!(out.contains(want), "{want} in {out}");
    }
    let inter = api(&s, &dev, "GET", "/api/diffs/D2/interdiff?from=1&to=2")?;
    assert_eq!(inter.get("same_patch"), Some(&json!(false)));
    assert!(text(&inter, "range_diff").contains("amended"), "{inter}");
    let files = api(&s, &dev, "GET", "/api/diffs/D2/versions/2/files")?;
    assert!(files.get("files").and_then(Value::as_array).is_some_and(|f| f.iter().any(|x| text(x, "path") == "b.txt")) && text(&files, "patch").contains("+two, amended"), "{files}");
    must(git(&home, &work, &["branch", "-qf", "stack", "HEAD"]), "keep it")?;
    must(git(&home, &work, &["checkout", "-q", "stack"]), "back")?;

    // The trunk moves; the stack, rebased onto it, gets rebase-only versions.
    must(git(&home, &work, &["checkout", "-q", "main"]), "main")?;
    std::fs::write(work.join("z.txt"), "elsewhere\n")?;
    must(git(&home, &work, &["add", "z.txt"]), "add")?;
    must(git(&home, &work, &["commit", "-qm", "trunk moves"]), "commit")?;
    must(git(&home, &work, &["push", "-q", &url(&s.admin), "HEAD:main"]), "the owner moves trunk")?;
    must(git(&home, &work, &["checkout", "-q", "stack"]), "stack")?;
    must(git(&home, &work, &["rebase", "-q", "main"]), "rebase")?;
    let out = must(git(&home, &work, &["push", "origin", "HEAD:refs/for/main"]), "the rebase")?;
    for want in ["D1 v2 (rebased", "D2 v3 (rebased", "D3 v2 (rebased"] {
        assert!(out.contains(want), "{want} in {out}");
    }
    assert_eq!(versions(&api(&s, &dev, "GET", "/api/diffs/D2")?), [(1, false), (2, false), (3, true)]);

    // Reordering is seen: D3 below D2.
    let top = must(git(&home, &work, &["rev-parse", "HEAD"]), "top")?.trim().to_owned();
    let mid = must(git(&home, &work, &["rev-parse", "HEAD~1"]), "mid")?.trim().to_owned();
    must(git(&home, &work, &["reset", "-q", "--hard", "HEAD~2"]), "to D1")?;
    must(git(&home, &work, &["cherry-pick", &top]), "D3 first")?;
    must(git(&home, &work, &["cherry-pick", &mid]), "then D2")?;
    let out = must(git(&home, &work, &["push", "origin", "HEAD:refs/for/main"]), "the reorder")?;
    assert!(out.contains("D3 v3 (moved in the stack)") && out.contains("D2 v4 (moved in the stack)") && out.contains("D1 v2 (unchanged)"), "{out}");
    assert_eq!((text(&api(&s, &dev, "GET", "/api/diffs/D3")?, "parent"), text(&api(&s, &dev, "GET", "/api/diffs/D2")?, "parent")), ("D1", "D3"));

    // Refused, saying why, nothing kept: no Change-Id; not a trunk; a viewer.
    std::fs::write(work.join("d.txt"), "four\n")?;
    must(git(&home, &work, &["add", "d.txt"]), "add")?;
    must(git(&home, &work, &["commit", "-qm", "no id"]), "commit")?;
    let (ok, out) = git(&home, &work, &["push", "origin", "HEAD:refs/for/main"])?;
    assert!(!ok && out.contains("no Change-Id") && out.contains("remote: kiln:"), "{out}");
    let (ok, out) = git(&home, &work, &["push", "origin", "HEAD:refs/for/nope"])?;
    assert!(!ok && out.contains("isn't a trunk"), "{out}");
    must(git(&home, &work, &["reset", "-q", "--hard", "HEAD~1"]), "drop it")?;
    must(git(&home, dir.path(), &["clone", "-q", &url(&viewer), "theirs"]), "the viewer's clone")?;
    let (ok, out) = git(&home, &dir.path().join("theirs"), &["push", &url(&viewer), "HEAD:refs/for/main"])?;
    assert!(!ok && out.contains("diffs.submit"), "{out}");
    let refs = must(git(&home, &work, &["ls-remote", &url(&dev)]), "ls-remote")?;
    assert!(!refs.contains("refs/for/"), "{refs}");

    // Push options: a draft, a topic, a reviewer.
    commit(&home, &work, "e.txt", "five\n", "five: e", "I4444444444444444444444444444444444444444")?;
    let out = must(git(&home, &work, &["push", "-o", "draft", "-o", "topic=letters", "-o", "reviewer=user:viewer", "-o", "bogus", "origin", "HEAD:refs/for/main"]), "a draft")?;
    assert!(out.contains("D4 v1 (new)") && out.contains("`bogus`"), "{out}");
    let d4 = api(&s, &dev, "GET", "/api/diffs/D4")?;
    assert_eq!((text(&d4, "state"), text(&d4, "topic"), text(&d4, "parent")), ("draft", "letters", "D2"), "{d4}");
    assert!(d4.get("reviewers").and_then(Value::as_array).is_some_and(|r| r.iter().any(|x| text(x, "reviewer") == "user:viewer")), "{d4}");
    // Its author's actions.
    assert_eq!(text(&api(&s, &dev, "POST", "/api/diffs/D4/ready")?, "state"), "needs-review");
    assert_eq!(text(&api(&s, &dev, "POST", "/api/diffs/D4/abandon")?, "state"), "abandoned");
    std::fs::write(work.join("e.txt"), "five, again\n")?;
    must(git(&home, &work, &["commit", "-q", "--amend", "--no-edit", "-a"]), "amend")?;
    let (ok, out) = git(&home, &work, &["push", "origin", "HEAD:refs/for/main"])?;
    assert!(!ok && out.contains("abandoned"), "{out}");
    assert_eq!(text(&api(&s, &dev, "POST", "/api/diffs/D4/reclaim")?, "state"), "needs-review");
    let (st, _) = http(s.port, "POST", "/api/diffs/D4/abandon", &viewer, Some(&json!({})))?;
    assert_eq!(st, 403, "a viewer doesn't act on diffs");
    let out = must(git(&home, &work, &["push", "origin", "HEAD:refs/for/main"]), "D4 v2")?;
    assert!(out.contains("D4 v2 (updated)"), "{out}");

    // Each new version's `pull_request` workflow starts, on the version's commit.
    let d3 = api(&s, &dev, "GET", "/api/diffs/D3")?;
    let latest = d3.get("versions").and_then(Value::as_array).and_then(|v| v.last()).map(|v| text(v, "commit").to_owned()).unwrap_or_default();
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let d = api(&s, &dev, "GET", "/api/diffs/D3")?;
        if d.get("runs").and_then(Value::as_array).is_some_and(|r| r.iter().any(|x| text(x, "event") == "pull_request")) {
            break;
        }
        anyhow::ensure!(Instant::now() < deadline, "no run for D3's latest version {latest}: {d}\n{}", s.log());
        std::thread::sleep(Duration::from_millis(500));
    }
    Ok(())
}

/// `kiln` with `args` in `dir`, as a developer whose token is `token` would run it.
fn kiln(home: &Path, dir: &Path, s: &Server, token: &str, args: &[&str]) -> anyhow::Result<(bool, String)> {
    let o = Command::new(env!("CARGO_BIN_EXE_kiln"))
        .args(args)
        .current_dir(dir)
        .env("HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", "Dev")
        .env("GIT_AUTHOR_EMAIL", "dev@example.com")
        .env("GIT_COMMITTER_NAME", "Dev")
        .env("GIT_COMMITTER_EMAIL", "dev@example.com")
        .env("KILN_URL", format!("http://127.0.0.1:{}", s.port))
        .env("KILN_TOKEN", token)
        .env("KILN_ADMIN_TOKEN", token)
        .env_remove("KILN_CONFIG")
        .output()?;
    Ok((o.status.success(), format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))))
}

/// The client side of W14-R03: `kiln clone` (kiln's credentials, the commit-msg hook), commits
/// that get Change-Ids by themselves, `kiln submit` adding the ones a commit lacks, `kiln diffs`,
/// `kiln checkout D2@v1`, and `kiln sync` dropping a diff the trunk already has.
#[test]
fn w14_r03_cli() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options::default())?;
    let home = dir.path().join("githome");
    std::fs::create_dir_all(&home)?;
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/app", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "native repository: {st} {body}");
    let dev = person(&s, "dev", &["developer@repo:acme/app"])?;

    let out = must(kiln(&home, dir.path(), &s, &dev, &["clone", "acme/app", "work"]), "kiln clone")?;
    assert!(out.contains("Change-Id"), "{out}");
    let work = dir.path().join("work");
    let g = |args: &[&str]| must(kiln_git(&home, &work, &s, &dev, args), &args.join(" "));
    std::fs::write(work.join("README.md"), "app\n")?;
    g(&["add", "README.md"])?;
    g(&["commit", "-qm", "first"])?;
    g(&["push", "-q", "origin", "HEAD:main"])?;
    g(&["fetch", "-q", "origin"])?;
    g(&["remote", "set-head", "origin", "main"])?;
    // The hook gives commits their Change-Ids; one made without it gets one from `kiln submit`.
    std::fs::write(work.join("a.txt"), "one\n")?;
    g(&["add", "a.txt"])?;
    g(&["commit", "-qm", "one: a"])?;
    assert!(g(&["log", "-1", "--format=%B"])?.contains("Change-Id: I"), "the hook");
    std::fs::write(work.join("b.txt"), "two\n")?;
    g(&["add", "b.txt"])?;
    g(&["commit", "-q", "--no-verify", "-m", "two: b"])?;
    assert!(!g(&["log", "-1", "--format=%B"])?.contains("Change-Id"));
    let out = must(kiln(&home, &work, &s, &dev, &["submit", "--draft", "-r", "user:dev"]), "kiln submit")?;
    assert!(out.contains("gave 1 commit a Change-Id") && out.contains("D1 v1 (new)") && out.contains("D2 v1 (new)"), "{out}");
    assert!(g(&["log", "-1", "--format=%B"])?.contains("Change-Id: I"), "HEAD moved to the rewritten commit");
    assert_eq!(g(&["status", "--porcelain"])?, "", "the working tree is as it was");

    let out = must(kiln(&home, &work, &s, &dev, &["review", "D2"]), "kiln review")?;
    assert!(out.contains("D2 two: b (draft, v1") && out.contains("not ready to land") && out.contains("it's a draft"), "{out}");
    let (ok, out) = kiln(&home, &work, &s, &dev, &["land", "D2"])?;
    assert!(!ok && out.contains("diffs.land"), "a developer doesn't land: {out}");
    let out = must(kiln(&home, &work, &s, &dev, &["diffs"]), "kiln diffs")?;
    assert!(out.contains("D1") && out.contains("D2") && out.contains("draft") && out.contains("(on D1)"), "{out}");
    let top = g(&["rev-parse", "HEAD"])?.trim().to_owned();
    let out = must(kiln(&home, &work, &s, &dev, &["checkout", "D1@v1"]), "kiln checkout")?;
    assert!(out.contains("D1 v1"), "{out}");
    assert_eq!(g(&["rev-parse", "HEAD"])?.trim(), g(&["rev-parse", &format!("{top}~1")])?.trim());
    g(&["checkout", "-q", &top])?;
    g(&["checkout", "-qb", "stack"])?;

    // D1 reaches the trunk (as landing will put it there); `kiln sync` drops it from the stack.
    let d1 = g(&["rev-parse", "HEAD~1"])?.trim().to_owned();
    must(git(&home, &work, &["push", "-q", &format!("http://x:{}@127.0.0.1:{}/git/acme/app.git", s.admin, s.port), &format!("{d1}:refs/heads/main")]), "the owner lands D1")?;
    let out = must(kiln(&home, &work, &s, &dev, &["sync"]), "kiln sync")?;
    let left = g(&["log", "--format=%s", "origin/main..HEAD"])?;
    assert_eq!(left.trim(), "two: b", "{out}\n{left}");
    Ok(())
}

/// git as the developer does it in a `kiln clone`: credentials from kiln's helper.
fn kiln_git(home: &Path, dir: &Path, s: &Server, token: &str, args: &[&str]) -> anyhow::Result<(bool, String)> {
    let o = Command::new("git")
        .args(["-c", "user.name=Dev", "-c", "user.email=dev@example.com"])
        .args(args)
        .current_dir(dir)
        .env("HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("KILN_URL", format!("http://127.0.0.1:{}", s.port))
        .env("KILN_TOKEN", token)
        .env_remove("KILN_CONFIG")
        .output()?;
    Ok((o.status.success(), format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))))
}

fn post(s: &Server, token: &str, path: &str, body: &Value) -> anyhow::Result<(u16, Value)> {
    let (st, text) = http(s.port, "POST", path, token, Some(body))?;
    Ok((st, serde_json::from_str(&text).unwrap_or(Value::String(text))))
}

/// Polls `f` until it says yes, for up to `secs` seconds.
fn until(secs: u64, what: &str, mut f: impl FnMut() -> anyhow::Result<Option<String>>) -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        match f()? {
            None => return Ok(()),
            Some(last) => anyhow::ensure!(Instant::now() < deadline, "{what}: {last}"),
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// W14-AC02 and AC04, with W14-R05: a review (draft comments published together, a thread
/// answered and resolved, accepts from someone other than the author) lets a stack land; each
/// strategy gives the trunk the history it should (`stack`: one commit each with its trailers,
/// `stack-squash`: one, `ff-only`: the commits as they are, refused when diverged); a diff
/// standing on a landed one is restacked as a rebase-only version; and a diff that loosens the
/// policy is judged by the trunk's.
#[test]
fn w14_ac02_strategies() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options::default())?;
    let home = dir.path().join("githome");
    std::fs::create_dir_all(&home)?;
    let url = |token: &str| format!("http://x:{token}@127.0.0.1:{}/git/acme/app.git", s.port);
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/app", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "native repository: {st} {body}");
    let dev = person(&s, "dev", &["developer@repo:acme/app"])?;
    let rev = person(&s, "rev", &["developer@repo:acme/app"])?;
    let lead = person(&s, "lead", &["maintainer@repo:acme/app"])?;

    must(git(&home, dir.path(), &["clone", "-q", &url(&dev), "work"]), "clone")?;
    let work = dir.path().join("work");
    std::fs::create_dir_all(work.join(".kiln"))?;
    std::fs::write(work.join(".kiln/review.yaml"), "land:\n  approvals: 1\n")?;
    std::fs::write(work.join("README.md"), "app\n")?;
    must(git(&home, &work, &["add", "."]), "add")?;
    must(git(&home, &work, &["commit", "-qm", "first"]), "commit")?;
    must(git(&home, &work, &["push", "-q", "origin", "main"]), "the first trunk")?;
    let trunk0 = must(git(&home, &work, &["rev-parse", "HEAD"]), "rev-parse")?.trim().to_owned();

    // D1 and D2, a stack.
    commit(&home, &work, "a.txt", "one\n", "one: a", "I1111111111111111111111111111111111111111")?;
    commit(&home, &work, "b.txt", "two\nmore\n", "two: b", "I2222222222222222222222222222222222222222")?;
    must(git(&home, &work, &["push", "-q", "origin", "HEAD:refs/for/main"]), "submit")?;
    let (st, why) = post(&s, &lead, "/api/diffs/D2/land", &json!({}))?;
    assert!(st == 409 && why.to_string().contains("0 of 1 accepts"), "{st} {why}");

    // A review: a draft only its author sees, published with the review; a thread that blocks
    // until it's answered and resolved.
    let (st, c) = post(&s, &rev, "/api/diffs/D2/comments", &json!({ "file": "b.txt", "start_line": 2, "body": "why more?" }))?;
    anyhow::ensure!(st == 200, "comment: {c}");
    let threads = |token: &str| -> anyhow::Result<Vec<Value>> { Ok(api(&s, token, "GET", "/api/diffs/D2/comments")?.get("threads").and_then(Value::as_array).cloned().unwrap_or_default()) };
    assert!(threads(&dev)?.is_empty(), "a draft is its author's alone");
    assert_eq!(threads(&rev)?.len(), 1);
    let (st, r) = post(&s, &rev, "/api/diffs/D2/review", &json!({ "action": "comment", "body": "a question" }))?;
    assert!(st == 200 && r.get("published") == Some(&json!(1)), "{r}");
    let t = threads(&dev)?;
    let thread = t.first().map(|x| text(x, "id").to_owned()).unwrap_or_default();
    assert_eq!(t.first().and_then(|x| x.pointer("/now/start_line")), Some(&json!(2)), "{t:?}");
    let inbox = api(&s, &dev, "GET", "/api/review/inbox")?;
    assert!(inbox.get("waiting_for_you").and_then(Value::as_array).is_some_and(|w| w.iter().any(|x| text(x, "thread") == thread)), "{inbox}");
    let (st, r) = post(&s, &dev, "/api/diffs/D2/comments", &json!({ "thread": thread, "body": "for the test", "publish": true }))?;
    anyhow::ensure!(st == 200, "reply: {r}");
    let inbox = api(&s, &dev, "GET", "/api/review/inbox")?;
    assert!(inbox.get("waiting_for_others").and_then(Value::as_array).is_some_and(|w| w.iter().any(|x| text(x, "thread") == thread)), "{inbox}");
    let (st, r) = post(&s, &dev, "/api/diffs/D2/review", &json!({ "action": "accept" }))?;
    assert_eq!(st, 403, "the author doesn't accept their own: {r}");
    let (_, r) = post(&s, &rev, "/api/diffs/D1/review", &json!({ "action": "accept" }))?;
    assert_eq!(text(&r, "state"), "accepted", "{r}");
    let (_, r) = post(&s, &rev, "/api/diffs/D2/review", &json!({ "action": "accept" }))?;
    assert_eq!(text(&r, "state"), "accepted", "{r}");
    let land = api(&s, &dev, "GET", "/api/diffs/D2/land")?;
    assert_eq!(land.get("blockers"), Some(&json!(["1 open thread"])), "{land}");
    let (st, _) = http(s.port, "PATCH", &format!("/api/comments/{}", text(&c, "id")), &rev, Some(&json!({ "state": "resolved" })))?;
    assert_eq!(st, 200);
    // A developer doesn't land; a maintainer lands D2, and D1 with it, one commit each.
    let (st, _) = post(&s, &dev, "/api/diffs/D2/land", &json!({}))?;
    assert_eq!(st, 403);
    let (st, e) = post(&s, &lead, "/api/diffs/D2/land", &json!({ "strategy": "stack" }))?;
    assert!(st == 200 && e.get("diffs") == Some(&json!(["D1", "D2"])), "{st} {e}");
    until(30, "D2 lands", || Ok((text(&api(&s, &dev, "GET", "/api/diffs/D2")?, "state") != "landed").then(|| "not yet".to_owned())))?;
    must(git(&home, &work, &["fetch", "-q", "origin"]), "fetch")?;
    let log = must(git(&home, &work, &["log", "--format=%H %P%n%B%x1e", &format!("{trunk0}..origin/main")]), "log")?;
    let commits: Vec<&str> = log.split('\x1e').map(str::trim).filter(|c| !c.is_empty()).collect();
    assert_eq!(commits.len(), 2, "{log}");
    assert!(commits.iter().any(|c| c.contains("Differential-Revision: D1")) && commits.iter().any(|c| c.contains("Differential-Revision: D2") && c.contains("Reviewed-by: user:rev")), "{log}");
    let d1 = api(&s, &dev, "GET", "/api/diffs/D1")?;
    assert_eq!(text(&d1, "state"), "landed");

    // AC04: a diff loosening the policy is judged by the trunk's.
    must(git(&home, &work, &["checkout", "-q", "-B", "loosen", "origin/main"]), "checkout")?;
    std::fs::write(work.join(".kiln/review.yaml"), "land:\n  approvals: 0\n")?;
    must(git(&home, &work, &["commit", "-qam", &format!("loosen\n\nChange-Id: {}", "I3333333333333333333333333333333333333333")]), "commit")?;
    must(git(&home, &work, &["push", "-q", "origin", "HEAD:refs/for/main"]), "submit")?;
    let (st, why) = post(&s, &lead, "/api/diffs/D3/land", &json!({}))?;
    assert!(st == 409 && why.to_string().contains("0 of 1 accepts"), "{st} {why}");

    // stack-squash: D4 and D5 as one commit; D6 above them, not landed, is restacked onto it.
    must(git(&home, &work, &["checkout", "-q", "-B", "s2", "origin/main"]), "checkout")?;
    commit(&home, &work, "c.txt", "c\n", "four: c", "I4444444444444444444444444444444444444444")?;
    commit(&home, &work, "d.txt", "d\n", "five: d", "I5555555555555555555555555555555555555555")?;
    commit(&home, &work, "e.txt", "e\n", "six: e", "I6666666666666666666666666666666666666666")?;
    must(git(&home, &work, &["push", "-q", "origin", "HEAD:refs/for/main"]), "submit")?;
    for d in ["D4", "D5", "D6"] {
        post(&s, &rev, &format!("/api/diffs/{d}/review"), &json!({ "action": "accept" }))?;
    }
    let before = must(git(&home, &work, &["rev-parse", "origin/main"]), "rev-parse")?.trim().to_owned();
    let (st, e) = post(&s, &lead, "/api/diffs/D5/land", &json!({ "strategy": "stack-squash" }))?;
    assert!(st == 200 && e.get("diffs") == Some(&json!(["D4", "D5"])), "{st} {e}");
    until(30, "D5 lands", || Ok((text(&api(&s, &dev, "GET", "/api/diffs/D5")?, "state") != "landed").then(|| "not yet".to_owned())))?;
    must(git(&home, &work, &["fetch", "-q", "origin"]), "fetch")?;
    let log = must(git(&home, &work, &["log", "--format=%B%x1e", &format!("{before}..origin/main")]), "log")?;
    let one: Vec<&str> = log.split('\x1e').map(str::trim).filter(|c| !c.is_empty()).collect();
    assert!(one.len() == 1 && one.first().is_some_and(|c| c.contains("Differential-Revision: D4") && c.contains("Differential-Revision: D5")), "{log}");
    let tip = must(git(&home, &work, &["rev-parse", "origin/main"]), "rev-parse")?.trim().to_owned();
    until(30, "D6 restacked", || {
        let d6 = api(&s, &dev, "GET", "/api/diffs/D6")?;
        let last = d6.get("versions").and_then(Value::as_array).and_then(|v| v.last()).cloned().unwrap_or_default();
        Ok((!(last.get("rebase_only") == Some(&json!(true)) && text(&last, "parent") == tip && d6.get("parent") == Some(&Value::Null))).then(|| d6.to_string()))
    })?;

    // ff-only: the trunk moves to the commit as it is; a diverged one is refused.
    must(git(&home, &work, &["fetch", "-q", "origin"]), "fetch")?;
    must(git(&home, &work, &["checkout", "-q", "-B", "ff", "origin/main"]), "checkout")?;
    let c7 = commit(&home, &work, "f.txt", "f\n", "seven: f", "I7777777777777777777777777777777777777777")?;
    must(git(&home, &work, &["push", "-q", "origin", "HEAD:refs/for/main"]), "submit")?;
    post(&s, &rev, "/api/diffs/D7/review", &json!({ "action": "accept" }))?;
    let (st, e) = post(&s, &lead, "/api/diffs/D7/land", &json!({ "strategy": "ff-only" }))?;
    anyhow::ensure!(st == 200, "ff-only: {e}");
    until(30, "D7 lands", || Ok((text(&api(&s, &dev, "GET", "/api/diffs/D7")?, "state") != "landed").then(|| "not yet".to_owned())))?;
    let refs = must(git(&home, &work, &["ls-remote", &url(&dev), "refs/heads/main"]), "ls-remote")?;
    assert!(refs.starts_with(&c7), "the trunk is the commit itself: {refs} vs {c7}");
    // D8 on an old trunk: ff-only can't.
    must(git(&home, &work, &["checkout", "-q", "-B", "old", &trunk0]), "checkout")?;
    commit(&home, &work, "g.txt", "g\n", "eight: g", "I8888888888888888888888888888888888888888")?;
    must(git(&home, &work, &["push", "-q", "origin", "HEAD:refs/for/main"]), "submit")?;
    post(&s, &rev, "/api/diffs/D8/review", &json!({ "action": "accept" }))?;
    let (st, e) = post(&s, &lead, "/api/diffs/D8/land", &json!({ "strategy": "ff-only" }))?;
    anyhow::ensure!(st == 200, "queued: {e}");
    let entry = text(&e, "entry").to_owned();
    until(30, "D8's entry is ejected", || {
        let q = api(&s, &dev, "GET", "/api/repos/acme%2Fapp/queue")?;
        let mine = q.get("entries").and_then(Value::as_array).and_then(|l| l.iter().find(|x| text(x, "id") == entry).cloned()).unwrap_or_default();
        Ok((!(text(&mine, "state") == "ejected" && text(&mine, "reason").contains("ff-only"))).then(|| mine.to_string()))
    })?;
    assert_eq!(text(&api(&s, &dev, "GET", "/api/diffs/D8")?, "state"), "accepted", "back to its review state");
    Ok(())
}

/// W14-AC03: four entries tested as one batch whose candidate fails (one of them brings `BAD`,
/// which only the `merge_group` run rejects): the queue bisects, ejects the culprit with its
/// reason and lands the other three; and an entry that conflicts with the trunk is ejected as
/// "needs rebase". The checks run for real on the server's own runner, which fetches each commit
/// through kiln's git with the leg's token.
#[test]
fn w14_ac03_queue() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { agent: true, ..Options::default() })?;
    let home = dir.path().join("githome");
    std::fs::create_dir_all(&home)?;
    let url = |token: &str| format!("http://x:{token}@127.0.0.1:{}/git/acme/app.git", s.port);
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/app", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "native repository: {st} {body}");
    let dev = person(&s, "dev", &["developer@repo:acme/app"])?;
    let rev = person(&s, "rev", &["developer@repo:acme/app"])?;
    let lead = person(&s, "lead", &["maintainer@repo:acme/app"])?;

    must(git(&home, dir.path(), &["clone", "-q", &url(&dev), "work"]), "clone")?;
    let work = dir.path().join("work");
    std::fs::create_dir_all(work.join(".kiln"))?;
    std::fs::create_dir_all(work.join(".github/workflows"))?;
    std::fs::write(work.join(".kiln/review.yaml"), "land:\n  approvals: 1\n  checks: [gate]\nqueue:\n  batch: 4\n")?;
    std::fs::write(
        work.join(".github/workflows/gate.yml"),
        r#"name: gate
on: [pull_request, merge_group]
jobs:
  gate:
    runs-on: local
    steps:
      - env:
          TOKEN: ${{ github.token }}
        run: |
          git init -q .
          git -c http.extraHeader="Authorization: Bearer $TOKEN" fetch -q --depth=1 "$GITHUB_SERVER_URL/$GITHUB_REPOSITORY.git" "$GITHUB_SHA"
          git checkout -q FETCH_HEAD
      - if: github.event_name == 'merge_group'
        run: test ! -f BAD
"#,
    )?;
    // GitHub's merge queue events (W03-R02): each entry's enqueued and dequeued, with why.
    std::fs::write(
        work.join(".github/workflows/queue.yml"),
        "name: queue\non:\n  pull_request:\n    types: [enqueued, dequeued]\njobs:\n  q:\n    runs-on: local\n    steps:\n      - run: echo \"queue ${{ github.event.action }} ${{ github.event.reason }} D${{ github.event.pull_request.number }}\"\n",
    )?;
    std::fs::write(work.join("README.md"), "app\n")?;
    must(git(&home, &work, &["add", "."]), "add")?;
    must(git(&home, &work, &["commit", "-qm", "first"]), "commit")?;
    must(git(&home, &work, &["push", "-q", "origin", "main"]), "the first trunk")?;
    let trunk0 = must(git(&home, &work, &["rev-parse", "HEAD"]), "rev-parse")?.trim().to_owned();

    // Four diffs, each on the trunk: one brings BAD. Two more change the same line.
    let files = [("a.txt", "a: one"), ("BAD", "b: the culprit"), ("c.txt", "c: three"), ("d.txt", "d: four")];
    for (i, (file, title)) in files.iter().enumerate() {
        must(git(&home, &work, &["checkout", "-q", "-B", &format!("s{i}"), &trunk0]), "checkout")?;
        commit(&home, &work, file, "x\n", title, &format!("I{}", format!("{i}").repeat(40)))?;
        must(git(&home, &work, &["push", "-q", "origin", "HEAD:refs/for/main"]), "submit")?;
    }
    for (i, line) in ["app, said one way\n", "app, said another\n"].iter().enumerate() {
        must(git(&home, &work, &["checkout", "-q", "-B", &format!("r{i}"), &trunk0]), "checkout")?;
        commit(&home, &work, "README.md", line, &format!("readme {i}"), &format!("I{}", format!("{}", 5 + i).repeat(40)))?;
        must(git(&home, &work, &["push", "-q", "origin", "HEAD:refs/for/main"]), "submit")?;
    }
    for d in ["D1", "D2", "D3", "D4", "D5", "D6"] {
        let (st, r) = post(&s, &rev, &format!("/api/diffs/{d}/review"), &json!({ "action": "accept" }))?;
        anyhow::ensure!(st == 200, "{d}: {r}");
    }
    // Each version's gate passes (the merge_group step is skipped on pull requests).
    for d in ["D1", "D2", "D3", "D4", "D5", "D6"] {
        until(180, &format!("{d}'s gate"), || {
            let l = api(&s, &dev, "GET", &format!("/api/diffs/{d}/land"))?;
            Ok((l.get("blockers") != Some(&json!([]))).then(|| format!("{l}\n{}", run_logs(&s, d))))
        })?;
    }

    // Four entries at once: one batch, which fails; bisected, the culprit goes, three land.
    for d in ["D1", "D2", "D3", "D4"] {
        let (st, e) = post(&s, &lead, &format!("/api/diffs/{d}/land"), &json!({}))?;
        anyhow::ensure!(st == 200, "{d}: {e}");
    }
    let state = |d: &str| -> anyhow::Result<String> { Ok(text(&api(&s, &dev, "GET", &format!("/api/diffs/{d}"))?, "state").to_owned()) };
    until(400, "the queue settles", || {
        let states: Vec<String> = ["D1", "D2", "D3", "D4"].iter().map(|d| state(d)).collect::<anyhow::Result<_>>()?;
        Ok((states != ["landed", "accepted", "landed", "landed"]).then(|| format!("{states:?}\n{}", api(&s, &dev, "GET", "/api/repos/acme%2Fapp/queue").map(|q| q.to_string()).unwrap_or_default())))
    })?;
    let q = api(&s, &dev, "GET", "/api/repos/acme%2Fapp/queue")?;
    let culprit = q.get("entries").and_then(Value::as_array).and_then(|l| l.iter().find(|x| x.get("diffs") == Some(&json!(["D2"])))).cloned().unwrap_or_default();
    assert!(text(&culprit, "state") == "ejected" && text(&culprit, "reason").contains("`gate` failure"), "{culprit}");
    must(git(&home, &work, &["fetch", "-q", "origin"]), "fetch")?;
    let landed = must(git(&home, &work, &["log", "--format=%s", &format!("{trunk0}..origin/main")]), "log")?;
    assert_eq!(landed.lines().count(), 3, "{landed}");
    assert!(!landed.contains("culprit"), "{landed}");

    // The two README changes: the first lands, the second needs a rebase.
    for d in ["D5", "D6"] {
        let (st, e) = post(&s, &lead, &format!("/api/diffs/{d}/land"), &json!({}))?;
        anyhow::ensure!(st == 200, "{d}: {e}");
    }
    until(300, "D5 lands and D6 is ejected", || {
        let (a, b) = (state("D5")?, state("D6")?);
        Ok(!(a == "landed" && b == "accepted")).map(|w| w.then(|| format!("{a} {b}")))
    })?;
    let q = api(&s, &dev, "GET", "/api/repos/acme%2Fapp/queue")?;
    let d6 = q.get("entries").and_then(Value::as_array).and_then(|l| l.iter().find(|x| x.get("diffs") == Some(&json!(["D6"])))).cloned().unwrap_or_default();
    assert!(text(&d6, "reason").contains("needs rebase") && text(&d6, "reason").contains("README.md"), "{d6}");
    // Each entry's merge queue events, with GitHub's reasons.
    let wanted = ["queue enqueued  D1", "queue dequeued MERGE D1", "queue dequeued CI_FAILURE D2", "queue dequeued MERGE D5", "queue dequeued INVALID_MERGE_COMMIT D6"];
    until(240, "the merge queue events", || {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/app&limit=200", &s.admin, None)?;
        let runs: Vec<Value> = serde_json::from_str::<Value>(&body)?.get("runs").and_then(Value::as_array).cloned().unwrap_or_default();
        let mut said = String::new();
        for r in runs.iter().filter(|r| text(r, "workflow").ends_with("queue.yml") && r.get("finished_at").is_some_and(|f| !f.is_null())) {
            let detail = api(&s, &s.admin, "GET", &format!("/api/ci/runs/{}", text(r, "id")))?;
            let leg = detail.pointer("/legs/0/id").and_then(Value::as_str).unwrap_or_default().to_owned();
            let (_, log) = http(s.port, "GET", &format!("/api/ci/legs/{leg}/logs?step=0"), &s.admin, None)?;
            said.push_str(&log);
        }
        Ok((!wanted.iter().all(|w| said.contains(w))).then_some(said))
    })?;
    Ok(())
}

/// The logs of the runs on a diff's latest version, for a failure's message.
fn run_logs(s: &Server, diff: &str) -> String {
    let mut out = String::new();
    let Ok(d) = api(s, &s.admin, "GET", &format!("/api/diffs/{diff}")) else { return out };
    for r in d.get("runs").and_then(Value::as_array).into_iter().flatten() {
        let Ok(run) = api(s, &s.admin, "GET", &format!("/api/runs/{}", text(r, "id"))) else { continue };
        for j in run.get("jobs").and_then(Value::as_array).into_iter().flatten() {
            if let Ok((_, logs)) = http(s.port, "GET", &format!("/api/jobs/{}/logs", text(j, "id")), &s.admin, None) {
                out.push_str(&format!("--- {} {}\n{}\n", text(r, "event"), text(j, "display"), logs.chars().rev().take(3000).collect::<String>().chars().rev().collect::<String>()));
            }
        }
    }
    out
}

/// W14-R04 and AC02's other half: a change request opened by a push to
/// `refs/for/main/branch/<name>`, its versions the branch's pushes, reviewed and landed whole by
/// each strategy a change request lands by (`squash`: one commit; `merge`: a merge commit;
/// `rebase`: each commit replayed; `ff-only`: the branch itself, refused once the trunk moved on);
/// opened through the API and `kiln cr open` too.
#[test]
fn w14_r04_change_requests() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options::default())?;
    let home = dir.path().join("githome");
    std::fs::create_dir_all(&home)?;
    let url = |token: &str| format!("http://x:{token}@127.0.0.1:{}/git/acme/app.git", s.port);
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/app", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "native repository: {st} {body}");
    let dev = person(&s, "dev", &["developer@repo:acme/app"])?;
    let rev = person(&s, "rev", &["developer@repo:acme/app"])?;
    let lead = person(&s, "lead", &["maintainer@repo:acme/app"])?;
    must(git(&home, dir.path(), &["clone", "-q", &url(&dev), "work"]), "clone")?;
    let work = dir.path().join("work");
    let g = |args: &[&str]| must(git(&home, &work, args), &args.join(" "));
    std::fs::write(work.join("README.md"), "app\n")?;
    g(&["add", "."])?;
    g(&["commit", "-qm", "first"])?;
    g(&["push", "-q", "origin", "main"])?;
    let plain = |file: &str, title: &str| -> anyhow::Result<()> {
        std::fs::write(work.join(file), format!("{title}\n"))?;
        g(&["add", file])?;
        g(&["commit", "-qm", title])?;
        Ok(())
    };
    let tip = || -> anyhow::Result<String> {
        g(&["fetch", "-q", "origin"])?;
        Ok(g(&["rev-parse", "origin/main"])?.trim().to_owned())
    };
    let wait_landed = |cr: &str| until(30, &format!("{cr} lands"), || Ok((text(&api(&s, &dev, "GET", &format!("/api/change-requests/{cr}"))?, "state") != "landed").then(|| "not yet".to_owned())));

    // Opened by a push; its versions are the branch's pushes.
    g(&["checkout", "-qb", "topic"])?;
    plain("a.txt", "topic: a")?;
    plain("b.txt", "topic: b")?;
    let out = g(&["push", "-o", "title=Topic work", "origin", "topic:refs/for/main/branch/topic"])?;
    assert!(out.contains("CR1 v1 (new)  Topic work"), "{out}");
    let cr = api(&s, &dev, "GET", "/api/change-requests/CR1")?;
    assert_eq!((text(&cr, "kind"), text(&cr, "branch"), text(&cr, "name")), ("change_request", "topic", "CR1"), "{cr}");
    assert_eq!(cr.get("commits").and_then(Value::as_array).map(Vec::len), Some(2), "{cr}");
    plain("c.txt", "topic: c")?;
    let out = g(&["push", "origin", "topic"])?;
    assert!(out.contains("CR1 v2 (updated)"), "a plain push of the branch: {out}");
    let files = api(&s, &dev, "GET", "/api/change-requests/CR1/versions/2/files")?;
    let names: Vec<&str> = files.get("files").and_then(Value::as_array).into_iter().flatten().map(|f| text(f, "path")).collect();
    assert_eq!(names, ["a.txt", "b.txt", "c.txt"], "{files}");
    let inter = api(&s, &dev, "GET", "/api/change-requests/CR1/interdiff?from=1&to=2")?;
    assert!(text(&inter, "range_diff").contains("topic: c"), "{inter}");
    let listed = api(&s, &dev, "GET", "/api/repos/acme%2Fapp/change-requests")?;
    assert_eq!(listed.get("change_requests").and_then(Value::as_array).map(Vec::len), Some(1), "{listed}");
    let (st, r) = post(&s, &lead, "/api/change-requests/CR1/land", &json!({ "strategy": "stack" }))?;
    assert!(st == 422 && r.to_string().contains("doesn't land a change request"), "{st} {r}");

    // Each strategy, on its own change request.
    for (n, strategy) in (1..).zip(["squash", "merge", "rebase", "ff-only"]) {
        let before = tip()?;
        if n > 1 {
            g(&["checkout", "-qB", strategy, "origin/main"])?;
            plain(&format!("{strategy}-1.txt"), &format!("{strategy}: one"))?;
            plain(&format!("{strategy}-2.txt"), &format!("{strategy}: two"))?;
            let out = g(&["push", "origin", &format!("{strategy}:refs/for/main/branch/{strategy}")])?;
            assert!(out.contains(&format!("CR{n} v1 (new)")), "{out}");
        }
        let name = format!("CR{n}");
        let (st, r) = post(&s, &rev, &format!("/api/change-requests/{name}/review"), &json!({ "action": "accept" }))?;
        anyhow::ensure!(st == 200, "{name}: {r}");
        let (st, e) = post(&s, &lead, &format!("/api/change-requests/{name}/land"), &json!({ "strategy": strategy }))?;
        anyhow::ensure!(st == 200, "{name} {strategy}: {e}");
        wait_landed(&name)?;
        let after = tip()?;
        let count = |range: &str, first_parent: bool| -> anyhow::Result<usize> {
            let mut args = vec!["rev-list", "--count"];
            if first_parent {
                args.push("--first-parent");
            }
            args.push(range);
            Ok(g(&args)?.trim().parse()?)
        };
        let range = format!("{before}..{after}");
        let msg = g(&["log", "-1", "--format=%B", &after])?;
        let parents = g(&["log", "-1", "--format=%P", &after])?.split_whitespace().count();
        match strategy {
            "squash" => {
                assert_eq!((count(&range, false)?, parents), (1, 1), "{strategy}");
                assert!(msg.contains("Change-Request: CR1") && msg.contains("Reviewed-by: user:rev") && msg.contains("Topic work"), "{msg}");
            }
            "merge" => {
                assert_eq!((count(&range, true)?, count(&range, false)?, parents), (1, 3, 2), "{strategy}");
                assert!(msg.starts_with(&format!("Merge {name}")) && msg.contains(&format!("Change-Request: {name}")), "{msg}");
            }
            "rebase" => {
                assert_eq!((count(&range, false)?, parents), (2, 1), "{strategy}");
                let both = g(&["log", "--format=%B", &range])?;
                assert_eq!(both.matches(&format!("Change-Request: {name}")).count(), 2, "{both}");
            }
            _ => {
                let head = g(&["rev-parse", strategy])?.trim().to_owned();
                assert_eq!(after, head, "ff-only: the trunk is the branch itself");
                assert!(!msg.contains("Change-Request"), "{msg}");
            }
        }
    }

    // ff-only, once the trunk moved on: refused, saying why.
    g(&["checkout", "-qB", "late", "origin/main~2"])?;
    plain("late.txt", "late: one")?;
    let out = g(&["push", "origin", "late:refs/for/main/branch/late"])?;
    assert!(out.contains("CR5 v1 (new)"), "{out}");
    post(&s, &rev, "/api/change-requests/CR5/review", &json!({ "action": "accept" }))?;
    let (_, e) = post(&s, &lead, "/api/change-requests/CR5/land", &json!({ "strategy": "ff-only" }))?;
    let entry = text(&e, "entry").to_owned();
    until(30, "CR5 is ejected", || {
        let q = api(&s, &dev, "GET", "/api/repos/acme%2Fapp/queue")?;
        let mine = q.get("entries").and_then(Value::as_array).and_then(|l| l.iter().find(|x| text(x, "id") == entry).cloned()).unwrap_or_default();
        Ok((!(text(&mine, "state") == "ejected" && text(&mine, "reason").contains("ff-only"))).then(|| mine.to_string()))
    })?;

    // Opened through the API for a branch already pushed, and with `kiln cr open`.
    g(&["checkout", "-qB", "pushed", "origin/main"])?;
    plain("p.txt", "pushed: p")?;
    g(&["push", "-q", "origin", "pushed"])?;
    let (st, r) = post(&s, &dev, "/api/repos/acme%2Fapp/change-requests", &json!({ "branch": "pushed", "title": "Already there" }))?;
    assert!(st == 200 && text(&r, "change_request") == "CR6", "{st} {r}");
    g(&["checkout", "-qB", "cli", "origin/main"])?;
    plain("q.txt", "cli: q")?;
    let out = must(kiln(&home, &work, &s, &dev, &["cr", "open", "--title", "From the CLI"]), "kiln cr open")?;
    assert!(out.contains("CR7 v1 (new)  From the CLI"), "{out}");
    plain("r.txt", "cli: r")?;
    let out = g(&["push"])?;
    assert!(out.contains("CR7 v2 (updated)"), "the branch tracks the server's: {out}");
    Ok(())
}

/// W14-R11: a diff is a pull request to a job's GitHub API calls: a `pull_request` workflow reads
/// it and its files, comments on it as a whole (and edits that comment, as sticky-comment actions
/// do), comments on a line, and approves; kiln keeps each as its own, the job as
/// `github-actions[bot]`, an agent whose approval counts only where agents may approve.
#[test]
fn w14_r11_pulls() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { agent: true, ..Options::default() })?;
    let home = dir.path().join("githome");
    std::fs::create_dir_all(&home)?;
    let url = |token: &str| format!("http://x:{token}@127.0.0.1:{}/git/acme/app.git", s.port);
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/app", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "native repository: {st} {body}");
    let dev = person(&s, "dev", &["developer@repo:acme/app"])?;
    must(git(&home, dir.path(), &["clone", "-q", &url(&dev), "work"]), "clone")?;
    let work = dir.path().join("work");
    std::fs::create_dir_all(work.join(".github/workflows"))?;
    std::fs::write(
        work.join(".github/workflows/bot.yml"),
        r#"on: pull_request
permissions:
  pull-requests: write
  issues: write
jobs:
  bot:
    runs-on: local
    steps:
      - env:
          TOKEN: ${{ github.token }}
          N: ${{ github.event.pull_request.number }}
          SHA: ${{ github.event.pull_request.head.sha }}
        run: |
          set -e
          api() { curl -sf -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" "$@"; }
          R="$GITHUB_API_URL/repos/$GITHUB_REPOSITORY"
          api "$R/pulls/$N" | grep -q '"title":"one: a"'
          api "$R/pulls/$N/files" | grep -q '"filename":"a.txt"'
          id=$(api -X POST "$R/issues/$N/comments" -d '{"body":"coverage: 90%"}' | sed -n 's/^{"id":\([0-9]*\).*/\1/p')
          api -X PATCH "$R/issues/comments/$id" -d '{"body":"coverage: 91%"}' > /dev/null
          api -X POST "$R/pulls/$N/comments" -d "{\"body\":\"nit\",\"path\":\"a.txt\",\"line\":1,\"side\":\"RIGHT\",\"commit_id\":\"$SHA\"}" > /dev/null
          api -X POST "$R/pulls/$N/reviews" -d '{"event":"APPROVE","body":"bot says yes"}' | grep -q '"state":"APPROVED"'
          echo done
"#,
    )?;
    std::fs::write(work.join("README.md"), "app\n")?;
    must(git(&home, &work, &["add", "."]), "add")?;
    must(git(&home, &work, &["commit", "-qm", "first"]), "commit")?;
    must(git(&home, &work, &["push", "-q", "origin", "main"]), "the first trunk")?;
    commit(&home, &work, "a.txt", "one\n", "one: a", "I1111111111111111111111111111111111111111")?;
    must(git(&home, &work, &["push", "-q", "origin", "HEAD:refs/for/main"]), "submit")?;

    // The bot's run passes, and what it did is kiln's.
    until(180, "the bot's run", || {
        let d = api(&s, &dev, "GET", "/api/diffs/D1")?;
        let done = d.get("runs").and_then(Value::as_array).is_some_and(|r| r.iter().any(|x| text(x, "status") == "succeeded"));
        let failed = d.get("runs").and_then(Value::as_array).is_some_and(|r| r.iter().any(|x| text(x, "status") == "failed"));
        anyhow::ensure!(!failed, "the bot's run failed:\n{}", run_logs(&s, "D1"));
        Ok((!done).then(|| d.to_string()))
    })?;
    let threads = api(&s, &dev, "GET", "/api/diffs/D1/comments")?;
    let list = threads.get("threads").and_then(Value::as_array).cloned().unwrap_or_default();
    let general = list.iter().find(|t| t.get("file") == Some(&Value::Null)).cloned().unwrap_or_default();
    let inline = list.iter().find(|t| text(t, "file") == "a.txt").cloned().unwrap_or_default();
    let first = |t: &Value| t.pointer("/comments/0").cloned().unwrap_or_default();
    assert_eq!((text(&first(&general), "body"), text(&first(&general), "author")), ("coverage: 91%", text(&first(&general), "author")), "{threads}");
    assert!(text(&first(&general), "author").starts_with("job:"), "{threads}");
    assert_eq!((text(&first(&inline), "body"), inline.get("start_line")), ("nit", Some(&json!(1))), "{threads}");
    // The bot's approval is an agent's: it doesn't count here.
    let land = api(&s, &dev, "GET", "/api/diffs/D1/land")?;
    assert!(land.get("blockers").and_then(Value::as_array).is_some_and(|b| b.iter().any(|x| x.as_str().is_some_and(|w| w.starts_with("0 of 1 accepts")))), "{land}");
    // A comment on the diff as a whole doesn't hold the land up; the line comment does.
    assert!(land.get("blockers").and_then(Value::as_array).is_some_and(|b| b.iter().any(|x| x.as_str() == Some("1 open thread"))), "{land}");
    Ok(())
}

/// The rest of hosting and review: branch protection (W14-R02: not deleted, not rewritten,
/// pushed by those who land), a reviewer's suggestion applied as an amend (R05), landed commits
/// signed (R07), the trunk mirrored upstream after a land (R01 `native + mirror`), and a patch
/// series submitted without git (R03, `git format-patch` output).
#[test]
fn w14_r05_suggestions_mirror_signing() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let key = dir.path().join("land_key");
    let signing = Command::new("ssh-keygen").args(["-q", "-t", "ed25519", "-N", "", "-C", "kiln-land", "-f"]).arg(&key).status().map(|s| s.success()).unwrap_or(false);
    let ci = if signing { format!("land_signing = {{ key = \"{}\" }}", key.display()) } else { String::new() };
    let s = start(dir.path(), &Options { ci: &ci, ..Options::default() })?;
    let home = dir.path().join("githome");
    std::fs::create_dir_all(&home)?;
    let url = |token: &str, repo: &str| format!("http://x:{token}@127.0.0.1:{}/git/acme/{repo}.git", s.port);
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/upstream", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "upstream: {st} {body}");
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/app", "mode": "native", "mirror": url(&s.admin, "upstream") })))?;
    anyhow::ensure!(st == 200, "native repository: {st} {body}");
    let dev = person(&s, "dev", &["developer@repo:acme/app"])?;
    let rev = person(&s, "rev", &["developer@repo:acme/app"])?;
    let lead = person(&s, "lead", &["maintainer@repo:acme/app"])?;
    must(git(&home, dir.path(), &["clone", "-q", &url(&dev, "app"), "work"]), "clone")?;
    let work = dir.path().join("work");
    let g = |args: &[&str]| must(git(&home, &work, args), &args.join(" "));
    std::fs::create_dir_all(work.join(".kiln"))?;
    std::fs::write(work.join(".kiln/review.yaml"), "land:\n  approvals: 1\nprotected: [\"release/*\"]\n")?;
    std::fs::write(work.join("README.md"), "app\n")?;
    g(&["add", "."])?;
    g(&["commit", "-qm", "first"])?;
    g(&["push", "-q", "origin", "main"])?;

    // Protected branches: those who land push them; nobody rewrites or deletes them.
    let (ok, out) = git(&home, &work, &["push", "origin", "HEAD:refs/heads/release/1"])?;
    assert!(!ok && out.contains("release/1 is protected") && out.contains("diffs.land"), "{out}");
    g(&["push", "-q", &url(&lead, "app"), "HEAD:refs/heads/release/1"])?;
    g(&["commit", "-q", "--allow-empty", "-m", "a fork"])?;
    let (ok, out) = git(&home, &work, &["push", "-f", &url(&lead, "app"), "HEAD~1:refs/heads/release/1", "--force"])?;
    assert!(ok || out.contains("Everything up-to-date"), "the same commit is no rewrite: {out}");
    g(&["commit", "-q", "--amend", "--allow-empty", "-m", "another fork"])?;
    g(&["push", "-q", &url(&lead, "app"), "HEAD:refs/heads/release/1"])?;
    g(&["reset", "-q", "--hard", "HEAD~1"])?;
    g(&["commit", "-q", "--allow-empty", "-m", "a rewrite"])?;
    let (ok, out) = git(&home, &work, &["push", "-f", &url(&lead, "app"), "HEAD:refs/heads/release/1"])?;
    assert!(!ok && out.contains("isn't rewritten") && out.contains("non-fast-forward"), "{out}");
    let (ok, out) = git(&home, &work, &["push", &url(&lead, "app"), ":refs/heads/release/1"])?;
    assert!(!ok && out.contains("isn't deleted"), "{out}");
    g(&["reset", "-q", "--hard", "origin/main"])?;

    // A suggestion, applied by the author: a new version with the lines replaced.
    commit(&home, &work, "a.txt", "one\ntwo\nthree\n", "one: a", "I1111111111111111111111111111111111111111")?;
    g(&["push", "-q", "origin", "HEAD:refs/for/main"])?;
    let (st, c) = post(&s, &rev, "/api/diffs/D1/comments", &json!({ "file": "a.txt", "start_line": 2, "body": "shout it", "suggestion": "TWO", "publish": true }))?;
    anyhow::ensure!(st == 200, "{c}");
    let (st, r) = post(&s, &dev, &format!("/api/comments/{}/apply", text(&c, "id")), &json!({}))?;
    anyhow::ensure!(st == 200, "apply: {r}");
    assert_eq!(r.get("version"), Some(&json!(2)), "{r}");
    let d1 = api(&s, &dev, "GET", "/api/diffs/D1")?;
    let v2 = d1.get("versions").and_then(Value::as_array).and_then(|v| v.last()).cloned().unwrap_or_default();
    g(&["fetch", "-q", "origin", "refs/kiln/diffs/D1/v2"])?;
    assert_eq!(g(&["show", "FETCH_HEAD:a.txt"])?, "one\nTWO\nthree\n");
    assert!(g(&["log", "-1", "--format=%B", "FETCH_HEAD"])?.contains("Change-Id: I1111111111111111111111111111111111111111"), "the amend keeps its Change-Id");
    assert_eq!(text(&v2, "rebase_only"), "", "{v2}");
    let threads = api(&s, &dev, "GET", "/api/diffs/D1/comments")?;
    assert_eq!(threads.pointer("/threads/0/state"), Some(&json!("resolved")), "{threads}");

    // Landed, signed, and mirrored upstream.
    post(&s, &rev, "/api/diffs/D1/review", &json!({ "action": "accept" }))?;
    let (st, e) = post(&s, &lead, "/api/diffs/D1/land", &json!({}))?;
    anyhow::ensure!(st == 200, "land: {e}");
    until(30, "D1 lands", || Ok((text(&api(&s, &dev, "GET", "/api/diffs/D1")?, "state") != "landed").then(|| "not yet".to_owned())))?;
    g(&["fetch", "-q", "origin"])?;
    let landed = g(&["rev-parse", "origin/main"])?.trim().to_owned();
    if signing {
        assert!(g(&["cat-file", "commit", &landed])?.contains("gpgsig -----BEGIN SSH SIGNATURE-----"), "signed");
    }
    until(30, "the mirror has the trunk", || {
        let up = must(git(&home, &work, &["ls-remote", &url(&s.admin, "upstream"), "refs/heads/main"]), "ls-remote")?;
        Ok((!up.starts_with(&landed)).then_some(up))
    })?;

    // A patch series without git: `git format-patch` output, submitted over the API.
    g(&["reset", "-q", "--hard", "origin/main"])?;
    std::fs::write(work.join("b.txt"), "bee\n")?;
    g(&["add", "b.txt"])?;
    g(&["commit", "-qm", "two: b\n\nWhy bees."])?;
    let mbox = g(&["format-patch", "-1", "--stdout"])?;
    let (st, r) = post(&s, &dev, "/api/repos/acme%2Fapp/submit", &json!({ "mbox": mbox, "draft": true }))?;
    anyhow::ensure!(st == 200, "submit: {st} {r}");
    assert_eq!(r.pointer("/diffs/0/name"), Some(&json!("D2")), "{r}");
    let d2 = api(&s, &dev, "GET", "/api/diffs/D2")?;
    assert_eq!((text(&d2, "title"), text(&d2, "state"), text(&d2, "summary")), ("two: b", "draft", "Why bees."), "{d2}");
    let (st, r) = post(&s, &dev, "/api/repos/acme%2Fapp/submit", &json!({ "patches": [{ "message": "nope", "diff": "diff --git a/zz b/zz\n--- a/zz\n+++ b/zz\n@@ -1 +1 @@\n-x\n+y\n" }] }))?;
    assert!(st == 422 && r.to_string().contains("doesn't apply"), "{st} {r}");
    Ok(())
}

/// A GET's status, headers (names lowercase) and body as bytes.
fn get_raw(port: u16, token: &str, path: &str) -> anyhow::Result<(u16, std::collections::HashMap<String, String>, Vec<u8>)> {
    use std::io::{Read as _, Write as _};
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port))?;
    s.set_read_timeout(Some(Duration::from_secs(60)))?;
    write!(s, "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n")?;
    let mut all = Vec::new();
    s.read_to_end(&mut all)?;
    let split = all.windows(4).position(|w| w == b"\r\n\r\n").ok_or_else(|| anyhow::anyhow!("no header end"))?;
    let head = String::from_utf8_lossy(all.get(..split).unwrap_or_default()).into_owned();
    let mut body = all.get(split + 4..).unwrap_or_default().to_vec();
    let status = head.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
    let headers: std::collections::HashMap<String, String> = head.lines().skip(1).filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned())).collect();
    if headers.get("transfer-encoding").is_some_and(|t| t.eq_ignore_ascii_case("chunked")) {
        let mut out = Vec::new();
        let mut rest = body.as_slice();
        while let Some(eol) = rest.windows(2).position(|w| w == b"\r\n") {
            let size = usize::from_str_radix(String::from_utf8_lossy(rest.get(..eol).unwrap_or_default()).trim(), 16).unwrap_or(0);
            if size == 0 {
                break;
            }
            out.extend_from_slice(rest.get(eol + 2..eol + 2 + size).unwrap_or_default());
            rest = rest.get(eol + 4 + size..).unwrap_or_default();
        }
        body = out;
    }
    Ok((status, headers, body))
}

/// What the review folds and shows as images (docs/ci.md W14, for W21's review pages): files
/// `.gitattributes` marks generated (`linguist-generated`) or not to diff (`-diff`, and `binary`)
/// say `generated`; a version's file is served as it is on either side, with its type, sandboxed,
/// up to 10 MiB.
#[test]
fn w14_generated_files_and_blobs() -> anyhow::Result<()> {
    const PNG_V1: &[u8] = b"\x89PNG\r\n\x1a\nthe first logo";
    const PNG_V2: &[u8] = b"\x89PNG\r\n\x1a\nthe second logo";
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options::default())?;
    let home = dir.path().join("githome");
    std::fs::create_dir_all(&home)?;
    let url = |token: &str| format!("http://x:{token}@127.0.0.1:{}/git/acme/app.git", s.port);
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/app", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "native repository: {st} {body}");
    let dev = person(&s, "dev", &["developer@repo:acme/app"])?;
    must(git(&home, dir.path(), &["clone", "-q", &url(&dev), "work"]), "clone")?;
    let work = dir.path().join("work");
    std::fs::write(work.join(".gitattributes"), "dist/** linguist-generated\n*.lock -diff\n*.bin binary\n")?;
    std::fs::write(work.join("README.md"), "app\n")?;
    std::fs::write(work.join("logo.png"), PNG_V1)?;
    must(git(&home, &work, &["add", "."]), "add")?;
    must(git(&home, &work, &["commit", "-qm", "first"]), "commit")?;
    must(git(&home, &work, &["push", "-q", "origin", "main"]), "the trunk")?;
    must(git(&home, &work, &["checkout", "-qb", "change"]), "branch")?;
    std::fs::create_dir_all(work.join("dist"))?;
    std::fs::write(work.join("dist/app.js"), "built();\n")?;
    std::fs::write(work.join("deps.lock"), "lock\n")?;
    std::fs::write(work.join("data.bin"), [0u8, 1, 2, 3])?;
    std::fs::write(work.join("src.rs"), "fn main() {}\n")?;
    std::fs::write(work.join("logo.png"), PNG_V2)?;
    std::fs::write(work.join("big.bin"), vec![0u8; 11 << 20])?;
    must(git(&home, &work, &["add", "."]), "add")?;
    must(git(&home, &work, &["commit", "-qm", "change\n\nChange-Id: I4444444444444444444444444444444444444444"]), "commit")?;
    must(git(&home, &work, &["push", "origin", "HEAD:refs/for/main"]), "submit")?;
    let files = api(&s, &dev, "GET", "/api/diffs/D1/versions/1/files")?;
    let generated = |p: &str| files.get("files").and_then(Value::as_array).and_then(|f| f.iter().find(|x| text(x, "path") == p)).and_then(|x| x.get("generated")).and_then(Value::as_bool);
    for (p, want) in [("dist/app.js", true), ("deps.lock", true), ("data.bin", true), ("big.bin", true), ("src.rs", false), ("logo.png", false)] {
        assert_eq!(generated(p), Some(want), "{p}: {files}");
    }
    // Both sides of the image, with its type, sandboxed.
    for (side, want) in [("old", PNG_V1), ("new", PNG_V2)] {
        let (st, h, bytes) = get_raw(s.port, &dev, &format!("/api/diffs/D1/versions/1/blob?path=logo.png&side={side}"))?;
        assert_eq!((st, bytes.as_slice()), (200, want), "{side}: {}", String::from_utf8_lossy(&bytes));
        assert_eq!(h.get("content-type").map(String::as_str), Some("image/png"), "{h:?}");
        assert_eq!(h.get("x-content-type-options").map(String::as_str), Some("nosniff"), "{h:?}");
        assert!(h.get("content-security-policy").is_some_and(|c| c.contains("sandbox")), "{h:?}");
    }
    // What isn't on a side, what's too big, and what isn't a path in the repository.
    let status = |q: &str| get_raw(s.port, &dev, &format!("/api/diffs/D1/versions/1/blob?{q}")).map(|r| r.0);
    assert_eq!(status("path=src.rs&side=old")?, 404, "new in this version");
    assert_eq!(status("path=big.bin")?, 413, "past 10 MiB");
    assert_eq!(status("path=../../etc/passwd")?, 400);
    assert_eq!(status("path=logo.png&side=both")?, 400);
    Ok(())
}

/// The facade's runs and code scanning (docs/ci.md W14-R11): a job reads its own run, the
/// repository's runs and its workflow as GitHub serves them, and code scanning takes SARIF (gzipped
/// and in base64), reads it into alerts, says it's complete, and marks an alert fixed when a
/// later analysis no longer finds it.
#[test]
fn w14_r11_runs_and_code_scanning() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { agent: true, ..Options::default() })?;
    let home = dir.path().join("githome");
    std::fs::create_dir_all(&home)?;
    let url = |token: &str| format!("http://x:{token}@127.0.0.1:{}/git/acme/app.git", s.port);
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/app", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "native repository: {st} {body}");
    let dev = person(&s, "dev", &["developer@repo:acme/app"])?;
    must(git(&home, dir.path(), &["clone", "-q", &url(&dev), "work"]), "clone")?;
    let work = dir.path().join("work");
    // The first trunk: the poller's first look takes it as it is.
    std::fs::write(work.join("README.md"), "app\n")?;
    must(git(&home, &work, &["add", "."]), "add")?;
    must(git(&home, &work, &["commit", "-qm", "first"]), "commit")?;
    must(git(&home, &work, &["push", "-q", "origin", "main"]), "the first trunk")?;
    until(60, "the first poll", || {
        let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        Ok((!(body.contains("acme/app") && !body.contains("\"last_polled_at\":null"))).then_some(body))
    })?;
    std::fs::create_dir_all(work.join(".github/workflows"))?;
    std::fs::write(
        work.join(".github/workflows/scan.yml"),
        r#"on: push
permissions:
  contents: read
  actions: read
  security-events: write
jobs:
  scan:
    runs-on: local
    steps:
      - env:
          TOKEN: ${{ github.token }}
        run: |
          set -e
          api() { curl -sf -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" "$@"; }
          R="$GITHUB_API_URL/repos/$GITHUB_REPOSITORY"
          run=$(api "$R/actions/runs/$GITHUB_RUN_ID")
          echo "$run" | grep -q '"path":".github/workflows/scan.yml"'
          wf=$(echo "$run" | sed -n 's/.*"workflow_url":"\([^"]*\)".*/\1/p')
          api "$wf" | grep -q '"state":"active"'
          api "$R/actions/runs?event=push" | grep -q '"total_count":1'
          api "$GITHUB_API_URL/meta" > /dev/null
          # What codeql-action asks before it analyzes: its features (none) and the repository's
          # custom properties (none).
          test "$(api "$R/code-scanning/codeql-action/features")" = "{}"
          test "$(api "$R/properties/values")" = "[]"
          sarif() { printf '{"version":"2.1.0","runs":[{"tool":{"driver":{"name":"Lint","rules":[{"id":"no-eval","defaultConfiguration":{"level":"error"}}]}},"results":[%s]}]}' "$1" | gzip -c | base64 -w0; }
          found='{"ruleId":"no-eval","message":{"text":"eval is evil"},"locations":[{"physicalLocation":{"artifactLocation":{"uri":"src/a.js"},"region":{"startLine":3}}}]}'
          up() { api -X POST "$R/code-scanning/sarifs" -d "{\"commit_sha\":\"$GITHUB_SHA\",\"ref\":\"$GITHUB_REF\",\"sarif\":\"$(sarif "$1")\",\"tool_name\":\"Lint\"}" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p'; }
          id=$(up "$found")
          api "$R/code-scanning/sarifs/$id" | grep -q '"processing_status":"complete"'
          api "$R/code-scanning/alerts?state=open" | grep -q '"id":"no-eval"'
          api "$R/code-scanning/analyses" | grep -q '"results_count":1'
          up "" > /dev/null
          api "$R/code-scanning/alerts?state=fixed" | grep -q '"number":1'
          code=$(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $TOKEN" -X POST "$R/code-scanning/sarifs" -d '{"commit_sha":"nope","ref":"refs/heads/main","sarif":"x"}')
          test "$code" = 422
          echo scanned
"#,
    )?;
    must(git(&home, &work, &["add", "."]), "add")?;
    must(git(&home, &work, &["commit", "-qm", "scan"]), "commit")?;
    // The owner moves the trunk (a developer may only open diffs to it).
    must(git(&home, &work, &["push", "-q", &url(&s.admin), "HEAD:main"]), "the workflow")?;
    let deadline = Instant::now() + Duration::from_secs(240);
    loop {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/app&limit=5", &s.admin, None)?;
        let v: Value = serde_json::from_str(&body).unwrap_or_default();
        let run = v.get("runs").and_then(Value::as_array).and_then(|r| r.first()).cloned();
        if let Some(r) = run.filter(|r| r.get("finished_at").is_some_and(|f| !f.is_null())) {
            let id = text(&r, "id").to_owned();
            let (_, detail) = http(s.port, "GET", &format!("/api/ci/runs/{id}"), &s.admin, None)?;
            let leg = serde_json::from_str::<Value>(&detail)?.pointer("/legs/0/id").and_then(Value::as_str).unwrap_or_default().to_owned();
            let mut log = String::new();
            for step in 0..3 {
                let (_, text) = http(s.port, "GET", &format!("/api/ci/legs/{leg}/logs?step={step}"), &s.admin, None)?;
                log.push_str(&text);
            }
            assert_eq!(text(&r, "conclusion"), "success", "{r}\n{log}\n{}", s.log());
            assert!(log.contains("scanned"), "{log}");
            return Ok(());
        }
        anyhow::ensure!(Instant::now() < deadline, "no run: {body}\n{}", s.log());
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Pull requests a job opens and labels (docs/ci.md W14-R11), as `create-pull-request` and
/// `actions/labeler` do: a branch pushed with the job's token, `POST /pulls` (one open per
/// branch), labels added, set and removed, `PATCH` for its title, description and state.
#[test]
fn w14_r11_pull_requests_jobs_open_and_label() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { agent: true, ..Options::default() })?;
    let home = dir.path().join("githome");
    std::fs::create_dir_all(&home)?;
    let url = |token: &str| format!("http://x:{token}@127.0.0.1:{}/git/acme/app.git", s.port);
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/app", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "native repository: {st} {body}");
    must(git(&home, dir.path(), &["clone", "-q", &url(&s.admin), "work"]), "clone")?;
    let work = dir.path().join("work");
    std::fs::write(work.join("README.md"), "app\n")?;
    must(git(&home, &work, &["add", "."]), "add")?;
    must(git(&home, &work, &["commit", "-qm", "first"]), "commit")?;
    must(git(&home, &work, &["push", "-q", "origin", "main"]), "the first trunk")?;
    until(60, "the first poll", || {
        let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        Ok((!(body.contains("acme/app") && !body.contains("\"last_polled_at\":null"))).then_some(body))
    })?;
    std::fs::create_dir_all(work.join(".github/workflows"))?;
    std::fs::write(
        work.join(".github/workflows/pr.yml"),
        r#"on: [push, pull_request]
permissions:
  contents: write
  pull-requests: write
  issues: write
jobs:
  open:
    runs-on: local
    steps:
      - uses: kiln/checkout@v1
      - env:
          TOKEN: ${{ github.token }}
        run: |
          set -e
          api() { curl -sf -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" "$@"; }
          code() { curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" "$@"; }
          R="$GITHUB_API_URL/repos/$GITHUB_REPOSITORY"
          # A branch, pushed with the job's own token, as create-pull-request pushes one.
          git checkout -q -b feature
          echo change > change.txt
          git add change.txt
          git -c user.name=bot -c user.email=bot@example.com commit -qm "a change"
          git -c http.extraheader="AUTHORIZATION: basic $(printf 'x-access-token:%s' "$TOKEN" | base64 -w0)" push -q "$GITHUB_SERVER_URL/$GITHUB_REPOSITORY.git" HEAD:refs/heads/feature
          pr=$(api -X POST "$R/pulls" -d '{"title":"A change","head":"acme:feature","base":"main","body":"Made by a job."}')
          n=$(echo "$pr" | sed -n 's/.*"number":\([0-9]*\).*/\1/p' | head -1)
          echo "$pr" | grep -q '"state":"open"'
          echo "$pr" | grep -q '"body":"Made by a job."'
          test "$(code -X POST "$R/pulls" -d '{"title":"again","head":"feature","base":"main"}')" = 422
          api "$R/pulls?state=open&head=acme:feature" | grep -q '"title":"A change"'
          # Labels: added, set, removed.
          api -X POST "$R/issues/$n/labels" -d '{"labels":["bug","ci"]}' | grep -q '"name":"ci"'
          api "$R/pulls/$n" | grep -q '"name":"bug"'
          test "$(api -X PUT "$R/issues/$n/labels" -d '["docs"]' | grep -o '"name"' | wc -l)" = 1
          api -X DELETE "$R/issues/$n/labels/docs" | grep -q '^\[\]$'
          test "$(code -X DELETE "$R/issues/$n/labels/docs")" = 404
          # Its title, description and state.
          api -X PATCH "$R/pulls/$n" -d '{"title":"Renamed","body":"Said again."}' | grep -q '"title":"Renamed"'
          api "$R/issues/$n" | grep -q '"body":"Said again."'
          api -X PATCH "$R/pulls/$n" -d '{"state":"closed"}' | grep -q '"state":"closed"'
          echo "opened and labeled $n"
"#,
    )?;
    must(git(&home, &work, &["add", "."]), "add")?;
    must(git(&home, &work, &["commit", "-qm", "pr"]), "commit")?;
    must(git(&home, &work, &["push", "-q", &url(&s.admin), "HEAD:main"]), "the workflow")?;
    let deadline = Instant::now() + Duration::from_secs(240);
    loop {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/app&limit=5", &s.admin, None)?;
        let v: Value = serde_json::from_str(&body).unwrap_or_default();
        let run = v.get("runs").and_then(Value::as_array).and_then(|r| r.iter().find(|r| text(r, "workflow").ends_with("pr.yml") && text(r, "branch") == "main")).cloned();
        if let Some(r) = run.filter(|r| r.get("finished_at").is_some_and(|f| !f.is_null())) {
            let id = text(&r, "id").to_owned();
            let (_, detail) = http(s.port, "GET", &format!("/api/ci/runs/{id}"), &s.admin, None)?;
            let leg = serde_json::from_str::<Value>(&detail)?.pointer("/legs/0/id").and_then(Value::as_str).unwrap_or_default().to_owned();
            let mut log = String::new();
            for step in 0..3 {
                let (_, text) = http(s.port, "GET", &format!("/api/ci/legs/{leg}/logs?step={step}"), &s.admin, None)?;
                log.push_str(&text);
            }
            assert_eq!(text(&r, "conclusion"), "success", "{r}\n{log}\n{}", s.log());
            assert!(log.contains("opened and labeled"), "{log}");
            // The change request is kiln's own, with the job's description and labels gone.
            let (_, crs) = http(s.port, "GET", "/api/repos/acme%2Fapp/change-requests", &s.admin, None)?;
            assert!(crs.contains("Renamed") && crs.contains("abandoned"), "{crs}");
            // What the job's token did (its push of `feature`, its pull request) started no run, as
            // on GitHub: the owner's push is the only one.
            std::thread::sleep(Duration::from_secs(5));
            let (_, all) = http(s.port, "GET", "/api/ci/runs?repo=acme/app&limit=20", &s.admin, None)?;
            let n = serde_json::from_str::<Value>(&all)?.get("runs").and_then(Value::as_array).map_or(0, Vec::len);
            assert_eq!(n, 1, "the job's token started runs: {all}");
            return Ok(());
        }
        anyhow::ensure!(Instant::now() < deadline, "no run: {body}\n{}", s.log());
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// `pull_request_target` from a change request on a native repository (docs/ci.md W03-R08, A.6):
/// the trunk's own workflow runs, at the trunk's tip, given the pull request; what the branch
/// changed in that workflow doesn't run.
#[test]
fn w03_r08_native_pull_request_target() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { agent: true, ..Options::default() })?;
    let home = dir.path().join("githome");
    std::fs::create_dir_all(&home)?;
    let url = |token: &str| format!("http://x:{token}@127.0.0.1:{}/git/acme/app.git", s.port);
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/app", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "native repository: {st} {body}");
    must(git(&home, dir.path(), &["clone", "-q", &url(&s.admin), "work"]), "clone")?;
    let work = dir.path().join("work");
    std::fs::write(work.join("README.md"), "app\n")?;
    must(git(&home, &work, &["add", "."]), "add")?;
    must(git(&home, &work, &["commit", "-qm", "first"]), "commit")?;
    must(git(&home, &work, &["push", "-q", "origin", "main"]), "the first trunk")?;
    until(60, "the first poll", || {
        let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        Ok((!(body.contains("acme/app") && !body.contains("\"last_polled_at\":null"))).then_some(body))
    })?;
    let workflow = |from: &str| format!("on: pull_request_target\njobs:\n  t:\n    runs-on: local\n    steps:\n      - run: echo \"target from {from} ref=$GITHUB_REF sha=$GITHUB_SHA pr=${{{{ github.event.pull_request.number }}}} head=${{{{ github.event.pull_request.head.ref }}}}\"\n");
    std::fs::create_dir_all(work.join(".github/workflows"))?;
    std::fs::write(work.join(".github/workflows/target.yml"), workflow("trunk"))?;
    must(git(&home, &work, &["add", "."]), "add")?;
    must(git(&home, &work, &["commit", "-qm", "target"]), "commit")?;
    must(git(&home, &work, &["push", "-q", "origin", "main"]), "the workflow")?;
    let tip = must(git(&home, &work, &["rev-parse", "HEAD"]), "tip")?.trim().to_owned();
    // A branch that changes the workflow, and a change request for it.
    must(git(&home, &work, &["checkout", "-q", "-b", "feature"]), "branch")?;
    std::fs::write(work.join(".github/workflows/target.yml"), workflow("branch"))?;
    std::fs::write(work.join("change.txt"), "change\n")?;
    must(git(&home, &work, &["add", "."]), "add")?;
    must(git(&home, &work, &["commit", "-qm", "a change"]), "commit")?;
    must(git(&home, &work, &["push", "-q", "origin", "feature"]), "the branch")?;
    let (st, cr) = http(s.port, "POST", "/api/repos/acme%2Fapp/change-requests", &s.admin, Some(&json!({ "branch": "feature" })))?;
    anyhow::ensure!(st == 200, "change request: {st} {cr}");
    let number = serde_json::from_str::<Value>(&cr)?.get("change_request").and_then(Value::as_str).unwrap_or_default().trim_start_matches("CR").to_owned();
    let deadline = Instant::now() + Duration::from_secs(240);
    loop {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/app&limit=10", &s.admin, None)?;
        let v: Value = serde_json::from_str(&body).unwrap_or_default();
        let run = v.get("runs").and_then(Value::as_array).and_then(|r| r.iter().find(|r| text(r, "event") == "pull_request_target")).cloned();
        if let Some(r) = run.filter(|r| r.get("finished_at").is_some_and(|f| !f.is_null())) {
            let id = text(&r, "id").to_owned();
            let (_, detail) = http(s.port, "GET", &format!("/api/ci/runs/{id}"), &s.admin, None)?;
            let leg = serde_json::from_str::<Value>(&detail)?.pointer("/legs/0/id").and_then(Value::as_str).unwrap_or_default().to_owned();
            let (_, log) = http(s.port, "GET", &format!("/api/ci/legs/{leg}/logs?step=0"), &s.admin, None)?;
            assert_eq!(text(&r, "conclusion"), "success", "{r}\n{log}\n{}", s.log());
            assert!(log.contains(&format!("target from trunk ref=refs/heads/main sha={tip} pr={number} head=feature")), "{log}");
            assert!(!log.contains("target from branch"), "the branch's workflow never runs this way: {log}");
            return Ok(());
        }
        anyhow::ensure!(Instant::now() < deadline, "no pull_request_target run: {body}\n{}", s.log());
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// The facade over HTTPS (docs/ci.md §2.6): a server whose address is `https://`, its certificate
/// the kiln CA's. A job's tools trust it beside the machine's roots: curl to the API, git to the
/// repository, and Node's `fetch`, as `create-pull-request` and `actions-gh-pages` reach GitHub
/// Enterprise Server's `https://` API and git.
#[test]
fn w14_r11_facade_over_https() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = common::start_with(dir.path(), &Options { agent: true, ..Options::default() }, true)?;
    let home = dir.path().join("githome");
    std::fs::create_dir_all(&home)?;
    let url = |token: &str| format!("http://x:{token}@127.0.0.1:{}/git/acme/app.git", s.port);
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/app", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "native repository: {st} {body}");
    must(git(&home, dir.path(), &["clone", "-q", &url(&s.admin), "work"]), "clone")?;
    let work = dir.path().join("work");
    std::fs::write(work.join("README.md"), "app\n")?;
    must(git(&home, &work, &["add", "."]), "add")?;
    must(git(&home, &work, &["commit", "-qm", "first"]), "commit")?;
    must(git(&home, &work, &["push", "-q", "origin", "main"]), "the first trunk")?;
    until(60, "the first poll", || {
        let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        Ok((!(body.contains("acme/app") && !body.contains("\"last_polled_at\":null"))).then_some(body))
    })?;
    std::fs::create_dir_all(work.join(".github/workflows"))?;
    std::fs::write(
        work.join(".github/workflows/tls.yml"),
        r#"on: push
jobs:
  tls:
    runs-on: local
    steps:
      - env:
          TOKEN: ${{ github.token }}
        run: |
          set -e
          # GitHub Enterprise Server's clients go https:// to the host they're given, whatever
          # its scheme, as create-pull-request does.
          host="${GITHUB_SERVER_URL#*://}"
          curl -sf -H "Authorization: Bearer $TOKEN" "https://$host/api/v3/repos/$GITHUB_REPOSITORY" | grep -q '"full_name"'
          git -c http.extraheader="AUTHORIZATION: basic $(printf 'x-access-token:%s' "$TOKEN" | base64 -w0)" ls-remote "https://$host/$GITHUB_REPOSITORY.git" HEAD | grep -q HEAD
          if command -v node > /dev/null; then
            node -e 'fetch("https://" + process.env.GITHUB_SERVER_URL.split("://")[1] + "/api/v3/meta", { headers: { authorization: "Bearer " + process.env.TOKEN } }).then(r => { if (!r.ok) process.exit(1); console.log("node fetched") }, e => { console.error(e.cause || e); process.exit(1) })'
          fi
          echo "tls trusted"
"#,
    )?;
    must(git(&home, &work, &["add", "."]), "add")?;
    must(git(&home, &work, &["commit", "-qm", "tls"]), "commit")?;
    must(git(&home, &work, &["push", "-q", "origin", "HEAD:main"]), "the workflow")?;
    let deadline = Instant::now() + Duration::from_secs(240);
    loop {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/app&limit=5", &s.admin, None)?;
        let v: Value = serde_json::from_str(&body).unwrap_or_default();
        let run = v.get("runs").and_then(Value::as_array).and_then(|r| r.first()).cloned();
        if let Some(r) = run.filter(|r| r.get("finished_at").is_some_and(|f| !f.is_null())) {
            let id = text(&r, "id").to_owned();
            let (_, detail) = http(s.port, "GET", &format!("/api/ci/runs/{id}"), &s.admin, None)?;
            let leg = serde_json::from_str::<Value>(&detail)?.pointer("/legs/0/id").and_then(Value::as_str).unwrap_or_default().to_owned();
            let (_, log) = http(s.port, "GET", &format!("/api/ci/legs/{leg}/logs?step=0"), &s.admin, None)?;
            assert_eq!(text(&r, "conclusion"), "success", "{r}\n{log}\n{}", s.log());
            assert!(log.contains("tls trusted"), "{log}");
            return Ok(());
        }
        anyhow::ensure!(Instant::now() < deadline, "no run: {body}\n{}", s.log());
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// A change request's conversation raises workflow events on a native repository (docs/ci.md
/// W03-R09, A.6): a comment on it as a whole `issue_comment` (from the default branch), one on its
/// lines `pull_request_review_comment`, a review `pull_request_review`, and kiln's `comment` with
/// its `commands` filter.
#[test]
fn w03_r09_native_conversation_events() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    // GitHub's workflows run beside the `.kiln/` one.
    let s = start(dir.path(), &Options { agent: true, ci: "kiln_workflows = true\ngithub_workflows = \"on\"", ..Options::default() })?;
    let home = dir.path().join("githome");
    std::fs::create_dir_all(&home)?;
    let url = |token: &str| format!("http://x:{token}@127.0.0.1:{}/git/acme/app.git", s.port);
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/app", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "native repository: {st} {body}");
    let dev = person(&s, "dev", &["developer@repo:acme/app"])?;
    must(git(&home, dir.path(), &["clone", "-q", &url(&s.admin), "work"]), "clone")?;
    let work = dir.path().join("work");
    std::fs::write(work.join("README.md"), "app\n")?;
    must(git(&home, &work, &["add", "."]), "add")?;
    must(git(&home, &work, &["commit", "-qm", "first"]), "commit")?;
    must(git(&home, &work, &["push", "-q", "origin", "main"]), "the first trunk")?;
    until(60, "the first poll", || {
        let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        Ok((!(body.contains("acme/app") && !body.contains("\"last_polled_at\":null"))).then_some(body))
    })?;
    let job = |on: &str, say: &str| format!("on: {on}\njobs:\n  j:\n    runs-on: local\n    steps:\n      - run: echo \"{say}\"\n");
    std::fs::create_dir_all(work.join(".github/workflows"))?;
    std::fs::create_dir_all(work.join(".kiln"))?;
    std::fs::write(work.join(".github/workflows/comment.yml"), job("issue_comment", "issue_comment ${{ github.event.comment.body }} on ${{ github.event.issue.number }} by ${{ github.event.sender.login }} ref=${{ github.ref }}"))?;
    std::fs::write(work.join(".github/workflows/line.yml"), job("pull_request_review_comment", "review comment on ${{ github.event.comment.path }}:${{ github.event.comment.line }}"))?;
    std::fs::write(work.join(".github/workflows/review.yml"), job("pull_request_review", "review ${{ github.event.review.state }} by ${{ github.event.review.user.login }}"))?;
    std::fs::write(work.join(".kiln/ship.yaml"), "on:\n  comment:\n    commands: [\"/deploy\"]\njobs:\n  d:\n    runs-on: local\n    steps:\n      - run: echo \"deploy asked, mentioning ${{ join(github.event.kiln.mentions, ',') }}\"\n")?;
    must(git(&home, &work, &["add", "."]), "add")?;
    must(git(&home, &work, &["commit", "-qm", "conversation workflows"]), "commit")?;
    must(git(&home, &work, &["push", "-q", "origin", "main"]), "the workflows")?;
    must(git(&home, &work, &["checkout", "-q", "-b", "feature"]), "branch")?;
    std::fs::write(work.join("change.txt"), "one\ntwo\n")?;
    must(git(&home, &work, &["add", "."]), "add")?;
    must(git(&home, &work, &["commit", "-qm", "a change"]), "commit")?;
    must(git(&home, &work, &["push", "-q", "origin", "feature"]), "the branch")?;
    let (st, cr) = http(s.port, "POST", "/api/repos/acme%2Fapp/change-requests", &s.admin, Some(&json!({ "branch": "feature" })))?;
    anyhow::ensure!(st == 200, "change request: {st} {cr}");
    let name = serde_json::from_str::<Value>(&cr)?.get("change_request").and_then(Value::as_str).unwrap_or_default().to_owned();
    let number = name.trim_start_matches("CR").to_owned();
    // The developer talks: a command, a plain remark, a line comment, an approval.
    for (path, b) in [
        ("comments", json!({ "body": "/deploy please @ada", "publish": true })),
        ("comments", json!({ "body": "looks fine", "publish": true })),
        ("comments", json!({ "body": "here", "file": "change.txt", "start_line": 2, "publish": true })),
        ("review", json!({ "action": "accept", "body": "ship it" })),
    ] {
        let (st, out) = http(s.port, "POST", &format!("/api/diffs/{name}/{path}"), &dev, Some(&b))?;
        anyhow::ensure!(st == 200, "{path}: {st} {out}");
    }
    let wanted = [
        ("issue_comment", format!("issue_comment /deploy please @ada on {number} by dev ref=refs/heads/main")),
        ("issue_comment", format!("issue_comment looks fine on {number} by dev")),
        ("pull_request_review_comment", "review comment on change.txt:2".to_owned()),
        ("pull_request_review", "review approved by dev".to_owned()),
        ("comment", "deploy asked, mentioning ada".to_owned()),
    ];
    let deadline = Instant::now() + Duration::from_secs(240);
    loop {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/app&limit=50", &s.admin, None)?;
        let runs: Vec<Value> = serde_json::from_str::<Value>(&body)?.get("runs").and_then(Value::as_array).cloned().unwrap_or_default();
        let mut logs: Vec<(String, String)> = Vec::new();
        for r in runs.iter().filter(|r| r.get("finished_at").is_some_and(|f| !f.is_null())) {
            let id = text(r, "id").to_owned();
            let (_, detail) = http(s.port, "GET", &format!("/api/ci/runs/{id}"), &s.admin, None)?;
            let leg = serde_json::from_str::<Value>(&detail)?.pointer("/legs/0/id").and_then(Value::as_str).unwrap_or_default().to_owned();
            let (_, log) = http(s.port, "GET", &format!("/api/ci/legs/{leg}/logs?step=0"), &s.admin, None)?;
            logs.push((text(r, "event").to_owned(), log));
        }
        if wanted.iter().all(|(event, said)| logs.iter().any(|(e, l)| e == event && l.contains(said.as_str()))) {
            // kiln's `comment` ran for the command alone, not the plain remark.
            assert_eq!(logs.iter().filter(|(e, _)| e == "comment").count(), 1, "{logs:?}");
            return Ok(());
        }
        anyhow::ensure!(Instant::now() < deadline, "not every conversation event ran: {logs:?}\n{}", s.log());
        std::thread::sleep(Duration::from_millis(700));
    }
}

/// The first branch pushed to an empty repository is its default, as on GitHub (W14-R01): one made
/// on `main` takes `master` when that's what comes first, and `refs/for/master` is a trunk's then.
/// A push refused while its pack is still coming says why: git hears the refusal, not a broken
/// connection.
#[test]
fn w14_r01_first_branch_and_refusals_said() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options::default())?;
    let home = dir.path().join("githome");
    std::fs::create_dir_all(&home)?;
    let url = format!("http://x:{}@127.0.0.1:{}/git/acme/first.git", s.admin, s.port);
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/first", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "native repository: {st} {body}");
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work)?;
    must(git(&home, &work, &["init", "-q", "-b", "master"]), "init")?;
    std::fs::write(work.join("README.md"), "first\n")?;
    must(git(&home, &work, &["add", "."]), "add")?;
    must(git(&home, &work, &["commit", "-qm", "first"]), "commit")?;
    let said = must(git(&home, &work, &["push", &url, "master"]), "the first branch")?;
    assert!(said.contains("master is the repository's default branch"), "{said}");
    let (_, repos) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
    let repos: Value = serde_json::from_str(&repos)?;
    let first = repos.as_array().and_then(|r| r.iter().find(|x| x.get("name").and_then(Value::as_str) == Some("acme/first"))).cloned().unwrap_or_default();
    assert_eq!(first.get("default_branch").and_then(Value::as_str), Some("master"), "{first}");
    // A pack of a few MiB (incompressible), pushed somewhere refused.
    let noise: Vec<u8> = (0..6u32 << 20).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
    std::fs::write(work.join("noise.bin"), noise)?;
    must(git(&home, &work, &["add", "."]), "add")?;
    must(git(&home, &work, &["commit", "-qm", "noise"]), "commit")?;
    let (ok, out) = git(&home, &work, &["push", &url, "HEAD:refs/for/nowhere"])?;
    assert!(!ok, "refused: {out}");
    assert!(out.contains("isn't a trunk") && !out.contains("RPC failed"), "git hears why: {out}");
    // And a trunk's review takes it.
    must(git(&home, &work, &["commit", "-q", "--amend", "-m", "noise\n\nChange-Id: I0123456789abcdef0123456789abcdef01234567"]), "amend")?;
    let out = must(git(&home, &work, &["push", &url, "HEAD:refs/for/master"]), "a submission to master")?;
    assert!(out.contains("D1"), "{out}");
    Ok(())
}

/// W14-R01: a public native repository is anyone's to read over git, as a public GitHub
/// repository is (a step's `git fetch` after `actions/checkout`'s `persist-credentials: false`,
/// or with a token that may not read it); pushing still needs a token that may, a token that
/// isn't kiln's is refused, and a private repository stays private.
#[test]
fn w14_r01_public_repositories_read_without_a_token() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options::default())?;
    let home = dir.path().join("githome");
    std::fs::create_dir_all(&home)?;
    for (name, public) in [("acme/open", true), ("acme/closed", false)] {
        let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": name, "mode": "native", "public": public })))?;
        anyhow::ensure!(st == 200, "{name}: {st} {body}");
        assert_eq!(serde_json::from_str::<Value>(&body)?.get("public").and_then(Value::as_bool), Some(public), "{body}");
    }
    let (_, repos) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
    let repos: Value = serde_json::from_str(&repos)?;
    let listed = |name: &str| repos.as_array().and_then(|r| r.iter().find(|x| x.get("name").and_then(Value::as_str) == Some(name))).and_then(|x| x.get("public")).and_then(Value::as_bool);
    assert_eq!((listed("acme/open"), listed("acme/closed")), (Some(true), Some(false)), "{repos}");
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work)?;
    must(git(&home, &work, &["init", "-q", "-b", "main"]), "init")?;
    std::fs::write(work.join("README.md"), "open\n")?;
    must(git(&home, &work, &["add", "."]), "add")?;
    must(git(&home, &work, &["commit", "-qm", "open"]), "commit")?;
    for name in ["acme/open", "acme/closed"] {
        must(git(&home, &work, &["push", &format!("http://x:{}@127.0.0.1:{}/git/{name}.git", s.admin, s.port), "main"]), "push")?;
    }
    let anonymous = |name: &str| format!("http://127.0.0.1:{}/git/{name}.git", s.port);
    // Read without a token: cloned, then fetched.
    let clone = dir.path().join("clone");
    must(git(&home, dir.path(), &["clone", "-q", &anonymous("acme/open"), clone.to_string_lossy().as_ref()]), "an anonymous clone of a public repository")?;
    assert_eq!(std::fs::read_to_string(clone.join("README.md"))?, "open\n");
    must(git(&home, &clone, &["fetch", "--tags", "-q"]), "an anonymous fetch")?;
    // Not pushed without one.
    std::fs::write(clone.join("README.md"), "changed\n")?;
    must(git(&home, &clone, &["commit", "-qam", "changed"]), "commit")?;
    let (pushed, out) = git(&home, &clone, &["-c", "credential.helper=", "push", "-q", "origin", "main"])?;
    assert!(!pushed, "an anonymous push is refused: {out}");
    // A token that isn't kiln's is refused, public or not (sent as checkout's extraheader sends
    // its own: a URL's credentials go only after a 401).
    let bad = format!("http.extraHeader=Authorization: Basic {}", base64::Engine::encode(&base64::engine::general_purpose::STANDARD, "x:not-a-token"));
    let (read, out) = git(&home, dir.path(), &["-c", &bad, "ls-remote", &anonymous("acme/open")])?;
    assert!(!read, "a wrong token reads nothing: {out}");
    // A private repository stays private.
    let (read, out) = git(&home, dir.path(), &["-c", "credential.helper=", "ls-remote", &anonymous("acme/closed")])?;
    assert!(!read, "a private repository needs a token: {out}");
    // A token of kiln's that may not read them (as a job's whose `permissions` leave `contents`
    // out) reads the public one as anyone does, and pushes nothing; the private one it can't read.
    let (st, body) = http(s.port, "POST", "/api/users", &s.admin, Some(&json!({ "name": "outsider", "grants": [] })))?;
    anyhow::ensure!((200..300).contains(&st), "{st} {body}");
    let (st, body) = http(s.port, "POST", "/api/tokens", &s.admin, Some(&json!({ "kind": "pat", "user": "outsider", "grants": ["owner"] })))?;
    anyhow::ensure!((200..300).contains(&st), "{st} {body}");
    let token = serde_json::from_str::<Value>(&body)?.get("token").and_then(Value::as_str).unwrap_or_default().to_owned();
    let theirs = format!("http.extraHeader=Authorization: Basic {}", base64::Engine::encode(&base64::engine::general_purpose::STANDARD, format!("x:{token}")));
    let (read, out) = git(&home, dir.path(), &["-c", &theirs, "ls-remote", &anonymous("acme/open")])?;
    assert!(read, "a token that may not read a public repository reads it as anyone does: {out}");
    let (read, out) = git(&home, dir.path(), &["-c", &theirs, "ls-remote", &anonymous("acme/closed")])?;
    assert!(!read, "nor the private one: {out}");
    let (pushed, out) = git(&home, &clone, &["-c", &theirs, "-c", "credential.helper=", "push", "-q", &anonymous("acme/open"), "main"])?;
    assert!(!pushed, "and pushes nothing: {out}");
    Ok(())
}

/// W10-R04: a job whose `permissions` grant nothing reads its public repository's public data
/// through the REST facade, as any token reads it on GitHub (CodeQL's `actions: read` is "only
/// required for workflows in private repositories"), but not its code scanning alerts, which
/// aren't public. A private repository's job reads nothing it isn't granted.
#[test]
fn w10_r04_a_public_repositorys_reads_need_no_permission() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let home = dir.path().join("githome");
    std::fs::create_dir_all(&home)?;
    let s = start(dir.path(), &Options { agent: true, ci: "kiln_workflows = true", ..Options::default() })?;
    let probe = "on: push\npermissions: {}\njobs:\n  read:\n    runs-on: local\n    checkout: false\n    steps:\n      - env:\n          TOKEN: ${{ github.token }}\n        run: |\n          code() { curl -s -o /dev/null -w '%{http_code}' -H \"Authorization: Bearer $TOKEN\" \"$GITHUB_API_URL/repos/$GITHUB_REPOSITORY/$1\"; }\n          echo \"runs=$(code actions/runs) commit=$(code commits/main) alerts=$(code code-scanning/alerts)\"\n";
    let mut said = Vec::new();
    for (name, public) in [("acme/open", true), ("acme/closed", false)] {
        let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": name, "mode": "native", "public": public })))?;
        anyhow::ensure!(st == 200, "{name}: {st} {body}");
        let work = dir.path().join(name.replace('/', "_"));
        std::fs::create_dir_all(&work)?;
        must(git(&home, &work, &["init", "-q", "-b", "main"]), "init")?;
        std::fs::write(work.join("README.md"), "probe\n")?;
        must(git(&home, &work, &["add", "."]), "add")?;
        must(git(&home, &work, &["commit", "-qm", "first"]), "commit")?;
        let url = format!("http://x:{}@127.0.0.1:{}/git/{name}.git", s.admin, s.port);
        must(git(&home, &work, &["push", "-q", &url, "main"]), "push")?;
        until(60, "the first poll", || {
            let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
            Ok(body.contains("\"last_polled_at\":null").then_some(body))
        })?;
        std::fs::create_dir_all(work.join(".kiln"))?;
        std::fs::write(work.join(".kiln/probe.yaml"), probe)?;
        must(git(&home, &work, &["add", "."]), "add")?;
        must(git(&home, &work, &["commit", "-qm", "probe"]), "commit")?;
        must(git(&home, &work, &["push", "-q", &url, "main"]), "push")?;
        let mut run = Value::Null;
        until(180, "the run", || {
            let (_, body) = http(s.port, "GET", &format!("/api/ci/runs?repo={name}&limit=5"), &s.admin, None)?;
            run = serde_json::from_str::<Value>(&body)?.pointer("/runs/0").cloned().unwrap_or_default();
            Ok(run.get("finished_at").is_none_or(Value::is_null).then(|| body.clone()))
        })?;
        let id = run.get("id").and_then(Value::as_str).unwrap_or_default();
        let (_, detail) = http(s.port, "GET", &format!("/api/ci/runs/{id}"), &s.admin, None)?;
        let leg = serde_json::from_str::<Value>(&detail)?.pointer("/legs/0/id").and_then(Value::as_str).unwrap_or_default().to_owned();
        let (_, log) = http(s.port, "GET", &format!("/api/ci/legs/{leg}/logs?step=0"), &s.admin, None)?;
        said.push((name, log));
    }
    let of = |name: &str| said.iter().find(|(n, _)| *n == name).map(|(_, l)| l.clone()).unwrap_or_default();
    assert!(of("acme/open").contains("runs=200 commit=200 alerts=403"), "{}\n{}", of("acme/open"), s.log());
    assert!(of("acme/closed").contains("runs=403 commit=403 alerts=403"), "{}", of("acme/closed"));
    Ok(())
}

/// W14-R11: a leg fetches a repository kiln doesn't have (`actions/checkout`'s `repository:`) from
/// kiln's code host as it would from GitHub's: passed on to where kiln takes that owner's actions
/// (`[[ci.actions.sources]]`, else GitHub), with no token, a public repository there. Here that
/// is another kiln's public repository; one it hasn't got isn't found.
#[test]
fn w14_r11_a_leg_fetches_another_public_repository() -> anyhow::Result<()> {
    let (dir_a, dir) = (tempfile::tempdir()?, tempfile::tempdir()?);
    let home = dir.path().join("githome");
    std::fs::create_dir_all(&home)?;
    // Elsewhere: a public repository on another kiln.
    let a = start(dir_a.path(), &Options::default())?;
    let (st, body) = http(a.port, "POST", "/api/repos", &a.admin, Some(&json!({ "name": "up/tools", "mode": "native", "public": true })))?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let tools = dir.path().join("tools");
    std::fs::create_dir_all(&tools)?;
    must(git(&home, &tools, &["init", "-q", "-b", "main"]), "init")?;
    std::fs::write(tools.join("TOOLS.md"), "the tools of another\n")?;
    must(git(&home, &tools, &["add", "."]), "add")?;
    must(git(&home, &tools, &["commit", "-qm", "tools"]), "commit")?;
    must(git(&home, &tools, &["push", "-q", &format!("http://x:{}@127.0.0.1:{}/git/up/tools.git", a.admin, a.port), "main"]), "push")?;
    // Here: `ext`'s repositories are that kiln's.
    let sources = format!("[[ci.actions.sources]]\nowners = [\"ext\"]\nurl = \"http://127.0.0.1:{}/git/up/{{repo}}.git\"\n", a.port);
    let s = start(dir.path(), &Options { agent: true, ci: "kiln_workflows = true", extra: &sources })?;
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/app", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work)?;
    must(git(&home, &work, &["init", "-q", "-b", "main"]), "init")?;
    std::fs::write(work.join("README.md"), "app\n")?;
    must(git(&home, &work, &["add", "."]), "add")?;
    must(git(&home, &work, &["commit", "-qm", "first"]), "commit")?;
    let url = format!("http://x:{}@127.0.0.1:{}/git/acme/app.git", s.admin, s.port);
    must(git(&home, &work, &["push", "-q", &url, "main"]), "push")?;
    until(60, "the first poll", || {
        let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        Ok(body.contains("\"last_polled_at\":null").then_some(body))
    })?;
    // As `actions/checkout` sends the job's token (`github.token`, no variable of a step's own).
    let fetch = "auth=$(printf 'x-access-token:%s' \"$TOKEN\" | base64 -w0)\n\
        git -c http.extraheader=\"AUTHORIZATION: basic $auth\" clone -q --depth 1 \"$GITHUB_SERVER_URL/ext/tools\" tools\n\
        cat tools/TOOLS.md\n\
        if git -c http.extraheader=\"AUTHORIZATION: basic $auth\" clone -q \"$GITHUB_SERVER_URL/ext/missing\" missing; then exit 1; fi\n";
    std::fs::create_dir_all(work.join(".kiln"))?;
    let workflow = String::from("on: push\njobs:\n  other:\n    runs-on: local\n    checkout: false\n    steps:\n      - env:\n          TOKEN: ${{ github.token }}\n        run: |\n");
    std::fs::write(work.join(".kiln/other.yaml"), workflow + &fetch.lines().map(|l| format!("          {}\n", l.trim_start())).collect::<String>())?;
    must(git(&home, &work, &["add", "."]), "add")?;
    must(git(&home, &work, &["commit", "-qm", "other"]), "commit")?;
    must(git(&home, &work, &["push", "-q", &url, "main"]), "push")?;
    let mut run = Value::Null;
    until(180, "the run", || {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/app&limit=5", &s.admin, None)?;
        run = serde_json::from_str::<Value>(&body)?.pointer("/runs/0").cloned().unwrap_or_default();
        Ok(run.get("finished_at").is_none_or(Value::is_null).then(|| body.clone()))
    })?;
    let id = run.get("id").and_then(Value::as_str).unwrap_or_default();
    let (_, detail) = http(s.port, "GET", &format!("/api/ci/runs/{id}"), &s.admin, None)?;
    let leg = serde_json::from_str::<Value>(&detail)?.pointer("/legs/0/id").and_then(Value::as_str).unwrap_or_default().to_owned();
    let mut log = String::new();
    for step in 0..3 {
        let (_, text) = http(s.port, "GET", &format!("/api/ci/legs/{leg}/logs?step={step}"), &s.admin, None)?;
        log.push_str(&text);
    }
    assert_eq!(run.get("conclusion"), Some(&json!("success")), "{detail}\n{log}\n{}", s.log());
    assert!(log.contains("the tools of another"), "{log}");
    Ok(())
}

/// kiln's other native events (W03-R02, R09): a change request's `pull_request` activity (ready
/// for review, edited, labeled and unlabeled, closed by abandoning, reopened by reclaiming), and
/// kiln's `agent` (an agent asks for a run), `webhook` (a signed call to a hook of the repository)
/// and `pipeline` (another workflow's run finished) events.
#[test]
fn w03_r09_native_activity_and_kiln_events() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { agent: true, ci: "kiln_workflows = true\ngithub_workflows = \"on\"", ..Options::default() })?;
    let home = dir.path().join("githome");
    std::fs::create_dir_all(&home)?;
    let url = |token: &str| format!("http://x:{token}@127.0.0.1:{}/git/acme/app.git", s.port);
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/app", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "native repository: {st} {body}");
    must(git(&home, dir.path(), &["clone", "-q", &url(&s.admin), "work"]), "clone")?;
    let work = dir.path().join("work");
    std::fs::create_dir_all(work.join(".github/workflows"))?;
    std::fs::create_dir_all(work.join(".kiln"))?;
    std::fs::write(
        work.join(".github/workflows/activity.yml"),
        "on:\n  pull_request:\n    types: [ready_for_review, closed, reopened, edited, labeled, unlabeled]\njobs:\n  j:\n    runs-on: local\n    steps:\n      - run: echo \"pr ${{ github.event.action }} label=${{ github.event.label.name }} from=${{ github.event.changes.title.from }} merged=${{ github.event.pull_request.merged }} labels=${{ join(github.event.pull_request.labels.*.name, ',') }}\"\n",
    )?;
    std::fs::write(work.join(".kiln/ask.yaml"), "on:\n  agent:\n    agents: [\"*\"]\n    inputs:\n      what: { type: string, required: true }\njobs:\n  a:\n    runs-on: local\n    steps:\n      - run: echo \"asked by ${{ github.actor }} for ${{ inputs.what }}\"\n")?;
    std::fs::write(work.join(".kiln/picky.yaml"), "on:\n  agent:\n    agents: [triage]\njobs:\n  a:\n    runs-on: local\n    steps:\n      - run: echo picky\n")?;
    std::fs::write(work.join(".kiln/hooked.yaml"), "name: hooked\non:\n  webhook:\n    filter: \"event.env == 'prod'\"\njobs:\n  h:\n    runs-on: local\n    steps:\n      - run: echo \"hooked ${{ github.event.action }} for ${{ github.event.env }}\"\n")?;
    std::fs::write(work.join(".kiln/after.yaml"), "on:\n  pipeline:\n    name: hooked\n    conclusion: [success]\njobs:\n  p:\n    runs-on: local\n    steps:\n      - run: echo \"after ${{ github.event.workflow_run.name }}\"\n")?;
    std::fs::write(work.join("README.md"), "app\n")?;
    must(git(&home, &work, &["add", "."]), "add")?;
    must(git(&home, &work, &["commit", "-qm", "workflows"]), "commit")?;
    must(git(&home, &work, &["push", "-q", "origin", "main"]), "the trunk")?;
    until(60, "the first poll", || {
        let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        Ok((!(body.contains("acme/app") && !body.contains("\"last_polled_at\":null"))).then_some(body))
    })?;
    // A draft change request, then what its author and reviewers do to it.
    must(git(&home, &work, &["checkout", "-q", "-b", "feature"]), "branch")?;
    std::fs::write(work.join("change.txt"), "one\n")?;
    must(git(&home, &work, &["add", "."]), "add")?;
    must(git(&home, &work, &["commit", "-qm", "a change"]), "commit")?;
    must(git(&home, &work, &["push", "-q", "origin", "feature"]), "the branch")?;
    let (st, cr) = http(s.port, "POST", "/api/repos/acme%2Fapp/change-requests", &s.admin, Some(&json!({ "branch": "feature", "draft": true, "title": "First title" })))?;
    anyhow::ensure!(st == 200, "change request: {st} {cr}");
    let name = serde_json::from_str::<Value>(&cr)?.get("change_request").and_then(Value::as_str).unwrap_or_default().to_owned();
    for (method, path, b) in [
        ("POST", "ready", json!({})),
        ("PATCH", "", json!({ "title": "Second title" })),
        ("POST", "labels", json!({ "labels": ["ci:full"] })),
        ("DELETE", "labels/ci%3Afull", json!({})),
        ("POST", "abandon", json!({})),
        ("POST", "reclaim", json!({})),
    ] {
        let at = if path.is_empty() { format!("/api/change-requests/{name}") } else { format!("/api/change-requests/{name}/{path}") };
        let (st, out) = http(s.port, method, &at, &s.admin, Some(&b))?;
        anyhow::ensure!(st == 200, "{method} {at}: {st} {out}");
        // In order: each action's run is made before the next changes the request.
        std::thread::sleep(Duration::from_millis(400));
    }
    // An agent asks; one the workflow doesn't name is refused.
    let (st, out) = http(s.port, "POST", "/api/ci/repos/acme%2Fapp/agent", &s.admin, Some(&json!({ "workflow": "ask.yaml", "inputs": { "what": "a smoke test" } })))?;
    anyhow::ensure!(st == 204, "agent: {st} {out}");
    let (st, out) = http(s.port, "POST", "/api/ci/repos/acme%2Fapp/agent", &s.admin, Some(&json!({ "workflow": "picky.yaml" })))?;
    assert_eq!(st, 403, "an agent picky.yaml doesn't name: {out}");
    // A hook, called signed (twice: one its filter takes), and unsigned.
    let (st, hook) = http(s.port, "POST", "/api/ci/repos/acme%2Fapp/hooks", &s.admin, Some(&json!({ "name": "release-bot" })))?;
    anyhow::ensure!(st == 201, "hook: {st} {hook}");
    let hook: Value = serde_json::from_str(&hook)?;
    let (id, secret) = (text(&hook, "id").to_owned(), text(&hook, "secret").to_owned());
    for (env, signed) in [("prod", true), ("dev", true), ("prod", false)] {
        let body = json!({ "action": "released", "env": env }).to_string();
        let sig = if signed { kiln::util::hmac_hex(secret.as_bytes(), body.as_bytes()) } else { "0".repeat(64) };
        let out = Command::new("curl").args(["-s", "-o", "/dev/null", "-w", "%{http_code}", "-X", "POST", "-H", "Content-Type: application/json", "-H", &format!("X-Kiln-Signature-256: sha256={sig}"), "--data-binary", &body, &format!("http://127.0.0.1:{}/api/ci/hooks/{id}", s.port)]).output()?;
        let code = String::from_utf8_lossy(&out.stdout).to_string();
        assert_eq!(code, if signed { "202" } else { "401" }, "the hook's call ({env}, signed: {signed})");
    }
    let wanted = [
        ("pull_request", "pr ready_for_review label= from= merged="),
        ("pull_request", "pr edited label= from=First title"),
        // `merged` is in every pull request event's payload, as on GitHub.
        ("pull_request", "pr labeled label=ci:full from= merged=false labels=ci:full"),
        ("pull_request", "pr unlabeled label=ci:full"),
        ("pull_request", "pr closed label= from= merged=false"),
        ("pull_request", "pr reopened"),
        ("agent", "asked by admin for a smoke test"),
        ("webhook", "hooked released for prod"),
        ("pipeline", "after hooked"),
    ];
    let deadline = Instant::now() + Duration::from_secs(300);
    loop {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/app&limit=50", &s.admin, None)?;
        let runs: Vec<Value> = serde_json::from_str::<Value>(&body)?.get("runs").and_then(Value::as_array).cloned().unwrap_or_default();
        let mut logs: Vec<(String, String)> = Vec::new();
        for r in runs.iter().filter(|r| r.get("finished_at").is_some_and(|f| !f.is_null())) {
            let id = text(r, "id").to_owned();
            let (_, detail) = http(s.port, "GET", &format!("/api/ci/runs/{id}"), &s.admin, None)?;
            let leg = serde_json::from_str::<Value>(&detail)?.pointer("/legs/0/id").and_then(Value::as_str).unwrap_or_default().to_owned();
            let (_, log) = http(s.port, "GET", &format!("/api/ci/legs/{leg}/logs?step=0"), &s.admin, None)?;
            logs.push((text(r, "event").to_owned(), log));
        }
        if wanted.iter().all(|(event, said)| logs.iter().any(|(e, l)| e == event && l.contains(said))) {
            // The hook's filter took the `prod` call alone; nothing ran for `opened`, which the
            // workflow's `types` leave out.
            assert_eq!(logs.iter().filter(|(e, _)| e == "webhook").count(), 1, "{logs:?}");
            assert!(!logs.iter().any(|(_, l)| l.contains("pr opened")), "{logs:?}");
            assert!(!logs.iter().any(|(_, l)| l.contains("picky")), "{logs:?}");
            return Ok(());
        }
        anyhow::ensure!(Instant::now() < deadline, "not every event ran: {logs:?}\n{}", s.log());
        std::thread::sleep(Duration::from_millis(700));
    }
}

/// W07-R01's `kiln://owner/repo/path@ref`: an action in one of kiln's own repositories, pinned at
/// run start from kiln's own git and run like GitHub's; its owner's repositories' to use, so
/// another owner's workflow fails at the start with why.
#[test]
fn w07_r01_kiln_actions() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { agent: true, ci: "kiln_workflows = true", ..Options::default() })?;
    let home = dir.path().join("githome");
    std::fs::create_dir_all(&home)?;
    let push = |name: &str, files: &[(&str, &str)], tag: Option<&str>| -> anyhow::Result<()> {
        let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": name, "mode": "native" })))?;
        anyhow::ensure!(st == 200, "{name}: {st} {body}");
        let work = dir.path().join(name.replace('/', "_"));
        std::fs::create_dir_all(&work)?;
        must(git(&home, &work, &["init", "-q", "-b", "main"]), "init")?;
        for (path, text) in files {
            let p = work.join(path);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(p, text)?;
        }
        must(git(&home, &work, &["add", "."]), "add")?;
        must(git(&home, &work, &["commit", "-qm", "first"]), "commit")?;
        if let Some(t) = tag {
            must(git(&home, &work, &["tag", t]), "tag")?;
        }
        let url = format!("http://x:{}@127.0.0.1:{}/git/{name}.git", s.admin, s.port);
        must(git(&home, &work, &["push", "-q", &url, "main", "--tags"]), "push")?;
        Ok(())
    };
    push("acme/tools", &[("hello/action.yml", "name: hello\ninputs:\n  who: { required: true }\nruns:\n  using: composite\n  steps:\n    - shell: bash\n      run: echo \"hello from kiln tools to ${{ inputs.who }}\"\n")], Some("v1"))?;
    let using = "on: push\njobs:\n  j:\n    runs-on: local\n    steps:\n      - uses: kiln://acme/tools/hello@v1\n        with:\n          who: ${{ github.repository }}\n";
    // The first push only records refs; the workflow comes with the second.
    for name in ["acme/app", "other/app"] {
        push(name, &[("README.md", "app\n")], None)?;
    }
    until(60, "the first polls", || {
        let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        Ok((body.contains("\"last_polled_at\":null")).then_some(body))
    })?;
    for name in ["acme/app", "other/app"] {
        let work = dir.path().join(name.replace('/', "_"));
        std::fs::create_dir_all(work.join(".kiln"))?;
        std::fs::write(work.join(".kiln/use.yaml"), using)?;
        must(git(&home, &work, &["add", "."]), "add")?;
        must(git(&home, &work, &["commit", "-qm", "use kiln tools"]), "commit")?;
        let url = format!("http://x:{}@127.0.0.1:{}/git/{name}.git", s.admin, s.port);
        must(git(&home, &work, &["push", "-q", &url, "main"]), "push")?;
    }
    let settled = |name: &str| -> anyhow::Result<Option<(String, String)>> {
        let (_, body) = http(s.port, "GET", &format!("/api/ci/runs?repo={name}&limit=5"), &s.admin, None)?;
        let runs: Value = serde_json::from_str(&body)?;
        let Some(r) = runs.pointer("/runs/0").filter(|r| r.get("finished_at").is_some_and(|f| !f.is_null())) else { return Ok(None) };
        let (_, detail) = http(s.port, "GET", &format!("/api/ci/runs/{}", text(r, "id")), &s.admin, None)?;
        let leg = serde_json::from_str::<Value>(&detail)?.pointer("/legs/0/id").and_then(Value::as_str).unwrap_or_default().to_owned();
        let (_, l) = http(s.port, "GET", &format!("/api/ci/legs/{leg}"), &s.admin, None)?;
        let l: Value = serde_json::from_str(&l)?;
        let notes: Vec<String> = l.get("annotations").and_then(Value::as_array).into_iter().flatten().filter_map(|a| a.get("message").and_then(Value::as_str).map(str::to_owned)).collect();
        let mut log = String::new();
        for step in 0..3 {
            let (_, t) = http(s.port, "GET", &format!("/api/ci/legs/{leg}/logs?step={step}"), &s.admin, None)?;
            log.push_str(&t);
        }
        Ok(Some((text(r, "conclusion").to_owned(), format!("{}\n{log}", notes.join("\n")))))
    };
    let deadline = Instant::now() + Duration::from_secs(300);
    let (ours, theirs) = loop {
        if let (Some(a), Some(b)) = (settled("acme/app")?, settled("other/app")?) {
            break (a, b);
        }
        anyhow::ensure!(Instant::now() < deadline, "the runs didn't finish\n{}", s.log());
        std::thread::sleep(Duration::from_millis(500));
    };
    assert_eq!(ours.0, "success", "{}", ours.1);
    assert!(ours.1.contains("hello from kiln tools to acme/app"), "{}", ours.1);
    assert_eq!(theirs.0, "failure", "{}", theirs.1);
    assert!(theirs.1.contains("another owner's repository"), "{}", theirs.1);
    Ok(())
}

/// W07-R10's `kiln/upload@v1` and `kiln/download@v1`: one job's files zipped and uploaded by the
/// runner itself (their common directory the zip's root, hidden files left out), the next job's
/// download of them, and the artifact in the run with its digest.
#[test]
fn w07_r10_kiln_upload_and_download() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { agent: true, ci: "kiln_workflows = true", ..Options::default() })?;
    let home = dir.path().join("githome");
    std::fs::create_dir_all(&home)?;
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/art", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work)?;
    must(git(&home, &work, &["init", "-q", "-b", "main"]), "init")?;
    std::fs::write(work.join("README.md"), "art\n")?;
    must(git(&home, &work, &["add", "."]), "add")?;
    must(git(&home, &work, &["commit", "-qm", "first"]), "commit")?;
    let url = format!("http://x:{}@127.0.0.1:{}/git/acme/art.git", s.admin, s.port);
    must(git(&home, &work, &["push", "-q", &url, "main"]), "push")?;
    until(60, "the first poll", || {
        let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        Ok((body.contains("\"last_polled_at\":null")).then_some(body))
    })?;
    std::fs::create_dir_all(work.join(".kiln"))?;
    std::fs::write(
        work.join(".kiln/art.yaml"),
        r#"on: push
jobs:
  make:
    runs-on: local
    checkout: false
    steps:
      - run: mkdir -p dist/lib && echo one > dist/app.js && echo two > dist/lib/x.js && echo hidden > dist/.secret
      - id: up
        uses: kiln/upload@v1
        with:
          name: dist
          path: dist
      - run: echo "uploaded ${{ steps.up.outputs.artifact-id }} ${{ steps.up.outputs.artifact-digest }}"
  take:
    needs: make
    runs-on: local
    checkout: false
    steps:
      - uses: kiln/download@v1
        with:
          name: dist
          path: got
      - run: |
          cat got/app.js got/lib/x.js
          test ! -e got/.secret && echo "no hidden file"
"#,
    )?;
    must(git(&home, &work, &["add", "."]), "add")?;
    must(git(&home, &work, &["commit", "-qm", "artifacts"]), "commit")?;
    must(git(&home, &work, &["push", "-q", &url, "main"]), "push")?;
    let deadline = Instant::now() + Duration::from_secs(300);
    let (run, legs) = loop {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/art&limit=5", &s.admin, None)?;
        let runs: Value = serde_json::from_str(&body)?;
        if let Some(r) = runs.pointer("/runs/0").filter(|r| r.get("finished_at").is_some_and(|f| !f.is_null()) && text(r, "workflow").ends_with("art.yaml")) {
            let (_, detail) = http(s.port, "GET", &format!("/api/ci/runs/{}", text(r, "id")), &s.admin, None)?;
            break (r.clone(), serde_json::from_str::<Value>(&detail)?.get("legs").and_then(Value::as_array).cloned().unwrap_or_default());
        }
        anyhow::ensure!(Instant::now() < deadline, "the run didn't finish: {body}\n{}", s.log());
        std::thread::sleep(Duration::from_millis(500));
    };
    let log_of = |key: &str| -> anyhow::Result<String> {
        let id = legs.iter().find(|l| text(l, "key") == key).map(|l| text(l, "id").to_owned()).unwrap_or_default();
        let mut all = String::new();
        for step in 0..4 {
            let (_, t) = http(s.port, "GET", &format!("/api/ci/legs/{id}/logs?step={step}"), &s.admin, None)?;
            all.push_str(&t);
        }
        Ok(all)
    };
    assert_eq!(text(&run, "conclusion"), "success", "{}\n{}\n{}", log_of("make")?, log_of("take")?, s.log());
    let take = log_of("take")?;
    assert!(take.contains("one") && take.contains("two") && take.contains("no hidden file"), "{take}");
    let (_, listed) = http(s.port, "GET", &format!("/api/ci/runs/{}/artifacts", text(&run, "id")), &s.admin, None)?;
    let made = log_of("make")?;
    let digest = made.split("uploaded ").nth(1).and_then(|r| r.split_whitespace().nth(1)).unwrap_or_default().to_owned();
    assert!(digest.len() == 64 && listed.contains(&digest) && listed.contains("\"dist\""), "{listed}\n{made}");
    Ok(())
}

/// W08-R09's `kiln/cache@v1` with its local tier: a first run misses and saves every chunk; a
/// second restores the first's entry by a restore key with nothing downloaded (the runner kept
/// the chunks), changes a little of it, and saves only the chunks that changed.
#[test]
fn w08_r09_kiln_cache_moves_only_new_chunks() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { agent: true, ci: "kiln_workflows = true", ..Options::default() })?;
    let home = dir.path().join("githome");
    std::fs::create_dir_all(&home)?;
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/cached", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work)?;
    must(git(&home, &work, &["init", "-q", "-b", "main"]), "init")?;
    std::fs::write(work.join("README.md"), "cached\n")?;
    must(git(&home, &work, &["add", "."]), "add")?;
    must(git(&home, &work, &["commit", "-qm", "first"]), "commit")?;
    let url = format!("http://x:{}@127.0.0.1:{}/git/acme/cached.git", s.admin, s.port);
    must(git(&home, &work, &["push", "-q", &url, "main"]), "push")?;
    until(60, "the first poll", || {
        let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        Ok((body.contains("\"last_polled_at\":null")).then_some(body))
    })?;
    // 24 MiB of the same noise each run (about a dozen chunks), and on the second run a line more.
    std::fs::create_dir_all(work.join(".kiln"))?;
    std::fs::write(
        work.join(".kiln/cache.yaml"),
        r#"on: push
jobs:
  build:
    runs-on: local
    checkout: false
    steps:
      - id: c
        uses: kiln/cache@v1
        with:
          key: big-${{ github.sha }}
          restore-keys: big-
          path: big
          local: true
      - run: |
          mkdir -p big
          if [ ! -f big/noise.bin ]; then
            python3 -c "import random; open('big/noise.bin','wb').write(random.Random(7).randbytes(24 << 20))"
          fi
          echo "run ${{ github.run_number }} hit=${{ steps.c.outputs.cache-hit }} matched=${{ steps.c.outputs.cache-matched-key }}" >> big/runs.txt
"#,
    )?;
    let runs = |n: usize| -> anyhow::Result<Vec<Value>> {
        let deadline = Instant::now() + Duration::from_secs(300);
        loop {
            let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/cached&limit=10", &s.admin, None)?;
            let list: Vec<Value> = serde_json::from_str::<Value>(&body)?.get("runs").and_then(Value::as_array).cloned().unwrap_or_default();
            let done: Vec<Value> = list.into_iter().filter(|r| text(r, "workflow").ends_with("cache.yaml") && r.get("finished_at").is_some_and(|f| !f.is_null())).collect();
            if done.len() >= n {
                return Ok(done);
            }
            anyhow::ensure!(Instant::now() < deadline, "{n} runs didn't finish: {body}\n{}", s.log());
            std::thread::sleep(Duration::from_millis(500));
        }
    };
    let log_of = |run: &Value| -> anyhow::Result<String> {
        let (_, detail) = http(s.port, "GET", &format!("/api/ci/runs/{}", text(run, "id")), &s.admin, None)?;
        let leg = serde_json::from_str::<Value>(&detail)?.pointer("/legs/0/id").and_then(Value::as_str).unwrap_or_default().to_owned();
        let mut all = String::new();
        for step in 0..6 {
            let (_, t) = http(s.port, "GET", &format!("/api/ci/legs/{leg}/logs?step={step}"), &s.admin, None)?;
            all.push_str(&t);
        }
        Ok(all)
    };
    must(git(&home, &work, &["add", "."]), "add")?;
    must(git(&home, &work, &["commit", "-qm", "cache"]), "commit")?;
    must(git(&home, &work, &["push", "-q", &url, "main"]), "push")?;
    let first = runs(1)?;
    let one = first.first().cloned().unwrap_or_default();
    let said = log_of(&one)?;
    assert_eq!(text(&one, "conclusion"), "success", "{said}\n{}", s.log());
    assert!(said.contains("Cache not found for input keys"), "{said}");
    let sent = |log: &str| -> Option<(u64, u64)> {
        let at = log.split("bytes sent").next()?.rsplit(';').next()?;
        let mut w = at.split_whitespace();
        let a = w.next()?.parse().ok()?;
        let _of = w.next()?;
        let b = w.next()?.parse().ok()?;
        Some((a, b))
    };
    let (up1, size1) = sent(&said).ok_or_else(|| anyhow::anyhow!("no save in: {said}"))?;
    assert_eq!(up1, size1, "the first save sends everything: {said}");
    // The second run: restored from the first's chunks on this runner, then a little more saved.
    std::fs::write(work.join("README.md"), "cached, again\n")?;
    must(git(&home, &work, &["commit", "-qam", "again"]), "commit")?;
    must(git(&home, &work, &["push", "-q", &url, "main"]), "push")?;
    let both = runs(2)?;
    let two = both.first().cloned().unwrap_or_default();
    let said = log_of(&two)?;
    assert_eq!(text(&two, "conclusion"), "success", "{said}\n{}", s.log());
    assert!(said.contains("Cache restored from key: big-") && said.contains("(0 bytes downloaded,"), "restored from the runner's own chunks: {said}");
    let (up2, size2) = sent(&said).ok_or_else(|| anyhow::anyhow!("no save in: {said}"))?;
    // (The archive's size may not change: tar pads the longer runs.txt to the same block. A
    // chunk is 2 MiB on average and at most 8: what changed is one or two of them.)
    assert!(size2 >= size1 && up2 * 2 < size2, "only what changed went: {up2} of {size2} bytes\n{said}");
    Ok(())
}

/// W08-R09's `scope: global`: a default branch's run saves an entry every repository of its owner
/// restores; another owner's repository doesn't see it, and a branch's run can't save there.
#[test]
fn w08_r09_kiln_cache_global_scope() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { agent: true, ci: "kiln_workflows = true", ..Options::default() })?;
    let home = dir.path().join("githome");
    std::fs::create_dir_all(&home)?;
    let workflow = "on: push\njobs:\n  c:\n    runs-on: local\n    checkout: false\n    steps:\n      - id: c\n        uses: kiln/cache@v1\n        with:\n          key: shared-${{ github.ref_name }}\n          path: shared\n          scope: global\n      - run: |\n          echo \"restored: $(cat shared/who.txt 2>/dev/null || echo nothing)\"\n          mkdir -p shared && echo \"${{ github.repository }}\" > shared/who.txt\n";
    // The workflow pushed to `branch` of `name` (made with it on main); what its run said.
    let run = |name: &str, branch: &str| -> anyhow::Result<String> {
        let work = dir.path().join(name.replace('/', "_"));
        let url = format!("http://x:{}@127.0.0.1:{}/git/{name}.git", s.admin, s.port);
        if branch == "main" {
            let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": name, "mode": "native" })))?;
            anyhow::ensure!(st == 200, "{st} {body}");
            std::fs::create_dir_all(&work)?;
            must(git(&home, &work, &["init", "-q", "-b", "main"]), "init")?;
            std::fs::write(work.join("README.md"), "shared\n")?;
            must(git(&home, &work, &["add", "."]), "add")?;
            must(git(&home, &work, &["commit", "-qm", "first"]), "commit")?;
            must(git(&home, &work, &["push", "-q", &url, "main"]), "push")?;
            until(60, "the first poll", || {
                let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
                Ok(body.contains("\"last_polled_at\":null").then_some(body))
            })?;
            std::fs::create_dir_all(work.join(".kiln"))?;
            std::fs::write(work.join(".kiln/shared.yaml"), workflow)?;
        } else {
            must(git(&home, &work, &["checkout", "-q", "-b", branch]), "branch")?;
            std::fs::write(work.join("README.md"), format!("shared, on {branch}\n"))?;
        }
        must(git(&home, &work, &["add", "."]), "add")?;
        must(git(&home, &work, &["commit", "-qm", "shared"]), "commit")?;
        must(git(&home, &work, &["push", "-q", &url, branch]), "push")?;
        let deadline = Instant::now() + Duration::from_secs(300);
        let done = loop {
            let (_, body) = http(s.port, "GET", &format!("/api/ci/runs?repo={name}&limit=10"), &s.admin, None)?;
            let list: Vec<Value> = serde_json::from_str::<Value>(&body)?.get("runs").and_then(Value::as_array).cloned().unwrap_or_default();
            if let Some(r) = list.into_iter().find(|r| text(r, "branch").trim_start_matches("refs/heads/") == branch && r.get("finished_at").is_some_and(|f| !f.is_null())) {
                break r;
            }
            anyhow::ensure!(Instant::now() < deadline, "{name}@{branch} didn't finish: {body}\n{}", s.log());
            std::thread::sleep(Duration::from_millis(500));
        };
        let (_, detail) = http(s.port, "GET", &format!("/api/ci/runs/{}", text(&done, "id")), &s.admin, None)?;
        let leg = serde_json::from_str::<Value>(&detail)?.pointer("/legs/0/id").and_then(Value::as_str).unwrap_or_default().to_owned();
        let (_, leg_detail) = http(s.port, "GET", &format!("/api/ci/legs/{leg}"), &s.admin, None)?;
        let mut said: Vec<String> = serde_json::from_str::<Value>(&leg_detail)?.get("annotations").and_then(Value::as_array).into_iter().flatten().filter_map(|a| a.get("message").and_then(Value::as_str).map(str::to_owned)).collect();
        for step in 0..4 {
            let (_, t) = http(s.port, "GET", &format!("/api/ci/legs/{leg}/logs?step={step}"), &s.admin, None)?;
            said.push(t);
        }
        Ok(format!("{}: {}", text(&done, "conclusion"), said.join("\n")))
    };
    let one = run("acme/one", "main")?;
    assert!(one.starts_with("success") && one.contains("Cache not found for input keys: shared-main") && one.contains("Cache saved with key: shared-main"), "{one}\n{}", s.log());
    let two = run("acme/two", "main")?;
    assert!(two.contains("Cache restored from key: shared-main") && two.contains("restored: acme/one") && !two.contains("Cache saved"), "the owner's other repository restores it: {two}");
    let three = run("other/three", "main")?;
    assert!(three.contains("Cache not found for input keys: shared-main") && three.contains("restored: nothing"), "another owner's doesn't see it: {three}");
    let branch = run("acme/one", "feature")?;
    assert!(branch.contains("only main's runs save to the `global` scope"), "a branch can't save there: {branch}");
    Ok(())
}

/// W16-R01's opt-in discovery: a job's `test-results: discover` takes the report its step wrote
/// where nextest writes it, though the step failed, and leaves out the report the checkout brought
/// (git tracks it).
#[test]
fn w16_r01_discovered_test_reports() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { agent: true, ci: "kiln_workflows = true", ..Options::default() })?;
    let home = dir.path().join("githome");
    std::fs::create_dir_all(&home)?;
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/tested", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work)?;
    must(git(&home, &work, &["init", "-q", "-b", "main"]), "init")?;
    std::fs::write(work.join("README.md"), "tested\n")?;
    must(git(&home, &work, &["add", "."]), "add")?;
    must(git(&home, &work, &["commit", "-qm", "first"]), "commit")?;
    let url = format!("http://x:{}@127.0.0.1:{}/git/acme/tested.git", s.admin, s.port);
    must(git(&home, &work, &["push", "-q", &url, "main"]), "push")?;
    until(60, "the first poll", || {
        let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        Ok(body.contains("\"last_polled_at\":null").then_some(body))
    })?;
    // A report the repository keeps (a fixture), and a job whose tests fail.
    std::fs::create_dir_all(work.join("fixtures"))?;
    std::fs::write(work.join("fixtures/junit.xml"), r#"<?xml version="1.0"?><testsuite name="fixture" tests="1"><testcase classname="fixture" name="from_the_fixture"/></testsuite>"#)?;
    std::fs::create_dir_all(work.join(".kiln"))?;
    std::fs::write(
        work.join(".kiln/tests.yaml"),
        r#"on: push
jobs:
  t:
    runs-on: local
    test-results: discover
    steps:
      - run: |
          mkdir -p target/nextest/ci
          printf '%s' '<?xml version="1.0"?><testsuites><testsuite name="calc" tests="2" failures="1"><testcase classname="calc" name="adds"/><testcase classname="calc" name="divides"><failure message="boom">boom</failure></testcase></testsuite></testsuites>' > target/nextest/ci/junit.xml
          exit 1
"#,
    )?;
    must(git(&home, &work, &["add", "."]), "add")?;
    must(git(&home, &work, &["commit", "-qm", "tests"]), "commit")?;
    must(git(&home, &work, &["push", "-q", &url, "main"]), "push")?;
    let deadline = Instant::now() + Duration::from_secs(300);
    let run = loop {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/tested&limit=5", &s.admin, None)?;
        let list: Vec<Value> = serde_json::from_str::<Value>(&body)?.get("runs").and_then(Value::as_array).cloned().unwrap_or_default();
        if let Some(r) = list.into_iter().find(|r| text(r, "workflow").ends_with("tests.yaml") && r.get("finished_at").is_some_and(|f| !f.is_null())) {
            break r;
        }
        anyhow::ensure!(Instant::now() < deadline, "the run didn't finish: {body}\n{}", s.log());
        std::thread::sleep(Duration::from_millis(500));
    };
    assert_eq!(text(&run, "conclusion"), "failure", "the tests failed: {run}");
    let (_, tests) = http(s.port, "GET", &format!("/api/ci/runs/{}/tests", text(&run, "id")), &s.admin, None)?;
    let tests: Value = serde_json::from_str(&tests)?;
    let mut seen: Vec<(String, String)> = tests.get("tests").and_then(Value::as_array).into_iter().flatten().map(|t| (text(t, "name").to_owned(), text(t, "outcome").to_owned())).collect();
    seen.sort();
    assert_eq!(seen, vec![("adds".to_owned(), "passed".to_owned()), ("divides".to_owned(), "failed".to_owned())], "the job's report, not the fixture: {tests}\n{}", s.log());
    Ok(())
}

/// W16-R09: a job's `bench-gate` result shows on the diff and in the land queue. Three trunk runs
/// make `parse`'s baseline; a diff makes it 10 % slower, past the gate's 5 %. The diff's run fails
/// with the gate's verdict beside it, and its candidate in the land queue fails the same way: the
/// entry is ejected saying which metric got worse, the verdict beside it too.
#[test]
fn w16_r09_bench_gate_on_the_diff_and_the_land_queue() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { agent: true, ci: "kiln_workflows = true", ..Options::default() })?;
    let home = dir.path().join("githome");
    std::fs::create_dir_all(&home)?;
    let url = |token: &str| format!("http://x:{token}@127.0.0.1:{}/git/acme/bench.git", s.port);
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/bench", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "native repository: {st} {body}");
    let dev = person(&s, "dev", &["developer@repo:acme/bench"])?;
    let rev = person(&s, "rev", &["developer@repo:acme/bench"])?;
    let lead = person(&s, "lead", &["maintainer@repo:acme/bench"])?;
    must(git(&home, dir.path(), &["clone", "-q", &url(&lead), "work"]), "clone")?;
    let work = dir.path().join("work");
    std::fs::write(work.join("README.md"), "bench\n")?;
    must(git(&home, &work, &["add", "."]), "add")?;
    must(git(&home, &work, &["commit", "-qm", "first"]), "commit")?;
    // The trunk moves as the owner moves it (only the land queue writes it for others).
    let owner = url(&s.admin);
    must(git(&home, &work, &["push", "-q", &owner, "HEAD:main"]), "push")?;
    until(60, "the first poll", || {
        let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        Ok(body.contains("\"last_polled_at\":null").then_some(body))
    })?;
    std::fs::create_dir_all(work.join(".kiln"))?;
    std::fs::write(work.join(".kiln/review.yaml"), "land:\n  approvals: 1\n")?;
    // The metric is whatever the commit's `speed.txt` says.
    std::fs::write(
        work.join(".kiln/bench.yaml"),
        r#"on: [push, diff, land]
jobs:
  bench:
    runs-on: local
    bench-gate: { metric: "parse*", max-regression: 5% }
    steps:
      - run: printf '{"name":"parse","value":%s,"unit":"ns","better":"lower"}\n' "$(cat speed.txt)" >> "$KILN_METRICS"
"#,
    )?;
    std::fs::write(work.join("speed.txt"), "1000\n")?;
    let runs = |sha: &str| -> anyhow::Result<Vec<Value>> {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/bench&limit=50", &s.admin, None)?;
        Ok(serde_json::from_str::<Value>(&body)?.get("runs").and_then(Value::as_array).cloned().unwrap_or_default().into_iter().filter(|r| text(r, "sha") == sha).collect())
    };
    let finished = |sha: &str, what: &str| -> anyhow::Result<Value> {
        let mut last = Value::Null;
        until(240, what, || {
            last = runs(sha)?.into_iter().find(|r| r.get("finished_at").is_some_and(|f| !f.is_null())).unwrap_or(Value::Null);
            Ok(last.is_null().then(|| s.log()))
        })?;
        Ok(last)
    };
    // The baseline: three trunk runs at 1,000 ns.
    for i in 0..3 {
        std::fs::write(work.join("README.md"), format!("bench {i}\n"))?;
        must(git(&home, &work, &["add", "."]), "add")?;
        must(git(&home, &work, &["commit", "-qm", &format!("trunk {i}")]), "commit")?;
        must(git(&home, &work, &["push", "-q", &owner, "HEAD:main"]), "push")?;
        let sha = must(git(&home, &work, &["rev-parse", "HEAD"]), "rev-parse")?.trim().to_owned();
        let run = finished(&sha, &format!("trunk run {i}"))?;
        anyhow::ensure!(text(&run, "conclusion") == "success", "trunk run {i}: {run}\n{}", s.log());
    }

    // A diff 10 % slower: its run fails, the gate's verdict beside it.
    let head = commit(&home, &work, "speed.txt", "1100\n", "Parse more carefully", &format!("I{}", "7".repeat(40)))?;
    must(git(&home, &work, &["push", "-q", &url(&dev), "HEAD:refs/for/main"]), "submit")?;
    let run = finished(&head, "the diff's run")?;
    assert_eq!(text(&run, "conclusion"), "failure", "the gate fails the diff's run: {run}");
    let d = api(&s, &dev, "GET", "/api/diffs/D1")?;
    let gates: Vec<Value> = d.get("runs").and_then(Value::as_array).into_iter().flatten().flat_map(|r| r.get("gates").and_then(Value::as_array).cloned().unwrap_or_default()).collect();
    assert!(
        gates.iter().any(|g| text(g, "level") == "error" && text(g, "job") == "bench" && text(g, "message").contains("parse is 10.0% worse than main") && text(g, "message").contains("more than the 5% it allows")),
        "the diff's gates: {d}"
    );

    // Accepted and landed, its candidate fails the gate too: ejected, saying why.
    let (st, r) = post(&s, &rev, "/api/diffs/D1/review", &json!({ "action": "accept" }))?;
    anyhow::ensure!(st == 200, "accept: {r}");
    let (st, e) = post(&s, &lead, "/api/diffs/D1/land", &json!({}))?;
    anyhow::ensure!(st == 200, "land: {e}");
    let mut entry = Value::Null;
    until(300, "the entry is ejected", || {
        let q = api(&s, &dev, "GET", "/api/repos/acme%2Fbench/queue")?;
        entry = q.get("entries").and_then(Value::as_array).and_then(|l| l.iter().find(|x| x.get("diffs") == Some(&json!(["D1"])))).cloned().unwrap_or(Value::Null);
        Ok((text(&entry, "state") != "ejected").then(|| format!("{q}\n{}", s.log())))
    })?;
    let reason = text(&entry, "reason");
    assert!(reason.contains("`.kiln/bench.yaml` failure on the candidate; bench-gate: parse is 10.0% worse than main"), "{entry}");
    let said: Vec<&Value> = entry.get("gates").and_then(Value::as_array).into_iter().flatten().collect();
    assert!(said.iter().any(|g| text(g, "level") == "error" && text(g, "message").contains("parse is 10.0% worse")), "the candidate's gates: {entry}");
    assert_eq!(text(&api(&s, &dev, "GET", "/api/diffs/D1")?, "state"), "accepted", "back to its review state");
    Ok(())
}

/// W15-R01's concurrency limit: with `concurrency: 1`, two jobs deploying to the environment run
/// one after the other, though the runner could take both at once (two slots).
#[test]
fn w15_r01_an_environments_deployments_one_at_a_time() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { agent: true, ci: "kiln_workflows = true", ..Options::default() })?;
    let home = dir.path().join("githome");
    std::fs::create_dir_all(&home)?;
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/envs", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work)?;
    must(git(&home, &work, &["init", "-q", "-b", "main"]), "init")?;
    std::fs::write(work.join("README.md"), "envs\n")?;
    must(git(&home, &work, &["add", "."]), "add")?;
    must(git(&home, &work, &["commit", "-qm", "first"]), "commit")?;
    let url = format!("http://x:{}@127.0.0.1:{}/git/acme/envs.git", s.admin, s.port);
    must(git(&home, &work, &["push", "-q", &url, "main"]), "push")?;
    until(60, "the first poll", || {
        let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        Ok(body.contains("\"last_polled_at\":null").then_some(body))
    })?;
    std::fs::create_dir_all(work.join(".kiln"))?;
    std::fs::write(work.join(".kiln/environments.yaml"), "staging:\n  concurrency: 1\n")?;
    let job = |name: &str| format!("  {name}:\n    runs-on: local\n    checkout: false\n    environment: staging\n    steps:\n      - run: sleep 3\n");
    std::fs::write(work.join(".kiln/deploys.yaml"), format!("on: push\njobs:\n{}{}", job("a"), job("b")))?;
    must(git(&home, &work, &["add", "."]), "add")?;
    must(git(&home, &work, &["commit", "-qm", "deploys"]), "commit")?;
    must(git(&home, &work, &["push", "-q", &url, "main"]), "push")?;
    let deadline = Instant::now() + Duration::from_secs(300);
    let legs = loop {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/envs&limit=5", &s.admin, None)?;
        let list: Vec<Value> = serde_json::from_str::<Value>(&body)?.get("runs").and_then(Value::as_array).cloned().unwrap_or_default();
        if let Some(r) = list.into_iter().find(|r| text(r, "workflow").ends_with("deploys.yaml") && r.get("finished_at").is_some_and(|f| !f.is_null())) {
            anyhow::ensure!(text(&r, "conclusion") == "success", "{r}\n{}", s.log());
            let (_, detail) = http(s.port, "GET", &format!("/api/ci/runs/{}", text(&r, "id")), &s.admin, None)?;
            break serde_json::from_str::<Value>(&detail)?.get("legs").and_then(Value::as_array).cloned().unwrap_or_default();
        }
        anyhow::ensure!(Instant::now() < deadline, "the run didn't finish: {body}\n{}", s.log());
        std::thread::sleep(Duration::from_millis(500));
    };
    let mut spans: Vec<(i64, i64)> = legs.iter().map(|l| (l.get("started_at").and_then(Value::as_i64).unwrap_or(0), l.get("finished_at").and_then(Value::as_i64).unwrap_or(0))).collect();
    spans.sort_unstable();
    assert_eq!(spans.len(), 2, "{legs:?}");
    let (first, second) = (spans.first().copied().unwrap_or_default(), spans.get(1).copied().unwrap_or_default());
    assert!(second.0 >= first.1, "one at a time: {spans:?}");
    Ok(())
}

/// W15-R02, R08 and R09: environments of each kind, the starter set and the board. A host
/// environment's jobs run on its runners: on the runner they name, or nowhere (waiting, unroutable)
/// when no runner meets them. A platform environment's jobs get its integration (one the
/// repository can't use fails the job before it starts). A kind comes from the file too, from its
/// targets. The board has every environment, its latest deployment and what waits to deploy there,
/// narrowed to the repositories its caller may read.
#[test]
fn w15_r08_kinds_the_starter_set_and_the_board() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { agent: true, ci: "kiln_workflows = true", ..Options::default() })?;
    let home = dir.path().join("githome");
    std::fs::create_dir_all(&home)?;
    for name in ["acme/kinds", "acme/other"] {
        let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": name, "mode": "native" })))?;
        anyhow::ensure!(st == 200, "{st} {body}");
    }
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work)?;
    must(git(&home, &work, &["init", "-q", "-b", "main"]), "init")?;
    std::fs::write(work.join("README.md"), "kinds\n")?;
    must(git(&home, &work, &["add", "."]), "add")?;
    must(git(&home, &work, &["commit", "-qm", "first"]), "commit")?;
    let url = format!("http://x:{}@127.0.0.1:{}/git/acme/kinds.git", s.admin, s.port);
    must(git(&home, &work, &["push", "-q", &url, "main"]), "push")?;
    // acme/other has no commits to look at: only acme/kinds is waited for.
    until(60, "the first poll", || {
        let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        let repos: Vec<Value> = serde_json::from_str(&body).unwrap_or_default();
        let polled = repos.iter().any(|r| text(r, "name") == "acme/kinds" && r.get("last_polled_at").is_some_and(|t| !t.is_null()));
        Ok((!polled).then_some(body))
    })?;

    // The starter set: made once, kept the second time.
    let (st, body) = http(s.port, "POST", "/api/ci/environments/starter?repo=acme/kinds", &s.admin, Some(&json!({})))?;
    anyhow::ensure!(st == 200, "{st} {body}");
    assert_eq!(serde_json::from_str::<Value>(&body)?.get("made"), Some(&json!(["dev", "staging", "production"])), "{body}");
    let (_, body) = http(s.port, "POST", "/api/ci/environments/starter?repo=acme/kinds", &s.admin, Some(&json!({})))?;
    let again: Value = serde_json::from_str(&body)?;
    assert_eq!(again.get("made"), Some(&json!([])), "{body}");
    assert_eq!(again.get("kept").and_then(Value::as_array).map(Vec::len), Some(3), "{body}");
    let (_, body) = http(s.port, "GET", "/api/ci/environments/production?repo=acme/kinds", &s.admin, None)?;
    let production: Value = serde_json::from_str(&body)?;
    assert_eq!(production.pointer("/protection/reviewers"), Some(&json!(["owner"])), "made by the server's own token: {body}");
    assert_eq!(production.pointer("/protection/branches"), Some(&json!(["main"])), "the trunk only: {body}");

    // Kinds, saved and refused.
    let save = |name: &str, body: Value| -> anyhow::Result<(u16, Value)> {
        let (st, b) = http(s.port, "PUT", &format!("/api/ci/environments/{name}?repo=acme/kinds"), &s.admin, Some(&body))?;
        Ok((st, serde_json::from_str(&b).unwrap_or(json!({ "text": b }))))
    };
    let (st, web) = save("web", json!({ "kind": "host", "targets": { "runners": "label local", "clusters": ["dropped"] } }))?;
    anyhow::ensure!(st == 200, "{st} {web}");
    assert_eq!(web.get("targets"), Some(&json!({ "runners": "label local" })), "only what its kind deploys to: {web}");
    for (name, body, why) in [
        ("bad", json!({ "kind": "host" }), "runners"),
        ("bad", json!({ "kind": "kubernetes", "targets": { "clusters": [] } }), "clusters"),
        ("bad", json!({ "kind": "space" }), "isn't a kind"),
        ("bad", json!({ "kind": "host", "targets": { "runners": "label (" } }), "its runners"),
        ("bad", json!({ "kind": "platform" }), "integration"),
    ] {
        let (st, v) = save(name, body.clone())?;
        assert_eq!(st, 400, "{body}: {v}");
        assert!(v.to_string().contains(why), "{body}: {v}");
    }
    for (name, body) in [("nowhere", json!({ "kind": "host", "targets": { "runners": "label nowhere" } })), ("site", json!({ "kind": "platform", "targets": { "integration": "nope" } }))] {
        let (st, v) = save(name, body)?;
        anyhow::ensure!(st == 200, "{st} {v}");
    }

    // A kind from the file, and the jobs deploying to each.
    std::fs::create_dir_all(work.join(".kiln"))?;
    std::fs::write(work.join(".kiln/environments.yaml"), "cluster:\n  clusters: [in-cluster]\n  namespace: web\n")?;
    let job = |id: &str, env: &str| format!("  {id}:\n    runs-on: local\n    checkout: false\n    environment: {env}\n    steps:\n      - run: echo deploying to {env}\n");
    std::fs::write(work.join(".kiln/deploys.yaml"), format!("on: push\njobs:\n{}{}{}{}", job("toweb", "web"), job("tonowhere", "nowhere"), job("tosite", "site"), job("toprod", "production")))?;
    must(git(&home, &work, &["add", "."]), "add")?;
    must(git(&home, &work, &["commit", "-qm", "deploys"]), "commit")?;
    must(git(&home, &work, &["push", "-q", &url, "main"]), "push")?;
    let deadline = Instant::now() + Duration::from_secs(300);
    let (run, legs) = loop {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/kinds&limit=5", &s.admin, None)?;
        let list: Vec<Value> = serde_json::from_str::<Value>(&body)?.get("runs").and_then(Value::as_array).cloned().unwrap_or_default();
        if let Some(r) = list.into_iter().find(|r| text(r, "workflow").ends_with("deploys.yaml")) {
            let (_, detail) = http(s.port, "GET", &format!("/api/ci/runs/{}", text(&r, "id")), &s.admin, None)?;
            let legs = serde_json::from_str::<Value>(&detail)?.get("legs").and_then(Value::as_array).cloned().unwrap_or_default();
            let of = |job: &str| legs.iter().find(|l| text(l, "job") == job).cloned().unwrap_or_default();
            let reason = |job: &str| of(job).pointer("/wait_reason/reason").and_then(Value::as_str).unwrap_or_default().to_owned();
            if text(&of("toweb"), "status") == "succeeded" && text(&of("tosite"), "status") == "failed" && reason("tonowhere") == "unroutable" && reason("toprod") == "approval" {
                break (text(&r, "id").to_owned(), legs);
            }
        }
        anyhow::ensure!(Instant::now() < deadline, "the deployments didn't settle: {body}\n{}", s.log());
        std::thread::sleep(Duration::from_millis(500));
    };
    let of = |job: &str| legs.iter().find(|l| text(l, "job") == job).cloned().unwrap_or_default();
    assert!(of("tonowhere").pointer("/wait_reason/failing").is_some_and(|f| f.to_string().contains("label nowhere")), "the environment's runners: {}", of("tonowhere"));
    let (_, notes) = http(s.port, "GET", &format!("/api/ci/runs/{run}/annotations"), &s.admin, None)?;
    let said = serde_json::from_str::<Value>(&notes)?.get("annotations").and_then(Value::as_array).is_some_and(|a| a.iter().any(|n| text(n, "leg") == text(&of("tosite"), "id") && text(n, "message").contains("nope")));
    assert!(said, "the platform's integration, said on its leg: {notes}");

    // The board: the latest deployment, what waits, the file's kind (trunk's file is read again
    // once what was read of it is 20 s old).
    let mut body = String::new();
    until(60, "the file's environment on the board", || {
        let (st, b) = http(s.port, "GET", "/api/ci/environments/board", &s.admin, None)?;
        anyhow::ensure!(st == 200, "{st} {b}");
        body = b.clone();
        Ok((!b.contains("\"name\":\"cluster\"")).then_some(b))
    })?;
    let rows: Vec<Value> = serde_json::from_str::<Value>(&body)?.get("environments").and_then(Value::as_array).cloned().unwrap_or_default();
    let row = |name: &str| rows.iter().find(|e| text(e, "repo") == "acme/kinds" && text(e, "name") == name).cloned().unwrap_or_default();
    assert_eq!(row("web").pointer("/latest_deployment/state").and_then(Value::as_str), Some("success"), "{body}");
    assert!(row("production").get("waiting").and_then(Value::as_array).is_some_and(|w| w.iter().any(|w| w.pointer("/wait/reason").and_then(Value::as_str) == Some("approval") && text(w, "leg") == text(&of("toprod"), "id"))), "{body}");
    assert_eq!(row("cluster").get("kind").and_then(Value::as_str), Some("kubernetes"), "from its clusters: {body}");
    assert_eq!(row("cluster").get("targets"), Some(&json!({ "clusters": ["in-cluster"], "namespace": "web" })), "{body}");
    assert_eq!(row("cluster").get("source").and_then(Value::as_str), Some("file"), "{body}");
    let (_, body) = http(s.port, "GET", "/api/ci/environments/production?repo=acme/kinds", &s.admin, None)?;
    assert!(serde_json::from_str::<Value>(&body)?.get("waiting").and_then(Value::as_array).is_some_and(|w| w.len() == 1), "one environment's page says what waits: {body}");

    // Narrowed: someone who reads only acme/other sees none of acme/kinds.
    let (st, body) = http(s.port, "POST", "/api/users", &s.admin, Some(&json!({ "name": "watcher", "grants": ["viewer@repo:acme/other"] })))?;
    anyhow::ensure!((200..300).contains(&st), "{st} {body}");
    let (_, body) = http(s.port, "POST", "/api/tokens", &s.admin, Some(&json!({ "kind": "pat", "user": "watcher", "grants": ["owner"] })))?;
    let watcher = serde_json::from_str::<Value>(&body)?.get("token").and_then(Value::as_str).unwrap_or_default().to_owned();
    let (st, body) = http(s.port, "GET", "/api/ci/environments/board", &watcher, None)?;
    anyhow::ensure!(st == 200, "{st} {body}");
    assert!(!body.contains("acme/kinds"), "{body}");
    let (st, _) = http(s.port, "GET", "/api/ci/environments?repo=acme/kinds", &watcher, None)?;
    assert_eq!(st, 403);
    Ok(())
}

/// W09-AC07: an environment's secret reaches a leg only after its deployment is approved. While
/// the leg waits for its reviewer nothing is sent (the secret is never used); once approved, the
/// leg gets it and its use is recorded.
#[test]
fn w09_ac07_environment_secrets_after_approval() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { agent: true, ci: "kiln_workflows = true", ..Options::default() })?;
    let home = dir.path().join("githome");
    std::fs::create_dir_all(&home)?;
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/guarded", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let scope = "/api/ci/secrets?scope=env:acme/guarded:production";
    let (st, body) = http(s.port, "PUT", "/api/ci/secrets/E_TOKEN?scope=env:acme/guarded:production", &s.admin, Some(&json!({ "value": "only-after-approval" })))?;
    anyhow::ensure!((200..300).contains(&st), "{st} {body}");
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work)?;
    must(git(&home, &work, &["init", "-q", "-b", "main"]), "init")?;
    std::fs::write(work.join("README.md"), "guarded\n")?;
    must(git(&home, &work, &["add", "."]), "add")?;
    must(git(&home, &work, &["commit", "-qm", "first"]), "commit")?;
    let url = format!("http://x:{}@127.0.0.1:{}/git/acme/guarded.git", s.admin, s.port);
    must(git(&home, &work, &["push", "-q", &url, "main"]), "push")?;
    until(60, "the first poll", || {
        let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        Ok(body.contains("\"last_polled_at\":null").then_some(body))
    })?;
    std::fs::create_dir_all(work.join(".kiln"))?;
    std::fs::write(work.join(".kiln/environments.yaml"), "production:\n  reviewers: [alice]\n")?;
    std::fs::write(
        work.join(".kiln/ship.yaml"),
        "on: push\njobs:\n  ship:\n    runs-on: local\n    checkout: false\n    environment: production\n    steps:\n      - env:\n          T: ${{ secrets.E_TOKEN }}\n        run: echo \"has it, ${#T} characters\"\n",
    )?;
    must(git(&home, &work, &["add", "."]), "add")?;
    must(git(&home, &work, &["commit", "-qm", "ship"]), "commit")?;
    must(git(&home, &work, &["push", "-q", &url, "main"]), "push")?;
    // The leg waits for its reviewer: the secret goes nowhere.
    until(120, "the leg waiting for approval", || {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/guarded&limit=5", &s.admin, None)?;
        let Some(r) = serde_json::from_str::<Value>(&body)?.get("runs").and_then(Value::as_array).and_then(|l| l.iter().find(|r| text(r, "workflow").ends_with("ship.yaml")).cloned()) else { return Ok(Some(body)) };
        let (_, detail) = http(s.port, "GET", &format!("/api/ci/runs/{}", text(&r, "id")), &s.admin, None)?;
        let waiting = serde_json::from_str::<Value>(&detail)?.pointer("/legs/0/wait_reason/reason").and_then(Value::as_str) == Some("approval");
        Ok((!waiting).then_some(detail))
    })?;
    std::thread::sleep(Duration::from_secs(2));
    let (_, listed) = http(s.port, "GET", scope, &s.admin, None)?;
    let used = |listed: &str| -> anyhow::Result<bool> { Ok(serde_json::from_str::<Value>(listed)?.pointer("/secrets/0/last_used_at").is_some_and(|v| !v.is_null())) };
    assert!(!used(&listed)?, "nothing sent before approval: {listed}");
    let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/guarded&limit=5", &s.admin, None)?;
    let id = serde_json::from_str::<Value>(&body)?.get("runs").and_then(Value::as_array).and_then(|l| l.iter().find(|r| text(r, "workflow").ends_with("ship.yaml")).map(|r| text(r, "id").to_owned())).unwrap_or_default();
    let (st, out) = http(s.port, "POST", &format!("/api/ci/runs/{id}/approve"), &s.admin, Some(&json!({})))?;
    anyhow::ensure!(st == 200, "approve: {st} {out}");
    let deadline = Instant::now() + Duration::from_secs(120);
    let finished = loop {
        let (_, detail) = http(s.port, "GET", &format!("/api/ci/runs/{id}"), &s.admin, None)?;
        let d: Value = serde_json::from_str(&detail)?;
        if d.pointer("/run/finished_at").is_some_and(|f| !f.is_null()) {
            break d;
        }
        anyhow::ensure!(Instant::now() < deadline, "the run didn't finish: {detail}\n{}", s.log());
        std::thread::sleep(Duration::from_millis(500));
    };
    assert_eq!(finished.pointer("/run/conclusion").and_then(Value::as_str), Some("success"), "{finished}\n{}", s.log());
    let leg = finished.pointer("/legs/0/id").and_then(Value::as_str).unwrap_or_default().to_owned();
    let mut log = String::new();
    for step in 0..3 {
        let (_, t) = http(s.port, "GET", &format!("/api/ci/legs/{leg}/logs?step={step}"), &s.admin, None)?;
        log.push_str(&t);
    }
    assert!(log.contains("has it, 19 characters"), "approved, it got the secret: {log}");
    let (_, listed) = http(s.port, "GET", scope, &s.admin, None)?;
    assert!(used(&listed)?, "its use recorded: {listed}");
    Ok(())
}

/// A local receiver for webhooks: each request's body, as it comes.
fn receiver() -> anyhow::Result<(u16, std::sync::Arc<std::sync::Mutex<Vec<String>>>)> {
    use std::io::{Read as _, Write as _};
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    let got = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let keep = std::sync::Arc::clone(&got);
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let mut buf = Vec::new();
            let mut chunk = [0u8; 8192];
            // The headers, then as much body as they say.
            let body = loop {
                let Ok(n) = stream.read(&mut chunk) else { break None };
                if n == 0 {
                    break None;
                }
                buf.extend_from_slice(chunk.get(..n).unwrap_or_default());
                let text = String::from_utf8_lossy(&buf).into_owned();
                if let Some((head, rest)) = text.split_once("\r\n\r\n") {
                    let len = head.lines().find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").and_then(|v| v.trim().parse::<usize>().ok())).unwrap_or(0);
                    if rest.len() >= len {
                        break Some(rest.to_owned());
                    }
                }
            };
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            if let (Some(b), Ok(mut g)) = (body, keep.lock()) {
                g.push(b);
            }
        }
    });
    Ok((port, got))
}

/// W15-R03's promotion and custom gates, end to end: production takes a commit only after it
/// deployed to staging and soaked there, and once its smoke workflow passed for it (the leg waits
/// with `promotion` and `gate`, then deploys); a commit whose smoke workflow fails never deploys
/// there, its job failing with why.
#[test]
fn w15_r03_promotion_and_custom_gates() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { agent: true, ci: "kiln_workflows = true", ..Options::default() })?;
    let home = dir.path().join("githome");
    std::fs::create_dir_all(&home)?;
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/promoted", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work)?;
    must(git(&home, &work, &["init", "-q", "-b", "main"]), "init")?;
    std::fs::write(work.join("README.md"), "promoted\n")?;
    must(git(&home, &work, &["add", "."]), "add")?;
    must(git(&home, &work, &["commit", "-qm", "first"]), "commit")?;
    let url = format!("http://x:{}@127.0.0.1:{}/git/acme/promoted.git", s.admin, s.port);
    must(git(&home, &work, &["push", "-q", &url, "main"]), "push")?;
    until(60, "the first poll", || {
        let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        Ok(body.contains("\"last_polled_at\":null").then_some(body))
    })?;
    std::fs::create_dir_all(work.join(".kiln"))?;
    std::fs::write(work.join(".kiln/environments.yaml"), "staging: {}\nproduction:\n  after: staging\n  soak: 4s\n  gates:\n    - workflow: .kiln/smoke.yaml\n")?;
    std::fs::write(
        work.join(".kiln/ship.yaml"),
        "on: push\njobs:\n  staging:\n    runs-on: local\n    checkout: false\n    environment: staging\n    steps: [{run: echo staged}]\n  production:\n    runs-on: local\n    checkout: false\n    needs: staging\n    environment: production\n    steps: [{run: echo shipped}]\n",
    )?;
    // The smoke test reads the commit (kiln's implicit checkout).
    std::fs::write(work.join(".kiln/smoke.yaml"), "on: push\njobs:\n  smoke:\n    runs-on: local\n    steps:\n      - run: sleep 2; test ! -f BROKEN\n")?;
    must(git(&home, &work, &["add", "."]), "add")?;
    must(git(&home, &work, &["commit", "-qm", "ship it"]), "commit")?;
    must(git(&home, &work, &["push", "-q", &url, "main"]), "push")?;
    let sha = must(git(&home, &work, &["rev-parse", "HEAD"]), "rev-parse")?.trim().to_owned();
    // The ship run of a commit, once it's done: its detail.
    let shipped = |sha: &str| -> anyhow::Result<Option<Value>> {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/promoted&limit=20", &s.admin, None)?;
        let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
        let Some(r) = v.get("runs").and_then(Value::as_array).into_iter().flatten().find(|r| text(r, "sha") == sha && text(r, "workflow").ends_with("ship.yaml") && r.get("finished_at").is_some_and(|f| !f.is_null())).cloned() else { return Ok(None) };
        let (_, detail) = http(s.port, "GET", &format!("/api/ci/runs/{}", text(&r, "id")), &s.admin, None)?;
        Ok(Some(serde_json::from_str(&detail)?))
    };
    let mut done = Value::Null;
    until(240, "the ship run", || {
        Ok(match shipped(&sha)? {
            Some(d) => {
                done = d;
                None
            }
            None => Some(s.log()),
        })
    })?;
    assert_eq!(done.pointer("/run/conclusion").and_then(Value::as_str), Some("success"), "{done}\n{}", s.log());
    let production = done.get("legs").and_then(Value::as_array).into_iter().flatten().find(|l| text(l, "job") == "production").cloned().unwrap_or_default();
    // It waited for staging's soak and the smoke run (seconds by wait reason, W19-R01).
    let waited = |reason: &str| production.pointer(&format!("/waits/{reason}")).and_then(Value::as_f64).unwrap_or(0.0);
    assert!(waited("promotion") + waited("gate") >= 3.0, "{production}");

    // A commit whose smoke workflow fails: production refuses it.
    std::fs::write(work.join("BROKEN"), "1\n")?;
    must(git(&home, &work, &["add", "."]), "add")?;
    must(git(&home, &work, &["commit", "-qm", "broken"]), "commit")?;
    must(git(&home, &work, &["push", "-q", &url, "main"]), "push")?;
    let broken = must(git(&home, &work, &["rev-parse", "HEAD"]), "rev-parse")?.trim().to_owned();
    until(240, "the broken commit's ship run", || {
        Ok(match shipped(&broken)? {
            Some(d) => {
                done = d;
                None
            }
            None => Some(s.log()),
        })
    })?;
    assert_eq!(done.pointer("/run/conclusion").and_then(Value::as_str), Some("failure"), "{done}");
    let production = done.get("legs").and_then(Value::as_array).into_iter().flatten().find(|l| text(l, "job") == "production").cloned().unwrap_or_default();
    assert!(text(&production, "error").contains("stopped by its gate .kiln/smoke.yaml"), "{production}");
    Ok(())
}

/// W19-R06's other events reach a channel: a job's failed deployment sends `deploy.failed`, and
/// a run whose report has a test a retry passed sends `leg.flaky` (a webhook here).
#[test]
fn w19_r06_deploy_failed_and_leg_flaky_reach_a_channel() -> anyhow::Result<()> {
    let (hook, got) = receiver()?;
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { agent: true, ci: "kiln_workflows = true\nallow_private_targets = [\"127.0.0.1\"]", ..Options::default() })?;
    let (st, body) = http(s.port, "POST", "/api/ci/integrations", &s.admin, Some(&json!({ "kind": "webhook", "name": "hook", "config": { "url": format!("http://127.0.0.1:{hook}/hook") }, "secret": "s3cret", "scope": "server" })))?;
    anyhow::ensure!((200..300).contains(&st), "{st} {body}");
    let home = dir.path().join("githome");
    std::fs::create_dir_all(&home)?;
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/told", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work)?;
    must(git(&home, &work, &["init", "-q", "-b", "main"]), "init")?;
    std::fs::write(work.join("README.md"), "told\n")?;
    must(git(&home, &work, &["add", "."]), "add")?;
    must(git(&home, &work, &["commit", "-qm", "first"]), "commit")?;
    let url = format!("http://x:{}@127.0.0.1:{}/git/acme/told.git", s.admin, s.port);
    must(git(&home, &work, &["push", "-q", &url, "main"]), "push")?;
    until(60, "the first poll", || {
        let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        Ok(body.contains("\"last_polled_at\":null").then_some(body))
    })?;
    std::fs::create_dir_all(work.join(".kiln"))?;
    std::fs::write(work.join(".kiln/notify.yaml"), "- on: [deploy.failed, leg.flaky]\n  to: webhook:hook\n")?;
    std::fs::write(
        work.join(".kiln/told.yaml"),
        r#"on: push
jobs:
  ship:
    runs-on: local
    checkout: false
    environment: staging
    steps:
      - run: exit 1
  tests:
    runs-on: local
    checkout: false
    steps:
      - test-results: { path: report.xml }
        run: |
          printf '%s' '<?xml version="1.0"?><testsuite name="calc" tests="1"><testcase classname="calc" name="adds"><flakyFailure message="boom" type="AssertionError"/></testcase></testsuite>' > report.xml
"#,
    )?;
    must(git(&home, &work, &["add", "."]), "add")?;
    must(git(&home, &work, &["commit", "-qm", "told"]), "commit")?;
    must(git(&home, &work, &["push", "-q", &url, "main"]), "push")?;
    until(180, "deploy.failed and leg.flaky at the webhook", || {
        let seen = got.lock().map(|g| g.clone()).unwrap_or_default();
        let has = |e: &str| seen.iter().any(|b| b.contains(&format!("\"event\":\"{e}\"")));
        Ok((!(has("deploy.failed") && has("leg.flaky"))).then(|| format!("{seen:?}\n{}", s.log())))
    })?;
    let seen = got.lock().map(|g| g.clone()).unwrap_or_default();
    let flaky = seen.iter().find(|b| b.contains("\"event\":\"leg.flaky\"")).cloned().unwrap_or_default();
    assert!(flaky.contains("calc adds"), "{flaky}");
    let failed = seen.iter().find(|b| b.contains("\"event\":\"deploy.failed\"")).cloned().unwrap_or_default();
    assert!(failed.contains("staging"), "{failed}");
    Ok(())
}

/// W20-R02 over MCP, as an agent uses it: a broken workflow edit is refused with its errors; a
/// good one, and a test's quarantine, become diffs (never writes to the trunk); the diffs are
/// listed and commented on; a secret is set and only its name comes back; an environment is saved
/// and listed; the runners are there to read.
#[test]
fn w20_r02_operations_over_mcp() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { agent: true, ..Options::default() })?;
    let home = dir.path().join("githome");
    std::fs::create_dir_all(&home)?;
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/agentic", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let url = format!("http://x:{}@127.0.0.1:{}/git/acme/agentic.git", s.admin, s.port);
    must(git(&home, dir.path(), &["clone", "-q", &url, "work"]), "clone")?;
    let work = dir.path().join("work");
    std::fs::create_dir_all(work.join(".github/workflows"))?;
    std::fs::write(work.join(".github/workflows/ci.yml"), "on: push\njobs:\n  a:\n    runs-on: local\n    steps:\n      - run: echo a\n")?;
    must(git(&home, &work, &["add", "."]), "add")?;
    must(git(&home, &work, &["commit", "-qm", "first"]), "commit")?;
    must(git(&home, &work, &["push", "-q", "origin", "main"]), "push")?;
    let trunk = must(git(&home, &work, &["rev-parse", "HEAD"]), "rev-parse")?.trim().to_owned();
    let mcp = |tool: &str, args: Value| -> anyhow::Result<(bool, String)> {
        let (st, body) = http(s.port, "POST", "/api/mcp", &s.admin, Some(&json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": tool, "arguments": args } })))?;
        anyhow::ensure!(st == 200, "{tool}: {st} {body}");
        let v: Value = serde_json::from_str(&body)?;
        let failed = v.pointer("/result/isError").and_then(Value::as_bool).unwrap_or(true);
        Ok((failed, v.pointer("/result/content/0/text").and_then(Value::as_str).unwrap_or_default().to_owned()))
    };
    // A broken edit is refused, with why; a good one is a diff.
    let (failed, out) = mcp("ci_workflows_edit", json!({ "repo": "acme/agentic", "path": ".github/workflows/ci.yml", "content": "on: push\njobs:\n  a:\n    steps: []\n" }))?;
    assert!(failed && out.contains("has errors"), "{out}");
    let (failed, out) = mcp("ci_workflows_edit", json!({ "repo": "acme/agentic", "path": ".github/workflows/ci.yml", "content": "on: push\njobs:\n  a:\n    runs-on: local\n    steps:\n      - run: echo b\n", "title": "Say b" }))?;
    assert!(!failed && out.contains("D1"), "{out}");
    let (failed, out) = mcp("ci_tests_propose_quarantine", json!({ "repo": "acme/agentic", "test": "calc::adds", "reason": "races the clock", "until": "2030-01-01" }))?;
    assert!(!failed && out.contains("D2"), "{out}");
    // Proposals, not writes: the trunk is where it was, and each diff's commit has the change.
    must(git(&home, &work, &["fetch", "-q", "origin"]), "fetch")?;
    assert_eq!(must(git(&home, &work, &["rev-parse", "origin/main"]), "rev-parse")?.trim(), trunk, "the trunk didn't move");
    must(git(&home, &work, &["fetch", "-q", "origin", "refs/kiln/diffs/D2/v1"]), "fetch D2")?;
    let quarantine = must(git(&home, &work, &["show", "FETCH_HEAD:.kiln/quarantine.yaml"]), "show")?;
    assert!(quarantine.contains("calc::adds") && quarantine.contains("races the clock") && quarantine.contains("2030-01-01") && !quarantine.contains("issue"), "{quarantine}");
    let (failed, out) = mcp("ci_tests_propose_quarantine", json!({ "repo": "acme/agentic", "test": "calc::adds", "reason": "again", "until": "soon" }))?;
    assert!(failed && out.contains("until"), "a bad entry isn't proposed: {out}");
    let (_, listed) = mcp("ci_diffs_list", json!({ "repo": "acme/agentic" }))?;
    assert!(listed.contains("Say b") && listed.contains("Quarantine calc::adds"), "{listed}");
    let (failed, out) = mcp("ci_diffs_comment", json!({ "diff": "D1", "body": "looks right", "publish": true }))?;
    assert!(!failed, "{out}");
    let (_, threads) = mcp("ci_diffs_threads", json!({ "diff": "D1" }))?;
    assert!(threads.contains("looks right"), "{threads}");
    // Secrets: only names come back.
    let (failed, out) = mcp("ci_secrets_set", json!({ "scope": "repo:acme/agentic", "name": "DEPLOY_TOKEN", "value": "sekrit-value-31337" }))?;
    assert!(!failed, "{out}");
    let (_, names) = mcp("ci_secrets_list", json!({ "scope": "repo:acme/agentic" }))?;
    assert!(names.contains("DEPLOY_TOKEN") && !names.contains("sekrit-value-31337"), "{names}");
    let (failed, out) = mcp("ci_environments_save", json!({ "repo": "acme/agentic", "name": "staging", "concurrency": 2 }))?;
    assert!(!failed, "{out}");
    let (_, envs) = mcp("ci_environments_list", json!({ "repo": "acme/agentic" }))?;
    assert!(envs.contains("\"staging\"") && envs.contains("\"concurrency\":2"), "{envs}");
    let (failed, runners) = mcp("ci_runners_list", json!({}))?;
    assert!(!failed && runners.contains("local"), "{runners}");
    // A runner image: checked as the editor checks it, and proposed as a diff once it's sound.
    let (failed, found) = mcp("ci_images_validate", json!({ "content": "FROM ubuntu\nRUN apt-get install curl\n" }))?;
    assert!(!failed && found.contains("DL3006") && found.contains("DL3014"), "{found}");
    let (failed, out) = mcp("ci_images_propose", json!({ "repo": "acme/agentic", "name": "bad", "content": "RUN echo\n" }))?;
    assert!(failed && out.contains("KDF1002"), "a Dockerfile with errors isn't proposed: {out}");
    let sound = "FROM ubuntu:24.04\nRUN apt-get update && apt-get install -y --no-install-recommends curl && rm -rf /var/lib/apt/lists/*\n";
    let (failed, out) = mcp("ci_images_propose", json!({ "repo": "acme/agentic", "name": "web", "content": sound }))?;
    assert!(!failed && out.contains("D3"), "{out}");
    must(git(&home, &work, &["fetch", "-q", "origin", "refs/kiln/diffs/D3/v1"]), "fetch D3")?;
    assert_eq!(must(git(&home, &work, &["show", "FETCH_HEAD:.kiln/images/web/Dockerfile"]), "show")?, sound);
    Ok(())
}

/// W17-R01's terminal from the CLI: `kiln debug terminal <leg>` into a running leg, with its input
/// piped (so the terminal here isn't made raw): a command runs in the leg's environment, and the
/// terminal ends when its shell does.
#[test]
fn w17_r01_kiln_debug_terminal() -> anyhow::Result<()> {
    use std::io::Write as _;
    let dir = tempfile::tempdir()?;
    // The server's own runner opens terminals into legs as its `[terminal]` switch says, in its
    // shell (`sh` here: a login shell may greet a home it hasn't seen with questions).
    let s = start(dir.path(), &Options { agent: true, ci: "kiln_workflows = true", extra: "[terminal]\nenabled = true\nshell = \"sh\"\n" })?;
    let home = dir.path().join("githome");
    std::fs::create_dir_all(&home)?;
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/held", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work)?;
    must(git(&home, &work, &["init", "-q", "-b", "main"]), "init")?;
    std::fs::write(work.join("README.md"), "held\n")?;
    must(git(&home, &work, &["add", "."]), "add")?;
    must(git(&home, &work, &["commit", "-qm", "first"]), "commit")?;
    let url = format!("http://x:{}@127.0.0.1:{}/git/acme/held.git", s.admin, s.port);
    must(git(&home, &work, &["push", "-q", &url, "main"]), "push")?;
    until(60, "the first poll", || {
        let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        Ok(body.contains("\"last_polled_at\":null").then_some(body))
    })?;
    std::fs::create_dir_all(work.join(".kiln"))?;
    std::fs::write(work.join(".kiln/hold.yaml"), "on: push\njobs:\n  hold:\n    runs-on: local\n    checkout: false\n    env:\n      SECRET_WORD: plums\n    steps:\n      - run: sleep 120\n")?;
    must(git(&home, &work, &["add", "."]), "add")?;
    must(git(&home, &work, &["commit", "-qm", "hold"]), "commit")?;
    must(git(&home, &work, &["push", "-q", &url, "main"]), "push")?;
    // The leg, once it runs.
    let deadline = Instant::now() + Duration::from_secs(120);
    let (run, leg) = loop {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/held&limit=5", &s.admin, None)?;
        let run = serde_json::from_str::<Value>(&body)?.pointer("/runs/0/id").and_then(Value::as_str).unwrap_or_default().to_owned();
        if !run.is_empty() {
            let (_, detail) = http(s.port, "GET", &format!("/api/ci/runs/{run}"), &s.admin, None)?;
            let d: Value = serde_json::from_str(&detail)?;
            if d.pointer("/legs/0/status").and_then(Value::as_str) == Some("running") {
                break (run, d.pointer("/legs/0/id").and_then(Value::as_str).unwrap_or_default().to_owned());
            }
        }
        anyhow::ensure!(Instant::now() < deadline, "the leg never ran: {body}\n{}", s.log());
        std::thread::sleep(Duration::from_millis(500));
    };
    let mut child = Command::new(env!("CARGO_BIN_EXE_kiln"))
        .args(["debug", "terminal", &leg])
        .env("HOME", &home)
        .env("KILN_URL", format!("http://127.0.0.1:{}", s.port))
        .env("KILN_ADMIN_TOKEN", &s.admin)
        .env_remove("KILN_CONFIG")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(b"echo word-is-$SECRET_WORD-$((6*7))\nexit\n")?;
    }
    let deadline = Instant::now() + Duration::from_secs(60);
    while child.try_wait()?.is_none() {
        if Instant::now() > deadline {
            let _ = child.kill();
            anyhow::bail!("the terminal didn't end with its shell\n{}", s.log());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let o = child.wait_with_output()?;
    let (out, err) = (String::from_utf8_lossy(&o.stdout).into_owned(), String::from_utf8_lossy(&o.stderr).into_owned());
    assert!(o.status.success(), "{out}\n{err}");
    assert!(out.contains("word-is-plums-42"), "the command ran in the leg's environment: {out}\n{err}");
    assert!(err.contains("its shell exited"), "{err}");
    let _ = http(s.port, "POST", &format!("/api/ci/runs/{run}/cancel"), &s.admin, Some(&json!({})));
    Ok(())
}

/// W15-R05's `kiln/deploy@v1`: a job bound to an environment releases an app there through the
/// deploy service and follows the release; one that isn't bound is refused, saying why; and the
/// `deploy:` shorthand binds its job and releases the one app the repository is the source of.
/// (No CD provider runs here, so a release ends where the service leaves it, or the step's time
/// runs out: what's checked is that CI asks as the job, and follows.)
#[test]
fn w15_r05_kiln_deploy_releases_from_the_job() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { agent: true, ci: "kiln_workflows = true", extra: "[cd]\nenabled = true\n" })?;
    let home = dir.path().join("githome");
    std::fs::create_dir_all(&home)?;
    // The repository first: an app names it as its source.
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/ship", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "{st} {body}");
    for (path, body) in [
        ("/api/deploy/environments", json!({ "name": "staging" })),
        ("/api/deploy/apps", json!({ "name": "web", "package": { "kind": "helm", "repo": "oci://registry.invalid/charts", "chart": "web" } })),
        ("/api/deploy/apps", json!({ "name": "api", "source": "acme/ship", "package": { "kind": "helm", "repo": "oci://registry.invalid/charts", "chart": "api" } })),
    ] {
        let (st, out) = http(s.port, "POST", path, &s.admin, Some(&body))?;
        anyhow::ensure!((200..300).contains(&st), "{path}: {st} {out}");
    }
    for app in ["web", "api"] {
        let (st, out) = http(s.port, "POST", &format!("/api/deploy/apps/{app}/environments/staging"), &s.admin, Some(&json!({})))?;
        anyhow::ensure!((200..300).contains(&st), "register {app}: {st} {out}");
    }
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work)?;
    must(git(&home, &work, &["init", "-q", "-b", "main"]), "init")?;
    std::fs::write(work.join("README.md"), "ship\n")?;
    must(git(&home, &work, &["add", "."]), "add")?;
    must(git(&home, &work, &["commit", "-qm", "first"]), "commit")?;
    let url = format!("http://x:{}@127.0.0.1:{}/git/acme/ship.git", s.admin, s.port);
    must(git(&home, &work, &["push", "-q", &url, "main"]), "push")?;
    until(60, "the first poll", || {
        let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        Ok((body.contains("\"last_polled_at\":null")).then_some(body))
    })?;
    std::fs::create_dir_all(work.join(".kiln"))?;
    std::fs::write(
        work.join(".kiln/ship.yaml"),
        r#"on: push
jobs:
  ship:
    runs-on: local
    checkout: false
    environment: staging
    steps:
      - id: d
        uses: kiln/deploy@v1
        with:
          app: web
          environment: staging
          revision: 1.2.3
          timeout: 8s
      - if: always()
        run: echo "release ${{ steps.d.outputs.release-id }} ended ${{ steps.d.outputs.state }}"
  stray:
    runs-on: local
    checkout: false
    steps:
      - uses: kiln/deploy@v1
        with:
          app: web
          environment: staging
          revision: 1.2.3
  short:
    runs-on: local
    checkout: false
    deploy: { environment: staging, revision: 1.2.3, timeout: 8s }
"#,
    )?;
    must(git(&home, &work, &["add", "."]), "add")?;
    must(git(&home, &work, &["commit", "-qm", "ship"]), "commit")?;
    must(git(&home, &work, &["push", "-q", &url, "main"]), "push")?;
    let deadline = Instant::now() + Duration::from_secs(300);
    let legs = loop {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/ship&limit=5", &s.admin, None)?;
        let runs: Value = serde_json::from_str(&body)?;
        if let Some(r) = runs.pointer("/runs/0").filter(|r| r.get("finished_at").is_some_and(|f| !f.is_null()) && text(r, "workflow").ends_with("ship.yaml")) {
            let (_, detail) = http(s.port, "GET", &format!("/api/ci/runs/{}", text(r, "id")), &s.admin, None)?;
            break serde_json::from_str::<Value>(&detail)?.get("legs").and_then(Value::as_array).cloned().unwrap_or_default();
        }
        anyhow::ensure!(Instant::now() < deadline, "the run didn't finish: {body}\n{}", s.log());
        std::thread::sleep(Duration::from_millis(500));
    };
    let said = |key: &str| -> anyhow::Result<(String, String)> {
        let l = legs.iter().find(|l| text(l, "key") == key).cloned().unwrap_or_default();
        let (_, detail) = http(s.port, "GET", &format!("/api/ci/legs/{}", text(&l, "id")), &s.admin, None)?;
        let detail: Value = serde_json::from_str(&detail)?;
        let notes: Vec<String> = detail.get("annotations").and_then(Value::as_array).into_iter().flatten().filter_map(|a| a.get("message").and_then(Value::as_str).map(str::to_owned)).collect();
        let mut log = notes.join("\n");
        for step in 0..3 {
            let (_, t) = http(s.port, "GET", &format!("/api/ci/legs/{}/logs?step={step}", text(&l, "id")), &s.admin, None)?;
            log.push_str(&t);
        }
        Ok((text(&l, "id").to_owned(), log))
    };
    let (stray_id, stray) = said("stray")?;
    assert!(stray.contains("kiln/deploy releases to the job's own environment"), "{stray}");
    let (ship_id, ship) = said("ship")?;
    assert!(ship.contains("of web to staging:"), "the release was followed: {ship}\n{}", s.log());
    let (_, releases) = http(s.port, "GET", "/api/deploy/releases?app=web", &s.admin, None)?;
    assert!(releases.contains(&format!("job:{ship_id}")) && !releases.contains(&format!("job:{stray_id}")), "asked as the bound job alone: {releases}");
    // The shorthand: no steps of its own, bound to staging, and the repository's own app.
    let (short_id, short) = said("short")?;
    assert!(short.contains("of api to staging:"), "{short}\n{}", s.log());
    let (_, releases) = http(s.port, "GET", "/api/deploy/releases?app=api", &s.admin, None)?;
    assert!(releases.contains(&format!("job:{short_id}")), "{releases}");
    Ok(())
}

/// W03-AC07: a diff's first and second versions are `pull_request` `opened` and `synchronize`,
/// with the diff's number, its head and its base, and a GitHub workflow `on: pull_request` runs
/// on both.
#[test]
fn w03_ac07_a_diffs_versions_are_pull_request_events() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { agent: true, ..Options::default() })?;
    let home = dir.path().join("githome");
    std::fs::create_dir_all(&home)?;
    let url = format!("http://x:{}@127.0.0.1:{}/git/acme/prs.git", s.admin, s.port);
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/prs", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "{st} {body}");
    must(git(&home, dir.path(), &["clone", "-q", &url, "work"]), "clone")?;
    let work = dir.path().join("work");
    std::fs::create_dir_all(work.join(".github/workflows"))?;
    std::fs::write(
        work.join(".github/workflows/pr.yml"),
        "on: pull_request\njobs:\n  show:\n    runs-on: local\n    steps:\n      - run: |\n          echo \"saw ${{ github.event.action }} of ${{ github.event.number }}: ${{ github.event.pull_request.head.sha }} onto ${{ github.event.pull_request.base.ref }} (${{ github.event.pull_request.commits }} commit, +${{ github.event.pull_request.additions }} -${{ github.event.pull_request.deletions }} in ${{ github.event.pull_request.changed_files }})\"\n",
    )?;
    std::fs::write(work.join("README.md"), "prs\n")?;
    must(git(&home, &work, &["add", "."]), "add")?;
    must(git(&home, &work, &["commit", "-qm", "first"]), "commit")?;
    must(git(&home, &work, &["push", "-q", "origin", "main"]), "the first trunk")?;
    let v1 = commit(&home, &work, "a.txt", "one\n", "one: a", "I4444444444444444444444444444444444444444")?;
    let out = must(git(&home, &work, &["push", "origin", "HEAD:refs/for/main"]), "v1")?;
    assert!(out.contains("D1 v1 (new)"), "{out}");
    std::fs::write(work.join("a.txt"), "one, again\n")?;
    must(git(&home, &work, &["commit", "-q", "--amend", "--no-edit", "-a"]), "amend")?;
    let v2 = must(git(&home, &work, &["rev-parse", "HEAD"]), "rev-parse")?.trim().to_owned();
    let out = must(git(&home, &work, &["push", "origin", "HEAD:refs/for/main"]), "v2")?;
    assert!(out.contains("D1 v2 (updated)"), "{out}");
    // Both versions' runs, and what each saw.
    let deadline = Instant::now() + Duration::from_secs(300);
    let saw = loop {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/prs&event=pull_request&limit=10", &s.admin, None)?;
        let done: Vec<Value> = serde_json::from_str::<Value>(&body)?.get("runs").and_then(Value::as_array).cloned().unwrap_or_default().into_iter().filter(|r| r.get("finished_at").is_some_and(|f| !f.is_null())).collect();
        if done.len() >= 2 {
            let mut logs = Vec::new();
            for r in &done {
                anyhow::ensure!(text(r, "conclusion") == "success", "{r}\n{}", s.log());
                let (_, detail) = http(s.port, "GET", &format!("/api/ci/runs/{}", text(r, "id")), &s.admin, None)?;
                let leg = serde_json::from_str::<Value>(&detail)?.pointer("/legs/0/id").and_then(Value::as_str).unwrap_or_default().to_owned();
                let mut all = String::new();
                for step in 0..4 {
                    let (_, t) = http(s.port, "GET", &format!("/api/ci/legs/{leg}/logs?step={step}"), &s.admin, None)?;
                    all.push_str(&t);
                }
                logs.push(all);
            }
            break logs;
        }
        anyhow::ensure!(Instant::now() < deadline, "two pull_request runs didn't finish: {body}\n{}", s.log());
        std::thread::sleep(Duration::from_millis(500));
    };
    // GitHub's counts, which a workflow sizes its fetch by (cpython's `--depth=$(( commits + 1 ))`).
    assert!(saw.iter().any(|l| l.contains(&format!("saw opened of 1: {v1} onto main (1 commit, +1 -0 in 1)"))), "v1 opened it: {saw:?}");
    assert!(saw.iter().any(|l| l.contains(&format!("saw synchronize of 1: {v2} onto main (1 commit, +1 -0 in 1)"))), "v2 synchronized it: {saw:?}");
    Ok(())
}

/// W03-AC05: a push both delivered by GitHub's webhook and seen by the poller runs once; a
/// redelivery of the webhook (the same `X-GitHub-Delivery`) does nothing, and neither does a new
/// delivery of the same push.
#[test]
fn w03_ac05_a_webhook_and_a_poll_of_one_push_run_once() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let home = dir.path().join("githome");
    std::fs::create_dir_all(&home)?;
    let repo = dir.path().join("hooked");
    std::fs::create_dir_all(&repo)?;
    must(git(&home, &repo, &["init", "-q", "-b", "main"]), "init")?;
    std::fs::write(repo.join("README.md"), "hooked\n")?;
    must(git(&home, &repo, &["add", "."]), "add")?;
    must(git(&home, &repo, &["commit", "-qm", "first"]), "commit")?;
    let extra = format!("[[repos]]\nname = \"acme/hooked\"\nclone_url = \"file://{}\"\nbranches = [\"main\"]\npoll_seconds = 1\n", repo.display());
    let s = start(dir.path(), &Options { agent: true, ci: "kiln_workflows = true", extra: &extra })?;
    until(60, "the first poll", || {
        let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        Ok((!(body.contains("acme/hooked") && !body.contains("\"last_polled_at\":null"))).then_some(body))
    })?;
    let before = must(git(&home, &repo, &["rev-parse", "HEAD"]), "rev-parse")?.trim().to_owned();
    std::fs::create_dir_all(repo.join(".kiln"))?;
    std::fs::write(repo.join(".kiln/hooked.yaml"), "on: push\njobs:\n  j:\n    runs-on: local\n    checkout: false\n    steps:\n      - run: echo hooked\n")?;
    must(git(&home, &repo, &["add", "."]), "add")?;
    must(git(&home, &repo, &["commit", "-qm", "hooked"]), "commit")?;
    let after = must(git(&home, &repo, &["rev-parse", "HEAD"]), "rev-parse")?.trim().to_owned();
    let payload = json!({
        "ref": "refs/heads/main", "before": before, "after": after, "created": false, "deleted": false, "forced": false,
        "repository": { "full_name": "acme/hooked", "name": "hooked", "owner": { "login": "acme" }, "default_branch": "main" },
        "head_commit": { "id": after, "message": "hooked", "timestamp": "2026-10-19T00:00:00Z", "author": { "name": "T", "email": "t@example.com" } },
        "pusher": { "name": "t" }, "sender": { "login": "t" },
    })
    .to_string();
    let deliver = |id: &str| -> anyhow::Result<String> {
        let out = Command::new("curl")
            .args(["-s", "-o", "/dev/null", "-w", "%{http_code}", "-X", "POST", "-H", "Content-Type: application/json", "-H", "X-GitHub-Event: push", "-H", &format!("X-GitHub-Delivery: {id}"), "--data-binary", &payload, &format!("http://127.0.0.1:{}/api/hooks/github", s.port)])
            .output()?;
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    };
    // The webhook, at once (the poll looks within a second too): whichever comes first runs it.
    assert_eq!(deliver("d-1")?, "202");
    let runs_of = || -> anyhow::Result<Vec<Value>> {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/hooked&limit=20", &s.admin, None)?;
        Ok(serde_json::from_str::<Value>(&body)?.get("runs").and_then(Value::as_array).cloned().unwrap_or_default().into_iter().filter(|r| text(r, "sha") == after).collect())
    };
    until(120, "the push's run", || {
        let n = runs_of()?.len();
        Ok((n == 0).then(|| "no run yet".to_owned()))
    })?;
    // The same delivery again, and a new delivery of the same push.
    assert_eq!(deliver("d-1")?, "202");
    assert_eq!(deliver("d-2")?, "202");
    // Polls go on meanwhile; nothing more runs.
    std::thread::sleep(Duration::from_secs(6));
    let runs = runs_of()?;
    assert_eq!(runs.len(), 1, "one run for the push: {runs:?}\n{}", s.log());
    Ok(())
}

/// A stand-in for OSV's API: `anyhow 1.0.0` has RUSTSEC-2099-0001, whose GHSA alias is high; it
/// keeps the purls each batch asked about.
fn osv_mock() -> anyhow::Result<(u16, std::sync::Arc<std::sync::Mutex<Vec<String>>>)> {
    use std::io::{Read as _, Write as _};
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    let asked = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let keep = std::sync::Arc::clone(&asked);
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let mut buf = Vec::new();
            let mut chunk = [0u8; 8192];
            let (head, body) = loop {
                let Ok(n) = stream.read(&mut chunk) else { break (String::new(), String::new()) };
                if n == 0 {
                    break (String::new(), String::new());
                }
                buf.extend_from_slice(chunk.get(..n).unwrap_or_default());
                let text = String::from_utf8_lossy(&buf).into_owned();
                if let Some((head, rest)) = text.split_once("\r\n\r\n") {
                    let len = head.lines().find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").and_then(|v| v.trim().parse::<usize>().ok())).unwrap_or(0);
                    if rest.len() >= len {
                        break (head.to_owned(), rest.to_owned());
                    }
                }
            };
            let path = head.split_whitespace().nth(1).unwrap_or_default().to_owned();
            let answer = match path.as_str() {
                "/v1/querybatch" => {
                    let q: Value = serde_json::from_str(&body).unwrap_or_default();
                    let purls: Vec<String> =
                        q.get("queries").and_then(Value::as_array).into_iter().flatten().filter_map(|x| x.pointer("/package/purl").and_then(Value::as_str).map(str::to_owned)).collect();
                    let results: Vec<Value> = purls.iter().map(|p| if p == "pkg:cargo/anyhow@1.0.0" { json!({ "vulns": [{ "id": "RUSTSEC-2099-0001" }] }) } else { json!({}) }).collect();
                    if let Ok(mut g) = keep.lock() {
                        g.extend(purls);
                    }
                    json!({ "results": results })
                }
                "/v1/vulns/RUSTSEC-2099-0001" => json!({ "id": "RUSTSEC-2099-0001", "aliases": ["GHSA-xxxx-yyyy-zzzz"], "summary": "anyhow 1.0.0 is a test's" }),
                "/v1/vulns/GHSA-xxxx-yyyy-zzzz" => json!({ "id": "GHSA-xxxx-yyyy-zzzz", "database_specific": { "severity": "HIGH" } }),
                _ => Value::Null,
            };
            let text = answer.to_string();
            let status = if answer.is_null() { "404 Not Found" } else { "200 OK" };
            let _ = stream.write_all(format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}", text.len()).as_bytes());
        }
    });
    Ok((port, asked))
}

fn cargo_lock(packages: &[(&str, &str)]) -> String {
    let mut out = String::from("# This file is automatically @generated by Cargo.\nversion = 4\n");
    for (name, version) in packages {
        out.push_str(&format!("\n[[package]]\nname = \"{name}\"\nversion = \"{version}\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n"));
    }
    out
}

/// The facade's dependency graph compare (docs/ci.md W14-R11), as `dependency-review-action` asks
/// it: what a push changed in its lockfile, a version bump as a removal and an addition, the added
/// packages looked up in the advisory database (`[ci.advisories]`, OSV's API). The older
/// `/git/refs` and `/statuses/{sha}` with it.
#[test]
fn w14_r11_dependency_graph_compare() -> anyhow::Result<()> {
    let (osv, asked) = osv_mock()?;
    let dir = tempfile::tempdir()?;
    let ci = format!("\n[ci.advisories]\nosv = \"http://127.0.0.1:{osv}\"\n");
    let s = start(dir.path(), &Options { agent: true, ci: &ci, ..Options::default() })?;
    let home = dir.path().join("githome");
    std::fs::create_dir_all(&home)?;
    let url = |token: &str| format!("http://x:{token}@127.0.0.1:{}/git/acme/app.git", s.port);
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/app", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "native repository: {st} {body}");
    must(git(&home, dir.path(), &["clone", "-q", &url(&s.admin), "work"]), "clone")?;
    let work = dir.path().join("work");
    std::fs::write(work.join("Cargo.lock"), cargo_lock(&[("serde", "1.0.1"), ("log", "0.4.0")]))?;
    must(git(&home, &work, &["add", "."]), "add")?;
    must(git(&home, &work, &["commit", "-qm", "first"]), "commit")?;
    must(git(&home, &work, &["push", "-q", "origin", "main"]), "the first trunk")?;
    until(60, "the first poll", || {
        let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        Ok((!(body.contains("acme/app") && !body.contains("\"last_polled_at\":null"))).then_some(body))
    })?;
    std::fs::create_dir_all(work.join(".github/workflows"))?;
    std::fs::write(
        work.join(".github/workflows/deps.yml"),
        r#"on: push
permissions:
  contents: read
  statuses: read
jobs:
  deps:
    runs-on: local
    steps:
      - env:
          TOKEN: ${{ github.token }}
          BEFORE: ${{ github.event.before }}
        run: |
          set -e
          api() { curl -sf -H "Authorization: Bearer $TOKEN" "$@"; }
          R="$GITHUB_API_URL/repos/$GITHUB_REPOSITORY"
          echo "compare: $(api "$R/dependency-graph/compare/$BEFORE...$GITHUB_SHA")"
          echo "one manifest: $(api "$R/dependency-graph/compare/$BEFORE...$GITHUB_SHA?name=nope.lock")"
          api "$R/git/refs" | grep -q '"ref":"refs/heads/main"'
          api "$R/git/refs/heads/main" | grep -q '^{.*"ref":"refs/heads/main"'
          test "$(api "$R/statuses/$GITHUB_SHA")" = "[]"
          echo compared
"#,
    )?;
    std::fs::write(work.join("Cargo.lock"), cargo_lock(&[("serde", "1.0.2"), ("log", "0.4.0"), ("anyhow", "1.0.0")]))?;
    must(git(&home, &work, &["add", "."]), "add")?;
    must(git(&home, &work, &["commit", "-qm", "deps"]), "commit")?;
    must(git(&home, &work, &["push", "-q", "origin", "HEAD:main"]), "the workflow")?;
    let deadline = Instant::now() + Duration::from_secs(240);
    let log = loop {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/app&limit=5", &s.admin, None)?;
        let v: Value = serde_json::from_str(&body).unwrap_or_default();
        let run = v.get("runs").and_then(Value::as_array).and_then(|r| r.first()).cloned();
        if let Some(r) = run.filter(|r| r.get("finished_at").is_some_and(|f| !f.is_null())) {
            let (_, detail) = http(s.port, "GET", &format!("/api/ci/runs/{}", text(&r, "id")), &s.admin, None)?;
            let leg = serde_json::from_str::<Value>(&detail)?.pointer("/legs/0/id").and_then(Value::as_str).unwrap_or_default().to_owned();
            let mut log = String::new();
            for step in 0..3 {
                let (_, text) = http(s.port, "GET", &format!("/api/ci/legs/{leg}/logs?step={step}"), &s.admin, None)?;
                log.push_str(&text);
            }
            assert_eq!(text(&r, "conclusion"), "success", "{r}\n{log}\n{}", s.log());
            break log;
        }
        anyhow::ensure!(Instant::now() < deadline, "no run: {body}\n{}", s.log());
        std::thread::sleep(Duration::from_millis(500));
    };
    assert!(log.contains("compared"), "{log}");
    let after = |marker: &str| -> Value { log.lines().find_map(|l| l.split_once(marker).map(|(_, j)| serde_json::from_str(j).unwrap_or_default())).unwrap_or_default() };
    let changes = after("compare: ");
    let short: Vec<(String, String, String, Vec<String>)> = changes
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| {
            let ghsas = c.get("vulnerabilities").and_then(Value::as_array).into_iter().flatten().map(|v| format!("{} {}", text(v, "advisory_ghsa_id"), text(v, "severity"))).collect();
            (text(c, "change_type").to_owned(), text(c, "name").to_owned(), text(c, "version").to_owned(), ghsas)
        })
        .collect();
    let want = [("removed", "serde", "1.0.1", vec![]), ("added", "anyhow", "1.0.0", vec!["GHSA-xxxx-yyyy-zzzz high".to_owned()]), ("added", "serde", "1.0.2", vec![])]
        .map(|(a, b, c, d)| (a.to_owned(), b.to_owned(), c.to_owned(), d));
    assert_eq!(short, want, "{changes}");
    assert!(changes.as_array().into_iter().flatten().all(|c| text(c, "manifest") == "Cargo.lock" && text(c, "ecosystem") == "rust"), "{changes}");
    assert_eq!(after("one manifest: "), json!([]), "{log}");
    // Only what the push added was asked about.
    let mut got = asked.lock().map(|g| g.clone()).unwrap_or_default();
    got.sort();
    assert_eq!(got, ["pkg:cargo/anyhow@1.0.0", "pkg:cargo/serde@1.0.2"]);
    Ok(())
}

/// The finished run of `acme/app` on `event`, and its first leg's log (its first `steps` steps).
fn ci_run_log(s: &Server, event: &str, steps: u32, secs: u64) -> anyhow::Result<(Value, String)> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/app&limit=10", &s.admin, None)?;
        let v: Value = serde_json::from_str(&body).unwrap_or_default();
        let run = v.get("runs").and_then(Value::as_array).into_iter().flatten().find(|r| text(r, "event") == event).cloned();
        if let Some(r) = run.filter(|r| r.get("finished_at").is_some_and(|f| !f.is_null())) {
            let (_, detail) = http(s.port, "GET", &format!("/api/ci/runs/{}", text(&r, "id")), &s.admin, None)?;
            let leg = serde_json::from_str::<Value>(&detail)?.pointer("/legs/0/id").and_then(Value::as_str).unwrap_or_default().to_owned();
            let mut log = String::new();
            for step in 0..steps {
                let (_, text) = http(s.port, "GET", &format!("/api/ci/legs/{leg}/logs?step={step}"), &s.admin, None)?;
                log.push_str(&text);
            }
            return Ok((r, log));
        }
        anyhow::ensure!(Instant::now() < deadline, "no finished {event} run: {body}\n{}", s.log());
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// W14-R11's GraphQL, as `gh` and GraphQL-asking actions use it from a job: a pull request (a
/// diff) read with a fragment, its labels and its head's checks rolled up; a comment added and a
/// label taken off by mutations, with ids from the queries; the open pull requests; `__type`'s
/// fields, as `gh` asks an Enterprise Server for them; and a root field kiln doesn't answer,
/// an error naming it with a warning on the leg.
#[test]
fn w14_r11_graphql() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { agent: true, ..Options::default() })?;
    let home = dir.path().join("githome");
    std::fs::create_dir_all(&home)?;
    let url = |token: &str| format!("http://x:{token}@127.0.0.1:{}/git/acme/app.git", s.port);
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/app", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "native repository: {st} {body}");
    let dev = person(&s, "dev", &["developer@repo:acme/app"])?;
    must(git(&home, dir.path(), &["clone", "-q", &url(&dev), "work"]), "clone")?;
    let work = dir.path().join("work");
    std::fs::create_dir_all(work.join(".github/workflows"))?;
    std::fs::write(
        work.join(".github/workflows/gql.yml"),
        r#"on: pull_request
permissions:
  contents: read
  pull-requests: write
  issues: write
  statuses: write
jobs:
  gql:
    runs-on: local
    steps:
      - env:
          TOKEN: ${{ github.token }}
          N: ${{ github.event.pull_request.number }}
          SHA: ${{ github.event.pull_request.head.sha }}
        run: |
          set -e
          api() { curl -sf -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" "$@"; }
          gql() { api "$GITHUB_GRAPHQL_URL" -d @"$1"; }
          R="$GITHUB_API_URL/repos/$GITHUB_REPOSITORY"
          OWNER=${GITHUB_REPOSITORY%/*}; NAME=${GITHUB_REPOSITORY#*/}
          api -X POST "$R/issues/$N/labels" -d '{"labels":["bug"]}' > /dev/null
          api -X POST "$R/statuses/$SHA" -d '{"state":"success","context":"lint"}' > /dev/null
          cat > q1.json <<JSON
          {"query":"query PullRequestByNumber(\$owner: String!, \$repo: String!, \$n: Int!) { repository(owner: \$owner, name: \$repo) { nameWithOwner defaultBranchRef { name } pullRequest(number: \$n) { ...pr } labels(first: 100) { nodes { id name } } } } fragment pr on PullRequest { id number title state isDraft headRefOid baseRefName author { __typename login } labels(first: 10) { totalCount nodes { name } } statusCheckRollup { state contexts(first: 10) { nodes { __typename ... on StatusContext { context state } } } } }","variables":{"owner":"$OWNER","repo":"$NAME","n":$N}}
          JSON
          ONE=$(gql q1.json)
          echo "q1: $ONE"
          PR=$(echo "$ONE" | grep -o '"id":"kiln-pr-[^"]*"' | head -1 | cut -d'"' -f4)
          LABEL=$(echo "$ONE" | grep -o '"id":"kiln-label-[^"]*"' | head -1 | cut -d'"' -f4)
          cat > q2.json <<JSON
          {"query":"mutation CommentCreate(\$input: AddCommentInput!) { addComment(input: \$input) { commentEdge { node { body author { login } } } subject { ... on PullRequest { number } } } }","variables":{"input":{"subjectId":"$PR","body":"from graphql"}}}
          JSON
          echo "q2: $(gql q2.json)"
          cat > q3.json <<JSON
          {"query":"mutation { removeLabelsFromLabelable(input: {labelableId: \"$PR\", labelIds: [\"$LABEL\"]}) { labelable { ... on Labelable { labels(first: 5) { totalCount } } } } }"}
          JSON
          echo "q3: $(gql q3.json)"
          cat > q4.json <<JSON
          {"query":"{ repository(owner: \"$OWNER\", name: \"$NAME\") { pullRequests(states: [OPEN], first: 5) { totalCount nodes { number } } } me: viewer { login } __type(name: \"PullRequest\") { fields(includeDeprecated: true) { name } } }"}
          JSON
          echo "q4: $(gql q4.json)"
          cat > q6.json <<JSON
          {"query":"query PullRequestSearch(\$q: String!) { search(query: \$q, type: ISSUE, first: 10) { issueCount nodes { ... on PullRequest { number title } } } }","variables":{"q":"repo:$GITHUB_REPOSITORY is:pr is:open author:dev"}}
          JSON
          echo "q6: $(gql q6.json)"
          echo "r1: $(api "$GITHUB_API_URL/search/issues?q=repo%3A$OWNER%2F$NAME+is%3Apr+is%3Aopen+one")"
          echo "r2: $(api "$GITHUB_API_URL/search/issues?q=repo%3A$OWNER%2F$NAME+is%3Apr+is%3Amerged")"
          echo '{"query":"{ licenses { name } }"}' > q5.json
          echo "q5: $(gql q5.json)"
          echo done
"#,
    )?;
    std::fs::write(work.join("README.md"), "app\n")?;
    must(git(&home, &work, &["add", "."]), "add")?;
    must(git(&home, &work, &["commit", "-qm", "first"]), "commit")?;
    must(git(&home, &work, &["push", "-q", "origin", "main"]), "the first trunk")?;
    commit(&home, &work, "a.txt", "one\n", "one: a", "I1111111111111111111111111111111111111111")?;
    must(git(&home, &work, &["push", "-q", "origin", "HEAD:refs/for/main"]), "submit")?;
    let (run, log) = ci_run_log(&s, "pull_request", 2, 240)?;
    assert_eq!(text(&run, "conclusion"), "success", "{run}\n{log}\n{}", s.log());
    let answer = |q: &str| -> Value { log.lines().find_map(|l| l.split_once(&format!("{q}: ")).and_then(|(_, j)| serde_json::from_str(j).ok())).unwrap_or_default() };
    let one = answer("q1");
    let pr = one.pointer("/data/repository/pullRequest").cloned().unwrap_or_default();
    assert_eq!(one.pointer("/data/repository/nameWithOwner"), Some(&json!("acme/app")), "{one}");
    assert_eq!(one.pointer("/data/repository/defaultBranchRef/name"), Some(&json!("main")), "{one}");
    assert_eq!((pr.get("number"), text(&pr, "title"), text(&pr, "state"), text(&pr, "baseRefName")), (Some(&json!(1)), "one: a", "OPEN", "main"), "{one}");
    assert_eq!((pr.pointer("/author/login"), pr.pointer("/author/__typename")), (Some(&json!("dev")), Some(&json!("User"))), "{one}");
    assert_eq!((pr.pointer("/labels/totalCount"), pr.pointer("/labels/nodes/0/name")), (Some(&json!(1)), Some(&json!("bug"))), "{one}");
    assert_eq!(pr.pointer("/statusCheckRollup/state"), Some(&json!("SUCCESS")), "{one}");
    assert_eq!(pr.pointer("/statusCheckRollup/contexts/nodes/0"), Some(&json!({ "__typename": "StatusContext", "context": "lint", "state": "SUCCESS" })), "{one}");
    assert!(one.get("errors").is_none(), "{one}");
    let two = answer("q2");
    assert_eq!(two.pointer("/data/addComment/commentEdge/node"), Some(&json!({ "body": "from graphql", "author": { "login": "github-actions" } })), "{two}");
    assert_eq!(two.pointer("/data/addComment/subject/number"), Some(&json!(1)), "{two}");
    assert_eq!(answer("q3").pointer("/data/removeLabelsFromLabelable/labelable/labels/totalCount"), Some(&json!(0)), "{log}");
    let four = answer("q4");
    assert_eq!(four.pointer("/data/repository/pullRequests/totalCount"), Some(&json!(1)), "{four}");
    assert_eq!(four.pointer("/data/me/login"), Some(&json!("github-actions")), "{four}");
    assert!(four.pointer("/data/__type/fields").and_then(Value::as_array).is_some_and(|f| f.iter().any(|x| text(x, "name") == "statusCheckRollup")), "{four}");
    // Search (W14-R11): GraphQL's, as `gh pr list --search` asks, and REST's.
    let six = answer("q6");
    assert_eq!(
        (six.pointer("/data/search/issueCount"), six.pointer("/data/search/nodes/0/number"), six.pointer("/data/search/nodes/0/title")),
        (Some(&json!(1)), Some(&json!(1)), Some(&json!("one: a"))),
        "{six}"
    );
    let r1 = answer("r1");
    assert_eq!((r1.get("total_count"), r1.pointer("/items/0/number")), (Some(&json!(1)), Some(&json!(1))), "{r1}");
    assert!(r1.pointer("/items/0/pull_request/url").is_some_and(|u| !u.is_null()), "an issue that is a pull request: {r1}");
    assert_eq!(answer("r2").get("total_count"), Some(&json!(0)), "nothing merged yet: {log}");
    let five = answer("q5");
    assert!(five.pointer("/errors/0/message").and_then(Value::as_str).is_some_and(|m| m.contains("not implemented by kiln: query field `licenses`")), "{five}");
    // What the mutations did is kiln's: the comment on the diff, as the job's.
    let threads = api(&s, &dev, "GET", "/api/diffs/D1/comments")?;
    let bodies: Vec<String> = threads.get("threads").and_then(Value::as_array).into_iter().flatten().filter_map(|t| t.pointer("/comments/0/body").and_then(Value::as_str).map(str::to_owned)).collect();
    assert!(bodies.iter().any(|b| b == "from graphql"), "{threads}");
    let (_, detail) = http(s.port, "GET", &format!("/api/ci/runs/{}", text(&run, "id")), &s.admin, None)?;
    let leg = serde_json::from_str::<Value>(&detail)?.pointer("/legs/0/id").and_then(Value::as_str).unwrap_or_default().to_owned();
    let (_, leg_json) = http(s.port, "GET", &format!("/api/ci/legs/{leg}"), &s.admin, None)?;
    assert!(leg_json.contains("GraphQL API for something kiln doesn't answer yet (query field `licenses`)"), "{leg_json}");
    Ok(())
}
