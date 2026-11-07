//! The conflict ladder (docs/ci.md W26-R17), the server's part. A conflicted replay (W26-R12's,
//! kept by `conflicts`) goes up `kiln_ci::ladder`'s rungs: what each rung makes of a conflict is a
//! candidate revision committed through `revision::commit` (as kiln, or as the agent that made it:
//! the path W27-R03 signs), kept at `refs/kiln/resolve/<diff>/<resolution>`, the conflicts it
//! doesn't resolve taking main's side, and tested by the workflows the land queue runs on a
//! candidate (`on: merge_group` or kiln's `on: land`) before it counts. Once every conflict has
//! one, they're committed and tested together (`…/<replay>/whole`); then a person accepts them,
//! unless the repository turned automatic resolution on. Accepted, they're the change's new
//! version (why `conflict resolved`), and each is remembered by its hunk's key for the next time
//! the same conflict comes, on any change of the repository.
//!
//! The ladder moves when a replay conflicts (`kick`), when a candidate's tests finish, and every
//! few seconds; what it decides is kept per replay (`review_ladders`, migration 20261029-2300) and
//! said on the change page.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use http::StatusCode;
use kiln_ci::ladder::{self, Author, Candidate, Rung, State, Step, Switches, Tests, Tried};
use kiln_ci::trigger::{Event, Source};
use rusqlite::{OptionalExtension as _, params};
use serde_json::{Value, json};

use super::Fail;
use super::conflicts::{self, ForLadder, Replay};
use super::diffs::{self, Diff};
use crate::authz::{Caller, Scope};
use crate::gitops::{self, LS_REMOTE_TIMEOUT};
use crate::hub::Hub;
use crate::wire::events::{ConflictChange, ServerEvent};

/// The rungs this kiln runs.
const CAN: &[Rung] = &[Rung::Structural, Rung::Remembered, Rung::Agent];

fn fail(e: anyhow::Error) -> Fail {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}

fn refused(m: impl Into<String>) -> Fail {
    (StatusCode::CONFLICT, m.into())
}

fn kicker() -> &'static crate::rt::Notify {
    static KICK: OnceLock<crate::rt::Notify> = OnceLock::new();
    KICK.get_or_init(crate::rt::Notify::new)
}

/// The diffs whose ladder is to move.
fn kicked() -> &'static parking_lot::Mutex<BTreeSet<String>> {
    static K: OnceLock<parking_lot::Mutex<BTreeSet<String>>> = OnceLock::new();
    K.get_or_init(Default::default)
}

/// Moves diff `id`'s ladder: its replay conflicted, or a person resolved one of its conflicts.
pub fn kick(id: &str) {
    kicked().lock().insert(id.to_owned());
    kicker().notify();
}

/// The ladder, for as long as the server runs: kicked, and every few seconds for tests under way.
pub async fn run_loop(hub: Arc<Hub>) -> anyhow::Result<()> {
    use futures::FutureExt as _;
    loop {
        if let Err(e) = tick(&hub).await {
            tracing::warn!(error = %format!("{e:#}"), "the conflict ladder stumbled");
        }
        futures::select_biased! {
            () = hub.shutdown.cancelled().fuse() => return Ok(()),
            () = kicker().notified().fuse() => {}
            () = crate::rt::sleep(Duration::from_secs(3)).fuse() => {}
        }
    }
}

