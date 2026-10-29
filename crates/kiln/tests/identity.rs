//! A change's identity (docs/ci.md W26-R03, W26-AC08), against a server of its own with real git
//! and no hook installed: a stack pushed with plain git keeps its diffs through an amend, a
//! rebase, a split and a squash; commits carrying jj's and GitButler's `change-id` header (made
//! with `git hash-object`) and Gerrit's `Change-Id` line (kiln's history) find their diffs however
//! their content changes; a trailer and an alias naming different changes are refused, saying
//! why; a commit kiln can't tell is asked about, and `-o change=` answers.

#![cfg(unix)]

mod common;

use std::path::Path;
use std::process::{Command, Stdio};

use common::{Options, Server, http, start};
use serde_json::{Value, json};

/// `git` with no configuration but this test's (and no hook: none is installed), in `dir`.
fn git(home: &Path, dir: &Path, args: &[&str]) -> anyhow::Result<(bool, String)> {
    git_in(home, dir, args, None)
}

fn git_in(home: &Path, dir: &Path, args: &[&str], input: Option<&[u8]>) -> anyhow::Result<(bool, String)> {
    let mut c = Command::new("git");
    c.args(["-c", "user.name=Dev", "-c", "user.email=dev@example.com", "-c", "init.defaultBranch=main", "-c", "credential.helper=", "-c", "commit.gpgsign=false"])
        .args(args)
        .current_dir(dir)
        .env("HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GIT_ASKPASS")
        .env_remove("SSH_ASKPASS")
        .stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = c.spawn()?;
    if let (Some(data), Some(mut stdin)) = (input, child.stdin.take()) {
        std::io::Write::write_all(&mut stdin, data)?;
    }
    let o = child.wait_with_output()?;
    Ok((o.status.success(), format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))))
}

fn must(r: anyhow::Result<(bool, String)>, what: &str) -> anyhow::Result<String> {
    let (ok, out) = r?;
    anyhow::ensure!(ok, "{what}: {out}");
    Ok(out)
}

fn person(s: &Server, name: &str, grants: &[&str]) -> anyhow::Result<String> {
    let (st, body) = http(s.port, "POST", "/api/users", &s.admin, Some(&json!({ "name": name, "grants": grants })))?;
    anyhow::ensure!((200..300).contains(&st), "{name}: {st} {body}");
    let (st, body) = http(s.port, "POST", "/api/tokens", &s.admin, Some(&json!({ "kind": "pat", "user": name, "grants": ["owner"] })))?;
    anyhow::ensure!((200..300).contains(&st), "{name}'s token: {st} {body}");
    serde_json::from_str::<Value>(&body)?.get("token").and_then(Value::as_str).map(str::to_owned).ok_or_else(|| anyhow::anyhow!("no token in {body}"))
}

fn diff(s: &Server, token: &str, name: &str) -> anyhow::Result<Value> {
    let (st, body) = http(s.port, "GET", &format!("/api/diffs/{name}"), token, None)?;
    anyhow::ensure!(st == 200, "{name}: {st} {body}");
    Ok(serde_json::from_str(&body)?)
}

fn text<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or_default()
}

/// Ten lines of `tag`, numbered from `from` (ten lines make a patch whose likeness tells).
fn lines(tag: &str, n: std::ops::Range<u32>) -> String {
    n.map(|k| format!("{tag} line {k}\n")).collect()
}

/// A native repository `acme/app` with a first trunk, cloned with plain git (no hook) on the
/// branch `stack`; the developer's token.
struct Repo {
    _dir: tempfile::TempDir,
    s: Server,
    home: std::path::PathBuf,
    work: std::path::PathBuf,
    dev: String,
}

impl Repo {
    fn new() -> anyhow::Result<Repo> {
        let dir = tempfile::tempdir()?;
        let s = start(dir.path(), &Options::default())?;
        let home = dir.path().join("githome");
        std::fs::create_dir_all(&home)?;
        let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/app", "mode": "native" })))?;
        anyhow::ensure!(st == 200, "native repository: {st} {body}");
        let dev = person(&s, "dev", &["developer@repo:acme/app"])?;
        let url = format!("http://x:{dev}@127.0.0.1:{}/git/acme/app.git", s.port);
        must(git(&home, dir.path(), &["clone", "-q", &url, "work"]), "clone")?;
        let work = dir.path().join("work");
        assert!(!work.join(".git/hooks/commit-msg").exists(), "no hook");
        std::fs::write(work.join("README.md"), "app\n")?;
        must(git(&home, &work, &["add", "."]), "add")?;
        must(git(&home, &work, &["commit", "-qm", "first"]), "commit")?;
        must(git(&home, &work, &["push", "-q", "origin", "main"]), "the first trunk")?;
        must(git(&home, &work, &["checkout", "-qb", "stack"]), "branch")?;
        Ok(Repo { _dir: dir, s, home, work, dev })
    }

