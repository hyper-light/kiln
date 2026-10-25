//! Rollouts on targets beyond Kubernetes (docs/cd-targets.md §5): the state machine
//! (`kiln_cd::rollout`) says what happens, and this does it.
//!
//! - **Starting.** A sync applying a traffic-carrying object with a rollout (the deploy file's
//!   `rollout:`, stamped on the object) doesn't move its traffic. Green is staged beside blue (a
//!   Lambda version, with a preview alias of its own), traffic stays on blue, and a rollout starts.
//!   The object reads as applied, as Argo CD reads a Rollout it applied, and its health follows
//!   the rollout. An app's first deploy, green running what blue does, and going back within
//!   `rollback-window` move all the traffic at once, as Argo Rollouts does. A newer revision
//!   supersedes a rollout under way: it rolls out from the same blue, from its first step.
//! - **Driving.** Each rollout has a driver. It reads the platform (every 3 s while the rollout
//!   runs, 15 s otherwise) and hands the state machine what happened. It keeps the rollout
//!   durably after each change, and does what the state machine says: move traffic, measure
//!   checks, let versions go. After a restart the drivers start again and ask again what was in
//!   flight. Only the driver writes a rollout once it started, and only over the version it read,
//!   so it never overwrites a rollout that superseded its own.
//! - **People** promote, pause, resume, abort, retry and answer approvals through the API.
//! - **After an abort** (`auto-rollback`, the default) the app goes back to the deployment that ran
//!   before and holds there until git changes it. Without it, traffic stays on blue and the app
//!   reads as degraded until a person retries or git moves on. A person's word for the app or its
//!   repository (`cd::rollback`) beats the deploy file's.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use async_channel::{Receiver, Sender};
use http::StatusCode;
use kiln_cd::apps::ROLLOUT;
use kiln_cd::engine::{Input, LiveChange};
use kiln_cd::obj::{self, ResKey};
use kiln_cd::rollout::{self, Action, Against, CheckSpec, Event, Measurement, Plan, Rollout, RolloutSpec, State, Step, Verdict};
use kiln_cd::state::{RolloutRecord, applied_key, rollout_id};
use parking_lot::Mutex;
use serde_json::{Value, json};

use super::targets::{self, Cx, Records, aws_lambda};
use super::{Cd, checks, now_ms};

/// How often a driver reads its platform while the rollout runs (or waits on the platform), and
/// otherwise.
const HOT: Duration = Duration::from_secs(3);
const REST: Duration = Duration::from_secs(15);
/// The least a driver waits between turns, so a deadline that doesn't move never spins it.
const LEAST: Duration = Duration::from_millis(50);
/// How long a driver waits for its target to connect before looking again.
const UNREACHABLE: Duration = Duration::from_secs(2);

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// What a driver hears: an event, with the rollout (its `started_ms`) whose action it answers;
/// none for what people do.
pub type Msg = (Option<u64>, Event);

/// The rollouts being driven: each driver's inbox, and approvals waiting for a person.
#[derive(Default)]
pub struct Drivers {
    inbox: Mutex<HashMap<String, Sender<Msg>>>,
    /// Approval checks waiting for a person, by rollout and check: the rollout (its start) and
    /// the ask they answer.
    approvals: Mutex<HashMap<(String, String), (u64, u64)>>,
}

impl Drivers {
    /// The approvals a rollout waits for: its checks' names.
    pub fn waiting(&self, id: &str) -> Vec<String> {
        let mut out: Vec<String> = self.approvals.lock().keys().filter(|(r, _)| r == id).map(|(_, c)| c.clone()).collect();
        out.sort();
        out
    }
}

/// Whether a rollout's platform can be called.
pub enum Reach {
    Now,
    /// Not yet: its target is still connecting.
    Later,
    /// Never: its app is gone.
    Never,
}

/// What a driver asks of its rollout's platform.
pub enum Op {
    /// Green's share, and whether it's ready.
    Observe,
    /// Give green this share.
    Shift(u8),
    /// Make green's preview (again).
    Preview,
    /// Let go of what the rollout made beside the object.
    Release,
}

/// The platform's answer.
pub enum Answer {
    Observed { weight: Option<u8>, ready: bool, why: String },
    Preview(Option<String>),
    Done,
}

/// What a driver needs from where it runs: the rollout kept, its platform, checks measured, and
/// someone to tell when it moves on. The server ([`Cd`]), or a test.
pub trait Host: Send + Sync + 'static {
    fn drivers(&self) -> &Drivers;
    fn records(&self) -> &dyn Records;
    fn reach(&self, rec: &RolloutRecord) -> Reach;
    fn call<'a>(&'a self, rec: &'a RolloutRecord, op: Op) -> BoxFuture<'a, Result<Answer>>;
    /// Measures check `name` once.
    fn measure<'a>(&'a self, rec: &'a RolloutRecord, _name: &'a str, check: &'a CheckSpec, against: Against) -> BoxFuture<'a, Measurement> {
        Box::pin(async move {
            let timeout = check.timing(false).ok().and_then(|t| t.timeout_ms).map_or(Duration::from_secs(30), Duration::from_millis);
            checks::measure(check, &context(rec, against), timeout).await
        })
    }
    /// The rollout moved on (`was`, before).
    fn changed(&self, rec: &RolloutRecord, was: &Rollout);
    /// How often it reads the platform: while it runs, and otherwise.
    fn every(&self) -> (Duration, Duration) {
        (HOT, REST)
    }
}

// ---------------------------------------------------------------- driving

/// Starts the driver of an object's rollout, unless one runs.
pub fn drive(host: Arc<dyn Host>, target: &str, key: &str) {
    let inbox = host.drivers().inbox.lock();
    inbox_of(&host, inbox, target, key);
}

/// Hands the driver of an object's rollout `msg`, starting one if none runs: whether it took it.
/// Sent under the inbox's lock, so a driver about to leave sees it and stays.
pub fn tell(host: Arc<dyn Host>, target: &str, key: &str, msg: Msg) -> bool {
    let inbox = host.drivers().inbox.lock();
    inbox_of(&host, inbox, target, key).try_send(msg).is_ok()
}

