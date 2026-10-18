//! Comparing two runs or legs (docs/ci.md W19-R03): what differs between them, and each step's
//! durations side by side.
//!
//! A side holds what a run used and saw: the commit, the actions it pinned, its variables, and
//! per leg its conclusion, runner, executor, machine, runner version, images, environment, cache
//! lookups and steps. [`differences`] lists what is categorically different (an action's SHA, an
//! image, a variable, a cache that hit once and missed once…); durations always differ a little,
//! so they are not differences but [`steps`], paired for reading.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

/// One run, as compared.
#[derive(Debug, Clone, Default)]
pub struct RunSide {
    pub sha: String,
    /// `uses` → the commit it was pinned to.
    pub actions: BTreeMap<String, String>,
    /// The `vars` context.
    pub vars: BTreeMap<String, String>,
    /// By leg key (`build`, `test (linux, 14)`).
    pub legs: BTreeMap<String, LegSide>,
}

/// One leg, as compared.
#[derive(Debug, Clone, Default)]
pub struct LegSide {
    pub conclusion: String,
    /// Seconds.
    pub duration: Option<f64>,
    pub runner: String,
    pub executor: String,
    /// What the runner reported about its machine: OS and architecture, CPU, kernel.
    pub machine: Option<String>,
    /// The runner's version.
    pub version: Option<String>,
    /// By role (`job`, `service db`): the image as named, and its content-addressed id.
    pub images: BTreeMap<String, (String, String)>,
    /// The job's environment as the runner saw it, secrets masked.
    pub env: BTreeMap<String, String>,
    /// By cache key prefix: `hit` or `miss` (a leg that looked up more than once: its last).
    pub cache: BTreeMap<String, String>,
    /// In the order they ran: name, seconds, conclusion.
    pub steps: Vec<(String, Option<f64>, String)>,
}

/// Something that differs: what kind, in which leg (none: the run's), what, and either side
/// (none: not there).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Difference {
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub leg: Option<String>,
    pub what: String,
    pub a: Option<String>,
    pub b: Option<String>,
}

/// A step in both legs, or one of them.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StepPair {
    pub name: String,
    pub a: Option<f64>,
    pub b: Option<f64>,
    pub a_conclusion: Option<String>,
    pub b_conclusion: Option<String>,
}

/// Every key either map has whose values differ (or that one lacks), as differences of `kind`.
fn maps(out: &mut Vec<Difference>, kind: &'static str, leg: Option<&str>, a: &BTreeMap<String, String>, b: &BTreeMap<String, String>) {
    let keys: BTreeSet<&String> = a.keys().chain(b.keys()).collect();
    for k in keys {
        let (x, y) = (a.get(k), b.get(k));
        if x != y {
            out.push(Difference { kind, leg: leg.map(str::to_owned), what: k.clone(), a: x.cloned(), b: y.cloned() });
        }
    }
}

fn one(out: &mut Vec<Difference>, kind: &'static str, leg: &str, a: Option<&str>, b: Option<&str>) {
    if a != b {
        out.push(Difference { kind, leg: Some(leg.to_owned()), what: kind.to_owned(), a: a.map(str::to_owned), b: b.map(str::to_owned) });
    }
}

/// Actions by name (`owner/repo[/path]`) → `ref sha`, so a moved ref or a new version is one
/// difference; a name a run uses at two refs keeps them apart.
fn actions(m: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    let name = |u: &str| u.split_once('@').map_or(u, |(n, _)| n).to_owned();
    let mut uses: BTreeMap<String, usize> = BTreeMap::new();
    for u in m.keys() {
        *uses.entry(name(u)).or_default() += 1;
    }
    m.iter()
        .map(|(u, sha)| {
            let n = name(u);
            if uses.get(&n) == Some(&1) { (n, format!("{} {sha}", u.split_once('@').map_or("", |(_, r)| r))) } else { (u.clone(), sha.clone()) }
        })
        .collect()
}

/// What differs between `a` and `b`, the run's first (actions, variables, legs one lacks), then
/// each leg's in key order.
pub fn differences(a: &RunSide, b: &RunSide) -> Vec<Difference> {
    let mut out = Vec::new();
    maps(&mut out, "action", None, &actions(&a.actions), &actions(&b.actions));
    maps(&mut out, "var", None, &a.vars, &b.vars);
    let keys: BTreeSet<&String> = a.legs.keys().chain(b.legs.keys()).collect();
    for k in keys {
        let (Some(x), Some(y)) = (a.legs.get(k), b.legs.get(k)) else {
            let has = |s: &RunSide| s.legs.get(k).map(|l| l.conclusion.clone());
            out.push(Difference { kind: "leg", leg: Some(k.clone()), what: "leg".into(), a: has(a), b: has(b) });
            continue;
        };
        out.extend(leg_differences(k, x, y));
    }
    out
}

