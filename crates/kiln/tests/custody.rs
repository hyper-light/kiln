//! W27-R08 (docs/ci.md; the Transfers plan's step 2, kiln plan
//! `01a17856b1b477eda14a574ae312a123`): custody records taken over the API, kept, walked and
//! listed, through the API and the CLI. The owner hands a workspace to kiln's server, which passes
//! it on to sam; kiln takes an image for the owner, who can't sign, and hands it to sam; an offer
//! sam never takes stays open, its giver holding it; a receipt of something other than what was
//! handed on breaks its chain, and the break is recorded; a record sam signs once their
//! certificate is revoked is refused, and recorded naming them.
//!
//! `cargo test -p kiln --test custody`; the blue/green hand-over needs a release build (an upgrade
//! downloads the server's own kiln, and a debug one is larger than kiln serves), and the runners
//! from before a test build's faults: `cargo test --release -p kiln --features chaos --test
//! custody` runs every one (.kiln/custody-lab.yaml, nightly).

#![cfg(unix)]

mod common;

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use common::agents::{self, Talk};
use common::{Options, http, start};
use kiln_ci::identity::chain::Identity;
use kiln_ci::identity::custody::{Body, Kind, to_value};
use kiln_ci::identity::statement::{PAYLOAD_CUSTODY, Statement, Subject, bundle, digest, payload, to_sign};
use rcgen::SigningKey as _;
use serde_json::{Value, json};

/// A custodian: a key made here, and the certificate kiln's CA issued for it.
struct Signer {
    uri: String,
    key: rcgen::KeyPair,
    der: Vec<u8>,
}

impl Signer {
    fn from_pem(uri: &str, key: &str, cert: &str) -> anyhow::Result<Self> {
        Ok(Self { uri: uri.to_owned(), key: rcgen::KeyPair::from_pem(key)?, der: kiln::pki::der_of(cert)? })
    }

    /// `s`'s payload as this custodian signs it: its body naming it.
    fn payload(&self, s: &Statement) -> anyhow::Result<Vec<u8>> {
        let mut s = s.clone();
        if let Some(o) = s.body.as_object_mut() {
            o.insert("by".into(), Value::String(self.uri.clone()));
        }
        Ok(payload(&s)?)
    }

    fn digest(&self, s: &Statement) -> anyhow::Result<String> {
        Ok(digest(PAYLOAD_CUSTODY, &self.payload(s)?))
    }

    fn sign(&self, s: &Statement) -> anyhow::Result<Value> {
        let p = self.payload(s)?;
        Ok(bundle(PAYLOAD_CUSTODY, &p, &self.key.sign(&to_sign(PAYLOAD_CUSTODY, &p))?, &self.der))
    }
}

/// How a test signs in: the owner's admin token, or a person's session.
#[derive(Clone, Copy)]
enum Auth<'a> {
    Token(&'a str),
    Cookie(&'a str),
}

/// `method path` as whoever `auth` is: its status and body.
fn ask(port: u16, method: &str, path: &str, auth: Auth<'_>, body: Option<&Value>) -> anyhow::Result<(u16, String)> {
    match auth {
        Auth::Token(t) => http(port, method, path, t, body),
        Auth::Cookie(c) => call(port, method, path, Some(c), body).map(|(st, b, _)| (st, b)),
    }
}

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

fn field<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or_default()
}

/// A key of this test's, certified by the server for whoever `auth` signs in as.
fn certified(port: u16, auth: Auth<'_>) -> anyhow::Result<Signer> {
    let key = kiln::pki::new_key_pem()?;
    let csr = kiln::pki::make_csr_with_key("kiln-identity", &key)?;
    let (st, body) = ask(port, "POST", "/api/me/identity", auth, Some(&json!({ "csr": csr })))?;
    anyhow::ensure!(st == 200, "certifying a person's key: {st} {body}");
    let v: Value = serde_json::from_str(&body)?;
    Signer::from_pem(field(&v, "uri"), &key, field(&v, "cert_pem"))
}

/// Person `name` added by the owner, signed in in a browser with the link the owner made: their
/// session's cookie.
fn person(port: u16, admin: &str, name: &str) -> anyhow::Result<String> {
    let (st, body) = http(port, "POST", "/api/users", admin, Some(&json!({ "name": name, "grants": ["developer"] })))?;
    anyhow::ensure!((200..300).contains(&st), "{name}: {st} {body}");
    let (st, body) = http(port, "POST", &format!("/api/users/{name}/link"), admin, None)?;
    anyhow::ensure!(st == 200, "{name}'s link: {st} {body}");
    let url = serde_json::from_str::<Value>(&body)?.get("url").and_then(Value::as_str).map(str::to_owned).ok_or_else(|| anyhow::anyhow!("no link: {body}"))?;
    let path = url.find("/api/").and_then(|at| url.get(at..)).ok_or_else(|| anyhow::anyhow!("a link without a path: {url}"))?;
    let (_, _, head) = call(port, "GET", path, None, None)?;
    head.lines()
        .filter(|l| l.to_ascii_lowercase().starts_with("set-cookie:"))
        .find_map(|l| l.split_once(':').map(|(_, v)| v.trim()).and_then(|v| v.split(';').next()).and_then(|v| v.strip_prefix("kiln_session=")))
        .map(str::to_owned)
        .ok_or_else(|| anyhow::anyhow!("{name}'s sign-in set no session: {head}"))
}

fn thing(name: &str, c: char) -> Subject {
    Subject { name: name.into(), digest: BTreeMap::from([("sha256".to_owned(), c.to_string().repeat(64))]), ..Subject::default() }
}

/// A custody record of `kind` about `what`, to or from someone, following `follows`, numbered
/// `seq` on its signer's stream `stream`.
fn record(kind: Kind, what: &Subject, to: Option<&str>, from: Option<&str>, follows: &[&str], stream: &str, seq: u64) -> Statement {
    let b = Body { to: to.map(str::to_owned), from: from.map(str::to_owned), why: "a test".into(), ..Body::new(kind, "") };
    Statement { subject: vec![what.clone()], follows: follows.iter().map(|d| (*d).to_owned()).collect(), stream: Some(stream.into()), seq: Some(seq), ..Statement::new(to_value(&b)) }
}

/// `POST /api/custody` of `signed` as `auth`: its status and answer.
fn hand(port: u16, auth: Auth<'_>, signed: &Value) -> anyhow::Result<(u16, Value)> {
    let (st, body) = ask(port, "POST", "/api/custody", auth, Some(signed))?;
    Ok((st, serde_json::from_str(&body).unwrap_or_else(|_| json!({ "body": body }))))
}

/// `GET /api/custody?digest=…`: a chain, walked now.
fn walked(port: u16, admin: &str, digest: &str) -> anyhow::Result<Value> {
    let (st, body) = http(port, "GET", &format!("/api/custody?digest={digest}"), admin, None)?;
    anyhow::ensure!(st == 200, "the chain from {digest}: {st} {body}");
    Ok(serde_json::from_str(&body)?)
}

