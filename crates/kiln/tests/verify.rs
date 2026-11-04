//! kiln verify (docs/ci.md W27-R10, R11; AC07, AC08), end to end. Sam's signed commits, each a diff
//! kim accepts on her laptop and the gate passes, land through main's queue (its commits kiln's
//! own, signed as kiln); main is built by a leg that signs, attesting what it built; and that's
//! released to prod. kiln verify walks the release back to sam, link by link, within 2 s for 10
//! commits, and names the link each tamper breaks: an artifact rebuilt, a forged review, a trail
//! entry edited, a commit pushed to main without the queue. A production environment refuses a
//! release whose chain doesn't verify.

#![cfg(unix)]

mod common;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use base64::Engine as _;
use common::agents::{self, Talk};
use common::{Options, Server, http, start_cd};
use serde_json::{Value, json};

const TALK: Talk = Talk { lines: 1, every_ms: 10 };

const PROVENANCE: &str = "https://slsa.dev/provenance/v1";

fn until<T>(within: Duration, what: &str, mut f: impl FnMut() -> anyhow::Result<Option<T>>) -> anyhow::Result<T> {
    let deadline = Instant::now() + within;
    loop {
        if let Some(t) = f()? {
            return Ok(t);
        }
        anyhow::ensure!(Instant::now() < deadline, "{what}: not within {within:?}");
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn get(s: &Server, path: &str) -> anyhow::Result<Value> {
    let (st, body) = http(s.port, "GET", path, &s.admin, None)?;
    anyhow::ensure!(st == 200, "{path}: {st} {body}");
    Ok(serde_json::from_str(&body)?)
}

fn post(s: &Server, path: &str, body: Option<&Value>) -> anyhow::Result<Value> {
    let (st, text) = http(s.port, "POST", path, &s.admin, body)?;
    anyhow::ensure!((200..300).contains(&st), "{path}: {st} {text}");
    Ok(serde_json::from_str(&text).unwrap_or(Value::Null))
}

fn text<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or_default()
}

/// A person on the server, with a token, signed in on a laptop of their own (`kiln login`), whose
/// git signs their commits as them (kiln's signing program, with the key `kiln login` made there).
struct Person {
    token: String,
    home: PathBuf,
    config: PathBuf,
    program: PathBuf,
}

impl Person {
    fn new(dir: &Path, s: &Server, name: &str) -> anyhow::Result<Self> {
        post(s, "/api/users", Some(&json!({ "name": name, "grants": ["developer"] })))?;
        let token = text(&post(s, "/api/tokens", Some(&json!({ "kind": "pat", "user": name, "grants": ["owner"] })))?, "token").to_owned();
        let link = text(&post(s, &format!("/api/users/{name}/link"), None)?, "url").to_owned();
        let laptop = dir.join(format!("{name}-laptop"));
        let p = Person { token, home: laptop.join("home"), config: laptop.join("config"), program: laptop.join("bin").join(kiln::gitsign::NAME) };
        for d in [&p.home, &p.config, &laptop.join("bin")] {
            std::fs::create_dir_all(d)?;
        }
        if std::fs::hard_link(env!("CARGO_BIN_EXE_kiln"), &p.program).is_err() {
            std::fs::copy(env!("CARGO_BIN_EXE_kiln"), &p.program)?;
        }
        let (ok, said) = p.kiln(&["login", "--link", &link])?;
        anyhow::ensure!(ok, "{name}'s kiln login --link: {said}");
        Ok(p)
    }

    /// `kiln args` on the laptop: whether it succeeded, and what it said.
    fn kiln(&self, args: &[&str]) -> anyhow::Result<(bool, String)> {
        let out = common::kiln().args(args).env("HOME", &self.home).env("XDG_CONFIG_HOME", &self.config).env("KILN_TOKEN", "nobody").output()?;
        Ok((out.status.success(), format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))))
    }

    /// git on the laptop, in `at`.
    fn git(&self, at: &Path, args: &[&str]) -> anyhow::Result<String> {
        let o = std::process::Command::new("git")
            .args(["-c", "init.defaultBranch=main", "-c", "credential.helper="])
            .args(args)
            .current_dir(at)
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", &self.config)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()?;
        let said = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
        anyhow::ensure!(o.status.success(), "git {args:?}: {said}");
        Ok(said)
    }

    /// `tree`'s commits signed as this person.
    fn signs_in(&self, tree: &Path, name: &str) -> anyhow::Result<()> {
        let mut config = vec![("user.name", name.to_owned()), ("user.email", format!("{name}@example.com"))];
        config.extend(kiln::gitsign::git_config(&self.program, kiln::gitsign::DEVICE_KEY));
        for (k, v) in &config {
            self.git(tree, &["config", "--local", k, v])?;
        }
        Ok(())
    }
}

