//! The security suite (docs/ci.md W22-AC07), against a server of its own:
//! - every route refuses a token that may do nothing;
//! - every YAML kiln reads from a repository refuses a bomb, at once;
//! - webhooks and channels never reach this machine or a private network unless allowed;
//! - a secret a run uses appears nowhere: not in its logs, the API, the events or the database's
//!   files.
//!
//! Fork isolation is W18-AC03's, signed URLs W08-AC04's, and archives the runner's own test
//! (`w22_ac07_tarball_stays_in_the_workspace`).

#![cfg(unix)]

mod common;

use std::io::{Read as _, Write as _};
use std::path::Path;
use std::time::{Duration, Instant};

use common::{Options, Server, concrete, http, start};
use serde_json::{Value, json};

/// A token that may do nothing: an agent's with everything its user may, for a user without a grant.
fn powerless(s: &Server) -> anyhow::Result<String> {
    let (st, body) = http(s.port, "POST", "/api/users", &s.admin, Some(&json!({ "name": "nobody", "grants": [] })))?;
    anyhow::ensure!((200..300).contains(&st), "user: {st} {body}");
    let (st, body) = http(s.port, "POST", "/api/tokens", &s.admin, Some(&json!({ "kind": "agent", "agent": "prowler", "user": "nobody", "grants": ["owner"] })))?;
    anyhow::ensure!((200..300).contains(&st), "token: {st} {body}");
    serde_json::from_str::<Value>(&body)?.get("token").and_then(Value::as_str).map(str::to_owned).ok_or_else(|| anyhow::anyhow!("no token in {body}"))
}

#[test]
fn w22_ac07_every_route_refuses_a_powerless_token() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options::default())?;
    let token = powerless(&s)?;
    let mut wrong = Vec::new();
    let mut checked = 0;
    for rule in kiln::authz::ROUTES {
        let path = concrete(rule.pattern);
        let methods: &[&str] = match rule.method {
            Some(m) => &[m][..],
            None => &["GET", "POST"],
        };
        for method in methods {
            checked += 1;
            // No token at all: refused everywhere, the signed-in routes too.
            let (anon, _) = http(s.port, method, &path, "", Some(&json!({})))?;
            if anon != 401 {
                wrong.push(format!("{method} {path} without a token: {anon}"));
            }
            // Signed-in routes are anyone's; a caller's own tokens are theirs (checked below).
            if rule.capability == kiln::authz::SIGNED_IN || rule.at == kiln::authz::At::Own {
                continue;
            }
            let (status, body) = http(s.port, method, &path, &token, Some(&json!({})))?;
            if status != 403 {
                wrong.push(format!("{method} {path} ({}): {status} {}", rule.capability, body.chars().take(120).collect::<String>()));
            }
        }
    }
    assert!(checked > 90, "{checked} routes");
    assert!(wrong.is_empty(), "{} of {checked} let through what they shouldn't:\n{}", wrong.len(), wrong.join("\n"));
    // Tokens are each caller's own: someone else's is neither listed nor reachable.
    let (st, body) = http(s.port, "POST", "/api/users", &s.admin, Some(&json!({ "name": "alice", "grants": ["viewer"] })))?;
    anyhow::ensure!((200..300).contains(&st), "alice: {st} {body}");
    let (st, body) = http(s.port, "POST", "/api/tokens", &s.admin, Some(&json!({ "kind": "agent", "agent": "helper", "user": "alice", "grants": ["viewer"] })))?;
    anyhow::ensure!((200..300).contains(&st), "alice's token: {st} {body}");
    let theirs = serde_json::from_str::<Value>(&body)?.get("id").and_then(Value::as_str).map(str::to_owned).unwrap_or_default();
    let (_, listed) = http(s.port, "GET", "/api/tokens", &token, None)?;
    assert!(!listed.contains("agent:helper") && !theirs.is_empty() && !listed.contains(&theirs), "{listed}");
    let (st, _) = http(s.port, "DELETE", &format!("/api/tokens/{theirs}"), &token, None)?;
    assert!(st == 403 || st == 404, "deleting someone else's token: {st}");
    let (_, still) = http(s.port, "GET", "/api/tokens", &s.admin, None)?;
    assert!(still.contains(&theirs), "it is still there");
    Ok(())
}

