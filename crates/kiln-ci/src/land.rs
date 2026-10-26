//! Review policy and landing (docs/ci.md W14-R05, R07 to R09), the pure part: `.kiln/review.yaml`
//! and `.kiln/owners.yaml` as read from the trunk, whether a diff's reviews accept it, what still
//! keeps it from landing, the merge strategies, the trailers landed commits carry, and how the
//! land queue batches and bisects. The server reads git, runs the checks and moves the trunk.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::trigger::pattern::Patterns;

/// A merge strategy, chosen at land time (W14-R07).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Strategy {
    /// Each diff its own commit, rebased onto the trunk's tip in stack order.
    Stack,
    /// The stack as one commit.
    StackSquash,
    /// A change request's commits as one commit.
    Squash,
    /// Each commit replayed onto the trunk's tip.
    Rebase,
    /// A merge commit joining the trunk and the branch.
    Merge,
    /// The trunk moved to the head, only if the trunk is its ancestor.
    FfOnly,
}

impl Strategy {
    pub const ALL: [Strategy; 6] = [Strategy::Stack, Strategy::StackSquash, Strategy::Squash, Strategy::Rebase, Strategy::Merge, Strategy::FfOnly];

    pub fn as_str(self) -> &'static str {
        match self {
            Strategy::Stack => "stack",
            Strategy::StackSquash => "stack-squash",
            Strategy::Squash => "squash",
            Strategy::Rebase => "rebase",
            Strategy::Merge => "merge",
            Strategy::FfOnly => "ff-only",
        }
    }

    pub fn parse(s: &str) -> Option<Strategy> {
        Strategy::ALL.into_iter().find(|x| x.as_str() == s.trim())
    }
}

/// How accepts carry over to newer versions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Sticky {
    /// Only accepts of the latest version count.
    None,
    /// Accepts carry over versions that only rebased.
    #[default]
    Rebase,
    /// As `rebase` (and, later, versions whose changes are trivial).
    Trivial,
}

/// `land:` of `.kiln/review.yaml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct Land {
    /// Accepts from people other than the author.
    pub approvals: u32,
    /// Every touched path needs one of its owners' accepts (`.kiln/owners.yaml`).
    pub owners: bool,
    /// Workflows (by name or path) or jobs (`workflow/job`) that must pass on the version and on
    /// the land candidate.
    pub checks: Vec<String>,
    /// No open threads.
    pub resolve_threads: bool,
    /// Agents' accepts count.
    pub agents_can_approve: bool,
    pub sticky_approvals: Sticky,
    /// The strategies allowed, and the default for stacks and for change requests.
    pub strategies: Vec<Strategy>,
    pub default: Option<Strategy>,
}

impl Default for Land {
    fn default() -> Self {
        Land { approvals: 1, owners: true, checks: Vec::new(), resolve_threads: true, agents_can_approve: false, sticky_approvals: Sticky::Rebase, strategies: Strategy::ALL.to_vec(), default: None }
    }
}

/// `queue:` of `.kiln/review.yaml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Queue {
    /// Candidates tested together.
    pub batch: u32,
}

impl Default for Queue {
    fn default() -> Self {
        Queue { batch: 1 }
    }
}

/// `.kiln/review.yaml`, read from the trunk when it applies (never from the diff it judges).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct Policy {
    pub trunks: Vec<String>,
    pub land: Land,
    pub queue: Queue,
    /// Who may push trunks directly, bypassing the queue.
    pub direct_push: Vec<String>,
    /// Branches (globs) that are never deleted or rewritten, and that only those who may land
    /// push (W14-R02's branch protection).
    pub protected: Vec<String>,
}

impl Policy {
    /// Whether `branch` is protected.
    pub fn protects(&self, branch: &str) -> bool {
        !self.protected.is_empty() && Patterns::new(&self.protected).is_ok_and(|p| p.matches(branch))
    }
}

impl Policy {
    pub fn parse(text: &str) -> Result<Policy, String> {
        if text.trim().is_empty() {
            return Ok(Policy::default());
        }
        serde_yaml::from_str(text).map_err(|e| format!(".kiln/review.yaml: {e}"))
    }