/// What the shop made: its server and runner, sam and sam's tree, main's head as released, and the
/// digest of what was built from it.
struct Shop {
    s: Server,
    _runner: agents::Runner,
    sam: Person,
    work: PathBuf,
    tip: String,
    digest: String,
}

const REVIEW: &str = "land:\n  approvals: 1\n  checks: [gate]\ndirect-push: [sam]\nsignatures:\n  mode: required\n";
const GATE: &str = "name: gate\non:\n  pull_request: {}\n  merge_group: {}\njobs:\n  gate:\n    runs-on: self-hosted\n    steps:\n      - run: echo gated\n";
const BUILD: &str = "on:\n  push:\n    branches: [main]\njobs:\n  image:\n    runs-on: self-hosted\n    steps:\n      - run: printf '%s\\n' \"$KILN_SHA\" > image.txt\n      - uses: kiln/attest@v1\n        with: { subject-path: image.txt }\n";

/// A server whose runner (`fedora-2`) signs as each leg, and its repository `acme/app`: main's
/// policy (one approval, the gate on each version and candidate, signed commits) and workflows
/// pushed first, then `n` diffs of sam's, stacked, each accepted by kim and gated, landed through
/// main's queue; main built and attested from its new head by a leg; released to prod as
/// app@1.0.0.
fn shop(dir: &Path, n: usize) -> anyhow::Result<Shop> {
    let s = start_cd(dir, &Options { agent: false, ci: "kiln_workflows = true\n", extra: "" })?;
    let (runner, log) = agents::runner_with(dir, s.port, "fedora-2", None, TALK, &[])?;
    // Under load (a gate beside it) a runner takes a while to connect.
    agents::connected(s.port, &s.admin, "fedora-2", &log, Duration::from_secs(180))?;
    post(&s, "/api/repos", Some(&json!({ "name": "acme/app", "mode": "native" })))?;
    let (sam, kim) = (Person::new(dir, &s, "sam")?, Person::new(dir, &s, "kim")?);
    sam.git(dir, &["clone", "-q", &format!("http://x:{}@127.0.0.1:{}/git/acme/app.git", sam.token, s.port), "work"])?;
    let work = dir.join("work");
    sam.signs_in(&work, "sam")?;
    for (path, body) in [(".kiln/review.yaml", REVIEW), (".kiln/gate.yaml", GATE), (".kiln/build.yaml", BUILD), ("README.md", "an app\n")] {
        let at = work.join(path);
        if let Some(parent) = at.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(at, body)?;
    }
    sam.git(&work, &["add", "-A"])?;
    sam.git(&work, &["commit", "-qm", "first"])?;
    sam.git(&work, &["push", "-q", "origin", "HEAD:main"])?;
    for i in 1..=n {
        std::fs::write(work.join(format!("f{i}.txt")), format!("change {i}\n"))?;
        sam.git(&work, &["add", "-A"])?;
        sam.git(&work, &["commit", "-qm", &format!("change {i}\n\nChange-Id: I{i:040x}\n")])?;
    }
    let said = sam.git(&work, &["push", "origin", "HEAD:refs/for/main"])?;
    anyhow::ensure!(said.contains(&format!("D{n} v1")), "the stack: {said}");
    let url = format!("http://127.0.0.1:{}", s.port);
    for i in 1..=n {
        let (ok, said) = kim.kiln(&["review", "--url", &url, "--token", &kim.token, &format!("D{i}"), "--accept"])?;
        anyhow::ensure!(ok, "kim's kiln review of D{i}: {said}");
    }
    for i in 1..=n {
        until(Duration::from_secs(300), &format!("D{i}'s gate"), || Ok((get(&s, &format!("/api/diffs/D{i}/land"))?.get("blockers") == Some(&json!([]))).then_some(())))?;
    }
    // The top of the stack lands it whole: one candidate, tested by the gate.
    post(&s, &format!("/api/diffs/D{n}/land"), Some(&json!({})))?;
    for i in 1..=n {
        until(Duration::from_secs(300), &format!("D{i} landed"), || Ok((text(&get(&s, &format!("/api/diffs/D{i}"))?, "state") == "landed").then_some(())))?;
    }
    let tip = sam.git(&work, &["ls-remote", "origin", "refs/heads/main"])?.split_whitespace().next().unwrap_or_default().to_owned();
    // main's head built, and what was built attested: kiln's provenance, and the leg's signature
    // of it.
    let db = rusqlite::Connection::open(dir.join("data/kiln.db"))?;
    let digest = until(Duration::from_secs(300), "main's head built and attested by kiln and its leg", || {
        let rows: Vec<(String, String)> = db.prepare("SELECT origin, bundle FROM ci_attestations WHERE predicate_type = ?1")?.query_map([PROVENANCE], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?;
        let of_tip: Vec<(String, Value)> = rows.into_iter().filter_map(|(origin, bundle)| statement_of(&bundle).map(|st| (origin, st))).filter(|(_, st)| commit_of(st) == Some(tip.as_str())).collect();
        let both = of_tip.iter().any(|(o, _)| o == "kiln") && of_tip.iter().any(|(o, _)| o == "leg");
        Ok(of_tip.first().and_then(|(_, st)| st.pointer("/subject/0/digest/sha256")).and_then(Value::as_str).filter(|_| both).map(|h| format!("sha256:{h}")))
    })?;
    post(&s, "/api/deploy/apps", Some(&json!({ "name": "app", "project": "acme", "package": { "kind": "helm", "repo": "oci://ghcr.io/acme/charts", "chart": "app" } })))?;
    for env in ["prod", "staging"] {
        post(&s, "/api/deploy/environments", Some(&json!({ "name": env, "provider": "kilncd" })))?;
        post(&s, &format!("/api/deploy/apps/app/environments/{env}"), Some(&json!({})))?;
    }
    release(&s, "prod", "1.0.0", &digest)?;
    Ok(Shop { s, _runner: runner, sam, work, tip, digest })
}

/// app released to `env` as `version`, its image `digest`.
fn release(s: &Server, env: &str, version: &str, digest: &str) -> anyhow::Result<()> {
    post(s, "/api/deploy/releases", Some(&json!({ "app": "app", "environment": env, "version": version, "artifacts": { "app": { "repository": "registry.example/acme/app", "tag": version, "digest": digest } } })))?;
    Ok(())
}

/// The in-toto statement a kept bundle signs.
fn statement_of(bundle: &str) -> Option<Value> {
    let b: Value = serde_json::from_str(bundle).ok()?;
    let payload = base64::engine::general_purpose::STANDARD.decode(b.pointer("/dsseEnvelope/payload")?.as_str()?).ok()?;
    serde_json::from_slice(&payload).ok()
}

/// The commit a provenance says it built from.
fn commit_of(st: &Value) -> Option<&str> {
    st.pointer("/predicate/buildDefinition/resolvedDependencies/0/digest/gitCommit").and_then(Value::as_str)
}

fn links(v: &Value) -> Vec<String> {
    v.get("links").and_then(Value::as_array).into_iter().flatten().map(|l| text(l, "link").to_owned()).collect()
}

/// `kiln verify` of the release (`q`, as the API takes it): its answer, which must not verify, and the
/// link it names, and why.
fn broken(s: &Server, q: &str) -> anyhow::Result<(String, String)> {
    let v = get(s, &format!("/api/verify?{q}"))?;
    anyhow::ensure!(v.get("verified") == Some(&json!(false)), "{q} verified: {v:#}");
    let b = v.get("broken").cloned().unwrap_or_default();
    Ok((text(&b, "link").to_owned(), text(&b, "why").to_owned()))
}

fn verified(s: &Server, q: &str) -> anyhow::Result<Value> {
    let v = get(s, &format!("/api/verify?{q}"))?;
    anyhow::ensure!(v.get("verified") == Some(&json!(true)), "{q} doesn't verify: {v:#}");
    Ok(v)
}

/// `kiln verify kind id`, the CLI, against the shop's server: whether it succeeded, and what it said.
fn cli(s: &Server, kind: &str, id: &str) -> anyhow::Result<(bool, String)> {
    let out = common::kiln().args(["verify", kind, id, "--url", &format!("http://127.0.0.1:{}", s.port), "--token", &s.admin]).output()?;
    Ok((out.status.success(), format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))))
}

