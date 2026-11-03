//! Ship's Pipelines (docs/ci.md W29-R01 to R03): every workflow, reusable workflow and action at
//! each repository's default-branch head, as Ship's rows by repository, what needs someone first.
//! Where each row stands is `kiln_ci::pipelines`'s to say: this gathers what it decides from (the
//! head's files, their runs on that branch, the runners, the tags), and `kiln_ci::pipelines`
//! shapes the JSON too.
//!
//! The rows are held in memory, as Ship's commits are. A repository's are gathered again once a run
//! or a job of it is heard on the events socket, or a push to it ([`heard`]), and at most
//! [`KEEP_FOR`] after they were. When a repository's rows change, a `pipelines` event names it, so
//! pages ask again.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use http::StatusCode;
use kiln_ci::pipelines::{self as core, Facts, Kind, Outcome, Row};
use kiln_ci::wf;
use parking_lot::Mutex;
use serde_json::Value;

use super::Fail;
use crate::http::server::{self as srv, Response};
use crate::hub::Hub;
use crate::store::Repo;

/// The longest a repository's rows are kept with nothing of it heard.
const KEEP_FOR: Duration = Duration::from_secs(30);
/// How many of a branch's newest runs are read to find each workflow's newest.
const RUNS_LOOK: i64 = 500;
/// How many of the head's newest commits are read to say who changed each file last.
const LOG_LOOK: &str = "400";
/// How long after something is heard its repository is gathered again, so a burst gathers once.
const SETTLE: Duration = Duration::from_millis(500);

/// A repository's rows as last gathered, at its default branch's head.
#[derive(Debug, Clone)]
struct Held {
    view: core::Repo,
    at: Instant,
    /// What its rows show, hashed: a gather that changes it sends `pipelines`.
    digest: u64,
}

/// What every repository's rows are gathered against: the server's routing rules, runners and
/// pools (for `KCI6007`), and whether a connected runner has each microVM executor.
struct Env {
    rules: Vec<kiln_ci::sched::routing::Rule>,
    runners: Vec<crate::store::AgentRow>,
    pools: Vec<kiln_ci::sched::pools::PoolSpec>,
    firecracker: bool,
    shards: bool,
}

impl Env {
    async fn read(hub: &Hub) -> Env {
        let has = |e: &str| hub.agents.lock().values().any(|a| a.connected && a.facts.as_deref().is_some_and(|f| kiln_ci::sched::routing::has_executor(f, e)));
        let (firecracker, shards) = (has("firecracker"), has("shards"));
        Env {
            rules: crate::ci::routing::sources(hub).await.effective(),
            runners: hub.store.agents().await.unwrap_or_default(),
            pools: crate::ci::pools::specs(hub).await,
            firecracker,
            shards,
        }
    }
}

#[derive(Default)]
struct Model {
    held: Mutex<BTreeMap<String, Arc<Held>>>,
    /// Repositories something was heard of since they were gathered.
    dirty: Mutex<HashSet<String>>,
    /// Every repository's rows are to be gathered again (frames went unheard).
    all: AtomicBool,
    listening: AtomicBool,
    settling: AtomicBool,
}

static MODEL: OnceLock<Arc<Model>> = OnceLock::new();

fn model() -> Arc<Model> {
    Arc::clone(MODEL.get_or_init(|| Arc::new(Model::default())))
}

/// `repo`'s refs moved (a push to a repository kiln hosts): its rows are gathered again.
pub(crate) fn heard(repo: &str) {
    if let Some(m) = MODEL.get() {
        m.dirty.lock().insert(repo.to_owned());
    }
}

impl Model {
    /// Hears `hub`'s events for as long as it runs: each run's and job's marks its repository's
    /// rows to be gathered again, soon. Once.
    fn listen(self: &Arc<Self>, hub: &Arc<Hub>) {
        if self.listening.swap(true, Ordering::AcqRel) {
            return;
        }
        let (me, hub) = (Arc::clone(self), Arc::clone(hub));
        let mut rx = hub.events.subscribe();
        crate::rt::spawn(async move {
            use futures::FutureExt as _;
            // Which repository each run heard of is: a job's frame names only its run.
            let mut runs: HashMap<String, String> = HashMap::new();
            loop {
                futures::select_biased! {
                    () = hub.shutdown.cancelled().fuse() => return,
                    m = rx.recv().fuse() => match m {
                        Ok(text) => {
                            if me.hear(&text, &mut runs) {
                                me.soon(&hub);
                            }
                        }
                        // Some went unheard: any of them may have been a run's.
                        Err(async_broadcast::RecvError::Overflowed(_)) => me.all.store(true, Ordering::Release),
                        Err(async_broadcast::RecvError::Closed) => return,
                    },
                }
            }
        });
    }

