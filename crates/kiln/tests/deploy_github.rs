//! Release repositories on GitHub (docs/deploy-api.md §7, P3), against a fake GitHub API and a
//! smart HTTP git server (`git http-backend`) that stands for GitHub's git and lets in only the
//! App's installation tokens: an environment's repository made through kiln's GitHub App, releases
//! committed to it by push, a deployment status for each, a cancelled release's commit reverted
//! there; with `via: pull-request` a release as a pull request (merged: delivered, the merge its
//! approval; closed: cancelled; cancelled in kiln: closed there); GitHub's webhooks waking it all,
//! and a commit kiln didn't make found as a release of origin `git`.

#![cfg(unix)]

mod common;

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aws_lc_rs::signature::KeyPair as _;
use base64::Engine as _;
use common::{Options, http, start_cd};
use serde_json::{Value, json};

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;
const B64URL: base64::engine::GeneralPurpose = base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// What the fake GitHub holds and was asked.
#[derive(Default)]
struct Github {
    /// (method, path with its query, body).
    calls: Vec<(String, String, Value)>,
    made: bool,
    /// Each token's request: its permissions and repositories.
    tokens: Vec<Value>,
    /// GitHub's JSON for each pull request.
    pulls: Vec<Value>,
    /// Each deployment: `{id, sha, environment}`.
    deployments: Vec<Value>,
    /// Each status: (deployment id, its body).
    statuses: Vec<(u64, Value)>,
}

