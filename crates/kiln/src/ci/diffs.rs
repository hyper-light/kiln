//! Diffs (docs/ci.md W14-R03): a push to `refs/for/<trunk>` of a native repository makes each
//! commit between the trunk and the pushed one a diff (`D12`), or a new version of the diff with
//! its `Change-Id` (`D12 v3`, marked rebase-only when only its base moved), links them as a stack,
//! keeps every version at `refs/kiln/diffs/D12/v3`, and starts the `pull_request` and `diff`
//! workflows of each new version. Reads: a repository's diffs, a diff with its versions, a
//! version's files, an interdiff between versions (`git range-diff`), a stack. Its author marks
//! it ready, abandons or reclaims it; anyone who may submit commandeers it.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use http::{Method, StatusCode};
use kiln_ci::review::{self, Known, Planned, State, Step};
use kiln_ci::trigger::context::{self, PullRequest, Side};
use kiln_ci::trigger::{Event, Source};
use rusqlite::{OptionalExtension as _, params};
use serde::Serialize;
use serde_json::{Value, json};

use super::Fail;
use crate::authz::{Caller, Scope};
use crate::gitops::{self, LS_REMOTE_TIMEOUT};
use crate::http::server::{self as srv, Response};
use crate::hub::Hub;
use crate::store::Repo;

fn fail(e: anyhow::Error) -> Fail {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}

/// What a git run said: whether it succeeded, its stdout and its stderr.
pub(crate) struct Ran {
    pub ok: bool,
    pub out: Vec<u8>,
    pub err: Vec<u8>,
}

/// git with `env` set and `input` on its stdin, however it ends.
pub(crate) async fn git_run(hub: &Hub, args: &[&str], env: &[(&str, &str)], input: Vec<u8>) -> anyhow::Result<Ran> {
    let mut cmd = gitops::git(hub);
    cmd.args(args);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let d = crate::proc::spawn_duplex(cmd)?;
    let stdin = d.stdin.clone();
    drop(d.stdin);
    crate::rt::spawn(async move {
        if !input.is_empty() {
            let _ = stdin.send(input).await;
        }
    });
    let (stdout, stderr, exit) = (d.stdout, d.stderr, d.exit);
    let read = async move {
        let err = crate::rt::spawn(async move {
            let mut err = Vec::new();
            while let Ok(c) = stderr.recv().await {
                err.extend_from_slice(&c);
            }
            err
        });
        let mut out = Vec::new();
        while let Ok(c) = stdout.recv().await {
            out.extend_from_slice(&c);
        }
        (out, err.await.unwrap_or_default(), exit.await.ok().flatten())
    };
    let what = args.iter().find(|a| !a.starts_with('-') && !a.contains('/')).copied().unwrap_or_default();
    let (out, err, code) = crate::rt::timeout(Duration::from_secs(300), read).await.map_err(|_| anyhow::anyhow!("git {what} took too long"))?;
    Ok(Ran { ok: code == Some(0), out, err })
}

/// git with `input` on its stdin: what it prints.
pub(crate) async fn git_with_input(hub: &Hub, args: &[&str], input: Vec<u8>) -> anyhow::Result<Vec<u8>> {
    let r = git_run(hub, args, &[], input).await?;
    let what = args.iter().find(|a| !a.starts_with('-') && !a.contains('/')).copied().unwrap_or_default();
    anyhow::ensure!(r.ok, "git {what}: {}", crate::util::truncate(String::from_utf8_lossy(&r.err).trim(), 600));
    Ok(r.out)
}

/// What a tree is made from in a temporary index.
pub(crate) enum Edit<'a> {
    /// A unified diff, applied (`git apply --cached`).
    Apply(&'a str),
    /// A file's new content, its mode kept.
    Put { file: &'a str, mode: &'a str, content: Vec<u8> },
}

/// `base`'s tree with `edit` made to it, written to the repository without a working tree;
/// why not when it doesn't apply.
pub(crate) async fn tree_with(hub: &Hub, path: &str, base: &str, edit: Edit<'_>) -> Result<String, String> {
    let index = std::env::temp_dir().join(format!("kiln-index-{}", crate::util::new_id()));
    let idx = index.to_string_lossy().into_owned();
    let env = [("GIT_INDEX_FILE", idx.as_str())];
    let result = async {
        let r = git_run(hub, &["-C", path, "read-tree", base], &env, Vec::new()).await.map_err(|e| format!("{e:#}"))?;
        if !r.ok {
            return Err(format!("{base}: {}", String::from_utf8_lossy(&r.err).trim()));
        }
        match edit {
            Edit::Apply(diff) => {
                let r = git_run(hub, &["-C", path, "apply", "--cached", "--whitespace=nowarn", "-"], &env, diff.as_bytes().to_vec()).await.map_err(|e| format!("{e:#}"))?;
                if !r.ok {
                    return Err(String::from_utf8_lossy(&r.err).trim().to_owned());
                }
            }
            Edit::Put { file, mode, content } => {
                let blob = git_with_input(hub, &["-C", path, "hash-object", "-w", "--stdin"], content).await.map_err(|e| format!("{e:#}"))?;
                let info = format!("{mode},{},{file}", String::from_utf8_lossy(&blob).trim());
                let r = git_run(hub, &["-C", path, "update-index", "--add", "--cacheinfo", &info], &env, Vec::new()).await.map_err(|e| format!("{e:#}"))?;
                if !r.ok {
                    return Err(String::from_utf8_lossy(&r.err).trim().to_owned());
                }
            }
        }
        let r = git_run(hub, &["-C", path, "write-tree"], &env, Vec::new()).await.map_err(|e| format!("{e:#}"))?;
        if !r.ok {
            return Err(String::from_utf8_lossy(&r.err).trim().to_owned());
        }
        Ok(String::from_utf8_lossy(&r.out).trim().to_owned())
    }
    .await;
    let _ = std::fs::remove_file(&index);
    result
}

/// A commit of `tree` on `parent`, by `author` (`Name <email>`, at `date` when given), committed
/// by `committer`, saying `message`.
pub(crate) async fn make_commit(hub: &Hub, path: &str, tree: &str, parent: &str, (author, date): (&str, Option<&str>), committer: &str, message: &str) -> Result<String, String> {
    let split = |who: &str| -> (String, String) {
        match who.rsplit_once('<') {
            Some((name, email)) => (name.trim().to_owned(), email.trim_end_matches('>').trim().to_owned()),
            None => (who.trim().to_owned(), format!("{}@kiln", who.trim().replace([' ', ':'], "-"))),
        }
    };
    let ((an, ae), (cn, ce)) = (split(author), split(committer));
    let mut env = vec![("GIT_AUTHOR_NAME", an.as_str()), ("GIT_AUTHOR_EMAIL", ae.as_str()), ("GIT_COMMITTER_NAME", cn.as_str()), ("GIT_COMMITTER_EMAIL", ce.as_str())];
    if let Some(d) = date {
        env.push(("GIT_AUTHOR_DATE", d));
    }
    let r = git_run(hub, &["-C", path, "commit-tree", tree, "-p", parent, "-F", "-"], &env, message.as_bytes().to_vec()).await.map_err(|e| format!("{e:#}"))?;
    if !r.ok {
        return Err(String::from_utf8_lossy(&r.err).trim().to_owned());
    }
    Ok(String::from_utf8_lossy(&r.out).trim().to_owned())
}