    /// Whether a frame of the events socket marked a repository's rows to be gathered again.
    fn hear(&self, text: &str, runs: &mut HashMap<String, String>) -> bool {
        // The type comes first: other frames are passed over on their first bytes.
        if !(text.starts_with(r#"{"type":"run""#) || text.starts_with(r#"{"type":"job""#)) {
            return false;
        }
        let Ok(v) = serde_json::from_str::<Value>(text) else { return false };
        let repo = match v.get("type").and_then(Value::as_str) {
            Some("run") => {
                let (Some(id), Some(repo)) = (v.pointer("/run/id").and_then(Value::as_str), v.pointer("/run/repo_name").and_then(Value::as_str)) else { return false };
                if runs.len() > 4096 {
                    runs.clear();
                }
                runs.insert(id.to_owned(), repo.to_owned());
                repo.to_owned()
            }
            Some("job") => match v.pointer("/job/run_id").and_then(Value::as_str).and_then(|id| runs.get(id)) {
                Some(repo) => repo.clone(),
                None => return false,
            },
            _ => return false,
        };
        self.dirty.lock().insert(repo);
        true
    }

    /// Gathers what was heard of after [`SETTLE`], so a page hears `pipelines` without asking.
    fn soon(self: &Arc<Self>, hub: &Arc<Hub>) {
        if self.settling.swap(true, Ordering::AcqRel) {
            return;
        }
        let (me, hub) = (Arc::clone(self), Arc::clone(hub));
        crate::rt::spawn(async move {
            crate::rt::sleep(SETTLE).await;
            me.settling.store(false, Ordering::Release);
            me.fresh(&hub).await;
        });
    }

    /// Every repository's rows: as held, unless something of it was heard since or they're older
    /// than [`KEEP_FOR`]; else gathered again. A repository whose rows changed gets a `pipelines`
    /// event; one that can't be gathered keeps the rows it had.
    async fn fresh(&self, hub: &Hub) -> Vec<Arc<Held>> {
        let repos = hub.store.repos().await.unwrap_or_default();
        let dirty: HashSet<String> = std::mem::take(&mut *self.dirty.lock());
        let all = self.all.swap(false, Ordering::AcqRel);
        let had: BTreeMap<String, Arc<Held>> = self.held.lock().clone();
        let stale = |name: &str| all || dirty.contains(name) || had.get(name).is_none_or(|h| h.at.elapsed() >= KEEP_FOR);
        let mut env: Option<Env> = None;
        let mut out: BTreeMap<String, Arc<Held>> = BTreeMap::new();
        for repo in repos.iter().filter(|r| r.enabled) {
            let old = had.get(&repo.name).cloned();
            if !stale(&repo.name)
                && let Some(h) = old.clone()
            {
                out.insert(repo.name.clone(), h);
                continue;
            }
            if env.is_none() {
                env = Some(Env::read(hub).await);
            }
            let Some(e) = env.as_ref() else { continue };
            match gather(hub, repo, e).await {
                Ok(Some(h)) => {
                    if old.as_ref().is_some_and(|o| o.digest != h.digest) {
                        hub.emit(&crate::wire::events::ServerEvent::Pipelines { repo: repo.name.clone() });
                    }
                    out.insert(repo.name.clone(), Arc::new(h));
                }
                Ok(None) => {}
                Err(err) => {
                    tracing::warn!("ship pipelines: {}'s rows kept as they were: {err:#}", repo.name);
                    if let Some(h) = old {
                        out.insert(repo.name.clone(), h);
                    }
                }
            }
        }
        *self.held.lock() = out.clone();
        out.into_values().collect()
    }
}

/// A repository's default branch: its own, else the first it follows, else `main`.
fn default_branch(repo: &Repo) -> String {
    repo.default_branch.clone().or_else(|| repo.branches.first().cloned()).unwrap_or_else(|| "main".to_owned())
}

/// What a commit's rows are read against besides its files: its repository's tags, who changed
/// each file last, and each workflow's newest runs on the default branch.
struct Ctx {
    /// The newest full version.
    released: Option<String>,
    /// Every full version with its commit, newest first (the pane's Versions).
    versions: Vec<(String, String)>,
    changes: Changes,
    by_path: HashMap<String, (Option<Outcome>, bool)>,
    released_outcome: (Option<Outcome>, bool),
}

/// The commit's files, and what its rows are read against.
async fn context(hub: &Hub, repo: &Repo, branch: &str, sha: &str) -> anyhow::Result<(crate::ci::load::Commit, Ctx)> {
    let commit = crate::ci::load::load(hub, repo, sha).await?;
    let dir = crate::gitops::ensure_commit(hub, repo, sha).await?;
    let d = dir.to_string_lossy().into_owned();
    // The tags, each with its commit (an annotated tag's peeled).
    let tags_text = git(hub, &d, &["for-each-ref", "--format=%(refname:strip=2) %(objectname) %(*objectname)", "refs/tags"]).await;
    let tags: Vec<(&str, &str)> = tags_text
        .lines()
        .filter_map(|l| {
            let mut w = l.split_whitespace();
            let name = w.next()?;
            let obj = w.next()?;
            Some((name, w.next().unwrap_or(obj)))
        })
        .collect();
    let versions: Vec<(String, String)> = core::releases(tags.iter().map(|(t, _)| *t)).into_iter().filter_map(|r| tags.iter().find(|(t, _)| *t == r).map(|(t, s)| ((*t).to_owned(), (*s).to_owned()))).collect();
    let released = versions.first().map(|(t, _)| t.clone());
    let released_sha = versions.first().map(|(_, s)| s.clone());
    // Who changed each file last: the commit's newest ancestors, newest first.
    let log = git(hub, &d, &["log", "-n", LOG_LOOK, "--format=%x00%H%x00%an%x00%at", "--name-only", sha, "--", ".github", ".kiln", "action.yml", "action.yaml"]).await;
    let changes = changes_of(&log);
    let (by_path, released_outcome) = runs_of(hub, repo, branch, released_sha.as_deref()).await?;
    Ok((commit, Ctx { released, versions, changes, by_path, released_outcome }))
}

/// A workflow's row (W29-R02).
fn workflow_row(repo: &Repo, l: &crate::ci::load::Loaded, ctx: &Ctx, env: &Env, github_note: &str) -> Row {
    let stem = stem_of(&l.path);
    let kind = l.workflow.as_ref().map_or(Kind::Workflow, Kind::of_workflow);
    let errors = l.diagnostics.iter().filter(|x| x.severity == wf::Severity::Error).count();
    let routable = l.workflow.as_ref().filter(|_| l.runs || kind == Kind::Reusable).map(|w| crate::ci::workflows::unknown_in(w, &repo.name, &env.rules, &env.runners, &env.pools).is_empty());
    let seen = ctx.by_path.get(&l.path).or_else(|| ctx.by_path.get(&format!("v1:{stem}"))).copied().unwrap_or_default();
    let facts = Facts {
        kind,
        name: l.workflow.as_ref().and_then(|w| w.name.clone()).unwrap_or_else(|| stem.clone()),
        path: l.path.clone(),
        runs: l.runs,
        errors: u32::try_from(errors).unwrap_or(u32::MAX),
        routable,
        latest: if kind == Kind::Reusable { ctx.released_outcome.0 } else { seen.0 },
        running: if kind == Kind::Reusable { ctx.released_outcome.1 } else { seen.1 },
        released: (kind == Kind::Reusable || l.workflow.as_ref().is_some_and(|w| release_trigger(w, ctx.released.as_deref()))).then(|| ctx.released.clone()).flatten(),
        ..Facts::default()
    };
    let line = workflow_line(l, kind, github_note);
    let (who, when, commit) = ctx.changes.get(&l.path).cloned().map_or((None, None, None), |(w, t, sha)| (Some(w), Some(t), Some(sha)));
    let mut row = Row::new(facts, Some(if l.dialect == kiln_ci::expr::Dialect::Kiln { "kiln" } else { "gha" }), line, who, when);
    row.commit = commit;
    row
}

/// W29-R02, pl-1: a workflow that runs on the newest release's tag shows that tag in its chip.
fn release_trigger(w: &wf::Workflow, tag: Option<&str>) -> bool {
    let Some(tag) = tag else { return false };
    w.trigger("push").and_then(|t| t.tags.as_ref()).and_then(|tags| kiln_ci::trigger::pattern::Patterns::new(tags).ok()).is_some_and(|tags| tags.matches(tag))
}

/// An action's row (W29-R02), from its `action.yml` as parsed.
fn action_row(path: &str, action: Option<&wf::Action>, diags: &[wf::Diag], ctx: &Ctx, env: &Env) -> Row {
    let kind = action.map_or(Kind::Unknown, Kind::of_action);
    let folder = path.rsplit_once('/').map_or("", |(d, _)| d);
    let errors = diags.iter().filter(|x| x.severity == wf::Severity::Error).count();
    let routable = match kind {
        Kind::Firecracker => Some(env.firecracker),
        Kind::Shards => Some(env.shards),
        _ => None,
    };
    let facts = Facts {
        kind,
        name: action.and_then(|a| a.name.clone()).unwrap_or_else(|| folder.rsplit('/').next().unwrap_or(folder).to_owned()),
        path: path.to_owned(),
        runs: true,
        errors: u32::try_from(errors).unwrap_or(u32::MAX),
        routable,
        latest: ctx.released_outcome.0,
        running: ctx.released_outcome.1,
        released: ctx.released.clone(),
        ..Facts::default()
    };
    let line = action_line(kind, action, errors, routable, ctx.released.as_deref());
    // An action is its folder: a change to any of its files is a change to it.
    let (who, when, commit) = ctx.changes.get_folder(folder).map_or((None, None, None), |(w, t, sha)| (Some(w), Some(t), Some(sha)));
    let mut row = Row::new(facts, None, line, who, when);
    row.commit = commit;
    row
}

/// A repository's rows at its default branch's head, or `None` when the branch has no commit.
async fn gather(hub: &Hub, repo: &Repo, env: &Env) -> anyhow::Result<Option<Held>> {
    let branch = default_branch(repo);
    let Some(sha) = crate::ci::load::resolve(hub, repo, &branch).await? else { return Ok(None) };
    let (commit, ctx) = context(hub, repo, &branch, &sha).await?;
    let mut rows: Vec<Row> = commit.workflows.iter().map(|l| workflow_row(repo, l, &ctx, env, &commit.github_note)).collect();
    for path in &commit.actions {
        let text = crate::ci::load::blob(hub, repo, &sha, path).await.unwrap_or_default();
        let (action, diags) = wf::parse_action(&text);
        rows.push(action_row(path, action.as_ref(), &diags, &ctx, env));
    }
    // Each open draft, a row of its own beside what's published (W29-R10).
    for d in crate::ci::pipeline_drafts::open_in(hub, &repo.name).await.unwrap_or_default() {
        rows.push(draft_row(hub, repo, &d, &ctx, env).await.0);
    }
    let access = crate::ci::sharing::access_of(hub, repo).await;
    let view = core::Repo::new(repo.name.clone(), branch, sha, rows, access);
    let digest = view.digest;
    Ok(Some(Held { view, at: Instant::now(), digest }))
}

/// A draft's row (W29-R10), as its file is at the draft's head, marked with the draft's state,
/// author and change; its Latest is its author's runs of it (W29-R14). With it, the file's text
/// and the head, where kiln can read them: a draft it can't reach still has its row.
async fn draft_row(hub: &Hub, repo: &Repo, d: &crate::ci::pipeline_drafts::Draft, ctx: &Ctx, env: &Env) -> (Row, Option<(String, String, Option<crate::ci::load::Loaded>)>) {
    let mark = core::DraftOf {
        id: d.id.clone(),
        state: if d.state == "review" {
            core::Draft::Review
        } else if d.author.starts_with(kiln_ci::updates::AUTHOR) {
            core::Draft::Update
        } else {
            core::Draft::Unpublished
        },
        author: d.author.clone(),
        change: d.name.clone().map(|name| core::DraftChange { kind: if d.kind == "diff" { "diff" } else { "change" }, name, url: d.url.clone() }),
    };
    let read = async {
        let head = crate::ci::pipeline_drafts::head(hub, repo, d).await?;
        let text = crate::ci::load::blob(hub, repo, &head, &d.path).await?;
        let commit = crate::ci::load::load(hub, repo, &head).await?;
        anyhow::Ok((head, text, commit.workflows.into_iter().find(|l| l.path == d.path)))
    };
    let read = read.await.ok();
    let mut row = match &read {
        Some((_, _, Some(l))) => workflow_row(repo, l, ctx, env, ""),
        Some((_, text, None)) => {
            let (action, diags) = wf::parse_action(text);
            action_row(&d.path, action.as_ref(), &diags, ctx, env)
        }
        None => {
            let kind = if matches!(wf::file_kind(&d.path), Some(wf::FileKind::Action)) { Kind::Unknown } else { Kind::Workflow };
            Row::new(Facts { kind, name: stem_of(&d.path), path: d.path.clone(), runs: true, ..Facts::default() }, None, String::new(), None, None)
        }
    };
    let (latest, running) = draft_runs(hub, &d.id).await.unwrap_or_default();
    row.facts.latest = latest;
    row.facts.running = running;
    row.who = Some(d.author.strip_prefix("user:").unwrap_or(&d.author).to_owned());
    row.when = Some(d.updated_at);
    row.commit = read.as_ref().map(|(head, ..)| head.clone());
    (row.drafted(mark), read)
}

/// The newest outcome of a draft's runs by its author's hand, and whether one is under way.
async fn draft_runs(hub: &Hub, draft: &str) -> anyhow::Result<(Option<Outcome>, bool)> {
    let draft = draft.to_owned();
    let statuses: Vec<String> = hub
        .store
        .call(move |c| {
            let mut st = c.prepare("SELECT status FROM runs WHERE draft = ?1 ORDER BY created_at DESC LIMIT 20")?;
            let out = st.query_map([&draft], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(out)
        })
        .await?;
    let mut seen = (None, false);
    for s in &statuses {
        fold(&mut seen, s);
    }
    Ok(seen)
}

/// A workflow's newest runs on `branch` (both engines'), newest first, for the pane's Runs.
async fn runs_list(hub: &Hub, repo: &Repo, branch: &str, path: &str) -> anyhow::Result<Vec<core::RunSeen>> {
    let (id, branch, path, stem) = (repo.id.clone(), branch.to_owned(), path.to_owned(), stem_of(path));
    hub.store
        .call(move |c| {
            // Each with its commit, when it was made, and the jobs that failed in it (pl-2's `9e1b2c0 · rust failed`).
            let mut st = c.prepare(
                "SELECT r.number, r.event, r.status, r.sha, r.created_at,
                        (SELECT group_concat(j.key, ', ') FROM jobs j WHERE j.run_id = r.id AND j.status = 'failed'),
                        (SELECT e.payload FROM ci_events e WHERE e.run_id = r.id), r.error
                 FROM runs r LEFT JOIN ci_workflows w ON w.id = r.workflow_id
                 WHERE r.repo_id = ?1 AND r.branch = ?2 AND (w.path = ?3 OR (r.workflow_id IS NULL AND r.pipeline = ?4))
                 ORDER BY r.created_at DESC LIMIT 10",
            )?;
            let mut out = Vec::new();
            let rows = st.query_map(rusqlite::params![id, branch, path, stem], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?, r.get::<_, i64>(4)?, r.get::<_, Option<String>>(5)?, r.get::<_, Option<String>>(6)?, r.get::<_, Option<String>>(7)?))
            })?;
            for row in rows {
                let (number, event, status, sha, at, failed, payload, error) = row?;
                let mut seen = (None, false);
                fold(&mut seen, &status);
                let failed = failed.map(|f| f.split(", ").map(str::to_owned).collect()).unwrap_or_default();
                let payload = payload.and_then(|p| serde_json::from_str(&p).ok()).unwrap_or_default();
                let subject = core::run_subject(&event, &payload);
                // W29-R23–R26: refused before any job, by the code its startup error opens with.
                let refused = error.as_deref().and_then(core::refusal_code);
                out.push(core::RunSeen { number, event, outcome: seen.0, running: seen.1, sha, at, failed, subject, refused });
            }
            Ok(out)
        })
        .await
}

