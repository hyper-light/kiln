//! Ship's Pipelines (docs/ci.md W29-R02 to R04): what each workflow, reusable workflow and action
//! is, where it stands, its three pips and its place among its repository's rows, and the list's
//! JSON; and, for an opened row's pane, each trigger's group and words and each job's stage. Decided
//! from what the server gathers and passes in (`crates/kiln/src/ship/pipelines.rs`): nothing here
//! reads a clock, a disk or the network.

use std::cmp::Ordering;
use std::collections::HashMap;

use serde::Serialize;
use serde_json::{Value, json};

use crate::wf::ir::Trigger;
use crate::wf::{Action, Workflow};

/// What a row is: W29-R02's kind facet.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    #[default]
    Workflow,
    /// A workflow others call (`on: workflow_call`).
    Reusable,
    Composite,
    Javascript,
    Docker,
    /// kiln's: a microVM of its own for each call (W29-R29).
    Firecracker,
    /// kiln's: a shards microVM for each call (W29-R30).
    Shards,
    /// An `action.yml` whose `runs.using` kiln doesn't know.
    Unknown,
}

impl Kind {
    /// A workflow's kind: reusable when another workflow can call it.
    pub fn of_workflow(wf: &Workflow) -> Kind {
        if wf.trigger("workflow_call").is_some() { Kind::Reusable } else { Kind::Workflow }
    }

    /// An action's kind, from its `runs.using`: `node12` to `node24` are JavaScript, as GitHub
    /// runs them (W07-R05).
    pub fn of_action(action: &Action) -> Kind {
        let using = action.runs.using.trim().to_ascii_lowercase();
        match using.as_str() {
            "composite" => Kind::Composite,
            "docker" => Kind::Docker,
            "firecracker" => Kind::Firecracker,
            "shards" => Kind::Shards,
            u if u.strip_prefix("node").is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit())) => Kind::Javascript,
            _ => Kind::Unknown,
        }
    }

    pub fn is_action(self) -> bool {
        !matches!(self, Kind::Workflow | Kind::Reusable)
    }

    /// Its name in the JSON, as it serializes.
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Workflow => "workflow",
            Kind::Reusable => "reusable",
            Kind::Composite => "composite",
            Kind::Javascript => "javascript",
            Kind::Docker => "docker",
            Kind::Firecracker => "firecracker",
            Kind::Shards => "shards",
            Kind::Unknown => "unknown",
        }
    }

    /// What a row's line calls it.
    pub fn words(self) -> &'static str {
        match self {
            Kind::Workflow => "workflow",
            Kind::Reusable => "reusable workflow",
            Kind::Composite => "composite action",
            Kind::Javascript => "JavaScript action",
            Kind::Docker => "Docker action",
            Kind::Firecracker => "Firecracker action",
            Kind::Shards => "Shards action",
            Kind::Unknown => "action",
        }
    }
}

/// Where an event comes from, for the pane's "show: all · GitHub's · kiln's" (W29-R06).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Whose {
    /// GitHub's, which a repository kiln hosts sends too (Appendix A.6's native column).
    Both,
    /// GitHub's, passed through from GitHub as received.
    Github,
    /// kiln's own (W03-R09).
    Kiln,
}

/// The pane's groups of triggers: Add a trigger's six (W29-R06), and GitHub's other events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Group {
    Code,
    Time,
    People,
    Workflows,
    Releases,
    Outside,
    Repository,
}

impl Group {
    pub fn words(self) -> &'static str {
        match self {
            Group::Code => "Code",
            Group::Time => "Time",
            Group::People => "People and agents",
            Group::Workflows => "Other workflows",
            Group::Releases => "Deploys and releases",
            Group::Outside => "Outside kiln",
            Group::Repository => "The repository's other events",
        }
    }
}

/// An event's place in the pane: its words, its group and whose it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct EventKind {
    pub event: &'static str,
    pub words: &'static str,
    pub group: Group,
    pub whose: Whose,
}

const fn ev(event: &'static str, words: &'static str, group: Group, whose: Whose) -> EventKind {
    EventKind { event, words, group, whose }
}

/// Every event `on:` takes: GitHub's (`wf::keys::GHA_EVENTS`) and kiln's (`KILN_EVENTS`).
const EVENTS: &[EventKind] = &[
    ev("push", "push", Group::Code, Whose::Both),
    ev("pull_request", "pull request", Group::Code, Whose::Both),
    ev("pull_request_target", "pull request (target)", Group::Code, Whose::Both),
    ev("diff", "diff", Group::Code, Whose::Kiln),
    ev("land", "land", Group::Code, Whose::Kiln),
    ev("merge_group", "merge group", Group::Code, Whose::Both),
    ev("create", "create", Group::Code, Whose::Both),
    ev("delete", "delete", Group::Code, Whose::Both),
    ev("tag", "tag (v1)", Group::Code, Whose::Kiln),
    ev("schedule", "schedule", Group::Time, Whose::Both),
    ev("workflow_dispatch", "manual", Group::People, Whose::Both),
    ev("manual", "manual (v1)", Group::People, Whose::Kiln),
    ev("agent", "agent", Group::People, Whose::Kiln),
    ev("comment", "comment", Group::People, Whose::Kiln),
    ev("review", "review", Group::People, Whose::Kiln),
    ev("issue_comment", "issue comment", Group::People, Whose::Both),
    ev("pull_request_review", "pull request review", Group::People, Whose::Both),
    ev("pull_request_review_comment", "review comment", Group::People, Whose::Both),
    ev("workflow_run", "pipeline", Group::Workflows, Whose::Both),
    ev("pipeline", "pipeline", Group::Workflows, Whose::Kiln),
    ev("workflow_call", "called", Group::Workflows, Whose::Both),
    ev("deploy", "deploy", Group::Releases, Whose::Kiln),
    ev("deployment", "deployment", Group::Releases, Whose::Both),
    ev("deployment_status", "deployment status", Group::Releases, Whose::Both),
    ev("release", "release", Group::Releases, Whose::Both),
    ev("registry_package", "package", Group::Releases, Whose::Both),
    ev("image_version", "image version", Group::Releases, Whose::Github),
    ev("webhook", "webhook", Group::Outside, Whose::Kiln),
    ev("repository_dispatch", "dispatch", Group::Outside, Whose::Both),
    ev("branch_protection_rule", "branch protection rule", Group::Repository, Whose::Github),
    ev("check_run", "check run", Group::Repository, Whose::Github),
    ev("check_suite", "check suite", Group::Repository, Whose::Github),
    ev("discussion", "discussion", Group::Repository, Whose::Github),
    ev("discussion_comment", "discussion comment", Group::Repository, Whose::Github),
    ev("fork", "fork", Group::Repository, Whose::Github),
    ev("gollum", "wiki page", Group::Repository, Whose::Github),
    ev("issues", "issues", Group::Repository, Whose::Github),
    ev("label", "label", Group::Repository, Whose::Github),
    ev("milestone", "milestone", Group::Repository, Whose::Github),
    ev("page_build", "page build", Group::Repository, Whose::Github),
    ev("project", "project", Group::Repository, Whose::Github),
    ev("project_card", "project card", Group::Repository, Whose::Github),
    ev("project_column", "project column", Group::Repository, Whose::Github),
    ev("public", "made public", Group::Repository, Whose::Github),
    ev("status", "commit status", Group::Repository, Whose::Github),
    ev("watch", "star", Group::Repository, Whose::Github),
];

/// An event's place, or `None` for a name `on:` doesn't take (W02 reports it).
pub fn event_kind(event: &str) -> Option<EventKind> {
    EVENTS.iter().find(|k| k.event == event).copied()
}

/// One `on:` entry as the pane's Triggers group shows it: its kind, and what its filters say.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TriggerLine {
    pub event: String,
    pub words: String,
    pub group: Group,
    pub whose: Option<Whose>,
    /// Its filters, in a line: `branches main · paths crates/**`, `0 3 * * * UTC`, `after ci`.
    pub value: String,
}

/// A trigger's line in the pane (W29-R04's Triggers group).
pub fn trigger_line(t: &Trigger) -> TriggerLine {
    let kind = event_kind(&t.event);
    let mut parts: Vec<String> = Vec::new();
    let list = |name: &str, v: &Option<Vec<String>>, parts: &mut Vec<String>| {
        if let Some(v) = v.as_ref().filter(|v| !v.is_empty()) {
            parts.push(format!("{name} {}", v.join(", ")));
        }
    };
    if !t.types.is_empty() {
        parts.push(t.types.join(", "));
    }
    list("branches", &t.branches, &mut parts);
    list("not branches", &t.branches_ignore, &mut parts);
    list("tags", &t.tags, &mut parts);
    list("not tags", &t.tags_ignore, &mut parts);
    list("paths", &t.paths, &mut parts);
    list("not paths", &t.paths_ignore, &mut parts);
    for s in &t.schedules {
        parts.push(match &s.timezone {
            Some(tz) => format!("{} {tz}", s.cron),
            None => format!("{} UTC", s.cron),
        });
    }
    if !t.workflows.is_empty() {
        parts.push(format!("after {}", t.workflows.join(", ")));
    }
    if !t.inputs.is_empty() {
        parts.push(format!("inputs: {}", t.inputs.iter().map(|i| i.name.as_str()).collect::<Vec<_>>().join(", ")));
    }
    if !t.outputs.is_empty() {
        parts.push(format!("outputs: {}", t.outputs.iter().map(|o| o.name.as_str()).collect::<Vec<_>>().join(", ")));
    }
    if !t.secrets.is_empty() {
        parts.push(format!("secrets: {}", t.secrets.iter().map(|s| s.name.as_str()).collect::<Vec<_>>().join(", ")));
    }
    for (k, _) in &t.extra {
        parts.push(k.clone());
    }
    TriggerLine {
        event: t.event.clone(),
        words: kind.map_or_else(|| t.event.replace('_', " "), |k| k.words.to_owned()),
        group: kind.map_or(Group::Repository, |k| k.group),
        whose: kind.map(|k| k.whose),
        value: parts.join(" · "),
    }
}

/// A workflow's jobs by stage (W29-R04's Stage 1, 2…): a job that needs none is in the first,
/// and one that needs others is one past the furthest of them, each stage's jobs in the file's
/// order. Jobs whose `needs` go round in a cycle or name no job (the validator's KCI4002 and
/// KCI4001) come last, in a stage of their own.
pub fn stages(wf: &Workflow) -> Vec<Vec<String>> {
    let mut depth: HashMap<&str, usize> = HashMap::new();
    // Each pass places the jobs whose needs are all placed; a pass that places none ends it.
    loop {
        let mut placed = false;
        for job in &wf.jobs {
            if depth.contains_key(job.id.as_str()) {
                continue;
            }
            let mut furthest = 0;
            let mut ready = true;
            for need in &job.needs {
                match depth.get(need.job()) {
                    Some(d) => furthest = furthest.max(*d),
                    None => {
                        ready = false;
                        break;
                    }
                }
            }
            if ready {
                depth.insert(job.id.as_str(), furthest + 1);
                placed = true;
            }
        }
        if !placed {
            break;
        }
    }
    let deepest = depth.values().copied().max().unwrap_or(0);
    let mut out: Vec<Vec<String>> = vec![Vec::new(); deepest];
    let mut stuck = Vec::new();
    for job in &wf.jobs {
        match depth.get(job.id.as_str()).and_then(|d| d.checked_sub(1)).and_then(|i| out.get_mut(i)) {
            Some(stage) => stage.push(job.id.clone()),
            None => stuck.push(job.id.clone()),
        }
    }
    if !stuck.is_empty() {
        out.push(stuck);
    }
    out
}

