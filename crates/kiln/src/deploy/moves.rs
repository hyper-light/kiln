//! Moving an environment from one CD provider to the other (docs/deploy-api.md §8.4). The release
//! repository stays as it is. The new provider gets every app that runs there in shadow: it
//! compares with what runs, and never writes. kiln shows where the two disagree (beyond each
//! provider's own tracking marks) and hands over when they agree, or when told to anyway: the old
//! provider stops, the new one syncs and takes each object over by its tracking, the old one lets
//! go of them, keeping them, and the environment names its new provider. Each object's uid is read
//! before and after: nothing is made again.
//!
//! A move is kept by the server and taken up again after a restart. Releases in the environment
//! wait until it's done or cancelled; a handover stopped part-way is finished by asking again.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::argocd::{self, Argo};
use super::flow::{Fail, Who};
use super::model::{App, ArgoSettings, Environment, KILNCD, Registration, ReleaseFile};
use super::{Deploy, appset, kilncd, reads, store};

/// An environment's move between providers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Move {
    pub environment: String,
    pub from: String,
    pub to: String,
    /// How the environment runs on Argo CD, when that's where it goes.
    #[serde(default)]
    pub argocd: ArgoSettings,
    pub phase: Phase,
    pub by: String,
    pub started_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<i64>,
    /// Hand over once the new provider agrees with what runs.
    #[serde(default)]
    pub handover: bool,
    /// Hand over whatever it says.
    #[serde(default)]
    pub force: bool,
    /// The handover began: the move only goes on from here, never back.
    #[serde(default)]
    pub handing_over: bool,
    #[serde(default)]
    pub registrations: Vec<Moved>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// The new provider is given each app in shadow, and compares.
    Comparing,
    /// It compared: see where it agrees.
    Compared,
    HandingOver,
    Moved,
    Failed,
    Cancelled,
}

/// One registration's part in a move.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Moved {
    pub app: String,
    /// The new provider would change nothing of what runs but its own tracking marks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agrees: Option<bool>,
    /// Where it disagrees: each object it would change, as a diff shows it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub differences: Vec<Value>,
    /// Each object's uid as the handover began (`group/kind/namespace/name`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub uids: BTreeMap<String, String>,
    /// Objects whose uid changed, or that are gone, after it: made again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remade: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// What a move asks for.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ask {
    /// `kilncd`, or `argocd/<integration>`.
    pub to: String,
    #[serde(default)]
    pub argocd: ArgoSettings,
    /// Hand over once the new provider agrees with what runs.
    #[serde(default)]
    pub handover: bool,
    /// With `handover`: whatever it says.
    #[serde(default)]
    pub force: bool,
}

fn key(env: &str) -> String {
    format!("deploy/move/{env}")
}