fn inbox_of(host: &Arc<dyn Host>, mut inbox: parking_lot::MutexGuard<'_, HashMap<String, Sender<Msg>>>, target: &str, key: &str) -> Sender<Msg> {
    let id = rollout_id(target, key);
    if let Some(tx) = inbox.get(&id) {
        return tx.clone();
    }
    let (tx, rx) = async_channel::unbounded();
    inbox.insert(id, tx.clone());
    let (h, t, k, me) = (host.clone(), target.to_owned(), key.to_owned(), tx.clone());
    crate::rt::spawn(async move { driver(h, t, k, me, rx).await });
    tx
}

/// A rollout's driver, until nothing is left to do: it drives whichever rollout of the object is
/// kept, so a newer one carries on with it.
async fn driver(host: Arc<dyn Host>, target: String, key: String, tx: Sender<Msg>, rx: Receiver<Msg>) {
    let id = rollout_id(&target, &key);
    let (hot, rest) = host.every();
    let mut next_read = Instant::now();
    let mut started = 0;
    // Something to do: a rollout not done, or what a person asked (a retry of one that is).
    let busy = |host: &Arc<dyn Host>, r: &RolloutRecord, rx: &Receiver<Msg>| (!r.rollout.done() || !rx.is_empty()) && !matches!(host.reach(r), Reach::Never);
    loop {
        let kept = host.records().rollout(&target, &key);
        let reach = kept.as_ref().map_or(Reach::Never, |r| host.reach(r));
        let Some(mut rec) = kept.filter(|r| busy(&host, r, &rx)) else {
            // Nothing to do, unless a rollout started or a message came meanwhile: decided under
            // the inbox's lock, which they're sent under, so they find this driver still here, or
            // none and start one.
            let mut inbox = host.drivers().inbox.lock();
            if host.records().rollout(&target, &key).is_some_and(|r| busy(&host, &r, &rx)) {
                continue;
            }
            inbox.remove(&id);
            host.drivers().approvals.lock().retain(|(r, _), _| r != &id);
            return;
        };
        if rec.started_ms != started {
            // A rollout this driver hasn't driven yet: what the one before waited for goes.
            started = rec.started_ms;
            next_read = Instant::now();
            host.drivers().approvals.lock().retain(|(r, _), (g, _)| r != &id || *g == started);
        }
        if matches!(reach, Reach::Later) {
            crate::rt::sleep(UNREACHABLE.min(rest)).await;
            continue;
        }
        let now = now_ms();
        let read_in = next_read.saturating_duration_since(Instant::now());
        let wait = rec.rollout.deadline().map_or(read_in, |d| read_in.min(Duration::from_millis(d.saturating_sub(now)))).max(LEAST);
        let (from, event) = match crate::rt::timeout(wait, rx.recv()).await {
            Ok(Ok(m)) => m,
            // Never: the driver holds a sender.
            Ok(Err(_)) => (None, Event::Tick),
            Err(_) if Instant::now() >= next_read => {
                let r = &rec.rollout;
                let running = r.state == State::Running || r.moving.is_some() || !r.ready;
                next_read = Instant::now() + if running { hot } else { rest };
                match host.call(&rec, Op::Observe).await {
                    Ok(Answer::Observed { weight, ready, why }) => (None, Event::Observed { weight, ready, why }),
                    Ok(_) => (None, Event::Tick),
                    Err(e) => {
                        tracing::debug!(rollout = %id, error = %format!("{e:#}"), "deployments: reading a rollout's platform failed");
                        (None, Event::Tick)
                    }
                }
            }
            Err(_) => (None, Event::Tick),
        };
        // An answer to an action of a rollout replaced since.
        if from.is_some_and(|g| g != rec.started_ms) {
            continue;
        }
        // A retry rolls green out again: its preview first (it went after keep-failed).
        if matches!(event, Event::Retry) && matches!(rec.rollout.state, State::Aborted { .. }) {
            match host.call(&rec, Op::Preview).await {
                Ok(Answer::Preview(url)) => rec.preview_url = url,
                Ok(_) => {}
                Err(e) => tracing::warn!(rollout = %id, error = %format!("{e:#}"), "deployments: green's preview couldn't be made again"),
            }
        }
        let before = rec.clone();
        let actions = rec.rollout.handle(now_ms(), event, &rec.checks);
        if rec != before {
            rec.updated_ms = now_ms();
            match host.records().keep_rollout(&target, &key, Some(rec.clone()), Some(before.version)).await {
                Ok(true) => host.changed(&rec, &before.rollout),
                // Someone wrote it meanwhile (a newer rollout): read it again.
                Ok(false) => continue,
                Err(e) => {
                    tracing::warn!(rollout = %id, error = %format!("{e:#}"), "deployments: keeping a rollout failed");
                    crate::rt::sleep(Duration::from_secs(1)).await;
                    continue;
                }
            }
        }
        for a in actions {
            act(&host, &id, &rec, a, &tx);
        }
    }
}

