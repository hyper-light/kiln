//! Keeping open changes fresh as main moves (docs/ci.md W26-R16, AC07), against a server of its own
//! with real `git`: when main moves, each open change is replayed onto it in kiln's own copy, stack
//! by stack, bottom first; a clean replay is a rebase-only revision of kiln's (its why `replayed`,
//! its author and message kept, kiln its committer), so approvals stay; a conflicting one is kept as
//! W26-R12's conflicts are and the changes above it wait; and no branch moves.

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

fn rev(bare: &Path, args: &[&str]) -> anyhow::Result<String> {
    let o = Command::new("git").arg("-C").arg(bare).args(args).env("GIT_CONFIG_NOSYSTEM", "1").output()?;
    anyhow::ensure!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
    Ok(String::from_utf8_lossy(&o.stdout).trim().to_owned())
}

/// Every branch of the bare repository and where it is.
fn branches(bare: &Path) -> anyhow::Result<String> {
    rev(bare, &["for-each-ref", "--format=%(refname) %(objectname)", "refs/heads/"])
}

fn person(s: &Server, name: &str, grants: &[&str]) -> anyhow::Result<String> {
    let (st, body) = http(s.port, "POST", "/api/users", &s.admin, Some(&json!({ "name": name, "grants": grants })))?;
    anyhow::ensure!((200..300).contains(&st), "{name}: {st} {body}");
    let (st, text) = http(s.port, "POST", "/api/tokens", &s.admin, Some(&json!({ "kind": "pat", "user": name, "grants": ["owner"] })))?;
    anyhow::ensure!((200..300).contains(&st), "token: {st} {text}");
    serde_json::from_str::<Value>(&text)?.get("token").and_then(Value::as_str).map(str::to_owned).ok_or_else(|| anyhow::anyhow!("no token in {text}"))
}

fn ok(s: &Server, token: &str, method: &str, path: &str, body: Option<&Value>) -> anyhow::Result<Value> {
    let (st, text) = http(s.port, method, path, token, body)?;
    anyhow::ensure!(st == 200, "{method} {path}: {st} {text}");
    Ok(serde_json::from_str(&text)?)
}

fn text<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or_default()
}

/// Polls `f` until it says yes, for up to `secs` seconds.
fn until(secs: u64, what: &str, mut f: impl FnMut() -> anyhow::Result<Option<String>>) -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        match f()? {
            None => return Ok(()),
            Some(last) => anyhow::ensure!(Instant::now() < deadline, "{what}: {last}"),
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn hosted(s: &Server) -> anyhow::Result<std::path::PathBuf> {
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/app", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "native repository: {st} {body}");
    let repo: Value = serde_json::from_str(&body)?;
    let url = text(&repo, "clone_url");
    Ok(std::path::PathBuf::from(url.strip_prefix("file://").unwrap_or(url)))
}

fn write(work: &Path, files: &[(&str, &[u8])]) -> anyhow::Result<()> {
    for (f, bytes) in files {
        if let Some(dir) = work.join(f).parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(work.join(f), bytes)?;
    }
    Ok(())
}

/// A change's versions (`/api/diffs/D1`, `/api/change-requests/CR1`).
fn versions(s: &Server, token: &str, change: &str) -> anyhow::Result<Vec<Value>> {
    Ok(ok(s, token, "GET", &format!("{change}/versions"), None)?.get("versions").and_then(Value::as_array).cloned().unwrap_or_default())
}

/// A server, a native repository with `files` on main, dev's clone of it and the owner's.
struct Setup {
    _dir: tempfile::TempDir,
    s: Server,
    home: std::path::PathBuf,
    bare: std::path::PathBuf,
    dev: String,
    kim: String,
    work: std::path::PathBuf,
    owner: std::path::PathBuf,
}

fn setup(files: &[(&str, &[u8])]) -> anyhow::Result<Setup> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options::default())?;
    let home = dir.path().join("githome");
    std::fs::create_dir_all(&home)?;
    let url = |token: &str| format!("http://x:{token}@127.0.0.1:{}/git/acme/app.git", s.port);
    let bare = hosted(&s)?;
    let dev = person(&s, "dev", &["developer@repo:acme/app"])?;
    let kim = person(&s, "kim", &["developer@repo:acme/app"])?;
    must(git(&home, dir.path(), &["clone", "-q", &url(&dev), "work"]), "clone")?;
    let work = dir.path().join("work");
    let mut all: Vec<(&str, &[u8])> = vec![(".kiln/review.yaml", b"land:\n  approvals: 1\n  resolve-threads: false\n")];
    all.extend_from_slice(files);
    write(&work, &all)?;
    must(git(&home, &work, &["add", "-A"]), "add")?;
    must(git(&home, &work, &["commit", "-qm", "first"]), "commit")?;
    must(git(&home, &work, &["push", "-q", "origin", "main"]), "the first trunk")?;
    must(git(&home, dir.path(), &["clone", "-q", &url(&s.admin), "owner"]), "the owner's clone")?;
    let owner = dir.path().join("owner");
    Ok(Setup { _dir: dir, s, home, bare, dev, kim, work, owner })
}

