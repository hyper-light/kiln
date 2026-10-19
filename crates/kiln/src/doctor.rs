//! What's wrong with a runner, and what fixes it (docs/agentic.md R; docs/releases.md, "When an
//! upgrade fails"): kiln's own checks of a machine, for people (Settings → Updates → Diagnose) and
//! agents (`runners.diagnose`), and the repairs kiln makes there itself (`runners.repair`).
//!
//! A diagnosis has two halves. What the server knows of the machine ([`server_side`]): whether
//! it's connected, the kiln it runs against the newest, its last upgrade and how that went, and
//! its memory and toolchains as it last reported them. And what the machine finds about itself
//! when asked ([`examine`], sent as [`ServerMsg::Diagnose`] to runners announcing
//! [`proto::CAP_DOCTOR`]): the room on its disks, the cache and the newest log of its self-builds,
//! its clock, and on a Mac whether kiln is signed. Each finding says what was seen and, when kiln
//! can fix it, the repair that does ([`Fix`]).
//!
//! No repair stops what the machine runs: none restarts its runner, and a cache is never cleared
//! while an upgrade builds from it. A repair that would need a person at the machine (setting up
//! signing on a Mac, which macOS asks about) says so instead of offering itself.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use http::{Method, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::hub::Hub;
use crate::ops::{Cx, Fail, Op, Out, path, schema, text};
use crate::proto::{self, ServerMsg};

/// How much a finding matters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    /// As it should be.
    Ok,
    /// Worth knowing; nothing is wrong.
    Info,
    /// It works, but won't for long, or works badly.
    Warn,
    /// It's what breaks upgrades or work here.
    Fail,
}

/// A repair that fixes a finding: its action ([`RUNNER_ACTIONS`], [`SERVER_ACTIONS`]) and what a
/// button says.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fix {
    pub action: String,
    pub label: String,
    /// For `retry-upgrade`: the way to upgrade (`download`, `build`); none for kiln's choice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub way: Option<String>,
}

/// One thing a diagnosis found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    /// What was checked (`disk.work`, `upgrade.last`, …), the same every time.
    pub id: String,
    pub level: Level,
    /// The one sentence that matters.
    pub title: String,
    /// What was seen, in more words (or a log's last lines).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix: Option<Fix>,
}

fn finding(id: &str, level: Level, title: impl Into<String>, detail: impl Into<String>, fix: Option<Fix>) -> Finding {
    Finding { id: id.to_owned(), level, title: title.into(), detail: detail.into(), fix }
}

fn fix(action: &str, label: &str, way: Option<&str>) -> Option<Fix> {
    Some(Fix { action: action.to_owned(), label: label.to_owned(), way: way.map(str::to_owned) })
}

/// What a machine found about itself ([`examine`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Machine {
    /// Its clock when it looked (Unix ms): the server compares it with its own.
    pub now_ms: i64,
    pub findings: Vec<Finding>,
}

/// Repairs a runner makes on its own machine ([`repair_here`]).
pub const RUNNER_ACTIONS: &[&str] = &["clear-self-build-cache", "prune-versions", "re-sign"];
/// Repairs the server makes for a runner.
pub const SERVER_ACTIONS: &[&str] = &["retry-upgrade"];

const GIB: u64 = 1 << 30;
/// A self-build cache bigger than this is worth clearing when room is short (or said of anyway).
const BIG_CACHE: u64 = 20 * GIB;
/// The most entries a size walk visits, so a huge cache can't keep a diagnosis waiting.
const WALK_ENTRIES: usize = 400_000;
/// How much of a log's end is read, and how many of its last lines are kept.
const TAIL_BYTES: u64 = 64 << 10;
const TAIL_LINES: usize = 25;
/// A clock this far from the server's (either way) is said.
const SKEW_MS: i64 = 60_000;
/// How long a machine has to answer a diagnosis, and a repair.
const DIAGNOSE_WAIT: Duration = Duration::from_secs(45);
const REPAIR_WAIT: Duration = Duration::from_secs(120);

// ------------------------------------------------------------------------------- on the machine

/// Where a runner keeps what a diagnosis looks at.
#[derive(Debug, Clone)]
pub struct Here {
    /// The runner's state (its instances, the builds it keeps under `versions/`).
    pub state_dir: PathBuf,
    /// Where its jobs and agents work.
    pub work_dir: PathBuf,
}

/// Where this machine builds kiln itself (`crate::fleet`'s self-build: a source tree per build
/// beside one cache of compiled crates, and a log per build).
fn self_build_dir() -> Option<PathBuf> {
    std::env::home_dir().map(|h| h.join(".cache").join("kiln").join("self-build"))
}