/// What kiln has seen of workflow `path` beyond its text, for its lines' words as the approved
/// stills have them (W29-R04, pl-2):
/// - each job's time in the workflow's last finished run on `branch`;
/// - each use of a repository kiln hosts against its newest release (`v2.4.0 is out`, `newest`);
/// - where each secret and variable it may read is set;
/// - and now, for the runs' ages.
async fn seen_of(hub: &Hub, repo: &Repo, branch: &str, path: &str, w: &wf::Workflow) -> core::Seen {
    let mut seen = core::Seen { now: crate::util::now_unix(), ..core::Seen::default() };
    let (id, b, p, stem) = (repo.id.clone(), branch.to_owned(), path.to_owned(), stem_of(path));
    // Each job's time and outcome in that run, and its commit (the strip's `its last run passed
    // in 18m 40s on a3f2c1d`).
    let last = hub
        .store
        .call(move |c| {
            let mut st = c.prepare(
                "SELECT j.key, j.started_at, j.finished_at, j.status, r.sha FROM jobs j JOIN runs r ON r.id = j.run_id WHERE j.run_id = (
                   SELECT r.id FROM runs r LEFT JOIN ci_workflows w ON w.id = r.workflow_id
                   WHERE r.repo_id = ?1 AND r.branch = ?2 AND (w.path = ?3 OR (r.workflow_id IS NULL AND r.pipeline = ?4)) AND r.finished_at IS NOT NULL
                   ORDER BY r.created_at DESC LIMIT 1)",
            )?;
            let rows = st.query_map(rusqlite::params![id, b, p, stem], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, Option<i64>>(1)?, r.get::<_, Option<i64>>(2)?, r.get::<_, String>(3)?, r.get::<_, String>(4)?))
            })?;
            let (mut secs, mut outcomes, mut sha) = (HashMap::new(), HashMap::new(), None);
            for row in rows {
                let (k, started, finished, status, at) = row?;
                if let (Some(s), Some(f)) = (started, finished) {
                    secs.insert(k.clone(), f - s);
                }
                let mut o = (None, false);
                fold(&mut o, &status);
                if let Some(o) = o.0 {
                    outcomes.insert(k, o);
                }
                sha = Some(at);
            }
            Ok((secs, outcomes, sha))
        })
        .await;
    (seen.job_secs, seen.job_outcome, seen.last_sha) = last.unwrap_or_default();
    // Each use of a repository kiln hosts, by the full version it names, against that
    // repository's newest release.
    let hosted = hub.store.repos().await.unwrap_or_default();
    let mut uses: Vec<_> = core::direct_uses(w).into_iter().map(|(u, call)| (u, call, 0)).collect();
    let mut listed = HashSet::new();
    let mut cursor = 0;
    while let Some((u, call, depth)) = uses.get(cursor).cloned() {
        cursor += 1;
        if !listed.insert(u.clone()) {
            continue;
        }
        let Some(r) = crate::ci::actions::Remote::parse(&u) else { continue };
        let other = hosted.iter().find(|x| x.name.eq_ignore_ascii_case(&format!("{}/{}", r.owner, r.repo)));
        let dir = if let Some(other) = other {
            seen.hosted.insert(u.clone());
            crate::ci::hosting::native_path(hub, other).unwrap_or_else(|| crate::gitops::mirror_path(hub, &other.name))
        } else {
            r.mirror(hub)
        };
        let dir = dir.to_string_lossy().into_owned();
        // W29-R04, pl-7: describe the exact cached workflow a job calls, including its own
        // dependencies. Never fetch to open a pane, or read a hosted callee the consumer may
        // not use. Invalid, absent and excessively nested callees leave the direct line alone.
        let allowed = if call && let Some(other) = other {
            kiln_ci::sharing::allows(crate::ci::sharing::access_of(hub, other).await, &other.name, &repo.name).is_ok()
        } else {
            true
        };
        if call && depth < 4 && seen.called.len() < 64 && allowed {
            let source = git(hub, &dir, &["show", &format!("{}:{}", r.git_ref, r.path)]).await;
            let dialect = if r.path.starts_with(".kiln/") { kiln_ci::expr::Dialect::Kiln } else { kiln_ci::expr::Dialect::Gha };
            let (callee, diags) = wf::parse_workflow(&r.path, &source, dialect);
            if let Some(callee) = callee.filter(|_| !diags.iter().any(wf::Diag::is_error)) {
                let facts = core::called_seen(&callee);
                uses.extend(facts.uses.iter().map(|(u, call)| (u.clone(), *call, depth + 1)));
                seen.called.insert(u.clone(), facts);
            }
        }
        match kiln_ci::releases::kind_of(&r.git_ref) {
            kiln_ci::releases::TagKind::Floating => {
                // A read of an existing mirror only: opening a pane never fetches an action or
                // changes its tag. Full releases must point at the floating tag's exact commit.
                let sha = git(hub, &dir, &["rev-parse", "--verify", &format!("refs/tags/{}^{{commit}}", r.git_ref)]).await;
                let sha = sha.trim();
                if !sha.is_empty() {
                    let tags = git(hub, &dir, &["for-each-ref", "--points-at", sha, "--format=%(refname:strip=2)", "refs/tags"]).await;
                    if let Some(words) = core::floating_words(&r.git_ref, tags.lines(), sha) {
                        seen.uses.insert(u, (words, false));
                    }
                }
            }
            kiln_ci::releases::TagKind::Full if other.is_some() => {
                let tags = git(hub, &dir, &["for-each-ref", "--format=%(refname:strip=2)", "refs/tags"]).await;
                if let Some(newest) = core::newest_release(tags.lines()) {
                    let standing = if newest == r.git_ref.as_str() { ("newest".to_owned(), false) } else { (format!("{newest} is out"), true) };
                    seen.uses.insert(u, standing);
                }
            }
            _ => {}
        }
    }
    seen.set_in = crate::ci::secrets::set_in(hub, repo, path).await.unwrap_or_default();
    seen
}

