//! The sessions started from agent definitions (kiln plan 01a1dd2e…, P5.2's page: an agent's row holds its sessions,
//! each with the version it started from, so one its definition has moved on from says so). As kiln opens a session
//! from a definition it keeps which, and at which version ([`record`]): a run of one (`agentdefs.run`), and a start of
//! one of kiln's UI agents, made from the person's own definition standing in for it or from kiln's own (`super::uses`).
//! It's kiln's to say, never what was asked (`crate::harness::lineage::Ask::definition`). `agentdefs.sessions` lists a
//! definition's ([`list`]), with what its starts took, or every definition's the caller sees at once ([`all`]: the
//! Agents page's rows); `agents.get` says a session's ([`of`]).
//!
//! What a start took comes from kiln's record of each start of a session's harness on its machine
//! (`crate::harness::session::Start`), of the sessions kiln still has: how it started, which that record says by its
//! engine (its harness a process of its machine's, or in a microVM), and, for a session's first start, how long from
//! its session's open on its machine to its harness running (room waits and image builds included), both read off
//! that machine's one clock. A microVM's start says too how its microVM started and how long that took, where Hex
//! measured it (`crate::harness::session::Measured`, P7.1): those are counted by kind ([`by_start`]). Every recorded
//! start counts: no window is picked.

use rusqlite::{OptionalExtension as _, params};
use serde::Serialize;
use serde_json::{Value, json};

use super::{Error, R, Who, builtin};
use crate::harness::session::{Info, Runs, Start};
use crate::hub::Hub;

/// The definition a session started from: its id, and its version then (kiln's own, which have no versions: 0).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Definition {
    pub id: String,
    pub version: i64,
}

impl Definition {
    /// One of kiln's own (`kiln:<name>`).
    pub fn kilns(id: &str) -> Definition {
        Definition { id: id.to_owned(), version: 0 }
    }
}

/// Session `session`, placed on `target` and acting for `person` (none: no one), started from `d`: kept, so its
/// definition lists it. A failure to keep it is logged, and the session runs either way.
pub async fn record(hub: &Hub, session: &str, d: &Definition, person: Option<&str>, target: &str) {
    let (s, id, version, p, t, at) = (session.to_owned(), d.id.clone(), d.version, person.unwrap_or_default().to_owned(), target.to_owned(), crate::util::now_ms());
    let kept = hub
        .store
        .call(move |c| {
            c.execute(
                "INSERT INTO agent_def_sessions (session_id, agent_def_id, version, person, target, at) VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
                 ON CONFLICT (session_id) DO UPDATE SET agent_def_id = excluded.agent_def_id, version = excluded.version, person = excluded.person, target = excluded.target, at = excluded.at",
                params![s, id, version, p, t, at],
            )?;
            Ok(())
        })
        .await;
    match kept {
        // Its definition's limits, where it sets any, are held to it from now (P5.4).
        Ok(()) => hub.harness.limits.tell(session),
        Err(e) => tracing::warn!(session, definition = %d.id, error = %format!("{e:#}"), "which agent definition a session started from wasn't kept"),
    }
}

/// A definition's name and the version it's at now (kiln's own: 0); none for one that's gone.
async fn now_of(hub: &Hub, id: &str) -> R<Option<(String, i64)>> {
    if let Some(b) = builtin::get(id) {
        return Ok(Some((b.name, 0)));
    }
    let i = id.to_owned();
    let row: Option<(String, i64)> = hub.store.read(move |c| Ok(c.query_row("SELECT name, version FROM agent_defs WHERE id = ?1", params![i], |r| Ok((r.get(0)?, r.get(1)?))).optional()?)).await?;
    Ok(row)
}

/// The definition session `session` started from, as `agents.get` says it: its id, the version it started from, the
/// definition's name and the version it's at now (`current`; a session behind it is one whose `version` is less),
/// both none once it's gone. None for a session that started from none.
pub async fn of(hub: &Hub, session: &str) -> R<Option<Value>> {
    let s = session.to_owned();
    let row: Option<(String, i64)> = hub.store.read(move |c| Ok(c.query_row("SELECT agent_def_id, version FROM agent_def_sessions WHERE session_id = ?1", params![s], |r| Ok((r.get(0)?, r.get(1)?))).optional()?)).await?;
    let Some((id, version)) = row else { return Ok(None) };
    let now = now_of(hub, &id).await?;
    Ok(Some(json!({"id": id, "version": version, "name": now.as_ref().map(|n| &n.0), "current": now.as_ref().map(|n| n.1)})))
}

