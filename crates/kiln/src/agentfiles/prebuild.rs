//! Agentfiles built ahead of their sessions (kiln plan 01a1dd2e6a0b76bbba478c532781feed P4.5): saving one of a person's
//! Agentfiles, or an agent definition that names one, asks each of their `microvm` runners to build it for that
//! runner's platform, exactly as a session there builds it as it starts. So the session that starts after the build is
//! done builds nothing: its machine's store holds its image's content already (`crate::sandbox::image::Pinned::built`,
//! said in its start's record as `built_us`).
//!
//! - **On which runners.** Its person's: each runner whose engine is `microvm` as the server has its facts
//!   (`crate::harness::engine::of`) and that its owner may start agents on (`runners.terminal` there,
//!   `crate::harness::lineage::may_run_on`), connected or not; outposts and one-job runners aside.
//! - **What is built.** Its text at its version now, once for each of kiln's harnesses a session of it runs: those it
//!   declares (`HARNESS <harness>`) and those of its person's agent definitions that name it, each beside kiln's harness
//!   directory for that harness as a session of it is built ([`source`], `crate::sandbox::isolated::source_of`), given
//!   the secrets its text names, read from its person's own (`crate::secrets::mine::for_build`, P4.7).
//! - **Where each build is** is kept in kiln.db's `prebuilds`, a row for each runner and harness ([`State`]): `queued` as
//!   a save asks, or `failed` where its secrets can't be read; then `building`, `done` (its image, pinned) or `failed`
//!   (Shards' words, or kiln's) as its runner says ([`Said`]). A save writes its rows anew; a runner's word moves the row
//!   of the version it built alone, and only forward ([`State::follows`]). Each change is told to the sockets of whoever
//!   hears of the Agentfile (`AgentfileChange::builds`).
//! - **The runner** names [`feature::PREBUILD`]. Its newest process is sent each build (`ServerMsg::Prebuild`), and
//!   asked again for those it hasn't said done or failed each time one of its processes names the feature (it
//!   connected again): one already made is found in its store at once. A runner that doesn't name it is asked nothing,
//!   and its row waits, queued.

use rusqlite::{OptionalExtension as _, params};
use serde::{Deserialize, Serialize};

use crate::exec::Uplink;
use crate::harness::engine::Engine;
use crate::hub::Hub;
use crate::instances::PeerId;
use crate::proto::{AgentMsg, ServerMsg, feature};
use crate::sandbox::image::Source;
use crate::secrets::mine::BuildSecret;
use crate::wire::events::{AgentfileChange, ServerEvent};

/// A build asked of a runner (`ServerMsg::Prebuild`): Agentfile `agentfile`'s `text` at `version`, for kiln's harness
/// `harness`, given `secrets`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ask {
    pub agentfile: String,
    pub version: i64,
    /// One of kiln's harnesses (`claude` ...): its directory is the one its build is given, as a session of it is.
    pub harness: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<BuildSecret>,
}

/// Where a build is (`prebuilds.state`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    /// Asked: its runner hasn't said it builds it yet (it's away, its word hasn't come, or it doesn't build ahead).
    Queued,
    Building,
    /// Its image is in its runner's store, which a session's start there finds.
    Done,
    /// Not made: Shards' words, or kiln's where it couldn't be asked.
    Failed,
}

impl State {
    fn as_str(self) -> &'static str {
        match self {
            State::Queued => "queued",
            State::Building => "building",
            State::Done => "done",
            State::Failed => "failed",
        }
    }

    fn of(s: &str) -> Option<State> {
        [State::Queued, State::Building, State::Done, State::Failed]
            .into_iter()
            .find(|st| st.as_str() == s)
    }

    /// The states a runner's word that a build is at `self` moves it from: queued to building, and either of them to its
    /// end. A word on a build already at its end (another build of it, asked again as its runner came back, ending
    /// second) moves nothing; only a save asks again.
    fn follows(self) -> &'static [State] {
        match self {
            State::Queued => &[],
            State::Building => &[State::Queued],
            State::Done | State::Failed => &[State::Queued, State::Building],
        }
    }
}