/// This machine's checks: its disks, its self-builds' cache and newest log, and (macOS) signing.
pub fn examine(here: &Here) -> Machine {
    let mut findings = Vec::new();
    let build_dir = self_build_dir();
    let cache = build_dir.as_deref().filter(|d| d.is_dir()).map(|d| size_of(d, WALK_ENTRIES));
    findings.push(disk_finding("disk.work", "where jobs and agents work", &here.work_dir, crate::runner::probe::volume(&here.work_dir), cache));
    if !same_volume(&here.state_dir, &here.work_dir) {
        findings.push(disk_finding("disk.state", "where the runner keeps its state and builds", &here.state_dir, crate::runner::probe::volume(&here.state_dir), cache));
    }
    if let Some((bytes, partial)) = cache {
        let size = said_bytes(bytes);
        let more = if partial { " or more (too many files to count them all)" } else { "" };
        findings.push(if bytes >= BIG_CACHE {
            finding("self_build.cache", Level::Warn, format!("Self-builds keep {size}{more} of compiled crates"), "It's a cache: clearing it frees the room, and the next self-build compiles from scratch.", fix("clear-self-build-cache", "Clear the self-build cache", None))
        } else {
            finding("self_build.cache", Level::Info, format!("Self-builds keep {size}{more} of compiled crates"), "", None)
        });
    }
    if let Some((log, at)) = build_dir.as_deref().and_then(newest_log) {
        let ago = std::time::SystemTime::now().duration_since(at).map(|d| d.as_secs()).unwrap_or(0);
        let tail = tail_of(&log, TAIL_BYTES, TAIL_LINES);
        let failed = tail.lines().any(|l| l.starts_with("error") || l.contains("error[E") || l.contains("could not compile") || l.contains("Killed"));
        findings.push(finding(
            "self_build.log",
            if failed { Level::Warn } else { Level::Info },
            if failed { format!("The newest self-build ({}, {} ago) ended with errors", file_name(&log), said_secs(ago)) } else { format!("The newest self-build log is {} ({} ago)", file_name(&log), said_secs(ago)) },
            tail,
            None,
        ));
    }
    if let Some(f) = signing() {
        findings.push(f);
    }
    Machine { now_ms: crate::util::now_ms(), findings }
}

/// A repair on this machine (one of [`RUNNER_ACTIONS`]): what it did, or why it couldn't.
pub fn repair_here(here: &Here, action: &str) -> Result<String, String> {
    match action {
        "clear-self-build-cache" => {
            if crate::updates::upgrading() {
                return Err("an upgrade is running on this machine: its build may be using the cache, so it stays".to_owned());
            }
            let dir = self_build_dir().ok_or("this machine has no home folder, so no self-build cache")?;
            if !dir.is_dir() {
                return Ok("There's no self-build cache on this machine: nothing to clear.".to_owned());
            }
            let entries = std::fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
            let (mut freed, mut kept_logs) = (0u64, 0usize);
            for e in entries.filter_map(Result::ok) {
                let p = e.path();
                // The logs stay: they're what a diagnosis reads.
                if p.extension().is_some_and(|x| x == "log") {
                    kept_logs = kept_logs.saturating_add(1);
                    continue;
                }
                let (bytes, _) = size_of(&p, WALK_ENTRIES);
                let gone = if p.is_dir() { std::fs::remove_dir_all(&p) } else { std::fs::remove_file(&p) };
                match gone {
                    Ok(()) => freed = freed.saturating_add(bytes),
                    Err(e) => return Err(format!("{} couldn't be removed ({e}); {} freed before it", p.display(), said_bytes(freed))),
                }
            }
            Ok(format!("Freed {} of self-build cache; its {kept_logs} log{} stay.", said_bytes(freed), if kept_logs == 1 { "" } else { "s" }))
        }
        "prune-versions" => {
            let dir = here.state_dir.join("versions");
            let (before, _) = size_of(&dir, WALK_ENTRIES);
            crate::updates::prune_versions(&here.state_dir, &crate::updates::own_build());
            let (after, _) = size_of(&dir, WALK_ENTRIES);
            Ok(format!("Removed older kiln builds kept under {}: {} freed; the one running and the newest few stay.", dir.display(), said_bytes(before.saturating_sub(after))))
        }
        "re-sign" => re_sign(),
        other => Err(format!("{other} isn't a repair a runner makes (those are {})", RUNNER_ACTIONS.join(", "))),
    }
}

#[cfg(target_os = "macos")]
fn signing() -> Option<Finding> {
    use crate::os::macsign::State;
    Some(match crate::os::macsign::state() {
        State::Ready(id) => finding("signing", Level::Ok, "kiln is signed here, so macOS keeps letting it onto the local network after upgrades", id, None),
        State::Missing(why) => finding(
            "signing",
            Level::Warn,
            "kiln isn't signed here: after each upgrade macOS may cut it off from the local network until someone allows it again",
            format!("{why}. Run `kiln signing setup` on the Mac once (macOS asks to trust kiln's certificate there), then repair with Sign kiln again."),
            fix("re-sign", "Sign kiln again", None),
        ),
        State::NotNeeded => return None,
    })
}

#[cfg(not(target_os = "macos"))]
fn signing() -> Option<Finding> {
    None
}