/// One session started from a definition, as `agentdefs.sessions` lists it.
#[derive(Debug, Clone, Serialize)]
pub struct Listed {
    /// The session's id (as `agents.*` name it).
    pub id: String,
    /// The definition's version it started from.
    pub version: i64,
    /// When it started from it, Unix ms.
    pub at: i64,
    /// Its runner (as `agents.list`'s `machine`: `local` for the server).
    pub runner: String,
    /// How it stands (as `agents.list`'s `stand`), or `gone` once kiln no longer has it.
    pub state: String,
    /// Where its harness runs: `host` (a process of its machine's) or `microvm`; none once it's gone.
    pub engine: Option<&'static str>,
    /// It as `agents.list` briefs it; none once it's gone.
    pub agent: Option<Value>,
    /// Why a limit of its definition (its time, tokens or cost) ended it (P5.4: `super::limits`), where one did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ended: Option<String>,
    /// The definition's experiment that drew it, and the variant it got (`super::experiments`, kiln plan
    /// 01a1cd8e862e7169a074db3d5fa33f3d P7.1); none for a session started outside one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub experiment: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
}

/// What a start of a harness was, as kiln's record of it says: its engine. (How a microVM started is its measure's,
/// [`by_start`].)
fn kind_of(runs: &Runs) -> &'static str {
    match runs {
        Runs::Host { .. } => "host",
        Runs::Microvm { .. } => "microvm",
    }
}

/// What the starts of `infos` took, by how each started ([`kind_of`]): how many there were, how many were timed
/// (each session's first: from its open on its machine to its harness running), and the median of those times.
fn starts(infos: &[Info]) -> Value {
    let mut kinds: std::collections::BTreeMap<&'static str, (u64, Vec<u64>)> = std::collections::BTreeMap::new();
    let mut n: u64 = 0;
    for info in infos {
        for (k, s) in info.starts.iter().enumerate() {
            n = n.saturating_add(1);
            let kind = kinds.entry(kind_of(&s.runs)).or_default();
            kind.0 = kind.0.saturating_add(1);
            // Its session's open and its start, on that machine's one clock: a first start before its open was read
            // off another's (a session moved on), and isn't timed.
            if k == 0
                && let Ok(ms) = u64::try_from(s.at.saturating_sub(info.created_at))
            {
                kind.1.push(ms);
            }
        }
    }
    let by_kind: serde_json::Map<String, Value> = kinds.into_iter().map(|(k, (n, timed))| (k.to_owned(), json!({"n": n, "timed": timed.len(), "p50_ms": kiln_ci::work::slots::median(&timed)}))).collect();
    // A warm pool's hosts, where its runners' Hex nodes report them: none does yet (kiln plan 01a1dd2e…, P3.1, P3.2).
    json!({"n": n, "by_kind": by_kind, "by_start": by_start(infos.iter().flat_map(|i| i.starts.iter())), "warm": Value::Null})
}

/// The kind of start a microVM's was, of Shards' word for how it started (its D253, Hex's `StartedFrom`): `cold` where
/// its guest booted (`booted`; `saved`, its image's template saved on the way; `replaced`, a template that failed to
/// restore made again), `template` where it restored its image's template (`restored`), `warm` where a warm run
/// host's microVM was ready before its run asked (`warm`), and `swapped` where its member was swapped into the
/// microVM its session ran in (P6c.5). A word kiln doesn't know yet is a kind of its own, so no start goes uncounted.
pub(super) fn start_kind(started_from: &str) -> String {
    match started_from {
        "booted" | "saved" | "replaced" => "cold",
        "restored" => "template",
        "warm" => "warm",
        crate::sandbox::isolated::SWAPPED => "swapped",
        other => other,
    }
    .to_owned()
}

