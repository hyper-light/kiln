//! What the engine knows of an app: where it deploys, how it syncs, what it ignores. The
//! engine is told this ([`AppConfig`]) by its owner, which builds it from `.kiln/deploy.yaml`,
//! from Argo CD manifests or from the API; sources and engines are the owner's business (it
//! renders), so they are absent here.
//!
//! Sync options keep Argo CD's spelling (`CreateNamespace=true`), parsed once into flags
//! ([`SyncOptions`]); the same strings are accepted on an app, on one sync, and in an object's
//! `argocd.argoproj.io/sync-options` annotation.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::obj::Str;

/// A resource's or an app's health, from most to least healthy (Argo CD's order).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub enum Health {
    /// Doing what it should.
    #[default]
    Healthy,
    /// Paused or waiting.
    Suspended,
    /// Not healthy yet, still able to become so.
    Progressing,
    /// In git, not in the cluster.
    Missing,
    /// Failed, or past its progress deadline.
    Degraded,
    /// No rule could tell.
    Unknown,
}

impl Health {
    /// Every status, in order.
    pub const ALL: [Health; 6] =
        [Health::Healthy, Health::Suspended, Health::Progressing, Health::Missing, Health::Degraded, Health::Unknown];

    /// Index into [`Health::ALL`].
    pub fn index(self) -> usize {
        self as usize
    }

    /// Argo CD's name for it.
    pub fn as_str(self) -> &'static str {
        match self {
            Health::Healthy => "Healthy",
            Health::Suspended => "Suspended",
            Health::Progressing => "Progressing",
            Health::Missing => "Missing",
            Health::Degraded => "Degraded",
            Health::Unknown => "Unknown",
        }
    }

    /// Parses Argo CD's name.
    pub fn parse(s: &str) -> Option<Health> {
        Health::ALL.into_iter().find(|h| h.as_str() == s)
    }
}

impl fmt::Display for Health {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An app's or a resource's sync state.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub enum Sync {
    /// Live matches git.
    Synced,
    /// Live differs from git (or is missing, or should be pruned).
    OutOfSync,
    /// Not known yet: never rendered, render failed, or the cluster is unreachable.
    #[default]
    Unknown,
}

impl Sync {
    /// Argo CD's name for it.
    pub fn as_str(self) -> &'static str {
        match self {
            Sync::Synced => "Synced",
            Sync::OutOfSync => "OutOfSync",
            Sync::Unknown => "Unknown",
        }
    }
}

/// Where deletes cascade (`PrunePropagationPolicy`, and an app's deletion).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize, Default)]
pub enum Propagation {
    /// Dependents first, then the object (Argo CD's default for prunes).
    #[default]
    Foreground,
    /// The object now; the garbage collector removes dependents.
    Background,
    /// The object only; dependents stay.
    Orphan,
}

impl Propagation {
    /// The Kubernetes `propagationPolicy` value.
    pub fn as_str(self) -> &'static str {
        match self {
            Propagation::Foreground => "Foreground",
            Propagation::Background => "Background",
            Propagation::Orphan => "Orphan",
        }
    }
}

/// What to do with objects git no longer holds.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize, Default)]
pub enum PruneMode {
    /// Delete them (when the sync prunes).
    #[default]
    Allow,
    /// Never delete them (`Prune=false`): they show as requiring pruning.
    Never,
    /// Delete them only once a person confirms (`Prune=confirm`).
    Confirm,
}

