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
    /// Repositories (by id) and trunks whose open changes changed: the collision radar looks again.
    changed: parking_lot::Mutex<BTreeSet<(String, String)>>,
    kick: Notify,
}

/// An open change of `trunk` in `repo` has a new revision: the collision radar looks again.
pub fn changed(hub: &Hub, repo: &Repo, trunk: &str) {
    hub.fresh.changed.lock().insert((repo.id.clone(), trunk.to_owned()));
    hub.fresh.kick.notify();
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
        let mut looks = std::mem::take(&mut *hub.fresh.changed.lock());
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
                looks.insert((repo.id.clone(), m.trunk.clone()));
            }
        }
        for (repo_id, trunk) in looks {
            let Some(repo) = hub.store.repos().await.ok().and_then(|rs| rs.into_iter().find(|r| r.id == repo_id)) else { continue };
            if let Err(e) = radar(&hub, &repo, &trunk).await {
                tracing::warn!(repo = %repo.name, %trunk, error = %format!("{e:#}"), "the collision radar didn't look");
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
    /// Its latest revision: what the changes above it are on.
    latest: Version,
    /// The revision a replay starts from: its latest, or the one a kept conflicted revision is a
    /// replay of (its markers are never replayed again).
    source: Version,
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
        let all = diffs::versions(hub, &diff).await?;
        let Some(latest) = all.last().cloned() else { continue };
        let source = conflicts::source(hub, &diff, &all).await.map_err(|(_, why)| anyhow::anyhow!(why))?.unwrap_or_else(|| latest.clone());
        let below = below.clone().filter(|b| open.contains(b.as_str()));
        out.push((Open { diff, latest, source }, below));
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
    let latest_of: HashMap<&str, &str> = found.iter().map(|(o, _)| (o.diff.id.as_str(), o.latest.commit.as_str())).collect();
    let mut changes = Vec::with_capacity(found.len());
    for (o, below) in &found {
        let on = if contains(hub, &path, tip, &o.latest.commit).await { tip.to_owned() } else { o.latest.base.clone() };
        let kind = if o.diff.kind == "change_request" { Kind::Request } else { Kind::Diff };
        let on_below = below.as_deref().and_then(|b| latest_of.get(b)).is_none_or(|b| *b == o.latest.parent);
        changes.push(Change { id: o.diff.id.clone(), kind, trunk: trunk.to_owned(), below: below.clone(), on, on_below });
    }
    let tips = BTreeMap::from([(trunk.to_owned(), tip.to_owned())]);
    let p = plan::plan(&tips, &changes);
    for (id, why) in &p.refused {
        tracing::warn!(repo = %repo.name, diff = %id, %why, "a change isn't replayed");
    }
    let by_id: HashMap<&str, &Open> = found.iter().map(|(o, _)| (o.diff.id.as_str(), o)).collect();
    let tip_name = format!("{trunk}@{}", tip.get(..7).unwrap_or(tip));
    // Each change replayed this round, cleanly or kept with its conflicts: its new commit, for the
    // changes above it.
    let mut replayed: HashMap<String, String> = HashMap::new();
    let mut made: Vec<(Diff, NewVersion)> = Vec::new();
    let mut conflicted: BTreeSet<String> = BTreeSet::new();
    let mut waiting = 0usize;
    for r in &p.replays {
        let Some(o) = by_id.get(r.id.as_str()) else { continue };
        let (onto, onto_name, below) = match &r.onto {
            Onto::Tip(t) => (t.clone(), trunk.to_owned(), None),
            Onto::Below(b) => match replayed.get(b).map(String::as_str).or_else(|| latest_of.get(b.as_str()).copied()) {
                Some(c) => (c.to_owned(), by_id.get(b.as_str()).map(|x| format!("{} v{}", x.diff.name, x.latest.n)).unwrap_or_else(|| b.clone()), Some(b.clone())),
                // What's below it couldn't be replayed: it waits for the next round.
                None => {
                    waiting += 1;
                    continue;
                }
            },
        };
        let request = o.diff.kind == "change_request";
        // W29-R25: a lock file is never a person's conflict; both locks follow the source tree.
        let (clean, merged) = match conflicts::replay_tree(hub, &path, &onto, &o.source, request).await {
            Ok(x) => x,
            Err((_, why)) => {
                tracing::warn!(repo = %repo.name, diff = %o.diff.name, %why, "a change's replay didn't run");
                waiting += 1;
                continue;
            }
        };
        // Clean or not, it's a revision: a conflicted one is git's merge with its markers in, kept
        // so the changes above it still move (as jj's do), its conflicts holding its gate.
        match commit(hub, &path, o, &onto, &merged.tree, &tip_name, clean).await {
            Ok((sha, message, patch_id)) => {
                replayed.insert(o.diff.id.clone(), sha.clone());
                let note = Some(if clean { format!("replayed onto {tip_name}") } else { format!("replayed onto {tip_name} with conflicts") });
                // A conflicting replay is kept where its conflicts fall first (W26-R12's records),
                // so its revision, going in with its name on it, never shows without them.
                let keeps = if clean {
                    None
                } else {
                    conflicted.insert(o.diff.id.clone());
                    match conflicts::keep(hub, &path, &o.diff, &o.source, (&onto, &onto_name, tip, below.clone()), &merged).await {
                        Ok(r) => Some(r.id),
                        Err((_, why)) => {
                            tracing::warn!(repo = %repo.name, diff = %o.diff.name, %why, "a conflicting replay couldn't be kept");
                            replayed.remove(&o.diff.id);
                            waiting += 1;
                            continue;
                        }
                    }
                };
                let why = Some(if clean { plan::WHY } else { plan::WHY_CONFLICTED }.to_owned());
                // Whether its diff is the replayed revision's exactly (context and whitespace too):
                // a diff's, compared by verbatim patch-id; a change request's merge has no one patch.
                let same_patch = match (clean, o.diff.kind.as_str()) {
                    (true, "diff") => {
                        let (now, was) = (verbatim(hub, &path, &sha).await, verbatim(hub, &path, &o.source.commit).await);
                        Some(now.is_some() && now == was)
                    }
                    _ => None,
                };
                made.push((o.diff.clone(), NewVersion { commit: sha, parent: onto, base: tip.to_owned(), patch_id, message, note, rebase_only: clean, pushed_by: "kiln".into(), parent_diff: below, why, keeps, same_patch }));
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
        let clean = !conflicted.contains(&d.id);
        // Pushed now only where the repository turned automatic pushes on (`fresh.push: auto`).
        if clean && plan::then(policy.fresh, plan::Outcome::Clean, d.branch.is_some() && d.kind == "change_request").push {
            match move_branch(hub, repo, &path, d, &v.commit).await {
                Ok(branch) => hub.store.audit("kiln".into(), "review.fresh.push".into(), format!("{} {} v{n}: {branch} moved to its replay, automatically (.kiln/review.yaml's fresh.push: auto)", repo.name, d.name)).await,
                Err(why) => tracing::info!(repo = %repo.name, diff = %d.name, %why, "a replay wasn't pushed automatically"),
            }
        }
    }
    let clean = made.len().saturating_sub(conflicted.len());
    if !made.is_empty() {
        hub.store.audit("kiln".into(), "review.fresh".into(), format!("{} {tip_name}: {clean} replayed clean, {} kept with conflicts, {waiting} waiting", repo.name, conflicted.len())).await;
        super::land::kick();
    }
    Ok(Round { replayed: clean, conflicted: conflicted.len(), waiting })
}

/// The commit of a replay of `o` onto `onto` (tree `tree`: a conflicted one's has git's markers in),
/// made by kiln through `revision::commit`: a diff's commit with its author, date and message kept
/// (its `Kiln-Change` line with it); a change request's branch merged with where it goes. Its SHA,
/// message and patch-id.
async fn commit(hub: &Hub, path: &str, o: &Open, onto: &str, tree: &str, tip_name: &str, clean: bool) -> Result<(String, String, String), String> {
    let v = &o.source;
    let made = match &o.diff.branch {
        Some(branch) if o.diff.kind == "change_request" => {
            let with = if clean { "" } else { ", with conflicts to resolve" };
            let message = format!("Merge {tip_name} into {branch}\n\nReplayed by kiln as {} moved{with}.\n", o.diff.trunk);
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
/// An open pull request of a host, as a round reads it: its host, number, latest head (its `n`
/// among the heads kiln has seen, and that commit), the branch it goes onto and its own branch.
struct Pull {
    host: String,
    number: i64,
    n: i64,
    head: String,
    base: String,
    head_ref: String,
}

impl Pull {
    fn id(&self) -> String {
        format!("pull:{}:{}", self.host, self.number)
    }
}

/// The open pull requests of `repo` (mirrored from its host, W26-R20), each at its latest head.
async fn open_pulls(hub: &Hub, repo: &Repo) -> anyhow::Result<Vec<Pull>> {
    let rid = repo.id.clone();
    hub.store
        .call(move |c| {
            let mut st = c.prepare(
                "SELECT p.host, p.number, h.n, h.sha, p.base, COALESCE(p.head_ref, '') FROM host_pulls p JOIN host_pull_heads h ON h.repo_id = p.repo_id AND h.host = p.host AND h.number = p.number
                  WHERE p.repo_id = ?1 AND p.state IN ('open', 'draft')
                    AND h.n = (SELECT MAX(x.n) FROM host_pull_heads x WHERE x.repo_id = p.repo_id AND x.host = p.host AND x.number = p.number)
                  ORDER BY p.host, p.number",
            )?;
            let rows = st.query_map([&rid], |r| Ok(Pull { host: r.get(0)?, number: r.get(1)?, n: r.get(2)?, head: r.get(3)?, base: r.get(4)?, head_ref: r.get(5)? }))?.filter_map(Result::ok).collect();
            Ok(rows)
        })
        .await
}

/// The pull requests of `trunk`'s stacks: those on `trunk`, and those on another's branch (a stack,
/// as GitHub's and GitLab's are), each with the one below it (by id), bottom ones first.
fn pull_stacks(all: Vec<Pull>, trunk: &str) -> Vec<(Pull, Option<String>)> {
    let by_branch: HashMap<(String, String), String> = all.iter().filter(|p| !p.head_ref.is_empty()).map(|p| ((p.host.clone(), p.head_ref.clone()), p.id())).collect();
    let mut on: Vec<(Pull, Option<String>)> = Vec::new();
    let mut ids: BTreeSet<String> = BTreeSet::new();
    let mut rest = all;
    // Those on the trunk, then each on one already taken, until none is left to take.
    loop {
        let before = on.len();
        let (take, keep): (Vec<Pull>, Vec<Pull>) = rest.into_iter().partition(|p| p.base == trunk || by_branch.get(&(p.host.clone(), p.base.clone())).is_some_and(|b| ids.contains(b)));
        for p in take {
            let below = (p.base != trunk).then(|| by_branch.get(&(p.host.clone(), p.base.clone())).cloned()).flatten();
            ids.insert(p.id());
            on.push((p, below));
        }
        rest = keep;
        if on.len() == before || rest.is_empty() || on.len() > 10_000 {
            break;
        }
    }
    on
}

/// A mirrored repository's pull requests of `trunk`, replayed in kiln's mirror as a change
/// request's are (the base merged into the head, by `git merge-tree`), stack by stack as
/// `kiln_ci::fresh::plan` says: one on the trunk onto its new tip, one on another's branch (a
/// stack's upper one) onto that one's replay. Each replay is committed by kiln through
/// `revision::commit` (a conflicting one with its markers in, so the ones above still move) and
/// kept (`review_pull_replays`, `refs/kiln/pulls/<host>/<n>/replays/<k>`) for Update to push.
/// Nothing moves on the host.
async fn pulls(hub: &Arc<Hub>, repo: &Repo, trunk: &str, tip: &str) -> anyhow::Result<Round> {
    let all = open_pulls(hub, repo).await?;
    let seen = all.len();
    let found = pull_stacks(all, trunk);
    tracing::info!(repo = %repo.name, %trunk, open = seen, on_trunk = found.len(), pulls = ?found.iter().map(|(p, b)| format!("{}<-{}", p.id(), b.as_deref().unwrap_or(trunk))).collect::<Vec<_>>(), "a host's open pull requests, as fresh sees them");
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
    let mut there: Vec<(Pull, Option<String>)> = Vec::new();
    for (p, below) in found {
        if gitops::ensure_commit(hub, repo, &p.head).await.is_err() {
            tracing::warn!(repo = %repo.name, host = %p.host, number = p.number, head = %p.head, "a pull request's head isn't in the mirror: it isn't replayed");
            out.waiting += 1;
            continue;
        }
        there.push((p, below));
    }
    let heads: HashMap<String, String> = there.iter().map(|(p, _)| (p.id(), p.head.clone())).collect();
    let mut changes = Vec::with_capacity(there.len());
    for (p, below) in &there {
        let on = if contains(hub, &path, tip, &p.head).await { tip.to_owned() } else { String::new() };
        let on_below = match below.as_ref().and_then(|b| heads.get(b)) {
            Some(b) => contains(hub, &path, b, &p.head).await,
            None => true,
        };
        changes.push(Change { id: p.id(), kind: Kind::Pull, trunk: trunk.to_owned(), below: below.clone(), on, on_below });
    }
    let planned = plan::plan(&BTreeMap::from([(trunk.to_owned(), tip.to_owned())]), &changes);
    let by_id: HashMap<String, &Pull> = there.iter().map(|(p, _)| (p.id(), p)).collect();
    let mut refs = String::from("start\n");
    // Each pull request replayed this round: its replay's commit, for the ones above it.
    let mut replayed: HashMap<String, String> = HashMap::new();
    let mut clean_pulls: Vec<i64> = Vec::new();
    for r in &planned.replays {
        let Some(p) = by_id.get(&r.id) else { continue };
        let (onto, onto_name) = match &r.onto {
            Onto::Tip(t) => (t.clone(), tip_name.clone()),
            Onto::Below(b) => match replayed.get(b).or_else(|| heads.get(b)) {
                Some(c) => (c.clone(), format!("#{}", b.rsplit(':').next().unwrap_or(b))),
                None => {
                    out.waiting += 1;
                    continue;
                }
            },
        };
        if replayed_pull(hub, repo, p, &onto).await? {
            continue;
        }
        let v = Version { n: 0, commit: p.head.clone(), parent: String::new(), base: String::new(), rebase_only: false, pushed_by: String::new(), note: None, created_at: 0, git_ref: String::new(), started_by: None, why: None, thread: None, checks_from: None, same_patch: None };
        // W29-R25: as a change's replay, its locks following its source tree.
        let (clean, merged) = match conflicts::replay_tree(hub, &path, &onto, &v, true).await {
            Ok(x) => x,
            Err((_, why)) => {
                tracing::warn!(repo = %repo.name, host = %p.host, number = p.number, %why, "a pull request's replay didn't run");
                out.waiting += 1;
                continue;
            }
        };
        let with = if clean { "" } else { ", with conflicts to resolve" };
        let message = format!("Merge {onto_name} into #{}\n\nReplayed by kiln as {trunk} moved{with}.\n", p.number);
        let commit = match revision::commit(hub, &path, &revision::Commit { tree: &merged.tree, parents: &[p.head.as_str(), onto.as_str()], author: ("kiln", None), by: "kiln", co_authors: &[], message: &message }).await {
            Ok(m) => m.sha,
            Err(e) => {
                tracing::warn!(repo = %repo.name, host = %p.host, number = p.number, error = %e, "a pull request's replay couldn't be committed");
                out.waiting += 1;
                continue;
            }
        };
        let k = keep_pull_replay(hub, repo, p, &onto, &onto_name, &merged.tree, &commit, clean).await?;
        refs.push_str(&format!("update refs/kiln/pulls/{}/{}/replays/{k} {commit}\n", p.host, p.number));
        replayed.insert(p.id(), commit);
        if clean {
            clean_pulls.push(p.number);
            out.replayed += 1;
        } else {
            out.conflicted += 1;
        }
    }
    if out.replayed + out.conflicted > 0 {
        refs.push_str("prepare\ncommit\n");
        diffs::git_with_input(hub, &["-C", &path, "update-ref", "--stdin"], refs.into_bytes()).await?;
        hub.store.audit("kiln".into(), "review.fresh".into(), format!("{} {tip_name}: {} pull requests replayed clean, {} with conflicts", repo.name, out.replayed, out.conflicted)).await;
    }
    // Pushed to the host now only where the repository turned automatic pushes on: bottom first.
    if policy.fresh.push == plan::Push::Auto && !clean_pulls.is_empty() {
        let (hub2, repo2) = (Arc::clone(hub), repo.clone());
        crate::rt::spawn(async move { auto_push_pulls(&hub2, &repo2, &clean_pulls).await });
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
#[allow(clippy::too_many_arguments)]
async fn keep_pull_replay(hub: &Hub, repo: &Repo, p: &Pull, onto: &str, onto_name: &str, tree: &str, commit: &str, clean: bool) -> anyhow::Result<i64> {
    let (rid, host, number, n, head) = (repo.id.clone(), p.host.clone(), p.number, p.n, p.head.clone());
    let (onto, onto_name, tree, commit) = (onto.to_owned(), onto_name.to_owned(), tree.to_owned(), commit.to_owned());
    hub.store
        .call(move |c| {
            let tx = crate::store::tx(c)?;
            let k: i64 = tx.query_row("SELECT COALESCE(MAX(k), 0) + 1 FROM review_pull_replays WHERE repo_id = ?1 AND host = ?2 AND number = ?3", rusqlite::params![rid, host, number], |r| r.get(0))?;
            let state = if clean { "clean" } else { "conflicted" };
            tx.execute(
                "INSERT OR IGNORE INTO review_pull_replays (id, repo_id, host, number, head_n, head_sha, onto, onto_name, tree, state, commit_sha, k, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                rusqlite::params![crate::util::new_id(), rid, host, number, n, head, onto, onto_name, tree, state, commit, k, crate::util::now_unix()],
            )?;
            tx.commit()?;
            Ok(k)
        })
        .await
}

/// Moves change request `d`'s branch to `replay`, its latest revision kiln replayed, if the branch
/// is still where the replay was made from (the replay's first parent): a push with a lease, in
/// kiln's own repository. Why it can't, in words.
async fn move_branch(hub: &Hub, repo: &Repo, path: &str, d: &Diff, replay: &str) -> Result<String, String> {
    let Some(branch) = d.branch.as_deref().filter(|_| d.kind == "change_request") else {
        return Err(format!("{} is a diff: its replay is its latest revision already, and it has no branch to move", d.name));
    };
    let at = gitops::run_git(hub, &["-C", path, "rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")], LS_REMOTE_TIMEOUT).await.map(|o| String::from_utf8_lossy(&o).trim().to_owned()).unwrap_or_default();
    if at == replay {
        return Err(format!("{branch} is at {}'s replay already", d.name));
    }
    let expected = gitops::run_git(hub, &["-C", path, "rev-parse", "--verify", "--quiet", &format!("{replay}^1")], LS_REMOTE_TIMEOUT).await.map(|o| String::from_utf8_lossy(&o).trim().to_owned()).map_err(|e| format!("{e:#}"))?;
    if gitops::run_git(hub, &["-C", path, "update-ref", &format!("refs/heads/{branch}"), replay, &expected], LS_REMOTE_TIMEOUT).await.is_err() {
        let now = gitops::run_git(hub, &["-C", path, "rev-parse", "--short", &format!("refs/heads/{branch}")], LS_REMOTE_TIMEOUT).await.map(|o| String::from_utf8_lossy(&o).trim().to_owned()).unwrap_or_default();
        return Err(format!("{branch} moved since {} was replayed (it's at {now} now): it replays again first", d.name));
    }
    // The poller takes the branch's move now: its runs, as any push's.
    gitops::want(hub, &repo.id);
    Ok(branch.to_owned())
}

/// `POST /api/diffs/:id/update` and `/api/change-requests/:id/update` (W26-R16): someone pressed
/// Update. A change request's branch moves to its latest replay (with a lease: refused, saying why,
/// when the branch moved since); a diff has no branch, its replay being its latest revision already.
pub async fn update(hub: &Hub, d: &Diff, caller: &crate::authz::Caller) -> Result<serde_json::Value, super::Fail> {
    use http::StatusCode;
    if !State::parse(&d.state).is_some_and(|s| s.is_open() && s != State::Landing) {
        return Err((StatusCode::CONFLICT, format!("{} is {}", d.name, d.state)));
    }
    let latest = diffs::versions(hub, d).await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?.pop().ok_or((StatusCode::CONFLICT, format!("{} has no revision", d.name)))?;
    if latest.why.as_deref() != Some(plan::WHY) {
        return Err((StatusCode::CONFLICT, format!("{} v{} isn't a replay: nothing to update", d.name, latest.n)));
    }
    let (repo, path) = diffs::home(hub, d).await?;
    let branch = move_branch(hub, &repo, &path, d, &latest.commit).await.map_err(|why| (StatusCode::CONFLICT, why))?;
    hub.store.audit(caller.name.clone(), "review.fresh.update".into(), format!("{} {} v{}: {branch} moved to its replay ({})", d.repo, d.name, latest.n, latest.commit)).await;
    Ok(serde_json::json!({ "change": d.name, "version": latest.n, "branch": branch, "head": latest.commit, "by": caller.name }))
}

/// `git diff -U0` from `from` to `to` in the repository at `path`.
async fn unified(hub: &Hub, path: &str, from: &str, to: &str) -> String {
    gitops::run_git(hub, &["-C", path, "diff", "-U0", "--no-color", "--no-ext-diff", from, to], LS_REMOTE_TIMEOUT).await.map(|o| String::from_utf8_lossy(&o).into_owned()).unwrap_or_default()
}

/// Where `commit` left the trunk at `tip`: their merge base.
async fn fork(hub: &Hub, path: &str, tip: &str, commit: &str) -> Option<String> {
    gitops::run_git(hub, &["-C", path, "merge-base", tip, commit], LS_REMOTE_TIMEOUT).await.ok().map(|o| String::from_utf8_lossy(&o).trim().to_owned()).filter(|s| !s.is_empty())
}

/// The collision radar (W26-R16): every open change of `trunk` in `repo` (kiln's diffs and change
/// requests; a mirrored repository's pull requests), the lines each changes (a diff its own patch's,
/// a branch's from where it left the trunk), and each pair from different stacks that change the same
/// lines of a file told to both their authors, once a pair and file (`kiln_ci::fresh::collisions`),
/// now rather than once one lands. A pair that no longer overlaps is marked gone.
pub async fn radar(hub: &Arc<Hub>, repo: &Repo, trunk: &str) -> anyhow::Result<usize> {
    let (path, native) = match super::hosting::native_path(hub, repo) {
        Some(p) => (p.to_string_lossy().into_owned(), true),
        None => (gitops::mirror_path(hub, &repo.name).to_string_lossy().into_owned(), false),
    };
    let Ok(tip) = gitops::run_git(hub, &["-C", &path, "rev-parse", "--verify", "--quiet", &format!("refs/heads/{trunk}^{{commit}}")], LS_REMOTE_TIMEOUT).await.map(|o| String::from_utf8_lossy(&o).trim().to_owned()) else {
        return Ok(0);
    };
    // (its name, its author, its stack's bottom, the diff of its lines)
    let mut changes: Vec<(String, String, String, String)> = Vec::new();
    if native {
        let found = open_changes(hub, repo, trunk).await?;
        let below: HashMap<String, String> = found.iter().filter_map(|(o, b)| b.clone().map(|b| (o.diff.id.clone(), b))).collect();
        let names: HashMap<String, String> = found.iter().map(|(o, _)| (o.diff.id.clone(), o.diff.name.clone())).collect();
        for (o, _) in &found {
            let mut bottom = o.diff.id.clone();
            for _ in 0..1000 {
                match below.get(&bottom) {
                    Some(b) => bottom = b.clone(),
                    None => break,
                }
            }
            let text = if o.diff.kind == "change_request" {
                match fork(hub, &path, &tip, &o.latest.commit).await {
                    Some(f) => unified(hub, &path, &f, &o.latest.commit).await,
                    None => continue,
                }
            } else {
                unified(hub, &path, &o.latest.parent, &o.latest.commit).await
            };
            changes.push((o.diff.name.clone(), o.diff.author_for.clone().unwrap_or_else(|| o.diff.author.clone()), names.get(&bottom).cloned().unwrap_or(bottom), text));
        }
    } else {
        let authors: HashMap<(String, i64), String> = {
            let rid = repo.id.clone();
            hub.store
                .call(move |c| {
                    let mut st = c.prepare("SELECT host, number, author FROM host_pulls WHERE repo_id = ?1")?;
                    let rows = st.query_map([&rid], |r| Ok(((r.get::<_, String>(0)?, r.get::<_, i64>(1)?), r.get::<_, String>(2)?)))?.filter_map(Result::ok).collect();
                    Ok(rows)
                })
                .await?
        };
        let stacked = pull_stacks(open_pulls(hub, repo).await?, trunk);
        let below: HashMap<String, String> = stacked.iter().filter_map(|(p, b)| b.clone().map(|b| (p.id(), b))).collect();
        let heads: HashMap<String, String> = stacked.iter().map(|(p, _)| (p.id(), p.head.clone())).collect();
        for (p, under) in &stacked {
            // A stacked pull request's own lines are from the one below's head; a bottom one's from
            // where it left the trunk.
            let from = match under.as_ref().and_then(|b| heads.get(b)) {
                Some(b) => Some(b.clone()),
                None => fork(hub, &path, &tip, &p.head).await,
            };
            let Some(f) = from else { continue };
            let name = p.id();
            let mut bottom = name.clone();
            for _ in 0..1000 {
                match below.get(&bottom) {
                    Some(b) => bottom = b.clone(),
                    None => break,
                }
            }
            let author = authors.get(&(p.host.clone(), p.number)).map(|a| format!("{}:{a}", p.host)).unwrap_or_default();
            changes.push((name, author, bottom, unified(hub, &path, &f, &p.head).await));
        }
    }
    let touches: Vec<plan::Touch> = changes
        .iter()
        .flat_map(|(name, _, stack, text)| plan::old_lines(text).into_iter().map(move |(file, start, end)| plan::Touch { change: name.clone(), stack: stack.clone(), file, start, end }))
        .collect();
    let now_overlapping: BTreeSet<(String, String, String)> = plan::collisions(&touches, &BTreeSet::new()).iter().map(plan::Collision::key).collect();
    let (rid, t) = (repo.id.clone(), trunk.to_owned());
    let told: BTreeSet<(String, String, String)> = hub
        .store
        .call(move |c| {
            let mut st = c.prepare("SELECT a, b, file FROM review_collisions WHERE repo_id = ?1 AND trunk = ?2 AND gone_at IS NULL")?;
            let rows = st.query_map([&rid, &t], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.filter_map(Result::ok).collect();
            Ok(rows)
        })
        .await?;
    let found = plan::collisions(&touches, &told);
    let gone: Vec<(String, String, String)> = told.difference(&now_overlapping).cloned().collect();
    let (rid, t, rows) = (repo.id.clone(), trunk.to_owned(), found.clone());
    hub.store
        .call(move |c| {
            let tx = crate::store::tx(c)?;
            let now = crate::util::now_unix();
            for k in &gone {
                tx.execute("UPDATE review_collisions SET gone_at = ?5 WHERE repo_id = ?1 AND a = ?2 AND b = ?3 AND file = ?4", rusqlite::params![rid, k.0, k.1, k.2, now])?;
            }
            for c in &rows {
                tx.execute(
                    "INSERT INTO review_collisions (repo_id, trunk, a, b, file, start_line, end_line, told_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                     ON CONFLICT (repo_id, a, b, file) DO UPDATE SET trunk = excluded.trunk, start_line = excluded.start_line, end_line = excluded.end_line, told_at = excluded.told_at, gone_at = NULL",
                    rusqlite::params![rid, t, c.a, c.b, c.file, c.start, c.end, now],
                )?;
            }
            tx.commit()?;
            Ok(())
        })
        .await?;
    let author: HashMap<&str, &str> = changes.iter().map(|(n, a, _, _)| (n.as_str(), a.as_str())).collect();
    for c in &found {
        let lines = if c.start == c.end { format!("line {}", c.start) } else { format!("lines {}–{}", c.start, c.end) };
        for (me, other) in [(&c.a, &c.b), (&c.b, &c.a)] {
            let said = format!("{me} and {other} both change {} {lines}", c.file);
            let who = author.get(me.as_str()).map(|a| vec![(*a).to_owned()]).unwrap_or_default();
            crate::rt::spawn(super::notify::diff_event(Arc::clone(hub), repo.name.clone(), "change.collision", me.clone(), String::new(), trunk.to_owned(), "kiln".into(), said, who, None));
        }
    }
    Ok(found.len())
}

/// `GET /api/diffs/:id/collisions` (W26-R16): the open changes `d` changes the same lines as, now.
pub async fn collisions_of(hub: &Hub, d: &Diff) -> anyhow::Result<serde_json::Value> {
    let (name, repo) = (d.name.clone(), d.repo.clone());
    let Some(r) = hub.store.repos().await?.into_iter().find(|r| r.name == repo) else { return Ok(serde_json::json!({ "collisions": [] })) };
    let rid = r.id;
    let rows: Vec<serde_json::Value> = hub
        .store
        .call(move |c| {
            let mut st = c.prepare("SELECT a, b, file, start_line, end_line, told_at FROM review_collisions WHERE repo_id = ?1 AND (a = ?2 OR b = ?2) AND gone_at IS NULL ORDER BY file, start_line")?;
            let rows = st
                .query_map([&rid, &name], |r| {
                    let (a, b): (String, String) = (r.get(0)?, r.get(1)?);
                    let with = if a == name { b } else { a };
                    Ok(serde_json::json!({ "with": with, "file": r.get::<_, String>(2)?, "start": r.get::<_, i64>(3)?, "end": r.get::<_, i64>(4)?, "told_at": r.get::<_, i64>(5)? }))
                })?
                .filter_map(Result::ok)
                .collect();
            Ok(rows)
        })
        .await?;
    Ok(serde_json::json!({ "collisions": rows }))
}

/// How fresh change `d` is (W26-R16), as its page and its row say it: the words
/// (`replays clean on main@4c1e2a`), whether Update can go and, when it can't, why (in words, for a
/// button that stays in place, disabled), and which version a replay's checks come from (`checks
/// from v4`: a replay runs none of its own). `null` for a change that's closed.
pub async fn state(hub: &Hub, d: &Diff, versions: &[Version]) -> serde_json::Value {
    use serde_json::json;
    let (Some(latest), true) = (versions.last(), State::parse(&d.state).is_some_and(|s| s.is_open() && s != State::Landing)) else { return serde_json::Value::Null };
    let Ok((_, path)) = diffs::home(hub, d).await else { return serde_json::Value::Null };
    let tip = gitops::run_git(hub, &["-C", &path, "rev-parse", "--verify", "--quiet", &format!("refs/heads/{}^{{commit}}", d.trunk)], LS_REMOTE_TIMEOUT).await.map(|o| String::from_utf8_lossy(&o).trim().to_owned()).unwrap_or_default();
    let below_conflicted = match diffs::find(hub, d.parent.as_deref().unwrap_or_default()).await {
        Ok(Some(b)) if d.parent.is_some() => diffs::versions(hub, &b).await.ok().and_then(|mut v| v.pop()).filter(|v| v.why.as_deref() == Some(plan::WHY_CONFLICTED)).map(|_| b.name),
        _ => None,
    };
    let on_tip = !tip.is_empty() && contains(hub, &path, &tip, &latest.commit).await;
    let fresh = match (latest.why.as_deref(), on_tip, below_conflicted) {
        (Some(w), true, _) if w == plan::WHY_CONFLICTED => plan::Freshness::Conflicted { tip: tip.clone() },
        (_, true, Some(below)) => plan::Freshness::AboveConflict { below, tip: tip.clone() },
        (Some(w), true, None) if w == plan::WHY => plan::Freshness::Clean { tip: tip.clone() },
        (_, true, None) => plan::Freshness::Current { tip: tip.clone() },
        (_, false, _) => plan::Freshness::Replaying { tip: tip.clone() },
    };
    let branch_at = match d.branch.as_deref() {
        Some(b) => gitops::run_git(hub, &["-C", &path, "rev-parse", "--verify", "--quiet", &format!("refs/heads/{b}")], LS_REMOTE_TIMEOUT).await.map(|o| String::from_utf8_lossy(&o).trim().to_owned()).ok(),
        None => None,
    };
    let why_not = if d.kind != "change_request" || d.branch.is_none() {
        Some("a diff has no branch: its replay is its latest revision".to_owned())
    } else if latest.why.as_deref() == Some(plan::WHY_CONFLICTED) {
        Some("resolve its conflicts first".to_owned())
    } else if latest.why.as_deref() != Some(plan::WHY) {
        Some("nothing to update: its branch is its latest revision".to_owned())
    } else if branch_at.as_deref() == Some(latest.commit.as_str()) {
        Some("its branch is at its replay already".to_owned())
    } else {
        None
    };
    json!({
        "words": fresh.words(&d.trunk),
        "tip": tip,
        "update": { "can": why_not.is_none(), "why_not": why_not },
        "checks_from": latest.checks_from,
    })
}

/// `commit`'s verbatim patch-id (`git patch-id --verbatim`): its diff with context and whitespace as
/// they are, so two commits share one only where the change reads exactly the same.
async fn verbatim(hub: &Hub, path: &str, commit: &str) -> Option<String> {
    let patch = diffs::git_run(hub, &["-C", path, "show", "--no-color", "--format=", commit], &[], Vec::new()).await.ok()?.out;
    let out = diffs::git_with_input(hub, &["-C", path, "patch-id", "--verbatim"], patch).await.ok()?;
    String::from_utf8_lossy(&out).split_whitespace().next().map(str::to_owned)
}

/// A host's pull request's latest replay onto its base, as Update pushes it: its host, the head it
/// was made from, and its commit (none: it conflicted).
struct PullReplay {
    host: String,
    head: String,
    commit: Option<String>,
    /// `clean`, or `conflicted` (its commit has git's markers in: Update won't push it).
    state: String,
}

/// Pull request `number` of `repo`: its latest replay of its latest head, if it has one.
async fn pull_replay(hub: &Hub, repo: &Repo, number: i64) -> anyhow::Result<Option<PullReplay>> {
    let rid = repo.id.clone();
    hub.store
        .call(move |c| {
            use rusqlite::OptionalExtension as _;
            Ok(c.query_row(
                "SELECT r.host, r.head_sha, r.commit_sha, r.state FROM review_pull_replays r
                  WHERE r.repo_id = ?1 AND r.number = ?2
                    AND r.head_sha = (SELECT h.sha FROM host_pull_heads h WHERE h.repo_id = r.repo_id AND h.host = r.host AND h.number = r.number ORDER BY h.n DESC LIMIT 1)
                  ORDER BY r.k DESC LIMIT 1",
                rusqlite::params![rid, number],
                |r| Ok(PullReplay { host: r.get(0)?, head: r.get(1)?, commit: r.get(2)?, state: r.get(3)? }),
            )
            .optional()?)
        })
        .await
}

/// Pushes pull request `number`'s replay to its host through the forge seam (W26-R20): GitHub's
/// update-branch against the head it was made from (the base merged in by GitHub itself; kiln never
/// pushes its own commit there), GitLab's rebase, or elsewhere the replay pushed with a lease. What
/// happened, in words; or why not.
async fn push_pull(hub: &Arc<Hub>, repo: &Repo, number: i64, by: &str) -> Result<serde_json::Value, (http::StatusCode, String)> {
    use http::StatusCode;
    use kiln_seams::forge::{Forge as _, Refusal, Update, Updated};
    let fail = |e: anyhow::Error| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"));
    let r = pull_replay(hub, repo, number).await.map_err(fail)?.ok_or((StatusCode::CONFLICT, format!("#{number} hasn't been replayed onto its base: nothing to update")))?;
    let Some(commit) = r.commit.filter(|_| r.state == "clean") else { return Err((StatusCode::CONFLICT, format!("#{number} conflicts with its base: resolve its conflicts first"))) };
    let (forge, name) = crate::githost::forge::for_repo(hub, repo).await.ok_or((StatusCode::CONFLICT, format!("{} isn't on a host kiln reaches", repo.name)))?;
    let n = u64::try_from(number).unwrap_or_default();
    // GitHub and GitLab update a branch themselves; elsewhere kiln's replay is pushed with a lease.
    let replayed = (!matches!(r.host.as_str(), "github" | "gitlab")).then(|| commit.clone());
    match forge.update(&name, n, Update { expected: r.head.clone(), replayed }).await {
        Ok(Updated::Done { head }) => {
            hub.store.audit(by.to_owned(), "review.fresh.update".into(), format!("{} #{number}: updated on {} to {head}", repo.name, r.host)).await;
            Ok(serde_json::json!({ "change": format!("#{number}"), "host": r.host, "head": head, "state": "updated" }))
        }
        Ok(Updated::Started) => {
            hub.store.audit(by.to_owned(), "review.fresh.update".into(), format!("{} #{number}: updating on {}, from {}", repo.name, r.host, r.head)).await;
            Ok(serde_json::json!({ "change": format!("#{number}"), "host": r.host, "head": r.head, "state": "updating" }))
        }
        Err(Refusal::Moved { head }) => Err((StatusCode::CONFLICT, format!("#{number}'s head moved since its replay (now {}): it replays again first", head.get(..7).unwrap_or(&head)))),
        Err(Refusal::RateLimited { resets_at }) => Err((StatusCode::TOO_MANY_REQUESTS, format!("{} is past its rate limit until {resets_at}: Update waits", r.host))),
        Err(e) => Err((StatusCode::BAD_GATEWAY, format!("{} didn't update #{number}: {e}", r.host))),
    }
}

/// `POST /api/ci/repos/:repo/pulls/:n/update` (W26-R16): someone pressed Update on a host's pull
/// request.
pub async fn update_pull(hub: &Arc<Hub>, repo: &str, n: &str, caller: Option<&crate::authz::Caller>) -> Result<serde_json::Value, (http::StatusCode, String)> {
    use http::StatusCode;
    let number: i64 = n.trim_start_matches('#').parse().map_err(|_| (StatusCode::BAD_REQUEST, format!("`{n}` isn't a pull request's number")))?;
    let r = hub.store.repos().await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?.into_iter().find(|r| r.name == repo).ok_or((StatusCode::NOT_FOUND, format!("no repository {repo}")))?;
    push_pull(hub, &r, number, caller.map_or("admin", |c| c.name.as_str())).await
}

/// The most of a host's hourly requests automatic pushes leave untouched: they stop below a fifth
/// left, saying so (the GitHub-in-Ship agent's advice: the limit is shared with everything else the
/// connection does, and GitHub has secondary limits on content-creating calls).
const AUTO_PUSH_FLOOR: u32 = 5;

/// Pushes each of `numbers`' replays automatically (`fresh.push: auto`), at least a second apart,
/// stopping below a fifth of the host's hourly limit left or on any rate refusal.
async fn auto_push_pulls(hub: &Arc<Hub>, repo: &Repo, numbers: &[i64]) {
    use kiln_seams::forge::Forge as _;
    let Some((forge, name)) = crate::githost::forge::for_repo(hub, repo).await else { return };
    for (i, n) in numbers.iter().enumerate() {
        if i > 0 {
            crate::rt::sleep(std::time::Duration::from_secs(1)).await;
        }
        if let Ok(rate) = forge.rate(&name).await
            && rate.remaining.saturating_mul(AUTO_PUSH_FLOOR) < rate.limit
        {
            tracing::warn!(repo = %repo.name, remaining = rate.remaining, limit = rate.limit, "automatic pushes stop: under a fifth of the host's hourly limit is left");
            hub.store.audit("kiln".into(), "review.fresh.push".into(), format!("{}: automatic pushes held, {} of {} requests left this hour", repo.name, rate.remaining, rate.limit)).await;
            return;
        }
        match push_pull(hub, repo, *n, "kiln").await {
            Ok(_) => {}
            Err((http::StatusCode::TOO_MANY_REQUESTS, why)) => {
                tracing::warn!(repo = %repo.name, %why, "automatic pushes stop: the host's rate limit");
                return;
            }
            Err((_, why)) => tracing::info!(repo = %repo.name, pull = n, %why, "a pull request's replay wasn't pushed automatically"),
        }
    }
}

/// `GET /api/ci/repos/:repo/pulls/:n/fresh` (W26-R16): how fresh a host's pull request is, as its
/// page says it: its latest head's latest replay onto its base, in words, and whether Update can
/// go (a clean replay) or why not. `null` before kiln has replayed it.
pub async fn pull_state(hub: &Arc<Hub>, repo: &str, n: &str) -> Result<serde_json::Value, (http::StatusCode, String)> {
    use http::StatusCode;
    use serde_json::json;
    let number: i64 = n.trim_start_matches('#').parse().map_err(|_| (StatusCode::BAD_REQUEST, format!("`{n}` isn't a pull request's number")))?;
    let r = hub.store.repos().await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?.into_iter().find(|r| r.name == repo).ok_or((StatusCode::NOT_FOUND, format!("no repository {repo}")))?;
    let rid = r.id.clone();
    let found: Option<(String, String, String, String)> = hub
        .store
        .call(move |c| {
            use rusqlite::OptionalExtension as _;
            Ok(c.query_row(
                "SELECT r.state, r.onto, r.onto_name, p.base FROM review_pull_replays r JOIN host_pulls p ON p.repo_id = r.repo_id AND p.host = r.host AND p.number = r.number
                  WHERE r.repo_id = ?1 AND r.number = ?2
                    AND r.head_sha = (SELECT h.sha FROM host_pull_heads h WHERE h.repo_id = r.repo_id AND h.host = r.host AND h.number = r.number ORDER BY h.n DESC LIMIT 1)
                  ORDER BY r.k DESC LIMIT 1",
                rusqlite::params![rid, number],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?)
        })
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    let Some((state, onto, onto_name, base)) = found else { return Ok(serde_json::Value::Null) };
    let clean = state == "clean";
    // On the trunk, the planner's words; on another's branch (a stack), that one's replay.
    let words = match (onto_name.starts_with('#'), clean) {
        (true, true) => format!("replays clean on {onto_name}'s replay"),
        (true, false) => format!("conflicts on {onto_name}'s replay"),
        (false, true) => plan::Freshness::Clean { tip: onto.clone() }.words(&base),
        (false, false) => plan::Freshness::Conflicted { tip: onto.clone() }.words(&base),
    };
    let why_not = (!clean).then(|| "resolve its conflicts first".to_owned());
    Ok(json!({ "words": words, "tip": onto, "update": { "can": clean, "why_not": why_not }, "checks_from": null }))
}
