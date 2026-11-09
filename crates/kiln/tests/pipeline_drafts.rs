//! Pipelines' drafts (docs/ci.md W29-R10 to R14), against a server of its own with its own runner
//! and real `git`: a draft saved on a repository kiln hosts, and on GitHub through a fake of its
//! API and its git.

#![cfg(unix)]

mod common;

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use common::{Got, Options, Server, Service, http, start};
use serde_json::{Value, json};

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

/// `git` with no configuration but this test's, in `dir`.
fn git(home: &Path, dir: &Path, args: &[&str]) -> anyhow::Result<String> {
    let o = Command::new("git")
        .args(["-c", "user.name=Ada", "-c", "user.email=ada@example.com", "-c", "init.defaultBranch=main", "-c", "credential.helper=", "-c", "commit.gpgsign=false"])
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

fn post(s: &Server, path: &str, body: &Value) -> anyhow::Result<Value> {
    let (st, text) = http(s.port, "POST", path, &s.admin, Some(body))?;
    anyhow::ensure!((200..300).contains(&st), "POST {path}: {st} {text}");
    Ok(serde_json::from_str(&text)?)
}

fn get(s: &Server, path: &str) -> anyhow::Result<Value> {
    let (st, text) = http(s.port, "GET", path, &s.admin, None)?;
    anyhow::ensure!(st == 200, "GET {path}: {st} {text}");
    Ok(serde_json::from_str(&text)?)
}

fn at<'a>(v: &'a Value, pointer: &str) -> &'a str {
    v.pointer(pointer).and_then(Value::as_str).unwrap_or_default()
}

/// Until `name`'s first poll is done: it takes the branch as it is.
fn polled(s: &Server, name: &str) -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let v = get(s, "/api/repos")?;
        let list = v.as_array().cloned().or_else(|| v.get("repos").and_then(Value::as_array).cloned()).unwrap_or_default();
        if list.iter().any(|r| r.get("name").and_then(Value::as_str) == Some(name) && r.get("last_polled_at").is_some_and(|t| !t.is_null())) {
            return Ok(());
        }
        anyhow::ensure!(Instant::now() < deadline, "{name} wasn't polled: {v}\n{}", s.log());
        std::thread::sleep(Duration::from_millis(200));
    }
}

const CI: &str = "name: ci\non: push\njobs:\n  build:\n    runs-on: local\n    steps:\n      - run: echo one\n";
const TRY_ONE: &str = "name: ci\non: push\njobs:\n  build:\n    runs-on: local\n    strategy:\n      matrix:\n        n: [1, 2]\n    steps:\n      - run: echo ${{ matrix.n }}\n";
const TRY_TWO: &str = "name: ci\non: push\njobs:\n  build:\n    runs-on: local\n    strategy:\n      matrix:\n        n: [1, 2, 3]\n    steps:\n      - run: echo ${{ matrix.n }}\n";

/// W29-R11 on a repository kiln hosts: Save draft opens a draft diff, and saving again is its next
/// version, never a second diff; kiln's record of it says so.
#[test]
fn w29_r11_save_draft_on_kiln() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { agent: true, ..Options::default() })?;
    let home = dir.path().join("githome");
    std::fs::create_dir_all(&home)?;
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": "acme/app", "mode": "native" })))?;
    anyhow::ensure!(st == 200, "native repository: {st} {body}");
    git(&home, dir.path(), &["clone", "-q", &format!("http://x:{}@127.0.0.1:{}/git/acme/app.git", s.admin, s.port), "app"])?;
    let work = dir.path().join("app");
    std::fs::create_dir_all(work.join(".github/workflows"))?;
    std::fs::write(work.join(".github/workflows/ci.yml"), CI)?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "first"])?;
    git(&home, &work, &["push", "-q", "origin", "main"])?;
    polled(&s, "acme/app")?;

    let edit = "/api/ci/repos/acme%2Fapp/workflows/.github/workflows/ci.yml/edit";
    let first = post(&s, edit, &json!({ "content": TRY_ONE, "draft": true, "title": "Try a matrix" }))?;
    assert_eq!((at(&first, "/draft/kind"), at(&first, "/draft/state")), ("diff", "draft"), "{first}");
    let name = at(&first, "/draft/name").to_owned();
    assert!(name.starts_with('D'), "{first}");
    let second = post(&s, edit, &json!({ "content": TRY_TWO, "draft": true, "title": "Try a matrix" }))?;
    assert_eq!((at(&second, "/draft/id"), at(&second, "/draft/name")), (at(&first, "/draft/id"), name.as_str()), "the same draft: {second}");
    let diff = get(&s, &format!("/api/diffs/{name}"))?;
    assert_eq!(at(&diff, "/state"), "draft", "{diff}");
    assert_eq!(diff.get("versions").and_then(Value::as_array).map(Vec::len), Some(2), "saved twice, one diff of two versions: {diff}");
    // With no `state`, the list has every diff, a draft too.
    let all = get(&s, "/api/repos/acme%2Fapp/diffs")?;
    let diffs = all.get("diffs").and_then(Value::as_array).cloned().or_else(|| all.as_array().cloned()).unwrap_or_default();
    assert_eq!(diffs.len(), 1, "{all}");
    // A draft is a diff or a host's change, nothing else.
    let (st, _) = http(s.port, "POST", edit, &s.admin, Some(&json!({ "content": TRY_TWO, "draft": true, "to": "patch" })))?;
    assert_eq!(st, 400);
    Ok(())
}

/// GitHub's git over smart HTTP, as GitHub serves it: `git http-backend` as a CGI behind a server
/// of this test's, each request on a thread of its own. Its port.
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

/// One request, answered by `git http-backend`.
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
    let (path, query) = target.split_once('?').unwrap_or((target.as_str(), ""));
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

/// A server whose GitHub App (its API a fake of GitHub's: the App's tokens, the Git Data API and
/// pulls) reaches `acme/app`, whose git a server of this test's serves; polled once.
struct OnGithub {
    s: Server,
    api: Service,
    _dir: tempfile::TempDir,
}