/// Sync options, parsed from Argo CD's strings. `None` means "not said here": options combine
/// from the object, the sync and the app, in that order of precedence.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct SyncOptions {
    /// `Prune=true|false|confirm` (`true` keeps the default).
    pub prune: Option<PruneMode>,
    /// `PruneLast=true`.
    pub prune_last: Option<bool>,
    /// `PrunePropagationPolicy=foreground|background|orphan`.
    pub propagation: Option<Propagation>,
    /// `CreateNamespace=true`.
    pub create_namespace: Option<bool>,
    /// `ServerSideApply=true|false`; kiln's default is true.
    pub server_side_apply: Option<bool>,
    /// `Replace=true`.
    pub replace: Option<bool>,
    /// `Force=true`.
    pub force: Option<bool>,
    /// `ApplyOutOfSyncOnly=true`.
    pub apply_out_of_sync_only: Option<bool>,
    /// `RespectIgnoreDifferences=true`.
    pub respect_ignore_differences: Option<bool>,
    /// `SkipDryRunOnMissingResource=true`.
    pub skip_dry_run_on_missing_resource: Option<bool>,
    /// `Validate=false` (stored as "validate").
    pub validate: Option<bool>,
    /// `FailOnSharedResource=true`.
    pub fail_on_shared_resource: Option<bool>,
    /// `Delete=false` (stored as "delete"): keep the object when the app is deleted.
    pub delete: Option<bool>,
    /// `Delete=confirm`: delete only once a person confirms.
    pub delete_confirm: Option<bool>,
}

/// An option string kiln didn't recognize, kept so the owner can report it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct UnknownOption(pub String);

impl SyncOptions {
    /// Parses a list of Argo CD option strings. Unknown options are returned, not fatal: Argo CD
    /// ignores them too.
    pub fn parse<'a>(items: impl IntoIterator<Item = &'a str>) -> (SyncOptions, Vec<UnknownOption>) {
        let mut o = SyncOptions::default();
        let mut unknown = Vec::new();
        for raw in items {
            for item in raw.split(',') {
                let item = item.trim();
                if item.is_empty() {
                    continue;
                }
                if !o.set(item) {
                    unknown.push(UnknownOption(item.to_owned()));
                }
            }
        }
        (o, unknown)
    }

    fn set(&mut self, item: &str) -> bool {
        let Some((k, v)) = item.split_once('=') else { return false };
        let (k, v) = (k.trim(), v.trim());
        let flag = || match v.to_ascii_lowercase().as_str() {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        };
        match k {
            "Prune" => {
                self.prune = match v.to_ascii_lowercase().as_str() {
                    "true" => Some(PruneMode::Allow),
                    "false" => Some(PruneMode::Never),
                    "confirm" => Some(PruneMode::Confirm),
                    _ => return false,
                }
            }
            "PruneLast" => self.prune_last = flag(),
            "PrunePropagationPolicy" => {
                self.propagation = match v.to_ascii_lowercase().as_str() {
                    "foreground" => Some(Propagation::Foreground),
                    "background" => Some(Propagation::Background),
                    "orphan" => Some(Propagation::Orphan),
                    _ => return false,
                }
            }
            "CreateNamespace" => self.create_namespace = flag(),
            "ServerSideApply" => self.server_side_apply = flag(),
            "Replace" => self.replace = flag(),
            "Force" => self.force = flag(),
            "ApplyOutOfSyncOnly" => self.apply_out_of_sync_only = flag(),
            "RespectIgnoreDifferences" => self.respect_ignore_differences = flag(),
            "SkipDryRunOnMissingResource" => self.skip_dry_run_on_missing_resource = flag(),
            "Validate" => self.validate = flag(),
            "FailOnSharedResource" => self.fail_on_shared_resource = flag(),
            "Delete" => match v.to_ascii_lowercase().as_str() {
                "confirm" => self.delete_confirm = Some(true),
                _ => self.delete = flag(),
            },
            _ => return false,
        }
        true
    }

    /// These options, with anything unset taken from `fallback`.
    pub fn or(self, fallback: SyncOptions) -> SyncOptions {
        SyncOptions {
            prune: self.prune.or(fallback.prune),
            prune_last: self.prune_last.or(fallback.prune_last),
            propagation: self.propagation.or(fallback.propagation),
            create_namespace: self.create_namespace.or(fallback.create_namespace),
            server_side_apply: self.server_side_apply.or(fallback.server_side_apply),
            replace: self.replace.or(fallback.replace),
            force: self.force.or(fallback.force),
            apply_out_of_sync_only: self.apply_out_of_sync_only.or(fallback.apply_out_of_sync_only),
            respect_ignore_differences: self.respect_ignore_differences.or(fallback.respect_ignore_differences),
            skip_dry_run_on_missing_resource: self
                .skip_dry_run_on_missing_resource
                .or(fallback.skip_dry_run_on_missing_resource),
            validate: self.validate.or(fallback.validate),
            fail_on_shared_resource: self.fail_on_shared_resource.or(fallback.fail_on_shared_resource),
            delete: self.delete.or(fallback.delete),
            delete_confirm: self.delete_confirm.or(fallback.delete_confirm),
        }
    }

    /// Whether to apply server-side (kiln's default).
    pub fn ssa(&self) -> bool {
        self.server_side_apply.unwrap_or(true)
    }
}

