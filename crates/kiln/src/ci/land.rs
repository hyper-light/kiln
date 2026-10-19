//! The land queue (docs/ci.md W14-R07, R08). "Land D3" lands D3 and every diff below it, in
//! order, each passing its trunk's policy on its latest version; the strategy is chosen then
//! (`stack`: one commit each, `stack-squash`: one for all, `ff-only`: the trunk moved to the
//! stack as it is), within `land.strategies`. Per trunk, the queue:
//!
//! 1. builds a candidate: the next batch of entries (up to `queue.batch`, halved after each
//!    failure until the culprit is alone) applied onto the trunk's tip, without a working tree
//!    (`git merge-tree`; a temporary worktree where git is older than 2.40), each landed commit
//!    with `Differential-Revision:` and `Reviewed-by:` trailers; an entry that conflicts leaves
//!    the queue as "needs rebase";
//! 2. keeps it at `refs/kiln/land/<trunk>/<batch>` and starts its `land` and `merge_group`
//!    workflows; a required check (`land.checks`) none of them runs ejects the batch;
//! 3. when they all pass, moves the trunk to it in one compare-and-swap update (a trunk that
//!    moved meanwhile gets a new candidate); a failure ejects a lone entry with its reason, or
//!    splits a batch;
//! 4. marks the diffs landed, says so (`pull_request` `closed`), and restacks the open diffs
//!    above them onto the new trunk as rebase-only versions.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use http::StatusCode;
use kiln_ci::land::{self, Strategy};
use kiln_ci::review::State;
use kiln_ci::trigger::{Event, Source};
use rusqlite::{OptionalExtension as _, params};
use serde_json::{Value, json};

use super::Fail;
use super::diffs::{self, Diff};
use crate::authz::Caller;
use crate::gitops::{self, LS_REMOTE_TIMEOUT};
use crate::hub::Hub;
use crate::store::Repo;

fn fail(e: anyhow::Error) -> Fail {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}

fn kicker() -> &'static crate::rt::Notify {
    static KICK: OnceLock<crate::rt::Notify> = OnceLock::new();
    KICK.get_or_init(crate::rt::Notify::new)
}

/// Wakes the queue: an entry came, a run finished.
pub fn kick() {
    kicker().notify();
}

/// Failures since the last land, per trunk: how far the next batch is halved.
fn failures() -> &'static parking_lot::Mutex<HashMap<(String, String), u32>> {
    static F: OnceLock<parking_lot::Mutex<HashMap<(String, String), u32>>> = OnceLock::new();
    F.get_or_init(Default::default)
}

/// The queue, for as long as the server runs: each trunk with entries, every few seconds or when
/// kicked.
pub async fn run_loop(hub: Arc<Hub>) -> anyhow::Result<()> {
    use futures::FutureExt as _;
    loop {
        if let Err(e) = tick(&hub).await {
            tracing::warn!(error = %format!("{e:#}"), "the land queue stumbled");
        }
        futures::select_biased! {
            () = hub.shutdown.cancelled().fuse() => return Ok(()),
            () = kicker().notified().fuse() => {}
            () = crate::rt::sleep(Duration::from_secs(3)).fuse() => {}
        }
    }
}

/// An entry of the queue, as kept.
#[derive(Debug, Clone)]
struct Entry {
    id: String,
    diffs: Vec<String>,
    strategy: Strategy,
    state: String,
    batch: Option<String>,
    base: Option<String>,
    candidate: Option<String>,
    /// The workflows started on the candidate (their paths).
    started: Vec<String>,
    /// The commit that carries each diff on the candidate.
    commits: BTreeMap<String, String>,
    enqueued_by: String,
}

fn entry_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Entry> {
    let diffs: String = r.get(1)?;
    let strategy: String = r.get(2)?;
    let started: Option<String> = r.get(7)?;
    let commits: Option<String> = r.get(8)?;
    Ok(Entry {
        id: r.get(0)?,
        diffs: serde_json::from_str(&diffs).unwrap_or_default(),
        strategy: Strategy::parse(&strategy).unwrap_or(Strategy::Stack),
        state: r.get(3)?,
        batch: r.get(4)?,
        base: r.get(5)?,
        candidate: r.get(6)?,
        started: started.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default(),
        commits: commits.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default(),
        enqueued_by: r.get(9)?,
    })
}

const ENTRY_COLUMNS: &str = "id, diffs, strategy, state, batch, base, candidate, checks, commits, enqueued_by";

