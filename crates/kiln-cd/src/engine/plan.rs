//! Plans: what deploying another render of an app would do, worked out from what the engine
//! already holds (the cluster's scopes, its live objects, the app's tracking and ignore rules)
//! without changing any of it. The owner renders an app's sources at a commit that isn't
//! deployed and asks [`Engine::plan`]; the answer is that render as the app would apply it, and
//! the steps a sync of it would take, in the order the sync takes them ([`sync::plan`]).

use std::sync::Arc;

use rustc_hash::{FxHashMap, FxHashSet};
use serde_json::Value;

use super::{App, Change, Engine, differs, recombine, render_of};
use crate::diff::{self, Ignores, LiveView, Verdict};
use crate::model::PruneMode;
use crate::obj::{self, GroupKind, ResKey, Str};
use crate::sync::{self, Phase};

/// One object of a planned render, as the app would apply it.
#[derive(Clone, Debug)]
pub struct Planned {
    /// Its key.
    pub key: ResKey,
    /// The object, with the app's namespace and tracking metadata.
    pub object: Value,
    /// A hash of `object`, so two renders' objects compare without a walk.
    pub hash: u64,
    /// It is a hook: a sync runs it rather than keeping it deployed.
    pub hook: bool,
}

/// What one step of a planned sync does.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StepKind {
    /// Creates an object the cluster doesn't have.
    Create,
    /// Changes one it has.
    Update,
    /// Deletes one git no longer holds, when the sync prunes.
    Prune,
    /// Runs a hook.
    Hook,
}

impl StepKind {
    /// The step's name in the API.
    pub fn as_str(self) -> &'static str {
        match self {
            StepKind::Create => "create",
            StepKind::Update => "update",
            StepKind::Prune => "prune",
            StepKind::Hook => "hook",
        }
    }
}

/// One step of a planned sync.
#[derive(Clone, Debug)]
pub struct Step {
    /// The object.
    pub key: ResKey,
    /// What the step does to it.
    pub kind: StepKind,
    /// Its phase.
    pub phase: Phase,
    /// Its wave.
    pub wave: i64,
    /// The live object as kiln compares it, and what the step makes of it ([`Engine::change`]).
    pub change: Change,
    /// Where an update differs (`.spec.replicas differs`).
    pub why: Option<String>,
}

/// What deploying a render would do ([`Engine::plan`]).
#[derive(Clone, Debug, Default)]
pub struct Plan {
    /// The render's key: the app's own render's, or 0 for a render given to plan.
    pub key: u64,
    /// Every object the render deploys, as the app applies it, hooks included, in key order.
    pub objects: Vec<Planned>,
    /// What a sync does, in its order: the objects it creates or changes, the hooks it runs, and
    /// the objects it prunes when it prunes.
    pub steps: Vec<Step>,
    /// Objects git no longer holds that a sync leaves in place: kept by `Prune=false`, or waiting
    /// for a person to confirm (`Prune=confirm`).
    pub held: Vec<(ResKey, PruneMode)>,
    /// Objects the cluster already has as the render has them.
    pub unchanged: u32,
    /// Objects of kinds not read from the cluster yet: what a sync does to them isn't known.
    pub unread: Vec<ResKey>,
    /// Objects another app deploys too, or whose live object carries another app's tracking,
    /// with that app's id: a sync of this one takes them over.
    pub shared: Vec<(ResKey, Str)>,
}

/// Objects the owner read from a cluster itself, by key: `None`, not there.
pub type Read = FxHashMap<ResKey, Option<Value>>;

/// One object as the sync planner sees it, with what a sync would make of it.
struct Candidate {
    key: ResKey,
    /// Its index in the plan's objects, if the render has it.
    desired: Option<usize>,
    live: Option<Value>,
    out_of_sync: bool,
    change: Change,
    why: Option<String>,
}

