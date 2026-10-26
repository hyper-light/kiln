//! W27-R01 (docs/ci.md): revocations are recorded and checked. A runner forgotten has its
//! certificate revoked, with when, why and by whom; after the server restarts, the old
//! certificate is still refused; the machine enrolling again under the same name gets a new key
//! that works, and the old certificate stays refused.
//!
//! `cargo test -p kiln --test identities`

#![cfg(unix)]

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use common::agents::{self, Talk};
use common::{Options, Server, http, start};
use serde_json::Value;

const TALK: Talk = Talk { lines: 1, every_ms: 10 };

/// How many times the runner's log says the server took it: a QUIC connection alone ("connected
/// to server") comes before the TLS directory decides.
fn connections(log: &Path) -> usize {
    std::fs::read_to_string(log).unwrap_or_default().matches("server welcomed agent").count()
}

/// Whether the server takes the runner again (its log says so once more) within `within`.
fn connects_again(log: &Path, within: Duration) -> bool {
    let (seen, deadline) = (connections(log), Instant::now() + within);
    while Instant::now() < deadline {
        if connections(log) > seen {
            return true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    false
}

/// The revocations of forgotten runners' machine certificates (runner processes' own
/// certificates are revoked as they're superseded, besides).
fn revocations(port: u16, admin: &str) -> anyhow::Result<Vec<Value>> {
    let (st, body) = http(port, "GET", "/api/identities/revocations", admin, None)?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let all = serde_json::from_str::<Value>(&body)?.get("revocations").and_then(Value::as_array).cloned().unwrap_or_default();
    Ok(all.into_iter().filter(|r| r.get("reason").and_then(Value::as_str) == Some("runner forgotten")).collect())
}

#[test]
fn w27_r01_revocation_survives_restart_and_reenrolling_keeps_it() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut s = start(dir.path(), &Options::default())?;
    let state = dir.path().join("fedora-2-state");

    // Enrolled and connected; then forgotten.
    let (runner, log) = agents::runner(dir.path(), s.port, "fedora-2", None, TALK)?;
    agents::connected(s.port, &s.admin, "fedora-2", &log, Duration::from_secs(60))?;
    // Its own log has said so too (it may trail the server's list), so what follows counts anew.
    let deadline = Instant::now() + Duration::from_secs(30);
    while connections(&log) == 0 {
        anyhow::ensure!(Instant::now() < deadline, "the runner never logged its welcome");
        std::thread::sleep(Duration::from_millis(100));
    }
    let old_cert = std::fs::read_to_string(state.join("agent.pem"))?;
    let (st, body) = http(s.port, "DELETE", "/api/agents/fedora-2", &s.admin, None)?;
    anyhow::ensure!(st == 204, "forget: {st} {body}");
    let revs = revocations(s.port, &s.admin)?;
    anyhow::ensure!(revs.len() == 1, "one certificate revoked: {revs:?}");
    let rev = revs.first().cloned().unwrap_or_default();
    assert_eq!(rev.get("reason").and_then(Value::as_str), Some("runner forgotten"));
    assert_eq!(rev.get("by").and_then(Value::as_str), Some("admin"));
    assert!(rev.get("at").and_then(Value::as_i64).is_some_and(|at| at > 1_700_000_000), "{rev}");
    assert!(!connects_again(&log, Duration::from_secs(5)), "refused at once:\n{}", std::fs::read_to_string(&log)?);
    drop(runner);

    // The server restarts: the old certificate is still refused (before W27 a restart forgot it).
    s.restart()?;
    let (runner, log) = agents::runner(dir.path(), s.port, "fedora-2", None, TALK)?;
    assert!(!connects_again(&log, Duration::from_secs(8)), "the revoked certificate came back after a restart:\n{}\n{}", std::fs::read_to_string(&log)?, s.log());
    drop(runner);
    assert_eq!(revocations(s.port, &s.admin)?.len(), 1, "the revocation is kept");

    // Enrolled again under the same name: a new key, which works.
    let old_state = dir.path().join("fedora-2-old-state");
    std::fs::rename(&state, &old_state)?;
    let (_runner, log) = agents::runner(dir.path(), s.port, "fedora-2", None, TALK)?;
    agents::connected(s.port, &s.admin, "fedora-2", &log, Duration::from_secs(60))?;
    assert_ne!(std::fs::read_to_string(state.join("agent.pem"))?, old_cert, "enrolling again brings a new certificate");

    // The old certificate is still refused: a copy of the old runner, beside the new one.
    let copy = dir.path().join("fedora-2-copy-state");
    std::fs::rename(&old_state, &copy)?;
    let (_copy, copy_log) = agents::runner(dir.path(), s.port, "fedora-2-copy", None, TALK)?;
    assert!(!connects_again(&copy_log, Duration::from_secs(8)), "the old certificate connected after the name enrolled again:\n{}", std::fs::read_to_string(&copy_log)?);
    assert_eq!(revocations(s.port, &s.admin)?.len(), 1, "re-enrolling undid nothing");
    Ok(())
}

/// The identities kiln issued of `kind`, each with who it verifies as now (or why not).
fn identities(s: &Server, kind: &str) -> anyhow::Result<Vec<Value>> {
    let (st, body) = http(s.port, "GET", &format!("/api/identities?kind={kind}"), &s.admin, None)?;
    anyhow::ensure!(st == 200, "{st} {body}");
    Ok(serde_json::from_str::<Value>(&body)?.get("identities").and_then(Value::as_array).cloned().unwrap_or_default())
}

/// Waits until `f` holds of the identities of `kind`.
fn until_identities(s: &Server, kind: &str, within: Duration, f: impl Fn(&[Value]) -> bool) -> anyhow::Result<Vec<Value>> {
    let deadline = Instant::now() + within;
    loop {
        let list = identities(s, kind)?;
        if f(&list) {
            return Ok(list);
        }
        anyhow::ensure!(Instant::now() < deadline, "identities of {kind}: {list:?}\n{}", s.log());
        std::thread::sleep(Duration::from_millis(300));
    }
}

fn field<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or_default()
}

