//! The ledger (`docs/observe.md` §10.7): every deployment and failure kept, each service and
//! environment's deployments in the order they finished; what a deployment's coming marks failed
//! (a rollback, a revert, a hotfix, as Datadog finds them); the four keys over a range.

use std::collections::{BTreeSet, HashMap};

use serde::{Deserialize, Serialize};

pub const NS_PER_S: i64 = 1_000_000_000;
pub const DAY_NS: i64 = 86_400 * NS_PER_S;
pub const WEEK_NS: i64 = 7 * DAY_NS;
/// How much older a deployment's previous one may be for its lead time to be read (Datadog's 60
/// days).
pub const LEAD_WINDOW_NS: i64 = 60 * DAY_NS;
/// The commits a deployment's lead time reads at the most (Datadog's).
pub const MAX_COMMITS: usize = 5_000;
/// The deployments of a service and environment a rollback's target, or what a revert reverts, is
/// looked for among: the latest so many. A bound on what one deployment costs (at a busy service's
/// six a day, five months); a service deployed less often is looked through further back.
pub const LOOKBACK: usize = 1_000;

/// A deployment's commit and the repository it is in.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Git {
    pub commit_sha: String,
    pub repository_url: String,
}

/// What a deployment's commits say of its lead time: how many, and their mean, least and most
/// (seconds from each's author's time to the deployment's finish).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Lead {
    pub commits: u32,
    pub mean_s: f64,
    pub min_s: f64,
    pub max_s: f64,
}

/// How a deployment was found failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Found {
    /// A later deployment went back past it.
    Rollback,
    /// A later deployment reverted a commit it brought.
    Revert,
    /// The deployment after it was a hotfix.
    Hotfix,
    /// Said so through the API.
    Manual,
}

/// How a failed deployment was remedied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemedyKind {
    Rollback,
    Rollforward,
}

/// The deployment that remedied a failed one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Remedy {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: RemedyKind,
    /// Its finish (ns): the failed deployment's recovery ends there.
    pub finished_at: i64,
}

/// A deployment marked failed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeFailure {
    pub found: Found,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remedy: Option<Remedy>,
}

/// A deployment as the ledger keeps it. Times are nanoseconds since the epoch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Deployment {
    pub id: String,
    pub service: String,
    #[serde(default)]
    pub env: String,
    #[serde(default)]
    pub team: String,
    #[serde(default)]
    pub version: String,
    pub started_at: i64,
    pub finished_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git: Option<Git>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub custom_tags: Vec<String>,
    /// Where it came from: `api` or `kiln.cd`.
    #[serde(default)]
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lead: Option<Lead>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change_failure: Option<ChangeFailure>,
    /// The failed deployments it remedied.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remedies: Vec<String>,
    /// Its commits' shas (their first 16 hex digits) and titles (hashed): what a later revert finds
    /// it by.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shas: Vec<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub titles: Vec<u64>,
}

/// A version's hash kept apart from a commit's.
const VERSION: u64 = 0x7665_7273_696f_6e00;

impl Deployment {
    /// What it deploys, as a rollback is found by: its commit (in any case), else its version;
    /// hashed, so that comparing two allocates nothing.
    fn ident(&self) -> Option<u64> {
        match &self.git {
            Some(g) if !g.commit_sha.trim().is_empty() => Some(fnv_lower(g.commit_sha.trim())),
            _ => Some(self.version.trim()).filter(|v| !v.is_empty()).map(|v| fnv(v) ^ VERSION),
        }
    }

    /// Its failure's recovery (ns), when it failed and was remedied.
    pub fn recovery_ns(&self) -> Option<i64> {
        let r = self.change_failure.as_ref()?.remedy.as_ref()?;
        Some(r.finished_at.saturating_sub(self.finished_at).max(0))
    }

    /// A tag of its own (`key:value` among its custom tags).
    fn custom(&self, key: &str) -> Option<&str> {
        self.custom_tags.iter().find_map(|t| t.split_once(':').filter(|(k, _)| *k == key).map(|(_, v)| v))
    }
}

/// A failure (an incident), as the ledger keeps it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Failure {
    pub id: String,
    #[serde(default)]
    pub services: Vec<String>,
    #[serde(default)]
    pub env: String,
    #[serde(default)]
    pub team: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub severity: String,
    pub started_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git: Option<Git>,
    #[serde(default)]
    pub version: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub custom_tags: Vec<String>,
}

/// A commit git's log gives between two deployments.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Commit {
    pub sha: String,
    /// Its author's time (s): what a rebase keeps.
    pub author_s: i64,
    /// Its message's first line.
    pub title: String,
    /// A merge (more than one parent): not counted in lead time, read for a hotfix's branch.
    pub merge: bool,
    /// The commit it reverts, as its message says (`This reverts commit <sha>.`).
    pub reverts: Option<String>,
}

/// A deployment the coming of another marks failed, how, and how it was remedied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mark {
    pub failed: String,
    pub found: Found,
    pub kind: RemedyKind,
}

/// FNV-1a over `s`: a hash that stays the same from build to build (it is kept).
fn fnv(s: &str) -> u64 {
    s.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3))
}

/// FNV-1a over `s`'s ASCII lowercase.
fn fnv_lower(s: &str) -> u64 {
    s.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| (h ^ u64::from(b.to_ascii_lowercase())).wrapping_mul(0x0100_0000_01b3))
}