/// How a runner says a build went.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "lowercase")]
pub enum How {
    Building,
    /// Its image, pinned by its digest (`NAME@sha256:…`).
    Done {
        image: String,
    },
    /// Why it made none: Shards' words, or kiln's.
    Failed {
        why: String,
    },
}

/// A runner's word on a build it was asked (`AgentMsg::Prebuilt`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Said {
    pub agentfile: String,
    pub version: i64,
    pub harness: String,
    pub how: How,
}

/// One runner's build of an Agentfile for one of kiln's harnesses, as `agentfiles.builds` lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Build {
    pub runner: String,
    pub harness: String,
    /// The Agentfile's version it builds.
    pub version: i64,
    pub state: State,
    /// Its image, pinned by its digest, once done.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// Why it failed: Shards' words, or kiln's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
    /// When it last changed (Unix ms).
    pub at: i64,
}

/// Kiln's harnesses `text` is built for, in kiln's order: each it declares (`HARNESS <harness>`), and each of `defined`
/// (the harnesses of the agent definitions that name it). A text Shards can't read declares none.
pub fn harnesses(text: &str, defined: &[String]) -> Vec<&'static str> {
    let declared: Vec<String> = super::settings::of(text)
        .map(|s| s.harnesses.into_iter().map(|h| h.name).collect())
        .unwrap_or_default();
    let mut out: Vec<&'static str> = Vec::new();
    for b in super::builtin::ALL {
        if (declared.iter().any(|d| d == b.harness)
            || defined.iter().any(|d| d.trim() == b.harness))
            && !out.contains(&b.harness)
        {
            out.push(b.harness);
        }
    }
    out
}

/// What a build of `ask` is made from: its Agentfile, beside kiln's harness directory for its harness, as a session of
/// that harness is built from it (`crate::sandbox::isolated::source_of`), so both are one tag in their machine's store.
pub fn source(ask: &Ask) -> Source<'_> {
    let beside = super::builtin::ALL
        .iter()
        .find(|b| b.harness == ask.harness);
    Source::Kept {
        id: &ask.agentfile,
        text: &ask.text,
        beside,
    }
}

// ------------------------------------------------------------------------------ on the server

/// Agentfile `id` as it is now, whoever's; none where it's gone.
async fn current(hub: &Hub, id: &str) -> anyhow::Result<Option<super::Agentfile>> {
    let anyone = super::Who {
        person: String::new(),
        all: true,
    };
    match super::get(hub, hub.blobs.blobs.as_ref(), &anyone, id).await {
        Ok(a) => Ok(Some(a)),
        Err(super::Error::Missing) => Ok(None),
        Err(e) => Err(anyhow::anyhow!("Agentfile {id}: {e}")),
    }
}

/// The runners `person`'s Agentfiles are built on: each whose engine is `microvm`, as the server has its facts, that
/// `person` may start agents on, connected or not; outposts and one-job runners aside. None where `person` can't sign
/// in any more. Sorted.
async fn runners_of(hub: &Hub, person: &str) -> Vec<String> {
    let Some(caller) = crate::authz::gate::person_for(hub, person).await else {
        return Vec::new();
    };
    let mut out: Vec<String> = hub
        .agents
        .load()
        .values()
        .filter(|a| !a.outpost && !a.ephemeral)
        .filter(|a| {
            a.facts.as_deref().and_then(crate::harness::engine::of) == Some(Engine::Microvm)
        })
        .filter(|a| {
            crate::harness::lineage::may_run_on(
                &caller,
                &format!("agent:{}", a.name),
                "build an Agentfile there",
            )
            .is_ok()
        })
        .map(|a| a.name.clone())
        .collect();
    out.sort();
    out
}