/// A finished run's or a tag's checks' outcome, as the Latest pip shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Passed,
    Failed,
    Cancelled,
}

/// How far a row's draft has come (W29-R10's words).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Draft {
    /// Edits not yet committed.
    NotSaved,
    /// Saved, and not yet landed.
    Unpublished,
    /// Someone else's, asking the viewer to review it.
    Review,
    /// kiln's update draft (W29-R22).
    Update,
}

/// A row's draft as its row and its pane's head show it (W29-R10): how far it has come, whose it
/// is, and the change it's saved as.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DraftOf {
    pub state: Draft,
    pub author: String,
    pub change: Option<DraftChange>,
}

/// The change a draft is saved as (W29-R11): a kiln diff (`D12`), or a host's pull or merge
/// request (`#5`, `!5`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DraftChange {
    /// `diff` or `change`.
    pub kind: &'static str,
    pub name: String,
    pub url: Option<String>,
}

/// What the server knows of a row, at its repository's default-branch head.
#[derive(Debug, Clone, Default)]
pub struct Facts {
    pub kind: Kind,
    /// A workflow's `name:` or its file's stem; an action's `name:` or its folder's.
    pub name: String,
    pub path: String,
    /// Whether it runs where it is: a GitHub workflow a `.kiln/` workflow takes the place of
    /// doesn't (W02-R02).
    pub runs: bool,
    /// Its errors: W02's checks and the server's (KCI6007's labels, the action policy).
    pub errors: u32,
    /// Whether a machine connected now can take every job (W05-R20), where that was asked.
    pub routable: Option<bool>,
    /// Its newest finished run on the default branch (a workflow), or its newest tag's own
    /// checks (an action or a reusable workflow).
    pub latest: Option<Outcome>,
    /// Whether a run (or a tag's checks) is under way now.
    pub running: bool,
    /// Its newest full-version tag, for what's shared.
    pub released: Option<String>,
    /// Turned off (W29-R15).
    pub off: bool,
    pub draft: Option<Draft>,
    /// What an update draft takes it to, as its repository's heading says it: `ci-kit v2.4.0`
    /// (W29-R22).
    pub update: Option<String>,
}

/// Where a row stands: Ship's stand (W26's names), its word, its three pips (Checks, Runs on,
/// Latest) as the UI's glyphs are named, and its place in the needs-someone-first order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Standing {
    pub stand: &'static str,
    pub word: &'static str,
    pub pips: [&'static str; 3],
    /// 0: needs someone (Review, Failing, Can't run); 1: a draft; 2: the rest.
    pub rank: u8,
}

/// A row's standing (W29-R02).
pub fn standing(f: &Facts) -> Standing {
    let checks = if f.errors > 0 { "failed" } else { "passed" };
    let runs_on = match f.routable {
        Some(true) => "passed",
        Some(false) => "blocked",
        None => "none",
    };
    let latest = match (f.running, f.latest) {
        (true, _) => "running",
        (false, Some(Outcome::Passed)) => "passed",
        (false, Some(Outcome::Failed)) => "failed",
        (false, Some(Outcome::Cancelled) | None) => "none",
    };
    let (stand, word, rank) = match f.draft {
        Some(Draft::Review) => ("needs_you", "Review", 0),
        Some(Draft::NotSaved) => ("waiting", "Not saved", 1),
        Some(Draft::Unpublished) => ("waiting", "Unpublished", 1),
        Some(Draft::Update) => ("waiting", "Update", 1),
        None if f.off => ("superseded", "Off", 2),
        // A reusable workflow is called, not triggered: it's used wherever its file isn't run.
        None if !f.runs && f.kind == Kind::Workflow => ("superseded", "Not run here", 2),
        None if f.errors > 0 || f.routable == Some(false) => ("needs_you", "Can't run", 0),
        None if f.latest == Some(Outcome::Failed) => ("failing", "Failing", 0),
        None if f.released.is_some() => ("shipped", "Released", 2),
        None if f.latest == Some(Outcome::Passed) => ("shipped", "Passing", 2),
        None if f.kind.is_action() || f.kind == Kind::Reusable => ("waiting", "Not released", 2),
        None => ("waiting", "Not run yet", 2),
    };
    Standing { stand, word, pips: [checks, runs_on, latest], rank }
}

/// A full version's parts (W29-R21): `vMAJOR.MINOR.PATCH` or `MAJOR.MINOR.PATCH`, with a
/// pre-release after `-`; a floating `v2` or `v2.4` isn't one.
pub fn full_version(tag: &str) -> Option<(u64, u64, u64, Option<&str>)> {
    let t = tag.strip_prefix('v').unwrap_or(tag);
    let (core, pre) = match t.split_once('-') {
        Some((c, p)) if !p.is_empty() => (c, Some(p)),
        Some(_) => return None,
        None => (t, None),
    };
    let mut parts = core.split('.');
    let mut next = || parts.next().filter(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit())).and_then(|p| p.parse::<u64>().ok());
    let (major, minor, patch) = (next()?, next()?, next()?);
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch, pre))
}

/// A repository's full versions among its tags, newest first by semver's precedence: a release
/// comes after its own pre-releases (a tag is the whole repository's, W29-R21).
pub fn releases<'a>(tags: impl IntoIterator<Item = &'a str>) -> Vec<&'a str> {
    let mut out: Vec<((u64, u64, u64, bool, &str), &str)> = tags.into_iter().filter_map(|t| full_version(t).map(|(a, b, c, pre)| ((a, b, c, pre.is_none(), pre.unwrap_or_default()), t))).collect();
    out.sort_by(|(x, _), (y, _)| y.cmp(x));
    out.into_iter().map(|(_, t)| t).collect()
}

/// The newest full version among a repository's tags ([`releases`]' first).
pub fn newest_release<'a>(tags: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    releases(tags).into_iter().next()
}

/// Which of two rows comes first in W29-R02's order: what needs someone first (Review, then
/// Failing, then Can't run), then drafts, then the rest; each by name (any case), then by path.
fn before(fa: &Facts, sa: &Standing, fb: &Facts, sb: &Standing) -> Ordering {
    let need = |s: &Standing| match s.word {
        "Review" => 0,
        "Failing" => 1,
        "Can't run" => 2,
        _ => 3,
    };
    sa.rank.cmp(&sb.rank).then_with(|| need(sa).cmp(&need(sb))).then_with(|| fa.name.to_lowercase().cmp(&fb.name.to_lowercase())).then_with(|| fa.path.cmp(&fb.path))
}

/// A repository's rows in W29-R02's order.
pub fn order(rows: &mut [(Facts, Standing)]) {
    rows.sort_by(|(fa, sa), (fb, sb)| before(fa, sa, fb, sb));
}

/// A repository's heading words (W29-R02): `7 workflows · 1 failing · 1 waits on you`,
/// `shared · 6 actions · 1 workflow · 1 draft`, or `3 workflows · ci-kit v2.4.0 is out` (an update
/// draft's); and how many of its rows need someone.
pub fn heading(rows: &[(Facts, Standing)], shared: bool) -> (String, u32) {
    heading_of(rows.iter().map(|(f, s)| (f, s)), shared)
}

fn heading_of<'a>(rows: impl Iterator<Item = (&'a Facts, &'a Standing)> + Clone, shared: bool) -> (String, u32) {
    let count = |p: &dyn Fn(&Facts, &Standing) -> bool| rows.clone().filter(|(f, s)| p(f, s)).count();
    let workflows = count(&|f, _| !f.kind.is_action());
    let actions = count(&|f, _| f.kind.is_action());
    let drafts = count(&|f, _| matches!(f.draft, Some(Draft::NotSaved | Draft::Unpublished)));
    let mut out: Vec<&str> = rows.clone().filter(|(f, _)| f.draft == Some(Draft::Update)).filter_map(|(f, _)| f.update.as_deref()).collect();
    out.sort_unstable();
    out.dedup();
    let failing = count(&|_, s| s.stand == "failing");
    let waits = count(&|f, _| f.draft == Some(Draft::Review));
    let needs = count(&|_, s| s.rank == 0);
    let n = |k: usize, one: &str, many: &str| format!("{k} {}", if k == 1 { one } else { many });
    let mut parts: Vec<String> = Vec::new();
    if shared {
        parts.push("shared".to_owned());
    }
    // The larger count first: a library of actions reads as one.
    let mut kinds = [(workflows, "workflow", "workflows"), (actions, "action", "actions")];
    if actions > workflows {
        kinds.reverse();
    }
    parts.extend(kinds.iter().filter(|(k, ..)| *k > 0).map(|(k, one, many)| n(*k, one, many)));
    if drafts > 0 {
        parts.push(n(drafts, "draft", "drafts"));
    }
    parts.extend(out.iter().map(|u| format!("{u} is out")));
    if failing > 0 {
        parts.push(format!("{failing} failing"));
    }
    if waits > 0 {
        parts.push(n(waits, "waits on you", "wait on you"));
    }
    (parts.join(" · "), u32::try_from(needs).unwrap_or(u32::MAX))
}

/// A row as the list shows it: its facts and where it stands, and what the server read besides.
#[derive(Debug, Clone)]
pub struct Row {
    pub facts: Facts,
    pub standing: Standing,
    /// A workflow's dialect: `gha` or `kiln`.
    pub dialect: Option<&'static str>,
    pub line: String,
    /// Who changed its file last, and when (seconds).
    pub who: Option<String>,
    pub when: Option<i64>,
    pub draft: Option<DraftOf>,
}

impl Row {
    pub fn new(facts: Facts, dialect: Option<&'static str>, line: String, who: Option<String>, when: Option<i64>) -> Row {
        let standing = standing(&facts);
        Row { facts, standing, dialect, line, who, when, draft: None }
    }

    /// The row of a draft (W29-R10): its facts take the draft's state, and its stand follows.
    pub fn drafted(mut self, d: DraftOf) -> Row {
        self.facts.draft = Some(d.state);
        self.standing = standing(&self.facts);
        self.draft = Some(d);
        self
    }

    /// The row as `viewer` sees it, where that differs: W29-R10's Review is someone else's draft
    /// waiting on you, so to its author a draft in review is still theirs, Unpublished.
    pub fn seen_by(&self, viewer: &str) -> Option<Row> {
        let d = self.draft.as_ref().filter(|d| d.state == Draft::Review && d.author == viewer)?;
        Some(self.clone().drafted(DraftOf { state: Draft::Unpublished, ..d.clone() }))
    }
}

