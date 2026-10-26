//! The forge seam for kiln's hosts (docs/seams.md, "Forge"; docs/ci.md W26-R20): [`HostForge`]
//! implements `kiln_seams::Forge` over GitHub's (kiln's GitHub App), GitLab's and Bitbucket's REST
//! APIs. Stacks are GitHub's Stacks API and GitLab's stacked merge requests (detected from their
//! target branches, one author's, 20 at most, as GitLab detects them); a host's queue is GitHub's
//! merge queue (the asynchronous merge with `merge_queue`, read and left by GraphQL) or GitLab's
//! merge trains; a stale change is updated by GitHub's update-branch against its expected head,
//! GitLab's rebase, or a commit kiln replayed, pushed with a lease; reviews are batched in GitHub's
//! pending review, GitLab's draft notes, or held by kiln for Bitbucket ([`Held`]); GitHub merges in
//! the background (its asynchronous merge, polled).
//!
//! Within the hosts' rate limits: every read is conditional (an `If-None-Match` with the ETag the
//! caller holds; GitHub doesn't count a 304), the limit is read from each answer's rate-limit
//! headers, and a request past it is refused `RateLimited` before it's sent. [`Watch`] follows a
//! repository's open changes with webhooks and ETag polling.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use kiln_seams::forge::*;
use parking_lot::Mutex;
use serde_json::{Value, json};

use super::{GitHost, bitbucket, github, gitlab};
use crate::http::client::Method as Http;
use crate::hub::Hub;
use crate::store::Repo;

/// The REST API version kiln asks GitHub for (its stacks and asynchronous merges are in it).
const GITHUB_API_VERSION: &str = "2026-03-10";
/// Background merges remembered (which changes each merges), oldest let go first.
const MERGES_KEPT: usize = 10_000;
/// Default branches remembered, by repository.
const TRUNKS_KEPT: usize = 1000;
/// What kiln does where a repository has no queue of its host's.
const KILN_QUEUE: &str = "lands it through kiln's merge queue (W28)";
/// What kiln does where a host can't update a change.
const KILN_UPDATE: &str = "replays it in its mirror and pushes it with a lease";
/// What kiln does for stacks on Bitbucket.
const BITBUCKET_STACKS: &str = "chains pull requests by their bases and holds their order with a commit status";

/// How kiln itself is known to a host.
pub enum Cred {
    /// kiln's GitHub App: an installation token for the repository.
    App(Arc<github::App>),
    /// An `Authorization` header (GitLab's token, Bitbucket's credentials).
    Header(String),
}

/// A person's `Authorization` header on the host, by login, when they signed in with it (W23-R08:
/// what they do from kiln is theirs); none: kiln acts.
pub type People = Box<dyn Fn(&str) -> Option<String> + Send + Sync>;
/// Pushes a commit to a branch with a lease: (repository, branch, commit, expected head).
pub type Pusher = Box<dyn Fn(String, String, String, String) -> BoxFuture<'static, Result<(), Refusal>> + Send + Sync>;
/// The time now, in Unix seconds.
pub type Now = Box<dyn Fn() -> u64 + Send + Sync>;

/// Comments kiln holds for a host with no batched reviews of its own (Bitbucket's), until their
/// author publishes them.
pub trait Held: Send + Sync {
    /// Holds `c` (its id given here): its id.
    fn add<'a>(&'a self, repo: &'a str, number: u64, c: Comment) -> BoxFuture<'a, Result<u64, Refusal>>;
    /// `by`'s held comments on change `number`, oldest first.
    fn list<'a>(&'a self, repo: &'a str, number: u64, by: &'a str) -> BoxFuture<'a, Result<Vec<Comment>, Refusal>>;
    /// Lets the held comments `ids` go (published).
    fn remove<'a>(&'a self, ids: &'a [u64]) -> BoxFuture<'a, Result<(), Refusal>>;
}

/// [`Held`] in memory (tests).
#[derive(Default)]
pub struct MemHeld(Mutex<(u64, Vec<(String, u64, Comment)>)>);

impl Held for MemHeld {
    fn add<'a>(&'a self, repo: &'a str, number: u64, c: Comment) -> BoxFuture<'a, Result<u64, Refusal>> {
        let mut g = self.0.lock();
        g.0 = g.0.saturating_add(1);
        let id = g.0;
        g.1.push((repo.to_owned(), number, Comment { id, ..c }));
        Box::pin(async move { Ok(id) })
    }
    fn list<'a>(&'a self, repo: &'a str, number: u64, by: &'a str) -> BoxFuture<'a, Result<Vec<Comment>, Refusal>> {
        let out = self.0.lock().1.iter().filter(|(r, n, c)| r == repo && *n == number && c.author == by).map(|(_, _, c)| c.clone()).collect();
        Box::pin(async move { Ok(out) })
    }
    fn remove<'a>(&'a self, ids: &'a [u64]) -> BoxFuture<'a, Result<(), Refusal>> {
        self.0.lock().1.retain(|(_, _, c)| !ids.contains(&c.id));
        Box::pin(async { Ok(()) })
    }
}

/// [`Held`] in kiln's store (`review_held`).
pub struct StoreHeld(pub Arc<Hub>);

fn store_err(e: anyhow::Error) -> Refusal {
    Refusal::Unavailable(format!("kiln's store: {e:#}"))
}

impl Held for StoreHeld {
    fn add<'a>(&'a self, repo: &'a str, number: u64, c: Comment) -> BoxFuture<'a, Result<u64, Refusal>> {
        let (repo, at) = (repo.to_owned(), crate::util::now_unix());
        Box::pin(async move {
            let n = i64::try_from(number).map_err(|_| Refusal::Invalid { what: "a change's number", why: number.to_string() })?;
            let id: i64 = self
                .0
                .store
                .call(move |db| {
                    db.execute(
                        "INSERT INTO review_held (repo, number, author, body, path, line, at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                        rusqlite::params![repo, n, c.author, c.body, c.line.as_ref().map(|l| l.path.clone()), c.line.as_ref().map(|l| l.line), at],
                    )?;
                    Ok(db.last_insert_rowid())
                })
                .await
                .map_err(store_err)?;
            Ok(u64::try_from(id).unwrap_or_default())
        })
    }
    fn list<'a>(&'a self, repo: &'a str, number: u64, by: &'a str) -> BoxFuture<'a, Result<Vec<Comment>, Refusal>> {
        let (repo, by) = (repo.to_owned(), by.to_owned());
        Box::pin(async move {
            let n = i64::try_from(number).unwrap_or(i64::MAX);
            self.0
                .store
                .call(move |db| {
                    let mut q = db.prepare("SELECT id, author, body, path, line FROM review_held WHERE repo = ?1 AND number = ?2 AND author = ?3 ORDER BY id")?;
                    let rows = q.query_map(rusqlite::params![repo, n, by], |r| {
                        let (path, line): (Option<String>, Option<u32>) = (r.get(3)?, r.get(4)?);
                        Ok(Comment {
                            id: u64::try_from(r.get::<_, i64>(0)?).unwrap_or_default(),
                            author: r.get(1)?,
                            body: r.get(2)?,
                            line: path.zip(line).map(|(path, line)| Line { path, line }),
                            review: None,
                        })
                    })?;
                    Ok(rows.collect::<Result<Vec<_>, _>>()?)
                })
                .await
                .map_err(store_err)
        })
    }
    fn remove<'a>(&'a self, ids: &'a [u64]) -> BoxFuture<'a, Result<(), Refusal>> {
        let ids: Vec<i64> = ids.iter().filter_map(|i| i64::try_from(*i).ok()).collect();
        Box::pin(async move {
            self.0
                .store
                .call(move |db| {
                    let tx = db.transaction()?;
                    for id in ids {
                        tx.execute("DELETE FROM review_held WHERE id = ?1", [id])?;
                    }
                    tx.commit()?;
                    Ok(())
                })
                .await
                .map_err(store_err)
        })
    }
}

#[derive(Default)]
struct St {
    rate: Option<Rate>,
    trunks: BTreeMap<String, String>,
    merges: BTreeMap<String, Vec<u64>>,
    merge_order: VecDeque<String>,
}

/// One host's answer.
struct Reply {
    status: u16,
    body: Value,
    etag: Option<String>,
    location: Option<String>,
}

/// The forge seam on one of kiln's hosts.
pub struct HostForge {
    host: Host,
    api: String,
    graphql: String,
    kiln: Cred,
    people: People,
    push: Pusher,
    now: Now,
    held: Arc<dyn Held>,
    st: Mutex<St>,
}

fn s(v: &Value, p: &str) -> String {
    v.pointer(p).and_then(Value::as_str).unwrap_or_default().to_owned()
}