fn on_github() -> anyhow::Result<OnGithub> {
    let dir = tempfile::tempdir()?;
    let kp = aws_lc_rs::rsa::KeyPair::generate(aws_lc_rs::rsa::KeySize::Rsa2048).map_err(|_| anyhow::anyhow!("keygen"))?;
    let der = aws_lc_rs::encoding::AsDer::<aws_lc_rs::encoding::Pkcs8V1Der>::as_der(&kp).map_err(|_| anyhow::anyhow!("der"))?;
    let key = dir.path().join("app.pem");
    std::fs::write(&key, format!("-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n", B64.encode(der.as_ref())))?;
    let secret = dir.path().join("webhook-secret");
    std::fs::write(&secret, "s3cret\n")?;
    let data = common::git_data::GitData::new(dir.path().join("githome"), dir.path().join("gh/acme/app.git"));
    let answer: Arc<common::Answer> = Arc::new(move |method: &str, got: &Got| {
        let path = got.path.split('?').next().unwrap_or_default();
        if let Some(result) = data.reply(method, path, &got.json()) {
            return match result { Ok((st, body)) => (st, if body.is_null() { String::new() } else { body.to_string() }), Err(e) => (500, json!({"message": format!("{e:#}")}).to_string()) };
        }
        let (st, v) = match (method, path) {
            ("GET", "/repos/acme/app/installation") => (200, json!({ "id": 7 })),
            ("POST", "/app/installations/7/access_tokens") => (201, json!({ "token": "ghs_fake_drafts", "expires_at": "2099-01-01T00:00:00Z" })),
            ("POST", "/repos/acme/app/pulls") => (201, json!({ "number": 12, "html_url": "https://github.example/acme/app/pull/12" })),
            ("GET", "/repos/acme/app/pulls/12") => (200, json!({ "number": 12, "draft": true, "node_id": "PR_kwDOdraft12" })),
            ("POST", "/graphql") => (200, json!({ "data": { "markPullRequestReadyForReview": { "pullRequest": { "isDraft": false } } } })),
            ("POST", "/repos/acme/app/issues/12/comments") => (201, json!({ "id": 1 })),
            ("PATCH", "/repos/acme/app/pulls/12") => (200, json!({ "number": 12, "state": "closed" })),
            _ => (404, json!({ "message": "Not Found" })),
        };
        (st, if v.is_null() { String::new() } else { v.to_string() })
    });
    let api = Service::start(answer)?;
    // GitHub's git: a bare repository with a workflow, served over smart HTTP.
    let gh = dir.path().join("gh");
    std::fs::create_dir_all(gh.join("acme"))?;
    let home = dir.path().join("githome");
    std::fs::create_dir_all(&home)?;
    git(&home, &gh, &["init", "-q", "--bare", "acme/app.git"])?;
    let work = dir.path().join("work");
    std::fs::create_dir_all(work.join(".github/workflows"))?;
    std::fs::write(work.join(".github/workflows/ci.yml"), CI)?;
    git(&home, &work, &["init", "-q"])?;
    git(&home, &work, &["add", "."])?;
    git(&home, &work, &["commit", "-qm", "first"])?;
    git(&home, &work, &["push", "-q", &gh.join("acme/app.git").to_string_lossy(), "HEAD:main"])?;
    let port = git_http(&gh)?;
    let ci = format!("github = {{ app_id = 99, private_key_file = \"{}\", webhook_secret_file = \"{}\", api_url = \"{}\", web_url = \"http://127.0.0.1:{port}\" }}", key.display(), secret.display(), api.url());
    let extra = format!("[[repos]]\nname = \"acme/app\"\nclone_url = \"http://127.0.0.1:{port}/acme/app.git\"\n");
    let s = start(dir.path(), &Options { agent: true, ci: &ci, extra: &extra })?;
    polled(&s, "acme/app")?;
    Ok(OnGithub { s, api, _dir: dir })
}

impl OnGithub {
    /// The bodies of the requests sent as `method` to `path` (its query left out), in order.
    fn sent(&self, method: &str, path: &str) -> Vec<Value> {
        self.api.requests().into_iter().filter(|(m, g)| m == method && g.path.split('?').next() == Some(path)).map(|(_, g)| g.json()).collect()
    }
}

/// W29-R11 on GitHub: Save draft makes a branch of the draft's own, commits to it saying
/// `[skip ci]`, and opens a draft pull request; saving again is a new commit on that branch, on
/// what was saved before, and no second pull request.
#[test]
fn w29_r11_save_draft_on_a_host() -> anyhow::Result<()> {
    let g = on_github()?;
    let edit = "/api/ci/repos/acme%2Fapp/workflows/.github/workflows/ci.yml/edit";
    let first = post(&g.s, edit, &json!({ "content": TRY_ONE, "draft": true, "title": "Try a matrix" }))?;
    let branch = at(&first, "/draft/branch").to_owned();
    assert!(branch.starts_with("kiln/draft/ci-") && branch.len() == "kiln/draft/ci-".len() + 8, "{first}");
    assert_eq!((at(&first, "/draft/kind"), at(&first, "/draft/name"), at(&first, "/draft/state")), ("change", "#12", "draft"), "{first}");
    let second = post(&g.s, edit, &json!({ "content": TRY_TWO, "draft": true, "title": "Try a matrix" }))?;
    assert_eq!((at(&second, "/draft/id"), at(&second, "/draft/branch")), (at(&first, "/draft/id"), branch.as_str()), "the same draft: {second}");
    assert_ne!(at(&second, "/draft/head"), at(&first, "/draft/head"), "{second}");

    let pulls = g.sent("POST", "/repos/acme/app/pulls");
    assert_eq!(pulls.len(), 1, "one pull request: {pulls:?}");
    assert_eq!((pulls.first().and_then(|p| p.get("draft")), pulls.first().and_then(|p| p.get("head"))), (Some(&json!(true)), Some(&json!(branch))), "{pulls:?}");
    let commits = g.sent("POST", "/repos/acme/app/git/commits");
    assert_eq!(commits.len(), 2, "{commits:?}");
    for c in &commits {
        assert!(c.get("message").and_then(Value::as_str).is_some_and(|m| m.ends_with("[skip ci]\n")), "a draft's commit says [skip ci]: {c}");
    }
    assert_eq!(commits.get(1).and_then(|c| c.get("parents")), Some(&json!([at(&first, "/draft/head")])), "the second is on the first");
    let made = g.sent("POST", "/repos/acme/app/git/refs");
    assert_eq!(made.iter().map(|r| r.get("ref").cloned()).collect::<Vec<_>>(), [Some(json!(format!("refs/heads/{branch}")))], "{made:?}");
    let moved = g.sent("PATCH", &format!("/repos/acme/app/git/refs/heads/{branch}"));
    assert_eq!(moved.iter().map(|r| r.get("sha").cloned()).collect::<Vec<_>>(), [Some(json!(at(&second, "/draft/head")))], "{moved:?}");
    Ok(())
}