/// Retries of a failed sync (Argo CD's `retry`).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Retry {
    /// Attempts after the first; negative means unlimited.
    pub limit: i64,
    /// First wait, in milliseconds (Argo CD's default is 5 s).
    pub backoff_ms: u64,
    /// Each wait is the last times this (default 2).
    pub factor: u64,
    /// The longest wait, in milliseconds (default 3 min).
    pub max_ms: u64,
}

impl Default for Retry {
    fn default() -> Self {
        Self { limit: 5, backoff_ms: 5_000, factor: 2, max_ms: 180_000 }
    }
}

impl Retry {
    /// The wait before retry number `attempt` (1-based).
    pub fn delay_ms(&self, attempt: u32) -> u64 {
        let mut d = self.backoff_ms.max(1);
        for _ in 1..attempt {
            d = d.saturating_mul(self.factor.max(1));
            if d >= self.max_ms {
                return self.max_ms;
            }
        }
        d.min(self.max_ms.max(1))
    }

    /// Whether another attempt is allowed after `attempts` failures.
    pub fn allows(&self, attempts: u32) -> bool {
        self.limit < 0 || i64::from(attempts) <= self.limit
    }
}

/// How an app syncs.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct SyncPolicy {
    /// Sync when git moves (Argo CD's `automated`).
    pub auto: bool,
    /// Automatic syncs prune.
    pub prune: bool,
    /// Sync again when the cluster drifts.
    pub self_heal: bool,
    /// Allow an automatic sync that would leave the app with no resources.
    pub allow_empty: bool,
    /// The app's options.
    pub options: SyncOptions,
    /// Retries of failed syncs.
    pub retry: Option<Retry>,
}

/// How kiln recognizes the objects an app owns.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub enum Tracking {
    /// kiln's: the `kiln.dev/app` label (a hash) plus the `kiln.dev/tracking` annotation.
    #[default]
    Kiln,
    /// Argo CD's `app.kubernetes.io/instance` label.
    ArgoLabel,
    /// Argo CD's `argocd.argoproj.io/tracking-id` annotation.
    ArgoAnnotation,
    /// Both of Argo CD's.
    ArgoAnnotationAndLabel,
}

/// Differences to ignore (Argo CD's `ignoreDifferences`).
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct IgnoreRule {
    /// Group the rule applies to (`*` or empty for any).
    pub group: String,
    /// Kind the rule applies to (`*` or empty for any).
    pub kind: String,
    /// Name, when the rule is for one object.
    pub name: Option<String>,
    /// Namespace, when the rule is for one namespace.
    pub namespace: Option<String>,
    /// RFC 6901 pointers (`/spec/replicas`).
    pub json_pointers: Vec<String>,
    /// jq-style paths (`.spec.template.spec.containers[].image`).
    pub jq_paths: Vec<String>,
    /// Fields owned by these field managers are ignored.
    pub managers: Vec<String>,
}

