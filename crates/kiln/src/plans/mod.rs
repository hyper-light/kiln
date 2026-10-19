//! Plans (docs/plans.md): what an agent will do, kept apart from any one session. A plan is
//! captured from an agent's plan mode or written by hand, changed (each change a version), shared
//! by a link, and run at will on any agent of any harness: as one message, or step by step, each
//! step a message waiting in the agent's queue for the turn before it to end. The same operations
//! serve the console (`/api/plans`), agents (kiln's MCP server: `plans.*`) and code mode.

pub mod agents;
pub mod criteria;
pub mod ops;

use rusqlite::{OptionalExtension as _, params};
use serde::{Deserialize, Serialize};

pub use criteria::{Criterion, Op, Test};
use crate::harness::model::{Command, SendMode};
use crate::harness::registry::Sent;
use crate::harness::session::Spec;
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

/// The session a plan was captured from.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Source {
    pub session: Option<String>,
    pub harness: Option<String>,
    pub machine: Option<String>,
    pub cwd: Option<String>,
    pub agent: Option<String>,
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
    pub items: Option<Vec<Item>>,
    pub tags: Option<Vec<String>>,
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
}

/// A failure the caller can act on.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Bad(String),
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

/// The message that hands an agent the whole plan, under its title (unless it opens with it).
pub fn message(p: &Plan) -> String {
    let all = markdown(p);
    let body = all.trim();
    let first = body.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    if first.starts_with('#') && title_of(first) == p.title {
        format!("Carry out this plan, step by step, and say when it is done.\n\n{body}")
    } else {
        format!("Carry out this plan, step by step, and say when it is done.\n\n# {}\n\n{body}", p.title)
    }
}