    /// The strategy to land with: the one asked for when it's allowed, else the default
    /// (`land.default`, else `stack` for diffs and `squash` for change requests).
    pub fn strategy(&self, asked: Option<Strategy>, change_request: bool) -> Result<Strategy, String> {
        let fallback = if change_request { Strategy::Squash } else { Strategy::Stack };
        let s = asked.or(self.land.default).unwrap_or(fallback);
        let fits = if change_request { !matches!(s, Strategy::Stack | Strategy::StackSquash) } else { !matches!(s, Strategy::Rebase | Strategy::Merge) };
        if !fits {
            return Err(format!("`{}` doesn't land a {}", s.as_str(), if change_request { "change request" } else { "stack of diffs" }));
        }
        if !self.land.strategies.contains(&s) {
            let allowed: Vec<&str> = self.land.strategies.iter().map(|s| s.as_str()).collect();
            return Err(format!("`{}` isn't allowed here (land.strategies: {})", s.as_str(), allowed.join(", ")));
        }
        Ok(s)
    }
}

/// A rule of `.kiln/owners.yaml`: these paths, these owners.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct OwnerRule {
    pub paths: Vec<String>,
    pub owners: Vec<String>,
    /// Suggested reviewers, not required.
    pub optional: bool,
}

/// `.kiln/owners.yaml`.
pub fn parse_owners(text: &str) -> Result<Vec<OwnerRule>, String> {
    if text.trim().is_empty() {
        return Ok(Vec::new());
    }
    serde_yaml::from_str(text).map_err(|e| format!(".kiln/owners.yaml: {e}"))
}

/// A principal as reviews name them: `user:ada` for `ada`, the rest as written.
pub fn principal(name: &str) -> String {
    let n = name.trim();
    if n.contains(':') || n == "admin" { n.to_owned() } else { format!("user:{n}") }
}

/// For each touched path that a required rule covers: its owners (the last matching rule wins,
/// as CODEOWNERS reads).
pub fn owners_of<'a>(rules: &'a [OwnerRule], paths: &[String]) -> BTreeMap<String, &'a [String]> {
    let mut out = BTreeMap::new();
    for p in paths {
        let rule = rules.iter().rev().find(|r| Patterns::new(&r.paths).is_ok_and(|pat| pat.matches(p)));
        if let Some(r) = rule.filter(|r| !r.optional && !r.owners.is_empty()) {
            out.insert(p.clone(), r.owners.as_slice());
        }
    }
    out
}

/// What a reviewer did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Accept,
    RequestChanges,
    Comment,
    Resign,
}

impl Action {
    pub fn parse(s: &str) -> Option<Action> {
        Some(match s {
            "accept" => Action::Accept,
            "request_changes" | "request-changes" => Action::RequestChanges,
            "comment" => Action::Comment,
            "resign" => Action::Resign,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Action::Accept => "accept",
            Action::RequestChanges => "request_changes",
            Action::Comment => "comment",
            Action::Resign => "resign",
        }
    }
}

/// A review, as kept: who, on which version, what.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Review {
    pub reviewer: String,
    /// The reviewer is an agent.
    pub agent: bool,
    pub version: u32,
    pub action: Action,
}

/// What a diff's land readiness depends on, as the server knows it.
#[derive(Debug, Clone, Default)]
pub struct Facts {
    pub author: String,
    /// Each version, in order: whether it only rebased.
    pub rebase_only: Vec<bool>,
    /// Every review, oldest first.
    pub reviews: Vec<Review>,
    pub open_threads: usize,
    /// The latest version's checks: workflow name, path, or `workflow/job` → its conclusion
    /// (`None`: not finished).
    pub checks: BTreeMap<String, Option<String>>,
    /// The latest version's files.
    pub paths: Vec<String>,
}

impl Facts {
    fn latest(&self) -> u32 {
        u32::try_from(self.rebase_only.len()).unwrap_or(u32::MAX)
    }

    /// Whether an accept of `version` still counts for the latest version.
    fn carries(&self, sticky: Sticky, version: u32) -> bool {
        let latest = self.latest();
        version == latest || (sticky != Sticky::None && version < latest && self.rebase_only.iter().skip(usize::try_from(version).unwrap_or(usize::MAX)).all(|r| *r))
    }