/// `GET /api/ship/pipelines/:owner/:name?path=&ref=` (W29-R04): one workflow or action opened at
/// `ref` (its repository's default branch unless given), as `kiln_ci::pipelines::pane_json`
/// shapes it: its head, its strip's verbs, its groups and its file. A repository the caller may
/// not read (`runs.read`) is as missing as one that isn't there.
pub async fn get(hub: &Arc<Hub>, owner: &str, name: &str, q: &HashMap<String, String>, caller: &crate::authz::Caller) -> Result<Response, Fail> {
    let full = format!("{owner}/{name}");
    let missing = || (StatusCode::NOT_FOUND, format!("no repository `{full}` you may read"));
    let internal = |e: anyhow::Error| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"));
    if let Some(names) = crate::authz::gate::readable(hub, caller, "runs.read").await
        && !names.iter().any(|n| n == &full)
    {
        return Err(missing());
    }
    let path = q.get("path").map(String::as_str).filter(|p| !p.is_empty()).ok_or_else(|| (StatusCode::BAD_REQUEST, "`path` names the workflow or action to open".to_owned()))?;
    let repo = hub.store.repos().await.map_err(internal)?.into_iter().find(|r| r.name == full).ok_or_else(missing)?;
    let (me, _) = crate::ci::diffs::author_of(&caller.name, Some(caller.kind));
    let scope = crate::authz::Scope::Repo { repo: repo.name.clone() };
    let viewer = core::Viewer { name: &me, can_submit: caller.can("diffs.submit", &scope), can_land: caller.can("diffs.land", &scope) };
    if let Some(id) = q.get("draft").filter(|d| !d.is_empty()) {
        return draft_pane(hub, &repo, id, path, viewer).await;
    }
    let branch = default_branch(&repo);
    let at = q.get("ref").filter(|r| !r.is_empty()).cloned().unwrap_or_else(|| branch.clone());
    if !crate::ci::load::ref_ok(&at) {
        return Err((StatusCode::BAD_REQUEST, format!("`{at}` isn't a branch, tag or commit")));
    }
    let sha = crate::ci::load::resolve(hub, &repo, &at).await.map_err(internal)?.ok_or_else(|| (StatusCode::NOT_FOUND, format!("`{full}` has no `{at}`")))?;
    let (commit, ctx) = context(hub, &repo, &branch, &sha).await.map_err(internal)?;
    let none_there = || (StatusCode::NOT_FOUND, format!("`{full}` has no workflow or action at `{path}` on `{at}`"));
    let is_workflow = commit.workflows.iter().any(|l| l.path == path);
    if !is_workflow && !commit.actions.iter().any(|a| a == path) {
        return Err(none_there());
    }
    let text = crate::ci::load::blob(hub, &repo, &sha, path).await.map_err(|_| none_there())?;
    let env = Env::read(hub).await;
    // What an action or reusable workflow has come to since its newest release: what releasing
    // it changes, and the commits to its files Release would tag (pl-5).
    let loaded = commit.workflows.iter().find(|l| l.path == path);
    let released = loaded.is_none_or(|l| l.workflow.as_ref().is_some_and(|w| Kind::of_workflow(w) == Kind::Reusable));
    let changes = if released { Some(since_release(hub, &repo, path, &text, &ctx.versions).await) } else { None };
    let newest = kiln_ci::pipelines::newest_release(ctx.versions.iter().map(|(t, _)| t.as_str())).filter(|_| released).map(str::to_owned);
    let commits = match &newest {
        Some(tag) => commits_since(hub, &repo, tag, &sha, path).await,
        None => Vec::new(),
    };
    let (row, groups, narrow) = match loaded {
        Some(l) => {
            let row = workflow_row(&repo, l, &ctx, &env, &commit.github_note);
            let runs = runs_list(hub, &repo, &branch, path).await.map_err(internal)?;
            let (access, used) = used_by(hub, &repo, path, false).await;
            let sharing = core::Sharing { repo: &repo.name, access, used_by: &used };
            let (groups, narrow) = match l.workflow.as_ref() {
                Some(w) => {
                    let mut seen = seen_of(hub, &repo, &branch, path, w).await;
                    seen.newest.clone_from(&newest);
                    seen.lock = Some(crate::ci::locks::pane_facts(hub, &repo, &sha, path, w).await);
                    // R04's pick words: its last run's failed jobs, and what its open drafts change (pl-2).
                    let narrow = core::narrows(w, &runs, &draft_workflows(hub, &repo, path).await);
                    (core::workflow_groups(w, &text, &runs, l.runs, &commit.github_note, Some(&sharing), &seen), narrow)
                }
                None => (Vec::new(), Vec::new()),
            };
            (row, groups, narrow)
        }
        None => {
            let (action, diags) = wf::parse_action(&text);
            let row = action_row(path, action.as_ref(), &diags, &ctx, &env);
            let (access, used) = used_by(hub, &repo, path, true).await;
            let sharing = core::Sharing { repo: &repo.name, access, used_by: &used };
            let seen = core::ActionSeen {
                tag_at: tag_times(hub, &repo, &sha).await,
                since: newest.clone().map(|t| (t, commits.len())),
                added: changes.iter().flatten().filter(|c| c.kind == "input" && c.what == "added").map(|c| c.name.clone()).collect(),
                now: crate::util::now_unix(),
                moved: crate::ci::locks::moved_tags(hub, &repo.name, crate::ci::hosting::native_path(hub, &repo).is_some()).await.map_err(internal)?.into_iter().filter_map(|m| {
                    Some((m.get("ref")?.as_str()?.to_owned(), (m.get("locked")?.as_str()?.to_owned(), m.get("now")?.as_str()?.to_owned())))
                }).collect(),
            };
            let groups = action.as_ref().map(|a| core::action_groups(a, row.facts.kind, &text, &sha, &ctx.versions, &sharing, &seen)).unwrap_or_default();
            (row, groups, Vec::new())
        }
    };
    let by_hand = runs_by_hand(commit.workflows.iter().find(|l| l.path == path));
    let since = newest.as_deref().map(|tag| core::Since { tag, commits: &commits });
    let overlay = overlay_of(hub, &repo, &sha, path).await;
    let overlay = overlay.as_ref().map(|(p, t)| (p.as_str(), t.as_str()));
    // What releasing it changes for those who use it, and the repository's own checks on its
    // head (pl-5's Changes).
    let lines = match (&since, &changes) {
        (Some(_), Some(changes)) => {
            let (outcome, running, jobs) = checks_on(hub, &repo, &sha).await;
            let mut lines = core::change_lines(changes, None);
            lines.push(core::checks_line(&repo.name, &sha, outcome, running, jobs));
            Some(lines)
        }
        _ => None,
    };
    let opened = core::Opened { repo: &full, row: &row, at: &at, sha: &sha, text: &text, viewer, by_hand, changes: changes.as_deref(), since, overlay, said: None, lines, update: None, narrow };
    Ok(srv::json(StatusCode::OK, &core::pane_json(&opened, &groups)))
}

/// The open drafts of `path` in `repo`, each as its head has the workflow (with its overlay, as
/// loading reads a GitHub workflow): what R04's `changed in a draft` sets the published file
/// against. A draft whose head can't be read yet is left out.
async fn draft_workflows(hub: &Hub, repo: &Repo, path: &str) -> Vec<wf::Workflow> {
    let mut out = Vec::new();
    for d in crate::ci::pipeline_drafts::open_in(hub, &repo.name).await.unwrap_or_default().iter().filter(|d| d.path == path) {
        let Ok(head) = crate::ci::pipeline_drafts::head(hub, repo, d).await else { continue };
        let Ok(text) = crate::ci::load::blob(hub, repo, &head, path).await else { continue };
        let Some(mut w) = wf::check_model(path, &text, None).1 else { continue };
        if let Some((_, o)) = overlay_of(hub, repo, &head, path).await.filter(|(_, o)| !o.trim().is_empty())
            && let (Some(ov), _) = wf::parse_overlay(&o)
        {
            wf::apply_overlay(&mut w, &ov);
        }
        out.push(w);
    }
    out
}