/// A second runner process of machine `name`, beside its first (a blue/green upgrade's), on the
/// same state.
fn instance(dir: &Path, port: u16, name: &str, id: &str) -> anyhow::Result<(agents::Runner, std::path::PathBuf)> {
    let join = std::fs::read_to_string(dir.join("data/join.token"))?.trim().to_owned();
    let log_path = dir.join(format!("{name}-{id}.log"));
    let log = std::fs::OpenOptions::new().create(true).append(true).open(&log_path)?;
    let child = common::kiln()
        .args(["agent", "--server", &format!("http://127.0.0.1:{port}"), "--token", &join, "--name", name, "--instance", id])
        .arg("--work-dir")
        .arg(dir.join(format!("{name}-{id}-work")))
        .arg("--state-dir")
        .arg(dir.join(format!("{name}-state")))
        .env("HOME", dir.join("home"))
        .env("NO_COLOR", "1")
        .stdout(std::process::Stdio::from(log.try_clone()?))
        .stderr(std::process::Stdio::from(log))
        .spawn()?;
    Ok((agents::Runner(child), log_path))
}

/// W27-R01, R02: each runner process has a key of its own, made where it runs, and a certificate
/// naming its machine, its instance, who enrolled the machine and the organization; one retired
/// (gone while another of its machine goes on) is revoked from then.
#[test]
fn w27_r01_each_runner_process_its_own_identity() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options::default())?;
    let (main, log) = agents::runner(dir.path(), s.port, "fedora-2", None, TALK)?;
    agents::connected(s.port, &s.admin, "fedora-2", &log, Duration::from_secs(60))?;
    let main_uri = "kiln://hyper-light/person/admin/machine/fedora-2/instance/main";
    let first = until_identities(&s, "runner", Duration::from_secs(30), |l| l.iter().any(|i| field(i, "subject") == main_uri))?;
    let mine = first.iter().find(|i| field(i, "subject") == main_uri).cloned().unwrap_or_default();
    assert_eq!(field(&mine, "verified"), "runner fedora-2 (instance main), enrolled by admin, of hyper-light", "{mine}");
    // Its certificate is on the machine; its key isn't anywhere on disk.
    let ids = dir.path().join("fedora-2-state/identities");
    let files: Vec<String> = std::fs::read_dir(&ids)?.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    assert!(files.iter().any(|f| f.starts_with("main-runner")), "{files:?}");
    for f in walk(dir.path())? {
        let text = std::fs::read_to_string(&f).unwrap_or_default();
        assert!(!(text.contains("PRIVATE KEY") && f.starts_with(&ids)), "a key written beside the identities: {}", f.display());
    }

    // A second process beside it: a key and certificate of its own.
    let (_b1, b1_log) = instance(dir.path(), s.port, "fedora-2", "b1")?;
    let b1_uri = "kiln://hyper-light/person/admin/machine/fedora-2/instance/b1";
    let both = until_identities(&s, "runner", Duration::from_secs(60), |l| l.iter().any(|i| field(i, "subject") == b1_uri && !field(i, "verified").is_empty()))
        .map_err(|e| anyhow::anyhow!("{e}\n{}", std::fs::read_to_string(&b1_log).unwrap_or_default()))?;
    let b1 = both.iter().find(|i| field(i, "subject") == b1_uri).cloned().unwrap_or_default();
    assert_eq!(field(&b1, "verified"), "runner fedora-2 (instance b1), enrolled by admin, of hyper-light");
    assert_ne!(field(&b1, "key"), field(&mine, "key"), "two processes, two keys");

    // The first goes while the second goes on: retired, its certificate revoked from then.
    drop(main);
    let after = until_identities(&s, "runner", Duration::from_secs(60), |l| l.iter().filter(|i| field(i, "subject") == main_uri).all(|i| field(i, "refused").contains("revoked")))?;
    let gone = after.iter().find(|i| field(i, "subject") == main_uri).cloned().unwrap_or_default();
    assert!(field(&gone, "refused").contains("runner process retired"), "{gone}");
    let still = after.iter().find(|i| field(i, "subject") == b1_uri).cloned().unwrap_or_default();
    assert!(!field(&still, "verified").is_empty(), "the second goes on: {still}");
    Ok(())
}

