//! Planning a sync: which objects to apply, prune and run as hooks, and in what order. The
//! order is Argo CD's (gitops-engine `pkg/sync`), so a repository written for Argo CD's waves
//! and hooks deploys the same way:
//!
//! 1. by phase: `PreSync`, `Sync`, `PostSync` (`SyncFail` only when the sync fails);
//! 2. by wave (`argocd.argoproj.io/sync-wave`, or a Helm hook's `helm.sh/hook-weight`);
//! 3. by kind, Namespaces and CRDs first and custom kinds last ([`KIND_ORDER`]);
//! 4. by name.
//!
//! A Namespace or CRD that sorts after an object needing it moves ahead of that object and takes
//! its phase and wave. Prunes run in waves mirrored from creation order, so what was created last
//! goes first; `PruneLast` moves a prune past the last wave of the `Sync` phase.
//!
//! Running the plan is the engine's ([`crate::engine`]): it applies one wave, waits until every
//! object of it is healthy, then moves on. [`next_group`] says which wave that is.

use std::cmp::Ordering;

use rustc_hash::{FxHashMap, FxHashSet};
use serde_json::Value;

use crate::model::{PruneMode, SyncOptions};
use crate::obj::{self, ResKey};

/// The phases of a sync, in order.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Phase {
    /// Before anything is applied (migrations).
    PreSync,
    /// The objects themselves, and `Sync` hooks.
    Sync,
    /// After every object is applied and healthy.
    PostSync,
    /// Only when the sync failed.
    SyncFail,
    /// After the app is deleted.
    PostDelete,
}

impl Phase {
    /// Argo CD's name for the phase.
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::PreSync => "PreSync",
            Phase::Sync => "Sync",
            Phase::PostSync => "PostSync",
            Phase::SyncFail => "SyncFail",
            Phase::PostDelete => "PostDelete",
        }
    }

    fn parse(s: &str) -> Option<Phase> {
        Some(match s {
            "PreSync" => Phase::PreSync,
            "Sync" => Phase::Sync,
            "PostSync" => Phase::PostSync,
            "SyncFail" => Phase::SyncFail,
            "PostDelete" => Phase::PostDelete,
            _ => return None,
        })
    }
}

/// When a hook object is deleted (`hook-delete-policy`).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct HookDelete {
    /// Delete the previous one before creating it (the default).
    pub before_creation: bool,
    /// Delete it once it succeeded.
    pub on_success: bool,
    /// Delete it once it failed.
    pub on_failure: bool,
}

/// What an object's hook annotations say.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Hook {
    /// The phases it runs in; empty for Helm hooks of types Argo CD doesn't run (`test`).
    pub phases: Vec<Phase>,
    /// When it is deleted.
    pub delete: HookDelete,
}

/// The annotation naming an object's hook phases.
pub const HOOK: &str = "argocd.argoproj.io/hook";
/// The annotation naming a hook's delete policies.
pub const HOOK_DELETE_POLICY: &str = "argocd.argoproj.io/hook-delete-policy";
/// The annotation naming an object's wave.
pub const SYNC_WAVE: &str = "argocd.argoproj.io/sync-wave";
/// kiln's spelling of the wave annotation.
pub const KILN_WAVE: &str = "kiln.dev/wave";
/// The annotation holding an object's own sync options.
pub const SYNC_OPTIONS: &str = "argocd.argoproj.io/sync-options";

fn csv<'a>(obj: &'a Value, key: &str) -> Vec<&'a str> {
    obj::annotation(obj, key).map(|v| v.split(',').map(str::trim).filter(|s| !s.is_empty()).collect()).unwrap_or_default()
}

/// Whether the object is not applied at all (`argocd.argoproj.io/hook: Skip` alone).
pub fn is_skipped(obj: &Value) -> bool {
    let types = csv(obj, HOOK);
    types == ["Skip"]
}