async fn tick(hub: &Arc<Hub>) -> anyhow::Result<()> {
    let mut ids: BTreeSet<String> = std::mem::take(&mut *kicked().lock());
    // Tests under way: a candidate's (`r`) or a whole's (`w`), its diff, commit and workflows. Read
    // every 3 s beside the writer, prepared once (the DB query audit, 2026-11-06).
    let pending: Vec<(String, String, String, String, Option<String>)> = hub
        .store
        .read(|c| {
            let mut st = c.prepare_cached(
                "SELECT 'r', id, diff_id, commit_sha, started FROM review_resolutions WHERE tests = 'pending' AND commit_sha IS NOT NULL
                 UNION ALL SELECT 'w', replay_id, diff_id, whole_sha, whole_started FROM review_ladders WHERE whole_tests = 'pending' AND whole_sha IS NOT NULL",
            )?;
            let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await?;
    for (what, id, diff_id, sha, started) in pending {
        let Some(d) = diffs::find(hub, &diff_id).await? else { continue };
        let started: Vec<String> = started.as_deref().and_then(|s| serde_json::from_str::<Value>(s).ok()).and_then(|v| v.get("workflows").cloned()).and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default();
        let tests = tests_of(hub, &d.repo, &sha, &started).await;
        if tests == Tests::Pending {
            continue;
        }
        let (now, t) = (crate::util::now_unix(), tests.as_str());
        hub.store
            .call(move |c| {
                let sql = if what == "r" { "UPDATE review_resolutions SET tests = ?2, updated_at = ?3 WHERE id = ?1" } else { "UPDATE review_ladders SET whole_tests = ?2, updated_at = ?3 WHERE replay_id = ?1" };
                c.execute(sql, params![id, t, now])?;
                Ok(())
            })
            .await?;
        ids.insert(diff_id);
    }
    for id in ids {
        let Some(d) = diffs::find(hub, &id).await? else { continue };
        match climb(hub, &d).await {
            Ok(()) => {
                retries().lock().remove(&id);
            }
            // Tried again on the next tick, a while: a replay kiln is still keeping as a revision
            // (W26-R16) reads as moved until its version is in.
            Err((_, why)) => {
                let n = {
                    let mut r = retries().lock();
                    let n = r.entry(id.clone()).or_insert(0);
                    *n += 1;
                    *n
                };
                if n <= RETRIES {
                    kicked().lock().insert(id);
                    tracing::info!(diff = %d.name, %why, try = n, "the conflict ladder couldn't move this change yet");
                } else {
                    retries().lock().remove(&id);
                    tracing::warn!(diff = %d.name, %why, "the conflict ladder couldn't move this change; it moves again when it's kicked");
                }
            }
        }
    }
    Ok(())
}

/// How many ticks in a row a change's climb is tried again after failing.
const RETRIES: u32 = 20;

/// Each change's climbs failed in a row.
fn retries() -> &'static parking_lot::Mutex<HashMap<String, u32>> {
    static R: OnceLock<parking_lot::Mutex<HashMap<String, u32>>> = OnceLock::new();
    R.get_or_init(Default::default)
}

/// How the workflows `started` on `sha` have done: none started is none to run.
async fn tests_of(hub: &Hub, repo: &str, sha: &str, started: &[String]) -> Tests {
    if started.is_empty() {
        return Tests::NoneToRun;
    }
    let Ok(checks) = super::review::checks_on(hub, repo, sha).await else { return Tests::Pending };
    let stem = |p: &str| p.rsplit('/').next().unwrap_or(p).trim_end_matches(".yml").trim_end_matches(".yaml").to_owned();
    let mut out = Tests::Passed;
    for w in started {
        match checks.get(w).or_else(|| checks.get(&stem(w))) {
            Some(Some(c)) if kiln_ci::land::passed(c) => {}
            Some(Some(_)) => return Tests::Failed,
            _ => out = Tests::Pending,
        }
    }
    out
}

// ------------------------------------------------------------------ records

/// A rung's try at a conflict, as kept.
#[derive(Debug, Clone)]
struct Try {
    id: String,
    conflict: String,
    rung: Rung,
    by: String,
    state: String,
    why: Option<String>,
    lines: Option<Vec<String>>,
    explanation: Option<String>,
    commit: Option<String>,
    tests: Option<Tests>,
    at: i64,
}

impl Try {
    fn candidate(&self) -> Candidate {
        let tried = match self.state.as_str() {
            "working" => Tried::Working,
            "resolved" => Tried::Resolved { tests: self.tests.unwrap_or(Tests::Pending) },
            _ => Tried::Kept { why: self.why.clone().unwrap_or_default() },
        };
        Candidate { rung: self.rung, by: self.by.clone(), tried }
    }

    fn view(&self) -> Value {
        json!({
            "id": self.id, "rung": self.rung.as_str(), "by": self.by, "made_by": crate::ship::made_by_actor(&self.by), "state": self.state, "why": self.why, "lines": self.lines,
            "explanation": self.explanation, "commit": self.commit, "tests": self.tests.map(Tests::as_str), "at": self.at,
        })
    }
}

async fn tries(hub: &Hub, replay: &str) -> Result<Vec<Try>, Fail> {
    let id = replay.to_owned();
    hub.store
        .call(move |c| {
            let mut st = c.prepare("SELECT id, conflict_id, rung, made_by, state, why, lines, explanation, commit_sha, tests, created_at FROM review_resolutions WHERE replay_id = ?1 ORDER BY created_at, rowid")?;
            let rows = st.query_map([&id], |r| {
                Ok(Try {
                    id: r.get(0)?,
                    conflict: r.get(1)?,
                    rung: Rung::parse(&r.get::<_, String>(2)?).unwrap_or(Rung::Person),
                    by: r.get(3)?,
                    state: r.get(4)?,
                    why: r.get(5)?,
                    lines: r.get::<_, Option<String>>(6)?.and_then(|t| serde_json::from_str(&t).ok()),
                    explanation: r.get(7)?,
                    commit: r.get(8)?,
                    tests: r.get::<_, Option<String>>(9)?.as_deref().and_then(Tests::parse),
                    at: r.get(10)?,
                })
            })?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
        .map_err(fail)
}

/// A replay's ladder, as kept.
#[derive(Debug, Clone, Default)]
struct Kept {
    state: Option<String>,
    why: Vec<String>,
    whole: Option<String>,
    /// The resolutions the whole was made of.
    whole_of: Vec<String>,
    whole_tests: Option<Tests>,
    accepted_by: Option<String>,
    accepted_at: Option<i64>,
    version: Option<u32>,
}

async fn kept(hub: &Hub, replay: &str) -> Result<Kept, Fail> {
    let id = replay.to_owned();
    let row = hub
        .store
        .call(move |c| {
            Ok(c.query_row("SELECT state, why, whole_sha, whole_started, whole_tests, accepted_by, accepted_at, version FROM review_ladders WHERE replay_id = ?1", [&id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, Option<String>>(5)?,
                    r.get::<_, Option<i64>>(6)?,
                    r.get::<_, Option<u32>>(7)?,
                ))
            })
            .optional()?)
        })
        .await
        .map_err(fail)?;
    let Some((state, why, whole, started, tests, accepted_by, accepted_at, version)) = row else { return Ok(Kept::default()) };
    let started: Value = started.as_deref().and_then(|s| serde_json::from_str(s).ok()).unwrap_or(Value::Null);
    let whole_of = started.get("of").cloned().and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default();
    Ok(Kept {
        state: Some(state),
        why: why.as_deref().and_then(|w| serde_json::from_str(w).ok()).unwrap_or_default(),
        whole,
        whole_of,
        whole_tests: tests.as_deref().and_then(Tests::parse),
        accepted_by,
        accepted_at,
        version,
    })
}

async fn save(hub: &Hub, d: &Diff, replay: &str, state: State, why: &[String]) -> Result<(), Fail> {
    let (rid, did, st, why, now) = (replay.to_owned(), d.id.clone(), state.as_str(), json!(why).to_string(), crate::util::now_unix());
    let changed = hub
        .store
        .call(move |c| {
            let before: Option<(String, Option<String>)> = c.query_row("SELECT state, why FROM review_ladders WHERE replay_id = ?1", [&rid], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
            if before.as_ref().is_some_and(|(s, w)| s == st && w.as_deref() == Some(why.as_str())) {
                return Ok(false);
            }
            c.execute(
                "INSERT INTO review_ladders (replay_id, diff_id, state, why, updated_at) VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT (replay_id) DO UPDATE SET state = excluded.state, why = excluded.why, updated_at = excluded.updated_at",
                params![rid, did, st, why, now],
            )?;
            Ok(true)
        })
        .await
        .map_err(fail)?;
    if changed {
        hub.emit(&ServerEvent::Conflict(Box::new(ConflictChange { state: Some(format!("ladder:{}", state.as_str())), ..conflicts::changed(d, replay) })));
    }
    Ok(())
}

/// A repository's automatic resolution, from its page in Repos (off until turned on; Ada's
/// decision 4).
async fn automatic_of(hub: &Hub, repo_id: &str) -> bool {
    let id = repo_id.to_owned();
    hub.store
        .call(move |c| Ok(c.query_row("SELECT automatic FROM review_ladder_switches WHERE repo_id = ?1", [&id], |r| r.get::<_, bool>(0)).optional()?))
        .await
        .ok()
        .flatten()
        .unwrap_or(false)
}

/// The ladder's switches for `d`: sending an agent is the trunk's `.kiln/review.yaml`
/// `fresh.conflict-agents` (W26-R16's setting, off by default); automatic resolution is the
/// repository's page in Repos.
async fn switches_for(hub: &Hub, d: &Diff, repo_id: &str, path: &str) -> Switches {
    let (policy, _) = super::review::policy(hub, path, &d.trunk).await;
    Switches { send_agent: policy.fresh.conflict_agents, automatic: automatic_of(hub, repo_id).await }
}

/// The language a file's hunks are remembered under: its extension (a file without one, its
/// name).
fn language_of(file: &str) -> String {
    let name = file.rsplit('/').next().unwrap_or(file);
    name.rsplit_once('.').map_or(name, |(_, ext)| ext).to_lowercase()
}

/// Remembers `lines` as the resolution of the hunk whose sides are `main` and `this` in `file`, in
/// repository `repo_id`, accepted by `by` (W26-R17's rung 2).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn remember(hub: &Hub, repo_id: &str, file: &str, main: &[String], this: &[String], lines: &[String], by: &str, resolution: Option<&str>) {
    let key = ladder::key(&language_of(file), main, this);
    let (repo, lines, by, resolution, now) = (repo_id.to_owned(), json!(lines).to_string(), by.to_owned(), resolution.map(str::to_owned), crate::util::now_unix());
    let done = hub
        .store
        .call(move |c| {
            c.execute(
                "INSERT INTO review_remembered (repo_id, hunk_key, lines, resolution_id, accepted_by, accepted_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT (repo_id, hunk_key) DO UPDATE SET lines = excluded.lines, resolution_id = excluded.resolution_id, accepted_by = excluded.accepted_by, accepted_at = excluded.accepted_at",
                params![repo, key, lines, resolution, by, now],
            )?;
            Ok(())
        })
        .await;
    if let Err(e) = done {
        tracing::warn!(error = %format!("{e:#}"), %file, "a resolution wasn't remembered");
    }
}

/// The resolution remembered for a hunk, and who accepted it.
async fn remembered(hub: &Hub, repo_id: &str, c: &ForLadder) -> Result<Option<(Vec<String>, String)>, Fail> {
    let (repo, key) = (repo_id.to_owned(), ladder::key(&language_of(&c.file), &c.main, &c.this));
    hub.store
        .call(move |db| {
            let got: Option<(String, String)> = db.query_row("SELECT lines, accepted_by FROM review_remembered WHERE repo_id = ?1 AND hunk_key = ?2", params![repo, key], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
            if got.is_some() {
                db.execute("UPDATE review_remembered SET used = used + 1 WHERE repo_id = ?1 AND hunk_key = ?2", params![repo, key])?;
            }
            Ok(got.and_then(|(lines, by)| serde_json::from_str::<Vec<String>>(&lines).ok().map(|l| (l, by))))
        })
        .await
        .map_err(fail)
}

// ------------------------------------------------------------------ the climb

/// One move of `d`'s ladder: each conflict tries its next rung, the resolutions are tested
/// together once each has one, and where the repository allows a tested whole goes on.
pub(crate) async fn climb(hub: &Arc<Hub>, d: &Diff) -> Result<(), Fail> {
    let Some(replay) = conflicts::current(hub, d).await?.filter(|r| r.state == "conflicted") else { return Ok(()) };
    let (repo_id, path) = conflicts::home(hub, d).await?;
    let views = conflicts::for_ladder(hub, &replay.id).await?;
    let all = tries(hub, &replay.id).await?;
    let before = kept(hub, &replay.id).await?;
    if before.accepted_by.is_some() {
        return Ok(());
    }
    let sw = switches_for(hub, d, &repo_id, &path).await;
    let author = if authors_agent_id(d).is_some() { Author::Agent { live: authors_agent(hub, d).await.is_some() } } else { Author::Person };
    let conflicts: Vec<ladder::Conflict> = views
        .iter()
        .map(|v| ladder::Conflict { id: v.id.clone(), place: v.place.clone(), text: v.text, candidates: all.iter().filter(|t| t.conflict == v.id).map(Try::candidate).collect(), taken: v.taken })
        .collect();
    // The whole stands only for the resolutions it was made of.
    let first = ladder::next(&conflicts, author, sw, CAN, None);
    let ready = ready(&first.steps, &all);
    let whole = if before.whole.is_some() && before.whole_of == ready.keys().cloned().collect::<Vec<_>>() { before.whole_tests } else { None };
    let next = ladder::next(&conflicts, author, sw, CAN, whole);
    let mut for_agent: Vec<&ForLadder> = Vec::new();
    for (id, step) in &next.steps {
        if let (Step::Try { rung }, Some(v)) = (step, views.iter().find(|v| &v.id == id)) {
            if *rung == Rung::Agent {
                for_agent.push(v);
                continue;
            }
            attempt(hub, d, &replay, &repo_id, v, *rung, &ready).await?;
            kick(&d.id);
        }
    }
    if !for_agent.is_empty() {
        ask_agent(hub, d, &replay, &for_agent, sw).await?;
    }
    match next.state {
        State::TestWhole => test_whole(hub, d, &replay, &ready).await?,
        State::GoesOn if !ready.is_empty() => {
            let by = super::cr::By { pushed_by: "kiln".into(), author: ("kiln".into(), "kiln"), started_by: None };
            finish(hub, d, &replay, &views, &all, "kiln", by).await?;
        }
        _ => {}
    }
    save(hub, d, &replay.id, next.state, &next.why).await
}

/// Each conflict's resolution whose tests passed: its try, by conflict id.
fn ready<'a>(steps: &[(String, Step)], all: &'a [Try]) -> BTreeMap<String, &'a Try> {
    let mut out = BTreeMap::new();
    for (id, s) in steps {
        if let Step::Ready { rung, .. } = s
            && let Some(t) = all.iter().rfind(|t| &t.conflict == id && t.rung == *rung && t.state == "resolved" && t.tests.is_some_and(Tests::count))
        {
            out.insert(id.clone(), t);
        }
    }
    out
}