impl Setup {
    /// One commit of `files` on dev's work tree.
    fn commit(&self, files: &[(&str, &[u8])], message: &str) -> anyhow::Result<()> {
        write(&self.work, files)?;
        must(git(&self.home, &self.work, &["add", "-A"]), "add")?;
        must(git(&self.home, &self.work, &["commit", "-qm", message]), "commit").map(|_| ())
    }

    /// Main moves: the owner commits `files` and pushes. Main's new tip.
    fn main_moves(&self, files: &[(&str, &[u8])], message: &str) -> anyhow::Result<String> {
        must(git(&self.home, &self.owner, &["pull", "-q", "--rebase", "origin", "main"]), "pull")?;
        write(&self.owner, files)?;
        must(git(&self.home, &self.owner, &["add", "-A"]), "add")?;
        must(git(&self.home, &self.owner, &["commit", "-qm", message]), "commit")?;
        must(git(&self.home, &self.owner, &["push", "-q", "origin", "HEAD:main"]), "main moves")?;
        rev(&self.bare, &["rev-parse", "refs/heads/main"])
    }
}

/// W26-AC07 and W26-R16 on a 3-diff stack (D1 ← D2 ← D3, kim's approval on D2), a change request
/// (CR4, branch topic), and a 2-diff stack whose bottom conflicts with main (D5 ← D6). Main moves
/// once. D1, D2 and D3 each get v2: rebase-only, why `replayed`, pushed by kiln, each on the one
/// below's replay, D1 on main's tip, their author and message kept and kiln their committer, kept at
/// `refs/kiln/diffs/<D>/v2`; D2 stays accepted. CR4 gets v2, topic merged with main, its branch not
/// moved. D5's replay conflicts: it's kept as v2 with its markers (its gate holding) and D6 still
/// moves, onto it; once D5's conflict is resolved and committed, D6 moves onto the resolution. No
/// branch but main moved.
#[test]
fn w26_ac07_main_moves_under_stacks_and_a_change_request() -> anyhow::Result<()> {
    let x = setup(&[("a.txt", b"a\n"), ("b.txt", b"b\n"), ("c.txt", b"c\n"), ("t.txt", b"t\n"), ("x.txt", b"x\n"), ("main.txt", b"main\n")])?;
    let s = &x.s;

    // The stack, D1 ← D2 ← D3.
    x.commit(&[("a.txt", b"a, D1\n")], "D1: a")?;
    x.commit(&[("b.txt", b"b, D2\n")], "D2: b")?;
    x.commit(&[("c.txt", b"c, D3\n")], "D3: c")?;
    let out = must(git(&x.home, &x.work, &["push", "origin", "HEAD:refs/for/main"]), "the stack")?;
    assert!(out.contains("D1") && out.contains("D3"), "{out}");
    ok(s, &x.kim, "POST", "/api/diffs/D2/review", Some(&json!({ "action": "accept" })))?;
    assert_eq!(text(&ok(s, &x.kim, "GET", "/api/diffs/D2", None)?, "state"), "accepted");

    // The change request, CR4.
    must(git(&x.home, &x.work, &["checkout", "-qb", "topic", "origin/main"]), "branch")?;
    x.commit(&[("t.txt", b"t, topic\n")], "topic work")?;
    let out = must(git(&x.home, &x.work, &["push", "origin", "topic:refs/for/main/branch/topic"]), "the change request")?;
    assert!(out.contains("CR4"), "{out}");

    // The stack whose bottom conflicts with main, D5 ← D6.
    must(git(&x.home, &x.work, &["checkout", "-qb", "clash", "origin/main"]), "branch")?;
    x.commit(&[("x.txt", b"x, D5\n")], "D5: x")?;
    x.commit(&[("d6.txt", b"D6\n")], "D6: a file of its own")?;
    let out = must(git(&x.home, &x.work, &["push", "origin", "HEAD:refs/for/main"]), "the clashing stack")?;
    assert!(out.contains("D5") && out.contains("D6"), "{out}");
    let before = branches(&x.bare)?;

    // Main moves.
    let tip = x.main_moves(&[("main.txt", b"main moved\n"), ("x.txt", b"x, main\n")], "main moves")?;
    let replayed = |change: &str| -> anyhow::Result<Option<Value>> { Ok(versions(s, &x.kim, change)?.into_iter().find(|v| v.get("n") == Some(&json!(2)))) };
    until(60, "D1, D2, D3 and CR4 replayed", || {
        for c in ["/api/diffs/D1", "/api/diffs/D2", "/api/diffs/D3", "/api/change-requests/CR4"] {
            if replayed(c)?.is_none() {
                return Ok(Some(format!("{c} has no v2: {:?}", versions(s, &x.kim, c)?)));
            }
        }
        Ok(None)
    })?;

    let mut below = tip.clone();
    for (d, file) in [("D1", "a"), ("D2", "b"), ("D3", "c")] {
        let v = replayed(&format!("/api/diffs/{d}"))?.unwrap_or_default();
        assert_eq!((text(&v, "why"), v.get("rebase_only"), text(&v, "pushed_by"), text(&v, "base")), ("replayed", Some(&json!(true)), "kiln", tip.as_str()), "{d}: {v}");
        assert_eq!(text(&v, "parent"), below, "{d} goes onto the one below's replay: {v}");
        assert_eq!(v.get("checks_from"), Some(&json!(1)), "{d}'s checks are v1's, not its own: {v}");
        let commit = text(&v, "commit").to_owned();
        assert_eq!(rev(&x.bare, &["rev-parse", &format!("refs/kiln/diffs/{d}/v2")])?, commit, "{d}'s replay is kept at its ref");
        let who = rev(&x.bare, &["log", "-1", "--format=%an <%ae>|%cn|%s", &commit])?;
        assert_eq!(who, format!("Test <test@example.com>|kiln|{d}: {file}"), "{d}: its author and message kept, kiln its committer");
        below = commit;
    }
    // A rebase-only revision keeps its approval.
    assert_eq!(text(&ok(s, &x.kim, "GET", "/api/diffs/D2", None)?, "state"), "accepted");

    // CR4: topic merged with main as a revision; its branch didn't move.
    let v = replayed("/api/change-requests/CR4")?.unwrap_or_default();
    assert_eq!((text(&v, "why"), v.get("rebase_only")), ("replayed", Some(&json!(true))), "{v}");
    let parents = rev(&x.bare, &["log", "-1", "--format=%P", text(&v, "commit")])?;
    assert_eq!(parents, format!("{} {tip}", rev(&x.bare, &["rev-parse", "refs/heads/topic"])?), "{v}");

    // D5 conflicts: kept as a revision with its markers, its gate holding; D6 above it still moves,
    // onto that revision.
    until(30, "D5's conflicted replay", || {
        let got = ok(s, &x.kim, "GET", "/api/diffs/D5/conflicts", None)?;
        Ok((got.pointer("/replay/state") != Some(&json!("conflicted"))).then(|| got.to_string()))
    })?;
    let d5 = replayed("/api/diffs/D5")?.unwrap_or_default();
    assert_eq!((text(&d5, "why"), d5.get("rebase_only")), ("replayed with conflicts", Some(&json!(false))), "{d5}");
    let d6 = replayed("/api/diffs/D6")?.unwrap_or_default();
    assert_eq!((text(&d6, "why"), text(&d6, "parent")), ("replayed", text(&d5, "commit")), "D6 moves onto D5's conflicted revision: {d6}");
    let gate = ok(s, &x.kim, "GET", "/api/diffs/D5/comments", None)?;
    assert_eq!(gate.pointer("/gate/verdict"), Some(&json!("hold")), "{gate}");

    // How fresh each is, in words, as the page and row say it; Update's state and why not.
    let short = tip.get(..6).unwrap_or_default();
    let fresh = |c: &str| -> anyhow::Result<Value> { Ok(ok(s, &x.kim, "GET", c, None)?.get("fresh").cloned().unwrap_or_default()) };
    let d1 = fresh("/api/diffs/D1")?;
    assert_eq!((text(&d1, "words"), d1.pointer("/update/can"), d1.pointer("/checks_from")), (format!("replays clean on main@{short}").as_str(), Some(&json!(false)), Some(&json!(1))), "{d1}");
    assert_eq!(d1.pointer("/update/why_not"), Some(&json!("a diff has no branch: its replay is its latest revision")), "{d1}");
    assert_eq!(text(&fresh("/api/diffs/D5")?, "words"), format!("conflicts on main@{short}"));
    assert_eq!(text(&fresh("/api/diffs/D6")?, "words"), format!("replays clean on D5's conflicts on main@{short}"));
    let cr4 = fresh("/api/change-requests/CR4")?;
    assert_eq!((cr4.pointer("/update/can"), text(&cr4, "words")), (Some(&json!(true)), format!("replays clean on main@{short}").as_str()), "{cr4}");

    // D5's conflict resolved and committed: D6 is replayed onto the resolution.
    let got = ok(s, &x.dev, "GET", "/api/diffs/D5/conflicts", None)?;
    let id = got.pointer("/conflicts/0/id").and_then(Value::as_str).unwrap_or_default().to_owned();
    ok(s, &x.dev, "POST", &format!("/api/diffs/D5/conflicts/{id}/resolution"), Some(&json!({ "take": "this" })))?;
    let r = ok(s, &x.dev, "POST", "/api/diffs/D5/conflicts/commit", Some(&json!({})))?;
    let resolved = text(&r, "commit").to_owned();
    assert_eq!((text(&r, "why"), r.get("version")), ("conflict resolved", Some(&json!(3))), "{r}");
    assert_eq!(rev(&x.bare, &["log", "-1", "--format=%P", &resolved])?, tip, "the resolution goes onto main's tip");
    until(60, "D6 replayed onto D5's resolution", || {
        let v = versions(s, &x.kim, "/api/diffs/D6")?.pop().unwrap_or_default();
        Ok((text(&v, "parent") != resolved).then(|| v.to_string()))
    })?;
    let gate = ok(s, &x.kim, "GET", "/api/diffs/D5/comments", None)?;
    assert_ne!(gate.pointer("/gate/verdict"), Some(&json!("hold")), "{gate}");

    // No branch moved but main.
    let after = branches(&x.bare)?;
    let unmoved = |list: &str| list.lines().filter(|l| !l.starts_with("refs/heads/main ")).map(str::to_owned).collect::<Vec<_>>();
    assert_eq!(unmoved(&after), unmoved(&before));
    Ok(())
}

