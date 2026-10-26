//! Priority by intent (docs/ci.md W28-R64, R65), over W05-R09's scale: what a run is for decides
//! how soon it goes. Gate first (it lands, deploys, or checks the commit a server runs), then what
//! someone waits on (a person who asked for it, an agent blocked on its verdict), then trunk, then
//! work in progress. Strict between classes; W05-R09's aging lifts what waits, so nothing starves:
//! a class `c` behind a saturated stream of gate work starts within `(90 − p_c) / aging` minutes,
//! and one leg's time.
//!
//! Opt-in (`[ci.scheduler] priority = "intent"`); a job's own `priority:` still overrides it.

use serde::Serialize;

/// What a run is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Intent {
    Gate,
    Waiting,
    Trunk,
    Wip,
}

impl Intent {
    /// Its default priority on W05-R09's scale.
    pub fn priority(self) -> u8 {
        match self {
            Intent::Gate => 90,
            Intent::Waiting => 80,
            Intent::Trunk => 60,
            Intent::Wip => 40,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Intent::Gate => "gate",
            Intent::Waiting => "waiting",
            Intent::Trunk => "trunk",
            Intent::Wip => "wip",
        }
    }
}

/// What the dispatcher knows of a run (and of the leg, for `deploys`).
#[derive(Debug, Clone, Copy, Default)]
pub struct Facts<'a> {
    /// The event that started it (`push`, `land`, `workflow_dispatch`, `agent`, ...).
    pub event: &'a str,
    /// The branch it runs on.
    pub branch: &'a str,
    /// The repository's default branch.
    pub default_branch: &'a str,
    /// The leg deploys (its job names an environment).
    pub deploys: bool,
    /// Its commit is the one a kiln server runs.
    pub server_commit: bool,
    /// Who is blocked on its verdict, and how: kiln's records (an agent's change request with
    /// `fix_checks`, a plan run's stack step, `ci.verdict.wait`).
    pub waiting: Option<&'a str>,
}

/// A run's (or leg's) intent, and why.
pub fn classify(f: &Facts) -> (Intent, String) {
    match f.event {
        "land" | "merge_group" => return (Intent::Gate, "it lands changes from the land queue".into()),
        "deploy" => return (Intent::Gate, "it deploys".into()),
        _ => {}
    }
    if f.deploys {
        return (Intent::Gate, "it deploys to an environment".into());
    }
    if f.server_commit {
        return (Intent::Gate, "its commit is the one this server runs".into());
    }
    match f.event {
        "workflow_dispatch" | "manual" => return (Intent::Waiting, "a person asked for it".into()),
        "agent" => return (Intent::Waiting, "an agent asked for it and waits for its verdict".into()),
        _ => {}
    }
    if let Some(w) = f.waiting {
        return (Intent::Waiting, w.to_owned());
    }
    if !f.branch.is_empty() && f.branch == f.default_branch {
        return (Intent::Trunk, format!("it runs on {}, the default branch", f.branch));
    }
    (Intent::Wip, if f.branch.is_empty() { "work in progress".into() } else { format!("work in progress on {}", f.branch) })
}

#[cfg(test)]
mod tests {
    use super::{Facts, Intent, classify};

    /// W28-R64: each case gets its class, with why.
    #[test]
    fn w28_r64_classification() {
        let f = |event: &'static str, branch: &'static str| Facts { event, branch, default_branch: "main", ..Facts::default() };
        let cases = [
            (f("land", "main"), Intent::Gate),
            (f("merge_group", "gh-readonly-queue/main/pr-1"), Intent::Gate),
            (f("deploy", "main"), Intent::Gate),
            (Facts { deploys: true, ..f("push", "main") }, Intent::Gate),
            (Facts { server_commit: true, ..f("push", "main") }, Intent::Gate),
            (f("workflow_dispatch", "main"), Intent::Waiting),
            (f("manual", "feature"), Intent::Waiting),
            (f("agent", "main"), Intent::Waiting),
            (Facts { waiting: Some("agent A's change CR12 waits on its verdict (fix_checks)"), ..f("push", "agents/fix-a") }, Intent::Waiting),
            (f("push", "main"), Intent::Trunk),
            (f("schedule", "main"), Intent::Trunk),
            (f("push", "feature"), Intent::Wip),
            (f("pull_request", "feature"), Intent::Wip),
            (f("diff", ""), Intent::Wip),
        ];
        for (facts, want) in cases {
            let (got, why) = classify(&facts);
            assert_eq!(got, want, "{facts:?}: {why}");
            assert!(!why.is_empty());
        }
        let p: Vec<u8> = [Intent::Gate, Intent::Waiting, Intent::Trunk, Intent::Wip].iter().map(|i| i.priority()).collect();
        assert!(p.windows(2).all(|w| w.first() > w.get(1)), "strict: {p:?}");
    }
}
