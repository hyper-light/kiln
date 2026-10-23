//! Programs: multi-stack programs kiln runs, as the Programs view shows them (Mission control,
//! `/agents/programs`; docs/programs.md). A program is a master plan (tagged `master`) and the
//! track plans that share its tag, each tagged `t<n>`: the critical path (master plan
//! 01a157fddb8276a2996246ea931c4808) first. It's run as goals on a coordinating agent (the master
//! plan's latest goals run): its goal stack is the list of record, its goals matched to tracks by
//! their ids (`CP-T7.1`).
//!
//! Each track's worker is assigned, and kept here (`programs.assign`: the coordinator or the
//! person): any agent of the same person, started by the coordinator or not. A goal of the
//! coordinator's stack put under way on the worker shows in that stack as under way there
//! (`goals::Item::on`). A track's state is its worker's session's: at work, paused at a usage
//! limit (its window, and when it starts over) or at its threshold.
//!
//! For each track the view says its worker and how it stands, the goal under way, its goals met of
//! all, its change requests, its pass (stride scheduling: each goal met adds 1/weight, and a free
//! slot goes to the ready track whose pass is lowest), its slot and its usage. The person sets the
//! slots and the weights (seeded once from the master plan's tracks table, then only theirs); the
//! coordinator is told, and follows them at its next goal boundary. The usage rules (the master
//! plan's §5) are kiln's per-session thresholds (`pause_at`): every worker pauses at 95% of a
//! window, the lowest-weight track at work at 80%, and above 80% of a weekly window no new goal
//! starts (the goal driver's, and an assignment's).

pub mod ops;

use std::collections::{BTreeMap, HashMap};

use rusqlite::OptionalExtension as _;
use serde::{Deserialize, Serialize};

use crate::harness::model::{Command, Limit, Status};
use crate::harness::session::Info;
use crate::hub::Hub;

/// Slots by default: at most this many tracks at work at once (the master plan's §5).
pub const SLOTS: u32 = 6;
/// A track's weight when its settings don't say.
pub const WEIGHT: u32 = 1;
/// The most slots, and the highest weight, the person may set.
pub const SLOTS_MAX: u32 = 32;
pub const WEIGHT_MAX: u32 = 100;
/// Above this share of a weekly window, no new goal starts.
pub const HOLD_PCT: f64 = 80.0;
/// Above this, a stack of the program the driver drives pauses (and goes on under `HOLD_PCT`).
pub const PAUSE_PCT: f64 = 95.0;
/// Every worker's threshold (`pause_at`): its session pauses once a window is this full.
pub const WORKER_PAUSE_AT: u8 = 95;
/// The lowest-weight track at work's worker's threshold.
pub const LOWEST_PAUSE_AT: u8 = 80;
/// How a stack paused for usage says so (its reason starts with it).
pub const PAUSED_FOR_USAGE: &str = "Paused for usage";
/// Most programs, and track plans a program reads; most change requests a track shows.
const PROGRAMS_MAX: i64 = 20;
const TRACKS_MAX: i64 = 64;
const CHANGES_MAX: usize = 8;

/// What a program's usage allows.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "rule", rename_all = "snake_case")]
pub enum Rule {
    /// Under the hold threshold, or not known.
    Ok,
    /// No new goal starts: the goal under way goes on.
    Hold { why: String },
    /// A stack the driver drives pauses (and the workers' thresholds pause their sessions).
    Pause { why: String },
}

impl Rule {
    /// The usage rule at `pct` of a weekly window (none: not known).
    pub fn at(pct: Option<f64>) -> Self {
        match pct {
            Some(p) if p > PAUSE_PCT => Self::Pause { why: format!("{PAUSED_FOR_USAGE}: the weekly window is at {p:.0}%, over {PAUSE_PCT:.0}% (the program's goal stacks pause)") },
            Some(p) if p > HOLD_PCT => Self::Hold { why: format!("Held for usage: the weekly window is at {p:.0}%, over {HOLD_PCT:.0}% (no new goal starts)") },
            _ => Self::Ok,
        }
    }

    pub fn holds_new_goals(&self) -> bool {
        !matches!(self, Self::Ok)
    }
}