async fn entries(hub: &Hub, repo_id: &str, trunk: &str, states: &'static [&'static str]) -> anyhow::Result<Vec<Entry>> {
    let (repo_id, trunk) = (repo_id.to_owned(), trunk.to_owned());
    hub.store
        .call(move |c| {
            let placeholders = states.iter().map(|s| format!("'{s}'")).collect::<Vec<_>>().join(", ");
            let sql = format!("SELECT {ENTRY_COLUMNS} FROM land_queue WHERE repo_id = ?1 AND trunk = ?2 AND state IN ({placeholders}) ORDER BY created_at, rowid");
            let mut st = c.prepare(&sql)?;
            let rows = st.query_map(params![repo_id, trunk], entry_row)?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
}

/// Sets each of `ids`' diffs' state.
async fn set_diff_state(hub: &Hub, ids: &[String], state: State) -> anyhow::Result<()> {
    let ids = ids.to_vec();
    hub.store
        .call(move |c| {
            let now = crate::util::now_unix();
            for id in &ids {
                c.execute("UPDATE diffs SET state = ?2, updated_at = ?3 WHERE id = ?1", params![id, state.as_str(), now])?;
            }
            Ok(())
        })
        .await
}

/// Says where an entry is (`queue`: `queued`, `testing`, `landed`, `ejected`, `cancelled`), with
/// its diffs and whose they are, for whoever follows them.
async fn tell(hub: &Hub, e: &Entry, state: &str, reason: Option<&str>) {
    let mut list = Vec::new();
    for id in &e.diffs {
        if let Ok(Some(d)) = diffs::find(hub, id).await {
            list.push(json!({ "diff": d.name, "repo": d.repo, "author": d.author, "author_for": d.author_for }));
        }
    }
    hub.emit(&json!({ "type": "queue", "entry": e.id, "state": state, "reason": reason, "strategy": e.strategy.as_str(), "diffs": list, "enqueued_by": e.enqueued_by }));
}

/// GitHub's merge queue events for diffs (W03-R02's native table): `pull_request` `enqueued`, or
/// `dequeued` with GitHub's `reason` (`MERGE`, `MANUAL`, `CI_FAILURE`, `INVALID_MERGE_COMMIT`,
/// `BRANCH_PROTECTIONS`, `UNKNOWN_REMOVAL_REASON`), sent by `by`.
async fn queue_event(hub: &Hub, ids: &[String], action: &str, reason: Option<&str>, by: &str) {
    for id in ids {
        let Ok(Some(d)) = diffs::find(hub, id).await else { continue };
        let Some(repo) = hub.store.repos().await.ok().and_then(|r| r.into_iter().find(|r| r.name == d.repo)) else { continue };
        if let Err(err) = diffs::activity(hub, &repo, &d, action, by, |p| {
            if let Some(r) = reason {
                p.insert("reason".into(), json!(r));
            }
        })
        .await
        {
            tracing::warn!(diff = %d.name, %action, error = %format!("{err:#}"), "its workflows didn't start");
        }
    }
}

/// Takes an entry out of the queue, saying why (`code`: GitHub's reason, for `dequeued`); its
/// diffs go back to their review state.
async fn eject(hub: &Arc<Hub>, e: &Entry, reason: &str, code: &str) -> anyhow::Result<()> {
    let (id, why) = (e.id.clone(), reason.to_owned());
    hub.store.call(move |c| Ok(c.execute("UPDATE land_queue SET state = 'ejected', reason = ?2, updated_at = ?3 WHERE id = ?1", params![id, why, crate::util::now_unix()]).map(|_| ())?)).await?;
    set_diff_state(hub, &e.diffs, State::Accepted).await?;
    hub.store.audit("kiln".into(), "queue.eject".into(), format!("{}: {reason}", e.id)).await;
    tell(hub, e, "ejected", Some(reason)).await;
    queue_event(hub, &e.diffs, "dequeued", Some(code), "kiln").await;
    crate::rt::spawn(super::notify::land_ejected(Arc::clone(hub), e.diffs.clone(), reason.to_owned()));
    tracing::info!(entry = %e.id, %reason, "ejected from the land queue");
    Ok(())
}

// ------------------------------------------------------------------ enqueueing

/// The open diffs `d` stands on, bottom first, and `d`.
async fn stack_below(hub: &Hub, d: &Diff) -> Result<Vec<Diff>, Fail> {
    let mut chain = vec![d.clone()];
    let mut at = d.parent.clone();
    while let Some(name) = at {
        let Some(below) = diffs::find(hub, &name).await.map_err(fail)? else { break };
        if below.state == State::Landed.as_str() {
            break;
        }
        if chain.iter().any(|x| x.id == below.id) || chain.len() > kiln_ci::review::MAX_STACK {
            break;
        }
        at = below.parent.clone();
        chain.push(below);
    }
    chain.reverse();
    Ok(chain)
}

/// `POST /api/diffs/:id/land` `{ strategy }`: the diff and those below it, into the queue.
pub async fn enqueue(hub: &Arc<Hub>, d: &Diff, caller: &Caller, body: &Value) -> Result<Value, Fail> {
    let (repo, path) = diffs::home(hub, d).await?;
    let (policy, _) = super::review::policy(hub, &path, &d.trunk).await;
    let asked = match body.get("strategy").and_then(Value::as_str) {
        Some(s) => Some(Strategy::parse(s).ok_or_else(|| (StatusCode::BAD_REQUEST, format!("no strategy `{s}`: stack, stack-squash, squash, rebase, merge or ff-only")))?),
        None => None,
    };
    let strategy = policy.strategy(asked, d.kind != "diff").map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e))?;
    // A squash of a stack is the stack as one commit.
    let strategy = if strategy == Strategy::Squash && d.kind == "diff" { Strategy::StackSquash } else { strategy };
    let chain = stack_below(hub, d).await?;
    let mut why = Vec::new();
    for x in &chain {
        let r = super::review::readiness(hub, x).await?;
        for b in r.get("blockers").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
            why.push(format!("{}: {b}", x.name));
        }
    }
    if !why.is_empty() {
        return Err((StatusCode::CONFLICT, format!("{} can't land yet: {}", d.name, why.join("; "))));
    }
    let ids: Vec<String> = chain.iter().map(|x| x.id.clone()).collect();
    let names: Vec<String> = chain.iter().map(|x| x.name.clone()).collect();
    let (entry, repo_id, trunk, by, list) = (crate::util::new_id(), repo.id.clone(), d.trunk.clone(), caller.name.clone(), serde_json::to_string(&ids).unwrap_or_default());
    let e2 = entry.clone();
    hub.store
        .call(move |c| {
            let now = crate::util::now_unix();
            c.execute(
                "INSERT INTO land_queue (id, repo_id, trunk, diffs, strategy, state, enqueued_by, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, 'queued', ?6, ?7, ?7)",
                params![e2, repo_id, trunk, list, strategy.as_str(), by, now],
            )?;
            Ok(())
        })
        .await
        .map_err(fail)?;
    set_diff_state(hub, &ids, State::Landing).await.map_err(fail)?;
    hub.store.audit(caller.name.clone(), "diffs.land".into(), format!("{} {} ({}) by {}", repo.name, names.join(" "), d.trunk, strategy.as_str())).await;
    queue_event(hub, &ids, "enqueued", None, &caller.name).await;
    kick();
    Ok(json!({ "entry": entry, "diffs": names, "trunk": d.trunk, "strategy": strategy.as_str(), "state": "queued" }))
}

/// `GET /api/repos/:repo/queue`: its entries, newest first (the last 100 that finished too).
pub async fn list(hub: &Hub, repo: &str) -> Result<Value, Fail> {
    let key = repo.to_owned();
    let rows: Vec<Value> = hub
        .store
        .call(move |c| {
            let mut st = c.prepare(
                "SELECT q.id, q.trunk, q.diffs, q.strategy, q.state, q.batch, q.candidate, q.reason, q.enqueued_by, q.created_at, q.updated_at FROM land_queue q JOIN repos p ON p.id = q.repo_id
                 WHERE p.name = ?1 OR p.id = ?1 ORDER BY q.state IN ('queued', 'testing') DESC, q.created_at DESC LIMIT 200",
            )?;
            let rows = st.query_map([&key], |r| {
                let ids: Vec<String> = serde_json::from_str(&r.get::<_, String>(2)?).unwrap_or_default();
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, ids, r.get::<_, String>(3)?, r.get::<_, String>(4)?, r.get::<_, Option<String>>(5)?, r.get::<_, Option<String>>(6)?, r.get::<_, Option<String>>(7)?, r.get::<_, String>(8)?, r.get::<_, i64>(9)?, r.get::<_, i64>(10)?))
            })?;
            let mut out = Vec::new();
            let mut name = c.prepare("SELECT 'D' || number FROM diffs WHERE id = ?1")?;
            for row in rows.filter_map(Result::ok) {
                let (id, trunk, ids, strategy, state, batch, candidate, reason, by, created, updated) = row;
                let names: Vec<String> = ids.iter().filter_map(|i| name.query_row([i], |r| r.get::<_, String>(0)).ok()).collect();
                out.push(json!({ "id": id, "trunk": trunk, "diffs": names, "strategy": strategy, "state": state, "batch": batch, "candidate": candidate, "reason": reason, "enqueued_by": by, "created_at": created, "updated_at": updated }));
            }
            Ok(out)
        })
        .await
        .map_err(fail)?;
    Ok(json!({ "entries": rows }))
}

