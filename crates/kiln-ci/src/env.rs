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
//! - **Promotion** (kiln's, as cd.md's): `after: staging` deploys only a commit that deployed to
//!   staging successfully, `soak: 30m` once it has been there that long.
//! - **Custom gates** (kiln's): `gates: [{ workflow: .kiln/smoke.yaml }, { check: security }]` deploy
//!   a commit only when that workflow's run, or that check or status (an attached agent's, say),
//!   passed for it; one that failed fails the job.

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
    /// U10: explicit canonical policy binding, never inferred from an environment name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policy: Option<EnvironmentId>,
    /// Counted approvals, shared with release gates. Legacy `reviewers` means one approval.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approvals: Option<Approvals>,
    /// Additional prerequisites; legacy `after` remains accepted on import.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub prerequisites: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub freeze: Option<String>,
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
    /// The environment a commit must have deployed to successfully first (kiln's, as cd.md's).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
    /// How long it must have run there (`30m`), with `after`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub soak: Option<String>,
    /// What must have passed for the commit first (kiln's).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub gates: Vec<CustomGate>,
}

/// A custom gate (W15-R03): a workflow, whose run for the commit must succeed, or a check run or
/// commit status of that name on the commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CustomGate {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<String>,
}

impl CustomGate {
    /// As people name it: the workflow's path, or the check's name.
    pub fn name(&self) -> String {
        self.workflow.clone().or_else(|| self.check.clone()).unwrap_or_default()
    }
}

/// What the dispatcher found for a commit, for its environment's promotion and custom gates.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Found {
    /// When its deployment to `after` succeeded, if it did.
    pub promoted_at: Option<i64>,
    /// U10: observations of every canonical prerequisite, supplied by the domain service.
    pub prerequisites: Vec<(String, Prerequisite)>,
    /// Each custom gate's name and result: passed, failed, or not yet (`None`).
    pub gates: Vec<(String, Option<bool>)>,
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