/// The person's settings of a program, as kept.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// None: the default ([`SLOTS`]).
    pub slots: Option<u32>,
    /// By track (`T7`); a track not here weighs [`WEIGHT`].
    pub weights: BTreeMap<String, u32>,
    /// The weights were seeded from the master plan's tracks table: from then on they're only
    /// the person's (an edit of the plan changes none).
    pub seeded: bool,
    pub updated_at: i64,
    /// Who changed them last.
    pub by: String,
    /// What the coordinator was last told of them, and whether it reached it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notice: Option<Notice>,
}

/// A message to a program's coordinator, kept until it's delivered (tried again every 15 s).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Notice {
    pub text: String,
    pub at: i64,
    /// kiln queued it for its session (the session, or its machine, took it), and the message's id.
    pub delivered: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Why it hasn't yet.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
}

/// A track's worker, as assigned, and the coordinator's goal under way on it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Assignment {
    pub agent: String,
    /// A goal of the coordinator's stack (its id), under way on the worker.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub goal: Option<String>,
    pub at: i64,
    pub by: String,
}

/// One change to programs' settings and assignments at a time (each is read, changed and kept
/// whole): a person's, the coordinator's, the weights' seeding and a notice's delivery. Taken
/// before the stacks' lock (`goals::CHANGING`), never while it's held.
pub(crate) static WRITING: async_lock::Mutex<()> = async_lock::Mutex::new(());

fn settings_key(program: &str) -> String {
    format!("programs/{program}/settings")
}
fn assignments_key(program: &str) -> String {
    format!("programs/{program}/assignments")
}
/// The thresholds kiln last set, by agent: one a person set since is theirs, and left.
fn thresholds_key(program: &str) -> String {
    format!("programs/{program}/thresholds")
}

/// A program, as the view shows it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Program {
    /// Its master plan's id.
    pub id: String,
    pub title: String,
    /// The tag its plans share (`critical-path`).
    pub tag: String,
    /// The agent its goals run on, when they do (`coordinator_id`), as kiln knows it now (none: its
    /// session isn't one kiln lists any more).
    pub coordinator: Option<Agent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coordinator_id: Option<String>,
    pub slots: u32,
    /// Tracks at work now (their worker on a goal of theirs).
    pub running: u32,
    pub settings: Settings,
    /// The fullest weekly window of its agents' accounts (the coordinator's and its workers'), and
    /// whose.
    pub usage: Option<Usage>,
    pub rule: Rule,
    /// Its goals that are a track's, met and in all; and the coordinator's goal under way.
    pub met: u32,
    pub total: u32,
    pub goal: Option<Goal>,
    pub tracks: Vec<Track>,
}

/// An agent, as the view names it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Agent {
    pub id: String,
    pub name: String,
    pub status: Status,
    /// Its session's pause: at a usage limit (`limit`) or its threshold (`threshold`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pause: Option<Pause>,
    /// Its threshold: its session pauses once a window is this full.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pause_at: Option<u8>,
    /// Its goal stack's state (`running`, `paused`, `done`) and why, when it has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stack: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

/// A session's pause, as the view says it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Pause {
    /// `limit` or `threshold`.
    pub reason: String,
    /// The window (`five_hour`, `seven_day`, …) and its label (`Session`, `Weekly`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Unix ms the window starts over.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub until: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Usage {
    /// Of its weekly window, 0–100.
    pub pct: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// Unix ms it starts over.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Goal {
    /// Its id in the stack it's kept in.
    pub id: String,
    /// `CP-T7.1`, when it has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    pub text: String,
    /// `active` (under way), `pending` (next).
    pub status: String,
    /// The agent it's under way on, and its name (a goal of the coordinator's, delegated).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on_name: Option<String>,
}

/// A track of a program.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Track {
    /// `T7`.
    pub id: String,
    pub name: String,
    pub plan: String,
    pub weight: u32,
    /// Its worker, as assigned (`assigned`: its id, known to kiln or not).
    pub worker: Option<Agent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assigned: Option<String>,
    pub goal: Option<Goal>,
    pub met: u32,
    pub total: u32,
    /// Its goals met, each adding 1/weight.
    pub pass: f64,
    /// `running` (at work), `next` (a free slot is its), `waiting`, `held` (next, but no new goal
    /// starts for usage), `paused` (its worker's session is paused), `done` (no goal left to go).
    pub slot: String,
    /// The threshold kiln holds its worker to: 95, or 80 for the lowest-weight track at work.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wants: Option<u8>,
    pub changes: Vec<Change>,
}

