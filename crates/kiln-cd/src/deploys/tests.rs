//! docs/ci.md W28-R38 and R39: the plan's three deploys, each wait naming what it shares, and the
//! queue run to the end over random deploys, as the release worker runs it.

use std::collections::{BTreeMap, BTreeSet};

use super::*;

fn deploy(id: &str, target: &str, resources: &[Resource], phase: Phase) -> Deploy {
    Deploy { id: id.into(), target: target.into(), resources: resources.iter().cloned().collect(), phase }
}

fn app(name: &str) -> Resource {
    Resource::App { name: name.into() }
}

fn place(cluster: &str, namespace: &str) -> Resource {
    Resource::Place { cluster: cluster.into(), namespace: namespace.into() }
}

fn config(cluster: &str, object: &str) -> Resource {
    Resource::Config { cluster: cluster.into(), object: object.into() }
}

fn verdict<'a>(g: &'a Deploys, id: &str) -> Result<&'a Verdict, String> {
    g.verdict(id).ok_or(format!("no verdict for {id}"))
}

/// The plan's test: app a in ns x, app b in ns y, app a again. a and b roll out together; the
/// second a supersedes the first.
#[test]
fn w28_r38_a_and_b_roll_out_together_and_the_second_a_supersedes_the_first() -> Result<(), String> {
    let a1 = || deploy("release 1", "a in prod", &[app("a"), place("prod", "x")], Phase::Ready);
    let b = || deploy("release 2", "b in prod", &[app("b"), place("prod", "y")], Phase::Ready);
    let a2 = |phase| deploy("release 3", "a in prod", &[app("a"), place("prod", "x")], phase);

    // All three asked for: a's newest takes the first's place before it starts, and b goes beside.
    let g = Deploys::new(&[a1(), b(), a2(Phase::Ready)]);
    assert_eq!(g.superseded_by("release 1").map(|d| d.id.as_str()), Some("release 3"), "a's first gives way to its newest");
    assert!(g.superseded_by("release 2").is_none() && g.superseded_by("release 3").is_none());
    assert!(verdict(&g, "release 2")?.at_once(), "b shares nothing with a");
    assert_eq!(verdict(&g, "release 2")?.why(), ["deploys beside release 1: nothing shared"]);
    let v = verdict(&g, "release 3")?;
    assert!(v.at_once());
    assert_eq!(v.supersedes(), ["release 1"]);
    assert_eq!(v.why(), ["supersedes release 1: the newest revision of a in prod rolls out", "deploys beside release 2: nothing shared"]);

    // a's first and b already rolling out together: the second a goes beside b, and the first
    // gives way once the second's write is done and it rolls out.
    let a1 = || deploy("release 1", "a in prod", &[app("a"), place("prod", "x")], Phase::Rolling);
    let b = || deploy("release 2", "b in prod", &[app("b"), place("prod", "y")], Phase::Rolling);
    let g = Deploys::new(&[a1(), b(), a2(Phase::Ready)]);
    assert!(verdict(&g, "release 2")?.at_once(), "a and b roll out together");
    assert!(verdict(&g, "release 3")?.at_once());
    assert_eq!(verdict(&g, "release 3")?.supersedes(), ["release 1"]);
    assert!(g.superseded_by("release 1").is_none(), "nothing newer has rolled out yet");
    let g = Deploys::new(&[a1(), b(), a2(Phase::Writing)]);
    assert!(g.superseded_by("release 1").is_none(), "the second's write isn't done");
    let g = Deploys::new(&[a1(), b(), a2(Phase::Rolling)]);
    assert_eq!(g.superseded_by("release 1").map(|d| d.id.as_str()), Some("release 3"), "only the newest a rolls out");
    assert!(g.superseded_by("release 2").is_none() && verdict(&g, "release 2")?.at_once(), "b rolls on beside it");
    Ok(())
}