/// A repository's rows at its default branch's head, as gathered: each row's JSON and the
/// repository's whole are rendered once here, so the list answers from memory by joining them.
#[derive(Debug, Clone)]
pub struct Repo {
    pub repo: String,
    pub branch: String,
    pub sha: String,
    /// In W29-R02's order.
    pub rows: Vec<Row>,
    /// Each row's JSON, in the same order.
    pub rendered: Vec<String>,
    /// Its heading's words, and how many of its rows need someone.
    pub say: String,
    pub needs: u32,
    /// Its JSON with every row (W29-R03's `ShipPipelineRepo`).
    pub whole: String,
    /// What it shows, hashed: a gather that changes it sends `pipelines`.
    pub digest: u64,
    /// Whose drafts in review it has: each of them sees it otherwise ([`Repo::seen_by`]).
    pub authors: Vec<String>,
}

impl Repo {
    pub fn new(repo: String, branch: String, sha: String, mut rows: Vec<Row>) -> Repo {
        order_rows(&mut rows);
        let rendered: Vec<String> = rows.iter().map(|r| row_json(&repo, &branch, &sha, r).to_string()).collect();
        // W29-R17's Access is the master plan's step 5: until then nothing is shared.
        let (say, needs) = heading_of(rows.iter().map(|r| (&r.facts, &r.standing)), false);
        let mut authors: Vec<String> = rows.iter().filter_map(|r| r.draft.as_ref()).filter(|d| d.state == Draft::Review).map(|d| d.author.clone()).collect();
        authors.sort_unstable();
        authors.dedup();
        let mut r = Repo { repo, branch, sha, rows, rendered, say, needs, whole: String::new(), digest: 0, authors };
        r.whole = r.text(r.rendered.iter().map(String::as_str));
        r.digest = fnv(FNV_START, r.whole.as_bytes());
        r
    }

    /// The repository as `viewer` sees it, where that differs from what everyone else sees: a
    /// draft of theirs in review ([`Row::seen_by`]), with its order and heading.
    pub fn seen_by(&self, viewer: &str) -> Option<Repo> {
        if !self.authors.iter().any(|a| a == viewer) {
            return None;
        }
        let rows = self.rows.iter().map(|r| r.seen_by(viewer).unwrap_or_else(|| r.clone())).collect();
        Some(Repo::new(self.repo.clone(), self.branch.clone(), self.sha.clone(), rows))
    }

    /// Its JSON with these of its rows' JSON.
    fn text<'a>(&self, rows: impl Iterator<Item = &'a str>) -> String {
        let head = json!({ "repo": self.repo, "say": self.say, "needs": self.needs, "access": "none", "ref": self.branch, "commit": self.sha }).to_string();
        let mut out = String::with_capacity(head.len() + self.rendered.iter().map(String::len).sum::<usize>() + self.rendered.len() + 16);
        out.push_str(head.strip_suffix('}').unwrap_or(&head));
        out.push_str(",\"rows\":[");
        for (i, r) in rows.enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(r);
        }
        out.push_str("]}");
        out
    }
}

/// Rows in W29-R02's order.
pub fn order_rows(rows: &mut [Row]) {
    rows.sort_by(|a, b| before(&a.facts, &a.standing, &b.facts, &b.standing));
}

/// FNV-1a, 64 bits, over `bytes`, as Ship's rows' `v` is made.
fn fnv(h: u64, bytes: &[u8]) -> u64 {
    bytes.iter().fold(h, |h, b| (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3))
}

const FNV_START: u64 = 0xcbf2_9ce4_8422_2325;

/// A row's JSON, with `v`: a hash of what it shows, so the page can keep a row that hasn't changed
/// as it is (W26-R06's).
pub fn row_json(repo: &str, branch: &str, sha: &str, r: &Row) -> Value {
    let mut v = json!({
        "key": format!("{repo}:{}", r.facts.path),
        "kind": r.facts.kind.as_str(),
        "dialect": r.dialect,
        "name": r.facts.name,
        "path": r.facts.path,
        "stand": r.standing.stand,
        "word": r.standing.word,
        "line": r.line,
        "pips": r.standing.pips,
        "who": r.who,
        "when": r.when,
        "ref": branch,
        "commit": sha,
        "released": r.facts.released,
        "draft": serde_json::to_value(&r.draft).unwrap_or(Value::Null),
    });
    let h = fnv(FNV_START, v.to_string().as_bytes());
    if let Some(o) = v.as_object_mut() {
        o.insert("v".into(), json!(format!("{h:016x}")));
    }
    v
}

/// Whether a row is one `q` (lowercased) asks for: its name, path, kind or word has it, any case.
fn row_matches(r: &Row, q: &str) -> bool {
    [r.facts.name.as_str(), r.facts.path.as_str(), r.facts.kind.as_str(), r.standing.word].iter().any(|v| v.to_lowercase().contains(q))
}

/// The list's JSON (W29-R03, `{ repos: ShipPipelineRepo[] }`): the repositories the caller may read
/// (`only`: their names, or `None` for every one), `repo` alone when it's given, and, with `q`
/// (lowercased), the rows it asks for; those with rows needing someone first, then by name. What
/// was rendered when the rows were gathered is joined as it is, but for a repository `viewer` sees
/// otherwise ([`Repo::seen_by`]).
pub fn list_text<'a>(repos: impl IntoIterator<Item = &'a Repo>, only: Option<&[String]>, repo: Option<&str>, q: Option<&str>, viewer: Option<&str>) -> String {
    let mut out: Vec<(bool, &str, std::borrow::Cow<'a, str>)> = Vec::new();
    for r in repos.into_iter().filter(|r| only.is_none_or(|names| names.iter().any(|n| n == &r.repo))).filter(|r| repo.is_none_or(|want| want == r.repo)) {
        let (needs, text) = match viewer.and_then(|v| r.seen_by(v)) {
            Some(own) => match text_of(&own, q) {
                Some(t) => (own.needs, std::borrow::Cow::Owned(t.into_owned())),
                None => continue,
            },
            None => match text_of(r, q) {
                Some(t) => (r.needs, t),
                None => continue,
            },
        };
        out.push((needs > 0, r.repo.as_str(), text));
    }
    out.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
    let mut body = String::with_capacity(out.iter().map(|(_, _, t)| t.len() + 1).sum::<usize>() + 16);
    body.push_str("{\"repos\":[");
    for (i, (_, _, t)) in out.iter().enumerate() {
        if i > 0 {
            body.push(',');
        }
        body.push_str(t);
    }
    body.push_str("]}");
    body
}

/// A repository's JSON in the list: whole, or (`q`) with the rows it asks for, none if none does.
fn text_of<'b>(r: &'b Repo, q: Option<&str>) -> Option<std::borrow::Cow<'b, str>> {
    match q {
        None => Some(std::borrow::Cow::Borrowed(r.whole.as_str())),
        Some(q) if r.repo.to_lowercase().contains(q) => Some(std::borrow::Cow::Borrowed(r.whole.as_str())),
        Some(q) => {
            let rows: Vec<&str> = r.rows.iter().zip(&r.rendered).filter(|(row, _)| row_matches(row, q)).map(|(_, j)| j.as_str()).collect();
            (!rows.is_empty()).then(|| std::borrow::Cow::Owned(r.text(rows.into_iter())))
        }
    }
}

/// The secrets and variables a workflow's or action's text names in its expressions
/// (`${{ secrets.TOKEN }}`, `${{ vars.REGION }}`), each once and sorted: the pane shows names only,
/// never a value (W29-R04).
pub fn names_used(text: &str) -> (Vec<String>, Vec<String>) {
    let mut secrets = std::collections::BTreeSet::new();
    let mut vars = std::collections::BTreeSet::new();
    let mut rest = text;
    while let Some(open) = rest.find("${{") {
        let after = rest.get(open + 3..).unwrap_or_default();
        let close = after.find("}}").unwrap_or(after.len());
        let body = after.get(..close).unwrap_or_default();
        for (root, into) in [("secrets.", &mut secrets), ("vars.", &mut vars)] {
            let mut b = body;
            while let Some(at) = b.find(root) {
                // A root, not the end of another name (`mysecrets.x`).
                let before = b.get(..at).and_then(|x| x.chars().next_back());
                let name: String = b.get(at + root.len()..).unwrap_or_default().chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-').collect();
                if !before.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.') && !name.is_empty() {
                    into.insert(name);
                }
                b = b.get(at + root.len()..).unwrap_or_default();
            }
        }
        rest = after.get(close..).unwrap_or_default();
    }
    (secrets.into_iter().collect(), vars.into_iter().collect())
}

/// Where a `uses:` comes from (W07-R01): a file of this repository, a container image, one of
/// kiln's own actions, a repository kiln hosts (`kiln://`), or one fetched by name (GitHub's, or
/// wherever `[[ci.actions.sources]]` sends its owner).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UseFrom {
    Here,
    Image,
    Kiln,
    Hosted,
    Named,
}

impl UseFrom {
    pub fn words(self) -> &'static str {
        match self {
            UseFrom::Here => "this repository",
            UseFrom::Image => "a container image",
            UseFrom::Kiln => "kiln's own",
            UseFrom::Hosted => "a repository kiln hosts",
            UseFrom::Named => "fetched by name",
        }
    }
}

pub fn use_from(uses: &str) -> UseFrom {
    let u = uses.trim();
    if u.starts_with("./") || u.starts_with("$/") {
        UseFrom::Here
    } else if u.starts_with("docker://") {
        UseFrom::Image
    } else if u.starts_with("kiln://") {
        UseFrom::Hosted
    } else if u.starts_with("kiln/") {
        UseFrom::Kiln
    } else {
        UseFrom::Named
    }
}

/// A `uses:`'s ref, if it pins one: `owner/repo@v4` is `v4`.
pub fn use_ref(uses: &str) -> Option<&str> {
    uses.rsplit_once('@').map(|(_, r)| r).filter(|r| !r.is_empty())
}

/// One line of a pane's group (W29-R04's `ShipPipelineLine`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Line {
    pub key: String,
    /// `trigger`, `job`, `use`, `secret`, `variable`, `run`, `setting`, `input`, `output`, `step`,
    /// `entry`, `image`, `version` or `note`.
    pub kind: &'static str,
    pub name: String,
    pub value: String,
    /// A glyph's name, where the line has a state (a run's outcome; `none` for what isn't here yet).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<&'static str>,
    /// A trigger's group, and whose it is (W29-R06).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub whose: Option<Whose>,
}

impl Line {
    fn new(key: impl Into<String>, kind: &'static str, name: impl Into<String>, value: impl Into<String>) -> Line {
        Line { key: key.into(), kind, name: name.into(), value: value.into(), state: None, group: None, whose: None }
    }

    fn state(mut self, s: &'static str) -> Line {
        self.state = Some(s);
        self
    }
}

