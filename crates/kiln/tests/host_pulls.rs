//! The hosts' pull requests mirrored into Ship's read model (docs/ci.md W26-R01, R20, AC01, AC09):
//! against simulators of GitHub's, GitLab's and Bitbucket's pull request APIs, shaped as each host
//! shapes them, kiln mirrors each open pull request (its head, its other commits, its reviewers,
//! reviews and threads) and Ship shows it by commit. Each simulator counts the requests its host
//! would count, answering 304 to a matching `If-None-Match` without counting it, as GitHub does.

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
}

fn stamp(t: i64) -> String {
    kiln::deploy::repo::rfc3339(t)
}

impl Sim {
    fn start(host: Host, name: &str) -> anyhow::Result<Arc<Sim>> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let sim = Arc::new(Sim { host, name: name.into(), port: listener.local_addr()?.port(), pulls: Mutex::default(), counted: AtomicUsize::new(0), free: AtomicUsize::new(0) });
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
                "head": { "ref": format!("b{}", p.number), "sha": p.head }, "base": { "ref": "main" }, "html_url": format!("{web}/{}/pull/{}", self.name, p.number),
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
            (Host::Github, "commits") => json!(p.commits.iter().chain(std::iter::once(&p.head)).map(|s| json!({ "sha": s })).collect::<Vec<_>>()),
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
    Pull { number: n, title: format!("change {n}"), author: "ada".into(), head: sha(n, 1), commits: vec![sha(n, 900)], requested: vec!["bob".into()], reviews: vec![], comments: vec![], updated: t }
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
