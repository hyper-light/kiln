//! The conflict ladder (docs/ci.md W26-R17), against a server of its own with real `git` and its
//! own runner: a resolution a person accepted once is offered the next time the same conflict
//! comes, on another change, once its tests and then the resolutions together passed; a person
//! accepts it and it's the change's new version. A remembered resolution whose tests fail is kept
//! as failed, said, and the conflict goes on up the ladder (here, to the person).

#![cfg(unix)]

mod common;

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use common::{Options, Server, http, start};
use serde_json::{Value, json};

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

fn show(bare: &Path, rev: &str, file: &str) -> anyhow::Result<String> {
    let o = Command::new("git").arg("-C").arg(bare).args(["show", &format!("{rev}:{file}")]).env("GIT_CONFIG_NOSYSTEM", "1").output()?;
    anyhow::ensure!(o.status.success(), "{rev}:{file}: {}", String::from_utf8_lossy(&o.stderr));
    Ok(String::from_utf8_lossy(&o.stdout).into_owned())
}

fn person(s: &Server, name: &str, grants: &[&str]) -> anyhow::Result<String> {
    let (st, body) = http(s.port, "POST", "/api/users", &s.admin, Some(&json!({ "name": name, "grants": grants })))?;
    anyhow::ensure!((200..300).contains(&st), "{name}: {st} {body}");
    let (st, text) = http(s.port, "POST", "/api/tokens", &s.admin, Some(&json!({ "kind": "pat", "user": name, "grants": ["owner"] })))?;
    anyhow::ensure!((200..300).contains(&st), "token: {st}");
    serde_json::from_str::<Value>(&text)?.get("token").and_then(Value::as_str).map(str::to_owned).ok_or_else(|| anyhow::anyhow!("no token in {text}"))
}

fn call(s: &Server, token: &str, method: &str, path: &str, body: Option<&Value>) -> anyhow::Result<(u16, Value)> {
    let (st, text) = http(s.port, method, path, token, body)?;
    Ok((st, serde_json::from_str(&text).unwrap_or(Value::String(text))))
}

fn ok(s: &Server, token: &str, method: &str, path: &str, body: Option<&Value>) -> anyhow::Result<Value> {
    let (st, v) = call(s, token, method, path, body)?;
    anyhow::ensure!(st == 200, "{method} {path}: {st} {v}");
    Ok(v)
}

fn text<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or_default()
}

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

/// A Rust file of three functions, `one`'s first line as `value` says.
fn expand(value: &str) -> String {
    format!("// plan/expand.rs\nfn one() {{\n    let a = {value};\n    more();\n}}\n\nfn two() {{\n    let b = 2;\n}}\n\nfn three() {{\n    let c = 3;\n}}\n")
}

/// The change's tests: fetched through kiln's git, and refused where one's first line is 99.
const GATE: &str = r#"name: gate
on: [merge_group]
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
      - run: "! grep -q 'let a = 99;' plan/expand.rs"
"#;

/// A trunk with the gate, two changes that each make one()'s line `this`, and main making it
/// `main`: both changes' replays conflict in the same hunk. The bare repository, and the tokens of
/// dev (the changes' author) and kim (a developer).
struct World {
    s: Server,
    bare: std::path::PathBuf,
    dev: String,
    kim: String,
}

