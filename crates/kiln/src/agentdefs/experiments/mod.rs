//! A person's experiments on their own agent definitions (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d P7.1, its data,
//! operations and draw; the UI's Experiments sections): each splits one definition's new sessions between
//! deployments. A definition may have several, one of them live at a time.
//!
//! - **Its variants** are deployments a new session starts in: a microVM made from an Agentfile and sized (vCPUs,
//!   memory), or its machine's host, and a model where one varies that too. Each has a weight, a whole percent of the
//!   split; together they're 100. Its first is the definition as it runs now (`current`) until one is promoted.
//! - **Its criteria** are queries in kiln's monitoring syntax (Datadog's), each compared with a bound by one of kiln's
//!   monitors' comparators: kept as their person wrote them, once kiln's monitors read them.
//! - **Its life**: live (its definition's new sessions drawn by its weights), paused (none drawn), stopped, or
//!   promoted (its variant made the definition's next version). Each change of its weights is its split's next
//!   version.
//! - **The draw** ([`assign`], from `crate::harness::lineage::start`): while it's live, each new session of the
//!   definition its owner starts (`agentdefs.run`, or a UI agent their definition stands in for, `super::uses`) is
//!   one of its variants: `kiln_ci::split::draw_parts` of its weights, its seed (a random number kiln made as it
//!   started) and the draw's index, so the same experiment draws the same again. Each draw is kept with its session,
//!   or with why its start was refused, which is that variant's failure ([`started`]).
//! - **Whose**: its definition's owner's, as agent definitions are ([`super::Who`]). Another's is as none, and kiln's
//!   own definitions take none.
//!
//! What its sessions measure (warm start, peak memory, failures) and judging its criteria through kiln's monitors
//! are P7.1's next part.

pub mod ops;
#[cfg(test)]
mod tests;

use std::collections::BTreeMap;

use rusqlite::{Connection, OptionalExtension as _, params};
use serde::Serialize;

use super::{AgentDef, Patch, Settings, Who, builtin};
use crate::harness::engine::Engine;
use crate::hub::Hub;

/// A split's parts: its weights are whole percents.
pub const PARTS: u32 = 100;

/// The most variants an experiment has: a split has [`PARTS`] parts, so no more can each have one.
pub const MOST_VARIANTS: usize = PARTS as usize;

/// How an experiment stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "AgentdefExperimentState"))]
#[serde(rename_all = "lowercase")]
pub enum State {
    /// Its definition's new sessions are drawn by its weights.
    Live,
    /// None are drawn until it's resumed.
    Paused,
    /// One of its variants was made its definition's next version: it's over.
    Promoted,
    /// It's over, its definition as it was.
    Stopped,
}

impl State {
    fn as_str(self) -> &'static str {
        match self {
            State::Live => "live",
            State::Paused => "paused",
            State::Promoted => "promoted",
            State::Stopped => "stopped",
        }
    }

    /// As kept; one a newer kiln kept that this one doesn't know reads as stopped, which draws nothing.
    fn of(s: &str) -> State {
        match s {
            "live" => State::Live,
            "paused" => State::Paused,
            "promoted" => State::Promoted,
            _ => State::Stopped,
        }
    }

    /// Its split still holds: its weights and variants may change.
    fn running(self) -> bool {
        matches!(self, State::Live | State::Paused)
    }
}

/// How a criterion compares its query's value with its bound: kiln's monitors' comparators (`kiln_obs::monitor::Op`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize, schemars::JsonSchema)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "AgentdefCriterionOp"))]
pub enum Comparison {
    #[serde(rename = ">")]
    Above,
    #[serde(rename = ">=")]
    AtLeast,
    #[serde(rename = "==")]
    Equal,
    #[serde(rename = "!=")]
    NotEqual,
    #[serde(rename = "<")]
    Below,
    #[serde(rename = "<=")]
    AtMost,
}

impl Comparison {
    /// As kiln's monitors write it.
    pub fn as_str(self) -> &'static str {
        match self {
            Comparison::Above => ">",
            Comparison::AtLeast => ">=",
            Comparison::Equal => "==",
            Comparison::NotEqual => "!=",
            Comparison::Below => "<",
            Comparison::AtMost => "<=",
        }
    }

    fn of(s: &str) -> Option<Comparison> {
        [Comparison::Above, Comparison::AtLeast, Comparison::Equal, Comparison::NotEqual, Comparison::Below, Comparison::AtMost].into_iter().find(|c| c.as_str() == s)
    }
}

/// A variant: a deployment new sessions start in, and its share of them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "AgentdefVariant"))]
pub struct Variant {
    pub id: String,
    /// The name its person gave it; none: what sets it apart from the others, as the UI says it.
    pub name: Option<String>,
    /// Where its sessions run.
    pub engine: Engine,
    /// A microVM's Agentfile: one of its owner's (`agentfile:<id>`) or kiln's for its harness (`harness-<harness>`);
    /// none on the host.
    pub agentfile: Option<String>,
    /// A microVM's vCPUs; none on the host.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub cpus: Option<u32>,
    /// A microVM's memory, in bytes; none on the host.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub memory: Option<u64>,
    /// Its model; none: its definition's.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub model: Option<String>,
    /// Its whole percent of new sessions: its experiment's variants' add up to 100.
    pub weight: u32,
    /// How many sessions it was drawn for started.
    pub sessions: u64,
    /// It's its definition as it runs now: its experiment's first variant, until one is promoted, then that one.
    pub current: bool,
}

/// A criterion: what its person judges the variants by.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "AgentdefCriterion"))]
pub struct Criterion {
    pub id: String,
    pub name: String,
    /// What it measures, in kiln's monitoring syntax (Datadog's, as kiln's monitors take a query), its comparison
    /// apart: `avg(last_7d):avg:kiln.session.warm_ms{experiment:<id>} by {variant}`.
    pub query: String,
    pub op: Comparison,
    pub bound: f64,
}

