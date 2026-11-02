//! The catalog of what kiln's repositories share, as the server gathers it (docs/ci.md W29-R20):
//! each hosted repository's actions and reusable workflows on its default branch, with its Access
//! and the newest tag that has each; one's versions and their checks; and which repositories' runs
//! pinned which version (`ci_uses`, written as each run starts).

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use http::StatusCode;
use kiln_ci::catalog::{Consumer, Entry, Filter};
use kiln_ci::expr::Dialect;
use kiln_ci::sharing::Access;
use kiln_ci::wf::Workflow;
use kiln_ci::wf::ir::Action;
use kiln_ci::wire::ActionRef;
use rusqlite::params;
use serde_json::{Value, json};

use super::Fail;
use super::actions::Remote;
use crate::gitops;
use crate::hub::Hub;
use crate::store::Repo;

const GIT: Duration = Duration::from_secs(60);

/// git in the repository `dir`, its output as text; none when it fails.
async fn git(hub: &Hub, dir: &str, args: &[&str]) -> Option<String> {
    let mut all = vec!["-C", dir];
    all.extend_from_slice(args);
    gitops::run_git(hub, &all, GIT).await.ok().map(|o| String::from_utf8_lossy(&o).into_owned())
}

/// A repository's tags, newest first, each with its commit.
async fn tags(hub: &Hub, dir: &str) -> Vec<(String, String)> {
    let out = git(hub, dir, &["for-each-ref", "--sort=-v:refname", "--count=50", "--format=%(refname:short) %(objectname) %(*objectname)", "refs/tags"]).await.unwrap_or_default();
    out.lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            let (tag, obj, peeled) = (f.next()?, f.next()?, f.next());
            Some((tag.to_owned(), peeled.unwrap_or(obj).to_owned()))
        })
        .collect()
}

/// Whether `path` is in the commit `sha`.
async fn has(hub: &Hub, dir: &str, sha: &str, path: &str) -> bool {
    git(hub, dir, &["cat-file", "-e", &format!("{sha}:{path}")]).await.is_some()
}

/// Whether the commit `sha` has the reusable workflow at `path` (a file), or the action in it (a
/// directory, `""` the root).
async fn has_at(hub: &Hub, dir: &str, sha: &str, path: &str) -> bool {
    if path.ends_with(".yml") || path.ends_with(".yaml") {
        return has(hub, dir, sha, path).await;
    }
    let base = if path.is_empty() { String::new() } else { format!("{path}/") };
    has(hub, dir, sha, &format!("{base}action.yml")).await || has(hub, dir, sha, &format!("{base}action.yaml")).await
}

/// The actions and reusable workflows of the repositories kiln hosts that `filter` keeps: those
/// the workflows of `caller` may use (W29-R17), or every one without it.
pub async fn hosted(hub: &Hub, filter: &Filter, caller: Option<&str>) -> Vec<Entry> {
    let repos = hub.store.repos().await.unwrap_or_default();
    let mut out = Vec::new();
    for r in &repos {
        let Some(dir) = super::hosting::native_path(hub, r) else { continue };
        let owner = r.name.split('/').next().unwrap_or_default();
        if !filter.owner.trim().is_empty() && !owner.eq_ignore_ascii_case(filter.owner.trim()) {
            continue;
        }
        let access = super::sharing::access_of(hub, r).await;
        if let Some(c) = caller
            && kiln_ci::sharing::allows(access, &r.name, c).is_err()
        {
            continue;
        }
        out.extend(entries(hub, r, &dir.to_string_lossy(), access).await.into_iter().filter(|e| filter.keeps(e)));
    }
    out
}

/// What a file of a repository's is, for the catalog.
enum Found {
    Action(String, Box<Action>),
    Workflow(Box<Workflow>),
}

