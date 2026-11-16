//! An agent definition and its Agentfile's `AGENT` kept consistent both ways (kiln plan
//! 01a1dd2e8618733a87f2793aab1322a8 P5.4). A definition that names one of its owner's Agentfiles
//! (`agentfile:<id>`) declaring an `AGENT` of the definition's name shares its model, tools, skills and
//! remote MCP servers with it (`crate::agentfiles::settings::AgentSettings`): an MCP server a command
//! starts is the definition's alone, and one the Agentfile fetches from another source its image's. A
//! change to either side is written into the other: a definition's into a new version of its Agentfile's
//! text, written first; an Agentfile's into each of its owner's definitions it declares, written after.
//! Each shared setting is merged three ways from what the changed side said before: a setting only one
//! side changed since takes that side's, one both changed alike is kept, and one both changed differently
//! refuses the change whole, naming it and each side's value ([`Diverged`]), so neither is overwritten
//! unseen. Every write is held to the version it was made from (`base`): one another change came before
//! is refused too.

use kiln_seams::blobs::Blobs;
use serde::Serialize;
use serde_json::{Value, json};

use super::{AgentDef, Error, Mcp, Patch, R, Settings, Who};
use crate::agentfiles::settings::{self, AgentSettings, Remote};
use crate::hub::Hub;

/// A setting an agent definition and its Agentfile's `AGENT` each changed differently since they last
/// agreed: which agent and setting, and what each side says now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Diverged {
    pub agent: String,
    pub setting: &'static str,
    pub definition: Value,
    pub agentfile: Value,
}

/// Each of `d` in words, for the refusal that names them.
pub fn words(d: &[Diverged]) -> String {
    let each: Vec<String> = d
        .iter()
        .map(|d| {
            format!(
                "{}'s {}: the agent definition says {}, its Agentfile's AGENT {}",
                d.agent, d.setting, d.definition, d.agentfile
            )
        })
        .collect();
    format!(
        "the agent definition and its Agentfile's AGENT each changed a setting differently since they last agreed ({}): make them agree in one of them, then save it again",
        each.join("; ")
    )
}

/// What a definition's settings share with its Agentfile's `AGENT`.
pub fn shared(s: &Settings) -> AgentSettings {
    let mcp = s
        .mcp
        .iter()
        .filter_map(|m| {
            Some(Remote {
                name: m.name.clone(),
                url: m.url.clone()?,
            })
        })
        .collect();
    AgentSettings {
        model: s.model.clone(),
        tools: s.tools.clone(),
        skills: s.skills.clone(),
        mcp,
    }
}

/// `s` saying what `m` says of what it shares: its own MCP servers a command starts kept after the
/// remote ones; `s` as it is where it says that already.
fn with(s: &Settings, m: &AgentSettings) -> Settings {
    if shared(s) == *m {
        return s.clone();
    }
    let remote = m.mcp.iter().map(|r| Mcp {
        name: r.name.clone(),
        url: Some(r.url.clone()),
        command: Vec::new(),
    });
    let mcp = remote
        .chain(s.mcp.iter().filter(|x| x.url.is_none()).cloned())
        .collect();
    Settings {
        model: m.model.clone(),
        tools: m.tools.clone(),
        skills: m.skills.clone(),
        mcp,
        ..s.clone()
    }
}

/// `p` setting what `s` says of the settings a definition shares.
fn patched(mut p: Patch, s: &Settings) -> Patch {
    p.model = Some(s.model.clone());
    p.tools = Some(s.tools.clone());
    p.skills = Some(s.skills.clone());
    p.mcp = Some(s.mcp.clone());
    p
}

/// Which side a change is of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Changed {
    Definition,
    Agentfile,
}

