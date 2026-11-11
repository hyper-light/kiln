//! Placement in a runner's node (docs/needs.md E5, docs/reach.md §2): the shell around kiln-ci's
//! pure core. The node a run is asked of owns it (`kiln_ci::place::owner::Owner`); the runner that
//! has the room admits it (`kiln_ci::place::runner::Admission`), by the engine's policy. Both live
//! here, in the node's one task, so a run asked of this machine for this machine is admitted with
//! no message at all, and nothing here waits on a lock.
//!
//! - **On the wire** (docs/wire.md, "Named features"): an owner's word goes in `Ask::Place`, a
//!   runner's in `Say::Place`, each `{"epoch": the sender's epoch, "spec": the run's spec id when it
//!   starts one, "msg": the core's message}` as JSON, and only between runners that named `place`.
//!   A runner from before is placed on as it was: the owner's `Fallback`.
//! - **Room** is measured, never assumed: the runner's admission admits against its machine's last
//!   measure, read on events (a run's start or end here, the OS's pressure, power and thermal
//!   events, the host sample's arrival) and never by a decision (W05-R26); without a sensor, each
//!   reading its host sampler feeds it.
//! - **Time** comes in: [`Placer::timer`] says when to be told it again, and the node sets a timer
//!   for then, not a poll.
//! - **No run starts twice**: a spec that comes for a run the admission didn't say `New` to is
//!   refused, never started; one that comes from an asker that doesn't place starts as before.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use kiln_ci::facts::RunnerFacts;
use kiln_ci::place::answer::Answer;
use kiln_ci::place::ask::{Ask, Peer, Sample};
use kiln_ci::place::msg::{How, ToOwner, ToRunner};
use kiln_ci::place::owner::{Act, Kept, Machine, Owner, Size};
use kiln_ci::place::runner::{Admission, Init, Record, Records, Settings, Start};
use kiln_seams::records::{Scan, TX_MAX, Tx, Write};
use kiln_ci::sched::state::{Cpus, Fence, Resources};

use super::directory::RunnerInfo;
use crate::proto::feature;
use kiln_ci::place::handoff::{Ballot, Handle, Word};

mod handoff;

/// The owner a CI leg's room is held for in a runner's admission (AUDIT M4, docs/needs.md E5):
/// the CI engine that assigned it. No runner can be named so (a `:` is in no host name).
pub const CI: &str = "ci:engine";

/// An agent session's claim on the runner it was placed on (AUDIT M4, docs/needs.md E5): the
/// owner that placed it (its node and epoch), its run there and the claim's fence. The server
/// gives it to that runner with the session (`harness::session::Spec::claim`); the runner holds
/// its room from the session's start to its end, in the same ledger as CI legs and commands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claim {
    pub owner: String,
    pub epoch: u64,
    pub run: String,
    pub fence: Fence,
}

/// What kiln's server asks a runner's node, as the owner of an agent session's start
/// (`ServerMsg::Place`, JSON): place `ask`, or let go of run `run` (the session didn't start).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum SessionAsk {
    Ask { ask: Box<Ask> },
    Cancel { run: String },
}

/// What the owner says back of an agent session's start (`AgentMsg::Placed`, JSON), as it
/// changes: placement's answer (queued, its place and why; never and why); `go`, to start it on
/// `runner` under `claim`; or `fallback`, no runner that places can start it, older ones could.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "word", rename_all = "snake_case")]
pub enum SessionWord {
    Answer { answer: Answer },
    Go { runner: String, claim: Claim },
    Fallback { runners: Vec<String> },
}

/// A runner's word `json` (`Say::Place`) as an owner from before `place.health` reads it
/// (AUDIT L1, L2): a reason it doesn't know (`Strained`, `Barred`, appended after it) is said as
/// `Draining`, which it does (it waits there, taking nothing new), rather than as a word it can't
/// read and drops; every other word as it is.
pub fn for_older_owner(json: String) -> String {
    use kiln_ci::place::msg::Reason;
    let Ok(mut e) = serde_json::from_str::<Envelope<ToOwner>>(&json) else { return json };
    match &mut e.msg {
        ToOwner::Waiting { reason, .. } if matches!(reason, Reason::Strained { .. } | Reason::Barred { .. }) => *reason = Reason::Draining,
        _ => return json,
    }
    serde_json::to_string(&e).unwrap_or(json)
}

/// Whether placement word `json` is one of room tokens' own (docs/lb.md "Room tokens"; LB08): a
/// runner's word to an owner alone (`fast_hint`) or an owner's give-back (`fast_release`), said only
/// to a peer whose link named `place.tokens`. Read off the op's tag, which serde writes first and
/// which no string inside the word can spell unescaped.
pub fn tokens_word(json: &str) -> bool {
    json.contains(r#""op":"fast_hint""#) || json.contains(r#""op":"fast_release""#)
}

/// A waiting reason's kind as `kiln_ci::place::msg::Reason` names it (the hint's quotes).
fn reason_kind(r: &kiln_ci::place::msg::Reason) -> &'static str {
    use kiln_ci::place::msg::Reason;
    match r {
        Reason::Room { .. } => "room",
        Reason::Kept { .. } => "kept",
        Reason::Line => "line",
        Reason::Whole { .. } => "whole",
        Reason::Draining => "draining",
        Reason::Away => "away",
        Reason::Strained { .. } => "strained",
        Reason::Barred { .. } => "barred",
    }
}

/// A waiting reason in words, as a refused leg says why.
fn reason_words(r: &kiln_ci::place::msg::Reason) -> String {
    use kiln_ci::place::msg::Reason;
    match r {
        Reason::Room { short } => {
            let dims: Vec<String> = short.iter().map(|s| format!("{:?}", s.dim).to_lowercase()).collect();
            if dims.is_empty() { "no room for it beside what kiln placed here".to_owned() } else { format!("too little {} free beside what kiln placed here", dims.join(", ")) }
        }
        Reason::Kept { .. } => "room here is kept for a run that waited first".to_owned(),
        Reason::Line => "runs ahead of it in this runner's line go first".to_owned(),
        Reason::Whole { jobs } => format!("it takes the runner whole, and {jobs} other pieces of kiln's work run here"),
        Reason::Draining => "this runner is draining".to_owned(),
        Reason::Away => "its owner's link is down".to_owned(),
        Reason::Strained { why } => format!("this machine is under strain: {why}"),
        Reason::Barred { why, .. } => why.clone(),
    }
}

/// A placement word's op and what it's about, read without the rest (LB03's keys for what's owed
/// on a link: the node reads them from every placement word, so nothing else is built).
#[derive(Deserialize)]
struct Head {
    msg: HeadMsg,
}

#[derive(Deserialize)]
struct HeadMsg {
    op: String,
    #[serde(default)]
    run: Option<String>,
    #[serde(default)]
    ask: Option<RunOnly>,
    #[serde(default)]
    handle: Option<Handle>,
}

#[derive(Deserialize)]
struct RunOnly {
    #[serde(default)]
    run: String,
}

/// The answer a placement word asks for at once, if it asks for one (LB03's detection on use,
/// `kiln_ci::detect`): a probe's first word, a take's hold, a sync's `Synced`; a hand-off's claim,
/// a run's members growing, a run's news. Named as [`answer_keys`] names what answers it.
pub fn owed_key(json: &str) -> Option<String> {
    let m = serde_json::from_str::<Head>(json).ok()?.msg;
    match m.op.as_str() {
        "probe" => m.ask.map(|a| format!("place:{}", a.run)),
        "take" => m.run.map(|r| format!("place:{r}")),
        "sync" => Some("place:".to_owned()),
        "claim" | "extend" | "orphan" => m.handle.map(|h| format!("hand:{}", h.adopted_id())),
        _ => None,
    }
}

/// What a placement word answers ([`owed_key`]): a refusal answers a claim, or the first owner's
/// own word about its run.
pub fn answer_keys(json: &str) -> Vec<String> {
    let Ok(Head { msg: m }) = serde_json::from_str::<Head>(json) else { return Vec::new() };
    match m.op.as_str() {
        "promise" | "extended" | "adopting" => m.handle.map(|h| format!("hand:{}", h.adopted_id())).into_iter().collect(),
        "refused" => m.handle.map(|h| vec![format!("hand:{}", h.adopted_id()), format!("place:{}", h.run)]).unwrap_or_default(),
        "synced" => vec!["place:".to_owned()],
        _ => m.run.map(|r| format!("place:{r}")).into_iter().collect(),
    }
}

/// A placement word on a link, either way.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope<M> {
    /// The sender's epoch: its node's, one past the last it used.
    pub epoch: u64,
    /// The spec id of the run a start is for (the `Ask::Run` that follows it on the link).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spec: Option<u64>,
    /// The ballot a word about a run goes under (LB03, `kiln_ci::place::handoff`): the claim its
    /// sender adopted it, or claimed it back, by; none, its first owner's. Appended: optional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ballot: Option<Ballot>,
    /// A probe's run's members, those being added among them (LB03): every runner that holds the
    /// run knows them all, to name them should it hold the news. Appended: optional.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<String>,
    pub msg: M,
}

/// What a local run's asker hears from placement.
#[derive(Debug, Clone, PartialEq)]
pub enum Placed {
    /// Its answer, the first and each change after.
    Answer(Answer),
    /// It's to start on `runner` under claim `fence`: copy its tree there, then start it.
    Go { runner: String, fence: Fence },
    /// No runner that places can start it now; older ones could: pick among them as before.
    Fallback { runners: Vec<String> },
}

/// Placement's hard state in its node's records (LB03, W05-R25; docs/reach.md §2.3): the owner's
/// runs ([`Kept`], with the agent session that asked, by run), the runner's record of what it
/// started (by owner, the owner's epoch and run, with the ask it was started for), and the first of
/// the runner's epochs whose records are kept.
pub const OWN: &str = "place.own";
pub const RAN: &str = "place.ran";
pub const META: &str = "place.meta";
const SINCE: &[u8] = b"since";
/// `forgot` NUL owner: this runner's epoch as it let go of that owner's ended records.
const FORGOT: &[u8] = b"forgot\0";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OwnRow {
    kept: Kept,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    asker: Option<super::told::Answers>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RanRow {
    record: Record,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ask: Option<Ask>,
}

/// A runner record's key: its owner, NUL, its epoch (8 bytes, big-endian), its run.
fn ran_key(owner: &str, epoch: u64, run: &str) -> Vec<u8> {
    let mut k = owner.as_bytes().to_vec();
    k.push(0);
    k.extend_from_slice(&epoch.to_be_bytes());
    k.extend_from_slice(run.as_bytes());
    k
}

fn ran_of(key: &[u8]) -> Option<(String, u64, String)> {
    let nul = key.iter().position(|b| *b == 0)?;
    let owner = std::str::from_utf8(key.get(..nul)?).ok()?.to_owned();
    let epoch = u64::from_be_bytes(key.get(nul.checked_add(1)?..nul.checked_add(9)?)?.try_into().ok()?);
    let run = std::str::from_utf8(key.get(nul.checked_add(9)?..)?).ok()?.to_owned();
    Some((owner, epoch, run))
}

/// What a node's records held for placement as it started ([`load`]).
#[derive(Debug, Default)]
pub struct Loaded {
    own: Vec<OwnRow>,
    ran: Vec<((String, u64, String), RanRow)>,
    since: Option<u64>,
    forgot: BTreeMap<String, u64>,
    /// Budgets' rows (LB08, `super::budget::FAMILY`), as kept.
    budget: Vec<Vec<u8>>,
    /// The promises its runner gave (LB03, [`handoff::HAND`]).
    hand: Vec<(Handle, kiln_ci::place::handoff::Row)>,
    /// Its runs' members, as their owner (LB03, [`handoff::MEMBERS`]).
    members: Vec<(String, Vec<(String, bool)>)>,
}

impl Loaded {
    pub fn runs(&self) -> usize {
        self.own.len()
    }
}

/// Every row of `family`, a page at a time.
async fn rows(r: &dyn kiln_seams::Records, family: &str) -> anyhow::Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let mut out = Vec::new();
    let mut after: Option<Vec<u8>> = None;
    loop {
        let page = r.scan(family, Scan { prefix: Vec::new(), after: after.clone(), limit: kiln_seams::records::SCAN_MAX, at: kiln_seams::records::Consistency::Linearizable }).await.map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let more = page.more;
        after = page.rows.last().map(|row| row.key.clone());
        out.extend(page.rows.into_iter().map(|row| (row.key, row.value)));
        if !more || after.is_none() {
            return Ok(out);
        }
    }
}

/// What this node's records hold for placement (LB03): read once, as the node starts, before any
/// placement word. A row that doesn't read is skipped, named in the log.
pub async fn load(r: &dyn kiln_seams::Records) -> anyhow::Result<Loaded> {
    let mut l = Loaded::default();
    for (k, v) in rows(r, OWN).await? {
        match serde_json::from_slice::<OwnRow>(&v) {
            Ok(row) => l.own.push(row),
            Err(e) => tracing::warn!(run = %String::from_utf8_lossy(&k), error = %e, "a run this node owned that doesn't read: left out"),
        }
    }
    for (k, v) in rows(r, RAN).await? {
        match (ran_of(&k), serde_json::from_slice::<RanRow>(&v)) {
            (Some(key), Ok(row)) => l.ran.push((key, row)),
            _ => tracing::warn!("a record of a run started here that doesn't read: left out"),
        }
    }
    for (k, v) in rows(r, handoff::MEMBERS).await? {
        match (String::from_utf8(k), serde_json::from_slice::<Vec<(String, bool)>>(&v)) {
            (Ok(run), Ok(m)) => l.members.push((run, m)),
            _ => tracing::warn!("a run's members this owner kept that don't read: left out"),
        }
    }
    for (k, v) in rows(r, handoff::HAND).await? {
        match (handoff::hand_of(&k), serde_json::from_slice::<kiln_ci::place::handoff::Row>(&v)) {
            (Some(h), Ok(row)) => l.hand.push((h, row)),
            _ => tracing::warn!("a promise this runner gave that doesn't read: left out"),
        }
    }
    l.budget = rows(r, super::budget::FAMILY).await?.into_iter().map(|(_, v)| v).collect();
    let epoch_of = |v: &[u8]| v.get(..8).and_then(|b| b.try_into().ok()).map(u64::from_be_bytes);
    for (k, v) in rows(r, META).await? {
        if k == SINCE {
            l.since = epoch_of(&v);
        } else if let (Some(owner), Some(e)) = (k.strip_prefix(FORGOT).and_then(|o| std::str::from_utf8(o).ok()), epoch_of(&v)) {
            l.forgot.insert(owner.to_owned(), e);
        }
    }
    Ok(l)
}

/// What the node is to do for placement.
#[derive(Debug, Clone, PartialEq)]
pub enum Out {
    /// Send `Ask::Place { json }` to runner `peer`.
    Tell { peer: String, json: String },
    /// Send `Say::Place { json }` to the owner `peer` (on its session's link).
    Say { peer: String, json: String },
    /// Tell the local run `run`'s asker.
    Local { run: String, placed: Placed },
    /// Tell the agent session that asked for a run what it's owed (LB03): a run taken up from
    /// before this node's restart, whose asker's own channel went with that process.
    Told { asker: super::told::Answers, answer: Answer },
}

/// What came of a spec a placing asker sent ([`Placer::spec`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Spec {
    /// Start it: the admission said so, for run `run`.
    Start { run: String },
    /// Don't: it ran here already, or its claim isn't live (why).
    Refuse(String),
    /// Not placed: an asker from before placement, or a run it didn't place; as before.
    Unplaced,
}

