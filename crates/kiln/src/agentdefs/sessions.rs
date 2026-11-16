//! The sessions started from agent definitions (kiln plan 01a1dd2e…, P5.2's page: an agent's row holds its sessions,
//! each with the version it started from, so one its definition has moved on from says so). As kiln opens a session
//! from a definition it keeps which, and at which version ([`record`]): a run of one (`agentdefs.run`), and a start of
//! one of kiln's UI agents, made from the person's own definition standing in for it or from kiln's own (`super::uses`).
//! It's kiln's to say, never what was asked (`crate::harness::lineage::Ask::definition`). `agentdefs.sessions` lists a
//! definition's ([`list`]), with what its starts took; `agents.get` says a session's ([`of`]).
//!
//! What a start took comes from kiln's record of each start of a session's harness on its machine
//! (`crate::harness::session::Start`), of the sessions kiln still has: how it started, which that record says by its
//! engine (its harness a process of its machine's, or in a microVM), and, for a session's first start, how long from
//! its session's open on its machine to its harness running (room waits and image builds included), both read off
//! that machine's one clock. Every recorded start counts: no window is picked.

use rusqlite::{OptionalExtension as _, params};
use serde::Serialize;
use serde_json::{Value, json};

use super::{Error, R, Who, builtin};
use crate::harness::session::{Info, Runs};
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
    if let Err(e) = kept {
        tracing::warn!(session, definition = %d.id, error = %format!("{e:#}"), "which agent definition a session started from wasn't kept");
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
    /// The definition's experiment that drew it, and the variant it got (`super::experiments`, kiln plan
    /// 01a1cd8e862e7169a074db3d5fa33f3d P7.1); none for a session started outside one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub experiment: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
}

/// What a start of a harness was, as kiln's record of it says: its engine. (How a microVM started, cold, from a
/// template or warm, isn't in that record.)
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
    json!({"n": n, "by_kind": by_kind, "warm": Value::Null})
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
    // Each with the experiment's draw it started as, where one drew it.
    let rows: Vec<(String, i64, String, i64, Option<String>, Option<String>)> = hub
        .store
        .read(move |c| {
            let mut st = c.prepare(
                "SELECT s.session_id, s.version, s.target, s.at, d.experiment_id, d.variant_id FROM agent_def_sessions s LEFT JOIN agent_def_experiment_draws d ON d.session_id = s.session_id \
                 WHERE s.agent_def_id = ?1 AND (?3 OR s.person = ?2) ORDER BY s.at DESC, s.session_id",
            )?;
            let rows = st.query_map(params![i, person, all], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?.collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await?;
    let mut listed = Vec::with_capacity(rows.len());
    let mut infos = Vec::new();
    for (session, version, target, at, experiment, variant) in rows {
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
                    experiment,
                    variant,
                });
                infos.push(info);
            }
            None => listed.push(Listed { id: session, version, at, runner: crate::harness::wait::machine(&target).to_owned(), state: "gone".to_owned(), engine: None, agent: None, experiment, variant }),
        }
    }
    let sessions = serde_json::to_value(&listed).map_err(|e| Error::Other(e.into()))?;
    Ok(json!({"sessions": sessions, "starts": starts(&infos)}))
}

/// Definition `id`'s sessions forgotten, as it goes.
pub fn forget_in(c: &rusqlite::Connection, id: &str) -> rusqlite::Result<usize> {
    c.execute("DELETE FROM agent_def_sessions WHERE agent_def_id = ?1", params![id])
}