/// Every file under `dir`.
fn walk(dir: &Path) -> anyhow::Result<Vec<std::path::PathBuf>> {
    let mut out = Vec::new();
    let mut todo = vec![dir.to_path_buf()];
    while let Some(d) = todo.pop() {
        for e in std::fs::read_dir(&d)?.filter_map(|e| e.ok()) {
            let p = e.path();
            if p.is_dir() {
                todo.push(p);
            } else {
                out.push(p);
            }
        }
    }
    Ok(out)
}

/// W27-R01, R02: an agent session has a key per epoch, made by the runner process holding it and
/// kept there; each epoch's certificate names the agent, the person it acts for and the epoch; a
/// hand-over to a newer process raises the epoch, and what the old epoch signs from then is
/// refused. No session key is written anywhere, in the runners' state or the server's.
#[test]
fn w27_r01_an_agent_session_a_key_per_epoch() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options::default())?;
    let (_main, log) = agents::runner(dir.path(), s.port, "box-a", None, TALK)?;
    agents::connected(s.port, &s.admin, "box-a", &log, Duration::from_secs(60))?;
    let cwd = dir.path().join("work-here");
    std::fs::create_dir_all(&cwd)?;
    let id = agents::open_session(s.port, &s.admin, "box-a", &cwd, Duration::from_secs(60))?;
    let agent_of = |l: &[Value], epoch: u64| l.iter().find(|i| field(i, "subject").ends_with(&format!("/agent/{id}/epoch/{epoch}"))).cloned();
    let first = until_identities(&s, "agent", Duration::from_secs(60), |l| l.iter().any(|i| field(i, "subject").contains(&format!("/agent/{id}/epoch/")) && !field(i, "verified").is_empty()))?;
    let e0 = first
        .iter()
        .filter_map(|i| field(i, "subject").rsplit_once("/epoch/").and_then(|(_, e)| e.parse::<u64>().ok()))
        .min()
        .ok_or_else(|| anyhow::anyhow!("no epoch: {first:?}"))?;
    let v0 = agent_of(&first, e0).unwrap_or_default();
    assert_eq!(field(&v0, "verified"), format!("agent {id}, epoch {e0}, for admin, of hyper-light"), "{v0}");

    // Two newer processes of the machine, one after the other: the session is handed to each, an
    // epoch up each time; the epoch it left is refused from the hand-over on.
    let mut keep = Vec::new();
    for (n, inst) in ["z1", "z2"].iter().enumerate() {
        keep.push(instance(dir.path(), s.port, "box-a", inst)?);
        let want = e0 + 1 + u64::try_from(n)?;
        let list = until_identities(&s, "agent", Duration::from_secs(120), |l| {
            agent_of(l, want).is_some_and(|i| !field(&i, "verified").is_empty()) && agent_of(l, want - 1).is_some_and(|i| field(&i, "refused").contains("revoked"))
        })?;
        let now = agent_of(&list, want).unwrap_or_default();
        assert_eq!(field(&now, "verified"), format!("agent {id}, epoch {want}, for admin, of hyper-light"));
        let left = agent_of(&list, want - 1).unwrap_or_default();
        assert!(field(&left, "refused").contains(&format!("moved on to epoch {want}")), "{left}");
        assert_ne!(field(&now, "key"), field(&left, "key"), "a new epoch, a new key");
    }
    let all = identities(&s, "agent")?;
    let epochs: Vec<&Value> = all.iter().filter(|i| field(i, "subject").contains(&format!("/agent/{id}/epoch/"))).collect();
    assert!(epochs.len() >= 3, "three epochs, each certified: {epochs:?}");

    // No session key on disk: not beside the certificates, not in the server's data or logs.
    for f in walk(dir.path())? {
        let name = f.to_string_lossy().into_owned();
        let watched = name.contains("/identities/") || name.ends_with("kiln.db") || name.ends_with(".log");
        if watched {
            let bytes = std::fs::read(&f).unwrap_or_default();
            assert!(!String::from_utf8_lossy(&bytes).contains("PRIVATE KEY"), "a private key in {name}");
        }
    }
    Ok(())
}