/// A sha's key: the whole of it hashed (in any case); none of what is no sha.
pub fn sha_key(sha: &str) -> Option<u64> {
    let s = sha.trim();
    ((7..=64).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_hexdigit())).then(|| fnv_lower(s))
}

/// A title's key.
pub fn title_key(title: &str) -> u64 {
    fnv(title.trim())
}

/// The title a revert's names: `Revert "<title>"`.
pub fn reverted_title(title: &str) -> Option<&str> {
    title.trim().strip_prefix("Revert \"")?.strip_suffix('"')
}

/// Whether a commit's title says it is a hotfix (a squashed pull request's title, a merge's branch).
pub fn says_hotfix(title: &str) -> bool {
    title.to_ascii_lowercase().contains("hotfix")
}

/// What `commits` say of a deployment finished at `finished` (ns): its lead (merges left out), each
/// commit's lead time (s), its shas' keys and its titles' keys.
pub fn read_commits(finished: i64, commits: &[Commit]) -> (Option<Lead>, Vec<f64>, Vec<u64>, Vec<u64>) {
    let mut each = Vec::new();
    let (mut shas, mut titles) = (Vec::with_capacity(commits.len()), Vec::with_capacity(commits.len()));
    for c in commits.iter().take(MAX_COMMITS) {
        if let Some(k) = sha_key(&c.sha) {
            shas.push(k);
        }
        titles.push(title_key(&c.title));
        if c.merge {
            continue;
        }
        let s = (finished / NS_PER_S).saturating_sub(c.author_s).max(0) as f64;
        each.push(s);
    }
    let lead = (!each.is_empty()).then(|| {
        let mean = each.iter().sum::<f64>() / each.len() as f64;
        let min = each.iter().copied().fold(f64::INFINITY, f64::min);
        let max = each.iter().copied().fold(0.0, f64::max);
        Lead { commits: u32::try_from(each.len()).unwrap_or(u32::MAX), mean_s: mean, min_s: min, max_s: max }
    });
    (lead, each, shas, titles)
}

/// The middle of `v` (the mean of the two middle values of an even count); none of nothing.
pub fn median(mut v: Vec<f64>) -> Option<f64> {
    v.retain(|x| x.is_finite());
    if v.is_empty() {
        return None;
    }
    v.sort_by(f64::total_cmp);
    let n = v.len();
    let hi = v.get(n / 2).copied()?;
    Some(if n % 2 == 1 { hi } else { (v.get(n / 2 - 1).copied()? + hi) / 2.0 })
}

type Key = (String, String);

fn key_of(service: &str, env: &str) -> Key {
    (service.to_owned(), env.to_owned())
}

/// What the four keys are taken over: a range of finish times (ns, `[from, to)`), and a service,
/// an environment and a team when given.
#[derive(Debug, Clone, Default)]
pub struct Query {
    pub from: i64,
    pub to: i64,
    pub service: Option<String>,
    pub env: Option<String>,
    pub team: Option<String>,
}

impl Query {
    fn picks(&self, service: &str, env: &str, team: &str) -> bool {
        self.service.as_deref().is_none_or(|s| s == service) && self.env.as_deref().is_none_or(|e| e == env) && self.team.as_deref().is_none_or(|t| t == team)
    }

    fn picks_failure(&self, f: &Failure) -> bool {
        self.service.as_deref().is_none_or(|s| f.services.iter().any(|x| x == s)) && self.env.as_deref().is_none_or(|e| e == f.env) && self.team.as_deref().is_none_or(|t| t == f.team)
    }
}

/// The four keys over a range, and week by week.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Summary {
    pub from: i64,
    pub to: i64,
    pub deployments: u64,
    /// Deployments a day.
    pub per_day: f64,
    /// The median of the deployments' lead times (each its commits' mean), seconds.
    pub lead_time_s: Option<f64>,
    /// The deployments that had one.
    pub lead_deployments: u64,
    pub change_failures: u64,
    pub failure_rate: Option<f64>,
    /// The median of the failed deployments' recoveries, seconds.
    pub recovery_s: Option<f64>,
    pub recovered: u64,
    /// Failures begun in the range, those still going, and the median of the others' durations.
    pub failures: u64,
    pub open_failures: u64,
    pub restore_s: Option<f64>,
    pub weeks: Vec<Week>,
}

/// A week's (from Monday, UTC) figures.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Week {
    /// Its first instant (ns), or the range's when it began later.
    pub start: i64,
    pub deployments: u64,
    pub change_failures: u64,
    pub lead_time_s: Option<f64>,
    pub recovery_s: Option<f64>,
}

/// The Monday 00:00 UTC on or before `t` (ns). The epoch was a Thursday.
pub fn week_of(t: i64) -> i64 {
    let monday = 4 * DAY_NS;
    t.saturating_sub(monday).div_euclid(WEEK_NS).saturating_mul(WEEK_NS).saturating_add(monday)
}

/// Every deployment and failure kept.
#[derive(Debug, Default)]
pub struct Ledger {
    deployments: HashMap<String, Deployment>,
    /// Each service and environment's deployments by finish (then id).
    order: HashMap<Key, BTreeSet<(i64, String)>>,
    /// Each service and environment's deployments by what they deploy, how many: a rollback's target
    /// is looked for only when what a deployment deploys was deployed before.
    deployed: HashMap<Key, HashMap<u64, u32>>,
    /// The teams the deployments name, how many each.
    teams: HashMap<String, u32>,
    failures: HashMap<String, Failure>,
}