/// Placement in one node: its owner and its runner's admission.
pub struct Placer {
    me: String,
    epoch: u64,
    owner: Owner,
    admission: Admission,
    /// Specs a placing asker's start was answered for: by (asker, spec id), the run and whether to
    /// start it.
    specs: BTreeMap<(String, u64), (String, u64, Result<(), String>)>,
    /// Runs started here for an owner, by (asker, spec id): its epoch and run.
    started: BTreeMap<(String, u64), (u64, String)>,
    /// The last directory, for the owner's machines.
    machines: Vec<Machine>,
    /// This runner's own facts and labels, as its owner's machine.
    facts: RunnerFacts,
    labels: Vec<String>,
    /// Owners whose link went down: back with their next word.
    away: std::collections::BTreeSet<String>,
    /// The runners this node's owner has told something: linked to again when the server is back,
    /// so what waits there hears where it stands.
    told_to: std::collections::BTreeSet<String>,
    /// Its admission's hint as last said (the `offers` feature, AUDIT O1), and the runs of this
    /// node's own that wait (as the owner's answers say) and those no longer waiting since.
    hinted: Option<crate::offers::hint::Hint>,
    waits: BTreeMap<String, crate::offers::hint::Wait>,
    gone: Vec<String>,
    waits_moved: bool,
    /// Its next hint is the whole of what waits (the server's first since it connected).
    whole: bool,
    /// What it measures of its machine on events (AUDIT L1, L2; none: it admits against the
    /// readings it's fed), the last measure (what its decisions read: W05-R26), and whether its
    /// machine is under strain, why.
    sensor: Option<crate::sense::Sensor>,
    last: Option<crate::sense::Measure>,
    strained: Option<String>,
    /// Probes its health bars (L2), by owner and epoch: answered so, and probed once a measure
    /// shows the cause cleared.
    parked: Vec<(String, u64, std::sync::Arc<Ask>)>,
    /// The groups of the placed runs here that run, by (owner, epoch, run): what each uses is
    /// read from its own group when the machine is, and taken out of what the machine has in use,
    /// so the room a run holds isn't counted again as others' load (docs/needs.md E5).
    groups: std::collections::HashMap<(String, u64, String), Held>,
    /// The kind of work each probe here is, as its asker keyed it (`Ask::key`), by (owner,
    /// epoch, run); and each asker's key as this runner's own runs of it are keyed (a command's
    /// key is its folder's and text's, and its folder here isn't its asker's): sense's health
    /// bars work here by the latter (AUDIT L2).
    asked_keys: std::collections::HashMap<(String, u64, String), String>,
    aliases: std::collections::HashMap<String, String>,
    /// Where its hard state is kept (LB03): this node's records, and what's changed and not kept
    /// yet, the newest of each row (`None`: it goes); the answers told whose rows go once what told
    /// them is kept; the agent session that asked for each run of its own that one did; and the runs
    /// taken up from before a restart.
    records: Option<std::sync::Arc<dyn kiln_seams::Records>>,
    unkept: BTreeMap<(&'static str, Vec<u8>), Option<Vec<u8>>>,
    to_deliver: Vec<String>,
    askers: BTreeMap<String, super::told::Answers>,
    restored: std::collections::BTreeSet<String>,
    /// The nodes in the last directory, and those this runner holds records for since it started:
    /// one gone from the directory is forgotten as an owner (LB03).
    known: std::collections::BTreeSet<String>,
    /// Its pace as sense last told it (W05-R26, `crate::sense::pace`): carried in its words.
    pace: kiln_ci::pace::Pace,
    /// Draining as the server says (an upgrade pending, or retiring), and as its battery does
    /// (why), the two together its admission's.
    draining: bool,
    unplugged: Option<String>,
    /// What this runner says of its topology in its next hint (LB08): set as it's read, sent only
    /// with a hint its admission makes due.
    pub topo: Option<crate::reach::topo::Word>,
    /// What this node's owner is given for each runner it might pick (LB08, `crate::reach::topo`):
    /// latency, rate and outages, measured or estimated.
    pub links: BTreeMap<String, kiln_ci::topo::Link>,
    /// Tenant budgets (LB08, `super::budget`): runs held back at their door before they're owned.
    budget: super::budget::Budget,
    /// Its part in hand-offs of runs whose owner can't be reached (LB03, [`handoff`]).
    hand: handoff::Hand,
}

/// Where a placed run's processes are counted, and its CPU time as last read with when.
/// Read through its cgroup, on Linux alone: elsewhere it's kept and never read.
#[derive(Debug, Clone)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
struct Held {
    at: Group,
    cpu: Option<(u64, std::time::Instant)>,
}

/// A placed run's group: a folder (a command's, found by its process; a CI leg's job group), or
/// an agent session's, found by its id as it reads (its group comes once its harness starts).
#[derive(Debug, Clone)]
pub enum Group {
    Dir(std::path::PathBuf),
    Session(String),
}

/// What a placed run uses now, read from its group (Linux; elsewhere nothing is read, and the run
/// counts as the machine's load as before): CPUs, its `cpu.stat` time over the time since its last
/// read, the same interval the machine's own idle share covers (both read in one measure);
/// memory, its anonymous memory (cgroup v2 `memory.stat` `anon`, what `MemAvailable` doesn't
/// count free; page cache is reclaimable and counted free there too), or, where its group has no
/// memory controller, the anonymous resident memory of its processes (`/proc/<pid>/status`
/// `RssAnon`).
fn group_use(h: &mut Held) -> Resources {
    #[cfg(target_os = "linux")]
    {
        let dir = match &h.at {
            Group::Dir(d) => d.clone(),
            Group::Session(id) => match crate::os::contain::Group::existing(id) {
                Some(g) => g.dir().to_path_buf(),
                None => return Resources::default(),
            },
        };
        let read = |n: &str| std::fs::read_to_string(dir.join(n)).ok();
        let now = std::time::Instant::now();
        let cpu_us = read("cpu.stat").and_then(|t| crate::sense::parse::keyed(&t, "usage_usec"));
        let milli = match (cpu_us, h.cpu) {
            (Some(c), Some((c0, t0))) => {
                let span = u64::try_from(now.duration_since(t0).as_micros()).unwrap_or(u64::MAX);
                u64::try_from(u128::from(c.saturating_sub(c0)).saturating_mul(1000) / u128::from(span.max(1))).unwrap_or(u64::MAX)
            }
            _ => 0,
        };
        h.cpu = cpu_us.map(|c| (c, now));
        let mem = read("memory.stat").and_then(|t| crate::sense::parse::keyed(&t, "anon")).unwrap_or_else(|| procs_in(&dir).into_iter().filter_map(rss_anon).sum());
        Resources { cpu: Cpus::milli(milli), mem, disk: 0, gpu: 0 }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = h;
        Resources::default()
    }
}

/// Every process in group `dir` and the groups under it.
#[cfg(target_os = "linux")]
fn procs_in(dir: &std::path::Path) -> Vec<u32> {
    let mut out: Vec<u32> = std::fs::read_to_string(dir.join("cgroup.procs")).map(|t| t.lines().filter_map(|l| l.trim().parse().ok()).collect()).unwrap_or_default();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten().filter(|e| e.file_type().is_ok_and(|t| t.is_dir())) {
            out.extend(procs_in(&e.path()));
        }
    }
    out
}

/// Process `pid`'s anonymous resident memory, bytes (`/proc/<pid>/status` `RssAnon`, in kB).
#[cfg(target_os = "linux")]
fn rss_anon(pid: u32) -> Option<u64> {
    let t = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let kb: u64 = t.lines().find_map(|l| l.strip_prefix("RssAnon:"))?.trim().trim_end_matches("kB").trim().parse().ok()?;
    kb.checked_mul(1024)
}

/// Placement's epoch for the node whose state lives in `dir`: one past the last it used, kept in
/// `dir/place-epoch` (docs/needs.md §8: a claim from before a restart is never taken for one after).
/// It's on disk before it's returned, so before anything is said in it (W05-R34, LB03): written to
/// a new file, that file synced, renamed over the old, and the folder synced, so a crash leaves the
/// old epoch or the new, never one lower and never a file half written. One that can't be kept is an
/// error: placement doesn't start in an epoch a restart could take up again.
pub fn next_epoch(dir: &std::path::Path) -> anyhow::Result<u64> {
    next_epoch_on(&EpochFs(dir))
}

/// The steps keeping an epoch takes on its disk, as POSIX makes them durable: a file's bytes once
/// it's synced, a rename once its folder is ([`EpochFs`]; a test's disk loses the rest when it
/// "crashes").
trait EpochDisk {
    fn read(&self) -> Option<String>;
    fn write_new(&self, s: &str) -> std::io::Result<()>;
    fn sync_new(&self) -> std::io::Result<()>;
    fn rename_new(&self) -> std::io::Result<()>;
    fn sync_dir(&self) -> std::io::Result<()>;
}

struct EpochFs<'a>(&'a std::path::Path);

impl EpochFs<'_> {
    fn path(&self) -> std::path::PathBuf {
        self.0.join("place-epoch")
    }

    fn new_path(&self) -> std::path::PathBuf {
        self.0.join("place-epoch.new")
    }
}

impl EpochDisk for EpochFs<'_> {
    fn read(&self) -> Option<String> {
        std::fs::read_to_string(self.path()).ok()
    }

    fn write_new(&self, s: &str) -> std::io::Result<()> {
        std::fs::write(self.new_path(), s)
    }

    fn sync_new(&self) -> std::io::Result<()> {
        std::fs::File::open(self.new_path())?.sync_all()
    }

    fn rename_new(&self) -> std::io::Result<()> {
        std::fs::rename(self.new_path(), self.path())
    }

    /// The folder's entry for the rename, synced (Unix; Windows renames through its journal, and
    /// the file itself is synced again).
    fn sync_dir(&self) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            std::fs::File::open(self.0)?.sync_all()
        }
        #[cfg(not(unix))]
        {
            std::fs::OpenOptions::new().write(true).open(self.path())?.sync_all()
        }
    }
}