/// A repository's own checks on `sha`: its runs' outcome as one, whether one's under way, and how
/// many jobs they have.
async fn checks_on(hub: &Hub, repo: &Repo, sha: &str) -> (Option<Outcome>, bool, usize) {
    let (id, sha) = (repo.id.clone(), sha.to_owned());
    let runs = hub
        .store
        .call(move |c| {
            let mut st = c.prepare("SELECT r.status, (SELECT count(DISTINCT COALESCE(j.job_id, j.key)) FROM jobs j WHERE j.run_id = r.id) FROM runs r WHERE r.repo_id = ?1 AND r.sha = ?2 AND r.draft IS NULL")?;
            let out = st.query_map(rusqlite::params![id, sha], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(out)
        })
        .await
        .unwrap_or_default();
    let statuses: Vec<String> = runs.iter().map(|(s, _)| s.clone()).collect();
    let (outcome, running) = together(&statuses);
    (outcome, running, runs.iter().map(|(_, n)| usize::try_from(*n).unwrap_or(0)).sum())
}

/// When each of `repo`'s tags was made (an annotated tag's own date, else its commit's), by name:
/// pl-5's `3d`.
async fn tag_times(hub: &Hub, repo: &Repo, sha: &str) -> HashMap<String, i64> {
    let Ok(dir) = crate::gitops::ensure_commit(hub, repo, sha).await else { return HashMap::new() };
    let out = git(hub, &dir.to_string_lossy(), &["for-each-ref", "--format=%(refname:strip=2) %(creatordate:unix)", "refs/tags"]).await;
    out.lines().filter_map(|l| l.split_once(' ').and_then(|(t, at)| at.trim().parse().ok().map(|at| (t.to_owned(), at)))).collect()
}

/// What releasing `text` (the file at `path` now) changes for the workflows that use it, since
/// the newest of `versions` (W29-R21, R22): nothing where it was never released.
async fn since_release(hub: &Hub, repo: &Repo, path: &str, text: &str, versions: &[(String, String)]) -> Vec<kiln_ci::wf::diff::Change> {
    let Some(newest) = kiln_ci::pipelines::newest_release(versions.iter().map(|(t, _)| t.as_str())) else { return Vec::new() };
    // By its tag's name: an annotated tag's own object isn't a commit to read a file at.
    let dir = crate::ci::hosting::native_path(hub, repo).unwrap_or_else(|| crate::gitops::mirror_path(hub, &repo.name)).to_string_lossy().into_owned();
    let old = git(hub, &dir, &["show", &format!("refs/tags/{newest}:{path}")]).await;
    if old.is_empty() {
        return Vec::new();
    }
    diff_of(path, &old, text)
}

/// kiln's update draft a verb names by its id (`draft`), of `path` in `repo`: the repository's,
/// not an author's (W29-R22). None when the body names none, or a draft that isn't kiln's update.
async fn update_draft(hub: &Hub, repo: &str, path: &str, body: &Value) -> Result<Option<crate::ci::pipeline_drafts::Draft>, Fail> {
    let Some(id) = body.get("draft").and_then(Value::as_str).filter(|d| !d.is_empty()) else { return Ok(None) };
    let d = crate::ci::pipeline_drafts::get(hub, id).await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    Ok(d.filter(|d| d.repo == repo && d.path == path && d.author.starts_with(kiln_ci::updates::AUTHOR)))
}

/// The commits to an action's folder (or a reusable workflow's file) between release `tag` and
/// `sha`, newest first, as pl-5's Changes view lists them.
async fn commits_since(hub: &Hub, repo: &Repo, tag: &str, sha: &str, path: &str) -> Vec<core::SinceCommit> {
    let dir = crate::ci::hosting::native_path(hub, repo).unwrap_or_else(|| crate::gitops::mirror_path(hub, &repo.name)).to_string_lossy().into_owned();
    let what = if path.ends_with("action.yml") || path.ends_with("action.yaml") { path.rsplit_once('/').map_or(path, |(d, _)| d) } else { path };
    let log = git(hub, &dir, &["log", "--max-count=50", "--format=%h%x00%s%x00%an%x00%at", &format!("refs/tags/{tag}..{sha}"), "--", what]).await;
    log.lines()
        .filter_map(|l| {
            let mut f = l.split('\0');
            let (sha, subject, author, at) = (f.next()?, f.next()?, f.next()?, f.next()?);
            Some(core::SinceCommit { sha: sha.to_owned(), subject: subject.to_owned(), author: author.to_owned(), at: at.parse().unwrap_or_default() })
        })
        .collect()
}

/// [`kiln_ci::wf::action_diff`] of a file's two texts, an action's or a reusable workflow's.
fn diff_of(path: &str, old: &str, new: &str) -> Vec<kiln_ci::wf::diff::Change> {
    use kiln_ci::wf::diff::Version;
    if path.ends_with("action.yml") || path.ends_with("action.yaml") {
        let (a, b) = (wf::parse_action(old).0.unwrap_or_default(), wf::parse_action(new).0.unwrap_or_default());
        return kiln_ci::wf::action_diff(Version::Action(&a, old), Version::Action(&b, new));
    }
    let parse = |t: &str| wf::parse_workflow(path, t, kiln_ci::expr::Dialect::Gha).0.unwrap_or_default();
    let (a, b) = (parse(old), parse(new));
    kiln_ci::wf::action_diff(Version::Workflow(&a), Version::Workflow(&b))
}

/// What an update draft changes for its consumer (W29-R22): for each `uses:` it moves (the
/// consumer's file on its trunk, against the draft's head), what the used action or reusable
/// workflow changes between the two versions, read from kiln's copy of it, and how much of its
/// file changed upstream.
async fn update_changes(hub: &Hub, repo: &Repo, path: &str, trunk: &str, text: &str) -> (Vec<kiln_ci::wf::diff::Change>, Vec<(String, String)>, Vec<core::Moved>) {
    let before = crate::ci::load::blob(hub, repo, trunk, path).await.unwrap_or_default();
    let mut changes = Vec::new();
    let mut upstream = Vec::new();
    let mut moved = Vec::new();
    for (name, old, new) in kiln_ci::updates::moved_between(&before, text) {
        let Some(r) = crate::ci::actions::Remote::parse(&format!("{name}@{new}")) else { continue };
        let full = format!("{}/{}", r.owner, r.repo);
        let dir = crate::ci::hosting::path_of(hub, &full).filter(|d| d.join("HEAD").exists()).unwrap_or_else(|| r.mirror(hub)).to_string_lossy().into_owned();
        let file = if r.path.ends_with(".yml") || r.path.ends_with(".yaml") {
            r.path.clone()
        } else {
            let base = if r.path.is_empty() { String::new() } else { format!("{}/", r.path) };
            let yml = format!("{base}action.yml");
            // `rev-parse` names the file's blob where it's there, and nothing where it isn't.
            if git(hub, &dir, &["rev-parse", "--verify", "--quiet", &format!("{new}:{yml}")]).await.trim().is_empty() { format!("{base}action.yaml") } else { yml }
        };
        let (a, b) = (git(hub, &dir, &["show", &format!("{old}:{file}")]).await, git(hub, &dir, &["show", &format!("{new}:{file}")]).await);
        changes.extend(diff_of(&file, &a, &b));
        let stat = git(hub, &dir, &["diff", "--shortstat", &old, &new, "--", r.path.as_str()]).await;
        upstream.push((format!("{full}/{} {old} → {new}", r.path), stat.trim().to_owned()));
        // By its own name, as the pane's Uses says it (`rust-check`), with the commits to it between its versions.
        let own = r.path.rsplit('/').next().filter(|s| !s.is_empty()).unwrap_or(&r.repo);
        let own = own.strip_suffix(".yml").or_else(|| own.strip_suffix(".yaml")).unwrap_or(own).to_owned();
        let commits = git(hub, &dir, &["rev-list", "--count", &format!("{old}..{new}"), "--", r.path.as_str()]).await.trim().parse().unwrap_or(0);
        moved.push(core::Moved { name: own, uses: name, from: old, to: new, commits });
    }
    (changes, upstream, moved)
}

/// What sharing says of `path` in `repo`'s pane (W29-R17, R20): the repository's Access, and which
/// repositories' runs pinned it (an action by its directory, as `uses:` names it; a workflow by
/// its file).
async fn used_by(hub: &Hub, repo: &Repo, path: &str, action: bool) -> (kiln_ci::sharing::Access, Vec<kiln_ci::catalog::Consumer>) {
    let access = crate::ci::sharing::access_of(hub, repo).await;
    let at = if action { path.rsplit_once('/').map_or("", |(dir, _)| dir) } else { path };
    let used = crate::ci::catalog::consumers(hub, &repo.name, Some(at)).await.unwrap_or_else(|e| {
        tracing::warn!(repo = %repo.name, %path, error = %format!("{e:#}"), "who uses it couldn't be read");
        Vec::new()
    });
    (access, used)
}

/// Whether a workflow runs by hand: it has `workflow_dispatch` (kiln's `manual` reads as it).
fn runs_by_hand(l: Option<&crate::ci::load::Loaded>) -> bool {
    l.and_then(|l| l.workflow.as_ref()).is_some_and(|w| w.on.iter().any(|t| t.event == "workflow_dispatch"))
}

/// A draft's pane (`draft=`, W29-R10): its file at the draft's head, its head's DRAFT mark, and
/// the strip's verbs for `viewer`. A draft that isn't open, or isn't this file's, is missing.
async fn draft_pane(hub: &Hub, repo: &Repo, id: &str, path: &str, viewer: core::Viewer<'_>) -> Result<Response, Fail> {
    let gone = || (StatusCode::NOT_FOUND, format!("no open draft {id} of {path} in {}", repo.name));
    let d = crate::ci::pipeline_drafts::get(hub, id).await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?.filter(|d| d.repo == repo.name && d.path == path && matches!(d.state.as_str(), "draft" | "review")).ok_or_else(gone)?;
    let branch = default_branch(repo);
    let tip = crate::ci::load::resolve(hub, repo, &branch).await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?.unwrap_or_default();
    let (_, ctx) = context(hub, repo, &branch, &tip).await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    let env = Env::read(hub).await;
    let (row, read) = draft_row(hub, repo, &d, &ctx, &env).await;
    let (head, text, loaded) = read.ok_or_else(|| (StatusCode::BAD_GATEWAY, format!("kiln can't read the draft's head on {}'s host yet", repo.name)))?;
    let (access, used) = used_by(hub, repo, path, loaded.is_none()).await;
    let sharing = core::Sharing { repo: &repo.name, access, used_by: &used };
    let mut groups = match &loaded {
        Some(l) => match l.workflow.as_ref() {
            Some(w) => {
                let mut seen = seen_of(hub, repo, &branch, path, w).await;
                seen.lock = Some(crate::ci::locks::pane_facts(hub, repo, &head, path, w).await);
                core::workflow_groups(w, &text, &runs_list(hub, repo, &branch, path).await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?, true, "", Some(&sharing), &seen)
            }
            None => Vec::new(),
        },
        None => {
            let (action, _) = wf::parse_action(&text);
            action.as_ref().map(|a| core::action_groups(a, row.facts.kind, &text, &head, &ctx.versions, &sharing, &core::ActionSeen::default())).unwrap_or_default()
        }
    };
    // kiln's update draft (W29-R22) in its ordinary groups, the use it moves marked in Uses; what
    // that changes for this repository in its strip and its Changes, with the draft's run and the
    // upstream files' diff (pl-7).
    let (mut said, mut lines, mut update) = (None, None, None);
    if d.author.starts_with(kiln_ci::updates::AUTHOR) {
        let (changes, upstream, moved) = update_changes(hub, repo, path, &tip, &text).await;
        let own = repo.name.rsplit('/').next().unwrap_or(&repo.name);
        core::mark_moved(&mut groups, &moved, d.name.as_deref());
        let mut l = core::change_lines(&changes, Some(own));
        l.extend(core::access_line(&changes));
        if let Some((jobs, outcome, running, secs)) = draft_run(hub, &d.id).await {
            l.push(core::draft_run_line(d.name.as_deref(), &jobs, outcome, running, secs));
        }
        l.extend(core::upstream_lines(&upstream));
        lines = Some(l);
        said = Some(core::update_said(&moved, &changes, row.draft.as_ref().and_then(|x| x.change.as_ref())));
        update = moved.into_iter().next().map(|m| (m.name, m.from, m.to));
    }
    // A lock-change draft (W29-R25): each use whose locked commit it changes marked in Uses, its
    // strip saying whose word the new code waits on, its Changes the upstream code.
    let mut review = crate::ci::locks::proposal_for(hub, repo, &d.id, &head).await?;
    if let Some(review) = review.as_mut().and_then(Value::as_object_mut) {
        let differences: BTreeMap<String, kiln_seams::dependencies::Difference> = review.get("differences").cloned().and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default();
        let accepted = crate::ci::locks::accepted_by(hub, repo, &d.id, &head).await;
        if let Some(w) = loaded.as_ref().and_then(|l| l.workflow.as_ref()) && said.is_none() {
            let relocked: Vec<(String, String, usize)> = kiln_ci::lock::editor::written(w)
                .into_iter()
                .filter_map(|(u, _, _)| {
                    let keys = [kiln_ci::lock::kiln::canonical_key(&u), kiln_ci::lock::github::action_pin(&u)];
                    let (_, diff) = differences.iter().find(|(k, _)| keys.iter().flatten().any(|key| key == *k))?;
                    Some((u, (diff.after.commit.clone(), diff.files.len())))
                })
                .collect::<BTreeMap<_, _>>()
                .into_iter()
                .map(|(u, (to, files))| (u, to, files))
                .collect();
            let pairs: Vec<(String, String)> = relocked.iter().map(|(u, to, _)| (u.clone(), to.clone())).collect();
            core::mark_relocked(&mut groups, &pairs, d.name.as_deref());
            said = Some(core::relock_said(&relocked, accepted.as_deref()));
        }
        review.insert("accepted_by".into(), serde_json::json!(accepted));
    }
    let at = d.branch.clone().or_else(|| d.name.clone()).unwrap_or_else(|| head.clone());
    let overlay = overlay_of(hub, repo, &head, path).await;
    let overlay = overlay.as_ref().map(|(p, t)| (p.as_str(), t.as_str()));
    let opened = core::Opened { repo: &repo.name, row: &row, at: &at, sha: &head, text: &text, viewer, by_hand: runs_by_hand(loaded.as_ref()), changes: None, since: None, overlay, said, lines, update, narrow: Vec::new() };
    let mut pane = core::pane_json(&opened, &groups);
    if let Some(review) = review && let Some(pane) = pane.as_object_mut() { pane.insert("lock_review".into(), review); }
    Ok(srv::json(StatusCode::OK, &pane))
}

/// A draft's newest run: each job's key and status (a job's legs as one: failed, else under way,
/// else waiting, else as it ended), its outcome, whether it's under way, and how long it's taken
/// so far. None before it has one.
async fn draft_run(hub: &Hub, draft: &str) -> Option<(Vec<(String, String)>, Option<Outcome>, bool, i64)> {
    let draft = draft.to_owned();
    let run = hub
        .store
        .call(move |c| {
            use rusqlite::OptionalExtension as _;
            let Some((id, status, started, finished)) = c
                .query_row("SELECT id, status, COALESCE(started_at, created_at), finished_at FROM runs WHERE draft = ?1 ORDER BY created_at DESC LIMIT 1", [&draft], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?, r.get::<_, Option<i64>>(3)?))
                })
                .optional()?
            else {
                return Ok(None);
            };
            let mut st = c.prepare("SELECT key, status FROM jobs WHERE run_id = ?1 ORDER BY rowid")?;
            let legs = st.query_map([&id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(Some((status, started, finished, legs)))
        })
        .await
        .ok()??;
    let (status, started, finished, legs) = run;
    let rank = |s: &str| match s {
        "failed" | "timed_out" => 0,
        "running" => 1,
        "queued" | "pending" => 2,
        _ => 3,
    };
    let mut jobs: Vec<(String, String)> = Vec::new();
    for (key, s) in legs {
        match jobs.iter_mut().find(|(k, _)| *k == key) {
            Some(had) if rank(&s) < rank(&had.1) => had.1 = s,
            Some(_) => {}
            None => jobs.push((key, s)),
        }
    }
    let mut seen = (None, false);
    fold(&mut seen, &status);
    Some((jobs, seen.0, seen.1, finished.unwrap_or_else(crate::util::now_unix) - started))
}