/// A change request of a track's, in words that say how far it went: `built` (open, pushed for
/// review), `merged` (landed), or `closed`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Change {
    pub title: String,
    pub url: String,
    pub branch: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub change: Option<String>,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checks: Option<String>,
}

// ------------------------------------------------------------------------------ the parts, pure

/// A track plan's number, from its `t<n>` tag.
pub fn track_number(tags: &[String]) -> Option<u32> {
    tags.iter().find_map(|t| t.strip_prefix('t').filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())).and_then(|n| n.parse().ok()))
}

/// A master plan's program tag: its first that isn't `master`.
pub fn program_tag(tags: &[String]) -> Option<String> {
    tags.iter().find(|t| t.as_str() != "master").cloned()
}

/// A track's name from its plan's title (`Critical path T1: Amphora (KilnDB made a product), …`:
/// `Amphora`); else from its tags.
pub fn track_name(title: &str, tags: &[String], tag: &str) -> String {
    let after = title.split_once(": ").map(|(_, rest)| rest);
    let name = after.map(|rest| rest.split([',', '(', ':']).next().unwrap_or(rest).trim().to_owned()).filter(|n| !n.is_empty() && n.chars().count() <= 40);
    name.or_else(|| tags.iter().find(|t| t.as_str() != tag && track_number(std::slice::from_ref(t)).is_none()).cloned()).unwrap_or_else(|| title.to_owned())
}

/// The weights a master plan's tracks table gives (a table with a `Weight` column, rows by track:
/// `| T1 Amphora | … | 5 | … |`), by track (`T1`): what a program's settings are seeded with.
pub fn plan_weights(body: &str) -> BTreeMap<String, u32> {
    let mut out = BTreeMap::new();
    let mut col: Option<usize> = None;
    for line in body.lines() {
        let line = line.trim();
        if !line.starts_with('|') {
            col = None;
            continue;
        }
        let cells: Vec<&str> = line.trim_matches('|').split('|').map(str::trim).collect();
        if let Some(i) = cells.iter().position(|c| c.eq_ignore_ascii_case("weight")) {
            col = Some(i);
            continue;
        }
        let (Some(i), Some(first)) = (col, cells.first()) else { continue };
        let Some(n) = track_of_cell(first) else { continue };
        if let Some(w) = cells.get(i).and_then(|w| w.parse::<u32>().ok()).filter(|w| (1..=WEIGHT_MAX).contains(w)) {
            out.insert(format!("T{n}"), w);
        }
    }
    out
}

/// `T7 Hex` (a table's first cell): 7.
fn track_of_cell(cell: &str) -> Option<u32> {
    let rest = cell.strip_prefix('T')?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() || rest.chars().nth(digits.len()).is_some_and(|c| !c.is_whitespace()) {
        return None;
    }
    digits.parse().ok()
}

/// A goal's key and track (`CP-T7.1 Hex set up; …`: (`CP-T7.1`, 7)): its first word, a prefix of
/// capitals, a dash, `T`, the track's number, a dot and the goal's.
pub fn goal_key(text: &str) -> Option<(String, u32)> {
    let word = text.split_whitespace().next()?;
    let (prefix, rest) = word.split_once('-')?;
    if prefix.is_empty() || !prefix.bytes().all(|b| b.is_ascii_uppercase()) {
        return None;
    }
    let (track, step) = rest.strip_prefix('T')?.split_once('.')?;
    let number = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if !number(track) || !number(step) {
        return None;
    }
    Some((word.to_owned(), track.parse().ok()?))
}

/// A key's track prefix (`CP-T7.1`: `CP-T7.`), what the track's other goals start with.
pub fn track_prefix(key: &str) -> Option<&str> {
    key.rfind('.').and_then(|i| key.get(..=i))
}

/// The highest share of a weekly window (`7d`) these limits say, and its account and reset.
pub fn weekly(limits: &[Limit]) -> Option<Usage> {
    limits
        .iter()
        .filter(|l| l.window.as_deref() == Some("7d") || l.id.starts_with("seven_day"))
        .filter_map(|l| l.used_percent.filter(|p| p.is_finite()).map(|p| Usage { pct: p.clamp(0.0, 100.0), account: l.account.clone(), resets_at: l.resets_at }))
        .max_by(|a, b| a.pct.total_cmp(&b.pct))
}

/// A track's pass: each of its goals met adds 1/weight.
pub fn pass(met: u32, weight: u32) -> f64 {
    f64::from(met) / f64::from(weight.max(1))
}

