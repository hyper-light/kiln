//! W24-R31's reaper over the machine's own projects (Ada, 2026-11-11: "The reaper needs to keep
//! things cleaned up"): the build output in the folders its agent sessions work in, and in
//! `~/.cache`. It recognizes build output only by its `CACHEDIR.TAG` (on 2026-11-14 a reaper that
//! took every folder of `~/.cache` that wasn't a repository for a cache deleted agents' patches,
//! research and scripts on adacachy), and it deletes on measurement alone, with no size, share,
//! age or schedule set for it ([`need`], [`plan`]): only when, at the fastest rate it has measured
//! the disk filling, the disk would fill before its next pass could finish. An agent at work in a
//! project cleans what the reaper doesn't recognize ([`ask`]).

use std::collections::BTreeMap;

use super::Entry;

/// Something the reaper recognized as build output, as the runner found it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub path: String,
    /// The project it's in (or `~/.cache` for what's there).
    pub group: String,
    pub bytes: u64,
    /// When it was last written (unix seconds).
    pub used: i64,
    /// A process names it now: it isn't the reaper's to take.
    pub busy: bool,
}

/// What a reap may evict, and what the reaper recognized in all (busy folders included).
pub fn entries(found: &[Found]) -> (Vec<Entry>, u64) {
    let held = found.iter().map(|f| f.bytes).fold(0u64, u64::saturating_add);
    // How long each saves isn't known: least recently used goes first.
    let entries = found.iter().filter(|f| !f.busy).map(|f| Entry { id: f.path.clone(), group: f.group.clone(), bytes: f.bytes, used: f.used, saved_ms: 0 }).collect();
    (entries, held)
}

/// W24-R31: a rate the disk filled at, as measured between two of the reaper's looks: the bytes
/// of free space lost over the seconds between them. Never one with no time between its looks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rate {
    bytes: u64,
    secs: u64,
}

impl Rate {
    /// `bytes` lost in `secs` seconds; none over no time.
    pub fn new(bytes: u64, secs: u64) -> Option<Rate> {
        (secs > 0).then_some(Rate { bytes, secs })
    }

    /// Faster than `other` (compared by cross-multiplying, so nothing is rounded).
    pub fn faster_than(self, other: Rate) -> bool {
        u128::from(self.bytes) * u128::from(other.secs) > u128::from(other.bytes) * u128::from(self.secs)
    }

    /// What it writes in `secs` seconds, rounded up.
    pub fn over(self, secs: u64) -> u64 {
        let written = u128::from(self.bytes) * u128::from(secs);
        let whole = written.checked_div(u128::from(self.secs)).unwrap_or(0);
        let rounded = whole + u128::from(written.checked_rem(u128::from(self.secs)).is_some_and(|r| r > 0));
        u64::try_from(rounded).unwrap_or(u64::MAX)
    }

    /// Bytes a second, rounded down: for people.
    pub fn per_second(self) -> u64 {
        self.bytes.checked_div(self.secs).unwrap_or(0)
    }
}

/// W24-R31: the fastest the disk has filled between two of the reaper's looks, after one more:
/// `previous` (unix seconds, bytes free then) and `now`'s. A look that finds more free (something
/// was freed) or no time passed measures no fill, and the fastest so far stands.
pub fn fastest(fastest: Option<Rate>, previous: Option<(i64, u64)>, now: (i64, u64)) -> Option<Rate> {
    let Some((then, was)) = previous else { return fastest };
    let secs = u64::try_from(now.0.saturating_sub(then)).unwrap_or(0);
    let Some(rate) = was.checked_sub(now.1).filter(|lost| *lost > 0).and_then(|lost| Rate::new(lost, secs)) else { return fastest };
    match fastest {
        Some(f) if !rate.faster_than(f) => Some(f),
        _ => Some(rate),
    }
}

/// W24-R31: what the reaper must free now, measured, never set. At `fastest` (the fastest the disk
/// has filled between two of its looks), the disk would fill within `horizon` seconds (the interval
/// between its last two looks and how long its last pass took) when fewer bytes are free than that
/// rate writes in that time: it must free the difference, and only that. With no rate measured yet
/// there's nothing to decide on, and it frees nothing.
pub fn need(free: u64, fastest: Option<Rate>, horizon: u64) -> u64 {
    fastest.map_or(0, |rate| rate.over(horizon).saturating_sub(free))
}

/// W24-R31: what goes to free `need` bytes: least recently used first (a busy folder is never an
/// entry), and only until what goes covers it. Nothing when nothing is needed.
pub fn plan(entries: &[Entry], need: u64) -> Vec<Entry> {
    let mut order: Vec<&Entry> = entries.iter().collect();
    order.sort_by(|a, b| a.used.cmp(&b.used).then(a.id.cmp(&b.id)));
    let mut covered = 0u64;
    let mut out = Vec::new();
    for e in order {
        if covered >= need {
            break;
        }
        covered = covered.saturating_add(e.bytes);
        out.push(e.clone());
    }
    out
}

