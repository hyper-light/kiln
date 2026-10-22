//! A GitLab as a git host (docs/deploy-api.md §7, P3; docs/ci.md W23), against a fake GitLab: its
//! REST API (v4) and its git over smart HTTP (`git http-backend`), which lets into a private
//! project only kiln's token. KilnCI runs a workflow of a project there and reports it as a commit
//! status; an environment's release repository is made there through the token, releases are
//! committed to it by push with a deployment for each, a cancelled release's commit is reverted
//! there; with `via: pull-request` a release is a merge request (merged: delivered, its merger the
//! approval; closed: cancelled; cancelled in kiln: closed there with a note); GitLab's webhooks
//! wake it all, and a commit kiln didn't make is found as a release of origin `git`.

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
const TOKEN: &str = "glpat-kiln";
const SECRET: &str = "gl-s3cret";

/// What the fake GitLab holds and was asked.
#[derive(Default)]
struct Gitlab {
    /// (method, path with its query, body).
    calls: Vec<(String, String, Value)>,
    made: bool,
    /// GitLab's JSON for each merge request.
    mrs: Vec<Value>,
    /// Each deployment: `{id, sha, environment, ref, status}`.
    deployments: Vec<Value>,
    /// Each commit status: (project, sha, its body).
    statuses: Vec<(String, String, Value)>,
}

type Shared = Arc<Mutex<Gitlab>>;

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

