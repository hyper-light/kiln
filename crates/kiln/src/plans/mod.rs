//! Plans (docs/plans.md): what an agent will do, kept apart from any one session. A plan is
//! captured from an agent's plan mode or written by hand, changed (each change a version), shared
//! by a link, and run at will on any agent of any harness: as one message, or step by step, each
//! step a message waiting in the agent's queue for the turn before it to end. The same operations
//! serve the console (`/api/plans`), agents (kiln's MCP server: `plans.*`) and code mode.

pub mod agents;
pub mod criteria;
pub mod ops;
pub mod stack;

use rusqlite::{OptionalExtension as _, params};
use serde::{Deserialize, Serialize};

pub use criteria::{Criterion, Op, Test};
use crate::harness::model::{Command, SendMode};
use crate::harness::registry::Sent;
use crate::hub::Hub;

/// A title's characters at the most, and a plan's bytes.
const TITLE: usize = 200;
const BODY: usize = 256 << 10;
/// Tags at the most, and each one's characters.
const TAGS: usize = 20;
const TAG: usize = 40;
/// Steps one run sends at the most.
const STEPS: usize = 100;
/// Items a plan has at the most, and each field's bytes.
const ITEMS: usize = 100;
const FIELD: usize = 16 << 10;

/// A plan, as kept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    pub id: String,
    pub title: String,
    /// Markdown; its top-level list items are its steps.
    pub body: String,
    pub tags: Vec<String>,
    /// Where it came from, when an agent made it.
    pub source: Option<Source>,
    /// The token of its share link, when it is shared.
    pub share: Option<String>,
    pub version: i64,
    pub created_at: i64,
    pub updated_at: i64,
    /// Its items: the work, piece by piece, each with what makes it done.
    pub items: Vec<Item>,
    /// Its steps: its items, else its body's top-level list items.
    pub steps: Vec<String>,
    /// How many times it was run.
    pub runs: i64,
    /// `planning` (made from a goal: its agent researches it and writes its steps, then says it's
    /// ready; it isn't run until then) or `ready`.
    pub phase: String,
    /// Its research: what its steps rest on (`crate::research`).
    pub research: Option<String>,
}

/// One step of a plan: a piece of the work, what it's like, what makes it done, and how it's
/// tested.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Item {
    /// What it is, short.
    pub title: String,
    /// What to do (markdown).
    pub description: String,
    /// Examples of what's meant.
    pub examples: String,
    /// When it's done: criteria, each in words or a measure.
    #[serde(deserialize_with = "criteria::criteria_list")]
    pub acceptance: Vec<Criterion>,
    /// How it's tested: scenarios, each with what it passes by.
    #[serde(deserialize_with = "criteria::test_list")]
    pub testing: Vec<Test>,
    /// The steps it waits for, by number (from 1): run with an agent per step, it starts once each
    /// of them is met, and steps that don't wait for each other run side by side. None in the whole
    /// plan: each step waits for the one before.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub needs: Vec<usize>,
    /// The findings of its plan's research it rests on (their ids: `f1`, …).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub basis: Vec<String>,
}

/// Criteria and tests a step has at the most.
const CRITERIA: usize = 50;

impl Item {
    /// The step as text for an agent: its title, what to do, examples, then its acceptance
    /// criteria and tests, a sentence each.
    pub fn text(&self) -> String {
        let mut out = self.title.trim().to_owned();
        let (d, e) = (self.description.trim(), self.examples.trim());
        if !d.is_empty() {
            out.push_str(&format!("\n\n{d}"));
        }
        if !e.is_empty() {
            out.push_str(&format!("\n\nExamples:\n{e}"));
        }
        if !self.acceptance.is_empty() {
            out.push_str("\n\nAcceptance criteria:");
            for c in &self.acceptance {
                out.push_str(&format!("\n- {}", c.sentence()));
            }
        }
        if !self.testing.is_empty() {
            out.push_str("\n\nTests:");
            for t in &self.testing {
                out.push_str(&format!("\n- {}", t.sentence()));
            }
        }
        out
    }

    /// The step as a goal: one line, what it is and what makes it done (its harness's /goal
    /// checks the condition, and takes it as what to work toward).
    pub fn goal(&self) -> String {
        let join = |v: Vec<String>| v.join("; ");
        let parts = [
            self.title.trim().to_owned(),
            self.description.trim().to_owned(),
            if self.examples.trim().is_empty() { String::new() } else { format!("Examples: {}", self.examples.trim()) },
            if self.acceptance.is_empty() { String::new() } else { format!("Done when: {}", join(self.acceptance.iter().map(Criterion::sentence).collect())) },
            if self.testing.is_empty() { String::new() } else { format!("Tests: {}", join(self.testing.iter().map(Test::sentence).collect())) },
        ];
        let line = parts.iter().filter(|p| !p.is_empty()).cloned().collect::<Vec<_>>().join(" — ").split_whitespace().collect::<Vec<_>>().join(" ");
        if line.chars().count() > 1990 { format!("{}…", line.chars().take(1990).collect::<String>()) } else { line }
    }

    /// What's wrong with it, if anything (`n`: its number).
    fn problem(&self, n: usize) -> Option<String> {
        if self.title.trim().is_empty() {
            return Some(format!("step {n} needs a title"));
        }
        if self.title.chars().count() > TITLE {
            return Some(format!("step {n}'s title is {TITLE} characters at the most"));
        }
        if self.basis.len() > CRITERIA || self.basis.iter().any(|b| b.trim().is_empty() || b.chars().count() > 40) {
            return Some(format!("step {n}'s basis is the ids of up to {CRITERIA} findings of its research (f1, f2 …)"));
        }
        if self.acceptance.len() > CRITERIA || self.testing.len() > CRITERIA || self.testing.iter().any(|t| t.criteria.len() > CRITERIA) {
            return Some(format!("step {n} has {CRITERIA} criteria and {CRITERIA} tests at the most"));
        }
        let size = self.description.len() + self.examples.len() + serde_json::to_string(&self.acceptance).map_or(0, |s| s.len()) + serde_json::to_string(&self.testing).map_or(0, |s| s.len());
        if size > FIELD * 4 {
            return Some(format!("step {n} is {} KiB at the most", (FIELD * 4) >> 10));
        }
        if let Some(p) = self.acceptance.iter().find_map(Criterion::problem) {
            return Some(format!("step {n}: {p}"));
        }
        for (i, t) in self.testing.iter().enumerate() {
            if t.scenario.trim().is_empty() && t.criteria.is_empty() {
                return Some(format!("step {n}, test {}: say its scenario", i + 1));
            }
            if let Some(p) = t.criteria.iter().find_map(Criterion::problem) {
                return Some(format!("step {n}, test {}: {p}", i + 1));
            }
        }
        None
    }
}

/// Step `n` (from 1) of plan `title` as goals of its own (`crate::goals::split`): each acceptance
/// criterion, then each test, a goal each, one line saying which step it's of.
pub fn part_goals(title: &str, n: usize, it: &Item) -> Vec<String> {
    let of = format!("\"{}\" (step {n} of the plan \"{}\")", it.title.trim(), title.trim());
    let (a, t) = (it.acceptance.len(), it.testing.len());
    let criteria = it.acceptance.iter().enumerate().map(|(k, c)| format!("{of}, acceptance criterion {} of {a}: {}", k + 1, c.sentence()));
    let tests = it.testing.iter().enumerate().map(|(k, x)| format!("{of}, test {} of {t}: {}", k + 1, x.sentence()));
    criteria
        .chain(tests)
        .map(|g| {
            let line = g.split_whitespace().collect::<Vec<_>>().join(" ");
            if line.chars().count() > 1990 { format!("{}…", line.chars().take(1990).collect::<String>()) } else { line }
        })
        .collect()
}

/// What each step waits for (indexes from 0), as a run of one agent per step runs them: as the
/// steps say (`Item::needs`); none saying, each step the one before (or none at all, side by side:
/// `parallel`). `n`: the steps run.
pub fn waits(items: &[Item], n: usize, parallel: bool) -> Vec<Vec<usize>> {
    if items.iter().take(n).any(|it| !it.needs.is_empty()) {
        return (0..n).map(|i| items.get(i).map(|it| it.needs.iter().filter_map(|d| d.checked_sub(1)).filter(|d| *d < n && *d != i).collect()).unwrap_or_default()).collect();
    }
    (0..n).map(|i| if parallel || i == 0 { Vec::new() } else { vec![i - 1] }).collect()
}

