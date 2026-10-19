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
use std::time::Duration;

use anyhow::Context as _;
use std::process::Command;

use futures::FutureExt as _;

use crate::hub::Hub;
use crate::pipeline::{self, Context, Pipeline};
use crate::store::{Repo, Run, Status};

const GIT_TIMEOUT: Duration = Duration::from_secs(600);
pub(crate) const LS_REMOTE_TIMEOUT: Duration = Duration::from_secs(60);

fn github_token(hub: &Hub) -> Option<String> {
    let p = hub.cfg.gitops.github_token_file.as_ref()?;
    std::fs::read_to_string(p).ok().map(|s| s.trim().to_owned()).filter(|s| !s.is_empty())
}

pub(crate) fn git(hub: &Hub) -> Command {
    let mut c = Command::new("git");
    crate::os::git_no_prompts(&mut c)
        .env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes -o StrictHostKeyChecking=accept-new")
        .stdin(std::process::Stdio::null());
    if let Some(t) = github_token(hub) {
        use base64::Engine as _;
        let basic = base64::engine::general_purpose::STANDARD.encode(format!("x-access-token:{t}"));
        c.arg("-c").arg(format!("http.https://github.com/.extraheader=Authorization: Basic {basic}"));
    }
    // GitLab's and Bitbucket's credentials, for what they cover (docs/ci.md W23-R04, R05).
    for (url, header) in crate::githost::git_headers(hub) {
        c.arg("-c").arg(format!("http.{url}.extraheader={header}"));
    }
    c
}

pub(crate) async fn run_git(hub: &Hub, args: &[&str], timeout: Duration) -> anyhow::Result<Vec<u8>> {
    let mut cmd = git(hub);
    cmd.args(args);
    let out = crate::proc::output(cmd, timeout)
        .await
        .with_context(|| format!("git {}", args.first().unwrap_or(&"")))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("git {}: {}", args.first().unwrap_or(&""), crate::util::truncate(err.trim(), 600));
    }
    Ok(out.stdout)
}