fn world(dir: &Path) -> anyhow::Result<World> {
    let s = start(dir, &Options { agent: true, ..Options::default() })?;
    let home = dir.join("githome");
    std::fs::create_dir_all(&home)?;
    let url = |token: &str| format!("http://x:{token}@127.0.0.1:{}/git/acme/app.git", s.port);
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/app", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "native repository: {st} {body}");
    let repo: Value = serde_json::from_str(&body)?;
    let bare = std::path::PathBuf::from(text(&repo, "clone_url").trim_start_matches("file://"));
    let dev = person(&s, "dev", &["developer@repo:acme/app"])?;
    let kim = person(&s, "kim", &["developer@repo:acme/app"])?;

    must(git(&home, dir, &["clone", "-q", &url(&dev), "work"]), "clone")?;
    let work = dir.join("work");
    for (f, body) in [(".kiln/review.yaml", "land:\n  approvals: 1\n  resolve-threads: false\n".to_owned()), (".github/workflows/gate.yml", GATE.to_owned()), ("plan/expand.rs", expand("1")), ("README.md", "app\n".to_owned())] {
        if let Some(d) = work.join(f).parent() {
            std::fs::create_dir_all(d)?;
        }
        std::fs::write(work.join(f), body)?;
    }
    must(git(&home, &work, &["add", "-A"]), "add")?;
    must(git(&home, &work, &["commit", "-qm", "first"]), "commit")?;
    must(git(&home, &work, &["push", "-q", "origin", "main"]), "the first trunk")?;
    let trunk0 = must(git(&home, &work, &["rev-parse", "HEAD"]), "rev-parse")?.trim().to_owned();

    // D1 and D2, each from the first trunk, each making one()'s line 10 (D2 a README line too).
    for (i, extra) in [(1, None), (2, Some("app, said by D2\n"))] {
        must(git(&home, &work, &["checkout", "-q", "-B", &format!("d{i}"), &trunk0]), "checkout")?;
        std::fs::write(work.join("plan/expand.rs"), expand("10"))?;
        if let Some(e) = extra {
            std::fs::write(work.join("README.md"), e)?;
        }
        must(git(&home, &work, &["commit", "-qam", &format!("D{i}: one is ten\n\nChange-Id: I{}", format!("{i}").repeat(40))]), "commit")?;
        must(git(&home, &work, &["push", "-q", "origin", "HEAD:refs/for/main"]), "submit")?;
    }

    // Main makes the same line 100.
    must(git(&home, dir, &["clone", "-q", &url(&s.admin), "owner"]), "the owner's clone")?;
    let owner = dir.join("owner");
    std::fs::write(owner.join("plan/expand.rs"), expand("100"))?;
    must(git(&home, &owner, &["commit", "-qam", "main moves"]), "commit")?;
    must(git(&home, &owner, &["push", "-q", "origin", "HEAD:main"]), "main moves")?;
    Ok(World { s, bare, dev, kim })
}

/// D1's one conflict resolved by dev with an edit to `value`, and committed: remembered.
fn resolve_d1(w: &World, value: &str) -> anyhow::Result<()> {
    let got = ok(&w.s, &w.kim, "GET", "/api/diffs/D1/conflicts", None)?;
    let id = got.pointer("/conflicts/0/id").and_then(Value::as_str).unwrap_or_default().to_owned();
    anyhow::ensure!(!id.is_empty(), "D1 conflicts: {got}");
    ok(&w.s, &w.dev, "POST", &format!("/api/diffs/D1/conflicts/{id}/resolution"), Some(&json!({ "take": "edit", "text": format!("    let a = {value};") })))?;
    ok(&w.s, &w.dev, "POST", "/api/diffs/D1/conflicts/commit", Some(&json!({})))?;
    Ok(())
}

fn ladder(w: &World, d: &str) -> anyhow::Result<Value> {
    ok(&w.s, &w.kim, "GET", &format!("/api/diffs/{d}/conflicts/ladder"), None)
}