/// A Change-Id no commit has yet.
pub(crate) fn new_change_id() -> String {
    let h = crate::util::sha256_hex(crate::util::random_token().as_bytes());
    format!("I{}", h.get(..40).unwrap_or(&h))
}

/// `POST /api/repos/:repo/submit` (W14-R03, for agents without git): `{ trunk?, patches: [{
/// message, diff, author?, date? }] | mbox: "<git format-patch output>", draft?, reviewers?,
/// topic?, message? }`. The patches become commits on the trunk's tip, given Change-Ids where
/// they lack one, and are submitted as a push to `refs/for/<trunk>` is.
pub async fn submit_patches(hub: &Arc<Hub>, repo_key: &str, caller: &Caller, body: &Value) -> Result<Value, Fail> {
    let repos = hub.store.repos().await.map_err(fail)?;
    let repo = repos.into_iter().find(|r| r.name == repo_key || r.id == repo_key).ok_or((StatusCode::NOT_FOUND, "no such repository".to_owned()))?;
    if !caller.can("diffs.submit", &Scope::Repo { repo: repo.name.clone() }) {
        return Err((StatusCode::FORBIDDEN, "submitting needs `diffs.submit`".into()));
    }
    let path = super::hosting::native_path(hub, &repo).ok_or((StatusCode::CONFLICT, "diffs are for repositories kiln hosts".to_owned()))?;
    let p = path.to_string_lossy().into_owned();
    let head = gitops::run_git(hub, &["-C", &p, "symbolic-ref", "--short", "HEAD"], LS_REMOTE_TIMEOUT).await.map(|o| String::from_utf8_lossy(&o).trim().to_owned()).unwrap_or_else(|_| "main".into());
    let trunk = body.get("trunk").and_then(Value::as_str).map_or(head, str::to_owned);
    let patches: Vec<review::Patch> = match (body.get("mbox").and_then(Value::as_str), body.get("patches")) {
        (Some(m), _) => review::parse_mbox(m),
        (None, Some(list)) => serde_json::from_value(list.clone()).map_err(|e| (StatusCode::BAD_REQUEST, format!("`patches`: {e}")))?,
        (None, None) => Vec::new(),
    };
    if patches.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "nothing to submit: give `patches` or `mbox`".into()));
    }
    let tip = gitops::run_git(hub, &["-C", &p, "rev-parse", "--verify", &format!("refs/heads/{trunk}^{{commit}}")], LS_REMOTE_TIMEOUT).await.map(|o| String::from_utf8_lossy(&o).trim().to_owned()).map_err(|_| (StatusCode::UNPROCESSABLE_ENTITY, format!("{trunk} has no commits yet")))?;
    let (name, kind) = author_of(&caller.name, Some(caller.kind));
    let me = name.strip_prefix("user:").unwrap_or(&name).to_owned();
    let mut parent = tip;
    for (i, patch) in patches.iter().enumerate() {
        let tree = tree_with(hub, &p, &parent, Edit::Apply(&patch.diff)).await.map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, format!("patch {} doesn't apply: {e}", i + 1)))?;
        let message = if patch.message.trim().is_empty() { return Err((StatusCode::BAD_REQUEST, format!("patch {} has no message", i + 1))) } else { review::with_change_id(&patch.message, &new_change_id()) };
        let author = patch.author.clone().unwrap_or_else(|| me.clone());
        parent = make_commit(hub, &p, &tree, &parent, (&author, patch.date.as_deref()), &me, &message).await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    }
    let mut given: Vec<String> = Vec::new();
    if body.get("draft").and_then(Value::as_bool) == Some(true) {
        given.push("draft".into());
    }
    for r in body.get("reviewers").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
        given.push(format!("reviewer={r}"));
    }
    for k in ["topic", "message"] {
        if let Some(v) = body.get(k).and_then(Value::as_str) {
            given.push(format!("{k}={v}"));
        }
    }
    let options = review::options(&given);
    let out = submit(hub, Submission { repo: &repo, path: &p, trunk: &trunk, sha: &parent, pushed_by: &caller.name, author: (name, kind), options }).await.map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e))?;
    Ok(json!({ "diffs": out }))
}

/// Who a diff is by, as its rows name them: `user:ada` (a person, through any of their tokens),
/// `agent:review`, `admin`, `job:<leg>`; and whether that is a `human` or an `agent`.
pub fn author_of(caller_name: &str, kind: Option<crate::authz::gate::Kind>) -> (String, &'static str) {
    use crate::authz::gate::Kind;
    match kind {
        Some(Kind::Agent) => (caller_name.split(' ').next().unwrap_or(caller_name).to_owned(), "agent"),
        Some(Kind::Pat) => (caller_name.rsplit(" of ").next().unwrap_or(caller_name).to_owned(), "human"),
        Some(Kind::Job) | None => (caller_name.to_owned(), "agent"),
        _ => (caller_name.to_owned(), "human"),
    }
}

/// Whom a principal acts for: the person after an agent's `for` (`agent:review for user:ada` is
/// `user:ada`'s); `None` for people themselves.
pub fn acting_for(caller_name: &str) -> Option<String> {
    caller_name.split_once(" for ").map(|(_, p)| p.trim().to_owned()).filter(|p| !p.is_empty())
}

/// A submission: the pushed commit for a trunk, by whom, with the push's options.
pub struct Submission<'a> {
    pub repo: &'a Repo,
    /// The native repository's bare repository.
    pub path: &'a str,
    pub trunk: &'a str,
    pub sha: &'a str,
    /// The principal as the audit names them.
    pub pushed_by: &'a str,
    pub author: (String, &'static str),
    pub options: review::Options,
}

/// What a submission did to one commit (or a change request's push).
#[derive(Debug, Clone, Serialize)]
pub struct Outcome {
    /// `D12`, `CR4`.
    pub name: String,
    pub number: u32,
    pub version: u32,
    /// `new`, `updated`, `rebased`, `reordered` or `unchanged`.
    pub kind: &'static str,
    pub title: String,
    pub url: String,
}