/// A GitHub workflow's overlay at `sha` (W02-R07): its path and text, or where Flow would write
/// one and nothing, when it has none yet (W29-R06). None for what isn't a GitHub workflow.
async fn overlay_of(hub: &Hub, repo: &Repo, sha: &str, path: &str) -> Option<(String, String)> {
    if wf::file_kind(path) != Some(wf::FileKind::GhaWorkflow) {
        return None;
    }
    let stem = stem_of(path);
    for ext in ["yaml", "yml"] {
        let at = format!(".kiln/overlays/{stem}.{ext}");
        if let Ok(text) = crate::ci::load::blob(hub, repo, sha, &at).await {
            return Some((at, text));
        }
    }
    Some((format!(".kiln/overlays/{stem}.yaml"), String::new()))
}

/// What `git -C dir <args>` prints, or nothing when it fails: what it reads is the repository's
/// mirror, and a row without its tags or its last change still shows.
async fn git(hub: &Hub, dir: &str, args: &[&str]) -> String {
    let mut a: Vec<&str> = vec!["-C", dir];
    a.extend_from_slice(args);
    crate::gitops::run_git(hub, &a, crate::gitops::LS_REMOTE_TIMEOUT).await.map(|o| String::from_utf8_lossy(&o).into_owned()).unwrap_or_default()
}


