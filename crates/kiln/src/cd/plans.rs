//! Plans: what a commit, or every diff of a stack, would deploy before it lands (docs/cd.md,
//! "Stacks, diffs and pull requests").
//!
//! A plan finds the apps a stack can move without rendering anything. An app's render key is the
//! content address of the paths its render reads, so the paths each commit changes say which
//! apps it touches. Those apps' sources are resolved at each commit of the stack, each distinct
//! content is rendered once (and kept, so planning again renders nothing), and the engine says
//! what each render would do to what runs ([`kiln_cd::engine::Engine::plan`]). Every diff gets
//! its **own change** (its render against the render of the diff below), the **stack's change**
//! (against trunk) and the **steps** a sync of it would take. A plan is a check: a diff fails when
//! its sources can't be read or rendered, or when it drops an object from git that `Prune=false`
//! keeps in the cluster.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::sync::Arc;

use http::StatusCode;
use kiln_cd::apps::AppDef;
use kiln_cd::engine::{Plan, Planned, Read, StepKind};
use kiln_cd::git::Mirror;
use kiln_cd::model::PruneMode;
use kiln_cd::obj::{self, Blob, GroupKind, ResKey};
use kiln_cd::render::Dep;
use parking_lot::Mutex;
use serde_json::{Value, json};

use super::ops::{self, Fail};
use super::{Cd, Resolved};

/// The most commits one plan takes.
pub const MAX_COMMITS: usize = 100;
/// The most apps one plan renders, unless asked for another number.
const MAX_APPS: usize = 200;
/// Apps planned at once.
const PARALLEL: usize = 4;
/// Lines one object's hunks may hold before only their counts are shown.
const MAX_LINES: usize = 2_000;
/// What kept renders may take, compressed.
const KEPT_BYTES: usize = 64 << 20;
/// Objects one app's plan reads from its cluster itself, at most.
const MAX_READS: usize = 200;
/// Branches listed per repository, the newest first.
const MAX_BRANCHES: usize = 50;

fn bad(msg: impl Into<String>) -> Fail {
    (StatusCode::BAD_REQUEST, msg.into())
}

fn missing(msg: impl Into<String>) -> Fail {
    (StatusCode::NOT_FOUND, msg.into())
}

fn failed(e: anyhow::Error) -> Fail {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}

/// What to plan.
#[derive(Clone, Debug, Default)]
pub struct Ask {
    /// The repository, in any spelling of its URL; by default whichever of the apps'
    /// repositories has the commit.
    pub repo: Option<String>,
    /// One commit (a hash, branch or tag), planned against its first parent, or `base`.
    pub commit: Option<String>,
    /// A stack's top commit: the stack is every commit above `base` up to it.
    pub head: Option<String>,
    /// The commit below the stack's bottom; by default where `head` leaves trunk.
    pub base: Option<String>,
    /// The branch it lands on; by default the repository's default branch.
    pub trunk: Option<String>,
    /// A pull request, whose head (`refs/pull/<n>/head`, or a GitLab merge request's) is fetched
    /// and planned as a stack.
    pub pr: Option<u64>,
    /// Only the apps this matches (`cd.apps.list`'s filters).
    pub selector: Value,
    /// Counts and keys only, no hunks.
    pub brief: bool,
    /// The most apps to plan.
    pub max_apps: usize,
}

impl Ask {
    /// Reads `cd.plans.get`'s input.
    pub fn from_json(v: &Value) -> Ask {
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::trim).filter(|x| !x.is_empty()).map(str::to_owned);
        Ask {
            repo: s("repo"),
            commit: s("commit"),
            head: s("head"),
            base: s("base"),
            trunk: s("trunk"),
            pr: v.get("pr").and_then(|p| p.as_u64().or_else(|| p.as_str().and_then(|s| s.trim().trim_start_matches('#').parse().ok()))),
            selector: v.clone(),
            brief: v.get("brief").and_then(Value::as_bool).unwrap_or(false),
            max_apps: v.get("max_apps").and_then(Value::as_u64).map_or(MAX_APPS, |n| usize::try_from(n).unwrap_or(usize::MAX).clamp(1, 5_000)),
        }
    }
}

/// Renders plans made, by key, compressed: the newest, within a byte budget.
#[derive(Default)]
pub struct Renders(Mutex<VecDeque<(u64, Arc<Kept>)>>);

/// One render kept: its objects and the Applications it renders (app of apps).
pub struct Kept {
    objects: Blob,
    children: Blob,
}

impl Kept {
    fn new(objects: Vec<Value>, children: Vec<Value>) -> Kept {
        Kept { objects: Blob::encode(&Value::Array(objects)), children: Blob::encode(&Value::Array(children)) }
    }

    fn bytes(&self) -> usize {
        self.objects.len() + self.children.len()
    }

    fn objects(&self) -> Vec<Value> {
        match self.objects.decode() {
            Ok(Value::Array(a)) => a,
            _ => Vec::new(),
        }
    }

