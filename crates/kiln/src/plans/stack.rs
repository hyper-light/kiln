//! A plan run's stack (docs/agentic.md P4): as each step is met, what its agent changed becomes one
//! commit on the step before's and goes up for review, so a run whose steps ran in order ends as a
//! stack with one diff per step.
//!
//! - **A repository kiln hosts:** each step's commit, with a Change-Id of its own, is pushed to
//!   `refs/for/<trunk>` with its own agent's token: kiln makes it a diff whose author is that
//!   step's agent, on the diff of the step before (W14-R03). Comments, reviews, CI and the land
//!   queue then find their way back to the agent that did the step (`crate::workspace::follow`).
//! - **Elsewhere** (GitHub, GitLab, Bitbucket): a branch per step, each pull request onto the
//!   branch of the step before (the first onto the trunk), opened when kiln has an integration
//!   there, else a link to open it.
//!
//! The commits are made beside the folder (`crate::workspace::git`): the person's branch, index
//! and working tree stay as they were. A step's commit has the files its agent changed, as they
//! were when the step was met. A step that changed nothing has no diff; one whose commit couldn't
//! be made hands its files on to the next, so the stack never leaves work out.
//!
//! While the run goes on, what review and CI say to a step's agent waits ([`held`]): the next
//! step's agent works in the same folder. Once the run ends it goes to the agent, which makes a
//! new version of its diff with `agents.change` ([`amend`]): its changes since go into its own
//! step, and the steps above are made again on it.

use std::sync::Arc;

use http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::harness::session::Info;
use crate::hub::Hub;
use crate::ops::Fail;
use crate::store::Repo;
use crate::workspace::change::{self, InStack, Record, Want};
use crate::workspace::git::{Facts, Ident};
use crate::workspace::{Ask, Push, Restack, Stacked};

/// Where a run's stack is kept (`kv`).
fn key(run: &str) -> String {
    format!("plans/stack/{run}")
}

/// What waits for a run to end (`kv`).
fn held_key(run: &str) -> String {
    format!("plans/stack-held/{run}")
}

/// One stack changes at a time (a step met while a person says another is, an amend).
static CHANGING: futures::lock::Mutex<()> = futures::lock::Mutex::new(());
/// One list of held messages changes at a time.
static HOLDING: futures::lock::Mutex<()> = futures::lock::Mutex::new(());

/// A run's stack, as kiln keeps it.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Stack {
    pub run: String,
    /// Why the run has no stack (its folder isn't in a git repository, …).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub off: Option<String>,
    /// The machine and the repository's top its steps worked in (the first step's).
    pub machine: String,
    pub top: String,
    /// The repository as kiln knows it, when it does.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// `kiln`, `github`, `gitlab`, `bitbucket`, `git` (a remote kiln has no integration with) or
    /// `local` (no remote: the commits stay in the folder's repository).
    pub provider: String,
    pub trunk: String,
    /// Where its commits are pushed: a URL or a remote's name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    /// The remote's URL, for links that open pull requests.
    pub remote_url: String,
    /// The commit the stack is on: the folder's last when its first step was met.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    pub steps: Vec<Entry>,
    pub updated_at: i64,
}

/// One step in the stack.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Entry {
    /// Its step, from 1.
    pub n: usize,
    pub title: String,
    pub agent: String,
    /// `made` (its commit is in the stack), `nothing` (its agent changed no files), `failed` (its
    /// files go with the next step's), `elsewhere` (its agent worked in another folder).
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// The files it changes (`made`), or those it hands on (`failed`).
    pub files: Vec<String>,
    pub message: String,
    pub change_id: String,
    /// Its branch, where its provider takes a branch per step.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Its commit has reached where the stack goes.
    pub pushed: bool,
    /// `D12`, `#34`; none until it is open.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub change: Option<String>,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// A message to a step's agent that waits for its run to end.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Held {
    pub agent: String,
    pub text: String,
}

/// Run `run`'s stack, when it has one.
pub async fn of(hub: &Hub, run: &str) -> Option<Stack> {
    let text = hub.store.kv_get(key(run)).await.ok().flatten()?;
    serde_json::from_str(&text).ok()
}

async fn save(hub: &Hub, st: &Stack) {
    if let Ok(text) = serde_json::to_string(st) {
        let _ = hub.store.kv_set(key(&st.run), text).await;
    }
    hub.emit(&json!({"type": "plan_stack", "run": st.run}));
}

/// A stack as a run's page shows it.
pub fn shown(st: &Stack) -> Value {
    json!({
        "off": st.off,
        "provider": st.provider,
        "repo": st.repo,
        "trunk": st.trunk,
        "base": st.base,
        "steps": st.steps.iter().map(|e| json!({
            "n": e.n,
            "state": e.state,
            "change": e.change,
            "url": e.url,
            "sha": e.sha,
            "branch": e.branch,
            "files": e.files,
            "pushed": e.pushed,
            "error": e.error,
        })).collect::<Vec<_>>(),
    })
}