/// W27-AC07: a deploy of 10 commits, each with its review, CI and the build, walked back to sam:
/// the release, its artifact and the leg that built it, each commit on main (signed by whoever made
/// it) and the land that put it there by the queue that tested it, kim's approval and main's
/// policy met, sam's revision; kiln's root. On a warm server, within 2 s.
#[test]
fn w27_ac07_a_deploy_verifies_end_to_end() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let shop = shop(dir.path(), 10)?;
    let s = &shop.s;
    // Warm: the first walk reads each version's patch-id, and signs the trail's checkpoint.
    verified(s, "deploy=prod/app@1.0.0")?;
    let started = Instant::now();
    let v = verified(s, "deploy=prod/app@1.0.0")?;
    let took = started.elapsed();
    let walked = v.get("took_ms").and_then(Value::as_u64).unwrap_or(u64::MAX);
    let l = links(&v);
    let count = |f: &dyn Fn(&str) -> bool| l.iter().filter(|x| f(x)).count();
    assert!(l.first().is_some_and(|x| x.starts_with("deploy prod/app@1.0.0")), "{l:#?}");
    assert!(l.iter().any(|x| x.starts_with(&format!("artifact app sha256:{}", shop.digest.trim_start_matches("sha256:").get(..12).unwrap_or_default()))), "{l:#?}");
    assert!(l.iter().any(|x| x.starts_with("build leg 'image' of run") && x.ends_with(&format!("on {}", common::machine("fedora-2")))), "{l:#?}");
    assert!(l.iter().any(|x| x.starts_with(&format!("{} on main", shop.tip.get(..7).unwrap_or_default()))), "main's head first: {l:#?}");
    assert_eq!(count(&|x| x.contains(" on main, ")), 10, "each commit on main, signed: {l:#?}");
    assert_eq!(count(&|x| x.contains(" landed on main by its queue")), 10, "{l:#?}");
    assert_eq!(count(&|x| x.starts_with("approved by kim (v1")), 10, "{l:#?}");
    assert_eq!(count(&|x| x.contains("met main's review policy")), 10, "{l:#?}");
    assert!(count(&|x| x.ends_with(", signed by sam")) >= 10, "each revision sam's: {l:#?}");
    assert!(l.last().is_some_and(|x| x.starts_with("kiln's root")), "{l:#?}");
    eprintln!("kiln verify of a deploy of 10 commits, {} links: {took:?} ({walked} ms walking)", l.len());
    assert!(took <= Duration::from_secs(2) && walked <= 2000, "kiln verify of 10 commits took {took:?} ({walked} ms walking)");
    let (ok, said) = cli(s, "deploy", "prod/app@1.0.0")?;
    assert!(ok && said.contains("verified ("), "kiln verify deploy: {said}");
    // And as kiln's operation (MCP's tool).
    let op = post(s, "/api/ops/ci.verify", Some(&json!({ "deploy": "prod/app@1.0.0" })))?;
    assert_eq!((op.get("verified"), links(&op).len()), (Some(&json!(true)), l.len()), "{op:#}");
    Ok(())
}