#[cfg(target_os = "macos")]
fn re_sign() -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|e| format!("this runner can't tell where its kiln is: {e}"))?;
    match crate::os::macsign::resign(&exe) {
        Ok(true) => Ok(format!("Signed {} again as kiln.", exe.display())),
        Ok(false) => Err("this Mac has no signing identity for kiln yet: run `kiln signing setup` on it once (macOS asks to trust the certificate)".to_owned()),
        Err(e) => Err(format!("{e:#}")),
    }
}

#[cfg(not(target_os = "macos"))]
fn re_sign() -> Result<String, String> {
    Err("only a Mac signs kiln: there's nothing to sign here".to_owned())
}

/// Whether `a` and `b` are on one volume (so one finding says the room of both).
#[cfg(unix)]
fn same_volume(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(x), Ok(y)) => x.dev() == y.dev(),
        _ => false,
    }
}

#[cfg(not(unix))]
fn same_volume(a: &Path, b: &Path) -> bool {
    a.components().next() == b.components().next()
}

/// Room on the volume holding `dir`, said of `what`.
fn disk_finding(id: &str, what: &str, dir: &Path, disk: Option<kiln_ci::facts::Disk>, cache: Option<(u64, bool)>) -> Finding {
    let Some(d) = disk.filter(|d| d.total > 0) else {
        return finding(id, Level::Info, format!("kiln can't tell how much room is left {what}"), dir.display().to_string(), None);
    };
    let pct = d.free.saturating_mul(100) / d.total;
    let level = if d.free < GIB || pct < 2 {
        Level::Fail
    } else if d.free < 5 * GIB || pct < 10 {
        Level::Warn
    } else {
        Level::Ok
    };
    let title = format!("{} free of {} {what}", said_bytes(d.free), said_bytes(d.total));
    // Short of room with a big self-build cache: clearing it is the repair.
    let fix = match (level, cache) {
        (Level::Fail | Level::Warn, Some((bytes, _))) if bytes >= GIB => fix("clear-self-build-cache", &format!("Clear the self-build cache ({})", said_bytes(bytes)), None),
        _ => None,
    };
    finding(id, level, title, dir.display().to_string(), fix)
}

/// The bytes under `p` (not following links), counting at most `budget` entries: `(bytes, partial)`.
fn size_of(p: &Path, budget: usize) -> (u64, bool) {
    let mut total = 0u64;
    let mut left = budget;
    let mut stack = vec![p.to_path_buf()];
    while let Some(next) = stack.pop() {
        let Ok(meta) = std::fs::symlink_metadata(&next) else { continue };
        if left == 0 {
            return (total, true);
        }
        left -= 1;
        if meta.is_dir() {
            if let Ok(rd) = std::fs::read_dir(&next) {
                stack.extend(rd.filter_map(Result::ok).map(|e| e.path()));
            }
        } else {
            total = total.saturating_add(meta.len());
        }
    }
    (total, false)
}

/// The newest `build-*.log` in `dir`, and when it was written.
fn newest_log(dir: &Path) -> Option<(PathBuf, std::time::SystemTime)> {
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_str().is_some_and(|n| n.starts_with("build-") && n.ends_with(".log")))
        .filter_map(|e| Some((e.path(), e.metadata().ok()?.modified().ok()?)))
        .max_by_key(|(_, at)| *at)
}

/// The last `lines` lines of the file at `path`, reading at most its last `bytes`.
fn tail_of(path: &Path, bytes: u64, lines: usize) -> String {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(path) else { return String::new() };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    if f.seek(SeekFrom::Start(len.saturating_sub(bytes))).is_err() {
        return String::new();
    }
    let mut buf = Vec::new();
    if f.take(bytes).read_to_end(&mut buf).is_err() {
        return String::new();
    }
    last_lines(&String::from_utf8_lossy(&buf), lines)
}

/// The last `n` lines of `text`, colour codes taken out.
fn last_lines(text: &str, n: usize) -> String {
    let all: Vec<String> = text.lines().map(strip_ansi).filter(|l| !l.trim().is_empty()).collect();
    all.iter().skip(all.len().saturating_sub(n)).cloned().collect::<Vec<_>>().join("\n")
}

fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

fn file_name(p: &Path) -> String {
    p.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_owned()
}

/// Bytes in words: `512 MiB`, `3.4 GiB`.
pub fn said_bytes(b: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = b as f64;
    let mut unit = "B";
    for u in UNITS {
        unit = u;
        if v < 1024.0 || u == "TiB" {
            break;
        }
        v /= 1024.0;
    }
    if unit == "B" || v >= 100.0 { format!("{v:.0} {unit}") } else { format!("{v:.1} {unit}") }
}

/// Seconds in words: `40 s`, `12 min`, `3 h`, `2 days`.
fn said_secs(s: u64) -> String {
    match s {
        0..60 => format!("{s} s"),
        60..3600 => format!("{} min", s / 60),
        3600..172_800 => format!("{} h", s / 3600),
        _ => format!("{} days", s / 86_400),
    }
}

// ------------------------------------------------------------------------------- on the server

