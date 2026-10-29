//! The hosts' pull requests, mirrored for Ship (docs/ci.md W26-R01, R20): each GitHub pull request,
//! GitLab merge request and Bitbucket pull request of a repository kiln has on that host, kept in
//! `host_pulls` (with the heads it has had and the commits its latest head brings), so Ship's read
//! model shows it by commit beside kiln's own diffs. Within the hosts' rate limits: a webhook marks
//! its pull request dirty and it's read once its burst settles; the list of open ones is read once
//! a minute as the backstop; every read sends the ETag it last got, so what hasn't changed answers
//! 304, which GitHub doesn't count. When to read what is `kiln_ci::mirror`'s to say; this reads and
//! writes.

use std::collections::HashMap;
use std::sync::Arc;

use kiln_ci::mirror::{Cadence, Mirror, Want};
use rusqlite::{OptionalExtension as _, params};
use serde_json::{Value, json};

use super::pulls::{self, On};
use crate::hub::Hub;
use crate::store::{Repo, Store};

/// What a read got: its status (304: unchanged since the ETag sent), its ETag, its JSON.
#[derive(Debug, Clone)]
pub struct Got {
    pub status: u16,
    pub etag: Option<String>,
    pub body: Value,
}

/// Conditional reads of a host's API.
pub trait Fetch {
    fn get(&self, path: &str, etag: Option<&str>) -> impl std::future::Future<Output = anyhow::Result<Got>>;
}

/// Which host a repository's pull requests are on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Host {
    Github,
    Gitlab,
    Bitbucket,
}

impl Host {
    pub fn word(self) -> &'static str {
        match self {
            Host::Github => "github",
            Host::Gitlab => "gitlab",
            Host::Bitbucket => "bitbucket",
        }
    }

    fn list(self, name: &str) -> String {
        match self {
            Host::Github => format!("/repos/{name}/pulls?state=open&per_page=100"),
            Host::Gitlab => format!("/projects/{}/merge_requests?state=opened&per_page=100", super::gitlab::id(name)),
            Host::Bitbucket => format!("/repositories/{name}/pullrequests?state=OPEN&pagelen=50"),
        }
    }

    /// A pull request's reads: itself, its commits, its reviews (Bitbucket's are on the pull
    /// request itself), its comments.
    fn reads(self, name: &str, n: u64) -> (String, String, Option<String>, String) {
        match self {
            Host::Github => (format!("/repos/{name}/pulls/{n}"), format!("/repos/{name}/pulls/{n}/commits?per_page=100"), Some(format!("/repos/{name}/pulls/{n}/reviews?per_page=100")), format!("/repos/{name}/pulls/{n}/comments?per_page=100")),
            Host::Gitlab => {
                let id = super::gitlab::id(name);
                (format!("/projects/{id}/merge_requests/{n}"), format!("/projects/{id}/merge_requests/{n}/commits?per_page=100"), Some(format!("/projects/{id}/merge_requests/{n}/approvals")), format!("/projects/{id}/merge_requests/{n}/discussions?per_page=100"))
            }
            Host::Bitbucket => (format!("/repositories/{name}/pullrequests/{n}"), format!("/repositories/{name}/pullrequests/{n}/commits?pagelen=100"), None, format!("/repositories/{name}/pullrequests/{n}/comments?pagelen=100")),
        }
    }

    fn principal(self, login: &str) -> String {
        format!("{}:{login}", self.word())
    }
}

/// The open pull requests a list shows: each one's number and `updated_at`.
fn listed(host: Host, v: &Value) -> Vec<(u64, String)> {
    let (items, number, updated) = match host {
        Host::Github => (v.as_array(), "/number", "/updated_at"),
        Host::Gitlab => (v.as_array(), "/iid", "/updated_at"),
        Host::Bitbucket => (v.get("values").and_then(Value::as_array), "/id", "/updated_on"),
    };
    items.into_iter().flatten().filter_map(|p| Some((p.pointer(number)?.as_u64()?, p.pointer(updated).and_then(Value::as_str).unwrap_or_default().to_owned()))).collect()
}

