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
        r#"on:
  push:
    branches: [main]
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
        let run = v.get("runs").and_then(Value::as_array).and_then(|r| r.iter().find(|r| text(r, "workflow").ends_with("pr.yml"))).cloned();
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
            return Ok(());
        }
        anyhow::ensure!(Instant::now() < deadline, "no run: {body}\n{}", s.log());
        std::thread::sleep(Duration::from_millis(500));
    }
}