/// The steps in rounds: each round's steps wait only for steps of the rounds before, so a round's
/// run side by side (with nothing to slow them, a run goes round by round). Steps in a loop of
/// waiting are left out (`check` refuses a plan with one).
pub fn rounds(needs: &[Vec<usize>]) -> Vec<Vec<usize>> {
    let mut done = vec![false; needs.len()];
    let mut out: Vec<Vec<usize>> = Vec::new();
    loop {
        let round: Vec<usize> = (0..needs.len()).filter(|&i| !done.get(i).copied().unwrap_or(true) && needs.get(i).is_some_and(|d| d.iter().all(|&j| done.get(j).copied().unwrap_or(true)))).collect();
        if round.is_empty() {
            return out;
        }
        for &i in &round {
            if let Some(d) = done.get_mut(i) {
                *d = true;
            }
        }
        out.push(round);
    }
}

/// The session a plan was captured from.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Source {
    pub session: Option<String>,
    pub harness: Option<String>,
    pub machine: Option<String>,
    pub cwd: Option<String>,
    pub agent: Option<String>,
    /// The goal it was made from (`goals_plan`): once every step is met, so is the goal.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub goal: Option<GoalOf>,
    /// The plan its agent proposed (plan mode), kept by kiln: one an agent, a version each time it
    /// proposes another ([`keep_proposed`]).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub proposed: bool,
}

/// A goal a plan was made from: its agent's, by id, in the words it had.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GoalOf {
    pub agent: String,
    pub id: String,
    pub text: String,
}

/// One version of a plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Version {
    pub version: i64,
    pub title: String,
    pub body: String,
    pub items: Vec<Item>,
    pub at: i64,
    /// `admin`, or `agent:<who>`.
    pub by: String,
}

/// A run of a plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Run {
    pub id: String,
    pub version: i64,
    /// The agent it went to (none for one agent per step: see `agents`).
    pub session: String,
    pub mode: String,
    pub at: i64,
    pub by: String,
    /// One agent per step: `running`, `stopped` or `done`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
}

/// A new plan.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct New {
    pub title: String,
    pub body: String,
    #[serde(deserialize_with = "given_items")]
    pub items: Vec<Item>,
    pub tags: Vec<String>,
    pub source: Option<Source>,
}

/// A change: what is given changes.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Patch {
    pub title: Option<String>,
    pub body: Option<String>,
    #[serde(deserialize_with = "given_some_items")]
    pub items: Option<Vec<Item>>,
    pub tags: Option<Vec<String>>,
}

/// Steps as given, what's wrong with one saying which it is ("step 2: criterion 1: …").
fn given_items<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<Item>, D::Error> {
    given_some_items(d).map(Option::unwrap_or_default)
}

fn given_some_items<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Vec<Item>>, D::Error> {
    use serde::de::Error as _;
    let Some(items) = Option::<Vec<serde_json::Value>>::deserialize(d)? else { return Ok(None) };
    let step = |n: usize, v| serde_json::from_value(v).map_err(|e| D::Error::custom(format!("step {n}: {e}")));
    items.into_iter().enumerate().map(|(i, v)| step(i + 1, v)).collect::<Result<_, _>>().map(Some)
}

/// Where a plan runs.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Target {
    /// An agent that runs already.
    Session(String),
    /// A new agent: where (`local` or `agent:<machine>`), which harness, in which folder.
    New { target: String, kind: String, cwd: Option<String> },
}

/// How a plan runs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// The whole plan, as one message.
    #[default]
    Message,
    /// Each step its own message, each sent once the turn before it ends.
    Steps,
    /// Each step a goal in the agent's goal stack (`crate::goals`): the next goes once the one
    /// before is met (its harness's own /goal where it has one).
    Goals,
    /// Each step an agent of its own, held to the step as a goal ([`agents`]): the next starts
    /// once the one before is met.
    Agents,
}

impl Mode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Message => "message",
            Self::Steps => "steps",
            Self::Goals => "goals",
            Self::Agents => "agents",
        }
    }
}

/// What a run did: the agent it went to, and the messages it sent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Ran {
    pub session: String,
    pub sent: usize,
    pub run: String,
    /// As goals, split into its acceptance criteria and tests: how many steps were.
    #[serde(skip_serializing_if = "is_zero")]
    pub split: usize,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_zero(n: &usize) -> bool {
    *n == 0
}

/// Which of a plan's steps a run takes, and how.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pick {
    /// Only these steps, by their number from 0 (none: all of them); each keeps its number.
    pub only: Option<Vec<usize>>,
    /// As goals, each step splits into its acceptance criteria and tests at once (the goal stack
    /// a step is), rather than when its agent splits it (`goals_split`).
    pub split: bool,
}

impl Pick {
    /// Of `n` steps, the ones it takes, in order.
    pub fn of(&self, n: usize) -> Vec<usize> {
        match &self.only {
            Some(o) => {
                let mut v: Vec<usize> = o.iter().copied().filter(|&i| i < n).collect();
                v.sort_unstable();
                v.dedup();
                v
            }
            None => (0..n).collect(),
        }
    }
}

/// A failure the caller can act on.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Bad(String),
    /// Refused as the agents' lineage refuses (`crate::harness::lineage`): its status and why.
    #[error("{1}")]
    Refused(http::StatusCode, String),
    #[error("no such plan")]
    Missing,
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

type R<T> = Result<T, Error>;

/// A plan's steps: the items of its top-level lists (`- x`, `* x`, `1. x`, `- [ ] x`), each with
/// the lines that continue it; none when it has no list.
pub fn steps(body: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut fence = false;
    for line in body.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            fence = !fence;
            continue;
        }
        if fence {
            continue;
        }
        let top = line.len() == trimmed.len();
        let item = top.then(|| list_item(trimmed)).flatten();
        match item {
            Some(text) if !text.is_empty() => out.push(text.to_owned()),
            Some(_) => {}
            None => {
                // A line indented under an item continues it; anything else ends the list.
                if !top
                    && !trimmed.is_empty()
                    && let Some(last) = out.last_mut()
                {
                    last.push(' ');
                    last.push_str(trimmed);
                }
            }
        }
    }
    out
}

/// The text of a list item's line, without its marker and checkbox.
fn list_item(line: &str) -> Option<&str> {
    let rest = if let Some(r) = line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")).or_else(|| line.strip_prefix("+ ")) {
        r
    } else {
        let digits = line.bytes().take_while(u8::is_ascii_digit).count();
        let after = line.get(digits..)?;
        if digits == 0 || digits > 3 {
            return None;
        }
        after.strip_prefix(". ").or_else(|| after.strip_prefix(") "))?
    };
    let rest = rest.strip_prefix("[ ] ").or_else(|| rest.strip_prefix("[x] ")).or_else(|| rest.strip_prefix("[X] ")).unwrap_or(rest);
    Some(rest.trim())
}

/// A title for a plan that has none: its first heading, else its first line, shortened.
pub fn title_of(body: &str) -> String {
    let line = body.lines().map(str::trim).find(|l| l.starts_with('#')).or_else(|| body.lines().map(str::trim).find(|l| !l.is_empty())).unwrap_or("Plan");
    let line = line.trim_start_matches('#').trim();
    let line = list_item(line).unwrap_or(line);
    let mut t: String = line.chars().take(80).collect();
    if line.chars().count() > 80 {
        t.push('…');
    }
    if t.is_empty() { "Plan".to_owned() } else { t }
}

/// The whole plan as markdown: its text, then its items, each under its own heading.
pub fn markdown(p: &Plan) -> String {
    let mut out = p.body.trim().to_owned();
    for (i, it) in p.items.iter().enumerate() {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(&format!("## {}. {}", i + 1, it.title.trim()));
        for (name, v) in [("", &it.description), ("Examples", &it.examples)] {
            let v = v.trim();
            if !v.is_empty() {
                out.push_str(&if name.is_empty() { format!("\n\n{v}") } else { format!("\n\n**{name}**\n\n{v}") });
            }
        }
        if !it.acceptance.is_empty() {
            out.push_str("\n\n**Acceptance criteria**\n");
            for c in &it.acceptance {
                out.push_str(&format!("\n- {}", c.sentence()));
            }
        }
        if !it.testing.is_empty() {
            out.push_str("\n\n**Tests**\n");
            for t in &it.testing {
                out.push_str(&format!("\n- {}", t.sentence()));
            }
        }
    }
    out
}