fn text(v: &Value, p: &str) -> String {
    v.pointer(p).and_then(Value::as_str).unwrap_or_default().to_owned()
}

fn unix(s: &str) -> i64 {
    crate::ci::providers::unix(Some(&Value::String(s.to_owned()))).unwrap_or(0)
}

/// A pull request as read: what `host_pulls` keeps of it.
#[derive(Debug, Clone, Default, PartialEq)]
struct Pull {
    title: String,
    author: String,
    state: String,
    url: String,
    base: String,
    head: String,
    requested: Vec<String>,
    merged_sha: String,
    updated: String,
}

fn pull_of(host: Host, v: &Value) -> Pull {
    let shaped = match host {
        Host::Github => pulls::gh_pull(v),
        Host::Gitlab => pulls::gl_pull(v),
        Host::Bitbucket => pulls::bb_pull(v),
    };
    let draft = shaped.get("draft").and_then(Value::as_bool).unwrap_or(false);
    let state = match text(&shaped, "/state").as_str() {
        "merged" => "merged",
        "closed" => "closed",
        _ if draft => "draft",
        _ => "open",
    };
    let merged_sha = match host {
        Host::Github | Host::Gitlab if state == "merged" => v.get("merge_commit_sha").or_else(|| v.get("squash_commit_sha")).and_then(Value::as_str).unwrap_or_default().to_owned(),
        Host::Bitbucket if state == "merged" => text(v, "/merge_commit/hash"),
        _ => String::new(),
    };
    Pull {
        title: text(&shaped, "/title"),
        author: host.principal(&text(&shaped, "/author")),
        state: state.to_owned(),
        url: text(&shaped, "/url"),
        base: text(&shaped, "/base"),
        head: text(&shaped, "/sha"),
        requested: shaped.get("reviewers").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(|l| host.principal(l)).collect(),
        merged_sha,
        updated: text(v, if host == Host::Bitbucket { "/updated_on" } else { "/updated_at" }),
    }
}

/// A pull request's commits, oldest first.
fn commits_of(host: Host, v: &Value) -> Vec<String> {
    match host {
        Host::Github => v.as_array().into_iter().flatten().map(|c| text(c, "/sha")).collect(),
        Host::Gitlab => v.as_array().into_iter().flatten().rev().map(|c| text(c, "/id")).collect(),
        Host::Bitbucket => v.get("values").and_then(Value::as_array).into_iter().flatten().rev().map(|c| text(c, "/hash")).collect(),
    }
}

/// A pull request's reviews: who, approve | request_changes | comment, when (unix s).
fn reviews_of(host: Host, v: &Value) -> Vec<Value> {
    let review = |by: String, verdict: &str, at: i64| json!({ "by": by, "verdict": verdict, "at": at });
    match host {
        Host::Github => v
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|r| {
                let verdict = match text(r, "/state").as_str() {
                    "APPROVED" => "approve",
                    "CHANGES_REQUESTED" => "request_changes",
                    "COMMENTED" => "comment",
                    _ => return None,
                };
                Some(review(host.principal(&text(r, "/user/login")), verdict, unix(&text(r, "/submitted_at"))))
            })
            .collect(),
        Host::Gitlab => v.get("approved_by").and_then(Value::as_array).into_iter().flatten().map(|a| review(host.principal(&text(a, "/user/username")), "approve", 0)).collect(),
        Host::Bitbucket => v
            .get("participants")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|p| {
                let who = p.pointer("/user/nickname").or_else(|| p.pointer("/user/display_name")).and_then(Value::as_str).unwrap_or_default();
                let verdict = if p.get("approved").and_then(Value::as_bool).unwrap_or(false) {
                    "approve"
                } else if p.get("state").and_then(Value::as_str) == Some("changes_requested") {
                    "request_changes"
                } else {
                    return None;
                };
                Some(review(host.principal(who), verdict, unix(&text(p, "/participated_on"))))
            })
            .collect(),
    }
}

