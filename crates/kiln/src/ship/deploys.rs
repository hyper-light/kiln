//! Ship's Deploys (docs/ci.md W26-R24): every KilnCD app as Ship's rows, by project, needs-you
//! first; an opened app's pane (its components grouped by what owns them, a rollout's steps, its
//! settings, its history of deploys and its source's commits); and the strip's verbs, each going to
//! its CD operation as the caller, with that operation's rights and audit. Where each app stands is
//! `kiln_cd::stands`'s to say: this reads KilnCD's state into it and shapes the JSON. Nothing is
//! kept here but the source logs that say how far trunk is ahead, for half a minute.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use http::{Method, StatusCode};
use kiln_cd::engine::OpStatus;
use kiln_cd::model::{Health, Sync};
use kiln_cd::stands::{self, App, Standing};
use kiln_cd::state::{Deployment, Pin, RolloutRecord};
use kiln_cd::tree::{self, Managed, Node};
use parking_lot::Mutex;
use serde_json::{Value, json};

use super::Fail;
use crate::cd::Cd;
use crate::http::server::{self as srv, Response};
use crate::hub::Hub;

/// How far back a source's log is read to say how far trunk is ahead of what runs.
const AHEAD_LOOK: usize = 100;
/// How long a source's log is kept.
const AHEAD_FOR: Duration = Duration::from_secs(30);

/// One app as KilnCD holds it, read at once: what `kiln_cd::stands` reads, and what its row shows
/// besides.
#[derive(Clone, Debug, Default)]
pub(crate) struct Facts {
    pub id: String,
    pub name: String,
    /// Its repository (`owner/name`), else its Argo CD project.
    pub project: String,
    pub environment: Option<String>,
    pub cluster: String,
    pub namespace: String,
    pub sync: Sync,
    pub health: Health,
    pub operation: Option<OpStatus>,
    pub message: Option<String>,
    pub error: Option<String>,
    pub shadow: bool,
    pub auto_sync: bool,
    pub history: VecDeque<Deployment>,
    pub pin: Option<Pin>,
    /// Its rollout under way, else its newest, with its id.
    pub rollout: Option<(String, RolloutRecord)>,
    pub chart: bool,
    pub ahead: Option<u32>,
    pub revision: Option<String>,
    /// Objects in git.
    pub objects: u32,
    /// Where its manifests come from (`deploy/helm/kiln @ main`).
    pub source: String,
    /// Its git source of its own, by index, and that source's repository and revision.
    pub own: Option<(usize, String, String)>,
}

impl Facts {
    fn app(&self) -> App<'_> {
        App {
            sync: self.sync,
            health: self.health,
            operation: self.operation.as_ref(),
            message: self.message.as_deref(),
            error: self.error.as_deref(),
            shadow: self.shadow,
            auto_sync: self.auto_sync,
            history: &self.history,
            pin: self.pin.as_ref(),
            rollout: self.rollout.as_ref().map(|(_, r)| r),
            chart: self.chart,
            ahead: self.ahead,
            revision: self.revision.as_deref(),
        }
    }

    /// The commit of its own source that runs now.
    fn running_commit(&self) -> Option<String> {
        let i = self.own.as_ref()?.0;
        let all = stands::deploys(&self.history);
        let d = all.iter().rev().find(|d| d.succeeded())?;
        d.sync.commits.get(i).cloned().flatten().or_else(|| d.sync.revision.clone())
    }
}

/// An app's project: the repository its deploy file is in or its manifests come from (`url`,
/// [`crate::cd::rollback::repo_of`]), as people name it (`owner/name`), else its Argo CD project.
fn project_of(url: Option<&str>, project: &str, names: &HashMap<String, String>) -> String {
    let Some(url) = url.filter(|u| !u.trim().is_empty()) else { return project.to_owned() };
    if let Some(n) = names.get(url) {
        return n.clone();
    }
    let u = kiln_cd::rollback::repo_name(url);
    let path = u.rsplit_once(':').filter(|(h, _)| !h.contains('/')).map_or(u.as_str(), |(_, p)| p);
    let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    match parts.as_slice() {
        [.., owner, name] if !owner.contains('.') => format!("{owner}/{name}"),
        [.., name] => (*name).to_owned(),
        [] => project.to_owned(),
    }
}

/// Every app KilnCD holds, read at once (`only`: one app).
pub(crate) fn gather(cd: &Cd, only: Option<&str>) -> Vec<Facts> {
    let names = cd.repo_names.read().clone();
    let mut out: Vec<Facts> = Vec::new();
    {
        let apps = cd.apps.read();
        let statuses = cd.statuses.read();
        for (id, e) in apps.iter().filter(|(id, _)| only.is_none_or(|o| o == id.as_str())) {
            let def = &e.def;
            let s = statuses.get(id.as_str());
            let own = def.sources.iter().enumerate().find(|(_, s)| s.chart.is_none() && s.as_ref.is_none()).map(|(i, s)| (i, s.repo.clone(), s.revision.clone()));
            let source = def.sources.first().map_or_else(String::new, |s| match &s.chart {
                Some(c) => format!("chart {c} {}", s.revision).trim_end().to_owned(),
                None if s.path.is_empty() => format!("{} @ {}", kiln_cd::rollback::repo_name(&s.repo), s.revision),
                None => format!("{} @ {}", s.path, if s.revision.is_empty() { "HEAD" } else { &s.revision }),
            });
            out.push(Facts {
                id: id.clone(),
                name: def.name.clone(),
                project: project_of(crate::cd::rollback::repo_of(def).as_deref(), &def.project, &names),
                environment: def.environment.clone(),
                cluster: def.destination.cluster.clone(),
                namespace: def.destination.namespace.clone(),
                sync: s.map_or(Sync::Unknown, |s| s.sync),
                health: s.map_or(Health::Unknown, |s| s.health),
                operation: s.and_then(|s| s.operation.clone()),
                message: s.and_then(|s| s.message.clone()),
                error: e.error.clone(),
                shadow: crate::cd::is_shadow(def),
                auto_sync: cd.auto_sync_of(id, def).on,
                chart: own.is_none() && def.sources.iter().any(|s| s.chart.is_some()),
                revision: s.and_then(|s| s.revision.as_deref().map(str::to_owned)),
                objects: s.map_or(0, |s| s.resources),
                source,
                own,
                ..Facts::default()
            });
        }
    }
    let kept: Vec<(VecDeque<Deployment>, Option<Pin>)> = cd.store.read(|d| out.iter().map(|f| (d.history.get(&f.id).cloned().unwrap_or_default(), d.pins.get(&f.id).cloned())).collect());
    for (f, (history, pin)) in out.iter_mut().zip(kept) {
        f.history = history;
        f.pin = pin;
        let mut rollouts = cd.rollouts_of(&f.id);
        rollouts.sort_by_key(|(_, r)| (r.rollout.finished(), std::cmp::Reverse(r.started_ms)));
        f.rollout = rollouts.into_iter().next();
    }
    out
}