pub fn mirror_path(hub: &Hub, repo: &str) -> PathBuf {
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

/// Create or refresh the bare mirror. Concurrent callers for one repo are serialised.
pub async fn sync_mirror(hub: &Hub, repo: &Repo) -> anyhow::Result<PathBuf> {
    static LOCKS: std::sync::OnceLock<parking_lot::Mutex<HashMap<String, Arc<async_lock::Mutex<()>>>>> =
        std::sync::OnceLock::new();
    let lock = LOCKS
        .get_or_init(Default::default)
        .lock()
        .entry(repo.name.clone())
        .or_default()
        .clone();
    let _g = lock.lock().await;
    let path = mirror_path(hub, &repo.name);
    let p = path.to_string_lossy().to_string();
    if path.join("HEAD").exists() {
        // The repository may have moved to another remote since the mirror was cloned.
        run_git(hub, &["-C", &p, "remote", "set-url", "origin", &repo.clone_url], LS_REMOTE_TIMEOUT).await?;
        run_git(hub, &["-C", &p, "fetch", "--prune", "--quiet", "origin",
            "+refs/heads/*:refs/heads/*", "+refs/tags/*:refs/tags/*", "+refs/pull/*/head:refs/pull/*/head"], GIT_TIMEOUT)
            .await?;
    } else {
        if let Some(parent) = path.parent() {
            let parent = parent.to_path_buf();
            crate::rt::blocking(move || std::fs::create_dir_all(parent)).await??;
        }
        let tmp = format!("{p}.tmp-{}", std::process::id());
        let t = tmp.clone();
        let _ = crate::rt::blocking(move || std::fs::remove_dir_all(t)).await;
        run_git(hub, &["clone", "--bare", "--quiet", &repo.clone_url, &tmp], GIT_TIMEOUT).await?;
        run_git(hub, &["-C", &tmp, "config", "remote.origin.fetch", "+refs/heads/*:refs/heads/*"], GIT_TIMEOUT).await?;
        run_git(hub, &["-C", &tmp, "fetch", "--quiet", "origin", "+refs/pull/*/head:refs/pull/*/head"], GIT_TIMEOUT)
            .await
            .ok();
        let (from, to) = (tmp.clone(), path.clone());
        crate::rt::blocking(move || std::fs::rename(from, to)).await??;
    }
    Ok(path)
}

pub(crate) async fn ensure_commit(hub: &Hub, repo: &Repo, sha: &str) -> anyhow::Result<PathBuf> {
    let path = mirror_path(hub, &repo.name);
    let p = path.to_string_lossy().to_string();
    let have = path.join("HEAD").exists()
        && run_git(hub, &["-C", &p, "cat-file", "-e", &format!("{sha}^{{commit}}")], LS_REMOTE_TIMEOUT).await.is_ok();
    if have {
        return Ok(path);
    }
    let path = sync_mirror(hub, repo).await?;
    if run_git(hub, &["-C", &p, "cat-file", "-e", &format!("{sha}^{{commit}}")], LS_REMOTE_TIMEOUT).await.is_err() {
        run_git(hub, &["-C", &p, "fetch", "--quiet", "origin", sha], GIT_TIMEOUT).await?;
    }
    Ok(path)
}

/// kiln's configuration files under `.kiln/`, which aren't pipelines: deployments
/// (docs/cd.md, docs/cd-targets.md), environments (docs/ci.md W15) and review (W14).
fn is_config(file: &str) -> bool {
    let stem = file.trim_start_matches(".kiln/").trim_end_matches(".yaml").trim_end_matches(".yml");
    matches!(stem, "deploy" | "environments" | "review")
}

/// Every `.kiln/*.yml|yaml` at `sha` that is a pipeline, parsed. Parse failures are returned per
/// file.
pub async fn load_pipelines(
    hub: &Hub,
    repo: &Repo,
    sha: &str,
) -> anyhow::Result<Vec<(String, Result<Pipeline, String>)>> {
    load_pipelines_opt(hub, repo, sha, true).await
}

/// [`load_pipelines`], with or without the auto-detected pipeline when there is none (the CI
/// engine's GitHub workflows take its place, docs/ci.md W02-R02).
async fn load_pipelines_opt(
    hub: &Hub,
    repo: &Repo,
    sha: &str,
    auto: bool,
) -> anyhow::Result<Vec<(String, Result<Pipeline, String>)>> {
    let path = ensure_commit(hub, repo, sha).await?;
    let p = path.to_string_lossy().to_string();
    let listing = run_git(hub, &["-C", &p, "ls-tree", "--name-only", sha, ".kiln/"], LS_REMOTE_TIMEOUT)
        .await
        .unwrap_or_default();
    let mut out = Vec::new();
    for file in String::from_utf8_lossy(&listing).lines() {
        if !(file.ends_with(".yml") || file.ends_with(".yaml")) || is_config(file) {
            continue;
        }
        let src = run_git(hub, &["-C", &p, "show", &format!("{sha}:{file}")], LS_REMOTE_TIMEOUT).await?;
        let parsed = pipeline::parse(&String::from_utf8_lossy(&src)).map_err(|e| e.to_string());
        out.push((file.to_owned(), parsed));
    }
    if out.is_empty() && auto && hub.cfg.gitops.auto_pipelines {
        let root = run_git(hub, &["-C", &p, "ls-tree", "--name-only", sha], LS_REMOTE_TIMEOUT).await?;
        let files: Vec<String> = String::from_utf8_lossy(&root).lines().map(str::to_owned).collect();
        if let Some(yaml) = crate::autodetect::pipeline_for(&files) {
            out.push(("auto".to_owned(), pipeline::parse(&yaml).map_err(|e| e.to_string())));
        }
    }
    Ok(out)
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

pub(crate) async fn changed_files(hub: &Hub, repo: &Repo, old: &str, new: &str) -> Option<Vec<String>> {
    let p = mirror_path(hub, &repo.name).to_string_lossy().to_string();
    let o = run_git(hub, &["-C", &p, "diff", "--name-only", old, new], LS_REMOTE_TIMEOUT).await.ok()?;
    Some(String::from_utf8_lossy(&o).lines().map(str::to_owned).collect())
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

/// Turn a trigger into runs: one per matching pipeline file.
pub async fn trigger(hub: &Hub, repo: &Repo, t: Trigger) -> anyhow::Result<Vec<Run>> {
    // A push to the branch kiln builds itself from: the server's own commit may have just arrived.
    let u = &hub.cfg.updates;
    if t.event == "push" && repo.name == u.release_repo && t.branch == u.release_branch {
        hub.updates.wake_in_step();
    }
    // The CI engine's GitHub workflows (docs/ci.md W03), when it is on; v1 keeps `.kiln/`.
    let github = match crate::ci::events::on_trigger(hub, repo, &t).await {
        Ok(ran) => ran,
        Err(e) => {
            tracing::warn!(repo = %repo.name, error = %format!("{e:#}"), "the CI engine could not read the commit's workflows");
            false
        }
    };
    // With `[ci] kiln_workflows` the engine ran the commit's `.kiln/` workflows too: v1 stands aside.
    if github && hub.cfg.ci.kiln_workflows {
        return Ok(vec![]);
    }
    let pipelines = load_pipelines_opt(hub, repo, &t.sha, !github).await?;
    if pipelines.is_empty() {
        return Ok(vec![]);
    }
    let changed = match &t.before {
        Some(b) if t.event == "push" => changed_files(hub, repo, b, &t.sha).await,
        _ => None,
    };
    let (message, author) = commit_info(hub, repo, &t.sha).await;
    let mut runs = Vec::new();
    for (file, parsed) in pipelines {
        let name_hint = file.trim_start_matches(".kiln/").trim_end_matches(".yaml").trim_end_matches(".yml").to_owned();
        let base = Run {
            id: crate::util::new_id(),
            repo_id: repo.id.clone(),
            repo_name: repo.name.clone(),
            number: 0,
            pipeline: name_hint.clone(),
            event: t.event.to_owned(),
            branch: t.branch.clone(),
            sha: t.sha.clone(),
            message: message.clone(),
            author: author.clone(),
            status: Status::Queued,
            created_at: 0,
            started_at: None,
            finished_at: None,
            error: None,
            concurrency_group: None,
        };
        let p = match parsed {
            Ok(p) => p,
            Err(e) => {
                // An invalid pipeline is a visible failed run, not a silent no-op.
                let run = hub.store.create_run(Run { error: Some(format!("{file}: {e}")), ..base }, vec![]).await?;
                hub.store.finish_job_less_run(run.id.clone(), Status::Failed).await?;
                hub.emit_run(&run.id).await;
                runs.push(run);
                continue;
            }
        };
        let name = if p.name.is_empty() { name_hint } else { p.name.clone() };
        if let Some(only) = &t.pipeline
            && only != &name && only != &file {
                continue;
            }
        let wanted = match t.event {
            "push" => p.on.push.as_ref().is_some_and(|f| {
                f.matches_branch(&t.branch)
                    && changed.as_ref().is_none_or(|c| f.matches_paths(c.iter().map(String::as_str)))
            }),
            "pull_request" => p.on.pull_request.is_some(),
            "tag" => p.on.tag.as_ref().is_some_and(|f| f.matches_branch(&t.branch)),
            "manual" => p.on.manual,
            "schedule" => !p.on.schedule.is_empty(),
            _ => false,
        };
        if !wanted {
            continue;
        }
        let mut ctx = Context::default();
        ctx.set("git.sha", t.sha.clone());
        ctx.set("git.short_sha", t.sha.get(..8).unwrap_or(&t.sha).to_owned());
        ctx.set("git.branch", t.branch.clone());
        ctx.set("branch", t.branch.clone());
        ctx.set("event", t.event);
        ctx.set("repo", repo.name.clone());
        // Groups are scoped to the repository (as on GitHub): `ci-main` in two repos must not
        // cancel each other.
        let group = p.concurrency.as_ref().map(|c| format!("{}:{}", repo.name, ctx.interpolate(&c.group)));
        let planned = match pipeline::plan(&p, &ctx) {
            Ok(j) => j,
            Err(e) => {
                let run = hub.store.create_run(Run { pipeline: name, error: Some(e.to_string()), ..base }, vec![]).await?;
                hub.store.finish_job_less_run(run.id.clone(), Status::Failed).await?;
                hub.emit_run(&run.id).await;
                runs.push(run);
                continue;
            }
        };
        let run = hub
            .store
            .create_run(Run { pipeline: name, concurrency_group: group.clone(), ..base }, planned)
            .await?;
        crate::metrics::global().runs_started.inc();
        tracing::info!(repo = %repo.name, run = run.number, event = t.event, branch = %t.branch, sha = %t.sha, "run created");
        if let (Some(g), Some(c)) = (group, &p.concurrency)
            && c.cancel_in_progress {
                for old in hub.store.active_runs_in_group(g, run.id.clone()).await? {
                    hub.cancel_run(&old, "superseded by a newer run").await?;
                }
            }
        report_status(hub, &run, Status::Pending).await;
        hub.emit_run(&run.id).await;
        runs.push(run);
    }
    hub.wake.notify();
    Ok(runs)
}

/// Poll one repository: diff refs against what we last saw and trigger runs for movement.
pub async fn poll_repo(hub: &Hub, repo: &Repo) -> anyhow::Result<usize> {
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
        // Whoever records a ref's move first triggers it; a webhook may have (docs/ci.md W03-R14).
        if !hub.store.advance_ref(repo.id.clone(), name.clone(), sha.clone()).await? {
            continue;
        }
        if first_poll {
            continue; // adopt the current state silently; build from the next change on
        }
        let (event, branch) = if let Some(b) = name.strip_prefix("refs/heads/") {
            if !repo.branches.is_empty() && !repo.branches.iter().any(|p| pipeline::glob_match(p, b)) {
                continue;
            }
            ("push", b.to_owned())
        } else if let Some(t) = name.strip_prefix("refs/tags/") {
            ("tag", t.to_owned())
        } else if let Some(pr) = name.strip_prefix("refs/pull/").and_then(|r| r.strip_suffix("/head")) {
            if !hub.cfg.gitops.pull_requests {
                continue;
            }
            ("pull_request", format!("pr/{pr}"))
        } else {
            continue;
        };
        let runs = trigger(
            hub,
            repo,
            Trigger { event, branch, sha: sha.clone(), before: old.cloned(), pipeline: None, payload: None },
        )
        .await?;
        triggered += runs.len();
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
            if let Some(t) = github_token(hub) {
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
        hub.gitops_kick.notify();
        if !hub.shutdown.sleep(Duration::from_secs(u64::from(hub.cfg.gitops.discover_seconds.max(60)))).await {
            return Ok(());
        }
    }
}

/// Poll every repository when it is due (or immediately when kicked by a webhook / the UI).
pub async fn poll_loop(hub: Arc<Hub>) -> anyhow::Result<()> {
    let sem = Arc::new(async_lock::Semaphore::new(hub.cfg.gitops.git_concurrency.max(1)));
    let inflight = Arc::new(parking_lot::Mutex::new(std::collections::HashSet::<String>::new()));
    let mut force = true;
    loop {
        let now = crate::util::now_unix();
        for repo in hub.store.repos().await? {
            if !repo.enabled {
                continue;
            }
            let due = force || repo.last_polled_at.is_none_or(|t| now - t >= i64::from(repo.poll_seconds.max(10)));
            if !due || inflight.lock().contains(&repo.id) {
                continue;
            }
            inflight.lock().insert(repo.id.clone());
            let (hub, sem, inflight) = (hub.clone(), sem.clone(), inflight.clone());
            crate::sup::spawn_guarded("poll-repo", async move {
                let _permit = sem.acquire().await;
                let r = poll_repo(&hub, &repo).await;
                let err = r.as_ref().err().map(|e| format!("{e:#}"));
                let result = match &r {
                    Ok(0) => "unchanged",
                    Ok(_) => "triggered",
                    Err(_) => "error",
                };
                crate::metrics::global().gitops_polls.inc(&[result]);
                if let Some(e) = &err {
                    tracing::warn!(repo = %repo.name, error = %e, "poll failed");
                }
                hub.store.repo_polled(repo.id.clone(), err).await?;
                inflight.lock().remove(&repo.id);
                Ok(())
            });
        }
        force = false;
        futures::select_biased! {
            () = hub.shutdown.cancelled().fuse() => return Ok(()),
            () = hub.gitops_kick.notified().fuse() => force = true,
            () = crate::rt::sleep(Duration::from_secs(5)).fuse() => {}
        }
    }
}

/// Fire `on.schedule` cron entries on each repository's default branch.
pub async fn schedule_loop(hub: Arc<Hub>) -> anyhow::Result<()> {
    let mut last_minute = -1i64;
    let mut cache: HashMap<(String, String), Vec<(String, Result<Pipeline, String>)>> = HashMap::new();
    let mut ci_cache = crate::ci::schedule::Cache::default();
    loop {
        if !hub.shutdown.sleep(Duration::from_secs(10)).await {
            return Ok(());
        }
        let now = time::OffsetDateTime::now_utc();
        let minute = now.unix_timestamp() / 60;
        if minute == last_minute {
            continue;
        }
        last_minute = minute;
        // The CI engine's workflows (docs/ci.md W03-R10), when it is on.
        crate::ci::schedule::tick(&hub, minute * 60, &mut ci_cache).await;
        for repo in hub.store.repos().await? {
            let refs: HashMap<String, String> = hub.store.refs(repo.id.clone()).await?.into_iter().collect();
            let Some(head) = refs.get("HEAD->") else { continue };
            let Some(sha) = refs.get(head) else { continue };
            let key = (repo.id.clone(), sha.clone());
            if !cache.contains_key(&key) {
                cache.retain(|(r, _), _| r != &repo.id);
                let p = load_pipelines(&hub, &repo, sha).await.unwrap_or_default();
                cache.insert(key.clone(), p);
            }
            let Some(pipes) = cache.get(&key) else { continue };
            for (file, p) in pipes {
                let Ok(p) = p else { continue };
                if p.on.schedule.iter().any(|c| cron_matches(c, now)) {
                    let branch = head.trim_start_matches("refs/heads/").to_owned();
                    let t = Trigger { event: "schedule", branch, sha: sha.clone(), before: None, pipeline: Some(file.clone()), payload: None };
                    if let Err(e) = trigger(&hub, &repo, t).await {
                        tracing::warn!(repo = %repo.name, error = %format!("{e:#}"), "scheduled run failed");
                    }
                }
            }
        }
    }
}

fn cron_field(spec: &str, value: u32, min: u32, max: u32) -> bool {
    spec.split(',').any(|part| {
        let (range, step) = match part.split_once('/') {
            Some((r, s)) => (r, s.parse::<u32>().unwrap_or(1).max(1)),
            None => (part, 1),
        };
        let (lo, hi) = if range == "*" {
            (min, max)
        } else if let Some((a, b)) = range.split_once('-') {
            (a.parse().unwrap_or(min), b.parse().unwrap_or(max))
        } else {
            let v = range.parse().unwrap_or(u32::MAX);
            (v, if step > 1 { max } else { v })
        };
        value >= lo && value <= hi && (value - lo).is_multiple_of(step)
    })
}

/// Five-field cron (`min hour dom mon dow`), UTC. Sunday is 0 (or 7).
pub fn cron_matches(expr: &str, t: time::OffsetDateTime) -> bool {
    let f: Vec<&str> = expr.split_whitespace().collect();
    let [m, h, dom, mon, dow] = f.as_slice() else { return false };
    let wd = u32::from(t.weekday().number_days_from_sunday());
    cron_field(m, u32::from(t.minute()), 0, 59)
        && cron_field(h, u32::from(t.hour()), 0, 23)
        && cron_field(dom, u32::from(t.day()), 1, 31)
        && cron_field(mon, u32::from(u8::from(t.month())), 1, 12)
        && (cron_field(dow, wd, 0, 7) || (wd == 0 && cron_field(dow, 7, 0, 7)))
}

/// Post a commit status to GitHub when a token is configured. Best effort, never fails a run.
pub async fn report_status(hub: &Hub, run: &Run, status: Status) {
    let Some(token) = github_token(hub) else { return };
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
    use super::*;

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