/// What differs between two legs (`key` names them in the result).
pub fn leg_differences(key: &str, x: &LegSide, y: &LegSide) -> Vec<Difference> {
    let mut out = Vec::new();
    one(&mut out, "conclusion", key, Some(&x.conclusion), Some(&y.conclusion));
    one(&mut out, "runner", key, Some(&x.runner), Some(&y.runner));
    one(&mut out, "executor", key, Some(&x.executor), Some(&y.executor));
    // A side that didn't report its machine (an older runner) isn't a difference.
    if x.machine.is_some() && y.machine.is_some() {
        one(&mut out, "machine", key, x.machine.as_deref(), y.machine.as_deref());
        one(&mut out, "version", key, x.version.as_deref(), y.version.as_deref());
        let image = |s: &LegSide| s.images.iter().map(|(r, (name, id))| (r.clone(), format!("{name}@{id}"))).collect::<BTreeMap<_, _>>();
        maps(&mut out, "image", Some(key), &image(x), &image(y));
        maps(&mut out, "env", Some(key), &x.env, &y.env);
    }
    maps(&mut out, "cache", Some(key), &x.cache, &y.cache);
    out
}

/// A step's name without the refs of the actions it names (`Run actions/cache@v4` → `Run
/// actions/cache`), so a new version's step pairs with the old one's.
fn plain(name: &str) -> String {
    name.split(' ').map(|w| if w.contains('/') { w.split_once('@').map_or(w, |(n, _)| n) } else { w }).collect::<Vec<_>>().join(" ")
}