type Shared = Arc<Mutex<Github>>;

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
            (b'+', _) => {
                out.push(b' ');
                i += 1;
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

/// A fake of the parts of GitHub's API kiln's App uses for release repositories; `root` is where
/// its git keeps repositories.
fn fake_github(public_key: Vec<u8>, root: PathBuf) -> anyhow::Result<(u16, Shared)> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    let shared: Shared = Arc::default();
    let gh = Arc::clone(&shared);
    std::thread::spawn(move || {
        let mut next_token = 0u64;
        for stream in listener.incoming().flatten() {
            let mut reader = BufReader::new(match stream.try_clone() {
                Ok(s) => s,
                Err(_) => continue,
            });
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                continue;
            }
            let mut parts = line.split_whitespace();
            let (method, full) = (parts.next().unwrap_or_default().to_owned(), parts.next().unwrap_or_default().to_owned());
            let (mut len, mut auth) = (0usize, String::new());
            loop {
                let mut h = String::new();
                if reader.read_line(&mut h).is_err() || h.trim().is_empty() {
                    break;
                }
                let (k, v) = h.split_once(':').unwrap_or((&h, ""));
                match k.trim().to_ascii_lowercase().as_str() {
                    "content-length" => len = v.trim().parse().unwrap_or(0),
                    "authorization" => auth = v.trim().to_owned(),
                    _ => {}
                }
            }
            let mut body = vec![0u8; len];
            if reader.read_exact(&mut body).is_err() {
                continue;
            }
            let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let jwt_ok = auth.strip_prefix("Bearer ").is_some_and(|jwt| {
                let mut p = jwt.split('.');
                let (h, b, s) = (p.next().unwrap_or_default(), p.next().unwrap_or_default(), p.next().unwrap_or_default());
                let key = aws_lc_rs::signature::UnparsedPublicKey::new(&aws_lc_rs::signature::RSA_PKCS1_2048_8192_SHA256, public_key.clone());
                let claims: Value = B64URL.decode(b).ok().and_then(|c| serde_json::from_slice(&c).ok()).unwrap_or(Value::Null);
                key.verify(format!("{h}.{b}").as_bytes(), &B64URL.decode(s).unwrap_or_default()).is_ok() && claims.get("iss") == Some(&json!(99))
            });
            let token_ok = auth.starts_with("Bearer ghs_fake_");
            let path = full.split('?').next().unwrap_or_default().to_owned();
            let q = query(&full);
            let arg = |k: &str| q.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone()).unwrap_or_default();
            let Ok(mut g) = gh.lock() else { continue };
            g.calls.push((method.clone(), full.clone(), body.clone()));
            let seg: Vec<&str> = path.trim_start_matches('/').split('/').collect();
            let (status, reply) = match (method.as_str(), seg.as_slice()) {
                ("GET", ["orgs", "acme", "installation"] | ["repos", "acme", "releases", "installation"]) if jwt_ok => (200, json!({ "id": 7 })),
                ("POST", ["app", "installations", "7", "access_tokens"]) if jwt_ok => {
                    next_token += 1;
                    g.tokens.push(body.clone());
                    (201, json!({ "token": format!("ghs_fake_{next_token}"), "expires_at": "2099-01-01T00:00:00Z" }))
                }
                ("GET", ["repos", "acme", "releases"]) if token_ok => {
                    if g.made { (200, json!({ "full_name": "acme/releases", "private": true })) } else { (404, json!({ "message": "Not Found" })) }
                }
                ("POST", ["orgs", "acme", "repos"]) if token_ok => {
                    let made = git(&root, &["init", "-q", "--bare", "acme/releases.git"]).is_ok();
                    g.made = made;
                    if made { (201, json!({ "full_name": "acme/releases" })) } else { (422, json!({ "message": "Repository creation failed." })) }
                }
                ("GET", ["repos", "acme", "releases", "pulls"]) if token_ok => {
                    let head = arg("head");
                    let found: Vec<Value> = g.pulls.iter().filter(|p| format!("acme:{}", p.pointer("/head/ref").and_then(Value::as_str).unwrap_or_default()) == head).cloned().collect();
                    (200, json!(found))
                }
                ("POST", ["repos", "acme", "releases", "pulls"]) if token_ok => {
                    let number = g.pulls.len() + 1;
                    let pull = json!({
                        "number": number, "state": "open", "merged": false, "merged_at": null, "merge_commit_sha": null,
                        "title": body.get("title"), "body": body.get("body"),
                        "head": { "ref": body.get("head") }, "base": { "ref": body.get("base") },
                        "html_url": format!("https://github.example/acme/releases/pull/{number}"),
                    });
                    g.pulls.push(pull.clone());
                    (201, pull)
                }
                ("GET", ["repos", "acme", "releases", "pulls", n]) if token_ok => match n.parse::<usize>().ok().and_then(|n| g.pulls.get(n.wrapping_sub(1))) {
                    Some(p) => (200, p.clone()),
                    None => (404, json!({ "message": "Not Found" })),
                },
                ("PATCH", ["repos", "acme", "releases", "pulls", n]) if token_ok => match n.parse::<usize>().ok().and_then(|n| g.pulls.get_mut(n.wrapping_sub(1))) {
                    Some(p) => {
                        if let (Some(o), Some(s)) = (p.as_object_mut(), body.get("state")) {
                            o.insert("state".into(), s.clone());
                        }
                        (200, p.clone())
                    }
                    None => (404, json!({ "message": "Not Found" })),
                },
                ("POST", ["repos", "acme", "releases", "issues", _, "comments"]) if token_ok => (201, json!({ "id": 1 })),
                ("DELETE", ["repos", "acme", "releases", "git", "refs", "heads", rest @ ..]) if token_ok => {
                    let branch = rest.join("/");
                    let gone = git(&root.join("acme/releases.git"), &["update-ref", "-d", &format!("refs/heads/{branch}")]).is_ok();
                    if gone { (204, Value::Null) } else { (422, json!({ "message": "Reference does not exist" })) }
                }
                ("GET", ["repos", "acme", "releases", "deployments"]) if token_ok => {
                    let (sha, env) = (arg("sha"), arg("environment"));
                    let found: Vec<Value> = g.deployments.iter().rev().filter(|d| d.get("sha") == Some(&json!(sha)) && d.get("environment") == Some(&json!(env))).cloned().collect();
                    (200, json!(found))
                }
                ("POST", ["repos", "acme", "releases", "deployments"]) if token_ok => {
                    let id = 100 + g.deployments.len();
                    g.deployments.push(json!({ "id": id, "sha": body.get("ref"), "environment": body.get("environment") }));
                    (201, json!({ "id": id }))
                }
                ("POST", ["repos", "acme", "releases", "deployments", id, "statuses"]) if token_ok => {
                    g.statuses.push((id.parse().unwrap_or(0), body.clone()));
                    (201, json!({ "id": 1 }))
                }
                (_, _) if !jwt_ok && !token_ok => (401, json!({ "message": "Bad credentials" })),
                _ => (404, json!({ "message": "Not Found" })),
            };
            drop(g);
            let text = if reply.is_null() { String::new() } else { reply.to_string() };
            let mut s = reader.into_inner();
            let _ = write!(s, "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}", text.len());
        }
    });
    Ok((port, shared))
}

