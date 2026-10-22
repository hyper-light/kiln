//! Research (docs/research.md): a question an agent looks into, what it found, and how far kiln
//! trusts each finding. The agent records its sources (each one's kind and where it is: a file and
//! its lines, a page, a DOI, a record of kiln's) and its findings, each a claim with the words it
//! quotes from its sources and the counterclaims it found; kiln looks for each quote where it says
//! it is (`check`), and grades each finding the same way every time (`score`), so what the agent
//! presents always carries its sources and a grade the person can weigh.
//!
//! A plan's planning phase is one (`plan`): what its steps rest on. Research stands alone too, for
//! any question a person asks an agent to look into.

pub mod check;
pub mod ops;
pub mod score;

use std::sync::Arc;

use rusqlite::{OptionalExtension as _, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use score::{Confidence, Type};

use crate::hub::Hub;

/// A research's sources and findings at the most, and the longest of each part.
const SOURCES: usize = 500;
const FINDINGS: usize = 300;
const CITES: usize = 30;
const COUNTERS: usize = 20;
const TEXT: usize = 4000;
const TITLE: usize = 300;
const LOCATOR: usize = 2000;

/// One source: what it is and where.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Source {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: Type,
    pub title: String,
    /// Where it is: a file (and its lines: `src/a.rs:40-52`), a URL, `doi:…`, or a record of
    /// kiln's (`plan:<id>`, `agent:<id>`), or for a measurement the command run.
    pub locator: String,
    /// The machine a file is on, when it isn't the agent's own.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub machine: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    /// When it was published or last changed, as it says.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub published: Option<String>,
    /// Who or what stands behind it, when sources of different places share one (a vendor's docs
    /// and its blog): they count once.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    /// Out of date for this question, as the agent judges it.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub dated: bool,
    /// When the agent read it (unix ms).
    pub retrieved_at: i64,
}

impl Source {
    /// On the machines and systems the agent works in, rather than elsewhere.
    pub fn internal(&self) -> bool {
        self.kind.internal()
    }
}

/// How kiln's look for a quote came out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckState {
    /// Being looked for.
    Pending,
    /// The quote is where it says it is.
    Found,
    /// Read where it says, and the quote isn't there.
    NotFound,
    /// A dead link, a file that isn't there.
    Gone,
    /// There, its content not words to look for (a picture, a file that isn't text), or nothing
    /// quoted to look for.
    Exists,
    /// Not looked for: what it is can't be read here (a PDF, a page drawn by scripts, a sign-in),
    /// or its machine can't be asked (`said` says why).
    Unchecked,
}

/// kiln's look for a citation's quote.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Check {
    pub state: CheckState,
    pub at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub said: Option<String>,
}

/// A source cited: the words quoted from it and where they are.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Cite {
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quote: Option<String>,
    /// Where in the source: lines, a section, a page, a time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub check: Option<Check>,
}

/// How strong a counterclaim is, as the agent says, when it has no sources of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Strength {
    Strong,
    Moderate,
    Weak,
}

/// A claim against a finding, with its own sources.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Counter {
    pub claim: String,
    pub cites: Vec<Cite>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strength: Option<Strength>,
}

/// One finding: a claim and what it rests on.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Finding {
    pub id: String,
    pub claim: String,
    pub cites: Vec<Cite>,
    pub against: Vec<Counter>,
    /// The agent infers it from its sources rather than reads it in them.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub inferred: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// When it was last changed (unix ms).
    pub at: i64,
}

/// Why research has no internal, or no external, sources: the agent says so.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Skipped {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub internal: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external: Option<String>,
}

/// Research as kept.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Research {
    pub id: String,
    pub question: String,
    /// The agent that does it (none: a person's).
    pub agent: Option<String>,
    pub by: String,
    /// The plan it's the planning phase of.
    pub plan: Option<String>,
    pub sources: Vec<Source>,
    pub findings: Vec<Finding>,
    pub skipped: Skipped,
    pub created_at: i64,
    pub updated_at: i64,
}

