//! Bitbucket as a git host (docs/deploy-api.md §7, P3; docs/ci.md W23), against a fake Bitbucket
//! Cloud: its REST API (2.0) and its git over smart HTTP (`git http-backend`), which lets into a
//! private repository only kiln's access token. KilnCI runs a workflow of a repository there and
//! reports it as a build status; an environment's release repository is made there with kiln's
//! token, releases are committed to it by push with a build status for each (`deploy/<env>/<app>`),
//! a cancelled release's commit is reverted there; with `via: pull-request` a release is a pull
//! request (merged: delivered, its merger the approval; declined: cancelled; cancelled in kiln:
//! declined there with a comment, its branch deleted); Bitbucket's signed webhooks wake it all, and
//! a commit kiln didn't make is found as a release of origin `git`.

#![cfg(unix)]

mod common;

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use common::{Options, http, start_cd};
use serde_json::{Value, json};

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;
const TOKEN: &str = "bb-kiln";
const SECRET: &str = "bb-s3cret";

/// What the fake Bitbucket holds and was asked.
#[derive(Default)]
struct Bitbucket {
    /// (method, path with its query, body).
    calls: Vec<(String, String, Value)>,
    made: bool,
    /// Bitbucket's JSON for each pull request of `acme/releases`.
    prs: Vec<Value>,
    /// Each build status: (repository, commit, its body), the newest of a key in its place.
    statuses: Vec<(String, String, Value)>,
    /// Repositories made besides the release repository (apps' sources), by slug.
    sources: Vec<String>,
}

type Shared = Arc<Mutex<Bitbucket>>;

fn decode(s: &str) -> String {
    let mut out = Vec::new();
    let b = s.as_bytes();
    let mut i = 0;
    while let Some(&c) = b.get(i) {
        match (c, b.get(i + 1..i + 3).and_then(|h| std::str::from_utf8(h).ok()).and_then(|h| u8::from_str_radix(h, 16).ok())) {
            (b'%', Some(v)) => {
                out.push(v);
                i += 3;
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn query(path: &str) -> Vec<(String, String)> {
    path.split_once('?').map(|(_, q)| q.split('&').filter_map(|kv| kv.split_once('=')).map(|(k, v)| (decode(k), decode(v))).collect()).unwrap_or_default()
}

fn git(dir: &Path, args: &[&str]) -> anyhow::Result<String> {
    let o = Command::new("git").args(["-c", "user.name=T", "-c", "user.email=t@example.com", "-c", "init.defaultBranch=main"]).args(args).current_dir(dir).env("GIT_CONFIG_NOSYSTEM", "1").env("HOME", dir).output()?;
    anyhow::ensure!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
    Ok(String::from_utf8_lossy(&o.stdout).trim().to_owned())
}

/// The fake Bitbucket: `/2.0/…` is its API (kiln's access token as a bearer), the rest its git;
/// `root` holds its repositories. Its port.
fn fake_bitbucket(root: PathBuf, shared: Shared) -> anyhow::Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let (root, shared) = (root.clone(), Arc::clone(&shared));
            std::thread::spawn(move || {
                let _ = serve(stream, &root, &shared, port);
            });
        }
    });
    Ok(port)
}

