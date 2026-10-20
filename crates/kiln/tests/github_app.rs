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
            let run555 = json!({ "id": 555, "name": "ci", "path": ".github/workflows/ci.yml", "display_title": "a failing run", "event": "push", "head_branch": "main", "head_sha": "a".repeat(40),
                                 "status": "completed", "conclusion": "failure", "run_number": 7, "run_attempt": 1, "html_url": "https://github.example/acme/gha/actions/runs/555",
                                 "created_at": "2026-10-18T12:00:00Z", "run_started_at": "2026-10-18T12:00:05Z", "updated_at": "2026-10-18T12:03:00Z", "actor": { "login": "ada" } });
            let run556 = json!({ "id": 556, "name": "ci", "path": ".github/workflows/ci.yml", "display_title": "a running one", "event": "push", "head_branch": "main", "head_sha": "b".repeat(40),
                                 "status": "in_progress", "conclusion": null, "run_number": 8, "run_attempt": 1, "html_url": "https://github.example/acme/gha/actions/runs/556",
                                 "created_at": "2026-10-18T13:00:00Z", "run_started_at": "2026-10-18T13:00:05Z", "updated_at": "2026-10-18T13:00:06Z", "actor": { "login": "ada" } });
            let mut location: Option<String> = None;
            let (status, reply) = match (method.as_str(), path.split('?').next().unwrap_or_default()) {
                ("GET", "/repos/acme/app/installation" | "/repos/acme/gha/installation") if jwt_ok => (200, json!({ "id": 7 })),
                ("GET", "/repos/acme/gha/actions/runs") if token_ok => (200, json!({ "total_count": 1, "workflow_runs": [run555] })),
                ("GET", "/repos/acme/gha/actions/runs/556") if token_ok => (200, run556),
                ("GET", "/repos/acme/gha/actions/runs/555/jobs") if token_ok => (200, json!({ "jobs": [{ "id": 9001, "name": "build", "status": "completed", "conclusion": "failure", "runner_name": "gh-1", "labels": ["ubuntu-latest"],
                    "started_at": "2026-10-18T12:00:06Z", "completed_at": "2026-10-18T12:02:00Z", "html_url": "https://github.example/acme/gha/actions/runs/555/job/9001",
                    "steps": [{ "name": "Set up job", "status": "completed", "conclusion": "success", "number": 1 }, { "name": "test", "status": "completed", "conclusion": "failure", "number": 2 }] }] })),
                ("GET", "/repos/acme/gha/actions/runs/556/jobs") if token_ok => (200, json!({ "jobs": [{ "id": 9002, "name": "test", "status": "in_progress", "conclusion": null, "steps": [] }] })),
                ("GET", "/repos/acme/gha/actions/jobs/9001/logs") if token_ok => {
                    location = Some(format!("http://127.0.0.1:{port}/blob/logs/9001"));
                    (302, json!({}))
                }
                ("GET", "/blob/logs/9001") if auth.is_empty() => (200, json!("build failed: boom")),
                ("POST", "/repos/acme/gha/actions/runs/555/rerun-failed-jobs") if token_ok => (201, json!({})),
                ("POST", "/repos/acme/gha/actions/runs/555/cancel") if token_ok => (202, json!({})),
                ("POST", "/repos/acme/gha/actions/workflows/ci.yml/dispatches") if token_ok => (204, json!(null)),
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
                // The Git Data API and pulls, for what kiln saves to GitHub (W21-R06).
                ("GET", p) if token_ok && p.starts_with("/repos/acme/app/git/commits/") => (200, json!({ "sha": p.rsplit('/').next().unwrap_or_default(), "tree": { "sha": "tree0" } })),
                ("POST", "/repos/acme/app/git/blobs") if token_ok => (201, json!({ "sha": "blob1" })),
                ("POST", "/repos/acme/app/git/trees") if token_ok => (201, json!({ "sha": "tree1" })),
                ("POST", "/repos/acme/app/git/commits") if token_ok => (201, json!({ "sha": "c".repeat(40) })),
                ("POST", "/repos/acme/app/git/refs") if token_ok => (201, json!({ "ref": body.get("ref").cloned().unwrap_or_default() })),
                ("POST", "/repos/acme/app/pulls") if token_ok => (201, json!({ "number": 12, "html_url": "https://github.example/acme/app/pull/12" })),
                (_, _) if !jwt_ok && !token_ok => (401, json!({ "message": "Bad credentials" })),
                _ => (404, json!({ "message": "Not Found" })),
            };
            if let Ok(mut c) = seen.lock() {
                c.push((method.clone(), path.clone(), auth.clone(), body));
            }
            let text = match &reply {
                Value::String(t) => t.clone(),
                Value::Null => String::new(),
                other => other.to_string(),
            };
            let extra = location.map(|l| format!("Location: {l}\r\n")).unwrap_or_default();
            let mut s = reader.into_inner();
            let _ = write!(s, "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{text}", text.len());
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
    git(&gh, &["init", "-q", "--bare", "acme/gha.git"])?;
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
    // And one the App's events run (W03).
    std::fs::write(
        work.join(".github/workflows/triage.yml"),
        "name: triage\non:\n  issues:\n    types: [opened]\njobs:\n  t:\n    runs-on: local\n    steps:\n      - run: test \"${{ github.event.issue.number }}\" = 5\n",
    )?;
    git(&work, &["init", "-q"])?;
    git(&work, &["add", "."])?;
    git(&work, &["commit", "-qm", "first"])?;
    git(&work, &["push", "-q", &gh.join("acme/app.git").to_string_lossy(), "HEAD:main"])?;
    let port = free_port()?;
    // The daemon dies with this test, however it ends (`setpriv --pdeathsig`, where there is one).
    // `git daemon` would run it as a child that outlives a killed `git`: git-daemon itself.
    let exec_path = String::from_utf8(Command::new("git").arg("--exec-path").output()?.stdout)?.trim().to_owned();
    let git_daemon = Path::new(&exec_path).join("git-daemon");
    let tied = Command::new("setpriv").arg("--help").output().is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("pdeathsig"));
    let mut cmd = if tied {
        let mut c = Command::new("setpriv");
        c.args(["--pdeathsig", "KILL"]).arg(&git_daemon);
        c
    } else {
        Command::new(&git_daemon)
    };
    let daemon = cmd.args(["--reuseaddr", "--export-all", &format!("--port={port}"), "--listen=127.0.0.1"]).arg(format!("--base-path={}", gh.display())).arg(&gh).spawn()?;
    let _daemon = Daemon(daemon);
    std::thread::sleep(Duration::from_millis(500));

    let ci = format!(
        "github = {{ app_id = 99, private_key_file = \"{}\", webhook_secret_file = \"{}\", api_url = \"http://127.0.0.1:{api}\", web_url = \"http://127.0.0.1:{port}\" }}",
        key.display(),
        secret.display()
    );
    let extra = format!("[[repos]]\nname = \"acme/app\"\nclone_url = \"git://127.0.0.1:{port}/acme/app.git\"\n\n[[repos]]\nname = \"acme/gha\"\nclone_url = \"git://127.0.0.1:{port}/acme/gha.git\"\n");
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

    // W03: the App's other events reach the workflows as GitHub's do, once per delivery: an issue
    // opened runs `on: issues` at the default branch; an issue edited, and the same delivery
    // again, don't.
    let deliver = |event: &str, id: &str, body: &Value| -> anyhow::Result<String> {
        let body = body.to_string();
        let sig = format!("sha256={}", kiln::util::hmac_hex(b"s3cret", body.as_bytes()));
        let mut conn = std::net::TcpStream::connect(("127.0.0.1", s.port))?;
        write!(conn, "POST /api/hooks/github/app HTTP/1.1\r\nHost: x\r\nX-GitHub-Event: {event}\r\nX-GitHub-Delivery: {id}\r\nX-Hub-Signature-256: {sig}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())?;
        let mut answer = String::new();
        conn.read_to_string(&mut answer)?;
        Ok(answer)
    };
    let issue = |action: &str| json!({ "action": action, "issue": { "number": 5, "title": "Broken", "state": "open", "user": { "login": "octocat" } }, "repository": { "full_name": "acme/app", "default_branch": "main" }, "sender": { "login": "octocat" } });
    for (id, action) in [("app-issue-1", "opened"), ("app-issue-1", "opened"), ("app-issue-2", "edited")] {
        let answer = deliver("issues", id, &issue(action))?;
        assert!(answer.starts_with("HTTP/1.1 202"), "{answer}");
    }
    let triage = || -> anyhow::Result<Vec<Value>> {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme%2Fapp", &s.admin, None)?;
        let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
        Ok(v.get("runs").and_then(Value::as_array).into_iter().flatten().filter(|r| r.get("event") == Some(&json!("issues"))).cloned().collect())
    };
    until(240, "the issue's run succeeds", || {
        let runs = triage()?;
        Ok((!runs.iter().any(|r| r.get("conclusion") == Some(&json!("success")))).then(|| format!("{runs:?}\n{}", s.log())))
    })?;
    let runs = triage()?;
    assert_eq!(runs.len(), 1, "one run for the opened issue, none for its redelivery or an edit: {runs:?}");
    let run = runs.first().cloned().unwrap_or_default();
    assert!(run.get("workflow").and_then(Value::as_str).is_some_and(|w| w.ends_with("triage.yml")) && run.get("branch") == Some(&json!("main")), "{run}");

    // W21-R06: a workflow saved in kiln's editor goes to GitHub through the App: a branch off the
    // default branch's head, written with a token that may write workflows, and its pull request.
    let changed = std::fs::read_to_string(work.join(".github/workflows/ci.yml"))?.replace("echo \"server: $GITHUB_SERVER_URL\"", "echo \"server: $GITHUB_SERVER_URL, again\"");
    let (st, body) = http(s.port, "POST", "/api/ci/repos/acme%2Fapp/workflows/.github/workflows/ci.yml/edit", &s.admin, Some(&json!({ "content": changed })))?;
    let saved: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    let branch = saved.get("branch").and_then(Value::as_str).unwrap_or_default().to_owned();
    assert!(
        st == 200 && saved.get("to") == Some(&json!("change")) && branch.starts_with("kiln/edit-ci-") && saved.pointer("/change/url") == Some(&json!("https://github.example/acme/app/pull/12")),
        "{st} {body}"
    );
    let sent = |what: &str| calls.lock().map(|c| c.iter().filter(|(m, p, _, _)| m == "POST" && p == what).map(|(_, _, _, b)| b.clone()).collect::<Vec<_>>()).unwrap_or_default();
    let tokens = sent("/app/installations/7/access_tokens");
    assert!(tokens.iter().any(|t| t.pointer("/permissions/workflows") == Some(&json!("write")) && t.pointer("/permissions/contents") == Some(&json!("write"))), "{tokens:?}");
    let commit = sent("/repos/acme/app/git/commits").first().cloned().unwrap_or_default();
    assert_eq!(commit.get("parents"), Some(&json!([sha])), "on the default branch's head: {commit}");
    let tree = sent("/repos/acme/app/git/trees").first().cloned().unwrap_or_default();
    assert_eq!((tree.get("base_tree"), tree.pointer("/tree/0/path")), (Some(&json!("tree0")), Some(&json!(".github/workflows/ci.yml"))), "{tree}");
    let made = sent("/repos/acme/app/git/refs").first().cloned().unwrap_or_default();
    assert_eq!((made.get("ref"), made.get("sha")), (Some(&json!(format!("refs/heads/{branch}"))), Some(&json!("c".repeat(40)))), "a new branch: {made}");
    let pull = sent("/repos/acme/app/pulls").first().cloned().unwrap_or_default();
    assert_eq!((pull.get("head"), pull.get("base")), (Some(&json!(branch)), Some(&json!("main"))), "{pull}");

    // W23-R03: acme/gha's CI is GitHub Actions'. Its runs come into kiln, logs, reruns, cancels
    // and dispatches go to GitHub.
    let (st, body) = http(s.port, "PUT", "/api/ci/repos/acme%2Fgha/providers", &s.admin, Some(&json!({ "providers": ["github-actions"] })))?;
    assert!(st == 200 && body.contains("github-actions"), "{st} {body}");
    let (st, body) = http(s.port, "PUT", "/api/ci/repos/acme%2Fgha/providers", &s.admin, Some(&json!({ "providers": ["gitlab-ci"] })))?;
    assert_eq!(st, 422, "only what the repository may have: {body}");
    let mut run_id = String::new();
    until(60, "GitHub Actions' run in kiln", || {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme%2Fgha", &s.admin, None)?;
        let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
        let runs = v.get("runs").and_then(Value::as_array).cloned().unwrap_or_default();
        let r = runs.iter().find(|r| r.get("external_number") == Some(&json!(7))).cloned();
        if let Some(r) = &r {
            assert_eq!((r.get("provider"), r.get("status"), r.get("conclusion")), (Some(&json!("github-actions")), Some(&json!("failed")), Some(&json!("failure"))), "{r}");
            run_id = r.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
        }
        Ok(r.is_none().then_some(body))
    })?;
    let (_, detail) = http(s.port, "GET", &format!("/api/ci/runs/{run_id}"), &s.admin, None)?;
    let d: Value = serde_json::from_str(&detail).unwrap_or(Value::Null);
    let leg = d.get("legs").and_then(Value::as_array).and_then(|l| l.first()).cloned().unwrap_or_default();
    assert_eq!((leg.get("name").or_else(|| leg.get("key")), leg.get("status")), (Some(&json!("build")), Some(&json!("failed"))), "{detail}");
    let leg_id = leg.get("id").and_then(Value::as_str).unwrap_or_default();
    let (st, log) = http(s.port, "GET", &format!("/api/ci/legs/{leg_id}/logs"), &s.admin, None)?;
    assert!(st == 200 && log.contains("build failed: boom"), "{st} {log}");
    let saw = |what: &str| calls.lock().map(|c| c.iter().any(|(m, p, _, _)| m == "POST" && p.ends_with(what))).unwrap_or(false);
    let (st, body) = http(s.port, "POST", &format!("/api/ci/runs/{run_id}/rerun"), &s.admin, Some(&json!({ "mode": "failed" })))?;
    assert!(st == 201 && saw("/actions/runs/555/rerun-failed-jobs"), "{st} {body}");
    let (st, body) = http(s.port, "POST", &format!("/api/ci/runs/{run_id}/cancel"), &s.admin, Some(&json!({})))?;
    assert!(st == 202 && saw("/actions/runs/555/cancel"), "{st} {body}");
    let (st, body) = http(s.port, "POST", "/api/ci/repos/acme%2Fgha/workflows/.github/workflows/ci.yml/dispatches", &s.admin, Some(&json!({ "ref": "main", "inputs": { "x": "1" } })))?;
    assert!(st == 204 && saw("/actions/workflows/ci.yml/dispatches"), "{st} {body}");
    let sent = calls.lock().map(|c| c.iter().find(|(_, p, _, _)| p.ends_with("/dispatches")).map(|(_, _, _, b)| b.clone()).unwrap_or_default()).unwrap_or_default();
    assert_eq!((sent.get("ref"), sent.pointer("/inputs/x")), (Some(&json!("main")), Some(&json!("1"))), "{sent}");
    // A job's event brings its run at once.
    let body = json!({ "action": "in_progress", "workflow_job": { "id": 9002, "run_id": 556 }, "repository": { "full_name": "acme/gha" } }).to_string();
    let sig = format!("sha256={}", kiln::util::hmac_hex(b"s3cret", body.as_bytes()));
    let mut conn = std::net::TcpStream::connect(("127.0.0.1", s.port))?;
    write!(conn, "POST /api/hooks/github/app HTTP/1.1\r\nHost: x\r\nX-GitHub-Event: workflow_job\r\nX-Hub-Signature-256: {sig}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())?;
    let mut answer = String::new();
    conn.read_to_string(&mut answer)?;
    assert!(answer.starts_with("HTTP/1.1 202"), "{answer}");
    until(15, "run 556 from its job's event", || {
        let (_, body) = http(s.port, "GET", "/api/ci/runs?repo=acme%2Fgha", &s.admin, None)?;
        Ok((!(body.contains("\"external_number\":8") && body.contains("\"running\""))).then_some(body))
    })?;
    Ok(())
}