/// One request with a session cookie (`None`: none): status, body and head.
fn call(port: u16, method: &str, path: &str, cookie: Option<&str>, body: Option<&Value>) -> anyhow::Result<(u16, String, String)> {
    use std::io::{Read as _, Write as _};
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port))?;
    s.set_read_timeout(Some(Duration::from_secs(30)))?;
    let payload = body.map(Value::to_string).unwrap_or_default();
    let cookie = cookie.map(|c| format!("Cookie: kiln_session={c}\r\n")).unwrap_or_default();
    write!(s, "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{cookie}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}", payload.len())?;
    let mut raw = Vec::new();
    s.read_to_end(&mut raw)?;
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, rest) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status: u16 = head.split(' ').nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
    let body = if head.to_ascii_lowercase().contains("transfer-encoding: chunked") { common::dechunk(rest) } else { rest.to_owned() };
    Ok((status, body, head.to_owned()))
}

/// Person `name` added and signed in with the one-time link the owner makes: their cookie.
fn signed_in(s: &Server, name: &str) -> anyhow::Result<String> {
    let (st, body) = http(s.port, "POST", "/api/users", &s.admin, Some(&serde_json::json!({ "name": name, "grants": ["developer"] })))?;
    anyhow::ensure!((200..300).contains(&st), "{name}: {st} {body}");
    let (st, body) = http(s.port, "POST", &format!("/api/users/{name}/link"), &s.admin, None)?;
    anyhow::ensure!(st == 200, "{name}'s link: {st} {body}");
    let url = serde_json::from_str::<Value>(&body)?.get("url").and_then(Value::as_str).map(str::to_owned).ok_or_else(|| anyhow::anyhow!("no link: {body}"))?;
    let path = url.find("/api/").and_then(|at| url.get(at..)).ok_or_else(|| anyhow::anyhow!("a link without a path: {url}"))?;
    let (_, _, head) = call(s.port, "GET", path, None, None)?;
    head.lines()
        .filter(|l| l.to_ascii_lowercase().starts_with("set-cookie:"))
        .find_map(|l| l.split_once(':').map(|(_, v)| v.trim()).and_then(|v| v.split(';').next()).and_then(|v| v.strip_prefix("kiln_session=")))
        .map(str::to_owned)
        .ok_or_else(|| anyhow::anyhow!("{name} wasn't signed in: {head}"))
}