/// `kiln <args>` against the test server, as the owner: what it printed.
fn cli(port: u16, admin: &str, args: &[&str]) -> anyhow::Result<String> {
    let out = common::kiln().args(args).env("KILN_URL", format!("http://127.0.0.1:{port}")).env("KILN_ADMIN_TOKEN", admin).output()?;
    anyhow::ensure!(out.status.success(), "kiln {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The hops of a walked chain without both sides named, or without a signer.
fn half_named(v: &Value) -> usize {
    let hops = v.get("hops").and_then(Value::as_array).cloned().unwrap_or_default();
    hops.iter().filter(|h| field(h, "signer").is_empty() || (field(h, "to").is_empty() && field(h, "from").is_empty())).count()
}

#[test]
fn w27_r08_records_through_the_api_and_the_cli() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options::default())?;
    let (port, admin) = (s.port, s.admin.as_str());
    let owner = certified(port, Auth::Token(admin))?;
    let cookie = person(port, admin, "sam")?;
    let sam = certified(port, Auth::Cookie(&cookie))?;
    let org = Identity::parse(&owner.uri)?.org;
    let server_uri = format!("kiln://{org}/server");
    let pki = dir.path().join("data").join("pki");
    let read = |p: &Path| std::fs::read_to_string(p).map_err(|e| anyhow::anyhow!("{}: {e}", p.display()));
    let kiln = Signer::from_pem(&server_uri, &read(&pki.join("server-signing.key"))?, &read(&pki.join("server-signing.pem"))?)?;

    // The owner hands the workspace to kiln's server, which passes it on to sam, who takes it.
    let ws = thing("workspace tree", 'a');
    let handed = record(Kind::HandedOn, &ws, Some(&server_uri), None, &[], "sync", 1);
    let (st, v) = hand(port, Auth::Token(admin), &owner.sign(&handed)?)?;
    anyhow::ensure!(st == 201, "{st} {v}");
    let h1 = owner.digest(&handed)?;
    assert_eq!(field(&v, "digest"), h1);
    let passed = Statement { body: to_value(&Body { to: Some(sam.uri.clone()), from: Some(owner.uri.clone()), why: "relayed".into(), ..Body::new(Kind::PassedOn, "") }), ..record(Kind::PassedOn, &ws, None, None, &[&h1], "relay", 1) };
    let (st, v) = hand(port, Auth::Token(admin), &kiln.sign(&passed)?)?;
    anyhow::ensure!(st == 201 && v.get("broke") == Some(&Value::Null), "{st} {v}");
    let p1 = kiln.digest(&passed)?;
    let taken = record(Kind::Received, &ws, None, Some(&server_uri), &[&p1], "sync", 1);
    let (st, v) = hand(port, Auth::Cookie(&cookie), &sam.sign(&taken)?)?;
    anyhow::ensure!(st == 201 && v.get("broke") == Some(&Value::Null), "{st} {v}");
    let r1 = sam.digest(&taken)?;
    // Handed again, it's the same record.
    let (st, v) = hand(port, Auth::Cookie(&cookie), &sam.sign(&taken)?)?;
    assert_eq!((st, v.get("again")), (200, Some(&json!(true))), "{v}");

    // The chain, walked from sam's receipt and from the workspace's own digest.
    let chain = walked(port, admin, &r1)?;
    assert_eq!(chain.get("unbroken"), Some(&json!(true)), "{chain}");
    let custodians: Vec<&str> = chain.get("hops").and_then(Value::as_array).map(|h| h.iter().map(|h| field(h, "custodian")).collect()).unwrap_or_default();
    assert_eq!(custodians, vec![sam.uri.as_str(), server_uri.as_str(), owner.uri.as_str()]);
    assert_eq!(field(&chain, "held_by"), sam.uri);
    assert_eq!(field(&walked(port, admin, &format!("sha256:{}", "a".repeat(64)))?, "from"), r1);
    let (st, _) = http(port, "GET", "/api/custody?digest=sha256:feed", admin, None)?;
    assert_eq!(st, 404);

    // kiln takes an image for the owner, who can't sign now, and hands it to sam.
    let img = thing("image kiln:4c1e2a", 'e');
    let land = record(Kind::HandedOn, &img, Some(&owner.uri), None, &[], "land", 1);
    hand(port, Auth::Token(admin), &kiln.sign(&land)?)?;
    let held = record(Kind::Received, &img, None, Some(&server_uri), &[&kiln.digest(&land)?], "sync", 2);
    hand(port, Auth::Token(admin), &owner.sign(&held)?)?;
    let take = Statement {
        body: to_value(&Body { to: Some(sam.uri.clone()), taken_for: Some(owner.uri.clone()), why: "the owner's machine is away".into(), ..Body::new(Kind::Taken, "") }),
        ..record(Kind::Taken, &img, None, None, &[&owner.digest(&held)?], "land", 2)
    };
    hand(port, Auth::Token(admin), &kiln.sign(&take)?)?;
    let got = record(Kind::Received, &img, None, Some(&owner.uri), &[&kiln.digest(&take)?], "sync", 2);
    let (st, v) = hand(port, Auth::Cookie(&cookie), &sam.sign(&got)?)?;
    anyhow::ensure!(st == 201 && v.get("broke") == Some(&Value::Null), "{st} {v}");
    let image_chain = walked(port, admin, &sam.digest(&got)?)?;
    assert_eq!(image_chain.get("unbroken"), Some(&json!(true)), "{image_chain}");
    let kinds: Vec<&str> = image_chain.get("hops").and_then(Value::as_array).map(|h| h.iter().map(|h| field(h, "kind")).collect()).unwrap_or_default();
    assert_eq!(kinds, vec!["received", "taken", "received", "handed-on"]);
    // Every hop of both chains names both its sides and who signed it.
    assert_eq!(half_named(&chain) + half_named(&image_chain), 0);

    // An offer sam never takes: open, the owner still holding it.
    let offer = record(Kind::HandedOn, &thing("report junit.xml", '7'), Some(&sam.uri), None, &[], "sync", 3);
    hand(port, Auth::Token(admin), &owner.sign(&offer)?)?;
    let open = walked(port, admin, &owner.digest(&offer)?)?;
    assert_eq!((open.get("open"), open.get("lapsed"), field(&open, "held_by")), (Some(&json!(true)), Some(&json!(false)), owner.uri.as_str()), "{open}");

    // sam says they received something other than what was passed on: kept, its break recorded.
    let other = record(Kind::Received, &thing("workspace tree", 'b'), None, Some(&server_uri), &[&p1], "sync", 3);
    let (st, v) = hand(port, Auth::Cookie(&cookie), &sam.sign(&other)?)?;
    assert_eq!(st, 201);
    assert!(field(&v, "broke").contains("changed between"), "{v}");
    let (st, body) = http(port, "GET", "/api/custody/breaks", admin, None)?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let breaks: Value = serde_json::from_str(&body)?;
    assert!(breaks.get("breaks").and_then(Value::as_array).is_some_and(|b| b.iter().any(|b| field(b, "kind") == "changed" && field(b, "record") == sam.digest(&other).unwrap_or_default())), "{breaks}");

    // sam's certificate revoked: what they sign after is refused, and recorded naming them.
    let (st, body) = http(port, "POST", "/api/identities/revoke", admin, Some(&json!({ "subject": sam.uri, "reason": "sam's laptop lost" })))?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let late = record(Kind::HandedOn, &thing("goal met", 'd'), Some(&owner.uri), None, &[], "sync", 4);
    let (st, v) = hand(port, Auth::Cookie(&cookie), &sam.sign(&late)?)?;
    assert_eq!(st, 422, "{v}");
    let (st, body) = http(port, "GET", "/api/transfers/refused", admin, None)?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let refused: Value = serde_json::from_str(&body)?;
    let first = refused.get("refused").and_then(Value::as_array).and_then(|r| r.first()).cloned().unwrap_or_default();
    assert_eq!((field(&first, "signer"), field(&first, "digest")), (sam.uri.as_str(), sam.digest(&late)?.as_str()), "{refused}");
    assert!(field(&first, "why").contains("sam's laptop lost"), "{refused}");
    assert!(field(&first, "came_from").starts_with("api:") && field(&first, "came_from").contains("sam"), "{refused}");

    // The CLI says the same.
    let shown = cli(port, admin, &["custody", "show", &r1])?;
    assert!(shown.starts_with("unbroken: 3 hop(s)"), "{shown}");
    assert!(shown.contains(&sam.uri) || shown.contains("sam of"), "{shown}");
    assert!(cli(port, admin, &["custody", "breaks"])?.contains("changed"));
    assert!(cli(port, admin, &["transfers", "refused"])?.contains(&sam.uri));
    Ok(())
}

const TALK: Talk = Talk { lines: 1, every_ms: 10 };

/// A newer runner process `id` of machine `name` (its first one's state folder): a session is
/// handed to it, an epoch up.
fn instance(dir: &Path, port: u16, name: &str, id: &str) -> anyhow::Result<agents::Runner> {
    instance_with(dir, port, name, id, None, &[])
}

/// [`instance`], with a test build's faults (`chaos`) and `env` in its environment.
fn instance_with(dir: &Path, port: u16, name: &str, id: &str, chaos: Option<&str>, env: &[(&str, &str)]) -> anyhow::Result<agents::Runner> {
    let join = std::fs::read_to_string(dir.join("data/join.token"))?.trim().to_owned();
    let log = std::fs::OpenOptions::new().create(true).append(true).open(dir.join(format!("{name}-{id}.log")))?;
    let mut cmd = common::kiln();
    cmd.args(["agent", "--server", &format!("http://127.0.0.1:{port}"), "--token", &join, "--name", name, "--instance", id]);
    if let Some(c) = chaos {
        cmd.args(["--chaos", c]);
    }
    let child = cmd
        .arg("--work-dir")
        .arg(dir.join(format!("{name}-{id}-work")))
        .arg("--state-dir")
        .arg(dir.join(format!("{name}-state")))
        .env("HOME", dir.join("home"))
        .env("NO_COLOR", "1")
        .env("PATH", format!("{}:{}", dir.join("bin").display(), std::env::var("PATH").unwrap_or_default()))
        .env("FAKE_CLAUDE_LINES", TALK.lines.to_string())
        .env("FAKE_CLAUDE_EVERY_MS", TALK.every_ms.to_string())
        .envs(env.iter().copied())
        .stdout(std::process::Stdio::from(log.try_clone()?))
        .stderr(std::process::Stdio::from(log))
        .spawn()?;
    Ok(agents::Runner(child))
}

/// The epoch session `id` was first certified at: once it is, the process holding it can sign it
/// on.
fn first_epoch(port: u16, admin: &str, id: &str) -> anyhow::Result<u64> {
    until(Duration::from_secs(60), || {
        let (st, body) = http(port, "GET", "/api/identities?kind=agent", admin, None)?;
        anyhow::ensure!(st == 200, "{st} {body}");
        let v: Value = serde_json::from_str(&body)?;
        let mark = format!("/agent/{id}/epoch/");
        Ok(v.get("identities").and_then(Value::as_array).and_then(|l| {
            l.iter().filter(|i| !field(i, "verified").is_empty()).find_map(|i| field(i, "subject").split_once(&mark).and_then(|(_, r)| r.split('/').next()).and_then(|e| e.parse().ok()))
        }))
    })
}

/// Once kiln keeps runner `machine`'s copy of the stand-in harness's own record of a session
/// (Claude Code's, under `<dir>/home/.claude`), which a resume from kiln's copy carries: the record
/// written, then the runner's store said to be in step with the server's after its follower can
/// have found the record (`convo::record` looks for one every 15 s) and the runner said so (every
/// 5 s).
fn record_kept(dir: &Path, port: u16, admin: &str, machine: &str) -> anyhow::Result<()> {
    record_written(dir)?;
    let after = kiln::util::now_ms() + 25_000;
    until(Duration::from_secs(120), || {
        let (st, body) = http(port, "GET", "/api/conversations/status", admin, None)?;
        anyhow::ensure!(st == 200, "{st} {body}");
        let v: Value = serde_json::from_str(&body)?;
        let stores = v.get("stores").and_then(Value::as_array).cloned().unwrap_or_default();
        Ok(stores.iter().any(|s| field(s, "machine") == machine && field(s, "role") == "runner" && field(s, "state") == "in_sync" && s.get("heard").and_then(Value::as_i64).is_some_and(|h| h >= after)).then_some(()))
    })
}

/// Once the stand-in harness has written its own record of a session (Claude Code's, in a project
/// folder under `<dir>/home/.claude/projects`: its own, or the one a move installed it in).
fn record_written(dir: &Path) -> anyhow::Result<()> {
    let projects = dir.join("home").join(".claude").join("projects");
    let written = |p: &Path| std::fs::read_dir(p).ok().is_some_and(|d| d.flatten().any(|e| e.path().extension().is_some_and(|x| x == "jsonl") && e.metadata().is_ok_and(|m| m.len() > 0)));
    until(Duration::from_secs(60), || Ok(std::fs::read_dir(&projects).ok().and_then(|d| d.flatten().find(|e| written(&e.path()))).map(|_| ())))
}

/// Whether runner `name` is connected, as the server says.
fn connected(port: u16, admin: &str, name: &str) -> anyhow::Result<bool> {
    let (_, body) = http(port, "GET", "/api/agents", admin, None)?;
    let v: Value = serde_json::from_str(&body).unwrap_or_default();
    let list = v.as_array().cloned().or_else(|| v.get("agents").and_then(Value::as_array).cloned()).unwrap_or_default();
    Ok(list.iter().any(|a| field(a, "name") == name && a.get("connected") == Some(&json!(true))))
}

/// The stand-in harnesses the sessions a test opens in `cwd` run in, stopped when it's dropped:
/// kiln keeps a harness running when its runner goes, as a real machine's does, so a test's
/// machine that goes away for good leaves its harness behind. Each says its pid in `cwd`'s
/// `.fake-claude-messages` as it's sent a message; only one whose home is the test's `home` is
/// stopped (its pid may be another process's since).
struct Harnesses {
    cwd: std::path::PathBuf,
    home: std::path::PathBuf,
}