impl Ledger {
    pub fn deployment(&self, id: &str) -> Option<&Deployment> {
        self.deployments.get(id)
    }

    pub fn failure(&self, id: &str) -> Option<&Failure> {
        self.failures.get(id)
    }

    /// How many deployments and failures it keeps.
    pub fn len(&self) -> (usize, usize) {
        (self.deployments.len(), self.failures.len())
    }

    pub fn deployments(&self) -> impl Iterator<Item = &Deployment> {
        self.deployments.values()
    }

    pub fn failures(&self) -> impl Iterator<Item = &Failure> {
        self.failures.values()
    }

    /// The deployment of `service` in `env` just before `(at, id)`.
    pub fn before(&self, service: &str, env: &str, at: i64, id: &str) -> Option<&Deployment> {
        let set = self.order.get(&key_of(service, env))?;
        let (_, prev) = set.range(..(at, id.to_owned())).next_back()?;
        self.deployments.get(prev)
    }

    /// The deployment of `service` in `env` just after `(at, id)`.
    pub fn after(&self, service: &str, env: &str, at: i64, id: &str) -> Option<&Deployment> {
        let set = self.order.get(&key_of(service, env))?;
        let start = (at, id.to_owned());
        let (_, next) = set.range((std::ops::Bound::Excluded(start), std::ops::Bound::Unbounded)).next()?;
        self.deployments.get(next)
    }

    /// A kept deployment, to change what it says of its commits (not its service, environment or
    /// finish: [`Ledger::put`] those).
    pub fn deployment_mut(&mut self, id: &str) -> Option<&mut Deployment> {
        self.deployments.get_mut(id)
    }

    /// `d` kept, replacing one of its id (which comes back): the marks that one had and `d` does
    /// not say stay (it was found failed, or remedied one).
    pub fn put(&mut self, mut d: Deployment) -> Option<Deployment> {
        let old = self.unlink(&d.id);
        if let Some(o) = &old {
            if d.change_failure.is_none() {
                d.change_failure.clone_from(&o.change_failure);
            }
            if d.remedies.is_empty() {
                d.remedies.clone_from(&o.remedies);
            }
        }
        let key = key_of(&d.service, &d.env);
        if let Some(i) = d.ident() {
            *self.deployed.entry(key.clone()).or_default().entry(i).or_default() += 1;
        }
        if !d.team.is_empty() {
            *self.teams.entry(d.team.clone()).or_default() += 1;
        }
        self.order.entry(key).or_default().insert((d.finished_at, d.id.clone()));
        self.deployments.insert(d.id.clone(), d);
        old
    }