/// W27-AC07: kiln verify names each tampered link. An artifact rebuilt (released by a digest kiln's
/// CI never attested); a forged review (kim's accept carrying another statement's signature); a
/// trail entry edited (the land's, its time moved a second); a commit that wasn't tested (sam's,
/// pushed straight to main). Each undone, the chain is whole again.
#[test]
fn w27_ac07_kiln_verify_names_each_broken_link() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let shop = shop(dir.path(), 2)?;
    let s = &shop.s;
    verified(s, "deploy=prod/app@1.0.0")?;
    let db = rusqlite::Connection::open(dir.path().join("data/kiln.db"))?;
    db.busy_timeout(Duration::from_secs(10))?;

    // An artifact rebuilt.
    release(s, "staging", "1.0.1", &format!("sha256:{}", "ab".repeat(32)))?;
    let (link, why) = broken(s, "deploy=staging/app@1.0.1")?;
    assert!(link.starts_with("artifact app sha256:abababababab") && why.contains("no provenance"), "{link}: {why}");

    // A forged review: kim's accept of D2, signed (it says) with the land's signature.
    let land = get(s, &format!("/api/repos/acme%2Fapp/lands/{}", shop.tip))?;
    let kims = get(s, "/api/diffs/D2/verdicts")?.get("verdicts").and_then(Value::as_array).and_then(|l| l.first()).map(|v| text(v, "digest").to_owned()).unwrap_or_default();
    let kept: String = db.query_row("SELECT bundle FROM trail_signatures WHERE statement = ?1", [&kims], |r| r.get(0))?;
    let mut forged: Value = serde_json::from_str(&kept)?;
    let another = land.pointer("/bundle/dsseEnvelope/signatures/0/sig").cloned().unwrap_or_default();
    if let Some(sig) = forged.pointer_mut("/dsseEnvelope/signatures/0/sig") {
        *sig = another;
    }
    db.execute("UPDATE trail_signatures SET bundle = ?2 WHERE statement = ?1", [&kims, &forged.to_string()])?;
    let (link, why) = broken(s, "deploy=prod/app@1.0.0")?;
    assert!(link == "approved by kim" && why.starts_with("its verdict on D2 v1"), "{link}: {why}");
    db.execute("UPDATE trail_signatures SET bundle = ?2 WHERE statement = ?1", [&kims, &kept])?;
    verified(s, "deploy=prod/app@1.0.0")?;

    // A trail entry edited: the land's.
    let landed = text(&land, "digest").to_owned();
    db.execute("UPDATE trail SET taken_at = taken_at + 1 WHERE statement = ?1", [&landed])?;
    let (link, why) = broken(s, "deploy=prod/app@1.0.0")?;
    assert!(link == format!("{} landed by main's queue", shop.tip.get(..7).unwrap_or_default()) && why.contains("isn't in the tree its checkpoint"), "{link}: {why}");
    db.execute("UPDATE trail SET taken_at = taken_at - 1 WHERE statement = ?1", [&landed])?;
    verified(s, "deploy=prod/app@1.0.0")?;

    // A commit that wasn't tested: sam's, signed, pushed straight to main (main's policy lets sam).
    let (sam, work) = (&shop.sam, &shop.work);
    sam.git(work, &["fetch", "-q", "origin", "main"])?;
    sam.git(work, &["checkout", "-q", "-B", "hot", "FETCH_HEAD"])?;
    std::fs::write(work.join("hot.txt"), "a hot fix\n")?;
    sam.git(work, &["add", "-A"])?;
    sam.git(work, &["commit", "-qm", "a hot fix"])?;
    sam.git(work, &["push", "-q", "origin", "HEAD:main"])?;
    let hot = sam.git(work, &["rev-parse", "HEAD"])?.trim().to_owned();
    let (link, why) = broken(s, &format!("commit=acme/app@{hot}"))?;
    assert!(link == format!("{} landed by main's queue", hot.get(..7).unwrap_or_default()) && why.contains("wasn't tested"), "{link}: {why}");
    let first = get(s, &format!("/api/verify?commit=acme/app@{hot}"))?;
    assert!(links(&first).first().is_some_and(|x| x.ends_with(" on main, signed by sam")), "on main and sam's: {first:#}");
    let (ok, said) = cli(s, "commit", &format!("acme/app@{hot}"))?;
    assert!(!ok && said.contains("the chain breaks at") && said.contains("wasn't tested"), "kiln verify commit: {said}");
    Ok(())
}