/// A billion laughs: ten levels of ten aliases each.
fn bomb(top: &str) -> String {
    let mut y = String::from("a: &a [\"lol\", \"lol\", \"lol\", \"lol\", \"lol\", \"lol\", \"lol\", \"lol\", \"lol\", \"lol\"]\n");
    let mut prev = "a".to_owned();
    for i in 0..9 {
        let name = format!("b{i}");
        y.push_str(&format!("{name}: &{name} [*{prev}, *{prev}, *{prev}, *{prev}, *{prev}, *{prev}, *{prev}, *{prev}, *{prev}, *{prev}]\n"));
        prev = name;
    }
    y.push_str(&format!("{top}: *{prev}\n"));
    y
}

/// `parse` refuses `text`, in well under a second.
fn refuses<T>(what: &str, text: &str, parse: impl FnOnce(&str) -> Result<T, String>) {
    let t = Instant::now();
    let r = parse(text);
    assert!(r.is_err(), "{what} took a bomb");
    assert!(t.elapsed() < Duration::from_secs(2), "{what} took {:?} over it", t.elapsed());
}

/// A parse that came out without an error.
fn some<T>((w, d): (Option<T>, Vec<kiln_ci::wf::diag::Diag>)) -> Result<(), String> {
    w.filter(|_| d.iter().all(|x| !x.is_error())).map(|_| ()).ok_or_else(|| "refused".to_owned())
}

#[test]
fn w22_ac07_yaml_bombs() {
    use kiln_ci::expr::Dialect;
    use kiln_ci::wf;
    refuses("a GitHub workflow", &bomb("jobs"), |t| some(wf::parse_workflow(".github/workflows/x.yml", t, Dialect::Gha)));
    refuses("a kiln workflow", &bomb("jobs"), |t| some(wf::parse_workflow(".kiln/x.yaml", t, Dialect::Kiln)));
    refuses("an overlay", &bomb("jobs"), |t| some(wf::parse_overlay(t)));
    refuses("an action", &bomb("runs"), |t| some(wf::parse_action(t)));
    refuses(".kiln/routing.yaml", &bomb("rules"), |t| kiln::ci::routing::parse_file(t.as_bytes()));
    refuses(".kiln/notify.yaml", &bomb("rules"), kiln::ci::notify::parse_rules);
    refuses(".kiln/config.yaml", &bomb("actions"), |t| serde_yaml::from_str::<kiln::ci::repo_config::RepoConfig>(t).map_err(|e| e.to_string()));
    refuses(".kiln/review.yaml", &bomb("trunks"), |t| serde_yaml::from_str::<kiln::ci::hosting::Review>(t).map_err(|e| e.to_string()));
    refuses("pnpm-lock.yaml", &format!("lockfileVersion: '9.0'\n{}", bomb("packages")), |t| kiln_ci::sbom::parse(kiln_ci::sbom::Lock::Pnpm, t));
    refuses("yarn.lock (berry)", &format!("__metadata:\n  version: 6\n{}", bomb("\"x@npm:1\"")), |t| kiln_ci::sbom::parse(kiln_ci::sbom::Lock::Yarn, t));
}

#[test]
fn w22_ac07_webhooks_reach_no_private_network() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options::default())?;
    for (kind, name, config, secret) in [
        ("webhook", "inside", json!({ "url": "http://127.0.0.1:9/hook" }), Some("k")),
        ("webhook", "metadata", json!({ "url": "http://169.254.169.254/latest/meta-data/" }), Some("k")),
        ("slack", "slack-in", json!({}), Some("http://10.0.0.1/services/x")),
    ] {
        let (st, body) = http(s.port, "POST", "/api/ci/integrations", &s.admin, Some(&json!({ "kind": kind, "name": name, "config": config, "secret": secret, "scope": "server" })))?;
        anyhow::ensure!((200..300).contains(&st), "{name}: {st} {body}");
        let (st, body) = http(s.port, "POST", &format!("/api/ci/integrations/{name}/verify"), &s.admin, None)?;
        let v: Value = serde_json::from_str(&body).unwrap_or_default();
        let error = v.get("error").and_then(Value::as_str).unwrap_or_default();
        assert!(st == 200 && v.get("ok") == Some(&json!(false)) && error.contains("private address"), "{name}: {st} {body}");
    }
    Ok(())
}

