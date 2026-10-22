//! The ledger (`docs/observe.md` §10.7): every deployment and failure kept, each service and
//! environment's deployments in the order they finished; what a deployment's coming marks failed
//! (a rollback, a revert, a hotfix, as Datadog finds them); the four keys over a range.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

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

/// A title as two are compared: trimmed, without a squashed pull request's number at its end
/// (GitHub's ` (#123)`).
fn title_text(title: &str) -> &str {
    let t = title.trim();
    if let Some(open) = t.rfind(" (#")
        && t.ends_with(')')
        && t.get(open + 3..t.len() - 1).is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    {
        return t.get(..open).unwrap_or(t).trim_end();
    }
    t
}

/// A title's key.
pub fn title_key(title: &str) -> u64 {
    fnv(title_text(title))
}

/// The title a revert's names: `Revert "<title>"` (a squashed pull request's, `Revert "<title>"
/// (#123)`).
pub fn reverted_title(title: &str) -> Option<&str> {
    title_text(title).strip_prefix("Revert \"")?.strip_suffix('"')
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

/// Names many deployments share (their services, environments, teams, sources and repositories),
/// each held once and told by its number. They are few (an organization's), and kept.
#[derive(Debug, Default)]
struct Names {
    all: Vec<Arc<str>>,
    of: HashMap<Arc<str>, u32>,
}

impl Names {
    /// `s`'s number, given it when it has none.
    fn put(&mut self, s: &str) -> u32 {
        if let Some(&i) = self.of.get(s) {
            return i;
        }
        let i = u32::try_from(self.all.len()).unwrap_or(u32::MAX);
        let a: Arc<str> = Arc::from(s);
        self.all.push(a.clone());
        self.of.insert(a, i);
        i
    }

    /// `s`'s number, if it has one.
    fn get(&self, s: &str) -> Option<u32> {
        self.of.get(s).copied()
    }

    fn str(&self, i: u32) -> &str {
        self.all.get(i as usize).map_or("", |s| s)
    }
}

/// A deployment as the ledger holds it: what many share by its number, the rest as small as it goes
/// (a busy kiln keeps a million).
#[derive(Debug)]
struct Row {
    id: Arc<str>,
    service: u32,
    env: u32,
    team: u32,
    source: u32,
    /// Its repository (by number) and commit; an empty commit, none.
    repo: u32,
    sha: Box<str>,
    version: Box<str>,
    started_at: i64,
    finished_at: i64,
    tags: Box<[Box<str>]>,
    lead: Option<Lead>,
    change_failure: Option<Box<ChangeFailure>>,
    remedies: Vec<Arc<str>>,
    /// Its commits' keys: their shas' (the first `shas`), then their titles'.
    commits: Box<[u64]>,
    shas: u32,
    /// What it deploys, hashed: what a rollback is told by.
    ident: Option<u64>,
    /// Its place among deployments finished at the same instant: its id's hash (another's, nudged).
    tie: u64,
}

impl Row {
    fn shas(&self) -> &[u64] {
        self.commits.get(..self.shas as usize).unwrap_or(&[])
    }

    fn titles(&self) -> &[u64] {
        self.commits.get(self.shas as usize..).unwrap_or(&[])
    }

    fn recovery_ns(&self) -> Option<i64> {
        let r = self.change_failure.as_ref()?.remedy.as_ref()?;
        Some(r.finished_at.saturating_sub(self.finished_at).max(0))
    }
}

/// A kept deployment, read where it is held.
#[derive(Debug, Clone, Copy)]
pub struct RowRef<'a> {
    r: &'a Row,
    n: &'a Names,
}

impl<'a> RowRef<'a> {
    pub fn id(&self) -> &'a str {
        &self.r.id
    }

    pub fn service_name(&self) -> &'a str {
        self.n.str(self.r.service)
    }

    pub fn env(&self) -> &'a str {
        self.n.str(self.r.env)
    }

    pub fn version(&self) -> &'a str {
        &self.r.version
    }

    pub fn started_at(&self) -> i64 {
        self.r.started_at
    }

    pub fn finished_at(&self) -> i64 {
        self.r.finished_at
    }

    pub fn change_failure(&self) -> Option<&'a ChangeFailure> {
        self.r.change_failure.as_deref()
    }

    /// It as the API and the ledger's file say it.
    pub fn to_deployment(&self) -> Deployment {
        let r = self.r;
        Deployment {
            id: r.id.to_string(),
            service: self.n.str(r.service).to_owned(),
            env: self.n.str(r.env).to_owned(),
            team: self.n.str(r.team).to_owned(),
            version: r.version.to_string(),
            started_at: r.started_at,
            finished_at: r.finished_at,
            git: (!r.sha.is_empty()).then(|| Git { commit_sha: r.sha.to_string(), repository_url: self.n.str(r.repo).to_owned() }),
            custom_tags: r.tags.iter().map(|t| t.to_string()).collect(),
            source: self.n.str(r.source).to_owned(),
            lead: r.lead,
            change_failure: r.change_failure.as_deref().cloned(),
            remedies: r.remedies.iter().map(|x| x.to_string()).collect(),
            shas: r.shas().to_vec(),
            titles: r.titles().to_vec(),
        }
    }
}