/// W26-AC07's measure: a 10-diff stack's replayed revisions are recorded within 60 s of main moving,
/// p95 over 20 moves (the test's clock, on real git).
#[test]
fn w26_ac07_ten_diff_stack_within_60s() -> anyhow::Result<()> {
    let files: Vec<(String, Vec<u8>)> = (1..=10).map(|i| (format!("f{i}.txt"), format!("{i}\n").into_bytes())).collect();
    let refs: Vec<(&str, &[u8])> = files.iter().map(|(f, b)| (f.as_str(), b.as_slice())).collect();
    let x = setup(&refs)?;
    for i in 1..=10 {
        x.commit(&[(&format!("f{i}.txt"), format!("{i}, changed\n").as_bytes())], &format!("change {i}"))?;
    }
    must(git(&x.home, &x.work, &["push", "origin", "HEAD:refs/for/main"]), "the stack")?;
    let mut took = Vec::new();
    for m in 1..=20 {
        let start = Instant::now();
        let tip = x.main_moves(&[("moves.txt", format!("{m}\n").as_bytes())], &format!("move {m}"))?;
        until(120, "the stack replayed", || {
            let top = versions(&x.s, &x.kim, "/api/diffs/D10")?.pop().unwrap_or_default();
            Ok((text(&top, "base") != tip).then(|| top.to_string()))
        })?;
        took.push(start.elapsed());
    }
    took.sort();
    let p95 = took.get(took.len() * 95 / 100 - 1).copied().unwrap_or_default();
    eprintln!("W26-AC07: 10-diff stack replayed after main moved: p95 {p95:?} over {} moves (max {:?})", took.len(), took.last());
    assert!(p95 <= Duration::from_secs(60), "p95 {p95:?}");
    Ok(())
}