#[cfg(test)]
thread_local! {
    /// A mistake planted for the epoch's test: its writes left unsynced.
    static EPOCH_UNSYNCED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn next_epoch_on(d: &impl EpochDisk) -> anyhow::Result<u64> {
    let last = d.read().and_then(|s| s.trim().parse::<u64>().ok()).unwrap_or(0);
    let next = last.saturating_add(1);
    d.write_new(&next.to_string())?;
    #[cfg(test)]
    if EPOCH_UNSYNCED.with(std::cell::Cell::get) {
        d.rename_new()?;
        return Ok(next);
    }
    d.sync_new()?;
    d.rename_new()?;
    d.sync_dir()?;
    Ok(next)
}

/// What sense measured as placement's sample: the machine (its CPUs, memory, work volume) and
/// what's in use of it, by everything. A share its interval was too short to show (no CPU tick
/// passed) is the last measure's.
pub fn sample_of_measure(m: &crate::sense::Measure, last: Option<&crate::sense::Measure>) -> Sample {
    let idle = m.cpu_idle.or_else(|| last.and_then(|l| l.cpu_idle)).unwrap_or(1.0);
    let busy = ((1.0 - idle.clamp(0.0, 1.0)) * f64::from(m.cpus) * 1000.0).round();
    let busy = if busy.is_finite() && busy >= 0.0 { busy as u64 } else { 0 };
    let (disk_total, disk_free) = m.space.map_or((0, 0), |s| (s.total, s.free));
    Sample {
        size: Resources { cpu: Cpus::whole(u64::from(m.cpus)), mem: m.mem_total, disk: disk_total, gpu: 0 },
        used: Resources { cpu: Cpus::milli(busy), mem: m.mem_total.saturating_sub(m.mem_available), disk: disk_total.saturating_sub(disk_free), gpu: 0 },
        ..Sample::default()
    }
}

/// Why this runner's health bars `ask`'s kind of work now (L2, sense's `Health::check` but its
/// strain, which the admission keeps), its kind `key` as this runner keys its own runs of it:
/// every run works in the work volume, so a full one, or its device's errors rising, bars all
/// work; a run of its kind killed for memory here, or missing a tool here, bars it whatever it
/// asks; memory and tools as asked bar the rest.
/// How a run that exited here ended, as this runner judges it when it ends (docs/lb.md "Good
/// places"), from its exit alone (`code`; `error`: it couldn't start, or `timed_out`, its own
/// limit ran out). It went well when it exited 0. The node let it down when it couldn't start
/// here, or exited as a shell does of a program that isn't here (`sense::health::classify`'s
/// missing tool). Any other failure has no cause this runner knows from its exit: the node's
/// fault or the work's own, which its owner settles by where its kind's next run goes and what
/// becomes of it. A run a signal ended (its asker stopped it, as far as an exit says) is judged
/// by no one.
pub fn how(code: Option<i32>, error: Option<&str>, timed_out: bool) -> How {
    use crate::sense::health::{Ending, Failure, classify};
    match (code, error) {
        (_, Some(_)) if timed_out => How::Unsure,
        (_, Some(_)) => How::LetDown,
        (Some(0), None) => How::Well,
        (Some(c), None) => match classify(&Ending { exit_code: Some(c), ..Ending::default() }) {
            Some(Failure::MissingTool { .. }) => How::LetDown,
            _ => How::Unsure,
        },
        (None, None) => How::Unsaid,
    }
}

fn barred(ask: &Ask, key: Option<String>, m: &crate::sense::Measure) -> Option<crate::sense::health::Refusal> {
    let tools = ask.needs.requires.iter().filter_map(|r| r.strip_prefix("tools has ")).map(|t| t.trim_matches(|c| c == '\'' || c == '"').to_owned()).collect();
    let a = crate::sense::health::Ask { key, disk: ask.needs.request.disk.max(1), mem: ask.needs.request.mem, tools };
    let h = crate::sense::health::runner().lock().ok()?;
    // Its strain is the admission's (it waits, saying so): only the causes its runs met bar here.
    h.check(&a, m, 0.0).err().filter(|r| r.kind != "strained")
}

/// The host sampler's reading as placement's sample: the machine and what's in use of it.
pub fn sample_of(s: &crate::metrics::HostSample) -> Sample {
    let busy = (f64::from(s.cpu_pct.clamp(0.0, 100.0)) * f64::from(s.cpus) * 10.0).round();
    let busy = if busy.is_finite() && busy >= 0.0 { busy as u64 } else { 0 };
    Sample {
        size: Resources { cpu: Cpus::whole(u64::from(s.cpus)), mem: s.mem_total, disk: s.disk_total, gpu: 0 },
        used: Resources { cpu: Cpus::milli(busy), mem: s.mem_used, disk: s.disk_used, gpu: 0 },
        ..Sample::default()
    }
}

/// A directory entry as the owner's machine: its facts, its size as measured (none until it is),
/// and whether it places.
pub fn machine_of(r: &RunnerInfo) -> Machine {
    let facts = r.facts.as_deref().cloned().unwrap_or_else(|| RunnerFacts { os: r.os.clone(), arch: r.arch.clone(), ..RunnerFacts::default() });
    let size = Size { cpu: (r.cpus > 0).then(|| Cpus::whole(u64::from(r.cpus))), mem: (r.mem_total > 0).then_some(r.mem_total), disk: None, gpu: r.facts.as_ref().map(|f| u32::try_from(f.gpus.len()).unwrap_or(u32::MAX)) };
    Machine { name: r.name.clone(), facts, labels: r.labels.clone(), size, epoch: 0, connected: r.connected && r.reach, places: feature::speaks(&r.features, feature::PLACE), idle: None, syncs: feature::speaks(&r.features, feature::PLACE_SYNC), fast: feature::speaks(&r.features, feature::FAST) }
}

impl Placer {
    /// Its hard state kept in `records` from now on (LB03): this node's own records, through
    /// kiln-seams' `Records` (a host's file of them, [`crate::records::open`]).
    pub fn keep_in(&mut self, records: std::sync::Arc<dyn kiln_seams::Records>) {
        self.owner.keeping(true);
        self.records = Some(records);
    }

    pub fn records(&self) -> Option<std::sync::Arc<dyn kiln_seams::Records>> {
        self.records.clone()
    }

    /// What this node's process before this one kept (LB03), taken up before any word. The owner's
    /// runs: each answer still owed told again (to the agent session that asked, through told), the
    /// rest asked again or held where their start went once the directory comes, and every runner
    /// that answers `Sync` asked what it started. The runner's records: the first epoch they're kept
    /// from is the one before this one's first; a run they have running goes on only if it's an agent
    /// session (its harness continues it), its room held again; any other (a command, which can't
    /// be followed on past the process that ran it) ended, lost with that process. A CI leg's room
    /// is never kept: its owner, the server's CI engine, keeps its grants itself (docs/needs.md §8).
    pub fn restore(&mut self, loaded: Loaded, now: i64) -> Vec<Out> {
        let Loaded { own, ran, since, forgot, budget, hand, members } = loaded;
        let fresh = since.is_none();
        let since = since.or(Some(self.epoch));
        if fresh {
            self.unkept.insert((META, SINCE.to_vec()), Some(self.epoch.to_be_bytes().to_vec()));
        }
        let mut records = Records::new();
        self.known.extend(ran.iter().map(|((owner, _, _), _)| owner.clone()));
        for ((owner, epoch, run), row) in &ran {
            records.entry(owner.clone()).or_default().entry(*epoch).or_default().insert(run.clone(), row.record);
        }
        self.admission.restore_records(records, since, forgot);
        self.admission.restore_record_asks(ran.iter().filter_map(|((owner, epoch, run), row)| Some(((owner.clone(), *epoch, run.clone()), row.ask.clone()?))));
        for ((owner, epoch, run), row) in ran {
            if row.record.ended.is_some() {
                continue;
            }
            let session = row.ask.as_ref().is_some_and(|a| a.key.as_deref().is_some_and(|k| k.starts_with("session:")));
            let adopted = match row.ask {
                Some(ask) if session => self.admission.adopt(Peer { name: &owner, epoch }, std::sync::Arc::new(ask), now),
                _ => false,
            };
            if !adopted {
                self.admission.lose(&owner, epoch, &run);
            }
        }
        let mut kept = Vec::with_capacity(own.len());
        for row in own {
            self.restored.insert(row.kept.ask.run.clone());
            if let Some(a) = row.asker {
                self.askers.insert(row.kept.ask.run.clone(), a);
            }
            kept.push(row.kept);
        }
        // Budgets before any word (LB08): a reservation whose run the owner didn't keep is let go.
        let owned: std::collections::BTreeSet<String> = kept.iter().map(|k| k.ask.run.clone()).collect();
        let (askers, b) = self.budget.restore(&self.me.clone(), budget, &owned, now);
        self.askers.extend(askers);
        let mut out = self.budget_outs(b, now);
        // Its runs' members (LB03), before they're asked again: a run it kept grows past them only
        // with their word. A run not kept is done with: its row goes.
        for (run, m) in members {
            if owned.contains(&run) {
                self.hand.members.restore(&run, m);
            } else {
                self.unkept.insert((handoff::MEMBERS, run.into_bytes()), None);
            }
        }
        let acts = self.owner.restore(kept);
        out.extend(self.acts(acts, now));
        // Its promises (LB03), and its own claims made again.
        out.extend(self.hand_restore(hand, now));
        out
    }

    /// The person's budgets as the server sends them with the directory (LB08): runs held back
    /// for them, placed as rights come.
    pub fn budgets(&mut self, published: Option<&serde_json::Value>, runners: &[RunnerInfo], now: i64) -> Vec<Out> {
        let b = self.budget.declared(published, runners, now);
        self.budget_outs(b, now)
    }

    /// What budgets said, as the node's: the asks they released placed here now, together (they
    /// arrive in one step: the owner's auction, once declared, places them jointly; LB08), their
    /// changes noted to be kept before any of it is published.
    fn budget_outs(&mut self, (asks, mut out): (Vec<Ask>, Vec<Out>), now: i64) -> Vec<Out> {
        if asks.len() > 1 {
            out.extend(self.ask_many_now(asks, now));
        } else {
            for a in asks {
                out.extend(self.ask_now(a, now));
            }
        }
        self.note_changes();
        out
    }

    /// Whether anything placement changed waits to be kept.
    pub fn unkept(&self) -> bool {
        !self.unkept.is_empty()
    }

    /// What placement changed since it was last kept, as transactions for this node's records, in
    /// order, at most [`TX_MAX`] writes each (one, but for a change of more runs than one takes):
    /// committed before anything they came with is published (LB03). The answers told with them
    /// are owed no more from then on: their rows go in what's kept next.
    pub fn take_unkept(&mut self) -> Vec<Tx> {
        let writes: Vec<Write> = std::mem::take(&mut self.unkept)
            .into_iter()
            .map(|((family, key), v)| match v {
                Some(value) => Write::Put { family: family.to_owned(), key, value },
                None => Write::Delete { family: family.to_owned(), key },
            })
            .collect();
        for run in std::mem::take(&mut self.to_deliver) {
            self.owner.delivered(&run);
            self.restored.remove(&run);
        }
        self.note_changes();
        writes.chunks(TX_MAX).map(|w| Tx { conds: Vec::new(), writes: w.to_vec() }).collect()
    }

    /// Transactions that couldn't be kept, put back under what changed since.
    pub fn unkept_back(&mut self, txs: Vec<Tx>) {
        for w in txs.into_iter().flat_map(|t| t.writes) {
            let (k, v) = match w {
                Write::Put { family, key, value } => ((family, key), Some(value)),
                Write::Delete { family, key } => ((family, key), None),
            };
            let family: &'static str = match k.0.as_str() {
                OWN => OWN,
                RAN => RAN,
                super::budget::FAMILY => super::budget::FAMILY,
                handoff::HAND => handoff::HAND,
                handoff::MEMBERS => handoff::MEMBERS,
                _ => META,
            };
            self.unkept.entry((family, k.1)).or_insert(v);
        }
    }

    /// The changes the owner and the admission made, noted to be kept. A CI leg's record isn't kept.
    fn note_changes(&mut self) {
        if self.records.is_none() {
            // Nothing's kept: what budgets changed goes nowhere.
            self.budget.note(&mut BTreeMap::new());
            // Nothing is kept: its promises and its runs' members live with its process.
            drop(self.hand.acc.changes());
            drop(self.hand.members.changes());
            return;
        }
        self.budget.note(&mut self.unkept);
        self.hand_changes();
        for (run, k) in self.owner.kept() {
            let v = k.and_then(|kept| serde_json::to_vec(&OwnRow { kept, asker: self.askers.get(run.as_ref()).cloned() }).ok());
            if v.is_none() {
                self.askers.remove(run.as_ref());
            }
            self.unkept.insert((OWN, run.as_bytes().to_vec()), v);
        }
        for (owner, epoch, run, rec, ask) in self.admission.record_changes() {
            if owner == CI {
                continue;
            }
            let v = rec.and_then(|record| serde_json::to_vec(&RanRow { record, ask: ask.map(|a| (*a).clone()) }).ok());
            self.unkept.insert((RAN, ran_key(&owner, epoch, &run)), v);
        }
        for (owner, epoch) in self.admission.forgot_changes() {
            let mut k = FORGOT.to_vec();
            k.extend_from_slice(owner.as_bytes());
            self.unkept.insert((META, k), Some(epoch.to_be_bytes().to_vec()));
        }
    }

    /// Its start under `fence` couldn't be kept (LB03): it isn't said, and the run waits again.
    pub fn unstarted(&mut self, run: &str, now: i64) -> Vec<Out> {
        let acts = self.owner.unstarted(run);
        self.acts(acts, now)
    }

    /// A run asked of this node by an agent session (`asker`), told through told what it's owed
    /// should this node start again before it's over.
    pub fn ask_from(&mut self, mut ask: Ask, asker: Option<super::told::Answers>, now: i64) -> Vec<Out> {
        if let Some(a) = asker {
            // Kept by each runner that holds it, for a node that adopts it to tell its agent (LB03).
            ask.asker = serde_json::to_string(&a.clipped()).ok();
            self.askers.insert(ask.run.clone(), a);
        }
        self.ask(ask, now)
    }

    /// Node `me` in `epoch`, its runner's facts and labels, its first reading and settings.
    pub fn new(me: &str, epoch: u64, facts: RunnerFacts, labels: Vec<String>, mut sample: Sample, settings: Settings, now: i64) -> Placer {
        sample.size.gpu = u32::try_from(facts.gpus.len()).unwrap_or(u32::MAX);
        // Its owner decides by its node's settings too (`[reach] learned_weights`, `auction`; LB08).
        let mut owner = Owner::seeded(me, epoch, rand::random());
        owner.configure(&settings);
        let init = Init { me: me.to_owned(), epoch, since: None, facts: facts.clone(), labels: labels.clone(), sample, records: Records::new(), settings };
        Placer { me: me.to_owned(), epoch, owner, admission: Admission::new(init, now), specs: BTreeMap::new(), started: BTreeMap::new(), machines: Vec::new(), facts, labels, away: std::collections::BTreeSet::new(), told_to: std::collections::BTreeSet::new(), hinted: None, waits: BTreeMap::new(), gone: Vec::new(), waits_moved: false, whole: true, sensor: None, last: None, strained: None, parked: Vec::new(), groups: std::collections::HashMap::new(), asked_keys: std::collections::HashMap::new(), aliases: std::collections::HashMap::new(), pace: kiln_ci::pace::Pace::default(), draining: false, unplugged: None, topo: None, links: BTreeMap::new(), records: None, unkept: BTreeMap::new(), to_deliver: Vec::new(), askers: BTreeMap::new(), restored: std::collections::BTreeSet::new(), known: std::collections::BTreeSet::new(), budget: super::budget::Budget::new(me, epoch), hand: handoff::Hand::default() }
    }

    /// It measures its machine itself on events, with `sensor` (AUDIT L1, L2; W05-R26).
    #[must_use]
    pub fn sensing(mut self, sensor: Option<crate::sense::Sensor>) -> Placer {
        self.sensor = sensor;
        // Its size as measured, before anything asks (its owner's own machine is that size).
        self.measure_now(crate::util::now_ms(), &mut Vec::new());
        self
    }

    /// Its machine measured now, on an event (docs/needs.md E5: room is measured, never assumed;
    /// W05-R26: never by a decision, which reads the last measure): the room its admission admits
    /// against; whether the machine is under strain past the scheduler's reserve (sense's
    /// predicate, the gate guard's: L1, the admission offers nothing new until a measure shows it
    /// isn't); and the causes sense's health keeps that this measure shows cleared (L2), the probes
    /// they parked probed now. Nothing to do without a sensor.
    fn measure_now(&mut self, now: i64, said: &mut Vec<(String, ToOwner)>) {
        let Some(sensor) = self.sensor.as_mut() else { return };
        let m = sensor.measure();
        let window = sensor.strain().cloned();
        let mut sample = sample_of_measure(&m, self.last.as_ref());
        sample.pace = self.pace;
        sample.size.gpu = u32::try_from(self.facts.gpus.len()).unwrap_or(u32::MAX);
        // What each placed run here uses, from its own group, in the same moment.
        sample.runs = self.groups.iter_mut().map(|((owner, epoch, run), h)| kiln_ci::place::ask::RunUse { owner: owner.clone(), epoch: *epoch, run: run.clone(), used: group_use(h) }).collect();
        self.admission.measured(sample, now, said);
        let target = kiln_ci::sched::state::knee(m.cpus);
        // Over the window the decision is about (sense::window): since the load here last changed.
        let strained = crate::sense::health::strained(window.as_ref().unwrap_or(&m), target);
        if let Ok(mut h) = crate::sense::health::runner().lock() {
            h.measured(&m);
        }
        if strained != self.strained {
            self.admission.strain(strained.clone(), now, said);
            self.strained = strained;
        }
        for (peer, epoch, ask) in std::mem::take(&mut self.parked) {
            let key = self.key_here(&ask);
            match barred(&ask, key, &m) {
                Some(_) => self.parked.push((peer, epoch, ask)),
                None => self.admission.probe(Peer { name: &peer, epoch }, ask, None, now, said),
            }
        }
        self.last = Some(m);
    }

    /// `ask`'s kind of work as this runner keys its own runs of it (its asker's key, or what it
    /// maps to here).
    fn key_here(&self, ask: &Ask) -> Option<String> {
        ask.key.as_ref().map(|k| self.aliases.get(k).cloned().unwrap_or_else(|| k.clone()))
    }

    /// Placed run `run` of owner `owner` (`epoch`) runs here keyed `here` (sense's key of its
    /// command in its folder here): its asker's key for its kind maps to that from now on (L2).
    pub fn ran_as(&mut self, owner: &str, epoch: u64, run: &str, here: String) {
        if let Some(k) = self.asked_keys.remove(&(owner.to_owned(), epoch, run.to_owned()))
            && k != here
        {
            self.aliases.insert(k, here);
        }
    }

    /// The run spec `id` of `peer` starts here keyed `here` ([`Self::ran_as`]).
    pub fn spec_ran_as(&mut self, peer: &str, id: u64, here: String) {
        if let Some((epoch, run)) = self.started.get(&(peer.to_owned(), id)).cloned() {
            self.ran_as(peer, epoch, &run, here);
        }
    }

    /// Placed run `run` of owner `owner` (`epoch`) runs in group `at`: what it uses is read from
    /// there and taken out of the machine's load, so its room isn't counted twice.
    pub fn grouped(&mut self, owner: &str, epoch: u64, run: &str, at: Group) {
        self.groups.insert((owner.to_owned(), epoch, run.to_owned()), Held { at, cpu: None });
        self.load_changed();
    }

    /// The placed run a process `pid` started for: its group, found by the process.
    fn grouped_pid(&mut self, owner: &str, epoch: u64, run: &str, pid: Option<u32>) {
        self.load_changed();
        #[cfg(target_os = "linux")]
        if let Some(dir) = pid.and_then(crate::os::contain::group_of_pid) {
            self.grouped(owner, epoch, run, Group::Dir(dir));
        }
        #[cfg(not(target_os = "linux"))]
        let _ = (owner, epoch, run, pid);
    }

    fn ungrouped(&mut self, owner: &str, epoch: u64, run: &str) {
        self.groups.remove(&(owner.to_owned(), epoch, run.to_owned()));
        self.load_changed();
    }

    /// Placed work started or ended here: the load strain is weighed under changed now, and its
    /// machine is measured once this input is done ([`Self::settle`]).
    fn load_changed(&mut self) {
        if let Some(s) = self.sensor.as_mut() {
            s.anchor();
        }
    }

    /// Its machine changed (W05-R26: the OS's pressure, power or thermal events woke its pace, the
    /// host's sample came, a leg ended): measured once this input is done ([`Self::settle`]).
    pub fn machine_changed(&mut self) {
        if let Some(s) = self.sensor.as_mut() {
            s.changed();
        }
    }

    /// Whether an event made a measure due ([`Self::settle`]).
    pub fn measure_due(&self) -> bool {
        self.sensor.as_ref().is_some_and(crate::sense::Sensor::due)
    }

    /// The measure an event made due, read once the input's words are out, so no decision waits
    /// on a read (W05-R26, W05-R03): one read, however many changes the input made.
    pub fn settle(&mut self, now: i64) -> Vec<Out> {
        if !self.measure_due() {
            return Vec::new();
        }
        let mut said = Vec::new();
        self.measure_now(now, &mut said);
        self.said(said, now)
    }

    /// Why its machine is under strain now, if it is, as its last measure said (the hint's state).
    pub fn strain(&self) -> Option<String> {
        self.strained.clone()
    }

    /// A probe from `peer` (`epoch`): to its admission, against the last measure (W05-R26: a
    /// decision reads nothing of the machine); or, when this runner's health bars its kind of work
    /// (L2), parked here and answered so, until a measure clears it.
    fn probe(&mut self, peer: &str, epoch: u64, ask: std::sync::Arc<Ask>, now: i64, said: &mut Vec<(String, ToOwner)>) {
        if let Some(k) = &ask.key {
            self.asked_keys.insert((peer.to_owned(), epoch, ask.run.clone()), k.clone());
        }
        let key = self.key_here(&ask);
        if self.strained.is_none()
            && let Some(m) = &self.last
            && let Some(r) = barred(&ask, key, m)
        {
            let free = self.admission.hint();
            said.push((peer.to_owned(), ToOwner::Waiting { run: ask.run.clone(), reason: kiln_ci::place::msg::Reason::Barred { cause: r.kind, why: r.why }, place: kiln_ci::place::msg::Place::default(), start: kiln_ci::place::msg::Estimate::default(), predicted: None, free, pace: self.pace }));
            self.parked.retain(|(p, _, a)| !(p == peer && a.run == ask.run));
            self.parked.push((peer.to_owned(), epoch, ask));
            return;
        }
        self.admission.probe(Peer { name: peer, epoch }, ask, None, now, said);
    }

    /// The placement kernel's start of `ask` from `peer` (docs/lb.md, "Adopted"): declined when
    /// sense's health bars, by the last measure, bar its kind of work here
    /// (its owner places it again, or probes it, and it waits saying why), else admitted or
    /// declined by the kernel's ledger, which says this admission's room while nothing waits here.
    #[allow(clippy::too_many_arguments)]
    fn fast(&mut self, peer: &str, epoch: u64, ask: std::sync::Arc<Ask>, kernel: kiln_ci::lb::Handle, at: u32, now: i64, said: &mut Vec<(String, ToOwner)>) -> Option<Start> {
        // Nothing measured here: a decision reads the last measure (sense's own events take new ones).
        if let Some(k) = &ask.key {
            self.asked_keys.insert((peer.to_owned(), epoch, ask.run.clone()), k.clone());
        }
        let key = self.key_here(&ask);
        if self.strained.is_none()
            && let Some(m) = &self.last
            && barred(&ask, key, m).is_some()
        {
            // None here for it, in the word its owner hears (what it holds, under room tokens).
            let word = self.admission.fast_word_none_to(Peer { name: peer, epoch });
            said.push((peer.to_owned(), ToOwner::FastDeclined { run: ask.run.clone(), kernel: kiln_ci::place::msg::FastHandle::of(kernel), why: kiln_ci::place::msg::FastWhy::Never, word }));
            return None;
        }
        self.admission.fast(Peer { name: peer, epoch }, ask, kernel, at, None, now, said)
    }

    /// The directory came (or changed): the owner's machines, this one among them as it measures
    /// itself.
    pub fn directory(&mut self, runners: &[RunnerInfo], now: i64) -> Vec<Out> {
        // Its per-core speed as the fleet's fit says it (W05-R26): sense's, for its pace.
        if let Some(me) = runners.iter().find(|r| r.name == self.me) {
            crate::sense::pace::set_speed(me.speed);
        }
        // Owners gone from the directory are forgotten: their records of ended runs go (LB03).
        let names: std::collections::BTreeSet<String> = runners.iter().map(|r| r.name.clone()).collect();
        // An empty directory is no word that anyone left (a server that answered nothing).
        if names.is_empty() {
            return self.directory_in(runners, now);
        }
        // Which nodes take hand-offs (LB03).
        self.hand_directory(runners);
        let gone: Vec<String> = self.known.iter().filter(|n| !names.contains(*n) && **n != self.me && *n != CI).cloned().collect();
        self.known = names;
        let mut forgot = Vec::new();
        for name in gone {
            self.admission.forget(&name, now, &mut forgot);
        }
        let mut out = self.said(forgot, now);
        out.extend(self.directory_in(runners, now));
        out
    }

    fn directory_in(&mut self, runners: &[RunnerInfo], now: i64) -> Vec<Out> {
        let mut ms: Vec<Machine> = runners.iter().filter(|r| r.name != self.me).map(machine_of).collect();
        let cap = self.admission.cap();
        ms.push(Machine { name: self.me.clone(), facts: self.facts.clone(), labels: self.labels.clone(), size: Size { cpu: Some(Cpus::milli(cap[0])), mem: Some(cap[1]), disk: Some(cap[2]).filter(|d| *d > 0), gpu: Some(u32::try_from(cap[3]).unwrap_or(u32::MAX)) }, epoch: self.epoch, connected: true, places: true, idle: None, syncs: true, fast: true });
        if ms == self.machines {
            return Vec::new();
        }
        self.machines = ms.clone();
        let mut acts = self.owner.machines(ms);
        acts.extend(self.owner.reask(now));
        self.acts(acts, now)
    }

    /// A run asked of this node: owned here, probed where it could run.
    pub fn ask(&mut self, ask: Ask, now: i64) -> Vec<Out> {
        // Held back first for its tenant's budget (LB08): placed once its worst case is reserved.
        let asker = self.askers.get(&ask.run).cloned();
        if let Some(b) = self.budget.gate(&ask, asker, now) {
            return self.budget_outs(b, now);
        }
        self.ask_now(ask, now)
    }

    /// A run owned here now: probed where it could run.
    fn ask_now(&mut self, ask: Ask, now: i64) -> Vec<Out> {
        self.waiting(&ask, now);
        let acts = self.owner.ask(ask, now);
        self.acts(acts, now)
    }

    /// Runs owned here now that arrived in one step (a budget let them through together): placed
    /// together, as the owner's settings say (`[reach] auction`: jointly; else one at a time).
    fn ask_many_now(&mut self, asks: Vec<Ask>, now: i64) -> Vec<Out> {
        for ask in &asks {
            self.waiting(ask, now);
        }
        let acts = self.owner.ask_many(asks, now);
        self.acts(acts, now)
    }

    /// A run of this node's own waits, as its hint will say until it starts.
    fn waiting(&mut self, ask: &Ask, now: i64) {
        let wait = crate::offers::hint::Wait { run: ask.run.clone(), need: ask.needs.describe(), ask: kiln_ci::place::ask::amounts(&ask.needs.request), class: ask.class, since: now, reason: String::new() };
        self.waits.insert(ask.run.clone(), wait);
    }

    /// Its asker doesn't want it any more.
    pub fn cancel(&mut self, run: &str, now: i64) -> Vec<Out> {
        let (held, b) = self.budget.cancel(run, now);
        let mut out = self.budget_outs(b, now);
        if !held {
            let acts = self.owner.cancel(run);
            out.extend(self.acts(acts, now));
        }
        out
    }

    /// The local run `run` is about to start on `runner` under `fence`: whether it still may
    /// ([`Owner::starting`]); for this runner, its admission says whether it starts (`New`).
    pub fn starting(&mut self, run: &str, runner: &str, fence: Fence, now: i64) -> (bool, Vec<Out>) {
        // The kernel's start of a run it placed (docs/lb.md): its ask, handle and epoch.
        let fast = self.owner.fast_start(run, fence);
        if !self.owner.starting(run, runner, fence) {
            return (false, Vec::new());
        }
        // A kernel start is its run's first word at a runner, or its next after the last declined
        // (a runner it went to before holds nothing): `runner` joins the run's members freely, kept
        // before the start is said, so that a hand-off's claim reaches it (LB03).
        if fast.is_some()
            && let kiln_ci::place::handoff::Verdict::Pass(members) = self.hand.members.write(run, runner, true)
            && runner == self.me
        {
            self.hand.acc.probed(&Handle::new(&self.me, self.epoch, run), members);
        }
        if runner != self.me {
            return (true, Vec::new());
        }
        let mut said = Vec::new();
        // Its tenant's usage elsewhere, carried here as a start to another runner carries it (W05-R32).
        let elsewhere = self.owner.start_carry(run);
        let me = Peer { name: &self.me, epoch: self.epoch };
        let started = match fast {
            Some((ask, _, kernel, at)) => {
                self.admission.carried(me, run, Some(&*ask), elsewhere, now, &mut said);
                let (name, epoch) = (self.me.clone(), self.epoch);
                self.fast(&name, epoch, ask, kernel, at, now, &mut said)
            }
            None => {
                self.admission.carried(me, run, None, elsewhere, now, &mut said);
                self.admission.start(me, run, fence, now, &mut said)
            }
        };
        let out = self.said(said, now);
        (started == Some(Start::New), out)
    }

    /// The start of run `run` on `runner` under `fence`, said with the spec id it's for, and with
    /// its tenant's usage elsewhere since this node's last word there about it (W05-R32), the
    /// kernel's start ([`ToRunner::Fast`]) as a claim's.
    pub fn start_word(&mut self, run: &str, fence: Fence, spec: u64) -> String {
        let elsewhere = self.owner.start_carry(run);
        // A kernel start says the run's members, as a probe does (its runner had none: LB03).
        let (msg, members) = match self.owner.fast_start(run, fence) {
            Some((ask, _, kernel, epoch)) => (ToRunner::Fast { ask, kernel: kiln_ci::place::msg::FastHandle::of(kernel), epoch, elsewhere }, self.hand.members.members(run)),
            None => (ToRunner::Start { run: run.to_owned(), fence, elsewhere }, Vec::new()),
        };
        serde_json::to_string(&Envelope { epoch: self.epoch, spec: Some(spec), ballot: self.hand.ballot_of(run), members, msg }).unwrap_or_default()
    }

    /// A run of this node's started here: its process is up (the runner's word), process `pid`.
    pub fn local_spawned_pid(&mut self, run: &str, pid: Option<u32>, now: i64) -> Vec<Out> {
        let (me, epoch) = (self.me.clone(), self.epoch);
        self.grouped_pid(&me, epoch, run, pid);
        self.local_spawned(run, now)
    }

    /// A run of this node's started here: its process is up (the runner's word).
    pub fn local_spawned(&mut self, run: &str, now: i64) -> Vec<Out> {
        let mut said = Vec::new();
        let me = Peer { name: &self.me, epoch: self.epoch };
        self.admission.spawned(me, run, &mut said);
        self.said(said, now)
    }

    /// A run of this node's here ended (`ok`: its process exited).
    pub fn local_ended(&mut self, run: &str, ok: bool, now: i64) -> Vec<Out> {
        let (me, epoch) = (self.me.clone(), self.epoch);
        self.ungrouped(&me, epoch, run);
        let mut said = Vec::new();
        let me = Peer { name: &self.me, epoch: self.epoch };
        self.admission.ended(me, run, ok, now, &mut said);
        self.admission.ack(me, run);
        self.said(said, now)
    }

    /// An owner's word, from runner `peer`'s link (`Ask::Place`).
    pub fn told(&mut self, peer: &str, json: &str, now: i64) -> Vec<Out> {
        self.hand_heard_from(peer);
        if let Some(b) = self.budget.word(peer, json, now) {
            return self.budget_outs(b, now);
        }
        let Ok(e) = serde_json::from_str::<Envelope<ToRunner>>(json) else {
            // The hand-off's own words (LB03), ops of their own.
            if let Ok(e) = serde_json::from_str::<Envelope<Word>>(json) {
                return self.hand_told(peer, e.msg, now);
            }
            tracing::debug!(%peer, "a placement word this runner couldn't read");
            return Vec::new();
        };
        let mut out = Vec::new();
        // The hand-off's gate (LB03): a word about a run promised to a claim, under a lower ballot,
        // is refused (its first owner's later take, start or withdraw among them); the word of the
        // claim that owns a run it adopted goes to the run as its runners know it.
        if let Some(run) = handoff::run_of(&e.msg) {
            let ballot = e.ballot.clone().unwrap_or(Ballot::FIRST);
            let Some(h) = self.hand_gate(peer, e.epoch, run, &ballot, now, &mut out) else { return out };
            if Handle::of_adopted(run).is_some() {
                out.extend(self.hand_owner_word(h, e.msg, now));
                return out;
            }
            if matches!(e.msg, ToRunner::Probe { .. } | ToRunner::Fast { .. }) {
                self.hand.acc.probed(&h, e.members.iter().cloned());
            }
        }
        let from = Peer { name: peer, epoch: e.epoch };
        let mut said = Vec::new();
        if self.away.remove(peer) {
            self.admission.back(from, now, &mut said);
        }
        match e.msg {
            ToRunner::Probe { ask, elsewhere } => {
                self.admission.carried(from, &ask.run, Some(&*ask), elsewhere, now, &mut said);
                self.probe(peer, e.epoch, ask, now, &mut said);
            }
            ToRunner::Withdraw { run } => {
                self.parked.retain(|(p, _, a)| !(p == peer && a.run == run));
                self.admission.withdraw(from, &run, now, &mut said);
            }
            ToRunner::Take { run, fence } => self.admission.take(from, &run, fence, now, &mut said),
            ToRunner::Decline { run, fence } => self.admission.decline(from, &run, fence, now, &mut said),
            ToRunner::Start { run, fence, elsewhere } => {
                self.admission.carried(from, &run, None, elsewhere, now, &mut said);
                let started = self.admission.start(from, &run, fence, now, &mut said);
                if let Some(spec) = e.spec {
                    let verdict = match started {
                        Some(Start::New) => Ok(()),
                        Some(Start::Already) => Err(format!("{run} ran here already: it isn't started again")),
                        None => Err(format!("{run}'s claim here isn't live: it isn't started")),
                    };
                    self.specs.insert((peer.to_owned(), spec), (run, e.epoch, verdict));
                }
            }
            ToRunner::Ack { run } => {
                self.admission.ack(from, &run);
                self.hand.acc.unheld(&Handle::new(peer, e.epoch, &run));
            }
            ToRunner::Fast { ask, kernel, epoch, elsewhere } => {
                let run = ask.run.clone();
                self.admission.carried(from, &run, Some(&*ask), elsewhere, now, &mut said);
                let started = self.fast(peer, e.epoch, ask, kernel.handle(), epoch, now, &mut said);
                if let Some(spec) = e.spec {
                    let verdict = match started {
                        Some(Start::New) => Ok(()),
                        Some(Start::Already) => Err(format!("{run} ran here already: it isn't started again")),
                        None => Err(format!("{run} has no room here now: it isn't started")),
                    };
                    self.specs.insert((peer.to_owned(), spec), (run, e.epoch, verdict));
                }
            }
            ToRunner::Sync => self.admission.sync(from, now, &mut said),
            // What it holds here and can't use, given back under room tokens (docs/lb.md "Room
            // tokens"; LB08): forwarded to the owner whose run last waited here.
            ToRunner::FastRelease { epoch, amount } => self.admission.fast_release(from, epoch, amount, now, &mut said),
        }
        out.extend(self.said(said, now));
        out
    }

    /// Whether owner `peer`'s link named `place.tokens` (docs/lb.md "Room tokens"; LB08), as the
    /// node reads it off the link each of its words came on: only then, and only where `[reach]
    /// tokens` is declared, is it told what it holds here, on placement's channel.
    pub fn tokens_with(&mut self, peer: &str, on: bool) {
        self.admission.tokens_with(peer, on);
    }

    /// A spec came from runner `peer` (`Ask::Run`): what to do with it.
    pub fn spec(&mut self, peer: &str, id: u64) -> Spec {
        match self.specs.remove(&(peer.to_owned(), id)) {
            Some((run, epoch, Ok(()))) => {
                self.started.insert((peer.to_owned(), id), (epoch, run.clone()));
                Spec::Start { run }
            }
            Some((_, _, Err(why))) => Spec::Refuse(why),
            None => Spec::Unplaced,
        }
    }

    /// A run started here for `peer` (spec `id`) is up, process `pid`.
    pub fn spawned_pid(&mut self, peer: &str, id: u64, pid: Option<u32>, now: i64) -> Vec<Out> {
        if let Some((epoch, run)) = self.started.get(&(peer.to_owned(), id)).cloned() {
            self.grouped_pid(peer, epoch, &run, pid);
        }
        self.spawned(peer, id, now)
    }

    /// A run started here for `peer` (spec `id`) is up.
    pub fn spawned(&mut self, peer: &str, id: u64, now: i64) -> Vec<Out> {
        let Some((epoch, run)) = self.started.get(&(peer.to_owned(), id)).cloned() else { return Vec::new() };
        let mut said = Vec::new();
        self.admission.spawned(Peer { name: peer, epoch }, &run, &mut said);
        self.said(said, now)
    }

    /// A run started here for `peer` (spec `id`) ended.
    pub fn ended(&mut self, peer: &str, id: u64, ok: bool, now: i64) -> Vec<Out> {
        self.ended_as(peer, id, ok, How::Unsaid, now)
    }

    /// [`Self::ended`], with this runner's judgment of how the run ended ([`how`]): what its
    /// owner's kernel is told of the assignment (docs/lb.md "Good places").
    pub fn ended_as(&mut self, peer: &str, id: u64, ok: bool, how: How, now: i64) -> Vec<Out> {
        let Some((epoch, run)) = self.started.remove(&(peer.to_owned(), id)) else { return Vec::new() };
        self.ungrouped(peer, epoch, &run);
        let mut said = Vec::new();
        self.admission.ended_as(Peer { name: peer, epoch }, &run, ok, how, now, &mut said);
        let mut out = self.said(said, now);
        // Its end to its claimant, or handed off if its owner can't be reached; the room it freed
        // may be what a waiting run of such an owner needs (LB03).
        out.extend(self.hand_ended(peer, epoch, &run, now));
        out.extend(self.hand_room(now));
        out
    }

    /// W13-R01: local facts go straight to admission, preserving the room ledger.
    pub fn refreshed(&mut self, facts: RunnerFacts, now: i64) -> Vec<Out> {
        if facts == self.facts { return Vec::new(); }
        self.facts = facts;
        let mut said = Vec::new();
        self.admission.refreshed(self.facts.clone(), self.labels.clone(), now, &mut said);
        let mut out = self.said(said, now);
        if let Some(me) = self.machines.iter_mut().find(|m| m.name == self.me) {
            me.facts = self.facts.clone();
            me.size.gpu = Some(u32::try_from(self.admission.cap()[3]).unwrap_or(u32::MAX));
        }
        let mut acts = self.owner.machines(self.machines.clone());
        acts.extend(self.owner.reask(now));
        out.extend(self.acts(acts, now));
        out
    }

    /// A runner's word, from `peer`'s link (`Say::Place`).
    pub fn heard(&mut self, peer: &str, json: &str, now: i64) -> Vec<Out> {
        self.hand_heard_from(peer);
        if let Some(b) = self.budget.word(peer, json, now) {
            return self.budget_outs(b, now);
        }
        let Ok(e) = serde_json::from_str::<Envelope<ToOwner>>(json) else {
            // The hand-off's own words (LB03): a promise, a refusal, a run's members grown.
            if let Ok(e) = serde_json::from_str::<Envelope<Word>>(json) {
                return self.hand_heard(peer, e.msg, now);
            }
            tracing::debug!(%peer, "a placement word this owner couldn't read");
            return Vec::new();
        };
        let mut out = self.hand_member_said(peer, &e.msg, now);
        let mut acts = self.owner.runner_epoch(peer, e.epoch);
        acts.extend(self.owner.heard(peer, e.msg, now));
        out.extend(self.acts(acts, now));
        out
    }

    /// A reading of this machine: the room it admits against; where it measures itself, a measure
    /// due once this input is done (the reading's figures aren't taken).
    pub fn measured(&mut self, mut sample: Sample, now: i64) -> Vec<Out> {
        sample.size.gpu = u32::try_from(self.facts.gpus.len()).unwrap_or(u32::MAX);
        let mut said = Vec::new();
        if self.sensor.is_some() {
            self.machine_changed();
        } else {
            self.admission.measured(Sample { pace: self.pace, ..sample }, now, &mut said);
        }
        self.said(said, now)
    }

    /// Sense told its pace (W05-R26): its words carry it from now on; and whether its battery
    /// drains it, why (`[runner] on_battery`), with the server's draining.
    pub fn paced(&mut self, pace: kiln_ci::pace::Pace, unplugged: Option<String>, now: i64) -> Vec<Out> {
        let mut said = Vec::new();
        self.pace = pace;
        self.admission.paced(pace);
        if unplugged != self.unplugged {
            self.unplugged = unplugged;
            self.admission.drain(self.draining || self.unplugged.is_some(), now, &mut said);
        }
        let mut out = self.said(said, now);
        out.extend(self.hand_room(now));
        out
    }

    /// This node's link to runner `peer` went down, or came back: what this node owns there waits
    /// for it, and is probed again when it's back.
    pub fn runner_link(&mut self, peer: &str, up: bool, now: i64) -> Vec<Out> {
        let acts = if up { self.owner.back(peer) } else { self.owner.away(peer) };
        let mut out = self.acts(acts, now);
        let b = self.budget.link(peer, up, now);
        out.extend(self.budget_outs(b, now));
        if up {
            out.extend(self.hand_relinked(peer, now));
        }
        out
    }

    /// Owner `peer`'s link to this runner went down: its runs leave the line (a claim it took stays
    /// held), and come back at their places with its next word.
    pub fn owner_away(&mut self, peer: &str, now: i64) -> Vec<Out> {
        let mut said = Vec::new();
        self.admission.away(peer, now, &mut said);
        self.away.insert(peer.to_owned());
        self.said(said, now)
    }

    /// What this runner's admission holds, for a view: its budget and room, what its claims hold,
    /// how many wait in each class, its timer.
    pub fn view(&self) -> serde_json::Value {
        let (room, held) = self.admission.room();
        let queued: serde_json::Map<String, serde_json::Value> = kiln_ci::sched::state::Class::ALL.iter().zip(self.admission.queued_by_class()).map(|(c, n)| (c.as_str().to_owned(), n.into())).collect();
        serde_json::json!({
            "runner": self.me,
            "epoch": self.epoch,
            "cap": self.admission.cap(),
            "room": room,
            "held": held,
            "queued": queued,
            "next": self.admission.timer(),
            "links": self.links,
        })
    }

    /// The time.
    pub fn tick(&mut self, now: i64) -> Vec<Out> {
        let acts = self.owner.tick(now);
        let mut out = self.acts(acts, now);
        let b = self.budget.tick(now);
        out.extend(self.budget_outs(b, now));
        let mut said = Vec::new();
        self.admission.tick(now, &mut said);
        out.extend(self.said(said, now));
        out
    }

    /// When to be told the time again.
    pub fn timer(&self) -> Option<i64> {
        [self.owner.timer(), self.admission.timer(), self.budget.timer()].into_iter().flatten().min()
    }

    /// The runners this node's owner has told something (to link to again).
    pub fn told_to(&self) -> Vec<String> {
        self.told_to.iter().cloned().collect()
    }

    /// CI leg `ask.run`, assigned this runner by the CI engine (AUDIT M4, G1): admitted against
    /// the same ledger as the runs owners place here, now or not at all. Room for it now, its
    /// claim is taken and started at once (held until [`Self::leg_ended`]); none, refused saying
    /// why, and the engine places it elsewhere or later (it queues in the engine, never here).
    pub fn leg(&mut self, ask: Ask, now: i64) -> (Result<(), String>, Vec<Out>) {
        let run = ask.run.clone();
        let ci = Peer { name: CI, epoch: self.epoch };
        let mut said = Vec::new();
        let ask = std::sync::Arc::new(ask);
        // The kernel's path first (docs/lb.md, "Adopted"): room for it now with nothing waiting
        // here, it is admitted in one step, in the same ledger; else probing's, now or not at all.
        let at = self.admission.fast_word().epoch;
        let epoch = self.epoch;
        if let Some(started) = self.fast(CI, epoch, std::sync::Arc::clone(&ask), kiln_ci::lb::Handle::NONE, at, now, &mut said) {
            let verdict = match started {
                Start::New => {
                    let mut more = Vec::new();
                    self.admission.spawned(ci, &run, &mut more);
                    said.extend(more);
                    self.load_changed();
                    #[cfg(target_os = "linux")]
                    if let Some(j) = crate::os::contain::Job::of(&run) {
                        self.grouped(CI, epoch, &run, Group::Dir(j.dir().to_path_buf()));
                    }
                    Ok(())
                }
                Start::Already => Err(format!("{run} ran here already: it isn't started again")),
            };
            said.retain(|(to, _)| to != CI);
            return (verdict, self.said(said, now));
        }
        said.retain(|(to, _)| to != CI);
        self.probe(CI, self.epoch, ask, now, &mut said);
        let mut verdict: Result<Fence, String> = Err("this runner's admission didn't answer".to_owned());
        let mut rest = Vec::new();
        for (to, msg) in said {
            if to != CI {
                rest.push((to, msg));
                continue;
            }
            match msg {
                ToOwner::Offer { run: r, fence, .. } if r == run => verdict = Ok(fence),
                ToOwner::Waiting { run: r, reason, .. } if r == run => verdict = Err(reason_words(&reason)),
                ToOwner::Never { run: r, why } if r == run => verdict = Err(why),
                _ => {}
            }
        }
        let mut more = Vec::new();
        let verdict = match verdict {
            Ok(fence) => {
                self.admission.take(ci, &run, fence, now, &mut more);
                match self.admission.start(ci, &run, fence, now, &mut more) {
                    Some(Start::New) => {
                        self.admission.spawned(ci, &run, &mut more);
                        self.load_changed();
                        #[cfg(target_os = "linux")]
                        if let Some(j) = crate::os::contain::Job::of(&run) {
                            let epoch = self.epoch;
                            self.grouped(CI, epoch, &run, Group::Dir(j.dir().to_path_buf()));
                        }
                        Ok(())
                    }
                    _ => {
                        self.admission.withdraw(ci, &run, now, &mut more);
                        Err("its room here lapsed before it could be held".to_owned())
                    }
                }
            }
            Err(why) => {
                self.parked.retain(|(p, _, a)| !(p == CI && a.run == run));
                self.admission.withdraw(ci, &run, now, &mut more);
                Err(why)
            }
        };
        rest.extend(more.into_iter().filter(|(to, _)| to != CI));
        (verdict, self.said(rest, now))
    }

    /// CI leg `run` ended here (`ok`: its process exited): its room is free at once.
    pub fn leg_ended(&mut self, run: &str, ok: bool, now: i64) -> Vec<Out> {
        let ci = Peer { name: CI, epoch: self.epoch };
        self.groups.remove(&(CI.to_owned(), self.epoch, run.to_owned()));
        let mut said = Vec::new();
        self.admission.ended(ci, run, ok, now, &mut said);
        self.admission.ack(ci, run);
        self.machine_changed();
        said.retain(|(to, _)| to != CI);
        self.said(said, now)
    }

    /// This runner takes nothing new while `draining` (an upgrade pending, or retiring), or does
    /// again: its runs wait saying so, and its hint says it (AUDIT O1).
    pub fn drain(&mut self, draining: bool, now: i64) -> Vec<Out> {
        let mut said = Vec::new();
        self.draining = draining;
        self.admission.drain(draining || self.unplugged.is_some(), now, &mut said);
        self.said(said, now)
    }

    /// Whether it measures its machine itself.
    pub fn senses(&self) -> bool {
        self.sensor.is_some()
    }

    /// This node's name and placement epoch: an owner's, as its claims name it.
    pub fn owner_of_claims(&self) -> (String, u64) {
        (self.me.clone(), self.epoch)
    }

    /// An agent session placed here under claim `c` started (AUDIT M4): its room is held from now
    /// until [`Self::session_ended`], as a run's is. Its owner's word that it may (`Start`) is the
    /// claim the server gave the session; this runner's word that it started goes to that owner.
    /// A claim that isn't live here any more starts nothing (`false`).
    pub fn session_start(&mut self, c: &Claim, session: Option<&str>, now: i64) -> (bool, Vec<Out>) {
        if let Some(id) = session {
            self.grouped(&c.owner, c.epoch, &c.run, Group::Session(id.to_owned()));
        }
        self.load_changed();
        let mut said = Vec::new();
        let started = if c.owner == self.me {
            // Its owner is this node: its start was taken as it was told to go (`starting`).
            self.admission.spawned(Peer { name: &self.me, epoch: self.epoch }, &c.run, &mut said);
            true
        } else {
            let from = Peer { name: &c.owner, epoch: c.epoch };
            match self.admission.start(from, &c.run, c.fence, now, &mut said) {
                Some(Start::New | Start::Already) => {
                    self.admission.spawned(from, &c.run, &mut said);
                    true
                }
                None => false,
            }
        };
        (started, self.said(said, now))
    }

    /// An agent session placed here under claim `c` ended (closed, or gone on elsewhere): its
    /// room is free at once.
    pub fn session_ended(&mut self, c: &Claim, now: i64) -> Vec<Out> {
        self.ungrouped(&c.owner, c.epoch, &c.run);
        if c.owner == self.me {
            return self.local_ended(&c.run, true, now);
        }
        let mut said = Vec::new();
        self.admission.ended(Peer { name: &c.owner, epoch: c.epoch }, &c.run, true, now, &mut said);
        self.said(said, now)
    }

    /// The runner's admission, for views (`/api/reach/offers`).
    pub fn admission(&self) -> &Admission {
        &self.admission
    }

    /// One of this node's runs was answered `a`: it waits (why, as the answer counts it first),
    /// or it doesn't any more.
    fn waited(&mut self, run: &str, a: &Answer) {
        match a {
            Answer::Queued { reasons, .. } => {
                let reason = reasons.first().map(|c| c.reason.clone()).unwrap_or_default();
                if let Some(w) = self.waits.get_mut(run).filter(|w| w.reason != reason) {
                    w.reason = reason;
                    self.waits_moved = true;
                }
            }
            _ => {
                // Said gone only if it was said to wait.
                if self.waits.remove(run).is_some_and(|w| !w.reason.is_empty()) {
                    self.gone.push(run.to_owned());
                }
            }
        }
    }

    /// The server's link came back (it takes hints, `offers`), or went: its next hint says all.
    pub fn hints_anew(&mut self) {
        self.hinted = None;
        self.whole = true;
    }

    /// Its admission's hint (the `offers` feature, docs/needs.md E5, AUDIT O1), JSON, when it
    /// changed something decided by since the last said (`crate::offers::relevant`, AUDIT O2): an
    /// answer for a size waiting or held here flips, its state, budget or line's counts move, a
    /// dimension's room goes to none or comes back (an owner choosing where to probe by hints
    /// takes a runner with none of what its run asks for one it doesn't fit), or this node's own
    /// runs that wait. Never per sample. `strained`: why its machine is, if it is (sense's
    /// health, as the gate's guard sees it).
    pub fn hint_due(&mut self, now: i64, strained: Option<String>) -> Option<String> {
        use crate::offers::hint::{Hint, Quote, Quoted, Taking};
        use crate::offers::relevant::{Said, relevant};
        let state = match (self.admission.taking(), strained) {
            (Err(kiln_ci::place::msg::Reason::Draining), _) => Taking::Draining,
            (_, Some(why)) => Taking::Strained { why },
            _ => Taking::Taking,
        };
        let room = kiln_ci::place::ask::amounts(&self.admission.hint());
        // Its admission's answer for each size waiting or held here, as a probe of it would get it.
        let mut sizes: BTreeMap<[u64; 4], kiln_ci::sched::state::Class> = BTreeMap::new();
        for p in self.admission.probes() {
            sizes.entry(p.ask).or_insert(p.class);
        }
        let quotes = sizes
            .into_iter()
            .map(|(a, class)| {
                let ask = Ask { needs: kiln_ci::sched::needs::Needs { request: kiln_ci::place::ask::resources(&a), ..kiln_ci::sched::needs::Needs::default() }, class, ..Ask::default() };
                let since = |t: Option<i64>| t.map(|t| u64::try_from(t.saturating_sub(now)).unwrap_or(0));
                let answer = match self.admission.quote(&ask, now) {
                    kiln_ci::place::runner::Quote::Now => Quoted::Now,
                    kiln_ci::place::runner::Quote::Never { why } => Quoted::Never { why },
                    kiln_ci::place::runner::Quote::Waits { reason, start } => Quoted::Waits { reason: reason_kind(&reason).to_owned(), soonest_in: since(start.soonest), latest_in: since(start.latest) },
                };
                Quote { ask: a, answer }
            })
            .collect();
        let mut hint = Hint { epoch: self.epoch, seq: 0, at: now, budget: self.admission.cap(), room, queued: self.admission.queued_by_class(), state, quotes, waits: Vec::new(), gone: Vec::new(), whole: self.whole, topo: None, fast: Some(self.admission.fast_word()) };
        let changed = match &self.hinted {
            None => true,
            Some(last) => {
                let roomy = |h: &Hint| h.room.map(|a| a > 0);
                // The sizes that wait or hold room here: what a change of room is decided by.
                let sizes: std::collections::BTreeSet<[u64; 4]> = self.admission.probes().into_iter().map(|p| p.ask).collect();
                let (was, is) = (Said { hint: last.clone(), heard: last.at }, Said { hint: hint.clone(), heard: hint.at });
                // The kernel's word changed: owners' kernels pick by it (docs/lb.md), one hint for
                // however many changes an input made.
                self.whole || self.waits_moved || !self.gone.is_empty() || roomy(last) != roomy(&hint) || last.fast != hint.fast || relevant(&was, &is, sizes.iter())
            }
        };
        if !changed {
            return None;
        }
        // The topology rides a hint that's due anyway: it never makes one due (LB08).
        hint.topo = self.topo.clone();
        hint.seq = self.hinted.as_ref().map_or(1, |h| h.seq.saturating_add(1));
        hint.waits = if self.whole || self.waits_moved { self.waits.values().filter(|w| !w.reason.is_empty()).cloned().collect() } else { Vec::new() };
        hint.gone = std::mem::take(&mut self.gone);
        (self.whole, self.waits_moved) = (false, false);
        let json = serde_json::to_string(&hint).ok()?;
        self.hinted = Some(Hint { waits: Vec::new(), gone: Vec::new(), ..hint });
        Some(json)
    }

    /// Another runner's room as the server passed it on (`ServerMsg::Offers`): its owner chooses
    /// where to probe by it (perf's `Owner::hint`); its own and stale ones aren't taken.
    pub fn hinted(&mut self, json: &str) {
        let Ok(r) = serde_json::from_str::<crate::offers::hint::Room>(json) else {
            tracing::debug!("a room hint this owner couldn't read");
            return;
        };
        if r.runner == self.me {
            return;
        }
        let free = kiln_ci::place::ask::resources(&r.room);
        self.owner.hint(&r.runner, free, r.order());
        if let Some(w) = &r.fast {
            self.owner.fast_hint(&r.runner, w);
        }
    }

    /// What the runner's admission said: to this node's owner, heard at once; to another, sent.
    fn said(&mut self, said: Vec<(String, ToOwner)>, now: i64) -> Vec<Out> {
        // This node's own runner, as its owner's kernel picks by it: its word as it is now.
        let (me, word) = (self.me.clone(), self.admission.fast_word());
        self.owner.fast_hint(&me, &word);
        let out = self.said_in(said, now);
        self.note_changes();
        out
    }

    fn said_in(&mut self, said: Vec<(String, ToOwner)>, now: i64) -> Vec<Out> {
        let mut out = Vec::new();
        for (to, msg) in said {
            if to == self.me {
                let me = self.me.clone();
                out.extend(self.hand_member_said(&me, &msg, now));
                let acts = self.owner.heard(&me, msg, now);
                out.extend(self.acts(acts, now));
            } else if let Ok(json) = serde_json::to_string(&Envelope { epoch: self.epoch, spec: None, ballot: None, members: Vec::new(), msg }) {
                out.push(Out::Say { peer: to, json });
            }
        }
        out
    }

    /// What the owner is to do: words for this runner go to its admission at once. What it changed
    /// is noted to be kept before any of it is published (LB03).
    fn acts(&mut self, acts: Vec<Act>, now: i64) -> Vec<Out> {
        let out = self.acts_in(acts, now);
        self.note_changes();
        out
    }

    fn acts_in(&mut self, acts: Vec<Act>, now: i64) -> Vec<Out> {
        let mut out = Vec::new();
        let mut queue: std::collections::VecDeque<Act> = acts.into();
        // The runs whose first probes go in this step: they make its members (LB03).
        let mut first: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        while let Some(a) = queue.pop_front() {
            let a = match a {
                // A probe past a run's members waits until every member that may hold it took the
                // runner (LB03, `kiln_ci::place::handoff::members`): a claim then reaches every
                // runner it could start on.
                Act::Tell { runner, msg: ToRunner::Probe { ask, elsewhere } } => {
                    let fresh = first.contains(&ask.run) || !self.hand.members.knows(&ask.run);
                    if fresh {
                        first.insert(ask.run.clone());
                    }
                    match self.hand_probe(&runner, &ask, elsewhere, fresh, now, &mut out) {
                        Some(members) if runner != self.me => {
                            self.told_to.insert(runner.clone());
                            let ballot = self.hand.ballot_of(&ask.run);
                            if let Ok(json) = serde_json::to_string(&Envelope { epoch: self.epoch, spec: None, ballot, members, msg: ToRunner::Probe { ask, elsewhere } }) {
                                out.push(Out::Tell { peer: runner, json });
                            }
                            continue;
                        }
                        Some(members) => {
                            self.hand.acc.probed(&Handle::new(&self.me, self.epoch, &ask.run), members);
                            Act::Tell { runner, msg: ToRunner::Probe { ask, elsewhere } }
                        }
                        None => continue,
                    }
                }
                // This owner's word about a run it adopted or claimed back, to itself as its runner:
                // through the gate, as any runner's word would go.
                Act::Tell { runner, msg } if runner == self.me && handoff::run_of(&msg).and_then(|r| self.hand.ballot_of(r)).is_some() => {
                    let ballot = handoff::run_of(&msg).and_then(|r| self.hand.ballot_of(r));
                    if let Ok(json) = serde_json::to_string(&Envelope { epoch: self.epoch, spec: None, ballot, members: Vec::new(), msg }) {
                        let me = self.me.clone();
                        out.extend(self.told(&me, &json, now));
                    }
                    continue;
                }
                a => a,
            };
            match a {
                Act::Tell { runner, msg } if runner == self.me => {
                    let mut said = Vec::new();
                    let me = Peer { name: &self.me, epoch: self.epoch };
                    match msg {
                        ToRunner::Probe { ask, elsewhere } => {
                            self.admission.carried(me, &ask.run, Some(&*ask), elsewhere, now, &mut said);
                            let (name, epoch) = (self.me.clone(), self.epoch);
                            self.probe(&name, epoch, ask, now, &mut said);
                        }
                        ToRunner::Withdraw { run } => {
                            self.parked.retain(|(p, _, a)| !(*p == self.me && a.run == run));
                            self.admission.withdraw(me, &run, now, &mut said);
                        }
                        ToRunner::Take { run, fence } => self.admission.take(me, &run, fence, now, &mut said),
                        ToRunner::Decline { run, fence } => self.admission.decline(me, &run, fence, now, &mut said),
                        ToRunner::Start { run, fence, elsewhere } => {
                            self.admission.carried(me, &run, None, elsewhere, now, &mut said);
                            self.admission.start(me, &run, fence, now, &mut said);
                        }
                        ToRunner::Ack { run } => self.admission.ack(me, &run),
                        ToRunner::Fast { ask, kernel, epoch, elsewhere } => {
                            self.admission.carried(me, &ask.run, Some(&*ask), elsewhere, now, &mut said);
                            let (name, e) = (self.me.clone(), self.epoch);
                            let _ = self.fast(&name, e, ask, kernel.handle(), epoch, now, &mut said);
                        }
                        ToRunner::Sync => self.admission.sync(me, now, &mut said),
                        // This node's owner hears its own runner's hints, never its tokens: it is
                        // never told another wants, so it gives nothing back here.
                        ToRunner::FastRelease { epoch, amount } => self.admission.fast_release(me, epoch, amount, now, &mut said),
                    }
                    for (to, msg) in said {
                        if to == self.me {
                            let me = self.me.clone();
                            out.extend(self.hand_member_said(&me, &msg, now));
                            queue.extend(self.owner.heard(&me, msg, now));
                        } else if let Ok(json) = serde_json::to_string(&Envelope { epoch: self.epoch, spec: None, ballot: None, members: Vec::new(), msg }) {
                            out.push(Out::Say { peer: to, json });
                        }
                    }
                }
                Act::Tell { runner, msg } => {
                    self.told_to.insert(runner.clone());
                    // A run it adopted, or claimed back: its words go under its claim (LB03).
                    let ballot = handoff::run_of(&msg).and_then(|r| self.hand.ballot_of(r));
                    if let Ok(json) = serde_json::to_string(&Envelope { epoch: self.epoch, spec: None, ballot, members: Vec::new(), msg }) {
                        out.push(Out::Tell { peer: runner, json });
                    }
                }
                Act::Answer { run, answer } => {
                    self.waited(&run, &answer);
                    let over = matches!(answer, Answer::Ended { .. } | Answer::Lost { .. });
                    if over || matches!(answer, Answer::Never { .. }) {
                        self.hand_forget(&run);
                    }
                    // Taken up from before a restart: its agent session is told through told.
                    if self.restored.contains(&run)
                        && let Some(a) = self.askers.get(&run)
                    {
                        out.push(Out::Told { asker: a.clone(), answer: answer.clone() });
                    }
                    if over && self.records.is_some() {
                        self.to_deliver.push(run.clone());
                    }
                    // Budgets settle by what the asker's told (LB08): a run's end frees its rights.
                    let b = self.budget.answered(&run, &answer, now);
                    out.push(Out::Local { run, placed: Placed::Answer(answer) });
                    out.extend(self.budget_outs(b, now));
                }
                Act::Go { run, runner, fence } => {
                    self.budget.go(&run);
                    out.push(Out::Local { run, placed: Placed::Go { runner, fence } });
                }
                Act::Fallback { run, runners } => out.push(Out::Local { run, placed: Placed::Fallback { runners } }),
                // A start said again: the local run's shell says it again with its spec.
                Act::Restart { run, runner, fence } => out.push(Out::Local { run, placed: Placed::Go { runner, fence } }),
            }
        }
        out
    }
}