/// A step that was just met.
pub struct Met<'a> {
    pub run: &'a str,
    pub n: usize,
    pub total: usize,
    pub title: &'a str,
    pub goal: &'a str,
    pub plan: &'a str,
    pub agent: Option<&'a str>,
    /// Whom the run's agents act for (`owner`, `user:<id>`).
    pub opener: Option<&'a str>,
}

/// Step `m.n` of run `m.run` is met: its agent's work becomes its commit in the run's stack and
/// goes up for review. Never fails: what went wrong is kept with the step, for its page.
pub async fn met(hub: &Arc<Hub>, m: Met<'_>) {
    let _one = CHANGING.lock().await;
    let mut st = of(hub, m.run).await.unwrap_or_else(|| Stack { run: m.run.to_owned(), ..Stack::default() });
    if st.off.is_some() || st.steps.iter().any(|e| e.n == m.n && e.state == "made") {
        return;
    }
    let entry = make(hub, &mut st, &m).await;
    if let Some(e) = &entry.error {
        tracing::info!(run = %m.run, step = m.n, error = %e, "a plan step's work didn't go up for review");
    }
    st.steps.retain(|e| e.n != m.n);
    st.steps.push(entry);
    st.steps.sort_by_key(|e| e.n);
    opened(hub, &mut st).await;
    st.updated_at = crate::util::now_unix();
    save(hub, &st).await;
}

/// The repository agent `info`'s folder is in, as its machine sees it.
async fn facts_of(hub: &Hub, info: &Info) -> Result<Facts, String> {
    match change::ask_machine(hub, info, &Ask::Git).await {
        Ok(v) => serde_json::from_value(v.get("facts").cloned().unwrap_or_default()).map_err(|e| format!("the machine's answer: {e}")),
        Err((_, m)) => Err(m),
    }
}

/// Where the stack goes, from the folder its first step's agent worked in: `Err` with why there's
/// no stack.
async fn place(hub: &Arc<Hub>, st: &mut Stack, facts: &Facts, info: &Info, plan: &str) -> Result<(), String> {
    let repos = hub.store.repos().await.map_err(|e| format!("{e:#}"))?;
    let found = change::matched(&repos, &facts.remotes, &hub.cfg.server.public_url).map(|(r, name)| (r.clone(), name));
    let host = match &found {
        Some((repo, _)) => crate::githost::for_repo(hub, repo).await,
        None => None,
    };
    st.machine = crate::harness::wait::machine(&info.target).to_owned();
    st.top.clone_from(&facts.top);
    st.repo = found.as_ref().map(|(r, _)| r.name.clone());
    st.trunk = found.as_ref().and_then(|(r, _)| r.default_branch.clone()).or_else(|| facts.default_branch.clone()).or_else(|| facts.branch.clone()).unwrap_or_else(|| "main".to_owned());
    st.base.clone_from(&facts.head);
    let remote = match &found {
        Some((_, name)) => Some(name.clone()),
        None if facts.remotes.iter().any(|r| r.name == "origin") => Some("origin".to_owned()),
        None => facts.remotes.first().map(|r| r.name.clone()),
    };
    st.remote_url = found.as_ref().map_or_else(|| remote.as_ref().and_then(|n| facts.remotes.iter().find(|r| &r.name == n)).map(|r| r.url.clone()).unwrap_or_default(), |(r, _)| r.clone_url.clone());
    match (&host, &found) {
        (Some(crate::githost::GitHost::Kiln), Some((repo, _))) => {
            st.provider = "kiln".to_owned();
            st.to = Some(format!("{}/git/{}.git", hub.cfg.server.public_url.trim_end_matches('/'), repo.name));
        }
        _ => {
            st.provider = if remote.is_some() { change::provider_of(host.as_ref()).to_owned() } else { "local".to_owned() };
            st.to = remote;
        }
    }
    tracing::info!(run = %st.run, plan, provider = %st.provider, trunk = %st.trunk, "a plan run's stack");
    Ok(())
}

/// The ref a step's commit goes to: kiln's review (`refs/for/<trunk>`, the run's diffs under one
/// topic), else its own branch.
fn to_ref(st: &Stack, e: &Entry) -> Option<String> {
    match (st.provider.as_str(), &e.branch) {
        ("kiln", _) => Some(format!("refs/for/{}%topic=plan-{}", st.trunk, change::id_end(&st.run))),
        ("local", _) => None,
        (_, Some(b)) => Some(format!("refs/heads/{b}")),
        (_, None) => None,
    }
}

/// An agent token for one push to the stack's repository as agent `agent`, acting for `opener`:
/// `(token id, token)`.
async fn token(hub: &Hub, st: &Stack, agent: &str, opener: &str) -> Result<(String, String), String> {
    let repo = st.repo.clone().unwrap_or_default();
    let grants = [format!("diffs.read@repo:{repo}"), format!("diffs.submit@repo:{repo}")];
    let grants: Vec<&str> = grants.iter().map(String::as_str).collect();
    crate::authz::tokens::mint_agent(hub, agent, opener, &grants, Some(1), "a plan step's change").await.map_err(|e| format!("an agent token: {e:#}"))
}