    /// The Applications it renders, as objects of the plan (kiln registers them as apps rather
    /// than applying them, so they have no steps, only changes).
    fn children(&self) -> Vec<Planned> {
        let Ok(Value::Array(a)) = self.children.decode() else { return Vec::new() };
        a.into_iter()
            .map(|v| Planned {
                key: ResKey::new(obj::group(&v), obj::kind(&v), obj::namespace(&v), obj::name(&v)),
                hash: obj::hash_value(&v),
                object: v,
                hook: false,
            })
            .collect()
    }
}

impl Renders {
    fn get(&self, key: u64) -> Option<Arc<Kept>> {
        let mut q = self.0.lock();
        let i = q.iter().position(|(k, _)| *k == key)?;
        let e = q.remove(i)?;
        let out = e.1.clone();
        q.push_back(e);
        Some(out)
    }

    fn put(&self, key: u64, kept: Arc<Kept>) {
        let mut q = self.0.lock();
        q.retain(|(k, _)| *k != key);
        q.push_back((key, kept));
        let mut total: usize = q.iter().map(|(_, k)| k.bytes()).sum();
        while total > KEPT_BYTES && q.len() > 1 {
            if let Some((_, k)) = q.pop_front() {
                total = total.saturating_sub(k.bytes());
            }
        }
    }
}

/// Whether two URLs name one repository: scheme, user, a trailing slash or `.git`, and the
/// case aside (`git@host:o/r` is `https://host/o/r`).
pub fn same_repo(a: &str, b: &str) -> bool {
    repo_name(a) == repo_name(b)
}

fn repo_name(url: &str) -> String {
    let mut s = url.trim().to_lowercase();
    if let Some(i) = s.find("://") {
        s = s.split_off(i + 3);
    } else if let Some((host, path)) = s.split_once(':') {
        s = format!("{host}/{path}");
    }
    if let Some(i) = s.find('@').filter(|i| !s.get(..*i).unwrap_or("").contains('/')) {
        s = s.split_off(i + 1);
    }
    let s = s.trim_end_matches('/');
    s.strip_suffix(".git").unwrap_or(s).to_owned()
}

/// Whether a source's revision follows `trunk`.
fn tracks(revision: &str, trunk: &str, default_branch: Option<&str>) -> bool {
    let r = revision.trim();
    let r = r.strip_prefix("refs/heads/").unwrap_or(r);
    r == trunk || ((r.is_empty() || r == "HEAD") && default_branch == Some(trunk))
}

/// Whether a path under `dir` changed.
fn under(changed: &BTreeSet<String>, dir: &str) -> bool {
    let d = dir.trim_matches('/');
    if d.is_empty() || d == "." {
        return true;
    }
    let prefix = format!("{d}/");
    changed.contains(d) || changed.range(prefix.clone()..).next().is_some_and(|c| c.starts_with(&prefix))
}

/// Whether a step's changed paths lie under any path an app reads from the stack's repository
/// (`None`: anything may have changed).
fn touches(def: &AppDef, deps: &[Vec<Dep>], in_stack: &[bool], changed: Option<&BTreeSet<String>>) -> bool {
    let Some(changed) = changed else { return true };
    if changed.is_empty() {
        return false;
    }
    if deps.len() != def.sources.len() {
        return true;
    }
    let stack_ref = |name: &str| def.sources.iter().zip(in_stack).any(|(s, i)| *i && s.as_ref.as_deref() == Some(name));
    deps.iter().enumerate().any(|(i, ds)| {
        ds.iter().any(|d| {
            let ours = match &d.source {
                None => in_stack.get(i).copied().unwrap_or(false),
                Some(name) => stack_ref(name),
            };
            ours && under(changed, &d.path)
        })
    })
}

/// A stack in one repository, resolved.
struct Stack {
    /// The repository's URL, as each app that deploys from it spells it.
    urls: Vec<String>,
    /// The branch it lands on.
    trunk: String,
    trunk_commit: String,
    default_branch: Option<String>,
    /// The commit below the bottom diff (`None` when the bottom is a root commit).
    base: Option<String>,
    /// Its commits, bottom first, and each as people read it.
    commits: Vec<String>,
    shown: Vec<Value>,
    /// The paths each step changes: trunk to the base first, then each commit from the one
    /// below. `None`: not known, so anything.
    changed: Vec<Option<BTreeSet<String>>>,
    notes: Vec<String>,
}

impl Stack {
    /// Which of an app's sources the stack moves: git sources in its repository that follow
    /// trunk. The others stay where trunk has them.
    fn moves(&self, def: &AppDef) -> Vec<bool> {
        def.sources
            .iter()
            .map(|s| s.chart.is_none() && self.urls.iter().any(|u| same_repo(u, &s.repo)) && tracks(&s.revision, &self.trunk, self.default_branch.as_deref()))
            .collect()
    }
}

enum Want {
    Commit(String),
    Head(String),
    Pr(u64),
}