impl IgnoreRule {
    /// Whether the rule applies to an object.
    pub fn matches(&self, group: &str, kind: &str, namespace: &str, name: &str) -> bool {
        let any = |pat: &str, v: &str| pat.is_empty() || pat == "*" || pat == v;
        any(&self.group, group)
            && any(&self.kind, kind)
            && self.name.as_deref().is_none_or(|n| n == name)
            && self.namespace.as_deref().is_none_or(|n| n == namespace)
    }
}

/// Everything the engine needs to know of one app.
#[derive(Clone, PartialEq, Debug)]
pub struct AppConfig {
    /// Unique id (`project/name`).
    pub id: Str,
    /// Project.
    pub project: Str,
    /// Destination cluster's name.
    pub cluster: Str,
    /// Destination namespace (for namespaced objects that don't say).
    pub namespace: Str,
    /// How it syncs.
    pub sync: SyncPolicy,
    /// Differences to ignore.
    pub ignore: Vec<IgnoreRule>,
    /// How its objects are recognized.
    pub tracking: Tracking,
    /// The field manager kiln applies as.
    pub manager: Str,
    /// The name the app's objects carry in their tracking metadata, when not its id: an Argo CD
    /// Application's name, for objects Argo CD deployed.
    pub tracking_name: Option<Str>,
    /// Compare and report, never write (shadow mode, for checking kiln against Argo CD before
    /// taking an app over).
    pub read_only: bool,
    /// Create the destination namespace (`CreateNamespace=true`), with these labels and
    /// annotations.
    pub create_namespace: Option<(Vec<(String, String)>, Vec<(String, String)>)>,
    /// They are its managed metadata (`managedNamespaceMetadata`), applied at every sync; without
    /// it the namespace is only made when missing, as Argo CD does.
    pub manage_namespace: bool,
}

impl AppConfig {
    /// An app with kiln's defaults: manual sync, kiln tracking, manager `kiln`.
    pub fn new(id: &str, cluster: &str, namespace: &str) -> Self {
        Self {
            id: id.into(),
            project: "default".into(),
            cluster: cluster.into(),
            namespace: namespace.into(),
            sync: SyncPolicy::default(),
            ignore: Vec::new(),
            tracking: Tracking::Kiln,
            manager: "kiln".into(),
            tracking_name: None,
            read_only: false,
            create_namespace: None,
            manage_namespace: false,
        }
    }

    /// The name tracking metadata uses for the app.
    pub fn tracked_as(&self) -> &str {
        self.tracking_name.as_deref().unwrap_or(&self.id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_parse_argo_spellings() {
        let (o, unknown) =
            SyncOptions::parse(["CreateNamespace=true", "PrunePropagationPolicy=background,PruneLast=true", "Prune=confirm", "Bogus=1"]);
        assert_eq!(o.create_namespace, Some(true));
        assert_eq!(o.propagation, Some(Propagation::Background));
        assert_eq!(o.prune_last, Some(true));
        assert_eq!(o.prune, Some(PruneMode::Confirm));
        assert_eq!(unknown, vec![UnknownOption("Bogus=1".into())]);
        assert!(o.ssa());
        let (o2, _) = SyncOptions::parse(["ServerSideApply=false"]);
        assert!(!o2.or(o).ssa());
    }

    #[test]
    fn retry_backs_off_to_its_cap() {
        let r = Retry::default();
        assert_eq!(r.delay_ms(1), 5_000);
        assert_eq!(r.delay_ms(2), 10_000);
        assert_eq!(r.delay_ms(10), 180_000);
        assert!(r.allows(5));
        assert!(!r.allows(6));
    }

    #[test]
    fn health_orders_like_argo() {
        assert!(Health::Degraded > Health::Progressing);
        assert!(Health::Unknown > Health::Degraded);
        assert!(Health::Missing > Health::Progressing);
        assert!(Health::Suspended > Health::Healthy);
    }
}
