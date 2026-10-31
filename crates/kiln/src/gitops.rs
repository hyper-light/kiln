//! GitOps: kiln pulls, it is never pushed to (webhooks only *hint* that a poll is due).
//!
//! * Discovery lists the configured GitHub orgs' public repositories (one unauthenticated API
//!   call per org every `discover_seconds`).
//! * Polling uses `git ls-remote`, which costs no API quota and needs no token, and sees branches,
//!   tags and pull-request heads (`refs/pull/N/head`).
//! * Pipelines are read from a local bare mirror with `git show <sha>:.kiln/<file>`; the same mirror
//!   serves source tarballs to Kubernetes job pods so they need neither git nor GitHub access.
//! * Git never prompts: `GIT_TERMINAL_PROMPT=0`, so a private repo without credentials fails fast
//!   and visibly instead of hanging on a password prompt.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use std::process::Command;

use futures::FutureExt as _;

use crate::hub::Hub;
use crate::pipeline;
use crate::store::{Repo, Run, Status};

const GIT_TIMEOUT: Duration = Duration::from_secs(600);
pub(crate) const LS_REMOTE_TIMEOUT: Duration = Duration::from_secs(60);

/// The shared transport supplies noninteractive Git and host-scoped credentials (U08).
pub(crate) fn git(hub: &Hub) -> Command {
    crate::git::command(hub)
}

pub(crate) async fn run_git(hub: &Hub, args: &[&str], timeout: Duration) -> anyhow::Result<Vec<u8>> {
    let mut cmd = git(hub);
    cmd.args(args);
    crate::git::run(cmd, timeout).await.with_context(|| format!("git {}", args.first().unwrap_or(&"")))
}

pub fn mirror_path(hub: &Hub, repo: &str) -> PathBuf {
    crate::git::logical_path(hub, &legacy_mirror(hub, repo))
}

fn legacy_mirror(hub: &Hub, repo: &str) -> PathBuf {
    hub.cfg.data("git").join(format!("{}.git", repo.replace('/', "__")))
}

fn default_url(name: &str) -> String {
    format!("https://github.com/{name}.git")
}

/// `ref → sha` for heads, tags and PR heads.
pub async fn ls_remote(hub: &Hub, url: &str) -> anyhow::Result<BTreeMap<String, String>> {
    let out = run_git(
        hub,
        &["ls-remote", "--symref", url, "HEAD", "refs/heads/*", "refs/tags/*", "refs/pull/*/head"],
        LS_REMOTE_TIMEOUT,
    )
    .await?;
    let mut refs = BTreeMap::new();
    for line in String::from_utf8_lossy(&out).lines() {
        if let Some(rest) = line.strip_prefix("ref: ") {
            // `ref: refs/heads/main\tHEAD`
            if let Some((target, _)) = rest.split_once('\t') {
                refs.insert("HEAD->".to_owned(), target.to_owned());
            }
            continue;
        }
        let Some((sha, name)) = line.split_once('\t') else { continue };
        // Peeled tags (`^{}`) point at the commit; prefer them.
        if let Some(tag) = name.strip_suffix("^{}") {
            refs.insert(tag.to_owned(), sha.to_owned());
        } else {
            refs.entry(name.to_owned()).or_insert_with(|| sha.to_owned());
        }
    }
    Ok(refs)
}

/// Whether `ls_remote` found anything to build: an empty repository has no refs, at most the
/// symbolic HEAD of its unborn default branch.
fn has_commits(refs: &BTreeMap<String, String>) -> bool {
    refs.keys().any(|k| k != "HEAD->")
}

/// U08: CI's read mirror uses the same clone/fetch engine and directory gate as CD.
pub async fn sync_mirror(hub: &Hub, repo: &Repo) -> anyhow::Result<PathBuf> {
    let mirror = crate::git::open_read(
        hub,
        &legacy_mirror(hub, &repo.name),
        &repo.clone_url,
        kiln_cd::git::Options { refs: kiln_cd::git::Refs::HeadsTagsPulls, ..Default::default() },
        GIT_TIMEOUT,
    ).await?;
    mirror.fetch_using(|cmd| crate::git::run(cmd, GIT_TIMEOUT)).await?;
    Ok(mirror.dir)
}

/// The mirror, with commit `sha` in it whole: fetched when it isn't there.
pub(crate) async fn ensure_commit(hub: &Hub, repo: &Repo, sha: &str) -> anyhow::Result<PathBuf> {
    let mirror = crate::git::open_read(hub, &legacy_mirror(hub, &repo.name), &repo.clone_url,
        kiln_cd::git::Options { refs: kiln_cd::git::Refs::HeadsTagsPulls, ..Default::default() }, GIT_TIMEOUT).await?;
    let p = mirror.dir.to_string_lossy().to_string();
    let have = || async { run_git(hub, &["-C", &p, "cat-file", "-e", &format!("{sha}^{{commit}}")], LS_REMOTE_TIMEOUT).await };
    if have().await.is_err() {
        mirror.fetch_using(|cmd| crate::git::run(cmd, GIT_TIMEOUT)).await?;
        if have().await.is_err() {
            mirror.fetch_one_using(sha, |cmd| crate::git::run(cmd, GIT_TIMEOUT)).await?;
        }
    }
    mirror.retain_using(sha, |cmd| crate::git::run(cmd, GIT_TIMEOUT)).await?;
    Ok(mirror.dir)
}