fn u(v: &Value, p: &str) -> u64 {
    v.pointer(p).and_then(Value::as_u64).unwrap_or_default()
}

fn arr<'v>(v: &'v Value, p: &str) -> impl Iterator<Item = &'v Value> {
    v.pointer(p).and_then(Value::as_array).into_iter().flatten()
}

/// A line comment's place from GitHub's, GitLab's or Bitbucket's fields.
fn line_of(path: Option<&str>, line: Option<u64>) -> Option<Line> {
    match (path, line.and_then(|l| u32::try_from(l).ok())) {
        (Some(p), Some(l)) if !p.is_empty() => Some(Line { path: p.to_owned(), line: l }),
        _ => None,
    }
}

fn check_body(body: &str) -> Result<(), Refusal> {
    if body.trim().is_empty() || body.len() > BODY_MAX {
        return Err(Refusal::Invalid { what: "a comment's body", why: format!("1 to {BODY_MAX} bytes, not blank ({})", body.len()) });
    }
    Ok(())
}

fn unsupported(what: &'static str, does: &str) -> Refusal {
    Refusal::Unsupported { what, kiln_does: does.to_owned() }
}

impl HostForge {
    /// A forge on `host` at `api` (its REST API's base), kiln known to it by `kiln`.
    pub fn new(host: Host, api: &str, kiln: Cred, people: People, push: Pusher, now: Now, held: Arc<dyn Held>) -> Self {
        let api = api.trim_end_matches('/').to_owned();
        let graphql = match &kiln {
            Cred::App(app) => app.graphql_url(),
            Cred::Header(_) => format!("{api}/graphql"),
        };
        Self { host, api, graphql, kiln, people, push, now, held, st: Mutex::new(St::default()) }
    }

    pub fn host(&self) -> Host {
        self.host
    }

    async fn auth(&self, repo: &str, by: Option<&str>) -> Result<String, Refusal> {
        if let Some(h) = by.and_then(|b| (self.people)(b)) {
            return Ok(h);
        }
        match &self.kiln {
            Cred::Header(h) => Ok(h.clone()),
            Cred::App(app) => {
                let p: BTreeMap<String, String> = [("pull_requests", "write"), ("contents", "write"), ("issues", "write"), ("metadata", "read")].iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect();
                let t = app.token(repo, &p).await.map_err(|e| Refusal::Unavailable(format!("GitHub: {e:#}")))?;
                Ok(format!("Bearer {t}"))
            }
        }
    }

    fn rate_from(&self, resp: &crate::http::client::Response) -> Option<Rate> {
        let prefix = if self.host == Host::Gitlab { "ratelimit-" } else { "x-ratelimit-" };
        let num = |n: &str| resp.header(&format!("{prefix}{n}")).and_then(|v| v.trim().parse::<u64>().ok());
        let (limit, remaining) = (num("limit")?, num("remaining")?);
        let resets_at = num("reset").unwrap_or_else(|| (self.now)().saturating_add(3600));
        Some(Rate { limit: u32::try_from(limit).unwrap_or(u32::MAX), remaining: u32::try_from(remaining).unwrap_or(u32::MAX), resets_at })
    }

