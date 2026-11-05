//! Ship's Pipelines (docs/ci.md W29-R02 to R04): what each workflow, reusable workflow and action
//! is, where it stands, its three pips and its place among its repository's rows, and the list's
//! JSON; and, for an opened row's pane, each trigger's group and words and each job's stage. Decided
//! from what the server gathers and passes in (`crates/kiln/src/ship/pipelines.rs`): nothing here
//! reads a clock, a disk or the network.

use std::cmp::Ordering;
use std::collections::HashMap;

use serde::Serialize;
use serde_json::{Value, json};

use crate::sharing::Access;
use crate::wf::ir::{Trigger, YVal};
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

/// One `on:` entry as the pane's Triggers group shows it: its kind, what it fires on, and what's
/// said beside it, as the approved stills word them (pl-2, pl-8).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TriggerLine {
    pub event: String,
    pub words: String,
    pub group: Group,
    pub whose: Option<Whose>,
    /// What it fires on: `main`, `into main`, `0 3 * * *`, `after ci`, `kiln-production`, `/smoke`.
    pub value: String,
    /// Beside it: `every push`, `drafts too`, `03:00 Los Angeles`, `succeeds on main`, `synced`.
    pub note: String,
}

/// A trigger's line in the pane (W29-R04's Triggers group).
pub fn trigger_line(t: &Trigger) -> TriggerLine {
    let kind = event_kind(&t.event);
    let (value, note) = trigger_words(t);
    TriggerLine {
        event: t.event.clone(),
        words: kind.map_or_else(|| t.event.replace('_', " "), |k| k.words.to_owned()),
        group: kind.map_or(Group::Repository, |k| k.group),
        whose: kind.map(|k| k.whose),
        value,
        note,
    }
}

/// A list of words from a kiln event's filter as written: `[synced, degraded]` or `synced`.
fn yval_words(v: &YVal) -> Option<String> {
    match v {
        YVal::Seq(items) => Some(items.iter().filter_map(YVal::as_str).collect::<Vec<_>>().join(", ")).filter(|w| !w.is_empty()),
        YVal::Bool(b) => Some(b.to_string()),
        other => other.as_str().map(str::to_owned).filter(|w| !w.is_empty()),
    }
}

/// A webhook's filter in words where it's one comparison (`event.status != 'ok'`: `status isn't
/// ok`), else as written.
fn filter_words(f: &str) -> String {
    let said = |op: &str, words: &str| {
        let (l, r) = f.split_once(op)?;
        let field = l.trim().rsplit('.').next()?.trim();
        let v = r.trim().trim_matches(|c| c == '\'' || c == '"');
        (!field.is_empty() && !v.is_empty() && !v.contains(' ')).then(|| format!("{field} {words} {v}"))
    };
    said("!=", "isn't").or_else(|| said("==", "is")).unwrap_or_else(|| f.trim().to_owned())
}

/// A schedule's time where its cron names one (`0 3 * * *`: `03:00`), with its zone's city
/// (`America/Los_Angeles`: `Los Angeles`), or the zone alone.
fn schedule_words(s: &crate::wf::ir::Schedule) -> String {
    let city = s.timezone.as_deref().map_or_else(|| "UTC".to_owned(), |z| z.rsplit('/').next().unwrap_or(z).replace('_', " "));
    let f: Vec<&str> = s.cron.split_whitespace().collect();
    match (f.first().and_then(|m| m.parse::<u32>().ok()), f.get(1).and_then(|h| h.parse::<u32>().ok())) {
        (Some(m), Some(h)) => format!("{h:02}:{m:02} {city}"),
        _ => city,
    }
}

/// A trigger's subject and note, by its event, as the approved stills word them (W29-R06).
fn trigger_words(t: &Trigger) -> (String, String) {
    let joined = |v: &Option<Vec<String>>| v.as_ref().filter(|v| !v.is_empty()).map(|v| v.join(", "));
    let extra = |k: &str| t.extra.iter().find(|(n, _)| n == k).and_then(|(_, v)| yval_words(v));
    let paths = || match (joined(&t.paths), joined(&t.paths_ignore)) {
        (Some(p), _) => Some(format!("paths {p}")),
        (None, Some(p)) => Some(format!("not paths {p}")),
        (None, None) => None,
    };
    let on_branches = || joined(&t.branches).map(|b| format!("on {b}"));
    match t.event.as_str() {
        "push" => {
            let subject = joined(&t.branches).or_else(|| joined(&t.tags).map(|x| format!("tags {x}"))).unwrap_or_else(|| "any branch".into());
            (subject, paths().unwrap_or_else(|| "every push".into()))
        }
        "pull_request" | "pull_request_target" => {
            let subject = format!("into {}", joined(&t.branches).unwrap_or_else(|| "any branch".into()));
            let mut note = if t.types.is_empty() { "drafts too".to_owned() } else { t.types.join(", ") };
            if let Some(p) = paths() {
                note = format!("{note} · {p}");
            }
            (subject, note)
        }
        "workflow_dispatch" if t.inputs.is_empty() => ("Run, kiln run, the API".into(), String::new()),
        "workflow_dispatch" => {
            let names = t.inputs.iter().map(|i| i.name.as_str()).collect::<Vec<_>>().join(", ");
            (format!("inputs: {names}"), t.inputs.iter().find(|i| !i.options.is_empty()).map(|i| i.options.join(" | ")).unwrap_or_default())
        }
        "schedule" => match t.schedules.as_slice() {
            [one] => (one.cron.clone(), schedule_words(one)),
            many => (format!("{} schedules", many.len()), many.iter().map(|s| s.cron.as_str()).collect::<Vec<_>>().join(", ")),
        },
        "deploy" => {
            let mut note = extra("states").unwrap_or_default();
            if let Some(env) = extra("environment") {
                note = if note.is_empty() { format!("in {env}") } else { format!("{note} in {env}") };
            }
            (extra("app").unwrap_or_else(|| "any app".into()), note)
        }
        "pipeline" | "workflow_run" => {
            let after = extra("name").or_else(|| (!t.workflows.is_empty()).then(|| t.workflows.join(", "))).unwrap_or_else(|| "any workflow".into());
            let ends = extra("conclusion").map(|c| {
                c.split(", ")
                    .map(|x| match x {
                        "success" => "succeeds",
                        "failure" => "fails",
                        "cancelled" => "is cancelled",
                        other => other,
                    })
                    .collect::<Vec<_>>()
                    .join(" or ")
            });
            let ends = ends.or_else(|| (!t.types.is_empty()).then(|| t.types.join(", ")));
            let note = [ends, on_branches()].into_iter().flatten().collect::<Vec<_>>().join(" ");
            (format!("after {after}"), note)
        }
        "webhook" => (extra("hook").unwrap_or_else(|| "any hook".into()), extra("filter").map(|f| filter_words(&f)).unwrap_or_default()),
        "comment" => (extra("commands").or_else(|| extra("mentions").map(|m| format!("@{m}"))).unwrap_or_else(|| "any comment".into()), "on a diff".into()),
        "review" => (extra("states").unwrap_or_else(|| "any review".into()), String::new()),
        "agent" => {
            let inputs = t.inputs.iter().map(|i| i.name.as_str()).collect::<Vec<_>>().join(", ");
            (extra("agents").unwrap_or_else(|| "any agent".into()), if inputs.is_empty() { String::new() } else { format!("inputs: {inputs}") })
        }
        "diff" => {
            let mut note = if extra("drafts").as_deref() == Some("true") { "drafts too".to_owned() } else { String::new() };
            if let Some(a) = extra("authors") {
                note = [note, a].into_iter().filter(|x| !x.is_empty()).collect::<Vec<_>>().join(" · ");
            }
            (format!("onto {}", joined(&t.branches).unwrap_or_else(|| "any trunk".into())), note)
        }
        "land" => (joined(&t.branches).unwrap_or_else(|| "any trunk".into()), String::new()),
        _ => (filters_line(t), String::new()),
    }
}

/// Any other trigger's filters in a line: `opened, closed · branches main · paths crates/**`.
fn filters_line(t: &Trigger) -> String {
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
    parts.join(" · ")
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
    /// Its record's id (kiln's `ci_pipeline_drafts`).
    pub id: String,
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

/// W29-R04, pl-2: the full release at a floating tag's commit, and that commit. `tags` must be
/// the tags pointing at that exact commit, not the repository's newest releases elsewhere.
pub fn floating_words<'a>(git_ref: &str, tags: impl IntoIterator<Item = &'a str>, sha: &str) -> Option<String> {
    if !matches!(crate::releases::kind_of(git_ref), crate::releases::TagKind::Floating) || sha.is_empty() {
        return None;
    }
    let prefix = format!("{}.", git_ref.strip_prefix('v').unwrap_or(git_ref));
    let tag = newest_release(tags.into_iter().filter(|t| t.strip_prefix('v').unwrap_or(t).starts_with(&prefix)))?;
    Some(format!("{} · {}", tag.strip_prefix('v').unwrap_or(tag), sha.get(..7).unwrap_or(sha)))
}

/// Which of two rows comes first in W29-R02's order: what needs someone first (Review, then
/// Failing, then Can't run), then drafts, then the rest; each by name (any case), then by path.
fn before(fa: &Facts, sa: &Standing, fb: &Facts, sb: &Standing) -> Ordering {
    urgency(sa).cmp(&urgency(sb)).then_with(|| fa.name.to_lowercase().cmp(&fb.name.to_lowercase())).then_with(|| fa.path.cmp(&fb.path))
}

/// W29-R02, pl-1: the same urgency decides both row and repository order.
fn urgency(s: &Standing) -> (u8, u8) {
    (s.rank, match s.word { "Review" => 0, "Failing" => 1, "Can't run" => 2, _ => 3 })
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
    /// A workflow's dialect: `gha` or `kiln`; a code workflow's language, `ts` or `js` (W29-R27).
    pub dialect: Option<&'static str>,
    pub line: String,
    /// Who changed its file last, and when (seconds).
    pub who: Option<String>,
    pub when: Option<i64>,
    /// The commit that last changed this file (or an action's folder), not its repository's tip (W29-R02, pl-1).
    pub commit: Option<String>,
    pub draft: Option<DraftOf>,
}

impl Row {
    pub fn new(facts: Facts, dialect: Option<&'static str>, line: String, who: Option<String>, when: Option<i64>) -> Row {
        let standing = standing(&facts);
        Row { facts, standing, dialect, line, who, when, commit: None, draft: None }
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
    /// Who may use its actions and reusable workflows (W29-R17).
    pub access: Access,
}

impl Repo {
    pub fn new(repo: String, branch: String, sha: String, mut rows: Vec<Row>, access: Access) -> Repo {
        order_rows(&mut rows);
        let rendered: Vec<String> = rows.iter().map(|r| row_json(&repo, &branch, &sha, r).to_string()).collect();
        let (say, needs) = heading_of(rows.iter().map(|r| (&r.facts, &r.standing)), access != Access::None);
        let mut authors: Vec<String> = rows.iter().filter_map(|r| r.draft.as_ref()).filter(|d| d.state == Draft::Review).map(|d| d.author.clone()).collect();
        authors.sort_unstable();
        authors.dedup();
        let mut r = Repo { repo, branch, sha, rows, rendered, say, needs, whole: String::new(), digest: 0, authors, access };
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
        Some(Repo::new(self.repo.clone(), self.branch.clone(), self.sha.clone(), rows, self.access))
    }