fn picks(ready: &BTreeMap<String, &Try>) -> HashMap<String, Vec<String>> {
    ready.iter().filter_map(|(id, t)| t.lines.clone().map(|l| (id.clone(), l))).collect()
}

/// `rung`'s try at conflict `c`, recorded: kept with why, or a candidate committed, kept and
/// under test.
async fn attempt(hub: &Hub, d: &Diff, replay: &Replay, repo_id: &str, c: &ForLadder, rung: Rung, ready: &BTreeMap<String, &Try>) -> Result<(), Fail> {
    let made: Result<(Vec<String>, String), String> = match rung {
        Rung::Structural => structural(hub, d, replay, c).await?,
        Rung::Remembered => match remembered(hub, repo_id, c).await? {
            Some((lines, by)) => Ok((lines, format!("remembered from {by}'s resolution"))),
            None => Err("nothing is remembered for this conflict".to_owned()),
        },
        _ => Err(format!("{} isn't on this kiln's ladder", rung.named())),
    };
    let id = crate::util::new_id();
    let (lines, explanation) = match made {
        Ok(m) => m,
        Err(why) => return record(hub, &id, replay, d, c, rung, "kiln", "kept", Some(&why), None, None).await,
    };
    let mut p = picks(ready);
    p.insert(c.id.clone(), lines.clone());
    let sha = conflicts::candidate(hub, d, replay, &p, "kiln", &[]).await?;
    let git_ref = format!("refs/kiln/resolve/{}/{id}", d.name);
    keep_ref(hub, d, &git_ref, &sha).await?;
    let started = test(hub, d, replay, &sha, &git_ref).await?;
    record(hub, &id, replay, d, c, rung, "kiln", "resolved", None, Some((&lines, &explanation)), Some((&sha, &started))).await
}