    /// One request to the host (as `by` where they signed in with it, else as kiln), refused
    /// before it's sent while the host's limit is spent (unless `free`: GitHub's rate itself).
    #[allow(clippy::too_many_arguments)]
    async fn send(&self, repo: &str, by: Option<&str>, method: Http, path: &str, body: Option<&Value>, etag: Option<&str>, free: bool) -> Result<Reply, Refusal> {
        let now = (self.now)();
        let spent = self.st.lock().rate.filter(|r| r.remaining == 0 && now < r.resets_at);
        if let (Some(r), false) = (spent, free) {
            return Err(Refusal::RateLimited { resets_at: r.resets_at });
        }
        let auth = self.auth(repo, by).await?;
        let url = if path.starts_with("http://") || path.starts_with("https://") { path.to_owned() } else { format!("{}{path}", self.api) };
        let client = crate::http::client::github().map_err(|e| Refusal::Unavailable(format!("{e:#}")))?;
        let mut req = client.request(method, &url).header("Authorization", &auth).header("User-Agent", "kiln").timeout(Duration::from_secs(30));
        req = match self.host {
            Host::Github => req.header("Accept", "application/vnd.github+json").header("X-GitHub-Api-Version", GITHUB_API_VERSION),
            Host::Gitlab | Host::Bitbucket => req.header("Accept", "application/json"),
        };
        if let Some(t) = etag.filter(|t| !t.is_empty()) {
            req = req.header("If-None-Match", t);
        }
        if let Some(b) = body {
            req = req.json(b);
        }
        let resp = req.send().await.map_err(|e| Refusal::Unavailable(format!("{}: {url}: {e:#}", self.host.name())))?;
        let status = resp.status();
        let rate = self.rate_from(&resp);
        if let Some(r) = rate {
            self.st.lock().rate = Some(r);
        }
        if status == 429 || (status == 403 && rate.is_some_and(|r| r.remaining == 0)) {
            return Err(Refusal::RateLimited { resets_at: rate.map_or(now.saturating_add(60), |r| r.resets_at) });
        }
        let (etag, location) = (resp.header("etag").map(str::to_owned), resp.header("location").map(str::to_owned));
        let bytes = resp.bytes(32 << 20).await.map_err(|e| Refusal::Unavailable(format!("{}: {url}: {e:#}", self.host.name())))?;
        let body = if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned())) };
        Ok(Reply { status, body, etag, location })
    }

    async fn get(&self, repo: &str, by: Option<&str>, path: &str) -> Result<Reply, Refusal> {
        self.send(repo, by, Http::Get, path, None, None, false).await
    }

    async fn with(&self, repo: &str, by: Option<&str>, method: Http, path: &str, body: &Value) -> Result<Reply, Refusal> {
        self.send(repo, by, method, path, Some(body), None, false).await
    }

    fn said(&self, v: &Value) -> String {
        match self.host {
            Host::Github => github::said(v),
            Host::Gitlab => gitlab::said(v),
            Host::Bitbucket => bitbucket::said(v),
        }
    }

    /// What an answer that isn't one of the expected statuses means.
    fn refusal(&self, what: &str, r: &Reply) -> Refusal {
        let said = self.said(&r.body);
        match r.status {
            404 => Refusal::NotFound { what: format!("{what} ({said})") },
            409 => Refusal::Conflict { why: format!("{what}: {said}") },
            400 | 422 => Refusal::Invalid { what: "a request", why: format!("{what}: {said}") },
            st => Refusal::Unavailable(format!("{what}: {} answered {st}: {said}", self.host.name())),
        }
    }

    fn ok(&self, what: &str, r: Reply, want: &[u16]) -> Result<Reply, Refusal> {
        if want.contains(&r.status) { Ok(r) } else { Err(self.refusal(what, &r)) }
    }

    /// The repository's path in the host's API.
    fn base(&self, repo: &str) -> String {
        match self.host {
            Host::Github => format!("/repos/{repo}"),
            Host::Gitlab => format!("/projects/{}", gitlab::id(repo)),
            Host::Bitbucket => format!("/repositories/{repo}"),
        }
    }

    /// Change `number`'s path.
    fn pull(&self, repo: &str, number: u64) -> String {
        match self.host {
            Host::Github => format!("/repos/{repo}/pulls/{number}"),
            Host::Gitlab => format!("/projects/{}/merge_requests/{number}", gitlab::id(repo)),
            Host::Bitbucket => format!("/repositories/{repo}/pullrequests/{number}"),
        }
    }

    // ------------------------------------------------------------------------- changes' shapes

    fn github_change(v: &Value) -> Change {
        let merged = v.get("merged_at").is_some_and(|m| !m.is_null()) || v.get("merged").and_then(Value::as_bool) == Some(true);
        let state = match (merged, s(v, "/state").as_str()) {
            (true, _) => State::Merged { commit: s(v, "/merge_commit_sha") },
            (false, "closed") => State::Closed,
            _ => State::Open,
        };
        let stack = v.get("stack").filter(|x| x.is_object()).map(|x| Place {
            stack: u(x, "/number"),
            position: u32::try_from(u(x, "/position")).unwrap_or(u32::MAX),
            size: u32::try_from(u(x, "/size")).unwrap_or(u32::MAX),
        });
        Change { number: u(v, "/number"), author: s(v, "/user/login"), title: s(v, "/title"), head_ref: s(v, "/head/ref"), head_sha: s(v, "/head/sha"), base_ref: s(v, "/base/ref"), state, stack, updating: false }
    }

    fn gitlab_change(v: &Value) -> Change {
        let commit = [s(v, "/merge_commit_sha"), s(v, "/squash_commit_sha"), s(v, "/sha")].into_iter().find(|c| !c.is_empty()).unwrap_or_default();
        let state = match s(v, "/state").as_str() {
            "merged" => State::Merged { commit },
            "closed" | "locked" => State::Closed,
            _ => State::Open,
        };
        Change {
            number: u(v, "/iid"),
            author: s(v, "/author/username"),
            title: s(v, "/title"),
            head_ref: s(v, "/source_branch"),
            head_sha: s(v, "/sha"),
            base_ref: s(v, "/target_branch"),
            state,
            stack: None,
            updating: v.get("rebase_in_progress").and_then(Value::as_bool).unwrap_or(false),
        }
    }

    /// A Bitbucket pull request, its head's short hash made whole (Bitbucket gives 12 characters).
    async fn bitbucket_change(&self, repo: &str, v: &Value) -> Result<Change, Refusal> {
        let mut head = s(v, "/source/commit/hash");
        if !head.is_empty() && head.len() < 40 {
            let r = self.get(repo, None, &format!("/repositories/{repo}/commit/{head}")).await?;
            head = s(&self.ok("the head commit", r, &[200])?.body, "/hash");
        }
        let state = match s(v, "/state").as_str() {
            "MERGED" => State::Merged { commit: s(v, "/merge_commit/hash") },
            "DECLINED" | "SUPERSEDED" => State::Closed,
            _ => State::Open,
        };
        Ok(Change {
            number: u(v, "/id"),
            author: [s(v, "/author/nickname"), s(v, "/author/display_name")].into_iter().find(|a| !a.is_empty()).unwrap_or_default(),
            title: s(v, "/title"),
            head_ref: s(v, "/source/branch/name"),
            head_sha: head,
            base_ref: s(v, "/destination/branch/name"),
            state,
            stack: None,
            updating: false,
        })
    }

    async fn change_of(&self, repo: &str, v: &Value) -> Result<Change, Refusal> {
        match self.host {
            Host::Github => Ok(Self::github_change(v)),
            Host::Gitlab => Ok(Self::gitlab_change(v)),
            Host::Bitbucket => self.bitbucket_change(repo, v).await,
        }
    }

    /// Change `number` as it is, and the host's own answer.
    async fn read(&self, repo: &str, number: u64) -> Result<(Change, Value), Refusal> {
        let path = match self.host {
            Host::Gitlab => format!("{}?include_rebase_in_progress=true", self.pull(repo, number)),
            _ => self.pull(repo, number),
        };
        let r = self.get(repo, None, &path).await?;
        let r = self.ok(&format!("change {number}"), r, &[200])?;
        let c = self.change_of(repo, &r.body).await?;
        Ok((c, r.body))
    }

    /// The change, open, or refused Closed.
    async fn read_open(&self, repo: &str, number: u64) -> Result<(Change, Value), Refusal> {
        let (c, v) = self.read(repo, number).await?;
        if c.state != State::Open {
            return Err(Refusal::Closed { number });
        }
        Ok((c, v))
    }

    /// The repository's default branch.
    async fn trunk(&self, repo: &str) -> Result<String, Refusal> {
        if let Some(t) = self.st.lock().trunks.get(repo) {
            return Ok(t.clone());
        }
        let r = self.get(repo, None, &self.base(repo)).await?;
        let v = self.ok("the repository", r, &[200])?.body;
        let t = match self.host {
            Host::Bitbucket => s(&v, "/mainbranch/name"),
            _ => s(&v, "/default_branch"),
        };
        let t = if t.is_empty() { TRUNK.to_owned() } else { t };
        let mut st = self.st.lock();
        if st.trunks.len() >= TRUNKS_KEPT {
            st.trunks.clear();
        }
        st.trunks.insert(repo.to_owned(), t.clone());
        Ok(t)
    }

    /// Every open change, by number (one page at a time), with the first page's ETag; GitLab's
    /// with their stacks as GitLab detects them.
    async fn list(&self, repo: &str, etag: Option<&str>) -> Result<Fresh<Vec<Change>>, Refusal> {
        let mut path = match self.host {
            Host::Github => format!("/repos/{repo}/pulls?state=open&per_page=100"),
            Host::Gitlab => format!("/projects/{}/merge_requests?state=opened&per_page=100", gitlab::id(repo)),
            Host::Bitbucket => format!("/repositories/{repo}/pullrequests?state=OPEN&pagelen=50"),
        };
        let (mut out, mut first_tag, mut page) = (Vec::new(), None, 1u32);
        loop {
            let r = self.send(repo, None, Http::Get, &path, None, if page == 1 { etag } else { None }, false).await?;
            if r.status == 304 && page == 1 {
                return Ok(Fresh::NotModified);
            }
            let r = self.ok("the open changes", r, &[200])?;
            if page == 1 {
                first_tag = r.etag.clone();
            }
            let items: Vec<Value> = match self.host {
                Host::Bitbucket => arr(&r.body, "/values").cloned().collect(),
                _ => r.body.as_array().cloned().unwrap_or_default(),
            };
            let full = items.len() >= 100;
            for v in &items {
                out.push(self.change_of(repo, v).await?);
            }
            let next = match self.host {
                Host::Bitbucket => r.body.get("next").and_then(Value::as_str).map(str::to_owned),
                _ if full && page < 100 => Some(format!("{}&page={}", path.split("&page=").next().unwrap_or(&path), page.saturating_add(1))),
                _ => None,
            };
            match next {
                Some(n) if page < 100 => {
                    path = n;
                    page = page.saturating_add(1);
                }
                _ => break,
            }
        }
        out.sort_by_key(|c| c.number);
        if self.host == Host::Gitlab {
            let trunk = self.trunk(repo).await?;
            gitlab_places(&mut out, &trunk);
        }
        Ok(Fresh::Changed { value: out, etag: first_tag.unwrap_or_default() })
    }

    async fn open_changes(&self, repo: &str) -> Result<Vec<Change>, Refusal> {
        self.list(repo, None).await?.changed().map(|(v, _)| v).ok_or_else(|| Refusal::Unavailable("an unconditional read answered NotModified".into()))
    }

    // ------------------------------------------------------------------------------ stacks

    fn github_stack(v: &Value) -> Stack {
        let open = arr(v, "/pull_requests").filter(|p| s(p, "/state") == "open" && p.get("merged_at").is_none_or(Value::is_null)).map(|p| u(p, "/number")).collect();
        Stack { number: u(v, "/number"), base: s(v, "/base/ref"), changes: open }
    }

    /// After GitHub refused a stack: Closed if one of `changes` isn't open, else what it said.
    async fn stack_refused(&self, repo: &str, changes: &[u64], r: &Reply) -> Refusal {
        for n in changes {
            match self.read(repo, *n).await {
                Ok((c, _)) if c.state != State::Open => return Refusal::Closed { number: *n },
                Err(e @ Refusal::NotFound { .. }) => return e,
                _ => {}
            }
        }
        self.refusal("the stack", r)
    }

    /// GitLab: checks `changes` continue a stack on `below` (`size` already in it, by `author`).
    async fn gitlab_chain(&self, repo: &str, open: &[Change], mut below: String, changes: &[u64], mut author: Option<String>, size: usize) -> Result<(), Refusal> {
        for (i, n) in changes.iter().enumerate() {
            let Some(c) = open.iter().find(|c| c.number == *n) else {
                self.read_open(repo, *n).await?;
                return Err(Refusal::Invalid { what: "a stack", why: format!("merge request {n} isn't open") });
            };
            if changes.iter().filter(|m| *m == n).count() > 1 {
                return Err(Refusal::Invalid { what: "a stack", why: format!("merge request {n} is in it twice") });
            }
            if c.base_ref != below {
                return Err(Refusal::Invalid { what: "a stack", why: format!("merge request {n} targets {}, not {below}", c.base_ref) });
            }
            let first = author.get_or_insert_with(|| c.author.clone());
            if *first != c.author {
                return Err(Refusal::StackLimit { why: format!("merge request {n} is by {}, not {first}: GitLab stacks one author's merge requests", c.author) });
            }
            let at = size.saturating_add(i).saturating_add(1);
            if at > GITLAB_STACK_MOST as usize {
                return Err(Refusal::StackLimit { why: format!("merge request {n} would be the stack's {at}th: GitLab stacks {GITLAB_STACK_MOST} merge requests at most") });
            }
            below = c.head_ref.clone();
        }
        Ok(())
    }

    // ------------------------------------------------------------------------------- queues

    async fn github_queue_on(&self, repo: &str) -> Result<bool, Refusal> {
        let trunk = self.trunk(repo).await?;
        let r = self.get(repo, None, &format!("/repos/{repo}/rules/branches/{trunk}")).await?;
        let r = self.ok("the trunk's rules", r, &[200])?;
        Ok(r.body.as_array().is_some_and(|rules| rules.iter().any(|x| s(x, "/type") == "merge_queue")))
    }

    async fn graphql(&self, repo: &str, query: &str, variables: Value) -> Result<Value, Refusal> {
        let url = self.graphql.clone();
        let r = self.with(repo, None, Http::Post, &url, &json!({ "query": query, "variables": variables })).await?;
        let r = self.ok("GitHub's GraphQL", r, &[200])?;
        if let Some(e) = r.body.pointer("/errors/0/message").and_then(Value::as_str) {
            return Err(Refusal::Invalid { what: "a GraphQL request", why: e.to_owned() });
        }
        Ok(r.body)
    }

    async fn queue(&self, repo: &str) -> Result<Vec<u64>, Refusal> {
        match self.host {
            Host::Github => {
                let (owner, name) = repo.split_once('/').unwrap_or((repo, ""));
                let trunk = self.trunk(repo).await?;
                let q = "query($owner:String!,$name:String!,$branch:String!){repository(owner:$owner,name:$name){mergeQueue(branch:$branch){entries(first:100){nodes{position pullRequest{number}}}}}}";
                let v = self.graphql(repo, q, json!({ "owner": owner, "name": name, "branch": trunk })).await?;
                let Some(mq) = v.pointer("/data/repository/mergeQueue").filter(|m| !m.is_null()) else { return Err(unsupported("a host's queue", KILN_QUEUE)) };
                let mut entries: Vec<(u64, u64)> = arr(mq, "/entries/nodes").map(|e| (u(e, "/position"), u(e, "/pullRequest/number"))).collect();
                entries.sort_unstable();
                Ok(entries.into_iter().map(|(_, n)| n).collect())
            }
            Host::Gitlab => {
                let trunk = self.trunk(repo).await?;
                let r = self.get(repo, None, &format!("/projects/{}/merge_trains/{}", gitlab::id(repo), crate::ci::cli::enc(&trunk))).await?;
                if matches!(r.status, 403 | 404) {
                    return Err(unsupported("a host's queue", KILN_QUEUE));
                }
                let r = self.ok("the merge train", r, &[200])?;
                Ok(r.body.as_array().into_iter().flatten().map(|car| u(car, "/merge_request/iid")).collect())
            }
            Host::Bitbucket => Err(unsupported("a host's queue", KILN_QUEUE)),
        }
    }

    // ------------------------------------------------------------------------------ reviews

    /// `by`'s pending GitHub review of change `number`: its id, node id and body.
    async fn pending(&self, repo: &str, number: u64, by: &str) -> Result<Option<(u64, String, String)>, Refusal> {
        let r = self.get(repo, Some(by), &format!("/repos/{repo}/pulls/{number}/reviews?per_page=100")).await?;
        let r = self.ok("the reviews", r, &[200])?;
        Ok(r.body.as_array().into_iter().flatten().find(|x| s(x, "/state") == "PENDING" && s(x, "/user/login") == by).map(|x| (u(x, "/id"), s(x, "/node_id"), s(x, "/body"))))
    }

    /// A GitHub review's comments: its line comments, then its body.
    async fn github_review_comments(&self, repo: &str, number: u64, by: Option<&str>, review: (u64, &str, &str), published: bool) -> Result<Vec<Comment>, Refusal> {
        let (id, author, body) = review;
        let r = self.get(repo, by, &format!("/repos/{repo}/pulls/{number}/reviews/{id}/comments?per_page=100")).await?;
        let r = self.ok("the review's comments", r, &[200])?;
        let mark = published.then_some(id);
        let mut out: Vec<Comment> = r
            .body
            .as_array()
            .into_iter()
            .flatten()
            .map(|c| Comment { id: u(c, "/id"), author: author.to_owned(), body: s(c, "/body"), line: line_of(c.get("path").and_then(Value::as_str), c.get("line").and_then(Value::as_u64)), review: mark })
            .collect();
        if !body.trim().is_empty() {
            out.push(Comment { id, author: author.to_owned(), body: body.to_owned(), line: None, review: mark });
        }
        Ok(out)
    }

    async fn gitlab_drafts(&self, repo: &str, number: u64, by: &str) -> Result<Vec<Comment>, Refusal> {
        let r = self.get(repo, Some(by), &format!("{}/draft_notes", self.pull(repo, number))).await?;
        let r = self.ok("the draft notes", r, &[200])?;
        Ok(r.body
            .as_array()
            .into_iter()
            .flatten()
            .map(|d| Comment { id: u(d, "/id"), author: by.to_owned(), body: s(d, "/note"), line: line_of(d.pointer("/position/new_path").and_then(Value::as_str), d.pointer("/position/new_line").and_then(Value::as_u64)), review: None })
            .collect())
    }

    fn remember_merge(&self, id: &str, merged: Vec<u64>) {
        let mut st = self.st.lock();
        if st.merges.len() >= MERGES_KEPT
            && let Some(old) = st.merge_order.pop_front()
        {
            st.merges.remove(&old);
        }
        st.merges.insert(id.to_owned(), merged);
        st.merge_order.push_back(id.to_owned());
    }
}

