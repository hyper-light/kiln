//! The hosts' pull requests mirrored into Ship's read model (docs/ci.md W26-R01, R20, AC01, AC09):
//! against simulators of GitHub's, GitLab's and Bitbucket's pull request APIs, shaped as each host
//! shapes them, kiln mirrors each open pull request (its head, its other commits, its reviewers,
//! reviews and threads) and Ship shows it by commit, and what the host verified of each commit's
//! signature (W27-R03). Each simulator counts the requests its host would count, answering 304 to a
//! matching `If-None-Match` without counting it, as GitHub does.

#![cfg(unix)]

mod common;

use std::collections::BTreeMap;
use std::hash::{Hash as _, Hasher as _};
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::{Options, http, start};
use kiln::githost::mirror::{self, Mirrored, State};
use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Host {
    Github,
    Gitlab,
    Bitbucket,
}

/// A pull request as a simulator keeps it.
#[derive(Debug, Clone)]
struct Pull {
    number: u64,
    title: String,
    author: String,
    head: String,
    /// The commits before the head, oldest first.
    commits: Vec<String>,
    requested: Vec<String>,
    reviews: Vec<(String, &'static str)>,
    comments: Vec<(String, String)>,
    updated: i64,
    /// The branch it goes onto: main, or another's (`b7`) in a stack.
    base: String,
}

/// A host's pull request API on a port of its own.
struct Sim {
    host: Host,
    name: String,
    port: u16,
    pulls: Mutex<BTreeMap<u64, Pull>>,
    /// Requests the host counts against its rate limit.
    counted: AtomicUsize,
    /// Requests answered 304, which it doesn't.
    free: AtomicUsize,
    /// The paths of those it counted.
    asked: Mutex<Vec<String>>,
    /// Signed commits: whether the host verified each, its word for it, and who signed it.
    signed: Mutex<BTreeMap<String, (bool, &'static str, &'static str)>>,
    /// A bare repository it serves as `/<name>.git` over git's dumb HTTP (its files as they are,
    /// after `git update-server-info`), for a test with real git behind it.
    git: Mutex<Option<std::path::PathBuf>>,
    /// GitHub's update-branch calls it took: the pull request and its `expected_head_sha`.
    updates: Mutex<Vec<(u64, String)>>,
}

/// When GitHub verified a signed commit, as it was pushed.
const VERIFIED_AT: i64 = 1_800_000_000;

fn stamp(t: i64) -> String {
    kiln::deploy::repo::rfc3339(t)
}

impl Sim {
    fn start(host: Host, name: &str) -> anyhow::Result<Arc<Sim>> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let sim = Arc::new(Sim {
            host,
            name: name.into(),
            port: listener.local_addr()?.port(),
            pulls: Mutex::default(),
            counted: AtomicUsize::new(0),
            free: AtomicUsize::new(0),
            asked: Mutex::default(),
            signed: Mutex::default(),
            git: Mutex::default(),
            updates: Mutex::default(),
        });
        let s = Arc::clone(&sim);
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let _ = s.serve(stream);
            }
        });
        Ok(sim)
    }

    fn add(&self, p: Pull) {
        if let Ok(mut pulls) = self.pulls.lock() {
            pulls.insert(p.number, p);
        }
    }

    fn change(&self, n: u64, f: impl FnOnce(&mut Pull)) {
        if let Some(p) = self.pulls.lock().ok().as_mut().and_then(|ps| ps.get_mut(&n)) {
            f(p);
        }
    }

    fn counted(&self) -> usize {
        self.counted.load(Ordering::Relaxed)
    }

    /// Commit `sha` signed: `verified` by the host or not, in its word, by `by`.
    fn sign(&self, sha: &str, verified: bool, word: &'static str, by: &'static str) {
        if let Ok(mut m) = self.signed.lock() {
            m.insert(sha.to_owned(), (verified, word, by));
        }
    }

    /// The counted requests whose paths hold `part`.
    fn asked(&self, part: &str) -> usize {
        self.asked.lock().map(|a| a.iter().filter(|p| p.contains(part)).count()).unwrap_or(0)
    }

    /// One of a pull request's commits as GitHub lists it, with what it verified of it (as pushed,
    /// at [`VERIFIED_AT`]).
    fn gh_commit(&self, sha: &str) -> Value {
        let signed = self.signed.lock().ok().and_then(|m| m.get(sha).copied());
        let verification = match signed {
            Some((verified, reason, _)) => json!({ "verified": verified, "reason": reason, "signature": "-----BEGIN PGP SIGNATURE-----", "payload": "tree", "verified_at": if verified { json!(stamp(VERIFIED_AT)) } else { Value::Null } }),
            None => json!({ "verified": false, "reason": "unsigned", "signature": null, "payload": null, "verified_at": null }),
        };
        json!({ "sha": sha, "commit": { "verification": verification }, "committer": signed.filter(|(v, ..)| *v).map(|(.., by)| json!({ "login": by })) })
    }

    fn serve(&self, stream: std::net::TcpStream) -> anyhow::Result<()> {
        let mut reader = BufReader::new(stream.try_clone()?);
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let mut parts = line.split_whitespace();
        let (method, path) = (parts.next().unwrap_or_default().to_owned(), parts.next().unwrap_or_default().to_owned());
        let (mut len, mut auth, mut etag) = (0usize, String::new(), None);
        loop {
            let mut h = String::new();
            if reader.read_line(&mut h)? == 0 || h.trim().is_empty() {
                break;
            }
            let (k, v) = h.split_once(':').unwrap_or((&h, ""));
            match k.trim().to_ascii_lowercase().as_str() {
                "content-length" => len = v.trim().parse().unwrap_or(0),
                "authorization" => auth = v.trim().to_owned(),
                "if-none-match" => etag = Some(v.trim().to_owned()),
                _ => {}
            }
        }
        let mut body = vec![0u8; len];
        reader.read_exact(&mut body)?;
        let git = self.git.lock().ok().and_then(|g| g.clone());
        if let (Some(dir), Some(file)) = (git, path.split('?').next().and_then(|p| p.strip_prefix(&format!("/{}.git/", self.name)))) {
            let mut s = stream;
            match std::fs::read(dir.join(file)) {
                Ok(bytes) => {
                    write!(s, "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", bytes.len())?;
                    s.write_all(&bytes)?;
                }
                Err(_) => write!(s, "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")?,
            }
            return Ok(());
        }
        // GitHub's update-branch (W26-R16's Update): taken, its base merged in the background.
        if method == "PUT"
            && let Some(n) = path.strip_prefix(&format!("/repos/{}/pulls/", self.name)).and_then(|r| r.strip_suffix("/update-branch")).and_then(|n| n.parse::<u64>().ok())
        {
            let expected = serde_json::from_slice::<Value>(&body).ok().and_then(|v| v.get("expected_head_sha").and_then(Value::as_str).map(str::to_owned)).unwrap_or_default();
            if let Ok(mut u) = self.updates.lock() {
                u.push((n, expected));
            }
            self.counted.fetch_add(1, Ordering::Relaxed);
            let mut s = stream;
            write!(s, "HTTP/1.1 202 Accepted\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}")?;
            return Ok(());
        }
        let (status, reply) = self.answer(&method, &path, &auth);
        let text = if reply.is_null() { String::new() } else { reply.to_string() };
        let mut h = std::collections::hash_map::DefaultHasher::new();
        text.hash(&mut h);
        let tag = format!("\"{:016x}\"", h.finish());
        let mut s = stream;
        if status == 200 && etag.as_deref() == Some(tag.as_str()) {
            self.free.fetch_add(1, Ordering::Relaxed);
            write!(s, "HTTP/1.1 304 Not Modified\r\nETag: {tag}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")?;
            return Ok(());
        }
        self.counted.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut a) = self.asked.lock() {
            a.push(path);
        }
        write!(s, "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nETag: {tag}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}", text.len())?;
        Ok(())
    }

    fn pull_json(&self, p: &Pull) -> Value {
        let web = format!("http://127.0.0.1:{}", self.port);
        let reviewers: Vec<Value> = p
            .requested
            .iter()
            .map(|r| match self.host {
                Host::Github => json!({ "login": r }),
                Host::Gitlab => json!({ "username": r }),
                Host::Bitbucket => json!({ "nickname": r }),
            })
            .collect();
        match self.host {
            Host::Github => json!({ "number": p.number, "title": p.title, "user": { "login": p.author }, "state": "open", "draft": false, "merged_at": null,
                "head": { "ref": format!("b{}", p.number), "sha": p.head }, "base": { "ref": p.base }, "html_url": format!("{web}/{}/pull/{}", self.name, p.number),
                "updated_at": stamp(p.updated), "requested_reviewers": reviewers }),
            Host::Gitlab => json!({ "iid": p.number, "title": p.title, "author": { "username": p.author }, "state": "opened", "draft": false, "source_branch": format!("b{}", p.number),
                "sha": p.head, "target_branch": "main", "web_url": format!("{web}/{}/-/merge_requests/{}", self.name, p.number), "updated_at": stamp(p.updated), "reviewers": reviewers }),
            Host::Bitbucket => json!({ "id": p.number, "title": p.title, "author": { "nickname": p.author }, "state": "OPEN",
                "source": { "branch": { "name": format!("b{}", p.number) }, "commit": { "hash": p.head } }, "destination": { "branch": { "name": "main" } },
                "links": { "html": { "href": format!("{web}/{}/pull-requests/{}", self.name, p.number) } }, "updated_on": stamp(p.updated), "reviewers": reviewers,
                "participants": p.reviews.iter().map(|(by, v)| json!({ "user": { "nickname": by }, "approved": *v == "approve", "state": if *v == "request_changes" { "changes_requested" } else { "approved" } })).collect::<Vec<_>>() }),
        }
    }

    /// Where the host keeps a repository's pull requests.
    fn prefix(&self) -> String {
        match self.host {
            Host::Github => format!("/repos/{}/pulls", self.name),
            Host::Gitlab => format!("/api/v4/projects/{}/merge_requests", self.name.replace('/', "%2F")),
            Host::Bitbucket => format!("/2.0/repositories/{}/pullrequests", self.name),
        }
    }

    fn answer(&self, method: &str, path: &str, auth: &str) -> (u16, Value) {
        let path = path.split('?').next().unwrap_or_default();
        // GitHub's App: its installation and a token for it.
        if self.host == Host::Github && auth.starts_with("Bearer ey") {
            return match (method, path) {
                ("GET", p) if p == format!("/repos/{}/installation", self.name) => (200, json!({ "id": 7 })),
                ("POST", "/app/installations/7/access_tokens") => (201, json!({ "token": "ghs_t", "expires_at": "2099-01-01T00:00:00Z" })),
                _ => (404, json!({ "message": "Not Found" })),
            };
        }
        let token = match self.host {
            Host::Github => "Bearer ghs_t",
            Host::Gitlab => "Bearer glpat-t",
            Host::Bitbucket => "Bearer bb-t",
        };
        if auth != token {
            return (401, json!({ "message": "401 Unauthorized" }));
        }
        // GitLab's signature of a commit: what it verified of it, or a 404 where it's unsigned.
        if let Some(sha) = path.strip_prefix(&format!("/api/v4/projects/{}/repository/commits/", self.name.replace('/', "%2F"))).and_then(|r| r.strip_suffix("/signature")).filter(|_| self.host == Host::Gitlab && method == "GET") {
            return match self.signed.lock().ok().and_then(|m| m.get(sha).copied()) {
                Some((verified, status, by)) => (200, json!({ "signature_type": "PGP", "verification_status": status, "gpg_key_user_email": if verified { json!(by) } else { Value::Null }, "commit_source": "gitaly" })),
                None => (404, json!({ "message": "404 Signature Not Found" })),
            };
        }
        let Ok(pulls) = self.pulls.lock() else { return (500, Value::Null) };
        let Some(rest) = path.strip_prefix(&self.prefix()).filter(|_| method == "GET") else { return (404, json!({ "message": "Not Found" })) };
        if rest.is_empty() {
            let all: Vec<Value> = pulls.values().map(|p| self.pull_json(p)).collect();
            return (200, if self.host == Host::Bitbucket { json!({ "values": all }) } else { json!(all) });
        }
        let mut segs = rest.trim_start_matches('/').splitn(2, '/');
        let Some(p) = segs.next().and_then(|n| n.parse::<u64>().ok()).and_then(|n| pulls.get(&n)) else { return (404, json!({ "message": "Not Found" })) };
        let what = segs.next().unwrap_or_default();
        let body = match (self.host, what) {
            (_, "") => self.pull_json(p),
            (Host::Github, "commits") => json!(p.commits.iter().chain(std::iter::once(&p.head)).map(|s| self.gh_commit(s)).collect::<Vec<_>>()),
            (Host::Gitlab, "commits") => json!(std::iter::once(&p.head).chain(p.commits.iter().rev()).map(|s| json!({ "id": s })).collect::<Vec<_>>()),
            (Host::Bitbucket, "commits") => json!({ "values": std::iter::once(&p.head).chain(p.commits.iter().rev()).map(|s| json!({ "hash": s })).collect::<Vec<_>>() }),
            (Host::Github, "reviews") => json!(
                p.reviews
                    .iter()
                    .map(|(by, v)| json!({ "user": { "login": by }, "state": match *v { "approve" => "APPROVED", "request_changes" => "CHANGES_REQUESTED", _ => "COMMENTED" }, "submitted_at": stamp(p.updated) }))
                    .collect::<Vec<_>>()
            ),
            (Host::Gitlab, "approvals") => json!({ "approved_by": p.reviews.iter().filter(|(_, v)| *v == "approve").map(|(by, _)| json!({ "user": { "username": by } })).collect::<Vec<_>>() }),
            (Host::Github, "comments") => json!(p.comments.iter().map(|(by, body)| json!({ "user": { "login": by }, "body": body, "position": 3, "in_reply_to_id": null })).collect::<Vec<_>>()),
            (Host::Gitlab, "discussions") => json!(p.comments.iter().map(|(by, body)| json!({ "notes": [{ "author": { "username": by }, "body": body, "resolvable": true, "resolved": false }] })).collect::<Vec<_>>()),
            (Host::Bitbucket, "comments") => {
                json!({ "values": p.comments.iter().map(|(by, body)| json!({ "user": { "nickname": by }, "content": { "raw": body }, "inline": { "path": "a.rs" }, "parent": null })).collect::<Vec<_>>() })
            }
            _ => return (404, json!({ "message": "Not Found" })),
        };
        (200, body)
    }
}