fn internal(e: impl std::fmt::Display) -> Fail {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

fn bad(m: impl Into<String>) -> Fail {
    (StatusCode::BAD_REQUEST, m.into())
}

fn conflict(m: impl Into<String>) -> Fail {
    (StatusCode::CONFLICT, m.into())
}

/// An environment's move, the one under way or the last.
pub async fn get(d: &Deploy, env: &str) -> anyhow::Result<Option<Move>> {
    Ok(match d.hub.store.kv_get(key(env)).await? {
        Some(text) => Some(serde_json::from_str(&text)?),
        None => None,
    })
}

async fn save(d: &Deploy, m: &Move) -> anyhow::Result<()> {
    d.hub.store.kv_set(key(&m.environment), serde_json::to_string(m)?).await?;
    d.hub.emit(&json!({ "type": "deploy_move", "move": m }));
    Ok(())
}

/// The move an environment is in, while releases there wait for it.
pub async fn under_way(d: &Deploy, env: &str) -> anyhow::Result<Option<Move>> {
    Ok(get(d, env).await?.filter(|m| matches!(m.phase, Phase::Comparing | Phase::Compared | Phase::HandingOver) || (m.phase == Phase::Failed && m.handing_over)))
}

/// Starts a move (compares, and hands over when asked and they agree), or finishes a handover
/// stopped part-way.
pub async fn start(d: &Deploy, who: &Who, env_name: &str, ask: Ask) -> Result<Move, Fail> {
    if who.agent {
        return Err((StatusCode::FORBIDDEN, "a person moves an environment between providers".into()));
    }
    let env = store::environment(&d.hub.store, env_name).await.map_err(internal)?.ok_or_else(|| (StatusCode::NOT_FOUND, format!("no environment {env_name}")))?;
    let to = ask.to.trim().to_owned();
    match to.strip_prefix("argocd/") {
        Some(name) => match crate::ci::integrations::get(&d.hub, name).await.map_err(internal)? {
            Some(i) if i.kind == argocd::KIND => {}
            Some(i) => return Err(bad(format!("{name} is a {} integration, not Argo CD", i.kind))),
            None => return Err(bad(format!("no integration {name}: add the Argo CD instance in Settings → Integrations first"))),
        },
        None if to == KILNCD => {}
        None => return Err(bad(format!("{to}: a provider is {KILNCD} or argocd/<integration>"))),
    }
    if to.starts_with("argocd/") && !env.targets.platforms.is_empty() {
        return Err(bad(format!("{env_name} deploys to platforms beyond Kubernetes, which only KilnCD does")));
    }
    let existing = get(d, env_name).await.map_err(internal)?;
    let now = crate::util::now_unix();
    let m = match existing {
        // A handover that began goes on to where it was going.
        Some(mut m) if m.handing_over && matches!(m.phase, Phase::Failed | Phase::HandingOver) => {
            if m.to != to {
                return Err(conflict(format!("{env_name}'s handover to {} began: it finishes first (ask again for {})", m.to, m.to)));
            }
            m.phase = Phase::HandingOver;
            m.message = None;
            m
        }
        Some(m) if matches!(m.phase, Phase::Comparing | Phase::HandingOver) => return Err(conflict(format!("{env_name} is moving to {} already", m.to))),
        // Its shadows there go first.
        Some(m) if matches!(m.phase, Phase::Compared | Phase::Failed) && m.to != to => {
            return Err(conflict(format!("{env_name} was compared on {}: cancel that move first (DELETE …/move), which takes its shadows away", m.to)));
        }
        _ => {
            if to == env.provider {
                return Err(bad(format!("{env_name} is on {to} already")));
            }
            for reg in store::registrations(&d.hub.store, None, Some(env_name)).await.map_err(internal)? {
                if let Some(r) = store::in_flight(&d.hub.store, &reg.app, env_name).await.map_err(internal)? {
                    return Err(conflict(format!("release {} of {} is in flight in {env_name}: a move waits for it", r.id, reg.app)));
                }
            }
            Move {
                environment: env_name.to_owned(),
                from: env.provider.clone(),
                to,
                argocd: ask.argocd,
                phase: Phase::Comparing,
                by: who.name.clone(),
                started_at: now,
                ended_at: None,
                handover: ask.handover,
                force: ask.force,
                handing_over: false,
                registrations: Vec::new(),
                message: None,
            }
        }
    };
    save(d, &m).await.map_err(internal)?;
    crate::rt::spawn(run(env_name.to_owned()));
    d.hub.store.audit(who.name.clone(), "deploy.environment.move".into(), format!("{env_name}: {} → {}{}", m.from, m.to, if m.handover { ", handing over" } else { "" })).await;
    Ok(m)
}

/// Ends a move that hasn't begun handing over: the new provider lets go of its shadows, which never
/// wrote, and what runs stays with the old one.
pub async fn cancel(d: &Deploy, who: &Who, env_name: &str) -> Result<Move, Fail> {
    let mut m = get(d, env_name).await.map_err(internal)?.ok_or_else(|| (StatusCode::NOT_FOUND, format!("{env_name} isn't moving")))?;
    if m.handing_over {
        return Err(conflict(format!("{env_name}'s handover to {} began: it can't be undone, ask again to finish it", m.to)));
    }
    if !matches!(m.phase, Phase::Compared | Phase::Failed) {
        return Err(conflict(format!("{env_name}'s move is {}: cancel it once it has compared", phase_name(m.phase))));
    }
    let env = store::environment(&d.hub.store, env_name).await.map_err(internal)?.ok_or_else(|| (StatusCode::NOT_FOUND, format!("no environment {env_name}")))?;
    let runs = running(d, &env).await.map_err(internal)?;
    let target = target(&env, &m);
    match target.provider.strip_prefix("argocd/") {
        Some(integration) => {
            let argo = Argo::of(&d.hub, integration).await.map_err(|e| (StatusCode::BAD_GATEWAY, e))?;
            let_go_on_argo(&argo, &target, &runs).await.map_err(|e| (StatusCode::BAD_GATEWAY, e))?;
        }
        None => {
            let cd = crate::cd::get().ok_or_else(|| (StatusCode::SERVICE_UNAVAILABLE, "KilnCD isn't running".to_owned()))?;
            for r in &runs {
                kilncd::remove(&d.hub, &cd, &format!("deploy for {}", who.name), &kilncd::app_id(&r.app, &env.name), true).await.map_err(|e| (StatusCode::BAD_GATEWAY, e))?;
            }
        }
    }
    m.phase = Phase::Cancelled;
    m.ended_at = Some(crate::util::now_unix());
    m.message = Some(format!("cancelled by {}: {} runs on {}", who.name, env_name, m.from));
    save(d, &m).await.map_err(internal)?;
    d.hub.store.audit(who.name.clone(), "deploy.environment.move.cancel".into(), env_name.to_owned()).await;
    Ok(m)
}

/// Takes up the moves under way, after a restart.
pub async fn resume(d: &Deploy) {
    let Ok(moves) = d.hub.store.kv_prefix("deploy/move/".into()).await else { return };
    for (_, text) in moves {
        if let Ok(m) = serde_json::from_str::<Move>(&text)
            && matches!(m.phase, Phase::Comparing | Phase::HandingOver)
        {
            crate::rt::spawn(run(m.environment));
        }
    }
}

fn phase_name(p: Phase) -> &'static str {
    match p {
        Phase::Comparing => "comparing",
        Phase::Compared => "compared",
        Phase::HandingOver => "handing over",
        Phase::Moved => "done",
        Phase::Failed => "failed",
        Phase::Cancelled => "cancelled",
    }
}