fn looks_like_hash(s: &str) -> bool {
    s.len() >= 7 && s.len() <= 64 && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// The commit `want` names in a mirror; `fetch` brings the mirror up to date for it first.
fn locate(m: &Mirror, want: &Want, fetch: bool) -> anyhow::Result<Option<String>> {
    match want {
        Want::Commit(c) | Want::Head(c) => {
            if looks_like_hash(c) && m.has(c) {
                return Ok(m.resolve(c).ok());
            }
            // A branch may have moved; a commit not fetched yet may be on no branch (fetched by its
            // hash), or on a branch the mirror hasn't fetched.
            if fetch && (!looks_like_hash(c) || m.fetch_one(c).is_err() || !m.has(c)) {
                m.fetch()?;
            }
            Ok(m.resolve(c).ok())
        }
        Want::Pr(n) => {
            let r = format!("refs/kiln/pull/{n}");
            m.fetch_one(&format!("+refs/pull/{n}/head:{r}")).or_else(|_| m.fetch_one(&format!("+refs/merge-requests/{n}/head:{r}")))?;
            Ok(m.resolve(&r).ok())
        }
    }
}

fn mirror(cd: &Cd, url: &str) -> anyhow::Result<Mirror> {
    Mirror::open(&cd.mirror_dir(url), url, cd.git_auth(url))
}

/// Every repository an app deploys from, by name, its spellings together, and how many apps
/// deploy from it.
fn repo_groups(cd: &Cd) -> BTreeMap<String, (Vec<String>, usize)> {
    let mut groups: BTreeMap<String, (Vec<String>, usize)> = BTreeMap::new();
    for e in cd.apps.read().values() {
        let mut seen = BTreeSet::new();
        for s in e.def.sources.iter().filter(|s| s.chart.is_none()) {
            let name = repo_name(&s.repo);
            let g = groups.entry(name.clone()).or_default();
            if !g.0.contains(&s.repo) {
                g.0.push(s.repo.clone());
            }
            if seen.insert(name) {
                g.1 += 1;
            }
        }
    }
    groups
}

/// The repositories apps deploy from, for choosing what to plan: each with how many apps
/// deploy from it, its default branch, and its newest branches.
pub async fn repos(cd: &Arc<Cd>) -> Result<Value, Fail> {
    let cd = cd.clone();
    let list = crate::rt::blocking(move || {
        repo_groups(&cd)
            .into_values()
            .filter_map(|(urls, apps)| {
                let url = urls.first()?.clone();
                // Only mirrors already fetched: listing never clones.
                let m = cd.mirror_dir(&url).join("HEAD").exists().then(|| mirror(&cd, &url).ok()).flatten();
                let branches: Vec<Value> = m
                    .as_ref()
                    .and_then(|m| m.branches(MAX_BRANCHES).ok())
                    .unwrap_or_default()
                    .into_iter()
                    .map(|(name, commit, at)| json!({"name": name, "commit": commit, "at_ms": at * 1000}))
                    .collect();
                Some(json!({"url": url, "urls": urls, "apps": apps, "default_branch": m.as_ref().and_then(Mirror::default_branch), "branches": branches}))
            })
            .collect::<Vec<Value>>()
    })
    .await
    .map_err(failed)?;
    Ok(json!({"repos": list}))
}

/// Finds what `ask` names: the repository, trunk, the base and the stack's commits.
fn find(cd: &Cd, ask: &Ask) -> Result<Stack, Fail> {
    // Revisions go to git as arguments: none may read as an option.
    for (name, v) in [("commit", &ask.commit), ("head", &ask.head), ("base", &ask.base), ("trunk", &ask.trunk)] {
        if v.as_deref().is_some_and(|r| r.starts_with('-') || r.chars().any(char::is_whitespace)) {
            return Err(bad(format!("{name} is not a revision")));
        }
    }
    let want = match (&ask.commit, &ask.head, ask.pr) {
        (Some(c), None, None) => Want::Commit(c.clone()),
        (None, Some(h), None) => Want::Head(h.clone()),
        (None, None, Some(n)) => Want::Pr(n),
        (None, None, None) => return Err(bad("say what to plan: a commit, a stack's head, or a pull request (pr)")),
        _ => return Err(bad("plan one of commit, head and pr")),
    };
    let mut groups: BTreeMap<String, Vec<String>> = repo_groups(cd).into_iter().map(|(name, (urls, _))| (name, urls)).collect();
    if let Some(r) = &ask.repo {
        groups.retain(|name, _| *name == repo_name(r));
        if groups.is_empty() {
            return Err(missing(format!("no app deploys from {r}")));
        }
    }
    // A repository named (or the only one) is fetched for what is asked; otherwise the mirrors
    // are searched as they are.
    let fetch = groups.len() == 1;
    if matches!(want, Want::Pr(_)) && !fetch {
        return Err(bad("say which repository the pull request is in (repo)"));
    }
    let mut found: Vec<(Vec<String>, Mirror, String)> = Vec::new();
    for (_, urls) in groups {
        let Some(first) = urls.first() else { continue };
        let m = mirror(cd, first).map_err(failed)?;
        if let Some(head) = locate(&m, &want, fetch).map_err(failed)? {
            found.push((urls, m, head));
        }
    }
    let what = match &want {
        Want::Commit(c) | Want::Head(c) => c.clone(),
        Want::Pr(n) => format!("pull request {n}"),
    };
    let (urls, m, head) = match found.len() {
        0 => return Err(missing(format!("no repository an app deploys from has {what}"))),
        1 => found.remove(0),
        _ => {
            let names: Vec<String> = found.iter().filter_map(|(u, _, _)| u.first().cloned()).collect();
            return Err(bad(format!("{what} is in several repositories ({}); say which (repo)", names.join(", "))));
        }
    };
    // The other spellings' mirrors need the commits too: their apps resolve from them.
    for u in urls.iter().skip(1) {
        let other = mirror(cd, u).map_err(failed)?;
        if !other.has(&head) && other.fetch_one(&head).is_err() {
            other.fetch().map_err(failed)?;
        }
    }
    let default_branch = m.default_branch();
    let trunk = match ask.trunk.clone().or_else(|| default_branch.clone()) {
        Some(t) => t.strip_prefix("refs/heads/").map(str::to_owned).unwrap_or(t),
        None => return Err(bad("the repository has no default branch; say which branch the stack lands on (trunk)")),
    };
    let trunk_commit = m.resolve(&trunk).map_err(|e| missing(format!("{e:#}")))?;
    let named_base = match &ask.base {
        Some(b) => Some(m.resolve(b).map_err(|e| missing(format!("{e:#}")))?),
        None => None,
    };
    let mut notes = Vec::new();
    let (base, commits) = match want {
        Want::Commit(_) => {
            let base = match named_base {
                Some(b) => Some(b),
                None => m.parent(&head).map_err(failed)?,
            };
            (base, vec![head.clone()])
        }
        Want::Head(_) | Want::Pr(_) => {
            let base = named_base.or_else(|| m.merge_base(&trunk_commit, &head));
            let commits = m.stack(base.as_deref(), &head, MAX_COMMITS).map_err(|e| bad(format!("{e:#}")))?;
            if commits.is_empty() {
                notes.push(format!("nothing to plan: {what} is on {trunk} already"));
            }
            (base, commits)
        }
    };
    let mut changed = Vec::with_capacity(commits.len() + 1);
    changed.push(match &base {
        Some(b) if *b == trunk_commit => Some(BTreeSet::new()),
        Some(b) => m.changed(&trunk_commit, b).ok().map(|p| p.into_iter().collect()),
        None => None,
    });
    let mut below = base.clone();
    let mut shown = Vec::with_capacity(commits.len());
    for c in &commits {
        changed.push(below.as_deref().and_then(|b| m.changed(b, c).ok()).map(|p| p.into_iter().collect()));
        below = Some(c.clone());
        shown.push(match m.commit(c) {
            Ok(x) => json!({"sha": x.sha, "author": x.author, "email": x.email, "at_ms": x.at * 1000, "subject": x.subject, "body": x.body}),
            Err(_) => json!({"sha": c}),
        });
    }
    Ok(Stack { urls, trunk, trunk_commit, default_branch, base, commits, shown, changed, notes })
}

/// An app's sources resolved for trunk, the base and each commit of the stack.
struct Resolutions {
    trunk: Resolved,
    base: Option<Result<Resolved, String>>,
    steps: Vec<Result<Resolved, String>>,
}

fn resolutions(cd: &Cd, st: &Stack, def: &AppDef, kube_version: Option<&str>, api_versions: &[String]) -> anyhow::Result<Resolutions> {
    let moves = st.moves(def);
    // Artifacts as a plan sees them: no registry is asked (a planned commit's image may not exist
    // yet).
    let with_artifacts = |mut r: Resolved| {
        let (a, p) = cd.planned_artifacts(def, &r.revision);
        r.add_artifacts(a, p);
        r
    };
    let trunk = with_artifacts(cd.inputs_of(def, kube_version, api_versions, None)?);
    let at = |c: &str| -> Vec<Option<String>> { trunk.commits.iter().zip(&moves).map(|(x, m)| if *m { Some(c.to_owned()) } else { x.clone() }).collect() };
    let resolve = |c: &str| cd.inputs_of(def, kube_version, api_versions, Some(&at(c))).map(with_artifacts).map_err(|e| format!("{e:#}"));
    // A step that changes nothing the app reads leaves its content as the step below had it.
    let same = |i: usize, below: &Resolved| !touches(def, &below.deps, &moves, st.changed.get(i).and_then(Option::as_ref));
    let base = st.base.as_deref().map(|b| if same(0, &trunk) { Ok(trunk.clone()) } else { resolve(b) });
    let mut below = base.as_ref().and_then(|b| b.as_ref().ok().cloned());
    let mut steps = Vec::with_capacity(st.commits.len());
    for (i, c) in st.commits.iter().enumerate() {
        let r = match &below {
            Some(b) if same(i + 1, b) => Ok(b.clone()),
            _ => resolve(c),
        };
        below = r.as_ref().ok().cloned();
        steps.push(r);
    }
    Ok(Resolutions { trunk, base, steps })
}

/// One render as a plan shows it: the engine's plan, and every object to compare (the plan's,
/// and the Applications it renders), in key order.
struct Shown {
    plan: Plan,
    objects: Vec<Planned>,
}

impl Shown {
    fn never(&self) -> BTreeSet<&ResKey> {
        self.plan.held.iter().filter(|(_, m)| *m == PruneMode::Never).map(|(k, _)| k).collect()
    }

    fn confirm(&self) -> BTreeSet<&ResKey> {
        self.plan.held.iter().filter(|(_, m)| *m == PruneMode::Confirm).map(|(k, _)| k).collect()
    }
}

fn named(k: &ResKey) -> String {
    if k.namespace.is_empty() { format!("{} {}", k.kind, k.name) } else { format!("{} {}/{}", k.kind, k.namespace, k.name) }
}

/// One object's change as a plan shows it: its key, what happens, and the hunks between the two
/// sides as YAML (Secrets masked), or only their counts when they are long or not asked for.
fn object_json(key: &ResKey, hook: bool, field: &str, what: &str, before: Option<&Value>, after: Option<&Value>, brief: bool) -> Value {
    let (mut b, mut a) = (before.cloned().map(ops::display), after.cloned().map(ops::display));
    ops::mask_secrets(&mut b, &mut a);
    let hunks = crate::harness::diff::hunks(&ops::yaml(b.as_ref()), &ops::yaml(a.as_ref()), 3);
    let count = |c: char| hunks.iter().flat_map(|h| h.lines.iter()).filter(|l| l.starts_with(c)).count();
    let (plus, minus) = (count('+'), count('-'));
    let mut v = json!({"key": ops::key_json(key), field: what, "hook": hook, "added_lines": plus, "removed_lines": minus});
    if !brief
        && plus + minus <= MAX_LINES
        && let Some(m) = v.as_object_mut()
    {
        m.insert("hunks".into(), json!(hunks));
    }
    v
}

/// How two renders' objects differ: each object added, changed or removed.
fn changes_json(old: &[Planned], new: &[Planned], brief: bool) -> Value {
    let index = |v: &[Planned]| -> BTreeMap<(ResKey, bool), usize> { v.iter().enumerate().map(|(i, p)| ((p.key.clone(), p.hook), i)).collect() };
    let (a, b) = (index(old), index(new));
    let (mut added, mut changed, mut removed) = (0u32, 0u32, 0u32);
    let mut out: Vec<(ResKey, Value)> = Vec::new();
    for (k, &j) in &b {
        let Some(n) = new.get(j) else { continue };
        match a.get(k).and_then(|&i| old.get(i)) {
            None => {
                added += 1;
                out.push((k.0.clone(), object_json(&k.0, k.1, "change", "added", None, Some(&n.object), brief)));
            }
            Some(o) if o.hash != n.hash => {
                changed += 1;
                out.push((k.0.clone(), object_json(&k.0, k.1, "change", "changed", Some(&o.object), Some(&n.object), brief)));
            }
            Some(_) => {}
        }
    }
    for (k, &i) in &a {
        let Some(o) = old.get(i) else { continue };
        if !b.contains_key(k) {
            removed += 1;
            out.push((k.0.clone(), object_json(&k.0, k.1, "change", "removed", Some(&o.object), None, brief)));
        }
    }
    out.sort_by(|x, y| x.0.cmp(&y.0));
    json!({"added": added, "changed": changed, "removed": removed, "objects": out.into_iter().map(|(_, v)| v).collect::<Vec<_>>()})
}

/// What a sync of the render would do against what runs.
fn steps_json(p: &Plan, brief: bool) -> Value {
    let n = |k: StepKind| p.steps.iter().filter(|s| s.kind == k).count();
    let steps: Vec<Value> = p
        .steps
        .iter()
        .map(|s| {
            let mut v = object_json(&s.key, s.kind == StepKind::Hook, "step", s.kind.as_str(), s.change.before.as_ref(), s.change.after.as_ref(), brief);
            if let Some(m) = v.as_object_mut() {
                m.insert("phase".into(), json!(s.phase.as_str()));
                m.insert("wave".into(), json!(s.wave));
                m.insert("why".into(), json!(s.why));
            }
            v
        })
        .collect();
    let held: Vec<Value> = p
        .held
        .iter()
        .map(|(k, m)| json!({"key": ops::key_json(k), "prune": if *m == PruneMode::Never { "false" } else { "confirm" }}))
        .collect();
    let shared: Vec<Value> = p.shared.iter().map(|(k, app)| json!({"key": ops::key_json(k), "app": app.as_ref()})).collect();
    json!({
        "create": n(StepKind::Create), "update": n(StepKind::Update), "prune": n(StepKind::Prune), "hooks": n(StepKind::Hook),
        "unchanged": p.unchanged, "unread": p.unread.len(), "held": held, "shared": shared, "steps": steps,
    })
}

/// What the render below a diff is.
#[derive(Clone, Copy)]
enum Below {
    /// Nothing: the diff is a root commit.
    Nothing,
    Key(u64),
    /// Its sources couldn't be read.
    Failed,
}

/// Each diff's entry for one app; `None` where the diff leaves the app as trunk and the diff
/// below have it.
type AppDiffs = Vec<Option<(bool, Value)>>;

async fn plan_app(cd: Arc<Cd>, st: Arc<Stack>, id: String, def: AppDef, brief: bool) -> Result<AppDiffs, String> {
    let cluster = cd.clusters.read().get(&def.destination.cluster).cloned();
    let (kube_version, api_versions) = match &cluster {
        Some(c) => (c.version.read().clone(), c.discovery.read().api_versions.clone()),
        None => (None, Vec::new()),
    };
    let res = {
        let (cd, st, def, kv, av) = (cd.clone(), st.clone(), def.clone(), kube_version.clone(), api_versions.clone());
        crate::rt::blocking(move || resolutions(&cd, &st, &def, kv.as_deref(), &av)).await.and_then(|r| r).map_err(|e| format!("its sources can't be read: {e:#}"))?
    };
    // Each distinct content rendered once; renders are kept by key across plans.
    let mut kept: HashMap<u64, Result<Arc<Kept>, String>> = HashMap::new();
    let all = std::iter::once(&res.trunk).chain(res.base.iter().filter_map(|b| b.as_ref().ok())).chain(res.steps.iter().filter_map(|s| s.as_ref().ok()));
    for r in all {
        if kept.contains_key(&r.key) {
            continue;
        }
        let got = match cd.plans.get(r.key) {
            Some(k) => Ok(k),
            None => {
                let (cd2, def2, r2, kv, av) = (cd.clone(), def.clone(), r.clone(), kube_version.clone(), api_versions.clone());
                match crate::rt::blocking(move || cd2.render_app(&def2, &r2, r2.key, kv.as_deref(), &av)).await.and_then(|x| x) {
                    Ok((objects, children)) => {
                        let k = Arc::new(Kept::new(objects, children));
                        cd.plans.put(r.key, k.clone());
                        Ok(k)
                    }
                    Err(e) => Err(format!("{e:#}")),
                }
            }
        };
        kept.insert(r.key, got);
    }
    let mut plans = engine_plans(&cd, &id, &kept, None, Arc::new(Read::default())).await;
    // Kinds the engine hasn't listed in the cluster (an app's first Secret, say): read the objects
    // from the cluster now, and plan those renders again with them.
    let unread: BTreeSet<ResKey> = plans.values().flat_map(|p| p.unread.iter().cloned()).collect();
    let mut unserved: BTreeSet<ResKey> = BTreeSet::new();
    if let Some(c) = cluster.as_ref().filter(|_| !unread.is_empty()) {
        let defined: BTreeSet<GroupKind> = kept.values().filter_map(|k| k.as_ref().ok()).flat_map(|k| crd_kinds(&k.objects())).collect();
        let mut read = Read::default();
        let mut gets = Vec::new();
        for key in unread.iter().take(MAX_READS) {
            let gk = key.group_kind();
            if cd.serves(c, &gk, std::time::Duration::ZERO).await {
                gets.push(key.clone());
            } else if defined.contains(&gk) {
                // Its CRD comes with the render: nothing of the kind is there yet.
                read.insert(key.clone(), None);
            } else {
                unserved.insert(key.clone());
            }
        }
        for batch in gets.chunks(8) {
            let got = futures::future::join_all(batch.iter().map(|k| c.get(k))).await;
            for (k, r) in batch.iter().zip(got) {
                if let Ok(v) = r {
                    read.insert(k.clone(), v);
                }
            }
        }
        if !read.is_empty() {
            let again: BTreeSet<u64> = plans.iter().filter(|(_, p)| !p.unread.is_empty()).map(|(k, _)| *k).collect();
            plans.extend(engine_plans(&cd, &id, &kept, Some(&again), Arc::new(read)).await);
        }
    }
    let mut shown: HashMap<u64, Shown> = HashMap::new();
    for (k, mut plan) in plans {
        let mut objects = std::mem::take(&mut plan.objects);
        if let Some(Ok(kp)) = kept.get(&k) {
            objects.extend(kp.children());
        }
        objects.sort_by(|x, y| x.key.cmp(&y.key).then(x.hook.cmp(&y.hook)));
        shown.insert(k, Shown { plan, objects });
    }
    let failure = |k: u64| -> String {
        match kept.get(&k) {
            Some(Err(e)) => format!("the render fails: {e}"),
            _ => "the app is gone".into(),
        }
    };
    let trunk = shown.get(&res.trunk.key);
    let auto = def.sync.auto;
    let shadow = super::is_shadow(&def);
    let head = json!({
        "app": id, "cluster": def.destination.cluster, "namespace": def.destination.namespace,
        "auto": auto, "auto_prune": auto && def.sync.prune, "shadow": shadow,
    });
    let entry = |ok: bool, touched: bool, error: Option<String>, problems: Vec<String>, notes: Vec<String>, own: Value, stack: Value, sync: Value| -> (bool, Value) {
        let mut v = head.clone();
        if let Some(m) = v.as_object_mut() {
            for (k, x) in [
                ("ok", json!(ok)),
                ("touched", json!(touched)),
                ("error", json!(error)),
                ("problems", json!(problems)),
                ("notes", json!(notes)),
                ("own", own),
                ("stack", stack),
                ("sync", sync),
            ] {
                m.insert(k.into(), x);
            }
        }
        (ok, v)
    };
    let mut below = match &res.base {
        None => Below::Nothing,
        Some(Ok(r)) => Below::Key(r.key),
        Some(Err(_)) => Below::Failed,
    };
    let mut out: AppDiffs = Vec::with_capacity(res.steps.len());
    for step in &res.steps {
        let item = match step {
            Err(e) => {
                let why = format!("its sources can't be read here: {e}");
                Some(entry(false, true, Some(why.clone()), vec![why], Vec::new(), Value::Null, Value::Null, Value::Null))
            }
            Ok(r) => {
                let touched = match below {
                    Below::Key(k) => k != r.key,
                    Below::Nothing | Below::Failed => true,
                };
                if !touched && r.key == res.trunk.key {
                    None
                } else {
                    match shown.get(&r.key) {
                        None => {
                            let why = failure(r.key);
                            Some(entry(false, touched, Some(why.clone()), vec![why], Vec::new(), Value::Null, Value::Null, Value::Null))
                        }
                        Some(cur) => {
                            let prev = match below {
                                Below::Key(k) => shown.get(&k),
                                _ => None,
                            };
                            let own = match below {
                                Below::Nothing => changes_json(&[], &cur.objects, brief),
                                _ => prev.map_or(Value::Null, |p| changes_json(&p.objects, &cur.objects, brief)),
                            };
                            let stack = trunk.map_or(Value::Null, |t| changes_json(&t.objects, &cur.objects, brief));
                            // What this diff leaves out of git that the cluster keeps.
                            let against = prev.or(trunk);
                            let (never, confirm) = (cur.never(), cur.confirm());
                            let (was_never, was_confirm) = against.map(|p| (p.never(), p.confirm())).unwrap_or_default();
                            let problems: Vec<String> = never
                                .difference(&was_never)
                                .map(|k| format!("{} leaves git but is marked Prune=false: no sync deletes it, so the app stays out of sync until someone does", named(k)))
                                .collect();
                            let mut notes: Vec<String> = Vec::new();
                            if matches!(below, Below::Failed) {
                                notes.push("the diff below can't be read, so this diff's own change isn't known".into());
                            }
                            notes.extend(confirm
                                .difference(&was_confirm)
                                .map(|k| format!("{} leaves git and is pruned only once a person confirms (Prune=confirm)", named(k))));
                            notes.extend(cur.plan.shared.iter().map(|(k, other)| format!("{} is {other}'s too: this sync takes it over", named(k))));
                            let (gone, unknown): (Vec<&ResKey>, Vec<&ResKey>) = cur.plan.unread.iter().partition(|k| unserved.contains(*k));
                            notes.extend(gone.iter().map(|k| {
                                format!("{} is of a kind cluster {} doesn't serve, and nothing in the render defines it: a sync of it fails until something installs it", named(k), def.destination.cluster)
                            }));
                            match unknown.len() {
                                0 => {}
                                1 => notes.push(format!("what a sync does to {} isn't known: it couldn't be read from the cluster", unknown.first().map(|k| named(k)).unwrap_or_default())),
                                n => notes.push(format!("what a sync does to {n} objects isn't known: they couldn't be read from the cluster")),
                            }
                            if cluster.is_none() {
                                notes.push(format!("cluster {} isn't connected: the steps are against what kiln last saw", def.destination.cluster));
                            }
                            let ok = problems.is_empty();
                            Some(entry(ok, touched, None, problems, notes, own, stack, steps_json(&cur.plan, brief)))
                        }
                    }
                }
            }
        };
        below = match step {
            Ok(r) => Below::Key(r.key),
            Err(_) => Below::Failed,
        };
        out.push(item);
    }
    Ok(out)
}

/// The engine's plans of an app's renders (those in `only`, or all).
async fn engine_plans(cd: &Cd, id: &str, kept: &HashMap<u64, Result<Arc<Kept>, String>>, only: Option<&BTreeSet<u64>>, read: Arc<Read>) -> HashMap<u64, Plan> {
    let jobs: Vec<(u64, Vec<Value>)> =
        kept.iter().filter(|(k, _)| only.is_none_or(|o| o.contains(k))).filter_map(|(k, v)| Some((*k, v.as_ref().ok()?.objects()))).collect();
    let app = id.to_owned();
    cd.query(move |e| jobs.into_iter().filter_map(|(k, objects)| Some((k, e.plan(&app, Some(objects), &read)?))).collect()).await.unwrap_or_default()
}

/// The kinds a render's CRDs define.
fn crd_kinds(objects: &[Value]) -> Vec<GroupKind> {
    objects
        .iter()
        .filter(|o| obj::kind(o) == "CustomResourceDefinition" && obj::group(o) == "apiextensions.k8s.io")
        .filter_map(|o| Some(GroupKind::new(obj::str_at(o, &["spec", "group"])?, obj::str_at(o, &["spec", "names", "kind"])?)))
        .collect()
}

/// Plans what `ask` names.
pub async fn plan(cd: &Arc<Cd>, ask: Ask) -> Result<Value, Fail> {
    let st = {
        let (cd, ask) = (cd.clone(), ask.clone());
        Arc::new(crate::rt::blocking(move || find(&cd, &ask)).await.map_err(failed)??)
    };
    // The apps the stack can move: those whose read paths it changes.
    let (mut affected, mut unaffected) = (Vec::new(), 0usize);
    {
        let apps = cd.apps.read();
        let statuses = cd.statuses.read();
        for (id, e) in apps.iter() {
            if !ops::selects(&ask.selector, e, statuses.get(id.as_str())) {
                continue;
            }
            let moves = st.moves(&e.def);
            if !moves.iter().any(|m| *m) {
                continue;
            }
            if st.changed.iter().any(|c| touches(&e.def, &e.deps, &moves, c.as_ref())) {
                affected.push((id.clone(), e.def.clone()));
            } else {
                unaffected += 1;
            }
        }
    }
    let total = affected.len();
    let unplanned: Vec<String> = affected.iter().skip(ask.max_apps).map(|(id, _)| id.clone()).collect();
    affected.truncate(ask.max_apps);
    let mut results = Vec::with_capacity(affected.len());
    for batch in affected.chunks(PARALLEL) {
        let runs = batch.iter().map(|(id, def)| plan_app(cd.clone(), st.clone(), id.clone(), def.clone(), ask.brief));
        results.extend(futures::future::join_all(runs).await);
    }
    let mut notes = st.notes.clone();
    for ((id, _), r) in affected.iter().zip(&results) {
        if let Err(e) = r {
            notes.push(format!("{id}: {e}"));
        }
    }
    let diffs: Vec<Value> = st
        .commits
        .iter()
        .enumerate()
        .map(|(i, _)| {
            let apps: Vec<&(bool, Value)> = results.iter().filter_map(|r| r.as_ref().ok()?.get(i)?.as_ref()).collect();
            json!({
                "commit": st.shown.get(i).cloned().unwrap_or(Value::Null),
                "ok": apps.iter().all(|(ok, _)| *ok),
                "apps": apps.iter().map(|(_, v)| v.clone()).collect::<Vec<_>>(),
            })
        })
        .collect();
    let ok = diffs.iter().all(|d| d.get("ok") == Some(&Value::Bool(true)));
    cd.hub.emit(&json!({
        "type": "cd_plan", "repo": st.urls.first(), "trunk": st.trunk, "base": st.base, "head": st.commits.last(),
        "commits": st.commits.len(), "apps": affected.len(), "ok": ok,
    }));
    Ok(json!({
        "repo": st.urls.first(),
        "trunk": {"branch": st.trunk, "commit": st.trunk_commit},
        "base": st.base,
        "ok": ok,
        "apps": {"affected": total, "planned": affected.len(), "unaffected": unaffected, "unplanned": unplanned},
        "notes": notes,
        "diffs": diffs,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_repository_has_many_spellings() {
        for (a, b) in [
            ("https://github.com/o/r", "https://github.com/o/r.git"),
            ("https://github.com/o/r/", "HTTPS://GitHub.com/o/r"),
            ("git@github.com:o/r.git", "https://github.com/o/r"),
            ("ssh://git@github.com/o/r", "https://github.com/o/r"),
            ("http://kiln-git.kiln.svc:8080/fixtures.git", "http://kiln-git.kiln.svc:8080/fixtures"),
        ] {
            assert!(same_repo(a, b), "{a} {b}");
        }
        assert!(!same_repo("https://github.com/o/r", "https://github.com/o/r2"));
        assert!(!same_repo("https://github.com/o/r", "https://gitlab.com/o/r"));
    }

    #[test]
    fn a_source_follows_trunk_by_name_or_by_head() {
        assert!(tracks("main", "main", Some("main")));
        assert!(tracks("refs/heads/main", "main", None));
        assert!(tracks("HEAD", "main", Some("main")));
        assert!(tracks("", "main", Some("main")));
        assert!(!tracks("HEAD", "release", Some("main")));
        assert!(!tracks("v1.2.0", "main", Some("main")));
    }

    #[test]
    fn a_change_touches_what_lies_under_a_path() {
        let changed: BTreeSet<String> = ["deploy/web/cm.yaml", "deploy/web-two/x.yaml", "README.md"].into_iter().map(String::from).collect();
        assert!(under(&changed, "deploy/web"));
        assert!(under(&changed, "deploy/web/cm.yaml"));
        assert!(under(&changed, "/deploy/"));
        assert!(under(&changed, ""));
        assert!(!under(&changed, "deploy/api"));
        assert!(!under(&changed, "deploy/we"));
        let only_sibling: BTreeSet<String> = ["deploy/web-two/x.yaml"].into_iter().map(String::from).collect();
        assert!(!under(&only_sibling, "deploy/web"));
    }
}