/// A runner's last upgrade, as a diagnosis reads it (`crate::updates::Job`).
#[derive(Debug, Clone)]
pub struct LastUpgrade {
    pub state: crate::updates::JobState,
    /// "native", "terminal", "build", …
    pub method: &'static str,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub log: Vec<String>,
    pub error: Option<String>,
}

impl From<&crate::updates::Job> for LastUpgrade {
    fn from(j: &crate::updates::Job) -> Self {
        LastUpgrade { state: j.state, method: j.method, started_at: j.started_at, ended_at: j.ended_at, log: j.log.clone(), error: j.error.clone() }
    }
}

/// What the server knows of runner `name` (its updates row, as Settings → Updates shows it; how it
/// was when last heard; its last upgrade), as findings.
pub fn server_side(info: Option<&crate::updates::RunnerInfo>, live: Option<&Live>, last_job: Option<&LastUpgrade>) -> Vec<Finding> {
    let mut out = Vec::new();
    match live {
        Some(l) if l.connected => out.push(finding("connection", Level::Ok, "Connected to this kiln", "", None)),
        Some(l) => out.push(finding(
            "connection",
            Level::Fail,
            format!("Not connected; last heard from {} ago", said_secs(u64::try_from(crate::util::now_unix().saturating_sub(l.last_seen)).unwrap_or(0))),
            "Until it's back, kiln can't look at the machine itself. If it's asleep or switched off, wake it; if its runner stopped, start it there (kiln agent, or its service).",
            None,
        )),
        None => {}
    }
    if let Some(i) = info {
        let target = i.target_build.as_deref().map(short).unwrap_or("none for its platform yet");
        if i.locked {
            out.push(finding("kiln.build", Level::Info, "Its machine refuses upgrades ([updates] accept = false in its kiln config)", "kiln never upgrades it until that changes on the machine.", None));
        } else if i.up_to_date {
            out.push(finding("kiln.build", Level::Ok, format!("Runs the newest kiln ({})", i.build.as_deref().map(short).unwrap_or("its build")), "", None));
        } else {
            let next = i.next.clone().unwrap_or_default();
            let ways: Vec<&crate::updates::WayOpen> = i.ways.iter().filter(|w| w.ok).collect();
            let fix = ways.first().map(|w| Fix { action: "retry-upgrade".to_owned(), label: format!("Upgrade it now ({})", way_word(w.way)), way: Some(way_word(w.way).to_owned()) });
            out.push(finding(
                "kiln.build",
                Level::Warn,
                format!("Behind the newest kiln: runs {}, the newest for it is {target}", i.build.as_deref().map(short).unwrap_or("an older build")),
                if next.is_empty() { String::new() } else { format!("Next, it {next}.") },
                fix,
            ));
        }
    }
    if let Some(j) = last_job {
        let when = said_secs(u64::try_from(crate::util::now_unix().saturating_sub(j.ended_at.unwrap_or(j.started_at))).unwrap_or(0));
        let tail = last_lines(&j.log.join("\n"), TAIL_LINES);
        match j.state {
            crate::updates::JobState::Failed => {
                let error = j.error.clone().unwrap_or_else(|| "it failed".to_owned());
                let detail = if tail.is_empty() { error.clone() } else { format!("{error}\n\n{tail}") };
                let other = info.and_then(|i| i.ways.iter().find(|w| w.ok && Some(way_word(w.way)) != method_way(j.method)));
                let retry = match other {
                    Some(w) => fix("retry-upgrade", &format!("Upgrade it the other way ({})", way_word(w.way)), Some(way_word(w.way))),
                    None => fix("retry-upgrade", "Try the upgrade again", method_way(j.method)),
                };
                out.push(finding("upgrade.last", Level::Fail, format!("Its last upgrade failed {when} ago ({})", j.method), detail, retry));
                if let Some((need, have)) = rust_needed(&j.log) {
                    let have = have.or_else(|| live.and_then(|l| l.tools.get("rustc")).and_then(|v| crate::runner::probe::version_in(v))).unwrap_or_else(|| "older".to_owned());
                    out.push(finding(
                        "toolchain.rust",
                        Level::Fail,
                        format!("Its Rust is {have}; building kiln needs {need} or newer"),
                        "Update Rust on the machine (rustup update), or upgrade it by download instead.",
                        info.and_then(|i| i.ways.iter().find(|w| w.ok && w.way == crate::updates::Way::Download)).and_then(|_| fix("retry-upgrade", "Upgrade it by download", Some("download"))),
                    ));
                }
            }
            crate::updates::JobState::Running => out.push(finding("upgrade.last", Level::Info, format!("An upgrade is under way ({}, started {when} ago)", j.method), tail, None)),
            crate::updates::JobState::Done => out.push(finding("upgrade.last", Level::Ok, format!("Its last upgrade worked ({when} ago)"), "", None)),
        }
    }
    if let Some(s) = live.and_then(|l| l.sample.as_ref()).filter(|s| s.mem_total > 0) {
        let pct = s.mem_used.saturating_mul(100) / s.mem_total;
        if pct >= 95 {
            out.push(finding("memory", Level::Warn, format!("Memory is {pct}% used ({} of {})", said_bytes(s.mem_used), said_bytes(s.mem_total)), "Builds and agents here may be killed for memory.", None));
        }
    }
    out
}