#[test]
fn w28_r39_every_wait_names_what_it_shares() -> Result<(), String> {
    let a = deploy("release 1", "a in prod", &[app("a"), place("prod", "x"), config("prod", "ConfigMap shop/settings")], Phase::Rolling);
    let b = deploy("release 2", "b in prod", &[app("b"), place("prod", "x")], Phase::Ready);
    let c = deploy("release 3", "c in prod", &[app("c"), place("prod", "z"), config("prod", "ConfigMap shop/settings")], Phase::Ready);
    let s = deploy("release 4", "a in staging", &[app("a"), place("dev", "x")], Phase::Ready);
    let g = Deploys::new(&[a, b, c, s]);
    assert_eq!(verdict(&g, "release 2")?.waits(), ["waits for release 1: both deploy to prod's x namespace"]);
    assert_eq!(verdict(&g, "release 3")?.waits(), ["waits for release 1: both deploy ConfigMap shop/settings on prod"]);
    // One app in another environment: another target, so it waits for the first rather than
    // superseding it.
    let v = verdict(&g, "release 4")?;
    assert_eq!(v.waits(), ["waits for release 1: both deploy app a"]);
    assert!(v.supersedes().is_empty());
    // Every wait of every deploy names what the two share.
    for v in &g.verdicts {
        for d in &v.decisions {
            if let Decision::Waits { shares, .. } = d {
                assert!(!shares.is_empty(), "{} {d}", v.deploy);
            }
        }
    }
    Ok(())
}

#[test]
fn w28_r38_ready_deploys_take_turns_oldest_first_and_held_ones_hold_nothing() -> Result<(), String> {
    // b ready ahead of c in one namespace: c waits for it, though neither has started.
    let b = |phase| deploy("release 1", "b in prod", &[app("b"), place("prod", "x")], phase);
    let c = |phase| deploy("release 2", "c in prod", &[app("c"), place("prod", "x")], phase);
    let g = Deploys::new(&[b(Phase::Ready), c(Phase::Ready)]);
    assert!(verdict(&g, "release 1")?.at_once());
    assert_eq!(verdict(&g, "release 2")?.waits_for(), ["release 1"]);
    // b waiting for an approval holds nothing: c goes before it, saying so.
    let g = Deploys::new(&[b(Phase::Held), c(Phase::Ready)]);
    let v = verdict(&g, "release 2")?;
    assert!(v.at_once());
    assert_eq!(v.why(), ["goes before release 1, which its gates hold: both deploy to prod's x namespace"]);
    // c under way holds the namespace for b, though b is older.
    let g = Deploys::new(&[b(Phase::Ready), c(Phase::Rolling)]);
    assert_eq!(verdict(&g, "release 1")?.waits(), ["waits for release 2: both deploy to prod's x namespace"]);
    Ok(())
}

#[test]
fn w28_r38_a_write_is_never_cut_short() -> Result<(), String> {
    let a1 = deploy("release 1", "a in prod", &[app("a"), place("prod", "x")], Phase::Writing);
    let a2 = deploy("release 2", "a in prod", &[app("a"), place("prod", "x")], Phase::Ready);
    let g = Deploys::new(&[a1, a2]);
    assert!(g.superseded_by("release 1").is_none());
    let v = verdict(&g, "release 2")?;
    assert_eq!(v.waits(), ["waits for release 1: both deploy to prod's x namespace; both deploy app a"]);
    Ok(())
}

#[test]
fn w28_r38_a_backlog_of_one_target_comes_down_to_the_newest() -> Result<(), String> {
    let a = |id: &str, phase| deploy(id, "a in prod", &[app("a"), place("prod", "x")], phase);
    let g = Deploys::new(&[a("release 1", Phase::Ready), a("release 2", Phase::Held), a("release 3", Phase::Ready)]);
    assert_eq!(g.superseded_by("release 1").map(|d| d.id.as_str()), Some("release 3"));
    assert_eq!(g.superseded_by("release 2").map(|d| d.id.as_str()), Some("release 3"));
    assert!(g.superseded_by("release 3").is_none());
    assert_eq!(verdict(&g, "release 3")?.supersedes(), ["release 1", "release 2"]);
    // A newer one its gates still hold doesn't take an older one's place: the older may go.
    let g = Deploys::new(&[a("release 1", Phase::Ready), a("release 2", Phase::Held)]);
    assert!(g.superseded_by("release 1").is_none());
    assert!(verdict(&g, "release 1")?.at_once());
    Ok(())
}

/// splitmix64: random queues from a seed, the same each run.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }

    fn pick<'a>(&mut self, of: &[&'a str]) -> &'a str {
        of.get(self.below(of.len() as u64) as usize).copied().unwrap_or_default()
    }
}