/// Runner `runner`'s newest process, where it names [`feature::PREBUILD`].
fn taker(hub: &Hub, runner: &str) -> Option<PeerId> {
    let inst = hub.instances.load();
    inst.newest(runner).filter(|p| {
        inst.get(*p)
            .is_some_and(|i| feature::speaks(&i.features, feature::PREBUILD))
    })
}

/// `ask` sent to runner `runner`'s process `peer` (`ServerMsg::Prebuild`).
async fn send(hub: &Hub, runner: &str, peer: PeerId, ask: &Ask) {
    match serde_json::to_string(ask) {
        Ok(json) => hub
            .command_peers(runner, vec![peer], &ServerMsg::Prebuild { json })
            .await
            .log_when_answered(crate::hub::LOG_WITHIN),
        Err(e) => {
            tracing::warn!(target: "kiln.prebuild", runner, agentfile = %ask.agentfile, error = %e, "a build couldn't be written to be asked")
        }
    }
}

/// Tells the sockets of whoever hears of Agentfile `id` (its owner's, and those of whoever reaches everyone's) that its
/// builds changed.
fn changed(hub: &Hub, owner: &str, id: &str, name: &str, version: i64) {
    hub.emit(&ServerEvent::Agentfile(AgentfileChange {
        owner: owner.to_owned(),
        id: id.to_owned(),
        name: name.to_owned(),
        version,
        deleted: false,
        builds: true,
    }));
}

/// Agentfile `id`'s builds ahead of its sessions asked anew, after a save of it or of an agent definition that names
/// it: its rows written anew at its version now ([`queue_in`]), and each runner that's here and names the feature sent
/// its builds. What can't be done is warned of; the save it follows stands.
pub async fn queue(hub: &Hub, id: &str) {
    if let Err(e) = queue_in(hub, id).await {
        tracing::warn!(target: "kiln.prebuild", agentfile = id, error = %format!("{e:#}"), "its builds ahead of its sessions weren't asked");
    }
}