/// `repo`'s actions and reusable workflows on its default branch, each one's `uses:` at the newest
/// tag that has it, else at the branch's commit (commits and tags, never a branch's name).
async fn entries(hub: &Hub, repo: &Repo, dir: &str, access: Access) -> Vec<Entry> {
    let Some(head) = git(hub, dir, &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"]).await.map(|s| s.trim().to_owned()).filter(|s| s.len() == 40) else { return Vec::new() };
    let files = git(hub, dir, &["ls-tree", "-r", "--name-only", &head, "--", "action.yml", "action.yaml", ".github/actions", ".kiln/actions", ".github/workflows"]).await.unwrap_or_default();
    let owner = repo.name.split('/').next().unwrap_or_default();
    let native = hub.cfg.ci.actions.source_of(owner).is_some_and(|s| s.native);
    let tags = tags(hub, dir).await;
    let mut out = Vec::new();
    for f in files.lines() {
        let dir_of = kiln_ci::catalog::action_dir(f);
        let workflow = matches!(kiln_ci::wf::file_kind(f), Some(kiln_ci::wf::FileKind::GhaWorkflow));
        if dir_of.is_none() && !workflow {
            continue;
        }
        let Some(text) = git(hub, dir, &["show", &format!("{head}:{f}")]).await else { continue };
        let found = match dir_of {
            Some(d) => kiln_ci::wf::parse_action(&text).0.map(|a| Found::Action(d.to_owned(), Box::new(a))),
            None => kiln_ci::wf::parse_workflow(f, &text, Dialect::Gha).0.filter(|w| w.on.iter().any(|t| t.event == "workflow_call")).map(|w| Found::Workflow(Box::new(w))),
        };
        let Some(found) = found else { continue };
        let mut at = head.clone();
        for (tag, sha) in &tags {
            if has(hub, dir, sha, f).await {
                at.clone_from(tag);
                break;
            }
        }
        match found {
            Found::Action(d, a) => {
                let uses = kiln_ci::catalog::uses(native, &repo.name, &d, &at);
                out.push(kiln_ci::catalog::of_action(&repo.name, &d, &a, access, uses));
            }
            Found::Workflow(w) => {
                let uses = kiln_ci::catalog::uses(native, &repo.name, f, &at);
                out.extend(kiln_ci::catalog::of_workflow(&repo.name, &w, access, uses));
            }
        }
    }
    out
}

/// `GET /api/ci/actions/:owner/:repo?path=` (`ci.actions.get`, W29-R20): one repository's actions
/// and reusable workflows, the one at `path` with it (an action's directory, `""` at the root, or
/// a workflow's file), with its versions (its tags, each with its commit and the conclusions of
/// its own runs there), its access, and its consumers (which repositories' runs pinned which
/// version). A repository kiln doesn't host has its mirror's tags and consumers, and no access.
pub async fn one(hub: &Hub, owner: &str, name: &str, path: Option<&str>) -> Result<Value, Fail> {
    let internal = |e: anyhow::Error| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"));
    let full = format!("{owner}/{name}");
    let path = path.map(|p| p.trim_matches('/').to_owned());
    let repos = hub.store.repos().await.map_err(internal)?;
    let hosted: Option<(&Repo, String)> = repos.iter().find_map(|r| super::hosting::native_path(hub, r).filter(|_| r.name.eq_ignore_ascii_case(&full)).map(|d| (r, d.to_string_lossy().into_owned())));
    let (dir, access, mut entries) = match &hosted {
        Some((r, d)) => {
            let access = super::sharing::access_of(hub, r).await;
            (Some(d.clone()), Some(access), entries(hub, r, d, access).await)
        }
        None => (Some(super::actions::mirror(hub, owner, name)).filter(|d| d.join("HEAD").exists()).map(|d| d.to_string_lossy().into_owned()), None, Vec::new()),
    };
    if let Some(p) = &path {
        entries.retain(|e| &e.path == p);
    }
    let mut versions = Vec::new();
    if let Some(d) = &dir {
        for (tag, sha) in tags(hub, d).await {
            // With a path, the tags that have it.
            if let Some(p) = &path
                && !has_at(hub, d, &sha, p).await
            {
                continue;
            }
            let checks = match &hosted {
                Some((r, _)) => checks_at(hub, &r.id, &sha).await.map_err(internal)?,
                None => Vec::new(),
            };
            versions.push(json!({ "ref": tag, "sha": sha, "checks": checks }));
        }
    }
    let consumers = consumers(hub, &full, path.as_deref()).await.map_err(internal)?;
    if dir.is_none() && consumers.is_empty() {
        return Err((StatusCode::NOT_FOUND, format!("`{full}` is no repository kiln hosts or mirrors")));
    }
    Ok(json!({
        "owner": owner,
        "repo": name,
        "path": path,
        "hosted": hosted.is_some(),
        "access": access,
        "entries": entries,
        "versions": versions,
        "consumers": consumers,
    }))
}

/// The newest run of each of a repository's workflows at `sha`: its own checks of a version.
async fn checks_at(hub: &Hub, repo_id: &str, sha: &str) -> anyhow::Result<Vec<Value>> {
    let (id, sha) = (repo_id.to_owned(), sha.to_owned());
    hub.store
        .call(move |c| {
            let mut st = c.prepare("SELECT workflow_id, status, conclusion FROM runs WHERE repo_id = ?1 AND sha = ?2 AND workflow_id IS NOT NULL ORDER BY created_at DESC LIMIT 100")?;
            let rows = st.query_map(params![id, sha], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, Option<String>>(2)?)))?;
            let mut seen = std::collections::BTreeSet::new();
            let mut out = Vec::new();
            for row in rows {
                let (workflow, status, conclusion) = row?;
                if seen.insert(workflow.clone()) {
                    out.push(json!({ "workflow": workflow, "status": status, "conclusion": conclusion }));
                }
            }
            Ok(out)
        })
        .await
}