/// A duration's seconds (`30m`).
fn anyhow_free_seconds(t: &str) -> Option<i64> {
    crate::trigger::cron::seconds(t).and_then(|s| i64::try_from(s).ok()).filter(|s| *s > 0)
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
        let opened = cron.fires_in(now.saturating_sub(secs), now, &tz, 10_000).last().copied();
        match (deny, opened) {
            (true, Some(t)) => return Ok(Some((t.saturating_add(secs), format!("a deny window (`{}`, {}) holds deployments until {}", w.deny.as_deref().unwrap_or_default(), w.duration, at(t.saturating_add(secs)))))),
            (true, None) => {}
            (false, Some(_)) => allow_open = Some(true),
            (false, None) => {
                allow_open.get_or_insert(false);
                if let Some(t) = cron.fires_in(now, now.saturating_add(60 * 24 * 3600), &tz, 1).first().copied() {
                    next_allow = Some(next_allow.map_or(t, |n| n.min(t)));
                }
            }
        }
    }
    Ok(match allow_open {
        Some(false) => {
            let until = next_allow.unwrap_or(now.saturating_add(3600));
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
        if let Some(t) = &self.soak {
            anyhow_free_seconds(t).ok_or_else(|| format!("`soak: {t}` isn't a duration (`30m`, `2h`, `1d`)"))?;
            if self.after.is_none() && self.prerequisites.is_empty() {
                return Err("`soak` says how long a commit runs in the environment `after` names: say `after` too".into());
            }
        }
        if self.after.as_deref().is_some_and(|a| a.trim().is_empty()) {
            return Err("`after` names an environment".into());
        }
        for g in &self.gates {
            match (&g.workflow, &g.check) {
                (Some(w), None) if w.starts_with(".github/workflows/") || w.starts_with(".kiln/") => {}
                (Some(w), None) => return Err(format!("gate `{w}`: a workflow's path, under .github/workflows/ or .kiln/")),
                (None, Some(c)) if !c.trim().is_empty() => {}
                _ => return Err("a gate is `workflow: <path>` or `check: <name>`".into()),
            }
        }
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.policy.is_none() && self.approvals.is_none() && self.prerequisites.is_empty() && self.freeze.is_none() && self.reviewers.is_empty() && self.wait_timer == 0 && self.branches.is_none() && self.tags.is_none() && self.windows.is_empty() && self.after.is_none() && self.gates.is_empty()
    }
}

/// U10: an environment name is meaningful only within its owner scope. Server release
/// environments and repository job environments never collide merely because names match.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "kebab-case")]
pub enum EnvironmentScope {
    Server,
    Repository(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentId {
    pub scope: EnvironmentScope,
    pub name: String,
}

impl EnvironmentId {
    /// A JSON tuple is unambiguous even when a repository id contains punctuation.
    pub fn key(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(&(&self.scope, self.name.to_ascii_lowercase()))
    }
}

/// U10: approvals authorize one protected action, never another job or release. A combined
/// flow can use a single explicit action id; matching names or commits confer no authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "kebab-case")]
pub enum ProtectedAction {
    Job(String),
    Release(String),
    Flow(String),
}

/// The common approval policy. Callers still enforce capabilities, agent permission and team
/// membership before recording a decision; the evaluator only receives eligible decisions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Approvals {
    #[serde(default = "one_approval")]
    pub count: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reviewers: Vec<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub self_approve: bool,
}

fn one_approval() -> u32 { 1 }

/// Normalize the person behind a user, token or delegated agent principal.
pub fn person(name: &str) -> &str {
    let name = name.rsplit_once(" for ").or_else(|| name.rsplit_once(" of ")).map_or(name, |(_, who)| who);
    name.strip_prefix("user:").unwrap_or(name)
}

/// Distinct eligible people who approved. Repeated durable decisions do not count twice.
pub fn approved_people<'a>(policy: &Approvals, actor: &str, people: impl IntoIterator<Item = &'a str>) -> Vec<&'a str> {
    let mut people: Vec<&str> = people.into_iter().map(person)
        .filter(|p| policy.self_approve || *p != person(actor))
        .filter(|p| policy.reviewers.is_empty() || policy.reviewers.iter().any(|r| r == *p))
        .collect();
    people.sort_unstable();
    people.dedup();
    people
}

/// U10: shared admission limit; the owner supplies active observations and serializes admission.
pub fn concurrency_available(limit: Option<u32>, active: usize) -> bool {
    limit.is_none_or(|n| active < usize::try_from(n).unwrap_or(usize::MAX))
}

/// A successful prerequisite's soak deadline. Saturation keeps hostile timestamps from panicking.
pub fn soak_until(since: i64, duration: Option<&str>, now: i64) -> Result<Option<i64>, String> {
    let Some(duration) = duration else { return Ok(None) };
    let seconds = anyhow_free_seconds(duration).ok_or_else(|| format!("soak `{duration}` isn't a duration (`30m`, `2h`, `1d`)"))?;
    Ok(Some(since.saturating_add(seconds)).filter(|until| *until > now))
}

/// U10: what a domain observed upstream. Artifact equality, remote signatures and the
/// definition of successful deployment belong to that domain, not to this policy engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Prerequisite {
    Running { since: i64 },
    Pending(String),
    Refused(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrerequisiteGate {
    Open,
    Waiting { until: Option<i64>, why: String },
    Refused(String),
}

pub fn prerequisite_gate(observed: &Prerequisite, soak: Option<&str>, now: i64) -> Result<PrerequisiteGate, String> {
    Ok(match observed {
        Prerequisite::Running { since } => match soak_until(*since, soak, now)? {
            Some(until) => PrerequisiteGate::Waiting { until: Some(until), why: format!("soaking until {}", at(until)) },
            None => PrerequisiteGate::Open,
        },
        Prerequisite::Pending(why) => PrerequisiteGate::Waiting { until: None, why: why.clone() },
        Prerequisite::Refused(why) => PrerequisiteGate::Refused(why.clone()),
    })
}

/// U10: canonical target identity; provider-specific execution consumes these declarations.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Targets {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub clusters: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runners: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub integration: Option<String>,
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub platforms: std::collections::BTreeMap<String, serde_json::Map<String, serde_json::Value>>,
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub kilncd: std::collections::BTreeMap<String, String>,
}