pub(crate) async fn commit_info(hub: &Hub, repo: &Repo, sha: &str) -> (String, String) {
    let p = mirror_path(hub, &repo.name).to_string_lossy().to_string();
    match run_git(hub, &["-C", &p, "log", "-1", "--format=%an%x00%B", sha], LS_REMOTE_TIMEOUT).await {
        Ok(o) => {
            let s = String::from_utf8_lossy(&o);
            let (a, m) = s.split_once('\0').unwrap_or(("", &s));
            (m.trim().to_owned(), a.to_owned())
        }
        Err(_) => (String::new(), String::new()),
    }
}

/// Where `repo`'s commits are read: kiln's own repository when kiln hosts it, whose diffs'
/// versions (`refs/kiln/diffs/…`) its mirror never fetches, else its mirror.
pub(crate) fn git_dir(hub: &Hub, repo: &Repo) -> PathBuf {
    crate::ci::hosting::native_path(hub, repo).unwrap_or_else(|| mirror_path(hub, &repo.name))
}

pub(crate) async fn changed_files(hub: &Hub, repo: &Repo, old: &str, new: &str) -> Option<Vec<String>> {
    let p = git_dir(hub, repo).to_string_lossy().to_string();
    let o = run_git(hub, &["-C", &p, "diff", "--name-only", old, new], LS_REMOTE_TIMEOUT).await.ok()?;
    Some(String::from_utf8_lossy(&o).lines().map(str::to_owned).collect())
}

/// A pull request's counts in its payload (W03-R07), as GitHub's: the commits `base..head`, and
/// the lines and files `base...head` changes (0 each that git can't say).
pub(crate) async fn pr_counts(hub: &Hub, repo: &Repo, base: &str, head: &str) -> kiln_ci::trigger::context::Counts {
    let mut c = kiln_ci::trigger::context::Counts::default();
    if base.is_empty() || head.is_empty() {
        return c;
    }
    let p = git_dir(hub, repo).to_string_lossy().to_string();
    if let Ok(o) = run_git(hub, &["-C", &p, "rev-list", "--count", &format!("{base}..{head}")], LS_REMOTE_TIMEOUT).await {
        c.commits = String::from_utf8_lossy(&o).trim().parse().unwrap_or(0);
    }
    if let Ok(o) = run_git(hub, &["-C", &p, "diff", "--numstat", &format!("{base}...{head}")], LS_REMOTE_TIMEOUT).await {
        for l in String::from_utf8_lossy(&o).lines() {
            // A binary file's lines are `-`: 0, as GitHub counts them.
            let mut n = l.split('\t').map(|n| n.parse::<u64>().unwrap_or(0));
            c.additions += n.next().unwrap_or(0);
            c.deletions += n.next().unwrap_or(0);
            c.changed_files += 1;
        }
    }
    c
}

#[derive(Debug, Clone)]
pub struct Trigger {
    pub event: &'static str,
    pub branch: String,
    pub sha: String,
    pub before: Option<String>,
    /// Restrict to one pipeline (manual runs).
    pub pipeline: Option<String>,
    /// GitHub's payload, when a webhook delivered the change (docs/ci.md W03-R07).
    pub payload: Option<std::sync::Arc<serde_json::Value>>,
}

/// Turn a trigger into recorded workflow runs through the sole CI engine (U01, W02-R06).
pub async fn trigger(hub: &Hub, repo: &Repo, t: Trigger) -> anyhow::Result<Vec<Run>> {
    let u = &hub.cfg.updates;
    if t.event == "push" && repo.name == u.release_repo && t.branch == u.release_branch {
        hub.updates.wake_in_step();
    }
    crate::ci::events::on_trigger(hub, repo, &t).await
}