/// GitLab's stacks among the open merge requests, as GitLab detects them: from a merge request
/// into the trunk, each one targeting the source branch of the one below, one author's, 2 to 20.
fn gitlab_places(open: &mut [Change], trunk: &str) {
    let mut chains: Vec<Vec<usize>> = Vec::new();
    for (i, c) in open.iter().enumerate() {
        if c.base_ref != trunk {
            continue;
        }
        let mut chain = vec![i];
        while let Some(last) = chain.last().and_then(|l| open.get(*l)) {
            let next = open.iter().position(|o| o.base_ref == last.head_ref && o.author == c.author && !chain.iter().any(|k| open.get(*k).is_some_and(|k| k.number == o.number)));
            match next {
                Some(n) if chain.len() <= GITLAB_STACK_MOST as usize => chain.push(n),
                _ => break,
            }
        }
        if (2..=GITLAB_STACK_MOST as usize).contains(&chain.len()) {
            chains.push(chain);
        }
    }
    for chain in chains {
        let stack = chain.first().and_then(|i| open.get(*i)).map(|c| c.number).unwrap_or_default();
        let size = u32::try_from(chain.len()).unwrap_or(u32::MAX);
        for (at, i) in chain.iter().enumerate() {
            if let Some(c) = open.get_mut(*i) {
                c.stack = Some(Place { stack, position: u32::try_from(at).unwrap_or(u32::MAX).saturating_add(1), size });
            }
        }
    }
}

/// The chain of one author's merge requests up from `bottom` (one into the trunk), each targeting
/// the source branch of the one below, however long.
fn gitlab_chain_from(open: &[Change], trunk: &str, bottom: u64) -> Option<Vec<u64>> {
    let first = open.iter().find(|c| c.number == bottom && c.base_ref == trunk)?;
    let mut chain = vec![first];
    while let Some(last) = chain.last() {
        match open.iter().find(|o| o.base_ref == last.head_ref && o.author == first.author && !chain.iter().any(|k| k.number == o.number)) {
            Some(next) if chain.len() <= open.len() => chain.push(next),
            _ => break,
        }
    }
    Some(chain.into_iter().map(|c| c.number).collect())
}

/// GitLab's stacks among `open`: each stack's changes, bottom to top, numbered by its bottom.
fn gitlab_stacks(open: &[Change]) -> BTreeMap<u64, Vec<u64>> {
    let mut out: BTreeMap<u64, Vec<(u32, u64)>> = BTreeMap::new();
    for c in open {
        if let Some(p) = c.stack {
            out.entry(p.stack).or_default().push((p.position, c.number));
        }
    }
    out.into_iter()
        .map(|(k, mut v)| {
            v.sort_unstable();
            (k, v.into_iter().map(|(_, n)| n).collect())
        })
        .collect()
}