/// The chain gate's result on release `r` (as the deploy API answers it).
fn chain_gate(r: &Value) -> Value {
    r.get("gates").and_then(Value::as_array).into_iter().flatten().find(|g| g.get("gate") == Some(&json!("chain"))).cloned().unwrap_or_default()
}

/// W27-AC08: a production environment refuses a deploy whose chain doesn't verify, naming the
/// link, and takes one whose chain does; an environment from before keeps what it has. `live`,
/// made as production, requires the chain from the start: the rebuilt image is refused at its
/// artifact, main's build taken with its chain verified. `staging`, made before, takes the rebuilt
/// one, saying its chain doesn't verify; made production since, it still doesn't require it.
#[test]
fn w27_ac08_production_refuses_an_unverified_chain() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let shop = shop(dir.path(), 1)?;
    let s = &shop.s;
    let require_chain = |env: &str| -> anyhow::Result<Option<Value>> { Ok(get(s, &format!("/api/deploy/environments/{env}"))?.pointer("/gates/require_chain").cloned()) };
    let ask = |env: &str, version: &str, digest: &str| json!({ "app": "app", "environment": env, "version": version, "artifacts": { "app": { "repository": "registry.example/acme/app", "tag": version, "digest": digest } } });
    let rebuilt = format!("sha256:{}", "ab".repeat(32));

    post(s, "/api/deploy/environments", Some(&json!({ "name": "live", "provider": "kilncd", "production": true })))?;
    post(s, "/api/deploy/apps/app/environments/live", Some(&json!({})))?;
    assert_eq!(require_chain("live")?, Some(json!(true)), "made as production, it requires the chain");
    let (st, body) = http(s.port, "POST", "/api/deploy/releases", &s.admin, Some(&ask("live", "1.0.1", &rebuilt)))?;
    assert!(st == 409 && body.contains("live requires a verified chain (require_chain): the chain breaks at artifact app sha256:abababababab"), "the rebuilt image, refused at its artifact: {st} {body}");
    let taken = post(s, "/api/deploy/releases", Some(&ask("live", "1.0.0", &shop.digest)))?;
    let gate = chain_gate(&taken);
    assert!(gate.get("passed") == Some(&json!(true)) && text(&gate, "detail").starts_with("its chain verifies"), "main's build, its chain verified: {taken:#}");

    assert_eq!(require_chain("staging")?, None, "from before, it doesn't require the chain");
    let said = post(s, "/api/deploy/releases", Some(&ask("staging", "1.0.1", &rebuilt)))?;
    let gate = chain_gate(&said);
    assert!(gate.get("passed") == Some(&json!(false)) && text(&gate, "detail").ends_with("(not required here)") && text(&gate, "detail").contains("the chain breaks at artifact app"), "taken, saying its chain doesn't verify: {said:#}");
    let (st, body) = http(s.port, "PUT", "/api/deploy/environments/staging", &s.admin, Some(&json!({ "name": "staging", "provider": "kilncd", "production": true })))?;
    anyhow::ensure!((200..300).contains(&st), "staging made production: {st} {body}");
    assert_eq!(require_chain("staging")?, None, "made production since, it keeps what it had");
    Ok(())
}