/// A finding with kiln's confidence in it.
#[derive(Debug, Clone, Serialize)]
pub struct Graded {
    #[serde(flatten)]
    pub finding: Finding,
    pub confidence: Confidence,
    /// Each counterclaim's weight against it, 0 to 1, in order.
    pub against_weights: Vec<f64>,
}

/// What research comes to: how many findings of each grade, its sources inside and out, and its
/// quotes still being looked for.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Summary {
    pub findings: usize,
    pub grades: std::collections::BTreeMap<&'static str, usize>,
    pub sources: usize,
    pub internal: usize,
    pub external: usize,
    pub checking: usize,
}

/// Research as it's shown: its findings graded, best first in the order the agent gave them.
#[derive(Debug, Clone, Serialize)]
pub struct View {
    pub id: String,
    pub question: String,
    pub agent: Option<String>,
    pub by: String,
    pub plan: Option<String>,
    pub sources: Vec<Source>,
    pub findings: Vec<Graded>,
    pub skipped: Skipped,
    pub summary: Summary,
    pub created_at: i64,
    pub updated_at: i64,
    /// For presenting it: its findings with their grades and a numbered list of their sources.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub markdown: Option<String>,
}

/// A failure the caller can act on.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Bad(String),
    #[error("no such research")]
    Missing,
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub type R<T> = Result<T, Error>;

/// One change to research at a time (an agent's additions, kiln's checks coming back): what's
/// changed is changed on it as it is.
static CHANGING: async_lock::Mutex<()> = async_lock::Mutex::new(());

fn clip(s: &str, n: usize) -> String {
    let t = s.trim();
    if t.chars().count() > n { format!("{}…", t.chars().take(n).collect::<String>()) } else { t.to_owned() }
}

fn load(c: &rusqlite::Connection, id: &str) -> anyhow::Result<Option<Research>> {
    c.query_row("SELECT id, question, agent, by, plan_id, sources, findings, skipped, created_at, updated_at FROM research WHERE id = ?1", params![id], |r| {
        let json = |i: usize| r.get::<_, String>(i);
        Ok(Research {
            id: r.get(0)?,
            question: r.get(1)?,
            agent: r.get(2)?,
            by: r.get(3)?,
            plan: r.get(4)?,
            sources: serde_json::from_str(&json(5)?).unwrap_or_default(),
            findings: serde_json::from_str(&json(6)?).unwrap_or_default(),
            skipped: serde_json::from_str(&json(7)?).unwrap_or_default(),
            created_at: r.get(8)?,
            updated_at: r.get(9)?,
        })
    })
    .optional()
    .map_err(Into::into)
}

fn save(c: &rusqlite::Connection, r: &Research) -> anyhow::Result<()> {
    c.execute(
        "UPDATE research SET sources = ?2, findings = ?3, skipped = ?4, plan_id = ?5, updated_at = ?6 WHERE id = ?1",
        params![r.id, serde_json::to_string(&r.sources)?, serde_json::to_string(&r.findings)?, serde_json::to_string(&r.skipped)?, r.plan, r.updated_at],
    )?;
    Ok(())
}

/// Research, as kept.
pub async fn get(hub: &Hub, id: &str) -> R<Research> {
    let id = id.to_owned();
    hub.store.call(move |c| load(c, &id)).await?.ok_or(Error::Missing)
}

/// The research a plan's planning phase is, if it has one.
pub async fn of_plan(hub: &Hub, plan: &str) -> Option<String> {
    let plan = plan.to_owned();
    hub.store.call(move |c| Ok(c.query_row("SELECT id FROM research WHERE plan_id = ?1 ORDER BY created_at DESC LIMIT 1", params![plan], |r| r.get(0)).optional()?)).await.ok().flatten()
}

/// Research begun on `question`, by `by`, for agent `agent` (none: a person's), for plan `plan`.
pub async fn start(hub: &Hub, question: &str, agent: Option<&str>, by: &str, plan: Option<&str>) -> R<Research> {
    let q = clip(question, TEXT);
    if q.is_empty() {
        return Err(Error::Bad("say what's to be found out: `question`".into()));
    }
    let r = Research {
        id: crate::util::new_id(),
        question: q,
        agent: agent.map(str::to_owned),
        by: by.to_owned(),
        plan: plan.map(str::to_owned),
        created_at: crate::util::now_ms(),
        updated_at: crate::util::now_ms(),
        ..Research::default()
    };
    let row = r.clone();
    hub.store
        .call(move |c| {
            c.execute(
                "INSERT INTO research (id, question, agent, by, plan_id, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
                params![row.id, row.question, row.agent, row.by, row.plan, row.created_at],
            )?;
            Ok(())
        })
        .await?;
    Ok(r)
}