/// A random queue: up to 12 deploys of 4 apps in 2 environments, apps sharing namespaces and
/// configs at random, each held by its gates or ready.
fn random_queue(rng: &mut Rng) -> Vec<Deploy> {
    let namespaces: BTreeMap<&str, &str> = ["a", "b", "c", "d"].into_iter().map(|a| (a, if rng.below(3) == 0 { "shared" } else { a })).collect();
    let configs: BTreeMap<&str, Option<&str>> = ["a", "b", "c", "d"].into_iter().map(|a| (a, (rng.below(4) == 0).then_some("ConfigMap shop/settings"))).collect();
    (1..=1 + rng.below(12))
        .map(|n| {
            let a = rng.pick(&["a", "b", "c", "d"]);
            let (env, cluster) = if rng.below(2) == 0 { ("staging", "dev") } else { ("prod", "prod") };
            let mut resources = vec![app(a), place(cluster, namespaces.get(a).copied().unwrap_or(a))];
            resources.extend(configs.get(a).copied().flatten().map(|o| config(cluster, o)));
            deploy(&format!("release {n}"), &format!("{a} in {env}"), &resources, if rng.below(3) == 0 { Phase::Held } else { Phase::Ready })
        })
        .collect()
}

/// What the queue's run came to.
#[derive(Default)]
struct Run {
    /// Each target's writes, by release number, in the order they started.
    writes: BTreeMap<String, Vec<u64>>,
    /// The deploys that rolled out to the end, and those superseded.
    finished: BTreeSet<String>,
    superseded: BTreeSet<String>,
    /// The step each deploy first started writing at.
    started: BTreeMap<String, u32>,
}

fn number(id: &str) -> u64 {
    id.rsplit(' ').next().and_then(|n| n.parse().ok()).unwrap_or(0)
}

/// Runs `queue` as the release worker does, oldest first each turn: a deploy superseded ends; one
/// waiting goes when its verdict lets it and its gates do (a held one's let it go one turn in
/// three), ending the waiting deploys of its target it supersedes; a write is done the turn after
/// it starts, ending the deploys of its target rolling out before it; a rollout ends two turns
/// later. After every move, no two deploys under way of different targets share a resource.
fn run(mut live: Vec<Deploy>, rng: &mut Rng) -> Result<Run, String> {
    let mut out = Run::default();
    let mut rolled: BTreeMap<String, u32> = BTreeMap::new();
    for turn in 0..400u32 {
        if live.is_empty() {
            return Ok(out);
        }
        let ids: Vec<String> = live.iter().map(|d| d.id.clone()).collect();
        for id in ids {
            let g = Deploys::new(&live);
            let Some(me) = g.deploy(&id).cloned() else { continue };
            let by = g.superseded_by(&id).map(|d| d.id.clone());
            let gone = |live: &mut Vec<Deploy>, id: &str| live.retain(|d| d.id != id);
            if let Some(by) = by {
                if me.phase == Phase::Writing {
                    return Err(format!("{id} was superseded by {by} mid-write"));
                }
                out.superseded.insert(id.clone());
                gone(&mut live, &id);
                continue;
            }
            match me.phase {
                Phase::Held | Phase::Ready => {
                    let v = verdict(&g, &id)?;
                    if !v.at_once() || (me.phase == Phase::Held && rng.below(3) != 0) {
                        continue;
                    }
                    for older in v.supersedes() {
                        if g.deploy(older).is_some_and(|d| !d.phase.going()) {
                            out.superseded.insert(older.to_owned());
                            gone(&mut live, older);
                        }
                    }
                    let n = number(&id);
                    if out.writes.get(&me.target).and_then(|w| w.last()).is_some_and(|last| *last > n) {
                        return Err(format!("{id} started writing after a newer deploy of {}", me.target));
                    }
                    if let Some(newer) = live.iter().find(|d| d.target == me.target && number(&d.id) > n && d.phase != Phase::Held) {
                        return Err(format!("{id} started writing while {}, newer, was {:?}: only the newest of a target rolls out", newer.id, newer.phase));
                    }
                    out.writes.entry(me.target.clone()).or_default().push(n);
                    out.started.entry(id.clone()).or_insert(turn);
                    set(&mut live, &id, Phase::Writing);
                }
                Phase::Writing => {
                    set(&mut live, &id, Phase::Rolling);
                    rolled.insert(id.clone(), turn);
                    let older: Vec<String> = live.iter().take_while(|d| d.id != id).filter(|d| d.target == me.target && d.phase == Phase::Rolling).map(|d| d.id.clone()).collect();
                    for o in older {
                        out.superseded.insert(o.clone());
                        gone(&mut live, &o);
                    }
                }
                Phase::Rolling => {
                    if rolled.get(&id).is_some_and(|at| turn >= at + 2) {
                        out.finished.insert(id.clone());
                        gone(&mut live, &id);
                    }
                }
            }
            safe(&live)?;
        }
    }
    Err(format!("the queue didn't drain in 400 turns: {:?}", live.iter().map(|d| (&d.id, d.phase)).collect::<Vec<_>>()))
}