/// One setting merged: `base`, what the changed side said before; `ours`, what it says now; `theirs`,
/// what the other side says. None where both changed it, differently.
fn pick<T: Clone + PartialEq>(base: &T, ours: &T, theirs: &T) -> Option<T> {
    if ours == base || ours == theirs {
        Some(theirs.clone())
    } else if theirs == base {
        Some(ours.clone())
    } else {
        None
    }
}

/// Agent `agent`'s shared settings merged three ways ([`pick`]), each changed differently on both sides
/// named.
fn merge(
    agent: &str,
    base: &AgentSettings,
    ours: &AgentSettings,
    theirs: &AgentSettings,
    changed: Changed,
) -> Result<AgentSettings, Vec<Diverged>> {
    let mut diverged = Vec::new();
    let mut note = |setting: &'static str, o: Value, t: Value| {
        let (definition, agentfile) = if changed == Changed::Definition {
            (o, t)
        } else {
            (t, o)
        };
        diverged.push(Diverged {
            agent: agent.to_owned(),
            setting,
            definition,
            agentfile,
        });
    };
    let model = pick(&base.model, &ours.model, &theirs.model);
    if model.is_none() {
        note("model", json!(ours.model), json!(theirs.model));
    }
    let tools = pick(&base.tools, &ours.tools, &theirs.tools);
    if tools.is_none() {
        note("tools", json!(ours.tools), json!(theirs.tools));
    }
    let skills = pick(&base.skills, &ours.skills, &theirs.skills);
    if skills.is_none() {
        note("skills", json!(ours.skills), json!(theirs.skills));
    }
    let remotes = |m: &[Remote]| {
        m.iter()
            .map(|r| json!({"name": r.name, "url": r.url}))
            .collect::<Vec<_>>()
    };
    let mcp = pick(&base.mcp, &ours.mcp, &theirs.mcp);
    if mcp.is_none() {
        note(
            "mcp",
            json!(remotes(&ours.mcp)),
            json!(remotes(&theirs.mcp)),
        );
    }
    match (model, tools, skills, mcp) {
        (Some(model), Some(tools), Some(skills), Some(mcp)) => Ok(AgentSettings {
            model,
            tools,
            skills,
            mcp,
        }),
        _ => Err(diverged),
    }
}

/// A definition's Agentfile, where it names one of its owner's that declares an `AGENT` of its name: the
/// Agentfile, and what it says of that agent.
struct Linked {
    file: crate::agentfiles::Agentfile,
    agent: AgentSettings,
}

fn kept_id(s: &Settings) -> Option<&str> {
    s.agentfile
        .as_deref()
        .and_then(|a| a.trim().strip_prefix(crate::sandbox::isolated::KEPT))
}

async fn linked(hub: &Hub, owner: &str, s: &Settings, name: &str) -> R<Option<Linked>> {
    let Some(id) = kept_id(s) else {
        return Ok(None);
    };
    let who = crate::agentfiles::Who {
        person: owner.to_owned(),
        all: false,
    };
    let file = match crate::agentfiles::get(hub, hub.blobs.blobs.as_ref(), &who, id).await {
        Ok(f) => f,
        // One it doesn't reach is the definition's check's to refuse.
        Err(crate::agentfiles::Error::Missing) => return Ok(None),
        Err(e) => {
            return Err(Error::Unavailable(format!(
                "its Agentfile couldn't be read: {e}"
            )));
        }
    };
    let read = settings::of(&file.text)
        .map_err(|e| Error::Bad(format!("its Agentfile {}: {e}", file.name)))?;
    Ok(read.agents.iter().find(|a| a.name == name).map(|a| Linked {
        agent: AgentSettings::from(a),
        file,
    }))
}