/// The message that hands an agent the whole plan, under its title (unless it opens with it); for a
/// plan whose steps say what they wait for, the order they go in, those side by side on subagents
/// of the agent's own, at most `at_once` at a time (its Subagents setting).
pub fn message(p: &Plan, at_once: Option<u32>) -> String {
    let all = markdown(p);
    let body = all.trim();
    let first = body.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    let order = order_words(p, at_once).map(|o| format!(" {o}")).unwrap_or_default();
    if first.starts_with('#') && title_of(first) == p.title {
        format!("Carry out this plan, step by step, and say when it is done.{order}\n\n{body}")
    } else {
        format!("Carry out this plan, step by step, and say when it is done.{order}\n\n# {}\n\n{body}", p.title)
    }
}

/// The message that hands an agent one step of a plan.
pub fn step_message(p: &Plan, i: usize, n: usize, step: &str) -> String {
    format!("Step {} of {} of the plan \"{}\": {}\n\nDo this step only, then say what you did.", i + 1, n, p.title, step)
}

/// The message that hands an agent steps of a plan that don't wait for each other (`at`: each one's
/// index from 0, with its text, of `n`), to run side by side on subagents of its own, at most
/// `at_once` at a time (its Subagents setting): one message, so the steps after them wait for it.
pub fn side_by_side_message(p: &Plan, at: &[(usize, &str)], n: usize, at_once: Option<u32>) -> String {
    let nums: Vec<usize> = at.iter().map(|(i, _)| *i).collect();
    let steps = at.iter().map(|(i, s)| format!("Step {}: {s}", i + 1)).collect::<Vec<_>>().join("\n\n");
    format!(
        "Steps {} of {n} of the plan \"{}\" don't wait for each other: run them side by side, a subagent of yours for each ({SUBAGENT_TOOLS}){}. Brief each with its step, and keep each to its step's own files.\n\n{steps}\n\nDo these steps only: wait for every subagent, check what each did, then say what each did.",
        numbers(&nums),
        p.title,
        at_most(at_once)
    )
}

/// How an agent starts subagents of its own, whatever its harness.
const SUBAGENT_TOOLS: &str = "your Agent or Task tool; without one, start agents with agents_start";