/// A pull request's open threads, and the latest one's author and words.
fn threads_of(host: Host, v: &Value) -> (i64, String, String) {
    let open: Vec<(String, String)> = match host {
        Host::Github => v.as_array().into_iter().flatten().filter(|c| c.get("in_reply_to_id").is_none_or(Value::is_null) && c.get("position").is_some_and(|p| !p.is_null())).map(|c| (host.principal(&text(c, "/user/login")), text(c, "/body"))).collect(),
        Host::Gitlab => v
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|d| {
                let first = d.pointer("/notes/0")?;
                (first.get("resolvable").and_then(Value::as_bool) == Some(true) && first.get("resolved").and_then(Value::as_bool) != Some(true)).then(|| (host.principal(&text(first, "/author/username")), text(first, "/body")))
            })
            .collect(),
        Host::Bitbucket => v
            .get("values")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|c| c.get("parent").is_none_or(Value::is_null) && c.get("inline").is_some_and(|i| !i.is_null()) && c.get("deleted").and_then(Value::as_bool) != Some(true))
            .map(|c| (host.principal(&text(c, "/user/nickname")), text(c, "/content/raw")))
            .collect(),
    };
    let (by, said) = open.last().cloned().unwrap_or_default();
    (i64::try_from(open.len()).unwrap_or(i64::MAX), by, said)
}

/// A repository's mirror as it goes: when to read what, and the ETag each read last got.
#[derive(Debug, Default)]
pub struct State {
    pub mirror: Mirror,
    etags: HashMap<String, String>,
}

impl State {
    pub fn new(cadence: Cadence) -> Self {
        Self { mirror: Mirror::new(cadence), etags: HashMap::new() }
    }
}

/// A repository whose pull requests are mirrored: its id in kiln, its host and its name there.
#[derive(Debug, Clone)]
pub struct Mirrored {
    pub repo_id: String,
    pub host: Host,
    pub name: String,
}

async fn read<F: Fetch>(fetch: &F, st: &mut State, path: &str) -> anyhow::Result<Got> {
    let got = fetch.get(path, st.etags.get(path).map(String::as_str)).await?;
    if got.status == 200
        && let Some(tag) = &got.etag
    {
        st.etags.insert(path.to_owned(), tag.clone());
    }
    Ok(got)
}

/// Reads what's due at `now` (ms) and writes what changed: the list when it's due, then each pull
/// request due (those the list just showed changed among them, each read with the list's copy of
/// it where the host's list carries the whole pull request, as GitHub's does).
pub async fn step<F: Fetch>(store: &Store, r: &Mirrored, fetch: &F, st: &mut State, now: i64) -> anyhow::Result<()> {
    let mut items: HashMap<u64, Value> = HashMap::new();
    if st.mirror.due(now).first() == Some(&Want::List) {
        let got = read(fetch, st, &r.host.list(&r.name)).await?;
        let open = match got.status {
            304 => None,
            200 => Some(listed(r.host, &got.body)),
            code => anyhow::bail!("{}'s pull requests of {} ({code})", r.host.word(), r.name),
        };
        if r.host == Host::Github {
            items = got.body.as_array().into_iter().flatten().filter_map(|p| Some((p.get("number")?.as_u64()?, p.clone()))).collect();
        }
        // Those it no longer shows: read once more, to learn how they ended.
        for n in st.mirror.listed(now, open.as_deref()) {
            refresh(store, r, fetch, st, n, None, now).await?;
        }
    }
    for want in st.mirror.due(now) {
        if let Want::Pull(n) = want {
            refresh(store, r, fetch, st, n, items.remove(&n), now).await?;
        }
    }
    Ok(())
}

