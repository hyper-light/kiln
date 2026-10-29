//! Conflicts in the diff (docs/ci.md W26-R12, AC04), against a server of its own with real `git`: a
//! change replayed onto a moved trunk conflicts; each conflict comes back where it falls (a text
//! file's at the change's own lines, main's side, this side and the base; a modify/delete, a
//! delete/modify and a binary file named, never as markers); each is resolved its own way (main,
//! this, both, an agent's resolution a person takes, an edit, a side); Commit resolution makes one
//! revision of whoever commits it, its why `conflict resolved`, its tree as chosen; and the gate
//! holds approving and landing until then.

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

/// A file's bytes at a commit of the bare repository, or `None` where it has none.
fn show(bare: &Path, rev: &str, file: &str) -> anyhow::Result<Option<Vec<u8>>> {
    let o = Command::new("git").arg("-C").arg(bare).args(["show", &format!("{rev}:{file}")]).env("GIT_CONFIG_NOSYSTEM", "1").output()?;
    Ok(o.status.success().then_some(o.stdout))
}

fn rev(bare: &Path, args: &[&str]) -> anyhow::Result<String> {
    let o = Command::new("git").arg("-C").arg(bare).args(args).env("GIT_CONFIG_NOSYSTEM", "1").output()?;
    anyhow::ensure!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
    Ok(String::from_utf8_lossy(&o.stdout).trim().to_owned())
}

/// A user with `grants`, and a personal access token that may do what they may.
fn person(s: &Server, name: &str, grants: &[&str]) -> anyhow::Result<String> {
    let (st, body) = http(s.port, "POST", "/api/users", &s.admin, Some(&json!({ "name": name, "grants": grants })))?;
    anyhow::ensure!((200..300).contains(&st), "{name}: {st} {body}");
    token(s, json!({ "kind": "pat", "user": name, "grants": ["owner"] }))
}

fn token(s: &Server, body: Value) -> anyhow::Result<String> {
    let (st, text) = http(s.port, "POST", "/api/tokens", &s.admin, Some(&body))?;
    anyhow::ensure!((200..300).contains(&st), "token {body}: {st} {text}");
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

fn error(v: &Value) -> String {
    text(v, "error").to_owned()
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

/// A native repository `acme/app` whose trunk lands with one approval (open threads don't hold
/// it: what holds it here is the gate), its bare repository's path.
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

/// expand.rs with each function's first line as `values` say: five conflicts apart (git merges
/// conflicts closer than four lines into one).
fn expand(header: &str, values: [&str; 5]) -> String {
    let names = ["one", "two", "three", "four", "five"];
    let vars = ["a", "b", "c", "d", "e"];
    let mut out = format!("{header}// expand.rs\n");
    for (i, ((name, var), value)) in names.iter().zip(vars).zip(values).enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&format!("fn {name}() {{\n    let {var} = {value};\n    {name}_more();\n}}\n"));
    }
    out
}

const BASE_PNG: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00base";
const MAIN_PNG: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00main";
const THIS_PNG: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00this";