/// W29-R25: the same Git Data commit contains the workflow and both locks; later saves keep
/// its branch and pull, and read-only verification does not create another.
#[test]
fn w29_r25_a_hosts_draft_commits_both_locks_and_keeps_its_identity() -> anyhow::Result<()> {
    let g = on_github()?;
    let home = g._dir.path().join("lock-home");
    std::fs::create_dir_all(&home)?;
    let publisher = hosted_with(&g.s, &home, g._dir.path(), "acme/tools", &[(".kiln/config.yaml", "share: owner\n"), (".github/actions/check/action.yml", "runs: {using: composite, steps: [{run: echo accepted, shell: bash}]}\n")])?;
    let accepted = git(&home, &publisher, &["rev-parse", "HEAD"])?.trim().to_owned();
    let uses = format!("kiln://acme/tools/.github/actions/check@{accepted}");
    let text = format!("name: ci\non: push\njobs:\n  check:\n    runs-on: local\n    steps:\n      - uses: {uses}\n");
    let edit = "/api/ci/repos/acme%2Fapp/workflows/.github/workflows/ci.yml/edit";
    let first = post(&g.s, edit, &json!({"content": text, "draft": true, "title": "Lock the shared check"}))?;
    let trees = g.sent("POST", "/repos/acme/app/git/trees");
    let paths: Vec<&str> = trees.first().and_then(|t| t.get("tree")).and_then(Value::as_array).into_iter().flatten().filter_map(|f| f.get("path").and_then(Value::as_str)).collect();
    for file in [".github/workflows/ci.yml", ".github/workflows/actions.lock", ".kiln/lock"] { assert!(paths.contains(&file), "{file}: {trees:?}"); }
    assert_eq!(g.sent("POST", "/repos/acme/app/git/commits").len(), 1);
    let check = get(&g.s, &format!("/api/ci/repos/acme%2Fapp/locks/verify?sha={}", at(&first, "/draft/head")))?;
    assert_eq!(check.get("fresh"), Some(&json!(true)), "{check}");
    let second = post(&g.s, edit, &json!({"content": format!("{text}# same accepted dependency\n"), "draft": true, "title": "Lock the shared check"}))?;
    assert_eq!((at(&second, "/draft/id"), at(&second, "/draft/branch")), (at(&first, "/draft/id"), at(&first, "/draft/branch")));
    assert_eq!(g.sent("POST", "/repos/acme/app/pulls").len(), 1);
    assert_eq!(g.sent("POST", "/repos/acme/app/git/commits").len(), 2);
    let (st, _) = http(g.s.port, "POST", "/api/ci/repos/acme%2Fapp/locks", &g.s.admin, Some(&json!({})))?;
    assert_eq!(st, 409, "there is no new lock change to propose");
    assert_eq!(g.sent("POST", "/repos/acme/app/git/commits").len(), 2, "no no-op host commit");
    assert_eq!(g.sent("POST", "/repos/acme/app/pulls").len(), 1);
    Ok(())
}

/// A repository's runs, newest first.
fn runs(s: &Server, repo: &str) -> anyhow::Result<Vec<Value>> {
    let v = get(s, &format!("/api/ci/runs?repo={repo}&limit=100"))?;
    Ok(v.get("runs").and_then(Value::as_array).cloned().unwrap_or_default())
}