    fn g(&self, args: &[&str]) -> anyhow::Result<String> {
        must(git(&self.home, &self.work, args), &args.join(" "))
    }

    /// `file` holding `body`, committed with plain git saying `message`: its sha.
    fn commit(&self, file: &str, body: &str, message: &str) -> anyhow::Result<String> {
        std::fs::write(self.work.join(file), body)?;
        self.g(&["add", file])?;
        self.g(&["commit", "-qm", message])?;
        Ok(self.g(&["rev-parse", "HEAD"])?.trim().to_owned())
    }

    /// `file` holding `body`, committed as jj and GitButler write commits: with a `header` line
    /// (`change-id <id>`), made with `git hash-object`. Its sha; HEAD moves to it.
    fn commit_with_header(&self, file: &str, body: &str, title: &str, header: &str) -> anyhow::Result<String> {
        std::fs::write(self.work.join(file), body)?;
        self.g(&["add", file])?;
        let tree = self.g(&["write-tree"])?.trim().to_owned();
        let parent = self.g(&["rev-parse", "HEAD"])?.trim().to_owned();
        let object = format!("tree {tree}\nparent {parent}\nauthor Dev <dev@example.com> 1793000000 +0000\ncommitter Dev <dev@example.com> 1793000000 +0000\n{header}\n\n{title}\n");
        let sha = must(git_in(&self.home, &self.work, &["hash-object", "-t", "commit", "-w", "--stdin"], Some(object.as_bytes())), "hash-object")?.trim().to_owned();
        self.g(&["reset", "-q", "--hard", &sha])?;
        Ok(sha)
    }

    /// `git push origin HEAD:refs/for/main` with push options `opts`.
    fn push(&self, opts: &[&str]) -> anyhow::Result<(bool, String)> {
        let mut args: Vec<&str> = vec!["push"];
        for o in opts {
            args.extend(["-o", o]);
        }
        args.extend(["origin", "HEAD:refs/for/main"]);
        git(&self.home, &self.work, &args)
    }

    fn submit(&self, opts: &[&str], wants: &[&str]) -> anyhow::Result<String> {
        let out = must(self.push(opts), "submit")?;
        for w in wants {
            anyhow::ensure!(out.contains(w), "{w} in {out}");
        }
        Ok(out)
    }

    fn diff(&self, name: &str) -> anyhow::Result<Value> {
        diff(&self.s, &self.dev, name)
    }
}

