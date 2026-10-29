//! Previews (docs/ci.md W15-R07): an environment of its own for each diff, stack or pull request,
//! made from one of trunk's `.kiln/environments.yaml` `previews:`, and removed when its change
//! lands, is abandoned or closes, or when it hasn't been deployed for its `ttl`.
//!
//! ```yaml
//! previews:
//!   preview:                  # jobs deploy with `environment: preview`
//!     kind: container
//!     runners: label web
//!     per: stack              # one for each stack, at its top diff (the default), or `diff`
//!     on: [diff, pull_request]
//!     ttl: 72h
//!     url: https://${{ preview.id }}.preview.example.com
//! ```
//!
//! - **Binding.** A leg whose `environment:` names one, in a run for a diff, a change request or
//!   a pull request (its event's payload says which), is bound to that change's preview as it's
//!   queued: `preview-D12` (`preview-CR7`, `preview-PR42`; a stack's is named for its lowest
//!   diff), made then if it isn't there. Everything after (protection, routing, secrets, the
//!   deployment record, the runner's `kiln/deploy@v1`) is that environment's, with the template's
//!   rules. A stack's lower diffs deploy nothing: their legs are skipped, saying whose preview it is.
//!   A run for none of these can't deploy to one, and its leg fails, saying so.
//! - **Removal.** Its deployments are marked inactive, and where its kind is `container` or
//!   `microvm`, each runner that deployed it is asked to take its deployment down
//!   (`ServerMsg::Undeploy`), now or when it next connects, until it says it has. Other kinds'
//!   deployments are their jobs' to take down: the `pull_request` event's `closed` (a diff's too)
//!   starts the workflows that do.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use kiln_ci::wire::{PreviewOf, Undeploy, Undeployed};
use parking_lot::Mutex;
use rusqlite::{OptionalExtension as _, params};
use serde_json::Value;

use super::environments::{EnvDef, Preview, Targets, Template};
use crate::hub::Hub;
use crate::proto::{CAP_UNDEPLOY, ServerMsg};
use crate::store::Repo;

/// How often expired previews go and removals are asked of runners again.
const TICK: Duration = Duration::from_secs(60);
/// How long a runner has to say it took a deployment down before it's asked again.
const ASK_AGAIN: Duration = Duration::from_secs(10 * 60);
/// How long a preview that went is kept, so its change's closing runs can still bind to it.
const KEPT: i64 = 3600;

/// What a run is for, as previews see it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Subject {
    /// Kiln's diff `D<n>`.
    Diff(u32),
    /// Kiln's change request `CR<n>`.
    ChangeRequest(u32),
    /// A host's pull request `#<n>`.
    PullRequest(u64),
}

/// The change a run's event is about: kiln's diff or change request (its payload's `kiln`), else a
/// pull request (a comment's on one too).
fn change_of(payload: &Value) -> Option<Subject> {
    let number = |p: &str| payload.pointer(p).and_then(Value::as_u64);
    if let Some(n) = number("/kiln/diff/number").and_then(|n| u32::try_from(n).ok()) {
        return Some(Subject::Diff(n));
    }
    if let Some(n) = number("/kiln/change_request/number").and_then(|n| u32::try_from(n).ok()) {
        return Some(Subject::ChangeRequest(n));
    }
    if let Some(n) = number("/pull_request/number") {
        return Some(Subject::PullRequest(n));
    }
    payload.pointer("/issue/pull_request").and_then(|_| number("/issue/number")).map(Subject::PullRequest)
}

/// How a leg naming an environment is bound (W15-R07).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Bind {
    /// The name isn't one of `previews:`: it's the environment of that name.
    Not,
    /// Its change's preview, there now.
    To(PreviewOf),
    /// It deploys nothing, and why: a stack's preview is its top diff's.
    Skip(String),
    /// It can't deploy there, and why.
    Refuse(String),
}

/// A diff or change request as its preview needs it: whether it's closed (landed or abandoned),
/// and its stack.
struct At {
    closed: bool,
    stack: Option<Stack>,
}