/// Each source's newest commits on its revision, kept a while: by repository and revision.
static LOGS: LazyLock<Mutex<HashMap<(String, String), (Instant, Arc<Vec<String>>)>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// How many commits each app's source has on its revision after the one that runs (as far as
/// [`AHEAD_LOOK`] goes): read from KilnCD's mirrors, each repository and revision once.
async fn read_ahead(cd: &Arc<Cd>, facts: &mut [Facts]) {
    let mut wanted: Vec<(String, String)> = facts.iter().filter_map(|f| f.own.as_ref().map(|(_, r, v)| (r.clone(), v.clone()))).collect();
    wanted.sort();
    wanted.dedup();
    let mut logs: HashMap<(String, String), Arc<Vec<String>>> = HashMap::new();
    for key in wanted {
        if let Some((at, l)) = LOGS.lock().get(&key)
            && at.elapsed() < AHEAD_FOR
        {
            logs.insert(key, l.clone());
            continue;
        }
        let (dir, auth, (url, rev)) = (cd.mirror_dir(&key.0), cd.git_auth(&key.0), key.clone());
        let got = crate::rt::blocking(move || kiln_cd::git::Mirror::open(&dir, &url, auth)?.log(&rev, AHEAD_LOOK)).await.ok().and_then(Result::ok);
        let Some(got) = got else { continue };
        let shas = Arc::new(got.into_iter().map(|c| c.sha).collect::<Vec<_>>());
        let mut cache = LOGS.lock();
        if cache.len() > 256 {
            cache.clear();
        }
        cache.insert(key.clone(), (Instant::now(), shas.clone()));
        logs.insert(key, shas);
    }
    for f in facts.iter_mut() {
        let (Some((_, repo, rev)), Some(runs)) = (f.own.clone(), f.running_commit()) else { continue };
        let Some(log) = logs.get(&(repo, rev)) else { continue };
        f.ahead = log.iter().position(|s| s.starts_with(&runs) || runs.starts_with(s.as_str())).and_then(|n| u32::try_from(n).ok());
    }
}

/// An app as Ship's row (W26-R24): its stand, word and line, its pips, who and when, where it
/// deploys and what runs; with `v`, so the page keeps a row that hasn't changed.
fn row_json(f: &Facts, s: &Standing) -> Value {
    super::with_version(json!({
        "app": f.id, "name": f.name, "project": f.project,
        "environment": f.environment, "cluster": f.cluster, "namespace": f.namespace,
        "stand": s.stand, "word": s.word, "line": s.line, "pips": s.pips,
        "by": s.by, "at": s.at_ms / 1000, "runs": s.runs,
        "sync": f.sync.as_str(), "health": f.health.as_str(), "auto_sync": f.auto_sync,
        "pinned": f.pin.as_ref().map(|p| json!({ "to": p.to, "revision": p.revision, "by": p.by, "at_ms": p.at_ms })),
    }))
}

/// Whether an app is one `q` asks for: its project, name, environment, cluster or namespace has
/// it, any case.
fn matches(f: &Facts, q: &str) -> bool {
    let q = q.trim().to_lowercase();
    q.is_empty() || [Some(&f.project), Some(&f.id), f.environment.as_ref(), Some(&f.cluster), Some(&f.namespace)].into_iter().flatten().any(|v| v.to_lowercase().contains(&q))
}

/// The list: apps by project, each project's apps and the projects themselves most pressing first
/// (then by name), each project with what its heading says.
pub(crate) fn list_json(facts: &[Facts], q: Option<&str>, now_ms: u64) -> Value {
    let mut by: BTreeMap<&str, Vec<(u8, &Facts, Standing)>> = BTreeMap::new();
    for f in facts.iter().filter(|f| q.is_none_or(|q| matches(f, q))) {
        let s = stands::standing(&f.app(), now_ms);
        by.entry(f.project.as_str()).or_default().push((stands::rank(s.stand), f, s));
    }
    let mut projects: Vec<(u8, &str, Vec<(u8, &Facts, Standing)>)> = by
        .into_iter()
        .map(|(p, mut rows)| {
            rows.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.name.cmp(&b.1.name)).then_with(|| a.1.id.cmp(&b.1.id)));
            (rows.iter().map(|r| r.0).min().unwrap_or(u8::MAX), p, rows)
        })
        .collect();
    projects.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(b.1)));
    let total: usize = projects.iter().map(|p| p.2.len()).sum();
    let out: Vec<Value> = projects
        .into_iter()
        .map(|(_, p, rows)| {
            let words: Vec<(kiln_cd::journey::Stand, &str)> = rows.iter().map(|(_, _, s)| (s.stand, s.word.as_str())).collect();
            let (say, needs) = stands::project_say(&words);
            json!({ "project": p, "say": say, "needs": needs, "apps": rows.iter().map(|(_, f, s)| row_json(f, s)).collect::<Vec<_>>() })
        })
        .collect();
    json!({ "projects": out, "total": total, "at_ms": now_ms })
}

/// What a setting does, on and off, in a few words.
fn setting(key: &str, on: bool, from: &str, can: bool) -> Value {
    let what = match (key, on) {
        ("auto_sync", true) => "syncs each time git changes what it renders",
        ("auto_sync", false) => "syncs when you say",
        ("auto_rollback", true) => "goes back on a failed deploy",
        ("auto_rollback", false) => "stays on a failed deploy",
        ("prune", true) => "a sync deletes what git dropped",
        ("prune", false) => "keeps what git dropped",
        ("self_heal", true) => "undoes changes made by hand",
        (_, _) => "leaves changes made by hand",
    };
    json!({ "key": key, "on": on, "from": from, "what": what, "can": can })
}

/// What the pane reads besides an app's facts: its tree, its source's newest commits, its
/// settings and its rollout's approvals.
#[derive(Debug, Default)]
pub(crate) struct Opened {
    pub nodes: Vec<Node>,
    /// The live tree was read (else what the engine holds).
    pub live: bool,
    pub errors: Vec<String>,
    pub commits: Vec<stands::Commit>,
    /// Auto-sync's and auto-rollback's word: on, and whose (`app`, `deploy-file`, `repo`, `default`).
    pub auto_sync: (bool, String),
    pub auto_rollback: (bool, String),
    pub prune: bool,
    pub self_heal: bool,
    /// The approvals its rollout waits for.
    pub approvals: Vec<String>,
}

/// An opened app's pane.
pub(crate) fn pane_json(f: &Facts, o: &Opened, now_ms: u64) -> Value {
    let a = f.app();
    let s = stands::standing(&a, now_ms);
    let groups = stands::components(&o.nodes, now_ms);
    let history = stands::history(&f.history, &o.commits, f.own.as_ref().map_or(0, |(i, _, _)| *i), stands::health_glyph(f.health));
    let back = history.iter().any(|l| l.to.is_some() || l.revision.is_some());
    let why_not = stands::why_not(&a, back);
    let rollout = f.rollout.as_ref().map(|(id, r)| {
        let mut v = serde_json::to_value(stands::rollout(id, r, now_ms)).unwrap_or(Value::Null);
        if let Some(m) = v.as_object_mut() {
            m.insert("approvals".into(), json!(o.approvals));
        }
        v
    });
    let settings = vec![
        setting("auto_sync", o.auto_sync.0, &o.auto_sync.1, !f.shadow),
        setting("auto_rollback", o.auto_rollback.0, &o.auto_rollback.1, true),
        setting("prune", o.prune, "deploy-file", false),
        setting("self_heal", o.self_heal, "deploy-file", false),
    ];
    json!({
        "app": row_json(f, &s),
        "about": { "cluster": f.cluster, "namespace": f.namespace, "objects": f.objects, "source": f.source },
        "groups": groups,
        "tree": { "live": o.live, "errors": o.errors },
        "rollout": rollout,
        "history": history,
        "settings": settings,
        "why_not": why_not,
    })
}

/// What KilnCD's engine holds of an app, as a tree with nothing live in it: a target beyond
/// Kubernetes (a Compose project, a host's service), or a cluster not reached.
async fn engine_nodes(cd: &Cd, id: &str) -> Vec<Node> {
    let key = id.to_owned();
    let views = cd.query(move |e| e.resources(&key)).await.unwrap_or_default();
    let managed: Vec<Managed> = views
        .into_iter()
        .map(|r| Managed {
            sync: if r.hook { "Hook".to_owned() } else { r.sync.as_str().to_owned() },
            health: r.health,
            message: r.message,
            hook: r.hook,
            missing: r.missing,
            requires_pruning: r.requires_pruning,
            why: r.why,
            tracked_by: r.tracked_by.map(|t| t.to_string()),
            shared_with: r.shared_with.iter().map(|s| s.to_string()).collect(),
            marks_only: r.marks_only,
            ..Managed::new(r.key)
        })
        .collect();
    crate::rt::blocking(move || tree::build(managed, Vec::new()).nodes).await.unwrap_or_default()
}

