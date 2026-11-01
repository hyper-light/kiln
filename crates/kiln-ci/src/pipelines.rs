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

/// The newest full version among a repository's tags, by semver's precedence: a release comes
/// after its own pre-releases (a tag is the whole repository's, W29-R21).
pub fn newest_release<'a>(tags: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    tags.into_iter()
        .filter_map(|t| full_version(t).map(|(a, b, c, pre)| ((a, b, c, pre.is_none(), pre.unwrap_or_default()), t)))
        .max_by(|(x, _), (y, _)| x.cmp(y))
        .map(|(_, t)| t)
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
}

impl Row {
    pub fn new(facts: Facts, dialect: Option<&'static str>, line: String, who: Option<String>, when: Option<i64>) -> Row {
        let standing = standing(&facts);
        Row { facts, standing, dialect, line, who, when }
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
}

impl Repo {
    pub fn new(repo: String, branch: String, sha: String, mut rows: Vec<Row>) -> Repo {
        order_rows(&mut rows);
        let rendered: Vec<String> = rows.iter().map(|r| row_json(&repo, &branch, &sha, r).to_string()).collect();
        // W29-R17's Access is the master plan's step 5: until then nothing is shared.
        let (say, needs) = heading_of(rows.iter().map(|r| (&r.facts, &r.standing)), false);
        let mut r = Repo { repo, branch, sha, rows, rendered, say, needs, whole: String::new(), digest: 0 };
        r.whole = r.text(r.rendered.iter().map(String::as_str));
        r.digest = fnv(FNV_START, r.whole.as_bytes());
        r
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
        // W29-R10's draft: the master plan's step 4.
        "draft": Value::Null,
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
/// was rendered when the rows were gathered is joined as it is.
pub fn list_text<'a>(repos: impl IntoIterator<Item = &'a Repo>, only: Option<&[String]>, repo: Option<&str>, q: Option<&str>) -> String {
    let mut out: Vec<(bool, &str, std::borrow::Cow<'a, str>)> = Vec::new();
    for r in repos.into_iter().filter(|r| only.is_none_or(|names| names.iter().any(|n| n == &r.repo))).filter(|r| repo.is_none_or(|want| want == r.repo)) {
        let text = match q {
            None => std::borrow::Cow::Borrowed(r.whole.as_str()),
            Some(q) if r.repo.to_lowercase().contains(q) => std::borrow::Cow::Borrowed(r.whole.as_str()),
            Some(q) => {
                let rows: Vec<&str> = r.rows.iter().zip(&r.rendered).filter(|(row, _)| row_matches(row, q)).map(|(_, j)| j.as_str()).collect();
                if rows.is_empty() {
                    continue;
                }
                std::borrow::Cow::Owned(r.text(rows.into_iter()))
            }
        };
        out.push((r.needs > 0, r.repo.as_str(), text));
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
        serde_json::from_str(&list_text(repos, only, repo, q)).unwrap_or_default()
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
            let body = list_text(&all, Some(&only), None, None);
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