/// Until a run `pick` finds is there, a repository's runs then; `what` for the failure.
fn until_run(s: &Server, repo: &str, what: &str, pick: impl Fn(&Value) -> bool) -> anyhow::Result<Vec<Value>> {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let all = runs(s, repo)?;
        if all.iter().any(&pick) {
            return Ok(all);
        }
        anyhow::ensure!(Instant::now() < deadline, "no run {what} in {repo}: {all:?}\n{}", s.log());
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// A repository kiln hosts, cloned under `dir` as `name`'s last part, its first commit (README)
/// pushed and polled.
fn hosted(s: &Server, home: &Path, dir: &Path, name: &str) -> anyhow::Result<std::path::PathBuf> {
    hosted_with(s, home, dir, name, &[])
}

/// [`hosted`], `files` in its first commit too.
fn hosted_with(s: &Server, home: &Path, dir: &Path, name: &str, files: &[(&str, &str)]) -> anyhow::Result<std::path::PathBuf> {
    let (st, body) = http(s.port, "POST", "/api/repos", &s.admin, Some(&json!({ "name": name, "mode": "native" })))?;
    anyhow::ensure!(st == 200, "{name}: {st} {body}");
    let leaf = name.rsplit('/').next().unwrap_or(name);
    git(home, dir, &["clone", "-q", &format!("http://x:{}@127.0.0.1:{}/git/{name}.git", s.admin, s.port), leaf])?;
    let work = dir.join(leaf);
    std::fs::write(work.join("README.md"), format!("{name}\n"))?;
    for (path, text) in files {
        let p = work.join(path);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(p, text)?;
    }
    git(home, &work, &["add", "."])?;
    git(home, &work, &["commit", "-qm", "first"])?;
    git(home, &work, &["push", "-q", "origin", "main"])?;
    polled(s, name)?;
    Ok(work)
}

/// Writes `files` in `work`, commits them and pushes `branch` (made from what's checked out).
fn push_files(home: &Path, work: &Path, branch: &str, files: &[(&str, &str)]) -> anyhow::Result<String> {
    if git(home, work, &["rev-parse", "--abbrev-ref", "HEAD"])?.trim() != branch {
        git(home, work, &["checkout", "-q", "-B", branch])?;
    }
    for (path, text) in files {
        let p = work.join(path);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(p, text)?;
    }
    git(home, work, &["add", "."])?;
    git(home, work, &["commit", "-qm", &format!("on {branch}")])?;
    git(home, work, &["push", "-q", "origin", branch])?;
    Ok(git(home, work, &["rev-parse", "HEAD"])?.trim().to_owned())
}

/// W29-R10: no trigger runs a draft. A push to a draft's branch starts nothing, though a workflow
/// takes every branch, and the preview says why; a draft diff doesn't run the workflow it drafts,
/// though that one says `drafts: true`, while another workflow's `drafts: true` runs on it.
#[test]
fn w29_r10_no_trigger_runs_a_draft() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { agent: true, ci: "kiln_workflows = true", extra: "" })?;
    let home = dir.path().join("githome");
    std::fs::create_dir_all(&home)?;
    let work = hosted(&s, &home, dir.path(), "acme/app")?;
    let all = "name: all\non:\n  push:\n    branches: ['**']\njobs:\n  a:\n    runs-on: local\n    steps:\n      - run: echo pushed\n";
    let watch = "name: watch\non:\n  diff:\n    drafts: true\njobs:\n  w:\n    runs-on: local\n    steps:\n      - run: echo watched\n";
    // Kiln's dialect: a commit with `.kiln/` workflows leaves GitHub's to GitHub.
    push_files(&home, &work, "main", &[(".kiln/all.yaml", all), (".kiln/watch.yaml", watch)])?;
    until_run(&s, "acme/app", "on main", |r| r.get("branch").and_then(Value::as_str) == Some("main"))?;

    // A draft's branch, then another: only the other runs.
    push_files(&home, &work, "kiln/draft/all-01a19f2c", &[("README.md", "a draft\n")])?;
    git(&home, &work, &["checkout", "-q", "main"])?;
    push_files(&home, &work, "feature", &[("README.md", "a feature\n")])?;
    let seen = until_run(&s, "acme/app", "on feature", |r| r.get("branch").and_then(Value::as_str) == Some("feature"))?;
    assert!(!seen.iter().any(|r| r.get("branch").and_then(Value::as_str).is_some_and(|b| b.contains("kiln/draft/"))), "nothing on a draft's branch: {seen:?}");
    let preview = post(&s, "/api/ci/plan-preview", &json!({ "repo": "acme/app", "ref": "kiln/draft/all-01a19f2c", "path": ".kiln/all.yaml", "event": "push" }))?;
    assert_eq!(preview.get("runs"), Some(&json!(false)), "{preview}");
    assert!(at(&preview, "/why").contains("draft"), "{preview}");

    // A draft diff of a workflow of its own.
    git(&home, &work, &["checkout", "-q", "main"])?;
    let own = "name: self\non:\n  diff:\n    drafts: true\njobs:\n  s:\n    runs-on: local\n    steps:\n      - run: echo self\n";
    let saved = post(&s, "/api/ci/repos/acme%2Fapp/workflows/.kiln/self.yaml/edit", &json!({ "content": own, "draft": true, "title": "A workflow of its own" }))?;
    assert_eq!(at(&saved, "/draft/kind"), "diff", "{saved}");
    let seen = until_run(&s, "acme/app", "of .kiln/watch.yaml on the draft diff", |r| r.get("workflow").and_then(Value::as_str) == Some(".kiln/watch.yaml"))?;
    assert!(!seen.iter().any(|r| r.get("workflow").and_then(Value::as_str) == Some(".kiln/self.yaml")), "the draft's own workflow doesn't run on it: {seen:?}");
    Ok(())
}

/// W29-R10: no other repository can use a draft. A `uses:` of another repository's draft branch,
/// or of a commit only that branch reaches (whole or abbreviated), fails its run before any job
/// starts (KCI4023): through `kiln://`, through a plain name a native source sends to kiln, and as
/// a job's reusable workflow call. Its published branch is used as ever, each way.
#[test]
fn w29_r10_no_other_repository_uses_a_draft() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    // acme's plain names are kiln's own repositories (W29-R18).
    let s = start(dir.path(), &Options { agent: true, ci: "kiln_workflows = true\n[[ci.actions.sources]]\nowners = [\"acme\"]\nnative = true\n", extra: "" })?;
    let home = dir.path().join("githome");
    std::fs::create_dir_all(&home)?;
    // Shared with its owner's repositories (W29-R17): a draft is refused all the same.
    let kit = hosted_with(&s, &home, dir.path(), "acme/kit", &[(".kiln/config.yaml", "share: owner\n")])?;
    let action = |said: &str| format!("name: x\nruns:\n  using: composite\n  steps:\n    - run: echo {said}\n      shell: bash\n");
    let build = "name: build\non: workflow_call\njobs:\n  b:\n    runs-on: local\n    steps:\n      - run: echo built\n";
    push_files(&home, &kit, "main", &[(".github/actions/x/action.yml", &action("published")), (".github/workflows/build.yml", build)])?;
    let drafted = push_files(&home, &kit, "kiln/draft/x-01a19f2c", &[(".github/actions/x/action.yml", &action("drafted"))])?;
    let short = drafted.get(..12).unwrap_or_default().to_owned();
    let app = hosted(&s, &home, dir.path(), "acme/app")?;
    let step = |name: &str, uses: &str| format!("name: {name}\non: push\njobs:\n  a:\n    runs-on: local\n    steps:\n      - uses: {uses}\n");
    let call = |name: &str, uses: &str| format!("name: {name}\non: push\njobs:\n  c:\n    uses: {uses}\n");
    // Each draft-only pin, and the same name at the published branch.
    let refused = [
        (".kiln/branch.yaml", step("branch", "kiln://acme/kit/.github/actions/x@kiln/draft/x-01a19f2c")),
        (".kiln/commit.yaml", step("commit", &format!("kiln://acme/kit/.github/actions/x@{drafted}"))),
        (".kiln/short.yaml", step("short", &format!("kiln://acme/kit/.github/actions/x@{short}"))),
        (".kiln/plain.yaml", step("plain", &format!("acme/kit/.github/actions/x@{drafted}"))),
        (".kiln/call.yaml", call("call", &format!("kiln://acme/kit/.github/workflows/build.yml@{drafted}"))),
        (".kiln/call-plain.yaml", call("call-plain", &format!("acme/kit/.github/workflows/build.yml@{drafted}"))),
    ];
    let used = [
        (".kiln/ok.yaml", step("ok", "kiln://acme/kit/.github/actions/x@main")),
        (".kiln/plain-ok.yaml", step("plain-ok", "acme/kit/.github/actions/x@main")),
        (".kiln/call-ok.yaml", call("call-ok", "acme/kit/.github/workflows/build.yml@main")),
    ];
    let files: Vec<(&str, &str)> = refused.iter().chain(used.iter()).map(|(p, t)| (*p, t.as_str())).collect();
    push_files(&home, &app, "main", &files)?;
    let done = |path: &str| {
        let path = path.to_owned();
        move |r: &Value| r.get("workflow").and_then(Value::as_str) == Some(path.as_str()) && r.get("finished_at").is_some_and(|f| !f.is_null())
    };
    for (path, _) in &refused {
        let seen = until_run(&s, "acme/app", &format!("of {path}, finished"), done(path))?;
        let run = seen.iter().find(|r| done(path)(r)).cloned().unwrap_or_default();
        assert!(at(&run, "/error").contains("KCI4023"), "{path}: {run}");
        assert_eq!(at(&run, "/conclusion"), "startup_failure", "before any job: {path}: {run}");
    }
    for (path, _) in &used {
        let seen = until_run(&s, "acme/app", &format!("of {path}, finished"), done(path))?;
        let run = seen.iter().find(|r| done(path)(r)).cloned().unwrap_or_default();
        assert!(!at(&run, "/error").contains("KCI4023"), "a published branch is used as ever: {path}: {run}");
        assert_eq!(at(&run, "/conclusion"), "success", "{path}: {run}\n{}", s.log());
    }
    Ok(())
}