/// Reads what an opened app's pane shows besides its facts.
async fn open(cd: &Arc<Cd>, f: &Facts) -> Opened {
    let def = cd.apps.read().get(&f.id).map(|e| e.def.clone());
    let mut o = Opened::default();
    let Some(def) = def else { return o };
    // The live tree where it deploys to a Kubernetes cluster kiln reaches; else what the engine holds.
    match (kiln_cd::providers::is_target(&def.destination.cluster), cd.live.tree(cd, &f.id, false).await) {
        (false, Ok(t)) => {
            o.nodes = t.tree.nodes.clone();
            o.live = true;
            o.errors = t.errors.clone();
        }
        (target, got) => {
            if let (false, Err((_, why))) = (target, got) {
                o.errors.push(why);
            }
            o.nodes = engine_nodes(cd, &f.id).await;
        }
    }
    o.commits = crate::cd::rollback::commits(cd, &def, &f.history)
        .await
        .into_iter()
        .filter_map(|c| {
            Some(stands::Commit {
                sha: c.get("sha")?.as_str()?.to_owned(),
                subject: c.get("subject").and_then(Value::as_str).unwrap_or_default().to_owned(),
                author: c.get("author").and_then(Value::as_str).unwrap_or_default().to_owned(),
                at: c.get("at").and_then(Value::as_i64).unwrap_or(0),
                same_as: c.get("same_as").and_then(Value::as_u64),
            })
        })
        .collect();
    let auto_sync = cd.auto_sync_of(&f.id, &def);
    o.auto_sync = (auto_sync.on, auto_sync.from.as_str().to_owned());
    let auto_rollback = cd.auto_rollback_of(&f.id, &def).decision;
    o.auto_rollback = (auto_rollback.on, auto_rollback.from.as_str().to_owned());
    o.prune = def.sync.prune;
    o.self_heal = def.sync.self_heal;
    o.approvals = f.rollout.as_ref().map(|(id, _)| cd.rollouts.waiting(id)).unwrap_or_default();
    o
}

/// Deployments are read with `runs.read` on the whole server, as `/api/cd`'s reads are.
async fn may_read(hub: &Hub, caller: &crate::authz::Caller) -> Result<(), Fail> {
    crate::authz::gate::check(hub, caller, &Method::GET, &["api", "cd", "apps"], &HashMap::new()).await
}