async fn revoke(hub: &Hub, ids: Vec<String>) {
    let now = crate::util::now_unix();
    for id in ids {
        let _ = hub.store.call(move |c| Ok(c.execute("UPDATE tokens SET revoked_at = ?2 WHERE id = ?1", rusqlite::params![id, now])?)).await;
    }
}

/// Step `m.n`'s entry: its commit made and pushed, or why not.
async fn make(hub: &Arc<Hub>, st: &mut Stack, m: &Met<'_>) -> Entry {
    let earlier = st.steps.iter().find(|e| e.n == m.n);
    let mut e = Entry {
        n: m.n,
        title: m.title.to_owned(),
        agent: m.agent.unwrap_or_default().to_owned(),
        state: "failed".to_owned(),
        change_id: earlier.map(|e| e.change_id.clone()).filter(|c| !c.is_empty()).unwrap_or_else(crate::ci::diffs::new_change_id),
        ..Entry::default()
    };
    let Some(info) = m.agent.and_then(|a| hub.harness.info(a)) else {
        e.error = Some("its agent has ended, so what it changed can't be read".to_owned());
        return e;
    };
    let machine = crate::harness::wait::machine(&info.target).to_owned();
    if !st.machine.is_empty() && machine != st.machine {
        e.state = "elsewhere".to_owned();
        e.error = Some(format!("its agent worked on {machine}, not {} where the stack is: its work is a change of its own (Open change request in its Changes)", st.machine));
        return e;
    }
    let facts = match facts_of(hub, &info).await {
        Ok(f) => f,
        Err(why) if st.machine.is_empty() => {
            st.off = Some(why.clone());
            e.error = Some(why);
            return e;
        }
        Err(why) => {
            e.error = Some(why);
            return e;
        }
    };
    if st.machine.is_empty() {
        if let Err(why) = place(hub, st, &facts, &info, m.plan).await {
            st.off = Some(why.clone());
            e.error = Some(why);
            return e;
        }
    } else if facts.top != st.top {
        e.state = "elsewhere".to_owned();
        e.error = Some(format!("its agent worked in {}, not {} where the stack is: its work is a change of its own (Open change request in its Changes)", facts.top, st.top));
        return e;
    }
    // Its files, and those of the steps since the last commit whose own didn't go.
    let mut paths = change::paths_of(hub, &info.id);
    for f in st.steps.iter().rev().take_while(|x| x.state != "made").filter(|x| x.state == "failed").flat_map(|x| x.files.iter()) {
        if !paths.contains(f) {
            paths.push(f.clone());
        }
    }
    e.files.clone_from(&paths);
    if paths.is_empty() {
        e.state = "nothing".to_owned();
        return e;
    }
    let title = if m.title.trim().is_empty() { format!("Step {} of {}", m.n, m.plan.trim()) } else { m.title.trim().to_owned() };
    let body = format!("{title}\n\n{}\n\nKiln-Agent: {} ({}, {})\nKiln-Plan-Run: {}, step {} of {}", m.goal.trim(), info.name, info.kind, info.id, m.run, m.n, m.total);
    e.message = kiln_ci::review::with_change_id(&body, &e.change_id);
    if st.provider != "kiln" && st.provider != "local" {
        e.branch = Some(format!("agents/{}-{}-{}", change::slug(m.plan), change::id_end(m.run), m.n).replace("agents/-", "agents/"));
    }
    let on = st.steps.iter().rev().find(|x| x.state == "made").and_then(|x| x.sha.clone());
    // The pushes: earlier steps' commits that haven't gone yet, each as its own agent, then this one.
    let opener = m.opener.unwrap_or("owner");
    let wanted: Vec<(String, String, Option<String>)> = st
        .steps
        .iter()
        .filter(|x| x.state == "made" && !x.pushed)
        .map(|x| (x.agent.clone(), x.sha.clone().unwrap_or_default(), to_ref(st, x)))
        .chain(std::iter::once((e.agent.clone(), String::new(), to_ref(st, &e))))
        .collect();
    let mut pushes: Vec<Push> = Vec::new();
    let mut minted: Vec<String> = Vec::new();
    for (agent, sha, to_ref) in wanted {
        let Some(to_ref) = to_ref else { continue };
        let bearer = if st.provider == "kiln" {
            match token(hub, st, &agent, opener).await {
                Ok((id, t)) => {
                    minted.push(id);
                    Some(t)
                }
                Err(why) => {
                    e.error = Some(why);
                    break;
                }
            }
        } else {
            None
        };
        pushes.push(Push { sha, to_ref, bearer });
    }
    let ask = Ask::Stack(Stacked {
        on: on.clone(),
        paths,
        message: e.message.clone(),
        fallback: Ident { name: format!("{} (kiln agent)", info.name), email: "agent@kiln.invalid".to_owned() },
        to: st.to.clone(),
        pushes: pushes.clone(),
    });
    let answer = change::ask_machine(hub, &info, &ask).await;
    revoke(hub, minted).await;
    let answer = match answer {
        Ok(a) => a,
        Err((_, why)) => {
            e.error = Some(why);
            return e;
        }
    };
    if answer.get("nothing").and_then(Value::as_bool) == Some(true) {
        e.state = "nothing".to_owned();
        e.files.clear();
        return e;
    }
    let made: crate::workspace::git::Made = match serde_json::from_value(answer.get("made").cloned().unwrap_or_default()) {
        Ok(m) => m,
        Err(err) => {
            e.error = Some(format!("the machine's answer: {err}"));
            return e;
        }
    };
    if st.base.is_none() || on.is_none() {
        st.base.clone_from(&made.parent);
    }
    e.state = "made".to_owned();
    e.sha = Some(made.sha);
    e.parent = made.parent;
    e.files = made.files;
    // What reached the remote: the first `pushed` of the pushes, in order.
    let pushed = answer.get("pushed").and_then(Value::as_u64).and_then(|n| usize::try_from(n).ok()).unwrap_or(0);
    for x in st.steps.iter_mut().filter(|x| x.state == "made" && !x.pushed).take(pushed) {
        x.pushed = true;
        x.error = None;
    }
    e.pushed = st.to.is_some() && pushed == pushes.len() && e.error.is_none();
    if let Some(why) = answer.get("push_error").and_then(Value::as_str) {
        e.error = Some(push_said(st, why));
    }
    e
}