#[cfg(test)]
mod key_tests {
    use super::*;

    /// AUDIT L1, L2 for owners from before: a runner's strained or barred word goes to an owner
    /// that didn't name `place.health` as `Draining`, a reason it reads, keeping its place, start
    /// and room; any other word goes as it is.
    #[test]
    fn an_owner_from_before_hears_strained_and_barred_as_draining() -> anyhow::Result<()> {
        use kiln_ci::place::msg::{Estimate, Place, Reason};
        let say = |reason: Reason| serde_json::to_string(&Envelope { epoch: 3, spec: None, ballot: None, members: Vec::new(), msg: ToOwner::Waiting { run: "r1".into(), reason, place: Place::of(2), start: Estimate::default(), predicted: None, free: Resources::default(), pace: Default::default() } });
        for r in [Reason::Strained { why: "stalled".into() }, Reason::Barred { cause: "disk".into(), why: "full".into() }] {
            let older: Envelope<ToOwner> = serde_json::from_str(&for_older_owner(say(r)?))?;
            anyhow::ensure!(older.epoch == 3 && matches!(older.msg, ToOwner::Waiting { reason: Reason::Draining, place, .. } if place == Place::of(2)), "{older:?}");
        }
        let line = say(Reason::Line)?;
        anyhow::ensure!(for_older_owner(line.clone()) == line, "a reason it reads goes as it is");
        anyhow::ensure!(crate::proto::feature::speaks(&crate::proto::feature::all(), crate::proto::feature::PLACE_HEALTH), "this build reads them");
        Ok(())
    }

