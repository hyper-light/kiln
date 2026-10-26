//! W28-AC42 to AC44 (docs/ci.md): `supersede: { action: adopt }` on a server. A newer push
//! withdraws the older run's demand instead of cancelling its work: a leg with the same inputs
//! adopts the execution in flight, work no one wants gives way to live work, and adoption holds
//! over a restart.

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

/// `acme/adopt`, a native repository with `crates/` and `ui/`, and a workflow that adopts.
struct Tree {
    home: PathBuf,
    work: PathBuf,
    push: String,
}

impl Tree {
    fn new(dir: &Path, s: &Server) -> anyhow::Result<Tree> {
        let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/adopt", "mode": "native" })))?;
        anyhow::ensure!(st == 200, "{st} {body}");
        let t = Tree { home: dir.join("githome"), work: dir.join("tree"), push: format!("http://x:{}@127.0.0.1:{}/git/acme/adopt.git", s.admin, s.port) };
        for d in [".kiln", "crates", "ui", "docs"] {
            std::fs::create_dir_all(t.work.join(d))?;
        }
        std::fs::create_dir_all(&t.home)?;
        std::fs::create_dir_all(dir.join("marks"))?;
        git(&t.home, &t.work, &["init", "-q", "-b", "main"])?;
        std::fs::write(t.work.join("crates/a.rs"), "fn a() {}\n")?;
        std::fs::write(t.work.join("ui/a.ts"), "export {};\n")?;
        t.commit("README.md", "adopt\n")?;
        until(s, 60, "the first poll", || {
            let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
            Ok(body.contains("\"last_polled_at\":null").then_some(body))
        })?;
        Ok(t)
    }

    /// `path` written with `text`, committed and pushed.
    fn commit(&self, path: &str, text: &str) -> anyhow::Result<()> {
        std::fs::write(self.work.join(path), text)?;
        git(&self.home, &self.work, &["add", "."])?;
        git(&self.home, &self.work, &["commit", "-qm", path])?;
        git(&self.home, &self.work, &["push", "-q", &self.push, "HEAD:main"])?;
        Ok(())
    }
}

/// The workflow: `ui` and `rust` reuse by their own folders, each marking every execution, and a
/// newer push adopts, finishing nothing no one wants (`finish-within: 0s`).
fn workflow(marks: &Path, secs: u32) -> String {
    let m = marks.display();
    format!(
        "on: push\nsupersede: {{ when: [branch], action: adopt, finish-within: 0s }}\njobs:\n  ui:\n    runs-on: local\n    reuse: {{ paths: [ui/**] }}\n    steps:\n      - run: echo \"$GITHUB_SHA\" >> {m}/ui; sleep {secs}\n  rust:\n    runs-on: local\n    reuse: {{ paths: [crates/**] }}\n    steps:\n      - run: echo \"$GITHUB_SHA\" >> {m}/rust; sleep {secs}\n"
    )
}

/// Runs of the workflow, newest first, once there are `n`.
fn runs(s: &Server, n: usize) -> anyhow::Result<Vec<String>> {
    let mut ids = Vec::new();
    until(s, 120, "the runs", || {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/adopt&workflow=.kiln/ci.yaml&limit=10", &s.admin, None)?;
        ids = serde_json::from_str::<Value>(&body)?.get("runs").and_then(Value::as_array).into_iter().flatten().filter_map(|r| r.get("id").and_then(Value::as_str).map(str::to_owned)).collect();
        Ok((ids.len() < n).then_some(body))
    })?;
    Ok(ids)
}

/// A run's leg of `key`: its id, status, wait reason and the leg it reused.
fn leg(s: &Server, run: &str, key: &str) -> anyhow::Result<(String, String, Value, Option<String>)> {
    let (_, detail) = http(s.port, "GET", &format!("/api/ci/runs/{run}"), &s.admin, None)?;
    let detail: Value = serde_json::from_str(&detail)?;
    let l = detail.get("legs").and_then(Value::as_array).into_iter().flatten().find(|l| l.get("key").and_then(Value::as_str) == Some(key)).cloned().unwrap_or_default();
    let text = |k: &str| l.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
    Ok((text("id"), text("status"), l.get("wait_reason").cloned().unwrap_or_default(), l.get("reused_from").and_then(Value::as_str).map(str::to_owned)))
}

fn wait_status(s: &Server, run: &str, key: &str, status: &[&str], secs: u64) -> anyhow::Result<()> {
    until(s, secs, &format!("{run}'s {key} {status:?}"), || {
        let (_, st, why, _) = leg(s, run, key)?;
        Ok((!status.contains(&st.as_str())).then(|| format!("{st} {why}")))
    })
}

fn adopting(s: &Server, run: &str, key: &str) -> anyhow::Result<()> {
    until(s, 120, &format!("{run}'s {key} adopting"), || {
        let (_, st, why, _) = leg(s, run, key)?;
        Ok((why.get("reason").and_then(Value::as_str) != Some("adopted")).then(|| format!("{st} {why}")))
    })
}

