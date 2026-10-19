//! The GitHub App (docs/ci.md W14-R10, R12), against a fake GitHub API and a `git daemon` that
//! stands for GitHub's git: a KilnCI run of a repository on GitHub gets a check run there, made
//! when the run starts and done with its conclusion, through installation tokens the App's JWT
//! (checked against its key) gets for that one repository; a job of a `github_api: github`
//! repository calls GitHub's API with such a token, its permissions the job's; GitHub's "Re-run"
//! (a signed `check_run` webhook) starts a new attempt, which gets a check run of its own.

#![cfg(unix)]

mod common;

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, Command};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aws_lc_rs::signature::KeyPair as _;
use base64::Engine as _;
use common::{Options, free_port, http, start};
use serde_json::{Value, json};

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;
const B64URL: base64::engine::GeneralPurpose = base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// What the fake GitHub was asked: (method, path, authorization, body).
type Calls = Arc<Mutex<Vec<(String, String, String, Value)>>>;

/// A fake of the parts of GitHub's API the App uses.
fn fake_github(public_key: Vec<u8>) -> anyhow::Result<(u16, Calls)> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    let calls: Calls = Arc::default();
    let seen = calls.clone();
    std::thread::spawn(move || {
        let mut next_check = 1000u64;
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
            let (method, path) = (parts.next().unwrap_or_default().to_owned(), parts.next().unwrap_or_default().to_owned());
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
            // The App's own calls carry its JWT, signed by its key.
            let jwt_ok = auth.strip_prefix("Bearer ").is_some_and(|jwt| {
                let mut p = jwt.split('.');
                let (h, b, s) = (p.next().unwrap_or_default(), p.next().unwrap_or_default(), p.next().unwrap_or_default());
                let key = aws_lc_rs::signature::UnparsedPublicKey::new(&aws_lc_rs::signature::RSA_PKCS1_2048_8192_SHA256, public_key.clone());
                let claims: Value = B64URL.decode(b).ok().and_then(|c| serde_json::from_slice(&c).ok()).unwrap_or(Value::Null);
                key.verify(format!("{h}.{b}").as_bytes(), &B64URL.decode(s).unwrap_or_default()).is_ok() && claims.get("iss") == Some(&json!(99))
            });
            let token_ok = auth.starts_with("Bearer ghs_fake_");
            let (status, reply) = match (method.as_str(), path.split('?').next().unwrap_or_default()) {
                ("GET", "/repos/acme/app/installation") if jwt_ok => (200, json!({ "id": 7 })),
                ("POST", "/app/installations/7/access_tokens") if jwt_ok => {
                    next_token += 1;
                    (201, json!({ "token": format!("ghs_fake_{next_token}"), "expires_at": "2099-01-01T00:00:00Z" }))
                }
                ("POST", "/repos/acme/app/check-runs") if token_ok => {
                    next_check += 1;
                    (201, json!({ "id": next_check }))
                }
                ("PATCH", p) if token_ok && p.starts_with("/repos/acme/app/check-runs/") => (200, json!({})),
                ("GET", "/repos/acme/app/issues") if token_ok => (200, json!([])),
                (_, _) if !jwt_ok && !token_ok => (401, json!({ "message": "Bad credentials" })),
                _ => (404, json!({ "message": "Not Found" })),
            };
            if let Ok(mut c) = seen.lock() {
                c.push((method.clone(), path.clone(), auth.clone(), body));
            }
            let text = reply.to_string();
            let mut s = reader.into_inner();
            let _ = write!(s, "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}", text.len());
        }
    });
    Ok((port, calls))
}

fn git(dir: &Path, args: &[&str]) -> anyhow::Result<String> {
    let o = Command::new("git").args(["-c", "user.name=T", "-c", "user.email=t@example.com", "-c", "init.defaultBranch=main"]).args(args).current_dir(dir).env("GIT_CONFIG_NOSYSTEM", "1").env("HOME", dir).output()?;
    anyhow::ensure!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
    Ok(String::from_utf8_lossy(&o.stdout).trim().to_owned())
}

struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
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