/// Research, newest change first: a plan's, an agent's, or every one (the last 100).
pub async fn list(hub: &Hub, plan: Option<String>, agent: Option<String>) -> R<Vec<View>> {
    let ids: Vec<String> = hub
        .store
        .call(move |c| {
            let mut st = c.prepare("SELECT id FROM research WHERE (?1 IS NULL OR plan_id = ?1) AND (?2 IS NULL OR agent = ?2) ORDER BY updated_at DESC LIMIT 100")?;
            let v = st.query_map(params![plan, agent], |r| r.get(0))?.collect::<Result<Vec<String>, _>>()?;
            Ok(v)
        })
        .await?;
    let mut out = Vec::new();
    for id in ids {
        if let Ok(r) = get(hub, &id).await {
            out.push(view(&r, false));
        }
    }
    Ok(out)
}

/// What an agent adds to research: sources, findings (a cite's `source` an id, or a source
/// itself), what goes, and why it has no internal or external sources.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Add {
    pub sources: Vec<Source>,
    pub findings: Vec<AddFinding>,
    pub remove: Vec<String>,
    pub skipped: Option<Skipped>,
}

/// A finding as an agent adds it.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct AddFinding {
    pub id: Option<String>,
    pub claim: String,
    pub cites: Vec<AddCite>,
    pub against: Vec<AddCounter>,
    pub inferred: bool,
    pub note: Option<String>,
}

/// A citation as an agent adds it: of a source by its id, or of a source given in place.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct AddCite {
    pub source: Value,
    pub quote: Option<String>,
    pub at: Option<String>,
}

/// A counterclaim as an agent adds it.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct AddCounter {
    pub claim: String,
    pub cites: Vec<AddCite>,
    pub strength: Option<Strength>,
}

/// The next free id `prefix`N among `taken`.
fn next_id(prefix: char, taken: &[String]) -> String {
    let n = taken.iter().filter_map(|t| t.strip_prefix(prefix).and_then(|n| n.parse::<u32>().ok())).max().unwrap_or(0);
    format!("{prefix}{}", n + 1)
}

/// A source as given, made tidy: its words trimmed and clipped, when it was read set.
fn tidy(mut s: Source, now: i64) -> R<Source> {
    s.title = clip(&s.title, TITLE);
    s.locator = clip(&s.locator, LOCATOR);
    if s.title.is_empty() && s.locator.is_empty() {
        return Err(Error::Bad("a source needs a title or where it is (`locator`)".into()));
    }
    if s.title.is_empty() {
        s.title.clone_from(&s.locator);
    }
    for v in [&mut s.machine, &mut s.author, &mut s.published, &mut s.origin] {
        *v = v.as_deref().map(|x| clip(x, TITLE)).filter(|x| !x.is_empty());
    }
    if s.retrieved_at == 0 {
        s.retrieved_at = now;
    }
    Ok(s)
}

