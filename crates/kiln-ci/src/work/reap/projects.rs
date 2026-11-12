//! W24-R31's reaper over the machine's own projects (Ada, 2026-11-11: "The reaper needs to keep
//! things cleaned up"): the build output in the folders its agent sessions work in, and what
//! `~/.cache` holds. The reaper deletes what it recognizes ([`entries`], then [`super::plan`] as
//! for every cache), and an agent at work in a project cleans what it doesn't ([`ask`]).

use super::{Entry, Resolved};

/// Something the reaper recognized as build output or cache, as the runner found it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub path: String,
    /// The project it's in (or `~/.cache` for what's there): `keep_last` keeps the newest of each.
    pub group: String,
    pub bytes: u64,
    /// When it was last written (unix seconds).
    pub used: i64,
    /// A process names it now: it isn't the reaper's to take.
    pub busy: bool,
}

/// What a reap may evict, and what the cache holds in all (busy folders included).
pub fn entries(found: &[Found]) -> (Vec<Entry>, u64) {
    let held = found.iter().map(|f| f.bytes).fold(0u64, u64::saturating_add);
    // How long each saves isn't known: least recently used goes first.
    let entries = found.iter().filter(|f| !f.busy).map(|f| Entry { id: f.path.clone(), group: f.group.clone(), bytes: f.bytes, used: f.used, saved_ms: 0 }).collect();
    (entries, held)
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

/// Whom the reaper asks to clean, once its own evictions leave `free` bytes free: none while free
/// space reaches the target, or while an ask of its is still with an agent (`waiting`); else an
/// idle agent of the project holding the most the reaper doesn't recognize. One ask at a time, so
/// each is answered before the next is chosen on what's free then.
pub fn ask(r: &Resolved, free: u64, waiting: bool, projects: &[Project]) -> Option<Ask> {
    if free >= r.target || waiting {
        return None;
    }
    let mut order: Vec<&Project> = projects.iter().filter(|p| p.bytes > 0).collect();
    order.sort_by(|a, b| b.bytes.cmp(&a.bytes).then(a.root.cmp(&b.root)));
    order.into_iter().find_map(|p| {
        let mut idle: Vec<&Agent> = p.agents.iter().filter(|a| a.idle).collect();
        idle.sort_by(|a, b| a.session.cmp(&b.session));
        idle.first().map(|a| Ask { session: a.session.clone(), root: p.root.clone(), bytes: p.bytes })
    })
}

/// What the reaper says to the agent it asks.
pub fn ask_text(machine: &str, free: u64, target: u64, a: &Ask) -> String {
    format!(
        "kiln's reaper: {machine}'s disk has {} free, under the {} it keeps free (W24-R31). It deleted the build output it recognizes; your project {} still holds {}. Delete what in it can be made again (build output, caches, downloaded dependencies, logs you're done with), never sources and never work that isn't committed, then say what you freed.",
        human(free),
        human(target),
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
    use super::super::{Policy, Scope, resolve};
    use super::*;

    const GIB: u64 = 1 << 30;

    fn policy() -> Resolved {
        resolve(1000 * GIB, &[(Scope::System, &Policy::default())])
    }

    fn project(root: &str, bytes: u64, agents: &[(&str, bool)]) -> Project {
        Project { root: root.into(), bytes, agents: agents.iter().map(|(s, idle)| Agent { session: (*s).into(), idle: *idle }).collect() }
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
    fn w24_r31_asks_the_largest_projects_idle_agent_only_under_the_target() {
        let r = policy();
        let projects = [project("/p/small", 5 * GIB, &[("s1", true)]), project("/p/big", 50 * GIB, &[("s3", false), ("s2", true)])];
        assert_eq!(ask(&r, r.target, false, &projects), None);
        assert_eq!(ask(&r, r.target - 1, false, &projects), Some(Ask { session: "s2".into(), root: "/p/big".into(), bytes: 50 * GIB }));
    }

    #[test]
    fn w24_r31_one_ask_at_a_time_and_none_to_a_busy_agent() {
        let r = policy();
        let projects = [project("/p/big", 50 * GIB, &[("s3", false)]), project("/p/small", 5 * GIB, &[("s1", true)])];
        assert_eq!(ask(&r, 0, true, &projects), None);
        // The biggest project's agent is at work: the next project's idle one is asked.
        assert_eq!(ask(&r, 0, false, &projects).map(|a| a.session), Some("s1".into()));
        assert_eq!(ask(&r, 0, false, &[project("/p/a", 0, &[("s1", true)])]), None);
    }

    #[test]
    fn w24_r31_ask_text_names_the_project_and_the_sizes() {
        let a = Ask { session: "s".into(), root: "/p/big".into(), bytes: 3 * GIB / 2 };
        let t = ask_text("laptop", GIB, 100 * GIB, &a);
        assert!(t.contains("laptop's disk has 1.0 GiB free") && t.contains("100.0 GiB") && t.contains("/p/big still holds 1.5 GiB"), "{t}");
        assert_eq!(human(512), "512 B");
    }
}