/// A diff's stack: its id, its lowest diff, and the open diff right above this one, if any (whose
/// runs deploy the stack's preview, unless one is above that).
struct Stack {
    id: String,
    lowest: u32,
    above: Option<u32>,
}

async fn diff_at(hub: &Hub, repo: &Repo, number: u32) -> anyhow::Result<Option<At>> {
    let id = repo.id.clone();
    hub.store
        .call(move |c| {
            let row: Option<(String, String, Option<String>)> =
                c.query_row("SELECT id, state, stack_id FROM diffs WHERE repo_id = ?1 AND number = ?2", params![id, number], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?;
            let Some((diff, state, stack)) = row else { return Ok(None) };
            let stack = match stack {
                Some(stack) => {
                    let lowest: u32 = c.query_row("SELECT MIN(number) FROM diffs WHERE stack_id = ?1", [&stack], |r| r.get(0))?;
                    let above: Option<u32> = c
                        .query_row("SELECT MIN(number) FROM diffs WHERE parent_diff = ?1 AND stack_id = ?2 AND state NOT IN ('landed', 'abandoned')", params![diff, stack], |r| r.get(0))
                        .optional()?
                        .flatten();
                    Some(Stack { id: stack, lowest, above })
                }
                None => None,
            };
            Ok(Some(At { closed: matches!(state.as_str(), "landed" | "abandoned"), stack }))
        })
        .await
}

/// `text` with `${{ preview.id }}`, `${{ preview.name }}` and `${{ preview.number }}` filled in;
/// any other expression stays as written.
fn fill(text: &str, name: &str, id: &str) -> String {
    let number: String = id.chars().filter(char::is_ascii_digit).collect();
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("${{") {
        let (before, from) = rest.split_at(start);
        out.push_str(before);
        let Some(end) = from.find("}}") else {
            out.push_str(from);
            return out;
        };
        let (expr, after) = from.split_at(end + 2);
        match expr.trim_start_matches("${{").trim_end_matches("}}").trim() {
            "preview.id" => out.push_str(id),
            "preview.name" => out.push_str(name),
            "preview.number" => out.push_str(&number),
            _ => out.push_str(expr),
        }
        rest = after;
    }
    out.push_str(rest);
    out
}

/// The leg of `repo` naming environment `name`, in a run whose event's payload is `payload`,
/// bound to its change's preview when `name` is one of `previews:` (W15-R07): made now if it
/// isn't there, and lasting its template's `ttl` from now. A closed change's run (its closing
/// event's) binds to the preview it had, if it's still kept, and makes none.
pub async fn bind(hub: &Hub, repo: &Repo, name: &str, payload: &Value) -> anyhow::Result<Bind> {
    let templates = super::environments::templates(hub, repo).await;
    let Some(t) = templates.get(&name.to_ascii_lowercase()) else { return Ok(Bind::Not) };
    let Some(change) = change_of(payload) else {
        return Ok(Bind::Refuse(format!("`{}` is a preview, an environment of each diff, stack or pull request: this run is for none of them", t.name)));
    };
    let (key, id, shown, closed) = match change {
        Subject::Diff(n) if !t.diffs => return Ok(Bind::Refuse(format!("`{}` previews pull requests only (its `on`), and D{n} is a diff", t.name))),
        Subject::ChangeRequest(_) | Subject::PullRequest(_) if !t.pull_requests => return Ok(Bind::Refuse(format!("`{}` previews diffs only (its `on`), and this run is for a pull request", t.name))),
        Subject::Diff(n) => {
            let at = diff_at(hub, repo, n).await?;
            let closed = at.as_ref().is_some_and(|a| a.closed);
            match at.and_then(|a| a.stack).filter(|_| !t.per_diff) {
                Some(Stack { above: Some(top), lowest, .. }) if !closed => {
                    return Ok(Bind::Skip(format!("D{n} is below D{top} in its stack, and the stack's preview (`{}-D{lowest}`) is its top diff's", t.name)));
                }
                Some(s) => (format!("S{}", s.id), format!("D{}", s.lowest), format!("D{}", s.lowest), closed),
                None => (format!("D{n}"), format!("D{n}"), format!("D{n}"), closed),
            }
        }
        Subject::ChangeRequest(n) => {
            let closed = diff_at(hub, repo, n).await?.is_some_and(|a| a.closed);
            (format!("CR{n}"), format!("CR{n}"), format!("CR{n}"), closed)
        }
        Subject::PullRequest(n) => {
            let closed = payload.get("action").and_then(Value::as_str) == Some("closed") || payload.pointer("/pull_request/state").and_then(Value::as_str) == Some("closed");
            (format!("PR{n}"), format!("PR{n}"), format!("#{n}"), closed)
        }
    };
    // A stack's keeps the name it was made with, as its lowest diff lands.
    let (rid, k, tn) = (repo.id.clone(), key.clone(), t.name.clone());
    let made: Option<(String, String)> = hub
        .store
        .call(move |c| Ok(c.query_row("SELECT name, shown FROM ci_previews WHERE repo_id = ?1 AND change = ?2 AND template = ?3", params![rid, k, tn], |r| Ok((r.get(0)?, r.get(1)?))).optional()?))
        .await?;
    let prefix = format!("{}-", t.name);
    let (name, id, shown) = match made {
        Some((name, shown)) => {
            let id = name.strip_prefix(&prefix).unwrap_or(&id).to_owned();
            (name, id, shown)
        }
        None if closed => return Ok(Bind::Refuse(format!("{shown} is closed, and its preview went with it"))),
        None => (format!("{prefix}{id}"), id, shown),
    };
    if closed {
        return Ok(Bind::To(PreviewOf { template: t.name.clone(), id }));
    }
    let url = t.def.url.as_deref().map(|u| fill(u, &name, &id));
    let now = crate::util::now_unix();
    let (rid, n, tn, k, sh, kind, expires) = (repo.id.clone(), name, t.name.clone(), key, shown, t.def.kind.clone(), now.saturating_add(t.ttl));
    hub.store
        .call(move |c| {
            c.execute(
                "INSERT INTO ci_previews (repo_id, name, template, change, shown, kind, url, created_at, updated_at, expires_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8, ?9)
                 ON CONFLICT(repo_id, name) DO UPDATE SET template = ?3, change = ?4, shown = ?5, kind = ?6, url = ?7, updated_at = ?8, expires_at = ?9, closed_at = NULL, closed_why = NULL",
                params![rid, n, tn, k, sh, kind, url, now, expires],
            )?;
            Ok(())
        })
        .await?;
    Ok(Bind::To(PreviewOf { template: t.name.clone(), id }))
}

/// What a preview's change is, from its key.
fn kind_of(change: &str) -> &'static str {
    if change.starts_with("CR") {
        "change_request"
    } else if change.starts_with("PR") {
        "pull_request"
    } else if change.starts_with('S') {
        "stack"
    } else {
        "diff"
    }
}