/// Does what the state machine said; the answers come back through the inbox.
fn act(host: &Arc<dyn Host>, id: &str, rec: &RolloutRecord, a: Action, tx: &Sender<Msg>) {
    let started = rec.started_ms;
    let (host, rec, tx) = (host.clone(), rec.clone(), tx.clone());
    match a {
        Action::Move { weight, id: ask } => {
            crate::rt::spawn(async move {
                let e = match host.call(&rec, Op::Shift(weight)).await {
                    Ok(_) => Event::Moved { id: ask },
                    Err(e) => Event::MoveFailed { id: ask, why: format!("{e:#}") },
                };
                let _ = tx.send((Some(started), e)).await;
            });
        }
        Action::Measure { check, against, id: ask, .. } => match rec.checks.get(&check).cloned() {
            // A person answers an approval (the API).
            Some(spec) if spec.approve.is_some() => {
                host.drivers().approvals.lock().insert((id.to_owned(), check), (started, ask));
            }
            Some(spec) => {
                crate::rt::spawn(async move {
                    let m = host.measure(&rec, &check, &spec, against).await;
                    let _ = tx.send((Some(started), Event::Measured { id: ask, measurement: m })).await;
                });
            }
            None => {
                let m = Measurement { at_ms: now_ms(), verdict: Verdict::Error, value: Value::Null, message: format!("checks: doesn't define {check}") };
                let _ = tx.try_send((Some(started), Event::Measured { id: ask, measurement: m }));
            }
        },
        Action::Cancel { id: ask } => {
            host.drivers().approvals.lock().retain(|_, v| *v != (started, ask));
        }
        Action::Release { version } => {
            crate::rt::spawn(async move {
                match host.call(&rec, Op::Release).await {
                    Ok(_) => {
                        let _ = tx.send((Some(started), Event::Released { version })).await;
                    }
                    // The state machine asks again in a minute.
                    Err(e) => tracing::warn!(app = %rec.app, error = %format!("{e:#}"), "deployments: letting a rollout's version go failed"),
                }
            });
        }
    }
}

/// The context a rollout's checks read (`rollout.*`).
pub fn context(rec: &RolloutRecord, against: Against) -> Value {
    let r = &rec.rollout;
    let phase = match r.plan.steps.get(r.step) {
        Some(Step::Watch { .. }) => "after",
        _ if r.plan.preview && r.weight == 0 => "before",
        _ => "during",
    };
    json!({
        "app": rec.app, "environment": rec.environment, "target": rec.target, "revision": rec.revision, "previous_revision": rec.previous,
        "weight": r.weight.min(100), "step": r.step + 1, "steps": r.plan.steps.len(), "phase": phase, "against": against,
        "preview_url": rec.preview_url, "stable_url": rec.stable_url, "blue": rec.blue, "green": rec.green,
    })
}

// ---------------------------------------------------------------- the platforms

/// Whether kiln moves an object's traffic in steps on its platform: the kinds rollouts drive.
pub fn rolls_out(provider: &str, kind: &str) -> bool {
    matches!((provider, kind), ("aws-lambda", "Alias"))
}

/// `function:alias`, an Alias's name, apart.
fn function_alias(name: &str) -> Result<(String, String)> {
    name.split_once(':').map(|(f, a)| (f.to_owned(), a.to_owned())).ok_or_else(|| anyhow!("`{name}` isn't function:alias"))
}

fn version_of(v: &Value) -> Result<String> {
    v.as_str().map(str::to_owned).ok_or_else(|| anyhow!("a rollout's version isn't text: {v}"))
}

/// Does `op` on a rollout's platform.
pub async fn platform(cx: &Cx<'_>, rec: &RolloutRecord, op: Op) -> Result<Answer> {
    let (f, a) = function_alias(obj::name(&rec.object))?;
    let (blue, green) = (version_of(&rec.blue)?, version_of(&rec.green)?);
    Ok(match op {
        Op::Observe => {
            let (weight, ready, why) = aws_lambda::observe(cx, &f, &a, &blue, &green).await?;
            Answer::Observed { weight, ready, why }
        }
        Op::Shift(w) => {
            aws_lambda::shift(cx, &f, &a, &blue, &green, w).await?;
            Answer::Done
        }
        Op::Preview => Answer::Preview(aws_lambda::preview(cx, &f, &a, &green).await?),
        Op::Release => {
            aws_lambda::release(cx, &f, &a).await?;
            Answer::Done
        }
    })
}

/// An object that rolled out is gone: its rollout, and what it made beside it, go too.
pub async fn forget(cx: &Cx<'_>, key: &ResKey) -> Result<()> {
    let (target, k) = (cx.conn.target.name(), applied_key(&key.group, &key.kind, &key.namespace, &key.name));
    if cx.records.rollout(&target, &k).is_none() {
        return Ok(());
    }
    let (f, a) = function_alias(&key.name)?;
    aws_lambda::release(cx, &f, &a).await?;
    cx.records.keep_rollout(&target, &k, None, None).await?;
    Ok(())
}

// ---------------------------------------------------------------- starting

/// The rollout and checks a sync stamped on an object (`kiln.dev/rollout`).
fn stamped(o: &Value) -> Result<Option<(RolloutSpec, BTreeMap<String, CheckSpec>)>> {
    let Some(text) = obj::annotation(o, ROLLOUT) else { return Ok(None) };
    let v: Value = serde_json::from_str(text)?;
    let spec: RolloutSpec = serde_json::from_value(v.get("rollout").cloned().unwrap_or(Value::Null))?;
    let checks: BTreeMap<String, CheckSpec> = serde_json::from_value(v.get("checks").cloned().unwrap_or_else(|| json!({})))?;
    Ok(Some((spec, checks)))
}

/// Whether a plan runs a check.
fn uses(plan: &Plan, name: &str) -> bool {
    let named = |names: &Vec<String>| names.iter().any(|n| n == name);
    plan.steps.iter().any(|s| matches!(s, Step::Checks { names, .. } | Step::Watch { names, .. } if named(names))) || plan.background.as_ref().is_some_and(|b| named(&b.names))
}

/// What a rollout starting knows of its app.
pub struct Start {
    pub app: String,
    pub environment: Option<String>,
    /// The revision syncing.
    pub revision: Option<String>,
    /// The app's deployments that succeeded, newest first: (history id, revision).
    pub before: Vec<(u64, Option<String>)>,
}

