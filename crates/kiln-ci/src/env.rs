//! Environments' protection (docs/ci.md W15-R03, R04), GitHub-exact where GitHub has it: a job
//! bound to an environment (`environment:`) starts only when the environment's rules let it.
//!
//! - **Deployment branches and tags.** With patterns, only refs matching them deploy; anything
//!   else fails the job with GitHub's message. A pull request's merge ref is a branch named
//!   `refs/pull/<n>/merge` here, as on GitHub, so it matches only patterns that say so.
//! - **Required reviewers.** The job waits until one of them approves; a rejection fails it.
//!   Who may review (listed, not the run's actor under `prevent-self-review`) is decided where
//!   people are known; [`gate`] takes the reviews that count.
//! - **Wait timer.** The job waits that many minutes after it became ready.
//! - **Windows** (kiln's, as cd.md's sync windows): the job deploys only while an `allow` window is
//!   open, and waits while a `deny` window is.

use serde::{Deserialize, Serialize};

use crate::trigger::pattern::Pattern;

/// The longest wait timer GitHub allows: 30 days.
pub const MAX_WAIT_MINUTES: u32 = 43_200;
/// The most required reviewers GitHub allows.
pub const MAX_REVIEWERS: usize = 6;

/// An environment's protection rules, as `.kiln/environments.yaml` and the API write them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct Protection {
    /// Required reviewers: people (`alice`) or teams (`team:web`); any one of them approves.
    pub reviewers: Vec<String>,
    /// The run's actor can't approve its own deployment.
    pub prevent_self_review: bool,
    /// Minutes a job waits after it's ready.
    pub wait_timer: u32,
    /// Branches that may deploy (GitHub's patterns); with neither these nor `tags`, any ref may.
    pub branches: Option<Vec<String>>,
    /// Tags that may deploy.
    pub tags: Option<Vec<String>>,
    /// When it may deploy (kiln's).
    pub windows: Vec<Window>,
}

/// A deploy window (W15-R03, as cd.md's sync windows): it opens at each time its five-field
/// cron names, for `duration` (`90m`, `2h`, `1d`), in `timezone` (UTC when none).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Window {
    /// Deployments go only while a window of these is open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow: Option<String>,
    /// Deployments wait while it is open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deny: Option<String>,
    pub duration: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
}

impl Window {
    /// Its cron, time zone and length; or what's wrong with it.
    fn parts(&self) -> Result<(crate::trigger::cron::Cron, jiff::tz::TimeZone, i64, bool), String> {
        let (schedule, deny) = match (&self.allow, &self.deny) {
            (Some(s), None) => (s, false),
            (None, Some(s)) => (s, true),
            _ => return Err("a window is either `allow: <cron>` or `deny: <cron>`".into()),
        };
        let cron = crate::trigger::cron::Cron::parse(schedule).map_err(|e| format!("window `{schedule}`: {e}"))?;
        let tz = crate::trigger::cron::zone(self.timezone.as_deref())?;
        let secs = crate::trigger::cron::seconds(&self.duration).and_then(|s| i64::try_from(s).ok()).filter(|s| *s > 0).ok_or_else(|| format!("window `{schedule}`: `{}` isn't a duration (`90m`, `2h`, `1d`)", self.duration))?;
        Ok((cron, tz, secs, deny))
    }
}

/// Seconds since the epoch, as RFC 3339.
fn at(secs: i64) -> String {
    jiff::Timestamp::from_second(secs).map_or_else(|_| secs.to_string(), |t| t.to_string())
}

/// Whether `windows` hold deployments at `now`: until when, and why; `None` when they may go.
pub fn closed(windows: &[Window], now: i64) -> Result<Option<(i64, String)>, String> {
    let (mut allow_open, mut next_allow) = (None::<bool>, None::<i64>);
    for w in windows {
        let (cron, tz, secs, deny) = w.parts()?;
        // Open now when it opened within its length.
        let opened = cron.fires_in(now - secs, now, &tz, 10_000).last().copied();
        match (deny, opened) {
            (true, Some(t)) => return Ok(Some((t + secs, format!("a deny window (`{}`, {}) holds deployments until {}", w.deny.as_deref().unwrap_or_default(), w.duration, at(t + secs))))),
            (true, None) => {}
            (false, Some(_)) => allow_open = Some(true),
            (false, None) => {
                allow_open.get_or_insert(false);
                if let Some(t) = cron.fires_in(now, now + 60 * 24 * 3600, &tz, 1).first().copied() {
                    next_allow = Some(next_allow.map_or(t, |n| n.min(t)));
                }
            }
        }
    }
    Ok(match allow_open {
        Some(false) => {
            let until = next_allow.unwrap_or(now + 3600);
            Some((until, format!("no allow window is open: the next opens at {}", at(until))))
        }
        _ => None,
    })
}