/// What the rotation needs of a track.
#[derive(Debug, Clone, PartialEq)]
pub struct Slotting {
    pub number: u32,
    pub weight: u32,
    pub pass: f64,
    /// At work on a goal of its (its worker on its goal under way), or paused mid-goal: it holds
    /// its slot.
    pub running: bool,
    /// None of its goals left to go.
    pub done: bool,
}

/// Each track's slot (in `tracks`' order): those at work keep theirs (a track is never stopped
/// mid-goal); the free slots go to the others not done, lowest pass first (then the heavier, then
/// the lower number): `next`; the rest `waiting`.
pub fn rotate(tracks: &[Slotting], slots: u32) -> Vec<&'static str> {
    let running = tracks.iter().filter(|t| t.running).count();
    let free = usize::try_from(slots).unwrap_or(usize::MAX).saturating_sub(running);
    let mut ready: Vec<&Slotting> = tracks.iter().filter(|t| !t.running && !t.done).collect();
    ready.sort_by(|a, b| a.pass.total_cmp(&b.pass).then(b.weight.cmp(&a.weight)).then(a.number.cmp(&b.number)));
    let next: Vec<u32> = ready.iter().take(free).map(|t| t.number).collect();
    tracks
        .iter()
        .map(|t| {
            if t.running {
                "running"
            } else if t.done {
                "done"
            } else if next.contains(&t.number) {
                "next"
            } else {
                "waiting"
            }
        })
        .collect()
}

/// The track at work (or paused mid-goal) whose worker pauses at 80% (the master plan's §5: the
/// lowest-weight one; then the one furthest ahead, then the higher number): its number.
pub fn lowest_weight(tracks: &[Slotting]) -> Option<u32> {
    tracks.iter().filter(|t| t.running).min_by(|a, b| a.weight.cmp(&b.weight).then(b.pass.total_cmp(&a.pass)).then(b.number.cmp(&a.number))).map(|t| t.number)
}

/// The threshold to set on a worker's session (`pause_at`), if it should change: kiln's (`want`)
/// when it has none, or still has the one kiln set last (`last`); one a person set since is
/// theirs, and kept.
pub fn threshold_change(now: Option<u8>, last: Option<u8>, want: Option<u8>) -> Option<Option<u8>> {
    if now == want {
        return None;
    }
    let ours = now.is_none() || now == last;
    ours.then_some(want)
}

// --------------------------------------------------------------------------- from kiln's records

/// A master plan: its id, title and tags, and the session its latest goals run went to.
struct Master {
    id: String,
    title: String,
    tags: Vec<String>,
    coordinator: Option<String>,
}

/// Master plans (tagged `master`), newest change first: their bodies aren't read (a program's
/// weights are its settings').
async fn masters(store: &crate::store::Store) -> anyhow::Result<Vec<Master>> {
    store
        .call(|c| {
            let mut st = c.prepare(
                "SELECT p.id, p.title, p.tags,
                        (SELECT r.session FROM plan_runs r WHERE r.plan_id = p.id AND r.mode = 'goals' AND r.session != '' ORDER BY r.at DESC LIMIT 1)
                 FROM plans p WHERE p.tags LIKE '%\"master\"%' ORDER BY p.updated_at DESC LIMIT ?1",
            )?;
            let rows = st.query_map(rusqlite::params![PROGRAMS_MAX], |r| {
                Ok(Master { id: r.get(0)?, title: r.get(1)?, tags: serde_json::from_str(&r.get::<_, String>(2)?).unwrap_or_default(), coordinator: r.get(3)? })
            })?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?.into_iter().filter(|m| m.tags.iter().any(|t| t == "master")).collect())
        })
        .await
}

/// A program's track plans: those with its tag and a `t<n>` tag, by number (id, title, tags).
async fn track_plans(store: &crate::store::Store, master: &str, tag: &str) -> anyhow::Result<Vec<(u32, String, String, Vec<String>)>> {
    // The tag as it's kept, quoted in the JSON list, `%`, `_` and `\` escaped for LIKE.
    let pattern = format!("%{}%", serde_json::to_string(tag)?.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"));
    let (master, tag) = (master.to_owned(), tag.to_owned());
    store
        .call(move |c| {
            let mut st = c.prepare("SELECT id, title, tags FROM plans WHERE id != ?1 AND tags LIKE ?2 ESCAPE '\\' ORDER BY updated_at DESC LIMIT ?3")?;
            let rows = st.query_map(rusqlite::params![master, pattern, TRACKS_MAX], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))?;
            let mut out: Vec<(u32, String, String, Vec<String>)> = Vec::new();
            for row in rows {
                let (id, title, tags) = row?;
                let tags: Vec<String> = serde_json::from_str(&tags).unwrap_or_default();
                if tags.contains(&tag) && !tags.iter().any(|t| t == "master")
                    && let Some(n) = track_number(&tags)
                {
                    out.push((n, id, title, tags));
                }
            }
            out.sort_by_key(|(n, ..)| *n);
            out.dedup_by_key(|(n, ..)| *n);
            Ok(out)
        })
        .await
}