/// An experiment, as its operations answer it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "AgentdefExperiment"))]
pub struct Experiment {
    pub id: String,
    /// Its agent definition.
    pub agentdef: String,
    pub name: String,
    pub state: State,
    /// Since when it's stood so, Unix ms.
    pub state_at: i64,
    /// Its split's version: 1, and one more with each change of its weights.
    pub version: i64,
    /// How many sessions it drew for started.
    pub sessions: u64,
    /// When it started, Unix ms.
    pub started_at: i64,
    /// The variant it promoted, once it did.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub promoted: Option<String>,
    /// Who started it, as the audit trail names them.
    pub by: String,
    pub criteria: Vec<Criterion>,
    pub variants: Vec<Variant>,
}

/// A failure the caller can act on.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Bad(String),
    #[error("no such experiment")]
    Missing,
    /// A variant or criterion it doesn't have.
    #[error("{0}")]
    NoSuch(String),
    /// What its state, or another of its definition's, doesn't let it do.
    #[error("{0}")]
    Conflict(String),
    /// A change made over split version `base`, which another change moved on from.
    #[error("its split was changed since version {base}, which this change was made over: it's version {now}. Read it again and make your change over that (`base` {now})")]
    Stale { base: i64, now: i64 },
    /// One of kiln's own definitions, which take none.
    #[error("{0} is kiln's own agent definition, and takes no experiment: copy it as yours (agentdefs.copy) and experiment on that")]
    Kilns(String),
    /// Variants kiln can't run: each reason.
    #[error("{}", .0.join("; "))]
    Refused(Vec<String>),
    /// Its definition's: none reached, or another change came first.
    #[error(transparent)]
    Def(#[from] super::Error),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

type R<T> = Result<T, Error>;

/// A variant as a new experiment, or an added one, asks for it (`weight` a new experiment's alone: one added joins
/// at 0).
#[derive(Debug, Clone, Default)]
pub struct VariantAsk {
    pub name: Option<String>,
    pub weight: u32,
    /// Left out: a microVM.
    pub engine: Option<Engine>,
    /// A microVM's: left out, its definition's (kiln's for its harness where it names none).
    pub agentfile: Option<String>,
    /// A microVM's: left out, its definition's, where it says them.
    pub cpus: Option<u32>,
    pub memory: Option<u64>,
    /// Left out: its definition's.
    pub model: Option<String>,
}

/// A criterion as it's added.
#[derive(Debug, Clone)]
pub struct CriterionAsk {
    pub name: String,
    pub query: String,
    pub op: Comparison,
    pub bound: f64,
}

/// What a new experiment asks for.
#[derive(Debug, Clone, Default)]
pub struct Ask {
    pub name: String,
    pub variants: Vec<VariantAsk>,
    pub criteria: Vec<CriterionAsk>,
}

/// A variant's deployment, checked, as kept.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Deployment {
    name: Option<String>,
    engine: Engine,
    agentfile: Option<String>,
    cpus: Option<u32>,
    memory: Option<u64>,
    model: Option<String>,
}

/// What an experiment is apart from its variants and criteria, as kept.
#[derive(Debug, Clone)]
struct Head {
    id: String,
    agentdef: String,
    name: String,
    state: State,
    state_at: i64,
    version: i64,
    promoted: Option<String>,
    started_at: i64,
    by: String,
}

const HEAD: &str = "id, agent_def_id, name, state, state_at, split_version, promoted, started_at, by";

fn head(r: &rusqlite::Row<'_>) -> rusqlite::Result<Head> {
    let state: String = r.get(3)?;
    Ok(Head { id: r.get(0)?, agentdef: r.get(1)?, name: r.get(2)?, state: State::of(&state), state_at: r.get(4)?, version: r.get(5)?, promoted: r.get(6)?, started_at: r.get(7)?, by: r.get(8)? })
}

/// A variant as kept.
#[derive(Debug, Clone)]
struct Row {
    id: String,
    weight: u32,
    deployment: Deployment,
}

const VARIANT: &str = "id, name, weight, engine, agentfile, cpus, memory, model";

fn variant(r: &rusqlite::Row<'_>) -> rusqlite::Result<Row> {
    let engine: String = r.get(3)?;
    let memory: Option<i64> = r.get(6)?;
    Ok(Row {
        id: r.get(0)?,
        weight: r.get(2)?,
        deployment: Deployment {
            name: r.get(1)?,
            engine: if engine == Engine::Host.as_str() { Engine::Host } else { Engine::Microvm },
            agentfile: r.get(4)?,
            cpus: r.get(5)?,
            memory: memory.and_then(|m| u64::try_from(m).ok()),
            model: r.get(7)?,
        },
    })
}

/// A variant in a few words, for what kiln says of it: its name, else its deployment.
fn shown(v: &Row) -> String {
    let d = &v.deployment;
    if let Some(n) = &d.name {
        return n.clone();
    }
    match d.engine {
        Engine::Host => "the host".to_owned(),
        Engine::Microvm => format!("{} with {} vCPU and {} MiB", d.agentfile.as_deref().unwrap_or("its Agentfile"), d.cpus.unwrap_or_default(), d.memory.unwrap_or_default() / super::MIB),
    }
}

fn variants_in(c: &Connection, experiment: &str) -> rusqlite::Result<Vec<Row>> {
    let mut st = c.prepare_cached(&format!("SELECT {VARIANT} FROM agent_def_experiment_variants WHERE experiment_id = ?1 ORDER BY position, id"))?;
    let rows = st.query_map(params![experiment], variant)?.collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn criteria_in(c: &Connection, experiment: &str) -> rusqlite::Result<Vec<Criterion>> {
    let mut st = c.prepare_cached("SELECT id, name, query, op, bound FROM agent_def_experiment_criteria WHERE experiment_id = ?1 ORDER BY position, id")?;
    let rows = st.query_map(params![experiment], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?, r.get::<_, f64>(4)?)))?;
    let mut out = Vec::new();
    for row in rows {
        let (id, name, query, op, bound) = row?;
        // One a newer kiln wrote with a comparator this one doesn't know isn't shown as another's.
        if let Some(op) = Comparison::of(&op) {
            out.push(Criterion { id, name, query, op, bound });
        }
    }
    Ok(out)
}