/// The agent a change's author names (`agent:<id> for user:…`), by id.
fn authors_agent_id(d: &Diff) -> Option<&str> {
    d.author.strip_prefix("agent:").and_then(|s| s.split(' ').next()).filter(|s| !s.is_empty())
}

/// The change's author's agent, while it runs: its session's id and name.
async fn authors_agent(hub: &Hub, d: &Diff) -> Option<(String, String)> {
    let id = authors_agent_id(d)?;
    let agent = crate::workspace::change::of(hub, id).await.map_or_else(|| id.to_owned(), |r| r.agent);
    hub.harness.info(&agent).filter(|i| i.status != crate::harness::model::Status::Exited).map(|i| (i.id, i.name))
}

/// Rung 3: the conflicts left, handed to the change's author's agent, or, where the repository
/// turns it on, to an agent kiln starts (on a runner kiln picks; it clones the repository into a
/// workspace of its own). Each is at work until the agent answers it (`answered`).
async fn ask_agent(hub: &Arc<Hub>, d: &Diff, replay: &Replay, left: &[&ForLadder], sw: Switches) -> Result<(), Fail> {
    let sha = conflicts::conflicted(hub, d, replay).await?;
    let git_ref = format!("refs/kiln/resolve/{}/{}/conflicted", d.name, replay.id);
    keep_ref(hub, d, &git_ref, &sha).await?;
    let text = brief(hub, d, replay, left, &git_ref);
    let asked: Result<(String, String, String), String> = match authors_agent(hub, d).await {
        Some((id, name)) => {
            if crate::workspace::follow::deliver(hub, &id, text).await {
                Ok((d.author.clone(), name, id))
            } else {
                Err(format!("kiln couldn't reach {name}"))
            }
        }
        None if sw.send_agent => {
            let opener = d.author_for.clone().unwrap_or_else(|| d.author.clone());
            let starter = crate::harness::lineage::Starter { parent: None, opener: Some(opener), actor: format!("ladder:{}", d.name), plan_run: None, asker: None };
            let ask = crate::harness::lineage::Ask { kind: "claude".into(), prompt: Some(text), name: Some(format!("Resolve {}'s conflicts", d.name)), count: Some(1), ..Default::default() };
            match crate::harness::lineage::start(hub, &starter, ask).await {
                Ok(infos) => infos.into_iter().next().map(|i| (format!("agent:{}", i.id), i.name, i.id)).ok_or_else(|| "no agent started".to_owned()),
                Err((_, why)) => Err(why),
            }
        }
        None => Err("no agent to ask".to_owned()),
    };
    // Ship's row says the agent is on it (W26-R18's Fixing), on the change's latest revision.
    if let (Ok((_, name, agent)), Some(v)) = (&asked, diffs::versions(hub, d).await.map_err(fail)?.pop())
        && let Err((_, why)) = crate::ship::conflicts_handed(hub, &d.repo, &v.commit, name, agent, v.n).await
    {
        tracing::info!(diff = %d.name, %why, "the hand-off wasn't kept for Ship");
    }
    for c in left {
        let id = crate::util::new_id();
        match &asked {
            Ok((by, name, _)) => record(hub, &id, replay, d, c, Rung::Agent, by, "working", None, Some((&[], &format!("asked {name}"))), None).await?,
            Err(why) => record(hub, &id, replay, d, c, Rung::Agent, "kiln", "kept", Some(why), None, None).await?,
        }
    }
    Ok(())
}