/// Moves a move along until it's compared, done or failed.
async fn run(env_name: String) {
    let Some(d) = super::get() else { return };
    loop {
        let Ok(Some(mut m)) = get(&d, &env_name).await else { return };
        match m.phase {
            Phase::Comparing => match compare(&d, &mut m).await {
                Ok(agree) if m.handover && (agree || m.force) => {
                    m.phase = Phase::HandingOver;
                    m.handing_over = true;
                }
                Ok(agree) => {
                    m.phase = Phase::Compared;
                    m.message = Some(if agree {
                        format!("{} agrees with what runs: hand over to it (ask again with handover)", m.to)
                    } else {
                        format!("{} would change what runs: see the differences; hand over anyway with force, or cancel", m.to)
                    });
                }
                Err(why) => {
                    m.phase = Phase::Failed;
                    m.message = Some(format!("comparing: {why}"));
                    m.ended_at = Some(crate::util::now_unix());
                }
            },
            Phase::HandingOver => match handover(&d, &mut m).await {
                Ok(()) => {
                    m.phase = Phase::Moved;
                    m.ended_at = Some(crate::util::now_unix());
                    let remade: Vec<String> = m.registrations.iter().flat_map(|r| r.remade.clone().unwrap_or_default()).collect();
                    m.message = Some(if remade.is_empty() { format!("{} runs on {}: nothing was made again", m.environment, m.to) } else { format!("{} runs on {}; made again: {}", m.environment, m.to, remade.join(", ")) });
                }
                Err(why) => {
                    m.phase = Phase::Failed;
                    m.message = Some(format!("the handover stopped: {why}. Ask again (with handover) to finish it"));
                }
            },
            _ => return,
        }
        if let Err(e) = save(&d, &m).await {
            tracing::warn!(environment = %env_name, error = %format!("{e:#}"), "deploy: a move couldn't be kept");
            return;
        }
        if d.hub.shutdown.is_cancelled() {
            return;
        }
    }
}