/// `h` whole: its variants (each with how many of its draws' sessions started, and whether it's what its definition
/// runs now) and its criteria.
fn whole_in(c: &Connection, h: Head) -> rusqlite::Result<Experiment> {
    let rows = variants_in(c, &h.id)?;
    let mut started: BTreeMap<String, u64> = BTreeMap::new();
    let mut st = c.prepare_cached("SELECT variant_id, COUNT(*) FROM agent_def_experiment_draws WHERE experiment_id = ?1 AND session_id IS NOT NULL GROUP BY variant_id")?;
    for row in st.query_map(params![h.id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))? {
        let (v, n) = row?;
        started.insert(v, u64::try_from(n).unwrap_or_default());
    }
    let current = h.promoted.clone().or_else(|| rows.first().map(|v| v.id.clone()));
    let variants = rows
        .into_iter()
        .map(|Row { id, weight, deployment: d }| Variant {
            sessions: started.get(&id).copied().unwrap_or_default(),
            current: current.as_deref() == Some(id.as_str()),
            id,
            name: d.name,
            engine: d.engine,
            agentfile: d.agentfile,
            cpus: d.cpus,
            memory: d.memory,
            model: d.model,
            weight,
        })
        .collect();
    let criteria = criteria_in(c, &h.id)?;
    Ok(Experiment {
        id: h.id,
        agentdef: h.agentdef,
        name: h.name,
        state: h.state,
        state_at: h.state_at,
        version: h.version,
        // Every draw's session that started, a removed variant's among them.
        sessions: started.values().sum(),
        started_at: h.started_at,
        promoted: h.promoted,
        by: h.by,
        criteria,
        variants,
    })
}

/// Experiment `id` of definition `def`, its head.
fn head_in(c: &Connection, id: &str, def: &str) -> rusqlite::Result<Option<Head>> {
    c.query_row(&format!("SELECT {HEAD} FROM agent_def_experiments WHERE id = ?1 AND agent_def_id = ?2"), params![id, def], head).optional()
}

/// Experiment `id` of definition `def`, whole.
fn find_in(c: &Connection, id: &str, def: &str) -> rusqlite::Result<Option<Experiment>> {
    head_in(c, id, def)?.map(|h| whole_in(c, h)).transpose()
}

/// The name of definition `def`'s live experiment other than `except`, if it has one.
fn live_in(c: &Connection, def: &str, except: &str) -> rusqlite::Result<Option<String>> {
    c.query_row("SELECT name FROM agent_def_experiments WHERE agent_def_id = ?1 AND state = 'live' AND id <> ?2", params![def, except], |r| r.get(0)).optional()
}

fn one_live(name: &str) -> Error {
    Error::Conflict(format!("{name} is live on this agent, and one at a time splits its sessions: pause, stop or promote it first"))
}

fn over(h: &Head) -> Error {
    Error::Conflict(format!("{} is {}: it's over", h.name, h.state.as_str()))
}

fn no_variant(h: &Head, id: &str) -> Error {
    Error::NoSuch(format!("{} has no variant {id}", h.name))
}

/// `text` as a name: trimmed, one line; none where it's empty.
fn line(text: &str, what: &str) -> R<Option<String>> {
    let t = text.trim();
    if t.chars().any(char::is_control) {
        return Err(Error::Bad(format!("{what} is one line of text")));
    }
    Ok((!t.is_empty()).then(|| t.to_owned()))
}

/// Whether `weights` are a split: each a whole percent, together [`PARTS`].
fn whole(weights: &[u32]) -> R<()> {
    if let Some(w) = weights.iter().find(|w| **w > PARTS) {
        return Err(Error::Bad(format!("a weight is a whole percent of new sessions, {PARTS} at most: not {w}")));
    }
    let sum: u64 = weights.iter().map(|w| u64::from(*w)).sum();
    if sum != u64::from(PARTS) {
        return Err(Error::Bad(format!("a split's weights are whole percents that add up to {PARTS}, not {sum}")));
    }
    Ok(())
}

/// kiln's own definitions take no experiment.
fn not_kilns(def: &str) -> R<()> {
    if def.starts_with(builtin::PREFIX) { Err(Error::Kilns(def.to_owned())) } else { Ok(()) }
}

/// Definition `def`, one `who` reaches that takes experiments: theirs (anyone's for `who.all`), not kiln's own.
async fn reach(hub: &Hub, who: &Who, def: &str) -> R<AgentDef> {
    not_kilns(def)?;
    Ok(super::get(hub, who, def).await?)
}