/// Merges `add` into research `r`, at `now`: sources by id (or by where they are: one place, one
/// source), findings by id, removals, and why sources of a kind are missing. A cite keeps kiln's
/// look for its quote when the quote and where it is are as they were; else it's looked for again.
/// Returns how many citations are to be looked for.
pub fn merge(r: &mut Research, add: Add, now: i64) -> R<usize> {
    for id in &add.remove {
        if r.findings.iter().any(|f| &f.id == id) {
            r.findings.retain(|f| &f.id != id);
        } else if r.sources.iter().any(|s| &s.id == id) {
            let cited = r.findings.iter().any(|f| f.cites.iter().chain(f.against.iter().flat_map(|c| c.cites.iter())).any(|c| &c.source == id));
            if cited {
                return Err(Error::Bad(format!("source {id} is cited: take it out of the findings that cite it first")));
            }
            r.sources.retain(|s| &s.id != id);
        } else {
            return Err(Error::Bad(format!("nothing here is {id}")));
        }
    }
    // Sources: one place, one source.
    let put = |r: &mut Research, s: Source| -> R<String> {
        let s = tidy(s, now)?;
        let at = r.sources.iter().position(|x| (!s.id.is_empty() && x.id == s.id) || (!s.locator.is_empty() && x.locator == s.locator));
        match at {
            Some(i) => {
                let id = r.sources.get(i).map(|x| x.id.clone()).unwrap_or_default();
                if let Some(x) = r.sources.get_mut(i) {
                    *x = Source { id: id.clone(), retrieved_at: x.retrieved_at, ..s };
                }
                Ok(id)
            }
            None => {
                let taken: Vec<String> = r.sources.iter().map(|x| x.id.clone()).collect();
                let id = if s.id.trim().is_empty() { next_id('s', &taken) } else { clip(&s.id, 40) };
                r.sources.push(Source { id: id.clone(), ..s });
                Ok(id)
            }
        }
    };
    for s in add.sources {
        put(r, s)?;
    }
    let cite_of = |r: &mut Research, c: AddCite| -> R<Cite> {
        let source = match c.source {
            Value::String(id) => {
                let id = id.trim().to_owned();
                if !r.sources.iter().any(|s| s.id == id) {
                    return Err(Error::Bad(format!("{id} isn't a source here: add it in `sources`, or give the source itself in the citation")));
                }
                id
            }
            v @ Value::Object(_) => put(r, serde_json::from_value(v).map_err(|e| Error::Bad(format!("a cited source: {e}")))?)?,
            _ => return Err(Error::Bad("a citation's `source` is a source's id, or the source itself".into())),
        };
        Ok(Cite { source, quote: c.quote.map(|q| clip(&q, TEXT)).filter(|q| !q.is_empty()), at: c.at.map(|a| clip(&a, TITLE)).filter(|a| !a.is_empty()), check: None })
    };
    for f in add.findings {
        let claim = clip(&f.claim, TEXT);
        if claim.is_empty() {
            return Err(Error::Bad("a finding needs its claim".into()));
        }
        if f.cites.len() > CITES || f.against.len() > COUNTERS {
            return Err(Error::Bad(format!("a finding has {CITES} citations and {COUNTERS} counterclaims at the most")));
        }
        let mut cites = Vec::new();
        for c in f.cites {
            cites.push(cite_of(r, c)?);
        }
        let mut against = Vec::new();
        for k in f.against {
            let mut kc = Vec::new();
            for c in k.cites {
                kc.push(cite_of(r, c)?);
            }
            let kclaim = clip(&k.claim, TEXT);
            if kclaim.is_empty() {
                return Err(Error::Bad("a counterclaim needs its claim".into()));
            }
            against.push(Counter { claim: kclaim, cites: kc, strength: k.strength });
        }
        let id = f.id.map(|i| clip(&i, 40)).filter(|i| !i.is_empty());
        let before = id.as_ref().and_then(|id| r.findings.iter().find(|x| &x.id == id)).cloned();
        // A citation as it was keeps kiln's look for it.
        let kept = |c: Cite| -> Cite {
            let old = before.as_ref().and_then(|b| b.cites.iter().chain(b.against.iter().flat_map(|k| k.cites.iter())).find(|o| o.source == c.source && o.quote == c.quote && o.at == c.at));
            Cite { check: old.and_then(|o| o.check.clone()), ..c }
        };
        let cites: Vec<Cite> = cites.into_iter().map(kept).collect();
        let against: Vec<Counter> = against.into_iter().map(|k| Counter { cites: k.cites.into_iter().map(kept).collect(), ..k }).collect();
        let note = f.note.map(|n| clip(&n, TEXT)).filter(|n| !n.is_empty());
        match id.and_then(|id| r.findings.iter().position(|x| x.id == id).map(|i| (i, id))) {
            Some((i, id)) => {
                if let Some(x) = r.findings.get_mut(i) {
                    *x = Finding { id, claim, cites, against, inferred: f.inferred, note, at: now };
                }
            }
            None => {
                let taken: Vec<String> = r.findings.iter().map(|x| x.id.clone()).collect();
                r.findings.push(Finding { id: next_id('f', &taken), claim, cites, against, inferred: f.inferred, note, at: now });
            }
        }
    }
    if let Some(k) = add.skipped {
        let why = |w: Option<String>| w.map(|x| clip(&x, TITLE)).filter(|x| !x.is_empty());
        r.skipped = Skipped { internal: why(k.internal).or_else(|| r.skipped.internal.clone()), external: why(k.external).or_else(|| r.skipped.external.clone()) };
    }
    if r.sources.len() > SOURCES || r.findings.len() > FINDINGS {
        return Err(Error::Bad(format!("research holds {SOURCES} sources and {FINDINGS} findings at the most")));
    }
    // What's to be looked for: each citation with no look yet.
    let mut pending = 0;
    for f in &mut r.findings {
        for c in f.cites.iter_mut().chain(f.against.iter_mut().flat_map(|k| k.cites.iter_mut())) {
            if c.check.is_none() {
                c.check = Some(Check { state: CheckState::Pending, at: now, said: None });
                pending += 1;
            }
        }
    }
    r.updated_at = now;
    Ok(pending)
}