impl Harnesses {
    fn new(cwd: &Path, home: &Path) -> anyhow::Result<Self> {
        std::fs::create_dir_all(cwd)?;
        std::fs::write(cwd.join(".fake-claude-messages"), "")?;
        Ok(Self { cwd: cwd.to_path_buf(), home: home.to_path_buf() })
    }
}

impl Drop for Harnesses {
    fn drop(&mut self) {
        let said = std::fs::read_to_string(self.cwd.join(".fake-claude-messages")).unwrap_or_default();
        let ours = format!("HOME={}", self.home.display());
        for pid in said.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()?.get("pid")?.as_i64()) {
            let env = std::fs::read(format!("/proc/{pid}/environ")).unwrap_or_default();
            if env.split(|b| *b == 0).any(|v| v == ours.as_bytes())
                && let Ok(pid) = i32::try_from(pid)
            {
                let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), nix::sys::signal::Signal::SIGKILL);
            }
        }
    }
}

/// Polls `f` every 250 ms for up to `within`, until it gives something.
fn until<T>(within: Duration, mut f: impl FnMut() -> anyhow::Result<Option<T>>) -> anyhow::Result<T> {
    let deadline = std::time::Instant::now() + within;
    loop {
        if let Some(v) = f()? {
            return Ok(v);
        }
        anyhow::ensure!(std::time::Instant::now() < deadline, "not within {within:?}");
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// The session's chain of custody, by its timelines' name, once it walks unbroken through `hops`;
/// if it doesn't, what the server and the runner processes in `dir` logged about custody.
fn session_chain(dir: &Path, port: u16, admin: &str, id: &str, hops: usize) -> anyhow::Result<Value> {
    let chain = until(Duration::from_secs(120), || {
        let (st, body) = http(port, "GET", &format!("/api/custody?name=session%20{id}%20at%20epoch%20*"), admin, None)?;
        if st != 200 {
            return Ok(None);
        }
        let v: Value = serde_json::from_str(&body)?;
        let n = v.get("hops").and_then(Value::as_array).map_or(0, Vec::len);
        Ok((v.get("unbroken") == Some(&json!(true)) && n >= hops).then_some(v))
    });
    chain.map_err(|e| anyhow::anyhow!("{e}: {}", said_of(dir, port, admin, id)))
}

/// Session `id`'s chain as it stands, and what the server and the runner processes in `dir` logged
/// of it and of custody: why a test of it didn't go through.
fn said_of(dir: &Path, port: u16, admin: &str, id: &str) -> String {
    let (_, now) = http(port, "GET", &format!("/api/custody?name=session%20{id}%20at%20epoch%20*"), admin, None).unwrap_or_default();
    let mut names: Vec<std::path::PathBuf> = std::fs::read_dir(dir).map(|d| d.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "log")).collect()).unwrap_or_default();
    names.sort();
    let logs: Vec<String> = names
        .iter()
        .map(|n| {
            let text = std::fs::read_to_string(n).unwrap_or_default();
            let lines: Vec<&str> = text.lines().filter(|l| l.contains(id) || ["custody", "took over", "taking over", "hand", "mov", "newest", "upgrad", "promot", "panicked", "fatal", "WARN", "ERROR"].iter().any(|w| l.contains(w))).collect();
            format!("{}:\n{}", n.display(), lines.get(lines.len().saturating_sub(60)..).unwrap_or(&[]).join("\n"))
        })
        .collect();
    format!("its chain now {now}\n{}", logs.join("\n"))
}

/// W27-R08 (the Transfers plan's step 3): an agent session handed to its machine's newer runner
/// process, twice, is signed for by both processes each time: the one it leaves signs its hand-on,
/// at the epoch it leaves; the one taking it signs its receipt, at the next. The chain walks
/// unbroken through both hand-overs, each hop naming the process that signed it.
#[test]
fn w27_r08_session_hand_overs_signed_by_both_processes() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options::default())?;
    let (port, admin) = (s.port, s.admin.as_str());
    let _harnesses = Harnesses::new(&dir.path().join("work-here"), &dir.path().join("home"))?;
    let (_main, log) = agents::runner(dir.path(), port, "box-a", None, TALK)?;
    agents::connected(port, admin, "box-a", &log, Duration::from_secs(60))?;
    let cwd = dir.path().join("work-here");
    std::fs::create_dir_all(&cwd)?;
    let id = agents::open_session(port, admin, "box-a", &cwd, Duration::from_secs(60))?;
    let e0 = first_epoch(port, admin, &id)?;
    let mut keep = Vec::new();
    keep.push(instance(dir.path(), port, "box-a", "z1")?);
    let one = session_chain(dir.path(), port, admin, &id, 2)?;
    let held = |v: &Value| -> Vec<(String, String)> {
        v.get("hops").and_then(Value::as_array).map(|h| h.iter().map(|h| (field(h, "kind").to_owned(), field(h, "custodian").to_owned())).collect()).unwrap_or_default()
    };
    let at = |e: u64, inst: &str| format!("/agent/{id}/epoch/{e}/machine/box-a/instance/{inst}");
    let first = held(&one);
    assert!(first.first().is_some_and(|(k, c)| k == "received" && c.ends_with(&at(e0 + 1, "z1"))), "{one}");
    assert!(first.get(1).is_some_and(|(k, c)| k == "handed-on" && c.ends_with(&at(e0, "main"))), "{one}");
    // Again, to a third process: the chain runs on through both hand-overs.
    keep.push(instance(dir.path(), port, "box-a", "z2")?);
    let two = session_chain(dir.path(), port, admin, &id, 4)?;
    let kinds: Vec<(String, String)> = held(&two);
    let expect = [("received", at(e0 + 2, "z2")), ("handed-on", at(e0 + 1, "z1")), ("received", at(e0 + 1, "z1")), ("handed-on", at(e0, "main"))];
    assert_eq!(kinds.len(), expect.len(), "{two}");
    for ((k, c), (want_k, want_c)) in kinds.iter().zip(expect.iter()) {
        assert!(k == want_k && c.ends_with(want_c.as_str()), "{k} {c}, wanted {want_k} {want_c}: {two}");
    }
    assert_eq!(half_named(&two), 0, "every hop names both its sides and who signed it");
    let (st, body) = http(port, "GET", "/api/custody/breaks", admin, None)?;
    anyhow::ensure!(st == 200, "{st} {body}");
    assert_eq!(serde_json::from_str::<Value>(&body)?.get("breaks"), Some(&json!([])), "{body}");
    Ok(())
}

/// W27-R08 (the Transfers plan's step 3): a session whose machine is away, so can't hand it on,
/// goes on on another from kiln's copy as kiln's take: signed by kiln's server for the process
/// that held it, saying when kiln last heard from that machine, and received by the process it
/// goes to. The chain walks unbroken from that receipt to the take.
#[test]
fn w27_r08_a_session_resumed_from_kilns_copy_is_kilns_take() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options::default())?;
    let (port, admin) = (s.port, s.admin.as_str());
    let _harnesses = Harnesses::new(&dir.path().join("work-here"), &dir.path().join("home"))?;
    // The stand-in keeps Claude Code's own record of the session, which a resume carries.
    let claude = dir.path().join("home").join(".claude").to_string_lossy().into_owned();
    let env = [("FAKE_CLAUDE_RECORD", "1"), ("CLAUDE_CONFIG_DIR", claude.as_str())];
    let (a, log_a) = agents::runner_with(dir.path(), port, "box-a", None, TALK, &env)?;
    let (_b, log_b) = agents::runner_with(dir.path(), port, "box-b", None, TALK, &env)?;
    agents::connected(port, admin, "box-a", &log_a, Duration::from_secs(60))?;
    agents::connected(port, admin, "box-b", &log_b, Duration::from_secs(60))?;
    let cwd = dir.path().join("work-here");
    std::fs::create_dir_all(&cwd)?;
    let id = agents::open_session(port, admin, "box-a", &cwd, Duration::from_secs(60))?;
    let e0 = first_epoch(port, admin, &id)?;
    record_kept(dir.path(), port, admin, "box-a")?;
    // box-a goes away: the session goes on on box-b, from kiln's copy.
    drop(a);
    until(Duration::from_secs(60), || Ok((!connected(port, admin, "box-a")?).then_some(())))?;
    let (st, body) = http(port, "POST", &format!("/api/harness/sessions/{id}/move"), admin, Some(&json!({"to": "box-b"})))?;
    anyhow::ensure!((200..300).contains(&st) && body.contains("from_store"), "{st} {body}");
    let v = session_chain(dir.path(), port, admin, &id, 2)?;
    let hops = v.get("hops").and_then(Value::as_array).cloned().unwrap_or_default();
    let (got, took) = (hops.first().cloned().unwrap_or_default(), hops.get(1).cloned().unwrap_or_default());
    assert!(field(&got, "kind") == "received" && field(&got, "custodian").contains(&format!("/agent/{id}/epoch/{}/machine/box-b/", e0 + 1)), "{v}");
    let server = format!("kiln://{}/server", field(&got, "custodian").trim_start_matches("kiln://").split('/').next().unwrap_or_default());
    assert!(field(&took, "kind") == "taken" && field(&took, "custodian") == server, "{v}");
    assert!(field(&took, "why").contains("box-a was last heard from at"), "{v}");
    assert_eq!(half_named(&v), 0, "every hop names both its sides and who signed it");
    Ok(())
}