/// W26-AC04 on a diff, as D41 has it: D1 replayed onto a moved main conflicts in expand.rs five
/// times (at D1's own lines though main added two above them), in gone.txt (main changed it, D1
/// deletes it), kept.txt (main deleted it, D1 changes it) and logo.png (binary). Each comes back
/// where it falls, the text ones with main's side, D1's and the base, the others named. The gate
/// holds accepting and landing for each conflict, unresolved or resolved and not yet committed. The
/// five text conflicts are resolved the five ways (main, this, both, an agent's resolution a person
/// takes, an edit), the others by a side; an agent's resolution is the agent's until a person takes
/// it, and an agent commits none. Main moving again on its own files keeps every resolution.
/// Commit resolution makes v2 of dev, who committed it (D1's author and message kept, the agent a
/// co-author), its why `conflict resolved`, its tree as chosen; then D1 is approved and lands.
#[test]
fn w26_ac04_conflicts_resolve_in_the_diff_and_commit_as_a_revision() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options::default())?;
    let home = dir.path().join("githome");
    std::fs::create_dir_all(&home)?;
    let url = |token: &str| format!("http://x:{token}@127.0.0.1:{}/git/acme/app.git", s.port);
    let bare = hosted(&s)?;
    let dev = person(&s, "dev", &["developer@repo:acme/app"])?;
    let kim = person(&s, "kim", &["developer@repo:acme/app"])?;
    let lead = person(&s, "lead", &["maintainer@repo:acme/app"])?;
    let fixer = token(&s, json!({ "kind": "agent", "agent": "fixer", "user": "dev", "grants": ["developer@repo:acme/app"] }))?;

    // The trunk.
    must(git(&home, dir.path(), &["clone", "-q", &url(&dev), "work"]), "clone")?;
    let work = dir.path().join("work");
    let base = expand("", ["1", "2", "3", "4", "5"]);
    write(
        &work,
        &[(".kiln/review.yaml", b"land:\n  approvals: 1\n  resolve-threads: false\n"), ("expand.rs", base.as_bytes()), ("gone.txt", b"keep me\n"), ("kept.txt", b"kept\n"), ("logo.png", BASE_PNG), ("clean.txt", b"clean\n")],
    )?;
    must(git(&home, &work, &["add", "-A"]), "add")?;
    must(git(&home, &work, &["commit", "-qm", "first"]), "commit")?;
    must(git(&home, &work, &["push", "-q", "origin", "main"]), "the first trunk")?;

    // D1.
    write(&work, &[("expand.rs", expand("", ["10", "20", "30", "40", "50"]).as_bytes()), ("kept.txt", b"kept, changed in D1\n"), ("logo.png", THIS_PNG)])?;
    must(git(&home, &work, &["rm", "-q", "gone.txt"]), "rm")?;
    must(git(&home, &work, &["add", "-A"]), "add")?;
    must(git(&home, &work, &["commit", "-qm", &format!("pipeline: dynamic matrices\n\nChange-Id: I{}", "4".repeat(40))]), "D1")?;
    must(git(&home, &work, &["push", "-q", "origin", "HEAD:refs/for/main"]), "submit")?;

    // Main moves: the same lines changed otherwise, two lines added above them, gone.txt changed,
    // kept.txt deleted, logo.png changed, clean.txt changed where D1 doesn't touch.
    must(git(&home, dir.path(), &["clone", "-q", &url(&s.admin), "owner"]), "the owner's clone")?;
    let owner = dir.path().join("owner");
    let header = "// main's header\n// two lines\n";
    write(&owner, &[("expand.rs", expand(header, ["100", "200", "300", "400", "500"]).as_bytes()), ("gone.txt", b"keep me, changed on main\n"), ("logo.png", MAIN_PNG), ("clean.txt", b"clean, main\n")])?;
    must(git(&home, &owner, &["rm", "-q", "kept.txt"]), "rm")?;
    must(git(&home, &owner, &["add", "-A"]), "add")?;
    must(git(&home, &owner, &["commit", "-qm", "main moves"]), "commit")?;
    must(git(&home, &owner, &["push", "-q", "origin", "HEAD:main"]), "main moves")?;

    // Each conflict where it falls.
    let conflicts = |token: &str| ok(&s, token, "GET", "/api/diffs/D1/conflicts", None);
    let got = conflicts(&kim)?;
    assert!(!got.to_string().contains("<<<<<<<") && !got.to_string().contains(">>>>>>>"), "no markers: {got}");
    assert_eq!((got.pointer("/replay/state"), got.pointer("/replay/version"), got.pointer("/replay/onto/name")), (Some(&json!("conflicted")), Some(&json!(1)), Some(&json!("main"))), "{got}");
    let list = got.get("conflicts").and_then(Value::as_array).cloned().unwrap_or_default();
    let shape: Vec<(&str, &str, Option<u64>)> = list.iter().map(|c| (text(c, "file"), text(c, "kind"), c.get("line").and_then(Value::as_u64))).collect();
    assert_eq!(
        shape,
        [
            ("expand.rs", "text", Some(3)),
            ("expand.rs", "text", Some(8)),
            ("expand.rs", "text", Some(13)),
            ("expand.rs", "text", Some(18)),
            ("expand.rs", "text", Some(23)),
            ("gone.txt", "modify_delete", None),
            ("kept.txt", "delete_modify", None),
            ("logo.png", "binary", None),
        ],
        "{got}"
    );
    let first = list.first().cloned().unwrap_or_default();
    assert_eq!(first.pointer("/main"), Some(&json!({ "start": 5, "lines": ["    let a = 100;"] })), "main's side at main's lines: {first}");
    assert_eq!(first.pointer("/this"), Some(&json!({ "start": 3, "lines": ["    let a = 10;"] })), "{first}");
    assert_eq!(first.pointer("/base/lines"), Some(&json!(["    let a = 1;"])), "{first}");
    assert_eq!((first.get("of"), first.get("takes")), (Some(&json!(5)), Some(&json!(["main", "this", "both", "agent", "edit"]))), "{first}");
    let gone = list.get(5).cloned().unwrap_or_default();
    assert_eq!(text(&gone, "says"), "gone.txt: main changed it and this change deletes it: keep main's, or delete it", "{gone}");
    assert!(gone.pointer("/main/blob").is_some() && gone.get("this").is_some_and(Value::is_null), "main's file, this side none: {gone}");
    assert_eq!(gone.get("takes"), Some(&json!(["main", "this", "agent"])), "{gone}");
    assert_eq!(text(&list.get(7).cloned().unwrap_or_default(), "says"), "logo.png: a binary file both main and this change changed: take main's or this change's");
    assert_eq!((got.get("unresolved"), got.get("ready")), (Some(&json!(8)), Some(&json!(false))));
    let id = |i: usize| list.get(i).map(|c| text(c, "id").to_owned()).unwrap_or_default();

    // The gate holds accepting, landing, and anyone's force.
    let accept = |token: &str| call(&s, token, "POST", "/api/diffs/D1/review", Some(&json!({ "action": "accept", "force": true })));
    let (st, r) = accept(&kim)?;
    assert_eq!(st, 409, "{r}");
    for want in ["expand.rs:3: resolve this conflict first", "expand.rs:23: resolve this conflict first", "gone.txt: resolve this conflict first", "kept.txt: resolve this conflict first", "logo.png: resolve this conflict first"] {
        assert!(error(&r).contains(want), "{want}: {r}");
    }
    let gate = ok(&s, &kim, "GET", "/api/diffs/D1/comments", None)?;
    assert_eq!((gate.pointer("/gate/verdict"), gate.pointer("/gate/conflicts")), (Some(&json!("hold")), Some(&json!(8))), "{gate}");
    let (st, r) = call(&s, &lead, "POST", "/api/diffs/D1/land", Some(&json!({})))?;
    assert!(st == 409 && error(&r).contains("expand.rs:3: resolve this conflict first"), "{st} {r}");

    // Resolutions: refused where they don't fit; each text conflict its own way.
    let resolve = |token: &str, i: usize, body: Value| call(&s, token, "POST", &format!("/api/diffs/D1/conflicts/{}/resolution", id(i)), Some(&body));
    let (st, r) = resolve(&dev, 5, json!({ "take": "both" }))?;
    assert_eq!((st, error(&r)), (400, "gone.txt: main changed it and this change deletes it: keep main's, or delete it".to_owned()), "{r}");
    let (st, r) = resolve(&dev, 7, json!({ "take": "edit", "text": "bytes" }))?;
    assert_eq!(st, 400, "a binary file takes a side: {r}");
    let (st, r) = resolve(&dev, 4, json!({ "take": "edit", "lines": ["<<<<<<< main", "x"] }))?;
    assert_eq!((st, error(&r)), (400, "expand.rs:23: the edit still has conflict markers".to_owned()), "{r}");
    let (st, r) = resolve(&dev, 3, json!({ "take": "agent" }))?;
    assert_eq!((st, error(&r)), (409, "expand.rs:18: no agent has resolved it".to_owned()), "{r}");
    let (st, r) = resolve(&kim, 0, json!({ "take": "sideways" }))?;
    assert_eq!(st, 400, "{r}");
    for (i, body) in [(0, json!({ "take": "main" })), (1, json!({ "take": "this" })), (2, json!({ "take": "both" })), (4, json!({ "take": "edit", "text": "    let e = edited();\n" }))] {
        let (st, r) = resolve(&dev, i, body)?;
        anyhow::ensure!(st == 200, "conflict {i}: {r}");
    }
    // The agent's resolution is the agent's, until a person takes it.
    let (st, r) = resolve(&fixer, 3, json!({ "take": "edit", "lines": ["    let d = agent();"] }))?;
    anyhow::ensure!(st == 200, "the agent's resolution: {r}");
    let fourth = r.pointer("/conflicts/3").cloned().unwrap_or_default();
    assert_eq!((fourth.pointer("/agent/by"), fourth.pointer("/agent/lines"), fourth.get("resolution")), (Some(&json!("agent:fixer")), Some(&json!(["    let d = agent();"])), Some(&Value::Null)), "{fourth}");
    assert_eq!(r.get("unresolved"), Some(&json!(4)), "{r}");
    let (st, r) = resolve(&dev, 3, json!({ "take": "agent" }))?;
    anyhow::ensure!(st == 200, "dev takes the agent's: {r}");
    assert_eq!(r.pointer("/conflicts/3/resolution/lines"), Some(&json!(["    let d = agent();"])), "{r}");
    assert_eq!(r.pointer("/conflicts/2/resolution/lines"), Some(&json!(["    let c = 300;", "    let c = 30;"])), "both: main's then D1's: {r}");

    // Not all resolved yet: Commit resolution names what's left, and the gate says which wait on
    // what.
    let commit = |token: &str| call(&s, token, "POST", "/api/diffs/D1/conflicts/commit", Some(&json!({})));
    let (st, r) = commit(&dev)?;
    assert_eq!((st, error(&r)), (409, "gone.txt: resolve this conflict first; kept.txt: resolve this conflict first; logo.png: resolve this conflict first".to_owned()), "{r}");
    let (st, r) = accept(&kim)?;
    assert!(st == 409 && error(&r).contains("expand.rs:3: commit its resolution first") && error(&r).contains("logo.png: resolve this conflict first"), "{r}");
    for (i, take) in [(5, "this"), (6, "this"), (7, "main")] {
        let (st, r) = resolve(&dev, i, json!({ "take": take }))?;
        anyhow::ensure!(st == 200, "conflict {i}: {r}");
    }
    // A resolution taken back, then given again.
    let (st, r) = call(&s, &dev, "DELETE", &format!("/api/diffs/D1/conflicts/{}/resolution", id(1)), None)?;
    assert_eq!((st, r.get("unresolved")), (200, Some(&json!(1))), "{r}");
    let (st, r) = resolve(&dev, 1, json!({ "take": "this" }))?;
    assert_eq!((st, r.get("unresolved"), r.get("ready")), (200, Some(&json!(0)), Some(&json!(true))), "{r}");
    // Each resolved, none committed: the gate still holds, saying so.
    let (st, r) = accept(&kim)?;
    assert!(st == 409 && error(&r).contains("logo.png: commit its resolution first") && !error(&r).contains("resolve this conflict"), "{r}");

    // Main moves again, on its own files: replayed again, every resolution kept.
    let replay = text(got.get("replay").unwrap_or(&Value::Null), "id").to_owned();
    write(&owner, &[("other.txt", b"elsewhere\n")])?;
    must(git(&home, &owner, &["add", "-A"]), "add")?;
    must(git(&home, &owner, &["commit", "-qm", "main moves again"]), "commit")?;
    must(git(&home, &owner, &["push", "-q", "origin", "HEAD:main"]), "main moves again")?;
    let again = conflicts(&kim)?;
    assert_ne!(again.pointer("/replay/id").and_then(Value::as_str), Some(replay.as_str()), "replayed onto main's new tip: {again}");
    assert_eq!((again.get("unresolved"), again.get("ready")), (Some(&json!(0)), Some(&json!(true))), "{again}");

    // An agent commits none; dev commits it: v2, its why, of dev.
    let (st, r) = commit(&fixer)?;
    assert_eq!(st, 403, "{r}");
    let r = ok(&s, &dev, "POST", "/api/diffs/D1/conflicts/commit", Some(&json!({})))?;
    assert_eq!((r.get("version"), text(&r, "why"), r.get("signer")), (Some(&json!(2)), "conflict resolved", Some(&Value::Null)), "unsigned until W27-R03: {r}");
    let d = ok(&s, &kim, "GET", "/api/diffs/D1", None)?;
    let v2 = d.pointer("/versions/1").cloned().unwrap_or_default();
    assert_eq!((v2.get("n"), text(&v2, "why"), text(&v2, "note"), v2.get("rebase_only")), (Some(&json!(2)), "conflict resolved", "conflict resolved", Some(&json!(false))), "{d}");
    assert!(text(&v2, "pushed_by").contains("dev"), "{v2}");
    let made = text(&v2, "commit").to_owned();
    assert_eq!(text(&r, "commit"), made);

    // Its tree, as chosen.
    let tip = rev(&bare, &["rev-parse", "refs/heads/main"])?;
    assert_eq!(text(&v2, "parent"), tip, "onto main's tip");
    let want = "// main's header\n// two lines\n// expand.rs\nfn one() {\n    let a = 100;\n    one_more();\n}\n\nfn two() {\n    let b = 20;\n    two_more();\n}\n\nfn three() {\n    let c = 300;\n    let c = 30;\n    three_more();\n}\n\nfn four() {\n    let d = agent();\n    four_more();\n}\n\nfn five() {\n    let e = edited();\n    five_more();\n}\n";
    assert_eq!(show(&bare, &made, "expand.rs")?.map(|b| String::from_utf8_lossy(&b).into_owned()).as_deref(), Some(want));
    assert_eq!(show(&bare, &made, "gone.txt")?, None, "D1's delete taken");
    assert_eq!(show(&bare, &made, "kept.txt")?.as_deref(), Some(&b"kept, changed in D1\n"[..]), "D1's change kept");
    assert_eq!(show(&bare, &made, "logo.png")?.as_deref(), Some(MAIN_PNG), "main's bytes");
    assert_eq!(show(&bare, &made, "clean.txt")?.as_deref(), Some(&b"clean, main\n"[..]), "main's change that merged cleanly");
    assert_eq!(show(&bare, &made, "other.txt")?.as_deref(), Some(&b"elsewhere\n"[..]));
    // Committed by dev, D1's author and message kept, the agent whose resolution was taken a
    // co-author.
    let who = rev(&bare, &["log", "-1", "--format=%an|%cn|%B", &made])?;
    assert!(who.starts_with("Test|dev|pipeline: dynamic matrices"), "{who}");
    assert!(who.contains(&format!("Change-Id: I{}", "4".repeat(40))) && who.contains("Co-authored-by: agent:fixer <agent-fixer@kiln>"), "{who}");

    // Approve waits no more, and D1 lands.
    let after = conflicts(&kim)?;
    assert_eq!((after.pointer("/replay/state"), after.pointer("/replay/resolved_in"), after.get("unresolved")), (Some(&json!("resolved")), Some(&json!(2)), Some(&json!(0))), "{after}");
    let (st, r) = call(&s, &kim, "POST", "/api/diffs/D1/review", Some(&json!({ "action": "accept" })))?;
    assert_eq!((st, text(&r, "state")), (200, "accepted"), "{r}");
    let (st, r) = call(&s, &lead, "POST", "/api/diffs/D1/land", Some(&json!({})))?;
    anyhow::ensure!(st == 200, "land: {r}");
    until(180, "D1 lands", || {
        let d = ok(&s, &dev, "GET", "/api/diffs/D1", None)?;
        Ok((text(&d, "state") != "landed").then(|| d.to_string()))
    })?;
    let (_, audit) = http(s.port, "GET", "/api/audit?limit=200", &s.admin, None)?;
    for want in ["diffs.conflict.resolve", "an agent's resolution, taking edit", "diffs.conflicts.commit", "acme/app D1 v2: 8 conflicts resolved onto main"] {
        assert!(audit.contains(want), "{want} in the audit: {audit}");
    }
    Ok(())
}