/// A Pipelines verb through Ship, as the admin: its status and answer.
fn act(s: &Server, body: &Value) -> anyhow::Result<(u16, Value)> {
    let (st, text) = http(s.port, "POST", "/api/ship/pipelines", &s.admin, Some(body))?;
    Ok((st, serde_json::from_str(&text).unwrap_or(Value::Null)))
}

/// A repository kiln hosts with `.github/workflows/ci.yml` on its trunk, polled once.
fn kiln_with_ci() -> anyhow::Result<(Server, tempfile::TempDir)> {
    let dir = tempfile::tempdir()?;
    let s = start(dir.path(), &Options { agent: true, ..Options::default() })?;
    let home = dir.path().join("githome");
    std::fs::create_dir_all(&home)?;
    let work = hosted(&s, &home, dir.path(), "acme/app")?;
    push_files(&home, &work, "main", &[(".github/workflows/ci.yml", CI)])?;
    Ok((s, dir))
}

/// The runs of `path` in `repo` that aren't its trunk's: a diff's.
fn diff_runs(s: &Server, repo: &str, path: &str) -> anyhow::Result<usize> {
    Ok(runs(s, repo)?.iter().filter(|r| at(r, "/workflow") == path && at(r, "/branch") != "main").count())
}

/// Until `path` has `n` runs of a diff in `repo`, and still has after 2 s: never more.
fn exactly(s: &Server, repo: &str, path: &str, n: usize) -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(120);
    while diff_runs(s, repo, path)? < n {
        anyhow::ensure!(Instant::now() < deadline, "{path} has fewer than {n} runs of a diff: {:?}\n{}", runs(s, repo)?, s.log());
        std::thread::sleep(Duration::from_millis(300));
    }
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(diff_runs(s, repo, path)?, n, "{path}: {:?}", runs(s, repo)?);
    Ok(())
}

/// W29-R12: Ready asks for review. On kiln the draft diff leaves draft, and the workflow it drafts,
/// held back while it was a draft in either dialect (W29-R10), starts once: a GitHub-dialect
/// `pull_request` one as the synchronize the draft held back, a kiln-dialect `on: diff` one from
/// the version's `diff` event. On GitHub the draft's head is committed again asking to skip
/// nothing, then GraphQL marks the pull request ready. The record says review, and Ready again is
/// refused.
#[test]
fn w29_r12_ready_asks_for_review() -> anyhow::Result<()> {
    let save = json!({ "repo": "acme/app", "path": ".github/workflows/ci.yml", "verb": "save_draft", "content": TRY_ONE, "title": "Try a matrix" });
    let ready = json!({ "repo": "acme/app", "path": ".github/workflows/ci.yml", "verb": "ready" });

    // On kiln, a workflow the draft doesn't change runs on its diff: the diff's events came.
    let (s, dir) = kiln_with_ci()?;
    let (home, work) = (dir.path().join("githome"), dir.path().join("app"));
    let watch = "name: watch\non: pull_request\njobs:\n  w:\n    runs-on: local\n    steps:\n      - run: echo watched\n";
    push_files(&home, &work, "main", &[(".github/workflows/watch.yml", watch)])?;
    let on_pr = "name: ci\non: pull_request\njobs:\n  build:\n    runs-on: local\n    steps:\n      - run: echo reviewed\n";
    let (st, saved) = act(&s, &json!({ "repo": "acme/app", "path": ".github/workflows/ci.yml", "verb": "save_draft", "content": on_pr, "title": "Check pull requests" }))?;
    assert_eq!(st, 200, "{saved}");
    let name = at(&saved, "/draft/name").to_owned();
    exactly(&s, "acme/app", ".github/workflows/watch.yml", 1)?;
    assert_eq!(diff_runs(&s, "acme/app", ".github/workflows/ci.yml")?, 0, "held back while it's a draft");
    let (st, out) = act(&s, &ready)?;
    assert_eq!((st, at(&out, "/draft/state")), (200, "review"), "{out}");
    assert_eq!(at(&get(&s, &format!("/api/diffs/{name}"))?, "/state"), "needs-review");
    exactly(&s, "acme/app", ".github/workflows/ci.yml", 1)?;
    let (st, again) = act(&s, &ready)?;
    assert_eq!(st, 409, "in review already: {again}");

    // In kiln's dialect, the `on: diff` one: held while it's a draft, then once.
    let dir = tempfile::tempdir()?;
    let k = start(dir.path(), &Options { agent: true, ci: "kiln_workflows = true", extra: "" })?;
    let home = dir.path().join("githome");
    std::fs::create_dir_all(&home)?;
    let work = hosted(&k, &home, dir.path(), "acme/app")?;
    let kiln_watch = "name: watch\non:\n  diff:\n    drafts: true\njobs:\n  w:\n    runs-on: local\n    steps:\n      - run: echo watched\n";
    push_files(&home, &work, "main", &[(".kiln/watch.yaml", kiln_watch), (".kiln/ci.yaml", "name: ci\non: push\njobs:\n  b:\n    runs-on: local\n    steps:\n      - run: echo pushed\n")])?;
    let on_diff = "name: ci\non: diff\njobs:\n  b:\n    runs-on: local\n    steps:\n      - run: echo reviewed\n";
    let (st, saved) = act(&k, &json!({ "repo": "acme/app", "path": ".kiln/ci.yaml", "verb": "save_draft", "content": on_diff, "title": "Check diffs" }))?;
    assert_eq!(st, 200, "{saved}");
    exactly(&k, "acme/app", ".kiln/watch.yaml", 1)?;
    assert_eq!(diff_runs(&k, "acme/app", ".kiln/ci.yaml")?, 0, "held back while it's a draft");
    let (st, out) = act(&k, &json!({ "repo": "acme/app", "path": ".kiln/ci.yaml", "verb": "ready" }))?;
    assert_eq!((st, at(&out, "/draft/state")), (200, "review"), "{out}");
    exactly(&k, "acme/app", ".kiln/ci.yaml", 1)?;

    let g = on_github()?;
    let (st, saved) = act(&g.s, &save)?;
    assert_eq!(st, 200, "{saved}");
    let head = at(&saved, "/draft/head").to_owned();
    let (st, out) = act(&g.s, &ready)?;
    assert_eq!((st, at(&out, "/draft/state")), (200, "review"), "{out}");
    let commits = g.sent("POST", "/repos/acme/app/git/commits");
    let last = commits.last().cloned().unwrap_or_default();
    let tree = git(&g._dir.path().join("githome"), &g._dir.path().join("gh/acme/app.git"), &["rev-parse", &format!("{head}^{{tree}}")])?.trim().to_owned();
    assert_eq!((last.get("message"), last.get("parents"), last.get("tree")), (Some(&json!("Try a matrix\n")), Some(&json!([head])), Some(&json!(tree))), "the same tree, asking to skip nothing: {last}");
    let moved = g.sent("PATCH", &format!("/repos/acme/app/git/refs/heads/{}", at(&saved, "/draft/branch")));
    assert_eq!(moved.last().and_then(|m| m.get("sha")), Some(&json!(at(&out, "/draft/head"))), "{moved:?}");
    let asked = g.sent("POST", "/graphql");
    assert_eq!(asked.first().and_then(|q| q.pointer("/variables/id")), Some(&json!("PR_kwDOdraft12")), "{asked:?}");
    Ok(())
}