impl Forge for HostForge {
    fn abilities<'a>(&'a self, repo: &'a str) -> BoxFuture<'a, Result<Abilities, Refusal>> {
        Box::pin(async move {
            let r = self.get(repo, None, &self.base(repo)).await?;
            let v = self.ok("the repository", r, &[200])?.body;
            let trunk = s(&v, if self.host == Host::Bitbucket { "/mainbranch/name" } else { "/default_branch" });
            if !trunk.is_empty() {
                let mut st = self.st.lock();
                if st.trunks.len() >= TRUNKS_KEPT {
                    st.trunks.clear();
                }
                st.trunks.insert(repo.to_owned(), trunk);
            }
            Ok(match self.host {
                Host::Github => {
                    let flag = |k: &str| v.get(k).and_then(Value::as_bool).unwrap_or(true);
                    let methods = [(Method::Merge, "allow_merge_commit"), (Method::Squash, "allow_squash_merge"), (Method::Rebase, "allow_rebase_merge")].into_iter().filter(|(_, k)| flag(k)).map(|(m, _)| m).collect();
                    Abilities {
                        host: Host::Github,
                        stacks: Stacks::Native { most: None, one_author: false, order: StackOrder::BottomUp },
                        queue: self.github_queue_on(repo).await?.then_some(QueueKind::MergeQueue),
                        update: Some(HostUpdate::UpdateBranch),
                        review: Batched::PendingReview,
                        merge_async: true,
                        methods,
                    }
                }
                Host::Gitlab => Abilities {
                    host: Host::Gitlab,
                    stacks: Stacks::Native { most: Some(GITLAB_STACK_MOST), one_author: true, order: StackOrder::Retarget },
                    queue: v.get("merge_trains_enabled").and_then(Value::as_bool).unwrap_or(false).then_some(QueueKind::MergeTrain),
                    update: Some(HostUpdate::Rebase),
                    review: Batched::DraftNotes,
                    merge_async: false,
                    methods: vec![Method::Merge, Method::Squash, Method::FastForward],
                },
                Host::Bitbucket => Abilities {
                    host: Host::Bitbucket,
                    stacks: Stacks::Kiln { does: BITBUCKET_STACKS.to_owned() },
                    queue: None,
                    update: None,
                    review: Batched::Held,
                    merge_async: false,
                    methods: vec![Method::Merge, Method::Squash, Method::FastForward],
                },
            })
        })
    }

    fn open<'a>(&'a self, repo: &'a str, open: Open) -> BoxFuture<'a, Result<Change, Refusal>> {
        Box::pin(async move {
            let (path, body) = match self.host {
                Host::Github => (format!("/repos/{repo}/pulls"), json!({ "head": open.head, "base": open.base, "title": open.title })),
                Host::Gitlab => (format!("/projects/{}/merge_requests", gitlab::id(repo)), json!({ "source_branch": open.head, "target_branch": open.base, "title": open.title })),
                Host::Bitbucket => (
                    format!("/repositories/{repo}/pullrequests"),
                    json!({ "title": open.title, "source": { "branch": { "name": open.head } }, "destination": { "branch": { "name": open.base } } }),
                ),
            };
            let r = self.with(repo, Some(&open.by), Http::Post, &path, &body).await?;
            let r = self.ok("the change", r, &[200, 201])?;
            self.change_of(repo, &r.body).await
        })
    }