fn short(build: &str) -> &str {
    build.get(..12).unwrap_or(build)
}

fn way_word(w: crate::updates::Way) -> &'static str {
    match w {
        crate::updates::Way::Download => "download",
        crate::updates::Way::Build => "build",
    }
}

/// The way an upgrade job went, as `retry-upgrade` names ways.
fn method_way(method: &str) -> Option<&'static str> {
    match method {
        "build" => Some("build"),
        "native" => Some("download"),
        _ => None,
    }
}

/// The Rust a failed build needed, and the one it had, as cargo says them ("… requires rustc
/// 1.98.1 or newer, while the currently active rustc version is 1.94.0", or "rustc 1.94.0 is not
/// supported by the following packages: … requires rustc 1.98.1").
fn rust_needed(log: &[String]) -> Option<(String, Option<String>)> {
    let after = |line: &str, mark: &str| line.find(mark).and_then(|i| line.get(i + mark.len()..)).and_then(crate::runner::probe::version_in);
    let need = log.iter().filter_map(|l| after(l, "requires rustc ")).max_by(|a, b| version_key(a).cmp(&version_key(b)))?;
    let have = log.iter().find_map(|l| after(l, "currently active rustc version is ").or_else(|| l.contains("is not supported by").then(|| after(l, "rustc ")).flatten()));
    Some((need, have))
}

/// A version's numbers, to compare (`1.98.1` > `1.94.0`).
fn version_key(v: &str) -> Vec<u64> {
    v.split('.').map(|p| p.parse().unwrap_or(0)).collect()
}

/// How the server last heard a runner.
#[derive(Debug, Clone, Default)]
pub struct Live {
    pub connected: bool,
    pub last_seen: i64,
    pub sample: Option<crate::metrics::HostSample>,
    pub tools: std::collections::BTreeMap<String, String>,
}

fn live_of(hub: &Hub, name: &str) -> Option<Live> {
    let agents = hub.agents.lock();
    let a = agents.get(name)?;
    Some(Live { connected: a.connected, last_seen: a.last_seen, sample: a.sample.clone(), tools: a.facts.as_ref().map(|f| f.tools.clone()).unwrap_or_default() })
}

/// Runner `name`, diagnosed: what the server knows, and what the machine finds about itself.
pub async fn diagnose(hub: &Arc<Hub>, name: &str) -> Result<Value, Fail> {
    let info = crate::updates::runners(hub).await.into_iter().find(|r| r.name == name);
    let live = live_of(hub, name);
    if info.is_none() && live.is_none() {
        return Err((StatusCode::NOT_FOUND, format!("kiln has no runner called {name}")));
    }
    let last_job = hub.updates.jobs().iter().filter(|j| j.kind == "runner" && j.target == name).max_by_key(|j| j.started_at).map(LastUpgrade::from);
    let mut findings = server_side(info.as_ref(), live.as_ref(), last_job.as_ref());
    let mut why: Option<String> = None;
    if live.as_ref().is_some_and(|l| l.connected) {
        let asked = crate::util::now_ms();
        match hub.ask_runner(name, proto::CAP_DOCTOR, DIAGNOSE_WAIT, |req| ServerMsg::Diagnose { req }).await {
            Ok(Some(Some(json))) => match serde_json::from_str::<Machine>(&json) {
                Ok(m) => {
                    // Its clock, against the server's halfway through the question.
                    let mid = asked.saturating_add(crate::util::now_ms().saturating_sub(asked) / 2);
                    let skew = m.now_ms.saturating_sub(mid);
                    if skew.abs() >= SKEW_MS {
                        findings.push(finding("clock", Level::Warn, format!("Its clock is {} {} the server's", said_secs(skew.unsigned_abs() / 1000), if skew > 0 { "ahead of" } else { "behind" }), "Certificates, tokens and schedules go wrong with a clock this far off: turn on network time on the machine.", None));
                    } else {
                        findings.push(finding("clock", Level::Ok, "Its clock agrees with the server's", "", None));
                    }
                    findings.extend(m.findings);
                }
                Err(e) => why = Some(format!("its answer was unreadable: {e}")),
            },
            Ok(Some(None)) => why = Some("it couldn't look at itself".to_owned()),
            Ok(None) => why = Some("its kiln is too old to look at itself: upgrade it to see its disks, caches and logs here".to_owned()),
            Err(e) => why = Some(format!("{e:#}")),
        }
    } else {
        why = Some("it isn't connected".to_owned());
    }
    // The worst first, then as checked.
    findings.sort_by_key(|f| std::cmp::Reverse(f.level));
    let worst = findings.first().map_or(Level::Ok, |f| f.level);
    Ok(json!({
        "runner": name,
        "at": crate::util::now_ms(),
        "worst": worst,
        "findings": findings,
        "machine_checked": why.is_none(),
        "why": why,
        "updates": info,
    }))
}