/// Reads one pull request and writes what changed of it (each read that answered 304 leaves its
/// part as it was): commits before the head, so Ship numbers the head newest.
async fn refresh<F: Fetch>(store: &Store, r: &Mirrored, fetch: &F, st: &mut State, n: u64, listed: Option<Value>, now: i64) -> anyhow::Result<()> {
    let (pull_path, commits_path, reviews_path, comments_path) = r.host.reads(&r.name, n);
    let pull = match listed {
        Some(body) => Got { status: 200, etag: None, body },
        None => read(fetch, st, &pull_path).await?,
    };
    if pull.status == 404 {
        let (repo, host, num) = (r.repo_id.clone(), r.host.word(), i64::try_from(n).unwrap_or(i64::MAX));
        store.call(move |c| Ok(c.execute("UPDATE host_pulls SET state = 'closed' WHERE repo_id = ?1 AND host = ?2 AND number = ?3 AND state NOT IN ('merged', 'closed')", params![repo, host, num]).map(|_| ())?)).await?;
        st.mirror.fetched(n, None);
        return Ok(());
    }
    let commits = read(fetch, st, &commits_path).await?;
    let reviews = match &reviews_path {
        Some(p) => Some(read(fetch, st, p).await?),
        None => None,
    };
    let comments = read(fetch, st, &comments_path).await?;
    let fresh = |g: &Got| g.status == 200;
    let p = fresh(&pull).then(|| pull_of(r.host, &pull.body));
    let commits = fresh(&commits).then(|| commits_of(r.host, &commits.body));
    let reviews = match (r.host, &reviews) {
        (Host::Bitbucket, _) => fresh(&pull).then(|| reviews_of(r.host, &pull.body)),
        (_, Some(g)) => fresh(g).then(|| reviews_of(r.host, &g.body)),
        (_, None) => None,
    };
    let threads = fresh(&comments).then(|| threads_of(r.host, &comments.body));
    let updated = p.as_ref().map(|p| p.updated.clone());
    let (repo, host, num, seen) = (r.repo_id.clone(), r.host.word(), i64::try_from(n).unwrap_or(i64::MAX), now / 1000);
    let open = store
        .call(move |c| {
            let tx = c.savepoint()?;
            if let Some(p) = &p {
                tx.execute(
                    "INSERT INTO host_pulls (repo_id, host, number, title, author, state, url, base, requested, merged_sha, host_updated, first_seen) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
                     ON CONFLICT (repo_id, host, number) DO UPDATE SET title = excluded.title, author = excluded.author, state = excluded.state, url = excluded.url, base = excluded.base,
                       requested = excluded.requested, merged_sha = excluded.merged_sha, host_updated = excluded.host_updated",
                    params![repo, host, num, p.title, p.author, p.state, p.url, p.base, serde_json::to_string(&p.requested)?, p.merged_sha, p.updated, seen],
                )?;
            }
            let known: bool = tx.query_row("SELECT EXISTS (SELECT 1 FROM host_pulls WHERE repo_id = ?1 AND host = ?2 AND number = ?3)", params![repo, host, num], |r| r.get(0))?;
            if !known {
                tx.commit()?;
                return Ok(None);
            }
            if let Some(rv) = &reviews {
                tx.execute("UPDATE host_pulls SET reviews = ?4 WHERE repo_id = ?1 AND host = ?2 AND number = ?3", params![repo, host, num, serde_json::to_string(rv)?])?;
            }
            if let Some((count, by, said)) = &threads {
                tx.execute("UPDATE host_pulls SET threads = ?4, said_by = ?5, said = ?6 WHERE repo_id = ?1 AND host = ?2 AND number = ?3", params![repo, host, num, count, by, said])?;
            }
            let head = match &p {
                Some(p) => p.head.clone(),
                None => tx.query_row("SELECT sha FROM host_pull_heads WHERE repo_id = ?1 AND host = ?2 AND number = ?3 ORDER BY n DESC LIMIT 1", params![repo, host, num], |r| r.get(0)).optional()?.unwrap_or_default(),
            };
            if let Some(cs) = &commits {
                tx.execute("DELETE FROM host_pull_commits WHERE repo_id = ?1 AND host = ?2 AND number = ?3", params![repo, host, num])?;
                for (i, sha) in cs.iter().filter(|s| !s.is_empty() && **s != head).enumerate() {
                    tx.execute("INSERT OR IGNORE INTO host_pull_commits (repo_id, host, number, sha, position) VALUES (?1, ?2, ?3, ?4, ?5)", params![repo, host, num, sha, i64::try_from(i).unwrap_or(i64::MAX)])?;
                }
            }
            let last: Option<(i64, String)> = tx.query_row("SELECT n, sha FROM host_pull_heads WHERE repo_id = ?1 AND host = ?2 AND number = ?3 ORDER BY n DESC LIMIT 1", params![repo, host, num], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
            if !head.is_empty() && last.as_ref().is_none_or(|(_, s)| *s != head) {
                tx.execute("INSERT INTO host_pull_heads (repo_id, host, number, n, sha, seen_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)", params![repo, host, num, last.map_or(1, |(k, _)| k + 1), head, seen])?;
            }
            let state: String = tx.query_row("SELECT state FROM host_pulls WHERE repo_id = ?1 AND host = ?2 AND number = ?3", params![repo, host, num], |r| r.get(0))?;
            let updated_now: String = tx.query_row("SELECT host_updated FROM host_pulls WHERE repo_id = ?1 AND host = ?2 AND number = ?3", params![repo, host, num], |r| r.get(0))?;
            tx.commit()?;
            Ok(matches!(state.as_str(), "open" | "draft").then_some(updated_now))
        })
        .await?;
    st.mirror.fetched(n, open.map(|u| updated.unwrap_or(u)));
    Ok(())
}

/// Sends a conditional GET (`If-None-Match: etag`): its status, its ETag and its JSON (none on a
/// 304).
pub(crate) async fn send_if(req: crate::http::client::RequestBuilder<'_>, etag: Option<&str>) -> anyhow::Result<(u16, Option<String>, Value)> {
    let req = match etag {
        Some(e) => req.header("If-None-Match", e),
        None => req,
    };
    let resp = req.timeout(std::time::Duration::from_secs(30)).send().await?;
    let (status, tag) = (resp.status(), resp.header("etag").map(str::to_owned));
    let bytes = resp.bytes(32 << 20).await?;
    let v = if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned())) };
    Ok((status, tag, v))
}