    /// Each reviewer's latest accept or request (comments and resignations aside; a resignation
    /// withdraws theirs).
    fn standing(&self) -> BTreeMap<&str, &Review> {
        let mut by: BTreeMap<&str, &Review> = BTreeMap::new();
        for r in &self.reviews {
            match r.action {
                Action::Accept | Action::RequestChanges => {
                    by.insert(r.reviewer.as_str(), r);
                }
                Action::Resign => {
                    by.remove(r.reviewer.as_str());
                }
                Action::Comment => {}
            }
        }
        by
    }

    /// Who accepts the latest version (by the policy: not the author, agents only when they
    /// count, accepts carried over rebase-only versions when sticky).
    pub fn accepting(&self, land: &Land) -> BTreeSet<String> {
        self.standing()
            .into_values()
            .filter(|r| r.action == Action::Accept && !r.reviewer.eq_ignore_ascii_case(&self.author) && (land.agents_can_approve || !r.agent) && self.carries(land.sticky_approvals, r.version))
            .map(|r| r.reviewer.clone())
            .collect()
    }

    /// Who asks for changes in the latest version (a newer version answers an older request).
    pub fn requesting(&self) -> Vec<String> {
        let latest = self.latest();
        self.standing().into_values().filter(|r| r.action == Action::RequestChanges && r.version == latest).map(|r| r.reviewer.clone()).collect()
    }
}

/// The review state a diff's reviews give it: `accepted` (enough accepts, nobody asking for
/// changes), `changes-requested`, or `needs-review`.
pub fn review_state(land: &Land, f: &Facts) -> crate::review::State {
    use crate::review::State;
    if !f.requesting().is_empty() {
        State::ChangesRequested
    } else if f.accepting(land).len() >= usize::try_from(land.approvals).unwrap_or(usize::MAX) {
        State::Accepted
    } else {
        State::NeedsReview
    }
}

/// Whether a check's conclusion lets a land through (as GitHub counts required checks: a
/// skipped or neutral one passes).
pub fn passed(conclusion: &str) -> bool {
    matches!(conclusion, "success" | "skipped" | "neutral")
}

/// Whether `check` (a workflow's name or path, or `workflow/job`) is among `checks`, passing.
fn check_state<'a>(checks: &'a BTreeMap<String, Option<String>>, check: &str) -> Option<&'a Option<String>> {
    checks.get(check).or_else(|| checks.iter().find(|(k, _)| k.eq_ignore_ascii_case(check)).map(|(_, v)| v))
}

/// What keeps a diff from landing by `land` (empty: nothing). Each reason is a sentence.
pub fn blockers(land: &Land, owners: &[OwnerRule], f: &Facts) -> Vec<String> {
    let mut why = Vec::new();
    let accepting = f.accepting(land);
    let need = usize::try_from(land.approvals).unwrap_or(usize::MAX);
    if accepting.len() < need {
        let who = if land.agents_can_approve { "reviewers" } else { "people" };
        why.push(format!("{} of {need} accepts (from {who} other than the author)", accepting.len()));
    }
    for r in f.requesting() {
        why.push(format!("{r} asked for changes"));
    }
    if land.resolve_threads && f.open_threads > 0 {
        why.push(format!("{} open thread{}", f.open_threads, if f.open_threads == 1 { "" } else { "s" }));
    }
    for c in &land.checks {
        match check_state(&f.checks, c) {
            Some(Some(conclusion)) if passed(conclusion) => {}
            Some(Some(conclusion)) => why.push(format!("`{c}` {conclusion}")),
            Some(None) => why.push(format!("`{c}` is still running")),
            None => why.push(format!("`{c}` hasn't run on this version")),
        }
    }
    if land.owners {
        let author = principal(&f.author);
        let mut missing: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (path, list) in owners_of(owners, &f.paths) {
            let named: Vec<String> = list.iter().map(|o| principal(o)).collect();
            if named.iter().any(|o| o.eq_ignore_ascii_case(&author) || accepting.iter().any(|a| a.eq_ignore_ascii_case(o))) {
                continue;
            }
            missing.entry(named.join(", ")).or_default().push(path);
        }
        for (who, paths) in missing {
            let first = paths.first().cloned().unwrap_or_default();
            let more = if paths.len() > 1 { format!(" and {} more", paths.len() - 1) } else { String::new() };
            why.push(format!("{first}{more} need{} an accept from {who}", if paths.len() == 1 { "s" } else { "" }));
        }
    }
    why
}