/// The starts of `starts` whose microVM said how it started (`Measured::started_from`), by kind ([`start_kind`]): how
/// many, how many were timed (from the launch asked to its member running, `Measured::start_us`), the median of those
/// times in microseconds, and how many each of Shards' words gave the kind.
fn by_start<'a>(starts: impl Iterator<Item = &'a Start>) -> Value {
    type Kind = (u64, Vec<u64>, std::collections::BTreeMap<String, u64>);
    let mut kinds: std::collections::BTreeMap<String, Kind> = std::collections::BTreeMap::new();
    for m in starts.filter_map(|s| s.measured.as_ref()) {
        let Some(from) = m.started_from.as_deref() else { continue };
        let kind = kinds.entry(start_kind(from)).or_default();
        kind.0 = kind.0.saturating_add(1);
        if let Some(us) = m.start_us {
            kind.1.push(us);
        }
        let word = kind.2.entry(from.to_owned()).or_default();
        *word = word.saturating_add(1);
    }
    Value::Object(
        kinds
            .into_iter()
            .map(|(k, (n, timed, from))| (k, json!({"n": n, "timed": timed.len(), "p50_us": kiln_ci::work::slots::median(&timed), "from": from})))
            .collect(),
    )
}

/// Definition `id`'s sessions that `who` may see (their own; everyone's for `who.all`), newest first, and what their
/// starts took. `id` is one of theirs (another's is as none) or one of kiln's own.
pub async fn list(hub: &Hub, who: &Who, id: &str) -> R<Value> {
    match builtin::get(id) {
        Some(_) => {}
        None if id.starts_with(builtin::PREFIX) => return Err(Error::Missing),
        None => {
            super::get(hub, who, id).await?;
        }
    }
    let (i, person, all) = (id.to_owned(), who.person.clone(), who.all);
    // Each with why a limit of its definition ended it, and the experiment's draw it started as, where either did.
    let rows: Vec<Row> = hub
        .store
        .read(move |c| {
            let mut st = c.prepare(&format!(
                "SELECT s.session_id, s.version, s.target, s.at, {ENDED}, x.experiment_id, x.variant_id FROM agent_def_sessions s {} LEFT JOIN agent_def_experiment_draws x ON x.session_id = s.session_id \
                 WHERE s.agent_def_id = ?1 AND (?3 OR s.person = ?2) ORDER BY s.at DESC, s.session_id",
                ended_join()
            ))?;
            let rows = st.query_map(params![i, person, all], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)))?.collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await?;
    listed_of(hub, rows)
}