/// What a refused push said, with what to do about it when kiln knows.
fn push_said(st: &Stack, said: &str) -> String {
    if st.provider == "kiln" && said.contains("has no Change-Id") {
        return format!("{said} (the folder's branch has commits {} doesn't have yet: put them up for review or land them first, and the stack goes up with the next step)", st.trunk);
    }
    said.to_owned()
}

/// The changes the stack's pushed steps are, opened where they aren't yet, and each step's agent's
/// record of its own (so review, CI and the land queue find their way back to it).
async fn opened(hub: &Arc<Hub>, st: &mut Stack) {
    let repo: Option<Repo> = match &st.repo {
        Some(name) => hub.store.repos().await.ok().and_then(|rs| rs.into_iter().find(|r| &r.name == name)),
        None => None,
    };
    let base_url = hub.cfg.server.public_url.trim_end_matches('/').to_owned();
    let host = match &repo {
        Some(r) => crate::githost::for_repo(hub, r).await,
        None => None,
    };
    let mut below: Option<String> = None;
    for i in 0..st.steps.len() {
        let Some(e) = st.steps.get(i).cloned() else { continue };
        if e.state != "made" {
            continue;
        }
        let mut version = 1u32;
        let mut review = None;
        let mut change = e.change.clone();
        let mut url = e.url.clone();
        match (st.provider.as_str(), &repo) {
            ("kiln", Some(r)) if e.pushed => {
                if let Some((number, n, state)) = diff_of(hub, &r.id, &st.trunk, &e.change_id).await {
                    change = Some(format!("D{number}"));
                    url = format!("{base_url}/diffs/D{number}");
                    version = n;
                    review = Some(state);
                }
            }
            ("local" | "kiln", _) => {}
            (_, _) if e.pushed && change.is_none() => {
                let base = below.clone().unwrap_or_else(|| st.trunk.clone());
                let branch = e.branch.clone().unwrap_or_default();
                let opened = match (&host, &repo) {
                    (Some(h), Some(r)) => match h.open_change(hub, r, &branch, &base, &e.title, &format!("Step {} of a plan run, on {base}.", e.n)).await {
                        Ok(c) => Some((format!("#{}", c.number), c.url)),
                        Err(err) => {
                            tracing::info!(run = %st.run, step = e.n, error = %format!("{err:#}"), "a step's pull request didn't open: its branch is pushed");
                            None
                        }
                    },
                    _ => None,
                };
                match opened {
                    Some((c, u)) => {
                        change = Some(c);
                        url = u;
                    }
                    None => url = change::compare_url(&st.remote_url, &base, &branch).unwrap_or_default(),
                }
            }
            _ => {}
        }
        below = e.branch.clone();
        if let Some(x) = st.steps.get_mut(i) {
            x.change.clone_from(&change);
            x.url.clone_from(&url);
        }
        // The step's agent's own record of it.
        let now = crate::util::now_unix();
        let previous = change::of(hub, &e.agent).await;
        let mut record = Record {
            agent: e.agent.clone(),
            repo: st.repo.clone(),
            provider: st.provider.clone(),
            change,
            url,
            branch: e.branch.clone().unwrap_or_default(),
            trunk: st.trunk.clone(),
            sha: e.sha.clone().unwrap_or_default(),
            version: if st.provider == "kiln" { version } else { previous.as_ref().map_or(1, |p| if p.sha == e.sha.clone().unwrap_or_default() { p.version } else { p.version.saturating_add(1) }) },
            files: e.files.clone(),
            title: e.title.clone(),
            state: previous.as_ref().map_or_else(|| "open".to_owned(), |p| p.state.clone()),
            review: review.or_else(|| previous.as_ref().and_then(|p| p.review.clone())),
            checks: previous.as_ref().and_then(|p| p.checks.clone()),
            fix_checks: previous.as_ref().is_none_or(|p| p.fix_checks),
            opened_at: previous.as_ref().map_or(now, |p| p.opened_at),
            updated_at: previous.as_ref().map_or(now, |p| p.updated_at),
            stack: Some(InStack { run: st.run.clone(), step: e.n }),
        };
        if previous.as_ref() != Some(&record) {
            record.updated_at = now;
            change::save(hub, &record).await;
        }
    }
}