fn sha(prefix: u64, n: u64) -> String {
    format!("{prefix:07}{n:033}")
}

fn pull(n: u64, t: i64) -> Pull {
    Pull { number: n, title: format!("change {n}"), author: "ada".into(), head: sha(n, 1), commits: vec![sha(n, 900)], requested: vec!["bob".into()], reviews: vec![], comments: vec![], updated: t, base: "main".into() }
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

fn s<'a>(v: &'a Value, p: &str) -> &'a str {
    v.pointer(p).and_then(Value::as_str).unwrap_or_default()
}

/// W26-AC01 (the hosts' part): a GitHub pull request, a GitLab merge request and a Bitbucket pull
/// request from their simulators are Ship items in /api/ship/commits, by commit: each head carries
/// its review (bob asked, his thread the line), its other commit folded under it, every item in
/// Ship's order (newest first). A GitLab webhook has its merge request read again once its burst
/// settles, well before the minute's list would.
#[test]
fn w26_ac01_github_gitlab_and_bitbucket_pull_requests_are_ship_items() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let d = dir.path().display().to_string();
    let kp = aws_lc_rs::rsa::KeyPair::generate(aws_lc_rs::rsa::KeySize::Rsa2048).map_err(|_| anyhow::anyhow!("keygen"))?;
    let der = aws_lc_rs::encoding::AsDer::<aws_lc_rs::encoding::Pkcs8V1Der>::as_der(&kp).map_err(|_| anyhow::anyhow!("der"))?;
    use base64::Engine as _;
    std::fs::write(dir.path().join("app.pem"), format!("-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n", base64::engine::general_purpose::STANDARD.encode(der.as_ref())))?;
    for (f, v) in [("gl-token", "glpat-t"), ("gl-hook", "gl-s3cret"), ("bb-token", "bb-t"), ("bb-hook", "bb-s3cret")] {
        std::fs::write(dir.path().join(f), format!("{v}\n"))?;
    }
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs();
    let now = i64::try_from(now)?;
    let (gh, gl, bb) = (Sim::start(Host::Github, "acme/gh")?, Sim::start(Host::Gitlab, "acme/gl")?, Sim::start(Host::Bitbucket, "acme/bb")?);
    for sim in [&gh, &gl, &bb] {
        sim.add(Pull { comments: vec![("bob".into(), "empty matrix meant?\n\nGitHub skips it.".into())], ..pull(5, now) });
    }
    let ci = format!("github = {{ app_id = 99, private_key_file = \"{d}/app.pem\", api_url = \"http://127.0.0.1:{0}\", web_url = \"http://127.0.0.1:{0}\" }}", gh.port);
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
"#,
        gh = gh.port,
        gl = gl.port,
        bb = bb.port,
    );
    let srv = start(dir.path(), &Options { ci: &ci, extra: &extra, ..Options::default() })?;
    let (port, admin) = (srv.port, srv.admin.as_str());
    let commits = |q: &str| -> anyhow::Result<Vec<Value>> {
        let (_, body) = http(port, "GET", &format!("/api/ship/commits?{q}"), admin, None)?;
        let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
        Ok(v.get("commits").and_then(Value::as_array).cloned().unwrap_or_default())
    };
    until(30, "each host's pull request in Ship", || {
        let all = commits("")?;
        Ok((all.iter().filter(|c| c.get("change").is_some_and(|ch| !ch.is_null())).count() < 6).then(|| format!("{all:?}\n{}", srv.log())))
    })?;

    let all = commits("")?;
    let cursors: Vec<i64> = all.iter().map(|c| c.get("cursor").and_then(Value::as_i64).unwrap_or(0)).collect();
    assert!(cursors.windows(2).all(|w| w.first() > w.get(1)), "newest first, in Ship's order: {cursors:?}");
    for (repo, host, name) in [("acme/gh", "github", "#5"), ("acme/gl", "gitlab", "!5"), ("acme/bb", "bitbucket", "#5")] {
        let rows = commits(&format!("repo={repo}"))?;
        let got: Vec<(&str, &str, &str, &str)> = rows.iter().map(|c| (s(c, "/sha"), s(c, "/host"), s(c, "/kind"), s(c, "/fold_under"))).collect();
        let (head, other) = (sha(5, 1), sha(5, 900));
        assert_eq!(got, vec![(head.as_str(), host, "request", ""), (other.as_str(), host, "request", head.as_str())], "{repo}: the head first, carrying the review; its other commit folded under it");
        let tip = rows.first().cloned().unwrap_or_default();
        assert_eq!(s(&tip, "/change/name"), name, "{repo}");
        assert_eq!(s(&tip, "/change/state"), "requested", "{repo}: {tip}");
        assert_eq!(tip.pointer("/change/turn"), Some(&json!([format!("{host}:bob")])), "{repo}: bob's turn");
        assert_eq!(s(&tip, "/line"), "bob: empty matrix meant? · 1 thread", "{repo}");
        let kiln_only = commits(&format!("repo={repo}&host=kiln"))?;
        assert!(kiln_only.is_empty(), "{repo}: host filters");
        let requests = commits(&format!("repo={repo}&kind=request&host={host}"))?;
        assert_eq!(requests.len(), 2, "{repo}: kind and host filter");
    }

    // linus approves on GitLab and its webhook comes: the merge request is read again once settled.
    gl.change(5, |p| {
        p.reviews.push(("linus".into(), "approve"));
        p.updated = now + 1;
    });
    let hook = json!({ "object_kind": "merge_request", "object_attributes": { "iid": 5 }, "project": { "path_with_namespace": "acme/gl", "web_url": format!("http://127.0.0.1:{}/acme/gl", gl.port) } }).to_string();
    let mut c = std::net::TcpStream::connect(("127.0.0.1", port))?;
    write!(c, "POST /api/hooks/gitlab HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nX-Gitlab-Event: Merge Request Hook\r\nX-Gitlab-Token: gl-s3cret\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{hook}", hook.len())?;
    let mut raw = String::new();
    c.read_to_string(&mut raw)?;
    assert!(raw.starts_with("HTTP/1.1 202"), "{raw}");
    until(40, "GitLab's approval, from its webhook", || {
        let tip = commits("repo=acme/gl")?.first().cloned().unwrap_or_default();
        Ok((s(&tip, "/change/state") != "approved").then(|| tip.to_string()))
    })?;
    Ok(())
}