/// Poll one repository: diff refs against what we last saw and trigger runs for movement.
pub async fn poll_repo(hub: &Hub, repo: &Repo) -> anyhow::Result<usize> {
    // Paused ingress must retain its last accepted refs, including the initial baseline.
    // Re-enabling CI then observes movement instead of treating unbuilt commits as accepted.
    if !hub.ci.enabled() { return Ok(0); }
    let remote = ls_remote(hub, &repo.clone_url).await?;
    let known: HashMap<String, String> = hub.store.refs(repo.id.clone()).await?.into_iter().collect();
    let first_poll = known.is_empty();
    let mut triggered = 0;
    for (name, sha) in &remote {
        if name == "HEAD->" {
            hub.store.set_ref(repo.id.clone(), name.clone(), sha.clone()).await?;
            continue;
        }
        let old = known.get(name);
        if old == Some(sha) {
            continue;
        }
        if first_poll {
            hub.store.advance_ref(repo.id.clone(), name.clone(), sha.clone()).await?;
            continue; // adopt silently on the first enabled poll
        }
        let (event, branch) = if let Some(b) = name.strip_prefix("refs/heads/") {
            if !repo.branches.is_empty() && !repo.branches.iter().any(|p| pipeline::glob_match(p, b)) {
                hub.store.advance_ref(repo.id.clone(), name.clone(), sha.clone()).await?;
                continue;
            }
            ("push", b.to_owned())
        } else if let Some(t) = name.strip_prefix("refs/tags/") {
            ("tag", t.to_owned())
        } else if let Some(pr) = name.strip_prefix("refs/pull/").and_then(|r| r.strip_suffix("/head")) {
            if !hub.cfg.gitops.pull_requests {
                hub.store.advance_ref(repo.id.clone(), name.clone(), sha.clone()).await?;
                continue;
            }
            ("pull_request", format!("pr/{pr}"))
        } else {
            continue;
        };
        let t = Trigger { event, branch, sha: sha.clone(), before: old.cloned(), pipeline: None, payload: None };
        let source = crate::ci::events::ref_ingress(hub, repo, &t).await?;
        if hub.store.advance_ref_ingress(repo.id.clone(), name.clone(), sha.clone(), source, None).await? {
            crate::ci::schedule::outbox::kick(hub).await;
            let u = &hub.cfg.updates;
            if event == "push" && repo.name == u.release_repo && t.branch == u.release_branch { hub.updates.wake_in_step(); }
            triggered += 1;
        }
    }
    Ok(triggered)
}

pub async fn manual(hub: &Hub, repo: &Repo, branch: Option<String>, pipeline: Option<String>) -> anyhow::Result<Vec<Run>> {
    let remote = ls_remote(hub, &repo.clone_url).await?;
    let branch = match branch {
        Some(b) => b,
        None => remote
            .get("HEAD->")
            .and_then(|h| h.strip_prefix("refs/heads/"))
            .unwrap_or("main")
            .to_owned(),
    };
    let sha = remote
        .get(&format!("refs/heads/{branch}"))
        .or_else(|| remote.get(&format!("refs/tags/{branch}")))
        .cloned()
        .or_else(|| (branch.len() == 40 && branch.chars().all(|c| c.is_ascii_hexdigit())).then(|| branch.clone()))
        .ok_or_else(|| anyhow::anyhow!("unknown branch, tag or sha `{branch}`"))?;
    trigger(hub, repo, Trigger { event: "manual", branch, sha, before: None, pipeline, payload: None }).await
}

/// Register repositories from config and org discovery.
async fn discover(hub: &Hub, http: &crate::http::client::Client) -> anyhow::Result<()> {
    let poll = hub.cfg.gitops.poll_seconds;
    for r in &hub.cfg.repos {
        let native = r.mode.as_deref() == Some("native");
        let clone_url = match r.mode.as_deref() {
            Some("native") => match crate::ci::hosting::create(hub, &r.name, r.default_branch.as_deref().unwrap_or("main"), r.import.as_deref()).await {
                Ok(path) => crate::ci::hosting::clone_url(&path),
                Err(e) => {
                    tracing::warn!(repo = %r.name, error = %format!("{e:#}"), "the native repository couldn't be made");
                    continue;
                }
            },
            Some("upstream") | None => r.clone_url.clone().unwrap_or_else(|| default_url(&r.name)),
            Some(m) => {
                tracing::warn!(repo = %r.name, mode = %m, "a repository's mode is `native` or `upstream`; left out");
                continue;
            }
        };
        hub.store
            .upsert_repo(Repo {
                id: crate::util::new_id(),
                name: r.name.clone(),
                clone_url,
                branches: r.branches.clone(),
                poll_seconds: r.poll_seconds.unwrap_or(poll),
                enabled: true,
                created_at: crate::util::now_unix(),
                last_polled_at: None,
                last_error: None,
                mode: if native { "native" } else { "upstream" }.into(),
                default_branch: r.default_branch.clone().filter(|_| native),
                mirror: r.mirror.clone().filter(|_| native),
            })
            .await?;
    }
    let existing: Vec<String> = hub.store.repos().await?.into_iter().map(|r| r.name).collect();
    for org in &hub.cfg.gitops.orgs {
        let mut page = 1;
        loop {
            let url = format!("https://api.github.com/orgs/{org}/repos?per_page=100&page={page}");
            let mut req = http.get(&url).header("Accept", "application/vnd.github+json");
            if let Some(t) = crate::git::github_token(hub) {
                req = req.bearer(&t);
            }
            let resp = req.send().await.context("github org listing")?;
            if let Some(rem) = resp.header("x-ratelimit-remaining").and_then(|v| v.parse().ok()) {
                crate::metrics::global().gitops_rate_remaining.set(rem);
            }
            if let Some(lim) = resp.header("x-ratelimit-limit").and_then(|v| v.parse().ok()) {
                crate::metrics::global().gitops_rate_limit.set(lim);
            }
            if !resp.is_success() {
                anyhow::bail!("github org {org}: HTTP {}", resp.status());
            }
            let repos: Vec<serde_json::Value> = resp.json(32 << 20).await?;
            let n = repos.len();
            for r in repos {
                let name = r.get("full_name").unwrap_or(&serde_json::Value::Null).as_str().unwrap_or_default().to_owned();
                if name.is_empty() || existing.contains(&name) {
                    continue;
                }
                if r.get("archived").unwrap_or(&serde_json::Value::Null).as_bool() == Some(true) || (hub.cfg.gitops.skip_forks && r.get("fork").unwrap_or(&serde_json::Value::Null).as_bool() == Some(true)) {
                    continue;
                }
                let url = r.get("clone_url").unwrap_or(&serde_json::Value::Null).as_str().map(str::to_owned).unwrap_or_else(|| default_url(&name));
                // GitHub fills in `size` lazily: a repository created and pushed minutes ago can
                // still say 0. Git knows (and costs no API quota): no branch or tag means empty.
                if r.get("size").unwrap_or(&serde_json::Value::Null).as_u64() == Some(0)
                    && !ls_remote(hub, &url).await.is_ok_and(|refs| has_commits(&refs))
                {
                    continue; // empty repository: nothing to build yet; re-checked next discovery
                }
                tracing::info!(repo = %name, "discovered repository");
                hub.store
                    .upsert_repo(Repo {
                        id: crate::util::new_id(),
                        name,
                        clone_url: url,
                        branches: vec![],
                        poll_seconds: poll,
                        enabled: true,
                        created_at: crate::util::now_unix(),
                        last_polled_at: None,
                        last_error: None,
                        mode: "upstream".into(),
                        default_branch: None,
                        mirror: None,
                    })
                    .await?;
            }
            if n < 100 {
                break;
            }
            page += 1;
        }
    }
    Ok(())
}