/// What an agent is asked: the conflicts left, where to work on them, and how to answer.
fn brief(hub: &Hub, d: &Diff, replay: &Replay, left: &[&ForLadder], git_ref: &str) -> String {
    let clone = format!("{}/git/{}.git", hub.cfg.server.public_url.trim_end_matches('/'), d.repo);
    let workspace = format!("~/.cache/kiln/resolve/{}/{}", d.name, replay.id);
    let mut out = format!(
        "{} conflicts with {} where it lands, and kiln's structural merge and the resolutions it remembers didn't settle {}. Resolve {}, say why, and run the tests they affect.\n\nWork in a workspace of your own:\n\n    git clone {clone} {workspace} && cd {workspace} && git fetch origin {git_ref} && git checkout FETCH_HEAD\n\nThat's {} replayed onto {}, git's markers (zdiff3: main's side, the base, this change's) where each conflict is.\n\nThe conflicts:\n",
        d.name,
        replay.onto_name,
        if left.len() == 1 { "this one" } else { "these" },
        if left.len() == 1 { "it" } else { "each" },
        d.name,
        replay.onto_name,
    );
    for c in left {
        out.push_str(&format!("\n- {} (conflict `{}`): main has\n{}\n  and {} has\n{}\n", c.place, c.id, c.main.iter().map(|l| format!("      {l}")).collect::<Vec<_>>().join("\n"), d.name, c.this.iter().map(|l| format!("      {l}")).collect::<Vec<_>>().join("\n")));
    }
    out.push_str(&format!(
        "\nAnswer each with ci.diffs.conflicts.resolve: {{\"diff\": \"{}\", \"conflict\": \"<its id>\", \"take\": \"edit\", \"text\": \"<the lines it resolves to>\", \"explanation\": \"<why, and which tests you ran and what they said>\"}}. kiln commits your resolution as you, runs the change's tests on it again, and a person confirms it before it lands.\n",
        d.name
    ));
    let facts = json!({ "diff": d.name, "replay": replay.id, "clone": clone, "ref": git_ref, "workspace": workspace, "conflicts": left.iter().map(|c| json!({ "id": c.id, "file": c.file, "place": c.place, "main": c.main, "this": c.this })).collect::<Vec<_>>() });
    out.push_str(&format!("\n```json\n{facts}\n```\n"));
    out
}

/// An agent's resolution of conflict `conflict` (its lines and why), as rung 3's candidate: a
/// revision committed as the agent (`by`), kept and under test.
pub(crate) async fn answered(hub: &Hub, d: &Diff, replay: &Replay, conflict: &str, by: &str, lines: &[String], explanation: Option<&str>) -> Result<(), Fail> {
    let all = tries(hub, &replay.id).await?;
    let mut p: HashMap<String, Vec<String>> = HashMap::new();
    for t in all.iter().filter(|t| t.conflict != conflict && t.state == "resolved" && t.tests.is_some_and(Tests::count)) {
        if let Some(l) = &t.lines {
            p.insert(t.conflict.clone(), l.clone());
        }
    }
    p.insert(conflict.to_owned(), lines.to_vec());
    let sha = conflicts::candidate(hub, d, replay, &p, by, &[]).await?;
    let working = all.iter().rfind(|t| t.conflict == conflict && t.rung == Rung::Agent && t.state == "working").map(|t| t.id.clone());
    let id = working.clone().unwrap_or_else(crate::util::new_id);
    let git_ref = format!("refs/kiln/resolve/{}/{id}", d.name);
    keep_ref(hub, d, &git_ref, &sha).await?;
    let started = test(hub, d, replay, &sha, &git_ref).await?;
    match working {
        Some(id) => {
            let tests = if started.is_empty() { Tests::NoneToRun } else { Tests::Pending };
            let (by, lines, explanation, sha, started, now) = (by.to_owned(), json!(lines).to_string(), explanation.map(str::to_owned), sha.clone(), json!({ "workflows": started }).to_string(), crate::util::now_unix());
            hub.store
                .call(move |c| {
                    c.execute(
                        "UPDATE review_resolutions SET state = 'resolved', made_by = ?2, lines = ?3, explanation = ?4, commit_sha = ?5, started = ?6, tests = ?7, updated_at = ?8 WHERE id = ?1",
                        params![id, by, lines, explanation, sha, started, tests.as_str(), now],
                    )?;
                    Ok(())
                })
                .await
                .map_err(fail)?;
        }
        None => {
            let views = conflicts::for_ladder(hub, &replay.id).await?;
            let Some(c) = views.iter().find(|v| v.id == conflict) else { return Ok(()) };
            record(hub, &id, replay, d, c, Rung::Agent, by, "resolved", None, Some((lines, explanation.unwrap_or_default())), Some((&sha, &started))).await?;
        }
    }
    kick(&d.id);
    Ok(())
}