/// The commits between `trunk` and `sha`, bottom first, each with its patch id.
async fn commits(hub: &Hub, path: &str, trunk_sha: &str, sha: &str) -> anyhow::Result<Vec<review::Commit>> {
    let range = format!("{trunk_sha}..{sha}");
    let most = format!("--max-count={}", review::MAX_STACK + 1);
    let log = gitops::run_git(hub, &["-C", path, "log", "--reverse", "--topo-order", &most, "--format=%H%x1f%P%x1f%B%x1e", &range], LS_REMOTE_TIMEOUT).await?;
    let mut out: Vec<review::Commit> = String::from_utf8_lossy(&log)
        .split('\x1e')
        .filter_map(|rec| {
            let mut f = rec.trim_start_matches('\n').splitn(3, '\x1f');
            let (sha, parents, message) = (f.next()?.trim(), f.next()?, f.next().unwrap_or_default());
            (!sha.is_empty()).then(|| review::Commit { sha: sha.to_owned(), parents: parents.split_whitespace().map(str::to_owned).collect(), message: message.to_owned(), patch_id: String::new() })
        })
        .collect();
    // `git patch-id --stable` over the same commits' patches: `<patch id> <commit>` each.
    if !out.is_empty() && out.len() <= review::MAX_STACK {
        let patches = gitops::run_git(hub, &["-C", path, "log", "-p", "--no-color", "--reverse", "--topo-order", &most, &range], LS_REMOTE_TIMEOUT).await?;
        if let Ok(ids) = git_with_input(hub, &["-C", path, "patch-id", "--stable"], patches).await {
            let by: HashMap<String, String> = String::from_utf8_lossy(&ids).lines().filter_map(|l| l.split_once(' ')).map(|(p, c)| (c.trim().to_owned(), p.to_owned())).collect();
            for c in &mut out {
                c.patch_id = by.get(&c.sha).cloned().unwrap_or_default();
            }
        }
    }
    Ok(out)
}

/// A diff's latest version, by Change-Id, for each of `ids`: its row id and what [`review::plan`]
/// needs.
fn known(c: &rusqlite::Connection, repo: &str, trunk: &str, ids: &[String]) -> rusqlite::Result<BTreeMap<String, (String, Option<String>, Known)>> {
    let mut out = BTreeMap::new();
    let mut st = c.prepare(
        "SELECT d.id, d.number, d.state, d.stack_id, v.n, v.commit_sha, v.parent, v.patch_id, v.message, v.base, (SELECT b.change_id FROM diffs b WHERE b.id = d.parent_diff)
         FROM diffs d JOIN diff_versions v ON v.diff_id = d.id AND v.n = (SELECT MAX(n) FROM diff_versions WHERE diff_id = d.id)
         WHERE d.repo_id = ?1 AND d.trunk = ?2 AND d.kind = 'diff' AND d.change_id = ?3 ORDER BY d.number DESC LIMIT 1",
    )?;
    for id in ids {
        let row = st
            .query_row(params![repo, trunk, id], |r| {
                let state: String = r.get(2)?;
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(3)?,
                    Known {
                        number: r.get(1)?,
                        state: State::parse(&state).unwrap_or(State::NeedsReview),
                        version: r.get(4)?,
                        commit: r.get(5)?,
                        parent: r.get(6)?,
                        base: r.get(9)?,
                        below: r.get(10)?,
                        patch_id: r.get(7)?,
                        message: r.get(8)?,
                    },
                ))
            })
            .optional()?;
        if let Some(k) = row {
            out.insert(id.clone(), k);
        }
    }
    Ok(out)
}

/// A new version to keep and test: which diff, which version, of which commit.
#[derive(Debug, Clone)]
pub(crate) struct Fresh {
    pub number: u32,
    pub n: u32,
    pub sha: String,
    pub parent: String,
    pub title: String,
    pub summary: String,
    pub message: String,
    pub draft: bool,
    pub rebase_only: bool,
    /// A change request's branch (`None`: a diff).
    pub branch: Option<String>,
}