/// The events a server sends (`/api/events`), gathered until `until` passes or `stop` says.
fn events(port: u16, token: &str, until: Instant, stop: std::sync::Arc<std::sync::atomic::AtomicBool>) -> std::thread::JoinHandle<String> {
    let token = token.to_owned();
    std::thread::spawn(move || {
        let mut got = Vec::new();
        let Ok(mut s) = std::net::TcpStream::connect(("127.0.0.1", port)) else { return String::new() };
        let _ = s.set_read_timeout(Some(Duration::from_millis(500)));
        // A WebSocket: the server's frames are unmasked, so what it sends is there as it is.
        let _ = write!(s, "GET /api/events HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {token}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n");
        let mut buf = [0u8; 16 << 10];
        while Instant::now() < until && !stop.load(std::sync::atomic::Ordering::SeqCst) {
            match s.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => got.extend_from_slice(buf.get(..n).unwrap_or_default()),
                Err(_) => {}
            }
        }
        String::from_utf8_lossy(&got).into_owned()
    })
}

fn git(dir: &Path, args: &[&str]) -> anyhow::Result<()> {
    let o = std::process::Command::new("git").args(args).current_dir(dir).env("GIT_AUTHOR_NAME", "t").env("GIT_AUTHOR_EMAIL", "t@example.invalid").env("GIT_COMMITTER_NAME", "t").env("GIT_COMMITTER_EMAIL", "t@example.invalid").output()?;
    anyhow::ensure!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
    Ok(())
}

/// Every file under `dir`, whole (the database's write-ahead log too).
fn files(dir: &Path, out: &mut Vec<(String, Vec<u8>)>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.filter_map(Result::ok) {
        let p = e.path();
        if p.is_dir() {
            files(&p, out);
        } else if let Ok(b) = std::fs::read(&p) {
            out.push((p.display().to_string(), b));
        }
    }
}