async fn kv_json<T: for<'de> Deserialize<'de> + Default>(store: &crate::store::Store, key: String) -> anyhow::Result<T> {
    Ok(store.kv_get(key).await?.map(|t| serde_json::from_str(&t)).transpose()?.unwrap_or_default())
}

async fn kv_save<T: Serialize>(store: &crate::store::Store, key: String, v: &T) -> anyhow::Result<()> {
    store.kv_set(key, serde_json::to_string(v)?).await?;
    Ok(())
}

/// A program's settings, as kept (the defaults when none are).
pub async fn settings(hub: &Hub, program: &str) -> anyhow::Result<Settings> {
    kv_json(&hub.store, settings_key(program)).await
}

/// Keeps a program's settings.
pub async fn save_settings(hub: &Hub, program: &str, s: &Settings) -> anyhow::Result<()> {
    kv_save(&hub.store, settings_key(program), s).await
}

/// A program's assignments, by track (`T7`).
pub async fn assignments(store: &crate::store::Store, program: &str) -> anyhow::Result<BTreeMap<String, Assignment>> {
    kv_json(store, assignments_key(program)).await
}

pub async fn save_assignments(store: &crate::store::Store, program: &str, a: &BTreeMap<String, Assignment>) -> anyhow::Result<()> {
    kv_save(store, assignments_key(program), a).await
}

/// A program's settings, seeded once: its weights from its master plan's tracks table, read the
/// first time the program is (then only the person's).
async fn seeded(hub: &Hub, master: &str) -> anyhow::Result<Settings> {
    let s = settings(hub, master).await?;
    if s.seeded {
        return Ok(s);
    }
    // Seeded under the lock, from the settings as they are then: a person's change made meanwhile
    // is kept.
    let _w = WRITING.lock().await;
    let mut s = settings(hub, master).await?;
    if s.seeded {
        return Ok(s);
    }
    let id = master.to_owned();
    let body: Option<String> = hub.store.call(move |c| Ok(c.query_row("SELECT body FROM plans WHERE id = ?1", rusqlite::params![id], |r| r.get(0)).optional()?)).await?;
    let weights = plan_weights(body.as_deref().unwrap_or_default());
    let found = !weights.is_empty();
    for (t, w) in weights {
        s.weights.entry(t).or_insert(w);
    }
    s.seeded = true;
    if s.by.is_empty() {
        s.by = if found { "kiln (weights from the master plan's tracks table)".into() } else { "kiln (the master plan's tracks table gave no weights: each track weighs 1)".into() };
    }
    save_settings(hub, master, &s).await?;
    Ok(s)
}

/// The usage rule that holds for `agent`: its program's (it's a program's coordinator, or a
/// track's worker), or `Ok`. `agents` are every agent kiln knows, as they are now. A store that
/// can't be read leaves it `Ok`, said in the log: the driver goes on.
pub async fn rule(store: &crate::store::Store, agents: &[Info], agent: &str) -> Rule {
    match rule_of(store, agents, agent).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(%agent, error = %format!("{e:#}"), "program usage rule: not read");
            Rule::Ok
        }
    }
}

async fn rule_of(store: &crate::store::Store, agents: &[Info], agent: &str) -> anyhow::Result<Rule> {
    for m in masters(store).await? {
        let Some(coordinator) = m.coordinator else { continue };
        let workers: Vec<String> = assignments(store, &m.id).await?.into_values().map(|a| a.agent).collect();
        if agent != coordinator && !workers.iter().any(|w| w == agent) {
            continue;
        }
        let members = agents.iter().filter(|i| i.id == coordinator || workers.contains(&i.id));
        let pct = members.filter_map(|i| weekly(&i.limits)).map(|u| u.pct).max_by(f64::total_cmp);
        return Ok(Rule::at(pct));
    }
    Ok(Rule::Ok)
}