/// W26-R16 with W26-R03: once kiln has replayed a stack onto the moved trunk, its author's own
/// rebase onto the same tip is the same patches on the same base, found to be the replays: nothing
/// new. (A repository with `fresh: replay: false` sees the author's rebase as rebase-only versions:
/// hosting's w14_ac01_stacks and identity's w26_ac08.)
#[test]
fn w26_r16_a_hand_rebase_after_the_replay_reads_unchanged() -> anyhow::Result<()> {
    let x = setup(&[("a.txt", b"a\n"), ("b.txt", b"b\n")])?;
    x.commit(&[("a.txt", b"a, D1\n")], "D1: a")?;
    x.commit(&[("b.txt", b"b, D2\n")], "D2: b")?;
    must(git(&x.home, &x.work, &["push", "origin", "HEAD:refs/for/main"]), "the stack")?;
    x.main_moves(&[("z.txt", b"elsewhere\n")], "main moves")?;
    until(60, "D1 and D2 replayed", || {
        let n = versions(&x.s, &x.kim, "/api/diffs/D2")?.len();
        Ok((n < 2).then(|| format!("D2 has {n} versions")))
    })?;
    must(git(&x.home, &x.work, &["pull", "-q", "--rebase", "origin", "main"]), "the author's own rebase")?;
    let out = must(git(&x.home, &x.work, &["push", "origin", "HEAD:refs/for/main"]), "the rebase")?;
    for want in ["D1 v2 (unchanged)", "D2 v2 (unchanged)"] {
        assert!(out.contains(want), "{want} in {out}");
    }
    assert_eq!(versions(&x.s, &x.kim, "/api/diffs/D2")?.len(), 2);
    Ok(())
}