/// W26-AC08, the examples of W26-R03: a stack made, amended and rebased with plain git and no
/// hook keeps D1–D3; D2 split in two keeps D2 for one half and says D4 was split from it; the two
/// halves squashed back keep D2, which absorbs D4. No commit ever carries an id.
#[test]
fn w26_ac08_plain_git_amend_rebase_split_squash() -> anyhow::Result<()> {
    let r = Repo::new()?;
    let c1 = r.commit("a.txt", &lines("a", 0..10), "one: a")?;
    let c2 = r.commit("b.txt", &lines("b", 0..10), "two: b")?;
    let c3 = r.commit("c.txt", &lines("c", 0..10), "three: c")?;
    let _ = c1;
    r.submit(&[], &["D1 v1 (new)", "D2 v1 (new)", "D3 v1 (new)"])?;
    assert!(!r.g(&["log", "--format=%B", "main..HEAD"])?.contains("Kiln-Change"), "plain git: no trailer in any commit");
    let d2 = r.diff("D2")?;
    let id = text(&d2, "change").to_owned();
    assert!(id.starts_with("kc-") && text(&d2, "parent") == "D1", "{d2}");

    // An amend of D2 with plain git: D2 v2; D3, restacked on it, unchanged.
    r.g(&["checkout", "-q", &c2])?;
    let amended = lines("b", 0..10).replace("b line 3\n", "b line three\n").replace("b line 4\n", "b line four\n");
    std::fs::write(r.work.join("b.txt"), &amended)?;
    r.g(&["commit", "-q", "--amend", "--no-edit", "-a"])?;
    r.g(&["cherry-pick", &c3])?;
    r.g(&["branch", "-qf", "stack", "HEAD"])?;
    r.g(&["checkout", "-q", "stack"])?;
    r.submit(&[], &["D1 v1 (unchanged)", "D2 v2 (updated)", "D3 v1 (unchanged)"])?;
    assert_eq!(text(&r.diff("D2")?, "change"), id, "the same change");

    // The trunk moves; the stack, rebased with plain git, gets rebase-only versions of D1–D3.
    r.g(&["checkout", "-q", "main"])?;
    r.commit("z.txt", "elsewhere\n", "trunk moves")?;
    must(git(&r.home, &r.work, &["push", "-q", &format!("http://x:{}@127.0.0.1:{}/git/acme/app.git", r.s.admin, r.s.port), "HEAD:main"]), "the owner moves trunk")?;
    r.g(&["checkout", "-q", "stack"])?;
    r.g(&["rebase", "-q", "main"])?;
    r.submit(&[], &["D1 v2 (rebased", "D2 v3 (rebased", "D3 v2 (rebased"])?;

    // D2 split in two: its first seven lines keep D2; the last three are D4, split from D2.
    let top = r.g(&["rev-parse", "HEAD"])?.trim().to_owned();
    let whole = r.g(&["show", "HEAD~1:b.txt"])?;
    let first: String = whole.lines().take(7).map(|l| format!("{l}\n")).collect();
    r.g(&["reset", "-q", "--hard", "HEAD~2"])?;
    r.commit("b.txt", &first, "two: b")?;
    r.commit("b.txt", &whole, "two: b, the rest")?;
    r.g(&["cherry-pick", &top])?;
    r.submit(&[], &["D2 v4 (updated)", "D4 v1 (new, split from D2)", "D3 v3 (moved in the stack)"])?;
    let d4 = r.diff("D4")?;
    assert_eq!((text(&d4, "split_from"), text(&d4, "parent"), text(&r.diff("D2")?, "change")), ("D2", "D2", id.as_str()), "{d4}");
    // Pushed again as it is: each half finds its own diff.
    r.g(&["commit", "-q", "--amend", "--no-edit"])?;
    r.submit(&[], &["D2 v4 (unchanged)", "D4 v1 (unchanged)"])?;

    // The halves squashed back into one commit: D2 keeps the change and absorbs D4.
    let top = r.g(&["rev-parse", "HEAD"])?.trim().to_owned();
    r.g(&["reset", "-q", "--hard", "HEAD~1"])?;
    r.g(&["reset", "-q", "--soft", "HEAD~2"])?;
    r.g(&["commit", "-qm", "two: b"])?;
    r.g(&["cherry-pick", &top])?;
    r.submit(&[], &["D2 v5 (updated; absorbed D4)", "D3 v4 (moved in the stack)"])?;
    let d4 = r.diff("D4")?;
    assert_eq!((text(&d4, "state"), text(&d4, "absorbed_into")), ("abandoned", "D2"), "{d4}");
    assert_eq!(text(&r.diff("D3")?, "parent"), "D2");
    Ok(())
}