    fn change<'a>(&'a self, repo: &'a str, number: u64, etag: Option<&'a str>) -> BoxFuture<'a, Result<Fresh<Change>, Refusal>> {
        Box::pin(async move {
            let path = match self.host {
                Host::Gitlab => format!("{}?include_rebase_in_progress=true", self.pull(repo, number)),
                _ => self.pull(repo, number),
            };
            let r = self.send(repo, None, Http::Get, &path, None, etag, false).await?;
            if r.status == 304 {
                return Ok(Fresh::NotModified);
            }
            let r = self.ok(&format!("change {number}"), r, &[200])?;
            let mut c = self.change_of(repo, &r.body).await?;
            if self.host == Host::Gitlab && c.state == State::Open {
                c.stack = self.open_changes(repo).await?.into_iter().find(|o| o.number == number).and_then(|o| o.stack);
            }
            Ok(Fresh::Changed { value: c, etag: r.etag.unwrap_or_default() })
        })
    }

    fn changes<'a>(&'a self, repo: &'a str, etag: Option<&'a str>) -> BoxFuture<'a, Result<Fresh<Vec<Change>>, Refusal>> {
        Box::pin(self.list(repo, etag))
    }

    fn update<'a>(&'a self, repo: &'a str, number: u64, update: Update) -> BoxFuture<'a, Result<Updated, Refusal>> {
        Box::pin(async move {
            let (c, _) = self.read_open(repo, number).await?;
            if c.head_sha != update.expected {
                return Err(Refusal::Moved { head: c.head_sha });
            }
            if let Some(sha) = update.replayed {
                return match (self.push)(repo.to_owned(), c.head_ref.clone(), sha.clone(), update.expected.clone()).await {
                    Ok(()) => Ok(Updated::Done { head: sha }),
                    Err(Refusal::Moved { .. }) => Err(Refusal::Moved { head: self.read(repo, number).await?.0.head_sha }),
                    Err(e) => Err(e),
                };
            }
            match self.host {
                Host::Github => {
                    let r = self.with(repo, None, Http::Put, &format!("{}/update-branch", self.pull(repo, number)), &json!({ "expected_head_sha": update.expected })).await?;
                    if r.status == 422 {
                        let head = self.read(repo, number).await?.0.head_sha;
                        if head != update.expected {
                            return Err(Refusal::Moved { head });
                        }
                    }
                    self.ok("updating the branch", r, &[202])?;
                    let head = self.read(repo, number).await?.0.head_sha;
                    Ok(if head == update.expected { Updated::Started } else { Updated::Done { head } })
                }
                Host::Gitlab => {
                    let r = self.with(repo, None, Http::Put, &format!("{}/rebase", self.pull(repo, number)), &json!({})).await?;
                    if r.status == 403 {
                        return Err(Refusal::Conflict { why: format!("GitLab won't rebase it: {}", self.said(&r.body)) });
                    }
                    self.ok("the rebase", r, &[202])?;
                    Ok(Updated::Started)
                }
                Host::Bitbucket => Err(unsupported("updating a change on the host", KILN_UPDATE)),
            }
        })
    }

    fn stack<'a>(&'a self, repo: &'a str, changes: Vec<u64>) -> BoxFuture<'a, Result<Stack, Refusal>> {
        Box::pin(async move {
            if self.host == Host::Bitbucket {
                return Err(unsupported("stacks", BITBUCKET_STACKS));
            }
            if changes.is_empty() {
                return Err(Refusal::Invalid { what: "a stack", why: "no changes".into() });
            }
            match self.host {
                Host::Github => {
                    let r = self.with(repo, None, Http::Post, &format!("/repos/{repo}/stacks"), &json!({ "pull_requests": changes })).await?;
                    if r.status != 201 && r.status != 200 {
                        return Err(self.stack_refused(repo, &changes, &r).await);
                    }
                    Ok(Self::github_stack(&r.body))
                }
                _ => {
                    // GitLab detects stacks itself: this checks `changes` make one as it would.
                    let open = self.open_changes(repo).await?;
                    let trunk = self.trunk(repo).await?;
                    self.gitlab_chain(repo, &open, trunk.clone(), &changes, None, 0).await?;
                    Ok(Stack { number: changes.first().copied().unwrap_or_default(), base: trunk, changes })
                }
            }
        })
    }

    fn stack_add<'a>(&'a self, repo: &'a str, stack: u64, changes: Vec<u64>) -> BoxFuture<'a, Result<Stack, Refusal>> {
        Box::pin(async move {
            match self.host {
                Host::Bitbucket => Err(unsupported("stacks", BITBUCKET_STACKS)),
                Host::Github => {
                    let r = self.with(repo, None, Http::Post, &format!("/repos/{repo}/stacks/{stack}/add"), &json!({ "pull_requests": changes })).await?;
                    if r.status != 200 {
                        return Err(self.stack_refused(repo, &changes, &r).await);
                    }
                    Ok(Self::github_stack(&r.body))
                }
                Host::Gitlab => {
                    let open = self.open_changes(repo).await?;
                    let trunk = self.trunk(repo).await?;
                    let mut have = gitlab_chain_from(&open, &trunk, stack).ok_or_else(|| Refusal::NotFound { what: format!("stack {stack}") })?;
                    if let Some(at) = have.iter().position(|n| changes.contains(n)) {
                        have.truncate(at);
                    }
                    let top = have.last().and_then(|n| open.iter().find(|c| c.number == *n)).ok_or_else(|| Refusal::NotFound { what: format!("stack {stack}") })?;
                    self.gitlab_chain(repo, &open, top.head_ref.clone(), &changes, Some(top.author.clone()), have.len()).await?;
                    Ok(Stack { number: stack, base: trunk, changes: have.into_iter().chain(changes).collect() })
                }
            }
        })
    }

    fn stack_of<'a>(&'a self, repo: &'a str, number: u64) -> BoxFuture<'a, Result<Option<Stack>, Refusal>> {
        Box::pin(async move {
            match self.host {
                Host::Bitbucket => Ok(None),
                Host::Github => {
                    let r = self.get(repo, None, &format!("/repos/{repo}/stacks?pull_request={number}")).await?;
                    let r = self.ok("the stacks", r, &[200])?;
                    Ok(r.body.as_array().and_then(|a| a.first()).map(Self::github_stack).filter(|s| s.changes.contains(&number)))
                }
                Host::Gitlab => {
                    let open = self.open_changes(repo).await?;
                    let trunk = self.trunk(repo).await?;
                    Ok(gitlab_stacks(&open).into_iter().find(|(_, v)| v.contains(&number)).map(|(k, v)| Stack { number: k, base: trunk, changes: v }))
                }
            }
        })
    }

    fn unstack<'a>(&'a self, repo: &'a str, stack: u64) -> BoxFuture<'a, Result<Option<Stack>, Refusal>> {
        Box::pin(async move {
            match self.host {
                Host::Bitbucket => Err(unsupported("stacks", BITBUCKET_STACKS)),
                Host::Gitlab => Err(unsupported("unstacking", "retargets each merge request onto the default branch")),
                Host::Github => {
                    let r = self.with(repo, None, Http::Post, &format!("/repos/{repo}/stacks/{stack}/unstack"), &json!({})).await?;
                    let r = self.ok("unstacking", r, &[200, 204])?;
                    Ok((r.status == 200).then(|| Self::github_stack(&r.body)))
                }
            }
        })
    }

    fn merge<'a>(&'a self, repo: &'a str, number: u64, merge: Merge) -> BoxFuture<'a, Result<Merging, Refusal>> {
        Box::pin(async move {
            let allowed = match self.host {
                Host::Github => matches!(merge.method, Method::Merge | Method::Squash | Method::Rebase),
                Host::Gitlab | Host::Bitbucket => matches!(merge.method, Method::Merge | Method::Squash | Method::FastForward),
            };
            if !allowed {
                return Err(Refusal::Invalid { what: "a merge method", why: format!("{} doesn't merge by {:?}", self.host.name(), merge.method) });
            }
            let (c, _) = self.read_open(repo, number).await?;
            if c.head_sha != merge.expected {
                return Err(Refusal::Moved { head: c.head_sha });
            }
            match self.host {
                Host::Github => {
                    let group = match c.stack {
                        Some(p) => match self.stack_of(repo, number).await? {
                            Some(s) => s.changes.into_iter().take(usize::try_from(p.position).unwrap_or(usize::MAX)).collect(),
                            None => vec![number],
                        },
                        None => vec![number],
                    };
                    let method = match merge.method {
                        Method::Squash => "squash",
                        Method::Rebase => "rebase",
                        _ => "merge",
                    };
                    let body = json!({ "merge_method": method, "sha": merge.expected, "merge_action": "direct_merge" });
                    let r = self.with(repo, None, Http::Put, &format!("{}/merge-async", self.pull(repo, number)), &body).await?;
                    match r.status {
                        202 => {
                            let id = s(&r.body, "/uuid");
                            self.remember_merge(&id, group);
                            Ok(Merging::Started { id })
                        }
                        200 => Err(Refusal::Closed { number }),
                        400 => Err(Refusal::Closed { number }),
                        409 => Err(Refusal::Conflict { why: format!("a merge of change {number} is under way already") }),
                        422 => {
                            let said = self.said(&r.body);
                            let low = said.to_ascii_lowercase();
                            Err(if low.contains("linear") || low.contains("rebase") { Refusal::NotLinear { why: said } } else { Refusal::Invalid { what: "a merge", why: said } })
                        }
                        _ => Err(self.refusal("the merge", &r)),
                    }
                }
                Host::Gitlab => {
                    let body = json!({ "sha": merge.expected, "squash": merge.method == Method::Squash });
                    let r = self.with(repo, None, Http::Put, &format!("{}/merge", self.pull(repo, number)), &body).await?;
                    match r.status {
                        200 => {
                            let commit = [s(&r.body, "/merge_commit_sha"), s(&r.body, "/squash_commit_sha"), s(&r.body, "/sha")].into_iter().find(|c| !c.is_empty()).unwrap_or_default();
                            Ok(Merging::Merged { merged: vec![number], commit })
                        }
                        409 => Err(Refusal::Moved { head: self.read(repo, number).await?.0.head_sha }),
                        405 | 406 | 422 => Err(Refusal::NotLinear { why: format!("GitLab can't merge it: {}", self.said(&r.body)) }),
                        _ => Err(self.refusal("the merge", &r)),
                    }
                }
                Host::Bitbucket => {
                    let strategy = match merge.method {
                        Method::Squash => "squash",
                        Method::FastForward => "fast_forward",
                        _ => "merge_commit",
                    };
                    let body = json!({ "type": "pullrequest_merge_parameters", "merge_strategy": strategy });
                    let r = self.with(repo, None, Http::Post, &format!("{}/merge", self.pull(repo, number)), &body).await?;
                    match r.status {
                        200 => Ok(Merging::Merged { merged: vec![number], commit: s(&r.body, "/merge_commit/hash") }),
                        202 => {
                            let id = r.location.clone().or_else(|| r.body.pointer("/links/self/href").and_then(Value::as_str).map(str::to_owned)).unwrap_or_default();
                            self.remember_merge(&id, vec![number]);
                            Ok(Merging::Started { id })
                        }
                        400 | 555 => Err(Refusal::NotLinear { why: format!("Bitbucket can't merge it: {}", self.said(&r.body)) }),
                        _ => Err(self.refusal("the merge", &r)),
                    }
                }
            }
        })
    }

    fn merge_result<'a>(&'a self, repo: &'a str, number: u64, id: &'a str) -> BoxFuture<'a, Result<MergeResult, Refusal>> {
        Box::pin(async move {
            let merged = self.st.lock().merges.get(id).cloned().unwrap_or_else(|| vec![number]);
            match self.host {
                Host::Github => {
                    let r = self.get(repo, None, &format!("{}/merge-async/{id}", self.pull(repo, number))).await?;
                    let r = self.ok(&format!("merge {id}"), r, &[200])?;
                    Ok(match s(&r.body, "/status").as_str() {
                        "merged" => MergeResult::Merged { merged, commit: s(&r.body, "/merge_commit_oid") },
                        "failed" => MergeResult::Failed { why: self.said(&r.body) },
                        _ => MergeResult::Pending,
                    })
                }
                Host::Bitbucket if !id.is_empty() => {
                    let r = self.get(repo, None, id).await?;
                    let r = self.ok(&format!("merge {id}"), r, &[200])?;
                    Ok(match s(&r.body, "/task_status").as_str() {
                        "SUCCESS" => MergeResult::Merged { merged, commit: s(&r.body, "/merge_result/merge_commit/hash") },
                        "PENDING" => MergeResult::Pending,
                        _ => MergeResult::Failed { why: self.said(&r.body) },
                    })
                }
                _ => Err(Refusal::NotFound { what: format!("merge {id} of change {number}") }),
            }
        })
    }

    fn enqueue<'a>(&'a self, repo: &'a str, number: u64, expected: &'a str) -> BoxFuture<'a, Result<Vec<u64>, Refusal>> {
        Box::pin(async move {
            if self.host == Host::Bitbucket {
                return Err(unsupported("a host's queue", KILN_QUEUE));
            }
            let (c, _) = self.read_open(repo, number).await?;
            if c.head_sha != expected {
                return Err(Refusal::Moved { head: c.head_sha });
            }
            match self.host {
                Host::Github => {
                    if !self.github_queue_on(repo).await? {
                        return Err(unsupported("a host's queue", KILN_QUEUE));
                    }
                    let body = json!({ "sha": expected, "merge_action": "merge_queue" });
                    let r = self.with(repo, None, Http::Put, &format!("{}/merge-async", self.pull(repo, number)), &body).await?;
                    match r.status {
                        200 => {}
                        202 => {
                            let id = s(&r.body, "/uuid");
                            let done = self.get(repo, None, &format!("{}/merge-async/{id}", self.pull(repo, number))).await?;
                            let done = self.ok("queueing", done, &[200])?;
                            if s(&done.body, "/status") == "failed" {
                                let said = self.said(&done.body);
                                return Err(if said.to_ascii_lowercase().contains("linear") { Refusal::NotLinear { why: said } } else { Refusal::Invalid { what: "queueing", why: said } });
                            }
                        }
                        400 => return Err(Refusal::Closed { number }),
                        422 => {
                            let said = self.said(&r.body);
                            return Err(if said.to_ascii_lowercase().contains("linear") { Refusal::NotLinear { why: said } } else { Refusal::Invalid { what: "queueing", why: said } });
                        }
                        _ => return Err(self.refusal("queueing", &r)),
                    }
                }
                _ => {
                    let path = format!("/projects/{}/merge_trains/merge_requests/{number}", gitlab::id(repo));
                    let r = self.with(repo, None, Http::Post, &path, &json!({ "sha": expected })).await?;
                    match r.status {
                        200..=202 => {}
                        403 | 404 => return Err(unsupported("a host's queue", KILN_QUEUE)),
                        409 => return Err(Refusal::Moved { head: self.read(repo, number).await?.0.head_sha }),
                        _ => return Err(self.refusal("the merge train", &r)),
                    }
                }
            }
            self.queue(repo).await
        })
    }

    fn dequeue<'a>(&'a self, repo: &'a str, number: u64) -> BoxFuture<'a, Result<Vec<u64>, Refusal>> {
        Box::pin(async move {
            let before = self.queue(repo).await?;
            if !before.contains(&number) {
                return Ok(Vec::new());
            }
            match self.host {
                Host::Github => {
                    let (_, v) = self.read(repo, number).await?;
                    let q = "mutation($id:ID!){dequeuePullRequest(input:{id:$id}){mergeQueueEntry{id}}}";
                    self.graphql(repo, q, json!({ "id": s(&v, "/node_id") })).await?;
                }
                _ => {
                    let r = self.with(repo, None, Http::Post, &format!("{}/cancel_auto_merge", self.pull(repo, number)), &json!({})).await?;
                    self.ok("leaving the merge train", r, &[200, 201])?;
                }
            }
            let after = self.queue(repo).await?;
            Ok(before.into_iter().filter(|n| !after.contains(n)).collect())
        })
    }

    fn queued<'a>(&'a self, repo: &'a str) -> BoxFuture<'a, Result<Vec<u64>, Refusal>> {
        Box::pin(self.queue(repo))
    }

    fn draft<'a>(&'a self, repo: &'a str, number: u64, by: &'a str, draft: Draft) -> BoxFuture<'a, Result<u64, Refusal>> {
        Box::pin(async move {
            check_body(&draft.body)?;
            if by.trim().is_empty() {
                return Err(Refusal::Invalid { what: "a draft's author", why: "empty".into() });
            }
            let (_, v) = self.read_open(repo, number).await?;
            match self.host {
                Host::Github => match (self.pending(repo, number, by).await?, &draft.line) {
                    (None, Some(l)) => {
                        let body = json!({ "comments": [{ "path": l.path, "line": l.line, "side": "RIGHT", "body": draft.body }] });
                        let r = self.with(repo, Some(by), Http::Post, &format!("{}/reviews", self.pull(repo, number)), &body).await?;
                        let review = u(&self.ok("the pending review", r, &[200, 201])?.body, "/id");
                        let cs = self.github_review_comments(repo, number, Some(by), (review, by, ""), false).await?;
                        Ok(cs.last().map(|c| c.id).unwrap_or(review))
                    }
                    (None, None) => {
                        let r = self.with(repo, Some(by), Http::Post, &format!("{}/reviews", self.pull(repo, number)), &json!({ "body": draft.body })).await?;
                        Ok(u(&self.ok("the pending review", r, &[200, 201])?.body, "/id"))
                    }
                    (Some((_, node, _)), Some(l)) => {
                        let q = "mutation($review:ID!,$path:String!,$line:Int!,$body:String!){addPullRequestReviewThread(input:{pullRequestReviewId:$review,path:$path,line:$line,side:RIGHT,body:$body}){thread{comments(first:1){nodes{databaseId}}}}}";
                        let v = self.graphql(repo, q, json!({ "review": node, "path": l.path, "line": l.line, "body": draft.body })).await?;
                        Ok(u(&v, "/data/addPullRequestReviewThread/thread/comments/nodes/0/databaseId"))
                    }
                    (Some((id, _, old)), None) => {
                        let body = if old.trim().is_empty() { draft.body } else { format!("{old}\n\n{}", draft.body) };
                        let r = self.with(repo, Some(by), Http::Put, &format!("{}/reviews/{id}", self.pull(repo, number)), &json!({ "body": body })).await?;
                        self.ok("the pending review", r, &[200])?;
                        Ok(id)
                    }
                },
                Host::Gitlab => {
                    let mut body = json!({ "note": draft.body });
                    if let (Some(l), Some(o)) = (&draft.line, body.as_object_mut()) {
                        o.insert(
                            "position".into(),
                            json!({ "position_type": "text", "new_path": l.path, "new_line": l.line, "base_sha": s(&v, "/diff_refs/base_sha"), "start_sha": s(&v, "/diff_refs/start_sha"), "head_sha": s(&v, "/diff_refs/head_sha") }),
                        );
                    }
                    let r = self.with(repo, Some(by), Http::Post, &format!("{}/draft_notes", self.pull(repo, number)), &body).await?;
                    Ok(u(&self.ok("the draft note", r, &[200, 201])?.body, "/id"))
                }
                Host::Bitbucket => self.held.add(repo, number, Comment { id: 0, author: by.to_owned(), body: draft.body, line: draft.line, review: None }).await,
            }
        })
    }

    fn drafts<'a>(&'a self, repo: &'a str, number: u64, by: &'a str) -> BoxFuture<'a, Result<Vec<Comment>, Refusal>> {
        Box::pin(async move {
            match self.host {
                Host::Github => match self.pending(repo, number, by).await? {
                    Some((id, _, body)) => self.github_review_comments(repo, number, Some(by), (id, by, &body), false).await,
                    None => Ok(Vec::new()),
                },
                Host::Gitlab => self.gitlab_drafts(repo, number, by).await,
                Host::Bitbucket => self.held.list(repo, number, by).await,
            }
        })
    }

    fn publish<'a>(&'a self, repo: &'a str, number: u64, by: &'a str, verdict: Verdict) -> BoxFuture<'a, Result<Review, Refusal>> {
        Box::pin(async move {
            self.read_open(repo, number).await?;
            let mark = |id: u64, cs: Vec<Comment>| cs.into_iter().map(|c| Comment { review: Some(id), ..c }).collect::<Vec<_>>();
            match self.host {
                Host::Github => {
                    let event = match verdict {
                        Verdict::Approve => "APPROVE",
                        Verdict::RequestChanges => "REQUEST_CHANGES",
                        Verdict::Comment => "COMMENT",
                    };
                    let (id, comments) = match self.pending(repo, number, by).await? {
                        Some((id, _, body)) => {
                            let cs = self.github_review_comments(repo, number, Some(by), (id, by, &body), false).await?;
                            let r = self.with(repo, Some(by), Http::Post, &format!("{}/reviews/{id}/events", self.pull(repo, number)), &json!({ "event": event })).await?;
                            let r = self.ok("submitting the review", r, &[200])?;
                            let id = r.body.get("id").and_then(Value::as_u64).unwrap_or(id);
                            (id, cs)
                        }
                        None => {
                            let r = self.with(repo, Some(by), Http::Post, &format!("{}/reviews", self.pull(repo, number)), &json!({ "event": event })).await?;
                            (u(&self.ok("the review", r, &[200, 201])?.body, "/id"), Vec::new())
                        }
                    };
                    Ok(Review { id, author: by.to_owned(), verdict, comments: mark(id, comments) })
                }
                Host::Gitlab => {
                    let drafts = self.gitlab_drafts(repo, number, by).await?;
                    let r = self.with(repo, Some(by), Http::Post, &format!("{}/draft_notes/bulk_publish", self.pull(repo, number)), &json!({})).await?;
                    self.ok("publishing the draft notes", r, &[200, 201, 204])?;
                    if verdict == Verdict::Approve {
                        let r = self.with(repo, Some(by), Http::Post, &format!("{}/approve", self.pull(repo, number)), &json!({})).await?;
                        self.ok("approving", r, &[200, 201])?;
                    }
                    let id = drafts.first().map(|d| d.id).unwrap_or_default();
                    Ok(Review { id, author: by.to_owned(), verdict, comments: mark(id, drafts) })
                }
                Host::Bitbucket => {
                    let held = self.held.list(repo, number, by).await?;
                    let mut posted = Vec::new();
                    let mut done = Vec::new();
                    for c in &held {
                        let mut body = json!({ "content": { "raw": c.body } });
                        if let (Some(l), Some(o)) = (&c.line, body.as_object_mut()) {
                            o.insert("inline".into(), json!({ "path": l.path, "to": l.line }));
                        }
                        let r = self.with(repo, Some(by), Http::Post, &format!("{}/comments", self.pull(repo, number)), &body).await;
                        let r = match r.and_then(|r| self.ok("the comment", r, &[200, 201])) {
                            Ok(r) => r,
                            Err(e) => {
                                self.held.remove(&done).await?;
                                return Err(e);
                            }
                        };
                        done.push(c.id);
                        posted.push(Comment { id: u(&r.body, "/id"), ..c.clone() });
                    }
                    self.held.remove(&done).await?;
                    let path = match verdict {
                        Verdict::Approve => Some("approve"),
                        Verdict::RequestChanges => Some("request-changes"),
                        Verdict::Comment => None,
                    };
                    if let Some(p) = path {
                        let r = self.with(repo, Some(by), Http::Post, &format!("{}/{p}", self.pull(repo, number)), &json!({})).await?;
                        self.ok("the verdict", r, &[200, 201])?;
                    }
                    let id = posted.first().map(|c| c.id).unwrap_or_default();
                    Ok(Review { id, author: by.to_owned(), verdict, comments: mark(id, posted) })
                }
            }
        })
    }

    fn comments<'a>(&'a self, repo: &'a str, number: u64) -> BoxFuture<'a, Result<Vec<Comment>, Refusal>> {
        Box::pin(async move {
            match self.host {
                Host::Github => {
                    let r = self.get(repo, None, &format!("{}/reviews?per_page=100", self.pull(repo, number))).await?;
                    let reviews = self.ok("the reviews", r, &[200])?.body;
                    let r = self.get(repo, None, &format!("{}/comments?per_page=100", self.pull(repo, number))).await?;
                    let lines = self.ok("the review comments", r, &[200])?.body;
                    let mut out = Vec::new();
                    let mut submitted: Vec<&Value> = reviews.as_array().into_iter().flatten().filter(|x| s(x, "/state") != "PENDING").collect();
                    submitted.sort_by_key(|x| u(x, "/id"));
                    for x in submitted {
                        let (id, author) = (u(x, "/id"), s(x, "/user/login"));
                        let mut mine: Vec<&Value> = lines.as_array().into_iter().flatten().filter(|c| u(c, "/pull_request_review_id") == id).collect();
                        mine.sort_by_key(|c| u(c, "/id"));
                        out.extend(mine.into_iter().map(|c| Comment {
                            id: u(c, "/id"),
                            author: s(c, "/user/login"),
                            body: s(c, "/body"),
                            line: line_of(c.get("path").and_then(Value::as_str), c.get("line").and_then(Value::as_u64)),
                            review: Some(id),
                        }));
                        let body = s(x, "/body");
                        if !body.trim().is_empty() {
                            out.push(Comment { id, author, body, line: None, review: Some(id) });
                        }
                    }
                    Ok(out)
                }
                Host::Gitlab => {
                    let r = self.get(repo, None, &format!("{}/notes?sort=asc&order_by=created_at&per_page=100", self.pull(repo, number))).await?;
                    let r = self.ok("the notes", r, &[200])?;
                    Ok(r.body
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter(|n| n.get("system").and_then(Value::as_bool) != Some(true))
                        .map(|n| Comment {
                            id: u(n, "/id"),
                            author: s(n, "/author/username"),
                            body: s(n, "/body"),
                            line: line_of(n.pointer("/position/new_path").and_then(Value::as_str), n.pointer("/position/new_line").and_then(Value::as_u64)),
                            review: None,
                        })
                        .collect())
                }
                Host::Bitbucket => {
                    let r = self.get(repo, None, &format!("{}/comments?pagelen=100", self.pull(repo, number))).await?;
                    let r = self.ok("the comments", r, &[200])?;
                    Ok(arr(&r.body, "/values")
                        .filter(|c| c.get("deleted").and_then(Value::as_bool) != Some(true))
                        .map(|c| Comment {
                            id: u(c, "/id"),
                            author: [s(c, "/user/nickname"), s(c, "/user/display_name")].into_iter().find(|a| !a.is_empty()).unwrap_or_default(),
                            body: s(c, "/content/raw"),
                            line: line_of(c.pointer("/inline/path").and_then(Value::as_str), c.pointer("/inline/to").and_then(Value::as_u64)),
                            review: None,
                        })
                        .collect())
                }
            }
        })
    }

    fn rate<'a>(&'a self, repo: &'a str) -> BoxFuture<'a, Result<Rate, Refusal>> {
        Box::pin(async move {
            if self.host == Host::Github {
                let r = self.send(repo, None, Http::Get, "/rate_limit", None, None, true).await?;
                let v = self.ok("the rate limit", r, &[200])?.body;
                let rate = Rate {
                    limit: u32::try_from(u(&v, "/resources/core/limit")).unwrap_or(u32::MAX),
                    remaining: u32::try_from(u(&v, "/resources/core/remaining")).unwrap_or(u32::MAX),
                    resets_at: u(&v, "/resources/core/reset"),
                };
                self.st.lock().rate = Some(rate);
                return Ok(rate);
            }
            let known = self.st.lock().rate;
            if let Some(r) = known {
                return Ok(r);
            }
            let r = self.get(repo, None, &self.base(repo)).await?;
            self.ok("the repository", r, &[200])?;
            let now = (self.now)();
            Ok(self.st.lock().rate.unwrap_or(Rate { limit: u32::MAX, remaining: u32::MAX, resets_at: now.saturating_add(3600) }))
        })
    }
}