fn call(s: &Server, token: &str, method: &str, path: &str, body: Option<&Value>) -> anyhow::Result<(u16, Value)> {
    let (st, text) = http(s.port, method, path, token, body)?;
    Ok((st, serde_json::from_str(&text).unwrap_or(Value::String(text))))
}

/// A change request (CR1, branch topic) whose replay onto a moved main is its v2: the head of its
/// branch, and v2's commit. `review` is the trunk's `.kiln/review.yaml`.
fn replayed_request(review: &[u8]) -> anyhow::Result<(Setup, String)> {
    let x = setup(&[("t.txt", b"t\n"), (".kiln/review.yaml", review)])?;
    must(git(&x.home, &x.work, &["checkout", "-qb", "topic"]), "branch")?;
    x.commit(&[("t.txt", b"t, topic\n")], "topic work")?;
    let out = must(git(&x.home, &x.work, &["push", "origin", "topic:refs/for/main/branch/topic"]), "the change request")?;
    anyhow::ensure!(out.contains("CR1"), "{out}");
    x.main_moves(&[("z.txt", b"elsewhere\n")], "main moves")?;
    let mut replay = String::new();
    until(60, "CR1 replayed", || {
        let v = versions(&x.s, &x.kim, "/api/change-requests/CR1")?.pop().unwrap_or_default();
        replay = text(&v, "commit").to_owned();
        Ok((text(&v, "why") != "replayed").then(|| v.to_string()))
    })?;
    Ok((x, replay))
}