/// A registration that runs something, as the move takes it.
struct Runs {
    app: App,
    reg: Registration,
    commit: String,
    file: ReleaseFile,
}

/// The registrations of an environment that run something: enabled, a release landed.
async fn running(d: &Deploy, env: &Environment) -> anyhow::Result<Vec<Runs>> {
    let mut out = Vec::new();
    for reg in store::registrations(&d.hub.store, None, Some(&env.name)).await? {
        let (true, Some(id)) = (reg.enabled, reg.running) else { continue };
        let Some(commit) = store::release(&d.hub.store, id).await?.and_then(|r| r.commit) else { continue };
        let Some(app) = store::app(&d.hub.store, &reg.app).await? else { continue };
        let file = super::flow::release_file_at(d, &app, env, &commit).await.map_err(anyhow::Error::msg)?;
        if file.version.is_none() {
            continue;
        }
        out.push(Runs { app, reg, commit, file });
    }
    Ok(out)
}

/// The environment as it will be on its new provider.
fn target(env: &Environment, m: &Move) -> Environment {
    Environment { provider: m.to.clone(), argocd: m.argocd, ..env.clone() }
}

async fn environment(d: &Deploy, m: &Move) -> Result<Environment, String> {
    store::environment(&d.hub.store, &m.environment).await.map_err(|e| format!("{e:#}"))?.ok_or_else(|| format!("no environment {}", m.environment))
}

/// How long the new provider has to compare.
const COMPARE: Duration = Duration::from_secs(180);

/// Gives the new provider each app in shadow and reads where it disagrees with what runs: whether
/// it agrees everywhere.
async fn compare(d: &Deploy, m: &mut Move) -> Result<bool, String> {
    let env = environment(d, m).await?;
    let target = target(&env, m);
    let runs = running(d, &env).await.map_err(|e| format!("{e:#}"))?;
    m.registrations = runs.iter().map(|r| Moved { app: r.app.name.clone(), ..Moved::default() }).collect();
    let differences = match target.provider.strip_prefix("argocd/") {
        Some(integration) => {
            let argo = Argo::of(&d.hub, integration).await?;
            shadow_on_argo(d, &argo, &target, &runs).await?;
            let mut out = Vec::new();
            for r in &runs {
                let items = argo.managed_resources(&argocd::application_name(&r.app, &target.name)).await?;
                out.push(items.iter().filter_map(reads::disagreement).collect::<Vec<Value>>());
            }
            out
        }
        None => {
            let cd = crate::cd::get().ok_or("KilnCD isn't running")?;
            shadow_on_kilncd(d, &cd, &target, &runs, &m.by).await?;
            let mut out = Vec::new();
            for r in &runs {
                let id = kilncd::app_id(&r.app, &target.name);
                let cx = crate::cd::ops::Cx { hub: Arc::clone(&d.hub), cd: Arc::clone(&cd), actor: format!("deploy for {}", m.by) };
                let diffs = crate::cd::ops::call(cx, "cd.apps.diff", json!({ "app": id })).await.map_err(|(_, why)| format!("KilnCD's diff of {id}: {why}"))?;
                // What differs only by tracking marks is what a sync takes over, not a change.
                out.push(diffs.get("diffs").and_then(Value::as_array).into_iter().flatten().filter(|x| x.get("marks_only").and_then(Value::as_bool) != Some(true)).cloned().collect());
            }
            out
        }
    };
    for (moved, diffs) in m.registrations.iter_mut().zip(differences) {
        moved.agrees = Some(diffs.is_empty());
        moved.differences = diffs;
    }
    Ok(m.registrations.iter().all(|r| r.agrees == Some(true)))
}