/// Rung 1: kiln's structural merge of the hunk in its file (`kiln_search::merge`), its lines, or
/// why it kept the markers.
async fn structural(hub: &Hub, d: &Diff, replay: &Replay, c: &ForLadder) -> Result<Result<(Vec<String>, String), String>, Fail> {
    let Some(a) = conflicts::around(hub, d, replay, &c.id).await? else { return Ok(Err("git didn't carry this conflict's base".to_owned())) };
    Ok(match kiln_search::merge::resolve_hunk(&a.file, &a.before, &a.base, &a.main, &a.this, &a.after) {
        kiln_search::merge::Outcome::Resolved(text) => {
            let body = text.strip_suffix('\n').unwrap_or(&text);
            let lines = if text.is_empty() { Vec::new() } else { body.split('\n').map(str::to_owned).collect() };
            Ok((lines, "merged by its syntax: each side changed different parts".to_owned()))
        }
        kiln_search::merge::Outcome::Kept(k) => Err(k.why().to_owned()),
    })
}

#[allow(clippy::too_many_arguments)]
async fn record(hub: &Hub, id: &str, replay: &Replay, d: &Diff, c: &ForLadder, rung: Rung, by: &str, state: &str, why: Option<&str>, made: Option<(&[String], &str)>, commit: Option<(&str, &[String])>) -> Result<(), Fail> {
    let tests = commit.map(|(_, started)| if started.is_empty() { Tests::NoneToRun } else { Tests::Pending });
    let row = (
        id.to_owned(),
        replay.id.clone(),
        c.id.clone(),
        d.id.clone(),
        rung.as_str(),
        by.to_owned(),
        state.to_owned(),
        why.map(str::to_owned),
        made.map(|(l, _)| json!(l).to_string()),
        made.map(|(_, e)| e.to_owned()),
        commit.map(|(s, _)| s.to_owned()),
        commit.map(|(_, w)| json!({ "workflows": w }).to_string()),
        tests.map(Tests::as_str),
    );
    let now = crate::util::now_unix();
    hub.store
        .call(move |db| {
            db.execute(
                "INSERT INTO review_resolutions (id, replay_id, conflict_id, diff_id, rung, made_by, state, why, lines, explanation, commit_sha, started, tests, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?14)",
                params![row.0, row.1, row.2, row.3, row.4, row.5, row.6, row.7, row.8, row.9, row.10, row.11, row.12, now],
            )?;
            Ok(())
        })
        .await
        .map_err(fail)?;
    hub.store.audit(by.to_owned(), "diffs.conflict.ladder".into(), format!("{} {} v{} ({}): {}, {state}{}", d.repo, d.name, replay.version, c.place, rung.named(), why.map(|w| format!(": {w}")).unwrap_or_default())).await;
    Ok(())
}

async fn keep_ref(hub: &Hub, d: &Diff, git_ref: &str, sha: &str) -> Result<(), Fail> {
    let (_, path) = conflicts::home(hub, d).await?;
    gitops::run_git(hub, &["-C", &path, "update-ref", git_ref, sha], LS_REMOTE_TIMEOUT).await.map_err(fail)?;
    Ok(())
}

/// Starts the change's tests on candidate `sha` (kept at `git_ref`): the workflows the land queue
/// runs on a candidate, with the replay's base as what it's measured from. Their paths.
async fn test(hub: &Hub, d: &Diff, replay: &Replay, sha: &str, git_ref: &str) -> Result<Vec<String>, Fail> {
    let (repo, path) = diffs::home(hub, d).await?;
    let ctx = super::events::facts(hub, &repo).await.repo;
    let tip = super::land::commit_of(hub, &path, sha).await;
    let kiln = kiln_ci::trigger::context::Person { name: "kiln".into(), email: String::new(), username: Some("kiln[bot]".into()) };
    let mut payload = kiln_ci::trigger::context::merge_group(&ctx, &d.trunk, &replay.onto, &tip, git_ref, &kiln);
    if let Some(p) = payload.as_object_mut() {
        p.insert("kiln".into(), json!({ "ladder": { "diff": d.name, "replay": replay.id, "version": replay.version } }));
    }
    let mg = Event { name: "merge_group".into(), action: "checks_requested".into(), repo: repo.name.clone(), git_ref: git_ref.to_owned(), sha: sha.to_owned(), base_ref: Some(d.trunk.clone()), actor: "kiln".into(), trusted: true, source: Source::Native, payload, ..Event::default() };
    let landing = Event { name: "land".into(), action: String::new(), ..mg.clone() };
    super::land::start_checks(hub, &repo, &(mg, landing)).await.map_err(fail)
}