/// The deployment `ask` asks of definition `d` (its owner's), said to be `which` in what kiln says of it: its engine
/// (a microVM by default); a microVM's Agentfile and size, the definition's where it gives none; its model, none for
/// the definition's. Checked as the definition would be were it run so (`super::check`).
async fn deployment(hub: &Hub, d: &AgentDef, ask: &VariantAsk, which: &str) -> R<Deployment> {
    let refuse = |why: Vec<String>| Error::Refused(why.into_iter().map(|w| format!("{which}: {w}")).collect());
    let name = line(ask.name.as_deref().unwrap_or_default(), "a variant's name")?;
    let model = match ask.model.as_deref().map(str::trim) {
        Some("") => return Err(refuse(vec!["`model` names a model, or is left out for its agent's".into()])),
        m => m.map(str::to_owned),
    };
    let engine = ask.engine.unwrap_or(Engine::Microvm);
    let mut s: Settings = d.settings.clone();
    s.model = model.clone().or(s.model);
    match engine {
        // What's given of a microVM is refused by the check, as a host's session has none.
        Engine::Host => (s.engine, s.agentfile, s.cpus, s.memory) = (Some(Engine::Host), ask.agentfile.clone(), ask.cpus, ask.memory),
        Engine::Microvm => {
            s.engine = None;
            s.agentfile = ask.agentfile.as_deref().map(str::trim).map(str::to_owned).or_else(|| d.settings.microvm_agentfile());
            s.cpus = ask.cpus.or(d.settings.cpus);
            s.memory = ask.memory.or(d.settings.memory);
            let mut unsaid = Vec::new();
            if s.agentfile.is_none() {
                unsaid.push(format!("kiln ships no Agentfile for {}: name one of yours (`agentfile`)", s.harness));
            }
            if s.cpus.is_none() {
                unsaid.push("say its microVM's vCPUs (`cpus`)".to_owned());
            }
            if s.memory.is_none() {
                unsaid.push("say its microVM's memory, in bytes (`memory`)".to_owned());
            }
            if let Some(m) = s.memory.filter(|m| i64::try_from(*m).is_err()) {
                unsaid.push(format!("`memory` is more bytes than kiln keeps a number of: {m}"));
            }
            if !unsaid.is_empty() {
                return Err(refuse(unsaid));
            }
        }
    }
    let why = super::check(hub, &d.owner, &s).await?;
    if !why.is_empty() {
        return Err(refuse(why));
    }
    Ok(Deployment { name, engine, agentfile: s.agentfile, cpus: s.cpus, memory: s.memory, model })
}

/// `ask` checked as a criterion: named, its query one kiln's monitors read with its comparison and bound after it
/// (`kiln_obs::monitor::parse`, as observe's monitor check reads one), kept as it was written.
fn criterion(ask: CriterionAsk) -> R<CriterionAsk> {
    let name = line(&ask.name, "a criterion's name")?.ok_or_else(|| Error::Bad("a criterion needs its name".into()))?;
    let query = ask.query.trim().to_owned();
    if query.is_empty() {
        return Err(Error::Bad("a criterion's `query` is what it measures, in kiln's monitoring syntax".into()));
    }
    if !ask.bound.is_finite() {
        return Err(Error::Bad("a criterion's `bound` is a number".into()));
    }
    let whole = format!("{query} {} {}", ask.op.as_str(), ask.bound);
    match kiln_obs::monitor::parse(&whole) {
        Ok(q) if q.op.as_str() == ask.op.as_str() && q.threshold.to_bits() == ask.bound.to_bits() => Ok(CriterionAsk { name, query, op: ask.op, bound: ask.bound }),
        Ok(_) => Err(Error::Bad(format!("a criterion's `query` is what's compared, its comparison apart (`op`, `bound`): not {query}"))),
        Err(e) => Err(Error::Bad(format!("kiln's monitors can't read `{whole}`: {e}"))),
    }
}

fn insert_variant(c: &Connection, experiment: &str, position: i64, d: &Deployment, weight: u32) -> rusqlite::Result<()> {
    let memory = d.memory.and_then(|m| i64::try_from(m).ok());
    c.execute(
        "INSERT INTO agent_def_experiment_variants (experiment_id, id, position, name, weight, engine, agentfile, cpus, memory, model) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![experiment, crate::util::new_id(), position, d.name, weight, d.engine.as_str(), d.agentfile, d.cpus, memory, d.model],
    )?;
    Ok(())
}

fn insert_criterion(c: &Connection, experiment: &str, position: i64, k: &CriterionAsk) -> rusqlite::Result<()> {
    c.execute(
        "INSERT INTO agent_def_experiment_criteria (experiment_id, id, position, name, query, op, bound) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![experiment, crate::util::new_id(), position, k.name, k.query, k.op.as_str(), k.bound],
    )?;
    Ok(())
}

/// The place after the last of `table`'s rows of `experiment`.
fn next_position(c: &Connection, table: &str, experiment: &str) -> rusqlite::Result<i64> {
    c.query_row(&format!("SELECT COALESCE(MAX(position) + 1, 0) FROM {table} WHERE experiment_id = ?1"), params![experiment], |r| r.get(0))
}

/// The experiments of definition `def` (one `who` reaches: another's is as none, kiln's own have none), or of all
/// `who`'s own (with `everyone`, everyone's: the server's owner's and those with `agentdefs.manage`), by definition,
/// each one's newest first.
pub async fn list(hub: &Hub, who: &Who, def: Option<&str>, everyone: bool) -> R<BTreeMap<String, Vec<Experiment>>> {
    if everyone && !who.all {
        return Err(Error::Def(super::Error::Forbidden(format!("everyone's experiments are for the server's owner and those with {}", super::MANAGE))));
    }
    let (filter, key) = match def {
        Some(id) if id.starts_with(builtin::PREFIX) => return Ok(BTreeMap::new()),
        Some(id) => {
            super::get(hub, who, id).await?;
            ("WHERE agent_def_id = ?1", Some(id.to_owned()))
        }
        None if everyone => ("", None),
        None => ("WHERE owner = ?1", Some(who.person.clone())),
    };
    let all: Vec<Experiment> = hub
        .store
        .read(move |c| {
            let mut st = c.prepare(&format!("SELECT {HEAD} FROM agent_def_experiments {filter} ORDER BY started_at DESC, id DESC"))?;
            let heads = match &key {
                Some(k) => st.query_map(params![k], head)?.collect::<Result<Vec<_>, _>>()?,
                None => st.query_map([], head)?.collect::<Result<Vec<_>, _>>()?,
            };
            let mut out = Vec::with_capacity(heads.len());
            for h in heads {
                out.push(whole_in(c, h)?);
            }
            Ok(out)
        })
        .await?;
    let mut grouped: BTreeMap<String, Vec<Experiment>> = BTreeMap::new();
    for e in all {
        grouped.entry(e.agentdef.clone()).or_default().push(e);
    }
    Ok(grouped)
}