/// W29-R13: Discard. On kiln the draft diff is abandoned, and saving again starts a new draft; on
/// GitHub the pull request is closed and the draft's branch deleted.
#[test]
fn w29_r13_discard() -> anyhow::Result<()> {
    let save = json!({ "repo": "acme/app", "path": ".github/workflows/ci.yml", "verb": "save_draft", "content": TRY_ONE, "title": "Try a matrix" });
    let discard = json!({ "repo": "acme/app", "path": ".github/workflows/ci.yml", "verb": "discard" });

    let (s, _dir) = kiln_with_ci()?;
    let (_, first) = act(&s, &save)?;
    let (st, out) = act(&s, &discard)?;
    assert_eq!((st, at(&out, "/draft/state")), (200, "discarded"), "{out}");
    assert_eq!(at(&get(&s, &format!("/api/diffs/{}", at(&first, "/draft/name")))?, "/state"), "abandoned");
    let (st, _) = act(&s, &discard)?;
    assert_eq!(st, 404, "nothing left to discard");
    let (_, next) = act(&s, &save)?;
    assert_ne!((at(&next, "/draft/id"), at(&next, "/draft/name")), (at(&first, "/draft/id"), at(&first, "/draft/name")), "a new draft: {next}");

    let g = on_github()?;
    let (_, saved) = act(&g.s, &save)?;
    let branch = at(&saved, "/draft/branch").to_owned();
    let (st, out) = act(&g.s, &discard)?;
    assert_eq!((st, at(&out, "/draft/state")), (200, "discarded"), "{out}");
    assert_eq!(g.sent("PATCH", "/repos/acme/app/pulls/12"), [json!({ "state": "closed" })]);
    assert_eq!(g.sent("DELETE", &format!("/repos/acme/app/git/refs/heads/{branch}")).len(), 1);
    Ok(())
}