/// The fake GitLab: `/api/v4/…` is its API (kiln's token as a bearer), the rest its git; `root`
/// holds its repositories. Its port.
fn fake_gitlab(root: PathBuf, shared: Shared) -> anyhow::Result<u16> {
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
    if let Some(api) = target.strip_prefix("/api/v4") {
        let ok = header("authorization").as_deref() == Some(&format!("Bearer {TOKEN}"));
        let (status, reply) = if ok { api_answer(&method, api, serde_json::from_slice(&body).unwrap_or(Value::Null), root, shared, port) } else { (401, json!({ "message": "401 Unauthorized" })) };
        let text = if reply.is_null() { String::new() } else { reply.to_string() };
        write!(w, "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}", text.len())?;
        return Ok(());
    }
    // git: kiln's token, as git sends it (`oauth2` and the token, in Basic); `acme/app` anyone.
    let (path, query) = target.split_once('?').unwrap_or((target.as_str(), ""));
    let token = header("authorization")
        .and_then(|a| a.strip_prefix("Basic ").and_then(|b| B64.decode(b).ok()))
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .filter(|up| up == &format!("oauth2:{TOKEN}"));
    if !path.starts_with("/acme/app.git") && token.is_none() {
        write!(w, "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"GitLab\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")?;
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
        cgi.env("REMOTE_USER", "oauth2");
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

/// The fake GitLab's API: the parts kiln's GitLab host uses.
fn api_answer(method: &str, full: &str, body: Value, root: &Path, shared: &Shared, port: u16) -> (u16, Value) {
    let path = full.split('?').next().unwrap_or_default().to_owned();
    let q = query(full);
    let arg = |k: &str| q.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone()).unwrap_or_default();
    let Ok(mut g) = shared.lock() else { return (500, Value::Null) };
    g.calls.push((method.to_owned(), full.to_owned(), body.clone()));
    let web = format!("http://127.0.0.1:{port}");
    let seg: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    match (method, seg.as_slice()) {
        ("GET", ["namespaces", "acme"]) => (200, json!({ "id": 5, "full_path": "acme", "kind": "group" })),
        ("GET", ["projects", "acme%2Freleases"]) => {
            if g.made {
                (200, json!({ "id": 42, "path_with_namespace": "acme/releases", "default_branch": "main", "web_url": format!("{web}/acme/releases") }))
            } else {
                (404, json!({ "message": "404 Project Not Found" }))
            }
        }
        ("POST", ["projects"]) if body.get("namespace_id") == Some(&json!(5)) && body.get("path") == Some(&json!("releases")) => {
            let made = git(root, &["init", "-q", "--bare", "acme/releases.git"]).is_ok();
            g.made = made;
            if made { (201, json!({ "id": 42, "path_with_namespace": "acme/releases" })) } else { (400, json!({ "message": "has already been taken" })) }
        }
        ("POST", ["projects", project, "statuses", sha]) => {
            g.statuses.push((decode(project), (*sha).to_owned(), body.clone()));
            (201, json!({ "id": g.statuses.len() }))
        }
        ("GET", ["projects", "acme%2Freleases", "merge_requests"]) => {
            let branch = arg("source_branch");
            let found: Vec<Value> = g.mrs.iter().filter(|m| m.get("source_branch") == Some(&json!(branch))).cloned().collect();
            (200, json!(found))
        }
        ("POST", ["projects", "acme%2Freleases", "merge_requests"]) => {
            let iid = g.mrs.len() + 1;
            let mr = json!({
                "iid": iid, "state": "opened", "merge_commit_sha": null, "squash_commit_sha": null,
                "title": body.get("title"), "description": body.get("description"),
                "source_branch": body.get("source_branch"), "target_branch": body.get("target_branch"),
                "web_url": format!("{web}/acme/releases/-/merge_requests/{iid}"),
            });
            g.mrs.push(mr.clone());
            (201, mr)
        }
        ("GET", ["projects", "acme%2Freleases", "merge_requests", n]) => match n.parse::<usize>().ok().and_then(|n| g.mrs.get(n.wrapping_sub(1))) {
            Some(m) => (200, m.clone()),
            None => (404, json!({ "message": "404 Not found" })),
        },
        ("PUT", ["projects", "acme%2Freleases", "merge_requests", n]) => match n.parse::<usize>().ok().and_then(|n| g.mrs.get_mut(n.wrapping_sub(1))) {
            Some(m) => {
                if body.get("state_event") == Some(&json!("close"))
                    && let Some(o) = m.as_object_mut()
                {
                    o.insert("state".into(), json!("closed"));
                }
                (200, m.clone())
            }
            None => (404, json!({ "message": "404 Not found" })),
        },
        ("POST", ["projects", "acme%2Freleases", "merge_requests", _, "notes"]) => (201, json!({ "id": 1 })),
        ("DELETE", ["projects", "acme%2Freleases", "repository", "branches", branch]) => {
            let gone = git(&root.join("acme/releases.git"), &["update-ref", "-d", &format!("refs/heads/{}", decode(branch))]).is_ok();
            if gone { (204, Value::Null) } else { (404, json!({ "message": "404 Branch Not Found" })) }
        }
        ("GET", ["projects", "acme%2Freleases", "deployments"]) => {
            let env = arg("environment");
            let found: Vec<Value> = g.deployments.iter().rev().filter(|d| d.get("environment") == Some(&json!(env))).cloned().collect();
            (200, json!(found))
        }
        ("POST", ["projects", "acme%2Freleases", "deployments"]) => {
            let id = 100 + g.deployments.len();
            let d = json!({ "id": id, "sha": body.get("sha"), "environment": body.get("environment"), "ref": body.get("ref"), "status": body.get("status") });
            g.deployments.push(d.clone());
            (201, d)
        }
        ("PUT", ["projects", "acme%2Freleases", "deployments", id]) => {
            let id: u64 = id.parse().unwrap_or(0);
            match g.deployments.iter_mut().find(|d| d.get("id") == Some(&json!(id))) {
                Some(d) => {
                    if let (Some(o), Some(s)) = (d.as_object_mut(), body.get("status")) {
                        o.insert("status".into(), s.clone());
                    }
                    (200, d.clone())
                }
                None => (404, json!({ "message": "404 Not found" })),
            }
        }
        _ => (404, json!({ "message": "404 Not Found" })),
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

/// GitLab's webhook delivery to kiln, with its token: kiln's status line.
fn deliver(port: u16, event: &str, body: &Value) -> anyhow::Result<String> {
    let body = body.to_string();
    let mut conn = std::net::TcpStream::connect(("127.0.0.1", port))?;
    write!(conn, "POST /api/hooks/gitlab HTTP/1.1\r\nHost: x\r\nX-Gitlab-Event: {event}\r\nX-Gitlab-Token: {SECRET}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())?;
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
fn p3_gitlab_release_repositories_and_ci() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let gl_root = dir.path().join("gl");
    std::fs::create_dir_all(gl_root.join("acme"))?;
    // An app's repository (anyone reads it): plain manifests under deploy/, tagged v1.
    git(&gl_root, &["init", "-q", "--bare", "acme/app.git"])?;
    let work = dir.path().join("work");
    std::fs::create_dir_all(work.join("deploy"))?;
    std::fs::write(work.join("deploy/config.yaml"), "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: web\ndata:\n  hello: world\n")?;
    git(&work, &["init", "-q"])?;
    git(&work, &["add", "."])?;
    git(&work, &["commit", "-qm", "first"])?;
    git(&work, &["tag", "v1"])?;
    git(&work, &["push", "-q", &gl_root.join("acme/app.git").to_string_lossy(), "HEAD:main", "v1"])?;
    // A private project with a KilnCI workflow.
    git(&gl_root, &["init", "-q", "--bare", "acme/ci.git"])?;
    let ci_work = dir.path().join("ci-work");
    std::fs::create_dir_all(ci_work.join(".kiln"))?;
    std::fs::write(ci_work.join(".kiln/ci.yaml"), "name: build\non: { push: {} }\njobs:\n  build:\n    runs-on: [self-hosted, local]\n    steps:\n      - run: echo built on kiln\n")?;
    git(&ci_work, &["init", "-q"])?;
    git(&ci_work, &["add", "."])?;
    git(&ci_work, &["commit", "-qm", "first"])?;
    git(&ci_work, &["push", "-q", &gl_root.join("acme/ci.git").to_string_lossy(), "HEAD:main"])?;
    let shared: Shared = Arc::default();
    let port = fake_gitlab(gl_root.clone(), Arc::clone(&shared))?;
    std::fs::write(dir.path().join("gl-token"), format!("{TOKEN}\n"))?;
    std::fs::write(dir.path().join("gl-secret"), format!("{SECRET}\n"))?;
    let extra = format!(
        "[[ci.gitlab]]\nurl = \"http://127.0.0.1:{port}\"\ntoken_file = \"{d}/gl-token\"\nwebhook_secret_file = \"{d}/gl-secret\"\ngroups = [\"acme\"]\n\n[[repos]]\nname = \"acme/ci\"\nclone_url = \"http://127.0.0.1:{port}/acme/ci.git\"\n",
        d = dir.path().display()
    );
    let s = start_cd(dir.path(), &Options { agent: true, ci: "kiln_workflows = true", extra: &extra })?;
    let saw = |m: &str, p: &str| shared.lock().map(|g| g.calls.iter().filter(|(cm, cp, _)| cm == m && cp.starts_with(p)).count()).unwrap_or(0);

    // KilnCI from a GitLab project: read with kiln's token, its run a commit status there.
    let mut repo_id = String::new();
    until(60, "the project is polled", || {
        let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        let list: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
        let r = list.as_array().and_then(|l| l.iter().find(|r| r.get("name") == Some(&json!("acme/ci")))).cloned().unwrap_or_default();
        repo_id = r.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
        Ok(r.get("last_polled_at").is_none_or(Value::is_null).then(|| format!("{body}\n{}", s.log())))
    })?;
    std::fs::write(ci_work.join("README.md"), "second\n")?;
    git(&ci_work, &["add", "README.md"])?;
    git(&ci_work, &["commit", "-qm", "second"])?;
    git(&ci_work, &["push", "-q", &gl_root.join("acme/ci.git").to_string_lossy(), "HEAD:main"])?;
    let sha = git(&ci_work, &["rev-parse", "HEAD"])?;
    let push = |project: &str, before: &str, after: &str| json!({ "object_kind": "push", "ref": "refs/heads/main", "before": before, "after": after, "project": { "path_with_namespace": project, "web_url": format!("http://127.0.0.1:{port}/{project}") } });
    assert!(deliver(s.port, "Push Hook", &push("acme/ci", "0", &sha))?.contains("202"));
    let statuses = |sha: &str| -> Vec<Value> { shared.lock().map(|g| g.statuses.iter().filter(|(p, s, _)| p == "acme/ci" && s == sha).map(|(_, _, b)| b.clone()).collect()).unwrap_or_default() };
    until(240, "the run's commit status, success", || {
        let got = statuses(&sha);
        let done = got.iter().any(|b| b.get("state") == Some(&json!("success")));
        anyhow::ensure!(!got.iter().any(|b| b.get("state") == Some(&json!("failed"))), "the run failed: {got:?}\n{}", s.log());
        Ok((!done).then(|| format!("{got:?}\nruns: {}\n{}", http(s.port, "GET", "/api/runs", &s.admin, None).map(|r| r.1).unwrap_or_default(), s.log())))
    })?;
    let got = statuses(&sha);
    assert!(got.iter().all(|b| b.get("name") == Some(&json!("kiln/build"))), "{got:?}");
    assert!(got.first().and_then(|b| b.get("target_url")).and_then(Value::as_str).is_some_and(|u| u.contains("/runs/")), "{got:?}");
    let _ = repo_id;

    // Two environments on one release repository on the GitLab, made through kiln's token once:
    // staging commits its releases, production proposes them as merge requests.
    let env = |name: &str, gates: Value| json!({ "name": name, "provider": "kilncd", "release_repo": { "host": "gitlab", "repo": "acme/releases", "branch": "main" }, "gates": gates });
    let (st, body) = http(s.port, "POST", "/api/deploy/environments", &s.admin, Some(&env("staging", json!({}))))?;
    anyhow::ensure!((200..300).contains(&st), "staging: {st} {body}\n{}", s.log());
    let (st, body) = http(s.port, "POST", "/api/deploy/environments", &s.admin, Some(&env("production", json!({ "via": "pull-request" }))))?;
    anyhow::ensure!((200..300).contains(&st), "production: {st} {body}");
    let made = shared.lock().map(|g| g.calls.iter().filter(|(m, p, _)| m == "POST" && p == "/projects").count()).unwrap_or(0);
    assert_eq!(made, 1, "made once");
    // A group no token of kiln's covers is refused.
    let (st, _) = http(s.port, "POST", "/api/deploy/environments", &s.admin, Some(&json!({ "name": "elsewhere", "release_repo": { "host": "gitlab", "repo": "other/releases" } })))?;
    assert_eq!(st, 400);

    let app = json!({ "name": "web", "project": "shop", "package": { "kind": "git", "repo": format!("http://127.0.0.1:{port}/acme/app.git"), "path": "deploy" } });
    let (st, body) = http(s.port, "POST", "/api/deploy/apps", &s.admin, Some(&app))?;
    anyhow::ensure!((200..300).contains(&st), "app: {st} {body}");
    for e in ["staging", "production"] {
        let (st, body) = http(s.port, "POST", &format!("/api/deploy/apps/web/environments/{e}"), &s.admin, Some(&json!({})))?;
        anyhow::ensure!(st == 201, "register in {e}: {st} {body}\n{}", s.log());
    }
    let releases = gl_root.join("acme/releases.git");
    let log = git(&releases, &["log", "--format=%B", "main"])?;
    assert!(log.contains("Kiln-Registration: web/staging") && log.contains("Kiln-Registration: web/production"), "{log}");

    // A release to staging: pushed, delivering, a deployment on GitLab.
    let id = ask(s.port, &s.admin, "staging")?;
    let mut commit = String::new();
    until(60, "staging's release delivering", || {
        let r = release(s.port, &s.admin, id)?;
        commit = r.get("commit").and_then(Value::as_str).unwrap_or_default().to_owned();
        Ok((r.get("state") != Some(&json!("delivering"))).then(|| format!("{r}\n{}", s.log())))
    })?;
    assert_eq!(git(&releases, &["rev-parse", "main"])?, commit);
    let deployment = |sha: &str, env: &str| -> Option<Value> { shared.lock().ok().and_then(|g| g.deployments.iter().rev().find(|d| d.get("sha") == Some(&json!(sha)) && d.get("environment") == Some(&json!(env))).cloned()) };
    until(30, "staging's deployment running", || Ok((deployment(&commit, "staging/web").and_then(|d| d.get("status").cloned()) != Some(json!("running"))).then(|| format!("{:?}", deployment(&commit, "staging/web")))))?;
    assert_eq!(deployment(&commit, "staging/web").and_then(|d| d.get("ref").cloned()), Some(json!("main")));
    // Cancelled while delivering: its commit reverted on GitLab, its deployment canceled.
    let (st, body) = http(s.port, "POST", &format!("/api/deploy/releases/{id}/cancel"), &s.admin, Some(&json!({})))?;
    anyhow::ensure!(st == 200, "cancel: {st} {body}");
    until(60, "the revert pushed", || {
        let log = git(&releases, &["log", "--format=%B", "main"])?;
        Ok((!log.contains(&format!("Kiln-Reverts: {id}"))).then(|| format!("{log}\n{}", s.log())))
    })?;
    until(30, "staging's deployment canceled", || Ok((deployment(&commit, "staging/web").and_then(|d| d.get("status").cloned()) != Some(json!("canceled"))).then(|| format!("{:?}", deployment(&commit, "staging/web")))))?;

    // Production: a merge request from a branch of the release's own.
    let id2 = ask(s.port, &s.admin, "production")?;
    until(60, "production's merge request", || {
        let r = release(s.port, &s.admin, id2)?;
        Ok((!r.get("message").and_then(Value::as_str).is_some_and(|m| m.contains("merge request !1"))).then(|| format!("{r}\n{}", s.log())))
    })?;
    let r = release(s.port, &s.admin, id2)?;
    let branch = format!("kiln/release-{id2}-{}", r.get("created_at").and_then(Value::as_i64).unwrap_or(0));
    let mr = shared.lock().map(|g| g.mrs.first().cloned().unwrap_or_default()).unwrap_or_default();
    assert_eq!((mr.get("source_branch"), mr.get("target_branch")), (Some(&json!(branch)), Some(&json!("main"))), "{mr}");
    let head = git(&releases, &["rev-parse", &branch])?;
    assert!(git(&releases, &["log", "-1", "--format=%B", &head])?.contains(&format!("Kiln-Release: {id2}")));
    // GitLab merges it, and says so: the merge is delivered, its merger the approval.
    let base = git(&releases, &["rev-parse", "main"])?;
    let tree = git(&releases, &["rev-parse", &format!("{head}^{{tree}}")])?;
    let merge = git(&releases, &["commit-tree", &tree, "-p", &base, "-p", &head, "-m", &format!("Merge branch '{branch}' into 'main'")])?;
    git(&releases, &["update-ref", "refs/heads/main", &merge])?;
    if let Ok(mut g) = shared.lock()
        && let Some(o) = g.mrs.get_mut(0).and_then(Value::as_object_mut)
    {
        o.insert("state".into(), json!("merged"));
        o.insert("merge_commit_sha".into(), json!(merge));
        o.insert("merge_user".into(), json!({ "username": "ada" }));
    }
    let mr_hook = |iid: u64, b: &str, state: &str, action: &str| json!({ "object_kind": "merge_request", "project": { "path_with_namespace": "acme/releases", "web_url": format!("http://127.0.0.1:{port}/acme/releases") }, "object_attributes": { "iid": iid, "source_branch": b, "target_branch": "main", "state": state, "action": action } });
    assert!(deliver(s.port, "Merge Request Hook", &mr_hook(1, &branch, "merged", "merge"))?.contains("202"));
    until(20, "the merge delivering", || {
        let r = release(s.port, &s.admin, id2)?;
        Ok((r.get("commit") != Some(&json!(merge))).then(|| format!("{r}\n{}", s.log())))
    })?;
    let r = release(s.port, &s.admin, id2)?;
    assert!(r.get("approvals").and_then(Value::as_array).is_some_and(|a| a.iter().any(|x| x.get("by") == Some(&json!("ada on GitLab")))), "{r}");
    until(30, "production's deployment running", || Ok((deployment(&merge, "production/web").and_then(|d| d.get("status").cloned()) != Some(json!("running"))).then(|| format!("{:?}", deployment(&merge, "production/web")))))?;
    until(10, "the release's branch deleted", || Ok((saw("DELETE", "/projects/acme%2Freleases/repository/branches/") == 0).then(|| "not yet".to_owned())))?;
    let (st, body) = http(s.port, "POST", &format!("/api/deploy/releases/{id2}/cancel"), &s.admin, Some(&json!({})))?;
    anyhow::ensure!(st == 200, "cancel: {st} {body}");
    until(60, "the merge reverted", || {
        let log = git(&releases, &["log", "--format=%B", "main"])?;
        Ok((!log.contains(&format!("Kiln-Reverts: {id2}"))).then(|| format!("{log}\n{}", s.log())))
    })?;

    // A merge request closed on GitLab: its release cancelled.
    let id3 = ask(s.port, &s.admin, "production")?;
    until(60, "the second merge request", || Ok((shared.lock().map(|g| g.mrs.len()).unwrap_or(0) < 2).then(|| s.log())))?;
    let b3 = shared.lock().map(|g| g.mrs.get(1).and_then(|m| m.get("source_branch")).and_then(Value::as_str).unwrap_or_default().to_owned()).unwrap_or_default();
    if let Ok(mut g) = shared.lock()
        && let Some(o) = g.mrs.get_mut(1).and_then(Value::as_object_mut)
    {
        o.insert("state".into(), json!("closed"));
    }
    assert!(deliver(s.port, "Merge Request Hook", &mr_hook(2, &b3, "closed", "close"))?.contains("202"));
    until(20, "closed is cancelled", || {
        let r = release(s.port, &s.admin, id3)?;
        Ok((r.get("state") != Some(&json!("cancelled"))).then(|| format!("{r}")))
    })?;
    let r = release(s.port, &s.admin, id3)?;
    assert!(r.get("message").and_then(Value::as_str).is_some_and(|m| m.contains("merge request !2 was closed without merging")), "{r}");

    // Cancelled in kiln while its merge request waits: closed on GitLab, with a note.
    let id4 = ask(s.port, &s.admin, "production")?;
    until(60, "the third merge request recorded", || {
        let r = release(s.port, &s.admin, id4)?;
        Ok((!r.get("message").and_then(Value::as_str).is_some_and(|m| m.contains("merge request !3"))).then(|| format!("{r}")))
    })?;
    let (st, body) = http(s.port, "POST", &format!("/api/deploy/releases/{id4}/cancel"), &s.admin, Some(&json!({})))?;
    anyhow::ensure!(st == 200, "cancel a proposed release: {st} {body}");
    until(20, "the merge request closed", || {
        let closed = shared.lock().map(|g| g.mrs.get(2).and_then(|m| m.get("state")) == Some(&json!("closed"))).unwrap_or(false);
        Ok((!closed || saw("POST", "/projects/acme%2Freleases/merge_requests/3/notes") == 0).then(|| s.log()))
    })?;

    // A commit kiln didn't make, and GitLab's push webhook: a release of origin `git`.
    let before = git(&releases, &["rev-parse", "main"])?;
    assert!(deliver(s.port, "Push Hook", &push("acme/releases", &before, &before))?.contains("202"));
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
    assert!(deliver(s.port, "Push Hook", &push("acme/releases", &before, &c))?.contains("202"));
    until(30, "a release of origin git", || {
        let (_, body) = http(s.port, "GET", "/api/deploy/releases?app=web&environment=staging", &s.admin, None)?;
        let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
        let found = v.get("releases").and_then(Value::as_array).is_some_and(|l| l.iter().any(|r| r.get("origin") == Some(&json!("git")) && r.get("commit") == Some(&json!(c))));
        Ok((!found).then(|| format!("{body}\n{}", s.log())))
    })?;
    Ok(())
}