#[test]
fn w14_r10_github_app() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    // The App's key, as GitHub would hand it over, and its webhook secret.
    let kp = aws_lc_rs::rsa::KeyPair::generate(aws_lc_rs::rsa::KeySize::Rsa2048).map_err(|_| anyhow::anyhow!("keygen"))?;
    let der = aws_lc_rs::encoding::AsDer::<aws_lc_rs::encoding::Pkcs8V1Der>::as_der(&kp).map_err(|_| anyhow::anyhow!("der"))?;
    let key = dir.path().join("app.pem");
    std::fs::write(&key, format!("-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n", B64.encode(der.as_ref())))?;
    let secret = dir.path().join("webhook-secret");
    std::fs::write(&secret, "s3cret\n")?;
    let (api, calls) = fake_github(kp.public_key().as_ref().to_vec())?;

    // GitHub's git: a bare repository `git daemon` serves, with a workflow and `github_api: github`.
    let gh = dir.path().join("gh");
    std::fs::create_dir_all(gh.join("acme"))?;
    git(&gh, &["init", "-q", "--bare", "acme/app.git"])?;
    let work = dir.path().join("work");
    std::fs::create_dir_all(work.join(".github/workflows"))?;
    std::fs::create_dir_all(work.join(".kiln"))?;
    std::fs::write(work.join(".kiln/config.yaml"), "github_api: github\n")?;
    std::fs::write(
        work.join(".github/workflows/ci.yml"),
        r#"name: ci
on: push
permissions:
  issues: read
jobs:
  build:
    runs-on: local
    steps:
      - env:
          TOKEN: ${{ github.token }}
        run: |
          set -e
          case "$TOKEN" in ghs_fake_*) ;; *) echo "not GitHub's token: $TOKEN"; exit 1 ;; esac
          curl -sf -H "Authorization: Bearer $TOKEN" "$GITHUB_API_URL/repos/$GITHUB_REPOSITORY/issues"
          echo "server: $GITHUB_SERVER_URL"