fn agent_of(info: &Info, stack: Option<&crate::goals::Stack>) -> Agent {
    // A stack with no goal left to go (none ever, or all ended or cleared) isn't shown.
    let stack = stack.filter(|s| s.items.iter().any(|i| matches!(i.status.as_str(), "pending" | "active")));
    Agent {
        id: info.id.clone(),
        name: info.name.clone(),
        status: info.status,
        pause: info.pause.as_ref().map(|p| Pause { reason: p.reason.clone(), window: p.window.clone(), label: p.label.clone(), until: p.until }),
        pause_at: info.pause_at,
        stack: stack.map(|s| s.state.clone()),
        reason: stack.and_then(|s| s.reason.clone()),
        usage: weekly(&info.limits),
    }
}

fn goal_of(i: &crate::goals::Item, names: &HashMap<&str, &str>) -> Goal {
    Goal {
        id: i.id.clone(),
        key: goal_key(&i.text).map(|(k, _)| k),
        text: i.text.clone(),
        status: i.status.clone(),
        on: i.on.clone(),
        on_name: i.on.as_deref().and_then(|o| names.get(o)).map(|n| (*n).to_owned()),
    }
}

/// Every program kiln runs, the critical path first.
pub async fn list(hub: &Hub) -> anyhow::Result<Vec<Program>> {
    let agents = hub.harness.list();
    let mut out = Vec::new();
    for m in masters(&hub.store).await? {
        out.push(build(hub, &m, &agents).await?);
    }
    // The critical path first; the rest as they came (newest change first).
    out.sort_by_key(|p| p.tag != "critical-path");
    Ok(out)
}

/// One program, by its master plan's id.
pub async fn get(hub: &Hub, id: &str) -> anyhow::Result<Option<Program>> {
    Ok(list(hub).await?.into_iter().find(|p| p.id == id))
}

/// The change records of a program's agents (its coordinator and its workers; kiln keeps one per
/// agent, `workspace::change`): one key each, so the read is as bounded as the tracks are.
async fn changes_of(store: &crate::store::Store, agents: &[&str]) -> anyhow::Result<Vec<crate::workspace::change::Record>> {
    let mut out: Vec<crate::workspace::change::Record> = Vec::new();
    for a in agents {
        if out.iter().any(|r| r.agent == *a) {
            continue;
        }
        let Some(text) = store.kv_get(crate::workspace::change::key(a)).await? else { continue };
        match serde_json::from_str(&text) {
            Ok(r) => out.push(r),
            Err(e) => tracing::warn!(agent = %a, error = %e, "program: an agent's change record isn't one kiln can read"),
        }
    }
    Ok(out)
}