    /// The deployment `id` out of the maps, its marks and those naming it untouched.
    fn unlink(&mut self, id: &str) -> Option<Deployment> {
        let d = self.deployments.remove(id)?;
        let key = key_of(&d.service, &d.env);
        if let Some(set) = self.order.get_mut(&key) {
            set.remove(&(d.finished_at, d.id.clone()));
            if set.is_empty() {
                self.order.remove(&key);
            }
        }
        if let Some(i) = d.ident()
            && let Some(m) = self.deployed.get_mut(&key)
        {
            if let Some(n) = m.get_mut(&i) {
                *n = n.saturating_sub(1);
                if *n == 0 {
                    m.remove(&i);
                }
            }
            if m.is_empty() {
                self.deployed.remove(&key);
            }
        }
        if let Some(n) = self.teams.get_mut(&d.team) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                self.teams.remove(&d.team);
            }
        }
        Some(d)
    }

    /// The deployment `id` gone, and every mark naming it: what it remedied is failed still (the
    /// failure was found), without its remedy; what remedied it no longer names it.
    pub fn remove(&mut self, id: &str) -> Option<Deployment> {
        let d = self.unlink(id)?;
        for failed in &d.remedies {
            if let Some(f) = self.deployments.get_mut(failed)
                && let Some(cf) = f.change_failure.as_mut()
                && cf.remedy.as_ref().is_some_and(|r| r.id == d.id)
            {
                cf.remedy = None;
            }
        }
        if let Some(r) = d.change_failure.as_ref().and_then(|cf| cf.remedy.as_ref())
            && let Some(by) = self.deployments.get_mut(&r.id)
        {
            by.remedies.retain(|x| *x != d.id);
        }
        Some(d)
    }

    pub fn put_failure(&mut self, f: Failure) -> Option<Failure> {
        self.failures.insert(f.id.clone(), f)
    }

    pub fn remove_failure(&mut self, id: &str) -> Option<Failure> {
        self.failures.remove(id)
    }

    /// Every deployment finished and failure begun before `before` (ns) gone: how many.
    pub fn expire(&mut self, before: i64) -> usize {
        let old: Vec<String> = self.deployments.values().filter(|d| d.finished_at < before).map(|d| d.id.clone()).collect();
        let gone_failures: Vec<String> = self.failures.values().filter(|f| f.started_at < before).map(|f| f.id.clone()).collect();
        for id in &old {
            self.remove(id);
        }
        for id in &gone_failures {
            self.failures.remove(id);
        }
        old.len() + gone_failures.len()
    }

    /// What the deployment `id` (kept) marks failed, its commits those `commits` (newest first, as
    /// git's log gives them): a rollback's, a revert's and a hotfix's (Datadog's rules), each failed
    /// deployment once (the first rule that finds it).
    pub fn detect(&self, id: &str, commits: &[Commit]) -> Vec<Mark> {
        let Some(d) = self.deployments.get(id) else { return Vec::new() };
        let mut out: Vec<Mark> = Vec::new();
        let mut push = |failed: &Deployment, found: Found, kind: RemedyKind| {
            if failed.id != d.id && !out.iter().any(|m| m.failed == failed.id) {
                out.push(Mark { failed: failed.id.clone(), found, kind });
            }
        };
        let prev = self.before(&d.service, &d.env, d.finished_at, &d.id);
        // A rollback: what it deploys was deployed before, but not just before; the deployment
        // after that earlier one failed (V1 → V2 → V3 → V1: V2). Looked for only when another
        // deployment of it is kept.
        if let Some(what) = d.ident()
            && prev.is_some_and(|p| p.ident() != Some(what))
            && self.deployed.get(&key_of(&d.service, &d.env)).and_then(|m| m.get(&what)).is_some_and(|n| *n > 1)
            && let Some(target) = self.earlier(d, |e| e.ident() == Some(what))
            && let Some(failed) = self.after(&target.service, &target.env, target.finished_at, &target.id)
        {
            push(failed, Found::Rollback, RemedyKind::Rollback);
        }
        // A revert: the earlier deployment that brought what it reverts (by its sha, else its
        // title); none when that came in this one.
        let here_shas: Vec<u64> = commits.iter().filter_map(|c| sha_key(&c.sha)).collect();
        let here_titles: Vec<u64> = commits.iter().map(|c| title_key(&c.title)).collect();
        for c in commits.iter().filter(|c| !c.merge) {
            let Some(title) = reverted_title(&c.title) else { continue };
            let sha = c.reverts.as_deref().and_then(sha_key);
            let (title_k, here) = (title_key(title), sha.is_some_and(|s| here_shas.contains(&s)));
            if here || (sha.is_none() && here_titles.contains(&title_k)) {
                continue;
            }
            let brought = sha.and_then(|s| self.earlier(d, |e| e.shas.contains(&s))).or_else(|| self.earlier(d, |e| e.titles.contains(&title_k)));
            if let Some(e) = brought {
                push(e, Found::Revert, RemedyKind::Rollforward);
            }
        }
        // A hotfix: the deployment just before it.
        if (commits.iter().any(|c| says_hotfix(&c.title)) || d.version.trim().ends_with("_hotfix"))
            && let Some(p) = prev
        {
            push(p, Found::Hotfix, RemedyKind::Rollforward);
        }
        out
    }

    /// The latest deployment before `d` (its service and environment, among the [`LOOKBACK`] before
    /// it) that `f` picks.
    fn earlier(&self, d: &Deployment, f: impl Fn(&Deployment) -> bool) -> Option<&Deployment> {
        let set = self.order.get(&key_of(&d.service, &d.env))?;
        set.range(..(d.finished_at, d.id.clone())).rev().take(LOOKBACK).filter_map(|(_, id)| self.deployments.get(id)).find(|e| f(e))
    }

    /// `failed` marked failed, found so, remedied by `remedy` (a kept deployment's id) when one did:
    /// whether that changed it. A mark found stands against one found later; one said by hand gives
    /// it its remedy (none given, the one found stays).
    pub fn mark(&mut self, failed: &str, found: Found, remedy: Option<(&str, RemedyKind)>) -> bool {
        let remedy = remedy.and_then(|(id, kind)| self.deployments.get(id).map(|r| Remedy { id: r.id.clone(), kind, finished_at: r.finished_at }));
        let Some(f) = self.deployments.get_mut(failed) else { return false };
        let next = match &f.change_failure {
            None => ChangeFailure { found, remedy },
            Some(cf) if found == Found::Manual => {
                let remedy = remedy.or_else(|| cf.remedy.clone());
                if remedy == cf.remedy {
                    return false;
                }
                ChangeFailure { found: cf.found, remedy }
            }
            Some(_) => return false,
        };
        let remedy = next.remedy.clone();
        let old = f.change_failure.replace(next);
        let failed_id = f.id.clone();
        // The remedies it names, kept both ways.
        if let Some(r) = old.and_then(|cf| cf.remedy)
            && let Some(was) = self.deployments.get_mut(&r.id)
        {
            was.remedies.retain(|x| *x != failed_id);
        }
        if let Some(r) = remedy
            && let Some(by) = self.deployments.get_mut(&r.id)
            && !by.remedies.contains(&failed_id)
        {
            by.remedies.push(failed_id);
        }
        true
    }

    /// `failed` no longer marked: whether it was.
    pub fn unmark(&mut self, failed: &str) -> bool {
        let Some(f) = self.deployments.get_mut(failed) else { return false };
        let Some(old) = f.change_failure.take() else { return false };
        let failed_id = f.id.clone();
        if let Some(r) = old.remedy
            && let Some(by) = self.deployments.get_mut(&r.id)
        {
            by.remedies.retain(|x| *x != failed_id);
        }
        true
    }

    /// The services, environments and teams it holds, each sorted: from what it keeps of them, not
    /// a look at every deployment.
    pub fn facets(&self) -> (Vec<String>, Vec<String>, Vec<String>) {
        let mut services: BTreeSet<&str> = self.order.keys().map(|(s, _)| s.as_str()).collect();
        let mut envs: BTreeSet<&str> = self.order.keys().map(|(_, e)| e.as_str()).collect();
        let mut teams: BTreeSet<&str> = self.teams.keys().map(String::as_str).collect();
        for f in self.failures.values() {
            services.extend(f.services.iter().map(String::as_str));
            envs.insert(&f.env);
            teams.insert(&f.team);
        }
        let own = |s: BTreeSet<&str>| s.into_iter().filter(|x| !x.is_empty()).map(str::to_owned).collect();
        (own(services), own(envs), own(teams))
    }

    /// The deployments finished in `from..to` (ns), each service and environment's in their order:
    /// what a list searches, never every deployment.
    pub fn finished_between(&self, from: i64, to: i64) -> impl Iterator<Item = &Deployment> + '_ {
        self.order.values().filter(move |_| from < to).flat_map(move |set| set.range((from, String::new())..(to, String::new()))).filter_map(|(_, id)| self.deployments.get(id))
    }

    /// The deployments `q` picks, in finish order (a range backwards picks none).
    fn picked<'a>(&'a self, q: &'a Query) -> impl Iterator<Item = &'a Deployment> + 'a {
        let mut keys: Vec<&Key> = self.order.keys().filter(|(s, e)| q.from < q.to && q.service.as_deref().is_none_or(|x| x == s) && q.env.as_deref().is_none_or(|x| x == e)).collect();
        keys.sort();
        keys.into_iter()
            .filter_map(|k| self.order.get(k))
            .flat_map(move |set| set.range((q.from, String::new())..(q.to, String::new())))
            .filter_map(|(_, id)| self.deployments.get(id))
            .filter(|d| q.picks(&d.service, &d.env, &d.team))
    }

    /// The four keys over `q`'s range, and week by week.
    pub fn summary(&self, q: &Query) -> Summary {
        let mut weeks: Vec<(i64, Vec<&Deployment>)> = Vec::new();
        let mut first = q.from;
        while first < q.to {
            let next = week_of(first).saturating_add(WEEK_NS);
            weeks.push((first, Vec::new()));
            first = next;
        }
        let all: Vec<&Deployment> = self.picked(q).collect();
        for d in &all {
            let at = weeks.partition_point(|(start, _)| *start <= d.finished_at);
            if let Some((_, w)) = at.checked_sub(1).and_then(|i| weeks.get_mut(i)) {
                w.push(d);
            }
        }
        let figures = |ds: &[&Deployment]| {
            let lead = median(ds.iter().filter_map(|d| d.lead.map(|l| l.mean_s)).collect());
            let recovery = median(ds.iter().filter_map(|d| d.recovery_ns()).map(|ns| ns as f64 / 1e9).collect());
            let failed = ds.iter().filter(|d| d.change_failure.is_some()).count() as u64;
            (lead, recovery, failed)
        };
        let (lead_time_s, recovery_s, change_failures) = figures(&all);
        let deployments = all.len() as u64;
        let days = (q.to.saturating_sub(q.from)).max(1) as f64 / DAY_NS as f64;
        let failures: Vec<&Failure> = self.failures.values().filter(|f| f.started_at >= q.from && f.started_at < q.to && q.picks_failure(f)).collect();
        let restore_s = median(failures.iter().filter_map(|f| f.finished_at.map(|e| e.saturating_sub(f.started_at).max(0) as f64 / 1e9)).collect());
        Summary {
            from: q.from,
            to: q.to,
            deployments,
            per_day: deployments as f64 / days,
            lead_time_s,
            lead_deployments: all.iter().filter(|d| d.lead.is_some()).count() as u64,
            change_failures,
            failure_rate: (deployments > 0).then(|| change_failures as f64 / deployments as f64),
            recovery_s,
            recovered: all.iter().filter(|d| d.recovery_ns().is_some()).count() as u64,
            failures: failures.len() as u64,
            open_failures: failures.iter().filter(|f| f.finished_at.is_none()).count() as u64,
            restore_s,
            weeks: weeks
                .iter()
                .map(|(start, ds)| {
                    let (lead_time_s, recovery_s, change_failures) = figures(ds);
                    Week { start: *start, deployments: ds.len() as u64, change_failures, lead_time_s, recovery_s }
                })
                .collect(),
        }
    }
}