/// Adds to research `id`, and has kiln look for the new quotes (in the background: `view` shows
/// them being looked for, then how it came out).
pub async fn add(hub: &Arc<Hub>, id: &str, add: Add) -> R<View> {
    let pending = {
        let _one = CHANGING.lock().await;
        let mut r = get(hub, id).await?;
        let pending = merge(&mut r, add, crate::util::now_ms())?;
        let row = r.clone();
        hub.store.call(move |c| save(c, &row)).await?;
        pending
    };
    if pending > 0 {
        let (h, id) = (Arc::clone(hub), id.to_owned());
        crate::rt::spawn(async move { check::run(&h, &id).await });
    }
    Ok(view(&get(hub, id).await?, true))
}

/// Says why research `id` has no internal, or no external, sources (what's said before stays,
/// unless said again).
pub async fn skip(hub: &Hub, id: &str, why: Skipped) -> R<()> {
    let _one = CHANGING.lock().await;
    let mut r = get(hub, id).await?;
    merge(&mut r, Add { skipped: Some(why), ..Add::default() }, crate::util::now_ms())?;
    hub.store.call(move |c| save(c, &r)).await?;
    Ok(())
}

/// Writes how kiln's looks came out into research `id` (each citation by its finding, where it
/// is among the finding's citations, and the quote it was for), as it is now.
pub async fn checked(hub: &Hub, id: &str, results: Vec<check::Result>) -> R<()> {
    let _one = CHANGING.lock().await;
    let mut r = get(hub, id).await?;
    for res in results {
        let Some(f) = r.findings.iter_mut().find(|f| f.id == res.finding) else { continue };
        let cites: Vec<&mut Cite> = f.cites.iter_mut().chain(f.against.iter_mut().flat_map(|k| k.cites.iter_mut())).collect();
        if let Some(c) = cites.into_iter().nth(res.index).filter(|c| c.source == res.source && c.quote == res.quote) {
            c.check = Some(res.check);
        }
    }
    let row = r;
    hub.store.call(move |c| save(c, &row)).await?;
    Ok(())
}