fn cd() -> Result<Arc<Cd>, Fail> {
    crate::cd::get().ok_or_else(|| (StatusCode::SERVICE_UNAVAILABLE, "deployments aren't on here, or are starting ([cd] enabled)".to_owned()))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// `GET /api/ship/deploys?q=`: every app, by project (W26-R24).
pub async fn list(hub: &Arc<Hub>, q: &HashMap<String, String>, caller: &crate::authz::Caller) -> Result<Response, Fail> {
    may_read(hub, caller).await?;
    Ok(srv::json(StatusCode::OK, &list_of(&cd()?, q.get("q").map(String::as_str), now_ms()).await))
}

/// The list as `cd` holds it.
async fn list_of(cd: &Arc<Cd>, q: Option<&str>, now_ms: u64) -> Value {
    let mut facts = gather(cd, None);
    read_ahead(cd, &mut facts).await;
    list_json(&facts, q, now_ms)
}

/// `GET /api/ship/deploys/:project/:name`: an app's pane (W26-R24).
pub async fn get(hub: &Arc<Hub>, project: &str, name: &str, caller: &crate::authz::Caller) -> Result<Response, Fail> {
    may_read(hub, caller).await?;
    Ok(srv::json(StatusCode::OK, &pane_of(&cd()?, &format!("{project}/{name}"), now_ms()).await?))
}

/// App `id`'s pane as `cd` holds it.
async fn pane_of(cd: &Arc<Cd>, id: &str, now_ms: u64) -> Result<Value, Fail> {
    let mut facts = gather(cd, Some(id));
    read_ahead(cd, &mut facts).await;
    let f = facts.pop().ok_or_else(|| (StatusCode::NOT_FOUND, format!("no app {id}")))?;
    let o = open(cd, &f).await;
    Ok(pane_json(&f, &o, now_ms))
}

/// The CD operation a verb of the strip is, and its input, from the body: only what that
/// operation takes. Picked objects (`resources`) come as keys (`apps~Deployment~web~api`) or as
/// objects.
fn op_of(app: &str, verb: &str, body: &Value) -> Result<(&'static str, Value), Fail> {
    let mut input = serde_json::Map::new();
    input.insert("app".into(), json!(app));
    let mut take = |keys: &[&str]| {
        for k in keys {
            if let Some(v) = body.get(*k).filter(|v| !v.is_null()) {
                input.insert((*k).to_owned(), v.clone());
            }
        }
    };
    let op = match verb {
        "sync" => {
            take(&["prune", "dry_run", "options"]);
            if let Some(rs) = body.get("resources").and_then(Value::as_array) {
                let mut keys = Vec::new();
                for r in rs {
                    let k = match r.as_str() {
                        Some(s) => crate::cd::api::parse_key(s).map(|k| json!({ "group": k.group.as_ref(), "kind": k.kind.as_ref(), "namespace": k.namespace.as_ref(), "name": k.name.as_ref() })),
                        None => Some(r.clone()),
                    };
                    keys.push(k.ok_or_else(|| (StatusCode::BAD_REQUEST, format!("`resources`: {r} isn't an object's key (group~Kind~namespace~name)")))?);
                }
                input.insert("resources".into(), Value::Array(keys));
            }
            "cd.apps.sync"
        }
        "refresh" => {
            take(&["hard"]);
            "cd.apps.refresh"
        }
        "rollback" => {
            take(&["to", "revision", "prune"]);
            "cd.apps.rollback"
        }
        "stop" => "cd.apps.terminate",
        "follow" => "cd.apps.follow",
        "promote" | "abort" | "approve" | "deny" => {
            take(&["rollout", "full", "reason", "check"]);
            input.insert("action".into(), json!(verb));
            "cd.apps.rollout_act"
        }
        "auto_sync" | "auto_rollback" => {
            input.insert("on".into(), body.get("on").cloned().unwrap_or(Value::Null));
            if verb == "auto_sync" { "cd.apps.set_auto_sync" } else { "cd.apps.set_auto_rollback" }
        }
        _ => return Err((StatusCode::BAD_REQUEST, format!("`verb` {verb}: sync, refresh, rollback, stop, follow, promote, abort, approve, deny, auto_sync or auto_rollback"))),
    };
    // Promote, abort, approve and deny take what applies to them only.
    if op == "cd.apps.rollout_act" {
        let keep: &[&str] = match verb {
            "promote" => &["full"],
            "abort" => &["reason"],
            _ => &["check"],
        };
        input.retain(|k, _| ["app", "action", "rollout"].contains(&k.as_str()) || keep.contains(&k.as_str()));
    }
    Ok((op, Value::Object(input)))
}

/// `POST /api/ship/deploys {app, verb, …}`: one of the strip's verbs on an app (W26-R24): sync
/// (`resources`: what's picked; `prune`), refresh (`hard`), rollback (`to` a deployment, or
/// `revision`: any commit), stop, follow, promote (`full`), abort (`reason`), approve and deny
/// (`check`), auto_sync and auto_rollback (`on`: true, false or null). Each is its CD operation,
/// as its route would let the caller (`environments.deploy` on the whole server, an agent where
/// its environment lets agents deploy) and audited in this request's one row; it answers what the
/// operation did, or why it couldn't.
pub async fn act(hub: &Arc<Hub>, caller: &crate::authz::Caller, body: &Value) -> Result<Response, Fail> {
    act_in(hub, caller, body, cd).await.map(|v| srv::json(StatusCode::OK, &v))
}

/// [`act`] on the KilnCD `cd` gives, once the caller may.
async fn act_in(hub: &Arc<Hub>, caller: &crate::authz::Caller, body: &Value, cd: impl FnOnce() -> Result<Arc<Cd>, Fail>) -> Result<Value, Fail> {
    let field = |k: &str| body.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned);
    let (Some(app), Some(verb)) = (field("app"), field("verb")) else {
        return Err((StatusCode::BAD_REQUEST, "`app` and `verb`: the app, and what to do to it".to_string()));
    };
    let (op, input) = op_of(&app, &verb, body)?;
    let def = crate::cd::ops::ops().iter().find(|o| o.name == op).ok_or_else(|| (StatusCode::INTERNAL_SERVER_ERROR, format!("no operation {op}")))?;
    let method = if def.read_only { Method::GET } else { Method::POST };
    crate::authz::gate::check(hub, caller, &method, &["api", "cd", "ops", op], &HashMap::new()).await?;
    def.input.fits(&input).map_err(|m| (StatusCode::BAD_REQUEST, m.said(op)))?;
    let cd = cd()?;
    let done = crate::cd::ops::call(crate::cd::ops::Cx { hub: Arc::clone(hub), cd, actor: caller.name.clone() }, op, input).await?;
    Ok(json!({ "app": app, "verb": verb, "op": op, "done": done }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiln_cd::engine::{OpPhase, Trigger};
    use kiln_cd::rollout::{Plan, Rollout, State, Step};

    const NOW: u64 = 1_793_511_180_000;
    const MIN: u64 = 60_000;

    fn dep(id: u64, op: u64, rev: &str, ok: bool, trigger: &str, at_ms: u64, message: &str) -> Deployment {
        Deployment { app: String::new(), op, revision: Some(rev.into()), trigger: trigger.into(), by: Some("ada".into()), succeeded: ok, message: message.into(), at_ms, id, commits: vec![Some(rev.into())], ..Deployment::default() }
    }

    fn facts(project: &str, name: &str, history: Vec<Deployment>) -> Facts {
        Facts {
            id: format!("default/{name}"),
            name: name.into(),
            project: project.into(),
            environment: Some("production".into()),
            cluster: "in-cluster".into(),
            namespace: name.into(),
            sync: Sync::Synced,
            health: Health::Healthy,
            history: history.into(),
            objects: 22,
            source: "deploy/helm/kiln @ main".into(),
            own: Some((0, format!("https://example/{project}"), "main".into())),
            ..Facts::default()
        }
    }

    const C1: &str = "1111111111111111111111111111111111111111";
    const C2: &str = "2222222222222222222222222222222222222222";
    const C3: &str = "3333333333333333333333333333333333333333";

    /// Several projects: an out-of-sync app and a synced one (kiln), a rolled-back one (site), a
    /// rollout waiting on a person and a sync under way (quiver).
    fn fleet() -> Vec<Facts> {
        let mut kiln = facts("hyper-light/kiln", "kiln-production", vec![dep(152, 1, C1, true, "manual", NOW - 2 * 60 * MIN, "synced"), dep(153, 2, C2, true, "manual", NOW - 36 * MIN, "synced")]);
        kiln.sync = Sync::OutOfSync;
        kiln.ahead = Some(2);
        let mut prom = facts("hyper-light/kiln", "kube-prometheus-stack-production", vec![dep(4, 1, "92.2.0", true, "manual", NOW - 6 * 24 * 60 * MIN, "synced")]);
        prom.chart = true;
        prom.own = None;
        let mut site = facts(
            "hyper-light/hyperlight-site",
            "site-production",
            vec![dep(18, 1, C1, true, "manual", NOW - 4 * 60 * MIN, "synced"), dep(19, 2, C3, true, "manual", NOW - 3 * 60 * MIN, "synced"), dep(20, 2, C3, false, "manual", NOW - 3 * 60 * MIN + MIN, "failed its health check"), dep(21, 3, C1, true, "rollback", NOW - 3 * 60 * MIN + 2 * MIN, "synced")],
        );
        site.pin = Some(Pin { to: 18, revision: Some(C1.into()), by: Some("auto-rollback".into()), at_ms: NOW - 3 * 60 * MIN + 2 * MIN, ..Pin::default() });
        let mut quiver = facts("hyper-light/quiver", "quiver-production", vec![dep(38, 1, C1, true, "manual", NOW - 2 * 24 * 60 * MIN, "synced"), dep(39, 2, C2, true, "manual", NOW - 5 * MIN, "synced")]);
        let mut m = Rollout::new(Plan { steps: vec![Step::Weight { weight: 20 }, Step::Approve, Step::Weight { weight: 100 }], ..Plan::default() }, NOW - 5 * MIN);
        m.state = State::Paused { why: "waiting for approval".into() };
        m.step = 1;
        m.weight = 20;
        quiver.rollout = Some((
            "ro-39".into(),
            RolloutRecord {
                app: "default/quiver-production".into(),
                environment: Some("production".into()),
                target: "aws-lambda:prod/us-east-1".into(),
                key: "fn".into(),
                object: Value::Null,
                revision: Some(C2.into()),
                previous: Some(C1.into()),
                back_to: Some(38),
                blue: Value::Null,
                green: Value::Null,
                preview_url: None,
                stable_url: None,
                rollout: m,
                checks: BTreeMap::new(),
                started_ms: NOW - 5 * MIN,
                updated_ms: NOW,
                version: 1,
            },
        ));
        let mut staging = facts("hyper-light/quiver", "quiver-staging", vec![dep(41, 1, C1, true, "auto", NOW - 60 * MIN, "synced")]);
        staging.environment = Some("staging".into());
        staging.revision = Some(C3.into());
        staging.operation = Some(OpStatus { id: 9, phase: OpPhase::Running, trigger: Trigger::Auto, attempt: 1, message: "syncing".into(), by: None, started_ms: NOW - 10_000, finished_ms: None });
        vec![prom, kiln, staging, quiver, site]
    }

    fn at<'a>(v: &'a Value, path: &str) -> &'a Value {
        v.pointer(path).unwrap_or(&Value::Null)
    }

    #[test]
    fn w26_r24_the_list_is_every_app_by_project_needs_you_first() {
        let v = list_json(&fleet(), None, NOW);
        let projects: Vec<(String, String, bool)> = at(&v, "/projects")
            .as_array()
            .into_iter()
            .flatten()
            .map(|p| (at(p, "/project").as_str().unwrap_or_default().to_owned(), at(p, "/say").as_str().unwrap_or_default().to_owned(), at(p, "/needs").as_bool().unwrap_or(false)))
            .collect();
        assert_eq!(
            projects,
            [
                ("hyper-light/kiln".to_owned(), "2 apps · 1 out of sync".to_owned(), true),
                ("hyper-light/quiver".to_owned(), "2 apps · 1 waits on you".to_owned(), true),
                ("hyper-light/hyperlight-site".to_owned(), "1 app · rolled back".to_owned(), true),
            ]
        );
        let rows: Vec<(String, String, String, String)> = at(&v, "/projects")
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|p| at(p, "/apps").as_array().cloned().unwrap_or_default())
            .map(|r| {
                let s = |k: &str| at(&r, k).as_str().unwrap_or_default().to_owned();
                (s("/name"), s("/stand"), s("/word"), s("/line"))
            })
            .collect();
        let row = |n: &str, st: &str, w: &str, l: &str| (n.to_owned(), st.to_owned(), w.to_owned(), l.to_owned());
        assert_eq!(
            rows,
            [
                row("kiln-production", "needs_you", "Out of sync", "#153 runs 2222222 · trunk is 2 ahead · healthy"),
                row("kube-prometheus-stack-production", "shipped", "Synced", "chart 92.2.0 · healthy"),
                row("quiver-production", "needs_you", "Needs you", "#39 is waiting for approval · #38 runs"),
                row("quiver-staging", "shipping", "Syncing", "syncing 3333333"),
                row("site-production", "rolled_back", "Rolled back", "#19 failed: failed its health check · back on #18"),
            ]
        );
        let kiln = at(&v, "/projects/0/apps/0");
        assert_eq!(at(kiln, "/pips").as_array().map(|p| p.iter().map(|x| format!("{}:{}", at(x, "/stage").as_str().unwrap_or_default(), at(x, "/state").as_str().unwrap_or_default())).collect::<Vec<_>>()), Some(vec!["sync:passed".to_owned(), "health:healthy".to_owned(), "soak:passed".to_owned()]));
        assert_eq!((at(kiln, "/environment"), at(kiln, "/runs"), at(kiln, "/by"), at(kiln, "/at")), (&json!("production"), &json!("2222222"), &json!("ada"), &json!((NOW - 36 * MIN) / 1000)));
        assert!(at(kiln, "/v").as_str().is_some_and(|v| v.len() == 16), "{kiln}");
        assert_eq!(at(&v, "/projects/1/apps/1/by"), &json!("auto-sync"));
        assert_eq!(at(&v, "/projects/2/apps/0/pinned/to"), &json!(18));
        assert_eq!(at(&v, "/total"), &json!(5));
        // What the search box asks for: an environment, a project, a cluster.
        let names = |q: &str| at(&list_json(&fleet(), Some(q), NOW), "/projects").as_array().into_iter().flatten().flat_map(|p| at(p, "/apps").as_array().cloned().unwrap_or_default()).map(|r| at(&r, "/name").as_str().unwrap_or_default().to_owned()).collect::<Vec<_>>();
        assert_eq!(names("STAGING"), ["quiver-staging"]);
        assert_eq!(names("hyperlight-site"), ["site-production"]);
        assert_eq!(names("in-cluster").len(), 5);
    }

    fn node(group: &str, kind: &str, name: &str, uid: &str, parents: &[&str]) -> Node {
        Node {
            id: uid.into(),
            uid: Some(uid.into()),
            group: group.into(),
            version: "v1".into(),
            kind: kind.into(),
            namespace: "kiln".into(),
            name: name.into(),
            parents: parents.iter().map(|p| (*p).to_owned()).collect(),
            managed: parents.is_empty(),
            health: Some("Healthy".into()),
            message: None,
            sync: parents.is_empty().then(|| "Synced".to_owned()),
            hook: false,
            missing: false,
            requires_pruning: false,
            why: None,
            tracked_by: None,
            shared_with: Vec::new(),
            marks_only: false,
            created_at: Some("2026-11-01T05:30:00Z".into()),
            info: Vec::new(),
            status: Some("Running".into()),
            ready: Some("1/1".into()),
            restarts: Some(0),
            images: Vec::new(),
            containers: Vec::new(),
            node: None,
        }
    }

    #[test]
    fn w26_r24_an_opened_apps_pane_is_its_components_rollout_settings_and_history() {
        let all = fleet();
        let kiln = all.iter().find(|f| f.name == "kiln-production").cloned().unwrap_or_default();
        let mut cm = node("", "ConfigMap", "kiln-config", "cm", &[]);
        cm.sync = Some("OutOfSync".into());
        let o = Opened {
            nodes: vec![node("apps", "Deployment", "kiln", "d", &[]), node("apps", "ReplicaSet", "kiln-cd9dc7b7", "rs", &["d"]), node("", "Pod", "kiln-cd9dc7b7-rsmwz", "p", &["rs"]), cm],
            live: true,
            commits: vec![
                stands::Commit { sha: C3.into(), subject: "Runner upgrades".into(), author: "ada".into(), at: ((NOW - 60 * MIN) / 1000) as i64, same_as: None },
                stands::Commit { sha: C2.into(), subject: "Merge main".into(), author: "ada".into(), at: ((NOW - 3 * 60 * MIN) / 1000) as i64, same_as: None },
            ],
            auto_sync: (false, "deploy-file".into()),
            auto_rollback: (true, "default".into()),
            ..Opened::default()
        };
        let v = pane_json(&kiln, &o, NOW);
        assert_eq!(at(&v, "/app/word"), &json!("Out of sync"));
        assert_eq!(at(&v, "/about"), &json!({ "cluster": "in-cluster", "namespace": "kiln-production", "objects": 22, "source": "deploy/helm/kiln @ main" }));
        let groups: Vec<(String, String)> = at(&v, "/groups").as_array().into_iter().flatten().map(|g| (at(g, "/name").as_str().unwrap_or_default().to_owned(), at(g, "/note").as_str().unwrap_or_default().to_owned())).collect();
        assert_eq!(groups, [("Deployment kiln".to_owned(), "1/1 ready".to_owned()), ("Config".to_owned(), "1 differs from git".to_owned())]);
        assert_eq!(at(&v, "/groups/0/items/0/key"), &json!("~Pod~kiln~kiln-cd9dc7b7-rsmwz"));
        assert_eq!(at(&v, "/groups/1/items/0/differs"), &json!(true));
        // History: the commit after what runs, then the deploy that runs it, then the one before.
        let lines: Vec<(Value, Value, Value, Value)> = at(&v, "/history").as_array().into_iter().flatten().map(|l| (at(l, "/deploy").clone(), at(l, "/short").clone(), at(l, "/now").clone(), at(l, "/revision").clone())).collect();
        assert_eq!(lines, [(json!(null), json!("3333333"), json!(false), json!(C3)), (json!(153), json!("2222222"), json!(true), json!(null)), (json!(152), json!("1111111"), json!(false), json!(null))]);
        assert_eq!(at(&v, "/history/2/to"), &json!(152));
        let settings: Vec<(Value, Value, Value)> = at(&v, "/settings").as_array().into_iter().flatten().map(|s| (at(s, "/key").clone(), at(s, "/on").clone(), at(s, "/can").clone())).collect();
        assert_eq!(settings, [(json!("auto_sync"), json!(false), json!(true)), (json!("auto_rollback"), json!(true), json!(true)), (json!("prune"), json!(false), json!(false)), (json!("self_heal"), json!(false), json!(false))]);
        assert_eq!(at(&v, "/settings/0/what"), &json!("syncs when you say"));
        assert_eq!(at(&v, "/why_not").as_object().map(|m| m.keys().cloned().collect::<Vec<_>>()), Some(vec!["abort".to_owned(), "approve".to_owned(), "deny".to_owned(), "follow".to_owned(), "promote".to_owned(), "stop".to_owned()]));
        assert_eq!(at(&v, "/rollout"), &Value::Null);
        // A rollout waiting on a person: its steps, and the approvals it waits for.
        let quiver = all.iter().find(|f| f.name == "quiver-production").cloned().unwrap_or_default();
        let v = pane_json(&quiver, &Opened { approvals: vec!["release-approval".into()], ..Opened::default() }, NOW);
        assert_eq!(at(&v, "/rollout/steps/1/state"), &json!("requested"));
        assert_eq!(at(&v, "/rollout/approvals"), &json!(["release-approval"]));
        assert!(at(&v, "/why_not/approve").is_null() && at(&v, "/why_not/promote").is_null(), "{}", at(&v, "/why_not"));
    }

    #[test]
    fn w26_r24_each_verb_is_its_cd_operation_with_only_what_that_takes() {
        let op = |verb: &str, body: Value| op_of("default/web", verb, &body).map_err(|(c, m)| format!("{c}: {m}"));
        assert_eq!(
            op("sync", json!({ "resources": ["apps~Deployment~web~api", { "group": "", "kind": "ConfigMap", "namespace": "web", "name": "cfg" }], "prune": true, "junk": 1 })),
            Ok(("cd.apps.sync", json!({ "app": "default/web", "prune": true, "resources": [{ "group": "apps", "kind": "Deployment", "namespace": "web", "name": "api" }, { "group": "", "kind": "ConfigMap", "namespace": "web", "name": "cfg" }] })))
        );
        assert_eq!(op("refresh", json!({ "hard": true })), Ok(("cd.apps.refresh", json!({ "app": "default/web", "hard": true }))));
        assert_eq!(op("rollback", json!({ "revision": C3 })), Ok(("cd.apps.rollback", json!({ "app": "default/web", "revision": C3 }))));
        assert_eq!(op("rollback", json!({ "to": 152 })), Ok(("cd.apps.rollback", json!({ "app": "default/web", "to": 152 }))));
        assert_eq!(op("stop", json!({})), Ok(("cd.apps.terminate", json!({ "app": "default/web" }))));
        assert_eq!(op("follow", json!({})), Ok(("cd.apps.follow", json!({ "app": "default/web" }))));
        assert_eq!(op("promote", json!({ "full": true, "check": "x" })), Ok(("cd.apps.rollout_act", json!({ "app": "default/web", "action": "promote", "full": true }))));
        assert_eq!(op("abort", json!({ "reason": "errors" })), Ok(("cd.apps.rollout_act", json!({ "app": "default/web", "action": "abort", "reason": "errors" }))));
        assert_eq!(op("approve", json!({ "check": "release", "rollout": "ro-1" })), Ok(("cd.apps.rollout_act", json!({ "app": "default/web", "action": "approve", "check": "release", "rollout": "ro-1" }))));
        assert_eq!(op("auto_sync", json!({ "on": true })), Ok(("cd.apps.set_auto_sync", json!({ "app": "default/web", "on": true }))));
        assert_eq!(op("auto_rollback", json!({})), Ok(("cd.apps.set_auto_rollback", json!({ "app": "default/web", "on": null }))));
        assert!(op("sync", json!({ "resources": ["Deployment"] })).is_err_and(|e| e.contains("isn't an object's key")));
        assert!(op("delete", json!({})).is_err_and(|e| e.starts_with("400")));
        // Each one's input is its operation's to the letter.
        for (verb, body) in [("sync", json!({ "resources": ["apps~Deployment~web~api"] })), ("refresh", json!({ "hard": false })), ("rollback", json!({ "to": 1 })), ("stop", json!({})), ("follow", json!({})), ("promote", json!({})), ("abort", json!({})), ("deny", json!({ "check": "c" })), ("auto_sync", json!({ "on": false })), ("auto_rollback", json!({ "on": true }))] {
            let (name, input) = op_of("default/web", verb, &body).map_err(|(_, m)| anyhow::anyhow!(m)).unwrap_or(("none", Value::Null));
            let def = crate::cd::ops::ops().iter().find(|o| o.name == name);
            assert!(def.is_some_and(|d| !d.read_only && d.input.fits(&input).is_ok()), "{verb}: {name} {input}");
        }
    }

    #[test]
    fn w26_r24_a_project_is_its_repository_by_name() {
        let names: HashMap<String, String> = [("https://kiln.lan/git/hyper-light/kiln.git".to_owned(), "hyper-light/kiln".to_owned())].into();
        assert_eq!(project_of(Some("https://kiln.lan/git/hyper-light/kiln.git"), "default", &names), "hyper-light/kiln");
        assert_eq!(project_of(Some("https://github.com/hyper-light/quiver.git"), "default", &names), "hyper-light/quiver");
        assert_eq!(project_of(Some("git@github.com:hyper-light/site.git"), "default", &names), "hyper-light/site");
        assert_eq!(project_of(Some("oci://ghcr.io"), "default", &names), "ghcr.io");
        assert_eq!(project_of(None, "default", &names), "default");
    }

    fn within(f: impl std::future::Future<Output = anyhow::Result<()>> + Send + 'static) -> anyhow::Result<()> {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(crate::rt::block_on(f));
        });
        rx.recv_timeout(Duration::from_secs(120)).unwrap_or_else(|_| Err(anyhow::anyhow!("not done within two minutes")))
    }

    fn git(dir: &std::path::Path, args: &[&str]) -> anyhow::Result<String> {
        let o = std::process::Command::new("git")
            .args(["-c", "user.name=ada", "-c", "user.email=ada@example.com", "-c", "init.defaultBranch=main", "-c", "commit.gpgsign=false"])
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()?;
        anyhow::ensure!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
        Ok(String::from_utf8_lossy(&o.stdout).trim().to_owned())
    }

    /// A repository at `dir` with two commits on main: their shas, oldest first.
    fn two_commits(dir: &std::path::Path) -> anyhow::Result<(String, String)> {
        std::fs::create_dir_all(dir.join("deploy"))?;
        git(dir, &["init", "-q"])?;
        let mut shas = Vec::new();
        for n in ["one", "two"] {
            std::fs::write(dir.join("deploy/cm.yaml"), format!("apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: web-config\ndata:\n  v: {n}\n"))?;
            git(dir, &["add", "."])?;
            git(dir, &["commit", "-q", "-m", &format!("Commit {n}")])?;
            shas.push(git(dir, &["rev-parse", "HEAD"])?);
        }
        let mut it = shas.into_iter();
        Ok((it.next().unwrap_or_default(), it.next().unwrap_or_default()))
    }

    /// What the simulated cluster's engine left, for the scratch KilnCD.
    struct Left {
        statuses: Vec<(String, kiln_cd::engine::Summary)>,
        deployments: Vec<Deployment>,
        now: u64,
    }

    /// W26-R24 over a scratch hub, with the simulated cluster: four apps of two repositories run on
    /// it as people and kiln ran them (kiln-production synced by a person, then behind git;
    /// kiln-staging synced by itself; quiver-production rolled back by a person; quiver-canary's
    /// rollout waiting for an approval), and KilnCD holds what the engine left, what its store kept
    /// (the history, the pin, the rollout) and its mirrors of the repositories. Ship's list, each
    /// pane and each verb are read and done from there.
    #[test]
    fn w26_r24_over_a_scratch_hub_with_the_simulated_cluster_ship_reads_what_cd_holds_and_acts_through_its_ops() -> anyhow::Result<()> {
        use kiln_cd::engine::{Config as EngineConfig, Engine, Input, OpPhase, Record, SyncRequest, Trigger};
        use kiln_cd::harness::{Harness, Renderer};
        use kiln_cd::model::{AppConfig, SyncPolicy};
        use kiln_cd::state::{Command, rollout_id};

        let dir = tempfile::tempdir()?;
        let (k1, k2) = two_commits(&dir.path().join("git/hyper-light/kiln"))?;
        let (q1, q2) = two_commits(&dir.path().join("git/hyper-light/quiver"))?;
        let short = |s: &str| s.get(..7).unwrap_or(s).to_owned();
        let url = |name: &str| format!("file://{}", dir.path().join("git/hyper-light").join(name).to_string_lossy());
        let (kiln_url, quiver_url) = (url("kiln"), url("quiver"));
        const KP: &str = "default/kiln-production";
        const KS: &str = "default/kiln-staging";
        const QP: &str = "default/quiver-production";
        const QC: &str = "default/quiver-canary";
        let start = 1_793_400_000_000;
        let (d1, d2, d3, d4) = (k1.clone(), k2.clone(), q1.clone(), q2.clone());
        let drive = move || {
            // Each app's objects at a commit: its Deployment's image and its config say which.
            let renderer: Renderer = Box::new(|_app: &str, rev: &str, _key: u64| {
                let r = rev.get(..7).unwrap_or(rev);
                Ok(vec![
                    json!({"apiVersion": "apps/v1", "kind": "Deployment", "metadata": {"name": "web"}, "spec": {"replicas": 1, "selector": {"matchLabels": {"app": "web"}}, "template": {"metadata": {"labels": {"app": "web"}}, "spec": {"containers": [{"name": "web", "image": format!("web:{r}")}]}}}}),
                    json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "web-config"}, "data": {"v": r}}),
                ])
            });
            let mut h = Harness::new(Engine::new(EngineConfig { wave_delay_ms: 0, ..EngineConfig::default() }), renderer);
            h.now = start;
            h.add_cluster(kiln_cd::sim::Cluster::new("dev"), None);
            for (id, auto) in [(KP, false), (KS, true), (QP, false), (QC, false)] {
                let mut a = AppConfig::new(id, "dev", id.trim_start_matches("default/"));
                a.sync = SyncPolicy { auto, prune: true, self_heal: false, ..SyncPolicy::default() };
                h.input(Input::UpsertApp(a));
            }
            let key = |id: &str, rev: &str| kiln_cd::obj::hash_bytes(format!("{id}@{rev}").as_bytes());
            let inputs = |h: &mut Harness, id: &str, rev: &str| h.input(Input::Inputs { app: id.into(), revision: rev.into(), key: key(id, rev), content: key(id, rev) });
            let by_ada = || SyncRequest { by: Some("ada".into()), ..SyncRequest::default() };
            let finished = |h: &Harness, id: &str| h.records.iter().filter(|r| matches!(r, Record::Finished { app, .. } if &**app == id)).count();
            let settled = |h: &Harness, id: &str| h.statuses.get(id).is_some_and(|s| s.sync == Sync::Synced && s.health == Health::Healthy && s.operation.as_ref().is_some_and(|o| o.phase == OpPhase::Succeeded));
            let sync = |h: &mut Harness, id: &str, n: usize| {
                h.input(Input::Sync { app: id.into(), request: by_ada() });
                h.run_until(60_000, |h| finished(h, id) == n && settled(h, id))
            };
            // kiln-production: a person syncs the first commit; then git moves on.
            inputs(&mut h, KP, &d1);
            let mut ok = sync(&mut h, KP, 1);
            // kiln-staging syncs the second by itself.
            inputs(&mut h, KS, &d2);
            ok &= h.run_until(60_000, |h| finished(h, KS) == 1 && settled(h, KS));
            inputs(&mut h, KP, &d2);
            h.run_for(10_000);
            // quiver-production: synced to each commit, then rolled back to the first.
            inputs(&mut h, QP, &d3);
            ok &= sync(&mut h, QP, 1);
            inputs(&mut h, QP, &d4);
            ok &= sync(&mut h, QP, 2);
            let back = SyncRequest { trigger: Some(Trigger::Rollback), ..by_ada() };
            h.input(Input::SyncTo { app: QP.into(), revision: d3.as_str().into(), key: key(QP, &d3), content: key(QP, &d3), request: back });
            ok &= h.run_until(60_000, |h| finished(h, QP) == 3 && settled(h, QP));
            // quiver-canary: the first commit, then the second, whose rollout waits.
            inputs(&mut h, QC, &d3);
            ok &= sync(&mut h, QC, 1);
            inputs(&mut h, QC, &d4);
            ok &= sync(&mut h, QC, 2);
            let deployments: Vec<Deployment> = h
                .records
                .iter()
                .filter_map(|r| match r {
                    Record::Finished { app, op, revision, key, content, trigger, by, succeeded, message, applied, pruned, at_ms } => Some(Deployment {
                        app: app.to_string(),
                        op: *op,
                        revision: revision.as_ref().map(|r| r.to_string()),
                        key: *key,
                        trigger: trigger.as_str().to_owned(),
                        by: by.clone(),
                        succeeded: *succeeded,
                        message: message.clone(),
                        applied: *applied,
                        pruned: *pruned,
                        at_ms: *at_ms,
                        content: *content,
                        commits: vec![revision.as_ref().map(|r| r.to_string())],
                        ..Deployment::default()
                    }),
                    _ => None,
                })
                .collect();
            let statuses = h.statuses.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
            let now = h.now;
            (h, (ok, Left { statuses, deployments, now }))
        };
        within(async move {
            let hub = crate::hub::tests::scratch_hub().await?;
            let (cd, (ok, left)) = Cd::scratch(hub.clone(), &dir.path().join("cd"), drive)?;
            anyhow::ensure!(ok, "the simulated cluster never settled: {:?}", left.statuses);
            // What KilnCD holds: the apps, the engine's statuses, the store's history, pin and rollout.
            for (id, repo, auto, env) in [(KP, &kiln_url, false, "production"), (KS, &kiln_url, true, "staging"), (QP, &quiver_url, false, "production"), (QC, &quiver_url, false, "canary")] {
                let name = id.trim_start_matches("default/");
                let def: kiln_cd::apps::AppDef = serde_json::from_value(json!({
                    "name": name, "project": "default",
                    "sources": [{ "repo": repo, "path": "deploy", "revision": "main", "chart": null, "as_ref": null, "engine": { "Directory": { "recurse": false, "include": null, "exclude": null } } }],
                    "destination": { "cluster": "dev", "namespace": name },
                    "sync": { "auto": auto, "prune": true, "self_heal": false, "allow_empty": false, "options": {}, "retry": null },
                    "ignore": [], "tracking": "Kiln", "origin": "Api", "environment": env, "labels": {}, "history_limit": 10, "cascade": true, "managed_namespace": null,
                }))?;
                let entry = crate::cd::AppEntry { def, commits: Vec::new(), deps: Vec::new(), key: None, error: None, children: Default::default(), resolved: VecDeque::new() };
                cd.apps.write().insert(id.to_owned(), entry);
            }
            cd.statuses.write().extend(left.statuses);
            let submit = |c: Command| {
                let cd = cd.clone();
                async move { cd.store.submit(c).await.map_err(|_| anyhow::anyhow!("the store stopped"))?.map_err(anyhow::Error::msg) }
            };
            for d in left.deployments {
                submit(Command::Deployed { deployment: Box::new(d), limit: 10 }).await?;
            }
            submit(Command::Pin { app: QP.into(), pin: Some(Box::new(Pin { to: 1, revision: Some(q1.clone()), commits: vec![Some(q1.clone())], head: 1, by: Some("ada".into()), at_ms: left.now })) }).await?;
            let mut m = Rollout::new(Plan { steps: vec![Step::Weight { weight: 20 }, Step::Approve, Step::Weight { weight: 100 }], ..Plan::default() }, left.now);
            m.state = State::Paused { why: "waiting for approval".into() };
            m.step = 1;
            m.weight = 20;
            let rid = rollout_id("dev", "apps/Deployment/quiver-canary/web");
            let rec = RolloutRecord {
                app: QC.into(), environment: Some("canary".into()), target: "dev".into(), key: "apps/Deployment/quiver-canary/web".into(), object: Value::Null,
                revision: Some(q2.clone()), previous: Some(q1.clone()), back_to: Some(1), blue: Value::Null, green: Value::Null, preview_url: None, stable_url: None,
                rollout: m, checks: BTreeMap::new(), started_ms: left.now, updated_ms: left.now, version: 1,
            };
            submit(Command::Rollout { id: rid, record: Some(Box::new(rec)), expect: None }).await?;
            let now = left.now + 2 * stands::SOAK_MS;

            // The list: by project, needs-you first, each row as defined.
            let v = list_of(&cd, None, now).await;
            let heads: Vec<(Value, Value)> = at(&v, "/projects").as_array().into_iter().flatten().map(|p| (at(p, "/project").clone(), at(p, "/say").clone())).collect();
            assert_eq!(heads, [(json!("hyper-light/kiln"), json!("2 apps · 1 out of sync")), (json!("hyper-light/quiver"), json!("2 apps · 1 waits on you"))]);
            let rows: Vec<(Value, Value, Value, Value)> = at(&v, "/projects")
                .as_array()
                .into_iter()
                .flatten()
                .flat_map(|p| at(p, "/apps").as_array().cloned().unwrap_or_default())
                .map(|r| (at(&r, "/name").clone(), at(&r, "/stand").clone(), at(&r, "/word").clone(), at(&r, "/line").clone()))
                .collect();
            assert_eq!(
                rows,
                [
                    (json!("kiln-production"), json!("needs_you"), json!("Out of sync"), json!(format!("#1 runs {} · trunk is 1 ahead · healthy", short(&k1)))),
                    (json!("kiln-staging"), json!("shipped"), json!("Synced"), json!(format!("#1 runs {} · healthy", short(&k2)))),
                    (json!("quiver-canary"), json!("needs_you"), json!("Needs you"), json!("#2 is waiting for approval · #1 runs")),
                    (json!("quiver-production"), json!("rolled_back"), json!("Rolled back"), json!("back on #1 · held there until git changes it")),
                ]
            );
            let kp = at(&v, "/projects/0/apps/0");
            assert_eq!((at(kp, "/by"), at(kp, "/environment"), at(kp, "/runs"), at(kp, "/sync"), at(kp, "/health")), (&json!("ada"), &json!("production"), &json!(short(&k1)), &json!("OutOfSync"), &json!("Healthy")));
            assert_eq!(at(&v, "/projects/0/apps/1/by"), &json!("auto-sync"));

            // kiln-production's pane: what differs from git, by owner (its cluster isn't reached
            // live: what the engine holds), and its history, main's newer commit beside its deploy.
            let pane = pane_of(&cd, KP, now).await.map_err(|(c, m)| anyhow::anyhow!("{c}: {m}"))?;
            let groups: Vec<(Value, Value, Value)> = at(&pane, "/groups").as_array().into_iter().flatten().map(|g| (at(g, "/name").clone(), at(g, "/note").clone(), at(g, "/owner/key").clone())).collect();
            assert_eq!(groups, [(json!("Deployment web"), json!("out of sync"), json!("apps~Deployment~kiln-production~web")), (json!("Config"), json!("1 differs from git"), Value::Null)]);
            assert_eq!(at(&pane, "/groups/1/items/0/state"), &json!("changes"));
            assert_eq!(at(&pane, "/tree/live"), &json!(false));
            let history: Vec<(Value, Value, Value)> = at(&pane, "/history").as_array().into_iter().flatten().map(|l| (at(l, "/deploy").clone(), at(l, "/revision").clone(), at(l, "/now").clone())).collect();
            assert_eq!(history, [(Value::Null, json!(k2), json!(false)), (json!(1), Value::Null, json!(true))]);
            // quiver-production's: its deploys of main's commits, what runs now among them.
            let pane = pane_of(&cd, QP, now).await.map_err(|(c, m)| anyhow::anyhow!("{c}: {m}"))?;
            let history: Vec<(Value, Value, Value, Value)> = at(&pane, "/history").as_array().into_iter().flatten().map(|l| (at(l, "/deploy").clone(), at(l, "/short").clone(), at(l, "/now").clone(), at(l, "/to").clone())).collect();
            assert_eq!(history, [(json!(2), json!(short(&q2)), json!(false), json!(2)), (json!(3), json!(short(&q1)), json!(true), Value::Null), (json!(1), json!(short(&q1)), json!(false), json!(1))]);
            // quiver-canary's: its rollout's steps, and what can't be done meanwhile.
            let pane = pane_of(&cd, QC, now).await.map_err(|(c, m)| anyhow::anyhow!("{c}: {m}"))?;
            let steps: Vec<(Value, Value)> = at(&pane, "/rollout/steps").as_array().into_iter().flatten().map(|s| (at(s, "/state").clone(), at(s, "/what").clone())).collect();
            assert_eq!(steps, [(json!("passed"), json!("weight 20%")), (json!("requested"), json!("a person approves")), (json!("none"), json!("weight 100%"))]);
            assert_eq!(at(&pane, "/why_not").as_object().map(|m| m.keys().cloned().collect::<Vec<_>>()), Some(vec!["follow".to_owned(), "stop".to_owned()]));

            // The verbs, as the caller: a viewer may do none; the owner's reach their operations,
            // and what an operation refuses comes back as it says it.
            let viewer = crate::authz::Caller::peer("vic", "viewer");
            let owner = crate::authz::Caller::owner();
            let act = |caller: &crate::authz::Caller, body: Value| {
                let (hub, cd, caller) = (hub.clone(), cd.clone(), caller.clone());
                async move { act_in(&hub, &caller, &body, || Ok(cd)).await }
            };
            for verb in ["sync", "refresh", "rollback", "stop", "follow", "promote", "abort", "approve", "deny", "auto_sync", "auto_rollback"] {
                let refused = act(&viewer, json!({ "app": QC, "verb": verb, "on": true, "check": "release" })).await;
                assert!(matches!(&refused, Err((StatusCode::FORBIDDEN, m)) if m.contains("environments.deploy")), "{verb}: {refused:?}");
            }
            let done = act(&owner, json!({ "app": KP, "verb": "auto_sync", "on": true })).await.map_err(|(c, m)| anyhow::anyhow!("{c}: {m}"))?;
            assert_eq!((at(&done, "/op"), at(&done, "/done/on"), at(&done, "/done/from")), (&json!("cd.apps.set_auto_sync"), &json!(true), &json!("app")));
            // Now kiln syncs it by itself: it waits for that, no one else.
            let v = list_of(&cd, Some("kiln-production"), now).await;
            assert_eq!((at(&v, "/projects/0/apps/0/stand"), at(&v, "/projects/0/apps/0/auto_sync")), (&json!("waiting"), &json!(true)));
            let done = act(&owner, json!({ "app": KP, "verb": "refresh", "hard": true })).await.map_err(|(c, m)| anyhow::anyhow!("{c}: {m}"))?;
            assert_eq!(at(&done, "/done/refreshing"), &json!(KP));
            let done = act(&owner, json!({ "app": QP, "verb": "follow" })).await.map_err(|(c, m)| anyhow::anyhow!("{c}: {m}"))?;
            assert_eq!(at(&done, "/done/following"), &json!(QP));
            assert!(cd.store.read(|d| !d.pins.contains_key(QP)), "followed: no longer held");
            let refused = act(&owner, json!({ "app": KP, "verb": "promote" })).await;
            assert!(matches!(&refused, Err((StatusCode::NOT_FOUND, m)) if m.contains("has no rollout")), "{refused:?}");
            let refused = act(&owner, json!({ "app": KP, "verb": "rollback", "revision": k2 })).await;
            assert!(matches!(&refused, Err((StatusCode::SERVICE_UNAVAILABLE, m)) if m.contains("cluster dev is not connected")), "{refused:?}");
            Ok(())
        })
    }
}