/// Applies a traffic-carrying object (the module's doc says how): the object as it now reads,
/// and whether a rollout of it is under way for a driver to drive.
pub async fn start(cx: &Cx<'_>, desired: &Value, s: &Start) -> Result<(Value, bool)> {
    let Some((spec, checks)) = stamped(desired)? else { return Ok((targets::apply(cx, desired).await?, false)) };
    let plan = rollout::compile(&spec, &checks).map_err(|e| anyhow!("rollout: {e}"))?;
    for (name, c) in checks.iter().filter(|(n, _)| uses(&plan, n)) {
        checks::measurable(c).map_err(|e| anyhow!("rollout: check {name}: {e}"))?;
    }
    let target = cx.conn.target.name();
    let key = applied_key(obj::group(desired), obj::kind(desired), obj::namespace(desired), obj::name(desired));
    let (function, alias) = function_alias(obj::name(desired))?;
    let staged = aws_lambda::stage(cx, desired).await?;
    let kept = cx.records.rollout(&target, &key);
    // The rollout kept moves traffic to this very version.
    if let Some(r) = kept.as_ref().filter(|r| r.green.as_str() == Some(staged.green.as_str())) {
        let same_plan = stamped(&r.object).ok().flatten().is_some_and(|(was, with)| was == spec && with == checks);
        match &r.rollout.state {
            State::Running | State::Paused { .. } if same_plan => return Ok((read(cx, desired).await?, true)),
            // Its steps or checks changed: it starts again from the first step with them (Argo
            // Rollouts does so when a canary's steps change).
            State::Running | State::Paused { .. } => {
                tracing::info!(app = %s.app, object = %obj::name(desired), "deployments: a rollout's steps or checks changed; it starts again from the first");
            }
            // Aborted: the traffic stays on blue until a person retries or git moves on.
            State::Aborted { .. } => {
                if r.rollout.reverted {
                    let (blue, green) = (version_of(&r.blue)?, version_of(&r.green)?);
                    if aws_lambda::observe(cx, &function, &alias, &blue, &green).await?.0 != Some(0) {
                        tracing::info!(app = %s.app, object = %obj::name(desired), "deployments: traffic moved off blue after its rollout aborted; back to blue");
                        aws_lambda::shift(cx, &function, &alias, &blue, &green, 0).await?;
                    }
                }
                return Ok((read(cx, desired).await?, !r.rollout.done()));
            }
            // It succeeded: green is what runs, all the traffic its own.
            State::Succeeded => return Ok((straight(cx, desired, &function, &alias).await?, !r.rollout.done())),
            State::Superseded { .. } => {}
        }
    }
    let active = kept.filter(|r| !r.rollout.finished());
    let window = plan.rollback_window as usize;
    let going_back = window > 0 && s.revision.as_ref().is_some_and(|rv| s.before.iter().take(window).any(|(_, r)| r.as_ref() == Some(rv)));
    let at_once = match &staged.blue {
        None => Some("the app's first deploy"),
        Some(b) if *b == staged.green => Some("the alias serves that version already"),
        Some(_) if staged.same => Some("green runs what blue does"),
        Some(_) if going_back => Some("it goes back to a recent revision (rollback-window)"),
        Some(_) => None,
    };
    if let Some(why) = at_once {
        if let Some(r) = &active {
            supersede(cx, &target, &key, r, s.revision.as_deref().unwrap_or(&staged.green)).await?;
        }
        tracing::info!(app = %s.app, object = %obj::name(desired), why, "deployments: all the traffic at once, no rollout");
        return Ok((straight(cx, desired, &function, &alias).await?, false));
    }
    // Blue is what had the traffic before any rollout under way: green replaces that one's.
    let blue = active.as_ref().and_then(|r| r.blue.as_str().map(str::to_owned)).or(staged.blue).ok_or_else(|| anyhow!("no version to roll out from"))?;
    aws_lambda::shift(cx, &function, &alias, &blue, &staged.green, 0).await?;
    let preview_url = aws_lambda::preview(cx, &function, &alias, &staged.green).await?;
    let stable_url = aws_lambda::alias_url(cx, &function, &alias).await?;
    let now = now_ms();
    let record = RolloutRecord {
        app: s.app.clone(),
        environment: s.environment.clone(),
        target: target.clone(),
        key: key.clone(),
        object: desired.clone(),
        revision: s.revision.clone(),
        previous: s.before.iter().find(|(_, r)| *r != s.revision).and_then(|(_, r)| r.clone()),
        back_to: s.before.first().map(|(id, _)| *id),
        blue: json!(blue),
        green: json!(staged.green),
        preview_url,
        stable_url,
        rollout: Rollout::new(plan, now),
        checks,
        started_ms: now,
        updated_ms: now,
        version: 0,
    };
    // Over whatever was kept: the rollout it supersedes, or the record of the last one.
    cx.records.keep_rollout(&target, &key, Some(record), None).await?;
    let made = json!({ "FunctionVersion": staged.green, "rollout": rollout_id(&target, &key) });
    cx.record(obj::group(desired), obj::kind(desired), obj::namespace(desired), obj::name(desired), Some(targets::applied(desired, made))).await?;
    tracing::info!(app = %s.app, object = %obj::name(desired), blue = %blue, green = %staged.green, "deployments: a rollout starts");
    Ok((read(cx, desired).await?, true))
}

/// Ends a rollout under way as superseded, with nothing left for it to do: the apply replacing it
/// moves the traffic.
async fn supersede(cx: &Cx<'_>, target: &str, key: &str, r: &RolloutRecord, by: &str) -> Result<()> {
    let mut r = r.clone();
    let now = now_ms();
    r.rollout.handle(now, Event::Supersede { revision: by.to_owned() }, &r.checks);
    r.rollout.moving = None;
    r.rollout.reverted = true;
    r.rollout.release = None;
    r.updated_ms = now;
    cx.records.keep_rollout(target, key, Some(r), None).await.map(|_| ())
}

/// All the traffic to what `desired` says, at once, and green's preview let go.
async fn straight(cx: &Cx<'_>, desired: &Value, function: &str, alias: &str) -> Result<Value> {
    let live = targets::apply(cx, desired).await?;
    aws_lambda::release(cx, function, alias).await?;
    Ok(live)
}

async fn read(cx: &Cx<'_>, desired: &Value) -> Result<Value> {
    targets::read(cx, desired).await?.ok_or_else(|| anyhow!("{} {} is gone", obj::kind(desired), obj::name(desired)))
}