impl Engine {
    /// What deploying `objects` would do to an app. `objects` is a render of its sources at some
    /// other commit, as [`super::Input::Rendered`] takes one; `None` plans the render the app
    /// compares against now (its drift). Nothing in the engine changes, and the cluster is as
    /// last watched. Objects of kinds the engine hasn't listed in the cluster yet are
    /// [`Plan::unread`], unless `read` has them: what the owner read from the cluster itself
    /// (`None`, not there). `None` back means there is no such app, or (for its own render) it
    /// has none yet.
    pub fn plan(&self, id: &str, objects: Option<Vec<Value>>, read: &Read) -> Option<Plan> {
        let a = *self.app_by_id.get(id)?;
        let app = self.app(a)?;
        let render = match objects {
            Some(objects) => Arc::new(render_of(0, objects, &mut |s| Str::from(s))),
            None => app.render.clone()?,
        };
        let c = app.cluster;
        let cl = self.clusters.get(c as usize)?;
        // Keys as the app's sync gives them: the cluster's scopes first, then the render's CRDs.
        let namespaced = |group: &Str, kind: &Str| {
            let gk = GroupKind { group: group.clone(), kind: kind.clone() };
            match render.scopes.iter().find(|(x, _)| *x == gk) {
                Some((_, ns)) if !cl.scopes.knows(&gk) => *ns,
                _ => cl.scopes.namespaced(group, kind),
            }
        };
        let namespace = self.namespace_render(&app.cfg, &render);
        let mut objects: Vec<Planned> = Vec::with_capacity(render.objects.len() + 1);
        let mut at: FxHashMap<ResKey, usize> = FxHashMap::default();
        for r in render.objects.iter().chain(namespace.iter().flat_map(|n| n.objects.iter())) {
            let ns = if namespaced(&r.group, &r.kind) {
                if r.namespace.is_empty() { app.cfg.namespace.clone() } else { r.namespace.clone() }
            } else {
                Str::from("")
            };
            let key = ResKey { group: r.group.clone(), kind: r.kind.clone(), namespace: ns, name: r.name.clone() };
            let Ok(mut v) = r.blob.decode() else { continue };
            self.decorate(app, &key, &mut v);
            let p = Planned { hash: obj::hash_value(&v), key, object: v, hook: r.hook };
            // Two objects with one key: the later one is what the app deploys.
            match (p.hook, at.get(&p.key)) {
                (false, Some(&i)) => {
                    if let Some(slot) = objects.get_mut(i) {
                        *slot = p;
                    }
                }
                (false, None) => {
                    at.insert(p.key.clone(), objects.len());
                    objects.push(p);
                }
                (true, _) => objects.push(p),
            }
        }
        let mut plan = Plan { key: render.key, ..Plan::default() };
        let mut cands: Vec<Candidate> = Vec::with_capacity(objects.len());
        for (i, p) in objects.iter().enumerate() {
            let create = || Change { before: None, after: Some(p.object.clone()) };
            if p.hook {
                cands.push(Candidate { key: p.key.clone(), desired: Some(i), live: None, out_of_sync: true, change: create(), why: None });
                continue;
            }
            let slots: Vec<_> = self.slots(c, &p.key).iter().collect();
            let seen = slots.iter().filter_map(|ix| self.res(c, *ix)).find_map(|r| r.live.as_ref());
            let other = slots.iter().filter_map(|ix| self.res(c, *ix)).find(|r| r.app != Some(a) && r.desired.is_some()).and_then(|r| r.app).or_else(|| seen.and_then(|l| l.tracked).filter(|t| *t != a));
            if let Some(o) = other {
                plan.shared.push((p.key.clone(), self.app(o).map(|x| x.cfg.id.clone()).unwrap_or_else(|| Str::from(""))));
            }
            let watched = seen.and_then(|l| l.blob.decode().ok());
            let live = match watched {
                Some(live) => Some(live),
                None if cl.listed.contains(&p.key.group_kind()) => None,
                None => match read.get(&p.key) {
                    // Read by the owner: kept as a watch keeps it, and claimed as a watch claims it.
                    Some(Some(v)) => {
                        if let Some(other) = self.owner_of(&p.key, v).filter(|o| *o != a) {
                            plan.shared.push((p.key.clone(), self.app(other).map(|x| x.cfg.id.clone()).unwrap_or_else(|| Str::from(""))));
                        }
                        let keep = |m: &str| app.cfg.ignore.iter().any(|r| r.managers.iter().any(|x| x == m));
                        Some(recombine(LiveView::keeping(v.clone(), &app.cfg.manager, keep)))
                    }
                    Some(None) => None,
                    None => {
                        plan.unread.push(p.key.clone());
                        continue;
                    }
                },
            };
            match live {
                None => cands.push(Candidate { key: p.key.clone(), desired: Some(i), live: None, out_of_sync: true, change: create(), why: None }),
                Some(live) => {
                    let (verdict, change) = compare(app, &p.object, LiveView::new(live.clone(), &app.cfg.manager));
                    let (out_of_sync, why) = match verdict {
                        Verdict::Synced => (false, None),
                        Verdict::OutOfSync { path, why } => (true, Some(differs(&path, why))),
                    };
                    if !out_of_sync {
                        plan.unchanged += 1;
                    }
                    cands.push(Candidate { key: p.key.clone(), desired: Some(i), live: Some(live), out_of_sync, change, why });
                }
            }
        }
        // What git no longer holds: the app's live objects the render leaves out.
        let kept: FxHashSet<&ResKey> = objects.iter().filter(|p| !p.hook).map(|p| &p.key).collect();
        let mut gone: Vec<Candidate> = Vec::new();
        for ix in &app.resources {
            let Some(res) = self.res(c, *ix) else { continue };
            // Pruned only when it carries this app's tracking and no other app deploys it.
            let own = res.live.as_ref().is_some_and(|l| l.tracked == Some(a));
            if res.app != Some(a) || kept.contains(&res.key) || !own || self.other_deployer(c, &res.key, a).is_some() {
                continue;
            }
            let Some(live) = res.live.as_ref().filter(|l| !l.hook).and_then(|l| l.blob.decode().ok()) else { continue };
            let before = LiveView::new(live.clone(), &app.cfg.manager).object;
            gone.push(Candidate { key: res.key.clone(), desired: None, live: Some(live), out_of_sync: true, change: Change { before: Some(before), after: None }, why: None });
        }
        gone.sort_by(|x, y| x.key.cmp(&y.key));
        cands.extend(gone);
        let for_sync: Vec<sync::Candidate<'_>> = cands
            .iter()
            .map(|x| sync::Candidate {
                key: &x.key,
                desired: x.desired.and_then(|i| objects.get(i)).map(|p| &p.object),
                live: x.live.as_ref(),
                out_of_sync: x.out_of_sync,
            })
            .collect();
        let planned = sync::plan(&for_sync, sync::Options { prune: true, sync: app.cfg.sync.options }, None);
        let hooks: FxHashMap<&ResKey, &Candidate> = cands.iter().filter(|x| x.desired.is_some_and(|i| objects.get(i).is_some_and(|p| p.hook))).map(|x| (&x.key, x)).collect();
        let rest: FxHashMap<&ResKey, &Candidate> = cands.iter().filter(|x| !x.desired.is_some_and(|i| objects.get(i).is_some_and(|p| p.hook))).map(|x| (&x.key, x)).collect();
        for t in planned.tasks {
            let (kind, x) = match t.action {
                sync::Action::Hook => (StepKind::Hook, hooks.get(&t.key)),
                sync::Action::Prune => (StepKind::Prune, rest.get(&t.key)),
                sync::Action::Apply => match rest.get(&t.key) {
                    Some(x) if !x.out_of_sync => continue,
                    Some(x) if x.live.is_none() => (StepKind::Create, Some(x)),
                    x => (StepKind::Update, x),
                },
            };
            let Some(x) = x else { continue };
            plan.steps.push(Step { key: t.key, kind, phase: t.phase, wave: t.wave, change: x.change.clone(), why: x.why.clone() });
        }
        plan.held = planned.held;
        drop(for_sync);
        objects.sort_by(|x, y| x.key.cmp(&y.key).then(x.hook.cmp(&y.hook)));
        plan.objects = objects;
        Some(plan)
    }
}

/// Compares a desired object with a live one as the app's status does (its ignore rules applied
/// to both sides), and, when they differ, works out what applying it makes of the live one.
fn compare(app: &App, desired: &Value, view: LiveView) -> (Verdict, Change) {
    let owned = view.owned(&app.cfg.manager);
    let mut d = desired.clone();
    let mut before = view.object.clone();
    let (ignores, _) = Ignores::for_object(app.cfg.ignore.iter(), &d);
    if !ignores.is_empty() {
        ignores.strip(&mut d, Some(&view));
        ignores.strip(&mut before, Some(&view));
    }
    match diff::compare(&d, &before, owned.as_ref()) {
        Verdict::Synced => (Verdict::Synced, Change { before: None, after: None }),
        verdict => {
            let after = diff::predict(&d, &before, owned.as_ref());
            (verdict, Change { before: Some(before), after: Some(after) })
        }
    }
}