async fn build(hub: &Hub, m: &Master, agents: &[Info]) -> anyhow::Result<Program> {
    let tag = program_tag(&m.tags).unwrap_or_default();
    let settings = seeded(hub, &m.id).await?;
    let slots = settings.slots.unwrap_or(SLOTS).clamp(1, SLOTS_MAX);
    let assigned = assignments(&hub.store, &m.id).await?;
    let by_id: HashMap<&str, &Info> = agents.iter().map(|i| (i.id.as_str(), i)).collect();
    let names: HashMap<&str, &str> = agents.iter().map(|i| (i.id.as_str(), i.name.as_str())).collect();
    let coordinator = m.coordinator.as_deref().and_then(|c| by_id.get(c).copied());
    // Its list of record, whether kiln knows the coordinator's session right now or not.
    let record = match m.coordinator.as_deref() {
        Some(c) => Some(crate::goals::get(hub, c).await.map_err(|e| anyhow::anyhow!("the coordinator's goal stack: {e}"))?),
        None => None,
    };
    let plans = if tag.is_empty() { Vec::new() } else { track_plans(&hub.store, &m.id, &tag).await? };
    let mut theirs: Vec<&str> = m.coordinator.as_deref().into_iter().collect();
    theirs.extend(assigned.values().map(|a| a.agent.as_str()));
    let changes = changes_of(&hub.store, &theirs).await?;

    let mut tracks = Vec::new();
    let mut slotting = Vec::new();
    let mut paused_rows = Vec::new();
    for (n, plan, title, tags) in &plans {
        let id = format!("T{n}");
        let weight = settings.weights.get(&id).copied().unwrap_or(WEIGHT).clamp(1, WEIGHT_MAX);
        let mine = |i: &&crate::goals::Item| goal_key(&i.text).is_some_and(|(_, t)| t == *n);
        let items: Vec<&crate::goals::Item> = record.as_ref().map(|r| r.items.iter().filter(mine).collect()).unwrap_or_default();
        let total = u32::try_from(items.len()).unwrap_or(u32::MAX);
        let met = u32::try_from(items.iter().filter(|i| i.status == "met").count()).unwrap_or(u32::MAX);
        let a = assigned.get(&id);
        // As assigned, whether kiln knows the agent right now or not (its machine away, say).
        let assignee = a.map(|a| a.agent.as_str()).filter(|a| !a.is_empty());
        let worker = assignee.and_then(|w| by_id.get(w).copied());
        let worker_stack = match assignee {
            Some(w) => Some(crate::goals::get(hub, w).await.map_err(|e| anyhow::anyhow!("{w}'s goal stack: {e}"))?),
            None => None,
        };
        // The goal under way: the coordinator's, under way on the worker; else the worker's own
        // (its stack's, of this track); else what comes next in the coordinator's.
        let delegated = items.iter().find(|i| i.status == "pending" && assignee.is_some() && i.on.as_deref() == assignee);
        let own = worker_stack.as_ref().and_then(|s| s.items.iter().filter(mine).find(|i| i.status == "active"));
        let next = items.iter().find(|i| i.status == "pending" && i.on.is_none());
        let goal = delegated.copied().or(own).or(next.copied()).map(|i| goal_of(i, &names));
        let under_way = delegated.is_some() || own.is_some();
        let paused = worker.is_some_and(|w| w.pause.is_some());
        // Its worker on its goal, at work or paused mid-goal: it holds its slot (and, the
        // lowest-weight one, its threshold of 80 while it's paused at it). An assignee kiln no
        // longer knows (its session closed: a machine that's away still has its sessions listed)
        // holds none; its row says so, its goal still out until it's assigned again.
        let holds = worker.is_some() && under_way;
        // Done: none of its goals is left to go (each met, or ended unmet or skipped).
        let done = total > 0 && items.iter().all(|i| !matches!(i.status.as_str(), "pending" | "active"));
        slotting.push(Slotting { number: *n, weight, pass: pass(met, weight), running: holds, done });
        paused_rows.push(paused && holds);
        let branch = format!("cp/t{n}-");
        let mut crs: Vec<Change> = changes
            .iter()
            .filter(|c| assignee == Some(c.agent.as_str()) || c.branch.to_ascii_lowercase().starts_with(&branch))
            .map(|c| Change {
                title: c.title.clone(),
                url: c.url.clone(),
                branch: c.branch.clone(),
                change: c.change.clone(),
                state: match c.state.as_str() {
                    "landed" => "merged".into(),
                    "open" => "built".into(),
                    other => other.to_owned(),
                },
                checks: c.checks.clone(),
            })
            .collect();
        crs.truncate(CHANGES_MAX);
        tracks.push(Track {
            id,
            name: track_name(title, tags, &tag),
            plan: plan.clone(),
            weight,
            worker: worker.map(|w| agent_of(w, worker_stack.as_ref())),
            assigned: a.map(|a| a.agent.clone()),
            goal,
            met,
            total,
            pass: pass(met, weight),
            slot: String::new(),
            wants: None,
            changes: crs,
        });
    }
    let members = coordinator.into_iter().chain(tracks.iter().filter_map(|t| t.worker.as_ref()).filter_map(|w| by_id.get(w.id.as_str()).copied()));
    let usage = members.filter_map(|i| weekly(&i.limits)).max_by(|a, b| a.pct.total_cmp(&b.pct));
    let rule = Rule::at(usage.as_ref().map(|u| u.pct));
    let lowest = lowest_weight(&slotting);
    for (((t, s), paused), slot) in tracks.iter_mut().zip(&slotting).zip(&paused_rows).zip(rotate(&slotting, slots)) {
        t.slot = match (slot, &rule) {
            (_, _) if *paused => "paused".into(),
            ("next", r) if r.holds_new_goals() => "held".into(),
            (s, _) => s.into(),
        };
        t.wants = t.worker.as_ref().map(|_| if lowest == Some(s.number) { LOWEST_PAUSE_AT } else { WORKER_PAUSE_AT });
    }
    let running = u32::try_from(tracks.iter().filter(|t| t.slot == "running").count()).unwrap_or(u32::MAX);
    Ok(Program {
        id: m.id.clone(),
        title: m.title.clone(),
        tag,
        coordinator: coordinator.map(|c| agent_of(c, record.as_ref())),
        coordinator_id: m.coordinator.clone(),
        slots,
        running,
        settings,
        usage,
        rule,
        met: tracks.iter().map(|t| t.met).sum(),
        total: tracks.iter().map(|t| t.total).sum(),
        goal: record.as_ref().and_then(|r| r.items.iter().find(|i| i.status == "active")).map(|i| goal_of(i, &names)),
        tracks,
    })
}