/// A kept deployment as a log search reads it: its service, its `env`, `team`, `version`, `id`,
/// `git.commit_sha` and `git.repository_url`, its tags, `change_failure` (`true` when marked).
impl kiln_obs::logs::search::Fields for RowRef<'_> {
    fn message(&self) -> &str {
        ""
    }
    fn service(&self) -> &str {
        self.n.str(self.r.service)
    }
    fn host(&self) -> &str {
        ""
    }
    fn source(&self) -> &str {
        "dora"
    }
    fn severity(&self) -> u8 {
        if self.r.change_failure.is_some() { 17 } else { 9 }
    }
    fn attr(&self, key: &str) -> Option<&str> {
        let r = self.r;
        match key {
            "env" => Some(self.n.str(r.env)),
            "team" => Some(self.n.str(r.team)),
            "version" => Some(&*r.version),
            "id" => Some(&*r.id),
            "git.commit_sha" => Some(&*r.sha),
            "git.repository_url" => (!r.sha.is_empty()).then(|| self.n.str(r.repo)),
            "change_failure" => Some(if r.change_failure.is_some() { "true" } else { "false" }),
            other => r.tags.iter().find_map(|t| t.split_once(':').filter(|(k, _)| *k == other).map(|(_, v)| v)),
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

type Key = (u32, u32);

/// Every deployment and failure kept.
#[derive(Debug, Default)]
pub struct Ledger {
    names: Names,
    /// The deployments, in no order (a slab: a removed one's place taken by the last), and where
    /// each id's is. A map of rows would round its table up to a power of two of them.
    rows: Vec<Row>,
    at: HashMap<Arc<str>, u32>,
    /// Each service and environment's deployments by finish (then a hash of the id), each its row's
    /// place: a range read in order without a lookup by id.
    order: HashMap<Key, BTreeMap<(i64, u64), u32>>,
    /// Each service and environment's deployments by what they deploy, how many: a rollback's target
    /// is looked for only when what a deployment deploys was deployed before.
    deployed: HashMap<Key, HashMap<u64, u32>>,
    /// The teams the deployments name, how many each.
    teams: HashMap<u32, u32>,
    failures: HashMap<String, Failure>,
}

impl Ledger {
    fn view<'a>(&'a self, r: &'a Row) -> RowRef<'a> {
        RowRef { r, n: &self.names }
    }

    fn get(&self, id: &str) -> Option<&Row> {
        self.at.get(id).and_then(|&i| self.rows.get(i as usize))
    }

    fn get_mut(&mut self, id: &str) -> Option<&mut Row> {
        let i = *self.at.get(id)?;
        self.rows.get_mut(i as usize)
    }

    /// A service and environment's key, if either is known.
    fn key(&self, service: &str, env: &str) -> Option<Key> {
        Some((self.names.get(service)?, self.names.get(env)?))
    }

    pub fn deployment(&self, id: &str) -> Option<Deployment> {
        self.get(id).map(|r| self.view(r).to_deployment())
    }

    /// A kept deployment, read where it is held.
    pub fn row(&self, id: &str) -> Option<RowRef<'_>> {
        self.get(id).map(|r| self.view(r))
    }

    pub fn failure(&self, id: &str) -> Option<&Failure> {
        self.failures.get(id)
    }

    /// How many deployments and failures it keeps.
    pub fn len(&self) -> (usize, usize) {
        (self.rows.len(), self.failures.len())
    }

    /// Every deployment, read where it is held.
    pub fn rows(&self) -> impl Iterator<Item = RowRef<'_>> {
        self.rows.iter().map(|r| self.view(r))
    }

    /// Every deployment, as the ledger's file says them.
    pub fn deployments(&self) -> impl Iterator<Item = Deployment> + '_ {
        self.rows().map(|r| r.to_deployment())
    }

    pub fn failures(&self) -> impl Iterator<Item = &Failure> {
        self.failures.values()
    }

    /// The row just before (or after) `(at, tie)` of `key`.
    fn next_to(&self, key: Key, at: i64, tie: u64, before: bool) -> Option<&Row> {
        let set = self.order.get(&key)?;
        let found = if before { set.range(..(at, tie)).next_back() } else { set.range((std::ops::Bound::Excluded((at, tie)), std::ops::Bound::Unbounded)).next() };
        found.and_then(|(_, &i)| self.rows.get(i as usize))
    }

    /// A deployment's tiebreak: its own, when it is kept; its id's hash else.
    fn tie_of(&self, id: &str) -> u64 {
        self.get(id).map_or_else(|| fnv(id), |r| r.tie)
    }

    /// The deployment of `service` in `env` just before `(at, id)`, `id` itself passed over (kept
    /// at another time, it is said again).
    pub fn before(&self, service: &str, env: &str, at: i64, id: &str) -> Option<Deployment> {
        let set = self.order.get(&self.key(service, env)?)?;
        set.range(..(at, self.tie_of(id))).rev().filter_map(|(_, &i)| self.rows.get(i as usize)).find(|r| &*r.id != id).map(|r| self.view(r).to_deployment())
    }

    /// The deployment of `service` in `env` just after `(at, id)`, `id` itself passed over.
    pub fn after(&self, service: &str, env: &str, at: i64, id: &str) -> Option<Deployment> {
        let set = self.order.get(&self.key(service, env)?)?;
        set.range((std::ops::Bound::Excluded((at, self.tie_of(id))), std::ops::Bound::Unbounded))
            .filter_map(|(_, &i)| self.rows.get(i as usize))
            .find(|r| &*r.id != id)
            .map(|r| self.view(r).to_deployment())
    }

    /// The latest deployment of `service` in `env` at `version`.
    pub fn find_version(&self, service: &str, env: &str, version: &str) -> Option<String> {
        let set = self.order.get(&self.key(service, env)?)?;
        set.values().rev().filter_map(|&i| self.rows.get(i as usize)).find(|r| &*r.version == version).map(|r| r.id.to_string())
    }

    /// What a kept deployment's commits say of it (its lead, its commits' keys): it as it is now.
    pub fn set_commits(&mut self, id: &str, lead: Option<Lead>, shas: &[u64], titles: &[u64]) -> Option<Deployment> {
        let r = self.get_mut(id)?;
        r.lead = lead;
        r.commits = shas.iter().chain(titles).copied().collect();
        r.shas = u32::try_from(shas.len()).unwrap_or(u32::MAX);
        self.deployment(id)
    }

    /// `d` kept, replacing one of its id: the marks that one had and `d` does not say stay (it was
    /// found failed, or remedied one). Whether one was replaced.
    pub fn put(&mut self, d: Deployment) -> bool {
        let old = self.unlink(&d.id);
        let ident = d.ident();
        let mut commits: Vec<u64> = d.shas;
        let shas = u32::try_from(commits.len()).unwrap_or(u32::MAX);
        commits.extend(d.titles);
        let id: Arc<str> = Arc::from(d.id.as_str());
        let mut row = Row {
            id: id.clone(),
            service: self.names.put(&d.service),
            env: self.names.put(&d.env),
            team: self.names.put(&d.team),
            source: self.names.put(&d.source),
            repo: d.git.as_ref().map_or(u32::MAX, |g| self.names.put(&g.repository_url)),
            sha: d.git.map(|g| g.commit_sha.into_boxed_str()).unwrap_or_default(),
            version: d.version.into_boxed_str(),
            started_at: d.started_at,
            finished_at: d.finished_at,
            tags: d.custom_tags.into_iter().map(String::into_boxed_str).collect(),
            lead: d.lead,
            change_failure: d.change_failure.map(Box::new),
            remedies: d.remedies.iter().map(|x| Arc::from(x.as_str())).collect(),
            commits: commits.into_boxed_slice(),
            shas,
            ident,
            tie: fnv(&id),
        };
        let existed = old.is_some();
        if let Some(o) = old {
            if row.change_failure.is_none() {
                row.change_failure = o.change_failure;
            }
            if row.remedies.is_empty() {
                row.remedies = o.remedies;
            }
        }
        let key = (row.service, row.env);
        if let Some(i) = row.ident {
            *self.deployed.entry(key).or_default().entry(i).or_default() += 1;
        }
        *self.teams.entry(row.team).or_default() += 1;
        let place = u32::try_from(self.rows.len()).unwrap_or(u32::MAX);
        let set = self.order.entry(key).or_default();
        // Two ids of one hash, finished at one instant: the later nudged past it.
        while set.contains_key(&(row.finished_at, row.tie)) {
            row.tie = row.tie.wrapping_add(1);
        }
        set.insert((row.finished_at, row.tie), place);
        self.at.insert(id, place);
        self.rows.push(row);
        existed
    }

    /// The deployment `id` out of the maps, its marks and those naming it untouched.
    fn unlink(&mut self, id: &str) -> Option<Row> {
        let i = self.at.remove(id)? as usize;
        if i >= self.rows.len() {
            return None;
        }
        let r = self.rows.swap_remove(i);
        let key = (r.service, r.env);
        if let Some(set) = self.order.get_mut(&key) {
            set.remove(&(r.finished_at, r.tie));
            if set.is_empty() {
                self.order.remove(&key);
            }
        }
        // The last row is in its place now: where it is said to be, there.
        if let Some(moved) = self.rows.get(i) {
            let place = u32::try_from(i).unwrap_or(u32::MAX);
            if let Some(x) = self.at.get_mut(&*moved.id) {
                *x = place;
            }
            if let Some(x) = self.order.get_mut(&(moved.service, moved.env)).and_then(|set| set.get_mut(&(moved.finished_at, moved.tie))) {
                *x = place;
            }
        }
        if let Some(i) = r.ident
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
        if let Some(n) = self.teams.get_mut(&r.team) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                self.teams.remove(&r.team);
            }
        }
        Some(r)
    }

    /// The deployment `id` gone, and every mark naming it: what it remedied is failed still (the
    /// failure was found), without its remedy; what remedied it no longer names it. It, as it was.
    pub fn remove(&mut self, id: &str) -> Option<Deployment> {
        let r = self.unlink(id)?;
        for failed in &r.remedies {
            if let Some(f) = self.get_mut(failed)
                && let Some(cf) = f.change_failure.as_mut()
                && cf.remedy.as_ref().is_some_and(|x| *x.id == *r.id)
            {
                cf.remedy = None;
            }
        }
        if let Some(x) = r.change_failure.as_ref().and_then(|cf| cf.remedy.as_ref())
            && let Some(by) = self.get_mut(x.id.as_str())
        {
            by.remedies.retain(|f| *f != r.id);
        }
        Some(self.view(&r).to_deployment())
    }

    pub fn put_failure(&mut self, f: Failure) -> Option<Failure> {
        self.failures.insert(f.id.clone(), f)
    }

    pub fn remove_failure(&mut self, id: &str) -> Option<Failure> {
        self.failures.remove(id)
    }

    /// Every deployment finished and failure begun before `before` (ns) gone: how many.
    pub fn expire(&mut self, before: i64) -> usize {
        let old: Vec<Arc<str>> = self.rows.iter().filter(|r| r.finished_at < before).map(|r| r.id.clone()).collect();
        let gone_failures: Vec<String> = self.failures.values().filter(|f| f.started_at < before).map(|f| f.id.clone()).collect();
        for id in &old {
            self.remove(id);
        }
        for id in &gone_failures {
            self.failures.remove(id);
        }
        old.len() + gone_failures.len()
    }

    /// Its maps no larger than what they hold (once it is read at the start).
    pub fn shrink(&mut self) {
        self.rows.shrink_to_fit();
        self.at.shrink_to_fit();
        self.order.shrink_to_fit();
        self.deployed.shrink_to_fit();
        for m in self.deployed.values_mut() {
            m.shrink_to_fit();
        }
        self.failures.shrink_to_fit();
    }

    /// What the deployment `id` (kept) marks failed, its commits those `commits` (newest first, as
    /// git's log gives them): a rollback's, a revert's and a hotfix's (Datadog's rules), each failed
    /// deployment once (the first rule that finds it).
    pub fn detect(&self, id: &str, commits: &[Commit]) -> Vec<Mark> {
        let Some(d) = self.get(id) else { return Vec::new() };
        let key = (d.service, d.env);
        let mut out: Vec<Mark> = Vec::new();
        let mut push = |failed: &Row, found: Found, kind: RemedyKind| {
            if failed.id != d.id && !out.iter().any(|m| *m.failed == *failed.id) {
                out.push(Mark { failed: failed.id.to_string(), found, kind });
            }
        };
        let prev = self.next_to(key, d.finished_at, d.tie, true);
        // A rollback: what it deploys was deployed before, but not just before; the deployment
        // after that earlier one failed (V1 → V2 → V3 → V1: V2). Looked for only when another
        // deployment of it is kept.
        if let Some(what) = d.ident
            && prev.is_some_and(|p| p.ident != Some(what))
            && self.deployed.get(&key).and_then(|m| m.get(&what)).is_some_and(|n| *n > 1)
            && let Some(target) = self.earlier(d, |e| e.ident == Some(what))
            && let Some(failed) = self.next_to(key, target.finished_at, target.tie, false)
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
            let brought = sha.and_then(|s| self.earlier(d, |e| e.shas().contains(&s))).or_else(|| self.earlier(d, |e| e.titles().contains(&title_k)));
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
    fn earlier(&self, d: &Row, f: impl Fn(&Row) -> bool) -> Option<&Row> {
        let set = self.order.get(&(d.service, d.env))?;
        set.range(..(d.finished_at, d.tie)).rev().take(LOOKBACK).filter_map(|(_, &i)| self.rows.get(i as usize)).find(|e| f(e))
    }

    /// `failed` marked failed, found so, remedied by `remedy` (a kept deployment's id) when one did:
    /// whether that changed it. A mark found stands against one found later; one said by hand gives
    /// it its remedy (none given, the one found stays).
    pub fn mark(&mut self, failed: &str, found: Found, remedy: Option<(&str, RemedyKind)>) -> bool {
        let remedy = remedy.and_then(|(id, kind)| self.get(id).map(|r| Remedy { id: r.id.to_string(), kind, finished_at: r.finished_at }));
        let Some(f) = self.get_mut(failed) else { return false };
        let next = match f.change_failure.as_deref() {
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
        let old = f.change_failure.replace(Box::new(next));
        let failed_id = f.id.clone();
        // The remedies it names, kept both ways.
        if let Some(r) = old.and_then(|cf| cf.remedy)
            && let Some(was) = self.get_mut(r.id.as_str())
        {
            was.remedies.retain(|x| *x != failed_id);
        }
        if let Some(r) = remedy
            && let Some(by) = self.get_mut(r.id.as_str())
            && !by.remedies.contains(&failed_id)
        {
            by.remedies.push(failed_id);
        }
        true
    }

    /// `failed` no longer marked: whether it was.
    pub fn unmark(&mut self, failed: &str) -> bool {
        let Some(f) = self.get_mut(failed) else { return false };
        let Some(old) = f.change_failure.take() else { return false };
        let failed_id = f.id.clone();
        if let Some(r) = old.remedy
            && let Some(by) = self.get_mut(r.id.as_str())
        {
            by.remedies.retain(|x| *x != failed_id);
        }
        true
    }

    /// The services, environments and teams it holds, each sorted: from what it keeps of them, not
    /// a look at every deployment.
    pub fn facets(&self) -> (Vec<String>, Vec<String>, Vec<String>) {
        let mut services: BTreeSet<&str> = self.order.keys().map(|(s, _)| self.names.str(*s)).collect();
        let mut envs: BTreeSet<&str> = self.order.keys().map(|(_, e)| self.names.str(*e)).collect();
        let mut teams: BTreeSet<&str> = self.teams.keys().map(|t| self.names.str(*t)).collect();
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
    pub fn finished_between(&self, from: i64, to: i64) -> impl Iterator<Item = RowRef<'_>> + '_ {
        self.order.values().filter(move |_| from < to).flat_map(move |set| set.range((from, 0)..(to, 0))).filter_map(|(_, &i)| self.rows.get(i as usize)).map(|r| self.view(r))
    }

    /// The deployments `q` picks, in finish order (a range backwards picks none).
    fn picked<'a>(&'a self, q: &'a Query) -> Vec<&'a Row> {
        if q.from >= q.to {
            return Vec::new();
        }
        let named = |want: &Option<String>| want.as_deref().map(|w| self.names.get(w));
        let (service, env, team) = (named(&q.service), named(&q.env), named(&q.team));
        // A name no deployment has: none.
        if [service, env, team].iter().any(|x| matches!(x, Some(None))) {
            return Vec::new();
        }
        let (service, env, team) = (service.flatten(), env.flatten(), team.flatten());
        let mut keys: Vec<&Key> = self.order.keys().filter(|(s, e)| service.is_none_or(|x| x == *s) && env.is_none_or(|x| x == *e)).collect();
        keys.sort();
        keys.into_iter()
            .filter_map(|k| self.order.get(k))
            .flat_map(|set| set.range((q.from, 0)..(q.to, 0)))
            .filter_map(|(_, &i)| self.rows.get(i as usize))
            .filter(|r| team.is_none_or(|t| t == r.team))
            .collect()
    }

    /// The four keys over `q`'s range, and week by week.
    pub fn summary(&self, q: &Query) -> Summary {
        let mut weeks: Vec<(i64, Vec<&Row>)> = Vec::new();
        let mut first = q.from;
        while first < q.to {
            let next = week_of(first).saturating_add(WEEK_NS);
            weeks.push((first, Vec::new()));
            first = next;
        }
        let all: Vec<&Row> = self.picked(q);
        for d in &all {
            let at = weeks.partition_point(|(start, _)| *start <= d.finished_at);
            if let Some((_, w)) = at.checked_sub(1).and_then(|i| weeks.get_mut(i)) {
                w.push(d);
            }
        }
        let figures = |ds: &[&Row]| {
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
        // GitHub's revert of a squashed pull request: no sha said, both titles numbered.
        let mut l = Ledger::default();
        let marked = run(
            &mut l,
            vec![
                (dep("a", "shop", 0, Some('1')), vec![]),
                (dep("b", "shop", 1, Some('2')), vec![commit('b', 0, "Add search (#122)")]),
                (dep("c", "shop", 2, Some('3')), vec![commit('c', 1, "Revert \"Add search (#122)\" (#123)")]),
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

    /// Puts, removals (each moving the last row into the removed one's place) and puts again, in a
    /// random order, against a list of what should be kept: every id finds its own deployment, none
    /// removed is found, and each deployment's neighbours are the list's.
    #[test]
    fn rows_stay_found_through_removals() {
        let mut seed = 0x5eed_u64;
        let mut next = |n: u64| {
            seed = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = seed;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            (z ^ (z >> 31)) % n
        };
        let mut l = Ledger::default();
        let mut kept: Vec<(i64, String)> = Vec::new();
        for round in 0..3_000 {
            let i = next(400) as i64;
            let id = format!("d{i:04}");
            if next(3) == 0 {
                let had = kept.iter().any(|(_, x)| *x == id);
                assert_eq!(l.remove(&id).is_some(), had, "round {round}: removing {id}");
                kept.retain(|(_, x)| *x != id);
            } else {
                l.put(dep(&id, "shop", i, Some('a')));
                kept.retain(|(_, x)| *x != id);
                kept.push((T0 + i * H, id));
            }
        }
        kept.sort();
        assert_eq!(l.len().0, kept.len());
        for (k, (at, id)) in kept.iter().enumerate() {
            assert_eq!(l.deployment(id).map(|d| d.id), Some(id.clone()));
            let before = k.checked_sub(1).and_then(|j| kept.get(j)).map(|(_, x)| x.clone());
            assert_eq!(l.before("shop", "prod", *at, id).map(|d| d.id), before, "{id}'s previous");
            assert_eq!(l.after("shop", "prod", *at, id).map(|d| d.id), kept.get(k + 1).map(|(_, x)| x.clone()), "{id}'s next");
        }
        for i in 0..400 {
            let id = format!("d{i:04}");
            assert_eq!(l.deployment(&id).is_some(), kept.iter().any(|(_, x)| *x == id), "{id}");
        }
    }

    /// A deployment said again with a later finish: what comes before its new time is another, not
    /// itself as it was.
    #[test]
    fn a_deployment_said_again_is_not_its_own_previous() {
        let mut l = Ledger::default();
        l.put(dep("a", "shop", 0, Some('1')));
        l.put(dep("b", "shop", 1, Some('2')));
        let later = T0 + 5 * H;
        assert_eq!(l.before("shop", "prod", later, "b").map(|d| d.id), Some("a".to_owned()));
        assert_eq!(l.after("shop", "prod", T0 - H, "a").map(|d| d.id), Some("b".to_owned()), "a passed over");
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
        // GitHub's squashed revert of a squashed pull request: both titles carry their numbers.
        assert_eq!(reverted_title("Revert \"Add search (#122)\" (#123)"), Some("Add search (#122)"));
        assert_eq!(title_key("Add search (#122)"), title_key("Add search"));
        assert_ne!(title_key("Add search (#x)"), title_key("Add search"), "only a number is a pull request's");
        assert!(says_hotfix("Merge branch 'HOTFIX/x'") && !says_hotfix("hot fix"));
    }
}