/// Each app as a KilnCD app whose automation is off, compared with what runs: it never syncs. Put
/// again as it is, it changes nothing.
async fn shadow_on_kilncd(d: &Deploy, cd: &Arc<crate::cd::Cd>, target: &Environment, runs: &[Runs], by: &str) -> Result<(), String> {
    let url = d.hosted.url(&target.release_repo.repo).map_err(|e| format!("{e:#}"))?;
    let actor = format!("deploy for {by}");
    for r in runs {
        let id = kilncd::app_id(&r.app, &target.name);
        // Once it took over (a handover begun), it isn't put back in shadow.
        if cd.apps.read().get(&id).is_some_and(|e| e.def.sync.auto) {
            continue;
        }
        let version = r.file.version.clone().unwrap_or_default();
        let chart = kilncd::git_chart(cd, &r.file.package, &version).await;
        let mut def = kilncd::definition(&r.app, target, &r.reg, &r.file, &r.commit, &url, chart)?;
        def.sync.auto = false;
        kilncd::deliver(&d.hub, cd, &actor, &def).await.map_err(|e| e.to_string())?;
    }
    let ids: Vec<String> = runs.iter().map(|r| kilncd::app_id(&r.app, &target.name)).collect();
    let ids_ref = &ids;
    wait(d, COMPARE, "KilnCD to compare each app with what runs", move || async move {
        Ok(ids_ref.iter().all(|id| cd.statuses.read().get(id).is_some_and(|s| matches!(s.sync.as_str(), "Synced" | "OutOfSync"))))
    })
    .await
}

/// Each app as an Argo CD Application that compares with what runs and never syncs: made by the
/// environment's ApplicationSet in shadow, or written by kiln with its automation off.
async fn shadow_on_argo(d: &Deploy, argo: &Argo, target: &Environment, runs: &[Runs]) -> Result<(), String> {
    if appset::integration_of(target).is_some() {
        appset::shadow(d, argo, target).await.map_err(|e| e.to_string())?;
    } else {
        for r in runs {
            let name = argocd::application_name(&r.app, &target.name);
            // Once it took over (a handover begun), it isn't put back in shadow.
            if argo.application(&name, None).await?.is_some_and(|a| a.pointer("/spec/syncPolicy/automated").is_some()) {
                continue;
            }
            let values = super::flow::values_at(d, &r.app, target, &r.commit).await?;
            let paused = Registration { paused: true, ..r.reg.clone() };
            let spec = argocd::application(&argo.namespace, &r.app, target, &paused, &r.file, values.as_deref(), &r.commit)?;
            argo.put_application(&spec).await.map_err(|e| e.to_string())?;
        }
    }
    let names: Vec<(String, i64, String)> = runs.iter().map(|r| (argocd::application_name(&r.app, &target.name), r.file.release, r.file.version.clone().unwrap_or_default())).collect();
    let names_ref = &names;
    wait(d, COMPARE, "Argo CD to compare each app with what runs", move || async move {
        for (name, release, version) in names_ref {
            match argo.application(name, None).await? {
                Some(app) if argocd::compared(&app, *release, version) => {}
                // Not made yet, or not compared since: asked again now.
                _ => {
                    let _ = argo.application(name, Some("hard")).await;
                    return Ok(false);
                }
            }
        }
        Ok(true)
    })
    .await
}