fn from_agentfile(e: crate::agentfiles::Error) -> Error {
    use crate::agentfiles::Error as A;
    match e {
        A::Diverged(d) => Error::Diverged(d),
        A::Conflict(m) => Error::Conflict(format!("its Agentfile: {m}")),
        A::Refused(m) => Error::Refused(vec![format!(
            "its Agentfile would then be refused by Shards: {m}"
        )]),
        A::Forbidden(m) => Error::Forbidden(m),
        A::Store(m) => Error::Unavailable(m),
        A::Other(e) => Error::Other(e),
        e @ (A::Bad(_) | A::Missing | A::NoRepo(_) | A::Agent { .. }) => {
            Error::Bad(format!("its Agentfile: {e}"))
        }
    }
}

/// What definition `name`, `owner`'s, saying `s` (and before it `was`, where it shared with this same
/// `AGENT`), shares with its Agentfile's `AGENT` once both agree, its Agentfile written first to say it,
/// over the version read; the definition as it would be is checked before, so one kiln can't run changes
/// neither. None where it names no Agentfile declaring it.
async fn into_agentfile(
    hub: &Hub,
    owner: &str,
    name: &str,
    s: &Settings,
    was: Option<AgentSettings>,
    by: &str,
) -> R<Option<AgentSettings>> {
    let Some(l) = linked(hub, owner, s, name).await? else {
        return Ok(None);
    };
    let merged = merge(
        name,
        &was.unwrap_or_default(),
        &shared(s),
        &l.agent,
        Changed::Definition,
    )
    .map_err(Error::Diverged)?;
    let why = super::check(hub, owner, &with(s, &merged)).await?;
    if !why.is_empty() {
        return Err(Error::Refused(why));
    }
    if merged != l.agent {
        let text = settings::set_agent(&l.file.text, name, &merged)
            .map_err(|e| Error::Bad(format!("its Agentfile {}: {e}", l.file.name)))?;
        let who = crate::agentfiles::Who {
            person: owner.to_owned(),
            all: false,
        };
        let patch = crate::agentfiles::Patch {
            name: None,
            text: Some(text),
        };
        crate::agentfiles::update(
            hub,
            hub.blobs.blobs.as_ref(),
            &who,
            &l.file.id,
            patch,
            Some(l.file.version),
            by,
        )
        .await
        .map_err(from_agentfile)?;
    }
    Ok(Some(merged))
}

/// A new definition (`super::create`), its Agentfile's `AGENT` of its name made to say what they share
/// first, and it taking what only its Agentfile says.
pub async fn create(hub: &Hub, who: &Who, name: &str, settings: Settings, by: &str) -> R<AgentDef> {
    let n = name.trim();
    if let Some(p) = super::name_problem(n) {
        return Err(Error::Bad(p));
    }
    let settings = match into_agentfile(hub, &who.person, n, &settings, None, by).await? {
        Some(m) => with(&settings, &m),
        None => settings,
    };
    super::create(hub, who, n, settings, by).await
}

/// Definition `id` changed as `patch` says, over `base` (`super::update`), and its Agentfile's `AGENT` of
/// its name made to say what they share, first.
pub async fn update(
    hub: &Hub,
    who: &Who,
    id: &str,
    patch: Patch,
    base: Option<i64>,
    by: &str,
) -> R<AgentDef> {
    let before = super::get(hub, who, id).await?;
    if let Some(b) = base.filter(|b| *b != before.version) {
        return Err(Error::Stale {
            base: b,
            current: Box::new(before),
        });
    }
    let name = patch
        .name
        .as_deref()
        .map_or_else(|| before.name.clone(), |n| n.trim().to_owned());
    let after = patch.clone().over(before.settings.clone());
    // What it shared before, where it shared it with this same AGENT: the merge's base.
    let was = (before.settings.agentfile == after.agentfile && before.name == name)
        .then(|| shared(&before.settings));
    let patch = match into_agentfile(hub, &before.owner, &name, &after, was, by).await? {
        Some(m) => patched(patch, &with(&after, &m)),
        None => patch,
    };
    super::update(hub, who, id, patch, Some(before.version), by).await
}