/// The object's hook, or `None` for an ordinary object.
pub fn hook_of(obj: &Value) -> Option<Hook> {
    let argo = csv(obj, HOOK);
    let phases: Vec<Phase> = if !argo.is_empty() {
        if argo == ["Skip"] {
            return None;
        }
        argo.iter().filter_map(|t| Phase::parse(t)).collect()
    } else {
        let helm = obj::annotation(obj, "helm.sh/hook")?;
        if helm == "crd-install" {
            return None;
        }
        helm.split(',')
            .filter_map(|t| match t.trim() {
                "pre-install" | "pre-upgrade" => Some(Phase::PreSync),
                "post-install" | "post-upgrade" => Some(Phase::PostSync),
                _ => None,
            })
            .collect()
    };
    let mut delete = HookDelete::default();
    let mut any = false;
    for p in csv(obj, HOOK_DELETE_POLICY).into_iter().chain(csv(obj, "helm.sh/hook-delete-policy")) {
        match p {
            "BeforeHookCreation" | "before-hook-creation" => delete.before_creation = true,
            "HookSucceeded" | "hook-succeeded" => delete.on_success = true,
            "HookFailed" | "hook-failed" => delete.on_failure = true,
            _ => continue,
        }
        any = true;
    }
    if !any {
        delete.before_creation = true;
    }
    let mut phases = phases;
    phases.sort();
    phases.dedup();
    Some(Hook { phases, delete })
}

/// The object's wave: `argocd.argoproj.io/sync-wave`, `kiln.dev/wave`, or a Helm hook weight.
pub fn wave_of(obj: &Value) -> i64 {
    for key in [SYNC_WAVE, KILN_WAVE, "helm.sh/hook-weight"] {
        if let Some(w) = obj::annotation(obj, key).and_then(|t| t.trim().parse().ok()) {
            return w;
        }
    }
    0
}

/// The object's own sync options.
pub fn options_of(obj: &Value) -> SyncOptions {
    match obj::annotation(obj, SYNC_OPTIONS) {
        Some(text) => SyncOptions::parse([text]).0,
        None => SyncOptions::default(),
    }
}

/// Kinds in the order Argo CD (and Helm) creates them; other kinds go after all of these.
pub const KIND_ORDER: &[&str] = &[
    "Namespace",
    "NetworkPolicy",
    "ResourceQuota",
    "LimitRange",
    "PodSecurityPolicy",
    "PodDisruptionBudget",
    "ServiceAccount",
    "Secret",
    "SecretList",
    "ConfigMap",
    "StorageClass",
    "PersistentVolume",
    "PersistentVolumeClaim",
    "CustomResourceDefinition",
    "ClusterRole",
    "ClusterRoleList",
    "ClusterRoleBinding",
    "ClusterRoleBindingList",
    "Role",
    "RoleList",
    "RoleBinding",
    "RoleBindingList",
    "Service",
    "DaemonSet",
    "Pod",
    "ReplicationController",
    "ReplicaSet",
    "Deployment",
    "HorizontalPodAutoscaler",
    "StatefulSet",
    "Job",
    "CronJob",
    "IngressClass",
    "Ingress",
    "APIService",
];

/// Where a kind sorts: known kinds by [`KIND_ORDER`], every other kind after them.
pub fn kind_rank(kind: &str) -> usize {
    KIND_ORDER.iter().position(|k| *k == kind).unwrap_or(KIND_ORDER.len())
}

/// What a task does.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    /// Apply the desired object.
    Apply,
    /// Delete a live object git no longer holds.
    Prune,
    /// Create (or recreate) a hook and wait for its result.
    Hook,
}

/// One step of a sync.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Task {
    /// What it does.
    pub action: Action,
    /// The object.
    pub key: ResKey,
    /// Its phase.
    pub phase: Phase,
    /// Its wave (after prune mirroring and dependency moves).
    pub wave: i64,
    /// Its kind's rank.
    pub rank: usize,
}

impl Task {
    fn cmp_order(&self, other: &Task) -> Ordering {
        self.phase
            .cmp(&other.phase)
            .then(self.wave.cmp(&other.wave))
            .then(self.rank.cmp(&other.rank))
            .then(self.key.name.cmp(&other.key.name))
    }
}

/// An object the plan may touch.
pub struct Candidate<'a> {
    /// Its key.
    pub key: &'a ResKey,
    /// The desired object, if git holds it.
    pub desired: Option<&'a Value>,
    /// The live object (its annotations at least), if it exists.
    pub live: Option<&'a Value>,
    /// Whether it differs from desired (for `ApplyOutOfSyncOnly`).
    pub out_of_sync: bool,
}

/// How to plan.
#[derive(Clone, Copy, Debug, Default)]
pub struct Options {
    /// Prune what git no longer holds.
    pub prune: bool,
    /// The sync's options, over the app's.
    pub sync: SyncOptions,
}