/// W26-AC09: an installation with 50 open pull requests, over a simulated hour (a passed-in
/// clock) of webhooks (a push, a review and a comment on each, at scattered times) and polling
/// with ETags, costs at most 600 requests the GitHub simulator counts (304s free; the limit is
/// 5,000), and the mirror ends as GitHub is.
#[test]
fn w26_ac09_fifty_pull_requests_an_hour_within_600_github_requests() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = kiln::store::Store::open(&dir.path().join("kiln.db"))?;
    let sim = Sim::start(Host::Github, "acme/app")?;
    let t0: i64 = 1_800_000_000;
    for n in 1..=50 {
        sim.add(pull(n, t0));
    }
    let fetch = mirror::Http { base: format!("http://127.0.0.1:{}", sim.port), authorization: "Bearer ghs_t".into() };
    let r = Mirrored { repo_id: "gh".into(), host: mirror::Host::Github, name: "acme/app".into() };
    // Each pull request's three events, seconds into the hour: a push, a review, a comment.
    let mut events: Vec<(i64, u64, u8)> = Vec::new();
    for n in 1..=50u64 {
        let k = i64::try_from(n)?;
        events.extend([((k * 71) % 3600, n, 0), ((k * 113 + 600) % 3600, n, 1), ((k * 157 + 1200) % 3600, n, 2)]);
    }
    events.sort();
    let sim2 = Arc::clone(&sim);
    let got: BTreeMap<i64, (String, i64, i64)> = kiln::rt::block_on(async move {
        let sim = sim2;
        let mut st = State::default();
        let mut next = events.iter().peekable();
        for s in 0..3600i64 {
            let now = (t0 + s) * 1000;
            while let Some(&&(_, n, kind)) = next.peek().filter(|(at, ..)| *at <= s) {
                sim.change(n, |p| {
                    p.updated = t0 + s;
                    match kind {
                        0 => {
                            p.commits.push(p.head.clone());
                            p.head = sha(n, 2 + u64::try_from(s).unwrap_or(0));
                        }
                        1 => p.reviews.push(("bob".into(), "request_changes")),
                        _ => p.comments.push(("bob".into(), "empty matrix meant?".into())),
                    }
                });
                st.mirror.hook(n, now);
                next.next();
            }
            mirror::step(&store, &r, &fetch, &mut st, now).await?;
        }
        // The mirror as it ends: each pull request's head, reviews and threads.
        store
            .call(|c| {
                let mut q = c.prepare("SELECT p.number, (SELECT h.sha FROM host_pull_heads h WHERE h.repo_id = p.repo_id AND h.host = p.host AND h.number = p.number ORDER BY h.n DESC LIMIT 1), json_array_length(p.reviews), p.threads FROM host_pulls p")?;
                let rows = q.query_map([], |r| Ok((r.get::<_, i64>(0)?, (r.get::<_, String>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?))))?;
                Ok(rows.collect::<Result<BTreeMap<_, _>, _>>()?)
            })
            .await
    })?;
    let (counted, free) = (sim.counted(), sim.free.load(Ordering::Relaxed));
    println!("w26_ac09: 50 pull requests, a simulated hour: {counted} GitHub requests counted, {free} answered 304 (free)");
    assert!(counted <= 600, "{counted} GitHub requests in an hour for 50 pull requests: more than 600");
    // The mirror ends as GitHub is: every head, every review, every thread.
    let want: BTreeMap<i64, (String, i64, i64)> = sim.pulls.lock().map_err(|_| anyhow::anyhow!("poisoned"))?.values().map(|p| (i64::try_from(p.number).unwrap_or(0), (p.head.clone(), i64::try_from(p.reviews.len()).unwrap_or(0), i64::try_from(p.comments.len()).unwrap_or(0)))).collect();
    assert_eq!(got, want);
    Ok(())
}