/// The diff with Change-Id `id` in repository `repo_id` into `trunk`: its number, latest version
/// and review state.
async fn diff_of(hub: &Hub, repo_id: &str, trunk: &str, id: &str) -> Option<(u32, u32, String)> {
    let (repo_id, trunk, id) = (repo_id.to_owned(), trunk.to_owned(), id.to_owned());
    hub.store
        .call(move |c| {
            use rusqlite::OptionalExtension as _;
            Ok(c.query_row(
                "SELECT d.number, (SELECT MAX(n) FROM diff_versions v WHERE v.diff_id = d.id), d.state FROM diffs d WHERE d.repo_id = ?1 AND d.trunk = ?2 AND d.change_id = ?3 ORDER BY d.number DESC LIMIT 1",
                rusqlite::params![repo_id, trunk, id],
                |r| Ok((r.get::<_, u32>(0)?, r.get::<_, Option<u32>>(1)?.unwrap_or(1), r.get::<_, String>(2)?)),
            )
            .optional()?)
        })
        .await
        .ok()
        .flatten()
}

/// Whether `text` for the agent of record `r` waits: its step's run still goes on (the next step's
/// agent works in the same folder). It goes to the agent when the run ends ([`release`]).
pub async fn held(hub: &Hub, r: &Record, text: &str) -> bool {
    let Some(s) = &r.stack else { return false };
    if super::agents::state(hub, &s.run).await.as_deref() != Some("running") {
        return false;
    }
    let _one = HOLDING.lock().await;
    let mut list: Vec<Held> = hub.store.kv_get(held_key(&s.run)).await.ok().flatten().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    list.push(Held { agent: r.agent.clone(), text: text.to_owned() });
    if let Ok(t) = serde_json::to_string(&list) {
        let _ = hub.store.kv_set(held_key(&s.run), t).await;
    }
    true
}

/// Run `run` has ended (done, or stopped): what waited for it goes to the steps' agents.
pub async fn release(hub: &Hub, run: &str) {
    let list: Vec<Held> = {
        let _one = HOLDING.lock().await;
        let list = hub.store.kv_get(held_key(run)).await.ok().flatten().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
        let _ = hub.store.kv_delete(held_key(run)).await;
        list
    };
    for h in list {
        crate::workspace::follow::deliver(hub, &h.agent, h.text).await;
    }
}