/// A new experiment on definition `def`, live from now on: its draws from `seed` (randomness its caller passes in), by
/// `by`. Its 2 to [`MOST_VARIANTS`] variants are each checked as the definition run so would be, their weights whole
/// percents that add up to 100; its criteria each read by kiln's monitors. Refused while another of the definition's
/// is live.
pub async fn create(hub: &Hub, who: &Who, def: &str, ask: Ask, seed: u64, by: &str) -> R<Experiment> {
    let d = reach(hub, who, def).await?;
    let name = line(&ask.name, "an experiment's name")?.ok_or_else(|| Error::Bad("an experiment needs its name".into()))?;
    if ask.variants.len() < 2 || ask.variants.len() > MOST_VARIANTS {
        return Err(Error::Bad(format!("an experiment splits its agent's new sessions between 2 and {MOST_VARIANTS} variants (`variants`): a split has {PARTS} parts")));
    }
    whole(&ask.variants.iter().map(|v| v.weight).collect::<Vec<_>>())?;
    let mut made = Vec::with_capacity(ask.variants.len());
    for (k, v) in ask.variants.iter().enumerate() {
        made.push((deployment(hub, &d, v, &format!("variant {}", k + 1)).await?, v.weight));
    }
    let criteria = ask.criteria.into_iter().map(criterion).collect::<R<Vec<_>>>()?;
    let (id, def, owner, by, at, seed) = (crate::util::new_id(), d.id, d.owner, by.to_owned(), crate::util::now_ms(), i64::from_ne_bytes(seed.to_ne_bytes()));
    hub.store
        .call(move |c| {
            let tx = crate::store::tx(c)?;
            if let Some(live) = live_in(&tx, &def, &id)? {
                return Ok(Err(one_live(&live)));
            }
            tx.execute(
                "INSERT INTO agent_def_experiments (id, agent_def_id, owner, name, state, state_at, split_version, seed, draws, promoted, started_at, by) VALUES (?1, ?2, ?3, ?4, 'live', ?5, 1, ?6, 0, NULL, ?5, ?7)",
                params![id, def, owner, name, at, seed, by],
            )?;
            for (k, (dep, weight)) in (0i64..).zip(&made) {
                insert_variant(&tx, &id, k, dep, *weight)?;
            }
            for (k, crit) in (0i64..).zip(&criteria) {
                insert_criterion(&tx, &id, k, crit)?;
            }
            let e = find_in(&tx, &id, &def)?;
            tx.commit()?;
            Ok(e.ok_or(Error::Missing))
        })
        .await?
}

/// Experiment `id` of definition `def` renamed `name`; with `variant`, that variant renamed instead (no name: back to
/// what sets it apart). Whatever it stands at.
pub async fn rename(hub: &Hub, who: &Who, def: &str, id: &str, name: Option<&str>, variant: Option<&str>) -> R<Experiment> {
    reach(hub, who, def).await?;
    let named = line(name.unwrap_or_default(), if variant.is_some() { "a variant's name" } else { "an experiment's name" })?;
    if variant.is_none() && named.is_none() {
        return Err(Error::Bad("an experiment needs its name".into()));
    }
    let (def, id, variant) = (def.to_owned(), id.to_owned(), variant.map(str::to_owned));
    hub.store
        .call(move |c| {
            let tx = crate::store::tx(c)?;
            let Some(h) = head_in(&tx, &id, &def)? else { return Ok(Err(Error::Missing)) };
            match &variant {
                Some(v) => {
                    if tx.execute("UPDATE agent_def_experiment_variants SET name = ?3 WHERE experiment_id = ?1 AND id = ?2", params![h.id, v, named])? == 0 {
                        return Ok(Err(no_variant(&h, v)));
                    }
                }
                None => {
                    tx.execute("UPDATE agent_def_experiments SET name = ?2 WHERE id = ?1", params![h.id, named])?;
                }
            }
            let e = find_in(&tx, &id, &def)?;
            tx.commit()?;
            Ok(e.ok_or(Error::Missing))
        })
        .await?
}

/// Experiment `id`'s split set to `weights` (each of its variants' whole percent, together 100), over split version
/// `base` where it names one: its split's next version, while it's live or paused. The same weights again are no new
/// version.
pub async fn weights(hub: &Hub, who: &Who, def: &str, id: &str, weights: BTreeMap<String, u32>, base: Option<i64>) -> R<Experiment> {
    reach(hub, who, def).await?;
    let (def, id) = (def.to_owned(), id.to_owned());
    hub.store
        .call(move |c| {
            let tx = crate::store::tx(c)?;
            let Some(h) = head_in(&tx, &id, &def)? else { return Ok(Err(Error::Missing)) };
            if !h.state.running() {
                return Ok(Err(over(&h)));
            }
            if let Some(base) = base.filter(|b| *b != h.version) {
                return Ok(Err(Error::Stale { base, now: h.version }));
            }
            let rows = variants_in(&tx, &h.id)?;
            if let Some(stray) = weights.keys().find(|k| !rows.iter().any(|v| v.id == **k)) {
                return Ok(Err(no_variant(&h, stray)));
            }
            let unweighted: Vec<String> = rows.iter().filter(|v| !weights.contains_key(&v.id)).map(shown).collect();
            if !unweighted.is_empty() {
                return Ok(Err(Error::Bad(format!("say every variant's weight: {} has none", unweighted.join(", ")))));
            }
            let next: Vec<u32> = rows.iter().map(|v| weights.get(&v.id).copied().unwrap_or_default()).collect();
            if let Err(e) = whole(&next) {
                return Ok(Err(e));
            }
            if rows.iter().zip(&next).any(|(v, w)| v.weight != *w) {
                for (v, w) in rows.iter().zip(&next) {
                    tx.execute("UPDATE agent_def_experiment_variants SET weight = ?3 WHERE experiment_id = ?1 AND id = ?2", params![h.id, v.id, w])?;
                }
                tx.execute("UPDATE agent_def_experiments SET split_version = split_version + 1 WHERE id = ?1", params![h.id])?;
            }
            let e = find_in(&tx, &id, &def)?;
            tx.commit()?;
            Ok(e.ok_or(Error::Missing))
        })
        .await?
}