/// W26-R17: a resolution accepted once is offered again the next time the same hunk conflicts, on
/// another change; its tests run first, then the resolutions together; a person accepts it, and
/// it's D2's new version, its why `conflict resolved`.
#[test]
fn w26_r17_a_resolution_accepted_once_is_offered_again() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let w = world(dir.path())?;
    resolve_d1(&w, "110")?;

    // D2's replay conflicts in the same hunk: the remembered resolution, tested, then the whole.
    let got = ok(&w.s, &w.kim, "GET", "/api/diffs/D2/conflicts", None)?;
    assert_eq!(got.pointer("/replay/state"), Some(&json!("conflicted")), "{got}");
    until(240, "D2's resolutions offered", || {
        let l = ladder(&w, "D2")?;
        Ok((text(&l, "state") != "offered").then(|| l.to_string()))
    })?;
    let l = ladder(&w, "D2")?;
    let tries: Vec<Value> = l.get("conflicts").and_then(Value::as_object).map(|m| m.values().flat_map(|v| v.as_array().cloned().unwrap_or_default()).collect()).unwrap_or_default();
    let remembered = tries.iter().find(|t| text(t, "rung") == "remembered").cloned().unwrap_or_default();
    assert_eq!((text(&remembered, "state"), text(&remembered, "tests"), text(&remembered, "by")), ("resolved", "passed", "kiln"), "{l}");
    assert_eq!(remembered.get("lines"), Some(&json!(["    let a = 110;"])), "{l}");
    assert!(text(&remembered, "explanation").contains("user:dev") || text(&remembered, "explanation").contains("dev"), "{l}");
    assert_eq!(l.pointer("/whole/tests"), Some(&json!("passed")), "{l}");
    assert!(l.get("why").and_then(Value::as_array).is_some_and(|w| w.iter().any(|x| x.as_str().is_some_and(|x| x.contains("a person confirms it")))), "{l}");

    // An agent may not accept it; kim does: D2 v2, with the remembered line and D2's README.
    let fixer = {
        let (st, t) = http(w.s.port, "POST", "/api/tokens", &w.s.admin, Some(&json!({ "kind": "agent", "agent": "fixer", "user": "dev", "grants": ["developer@repo:acme/app"] })))?;
        anyhow::ensure!((200..300).contains(&st), "the agent's token: {st}");
        text(&serde_json::from_str::<Value>(&t)?, "token").to_owned()
    };
    let (st, r) = call(&w.s, &fixer, "POST", "/api/diffs/D2/conflicts/accept", Some(&json!({})))?;
    assert_eq!(st, 403, "{r}");
    let r = ok(&w.s, &w.kim, "POST", "/api/diffs/D2/conflicts/accept", Some(&json!({})))?;
    assert_eq!((r.get("version"), text(&r, "why")), (Some(&json!(2)), "conflict resolved"), "{r}");
    let v2 = ok(&w.s, &w.kim, "GET", "/api/diffs/D2", None)?;
    let commit = v2.get("versions").and_then(Value::as_array).and_then(|v| v.last()).map(|v| text(v, "commit").to_owned()).unwrap_or_default();
    assert_eq!(show(&w.bare, &commit, "plan/expand.rs")?, expand("110"), "the remembered resolution");
    assert_eq!(show(&w.bare, &commit, "README.md")?, "app, said by D2\n", "D2's own change kept");
    // Accepted, and nothing left to resolve.
    let got = ok(&w.s, &w.kim, "GET", "/api/diffs/D2/conflicts", None)?;
    assert_ne!(got.pointer("/replay/state"), Some(&json!("conflicted")), "{got}");
    Ok(())
}

/// W26-R17: a remembered resolution whose tests fail is kept as failed, said, and the conflict
/// goes on up the ladder: here the person, who resolves it themselves.
#[test]
fn w26_r17_a_rung_whose_tests_fail_hands_on() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let w = world(dir.path())?;
    // 99 fails the gate.
    resolve_d1(&w, "99")?;
    ok(&w.s, &w.kim, "GET", "/api/diffs/D2/conflicts", None)?;
    until(240, "D2's remembered resolution tested", || {
        let l = ladder(&w, "D2")?;
        Ok((text(&l, "state") != "held").then(|| l.to_string()))
    })?;
    let l = ladder(&w, "D2")?;
    let tries: Vec<Value> = l.get("conflicts").and_then(Value::as_object).map(|m| m.values().flat_map(|v| v.as_array().cloned().unwrap_or_default()).collect()).unwrap_or_default();
    let remembered = tries.iter().find(|t| text(t, "rung") == "remembered").cloned().unwrap_or_default();
    assert_eq!((text(&remembered, "state"), text(&remembered, "tests")), ("resolved", "failed"), "{l}");
    let why = l.get("why").map(Value::to_string).unwrap_or_default();
    assert!(why.contains("failed its tests") && why.contains("a person takes a side or edits"), "{l}");
    let (st, r) = call(&w.s, &w.kim, "POST", "/api/diffs/D2/conflicts/accept", Some(&json!({})))?;
    assert_eq!(st, 409, "nothing offered: {r}");
    Ok(())
}