/// Holds each program's workers to their thresholds (`pause_at`: 95, or 80 for the lowest-weight
/// track at work), leaving one a person set; takes back kiln's from an agent no longer a worker;
/// and delivers a settings notice its coordinator hasn't had. The goal driver runs it every 15 s.
pub async fn enforce(hub: &Hub) {
    if let Err(e) = enforce_all(hub).await {
        tracing::warn!(error = %format!("{e:#}"), "programs: thresholds and notices not held this time");
    }
}

async fn enforce_all(hub: &Hub) -> anyhow::Result<()> {
    let agents = hub.harness.list();
    for p in list(hub).await? {
        let mut last: BTreeMap<String, u8> = kv_json(&hub.store, thresholds_key(&p.id)).await?;
        let mut wanted: BTreeMap<String, u8> = BTreeMap::new();
        for t in &p.tracks {
            if let (Some(w), Some(want)) = (&t.worker, t.wants) {
                wanted.insert(w.id.clone(), want);
            }
        }
        let mut changed = false;
        let seen: Vec<String> = last.keys().cloned().collect();
        for agent in wanted.keys().chain(seen.iter().filter(|a| !wanted.contains_key(*a))).cloned().collect::<Vec<_>>() {
            // A session that's gone: what kiln set on it is forgotten.
            let Some(info) = agents.iter().find(|i| i.id == agent) else {
                changed |= last.remove(&agent).is_some();
                continue;
            };
            let want = wanted.get(&agent).copied();
            match threshold_change(info.pause_at, last.get(&agent).copied(), want) {
                Some(set) => {
                    if matches!(hub.harness.command(hub, &agent, Command::PauseAt { percent: set }).await, crate::harness::registry::Sent::Ok) {
                        match set {
                            Some(v) => last.insert(agent.clone(), v),
                            None => last.remove(&agent),
                        };
                        changed = true;
                        tracing::info!(%agent, program = %p.id, threshold = ?set, "program worker's usage threshold set");
                    }
                }
                // No longer a worker, and its threshold a person's (or none): nothing of kiln's
                // left on it to take back.
                None if want.is_none() => changed |= last.remove(&agent).is_some(),
                None => {}
            }
        }
        if changed {
            kv_save(&hub.store, thresholds_key(&p.id), &last).await?;
        }
        if let (Some(c), Some(n)) = (&p.coordinator_id, p.settings.notice.as_ref().filter(|n| !n.delivered)) {
            let sent = deliver(hub, c, n.clone()).await;
            // Kept only if it's still the notice the settings hold: a change of the person's made
            // meanwhile (and its own notice) stands.
            let _w = WRITING.lock().await;
            let mut s = settings(hub, &p.id).await?;
            if s.notice.as_ref() == Some(n) {
                s.notice = Some(sent);
                save_settings(hub, &p.id, &s).await?;
            }
        }
    }
    Ok(())
}

/// Sends a notice to agent `to` (queued after its turn): delivered, with its message's id, once its
/// session takes it; else why not, to be tried again.
pub async fn deliver(hub: &Hub, to: &str, mut n: Notice) -> Notice {
    let message = crate::util::new_id();
    let send = Command::Send { id: message.clone(), text: n.text.clone(), mode: crate::harness::model::SendMode::Queue, attachments: Vec::new(), overrides: None };
    match hub.harness.command(hub, to, send).await {
        crate::harness::registry::Sent::Ok => {
            n.delivered = true;
            n.message = Some(message);
            n.why = None;
        }
        crate::harness::registry::Sent::NoSession => n.why = Some("kiln doesn't know the coordinator's session right now".into()),
        crate::harness::registry::Sent::Unreachable => n.why = Some("the coordinator's machine isn't connected right now".into()),
    }
    n
}

#[cfg(test)]
mod tests;