/// `DELETE /api/queue/:entry`: out of the queue (not once it landed).
pub async fn cancel(hub: &Hub, entry: &str, caller: &Caller) -> Result<Value, Fail> {
    let key = entry.to_owned();
    let found = hub.store.call(move |c| Ok(c.query_row(&format!("SELECT {ENTRY_COLUMNS} FROM land_queue WHERE id = ?1"), [&key], entry_row).optional()?)).await.map_err(fail)?;
    let Some(e) = found else { return Err((StatusCode::NOT_FOUND, "no such entry".into())) };
    if !matches!(e.state.as_str(), "queued" | "testing") {
        return Err((StatusCode::CONFLICT, format!("the entry is {}", e.state)));
    }
    let id = e.id.clone();
    hub.store.call(move |c| Ok(c.execute("UPDATE land_queue SET state = 'cancelled', reason = 'cancelled', updated_at = ?2 WHERE id = ?1", params![id, crate::util::now_unix()]).map(|_| ())?)).await.map_err(fail)?;
    set_diff_state(hub, &e.diffs, State::Accepted).await.map_err(fail)?;
    // The rest of its batch is tested again without it.
    if let Some(batch) = &e.batch {
        let b = batch.clone();
        hub.store.call(move |c| Ok(c.execute("UPDATE land_queue SET state = 'queued', batch = NULL, candidate = NULL, base = NULL WHERE batch = ?1 AND state = 'testing'", [&b]).map(|_| ())?)).await.map_err(fail)?;
    }
    hub.store.audit(caller.name.clone(), "queue.cancel".into(), e.id.clone()).await;
    queue_event(hub, &e.diffs, "dequeued", Some("MANUAL"), &caller.name).await;
    kick();
    Ok(json!({ "entry": e.id, "state": "cancelled" }))
}

// ------------------------------------------------------------------ building candidates

/// The git version, `(major, minor)`.
async fn git_version(hub: &Hub) -> (u32, u32) {
    static V: OnceLock<(u32, u32)> = OnceLock::new();
    if let Some(v) = V.get() {
        return *v;
    }
    let out = gitops::run_git(hub, &["version"], LS_REMOTE_TIMEOUT).await.map(|o| String::from_utf8_lossy(&o).into_owned()).unwrap_or_default();
    let mut nums = out.split_whitespace().nth(2).unwrap_or_default().split('.').map(|n| n.parse::<u32>().unwrap_or(0));
    let v = (nums.next().unwrap_or(0), nums.next().unwrap_or(0));
    *V.get_or_init(|| v)
}

/// Why a commit didn't go onto the candidate.
enum Refusal {
    /// It conflicts there: the files.
    Conflict(Vec<String>),
    Other(String),
}

/// The tree of `commit` (whose parent is `parent`) applied onto `onto`: a cherry-pick without a
/// working tree.
async fn pick_tree(hub: &Hub, path: &str, onto: &str, commit: &str, parent: &str) -> Result<String, Refusal> {
    if git_version(hub).await >= (2, 40) {
        let base = format!("--merge-base={parent}");
        let r = diffs::git_run(hub, &["-C", path, "merge-tree", "--write-tree", "--no-messages", "--name-only", &base, onto, commit], &[], Vec::new()).await.map_err(|e| Refusal::Other(format!("{e:#}")))?;
        let text = String::from_utf8_lossy(&r.out).into_owned();
        let mut lines = text.lines();
        let tree = lines.next().unwrap_or_default().trim().to_owned();
        if r.ok && !tree.is_empty() {
            return Ok(tree);
        }
        let files: Vec<String> = lines.filter(|l| !l.is_empty()).map(str::to_owned).collect();
        if !files.is_empty() {
            return Err(Refusal::Conflict(files));
        }
        return Err(Refusal::Other(String::from_utf8_lossy(&r.err).trim().to_owned()));
    }
    // Older git: a temporary worktree of the bare repository.
    let tmp = std::env::temp_dir().join(format!("kiln-land-{}", crate::util::new_id()));
    let t = tmp.to_string_lossy().into_owned();
    let made = gitops::run_git(hub, &["-C", path, "worktree", "add", "--quiet", "--detach", &t, onto], LS_REMOTE_TIMEOUT).await;
    let result = match made {
        Err(e) => Err(Refusal::Other(format!("{e:#}"))),
        Ok(_) => {
            let r = diffs::git_run(hub, &["-C", &t, "-c", "user.name=kiln", "-c", "user.email=kiln@localhost", "cherry-pick", "--no-commit", "--allow-empty", commit], &[], Vec::new()).await.map_err(|e| Refusal::Other(format!("{e:#}")));
            match r {
                Ok(r) if r.ok => gitops::run_git(hub, &["-C", &t, "write-tree"], LS_REMOTE_TIMEOUT).await.map(|o| String::from_utf8_lossy(&o).trim().to_owned()).map_err(|e| Refusal::Other(format!("{e:#}"))),
                Ok(_) => {
                    let files = gitops::run_git(hub, &["-C", &t, "diff", "--name-only", "--diff-filter=U"], LS_REMOTE_TIMEOUT).await.map(|o| String::from_utf8_lossy(&o).lines().map(str::to_owned).collect()).unwrap_or_default();
                    Err(Refusal::Conflict(files))
                }
                Err(e) => Err(e),
            }
        }
    };
    let _ = gitops::run_git(hub, &["-C", path, "worktree", "remove", "--force", &t], LS_REMOTE_TIMEOUT).await;
    let _ = std::fs::remove_dir_all(&tmp);
    result
}