pub async fn discover_loop(hub: Arc<Hub>) -> anyhow::Result<()> {
    let http = crate::http::client::github()?;
    loop {
        if let Err(e) = discover(&hub, http).await {
            tracing::warn!(error = %format!("{e:#}"), "repository discovery failed");
        }
        wake(&hub);
        if !hub.shutdown.sleep(Duration::from_secs(u64::from(hub.cfg.gitops.discover_seconds.max(60)))).await {
            return Ok(());
        }
    }
}

/// A failed poll's backoff: from 10 s, doubling with each failure in a row up to 5 minutes, with
/// jitter. A repository is polled again only once it's over (and its interval, unless asked).
const BACKOFF_FIRST: Duration = Duration::from_secs(10);
const BACKOFF_MAX: Duration = Duration::from_secs(300);

/// The backoff after `failures` failed polls in a row.
fn backoff(failures: u32) -> Duration {
    let d = BACKOFF_FIRST.saturating_mul(2u32.saturating_pow(failures.saturating_sub(1).min(16)));
    crate::sup::jitter(d.min(BACKOFF_MAX))
}

/// Asks the poller to poll repository `id` (by its id) now: a push, a webhook, a new repository.
/// One backing off after a failed poll is polled once its backoff is over.
pub fn want(hub: &Hub, id: &str) {
    hub.gitops_wanted.lock().insert(id.to_owned());
    hub.gitops_kick.notify();
}

/// [`want`] for the repository a webhook names (`owner/name`, as its host spells it), if kiln has
/// it; otherwise the poller is only woken.
pub async fn want_named(hub: &Hub, name: &str) {
    match hub.store.repos().await {
        Ok(repos) => match repos.into_iter().find(|r| r.name.eq_ignore_ascii_case(name)) {
            Some(r) => want(hub, &r.id),
            None => wake(hub),
        },
        Err(e) => {
            tracing::warn!(repo = %name, error = %format!("{e:#}"), "a webhook's repository couldn't be looked up; the poller is only woken");
            wake(hub);
        }
    }
}

/// Wakes the poller to poll what's due (a repository discovered or added is: it was never
/// polled). It asks for nothing else.
pub fn wake(hub: &Hub) {
    hub.gitops_kick.notify();
}

/// Polls each repository when its interval says, when it's asked for ([`want`]), and every one
/// once as the server starts. A failed poll backs off ([`backoff`]) whatever asks, so polls that
/// fail at once (a DNS outage) never spin, and a kick polls only what it names.
pub async fn poll_loop(hub: Arc<Hub>) -> anyhow::Result<()> {
    run_polls(hub, |hub: Arc<Hub>, repo: Repo| async move { poll_repo(&hub, &repo).await }).await
}