/// A group of a pane: `Triggers`, `Stage 1`, `Inputs` (W29-R04).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PaneGroup {
    pub name: String,
    pub lines: Vec<Line>,
}

fn group(name: impl Into<String>, lines: Vec<Line>) -> PaneGroup {
    PaneGroup { name: name.into(), lines }
}

/// A run as the pane's Runs group shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunSeen {
    pub number: i64,
    pub event: String,
    pub outcome: Option<Outcome>,
    pub running: bool,
}

/// A job's line: its name, where it runs (its labels, and bare metal unless a container is
/// asked for) or the workflow it calls, and what it needs.
fn job_line(j: &crate::wf::Job) -> Line {
    let mut value = match (&j.uses, &j.runs_on) {
        (Some(u), _) => format!("calls {u}"),
        (None, Some(crate::wf::ir::RunsOn::Labels(l))) => l.iter().map(|t| t.src.as_str()).collect::<Vec<_>>().join(", "),
        (None, Some(crate::wf::ir::RunsOn::Group { group, labels })) => {
            let labels = labels.iter().map(|t| t.src.as_str()).collect::<Vec<_>>().join(", ");
            match group {
                Some(g) if labels.is_empty() => format!("group {}", g.src),
                Some(g) => format!("group {} · {labels}", g.src),
                None => labels,
            }
        }
        (None, None) => String::new(),
    };
    if j.uses.is_none() {
        let machine = if j.container.is_some() || j.kiln.image.is_some() { "in a container" } else { "bare metal" };
        value = if value.is_empty() { machine.to_owned() } else { format!("{value} · {machine}") };
    }
    if !j.needs.is_empty() {
        value.push_str(&format!(" · needs {}", j.needs.iter().map(|n| n.job()).collect::<Vec<_>>().join(", ")));
    }
    Line::new(format!("jobs.{}", j.id), "job", j.name.as_ref().map_or_else(|| j.id.clone(), |n| n.src.clone()), value)
}

fn outcome_glyph(outcome: Option<Outcome>, running: bool) -> &'static str {
    match (running, outcome) {
        (true, _) => "running",
        (false, Some(Outcome::Passed)) => "passed",
        (false, Some(Outcome::Failed)) => "failed",
        (false, Some(Outcome::Cancelled) | None) => "none",
    }
}

/// A workflow's groups (W29-R04): Triggers, its stages, Uses, Secrets and variables (names only),
/// Runs, Settings, and a reusable workflow's interface.
pub fn workflow_groups(w: &Workflow, text: &str, runs: &[RunSeen], runs_here: bool, note: &str) -> Vec<PaneGroup> {
    let mut out = Vec::new();
    let triggers: Vec<Line> = w
        .on
        .iter()
        .map(|t| {
            let l = trigger_line(t);
            Line { key: format!("on.{}", t.event), kind: "trigger", name: l.words, value: l.value, state: None, group: Some(l.group.words()), whose: l.whose }
        })
        .collect();
    out.push(group("Triggers", triggers));
    if let Some(call) = w.trigger("workflow_call") {
        let mut lines: Vec<Line> = call
            .inputs
            .iter()
            .map(|i| {
                let mut v = i.ty.clone().unwrap_or_else(|| "string".into());
                v.push_str(if i.required { " · required" } else { "" });
                Line::new(format!("call.inputs.{}", i.name), "input", i.name.clone(), v)
            })
            .collect();
        lines.extend(call.outputs.iter().map(|o| Line::new(format!("call.outputs.{}", o.name), "output", o.name.clone(), o.value.src.clone())));
        lines.extend(call.secrets.iter().map(|x| Line::new(format!("call.secrets.{}", x.name), "secret", x.name.clone(), if x.required { "required" } else { "optional" })));
        out.push(group("Called with", lines));
    }
    for (i, ids) in stages(w).iter().enumerate() {
        let lines = ids.iter().filter_map(|id| w.job(id)).map(job_line).collect();
        out.push(group(format!("Stage {}", i + 1), lines));
    }
    let mut uses: Vec<Line> = Vec::new();
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for j in &w.jobs {
        let calls = j.uses.iter().map(|u| (u.as_str(), true));
        let steps = j.steps.iter().filter_map(|s| s.uses.as_deref()).map(|u| (u, false));
        for (u, call) in calls.chain(steps) {
            if !seen.insert(u) {
                continue;
            }
            let mut value = use_from(u).words().to_owned();
            if call {
                value = format!("a reusable workflow, {value}");
            }
            if let Some(r) = use_ref(u) {
                value.push_str(&format!(" · {r}"));
            }
            uses.push(Line::new(format!("uses.{u}"), "use", u, value));
        }
    }
    out.push(group("Uses", uses));
    let (secrets, vars) = names_used(text);
    let mut declared: Vec<String> = w.secrets.iter().map(|x| x.name.clone()).collect();
    declared.extend(w.trigger("workflow_call").map(|t| t.secrets.iter().map(|x| x.name.clone()).collect::<Vec<_>>()).unwrap_or_default());
    let mut names: Vec<Line> = Vec::new();
    for n in secrets.iter().chain(declared.iter().filter(|d| !secrets.contains(d))) {
        let value = match (secrets.contains(n), declared.contains(n)) {
            (true, true) => "declared, used",
            (false, true) => "declared",
            _ => "used",
        };
        names.push(Line::new(format!("secrets.{n}"), "secret", n.clone(), value));
    }
    names.extend(vars.iter().map(|n| Line::new(format!("vars.{n}"), "variable", n.clone(), "used")));
    out.push(group("Secrets and variables", names));
    let runs_lines: Vec<Line> = runs
        .iter()
        .map(|r| {
            let status = match (r.running, r.outcome) {
                (true, _) => "under way",
                (false, Some(Outcome::Passed)) => "passed",
                (false, Some(Outcome::Failed)) => "failed",
                (false, Some(Outcome::Cancelled)) => "cancelled",
                (false, None) => "queued",
            };
            Line::new(format!("runs.{}", r.number), "run", format!("#{}", r.number), format!("{} · {status}", r.event)).state(outcome_glyph(r.outcome, r.running))
        })
        .collect();
    out.push(group("Runs", runs_lines));
    let mut settings = vec![if runs_here { Line::new("settings.on", "setting", "Turned on", "runs on its triggers") } else { Line::new("settings.on", "setting", "Not run here", note) }];
    settings.push(Line::new("settings.machines", "setting", "Machines", "bare metal unless a job says"));
    if let Some(c) = &w.concurrency {
        settings.push(Line::new("settings.concurrency", "setting", "Concurrency", c.group.src.clone()));
    }
    out.push(group("Settings", settings));
    out
}

/// What a microVM action's `runs` says beyond what `wf::Action` reads (W29-R29, R30's `resources`
/// and `egress`), from its text: each `None` when not given.
fn microvm_keys(text: &str) -> (Option<String>, Option<String>) {
    let Ok(doc) = serde_yaml::from_str::<serde_yaml::Value>(text) else { return (None, None) };
    let runs = doc.get("runs");
    let resources = runs.and_then(|r| r.get("resources")).and_then(|v| v.as_mapping()).map(|m| {
        m.iter().filter_map(|(k, v)| Some(format!("{} {}", k.as_str()?, yaml_scalar(v)?))).collect::<Vec<_>>().join(", ")
    });
    let egress = runs.and_then(|r| r.get("egress")).and_then(|v| v.as_sequence()).map(|s| s.iter().filter_map(yaml_scalar).collect::<Vec<_>>().join(", "));
    (resources.filter(|s| !s.is_empty()), egress.filter(|s| !s.is_empty()))
}

fn yaml_scalar(v: &serde_yaml::Value) -> Option<String> {
    match v {
        serde_yaml::Value::String(s) => Some(s.clone()),
        serde_yaml::Value::Number(n) => Some(n.to_string()),
        serde_yaml::Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// An action's groups (W29-R04): Inputs and Outputs, then its kind's own (a composite's steps, a
/// JavaScript action's entry points, a Docker action's image, a microVM's image, resources and
/// egress), its Versions (`tags`: newest first, each with its commit), and what comes with
/// sharing (Used by, Access: W29-R17, R20).
pub fn action_groups(a: &Action, kind: Kind, text: &str, head: &str, tags: &[(String, String)]) -> Vec<PaneGroup> {
    let mut out = Vec::new();
    let inputs = a
        .inputs
        .iter()
        .map(|i| {
            let mut v = if i.required { "required".to_owned() } else { i.default.as_ref().map_or_else(|| "optional".to_owned(), |d| format!("default {}", d.src)) };
            if let Some(d) = i.description.as_deref().map(str::trim).filter(|d| !d.is_empty()) {
                v.push_str(&format!(" · {d}"));
            }
            Line::new(format!("inputs.{}", i.name), "input", i.name.clone(), v)
        })
        .collect();
    out.push(group("Inputs", inputs));
    out.push(group("Outputs", a.outputs.iter().map(|o| Line::new(format!("outputs.{}", o.name), "output", o.name.clone(), o.description.clone().unwrap_or_default())).collect()));
    let r = &a.runs;
    match kind {
        Kind::Composite => {
            let steps = r
                .steps
                .iter()
                .enumerate()
                .map(|(i, st)| {
                    let name = st.name.as_ref().map(|n| n.src.clone()).or_else(|| st.id.clone()).unwrap_or_else(|| format!("step {}", i + 1));
                    let value = st.uses.clone().or_else(|| st.run.as_ref().map(|x| x.src.lines().next().unwrap_or_default().to_owned())).unwrap_or_default();
                    Line::new(format!("runs.steps.{i}"), "step", name, value)
                })
                .collect();
            out.push(group("Steps", steps));
        }
        Kind::Javascript => {
            let lines = [("main", &r.main), ("pre", &r.pre), ("post", &r.post)].into_iter().filter_map(|(k, v)| v.as_ref().map(|f| Line::new(format!("runs.{k}"), "entry", k, f.clone()))).collect();
            out.push(group(format!("Runs on Node ({})", r.using), lines));
        }
        Kind::Docker => {
            let mut lines = vec![Line::new("runs.image", "image", "image", r.image.clone().unwrap_or_default())];
            lines.extend(r.entrypoint.as_ref().map(|e| Line::new("runs.entrypoint", "entry", "entrypoint", e.clone())));
            if !r.args.is_empty() {
                lines.push(Line::new("runs.args", "entry", "args", r.args.iter().map(|t| t.src.as_str()).collect::<Vec<_>>().join(" ")));
            }
            out.push(group("Image", lines));
        }
        Kind::Firecracker | Kind::Shards => {
            let (resources, egress) = microvm_keys(text);
            let mut lines = vec![Line::new("runs.image", "image", "image", r.image.clone().unwrap_or_default())];
            lines.push(Line::new("runs.resources", "setting", "resources", resources.unwrap_or_else(|| "as the leg's".into())));
            lines.push(Line::new("runs.egress", "setting", "egress", egress.unwrap_or_else(|| "none".into())));
            out.push(group(if kind == Kind::Firecracker { "Firecracker microVM" } else { "Shards microVM" }, lines));
        }
        Kind::Workflow | Kind::Reusable | Kind::Unknown => {}
    }
    let mut versions = vec![Line::new("versions.head", "version", "the default branch's tip", head.get(..12).unwrap_or(head))];
    versions.extend(tags.iter().map(|(t, sha)| Line::new(format!("versions.{t}"), "version", t.clone(), sha.get(..12).unwrap_or(sha))));
    out.push(group("Versions", versions));
    out.push(group("Used by", vec![Line::new("used_by", "note", "Who uses it", "comes with sharing (W29-R20)").state("none")]));
    out.push(group("Settings", vec![Line::new("settings.access", "setting", "Access", "not shared: Access comes with sharing (W29-R17)").state("none")]));
    out
}

/// A strip's verb (W29-R04's `ShipPipelineVerb`): dimmed, with why, until it's built; never
/// hidden.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Verb {
    pub verb: &'static str,
    pub label: &'static str,
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub why_not: Option<&'static str>,
}

/// The strip's verbs for a row of `kind` (W29-R04): Run, Save draft, Ready and Discard, and
/// Release for an action or reusable workflow. None is built yet (the master plan's steps 4 to 6),
/// so each says when it comes.
pub fn verbs(kind: Kind) -> Vec<Verb> {
    let dim = |verb, label, why| Verb { verb, label, enabled: false, why_not: Some(why) };
    let mut out = vec![
        dim("run", "Run", "Run by hand comes with drafts (W29-R14)"),
        dim("save_draft", "Save draft", "Drafts come next (W29-R10, R11)"),
        dim("ready", "Ready", "Drafts come next (W29-R12)"),
        dim("discard", "Discard", "Drafts come next (W29-R13)"),
    ];
    if kind.is_action() || kind == Kind::Reusable {
        out.push(dim("release", "Release", "Releases by tag come after sharing (W29-R21)"));
    }
    out
}

/// Who looks at a pane, for its verbs: their name, and whether they may submit changes to its
/// repository (`diffs.submit`).
#[derive(Debug, Clone, Copy)]
pub struct Viewer<'a> {
    pub name: &'a str,
    pub can_submit: bool,
}