#[test]
fn w22_ac07_secrets_never_leak() -> anyhow::Result<()> {
    const CANARY: &str = "kiln-canary-7f3e9a2d41c8";
    let dir = tempfile::tempdir()?;
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(repo.join(".github/workflows"))?;
    std::fs::write(repo.join("README.md"), "leak test\n")?;
    git(&repo, &["init", "-q", "-b", "main"])?;
    git(&repo, &["add", "."])?;
    git(&repo, &["commit", "-qm", "start"])?;
    let extra = format!("[[repos]]\nname = \"acme/leak\"\nclone_url = \"file://{}\"\nbranches = [\"main\"]\npoll_seconds = 1\n", repo.display());
    let s = start(dir.path(), &Options { agent: true, ci: "github_workflows = \"on\"", extra: &extra })?;
    let (st, body) = http(s.port, "PUT", "/api/ci/secrets/LEAKY?scope=repo:acme/leak", &s.admin, Some(&json!({ "value": CANARY })))?;
    anyhow::ensure!((200..300).contains(&st), "secret: {st} {body}");
    // Its first poll takes the branch as it is; the workflow comes after, as a push.
    let polled = Instant::now() + Duration::from_secs(60);
    loop {
        let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        let v: Value = serde_json::from_str(&body).unwrap_or_default();
        let list = v.as_array().cloned().or_else(|| v.get("repos").and_then(Value::as_array).cloned()).unwrap_or_default();
        if list.iter().any(|r| r.get("name").and_then(Value::as_str) == Some("acme/leak") && r.get("last_polled_at").is_some_and(|t| !t.is_null())) {
            break;
        }
        anyhow::ensure!(Instant::now() < polled, "the repository was never polled: {body}");
        std::thread::sleep(Duration::from_millis(300));
    }
    std::fs::write(
        repo.join(".github/workflows/leak.yml"),
        r###"on: push
jobs:
  leak:
    runs-on: [self-hosted, local]
    outputs:
      o: ${{ steps.s.outputs.out }}
    steps:
      - id: s
        env:
          LEAKY: ${{ secrets.LEAKY }}
        run: |
          echo "plain: ${{ secrets.LEAKY }}"
          echo "env: $LEAKY"
          echo "::warning::annotated $LEAKY"
          echo "::notice title=$LEAKY::in a title"
          echo "## summary $LEAKY" >> "$GITHUB_STEP_SUMMARY"
          echo "out=$LEAKY" >> "$GITHUB_OUTPUT"
          echo "LEAKED=$LEAKY" >> "$GITHUB_ENV"
      - run: echo "next step $LEAKED"
"###,
    )?;
    git(&repo, &["add", "."])?;
    git(&repo, &["commit", "-qm", "leak"])?;
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stream = events(s.port, &s.admin, Instant::now() + Duration::from_secs(150), std::sync::Arc::clone(&stop));
    // The run, to its end.
    let deadline = Instant::now() + Duration::from_secs(120);
    let run = loop {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/leak&limit=5", &s.admin, None)?;
        let v: Value = serde_json::from_str(&body).unwrap_or_default();
        let done = v.get("runs").and_then(Value::as_array).and_then(|r| r.first()).filter(|r| r.get("finished_at").is_some_and(|f| !f.is_null())).cloned();
        if let Some(r) = done {
            break r;
        }
        anyhow::ensure!(Instant::now() < deadline, "the run didn't finish: {body}");
        std::thread::sleep(Duration::from_millis(500));
    };
    let id = run.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
    assert_eq!(run.get("conclusion").and_then(Value::as_str), Some("success"), "{run}");

    // What the API shows of it.
    let mut seen: Vec<(String, String)> = Vec::new();
    for path in [format!("/api/ci/runs/{id}"), format!("/api/ci/runs/{id}/annotations"), format!("/api/ci/runs/{id}/summary"), format!("/api/ci/runs/{id}/graph"), "/api/ci/runs?limit=5".into()] {
        let (status, body) = http(s.port, "GET", &path, &s.admin, None)?;
        // A scan of an error page would prove nothing.
        assert_eq!(status, 200, "{path}: {body}");
        seen.push((path, body));
    }
    // What the secret went into, masked.
    assert!(seen.iter().any(|(p, b)| p.ends_with("/summary") && b.contains("summary ***")), "the summary: {seen:?}");
    assert!(seen.iter().any(|(p, b)| p.ends_with("/annotations") && b.contains("annotated ***")), "the annotation: {seen:?}");
    let (_, detail) = http(s.port, "GET", &format!("/api/ci/runs/{id}"), &s.admin, None)?;
    let legs: Vec<String> = serde_json::from_str::<Value>(&detail)?.get("legs").and_then(Value::as_array).into_iter().flatten().filter_map(|l| l.get("id").and_then(Value::as_str).map(str::to_owned)).collect();
    assert!(!legs.is_empty(), "{detail}");
    let mut logs = String::new();
    for leg in &legs {
        for step in 0..6 {
            let (_, body) = http(s.port, "GET", &format!("/api/ci/legs/{leg}/logs?step={step}"), &s.admin, None)?;
            logs.push_str(&body);
        }
        let (status, body) = http(s.port, "GET", &format!("/api/ci/legs/{leg}"), &s.admin, None)?;
        assert_eq!(status, 200, "the leg: {body}");
        seen.push((format!("/api/ci/legs/{leg}"), body));
    }
    // The secret was used: the logs show it masked.
    assert!(logs.contains("plain: ***") && logs.contains("next step ***"), "{logs}");
    seen.push(("logs".into(), logs));
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    let streamed = stream.join().unwrap_or_default();
    assert!(streamed.starts_with("HTTP/1.1 101") && streamed.contains("\"type\""), "no events came: {}", streamed.chars().take(200).collect::<String>());
    seen.push(("events".into(), streamed));
    for (what, text) in &seen {
        assert!(!text.contains(CANARY), "the secret is in {what}");
    }
    // Nor anywhere on disk, the database's files included.
    let mut all = Vec::new();
    files(&dir.path().join("data"), &mut all);
    assert!(all.len() > 3, "{} files", all.len());
    for (path, bytes) in &all {
        assert!(!bytes.windows(CANARY.len()).any(|w| w == CANARY.as_bytes()), "the secret is in {path}");
    }
    Ok(())
}