/// A host's API at `base`, read with `authorization` (its `Authorization` header).
pub struct Http {
    pub base: String,
    pub authorization: String,
}

impl Fetch for Http {
    async fn get(&self, path: &str, etag: Option<&str>) -> anyhow::Result<Got> {
        let req = crate::http::client::github()?.request(crate::http::client::Method::Get, &format!("{}{path}", self.base)).header("Authorization", &self.authorization).header("Accept", "application/json");
        let (status, etag, body) = send_if(req, etag).await?;
        Ok(Got { status, etag, body })
    }
}

/// Reads through kiln's credentials on the repository's host.
pub struct Live(On);

impl Fetch for Live {
    async fn get(&self, path: &str, etag: Option<&str>) -> anyhow::Result<Got> {
        let (status, etag, body) = match &self.0 {
            On::Github(app, name) => {
                let token = pulls::gh_token(app, name, false).await?;
                app.get_if(path, &token, etag).await?
            }
            On::Gitlab(g, _) => g.get_if(path, etag).await?,
            On::Bitbucket(b, _) => b.get_if(path, etag).await?,
        };
        Ok(Got { status, etag, body })
    }
}

/// Webhooks waiting for their repository's next step: (repo id, number, when, ms).
fn hooks() -> &'static parking_lot::Mutex<Vec<(String, u64, i64)>> {
    static H: std::sync::OnceLock<parking_lot::Mutex<Vec<(String, u64, i64)>>> = std::sync::OnceLock::new();
    H.get_or_init(Default::default)
}