/// The message that hands an agent one step of a plan.
pub fn step_message(p: &Plan, i: usize, n: usize, step: &str) -> String {
    format!("Step {} of {} of the plan \"{}\": {}\n\nDo this step only, then say what you did.", i + 1, n, p.title, step)
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
    Ok(())
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

const COLUMNS: &str = "p.id, p.title, p.body, p.tags, p.source, p.share, p.version, p.created_at, p.updated_at, (SELECT COUNT(*) FROM plan_runs r WHERE r.plan_id = p.id), p.items";

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
            let tx = c.transaction()?;
            tx.execute(
                "INSERT INTO plans (id, title, body, items, tags, source, version, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, ?7, ?7)",
                params![id, title, body, items, tags, source, now],
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
            let tx = c.transaction()?;
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
            let tx = c.transaction()?;
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

/// Runs a plan on an agent: an existing one, or a new one started with it.
pub async fn run(hub: &Hub, id: &str, target: Target, mode: Mode, by: &str) -> R<Ran> {
    let p = get(hub, id).await?;
    if mode == Mode::Goals {
        return run_goals(hub, &p, target, by).await;
    }
    if mode == Mode::Agents {
        return Err(Error::Bad("one agent per step starts agents of its own: run it with `new` (agents::start)".into()));
    }
    let messages: Vec<String> = match mode {
        Mode::Message | Mode::Goals | Mode::Agents => vec![message(&p)],
        Mode::Steps => {
            let steps: Vec<&String> = p.steps.iter().take(STEPS).collect();
            if steps.is_empty() {
                return Err(Error::Bad("this plan has no steps (no list in it): run it as one message".into()));
            }
            let n = steps.len();
            steps.iter().enumerate().map(|(i, s)| step_message(&p, i, n, s)).collect()
        }
    };
    let (session, rest) = match target {
        Target::Session(session) => (session, messages.as_slice()),
        Target::New { target, kind, cwd } => {
            let (first, rest) = messages.split_first().ok_or_else(|| Error::Bad("nothing to send".into()))?;
            let spec = Spec { kind, cwd: cwd.filter(|c| !c.trim().is_empty()), prompt: Some(first.clone()), name: Some(p.title.clone()), ..Spec::default() };
            let infos = hub.harness.open(hub, &target, spec, 1).await?;
            let info = infos.into_iter().next().ok_or_else(|| Error::Bad("the agent did not start".into()))?;
            (info.id, rest)
        }
    };
    let mut sent = messages.len().saturating_sub(rest.len());
    for text in rest {
        let c = Command::Send { id: crate::util::new_id(), text: text.clone(), mode: SendMode::Queue, attachments: Vec::new(), overrides: None };
        match hub.harness.command(hub, &session, c).await {
            Sent::Ok => sent += 1,
            Sent::NoSession => return Err(Error::Bad("no such agent".into())),
            Sent::Unreachable => return Err(Error::Bad("that agent's machine is not connected".into())),
        }
    }
    let (run, now, by_s, version, mode_s, plan_id, session_c) = (crate::util::new_id(), crate::util::now_unix(), by.to_owned(), p.version, mode.as_str(), p.id.clone(), session.clone());
    let run_c = run.clone();
    hub.store
        .call(move |c| {
            c.execute(
                "INSERT INTO plan_runs (id, plan_id, version, session, mode, at, by) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![run_c, plan_id, version, session_c, mode_s, now, by_s],
            )?;
            Ok(())
        })
        .await?;
    Ok(Ran { session, sent, run })
}

/// A plan's steps as goals, after the agent's others (a new agent is started for them), and the
/// stack running.
async fn run_goals(hub: &Hub, p: &Plan, target: Target, by: &str) -> R<Ran> {
    let goals: Vec<String> = if p.items.is_empty() { p.steps.iter().take(STEPS).cloned().collect() } else { p.items.iter().take(STEPS).map(Item::goal).collect() };
    if goals.is_empty() {
        return Err(Error::Bad("this plan has no steps (no list in it) to make goals of".into()));
    }
    let session = match target {
        Target::Session(s) => s,
        Target::New { target, kind, cwd } => {
            let spec = Spec { kind, cwd: cwd.filter(|c| !c.trim().is_empty()), name: Some(p.title.clone()), ..Spec::default() };
            let infos = hub.harness.open(hub, &target, spec, 1).await?;
            infos.into_iter().next().ok_or_else(|| Error::Bad("the agent did not start".into()))?.id
        }
    };
    let sent = goals.len();
    crate::goals::add(hub, &session, goals, true).await.map_err(|e| match e {
        crate::goals::Error::NoAgent => Error::Bad("no such agent".into()),
        crate::goals::Error::Bad(m) => Error::Bad(m),
        crate::goals::Error::Other(e) => Error::Other(e),
    })?;
    let (run, now, by_s, version, plan_id, session_c) = (crate::util::new_id(), crate::util::now_unix(), by.to_owned(), p.version, p.id.clone(), session.clone());
    let run_c = run.clone();
    hub.store
        .call(move |c| {
            c.execute(
                "INSERT INTO plan_runs (id, plan_id, version, session, mode, at, by) VALUES (?1, ?2, ?3, ?4, 'goals', ?5, ?6)",
                params![run_c, plan_id, version, session_c, now, by_s],
            )?;
            Ok(())
        })
        .await?;
    Ok(Ran { session, sent, run })
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
    fn tags_are_trimmed_and_unique() {
        assert_eq!(clean_tags(vec![" a ".into(), "a".into(), "".into(), "b".into()]).ok(), Some(vec!["a".to_owned(), "b".to_owned()]));
        assert!(clean_tags(vec!["x".repeat(41)]).is_err());
    }

    #[test]
    fn messages_hand_over_the_plan_or_one_step() {
        let p = Plan { id: "p".into(), title: "T".into(), body: "- a\n- b".into(), items: vec![], tags: vec![], source: None, share: None, version: 1, created_at: 0, updated_at: 0, steps: vec!["a".into(), "b".into()], runs: 0 };
        assert!(message(&p).contains("# T\n\n- a\n- b"));
        let headed = Plan { body: "# T\n\n- a\n- b".into(), ..p.clone() };
        assert_eq!(message(&headed).matches("# T").count(), 1);
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
        };
        let p = Plan { id: "p".into(), title: "Lexer".into(), body: "Why: the scanner grew.".into(), items: vec![it.clone(), Item { title: "Ship it".into(), ..Item::default() }], tags: vec![], source: None, share: None, version: 1, created_at: 0, updated_at: 0, steps: vec![], runs: 0 };
        let md = markdown(&p);
        assert!(md.starts_with("Why: the scanner grew.\n\n## 1. Split the scanner\n\nMove number scanning"), "{md}");
        assert!(md.contains("**Acceptance criteria**\n\n- scan_number exists\n- scan time is at most 2 ms") && md.contains("**Tests**\n\n- hex literals — passes when 0x1F reads as 31") && md.contains("## 2. Ship it"), "{md}");
        assert!(message(&p).contains("# Lexer\n\nWhy: the scanner grew."));
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
    }

}