impl Targets {
    /// Restrict a declaration to the target kind it names.
    pub fn of(self, kind: Option<&str>) -> Self {
        match kind {
            Some("kubernetes") => Self { clusters: self.clusters, namespace: self.namespace.filter(|n| !n.trim().is_empty()), kilncd: self.kilncd, ..Self::default() },
            Some("host" | "container" | "microvm") => Self { runners: self.runners.map(|r| r.trim().to_owned()), ..Self::default() },
            Some("platform") => Self { integration: self.integration.map(|i| i.trim().to_owned()), platforms: self.platforms, ..Self::default() },
            _ => Self::default(),
        }
    }
}

/// U10: one persisted policy and target identity, independent of its job/release consumer.
/// Consumer-specific provider settings and release observations remain outside this record.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EnvironmentPolicy {
    /// The declared producer, so removing versioned configuration cannot leave stale rules.
    pub source: String,
    /// Prior saved definition while a versioned config explicitly overrides it. This is an
    /// import/source snapshot, never another evaluated policy; removing that config restores it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback: Option<Box<EnvironmentPolicy>>,
    pub protection: Protection,
    pub concurrency: Option<u32>,
    pub agents_approve: bool,
    pub targets: Targets,
    pub kind: Option<String>,
}

/// A durable decision's explicit protection boundary (U10).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionReview {
    pub environment: EnvironmentId,
    pub action: ProtectedAction,
    pub review: Review,
}

/// Select only decisions for this exact action and owner. Rejections remain durable as well.
pub fn action_reviews(environment: &EnvironmentId, action: &ProtectedAction, reviews: &[ActionReview]) -> Vec<Review> {
    reviews.iter().filter(|r| r.environment.scope == environment.scope && r.environment.name.eq_ignore_ascii_case(&environment.name) && r.action == *action).map(|r| r.review.clone()).collect()
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
    /// `window` says why when a deploy window holds it, `promotion` when the commit hasn't
    /// deployed to `after` or soaked there, `gate` when a custom gate hasn't passed yet.
    Waiting { reviewers: Vec<String>, until: Option<i64>, window: Option<String>, promotion: Option<String>, gate: Option<String> },
    /// It may start.
    Open,
}

/// The gate for a job of `git_ref` bound to environment `env`, `ready_at` being when it could
/// first have started.
pub fn gate(env: &str, p: &Protection, git_ref: &str, ready_at: i64, now: i64, reviews: &[Review]) -> Gate {
    gate_with(env, p, git_ref, ready_at, now, reviews, &Found::default())
}