fn marks(dir: &Path, job: &str) -> usize {
    std::fs::read_to_string(dir.join("marks").join(job)).map(|t| t.lines().count()).unwrap_or(0)
}

/// W28-AC42: push A, then B touching only `docs/` while A's rust leg runs. B's rust leg adopts
/// A's: one execution, both runs green, A not cancelled, B's leg `reused_from` A's.
#[test]
fn w28_ac42_a_docs_only_push_adopts_the_rust_leg() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { agent: true, ci: "kiln_workflows = true", ..Options::default() })?;
    let tree = Tree::new(dir.path(), &s)?;
    tree.commit(".kiln/ci.yaml", &workflow(&dir.path().join("marks"), 20))?;
    let a = runs(&s, 1)?.first().cloned().unwrap_or_default();
    wait_status(&s, &a, "rust", &["running"], 120)?;
    tree.commit("docs/a.md", "docs only\n")?;
    let b = runs(&s, 2)?.first().cloned().unwrap_or_default();
    adopting(&s, &b, "rust")?;
    // The leg's routing says why.
    let (b_rust, ..) = leg(&s, &b, "rust")?;
    let (_, routing) = http(s.port, "GET", &format!("/api/ci/legs/{b_rust}/routing"), &s.admin, None)?;
    assert!(routing.contains("adopts") && routing.contains("\"demand\""), "{routing}");
    for (run, key) in [(&a, "rust"), (&a, "ui"), (&b, "rust"), (&b, "ui")] {
        wait_status(&s, run, key, &["succeeded"], 180)?;
    }
    let (a_rust, ..) = leg(&s, &a, "rust")?;
    assert_eq!(leg(&s, &b, "rust")?.3.as_deref(), Some(a_rust.as_str()), "B's rust leg is reused from A's");
    assert_eq!((marks(dir.path(), "rust"), marks(dir.path(), "ui")), (1, 1), "each ran once:\n{}", s.log());
    Ok(())
}

/// W28-AC43: B touches `crates/` while A's rust leg runs and both slots are busy. A's rust leg,
/// which no live demand wants, is cancelled for B's, saying why; B's ui leg adopts A's.
#[test]
fn w28_ac43_a_crates_push_frees_what_no_one_wants() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { agent: true, ci: "kiln_workflows = true", ..Options::default() })?;
    let tree = Tree::new(dir.path(), &s)?;
    tree.commit(".kiln/ci.yaml", &workflow(&dir.path().join("marks"), 30))?;
    let a = runs(&s, 1)?.first().cloned().unwrap_or_default();
    wait_status(&s, &a, "rust", &["running"], 120)?;
    wait_status(&s, &a, "ui", &["running"], 120)?;
    tree.commit("crates/b.rs", "fn b() {}\n")?;
    let b = runs(&s, 2)?.first().cloned().unwrap_or_default();
    wait_status(&s, &a, "rust", &["cancelled"], 120)?;
    let (a_rust, ..) = leg(&s, &a, "rust")?;
    let (_, detail) = http(s.port, "GET", &format!("/api/ci/legs/{a_rust}"), &s.admin, None)?;
    assert!(detail.contains("no live demand wants it") && detail.contains("goes back to lane"), "the cancel says why: {detail}");
    for key in ["rust", "ui"] {
        wait_status(&s, &b, key, &["succeeded"], 180)?;
    }
    let (a_ui, ..) = leg(&s, &a, "ui")?;
    assert_eq!(leg(&s, &b, "ui")?.3.as_deref(), Some(a_ui.as_str()), "B's ui leg is reused from A's");
    assert_eq!(marks(dir.path(), "ui"), 1, "the ui ran once:\n{}", s.log());
    Ok(())
}

/// W28-AC44: the server killed while B's rust leg adopts A's. After the restart the work in flight
/// is rebuilt from SQLite, and both legs settle, the work run no more than A's attempts.
#[test]
fn w28_ac44_adoption_holds_over_a_restart() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut s = start(dir.path(), &Options { agent: true, ci: "kiln_workflows = true", ..Options::default() })?;
    let tree = Tree::new(dir.path(), &s)?;
    tree.commit(".kiln/ci.yaml", &workflow(&dir.path().join("marks"), 30))?;
    let a = runs(&s, 1)?.first().cloned().unwrap_or_default();
    wait_status(&s, &a, "rust", &["running"], 120)?;
    tree.commit("docs/a.md", "docs only\n")?;
    let b = runs(&s, 2)?.first().cloned().unwrap_or_default();
    adopting(&s, &b, "rust")?;
    s.restart()?;
    for (run, key) in [(&a, "rust"), (&b, "rust")] {
        wait_status(&s, run, key, &["succeeded"], 240)?;
    }
    let (a_rust, ..) = leg(&s, &a, "rust")?;
    assert_eq!(leg(&s, &b, "rust")?.3.as_deref(), Some(a_rust.as_str()), "B's rust leg still adopts A's:\n{}", s.log());
    assert!(marks(dir.path(), "rust") <= 2, "only A's attempts ran the work:\n{}", s.log());
    Ok(())
}