/// The strip's verbs for `row` as `viewer` sees it, once drafts are built (W29-R04, R10 to R14),
/// each there or dimmed with why:
/// - Run: its author's draft, a workflow that runs by hand (`by_hand`: it has `workflow_dispatch`,
///   or kiln's `manual`), or an action's Try it (W21-R07);
/// - Save draft: whoever may submit;
/// - Ready and Discard: the draft's own author;
/// - Release, for what's shared: dimmed until releases are built (W29-R21).
pub fn verbs_for(row: &Row, viewer: Viewer<'_>, by_hand: bool) -> Vec<Verb> {
    let on = |verb: &'static str, label: &'static str| Verb { verb, label, enabled: true, why_not: None };
    let dim = |verb: &'static str, label: &'static str, why: &'static str| Verb { verb, label, enabled: false, why_not: Some(why) };
    let kind = row.facts.kind;
    let draft = row.draft.as_ref();
    let mine = draft.is_some_and(|d| d.author == viewer.name);
    let update = draft.is_some_and(|d| d.state == Draft::Update);
    let run = match draft {
        Some(_) if mine => on("run", "Run"),
        Some(_) => dim("run", "Run", "A draft runs only by its author's hand (W29-R14)"),
        None if kind == Kind::Reusable => dim("run", "Run", "A reusable workflow runs where it's called (W29-R14)"),
        None if kind == Kind::Workflow && !by_hand => dim("run", "Run", "It has no workflow_dispatch or manual trigger to run it by hand (W29-R14)"),
        None => on("run", "Run"),
    };
    let save = if viewer.can_submit { on("save_draft", "Save draft") } else { dim("save_draft", "Save draft", "Saving a draft needs diffs.submit (W29-R11)") };
    let ready = match draft {
        None => dim("ready", "Ready", "Ready is for a draft (W29-R12)"),
        Some(_) if update => dim("ready", "Ready", "kiln's update drafts come with releases (W29-R22)"),
        Some(_) if !mine => dim("ready", "Ready", "Only its author marks it ready (W29-R12)"),
        Some(d) if d.state == Draft::Review => dim("ready", "Ready", "It's in review already (W29-R12)"),
        Some(_) => on("ready", "Ready"),
    };
    let discard = match draft {
        None => dim("discard", "Discard", "Discard is for a draft (W29-R13)"),
        Some(_) if update => dim("discard", "Discard", "kiln's update drafts come with releases (W29-R22)"),
        Some(_) if !mine => dim("discard", "Discard", "Only its author discards it (W29-R13)"),
        Some(_) => on("discard", "Discard"),
    };
    let mut out = vec![run, save, ready, discard];
    if kind.is_action() || kind == Kind::Reusable {
        out.push(dim("release", "Release", "Releases by tag come after sharing (W29-R21)"));
    }
    out
}

/// What the pane's head and detail say of the opened file (W29-R04).
#[derive(Debug, Clone)]
pub struct Opened<'a> {
    pub repo: &'a str,
    pub row: &'a Row,
    pub at: &'a str,
    pub sha: &'a str,
    pub text: &'a str,
}