/// W27-R01, R02: a person's key is made on their own device and certified for a few hours after
/// they sign in; disabling them refuses their certificates from then.
#[test]
fn w27_r01_people_certified_after_sign_in() -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options::default())?;

    // The owner, from the CLI: the key stays in its config, 0600; the server keeps the certificate.
    let config = dir.path().join("owner-config");
    let out = common::kiln()
        .args(["login", "--certificate", "--url", &format!("http://127.0.0.1:{}", s.port), "--token", &s.admin])
        .env("XDG_CONFIG_HOME", &config)
        .env("HOME", dir.path().join("home"))
        .output()?;
    let said = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    anyhow::ensure!(out.status.success(), "kiln login --certificate: {said}");
    assert!(said.contains("Certified: admin of hyper-light"), "{said}");
    let key = config.join("kiln/identity.key");
    assert_eq!(std::fs::metadata(&key)?.permissions().mode() & 0o777, 0o600);
    assert!(config.join("kiln/identity.pem").exists());
    let people = until_identities(&s, "person", Duration::from_secs(10), |l| !l.is_empty())?;
    let owner = people.first().cloned().unwrap_or_default();
    assert_eq!(field(&owner, "verified"), "admin of hyper-light");
    let lived = owner.get("not_after").and_then(Value::as_i64).unwrap_or_default() - owner.get("not_before").and_then(Value::as_i64).unwrap_or_default();
    assert!(lived <= 24 * 3600, "a day at most: {lived} s");

    // A person signed in, their key made on their side.
    let cookie = signed_in(&s, "ada")?;
    let kp = rcgen::KeyPair::generate()?;
    let csr = rcgen::CertificateParams::new(vec!["ada-laptop".to_owned()])?.serialize_request(&kp)?.pem()?;
    let (st, body, _) = call(s.port, "POST", "/api/me/identity", Some(&cookie), Some(&serde_json::json!({ "csr": csr })))?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let v: Value = serde_json::from_str(&body)?;
    assert_eq!(field(&v, "identity"), "ada of hyper-light");
    let ada_uri = field(&v, "uri").to_owned();
    // Nobody signed in gets none.
    let (st, _, _) = call(s.port, "POST", "/api/me/identity", None, Some(&serde_json::json!({ "csr": csr })))?;
    assert!(st == 401 || st == 403, "{st}");

    // Disabled: refused from then.
    let (st, body) = http(s.port, "DELETE", "/api/users/ada", &s.admin, None)?;
    anyhow::ensure!((200..300).contains(&st), "{st} {body}");
    let after = identities(&s, "person")?;
    let ada = after.iter().find(|i| field(i, "subject") == ada_uri).cloned().unwrap_or_default();
    assert!(field(&ada, "refused").contains("person disabled"), "{ada}");
    assert!(!field(after.iter().find(|i| field(i, "subject").ends_with("/person/admin")).unwrap_or(&Value::Null), "verified").is_empty());

    // No private key reached the server.
    let db = std::fs::read(dir.path().join("data/kiln.db")).unwrap_or_default();
    assert!(!String::from_utf8_lossy(&db).contains("PRIVATE KEY"));
    Ok(())
}