/// A deployment as a log search reads it: its service, its `env`, `team`, `version`, `git.commit_sha`
/// and `git.repository_url`, its tags, `change_failure` (`true` when marked).
impl kiln_obs::logs::search::Fields for Deployment {
    fn message(&self) -> &str {
        ""
    }
    fn service(&self) -> &str {
        &self.service
    }
    fn host(&self) -> &str {
        ""
    }
    fn source(&self) -> &str {
        "dora"
    }
    fn severity(&self) -> u8 {
        if self.change_failure.is_some() { 17 } else { 9 }
    }
    fn attr(&self, key: &str) -> Option<&str> {
        match key {
            "env" => Some(self.env.as_str()),
            "team" => Some(self.team.as_str()),
            "version" => Some(self.version.as_str()),
            "id" => Some(self.id.as_str()),
            "git.commit_sha" => self.git.as_ref().map(|g| g.commit_sha.as_str()),
            "git.repository_url" => self.git.as_ref().map(|g| g.repository_url.as_str()),
            "change_failure" => Some(if self.change_failure.is_some() { "true" } else { "false" }),
            other => self.custom(other),
        }
        .filter(|v| !v.is_empty())
    }
    fn trace_id(&self) -> Option<[u8; 16]> {
        None
    }
    fn span_id(&self) -> Option<[u8; 8]> {
        None
    }
}