impl Protection {
    /// The rules' problems, GitHub's limits included.
    pub fn check(&self) -> Result<(), String> {
        if self.reviewers.len() > MAX_REVIEWERS {
            return Err(format!("an environment has at most {MAX_REVIEWERS} required reviewers"));
        }
        if self.wait_timer > MAX_WAIT_MINUTES {
            return Err(format!("the wait timer is at most {MAX_WAIT_MINUTES} minutes (30 days)"));
        }
        for p in self.branches.iter().chain(self.tags.iter()).flatten() {
            Pattern::new(p).map_err(|e| format!("`{p}`: {e}"))?;
        }
        for w in &self.windows {
            w.parts()?;
        }
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.reviewers.is_empty() && self.wait_timer == 0 && self.branches.is_none() && self.tags.is_none() && self.windows.is_empty()
    }
}

/// A reviewer's decision on a run's deployment to an environment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Review {
    pub by: String,
    pub approved: bool,
    pub comment: String,
    pub at: i64,
}

/// What an environment's rules say about a job now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gate {
    /// The ref may not deploy here: the job fails with this, GitHub's message.
    Refused(String),
    /// A reviewer rejected it: the job fails.
    Rejected { by: String, comment: String },
    /// It waits: for one of `reviewers` to approve, or until `until` (Unix seconds), or both;
    /// `window` says why when a deploy window holds it.
    Waiting { reviewers: Vec<String>, until: Option<i64>, window: Option<String> },
    /// It may start.
    Open,
}

/// The gate for a job of `git_ref` bound to environment `env`, `ready_at` being when it could
/// first have started.
pub fn gate(env: &str, p: &Protection, git_ref: &str, ready_at: i64, now: i64, reviews: &[Review]) -> Gate {
    if p.branches.is_some() || p.tags.is_some() {
        let (what, name, patterns) = match git_ref.strip_prefix("refs/tags/") {
            Some(tag) => ("Tag", tag, &p.tags),
            None => ("Branch", git_ref.strip_prefix("refs/heads/").unwrap_or(git_ref), &p.branches),
        };
        let allowed = patterns.as_ref().is_some_and(|ps| ps.iter().filter_map(|x| Pattern::new(x).ok()).any(|x| x.matches(name)));
        if !allowed {
            return Gate::Refused(format!("{what} \"{name}\" is not allowed to deploy to {env} due to environment protection rules."));
        }
    }
    if let Some(r) = reviews.iter().filter(|r| !r.approved).max_by_key(|r| r.at) {
        return Gate::Rejected { by: r.by.clone(), comment: r.comment.clone() };
    }
    let approved = p.reviewers.is_empty() || reviews.iter().any(|r| r.approved);
    let timer = (p.wait_timer > 0).then(|| ready_at + i64::from(p.wait_timer) * 60).filter(|t| *t > now);
    // A closed window holds it until it opens (rules that don't check out were refused earlier).
    let window = closed(&p.windows, now).ok().flatten();
    if approved && timer.is_none() && window.is_none() {
        return Gate::Open;
    }
    let until = match (timer, window.as_ref().map(|(t, _)| *t)) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    };
    Gate::Waiting { reviewers: if approved { Vec::new() } else { p.reviewers.clone() }, until, window: window.map(|(_, why)| why) }
}