/// An agent session at work in a project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Agent {
    pub session: String,
    /// Between turns, so a message starts one.
    pub idle: bool,
}

/// A project its agent sessions work in, and what it holds the reaper doesn't recognize.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    pub root: String,
    pub bytes: u64,
    pub agents: Vec<Agent>,
}

/// The agent the reaper asks to clean its project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ask {
    pub session: String,
    pub root: String,
    pub bytes: u64,
}

/// Whom the reaper asks to clean, once its own deletions leave `need` bytes still to free ([`need`]
/// again, on what's free then): none while nothing is, or while an ask of its is still with an agent
/// (`waiting`); else an idle agent of the project holding the most the reaper doesn't recognize.
/// One ask at a time, so each is answered before the next is chosen on what's free then.
///
/// `answered`: what each project held that the reaper doesn't recognize when its agent last
/// answered an ask (by its root). A project is asked again only once it holds more than that: an
/// answer that its sources have nothing to free isn't asked for again and again (W24-R31).
pub fn ask(need: u64, waiting: bool, projects: &[Project], answered: &BTreeMap<String, u64>) -> Option<Ask> {
    if need == 0 || waiting {
        return None;
    }
    let mut order: Vec<&Project> = projects.iter().filter(|p| p.bytes > 0 && answered.get(&p.root).is_none_or(|held| p.bytes > *held)).collect();
    order.sort_by(|a, b| b.bytes.cmp(&a.bytes).then(a.root.cmp(&b.root)));
    order.into_iter().find_map(|p| {
        let mut idle: Vec<&Agent> = p.agents.iter().filter(|a| a.idle).collect();
        idle.sort_by(|a, b| a.session.cmp(&b.session));
        idle.first().map(|a| Ask { session: a.session.clone(), root: p.root.clone(), bytes: p.bytes })
    })
}

/// What the reaper says to the agent it asks: `free` bytes free, `need` still to free, the disk
/// filling at `rate` would fill within `horizon` seconds.
pub fn ask_text(machine: &str, free: u64, need: u64, rate: Rate, horizon: u64, a: &Ask) -> String {
    format!(
        "kiln's reaper: {machine}'s disk has {} free. At the fastest it has measured the disk filling ({}/s), it would fill within {} (W24-R31). It deleted the build output it recognizes and still needs {} freed; your project {} holds {} it doesn't recognize. Delete what in it can be made again (build output, caches, downloaded dependencies, logs you're done with), never sources and never work that isn't committed, then say what you freed.",
        human(free),
        human(rate.per_second()),
        super::duration_text(horizon),
        human(need),
        a.root,
        human(a.bytes)
    )
}