fn serve(stream: std::net::TcpStream, root: &Path, shared: &Shared, port: u16) -> anyhow::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut w = stream;
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let (method, target) = (parts.next().unwrap_or("GET").to_owned(), parts.next().unwrap_or("/").to_owned());
    let mut headers: Vec<(String, String)> = Vec::new();
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h)? == 0 || h.trim_end().is_empty() {
            break;
        }
        if let Some((k, v)) = h.trim_end().split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_owned()));
        }
    }
    let header = |k: &str| headers.iter().find(|(h, _)| h == k).map(|(_, v)| v.clone());
    if header("expect").is_some_and(|e| e.eq_ignore_ascii_case("100-continue")) {
        w.write_all(b"HTTP/1.1 100 Continue\r\n\r\n")?;
    }
    let mut body = Vec::new();
    if header("transfer-encoding").is_some_and(|t| t.contains("chunked")) {
        loop {
            let mut size = String::new();
            reader.read_line(&mut size)?;
            let n = usize::from_str_radix(size.trim().split(';').next().unwrap_or("0"), 16)?;
            let mut chunk = vec![0; n + 2];
            reader.read_exact(&mut chunk)?;
            if n == 0 {
                break;
            }
            body.extend(chunk.get(..n).unwrap_or_default());
        }
    } else if let Some(n) = header("content-length").and_then(|n| n.parse::<usize>().ok()) {
        body.resize(n, 0);
        reader.read_exact(&mut body)?;
    }
    if let Some(api) = target.strip_prefix("/2.0") {
        let ok = header("authorization").as_deref() == Some(&format!("Bearer {TOKEN}"));
        let (status, reply) = if ok { api_answer(&method, api, serde_json::from_slice(&body).unwrap_or(Value::Null), root, shared, port) } else { (401, json!({ "type": "error", "error": { "message": "Unauthorized" } })) };
        let text = if reply.is_null() { String::new() } else { reply.to_string() };
        write!(w, "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}", text.len())?;
        return Ok(());
    }
    // git: kiln's access token, as git sends it (`x-token-auth` and the token, in Basic); `acme/app`
    // anyone.
    let (path, query) = target.split_once('?').unwrap_or((target.as_str(), ""));
    let token = header("authorization")
        .and_then(|a| a.strip_prefix("Basic ").and_then(|b| B64.decode(b).ok()))
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .filter(|up| up == &format!("x-token-auth:{TOKEN}"));
    // A repository whose pushes this Bitbucket refuses: what kiln does with one it made and couldn't fill.
    if path.starts_with("/acme/broken-") && (path.ends_with("/git-receive-pack") || query.contains("service=git-receive-pack")) {
        write!(w, "HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")?;
        return Ok(());
    }
    if !path.starts_with("/acme/app.git") && token.is_none() {
        write!(w, "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"Bitbucket\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")?;
        return Ok(());
    }
    let mut cgi = Command::new("git");
    cgi.arg("http-backend")
        .env("GIT_PROJECT_ROOT", root)
        .env("GIT_HTTP_EXPORT_ALL", "1")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("REQUEST_METHOD", &method)
        .env("PATH_INFO", path)
        .env("QUERY_STRING", query)
        .env("CONTENT_LENGTH", body.len().to_string())
        .env("REMOTE_ADDR", "127.0.0.1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if token.is_some() {
        cgi.env("REMOTE_USER", "x-token-auth");
    }
    for (h, var) in [("content-type", "CONTENT_TYPE"), ("content-encoding", "HTTP_CONTENT_ENCODING"), ("git-protocol", "GIT_PROTOCOL")] {
        if let Some(v) = header(h) {
            cgi.env(var, v);
        }
    }
    let mut child = cgi.spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(&body)?;
    }
    let out = child.wait_with_output()?.stdout;
    let (head, rest) = match out.windows(4).position(|w| w == b"\r\n\r\n") {
        Some(i) => (out.get(..i).unwrap_or_default(), out.get(i + 4..).unwrap_or_default()),
        None => match out.windows(2).position(|w| w == b"\n\n") {
            Some(i) => (out.get(..i).unwrap_or_default(), out.get(i + 2..).unwrap_or_default()),
            None => (&[][..], out.as_slice()),
        },
    };
    let mut status = "200 OK".to_owned();
    let mut response = Vec::new();
    for h in String::from_utf8_lossy(head).lines().map(str::trim).filter(|h| !h.is_empty()) {
        match h.strip_prefix("Status:") {
            Some(s) => status = s.trim().to_owned(),
            None => response.extend_from_slice(format!("{h}\r\n").as_bytes()),
        }
    }
    write!(w, "HTTP/1.1 {status}\r\n")?;
    w.write_all(&response)?;
    write!(w, "Content-Length: {}\r\nConnection: close\r\n\r\n", rest.len())?;
    w.write_all(rest)?;
    Ok(())
}