/// W26-R16, AC07: a branch moves only when someone presses Update. CR1's replay leaves topic where it
/// was; Update moves it to the replay, with a lease (pressed again, it says it's there already); a
/// diff has no branch to move; and a branch pushed since its replay is refused, saying why.
#[test]
fn w26_r16_update_moves_a_branch_only_when_asked() -> anyhow::Result<()> {
    let (x, replay) = replayed_request(b"land:\n  approvals: 1\n")?;
    let s = &x.s;
    let topic = |x: &Setup| rev(&x.bare, &["rev-parse", "refs/heads/topic"]);
    let head = topic(&x)?;
    assert_ne!(head, replay, "the replay didn't move topic");
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(topic(&x)?, head, "nor does anything later, unasked");
    let r = ok(s, &x.kim, "POST", "/api/change-requests/CR1/update", Some(&json!({})))?;
    assert_eq!((text(&r, "branch"), text(&r, "head")), ("topic", replay.as_str()), "{r}");
    assert_eq!(topic(&x)?, replay, "Update moved topic to the replay");
    let (st, r) = call(s, &x.kim, "POST", "/api/change-requests/CR1/update", Some(&json!({})))?;
    assert!(st == 409 && r.to_string().contains("at CR1's replay already"), "{st} {r}");

    // A diff has no branch.
    must(git(&x.home, &x.work, &["checkout", "-q", "-B", "d", "origin/main"]), "branch")?;
    x.commit(&[("d.txt", b"d\n")], "a diff")?;
    must(git(&x.home, &x.work, &["push", "origin", "HEAD:refs/for/main"]), "D2")?;
    let main = x.main_moves(&[("z.txt", b"moved again\n")], "main moves again")?;
    until(60, "D2 replayed", || {
        let v = versions(s, &x.kim, "/api/diffs/D2")?.pop().unwrap_or_default();
        Ok((text(&v, "base") != main).then(|| v.to_string()))
    })?;
    let (st, r) = call(s, &x.kim, "POST", "/api/diffs/D2/update", Some(&json!({})))?;
    assert!(st == 409 && r.to_string().contains("has no branch to move"), "{st} {r}");

    // CR1 replayed again onto that move; then its author pushes to topic before anyone presses
    // Update: Update is refused (the branch moved, or the push is CR1's latest revision now) and the
    // author's push stands.
    until(60, "CR1 replayed again", || {
        let v = versions(s, &x.kim, "/api/change-requests/CR1")?.pop().unwrap_or_default();
        Ok((text(&v, "commit") == replay).then(|| v.to_string()))
    })?;
    must(git(&x.home, &x.work, &["fetch", "-q", "origin"]), "fetch")?;
    must(git(&x.home, &x.work, &["checkout", "-q", "-B", "topic2", "origin/topic"]), "topic")?;
    x.commit(&[("t.txt", b"t, topic, more\n")], "more topic work")?;
    let pushed = must(git(&x.home, &x.work, &["rev-parse", "HEAD"]), "rev")?.trim().to_owned();
    must(git(&x.home, &x.work, &["push", "-q", "origin", "HEAD:topic"]), "push topic")?;
    let (st, r) = call(s, &x.kim, "POST", "/api/change-requests/CR1/update", Some(&json!({})))?;
    assert!(st == 409 && (r.to_string().contains("topic moved since CR1 was replayed") || r.to_string().contains("isn't a replay")), "{st} {r}");
    assert_eq!(topic(&x)?, pushed, "the author's push stands");
    Ok(())
}

/// W26-R16, AC07: with `fresh.push: auto`, a change request's replay is pushed to its branch as
/// main moves, and says so in the audit.
#[test]
fn w26_r16_auto_push_only_when_declared() -> anyhow::Result<()> {
    let (x, replay) = replayed_request(b"land:\n  approvals: 1\nfresh:\n  push: auto\n")?;
    until(30, "topic pushed automatically", || {
        let at = rev(&x.bare, &["rev-parse", "refs/heads/topic"])?;
        Ok((at != replay).then(|| format!("topic at {at}, the replay {replay}")))
    })?;
    Ok(())
}