/// Research as it's shown, its findings graded (`markdown`: with the words to present it in).
pub fn view(r: &Research, markdown: bool) -> View {
    let findings: Vec<Graded> = r
        .findings
        .iter()
        .map(|f| Graded { finding: f.clone(), confidence: score::confidence(f, &r.sources), against_weights: f.against.iter().map(|k| score::against(k, &r.sources)).collect() })
        .collect();
    let mut grades = std::collections::BTreeMap::new();
    for g in &findings {
        *grades.entry(g.confidence.grade).or_insert(0) += 1;
    }
    let checking =
        r.findings.iter().flat_map(|f| f.cites.iter().chain(f.against.iter().flat_map(|k| k.cites.iter()))).filter(|c| c.check.as_ref().is_some_and(|k| k.state == CheckState::Pending)).count();
    let summary = Summary {
        findings: findings.len(),
        grades,
        sources: r.sources.len(),
        internal: r.sources.iter().filter(|s| s.internal()).count(),
        external: r.sources.iter().filter(|s| !s.internal()).count(),
        checking,
    };
    let mut v = View {
        id: r.id.clone(),
        question: r.question.clone(),
        agent: r.agent.clone(),
        by: r.by.clone(),
        plan: r.plan.clone(),
        sources: r.sources.clone(),
        findings,
        skipped: r.skipped.clone(),
        summary,
        created_at: r.created_at,
        updated_at: r.updated_at,
        markdown: None,
    };
    if markdown {
        v.markdown = Some(markdown_of(&v));
    }
    v
}

/// What a check's state reads as next to its source.
fn check_words(c: Option<&Check>) -> &'static str {
    match c.map(|c| c.state) {
        Some(CheckState::Found) => "quote found",
        Some(CheckState::NotFound) => "quote NOT found there",
        Some(CheckState::Gone) => "not there (dead link or missing file)",
        Some(CheckState::Exists) => "there",
        Some(CheckState::Pending) => "being checked",
        Some(CheckState::Unchecked) | None => "not checked",
    }
}