/// Every definition's sessions that `who` may see, by the definition's id, each as [`list`] answers for it: of theirs
/// (everyone's for `who.all`) and of kiln's own as they started them, in one read. A definition none of whose sessions
/// they see isn't among them, nor one that's gone (deleted, or one of kiln's it no longer has).
pub async fn all(hub: &Hub, who: &Who) -> R<Value> {
    let (person, all) = (who.person.clone(), who.all);
    // Each with its definition's owner, why a limit of it ended it, and the experiment's draw it started as, where
    // either did.
    let rows: Vec<(String, Option<String>, Row)> = hub
        .store
        .read(move |c| {
            let mut st = c.prepare(&format!(
                "SELECT s.agent_def_id, d.owner, s.session_id, s.version, s.target, s.at, {ENDED}, x.experiment_id, x.variant_id FROM agent_def_sessions s LEFT JOIN agent_defs d ON d.id = s.agent_def_id {} \
                 LEFT JOIN agent_def_experiment_draws x ON x.session_id = s.session_id WHERE (?2 OR s.person = ?1) ORDER BY s.agent_def_id, s.at DESC, s.session_id",
                ended_join()
            ))?;
            let rows = st.query_map(params![person, all], |r| Ok((r.get(0)?, r.get(1)?, (r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?))))?.collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await?;
    let mut by_def: std::collections::BTreeMap<String, Vec<Row>> = std::collections::BTreeMap::new();
    for (def, owner, row) in rows {
        // One the caller reaches (another's is as none, as `list` has it), or one of kiln's own that it still has.
        let seen = match owner {
            Some(owner) => who.reaches(&owner),
            None => builtin::get(&def).is_some(),
        };
        if seen {
            by_def.entry(def).or_default().push(row);
        }
    }
    let mut answer = serde_json::Map::new();
    for (def, rows) in by_def {
        answer.insert(def, listed_of(hub, rows)?);
    }
    Ok(Value::Object(answer))
}

/// One session of a definition as kiln keeps it: its id, the definition's version it started from, where it was
/// placed and when (Unix ms), and why a limit of its definition ended it, where one did; and the experiment that drew
/// it and the variant it got, where one did (`super::experiments`, kiln plan 01a1cd8e862e7169a074db3d5fa33f3d P7.1).
type Row = (String, i64, String, i64, Option<String>, Option<String>, Option<String>);

/// Why a limit ended a session, read beside it: its row of `kv` ([`super::limits::ended_key`]), joined as `e`.
const ENDED: &str = "e.v";

fn ended_join() -> String {
    format!("LEFT JOIN kv e ON e.k = '{}' || s.session_id", super::limits::ENDED)
}

/// The sessions `rows` keep, in their order, as `agentdefs.sessions` lists them, and what their starts took.
fn listed_of(hub: &Hub, rows: Vec<Row>) -> R<Value> {
    let mut listed = Vec::with_capacity(rows.len());
    let mut infos = Vec::new();
    for (session, version, target, at, ended, experiment, variant) in rows {
        match hub.harness.info(&session) {
            Some(info) => {
                listed.push(Listed {
                    id: session,
                    version,
                    at,
                    runner: crate::harness::wait::machine(&info.target).to_owned(),
                    state: crate::harness::wait::stand(&info).to_owned(),
                    engine: Some(if info.isolation.is_some() { "microvm" } else { "host" }),
                    agent: Some(crate::harness::wait::brief(&info)),
                    ended,
                    experiment,
                    variant,
                });
                infos.push(info);
            }
            None => listed.push(Listed { id: session, version, at, runner: crate::harness::wait::machine(&target).to_owned(), state: "gone".to_owned(), engine: None, agent: None, ended, experiment, variant }),
        }
    }
    let sessions = serde_json::to_value(&listed).map_err(|e| Error::Other(e.into()))?;
    Ok(json!({"sessions": sessions, "starts": starts(&infos)}))
}

/// Definition `id`'s sessions forgotten, as it goes.
pub fn forget_in(c: &rusqlite::Connection, id: &str) -> rusqlite::Result<usize> {
    c.execute("DELETE FROM kv WHERE k IN (SELECT ?2 || session_id FROM agent_def_sessions WHERE agent_def_id = ?1)", params![id, super::limits::ENDED])?;
    c.execute("DELETE FROM agent_def_sessions WHERE agent_def_id = ?1", params![id])
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::by_start;
    use crate::harness::session::{Measured, Runs, Start};

    fn start(started_from: Option<&str>, start_us: Option<u64>) -> Start {
        let measured = started_from.map(|f| Measured { start_us, started_from: Some(f.to_owned()), ..Measured::default() });
        Start { at: 0, runs: Runs::Host { pid: 1 }, ended: None, measured }
    }

    /// kiln plan 01a1dd2e…, P5.2's page (uicontract slice 2b): a definition's starts by how each microVM started, each
    /// kind's median start where Hex measured it, and Shards' words each kind counts; starts that measured nothing are
    /// in no kind, and a word kiln doesn't know is a kind of its own.
    #[test]
    fn p5_2_starts_are_counted_by_how_their_microvm_started() {
        let starts = [
            start(Some("warm"), Some(1_000)),
            start(Some("warm"), Some(3_000)),
            start(Some("warm"), Some(2_000)),
            start(Some("restored"), Some(30_000)),
            start(Some("booted"), Some(90_000)),
            start(Some("saved"), Some(300_000)),
            start(Some("replaced"), None),
            start(Some("swapped"), Some(500)),
            start(Some("thawed"), Some(7)),
            start(None, None),
        ];
        let got = by_start(starts.iter());
        let want = json!({
            "warm": {"n": 3, "timed": 3, "p50_us": 2_000, "from": {"warm": 3}},
            "template": {"n": 1, "timed": 1, "p50_us": 30_000, "from": {"restored": 1}},
            "cold": {"n": 3, "timed": 2, "p50_us": 90_000, "from": {"booted": 1, "replaced": 1, "saved": 1}},
            "swapped": {"n": 1, "timed": 1, "p50_us": 500, "from": {"swapped": 1}},
            "thawed": {"n": 1, "timed": 1, "p50_us": 7, "from": {"thawed": 1}},
        });
        assert_eq!(got, want);
        assert_eq!(by_start(std::iter::empty()), json!({}), "no microVM start: no kind");
    }
}