/// Until run `id` is finished: `{ run, legs }`.
fn finished(s: &Server, id: &str) -> anyhow::Result<Value> {
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        let run = get(s, &format!("/api/ci/runs/{id}"))?;
        if run.pointer("/run/finished_at").is_some_and(|f| !f.is_null()) {
            return Ok(run);
        }
        anyhow::ensure!(Instant::now() < deadline, "run {id} didn't finish: {run}\n{}", s.log());
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// A run's legs, by job.
fn legs(run: &Value) -> Vec<(String, String, String)> {
    run.get("legs").and_then(Value::as_array).into_iter().flatten().map(|l| (at(l, "/job").to_owned(), at(l, "/conclusion").to_owned(), at(l, "/error").to_owned())).collect()
}

/// W29-R14: a draft runs by its author's hand, as the event they pick: a run of a commit of its
/// own, so its diff counts no check of it, marked the draft's, its job that names an environment
/// stopped before it deploys, saying why. A published workflow without workflow_dispatch doesn't
/// run by hand, saying so.
#[test]
fn w29_r14_run_a_draft_by_hand() -> anyhow::Result<()> {
    let (s, _dir) = kiln_with_ci()?;
    let path = ".github/workflows/try.yml";
    let draft = "name: try\non: push\njobs:\n  build:\n    runs-on: local\n    steps:\n      - run: echo built\n  deploy:\n    needs: build\n    runs-on: local\n    environment: production\n    steps:\n      - run: echo deployed\n";
    let (st, saved) = act(&s, &json!({ "repo": "acme/app", "path": path, "verb": "save_draft", "content": draft, "title": "Try a deploy" }))?;
    assert_eq!(st, 200, "{saved}");
    let (st, ran) = act(&s, &json!({ "repo": "acme/app", "path": path, "verb": "run", "event": "push" }))?;
    assert_eq!(st, 200, "{ran}");
    let id = at(&ran, "/runs/0/id").to_owned();
    let run = finished(&s, &id)?;
    assert_eq!(at(&run, "/run/draft"), at(&saved, "/draft/id"), "marked the draft's: {run}");
    let diff = get(&s, &format!("/api/diffs/{}", at(&saved, "/draft/name")))?;
    let versions: Vec<&str> = diff.get("versions").and_then(Value::as_array).into_iter().flatten().filter_map(|v| v.get("commit").and_then(Value::as_str)).collect();
    assert!(!versions.contains(&at(&run, "/run/sha")), "a commit of its own: {run} {diff}");
    assert!(!diff.get("runs").and_then(Value::as_array).into_iter().flatten().any(|r| r.get("id").and_then(Value::as_str) == Some(id.as_str())), "its diff counts no check of it: {diff}");
    let by_job = legs(&run);
    assert!(by_job.iter().any(|(j, c, _)| j == "build" && c == "success"), "{by_job:?}");
    assert!(by_job.iter().any(|(j, c, e)| j == "deploy" && c != "success" && e.contains("draft")), "it doesn't deploy, saying why: {by_job:?}");
    // The published workflow takes only `push`: it doesn't run by hand.
    let (st, said) = act(&s, &json!({ "repo": "acme/app", "path": ".github/workflows/ci.yml", "verb": "run" }))?;
    assert_eq!(st, 422, "{said}");
    assert!(said.to_string().contains("workflow_dispatch"), "{said}");
    Ok(())
}

/// W29-R14: Run with chosen jobs runs those and what they need, nothing else.
#[test]
fn w29_r14_run_chosen_jobs() -> anyhow::Result<()> {
    let (s, dir) = kiln_with_ci()?;
    let home = dir.path().join("githome");
    let work = dir.path().join("app");
    let pick = "name: pick\non: workflow_dispatch\njobs:\n  b:\n    runs-on: local\n    steps:\n      - run: echo b\n  a:\n    needs: b\n    runs-on: local\n    steps:\n      - run: echo a\n  c:\n    runs-on: local\n    steps:\n      - run: echo c\n";
    push_files(&home, &work, "main", &[(".github/workflows/pick.yml", pick)])?;
    // Asked again until kiln has the push.
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let (st, ran) = act(&s, &json!({ "repo": "acme/app", "path": ".github/workflows/pick.yml", "verb": "run", "jobs": ["a"] }))?;
        if st == 200 {
            break;
        }
        anyhow::ensure!(st == 404 && Instant::now() < deadline, "{st} {ran}");
        std::thread::sleep(Duration::from_millis(300));
    }
    let seen = until_run(&s, "acme/app", "of pick.yml by hand", |r| r.get("workflow").and_then(Value::as_str) == Some(".github/workflows/pick.yml") && r.get("event").and_then(Value::as_str) == Some("workflow_dispatch"))?;
    let id = seen.iter().find(|r| r.get("event").and_then(Value::as_str) == Some("workflow_dispatch")).map(|r| at(r, "/id").to_owned()).unwrap_or_default();
    let run = finished(&s, &id)?;
    let mut jobs: Vec<String> = legs(&run).into_iter().map(|(j, ..)| j).collect();
    jobs.sort();
    assert_eq!(jobs, ["a", "b"], "a and what it needs: {run}");
    let (st, said) = act(&s, &json!({ "repo": "acme/app", "path": ".github/workflows/pick.yml", "verb": "run", "jobs": ["nope"] }))?;
    assert_eq!(st, 404, "{said}");
    Ok(())
}

/// W29-R08's New: a workflow's or action's first files as `kiln_ci::scaffold` makes them (or as
/// the caller edited them), saved together as the caller's draft: on kiln one draft diff of them
/// all, on GitHub one commit of them all on the draft's branch. A dry run says them and saves
/// nothing; a name the trunk has already is refused; a new workflow's draft runs on no trigger.
#[test]
fn w29_r08_new_makes_a_draft() -> anyhow::Result<()> {
    let (s, _dir) = kiln_with_ci()?;
    let paths = |v: &Value| -> Vec<String> { v.get("files").and_then(Value::as_array).map(|a| a.iter().filter_map(|f| f.get("path").and_then(Value::as_str).or_else(|| f.as_str())).map(str::to_owned).collect()).unwrap_or_default() };
    // A dry run: the three files a Firecracker action is, nothing saved.
    let (st, dry) = act(&s, &json!({ "repo": "acme/app", "verb": "new", "kind": "firecracker", "name": "fuzz-target", "dry": true }))?;
    assert_eq!(st, 200, "{dry}");
    assert_eq!(paths(&dry), [".github/actions/fuzz-target/action.yml", ".github/actions/fuzz-target/Dockerfile", ".github/actions/fuzz-target/run.sh"]);
    // W29-R08, pl-6: New's machines are the connected runners' facts; its draft is not allocated by a preview.
    assert!(dry.pointer("/machines/firecracker").is_some_and(Value::is_array), "{dry}");
    assert_eq!(dry.pointer("/machines/shards"), Some(&json!([])), "{dry}");
    assert_eq!(at(&dry, "/next_version"), "v0.1.0", "no previous release: {dry}");
    assert_eq!(at(&dry, "/draft_name"), "D1", "{dry}");
    assert!(get(&s, "/api/repos/acme%2Fapp/diffs")?.to_string().matches("\"name\":\"D").count() == 0, "a dry run saves nothing");
    // Saved as edited: one draft diff of the three.
    let mut files = dry.get("files").cloned().unwrap_or_default();
    if let Some(run) = files.as_array_mut().and_then(|a| a.get_mut(2)).and_then(Value::as_object_mut) {
        run.insert("text".into(), json!("#!/bin/sh\necho fuzzing \"$1\"\n"));
    }
    let (st, saved) = act(&s, &json!({ "repo": "acme/app", "verb": "new", "kind": "firecracker", "name": "fuzz-target", "files": files, "title": "A fuzz action" }))?;
    assert_eq!((st, at(&saved, "/draft/kind"), at(&saved, "/draft/state"), at(&saved, "/draft/path")), (200, "diff", "draft", ".github/actions/fuzz-target/action.yml"), "{saved}");
    let changed = get(&s, &format!("/api/diffs/{}/versions/1/files", at(&saved, "/draft/name")))?.to_string();
    for p in paths(&dry) {
        assert!(changed.contains(&p), "{p} in the draft: {changed}");
    }
    // Its row: DRAFT, Unpublished, the action's.
    let list = get(&s, "/api/ship/pipelines?repo=acme/app")?;
    let rows: Vec<Value> = list.pointer("/repos/0/rows").and_then(Value::as_array).cloned().unwrap_or_default();
    let row = rows.iter().find(|r| at(r, "/path") == ".github/actions/fuzz-target/action.yml").cloned().unwrap_or_default();
    assert_eq!((at(&row, "/word"), at(&row, "/draft/state")), ("Unpublished", "unpublished"), "{list}");
    // A name the trunk has: refused, saying so.
    let (st, out) = act(&s, &json!({ "repo": "acme/app", "verb": "new", "kind": "workflow", "name": "ci" }))?;
    assert_eq!(st, 409, "{out}");
    // A workflow, recommended: its draft runs on no trigger.
    let (st, wf) = act(&s, &json!({ "repo": "acme/app", "verb": "new", "kind": "workflow", "language": "kiln", "name": "nightly", "from": "recommended" }))?;
    assert_eq!((st, at(&wf, "/path")), (200, ".kiln/nightly.yaml"), "{wf}");
    exactly(&s, "acme/app", ".kiln/nightly.yaml", 0)?;

    // On GitHub: one commit of an action's three files on the draft's branch, a draft pull request.
    let g = on_github()?;
    let (st, out) = act(&g.s, &json!({ "repo": "acme/app", "verb": "new", "kind": "docker", "name": "lint" }))?;
    assert_eq!((st, at(&out, "/draft/kind")), (200, "change"), "{out}");
    let trees = g.sent("POST", "/repos/acme/app/git/trees");
    let tree = trees.first().map(ToString::to_string).unwrap_or_default();
    for p in [".github/actions/lint/action.yml", ".github/actions/lint/Dockerfile", ".github/actions/lint/entrypoint.sh"] {
        assert!(tree.contains(p), "{p} in the commit's tree: {trees:?}");
    }
    assert_eq!(g.sent("POST", "/repos/acme/app/git/commits").len(), 1, "one commit");
    assert_eq!(g.sent("POST", "/repos/acme/app/pulls").first().and_then(|p| p.get("draft")), Some(&json!(true)));
    Ok(())
}