fn git(home: &Path, dir: &Path, args: &[&str]) -> anyhow::Result<String> {
    let o = std::process::Command::new("git")
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

/// The leg's steps look for a key in what they can reach (its environment), and find none.
const LEG: &str = r#"on: push
jobs:
  e2e:
    runs-on: local
    steps:
      - run: |
          if env | grep -q "PRIVATE KEY"; then echo "a private key in the environment"; exit 1; fi
          sleep 1
          echo "no key here"
"#;

/// W27-R01, R02: a CI leg has a certificate of its own, for a key its runner made outside the
/// steps' reach, naming its run, attempt and leg, its runner (and who enrolled it) and, in
/// Fulcio's extensions, its commit.
#[test]
fn w27_r01_a_ci_leg_its_own_certificate() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { agent: true, ci: "kiln_workflows = true", extra: "" })?;
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&serde_json::json!({ "name": "acme/app", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let (home, work) = (dir.path().join("githome"), dir.path().join("tree"));
    std::fs::create_dir_all(&home)?;
    std::fs::create_dir_all(work.join(".kiln"))?;
    let push = format!("http://x:{}@127.0.0.1:{}/git/acme/app.git", s.admin, s.port);
    git(&home, &work, &["init", "-q", "-b", "main"])?;
    std::fs::write(work.join("README.md"), "an app\n")?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "first"])?;
    git(&home, &work, &["push", "-q", &push, "HEAD:main"])?;
    // Once the server has polled its trunk, the workflow comes as a push.
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        if !body.contains("\"last_polled_at\":null") {
            break;
        }
        anyhow::ensure!(Instant::now() < deadline, "never polled: {body}");
        std::thread::sleep(Duration::from_millis(300));
    }
    std::fs::write(work.join(".kiln/e2e.yaml"), LEG)?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "the workflow"])?;
    let sha = git(&home, &work, &["rev-parse", "HEAD"])?.trim().to_owned();
    git(&home, &work, &["push", "-q", &push, "HEAD:main"])?;

    // The run finishes, its leg successful (it found no key).
    let deadline = Instant::now() + Duration::from_secs(300);
    loop {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme/app&limit=10", &s.admin, None)?;
        let runs: Vec<Value> = serde_json::from_str::<Value>(&body)?.get("runs").and_then(Value::as_array).cloned().unwrap_or_default();
        if let Some(r) = runs.iter().find(|r| r.get("finished_at").is_some_and(|f| !f.is_null())) {
            assert_eq!(r.get("conclusion").and_then(Value::as_str), Some("success"), "{r}\n{}", s.log());
            break;
        }
        anyhow::ensure!(Instant::now() < deadline, "no finished run: {body}\n{}", s.log());
        std::thread::sleep(Duration::from_millis(500));
    }
    let legs = until_identities(&s, "leg", Duration::from_secs(30), |l| l.iter().any(|i| !field(i, "verified").is_empty()))?;
    let leg = legs.iter().find(|i| !field(i, "verified").is_empty()).cloned().unwrap_or_default();
    let said = field(&leg, "verified");
    let short = sha.get(..7).unwrap_or_default();
    assert!(said.starts_with(&format!("leg e2e of run 1 (attempt 1) at {short}, on runner ")), "{said}");
    assert!(said.ends_with(", enrolled by admin, of hyper-light"), "{said}");
    assert!(field(&leg, "subject").contains("/run/1/attempt/1/leg/e2e"), "{leg}");
    Ok(())
}

/// W27-R01: the server and CD sign as themselves, under certificates of their own whose keys stay
/// in the server's data (0600), kept across a restart.
#[test]
fn w27_r01_the_server_and_cd_their_own_identities() -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir()?;
    let mut s = start(dir.path(), &Options::default())?;
    let server = until_identities(&s, "server", Duration::from_secs(10), |l| !l.is_empty())?;
    assert_eq!(field(server.first().unwrap_or(&Value::Null), "verified"), "kiln server of hyper-light");
    let cd = until_identities(&s, "cd", Duration::from_secs(10), |l| !l.is_empty())?;
    assert_eq!(field(cd.first().unwrap_or(&Value::Null), "verified"), "CD, for the kiln server of hyper-light");
    for k in ["server-signing.key", "cd-signing.key"] {
        assert_eq!(std::fs::metadata(dir.path().join("data/pki").join(k))?.permissions().mode() & 0o777, 0o600, "{k}");
    }
    // A restart keeps them: nothing new issued.
    s.restart()?;
    assert_eq!(identities(&s, "server")?.len(), 1);
    assert_eq!(identities(&s, "cd")?.len(), 1);
    Ok(())
}