/// Two legs' steps side by side: paired by name (an action's ref aside), the nth of a name with
/// the nth, in `a`'s order with `b`'s others after.
pub fn steps(a: &LegSide, b: &LegSide) -> Vec<StepPair> {
    let mut used = vec![false; b.steps.len()];
    let mut out = Vec::new();
    for (name, secs, concl) in &a.steps {
        let free = |i: usize| used.get(i) == Some(&false);
        let found = b
            .steps
            .iter()
            .enumerate()
            .find(|(i, (n, ..))| n == name && free(*i))
            .or_else(|| b.steps.iter().enumerate().find(|(i, (n, ..))| plain(n) == plain(name) && free(*i)))
            .map(|(i, s)| (i, s.clone()));
        match found {
            Some((i, (_, other, oc))) => {
                if let Some(u) = used.get_mut(i) {
                    *u = true;
                }
                out.push(StepPair { name: name.clone(), a: *secs, b: other, a_conclusion: Some(concl.clone()), b_conclusion: Some(oc) });
            }
            None => out.push(StepPair { name: name.clone(), a: *secs, b: None, a_conclusion: Some(concl.clone()), b_conclusion: None }),
        }
    }
    for (i, (name, secs, concl)) in b.steps.iter().enumerate() {
        if used.get(i) == Some(&false) {
            out.push(StepPair { name: name.clone(), a: None, b: *secs, a_conclusion: None, b_conclusion: Some(concl.clone()) });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leg(cache: &str) -> LegSide {
        LegSide {
            conclusion: "success".into(),
            duration: Some(42.0),
            runner: "r1".into(),
            executor: "docker".into(),
            machine: Some("linux/x86_64 · EPYC · 6.8 · docker".into()),
            version: Some("0.1.0".into()),
            images: [("job".to_owned(), ("node:22".to_owned(), "sha256:aa".to_owned()))].into(),
            env: [("CI".to_owned(), "true".to_owned()), ("TOKEN".to_owned(), "***".to_owned())].into(),
            cache: [("Linux-npm".to_owned(), cache.to_owned())].into(),
            steps: vec![("Check out".into(), Some(1.0), "success".into()), ("Run npm ci".into(), Some(20.0), "success".into()), ("Run npm test".into(), Some(21.0), "success".into())],
        }
    }

    fn run(setup_node: &str, cache: &str) -> RunSide {
        RunSide {
            sha: "c1".into(),
            actions: [("actions/checkout@v4".to_owned(), "11bd".to_owned()), ("actions/setup-node@v4".to_owned(), setup_node.to_owned())].into(),
            vars: [("REGION".to_owned(), "eu".to_owned())].into(),
            legs: [("build".to_owned(), leg(cache)), ("lint".to_owned(), LegSide { cache: BTreeMap::new(), ..leg("hit") })].into(),
        }
    }

    /// W19-AC03: two runs differing in one action SHA and one cache miss show exactly those
    /// differences, however their durations differ.
    #[test]
    fn w19_ac03_compare() {
        let a = run("49933ea", "hit");
        let mut b = run("1d0ff46", "miss");
        if let Some(l) = b.legs.get_mut("build") {
            l.duration = Some(95.0);
            l.steps = vec![("Check out".into(), Some(1.2), "success".into()), ("Run npm ci".into(), Some(70.0), "success".into()), ("Run npm test".into(), Some(23.8), "success".into())];
        }
        let d = differences(&a, &b);
        assert_eq!(
            d,
            vec![
                Difference { kind: "action", leg: None, what: "actions/setup-node".into(), a: Some("v4 49933ea".into()), b: Some("v4 1d0ff46".into()) },
                Difference { kind: "cache", leg: Some("build".into()), what: "Linux-npm".into(), a: Some("hit".into()), b: Some("miss".into()) },
            ]
        );
        assert!(differences(&a, &a).is_empty(), "a run is like itself");
        // A new version of an action is one difference, not one gone and one new.
        let mut c = run("49933ea", "hit");
        c.actions.remove("actions/setup-node@v4");
        c.actions.insert("actions/setup-node@v5".into(), "a0853c2".into());
        assert_eq!(differences(&a, &c).iter().map(|d| (d.what.as_str(), d.b.as_deref())).collect::<Vec<_>>(), vec![("actions/setup-node", Some("v5 a0853c2"))]);
        // The steps, side by side: the slower `npm ci` is where the miss shows.
        let (x, y) = (a.legs.get("build").cloned().unwrap_or_default(), b.legs.get("build").cloned().unwrap_or_default());
        let s = steps(&x, &y);
        assert_eq!(s.len(), 3);
        assert_eq!(s.get(1).map(|p| (p.name.as_str(), p.a, p.b)), Some(("Run npm ci", Some(20.0), Some(70.0))));
    }

    #[test]
    fn w19_r03_more() {
        let a = run("x", "hit");
        let mut b = run("x", "hit");
        b.legs.remove("lint");
        if let Some(l) = b.legs.get_mut("build") {
            l.images.insert("job".into(), ("node:22".into(), "sha256:bb".into()));
            l.env.insert("NODE_ENV".into(), "test".into());
            l.machine = Some("linux/aarch64 · Neoverse · 6.8 · docker".into());
            l.steps.insert(1, ("Run setup".into(), Some(3.0), "success".into()));
        }
        assert_eq!(differences(&a, &b).iter().map(|d| d.kind).collect::<Vec<_>>(), vec!["machine", "image", "env", "leg"]);
        // A runner that didn't report its machine leaves those out, but not the cache.
        let mut c = run("x", "miss");
        if let Some(l) = c.legs.get_mut("build") {
            l.machine = None;
            l.env.clear();
        }
        assert_eq!(differences(&a, &c).iter().map(|d| d.kind).collect::<Vec<_>>(), vec!["cache"]);
        // A new version of an action's step pairs with the old one's.
        let mut v = a.legs.get("build").cloned().unwrap_or_default();
        if let Some(s) = v.steps.first_mut() {
            s.0 = "Run actions/checkout@v5".into();
        }
        let mut u = a.legs.get("build").cloned().unwrap_or_default();
        if let Some(s) = u.steps.first_mut() {
            s.0 = "Run actions/checkout@v4".into();
        }
        assert_eq!(steps(&u, &v).len(), 3, "{:?}", steps(&u, &v));
        // Steps one leg lacks are paired with nothing.
        let s = steps(a.legs.get("build").unwrap_or(&LegSide::default()), b.legs.get("build").unwrap_or(&LegSide::default()));
        assert_eq!(s.iter().map(|p| (p.name.as_str(), p.a.is_some(), p.b.is_some())).collect::<Vec<_>>(), vec![("Check out", true, true), ("Run npm ci", true, true), ("Run npm test", true, true), ("Run setup", false, true)]);
    }
}