/// A variant added to experiment `id`, last, at weight 0 (its split as it was: weights give it a share), checked as
/// its definition run so would be; while it's live or paused, and has fewer than [`MOST_VARIANTS`].
pub async fn add_variant(hub: &Hub, who: &Who, def: &str, id: &str, ask: VariantAsk) -> R<Experiment> {
    let d = reach(hub, who, def).await?;
    let dep = deployment(hub, &d, &ask, "the new variant").await?;
    let (def, id) = (def.to_owned(), id.to_owned());
    hub.store
        .call(move |c| {
            let tx = crate::store::tx(c)?;
            let Some(h) = head_in(&tx, &id, &def)? else { return Ok(Err(Error::Missing)) };
            if !h.state.running() {
                return Ok(Err(over(&h)));
            }
            if variants_in(&tx, &h.id)?.len() >= MOST_VARIANTS {
                return Ok(Err(Error::Conflict(format!("{} has {MOST_VARIANTS} variants, as many as a split of {PARTS} parts can give one each", h.name))));
            }
            let at = next_position(&tx, "agent_def_experiment_variants", &h.id)?;
            insert_variant(&tx, &h.id, at, &dep, 0)?;
            let e = find_in(&tx, &id, &def)?;
            tx.commit()?;
            Ok(e.ok_or(Error::Missing))
        })
        .await?
}

/// Variant `variant` taken out of experiment `id`, while it's live or paused: never what its definition runs now, and
/// never below 2. Its share goes back to the others in proportion (`kiln_ci::split::rescale`), its split's next
/// version where it had one. Its draws stay the experiment's.
pub async fn remove_variant(hub: &Hub, who: &Who, def: &str, id: &str, variant: &str) -> R<Experiment> {
    reach(hub, who, def).await?;
    let (def, id, variant) = (def.to_owned(), id.to_owned(), variant.to_owned());
    hub.store
        .call(move |c| {
            let tx = crate::store::tx(c)?;
            let Some(h) = head_in(&tx, &id, &def)? else { return Ok(Err(Error::Missing)) };
            if !h.state.running() {
                return Ok(Err(over(&h)));
            }
            let rows = variants_in(&tx, &h.id)?;
            let Some(gone) = rows.iter().find(|v| v.id == variant) else { return Ok(Err(no_variant(&h, &variant))) };
            let current = h.promoted.clone().or_else(|| rows.first().map(|v| v.id.clone()));
            if current.as_deref() == Some(variant.as_str()) {
                return Ok(Err(Error::Conflict(format!("{} is what its agent runs now: the split keeps it", shown(gone)))));
            }
            if rows.len() <= 2 {
                return Ok(Err(Error::Conflict(format!("{} splits between 2 variants, the fewest a split has: stop it instead", h.name))));
            }
            let rest: Vec<&Row> = rows.iter().filter(|v| v.id != variant).collect();
            if gone.weight > 0 {
                let Some(next) = kiln_ci::split::rescale(&rest.iter().map(|v| v.weight).collect::<Vec<_>>(), PARTS) else {
                    return Ok(Err(Error::Conflict(format!("{} takes every new session: give the others their share first (agentdefs.experiments.weights)", shown(gone)))));
                };
                for (v, w) in rest.iter().zip(&next) {
                    tx.execute("UPDATE agent_def_experiment_variants SET weight = ?3 WHERE experiment_id = ?1 AND id = ?2", params![h.id, v.id, w])?;
                }
                tx.execute("UPDATE agent_def_experiments SET split_version = split_version + 1 WHERE id = ?1", params![h.id])?;
            }
            tx.execute("DELETE FROM agent_def_experiment_variants WHERE experiment_id = ?1 AND id = ?2", params![h.id, variant])?;
            let e = find_in(&tx, &id, &def)?;
            tx.commit()?;
            Ok(e.ok_or(Error::Missing))
        })
        .await?
}

/// A criterion added to experiment `id`, last, once kiln's monitors read it; whatever the experiment stands at.
pub async fn add_criterion(hub: &Hub, who: &Who, def: &str, id: &str, ask: CriterionAsk) -> R<Experiment> {
    reach(hub, who, def).await?;
    let k = criterion(ask)?;
    let (def, id) = (def.to_owned(), id.to_owned());
    hub.store
        .call(move |c| {
            let tx = crate::store::tx(c)?;
            let Some(h) = head_in(&tx, &id, &def)? else { return Ok(Err(Error::Missing)) };
            let at = next_position(&tx, "agent_def_experiment_criteria", &h.id)?;
            insert_criterion(&tx, &h.id, at, &k)?;
            let e = find_in(&tx, &id, &def)?;
            tx.commit()?;
            Ok(e.ok_or(Error::Missing))
        })
        .await?
}