/// A planned sync.
#[derive(Clone, Debug, Default)]
pub struct Plan {
    /// The tasks, in order.
    pub tasks: Vec<Task>,
    /// Objects that need pruning but are kept (`Prune=false`) or wait for a person
    /// (`Prune=confirm`).
    pub held: Vec<(ResKey, PruneMode)>,
    /// `SyncFail` hooks, run only if the sync fails.
    pub on_fail: Vec<Task>,
}

/// Plans a sync over the app's objects. `selected` limits it to some objects (a selective sync).
pub fn plan(cands: &[Candidate<'_>], opts: Options, selected: Option<&FxHashSet<ResKey>>) -> Plan {
    let mut out = Plan::default();
    let only_out_of_sync = opts.sync.apply_out_of_sync_only.unwrap_or(false);
    for c in cands {
        if selected.is_some_and(|s| !s.contains(c.key)) {
            continue;
        }
        let rank = kind_rank(&c.key.kind);
        match (c.desired, c.live) {
            (Some(d), _) => {
                if is_skipped(d) {
                    continue;
                }
                if let Some(hook) = hook_of(d) {
                    for phase in hook.phases {
                        let task = Task { action: Action::Hook, key: c.key.clone(), phase, wave: wave_of(d), rank };
                        if phase == Phase::SyncFail {
                            out.on_fail.push(task);
                        } else if phase != Phase::PostDelete {
                            out.tasks.push(task);
                        }
                    }
                    continue;
                }
                if only_out_of_sync && !c.out_of_sync {
                    continue;
                }
                out.tasks.push(Task { action: Action::Apply, key: c.key.clone(), phase: Phase::Sync, wave: wave_of(d), rank });
            }
            (None, Some(l)) => {
                if hook_of(l).is_some() {
                    continue;
                }
                let own = options_of(l).or(opts.sync);
                match own.prune.unwrap_or(PruneMode::Allow) {
                    PruneMode::Allow if opts.prune => {
                        out.tasks.push(Task { action: Action::Prune, key: c.key.clone(), phase: Phase::Sync, wave: wave_of(l), rank })
                    }
                    PruneMode::Allow => {}
                    mode => out.held.push((c.key.clone(), mode)),
                }
            }
            (None, None) => {}
        }
    }
    mirror_prune_waves(&mut out.tasks);
    prune_last(&mut out.tasks, cands, opts.sync);
    out.tasks.sort_by(Task::cmp_order);
    adjust_dependencies(&mut out.tasks, cands);
    out.on_fail.sort_by(Task::cmp_order);
    out
}

/// Prune waves are swapped symmetrically, so the last wave created is the first pruned.
fn mirror_prune_waves(tasks: &mut [Task]) {
    let mut waves: Vec<i64> = tasks.iter().filter(|t| t.action == Action::Prune).map(|t| t.wave).collect();
    waves.sort_unstable();
    waves.dedup();
    let n = waves.len();
    let swap: FxHashMap<i64, i64> =
        waves.iter().enumerate().filter_map(|(i, w)| Some((*w, *waves.get(n.checked_sub(1)?.checked_sub(i)?)?))).collect();
    for t in tasks.iter_mut().filter(|t| t.action == Action::Prune) {
        if let Some(w) = swap.get(&t.wave) {
            t.wave = *w;
        }
    }
}

fn prune_last(tasks: &mut [Task], cands: &[Candidate<'_>], sync: SyncOptions) {
    let last = tasks.iter().filter(|t| t.phase == Phase::Sync).map(|t| t.wave).max().unwrap_or(0).max(0) + 1;
    let live_opts: FxHashMap<&ResKey, SyncOptions> =
        cands.iter().filter_map(|c| Some((c.key, options_of(c.live?)))).collect();
    for t in tasks.iter_mut().filter(|t| t.action == Action::Prune) {
        let own = live_opts.get(&t.key).copied().unwrap_or_default();
        if own.prune_last.or(sync.prune_last).unwrap_or(false) {
            t.wave = last;
        }
    }
}

/// Moves each Namespace before the first task in it, and each CRD before the first object of
/// its kind, taking that task's phase and wave (gitops-engine's `adjustDeps`).
fn adjust_dependencies(tasks: &mut Vec<Task>, cands: &[Candidate<'_>]) {
    let desired: FxHashMap<&ResKey, &Value> = cands.iter().filter_map(|c| Some((c.key, c.desired?))).collect();
    adjust(tasks, |t| (t.key.kind.as_ref() == "Namespace" && t.key.group.is_empty()).then(|| t.key.name.to_string()), |t| {
        (!t.key.namespace.is_empty()).then(|| t.key.namespace.to_string())
    });
    adjust(
        tasks,
        |t| {
            if t.key.kind.as_ref() != "CustomResourceDefinition" {
                return None;
            }
            let d = desired.get(&t.key)?;
            let group = obj::str_at(d, &["spec", "group"])?;
            let kind = obj::str_at(d, &["spec", "names", "kind"])?;
            Some(format!("{group}/{kind}"))
        },
        |t| Some(format!("{}/{}", t.key.group, t.key.kind)),
    );
}

fn adjust(tasks: &mut Vec<Task>, is_dep: impl Fn(&Task) -> Option<String>, refs: impl Fn(&Task) -> Option<String>) {
    let mut first: FxHashMap<String, usize> = FxHashMap::default();
    let mut i = 0;
    while i < tasks.len() {
        let Some(t) = tasks.get(i) else { break };
        if t.action == Action::Prune {
            i += 1;
            continue;
        }
        if let Some(dep) = is_dep(t) {
            if let Some(&index) = first.get(&dep) {
                let (wave, phase) = match tasks.get(index) {
                    Some(target) => (target.wave, target.phase),
                    None => break,
                };
                let mut moved = tasks.remove(i);
                moved.wave = wave;
                moved.phase = phase;
                tasks.insert(index, moved);
                for v in first.values_mut() {
                    if *v >= index {
                        *v += 1;
                    }
                }
            }
        } else if let Some(dep) = refs(t) {
            first.entry(dep).or_insert(i);
        }
        i += 1;
    }
}

/// Where a task stands while the plan runs.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum TaskState {
    /// Not started.
    #[default]
    Pending,
    /// Sent to the cluster.
    Running,
    /// Applied (or deleted, or the hook succeeded), not yet confirmed healthy.
    Done,
    /// Done, and its object is healthy (or has no health), so the next wave may start.
    Settled,
    /// Failed.
    Failed,
}

/// The group (phase and wave) to work on next: the first one with a task not settled.
/// `None` when every task is settled.
pub fn next_group(tasks: &[Task], states: &[TaskState]) -> Option<(Phase, i64)> {
    tasks
        .iter()
        .zip(states)
        .find(|(_, s)| **s != TaskState::Settled)
        .map(|(t, _)| (t.phase, t.wave))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn key(kind: &str, ns: &str, name: &str) -> ResKey {
        let group = if kind == "Deployment" { "apps" } else if kind == "CustomResourceDefinition" { "apiextensions.k8s.io" } else { "" };
        ResKey::new(group, kind, ns, name)
    }

    fn obj(kind: &str, name: &str, annotations: Value) -> Value {
        json!({"kind": kind, "metadata": {"name": name, "annotations": annotations}})
    }

    #[test]
    fn orders_by_phase_wave_kind_and_name() {
        let k = [key("Deployment", "w", "web"), key("Service", "w", "web"), key("ConfigMap", "w", "b"), key("ConfigMap", "w", "a"),
            key("Job", "w", "migrate"), key("Widget", "w", "x")];
        let d = [
            obj("Deployment", "web", json!({})),
            obj("Service", "web", json!({})),
            obj("ConfigMap", "b", json!({})),
            obj("ConfigMap", "a", json!({SYNC_WAVE: "1"})),
            obj("Job", "migrate", json!({HOOK: "PreSync"})),
            obj("Widget", "x", json!({})),
        ];
        let cands: Vec<Candidate> =
            k.iter().zip(&d).map(|(k, d)| Candidate { key: k, desired: Some(d), live: None, out_of_sync: true }).collect();
        let p = plan(&cands, Options::default(), None);
        let order: Vec<(&str, &str)> = p.tasks.iter().map(|t| (t.key.kind.as_ref(), t.key.name.as_ref())).collect();
        assert_eq!(order, [("Job", "migrate"), ("ConfigMap", "b"), ("Service", "web"), ("Deployment", "web"), ("Widget", "x"), ("ConfigMap", "a")]);
    }

    #[test]
    fn prunes_mirror_waves_and_honor_prune_last_and_prune_false() {
        let k = [key("ConfigMap", "w", "early"), key("ConfigMap", "w", "late"), key("ConfigMap", "w", "keep"), key("ConfigMap", "w", "last")];
        let live = [
            obj("ConfigMap", "early", json!({SYNC_WAVE: "-1"})),
            obj("ConfigMap", "late", json!({SYNC_WAVE: "5"})),
            obj("ConfigMap", "keep", json!({SYNC_OPTIONS: "Prune=false"})),
            obj("ConfigMap", "last", json!({SYNC_OPTIONS: "PruneLast=true"})),
        ];
        let cands: Vec<Candidate> =
            k.iter().zip(&live).map(|(k, l)| Candidate { key: k, desired: None, live: Some(l), out_of_sync: true }).collect();
        let p = plan(&cands, Options { prune: true, ..Options::default() }, None);
        let waves: Vec<(&str, i64)> = p.tasks.iter().map(|t| (t.key.name.as_ref(), t.wave)).collect();
        assert_eq!(waves, [("late", -1), ("early", 5), ("last", 6)]);
        assert_eq!(p.held, vec![(key("ConfigMap", "w", "keep"), PruneMode::Never)]);
    }

    #[test]
    fn namespaces_and_crds_move_before_what_needs_them() {
        let k = [key("ConfigMap", "web", "cfg"), key("Namespace", "", "web"), key("Widget", "web", "w"), key("CustomResourceDefinition", "", "widgets.example.com")];
        let d = [
            obj("ConfigMap", "cfg", json!({SYNC_WAVE: "-5"})),
            obj("Namespace", "web", json!({})),
            json!({"kind": "Widget", "apiVersion": "example.com/v1", "metadata": {"name": "w", "annotations": {SYNC_WAVE: "-3"}}}),
            json!({"kind": "CustomResourceDefinition", "metadata": {"name": "widgets.example.com"}, "spec": {"group": "example.com", "names": {"kind": "Widget"}}}),
        ];
        let mut keys = k.to_vec();
        if let Some(w) = keys.get_mut(2) {
            w.group = "example.com".into();
        }
        let cands: Vec<Candidate> =
            keys.iter().zip(&d).map(|(k, d)| Candidate { key: k, desired: Some(d), live: None, out_of_sync: true }).collect();
        let p = plan(&cands, Options::default(), None);
        let order: Vec<(&str, i64)> = p.tasks.iter().map(|t| (t.key.kind.as_ref(), t.wave)).collect();
        assert_eq!(order, [("Namespace", -5), ("ConfigMap", -5), ("CustomResourceDefinition", -3), ("Widget", -3)]);
    }

    #[test]
    fn helm_hooks_map_to_phases_and_test_hooks_never_run() {
        let pre = obj("Job", "j", json!({"helm.sh/hook": "pre-install,pre-upgrade", "helm.sh/hook-weight": "-2", "helm.sh/hook-delete-policy": "hook-succeeded"}));
        let h = hook_of(&pre);
        assert_eq!(h.as_ref().map(|h| h.phases.clone()), Some(vec![Phase::PreSync]));
        assert_eq!(h.map(|h| h.delete), Some(HookDelete { before_creation: false, on_success: true, on_failure: false }));
        assert_eq!(wave_of(&pre), -2);
        assert_eq!(hook_of(&obj("Pod", "t", json!({"helm.sh/hook": "test"}))).map(|h| h.phases.len()), Some(0));
        assert!(is_skipped(&obj("Pod", "s", json!({HOOK: "Skip"}))));
        assert_eq!(hook_of(&obj("Job", "a", json!({HOOK: "PostSync"}))).map(|h| h.delete.before_creation), Some(true));
    }

    #[test]
    fn next_group_waits_for_every_task_to_settle() {
        let t = |w| Task { action: Action::Apply, key: key("ConfigMap", "w", "a"), phase: Phase::Sync, wave: w, rank: 0 };
        let tasks = [t(0), t(0), t(1)];
        assert_eq!(next_group(&tasks, &[TaskState::Settled, TaskState::Done, TaskState::Pending]), Some((Phase::Sync, 0)));
        assert_eq!(next_group(&tasks, &[TaskState::Settled, TaskState::Settled, TaskState::Pending]), Some((Phase::Sync, 1)));
        assert_eq!(next_group(&tasks, &[TaskState::Settled; 3]), None);
    }
}