/// What a repair asks for.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Ask {
    pub action: String,
    /// For `retry-upgrade`: `download` or `build`.
    #[serde(default)]
    pub way: Option<String>,
}

/// A repair of runner `name`: `{ok, action, message}` (or `{ok, action, job}` for an upgrade).
pub async fn repair(hub: &Arc<Hub>, name: &str, ask: &Ask) -> Result<Value, Fail> {
    let action = ask.action.trim();
    if action == "retry-upgrade" {
        let way = match ask.way.as_deref().map(str::trim).filter(|w| !w.is_empty()) {
            None => None,
            Some("download") => Some(crate::updates::Way::Download),
            Some("build") => Some(crate::updates::Way::Build),
            Some(other) => return Err((StatusCode::BAD_REQUEST, format!("{other} isn't a way to upgrade (download or build)"))),
        };
        let job = crate::updates::upgrade_runner(hub, name, way).await.map_err(|e| (StatusCode::CONFLICT, format!("{e:#}")))?;
        return Ok(json!({"ok": true, "action": action, "job": job}));
    }
    if !RUNNER_ACTIONS.contains(&action) {
        return Err((StatusCode::BAD_REQUEST, format!("{action:?} isn't a repair: they are {}", RUNNER_ACTIONS.iter().chain(SERVER_ACTIONS).copied().collect::<Vec<_>>().join(", "))));
    }
    if !live_of(hub, name).is_some_and(|l| l.connected) {
        return Err((StatusCode::CONFLICT, format!("{name} isn't connected")));
    }
    if action == "clear-self-build-cache" && hub.updates.jobs().iter().any(|j| j.kind == "runner" && j.target == name && j.state == crate::updates::JobState::Running) {
        return Err((StatusCode::CONFLICT, format!("{name} is being upgraded: its build may be using the cache, so it stays until that's done")));
    }
    let a = action.to_owned();
    match hub.ask_runner(name, proto::CAP_DOCTOR, REPAIR_WAIT, move |req| ServerMsg::Repair { req, action: a }).await {
        Ok(Some(Some(json))) => {
            let v: Value = serde_json::from_str(&json).map_err(|e| (StatusCode::BAD_GATEWAY, format!("{name}'s answer was unreadable: {e}")))?;
            match v.get("error").and_then(Value::as_str) {
                Some(e) => Err((StatusCode::CONFLICT, e.to_owned())),
                None => Ok(json!({"ok": true, "action": action, "message": v.get("message").cloned().unwrap_or(Value::Null)})),
            }
        }
        Ok(Some(None)) => Err((StatusCode::BAD_GATEWAY, format!("{name} didn't say how the repair went"))),
        Ok(None) => Err((StatusCode::CONFLICT, format!("{name}'s kiln is too old to repair itself: upgrade it first"))),
        Err(e) => Err((StatusCode::BAD_GATEWAY, format!("{e:#}"))),
    }
}

/// A repair on this machine, as the runner answers [`ServerMsg::Repair`]: `{message}` or `{error}`.
pub fn answer_repair(here: &Here, action: &str) -> String {
    match repair_here(here, action) {
        Ok(m) => json!({"message": m}).to_string(),
        Err(e) => json!({"error": e}).to_string(),
    }
}

// ------------------------------------------------------------------------------- operations

const RUNNER: &str = "The runner (machine) by name, as Runners lists it.";

fn runner_of(v: &Value) -> Result<String, Fail> {
    text(v, "runner")
}