/// A job's `deploy: { app, environment, revision, wait, timeout, artifacts }` (kiln dialect,
/// W15-R05) as the step it runs after its own: `kiln/deploy@v1` with those inputs. The job is
/// bound to the environment too (the front end sees to that), so its protection holds first.
pub fn deploy_step(v: &crate::wf::ir::YVal) -> Option<crate::wf::ir::Step> {
    use crate::wf::ir::{Step, Tpl, YVal};
    let YVal::Map(entries) = v else { return None };
    let with: Vec<(String, YVal)> = entries.iter().filter(|(k, _)| matches!(k.as_str(), "app" | "environment" | "revision" | "wait" | "timeout" | "artifacts")).cloned().collect();
    Some(Step { name: Some(Tpl::new("Deploy", Default::default())), uses: Some("kiln/deploy@v1".into()), with, ..Step::default() })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn review(by: &str, approved: bool, at: i64) -> Review {
        Review { by: by.into(), approved, comment: if approved { String::new() } else { "not now".into() }, at }
    }

    /// W15-AC01: each rule blocks and releases as specified.
    #[test]
    fn w15_ac01_branches_and_tags() {
        let p = Protection { branches: Some(vec!["main".into(), "release/*".into()]), ..Protection::default() };
        assert_eq!(gate("production", &p, "refs/heads/main", 0, 0, &[]), Gate::Open);
        assert_eq!(gate("production", &p, "refs/heads/release/1.2", 0, 0, &[]), Gate::Open);
        assert_eq!(gate("production", &p, "refs/heads/feature", 0, 0, &[]), Gate::Refused("Branch \"feature\" is not allowed to deploy to production due to environment protection rules.".into()));
        assert_eq!(
            gate("production", &p, "refs/pull/7/merge", 0, 0, &[]),
            Gate::Refused("Branch \"refs/pull/7/merge\" is not allowed to deploy to production due to environment protection rules.".into()),
            "a pull request's merge ref, as GitHub names it"
        );
        assert_eq!(gate("production", &p, "refs/tags/v1.0", 0, 0, &[]), Gate::Refused("Tag \"v1.0\" is not allowed to deploy to production due to environment protection rules.".into()), "branch patterns alone let no tag deploy");
        let tags = Protection { tags: Some(vec!["v*".into()]), ..Protection::default() };
        assert_eq!(gate("production", &tags, "refs/tags/v1.0", 0, 0, &[]), Gate::Open);
        assert!(matches!(gate("production", &tags, "refs/heads/main", 0, 0, &[]), Gate::Refused(_)));
        assert_eq!(gate("production", &Protection::default(), "refs/heads/anything", 0, 0, &[]), Gate::Open, "no patterns: any ref");
    }

    #[test]
    fn w15_ac01_reviewers_and_the_wait_timer() {
        let p = Protection { reviewers: vec!["alice".into(), "team:ops".into()], wait_timer: 10, ..Protection::default() };
        // Ready at 1000: 10 minutes to wait, and a reviewer.
        assert_eq!(gate("prod", &p, "refs/heads/main", 1000, 1000, &[]), Gate::Waiting { reviewers: vec!["alice".into(), "team:ops".into()], until: Some(1600), window: None });
        assert_eq!(gate("prod", &p, "refs/heads/main", 1000, 1200, &[review("alice", true, 1100)]), Gate::Waiting { reviewers: vec![], until: Some(1600), window: None }, "approved, the timer still runs");
        assert_eq!(gate("prod", &p, "refs/heads/main", 1000, 1600, &[review("alice", true, 1100)]), Gate::Open);
        assert_eq!(gate("prod", &p, "refs/heads/main", 1000, 1700, &[]), Gate::Waiting { reviewers: vec!["alice".into(), "team:ops".into()], until: None, window: None }, "the timer ran out, the reviewer not yet");
        assert_eq!(gate("prod", &p, "refs/heads/main", 1000, 1700, &[review("alice", true, 1100), review("bob", false, 1200)]), Gate::Rejected { by: "bob".into(), comment: "not now".into() }, "a rejection wins");
    }

    #[test]
    fn w15_r03_github_limits() {
        assert!(Protection { reviewers: (0..7).map(|i| format!("u{i}")).collect(), ..Protection::default() }.check().is_err());
        assert!(Protection { wait_timer: MAX_WAIT_MINUTES + 1, ..Protection::default() }.check().is_err());
        assert!(Protection { wait_timer: MAX_WAIT_MINUTES, reviewers: vec!["a".into()], branches: Some(vec!["main".into()]), ..Protection::default() }.check().is_ok());
        let parsed: Protection = serde_yaml::from_str("reviewers: [alice]\nprevent-self-review: true\nwait-timer: 5\nbranches: [main]\n").unwrap_or_default();
        assert_eq!(parsed, Protection { reviewers: vec!["alice".into()], prevent_self_review: true, wait_timer: 5, branches: Some(vec!["main".into()]), tags: None, windows: Vec::new() });
    }

    /// W15-R05: a kiln job's `deploy:` binds it to the environment and deploys after its steps;
    /// it needs no steps of its own, and can't name an environment other than the job's.
    #[test]
    fn w15_r05_the_deploy_shorthand() {
        let src = "on: push\njobs:\n  ship:\n    runs-on: x\n    deploy: { app: web, environment: staging, timeout: 5m, wait: synced }\n  odd:\n    runs-on: x\n    environment: production\n    deploy: { environment: staging }\n    steps:\n      - run: echo\n";
        let (w, d) = crate::wf::parse_workflow(".kiln/ship.yaml", src, crate::expr::Dialect::Kiln);
        let job = |id: &str| w.as_ref().and_then(|w| w.jobs.iter().find(|j| j.id == id)).cloned().unwrap_or_default();
        let ship = job("ship");
        assert_eq!(ship.environment.as_ref().map(|e| e.name.src.as_str()), Some("staging"), "bound to its environment");
        let step = ship.kiln.deploy.as_ref().and_then(deploy_step).unwrap_or_default();
        assert_eq!(step.uses.as_deref(), Some("kiln/deploy@v1"));
        let mut keys: Vec<&str> = step.with.iter().map(|(k, _)| k.as_str()).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["app", "environment", "timeout", "wait"]);
        let errors: Vec<&str> = d.iter().filter(|x| x.is_error()).map(|x| x.code).collect();
        assert_eq!(errors, ["KCI1021"], "only odd's other environment is an error: {d:?}");
    }

    /// W15-R03's windows: a deny window holds a deployment until it closes; with allow windows,
    /// one waits for the next to open. Rules that don't check out are refused.
    #[test]
    fn w15_r03_windows() {
        // Saturday 2026-10-17 12:00 UTC, and Monday 2026-10-19 09:30 UTC.
        let (saturday, monday) = (1_792_238_400, 1_792_402_200);
        let weekend = Protection { windows: vec![Window { allow: None, deny: Some("0 0 * * 6".into()), duration: "48h".into(), timezone: None }], ..Protection::default() };
        assert!(weekend.check().is_ok());
        let held = gate("prod", &weekend, "refs/heads/main", saturday, saturday, &[]);
        let monday_midnight = 1_792_368_000;
        assert!(matches!(&held, Gate::Waiting { until: Some(t), window: Some(why), .. } if *t == monday_midnight && why.contains("deny window")), "{held:?}");
        assert_eq!(gate("prod", &weekend, "refs/heads/main", monday, monday, &[]), Gate::Open);
        // Office hours: open 09:00 to 17:00 on weekdays.
        let office = Protection { windows: vec![Window { allow: Some("0 9 * * 1-5".into()), deny: None, duration: "8h".into(), timezone: Some("UTC".into()) }], ..Protection::default() };
        assert_eq!(gate("prod", &office, "refs/heads/main", monday, monday, &[]), Gate::Open);
        let early = monday - 2 * 3600;
        assert!(matches!(gate("prod", &office, "refs/heads/main", early, early, &[]), Gate::Waiting { until: Some(t), window: Some(_), .. } if t == monday_midnight + 9 * 3600));
        for bad in [Window { allow: Some("0 9 * * *".into()), deny: Some("0 0 * * *".into()), duration: "1h".into(), timezone: None }, Window { allow: Some("nope".into()), deny: None, duration: "1h".into(), timezone: None }, Window { allow: Some("0 9 * * *".into()), deny: None, duration: "soon".into(), timezone: None }] {
            assert!(Protection { windows: vec![bad.clone()], ..Protection::default() }.check().is_err(), "{bad:?}");
        }
    }
}