/// W29-S03 (R06, R07): kiln's own `diff` trigger of a GitHub-dialect workflow goes in its overlay,
/// so the file stays GitHub's (W02-R07). The pane says where the overlay goes. The plan preview
/// plans the `diff` event with the overlay as edited, its schedule in the overlay's zone, and
/// doesn't without it. Save draft keeps the workflow and its overlay in one draft diff. Another
/// workflow's overlay, or GitHub's events in one, is refused.
#[test]
fn w29_s03_kilns_own_trigger_goes_in_its_overlay() -> anyhow::Result<()> {
    let (s, dir) = kiln_with_ci()?;
    let (home, work) = (dir.path().join("githome"), dir.path().join("app"));
    let sha = git(&home, &work, &["rev-parse", "HEAD"])?.trim().to_owned();
    let path = ".github/workflows/ci.yml";
    let pane = get(&s, &format!("/api/ship/pipelines/acme/app?path={path}"))?;
    assert_eq!((at(&pane, "/detail/overlay/path"), at(&pane, "/detail/overlay/text")), (".kiln/overlays/ci.yaml", ""), "{pane}");
    // A schedule in the file; kiln's `diff`, and the schedule's zone, in its overlay.
    let content = "name: ci\non:\n  push:\n  schedule:\n    - cron: \"0 3 * * *\"\njobs:\n  build:\n    runs-on: local\n    steps:\n      - run: echo one\n";
    let overlay = "on:\n  diff:\n    branches: [main]\n  schedule:\n    - cron: \"0 3 * * *\"\n      timezone: America/Los_Angeles\n";
    let preview = |o: Option<&str>| -> anyhow::Result<Value> {
        let mut b = json!({ "repo": "acme/app", "ref": "main", "sha": sha, "path": path, "content": content, "event": "diff" });
        if let (Some(o), Some(m)) = (o, b.as_object_mut()) {
            m.insert("overlay".into(), json!(o));
        }
        let (st, text) = http(s.port, "POST", "/api/ci/plan-preview", &s.admin, Some(&b))?;
        anyhow::ensure!(st == 200, "{st} {text}");
        Ok(serde_json::from_str(&text)?)
    };
    let with = preview(Some(overlay))?;
    assert_eq!(with.get("runs"), Some(&json!(true)), "{with}");
    assert_eq!(with.pointer("/schedules/0/timezone").and_then(Value::as_str), Some("America/Los_Angeles"), "{with}");
    let without = preview(None)?;
    assert_eq!(without.get("runs"), Some(&json!(false)), "{without}");
    // Saved together: one draft diff of both, its pane holding the overlay as saved.
    let (st, saved) = act(&s, &json!({ "repo": "acme/app", "path": path, "verb": "save_draft", "content": content, "files": [{ "path": ".kiln/overlays/ci.yaml", "text": overlay }], "title": "Run on diffs" }))?;
    assert_eq!((st, at(&saved, "/draft/kind")), (200, "diff"), "{saved}");
    let changed = get(&s, &format!("/api/diffs/{}/versions/1/files", at(&saved, "/draft/name")))?.to_string();
    for p in [path, ".kiln/overlays/ci.yaml"] {
        assert!(changed.contains(p), "{p} in the draft: {changed}");
    }
    let draft = get(&s, &format!("/api/ship/pipelines/acme/app?path={path}&draft={}", at(&saved, "/draft/id")))?;
    assert_eq!(at(&draft, "/detail/overlay/text"), overlay, "{draft}");
    // Another workflow's overlay: refused; GitHub's events in its own: its errors, nothing saved.
    let (st, out) = act(&s, &json!({ "repo": "acme/app", "path": path, "verb": "save_draft", "content": content, "files": [{ "path": ".kiln/overlays/other.yaml", "text": overlay }] }))?;
    assert_eq!(st, 400, "{out}");
    let (st, out) = act(&s, &json!({ "repo": "acme/app", "path": path, "verb": "save_draft", "content": content, "files": [{ "path": ".kiln/overlays/ci.yaml", "text": "on:\n  pull_request: {}\n" }] }))?;
    assert_eq!(st, 422, "{out}");
    Ok(())
}