/// Criterion `criterion` taken out of experiment `id`, whatever it stands at.
pub async fn remove_criterion(hub: &Hub, who: &Who, def: &str, id: &str, criterion: &str) -> R<Experiment> {
    reach(hub, who, def).await?;
    let (def, id, criterion) = (def.to_owned(), id.to_owned(), criterion.to_owned());
    hub.store
        .call(move |c| {
            let tx = crate::store::tx(c)?;
            let Some(h) = head_in(&tx, &id, &def)? else { return Ok(Err(Error::Missing)) };
            if tx.execute("DELETE FROM agent_def_experiment_criteria WHERE experiment_id = ?1 AND id = ?2", params![h.id, criterion])? == 0 {
                return Ok(Err(Error::NoSuch(format!("{} has no criterion {criterion}", h.name))));
            }
            let e = find_in(&tx, &id, &def)?;
            tx.commit()?;
            Ok(e.ok_or(Error::Missing))
        })
        .await?
}

/// A change of how an experiment stands, as a person asks it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Move {
    /// Live to paused: no more draws.
    Pause,
    /// Paused to live again, while no other of its definition's is.
    Resume,
    /// Live or paused to stopped: over, its definition as it was.
    Stop,
}

/// Experiment `id` moved as `to` says. Asked again where it stands already, it's as it is; one that's over moves no
/// more.
pub async fn set_state(hub: &Hub, who: &Who, def: &str, id: &str, to: Move) -> R<Experiment> {
    reach(hub, who, def).await?;
    let (def, id, at) = (def.to_owned(), id.to_owned(), crate::util::now_ms());
    hub.store
        .call(move |c| {
            let tx = crate::store::tx(c)?;
            let Some(h) = head_in(&tx, &id, &def)? else { return Ok(Err(Error::Missing)) };
            let next = match (to, h.state) {
                (Move::Pause, State::Paused) | (Move::Resume, State::Live) | (Move::Stop, State::Stopped) => None,
                (Move::Pause, State::Live) => Some(State::Paused),
                (Move::Resume, State::Paused) => match live_in(&tx, &def, &h.id)? {
                    Some(live) => return Ok(Err(one_live(&live))),
                    None => Some(State::Live),
                },
                (Move::Stop, State::Live | State::Paused) => Some(State::Stopped),
                (_, State::Promoted | State::Stopped) => return Ok(Err(over(&h))),
            };
            if let Some(next) = next {
                tx.execute("UPDATE agent_def_experiments SET state = ?2, state_at = ?3 WHERE id = ?1", params![h.id, next.as_str(), at])?;
            }
            let e = find_in(&tx, &id, &def)?;
            tx.commit()?;
            Ok(e.ok_or(Error::Missing))
        })
        .await?
}

/// What making variant `v` its definition's next version changes of `s`, its settings now: its engine, a microVM's
/// Agentfile where it's another than the one `s` runs in, its size, and its model where it has one of its own.
fn promotion(s: &Settings, v: &Variant) -> Patch {
    let mut p = Patch::default();
    match v.engine {
        Engine::Host => (p.engine, p.agentfile, p.cpus, p.memory) = (Some(Some(Engine::Host)), Some(None), Some(None), Some(None)),
        Engine::Microvm => {
            p.engine = Some(None);
            if v.agentfile != s.agentfile_in_use() {
                p.agentfile = Some(v.agentfile.clone());
            }
            (p.cpus, p.memory) = (Some(v.cpus), Some(v.memory));
        }
    }
    if let Some(m) = &v.model {
        p.model = Some(Some(m.clone()));
    }
    p
}

/// Variant `variant` promoted, while experiment `id` is live or paused: made its definition's next version (checked
/// and kept as any change of it is, `super::sync::update`: what it shares with its Agentfile's `AGENT` written there
/// first, P5.4; over the version it's at as this begins: another change between refuses it), and the experiment over.
/// The definition as it is now comes back with it. Promoted already, it's as it is.
pub async fn promote(hub: &Hub, who: &Who, def: &str, id: &str, variant: &str, by: &str) -> R<(Experiment, AgentDef)> {
    let d = reach(hub, who, def).await?;
    let (dd, ii) = (def.to_owned(), id.to_owned());
    let e = hub.store.read(move |c| Ok(find_in(c, &ii, &dd)?)).await?.ok_or(Error::Missing)?;
    let Some(v) = e.variants.iter().find(|v| v.id == variant).cloned() else {
        return Err(Error::NoSuch(format!("{} has no variant {variant}", e.name)));
    };
    match e.state {
        State::Promoted if e.promoted.as_deref() == Some(variant) => return Ok((e, d)),
        State::Promoted | State::Stopped => return Err(Error::Conflict(format!("{} is {}: it's over", e.name, e.state.as_str()))),
        State::Live | State::Paused => {}
    }
    let d = super::sync::update(hub, who, def, promotion(&d.settings, &v), Some(d.version), by).await?;
    let (def, id, variant, at) = (def.to_owned(), id.to_owned(), variant.to_owned(), crate::util::now_ms());
    let e = hub
        .store
        .call(move |c| {
            let tx = crate::store::tx(c)?;
            let Some(h) = head_in(&tx, &id, &def)? else { return Ok(Err(Error::Missing)) };
            if !h.state.running() {
                return Ok(Err(over(&h)));
            }
            tx.execute("UPDATE agent_def_experiments SET state = 'promoted', promoted = ?2, state_at = ?3 WHERE id = ?1", params![h.id, variant, at])?;
            let e = find_in(&tx, &id, &def)?;
            tx.commit()?;
            Ok(e.ok_or(Error::Missing))
        })
        .await??;
    Ok((e, d))
}

/// Experiment `id` deleted, its variants, criteria and draws with it, whatever it stands at: what it was. The
/// sessions it drew go on as they started.
pub async fn delete(hub: &Hub, who: &Who, def: &str, id: &str) -> R<Experiment> {
    reach(hub, who, def).await?;
    let (def, id) = (def.to_owned(), id.to_owned());
    hub.store
        .call(move |c| {
            let tx = crate::store::tx(c)?;
            let Some(e) = find_in(&tx, &id, &def)? else { return Ok(Err(Error::Missing)) };
            for table in ["agent_def_experiment_draws", "agent_def_experiment_criteria", "agent_def_experiment_variants"] {
                tx.execute(&format!("DELETE FROM {table} WHERE experiment_id = ?1"), params![id])?;
            }
            tx.execute("DELETE FROM agent_def_experiments WHERE id = ?1", params![id])?;
            tx.commit()?;
            Ok(Ok(e))
        })
        .await?
}