    /// Its JSON with these of its rows' JSON.
    fn text<'a>(&self, rows: impl Iterator<Item = &'a str>) -> String {
        let head = json!({ "repo": self.repo, "say": self.say, "needs": self.needs, "access": self.access, "ref": self.branch, "commit": self.sha }).to_string();
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
        // A draft's row stands beside its file's published one (W29-R10).
        "key": match &r.draft {
            Some(d) => format!("{repo}:{}:draft:{}", r.facts.path, d.id),
            None => format!("{repo}:{}", r.facts.path),
        },
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
        "commit": r.commit.as_deref().unwrap_or(sha),
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
    let mut out: Vec<((u8, u8), &str, std::borrow::Cow<'a, str>)> = Vec::new();
    for r in repos.into_iter().filter(|r| only.is_none_or(|names| names.iter().any(|n| n == &r.repo))).filter(|r| repo.is_none_or(|want| want == r.repo)) {
        let priority = |rows: &[Row]| rows.iter().map(|r| urgency(&r.standing)).min().unwrap_or((2, 3));
        let (rank, text) = match viewer.and_then(|v| r.seen_by(v)) {
            Some(own) => match text_of(&own, q) {
                Some(t) => (priority(&own.rows), std::borrow::Cow::Owned(t.into_owned())),
                None => continue,
            },
            None => match text_of(r, q) {
                Some(t) => (priority(&r.rows), t),
                None => continue,
            },
        };
        out.push((rank, r.repo.as_str(), text));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(b.1)));
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
    /// What's said beside a trigger's value (`every push`, `03:00 Los Angeles`); a run's event
    /// (`schedule`), its kind word where it isn't a push.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl Line {
    fn new(key: impl Into<String>, kind: &'static str, name: impl Into<String>, value: impl Into<String>) -> Line {
        Line { key: key.into(), kind, name: name.into(), value: value.into(), state: None, group: None, whose: None, note: None }
    }

    fn state(mut self, s: &'static str) -> Line {
        self.state = Some(s);
        self
    }

    fn note(mut self, n: impl Into<String>) -> Line {
        self.note = Some(n.into());
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
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunSeen {
    pub number: i64,
    pub event: String,
    pub outcome: Option<Outcome>,
    pub running: bool,
    /// Its commit, when it was made (seconds), and the jobs that failed in it.
    pub sha: String,
    pub at: i64,
    pub failed: Vec<String>,
    /// What started it, when its event names something beyond its commit (pl-8).
    pub subject: Option<String>,
    /// W29-R23–R26: the code that refused it before any job started (`KCI4055`).
    pub refused: Option<String>,
}

/// W29-R23–R26: the code a run's startup error opens with, when kiln refused it before any job.
pub fn refusal_code(error: &str) -> Option<String> {
    let code = error.split_whitespace().next()?.trim_end_matches(':');
    code.strip_prefix("KCI").is_some_and(|n| n.len() == 4 && n.bytes().all(|b| b.is_ascii_digit())).then(|| code.to_owned())
}

/// W29-R04, pl-8: a run by what started it. Only event metadata is read, never input values or
/// the rest of a comment. Without enough metadata its line falls back to its commit.
pub fn run_subject(event: &str, payload: &serde_json::Value) -> Option<String> {
    let text = |p| payload.pointer(p).and_then(serde_json::Value::as_str).filter(|s| !s.is_empty());
    match event {
        "deploy" => {
            let state = text("/kiln/deploy/state")?;
            let op = payload.pointer("/kiln/deploy/operation/id").and_then(serde_json::Value::as_i64);
            Some(op.map_or_else(|| format!("{} {state}", text("/kiln/deploy/app").unwrap_or("deploy")), |n| format!("#{n} {state}")))
        }
        "schedule" => {
            let cron = text("/schedule")?;
            let mut parts = cron.split_whitespace();
            let minute = parts.next().and_then(|s| s.parse::<u8>().ok()).filter(|m| *m < 60);
            let hour = parts.next().and_then(|s| s.parse::<u8>().ok()).filter(|h| *h < 24);
            Some(match (hour, minute) { (Some(h), Some(m)) => format!("{h:02}:{m:02}"), _ => cron.to_owned() })
        }
        "comment" | "issue_comment" => {
            let command = text("/comment/body")?.split_whitespace().next().filter(|s| s.starts_with('/') && s.len() <= 100)?;
            let n = payload.pointer("/issue/number").or_else(|| payload.pointer("/pull_request/number")).and_then(serde_json::Value::as_i64)?;
            Some(format!("{command} on #{n}"))
        }
        _ => None,
    }
}

/// What kiln has seen of a workflow beyond its text, for its lines' words as the approved stills
/// have them (pl-2): each job's time in its last finished run, each use's standing against its
/// newest release, where each secret and variable is set, and now, for the runs' ages.
#[derive(Debug, Clone, Default)]
pub struct Seen {
    /// Seconds, by job id.
    pub job_secs: HashMap<String, i64>,
    /// Each job's outcome in that run, by job id, and the run's commit: the strip's `its last run
    /// passed in 18m 40s on a3f2c1d` (pl-2).
    pub job_outcome: HashMap<String, Outcome>,
    pub last_sha: Option<String>,
    /// By `uses`: `v2.4.0 is out` (and that it's behind) or `newest`.
    pub uses: HashMap<String, (String, bool)>,
    /// The `uses` of a repository kiln hosts, named plainly (`acme/kit/.github/actions/x@v1`).
    pub hosted: std::collections::HashSet<String>,
    /// Facts read from the exact cached version a job calls (W29-R04, pl-7).
    pub called: HashMap<String, CalledSeen>,
    /// By name: `repository`, `organization`, `server`.
    pub set_in: HashMap<String, String>,
    pub now: i64,
    /// A reusable workflow's newest release, against which its Used by marks who's behind.
    pub newest: Option<String>,
    /// Its locks and the trunk's lock policy (W29-R23–R26), where kiln read them.
    pub lock: Option<LockSeen>,
}

/// W29-R23–R26: a workflow's locks at the pane's commit, and the trunk's trusted lock policy.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LockSeen {
    /// How many of the uses a lock can hold its locks hold, of how many; None when no lock has an
    /// entry for it.
    pub held: Option<(usize, usize)>,
    /// A use that a lock can hold and none does: what makes an onboarded file stale.
    pub missing: Option<String>,
    pub require: crate::lock::verify::Require,
    pub moved_tags: crate::lock::verify::MovedTags,
    /// The full versions it uses whose tags now name another commit than this repository accepted.
    pub moved: Vec<String>,
}

impl LockSeen {
    fn complete(&self) -> bool { self.held.is_some() && self.missing.is_none() }

    /// Its lines in Settings, only those with something to say (Settings that says only its
    /// defaults isn't shown): what its locks hold, what the trunk requires of them, moved tags.
    fn lines(&self) -> Vec<Line> {
        use crate::lock::verify::{MovedTags, Require};
        let held = match (&self.held, &self.missing) {
            (Some(_), Some(u)) => Line::new("settings.lock", "setting", "Locked", format!("{u} isn't locked yet")).state("flag"),
            (Some((1, _)), None) => Line::new("settings.lock", "setting", "Locked", "its one use at its commit").state("passed"),
            (Some((0, _)), None) => Line::new("settings.lock", "setting", "Locked", "it uses nothing a lock holds").state("passed"),
            (Some((n, _)), None) => Line::new("settings.lock", "setting", "Locked", format!("{n} uses at their commits")).state("passed"),
            (None, _) => Line::new("settings.lock", "setting", "Not locked", "its uses follow their tags").state("flag"),
        };
        let require = match self.require {
            Require::Off => Line::new("settings.require_lock", "setting", "Require lockfile", "off").state("none"),
            Require::Report => Line::new("settings.require_lock", "setting", "Require lockfile", "report: its runs say what isn't locked").state(if self.complete() { "passed" } else { "flag" }),
            Require::Enforce => Line::new("settings.require_lock", "setting", "Require lockfile", "enforce: what isn't locked doesn't run").state(if self.complete() { "passed" } else { "failed" }),
        };
        let (word, state) = match self.moved_tags { MovedTags::Flag => ("flagged", "flag"), MovedTags::Refuse => ("refused", "failed") };
        let moved = match self.moved.as_slice() {
            [] => Line::new("settings.moved_tags", "setting", "Moved tags", format!("{word} when a full version moves")).state("none"),
            [one] => Line::new("settings.moved_tags", "setting", "Moved tags", format!("{one} moved: {word}")).state(state),
            [first, rest @ ..] => Line::new("settings.moved_tags", "setting", "Moved tags", format!("{first} and {} more moved: {word}", rest.len())).state(state),
        };
        // What its locks hold and the trunk's word on them, once it has a lock or the trunk asks
        // for one; moved tags, once one moved or the trunk refuses them.
        let mut out = Vec::new();
        if self.held.is_some() || self.require != Require::Off { out.extend([held, require]); }
        if !self.moved.is_empty() || self.moved_tags == MovedTags::Refuse { out.push(moved); }
        out
    }
}

/// A called workflow's one job, where its jobs run when they agree, and its own references.
/// No file is fetched to supply these facts: absent cached facts leave the caller's line alone.
#[derive(Debug, Clone, Default)]
pub struct CalledSeen {
    pub job: Option<String>,
    pub machine: Option<&'static str>,
    pub uses: Vec<(String, bool)>,
}

/// W29-R04: references in the file's order, with workflow calls distinguished from actions.
pub fn direct_uses(w: &Workflow) -> Vec<(String, bool)> {
    w.jobs.iter().flat_map(|j| j.uses.iter().cloned().map(|u| (u, true)).chain(j.steps.iter().filter_map(|s| s.uses.clone()).map(|u| (u, false)))).collect()
}

/// W29-R04: the cached callee's display facts, without changing the caller's job identity.
pub fn called_seen(w: &Workflow) -> CalledSeen {
    let machine = |j: &crate::wf::Job| {
        j.uses.is_none().then_some(if j.container.is_some() || j.kiln.image.is_some() { "in a container" } else { "bare metal" })
    };
    let first = w.jobs.first();
    CalledSeen {
        job: first.filter(|_| w.jobs.len() == 1).map(|j| j.id.clone()),
        machine: first.and_then(machine).filter(|m| w.jobs.iter().all(|j| machine(j) == Some(*m))),
        uses: direct_uses(w),
    }
}

/// W29-R04, pl-7: a call followed by its cached dependencies, once each, in file order. Cycles
/// and excessive nesting cannot recurse or grow the pane indefinitely.
fn pane_uses(w: &Workflow, seen: &Seen) -> Vec<(String, bool)> {
    let mut todo: Vec<_> = direct_uses(w).into_iter().rev().map(|(u, call)| (u, call, 0)).collect();
    let (mut out, mut listed) = (Vec::new(), std::collections::HashSet::new());
    while let Some((u, call, depth)) = todo.pop() {
        if !listed.insert(u.clone()) {
            continue;
        }
        if call && depth < 4 && let Some(c) = seen.called.get(&u) {
            todo.extend(c.uses.iter().rev().map(|(u, call)| (u.clone(), *call, depth + 1)));
        }
        out.push((u, call));
    }
    out
}

/// What kiln has seen of an action beyond its text, for its lines' words as pl-5 has them: when
/// each tag was made, its newest release and the commits since, and the inputs added since.
#[derive(Debug, Clone, Default)]
pub struct ActionSeen {
    /// Seconds, by tag.
    pub tag_at: HashMap<String, i64>,
    /// Its newest release, and how many commits to its files came after it.
    pub since: Option<(String, usize)>,
    /// The inputs added since its newest release.
    pub added: Vec<String>,
    pub now: i64,
    /// W29-R26: accepted and observed commits for a full-version tag that moved.
    pub moved: HashMap<String, (String, String)>,
}

/// A span in its largest unit, as the stills say it: `40s`, `18m`, `2h`, `3d`, `3w`.
pub fn span_words(secs: i64) -> String {
    match secs.max(0) {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s if s < 7 * 86_400 => format!("{}d", s / 86_400),
        s => format!("{}w", s / (7 * 86_400)),
    }
}

/// A span in its two largest units, as pl-2's strip says a job's time: `40s`, `18m 40s`, `2h 5m`,
/// `3d 4h`.
pub fn span_exact(secs: i64) -> String {
    let two = |a: i64, ua: &str, b: i64, ub: &str| if b == 0 { format!("{a}{ua}") } else { format!("{a}{ua} {b}{ub}") };
    match secs.max(0) {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => two(s / 60, "m", s % 60, "s"),
        s if s < 86_400 => two(s / 3600, "h", s % 3600 / 60, "m"),
        s => two(s / 86_400, "d", s % 86_400 / 3600, "h"),
    }
}

/// A job's line, as the approved stills word it (pl-2): its id, then its legs where a matrix
/// makes more than one (`3 platforms`), where it runs (bare metal unless a container is asked
/// for) and its labels beyond that, or the workflow it calls; its time in the last finished run;
/// and what it needs, which the stage's note reads (`after rust and ui`). Its note is what the
/// strip says of it picked: where it runs, what it reserves and its last run (`bare metal, linux ·
/// 2 CPUs, 16 GiB · its last run passed in 18m 40s on a3f2c1d`).
fn job_line(j: &crate::wf::Job, seen: &Seen) -> Line {
    let called = j.uses.as_ref().and_then(|u| seen.called.get(u));
    let mut legs: Option<String> = None;
    let mut at: Vec<String> = Vec::new();
    match &j.uses {
        Some(u) => {
            at.push(format!("calls {u}"));
            at.extend(called.and_then(|c| c.machine).map(str::to_owned));
        }
        None => {
            if let Some(crate::wf::ir::Matrix::Spec(m)) = j.strategy.as_ref().and_then(|s| s.matrix.as_ref()) {
                let count = |a: &crate::wf::ir::Axis| if let crate::wf::ir::Axis::Values(v) = a { v.len().max(1) } else { 1 };
                // kiln's `platforms`, or an axis named for them (`platform`, `os`): pl-2's `3 platforms`.
                let named = m.axes.iter().find(|(k, a)| matches!(k.as_str(), "platform" | "platforms" | "os") && count(a) > 1).map(|(_, a)| count(a));
                legs = match (&m.platforms, named) {
                    (Some(p), _) if count(p) > 1 => Some(format!("{} platforms", count(p))),
                    (_, Some(n)) => Some(format!("{n} platforms")),
                    _ => Some(m.axes.iter().map(|(_, a)| count(a)).product::<usize>()).filter(|n| *n > 1).map(|n| format!("{n} legs")),
                };
            }
            at.push(if j.container.is_some() || j.kiln.image.is_some() { "in a container" } else { "bare metal" }.to_owned());
            match &j.runs_on {
                Some(crate::wf::ir::RunsOn::Labels(l)) => at.extend(l.iter().map(|t| t.src.clone()).filter(|t| !matches!(t.as_str(), "baremetal" | "bare-metal"))),
                Some(crate::wf::ir::RunsOn::Group { group, labels }) => {
                    at.extend(group.iter().map(|g| format!("group {}", g.src)));
                    at.extend(labels.iter().map(|t| t.src.clone()).filter(|t| !matches!(t.as_str(), "baremetal" | "bare-metal")));
                }
                None => {}
            }
        }
    }
    // A job only a branch runs (`if: github.ref == 'refs/heads/main'`): pl-2's `on main`.
    let on = j.cond.as_ref().and_then(|c| on_branch(&c.src)).map(|b| format!("on {b}"));
    let secs = seen.job_secs.get(&j.id).copied();
    let mut parts: Vec<String> = legs.iter().chain(&at).chain(&on).cloned().collect();
    // Its time in the last finished run (pl-2's `18m`).
    parts.extend(secs.map(span_words));
    if !j.needs.is_empty() {
        parts.push(format!("needs {}", j.needs.iter().map(|n| n.job()).collect::<Vec<_>>().join(", ")));
    }
    let mut said: Vec<String> = legs.into_iter().chain([at.join(", ")]).chain(on).collect();
    said.extend(reserved_words(j));
    let last = match (seen.job_outcome.get(&j.id), secs) {
        (Some(o), Some(s)) => {
            let on = seen.last_sha.as_deref().map(|c| format!(" on {}", c.get(..7).unwrap_or(c))).unwrap_or_default();
            Some(match o {
                Outcome::Passed => format!("its last run passed in {}{on}", span_exact(s)),
                Outcome::Failed => format!("its last run failed after {}{on}", span_exact(s)),
                Outcome::Cancelled => format!("its last run was cancelled after {}{on}", span_exact(s)),
            })
        }
        _ => None,
    };
    said.extend(last);
    let name = called.and_then(|c| c.job.as_ref()).map(|id| format!("{} / {id}", j.id)).unwrap_or_else(|| j.id.clone());
    Line::new(format!("jobs.{}", j.id), "job", name, parts.join(" · ")).note(said.join(" · "))
}

/// What a job reserves (`resources:`), as pl-2's strip says it: `2 CPUs, 16 GiB`.
fn reserved_words(j: &crate::wf::Job) -> Option<String> {
    use crate::sched::needs;
    let r = needs::resource_spec(j.kiln.resources.as_ref()?).resources().ok()?;
    let mut parts = Vec::new();
    if r.cpu > 0.0 {
        parts.push(needs::cpus(r.cpu));
    }
    if r.mem > 0 {
        parts.push(needs::show_size(r.mem));
    }
    if r.disk > 0 {
        parts.push(format!("{} of disk", needs::show_size(r.disk)));
    }
    if r.gpu > 0 {
        parts.push(needs::gpus(r.gpu));
    }
    (!parts.is_empty()).then(|| parts.join(", "))
}

/// The branch a job's `if:` keeps it to, where that's all it says: `github.ref == 'refs/heads/main'`
/// or `github.ref_name == 'main'`.
fn on_branch(cond: &str) -> Option<String> {
    let c = cond.trim().trim_start_matches("${{").trim_end_matches("}}").trim();
    let (l, r) = c.split_once("==")?;
    let v = r.trim().trim_matches(|x| x == '\'' || x == '"');
    match l.trim() {
        "github.ref" => v.strip_prefix("refs/heads/").map(str::to_owned),
        "github.ref_name" => Some(v.to_owned()),
        _ => None,
    }
    .filter(|b| !b.is_empty() && !b.contains(' '))
}

fn outcome_glyph(outcome: Option<Outcome>, running: bool) -> &'static str {
    match (running, outcome) {
        (true, _) => "running",
        (false, Some(Outcome::Passed)) => "passed",
        (false, Some(Outcome::Failed)) => "failed",
        (false, Some(Outcome::Cancelled) | None) => "none",
    }
}

/// A workflow's groups (W29-R04): Triggers, its stages (Jobs for one stage containing a call), Uses,
/// Secrets and variables (names only, when used),
/// Runs, Settings, and a reusable workflow's interface; with `sharing`, a reusable workflow's Used
/// by and its Access in Settings (W29-R17, R20).
pub fn workflow_groups(w: &Workflow, text: &str, runs: &[RunSeen], runs_here: bool, note: &str, sharing: Option<&Sharing<'_>>, seen: &Seen) -> Vec<PaneGroup> {
    let mut out = Vec::new();
    let triggers: Vec<Line> = w
        .on
        .iter()
        .map(|t| {
            let l = trigger_line(t);
            let note = (!l.note.is_empty()).then_some(l.note);
            Line { key: format!("on.{}", t.event), kind: "trigger", name: l.words, value: l.value, state: None, group: Some(l.group.words()), whose: l.whose, note }
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
    let stages = stages(w);
    let calls = stages.len() == 1 && w.jobs.iter().any(|j| j.uses.is_some());
    for (i, ids) in stages.iter().enumerate() {
        let lines = ids.iter().filter_map(|id| w.job(id)).map(|j| job_line(j, seen)).collect();
        out.push(group(if calls { "Jobs".to_owned() } else { format!("Stage {}", i + 1) }, lines));
    }
    let mut uses: Vec<Line> = Vec::new();
    for (u, call) in pane_uses(w, seen) {
        let from = if seen.hosted.contains(&u) { UseFrom::Hosted } else { use_from(&u) };
        let mut value = from.words().to_owned();
        if call {
            value = format!("a reusable workflow, {value}");
        }
        // Its standing against its newest release, where kiln knows it (pl-2's `v2.4.0 is out`).
        let line = match seen.uses.get(&u) {
            Some((words, behind)) => {
                let l = Line::new(format!("uses.{u}"), "use", u, format!("{value} · {words}"));
                if *behind { l.state("flag") } else { l }
            }
            None => Line::new(format!("uses.{u}"), "use", u, value),
        };
        uses.push(line);
    }
    if !uses.is_empty() {
        out.push(group("Uses", uses));
    }
    let (secrets, vars) = names_used(text);
    let mut declared: Vec<String> = w.secrets.iter().map(|x| x.name.clone()).collect();
    declared.extend(w.trigger("workflow_call").map(|t| t.secrets.iter().map(|x| x.name.clone()).collect::<Vec<_>>()).unwrap_or_default());
    let mut names: Vec<Line> = Vec::new();
    // Where each is set, as pl-2 says it; else whether it's declared or used.
    for n in secrets.iter().chain(declared.iter().filter(|d| !secrets.contains(d))) {
        let value = match (seen.set_in.get(n), secrets.contains(n), declared.contains(n)) {
            (Some(at), _, _) => at.clone(),
            (None, true, true) => "declared, used".to_owned(),
            (None, false, true) => "declared".to_owned(),
            _ => "used".to_owned(),
        };
        names.push(Line::new(format!("secrets.{n}"), "secret", n.clone(), value));
    }
    names.extend(vars.iter().map(|n| Line::new(format!("vars.{n}"), "variable", n.clone(), seen.set_in.get(n).cloned().unwrap_or_else(|| "used".to_owned()))));
    if !names.is_empty() {
        out.push(group("Secrets and variables", names));
    }
    // Each run by its commit, what came of it (the jobs that failed), and how long ago, as pl-2
    // says them; its event beside it.
    let runs_lines: Vec<Line> = runs
        .iter()
        .map(|r| {
            let status = match (r.running, r.outcome) {
                (true, _) => "under way".to_owned(),
                (false, Some(Outcome::Passed)) => "passed".to_owned(),
                (false, Some(Outcome::Failed)) if !r.failed.is_empty() => format!("{} failed", r.failed.join(", ")),
                (false, Some(Outcome::Failed)) if r.refused.is_some() => format!("refused ({})", r.refused.as_deref().unwrap_or_default()),
                (false, Some(Outcome::Failed)) => "failed".to_owned(),
                (false, Some(Outcome::Cancelled)) => "cancelled".to_owned(),
                (false, None) => "queued".to_owned(),
            };
            let value = if r.at > 0 && seen.now > 0 { format!("{status} · {}", span_words(seen.now - r.at)) } else { status };
            let name = r.subject.clone().unwrap_or_else(|| r.sha.get(..7).map_or_else(|| format!("#{}", r.number), str::to_owned));
            Line::new(format!("runs.{}", r.number), "run", name, value).state(outcome_glyph(r.outcome, r.running)).note(r.event.clone())
        })
        .collect();
    out.push(group("Runs", runs_lines));
    let shared = sharing.filter(|_| w.trigger("workflow_call").is_some());
    if let Some(s) = shared {
        out.push(group("Used by", s.used_by_lines(seen.newest.as_deref())));
    }
    let mut settings = vec![if runs_here { Line::new("settings.on", "setting", "Turned on", "runs on its triggers") } else { Line::new("settings.on", "setting", "Not run here", note) }];
    settings.push(Line::new("settings.machines", "setting", "Machines", "bare metal unless a job says"));
    if let Some(c) = &w.concurrency {
        settings.push(Line::new("settings.concurrency", "setting", "Concurrency", c.group.src.clone()));
    }
    settings.extend(seen.lock.iter().flat_map(LockSeen::lines));
    settings.extend(shared.map(Sharing::access_line));
    out.push(group("Settings", settings));
    out
}

/// What sharing says in an action's or a reusable workflow's pane (W29-R17, R20): its repository's
/// Access, and which repositories' runs pinned which of its versions.
#[derive(Debug, Clone, Copy)]
pub struct Sharing<'a> {
    /// Its repository (`acme/kit`).
    pub repo: &'a str,
    pub access: Access,
    pub used_by: &'a [crate::catalog::Consumer],
}

impl Sharing<'_> {
    /// Used by's lines: each repository and version, newest first; a note when there's none.
    /// Used by's lines, as pl-5: each repository by its workflows and the version they use, marked
    /// where that's behind `newest`; a note when there's none.
    fn used_by_lines(&self, newest: Option<&str>) -> Vec<Line> {
        if self.used_by.is_empty() {
            return vec![Line::new("used_by", "note", "Who uses it", "no other repository's runs have used it yet").state("none")];
        }
        self.used_by
            .iter()
            .map(|c| {
                let value = if c.workflows.is_empty() { c.git_ref.clone() } else { format!("{} · {}", c.workflows.join(", "), c.git_ref) };
                let behind = newest.is_some_and(|n| full_version(&c.git_ref).is_some() && c.git_ref != n && newest_release([c.git_ref.as_str(), n]) == Some(n));
                let l = Line::new(format!("used_by.{}@{}", c.repo, c.git_ref), "consumer", c.repo.clone(), value);
                if behind { l.state("flag") } else { l }
            })
            .collect()
    }

    /// Settings' Access line, its state the level (`none`, `owner`, `everyone`).
    fn access_line(&self) -> Line {
        let owner = self.repo.split('/').next().unwrap_or(self.repo);
        let words = match self.access {
            Access::None => "not shared: only its own workflows use it".to_owned(),
            Access::Owner => format!("shared with {owner}'s repositories"),
            Access::Everyone => "shared with everyone on this kiln".to_owned(),
        };
        Line::new("settings.access", "setting", "Access", words).state(self.access.as_str())
    }
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
/// egress), its Versions (`tags`: newest first, each with its commit), and what sharing says:
/// Used by, and its Access in Settings (W29-R17, R20).
pub fn action_groups(a: &Action, kind: Kind, text: &str, head: &str, tags: &[(String, String)], sharing: &Sharing<'_>, seen: &ActionSeen) -> Vec<PaneGroup> {
    let mut out = Vec::new();
    // Each input by what it takes when it isn't given (pl-5): its default, what its description
    // says applies (`…; rust-toolchain.toml's when not given`), or that it's required; one added
    // since the newest release says so.
    let inputs = a
        .inputs
        .iter()
        .map(|i| {
            let added = seen.added.iter().any(|n| n == &i.name);
            let unless = || i.description.as_deref().and_then(|d| d.rsplit(';').next()).map(str::trim).and_then(|d| d.strip_suffix(" when not given")).map(str::to_owned);
            let v = match (added, &seen.since) {
                (true, Some((tag, _))) => format!("new since {tag}"),
                _ if i.required => "required".to_owned(),
                _ => i.default.as_ref().map(|d| d.src.clone()).or_else(unless).unwrap_or_else(|| "optional".to_owned()),
            };
            let l = Line::new(format!("inputs.{}", i.name), "input", i.name.clone(), v);
            if added { l.state("added") } else { l }
        })
        .collect();
    out.push(group("Inputs", inputs));
    if !a.outputs.is_empty() {
        out.push(group("Outputs", a.outputs.iter().map(|o| Line::new(format!("outputs.{}", o.name), "output", o.name.clone(), o.description.clone().unwrap_or_default())).collect()));
    }
    let r = &a.runs;
    match kind {
        Kind::Composite => {
            let steps = r
                .steps
                .iter()
                .enumerate()
                .map(|(i, st)| {
                    // As pl-5: an action by what it uses and what it's given (`kiln/cache@v1 ·
                    // ~/.cargo, target`), a command by its name and that it runs.
                    let name = st.uses.clone().or_else(|| st.name.as_ref().map(|n| n.src.clone())).or_else(|| st.id.clone()).or_else(|| st.run.as_ref().map(|x| x.src.lines().next().unwrap_or_default().to_owned())).unwrap_or_else(|| format!("step {}", i + 1));
                    let value = if st.uses.is_some() {
                        st.with.iter().filter_map(|(_, v)| yval_words(v)).collect::<Vec<_>>().join(", ")
                    } else if st.run.as_ref().is_some_and(|r| name == r.src.trim() && !name.contains('\n')) {
                        "its output".to_owned()
                    } else {
                        "run".to_owned()
                    };
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
    // As pl-5: the tip by how far it's come since the newest release; each release by its age and
    // the repositories that use it; a floating tag by the release it stands for.
    let since = match &seen.since {
        Some((tag, 0)) => format!("as {tag}"),
        Some((tag, 1)) => format!("1 commit since {tag}"),
        Some((tag, n)) => format!("{n} commits since {tag}"),
        None => String::new(),
    };
    let mut versions = vec![Line::new("versions.head", "version", head.get(..7).unwrap_or(head), since)];
    let newest = seen.since.as_ref().map(|(t, _)| t.as_str());
    versions.extend(tags.iter().map(|(t, sha)| {
        let value = match crate::releases::kind_of(t) {
            crate::releases::TagKind::Full => {
                let users = sharing.used_by.iter().filter(|c| &c.git_ref == t).map(|c| c.repo.as_str()).collect::<std::collections::HashSet<_>>().len();
                let age = seen.tag_at.get(t).filter(|_| seen.now > 0).map(|at| span_words(seen.now - at));
                let users = match users {
                    0 => None,
                    1 => Some("1 repository".to_owned()),
                    n => Some(format!("{n} repositories")),
                };
                [age, users].into_iter().flatten().collect::<Vec<_>>().join(" · ")
            }
            crate::releases::TagKind::Floating => tags.iter().find(|(o, s)| o != t && s == sha && crate::releases::kind_of(o) == crate::releases::TagKind::Full).map_or_else(|| sha.get(..7).unwrap_or(sha).to_owned(), |(o, _)| format!("the newest {t}: {o}")),
            crate::releases::TagKind::Other => sha.get(..7).unwrap_or(sha).to_owned(),
        };
        match seen.moved.get(t) {
            Some((before, now)) => Line::new(format!("versions.{t}"), "version", t.clone(), if before == now { "source identity changed".to_owned() } else { format!("tag moved · {} → {}", before.get(..7).unwrap_or(before), now.get(..7).unwrap_or(now)) }).state("flag"),
            None => Line::new(format!("versions.{t}"), "version", t.clone(), value),
        }
    }));
    out.push(group("Versions", versions));
    out.push(group("Used by", sharing.used_by_lines(newest)));
    out.push(group("Settings", vec![sharing.access_line()]));
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
    /// What it asks before it acts (Release's Keep · Release, W29-R21): what it changes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub asks: Option<String>,
    /// Release's versions, the one what it changes calls for first ([`crate::releases::next_versions`]).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

/// Who looks at a pane, for its verbs: their name, and whether they may submit changes to its
/// repository (`diffs.submit`).
#[derive(Debug, Clone, Copy)]
pub struct Viewer<'a> {
    pub name: &'a str,
    pub can_submit: bool,
    /// May land on the repository: push a tag, or Release one (W29-R21).
    pub can_land: bool,
}

/// The strip's verbs for `row` as `viewer` sees it, once drafts are built (W29-R04, R10 to R14),
/// each there or dimmed with why:
/// - Run: its author's draft, a workflow that runs by hand (`by_hand`: it has `workflow_dispatch`,
///   or kiln's `manual`), or an action's Try it (W21-R07);
/// - Save draft: whoever may submit;
/// - Ready and Discard: the draft's own author;
/// - Release, for what's shared: dimmed until releases are built (W29-R21).
pub fn verbs_for(row: &Row, viewer: Viewer<'_>, by_hand: bool) -> Vec<Verb> {
    let on = |verb: &'static str, label: &'static str| Verb { verb, label, enabled: true, why_not: None, asks: None, tags: Vec::new() };
    let dim = |verb: &'static str, label: &'static str, why: &'static str| Verb { verb, label, enabled: false, why_not: Some(why), asks: None, tags: Vec::new() };
    let kind = row.facts.kind;
    let draft = row.draft.as_ref();
    let mine = draft.is_some_and(|d| d.author == viewer.name);
    let update = draft.is_some_and(|d| d.state == Draft::Update);
    let run = match draft {
        Some(_) if mine => on("run", "Run"),
        // kiln's update drafts are the repository's: whoever may submit to it runs them (pl-7).
        Some(_) if update && viewer.can_submit => on("run", "Run"),
        Some(_) => dim("run", "Run", "A draft runs only by its author's hand (W29-R14)"),
        None if kind == Kind::Reusable => dim("run", "Run", "A reusable workflow runs where it's called (W29-R14)"),
        None if kind == Kind::Workflow && !by_hand => dim("run", "Run", "It has no workflow_dispatch or manual trigger to run it by hand (W29-R14)"),
        None => on("run", "Run"),
    };
    let save = if viewer.can_submit { on("save_draft", "Save draft") } else { dim("save_draft", "Save draft", "Saving a draft needs diffs.submit (W29-R11)") };
    let ready = match draft {
        None => dim("ready", "Ready", "Ready is for a draft (W29-R12)"),
        Some(_) if update && viewer.can_submit => on("ready", "Ready"),
        Some(_) if update => dim("ready", "Ready", "Marking kiln's update draft ready needs diffs.submit (W29-R22)"),
        Some(_) if !mine => dim("ready", "Ready", "Only its author marks it ready (W29-R12)"),
        Some(d) if d.state == Draft::Review => dim("ready", "Ready", "It's in review already (W29-R12)"),
        Some(_) => on("ready", "Ready"),
    };
    let discard = match draft {
        None => dim("discard", "Discard", "Discard is for a draft (W29-R13)"),
        Some(_) if update && viewer.can_submit => on("discard", "Discard"),
        Some(_) if update => dim("discard", "Discard", "Discarding kiln's update draft needs diffs.submit (W29-R22)"),
        Some(_) if !mine => dim("discard", "Discard", "Only its author discards it (W29-R13)"),
        Some(_) => on("discard", "Discard"),
    };
    let mut out = vec![run, save, ready, discard];
    if kind.is_action() || kind == Kind::Reusable {
        out.push(match draft {
            Some(_) => dim("release", "Release", "A draft is released once it's published (W29-R21)"),
            None if !viewer.can_land => dim("release", "Release", "Releasing needs diffs.land, as pushing a tag does (W29-R21)"),
            None => on("release", "Release"),
        });
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
    /// Who opened it, for its verbs and its draft's word ([`Row::seen_by`]).
    pub viewer: Viewer<'a>,
    /// A workflow that runs by hand: it has `workflow_dispatch` (kiln's `manual` reads as it).
    pub by_hand: bool,
    /// What its next release changes for its consumers, since its newest (W29-R21, R22).
    pub changes: Option<&'a [crate::wf::diff::Change]>,
    /// Its newest release and the commits to its files since (pl-5's Changes view).
    pub since: Option<Since<'a>>,
    /// A GitHub workflow's overlay (W02-R07): its path and text, empty where it has none yet. Flow
    /// writes kiln's own triggers there (W29-R06).
    pub overlay: Option<(&'a str, &'a str)>,
    /// What the strip says before anything's picked, where the pane has its own words (an update
    /// draft's, [`update_said`]), and the words it sets in bold.
    pub said: Option<(Vec<String>, Vec<String>)>,
    /// The Changes view's lines: what a release changes for those who use it and the repository's
    /// own checks (pl-5), or what an update changes for its repository and its draft's run (pl-7).
    pub lines: Option<Vec<Line>>,
    /// An update draft's moved use: its name, and its version before and after (pl-7's `What
    /// v2.4.0 changes for quiver`).
    pub update: Option<(String, String, String)>,
    /// A published workflow's pick words and the lines each keeps ([`narrows`]); none, `all` alone.
    pub narrow: Vec<(&'static str, Vec<String>)>,
}

/// An action's or reusable workflow's commits since its newest release (W29-R21): what Release
/// would tag.
#[derive(Debug, Clone, Copy)]
pub struct Since<'a> {
    pub tag: &'a str,
    pub commits: &'a [SinceCommit],
}

/// One commit since a release: its short hash, subject, author and time (seconds).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SinceCommit {
    pub sha: String,
    pub subject: String,
    pub author: String,
    pub at: i64,
}

/// Release's Keep · Release words: what releasing the head changes for its consumers (W29-R21).
pub fn release_asks(changes: &[crate::wf::diff::Change]) -> String {
    if changes.is_empty() {
        return "Keep · Release: nothing changes for the workflows that use it".to_owned();
    }
    let said: Vec<String> = changes.iter().map(change_words).collect();
    format!("Keep · Release: for the workflows that use it, {}", said.join("; "))
}

fn change_words(c: &crate::wf::diff::Change) -> String {
    match &c.detail {
        Some(d) => format!("{} `{}` {}, {d}", c.kind, c.name, c.what),
        None => format!("{} `{}` {}", c.kind, c.name, c.what),
    }
}

/// An update's upstream, as its pane's lines (W29-R22): each used file between its two versions,
/// and how much of it changed.
pub fn upstream_lines(upstream: &[(String, String)]) -> Vec<Line> {
    upstream.iter().map(|(name, stat)| Line::new(format!("upstream.{name}"), "upstream", name.clone(), if stat.is_empty() { "no change to its files".to_owned() } else { stat.clone() })).collect()
}

/// What a release or an update changes for those who use it (W29-R21, R22), in sentences as the
/// approved stills say them (pl-5, pl-7): each names what changed (its key's last part, which the
/// pane sets in bold) and is marked `safe` where nothing they pass has to change, `breaks` where
/// something does, and `access` where it may read or reach more. `who` is the repository an update
/// is for (`quiver passes none, so nothing changes`); none, those who use it.
pub fn change_lines(changes: &[crate::wf::diff::Change], who: Option<&str>) -> Vec<Line> {
    if changes.is_empty() {
        let said = who.map_or_else(|| "nothing they pass changes".to_owned(), |w| format!("nothing {w} passes changes"));
        return vec![Line::new("changes", "note", said, "same").state("passed")];
    }
    let each = who.unwrap_or("each use");
    let passes = who.map_or_else(|| "they pass".to_owned(), |w| format!("{w} passes"));
    changes
        .iter()
        .map(|c| {
            let n = &c.name;
            let required = c.detail.as_deref() == Some("required");
            let (said, mark, state) = match (c.kind, c.what) {
                ("input", "added") if required => (format!("a new input, {n} (required): {each} has to pass it"), "breaks", "flag"),
                ("input", "added") => (format!("a new input, {n} (optional): {}", who.map_or_else(|| "nothing they pass changes".to_owned(), |w| format!("{w} passes none, so nothing changes"))), "safe", "added"),
                ("input", "removed") => (format!("input {n} removed: a use that passes it has to drop it"), "breaks", "flag"),
                ("secret", "added") if required => (format!("a new secret, {n} (required): {each} has to pass it"), "breaks", "flag"),
                ("secret", "added") => (format!("a new secret, {n} (optional): it may read one more"), "access", "flag"),
                ("secret", "removed") => (format!("secret {n} removed: nothing {passes} has to change"), "safe", "passed"),
                (k, "now required") => (format!("{k} {n} is required now: {each} has to pass it"), "breaks", "flag"),
                (k, "no longer required") => (format!("{k} {n} is optional now: nothing {passes} has to change"), "safe", "passed"),
                ("permission", "added") => (format!("a new permission, {n}: {}", c.detail.as_deref().unwrap_or("read")), "access", "flag"),
                ("permission", "changed") => (format!("permission {n}: {}", c.detail.as_deref().unwrap_or("changed")), "access", "flag"),
                ("permission", "removed") => (format!("permission {n} no longer asked for"), "safe", "passed"),
                ("egress", "added") => (format!("a new host it may reach, {n}"), "access", "flag"),
                ("egress", "removed") => (format!("host {n} no longer reached"), "safe", "passed"),
                (k, "added") => (format!("a new {k}, {n}"), "safe", "added"),
                (k, "removed") => (format!("{k} {n} taken out"), "safe", "passed"),
                (k, what) => (format!("{k} {n} {what}{}", c.detail.as_deref().map(|d| format!(": {d}")).unwrap_or_default()), "", "passed"),
            };
            Line::new(format!("changes.{}.{}", c.kind, c.name), "change", said, mark).state(state)
        })
        .collect()
}

/// Whether `changes` let what's used read or reach more: a new secret or permission, a raised
/// one, or a new host.
fn more_access(changes: &[crate::wf::diff::Change]) -> bool {
    changes.iter().any(|c| matches!((c.kind, c.what), ("secret", "added") | ("permission", "added" | "changed") | ("egress", "added")))
}

/// An update's line where it asks for no more than it had (pl-7): `no new secrets, permissions or
/// egress · same`.
pub fn access_line(changes: &[crate::wf::diff::Change]) -> Option<Line> {
    (!more_access(changes)).then(|| Line::new("changes.access", "note", "no new secrets, permissions or egress", "same").state("passed"))
}

/// A repository's own checks on a commit (pl-5's Changes): `ci-kit's own checks passed on e41a9c2
/// · 4 jobs`.
pub fn checks_line(repo: &str, sha: &str, outcome: Option<Outcome>, running: bool, jobs: usize) -> Line {
    let (own, at) = (repo.rsplit('/').next().unwrap_or(repo), sha.get(..7).unwrap_or(sha));
    let (said, state) = match (outcome, running) {
        (Some(Outcome::Failed), _) => (format!("{own}'s own checks failed on {at}"), "failed"),
        (_, true) => (format!("{own}'s own checks are running on {at}"), "running"),
        (Some(Outcome::Passed), false) => (format!("{own}'s own checks passed on {at}"), "passed"),
        (Some(Outcome::Cancelled), false) => (format!("{own}'s own checks were cancelled on {at}"), "none"),
        (None, false) => (format!("{own}'s own checks haven't run on {at}"), "none"),
    };
    Line::new("changes.checks", "note", said, match jobs {
        0 => String::new(),
        1 => "1 job".to_owned(),
        n => format!("{n} jobs"),
    })
    .state(state)
}

/// A draft's newest run (pl-7's Changes): its jobs by what's become of them, and how long it's
/// taken: `#88's run on its draft: rust / check running, bench queued · 4m`. `jobs` are each job's
/// key and status, in its run's order.
pub fn draft_run_line(name: Option<&str>, jobs: &[(String, String)], outcome: Option<Outcome>, running: bool, secs: i64) -> Line {
    let run = name.map_or_else(|| "the draft's run".to_owned(), |n| format!("{n}'s run on its draft"));
    let of = |states: &[&str]| jobs.iter().filter(|(_, s)| states.contains(&s.as_str())).map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(", ");
    let (failed, under_way, waiting) = (of(&["failed", "timed_out"]), of(&["running"]), of(&["queued", "pending"]));
    let (said, state) = if running {
        let parts: Vec<String> = [(failed, "failed"), (under_way, "running"), (waiting, "queued")].into_iter().filter(|(j, _)| !j.is_empty()).map(|(j, w)| format!("{j} {w}")).collect();
        (if parts.is_empty() { format!("{run} is starting") } else { format!("{run}: {}", parts.join(", ")) }, "running")
    } else {
        match outcome {
            Some(Outcome::Passed) => (format!("{run} passed"), "passed"),
            Some(Outcome::Failed) if !failed.is_empty() => (format!("{run}: {failed} failed"), "failed"),
            Some(Outcome::Failed) => (format!("{run} failed"), "failed"),
            Some(Outcome::Cancelled) => (format!("{run} was cancelled"), "none"),
            None => (format!("{run} hasn't started"), "none"),
        }
    };
    Line::new("changes.run", "note", said, span_words(secs)).state(state)
}

/// R04's pick words (pl-2), each with the lines it keeps undimmed: the jobs its last finished run
/// failed, and the lines an open draft changes (a trigger or job a draft has otherwise or not at
/// all, a use it no longer has). `runs` are newest first; `drafts` are its open drafts' workflows.
pub fn narrows(w: &Workflow, runs: &[RunSeen], drafts: &[Workflow]) -> Vec<(&'static str, Vec<String>)> {
    let failed = runs.iter().find(|r| r.outcome.is_some()).map(|r| r.failed.iter().filter(|k| w.job(k).is_some()).map(|k| format!("jobs.{k}")).collect()).unwrap_or_default();
    let mut changed: Vec<String> = Vec::new();
    let mut keep = |k: String| {
        if !changed.contains(&k) {
            changed.push(k);
        }
    };
    for d in drafts {
        // Where it's written isn't part of either's JSON: only what they say is compared.
        for t in &w.on {
            if !d.on.iter().any(|x| x.event == t.event && same_json(x, t)) {
                keep(format!("on.{}", t.event));
            }
        }
        for j in &w.jobs {
            if !d.job(&j.id).is_some_and(|x| same_json(x, j)) {
                keep(format!("jobs.{}", j.id));
            }
        }
        let theirs = uses_in(d);
        for u in uses_in(w) {
            if !theirs.contains(&u) {
                keep(format!("uses.{u}"));
            }
        }
    }
    vec![("failed last run", failed), ("changed in a draft", changed)]
}

fn same_json<T: Serialize>(a: &T, b: &T) -> bool {
    matches!((serde_json::to_value(a), serde_json::to_value(b)), (Ok(x), Ok(y)) if x == y)
}

/// Each `uses:` of a workflow's jobs and their steps, as written.
fn uses_in(w: &Workflow) -> Vec<&str> {
    w.jobs.iter().flat_map(|j| j.uses.as_deref().into_iter().chain(j.steps.iter().filter_map(|s| s.uses.as_deref()))).collect()
}

/// A `uses:` an update moves (W29-R22): its name as the pane says it (`rust-check`) and as it's
/// written without its ref (`hyper-light/ci-kit/.github/workflows/rust-check.yml`), its version
/// before and after, and the commits to it between them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Moved {
    pub name: String,
    pub uses: String,
    pub from: String,
    pub to: String,
    pub commits: usize,
}

/// kiln's update draft in its ordinary groups (pl-7): each use it moves stands in Uses under the
/// version it had, marked with where it goes (`rust-check@v2.3.0 → v2.4.0 in #88`). Its key stays
/// the draft's text's, where its lines are.
pub fn mark_moved(groups: &mut [PaneGroup], moved: &[Moved], draft: Option<&str>) {
    for l in groups.iter_mut().filter(|g| g.name == "Uses").flat_map(|g| g.lines.iter_mut()) {
        let Some(m) = moved.iter().find(|m| l.key == format!("uses.{}@{}", m.uses, m.to)) else { continue };
        let from = l.value.split(" · ").next().unwrap_or_default().to_owned();
        l.name = format!("{}@{}", m.uses, m.from);
        l.value = format!("{from} · → {}{}", m.to, draft.map(|d| format!(" in {d}")).unwrap_or_default());
        l.state = Some("changes");
    }
}

/// W29-R25: a lock-change draft's uses whose locked commit it changes (each written use and the
/// commit it moves to), marked in Uses as an update's moved use is (pl-7); picked, its Changes
/// show the upstream code.
pub fn mark_relocked(groups: &mut [PaneGroup], relocked: &[(String, String)], draft: Option<&str>) {
    for l in groups.iter_mut().filter(|g| g.name == "Uses").flat_map(|g| g.lines.iter_mut()) {
        let Some((_, to)) = relocked.iter().find(|(u, _)| l.key == format!("uses.{u}")) else { continue };
        let was = l.value.split(" · ").next().unwrap_or_default().to_owned();
        l.value = format!("{was} · → {}{}", to.get(..7).unwrap_or(to), draft.map(|d| format!(" in {d}")).unwrap_or_default());
        l.state = Some("changes");
    }
}

/// A lock-change draft's strip before anything's picked, and its words in bold: what moves, how
/// much of its code changed, and whose word it waits on: `acme/tools@v1 moves to e4f5a6b · its
/// code changed in 3 files · it runs nowhere until a person accepts it`.
pub fn relock_said(relocked: &[(String, String, usize)], accepted: Option<&str>) -> (Vec<String>, Vec<String>) {
    let mut said: Vec<String> = relocked.iter().map(|(u, to, _)| format!("{u} moves to {}", to.get(..7).unwrap_or(to))).collect();
    let files: usize = relocked.iter().map(|(_, _, n)| n).sum();
    said.push(if files == 1 { "its code changed in 1 file".to_owned() } else { format!("its code changed in {files} files") });
    said.push(accepted.map_or_else(|| "it runs nowhere until a person accepts it".to_owned(), |by| format!("accepted by {}", by.strip_prefix("user:").unwrap_or(by))));
    let strong = relocked.iter().map(|(u, _, _)| u.clone()).collect();
    (said, strong)
}

/// An update draft's strip before anything's picked (pl-7), and the words it sets in bold: what's
/// out, the commits since the version it had, what it changes in short, and the draft kiln
/// opened: `rust-check v2.4.0 is out · 3 commits since v2.3.0 · a new optional input; no new
/// secrets, permissions or egress · kiln opened draft pull request #88`.
pub fn update_said(moved: &[Moved], changes: &[crate::wf::diff::Change], change: Option<&DraftChange>) -> (Vec<String>, Vec<String>) {
    let names: Vec<String> = moved.iter().map(|m| format!("{} {}", m.name, m.to)).collect();
    let mut said = Vec::new();
    match names.as_slice() {
        [] => {}
        [one] => said.push(format!("{one} is out")),
        [rest @ .., last] => said.push(format!("{} and {last} are out", rest.join(", "))),
    }
    let mut since: Vec<String> = Vec::new();
    for m in moved {
        let words = format!("{} since {}", if m.commits == 1 { "1 commit".to_owned() } else { format!("{} commits", m.commits) }, m.from);
        if !since.contains(&words) {
            since.push(words);
        }
    }
    if !since.is_empty() {
        said.push(since.join(", "));
    }
    said.push(change_summary(changes));
    let mut strong = names;
    if let Some(c) = change {
        let what = if c.kind == "diff" {
            "diff"
        } else if c.name.starts_with('!') {
            "merge request"
        } else {
            "pull request"
        };
        said.push(format!("kiln opened draft {what} {}", c.name));
        strong.push(c.name.clone());
    }
    (said, strong)
}

/// What `changes` come to in short, for an update's strip: `a new optional input; no new secrets,
/// permissions or egress`.
fn change_summary(changes: &[crate::wf::diff::Change]) -> String {
    let mut counted: Vec<(&'static str, &'static str, usize)> = Vec::new();
    for c in changes {
        let required = c.detail.as_deref() == Some("required");
        let (one, many) = match (c.kind, c.what) {
            ("input", "added") if required => ("a new required input", "new required inputs"),
            ("input", "added") => ("a new optional input", "new optional inputs"),
            ("input", "removed") => ("an input removed", "inputs removed"),
            ("input", "now required") => ("an input required now", "inputs required now"),
            ("input", "no longer required") => ("an input optional now", "inputs optional now"),
            ("secret", "added") => ("a new secret", "new secrets"),
            ("secret", "removed") => ("a secret removed", "secrets removed"),
            ("secret", "now required") => ("a secret required now", "secrets required now"),
            ("secret", "no longer required") => ("a secret optional now", "secrets optional now"),
            ("permission", "added") => ("a new permission", "new permissions"),
            ("permission", "changed") => ("a permission changed", "permissions changed"),
            ("permission", "removed") => ("a permission removed", "permissions removed"),
            ("egress", "added") => ("a new host it reaches", "new hosts it reaches"),
            ("egress", "removed") => ("a host it no longer reaches", "hosts it no longer reaches"),
            ("step", "added") => ("a new step", "new steps"),
            ("step", "removed") => ("a step taken out", "steps taken out"),
            ("job", "added") => ("a new job", "new jobs"),
            ("job", "removed") => ("a job taken out", "jobs taken out"),
            _ => ("a change", "changes"),
        };
        match counted.iter_mut().find(|(o, ..)| *o == one) {
            Some(had) => had.2 += 1,
            None => counted.push((one, many, 1)),
        }
    }
    let said: Vec<String> = counted.into_iter().map(|(one, many, n)| if n == 1 { one.to_owned() } else { format!("{n} {many}") }).collect();
    match (said.is_empty(), more_access(changes)) {
        (true, _) => "no new inputs, secrets, permissions or egress".to_owned(),
        (false, false) => format!("{}; no new secrets, permissions or egress", said.join(", ")),
        (false, true) => said.join(", "),
    }
}

/// An opened row's pane (W29-R04's `ShipPipelinePane { head, strip, groups, detail }`).
pub fn pane_json(o: &Opened<'_>, groups: &[PaneGroup]) -> Value {
    let seen = o.row.seen_by(o.viewer.name);
    let row = seen.as_ref().unwrap_or(o.row);
    let f = &row.facts;
    // A code workflow is its program (W29-R27): Code, the YAML it makes, and its plan.
    let views: &[&str] = match (f.kind.is_action(), row.dialect) {
        (true, _) => &["Changes", "YAML", "Try it"],
        (false, Some("ts" | "js")) => &["Code", "What it makes", "Plan"],
        (false, _) => &["YAML", "Flow", "Plan"],
    };
    json!({
        "head": {
            "title": f.name, "path": f.path, "kind": f.kind.as_str(), "dialect": row.dialect,
            "ref": o.at, "commit": o.sha, "draft": serde_json::to_value(&row.draft).unwrap_or(Value::Null),
            "stand": row.standing.stand, "word": row.standing.word, "pips": row.standing.pips,
        },
        "strip": {
            "said": o.said.as_ref().map_or_else(|| vec![format!("{} · {}", f.kind.words(), row.line)], |(said, _)| said.clone()),
            "strong": o.said.as_ref().map(|(_, strong)| strong.clone()).unwrap_or_default(),
            "pick": o.narrow.iter().map(|(w, _)| *w).chain(["all"]).collect::<Vec<_>>(),
            "narrow": o.narrow.iter().map(|(w, keys)| ((*w).to_owned(), json!(keys))).collect::<serde_json::Map<_, _>>(),
            "verbs": verbs_for(row, o.viewer, o.by_hand).into_iter().map(|mut v| {
                if v.verb == "release" && v.enabled {
                    v.asks = o.changes.map(release_asks);
                    v.tags = crate::releases::next_versions(f.released.as_deref(), o.changes.unwrap_or_default());
                }
                v
            }).collect::<Vec<_>>(),
        },
        "groups": groups,
        "detail": {
            "name": f.path, "views": views, "text": o.text,
            "since": o.since.map(|s| json!({ "tag": s.tag, "commits": s.commits })),
            "overlay": o.overlay.map(|(path, text)| json!({ "path": path, "text": text })),
            "changes": o.lines,
            "update": o.update.as_ref().map(|(uses, from, to)| json!({ "uses": uses, "from": from, "to": to })),
        },
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
    fn w29_r04_a_triggers_line_says_what_it_fires_on_as_the_stills_do() {
        use crate::wf::ir::Input;
        let said = |t: Trigger| {
            let l = trigger_line(&t);
            (l.value, l.note)
        };
        let words = |a: &str, b: &str| (a.to_owned(), b.to_owned());
        let x = |k: &str, v: YVal| (k.to_owned(), v);
        let s = |v: &str| YVal::Str(crate::wf::ir::Tpl { src: v.into(), ..Default::default() });
        let list = |v: &[&str]| YVal::Seq(v.iter().map(|w| s(w)).collect());
        let main = Some(vec!["main".to_owned()]);
        // pl-2's.
        assert_eq!(said(Trigger { event: "push".into(), branches: main.clone(), ..Trigger::default() }), words("main", "every push"));
        assert_eq!(said(Trigger { event: "pull_request".into(), branches: main.clone(), ..Trigger::default() }), words("into main", "drafts too"));
        assert_eq!(said(Trigger { event: "workflow_dispatch".into(), ..Trigger::default() }), words("Run, kiln run, the API", ""));
        // pl-8's.
        let target = Input { name: "target".into(), options: vec!["staging".into(), "production".into()], ..Input::default() };
        assert_eq!(said(Trigger { event: "workflow_dispatch".into(), inputs: vec![target], ..Trigger::default() }), words("inputs: target", "staging | production"));
        assert_eq!(said(Trigger { event: "deploy".into(), extra: vec![x("app", s("kiln-production")), x("states", list(&["synced"]))], ..Trigger::default() }), words("kiln-production", "synced"));
        let la = Schedule { cron: "0 3 * * *".into(), timezone: Some("America/Los_Angeles".into()), ..Schedule::default() };
        assert_eq!(said(Trigger { event: "schedule".into(), schedules: vec![la.clone()], ..Trigger::default() }), words("0 3 * * *", "03:00 Los Angeles"));
        assert_eq!(said(Trigger { event: "pipeline".into(), branches: main.clone(), extra: vec![x("name", s("ci")), x("conclusion", list(&["success"]))], ..Trigger::default() }), words("after ci", "succeeds on main"));
        assert_eq!(said(Trigger { event: "webhook".into(), extra: vec![x("hook", s("status-page")), x("filter", s("event.status != 'ok'"))], ..Trigger::default() }), words("status-page", "status isn't ok"));
        assert_eq!(said(Trigger { event: "comment".into(), extra: vec![x("commands", list(&["/smoke"]))], ..Trigger::default() }), words("/smoke", "on a diff"));
        // The rest as their filters say.
        assert_eq!(said(Trigger { event: "push".into(), branches: main.clone(), paths: Some(vec!["crates/**".into()]), ..Trigger::default() }), words("main", "paths crates/**"));
        let utc = Schedule { cron: "*/30 * * * *".into(), ..Schedule::default() };
        assert_eq!(said(Trigger { event: "schedule".into(), schedules: vec![utc.clone()], ..Trigger::default() }), words("*/30 * * * *", "UTC"));
        assert_eq!(said(Trigger { event: "schedule".into(), schedules: vec![la, utc], ..Trigger::default() }), words("2 schedules", "0 3 * * *, */30 * * * *"));
        assert_eq!(said(Trigger { event: "workflow_run".into(), types: vec!["completed".into()], workflows: vec!["ci".into()], ..Trigger::default() }), words("after ci", "completed"));
        assert_eq!(said(Trigger { event: "webhook".into(), extra: vec![x("hook", s("h")), x("filter", s("a && b"))], ..Trigger::default() }), words("h", "a && b"));
        assert_eq!(said(Trigger { event: "release".into(), types: vec!["published".into()], ..Trigger::default() }), words("published", ""));
        let odd = trigger_line(&Trigger { event: "made_up".into(), ..Trigger::default() });
        assert_eq!((odd.words.as_str(), odd.group, odd.whose), ("made up", Group::Repository, None));
    }

    #[test]
    fn w29_r04_a_jobs_line_says_its_platforms_and_its_branch_as_pl_2_does() {
        let text = "on: push\njobs:\n  ui-checks:\n    runs-on: [baremetal, docker]\n    if: github.ref == 'refs/heads/main'\n    steps: [{run: x}]\n  release-binary:\n    runs-on: [baremetal]\n    strategy:\n      matrix:\n        platform: [linux/amd64, darwin/arm64, windows/amd64]\n    steps: [{run: x}]\n  shards:\n    runs-on: [baremetal]\n    if: ${{ github.ref_name == 'release' }}\n    strategy:\n      matrix:\n        n: [1, 2]\n        m: [a, b]\n    steps: [{run: x}]\n";
        let (_, w) = crate::wf::check_model(".github/workflows/ci.yml", text, None);
        let w = w.unwrap_or_default();
        let said = |id: &str| w.job(id).map(|j| job_line(j, &Seen::default()).value).unwrap_or_default();
        assert_eq!(said("ui-checks"), "bare metal · docker · on main");
        assert_eq!(said("release-binary"), "3 platforms · bare metal");
        assert_eq!(said("shards"), "4 legs · bare metal · on release");
        assert_eq!(on_branch("github.event_name == 'push'"), None, "a condition that isn't a branch's says nothing");
    }

    #[test]
    fn w29_r04_the_pick_words_keep_the_last_runs_failed_jobs_and_what_a_draft_changes() {
        let text = "on:\n  push:\n    branches: [main]\njobs:\n  rust:\n    runs-on: local\n    steps:\n      - uses: acme/kit/setup@v1\n      - run: cargo test\n  ui:\n    runs-on: local\n    steps:\n      - run: npm test\n";
        let parse = |t: &str| crate::wf::check_model(".github/workflows/ci.yml", t, None).1.unwrap_or_default();
        let w = parse(text);
        // The newest finished run's: one under way says nothing yet, and a key that isn't a job is left out.
        let runs = [
            RunSeen { running: true, ..RunSeen::default() },
            RunSeen { outcome: Some(Outcome::Failed), failed: vec!["ui".into(), "gone".into()], ..RunSeen::default() },
            RunSeen { outcome: Some(Outcome::Failed), failed: vec!["rust".into()], ..RunSeen::default() },
        ];
        // A draft that moves the use, a line above everything: only what it says counts, not where.
        let moved = parse(&format!("# moved\n{}", text.replace("setup@v1", "setup@v2")));
        let said = narrows(&w, &runs, &[moved]);
        assert_eq!(said, [("failed last run", vec!["jobs.ui".to_owned()]), ("changed in a draft", vec!["jobs.rust".to_owned(), "uses.acme/kit/setup@v1".to_owned()])]);
        let branches = parse(&text.replace("[main]", "[main, next]"));
        assert_eq!(narrows(&w, &[], &[branches]).get(1).map(|(_, k)| k.clone()), Some(vec!["on.push".to_owned()]));
        assert_eq!(narrows(&w, &[], &[parse(text)]), [("failed last run", Vec::<String>::new()), ("changed in a draft", Vec::new())], "a draft that's the same changes nothing");
    }

    #[test]
    fn w29_r22_changes_in_sentences_as_pl_5_and_pl_7_say_them() {
        use crate::wf::diff::Change;
        let c = |kind: &'static str, name: &str, what: &'static str, detail: Option<&str>| Change { kind, name: name.to_owned(), what, detail: detail.map(str::to_owned) };
        let targets = [c("input", "targets", "added", None)];
        let said = |lines: Vec<Line>| lines.into_iter().map(|l| (l.name, l.value, l.state)).collect::<Vec<_>>();
        // pl-5: for those who use it.
        assert_eq!(said(change_lines(&targets, None)), [("a new input, targets (optional): nothing they pass changes".to_owned(), "safe".to_owned(), Some("added"))]);
        // pl-7: for the repository an update is for.
        assert_eq!(said(change_lines(&targets, Some("quiver"))), [("a new input, targets (optional): quiver passes none, so nothing changes".to_owned(), "safe".to_owned(), Some("added"))]);
        let more = [c("input", "where", "added", Some("required")), c("input", "how", "now required", None), c("permission", "contents", "changed", Some("read → write")), c("egress", "b.example", "added", None), c("step", "bye", "removed", None)];
        assert_eq!(
            said(change_lines(&more, None)).into_iter().map(|(n, v, _)| format!("{n} · {v}")).collect::<Vec<_>>(),
            [
                "a new input, where (required): each use has to pass it · breaks",
                "input how is required now: each use has to pass it · breaks",
                "permission contents: read → write · access",
                "a new host it may reach, b.example · access",
                "step bye taken out · safe",
            ]
        );
        assert_eq!(said(change_lines(&[], Some("quiver"))), [("nothing quiver passes changes".to_owned(), "same".to_owned(), Some("passed"))]);
        assert_eq!(access_line(&targets).map(|l| (l.name, l.value)), Some(("no new secrets, permissions or egress".to_owned(), "same".to_owned())));
        assert!(access_line(&more).is_none(), "an update that may reach more says so line by line");
        // pl-5's own checks, pl-7's draft run.
        let checks = checks_line("hyper-light/ci-kit", "e41a9c2b8d", Some(Outcome::Passed), false, 4);
        assert_eq!((checks.name.as_str(), checks.value.as_str(), checks.state), ("ci-kit's own checks passed on e41a9c2", "4 jobs", Some("passed")));
        let jobs = [("rust / check".to_owned(), "running".to_owned()), ("bench".to_owned(), "queued".to_owned())];
        let run = draft_run_line(Some("#88"), &jobs, None, true, 4 * 60 + 12);
        assert_eq!((run.name.as_str(), run.value.as_str(), run.state), ("#88's run on its draft: rust / check running, bench queued", "4m", Some("running")));
        assert_eq!(draft_run_line(Some("#88"), &[], Some(Outcome::Passed), false, 600).name, "#88's run on its draft passed");
        // pl-7's strip, and its bold words.
        let moved = [Moved { name: "rust-check".into(), uses: "hyper-light/ci-kit/.github/workflows/rust-check.yml".into(), from: "v2.3.0".into(), to: "v2.4.0".into(), commits: 3 }];
        let pr = DraftChange { kind: "change", name: "#88".into(), url: None };
        let (strip, strong) = update_said(&moved, &targets, Some(&pr));
        assert_eq!(strip.join(" · "), "rust-check v2.4.0 is out · 3 commits since v2.3.0 · a new optional input; no new secrets, permissions or egress · kiln opened draft pull request #88");
        assert_eq!(strong, ["rust-check v2.4.0", "#88"]);
        assert_eq!(update_said(&moved, &more, None).0.get(2).map(String::as_str), Some("a new required input, an input required now, a permission changed, a new host it reaches, a step taken out"));
        assert_eq!(update_said(&moved, &[], None).0.get(2).map(String::as_str), Some("no new inputs, secrets, permissions or egress"));
        // pl-7's Uses: the moved use under the version it had, marked, its key the draft's.
        let key = "uses.hyper-light/ci-kit/.github/workflows/rust-check.yml@v2.4.0";
        let mut groups = vec![group("Uses", vec![Line::new(key, "use", "hyper-light/ci-kit/.github/workflows/rust-check.yml@v2.4.0", "a repository kiln hosts · newest"), Line::new("uses.kiln/checkout@v1", "use", "kiln/checkout@v1", "kiln's own")])];
        mark_moved(&mut groups, &moved, Some("#88"));
        let lines: Vec<(&str, &str, &str, Option<&str>)> = groups.iter().flat_map(|g| &g.lines).map(|l| (l.key.as_str(), l.name.as_str(), l.value.as_str(), l.state)).collect();
        assert_eq!(lines, [(key, "hyper-light/ci-kit/.github/workflows/rust-check.yml@v2.3.0", "a repository kiln hosts · → v2.4.0 in #88", Some("changes")), ("uses.kiln/checkout@v1", "kiln/checkout@v1", "kiln's own", None)]);
    }

    #[test]
    fn w29_r04_a_picked_jobs_strip_says_what_it_reserves_and_its_last_run_as_pl_2_does() {
        let text = "on: push\njobs:\n  rust:\n    runs-on: [baremetal, linux]\n    resources: { cpu: 2, memory: 16Gi }\n    steps: [{run: x}]\n  ui:\n    runs-on: [baremetal]\n    steps: [{run: x}]\n  vendor:\n    runs-on: [baremetal]\n    steps: [{run: x}]\n";
        let (_, w) = crate::wf::check_model(".kiln/ci.yaml", text, None);
        let w = w.unwrap_or_default();
        let seen = Seen {
            job_secs: HashMap::from([("rust".to_owned(), 18 * 60 + 40), ("ui".to_owned(), 4 * 60)]),
            job_outcome: HashMap::from([("rust".to_owned(), Outcome::Passed), ("ui".to_owned(), Outcome::Failed)]),
            last_sha: Some("a3f2c1d9e8b7".to_owned()),
            ..Seen::default()
        };
        let note = |id: &str| w.job(id).and_then(|j| job_line(j, &seen).note).unwrap_or_default();
        assert_eq!(note("rust"), "bare metal, linux · 2 CPUs, 16 GiB · its last run passed in 18m 40s on a3f2c1d");
        assert_eq!(note("ui"), "bare metal · its last run failed after 4m on a3f2c1d");
        assert_eq!(note("vendor"), "bare metal", "a job the last run didn't time says where it runs alone");
        assert_eq!(w.job("rust").map(|j| job_line(j, &seen).value).unwrap_or_default(), "bare metal · linux · 18m", "its line keeps the stills' value");
        for (secs, said) in [(40, "40s"), (18 * 60 + 40, "18m 40s"), (18 * 60, "18m"), (2 * 3600 + 5 * 60, "2h 5m"), (3 * 86_400 + 4 * 3600, "3d 4h")] {
            assert_eq!(span_exact(secs), said);
        }
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
        Repo::new(format!("acme/r{n:03}"), "main".into(), "a3f2c1d4e5f60718293a4b5c6d7e8f9012345678".into(), rows, Access::None)
    }

    fn list(repos: &[Repo], only: Option<&[String]>, repo: Option<&str>, q: Option<&str>) -> Value {
        serde_json::from_str(&list_text(repos, only, repo, q, None)).unwrap_or_default()
    }

    #[test]
    fn w29_r02_repositories_follow_their_most_urgent_row_and_rows_keep_their_own_commit() {
        let make = |name: &str, facts: Facts| Repo::new(name.to_owned(), "main".into(), "tip".into(), vec![Row::new(Facts { runs: true, ..facts }, None, String::new(), None, None)], Access::None);
        let mut reviewed = make("z/review", Facts { draft: Some(Draft::Review), ..Facts::default() });
        let mut changed = reviewed.rows.first().cloned().unwrap_or_else(|| Row::new(Facts::default(), None, String::new(), None, None));
        changed.commit = Some("file-commit".into());
        assert_eq!(row_json("z/review", "main", "tip", &changed).get("commit").and_then(Value::as_str), Some("file-commit"));
        reviewed.rows.push(Row::new(Facts { routable: Some(false), ..Facts::default() }, None, String::new(), None, None));
        let all = [
            make("a/blocked", Facts { routable: Some(false), ..Facts::default() }),
            make("b/passing", Facts { latest: Some(Outcome::Passed), ..Facts::default() }),
            make("c/draft", Facts { draft: Some(Draft::Unpublished), ..Facts::default() }),
            make("d/failing", Facts { latest: Some(Outcome::Failed), ..Facts::default() }),
            reviewed,
        ];
        let v = list(&all, None, None, None);
        let names: Vec<&str> = v.get("repos").and_then(Value::as_array).into_iter().flatten().filter_map(|r| r.get("repo").and_then(Value::as_str)).collect();
        assert_eq!(names, ["z/review", "d/failing", "a/blocked", "c/draft", "b/passing"]);
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
            let groups = workflow_groups(&w, text, &[], true, "", None, &Seen::default());
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
        let runs = [
            RunSeen { number: 12, event: "push".into(), outcome: Some(Outcome::Failed), sha: "9e1b2c0d".into(), at: 1_000, failed: vec!["rust".into()], ..RunSeen::default() },
            RunSeen { number: 13, event: "schedule".into(), running: true, ..RunSeen::default() },
        ];
        let seen = Seen { job_secs: [("build".to_owned(), 18 * 60 + 40)].into(), set_in: [("TOKEN".to_owned(), "repository".to_owned())].into(), now: 1_000 + 2 * 3600, ..Seen::default() };
        let groups = workflow_groups(&w, EVERY_GITHUB_EVENT, &runs, true, "", None, &seen);
        let names: Vec<&str> = groups.iter().map(|g| g.name.as_str()).collect();
        assert_eq!(names, ["Triggers", "Called with", "Stage 1", "Stage 2", "Uses", "Secrets and variables", "Runs", "Settings"]);
        let lines = |n: &str| groups.iter().find(|g| g.name == n).map(|g| g.lines.clone()).unwrap_or_default();
        let call: Vec<(&str, String)> = lines("Called with").iter().map(|l| (l.kind, l.name.clone())).collect();
        assert_eq!(call, [("input", "toolchain".to_owned()), ("output", "built".into()), ("secret", "TOKEN".into())], "a reusable workflow's workflow_call inputs, outputs and secrets");
        let push = lines("Triggers").into_iter().find(|l| l.key == "on.push").unwrap_or_else(|| Line::new("", "trigger", "", ""));
        assert_eq!((push.value.as_str(), push.note.as_deref(), push.group, push.whose), ("main", Some("paths crates/**"), Some("Code"), Some(Whose::Both)));
        assert_eq!(lines("Stage 2").first().map(|l| (l.name.clone(), l.value.clone())), Some(("test".to_owned(), "bare metal · linux · needs build".to_owned())));
        assert_eq!(lines("Stage 1").first().map(|l| l.value.clone()), Some("bare metal · linux · x64 · 18m".to_owned()), "its last run's time");
        let uses: Vec<String> = lines("Uses").iter().map(|l| format!("{}: {}", l.name, l.value)).collect();
        assert_eq!(uses, ["actions/checkout@v4: fetched by name", "./.github/actions/setup: this repository"]);
        let names_only: Vec<(&str, String, String)> = lines("Secrets and variables").iter().map(|l| (l.kind, l.name.clone(), l.value.clone())).collect();
        assert_eq!(names_only, [("secret", "TOKEN".to_owned(), "repository".to_owned()), ("variable", "REGION".into(), "used".into())]);
        assert!(groups.iter().flat_map(|g| &g.lines).all(|l| !l.value.contains("${{ secrets")), "names only, never a value");
        let runs: Vec<(String, String, Option<&str>, Option<String>)> = lines("Runs").iter().map(|l| (l.name.clone(), l.value.clone(), l.state, l.note.clone())).collect();
        assert_eq!(runs, [("9e1b2c0".to_owned(), "rust failed · 2h".to_owned(), Some("failed"), Some("push".to_owned())), ("#13".into(), "under way".into(), Some("running"), Some("schedule".into()))]);
        let off = workflow_groups(&w, EVERY_GITHUB_EVENT, &[], false, "GitHub workflows not run: this commit has .kiln/ workflows", None, &Seen::default());
        assert_eq!(off.iter().find(|g| g.name == "Settings").and_then(|g| g.lines.first()).map(|l| l.name.clone()), Some("Not run here".to_owned()));
    }

    #[test]
    fn w29_r26_settings_say_the_lock_the_trunk_requires_and_moved_tags_and_refused_runs_say_why() {
        use crate::lock::verify::{MovedTags, Require};
        let (_, w) = crate::wf::check_model(".github/workflows/every.yml", EVERY_GITHUB_EVENT, None);
        let w = w.unwrap_or_default();
        let settings = |lock: LockSeen| {
            let seen = Seen { lock: Some(lock), ..Seen::default() };
            let groups = workflow_groups(&w, EVERY_GITHUB_EVENT, &[], true, "", None, &seen);
            groups.into_iter().find(|g| g.name == "Settings").map(|g| g.lines.into_iter().filter(|l| l.key.contains("lock") || l.key.contains("moved")).map(|l| (l.name, l.value, l.state)).collect::<Vec<_>>()).unwrap_or_default()
        };
        assert_eq!(settings(LockSeen::default()), [], "Settings says nothing of locks it doesn't need to");
        assert_eq!(settings(LockSeen { held: Some((1, 1)), ..LockSeen::default() }), [("Locked".to_owned(), "its one use at its commit".to_owned(), Some("passed")), ("Require lockfile".into(), "off".into(), Some("none"))]);
        assert_eq!(settings(LockSeen { moved_tags: MovedTags::Refuse, ..LockSeen::default() }), [("Moved tags".to_owned(), "refused when a full version moves".to_owned(), Some("none"))]);
        let locked = LockSeen { held: Some((3, 3)), require: Require::Enforce, ..LockSeen::default() };
        assert_eq!(settings(locked.clone()).into_iter().take(2).collect::<Vec<_>>(), [("Locked".to_owned(), "3 uses at their commits".to_owned(), Some("passed")), ("Require lockfile".into(), "enforce: what isn't locked doesn't run".into(), Some("passed"))]);
        let stale = LockSeen { held: Some((2, 3)), missing: Some("acme/other@v2".into()), ..locked };
        assert_eq!(settings(stale).into_iter().take(2).collect::<Vec<_>>(), [("Locked".to_owned(), "acme/other@v2 isn't locked yet".to_owned(), Some("flag")), ("Require lockfile".into(), "enforce: what isn't locked doesn't run".into(), Some("failed"))]);
        let report = LockSeen { require: Require::Report, moved_tags: MovedTags::Refuse, moved: vec!["acme/tools@v1.2.3".into(), "acme/kit@v2.0.1".into()], ..LockSeen::default() };
        assert_eq!(settings(report), [
            ("Not locked".to_owned(), "its uses follow their tags".to_owned(), Some("flag")),
            ("Require lockfile".into(), "report: its runs say what isn't locked".into(), Some("flag")),
            ("Moved tags".into(), "acme/tools@v1.2.3 and 1 more moved: refused".into(), Some("failed")),
        ]);
        assert_eq!(refusal_code("KCI4055 .github/workflows/ci.yml isn't locked"), Some("KCI4055".to_owned()));
        assert_eq!(refusal_code("KCI4051: the source is unavailable"), Some("KCI4051".to_owned()));
        assert_eq!(refusal_code("runner went away"), None);
        let runs = [RunSeen { number: 7, outcome: Some(Outcome::Failed), sha: "9e1b2c0d".into(), refused: refusal_code("KCI4056 acme/tools@v1.2.3 moved"), ..RunSeen::default() }];
        let groups = workflow_groups(&w, EVERY_GITHUB_EVENT, &runs, true, "", None, &Seen::default());
        let run = groups.iter().find(|g| g.name == "Runs").and_then(|g| g.lines.first()).map(|l| (l.value.clone(), l.state));
        assert_eq!(run, Some(("refused (KCI4056)".to_owned(), Some("failed"))));
    }

    #[test]
    fn w29_r25_a_lock_change_drafts_moved_uses_and_its_strip() {
        let mut groups = vec![group("Uses", vec![Line::new("uses.acme/tools@v1", "use", "acme/tools@v1", "fetched by name"), Line::new("uses.kiln/checkout@v1", "use", "kiln/checkout@v1", "kiln's own")])];
        mark_relocked(&mut groups, &[("acme/tools@v1".to_owned(), "e4f5a6b7c8d9".to_owned())], Some("D12"));
        let lines: Vec<_> = groups.iter().flat_map(|g| &g.lines).map(|l| (l.value.clone(), l.state)).collect();
        assert_eq!(lines, [("fetched by name · → e4f5a6b in D12".to_owned(), Some("changes")), ("kiln's own".into(), None)]);
        let (said, strong) = relock_said(&[("acme/tools@v1".to_owned(), "e4f5a6b7c8d9".to_owned(), 3)], None);
        assert_eq!(said, ["acme/tools@v1 moves to e4f5a6b", "its code changed in 3 files", "it runs nowhere until a person accepts it"]);
        assert_eq!(strong, ["acme/tools@v1"]);
        assert_eq!(relock_said(&[("acme/tools@v1".to_owned(), "e4f5a6b".to_owned(), 1)], Some("user:ada")).0.last().map(String::as_str), Some("accepted by ada"));
    }

    #[test]
    fn w29_r04_an_actions_groups_by_kind() {
        let cases = [
            ("name: setup\ninputs:\n  toolchain:\n    description: Which toolchain\n    required: true\n  cache:\n    default: 'true'\noutputs:\n  path:\n    description: Where it went\nruns:\n  using: composite\n  steps:\n    - name: Install\n      run: echo installing\n      shell: bash\n    - uses: actions/cache@v4\n", Kind::Composite, "Steps", vec!["Install: run", "actions/cache@v4: "]),
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
            let groups = action_groups(&a, kind, text, "a3f2c1d4e5f60718", &tags, &Sharing { repo: "acme/kit", access: Access::None, used_by: &[] }, &ActionSeen::default());
            let names: Vec<&str> = groups.iter().map(|g| g.name.as_str()).collect();
            // Outputs only where it has some (pl-5 has none).
            let want_names: Vec<&str> = if text.contains("outputs:") { vec!["Inputs", "Outputs", own, "Versions", "Used by", "Settings"] } else { vec!["Inputs", own, "Versions", "Used by", "Settings"] };
            assert_eq!(names, want_names, "{kind:?}");
            let lines: Vec<String> = groups.iter().find(|g| g.name == own).map(|g| g.lines.iter().map(|l| format!("{}: {}", l.name, l.value)).collect()).unwrap_or_default();
            assert_eq!(lines, want, "{kind:?}");
            let versions: Vec<String> = groups.iter().find(|g| g.name == "Versions").map(|g| g.lines.iter().map(|l| l.name.clone()).collect()).unwrap_or_default();
            assert_eq!(versions, ["a3f2c1d", "v2.4.0"]);
        }
        let (a, _) = crate::wf::parse_action(cases_text());
        let groups = action_groups(&a.unwrap_or_default(), Kind::Composite, cases_text(), "a3f2", &[], &Sharing { repo: "acme/kit", access: Access::None, used_by: &[] }, &ActionSeen::default());
        let inputs: Vec<String> = groups.iter().find(|g| g.name == "Inputs").map(|g| g.lines.iter().map(|l| format!("{}: {}", l.name, l.value)).collect()).unwrap_or_default();
        assert_eq!(inputs, ["toolchain: required", "cache: true"]);
    }

    /// pl-5's lines: inputs by what they take when not given, one added since the newest release
    /// saying so; steps by what they use and are given; the tip by the commits since the newest
    /// release, each release by its age and users, the floating tag by what it stands for; Used by
    /// with its workflows, marked where it's behind.
    #[test]
    fn w29_r04_an_actions_lines_as_pl_5_says_them() {
        let text = "name: setup-rust\ninputs:\n  toolchain:\n    description: the toolchain; rust-toolchain.toml's when not given\n    required: false\n  components:\n    default: clippy, rustfmt\n  targets:\n    required: false\nruns:\n  using: composite\n  steps:\n    - uses: kiln/cache@v1\n      with: { path: \"~/.cargo, target\" }\n    - name: rustup toolchain install\n      run: rustup toolchain install stable\n      shell: bash\n    - name: rustc --version\n      run: rustc --version\n      shell: bash\n";
        let (a, _) = crate::wf::parse_action(text);
        let tags = vec![("v2.4.0".to_owned(), "7ac3e1d".to_owned()), ("v2.3.0".to_owned(), "5b2c9e1".to_owned()), ("v2".to_owned(), "7ac3e1d".to_owned())];
        let consumer = |repo: &str, workflows: &[&str], r: &str| crate::catalog::Consumer { repo: repo.into(), git_ref: r.into(), workflows: workflows.iter().map(|w| (*w).to_owned()).collect(), ..crate::catalog::Consumer::default() };
        let used = [consumer("hyper-light/kiln", &["ci", "release"], "v2.3.0"), consumer("hyper-light/quiver", &["ci"], "v2.4.0")];
        let day = 86_400;
        let seen = ActionSeen { tag_at: [("v2.4.0".to_owned(), 0), ("v2.3.0".to_owned(), -18 * day)].into(), since: Some(("v2.4.0".to_owned(), 3)), added: vec!["targets".into()], now: 3 * day, ..Default::default() };
        let groups = action_groups(&a.unwrap_or_default(), Kind::Composite, text, "e41a9c2b7d6e", &tags, &Sharing { repo: "hyper-light/ci-kit", access: Access::Owner, used_by: &used }, &seen);
        let said = |n: &str| groups.iter().find(|g| g.name == n).map(|g| g.lines.iter().map(|l| (format!("{}: {}", l.name, l.value), l.state)).collect::<Vec<_>>()).unwrap_or_default();
        assert_eq!(said("Inputs"), [("toolchain: rust-toolchain.toml's".to_owned(), None), ("components: clippy, rustfmt".into(), None), ("targets: new since v2.4.0".into(), Some("added"))]);
        assert_eq!(said("Steps"), [("kiln/cache@v1: ~/.cargo, target".to_owned(), None), ("rustup toolchain install: run".into(), None), ("rustc --version: its output".into(), None)]);
        assert_eq!(said("Versions").into_iter().map(|(l, _)| l).collect::<Vec<_>>(), ["e41a9c2: 3 commits since v2.4.0", "v2.4.0: 3d · 1 repository", "v2.3.0: 3w · 1 repository", "v2: the newest v2: v2.4.0"]);
        assert_eq!(said("Used by"), [("hyper-light/kiln: ci, release · v2.3.0".to_owned(), Some("flag")), ("hyper-light/quiver: ci · v2.4.0".into(), None)]);
        assert!(groups.iter().all(|g| g.name != "Outputs"), "no Outputs group without outputs");
    }

    #[test]
    fn w29_r26_a_moved_version_is_flagged_on_its_existing_catalog_line() {
        let text = "runs: {using: composite, steps: []}\n";
        let (action, _) = crate::wf::parse_action(text);
        let action = action.unwrap_or_default();
        let tags = [("v1.2.3".into(), "b".repeat(40))];
        let seen = ActionSeen { moved: [("v1.2.3".into(), ("a".repeat(40), "b".repeat(40)))].into(), ..Default::default() };
        let groups = action_groups(&action, Kind::Composite, text, "bbbbbbb", &tags, &Sharing { repo: "acme/tools", access: Access::Owner, used_by: &[] }, &seen);
        let line = groups.iter().flat_map(|g| &g.lines).find(|l| l.name == "v1.2.3");
        assert!(line.is_some_and(|l| l.state == Some("flag") && l.value == "tag moved · aaaaaaa → bbbbbbb"));
    }

    /// W29-R04: event subjects and floating releases come from the supplied records, with no
    /// inference of a release from an unrelated commit; absent event facts leave the SHA fallback.
    #[test]
    fn w29_r04_recorded_run_subjects_and_floating_releases() {
        use serde_json::json;
        assert_eq!(run_subject("deploy", &json!({"kiln":{"deploy":{"state":"synced", "operation":{"id":153}}}})), Some("#153 synced".into()));
        assert_eq!(run_subject("schedule", &json!({"schedule":"0 3 * * *"})), Some("03:00".into()));
        assert_eq!(run_subject("schedule", &json!({"schedule":"*/15 * * * *"})), Some("*/15 * * * *".into()));
        assert_eq!(run_subject("comment", &json!({"issue":{"number":212}, "comment":{"body":"/smoke some private arguments\nmore text"}})), Some("/smoke on #212".into()));
        assert_eq!(run_subject("comment", &json!({"issue":{"number":212}, "comment":{"body":"a conversation"}})), None);
        assert_eq!(run_subject("deploy", &json!({})), None);
        assert_eq!(floating_words("v4", ["v4", "v4.2.3", "v4.2.2", "v5.0.0"], "5a3ec84d4a"), Some("4.2.3 · 5a3ec84".into()));
        assert_eq!(floating_words("v4.2", ["v4.2.3", "v4.3.0"], "5a3ec84d4a"), Some("4.2.3 · 5a3ec84".into()));
        assert_eq!(floating_words("v4", ["v4"], "5a3ec84d4a"), None);
        assert_eq!(floating_words("v4.2.3", ["v4.2.3"], "5a3ec84d4a"), None);
        let groups = workflow_groups(&Workflow::default(), "", &[], true, "", None, &Seen::default());
        assert!(!groups.iter().any(|g| matches!(g.name.as_str(), "Uses" | "Secrets and variables")), "no empty groups");
    }

    /// W29-R04, pl-7: cached callee facts name its job and machine, and put its dependencies
    /// after the call, once each. Cycles cannot recurse; absent facts preserve the direct call.
    #[test]
    fn w29_r04_a_cached_callees_job_and_dependencies() {
        let reference = "acme/kit/.github/workflows/check.yml@v2.4.0";
        let caller = format!("on: push\njobs:\n  rust:\n    uses: {reference}\n  bench:\n    runs-on: local\n    steps:\n      - uses: kiln/checkout@v1\n");
        let child = "on: workflow_call\njobs:\n  check:\n    runs-on: local\n    steps:\n      - uses: acme/kit/.github/actions/setup@v2.4.0\n      - uses: kiln/checkout@v1\n";
        let w = crate::wf::parse_workflow(".github/workflows/ci.yml", &caller, crate::expr::Dialect::Gha).0.unwrap_or_default();
        let callee = crate::wf::parse_workflow(".github/workflows/check.yml", child, crate::expr::Dialect::Gha).0.unwrap_or_default();
        let mut seen = Seen { called: [(reference.to_owned(), called_seen(&callee))].into(), ..Seen::default() };
        let groups = workflow_groups(&w, &caller, &[], true, "", None, &seen);
        let job = groups.iter().find(|g| g.name == "Jobs").and_then(|g| g.lines.first());
        assert_eq!(job.map(|l| (l.key.as_str(), l.name.as_str(), l.value.as_str())), Some(("jobs.rust", "rust / check", format!("calls {reference} · bare metal").as_str())));
        let want = [reference, "acme/kit/.github/actions/setup@v2.4.0", "kiln/checkout@v1"];
        assert_eq!(pane_uses(&w, &seen).iter().map(|(u, _)| u.as_str()).collect::<Vec<_>>(), want);
        if let Some(c) = seen.called.get_mut(reference) {
            c.uses.push((reference.into(), true));
        }
        assert_eq!(pane_uses(&w, &seen).iter().map(|(u, _)| u.as_str()).collect::<Vec<_>>(), want);
        assert_eq!(w.jobs.first().map(|j| job_line(j, &Seen::default()).name), Some("rust".into()));
        let delegated = Workflow { jobs: vec![crate::wf::Job { uses: Some(reference.into()), ..crate::wf::Job::default() }], ..Workflow::default() };
        assert_eq!(called_seen(&delegated).machine, None, "a delegated job's machine is not guessed");
    }

    /// W29-R17, R20: a shared repository's head says its access and its heading says shared; an
    /// action's pane and a reusable workflow's say who used which version and the Access, and a
    /// workflow that isn't called has neither.
    #[test]
    fn w29_r17_access_and_used_by_in_panes() {
        let used = [crate::catalog::Consumer { repo: "acme/app".into(), path: ".github/actions/setup".into(), kind: "action".into(), git_ref: "v1".into(), sha: "0123456789abcdef0123".into(), runs: 3, last_at: 9, workflows: vec!["ci".into()] }];
        let sharing = Sharing { repo: "acme/kit", access: Access::Owner, used_by: &used };
        let (a, _) = crate::wf::parse_action(cases_text());
        let groups = action_groups(&a.unwrap_or_default(), Kind::Composite, cases_text(), "a3f2", &[], &sharing, &ActionSeen::default());
        let lines = |groups: &[PaneGroup], n: &str| groups.iter().find(|g| g.name == n).map(|g| g.lines.clone()).unwrap_or_default();
        let by: Vec<(String, String)> = lines(&groups, "Used by").iter().map(|l| (l.name.clone(), l.value.clone())).collect();
        assert_eq!(by, [("acme/app".to_owned(), "ci · v1".to_owned())]);
        let access = lines(&groups, "Settings").into_iter().find(|l| l.key == "settings.access").map(|l| (l.value, l.state));
        assert_eq!(access, Some(("shared with acme's repositories".to_owned(), Some("owner"))));
        let none = Sharing { repo: "acme/kit", access: Access::None, used_by: &[] };
        let groups = action_groups(&Action::default(), Kind::Composite, "", "a3f2", &[], &none, &ActionSeen::default());
        assert_eq!(lines(&groups, "Used by").first().map(|l| (l.kind, l.state)), Some(("note", Some("none"))));
        assert_eq!(lines(&groups, "Settings").first().map(|l| l.state), Some(Some("none")));

        let call = "on:\n  workflow_call:\njobs:\n  b:\n    runs-on: local\n    steps:\n      - run: echo\n";
        let (_, w) = crate::wf::check_model(".github/workflows/build.yml", call, None);
        let groups = workflow_groups(&w.unwrap_or_default(), call, &[], true, "", Some(&sharing), &Seen::default());
        let names: Vec<&str> = groups.iter().map(|g| g.name.as_str()).collect();
        assert_eq!(names.iter().rev().take(2).copied().collect::<Vec<_>>(), ["Settings", "Used by"], "{names:?}");
        assert!(lines(&groups, "Settings").iter().any(|l| l.key == "settings.access" && l.state == Some("owner")));
        let push = "on: push\njobs:\n  b:\n    runs-on: local\n    steps:\n      - run: echo\n";
        let (_, w) = crate::wf::check_model(".github/workflows/ci.yml", push, None);
        let plain = workflow_groups(&w.unwrap_or_default(), push, &[], true, "", Some(&sharing), &Seen::default());
        assert!(!plain.iter().any(|g| g.name == "Used by"), "a workflow no one calls has no Used by");

        let shared = Repo::new("acme/kit".into(), "main".into(), "a".repeat(40), vec![], Access::Owner);
        let head: Value = serde_json::from_str(&shared.whole).unwrap_or_default();
        assert_eq!(head.get("access"), Some(&json!("owner")), "{head}");
        assert!(shared.say.starts_with("shared"), "{}", shared.say);
        let mine = Repo::new("acme/app".into(), "main".into(), "a".repeat(40), vec![], Access::None);
        assert!(!mine.say.contains("shared"), "{}", mine.say);
    }

    fn cases_text() -> &'static str {
        "name: setup\ninputs:\n  toolchain:\n    description: Which toolchain\n    required: true\n  cache:\n    default: 'true'\nruns:\n  using: composite\n  steps:\n    - run: echo hi\n      shell: bash\n"
    }

    #[test]
    fn w29_r04_verbs_by_draft_state() {
        let ada = Viewer { name: "ada", can_submit: true, can_land: true };
        let sam = Viewer { name: "sam", can_submit: true, can_land: false };
        let reader = Viewer { name: "rae", can_submit: false, can_land: false };
        let draft = |state| DraftOf { id: "d1".into(), state, author: "ada".into(), change: Some(DraftChange { kind: "diff", name: "D12".into(), url: None }) };
        let wf = Row::new(facts(Kind::Workflow), Some("gha"), String::new(), None, None);
        let saved = wf.clone().drafted(draft(Draft::Unpublished));
        let review = wf.clone().drafted(draft(Draft::Review));
        let update = wf.clone().drafted(DraftOf { id: "d2".into(), state: Draft::Update, author: "kiln".into(), change: None });
        let said = |row: &Row, v: Viewer<'_>, by_hand: bool| -> Vec<(&'static str, bool, Option<&'static str>)> { verbs_for(row, v, by_hand).into_iter().map(|x| (x.verb, x.enabled, x.why_not)).collect() };
        let on = |row: &Row, v: Viewer<'_>, by_hand: bool| -> Vec<&'static str> { verbs_for(row, v, by_hand).into_iter().filter(|x| x.enabled).map(|x| x.verb).collect() };
        // The author's own draft: everything but Release, which is for what's published.
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
        // A reusable workflow runs where it's called; an action runs by Try it.
        let reusable = Row::new(facts(Kind::Reusable), Some("gha"), String::new(), None, None);
        assert!(on(&reusable, ada, true).iter().all(|v| *v != "run"));
        // Release, for one who may land on it (W29-R21).
        assert!(on(&reusable, ada, false).contains(&"release"));
        assert!(said(&reusable, sam, false).iter().any(|(v, enabled, why)| *v == "release" && !enabled && why.is_some_and(|w| w.contains("diffs.land"))));
        assert!(on(&Row::new(facts(Kind::Composite), None, String::new(), None, None), ada, false).contains(&"run"));
        // kiln's update drafts are the repository's (pl-7): whoever may submit runs, readies or discards them.
        assert_eq!(on(&update, ada, false), ["run", "save_draft", "ready", "discard"]);
        assert!(on(&update, reader, false).is_empty());
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
        let review = mk("stress", Kind::Workflow).drafted(DraftOf { id: "d3".into(), state: Draft::Review, author: "ada".into(), change: Some(DraftChange { kind: "change", name: "#5".into(), url: Some("https://github.com/acme/app/pull/5".into()) }) });
        let repo = Repo::new("acme/app".into(), "main".into(), "a".repeat(40), vec![mk("ci", Kind::Workflow), review], Access::None);
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