/// The forge for `repo` on its host, as kiln acts there: its forge and the repository's name on
/// the host. Kiln's own git server has none (its reviews are kiln's).
pub async fn for_repo(hub: &Arc<Hub>, repo: &Repo) -> Option<(HostForge, String)> {
    let gh = super::for_repo(hub, repo).await?;
    let (host, api, cred, name) = match &gh {
        GitHost::Kiln => return None,
        GitHost::Github(app) => (Host::Github, app.api.clone(), Cred::App(Arc::clone(app)), app.name_of(repo)?),
        GitHost::Gitlab(g) => (Host::Gitlab, g.api.clone(), Cred::Header(g.auth_header()), g.name_of(repo)?),
        GitHost::Bitbucket(b) => (Host::Bitbucket, b.api.clone(), Cred::Header(b.api_auth()), b.name_of(repo)?),
    };
    let (h, r) = (Arc::clone(hub), repo.clone());
    let push: Pusher = Box::new(move |_, branch, sha, expected| {
        let (h, r, gh) = (Arc::clone(&h), r.clone(), gh.clone());
        Box::pin(async move {
            let local = crate::gitops::mirror_path(&h, &r.name).to_string_lossy().into_owned();
            gh.push(&h, &r, &local, &sha, &branch, Some(&expected)).await.map_err(|e| match e {
                super::CommitError::Moved => Refusal::Moved { head: String::new() },
                super::CommitError::Other(e) => Refusal::Unavailable(format!("{e:#}")),
            })
        })
    });
    let now: Now = Box::new(|| u64::try_from(crate::util::now_unix()).unwrap_or_default());
    Some((HostForge::new(host, &api, cred, Box::new(|_| None), push, now, Arc::new(StoreHeld(Arc::clone(hub)))), name))
}