/// W10: lists narrowed row by row. A person who may read one repository lists runs without
/// saying which, and gets that repository's alone; asking for another gets nothing; the owner
/// gets both.
#[test]
fn w10_run_lists_narrowed_to_what_the_caller_reads() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { agent: false, ci: "kiln_workflows = true", ..Options::default() })?;
    for name in ["acme/a", "acme/b"] {
        let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": name, "mode": "native" })))?;
        anyhow::ensure!(st == 200, "{name}: {st} {body}");
        let work = dir.path().join(name.replace('/', "-"));
        std::fs::create_dir_all(&work)?;
        git(&work, &["init", "-q", "-b", "main"])?;
        std::fs::write(work.join("README.md"), "x\n")?;
        git(&work, &["add", "."])?;
        git(&work, &["commit", "-qm", "first"])?;
        let url = format!("http://x:{}@127.0.0.1:{}/git/{name}.git", s.admin, s.port);
        git(&work, &["push", "-q", &url, "main"])?;
        let deadline = Instant::now() + Duration::from_secs(60);
        while http(s.port, "GET", "/api/repos", &s.admin, None)?.1.contains("\"last_polled_at\":null") {
            anyhow::ensure!(Instant::now() < deadline, "never polled");
            std::thread::sleep(Duration::from_millis(300));
        }
        std::fs::create_dir_all(work.join(".kiln"))?;
        std::fs::write(work.join(".kiln/ci.yaml"), "on: push\njobs:\n  a:\n    runs-on: nowhere\n    steps:\n      - run: echo waits\n")?;
        git(&work, &["add", "."])?;
        git(&work, &["commit", "-qm", "ci"])?;
        git(&work, &["push", "-q", &url, "main"])?;
    }
    let repos_of = |token: &str, path: &str| -> anyhow::Result<Vec<String>> {
        let (st, body) = http(s.port, "GET", path, token, None)?;
        anyhow::ensure!(st == 200, "{path}: {st} {body}");
        let v: Value = serde_json::from_str(&body)?;
        let list = v.get("runs").cloned().unwrap_or(v);
        let mut names: Vec<String> = list.as_array().into_iter().flatten().filter_map(|r| r.get("repo").or_else(|| r.get("repo_name")).and_then(Value::as_str)).map(str::to_owned).collect();
        names.sort();
        names.dedup();
        Ok(names)
    };
    let deadline = Instant::now() + Duration::from_secs(60);
    while repos_of(&s.admin, "/api/ci/runs")? != ["acme/a", "acme/b"] {
        anyhow::ensure!(Instant::now() < deadline, "both repositories' runs: {:?}", repos_of(&s.admin, "/api/ci/runs"));
        std::thread::sleep(Duration::from_millis(300));
    }
    let (st, body) = http(s.port, "POST", "/api/users", &s.admin, Some(&json!({ "name": "dev", "grants": ["developer@repo:acme/a"] })))?;
    anyhow::ensure!((200..300).contains(&st), "user: {st} {body}");
    let (st, body) = http(s.port, "POST", "/api/tokens", &s.admin, Some(&json!({ "kind": "pat", "user": "dev", "grants": ["owner"] })))?;
    anyhow::ensure!((200..300).contains(&st), "token: {st} {body}");
    let dev = serde_json::from_str::<Value>(&body)?.get("token").and_then(Value::as_str).map(str::to_owned).unwrap_or_default();
    assert_eq!(repos_of(&dev, "/api/ci/runs")?, ["acme/a"]);
    assert_eq!(repos_of(&dev, "/api/ci/runs?repo=acme/b")?, Vec::<String>::new());
    assert_eq!(repos_of(&dev, "/api/runs")?, ["acme/a"]);
    assert_eq!(repos_of(&s.admin, "/api/runs")?, ["acme/a", "acme/b"]);
    // The overview: dev's is acme/a's alone, with none of the server's (runners, its host).
    let (st, body) = http(s.port, "GET", "/api/overview", &dev, None)?;
    anyhow::ensure!(st == 200, "dev's overview: {st} {body}");
    let mine: Value = serde_json::from_str(&body)?;
    assert_eq!(mine.get("narrowed"), Some(&json!(["acme/a"])), "{mine}");
    assert_eq!(mine.pointer("/stats/runs_24h"), Some(&json!(1)), "{mine}");
    assert!(mine.get("agents").is_some_and(Value::is_null) && mine.pointer("/system/hostname").is_some_and(Value::is_null), "{mine}");
    let all: Value = serde_json::from_str(&http(s.port, "GET", "/api/overview", &s.admin, None)?.1)?;
    assert!(all.get("narrowed").is_none() && all.get("agents").is_some_and(Value::is_object), "{all}");
    assert_eq!(all.pointer("/stats/runs_24h"), Some(&json!(2)), "{all}");
    // The event stream: dev hears acme/a's new run, and nothing of acme/b's.
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let heard = events(s.port, &dev, Instant::now() + Duration::from_secs(60), std::sync::Arc::clone(&stop));
    std::thread::sleep(Duration::from_millis(500));
    for name in ["acme/a", "acme/b"] {
        let work = dir.path().join(name.replace('/', "-"));
        std::fs::write(work.join("README.md"), "again\n")?;
        git(&work, &["commit", "-qam", "again"])?;
        git(&work, &["push", "-q", &format!("http://x:{}@127.0.0.1:{}/git/{name}.git", s.admin, s.port), "main"])?;
    }
    let deadline = Instant::now() + Duration::from_secs(60);
    while repos_of(&s.admin, "/api/ci/runs?limit=10")?.len() < 2 || http(s.port, "GET", "/api/ci/runs?limit=4", &s.admin, None)?.1.matches("\"again\"").count() < 2 {
        anyhow::ensure!(Instant::now() < deadline, "the new runs");
        std::thread::sleep(Duration::from_millis(300));
    }
    std::thread::sleep(Duration::from_secs(2));
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    let heard = heard.join().unwrap_or_default();
    assert!(heard.contains("acme/a") && !heard.contains("acme/b"), "{heard}");
    Ok(())
}

