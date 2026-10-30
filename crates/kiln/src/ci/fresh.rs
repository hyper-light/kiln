//! Keeping open changes fresh as their trunk moves (docs/ci.md W26-R16). A trunk's move (a push the
//! poller sees, or the land queue's own) is queued on the hub ([`moved`]); [`run`] takes what's
//! queued, a burst of moves as one (`kiln_ci::fresh::collapse`), and replays each open change of
//! the trunk onto its new tip as `kiln_ci::fresh::plan` says: stack by stack, bottom first, in
//! kiln's own copy, no branch moving. A replay is `git merge-tree --write-tree` (a diff as a
//! cherry-pick of its commit onto where it goes, a change request as its branch merged with the
//! trunk); a clean one is committed through `revision::commit` as kiln (its author and message
//! kept; the path W27-R03 signs, so the signature comes with it: unsigned until then) and kept as a
//! rebase-only revision, why `replayed`, so approvals stay. A round's revisions go in at once, their
//! refs (`refs/kiln/diffs/<D>/v<n>`) moved in one transaction. A conflicting replay is kept as
//! W26-R12's conflicts are (`conflicts::current`), for the conflict ladder (W26-R17), and the
//! changes above it wait on it. Nothing is pushed here: Update and automatic pushes are W26-R16's
//! next part.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use futures::FutureExt as _;
use kiln_ci::fresh::{self as plan, Change, Kind, Move, Onto};
use kiln_ci::review::State;

use super::conflicts;
use super::diffs::{self, Diff, NewVersion, Version};
use super::revision;
use crate::gitops::{self, LS_REMOTE_TIMEOUT};
use crate::hub::Hub;
use crate::rt::Notify;
use crate::store::Repo;

/// Trunk moves waiting for [`run`]: each repository's, in the order they came.
#[derive(Default)]
pub struct Queue {
    moves: parking_lot::Mutex<Vec<(String, Move)>>,
    kick: Notify,
}

/// Trunk `trunk` of `repo` moved from `before` to `after`: its open changes are replayed onto it.
pub fn moved(hub: &Hub, repo: &Repo, trunk: &str, before: &str, after: &str) {
    hub.fresh.moves.lock().push((repo.id.clone(), Move { trunk: trunk.to_owned(), before: before.to_owned(), after: after.to_owned() }));
    hub.fresh.kick.notify();
}

/// Replays as trunks move, until the server stops.
pub async fn run(hub: Arc<Hub>) -> anyhow::Result<()> {
    loop {
        let queued = std::mem::take(&mut *hub.fresh.moves.lock());
        let mut by_repo: BTreeMap<String, Vec<Move>> = BTreeMap::new();
        for (repo, m) in queued {
            by_repo.entry(repo).or_default().push(m);
        }
        for (repo_id, moves) in by_repo {
            let Some(repo) = hub.store.repos().await.ok().and_then(|rs| rs.into_iter().find(|r| r.id == repo_id)) else { continue };
            for m in plan::collapse(&moves) {
                let done = round(&hub, &repo, &m.trunk, &m.after).await;
                match done {
                    Ok(r) => tracing::info!(repo = %repo.name, trunk = %m.trunk, tip = %m.after, replayed = r.replayed, conflicted = r.conflicted, waiting = r.waiting, "open changes replayed onto the trunk"),
                    Err(e) => tracing::warn!(repo = %repo.name, trunk = %m.trunk, error = %format!("{e:#}"), "open changes couldn't be replayed onto the trunk"),
                }
            }
        }
        futures::select_biased! {
            () = hub.shutdown.cancelled().fuse() => return Ok(()),
            () = hub.fresh.kick.notified().fuse() => {}
        }
    }
}

/// What a round did.
#[derive(Debug, Default, Clone, Copy)]
pub struct Round {
    pub replayed: usize,
    pub conflicted: usize,
    pub waiting: usize,
}

/// One open diff or change request, as a round reads it.
struct Open {
    diff: Diff,
    latest: Version,
}