    /// LB08: room tokens' own words (a runner's word to an owner alone, an owner's give-back) are
    /// told apart from every other placement word by their op, which the node holds to the peers
    /// whose links named `place.tokens`; a run whose key spells the op in a string is no such word.
    #[test]
    fn lb08_room_tokens_words_are_known_by_their_op() -> anyhow::Result<()> {
        use kiln_ci::place::msg::FastWord;
        let env = |msg: ToOwner| serde_json::to_string(&Envelope { epoch: 3, spec: None, ballot: None, members: Vec::new(), msg });
        let hint = env(ToOwner::FastHint { word: FastWord { tokens: true, ..FastWord::default() } })?;
        let release = serde_json::to_string(&Envelope { epoch: 3, spec: None, ballot: None, members: Vec::new(), msg: ToRunner::FastRelease { epoch: 1, amount: [1000, 0, 0, 0] } })?;
        anyhow::ensure!(tokens_word(&hint) && tokens_word(&release), "{hint} {release}");
        let ask = Ask { run: "r1".into(), key: Some(r#"cmd:echo "op":"fast_release""#.into()), ..Ask::default() };
        let probe = serde_json::to_string(&Envelope { epoch: 3, spec: None, ballot: None, members: Vec::new(), msg: ToRunner::Probe { ask: std::sync::Arc::new(ask), elsewhere: 0 } })?;
        anyhow::ensure!(!tokens_word(&probe) && !tokens_word(&env(ToOwner::Ran { run: "r1".into() })?), "{probe}");
        anyhow::ensure!(crate::proto::feature::speaks(&crate::proto::feature::all(), crate::proto::feature::PLACE_TOKENS), "this build speaks them");
        Ok(())
    }

    /// AUDIT L2 by run key: a probe carries its asker's key for its kind of work; once this runner
    /// ran a run of that kind (keyed here by its command in its folder here, not its asker's), the
    /// asker's key maps to its own, and a run of that kind killed for memory here bars the next of
    /// it, though it asks for no memory, until memory is back (an event, never a timer).
    #[test]
    fn l2_a_probe_is_barred_by_what_its_kind_of_work_met_here() {
        let mut p = Placer::new("doer", 1, kiln_ci::facts::RunnerFacts::default(), Vec::new(), Sample::default(), Settings::default(), 0);
        let (asker_key, here) = (format!("cmd:asker-{}", crate::util::new_id()), format!("cmd:here-{}", crate::util::new_id()));
        let ask = |run: &str| Ask { run: run.into(), key: Some(asker_key.clone()), ..Ask::default() };
        let mut said = Vec::new();
        p.probe("owner", 1, std::sync::Arc::new(ask("r0")), 0, &mut said);
        p.ran_as("owner", 1, "r0", here.clone());
        assert_eq!(p.key_here(&ask("r1")), Some(here.clone()), "the asker's key maps to this runner's");
        let m = crate::sense::Measure { mem_total: 16 << 30, mem_available: 1 << 30, ..crate::sense::Measure::default() };
        if let Ok(mut h) = crate::sense::health::runner().lock() {
            h.ended(&here, Some(&crate::sense::health::Failure::OutOfMemory), &m, 0, 8 << 30);
        }
        let r = barred(&ask("r1"), p.key_here(&ask("r1")), &m);
        assert!(r.as_ref().is_some_and(|r| r.kind == "memory"), "{r:?}");
        // Another kind of work, asking no memory, isn't barred by it.
        let other = Ask { run: "r2".into(), key: Some(format!("cmd:other-{}", crate::util::new_id())), ..Ask::default() };
        assert_eq!(barred(&other, p.key_here(&other), &m), None);
        // Memory back past what the killed run held: the bar clears as it's measured.
        let back = crate::sense::Measure { mem_available: 12 << 30, ..m };
        if let Ok(mut h) = crate::sense::health::runner().lock() {
            h.measured(&back);
        }
        assert_eq!(barred(&ask("r1"), p.key_here(&ask("r1")), &back), None);
    }

    /// W05-R32 through the shell: an owner from before fair share across the fleet says no usage on
    /// its probes and starts, and they read as before; a probe that carries its tenant's usage
    /// elsewhere is offered as ever; a start this node says carries what it owes that runner, and
    /// none is not said.
    #[test]
    fn w05_r32_probes_and_starts_carry_usage_elsewhere_and_an_older_owner_s_read_as_none() -> anyhow::Result<()> {
        let size = Resources { cpu: Cpus::whole(4), mem: 8 << 30, ..Resources::default() };
        let mut p = Placer::new("doer", 1, RunnerFacts::default(), Vec::new(), Sample { size, ..Sample::default() }, Settings::default(), 0);
        let offered = |out: &[Out], run: &str| out.iter().any(|o| matches!(o, Out::Say { peer, json } if peer == "owner-a" && json.contains(r#""op":"offer""#) && json.contains(&format!(r#""run":"{run}""#))));
        let older = r#"{"epoch":1,"msg":{"op":"probe","ask":{"run":"r1","needs":{"request":{"cpu":1,"mem":0,"disk":0,"gpu":0}},"class":"batch","repo":"acme","actor":"a"}}}"#;
        let out = p.told("owner-a", older, 0);
        anyhow::ensure!(offered(&out, "r1"), "an older owner's probe is offered as before: {out:?}");
        let ask = Ask { run: "r2".into(), needs: kiln_ci::sched::needs::Needs { request: Resources { cpu: Cpus::whole(1), ..Resources::default() }, ..Default::default() }, repo: "acme".into(), actor: "a".into(), ..Ask::default() };
        let carried = serde_json::to_string(&Envelope { epoch: 1, spec: None, ballot: None, members: Vec::new(), msg: ToRunner::Probe { ask: std::sync::Arc::new(ask), elsewhere: 1500 } })?;
        anyhow::ensure!(carried.contains(r#""elsewhere":1500"#), "{carried}");
        let out = p.told("owner-a", &carried, 0);
        anyhow::ensure!(offered(&out, "r2"), "a probe carrying usage is offered as ever: {out:?}");
        let start = p.start_word("r9", Fence { epoch: 1, n: 1 }, 7);
        let e: Envelope<ToRunner> = serde_json::from_str(&start)?;
        anyhow::ensure!(matches!(e.msg, ToRunner::Start { elsewhere: 0, .. }) && !start.contains("elsewhere"), "nothing owed, nothing said: {start}");
        Ok(())
    }
}

#[cfg(test)]
mod measure_tests {
    use super::*;

    /// W05-R26, W05-R03: a decision reads the last measure, never the machine (a read costs
    /// milliseconds on a Mac, a decision nanoseconds). Probes and a CI leg's verdict read nothing;
    /// a run's start or end, the OS's events and the host's sample each make a measure due, read
    /// once the input is done (`settle`), not before.
    #[test]
    fn w05_r26_decisions_read_the_last_measure_not_the_machine() {
        let mut p = Placer::new("doer", 1, RunnerFacts::default(), Vec::new(), Sample::default(), Settings::default(), 0);
        // Its sensor, without `sensing`'s first measure (which feeds sense's health, which other tests share).
        p.sensor = Some(crate::sense::Sensor::new(std::path::Path::new(env!("CARGO_MANIFEST_DIR"))));
        let reads = |p: &Placer| p.sensor.as_ref().map_or(0, crate::sense::Sensor::reads);
        let before = reads(&p);
        let mut said = Vec::new();
        for k in 0..100 {
            p.probe("owner", 1, std::sync::Arc::new(Ask { run: format!("r{k}"), ..Ask::default() }), k, &mut said);
        }
        assert!(reads(&p) == before && !p.measure_due(), "a probe read the machine, or made a measure due");
        let (verdict, _) = p.leg(Ask { run: "leg".into(), ..Ask::default() }, 200);
        assert_eq!(reads(&p), before, "a leg's verdict read the machine");
        assert_eq!(p.measure_due(), verdict.is_ok(), "a leg's start (only) makes a measure due");
        let _ = p.local_ended("r0", true, 300);
        assert!(reads(&p) == before && p.measure_due(), "a run's end read the machine at once, or made no measure due");
        p.machine_changed();
        let _ = p.measured(Sample::default(), 400);
        assert!(reads(&p) == before && p.measure_due(), "an event read the machine before its input was done");
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    /// Room counted once (docs/needs.md E5): a placed run's use is read from its own group, its
    /// CPUs as its `cpu.stat` time over the time since its last read (none at its first read: no
    /// interval yet), its memory as `memory.stat`'s anonymous memory.
    #[test]
    fn a_placed_runs_use_is_read_from_its_own_group() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        std::fs::write(dir.path().join("cpu.stat"), "usage_usec 1000000\nuser_usec 900000\n")?;
        std::fs::write(dir.path().join("memory.stat"), "anon 2147483648\nfile 999\n")?;
        let mut h = Held { at: Group::Dir(dir.path().to_path_buf()), cpu: None };
        let first = group_use(&mut h);
        anyhow::ensure!(first.cpu.get() == 0 && first.mem == 2 << 30, "{first:?}");
        // As if 2 s of CPU time went by over the interval since: some CPUs, never more than 2 s
        // over the (shorter or longer) wall time between the reads, as the kernel counted them.
        let before = h.cpu.map(|(_, t)| t).ok_or_else(|| anyhow::anyhow!("its time kept"))?;
        std::fs::write(dir.path().join("cpu.stat"), "usage_usec 3000000\n")?;
        // The span its second read measures is between these: up to just before that read, and to just after it.
        let shortest = before.elapsed().as_micros();
        let second = group_use(&mut h);
        let longest = before.elapsed().as_micros();
        let milli = u128::from(second.cpu.get());
        anyhow::ensure!(milli > 0 && milli <= 2_000_000_000 / shortest.max(1) + 1 && milli >= 2_000_000_000 / longest.max(1), "{second:?} over {shortest} to {longest} us");
        // No memory controller: its processes' anonymous memory (none here: no processes).
        std::fs::remove_file(dir.path().join("memory.stat"))?;
        anyhow::ensure!(group_use(&mut h).mem == 0);
        Ok(())
    }
}

/// LB03 through the real store (a host's records, Turso's file, in a temp folder): an owner node
/// and a runner node, their words carried between them, each "crashed" by dropping it at a durable
/// point and started again from its records alone.
#[cfg(test)]
mod durable_tests {
    use super::*;
    use kiln_ci::sched::needs::Needs;
    use kiln_ci::sched::state::Class;
    use std::sync::Arc;

    const GIB: u64 = 1 << 30;

    fn info(name: &str) -> RunnerInfo {
        RunnerInfo { name: name.into(), os: "linux".into(), arch: "x86_64".into(), connected: true, reach: true, features: vec![feature::PLACE.into(), feature::PLACE_SYNC.into()], cpus: 8, mem_total: 32 * GIB, ..RunnerInfo::default() }
    }

    fn node(name: &str, epoch: u64) -> Placer {
        let sample = Sample { size: Resources { cpu: Cpus::whole(8), mem: 32 * GIB, disk: 500 * GIB, gpu: 0 }, ..Sample::default() };
        Placer::new(name, epoch, RunnerFacts { os: "linux".into(), arch: "x86_64".into(), ..RunnerFacts::default() }, Vec::new(), sample, Settings::default(), 0)
    }

    /// A node started (again) in `epoch` from what `records` hold.
    async fn boot(name: &str, epoch: u64, records: &Arc<dyn kiln_seams::Records>, peers: &[&str]) -> anyhow::Result<(Placer, Vec<Out>)> {
        let mut p = node(name, epoch);
        p.keep_in(Arc::clone(records));
        let loaded = load(&**records).await?;
        let mut outs = p.restore(loaded, 10);
        let infos: Vec<RunnerInfo> = peers.iter().map(|n| info(n)).collect();
        outs.extend(p.directory(&infos, 10));
        Ok((p, outs))
    }

    /// What `p` changed, kept: as the node does at a batch's end.
    async fn keep(p: &mut Placer) -> anyhow::Result<()> {
        if let Some(r) = p.records() {
            for tx in p.take_unkept() {
                r.commit(tx).await.map_err(|e| anyhow::anyhow!("{e:?}"))?;
            }
        }
        Ok(())
    }

    fn ask(run: &str, key: Option<&str>) -> Ask {
        let needs = Needs { request: Resources { cpu: Cpus::whole(2), mem: 4 * GIB, disk: 0, gpu: 0 }, ..Needs::default() };
        Ask { run: run.into(), needs, class: Class::Interactive, repo: "kiln".into(), actor: "agent".into(), among: vec!["doer".into()], key: key.map(str::to_owned), ..Ask::default() }
    }

    /// Carries every word between `o` (node "asker") and `d` (node "doer") until none is left,
    /// keeping each side's changes before what it said goes (LB03). The local outs it saw.
    async fn carry(o: &mut Placer, d: &mut Placer, outs: Vec<Out>, from_doer: bool, now: i64) -> anyhow::Result<Vec<Out>> {
        let mut seen = Vec::new();
        let mut q: std::collections::VecDeque<(bool, Out)> = outs.into_iter().map(|x| (from_doer, x)).collect();
        while let Some((doer_said, out)) = q.pop_front() {
            match out {
                Out::Tell { peer, json } if !doer_said && peer == "doer" => {
                    keep(o).await?;
                    let more = d.told("asker", &json, now);
                    q.extend(more.into_iter().map(|x| (true, x)));
                }
                Out::Say { peer, json } if doer_said && peer == "asker" => {
                    keep(d).await?;
                    let more = o.heard("doer", &json, now);
                    q.extend(more.into_iter().map(|x| (false, x)));
                }
                other => seen.push(other),
            }
        }
        keep(o).await?;
        keep(d).await?;
        Ok(seen)
    }

    fn go(seen: &[Out]) -> Option<Fence> {
        seen.iter().find_map(|x| if let Out::Local { placed: Placed::Go { fence, .. }, .. } = x { Some(*fence) } else { None })
    }

    fn results(seen: &[Out], run: &str) -> usize {
        seen.iter().filter(|x| matches!(x, Out::Local { run: r, placed: Placed::Answer(Answer::Ended { .. } | Answer::Lost { .. }) } if r == run)).count()
    }

    /// Places `run` from `o` on `d` and starts it there (its start kept first, its record kept
    /// before its process starts), spec 1: the doer's "process" is up.
    async fn start(o: &mut Placer, d: &mut Placer, run: &str, key: Option<&str>) -> anyhow::Result<Fence> {
        let outs = o.ask_from(ask(run, key), None, 20);
        let mut seen = carry(o, d, outs, false, 20).await?;
        let outs = o.tick(5_000);
        seen.extend(carry(o, d, outs, false, 5_000).await?);
        let fence = go(&seen).ok_or_else(|| anyhow::anyhow!("no go: {seen:?}"))?;
        let (ok, outs) = o.starting(run, "doer", fence, 5_001);
        anyhow::ensure!(ok, "its claim is held");
        carry(o, d, outs, false, 5_001).await?;
        let word = o.start_word(run, fence, 1);
        let outs = d.told("asker", &word, 5_002);
        carry(o, d, outs, true, 5_002).await?;
        anyhow::ensure!(matches!(d.spec("asker", 1), Spec::Start { .. }), "its admission said start");
        keep(d).await?;
        let outs = d.spawned("asker", 1, 5_003);
        carry(o, d, outs, true, 5_003).await?;
        Ok(fence)
    }

    /// An MCP run whose caller went, or a detached `kiln run`, placed on another runner: its owner
    /// starts again (its asker's channel gone with it) as it runs. The new process takes it up
    /// from its records, asks the runner what it started (`Sync`), holds it running there, never
    /// places it again, and takes its end once, though that end comes twice.
    #[test]
    fn lb03_an_owner_started_again_takes_up_its_running_run_and_its_one_end() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let dir = tempfile::tempdir()?;
            let (ra, rd): (Arc<dyn kiln_seams::Records>, Arc<dyn kiln_seams::Records>) = (crate::records::open(&dir.path().join("a.db"))?, crate::records::open(&dir.path().join("d.db"))?);
            let (mut o, outs) = boot("asker", 1, &ra, &["doer"]).await?;
            let (mut d, outs_d) = boot("doer", 1, &rd, &["asker"]).await?;
            carry(&mut o, &mut d, outs, false, 10).await?;
            carry(&mut o, &mut d, outs_d, true, 10).await?;
            start(&mut o, &mut d, "r-1", None).await?;
            anyhow::ensure!(o.owner.stage("r-1").is_some_and(|(s, at)| s == "running" && at == Some("doer")), "{:?}", o.owner.stage("r-1"));
            // The owner's process ends; the next one takes up what its records hold.
            drop(o);
            let (mut o, outs) = boot("asker", 2, &ra, &["doer"]).await?;
            let seen = carry(&mut o, &mut d, outs, false, 6_000).await?;
            anyhow::ensure!(o.owner.stage("r-1").is_some_and(|(s, at)| s == "running" && at == Some("doer")), "held running where it runs: {:?}", o.owner.stage("r-1"));
            anyhow::ensure!(!seen.iter().any(|x| matches!(x, Out::Local { placed: Placed::Go { .. }, .. })), "never placed again: {seen:?}");
            // It ends; the end is said twice (a duplicate on the link): one result.
            let outs = d.ended("asker", 1, true, 7_000);
            let dup = outs.clone();
            let mut seen = carry(&mut o, &mut d, outs, true, 7_000).await?;
            seen.extend(carry(&mut o, &mut d, dup, true, 7_001).await?);
            anyhow::ensure!(results(&seen, "r-1") == 1, "one result: {seen:?}");
            anyhow::ensure!(d.admission().records().is_empty(), "its record let go on the owner's word: {:?}", d.admission().records());
            // Kept, then told: nothing of it is left in the owner's records once what told it is kept.
            keep(&mut o).await?;
            let left = load(&*ra).await?;
            anyhow::ensure!(left.runs() == 0, "{left:?}");
            anyhow::Ok(())
        })
    }

    /// An owner whose start of a run was never kept (it went before its commit): started again, it
    /// takes the run up as asked, and the claim its earlier process took is let go at the runner on
    /// its first word; the run starts once.
    #[test]
    fn lb03_a_start_not_kept_is_never_said_and_the_run_starts_once() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let dir = tempfile::tempdir()?;
            let (ra, rd): (Arc<dyn kiln_seams::Records>, Arc<dyn kiln_seams::Records>) = (crate::records::open(&dir.path().join("a.db"))?, crate::records::open(&dir.path().join("d.db"))?);
            let (mut o, outs) = boot("asker", 1, &ra, &["doer"]).await?;
            let (mut d, outs_d) = boot("doer", 1, &rd, &["asker"]).await?;
            carry(&mut o, &mut d, outs, false, 10).await?;
            carry(&mut o, &mut d, outs_d, true, 10).await?;
            let outs = o.ask_from(ask("r-2", None), None, 20);
            let mut seen = carry(&mut o, &mut d, outs, false, 20).await?;
            let outs = o.tick(5_000);
            seen.extend(carry(&mut o, &mut d, outs, false, 5_000).await?);
            let fence = go(&seen).ok_or_else(|| anyhow::anyhow!("no go"))?;
            let (ok, _) = o.starting("r-2", "doer", fence, 5_001);
            anyhow::ensure!(ok);
            // It ends here, before the start is kept: nothing was said.
            drop(o);
            let (mut o, outs) = boot("asker", 2, &ra, &["doer"]).await?;
            let mut seen = carry(&mut o, &mut d, outs, false, 6_000).await?;
            let outs = o.tick(12_000);
            seen.extend(carry(&mut o, &mut d, outs, false, 12_000).await?);
            let fence2 = go(&seen).ok_or_else(|| anyhow::anyhow!("asked again and offered: {seen:?}"))?;
            anyhow::ensure!(fence2 != fence, "a claim of its own");
            anyhow::ensure!(d.admission().probes().iter().filter(|v| v.run == "r-2").count() == 1, "the earlier process's claim let go: {:?}", d.admission().probes());
            anyhow::Ok(())
        })
    }

    /// A child agent's session placed on a runner, the runner started again: its harness goes on,
    /// so its claim's room is held again from the runner's records as it starts (the ledger never
    /// admits past its budget beside it), and its owner, asking what it started, holds it there:
    /// never placed twice. A command it ran can't be followed on: its record ends it, lost. A CI
    /// leg's room is never kept: the server's CI engine keeps its grants.
    #[test]
    fn lb03_a_session_claim_outlives_its_runner_restart_held_and_never_placed_twice() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let dir = tempfile::tempdir()?;
            let (ra, rd): (Arc<dyn kiln_seams::Records>, Arc<dyn kiln_seams::Records>) = (crate::records::open(&dir.path().join("a.db"))?, crate::records::open(&dir.path().join("d.db"))?);
            let (mut o, outs) = boot("asker", 1, &ra, &["doer"]).await?;
            let (mut d, outs_d) = boot("doer", 1, &rd, &["asker"]).await?;
            carry(&mut o, &mut d, outs, false, 10).await?;
            carry(&mut o, &mut d, outs_d, true, 10).await?;
            start(&mut o, &mut d, "session-1", Some("session:claude")).await?;
            let leg = Ask { run: "leg-1".into(), needs: Needs { request: Resources { cpu: Cpus::whole(1), mem: GIB, disk: 0, gpu: 0 }, ..Needs::default() }, ..Ask::default() };
            let (verdict, _) = d.leg(leg, 5_100);
            anyhow::ensure!(verdict.is_ok(), "{verdict:?}");
            keep(&mut d).await?;
            // The runner's process ends; the next starts from its records.
            drop(d);
            let (mut d, outs_d) = boot("doer", 2, &rd, &["asker"]).await?;
            let running: Vec<String> = d.admission().probes().into_iter().filter(|v| v.state == "running").map(|v| v.run).collect();
            anyhow::ensure!(running == vec!["session-1".to_owned()], "the session's room held again, the leg's not kept: {running:?}");
            let session = kiln_ci::place::ask::amounts(&ask("session-1", None).needs.request);
            anyhow::ensure!(d.admission().room().1 == session, "what it holds is the session's: {:?}", d.admission().room().1);
            carry(&mut o, &mut d, outs_d, true, 6_000).await?;
            // The owner hears the runner's new epoch on its next word, and asks what it started.
            let outs = d.tick(6_100);
            let mut seen = carry(&mut o, &mut d, outs, true, 6_100).await?;
            let outs = o.runner_link("doer", false, 6_200);
            seen.extend(carry(&mut o, &mut d, outs, false, 6_200).await?);
            let outs = o.runner_link("doer", true, 6_300);
            seen.extend(carry(&mut o, &mut d, outs, false, 6_300).await?);
            anyhow::ensure!(o.owner.stage("session-1").is_some_and(|(s, at)| s == "running" && at == Some("doer")), "{:?}", o.owner.stage("session-1"));
            anyhow::ensure!(!seen.iter().any(|x| matches!(x, Out::Local { placed: Placed::Go { .. }, .. })), "never placed twice: {seen:?}");
            // The session ends: its room is free, its one result told.
            let claim = Claim { owner: "asker".into(), epoch: 1, run: "session-1".into(), fence: d.admission().probes().first().and_then(|v| v.fence).unwrap_or_default() };
            let outs = d.session_ended(&claim, 7_000);
            let seen = carry(&mut o, &mut d, outs, true, 7_000).await?;
            anyhow::ensure!(results(&seen, "session-1") == 1, "{seen:?}");
            anyhow::ensure!(d.admission().probes().is_empty(), "{:?}", d.admission().probes());
            anyhow::Ok(())
        })
    }

    /// A disk as POSIX keeps one: a file's bytes durable once synced, a rename once its folder is;
    /// a crash loses the rest.
    #[derive(Default)]
    struct SimDisk {
        seen: std::cell::RefCell<(Option<String>, Option<String>, bool)>,
        kept: std::cell::RefCell<(Option<String>, Option<String>)>,
    }

    impl SimDisk {
        fn crash(&self) {
            let k = self.kept.borrow().clone();
            *self.seen.borrow_mut() = (k.0, k.1.clone(), k.1.is_some());
        }
    }

    impl EpochDisk for SimDisk {
        fn read(&self) -> Option<String> {
            self.seen.borrow().0.clone()
        }
        fn write_new(&self, s: &str) -> std::io::Result<()> {
            let mut v = self.seen.borrow_mut();
            v.1 = Some(s.to_owned());
            v.2 = false;
            Ok(())
        }
        fn sync_new(&self) -> std::io::Result<()> {
            let mut v = self.seen.borrow_mut();
            v.2 = true;
            self.kept.borrow_mut().1.clone_from(&v.1);
            Ok(())
        }
        fn rename_new(&self) -> std::io::Result<()> {
            let mut v = self.seen.borrow_mut();
            v.0 = v.1.take();
            Ok(())
        }
        /// The rename on disk: the file's name, with its bytes only if they were synced.
        fn sync_dir(&self) -> std::io::Result<()> {
            let v = self.seen.borrow();
            let mut k = self.kept.borrow_mut();
            k.0 = if v.2 { v.0.clone() } else { Some(String::new()) };
            k.1 = None;
            Ok(())
        }
    }

    /// W05-R34, LB03: a runner crashes after its epoch rose; started again, its epoch is higher
    /// still, and the start its owner said under a claim of the epoch before (on its way as the
    /// runner went) is refused. Planted: the epoch left unsynced is taken up again, and so is that
    /// start.
    #[test]
    fn lb03_an_epoch_is_on_disk_before_its_words_so_a_crash_never_takes_it_up_again() {
        let case = || -> Result<(), String> {
            let disk = SimDisk::default();
            let admission = |epoch: u64| {
                let sample = Sample { size: Resources { cpu: Cpus::whole(8), mem: 32 * GIB, disk: 500 * GIB, gpu: 0 }, ..Sample::default() };
                Admission::new(Init { me: "doer".into(), epoch, since: None, facts: RunnerFacts::default(), labels: Vec::new(), sample, records: Records::new(), settings: Settings::default() }, 0)
            };
            let from = Peer { name: "asker", epoch: 1 };
            let offer = |said: &[(String, ToOwner)]| said.iter().find_map(|(_, m)| if let ToOwner::Offer { fence, .. } = m { Some(*fence) } else { None });
            let e1 = next_epoch_on(&disk).map_err(|e| e.to_string())?;
            let mut a = admission(e1);
            let mut said = Vec::new();
            a.probe(from, ask("x", None), None, 0, &mut said);
            let old = offer(&said).ok_or("offered")?;
            a.take(from, "x", old, 1, &mut said);
            disk.crash();
            let e2 = next_epoch_on(&disk).map_err(|e| e.to_string())?;
            let mut b = admission(e2);
            let mut said = Vec::new();
            b.probe(from, ask("x", None), None, 2, &mut said);
            offer(&said).ok_or("offered again")?;
            let mut said = Vec::new();
            let stale = b.start(from, "x", old, 3, &mut said);
            if e2 <= e1 || stale.is_some() {
                return Err(format!("epoch {e1} then {e2}; the start under {old:?} from before: {stale:?}"));
            }
            Ok(())
        };
        assert_eq!(case(), Ok(()));
        EPOCH_UNSYNCED.with(|c| c.set(true));
        let planted = case();
        EPOCH_UNSYNCED.with(|c| c.set(false));
        assert!(planted.is_err(), "planted: an unsynced epoch is taken up again after a crash: {planted:?}");
    }

    /// LB03: an owner gone from the directory is forgotten at its runner, its ended records let go
    /// in the runner's records too.
    #[test]
    fn lb03_an_owner_gone_from_the_directory_takes_its_ended_records_with_it() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let dir = tempfile::tempdir()?;
            let (ra, rd): (Arc<dyn kiln_seams::Records>, Arc<dyn kiln_seams::Records>) = (crate::records::open(&dir.path().join("a.db"))?, crate::records::open(&dir.path().join("d.db"))?);
            let (mut o, outs) = boot("asker", 1, &ra, &["doer"]).await?;
            let (mut d, outs_d) = boot("doer", 1, &rd, &["asker"]).await?;
            carry(&mut o, &mut d, outs, false, 10).await?;
            carry(&mut o, &mut d, outs_d, true, 10).await?;
            start(&mut o, &mut d, "r-3", None).await?;
            // It ends, and its owner never hears (its link gone): the record stays, ended.
            let _ = d.ended("asker", 1, true, 7_000);
            keep(&mut d).await?;
            anyhow::ensure!(load(&*rd).await?.ran.len() == 1, "kept, ended");
            // The asker leaves the directory: forgotten, its ended record gone from disk too.
            let outs = d.directory(&[info("other")], 8_000);
            carry(&mut o, &mut d, outs, true, 8_000).await?;
            let l = load(&*rd).await?;
            anyhow::ensure!(l.ran.is_empty() && l.forgot.contains_key("asker"), "{l:?}");
            anyhow::Ok(())
        })
    }

    /// What keeping costs a decision, through a host's records on this machine's disk: a start
    /// (the owner's start, then the runner's record) and an end (the record's end and the owner's
    /// result, one batch), each a commit of its own, timed; and a burst of 1,000 asks handled as one
    /// node's batch, kept in one commit. `cargo test -p kiln --lib lb03_cost -- --ignored --nocapture`.
    #[test]
    #[ignore = "a measurement: run by hand on the machine whose disk it measures"]
    fn lb03_cost_of_keeping_a_decision() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let dir = tempfile::tempdir_in(crate::os::home_dir().join(".cache"))?;
            let r: Arc<dyn kiln_seams::Records> = crate::records::open(&dir.path().join("cost.db"))?;
            let row = |i: u32| serde_json::to_vec(&OwnRow { kept: Kept { ask: ask(&format!("run-{i}"), None), epoch: 1, attempt: None, done: None }, asker: None }).unwrap_or_default();
            let mut us = Vec::new();
            for i in 0..4_000u32 {
                let tx = Tx { conds: Vec::new(), writes: vec![Write::Put { family: OWN.into(), key: format!("run-{i}").into_bytes(), value: row(i) }] };
                let t = std::time::Instant::now();
                r.commit(tx).await.map_err(|e| anyhow::anyhow!("{e:?}"))?;
                us.push(t.elapsed().as_micros());
            }
            us.sort_unstable();
            let at = |q: usize| us.get(us.len().saturating_mul(q) / 100).copied().unwrap_or(0);
            println!("one commit (a transition): median {} us, p99 {} us, over {}", at(50), at(99), us.len());
            // A burst: 1,000 asks in one batch, their rows in one commit.
            let mut writes = Vec::new();
            for i in 0..1_000u32 {
                writes.push(Write::Put { family: OWN.into(), key: format!("burst-{i}").into_bytes(), value: row(i) });
            }
            let t = std::time::Instant::now();
            for w in writes.chunks(TX_MAX) {
                r.commit(Tx { conds: Vec::new(), writes: w.to_vec() }).await.map_err(|e| anyhow::anyhow!("{e:?}"))?;
            }
            println!("a burst of 1000 asks kept in {} commit(s): {} us", 1_000usize.div_ceil(TX_MAX), t.elapsed().as_micros());
            // The same burst, a commit each, handed to the store together: its group commit.
            let t = std::time::Instant::now();
            let commits: Vec<_> = (0..1_000u32).map(|i| r.commit(Tx { conds: Vec::new(), writes: vec![Write::Put { family: OWN.into(), key: format!("each-{i}").into_bytes(), value: row(i) }] })).collect();
            for c in futures::future::join_all(commits).await {
                c.map_err(|e| anyhow::anyhow!("{e:?}"))?;
            }
            println!("1000 commits handed together (group commit): {} us", t.elapsed().as_micros());
            anyhow::Ok(())
        })
    }
}