/// The tree of `theirs` merged into `ours` (their merge base found by git), without a working
/// tree.
async fn merge_tree(hub: &Hub, path: &str, ours: &str, theirs: &str) -> Result<String, Refusal> {
    if git_version(hub).await >= (2, 38) {
        let r = diffs::git_run(hub, &["-C", path, "merge-tree", "--write-tree", "--no-messages", "--name-only", ours, theirs], &[], Vec::new()).await.map_err(|e| Refusal::Other(format!("{e:#}")))?;
        let text = String::from_utf8_lossy(&r.out).into_owned();
        let mut lines = text.lines();
        let tree = lines.next().unwrap_or_default().trim().to_owned();
        if r.ok && !tree.is_empty() {
            return Ok(tree);
        }
        let files: Vec<String> = lines.filter(|l| !l.is_empty()).map(str::to_owned).collect();
        return Err(if files.is_empty() { Refusal::Other(String::from_utf8_lossy(&r.err).trim().to_owned()) } else { Refusal::Conflict(files) });
    }
    let tmp = std::env::temp_dir().join(format!("kiln-land-{}", crate::util::new_id()));
    let t = tmp.to_string_lossy().into_owned();
    let result = match gitops::run_git(hub, &["-C", path, "worktree", "add", "--quiet", "--detach", &t, ours], LS_REMOTE_TIMEOUT).await {
        Err(e) => Err(Refusal::Other(format!("{e:#}"))),
        Ok(_) => match diffs::git_run(hub, &["-C", &t, "-c", "user.name=kiln", "-c", "user.email=kiln@localhost", "merge", "--no-commit", "--no-ff", "--quiet", theirs], &[], Vec::new()).await {
            Ok(r) if r.ok => gitops::run_git(hub, &["-C", &t, "write-tree"], LS_REMOTE_TIMEOUT).await.map(|o| String::from_utf8_lossy(&o).trim().to_owned()).map_err(|e| Refusal::Other(format!("{e:#}"))),
            Ok(_) => Err(Refusal::Conflict(gitops::run_git(hub, &["-C", &t, "diff", "--name-only", "--diff-filter=U"], LS_REMOTE_TIMEOUT).await.map(|o| String::from_utf8_lossy(&o).lines().map(str::to_owned).collect()).unwrap_or_default())),
            Err(e) => Err(Refusal::Other(format!("{e:#}"))),
        },
    };
    let _ = gitops::run_git(hub, &["-C", path, "worktree", "remove", "--force", &t], LS_REMOTE_TIMEOUT).await;
    let _ = std::fs::remove_dir_all(&tmp);
    result
}

/// A change request applied onto `tip` by its strategy (W14-R07): `squash` (one commit), `merge`
/// (a merge commit), `rebase` (each commit replayed), `ff-only` (the branch's head, when the
/// trunk is its ancestor).
async fn apply_change_request(hub: &Hub, path: &str, tip: &str, strategy: Strategy, d: &Diff, v: &diffs::Version, reviewers: &[String]) -> Result<Applied, Refusal> {
    let other = |x: anyhow::Error| Refusal::Other(format!("{x:#}"));
    let head = v.commit.clone();
    let mut commits = BTreeMap::new();
    let message = format!("{}\n\n{}", d.title, d.summary);
    match strategy {
        Strategy::FfOnly => {
            if gitops::run_git(hub, &["-C", path, "merge-base", "--is-ancestor", tip, &head], LS_REMOTE_TIMEOUT).await.is_err() {
                return Err(Refusal::Other(format!("ff-only: {} has moved past the branch; rebase it onto {} or land with another strategy", d.trunk, d.trunk)));
            }
            commits.insert(d.id.clone(), head.clone());
            Ok(Applied { tip: head, commits })
        }
        Strategy::Merge => {
            let tree = merge_tree(hub, path, tip, &head).await?;
            let who = author(hub, path, &head).await.map_err(other)?;
            let msg = land::landed_message(&format!("Merge {}: {}\n\n{}", d.name, d.title, d.summary), &d.name, reviewers);
            let c = commit_tree(hub, path, &tree, &[tip, &head], &who, &msg).await.map_err(other)?;
            commits.insert(d.id.clone(), c.clone());
            Ok(Applied { tip: c, commits })
        }
        Strategy::Rebase => {
            let list = gitops::run_git(hub, &["-C", path, "rev-list", "--reverse", "--topo-order", "--no-merges", &format!("{}..{head}", v.base)], LS_REMOTE_TIMEOUT).await.map(|o| String::from_utf8_lossy(&o).lines().map(str::to_owned).collect::<Vec<_>>()).map_err(other)?;
            let mut at = tip.to_owned();
            for c in &list {
                let parent = gitops::run_git(hub, &["-C", path, "rev-parse", &format!("{c}^")], LS_REMOTE_TIMEOUT).await.map(|o| String::from_utf8_lossy(&o).trim().to_owned()).map_err(other)?;
                let tree = pick_tree(hub, path, &at, c, &parent).await?;
                let who = author(hub, path, c).await.map_err(other)?;
                let msg = gitops::run_git(hub, &["-C", path, "log", "-1", "--format=%B", c], LS_REMOTE_TIMEOUT).await.map(|o| String::from_utf8_lossy(&o).into_owned()).unwrap_or_default();
                at = commit_tree(hub, path, &tree, &[&at], &who, &land::landed_message(&msg, &d.name, reviewers)).await.map_err(other)?;
            }
            commits.insert(d.id.clone(), at.clone());
            Ok(Applied { tip: at, commits })
        }
        // `squash`, the default.
        _ => {
            let tree = merge_tree(hub, path, tip, &head).await?;
            let who = author(hub, path, &head).await.map_err(other)?;
            let c = commit_tree(hub, path, &tree, &[tip], &who, &land::landed_message(&message, &d.name, reviewers)).await.map_err(other)?;
            commits.insert(d.id.clone(), c.clone());
            Ok(Applied { tip: c, commits })
        }
    }
}

/// The author of `commit`: name, email, date (raw).
async fn author(hub: &Hub, path: &str, commit: &str) -> anyhow::Result<(String, String, String)> {
    let o = gitops::run_git(hub, &["-C", path, "log", "-1", "--format=%an%x00%ae%x00%ad", "--date=raw", commit], LS_REMOTE_TIMEOUT).await?;
    let text = String::from_utf8_lossy(&o).trim().to_owned();
    let mut f = text.split('\0');
    Ok((f.next().unwrap_or_default().to_owned(), f.next().unwrap_or_default().to_owned(), f.next().unwrap_or_default().to_owned()))
}