/// A new version of agent `info`'s step in the stack of plan run `run`: what it changed since goes
/// into its step's commit, and the steps above are made again on it. `Ok(None)` when its step has
/// no commit in the stack (its run hasn't one, or its step changed nothing), so its work is a
/// change of its own.
pub async fn amend(hub: &Arc<Hub>, actor: &str, info: &Info, want: &Want, run: &str) -> Result<Option<Record>, Fail> {
    let in_run = super::agents::state(hub, run).await;
    let mine = of(hub, run).await.and_then(|st| st.steps.iter().find(|e| e.agent == info.id).map(|e| (e.n, e.state.clone())));
    if in_run.as_deref() == Some("running") {
        return Err((
            StatusCode::CONFLICT,
            match &mine {
                Some((n, _)) => format!("{} did step {n} of a plan run that is still going (the next step works in the same folder): a new version of its diff waits until the run ends", info.name),
                None => format!("{} is a step of a plan run that is still going: its work goes up for review as the step's diff when the step is met", info.name),
            },
        ));
    }
    let Some((n, state)) = mine else { return Ok(None) };
    if state != "made" {
        return Ok(None);
    }
    let _one = CHANGING.lock().await;
    let Some(mut st) = of(hub, run).await else { return Ok(None) };
    let paths = change::paths_of(hub, &info.id);
    if paths.is_empty() {
        return Err((StatusCode::CONFLICT, format!("{} hasn't changed any files", info.name)));
    }
    let from: Vec<usize> = st.steps.iter().enumerate().filter(|(_, e)| e.n >= n && e.state == "made").map(|(i, _)| i).collect();
    let commits: Vec<String> = from.iter().filter_map(|&i| st.steps.get(i).and_then(|e| e.sha.clone())).collect();
    let Some(first) = from.first().and_then(|&i| st.steps.get(i)).cloned() else { return Ok(None) };
    // A new title or body for its step: the same trailers (its Change-Id) under them.
    let message = (want.title.is_some() || want.body.is_some()).then(|| {
        let trailers: String = first.message.trim_end().rsplit("\n\n").next().unwrap_or_default().to_owned();
        let title = want.title.as_deref().map(str::trim).filter(|t| !t.is_empty()).unwrap_or(&first.title);
        let body = want.body.as_deref().map(str::trim).filter(|b| !b.is_empty()).map(|b| format!("\n\n{b}")).unwrap_or_default();
        format!("{title}{body}\n\n{trailers}\n")
    });
    let last = from.len().saturating_sub(1);
    let refs: Vec<Option<String>> = from
        .iter()
        .enumerate()
        .map(|(j, &i)| match st.steps.get(i) {
            Some(e) if st.provider != "kiln" || j == last => to_ref(&st, e),
            _ => None,
        })
        .collect();
    let opener = hub.store.kv_get(crate::mcp::opener_key(&info.id)).await.ok().flatten().unwrap_or_else(|| "owner".to_owned());
    let minted = if st.provider == "kiln" { Some(token(hub, &st, &info.id, &opener).await.map_err(|m| (StatusCode::FORBIDDEN, m))?) } else { None };
    let ask = Ask::Restack(Restack {
        commits,
        paths,
        message: message.clone(),
        fallback: Ident { name: format!("{} (kiln agent)", info.name), email: "agent@kiln.invalid".to_owned() },
        to: st.to.clone(),
        refs: refs.clone(),
        bearer: minted.as_ref().map(|(_, t)| t.clone()),
    });
    let answer = change::ask_machine(hub, info, &ask).await;
    if let Some((id, _)) = minted {
        revoke(hub, vec![id]).await;
    }
    let answer = answer?;
    let made: Vec<crate::workspace::git::Made> = serde_json::from_value(answer.get("made").cloned().unwrap_or_default()).map_err(|e| (StatusCode::BAD_GATEWAY, format!("the machine's answer: {e}")))?;
    let error = answer.get("push_error").and_then(Value::as_str).map(|s| push_said(&st, s));
    for (&i, m) in from.iter().zip(&made) {
        if let Some(e) = st.steps.get_mut(i) {
            e.sha = Some(m.sha.clone());
            e.parent.clone_from(&m.parent);
            e.files.clone_from(&m.files);
            e.pushed = st.to.is_some() && error.is_none();
            e.error.clone_from(&error);
        }
    }
    if let (Some(msg), Some(e)) = (message, from.first().and_then(|&i| st.steps.get_mut(i))) {
        e.title = want.title.clone().filter(|t| !t.trim().is_empty()).unwrap_or_else(|| e.title.clone());
        e.message = msg;
    }
    opened(hub, &mut st).await;
    st.updated_at = crate::util::now_unix();
    save(hub, &st).await;
    if let Some(e) = error {
        return Err((StatusCode::BAD_GATEWAY, format!("step {n}'s new version is made, but it didn't go up: {e}")));
    }
    let record = change::of(hub, &info.id).await.ok_or((StatusCode::INTERNAL_SERVER_ERROR, "the step's record went missing".to_owned()))?;
    hub.store
        .audit(actor.to_owned(), "agent.change".into(), format!("{} ({}): step {n} of plan run {run}, {} v{}", info.name, crate::harness::lineage::short(&info.id), record.change.clone().unwrap_or_default(), record.version))
        .await;
    Ok(Some(record))
}

/// P-AC1 on a hub of its own, against kiln's own git hosting over HTTP, under a timeout: a plan of
/// three steps runs as three agents in order, each starting once the step before is met, and ends
/// as a stack of three diffs, each by its own step's agent. What review says to a step's agent
/// waits while the run goes on; once it ends, a step's agent makes a new version of its diff and
/// the steps above stay as they were. No harness runs (each step's is a program that isn't
/// there): each agent's changeset is set as its machine would report it, and each goal is marked
/// met in the store as its goal stack would.
#[cfg(all(test, unix))]
mod end_to_end {
    use std::path::Path;
    use std::process::Command;
    use std::sync::Arc;
    use std::time::Duration;

    use serde_json::json;

    use crate::plans::agents::{self, Config, Place};
    use crate::store::Repo;