/// A failure as a log search reads it: its first service (`service:`, and every one as
/// `services`), `env`, `team`, `severity`, `name`, `version`, its tags.
impl kiln_obs::logs::search::Fields for Failure {
    fn message(&self) -> &str {
        &self.name
    }
    fn service(&self) -> &str {
        self.services.first().map_or("", String::as_str)
    }
    fn host(&self) -> &str {
        ""
    }
    fn source(&self) -> &str {
        "dora"
    }
    fn severity(&self) -> u8 {
        17
    }
    fn attr(&self, key: &str) -> Option<&str> {
        match key {
            "env" => Some(self.env.as_str()),
            "team" => Some(self.team.as_str()),
            "severity" => Some(self.severity.as_str()),
            "name" => Some(self.name.as_str()),
            "version" => Some(self.version.as_str()),
            "id" => Some(self.id.as_str()),
            other => self.custom_tags.iter().find_map(|t| t.split_once(':').filter(|(k, _)| *k == other).map(|(_, v)| v)),
        }
        .filter(|v| !v.is_empty())
    }
    fn trace_id(&self) -> Option<[u8; 16]> {
        None
    }
    fn span_id(&self) -> Option<[u8; 8]> {
        None
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    const T0: i64 = 1_790_000_000 * NS_PER_S;
    const H: i64 = 3_600 * NS_PER_S;

    fn sha(c: char) -> String {
        c.to_string().repeat(40)
    }

    /// A deployment of `service` at `h` hours past T0, of commit `c` (a hex digit) or none.
    fn dep(id: &str, service: &str, h: i64, c: Option<char>) -> Deployment {
        Deployment {
            id: id.into(),
            service: service.into(),
            env: "prod".into(),
            team: "core".into(),
            version: String::new(),
            started_at: T0 + h * H - 60 * NS_PER_S,
            finished_at: T0 + h * H,
            git: c.map(|c| Git { commit_sha: sha(c), repository_url: "https://github.com/acme/shop".into() }),
            custom_tags: Vec::new(),
            source: "api".into(),
            lead: None,
            change_failure: None,
            remedies: Vec::new(),
            shas: Vec::new(),
            titles: Vec::new(),
        }
    }

    fn commit(c: char, author_h: i64, title: &str) -> Commit {
        Commit { sha: sha(c), author_s: (T0 + author_h * H) / NS_PER_S, title: title.into(), merge: false, reverts: None }
    }

    /// Puts each, runs what each marks and marks it, as the server does: what was marked, in order.
    fn run(l: &mut Ledger, deps: Vec<(Deployment, Vec<Commit>)>) -> Vec<(String, Found, String)> {
        let mut out = Vec::new();
        for (mut d, commits) in deps {
            let (lead, _, shas, titles) = read_commits(d.finished_at, &commits);
            (d.lead, d.shas, d.titles) = (lead, shas, titles);
            let id = d.id.clone();
            l.put(d);
            for m in l.detect(&id, &commits) {
                if l.mark(&m.failed, m.found, Some((&id, m.kind))) {
                    out.push((m.failed.clone(), m.found, id.clone()));
                }
            }
        }
        out
    }

    /// V1 → V2 → V3 → V1: V2 failed, rolled back by the last; a redeploy of what runs is no
    /// rollback; with no git, versions are what is compared.
    #[test]
    fn a_rollback_marks_the_deployment_after_the_one_gone_back_to() {
        let mut l = Ledger::default();
        let marked = run(
            &mut l,
            vec![(dep("v1", "shop", 0, Some('1')), vec![]), (dep("v2", "shop", 1, Some('2')), vec![]), (dep("v3", "shop", 2, Some('3')), vec![]), (dep("back", "shop", 3, Some('1')), vec![])],
        );
        assert_eq!(marked, vec![("v2".into(), Found::Rollback, "back".into())]);
        let v2 = l.deployment("v2").unwrap();
        assert_eq!(v2.change_failure.as_ref().unwrap().remedy.as_ref().map(|r| (r.id.as_str(), r.kind)), Some(("back", RemedyKind::Rollback)));
        assert_eq!(v2.recovery_ns(), Some(2 * H));
        assert_eq!(l.deployment("back").unwrap().remedies, vec!["v2".to_owned()]);
        // The same again: no rollback.
        assert!(run(&mut l, vec![(dep("again", "shop", 4, Some('1')), vec![])]).is_empty());
        // Another service's history is its own.
        assert!(run(&mut l, vec![(dep("o1", "cart", 5, Some('1')), vec![]), (dep("o2", "cart", 6, Some('2')), vec![])]).is_empty());
        // Versions, with no git.
        let mut l = Ledger::default();
        let v = |id: &str, h: i64, version: &str| Deployment { version: version.into(), ..dep(id, "api", h, None) };
        let marked = run(&mut l, vec![(v("a", 0, "1.0"), vec![]), (v("b", 1, "1.1"), vec![]), (v("c", 2, "1.0"), vec![])]);
        assert_eq!(marked, vec![("b".into(), Found::Rollback, "c".into())]);
    }

    /// A revert finds the deployment that brought what it reverts, by its sha, else its title; none
    /// when both came in one deployment.
    #[test]
    fn a_revert_marks_the_deployment_that_brought_what_it_reverts() {
        let mut l = Ledger::default();
        let mut revert = commit('d', 5, "Revert \"Add search\"");
        revert.reverts = Some(sha('b'));
        let marked = run(
            &mut l,
            vec![
                (dep("d1", "shop", 0, Some('1')), vec![]),
                (dep("d2", "shop", 2, Some('2')), vec![commit('b', 1, "Add search"), commit('c', 1, "Tidy")]),
                (dep("d3", "shop", 4, Some('3')), vec![commit('e', 3, "Faster cart")]),
                (dep("d4", "shop", 6, Some('4')), vec![revert]),
            ],
        );
        assert_eq!(marked, vec![("d2".into(), Found::Revert, "d4".into())]);
        assert_eq!(l.deployment("d2").unwrap().change_failure.as_ref().unwrap().remedy.as_ref().unwrap().kind, RemedyKind::Rollforward);
        // By title, a revert that names no sha.
        let mut l = Ledger::default();
        let marked = run(
            &mut l,
            vec![
                (dep("a", "shop", 0, Some('1')), vec![]),
                (dep("b", "shop", 1, Some('2')), vec![commit('b', 0, "Add search")]),
                (dep("c", "shop", 2, Some('3')), vec![commit('c', 1, "Revert \"Add search\"")]),
            ],
        );
        assert_eq!(marked, vec![("b".into(), Found::Revert, "c".into())]);
        // Both in one deployment: nothing failed.
        let mut l = Ledger::default();
        let mut r = commit('c', 1, "Revert \"Add search\"");
        r.reverts = Some(sha('b'));
        assert!(run(&mut l, vec![(dep("a", "shop", 0, Some('1')), vec![]), (dep("b", "shop", 2, Some('2')), vec![r, commit('b', 1, "Add search")])]).is_empty());
    }

    /// A hotfix (a commit's title, a merge's branch, a version ending `_hotfix`) marks the deployment
    /// just before it; a deployment found failed twice is marked once, by the first rule.
    #[test]
    fn a_hotfix_marks_the_deployment_before_it() {
        let mut l = Ledger::default();
        let mut merge = commit('m', 2, "Merge pull request #12 from acme/hotfix-login");
        merge.merge = true;
        let marked =
            run(&mut l, vec![(dep("a", "shop", 0, Some('1')), vec![]), (dep("b", "shop", 1, Some('2')), vec![]), (dep("c", "shop", 3, Some('3')), vec![merge, commit('f', 2, "Fix the login")])]);
        assert_eq!(marked, vec![("b".into(), Found::Hotfix, "c".into())]);
        let v = |id: &str, h: i64, version: &str| Deployment { version: version.into(), ..dep(id, "api", h, None) };
        let mut l = Ledger::default();
        assert_eq!(run(&mut l, vec![(v("x", 0, "2.0"), vec![]), (v("y", 1, "2.0_hotfix"), vec![])]), vec![("x".into(), Found::Hotfix, "y".into())]);
        // Rolled back to, and a hotfix too: marked by the rollback alone.
        let mut l = Ledger::default();
        let marked = run(&mut l, vec![(dep("p", "shop", 0, Some('1')), vec![]), (dep("q", "shop", 1, Some('2')), vec![]), (dep("r", "shop", 2, Some('1')), vec![commit('9', 1, "hotfix: undo q")])]);
        assert_eq!(marked, vec![("q".into(), Found::Rollback, "r".into())]);
    }

    /// Marks by hand: they give a remedy, keep a found one when none is given, and come off;
    /// removing a deployment takes its marks with it.
    #[test]
    fn marks_by_hand_and_removals_keep_both_sides_agreeing() {
        let mut l = Ledger::default();
        run(&mut l, vec![(dep("a", "shop", 0, Some('1')), vec![]), (dep("b", "shop", 1, Some('2')), vec![]), (dep("c", "shop", 3, Some('3')), vec![])]);
        assert!(l.mark("b", Found::Manual, None));
        assert!(!l.mark("b", Found::Manual, None), "the same again changes nothing");
        assert!(l.mark("b", Found::Manual, Some(("c", RemedyKind::Rollforward))));
        assert_eq!(l.deployment("b").unwrap().recovery_ns(), Some(2 * H));
        assert_eq!(l.deployment("c").unwrap().remedies, vec!["b".to_owned()]);
        // A rule finding it later leaves the mark as it is.
        assert!(!l.mark("b", Found::Hotfix, Some(("a", RemedyKind::Rollforward))));
        // Removing its remedy leaves it failed, unremedied.
        l.remove("c");
        let b = l.deployment("b").unwrap();
        assert!(b.change_failure.is_some() && b.recovery_ns().is_none());
        assert!(l.unmark("b") && !l.unmark("b"));
        // Put again (its id), a deployment keeps its marks.
        assert!(l.mark("a", Found::Manual, Some(("b", RemedyKind::Rollback))));
        let again = Deployment { version: "x".into(), change_failure: None, remedies: Vec::new(), ..l.deployment("a").unwrap().clone() };
        l.put(again);
        assert!(l.deployment("a").unwrap().change_failure.is_some());
    }

    /// Lead times: merges left out; a deployment's is its commits' mean; the range's the median of
    /// its deployments'; failure rate and recovery over the range; week by week from Monday; a
    /// service, an environment, a team; a range backwards, nothing.
    #[test]
    fn the_four_keys_are_taken_over_a_range() {
        let mut l = Ledger::default();
        let mut merge = commit('m', 0, "Merge branch 'feature'");
        merge.merge = true;
        let deps = vec![
            (dep("a", "shop", 0, Some('1')), vec![]),
            // Commits 2 h and 4 h old as it finishes: 3 h its lead.
            (dep("b", "shop", 10, Some('2')), vec![commit('b', 8, "one"), commit('c', 6, "two"), merge]),
            // 1 h.
            (dep("c", "shop", 20, Some('3')), vec![commit('d', 19, "three")]),
            // Back to b: c failed, recovered in 2 h.
            (dep("d", "shop", 22, Some('2')), vec![]),
            (dep("e", "cart", 30, Some('5')), vec![]),
        ];
        run(&mut l, deps);
        assert_eq!(l.deployment("b").unwrap().lead.map(|x| (x.commits, x.mean_s, x.min_s, x.max_s)), Some((2, 3.0 * 3600.0, 7200.0, 14_400.0)));
        let q = Query { from: T0, to: T0 + 7 * DAY_NS, ..Query::default() };
        let s = l.summary(&q);
        assert_eq!((s.deployments, s.change_failures, s.recovered), (5, 1, 1));
        assert_eq!(s.lead_time_s, Some(2.0 * 3600.0), "the median of 3 h and 1 h");
        assert_eq!(s.failure_rate, Some(0.2));
        assert_eq!(s.recovery_s, Some(2.0 * 3600.0));
        assert!((s.per_day - 5.0 / 7.0).abs() < 1e-12);
        assert_eq!(s.weeks.iter().map(|w| w.deployments).sum::<u64>(), 5);
        assert!(s.weeks.iter().skip(1).all(|w| w.start == week_of(w.start)), "every week but the first begins on a Monday");
        let shop = l.summary(&Query { service: Some("shop".into()), ..q.clone() });
        assert_eq!((shop.deployments, shop.failure_rate), (4, Some(0.25)));
        assert_eq!(l.summary(&Query { team: Some("nobody".into()), ..q.clone() }).deployments, 0);
        let backwards = l.summary(&Query { from: q.to, to: q.from, ..q });
        assert_eq!((backwards.deployments, backwards.weeks.len()), (0, 0));
    }

    /// A rollback's target is looked for among the latest [`LOOKBACK`] deployments: one past them is
    /// not found; one among them is, however long ago.
    #[test]
    fn a_rollback_is_looked_for_among_the_latest_deployments() {
        let v = |id: String, h: i64, version: &str| Deployment { version: version.into(), ..dep(&id, "api", h, None) };
        let mut l = Ledger::default();
        l.put(v("first".into(), 0, "old"));
        for i in 0..LOOKBACK as i64 {
            l.put(v(format!("n{i}"), 1 + i, &format!("1.{i}")));
        }
        // LOOKBACK deployments after "first": it is past the bound.
        l.put(v("back".into(), 2 + LOOKBACK as i64, "old"));
        assert!(l.detect("back", &[]).is_empty());
        l.remove("n0");
        // One fewer between them: found, and the one after it failed.
        assert_eq!(l.detect("back", &[]), vec![Mark { failed: "n1".into(), found: Found::Rollback, kind: RemedyKind::Rollback }]);
        // A version deployed once is no rollback, and looks through nothing.
        l.put(v("new".into(), 3 + LOOKBACK as i64, "brand-new"));
        assert!(l.detect("new", &[]).is_empty());
    }

    #[test]
    fn medians_weeks_and_keys_are_what_they_say() {
        assert_eq!(median(vec![]), None);
        assert_eq!(median(vec![3.0, 1.0, 2.0]), Some(2.0));
        assert_eq!(median(vec![4.0, 1.0, 2.0, 3.0]), Some(2.5));
        assert_eq!(median(vec![f64::NAN, 1.0]), Some(1.0));
        // 1970-01-05 was a Monday.
        assert_eq!(week_of(4 * DAY_NS), 4 * DAY_NS);
        assert_eq!(week_of(10 * DAY_NS + 5), 4 * DAY_NS);
        assert_eq!(week_of(11 * DAY_NS), 11 * DAY_NS);
        assert_eq!(sha_key("abcdef1"), sha_key(" ABCDEF1 "));
        assert_ne!(sha_key(&format!("{:040x}", 1)), sha_key(&format!("{:040x}", 2)), "the whole sha, not its first digits");
        assert_eq!(sha_key("xyz"), None);
        assert_eq!(sha_key("abc"), None);
        assert_eq!(reverted_title("Revert \"Add \"quoted\" search\""), Some("Add \"quoted\" search"));
        assert!(says_hotfix("Merge branch 'HOTFIX/x'") && !says_hotfix("hot fix"));
    }
}