/// Makes the submission's diffs and versions (W14-R03), refused whole, saying why, when any of its
/// commits can't be one. Its new versions' workflows start in the background.
pub async fn submit(hub: &Arc<Hub>, s: Submission<'_>) -> Result<Vec<Outcome>, String> {
    let trunk_ref = format!("refs/heads/{}", s.trunk);
    let trunk_sha = gitops::run_git(hub, &["-C", s.path, "rev-parse", "--verify", "--quiet", &format!("{trunk_ref}^{{commit}}")], LS_REMOTE_TIMEOUT)
        .await
        .map(|o| String::from_utf8_lossy(&o).trim().to_owned())
        .map_err(|_| format!("{} has no commits yet: push it first", s.trunk))?;
    let list = commits(hub, s.path, &trunk_sha, s.sha).await.map_err(|e| format!("{e:#}"))?;
    let base = gitops::run_git(hub, &["-C", s.path, "merge-base", &trunk_sha, s.sha], LS_REMOTE_TIMEOUT).await.map(|o| String::from_utf8_lossy(&o).trim().to_owned()).unwrap_or_else(|_| trunk_sha.clone());
    let ids: Vec<String> = list.iter().filter_map(|c| review::change_id(&c.message)).collect();
    let (repo_id, trunk) = (s.repo.id.clone(), s.trunk.to_owned());
    let found = hub.store.call(move |c| Ok(known(c, &repo_id, &trunk, &ids)?)).await.map_err(|e| format!("{e:#}"))?;
    let plain: BTreeMap<String, Known> = found.iter().map(|(k, (_, _, v))| (k.clone(), v.clone())).collect();
    let planned = review::plan(&list, &plain, &base)?;
    let messages: HashMap<String, String> = list.iter().map(|c| (c.sha.clone(), c.message.clone())).collect();
    let patches: HashMap<String, String> = list.iter().map(|c| (c.sha.clone(), c.patch_id.clone())).collect();

    // The rows, in one transaction: diffs, versions, the stack.
    let (repo_id, trunk, opts) = (s.repo.id.clone(), s.trunk.to_owned(), s.options.clone());
    let (pushed_by, author, author_kind) = (s.pushed_by.to_owned(), s.author.0.clone(), s.author.1);
    let rows: Vec<(Planned, Option<(String, Option<String>)>)> = planned.into_iter().map(|p| { let f = found.get(&p.change_id).map(|(id, stack, _)| (id.clone(), stack.clone())); (p, f) }).collect();
    let (outcomes, fresh) = hub
        .store
        .call(move |c| {
            let tx = c.transaction()?;
            let now = crate::util::now_unix();
            // The stack: the bottom diff's, or a new one.
            let stack = match rows.first().and_then(|(_, f)| f.as_ref()).and_then(|(_, s)| s.clone()) {
                Some(s) => s,
                None => {
                    let id = crate::util::new_id();
                    tx.execute("INSERT INTO stacks (id, repo_id, trunk, created_at) VALUES (?1, ?2, ?3, ?4)", params![id, repo_id, trunk, now])?;
                    id
                }
            };
            let mut outcomes = Vec::new();
            let mut fresh = Vec::new();
            let mut below: Option<String> = None;
            for (p, existing) in rows {
                let message = messages.get(&p.sha).cloned().unwrap_or_default();
                let patch_id = patches.get(&p.sha).cloned().unwrap_or_default();
                let (diff_id, number, n, kind, rebase_only) = match (&p.step, existing) {
                    (Step::New, _) => {
                        // Diffs and change requests share the numbers: no D12 beside a CR12.
                        let number: u32 = tx.query_row("SELECT COALESCE(MAX(number), 0) + 1 FROM diffs", [], |r| r.get(0))?;
                        let id = crate::util::new_id();
                        let state = if opts.draft { State::Draft } else { State::NeedsReview };
                        tx.execute(
                            "INSERT INTO diffs (id, repo_id, number, kind, change_id, title, summary, author, author_kind, state, trunk, stack_id, parent_diff, topic, created_at, updated_at, author_for)
                             VALUES (?1, ?2, ?3, 'diff', ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?14, ?15)",
                            params![id, repo_id, number, p.change_id, p.title, p.summary, author, author_kind, state.as_str(), trunk, stack, below, opts.topic, now, acting_for(&pushed_by)],
                        )?;
                        (id, number, 1u32, "new", false)
                    }
                    (Step::Version { number, n, rebase_only, reordered }, Some((id, _))) => {
                        // A new version returns a diff asked for changes to review; `-o ready` a draft.
                        tx.execute(
                            "UPDATE diffs SET title = ?2, summary = ?3, updated_at = ?4, stack_id = ?5, parent_diff = ?6, topic = COALESCE(?7, topic),
                                    state = CASE WHEN state = 'changes-requested' OR (state = 'draft' AND ?8) THEN 'needs-review' ELSE state END
                             WHERE id = ?1",
                            params![id, p.title, p.summary, now, stack, below, opts.topic, opts.ready],
                        )?;
                        let kind = match (*reordered, *rebase_only) {
                            (true, _) => "reordered",
                            (false, true) => "rebased",
                            (false, false) => "updated",
                        };
                        (id, *number, *n, kind, *rebase_only)
                    }
                    (Step::Unchanged { number, n }, Some((id, _))) => {
                        tx.execute("UPDATE diffs SET stack_id = ?2, parent_diff = ?3 WHERE id = ?1", params![id, stack, below])?;
                        (id, *number, *n, "unchanged", false)
                    }
                    (_, None) => anyhow::bail!("a diff that was there isn't"),
                };
                if kind != "unchanged" {
                    tx.execute(
                        "INSERT INTO diff_versions (diff_id, n, commit_sha, parent, base, patch_id, message, note, rebase_only, pushed_by, created_at)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                        params![diff_id, n, p.sha, p.parent, base, patch_id, message, opts.message, rebase_only, pushed_by, now],
                    )?;
                    let draft: String = tx.query_row("SELECT state FROM diffs WHERE id = ?1", [&diff_id], |r| r.get(0))?;
                    fresh.push(Fresh { number, n, sha: p.sha.clone(), parent: p.parent.clone(), title: p.title.clone(), summary: p.summary.clone(), message, draft: draft == "draft", rebase_only, branch: None });
                }
                // `-o reviewer=` asks for review of what the push changed.
                for r in opts.reviewers.iter().filter(|_| kind != "unchanged") {
                    tx.execute("INSERT OR IGNORE INTO diff_reviewers (diff_id, reviewer, state, added_at) VALUES (?1, ?2, 'not-looked', ?3)", params![diff_id, r, now])?;
                }
                outcomes.push((number, n, kind, p.title.clone()));
                below = Some(diff_id);
            }
            tx.commit()?;
            Ok((outcomes, fresh))
        })
        .await
        .map_err(|e| format!("{e:#}"))?;

    // Every version is kept, at its own ref.
    for f in &fresh {
        let r = format!("refs/kiln/diffs/D{}/v{}", f.number, f.n);
        if let Err(e) = gitops::run_git(hub, &["-C", s.path, "update-ref", &r, &f.sha], LS_REMOTE_TIMEOUT).await {
            tracing::warn!(diff = f.number, version = f.n, error = %format!("{e:#}"), "a diff's version ref couldn't be written");
        }
    }
    let base_url = hub.cfg.server.public_url.trim_end_matches('/').to_owned();
    let out: Vec<Outcome> = outcomes.into_iter().map(|(number, version, kind, title)| Outcome { name: format!("D{number}"), number, version, kind, title, url: format!("{base_url}/diffs/D{number}") }).collect();
    hub.store.audit(s.pushed_by.to_owned(), "diffs.submit".into(), format!("{} {}: {}", s.repo.name, s.trunk, out.iter().map(|o| format!("D{} v{} {}", o.number, o.version, o.kind)).collect::<Vec<_>>().join(", "))).await;
    // Their checks.
    let (hub2, repo, trunk, author, base_sha) = (hub.clone(), s.repo.clone(), s.trunk.to_owned(), s.author.clone(), trunk_sha);
    crate::rt::spawn(async move {
        for f in fresh {
            if let Err(e) = checks(&hub2, &repo, &trunk, &base_sha, &author, &f).await {
                tracing::warn!(diff = f.number, version = f.n, error = %format!("{e:#}"), "a diff's workflows didn't start");
            }
        }
    });
    Ok(out)
}