/// An opened row's pane (W29-R04's `ShipPipelinePane { head, strip, groups, detail }`).
pub fn pane_json(o: &Opened<'_>, groups: &[PaneGroup]) -> Value {
    let f = &o.row.facts;
    let views: &[&str] = if f.kind.is_action() { &["Changes", "YAML", "Try it"] } else { &["YAML", "Flow", "Plan"] };
    json!({
        "head": {
            "title": f.name, "path": f.path, "kind": f.kind.as_str(), "dialect": o.row.dialect,
            "ref": o.at, "commit": o.sha, "draft": serde_json::to_value(&o.row.draft).unwrap_or(Value::Null),
            "stand": o.row.standing.stand, "word": o.row.standing.word, "pips": o.row.standing.pips,
        },
        "strip": {
            "said": [format!("{} · {}", f.kind.words(), o.row.line)],
            "pick": ["all"],
            "verbs": verbs(f.kind),
        },
        "groups": groups,
        "detail": { "name": f.path, "views": views, "text": o.text },
        "repo": o.repo,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wf::ir::{ActionRuns, Job, Need, Schedule};
    use crate::wf::keys::{GHA_EVENTS, KILN_EVENTS};

    fn action(using: &str) -> Action {
        Action { runs: ActionRuns { using: using.to_owned(), ..ActionRuns::default() }, ..Action::default() }
    }

    fn job(id: &str, needs: &[&str]) -> Job {
        Job { id: id.to_owned(), needs: needs.iter().map(|n| Need::Job((*n).to_owned())).collect(), ..Job::default() }
    }

    #[test]
    fn w29_r06_every_trigger_kind_has_its_place() {
        for event in GHA_EVENTS.iter().chain(KILN_EVENTS) {
            let k = event_kind(event);
            assert!(k.is_some(), "`{event}` has no place in the pane");
            let kiln = KILN_EVENTS.contains(event);
            assert_eq!(k.map(|k| k.whose == Whose::Kiln), Some(kiln), "`{event}`: kiln's are kiln's alone");
        }
        for k in EVENTS {
            assert!(GHA_EVENTS.contains(&k.event) || KILN_EVENTS.contains(&k.event), "`{}` isn't an event `on:` takes", k.event);
        }
        assert_eq!(EVENTS.len(), GHA_EVENTS.len() + KILN_EVENTS.len(), "each once");
        assert_eq!(event_kind("not_an_event"), None);
    }

    #[test]
    fn w29_r06_add_a_trigger_groups() {
        let group = |e: &str| event_kind(e).map(|k| k.group);
        for (event, g) in [("push", Group::Code), ("diff", Group::Code), ("merge_group", Group::Code), ("schedule", Group::Time), ("workflow_dispatch", Group::People), ("agent", Group::People), ("workflow_run", Group::Workflows), ("workflow_call", Group::Workflows), ("deploy", Group::Releases), ("registry_package", Group::Releases), ("webhook", Group::Outside), ("repository_dispatch", Group::Outside), ("watch", Group::Repository)] {
            assert_eq!(group(event), Some(g), "{event}");
        }
    }

    #[test]
    fn w29_r04_a_triggers_line_says_its_filters() {
        let push = Trigger { event: "push".into(), branches: Some(vec!["main".into()]), paths: Some(vec!["crates/**".into()]), ..Trigger::default() };
        let l = trigger_line(&push);
        assert_eq!((l.words.as_str(), l.group, l.value.as_str()), ("push", Group::Code, "branches main · paths crates/**"));
        let nightly = Trigger { event: "schedule".into(), schedules: vec![Schedule { cron: "0 3 * * *".into(), ..Schedule::default() }, Schedule { cron: "0 12 * * 1".into(), timezone: Some("Europe/Oslo".into()), ..Schedule::default() }], ..Trigger::default() };
        assert_eq!(trigger_line(&nightly).value, "0 3 * * * UTC · 0 12 * * 1 Europe/Oslo");
        let after = Trigger { event: "workflow_run".into(), types: vec!["completed".into()], workflows: vec!["ci".into()], ..Trigger::default() };
        assert_eq!(trigger_line(&after).value, "completed · after ci");
        let odd = trigger_line(&Trigger { event: "made_up".into(), ..Trigger::default() });
        assert_eq!((odd.words.as_str(), odd.group, odd.whose), ("made up", Group::Repository, None));
    }

    #[test]
    fn w29_r02_every_action_kind() {
        for (using, kind) in [("node12", Kind::Javascript), ("node16", Kind::Javascript), ("node20", Kind::Javascript), ("node24", Kind::Javascript), ("composite", Kind::Composite), ("docker", Kind::Docker), ("Docker", Kind::Docker), ("firecracker", Kind::Firecracker), ("shards", Kind::Shards), ("node", Kind::Unknown), ("nodejs", Kind::Unknown), ("", Kind::Unknown), ("wasm", Kind::Unknown)] {
            assert_eq!(Kind::of_action(&action(using)), kind, "{using:?}");
        }
        for k in [Kind::Composite, Kind::Javascript, Kind::Docker, Kind::Firecracker, Kind::Shards, Kind::Unknown] {
            assert!(k.is_action(), "{k:?}");
        }
        assert!(!Kind::Workflow.is_action() && !Kind::Reusable.is_action());
        let called = Workflow { on: vec![Trigger { event: "workflow_call".into(), ..Trigger::default() }], ..Workflow::default() };
        assert_eq!(Kind::of_workflow(&called), Kind::Reusable);
        assert_eq!(Kind::of_workflow(&Workflow::default()), Kind::Workflow);
    }

    #[test]
    fn w29_r04_stages_by_needs() {
        let wf = Workflow { jobs: vec![job("test", &["build"]), job("build", &[]), job("lint", &[]), job("deploy", &["test", "lint"]), job("a", &["b"]), job("b", &["a"]), job("orphan", &["gone"])], ..Workflow::default() };
        assert_eq!(stages(&wf), vec![vec!["build".to_owned(), "lint".into()], vec!["test".into()], vec!["deploy".into()], vec!["a".into(), "b".into(), "orphan".into()]]);
        assert_eq!(stages(&Workflow::default()), Vec::<Vec<String>>::new());
    }

    fn facts(kind: Kind) -> Facts {
        Facts { kind, name: "x".into(), path: "x".into(), runs: true, routable: Some(true), ..Facts::default() }
    }

    #[test]
    fn w29_r02_stands_words_and_pips() {
        let cases = [
            (Facts { latest: Some(Outcome::Passed), ..facts(Kind::Workflow) }, ("shipped", "Passing", ["passed", "passed", "passed"], 2)),
            (Facts { latest: Some(Outcome::Failed), ..facts(Kind::Workflow) }, ("failing", "Failing", ["passed", "passed", "failed"], 0)),
            (Facts { errors: 2, ..facts(Kind::Workflow) }, ("needs_you", "Can't run", ["failed", "passed", "none"], 0)),
            (Facts { routable: Some(false), released: Some("v0.1.0".into()), latest: Some(Outcome::Passed), ..facts(Kind::Shards) }, ("needs_you", "Can't run", ["passed", "blocked", "passed"], 0)),
            (Facts { runs: false, routable: None, ..facts(Kind::Workflow) }, ("superseded", "Not run here", ["passed", "none", "none"], 2)),
            (Facts { off: true, latest: Some(Outcome::Failed), ..facts(Kind::Workflow) }, ("superseded", "Off", ["passed", "passed", "failed"], 2)),
            (Facts { released: Some("v2.4.0".into()), latest: Some(Outcome::Passed), ..facts(Kind::Composite) }, ("shipped", "Released", ["passed", "passed", "passed"], 2)),
            (Facts { routable: None, ..facts(Kind::Docker) }, ("waiting", "Not released", ["passed", "none", "none"], 2)),
            (facts(Kind::Workflow), ("waiting", "Not run yet", ["passed", "passed", "none"], 2)),
            (Facts { draft: Some(Draft::Review), ..facts(Kind::Workflow) }, ("needs_you", "Review", ["passed", "passed", "none"], 0)),
            (Facts { draft: Some(Draft::NotSaved), ..facts(Kind::Workflow) }, ("waiting", "Not saved", ["passed", "passed", "none"], 1)),
            (Facts { draft: Some(Draft::Unpublished), routable: None, ..facts(Kind::Firecracker) }, ("waiting", "Unpublished", ["passed", "none", "none"], 1)),
            (Facts { draft: Some(Draft::Update), running: true, ..facts(Kind::Workflow) }, ("waiting", "Update", ["passed", "passed", "running"], 1)),
        ];
        for (f, (stand, word, pips, rank)) in cases {
            assert_eq!(standing(&f), Standing { stand, word, pips, rank }, "{f:?}");
        }
    }

    #[test]
    fn w29_r02_order_and_heading() {
        let row = |name: &str, f: Facts| {
            let f = Facts { name: name.to_owned(), path: format!(".kiln/{name}.yaml"), ..f };
            let s = standing(&f);
            (f, s)
        };
        let mut rows = vec![
            row("smoke", Facts { latest: Some(Outcome::Passed), ..facts(Kind::Workflow) }),
            row("ci", Facts { latest: Some(Outcome::Passed), ..facts(Kind::Workflow) }),
            row("Stress", Facts { draft: Some(Draft::Review), ..facts(Kind::Workflow) }),
            row("e2e", Facts { latest: Some(Outcome::Failed), ..facts(Kind::Workflow) }),
            row("nightly", Facts { draft: Some(Draft::NotSaved), ..facts(Kind::Workflow) }),
        ];
        order(&mut rows);
        assert_eq!(rows.iter().map(|(f, _)| f.name.as_str()).collect::<Vec<_>>(), ["Stress", "e2e", "nightly", "ci", "smoke"]);
        assert_eq!(heading(&rows, false), ("5 workflows · 1 draft · 1 failing · 1 waits on you".to_owned(), 2));
        let lib = vec![
            row("setup-rust", Facts { released: Some("v2.4.0".into()), ..facts(Kind::Composite) }),
            row("fuzz", Facts { released: Some("v0.3.0".into()), ..facts(Kind::Firecracker) }),
            row("fuzz-target", Facts { draft: Some(Draft::Unpublished), ..facts(Kind::Firecracker) }),
            row("rust-check", Facts { released: Some("v2.4.0".into()), ..facts(Kind::Reusable) }),
        ];
        assert_eq!(heading(&lib, true), ("shared · 3 actions · 1 workflow · 1 draft".to_owned(), 0));
    }

    fn list_repo(n: usize, rows: usize) -> Repo {
        let rows: Vec<Row> = (0..rows)
            .map(|i| {
                let facts = Facts { kind: if i % 3 == 0 { Kind::Composite } else { Kind::Workflow }, name: format!("w{i}"), path: format!(".kiln/w{i}.yaml"), runs: true, routable: Some(true), latest: Some(if i % 7 == 0 { Outcome::Failed } else { Outcome::Passed }), ..Facts::default() };
                Row::new(facts, Some("kiln"), "push, pull request · 4 jobs".into(), Some("ada".into()), Some(1_700_000_000))
            })
            .collect();
        Repo::new(format!("acme/r{n:03}"), "main".into(), "a3f2c1d4e5f60718293a4b5c6d7e8f9012345678".into(), rows)
    }

    fn list(repos: &[Repo], only: Option<&[String]>, repo: Option<&str>, q: Option<&str>) -> Value {
        serde_json::from_str(&list_text(repos, only, repo, q, None)).unwrap_or_default()
    }

    #[test]
    fn w29_r03_the_list_and_what_it_shows() {
        let repos = vec![list_repo(1, 3), list_repo(0, 7)];
        let only = vec!["acme/r000".to_owned()];
        let v = list(&repos, Some(&only), None, None);
        let names: Vec<&str> = v.pointer("/repos").and_then(Value::as_array).map(|a| a.iter().filter_map(|r| r.get("repo").and_then(Value::as_str)).collect()).unwrap_or_default();
        assert_eq!(names, ["acme/r000"], "only what the caller may read");
        let r = v.pointer("/repos/0").cloned().unwrap_or_default();
        assert_eq!(r.get("say").and_then(Value::as_str), Some("4 workflows · 3 actions · 1 failing"), "{r}");
        assert_eq!(r.pointer("/rows/0/word").and_then(Value::as_str), Some("Failing"), "needs someone first: {r}");
        for key in ["key", "kind", "dialect", "name", "path", "stand", "word", "line", "pips", "who", "when", "ref", "commit", "draft", "v"] {
            assert!(r.pointer("/rows/0").and_then(|row| row.get(key)).is_some(), "{key}: {r}");
        }
        let both = list(&repos, None, None, None);
        let first: Vec<&str> = both.pointer("/repos").and_then(Value::as_array).map(|a| a.iter().filter_map(|r| r.get("repo").and_then(Value::as_str)).collect()).unwrap_or_default();
        assert_eq!(first, ["acme/r000", "acme/r001"], "the one needing someone first, then by name");
        assert_eq!(list(&repos, None, Some("acme/r001"), None).pointer("/repos").and_then(Value::as_array).map(Vec::len), Some(1));
        let asked = list(&repos, None, None, Some("w2"));
        assert_eq!(asked.pointer("/repos/0/rows").and_then(Value::as_array).map(Vec::len), Some(1), "{asked}");
        assert_eq!(list(&repos, None, None, Some("nothing")).pointer("/repos").and_then(Value::as_array).map(Vec::len), Some(0));
        assert_eq!(list_repo(1, 3).digest, list_repo(1, 3).digest, "the same rows, the same digest");
        assert_ne!(list_repo(1, 3).digest, list_repo(1, 4).digest, "a row more is another digest");
    }

    /// W29-AC03: the list answered from memory, 200 repositories and 2,000 rows, within 50 ms at p99
    /// in a release build (`cargo test --release -p kiln-ci --lib pipelines`). A debug build measures
    /// and says what it found, without the bound.
    #[test]
    fn w29_ac03_the_list_from_memory() {
        let all: Vec<Repo> = (0..200).map(|r| list_repo(r, 10)).collect();
        let only: Vec<String> = all.iter().map(|r| r.repo.clone()).collect();
        let mut took: Vec<std::time::Duration> = Vec::new();
        for _ in 0..100 {
            let t = std::time::Instant::now();
            let body = list_text(&all, Some(&only), None, None, None);
            took.push(t.elapsed());
            assert!(body.len() > 100_000, "{}", body.len());
        }
        took.sort();
        let p99 = took.get(98).copied().unwrap_or_default();
        println!("W29-AC03: the list from memory, 200 repositories, 2,000 rows: p99 {p99:?}");
        if !cfg!(debug_assertions) {
            assert!(p99 <= std::time::Duration::from_millis(50), "p99 {p99:?}");
        }
    }

    /// Every one of GitHub's events, as `on:` names it, in a GitHub-dialect workflow.
    const EVERY_GITHUB_EVENT: &str = "name: every event\non:\n  branch_protection_rule:\n  check_run:\n  check_suite:\n  create:\n  delete:\n  deployment:\n  deployment_status:\n  discussion:\n  discussion_comment:\n  fork:\n  gollum:\n  image_version:\n  issue_comment:\n  issues:\n  label:\n  merge_group:\n  milestone:\n  page_build:\n  project:\n  project_card:\n  project_column:\n  public:\n  pull_request:\n    branches: [main]\n  pull_request_review:\n  pull_request_review_comment:\n  pull_request_target:\n  push:\n    branches: [main]\n    paths: ['crates/**']\n  registry_package:\n  release:\n    types: [published]\n  repository_dispatch:\n  schedule:\n    - cron: '0 3 * * *'\n  status:\n  watch:\n  workflow_call:\n    inputs:\n      toolchain:\n        type: string\n        required: true\n    outputs:\n      built:\n        value: ${{ jobs.build.outputs.built }}\n    secrets:\n      TOKEN:\n        required: true\n  workflow_dispatch:\n  workflow_run:\n    workflows: [ci]\n    types: [completed]\njobs:\n  build:\n    runs-on: [linux, x64]\n    outputs:\n      built: yes\n    steps:\n      - uses: actions/checkout@v4\n      - run: echo ${{ secrets.TOKEN }} ${{ vars.REGION }}\n  test:\n    needs: build\n    runs-on: linux\n    steps:\n      - uses: ./.github/actions/setup\n";

    /// Every one of kiln's events, in a kiln-dialect workflow.
    const EVERY_KILN_EVENT: &str = "name: every kiln event\non:\n  diff:\n  land:\n  review:\n  comment:\n  pipeline:\n    workflows: [ci]\n  agent:\n  deploy:\n  webhook:\n  manual:\n  tag:\njobs:\n  check:\n    runs-on: local\n    steps:\n      - run: echo checked\n";

    #[test]
    fn w29_r04_every_trigger_kind_in_a_pane() {
        let mut lines = 0;
        for (path, text, events) in [(".github/workflows/every.yml", EVERY_GITHUB_EVENT, GHA_EVENTS), (".kiln/every.yaml", EVERY_KILN_EVENT, KILN_EVENTS)] {
            let (_, w) = crate::wf::check_model(path, text, None);
            let w = w.unwrap_or_default();
            let groups = workflow_groups(&w, text, &[], true, "");
            let triggers = groups.iter().find(|g| g.name == "Triggers").map(|g| g.lines.clone()).unwrap_or_default();
            for e in events {
                // kiln's v1 forms are read as the events they are (W02-R06): `manual` is
                // `workflow_dispatch`, `tag` a push of tags.
                let key = match *e {
                    "manual" => "on.workflow_dispatch".to_owned(),
                    "tag" => "on.push".to_owned(),
                    other => format!("on.{other}"),
                };
                let l = triggers.iter().find(|l| l.key == key);
                assert!(l.is_some_and(|l| !l.name.is_empty() && l.group.is_some()), "{path}: `{e}` has no line with its group and words: {triggers:?}");
            }
            if path.starts_with(".kiln/") {
                let tag = triggers.iter().find(|l| l.key == "on.push").map(|l| l.value.clone());
                assert_eq!(tag.as_deref(), Some("tags *"), "v1's `tag`: {triggers:?}");
            }
            lines += triggers.len();
        }
        assert_eq!(lines, GHA_EVENTS.len() + KILN_EVENTS.len(), "46 lines between the two panes");
    }

    #[test]
    fn w29_r04_a_workflows_groups() {
        let (_, w) = crate::wf::check_model(".github/workflows/every.yml", EVERY_GITHUB_EVENT, None);
        let w = w.unwrap_or_default();
        let runs = [RunSeen { number: 12, event: "push".into(), outcome: Some(Outcome::Failed), running: false }, RunSeen { number: 13, event: "schedule".into(), outcome: None, running: true }];
        let groups = workflow_groups(&w, EVERY_GITHUB_EVENT, &runs, true, "");
        let names: Vec<&str> = groups.iter().map(|g| g.name.as_str()).collect();
        assert_eq!(names, ["Triggers", "Called with", "Stage 1", "Stage 2", "Uses", "Secrets and variables", "Runs", "Settings"]);
        let lines = |n: &str| groups.iter().find(|g| g.name == n).map(|g| g.lines.clone()).unwrap_or_default();
        let call: Vec<(&str, String)> = lines("Called with").iter().map(|l| (l.kind, l.name.clone())).collect();
        assert_eq!(call, [("input", "toolchain".to_owned()), ("output", "built".into()), ("secret", "TOKEN".into())], "a reusable workflow's workflow_call inputs, outputs and secrets");
        let push = lines("Triggers").into_iter().find(|l| l.key == "on.push").unwrap_or_else(|| Line::new("", "trigger", "", ""));
        assert_eq!((push.value.as_str(), push.group, push.whose), ("branches main · paths crates/**", Some("Code"), Some(Whose::Both)));
        assert_eq!(lines("Stage 2").first().map(|l| l.value.clone()), Some("linux · bare metal · needs build".to_owned()));
        let uses: Vec<String> = lines("Uses").iter().map(|l| format!("{}: {}", l.name, l.value)).collect();
        assert_eq!(uses, ["actions/checkout@v4: fetched by name · v4", "./.github/actions/setup: this repository"]);
        let names_only: Vec<(&str, String, String)> = lines("Secrets and variables").iter().map(|l| (l.kind, l.name.clone(), l.value.clone())).collect();
        assert_eq!(names_only, [("secret", "TOKEN".to_owned(), "declared, used".to_owned()), ("variable", "REGION".into(), "used".into())]);
        assert!(groups.iter().flat_map(|g| &g.lines).all(|l| !l.value.contains("${{ secrets")), "names only, never a value");
        let runs: Vec<(String, Option<&str>)> = lines("Runs").iter().map(|l| (l.value.clone(), l.state)).collect();
        assert_eq!(runs, [("push · failed".to_owned(), Some("failed")), ("schedule · under way".into(), Some("running"))]);
        let off = workflow_groups(&w, EVERY_GITHUB_EVENT, &[], false, "GitHub workflows not run: this commit has .kiln/ workflows");
        assert_eq!(off.iter().find(|g| g.name == "Settings").and_then(|g| g.lines.first()).map(|l| l.name.clone()), Some("Not run here".to_owned()));
    }

    #[test]
    fn w29_r04_an_actions_groups_by_kind() {
        let cases = [
            ("name: setup\ninputs:\n  toolchain:\n    description: Which toolchain\n    required: true\n  cache:\n    default: 'true'\noutputs:\n  path:\n    description: Where it went\nruns:\n  using: composite\n  steps:\n    - name: Install\n      run: echo installing\n      shell: bash\n    - uses: actions/cache@v4\n", Kind::Composite, "Steps", vec!["Install: echo installing", "step 2: actions/cache@v4"]),
            ("name: comment\nruns:\n  using: node24\n  main: dist/index.js\n  post: dist/post.js\n", Kind::Javascript, "Runs on Node (node24)", vec!["main: dist/index.js", "post: dist/post.js"]),
            ("name: image\nruns:\n  using: docker\n  image: Dockerfile\n  entrypoint: /entry.sh\n  args: [--fast]\n", Kind::Docker, "Image", vec!["image: Dockerfile", "entrypoint: /entry.sh", "args: --fast"]),
            ("name: fuzz\nruns:\n  using: firecracker\n  image: docker://alpine:3.20\n  resources:\n    cpus: 2\n    memory: 4g\n  egress: [crates.io, static.rust-lang.org]\n", Kind::Firecracker, "Firecracker microVM", vec!["image: docker://alpine:3.20", "resources: cpus 2, memory 4g", "egress: crates.io, static.rust-lang.org"]),
            ("name: sandbox\nruns:\n  using: shards\n  image: docker://alpine:3.20\n", Kind::Shards, "Shards microVM", vec!["image: docker://alpine:3.20", "resources: as the leg's", "egress: none"]),
        ];
        let tags = vec![("v2.4.0".to_owned(), "1234567890abcdef1234".to_owned())];
        for (text, kind, own, want) in cases {
            let (a, _) = crate::wf::parse_action(text);
            let a = a.unwrap_or_default();
            assert_eq!(Kind::of_action(&a), kind, "{text}");
            let groups = action_groups(&a, kind, text, "a3f2c1d4e5f60718", &tags);
            let names: Vec<&str> = groups.iter().map(|g| g.name.as_str()).collect();
            assert_eq!(names, ["Inputs", "Outputs", own, "Versions", "Used by", "Settings"], "{kind:?}");
            let lines: Vec<String> = groups.iter().find(|g| g.name == own).map(|g| g.lines.iter().map(|l| format!("{}: {}", l.name, l.value)).collect()).unwrap_or_default();
            assert_eq!(lines, want, "{kind:?}");
            let versions: Vec<String> = groups.iter().find(|g| g.name == "Versions").map(|g| g.lines.iter().map(|l| l.name.clone()).collect()).unwrap_or_default();
            assert_eq!(versions, ["the default branch's tip", "v2.4.0"]);
        }
        let (a, _) = crate::wf::parse_action(cases_text());
        let groups = action_groups(&a.unwrap_or_default(), Kind::Composite, cases_text(), "a3f2", &[]);
        let inputs: Vec<String> = groups.iter().find(|g| g.name == "Inputs").map(|g| g.lines.iter().map(|l| format!("{}: {}", l.name, l.value)).collect()).unwrap_or_default();
        assert_eq!(inputs, ["toolchain: required · Which toolchain", "cache: default true"]);
    }

    fn cases_text() -> &'static str {
        "name: setup\ninputs:\n  toolchain:\n    description: Which toolchain\n    required: true\n  cache:\n    default: 'true'\nruns:\n  using: composite\n  steps:\n    - run: echo hi\n      shell: bash\n"
    }

    #[test]
    fn w29_r04_verbs_are_dimmed_never_hidden() {
        let w = verbs(Kind::Workflow);
        assert_eq!(w.iter().map(|v| v.verb).collect::<Vec<_>>(), ["run", "save_draft", "ready", "discard"]);
        assert!(w.iter().all(|v| !v.enabled && v.why_not.is_some_and(|y| y.contains("W29-R"))), "{w:?}");
        assert_eq!(verbs(Kind::Composite).iter().map(|v| v.verb).collect::<Vec<_>>(), ["run", "save_draft", "ready", "discard", "release"]);
        assert!(verbs(Kind::Reusable).iter().any(|v| v.verb == "release"));
    }

    #[test]
    fn w29_r04_verbs_by_draft_state() {
        let ada = Viewer { name: "ada", can_submit: true };
        let sam = Viewer { name: "sam", can_submit: true };
        let reader = Viewer { name: "rae", can_submit: false };
        let draft = |state| DraftOf { state, author: "ada".into(), change: Some(DraftChange { kind: "diff", name: "D12".into(), url: None }) };
        let wf = Row::new(facts(Kind::Workflow), Some("gha"), String::new(), None, None);
        let saved = wf.clone().drafted(draft(Draft::Unpublished));
        let review = wf.clone().drafted(draft(Draft::Review));
        let update = wf.clone().drafted(DraftOf { state: Draft::Update, author: "kiln".into(), change: None });
        let said = |row: &Row, v: Viewer<'_>, by_hand: bool| -> Vec<(&'static str, bool, Option<&'static str>)> { verbs_for(row, v, by_hand).into_iter().map(|x| (x.verb, x.enabled, x.why_not)).collect() };
        let on = |row: &Row, v: Viewer<'_>, by_hand: bool| -> Vec<&'static str> { verbs_for(row, v, by_hand).into_iter().filter(|x| x.enabled).map(|x| x.verb).collect() };
        // The author's own draft: everything but Release, which isn't built yet.
        assert_eq!(on(&saved, ada, false), ["run", "save_draft", "ready", "discard"]);
        // In review: Ready is done; Run and Discard are still theirs.
        assert_eq!(on(&review, ada, false), ["run", "save_draft", "discard"]);
        // Someone else's: only Save draft (their own draft of it), the rest saying why.
        assert_eq!(on(&review, sam, false), ["save_draft"]);
        assert!(said(&review, sam, false).iter().any(|(v, _, why)| *v == "ready" && why.is_some_and(|w| w.contains("Only its author"))));
        // A published workflow runs by hand only with workflow_dispatch or manual.
        assert_eq!(on(&wf, sam, true), ["run", "save_draft"]);
        assert_eq!(on(&wf, sam, false), ["save_draft"]);
        assert_eq!(on(&wf, reader, true), ["run"], "reading isn't submitting");
        // A reusable workflow runs where it's called; an action runs by Try it; kiln's update drafts wait.
        let reusable = Row::new(facts(Kind::Reusable), Some("gha"), String::new(), None, None);
        assert!(on(&reusable, ada, true).iter().all(|v| *v != "run"));
        assert!(on(&Row::new(facts(Kind::Composite), None, String::new(), None, None), ada, false).contains(&"run"));
        assert_eq!(on(&update, ada, false), ["save_draft"]);
        // Never hidden: each verb is there, and each one off says why.
        for (row, v) in [(&saved, ada), (&review, sam), (&wf, reader), (&update, sam), (&reusable, reader)] {
            let all = said(row, v, false);
            assert!(all.len() >= 4, "{all:?}");
            assert!(all.iter().all(|(_, enabled, why)| *enabled || why.is_some_and(|w| w.contains("W29-R"))), "{all:?}");
        }
    }

    #[test]
    fn w29_r10_a_draft_in_review_is_its_authors() {
        let mk = |name: &str, kind: Kind| Row::new(Facts { name: name.into(), path: format!(".github/workflows/{name}.yml"), ..facts(kind) }, Some("gha"), String::new(), None, None);
        let review = mk("stress", Kind::Workflow).drafted(DraftOf { state: Draft::Review, author: "ada".into(), change: Some(DraftChange { kind: "change", name: "#5".into(), url: Some("https://github.com/acme/app/pull/5".into()) }) });
        let repo = Repo::new("acme/app".into(), "main".into(), "a".repeat(40), vec![mk("ci", Kind::Workflow), review]);
        assert_eq!(repo.authors, ["ada"]);
        // Everyone else is asked to review it.
        let them = list(std::slice::from_ref(&repo), None, None, None);
        assert_eq!(them.pointer("/repos/0/rows/0/word").and_then(Value::as_str), Some("Review"), "{them}");
        assert_eq!(them.pointer("/repos/0/say").and_then(Value::as_str), Some("2 workflows · 1 waits on you"), "{them}");
        assert_eq!(them.pointer("/repos/0/rows/0/draft/change/name").and_then(Value::as_str), Some("#5"), "{them}");
        assert_eq!(them.pointer("/repos/0/rows/0/draft/state").and_then(Value::as_str), Some("review"), "{them}");
        // Its author waits on them: to her it's still hers, unpublished.
        let her: Value = serde_json::from_str(&list_text(std::slice::from_ref(&repo), None, None, None, Some("ada"))).unwrap_or_default();
        let row = her.pointer("/repos/0/rows").and_then(Value::as_array).and_then(|rows| rows.iter().find(|r| r.get("name").and_then(Value::as_str) == Some("stress")).cloned()).unwrap_or_default();
        assert_eq!((row.get("word").and_then(Value::as_str), row.get("stand").and_then(Value::as_str)), (Some("Unpublished"), Some("waiting")), "{her}");
        assert_eq!(her.pointer("/repos/0/say").and_then(Value::as_str), Some("2 workflows · 1 draft"), "{her}");
        assert_eq!(her.pointer("/repos/0/needs").and_then(Value::as_u64), Some(0), "{her}");
        // Someone with no draft there sees what everyone does, and a row without one says null.
        assert_eq!(list_text(std::slice::from_ref(&repo), None, None, None, Some("sam")), list_text(std::slice::from_ref(&repo), None, None, None, None));
        assert!(them.pointer("/repos/0/rows/1/draft").is_some_and(Value::is_null), "{them}");
    }

    #[test]
    fn w29_r04_names_used_are_names_only() {
        let text = "env:\n  T: ${{ secrets.TOKEN }}\n  R: ${{ vars.REGION }}\n  X: ${{ secrets.TOKEN }} ${{ fromJSON(vars.MATRIX) }}\n  N: ${{ mysecrets.NOPE }} secrets.OUTSIDE\n";
        assert_eq!(names_used(text), (vec!["TOKEN".to_owned()], vec!["MATRIX".to_owned(), "REGION".into()]));
    }

    #[test]
    fn w29_r04_where_a_use_comes_from() {
        for (u, f) in [("./.github/actions/setup", UseFrom::Here), ("docker://alpine:3.20", UseFrom::Image), ("kiln/checkout@v1", UseFrom::Kiln), ("kiln://acme/kit/x@v2", UseFrom::Hosted), ("actions/checkout@v4", UseFrom::Named)] {
            assert_eq!(use_from(u), f, "{u}");
        }
        assert_eq!(use_ref("actions/checkout@v4"), Some("v4"));
        assert_eq!(use_ref("./local"), None);
    }

    #[test]
    fn w29_r21_full_versions_and_the_newest_release() {
        assert_eq!(full_version("v2.4.1"), Some((2, 4, 1, None)));
        assert_eq!(full_version("1.0.0-rc.1"), Some((1, 0, 0, Some("rc.1"))));
        for floating in ["v2", "v2.4", "v2.4.1.0", "v2.x.1", "latest", "v1.2.3-", "", "v"] {
            assert_eq!(full_version(floating), None, "{floating:?}");
        }
        assert_eq!(newest_release(["v1.9.9", "v1.10.0", "v2", "nightly"]), Some("v1.10.0"));
        assert_eq!(newest_release(["v2.0.0-rc.1", "v1.10.0"]), Some("v2.0.0-rc.1"));
        assert_eq!(newest_release(["v2.0.0-rc.2", "v2.0.0", "v2.0.0-rc.10"]), Some("v2.0.0"));
        assert_eq!(newest_release(["v2", "main"]), None);
        assert_eq!(releases(["v1.0.0", "v2", "v1.10.0", "v1.2.0-rc.1", "v1.2.0"]), ["v1.10.0", "v1.2.0", "v1.2.0-rc.1", "v1.0.0"]);
    }

    #[test]
    fn w29_r02_a_reusable_workflow_is_never_not_run_here() {
        let s = standing(&Facts { runs: false, routable: None, ..facts(Kind::Reusable) });
        assert_eq!((s.stand, s.word), ("waiting", "Not released"));
    }

    /// The approved stills' rows (`~/.cache/kiln-pipelines-design/design.mjs`, ROWS), each as the
    /// facts it shows, give the stills' stand, word and pips; their three repositories, the stills'
    /// headings and order.
    #[test]
    fn w29_r02_the_stills_rows() {
        let released = |v: &str| Some(v.to_owned());
        let passed = Some(Outcome::Passed);
        let rows: Vec<(&str, Facts, (&str, &str, [&str; 3]))> = vec![
            ("stress", Facts { draft: Some(Draft::Review), latest: passed, ..facts(Kind::Workflow) }, ("needs_you", "Review", ["passed", "passed", "passed"])),
            ("e2e", Facts { latest: Some(Outcome::Failed), ..facts(Kind::Workflow) }, ("failing", "Failing", ["passed", "passed", "failed"])),
            ("ci", Facts { latest: passed, ..facts(Kind::Workflow) }, ("shipped", "Passing", ["passed", "passed", "passed"])),
            ("ciDraft", Facts { draft: Some(Draft::NotSaved), latest: passed, ..facts(Kind::Workflow) }, ("waiting", "Not saved", ["passed", "passed", "passed"])),
            ("release", Facts { latest: passed, ..facts(Kind::Workflow) }, ("shipped", "Passing", ["passed", "passed", "passed"])),
            ("nightly", Facts { latest: passed, ..facts(Kind::Workflow) }, ("shipped", "Passing", ["passed", "passed", "passed"])),
            ("smoke", Facts { latest: passed, ..facts(Kind::Workflow) }, ("shipped", "Passing", ["passed", "passed", "passed"])),
            ("ghci", Facts { runs: false, routable: None, ..facts(Kind::Workflow) }, ("superseded", "Not run here", ["passed", "none", "none"])),
            ("setupRust", Facts { released: released("v2.4.0"), latest: passed, ..facts(Kind::Composite) }, ("shipped", "Released", ["passed", "passed", "passed"])),
            ("rustCheck", Facts { released: released("v2.4.0"), latest: passed, ..facts(Kind::Reusable) }, ("shipped", "Released", ["passed", "passed", "passed"])),
            ("prComment", Facts { released: released("v3.1.0"), latest: passed, ..facts(Kind::Javascript) }, ("shipped", "Released", ["passed", "passed", "passed"])),
            ("buildImage", Facts { released: released("v1.4.0"), latest: passed, ..facts(Kind::Docker) }, ("shipped", "Released", ["passed", "passed", "passed"])),
            ("fuzz", Facts { released: released("v0.3.0"), latest: passed, ..facts(Kind::Firecracker) }, ("shipped", "Released", ["passed", "passed", "passed"])),
            ("sandbox", Facts { released: released("v0.1.0"), routable: Some(false), ..facts(Kind::Shards) }, ("needs_you", "Can't run", ["passed", "blocked", "none"])),
            ("quiverCi", Facts { draft: Some(Draft::Update), running: true, update: Some("ci-kit v2.4.0".into()), ..facts(Kind::Workflow) }, ("waiting", "Update", ["passed", "passed", "running"])),
            ("fuzzTarget", Facts { draft: Some(Draft::Unpublished), ..facts(Kind::Firecracker) }, ("waiting", "Unpublished", ["passed", "passed", "none"])),
        ];
        for (name, f, (stand, word, pips)) in &rows {
            let s = standing(f);
            assert_eq!((s.stand, s.word, s.pips), (*stand, *word, *pips), "{name}");
        }
        let repo = |names: &[(&str, &str)]| -> Vec<(Facts, Standing)> {
            names
                .iter()
                .filter_map(|(key, shown)| rows.iter().find(|(k, ..)| k == key).map(|(_, f, _)| Facts { name: (*shown).to_owned(), ..f.clone() }))
                .map(|f| {
                    let s = standing(&f);
                    (f, s)
                })
                .collect()
        };
        let mut kiln = repo(&[("stress", "stress"), ("e2e", "e2e"), ("ci", "ci"), ("release", "release"), ("nightly", "nightly"), ("smoke", "smoke"), ("ghci", "GitHub CI")]);
        assert_eq!(heading(&kiln, false), ("7 workflows · 1 failing · 1 waits on you".to_owned(), 2));
        order(&mut kiln);
        assert_eq!(kiln.iter().map(|(f, _)| f.name.as_str()).collect::<Vec<_>>(), ["stress", "e2e", "ci", "GitHub CI", "nightly", "release", "smoke"]);
        let kit = repo(&[("sandbox", "sandbox"), ("fuzzTarget", "fuzz-target"), ("setupRust", "setup-rust"), ("rustCheck", "rust-check"), ("prComment", "pr-comment"), ("buildImage", "build-image"), ("fuzz", "fuzz")]);
        assert_eq!(heading(&kit, true), ("shared · 6 actions · 1 workflow · 1 draft".to_owned(), 1));
        let quiver = repo(&[("quiverCi", "ci"), ("release", "release"), ("nightly", "nightly")]);
        assert_eq!(heading(&quiver, false), ("3 workflows · ci-kit v2.4.0 is out".to_owned(), 0));
    }
}