/// W26-AC08: commits from jj and GitButler (their `change-id` and `gitbutler-change-id` headers,
/// made with `git hash-object`) and with Gerrit's `Change-Id` line keep their diffs when their
/// content and titles change entirely; a trailer and an alias naming different changes are
/// refused, saying why; a commit kiln can't tell is asked about, and `-o change=` answers.
#[test]
fn w26_ac08_jj_gitbutler_change_id_and_questions() -> anyhow::Result<()> {
    let r = Repo::new()?;
    const JJ: &str = "change-id mlqnqnkrxpuvuuxzlzoltostwlwyskpx";
    const GB: &str = "gitbutler-change-id 96420be4-a3ed-4bea-b534-ff2160cbb848";
    const CID: &str = "I3333333333333333333333333333333333333333";
    let trunk = r.g(&["rev-parse", "HEAD"])?.trim().to_owned();
    r.commit_with_header("a.txt", &lines("a", 0..10), "one: from jj", JJ)?;
    r.commit_with_header("b.txt", &lines("b", 0..10), "two: from GitButler", GB)?;
    r.commit("c.txt", &lines("c", 0..10), &format!("three: with a Change-Id\n\nChange-Id: {CID}"))?;
    r.submit(&[], &["D1 v1 (new)", "D2 v1 (new)", "D3 v1 (new)"])?;
    assert_eq!(text(&r.diff("D3")?, "change_id"), CID, "the Change-Id it carries is kept beside its identity");

    // Rewritten entirely, titles too: only the ids say which diffs they are.
    r.g(&["reset", "-q", "--hard", &trunk])?;
    r.commit_with_header("a.txt", &lines("jj", 0..4), "uno", JJ)?;
    r.commit_with_header("b.txt", &lines("gb", 0..4), "dos", GB)?;
    r.commit("c.txt", &lines("gerrit", 0..4), &format!("tres\n\nChange-Id: {CID}"))?;
    r.submit(&[], &["D1 v2 (updated)  uno", "D2 v2 (updated)  dos", "D3 v2 (updated)  tres"])?;

    // A trailer naming D1 with a Change-Id that is D3's: refused, saying both, nothing kept.
    let d1 = text(&r.diff("D1")?, "change").to_owned();
    let before = r.g(&["rev-parse", "HEAD"])?.trim().to_owned();
    r.commit("d.txt", "d\n", &format!("four\n\nKiln-Change: {d1}\nChange-Id: {CID}"))?;
    let (ok, out) = r.push(&[])?;
    assert!(!ok && out.contains("remote: kiln:") && out.contains("can't name two changes") && out.contains("D1") && out.contains("D3") && out.contains(CID), "{out}");
    assert_eq!(r.diff("D4").ok().map(|d| d.to_string()), None, "nothing kept");
    r.g(&["reset", "-q", "--hard", &before])?;

    // One commit with half of D2's lines and half of D3's: kiln can't tell, and asks.
    r.g(&["reset", "-q", "--hard", "HEAD~2"])?;
    let gb: String = lines("gb", 0..4).lines().take(2).map(|l| format!("{l}\n")).collect();
    let ge: String = lines("gerrit", 0..4).lines().take(2).map(|l| format!("{l}\n")).collect();
    std::fs::write(r.work.join("b.txt"), &gb)?;
    std::fs::write(r.work.join("c.txt"), &ge)?;
    r.g(&["add", "b.txt", "c.txt"])?;
    r.g(&["commit", "-qm", "mixed"])?;
    let short = r.g(&["rev-parse", "--short=7", "HEAD"])?.trim().to_owned();
    let (ok, out) = r.push(&[])?;
    assert!(!ok && out.contains(&format!("kiln can't tell which change {short}")) && out.contains("D2") && out.contains("D3") && out.contains(&format!("change={short}:")), "{out}");
    // The answer settles it.
    r.submit(&[&format!("change={short}:D2")], &["D1 v2 (unchanged)", "D2 v3 (updated)  mixed"])?;
    assert_eq!(text(&r.diff("D3")?, "state"), "needs-review", "D3 is untouched");
    Ok(())
}

/// Every commit kiln makes for a change carries its `Kiln-Change` (W26-R03): submissions without
/// git (each patch given one, one that has a Change-Id kept as it is), and the diff each became
/// is that change.
#[test]
fn w26_r03_commits_kiln_makes_carry_the_trailer() -> anyhow::Result<()> {
    let r = Repo::new()?;
    let patch = |file: &str, body: &str| format!("diff --git a/{file} b/{file}\nnew file mode 100644\n--- /dev/null\n+++ b/{file}\n@@ -0,0 +1 @@\n+{body}\n");
    let body = json!({ "patches": [
        { "message": "one: by an agent\n", "diff": patch("p.txt", "p") },
        { "message": "two: an old client's\n\nChange-Id: I4444444444444444444444444444444444444444\n", "diff": patch("q.txt", "q") },
    ] });
    let (st, out) = http(r.s.port, "POST", "/api/repos/acme%2Fapp/submit", &r.dev, Some(&body))?;
    anyhow::ensure!(st == 200, "submit: {st} {out}");
    r.g(&["fetch", "-q", "origin", "refs/kiln/diffs/D2/v1"])?;
    let log = r.g(&["log", "--format=%B%x1e", "-2", "FETCH_HEAD"])?;
    let msgs: Vec<&str> = log.split('\x1e').map(str::trim).filter(|m| !m.is_empty()).collect();
    let (two, one) = (msgs.first().copied().unwrap_or_default(), msgs.get(1).copied().unwrap_or_default());
    let d1 = r.diff("D1")?;
    assert!(one.contains(&format!("Kiln-Change: {}", text(&d1, "change"))), "{one}\n{d1}");
    assert!(two.contains("Change-Id: I4444") && !two.contains("Kiln-Change"), "a Change-Id says which change it is already: {two}");
    Ok(())
}