/// The events a new version is (W03-R09): `pull_request` (`opened` for v1, `synchronize` after)
/// and, for a diff, kiln's `diff`, with it in the payload's `kiln.diff` (`kiln.change_request`).
fn events_of(repo: &Repo, ctx: &context::Repo, trunk: &str, base_sha: &str, author: &(String, &'static str), f: &Fresh, changed: Option<Vec<String>>) -> (Event, Event) {
    let pr = PullRequest {
        number: u64::from(f.number),
        title: Some(f.title.clone()),
        body: Some(f.summary.clone()),
        draft: Some(f.draft),
        state: "open".into(),
        author: Some(author.0.clone()),
        labels: Some(Vec::new()),
        head: Side { branch: f.branch.clone().unwrap_or_else(|| format!("D{}", f.number)), sha: f.sha.clone(), repo: ctx.clone() },
        base: Side { branch: trunk.to_owned(), sha: base_sha.to_owned(), repo: ctx.clone() },
    };
    let mut e = Event {
        name: "pull_request".into(),
        action: if f.n == 1 { "opened" } else { "synchronize" }.into(),
        repo: repo.name.clone(),
        git_ref: format!("refs/pull/{}/merge", f.number),
        sha: f.sha.clone(),
        base_ref: Some(trunk.to_owned()),
        head_ref: Some(f.branch.clone().unwrap_or_else(|| format!("D{}", f.number))),
        actor: author.0.clone(),
        trusted: true,
        source: Source::Native,
        changed,
        message: Some(f.message.clone()),
        ..Event::default()
    };
    e.payload = context::pull_request(&e, ctx, &pr);
    let (key, name) = if f.branch.is_some() { ("change_request", format!("CR{}", f.number)) } else { ("diff", format!("D{}", f.number)) };
    if let Some(p) = e.payload.as_object_mut() {
        p.insert("kiln".into(), json!({ key: { "number": f.number, "id": name, "version": f.n, "author": author.0, "author_kind": author.1, "draft": f.draft, "rebase_only": f.rebase_only, "ref": format!("refs/kiln/diffs/{name}/v{}", f.n), "branch": f.branch } }));
    }
    let d = Event { name: "diff".into(), action: String::new(), ..e.clone() };
    (e, d)
}

/// Starts a new version's workflows: those the engine runs whose `on` takes either event (a
/// change request's: `pull_request`).
pub(crate) async fn checks(hub: &Hub, repo: &Repo, trunk: &str, base_sha: &str, author: &(String, &'static str), f: &Fresh) -> anyhow::Result<()> {
    if !hub.ci.enabled() {
        return Ok(());
    }
    let ctx = super::events::facts(hub, repo).await.repo;
    let changed = gitops::changed_files(hub, repo, &f.parent, &f.sha).await;
    let (pr, diff) = events_of(repo, &ctx, trunk, base_sha, author, f, changed);
    let commit = super::load::load(hub, repo, &f.sha).await?;
    for l in commit.workflows.iter().filter(|l| l.runs && (l.dialect == kiln_ci::expr::Dialect::Gha || hub.cfg.ci.kiln_workflows)) {
        let both = [&pr, &diff];
        for e in both.iter().take(if f.branch.is_some() { 1 } else { 2 }) {
            super::events::start(hub, repo, e, l, &f.message, &author.0, Value::Null).await?;
        }
    }
    Ok(())
}

/// A new version of `d` that kiln made (a restack onto a moved trunk, W14-R08): kept at its ref,
/// its checks started. Its number.
pub(crate) async fn add_version(hub: &Arc<Hub>, repo: &Repo, path: &str, d: &Diff, v: NewVersion) -> anyhow::Result<u32> {
    let (id, nv) = (d.id.clone(), v.clone());
    let n: u32 = hub
        .store
        .call(move |c| {
            let tx = c.transaction()?;
            let n: u32 = tx.query_row("SELECT COALESCE(MAX(n), 0) + 1 FROM diff_versions WHERE diff_id = ?1", [&id], |r| r.get(0))?;
            let now = crate::util::now_unix();
            tx.execute(
                "INSERT INTO diff_versions (diff_id, n, commit_sha, parent, base, patch_id, message, note, rebase_only, pushed_by, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![id, n, nv.commit, nv.parent, nv.base, nv.patch_id, nv.message, nv.note, nv.rebase_only, nv.pushed_by, now],
            )?;
            tx.execute("UPDATE diffs SET parent_diff = ?2, updated_at = ?3 WHERE id = ?1", params![id, nv.parent_diff, now])?;
            tx.commit()?;
            Ok(n)
        })
        .await?;
    gitops::run_git(hub, &["-C", path, "update-ref", &format!("refs/kiln/diffs/{}/v{n}", d.name), &v.commit], LS_REMOTE_TIMEOUT).await?;
    let (title, summary) = review::title_and_summary(&v.message);
    let f = Fresh { number: d.number, n, sha: v.commit.clone(), parent: v.parent.clone(), title, summary, message: v.message.clone(), draft: d.state == "draft", rebase_only: v.rebase_only, branch: d.branch.clone() };
    let (hub2, repo2, trunk, author, base) = (hub.clone(), repo.clone(), d.trunk.clone(), (d.author.clone(), if d.author_kind == "agent" { "agent" } else { "human" }), v.base.clone());
    crate::rt::spawn(async move {
        if let Err(e) = checks(&hub2, &repo2, &trunk, &base, &author, &f).await {
            tracing::warn!(diff = f.number, version = f.n, error = %format!("{e:#}"), "a diff's workflows didn't start");
        }
    });
    Ok(n)
}

/// A version kiln makes.
#[derive(Debug, Clone)]
pub(crate) struct NewVersion {
    pub commit: String,
    pub parent: String,
    pub base: String,
    pub patch_id: String,
    pub message: String,
    pub note: Option<String>,
    pub rebase_only: bool,
    pub pushed_by: String,
    pub parent_diff: Option<String>,
}

/// `pull_request` `closed` for a diff that landed (`merged`) or was abandoned (W03-R09's table).
pub(crate) async fn closed(hub: &Hub, repo: &Repo, d: &Diff, merged: bool) -> anyhow::Result<()> {
    if !hub.ci.enabled() {
        return Ok(());
    }
    let Some(v) = versions(hub, d).await?.pop() else { return Ok(()) };
    let ctx = super::events::facts(hub, repo).await.repo;
    let (title, summary) = (d.title.clone(), d.summary.clone());
    let f = Fresh { number: d.number, n: v.n, sha: v.commit.clone(), parent: v.parent.clone(), title, summary, message: String::new(), draft: false, rebase_only: v.rebase_only, branch: d.branch.clone() };
    let author = (d.author.clone(), if d.author_kind == "agent" { "agent" } else { "human" });
    let (mut e, _) = events_of(repo, &ctx, &d.trunk, &v.base, &author, &f, None);
    e.action = "closed".into();
    if let Some(p) = e.payload.pointer_mut("/pull_request").and_then(Value::as_object_mut) {
        p.insert("state".into(), json!("closed"));
        p.insert("merged".into(), json!(merged));
    }
    let commit = super::load::load(hub, repo, &v.commit).await?;
    for l in commit.workflows.iter().filter(|l| l.runs && (l.dialect == kiln_ci::expr::Dialect::Gha || hub.cfg.ci.kiln_workflows)) {
        super::events::start(hub, repo, &e, l, &d.title, &d.author, Value::Null).await?;
    }
    Ok(())
}

// ------------------------------------------------------------------ reads

/// A diff as the API shows it.
#[derive(Debug, Clone, Serialize)]
pub struct Diff {
    pub id: String,
    /// `D12`.
    pub name: String,
    pub number: u32,
    pub kind: String,
    pub repo: String,
    pub change_id: Option<String>,
    pub title: String,
    pub summary: String,
    pub author: String,
    pub author_kind: String,
    pub state: String,
    pub trunk: String,
    pub stack: Option<String>,
    /// The diff below it in its stack.
    pub parent: Option<String>,
    pub topic: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    /// A change request's branch.
    pub branch: Option<String>,
    /// Whom the author acts for: an agent's person.
    pub author_for: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Version {
    pub n: u32,
    pub commit: String,
    pub parent: String,
    pub base: String,
    pub rebase_only: bool,
    pub pushed_by: String,
    pub note: Option<String>,
    pub created_at: i64,
    /// Where it is kept: `refs/kiln/diffs/D12/v3`.
    #[serde(rename = "ref")]
    pub git_ref: String,
}

const DIFF_COLUMNS: &str = "d.id, d.number, d.kind, p.name, d.change_id, d.title, d.summary, d.author, d.author_kind, d.state, d.trunk, d.stack_id, pd.number, d.topic, d.created_at, d.updated_at, d.branch, d.author_for";
const DIFF_FROM: &str = "diffs d JOIN repos p ON p.id = d.repo_id LEFT JOIN diffs pd ON pd.id = d.parent_diff";

fn diff_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Diff> {
    let number: u32 = r.get(1)?;
    let kind: String = r.get(2)?;
    let parent: Option<u32> = r.get(12)?;
    Ok(Diff {
        id: r.get(0)?,
        name: if kind == "diff" { format!("D{number}") } else { format!("CR{number}") },
        number,
        kind,
        repo: r.get(3)?,
        change_id: r.get(4)?,
        title: r.get(5)?,
        summary: r.get(6)?,
        author: r.get(7)?,
        author_kind: r.get(8)?,
        state: r.get(9)?,
        trunk: r.get(10)?,
        stack: r.get(11)?,
        parent: parent.map(|n| format!("D{n}")),
        topic: r.get(13)?,
        created_at: r.get(14)?,
        updated_at: r.get(15)?,
        branch: r.get(16)?,
        author_for: r.get(17)?,
    })
}

/// What a key names: (kind, number) for `D12`, `CR4`, or a bare number (any kind).
pub fn key_of(key: &str) -> (Option<&'static str>, Option<u32>) {
    let k = key.trim().to_ascii_uppercase();
    let (kind, num) = if let Some(n) = k.strip_prefix("CR") { (Some("change_request"), n) } else if let Some(n) = k.strip_prefix('D') { (Some("diff"), n) } else { (None, k.as_str()) };
    (kind, num.parse().ok())
}

/// The diff or change request `key` names: `D12`, `CR4`, `12`, or its id.
pub async fn find(hub: &Hub, key: &str) -> anyhow::Result<Option<Diff>> {
    let key = key.trim().to_owned();
    let (kind, number) = key_of(&key);
    hub.store
        .call(move |c| {
            let sql = format!("SELECT {DIFF_COLUMNS} FROM {DIFF_FROM} WHERE d.id = ?1 OR (d.number = ?2 AND (?3 IS NULL OR d.kind = ?3))");
            Ok(c.query_row(&sql, params![key, number, kind], diff_row).optional()?)
        })
        .await
}

pub(crate) async fn versions(hub: &Hub, d: &Diff) -> anyhow::Result<Vec<Version>> {
    let (id, name) = (d.id.clone(), d.name.clone());
    hub.store
        .call(move |c| {
            let mut st = c.prepare("SELECT n, commit_sha, parent, base, rebase_only, pushed_by, note, created_at FROM diff_versions WHERE diff_id = ?1 ORDER BY n")?;
            let rows = st.query_map([&id], |r| {
                let n: u32 = r.get(0)?;
                Ok(Version { n, commit: r.get(1)?, parent: r.get(2)?, base: r.get(3)?, rebase_only: r.get(4)?, pushed_by: r.get(5)?, note: r.get(6)?, created_at: r.get(7)?, git_ref: format!("refs/kiln/diffs/{name}/v{n}") })
            })?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
}

async fn reviewers(hub: &Hub, d: &Diff) -> anyhow::Result<Vec<Value>> {
    let id = d.id.clone();
    hub.store
        .call(move |c| {
            let mut st = c.prepare("SELECT reviewer, state FROM diff_reviewers WHERE diff_id = ?1 ORDER BY added_at, reviewer")?;
            let rows = st.query_map([&id], |r| Ok(json!({ "reviewer": r.get::<_, String>(0)?, "state": r.get::<_, String>(1)? })))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
}

/// The native repository a diff lives in, and its bare repository.
pub(crate) async fn home(hub: &Hub, d: &Diff) -> Result<(Repo, String), Fail> {
    let repos = hub.store.repos().await.map_err(fail)?;
    let repo = repos.into_iter().find(|r| r.name == d.repo).ok_or((StatusCode::NOT_FOUND, "the diff's repository is gone".to_owned()))?;
    let path = super::hosting::native_path(hub, &repo).ok_or((StatusCode::CONFLICT, "the diff's repository isn't hosted here any more".to_owned()))?;
    Ok((repo, path.to_string_lossy().into_owned()))
}

fn version_of(list: &[Version], n: &str) -> Result<Version, Fail> {
    let n: u32 = n.trim_start_matches('v').parse().map_err(|_| (StatusCode::BAD_REQUEST, format!("`{n}` isn't a version")))?;
    list.iter().find(|v| v.n == n).cloned().ok_or((StatusCode::NOT_FOUND, format!("no version {n}")))
}

/// The files a version changes (renames found), with the patch, and which of them `viewer`
/// marked viewed (and that haven't changed since).
async fn files(hub: &Hub, d: &Diff, v: &Version, viewer: &str, latest: &Version) -> Result<Value, Fail> {
    let (_, path) = home(hub, d).await?;
    let stat = gitops::run_git(hub, &["-C", &path, "diff", "--numstat", "-z", "-M", &v.parent, &v.commit], LS_REMOTE_TIMEOUT).await.map_err(fail)?;
    let status = gitops::run_git(hub, &["-C", &path, "diff", "--name-status", "-z", "-M", &v.parent, &v.commit], LS_REMOTE_TIMEOUT).await.map_err(fail)?;
    let patch = gitops::run_git(hub, &["-C", &path, "diff", "--no-color", "-M", &v.parent, &v.commit], LS_REMOTE_TIMEOUT).await.map_err(fail)?;
    let mut list = numstat(&String::from_utf8_lossy(&stat));
    let statuses = name_status(&String::from_utf8_lossy(&status));
    let seen = if v.n == latest.n { super::review::viewed_files(hub, d, viewer, latest).await } else { HashMap::new() };
    for f in &mut list {
        let path = f.get("path").and_then(Value::as_str).unwrap_or_default().to_owned();
        if let Some(o) = f.as_object_mut() {
            if let Some(s) = statuses.get(&path) {
                o.insert("status".into(), json!(s));
            }
            o.insert("viewed".into(), json!(seen.contains_key(&path)));
        }
    }
    const MAX: usize = 8 << 20;
    let truncated = patch.len() > MAX;
    let text = String::from_utf8_lossy(patch.get(..MAX.min(patch.len())).unwrap_or_default()).into_owned();
    Ok(json!({ "diff": d.name, "version": v.n, "commit": v.commit, "parent": v.parent, "files": list, "patch": text, "truncated": truncated }))
}

/// `git diff --numstat -z`: `added\tdeleted\tpath\0`, or for a rename `added\tdeleted\t\0old\0new\0`.
fn numstat(out: &str) -> Vec<Value> {
    let mut list = Vec::new();
    let mut parts = out.split('\0');
    while let Some(head) = parts.next() {
        let mut f = head.splitn(3, '\t');
        let (Some(a), Some(dl), Some(p)) = (f.next(), f.next(), f.next()) else { continue };
        let binary = a == "-" && dl == "-";
        let (old, path) = if p.is_empty() { (parts.next().map(str::to_owned), parts.next().unwrap_or_default().to_owned()) } else { (None, p.to_owned()) };
        list.push(json!({ "path": path, "old_path": old, "added": a.parse::<u64>().ok(), "deleted": dl.parse::<u64>().ok(), "binary": binary, "status": "M" }));
    }
    list
}

/// `git diff --name-status -z`: `M\0path\0`, `R100\0old\0new\0`: path → status letter.
fn name_status(out: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let mut parts = out.split('\0').filter(|s| !s.is_empty());
    while let Some(st) = parts.next() {
        let letter = st.get(..1).unwrap_or("M").to_owned();
        if matches!(letter.as_str(), "R" | "C") {
            let _old = parts.next();
        }
        if let Some(p) = parts.next() {
            map.insert(p.to_owned(), letter);
        }
    }
    map
}

/// What changed between two versions, rebase noise removed (`git range-diff`, the two always
/// paired: they are one diff, however much it changed).
async fn interdiff(hub: &Hub, d: &Diff, a: &Version, b: &Version) -> Result<Value, Fail> {
    let (_, path) = home(hub, d).await?;
    let out = gitops::run_git(hub, &["-C", &path, "range-diff", "--no-color", "--creation-factor=100", &format!("{}..{}", a.parent, a.commit), &format!("{}..{}", b.parent, b.commit)], LS_REMOTE_TIMEOUT).await.map_err(fail)?;
    // The patch itself between them too: what a reviewer reads first.
    let patch_a = gitops::run_git(hub, &["-C", &path, "diff", "--no-color", &a.parent, &a.commit], LS_REMOTE_TIMEOUT).await.map_err(fail)?;
    let patch_b = gitops::run_git(hub, &["-C", &path, "diff", "--no-color", &b.parent, &b.commit], LS_REMOTE_TIMEOUT).await.map_err(fail)?;
    Ok(json!({ "diff": d.name, "from": a.n, "to": b.n, "same_patch": patch_a == patch_b, "range_diff": String::from_utf8_lossy(&out) }))
}

async fn stack(hub: &Hub, id: &str) -> Result<Value, Fail> {
    let id = id.to_owned();
    let list = hub
        .store
        .call(move |c| {
            let sql = format!("SELECT {DIFF_COLUMNS} FROM {DIFF_FROM} WHERE d.stack_id = ?1 ORDER BY d.number");
            let mut st = c.prepare(&sql)?;
            let rows = st.query_map([&id], diff_row)?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
        .map_err(fail)?;
    if list.is_empty() {
        return Err((StatusCode::NOT_FOUND, "no such stack".into()));
    }
    // Bottom to top: each diff after the one below it (a tree when it forked).
    let mut ordered: Vec<Diff> = Vec::new();
    let mut left = list;
    let mut below: Vec<Option<String>> = vec![None];
    while !left.is_empty() {
        let (next, rest): (Vec<Diff>, Vec<Diff>) = left.into_iter().partition(|d| below.contains(&d.parent));
        if next.is_empty() {
            // Whose parent isn't in this stack (it moved): after the rest, as they come.
            ordered.extend(rest);
            break;
        }
        below = next.iter().map(|d| Some(d.name.clone())).collect();
        ordered.extend(next);
        left = rest;
    }
    Ok(json!({ "id": ordered.first().and_then(|d| d.stack.clone()), "trunk": ordered.first().map(|d| d.trunk.clone()), "repo": ordered.first().map(|d| d.repo.clone()), "diffs": ordered }))
}

/// A repository's diffs (or change requests), newest first: `state`, `author`, `reviewer` filter.
async fn list(hub: &Hub, repo: &str, q: &HashMap<String, String>, kind: &'static str) -> Result<Value, Fail> {
    let (repo, state, author, reviewer) = (repo.to_owned(), q.get("state").cloned(), q.get("author").cloned(), q.get("reviewer").cloned());
    let limit: i64 = q.get("limit").and_then(|l| l.parse().ok()).unwrap_or(100).clamp(1, 1000);
    let rows = hub
        .store
        .call(move |c| {
            let sql = format!(
                "SELECT {DIFF_COLUMNS} FROM {DIFF_FROM} WHERE (p.name = ?1 OR p.id = ?1) AND d.kind = ?6 AND (?2 IS NULL OR d.state = ?2 OR (?2 = 'open' AND d.state NOT IN ('landed', 'abandoned')))
                 AND (?3 IS NULL OR d.author = ?3) AND (?4 IS NULL OR EXISTS (SELECT 1 FROM diff_reviewers r WHERE r.diff_id = d.id AND r.reviewer = ?4))
                 ORDER BY d.number DESC LIMIT ?5"
            );
            let mut st = c.prepare(&sql)?;
            let rows = st.query_map(params![repo, state, author, reviewer, limit, kind], diff_row)?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
        .map_err(fail)?;
    Ok(if kind == "diff" { json!({ "diffs": rows }) } else { json!({ "change_requests": rows }) })
}

/// A diff whole: it, its versions, its reviewers, and the runs on its latest version.
async fn get(hub: &Hub, d: &Diff) -> Result<Value, Fail> {
    let vs = versions(hub, d).await.map_err(fail)?;
    let rs = reviewers(hub, d).await.map_err(fail)?;
    let latest = vs.last().map(|v| v.commit.clone()).unwrap_or_default();
    let repo = d.repo.clone();
    let runs = hub
        .store
        .call(move |c| {
            let mut st = c.prepare("SELECT r.id, r.number, r.pipeline, r.run_name, r.event, r.status, r.conclusion FROM runs r JOIN repos p ON p.id = r.repo_id WHERE p.name = ?1 AND r.sha = ?2 ORDER BY r.created_at DESC LIMIT 100")?;
            let rows = st.query_map(params![repo, latest], |r| {
                Ok(json!({ "id": r.get::<_, String>(0)?, "number": r.get::<_, Option<i64>>(1)?, "workflow": r.get::<_, Option<String>>(2)?, "name": r.get::<_, Option<String>>(3)?, "event": r.get::<_, String>(4)?, "status": r.get::<_, String>(5)?, "conclusion": r.get::<_, Option<String>>(6)? }))
            })?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
        .map_err(fail)?;
    let commits = match (d.kind.as_str(), vs.last(), home(hub, d).await) {
        ("change_request", Some(l), Ok((_, path))) => json!(super::cr::commits(hub, &path, &l.base, &l.commit).await),
        _ => Value::Null,
    };
    let mut v = serde_json::to_value(d).unwrap_or(Value::Null);
    if let Some(o) = v.as_object_mut() {
        if !commits.is_null() {
            o.insert("commits".into(), commits);
        }
        o.insert("versions".into(), json!(vs));
        o.insert("reviewers".into(), json!(rs));
        o.insert("runs".into(), json!(runs));
    }
    Ok(v)
}

/// The author's actions (and commandeering): `ready`, `abandon`, `reclaim`, `commandeer`.
async fn act(hub: &Hub, d: &Diff, action: &str, caller: &Caller) -> Result<Value, Fail> {
    let who = author_of(&caller.name, Some(caller.kind)).0;
    let theirs = caller.is_owner() || d.author.eq_ignore_ascii_case(&who);
    let state = State::parse(&d.state).unwrap_or(State::NeedsReview);
    let (to, author) = match action {
        "commandeer" if state.is_open() => (state, Some(who.clone())),
        "commandeer" => return Err((StatusCode::CONFLICT, format!("{} is {}: reclaim it first", d.name, d.state))),
        _ if !theirs => return Err((StatusCode::FORBIDDEN, format!("{} is {}'s: commandeer it first", d.name, d.author))),
        "ready" if state == State::Draft => (State::NeedsReview, None),
        "ready" => return Err((StatusCode::CONFLICT, format!("{} is {}, not a draft", d.name, d.state))),
        "abandon" if state.is_open() && state != State::Landing => (State::Abandoned, None),
        "abandon" => return Err((StatusCode::CONFLICT, format!("{} is {}", d.name, d.state))),
        "reclaim" if state == State::Abandoned => (State::NeedsReview, None),
        "reclaim" => return Err((StatusCode::CONFLICT, format!("{} isn't abandoned", d.name))),
        other => return Err((StatusCode::NOT_FOUND, format!("no action `{other}`"))),
    };
    let (id, to_s, author_c, for_c) = (d.id.clone(), to.as_str(), author.clone(), acting_for(&caller.name));
    hub.store
        .call(move |c| {
            c.execute(
                "UPDATE diffs SET state = ?2, author = COALESCE(?3, author), author_for = CASE WHEN ?3 IS NULL THEN author_for ELSE ?5 END, updated_at = ?4 WHERE id = ?1",
                params![id, to_s, author_c, crate::util::now_unix(), for_c],
            )?;
            Ok(())
        })
        .await
        .map_err(fail)?;
    hub.store.audit(caller.name.clone(), format!("diffs.{action}"), format!("{} {}", d.repo, d.name)).await;
    Ok(json!({ "diff": d.name, "state": to.as_str(), "author": author.unwrap_or_else(|| d.author.clone()) }))
}

/// `/api/diffs/<id>[/…]`, `/api/repos/<repo>/diffs` and `/api/stacks/<id>`; the route table
/// (W10-R05) has let the caller through for the diff's repository.
pub async fn route(hub: &Arc<Hub>, method: &Method, segs: &[&str], q: &HashMap<String, String>, caller: &Caller, body: &Value) -> Result<Response, Fail> {
    let ok = |v: Value| Ok(srv::json(StatusCode::OK, &v));
    match (method, segs) {
        (&Method::GET, ["repos", repo, "diffs"]) => ok(list(hub, repo, q, "diff").await?),
        (&Method::GET, ["repos", repo, "change-requests"]) => ok(list(hub, repo, q, "change_request").await?),
        (&Method::POST, ["repos", repo, "change-requests"]) => ok(super::cr::create(hub, repo, caller, body).await?),
        (&Method::POST, ["repos", repo, "submit"]) => ok(submit_patches(hub, repo, caller, body).await?),
        (&Method::POST, ["comments", id, "apply"]) => ok(super::review::apply_suggestion(hub, id, caller).await?),
        (&Method::GET, ["repos", repo, "queue"]) => ok(super::land::list(hub, repo).await?),
        (&Method::GET, ["stacks", id]) => ok(stack(hub, id).await?),
        (&Method::GET, ["review", "inbox"]) => ok(super::review::inbox(hub, caller).await?),
        (&Method::PATCH, ["comments", id]) => ok(super::review::edit(hub, id, caller, body).await?),
        (&Method::DELETE, ["queue", id]) => ok(super::land::cancel(hub, id, caller).await?),
        (_, ["diffs" | "change-requests", key, rest @ ..]) => {
            let d = find(hub, key).await.map_err(fail)?.ok_or((StatusCode::NOT_FOUND, format!("no diff {key}")))?;
            // A caller sees a diff where they may read diffs (the route table checked it there).
            if !caller.can("diffs.read", &Scope::Repo { repo: d.repo.clone() }) {
                return Err((StatusCode::NOT_FOUND, format!("no diff {key}")));
            }
            match (method, rest) {
                (&Method::GET, []) => ok(get(hub, &d).await?),
                (&Method::GET, ["versions"]) => ok(json!({ "versions": versions(hub, &d).await.map_err(fail)? })),
                (&Method::GET, ["versions", n, "files"]) => {
                    let vs = versions(hub, &d).await.map_err(fail)?;
                    let latest = vs.last().cloned().ok_or((StatusCode::NOT_FOUND, "no versions".to_owned()))?;
                    ok(files(hub, &d, &version_of(&vs, n)?, &super::review::who(caller).0, &latest).await?)
                }
                (&Method::GET, ["interdiff"]) => {
                    let vs = versions(hub, &d).await.map_err(fail)?;
                    let last = vs.last().map(|v| v.n.to_string()).unwrap_or_default();
                    let from = version_of(&vs, q.get("from").map_or("1", String::as_str))?;
                    let to = version_of(&vs, q.get("to").map_or(last.as_str(), String::as_str))?;
                    ok(interdiff(hub, &d, &from, &to).await?)
                }
                (&Method::GET, ["comments"]) => ok(json!({ "threads": super::review::threads(hub, &d, &super::review::who(caller).0).await? })),
                (&Method::POST, ["comments"]) => ok(super::review::comment(hub, &d, caller, body).await?),
                (&Method::POST, ["review"]) => ok(super::review::review(hub, &d, caller, body).await?),
                (&Method::POST, ["viewed"]) => ok(super::review::viewed(hub, &d, caller, body).await?),
                (&Method::GET, ["land"]) => ok(super::review::readiness(hub, &d).await?),
                (&Method::POST, ["land"]) => {
                    if !caller.can("diffs.land", &Scope::Repo { repo: d.repo.clone() }) {
                        return Err((StatusCode::FORBIDDEN, "landing needs `diffs.land`".into()));
                    }
                    ok(super::land::enqueue(hub, &d, caller, body).await?)
                }
                (&Method::POST, [action @ ("ready" | "abandon" | "reclaim" | "commandeer")]) => {
                    if !caller.can("diffs.submit", &Scope::Repo { repo: d.repo.clone() }) {
                        return Err((StatusCode::FORBIDDEN, format!("{action} needs `diffs.submit`")));
                    }
                    ok(act(hub, &d, action, caller).await?)
                }
                _ => Err((StatusCode::NOT_FOUND, "not found".into())),
            }
        }
        _ => Err((StatusCode::NOT_FOUND, "not found".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w14_r03_files() {
        let list = numstat(concat!("3\t1\tsrc/a.rs\0", "-\t-\tlogo.png\0", "2\t0\t\0old.rs\0new.rs\0"));
        let paths: Vec<&str> = list.iter().filter_map(|f| f.get("path").and_then(Value::as_str)).collect();
        assert_eq!(paths, ["src/a.rs", "logo.png", "new.rs"]);
        assert_eq!(list.get(1).and_then(|f| f.get("binary")), Some(&json!(true)));
        assert_eq!(list.get(2).and_then(|f| f.get("old_path")), Some(&json!("old.rs")));
        let st = name_status("M\0src/a.rs\0A\0logo.png\0R095\0old.rs\0new.rs\0");
        assert_eq!((st.get("src/a.rs").map(String::as_str), st.get("new.rs").map(String::as_str)), (Some("M"), Some("R")));
        assert_eq!(st.get("logo.png").map(String::as_str), Some("A"));
    }

    #[test]
    fn w14_r03_authors() {
        use crate::authz::gate::Kind;
        assert_eq!(author_of("pat:abc of user:ada", Some(Kind::Pat)), ("user:ada".to_owned(), "human"));
        assert_eq!(author_of("agent:review for user:ada", Some(Kind::Agent)), ("agent:review".to_owned(), "agent"));
        assert_eq!(author_of("user:ada", Some(Kind::User)), ("user:ada".to_owned(), "human"));
        assert_eq!(author_of("job:L1", None), ("job:L1".to_owned(), "agent"));
    }
}