/// Bytes as people read them: `412.5 GiB`.
pub fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = bytes as f64;
    let mut unit = 0usize;
    while v >= 1024.0 && unit + 1 < UNITS.len() {
        v /= 1024.0;
        unit += 1;
    }
    let name = UNITS.get(unit).copied().unwrap_or("B");
    if unit == 0 { format!("{bytes} B") } else { format!("{v:.1} {name}") }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;

    fn project(root: &str, bytes: u64, agents: &[(&str, bool)]) -> Project {
        Project { root: root.into(), bytes, agents: agents.iter().map(|(s, idle)| Agent { session: (*s).into(), idle: *idle }).collect() }
    }

    fn entry(id: &str, bytes: u64, used: i64) -> Entry {
        Entry { id: id.into(), group: "g".into(), bytes, used, saved_ms: 0 }
    }

    #[test]
    fn w24_r31_busy_build_output_is_held_but_never_an_entry() {
        let found = [
            Found { path: "/p/a/target".into(), group: "/p/a".into(), bytes: 10, used: 1, busy: false },
            Found { path: "/p/b/target".into(), group: "/p/b".into(), bytes: 20, used: 2, busy: true },
        ];
        let (entries, held) = entries(&found);
        assert_eq!(held, 30);
        assert_eq!(entries.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), ["/p/a/target"]);
    }

    #[test]
    fn w24_r31_no_rate_over_no_time_and_none_from_a_rise() {
        assert_eq!(Rate::new(GIB, 0), None);
        // The first look has nothing before it to measure against.
        assert_eq!(fastest(None, None, (100, 50 * GIB)), None);
        // Free space grew (something was freed): no fill measured, the fastest so far stands.
        let one = Rate::new(GIB, 60);
        assert_eq!(fastest(one, Some((100, 40 * GIB)), (160, 50 * GIB)), one);
        // Two looks in the same second measure nothing.
        assert_eq!(fastest(None, Some((100, 50 * GIB)), (100, 40 * GIB)), None);
    }

    #[test]
    fn w24_r31_the_fastest_fill_measured_stands_until_a_faster_one() {
        let slow = fastest(None, Some((0, 100 * GIB)), (60, 99 * GIB));
        assert_eq!(slow, Rate::new(GIB, 60));
        // Slower than that: it stands.
        assert_eq!(fastest(slow, Some((60, 99 * GIB)), (180, 98 * GIB)), slow);
        // Faster: it's the fastest now.
        assert_eq!(fastest(slow, Some((180, 98 * GIB)), (210, 96 * GIB)), Rate::new(2 * GIB, 30));
    }

    #[test]
    fn w24_r31_need_is_what_the_fastest_fill_writes_within_the_horizon_beyond_what_is_free() {
        // Nothing measured yet: nothing to decide on, however little is free.
        assert_eq!(need(0, None, 120), 0);
        let rate = Rate::new(GIB, 10);
        // 1 GiB per 10 s over a 60 s horizon writes 6 GiB: with 6 GiB or more free it wouldn't fill.
        assert_eq!(need(6 * GIB, rate, 60), 0);
        assert_eq!(need(100 * GIB, rate, 60), 0);
        // With 4 GiB free it would: the difference, and only that, is needed.
        assert_eq!(need(4 * GIB, rate, 60), 2 * GIB);
        // What it writes is rounded up, never down.
        assert_eq!(Rate::new(10, 3).map(|r| r.over(1)), Some(4));
        assert_eq!(Rate::new(u64::MAX, 1).map(|r| r.over(u64::MAX)), Some(u64::MAX));
    }

    #[test]
    fn w24_r31_plan_takes_least_recently_used_first_and_only_until_the_need_is_covered() {
        let entries = [entry("/c", 5, 30), entry("/a", 5, 10), entry("/b", 5, 20)];
        assert!(plan(&entries, 0).is_empty(), "nothing needed, nothing goes");
        assert_eq!(plan(&entries, 6).iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), ["/a", "/b"]);
        assert_eq!(plan(&entries, 5).iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), ["/a"]);
        // More needed than there is: everything goes, and no more.
        assert_eq!(plan(&entries, 100).len(), 3);
    }

    #[test]
    fn w24_r31_asks_the_largest_projects_idle_agent_only_while_still_needed() {
        let none = BTreeMap::new();
        let projects = [project("/p/small", 5 * GIB, &[("s1", true)]), project("/p/big", 50 * GIB, &[("s3", false), ("s2", true)])];
        assert_eq!(ask(0, false, &projects, &none), None);
        assert_eq!(ask(1, false, &projects, &none), Some(Ask { session: "s2".into(), root: "/p/big".into(), bytes: 50 * GIB }));
    }

    #[test]
    fn w24_r31_one_ask_at_a_time_and_none_to_a_busy_agent() {
        let none = BTreeMap::new();
        let projects = [project("/p/big", 50 * GIB, &[("s3", false)]), project("/p/small", 5 * GIB, &[("s1", true)])];
        assert_eq!(ask(GIB, true, &projects, &none), None);
        // The biggest project's agent is at work: the next project's idle one is asked.
        assert_eq!(ask(GIB, false, &projects, &none).map(|a| a.session), Some("s1".into()));
        assert_eq!(ask(GIB, false, &[project("/p/a", 0, &[("s1", true)])], &none), None);
    }

    #[test]
    fn w24_r31_a_project_that_answered_is_asked_again_only_once_it_holds_more() {
        let projects = [project("/p/big", 50 * GIB, &[("s2", true)]), project("/p/small", 5 * GIB, &[("s1", true)])];
        // The big project's agent answered when it held what it holds now: it isn't asked again; the next one is.
        let answered = BTreeMap::from([("/p/big".to_owned(), 50 * GIB)]);
        assert_eq!(ask(GIB, false, &projects, &answered).map(|a| a.root), Some("/p/small".into()));
        // Nor once it holds less.
        let answered = BTreeMap::from([("/p/big".to_owned(), 60 * GIB), ("/p/small".to_owned(), 5 * GIB)]);
        assert_eq!(ask(GIB, false, &projects, &answered), None);
        // It grew past what it held when it answered: it's asked again.
        let answered = BTreeMap::from([("/p/big".to_owned(), 50 * GIB - 1)]);
        assert_eq!(ask(GIB, false, &projects, &answered).map(|a| a.root), Some("/p/big".into()));
    }

    #[test]
    fn w24_r31_ask_text_names_the_project_the_sizes_and_the_measured_rate() -> Result<(), String> {
        let a = Ask { session: "s".into(), root: "/p/big".into(), bytes: 3 * GIB / 2 };
        let rate = Rate::new(2 * GIB, 1).ok_or("a rate over a second")?;
        let t = ask_text("laptop", GIB, 4 * GIB, rate, 90, &a);
        assert!(t.contains("laptop's disk has 1.0 GiB free") && t.contains("(2.0 GiB/s)") && t.contains("within 90s") && t.contains("still needs 4.0 GiB") && t.contains("/p/big holds 1.5 GiB"), "{t}");
        assert_eq!(human(512), "512 B");
        Ok(())
    }
}