/// W27-R08 (the Transfers plan's step 3): a session moved from one machine to another, both
/// online, is signed for by both processes: box-a's at the epoch it leaves, box-b's at the next.
#[test]
fn w27_r08_session_hand_overs_a_move_signed_by_both_machines() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options::default())?;
    let (port, admin) = (s.port, s.admin.as_str());
    let _harnesses = Harnesses::new(&dir.path().join("work-here"), &dir.path().join("home"))?;
    // The stand-in keeps Claude Code's own record of the session, which a move carries.
    let claude = dir.path().join("home").join(".claude").to_string_lossy().into_owned();
    let env = [("FAKE_CLAUDE_RECORD", "1"), ("CLAUDE_CONFIG_DIR", claude.as_str())];
    let (_a, log_a) = agents::runner_with(dir.path(), port, "box-a", None, TALK, &env)?;
    let (_b, log_b) = agents::runner_with(dir.path(), port, "box-b", None, TALK, &env)?;
    agents::connected(port, admin, "box-a", &log_a, Duration::from_secs(60))?;
    agents::connected(port, admin, "box-b", &log_b, Duration::from_secs(60))?;
    let cwd = dir.path().join("work-here");
    std::fs::create_dir_all(&cwd)?;
    let id = agents::open_session(port, admin, "box-a", &cwd, Duration::from_secs(60))?;
    let e0 = first_epoch(port, admin, &id)?;
    record_written(dir.path())?;
    let moved = until(Duration::from_secs(60), || {
        let (st, body) = http(port, "POST", &format!("/api/harness/sessions/{id}/move"), admin, Some(&json!({"to": "box-b"})))?;
        Ok((200..300).contains(&st).then_some(body))
    })?;
    anyhow::ensure!(!moved.contains("from_store"), "a move between two machines online, not a resume: {moved}");
    let v = session_chain(dir.path(), port, admin, &id, 2)?;
    let hops = v.get("hops").and_then(Value::as_array).cloned().unwrap_or_default();
    let (got, gave) = (hops.first().cloned().unwrap_or_default(), hops.get(1).cloned().unwrap_or_default());
    assert!(field(&got, "kind") == "received" && field(&got, "custodian").contains(&format!("/agent/{id}/epoch/{}/machine/box-b/", e0 + 1)), "{v}");
    assert!(field(&gave, "kind") == "handed-on" && field(&gave, "custodian").ends_with(&format!("/agent/{id}/epoch/{e0}/machine/box-a/instance/main")), "{v}");
    assert_eq!(half_named(&v), 0, "every hop names both its sides and who signed it");
    Ok(())
}

/// A runner kept by `kiln supervise`, as `kiln install --agent` runs one (so it upgrades
/// blue/green), in a process group of its own: dropped, the group goes, the supervisor with the
/// runner and the new build it started beside it.
struct Supervised(std::process::Child);

impl Drop for Supervised {
    fn drop(&mut self) {
        if let Ok(pid) = i32::try_from(self.0.id()) {
            let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pid), nix::sys::signal::Signal::SIGKILL);
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Runner `name` under `kiln supervise`, as [`agents::runner`] starts one otherwise, writing to
/// `<dir>/<name>.log` (the new build an upgrade starts beside it too). It runs `exe`, a build other
/// than the server's, so the server has one to upgrade it to.
fn supervised(dir: &Path, port: u16, name: &str, exe: &Path, env: &[(&str, &str)]) -> anyhow::Result<(Supervised, std::path::PathBuf)> {
    use std::os::unix::fs::PermissionsExt as _;
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin)?;
    std::fs::copy(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-claude/claude"), bin.join("claude"))?;
    std::fs::set_permissions(bin.join("claude"), std::fs::Permissions::from_mode(0o755))?;
    let join = std::fs::read_to_string(dir.join("data/join.token"))?.trim().to_owned();
    let home = dir.join("home");
    std::fs::create_dir_all(&home)?;
    let log_path = dir.join(format!("{name}.log"));
    let log = std::fs::OpenOptions::new().create(true).append(true).open(&log_path)?;
    let mut cmd = common::kiln();
    cmd.arg("supervise")
        .arg("--exe")
        .arg(exe)
        .args(["--", "agent", "--server", &format!("http://127.0.0.1:{port}"), "--token", &join, "--name", name])
        .arg("--work-dir")
        .arg(dir.join(format!("{name}-work")))
        .arg("--state-dir")
        .arg(dir.join(format!("{name}-state")))
        .env("HOME", &home)
        .env_remove("GIT_CONFIG_GLOBAL")
        .env("NO_COLOR", "1")
        .env("FAKE_CLAUDE_LINES", TALK.lines.to_string())
        .env("FAKE_CLAUDE_EVERY_MS", TALK.every_ms.to_string())
        .env("PATH", format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default()))
        .envs(env.iter().copied())
        .stdout(std::process::Stdio::from(log.try_clone()?))
        .stderr(std::process::Stdio::from(log));
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    Ok((Supervised(cmd.spawn()?), log_path))
}

/// What the server took again from runner `machine`'s store as it had it already, and what it left
/// out as a former writer's (rows sent twice, or lost to an epoch that rose too soon), since it
/// started: the board's feed intake.
fn sent_again(port: u16, admin: &str, machine: &str) -> anyhow::Result<(u64, u64)> {
    let (st, body) = http(port, "GET", "/api/conversations/status", admin, None)?;
    anyhow::ensure!(st == 200, "{st} {body}");
    let v: Value = serde_json::from_str(&body)?;
    let stores = v.get("stores").and_then(Value::as_array).cloned().unwrap_or_default();
    let n = |k: &str| stores.iter().filter(|s| field(s, "machine") == machine && field(s, "role") == "runner").filter_map(|s| s.pointer(&format!("/feed/intake/{k}")).and_then(Value::as_u64)).sum();
    Ok((n("same"), n("fenced")))
}

/// W27-R08 (the Transfers plan's step 3): a blue/green upgrade hands its machine's session to the
/// new build signed for, an epoch up: the runner upgraded signs its hand-on at the epoch the
/// session had, the new build its receipt at the next once it's promoted, and it goes on at that
/// epoch, with nothing the server's copy has sent to it again.
#[test]
#[cfg_attr(debug_assertions, ignore = "needs a release build: an upgrade downloads the server's own kiln, and a debug one is larger than kiln serves (updates::MAX_BINARY)")]
fn w27_r08_session_hand_overs_blue_green_sends_nothing_again() -> anyhow::Result<()> {
    // The new build is this one, downloaded from the server: hundreds of MB for a debug build, so
    // under the build's own temporary folder, on disk.
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))?;
    let s = start(dir.path(), &Options::default())?;
    let (port, admin) = (s.port, s.admin.as_str());
    let _harnesses = Harnesses::new(&dir.path().join("work-here"), &dir.path().join("home"))?;
    // box-a runs a build of its own (this one, a byte longer), so the server's is newer to it.
    let before_build = dir.path().join("kiln-before");
    let mut bytes = std::fs::read(env!("CARGO_BIN_EXE_kiln"))?;
    bytes.push(0);
    std::fs::write(&before_build, bytes)?;
    std::fs::set_permissions(&before_build, std::os::unix::fs::PermissionsExt::from_mode(0o755))?;
    let (_box_a, log) = supervised(dir.path(), port, "box-a", &before_build, &[])?;
    agents::connected(port, admin, "box-a", &log, Duration::from_secs(60))?;
    let cwd = dir.path().join("work-here");
    std::fs::create_dir_all(&cwd)?;
    let id = agents::open_session(port, admin, "box-a", &cwd, Duration::from_secs(60))?;
    let e0 = first_epoch(port, admin, &id)?;
    let before = sent_again(port, admin, "box-a")?;
    let (st, body) = http(port, "POST", "/api/updates/runners/box-a", admin, None)?;
    anyhow::ensure!(st == 202, "{st} {body}");
    let job = serde_json::from_str::<Value>(&body)?.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
    // What the upgrade, the runner and the server said of it, when it doesn't go through.
    let said = |e: anyhow::Error| {
        let (_, j) = http(port, "GET", &format!("/api/updates/jobs/{job}"), admin, None).unwrap_or_default();
        let lines = |f: &str| {
            let text = std::fs::read_to_string(dir.path().join(f)).unwrap_or_default();
            let hits: Vec<&str> = text.lines().filter(|l| ["upgrad", "build", "green", "hand", "custody", "promot", "take", "ready", "WARN", "ERROR"].iter().any(|w| l.contains(w))).collect();
            hits.get(hits.len().saturating_sub(60)..).unwrap_or(&[]).join("\n")
        };
        anyhow::anyhow!("{e}\njob: {j}\nbox-a:\n{}\nserver:\n{}", lines("box-a.log"), lines("server.log"))
    };
    let v = until(Duration::from_secs(300), || {
        let (st, body) = http(port, "GET", &format!("/api/custody?name=session%20{id}%20at%20epoch%20*"), admin, None)?;
        let v: Value = if st == 200 { serde_json::from_str(&body)? } else { Value::Null };
        let n = v.get("hops").and_then(Value::as_array).map_or(0, Vec::len);
        Ok((v.get("unbroken") == Some(&json!(true)) && n >= 2).then_some(v))
    })
    .map_err(said)?;
    let hops = v.get("hops").and_then(Value::as_array).cloned().unwrap_or_default();
    let (got, gave) = (hops.first().cloned().unwrap_or_default(), hops.get(1).cloned().unwrap_or_default());
    let green = format!("/agent/{id}/epoch/{}/machine/box-a/instance/", e0 + 1);
    assert!(field(&got, "kind") == "received" && field(&got, "custodian").contains(&green) && !field(&got, "custodian").ends_with("/instance/main"), "{v}");
    assert!(field(&gave, "kind") == "handed-on" && field(&gave, "custodian").ends_with(&format!("/agent/{id}/epoch/{e0}/machine/box-a/instance/main")), "{v}");
    assert_eq!(half_named(&v), 0, "every hop names both its sides and who signed it");
    // It goes on at the next epoch, its state saying so, with nothing sent again.
    until(Duration::from_secs(60), || {
        let (_, body) = http(port, "GET", &format!("/api/harness/sessions/{id}"), admin, None)?;
        let i: Value = serde_json::from_str(&body).unwrap_or_default();
        let epoch = i.get("epoch").or_else(|| i.pointer("/info/epoch")).and_then(Value::as_u64);
        Ok((epoch == Some(e0 + 1)).then_some(()))
    })?;
    std::thread::sleep(Duration::from_secs(6));
    assert_eq!(sent_again(port, admin, "box-a")?, before, "rows the server had already, or left out as a former writer's, across the hand-over");
    Ok(())
}

/// Of session `id`'s chain of custody, the transfers: each receipt and the hand-on or take it
/// follows, both signed (the chain walks unbroken), as (epoch received at, its receiver).
fn transfers(v: &Value) -> Vec<(u64, String)> {
    let hops = v.get("hops").and_then(Value::as_array).cloned().unwrap_or_default();
    let epoch = |c: &str| c.split_once("/epoch/").and_then(|(_, r)| r.split('/').next()).and_then(|e| e.parse().ok()).unwrap_or(u64::MAX);
    hops.windows(2)
        .filter(|w| matches!(w, [got, gave] if field(got, "kind") == "received" && matches!(field(gave, "kind"), "handed-on" | "taken") && !field(got, "signer").is_empty() && !field(gave, "signer").is_empty()))
        .filter_map(|w| w.first().map(|got| (epoch(field(got, "custodian")), field(got, "custodian").to_owned())))
        .collect()
}

