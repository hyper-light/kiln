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
//!   of the version it built alone, and only forward ([`State::follows`]). Each change of its rows is told once, as a
//!   frame of its own (`ServerEvent::AgentfileBuilds`), to the sockets of whoever hears of the Agentfile: the save's own
//!   frame (`ServerEvent::Agentfile`) tells its version.
//! - **Built again.** Whoever reaches it asks one runner's builds of it again (`agentfiles.rebuild`, [`rebuild`]) as a
//!   save asks them, at its version now: each that ended, done or failed, is `queued` again ([`State::ended`]). Refused,
//!   nothing written ([`Refusal`]), where that runner builds none of its person's Agentfiles or its kiln builds nothing
//!   ahead, where none of its sessions runs the harness named, or where one of those builds is underway.
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
use crate::wire::events::{AgentfileBuilds, ServerEvent};

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
    /// second) moves nothing; only a save, or a person's ask to build it again ([`State::ended`]), asks again.
    fn follows(self) -> &'static [State] {
        match self {
            State::Queued => &[],
            State::Building => &[State::Queued],
            State::Done | State::Failed => &[State::Queued, State::Building],
        }
    }

    /// Whether a build at `self` has ended, so a person's ask moves it back to queued ([`rebuild`]). One queued or
    /// building is underway until its runner's word ends it: asked twice at one version, its runner's first word to end
    /// it would stand over the second's ([`State::follows`]).
    fn ended(self) -> bool {
        matches!(self, State::Done | State::Failed)
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

/// Why a runner's builds of an Agentfile aren't asked again ([`rebuild`]): nothing is written. Each runner by the name
/// people see.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    /// Kiln knows no runner of that name that builds Agentfiles: none, or an outpost or a one-job runner.
    #[error(
        "no runner {0} builds Agentfiles ahead of their sessions: kiln knows none of that name, and outposts and one-job runners build none"
    )]
    NoRunner(String),
    /// Its engine isn't `microvm`, as the server has its facts.
    #[error(
        "{0} builds no Agentfile: its sessions run on the machine, from no image (its engine isn't microvm)"
    )]
    NotMicrovm(String),
    /// The Agentfile's person may not start agents there (`runners.terminal`), or can't sign in any more: kiln's words.
    #[error("{0}")]
    NotTheirs(String),
    /// Its newest process said what it speaks, without [`feature::PREBUILD`].
    #[error("{0}'s kiln builds no Agentfile ahead of its sessions: upgrade it (System → Updates)")]
    Older(String),
    /// None of its sessions runs the harness named (any of kiln's, where none is).
    #[error("{}", no_harness(.0.as_deref()))]
    NoHarness(Option<String>),
    /// Its build on `runner` for `harness` is queued or building ([`State::ended`]).
    #[error("its build on {runner} for {harness} is {} already: it can be built again once its runner says it's done or failed", .state.as_str())]
    Underway {
        runner: String,
        harness: String,
        state: State,
    },
}