/// A webhook named pull request `number` of a repository.
pub fn hook(repo_id: &str, number: u64) {
    let mut h = hooks().lock();
    // Bounded: a flood of webhooks keeps the newest 10,000 (the list's backstop catches the rest).
    if h.len() >= 10_000 {
        h.remove(0);
    }
    h.push((repo_id.to_owned(), number, crate::util::now_ms()));
}

/// GitHub's webhooks about a pull request (its own events, its reviews, their comments, and the
/// conversation's comments on one).
pub async fn on_github_event(hub: &Hub, event: &str, v: &Value) {
    let number = match event {
        "pull_request" | "pull_request_review" | "pull_request_review_comment" | "pull_request_review_thread" => v.pointer("/pull_request/number").and_then(Value::as_u64),
        "issue_comment" if v.pointer("/issue/pull_request").is_some() => v.pointer("/issue/number").and_then(Value::as_u64),
        _ => None,
    };
    let (Some(number), Some(full)) = (number, v.pointer("/repository/full_name").and_then(Value::as_str)) else { return };
    let Some(app) = super::github::app(hub).await else { return };
    if let Some(repo) = hub.store.repos().await.unwrap_or_default().into_iter().find(|r| app.name_of(r).is_some_and(|n| n.eq_ignore_ascii_case(full))) {
        hook(&repo.id, number);
    }
}

/// GitLab's merge request events and their notes.
pub fn on_gitlab_event(repo: &Repo, event: &str, v: &Value) {
    let number = match event {
        "Merge Request Hook" => v.pointer("/object_attributes/iid").and_then(Value::as_u64),
        "Note Hook" => v.pointer("/merge_request/iid").and_then(Value::as_u64),
        _ => None,
    };
    if let Some(n) = number {
        hook(&repo.id, n);
    }
}

/// Bitbucket's pull request events (`pullrequest:*`).
pub fn on_bitbucket_event(repo: &Repo, event: &str, v: &Value) {
    if event.starts_with("pullrequest:")
        && let Some(n) = v.pointer("/pullrequest/id").and_then(Value::as_u64)
    {
        hook(&repo.id, n);
    }
}

/// Keeps every repository on a host mirrored, a step every two seconds.
pub async fn run(hub: Arc<Hub>) -> anyhow::Result<()> {
    let mut states: HashMap<String, State> = HashMap::new();
    loop {
        let now = crate::util::now_ms();
        let pending: Vec<(String, u64, i64)> = std::mem::take(&mut *hooks().lock());
        for repo in hub.store.repos().await.unwrap_or_default() {
            let Ok(on) = pulls::on(&hub, &repo).await else { continue };
            let host = match &on {
                On::Github(..) => Host::Github,
                On::Gitlab(..) => Host::Gitlab,
                On::Bitbucket(..) => Host::Bitbucket,
            };
            let name = match &on {
                On::Github(_, n) | On::Gitlab(_, n) | On::Bitbucket(_, n) => n.clone(),
            };
            let st = states.entry(repo.id.clone()).or_default();
            for (_, n, at) in pending.iter().filter(|(id, ..)| *id == repo.id) {
                st.mirror.hook(*n, *at);
            }
            let r = Mirrored { repo_id: repo.id.clone(), host, name };
            if let Err(e) = step(&hub.store, &r, &Live(on), st, now).await {
                tracing::debug!(repo = %repo.name, error = %format!("{e:#}"), "a host's pull requests didn't mirror");
            }
        }
        crate::rt::sleep(std::time::Duration::from_secs(2)).await;
    }
}