"#,
    )?;
    git(&work, &["init", "-q"])?;
    git(&work, &["add", "."])?;
    git(&work, &["commit", "-qm", "first"])?;
    git(&work, &["push", "-q", &gh.join("acme/app.git").to_string_lossy(), "HEAD:main"])?;
    let port = free_port()?;
    let daemon = Command::new("git").args(["daemon", "--reuseaddr", "--export-all", &format!("--port={port}"), "--listen=127.0.0.1"]).arg(format!("--base-path={}", gh.display())).arg(&gh).spawn()?;
    let _daemon = Daemon(daemon);
    std::thread::sleep(Duration::from_millis(500));

    let ci = format!(
        "github = {{ app_id = 99, private_key_file = \"{}\", webhook_secret_file = \"{}\", api_url = \"http://127.0.0.1:{api}\", web_url = \"http://127.0.0.1:{port}\" }}",
        key.display(),
        secret.display()
    );
    let extra = format!("[[repos]]\nname = \"acme/app\"\nclone_url = \"git://127.0.0.1:{port}/acme/app.git\"\n");
    let s = start(dir.path(), &Options { agent: true, ci: &ci, extra: &extra })?;
    let (st, app) = http(s.port, "GET", "/api/ci/github/app", &s.admin, None)?;
    assert!(st == 200 && app.contains("\"configured\":true") && app.contains("\"source\":\"config\""), "{st} {app}");
    // kiln adopts what the repository has on its first look; the next commit is built.
    let mut repo_id = String::new();
    until(60, "the repository is polled", || {
        let (_, body) = http(s.port, "GET", "/api/repos", &s.admin, None)?;
        let list: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
        let r = list.as_array().and_then(|l| l.iter().find(|r| r.get("name") == Some(&json!("acme/app")))).cloned().unwrap_or_default();
        repo_id = r.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
        Ok(r.get("last_polled_at").is_none_or(Value::is_null).then_some(body))
    })?;
    std::fs::write(work.join("README.md"), "second\n")?;
    git(&work, &["add", "README.md"])?;
    git(&work, &["commit", "-qm", "second"])?;
    git(&work, &["push", "-q", &gh.join("acme/app.git").to_string_lossy(), "HEAD:main"])?;
    let sha = git(&work, &["rev-parse", "HEAD"])?;
    let (st, body) = http(s.port, "POST", &format!("/api/repos/{repo_id}/poll"), &s.admin, Some(&json!({})))?;
    anyhow::ensure!((200..300).contains(&st), "poll: {st} {body}");

    // The run, and its check run on GitHub: made, then done with its conclusion.
    let checks = |c: &Calls| -> Vec<(String, String, Value)> { c.lock().map(|v| v.iter().filter(|(_, p, _, _)| p.contains("/check-runs")).map(|(m, p, _, b)| (m.clone(), p.clone(), b.clone())).collect()).unwrap_or_default() };
    until(240, "the check run completes", || {
        let list = checks(&calls);
        let done = list.iter().any(|(m, _, b)| m == "PATCH" && b.get("conclusion") == Some(&json!("success")));
        let failed = list.iter().any(|(_, _, b)| b.get("conclusion") == Some(&json!("failure")));
        anyhow::ensure!(!failed, "the run failed: {list:?}\n{}", s.log());
        Ok((!done).then(|| format!("{list:?}\nruns: {}\ncalls: {:?}\n{}", http(s.port, "GET", "/api/runs", &s.admin, None).map(|r| r.1).unwrap_or_default(), calls.lock().map(|c| c.iter().map(|(m, p, _, _)| format!("{m} {p}")).collect::<Vec<_>>()).unwrap_or_default(), s.log())))
    })?;
    let list = checks(&calls);
    let (_, _, made) = list.iter().find(|(m, _, _)| m == "POST").cloned().unwrap_or_default();
    assert_eq!((made.get("head_sha"), made.get("name")), (Some(&json!(sha)), Some(&json!("ci"))), "{made}");
    let run = made.get("external_id").and_then(Value::as_str).unwrap_or_default().to_owned();
    assert!(!run.is_empty());
    // Installation tokens: one repository each, the permissions a call needs (the job's: its own).
    let tokens: Vec<Value> = calls.lock().map(|v| v.iter().filter(|(_, p, _, _)| p.ends_with("/access_tokens")).map(|(_, _, _, b)| b.clone()).collect()).unwrap_or_default();
    assert!(tokens.iter().all(|t| t.get("repositories") == Some(&json!(["app"]))), "{tokens:?}");
    assert!(tokens.iter().any(|t| t.pointer("/permissions/checks") == Some(&json!("write"))), "{tokens:?}");
    assert!(tokens.iter().any(|t| t.pointer("/permissions/issues") == Some(&json!("read")) && t.pointer("/permissions/checks").is_none()), "the job's: {tokens:?}");
    assert!(calls.lock().map(|v| v.iter().any(|(m, p, a, _)| m == "GET" && p == "/repos/acme/app/issues" && a.starts_with("Bearer ghs_fake_"))).unwrap_or(false), "the job called GitHub's API");

    // GitHub's Re-run: a new attempt, and a check run of its own.
    let posts_before = checks(&calls).iter().filter(|(m, _, _)| m == "POST").count();
    let body = json!({ "action": "rerequested", "check_run": { "id": 1001, "external_id": run }, "repository": { "full_name": "acme/app" } }).to_string();
    let sig = format!("sha256={}", kiln::util::hmac_hex(b"s3cret", body.as_bytes()));
    let mut conn = std::net::TcpStream::connect(("127.0.0.1", s.port))?;
    write!(conn, "POST /api/hooks/github/app HTTP/1.1\r\nHost: x\r\nX-GitHub-Event: check_run\r\nX-Hub-Signature-256: {sig}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())?;
    let mut answer = String::new();
    conn.read_to_string(&mut answer)?;
    assert!(answer.starts_with("HTTP/1.1 202"), "{answer}");
    let mut bad = std::net::TcpStream::connect(("127.0.0.1", s.port))?;
    write!(bad, "POST /api/hooks/github/app HTTP/1.1\r\nHost: x\r\nX-GitHub-Event: check_run\r\nX-Hub-Signature-256: sha256=00\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())?;
    let mut refused = String::new();
    bad.read_to_string(&mut refused)?;
    assert!(refused.starts_with("HTTP/1.1 401"), "an unsigned event is refused: {refused}");
    until(240, "the second attempt's check run", || {
        let posts = checks(&calls).iter().filter(|(m, _, _)| m == "POST").count();
        Ok((posts <= posts_before).then(|| format!("{:?}", checks(&calls))))
    })?;
    Ok(())
}