/// [`Refusal::NoHarness`]'s words.
fn no_harness(harness: Option<&str>) -> String {
    match harness {
        Some(h) => format!(
            "none of its sessions runs {h}: it declares no HARNESS {h}, and none of its person's agent definitions that name it runs {h}"
        ),
        None => "none of its sessions runs one of kiln's harnesses: it declares no HARNESS, and no agent definition names it, so none of it is built ahead".to_owned(),
    }
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

/// Whether `caller`'s Agentfiles are built on runner `a`: its engine is `microvm`, as the server has its facts, and
/// `caller` may start agents there; outposts and one-job runners build none. Else why not.
fn builds_for(caller: &crate::authz::Caller, a: &crate::hub::AgentLive) -> Result<(), Refusal> {
    if a.outpost || a.ephemeral {
        return Err(Refusal::NoRunner(a.shown().to_owned()));
    }
    if a.facts.as_deref().and_then(crate::harness::engine::of) != Some(Engine::Microvm) {
        return Err(Refusal::NotMicrovm(a.shown().to_owned()));
    }
    crate::harness::lineage::may_run_on(
        caller,
        &format!("agent:{}", a.name),
        "build an Agentfile there",
    )
    .map_err(|(_, why)| Refusal::NotTheirs(why))
}

/// The runners `person`'s Agentfiles are built on ([`builds_for`]), connected or not. None where `person` can't sign in
/// any more. Sorted.
async fn runners_of(hub: &Hub, person: &str) -> Vec<String> {
    let Some(caller) = crate::authz::gate::person_for(hub, person).await else {
        return Vec::new();
    };
    let mut out: Vec<String> = hub
        .agents
        .load()
        .values()
        .filter(|a| builds_for(&caller, a).is_ok())
        .map(|a| a.name.clone())
        .collect();
    out.sort();
    out
}

/// How a runner takes builds now, as its newest process says ([`takes`]).
enum Takes {
    /// It names [`feature::PREBUILD`]: each is sent to it at once.
    Now(PeerId),
    /// It's away, or hasn't said what it speaks (yet, or ever: a kiln from before named features): each waits, queued,
    /// until one of its processes names the feature ([`on_features`]).
    Later,
    /// It said what it speaks, without the feature: its kiln builds nothing ahead.
    Never,
}

/// How runner `runner` takes builds now. A process that has said what it speaks named every feature of its build, so
/// one that named none hasn't said yet.
fn takes(hub: &Hub, runner: &str) -> Takes {
    let inst = hub.instances.load();
    let Some((peer, said)) = inst
        .newest(runner)
        .and_then(|p| inst.get(p).map(|i| (p, &i.features)))
    else {
        return Takes::Later;
    };
    if feature::speaks(said, feature::PREBUILD) {
        Takes::Now(peer)
    } else if said.is_empty() {
        Takes::Later
    } else {
        Takes::Never
    }
}

/// Runner `runner`'s newest process, where it names [`feature::PREBUILD`].
fn taker(hub: &Hub, runner: &str) -> Option<PeerId> {
    match takes(hub, runner) {
        Takes::Now(peer) => Some(peer),
        Takes::Later | Takes::Never => None,
    }
}

/// Whether runner `runner` builds `person`'s Agentfiles ([`builds_for`]) and its kiln builds them ahead, or may (it's
/// away, or hasn't said yet): else why not.
async fn builder(hub: &Hub, person: &str, runner: &str) -> Result<(), Refusal> {
    let caller = crate::authz::gate::person_for(hub, person)
        .await
        .ok_or_else(|| {
            Refusal::NotTheirs(format!(
                "{person} can't sign in here any more: kiln builds none of their Agentfiles"
            ))
        })?;
    let a = hub
        .agents
        .load()
        .get(runner)
        .cloned()
        .ok_or_else(|| Refusal::NoRunner(runner.to_owned()))?;
    builds_for(&caller, &a)?;
    match takes(hub, runner) {
        Takes::Never => Err(Refusal::Older(a.shown().to_owned())),
        Takes::Now(_) | Takes::Later => Ok(()),
    }
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
fn changed(hub: &Hub, owner: &str, id: &str) {
    hub.emit(&ServerEvent::AgentfileBuilds(AgentfileBuilds {
        owner: owner.to_owned(),
        id: id.to_owned(),
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

/// What [`queue_in`]'s write found.
enum Queued {
    /// The Agentfile was saved again before it: the later save's rows stand.
    SavedOver,
    /// It had no rows and has none: nothing changed to tell, nothing to send.
    Unchanged,
    /// Its rows were written anew.
    Written,
}

/// [`queue`]'s work: a row for each of its person's runners ([`runners_of`]) and each harness its sessions run
/// ([`harnesses`]), `queued`, or `failed` saying why where its secrets can't be read. A save that finds the Agentfile
/// changed again before it writes leaves the rows to the later save's. Whoever hears of the Agentfile is told its builds
/// changed only where its rows did, as [`heard`] tells a runner's word.
async fn queue_in(hub: &Hub, id: &str) -> anyhow::Result<()> {
    let Some(kept) = current(hub, id).await? else {
        return Ok(());
    };
    let asking = Asking::of(hub, kept).await?;
    let runners = runners_of(hub, &asking.kept.owner).await;
    let (state, why) = asking.first();
    let rows: Vec<(String, &'static str)> = runners
        .iter()
        .flat_map(|r| asking.harnesses.iter().map(move |h| (r.clone(), *h)))
        .collect();
    let (i, version, now) = (id.to_owned(), asking.kept.version, crate::util::now_ms());
    let written = hub
        .store
        .call(move |c| {
            let tx = crate::store::tx(c)?;
            let at: Option<i64> = tx.query_row("SELECT version FROM agentfiles WHERE id = ?1", params![i], |r| r.get(0)).optional()?;
            if at != Some(version) {
                return Ok(Queued::SavedOver);
            }
            let gone = tx.execute("DELETE FROM prebuilds WHERE agentfile_id = ?1", params![i])?;
            for (runner, harness) in &rows {
                tx.execute("INSERT INTO prebuilds (agentfile_id, runner, harness, version, state, why, at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)", params![i, runner, harness, version, state.as_str(), why, now])?;
            }
            tx.commit()?;
            Ok(if gone == 0 && rows.is_empty() { Queued::Unchanged } else { Queued::Written })
        })
        .await?;
    match written {
        Queued::SavedOver | Queued::Unchanged => return Ok(()),
        Queued::Written => {}
    }
    let kept = &asking.kept;
    changed(hub, &kept.owner, id);
    for runner in &runners {
        asking.send_to(hub, runner, &asking.harnesses).await;
    }
    Ok(())
}

/// What a save, or a person's ask to build again, asks of runners: Agentfile `kept` at its version now, kiln's harnesses
/// its sessions run ([`harnesses`]: those it declares, and its person's agent definitions' that name it), and the
/// secrets of its person's it names, or why they can't be read.
struct Asking {
    kept: super::Agentfile,
    harnesses: Vec<&'static str>,
    secrets: Result<Vec<BuildSecret>, String>,
}

impl Asking {
    async fn of(hub: &Hub, kept: super::Agentfile) -> anyhow::Result<Asking> {
        let owner = crate::agentdefs::Who {
            person: kept.owner.clone(),
            all: false,
        };
        let named = format!("{}{}", crate::sandbox::isolated::KEPT, kept.id);
        let defined: Vec<String> = crate::agentdefs::list(hub, &owner, false)
            .await
            .map_err(|e| anyhow::anyhow!("its agent definitions: {e}"))?
            .into_iter()
            .filter(|d| d.agentfile.as_deref().map(str::trim) == Some(named.as_str()))
            .map(|d| d.harness)
            .collect();
        let harnesses = harnesses(&kept.text, &defined);
        let secrets = crate::secrets::mine::for_build(hub, &kept.owner, &kept.text).await;
        Ok(Asking {
            kept,
            harnesses,
            secrets,
        })
    }

    /// Where its builds start: `queued`, or `failed` saying why its secrets can't be read (and none is sent).
    fn first(&self) -> (State, Option<String>) {
        match &self.secrets {
            Ok(_) => (State::Queued, None),
            Err(why) => (State::Failed, Some(why.clone())),
        }
    }

    /// Its builds for `harnesses` sent to runner `runner`, where it takes them now ([`taker`]) and its secrets were read.
    async fn send_to(&self, hub: &Hub, runner: &str, harnesses: &[&'static str]) {
        let (Ok(secrets), Some(peer)) = (&self.secrets, taker(hub, runner)) else {
            return;
        };
        for harness in harnesses {
            send(
                hub,
                runner,
                peer,
                &Ask {
                    agentfile: self.kept.id.clone(),
                    version: self.kept.version,
                    harness: (*harness).to_owned(),
                    text: self.kept.text.clone(),
                    secrets: secrets.clone(),
                },
            )
            .await;
        }
    }
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
                changed(hub, &a.owner, &a.id);
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

/// Agentfile `id`'s builds on runner `runner` asked again (`agentfiles.rebuild`), for a caller who reaches it (another's
/// is as none), as [`queue_in`] asks a save's: for `harness` where named, else for each harness its sessions run, each
/// written anew at its version now, `queued` (`failed` where its secrets can't be read), and sent to the runner where it
/// takes them now ([`Takes`]). Answers its builds then, as [`builds`] reads them, or why that runner's aren't asked
/// again ([`Refusal`]), nothing written. A save between the two leaves its own asks standing.
pub async fn rebuild(
    hub: &Hub,
    who: &super::Who,
    id: &str,
    runner: &str,
    harness: Option<&str>,
) -> super::R<Result<Vec<Build>, Refusal>> {
    let kept = super::get(hub, hub.blobs.blobs.as_ref(), who, id).await?;
    if let Err(r) = builder(hub, &kept.owner, runner).await {
        return Ok(Err(r));
    }
    let asking = Asking::of(hub, kept).await?;
    let asked: Vec<&'static str> = asking
        .harnesses
        .iter()
        .copied()
        .filter(|h| harness.is_none_or(|named| named == *h))
        .collect();
    if asked.is_empty() {
        return Ok(Err(Refusal::NoHarness(harness.map(str::to_owned))));
    }
    let (state, why) = asking.first();
    let (i, r, shown, hs) = (
        id.to_owned(),
        runner.to_owned(),
        hub.shown(runner),
        asked.clone(),
    );
    let (version, now) = (asking.kept.version, crate::util::now_ms());
    let written = hub
        .store
        .call(move |c| {
            let tx = crate::store::tx(c)?;
            let kept_at: Option<i64> = tx.query_row("SELECT version FROM agentfiles WHERE id = ?1", params![i], |row| row.get(0)).optional()?;
            if kept_at != Some(version) {
                return Ok(Ok(false));
            }
            for h in &hs {
                let was: Option<(i64, String)> = tx.query_row("SELECT version, state FROM prebuilds WHERE agentfile_id = ?1 AND runner = ?2 AND harness = ?3", params![i, r, h], |row| Ok((row.get(0)?, row.get(1)?))).optional()?;
                let underway = was.filter(|(v, _)| *v == version).and_then(|(_, s)| State::of(&s)).filter(|s| !s.ended());
                if let Some(at) = underway {
                    return Ok(Err(Refusal::Underway { runner: shown, harness: (*h).to_owned(), state: at }));
                }
            }
            for h in &hs {
                tx.execute("INSERT OR REPLACE INTO prebuilds (agentfile_id, runner, harness, version, state, why, at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)", params![i, r, h, version, state.as_str(), why, now])?;
            }
            tx.commit()?;
            Ok(Ok(true))
        })
        .await?;
    match written {
        Err(underway) => return Ok(Err(underway)),
        Ok(true) => {
            changed(hub, &asking.kept.owner, id);
            asking.send_to(hub, runner, &asked).await;
        }
        Ok(false) => {}
    }
    builds(hub, who, id).await.map(Ok)
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