/// Agentfile `id` changed as `patch` says, over `base` (`crate::agentfiles::update`), and each of its
/// owner's definitions that names it and that its new text declares made to say what they share: the text
/// first takes what only a definition changed since, then it's kept, then each definition is written over
/// the version read (one that another change moved on says so, [`crate::agentfiles::Synced::refused`]).
pub async fn agentfile_update(
    hub: &Hub,
    blobs: &dyn Blobs,
    who: &crate::agentfiles::Who,
    id: &str,
    patch: crate::agentfiles::Patch,
    base: Option<i64>,
    by: &str,
) -> crate::agentfiles::R<crate::agentfiles::Saved> {
    use crate::agentfiles as af;
    let before = af::get(hub, blobs, who, id).await?;
    let stale = base.is_some_and(|b| b != before.version);
    // A rename alone, a change over a version another moved on from, or a text Shards can't read: the
    // update says so, as it would without its definitions.
    let Some(Ok(now)) = patch.text.as_deref().map(settings::of).filter(|_| !stale) else {
        return af::update(hub, blobs, who, id, patch, base, by).await;
    };
    let owner = Who {
        person: before.owner.clone(),
        all: false,
    };
    let defs = naming(hub, &owner, id)
        .await
        .map_err(|e| af::Error::Store(format!("its agent definitions couldn't be read: {e}")))?;
    let was = settings::of(&before.text).ok();
    let (mut text, mut writes, mut diverged) = (
        patch.text.clone().unwrap_or_default(),
        Vec::new(),
        Vec::new(),
    );
    for d in defs {
        let Some(ours) = now
            .agents
            .iter()
            .find(|a| a.name == d.name)
            .map(AgentSettings::from)
        else {
            continue;
        };
        let base_s = was
            .as_ref()
            .and_then(|w| w.agents.iter().find(|a| a.name == d.name))
            .map(AgentSettings::from)
            .unwrap_or_default();
        let theirs = shared(&d.settings);
        let m = match merge(&d.name, &base_s, &ours, &theirs, Changed::Agentfile) {
            Ok(m) => m,
            Err(mut found) => {
                diverged.append(&mut found);
                continue;
            }
        };
        if m != ours {
            text = settings::set_agent(&text, &d.name, &m)
                .map_err(|e| af::Error::Bad(e.to_string()))?;
        }
        if m != theirs {
            let s = with(&d.settings, &m);
            let why = super::check(hub, &d.owner, &s)
                .await
                .map_err(|e| af::Error::Store(e.to_string()))?;
            if !why.is_empty() {
                return Err(af::Error::Agent { agent: d.name, why });
            }
            writes.push((d, s));
        }
    }
    if !diverged.is_empty() {
        return Err(af::Error::Diverged(diverged));
    }
    let mut saved = af::update(
        hub,
        blobs,
        who,
        id,
        af::Patch {
            name: patch.name,
            text: Some(text),
        },
        Some(before.version),
        by,
    )
    .await?;
    for (d, s) in writes {
        saved.synced.push(
            match super::update(
                hub,
                &owner,
                &d.id,
                patched(Patch::default(), &s),
                Some(d.version),
                by,
            )
            .await
            {
                Ok(n) => af::Synced {
                    id: n.id,
                    name: n.name,
                    version: n.version,
                    refused: None,
                },
                Err(e) => af::Synced {
                    id: d.id,
                    name: d.name,
                    version: d.version,
                    refused: Some(e.to_string()),
                },
            },
        );
    }
    Ok(saved)
}

/// `owner`'s definitions that name Agentfile `id`.
async fn naming(hub: &Hub, owner: &Who, id: &str) -> R<Vec<AgentDef>> {
    let named = format!("{}{id}", crate::sandbox::isolated::KEPT);
    let mut out = Vec::new();
    for b in super::list(hub, owner, false).await? {
        if b.agentfile.as_deref().map(str::trim) == Some(named.as_str()) {
            out.push(super::get(hub, owner, &b.id).await?);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests;