#[cfg(test)]
mod topo_tests {
    use super::*;

    /// LB08: the topology never makes a hint due. A coordinate that moves with the admission
    /// unchanged sends nothing; the next hint the admission makes due carries the newest word.
    #[test]
    fn lb08_an_idle_runner_sends_nothing_for_its_topology() -> anyhow::Result<()> {
        let facts = RunnerFacts { os: "linux".into(), arch: "x86_64".into(), ..RunnerFacts::default() };
        let mut p = Placer::new("me", 1, facts, Vec::new(), Sample::default(), Settings::default(), 0);
        anyhow::ensure!(p.hint_due(0, None).is_some(), "the first hint after it connects is whole");
        let word = |h: i64| crate::reach::topo::Word { c: Some(kiln_ci::topo::Announce { c: kiln_ci::topo::Coord { x: [0, 0], h, e: 1 << 30 }, ..Default::default() }), ..Default::default() };
        for (k, h) in [100_000i64, 250_000, 900_000].into_iter().enumerate() {
            p.topo = Some(word(h));
            let at = 1_000 * (i64::try_from(k).unwrap_or(0) + 1);
            anyhow::ensure!(p.hint_due(at, None).is_none(), "a hint went for the topology alone at {h}");
        }
        p.hints_anew();
        let json = p.hint_due(10_000, None).ok_or_else(|| anyhow::anyhow!("no hint when the admission made one due"))?;
        let hint: crate::offers::hint::Hint = serde_json::from_str(&json)?;
        anyhow::ensure!(hint.topo == Some(word(900_000)), "{:?}", hint.topo);
        Ok(())
    }
}

#[cfg(test)]
mod hex_gpu_port_tests {
    use super::*;
    #[test]
    fn w13_r01_gpu_inventory_survives_host_samples_and_changes_on_local_facts() {
        let facts = RunnerFacts { gpus: vec![kiln_ci::facts::Gpu { model: "A100".into(), memory: 80 << 30, ..Default::default() }; 2], ..RunnerFacts::default() };
        let mut p = Placer::new("gpu-node", 1, facts, Vec::new(), Sample::default(), Settings::default(), 0);
        assert_eq!(p.admission.cap()[3], 2);
        p.measured(Sample::default(), 1);
        assert_eq!(p.admission.cap()[3], 2);
        p.refreshed(RunnerFacts::default(), 2);
        assert_eq!(p.admission.cap()[3], 0);
    }
}