/// [`queue`]'s work: a row for each of its person's runners ([`runners_of`]) and each harness its sessions run
/// ([`harnesses`]), `queued`, or `failed` saying why where its secrets can't be read. A save that finds the Agentfile
/// changed again before it writes leaves the rows to the later save's.
async fn queue_in(hub: &Hub, id: &str) -> anyhow::Result<()> {
    let Some(kept) = current(hub, id).await? else {
        return Ok(());
    };
    let owner = crate::agentdefs::Who {
        person: kept.owner.clone(),
        all: false,
    };
    let named = format!("{}{id}", crate::sandbox::isolated::KEPT);
    let defined: Vec<String> = crate::agentdefs::list(hub, &owner, false)
        .await
        .map_err(|e| anyhow::anyhow!("its agent definitions: {e}"))?
        .into_iter()
        .filter(|d| d.agentfile.as_deref().map(str::trim) == Some(named.as_str()))
        .map(|d| d.harness)
        .collect();
    let harnesses = harnesses(&kept.text, &defined);
    let runners = runners_of(hub, &kept.owner).await;
    let secrets = crate::secrets::mine::for_build(hub, &kept.owner, &kept.text).await;
    let (state, why) = match &secrets {
        Ok(_) => (State::Queued, None),
        Err(w) => (State::Failed, Some(w.clone())),
    };
    let rows: Vec<(String, &'static str)> = runners
        .iter()
        .flat_map(|r| harnesses.iter().map(move |h| (r.clone(), *h)))
        .collect();
    let (i, version, now) = (id.to_owned(), kept.version, crate::util::now_ms());
    let written = hub
        .store
        .call(move |c| {
            let tx = crate::store::tx(c)?;
            let at: Option<i64> = tx.query_row("SELECT version FROM agentfiles WHERE id = ?1", params![i], |r| r.get(0)).optional()?;
            if at != Some(version) {
                return Ok(false);
            }
            tx.execute("DELETE FROM prebuilds WHERE agentfile_id = ?1", params![i])?;
            for (runner, harness) in &rows {
                tx.execute("INSERT INTO prebuilds (agentfile_id, runner, harness, version, state, why, at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)", params![i, runner, harness, version, state.as_str(), why, now])?;
            }
            tx.commit()?;
            Ok(true)
        })
        .await?;
    if !written {
        return Ok(());
    }
    changed(hub, &kept.owner, id, &kept.name, kept.version);
    let Ok(secrets) = secrets else { return Ok(()) };
    for runner in &runners {
        let Some(peer) = taker(hub, runner) else {
            continue;
        };
        for harness in &harnesses {
            send(
                hub,
                runner,
                peer,
                &Ask {
                    agentfile: id.to_owned(),
                    version: kept.version,
                    harness: (*harness).to_owned(),
                    text: kept.text.clone(),
                    secrets: secrets.clone(),
                },
            )
            .await;
        }
    }
    Ok(())
}

/// Runner `runner`'s word on a build it was asked (`AgentMsg::Prebuilt`): the row of the version it built moved to it
/// where it follows the row's state ([`State::follows`]), and whoever hears of the Agentfile told. A word on a version
/// saved over since, on a build it wasn't asked, or one this server can't read, moves nothing.
pub async fn heard(hub: &Hub, runner: &str, json: &str) {
    let said: Said = match serde_json::from_str(json) {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!(target: "kiln.prebuild", runner, error = %e, "a build's word this server couldn't read");
            return;
        }
    };
    let (state, image, why) = match said.how {
        How::Building => (State::Building, None, None),
        How::Done { image } => (State::Done, Some(image), None),
        How::Failed { why } => (State::Failed, None, Some(why)),
    };
    let from: Vec<&str> = state.follows().iter().map(|s| s.as_str()).collect();
    let (id, r, harness, version, now) = (
        said.agentfile.clone(),
        runner.to_owned(),
        said.harness,
        said.version,
        crate::util::now_ms(),
    );
    let moved = hub
        .store
        .call(move |c| {
            let from = serde_json::to_string(&from)?;
            let n = c.execute(
                "UPDATE prebuilds SET state = ?5, image = ?6, why = ?7, at = ?8 WHERE agentfile_id = ?1 AND runner = ?2 AND harness = ?3 AND version = ?4 AND state IN (SELECT value FROM json_each(?9))",
                params![id, r, harness, version, state.as_str(), image, why, now, from],
            )?;
            Ok(n > 0)
        })
        .await;
    match moved {
        Ok(true) => {
            let anyone = super::Who {
                person: String::new(),
                all: true,
            };
            if let Ok(a) = super::brief(hub, &anyone, &said.agentfile).await {
                changed(hub, &a.owner, &a.id, &a.name, a.version);
            }
        }
        Ok(false) => {}
        Err(e) => {
            tracing::warn!(target: "kiln.prebuild", runner, agentfile = %said.agentfile, error = %format!("{e:#}"), "a build's word wasn't kept")
        }
    }
}

/// A runner process named the features it speaks (`AgentMsg::Features`): where they hold [`feature::PREBUILD`], its
/// runner's newest process that does is asked again for each of its builds that isn't done or failed, at the version
/// its row names where that's its Agentfile's still.
pub async fn on_features(hub: &Hub, runner: &str, names: &[String]) {
    if !feature::speaks(names, feature::PREBUILD) {
        return;
    }
    let Some(peer) = taker(hub, runner) else {
        return;
    };
    let r = runner.to_owned();
    let rows = hub
        .store
        .read(move |c| {
            let mut st = c.prepare("SELECT agentfile_id, harness, version FROM prebuilds WHERE runner = ?1 AND state IN ('queued', 'building') ORDER BY agentfile_id, harness")?;
            let rows = st.query_map(params![r], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, i64>(2)?)))?.collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await;
    let rows = match rows {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(target: "kiln.prebuild", runner, error = %format!("{e:#}"), "its builds waiting weren't read: they're asked at their Agentfiles' next save");
            return;
        }
    };
    for (id, harness, version) in rows {
        let Ok(Some(kept)) = current(hub, &id).await else {
            continue;
        };
        if kept.version != version {
            continue;
        }
        let Ok(secrets) = crate::secrets::mine::for_build(hub, &kept.owner, &kept.text).await
        else {
            continue;
        };
        send(
            hub,
            runner,
            peer,
            &Ask {
                agentfile: id,
                version,
                harness,
                text: kept.text,
                secrets,
            },
        )
        .await;
    }
}