/// W27-R10: a deploy's trail exported for an auditor verifies offline against kiln's root with no
/// server, and doesn't once a statement or a proof in it is changed. A revision's Source VSA (SLSA's
/// source track) is signed as kiln's server, says what the source track asks, and is in the trail.
#[test]
fn w27_r10_an_exported_trail_verifies_offline() -> anyhow::Result<()> {
    let b64 = base64::engine::general_purpose::STANDARD;
    let dir = tempfile::tempdir()?;
    let shop = shop(dir.path(), 1)?;
    let s = &shop.s;
    let export = get(s, "/api/verify/export?deploy=prod/app@1.0.0")?;
    let (st, pem) = http(s.port, "GET", "/ca.pem", "", None)?;
    anyhow::ensure!(st == 200, "kiln's root: {st} {pem}");
    let root = dir.path().join("ca.pem");
    std::fs::write(&root, &pem)?;
    let offline = |e: &Value, name: &str| -> anyhow::Result<(bool, String)> {
        let file = dir.path().join(name);
        std::fs::write(&file, serde_json::to_vec_pretty(e)?)?;
        let out = common::kiln().args(["verify", "--offline"]).arg(&file).arg("--root").arg(&root).env_remove("KILN_URL").env_remove("KILN_ADMIN_TOKEN").output()?;
        Ok((out.status.success(), format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))))
    };
    let (ok, said) = offline(&export, "trail.json")?;
    assert!(ok && said.contains("verified offline") && said.contains("→ the trail's checkpoint"), "the export, offline: {said}");

    // A statement a link rests on, changed; then its proof.
    let rests = export.get("links").and_then(Value::as_array).into_iter().flatten().find_map(|l| l.pointer("/rests_on/0").and_then(Value::as_str)).map(str::to_owned).ok_or_else(|| anyhow::anyhow!("no link rests on a statement: {export:#}"))?;
    let at = export.get("statements").and_then(Value::as_array).and_then(|list| list.iter().position(|x| x.pointer("/entry/statement").and_then(Value::as_str) == Some(rests.as_str()))).ok_or_else(|| anyhow::anyhow!("{rests} isn't exported"))?;
    let mut changed = export.clone();
    if let Some(p) = changed.pointer_mut(&format!("/statements/{at}/payload")) {
        *p = json!(b64.encode(br#"{"_type":"https://in-toto.io/Statement/v1","predicate":{"edited":true}}"#));
    }
    let (ok, said) = offline(&changed, "changed.json")?;
    assert!(!ok && said.contains("the chain breaks at") && said.contains("names"), "a statement changed: {said}");
    let mut proof = export.clone();
    if let Some(h) = proof.pointer_mut(&format!("/statements/{at}/proof/0")) {
        *h = json!("00".repeat(32));
    }
    let (ok, said) = offline(&proof, "proof.json")?;
    assert!(!ok && said.contains("isn't in the tree"), "a proof changed: {said}");

    // The revision that's released: its Source VSA.
    let v = post(s, "/api/verify/vsa", Some(&json!({ "commit": format!("acme/app@{}", shop.tip) })))?;
    assert_eq!(v.get("verified"), Some(&json!(true)), "{v:#}");
    let vsa = v.get("vsa").cloned().unwrap_or_default();
    let at = |p: &str| vsa.pointer(p).cloned().unwrap_or_default();
    assert_eq!((at("/predicateType"), at("/subject/0/digest/gitCommit"), at("/subject/0/annotations/sourceRefs")), (json!("https://slsa.dev/verification_summary/v1"), json!(shop.tip), json!(["refs/heads/main"])), "{vsa:#}");
    assert_eq!((at("/predicate/verificationResult"), at("/predicate/verifiedLevels")), (json!("PASSED"), json!(["SLSA_SOURCE_LEVEL_3"])), "{vsa:#}");
    assert!(text(&vsa, "_type") == "https://in-toto.io/Statement/v1" && at("/predicate/resourceUri").as_str().is_some_and(|u| u.starts_with("git+") && u.ends_with("/acme/app")), "{vsa:#}");
    // Signed as kiln's server: its DSSE signature, under the certificate it carries, to kiln's root.
    let now = i64::try_from(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs())?;
    let bundle = v.get("bundle").cloned().unwrap_or_default();
    let field = |p: &str| -> anyhow::Result<Vec<u8>> { Ok(b64.decode(bundle.pointer(p).and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("no {p}: {bundle}"))?)?) };
    let (payload_type, payload) = (text(bundle.get("dsseEnvelope").unwrap_or(&Value::Null), "payloadType"), field("/dsseEnvelope/payload")?);
    assert_eq!(serde_json::from_slice::<Value>(&payload)?, vsa, "the bundle signs the VSA");
    let leaf = field("/verificationMaterial/certificate/rawBytes")?;
    let signer = kiln_ci::identity::chain::verify_signed(&[&leaf], &kiln::pki::der_of(&pem)?, &[], now, &kiln_ci::attest::pae(payload_type, &payload), &field("/dsseEnvelope/signatures/0/sig")?)?;
    assert_eq!(signer.identity.uri().map_err(|e| anyhow::anyhow!("{e}"))?, "kiln://hyper-light/server", "signed as kiln's server");
    let digest = kiln_ci::identity::statement::digest(payload_type, &payload);
    let entries = get(s, "/api/trail?from=0&limit=1000")?;
    assert!(entries.get("entries").and_then(Value::as_array).into_iter().flatten().any(|e| text(e, "statement") == digest), "the Source VSA in the trail");
    Ok(())
}