/// W27-R03: for a host's pull request kiln records what the host verified of each of its commits
/// and marks what it couldn't. GitHub's verdicts come on the commits the mirror reads already (no
/// request more); GitLab is asked of each commit's signature once (a push later asks of its new
/// commit alone); Bitbucket verifies none, so each of its commits is kept unknown, saying so. Read
/// back as the review page has them: each commit oldest first, the head last.
#[test]
fn w27_r03_kiln_records_what_each_host_verified_of_a_pull_requests_commits() -> anyhow::Result<()> {
    let t0: i64 = 1_800_000_000;
    for host in [Host::Github, Host::Gitlab, Host::Bitbucket] {
        let dir = tempfile::tempdir()?;
        let store = kiln::store::Store::open(&dir.path().join("kiln.db"))?;
        let sim = Sim::start(host, "acme/app")?;
        let (good, unknown_key, head, pushed) = (sha(5, 900), sha(5, 901), sha(5, 1), sha(5, 2));
        sim.add(Pull { commits: vec![good.clone(), unknown_key.clone()], ..pull(5, t0) });
        let (word, signer, base, auth, mhost, hword) = match host {
            Host::Github => ("valid", "ada", format!("http://127.0.0.1:{}", sim.port), "Bearer ghs_t", mirror::Host::Github, "github"),
            Host::Gitlab => ("verified", "ada@example.com", format!("http://127.0.0.1:{}/api/v4", sim.port), "Bearer glpat-t", mirror::Host::Gitlab, "gitlab"),
            Host::Bitbucket => ("verified", "ada", format!("http://127.0.0.1:{}/2.0", sim.port), "Bearer bb-t", mirror::Host::Bitbucket, "bitbucket"),
        };
        sim.sign(&good, true, word, signer);
        sim.sign(&unknown_key, false, "unknown_key", "");
        let fetch = mirror::Http { base, authorization: auth.into() };
        let r = Mirrored { repo_id: "r".into(), host: mhost, name: "acme/app".into() };
        let (sim2, head2, pushed2) = (Arc::clone(&sim), head.clone(), pushed.clone());
        let kept = kiln::rt::block_on(async move {
            let (sim, head, pushed) = (sim2, head2, pushed2);
            let mut st = State::default();
            let mut now = t0 * 1000;
            // Until each commit has a verdict kept: `want` commits, the last `last`.
            let settle = async |st: &mut State, now: &mut i64, want: usize, last: &str| -> anyhow::Result<Vec<Value>> {
                for _ in 0..120 {
                    mirror::step(&store, &r, &fetch, st, *now).await?;
                    let got = mirror::signatures(&store, "r", hword, 5).await?;
                    if got.len() == want && got.last().is_some_and(|v| s(v, "/sha") == last) && got.iter().all(|v| !v.get("read_at").is_none_or(Value::is_null)) {
                        return Ok(got);
                    }
                    *now += 1000;
                }
                anyhow::bail!("{hword}: no verdict on each of {want} commits")
            };
            let first = settle(&mut st, &mut now, 3, &head).await?;
            let asked_first = sim.asked("/signature");
            sim.change(5, |p| {
                p.commits.push(p.head.clone());
                p.head = pushed.clone();
                p.updated = t0 + 30;
            });
            st.mirror.hook(5, now);
            let second = settle(&mut st, &mut now, 4, &pushed).await?;
            anyhow::Ok((first, asked_first, second))
        })?;
        let (first, asked_first, second) = kept;
        let said: Vec<(&str, &str, &str, &str)> = first.iter().map(|v| (s(v, "/sha"), s(v, "/state"), s(v, "/reason"), s(v, "/signer"))).collect();
        let want: Vec<(&str, &str, &str, &str)> = match host {
            Host::Bitbucket => [&good, &unknown_key, &head].iter().map(|c| (c.as_str(), "unknown", "Bitbucket verifies no commit's signature", "")).collect(),
            _ => vec![(good.as_str(), "verified", word, signer), (unknown_key.as_str(), "unverified", "unknown_key", ""), (head.as_str(), "unsigned", "unsigned", "")],
        };
        assert_eq!(said, want, "{hword}: each commit's verdict, oldest first, the head last");
        let at = first.first().and_then(|v| v.get("verified_at")).and_then(Value::as_i64);
        assert_eq!(at, (host == Host::Github).then_some(VERIFIED_AT), "{hword}: when the host verified it, where it says");
        assert_eq!(second.iter().map(|v| s(v, "/sha")).collect::<Vec<_>>(), vec![good.as_str(), unknown_key.as_str(), head.as_str(), pushed.as_str()], "{hword}: after a push");
        assert_eq!(second.get(..3), Some(first.as_slice()), "{hword}: verdicts kept as first read");
        match host {
            Host::Gitlab => assert_eq!((asked_first, sim.asked("/signature")), (3, 4), "GitLab: each commit's signature asked once"),
            _ => assert_eq!(sim.asked("/signature"), 0, "{hword}: no request more for its verdicts"),
        }
    }
    Ok(())
}