/// W27-R08 (the Transfers plan's step 3, its test): one session moved to another machine, upgraded
/// there blue/green, taken over by a newer process of that machine, and resumed from kiln's copy
/// once that machine is away: four hand-overs, epochs e0 to e0 + 4, each a verified transfer.
#[test]
#[cfg_attr(debug_assertions, ignore = "needs a release build: an upgrade downloads the server's own kiln, and a debug one is larger than kiln serves (updates::MAX_BINARY)")]
fn w27_r08_session_hand_overs() -> anyhow::Result<()> {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))?;
    let s = start(dir.path(), &Options::default())?;
    let (port, admin) = (s.port, s.admin.as_str());
    let _harnesses = Harnesses::new(&dir.path().join("work-here"), &dir.path().join("home"))?;
    let claude = dir.path().join("home").join(".claude").to_string_lossy().into_owned();
    let env = [("FAKE_CLAUDE_RECORD", "1"), ("CLAUDE_CONFIG_DIR", claude.as_str())];
    let (_a, log_a) = agents::runner_with(dir.path(), port, "box-a", None, TALK, &env)?;
    let before_build = dir.path().join("kiln-before");
    let mut bytes = std::fs::read(env!("CARGO_BIN_EXE_kiln"))?;
    bytes.push(0);
    std::fs::write(&before_build, bytes)?;
    std::fs::set_permissions(&before_build, std::os::unix::fs::PermissionsExt::from_mode(0o755))?;
    let (box_b, log_b) = supervised(dir.path(), port, "box-b", &before_build, &env)?;
    agents::connected(port, admin, "box-a", &log_a, Duration::from_secs(60))?;
    agents::connected(port, admin, "box-b", &log_b, Duration::from_secs(60))?;
    let cwd = dir.path().join("work-here");
    std::fs::create_dir_all(&cwd)?;
    let id = agents::open_session(port, admin, "box-a", &cwd, Duration::from_secs(60))?;
    let e0 = first_epoch(port, admin, &id)?;
    let chain_through = |n: usize, after: &str| -> anyhow::Result<Value> {
        until(Duration::from_secs(300), || {
            let (st, body) = http(port, "GET", &format!("/api/custody?name=session%20{id}%20at%20epoch%20*"), admin, None)?;
            let v: Value = if st == 200 { serde_json::from_str(&body)? } else { Value::Null };
            Ok((v.get("unbroken") == Some(&json!(true)) && transfers(&v).len() >= n).then_some(v))
        })
        .map_err(|e| anyhow::anyhow!("{after}: {n} transfers {e}: {}", said_of(dir.path(), port, admin, &id)))
    };
    // Moved to box-b, both online.
    record_written(dir.path())?;
    until(Duration::from_secs(60), || {
        let (st, _) = http(port, "POST", &format!("/api/harness/sessions/{id}/move"), admin, Some(&json!({"to": "box-b"})))?;
        Ok((200..300).contains(&st).then_some(()))
    })?;
    chain_through(1, "moved")?;
    // box-b upgraded blue/green.
    let (st, body) = http(port, "POST", "/api/updates/runners/box-b", admin, None)?;
    anyhow::ensure!(st == 202, "{st} {body}");
    chain_through(2, "upgraded blue/green")?;
    // Taken over by a newer process of box-b: newer than the build the upgrade started, as its
    // instance record says (an instance started without one ranks oldest).
    let z1_dir = kiln::upgrade::instance_dir(&dir.path().join("box-b-state"), "z1");
    std::fs::create_dir_all(&z1_dir)?;
    std::fs::write(z1_dir.join("instance.json"), serde_json::to_vec(&kiln::upgrade::InstanceRecord { id: "z1".into(), created_at: kiln::util::now_unix() + 1 })?)?;
    let z1 = instance_with(dir.path(), port, "box-b", "z1", None, &env)?;
    chain_through(3, "taken over")?;
    // box-b away: it goes on on box-a, from kiln's copy.
    record_kept(dir.path(), port, admin, "box-b").map_err(|e| anyhow::anyhow!("box-b's copy of the record: {e}: {}", said_of(dir.path(), port, admin, &id)))?;
    drop((z1, box_b));
    until(Duration::from_secs(120), || Ok((!connected(port, admin, "box-b")?).then_some(()))).map_err(|e| {
        let (_, agents) = http(port, "GET", "/api/agents", admin, None).unwrap_or_default();
        anyhow::anyhow!("box-b away: {e}: {agents}")
    })?;
    let (st, body) = http(port, "POST", &format!("/api/harness/sessions/{id}/move"), admin, Some(&json!({"to": "box-a"})))?;
    anyhow::ensure!((200..300).contains(&st) && body.contains("from_store"), "{st} {body}");
    let v = chain_through(4, "resumed")?;
    let got = transfers(&v);
    assert_eq!(got.iter().map(|(e, _)| *e).collect::<Vec<_>>(), vec![e0 + 4, e0 + 3, e0 + 2, e0 + 1], "verified transfers, newest first: {v}");
    assert!(got.first().is_some_and(|(_, c)| c.contains("/machine/box-a/")) && got.get(1).is_some_and(|(_, c)| c.ends_with("/instance/z1")), "{v}");
    assert_eq!(half_named(&v), 0, "every hop names both its sides and who signed it");
    Ok(())
}

/// The hand-overs kiln recorded as unsigned, for session `id`, once there are `n`.
#[cfg(feature = "chaos")]
fn unsigned_of(dir: &Path, port: u16, admin: &str, id: &str, n: usize) -> anyhow::Result<Vec<Value>> {
    let mut seen = Value::Null;
    until(Duration::from_secs(120), || {
        let (st, body) = http(port, "GET", "/api/transfers/unsigned", admin, None)?;
        anyhow::ensure!(st == 200, "{st} {body}");
        let v: Value = serde_json::from_str(&body)?;
        let mine: Vec<Value> = v.get("unsigned").and_then(Value::as_array).cloned().unwrap_or_default().into_iter().filter(|u| field(u, "session") == id).collect();
        seen = Value::Array(mine.clone());
        Ok((mine.len() >= n).then_some(mine))
    })
    .map_err(|e| anyhow::anyhow!("{n} unsigned hand-overs {e}: {seen}: {}", said_of(dir, port, admin, id)))
}

/// W27-R08 (the Transfers plan's step 3): a runner from before signed hand-overs (a test build's
/// `unsigned` fault) hands a session to a newer one, which hands it on to another from before:
/// both hand it over as they did, each hand-over recorded as unsigned, saying which side is from
/// before. `cargo test -p kiln --features chaos --test custody`
#[cfg(feature = "chaos")]
#[test]
fn w27_r08_session_hand_overs_with_runners_from_before_go_unsigned() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options::default())?;
    let (port, admin) = (s.port, s.admin.as_str());
    let _harnesses = Harnesses::new(&dir.path().join("work-here"), &dir.path().join("home"))?;
    let (_main, log) = agents::runner(dir.path(), port, "box-a", Some("unsigned"), TALK)?;
    agents::connected(port, admin, "box-a", &log, Duration::from_secs(60))?;
    let cwd = dir.path().join("work-here");
    std::fs::create_dir_all(&cwd)?;
    let id = agents::open_session(port, admin, "box-a", &cwd, Duration::from_secs(60))?;
    let e0 = first_epoch(port, admin, &id)?;
    // To a newer runner, from one from before: it signed no hand-on.
    let _z1 = instance(dir.path(), port, "box-a", "z1")?;
    let one = unsigned_of(dir.path(), port, admin, &id, 1)?;
    let first = one.iter().find(|u| u.get("epoch") == Some(&json!(e0 + 1))).cloned().unwrap_or_default();
    assert!(field(&first, "gave").ends_with(&format!("/agent/{id}/epoch/{e0}/machine/box-a/instance/main")) && field(&first, "why").contains("signed no hand-on"), "{one:?}");
    // From the newer runner to another from before: it signed no receipt.
    let _z2 = instance_with(dir.path(), port, "box-a", "z2", Some("unsigned"), &[])?;
    let two = unsigned_of(dir.path(), port, admin, &id, 2)?;
    let second = two.iter().find(|u| u.get("epoch") == Some(&json!(e0 + 2))).cloned().unwrap_or_default();
    assert!(field(&second, "gave").ends_with(&format!("/agent/{id}/epoch/{}/machine/box-a/instance/z1", e0 + 1)) && field(&second, "why").contains("signed no receipt"), "{two:?}");
    Ok(())
}