/// A file's name without its folder and extension: `.kiln/ci.yaml` is `ci`.
fn stem_of(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.rsplit_once('.').map_or(name, |(s, _)| s).to_owned()
}

/// Who changed each file last and at which commit (W29-R02), from `git log --format=%x00%H%x00%an%x00%at --name-only`, newest first.
#[derive(Debug, Default)]
struct Changes {
    first: HashMap<String, (String, i64, String)>,
    /// Each commit's author, time and files, newest first.
    commits: Vec<(String, i64, String, Vec<String>)>,
}

impl Changes {
    fn get(&self, path: &str) -> Option<&(String, i64, String)> {
        self.first.get(path)
    }

    /// The newest change to any file under `folder`.
    fn get_folder(&self, folder: &str) -> Option<(String, i64, String)> {
        let prefix = format!("{folder}/");
        self.commits.iter().find(|(_, _, _, files)| files.iter().any(|f| folder.is_empty() || f.starts_with(&prefix))).map(|(w, t, sha, _)| (w.clone(), *t, sha.clone()))
    }
}

fn changes_of(log: &str) -> Changes {
    let mut out = Changes::default();
    // Each commit is `\0sha\0name\0time\n\nfile\nfile\n`: split on NUL after the empty first part.
    let parts: Vec<&str> = log.split('\0').skip(1).collect();
    for block in parts.chunks(3) {
        let [sha, who, rest] = block else { continue };
        let mut lines = rest.lines();
        let when = lines.next().and_then(|t| t.trim().parse::<i64>().ok()).unwrap_or_default();
        let files: Vec<String> = lines.map(str::trim).filter(|f| !f.is_empty()).map(str::to_owned).collect();
        for f in &files {
            out.first.entry(f.clone()).or_insert_with(|| ((*who).to_owned(), when, (*sha).to_owned()));
        }
        out.commits.push(((*who).to_owned(), when, (*sha).to_owned(), files));
    }
    out
}

/// The newest finished outcome, and whether one is under way, of each workflow on `branch` (v2
/// runs by their workflow's path, v1 runs as `v1:<pipeline>`); and of the runs at `released`'s
/// commit, all together: an action's or a reusable workflow's newest tag's own checks.
async fn runs_of(hub: &Hub, repo: &Repo, branch: &str, released: Option<&str>) -> anyhow::Result<(HashMap<String, (Option<Outcome>, bool)>, (Option<Outcome>, bool))> {
    let (id, branch, released) = (repo.id.clone(), branch.to_owned(), released.map(str::to_owned));
    hub.store
        .call(move |c| {
            let mut st = c.prepare(
                "SELECT r.pipeline, COALESCE(w.path, ''), r.status FROM runs r LEFT JOIN ci_workflows w ON w.id = r.workflow_id
                 WHERE r.repo_id = ?1 AND r.branch = ?2 AND r.draft IS NULL ORDER BY r.created_at DESC LIMIT ?3",
            )?;
            let mut by: HashMap<String, (Option<Outcome>, bool)> = HashMap::new();
            let rows = st.query_map(rusqlite::params![id, branch, RUNS_LOOK], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))?;
            for row in rows {
                let (pipeline, path, status) = row?;
                let key = if path.is_empty() { format!("v1:{pipeline}") } else { path };
                let seen = by.entry(key).or_insert((None, false));
                fold(seen, &status);
            }
            let mut tag = (None, false);
            if let Some(sha) = released {
                let mut st = c.prepare("SELECT status FROM runs WHERE repo_id = ?1 AND sha = ?2 ORDER BY created_at DESC LIMIT 100")?;
                let mut all: Vec<String> = Vec::new();
                for s in st.query_map(rusqlite::params![id, sha], |r| r.get::<_, String>(0))? {
                    all.push(s?);
                }
                tag = together(&all);
            }
            Ok((by, tag))
        })
        .await
}

/// A workflow's newest runs, newest first, folded into its newest finished outcome and whether a
/// newer one is under way.
fn fold(seen: &mut (Option<Outcome>, bool), status: &str) {
    if seen.0.is_some() {
        return;
    }
    match status {
        "succeeded" => seen.0 = Some(Outcome::Passed),
        "failed" | "timed_out" => seen.0 = Some(Outcome::Failed),
        "cancelled" => seen.0 = Some(Outcome::Cancelled),
        "pending" | "queued" | "running" => seen.1 = true,
        _ => {}
    }
}

/// A commit's runs as one outcome: failed if any failed, passed if every finished one passed.
fn together(statuses: &[String]) -> (Option<Outcome>, bool) {
    let running = statuses.iter().any(|s| matches!(s.as_str(), "pending" | "queued" | "running"));
    let failed = statuses.iter().any(|s| matches!(s.as_str(), "failed" | "timed_out"));
    let passed = statuses.iter().any(|s| s == "succeeded");
    let outcome = if failed {
        Some(Outcome::Failed)
    } else if passed {
        Some(Outcome::Passed)
    } else if statuses.iter().any(|s| s == "cancelled") {
        Some(Outcome::Cancelled)
    } else {
        None
    };
    (outcome, running)
}

/// A workflow's line: what starts it and how many jobs it has; a reusable workflow's inputs; a
/// GitHub workflow that doesn't run here, why.
fn workflow_line(l: &crate::ci::load::Loaded, kind: Kind, github_note: &str) -> String {
    let Some(w) = &l.workflow else { return format!("{} · can't be read", l.path) };
    if !l.runs && kind == Kind::Workflow {
        return format!("{} · {github_note}", l.path);
    }
    let jobs = match w.jobs.len() {
        1 => "1 job".to_owned(),
        n => format!("{n} jobs"),
    };
    if kind == Kind::Reusable {
        let inputs: Vec<&str> = w.trigger("workflow_call").map(|t| t.inputs.iter().map(|i| i.name.as_str()).collect()).unwrap_or_default();
        return if inputs.is_empty() { format!("reusable workflow · {jobs}") } else { format!("reusable workflow · inputs: {} · {jobs}", inputs.join(", ")) };
    }
    let mut on: Vec<String> = Vec::new();
    for t in &w.on {
        let words = core::trigger_line(t).words;
        if !on.contains(&words) {
            on.push(words);
        }
    }
    if on.is_empty() { jobs } else { format!("{} · {jobs}", on.join(", ")) }
}

/// An action's line: its kind, what it says it does, why it can't run, and its newest release.
fn action_line(kind: Kind, action: Option<&wf::Action>, errors: usize, routable: Option<bool>, released: Option<&str>) -> String {
    let mut parts = vec![kind.words().to_owned()];
    match (kind, routable) {
        (Kind::Shards, Some(false)) => parts.push("no machine runs shards microVMs yet".to_owned()),
        (Kind::Firecracker, Some(false)) => parts.push("no machine with KVM is connected".to_owned()),
        _ => {}
    }
    if errors > 0 {
        parts.push(if errors == 1 { "1 problem".to_owned() } else { format!("{errors} problems") });
    } else if let Some(d) = action.and_then(|a| a.description.as_deref()).map(str::trim).filter(|d| !d.is_empty()) {
        parts.push(d.to_owned());
    }
    if let Some(r) = released {
        parts.push(r.to_owned());
    }
    parts.join(" · ")
}

/// `GET /api/ship/pipelines?q=&repo=` (W29-R03): the repositories the caller may read
/// (`runs.read`), each with its heading's words and its rows, from memory.
pub async fn list(hub: &Arc<Hub>, q: &HashMap<String, String>, caller: &crate::authz::Caller) -> Result<Response, Fail> {
    let only = crate::authz::gate::readable(hub, caller, "runs.read").await;
    let m = model();
    m.listen(hub);
    let held = m.fresh(hub).await;
    let repo = q.get("repo").map(String::as_str).filter(|r| !r.is_empty());
    let text = q.get("q").map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty());
    // Each repository's JSON was rendered when it was gathered: the list joins them.
    // Drafts name their authors as diffs do (`user:ada`): so does the viewer.
    let (me, _) = crate::ci::diffs::author_of(&caller.name, Some(caller.kind));
    let body = core::list_text(held.iter().map(|h| &h.view), only.as_deref(), repo, text.as_deref(), Some(me.as_str()));
    Ok(srv::response(StatusCode::OK, "application/json", srv::full(body)))
}