/// Every conflict's resolution, committed together and under test.
async fn test_whole(hub: &Hub, d: &Diff, replay: &Replay, ready: &BTreeMap<String, &Try>) -> Result<(), Fail> {
    let agents: Vec<&str> = ready.values().filter(|t| t.rung == Rung::Agent).map(|t| t.by.as_str()).collect::<BTreeSet<_>>().into_iter().collect();
    let sha = conflicts::candidate(hub, d, replay, &picks(ready), "kiln", &agents).await?;
    let git_ref = format!("refs/kiln/resolve/{}/{}/whole", d.name, replay.id);
    keep_ref(hub, d, &git_ref, &sha).await?;
    let started = test(hub, d, replay, &sha, &git_ref).await?;
    let tests = if started.is_empty() { Tests::NoneToRun } else { Tests::Pending };
    let of: Vec<&String> = ready.keys().collect();
    let (rid, did, sha2, st, t, now) = (replay.id.clone(), d.id.clone(), sha.clone(), json!({ "workflows": started, "of": of }).to_string(), tests.as_str(), crate::util::now_unix());
    hub.store
        .call(move |c| {
            c.execute(
                "INSERT INTO review_ladders (replay_id, diff_id, state, whole_sha, whole_started, whole_tests, updated_at) VALUES (?1, ?2, 'test_whole', ?3, ?4, ?5, ?6)
                 ON CONFLICT (replay_id) DO UPDATE SET whole_sha = excluded.whole_sha, whole_started = excluded.whole_started, whole_tests = excluded.whole_tests, updated_at = excluded.updated_at",
                params![rid, did, sha2, st, t, now],
            )?;
            Ok(())
        })
        .await
        .map_err(fail)?;
    kick(&d.id);
    Ok(())
}

/// The tested whole of `replay` as `d`'s new version, accepted by `me`; each resolution in it
/// remembered.
async fn finish(hub: &Arc<Hub>, d: &Diff, replay: &Replay, views: &[ForLadder], all: &[Try], me: &str, by: super::cr::By) -> Result<u32, Fail> {
    let k = kept(hub, &replay.id).await?;
    let sha = k.whole.clone().ok_or_else(|| refused(format!("{}: its resolutions aren't tested together yet", d.name)))?;
    let n = conflicts::accept(hub, d, replay, &sha, me, by).await?;
    let (repo_id, _) = conflicts::home(hub, d).await?;
    for id in &k.whole_of {
        let (Some(t), Some(v)) = (all.iter().rfind(|t| &t.conflict == id && t.state == "resolved"), views.iter().find(|v| &v.id == id)) else { continue };
        if let Some(lines) = &t.lines {
            remember(hub, &repo_id, &v.file, &v.main, &v.this, lines, me, Some(&t.id)).await;
        }
    }
    let (rid, me2, now) = (replay.id.clone(), me.to_owned(), crate::util::now_unix());
    hub.store
        .call(move |c| {
            c.execute("UPDATE review_ladders SET state = 'accepted', accepted_by = ?2, accepted_at = ?3, version = ?4, updated_at = ?3 WHERE replay_id = ?1", params![rid, me2, now, n])?;
            Ok(())
        })
        .await
        .map_err(fail)?;
    hub.store.audit(me.to_owned(), "diffs.conflicts.accept".into(), format!("{} {} v{n}: {} resolutions accepted ({sha})", d.repo, d.name, k.whole_of.len())).await;
    Ok(n)
}

// ------------------------------------------------------------------ the API

/// `GET /api/diffs/:id/conflicts/ladder`: where `d`'s conflicted replay is on the ladder: its state
/// and why, each conflict's tries, and the resolutions tested together.
pub async fn view(hub: &Hub, d: &Diff) -> Result<Value, Fail> {
    let Some(replay) = conflicts::current(hub, d).await?.filter(|r| r.state == "conflicted") else { return Ok(json!({ "diff": d.name, "replay": null })) };
    let (repo_id, path) = conflicts::home(hub, d).await?;
    let all = tries(hub, &replay.id).await?;
    let k = kept(hub, &replay.id).await?;
    let sw = switches_for(hub, d, &repo_id, &path).await;
    let mut by: BTreeMap<&str, Vec<Value>> = BTreeMap::new();
    for t in &all {
        by.entry(t.conflict.as_str()).or_default().push(t.view());
    }
    Ok(json!({
        "diff": d.name, "replay": replay.id, "version": replay.version,
        "state": k.state, "why": k.why,
        "conflicts": by,
        "whole": k.whole.as_ref().map(|sha| json!({ "commit": sha, "of": k.whole_of, "tests": k.whole_tests.map(Tests::as_str) })),
        "accepted": k.accepted_by.as_ref().map(|who| json!({ "by": who, "at": k.accepted_at, "version": k.version, "signer": Value::Null })),
        "switches": { "conflict_agents": sw.send_agent, "automatic": sw.automatic },
    }))
}

/// The ladder in `GET /api/diffs/:id/conflicts` (`v`, its view of `replay`): each conflict's
/// `offered`, its rung's latest try (one whose tests passed first), and the replay's `ladder`, its
/// state and why.
pub(crate) async fn annotate(hub: &Hub, replay: &str, v: &mut Value) {
    let (Ok(all), Ok(k)) = (tries(hub, replay).await, kept(hub, replay).await) else { return };
    if let Some(list) = v.get_mut("conflicts").and_then(Value::as_array_mut) {
        for c in list {
            let id = c.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
            let mine: Vec<&Try> = all.iter().filter(|t| t.conflict == id).collect();
            let best = mine.iter().rev().find(|t| t.state == "resolved" && t.tests.is_some_and(Tests::count)).or_else(|| mine.last());
            if let (Some(o), Some(t)) = (c.as_object_mut(), best) {
                let mut seen = t.view();
                if let Some(s) = seen.as_object_mut() {
                    s.insert("signer".into(), Value::Null);
                }
                o.insert("offered".into(), seen);
            }
        }
    }
    if let Some(o) = v.as_object_mut() {
        o.insert("ladder".into(), k.state.as_ref().map_or(Value::Null, |s| json!({ "state": s, "why": k.why })));
    }
}