/// The runners' operations (docs/agentic.md R1–R3): list, diagnose, repair, and their upgrades.
pub fn list_ops() -> Vec<Op> {
    vec![
        Op {
            name: "runners.list",
            summary: "Every runner (machine) of this kiln: connected or not, its OS and architecture, labels, slots, what it runs now, its kiln version, battery, and when it was last heard.",
            read_only: true,
            destructive: false,
            input: || schema(json!({}), &[]),
            route: |_| Ok((Method::GET, path(&["api", "agents"]))),
            run: |cx, _| Box::pin(async move { serde_json::to_value(cx.hub.agent_list().await).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())) }),
        },
        Op {
            name: "runners.diagnose",
            summary: "Diagnose a runner: what the server knows (connected, its kiln against the newest, its last upgrade with its error and log, memory, the Rust a self-build needs) and what the machine finds about itself (room on its disks, the self-build cache and newest log, its clock, macOS signing). Findings come worst first; one kiln can fix carries `fix` ({action, label, way?}) to pass to runners.repair.",
            read_only: true,
            destructive: false,
            input: || schema(json!({"runner": {"type": "string", "description": RUNNER}}), &["runner"]),
            route: |v| Ok((Method::GET, path(&["api", "agents", &runner_of(v)?, "diagnose"]))),
            run: |cx, v| Box::pin(async move { diagnose(&cx.hub, &runner_of(&v)?).await }),
        },
        Op {
            name: "runners.repair",
            summary: "Repair a runner, as a diagnosis's `fix` says: `retry-upgrade` (with `way` download or build: upgrade it now, nothing it runs stops), `clear-self-build-cache` (frees what self-builds compiled; refused while an upgrade runs), `prune-versions` (older kiln builds the runner keeps), `re-sign` (macOS: sign kiln again). None restarts the runner or stops what it runs.",
            read_only: false,
            destructive: false,
            input: || {
                schema(
                    json!({
                        "runner": {"type": "string", "description": RUNNER},
                        "action": {"type": "string", "enum": ["retry-upgrade", "clear-self-build-cache", "prune-versions", "re-sign"]},
                        "way": {"type": "string", "enum": ["download", "build"], "description": "For retry-upgrade: how; by default the way kiln can."}
                    }),
                    &["runner", "action"],
                )
            },
            route: |v| Ok((Method::POST, path(&["api", "agents", &runner_of(v)?, "repair"]))),
            run: |cx, v| {
                Box::pin(async move {
                    let ask = Ask { action: text(&v, "action")?, way: v.get("way").and_then(Value::as_str).map(str::to_owned) };
                    repair(&cx.hub, &runner_of(&v)?, &ask).await
                })
            },
        },
        Op {
            name: "updates.list",
            summary: "Every runner's kiln against the newest: its build, whether it's up to date, what it does next, the ways it can be upgraded now (or why not), its policy; and the latest upgrades with their state, error and log.",
            read_only: true,
            destructive: false,
            input: || schema(json!({}), &[]),
            route: |_| Ok((Method::GET, path(&["api", "updates"]))),
            run: |cx, _| Box::pin(async move { Ok(json!({"runners": crate::updates::runners(&cx.hub).await, "jobs": cx.hub.updates.jobs().into_iter().take(20).collect::<Vec<_>>()})) }),
        },
        Op {
            name: "updates.retry",
            summary: "Upgrade a runner to the newest kiln now (a failed upgrade tried again), optionally `way` download or build. Nothing it runs stops: its agents and jobs carry on in the new build.",
            read_only: false,
            destructive: false,
            input: || schema(json!({"runner": {"type": "string", "description": RUNNER}, "way": {"type": "string", "enum": ["download", "build"]}}), &["runner"]),
            route: |v| Ok((Method::POST, path(&["api", "updates", "runners", &runner_of(v)?]))),
            run: |cx, v| {
                Box::pin(async move {
                    let ask = Ask { action: "retry-upgrade".to_owned(), way: v.get("way").and_then(Value::as_str).map(str::to_owned) };
                    repair(&cx.hub, &runner_of(&v)?, &ask).await
                })
            },
        },
        Op {
            name: "updates.policy",
            summary: "What a runner does with a newer kiln: `auto` (upgraded as soon as there's a build for it), `notify` (told, upgraded when someone chooses), or `off` (left alone).",
            read_only: false,
            destructive: false,
            input: || schema(json!({"runner": {"type": "string", "description": RUNNER}, "policy": {"type": "string", "enum": ["auto", "notify", "off"]}}), &["runner", "policy"]),
            route: |v| Ok((Method::PUT, path(&["api", "updates", "runners", &runner_of(v)?]))),
            run: |cx, v| Box::pin(set_policy(cx, v)),
        },
    ]
}