/// W10-R02's `secrets.use`: a run's legs are given only the secrets whoever started it may use.
/// Two people push branches of a kiln-hosted repository: a developer, whose role uses any secret,
/// and one whose grants deny `DEPLOY_KEY` there. The first's run has both of the repository's
/// secrets; the second's has `OTHER` alone, and its leg says `DEPLOY_KEY` was withheld and why.
#[test]
fn w10_secrets_use_holds_a_run_to_whoever_started_it() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { agent: true, ci: "kiln_workflows = true", ..Options::default() })?;
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/held", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "{st} {body}");
    for (name, value) in [("DEPLOY_KEY", "deploy-value-1"), ("OTHER", "other-value-2")] {
        let (st, body) = http(s.port, "PUT", &format!("/api/ci/secrets/{name}?scope=repo:acme/held"), &s.admin, Some(&json!({ "value": value })))?;
        anyhow::ensure!((200..300).contains(&st), "{name}: {st} {body}");
    }
    let person = |name: &str, grants: &[&str]| -> anyhow::Result<String> {
        let (st, body) = http(s.port, "POST", "/api/users", &s.admin, Some(&json!({ "name": name, "grants": grants })))?;
        anyhow::ensure!((200..300).contains(&st), "{name}: {st} {body}");
        let (st, body) = http(s.port, "POST", "/api/tokens", &s.admin, Some(&json!({ "kind": "pat", "user": name, "grants": ["owner"] })))?;
        anyhow::ensure!((200..300).contains(&st), "{name}'s token: {st} {body}");
        Ok(serde_json::from_str::<Value>(&body)?.get("token").and_then(Value::as_str).map(str::to_owned).unwrap_or_default())
    };
    let dev = person("dev", &["developer@repo:acme/held"])?;
    let held = person("held", &["developer@repo:acme/held", "!secrets.use:DEPLOY_KEY@repo:acme/held"])?;
    let url = |token: &str| format!("http://x:{token}@127.0.0.1:{}/git/acme/held.git", s.port);
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work)?;
    git(&work, &["init", "-q", "-b", "main"])?;
    std::fs::write(work.join("README.md"), "held\n")?;
    git(&work, &["add", "."])?;
    git(&work, &["commit", "-qm", "first"])?;
    git(&work, &["push", "-q", &url(&s.admin), "main"])?;
    let deadline = Instant::now() + Duration::from_secs(60);
    while http(s.port, "GET", "/api/repos", &s.admin, None)?.1.contains("\"last_polled_at\":null") {
        anyhow::ensure!(Instant::now() < deadline, "never polled");
        std::thread::sleep(Duration::from_millis(300));
    }
    std::fs::create_dir_all(work.join(".kiln"))?;
    std::fs::write(
        work.join(".kiln/ci.yaml"),
        "on: push\njobs:\n  a:\n    runs-on: local\n    steps:\n      - env:\n          D: ${{ secrets.DEPLOY_KEY }}\n          O: ${{ secrets.OTHER }}\n        run: echo \"deploy=${D:+given} other=${O:+given}\"\n",
    )?;
    git(&work, &["add", "."])?;
    git(&work, &["commit", "-qm", "ci"])?;
    git(&work, &["push", "-q", &url(&s.admin), "main"])?;
    // Each person's branch: its run, its log and its annotations.
    let ran = |branch: &str, token: &str| -> anyhow::Result<(String, String)> {
        git(&work, &["checkout", "-q", "-B", branch, "main"])?;
        std::fs::write(work.join("README.md"), format!("{branch}\n"))?;
        git(&work, &["commit", "-qam", branch])?;
        git(&work, &["push", "-q", &url(token), &format!("HEAD:refs/heads/{branch}")])?;
        let deadline = Instant::now() + Duration::from_secs(120);
        let run = loop {
            let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/held&limit=20", &s.admin, None)?;
            let v: Value = serde_json::from_str(&body).unwrap_or_default();
            let found = v.get("runs").and_then(Value::as_array).into_iter().flatten().find(|r| r.get("branch").and_then(Value::as_str) == Some(branch) && r.get("finished_at").is_some_and(|f| !f.is_null())).cloned();
            if let Some(r) = found {
                break r;
            }
            anyhow::ensure!(Instant::now() < deadline, "{branch}'s run didn't finish: {body}\n{}", s.log());
            std::thread::sleep(Duration::from_millis(500));
        };
        let id = run.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
        let (_, detail) = http(s.port, "GET", &format!("/api/ci/runs/{id}"), &s.admin, None)?;
        let mut logs = String::new();
        for leg in serde_json::from_str::<Value>(&detail)?.get("legs").and_then(Value::as_array).into_iter().flatten().filter_map(|l| l.get("id").and_then(Value::as_str)) {
            for step in 0..4 {
                logs.push_str(&http(s.port, "GET", &format!("/api/ci/legs/{leg}/logs?step={step}"), &s.admin, None)?.1);
            }
        }
        let (_, notes) = http(s.port, "GET", &format!("/api/ci/runs/{id}/annotations"), &s.admin, None)?;
        Ok((logs, notes))
    };
    let (logs, notes) = ran("dev-branch", &dev)?;
    assert!(logs.contains("deploy=given other=given"), "a developer's run has both: {logs}");
    assert!(!notes.contains("withheld"), "{notes}");
    let (logs, notes) = ran("held-branch", &held)?;
    assert!(logs.contains("deploy= other=given"), "DEPLOY_KEY is withheld from the run held pushed: {logs}");
    assert!(notes.contains("secret DEPLOY_KEY withheld") && notes.contains("user:held") && notes.contains("secrets.use:DEPLOY_KEY"), "and the leg says so: {notes}");
    Ok(())
}