/// `POST /api/diffs/:id/conflicts/accept`: a person accepts the resolutions the ladder offers (all
/// of them, tested together): `d`'s new version, its why `conflict resolved`. An agent can't.
pub async fn accept(hub: &Arc<Hub>, d: &Diff, caller: &Caller) -> Result<Value, Fail> {
    conflicts::may_change(caller, d)?;
    let (me, agent) = super::review::who(caller);
    if agent {
        return Err((StatusCode::FORBIDDEN, format!("{}: a person accepts a conflict's resolution", d.name)));
    }
    let replay = conflicts::current(hub, d).await?.filter(|r| r.state == "conflicted").ok_or_else(|| refused(format!("{} goes onto where it lands cleanly: nothing to accept", d.name)))?;
    let k = kept(hub, &replay.id).await?;
    if k.state.as_deref() != Some(State::Offered.as_str()) {
        let why = if k.why.is_empty() { "nothing is offered yet".to_owned() } else { k.why.join("; ") };
        return Err(refused(format!("{}: {why}", d.name)));
    }
    let views = conflicts::for_ladder(hub, &replay.id).await?;
    let all = tries(hub, &replay.id).await?;
    let by = super::cr::By { pushed_by: caller.name.clone(), author: diffs::author_of(&caller.name, Some(caller.kind)), started_by: caller.started_by() };
    let n = finish(hub, d, &replay, &views, &all, &me, by).await?;
    Ok(json!({ "diff": d.name, "version": n, "why": conflicts::WHY, "accepted_by": me, "replay": replay.id }))
}

/// `GET` or `PUT /api/repos/:repo/ladder`: a repository's automatic resolution (`automatic`, off
/// by default; changing it takes `diffs.land` on the repository), and whether its conflicts send an
/// agent (`conflict_agents`, read only: the trunk's `.kiln/review.yaml` `fresh.conflict-agents`).
pub async fn switches(hub: &Hub, repo: &str, caller: &Caller, body: Option<&Value>) -> Result<Value, Fail> {
    let repos = hub.store.repos().await.map_err(fail)?;
    let r = repos.into_iter().find(|r| r.name == repo).ok_or((StatusCode::NOT_FOUND, format!("no repository {repo}")))?;
    let scope = Scope::Repo { repo: r.name.clone() };
    if !caller.can("diffs.read", &scope) {
        return Err((StatusCode::FORBIDDEN, "reading a repository's ladder needs `diffs.read`".into()));
    }
    if let Some(body) = body {
        if !caller.can("diffs.land", &scope) {
            return Err((StatusCode::FORBIDDEN, "changing a repository's ladder needs `diffs.land`".into()));
        }
        if body.get("conflict_agents").is_some() || body.get("send_agent").is_some() {
            return Err((StatusCode::BAD_REQUEST, "sending an agent is the trunk's .kiln/review.yaml: `fresh: conflict-agents: true`".into()));
        }
        let was = automatic_of(hub, &r.id).await;
        let automatic = body.get("automatic").map_or(Ok(was), |v| v.as_bool().ok_or_else(|| (StatusCode::BAD_REQUEST, "`automatic`: true or false".to_owned())))?;
        let (id, by, now) = (r.id.clone(), caller.name.clone(), crate::util::now_unix());
        hub.store
            .call(move |c| {
                c.execute(
                    "INSERT INTO review_ladder_switches (repo_id, automatic, set_by, set_at) VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT (repo_id) DO UPDATE SET automatic = excluded.automatic, set_by = excluded.set_by, set_at = excluded.set_at",
                    params![id, automatic, by, now],
                )?;
                Ok(())
            })
            .await
            .map_err(fail)?;
        hub.store.audit(caller.name.clone(), "repo.ladder".into(), format!("{}: automatic resolution {}", r.name, if automatic { "on" } else { "off" })).await;
    }
    let automatic = automatic_of(hub, &r.id).await;
    let trunk = r.default_branch.clone().unwrap_or_else(|| "main".to_owned());
    let conflict_agents = match super::hosting::native_path(hub, &r) {
        Some(path) => super::review::policy(hub, &path.to_string_lossy(), &trunk).await.0.fresh.conflict_agents,
        None => false,
    };
    Ok(json!({
        "repo": r.name, "conflict_agents": conflict_agents, "automatic": automatic,
        "says": {
            "conflict_agents": "Set in the trunk's .kiln/review.yaml (fresh: conflict-agents: true): when a change's conflicts are left after kiln's merge and what it remembers, kiln sends an agent to a machine with the harness, in a workspace of its own, to resolve them and run their tests.",
            "automatic": "A resolution whose tests pass goes on without a person: it becomes the change's new version by itself. While this is off, a person confirms each one.",
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::language_of;

    #[test]
    fn w26_r17_a_files_language_is_its_extension() {
        assert_eq!(language_of("plan/expand.rs"), "rs");
        assert_eq!(language_of("ui/App.TSX"), "tsx");
        assert_eq!(language_of("Makefile"), "makefile");
    }
}