/// The open changes of `trunk` in `repo`, each with its latest revision, by number; and the
/// open one below each.
async fn open_changes(hub: &Hub, repo: &Repo, trunk: &str) -> anyhow::Result<Vec<(Open, Option<String>)>> {
    let (rid, t) = (repo.id.clone(), trunk.to_owned());
    let rows: Vec<(String, Option<String>, String)> = hub
        .store
        .call(move |c| {
            let mut st = c.prepare("SELECT id, parent_diff, state FROM diffs WHERE repo_id = ?1 AND trunk = ?2 ORDER BY number")?;
            let rows = st.query_map([&rid, &t], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.filter_map(Result::ok).collect();
            Ok(rows)
        })
        .await?;
    let open: BTreeSet<&str> = rows.iter().filter(|(_, _, s)| State::parse(s).is_some_and(|s| s.is_open() && s != State::Landing)).map(|(id, _, _)| id.as_str()).collect();
    let mut out = Vec::new();
    for (id, below, _) in rows.iter().filter(|(id, _, _)| open.contains(id.as_str())) {
        let Some(diff) = diffs::find(hub, id).await? else { continue };
        let Some(latest) = diffs::versions(hub, &diff).await?.pop() else { continue };
        let below = below.clone().filter(|b| open.contains(b.as_str()));
        out.push((Open { diff, latest }, below));
    }
    Ok(out)
}

/// Whether `ancestor` is `commit` or comes before it.
async fn contains(hub: &Hub, path: &str, ancestor: &str, commit: &str) -> bool {
    ancestor == commit || gitops::run_git(hub, &["-C", path, "merge-base", "--is-ancestor", ancestor, commit], LS_REMOTE_TIMEOUT).await.is_ok()
}

/// Replays `repo`'s open changes of `trunk` onto `tip`, as the planner says: a repository kiln
/// hosts, its diffs and change requests; one it mirrors, its host's pull requests ([`pulls`]).
pub async fn round(hub: &Arc<Hub>, repo: &Repo, trunk: &str, tip: &str) -> anyhow::Result<Round> {
    let Some(path) = super::hosting::native_path(hub, repo) else { return pulls(hub, repo, trunk, tip).await };
    let path = path.to_string_lossy().into_owned();
    // `.kiln/review.yaml` as the trunk has it now: a repository can keep its changes where their
    // authors put them.
    let (policy, _) = super::review::policy(hub, &path, trunk).await;
    if !policy.fresh.replay {
        return Ok(Round::default());
    }
    let found = open_changes(hub, repo, trunk).await?;
    if found.is_empty() {
        return Ok(Round::default());
    }
    let mut changes = Vec::with_capacity(found.len());
    for (o, below) in &found {
        let on = if contains(hub, &path, tip, &o.latest.commit).await { tip.to_owned() } else { o.latest.base.clone() };
        let kind = if o.diff.kind == "change_request" { Kind::Request } else { Kind::Diff };
        changes.push(Change { id: o.diff.id.clone(), kind, trunk: trunk.to_owned(), below: below.clone(), on });
    }
    let tips = BTreeMap::from([(trunk.to_owned(), tip.to_owned())]);
    let p = plan::plan(&tips, &changes);
    for (id, why) in &p.refused {
        tracing::warn!(repo = %repo.name, diff = %id, %why, "a change isn't replayed");
    }
    let by_id: HashMap<&str, &Open> = found.iter().map(|(o, _)| (o.diff.id.as_str(), o)).collect();
    let tip_name = format!("{trunk}@{}", tip.get(..7).unwrap_or(tip));
    // Each change replayed cleanly this round: its new commit, for the changes above it.
    let mut replayed: HashMap<String, String> = HashMap::new();
    let mut made: Vec<(Diff, NewVersion)> = Vec::new();
    let mut conflicted: BTreeSet<String> = BTreeSet::new();
    let mut waiting = 0usize;
    for r in &p.replays {
        let Some(o) = by_id.get(r.id.as_str()) else { continue };
        let (onto, below) = match &r.onto {
            Onto::Tip(t) => (t.clone(), None),
            Onto::Below(b) => match replayed.get(b) {
                Some(c) => (c.clone(), Some(b.clone())),
                // What's below it conflicted or couldn't be replayed: it waits.
                None => {
                    waiting += 1;
                    continue;
                }
            },
        };
        let request = o.diff.kind == "change_request";
        let (clean, merged) = match conflicts::merge_tree(hub, &path, &onto, &o.latest, request).await {
            Ok(x) => x,
            Err((_, why)) => {
                tracing::warn!(repo = %repo.name, diff = %o.diff.name, %why, "a change's replay didn't run");
                waiting += 1;
                continue;
            }
        };
        if !clean {
            conflicted.insert(o.diff.id.clone());
            continue;
        }
        match commit(hub, &path, o, &onto, &merged.tree, &tip_name).await {
            Ok((sha, message, patch_id)) => {
                replayed.insert(o.diff.id.clone(), sha.clone());
                let note = Some(format!("replayed onto {tip_name}"));
                made.push((o.diff.clone(), NewVersion { commit: sha, parent: onto, base: tip.to_owned(), patch_id, message, note, rebase_only: true, pushed_by: "kiln".into(), parent_diff: below }));
            }
            Err(e) => {
                tracing::warn!(repo = %repo.name, diff = %o.diff.name, error = %e, "a change's replay couldn't be committed");
                waiting += 1;
            }
        }
    }
    // A replay starts no runs: the patch is the one tested, its checks carry as its approvals do, and
    // the land queue's candidate tests it on the trunk as it lands.
    let numbers = diffs::add_versions(hub, repo, &path, made.clone(), false).await?;
    for ((d, v), n) in made.iter().zip(&numbers) {
        // Why the revision exists (W26-R04).
        revision::record(hub, &d.id, *n, &v.commit, &revision::Why { why: plan::WHY, thread: None, comment: None, note: None }).await?;
    }
    // A conflicting replay is kept where it falls, for the ladder; the ones above it wait.
    for id in &conflicted {
        if let Some(o) = by_id.get(id.as_str())
            && let Err((_, why)) = conflicts::current(hub, &o.diff).await
        {
            tracing::warn!(repo = %repo.name, diff = %o.diff.name, %why, "a conflicting replay couldn't be kept");
        }
    }
    if !made.is_empty() || !conflicted.is_empty() {
        hub.store.audit("kiln".into(), "review.fresh".into(), format!("{} {tip_name}: {} replayed, {} conflicted, {waiting} waiting", repo.name, made.len(), conflicted.len())).await;
        super::land::kick();
    }
    Ok(Round { replayed: made.len(), conflicted: conflicted.len(), waiting })
}

/// The commit of a clean replay of `o` onto `onto` (tree `tree`), made by kiln through
/// `revision::commit`: a diff's commit with its author, date and message kept (its `Kiln-Change`
/// line with it); a change request's branch merged with where it goes. Its SHA, message and
/// patch-id.
async fn commit(hub: &Hub, path: &str, o: &Open, onto: &str, tree: &str, tip_name: &str) -> Result<(String, String, String), String> {
    let v = &o.latest;
    let made = match &o.diff.branch {
        Some(branch) if o.diff.kind == "change_request" => {
            let message = format!("Merge {tip_name} into {branch}\n\nReplayed by kiln as {} moved.\n", o.diff.trunk);
            revision::commit(hub, path, &revision::Commit { tree, parents: &[v.commit.as_str(), onto], author: ("kiln", None), by: "kiln", co_authors: &[], message: &message }).await?
        }
        _ => {
            let message = gitops::run_git(hub, &["-C", path, "log", "-1", "--format=%B", &v.commit], LS_REMOTE_TIMEOUT).await.map(|o| String::from_utf8_lossy(&o).into_owned()).map_err(|e| format!("{e:#}"))?;
            let message = kiln_ci::identity::with_kiln_changes(&message, &o.diff.change.iter().cloned().collect::<Vec<_>>());
            let who = gitops::run_git(hub, &["-C", path, "log", "-1", "--format=%an <%ae>%x00%ad", "--date=raw", &v.commit], LS_REMOTE_TIMEOUT).await.map(|o| String::from_utf8_lossy(&o).trim().to_owned()).map_err(|e| format!("{e:#}"))?;
            let (author, date) = who.split_once('\0').unwrap_or((&who, ""));
            revision::commit(hub, path, &revision::Commit { tree, parents: &[onto], author: (author, Some(date)), by: "kiln", co_authors: &[], message: &message }).await?
        }
    };
    let patch = diffs::git_run(hub, &["-C", path, "show", "--no-color", &made.sha], &[], Vec::new()).await.ok().map(|r| r.out).unwrap_or_default();
    let patch_id = diffs::git_with_input(hub, &["-C", path, "patch-id", "--stable"], patch).await.ok().and_then(|o| String::from_utf8_lossy(&o).split_whitespace().next().map(str::to_owned)).unwrap_or_default();
    Ok((made.sha, made.message, patch_id))
}

/// An open pull request of a host, as a round reads it: its host, number, latest head (its `n`
/// among the heads kiln has seen, and that commit).
struct Pull {
    host: String,
    number: i64,
    n: i64,
    head: String,
}

/// The open pull requests on `trunk` of `repo` (mirrored from its host, W26-R20), each at its
/// latest head.
async fn open_pulls(hub: &Hub, repo: &Repo, trunk: &str) -> anyhow::Result<Vec<Pull>> {
    let (rid, t) = (repo.id.clone(), trunk.to_owned());
    hub.store
        .call(move |c| {
            let mut st = c.prepare(
                "SELECT p.host, p.number, h.n, h.sha FROM host_pulls p JOIN host_pull_heads h ON h.repo_id = p.repo_id AND h.host = p.host AND h.number = p.number
                  WHERE p.repo_id = ?1 AND p.base = ?2 AND p.state IN ('open', 'draft')
                    AND h.n = (SELECT MAX(x.n) FROM host_pull_heads x WHERE x.repo_id = p.repo_id AND x.host = p.host AND x.number = p.number)
                  ORDER BY p.host, p.number",
            )?;
            let rows = st.query_map([&rid, &t], |r| Ok(Pull { host: r.get(0)?, number: r.get(1)?, n: r.get(2)?, head: r.get(3)? }))?.filter_map(Result::ok).collect();
            Ok(rows)
        })
        .await
}

/// A mirrored repository's pull requests on `trunk`, replayed onto `tip` in kiln's mirror, as a
/// change request's are (its base merged into its head, by `git merge-tree`); each replay kept
/// (`review_pull_replays`), a clean one's commit made by kiln through `revision::commit` and kept at
/// `refs/kiln/pulls/<host>/<n>/replays/<k>` for Update to push. Nothing moves on the host.
async fn pulls(hub: &Arc<Hub>, repo: &Repo, trunk: &str, tip: &str) -> anyhow::Result<Round> {
    let found = open_pulls(hub, repo, trunk).await?;
    if found.is_empty() {
        return Ok(Round::default());
    }
    let path = gitops::ensure_commit(hub, repo, tip).await?.to_string_lossy().into_owned();
    let (policy, _) = super::review::policy(hub, &path, trunk).await;
    if !policy.fresh.replay {
        return Ok(Round::default());
    }
    let tip_name = format!("{trunk}@{}", tip.get(..7).unwrap_or(tip));
    let mut out = Round::default();
    let mut refs = String::from("start\n");
    for p in &found {
        if gitops::ensure_commit(hub, repo, &p.head).await.is_err() {
            tracing::warn!(repo = %repo.name, host = %p.host, number = p.number, head = %p.head, "a pull request's head isn't in the mirror: it isn't replayed");
            out.waiting += 1;
            continue;
        }
        if contains(hub, &path, tip, &p.head).await || replayed_pull(hub, repo, p, tip).await? {
            continue;
        }
        let v = Version { n: 0, commit: p.head.clone(), parent: String::new(), base: String::new(), rebase_only: false, pushed_by: String::new(), note: None, created_at: 0, git_ref: String::new(), started_by: None, why: None, thread: None, checks_from: None };
        let (clean, merged) = match conflicts::merge_tree(hub, &path, tip, &v, true).await {
            Ok(x) => x,
            Err((_, why)) => {
                tracing::warn!(repo = %repo.name, host = %p.host, number = p.number, %why, "a pull request's replay didn't run");
                out.waiting += 1;
                continue;
            }
        };
        let commit = if clean {
            let message = format!("Merge {tip_name} into #{}\n\nReplayed by kiln as {trunk} moved.\n", p.number);
            match revision::commit(hub, &path, &revision::Commit { tree: &merged.tree, parents: &[p.head.as_str(), tip], author: ("kiln", None), by: "kiln", co_authors: &[], message: &message }).await {
                Ok(m) => Some(m.sha),
                Err(e) => {
                    tracing::warn!(repo = %repo.name, host = %p.host, number = p.number, error = %e, "a pull request's replay couldn't be committed");
                    out.waiting += 1;
                    continue;
                }
            }
        } else {
            None
        };
        let k = keep_pull_replay(hub, repo, p, tip, &tip_name, &merged.tree, commit.as_deref()).await?;
        if let Some(c) = &commit {
            refs.push_str(&format!("update refs/kiln/pulls/{}/{}/replays/{k} {c}\n", p.host, p.number));
            out.replayed += 1;
        } else {
            out.conflicted += 1;
        }
    }
    if out.replayed > 0 {
        refs.push_str("prepare\ncommit\n");
        diffs::git_with_input(hub, &["-C", &path, "update-ref", "--stdin"], refs.into_bytes()).await?;
    }
    if out.replayed + out.conflicted > 0 {
        hub.store.audit("kiln".into(), "review.fresh".into(), format!("{} {tip_name}: {} pull requests replayed, {} conflicted", repo.name, out.replayed, out.conflicted)).await;
    }
    Ok(out)
}

/// Whether pull request `p`'s head was replayed onto `tip` already.
async fn replayed_pull(hub: &Hub, repo: &Repo, p: &Pull, tip: &str) -> anyhow::Result<bool> {
    let (rid, host, number, head, onto) = (repo.id.clone(), p.host.clone(), p.number, p.head.clone(), tip.to_owned());
    hub.store
        .call(move |c| {
            let n: i64 = c.query_row(
                "SELECT COUNT(*) FROM review_pull_replays WHERE repo_id = ?1 AND host = ?2 AND number = ?3 AND head_sha = ?4 AND onto = ?5",
                rusqlite::params![rid, host, number, head, onto],
                |r| r.get(0),
            )?;
            Ok(n > 0)
        })
        .await
}

/// Keeps pull request `p`'s replay onto `tip`: its number among the pull request's replays.
async fn keep_pull_replay(hub: &Hub, repo: &Repo, p: &Pull, tip: &str, tip_name: &str, tree: &str, commit: Option<&str>) -> anyhow::Result<i64> {
    let (rid, host, number, n, head) = (repo.id.clone(), p.host.clone(), p.number, p.n, p.head.clone());
    let (onto, onto_name, tree, commit) = (tip.to_owned(), tip_name.to_owned(), tree.to_owned(), commit.map(str::to_owned));
    hub.store
        .call(move |c| {
            let tx = crate::store::tx(c)?;
            let k: i64 = tx.query_row("SELECT COALESCE(MAX(k), 0) + 1 FROM review_pull_replays WHERE repo_id = ?1 AND host = ?2 AND number = ?3", rusqlite::params![rid, host, number], |r| r.get(0))?;
            let state = if commit.is_some() { "clean" } else { "conflicted" };
            tx.execute(
                "INSERT OR IGNORE INTO review_pull_replays (id, repo_id, host, number, head_n, head_sha, onto, onto_name, tree, state, commit_sha, k, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                rusqlite::params![crate::util::new_id(), rid, host, number, n, head, onto, onto_name, tree, state, commit, k, crate::util::now_unix()],
            )?;
            tx.commit()?;
            Ok(k)
        })
        .await
}
