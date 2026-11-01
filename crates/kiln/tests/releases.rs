//! Releases by tag and update drafts (docs/ci.md W29-R21, R22), against a server of its own with
//! real `git`: what a push may do to a release's tag in a repository kiln hosts.

#![cfg(unix)]

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use common::{Options, Server, http, start};
use serde_json::json;

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
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": name, "mode": "native" })))?;
    anyhow::ensure!(st == 200, "{name}: {st} {body}");
    let leaf = name.replace('/', "_");
    must(home, dir, &["clone", "-q", &format!("http://x:{}@127.0.0.1:{}/git/{name}.git", s.admin, s.port), &leaf])?;
    let work = dir.join(&leaf);
    std::fs::write(work.join("README.md"), format!("{name}\n"))?;
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