/// W26-R16: a change that turns automatic pushes on warns before it's approved, in plain words:
/// every open change's branch pushed each time main moves, the API use and CI runs on GitHub,
/// GitLab and Bitbucket, and pulling or rebasing before pushing.
#[test]
fn w26_r16_turning_auto_push_on_warns() -> anyhow::Result<()> {
    let x = setup(&[])?;
    x.commit(&[(".kiln/review.yaml", b"land:\n  approvals: 1\n  resolve-threads: false\nfresh:\n  push: auto\n")], "fresh: push replays automatically")?;
    must(git(&x.home, &x.work, &["push", "origin", "HEAD:refs/for/main"]), "D1")?;
    let (st, r) = call(&x.s, &x.kim, "POST", "/api/diffs/D1/review", Some(&json!({ "action": "accept" })))?;
    let said = r.to_string();
    assert_eq!(st, 409, "{r}");
    for words in ["every open change's branch is pushed each time main moves", "GitHub, GitLab and Bitbucket", "heavy API use", "many more CI runs", "pull or rebase before they push", "approve anyway with `force`"] {
        assert!(said.contains(words), "{words}: {said}");
    }
    ok(&x.s, &x.kim, "POST", "/api/diffs/D1/review", Some(&json!({ "action": "accept", "force": true })))?;
    Ok(())
}

/// W26-R16's collision radar: two open changes from different stacks that change the same lines of
/// a file are named to each other now, not once one lands (`GET /api/diffs/:id/collisions`); one
/// stacked on the other isn't a collision; a change that moves off those lines is no longer one.
#[test]
fn w26_r16_collision_radar_names_both_changes() -> anyhow::Result<()> {
    let body: String = (1..=80).map(|i| format!("line {i}\n")).collect();
    let x = setup(&[("sched/fair.rs", body.as_bytes())])?;
    let s = &x.s;
    let edit = |from: u32, to: u32, tag: &str| -> String { (1..=80).map(|i| if (from..=to).contains(&i) { format!("line {i}, {tag}\n") } else { format!("line {i}\n") }).collect() };
    // D1 changes lines 40–58; D2, on top of it, too (one stack: no collision).
    x.commit(&[("sched/fair.rs", edit(40, 58, "D1").as_bytes())], "D1: fair")?;
    x.commit(&[("other.rs", b"x\n")], "D2: other, above D1")?;
    must(git(&x.home, &x.work, &["push", "origin", "HEAD:refs/for/main"]), "D1 and D2")?;
    // D3, its own stack, changes 45–70.
    must(git(&x.home, &x.work, &["checkout", "-qb", "d3", "origin/main"]), "branch")?;
    x.commit(&[("sched/fair.rs", edit(45, 70, "D3").as_bytes())], "D3: fair too")?;
    let sha = must(git(&x.home, &x.work, &["rev-parse", "HEAD"]), "rev")?.trim().to_owned();
    must(git(&x.home, &x.work, &["push", "-o", &format!("change={sha}:new"), "origin", "HEAD:refs/for/main"]), "D3")?;
    let listed = |d: &str| -> anyhow::Result<Vec<Value>> { Ok(ok(s, &x.kim, "GET", &format!("/api/diffs/{d}/collisions"), None)?.get("collisions").and_then(Value::as_array).cloned().unwrap_or_default()) };
    until(30, "D1 and D3 named to each other", || {
        let one = listed("D1")?;
        Ok((one.len() != 1).then(|| format!("{one:?}")))
    })?;
    let one = listed("D1")?.pop().unwrap_or_default();
    assert_eq!((text(&one, "with"), text(&one, "file"), one.get("start"), one.get("end")), ("D3", "sched/fair.rs", Some(&json!(45)), Some(&json!(58))), "{one}");
    let three = listed("D3")?;
    assert_eq!(three.iter().map(|c| text(c, "with")).collect::<Vec<_>>(), ["D1"], "{three:?}");
    assert!(listed("D2")?.is_empty(), "D2 is in D1's stack and touches other lines");
    // D3 moves off those lines: no longer a collision.
    write(&x.work, &[("sched/fair.rs", edit(75, 80, "D3").as_bytes())])?;
    must(git(&x.home, &x.work, &["commit", "-q", "--amend", "-a", "--no-edit"]), "amend")?;
    let sha = must(git(&x.home, &x.work, &["rev-parse", "HEAD"]), "rev")?.trim().to_owned();
    must(git(&x.home, &x.work, &["push", "-o", &format!("change={sha}:D3"), "origin", "HEAD:refs/for/main"]), "D3 again")?;
    until(30, "D1 and D3 no longer named", || {
        let one = listed("D1")?;
        Ok((!one.is_empty()).then(|| format!("{one:?}")))
    })?;
    Ok(())
}