/// `repo`'s previews, each the environment its template says, by lower-cased name; one whose
/// template is gone keeps no rules until it goes.
pub async fn defs(hub: &Hub, repo: &Repo, templates: &BTreeMap<String, Template>) -> anyhow::Result<Vec<(String, EnvDef)>> {
    let id = repo.id.clone();
    type Row = (String, String, String, String, Option<String>, Option<String>, (i64, i64, Option<i64>, Option<String>));
    let rows: Vec<Row> = hub
        .store
        .call(move |c| {
            let mut st = c.prepare("SELECT name, template, change, shown, kind, url, created_at, expires_at, closed_at, closed_why FROM ci_previews WHERE repo_id = ?1 ORDER BY name")?;
            let rows = st.query_map([&id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, (r.get(6)?, r.get(7)?, r.get(8)?, r.get(9)?))))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await?;
    Ok(rows
        .into_iter()
        .map(|(name, template, change, shown, kind, url, (created_at, expires_at, removed_at, removed))| {
            let mut d = templates.get(&template.to_ascii_lowercase()).map(|t| t.def.clone()).unwrap_or_else(|| EnvDef {
                name: String::new(),
                protection: kiln_ci::env::Protection::default(),
                url: None,
                kind: kind.clone(),
                targets: Targets::default(),
                source: String::new(),
                agents_approve: false,
                concurrency: None,
                preview: None,
            });
            d.url = url.or(d.url);
            d.source = "preview".into();
            d.preview = Some(Preview { template, of: shown, kind: kind_of(&change), created_at, expires_at, removed_at, removed });
            d.name = name;
            (d.name.to_ascii_lowercase(), d)
        })
        .collect())
}

/// Removes `repo`'s preview `name`, for `why` (`D12 landed`): its deployments marked inactive and,
/// for a `container` or `microvm` one, taken down on the runners that made them. Whether there
/// was one.
pub async fn end(hub: &Hub, repo: &Repo, name: &str, why: &str) -> anyhow::Result<bool> {
    let (id, repo_name, n, said) = (repo.id.clone(), repo.name.clone(), name.to_owned(), why.to_owned());
    type Gone = (String, Option<String>, Vec<i64>, Vec<String>);
    let gone: Option<Gone> = hub
        .store
        .call(move |c| {
            let tx = crate::store::tx(c)?;
            let row: Option<(String, Option<String>)> =
                tx.query_row("SELECT name, kind FROM ci_previews WHERE repo_id = ?1 AND name = ?2 AND closed_at IS NULL", params![id, n], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
            let Some((name, kind)) = row else { return Ok(None) };
            // Its deployments not yet inactive, and the runners they were made on.
            let live: Vec<i64> = {
                let mut st = tx.prepare(
                    "SELECT d.id FROM ci_deployments d WHERE d.repo_id = ?1 AND d.environment = ?2
                       AND COALESCE((SELECT s.state FROM ci_deployment_statuses s WHERE s.deployment_id = d.id ORDER BY s.id DESC LIMIT 1), '') != 'inactive'",
                )?;
                st.query_map(params![id, name], |r| r.get(0))?.collect::<Result<_, _>>()?
            };
            let runners: Vec<String> = {
                let mut st = tx.prepare("SELECT DISTINCT j.agent FROM ci_deployments d JOIN jobs j ON j.id = d.leg_id WHERE d.repo_id = ?1 AND d.environment = ?2 AND COALESCE(j.agent, '') != ''")?;
                st.query_map(params![id, name], |r| r.get(0))?.collect::<Result<_, _>>()?
            };
            let now = crate::util::now_unix();
            // Kept a while, for its change's closing runs (a job taking down what kiln doesn't).
            tx.execute("UPDATE ci_previews SET closed_at = ?3, closed_why = ?4 WHERE repo_id = ?1 AND name = ?2", params![id, name, now, said])?;
            if let Some(k) = kind.as_deref().filter(|k| matches!(*k, "container" | "microvm")) {
                for r in &runners {
                    tx.execute("INSERT OR IGNORE INTO ci_preview_removals (runner, kind, repo, environment, created_at) VALUES (?1, ?2, ?3, ?4, ?5)", params![r, k, repo_name, name, now])?;
                }
            }
            tx.commit()?;
            Ok(Some((name, kind, live, runners)))
        })
        .await?;
    let Some((name, kind, live, runners)) = gone else { return Ok(false) };
    for d in live {
        super::environments::status(hub, d, "inactive", None, Some(format!("The preview went: {why}")), "kiln").await?;
    }
    tracing::info!(repo = %repo.name, environment = %name, kind = kind.as_deref().unwrap_or("none"), why, "a preview went");
    hub.emit(&crate::wire::events::ServerEvent::PreviewRemoved { repo: repo.name.clone(), environment: name.to_owned(), why: why.to_owned() });
    if matches!(kind.as_deref(), Some("container" | "microvm")) {
        for r in &runners {
            ask(hub, r).await;
        }
    }
    Ok(true)
}

/// The previews of `repo` for `changes` (their keys: `D12`, `S<stack>`, `PR42`), removed for `why`.
async fn end_for(hub: &Hub, repo: &Repo, changes: Vec<String>, why: &str) {
    let id = repo.id.clone();
    let names: Vec<String> = match hub
        .store
        .call(move |c| {
            let mut out = Vec::new();
            let mut st = c.prepare("SELECT name FROM ci_previews WHERE repo_id = ?1 AND change = ?2 AND closed_at IS NULL")?;
            for k in &changes {
                out.extend(st.query_map(params![id, k], |r| r.get::<_, String>(0))?.filter_map(Result::ok));
            }
            Ok(out)
        })
        .await
    {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!(repo = %repo.name, error = %format!("{e:#}"), "a change's previews couldn't be read");
            return;
        }
    };
    for name in names {
        if let Err(e) = end(hub, repo, &name, why).await {
            tracing::warn!(repo = %repo.name, environment = %name, error = %format!("{e:#}"), "a preview couldn't be removed");
        }
    }
}

/// Diff or change request `d` closed (`merged`: it landed): its preview goes, and its stack's
/// once no diff of the stack is open.
pub async fn closed(hub: &Hub, repo: &Repo, d: &super::diffs::Diff, merged: bool) {
    let why = match (d.kind.as_str(), merged) {
        (_, true) => format!("{} landed", d.name),
        ("change_request", false) => format!("{} was closed", d.name),
        _ => format!("{} was abandoned", d.name),
    };
    let mut changes = vec![d.name.clone()];
    if let Some(stack) = d.stack.clone() {
        let s = stack.clone();
        let open: bool =
            hub.store.call(move |c| Ok(c.query_row("SELECT EXISTS(SELECT 1 FROM diffs WHERE stack_id = ?1 AND state NOT IN ('landed', 'abandoned'))", [&s], |r| r.get(0))?)).await.unwrap_or(true);
        if !open {
            changes.push(format!("S{stack}"));
        }
    }
    end_for(hub, repo, changes, &why).await;
}

/// A host's pull request `number` closed (`merged` or not): its preview goes.
pub async fn pull_request_closed(hub: &Hub, repo: &Repo, number: u64, merged: bool) {
    let why = if merged { format!("#{number} was merged") } else { format!("#{number} was closed") };
    end_for(hub, repo, vec![format!("PR{number}")], &why).await;
}

/// A leg of `repo` bound to preview `name` ended on `runner`: when the preview went while it ran
/// (its change closed as it deployed), what it made there is taken down again, after it.
pub async fn leg_ended(hub: &Hub, repo: &Repo, name: &str, runner: &str) {
    let (id, repo_name, n, r) = (repo.id.clone(), repo.name.clone(), name.to_owned(), runner.to_owned());
    let asked = hub
        .store
        .call(move |c| {
            let kind: Option<Option<String>> = c.query_row("SELECT kind FROM ci_previews WHERE repo_id = ?1 AND name = ?2 AND closed_at IS NOT NULL", params![id, n], |x| x.get(0)).optional()?;
            let Some(kind) = kind.flatten().filter(|k| matches!(k.as_str(), "container" | "microvm")) else { return Ok(None) };
            c.execute("INSERT OR IGNORE INTO ci_preview_removals (runner, kind, repo, environment, created_at) VALUES (?1, ?2, ?3, ?4, ?5)", params![r, kind, repo_name, n, crate::util::now_unix()])?;
            Ok(Some((r, kind, repo_name, n.to_ascii_lowercase())))
        })
        .await;
    match asked {
        Ok(Some(key)) => {
            // Asked now, even when it was asked before this leg's deployment came up.
            ASKED.lock().get_or_insert_with(HashMap::new).remove(&key);
            ask(hub, runner).await;
        }
        Ok(None) => {}
        Err(e) => tracing::warn!(repo = %repo.name, environment = name, error = %format!("{e:#}"), "a gone preview's late deployment couldn't be taken down"),
    }
}

/// Takes environment `name`'s `kind` deployment (`container`, `microvm`) of `repo` down on
/// `runners`, as a gone preview's is: asked now of each runner that's here, and again whenever
/// one comes back, until it says it's done. A CD provider's object gone (docs/cd-targets.md §2.2).
pub async fn undeploy_on(hub: &Hub, repo: &Repo, kind: &str, name: &str, runners: &[String]) -> anyhow::Result<()> {
    let (repo_name, k, n, rs) = (repo.name.clone(), kind.to_owned(), name.to_owned(), runners.to_vec());
    hub.store
        .call(move |c| {
            for r in &rs {
                c.execute("INSERT OR IGNORE INTO ci_preview_removals (runner, kind, repo, environment, created_at) VALUES (?1, ?2, ?3, ?4, ?5)", params![r, k, repo_name, n, crate::util::now_unix()])?;
            }
            Ok(())
        })
        .await?;
    for r in runners {
        ask(hub, r).await;
    }
    Ok(())
}

/// When each removal was last asked of its runner, so a runner that's slow to answer isn't asked
/// again at once.
static ASKED: Mutex<Option<HashMap<(String, String, String, String), Instant>>> = Mutex::new(None);

/// The removals waiting on `runner`, asked of it if it's here and takes them.
pub async fn ask(hub: &Hub, runner: &str) {
    let Some(peer) = hub.instances.lock().newest_with(runner, CAP_UNDEPLOY) else { return };
    let r = runner.to_owned();
    let asks: Vec<Undeploy> = hub
        .store
        .call(move |c| {
            let mut st = c.prepare("SELECT kind, repo, environment FROM ci_preview_removals WHERE runner = ?1 ORDER BY created_at")?;
            let rows = st.query_map([&r], |x| Ok(Undeploy { kind: x.get(0)?, repo: x.get(1)?, environment: x.get(2)? }))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
        .unwrap_or_default();
    for a in asks {
        let key = (runner.to_owned(), a.kind.clone(), a.repo.clone(), a.environment.to_ascii_lowercase());
        if ASKED.lock().get_or_insert_with(HashMap::new).get(&key).is_some_and(|at| at.elapsed() < ASK_AGAIN) {
            continue;
        }
        let Ok(json) = serde_json::to_string(&a) else { continue };
        hub.command_peers(runner, vec![peer], &ServerMsg::Undeploy { json }).await.log_when_answered(crate::hub::LOG_WITHIN);
        ASKED.lock().get_or_insert_with(HashMap::new).insert(key, Instant::now());
    }
}

/// A runner connected: what's waiting for it to take down is asked of it now.
pub async fn runner_up(hub: &Hub, runner: &str) {
    ASKED.lock().get_or_insert_with(HashMap::new).retain(|k, _| k.0 != runner);
    ask(hub, runner).await;
}

/// `runner` says it took a deployment down (`AgentMsg::Undeployed`), or why it couldn't.
pub async fn undeployed(hub: &Hub, runner: &str, json: &str) {
    let said: Undeployed = match serde_json::from_str(json) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(runner, error = %e, "a runner's word on a deployment it took down doesn't read");
            return;
        }
    };
    let a = said.asked;
    if let Some(e) = said.error {
        tracing::warn!(runner, repo = %a.repo, environment = %a.environment, kind = %a.kind, error = %e, "a runner couldn't take a preview's deployment down: it's asked again later");
        return;
    }
    tracing::info!(runner, repo = %a.repo, environment = %a.environment, kind = %a.kind, "a runner took a preview's deployment down");
    ASKED.lock().get_or_insert_with(HashMap::new).remove(&(runner.to_owned(), a.kind.clone(), a.repo.clone(), a.environment.to_ascii_lowercase()));
    let r = runner.to_owned();
    if let Err(e) =
        hub.store.call(move |c| Ok(c.execute("DELETE FROM ci_preview_removals WHERE runner = ?1 AND kind = ?2 AND repo = ?3 AND environment = ?4", params![r, a.kind, a.repo, a.environment])?)).await
    {
        tracing::warn!(error = %format!("{e:#}"), "a removal that was done couldn't be forgotten");
    }
}

/// `secs` as people say it: `3 days`, `5 hours`, `1 minute`.
fn lasting(secs: i64) -> String {
    let (n, unit) = match secs {
        s if s >= 2 * 86_400 => (s / 86_400, "day"),
        s if s >= 2 * 3600 => (s / 3600, "hour"),
        s => ((s / 60).max(1), "minute"),
    };
    format!("{n} {unit}{}", if n == 1 { "" } else { "s" })
}

/// The previews past their `ttl`, removed.
async fn expire(hub: &Hub) -> anyhow::Result<()> {
    let now = crate::util::now_unix();
    let due: Vec<(String, String, i64)> = hub
        .store
        .call(move |c| {
            // Gone an hour ago: their change's closing runs have had their time.
            c.execute("DELETE FROM ci_previews WHERE closed_at <= ?1", [now.saturating_sub(KEPT)])?;
            let mut st = c.prepare("SELECT repo_id, name, updated_at FROM ci_previews WHERE closed_at IS NULL AND expires_at <= ?1")?;
            let rows = st.query_map([now], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await?;
    if due.is_empty() {
        return Ok(());
    }
    let repos = hub.store.repos().await?;
    for (repo_id, name, updated_at) in due {
        let Some(repo) = repos.iter().find(|r| r.id == repo_id) else { continue };
        end(hub, repo, &name, &format!("it expired, not deployed for {}", lasting(now.saturating_sub(updated_at)))).await?;
    }
    Ok(())
}

/// Previews past their `ttl` go, and removals waiting on runners that are here are asked again.
pub async fn run(hub: Arc<Hub>) -> anyhow::Result<()> {
    loop {
        if !hub.shutdown.sleep(TICK).await {
            return Ok(());
        }
        if let Err(e) = expire(&hub).await {
            tracing::warn!(error = %format!("{e:#}"), "expired previews couldn't be removed");
        }
        let runners: Vec<String> = hub
            .store
            .call(|c| {
                let mut st = c.prepare("SELECT DISTINCT runner FROM ci_preview_removals")?;
                let rows = st.query_map([], |r| r.get(0))?;
                Ok(rows.collect::<Result<Vec<_>, _>>()?)
            })
            .await
            .unwrap_or_default();
        for r in runners {
            ask(&hub, &r).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use super::*;

    #[test]
    fn w15_r07_what_a_run_is_for() {
        let diff = json!({ "pull_request": { "number": 12 }, "kiln": { "diff": { "number": 12, "id": "D12" } } });
        assert_eq!(change_of(&diff), Some(Subject::Diff(12)));
        let cr = json!({ "pull_request": { "number": 7 }, "kiln": { "change_request": { "number": 7 } } });
        assert_eq!(change_of(&cr), Some(Subject::ChangeRequest(7)));
        assert_eq!(change_of(&json!({ "pull_request": { "number": 42 } })), Some(Subject::PullRequest(42)));
        assert_eq!(change_of(&json!({ "issue": { "number": 9, "pull_request": {} } })), Some(Subject::PullRequest(9)));
        assert_eq!(change_of(&json!({ "issue": { "number": 9 } })), None, "an issue isn't a pull request");
        assert_eq!(change_of(&json!({ "ref": "refs/heads/main" })), None);
    }

    #[test]
    fn w15_r07_a_previews_url() {
        assert_eq!(fill("https://${{ preview.id }}.example.test/${{preview.number}}", "preview-D12", "D12"), "https://D12.example.test/12");
        assert_eq!(fill("http://x/${{ preview.name }}", "web-PR4", "PR4"), "http://x/web-PR4");
        assert_eq!(fill("http://x/${{ github.sha }}/${{ preview.id", "p-D1", "D1"), "http://x/${{ github.sha }}/${{ preview.id", "the rest as written");
        assert_eq!(kind_of("S0f3a"), "stack");
        assert_eq!(kind_of("CR3"), "change_request");
        assert_eq!(kind_of("PR3"), "pull_request");
        assert_eq!(kind_of("D3"), "diff");
        assert_eq!((lasting(3 * 86_400), lasting(5 * 3600), lasting(30), lasting(90 * 60)), ("3 days".into(), "5 hours".into(), "1 minute".into(), "90 minutes".into()));
    }
}