// ---------------------------------------------------------------- the server's

impl Cd {
    /// Applies a traffic-carrying object to a target, rolling it out when its app has a rollout.
    pub(super) async fn rollout_apply(self: &Arc<Self>, cx: &Cx<'_>, desired: &Value) -> Result<Value> {
        let app = self.app_id_of(&targets::app_of(desired).unwrap_or_default());
        let environment = self.apps.read().get(&app).and_then(|e| e.def.environment.clone());
        let revision = self.started.lock().get(&app).and_then(|s| s.revision.clone());
        let before = self.store.read(|d| d.history.get(&app).map(|h| h.iter().rev().filter(|d| d.succeeded).map(|d| (d.id, d.revision.clone())).collect()).unwrap_or_default());
        let (live, rolling) = start(cx, desired, &Start { app: app.clone(), environment, revision, before }).await?;
        if rolling {
            let (target, key) = (cx.conn.target.name(), applied_key(obj::group(desired), obj::kind(desired), obj::namespace(desired), obj::name(desired)));
            self.hub.emit(&json!({ "type": "cd_rollout", "app": app, "id": rollout_id(&target, &key) }));
            drive(self.clone(), &target, &key);
        }
        Ok(live)
    }

    /// The app an object's tracking names: its id, or the name of an app adopted from Argo CD.
    fn app_id_of(&self, tracked: &str) -> String {
        let apps = self.apps.read();
        if apps.contains_key(tracked) {
            return tracked.to_owned();
        }
        apps.iter().find(|(_, e)| e.def.name == tracked).map_or_else(|| tracked.to_owned(), |(id, _)| id.clone())
    }

    /// After a start: drives every rollout not done, asking again what was in flight.
    pub(super) fn resume_rollouts(self: &Arc<Self>) {
        let kept: Vec<(String, String)> = self.store.read(|d| d.rollouts.values().filter(|r| !r.rollout.done()).map(|r| (r.target.clone(), r.key.clone())).collect());
        for (target, key) in kept {
            tell(self.clone(), &target, &key, (None, Event::Restarted));
        }
    }

    /// An app's rollouts, by id.
    pub fn rollouts_of(&self, app: &str) -> Vec<(String, RolloutRecord)> {
        self.store.read(|d| d.rollouts.iter().filter(|(_, r)| r.app == app).map(|(id, r)| (id.clone(), r.clone())).collect())
    }

    /// Hands a rollout what a person did.
    pub fn rollout_event(self: &Arc<Self>, id: &str, event: Event) -> Result<(), (StatusCode, String)> {
        let rec = self.store.read(|d| d.rollouts.get(id).cloned()).ok_or_else(|| (StatusCode::NOT_FOUND, format!("no rollout {id}")))?;
        let r = &rec.rollout;
        let refuse = |why: String| Err((StatusCode::CONFLICT, why));
        match &event {
            Event::Retry if !matches!(r.state, State::Aborted { .. }) => return refuse("only an aborted rollout is retried".into()),
            // Rolled back since: following git again rolls the new version out again.
            Event::Retry if self.store.read(|d| d.pins.contains_key(&rec.app)) => {
                return refuse(format!("{} went back to an earlier deployment after the rollout aborted: let it follow git again (cd.apps.follow) to roll the new version out again", rec.app));
            }
            Event::Retry => {}
            _ if r.finished() => return refuse(format!("the rollout is over ({})", r.summary().get("state").and_then(Value::as_str).unwrap_or_default())),
            _ => {}
        }
        if !tell(self.clone(), &rec.target, &rec.key, (None, event)) {
            return Err((StatusCode::SERVICE_UNAVAILABLE, "the rollout's driver stopped".to_owned()));
        }
        Ok(())
    }

    /// A person's answer to an approval a rollout waits for.
    pub fn rollout_approve(self: &Arc<Self>, id: &str, check: &str, yes: bool, by: &str) -> Result<(), (StatusCode, String)> {
        let rec = self.store.read(|d| d.rollouts.get(id).cloned()).ok_or_else(|| (StatusCode::NOT_FOUND, format!("no rollout {id}")))?;
        let (started, ask) = self
            .rollouts
            .approvals
            .lock()
            .remove(&(id.to_owned(), check.to_owned()))
            .ok_or_else(|| (StatusCode::CONFLICT, format!("rollout {id} waits for no approval of {check}")))?;
        let message = format!("{} by {by}", if yes { "approved" } else { "refused" });
        let measurement = Measurement { at_ms: now_ms(), verdict: if yes { Verdict::Pass } else { Verdict::Fail }, value: json!({ "approved": yes, "by": by }), message };
        if !tell(self.clone(), &rec.target, &rec.key, (Some(started), Event::Measured { id: ask, measurement })) {
            return Err((StatusCode::SERVICE_UNAVAILABLE, "the rollout's driver stopped".to_owned()));
        }
        Ok(())
    }

    /// Reads a rollout's object again for the engine: its health follows the rollout.
    fn reread(self: &Arc<Self>, rec: &RolloutRecord) {
        let (cd, rec) = (self.clone(), rec.clone());
        crate::rt::spawn(async move {
            let Some(conn) = cd.target(&rec.target) else { return };
            let read = async {
                let cx = Cx { conn: &conn, creds: cd.creds(&conn, Some(&rec.app), "watch").await?, records: cd.as_ref(), files: cd.hub.as_ref() };
                targets::read(&cx, &rec.object).await
            };
            match read.await {
                Ok(Some(o)) => cd.input(Input::Live { cluster: rec.target.as_str().into(), change: LiveChange::Upsert(o) }),
                Ok(None) => {}
                Err(e) => tracing::debug!(app = %rec.app, error = %format!("{e:#}"), "deployments: reading a rollout's object failed"),
            }
        });
    }