/// A commit of `tree` on `parents`, by `who` (committed by kiln, signed when
/// `[ci.land_signing]` says so), saying `message`.
async fn commit_tree(hub: &Hub, path: &str, tree: &str, parents: &[&str], who: &(String, String, String), message: &str) -> anyhow::Result<String> {
    let signing = hub.cfg.ci.land_signing.as_ref().filter(|s| !s.key.is_empty());
    let (fmt, key) = signing.map(|s| (format!("gpg.format={}", s.format.as_deref().unwrap_or("ssh")), format!("user.signingkey={}", s.key))).unwrap_or_default();
    let mut args: Vec<&str> = Vec::new();
    if signing.is_some() {
        args.extend(["-c", &fmt, "-c", &key]);
    }
    args.extend(["-C", path, "commit-tree", tree]);
    for p in parents {
        args.extend(["-p", p]);
    }
    if signing.is_some() {
        args.push("-S");
    }
    args.extend(["-F", "-"]);
    let env = [("GIT_AUTHOR_NAME", who.0.as_str()), ("GIT_AUTHOR_EMAIL", who.1.as_str()), ("GIT_AUTHOR_DATE", who.2.as_str()), ("GIT_COMMITTER_NAME", "kiln"), ("GIT_COMMITTER_EMAIL", "kiln@localhost")];
    let r = diffs::git_run(hub, &args, &env, message.as_bytes().to_vec()).await?;
    anyhow::ensure!(r.ok, "git commit-tree: {}", String::from_utf8_lossy(&r.err).trim());
    Ok(String::from_utf8_lossy(&r.out).trim().to_owned())
}

/// What an entry put on the candidate: its new tip, and the commit that carries each diff.
struct Applied {
    tip: String,
    commits: BTreeMap<String, String>,
}

/// An entry's diffs applied onto `tip` by its strategy.
async fn apply(hub: &Hub, path: &str, tip: &str, e: &Entry) -> Result<Applied, Refusal> {
    let mut chain = Vec::new();
    for id in &e.diffs {
        let d = diffs::find(hub, id).await.map_err(|x| Refusal::Other(format!("{x:#}")))?.ok_or_else(|| Refusal::Other("a diff of it is gone".into()))?;
        let v = diffs::versions(hub, &d).await.map_err(|x| Refusal::Other(format!("{x:#}")))?.pop().ok_or_else(|| Refusal::Other(format!("{} has no version", d.name)))?;
        let f = super::review::facts(hub, &d, &diffs::versions(hub, &d).await.unwrap_or_default()).await.map_err(|(_, m)| Refusal::Other(m))?;
        let (policy, _) = super::review::policy(hub, path, &d.trunk).await;
        let reviewers: Vec<String> = f.accepting(&policy.land).into_iter().collect();
        chain.push((d, v, reviewers));
    }
    // A change request lands whole, by its own strategies.
    if let [(d, v, reviewers)] = chain.as_slice()
        && d.kind == "change_request"
    {
        return apply_change_request(hub, path, tip, e.strategy, d, v, reviewers).await;
    }
    let msg_of = |sha: &str| {
        let s = sha.to_owned();
        async move { gitops::run_git(hub, &["-C", path, "log", "-1", "--format=%B", &s], LS_REMOTE_TIMEOUT).await.map(|o| String::from_utf8_lossy(&o).into_owned()).unwrap_or_default() }
    };
    let mut commits = BTreeMap::new();
    match e.strategy {
        Strategy::FfOnly => {
            // The stack exactly as it is, on the trunk's tip.
            let shas: Vec<String> = chain.iter().map(|(_, v, _)| v.commit.clone()).collect();
            let Some(top) = shas.last().cloned() else { return Err(Refusal::Other("nothing to land".into())) };
            let between = gitops::run_git(hub, &["-C", path, "rev-list", "--reverse", "--topo-order", &format!("{tip}..{top}")], LS_REMOTE_TIMEOUT).await.map(|o| String::from_utf8_lossy(&o).lines().map(str::to_owned).collect::<Vec<_>>()).unwrap_or_default();
            let on_tip = gitops::run_git(hub, &["-C", path, "merge-base", "--is-ancestor", tip, &top], LS_REMOTE_TIMEOUT).await.is_ok();
            if !on_tip || between != shas {
                return Err(Refusal::Other("ff-only: the stack isn't on the trunk's tip as it is; `kiln sync` and submit again, or land with another strategy".into()));
            }
            for (d, v, _) in &chain {
                commits.insert(d.id.clone(), v.commit.clone());
            }
            Ok(Applied { tip: top, commits })
        }
        Strategy::StackSquash | Strategy::Squash => {
            let mut scratch = tip.to_owned();
            let mut tree = String::new();
            for (_, v, _) in &chain {
                tree = pick_tree(hub, path, &scratch, &v.commit, &v.parent).await?;
                let who = author(hub, path, &v.commit).await.map_err(|x| Refusal::Other(format!("{x:#}")))?;
                scratch = commit_tree(hub, path, &tree, &[&scratch], &who, "scratch").await.map_err(|x| Refusal::Other(format!("{x:#}")))?;
            }
            let Some((top, top_v, _)) = chain.last() else { return Err(Refusal::Other("nothing to land".into())) };
            let mut message = format!("{}\n\n", top.title);
            for (d, _, _) in &chain {
                message.push_str(&format!("- {}: {}\n", d.name, d.title));
            }
            let mut reviewers: Vec<String> = chain.iter().flat_map(|(_, _, r)| r.clone()).collect();
            reviewers.sort();
            reviewers.dedup();
            let revisions: Vec<String> = chain.iter().map(|(d, _, _)| d.name.clone()).collect();
            let mut message = land::landed_message(&message, &revisions.join(", "), &reviewers);
            if revisions.len() > 1 {
                message = message.replace(&format!("Differential-Revision: {}", revisions.join(", ")), &revisions.iter().map(|r| format!("Differential-Revision: {r}")).collect::<Vec<_>>().join("\n"));
            }
            let who = author(hub, path, &top_v.commit).await.map_err(|x| Refusal::Other(format!("{x:#}")))?;
            let c = commit_tree(hub, path, &tree, &[tip], &who, &message).await.map_err(|x| Refusal::Other(format!("{x:#}")))?;
            for (d, _, _) in &chain {
                commits.insert(d.id.clone(), c.clone());
            }
            Ok(Applied { tip: c, commits })
        }
        // `stack` (and anything else a diff lands by): one commit each, in order.
        _ => {
            let mut at = tip.to_owned();
            for (d, v, reviewers) in &chain {
                let tree = pick_tree(hub, path, &at, &v.commit, &v.parent).await?;
                let who = author(hub, path, &v.commit).await.map_err(|x| Refusal::Other(format!("{x:#}")))?;
                let message = land::landed_message(&msg_of(&v.commit).await, &d.name, reviewers);
                at = commit_tree(hub, path, &tree, &[&at], &who, &message).await.map_err(|x| Refusal::Other(format!("{x:#}")))?;
                commits.insert(d.id.clone(), at.clone());
            }
            Ok(Applied { tip: at, commits })
        }
    }
}

