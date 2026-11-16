//! Which of a person's own agent definitions stands in for one of kiln's UI agents, for them alone
//! (kiln plan 01a1dd2e8618733a87f2793aab1322a8 P5.1b: "a person's copy may replace kiln's UI agent for
//! them"). A person names, for each of kiln's agents that's `replaceable` ([`super::builtin`]), one of
//! their own definitions, or none for kiln's own again; where kiln starts that agent for them, it starts
//! theirs: its harness, model and Agentfile, first told its instructions and then kiln's brief, which
//! carries what that start is about. Anyone else's start of it is kiln's own.

use std::collections::BTreeMap;

use rusqlite::params;

use super::sessions::Definition;
use super::{AgentDef, Error, R, Settings, Who, builtin};
use crate::hub::Hub;

/// Each of kiln's agents `person` has their own stand in for, and which (`kiln:<name>` → its id).
pub async fn of(hub: &Hub, person: &str) -> R<BTreeMap<String, String>> {
    let p = person.to_owned();
    let rows: Vec<(String, String)> = hub
        .store
        .read(move |c| {
            let mut st = c.prepare("SELECT builtin, agent_def_id FROM agent_def_uses WHERE person = ?1")?;
            let rows = st.query_map(params![p], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await?;
    Ok(rows.into_iter().collect())
}

/// `who`'s own definition `id` stands in for kiln's `kiln_agent` for them; with none, kiln's own again.
/// Only a `replaceable` one of kiln's, and only one of their own (another's, even one they reach, is as
/// none): what they have now.
pub async fn set(hub: &Hub, who: &Who, kiln_agent: &str, id: Option<&str>) -> R<BTreeMap<String, String>> {
    let Some(b) = builtin::get(kiln_agent) else {
        return Err(Error::Bad(format!("{kiln_agent} isn't one of kiln's agent definitions (`kiln:<name>`, as agentdefs.list's `builtin` names them)")));
    };
    if !b.replaceable {
        let these: Vec<String> = builtin::all().into_iter().filter(|b| b.replaceable).map(|b| b.id).collect();
        return Err(Error::Bad(format!("{kiln_agent} isn't one kiln starts for you that yours can stand in for: {}", these.join(", "))));
    }
    let (person, k, now) = (who.person.clone(), b.id.clone(), crate::util::now_unix());
    match id {
        Some(id) => {
            let mine = Who { person: who.person.clone(), all: false };
            let d = super::get(hub, &mine, id).await?;
            hub.store
                .call(move |c| {
                    c.execute(
                        "INSERT INTO agent_def_uses (person, builtin, agent_def_id, at) VALUES (?1, ?2, ?3, ?4) ON CONFLICT (person, builtin) DO UPDATE SET agent_def_id = excluded.agent_def_id, at = excluded.at",
                        params![person, k, d.id, now],
                    )?;
                    Ok(())
                })
                .await?;
        }
        None => {
            hub.store
                .call(move |c| {
                    c.execute("DELETE FROM agent_def_uses WHERE person = ?1 AND builtin = ?2", params![person, k])?;
                    Ok(())
                })
                .await?;
        }
    }
    of(hub, &who.person).await
}

/// What kiln starts for `person` where it would start its own `kiln_agent`: their own definition where
/// one stands in for it, else none (kiln's own).
async fn stand_in(hub: &Hub, person: &str, kiln_agent: &str) -> R<Option<AgentDef>> {
    let Some(id) = of(hub, person).await?.remove(kiln_agent) else { return Ok(None) };
    let mine = Who { person: person.to_owned(), all: false };
    Ok(Some(super::get(hub, &mine, &id).await?))
}

/// What kiln starts for `person` (none: no person) as its own `kiln_agent`: their definition standing in
/// for it, else kiln's own; and which it is, for the sessions it starts to say (`super::sessions`).
pub async fn or_kilns(hub: &Hub, person: Option<&str>, kiln_agent: &str) -> R<(Settings, Definition)> {
    if let Some(p) = person
        && let Some(d) = stand_in(hub, p, kiln_agent).await?
    {
        return Ok((d.settings, Definition { id: d.id, version: d.version }));
    }
    let b = builtin::get(kiln_agent).ok_or_else(|| Error::Bad(format!("{kiln_agent} isn't one of kiln's agent definitions")))?;
    Ok((b.settings, Definition::kilns(&b.id)))
}

/// `ask`, kiln's start of its `kiln_agent` for `person` (its brief its prompt), as `person`'s own
/// definition standing in for it makes it: that one's harness, model and Agentfile, first told its
/// instructions and then kiln's brief, its mode `mode_for` its harness says. As it was where none does.
/// Either way it says which definition its session started from (`Ask::definition`): theirs, or kiln's.
pub async fn ask_for(hub: &Hub, person: Option<&str>, kiln_agent: &str, mut ask: crate::harness::lineage::Ask, mode_for: fn(&str) -> Option<String>) -> R<crate::harness::lineage::Ask> {
    let stood_in = match person {
        Some(person) => stand_in(hub, person, kiln_agent).await?,
        None => None,
    };
    let Some(d) = stood_in else {
        ask.definition = Some(Definition::kilns(kiln_agent));
        return Ok(ask);
    };
    let s = d.settings;
    ask.prompt = super::first_message(&s, ask.prompt.as_deref());
    ask.mode = mode_for(&s.harness);
    ask.kind = s.harness;
    ask.model = s.model;
    ask.image = s.agentfile;
    ask.definition = Some(Definition { id: d.id, version: d.version });
    Ok(ask)
}