/// The new provider takes each app over, the old one lets go of it, and the environment names the
/// new one; then each object's uid says whether anything was made again. Each step taken again
/// does nothing more, so a handover stopped part-way is finished by taking it again.
async fn handover(d: &Deploy, m: &mut Move) -> Result<(), String> {
    let mut env = environment(d, m).await?;
    let cd = crate::cd::get().ok_or("KilnCD isn't running: it is one side of every move")?;
    let cluster_name = env.targets.kubernetes.first().cloned().unwrap_or_else(|| kiln_cd::apps::IN_CLUSTER.to_owned());
    let cluster = cd.clusters.read().get(&cluster_name).cloned().ok_or_else(|| format!("KilnCD has no cluster {cluster_name}"))?;
    if env.provider != m.to {
        let target = target(&env, m);
        let runs = running(d, &env).await.map_err(|e| format!("{e:#}"))?;
        let to_kilncd = target.provider == KILNCD;
        let (argo_env, kilncd_env) = if to_kilncd { (&env, &target) } else { (&target, &env) };
        let integration = argo_env.provider.strip_prefix("argocd/").ok_or("a move goes between KilnCD and Argo CD")?;
        let argo = Argo::of(&d.hub, integration).await?;
        let actor = format!("deploy for {}", m.by);
        if m.registrations.len() != runs.len() {
            m.registrations = runs.iter().map(|r| Moved { app: r.app.name.clone(), ..Moved::default() }).collect();
        }
        // The new provider holds each app (in shadow, unless it took it over already).
        if to_kilncd {
            shadow_on_kilncd(d, &cd, &target, &runs, &m.by).await?;
        } else {
            shadow_on_argo(d, &argo, &target, &runs).await?;
        }
        // What each runs, by uid, before anything changes hands (kept from the first try).
        for (moved, r) in m.registrations.iter_mut().zip(&runs) {
            if moved.uids.is_empty() {
                let app = argo.application(&argocd::application_name(&r.app, &argo_env.name), None).await?.unwrap_or(Value::Null);
                moved.uids = uids(&cluster, &app).await;
            }
        }
        let timeout = Duration::from_secs(target.watch.timeout_secs);
        for r in &runs {
            let name = argocd::application_name(&r.app, &argo_env.name);
            let id = kilncd::app_id(&r.app, &kilncd_env.name);
            if to_kilncd {
                // Argo CD stops; KilnCD syncs, taking each object over by its tracking.
                argo.set_automation(&name, None).await?;
                if r.reg.paused {
                    let cx = crate::cd::ops::Cx { hub: Arc::clone(&d.hub), cd: Arc::clone(&cd), actor: actor.clone() };
                    crate::cd::ops::call(cx, "cd.apps.sync", json!({ "app": id })).await.map_err(|(_, why)| format!("KilnCD's sync of {id}: {why}"))?;
                } else {
                    kilncd::automation(&d.hub, &cd, &actor, &id, true).await?;
                }
                let (cd_ref, id_ref) = (&cd, id.as_str());
                wait(d, timeout, &format!("KilnCD to run {} synced and healthy", r.app.name), move || async move {
                    Ok(cd_ref.statuses.read().get(id_ref).is_some_and(|s| s.sync.as_str() == "Synced" && s.health.as_str() == "Healthy"))
                })
                .await?;
            } else {
                // KilnCD stops; Argo CD syncs, taking each object over by its tracking.
                kilncd::automation(&d.hub, &cd, &actor, &id, false).await?;
                if r.reg.paused {
                    argo.sync(&name, target.sync.prune).await?;
                } else {
                    argo.set_automation(&name, Some((target.sync.prune, target.sync.self_heal))).await?;
                }
                let (release, version) = (r.file.release, r.file.version.clone().unwrap_or_default());
                let (argo_ref, name_ref, version_ref) = (&argo, name.as_str(), version.as_str());
                wait(d, timeout, &format!("Argo CD to run {} synced and healthy", r.app.name), move || async move {
                    Ok(argo_ref.application(name_ref, None).await?.is_some_and(|app| argocd::landed(&app, release, version_ref).0 == super::provider::Landed::Yes))
                })
                .await?;
            }
        }
        // The old provider lets go, keeping what runs; the environment names the new one.
        if to_kilncd {
            let_go_on_argo(&argo, &env, &runs).await?;
        } else {
            for r in &runs {
                kilncd::remove(&d.hub, &cd, &actor, &kilncd::app_id(&r.app, &env.name), true).await?;
            }
            // Its ApplicationSet makes the next Applications automated, as the environment says.
            if appset::integration_of(&target).is_some() {
                appset::ensure(d, &argo, &target).await.map_err(|e| e.to_string())?;
            }
        }
        env.provider = m.to.clone();
        env.argocd = m.argocd;
        store::put_environment(&d.hub.store, &env).await.map_err(|e| format!("{e:#}"))?;
        d.wake.notify();
    }
    // Nothing was made again: each object has the uid it had.
    for moved in &mut m.registrations {
        let mut remade = Vec::new();
        for (k, uid) in &moved.uids {
            let Some(key) = res_key(k) else { continue };
            let now = cluster.get(&key).await.ok().flatten().and_then(|o| o.pointer("/metadata/uid").and_then(Value::as_str).map(str::to_owned));
            if now.as_deref() != Some(uid.as_str()) {
                remade.push(k.clone());
            }
        }
        moved.remade = Some(remade);
    }
    Ok(())
}