/// A commit as payloads describe it: its tree, message, and who and when.
async fn commit_of(hub: &Hub, path: &str, sha: &str) -> kiln_ci::trigger::context::Commit {
    use kiln_ci::trigger::context::{Commit, Person};
    let o = gitops::run_git(hub, &["-C", path, "show", "-s", "--format=%T%x00%an%x00%ae%x00%aI%x00%cn%x00%ce%x00%B", sha], LS_REMOTE_TIMEOUT).await.unwrap_or_default();
    let text = String::from_utf8_lossy(&o).into_owned();
    let f: Vec<&str> = text.splitn(7, '\0').collect();
    let get = |i: usize| f.get(i).copied().unwrap_or_default().trim_end_matches('\n').to_owned();
    let person = |name: String, email: String| Person { name, email, username: None };
    Commit { id: sha.to_owned(), tree_id: get(0), message: get(6), timestamp: get(3), author: person(get(1), get(2)), committer: person(get(4), get(5)) }
}

/// The candidate's `land` and `merge_group` events: GitHub's payload, with kiln's own part
/// (`kiln.land`).
fn candidate_events(ctx: &kiln_ci::trigger::context::Repo, repo: &Repo, trunk: &str, base: &str, tip: &kiln_ci::trigger::context::Commit, batch: &str, names: &[String]) -> (Event, Event) {
    let head_ref = format!("refs/kiln/land/{trunk}/{batch}");
    let kiln = kiln_ci::trigger::context::Person { name: "kiln".into(), email: String::new(), username: Some("kiln[bot]".into()) };
    let mut payload = kiln_ci::trigger::context::merge_group(ctx, trunk, base, tip, &head_ref, &kiln);
    if let Some(p) = payload.as_object_mut() {
        p.insert("kiln".into(), json!({ "land": { "trunk": trunk, "batch": batch, "diffs": names } }));
    }
    let tip = tip.id.as_str();
    let mg = Event { name: "merge_group".into(), action: "checks_requested".into(), repo: repo.name.clone(), git_ref: head_ref.clone(), sha: tip.to_owned(), base_ref: Some(trunk.to_owned()), actor: "kiln".into(), trusted: true, source: Source::Native, payload, ..Event::default() };
    let landing = Event { name: "land".into(), action: String::new(), ..mg.clone() };
    (mg, landing)
}

/// Starts the candidate's workflows: those whose `on` takes `merge_group` or `land`. Their paths.
async fn start_checks(hub: &Hub, repo: &Repo, events: &(Event, Event)) -> anyhow::Result<Vec<String>> {
    if !hub.ci.enabled() {
        return Ok(Vec::new());
    }
    let commit = super::load::load(hub, repo, &events.0.sha).await?;
    let mut started = Vec::new();
    for l in commit.workflows.iter().filter(|l| l.runs && (l.dialect == kiln_ci::expr::Dialect::Gha || hub.cfg.ci.kiln_workflows)) {
        for e in [&events.0, &events.1] {
            if super::events::start(hub, repo, e, l, "land candidate", "kiln", Value::Null).await? {
                started.push(l.path.clone());
                break;
            }
        }
    }
    Ok(started)
}

/// A required check the candidate's workflows don't cover.
fn uncovered(required: &[String], started: &[String], names: &BTreeMap<String, Option<String>>) -> Vec<String> {
    let stem = |p: &str| p.rsplit('/').next().unwrap_or(p).trim_end_matches(".yml").trim_end_matches(".yaml").to_owned();
    required
        .iter()
        .filter(|c| {
            let head = c.split('/').next().unwrap_or(c);
            !(started.iter().any(|s| s == c.as_str() || stem(s) == head || s == head) || names.contains_key(c.as_str()) || names.keys().any(|k| k.split('/').next() == Some(head)))
        })
        .cloned()
        .collect()
}

// ------------------------------------------------------------------ the queue's step