/// Definition `def`'s experiments forgotten, their variants, criteria and draws with them, as it goes.
pub fn forget_in(c: &Connection, def: &str) -> rusqlite::Result<usize> {
    for table in ["agent_def_experiment_draws", "agent_def_experiment_criteria", "agent_def_experiment_variants"] {
        c.execute(&format!("DELETE FROM {table} WHERE experiment_id IN (SELECT id FROM agent_def_experiments WHERE agent_def_id = ?1)"), params![def])?;
    }
    c.execute("DELETE FROM agent_def_experiments WHERE agent_def_id = ?1", params![def])
}

/// A new session's draw: the variant its definition's live experiment gave it, and what it starts as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Draw {
    pub experiment: String,
    pub variant: String,
    /// Its index among its experiment's draws, from 0, and the split version it was drawn by.
    pub n: i64,
    pub version: i64,
    pub engine: Engine,
    pub agentfile: Option<String>,
    pub cpus: Option<u32>,
    pub memory: Option<u64>,
    /// Its own model; none: its definition's.
    pub model: Option<String>,
}

impl Draw {
    /// As its session keeps it (`Spec::experiment`), for its life.
    pub fn assigned(&self) -> crate::harness::session::Assigned {
        crate::harness::session::Assigned { experiment: self.experiment.clone(), arm: self.variant.clone(), promoted: false }
    }

    /// Its microVM's size as a start takes it (vCPUs, MiB); none on the host.
    pub fn size(&self) -> Option<(u32, u64)> {
        (self.engine == Engine::Microvm).then(|| (self.cpus.unwrap_or_default(), self.memory.map_or(0, |m| m / super::MIB)))
    }
}

/// Draws experiment `id`'s next, while it's live, kept as its own (its index taken in the same transaction, so no
/// two draws share one); none once it isn't.
fn draw_in(c: &Connection, id: &str, at: i64) -> anyhow::Result<Option<Draw>> {
    let tx = crate::store::tx(c)?;
    let Some((state, seed, n, version)) = tx.query_row("SELECT state, seed, draws, split_version FROM agent_def_experiments WHERE id = ?1", params![id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?))).optional()? else {
        return Ok(None);
    };
    if State::of(&state) != State::Live {
        return Ok(None);
    }
    let rows = variants_in(&tx, id)?;
    let parts: Vec<u32> = rows.iter().map(|v| v.weight).collect();
    let (seed, index) = (u64::from_ne_bytes(seed.to_ne_bytes()), u64::try_from(n)?);
    let Some(v) = kiln_ci::split::draw_parts(&parts, seed, index).and_then(|k| rows.get(k)) else { return Ok(None) };
    tx.execute("INSERT INTO agent_def_experiment_draws (experiment_id, draw, variant_id, split_version, at) VALUES (?1, ?2, ?3, ?4, ?5)", params![id, n, v.id, version, at])?;
    tx.execute("UPDATE agent_def_experiments SET draws = draws + 1 WHERE id = ?1", params![id])?;
    let d = &v.deployment;
    let drawn = Draw { experiment: id.to_owned(), variant: v.id.clone(), n, version, engine: d.engine, agentfile: d.agentfile.clone(), cpus: d.cpus, memory: d.memory, model: d.model.clone() };
    tx.commit()?;
    Ok(Some(drawn))
}

/// The variant a new session of definition `def` that `person` starts is (P7.1): its live experiment's draw, where
/// it's one of theirs with one live; none otherwise (it starts as its definition is), or where the draw couldn't be
/// kept (said in the log: the session starts as asked).
pub async fn assign(hub: &Hub, def: &str, person: &str) -> Option<Draw> {
    if def.starts_with(builtin::PREFIX) {
        return None;
    }
    // A read first: most definitions have none live, and a read never waits behind a write.
    let (d, p) = (def.to_owned(), person.to_owned());
    let live: Option<String> = match hub.store.read(move |c| Ok(c.query_row("SELECT id FROM agent_def_experiments WHERE agent_def_id = ?1 AND owner = ?2 AND state = 'live'", params![d, p], |r| r.get(0)).optional()?)).await {
        Ok(live) => live,
        Err(e) => {
            tracing::warn!(definition = %def, error = %format!("{e:#}"), "an agent definition's live experiment couldn't be read: its session starts as its definition is");
            None
        }
    };
    let id = live?;
    let at = crate::util::now_ms();
    let experiment = id.clone();
    match hub.store.call(move |c| draw_in(c, &id, at)).await {
        Ok(drawn) => drawn,
        Err(e) => {
            tracing::warn!(%experiment, error = %format!("{e:#}"), "an experiment's draw wasn't kept: its session starts as its definition is");
            None
        }
    }
}

/// What draw `d`'s start came to, kept as its own: its session, or why it was refused (its variant's failure).
pub async fn started(hub: &Hub, d: &Draw, came: Result<String, String>) {
    let (experiment, n) = (d.experiment.clone(), d.n);
    let (session, refused) = match came {
        Ok(s) => (Some(s), None),
        Err(why) => (None, Some(why)),
    };
    let kept = hub
        .store
        .call(move |c| {
            c.execute("UPDATE agent_def_experiment_draws SET session_id = ?3, refused = ?4 WHERE experiment_id = ?1 AND draw = ?2", params![experiment, n, session, refused])?;
            Ok(())
        })
        .await;
    if let Err(e) = kept {
        tracing::warn!(experiment = %d.experiment, draw = d.n, error = %format!("{e:#}"), "what an experiment's draw came to wasn't kept");
    }
}