/// Steps (indexes from 0) as their numbers are said: "2", "2 and 3", "2, 3 and 4".
fn numbers(i: &[usize]) -> String {
    let n: Vec<String> = i.iter().map(|i| (i + 1).to_string()).collect();
    match n.as_slice() {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// How many subagents at once, as a message says it (none: as many as the agent likes).
fn at_most(at_once: Option<u32>) -> String {
    match at_once {
        Some(1) => ", one at a time (your Subagents setting)".to_owned(),
        Some(n) => format!(", at most {n} at once (your Subagents setting)"),
        None => String::new(),
    }
}

/// The order a plan's steps go in, for the agent that carries out the whole plan: its steps in
/// rounds as they say they wait for each other ([`rounds`]), each round's steps side by side on
/// subagents of its own (at most `at_once` at a time). None when its steps say nothing of waiting,
/// or none of them run side by side.
fn order_words(p: &Plan, at_once: Option<u32>) -> Option<String> {
    let n = p.items.len().min(STEPS);
    let rounds = picked_rounds(&p.items, n, &(0..n).collect::<Vec<_>>());
    if rounds.iter().all(|r| r.len() < 2) {
        return None;
    }
    let order = rounds.iter().map(|r| if r.len() > 1 { format!("{} side by side", numbers(r)) } else { numbers(r) }).collect::<Vec<_>>().join(", then ");
    Some(format!(
        "Its steps say what they wait for, so they go in this order: {order}. Run the steps side by side on subagents of yours, one a step ({SUBAGENT_TOOLS}){}, and check what each did before the steps that wait for them.",
        at_most(at_once)
    ))
}

/// The steps `picked` (indexes from 0, of `n`) in the rounds they run in, as the plan's steps say
/// what they wait for (`needs`; a step outside `picked` taken as done): each round's steps wait only
/// for steps of the rounds before, so they run side by side. A plan whose steps say nothing of
/// waiting has each step a round of its own, in order.
pub fn picked_rounds(items: &[Item], n: usize, picked: &[usize]) -> Vec<Vec<usize>> {
    if !items.iter().take(n).any(|it| !it.needs.is_empty()) {
        return picked.iter().map(|&i| vec![i]).collect();
    }
    let needs: Vec<Vec<usize>> = waits(items, n, false).into_iter().enumerate().map(|(i, d)| if picked.contains(&i) { d.into_iter().filter(|j| picked.contains(j)).collect() } else { Vec::new() }).collect();
    rounds(&needs).into_iter().map(|r| r.into_iter().filter(|i| picked.contains(i)).collect::<Vec<_>>()).filter(|r| !r.is_empty()).collect()
}

fn clean_tags(tags: Vec<String>) -> R<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for t in tags {
        let t = t.trim().to_owned();
        if t.is_empty() || out.contains(&t) {
            continue;
        }
        if t.chars().count() > TAG {
            return Err(Error::Bad(format!("a tag is {TAG} characters at the most")));
        }
        out.push(t);
    }
    if out.len() > TAGS {
        return Err(Error::Bad(format!("a plan has {TAGS} tags at the most")));
    }
    Ok(out)
}

fn check(title: &str, body: &str, items: &[Item]) -> R<()> {
    if title.trim().is_empty() {
        return Err(Error::Bad("a plan needs a title".into()));
    }
    if title.chars().count() > TITLE {
        return Err(Error::Bad(format!("a title is {TITLE} characters at the most")));
    }
    if body.trim().is_empty() && items.is_empty() {
        return Err(Error::Bad("a plan needs something in it: text, or a step".into()));
    }
    if body.len() > BODY {
        return Err(Error::Bad(format!("a plan is {} KiB at the most", BODY >> 10)));
    }
    if items.len() > ITEMS {
        return Err(Error::Bad(format!("a plan has {ITEMS} steps at the most")));
    }
    if let Some(p) = items.iter().enumerate().find_map(|(i, it)| it.problem(i + 1)) {
        return Err(Error::Bad(p));
    }
    if let Some(p) = needs_problem(items) {
        return Err(Error::Bad(p));
    }
    Ok(())
}

/// What's wrong with what the steps wait for, if anything: a step that isn't there, a step that
/// waits for itself, or steps that wait for each other round in a loop.
fn needs_problem(items: &[Item]) -> Option<String> {
    let n = items.len();
    for (i, it) in items.iter().enumerate() {
        if let Some(d) = it.needs.iter().find(|&&d| d == 0 || d > n) {
            return Some(format!("step {} waits for step {d}, which the plan doesn't have (steps count from 1)", i + 1));
        }
        if it.needs.contains(&(i + 1)) {
            return Some(format!("step {} waits for itself", i + 1));
        }
    }
    let needs = waits(items, n, false);
    let placed: usize = rounds(&needs).iter().map(Vec::len).sum();
    if placed < n {
        let ran: Vec<usize> = rounds(&needs).concat();
        let stuck: Vec<String> = (0..n).filter(|i| !ran.contains(i)).map(|i| (i + 1).to_string()).collect();
        return Some(format!("steps {} wait for each other round in a loop, so none of them could start", stuck.join(", ")));
    }
    None
}

/// What a plan's title is when none is given: its text's heading or first line, else its first
/// item's title.
fn title_for(body: &str, items: &[Item]) -> String {
    if body.trim().is_empty() {
        items.first().map(|i| i.title.trim().chars().take(80).collect()).filter(|t: &String| !t.is_empty()).unwrap_or_else(|| "Plan".to_owned())
    } else {
        title_of(body)
    }
}

const COLUMNS: &str = "p.id, p.title, p.body, p.tags, p.source, p.share, p.version, p.created_at, p.updated_at, (SELECT COUNT(*) FROM plan_runs r WHERE r.plan_id = p.id), p.items, p.phase, (SELECT x.id FROM research x WHERE x.plan_id = p.id ORDER BY x.created_at DESC LIMIT 1)";

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Plan> {
    let tags: String = r.get(3)?;
    let source: Option<String> = r.get(4)?;
    let body: String = r.get(2)?;
    let items: Vec<Item> = serde_json::from_str(&r.get::<_, String>(10)?).unwrap_or_default();
    Ok(Plan {
        id: r.get(0)?,
        title: r.get(1)?,
        steps: if items.is_empty() { steps(&body) } else { items.iter().map(Item::text).collect() },
        items,
        body,
        tags: serde_json::from_str(&tags).unwrap_or_default(),
        source: source.and_then(|s| serde_json::from_str(&s).ok()),
        share: r.get(5)?,
        version: r.get(6)?,
        created_at: r.get(7)?,
        updated_at: r.get(8)?,
        runs: r.get(9)?,
        phase: r.get(11)?,
        research: r.get(12)?,
    })
}

fn get_in(c: &rusqlite::Connection, id: &str) -> rusqlite::Result<Option<Plan>> {
    c.query_row(&format!("SELECT {COLUMNS} FROM plans p WHERE p.id = ?1"), params![id], row).optional()
}

/// Plans, newest change first: those whose title, body or tags hold `q` (any case), with `tag`.
pub async fn list(hub: &Hub, q: Option<String>, tag: Option<String>) -> R<Vec<Plan>> {
    let all = hub
        .store
        .call(move |c| {
            let mut st = c.prepare(&format!("SELECT {COLUMNS} FROM plans p ORDER BY p.updated_at DESC, p.id"))?;
            let rows = st.query_map([], row)?.collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await?;
    let q = q.map(|q| q.to_lowercase()).filter(|q| !q.trim().is_empty());
    Ok(all
        .into_iter()
        .filter(|p| tag.as_ref().is_none_or(|t| p.tags.contains(t)))
        .filter(|p| q.as_ref().is_none_or(|q| p.title.to_lowercase().contains(q) || p.body.to_lowercase().contains(q) || p.tags.iter().any(|t| t.to_lowercase().contains(q)) || p.steps.iter().any(|s| s.to_lowercase().contains(q))))
        .collect())
}

pub async fn get(hub: &Hub, id: &str) -> R<Plan> {
    let id = id.to_owned();
    hub.store.call(move |c| Ok(get_in(c, &id)?)).await?.ok_or(Error::Missing)
}

/// A shared plan, by its link's token.
pub async fn shared(hub: &Hub, token: &str) -> R<Plan> {
    let token = token.to_owned();
    hub.store
        .call(move |c| Ok(c.query_row(&format!("SELECT {COLUMNS} FROM plans p WHERE p.share = ?1"), params![token], row).optional()?))
        .await?
        .ok_or(Error::Missing)
}

pub async fn create(hub: &Hub, n: New, by: &str) -> R<Plan> {
    create_in(hub, n, by, "ready").await
}

/// A new plan in `phase` (`planning`: made from a goal, `crate::goals::plan`).
pub async fn create_in(hub: &Hub, n: New, by: &str, phase: &'static str) -> R<Plan> {
    let title = if n.title.trim().is_empty() { title_for(&n.body, &n.items) } else { n.title.trim().to_owned() };
    check(&title, &n.body, &n.items)?;
    let items = serde_json::to_string(&n.items).map_err(anyhow::Error::from)?;
    let tags = serde_json::to_string(&clean_tags(n.tags)?).map_err(anyhow::Error::from)?;
    let source = n.source.map(|s| serde_json::to_string(&fill_source(hub, s))).transpose().map_err(anyhow::Error::from)?;
    let (id, now, by) = (crate::util::new_id(), crate::util::now_unix(), by.to_owned());
    let body = n.body;
    let plan = hub
        .store
        .call(move |c| {
            let tx = crate::store::tx(c)?;
            tx.execute(
                "INSERT INTO plans (id, title, body, items, tags, source, version, created_at, updated_at, phase) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, ?7, ?7, ?8)",
                params![id, title, body, items, tags, source, now, phase],
            )?;
            tx.execute("INSERT INTO plan_versions (plan_id, version, title, body, items, at, by) VALUES (?1, 1, ?2, ?3, ?4, ?5, ?6)", params![id, title, body, items, now, by])?;
            let p = get_in(&tx, &id)?;
            tx.commit()?;
            Ok(p)
        })
        .await?;
    plan.ok_or(Error::Missing)
}

/// Where a plan came from, completed from its agent when only the agent is named.
fn fill_source(hub: &Hub, s: Source) -> Source {
    let Some(i) = s.session.as_deref().and_then(|id| hub.harness.list().into_iter().find(|i| i.id == id)) else {
        return s;
    };
    Source {
        harness: s.harness.or(Some(i.kind)),
        machine: s.machine.or_else(|| (i.target != "local").then(|| i.target.trim_start_matches("agent:").to_owned())),
        cwd: s.cwd.or(Some(i.cwd)),
        agent: s.agent.or(Some(i.name)),
        session: s.session,
        goal: s.goal,
        proposed: s.proposed,
    }
}

/// Of `plans`, an agent's (`session`: made in its conversation) and the ones agents proposed
/// (`proposed`), when asked.
pub fn of_source(plans: Vec<Plan>, session: Option<&str>, proposed: bool) -> Vec<Plan> {
    plans
        .into_iter()
        .filter(|p| session.is_none_or(|x| p.source.as_ref().and_then(|s| s.session.as_deref()) == Some(x)))
        .filter(|p| !proposed || p.source.as_ref().is_some_and(|s| s.proposed))
        .collect()
}

/// The plan agent `agent` proposed (plan mode), kept: one plan an agent, a new version each time it
/// proposes another (its title from its words again); the same words again change nothing.
pub async fn keep_proposed(hub: &Hub, agent: &str, words: &str) -> R<Plan> {
    let words = words.trim();
    let mine = list(hub, None, None).await?.into_iter().find(|p| p.source.as_ref().is_some_and(|s| s.proposed && s.session.as_deref() == Some(agent)));
    match mine {
        Some(p) if p.body.trim() == words => Ok(p),
        Some(p) => update(hub, &p.id, Patch { title: Some(String::new()), body: Some(words.to_owned()), ..Patch::default() }, "kiln").await,
        None => {
            let source = Source { session: Some(agent.to_owned()), proposed: true, ..Source::default() };
            create(hub, New { body: words.to_owned(), source: Some(source), ..New::default() }, "kiln").await
        }
    }
}

/// Changes a plan; a new title or body is a new version.
pub async fn update(hub: &Hub, id: &str, patch: Patch, by: &str) -> R<Plan> {
    let old = get(hub, id).await?;
    let body = patch.body.unwrap_or_else(|| old.body.clone());
    let items = patch.items.unwrap_or_else(|| old.items.clone());
    // A title emptied is taken from the plan, as for a new one.
    let title = match patch.title.map(|t| t.trim().to_owned()) {
        Some(t) if t.is_empty() => title_for(&body, &items),
        Some(t) => t,
        None => old.title.clone(),
    };
    check(&title, &body, &items)?;
    let tags = match patch.tags {
        Some(t) => clean_tags(t)?,
        None => old.tags.clone(),
    };
    let new_version = title != old.title || body != old.body || items != old.items;
    let tags = serde_json::to_string(&tags).map_err(anyhow::Error::from)?;
    let items = serde_json::to_string(&items).map_err(anyhow::Error::from)?;
    let (id, now, by) = (id.to_owned(), crate::util::now_unix(), by.to_owned());
    let plan = hub
        .store
        .call(move |c| {
            let tx = crate::store::tx(c)?;
            let version = if new_version { old.version.saturating_add(1) } else { old.version };
            tx.execute("UPDATE plans SET title = ?2, body = ?3, items = ?4, tags = ?5, version = ?6, updated_at = ?7 WHERE id = ?1", params![id, title, body, items, tags, version, now])?;
            if new_version {
                tx.execute("INSERT INTO plan_versions (plan_id, version, title, body, items, at, by) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)", params![id, version, title, body, items, now, by])?;
            }
            let p = get_in(&tx, &id)?;
            tx.commit()?;
            Ok(p)
        })
        .await?;
    plan.ok_or(Error::Missing)
}

/// A version of the plan made current again (as a new version, so nothing is lost).
pub async fn restore(hub: &Hub, id: &str, version: i64, by: &str) -> R<Plan> {
    let old = versions(hub, id).await?.into_iter().find(|v| v.version == version).ok_or_else(|| Error::Bad(format!("the plan has no version {version}")))?;
    update(hub, id, Patch { title: Some(old.title), body: Some(old.body), items: Some(old.items), tags: None }, by).await
}

pub async fn delete(hub: &Hub, id: &str) -> R<()> {
    let id = id.to_owned();
    let gone = hub
        .store
        .call(move |c| {
            let tx = crate::store::tx(c)?;
            let n = tx.execute("DELETE FROM plans WHERE id = ?1", params![id])?;
            tx.execute("DELETE FROM plan_versions WHERE plan_id = ?1", params![id])?;
            tx.execute("DELETE FROM plan_runs WHERE plan_id = ?1", params![id])?;
            tx.commit()?;
            Ok(n > 0)
        })
        .await?;
    if gone { Ok(()) } else { Err(Error::Missing) }
}

/// Every version, newest first.
pub async fn versions(hub: &Hub, id: &str) -> R<Vec<Version>> {
    get(hub, id).await?;
    let id = id.to_owned();
    Ok(hub
        .store
        .call(move |c| {
            let mut st = c.prepare("SELECT version, title, body, at, by, items FROM plan_versions WHERE plan_id = ?1 ORDER BY version DESC")?;
            let rows = st
                .query_map(params![id], |r| {
                    let items: String = r.get(5)?;
                    Ok(Version { version: r.get(0)?, title: r.get(1)?, body: r.get(2)?, items: serde_json::from_str(&items).unwrap_or_default(), at: r.get(3)?, by: r.get(4)? })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await?)
}

/// Every run, newest first.
pub async fn runs(hub: &Hub, id: &str) -> R<Vec<Run>> {
    get(hub, id).await?;
    let id = id.to_owned();
    Ok(hub
        .store
        .call(move |c| {
            let mut st = c.prepare("SELECT id, version, session, mode, at, by, state FROM plan_runs WHERE plan_id = ?1 ORDER BY at DESC LIMIT 100")?;
            let rows = st.query_map(params![id], |r| Ok(Run { id: r.get(0)?, version: r.get(1)?, session: r.get(2)?, mode: r.get(3)?, at: r.get(4)?, by: r.get(5)?, state: r.get(6)? }))?.collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await?)
}

/// Shares a plan by a link (a new token each time it is shared), or stops sharing it.
pub async fn share(hub: &Hub, id: &str, on: bool) -> R<Plan> {
    get(hub, id).await?;
    let (id, token) = (id.to_owned(), on.then(crate::util::random_token));
    let plan = hub
        .store
        .call(move |c| {
            c.execute("UPDATE plans SET share = ?2 WHERE id = ?1", params![id, token])?;
            Ok(get_in(c, &id)?)
        })
        .await?;
    plan.ok_or(Error::Missing)
}

/// Starts the agent a plan runs on, as `by` starts agents (`crate::harness::lineage::start`): an
/// agent's is its child, held to what it may have and within its limits.
async fn start_for(hub: &Hub, by: &crate::harness::lineage::Starter, target: String, kind: String, cwd: Option<String>, prompt: Option<String>, name: String) -> R<String> {
    let ask = crate::harness::lineage::Ask { kind, on: Some(target), cwd: cwd.filter(|c| !c.trim().is_empty()), prompt, name: Some(name), count: Some(1), ..crate::harness::lineage::Ask::default() };
    let infos = crate::harness::lineage::start(hub, by, ask).await.map_err(|(c, m)| Error::Refused(c, m))?;
    infos.into_iter().next().map(|i| i.id).ok_or_else(|| Error::Bad("the agent did not start".into()))
}

/// Runs a plan on an agent: an existing one, or a new one started with it (as `by` starts agents).
///
/// With `key` (an idempotency key, CP-T3.2) the run's id is made from it, the plan and who runs it,
/// so running it again under the key is the same run: on an agent that runs, it's done again (its
/// messages are the same ones, which the agent takes once) and answered as it was; one that starts
/// an agent, or adds goals, is answered as it was once it's done, and refused while it's under way
/// (or stopped part-way), saying so: it's never done twice.
pub async fn run(hub: &Hub, id: &str, target: Target, mode: Mode, pick: &Pick, by: &crate::harness::lineage::Starter, key: Option<&str>) -> R<Ran> {
    let p = get(hub, id).await?;
    ready_to_run(&p)?;
    let run = key.map_or_else(crate::util::new_id, |k| crate::util::derive_id(&["plans.run", &p.id, k]));
    // Done again only where doing it again is safe: an agent that runs, sent the same messages.
    let again_safe = matches!(target, Target::Session(_)) && mode != Mode::Goals;
    if key.is_some()
        && let Some(done) = claim(hub, &p, &run, mode, pick, by, again_safe).await?
    {
        return Ok(done);
    }
    // A run that failed before it did anything (started no agent, sent nothing) lets go of its
    // claim: run again under its key, it's tried again.
    let mut acted = false;
    let ran = run_claimed(hub, &p, target, mode, pick, by, run.clone(), &mut acted).await;
    if ran.is_err() && key.is_some() && !acted {
        let run_c = run.clone();
        let released = hub.store.call(move |c| Ok(c.execute("DELETE FROM plan_runs WHERE id = ?1 AND session = ''", params![run_c])?)).await;
        if let Err(e) = released {
            tracing::warn!(%run, error = %format!("{e:#}"), "a plan run that failed couldn't let go of its key: run again under it, it's refused");
        }
    }
    ran
}

/// Plan `p`'s run `run`, claimed under its key if it has one (`run`): `acted` once it started an
/// agent or sent anything.
#[allow(clippy::too_many_arguments)]
async fn run_claimed(hub: &Hub, p: &Plan, target: Target, mode: Mode, pick: &Pick, by: &crate::harness::lineage::Starter, run: String, acted: &mut bool) -> R<Ran> {
    if mode == Mode::Goals {
        return run_goals(hub, p, target, pick, by, run, acted).await;
    }
    if mode == Mode::Agents {
        return Err(Error::Bad("one agent per step starts agents of its own: run it with `new` (agents::start)".into()));
    }
    if pick.only.is_some() && mode == Mode::Message {
        return Err(Error::Bad("`only` picks steps: run them as steps, goals or agents".into()));
    }
    // The most subagents the agent runs at once (its Subagents setting): its steps side by side go to
    // subagents of its own, within it (docs/agentic.md P2). A new agent has no setting of its own.
    let at_once = match &target {
        Target::Session(s) => hub.harness.info(s).and_then(|i| i.max_subagents),
        Target::New { .. } => None,
    };
    let messages: Vec<String> = match mode {
        Mode::Message | Mode::Goals | Mode::Agents => vec![message(p, at_once)],
        Mode::Steps => {
            let steps: Vec<&String> = p.steps.iter().take(STEPS).collect();
            if steps.is_empty() {
                return Err(Error::Bad("this plan has no steps (no list in it): run it as one message".into()));
            }
            let n = steps.len();
            let picked = pick.of(n);
            if picked.is_empty() {
                return Err(Error::Bad(format!("none of those steps is in this plan: it has {n}")));
            }
            // A message a round: steps that don't wait for each other in one, side by side.
            picked_rounds(&p.items, n, &picked)
                .iter()
                .filter_map(|r| match r.as_slice() {
                    [one] => Some(step_message(p, *one, n, steps.get(*one)?)),
                    many => {
                        let at: Vec<(usize, &str)> = many.iter().filter_map(|&i| Some((i, steps.get(i)?.as_str()))).collect();
                        Some(side_by_side_message(p, &at, n, at_once))
                    }
                })
                .collect()
        }
    };
    // The run's id is made first, and each of its messages' ids from it and its place among them:
    // whatever sends one again sends the same message, which its agent gets once (CP-T3.2).
    let (session, rest) = match target {
        Target::Session(session) => (session, messages.as_slice()),
        Target::New { target, kind, cwd } => {
            let (first, rest) = messages.split_first().ok_or_else(|| Error::Bad("nothing to send".into()))?;
            let started = start_for(hub, by, target, kind, cwd, Some(first.clone()), p.title.clone()).await?;
            *acted = true;
            (started, rest)
        }
    };
    let first = messages.len().saturating_sub(rest.len());
    let mut sent = first;
    for (n, text) in rest.iter().enumerate() {
        let step = n.saturating_add(first).to_string();
        let c = Command::Send { id: crate::util::derive_id(&["plan-run", &run, &step]), text: text.clone(), mode: SendMode::Queue, attachments: Vec::new(), overrides: None, reply_to: None, task: None };
        match hub.harness.command(hub, &session, c).await {
            Sent::Ok | Sent::Accepted(_) | Sent::Taken | Sent::Parked => {
                sent += 1;
                *acted = true;
            }
            Sent::Refused(r) => return Err(Error::Bad(r.to_string())),
            s @ (Sent::Rejected { .. } | Sent::Unanswered(_)) => return Err(Error::Bad(s.failure().map_or_else(|| "its runner refused it".to_owned(), |(_, why)| why))),
            Sent::NoSession => return Err(Error::Bad("no such agent".into())),
            Sent::Unreachable => return Err(Error::Bad("that agent's machine is not connected".into())),
        }
    }
    let (now, by_s, version, mode_s, plan_id, session_c) = (crate::util::now_unix(), by.actor.clone(), p.version, mode.as_str(), p.id.clone(), session.clone());
    let run_c = run.clone();
    hub.store
        .call(move |c| {
            // A run claimed under its key (`claim`) is done now: it says where it ran.
            c.execute(
                "INSERT INTO plan_runs (id, plan_id, version, session, mode, at, by) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) ON CONFLICT (id) DO UPDATE SET session = excluded.session, at = excluded.at",
                params![run_c, plan_id, version, session_c, mode_s, now, by_s],
            )?;
            Ok(())
        })
        .await?;
    Ok(Ran { session, sent, run, split: 0 })
}

/// Notes plan `p` run on agent `session` in `mode` (`goals`: its steps on that agent's stack),
/// by `by`: the run's id.
pub async fn note_run(hub: &Hub, p: &Plan, session: &str, mode: &'static str, by: &str) -> R<String> {
    let (run, now, version, plan_id, session, by) = (crate::util::new_id(), crate::util::now_unix(), p.version, p.id.clone(), session.to_owned(), by.to_owned());
    let run_c = run.clone();
    hub.store
        .call(move |c| {
            c.execute("INSERT INTO plan_runs (id, plan_id, version, session, mode, at, by) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)", params![run_c, plan_id, version, session, mode, now, by])?;
            Ok(())
        })
        .await?;
    Ok(run)
}

/// A plan in its planning phase isn't run: its agent says it's ready first (`plans.ready`).
pub fn ready_to_run(p: &Plan) -> R<()> {
    if p.phase == "planning" {
        return Err(Error::Bad(format!("\"{}\" is still being planned: its agent researches it and writes its steps, then says it's ready (plans_ready)", p.title)));
    }
    Ok(())
}

/// What saying a plan is ready came to: what its steps rest on that's weak, said so.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Ready {
    pub warnings: Vec<String>,
}

fn research_error(e: crate::research::Error) -> Error {
    match e {
        crate::research::Error::Bad(m) => Error::Bad(m),
        crate::research::Error::Missing => Error::Bad("its research is gone".into()),
        crate::research::Error::Other(e) => Error::Other(e),
    }
}

/// Ends plan `id`'s planning phase: it has steps, its research has sources inside and out (or says
/// why not: `skipped`), and each step rests only on findings of its research. What's weak (a step
/// resting on nothing, or on findings graded D or F; quotes still being checked) comes back as
/// warnings, for the agent to say. A plan ready already stays as it is.
pub async fn ready(hub: &Hub, id: &str, skipped: Option<crate::research::Skipped>) -> R<(Plan, Ready)> {
    let p = get(hub, id).await?;
    if p.phase != "planning" {
        return Ok((p, Ready::default()));
    }
    if p.steps.is_empty() {
        return Err(Error::Bad("it has no steps yet: write them (plans_update with `items`, each with its acceptance criteria, tests and the findings it rests on), then say it's ready".into()));
    }
    let mut warnings = Vec::new();
    if let Some(rid) = p.research.clone() {
        if let Some(k) = skipped {
            crate::research::skip(hub, &rid, k).await.map_err(research_error)?;
        }
        let r = crate::research::get(hub, &rid).await.map_err(research_error)?;
        let v = crate::research::view(&r, false);
        if v.summary.internal == 0 && r.skipped.internal.is_none() {
            return Err(Error::Bad(
                "its research has no internal sources: look inside first (files on your machine and others: code, docs, data, pictures, video; kiln's records: conversations_search, plans, CI runs, deployments) and add what you find (research_add), or say why there are none (`skipped.internal`)".into(),
            ));
        }
        if v.summary.external == 0 && r.skipped.external.is_none() {
            return Err(Error::Bad(
                "its research has no external sources: look outside too (the web, official docs and standards, papers, blogs, forums) and add what you find (research_add), or say why there are none (`skipped.external`)".into(),
            ));
        }
        let ids: Vec<&str> = r.findings.iter().map(|f| f.id.as_str()).collect();
        for (i, it) in p.items.iter().enumerate() {
            if let Some(b) = it.basis.iter().find(|b| !ids.contains(&b.as_str())) {
                return Err(Error::Bad(format!("step {} rests on {b}, which isn't a finding of its research", i + 1)));
            }
        }
        for (i, it) in p.items.iter().enumerate() {
            if it.basis.is_empty() {
                warnings.push(format!("step {} rests on no finding of its research", i + 1));
                continue;
            }
            let weak: Vec<String> = it
                .basis
                .iter()
                .filter_map(|b| v.findings.iter().find(|g| &g.finding.id == b))
                .filter(|g| matches!(g.confidence.grade, "D" | "F"))
                .map(|g| format!("{} ({} · {})", g.finding.id, g.confidence.grade, g.confidence.score))
                .collect();
            if !weak.is_empty() {
                warnings.push(format!("step {} rests on weak findings: {}", i + 1, weak.join(", ")));
            }
        }
        if v.summary.checking > 0 {
            warnings.push(format!("{} quotes are still being checked: grades may change", v.summary.checking));
        }
    }
    let (pid, now) = (p.id.clone(), crate::util::now_unix());
    hub.store
        .call(move |c| {
            c.execute("UPDATE plans SET phase = 'ready', updated_at = ?2 WHERE id = ?1", params![pid, now])?;
            Ok(())
        })
        .await?;
    Ok((get(hub, id).await?, Ready { warnings }))
}

/// A goal stack's failure as a plan's.
fn goals_error(e: crate::goals::Error) -> Error {
    match e {
        crate::goals::Error::NoAgent => Error::Bad("no such agent".into()),
        crate::goals::Error::Bad(m) => Error::Bad(m),
        crate::goals::Error::Other(e) => Error::Other(e),
    }
}

/// A plan's steps as goals, after the agent's others (a new agent is started for them), and the
/// stack running.
async fn run_goals(hub: &Hub, p: &Plan, target: Target, pick: &Pick, by: &crate::harness::lineage::Starter, run: String, acted: &mut bool) -> R<Ran> {
    let all: Vec<String> = if p.items.is_empty() { p.steps.iter().take(STEPS).cloned().collect() } else { p.items.iter().take(STEPS).map(Item::goal).collect() };
    if all.is_empty() {
        return Err(Error::Bad("this plan has no steps (no list in it) to make goals of".into()));
    }
    let picked = pick.of(all.len());
    if picked.is_empty() {
        return Err(Error::Bad(format!("none of those steps is in this plan: it has {}", all.len())));
    }
    // Each keeps its number in the plan.
    let texts: Vec<(usize, String)> = picked.iter().filter_map(|&i| Some((i, all.get(i)?.clone()))).collect();
    let session = match target {
        Target::Session(s) => s,
        Target::New { target, kind, cwd } => {
            let started = start_for(hub, by, target, kind, cwd, None, p.title.clone()).await?;
            *acted = true;
            started
        }
    };
    let sent = texts.len();
    // Each goal says the step it's made from: the agent splits one into its acceptance criteria and
    // tests (`crate::goals::split`).
    let goals = texts
        .into_iter()
        .map(|(i, text)| crate::goals::NewGoal {
            text,
            step: Some(crate::goals::StepRef { plan: p.id.clone(), version: p.version, n: i + 1, run: Some(run.clone()) }),
            ..crate::goals::NewGoal::default()
        })
        .collect();
    let stack = crate::goals::add_at(hub, &session, goals, true, &crate::goals::At::End).await.map_err(goals_error)?;
    *acted = true;
    // Each step its goal stack: its acceptance criteria and tests, a goal each, before it (one
    // without either stays as it is).
    let mut split = 0;
    if pick.split {
        let made: Vec<String> = stack.items.iter().filter(|i| i.step.as_ref().is_some_and(|s| s.run.as_deref() == Some(run.as_str())) && i.part_of.is_none()).map(|i| i.id.clone()).collect();
        for id in made {
            if crate::goals::split(hub, &session, Some(&id), Vec::new()).await.is_ok() {
                split += 1;
            }
        }
    }
    let (now, by_s, version, plan_id, session_c) = (crate::util::now_unix(), by.actor.clone(), p.version, p.id.clone(), session.clone());
    let run_c = run.clone();
    hub.store
        .call(move |c| {
            c.execute(
                "INSERT INTO plan_runs (id, plan_id, version, session, mode, at, by) VALUES (?1, ?2, ?3, ?4, 'goals', ?5, ?6) ON CONFLICT (id) DO UPDATE SET session = excluded.session, at = excluded.at",
                params![run_c, plan_id, version, session_c, now, by_s],
            )?;
            Ok(())
        })
        .await?;
    Ok(Ran { session, sent, run, split })
}

/// A run under an idempotency key, before it's done: the run it was, if it was done (its answer,
/// again), else none, the run claimed now. One under way (or stopped part-way) is refused unless
/// doing it again is safe (`again_safe`: its messages to an agent that runs, the same ones again).
async fn claim(hub: &Hub, p: &Plan, run: &str, mode: Mode, pick: &Pick, by: &crate::harness::lineage::Starter, again_safe: bool) -> R<Option<Ran>> {
    // What was asked under the key: asked again for other steps, or in another mode, it's another
    // request, refused (never answered as the run it isn't).
    let shape = serde_json::json!({"only": pick.only, "split": pick.split}).to_string();
    let (run_c, plan_id, version, mode_s, by_s, now, shape_c) = (run.to_owned(), p.id.clone(), p.version, mode.as_str(), by.actor.clone(), crate::util::now_unix(), shape.clone());
    // Claimed with no session yet: it says where it ran once it's done.
    let found: Option<(String, String, String)> = hub
        .store
        .call(move |c| {
            let claimed = c.execute(
                "INSERT INTO plan_runs (id, plan_id, version, session, mode, at, by, config) VALUES (?1, ?2, ?3, '', ?4, ?5, ?6, ?7) ON CONFLICT (id) DO NOTHING",
                params![run_c, plan_id, version, mode_s, now, by_s, shape_c],
            )?;
            if claimed == 1 {
                return Ok(None);
            }
            Ok(c.query_row("SELECT session, mode, COALESCE(config, '') FROM plan_runs WHERE id = ?1", params![run_c], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?)
        })
        .await?;
    if let Some((_, was_mode, was_shape)) = &found
        && (was_mode != mode.as_str() || *was_shape != shape)
    {
        return Err(Error::Refused(http::StatusCode::CONFLICT, "this key was given to another request (other steps, or another mode): give this one a key of its own".into()));
    }
    let found = found.map(|(session, _, _)| session);
    match found {
        None => Ok(None),
        Some(session) if !session.is_empty() => {
            let sent = match mode {
                Mode::Steps => pick.of(p.steps.len().min(STEPS)).len(),
                Mode::Goals => pick.of(if p.items.is_empty() { p.steps.len().min(STEPS) } else { p.items.len().min(STEPS) }).len(),
                Mode::Message | Mode::Agents => 1,
            };
            let split = if mode == Mode::Goals && pick.split { split_of(hub, &session, run).await } else { 0 };
            Ok(Some(Ran { session, sent, run: run.to_owned(), split }))
        }
        Some(_) if again_safe => Ok(None),
        Some(_) => Err(Error::Refused(http::StatusCode::CONFLICT, "a run under this key is under way, or stopped part-way: see plans.runs.list for where it is".into())),
    }
}

/// How many of run `run`'s goals on agent `session`'s stack were split into their parts.
async fn split_of(hub: &Hub, session: &str, run: &str) -> usize {
    let Ok(stack) = crate::goals::get(hub, session).await else {
        return 0;
    };
    let ours = |i: &crate::goals::Item| i.part_of.is_none() && i.step.as_ref().is_some_and(|s| s.run.as_deref() == Some(run));
    stack.items.iter().filter(|i| ours(i) && stack.items.iter().any(|j| j.part_of.as_deref() == Some(i.id.as_str()))).count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plans_steps_are_its_top_level_list() {
        let body = "# Ship sync\n\nWhy: copies.\n\n1. Write the engine\n   with tests\n2. [x] Wire it\n- [ ] Screenshot it\n  - a sub-point stays in its step\n\n```\n- not a step\n```\nDone.";
        assert_eq!(steps(body), ["Write the engine with tests", "Wire it", "Screenshot it - a sub-point stays in its step"]);
        assert!(steps("Just do the thing.").is_empty());
    }

    #[test]
    fn a_title_comes_from_the_first_heading_or_line() {
        assert_eq!(title_of("Intro\n# The plan\n- a"), "The plan");
        assert_eq!(title_of("\n  first line here\n"), "first line here");
        assert_eq!(title_of(""), "Plan");
        assert_eq!(title_of("- [ ] read the test\n- fix it"), "read the test");
        assert!(title_of(&"x".repeat(200)).ends_with('…'));
    }

    #[test]
    fn a_step_given_wrong_says_which_it_is() {
        let given = serde_json::json!({"items": [{"title": "a"}, {"title": "b", "acceptance": [{"kind": "quantitative", "target": "x", "op": "equals", "value": 1}]}]});
        let e = serde_json::from_value::<Patch>(given).err().map(|e| e.to_string()).unwrap_or_default();
        assert!(e.starts_with("step 2: criterion 1: unknown variant `equals`"), "{e}");
        let p = serde_json::from_value::<Patch>(serde_json::json!({"title": "t"})).ok();
        assert!(p.is_some_and(|p| p.items.is_none()), "steps not given stay as they are");
        let n = serde_json::from_value::<New>(serde_json::json!({"title": "t", "items": [{"title": "a"}]})).ok();
        assert_eq!(n.map(|n| n.items.len()), Some(1));
    }

    #[test]
    fn tags_are_trimmed_and_unique() {
        assert_eq!(clean_tags(vec![" a ".into(), "a".into(), "".into(), "b".into()]).ok(), Some(vec!["a".to_owned(), "b".to_owned()]));
        assert!(clean_tags(vec!["x".repeat(41)]).is_err());
    }

    #[test]
    fn messages_hand_over_the_plan_or_one_step() {
        let p = Plan {
            id: "p".into(),
            title: "T".into(),
            body: "- a\n- b".into(),
            items: vec![],
            tags: vec![],
            source: None,
            share: None,
            version: 1,
            created_at: 0,
            updated_at: 0,
            steps: vec!["a".into(), "b".into()],
            runs: 0,
            phase: "ready".into(),
            research: None,
        };
        assert!(message(&p, None).contains("# T\n\n- a\n- b"));
        let headed = Plan { body: "# T\n\n- a\n- b".into(), ..p.clone() };
        assert_eq!(message(&headed, None).matches("# T").count(), 1);
        assert!(step_message(&p, 1, 2, "b").starts_with("Step 2 of 2 of the plan \"T\": b"));
    }

    #[test]
    fn items_read_as_markdown_as_steps_and_as_goals() {
        let le = Criterion::Quantitative { target: "scan time".into(), op: Op::Le, value: "2".into(), value2: String::new(), unit: "ms".into(), baseline: String::new(), measured: String::new() };
        let it = Item {
            title: "Split the scanner".into(),
            description: "Move number scanning\ninto its own fn.".into(),
            examples: "0x1F, 1e9".into(),
            acceptance: vec![Criterion::Qualitative { text: "scan_number exists".into() }, le],
            testing: vec![Test { scenario: "hex literals".into(), criteria: vec![Criterion::Qualitative { text: "0x1F reads as 31".into() }] }],
            needs: vec![],
            basis: vec![],
        };
        let p = Plan {
            id: "p".into(),
            title: "Lexer".into(),
            body: "Why: the scanner grew.".into(),
            items: vec![it.clone(), Item { title: "Ship it".into(), ..Item::default() }],
            tags: vec![],
            source: None,
            share: None,
            version: 1,
            created_at: 0,
            updated_at: 0,
            steps: vec![],
            runs: 0,
            phase: "ready".into(),
            research: None,
        };
        let md = markdown(&p);
        assert!(md.starts_with("Why: the scanner grew.\n\n## 1. Split the scanner\n\nMove number scanning"), "{md}");
        assert!(md.contains("**Acceptance criteria**\n\n- scan_number exists\n- scan time is at most 2 ms") && md.contains("**Tests**\n\n- hex literals — passes when 0x1F reads as 31") && md.contains("## 2. Ship it"), "{md}");
        assert!(message(&p, None).contains("# Lexer\n\nWhy: the scanner grew."));
        assert!(it.text().starts_with("Split the scanner\n\nMove number scanning") && it.text().contains("Acceptance criteria:\n- scan_number exists\n- scan time is at most 2 ms\n\nTests:\n- hex literals"), "{}", it.text());
        assert_eq!(
            it.goal(),
            "Split the scanner — Move number scanning into its own fn. — Examples: 0x1F, 1e9 — Done when: scan_number exists; scan time is at most 2 ms — Tests: hex literals — passes when 0x1F reads as 31"
        );
        assert!(Item { title: "x".repeat(50), description: "y ".repeat(2000), ..Item::default() }.goal().chars().count() <= 1991);
        assert!(check("T", "", &[Item { title: "a".into(), ..Item::default() }]).is_ok());
        assert!(check("T", "", &[]).is_err());
        assert!(check("T", "", &[Item::default()]).is_err());
        let bad = Item { title: "a".into(), acceptance: vec![Criterion::Quantitative { target: "x".into(), op: Op::Le, value: "soon".into(), value2: String::new(), unit: String::new(), baseline: String::new(), measured: String::new() }], ..Item::default() };
        assert!(matches!(check("T", "", &[bad]), Err(Error::Bad(m)) if m.starts_with("step 1: x's threshold is a number")));
        assert_eq!(title_for("", &[Item { title: "First thing".into(), ..Item::default() }]), "First thing");
        // Split into goals: each criterion, then each test, saying which step it's of.
        assert_eq!(
            part_goals("Lexer", 1, &it),
            [
                "\"Split the scanner\" (step 1 of the plan \"Lexer\"), acceptance criterion 1 of 2: scan_number exists",
                "\"Split the scanner\" (step 1 of the plan \"Lexer\"), acceptance criterion 2 of 2: scan time is at most 2 ms",
                "\"Split the scanner\" (step 1 of the plan \"Lexer\"), test 1 of 1: hex literals — passes when 0x1F reads as 31",
            ]
        );
        assert!(part_goals("Lexer", 2, &Item { title: "Ship it".into(), ..Item::default() }).is_empty());
    }

    /// What each step waits for: as the steps say; none saying, each the one before, or none at all
    /// side by side. The rounds a run goes in, and the plans that can't run: a step that isn't
    /// there, one that waits for itself, steps that wait for each other in a loop.
    #[test]
    fn steps_wait_for_the_steps_they_need_and_run_in_rounds() {
        let it = |needs: &[usize]| Item { title: "s".into(), needs: needs.to_vec(), ..Item::default() };
        let chain = [it(&[]), it(&[]), it(&[])];
        assert_eq!(waits(&chain, 3, false), [vec![], vec![0], vec![1]]);
        assert_eq!(waits(&chain, 3, true), [Vec::<usize>::new(), vec![], vec![]]);
        assert_eq!(rounds(&waits(&chain, 3, false)), [vec![0], vec![1], vec![2]]);
        // Said by the steps: those that say none wait for none, side by side or not.
        let graph = [it(&[]), it(&[1]), it(&[1]), it(&[]), it(&[2, 3])];
        assert_eq!(waits(&graph, 5, false), waits(&graph, 5, true));
        assert_eq!(rounds(&waits(&graph, 5, false)), [vec![0, 3], vec![1, 2], vec![4]]);
        // Body steps (no items): as the run says.
        assert_eq!(waits(&[], 2, false), [vec![], vec![0]]);
        assert!(check("T", "", &graph).is_ok());
        let bad = |items: &[Item]| match check("T", "", items) {
            Err(Error::Bad(m)) => m,
            _ => String::new(),
        };
        assert_eq!(bad(&[it(&[]), it(&[3])]), "step 2 waits for step 3, which the plan doesn't have (steps count from 1)");
        assert_eq!(bad(&[it(&[]), it(&[0])]), "step 2 waits for step 0, which the plan doesn't have (steps count from 1)");
        assert_eq!(bad(&[it(&[1])]), "step 1 waits for itself");
        assert_eq!(bad(&[it(&[]), it(&[3]), it(&[2])]), "steps 2, 3 wait for each other round in a loop, so none of them could start");
        // A step's needs don't change how it reads: only how it runs.
        let saved = serde_json::to_value(&chain[0]).unwrap_or_default();
        assert!(saved.get("needs").is_none(), "{saved}");
        let read: Item = serde_json::from_value(serde_json::json!({"title": "x", "needs": [1, 2]})).unwrap_or_default();
        assert_eq!(read.needs, [1, 2]);
    }

    /// docs/agentic.md P2: run on an agent as one message or a step a turn, a plan whose steps say
    /// what they wait for goes in rounds: the steps of a round, side by side, go to subagents of the
    /// agent's own, at most its Subagents setting at once (a step a turn: one message a round, so the
    /// steps after them wait for it). A plan whose steps say nothing of waiting goes as before, a
    /// step at a time; and steps picked (`only`) wait only for each other.
    #[test]
    fn p2_steps_side_by_side_go_to_the_agent_s_subagents_at_most_its_setting_at_once() {
        let it = |t: &str, needs: &[usize]| Item { title: t.into(), needs: needs.to_vec(), ..Item::default() };
        let items = vec![it("Schema", &[]), it("API", &[1]), it("UI", &[1]), it("Docs", &[]), it("Ship", &[2, 3])];
        assert_eq!(picked_rounds(&items, 5, &[0, 1, 2, 3, 4]), [vec![0, 3], vec![1, 2], vec![4]]);
        assert_eq!(picked_rounds(&items, 5, &[1, 2, 4]), [vec![1, 2], vec![4]], "step 1, not picked, is taken as done");
        let chain = vec![it("a", &[]), it("b", &[]), it("c", &[])];
        assert_eq!(picked_rounds(&chain, 3, &[0, 2]), [vec![0], vec![2]], "saying nothing of waiting: a step at a time, in order");
        let p = Plan {
            id: "p".into(),
            title: "Search".into(),
            body: String::new(),
            steps: items.iter().map(Item::text).collect(),
            items,
            tags: vec![],
            source: None,
            share: None,
            version: 1,
            created_at: 0,
            updated_at: 0,
            runs: 0,
            phase: "ready".into(),
            research: None,
        };
        let whole = message(&p, Some(2));
        assert!(whole.contains("they go in this order: 1 and 4 side by side, then 2 and 3 side by side, then 5.") && whole.contains("at most 2 at once (your Subagents setting)"), "{whole}");
        assert!(!message(&Plan { items: chain, ..p.clone() }, Some(2)).contains("side by side"), "nothing to say of a plan whose steps say nothing of waiting");
        let two = side_by_side_message(&p, &[(1, "API"), (2, "UI")], 5, Some(1));
        assert!(two.starts_with("Steps 2 and 3 of 5 of the plan \"Search\" don't wait for each other: run them side by side, a subagent of yours for each"), "{two}");
        assert!(two.contains("one at a time (your Subagents setting)") && two.contains("Step 2: API\n\nStep 3: UI") && two.ends_with("then say what each did."), "{two}");
        assert!(!side_by_side_message(&p, &[(1, "API"), (2, "UI")], 5, None).contains("Subagents setting"), "no setting of its own: no limit said");
    }
}