async fn tick(hub: &Arc<Hub>) -> anyhow::Result<()> {
    let groups: Vec<(String, String)> = hub
        .store
        .call(|c| {
            let mut st = c.prepare("SELECT DISTINCT repo_id, trunk FROM land_queue WHERE state IN ('queued', 'testing')")?;
            let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await?;
    if groups.is_empty() {
        return Ok(());
    }
    let repos = hub.store.repos().await?;
    for (repo_id, trunk) in groups {
        let Some(repo) = repos.iter().find(|r| r.id == repo_id) else { continue };
        if let Err(e) = step(hub, repo, &trunk).await {
            tracing::warn!(repo = %repo.name, %trunk, error = %format!("{e:#}"), "the land queue couldn't move");
        }
    }
    Ok(())
}

/// One move of a trunk's queue: a tested batch lands or comes apart, else the next is built.
async fn step(hub: &Arc<Hub>, repo: &Repo, trunk: &str) -> anyhow::Result<()> {
    let Some(path) = super::hosting::native_path(hub, repo) else {
        for e in entries(hub, &repo.id, trunk, &["queued", "testing"]).await? {
            eject(hub, &e, "the repository isn't hosted here", "UNKNOWN_REMOVAL_REASON").await?;
        }
        return Ok(());
    };
    let path = path.to_string_lossy().into_owned();
    let (policy, _) = super::review::policy(hub, &path, trunk).await;
    let key = (repo.id.clone(), trunk.to_owned());
    let testing = entries(hub, &repo.id, trunk, &["testing"]).await?;
    if let Some(first) = testing.first() {
        let (Some(candidate), Some(base), Some(batch)) = (first.candidate.clone(), first.base.clone(), first.batch.clone()) else {
            requeue(hub, &testing).await?;
            return Ok(());
        };
        let checks = super::review::checks_on(hub, &repo.name, &candidate).await?;
        // Every workflow the candidate started, and every required check, must pass.
        let stem = |p: &str| p.rsplit('/').next().unwrap_or(p).trim_end_matches(".yml").trim_end_matches(".yaml").to_owned();
        let mut gates: Vec<String> = first.started.clone();
        gates.extend(policy.land.checks.iter().cloned());
        let mut failed: Vec<String> = Vec::new();
        let mut waiting = false;
        for g in &gates {
            match checks.get(g).or_else(|| checks.get(&stem(g))) {
                Some(Some(c)) if land::passed(c) => {}
                Some(Some(c)) => failed.push(format!("`{g}` {c}")),
                _ => waiting = true,
            }
        }
        if !failed.is_empty() {
            let reason = format!("{} on the candidate", failed.join(", "));
            let _ = gitops::run_git(hub, &["-C", &path, "update-ref", "-d", &format!("refs/kiln/land/{trunk}/{batch}")], LS_REMOTE_TIMEOUT).await;
            if testing.len() > 1 {
                // Bisect: the next batch is half as big.
                *failures().lock().entry(key).or_insert(0) += 1;
                requeue(hub, &testing).await?;
                tracing::info!(repo = %repo.name, %trunk, %reason, "a batch failed: testing it in halves");
            } else {
                // The culprit, alone: out, and the rest tested whole again.
                eject(hub, first, &reason, "CI_FAILURE").await?;
                failures().lock().remove(&key);
                kick();
            }
            return Ok(());
        }
        if waiting {
            return Ok(());
        }
        // Everything passed: the trunk moves, if it's where the candidate was built.
        let moved = gitops::run_git(hub, &["-C", &path, "update-ref", "-m", "kiln land", &format!("refs/heads/{trunk}"), &candidate, &base], LS_REMOTE_TIMEOUT).await;
        let _ = gitops::run_git(hub, &["-C", &path, "update-ref", "-d", &format!("refs/kiln/land/{trunk}/{batch}")], LS_REMOTE_TIMEOUT).await;
        if moved.is_err() {
            tracing::info!(repo = %repo.name, %trunk, "the trunk moved while its candidate was tested: building another");
            requeue(hub, &testing).await?;
            return Ok(());
        }
        failures().lock().remove(&key);
        landed(hub, repo, &path, trunk, &testing, &candidate).await?;
        return Ok(());
    }

    // Nothing under test: build the next batch.
    let queued = entries(hub, &repo.id, trunk, &["queued"]).await?;
    if queued.is_empty() {
        return Ok(());
    }
    let n = land::next_batch(policy.queue.batch, failures().lock().get(&key).copied().unwrap_or(0));
    let base = gitops::run_git(hub, &["-C", &path, "rev-parse", "--verify", &format!("refs/heads/{trunk}^{{commit}}")], LS_REMOTE_TIMEOUT).await.map(|o| String::from_utf8_lossy(&o).trim().to_owned())?;
    let mut tip = base.clone();
    let mut built: Vec<(Entry, Applied)> = Vec::new();
    for e in queued.into_iter().take(usize::try_from(n).unwrap_or(1)) {
        match apply(hub, &path, &tip, &e).await {
            Ok(a) => {
                tip = a.tip.clone();
                built.push((e, a));
            }
            Err(Refusal::Conflict(files)) => {
                let shown: Vec<String> = files.iter().take(5).cloned().collect();
                eject(hub, &e, &format!("needs rebase: conflicts in {}{}", shown.join(", "), if files.len() > 5 { ", …" } else { "" }), "INVALID_MERGE_COMMIT").await?;
            }
            Err(Refusal::Other(why)) => eject(hub, &e, &why, "UNKNOWN_REMOVAL_REASON").await?,
        }
    }
    if built.is_empty() {
        kick();
        return Ok(());
    }
    let batch = crate::util::new_id();
    gitops::run_git(hub, &["-C", &path, "update-ref", &format!("refs/kiln/land/{trunk}/{batch}"), &tip], LS_REMOTE_TIMEOUT).await?;
    let mut names = Vec::new();
    for (e, _) in &built {
        for id in &e.diffs {
            if let Some(d) = diffs::find(hub, id).await? {
                names.push(d.name);
            }
        }
    }
    let ctx = super::events::facts(hub, repo).await.repo;
    let head = commit_of(hub, &path, &tip).await;
    let events = candidate_events(&ctx, repo, trunk, &base, &head, &batch, &names);
    let started = start_checks(hub, repo, &events).await.unwrap_or_else(|e| {
        tracing::warn!(repo = %repo.name, error = %format!("{e:#}"), "the candidate's workflows didn't start");
        Vec::new()
    });
    let missing = uncovered(&policy.land.checks, &started, &BTreeMap::new());
    if !missing.is_empty() {
        let _ = gitops::run_git(hub, &["-C", &path, "update-ref", "-d", &format!("refs/kiln/land/{trunk}/{batch}")], LS_REMOTE_TIMEOUT).await;
        for (e, _) in &built {
            eject(hub, e, &format!("{} must pass on the candidate, and no workflow runs it there: add `on: merge_group` (or kiln's `on: land`)", missing.iter().map(|m| format!("`{m}`")).collect::<Vec<_>>().join(", ")), "BRANCH_PROTECTIONS").await?;
        }
        return Ok(());
    }
    let started_json = serde_json::to_string(&started).unwrap_or_default();
    for (e, a) in &built {
        let (id, b, base2, cand, st, commits) = (e.id.clone(), batch.clone(), base.clone(), tip.clone(), started_json.clone(), serde_json::to_string(&a.commits).unwrap_or_default());
        hub.store
            .call(move |c| {
                c.execute("UPDATE land_queue SET state = 'testing', batch = ?2, base = ?3, candidate = ?4, checks = ?5, commits = ?6, updated_at = ?7 WHERE id = ?1", params![id, b, base2, cand, st, commits, crate::util::now_unix()])?;
                Ok(())
            })
            .await?;
    }
    tracing::info!(repo = %repo.name, %trunk, candidate = %tip, diffs = %names.join(" "), checks = started.len(), "a land candidate is under test");
    for (e, _) in &built {
        tell(hub, e, "testing", None).await;
    }
    // Nothing to wait for: the next step lands it.
    if started.is_empty() {
        kick();
    }
    Ok(())
}

/// A batch back in the queue, to be built again.
async fn requeue(hub: &Hub, list: &[Entry]) -> anyhow::Result<()> {
    let ids: Vec<String> = list.iter().map(|e| e.id.clone()).collect();
    hub.store
        .call(move |c| {
            for id in &ids {
                c.execute("UPDATE land_queue SET state = 'queued', batch = NULL, base = NULL, candidate = NULL, checks = NULL, commits = NULL, updated_at = ?2 WHERE id = ?1", params![id, crate::util::now_unix()])?;
            }
            Ok(())
        })
        .await?;
    kick();
    Ok(())
}

/// The trunk is at `tip`: the batch's entries and diffs landed; the open diffs above them restacked.
async fn landed(hub: &Arc<Hub>, repo: &Repo, path: &str, trunk: &str, batch: &[Entry], tip: &str) -> anyhow::Result<()> {
    let mut all = Vec::new();
    // Each diff's closing event is sent by whoever asked it to land.
    let mut landed_by: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for e in batch {
        let (id, commits) = (e.id.clone(), e.commits.clone());
        hub.store
            .call(move |c| {
                let now = crate::util::now_unix();
                c.execute("UPDATE land_queue SET state = 'landed', updated_at = ?2 WHERE id = ?1", params![id, now])?;
                for (diff, commit) in &commits {
                    c.execute("UPDATE diffs SET state = 'landed', landed_commit = ?2, landed_at = ?3, updated_at = ?3 WHERE id = ?1", params![diff, commit, now])?;
                }
                Ok(())
            })
            .await?;
        all.extend(e.diffs.iter().cloned());
        landed_by.extend(e.diffs.iter().map(|d| (d.clone(), e.enqueued_by.clone())));
        hub.store.audit("kiln".into(), "queue.land".into(), format!("{} {trunk} at {tip}: entry {} by {}", repo.name, e.id, e.enqueued_by)).await;
        tell(hub, e, "landed", None).await;
        queue_event(hub, &e.diffs, "dequeued", Some("MERGE"), &e.enqueued_by).await;
    }
    tracing::info!(repo = %repo.name, %trunk, %tip, diffs = all.len(), "landed");
    // For whoever follows lands (deploy's releases of origin `git`, docs/deploy-api.md).
    let mut names = Vec::new();
    for id in &all {
        if let Ok(Some(d)) = diffs::find(hub, id).await {
            names.push(d.name);
        }
    }
    let by: Vec<&str> = batch.iter().map(|e| e.enqueued_by.as_str()).collect();
    hub.emit(&json!({ "type": "landed", "repo": repo.id, "repo_name": repo.name, "trunk": trunk, "tip": tip, "diffs": names, "by": by }));
    super::hosting::mirror_trunk(hub, repo, path, trunk);
    // The trunk's push runs (the poller sees it now).
    hub.gitops_kick.notify();
    for id in &all {
        if let Ok(Some(d)) = diffs::find(hub, id).await
            && let Err(e) = diffs::closed(hub, repo, &d, true, landed_by.get(id).map_or("kiln", String::as_str)).await
        {
            tracing::warn!(diff = %d.name, error = %format!("{e:#}"), "a landed diff's closing event didn't start");
        }
    }
    restack(hub, repo, path, trunk, &all, tip).await;
    Ok(())
}

/// The open diffs standing on `landed` ones, onto `tip` as rebase-only versions, each on the one
/// below it; a chain that conflicts stays as it was (its author syncs it).
async fn restack(hub: &Arc<Hub>, repo: &Repo, path: &str, trunk: &str, landed: &[String], tip: &str) {
    let children = |parents: Vec<String>| {
        let hub = hub.clone();
        async move {
            hub.store
                .call(move |c| {
                    let mut out = Vec::new();
                    let mut st = c.prepare("SELECT id FROM diffs WHERE parent_diff = ?1 AND state NOT IN ('landed', 'abandoned', 'landing')")?;
                    for p in &parents {
                        out.extend(st.query_map([p], |r| r.get::<_, String>(0))?.filter_map(Result::ok));
                    }
                    Ok(out)
                })
                .await
                .unwrap_or_default()
        }
    };
    // (diff, the commit it goes on, the diff below it now).
    let mut work: Vec<(String, String, Option<String>)> = children(landed.to_vec()).await.into_iter().map(|id| (id, tip.to_owned(), None)).collect();
    let mut seen = 0usize;
    while let Some((id, onto, below)) = work.pop() {
        seen += 1;
        if seen > 500 {
            break;
        }
        let Ok(Some(d)) = diffs::find(hub, &id).await else { continue };
        let Some(v) = diffs::versions(hub, &d).await.ok().and_then(|mut v| v.pop()) else { continue };
        let tree = match pick_tree(hub, path, &onto, &v.commit, &v.parent).await {
            Ok(t) => t,
            Err(_) => {
                tracing::info!(diff = %d.name, "a diff above a landed one doesn't restack cleanly; its author syncs it");
                continue;
            }
        };
        let Ok(who) = author(hub, path, &v.commit).await else { continue };
        let message = gitops::run_git(hub, &["-C", path, "log", "-1", "--format=%B", &v.commit], LS_REMOTE_TIMEOUT).await.map(|o| String::from_utf8_lossy(&o).into_owned()).unwrap_or_default();
        let Ok(c) = commit_tree(hub, path, &tree, &[&onto], &who, &message).await else { continue };
        let patch = diffs::git_run(hub, &["-C", path, "show", "--no-color", &c], &[], Vec::new()).await.ok().map(|r| r.out).unwrap_or_default();
        let patch_id = diffs::git_with_input(hub, &["-C", path, "patch-id", "--stable"], patch).await.ok().and_then(|o| String::from_utf8_lossy(&o).split_whitespace().next().map(str::to_owned)).unwrap_or_default();
        let nv = diffs::NewVersion { commit: c.clone(), parent: onto.clone(), base: tip.to_owned(), patch_id, message, note: Some(format!("restacked onto {trunk} after a land")), rebase_only: true, pushed_by: "kiln".into(), parent_diff: below.clone() };
        match diffs::add_version(hub, repo, path, &d, nv).await {
            Ok(n) => tracing::info!(diff = %d.name, version = n, "restacked onto the trunk"),
            Err(e) => {
                tracing::warn!(diff = %d.name, error = %format!("{e:#}"), "a restack's version couldn't be kept");
                continue;
            }
        }
        for child in children(vec![d.id.clone()]).await {
            work.push((child, c.clone(), Some(d.id.clone())));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w14_r08_uncovered() {
        let started = vec![".github/workflows/ci.yml".to_owned()];
        let none = BTreeMap::new();
        assert!(uncovered(&["ci".into(), "ci/test".into(), ".github/workflows/ci.yml".into()], &started, &none).is_empty());
        assert_eq!(uncovered(&["lint".into(), "ci".into()], &started, &none), ["lint"]);
    }
}