/// Research to present: each finding with its grade, score and why, the sources it cites by
/// number (and how kiln's look for each quote came out), and what's against it; then the sources,
/// numbered, each with its kind and where it is.
pub fn markdown_of(v: &View) -> String {
    let number = |id: &str| v.sources.iter().position(|s| s.id == id).map_or(0, |i| i + 1);
    let mut out = format!("**Research:** {}\n\n", v.question);
    let s = &v.summary;
    let grades: Vec<String> = s.grades.iter().map(|(g, n)| format!("{g} {n}")).collect();
    out.push_str(&format!(
        "{} finding{} ({}) from {} source{} ({} internal, {} external){}\n\n",
        s.findings,
        if s.findings == 1 { "" } else { "s" },
        if grades.is_empty() { "none yet".to_owned() } else { grades.join(", ") },
        s.sources,
        if s.sources == 1 { "" } else { "s" },
        s.internal,
        s.external,
        if s.checking > 0 { format!("; {} quotes still being checked", s.checking) } else { String::new() }
    ));
    for g in &v.findings {
        let c = &g.confidence;
        let mut refs: Vec<String> = Vec::new();
        for x in &g.finding.cites {
            let r = format!("[{}]", number(&x.source));
            if !refs.contains(&r) {
                refs.push(r);
            }
        }
        out.push_str(&format!("- **{} · {} ({})** {} {}\n  _{}_\n", c.grade, c.score, c.words, g.finding.claim, refs.join(""), c.why));
        for x in &g.finding.cites {
            if let Some(q) = &x.quote {
                out.push_str(&format!("  - [{}] \"{}\"{} · {}\n", number(&x.source), clip(q, 240), x.at.as_deref().map(|a| format!(" ({a})")).unwrap_or_default(), check_words(x.check.as_ref())));
            }
        }
        for (k, w) in g.finding.against.iter().zip(&g.against_weights) {
            let refs: Vec<String> = k.cites.iter().map(|x| format!("[{}]", number(&x.source))).collect();
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let pct = (w * 100.0).round() as u32;
            out.push_str(&format!("  - Against: {} {} (weighs {pct}%)\n", k.claim, refs.join("")));
        }
    }
    if let Some(w) = &v.skipped.internal {
        out.push_str(&format!("\nNo internal sources: {w}\n"));
    }
    if let Some(w) = &v.skipped.external {
        out.push_str(&format!("\nNo external sources: {w}\n"));
    }
    if !v.sources.is_empty() {
        out.push_str("\n**Sources**\n\n");
        for (i, src) in v.sources.iter().enumerate() {
            let when = src.published.as_deref().map(|p| format!(" · {p}")).unwrap_or_default();
            let dated = if src.dated { " · out of date" } else { "" };
            out.push_str(&format!("{}. {} ({}, {}) — {}{when}{dated}\n", i + 1, src.title, src.kind.words(), if src.internal() { "internal" } else { "external" }, src.locator));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn add(v: Value) -> Add {
        serde_json::from_value(v).unwrap_or_default()
    }

    #[test]
    fn findings_and_sources_merge_by_id_and_place_and_new_quotes_are_looked_for() -> Result<(), Error> {
        let mut r = Research { id: "r1".into(), question: "q".into(), ..Research::default() };
        let n = merge(
            &mut r,
            add(json!({
                "sources": [{"type": "code", "title": "the store", "locator": "src/convo/store.rs:396"}],
                "findings": [{"claim": "it searches", "cites": [
                    {"source": "s1", "quote": "pub fn search"},
                    {"source": {"type": "docs", "title": "FTS5", "locator": "https://sqlite.org/fts5.html"}, "quote": "trigram"}
                ], "against": [{"claim": "slow at scale", "strength": "weak"}]}]
            })),
            10,
        )?;
        assert_eq!((n, r.sources.len(), r.findings.len()), (2, 2, 1));
        assert_eq!(r.sources.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), ["s1", "s2"]);
        assert!(r.findings.first().is_some_and(|f| f.id == "f1" && f.cites.iter().all(|c| c.check.as_ref().is_some_and(|k| k.state == CheckState::Pending))));
        // One place, one source; a finding changed by its id keeps the looks of its quotes as they were.
        if let Some(c) = r.findings.first_mut().and_then(|f| f.cites.first_mut()) {
            c.check = Some(Check { state: CheckState::Found, at: 11, said: None });
        }
        let n = merge(
            &mut r,
            add(json!({"findings": [{"id": "f1", "claim": "it searches, any case", "cites": [
                {"source": {"type": "code", "title": "store.rs", "locator": "src/convo/store.rs:396"}, "quote": "pub fn search"},
                {"source": "s2", "quote": "trigram tokenizer"}
            ]}]})),
            20,
        )?;
        assert_eq!((n, r.sources.len()), (1, 2), "the store's citation as it was; the docs' quote changed");
        let f = r.findings.first().cloned().unwrap_or_default();
        assert_eq!(f.cites.first().and_then(|c| c.check.as_ref()).map(|k| k.state), Some(CheckState::Found));
        assert!(f.against.is_empty() && f.claim == "it searches, any case");
        // Taking out a cited source is refused; the finding first, then it.
        assert!(matches!(merge(&mut r, add(json!({"remove": ["s2"]})), 30), Err(Error::Bad(_))));
        merge(&mut r, add(json!({"remove": ["f1"]})), 30)?;
        merge(&mut r, add(json!({"remove": ["s2"]})), 30)?;
        assert_eq!((r.findings.len(), r.sources.len()), (0, 1));
        // A citation of a source that isn't here, and a source with nothing to it: refused.
        assert!(matches!(merge(&mut r, add(json!({"findings": [{"claim": "c", "cites": [{"source": "s9"}]}]})), 40), Err(Error::Bad(_))));
        assert!(matches!(merge(&mut r, add(json!({"sources": [{"type": "web"}]})), 40), Err(Error::Bad(_))));
        Ok(())
    }

    #[test]
    fn research_is_presented_with_its_grades_and_numbered_sources() -> Result<(), Error> {
        let mut r = Research { id: "r1".into(), question: "Can agents search conversations?".into(), ..Research::default() };
        merge(
            &mut r,
            add(json!({
                "findings": [{"claim": "The store searches every agent's events.", "cites": [{"source": {"type": "code", "title": "store.rs", "locator": "crates/kiln/src/convo/store.rs:396"}, "quote": "pub fn search(&self, q: Query)"}]}],
                "skipped": {"external": "nothing outside kiln bears on it"}
            })),
            1,
        )?;
        if let Some(c) = r.findings.first_mut().and_then(|f| f.cites.first_mut()) {
            c.check = Some(Check { state: CheckState::Found, at: 2, said: None });
        }
        let v = view(&r, true);
        let md = v.markdown.unwrap_or_default();
        assert!(md.contains("**A · 90 (high)** The store searches every agent's events. [1]"), "{md}");
        assert!(md.contains("[1] \"pub fn search(&self, q: Query)\" · quote found"), "{md}");
        assert!(md.contains("1. store.rs (code, internal) — crates/kiln/src/convo/store.rs:396"), "{md}");
        assert!(md.contains("No external sources: nothing outside kiln bears on it"), "{md}");
        assert_eq!((v.summary.internal, v.summary.external, v.summary.grades.get("A").copied()), (1, 0, Some(1)));
        Ok(())
    }
}

/// kiln's looks for quotes, on a hub of its own, under a timeout: an agent's research cites a file in
/// its folder, and kiln reads it there (through the question it asks the agent's machine), finding
/// one quote and not another; the grade follows.
#[cfg(all(test, unix))]
mod checked_tests {
    use std::sync::Arc;
    use std::time::Duration;

    use serde_json::json;

    use super::*;
    use crate::harness::lineage::{Ask, Starter, start};

    #[test]
    fn kiln_looks_for_each_quote_in_the_agent_s_own_folder() -> anyhow::Result<()> {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(crate::rt::block_on(async {
                let hub = crate::hub::tests::scratch_hub_with(|c| {
                    c.terminal.enabled = true;
                    c.conversations.off = true;
                })
                .await?;
                let h = Arc::clone(&hub);
                crate::rt::spawn(async move {
                    let _ = crate::harness::registry::Registry::run_local(h).await;
                });
                hub.harness.set_test_program("/nonexistent/kiln-test-no-harness");
                let folder = tempfile::tempdir()?;
                std::fs::write(folder.path().join("store.rs"), "pub fn search(&self,\n    q: Query) -> Vec<Row> {}\n")?;
                let person = Starter { parent: None, opener: Some("owner".into()), actor: "admin".into(), plan_run: None };
                let ask = Ask { kind: "claude".into(), on: Some("local".into()), cwd: Some(folder.path().to_string_lossy().into_owned()), name: Some("R".into()), count: Some(1), ..Ask::default() };
                let a = start(&hub, &person, ask).await.map_err(|(c, m)| anyhow::anyhow!("{c}: {m}"))?.into_iter().next().ok_or_else(|| anyhow::anyhow!("no agent"))?;
                let r = super::start(&hub, "Does the store search?", Some(&a.id), "admin", None).await?;
                let given: Add = serde_json::from_value(json!({"findings": [
                    {"claim": "it searches", "cites": [{"source": {"type": "code", "title": "store.rs", "locator": "store.rs:1-2"}, "quote": "pub fn search(&self, q: Query)"}]},
                    {"claim": "it finds", "cites": [{"source": "s1", "quote": "pub fn find"}]}
                ]}))?;
                let v = add(&hub, &r.id, given).await?;
                anyhow::ensure!(v.summary.checking == 2, "both quotes looked for: {:?}", v.summary);
                let mut states = Vec::new();
                for _ in 0..200 {
                    let now = get(&hub, &r.id).await?;
                    states = now.findings.iter().filter_map(|f| f.cites.first().and_then(|c| c.check.as_ref()).map(|k| k.state)).collect();
                    if states.iter().all(|s| *s != CheckState::Pending) {
                        break;
                    }
                    crate::rt::sleep(Duration::from_millis(25)).await;
                }
                let said: Vec<Option<String>> = get(&hub, &r.id).await?.findings.iter().filter_map(|f| f.cites.first().and_then(|c| c.check.as_ref()).map(|k| k.said.clone())).collect();
                anyhow::ensure!(states == [CheckState::Found, CheckState::NotFound], "{states:?} {said:?}");
                let v = view(&get(&hub, &r.id).await?, false);
                let grades: Vec<(&str, u8)> = v.findings.iter().map(|g| (g.confidence.grade, g.confidence.score)).collect();
                anyhow::ensure!(grades == [("A", 90), ("F", 27)], "found in full; looked for and not there: {grades:?}");
                Ok(())
            }));
        });
        match rx.recv_timeout(Duration::from_secs(90)) {
            Ok(r) => r,
            Err(_) => anyhow::bail!("it hung"),
        }
    }
}