/// A repository's open changes followed within the host's rate limit (W26-AC09): a webhook for a
/// change reads that change (one request), and a poll reads the open changes with their ETag, so
/// a poll that finds nothing new is free on GitHub.
#[derive(Default)]
pub struct Watch {
    list: Option<String>,
    tags: BTreeMap<u64, String>,
    /// The open changes as last read, by number.
    pub changes: BTreeMap<u64, Change>,
}

impl Watch {
    /// A poll: the changes that changed since the last.
    pub async fn poll(&mut self, forge: &dyn Forge, repo: &str) -> Result<Vec<u64>, Refusal> {
        let Fresh::Changed { value, etag } = forge.changes(repo, self.list.as_deref()).await? else { return Ok(Vec::new()) };
        self.list = Some(etag);
        let mut moved: Vec<u64> = value.iter().filter(|c| self.changes.get(&c.number) != Some(*c)).map(|c| c.number).collect();
        moved.extend(self.changes.keys().filter(|n| !value.iter().any(|c| c.number == **n)).copied());
        self.changes = value.into_iter().map(|c| (c.number, c)).collect();
        moved.sort_unstable();
        moved.dedup();
        Ok(moved)
    }

    /// A webhook said change `number` changed: whether it did.
    pub async fn hook(&mut self, forge: &dyn Forge, repo: &str, number: u64) -> Result<bool, Refusal> {
        match forge.change(repo, number, self.tags.get(&number).map(String::as_str)).await? {
            Fresh::NotModified => Ok(false),
            Fresh::Changed { value, etag } => {
                self.tags.insert(number, etag);
                let changed = self.changes.get(&number) != Some(&value);
                if value.state == State::Open {
                    self.changes.insert(number, value);
                } else {
                    self.changes.remove(&number);
                    self.tags.remove(&number);
                }
                Ok(changed)
            }
        }
    }
}