/// The fake Bitbucket's API: the parts kiln's Bitbucket host uses.
fn api_answer(method: &str, full: &str, body: Value, root: &Path, shared: &Shared, port: u16) -> (u16, Value) {
    let path = full.split('?').next().unwrap_or_default().to_owned();
    let q = query(full);
    let Ok(mut b) = shared.lock() else { return (500, Value::Null) };
    b.calls.push((method.to_owned(), full.to_owned(), body.clone()));
    let web = format!("http://127.0.0.1:{port}");
    let seg: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    let error = |m: &str| json!({ "type": "error", "error": { "message": m } });
    match (method, seg.as_slice()) {
        ("GET", ["repositories", "acme", slug]) if *slug != "releases" => {
            if b.sources.iter().any(|s| s == slug) { (200, json!({ "full_name": format!("acme/{slug}"), "is_private": true })) } else { (404, error("Repository not found")) }
        }
        ("POST", ["repositories", "acme", slug]) if *slug != "releases" && body.get("is_private") == Some(&json!(true)) => {
            let made = !b.sources.iter().any(|s| s == slug) && git(root, &["init", "-q", "--bare", &format!("acme/{slug}.git")]).is_ok();
            if made {
                b.sources.push((*slug).to_owned());
                (200, json!({ "full_name": format!("acme/{slug}"), "is_private": true }))
            } else {
                (400, error("Repository with this Slug and Owner already exists."))
            }
        }
        ("DELETE", ["repositories", "acme", slug]) if b.sources.iter().any(|s| s == slug) => {
            let _ = std::fs::remove_dir_all(root.join(format!("acme/{slug}.git")));
            b.sources.retain(|s| s != slug);
            (204, Value::Null)
        }
        ("GET", ["repositories", "acme", "releases"]) => {
            if b.made {
                (200, json!({ "full_name": "acme/releases", "is_private": true, "mainbranch": { "name": "main" }, "links": { "html": { "href": format!("{web}/acme/releases") } } }))
            } else {
                (404, error("Repository acme/releases not found"))
            }
        }
        ("POST", ["repositories", "acme", "releases"]) if body.get("is_private") == Some(&json!(true)) => {
            let made = !b.made && git(root, &["init", "-q", "--bare", "acme/releases.git"]).is_ok();
            b.made |= made;
            if made { (200, json!({ "full_name": "acme/releases", "is_private": true })) } else { (400, error("Repository with this Slug and Owner already exists.")) }
        }
        ("POST", ["repositories", ws, slug, "commit", sha, "statuses", "build"]) => {
            let (repo, sha) = (format!("{ws}/{slug}"), (*sha).to_owned());
            let key = body.get("key").cloned();
            b.statuses.retain(|(r, s, old)| !(r == &repo && s == &sha && old.get("key") == key.as_ref()));
            b.statuses.push((repo, sha, body.clone()));
            (201, body)
        }
        ("GET", ["repositories", "acme", "releases", "pullrequests"]) => {
            // `q`: `source.branch.name="<branch>"`.
            let want = q.iter().find(|(k, _)| k == "q").and_then(|(_, v)| v.split('"').nth(1).map(str::to_owned)).unwrap_or_default();
            let found: Vec<Value> = b.prs.iter().filter(|p| p.pointer("/source/branch/name") == Some(&json!(want))).cloned().collect();
            (200, json!({ "values": found, "pagelen": 50, "page": 1 }))
        }
        ("POST", ["repositories", "acme", "releases", "pullrequests"]) => {
            let id = b.prs.len() + 1;
            let pr = json!({
                "id": id, "state": "OPEN", "title": body.get("title"), "description": body.get("description"),
                "source": body.get("source"), "destination": body.get("destination"),
                "merge_commit": null, "closed_by": null,
                "links": { "html": { "href": format!("{web}/acme/releases/pull-requests/{id}") } },
            });
            b.prs.push(pr.clone());
            (201, pr)
        }
        ("GET", ["repositories", "acme", "releases", "pullrequests", n]) => match n.parse::<usize>().ok().and_then(|n| b.prs.get(n.wrapping_sub(1))) {
            Some(p) => (200, p.clone()),
            None => (404, error("Pull request not found")),
        },
        ("POST", ["repositories", "acme", "releases", "pullrequests", n, "comments"]) => match n.parse::<usize>().ok().and_then(|n| b.prs.get_mut(n.wrapping_sub(1))).and_then(Value::as_object_mut) {
            Some(p) => {
                p.insert("comment".into(), body.pointer("/content/raw").cloned().unwrap_or(Value::Null));
                (201, json!({ "id": 1 }))
            }
            None => (404, error("Pull request not found")),
        },
        ("POST", ["repositories", "acme", "releases", "pullrequests", n, "decline"]) => match n.parse::<usize>().ok().and_then(|n| b.prs.get_mut(n.wrapping_sub(1))) {
            Some(p) if p.get("state") == Some(&json!("OPEN")) => {
                if let Some(o) = p.as_object_mut() {
                    o.insert("state".into(), json!("DECLINED"));
                }
                (200, p.clone())
            }
            Some(_) => (400, error("Only open pull requests can be declined")),
            None => (404, error("Pull request not found")),
        },
        ("DELETE", ["repositories", "acme", "releases", "refs", "branches", branch @ ..]) => {
            let name = branch.iter().map(|s| decode(s)).collect::<Vec<_>>().join("/");
            let gone = git(&root.join("acme/releases.git"), &["update-ref", "-d", &format!("refs/heads/{name}")]).is_ok();
            if gone { (204, Value::Null) } else { (404, error("Branch not found")) }
        }
        _ => (404, error("Resource not found")),
    }
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

/// Bitbucket's webhook delivery to kiln, signed with the webhook's secret: kiln's status line.
fn deliver(port: u16, event: &str, body: &Value) -> anyhow::Result<String> {
    let body = body.to_string();
    let key = aws_lc_rs::hmac::Key::new(aws_lc_rs::hmac::HMAC_SHA256, SECRET.as_bytes());
    let sig: String = aws_lc_rs::hmac::sign(&key, body.as_bytes()).as_ref().iter().map(|b| format!("{b:02x}")).collect();
    let mut conn = std::net::TcpStream::connect(("127.0.0.1", port))?;
    write!(conn, "POST /api/hooks/bitbucket HTTP/1.1\r\nHost: x\r\nX-Event-Key: {event}\r\nX-Hub-Signature: sha256={sig}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())?;
    let mut answer = String::new();
    conn.read_to_string(&mut answer)?;
    Ok(answer.lines().next().unwrap_or_default().to_owned())
}

fn release(port: u16, admin: &str, id: i64) -> anyhow::Result<Value> {
    let (_, body) = http(port, "GET", &format!("/api/deploy/releases/{id}"), admin, None)?;
    Ok(serde_json::from_str(&body).unwrap_or(Value::Null))
}

fn ask(port: u16, admin: &str, env: &str) -> anyhow::Result<i64> {
    let (st, body) = http(port, "POST", "/api/deploy/releases", admin, Some(&json!({ "app": "web", "environment": env, "version": "v1" })))?;
    anyhow::ensure!(st == 201, "a release to {env}: {st} {body}");
    let v: Value = serde_json::from_str(&body)?;
    v.get("id").and_then(Value::as_i64).ok_or_else(|| anyhow::anyhow!("no id: {body}"))
}

#[test]
fn p3_bitbucket_release_repositories_and_ci() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let bb_root = dir.path().join("bb");
    std::fs::create_dir_all(bb_root.join("acme"))?;
    // An app's repository (anyone reads it): plain manifests under deploy/, tagged v1.
    git(&bb_root, &["init", "-q", "--bare", "acme/app.git"])?;
    let work = dir.path().join("work");
    std::fs::create_dir_all(work.join("deploy"))?;
    std::fs::write(work.join("deploy/config.yaml"), "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: web\ndata:\n  hello: world\n")?;
    git(&work, &["init", "-q"])?;
    git(&work, &["add", "."])?;
    git(&work, &["commit", "-qm", "first"])?;
    git(&work, &["tag", "v1"])?;
    git(&work, &["push", "-q", &bb_root.join("acme/app.git").to_string_lossy(), "HEAD:main", "v1"])?;
    // A private repository with a KilnCI workflow.
    git(&bb_root, &["init", "-q", "--bare", "acme/ci.git"])?;
    let ci_work = dir.path().join("ci-work");
    std::fs::create_dir_all(ci_work.join(".kiln"))?;
    std::fs::write(ci_work.join(".kiln/ci.yaml"), "name: build\non: { push: {} }\njobs:\n  build:\n    runs-on: [self-hosted, local]\n    steps:\n      - run: echo built on kiln\n")?;
    git(&ci_work, &["init", "-q"])?;
    git(&ci_work, &["add", "."])?;
    git(&ci_work, &["commit", "-qm", "first"])?;
    git(&ci_work, &["push", "-q", &bb_root.join("acme/ci.git").to_string_lossy(), "HEAD:main"])?;
    let shared: Shared = Arc::default();
    let port = fake_bitbucket(bb_root.clone(), Arc::clone(&shared))?;
    std::fs::write(dir.path().join("bb-token"), format!("{TOKEN}\n"))?;
    std::fs::write(dir.path().join("bb-secret"), format!("{SECRET}\n"))?;
    let extra = format!(
        "[ci.bitbucket]\nweb_url = \"http://127.0.0.1:{port}\"\napi_url = \"http://127.0.0.1:{port}/2.0\"\ntoken_file = \"{d}/bb-token\"\nwebhook_secret_file = \"{d}/bb-secret\"\nworkspaces = [\"acme\"]\n\n[[repos]]\nname = \"acme/ci\"\nclone_url = \"http://127.0.0.1:{port}/acme/ci.git\"\n",
        d = dir.path().display()
    );
    let s = start_cd(dir.path(), &Options { agent: true, ci: "kiln_workflows = true", extra: &extra })?;
    let saw = |m: &str, p: &str| shared.lock().map(|b| b.calls.iter().filter(|(cm, cp, _)| cm == m && cp.starts_with(p)).count()).unwrap_or(0);

    // KilnCI from a Bitbucket repository: read with kiln's access token, its run a build status there.
    until(60, "the repository is polled", || {
        let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        let list: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
        let r = list.as_array().and_then(|l| l.iter().find(|r| r.get("name") == Some(&json!("acme/ci")))).cloned().unwrap_or_default();
        Ok(r.get("last_polled_at").is_none_or(Value::is_null).then(|| format!("{body}\n{}", s.log())))
    })?;
    std::fs::write(ci_work.join("README.md"), "second\n")?;
    git(&ci_work, &["add", "README.md"])?;
    git(&ci_work, &["commit", "-qm", "second"])?;
    let before_ci = git(&ci_work, &["rev-parse", "HEAD~1"])?;
    git(&ci_work, &["push", "-q", &bb_root.join("acme/ci.git").to_string_lossy(), "HEAD:main"])?;
    let sha = git(&ci_work, &["rev-parse", "HEAD"])?;
    let push = |repo: &str, before: &str, after: &str| {
        json!({ "repository": { "full_name": repo }, "push": { "changes": [{ "new": { "type": "branch", "name": "main", "target": { "hash": after } }, "old": { "type": "branch", "name": "main", "target": { "hash": before } } }] } })
    };
    assert!(deliver(s.port, "repo:push", &push("acme/ci", &before_ci, &sha))?.contains("202"));
    let statuses = |repo: &str, sha: &str| -> Vec<Value> { shared.lock().map(|b| b.statuses.iter().filter(|(r, s, _)| r == repo && s == sha).map(|(_, _, v)| v.clone()).collect()).unwrap_or_default() };
    until(240, "the run's build status, successful", || {
        let got = statuses("acme/ci", &sha);
        anyhow::ensure!(!got.iter().any(|b| b.get("state") == Some(&json!("FAILED"))), "the run failed: {got:?}\n{}", s.log());
        let done = got.iter().any(|b| b.get("state") == Some(&json!("SUCCESSFUL")));
        Ok((!done).then(|| format!("{got:?}\nruns: {}\n{}", http(s.port, "GET", "/api/runs", &s.admin, None).map(|r| r.1).unwrap_or_default(), s.log())))
    })?;
    let got = statuses("acme/ci", &sha);
    assert!(got.iter().all(|b| b.get("key") == Some(&json!("kiln/build"))), "{got:?}");
    assert!(got.first().and_then(|b| b.get("url")).and_then(Value::as_str).is_some_and(|u| u.contains("/runs/")), "{got:?}");

    // Bitbucket is a git host release repositories may be on, with its workspaces.
    let (_, body) = http(s.port, "GET", "/api/deploy/git-hosts", &s.admin, None)?;
    let hosts: Value = serde_json::from_str(&body)?;
    assert!(hosts.get("hosts").and_then(Value::as_array).is_some_and(|l| l.iter().any(|h| h.get("host") == Some(&json!("bitbucket")) && h.get("groups") == Some(&json!(["acme"])))), "{body}");

    // Two environments on one release repository on Bitbucket, made with kiln's token once:
    // staging commits its releases, production proposes them as pull requests.
    let env = |name: &str, gates: Value| json!({ "name": name, "provider": "kilncd", "release_repo": { "host": "bitbucket", "repo": "acme/releases", "branch": "main" }, "gates": gates });
    let (st, body) = http(s.port, "POST", "/api/deploy/environments", &s.admin, Some(&env("staging", json!({}))))?;
    anyhow::ensure!((200..300).contains(&st), "staging: {st} {body}\n{}", s.log());
    let (st, body) = http(s.port, "POST", "/api/deploy/environments", &s.admin, Some(&env("production", json!({ "via": "pull-request" }))))?;
    anyhow::ensure!((200..300).contains(&st), "production: {st} {body}");
    let made = shared.lock().map(|b| b.calls.iter().filter(|(m, p, _)| m == "POST" && p == "/repositories/acme/releases").count()).unwrap_or(0);
    assert_eq!(made, 1, "made once");
    // A workspace kiln's credentials don't cover, or a name that isn't `workspace/name`: refused.
    for repo in ["other/releases", "acme/platform/releases"] {
        let (st, body) = http(s.port, "POST", "/api/deploy/environments", &s.admin, Some(&json!({ "name": "elsewhere", "release_repo": { "host": "bitbucket", "repo": repo } })))?;
        assert_eq!(st, 400, "{repo}: {body}");
    }

    let app = json!({ "name": "web", "project": "shop", "package": { "kind": "git", "repo": format!("http://127.0.0.1:{port}/acme/app.git"), "path": "deploy" } });
    let (st, body) = http(s.port, "POST", "/api/deploy/apps", &s.admin, Some(&app))?;
    anyhow::ensure!((200..300).contains(&st), "app: {st} {body}");
    for e in ["staging", "production"] {
        let (st, body) = http(s.port, "POST", &format!("/api/deploy/apps/web/environments/{e}"), &s.admin, Some(&json!({})))?;
        anyhow::ensure!(st == 201, "register in {e}: {st} {body}\n{}", s.log());
    }
    let releases = bb_root.join("acme/releases.git");
    let log = git(&releases, &["log", "--format=%B", "main"])?;
    assert!(log.contains("Kiln-Registration: web/staging") && log.contains("Kiln-Registration: web/production"), "{log}");

    // A release to staging: pushed, delivering, a build status of its commit on Bitbucket.
    let id = ask(s.port, &s.admin, "staging")?;
    let mut commit = String::new();
    until(60, "staging's release delivering", || {
        let r = release(s.port, &s.admin, id)?;
        commit = r.get("commit").and_then(Value::as_str).unwrap_or_default().to_owned();
        Ok((r.get("state") != Some(&json!("delivering"))).then(|| format!("{r}\n{}", s.log())))
    })?;
    assert_eq!(git(&releases, &["rev-parse", "main"])?, commit);
    let deployed = |sha: &str, env: &str| -> Option<Value> { statuses("acme/releases", sha).into_iter().find(|b| b.get("key") == Some(&json!(format!("deploy/{env}")))) };
    until(30, "staging's build status in progress", || Ok((deployed(&commit, "staging/web").and_then(|d| d.get("state").cloned()) != Some(json!("INPROGRESS"))).then(|| format!("{:?}", deployed(&commit, "staging/web")))))?;
    let d = deployed(&commit, "staging/web").unwrap_or_default();
    assert!(d.get("url").and_then(Value::as_str).is_some_and(|u| u.ends_with(&format!("/deployments/releases/{id}"))), "{d}");
    assert!(d.get("description").and_then(Value::as_str).is_some_and(|t| t.starts_with(&format!("release {id}"))), "{d}");
    // Cancelled while delivering: its commit reverted on Bitbucket, its build status stopped.
    let (st, body) = http(s.port, "POST", &format!("/api/deploy/releases/{id}/cancel"), &s.admin, Some(&json!({})))?;
    anyhow::ensure!(st == 200, "cancel: {st} {body}");
    until(60, "the revert pushed", || {
        let log = git(&releases, &["log", "--format=%B", "main"])?;
        Ok((!log.contains(&format!("Kiln-Reverts: {id}"))).then(|| format!("{log}\n{}", s.log())))
    })?;
    until(30, "staging's build status stopped", || Ok((deployed(&commit, "staging/web").and_then(|d| d.get("state").cloned()) != Some(json!("STOPPED"))).then(|| format!("{:?}", deployed(&commit, "staging/web")))))?;

    // Production: a pull request from a branch of the release's own.
    let id2 = ask(s.port, &s.admin, "production")?;
    until(60, "production's pull request", || {
        let r = release(s.port, &s.admin, id2)?;
        Ok((!r.get("message").and_then(Value::as_str).is_some_and(|m| m.contains("pull request #1"))).then(|| format!("{r}\n{}", s.log())))
    })?;
    let r = release(s.port, &s.admin, id2)?;
    let branch = format!("kiln/release-{id2}-{}", r.get("created_at").and_then(Value::as_i64).unwrap_or(0));
    let pr = shared.lock().map(|b| b.prs.first().cloned().unwrap_or_default()).unwrap_or_default();
    assert_eq!((pr.pointer("/source/branch/name"), pr.pointer("/destination/branch/name")), (Some(&json!(branch)), Some(&json!("main"))), "{pr}");
    assert_eq!(r.pointer("/provider/pull_request/url").and_then(Value::as_str), Some(format!("http://127.0.0.1:{port}/acme/releases/pull-requests/1").as_str()), "{r}");
    let head = git(&releases, &["rev-parse", &branch])?;
    assert!(git(&releases, &["log", "-1", "--format=%B", &head])?.contains(&format!("Kiln-Release: {id2}")));
    // Bitbucket merges it, and says so: the merge is delivered, its merger the approval.
    let base = git(&releases, &["rev-parse", "main"])?;
    let tree = git(&releases, &["rev-parse", &format!("{head}^{{tree}}")])?;
    let merge = git(&releases, &["commit-tree", &tree, "-p", &base, "-p", &head, "-m", &format!("Merged in {branch} (pull request #1)")])?;
    git(&releases, &["update-ref", "refs/heads/main", &merge])?;
    if let Ok(mut b) = shared.lock()
        && let Some(o) = b.prs.get_mut(0).and_then(Value::as_object_mut)
    {
        o.insert("state".into(), json!("MERGED"));
        o.insert("merge_commit".into(), json!({ "hash": merge }));
        o.insert("closed_by".into(), json!({ "nickname": "ada" }));
    }
    let pr_hook = |id: u64, b: &str, state: &str| json!({ "repository": { "full_name": "acme/releases" }, "pullrequest": { "id": id, "state": state, "source": { "branch": { "name": b } }, "destination": { "branch": { "name": "main" } } } });
    assert!(deliver(s.port, "pullrequest:fulfilled", &pr_hook(1, &branch, "MERGED"))?.contains("202"));
    until(20, "the merge delivering", || {
        let r = release(s.port, &s.admin, id2)?;
        Ok((r.get("commit") != Some(&json!(merge))).then(|| format!("{r}\n{}", s.log())))
    })?;
    let r = release(s.port, &s.admin, id2)?;
    assert!(r.get("approvals").and_then(Value::as_array).is_some_and(|a| a.iter().any(|x| x.get("by") == Some(&json!("ada on Bitbucket")))), "{r}");
    until(30, "production's build status in progress", || Ok((deployed(&merge, "production/web").and_then(|d| d.get("state").cloned()) != Some(json!("INPROGRESS"))).then(|| format!("{:?}", deployed(&merge, "production/web")))))?;
    until(10, "the release's branch deleted", || Ok((saw("DELETE", "/repositories/acme/releases/refs/branches/kiln/release-") == 0).then(|| "not yet".to_owned())))?;
    assert!(git(&releases, &["rev-parse", "--verify", "-q", &branch]).is_err(), "the branch is gone");
    let (st, body) = http(s.port, "POST", &format!("/api/deploy/releases/{id2}/cancel"), &s.admin, Some(&json!({})))?;
    anyhow::ensure!(st == 200, "cancel: {st} {body}");
    until(60, "the merge reverted", || {
        let log = git(&releases, &["log", "--format=%B", "main"])?;
        Ok((!log.contains(&format!("Kiln-Reverts: {id2}"))).then(|| format!("{log}\n{}", s.log())))
    })?;

    // A pull request declined on Bitbucket: its release cancelled.
    let id3 = ask(s.port, &s.admin, "production")?;
    until(60, "the second pull request", || Ok((shared.lock().map(|b| b.prs.len()).unwrap_or(0) < 2).then(|| s.log())))?;
    let b3 = shared.lock().map(|b| b.prs.get(1).and_then(|p| p.pointer("/source/branch/name")).and_then(Value::as_str).unwrap_or_default().to_owned()).unwrap_or_default();
    if let Ok(mut b) = shared.lock()
        && let Some(o) = b.prs.get_mut(1).and_then(Value::as_object_mut)
    {
        o.insert("state".into(), json!("DECLINED"));
    }
    assert!(deliver(s.port, "pullrequest:rejected", &pr_hook(2, &b3, "DECLINED"))?.contains("202"));
    until(20, "declined is cancelled", || {
        let r = release(s.port, &s.admin, id3)?;
        Ok((r.get("state") != Some(&json!("cancelled"))).then(|| format!("{r}")))
    })?;
    let r = release(s.port, &s.admin, id3)?;
    assert!(r.get("message").and_then(Value::as_str).is_some_and(|m| m.contains("pull request #2 was closed without merging")), "{r}");

    // Cancelled in kiln while its pull request waits: declined on Bitbucket, with a comment.
    let id4 = ask(s.port, &s.admin, "production")?;
    until(60, "the third pull request recorded", || {
        let r = release(s.port, &s.admin, id4)?;
        Ok((!r.get("message").and_then(Value::as_str).is_some_and(|m| m.contains("pull request #3"))).then(|| format!("{r}")))
    })?;
    let (st, body) = http(s.port, "POST", &format!("/api/deploy/releases/{id4}/cancel"), &s.admin, Some(&json!({})))?;
    anyhow::ensure!(st == 200, "cancel a proposed release: {st} {body}");
    until(20, "the pull request declined", || {
        let pr = shared.lock().map(|b| b.prs.get(2).cloned().unwrap_or_default()).unwrap_or_default();
        let declined = pr.get("state") == Some(&json!("DECLINED")) && pr.get("comment").and_then(Value::as_str).is_some_and(|c| c.contains(&format!("kiln's release {id4}")));
        Ok((!declined).then(|| format!("{pr}\n{}", s.log())))
    })?;

    // A commit kiln didn't make, and Bitbucket's push webhook: a release of origin `git`.
    let before = git(&releases, &["rev-parse", "main"])?;
    assert!(deliver(s.port, "repo:push", &push("acme/releases", &before, &before))?.contains("202"));
    std::thread::sleep(Duration::from_secs(4));
    let index = dir.path().join("hand-index");
    let blob = {
        let mut c = Command::new("git").args(["hash-object", "-w", "--stdin"]).current_dir(&releases).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn()?;
        if let Some(mut i) = c.stdin.take() {
            i.write_all(b"replicaCount: 2\n")?;
        }
        String::from_utf8_lossy(&c.wait_with_output()?.stdout).trim().to_owned()
    };
    let hand = |args: &[&str]| -> anyhow::Result<String> {
        let o = Command::new("git").args(args).current_dir(&releases).env("GIT_INDEX_FILE", &index).env("GIT_AUTHOR_NAME", "ada").env("GIT_AUTHOR_EMAIL", "ada@example.com").env("GIT_COMMITTER_NAME", "ada").env("GIT_COMMITTER_EMAIL", "ada@example.com").output()?;
        anyhow::ensure!(o.status.success(), "{args:?}: {}", String::from_utf8_lossy(&o.stderr));
        Ok(String::from_utf8_lossy(&o.stdout).trim().to_owned())
    };
    hand(&["read-tree", "main"])?;
    hand(&["update-index", "--add", "--cacheinfo", &format!("100644,{blob},shop/web/staging/values.yaml")])?;
    let t = hand(&["write-tree"])?;
    let c = hand(&["commit-tree", &t, "-p", &before, "-m", "two replicas in staging"])?;
    hand(&["update-ref", "refs/heads/main", &c])?;
    assert!(deliver(s.port, "repo:push", &push("acme/releases", &before, &c))?.contains("202"));
    until(30, "a release of origin git", || {
        let (_, body) = http(s.port, "GET", "/api/deploy/releases?app=web&environment=staging", &s.admin, None)?;
        let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
        let found = v.get("releases").and_then(Value::as_array).is_some_and(|l| l.iter().any(|r| r.get("origin") == Some(&json!("git")) && r.get("commit") == Some(&json!(c))));
        Ok((!found).then(|| format!("{body}\n{}", s.log())))
    })?;

    // An app's source repository made on Bitbucket with kiln's access token, from a starter there.
    common::source_on_host(&s, "bitbucket", &bb_root, &format!("http://127.0.0.1:{port}/acme/ci.git"), (".kiln/ci.yaml", "built on kiln"), true)?;
    // One made whose first commit Bitbucket refuses: deleted again, and no app made.
    let t = json!({ "name": "broken-src", "package": { "kind": "helm", "repo": "oci://ghcr.io/acme/charts", "chart": "{app}" }, "repository": { "host": "bitbucket", "name": "acme/broken-{app}", "starter": format!("http://127.0.0.1:{port}/acme/app.git") } });
    let (st, body) = http(s.port, "POST", "/api/deploy/templates", &s.admin, Some(&t))?;
    anyhow::ensure!((200..300).contains(&st), "{st} {body}");
    let (st, body) = http(s.port, "POST", "/api/deploy/apps", &s.admin, Some(&json!({ "name": "shop", "template": "broken-src" })))?;
    assert!(st == 502 && body.contains("deleted again"), "{st} {body}");
    assert!(!bb_root.join("acme/broken-shop.git").exists() && saw("DELETE", "/repositories/acme/broken-shop") == 1, "nothing half made stays");
    assert_eq!(http(s.port, "GET", "/api/deploy/apps/shop", &s.admin, None)?.0, 404, "no app");
    Ok(())
}