/// A landed commit's message: the diff's (or change request's) and kiln's trailers, with each
/// landed change's `Kiln-Change` (W26-R03; a stack squashed into one commit carries each).
pub fn landed_message(message: &str, revision: &str, reviewed_by: &[String], changes: &[String]) -> String {
    let body = message.trim_end();
    let has = crate::review::trailers(body);
    let mut out = String::from(body);
    out.push_str(if has.is_empty() { "\n\n" } else { "\n" });
    let key = if revision.starts_with("CR") { "Change-Request" } else { "Differential-Revision" };
    out.push_str(&format!("{key}: {revision}\n"));
    for r in reviewed_by {
        out.push_str(&format!("Reviewed-by: {r}\n"));
    }
    crate::identity::with_kiln_changes(&out, changes)
}

/// How many candidates the queue tests together next: the policy's batch, halved after each
/// failure (so a failing batch is bisected down to its culprit), whole again after a land.
pub fn next_batch(policy_batch: u32, failures_since_land: u32) -> u32 {
    let b = policy_batch.max(1);
    b.checked_shr(failures_since_land).unwrap_or(0).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn review(reviewer: &str, version: u32, action: Action) -> Review {
        Review { reviewer: reviewer.into(), agent: reviewer.starts_with("agent:"), version, action }
    }

    #[test]
    fn w14_r09_policy_file() -> Result<(), String> {
        let p = Policy::parse("trunks: [main, release]\nland:\n  approvals: 2\n  checks: [ci]\n  agents-can-approve: true\n  sticky-approvals: none\n  strategies: [stack, squash]\n  default: squash\nqueue:\n  batch: 4\ndirect-push: [ada]\n")?;
        assert_eq!((p.trunks.len(), p.land.approvals, p.land.sticky_approvals, p.queue.batch), (2, 2, Sticky::None, 4));
        assert_eq!(p.land.default, Some(Strategy::Squash));
        // Defaults: one accept, open threads block, owners count, every strategy.
        let d = Policy::parse("")?;
        assert_eq!((d.land.approvals, d.land.resolve_threads, d.land.owners, d.land.strategies.len()), (1, true, true, 6));
        // Strategies: the one asked when allowed, else the default; each to its kind.
        assert_eq!(d.strategy(None, false), Ok(Strategy::Stack));
        assert_eq!(d.strategy(None, true), Ok(Strategy::Squash));
        assert!(d.strategy(Some(Strategy::Merge), false).is_err(), "a stack isn't merged");
        assert!(d.strategy(Some(Strategy::Stack), true).is_err(), "a change request isn't a stack");
        assert!(p.strategy(Some(Strategy::FfOnly), true).is_err_and(|e| e.contains("isn't allowed")));
        assert!(Policy::parse("land: [").is_err());
        Ok(())
    }

    #[test]
    fn w14_r09_accepts() {
        let land = Land::default();
        let mut f = Facts { author: "user:ada".into(), rebase_only: vec![false], ..Facts::default() };
        assert_eq!(review_state(&land, &f), crate::review::State::NeedsReview);
        // The author's own accept doesn't count; an agent's doesn't unless allowed.
        f.reviews = vec![review("user:ada", 1, Action::Accept), review("agent:review", 1, Action::Accept)];
        assert!(f.accepting(&land).is_empty());
        assert_eq!(f.accepting(&Land { agents_can_approve: true, ..Land::default() }).len(), 1);
        // Bob accepts v1; v2 only rebased: still accepted (sticky), unless sticky is off.
        f.reviews.push(review("user:bob", 1, Action::Accept));
        f.rebase_only = vec![false, true];
        assert_eq!(review_state(&land, &f), crate::review::State::Accepted);
        assert_eq!(review_state(&Land { sticky_approvals: Sticky::None, ..Land::default() }, &f), crate::review::State::NeedsReview);
        // v3 changes the patch: v1's accept is gone.
        f.rebase_only = vec![false, true, false];
        assert_eq!(review_state(&land, &f), crate::review::State::NeedsReview);
        // A request on the latest version: changes requested; an answer (a newer version) clears it.
        f.reviews.push(review("user:cy", 3, Action::RequestChanges));
        assert_eq!(review_state(&land, &f), crate::review::State::ChangesRequested);
        f.rebase_only.push(false);
        assert!(f.requesting().is_empty());
        // Resigning withdraws a reviewer's standing.
        f.reviews.push(review("user:dee", 4, Action::Accept));
        assert_eq!(f.accepting(&land).len(), 1);
        f.reviews.push(review("user:dee", 4, Action::Resign));
        assert!(f.accepting(&land).is_empty());
    }

    #[test]
    fn w14_r09_blockers() -> Result<(), String> {
        let land = Land { checks: vec!["ci".into(), "lint".into()], ..Land::default() };
        let owners = parse_owners("- paths: [\"crates/**\"]\n  owners: [ada]\n- paths: [\"docs/**\"]\n  owners: [bob]\n  optional: true\n- paths: [\"crates/kiln/src/harness/**\"]\n  owners: [cy, team:agents]\n")?;
        let mut f = Facts {
            author: "user:dee".into(),
            rebase_only: vec![false],
            open_threads: 2,
            checks: [("ci".to_owned(), Some("success".to_owned())), ("lint".to_owned(), None)].into(),
            paths: vec!["crates/kiln/src/harness/a.rs".into(), "crates/x.rs".into(), "docs/a.md".into()],
            ..Facts::default()
        };
        let b = blockers(&land, &owners, &f);
        assert!(b.iter().any(|w| w.starts_with("0 of 1 accepts")), "{b:?}");
        assert!(b.iter().any(|w| w == "2 open threads") && b.iter().any(|w| w == "`lint` is still running"), "{b:?}");
        assert!(b.iter().any(|w| w.contains("crates/x.rs needs an accept from user:ada")), "{b:?}");
        assert!(b.iter().any(|w| w.contains("harness/a.rs needs an accept from user:cy, team:agents")), "the last matching rule: {b:?}");
        assert!(!b.iter().any(|w| w.contains("docs/")), "optional owners don't block: {b:?}");
        // Ada and Cy accept, threads resolved, lint passes: nothing left.
        f.reviews = vec![review("user:ada", 1, Action::Accept), review("user:cy", 1, Action::Accept)];
        f.open_threads = 0;
        f.checks.insert("lint".into(), Some("success".into()));
        assert_eq!(blockers(&land, &owners, &f), Vec::<String>::new());
        f.checks.insert("lint".into(), Some("failure".into()));
        assert_eq!(blockers(&land, &owners, &f), ["`lint` failure"]);
        f.checks.remove("lint");
        assert_eq!(blockers(&land, &owners, &f), ["`lint` hasn't run on this version"]);
        Ok(())
    }

    #[test]
    fn w14_r07_trailers_and_batches() {
        assert_eq!(landed_message("one: a\n\nbody\n\nChange-Id: I1\n", "D12", &["user:bob".into()], &[]), "one: a\n\nbody\n\nChange-Id: I1\nDifferential-Revision: D12\nReviewed-by: user:bob\n");
        assert_eq!(landed_message("fix", "CR4", &[], &[]), "fix\n\nChange-Request: CR4\n");
        // W26-R03: each landed change's trailer, once, beside one the commit already had.
        let kc = |s: &str| format!("kc-{s:0>16}");
        assert_eq!(landed_message("fix", "D3", &[], &[kc("a")]), format!("fix\n\nDifferential-Revision: D3\nKiln-Change: {}\n", kc("a")));
        let had = format!("fix\n\nKiln-Change: {}\n", kc("a"));
        assert_eq!(landed_message(&had, "D3", &[], &[kc("a"), kc("b")]), format!("fix\n\nKiln-Change: {}\nDifferential-Revision: D3\nKiln-Change: {}\n", kc("a"), kc("b")));
        assert_eq!((next_batch(4, 0), next_batch(4, 1), next_batch(4, 2), next_batch(4, 5), next_batch(0, 0)), (4, 2, 1, 1, 1));
    }
}