/// `git` with no configuration but this test's, in `dir`: whether it went, and what it printed.
fn git(dir: &std::path::Path, args: &[&str]) -> anyhow::Result<String> {
    let o = std::process::Command::new("git")
        .args(["-c", "user.name=Test", "-c", "user.email=test@example.com", "-c", "init.defaultBranch=main"])
        .args(args)
        .current_dir(dir)
        .env("HOME", dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()?;
    anyhow::ensure!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
    Ok(String::from_utf8_lossy(&o.stdout).trim().to_owned())
}

/// Whether Ship shows commit `sha` of acme/gh as a pull request's (kiln's mirror has read it), not
/// only as a commit the git poller saw.
fn mirrored(srv: &common::Server, sha: &str) -> anyhow::Result<bool> {
    let (_, body) = http(srv.port, "GET", "/api/ship/commits?repo=acme/gh", &srv.admin, None)?;
    let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    Ok(v.get("commits").and_then(Value::as_array).into_iter().flatten().any(|c| c.get("sha") == Some(&json!(sha)) && c.get("change").is_some_and(|ch| !ch.is_null())))
}

/// The directory named `name` under `dir`, at any depth.
fn find_dir(dir: &std::path::Path, name: &str) -> Option<std::path::PathBuf> {
    for e in std::fs::read_dir(dir).ok()?.flatten() {
        let p = e.path();
        if p.is_dir() {
            if p.file_name().is_some_and(|n| n == name) {
                return Some(p);
            }
            if let Some(found) = find_dir(&p, name) {
                return Some(found);
            }
        }
    }
    None
}

/// GitHub's simulator with real git behind it: `acme/gh`, its main, and pull request #7's head at
/// refs/pull/7/head, `review` as main's `.kiln/review.yaml` when given; a kiln server mirroring it,
/// once #7 is in Ship. (The temp dir, the server, the simulator, #7's head, the host's work tree
/// and bare repository.)
fn pull_on_github(review: Option<&str>, notify: Option<&str>) -> anyhow::Result<(tempfile::TempDir, common::Server, Arc<Sim>, String, std::path::PathBuf, std::path::PathBuf)> {
    let dir = tempfile::tempdir()?;
    let d = dir.path().display().to_string();
    let kp = aws_lc_rs::rsa::KeyPair::generate(aws_lc_rs::rsa::KeySize::Rsa2048).map_err(|_| anyhow::anyhow!("keygen"))?;
    let der = aws_lc_rs::encoding::AsDer::<aws_lc_rs::encoding::Pkcs8V1Der>::as_der(&kp).map_err(|_| anyhow::anyhow!("der"))?;
    use base64::Engine as _;
    std::fs::write(dir.path().join("app.pem"), format!("-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n", base64::engine::general_purpose::STANDARD.encode(der.as_ref())))?;
    // The host's repository: main, and pull request #7's head at refs/pull/7/head.
    let host_git = dir.path().join("host");
    std::fs::create_dir_all(&host_git)?;
    git(&host_git, &["init", "-q", "--bare", "gh.git"])?;
    let bare = host_git.join("gh.git");
    git(&host_git, &["clone", "-q", &bare.display().to_string(), "work"])?;
    let work = host_git.join("work");
    std::fs::write(work.join("README.md"), "app\n")?;
    std::fs::write(work.join("f.txt"), "one\n")?;
    if let Some(r) = review {
        std::fs::create_dir_all(work.join(".kiln"))?;
        std::fs::write(work.join(".kiln/review.yaml"), r)?;
    }
    if let Some(n) = notify {
        std::fs::create_dir_all(work.join(".kiln"))?;
        std::fs::write(work.join(".kiln/notify.yaml"), n)?;
    }
    git(&work, &["add", "-A"])?;
    git(&work, &["commit", "-qm", "first"])?;
    git(&work, &["push", "-q", "origin", "HEAD:main"])?;
    git(&work, &["checkout", "-qb", "b7"])?;
    std::fs::write(work.join("f.txt"), "one, in #7\n")?;
    git(&work, &["commit", "-qam", "the pull request's change"])?;
    git(&work, &["push", "-q", "origin", "HEAD:refs/pull/7/head"])?;
    let head = git(&work, &["rev-parse", "HEAD"])?;

    git(&bare, &["update-server-info"])?;
    let now = i64::try_from(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs())?;
    let gh = Sim::start(Host::Github, "acme/gh")?;
    if let Ok(mut g) = gh.git.lock() {
        *g = Some(bare.clone());
    }
    gh.add(Pull { head: head.clone(), commits: vec![], ..pull(7, now) });
    let ci = format!("github = {{ app_id = 99, private_key_file = \"{d}/app.pem\", api_url = \"http://127.0.0.1:{0}\", web_url = \"http://127.0.0.1:{0}\" }}", gh.port);
    let extra = format!("[[repos]]\nname = \"acme/gh\"\nclone_url = \"http://127.0.0.1:{}/acme/gh.git\"\npoll_seconds = 1\n", gh.port);
    let srv = start(dir.path(), &Options { ci: &ci, extra: &extra, ..Options::default() })?;
    until(120, "#7 mirrored into Ship as a pull request", || Ok((!mirrored(&srv, &head)?).then(|| "not yet".to_owned())))?;
    std::thread::sleep(Duration::from_secs(2));
    Ok((dir, srv, gh, head, work, bare))
}

/// W26-R16 on a host's pull request, real git behind GitHub's simulator: when the base branch moves,
/// kiln replays pull request #7 onto it in its own mirror (its base merged into its head, kiln the
/// committer) and keeps it at `refs/kiln/pulls/github/7/replays/1`; nothing moves on the host.
#[test]
fn w26_r16_a_pull_request_is_replayed_in_kilns_mirror() -> anyhow::Result<()> {
    let (dir, srv, gh, head, work, bare) = pull_on_github(None, None)?;
    let (port, admin) = (srv.port, srv.admin.as_str());

    // main moves on the host.
    git(&work, &["checkout", "-q", "main"])?;
    std::fs::write(work.join("z.txt"), "elsewhere\n")?;
    git(&work, &["add", "z.txt"])?;
    git(&work, &["commit", "-qm", "main moves"])?;
    git(&work, &["push", "-q", "origin", "HEAD:main"])?;
    git(&bare, &["update-server-info"])?;
    let tip = git(&work, &["rev-parse", "HEAD"])?;
    let host_refs = git(&bare, &["for-each-ref", "--format=%(refname) %(objectname)"])?;

    let mut replay = String::new();
    until(60, "#7 replayed in kiln's mirror", || {
        let Some(mirror) = find_dir(dir.path(), "acme__gh.git") else { return Ok(Some("no mirror yet".into())) };
        match git(&mirror, &["rev-parse", "--verify", "--quiet", "refs/kiln/pulls/github/7/replays/1"]) {
            Ok(c) if !c.is_empty() => {
                replay = c;
                Ok(None)
            }
            _ => Ok(Some(srv.log())),
        }
    })?;
    let mirror = find_dir(dir.path(), "acme__gh.git").ok_or_else(|| anyhow::anyhow!("no mirror"))?;
    let shape = git(&mirror, &["log", "-1", "--format=%P|%cn|%s", &replay])?;
    assert_eq!(shape, format!("{head} {tip}|kiln|Merge main@{} into #7", tip.get(..7).unwrap_or_default()), "its base merged into its head, by kiln");
    assert_eq!(git(&mirror, &["show", &format!("{replay}:f.txt")])?, "one, in #7");
    assert_eq!(git(&mirror, &["show", &format!("{replay}:z.txt")])?, "elsewhere");
    assert_eq!(git(&bare, &["for-each-ref", "--format=%(refname) %(objectname)"])?, host_refs, "nothing moved on the host");
    assert!(gh.updates.lock().map(|u| u.is_empty()).unwrap_or(false), "nothing asked of GitHub until Update");
    // How fresh #7 is, as its page says it, and Update ready to go.
    let (st, body) = http(port, "GET", "/api/ci/repos/acme%2Fgh/pulls/7/fresh", admin, None)?;
    assert_eq!(st, 200, "{body}");
    let f: Value = serde_json::from_str(&body)?;
    assert_eq!((s(&f, "/words"), f.pointer("/update/can")), (format!("replays clean on main@{}", tip.get(..6).unwrap_or_default()).as_str(), Some(&json!(true))), "{f}");

    // Update: GitHub's update-branch, against the head the replay was made from.
    let (st, body) = http(port, "POST", "/api/ci/repos/acme%2Fgh/pulls/7/update", admin, Some(&json!({})))?;
    assert_eq!(st, 200, "{body}");
    let r: Value = serde_json::from_str(&body)?;
    assert_eq!((s(&r, "/host"), s(&r, "/state")), ("github", "updating"), "{r}");
    assert_eq!(gh.updates.lock().map(|u| u.clone()).unwrap_or_default(), vec![(7, head.clone())]);
    Ok(())
}

/// W26-R16 on a host's pull request with `fresh.push: auto`: as main moves, kiln pushes #7's replay
/// to GitHub by itself (update-branch against the head it was made from), nobody pressing Update.
#[test]
fn w26_r16_a_pull_request_is_pushed_automatically_when_declared() -> anyhow::Result<()> {
    let (_dir, _srv, gh, head, work, bare) = pull_on_github(Some("fresh:\n  push: auto\n"), None)?;
    git(&work, &["checkout", "-q", "main"])?;
    std::fs::write(work.join("z.txt"), "elsewhere\n")?;
    git(&work, &["add", "z.txt"])?;
    git(&work, &["commit", "-qm", "main moves"])?;
    git(&work, &["push", "-q", "origin", "HEAD:main"])?;
    git(&bare, &["update-server-info"])?;
    until(60, "#7 pushed to GitHub by itself", || {
        let u = gh.updates.lock().map(|u| u.clone()).unwrap_or_default();
        Ok((u != vec![(7, head.clone())]).then(|| format!("{u:?}")))
    })?;
    Ok(())
}

/// W26-R16 on a host's stack: #8 goes onto #7's branch (b7), as GitHub's stacked pull requests do.
/// When main moves, #7 is replayed onto it and #8 onto #7's replay, in kiln's mirror; Update on #8
/// is GitHub's update-branch, which merges #8's base as GitHub has it (b7), not kiln's replay: a
/// stack updates bottom first.
#[test]
fn w26_r16_a_hosts_stack_replays_each_onto_the_one_below() -> anyhow::Result<()> {
    let (dir, srv, gh, head7, work, bare) = pull_on_github(None, None)?;
    // #8 on b7.
    git(&work, &["checkout", "-qb", "b8"])?;
    std::fs::write(work.join("g.txt"), "eight\n")?;
    git(&work, &["add", "g.txt"])?;
    git(&work, &["commit", "-qm", "#8 on #7"])?;
    git(&work, &["push", "-q", "origin", "HEAD:refs/pull/8/head"])?;
    git(&bare, &["update-server-info"])?;
    let head8 = git(&work, &["rev-parse", "HEAD"])?;
    let now = i64::try_from(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs())?;
    gh.add(Pull { head: head8.clone(), commits: vec![], base: "b7".into(), ..pull(8, now) });
    until(120, "#8 mirrored into Ship as a pull request", || Ok((!mirrored(&srv, &head8)?).then(|| "not yet".to_owned())))?;
    std::thread::sleep(Duration::from_secs(2));

    // main moves.
    git(&work, &["checkout", "-q", "main"])?;
    std::fs::write(work.join("z.txt"), "elsewhere\n")?;
    git(&work, &["add", "z.txt"])?;
    git(&work, &["commit", "-qm", "main moves"])?;
    git(&work, &["push", "-q", "origin", "HEAD:main"])?;
    git(&bare, &["update-server-info"])?;
    let tip = git(&work, &["rev-parse", "HEAD"])?;

    let replay = |n: u64| -> Option<String> {
        let mirror = find_dir(dir.path(), "acme__gh.git")?;
        git(&mirror, &["rev-parse", "--verify", "--quiet", &format!("refs/kiln/pulls/github/{n}/replays/1")]).ok().filter(|c| !c.is_empty())
    };
    until(60, "#7 and #8 replayed", || Ok((replay(7).is_none() || replay(8).is_none()).then(|| srv.log())))?;
    let mirror = find_dir(dir.path(), "acme__gh.git").ok_or_else(|| anyhow::anyhow!("no mirror"))?;
    let (r7, r8) = (replay(7).unwrap_or_default(), replay(8).unwrap_or_default());
    assert_eq!(git(&mirror, &["log", "-1", "--format=%P", &r7])?, format!("{head7} {tip}"), "#7: main merged into its head");
    assert_eq!(git(&mirror, &["log", "-1", "--format=%P|%cn|%s", &r8])?, format!("{head8} {r7}|kiln|Merge #7 into #8"), "#8: #7's replay merged into its head");
    assert_eq!(git(&mirror, &["show", &format!("{r8}:z.txt")])?, "elsewhere", "#8's replay has main's move, through #7's");

    // Update on #8: GitHub's update-branch against #8's head.
    let (st, body) = http(srv.port, "POST", "/api/ci/repos/acme%2Fgh/pulls/8/update", &srv.admin, Some(&json!({})))?;
    assert_eq!(st, 200, "{body}");
    assert!(gh.updates.lock().map(|u| u.contains(&(8, head8.clone()))).unwrap_or(false));
    Ok(())
}

/// W26-R16's collision radar on a host: #7 (ada's) and #8 (lee's), both on main, both change
/// f.txt's line 1. When main moves, each is told once through `change.collision` (here also to the
/// admin's inbox, `inbox:admin`, where the test can read it: the authors are GitHub logins); a
/// second move with the same overlap tells no one.
#[test]
fn w26_r16_the_collision_radar_tells_a_hosts_pull_requests_once() -> anyhow::Result<()> {
    let (_dir, srv, gh, _head7, work, bare) = pull_on_github(None, Some("- on: [change.collision]\n  to: [inbox, inbox:admin]\n"))?;
    git(&work, &["checkout", "-qb", "b8", "main"])?;
    std::fs::write(work.join("f.txt"), "one, in #8\n")?;
    git(&work, &["commit", "-qam", "#8 changes the same line"])?;
    git(&work, &["push", "-q", "origin", "HEAD:refs/pull/8/head"])?;
    git(&bare, &["update-server-info"])?;
    let head8 = git(&work, &["rev-parse", "HEAD"])?;
    let now = i64::try_from(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs())?;
    gh.add(Pull { head: head8.clone(), commits: vec![], author: "lee".into(), ..pull(8, now) });
    until(120, "#8 mirrored into Ship as a pull request", || Ok((!mirrored(&srv, &head8)?).then(|| "not yet".to_owned())))?;
    std::thread::sleep(Duration::from_secs(2));
    let mut n = 0;
    let mut move_main = |what: &str| -> anyhow::Result<()> {
        n += 1;
        git(&work, &["checkout", "-q", "main"])?;
        std::fs::write(work.join("z.txt"), format!("{what} {n}\n"))?;
        git(&work, &["add", "z.txt"])?;
        git(&work, &["commit", "-qm", what])?;
        git(&work, &["push", "-q", "origin", "HEAD:main"])?;
        git(&bare, &["update-server-info"])
            .map(|_| ())
    };
    let told = || -> anyhow::Result<Vec<String>> {
        let (_, body) = http(srv.port, "GET", "/api/review/inbox", &srv.admin, None)?;
        let v: Value = serde_json::from_str(&body)?;
        Ok(v.get("notifications").and_then(Value::as_array).cloned().unwrap_or_default().iter().filter(|n| s(n, "/event") == "change.collision").map(|n| s(n, "/line").to_owned()).collect())
    };
    move_main("main moves")?;
    until(60, "both pull requests told", || {
        let t = told()?;
        Ok((t.len() < 2).then(|| format!("{t:?}\n{}", srv.log().lines().filter(|l| l.contains("radar") || l.contains("collision") || l.contains("replayed") || l.contains("as fresh sees") || l.contains("pull request")).collect::<Vec<_>>().join("\n"))))
    })?;
    let t = told()?;
    assert_eq!(t.len(), 2, "one notice for each pull request: {t:?}");
    assert!(t.iter().any(|l| l.contains("pull:github:7 and pull:github:8 both change f.txt line 1")), "{t:?}");
    assert!(t.iter().any(|l| l.contains("pull:github:8 and pull:github:7 both change f.txt line 1")), "{t:?}");
    // Main moves again, the overlap the same: no one is told twice.
    move_main("main moves again")?;
    std::thread::sleep(Duration::from_secs(8));
    assert_eq!(told()?.len(), 2, "the same overlap tells no one again");
    Ok(())
}