/// Agentfile `id`'s builds ahead of its sessions, for a caller who reaches it (another's is as none): each of its
/// rows on a runner kiln still knows, by runner and harness.
pub async fn builds(hub: &Hub, who: &super::Who, id: &str) -> super::R<Vec<Build>> {
    super::brief(hub, who, id).await?;
    let i = id.to_owned();
    let rows = hub
        .store
        .read(move |c| {
            let mut st = c.prepare("SELECT runner, harness, version, state, image, why, at FROM prebuilds WHERE agentfile_id = ?1 ORDER BY runner, harness")?;
            let rows = st
                .query_map(params![i], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?, r.get::<_, String>(3)?, r.get::<_, Option<String>>(4)?, r.get::<_, Option<String>>(5)?, r.get::<_, i64>(6)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await?;
    let known = hub.agents.load();
    Ok(rows
        .into_iter()
        .filter(|row| known.contains_key(&row.0))
        .filter_map(|(runner, harness, version, state, image, why, at)| {
            Some(Build {
                runner,
                harness,
                version,
                state: State::of(&state)?,
                image,
                why,
                at,
            })
        })
        .collect())
}

// ------------------------------------------------------------------------------ on the runner

/// [`ServerMsg::Prebuild`] on a runner: `ask`'s build, in the store its microVMs `vms` boot from, said building as it
/// begins and done or failed as it ends (`AgentMsg::Prebuilt`). One this runner can't read is let go, said in its log.
pub fn told(json: &str, up: Uplink, vms: Option<std::sync::Arc<crate::sandbox::Microvms>>) {
    let ask: Ask = match serde_json::from_str(json) {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!(target: "kiln.prebuild", error = %e, "a build asked ahead of its sessions this runner couldn't read");
            return;
        }
    };
    crate::rt::spawn(async move {
        say(&up, &ask, How::Building).await;
        let how = match build(vms.as_deref(), &ask).await {
            Ok(image) => How::Done { image },
            Err(why) => How::Failed { why },
        };
        say(&up, &ask, how).await;
    });
}

/// Says to the server how `ask`'s build went.
async fn say(up: &Uplink, ask: &Ask, how: How) {
    let said = Said {
        agentfile: ask.agentfile.clone(),
        version: ask.version,
        harness: ask.harness.clone(),
        how,
    };
    match serde_json::to_string(&said) {
        Ok(json) => up.msg(AgentMsg::Prebuilt { json }).await,
        Err(e) => {
            tracing::warn!(target: "kiln.prebuild", agentfile = %ask.agentfile, error = %e, "a build's word couldn't be written")
        }
    }
}

/// `ask`'s image in the store of microVMs `vms`, as a session of its harness gets its own there ([`source`]): pinned by its
/// digest, found there or built; else why not, in Shards' words or kiln's.
pub async fn build(vms: Option<&crate::sandbox::Microvms>, ask: &Ask) -> Result<String, String> {
    let vms = vms.ok_or_else(|| crate::sandbox::NO_MICROVM.to_owned())?;
    vms.images
        .pinned(source(ask), &ask.secrets)
        .await
        .map(|p| p.image)
        .map_err(|r| r.to_string())
}

#[cfg(test)]
mod tests;