    /// After an abort: the app back to the deployment that ran before, once no sync runs.
    fn roll_back_after(self: &Arc<Self>, rec: &RolloutRecord, to: u64) {
        let (cd, app) = (self.clone(), rec.app.clone());
        tracing::info!(app = %app, to, "deployments: a rollout aborted; the app goes back to the deployment that ran before");
        crate::rt::spawn(async move {
            for _ in 0..30 {
                match super::ops::roll_back(&cd, &app, Some(to), None, None, "kiln (its rollout aborted)").await {
                    Ok(_) => return,
                    // The sync that started the rollout is still running.
                    Err((StatusCode::CONFLICT, _)) => {
                        if !cd.hub.shutdown.sleep(Duration::from_secs(10)).await {
                            return;
                        }
                    }
                    Err((_, e)) => {
                        tracing::warn!(app = %app, error = %e, "deployments: going back after an aborted rollout failed");
                        return;
                    }
                }
            }
            tracing::warn!(app = %app, "deployments: going back after an aborted rollout gave up: a sync kept running");
        });
    }
}

impl Host for Cd {
    fn drivers(&self) -> &Drivers {
        &self.rollouts
    }

    fn records(&self) -> &dyn Records {
        self
    }

    fn reach(&self, rec: &RolloutRecord) -> Reach {
        if !self.apps.read().contains_key(&rec.app) {
            Reach::Never
        } else if self.target(&rec.target).is_some() {
            Reach::Now
        } else {
            Reach::Later
        }
    }

    fn call<'a>(&'a self, rec: &'a RolloutRecord, op: Op) -> BoxFuture<'a, Result<Answer>> {
        Box::pin(async move {
            let conn = self.target(&rec.target).ok_or_else(|| anyhow!("target {} is not connected", rec.target))?;
            let cx = Cx { conn: &conn, creds: self.creds(&conn, Some(&rec.app), "rollout").await?, records: self, files: self.hub.as_ref() };
            platform(&cx, rec, op).await
        })
    }

