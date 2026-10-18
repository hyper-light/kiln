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