/// GitHub's git over smart HTTP: `git http-backend` behind a server of this test's, each request on
/// a thread of its own. The release repository answers only an installation token (as GitHub's
/// private repositories do); `acme/app` anyone. Its port.
fn git_http(root: &Path) -> anyhow::Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    let root = root.to_path_buf();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let root = root.clone();
            std::thread::spawn(move || {
                let _ = serve_git(stream, &root);
            });
        }
    });
    Ok(port)
}

fn serve_git(stream: std::net::TcpStream, root: &Path) -> anyhow::Result<()> {
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
    let (path, query) = target.split_once('?').unwrap_or((target.as_str(), ""));
    // The App's token, as git sends it: `x-access-token` and the token, in Basic.
    let token = header("authorization")
        .and_then(|a| a.strip_prefix("Basic ").and_then(|b| B64.decode(b).ok()))
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .filter(|up| up.starts_with("x-access-token:ghs_fake_"));
    if path.starts_with("/acme/releases.git") && token.is_none() {
        write!(w, "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"GitHub\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")?;
        return Ok(());
    }
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
    // An authenticated user may push (http-backend's default for receive-pack).
    if token.is_some() {
        cgi.env("REMOTE_USER", "x-access-token");
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

/// GitHub's webhook delivery to kiln, signed with the App's webhook secret: kiln's status.
fn deliver(port: u16, event: &str, body: &Value) -> anyhow::Result<String> {
    let body = body.to_string();
    let sig = format!("sha256={}", kiln::util::hmac_hex(b"s3cret", body.as_bytes()));
    let mut conn = std::net::TcpStream::connect(("127.0.0.1", port))?;
    write!(conn, "POST /api/hooks/github/app HTTP/1.1\r\nHost: x\r\nX-GitHub-Event: {event}\r\nX-Hub-Signature-256: {sig}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())?;
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
fn p3_release_repositories_on_github() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let kp = aws_lc_rs::rsa::KeyPair::generate(aws_lc_rs::rsa::KeySize::Rsa2048).map_err(|_| anyhow::anyhow!("keygen"))?;
    let der = aws_lc_rs::encoding::AsDer::<aws_lc_rs::encoding::Pkcs8V1Der>::as_der(&kp).map_err(|_| anyhow::anyhow!("der"))?;
    let key = dir.path().join("app.pem");
    std::fs::write(&key, format!("-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n", B64.encode(der.as_ref())))?;
    let secret = dir.path().join("webhook-secret");
    std::fs::write(&secret, "s3cret\n")?;
    // GitHub's git, and an app's repository on it: plain manifests under deploy/, tagged v1.
    let gh = dir.path().join("gh");
    std::fs::create_dir_all(gh.join("acme"))?;
    git(&gh, &["init", "-q", "--bare", "acme/app.git"])?;
    let work = dir.path().join("work");
    std::fs::create_dir_all(work.join("deploy"))?;
    std::fs::write(work.join("deploy/config.yaml"), "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: web\ndata:\n  hello: world\n")?;
    git(&work, &["init", "-q"])?;
    git(&work, &["add", "."])?;
    git(&work, &["commit", "-qm", "first"])?;
    git(&work, &["tag", "v1"])?;
    git(&work, &["push", "-q", &gh.join("acme/app.git").to_string_lossy(), "HEAD:main", "v1"])?;
    let port = git_http(&gh)?;
    let (api, fake) = fake_github(kp.public_key().as_ref().to_vec(), gh.clone())?;
    let ci = format!(
        "github = {{ app_id = 99, private_key_file = \"{}\", webhook_secret_file = \"{}\", api_url = \"http://127.0.0.1:{api}\", web_url = \"http://127.0.0.1:{port}\" }}",
        key.display(),
        secret.display()
    );
    let s = start_cd(dir.path(), &Options { ci: &ci, ..Options::default() })?;
    let saw = |m: &str, p: &str| fake.lock().map(|g| g.calls.iter().filter(|(cm, cp, _)| cm == m && cp.starts_with(p)).count()).unwrap_or(0);

    // Two environments on one release repository on GitHub, made through the App once: staging
    // commits its releases, production proposes them as pull requests.
    let env = |name: &str, gates: Value| json!({ "name": name, "provider": "kilncd", "release_repo": { "host": "github", "repo": "acme/releases", "branch": "main" }, "gates": gates });
    let (st, body) = http(s.port, "POST", "/api/deploy/environments", &s.admin, Some(&env("staging", json!({}))))?;
    anyhow::ensure!((200..300).contains(&st), "staging: {st} {body}\n{}", s.log());
    let (st, body) = http(s.port, "POST", "/api/deploy/environments", &s.admin, Some(&env("production", json!({ "via": "pull-request" }))))?;
    anyhow::ensure!((200..300).contains(&st), "production: {st} {body}");
    assert_eq!(saw("POST", "/orgs/acme/repos"), 1, "made once");
    // A pull request is GitHub's, and its merge is the approval.
    let refused = |e: Value| http(s.port, "POST", "/api/deploy/environments", &s.admin, Some(&e)).map(|r| r.0);
    assert_eq!(refused(json!({ "name": "pr-on-kiln", "release_repo": { "host": "kiln", "repo": "kiln/releases" }, "gates": { "via": "pull-request" } }))?, 400);
    assert_eq!(refused(env("pr-approvals", json!({ "via": "pull-request", "approvals": { "count": 1 } })))?, 400);

    let app = json!({ "name": "web", "project": "shop", "package": { "kind": "git", "repo": format!("http://127.0.0.1:{port}/acme/app.git"), "path": "deploy" } });
    let (st, body) = http(s.port, "POST", "/api/deploy/apps", &s.admin, Some(&app))?;
    anyhow::ensure!((200..300).contains(&st), "app: {st} {body}");
    for e in ["staging", "production"] {
        let (st, body) = http(s.port, "POST", &format!("/api/deploy/apps/web/environments/{e}"), &s.admin, Some(&json!({})))?;
        anyhow::ensure!(st == 201, "register in {e}: {st} {body}\n{}", s.log());
    }
    // Pushed to GitHub with tokens for that one repository.
    let releases = gh.join("acme/releases.git");
    let log = git(&releases, &["log", "--format=%B", "main"])?;
    assert!(log.contains("Kiln-Registration: web/staging") && log.contains("Kiln-Registration: web/production"), "{log}");
    let tokens = fake.lock().map(|g| g.tokens.clone()).unwrap_or_default();
    assert!(tokens.iter().any(|t| t.get("repositories") == Some(&json!(["releases"])) && t.pointer("/permissions/contents") == Some(&json!("write"))), "{tokens:?}");

    // A release to staging: its commit pushed, then delivered, and a deployment on GitHub.
    let id = ask(s.port, &s.admin, "staging")?;
    let mut commit = String::new();
    until(60, "staging's release delivering", || {
        let r = release(s.port, &s.admin, id)?;
        commit = r.get("commit").and_then(Value::as_str).unwrap_or_default().to_owned();
        Ok((r.get("state") != Some(&json!("delivering"))).then(|| format!("{r}\n{}", s.log())))
    })?;
    assert_eq!(git(&releases, &["rev-parse", "main"])?, commit);
    assert!(git(&releases, &["log", "-1", "--format=%B", &commit])?.contains(&format!("Kiln-Release: {id}")));
    let status_of = |sha: &str, env: &str| -> Vec<Value> {
        fake.lock()
            .map(|g| {
                let ids: Vec<u64> = g.deployments.iter().filter(|d| d.get("sha") == Some(&json!(sha)) && d.get("environment") == Some(&json!(env))).filter_map(|d| d.get("id").and_then(Value::as_u64)).collect();
                g.statuses.iter().filter(|(d, _)| ids.contains(d)).map(|(_, b)| b.clone()).collect()
            })
            .unwrap_or_default()
    };
    until(30, "staging's deployment in progress", || Ok((!status_of(&commit, "staging/web").iter().any(|b| b.get("state") == Some(&json!("in_progress")))).then(|| format!("{:?}", status_of(&commit, "staging/web")))))?;
    let first = status_of(&commit, "staging/web").into_iter().next().unwrap_or_default();
    assert!(first.get("log_url").and_then(Value::as_str).is_some_and(|u| u.ends_with(&format!("/deployments/releases/{id}"))), "{first}");

    // Cancelled while delivering: its commit reverted on GitHub, its deployment inactive.
    let (st, body) = http(s.port, "POST", &format!("/api/deploy/releases/{id}/cancel"), &s.admin, Some(&json!({})))?;
    anyhow::ensure!(st == 200, "cancel: {st} {body}");
    until(60, "the revert pushed", || {
        let log = git(&releases, &["log", "--format=%B", "main"])?;
        Ok((!log.contains(&format!("Kiln-Reverts: {id}"))).then(|| format!("{log}\n{}", s.log())))
    })?;
    until(30, "staging's deployment inactive", || Ok((!status_of(&commit, "staging/web").iter().any(|b| b.get("state") == Some(&json!("inactive")))).then(|| format!("{:?}", status_of(&commit, "staging/web")))))?;

    // Production: a pull request from a branch of the release's own.
    let id2 = ask(s.port, &s.admin, "production")?;
    until(60, "production's pull request", || {
        let r = release(s.port, &s.admin, id2)?;
        Ok((!r.get("message").and_then(Value::as_str).is_some_and(|m| m.contains("pull request #1"))).then(|| format!("{r}\n{}", s.log())))
    })?;
    let r = release(s.port, &s.admin, id2)?;
    assert_eq!((r.get("state"), r.get("commit")), (Some(&json!("committing")), None), "{r}");
    // Its branch: its id and when it was asked for, so another kiln's release 2 is never taken for it.
    let branch = format!("kiln/release-{id2}-{}", r.get("created_at").and_then(Value::as_i64).unwrap_or(0));
    let pull = fake.lock().map(|g| g.pulls.first().cloned().unwrap_or_default()).unwrap_or_default();
    assert_eq!((pull.pointer("/head/ref"), pull.pointer("/base/ref")), (Some(&json!(branch)), Some(&json!("main"))), "{pull}");
    assert_eq!(r.pointer("/provider/pull_request/branch"), Some(&json!(branch)), "{r}");
    assert!(pull.get("body").and_then(Value::as_str).is_some_and(|b| b.contains("Merging this pull request releases it")), "{pull}");
    let head = git(&releases, &["rev-parse", &branch])?;
    assert!(git(&releases, &["log", "-1", "--format=%B", &head])?.contains(&format!("Kiln-Release: {id2}")));
    assert_ne!(git(&releases, &["rev-parse", "main"])?, head, "main waits for the merge");

    // GitHub merges it (a merge commit), and says so: the merge is delivered, its approval kept.
    let base = git(&releases, &["rev-parse", "main"])?;
    let tree = git(&releases, &["rev-parse", &format!("{head}^{{tree}}")])?;
    let merge = git(&releases, &["commit-tree", &tree, "-p", &base, "-p", &head, "-m", &format!("Merge pull request #1 from acme/{branch}")])?;
    git(&releases, &["update-ref", "refs/heads/main", &merge])?;
    if let Ok(mut g) = fake.lock()
        && let Some(o) = g.pulls.get_mut(0).and_then(Value::as_object_mut)
    {
        o.insert("state".into(), json!("closed"));
        o.insert("merged".into(), json!(true));
        o.insert("merged_at".into(), json!("2026-10-20T12:00:00Z"));
        o.insert("merge_commit_sha".into(), json!(merge));
        o.insert("merged_by".into(), json!({ "login": "grace" }));
    }
    let hook = |n: u64, b: &str, merged: bool| json!({ "action": "closed", "number": n, "pull_request": { "number": n, "merged": merged, "head": { "ref": b } }, "repository": { "full_name": "acme/releases" } });
    assert!(deliver(s.port, "pull_request", &hook(1, &branch, true))?.contains("202"));
    until(20, "the merge delivering", || {
        let r = release(s.port, &s.admin, id2)?;
        Ok((r.get("commit") != Some(&json!(merge))).then(|| format!("{r}\n{}", s.log())))
    })?;
    let r = release(s.port, &s.admin, id2)?;
    assert!(r.get("approvals").and_then(Value::as_array).is_some_and(|a| a.iter().any(|x| x.get("by") == Some(&json!("grace on GitHub")))), "{r}");
    until(30, "production's deployment in progress", || Ok((!status_of(&merge, "production/web").iter().any(|b| b.get("state") == Some(&json!("in_progress")))).then(|| format!("{:?}", status_of(&merge, "production/web")))))?;
    until(10, "the release's branch deleted", || Ok((saw("DELETE", &format!("/repos/acme/releases/git/refs/heads/{branch}")) == 0).then(|| "not yet".to_owned())))?;
    // Cancelled while delivering: the merge reverted on GitHub.
    let (st, body) = http(s.port, "POST", &format!("/api/deploy/releases/{id2}/cancel"), &s.admin, Some(&json!({})))?;
    anyhow::ensure!(st == 200, "cancel: {st} {body}");
    until(60, "the merge reverted", || {
        let log = git(&releases, &["log", "--format=%B", "main"])?;
        Ok((!log.contains(&format!("Kiln-Reverts: {id2}"))).then(|| format!("{log}\n{}", s.log())))
    })?;

    // A pull request closed unmerged on GitHub: its release cancelled, its branch gone.
    let id3 = ask(s.port, &s.admin, "production")?;
    until(60, "the second pull request", || Ok((fake.lock().map(|g| g.pulls.len()).unwrap_or(0) < 2).then(|| s.log())))?;
    let b3 = fake.lock().map(|g| g.pulls.get(1).and_then(|p| p.pointer("/head/ref")).and_then(Value::as_str).unwrap_or_default().to_owned()).unwrap_or_default();
    assert!(b3.starts_with(&format!("kiln/release-{id3}-")), "{b3}");
    if let Ok(mut g) = fake.lock()
        && let Some(o) = g.pulls.get_mut(1).and_then(Value::as_object_mut)
    {
        o.insert("state".into(), json!("closed"));
    }
    assert!(deliver(s.port, "pull_request", &hook(2, &b3, false))?.contains("202"));
    until(20, "closed is cancelled", || {
        let r = release(s.port, &s.admin, id3)?;
        Ok((r.get("state") != Some(&json!("cancelled"))).then(|| format!("{r}")))
    })?;
    let r = release(s.port, &s.admin, id3)?;
    assert!(r.get("message").and_then(Value::as_str).is_some_and(|m| m.contains("closed without merging")), "{r}");

    // Cancelled in kiln while its pull request waits: closed on GitHub, saying why.
    let id4 = ask(s.port, &s.admin, "production")?;
    until(60, "the third pull request", || Ok((fake.lock().map(|g| g.pulls.len()).unwrap_or(0) < 3).then(|| s.log())))?;
    until(20, "the third pull request recorded", || {
        let r = release(s.port, &s.admin, id4)?;
        Ok((!r.get("message").and_then(Value::as_str).is_some_and(|m| m.contains("pull request #3"))).then(|| format!("{r}")))
    })?;
    let (st, body) = http(s.port, "POST", &format!("/api/deploy/releases/{id4}/cancel"), &s.admin, Some(&json!({})))?;
    anyhow::ensure!(st == 200, "cancel a proposed release: {st} {body}");
    until(20, "the pull request closed", || {
        let closed = fake.lock().map(|g| g.pulls.get(2).and_then(|p| p.get("state")) == Some(&json!("closed"))).unwrap_or(false);
        Ok((!closed || saw("POST", "/repos/acme/releases/issues/3/comments") == 0).then(|| s.log()))
    })?;

    // A commit kiln didn't make, and GitHub's push webhook: a release of origin `git`.
    let push_hook = |before: &str, after: &str| json!({ "ref": "refs/heads/main", "before": before, "after": after, "repository": { "full_name": "acme/releases" } });
    let before = git(&releases, &["rev-parse", "main"])?;
    assert!(deliver(s.port, "push", &push_hook(&before, &before))?.contains("202"));
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
    assert!(deliver(s.port, "push", &push_hook(&before, &c))?.contains("202"));
    until(30, "a release of origin git", || {
        let (_, body) = http(s.port, "GET", "/api/deploy/releases?app=web&environment=staging", &s.admin, None)?;
        let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
        let found = v.get("releases").and_then(Value::as_array).is_some_and(|l| l.iter().any(|r| r.get("origin") == Some(&json!("git")) && r.get("commit") == Some(&json!(c))));
        Ok((!found).then(|| format!("{body}\n{}", s.log())))
    })?;
    Ok(())
}
