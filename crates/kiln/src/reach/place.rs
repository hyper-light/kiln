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
//! - **Room** is measured, never assumed: the runner's admission is told each reading of its host
//!   sampler as it comes (until `lb/sense` gives it counters read at decision time), and admits
//!   against that.
//! - **Time** comes in: [`Placer::timer`] says when to be told it again, and the node sets a timer
//!   for then, not a poll.
//! - **No run starts twice**: a spec that comes for a run the admission didn't say `New` to is
//!   refused, never started; one that comes from an asker that doesn't place starts as before.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use kiln_ci::facts::RunnerFacts;
use kiln_ci::place::answer::Answer;
use kiln_ci::place::ask::{Ask, Peer, Sample};
use kiln_ci::place::msg::{ToOwner, ToRunner};
use kiln_ci::place::owner::{Act, Kept, Machine, Owner, Size};
use kiln_ci::place::runner::{Admission, Init, Record, Records, Settings, Start};
use kiln_seams::records::{Scan, TX_MAX, Tx, Write};
use kiln_ci::sched::state::{Cpus, Fence, Resources};

use super::directory::RunnerInfo;
use crate::proto::feature;

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

/// A placement word on a link, either way.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope<M> {
    /// The sender's epoch: its node's, one past the last it used.
    pub epoch: u64,
    /// The spec id of the run a start is for (the `Ask::Run` that follows it on the link).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spec: Option<u64>,
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
    l.since = r.get(META, SINCE, kiln_seams::records::Consistency::Linearizable).await.map_err(|e| anyhow::anyhow!("{e:?}"))?.and_then(|row| row.value.get(..8).and_then(|b| b.try_into().ok()).map(u64::from_be_bytes));
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
    /// What it measures of its machine when it decides (AUDIT L1, L2; none: it admits against the
    /// readings it's fed), the last measure, and whether its machine is under strain, why.
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
}