/// W27-AC05 (the Transfers plan's step 4): the 14:18 drop played again. Seven sessions on a machine
/// upgraded blue/green; as kiln certifies each at its next epoch for the new build, the server also
/// tells the runner it took them over from what it told it before transfers named a session's
/// holder (a test build's `stale_words`): hand each to the newest, and that it was taken. Every
/// such word is refused, and recorded naming whom it was for and the runner that refused it; none
/// is applied, and the new build holds all seven.
#[cfg(feature = "chaos")]
#[test]
#[cfg_attr(debug_assertions, ignore = "needs a release build: an upgrade downloads the server's own kiln, and a debug one is larger than kiln serves (updates::MAX_BINARY)")]
fn w27_ac05_the_1418_drop_played_again() -> anyhow::Result<()> {
    const SESSIONS: usize = 7;
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))?;
    let s = common::start_env(dir.path(), &Options::default(), false, &[("KILN_CHAOS", "stale_words")])?;
    let (port, admin) = (s.port, s.admin.as_str());
    let _harnesses = Harnesses::new(&dir.path().join("work-here"), &dir.path().join("home"))?;
    let before_build = dir.path().join("kiln-before");
    let mut bytes = std::fs::read(env!("CARGO_BIN_EXE_kiln"))?;
    bytes.push(0);
    std::fs::write(&before_build, bytes)?;
    std::fs::set_permissions(&before_build, std::os::unix::fs::PermissionsExt::from_mode(0o755))?;
    let (_box_a, log) = supervised(dir.path(), port, "box-a", &before_build, &[])?;
    agents::connected(port, admin, "box-a", &log, Duration::from_secs(60))?;
    let cwd = dir.path().join("work-here");
    let mut sessions = Vec::new();
    for _ in 0..SESSIONS {
        let id = agents::open_session(port, admin, "box-a", &cwd, Duration::from_secs(60))?;
        let e0 = first_epoch(port, admin, &id)?;
        sessions.push((id, e0));
    }
    let (st, body) = http(port, "POST", "/api/updates/runners/box-a", admin, None)?;
    anyhow::ensure!(st == 202, "{st} {body}");
    // Each goes on at its next epoch in the new build, which received it.
    for (id, e0) in &sessions {
        let v = until(Duration::from_secs(300), || {
            let (st, body) = http(port, "GET", &format!("/api/custody?name=session%20{id}%20at%20epoch%20*"), admin, None)?;
            let v: Value = if st == 200 { serde_json::from_str(&body)? } else { Value::Null };
            Ok((v.get("unbroken") == Some(&json!(true)) && field(&v, "held_by").contains(&format!("/agent/{id}/epoch/{}/machine/box-a/", e0 + 1))).then_some(v))
        })
        .map_err(|e| anyhow::anyhow!("session {id}: {e}: {}", said_of(dir.path(), port, admin, id)))?;
        assert!(!field(&v, "held_by").ends_with("/instance/main"), "held by the new build: {v}");
    }
    // The stale words: each refused, and recorded naming whom it was for and who refused it.
    let refused = until(Duration::from_secs(60), || {
        let (st, body) = http(port, "GET", "/api/transfers/refused", admin, None)?;
        anyhow::ensure!(st == 200, "{st} {body}");
        let v: Value = serde_json::from_str(&body)?;
        let words: Vec<Value> = v.get("refused").and_then(Value::as_array).cloned().unwrap_or_default().into_iter().filter(|r| field(r, "act") == "a hand-over's word").collect();
        Ok((words.len() >= 2 * SESSIONS).then_some(words))
    })
    .map_err(|e| anyhow::anyhow!("{e}: {}", sessions.first().map(|(id, _)| said_of(dir.path(), port, admin, id)).unwrap_or_default()))?;
    for (id, _) in &sessions {
        let of: Vec<&Value> = refused.iter().filter(|r| field(r, "why").contains(id.as_str())).collect();
        assert_eq!(of.len(), 2, "both stale words about session {id} refused: {refused:?}");
        assert!(of.iter().all(|r| field(r, "came_from") == "runner box-a (instance main)" && field(r, "why").contains("handed to its machine's new build")), "{of:?}");
    }
    // None applied: every session still runs, held by the new build.
    std::thread::sleep(Duration::from_secs(5));
    for (id, e0) in &sessions {
        let (st, body) = http(port, "GET", &format!("/api/harness/sessions/{id}"), admin, None)?;
        anyhow::ensure!(st == 200, "session {id}: {st} {body}");
        let i: Value = serde_json::from_str(&body).unwrap_or_default();
        let status = i.get("status").or_else(|| i.pointer("/info/status")).and_then(Value::as_str).unwrap_or_default();
        assert!(!matches!(status, "exited" | "closed"), "session {id} went on: {i}");
        let (_, chain) = http(port, "GET", &format!("/api/custody?name=session%20{id}%20at%20epoch%20*"), admin, None)?;
        let v: Value = serde_json::from_str(&chain).unwrap_or_default();
        assert!(field(&v, "held_by").contains(&format!("/agent/{id}/epoch/{}/machine/box-a/", e0 + 1)) && !field(&v, "held_by").ends_with("/instance/main"), "{v}");
    }
    Ok(())
}

/// Operation `name` as an admin calls it (`POST /api/ops/<name>`): its answer.
#[cfg(feature = "chaos")]
fn op(port: u16, admin: &str, name: &str, input: &Value) -> anyhow::Result<Value> {
    let (st, body) = http(port, "POST", &format!("/api/ops/{name}"), admin, Some(input))?;
    anyhow::ensure!((200..300).contains(&st), "{name}: {st} {body}");
    Ok(serde_json::from_str(&body).unwrap_or(Value::Null))
}

/// W27-AC05 (the Transfers plan's step 5): the twin played again. A session moves off a machine
/// cut off from kiln (it goes on on another from kiln's copy); its copy there, still running at the
/// epoch it left, marks its goal met, starts a CI run and starts a run on another machine. None of
/// it is done: each act is refused, and recorded with who signed it; once the machine is back, its
/// runner is told to let go of the copy at its first word; and the session at its next epoch goes
/// on untouched.
#[cfg(feature = "chaos")]
#[test]
fn w27_ac05_the_twin_played_again() -> anyhow::Result<()> {
    use kiln::reach::local::{Client, Ev, Req};
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options::default())?;
    let (port, admin) = (s.port, s.admin.as_str());
    let cwd = dir.path().join("work-here");
    let _harnesses = Harnesses::new(&cwd, &dir.path().join("home"))?;
    let claude = dir.path().join("home").join(".claude").to_string_lossy().into_owned();
    let env = [("FAKE_CLAUDE_RECORD", "1"), ("CLAUDE_CONFIG_DIR", claude.as_str())];
    // box-a's link drops a minute after each connection, and stays away a minute.
    let (_a, log_a) = agents::runner_with(dir.path(), port, "box-a", Some("drop=60s,down=60s"), TALK, &env)?;
    let (_b, log_b) = agents::runner_with(dir.path(), port, "box-b", None, TALK, &env)?;
    let (_c, log_c) = agents::runner_with(dir.path(), port, "box-c", None, TALK, &env)?;
    for (name, log) in [("box-a", &log_a), ("box-b", &log_b), ("box-c", &log_c)] {
        agents::connected(port, admin, name, log, Duration::from_secs(60))?;
    }
    let id = agents::open_session(port, admin, "box-a", &cwd, Duration::from_secs(60))?;
    let e0 = first_epoch(port, admin, &id)?;
    let text = "the twin's goal: never met by a copy left behind";
    op(port, admin, "goals.add", &json!({"agent": id, "goals": [{"text": text}]}))?;
    let stack = op(port, admin, "goals.get", &json!({"agent": id}))?;
    let goal_id = stack.get("items").and_then(Value::as_array).and_then(|l| l.iter().find(|g| field(g, "text") == text)).map(|g| field(g, "id").to_owned()).ok_or_else(|| anyhow::anyhow!("no goal added: {stack}"))?;
    record_kept(dir.path(), port, admin, "box-a")?;
    let copy: i64 = {
        let said = std::fs::read_to_string(cwd.join(".fake-claude-messages"))?;
        said.lines().find_map(|l| serde_json::from_str::<Value>(l).ok()?.get("pid")?.as_i64()).ok_or_else(|| anyhow::anyhow!("the copy's harness said no pid"))?
    };
    // box-a cut off: the session goes on on box-b, from kiln's copy.
    until(Duration::from_secs(120), || Ok((!connected(port, admin, "box-a")?).then_some(())))?;
    let (st, body) = http(port, "POST", &format!("/api/harness/sessions/{id}/move"), admin, Some(&json!({"to": "box-b"})))?;
    anyhow::ensure!((200..300).contains(&st) && body.contains("from_store"), "{st} {body}");
    session_chain(dir.path(), port, admin, &id, 2)?;
    // The copy, cut off and still at epoch e0, acts: its goal met and a CI run, through its runner
    // as its MCP server asks, and a run on box-c, as its own child.
    let sock = kiln::reach::local::path("box-a", &dir.path().join("box-a-state"));
    let ask = |name: &str, input: Value| -> anyhow::Result<String> {
        let mut c = Client::connect(&sock)?;
        c.send(&Req::Ask { json: json!({"op": "call", "session": id, "name": name, "input": input}).to_string() })?;
        Ok(match c.event()? {
            Some(Ev::Answer(a)) => a,
            Some(Ev::Error(e)) => e,
            other => format!("{other:?}"),
        })
    };
    let met = ask("goals.done", json!({"id": goal_id, "evidence": "said by a copy left behind"}))?;
    let ci = ask("ci.runs.trigger", json!({"repo": "hyper-light/kiln", "workflow": "ci.yaml", "ref": "main"}))?;
    let ran = dir.path().join("twin-ran");
    std::fs::write(cwd.join(format!(".fake-claude-acts-{copy}")), format!("{} run --on box-c -- touch {}\n", env!("CARGO_BIN_EXE_kiln"), ran.display()))?;
    let acted = until(Duration::from_secs(60), || Ok(std::fs::read_to_string(cwd.join(".fake-claude-acted")).ok().filter(|t| !t.is_empty())))?;
    // box-a back: its runner is told to let go of the copy, and what the copy tried is recorded.
    until(Duration::from_secs(150), || Ok(connected(port, admin, "box-a")?.then_some(())))?;
    let refused = until(Duration::from_secs(90), || {
        let (_, body) = http(port, "GET", "/api/transfers/refused", admin, None)?;
        let v: Value = serde_json::from_str(&body).unwrap_or_default();
        let of: Vec<Value> = v.get("refused").and_then(Value::as_array).cloned().unwrap_or_default().into_iter().filter(|r| field(r, "act").contains(&id) && field(r, "came_from").starts_with("runner box-a")).collect();
        Ok((of.len() >= 3).then_some(of))
    })
    .map_err(|e| anyhow::anyhow!("{e}: goals.done said {met}; ci said {ci}; the run said {acted}: {}", said_of(dir.path(), port, admin, &id)))?;
    let server_log = std::fs::read_to_string(dir.path().join("server.log")).unwrap_or_default();
    assert!(server_log.contains("its runner is told to let go of it") && server_log.contains(&id), "the copy was told to let go at its runner's first word");
    // None of it done: the goal is still to meet, no CI run started, nothing ran on box-c.
    let goals = op(port, admin, "goals.get", &json!({"agent": id}))?;
    let still = goals.get("items").and_then(Value::as_array).is_some_and(|l| l.iter().any(|g| field(g, "id") == goal_id && field(g, "status") != "met"));
    assert!(still, "the copy's goals.done wasn't applied: {goals}");
    let runs = op(port, admin, "ci.runs.list", &json!({}))?;
    assert!(runs.get("runs").and_then(Value::as_array).is_none_or(Vec::is_empty), "no CI run started: {runs}");
    assert!(!ran.exists(), "nothing ran on box-c: {acted}");
    assert!(refused.iter().any(|r| field(r, "act").contains("goals.done")) && refused.iter().any(|r| field(r, "act").contains("ci.runs.trigger")) && refused.iter().any(|r| field(r, "act").contains("a run on box-c")), "{refused:?}");
    // Each recorded with who signed it, the epoch it's from and the transfer that superseded it.
    let from_e0 = |r: &Value| r.get("epoch").and_then(Value::as_u64) == Some(e0) && field(r, "signer").contains(&format!("/agent/{id}/epoch/{e0}/machine/box-a/")) && !field(r, "superseded_by").is_empty() && !field(r, "digest").is_empty();
    assert!(refused.iter().all(from_e0), "{refused:?}");
    // The session at its next epoch goes on untouched, on box-b.
    let v = session_chain(dir.path(), port, admin, &id, 2)?;
    assert!(field(&v, "held_by").contains(&format!("/agent/{id}/epoch/{}/machine/box-b/", e0 + 1)), "{v}");
    Ok(())
}