/// [`poll_loop`], with the poll it runs.
async fn run_polls<P, F>(hub: Arc<Hub>, poll: P) -> anyhow::Result<()>
where
    P: Fn(Arc<Hub>, Repo) -> F + Clone + Send + Sync + 'static,
    F: std::future::Future<Output = anyhow::Result<usize>> + Send + 'static,
{
    let sem = Arc::new(async_lock::Semaphore::new(hub.cfg.gitops.git_concurrency.max(1)));
    let inflight = Arc::new(parking_lot::Mutex::new(std::collections::HashSet::<String>::new()));
    // Asked for while their poll ran (a push meanwhile): asked for again as that poll ends.
    let again = Arc::new(parking_lot::Mutex::new(std::collections::HashSet::<String>::new()));
    // Each failing repository's failures in a row, and when it may be polled again.
    let failing = Arc::new(parking_lot::Mutex::new(HashMap::<String, (u32, Instant)>::new()));
    let mut start = true;
    loop {
        let now = crate::util::now_unix();
        let asked = std::mem::take(&mut *hub.gitops_wanted.lock());
        for repo in hub.store.repos().await? {
            if !repo.enabled {
                continue;
            }
            let wanted = asked.contains(&repo.id);
            if inflight.lock().contains(&repo.id) {
                if wanted {
                    again.lock().insert(repo.id.clone());
                }
                continue;
            }
            if failing.lock().get(&repo.id).is_some_and(|(_, until)| Instant::now() < *until) {
                // Still asked for once its backoff is over.
                if wanted {
                    hub.gitops_wanted.lock().insert(repo.id.clone());
                }
                continue;
            }
            let due = start || wanted || repo.last_polled_at.is_none_or(|t| now - t >= i64::from(repo.poll_seconds.max(10)));
            if !due {
                continue;
            }
            inflight.lock().insert(repo.id.clone());
            let (hub, sem, inflight, again, failing, poll) = (hub.clone(), sem.clone(), inflight.clone(), again.clone(), failing.clone(), poll.clone());
            crate::sup::spawn_guarded("poll-repo", async move {
                let _permit = sem.acquire().await;
                let r = poll(Arc::clone(&hub), repo.clone()).await;
                let err = r.as_ref().err().map(|e| format!("{e:#}"));
                let result = match &r {
                    Ok(0) => "unchanged",
                    Ok(_) => "triggered",
                    Err(_) => "error",
                };
                crate::metrics::global().gitops_polls.inc(&[result]);
                match &err {
                    Some(e) => {
                        let mut f = failing.lock();
                        let (n, until) = f.entry(repo.id.clone()).or_insert((0, Instant::now()));
                        *n = n.saturating_add(1);
                        let wait = backoff(*n);
                        *until = Instant::now() + wait;
                        tracing::warn!(repo = %repo.name, error = %e, failures = *n, retry_in_s = wait.as_secs(), "poll failed");
                    }
                    None => {
                        failing.lock().remove(&repo.id);
                    }
                }
                hub.store.repo_polled(repo.id.clone(), err).await?;
                inflight.lock().remove(&repo.id);
                if again.lock().remove(&repo.id) {
                    want(&hub, &repo.id);
                }
                Ok(())
            });
        }
        start = false;
        futures::select_biased! {
            () = hub.shutdown.cancelled().fuse() => return Ok(()),
            () = hub.gitops_kick.notified().fuse() => {}
            () = crate::rt::sleep(Duration::from_secs(5)).fuse() => {}
        }
    }
}

/// Fire workflow schedules through the shared cron engine and durable fire records (U04,
/// docs/ci.md W03-R10). Both dialects use the same path.
pub async fn schedule_loop(hub: Arc<Hub>) -> anyhow::Result<()> {
    let mut last_minute = -1i64;
    let mut cache = crate::ci::schedule::Cache::default();
    loop {
        if !hub.shutdown.sleep(Duration::from_secs(10)).await {
            return Ok(());
        }
        let minute = crate::util::now_unix() / 60;
        if minute != last_minute {
            last_minute = minute;
            crate::ci::schedule::tick(&hub, minute * 60, &mut cache).await;
        }
    }
}

/// Compatibility entry point for UTC cron consumers (U04): parsing and matching are shared with
/// workflow schedules and attachment validation, including named fields and POSIX day semantics.
pub fn cron_matches(expr: &str, t: time::OffsetDateTime) -> bool {
    use kiln_ci::trigger::cron::{Cron, TimeZone};
    Cron::parse(expr).is_ok_and(|c| c.fires_at(t.unix_timestamp(), &TimeZone::UTC))
}

/// Post a commit status to GitHub when a token is configured. Best effort, never fails a run.
pub async fn report_status(hub: &Hub, run: &Run, status: Status) {
    let Some(token) = crate::git::github_token(hub) else { return };
    let state = match status {
        Status::Succeeded | Status::Skipped => "success",
        Status::Failed | Status::TimedOut => "failure",
        Status::Cancelled => "error",
        _ => "pending",
    };
    if run.event == "manual" || run.event == "schedule" {
        return;
    }
    let body = serde_json::json!({
        "state": state,
        "context": format!("kiln/{}", run.pipeline),
        "description": format!("run #{} {}", run.number, status.as_str()),
        "target_url": format!("{}/runs/{}", hub.cfg.server.public_url.trim_end_matches('/'), run.id),
    });
    let url = format!("https://api.github.com/repos/{}/statuses/{}", run.repo_name, run.sha);
    let r = match crate::http::client::github() {
        Ok(client) => client
            .post(&url)
            .bearer(&token)
            .header("Accept", "application/vnd.github+json")
            .json(&body)
            .timeout(Duration::from_secs(15))
            .send()
            .await
            .map(|resp| resp.status()),
        Err(e) => Err(e),
    };
    match r {
        Ok(code) if (200..300).contains(&code) => {}
        Ok(code) => tracing::debug!(code, "commit status not accepted"),
        Err(e) => tracing::debug!(error = %e, "commit status not posted"),
    }
}

