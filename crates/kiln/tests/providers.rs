//! One contract for every CI provider (docs/ci.md W23-AC01): against fakes of GitHub's, GitLab's
//! and Bitbucket's APIs, built from their documented shapes, the same kiln calls list a run, follow
//! it (its jobs and a job's log), rerun it and cancel it, whichever provider it's on. Beside them:
//! GitHub's and GitLab's artifacts and deployment approvals, Bitbucket's environments, a dispatch on
//! each, a commit's checks from every provider, and GitLab's and Bitbucket's signed webhooks.

#![cfg(unix)]

mod common;

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use common::{Options, http, start};
use serde_json::{Value, json};

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

/// A request a fake was sent.
struct Req {
    method: String,
    /// With its query, as sent (so `acme%2Fgl` stays encoded).
    path: String,
    auth: String,
    body: Value,
}

/// What a fake answers: a status, a body (JSON, or text as a JSON string), and where it redirects.
type Answer = (u16, Value, Option<String>);

/// What a fake was asked: (method, path, body).
type Calls = Arc<Mutex<Vec<(String, String, Value)>>>;

/// A fake API on a port of its own, answering with `answer(request, its port)`.
fn fake(answer: impl Fn(&Req, u16) -> Answer + Send + 'static) -> anyhow::Result<(u16, Calls)> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    let calls: Calls = Arc::default();
    let seen = calls.clone();
    std::thread::spawn(move || {
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
            let req = Req { method, path, auth, body: serde_json::from_slice(&body).unwrap_or(Value::Null) };
            let (status, reply, location) = answer(&req, port);
            if let Ok(mut c) = seen.lock() {
                c.push((req.method.clone(), req.path.clone(), req.body.clone()));
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

fn asked(calls: &Calls, method: &str, path: &str) -> Option<Value> {
    calls.lock().ok()?.iter().rev().find(|(m, p, _)| m == method && p.split('?').next() == Some(path)).map(|(_, _, b)| b.clone())
}

const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

fn gh_pull(port: u16) -> Value {
    json!({ "number": 5, "title": "Faster builds", "body": "It's faster.", "user": { "login": "ada" }, "state": "open", "draft": false, "merged_at": null,
            "head": { "ref": "feat", "sha": SHA }, "base": { "ref": "main", "sha": "b".repeat(40) }, "html_url": format!("http://127.0.0.1:{port}/acme/gh/pull/5"), "updated_at": "2026-10-18T12:00:00Z", "mergeable": true })
}

/// GitHub: the App's installation and token, and Actions.
fn github(req: &Req, port: u16) -> Answer {
    let app = req.auth.starts_with("Bearer ey");
    let token = req.auth == "Bearer ghs_t";
    let run = json!({ "id": 700, "name": "ci", "path": ".github/workflows/ci.yml", "display_title": "on GitHub", "event": "push", "head_branch": "main", "head_sha": SHA,
                      "status": "completed", "conclusion": "failure", "run_number": 3, "run_attempt": 1, "html_url": format!("http://127.0.0.1:{port}/acme/gh/actions/runs/700"),
                      "created_at": "2026-10-18T12:00:00Z", "run_started_at": "2026-10-18T12:00:05Z", "updated_at": "2026-10-18T12:03:00Z", "actor": { "login": "ada" } });
    let path = req.path.split('?').next().unwrap_or_default();
    match (req.method.as_str(), path) {
        ("GET", "/repos/acme/gh/installation") if app => (200, json!({ "id": 7 }), None),
        ("POST", "/app/installations/7/access_tokens") if app => (201, json!({ "token": "ghs_t", "expires_at": "2099-01-01T00:00:00Z" }), None),
        ("GET", "/repos/acme/gh/actions/runs") if token => (200, json!({ "total_count": 1, "workflow_runs": [run] }), None),
        ("GET", "/repos/acme/gh/actions/runs/700") if token => (200, run, None),
        ("GET", "/repos/acme/gh/actions/runs/700/jobs") if token => (
            200,
            json!({ "jobs": [{ "id": 7001, "name": "build", "status": "completed", "conclusion": "failure", "runner_name": "gh-1", "labels": ["ubuntu-latest"], "html_url": "u",
                               "started_at": "2026-10-18T12:00:06Z", "completed_at": "2026-10-18T12:02:00Z", "steps": [{ "name": "test", "status": "completed", "conclusion": "failure", "number": 1 }] }] }),
            None,
        ),
        ("GET", "/repos/acme/gh/actions/jobs/7001/logs") if token => (302, Value::Null, Some(format!("http://127.0.0.1:{port}/blob/7001"))),
        ("GET", "/blob/7001") if req.auth.is_empty() => (200, json!("github: build failed"), None),
        ("POST", "/repos/acme/gh/actions/runs/700/rerun-failed-jobs") if token => (201, json!({}), None),
        ("POST", "/repos/acme/gh/actions/runs/700/cancel") if token => (202, json!({}), None),
        ("GET", "/repos/acme/gh/actions/runs/700/artifacts") if token => (
            200,
            json!({ "total_count": 1, "artifacts": [{ "id": 71, "name": "dist", "size_in_bytes": 4, "expired": false, "created_at": "2026-10-18T12:02:00Z", "expires_at": "2099-01-01T00:00:00Z" }] }),
            None,
        ),
        ("GET", "/repos/acme/gh/actions/artifacts/71/zip") if token => (302, Value::Null, Some(format!("http://127.0.0.1:{port}/blob/a71"))),
        ("GET", "/blob/a71") if req.auth.is_empty() => (200, json!("PKgh"), None),
        ("GET", "/repos/acme/gh/actions/runs/700/pending_deployments") if token => (200, json!([{ "environment": { "id": 5, "name": "prod" }, "wait_timer": 0, "current_user_can_approve": true, "reviewers": [] }]), None),
        ("POST", "/repos/acme/gh/actions/runs/700/pending_deployments") if token => (200, json!([{ "id": 1, "environment": "prod" }]), None),
        ("POST", "/repos/acme/gh/actions/workflows/ci.yml/dispatches") if token => (204, Value::Null, None),
        // Pull requests (W23-R06).
        ("GET", "/repos/acme/gh/pulls/5") if token => (200, gh_pull(port), None),
        ("GET", "/repos/acme/gh/pulls") if token => (200, json!([gh_pull(port)]), None),
        ("GET", "/repos/acme/gh/pulls/5/files") if token => (200, json!([{ "filename": "src/a.rs", "status": "modified", "additions": 1, "deletions": 1, "patch": "@@ -1 +1 @@\n-a\n+b" }]), None),
        ("GET", "/repos/acme/gh/issues/5/comments") if token => (200, json!([{ "id": 11, "user": { "login": "ada" }, "body": "looks good", "created_at": "2026-10-18T12:00:00Z" }]), None),
        ("GET", "/repos/acme/gh/pulls/5/comments") if token => (
            200,
            json!([{ "id": 21, "path": "src/a.rs", "line": 1, "side": "RIGHT", "user": { "login": "bob" }, "body": "why?", "created_at": "2026-10-18T12:01:00Z" },
                   { "id": 22, "in_reply_to_id": 21, "path": "src/a.rs", "line": 1, "side": "RIGHT", "user": { "login": "ada" }, "body": "because", "created_at": "2026-10-18T12:02:00Z" }]),
            None,
        ),
        ("GET", "/repos/acme/gh/pulls/5/reviews") if token => (200, json!([{ "user": { "login": "bob" }, "state": "APPROVED", "body": "", "submitted_at": "2026-10-18T12:03:00Z" }]), None),
        ("GET", "/repos/acme/gh") if token => (200, json!({ "allow_merge_commit": true, "allow_squash_merge": true, "allow_rebase_merge": false }), None),
        ("POST", "/repos/acme/gh/pulls/5/comments") if token => (201, json!({ "id": 23 }), None),
        ("POST", "/repos/acme/gh/pulls/5/reviews") if token => (200, json!({ "id": 31 }), None),
        ("PUT", "/repos/acme/gh/pulls/5/merge") if token => (200, json!({ "sha": "f".repeat(40), "merged": true }), None),
        _ => (404, json!({ "message": "Not Found" }), None),
    }
}

/// GitLab's API (v4).
fn gitlab(req: &Req, port: u16) -> Answer {
    if req.auth != "Bearer glpat-t" {
        return (401, json!({ "message": "401 Unauthorized" }), None);
    }
    let web = format!("http://127.0.0.1:{port}");
    let pipeline = |id: u64, status: &str| {
        json!({ "id": id, "iid": id - 54, "status": status, "source": "push", "ref": "main", "sha": SHA, "web_url": format!("{web}/acme/gl/-/pipelines/{id}"), "user": { "username": "grace" },
                "created_at": "2026-10-18T12:00:00.000Z", "started_at": "2026-10-18T12:00:04.000Z", "finished_at": if status == "failed" { json!("2026-10-18T12:02:00.000Z") } else { Value::Null } })
    };
    let p = "/api/v4/projects/acme%2Fgl";
    let path = req.path.split('?').next().unwrap_or_default();
    let rest = path.strip_prefix(p).unwrap_or("-");
    match (req.method.as_str(), rest) {
        ("GET", "/pipelines") => (200, json!([{ "id": 61, "iid": 7, "status": "failed", "source": "push", "ref": "main", "sha": SHA, "web_url": format!("{web}/acme/gl/-/pipelines/61") }]), None),
        ("GET", "/pipelines/61") => (200, pipeline(61, "failed"), None),
        ("GET", "/pipelines/62") => (200, pipeline(62, "running"), None),
        ("GET", "/pipelines/61/jobs") => (
            200,
            json!([{ "id": 8, "name": "test", "stage": "test", "status": "failed", "tag_list": ["docker"], "runner": { "description": "gl-runner" }, "web_url": format!("{web}/acme/gl/-/jobs/8"),
                     "started_at": "2026-10-18T12:00:05Z", "finished_at": "2026-10-18T12:01:30Z", "artifacts_file": { "filename": "artifacts.zip", "size": 4 }, "artifacts_expire_at": "2099-01-01T00:00:00Z" },
                   { "id": 9, "name": "deploy", "stage": "deploy", "status": "manual", "tag_list": [] }]),
            None,
        ),
        ("GET", "/pipelines/62/jobs") => (200, json!([]), None),
        ("GET", "/jobs/8/trace") => (200, json!("gitlab: test failed"), None),
        ("GET", "/jobs/8/artifacts") => (200, json!("PKgl"), None),
        ("POST", "/pipelines/61/retry") => (201, pipeline(61, "running"), None),
        ("POST", "/pipelines/61/cancel") => (200, pipeline(61, "canceled"), None),
        ("POST", "/pipeline") => (201, pipeline(62, "created"), None),
        ("GET", "/deployments") if req.path.contains("status=blocked") => {
            (200, json!([{ "id": 31, "status": "blocked", "environment": { "name": "prod" }, "deployable": { "id": 9, "name": "deploy", "pipeline": { "id": 61 } }, "pending_approval_count": 1 }]), None)
        }
        ("POST", "/deployments/31/approval") => (201, json!({ "status": "approved" }), None),
        ("GET", "/ci/lint") => (200, json!({ "valid": false, "errors": ["jobs:test script can't be blank"], "warnings": [] }), None),
        // Merge requests (W23-R06).
        ("GET", "/merge_requests/3") => (
            200,
            json!({ "iid": 3, "title": "Tidy", "description": "", "author": { "username": "grace" }, "state": "opened", "source_branch": "feat", "target_branch": "main", "sha": SHA,
                    "diff_refs": { "base_sha": "b".repeat(40), "start_sha": "b".repeat(40), "head_sha": SHA }, "web_url": format!("{web}/acme/gl/-/merge_requests/3"), "detailed_merge_status": "mergeable" }),
            None,
        ),
        ("GET", "/merge_requests/3/diffs") => (200, json!([{ "old_path": "a.txt", "new_path": "a.txt", "diff": "@@ -1 +1 @@\n-x\n+y\n" }]), None),
        ("GET", "/merge_requests/3/discussions") => (
            200,
            json!([{ "id": "d1", "individual_note": false, "notes": [{ "id": 41, "body": "nit", "author": { "username": "linus" }, "position": { "new_path": "a.txt", "new_line": 1 }, "resolved": false }] },
                   { "id": "d2", "individual_note": true, "notes": [{ "id": 42, "body": "thanks", "author": { "username": "grace" } }] }]),
            None,
        ),
        ("GET", "/merge_requests/3/approvals") => (200, json!({ "approved_by": [{ "user": { "username": "linus" } }] }), None),
        ("GET", "") => (200, json!({ "merge_method": "merge", "squash_option": "default_off" }), None),
        ("POST", "/merge_requests/3/discussions") => (201, json!({ "id": "d3" }), None),
        ("POST", "/merge_requests/3/approve") => (201, json!({}), None),
        ("PUT", "/merge_requests/3/merge") => (200, json!({ "state": "merged", "merge_commit_sha": null, "squash_commit_sha": "e".repeat(40) }), None),
        _ => (404, json!({ "message": "404 Not Found" }), None),
    }
}

/// Bitbucket Cloud's API (2.0).
fn bitbucket(req: &Req, port: u16) -> Answer {
    if req.auth != "Bearer bb-t" {
        return (401, json!({ "type": "error", "error": { "message": "Unauthorized" } }), None);
    }
    let _ = port;
    let pipeline = |uuid: &str, number: u64, state: Value| {
        json!({ "uuid": uuid, "build_number": number, "run_number": 1, "created_on": "2026-10-18T12:00:00Z", "completed_on": if state.get("name") == Some(&json!("COMPLETED")) { json!("2026-10-18T12:04:00Z") } else { Value::Null },
                "state": state, "trigger": { "name": "PUSH" }, "creator": { "nickname": "linus" },
                "target": { "type": "pipeline_ref_target", "ref_type": "branch", "ref_name": "main", "commit": { "type": "commit", "hash": SHA }, "selector": { "type": "branches", "pattern": "main" } } })
    };
    let p = "/2.0/repositories/acme/bb";
    let path = req.path.split('?').next().unwrap_or_default();
    let rest = path.strip_prefix(p).unwrap_or("-");
    match (req.method.as_str(), rest) {
        ("GET", "/pipelines") => (200, json!({ "values": [pipeline("{9f}", 12, json!({ "name": "IN_PROGRESS", "stage": { "name": "RUNNING" } }))] }), None),
        ("GET", "/pipelines/%7B9f%7D") => (200, pipeline("{9f}", 12, json!({ "name": "IN_PROGRESS", "stage": { "name": "RUNNING" } })), None),
        ("GET", "/pipelines/%7Ba0%7D") => (200, pipeline("{a0}", 13, json!({ "name": "PENDING" })), None),
        ("GET", "/pipelines/%7B9f%7D/steps") => (
            200,
            json!({ "values": [{ "uuid": "{s1}", "name": "Build", "state": { "name": "IN_PROGRESS" }, "started_on": "2026-10-18T12:00:10Z", "image": { "name": "node:22" } }] }),
            None,
        ),
        ("GET", "/pipelines/%7Ba0%7D/steps") => (200, json!({ "values": [] }), None),
        ("GET", "/pipelines/%7B9f%7D/steps/%7Bs1%7D/log") => (200, json!("bitbucket: building"), None),
        ("POST", "/pipelines") => (201, pipeline("{a0}", 13, json!({ "name": "PENDING" })), None),
        ("POST", "/pipelines/%7B9f%7D/stopPipeline") => (204, Value::Null, None),
        ("GET", "/environments") => (200, json!({ "values": [{ "uuid": "{e1}", "name": "Production", "environment_type": { "name": "Production" } }] }), None),
        ("GET", "/deployments") => (
            200,
            json!({ "values": [{ "uuid": "{d1}", "environment": { "uuid": "{e1}" }, "state": { "name": "COMPLETED", "status": { "name": "SUCCESSFUL" }, "started_on": "2026-10-18T11:00:00Z" },
                                "release": { "commit": { "hash": SHA }, "pipeline": { "uuid": "{8e}" }, "url": "u" } }] }),
            None,
        ),
        // Pull requests (W23-R06).
        ("GET", "/pullrequests/9") => (
            200,
            json!({ "id": 9, "title": "Docs", "description": "", "author": { "nickname": "linus" }, "state": "OPEN", "source": { "branch": { "name": "feat" }, "commit": { "hash": "abc" } },
                    "destination": { "branch": { "name": "main" }, "commit": { "hash": "def" } }, "links": { "html": { "href": "u" } }, "participants": [{ "user": { "nickname": "ada" }, "approved": true, "state": "approved" }] }),
            None,
        ),
        ("GET", "/pullrequests/9/diff") => (200, json!("diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-x\n+y\n"), None),
        ("GET", "/pullrequests/9/comments") => (
            200,
            json!({ "values": [{ "id": 51, "content": { "raw": "hm" }, "user": { "nickname": "ada" }, "inline": { "path": "a.txt", "to": 1 } }, { "id": 52, "parent": { "id": 51 }, "content": { "raw": "ok" }, "user": { "nickname": "linus" } }] }),
            None,
        ),
        ("POST", "/pullrequests/9/request-changes") => (200, json!({}), None),
        ("POST", "/pullrequests/9/comments") => (201, json!({ "id": 53 }), None),
        ("POST", "/pullrequests/9/merge") => (200, json!({ "state": "MERGED", "merge_commit": { "hash": "abcdef123456" } }), None),
        _ => (404, json!({ "type": "error", "error": { "message": "Not found" } }), None),
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

/// The repository's one run from `provider`, once kiln has it, and its first leg.
fn the_run(port: u16, admin: &str, repo: &str, provider: &str) -> anyhow::Result<(Value, Value)> {
    let mut found = (Value::Null, Value::Null);
    until(30, &format!("{repo}'s {provider} run"), || {
        let (_, body) = http(port, "GET", &format!("/api/ci/runs?repo={repo}"), admin, None)?;
        let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
        let Some(run) = v.get("runs").and_then(Value::as_array).and_then(|l| l.iter().find(|r| r.get("provider") == Some(&json!(provider)))).cloned() else { return Ok(Some(body)) };
        let id = run.get("id").and_then(Value::as_str).unwrap_or_default();
        let (_, one) = http(port, "GET", &format!("/api/ci/runs/{id}"), admin, None)?;
        let one: Value = serde_json::from_str(&one).unwrap_or(Value::Null);
        match one.pointer("/legs/0") {
            Some(leg) => {
                found = (run, leg.clone());
                Ok(None)
            }
            None => Ok(Some(one.to_string())),
        }
    })?;
    Ok(found)
}

fn s<'a>(v: &'a Value, p: &str) -> &'a str {
    v.pointer(p).and_then(Value::as_str).unwrap_or_default()
}

#[test]
fn w23_ac01_contract() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let d = dir.path().display().to_string();
    let kp = aws_lc_rs::rsa::KeyPair::generate(aws_lc_rs::rsa::KeySize::Rsa2048).map_err(|_| anyhow::anyhow!("keygen"))?;
    let der = aws_lc_rs::encoding::AsDer::<aws_lc_rs::encoding::Pkcs8V1Der>::as_der(&kp).map_err(|_| anyhow::anyhow!("der"))?;
    std::fs::write(dir.path().join("app.pem"), format!("-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n", B64.encode(der.as_ref())))?;
    for (f, v) in [("gl-token", "glpat-t"), ("gl-hook", "gl-s3cret"), ("bb-token", "bb-t"), ("bb-hook", "bb-s3cret")] {
        std::fs::write(dir.path().join(f), format!("{v}\n"))?;
    }
    let (gh, gh_calls) = fake(github)?;
    let (gl, gl_calls) = fake(gitlab)?;
    let (bb, bb_calls) = fake(bitbucket)?;
    let ci = format!("github = {{ app_id = 99, private_key_file = \"{d}/app.pem\", api_url = \"http://127.0.0.1:{gh}\", web_url = \"http://127.0.0.1:{gh}\" }}");
    let extra = format!(
        r#"[[ci.gitlab]]
url = "http://127.0.0.1:{gl}"
token_file = "{d}/gl-token"
webhook_secret_file = "{d}/gl-hook"

[ci.bitbucket]
web_url = "http://127.0.0.1:{bb}"
api_url = "http://127.0.0.1:{bb}/2.0"
token_file = "{d}/bb-token"
webhook_secret_file = "{d}/bb-hook"

[[repos]]
name = "acme/gh"
clone_url = "http://127.0.0.1:{gh}/acme/gh.git"

[[repos]]
name = "acme/gl"
clone_url = "http://127.0.0.1:{gl}/acme/gl.git"

[[repos]]
name = "acme/bb"
clone_url = "http://127.0.0.1:{bb}/acme/bb.git"
"#
    );
    let srv = start(dir.path(), &Options { ci: &ci, extra: &extra, ..Options::default() })?;
    let (port, admin) = (srv.port, srv.admin.as_str());
    let call = |method: &str, path: &str, body: Option<&Value>| -> anyhow::Result<(u16, Value)> {
        let (st, b) = http(port, method, path, admin, body)?;
        Ok((st, serde_json::from_str(&b).unwrap_or(Value::String(b))))
    };

    // Each repository's CI on its host's: kiln offers what kiln reaches.
    for (repo, provider) in [("acme/gh", "github-actions"), ("acme/gl", "gitlab-ci"), ("acme/bb", "bitbucket-pipelines")] {
        let (st, v) = call("GET", &format!("/api/ci/repos/{}/providers", repo.replace('/', "%2F")), None)?;
        assert!(st == 200 && v.get("available").is_some_and(|a| a.to_string().contains(provider)), "{repo}: {st} {v}");
        let (st, v) = call("PUT", &format!("/api/ci/repos/{}/providers", repo.replace('/', "%2F")), Some(&json!({ "providers": [provider] })))?;
        assert_eq!(st, 200, "{repo}: {v}");
    }
    let (st, v) = call("PUT", "/api/ci/repos/acme%2Fgl/providers", Some(&json!({ "providers": ["bitbucket-pipelines"] })))?;
    assert_eq!(st, 422, "a provider the repository can't have: {v}");

    // GitHub and GitLab: looked at now; Bitbucket: its signed webhook says a build status changed.
    for repo in ["acme/gh", "acme/gl"] {
        let (st, v) = call("POST", &format!("/api/ci/repos/{}/providers/sync", repo.replace('/', "%2F")), Some(&json!({})))?;
        assert_eq!(st, 200, "{repo}: {v}\n{}", srv.log());
    }
    let hook = json!({ "repository": { "full_name": "acme/bb", "links": { "html": { "href": format!("http://127.0.0.1:{bb}/acme/bb") } } }, "commit_status": { "state": "INPROGRESS", "key": "pipelines" } }).to_string();
    let key = aws_lc_rs::hmac::Key::new(aws_lc_rs::hmac::HMAC_SHA256, b"bb-s3cret");
    let sig: String = aws_lc_rs::hmac::sign(&key, hook.as_bytes()).as_ref().iter().map(|b| format!("{b:02x}")).collect();
    let deliver = |path: &str, headers: &[(&str, String)], body: &str| -> anyhow::Result<u16> {
        let mut c = std::net::TcpStream::connect(("127.0.0.1", port))?;
        let h: String = headers.iter().map(|(k, v)| format!("{k}: {v}\r\n")).collect();
        write!(c, "POST {path} HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\n{h}Content-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())?;
        let mut raw = String::new();
        c.read_to_string(&mut raw)?;
        Ok(raw.split(' ').nth(1).and_then(|s| s.parse().ok()).unwrap_or(0))
    };
    assert_eq!(deliver("/api/hooks/bitbucket", &[("X-Event-Key", "repo:commit_status_updated".into()), ("X-Hub-Signature", "sha256=00".into())], &hook)?, 401, "a wrong signature");
    assert_eq!(deliver("/api/hooks/bitbucket", &[("X-Event-Key", "repo:commit_status_updated".into()), ("X-Hub-Signature", format!("sha256={sig}"))], &hook)?, 202);

    // The same calls, whichever provider: list, follow (jobs, a log), rerun, cancel.
    let cases = [
        ("acme/gh", "github-actions", "failed", "github: build failed", "/repos/acme/gh/actions/runs/700/rerun-failed-jobs", "/repos/acme/gh/actions/runs/700/cancel", &gh_calls),
        ("acme/gl", "gitlab-ci", "failed", "gitlab: test failed", "/api/v4/projects/acme%2Fgl/pipelines/61/retry", "/api/v4/projects/acme%2Fgl/pipelines/61/cancel", &gl_calls),
        ("acme/bb", "bitbucket-pipelines", "running", "bitbucket: building", "/2.0/repositories/acme/bb/pipelines", "/2.0/repositories/acme/bb/pipelines/%7B9f%7D/stopPipeline", &bb_calls),
    ];
    let mut ids = std::collections::HashMap::new();
    for (repo, provider, status, log, rerun_path, cancel_path, calls) in cases {
        let (run, leg) = the_run(port, admin, repo, provider)?;
        ids.insert(repo, s(&run, "/id").to_owned());
        assert_eq!((s(&run, "/status"), s(&run, "/sha")), (status, SHA), "{repo}: {run}");
        assert!(s(&run, "/external_url").starts_with("http"), "{repo}: a link back: {run}");
        let id = s(&run, "/id").to_owned();
        let (st, text) = http(port, "GET", &format!("/api/ci/legs/{}/logs", s(&leg, "/id")), admin, None)?;
        assert!(st == 200 && text.contains(log), "{repo}: {st} {text}");
        let mode = if provider == "bitbucket-pipelines" { "all" } else { "failed" };
        let (st, v) = call("POST", &format!("/api/ci/runs/{id}/rerun"), Some(&json!({ "mode": mode })))?;
        assert!(st == 201 && asked(calls, "POST", rerun_path).is_some(), "{repo}: rerun {st} {v}");
        let (st, v) = call("POST", &format!("/api/ci/runs/{id}/cancel"), None)?;
        assert!(st == 202 && asked(calls, "POST", cancel_path).is_some(), "{repo}: cancel {st} {v}");
    }
    // Bitbucket's rerun is a new pipeline on the same commit and selector.
    let again = asked(&bb_calls, "POST", "/2.0/repositories/acme/bb/pipelines").unwrap_or_default();
    assert_eq!((s(&again, "/target/commit/hash"), s(&again, "/target/selector/pattern")), (SHA, "main"), "{again}");
    let bb_run = ids.get("acme/bb").cloned().unwrap_or_default();
    let (st, v) = call("POST", &format!("/api/ci/runs/{bb_run}/rerun"), Some(&json!({ "mode": "failed" })))?;
    assert!(st == 502 && v.to_string().contains("whole pipelines"), "Bitbucket's API has no rerun of failed steps: {st} {v}");

    // Artifacts and approvals, where the provider has them.
    for (repo, archive) in [("acme/gh", "PKgh"), ("acme/gl", "PKgl")] {
        let id = ids.get(repo).cloned().unwrap_or_default();
        let (st, v) = call("GET", &format!("/api/ci/runs/{id}/artifacts"), None)?;
        let art = v.pointer("/artifacts/0").cloned().unwrap_or_default();
        assert!(st == 200 && art.get("id").is_some(), "{repo}: {st} {v}");
        let aid = art.get("id").map(|i| i.as_str().map_or_else(|| i.to_string(), str::to_owned)).unwrap_or_default();
        let (st, bytes) = http(port, "GET", &format!("/api/ci/runs/{id}/artifacts/{aid}/download"), admin, None)?;
        assert!(st == 200 && bytes.contains(archive), "{repo}: {st} {bytes}");
        let (st, v) = call("GET", &format!("/api/ci/runs/{id}/pending"), None)?;
        assert!(st == 200 && s(&v, "/pending/0/environment") == "prod", "{repo}: {st} {v}");
        let (st, v) = call("POST", &format!("/api/ci/runs/{id}/approve"), Some(&json!({ "state": "approved", "environments": ["prod"], "comment": "ship it" })))?;
        assert_eq!(st, 200, "{repo}: {v}");
    }
    let gh_review = asked(&gh_calls, "POST", "/repos/acme/gh/actions/runs/700/pending_deployments").unwrap_or_default();
    assert_eq!((gh_review.get("environment_ids"), s(&gh_review, "/state"), s(&gh_review, "/comment")), (Some(&json!([5])), "approved", "ship it"), "{gh_review}");
    let gl_review = asked(&gl_calls, "POST", "/api/v4/projects/acme%2Fgl/deployments/31/approval").unwrap_or_default();
    assert_eq!((s(&gl_review, "/status"), s(&gl_review, "/comment")), ("approved", "ship it"), "{gl_review}");

    // Dispatches: GitHub's workflow_dispatch, a GitLab pipeline with variables, a Bitbucket custom pipeline.
    for (repo, path) in [("acme/gh", ".github/workflows/ci.yml"), ("acme/gl", ".gitlab-ci.yml"), ("acme/bb", "nightly")] {
        let (st, v) = call("POST", &format!("/api/ci/repos/{}/workflows/{path}/dispatches", repo.replace('/', "%2F")), Some(&json!({ "ref": "main", "inputs": { "level": "full" } })))?;
        assert_eq!(st, 204, "{repo}: {v}");
    }
    let gh_d = asked(&gh_calls, "POST", "/repos/acme/gh/actions/workflows/ci.yml/dispatches").unwrap_or_default();
    assert_eq!((s(&gh_d, "/ref"), s(&gh_d, "/inputs/level")), ("main", "full"), "{gh_d}");
    let gl_d = asked(&gl_calls, "POST", "/api/v4/projects/acme%2Fgl/pipeline").unwrap_or_default();
    assert_eq!((s(&gl_d, "/ref"), s(&gl_d, "/variables/0/key"), s(&gl_d, "/variables/0/value")), ("main", "level", "full"), "{gl_d}");
    let bb_d = asked(&bb_calls, "POST", "/2.0/repositories/acme/bb/pipelines").unwrap_or_default();
    assert_eq!((s(&bb_d, "/target/selector/pattern"), s(&bb_d, "/variables/0/key")), ("nightly", "level"), "{bb_d}");

    // A commit's checks from every provider; GitLab's CI file checked by GitLab; Bitbucket's environments.
    let (st, v) = call("GET", &format!("/api/ci/repos/acme%2Fgl/commits/{SHA}/checks"), None)?;
    assert!(st == 200 && v.to_string().contains("gitlab-ci"), "{st} {v}");
    let (st, v) = call("GET", "/api/ci/repos/acme%2Fgl/providers/workflows", None)?;
    assert!(st == 200 && v.to_string().contains("script can't be blank"), "{st} {v}");
    let (st, v) = call("GET", "/api/ci/environments?repo=acme/bb", None)?;
    assert!(st == 200 && v.to_string().contains("Production") && v.to_string().contains("SUCCESSFUL"), "{st} {v}");

    // GitLab's webhook: its token, then the pipeline it names is kept at once.
    let pipeline_hook = json!({ "object_kind": "pipeline", "object_attributes": { "id": 62 }, "project": { "path_with_namespace": "acme/gl", "web_url": format!("http://127.0.0.1:{gl}/acme/gl") } }).to_string();
    assert_eq!(deliver("/api/hooks/gitlab", &[("X-Gitlab-Event", "Pipeline Hook".into()), ("X-Gitlab-Token", "wrong".into())], &pipeline_hook)?, 401);
    assert_eq!(deliver("/api/hooks/gitlab", &[("X-Gitlab-Event", "Pipeline Hook".into()), ("X-Gitlab-Token", "gl-s3cret".into())], &pipeline_hook)?, 202);
    until(20, "GitLab's pipeline 62 from its webhook", || {
        let (_, body) = http(port, "GET", "/api/ci/runs?repo=acme/gl", admin, None)?;
        Ok((!body.contains("pipelines/62")).then_some(body))
    })?;
    Ok(())
}

/// Pull requests on every host (docs/ci.md W23-R06): the same calls read a GitHub pull request, a
/// GitLab merge request and a Bitbucket pull request (the conversation, threads on lines, reviews,
/// files, merge methods), comment on a line, review, and merge by the host's methods.
#[test]
fn w23_r06_pulls() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let d = dir.path().display().to_string();
    let kp = aws_lc_rs::rsa::KeyPair::generate(aws_lc_rs::rsa::KeySize::Rsa2048).map_err(|_| anyhow::anyhow!("keygen"))?;
    let der = aws_lc_rs::encoding::AsDer::<aws_lc_rs::encoding::Pkcs8V1Der>::as_der(&kp).map_err(|_| anyhow::anyhow!("der"))?;
    std::fs::write(dir.path().join("app.pem"), format!("-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n", B64.encode(der.as_ref())))?;
    for (f, v) in [("gl-token", "glpat-t"), ("bb-token", "bb-t")] {
        std::fs::write(dir.path().join(f), format!("{v}\n"))?;
    }
    let (gh, gh_calls) = fake(github)?;
    let (gl, gl_calls) = fake(gitlab)?;
    let (bb, bb_calls) = fake(bitbucket)?;
    let ci = format!("github = {{ app_id = 99, private_key_file = \"{d}/app.pem\", api_url = \"http://127.0.0.1:{gh}\", web_url = \"http://127.0.0.1:{gh}\" }}");
    let extra = format!(
        "[[ci.gitlab]]\nurl = \"http://127.0.0.1:{gl}\"\ntoken_file = \"{d}/gl-token\"\n\n[ci.bitbucket]\nweb_url = \"http://127.0.0.1:{bb}\"\napi_url = \"http://127.0.0.1:{bb}/2.0\"\ntoken_file = \"{d}/bb-token\"\n\n\
         [[repos]]\nname = \"acme/gh\"\nclone_url = \"http://127.0.0.1:{gh}/acme/gh.git\"\n\n[[repos]]\nname = \"acme/gl\"\nclone_url = \"http://127.0.0.1:{gl}/acme/gl.git\"\n\n\
         [[repos]]\nname = \"acme/bb\"\nclone_url = \"http://127.0.0.1:{bb}/acme/bb.git\"\n"
    );
    let srv = start(dir.path(), &Options { ci: &ci, extra: &extra, ..Options::default() })?;
    let (port, admin) = (srv.port, srv.admin.as_str());
    let call = |method: &str, path: &str, body: Option<&Value>| -> anyhow::Result<(u16, Value)> {
        let (st, b) = http(port, method, path, admin, body)?;
        Ok((st, serde_json::from_str(&b).unwrap_or(Value::String(b))))
    };

    let (st, v) = call("GET", "/api/ci/repos/acme%2Fgh/pulls", None)?;
    assert!(st == 200 && s(&v, "/pulls/0/title") == "Faster builds", "{st} {v}\n{}", srv.log());
    // One shape, whichever host: (repo, number, a thread's line, its replies, a review's state, merge methods).
    for (repo, n, line, replies, review, methods) in [("acme%2Fgh", 5, 1, 2, "approved", json!(["merge", "squash"])), ("acme%2Fgl", 3, 1, 1, "approved", json!(["merge", "squash"])), ("acme%2Fbb", 9, 1, 2, "approved", json!(["merge", "squash", "fast-forward"]))] {
        let (st, v) = call("GET", &format!("/api/ci/repos/{repo}/pulls/{n}"), None)?;
        assert_eq!(st, 200, "{repo}: {v}");
        assert_eq!((v.pointer("/threads/0/line"), v.pointer("/threads/0/comments").and_then(Value::as_array).map(Vec::len)), (Some(&json!(line)), Some(replies)), "{repo}: {v}");
        assert_eq!((s(&v, "/reviews/0/state"), v.get("merge_methods")), (review, Some(&methods)), "{repo}: {v}");
        assert!(v.pointer("/files/0/patch").and_then(Value::as_str).is_some_and(|p| p.contains("@@")) && v.get("checks").is_some(), "{repo}: {v}");
    }
    assert!(call("GET", "/api/ci/repos/acme%2Fgh/pulls/5", None)?.1.pointer("/conversation/0/body") == Some(&json!("looks good")));
    assert!(call("GET", "/api/ci/repos/acme%2Fgl/pulls/3", None)?.1.pointer("/conversation/0/body") == Some(&json!("thanks")));

    // A comment on a line, a review, a merge: on each host, by its own calls.
    for (repo, n) in [("acme%2Fgh", 5), ("acme%2Fgl", 3), ("acme%2Fbb", 9)] {
        let (st, v) = call("POST", &format!("/api/ci/repos/{repo}/pulls/{n}/comments"), Some(&json!({ "body": "and here?", "path": "a.txt", "line": 1 })))?;
        assert!(st == 201 && s(&v, "/as") == "kiln", "{repo}: {st} {v}");
    }
    let gh_line = asked(&gh_calls, "POST", "/repos/acme/gh/pulls/5/comments").unwrap_or_default();
    assert_eq!((s(&gh_line, "/commit_id"), s(&gh_line, "/side"), gh_line.get("line")), (SHA, "RIGHT", Some(&json!(1))), "{gh_line}");
    let gl_line = asked(&gl_calls, "POST", "/api/v4/projects/acme%2Fgl/merge_requests/3/discussions").unwrap_or_default();
    assert_eq!((s(&gl_line, "/position/head_sha"), gl_line.pointer("/position/new_line")), (SHA, Some(&json!(1))), "{gl_line}");
    let bb_line = asked(&bb_calls, "POST", "/2.0/repositories/acme/bb/pullrequests/9/comments").unwrap_or_default();
    assert_eq!((s(&bb_line, "/inline/path"), bb_line.pointer("/inline/to")), ("a.txt", Some(&json!(1))), "{bb_line}");

    for (repo, n, verdict) in [("acme%2Fgh", 5, "approve"), ("acme%2Fgl", 3, "approve"), ("acme%2Fbb", 9, "request_changes")] {
        let (st, v) = call("POST", &format!("/api/ci/repos/{repo}/pulls/{n}/reviews"), Some(&json!({ "verdict": verdict, "body": "" })))?;
        assert_eq!(st, 200, "{repo}: {v}");
    }
    assert_eq!(s(&asked(&gh_calls, "POST", "/repos/acme/gh/pulls/5/reviews").unwrap_or_default(), "/event"), "APPROVE");
    assert!(asked(&gl_calls, "POST", "/api/v4/projects/acme%2Fgl/merge_requests/3/approve").is_some());
    assert!(asked(&bb_calls, "POST", "/2.0/repositories/acme/bb/pullrequests/9/request-changes").is_some());

    let (st, v) = call("POST", "/api/ci/repos/acme%2Fgh/pulls/5/merge", Some(&json!({ "method": "fast-forward" })))?;
    assert!(st == 422 && v.to_string().contains("GitHub merges by"), "GitHub has no fast-forward merge: {st} {v}");
    for (repo, n, method) in [("acme%2Fgh", 5, "squash"), ("acme%2Fgl", 3, "squash"), ("acme%2Fbb", 9, "fast-forward")] {
        let (st, v) = call("POST", &format!("/api/ci/repos/{repo}/pulls/{n}/merge"), Some(&json!({ "method": method })))?;
        assert!(st == 200 && v.get("merged") == Some(&json!(true)), "{repo}: {st} {v}");
    }
    assert_eq!(s(&asked(&gh_calls, "PUT", "/repos/acme/gh/pulls/5/merge").unwrap_or_default(), "/merge_method"), "squash");
    assert_eq!(asked(&gl_calls, "PUT", "/api/v4/projects/acme%2Fgl/merge_requests/3/merge").unwrap_or_default().get("squash"), Some(&json!(true)));
    assert_eq!(s(&asked(&bb_calls, "POST", "/2.0/repositories/acme/bb/pullrequests/9/merge").unwrap_or_default(), "/merge_strategy"), "fast_forward");
    Ok(())
}