async fn set_policy(cx: Cx, v: Value) -> Out {
    let name = runner_of(&v)?;
    let p: crate::updates::Policy = serde_json::from_value(json!(text(&v, "policy")?)).map_err(|_| (StatusCode::BAD_REQUEST, "`policy` is auto, notify or off".to_owned()))?;
    crate::updates::set_policy(&cx.hub, &name, p).await.map_err(|e| (StatusCode::CONFLICT, format!("{e:#}")))?;
    Ok(json!({"runner": name, "policy": p}))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(state: crate::updates::JobState, method: &'static str, error: Option<&str>) -> LastUpgrade {
        LastUpgrade {
            state,
            method,
            started_at: crate::util::now_unix() - 600,
            ended_at: Some(crate::util::now_unix() - 300),
            log: vec!["\u{1b}[31merror[E0658]\u{1b}[0m: use of unstable library feature".into(), "error: could not compile `kiln`".into()],
            error: error.map(str::to_owned),
        }
    }

    #[test]
    fn a_failed_self_build_says_why_shows_its_log_and_offers_the_other_way() {
        let live = Live { connected: true, last_seen: crate::util::now_unix(), sample: None, tools: [("rustc".to_owned(), "1.94.0".to_owned())].into() };
        let mut j = job(crate::updates::JobState::Failed, "build", Some("the build failed"));
        j.log.push("error: package `hyper-rt v0.3.0` cannot be built because it requires rustc 1.98.1 or newer, while the currently active rustc version is 1.94.0".into());
        let f = server_side(None, Some(&live), Some(&j));
        let up = f.iter().find(|f| f.id == "upgrade.last").map(|f| (f.level, f.detail.contains("could not compile"), f.detail.contains('\u{1b}')));
        assert_eq!(up, Some((Level::Fail, true, false)), "{f:#?}");
        let rust = f.iter().find(|f| f.id == "toolchain.rust");
        assert!(rust.is_some_and(|r| r.level == Level::Fail && r.title.contains("1.94.0") && r.title.contains("1.98.1")), "{f:#?}");
        // A build that failed for something else says nothing of Rust.
        let other = job(crate::updates::JobState::Failed, "build", Some("the build failed"));
        assert!(server_side(None, Some(&live), Some(&other)).iter().all(|f| f.id != "toolchain.rust"));
    }

    #[test]
    fn an_offline_runner_is_said_to_be_and_a_worked_upgrade_is_fine() {
        let live = Live { connected: false, last_seen: crate::util::now_unix() - 7200, ..Live::default() };
        let f = server_side(None, Some(&live), Some(&job(crate::updates::JobState::Done, "native", None)));
        assert!(f.iter().any(|f| f.id == "connection" && f.level == Level::Fail && f.title.contains("2 h")), "{f:#?}");
        assert!(f.iter().any(|f| f.id == "upgrade.last" && f.level == Level::Ok), "{f:#?}");
    }

    #[test]
    fn room_is_judged_by_bytes_and_share_and_a_big_cache_is_the_repair() {
        let d = |free: u64, total: u64| Some(kiln_ci::facts::Disk { total, free });
        let p = Path::new("/w");
        assert_eq!(disk_finding("d", "here", p, d(500 << 20, 100 * GIB), None).level, Level::Fail);
        assert_eq!(disk_finding("d", "here", p, d(4 * GIB, 100 * GIB), None).level, Level::Warn);
        assert_eq!(disk_finding("d", "here", p, d(50 * GIB, 100 * GIB), None).level, Level::Ok);
        let short = disk_finding("d", "here", p, d(GIB / 2, 100 * GIB), Some((30 * GIB, false)));
        assert_eq!(short.fix.map(|f| f.action), Some("clear-self-build-cache".to_owned()));
        assert_eq!(disk_finding("d", "here", p, None, None).level, Level::Info);
    }

    #[test]
    fn the_rust_a_build_needs_is_read_from_cargos_words() {
        let old = vec!["error: package `a v1.0.0` cannot be built because it requires rustc 1.98.1 or newer, while the currently active rustc version is 1.94.0".to_owned()];
        assert_eq!(rust_needed(&old), Some(("1.98.1".to_owned(), Some("1.94.0".to_owned()))));
        let new = vec!["error: rustc 1.94.0 is not supported by the following packages:".to_owned(), "  a@1.0.0 requires rustc 1.97".to_owned(), "  b@2.0.0 requires rustc 1.98.1".to_owned()];
        assert_eq!(rust_needed(&new), Some(("1.98.1".to_owned(), Some("1.94.0".to_owned()))));
        assert_eq!(rust_needed(&["error: could not compile `kiln`".to_owned()]), None);
    }

    #[test]
    fn a_log_tail_is_its_last_lines_without_colour() {
        let text = (1..=40).map(|n| format!("\u{1b}[1mline {n}\u{1b}[0m")).collect::<Vec<_>>().join("\n");
        let t = last_lines(&text, 3);
        assert_eq!(t, "line 38\nline 39\nline 40");
    }

    #[test]
    fn sizes_and_times_read_as_words() {
        assert_eq!(said_bytes(512), "512 B");
        assert_eq!(said_bytes(3 * GIB + GIB / 2), "3.5 GiB");
        assert_eq!(said_bytes(250 * GIB), "250 GiB");
        assert_eq!(said_secs(45), "45 s");
        assert_eq!(said_secs(7200), "2 h");
    }

    #[test]
    fn a_runner_refuses_repairs_it_doesnt_know_and_prunes_what_it_keeps() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let here = Here { state_dir: dir.path().join("state"), work_dir: dir.path().join("work") };
        assert!(repair_here(&here, "rm -rf /").is_err());
        // Pruning a folder that isn't there yet is fine, and says what it freed.
        let said = repair_here(&here, "prune-versions").map_err(anyhow::Error::msg)?;
        assert!(said.contains("0 B freed"), "{said}");
        let v: Value = serde_json::from_str(&answer_repair(&here, "nonsense"))?;
        assert!(v.get("error").is_some());
        Ok(())
    }

    #[test]
    fn a_machine_examines_its_disks_and_says_its_clock() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let here = Here { state_dir: dir.path().join("state"), work_dir: dir.path().to_path_buf() };
        std::fs::create_dir_all(&here.state_dir)?;
        let m = examine(&here);
        assert!(m.now_ms > 0);
        assert!(m.findings.iter().any(|f| f.id == "disk.work"), "{m:#?}");
        Ok(())
    }

    #[test]
    fn every_operation_names_a_route_and_reads_exactly_when_it_gets() {
        for op in list_ops() {
            let v = json!({"runner": "mac", "action": "prune-versions", "policy": "auto"});
            let (method, segs) = (op.route)(&v).unwrap_or((Method::OPTIONS, Vec::new()));
            assert!(!segs.is_empty(), "{} has no route", op.name);
            assert_eq!(op.read_only, method == Method::GET, "{}", op.name);
        }
    }
}