/// W27-R08 (the Transfers plan's step 6): work handed between agents and to people, signed by
/// both sides. A plan run with an agent per step, side by side: each agent signs its receipt of
/// kiln's hand-on as it takes its step up, at its first turn. Step 1, tried again, goes to a new
/// agent, kiln taking it for the first, which, asking still as the step's agent, is refused and
/// recorded. Step 2, taken over by the owner from a device with no key, gets kiln's receipt for
/// them; step 3, taken over by sam with a key, gets sam's own. Step 1's agent starts a subagent for
/// a task it hands on signed, which kiln passes on unchanged and the subagent signs for as it takes
/// it up. No transfer without both sides' signatures: every chain walks unbroken, none left open.
#[test]
fn w27_r08_work_handed() -> anyhow::Result<()> {
    use kiln::reach::local::{Client, Ev, Req};
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options::default())?;
    let (port, admin) = (s.port, s.admin.as_str());
    let cwd = dir.path().join("work-here");
    let _harnesses = Harnesses::new(&cwd, &dir.path().join("home"))?;
    let claude = dir.path().join("home").join(".claude").to_string_lossy().into_owned();
    let env = [("FAKE_CLAUDE_RECORD", "1"), ("CLAUDE_CONFIG_DIR", claude.as_str())];
    let (_a, log_a) = agents::runner_with(dir.path(), port, "box-a", None, TALK, &env)?;
    agents::connected(port, admin, "box-a", &log_a, Duration::from_secs(60))?;
    let json_of = |what: &str, (st, body): (u16, String)| -> anyhow::Result<Value> {
        anyhow::ensure!((200..300).contains(&st), "{what}: {st} {body}");
        Ok(serde_json::from_str(&body)?)
    };
    let plan = json_of("the plan", http(port, "POST", "/api/plans", admin, Some(&json!({ "title": "work handed", "body": "- build it\n- test it\n- ship it\n" })))?)?;
    let at = cwd.to_string_lossy().into_owned();
    let ran = json_of("the run", http(port, "POST", &format!("/api/plans/{}/run", field(&plan, "id")), admin, Some(&json!({ "mode": "agents", "parallel": true, "new": { "on": "box-a", "kind": "claude", "cwd": at } })))?)?;
    let run = field(&ran, "run").to_owned();
    let chain_of = |name: &str| -> anyhow::Result<Value> {
        let (_, body) = http(port, "GET", &format!("/api/custody?name={}", name.replace(' ', "%20").replace('\'', "%27")), admin, None)?;
        Ok(serde_json::from_str(&body).unwrap_or_default())
    };
    let hops = |v: &Value| -> Vec<Value> { v.get("hops").and_then(Value::as_array).cloned().unwrap_or_default() };
    let kinds = |v: &Value| -> Vec<String> { hops(v).iter().map(|h| field(h, "kind").to_owned()).collect() };
    let received = |name: &str, by: &str| -> anyhow::Result<Value> {
        until(Duration::from_secs(120), || {
            let v = chain_of(name)?;
            let first = hops(&v).first().cloned().unwrap_or_default();
            Ok((v.get("unbroken") == Some(&json!(true)) && field(&first, "kind") == "received" && field(&first, "custodian").contains(by)).then_some(v))
        })
        .map_err(|e| anyhow::anyhow!("{e}: {name}: {}\n{}", chain_of(name).unwrap_or_default(), said_of(dir.path(), port, admin, by)))
    };
    let agent_of = |n: u64| -> anyhow::Result<String> {
        let v = json_of("the run", http(port, "GET", &format!("/api/plans/runs/{run}"), admin, None)?)?;
        Ok(v.get("steps").and_then(Value::as_array).and_then(|s| s.iter().find(|x| x.get("n") == Some(&json!(n)))).map(|x| field(x, "agent_id").to_owned()).unwrap_or_default())
    };
    let step = |n: u64| format!("plan run {run} step {n}");
    // Each step's agent takes it up at its first turn: kiln's hand-on, and its receipt.
    let mut firsts = Vec::new();
    for n in 1..=3 {
        let a = until(Duration::from_secs(120), || Ok(Some(agent_of(n)?).filter(|a| !a.is_empty())))?;
        let v = received(&step(n), &format!("/agent/{a}/"))?;
        assert_eq!(kinds(&v), ["received", "handed-on"], "{v}");
        firsts.push(a);
    }
    let first = firsts.first().cloned().unwrap_or_default();
    // Step 1 tried again: kiln takes it for its agent and hands it to the next, which signs.
    json_of("the retry", http(port, "POST", &format!("/api/plans/runs/{run}/steps/1/retry"), admin, Some(&json!({})))?)?;
    let second = until(Duration::from_secs(120), || Ok(Some(agent_of(1)?).filter(|a| !a.is_empty() && *a != first)))?;
    let v = received(&step(1), &format!("/agent/{second}/"))?;
    assert_eq!(kinds(&v), ["received", "taken", "received", "handed-on"], "{v}");
    let take = hops(&v).get(1).map(|h| field(h, "record").to_owned()).unwrap_or_default();
    // The first agent, asking still as the step's: refused, and recorded with the take.
    let sock = kiln::reach::local::path("box-a", &dir.path().join("box-a-state"));
    let ask_as = |session: &str, name: &str, input: Value| -> anyhow::Result<String> {
        let mut c = Client::connect(&sock)?;
        c.send(&Req::Ask { json: json!({ "op": "call", "session": session, "name": name, "input": input }).to_string() })?;
        Ok(match c.event()? {
            Some(Ev::Answer(a)) => a,
            Some(Ev::Error(e)) => e,
            other => format!("{other:?}"),
        })
    };
    let said = ask_as(&first, "goals.done", json!({ "evidence": "said by the try before" }))?;
    assert!(said.contains("refused") && said.contains("no longer holds"), "{said}");
    let refused = json_of("the refusals", http(port, "GET", "/api/transfers/refused", admin, None)?)?;
    let of_first: Vec<Value> = refused.get("refused").and_then(Value::as_array).into_iter().flatten().filter(|r| field(r, "act").contains(&first)).cloned().collect();
    assert!(of_first.iter().any(|r| field(r, "superseded_by") == take && field(r, "came_from").starts_with("runner box-a")), "{of_first:?}");
    // Step 2 taken over by the owner, from a device with no key: kiln's receipt for them.
    let took = json_of("step 2 taken over", http(port, "POST", &format!("/api/plans/runs/{run}/steps/2/take"), admin, None)?)?;
    assert!(took.pointer("/custody/kiln_signed_for_you").is_some_and(Value::is_string), "{took}");
    let v = chain_of(&step(2))?;
    let kiln_for = hops(&v).first().cloned().unwrap_or_default();
    assert_eq!(kinds(&v), ["received", "taken", "received", "handed-on"], "{v}");
    assert!(field(&kiln_for, "custodian").ends_with("/server") && field(&kiln_for, "taken_for").ends_with("/person/admin") && field(&kiln_for, "why").contains("no key on the device that asked"), "kiln for the owner, never the owner: {kiln_for}");
    // Step 3 taken over by sam, with a key: sam's own receipt.
    let sam = person(port, admin, "sam")?;
    let sams_key = certified(port, Auth::Cookie(&sam))?;
    let (st, body) = ask(port, "POST", &format!("/api/plans/runs/{run}/steps/3/take"), Auth::Cookie(&sam), Some(&json!({ "signs": true })))?;
    anyhow::ensure!(st == 200, "step 3 taken over by sam: {st} {body}");
    let c = serde_json::from_str::<Value>(&body)?.get("custody").cloned().unwrap_or_default();
    let subject: Subject = serde_json::from_value(c.get("subject").cloned().unwrap_or_default())?;
    let b = Body { from: Some(field(&c, "from").to_owned()), why: "step 3, taken over".into(), ..Body::new(Kind::Received, "") };
    let receipt = sams_key.sign(&Statement { subject: vec![subject], follows: vec![field(&c, "hand_on").to_owned()], ..Statement::new(to_value(&b)) })?;
    let (st, body) = ask(port, "POST", &format!("/api/plans/runs/{run}/steps/3/receipt"), Auth::Cookie(&sam), Some(&json!({ "receipt": receipt })))?;
    anyhow::ensure!(st == 200, "sam's receipt: {st} {body}");
    let v = chain_of(&step(3))?;
    let sams = hops(&v).first().cloned().unwrap_or_default();
    assert!(field(&sams, "custodian") == sams_key.uri && sams.get("taken_for").is_none_or(Value::is_null), "sam's own: {sams}");
    // A subagent: step 1's agent starts one for a task, handing it on signed.
    let started = ask_as(&second, "agents.start", json!({ "kind": "claude", "prompt": "count the tests", "cwd": at }))?;
    let child = serde_json::from_str::<Value>(&started).ok().and_then(|v| v.pointer("/ok/agents/0/id").and_then(Value::as_str).map(str::to_owned)).ok_or_else(|| anyhow::anyhow!("no subagent started: {started}"))?;
    let task = format!("a subagent's task from agent {second}");
    let v = received(&task, &format!("/agent/{child}/"))?;
    assert_eq!(kinds(&v), ["received", "passed-on", "handed-on"], "{v}");
    let custodians: Vec<String> = hops(&v).iter().map(|h| field(h, "custodian").to_owned()).collect();
    assert!(custodians.get(1).is_some_and(|c| c.ends_with("/server")) && custodians.get(2).is_some_and(|c| c.contains(&format!("/agent/{second}/"))), "{custodians:?}");
    // No transfer without both sides' signatures: every chain unbroken, none open.
    for name in [step(1), step(2), step(3), task] {
        let v = chain_of(&name)?;
        assert!(v.get("unbroken") == Some(&json!(true)) && v.get("open") != Some(&json!(true)) && half_named(&v) == 0, "{name}: {v}");
    }
    Ok(())
}