/// Argo CD lets go of an environment's Applications, keeping what they run: their ApplicationSet
/// first (it would make them again), then each Application, without its objects.
async fn let_go_on_argo(argo: &Argo, env: &Environment, runs: &[Runs]) -> Result<(), String> {
    let names: Vec<String> = runs.iter().map(|r| argocd::application_name(&r.app, &env.name)).collect();
    // Without the finalizer that deletes what an Application runs, should anything delete it.
    for name in &names {
        argo.keep_objects(name).await?;
    }
    if appset::integration_of(env).is_some() {
        argo.delete_application_set(&argocd::set_name(&env.name)).await?;
    }
    for name in &names {
        argo.delete_application(name, true).await?;
    }
    Ok(())
}

/// Each object an Application says it runs, by `group/kind/namespace/name`, with its uid now.
async fn uids(cluster: &crate::cd::kube::Cluster, app: &Value) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for r in app.pointer("/status/resources").and_then(Value::as_array).into_iter().flatten() {
        let f = |k: &str| r.get(k).and_then(Value::as_str).unwrap_or("").to_owned();
        let key = kiln_cd::obj::ResKey::new(&f("group"), &f("kind"), &f("namespace"), &f("name"));
        if let Ok(Some(o)) = cluster.get(&key).await
            && let Some(uid) = o.pointer("/metadata/uid").and_then(Value::as_str)
        {
            out.insert(format!("{}/{}/{}/{}", f("group"), f("kind"), f("namespace"), f("name")), uid.to_owned());
        }
    }
    out
}

fn res_key(k: &str) -> Option<kiln_cd::obj::ResKey> {
    let mut parts = k.splitn(4, '/');
    let (g, kind, ns, name) = (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
    Some(kiln_cd::obj::ResKey::new(g, kind, ns, name))
}

/// Waits until `done` says so, up to `limit`: what for says what it waited on when it gives up.
async fn wait<F, Fut>(d: &Deploy, limit: Duration, what: &str, done: F) -> Result<(), String>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<bool, String>>,
{
    let mut waited = Duration::ZERO;
    loop {
        if done().await? {
            return Ok(());
        }
        if waited >= limit {
            return Err(format!("waited {}s for {what}", limit.as_secs()));
        }
        if !d.hub.shutdown.sleep(Duration::from_secs(3)).await {
            return Err("kiln is stopping".into());
        }
        waited += Duration::from_secs(3);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_move_is_under_way_until_it_ends_and_a_handover_that_began_only_goes_on() {
        let m = |phase, handing_over| Move {
            environment: "staging".into(),
            from: "argocd/argo".into(),
            to: KILNCD.into(),
            argocd: ArgoSettings::default(),
            phase,
            by: "ada".into(),
            started_at: 0,
            ended_at: None,
            handover: true,
            force: false,
            handing_over,
            registrations: vec![],
            message: None,
        };
        let open = |m: &Move| matches!(m.phase, Phase::Comparing | Phase::Compared | Phase::HandingOver) || (m.phase == Phase::Failed && m.handing_over);
        assert!(open(&m(Phase::Compared, false)), "releases wait while the new provider's shadows compare");
        assert!(open(&m(Phase::Failed, true)), "a handover stopped part-way holds releases until it's finished");
        assert!(!open(&m(Phase::Failed, false)) && !open(&m(Phase::Moved, true)) && !open(&m(Phase::Cancelled, false)));
        let text = serde_json::to_string(&m(Phase::HandingOver, true)).unwrap_or_default();
        assert!(text.contains("\"phase\":\"handing_over\""), "{text}");
        assert_eq!(res_key("apps/Deployment/shop/web").map(|k| k.to_string()), Some(kiln_cd::obj::ResKey::new("apps", "Deployment", "shop", "web").to_string()));
        assert_eq!(res_key("/Service/shop/web").map(|k| k.group.to_string()), Some(String::new()), "the core group is empty");
    }
}