/// W26-R12 on a change request (W14-R04): its branch replayed onto a moved main conflicts; taking
/// both and committing makes a merge commit on its branch (its first parent the branch's head, its
/// second main's tip), of whoever committed it, a new version whose why is `conflict resolved`;
/// accepting waits for it.
#[test]
fn w26_r12_a_change_requests_resolution_is_a_merge_commit() -> anyhow::Result<()> {
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
    write(&work, &[(".kiln/review.yaml", b"land:\n  approvals: 1\n  resolve-threads: false\n"), ("f.txt", b"one\ntwo\nthree\n")])?;
    must(git(&home, &work, &["add", "-A"]), "add")?;
    must(git(&home, &work, &["commit", "-qm", "first"]), "commit")?;
    must(git(&home, &work, &["push", "-q", "origin", "main"]), "the first trunk")?;
    must(git(&home, &work, &["checkout", "-qb", "topic"]), "branch")?;
    write(&work, &[("f.txt", b"one\nTWO, topic\nthree\n")])?;
    must(git(&home, &work, &["commit", "-qam", "topic work"]), "commit")?;
    let out = must(git(&home, &work, &["push", "origin", "topic:refs/for/main/branch/topic"]), "the change request")?;
    assert!(out.contains("CR1"), "{out}");
    let head = rev(&bare, &["rev-parse", "refs/heads/topic"])?;

    must(git(&home, dir.path(), &["clone", "-q", &url(&s.admin), "owner"]), "the owner's clone")?;
    let owner = dir.path().join("owner");
    write(&owner, &[("f.txt", b"one\nTWO, main\nthree\n")])?;
    must(git(&home, &owner, &["commit", "-qam", "main moves"]), "commit")?;
    must(git(&home, &owner, &["push", "-q", "origin", "HEAD:main"]), "main moves")?;
    let tip = rev(&bare, &["rev-parse", "refs/heads/main"])?;

    let got = ok(&s, &kim, "GET", "/api/change-requests/CR1/conflicts", None)?;
    let c = got.pointer("/conflicts/0").cloned().unwrap_or_default();
    assert_eq!((text(&c, "file"), c.get("line"), c.pointer("/main/lines"), c.pointer("/this/lines")), ("f.txt", Some(&json!(2)), Some(&json!(["TWO, main"])), Some(&json!(["TWO, topic"]))), "{got}");
    let (st, r) = call(&s, &kim, "POST", "/api/change-requests/CR1/review", Some(&json!({ "action": "accept" })))?;
    assert_eq!((st, error(&r)), (409, "f.txt:2: resolve this conflict first".to_owned()), "{r}");

    ok(&s, &dev, "POST", &format!("/api/change-requests/CR1/conflicts/{}/resolution", text(&c, "id")), Some(&json!({ "take": "both" })))?;
    let r = ok(&s, &dev, "POST", "/api/change-requests/CR1/conflicts/commit", Some(&json!({})))?;
    assert_eq!((r.get("version"), text(&r, "why")), (Some(&json!(2)), "conflict resolved"), "{r}");
    let merged = rev(&bare, &["rev-parse", "refs/heads/topic"])?;
    assert_eq!(text(&r, "commit"), merged, "the branch moved to it");
    assert_eq!(rev(&bare, &["log", "-1", "--format=%P|%an|%cn", &merged])?, format!("{head} {tip}|dev|dev"), "a merge of main into the branch, by dev");
    assert_eq!(show(&bare, &merged, "f.txt")?.as_deref(), Some(&b"one\nTWO, main\nTWO, topic\nthree\n"[..]), "both: main's then the change's");
    let cr = ok(&s, &kim, "GET", "/api/change-requests/CR1", None)?;
    assert_eq!((cr.pointer("/versions/1/why"), cr.pointer("/versions/1/commit")), (Some(&json!("conflict resolved")), Some(&json!(merged))), "{cr}");
    let (st, r) = call(&s, &kim, "POST", "/api/change-requests/CR1/review", Some(&json!({ "action": "accept" })))?;
    assert_eq!((st, text(&r, "state")), (200, "accepted"), "{r}");
    Ok(())
}