/// `git <args>` in `dir`, as a test's own git (no system config, no prompts): what it said.
fn git(home: &Path, dir: &Path, args: &[&str]) -> anyhow::Result<String> {
    let o = std::process::Command::new("git")
        .args(["-c", "user.name=Test", "-c", "user.email=test@example.com", "-c", "init.defaultBranch=main", "-c", "credential.helper=", "-c", "commit.gpgsign=false"])
        .args(args)
        .current_dir(dir)
        .env("HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GIT_ASKPASS")
        .env_remove("SSH_ASKPASS")
        .output()?;
    let out = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    anyhow::ensure!(o.status.success(), "git {args:?}: {out}");
    Ok(out)
}

/// User `name`, a developer of `repo`, with a personal access token of their own: the token.
fn developer(port: u16, admin: &str, name: &str, repo: &str) -> anyhow::Result<String> {
    let (st, body) = http(port, "POST", "/api/users", admin, Some(&json!({ "name": name, "grants": [format!("developer@repo:{repo}")] })))?;
    anyhow::ensure!((200..300).contains(&st), "{name}: {st} {body}");
    let (st, body) = http(port, "POST", "/api/tokens", admin, Some(&json!({ "kind": "pat", "user": name, "grants": ["owner"] })))?;
    anyhow::ensure!((200..300).contains(&st), "{name}'s token: {st} {body}");
    serde_json::from_str::<Value>(&body)?.get("token").and_then(Value::as_str).map(str::to_owned).ok_or_else(|| anyhow::anyhow!("no token in {body}"))
}

/// A native repository `repo` with a first commit on main, and a clone of it: (git's home, the
/// clone).
fn native(dir: &Path, port: u16, admin: &str, repo: &str) -> anyhow::Result<(std::path::PathBuf, std::path::PathBuf)> {
    let (st, body) = http(port, "POST", "/api/repos", admin, Some(&json!({ "name": repo, "mode": "native" })))?;
    anyhow::ensure!(st == 200, "{repo}: {st} {body}");
    let (home, work) = (dir.join("githome"), dir.join("clone"));
    std::fs::create_dir_all(&home)?;
    git(&home, dir, &["clone", "-q", &format!("http://x:{admin}@127.0.0.1:{port}/git/{repo}.git"), "clone"])?;
    std::fs::write(work.join("README.md"), "notes\n")?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "first"])?;
    git(&home, &work, &["push", "-q", "origin", "HEAD:main"])?;
    Ok((home, work))
}

/// The custody chain of the thing named `name`, walked now.
fn chain_named(port: u16, admin: &str, name: &str) -> anyhow::Result<Value> {
    let (_, body) = http(port, "GET", &format!("/api/custody?name={}", name.replace(' ', "%20")), admin, None)?;
    Ok(serde_json::from_str(&body).unwrap_or_default())
}

/// W27-R08 (the Transfers plan's step 7): a change changes hands signed. kim gives D1 to sam, who
/// takes it; lee commandeers it: two transfers, each with both sides (kiln's take for the side not
/// there, its receipt for a person with no key on the device that asked). sam, its author no more,
/// pushing a version of it as its author, or abandoning it, is refused and recorded with the
/// transfer that superseded them.
#[test]
fn w27_r08_change_hands() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options::default())?;
    let (port, admin, repo) = (s.port, s.admin.as_str(), "acme/notes");
    let (home, work) = native(dir.path(), port, admin, repo)?;
    let (kim, sam, lee) = (developer(port, admin, "kim", repo)?, developer(port, admin, "sam", repo)?, developer(port, admin, "lee", repo)?);
    let origin = |token: &str| format!("http://x:{token}@127.0.0.1:{port}/git/{repo}.git");
    git(&home, &work, &["checkout", "-qb", "change"])?;
    std::fs::write(work.join("notes.txt"), "a note\n")?;
    git(&home, &work, &["add", "notes.txt"])?;
    git(&home, &work, &["commit", "-qm", "A note\n\nChange-Id: I5555555555555555555555555555555555555555"])?;
    git(&home, &work, &["push", "-q", &origin(&kim), "HEAD:refs/for/main"])?;
    let author = || -> anyhow::Result<String> {
        let (_, body) = http(port, "GET", "/api/diffs/D1", admin, None)?;
        Ok(field(&serde_json::from_str::<Value>(&body)?, "author").to_owned())
    };
    anyhow::ensure!(author()? == "user:kim", "{}", author()?);
    // Given by kim, taken by sam.
    let (st, body) = http(port, "POST", "/api/diffs/D1/give", &kim, Some(&json!({ "to": "user:sam" })))?;
    anyhow::ensure!(st == 200, "kim gives D1: {st} {body}");
    anyhow::ensure!(author()? == "user:kim", "given, it's kim's until sam takes it");
    let (st, body) = http(port, "POST", "/api/diffs/D1/take", &sam, None)?;
    anyhow::ensure!(st == 200, "sam takes D1: {st} {body}");
    anyhow::ensure!(author()? == "user:sam", "{}", author()?);
    // Commandeered by lee.
    let (st, body) = http(port, "POST", "/api/diffs/D1/commandeer", &lee, None)?;
    anyhow::ensure!(st == 200, "lee commandeers D1: {st} {body}");
    anyhow::ensure!(author()? == "user:lee", "{}", author()?);
    let v = chain_named(port, admin, &format!("change {repo} D1"))?;
    let kinds: Vec<&str> = v.get("hops").and_then(Value::as_array).into_iter().flatten().filter_map(|h| h.get("kind").and_then(Value::as_str)).collect();
    assert!(v.get("unbroken") == Some(&json!(true)) && kinds == ["received", "taken", "received", "taken"] && half_named(&v) == 0, "two transfers, both sides each: {v}");
    let take_from_sam = v.pointer("/hops/1/record").and_then(Value::as_str).unwrap_or_default().to_owned();
    // sam, its author no more: a version pushed as its author, and abandoning it, refused.
    std::fs::write(work.join("notes.txt"), "a note, by its author before\n")?;
    git(&home, &work, &["commit", "-q", "--amend", "-am", "A note\n\nChange-Id: I5555555555555555555555555555555555555555"])?;
    let pushed = git(&home, &work, &["push", "-q", &origin(&sam), "HEAD:refs/for/main"]);
    assert!(pushed.as_ref().is_err_and(|e| e.to_string().contains("changed hands")), "{pushed:?}");
    let (st, body) = http(port, "POST", "/api/diffs/D1/abandon", &sam, None)?;
    assert!(st == 403 && body.contains("changed hands"), "{st} {body}");
    let (_, body) = http(port, "GET", "/api/transfers/refused", admin, None)?;
    let refused: Vec<Value> = serde_json::from_str::<Value>(&body)?.get("refused").and_then(Value::as_array).cloned().unwrap_or_default();
    let of_sam: Vec<&Value> = refused.iter().filter(|r| field(r, "signer").ends_with("/person/sam") && field(r, "superseded_by") == take_from_sam).collect();
    assert!(of_sam.iter().any(|r| field(r, "act").starts_with("push of a version")) && of_sam.iter().any(|r| field(r, "act").starts_with("abandon")), "{refused:?}");
    anyhow::ensure!(author()? == "user:lee", "nothing sam did took");
    Ok(())
}

/// W27-R08 (the Transfers plan's step 7): a review request passed on changes hands signed. rev1
/// passes D1's to rev2, who takes it: the request moves, with both sides. rev2 passes it on to rev3,
/// who never takes it: it stays rev2's.
#[test]
fn w27_r08_review_passed_on() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options::default())?;
    let (port, admin, repo) = (s.port, s.admin.as_str(), "acme/notes");
    let (home, work) = native(dir.path(), port, admin, repo)?;
    let kim = developer(port, admin, "kim", repo)?;
    let (rev1, rev2, _rev3) = (developer(port, admin, "rev1", repo)?, developer(port, admin, "rev2", repo)?, developer(port, admin, "rev3", repo)?);
    git(&home, &work, &["checkout", "-qb", "change"])?;
    std::fs::write(work.join("notes.txt"), "a note\n")?;
    git(&home, &work, &["add", "notes.txt"])?;
    git(&home, &work, &["commit", "-qm", "A note\n\nChange-Id: I6666666666666666666666666666666666666666"])?;
    git(&home, &work, &["push", "-q", "-o", "reviewer=user:rev1", &format!("http://x:{kim}@127.0.0.1:{port}/git/{repo}.git"), "HEAD:refs/for/main"])?;
    let reviewers = || -> anyhow::Result<Vec<String>> {
        let (_, body) = http(port, "GET", "/api/diffs/D1", admin, None)?;
        let v: Value = serde_json::from_str(&body)?;
        let mut r: Vec<String> = v.get("reviewers").and_then(Value::as_array).into_iter().flatten().filter_map(|r| r.get("reviewer").or_else(|| r.get("name")).and_then(Value::as_str).map(str::to_owned)).collect();
        r.sort();
        Ok(r)
    };
    anyhow::ensure!(reviewers()? == ["user:rev1"], "{:?}", reviewers()?);
    // Passed on and taken: the request moves.
    let (st, body) = http(port, "POST", "/api/diffs/D1/reviewers/pass", &rev1, Some(&json!({ "to": "user:rev2" })))?;
    anyhow::ensure!(st == 200, "rev1 passes D1's review: {st} {body}");
    anyhow::ensure!(reviewers()? == ["user:rev1"], "passed, it's rev1's until rev2 takes it");
    let (st, body) = http(port, "POST", "/api/diffs/D1/reviewers/take", &rev2, None)?;
    anyhow::ensure!(st == 200, "rev2 takes it: {st} {body}");
    anyhow::ensure!(reviewers()? == ["user:rev2"], "{:?}", reviewers()?);
    let name = format!("review request of {repo} D1");
    let v = chain_named(port, admin, &name)?;
    let kinds: Vec<&str> = v.get("hops").and_then(Value::as_array).into_iter().flatten().filter_map(|h| h.get("kind").and_then(Value::as_str)).collect();
    assert!(v.get("unbroken") == Some(&json!(true)) && kinds == ["received", "taken"] && half_named(&v) == 0, "moved with both sides: {v}");
    // Passed on again, never taken: it stays rev2's.
    let (st, body) = http(port, "POST", "/api/diffs/D1/reviewers/pass", &rev2, Some(&json!({ "to": "user:rev3" })))?;
    anyhow::ensure!(st == 200, "rev2 passes it on: {st} {body}");
    anyhow::ensure!(reviewers()? == ["user:rev2"], "never taken, it stays rev2's: {:?}", reviewers()?);
    let v = chain_named(port, admin, &name)?;
    assert!(v.get("open") == Some(&json!(true)), "the pass to rev3 waits, answered by no one: {v}");
    Ok(())
}