    fn changed(&self, rec: &RolloutRecord, was: &Rollout) {
        self.hub.emit(&json!({ "type": "cd_rollout", "app": rec.app, "id": rollout_id(&rec.target, &rec.key) }));
        let Some(cd) = super::get() else { return };
        if rec.rollout.summary() != was.summary() {
            cd.reread(rec);
        }
        let aborted = |r: &Rollout| matches!(r.state, State::Aborted { .. });
        if aborted(&rec.rollout) && !aborted(was) {
            // The rollout's own `auto-rollback`, unless a person set the app's or its repository's.
            let auto = cd.auto_rollback(&rec.app, Some(rec.rollout.plan.auto_rollback));
            match rec.back_to {
                Some(to) if auto.decision.on => cd.roll_back_after(rec, to),
                Some(to) => tracing::info!(app = %rec.app, to, why = %auto.why(), "deployments: a rollout aborted; auto-rollback is off, so the app stays where it is"),
                None => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use kiln_cd::providers::aws_lambda as p;
    use kiln_cd::providers::{RenderInput, Target};
    use serde_json::Map;

    use super::*;
    use crate::cd::targets::fake::{self, Fake, MemFiles, MemRecords};
    use crate::cd::targets::{Conn, Creds};
    use crate::ci::integrations::aws::{self, Aws, Credentials};

    const APP: &str = "default/thumbs-production";

    /// A host on the fake Lambda: rollouts kept in memory, checks answered as the test says.
    struct TestHost {
        drivers: Drivers,
        conn: Conn,
        records: MemRecords,
        files: MemFiles,
        /// Each check's verdict from now on (passing when not said).
        verdicts: Mutex<HashMap<String, Verdict>>,
        /// What was measured: the check, green's share then, against what, the preview's address.
        measured: Mutex<Vec<(String, u8, Against, Option<String>)>>,
        /// Each change: the rollout's state and green's share.
        seen: Mutex<Vec<String>>,
    }

    impl TestHost {
        fn cx(&self) -> Cx<'_> {
            let creds = Credentials { access_key_id: "AKIDFAKE".into(), secret_access_key: "s".into(), session_token: Some("t".into()), expires_at: None };
            Cx { conn: &self.conn, creds: Creds::Aws(Aws { creds, region: "us-east-1".into() }), records: &self.records, files: &self.files }
        }
    }

    impl Host for TestHost {
        fn drivers(&self) -> &Drivers {
            &self.drivers
        }

        fn records(&self) -> &dyn Records {
            &self.records
        }

        fn reach(&self, _: &RolloutRecord) -> Reach {
            Reach::Now
        }

        fn call<'a>(&'a self, rec: &'a RolloutRecord, op: Op) -> BoxFuture<'a, Result<Answer>> {
            Box::pin(async move { platform(&self.cx(), rec, op).await })
        }

        fn measure<'a>(&'a self, rec: &'a RolloutRecord, name: &'a str, _: &'a CheckSpec, against: Against) -> BoxFuture<'a, Measurement> {
            Box::pin(async move {
                let verdict = self.verdicts.lock().get(name).copied().unwrap_or(Verdict::Pass);
                self.measured.lock().push((name.to_owned(), rec.rollout.weight.min(100), against, rec.preview_url.clone()));
                Measurement { at_ms: now_ms(), verdict, value: Value::Null, message: String::new() }
            })
        }

        fn changed(&self, rec: &RolloutRecord, _: &Rollout) {
            let s = rec.rollout.summary();
            self.seen.lock().push(format!("{} {}", s.get("state").and_then(Value::as_str).unwrap_or_default(), rec.rollout.weight));
        }

        fn every(&self) -> (Duration, Duration) {
            (Duration::from_millis(20), Duration::from_millis(40))
        }
    }

    /// The function at `code`, its alias with `rollout` and `checks` stamped as a sync does, and
    /// the alias's URL.
    fn objects(code: &str, rollout: &str, checks: &str) -> anyhow::Result<(Value, Value, Value)> {
        let spec = json!({ "function": "thumbs", "code": code, "set": { "Runtime": "python3.13", "Handler": "app.handler", "Role": "arn:aws:iam::1:role/thumbs" }, "url": { "AuthType": "NONE" } });
        let none = Map::new();
        let objs = p::render(&RenderInput { spec: &spec, target: &none, app: "thumbs", namespace: "", revision: "abc", read: &|_| None }).map_err(|e| anyhow!("{e}"))?;
        let mut objs = objs.into_iter().map(|o| fake::tracked(o, APP));
        let (f, mut a, u) = (objs.next().unwrap_or_default(), objs.next().unwrap_or_default(), objs.next().unwrap_or_default());
        let stamp = json!({ "rollout": serde_yaml::from_str::<Value>(rollout)?, "checks": serde_yaml::from_str::<Value>(checks)? });
        if let Some(ann) = obj::ensure_map(&mut a, &["metadata", "annotations"]) {
            ann.insert(ROLLOUT.into(), Value::String(stamp.to_string()));
        }
        Ok((f, a, u))
    }

    /// A sync of `code`: the function (a new version), the alias through [`start`] (and its
    /// driver), the URL. The alias as it then reads, and whether a rollout is under way.
    async fn deploy(host: &Arc<TestHost>, code: &str, rollout: &str, checks: &str) -> anyhow::Result<(Value, bool)> {
        let (f, a, u) = objects(code, rollout, checks)?;
        let cx = host.cx();
        targets::apply(&cx, &f).await?;
        let (live, rolling) = start(&cx, &a, &Start { app: APP.into(), environment: Some("production".into()), revision: Some(code.into()), before: Vec::new() }).await?;
        if rolling {
            drive(host.clone(), &cx.conn.target.name(), &alias_key());
        }
        targets::apply(&cx, &u).await?;
        Ok((live, rolling))
    }

    fn alias_key() -> String {
        applied_key(p::GROUP, "Alias", "", "thumbs:live")
    }

    fn kept(host: &TestHost) -> Option<RolloutRecord> {
        host.records.rollout(&host.conn.target.name(), &alias_key())
    }

    /// Waits until `f` holds (for at most 10 seconds).
    async fn until(what: &str, f: impl Fn() -> bool) -> anyhow::Result<()> {
        let end = Instant::now() + Duration::from_secs(10);
        while !f() {
            if Instant::now() > end {
                anyhow::bail!("waited too long for {what}");
            }
            crate::rt::sleep(Duration::from_millis(10)).await;
        }
        Ok(())
    }

    /// The alias on the fake: its version, and the weights of others.
    fn alias(fake: &Fake, name: &str) -> Option<(String, Value)> {
        let st = fake.state.lock();
        let a = st.functions.get("thumbs")?.aliases.get(name)?;
        Some((a.get("FunctionVersion")?.as_str()?.to_owned(), a.pointer("/RoutingConfig/AdditionalVersionWeights").cloned().unwrap_or_else(|| json!({}))))
    }

    fn health(o: &Value) -> Option<String> {
        kiln_cd::providers::health(o).map(|s| format!("{:?}", s.health))
    }

    /// A Lambda alias rolled out blue/green, end to end on the fake: the first deploy takes the
    /// traffic at once; the next waits on blue with green's preview checked, ramps to a quarter
    /// and all of it with the after checks, and lets the preview go once it's kept; a failing
    /// check aborts the next back to blue and holds it there, reading as degraded, until a person
    /// retries it; one waiting for a person is superseded by a newer revision, which a person then
    /// promotes.
    #[test]
    fn a_lambda_alias_rolls_out_blue_green() -> anyhow::Result<()> {
        let _one = fake::LAMBDA.lock();
        crate::rt::block_on(async {
            let fake = Arc::new(Fake::default());
            let base = fake::start(fake.clone()).await?;
            for service in ["lambda", "sts", "s3"] {
                aws::override_endpoint(service, &base);
            }
            let target = Target::parse("aws-lambda/aws-test/us-east-1").ok_or_else(|| anyhow!("target"))?;
            let def = target.def().ok_or_else(|| anyhow!("provider"))?;
            let host = Arc::new(TestHost {
                drivers: Drivers::default(),
                conn: Conn::quick(target, def),
                records: MemRecords::default(),
                files: MemFiles::default(),
                verdicts: Mutex::new(HashMap::new()),
                measured: Mutex::new(Vec::new()),
                seen: Mutex::new(Vec::new()),
            });
            let bg = "blue-green: { before: smoke, ramp: [25], after: errors, window: 100ms, keep: 50ms }\nkeep-failed: 50ms";
            let checks = "smoke: { http: { url: '${{ rollout.preview_url }}' }, count: 1 }\nerrors: { http: { url: 'https://errors.example' }, count: 2, every: 20ms }";

            // The first deploy: all the traffic at once, no rollout.
            let (_, rolling) = deploy(&host, "s3://bucket/thumbs/v1.zip", bg, checks).await?;
            assert!(!rolling, "an app's first deploy doesn't roll out");
            assert_eq!(alias(&fake, "live").map(|a| a.0), Some("1".into()));

            // The next: green staged beside blue, which keeps the traffic; the alias reads as
            // applied, its health the rollout's.
            let (live, rolling) = deploy(&host, "s3://bucket/thumbs/v2.zip", bg, checks).await?;
            assert!(rolling);
            assert_eq!(live.pointer("/spec/FunctionVersion"), Some(&json!(p::PUBLISHED)), "the alias reads as applied");
            assert_eq!(health(&live).as_deref(), Some("Progressing"));
            let r = kept(&host).ok_or_else(|| anyhow!("no rollout kept"))?;
            assert_eq!((r.blue.as_str(), r.green.as_str()), (Some("1"), Some("2")));
            assert!(r.preview_url.as_deref().is_some_and(|u| u.contains("live-preview")), "green has a preview URL: {:?}", r.preview_url);
            assert!(r.stable_url.as_deref().is_some_and(|u| u.starts_with("https://thumbs.")), "checks read production's URL too: {:?}", r.stable_url);
            assert_eq!(alias(&fake, "live-preview").map(|a| a.0), Some("2".into()));
            until("the rollout to succeed and let blue go", || kept(&host).is_some_and(|r| r.rollout.state == State::Succeeded && r.rollout.done())).await?;
            assert_eq!(alias(&fake, "live"), Some(("2".into(), json!({}))), "green has all the traffic");
            assert_eq!(alias(&fake, "live-preview"), None, "the preview went once blue was let go");
            let measured: Vec<(String, u8, Against)> = host.measured.lock().iter().map(|(n, w, a, _)| (n.clone(), *w, *a)).collect();
            assert_eq!(measured.first(), Some(&("smoke".to_owned(), 0, Against::Preview)), "green's preview is checked before it takes traffic");
            assert_eq!(measured.iter().filter(|m| **m == ("errors".to_owned(), 25, Against::Production)).count(), 2, "{measured:?}");
            assert!(measured.iter().any(|m| *m == ("errors".to_owned(), 100, Against::Production)), "the window watches all the traffic: {measured:?}");
            assert!(host.measured.lock().first().is_some_and(|m| m.3.as_deref().is_some_and(|u| u.contains("live-preview"))), "checks read the preview's address");

            // A failing check: back to blue, green's preview let go after keep-failed, and the
            // alias holds blue and reads as degraded; syncing it again starts nothing.
            host.verdicts.lock().insert("errors".into(), Verdict::Fail);
            host.measured.lock().clear();
            let (_, rolling) = deploy(&host, "s3://bucket/thumbs/v3.zip", bg, checks).await?;
            assert!(rolling);
            until("the rollout to abort and let green go", || kept(&host).is_some_and(|r| matches!(r.rollout.state, State::Aborted { .. }) && r.rollout.done())).await?;
            assert_eq!(alias(&fake, "live"), Some(("2".into(), json!({}))), "blue has the traffic back");
            assert_eq!(alias(&fake, "live-preview"), None);
            assert!(host.seen.lock().iter().any(|s| s == "running 25"), "green had a quarter before the check failed: {:?}", host.seen.lock());
            let started = kept(&host).map(|r| r.started_ms);
            let (live, rolling) = deploy(&host, "s3://bucket/thumbs/v3.zip", bg, checks).await?;
            assert!(!rolling, "an aborted rollout waits for a person or git");
            assert_eq!(kept(&host).map(|r| r.started_ms), started, "nothing new started");
            assert_eq!(live.pointer("/spec/FunctionVersion"), Some(&json!(p::PUBLISHED)));
            assert_eq!(health(&live).as_deref(), Some("Degraded"));

            // A person retries it, its driver long gone: green's preview again, from the first
            // step, this time to the end.
            host.verdicts.lock().clear();
            assert!(tell(host.clone(), &host.conn.target.name(), &alias_key(), (None, Event::Retry)));
            until("the retried rollout to succeed", || kept(&host).is_some_and(|r| r.rollout.state == State::Succeeded && r.rollout.done())).await?;
            assert_eq!(alias(&fake, "live"), Some(("3".into(), json!({}))));
            assert_eq!(kept(&host).map(|r| r.started_ms), started, "the same rollout, retried");

            // One waiting for a person, superseded by a newer revision: that one rolls out from
            // the same blue, and a person promotes it.
            let manual = "blue-green: { before: smoke, promote: manual, after: errors, window: 50ms, keep: 50ms }";
            deploy(&host, "s3://bucket/thumbs/v4.zip", manual, checks).await?;
            until("version 4 to wait for a person", || kept(&host).is_some_and(|r| matches!(r.rollout.state, State::Paused { .. }))).await?;
            let waiting = targets::read(&host.cx(), &objects("s3://bucket/thumbs/v4.zip", manual, checks)?.1).await?.unwrap_or_default();
            assert_eq!(health(&waiting).as_deref(), Some("Suspended"));
            deploy(&host, "s3://bucket/thumbs/v5.zip", manual, checks).await?;
            let r = kept(&host).ok_or_else(|| anyhow!("no rollout kept"))?;
            assert_eq!((r.blue.as_str(), r.green.as_str()), (Some("3"), Some("5")), "the newer rollout starts from the same blue");
            assert_eq!(alias(&fake, "live-preview").map(|a| a.0), Some("5".into()));
            until("version 5 to wait for a person", || kept(&host).is_some_and(|r| matches!(r.rollout.state, State::Paused { .. }))).await?;

            // Synced again as it is, it carries on; with its steps changed, it starts again from
            // the first, as Argo Rollouts does.
            let started = kept(&host).map(|r| r.started_ms);
            deploy(&host, "s3://bucket/thumbs/v5.zip", manual, checks).await?;
            assert_eq!(kept(&host).map(|r| r.started_ms), started, "the same rollout carries on");
            let changed = "blue-green: { before: smoke, promote: manual, after: errors, window: 60ms, keep: 50ms }";
            deploy(&host, "s3://bucket/thumbs/v5.zip", changed, checks).await?;
            let r = kept(&host).ok_or_else(|| anyhow!("no rollout kept"))?;
            assert_ne!(Some(r.started_ms), started, "new steps start it again");
            assert!(r.rollout.plan.steps.iter().any(|s| matches!(s, Step::Watch { window_ms: 60, .. })), "with the new steps");
            assert_eq!((r.blue.as_str(), r.green.as_str()), (Some("3"), Some("5")));
            until("version 5 to wait for a person again", || kept(&host).is_some_and(|r| matches!(r.rollout.state, State::Paused { .. }))).await?;
            assert!(tell(host.clone(), &host.conn.target.name(), &alias_key(), (None, Event::Promote { full: false })));
            until("version 5 to succeed", || kept(&host).is_some_and(|r| r.rollout.state == State::Succeeded && r.rollout.done())).await?;
            assert_eq!(alias(&fake, "live"), Some(("5".into(), json!({}))));
            Ok(())
        })
    }
}