    fn run(f: impl std::future::Future<Output = anyhow::Result<()>> + Send + 'static) -> anyhow::Result<()> {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(crate::rt::block_on(f));
        });
        match rx.recv_timeout(Duration::from_secs(150)) {
            Ok(r) => r,
            Err(_) => anyhow::bail!("the run hung"),
        }
    }

    fn sh(dir: &Path, script: &str) -> anyhow::Result<String> {
        let out = Command::new("sh").arg("-c").arg(script).current_dir(dir).env("GIT_CONFIG_GLOBAL", "/dev/null").env("GIT_CONFIG_NOSYSTEM", "1").env("GIT_TERMINAL_PROMPT", "0").output()?;
        anyhow::ensure!(out.status.success(), "{script}: {}", String::from_utf8_lossy(&out.stderr));
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    }

    /// [`sh`] off the runtime's threads: kiln's own git server answers on them, so a push waiting
    /// on one would wait for ever.
    async fn sh_off(dir: &Path, script: &str) -> anyhow::Result<String> {
        let (dir, script) = (dir.to_path_buf(), script.to_owned());
        crate::rt::blocking(move || sh(&dir, &script)).await?
    }

    /// The diffs kiln made, by number: their author, the number of the one below, their stack and
    /// their latest version.
    async fn diffs(hub: &crate::hub::Hub) -> anyhow::Result<Vec<(u32, String, Option<u32>, String, u32)>> {
        hub.store
            .call(|c| {
                let mut st = c.prepare(
                    "SELECT d.number, d.author, (SELECT p.number FROM diffs p WHERE p.id = d.parent_diff), COALESCE(d.stack_id, ''), (SELECT MAX(n) FROM diff_versions v WHERE v.diff_id = d.id) FROM diffs d ORDER BY d.number",
                )?;
                let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get::<_, Option<u32>>(4)?.unwrap_or(0))))?.collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .await
    }

    #[test]
    fn p_ac1_a_plan_s_steps_end_as_a_stack_of_diffs_each_by_its_own_agent() -> anyhow::Result<()> {
        run(async {
            // kiln's API on a port of its own: the machine pushes to its git as an agent would.
            let tcp = hyper_rt::tcp::TcpListener::bind(std::net::SocketAddr::from(([127, 0, 0, 1], 0)), 16).map_err(|e| anyhow::anyhow!("{e}"))?;
            let port = tcp.local_addr().map_err(|e| anyhow::anyhow!("{e}"))?.port();
            let hub = crate::hub::tests::scratch_hub_with(move |c| {
                c.terminal.enabled = true;
                c.server.public_url = format!("http://127.0.0.1:{port}");
            })
            .await?;
            let api = Arc::new(crate::api::Api { hub: Arc::clone(&hub) });
            crate::rt::spawn(async move {
                while let Ok(stream) = tcp.accept().await {
                    let (api, remote) = (Arc::clone(&api), stream.peer_addr().ok());
                    crate::rt::spawn(async move { crate::http::server::serve_accepted(stream, api, remote, None).await });
                }
            });
            let h = Arc::clone(&hub);
            crate::rt::spawn(async move {
                let _ = crate::harness::registry::Registry::run_local(h).await;
            });
            let h = Arc::clone(&hub);
            crate::rt::spawn(async move {
                let _ = crate::workspace::follow::follow_loop(h).await;
            });
            hub.harness.set_test_program("/nonexistent/kiln-test-no-harness");
            // A repository kiln hosts, its trunk pushed by its owner from a person's folder.
            let path = crate::ci::hosting::create(&hub, "acme/app", "main", None).await?;
            let repo = Repo { id: crate::util::new_id(), name: "acme/app".into(), clone_url: crate::ci::hosting::clone_url(&path), branches: Vec::new(), poll_seconds: 3600, enabled: true, created_at: 0, last_polled_at: None, last_error: None, mode: "native".into(), default_branch: Some("main".into()), mirror: None };
            hub.store.upsert_repo(repo).await?;
            let work = tempfile::tempdir()?;
            let url = format!("http://127.0.0.1:{port}/git/acme/app.git");
            sh_off(
                work.path(),
                &format!("git init -q -b main && git config user.name Ada && git config user.email ada@example.com && git config commit.gpgsign false && git config tag.gpgsign false && printf 'app\\n' > README.md && git add . && git commit -qm first && git remote add origin {url} && git -c 'http.extraHeader=Authorization: Bearer test-admin-token' push -q origin main"),
            )
            .await?;
            let head = sh_off(work.path(), "git rev-parse HEAD").await?;
            let cwd = work.path().to_string_lossy().into_owned();
            let cx = crate::ops::Cx { hub: Arc::clone(&hub), caller: crate::authz::Caller::owner() };
            // A plan of three steps, run as one agent per step in that folder.
            let items = ["Write the engine", "Wire it", "Document it"].iter().map(|t| super::super::Item { title: (*t).to_owned(), ..Default::default() }).collect();
            let p = super::super::create(&hub, super::super::New { title: "Ship sync".into(), items, ..Default::default() }, "admin").await?;
            let cfg = Config { place: Place { on: Some("local".into()), kind: Some("claude".into()), cwd: Some(cwd.clone()), ..Place::default() }, opener: Some("owner".into()), actor: "admin".into(), ..Config::default() };
            let run = agents::start(&hub, &p, cfg).await?.run;
            let work_of = [("engine.rs", "fn engine() {}\n"), ("wire.rs", "fn wire() {\n    engine();\n}\n"), ("README.md", "app\n\nIt syncs.\n")];
            let mut ids: Vec<String> = Vec::new();
            for (i, (file, text)) in work_of.iter().enumerate() {
                let v = agents::view(&hub, &run).await?;
                let stands: Vec<&str> = v.steps.iter().map(|s| s.stand.as_str()).collect();
                anyhow::ensure!(stands.get(i) == Some(&"working") && stands.iter().skip(i + 1).all(|s| *s == "waiting"), "step {} starts once the one before is met: {stands:?}", i + 1);
                let agent = v.steps.get(i).and_then(|s| s.agent_id.clone()).ok_or_else(|| anyhow::anyhow!("no agent for step {}", i + 1))?;
                // Before its step is met, its work isn't a change of its own.
                let early = crate::ops::call(cx.clone(), "agents.change", json!({"agent": agent})).await.err().map(|(_, m)| m).unwrap_or_default();
                anyhow::ensure!(early.contains("still going"), "{early}");
                std::fs::write(work.path().join(file), text)?;
                anyhow::ensure!(hub.harness.set_changes(&agent, json!([{"path": format!("{cwd}/{file}"), "added": 1, "removed": 0}])), "no agent to set the changes of");
                let a = agent.clone();
                hub.store.call(move |c| Ok(c.execute("UPDATE goal_items SET status = 'met' WHERE agent = ?1", rusqlite::params![a])?)).await?;
                agents::advance(&hub, &run).await?;
                let st = super::of(&hub, &run).await.ok_or_else(|| anyhow::anyhow!("no stack"))?;
                let e = st.steps.get(i).cloned().unwrap_or_default();
                anyhow::ensure!(e.state == "made" && e.pushed && e.change.as_deref() == Some(format!("D{}", i + 1).as_str()), "step {}: {e:?} {:?}", i + 1, st.off);
                let record = crate::workspace::change::of(&hub, &agent).await.ok_or_else(|| anyhow::anyhow!("no record for step {}", i + 1))?;
                anyhow::ensure!(record.change == e.change && record.stack.as_ref().is_some_and(|s| s.step == i + 1 && s.run == run), "{record:?}");
                if i == 0 {
                    // While the run goes on, a reviewer's comment on step 1's diff waits for it to end.
                    hub.emit(&json!({"type": "comment", "diff": "D1", "diff_author": format!("agent:{agent}"), "author": "user:ada", "body": "Call it run_engine", "file": "engine.rs", "start_line": 1}));
                    let mut waited = false;
                    for _ in 0..250 {
                        if hub.store.kv_get(format!("plans/stack-held/{run}")).await?.is_some_and(|t| t.contains("Call it run_engine")) {
                            waited = true;
                            break;
                        }
                        crate::rt::sleep(Duration::from_millis(20)).await;
                    }
                    anyhow::ensure!(waited, "the comment didn't wait for the run");
                }
                ids.push(agent);
            }
            // Done: a stack of three diffs, each on the one before, each by its own step's agent.
            let v = agents::view(&hub, &run).await?;
            anyhow::ensure!(v.state == "done" && v.met == 3, "{} {:?}", v.state, v.reason);
            let made = diffs(&hub).await?;
            let stack = made.first().map(|d| d.3.clone()).unwrap_or_default();
            let want: Vec<(u32, String, Option<u32>, String, u32)> = ids.iter().enumerate().map(|(i, id)| (u32::try_from(i).unwrap_or(0) + 1, format!("agent:{id}"), (i > 0).then(|| u32::try_from(i).unwrap_or(0)), stack.clone(), 1)).collect();
            anyhow::ensure!(!stack.is_empty() && made == want, "{made:?}");
            let shown = v.stack.clone().unwrap_or_default();
            anyhow::ensure!(shown.get("provider") == Some(&json!("kiln")) && shown.get("base") == Some(&json!(head)), "{shown}");
            // What waited went when the run ended; the folder's branch and index never moved.
            anyhow::ensure!(hub.store.kv_get(format!("plans/stack-held/{run}")).await?.is_none(), "held messages left");
            anyhow::ensure!(sh_off(work.path(), "git rev-parse HEAD").await? == head && sh_off(work.path(), "git diff --cached --name-only").await?.is_empty(), "the folder moved");
            // Step 1's agent answers the comment: its diff's new version; the steps above as they were.
            let first = ids.first().cloned().unwrap_or_default();
            std::fs::write(work.path().join("engine.rs"), "fn run_engine() {}\n")?;
            let out = crate::ops::call(cx.clone(), "agents.change", json!({"agent": first})).await.map_err(|(c, m)| anyhow::anyhow!("{c}: {m}"))?;
            anyhow::ensure!(out.pointer("/change_request/change") == Some(&json!("D1")) && out.pointer("/change_request/version") == Some(&json!(2)), "{out}");
            let versions: Vec<u32> = diffs(&hub).await?.iter().map(|d| d.4).collect();
            anyhow::ensure!(versions == [2, 1, 1], "{versions:?}");
            let st = super::of(&hub, &run).await.ok_or_else(|| anyhow::anyhow!("no stack"))?;
            let shas: Vec<String> = st.steps.iter().filter_map(|e| e.sha.clone()).collect();
            let [one, two, three] = shas.as_slice() else { anyhow::bail!("{shas:?}") };
            anyhow::ensure!(sh_off(work.path(), &format!("git show {one}:engine.rs")).await? == "fn run_engine() {}" && sh_off(work.path(), &format!("git rev-parse {three}^ {two}^")).await? == format!("{two}\n{one}"), "restacked");
            anyhow::ensure!(sh_off(work.path(), "git rev-parse HEAD").await? == head, "the folder's branch didn't move");
            Ok(())
        })
    }
}