/// Where a placed run's processes are counted, and its CPU time as last read with when.
#[derive(Debug, Clone)]
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
pub fn next_epoch(dir: &std::path::Path) -> u64 {
    let path = dir.join("place-epoch");
    let last = std::fs::read_to_string(&path).ok().and_then(|s| s.trim().parse::<u64>().ok()).unwrap_or(0);
    let next = last.saturating_add(1);
    if let Err(e) = std::fs::write(&path, next.to_string()) {
        tracing::warn!(error = %e, path = %path.display(), "couldn't keep placement's epoch: a restart may take up this one again");
    }
    next
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
    let size = Size { cpu: (r.cpus > 0).then(|| Cpus::whole(u64::from(r.cpus))), mem: (r.mem_total > 0).then_some(r.mem_total), disk: None, gpu: None };
    Machine { name: r.name.clone(), facts, labels: r.labels.clone(), size, epoch: 0, connected: r.connected && r.reach, places: feature::speaks(&r.features, feature::PLACE), idle: None, syncs: feature::speaks(&r.features, feature::PLACE_SYNC) }
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
        let Loaded { own, ran, since } = loaded;
        let fresh = since.is_none();
        let since = since.or(Some(self.epoch));
        if fresh {
            self.unkept.insert((META, SINCE.to_vec()), Some(self.epoch.to_be_bytes().to_vec()));
        }
        let mut records = Records::new();
        for ((owner, epoch, run), row) in &ran {
            records.entry(owner.clone()).or_default().entry(*epoch).or_default().insert(run.clone(), row.record);
        }
        self.admission.restore_records(records, since);
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
        let acts = self.owner.restore(kept);
        self.acts(acts, now)
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
                _ => META,
            };
            self.unkept.entry((family, k.1)).or_insert(v);
        }
    }

    /// The changes the owner and the admission made, noted to be kept. A CI leg's record isn't kept.
    fn note_changes(&mut self) {
        if self.records.is_none() {
            return;
        }
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
    }

    /// Its start under `fence` couldn't be kept (LB03): it isn't said, and the run waits again.
    pub fn unstarted(&mut self, run: &str, now: i64) -> Vec<Out> {
        let acts = self.owner.unstarted(run);
        self.acts(acts, now)
    }

    /// A run asked of this node by an agent session (`asker`), told through told what it's owed
    /// should this node start again before it's over.
    pub fn ask_from(&mut self, ask: Ask, asker: Option<super::told::Answers>, now: i64) -> Vec<Out> {
        if let Some(a) = asker {
            self.askers.insert(ask.run.clone(), a);
        }
        self.ask(ask, now)
    }

    /// Node `me` in `epoch`, its runner's facts and labels, its first reading and settings.
    pub fn new(me: &str, epoch: u64, facts: RunnerFacts, labels: Vec<String>, sample: Sample, settings: Settings, now: i64) -> Placer {
        let init = Init { me: me.to_owned(), epoch, since: None, facts: facts.clone(), labels: labels.clone(), sample, records: Records::new(), settings };
        Placer { me: me.to_owned(), epoch, owner: Owner::seeded(me, epoch, rand::random()), admission: Admission::new(init, now), specs: BTreeMap::new(), started: BTreeMap::new(), machines: Vec::new(), facts, labels, away: std::collections::BTreeSet::new(), told_to: std::collections::BTreeSet::new(), hinted: None, waits: BTreeMap::new(), gone: Vec::new(), waits_moved: false, whole: true, sensor: None, last: None, strained: None, parked: Vec::new(), groups: std::collections::HashMap::new(), asked_keys: std::collections::HashMap::new(), aliases: std::collections::HashMap::new(), records: None, unkept: BTreeMap::new(), to_deliver: Vec::new(), askers: BTreeMap::new(), restored: std::collections::BTreeSet::new() }
    }

    /// It measures its machine itself when it decides, with `sensor` (AUDIT L1, L2).
    #[must_use]
    pub fn sensing(mut self, sensor: Option<crate::sense::Sensor>) -> Placer {
        self.sensor = sensor;
        // Its size as measured, before anything asks (its owner's own machine is that size).
        self.measure_now(crate::util::now_ms(), &mut Vec::new());
        self
    }

    /// Its machine measured now, at decision time (docs/needs.md E5: room is measured, never
    /// assumed): the room its admission admits against; whether the machine is under strain past
    /// the scheduler's reserve (sense's predicate, the gate guard's: L1, the admission offers
    /// nothing new until a measure shows it isn't); and the causes sense's health keeps that this
    /// measure shows cleared (L2), the probes they parked probed now. Nothing to do without a
    /// sensor.
    fn measure_now(&mut self, now: i64, said: &mut Vec<(String, ToOwner)>) {
        let Some(sensor) = self.sensor.as_mut() else { return };
        let m = sensor.measure();
        let window = sensor.strain().cloned();
        let mut sample = sample_of_measure(&m, self.last.as_ref());
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

    /// Placed work started or ended here: the load strain is weighed under changed now.
    fn load_changed(&mut self) {
        if let Some(s) = self.sensor.as_mut() {
            s.anchor();
        }
    }

    /// Why its machine is under strain now, if it is, as its last measure said (the hint's state).
    pub fn strain(&self) -> Option<String> {
        self.strained.clone()
    }

    /// A probe from `peer` (`epoch`): to its admission, measured first; or, when this runner's
    /// health bars its kind of work (L2), parked here and answered so, until a measure clears it.
    fn probe(&mut self, peer: &str, epoch: u64, ask: std::sync::Arc<Ask>, now: i64, said: &mut Vec<(String, ToOwner)>) {
        self.measure_now(now, said);
        if let Some(k) = &ask.key {
            self.asked_keys.insert((peer.to_owned(), epoch, ask.run.clone()), k.clone());
        }
        let key = self.key_here(&ask);
        if self.strained.is_none()
            && let Some(m) = &self.last
            && let Some(r) = barred(&ask, key, m)
        {
            let free = self.admission.hint();
            said.push((peer.to_owned(), ToOwner::Waiting { run: ask.run.clone(), reason: kiln_ci::place::msg::Reason::Barred { cause: r.kind, why: r.why }, place: kiln_ci::place::msg::Place::default(), start: kiln_ci::place::msg::Estimate::default(), predicted: None, free }));
            self.parked.retain(|(p, _, a)| !(p == peer && a.run == ask.run));
            self.parked.push((peer.to_owned(), epoch, ask));
            return;
        }
        self.admission.probe(Peer { name: peer, epoch }, ask, None, now, said);
    }

    /// The directory came (or changed): the owner's machines, this one among them as it measures
    /// itself.
    pub fn directory(&mut self, runners: &[RunnerInfo], now: i64) -> Vec<Out> {
        let mut ms: Vec<Machine> = runners.iter().filter(|r| r.name != self.me).map(machine_of).collect();
        let cap = self.admission.cap();
        ms.push(Machine { name: self.me.clone(), facts: self.facts.clone(), labels: self.labels.clone(), size: Size { cpu: Some(Cpus::milli(cap[0])), mem: Some(cap[1]), disk: Some(cap[2]).filter(|d| *d > 0), gpu: Some(u32::try_from(cap[3]).unwrap_or(u32::MAX)) }, epoch: self.epoch, connected: true, places: true, idle: None, syncs: true });
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
        let wait = crate::offers::hint::Wait { run: ask.run.clone(), need: ask.needs.describe(), ask: kiln_ci::place::ask::amounts(&ask.needs.request), class: ask.class, since: now, reason: String::new() };
        self.waits.insert(ask.run.clone(), wait);
        let acts = self.owner.ask(ask, now);
        self.acts(acts, now)
    }

    /// Its asker doesn't want it any more.
    pub fn cancel(&mut self, run: &str, now: i64) -> Vec<Out> {
        let acts = self.owner.cancel(run);
        self.acts(acts, now)
    }

    /// The local run `run` is about to start on `runner` under `fence`: whether it still may
    /// ([`Owner::starting`]); for this runner, its admission says whether it starts (`New`).
    pub fn starting(&mut self, run: &str, runner: &str, fence: Fence, now: i64) -> (bool, Vec<Out>) {
        if !self.owner.starting(run, runner, fence) {
            return (false, Vec::new());
        }
        if runner != self.me {
            return (true, Vec::new());
        }
        let mut said = Vec::new();
        let me = Peer { name: &self.me, epoch: self.epoch };
        let started = self.admission.start(me, run, fence, now, &mut said);
        let out = self.said(said, now);
        (started == Some(Start::New), out)
    }

    /// The start of run `run` on `runner` under `fence`, said with the spec id it's for.
    pub fn start_word(&self, run: &str, fence: Fence, spec: u64) -> String {
        serde_json::to_string(&Envelope { epoch: self.epoch, spec: Some(spec), msg: ToRunner::Start { run: run.to_owned(), fence } }).unwrap_or_default()
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
        self.measure_now(now, &mut said);
        self.said(said, now)
    }

    /// An owner's word, from runner `peer`'s link (`Ask::Place`).
    pub fn told(&mut self, peer: &str, json: &str, now: i64) -> Vec<Out> {
        let Ok(e) = serde_json::from_str::<Envelope<ToRunner>>(json) else {
            tracing::debug!(%peer, "a placement word this runner couldn't read");
            return Vec::new();
        };
        let from = Peer { name: peer, epoch: e.epoch };
        let mut said = Vec::new();
        if self.away.remove(peer) {
            self.admission.back(from, now, &mut said);
        }
        match e.msg {
            ToRunner::Probe { ask } => self.probe(peer, e.epoch, ask, now, &mut said),
            ToRunner::Withdraw { run } => {
                self.parked.retain(|(p, _, a)| !(p == peer && a.run == run));
                self.admission.withdraw(from, &run, now, &mut said);
            }
            ToRunner::Take { run, fence } => self.admission.take(from, &run, fence, now, &mut said),
            ToRunner::Decline { run, fence } => self.admission.decline(from, &run, fence, now, &mut said),
            ToRunner::Start { run, fence } => {
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
            ToRunner::Ack { run } => self.admission.ack(from, &run),
            ToRunner::Sync => self.admission.sync(from, now, &mut said),
        }
        self.said(said, now)
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
        let Some((epoch, run)) = self.started.remove(&(peer.to_owned(), id)) else { return Vec::new() };
        self.ungrouped(peer, epoch, &run);
        let mut said = Vec::new();
        self.admission.ended(Peer { name: peer, epoch }, &run, ok, now, &mut said);
        self.measure_now(now, &mut said);
        self.said(said, now)
    }

    /// A runner's word, from `peer`'s link (`Say::Place`).
    pub fn heard(&mut self, peer: &str, json: &str, now: i64) -> Vec<Out> {
        let Ok(e) = serde_json::from_str::<Envelope<ToOwner>>(json) else {
            tracing::debug!(%peer, "a placement word this owner couldn't read");
            return Vec::new();
        };
        let mut acts = self.owner.runner_epoch(peer, e.epoch);
        acts.extend(self.owner.heard(peer, e.msg, now));
        self.acts(acts, now)
    }

    /// A reading of this machine: the room it admits against; where it measures itself, the
    /// moment to (the reading's figures aren't taken).
    pub fn measured(&mut self, sample: Sample, now: i64) -> Vec<Out> {
        let mut said = Vec::new();
        if self.sensor.is_some() {
            self.measure_now(now, &mut said);
        } else {
            self.admission.measured(sample, now, &mut said);
        }
        self.said(said, now)
    }

    /// This node's link to runner `peer` went down, or came back: what this node owns there waits
    /// for it, and is probed again when it's back.
    pub fn runner_link(&mut self, peer: &str, up: bool, now: i64) -> Vec<Out> {
        let acts = if up { self.owner.back(peer) } else { self.owner.away(peer) };
        self.acts(acts, now)
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
        })
    }

    /// The time.
    pub fn tick(&mut self, now: i64) -> Vec<Out> {
        let acts = self.owner.tick(now);
        let mut out = self.acts(acts, now);
        let mut said = Vec::new();
        self.admission.tick(now, &mut said);
        out.extend(self.said(said, now));
        out
    }

    /// When to be told the time again.
    pub fn timer(&self) -> Option<i64> {
        [self.owner.timer(), self.admission.timer()].into_iter().flatten().min()
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
        self.probe(CI, self.epoch, std::sync::Arc::new(ask), now, &mut said);
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
        self.measure_now(now, &mut said);
        said.retain(|(to, _)| to != CI);
        self.said(said, now)
    }

    /// This runner takes nothing new while `draining` (an upgrade pending, or retiring), or does
    /// again: its runs wait saying so, and its hint says it (AUDIT O1).
    pub fn drain(&mut self, draining: bool, now: i64) -> Vec<Out> {
        let mut said = Vec::new();
        self.admission.drain(draining, now, &mut said);
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
        self.measure_now(now, &mut said);
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
        let mut hint = Hint { epoch: self.epoch, seq: 0, at: now, budget: self.admission.cap(), room, queued: self.admission.queued_by_class(), state, quotes, waits: Vec::new(), gone: Vec::new(), whole: self.whole };
        let changed = match &self.hinted {
            None => true,
            Some(last) => {
                let roomy = |h: &Hint| h.room.map(|a| a > 0);
                // The sizes that wait or hold room here: what a change of room is decided by.
                let sizes: std::collections::BTreeSet<[u64; 4]> = self.admission.probes().into_iter().map(|p| p.ask).collect();
                let (was, is) = (Said { hint: last.clone(), heard: last.at }, Said { hint: hint.clone(), heard: hint.at });
                self.whole || self.waits_moved || !self.gone.is_empty() || roomy(last) != roomy(&hint) || relevant(&was, &is, sizes.iter())
            }
        };
        if !changed {
            return None;
        }
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
    }

    /// What the runner's admission said: to this node's owner, heard at once; to another, sent.
    fn said(&mut self, said: Vec<(String, ToOwner)>, now: i64) -> Vec<Out> {
        let out = self.said_in(said, now);
        self.note_changes();
        out
    }

    fn said_in(&mut self, said: Vec<(String, ToOwner)>, now: i64) -> Vec<Out> {
        let mut out = Vec::new();
        for (to, msg) in said {
            if to == self.me {
                let acts = self.owner.heard(&self.me.clone(), msg, now);
                out.extend(self.acts(acts, now));
            } else if let Ok(json) = serde_json::to_string(&Envelope { epoch: self.epoch, spec: None, msg }) {
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
        while let Some(a) = queue.pop_front() {
            match a {
                Act::Tell { runner, msg } if runner == self.me => {
                    let mut said = Vec::new();
                    let me = Peer { name: &self.me, epoch: self.epoch };
                    match msg {
                        ToRunner::Probe { ask } => {
                            let (name, epoch) = (self.me.clone(), self.epoch);
                            self.probe(&name, epoch, ask, now, &mut said);
                        }
                        ToRunner::Withdraw { run } => {
                            self.parked.retain(|(p, _, a)| !(*p == self.me && a.run == run));
                            self.admission.withdraw(me, &run, now, &mut said);
                        }
                        ToRunner::Take { run, fence } => self.admission.take(me, &run, fence, now, &mut said),
                        ToRunner::Decline { run, fence } => self.admission.decline(me, &run, fence, now, &mut said),
                        ToRunner::Start { run, fence } => {
                            self.admission.start(me, &run, fence, now, &mut said);
                        }
                        ToRunner::Ack { run } => self.admission.ack(me, &run),
                        ToRunner::Sync => self.admission.sync(me, now, &mut said),
                    }
                    for (to, msg) in said {
                        if to == self.me {
                            queue.extend(self.owner.heard(&self.me.clone(), msg, now));
                        } else if let Ok(json) = serde_json::to_string(&Envelope { epoch: self.epoch, spec: None, msg }) {
                            out.push(Out::Say { peer: to, json });
                        }
                    }
                }
                Act::Tell { runner, msg } => {
                    self.told_to.insert(runner.clone());
                    if let Ok(json) = serde_json::to_string(&Envelope { epoch: self.epoch, spec: None, msg }) {
                        out.push(Out::Tell { peer: runner, json });
                    }
                }
                Act::Answer { run, answer } => {
                    self.waited(&run, &answer);
                    let over = matches!(answer, Answer::Ended { .. } | Answer::Lost { .. });
                    // Taken up from before a restart: its agent session is told through told.
                    if self.restored.contains(&run)
                        && let Some(a) = self.askers.get(&run)
                    {
                        out.push(Out::Told { asker: a.clone(), answer: answer.clone() });
                    }
                    if over && self.records.is_some() {
                        self.to_deliver.push(run.clone());
                    }
                    out.push(Out::Local { run, placed: Placed::Answer(answer) });
                }
                Act::Go { run, runner, fence } => out.push(Out::Local { run, placed: Placed::Go { runner, fence } }),
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
        let say = |reason: Reason| serde_json::to_string(&Envelope { epoch: 3, spec: None, msg: ToOwner::Waiting { run: "r1".into(), reason, place: Place::of(2), start: Estimate::default(), predicted: None, free: Resources::default() } });
        for r in [Reason::Strained { why: "stalled".into() }, Reason::Barred { cause: "disk".into(), why: "full".into() }] {
            let older: Envelope<ToOwner> = serde_json::from_str(&for_older_owner(say(r)?))?;
            anyhow::ensure!(older.epoch == 3 && matches!(older.msg, ToOwner::Waiting { reason: Reason::Draining, place, .. } if place == Place::of(2)), "{older:?}");
        }
        let line = say(Reason::Line)?;
        anyhow::ensure!(for_older_owner(line.clone()) == line, "a reason it reads goes as it is");
        anyhow::ensure!(crate::proto::feature::speaks(&crate::proto::feature::all(), crate::proto::feature::PLACE_HEALTH), "this build reads them");
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
            carry(&mut o, &mut d, outs, false, 20).await?;
            let outs = o.tick(5_000);
            let seen = carry(&mut o, &mut d, outs, false, 5_000).await?;
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