fn set(live: &mut [Deploy], id: &str, phase: Phase) {
    if let Some(d) = live.iter_mut().find(|d| d.id == id) {
        d.phase = phase;
    }
}

/// No two deploys under way of different targets share a resource; a target has one write at a
/// time.
fn safe(live: &[Deploy]) -> Result<(), String> {
    let going: Vec<&Deploy> = live.iter().filter(|d| d.phase.going()).collect();
    for (i, a) in going.iter().enumerate() {
        for b in going.iter().skip(i + 1) {
            if a.target != b.target && !a.resources.is_disjoint(&b.resources) {
                return Err(format!("{} and {} are under way together, sharing {:?}", a.id, b.id, a.resources.intersection(&b.resources).collect::<Vec<_>>()));
            }
            if a.target == b.target && a.phase == Phase::Writing && b.phase == Phase::Writing {
                return Err(format!("{} and {} write {} at once", a.id, b.id, a.target));
            }
        }
    }
    Ok(())
}

/// Over 10,000 random queues run to the end: deploys under way never share a resource across
/// targets; a target's writes start oldest first, none while a newer deploy of the target is ready
/// or under way, and none is cut short; of each target the newest deploy always rolls out to the
/// end, the rest finishing before it or superseded; a deploy that shares nothing with any other
/// starts at once; and every wait names what it shares.
#[test]
fn w28_r38_r39_random_queues_run_to_the_end() -> Result<(), String> {
    let mut rng = Rng(0x5EED_0028_0038_0039);
    for case in 0..10_000 {
        let queue = random_queue(&mut rng);
        let g = Deploys::new(&queue);
        for v in &g.verdicts {
            let me = g.deploy(&v.deploy).ok_or("a verdict of no deploy")?;
            for d in &v.decisions {
                if let Decision::Waits { other, shares } = d {
                    let them = g.deploy(other).ok_or("a wait for no deploy")?;
                    let both: Vec<Resource> = me.resources.intersection(&them.resources).cloned().collect();
                    if shares.is_empty() || *shares != both {
                        return Err(format!("case {case}: {} {d} names {shares:?}, they share {both:?}", v.deploy));
                    }
                }
            }
        }
        // Deploys ready and sharing nothing with another (nor a target) go on their first turn.
        let alone: Vec<String> = queue
            .iter()
            .filter(|d| d.phase == Phase::Ready && queue.iter().all(|o| o.id == d.id || (o.target != d.target && o.resources.is_disjoint(&d.resources))))
            .map(|d| d.id.clone())
            .collect();
        let newest: BTreeMap<String, String> = queue.iter().map(|d| (d.target.clone(), d.id.clone())).collect();
        let r = run(queue.clone(), &mut rng).map_err(|e| format!("case {case}: {e}"))?;
        for id in &alone {
            if r.started.get(id) != Some(&0) {
                return Err(format!("case {case}: {id} shares nothing and didn't start at once"));
            }
        }
        for (target, id) in &newest {
            if !r.finished.contains(id) {
                return Err(format!("case {case}: {target}'s newest, {id}, didn't roll out"));
            }
        }
        for d in &queue {
            if r.finished.contains(&d.id) == r.superseded.contains(&d.id) {
                return Err(format!("case {case}: {} didn't end exactly once", d.id));
            }
        }
        // Superseded deploys of a target were all older than one that rolled out.
        for id in &r.superseded {
            let target = queue.iter().find(|d| &d.id == id).map(|d| d.target.as_str()).unwrap_or_default();
            if !queue.iter().any(|d| d.target == target && number(&d.id) > number(id) && r.finished.contains(&d.id)) {
                return Err(format!("case {case}: {id} was superseded by nothing that rolled out"));
            }
        }
    }
    Ok(())
}