/// Which repositories' runs pinned which version of `full`'s `path` (any of its paths without
/// one), most recent first.
pub(crate) async fn consumers(hub: &Hub, full: &str, path: Option<&str>) -> anyhow::Result<Vec<Consumer>> {
    let (publisher, path) = (full.to_ascii_lowercase(), path.map(str::to_owned));
    hub.store
        .call(move |c| {
            // With the workflows whose runs used it, by name (Ship's Used by, W29-R20).
            let mut st = c.prepare(
                "SELECT p.name, u.path, u.kind, u.git_ref, u.sha, COUNT(*), MAX(u.at), group_concat(DISTINCT w.path) FROM ci_uses u JOIN repos p ON p.id = u.repo_id \
                 LEFT JOIN runs r ON r.id = u.run_id LEFT JOIN ci_workflows w ON w.id = r.workflow_id \
                 WHERE u.publisher = ?1 AND (?2 IS NULL OR u.path = ?2) GROUP BY u.repo_id, u.path, u.git_ref, u.sha ORDER BY MAX(u.at) DESC LIMIT 200",
            )?;
            let stem = |p: &str| p.rsplit('/').next().map(|n| n.rsplit_once('.').map_or(n, |(s, _)| s)).unwrap_or(p).to_owned();
            let rows = st.query_map(params![publisher, path], |r| {
                let workflows: Option<String> = r.get(7)?;
                let mut workflows: Vec<String> = workflows.map(|w| w.split(',').map(stem).collect()).unwrap_or_default();
                workflows.sort();
                workflows.dedup();
                Ok(Consumer { repo: r.get(0)?, path: r.get(1)?, kind: r.get(2)?, git_ref: r.get(3)?, sha: r.get(4)?, runs: r.get(5)?, last_at: r.get(6)?, workflows })
            })?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
}

/// What a run of `repo` pinned of other repositories as it started: its actions (`actions`,
/// W07-R02) and the remote reusable workflows it calls (`called`, by their `uses:`), for the
/// catalog's consumers (W29-R20). A failure is logged; the run goes on.
pub async fn record(hub: &Hub, run: &str, repo: &Repo, actions: &BTreeMap<String, ActionRef>, called: &HashMap<String, Arc<Workflow>>) {
    let mut rows: Vec<(String, String, &'static str, String, String)> = Vec::new();
    for (uses, a) in actions {
        if a.error.is_some() || a.sha.is_empty() || a.repo.eq_ignore_ascii_case(&repo.name) {
            continue;
        }
        let Some(r) = Remote::parse(uses) else { continue };
        rows.push((a.repo.to_ascii_lowercase(), a.path.clone(), "action", r.git_ref, a.sha.clone()));
    }
    for (uses, w) in called {
        let (Some(o), Some(r)) = (&w.origin, Remote::parse(uses)) else { continue };
        if o.repository.eq_ignore_ascii_case(&repo.name) {
            continue;
        }
        rows.push((o.repository.to_ascii_lowercase(), r.path, "workflow", r.git_ref, o.sha.clone()));
    }
    if rows.is_empty() {
        return;
    }
    let (run, repo_id, at) = (run.to_owned(), repo.id.clone(), crate::util::now_unix());
    let done = hub
        .store
        .call(move |c| {
            let mut st = c.prepare("INSERT OR IGNORE INTO ci_uses (run_id, repo_id, publisher, path, kind, git_ref, sha, at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)")?;
            for (publisher, path, kind, git_ref, sha) in rows {
                st.execute(params![run, repo_id, publisher, path, kind, git_ref, sha, at])?;
            }
            Ok(())
        })
        .await;
    if let Err(e) = done {
        tracing::warn!(repo = %repo.name, error = %format!("{e:#}"), "what the run pinned wasn't recorded for the catalog");
    }
}