/// `POST /api/ship/pipelines { repo, path, verb, … }` (docs/ci.md W29-R04, AC04): one of a
/// strip's verbs on the caller's own work, each as its own operation and with that operation's
/// right on the repository:
/// - `save_draft` (`content`, `title`): `ci.workflows.edit` with `draft` (W29-R11);
/// - `ready`: the caller's draft of the file asks for review (W29-R12);
/// - `discard`: the caller's draft of it is gone (W29-R13);
/// - `run` (`event`, `jobs`, `inputs`, `ref`, `sha`): the caller's draft of it as their test, or the
///   published workflow by hand (`ci.runs.trigger`, W29-R14).
pub async fn act(hub: &Arc<Hub>, caller: &crate::authz::Caller, body: &Value) -> Result<Response, Fail> {
    let text = |k: &str| body.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty());
    let need = |k: &str| text(k).ok_or_else(|| (StatusCode::BAD_REQUEST, format!("`{k}` is required")));
    let (name, verb) = (need("repo")?, need("verb")?);
    // New names no path: it makes one (W29-R08).
    let path = if verb == "new" { text("path").unwrap_or_default() } else { need("path")? };
    let missing = || (StatusCode::NOT_FOUND, format!("no repository `{name}` you may read"));
    let repo = hub.store.repos().await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?.into_iter().find(|r| r.name == name).ok_or_else(missing)?;
    let scope = crate::authz::Scope::Repo { repo: repo.name.clone() };
    if !caller.can("runs.read", &scope) {
        return Err(missing());
    }
    let out = match verb {
        "save_draft" => {
            let mut ask = body.clone();
            if let Some(o) = ask.as_object_mut() {
                o.insert("draft".into(), Value::Bool(true));
            }
            crate::ci::workflows::edit(hub, caller, &repo.name, path, &ask).await?
        }
        "ready" | "discard" => {
            if !caller.can("diffs.submit", &scope) {
                return Err((StatusCode::FORBIDDEN, format!("a draft of {} is its author's to {verb}, with `diffs.submit`", repo.name)));
            }
            let mut d = update_draft(hub, &repo.name, path, body).await?;
            if d.is_none() {
                let (author, _) = crate::ci::diffs::author_of(&caller.name, Some(caller.kind));
                d = crate::ci::pipeline_drafts::open_for(hub, &repo.name, path, &author).await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
            }
            let mut d = d.ok_or_else(|| (StatusCode::NOT_FOUND, format!("you have no draft of {path} in {}", repo.name)))?;
            if verb == "ready" { crate::ci::pipeline_drafts::ready(hub, caller, &repo, &mut d).await? } else { crate::ci::pipeline_drafts::discard(hub, caller, &repo, &mut d).await? }
        }
        "run" => {
            // kiln's update draft runs by the hand of whoever may submit to its repository (pl-7).
            if let Some(d) = update_draft(hub, &repo.name, path, body).await? {
                if !caller.can("diffs.submit", &scope) {
                    return Err((StatusCode::FORBIDDEN, format!("running kiln's update draft of {path} needs `diffs.submit` on {}", repo.name)));
                }
                let jobs: Vec<String> = body.get("jobs").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect()).unwrap_or_default();
                let inputs = body.get("inputs").and_then(Value::as_object).cloned().unwrap_or_default();
                let event = text("event").unwrap_or("push");
                let out = crate::ci::uploads::draft_run(hub, &repo, &d, caller, event, &jobs, inputs).await?;
                heard(&repo.name);
                return Ok(srv::json(StatusCode::OK, &out));
            }
            // The caller's draft of the file runs as their test; else the published workflow, by
            // hand (W29-R14).
            let (author, _) = crate::ci::diffs::author_of(&caller.name, Some(caller.kind));
            let drafted = crate::ci::pipeline_drafts::open_for(hub, &repo.name, path, &author).await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?.is_some();
            let mut ask = body.clone();
            if let Some(o) = ask.as_object_mut() {
                o.insert("workflow".into(), Value::String(path.to_owned()));
                o.insert("draft".into(), Value::Bool(drafted));
            }
            crate::ci::events::by_hand(hub, caller, &repo, &ask).await?
        }
        // Release by tag (W29-R21), as `ci.releases.tag`: `sha`, `tag`, `major`, `confirm`.
        "release" => {
            if !caller.can("diffs.land", &scope) {
                return Err((StatusCode::FORBIDDEN, format!("releasing {} needs `diffs.land`, as pushing a tag does (W29-R21)", repo.name)));
            }
            crate::ci::releases::tag(hub, caller, &repo.name, body).await?
        }
        // W29-R25's "lock it": the editor's file and overlay saved with their locks, in the caller's draft.
        "lock" => {
            let mut ask = serde_json::Map::new();
            ask.insert("path".into(), Value::from(path));
            for k in ["content", "files"] {
                if let Some(v) = body.get(k) {
                    ask.insert(k.into(), v.clone());
                }
            }
            crate::ci::locks::update(hub, caller, &repo.name, &Value::Object(ask)).await?
        }
        // A person's word on a lock-change draft's upstream code, as its head and evidence stand (W29-R25).
        "accept" => {
            let ask = serde_json::json!({ "draft": need("draft")?, "head": need("head")?, "evidence_sha256": need("evidence_sha256")? });
            crate::ci::locks::accept(hub, caller, &repo.name, &ask).await?
        }
        // New (W29-R08): a workflow's or action's first files, saved as the caller's draft; `dry` says them.
        "new" => {
            if body.get("dry").and_then(Value::as_bool) != Some(true) && !caller.can("diffs.submit", &scope) {
                return Err((StatusCode::FORBIDDEN, format!("a new workflow or action in {} is saved as a draft, which needs `diffs.submit`", repo.name)));
            }
            crate::ci::workflows::new_draft(hub, caller, &repo.name, body).await?
        }
        other => return Err((StatusCode::BAD_REQUEST, format!("`verb` is save_draft, ready, discard, run, release, new, lock or accept, not `{other}`"))),
    };
    heard(&repo.name);
    Ok(srv::json(StatusCode::OK, &out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w29_r02_who_changed_what_last() {
        let log = "\0a3f2c1d\0ada\x001700000300\n.kiln/ci.yaml\n.github/actions/setup/action.yml\n\n\0b07d1e9\0sam\x001700000200\n.github/actions/setup/run.sh\n.kiln/ci.yaml\n";
        let c = changes_of(log);
        assert_eq!(c.get(".kiln/ci.yaml"), Some(&("ada".to_owned(), 1_700_000_300, "a3f2c1d".to_owned())));
        assert_eq!(c.get_folder(".github/actions/setup"), Some(("ada".to_owned(), 1_700_000_300, "a3f2c1d".to_owned())));
        assert_eq!(c.get(".github/actions/setup/run.sh"), Some(&("sam".to_owned(), 1_700_000_200, "b07d1e9".to_owned())));
        assert_eq!(c.get_folder(".kiln/actions/none"), None);
        assert!(changes_of("").commits.is_empty());
    }

    #[test]
    fn w29_r02_a_release_workflow_names_the_tag_it_runs_on() {
        let parse = |s: &str| wf::check_model(".kiln/release.yaml", s, None).1.unwrap_or_default();
        let tags = parse("on:\n  push:\n    tags: ['v*', '!v*-rc*']\njobs: {}\n");
        assert!(release_trigger(&tags, Some("v0.2.0")));
        assert!(!release_trigger(&tags, Some("v0.2.0-rc1")));
        assert!(!release_trigger(&tags, None));
        assert!(!release_trigger(&parse("on: push\njobs: {}\n"), Some("v0.2.0")), "an ordinary workflow keeps its branch");
    }

    #[test]
    fn w29_r02_newest_runs_fold() {
        let mut seen = (None, false);
        for s in ["running", "failed", "succeeded"] {
            fold(&mut seen, s);
        }
        assert_eq!(seen, (Some(Outcome::Failed), true), "the newest finished, and one under way after it");
        assert_eq!(together(&["succeeded".into(), "failed".into()]), (Some(Outcome::Failed), false));
        assert_eq!(together(&["succeeded".into(), "queued".into()]), (Some(Outcome::Passed), true));
        assert_eq!(together(&[]), (None, false));
    }
}