/// `git archive` of a commit, for pods that build without git. Returns a tar stream.
pub async fn archive(hub: &Hub, repo: &Repo, sha: &str) -> anyhow::Result<async_channel::Receiver<bytes::Bytes>> {
    let path = ensure_commit(hub, repo, sha).await?;
    let mut cmd = git(hub);
    cmd.arg("-C").arg(path).args(["archive", "--format=tar", sha]);
    crate::proc::stdout_stream(cmd)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    /// Repositories kiln polls, made in `hub`'s store: `n` of them, never polled, polled every
    /// `every` seconds.
    async fn repos(hub: &Hub, n: usize, every: u32) -> anyhow::Result<Vec<String>> {
        let mut ids = Vec::new();
        for i in 0..n {
            let r = hub
                .store
                .upsert_repo(Repo {
                    id: crate::util::new_id(),
                    name: format!("acme/r{i}"),
                    clone_url: format!("https://git.example.invalid/acme/r{i}.git"),
                    branches: Vec::new(),
                    poll_seconds: every,
                    enabled: true,
                    created_at: crate::util::now_unix(),
                    last_polled_at: None,
                    last_error: None,
                    mode: "upstream".into(),
                    default_branch: None,
                    mirror: None,
                })
                .await?;
            ids.push(r.id);
        }
        Ok(ids)
    }

    #[test]
    fn u01_disabled_poll_preserves_refs_without_remote_access() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub_with(|cfg| cfg.ci.enabled = false).await?;
            let repo = hub.store.upsert_repo(Repo { id: "paused".into(), name: "paused".into(), clone_url: String::new(), branches: Vec::new(), poll_seconds: 60, enabled: true, created_at: 0, last_polled_at: None, last_error: None, mode: "upstream".into(), default_branch: None, mirror: None }).await?;
            hub.store.advance_ref(repo.id.clone(), "refs/heads/main".into(), "before".into()).await?;
            hub.store.advance_ref(repo.id.clone(), "refs/tags/v1".into(), "tag-before".into()).await?;
            let before = hub.store.refs(repo.id.clone()).await?;
            assert_eq!(poll_repo(&hub, &repo).await?, 0, "disabled ingress does not require or contact a remote");
            assert_eq!(hub.store.refs(repo.id.clone()).await?, before, "re-enabling retains the same accepted baseline");
            Ok(())
        })
    }

    /// Polls that fail at once don't spin (production, 2026-10-24: in a DNS outage each failed poll
    /// kicked a round over every repository, some 880 `git ls-remote` a second). Every poll fails
    /// in 5 ms while a kick comes every 10 ms, a plain one and one naming each repository: each is
    /// polled once as the poller starts, then backs off.
    #[test]
    fn polls_that_fail_fast_stay_bounded_while_kicks_keep_coming() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let ids = repos(&hub, 6, 10).await?;
            let polls = Arc::new(AtomicUsize::new(0));
            let counted = Arc::clone(&polls);
            let poller = crate::rt::spawn(run_polls(Arc::clone(&hub), move |_hub: Arc<Hub>, _repo: Repo| {
                let counted = Arc::clone(&counted);
                async move {
                    counted.fetch_add(1, Ordering::AcqRel);
                    crate::rt::sleep(Duration::from_millis(5)).await;
                    Err::<usize, _>(anyhow::anyhow!("could not resolve host: git.example.invalid"))
                }
            }));
            let until = Instant::now() + Duration::from_secs(2);
            while Instant::now() < until {
                wake(&hub);
                for id in &ids {
                    want(&hub, id);
                }
                crate::rt::sleep(Duration::from_millis(10)).await;
            }
            hub.shutdown.cancel();
            poller.await??;
            let n = polls.load(Ordering::Acquire);
            anyhow::ensure!(n <= 2 * ids.len(), "{n} polls of {} repositories in 2 s", ids.len());
            for r in hub.store.repos().await? {
                anyhow::ensure!(r.last_error.is_some(), "{} wasn't polled", r.name);
            }
            Ok(())
        })
    }

    /// A kick polls what it names, and only that: a plain wake polls nothing that isn't due; a
    /// repository asked for while its poll runs is polled once more as it ends.
    #[test]
    fn a_kick_polls_only_what_it_names() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let ids = repos(&hub, 3, 3600).await?;
            let [a, b, c] = <[String; 3]>::try_from(ids.clone()).map_err(|_| anyhow::anyhow!("three repositories"))?;
            let polls = Arc::new(parking_lot::Mutex::new(HashMap::<String, usize>::new()));
            let (counted, slow) = (Arc::clone(&polls), a.clone());
            let poller = crate::rt::spawn(run_polls(Arc::clone(&hub), move |_hub: Arc<Hub>, repo: Repo| {
                let (counted, slow) = (Arc::clone(&counted), slow.clone());
                async move {
                    *counted.lock().entry(repo.id.clone()).or_default() += 1;
                    crate::rt::sleep(Duration::from_millis(if repo.id == slow { 300 } else { 1 })).await;
                    Ok::<usize, anyhow::Error>(0)
                }
            }));
            let count = |id: &str| polls.lock().get(id).copied().unwrap_or(0);
            let settle = |want: Vec<(String, usize)>| {
                let polls = Arc::clone(&polls);
                async move {
                    let until = Instant::now() + Duration::from_secs(3);
                    while Instant::now() < until {
                        if want.iter().all(|(id, n)| polls.lock().get(id).copied().unwrap_or(0) == *n) {
                            return true;
                        }
                        crate::rt::sleep(Duration::from_millis(10)).await;
                    }
                    false
                }
            };
            // As it starts, each once.
            anyhow::ensure!(settle(ids.iter().map(|i| (i.clone(), 1)).collect()).await, "the first round: {:?}", polls.lock());
            crate::rt::sleep(Duration::from_millis(400)).await;
            // A plain wake: nothing is due.
            wake(&hub);
            crate::rt::sleep(Duration::from_millis(200)).await;
            anyhow::ensure!(ids.iter().all(|i| count(i) == 1), "a wake polled what wasn't due: {:?}", polls.lock());
            // One named: that one.
            want(&hub, &b);
            anyhow::ensure!(settle(vec![(b.clone(), 2)]).await, "the one asked for wasn't polled: {:?}", polls.lock());
            anyhow::ensure!(count(&a) == 1 && count(&c) == 1, "others were polled: {:?}", polls.lock());
            // Asked for while its poll runs: once more as it ends, and no more.
            want(&hub, &a);
            anyhow::ensure!(settle(vec![(a.clone(), 2)]).await, "a wasn't polled: {:?}", polls.lock());
            want(&hub, &a);
            anyhow::ensure!(settle(vec![(a.clone(), 3)]).await, "a wasn't polled again after its poll: {:?}", polls.lock());
            crate::rt::sleep(Duration::from_millis(800)).await;
            anyhow::ensure!(count(&a) == 3 && count(&b) == 2 && count(&c) == 1, "polled more than asked: {:?}", polls.lock());
            hub.shutdown.cancel();
            poller.await??;
            Ok(())
        })
    }

    /// git in `at`, as a test's committer: what it printed.
    #[cfg(unix)]
    async fn git_at(hub: &Hub, at: &std::path::Path, args: &[&str]) -> anyhow::Result<Vec<u8>> {
        let at = at.to_string_lossy();
        let mut all = vec!["-C", &*at, "-c", "user.name=T", "-c", "user.email=t@x", "-c", "commit.gpgsign=false"];
        all.extend_from_slice(args);
        run_git(hub, &all, LS_REMOTE_TIMEOUT).await
    }

    /// Lets a held fetch go on, however the test ends.
    #[cfg(unix)]
    struct Go(std::path::PathBuf);

    #[cfg(unix)]
    impl Drop for Go {
        fn drop(&mut self) {
            let _ = std::fs::write(&self.0, b"");
        }
    }

    /// A commit a fetch is still writing into the mirror is read whole: its archive waits for the
    /// fetch (W03-R08: a change request's checks read its workflows while the poller fetched them,
    /// and git couldn't read `.github`, 2026-10-28). The fetch is caught part-way: the upload-pack
    /// it talks to passes on the first 70,000 bytes of its answer and holds the rest until `go` is
    /// there, and the mirror unpacks what it fetches, an object at a time, as git does a small
    /// fetch.
    #[cfg(unix)]
    #[test]
    fn w03_r08_a_commit_a_fetch_is_still_writing_is_read_whole() -> anyhow::Result<()> {
        use std::os::unix::fs::PermissionsExt as _;
        crate::rt::block_on(async {
            let hub = crate::hub::tests::scratch_hub().await?;
            let dir = tempfile::tempdir()?;
            let (origin, work, go) = (dir.path().join("origin.git"), dir.path().join("work"), dir.path().join("go"));
            let _go = Go(go.clone());
            let o = origin.to_string_lossy().into_owned();
            run_git(&hub, &["init", "-q", "--bare", "-b", "main", &o], LS_REMOTE_TIMEOUT).await?;
            run_git(&hub, &["init", "-q", "-b", "main", &work.to_string_lossy()], LS_REMOTE_TIMEOUT).await?;
            std::fs::write(work.join("README.md"), "app\n")?;
            git_at(&hub, &work, &["add", "."]).await?;
            git_at(&hub, &work, &["commit", "-qm", "first"]).await?;
            git_at(&hub, &work, &["push", "-q", &o, "main"]).await?;
            let repo = Repo {
                id: crate::util::new_id(),
                name: "acme/app".into(),
                clone_url: format!("file://{o}"),
                branches: Vec::new(),
                poll_seconds: 60,
                enabled: true,
                created_at: crate::util::now_unix(),
                last_polled_at: None,
                last_error: None,
                mode: "upstream".into(),
                default_branch: None,
                mirror: None,
            };
            let mirror = sync_mirror(&hub, &repo).await?;
            // From now on the mirror's fetches are held part-way until `go` (a minute at most),
            // and unpacked. `dd` passes on each byte as it comes: git's answers are a conversation.
            let held = dir.path().join("held-upload-pack");
            let script = format!(
                "#!/bin/sh\ngit upload-pack \"$@\" | {{\n  dd bs=1 count=70000 2>/dev/null\n  i=0\n  while [ ! -e '{go}' ] && [ $i -lt 1200 ]; do sleep 0.05; i=$((i + 1)); done\n  exec cat\n}}\n",
                go = go.display()
            );
            std::fs::write(&held, script)?;
            std::fs::set_permissions(&held, std::fs::Permissions::from_mode(0o755))?;
            git_at(&hub, &mirror, &["config", &format!("remote.{}.uploadpack", repo.clone_url), &held.to_string_lossy()]).await?;
            // As git writes a small fetch by default, whatever this machine's git says.
            for (k, v) in [("fetch.unpackLimit", "100000"), ("fetch.fsckObjects", "false"), ("core.fsyncMethod", "fsync")] {
                git_at(&hub, &mirror, &["config", k, v]).await?;
            }
            // A commit with a workflow, and 128 KiB besides that doesn't compress: more than the
            // first 70,000 bytes.
            std::fs::create_dir_all(work.join(".github/workflows"))?;
            std::fs::write(work.join(".github/workflows/ci.yml"), "on: push\njobs:\n  t:\n    runs-on: local\n    steps:\n      - run: echo hi\n")?;
            let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
            for i in 0..64 {
                let mut noise = Vec::with_capacity(2048);
                while noise.len() < 2048 {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    noise.extend_from_slice(&x.to_le_bytes());
                }
                std::fs::write(work.join(format!("data{i:02}")), noise)?;
            }
            git_at(&hub, &work, &["add", "."]).await?;
            git_at(&hub, &work, &["commit", "-qm", "a workflow"]).await?;
            git_at(&hub, &work, &["push", "-q", &o, "main"]).await?;
            let sha = String::from_utf8_lossy(&git_at(&hub, &work, &["rev-parse", "HEAD"]).await?).trim().to_owned();
            let last = String::from_utf8_lossy(&git_at(&hub, &work, &["rev-parse", "HEAD:data63"]).await?).trim().to_owned();
            let whole = git_at(&hub, &origin, &["archive", "--format=tar", &sha]).await?;
            // The poller's fetch...
            let fetching = crate::rt::spawn({
                let (hub, repo) = (Arc::clone(&hub), repo.clone());
                async move { sync_mirror(&hub, &repo).await }
            });
            // ...part-way: the commit is in the mirror, and not all it refers to.
            let m = mirror.to_string_lossy().into_owned();
            let until = Instant::now() + Duration::from_secs(30);
            while run_git(&hub, &["-C", &m, "cat-file", "-e", &format!("{sha}^{{commit}}")], LS_REMOTE_TIMEOUT).await.is_err() {
                anyhow::ensure!(Instant::now() < until, "the fetch never wrote {sha}");
                crate::rt::sleep(Duration::from_millis(5)).await;
            }
            anyhow::ensure!(run_git(&hub, &["-C", &m, "cat-file", "-e", &last], LS_REMOTE_TIMEOUT).await.is_err(), "the fetch wrote its objects at once: nothing was caught part-way");
            // The commit's archive, asked for now, isn't read from what the fetch has written so
            // far: it comes once the fetch is done.
            let mut reader = crate::rt::spawn({
                let (hub, repo, sha) = (Arc::clone(&hub), repo.clone(), sha.clone());
                async move {
                    let rx = archive(&hub, &repo, &sha).await?;
                    let mut out = Vec::new();
                    while let Ok(b) = rx.recv().await {
                        out.extend_from_slice(&b);
                    }
                    Ok::<Vec<u8>, anyhow::Error>(out)
                }
            });
            let early = crate::rt::timeout(Duration::from_secs(2), &mut reader).await.ok();
            std::fs::write(&go, b"")?;
            let got = match early {
                Some(r) => r,
                None => reader.await,
            }??;
            fetching.await??;
            anyhow::ensure!(got == whole, "{sha}'s archive, asked for while a fetch wrote it: {} bytes, not {}", got.len(), whole.len());
            Ok(())
        })
    }

    #[test]
    fn empty_repository_has_no_commits() {
        let mut refs = BTreeMap::new();
        assert!(!has_commits(&refs));
        refs.insert("HEAD->".to_owned(), "refs/heads/main".to_owned());
        assert!(!has_commits(&refs));
        refs.insert("refs/heads/main".to_owned(), "c5781cd98aa37175970723a4de31f3af5077311f".to_owned());
        assert!(has_commits(&refs));
    }

    #[test]
    fn cron() {
        let t = time::macros::datetime!(2026-10-13 03:30 UTC); // a Tuesday
        assert!(cron_matches("30 3 * * *", t));
        assert!(cron_matches("*/15 * * * 2", t));
        assert!(cron_matches("0-45/10 3 13 10 1-5", t));
        assert!(!cron_matches("0 3 * * *", t));
        assert!(!cron_matches("30 3 * * 0,6", t));
        assert!(!cron_matches("bad", t));
    }
}