/// [`gate`], with what was found of the commit's promotion and custom gates.
pub fn gate_with(env: &str, p: &Protection, git_ref: &str, ready_at: i64, now: i64, reviews: &[Review], found: &Found) -> Gate {
    if let Err(why) = p.check() {
        return Gate::Refused(why);
    }
    if let Some(why) = &p.freeze {
        return Gate::Waiting { reviewers: Vec::new(), until: None, window: Some(format!("{env} is frozen: {why}")), promotion: None, gate: None };
    }
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
    // A custom gate that failed fails the job; one not passed yet holds it.
    let named: Vec<String> = p.gates.iter().map(CustomGate::name).collect();
    if let Some((name, _)) = found.gates.iter().find(|(n, r)| *r == Some(false) && named.contains(n)) {
        return Gate::Refused(format!("The deployment to {env} was stopped by its gate {name}: it failed for this commit."));
    }
    let open: Vec<&String> = named.iter().filter(|n| !found.gates.iter().any(|(g, r)| g == *n && *r == Some(true))).collect();
    let gate = (!open.is_empty()).then(|| format!("{} to pass for this commit", open.iter().map(|n| format!("`{n}`")).collect::<Vec<_>>().join(", ")));
    // Promotion: the commit deployed to `after`, and soaked there.
    let (promotion, soaked) = match (&p.after, found.promoted_at) {
        (None, _) => (None, None),
        (Some(a), None) => (Some(format!("this commit to deploy to {a} first")), None),
        (Some(a), Some(t)) => match soak_until(t, p.soak.as_deref(), now).ok().flatten() {
            Some(until) => (Some(format!("this commit to soak in {a} until {}", at(until))), Some(until)),
            None => (None, None),
        },
    };
    // Review eligibility and self-review checks are enforced at the durable write boundary.
    let approval_policy = Approvals { count: p.approvals.as_ref().map_or(1, |a| a.count.max(1)), reviewers: Vec::new(), self_approve: true };
    let required = p.approvals.is_some() || !p.reviewers.is_empty();
    let have = approved_people(&approval_policy, "", reviews.iter().filter(|r| r.approved).map(|r| r.by.as_str())).len();
    let approved = !required || have >= usize::try_from(approval_policy.count).unwrap_or(usize::MAX);
    let reviewers = p.approvals.as_ref().map_or(&p.reviewers, |a| &a.reviewers);
    let mut prerequisite_wait = None;
    let mut prerequisite_until = None;
    for name in &p.prerequisites {
        let missing = Prerequisite::Pending(format!("this commit to deploy to {name} first"));
        let observed = found.prerequisites.iter().find(|(n, _)| n == name).map_or(&missing, |(_, o)| o);
        match prerequisite_gate(observed, p.soak.as_deref(), now) {
            Ok(PrerequisiteGate::Open) => {}
            Ok(PrerequisiteGate::Waiting { until, why }) => {
                prerequisite_wait.get_or_insert(format!("{name}: {why}"));
                prerequisite_until = prerequisite_until.into_iter().chain(until).max();
            }
            Ok(PrerequisiteGate::Refused(why)) | Err(why) => return Gate::Refused(why),
        }
    }
    let promotion = promotion.or(prerequisite_wait);
    let timer = (p.wait_timer > 0).then(|| ready_at.saturating_add(i64::from(p.wait_timer) * 60)).filter(|t| *t > now);
    // A closed window holds it until it opens (rules that don't check out were refused earlier).
    let window = closed(&p.windows, now).ok().flatten();
    if approved && timer.is_none() && window.is_none() && promotion.is_none() && gate.is_none() {
        return Gate::Open;
    }
    let until = [timer, window.as_ref().map(|(t, _)| *t), soaked, prerequisite_until].into_iter().flatten().max();
    Gate::Waiting { reviewers: if approved { Vec::new() } else if reviewers.is_empty() { vec!["anyone who may approve here".into()] } else { reviewers.clone() }, until, window: window.map(|(_, why)| why), promotion, gate }
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

    #[test]
    fn u10_approvals_are_bound_to_owner_environment_and_action() {
        let environment = EnvironmentId { scope: EnvironmentScope::Repository("repo-a".into()), name: "production".into() };
        let job = ProtectedAction::Job("run-1".into());
        let release = ProtectedAction::Release("release-1".into());
        let decisions = vec![ActionReview { environment: environment.clone(), action: job.clone(), review: review("alice", true, 10) }];
        assert_eq!(action_reviews(&environment, &job, &decisions).len(), 1);
        assert!(action_reviews(&environment, &release, &decisions).is_empty());
        let other = EnvironmentId { scope: EnvironmentScope::Repository("repo-b".into()), ..environment.clone() };
        assert!(action_reviews(&other, &job, &decisions).is_empty());
        // One deliberately shared flow action reuses its durable decision at both boundaries.
        let flow = ProtectedAction::Flow("ci-to-deploy-1".into());
        let decisions = vec![ActionReview { environment: environment.clone(), action: flow.clone(), review: review("alice", true, 10) }];
        let policy = Protection { reviewers: vec!["alice".into()], ..Protection::default() };
        let selected = action_reviews(&environment, &flow, &decisions);
        assert_eq!(gate("production", &policy, "refs/heads/main", 0, 10, &selected), Gate::Open);
        assert!(matches!(gate("production", &policy, "refs/heads/main", 0, 10, &action_reviews(&environment, &release, &decisions)), Gate::Waiting { .. }));
        assert!(action_reviews(&environment, &job, &decisions).is_empty());
    }

    #[test]
    fn u10_counted_approvals_deduplicate_people_and_preserve_self_review() {
        let policy = Approvals { count: 2, reviewers: vec!["alice".into(), "bob".into()], self_approve: false };
        assert_eq!(approved_people(&policy, "user:alice", ["user:alice", "user:bob", "agent:fix for user:bob", "user:carol"]), ["bob"]);
        let protection = Protection { approvals: Some(policy), ..Protection::default() };
        assert!(matches!(gate("prod", &protection, "refs/heads/main", 0, 10, &[review("bob", true, 1), review("bob", true, 2)]), Gate::Waiting { .. }));
        assert_eq!(gate("prod", &protection, "refs/heads/main", 0, 10, &[review("alice", true, 1), review("bob", true, 2)]), Gate::Open);
    }

    #[test]
    fn u10_shared_prerequisite_soak_and_concurrency_decisions() -> Result<(), String> {
        let policy = Protection { prerequisites: vec!["staging".into(), "canary".into()], soak: Some("1m".into()), ..Protection::default() };
        let mut found = Found { prerequisites: vec![("staging".into(), Prerequisite::Running { since: 100 })], ..Found::default() };
        assert!(matches!(gate_with("prod", &policy, "refs/heads/main", 0, 160, &[], &found), Gate::Waiting { promotion: Some(_), .. }));
        found.prerequisites.push(("canary".into(), Prerequisite::Running { since: 120 }));
        assert!(matches!(gate_with("prod", &policy, "refs/heads/main", 0, 160, &[], &found), Gate::Waiting { until: Some(180), .. }));
        assert_eq!(gate_with("prod", &policy, "refs/heads/main", 0, 180, &[], &found), Gate::Open);
        assert_eq!(prerequisite_gate(&Prerequisite::Refused("different artifacts".into()), Some("1m"), 180)?, PrerequisiteGate::Refused("different artifacts".into()));
        assert!(concurrency_available(Some(2), 1));
        assert!(!concurrency_available(Some(2), 2));
        assert!(concurrency_available(None, usize::MAX));
        Ok(())
    }

    #[test]
    fn u10_invalid_persisted_policy_refuses_instead_of_opening() {
        let policy = Protection { windows: vec![Window { allow: Some("bad cron".into()), deny: None, duration: "1h".into(), timezone: None }], ..Protection::default() };
        assert!(matches!(gate("prod", &policy, "refs/heads/main", 0, 100, &[]), Gate::Refused(_)));
    }

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
        assert_eq!(gate("prod", &p, "refs/heads/main", 1000, 1000, &[]), Gate::Waiting { reviewers: vec!["alice".into(), "team:ops".into()], until: Some(1600), window: None, promotion: None, gate: None });
        assert_eq!(gate("prod", &p, "refs/heads/main", 1000, 1200, &[review("alice", true, 1100)]), Gate::Waiting { reviewers: vec![], until: Some(1600), window: None, promotion: None, gate: None }, "approved, the timer still runs");
        assert_eq!(gate("prod", &p, "refs/heads/main", 1000, 1600, &[review("alice", true, 1100)]), Gate::Open);
        assert_eq!(gate("prod", &p, "refs/heads/main", 1000, 1700, &[]), Gate::Waiting { reviewers: vec!["alice".into(), "team:ops".into()], until: None, window: None, promotion: None, gate: None }, "the timer ran out, the reviewer not yet");
        assert_eq!(gate("prod", &p, "refs/heads/main", 1000, 1700, &[review("alice", true, 1100), review("bob", false, 1200)]), Gate::Rejected { by: "bob".into(), comment: "not now".into() }, "a rejection wins");
    }

    #[test]
    fn w15_r03_github_limits() {
        assert!(Protection { reviewers: (0..7).map(|i| format!("u{i}")).collect(), ..Protection::default() }.check().is_err());
        assert!(Protection { wait_timer: MAX_WAIT_MINUTES + 1, ..Protection::default() }.check().is_err());
        assert!(Protection { wait_timer: MAX_WAIT_MINUTES, reviewers: vec!["a".into()], branches: Some(vec!["main".into()]), ..Protection::default() }.check().is_ok());
        let parsed: Protection = serde_yaml::from_str("reviewers: [alice]\nprevent-self-review: true\nwait-timer: 5\nbranches: [main]\n").unwrap_or_default();
        assert_eq!(parsed, Protection { reviewers: vec!["alice".into()], prevent_self_review: true, wait_timer: 5, branches: Some(vec!["main".into()]), ..Protection::default() });
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
    /// W15-R03's promotion and custom gates: a commit deploys after it deployed to `after` and
    /// soaked there, and once its gates passed; a failed gate fails the job.
    #[test]
    fn w15_r03_promotion_and_custom_gates() {
        let yaml = "after: staging\nsoak: 30m\ngates:\n  - workflow: .kiln/smoke.yaml\n  - check: security\n";
        let p: Protection = serde_yaml::from_str(yaml).unwrap_or_default();
        assert_eq!(p.check(), Ok(()));
        let found = |promoted: Option<i64>, smoke: Option<bool>, security: Option<bool>| Found { promoted_at: promoted, prerequisites: Vec::new(), gates: vec![(".kiln/smoke.yaml".into(), smoke), ("security".into(), security)] };
        let held = |g: Gate| match g {
            Gate::Waiting { promotion, gate, until, .. } => (promotion, gate, until),
            other => (Some(format!("{other:?}")), None, None),
        };
        // Not in staging yet, nor the gates passed.
        let (promotion, gate, _) = held(gate_with("production", &p, "refs/heads/main", 0, 10_000, &[], &found(None, None, None)));
        assert!(promotion.is_some_and(|w| w.contains("deploy to staging first")) && gate.is_some_and(|w| w.contains("`.kiln/smoke.yaml`, `security`")));
        // In staging since 9,000: it soaks until 10,800.
        let (promotion, gate, until) = held(gate_with("production", &p, "refs/heads/main", 0, 10_000, &[], &found(Some(9_000), Some(true), None)));
        assert!(promotion.is_some_and(|w| w.contains("soak in staging")) && gate.is_some_and(|w| w.contains("`security`") && !w.contains("smoke")));
        assert_eq!(until, Some(10_800));
        assert_eq!(gate_with("production", &p, "refs/heads/main", 0, 10_800, &[], &found(Some(9_000), Some(true), Some(true))), Gate::Open);
        assert!(matches!(gate_with("production", &p, "refs/heads/main", 0, 10_800, &[], &found(Some(9_000), Some(false), Some(true))), Gate::Refused(m) if m.contains("stopped by its gate .kiln/smoke.yaml")));
        // Bad rules are refused.
        for bad in ["soak: 30m\n", "after: staging\nsoak: soon\n", "gates: [{ workflow: smoke.yaml }]\n", "gates: [{ workflow: .kiln/a.yaml, check: b }]\n", "gates: [{}]\n"] {
            let p: Protection = serde_yaml::from_str(bad).unwrap_or_else(|_| Protection { gates: vec![CustomGate { workflow: None, check: None }], ..Protection::default() });
            assert!(p.check().is_err(), "{bad}");
        }
    }

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
