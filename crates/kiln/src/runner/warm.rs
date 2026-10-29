//! Warm workspaces on this runner (docs/ci.md W24-R11, R15). A leg whose job keeps folders warm
//! (`work: { warm: [target] }`) works in one of its lane's workspaces ("slots") instead of a fresh
//! one: each at a stable path, `<work>/warm/<repository>/<lane>/<n>` (the leg's runner workspace:
//! the repository's directory in it, `_temp` beside), leased to one leg at a time. What a lease
//! takes, clones, makes or drops is `kiln_ci::work::slots`'s to decide; this carries it out.
//!
//! - **Leases** are decided under the lane's lock (`<lane>/.lock`), so the machine's other runner
//!   processes, which share the work directory, decide one at a time too. A slot is then held by
//!   its own lock (`<n>.lease`) until its leg lets its workspace go; its record (`<n>.json`) says
//!   what it holds.
//! - **In place.** A slot's checkout ([`check_out`]) fetches the leg's commit (from this runner's
//!   mirror of the repository when it has it), refreshes git's index, then `git checkout --force`s
//!   it, which rewrites only the files that differ: the rest keep their mtimes, as cargo's
//!   freshness needs. Everything but the kept folders is cleaned (`git clean -ffdx -e …`).
//! - **Clones** are copy-on-write, never copies: `cp -a --reflink=always` on Linux (btrfs, XFS),
//!   `cp -c -a` on APFS. Taking a lane's last free slot first clones a spare from it, for a second
//!   leg of the lane. A volume that can't clone keeps workspaces in place: a second leg of a lane
//!   gets a new slot, or runs cold.
//! - **Untrusted legs** work on a throwaway clone (`<lane>/t-<id>`), removed as they end.
//! - **Budgets** (R15): every lane together keeps at most `KILN_WARM_MAX` (a quarter of the
//!   volume by default, at most 512 GiB), a lane at most its job's `warm.budget`, and below a floor
//!   of free space, `KILN_WARM_FLOOR` (a tenth of the volume, at most 100 GiB), nothing new is
//!   kept. A slot is measured (the blocks its files hold) as it's let go, and what's over then
//!   goes, least recently used first. A clone counts whole once measured: the volume's shared
//!   extents aren't read yet.
//! - **A holder that died** (its runner stopped mid-leg) left its slot leased with no lock held:
//!   the next lease of the lane resets it to its repository, or drops it.
//! - **For placement** (W24-R18) each lane keeps how long its trusted legs that succeeded took
//!   here, warm or cold (`<lane>/runs.json`, [`ran`]), and the runner tells the server what it
//!   holds warm ([`state`]) whenever that changed ([`changes`]).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use kiln_ci::wire::AssignmentV2;
use kiln_ci::work::reap;
use kiln_ci::work::slots::{self, Budget, Clones, Disk, Full, LaneOf, Recovery, Slot, State, Take, Why};
use serde::{Deserialize, Serialize};

/// The most slots a lane keeps.
const SLOTS: u32 = 4;

/// How many times what this process's legs left of the warm workspaces changed: a lease, a
/// release, a measurement or an eviction, a run's history. The runner tells the server again once
/// it moved ([`state`], W24-R18).
static CHANGES: AtomicU64 = AtomicU64::new(0);

/// [`CHANGES`] now.
pub fn changes() -> u64 {
    CHANGES.load(Ordering::Relaxed)
}

fn changed() {
    CHANGES.fetch_add(1, Ordering::Relaxed);
}

/// Where warm workspaces are: `<work>/warm`.
pub fn root(work: &Path) -> PathBuf {
    work.join("warm")
}

fn short(hex: &str) -> String {
    hex.chars().take(16).collect()
}

/// A lane's directory: `<work>/warm/<repository>/<lane>`, each hashed.
pub fn lane_dir(work: &Path, repo: &str, lane: &str) -> PathBuf {
    root(work).join(short(&crate::util::sha256_hex(repo.as_bytes()))).join(short(lane))
}

fn slot_dir(lane: &Path, n: u32) -> PathBuf {
    lane.join(n.to_string())
}

fn record_path(lane: &Path, n: u32) -> PathBuf {
    lane.join(format!("{n}.json"))
}

/// The lock a slot (or a throwaway clone) is held by while a leg works in it: `<dir>.lease`.
fn lease_path(dir: &Path) -> PathBuf {
    dir.with_extension("lease")
}

/// What a lane is, beside its slots (`lane.json`): for eviction, the job's budget.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct LaneFile {
    key: String,
    lane: LaneOf,
    budget: Option<u64>,
    kept: Vec<String>,
}

fn read_lane(lane: &Path) -> Option<LaneFile> {
    serde_json::from_slice(&std::fs::read(lane.join("lane.json")).ok()?).ok()
}

/// How long a lane's legs took here (`runs.json`, W24-R18); nothing yet, or unreadable, none.
fn read_runs(lane: &Path) -> slots::Runs {
    std::fs::read(lane.join("runs.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

/// A trusted leg of the lane at `lane` succeeded here in `ms`, having started from one of its
/// workspaces (`warm`) or from none (blocking): its history keeps it, for placement's expected
/// finish (W24-R18). Nothing for a lane a lease never made.
pub fn ran(lane: &Path, warm: bool, ms: u64) {
    if read_lane(lane).is_none() {
        return;
    }
    let Ok(_locked) = lock_file(&lane.join(".lock")) else { return };
    let mut runs = read_runs(lane);
    runs.add(warm, ms);
    let at = lane.join("runs.json");
    let tmp = at.with_extension(format!("json.{}", crate::util::new_id()));
    let wrote = serde_json::to_vec(&runs).map_err(std::io::Error::other).and_then(|b| std::fs::write(&tmp, b)).and_then(|()| std::fs::rename(&tmp, &at));
    match wrote {
        Ok(()) => changed(),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            tracing::debug!(lane = %lane.display(), error = %e, "a warm lane's history couldn't be written");
        }
    }
}

/// What this runner holds warm (blocking), as it tells the server (`AgentMsg::WorkState`,
/// W24-R18): each lane's record, its slots and its history. A lane without a record (one a lease
/// never finished making) is left out.
pub fn state(work: &Path) -> slots::WorkState {
    let lanes = lanes(work).into_iter().filter_map(|dir| read_lane(&dir).map(|f| slots::LaneState { key: f.key, lane: f.lane, slots: read_slots(&dir), runs: read_runs(&dir) })).collect();
    slots::WorkState { lanes }
}

/// A lane's slots, each numbered by its directory. A record whose directory is gone isn't one; a
/// directory without a readable record (a lease that stopped short) is dirty, with no checkout.
fn read_slots(lane: &Path) -> Vec<Slot> {
    let mut records: BTreeMap<u32, Slot> = BTreeMap::new();
    let mut dirs: HashSet<u32> = HashSet::new();
    for e in std::fs::read_dir(lane).into_iter().flatten().flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if let Some(n) = name.strip_suffix(".json").and_then(|s| s.parse::<u32>().ok()) {
            if let Some(s) = std::fs::read(e.path()).ok().and_then(|b| serde_json::from_slice::<Slot>(&b).ok()) {
                records.insert(n, Slot { n, ..s });
            }
        } else if let Ok(n) = name.parse::<u32>()
            && e.path().is_dir()
        {
            dirs.insert(n);
        }
    }
    records.retain(|n, _| dirs.contains(n));
    for n in dirs {
        records.entry(n).or_insert(Slot { n, state: State::Dirty, ..Slot::default() });
    }
    records.into_values().collect()
}

/// A slot's record, written whole or not at all.
fn write_slot(lane: &Path, s: &Slot) -> std::io::Result<()> {
    let at = record_path(lane, s.n);
    let tmp = at.with_extension(format!("json.{}", crate::util::new_id()));
    std::fs::write(&tmp, serde_json::to_vec(s).map_err(std::io::Error::other)?)?;
    std::fs::rename(tmp, at)
}

/// `path`'s lock, waiting for it.
fn lock_file(path: &Path) -> std::io::Result<File> {
    let f = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(path)?;
    f.lock()?;
    Ok(f)
}

/// `path`'s lock, if no one holds it.
fn try_lock_file(path: &Path) -> Option<File> {
    let f = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(path).ok()?;
    f.try_lock().ok()?;
    Some(f)
}

/// Whether a leg, of this process or another, holds `dir`'s lease. A probe that takes it lets go
/// at once, as a lease does (`Lease`'s drop).
fn held(dir: &Path) -> bool {
    let Ok(f) = File::open(lease_path(dir)) else { return false };
    match f.try_lock() {
        Ok(()) => {
            let _ = f.unlock();
            false
        }
        Err(e) => matches!(e, std::fs::TryLockError::WouldBlock),
    }
}

fn remove_any(p: &Path) -> std::io::Result<()> {
    if std::fs::symlink_metadata(p)?.is_dir() { std::fs::remove_dir_all(p) } else { std::fs::remove_file(p) }
}

/// `dir` moved aside at once, into `<work>/warm/.trash`, to be removed later ([`empty_trash`]):
/// no lease finds it half gone. Its lease's lock file goes with it.
fn trash(work: &Path, dir: &Path) {
    let bin = root(work).join(".trash");
    let aside = bin.join(crate::util::new_id());
    if std::fs::create_dir_all(&bin).and_then(|()| std::fs::rename(dir, &aside)).is_err() {
        super::workspaces::remove(dir);
    }
    let _ = std::fs::remove_file(lease_path(dir));
}

/// Drops slot `n` of `lane`: its directory, its record and what a leg left beside it.
fn drop_slot(work: &Path, lane: &Path, n: u32) {
    let dir = slot_dir(lane, n);
    if dir.exists() {
        trash(work, &dir);
    }
    for ext in ["json", "lock", "lease", "docker.sock", "buildkitd.toml"] {
        let _ = std::fs::remove_file(dir.with_extension(ext));
    }
}

/// Removes what dropped slots and throwaway clones left in the trash: as root, by a container,
/// what a Docker action wrote as root.
pub fn empty_trash(work: &Path) {
    for e in std::fs::read_dir(root(work).join(".trash")).into_iter().flatten().flatten() {
        super::workspaces::remove(&e.path());
    }
}

/// Whether the volume under `dir` clones copy-on-write, tried once per directory: on Linux by
/// cloning a file of its own (`cp --reflink=always` fails rather than copy), on macOS by the
/// volume's kind (APFS: `cp -c` would copy where it can't clone).
pub fn clones(dir: &Path) -> Clones {
    static SEEN: LazyLock<parking_lot::Mutex<HashMap<PathBuf, Clones>>> = LazyLock::new(Default::default);
    if let Some(c) = SEEN.lock().get(dir) {
        return *c;
    }
    let c = probe(dir);
    SEEN.lock().insert(dir.to_path_buf(), c);
    c
}

#[cfg(target_os = "linux")]
fn probe(dir: &Path) -> Clones {
    let id = crate::util::new_id();
    let (a, b) = (dir.join(format!(".clone-probe-{id}")), dir.join(format!(".clone-probe-{id}-b")));
    let cloned = std::fs::write(&a, b"kiln").is_ok() && Command::new("cp").arg("--reflink=always").arg(&a).arg(&b).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success());
    let _ = std::fs::remove_file(&a);
    let _ = std::fs::remove_file(&b);
    if cloned { Clones::Cow } else { Clones::No }
}

#[cfg(target_os = "macos")]
fn probe(dir: &Path) -> Clones {
    match nix::sys::statfs::statfs(dir) {
        Ok(s) if s.filesystem_type_name() == "apfs" => Clones::Cow,
        _ => Clones::No,
    }
}

/// NTFS has no clones (ReFS's block cloning isn't used yet).
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn probe(_dir: &Path) -> Clones {
    Clones::No
}

/// `from` cloned copy-on-write as `to` (which mustn't exist yet), with its modes, links and mtimes:
/// `cp -a --reflink=always`, or `cp -c -a` on macOS. Never a copy: where the volume can't clone it
/// fails, and what it began is removed. How long it took.
pub fn clone_tree(from: &Path, to: &Path) -> anyhow::Result<Duration> {
    let started = Instant::now();
    let mut cp = Command::new("cp");
    if cfg!(target_os = "macos") {
        cp.args(["-c", "-a"]);
    } else {
        cp.args(["-a", "--reflink=always"]);
    }
    let out = cp.arg(from).arg(to).stdin(Stdio::null()).output()?;
    if !out.status.success() {
        if to.exists() {
            super::workspaces::remove(to);
        }
        anyhow::bail!("cp: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(started.elapsed())
}

/// The bytes `dir` holds on its volume: the blocks of its files and folders, links not followed
/// and a file of several names counted once. Shared extents aren't read: a clone counts whole.
pub fn usage(dir: &Path) -> u64 {
    let mut total = 0u64;
    let mut seen: HashSet<(u64, u64)> = HashSet::new();
    let mut todo = vec![dir.to_path_buf()];
    while let Some(d) = todo.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let Ok(m) = e.metadata() else { continue };
            if m.is_dir() {
                todo.push(e.path());
            }
            if first_name(&m, &mut seen) {
                total = total.saturating_add(on_disk(&m));
            }
        }
    }
    total
}

#[cfg(unix)]
fn on_disk(m: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt as _;
    m.blocks().saturating_mul(512)
}

#[cfg(not(unix))]
fn on_disk(m: &std::fs::Metadata) -> u64 {
    m.len()
}

/// Whether this is the first of a file's names seen (a hard link's others aren't counted again).
#[cfg(unix)]
fn first_name(m: &std::fs::Metadata, seen: &mut HashSet<(u64, u64)>) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    m.is_dir() || m.nlink() < 2 || seen.insert((m.dev(), m.ino()))
}

#[cfg(not(unix))]
fn first_name(_m: &std::fs::Metadata, _seen: &mut HashSet<(u64, u64)>) -> bool {
    true
}

/// This runner's budget for warm workspaces (W24-R15): `KILN_WARM_MAX` (`300G`) for every lane
/// together, a quarter of the volume by default and at most 512 GiB; `KILN_WARM_FLOOR`, the free
/// space below which nothing new is kept, a tenth of the volume and at most 100 GiB.
pub fn budget(work: &Path) -> Budget {
    let env = |k: &str| std::env::var(k).ok().and_then(|v| super::cache::parse_size(&v));
    let (runner, floor) = match crate::os::disk_space(work) {
        Some((size, _)) if size > 0 => ((size / 4).min(512 << 30), (size / 10).min(100 << 30)),
        _ => (64 << 30, 10 << 30),
    };
    Budget { runner: env("KILN_WARM_MAX").unwrap_or(runner), floor: env("KILN_WARM_FLOOR").unwrap_or(floor), slots: SLOTS }
}

/// Every lane's directory under `<work>/warm`.
fn lanes(work: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for repo in std::fs::read_dir(root(work)).into_iter().flatten().flatten() {
        if repo.file_name().to_string_lossy().starts_with('.') || !repo.path().is_dir() {
            continue;
        }
        for lane in std::fs::read_dir(repo.path()).into_iter().flatten().flatten() {
            if lane.path().is_dir() {
                out.push(lane.path());
            }
        }
    }
    out.sort();
    out
}

/// `git -C dir args…`: what it printed, or why it failed.
fn git(dir: &Path, args: &[&str]) -> anyhow::Result<String> {
    let mut cmd = Command::new("git");
    crate::os::git_no_prompts(cmd.arg("-C").arg(dir).args(args).stdin(Stdio::null()));
    let out = cmd.output()?;
    anyhow::ensure!(out.status.success(), "git {}: {}", args.first().copied().unwrap_or_default(), String::from_utf8_lossy(&out.stderr).trim());
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Whether the repository at `repo` (its own `.git`, never one it's inside) has commit `sha`.
pub fn has_commit(repo: &Path, sha: &str) -> bool {
    !sha.is_empty() && repo.join(".git").exists() && git(repo, &["cat-file", "-e", format!("{sha}^{{commit}}").as_str()]).is_ok()
}

/// The commit the repository at `repo` has checked out.
fn head(repo: &Path) -> Option<String> {
    if !repo.join(".git").exists() {
        return None;
    }
    git(repo, &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"]).ok().map(|s| s.trim().to_owned()).filter(|s| !s.is_empty())
}

/// This runner's mirror of `repo` (v1's `<work>/git/<repository>.git`, or under `KILN_GIT_CACHE`),
/// when it has one.
pub fn mirror_of(work: &Path, repo: &str) -> Option<PathBuf> {
    let cache = std::env::var_os("KILN_GIT_CACHE").map_or_else(|| work.join("git"), PathBuf::from);
    let slug: String = repo.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
    let m = cache.join(format!("{slug}.git"));
    m.join("HEAD").is_file().then_some(m)
}

/// This runner's mirror of `repo`, when it has commit `sha`: where a slot fetches it from without
/// asking the server.
pub fn mirror_with(work: &Path, repo: &str, sha: &str) -> Option<PathBuf> {
    let m = mirror_of(work, repo)?;
    (!sha.is_empty() && git(&m, &["cat-file", "-e", format!("{sha}^{{commit}}").as_str()]).is_ok()).then_some(m)
}

/// How many commits apart each free slot's base is from `sha`: none for its own commit, else as
/// this runner's mirror counts them (both sides of their merge base), where it has both.
fn distances(work: &Path, repo: &str, sha: &str, found: &[Slot]) -> BTreeMap<u32, u64> {
    let mirror = mirror_of(work, repo);
    found
        .iter()
        .filter(|s| s.state == State::Free)
        .filter_map(|s| {
            let base = s.base.as_deref()?;
            if base == sha {
                return Some((s.n, 0));
            }
            let out = git(mirror.as_deref()?, &["rev-list", "--count", "--left-right", format!("{base}...{sha}").as_str()]).ok()?;
            let mut sides = out.split_whitespace().map(|p| p.parse::<u64>().ok());
            let (a, b) = (sides.next()??, sides.next()??);
            Some((s.n, a.saturating_add(b)))
        })
        .collect()
}

/// A job's kept folders (`warm: [target]`) as paths in its workspace, and those that aren't (an
/// absolute path, `..`, a glob, `.git`), which aren't kept.
pub fn kept_paths(given: &[String]) -> (Vec<String>, Vec<String>) {
    let (mut ok, mut refused) = (Vec::<String>::new(), Vec::new());
    for g in given {
        let p = g.trim().trim_end_matches('/');
        let inside = !p.is_empty()
            && !p.starts_with('/')
            && !p.contains(['\\', '*', '?', '[', ':'])
            && !p.split('/').any(|c| c.is_empty() || c == "." || c == ".." || c == ".git");
        if !inside {
            refused.push(g.clone());
        } else if !ok.iter().any(|k| k == p) {
            ok.push(p.to_owned());
        }
    }
    (ok, refused)
}

/// A leg's lane (W24-R11), from its assignment: its repository; its workflow's file and its job's
/// id; its matrix values; its executor and the image it runs in. A profile, and the toolchain's
/// own identity, aren't known here yet.
pub fn lane_of(a: &AssignmentV2) -> LaneOf {
    let g = &a.contexts.github;
    let s = |k: &str| g.get(k).and_then(serde_json::Value::as_str).unwrap_or_default();
    // `owner/repo/<path>@<ref>`: the file, not the ref, so a branch's legs share trunk's lane.
    let workflow = s("workflow_ref").strip_prefix(s("repository")).map(|x| x.trim_start_matches('/')).and_then(|x| x.rsplit_once('@')).map_or("", |(p, _)| p);
    // As the server keys the lane for placement too (W24-R18), but for the toolchain.
    let toolchain = slots::toolchain(&a.executor.kind, a.executor.image.as_deref(), a.container.as_deref());
    slots::lane_for(&a.repo, workflow, &a.leg.job_id, &a.contexts.matrix, toolchain)
}

/// What a leg asks of its lane.
#[derive(Debug, Clone)]
pub struct Ask {
    pub repo: String,
    pub lane: LaneOf,
    /// The leg, named in its slot's record while it holds it.
    pub leg: String,
    /// The commit it checks out.
    pub sha: String,
    pub trusted: bool,
    /// Its kept folders, as [`kept_paths`] has them.
    pub kept: Vec<String>,
    /// Its lane's budget: the job's `warm.budget`.
    pub budget: Option<u64>,
    /// Its workspace's name in its runner workspace (its repository's).
    pub name: String,
}

/// A leg's hold on a warm workspace, until it's [`release`]d. Dropped without one (its runner
/// stopped), its slot is found leased with no one holding it: the next lease resets or drops it.
#[derive(Debug)]
pub struct Lease {
    /// The leg's runner workspace: a slot of its lane, or an untrusted leg's throwaway clone of one.
    pub dir: PathBuf,
    /// What the leg's log says of it.
    pub said: String,
    /// It holds a checkout the leg starts from: warm, not a new, empty slot (W24-R18's history).
    pub warm: bool,
    /// Its lane's directory.
    pub lane: PathBuf,
    /// The slot's number; `None` for a throwaway clone.
    n: Option<u32>,
    name: String,
    quarantine: bool,
    _lock: File,
}

impl Drop for Lease {
    /// Its lock let go of outright (W24-R18): closing it alone doesn't while a child of this
    /// process holds a copy (a fork's descriptors live until its exec), and the lane's next leg
    /// would find the slot leased and start cold.
    fn drop(&mut self) {
        let _ = self._lock.unlock();
    }
}

/// What a lease gave.
#[derive(Debug)]
pub enum Leased {
    Warm(Lease),
    /// No warm workspace (why, for the leg's log): the leg runs cold in a workspace of its own.
    Cold(String),
}

fn gib(bytes: u64) -> String {
    format!("{:.1} GiB", bytes as f64 / f64::from(1u32 << 30))
}

fn ago(secs: i64) -> String {
    match secs.max(0) {
        s if s < 120 => format!("{s}s ago"),
        s if s < 7_200 => format!("{}m ago", s / 60),
        s if s < 172_800 => format!("{}h ago", s / 3_600),
        s => format!("{}d ago", s / 86_400),
    }
}

/// A slot's state, for the leg's log: its commit, how far from the leg's, and when it was let go.
fn at(s: &Slot, distance: Option<u64>, now: i64) -> String {
    match &s.base {
        Some(b) => {
            let far = match distance {
                Some(0) => "the leg's own commit".to_owned(),
                Some(d) => format!("{d} commits from the leg's"),
                None => "how far from the leg's isn't known here".to_owned(),
            };
            format!("at {} ({far}), let go {}", b.get(..12).unwrap_or(b), ago(now - s.used))
        }
        None => "with no checkout".to_owned(),
    }
}

/// Why a leg runs cold, for its log.
fn cold(why: Full, lane: &[Slot], budget: &Budget, lane_budget: Option<u64>, disk: &Disk) -> String {
    let leased = if lane.is_empty() { "the lane has no workspace yet".to_owned() } else { format!("each of the lane's {} workspaces is leased", lane.len()) };
    let room = match why {
        Full::Floor => format!("the volume has {} free, below the floor ({}) under which nothing new is kept", gib(disk.free), gib(budget.floor)),
        Full::Runner => format!("this runner's warm workspaces hold {} of their budget of {} (`KILN_WARM_MAX`)", gib(disk.kept), gib(budget.runner)),
        Full::Lane => format!("they hold {} of the job's `warm.budget` of {}", gib(slots::lane_bytes(lane)), gib(lane_budget.unwrap_or_default())),
        Full::Slots => format!("a lane keeps at most {} workspaces", budget.slots),
    };
    format!("No warm workspace: {leased}, and {room}. The leg runs cold.")
}

/// Leases a warm workspace for a leg (W24-R11); blocking, under the lane's lock.
pub fn lease(work: &Path, ask: &Ask) -> anyhow::Result<Leased> {
    let root = root(work);
    std::fs::create_dir_all(&root)?;
    lease_with(work, ask, clones(&root), &budget(work))
}

/// [`lease`], with whether the volume clones and the runner's budget given.
pub fn lease_with(work: &Path, ask: &Ask, clones: Clones, budget: &Budget) -> anyhow::Result<Leased> {
    use anyhow::Context as _;
    let key = slots::lane(&ask.lane).hex();
    let lane = lane_dir(work, &ask.repo, &key);
    std::fs::create_dir_all(&lane).with_context(|| format!("create {}", lane.display()))?;
    let _locked = lock_file(&lane.join(".lock")).context("the lane's lock")?;
    let about = LaneFile { key: key.clone(), lane: ask.lane.clone(), budget: ask.budget, kept: ask.kept.clone() };
    let _ = std::fs::write(lane.join("lane.json"), serde_json::to_vec_pretty(&about)?);
    let now = crate::util::now_unix();
    let mut found = recover_lane(work, &lane, &ask.name, now);
    let distance = distances(work, &ask.repo, &ask.sha, &found);
    let others = lanes(work).into_iter().filter(|l| *l != lane).fold(0u64, |a, l| a.saturating_add(slots::lane_bytes(&read_slots(&l))));
    let disk = Disk { free: crate::os::disk_available(&lane).unwrap_or(u64::MAX), kept: others.saturating_add(slots::lane_bytes(&found)) };
    if !ask.trusted {
        return Ok(throwaway(work, &lane, ask, &found, &distance, clones, budget, &disk));
    }
    let kept = if ask.kept.is_empty() { "nothing".to_owned() } else { ask.kept.join(", ") };
    // A free slot whose lease another process holds after all (it can't, under the lane's lock,
    // but a record is only a file) is taken for leased, and another chosen.
    for _ in 0..=SLOTS {
        let c = slots::choose(&found, &distance, clones, ask.budget, budget, &disk);
        let n = match c.take {
            Take::Slot(n) | Take::New(n) => n,
            Take::Cold(why) => return Ok(Leased::Cold(cold(why, &found, budget, ask.budget, &disk))),
        };
        let dir = slot_dir(&lane, n);
        let Some(lock) = try_lock_file(&lease_path(&dir)) else {
            match found.iter_mut().find(|s| s.n == n) {
                Some(s) => s.state = State::Leased,
                None => found.push(Slot { n, state: State::Leased, ..Slot::default() }),
            }
            continue;
        };
        let mut said = String::new();
        let mut s = found.iter().find(|s| s.n == n).cloned().unwrap_or(Slot { n, used: now, ..Slot::default() });
        if let Take::Slot(_) = c.take {
            said.push_str(&format!("Warm workspace: slot {n} of the job's lane on this runner, {}; kept: {kept}.", at(&s, distance.get(&n).copied(), now)));
        } else {
            let why = if found.is_empty() { "the lane's first: cold this time, warm for the job's next legs here".to_owned() } else { format!("cold this time: each of the lane's {} workspaces is leased", found.len()) };
            said.push_str(&format!("Warm workspace: a new slot {n} of the job's lane on this runner ({why}); kept: {kept}."));
        }
        // The spare, cloned before the leg writes: a leg of the lane that comes while this one runs
        // gets a clone of a workspace no one is writing.
        if let Some(m) = c.spare {
            match clone_tree(&dir, &slot_dir(&lane, m)) {
                Ok(took) => {
                    let shared = s.bytes.saturating_add(s.shared);
                    let spare = Slot { n: m, state: State::Free, base: s.base.clone(), used: s.used, bytes: 0, shared, holder: None };
                    s.bytes = 0;
                    s.shared = shared;
                    if write_slot(&lane, &spare).is_ok() {
                        said.push_str(&format!(" A copy-on-write clone of it (slot {m}, made in {:.1}s) waits for a leg of the lane that comes while this one runs.", took.as_secs_f64()));
                    }
                }
                Err(e) => tracing::warn!(lane = %lane.display(), error = %format!("{e:#}"), "a warm workspace's spare couldn't be cloned"),
            }
        }
        if let Err(e) = prepare(&dir, &ask.name) {
            // Something the last leg left that this runner can't remove (a container's root files):
            // the slot is never leased again, and goes at the next sweep.
            let _ = write_slot(&lane, &Slot { state: State::Quarantined, holder: None, ..s });
            return Ok(Leased::Cold(format!("No warm workspace: slot {n} of the job's lane couldn't be cleared ({e}), so it goes. The leg runs cold.")));
        }
        let warm = matches!(c.take, Take::Slot(_)) && s.base.is_some();
        write_slot(&lane, &Slot { state: State::Leased, holder: Some(ask.leg.clone()), ..s }).context("the slot's record")?;
        changed();
        return Ok(Leased::Warm(Lease { dir, said, warm, lane, n: Some(n), name: ask.name.clone(), quarantine: false, _lock: lock }));
    }
    Ok(Leased::Cold("No warm workspace: the lane's workspaces are all held. The leg runs cold.".to_owned()))
}

/// An untrusted leg's lease: a throwaway copy-on-write clone of a free slot, or cold.
#[allow(clippy::too_many_arguments)]
fn throwaway(work: &Path, lane: &Path, ask: &Ask, found: &[Slot], distance: &BTreeMap<u32, u64>, clones: Clones, budget: &Budget, disk: &Disk) -> Leased {
    let Some(n) = slots::throwaway(found, distance, clones, budget, disk) else {
        let why = if clones == Clones::No {
            "this runner's volume can't clone one".to_owned()
        } else if disk.free < budget.floor {
            format!("the volume has {} free, below its floor ({})", gib(disk.free), gib(budget.floor))
        } else {
            "the lane has no free workspace to clone".to_owned()
        };
        return Leased::Cold(format!("No warm workspace: an untrusted leg works only on a throwaway clone of one, and {why}. The leg runs cold."));
    };
    let dir = lane.join(format!("t-{}", crate::util::new_id()));
    let Some(lock) = try_lock_file(&lease_path(&dir)) else { return Leased::Cold("No warm workspace: its throwaway clone couldn't be locked. The leg runs cold.".to_owned()) };
    let now = crate::util::now_unix();
    let s = found.iter().find(|s| s.n == n).cloned().unwrap_or_default();
    match clone_tree(&slot_dir(lane, n), &dir).and_then(|took| prepare(&dir, &ask.name).map(|()| took).map_err(anyhow::Error::from)) {
        Ok(took) => Leased::Warm(Lease {
            said: format!("Warm workspace: a throwaway copy-on-write clone of slot {n} of the job's lane, {}, made in {:.1}s. The leg is untrusted: nothing it writes is kept.", at(&s, distance.get(&n).copied(), now), took.as_secs_f64()),
            dir,
            warm: s.base.is_some(),
            lane: lane.to_path_buf(),
            n: None,
            name: ask.name.clone(),
            quarantine: false,
            _lock: lock,
        }),
        Err(e) => {
            if dir.exists() {
                trash(work, &dir);
            }
            let _ = std::fs::remove_file(lease_path(&dir));
            Leased::Cold(format!("No warm workspace: the lane's workspace couldn't be cloned ({e:#}). The leg runs cold."))
        }
    }
}

/// A slot made ready for its leg: everything in it but the workspace goes (the last leg's
/// `_temp`, what it made beside its repository, a keep's mark), and what a leg left beside it
/// (its Docker socket, BuildKit's config).
fn prepare(dir: &Path, name: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(dir.join(name))?;
    for e in std::fs::read_dir(dir)?.flatten() {
        if e.file_name().to_str() != Some(name) {
            remove_any(&e.path())?;
        }
    }
    for ext in ["docker.sock", "buildkitd.toml"] {
        let _ = std::fs::remove_file(dir.with_extension(ext));
    }
    Ok(())
}

/// A slot reset to its repository: everything but its workspace's `.git` goes, with the locks a
/// git stopped mid-way left in it.
fn reset(dir: &Path, name: &str) -> std::io::Result<()> {
    for e in std::fs::read_dir(dir)?.flatten() {
        if e.file_name().to_str() != Some(name) {
            remove_any(&e.path())?;
        }
    }
    let ws = dir.join(name);
    for e in std::fs::read_dir(&ws).into_iter().flatten().flatten() {
        if e.file_name().to_str() != Some(".git") {
            remove_any(&e.path())?;
        }
    }
    for e in std::fs::read_dir(ws.join(".git")).into_iter().flatten().flatten() {
        if e.path().extension().is_some_and(|x| x == "lock") {
            let _ = std::fs::remove_file(e.path());
        }
    }
    Ok(())
}

/// A lane's slots as a lease finds them, each recovered (W24-R11): one leased by a leg that's gone
/// (no one holds its lock, and it isn't kept to look into) is reset to its repository or dropped;
/// throwaway clones no leg holds, and records of slots that are gone, go.
fn recover_lane(work: &Path, lane: &Path, name: &str, now: i64) -> Vec<Slot> {
    for e in std::fs::read_dir(lane).into_iter().flatten().flatten() {
        let (path, file) = (e.path(), e.file_name().to_string_lossy().into_owned());
        if file.starts_with("t-") && path.is_dir() && !held(&path) {
            trash(work, &path);
        } else if let Some(n) = file.strip_suffix(".json").and_then(|s| s.parse::<u32>().ok())
            && !slot_dir(lane, n).is_dir()
        {
            let _ = std::fs::remove_file(&path);
        }
    }
    let mut out = Vec::new();
    for s in read_slots(lane) {
        let dir = slot_dir(lane, s.n);
        let alive = held(&dir) || super::workspaces::kept_until(&dir).is_some_and(|t| t > now);
        match slots::recover(&s, alive) {
            Recovery::Keep => out.push(s),
            Recovery::Reset => match reset(&dir, name) {
                Ok(()) => {
                    tracing::info!(slot = %dir.display(), "a warm workspace whose leg died was reset to its repository");
                    let s = Slot { state: State::Free, base: None, holder: None, ..s };
                    let _ = write_slot(lane, &s);
                    out.push(s);
                }
                Err(e) => {
                    tracing::warn!(slot = %dir.display(), error = %e, "a warm workspace whose leg died couldn't be reset: it goes");
                    drop_slot(work, lane, s.n);
                }
            },
            Recovery::Drop => drop_slot(work, lane, s.n),
        }
    }
    out
}

/// A leg is done with its warm workspace (blocking). A slot goes back to its lane as it is, free
/// for the lane's next leg (or quarantined, when it couldn't be checked out in place or cleared),
/// its base the commit its repository has checked out; a throwaway clone goes, nothing of it
/// kept. The slot to measure, when it was one.
pub fn release(work: &Path, l: Lease) -> Option<(PathBuf, u32)> {
    let Some(n) = l.n else {
        trash(work, &l.dir);
        return None;
    };
    let base = head(&l.dir.join(&l.name));
    let lane = l.lane.clone();
    let guard = lock_file(&lane.join(".lock"));
    let s = read_slots(&lane).into_iter().find(|s| s.n == n).unwrap_or(Slot { n, ..Slot::default() });
    let state = if l.quarantine { State::Quarantined } else { State::Free };
    if let Err(e) = write_slot(&lane, &Slot { state, base, used: crate::util::now_unix(), holder: None, ..s }) {
        tracing::warn!(lane = %lane.display(), error = %e, "a warm workspace's record couldn't be written");
    }
    changed();
    // Its lock goes before the lane's: a lease never finds it free and held.
    drop(l);
    drop(guard);
    Some((lane, n))
}

/// What's over budget, dropped (W24-R15): every lane read, `slots::evict` deciding, and each slot
/// dropped under its lane's lock if no lease came for it since. What went, said.
pub fn sweep(work: &Path, budget: &Budget) -> Vec<String> {
    let dirs = lanes(work);
    let found: Vec<slots::Lane> = dirs.iter().map(|l| slots::Lane { slots: read_slots(l), budget: read_lane(l).and_then(|f| f.budget) }).collect();
    let free = crate::os::disk_available(&root(work)).unwrap_or(u64::MAX);
    let mut went = Vec::new();
    for (i, n, why) in slots::evict(&found, budget, free) {
        let Some(lane) = dirs.get(i) else { continue };
        let Ok(_locked) = lock_file(&lane.join(".lock")) else { continue };
        let now = read_slots(lane).into_iter().find(|s| s.n == n);
        if now.is_none_or(|s| s.state == State::Leased) || held(&slot_dir(lane, n)) {
            continue;
        }
        drop_slot(work, lane, n);
        let why = match why {
            Why::Quarantined => "it was quarantined",
            Why::Lane => "its lane held more than the job's budget",
            Why::Runner => "this runner's warm workspaces held more than their budget",
            Why::Floor => "the volume was below its floor",
        };
        went.push(format!("slot {n} of {}: {why}", lane.display()));
    }
    went
}

/// The runner scope's defaults (W24-R31): `KILN_WARM_MAX` as its ceiling and `KILN_WARM_FLOOR` as
/// its floor, where they're set.
pub fn env_policy() -> reap::Policy {
    let env = |k: &str| std::env::var(k).ok().and_then(|v| super::cache::parse_size(&v)).map(reap::Size::Bytes);
    reap::Policy { ceiling: env("KILN_WARM_MAX"), floor: env("KILN_WARM_FLOOR"), ..reap::Policy::default() }
}

/// What the reaper last did here (W24-R31), newest last, for the server (`WorkState`'s `reaped`).
static REAPED: LazyLock<parking_lot::Mutex<std::collections::VecDeque<reap::Eviction>>> = LazyLock::new(Default::default);

/// The reaper's evictions on this runner since the server last took them.
pub fn take_reaped() -> Vec<reap::Eviction> {
    REAPED.lock().drain(..).collect()
}

/// W24-R31's reaper over this runner's warm workspaces. The policy is `layers` (the server's
/// system and runner scopes) over [`env_policy`]; the volume `disk` (`(size, free)`, measured with
/// statvfs when `None`). When free space is under the floor, the workspaces past their ceiling, or
/// the schedule due (`last`: when it last reaped on schedule), each slot no leg holds is an entry
/// of its lane, worth the time its lane's warm runs save over its cold ones, and
/// `kiln_ci::work::reap::plan` decides; each eviction is dropped under its lane's lock, logged,
/// counted and kept for the server. Why it ran, and what went.
pub fn reap(work: &Path, layers: &[(reap::Scope, reap::Policy)], last: Option<i64>, now: i64, disk: Option<(u64, u64)>) -> (Option<reap::Trigger>, Vec<reap::Eviction>) {
    let Some((size, free)) = disk.or_else(|| crate::os::disk_space(&root(work))) else { return (None, Vec::new()) };
    let env = env_policy();
    let mut all: Vec<(reap::Scope, &reap::Policy)> = layers.iter().map(|(s, p)| (*s, p)).collect();
    all.insert(0, (reap::Scope::Runner, &env));
    let policy = reap::resolve(size, &all);
    let mut entries = Vec::new();
    let mut held_bytes = 0u64;
    for lane in lanes(work) {
        let runs = read_runs(&lane);
        let median = |v: &[u64]| -> u64 {
            let mut v = v.to_vec();
            v.sort_unstable();
            v.get(v.len() / 2).copied().unwrap_or(0)
        };
        let saved_ms = median(&runs.cold_ms).saturating_sub(median(&runs.warm_ms));
        for slot in read_slots(&lane) {
            held_bytes = held_bytes.saturating_add(slot.bytes);
            if slot.state == State::Leased || held(&slot_dir(&lane, slot.n)) {
                continue;
            }
            entries.push(reap::Entry { id: format!("{}#{}", lane.display(), slot.n), group: lane.display().to_string(), bytes: slot.bytes, used: slot.used, saved_ms });
        }
    }
    let Some(trigger) = reap::due(&policy, free, held_bytes, 0, last, now) else { return (None, Vec::new()) };
    let mut went = Vec::new();
    for e in reap::plan(&policy, &entries, free, held_bytes, now) {
        let Some((lane, n)) = e.id.rsplit_once('#').and_then(|(l, n)| Some((PathBuf::from(l), n.parse::<u32>().ok()?))) else { continue };
        let Ok(_locked) = lock_file(&lane.join(".lock")) else { continue };
        if read_slots(&lane).into_iter().find(|s| s.n == n).is_none_or(|s| s.state == State::Leased) || held(&slot_dir(&lane, n)) {
            continue;
        }
        drop_slot(work, &lane, n);
        let reason = match e.reason {
            reap::Reason::MaxAge => "max_age",
            reap::Reason::Target => "target",
            reap::Reason::Floor => "floor",
        };
        tracing::info!(slot = %e.id, bytes = e.bytes, reason, rule = e.rule, scope = ?e.scope, trigger = ?trigger, "the reaper evicted a warm workspace");
        crate::metrics::global().work_reaped_bytes.add(&["warm", reason], e.bytes);
        went.push(e);
    }
    if !went.is_empty() {
        empty_trash(work);
        changed();
        let mut kept = REAPED.lock();
        kept.extend(went.iter().cloned());
        while kept.len() > 200 {
            kept.pop_front();
        }
    }
    (Some(trigger), went)
}

/// The server's reaper policies for this runner (its system and runner scopes), as it last said.
static SERVER_POLICIES: LazyLock<parking_lot::Mutex<Vec<(reap::Scope, reap::Policy)>>> = LazyLock::new(Default::default);

/// The server's reaper policies for this runner, as it last said (W24-R31).
pub fn set_server_policies(layers: Vec<(reap::Scope, reap::Policy)>) {
    *SERVER_POLICIES.lock() = layers;
}

/// W24-R31: the reaper's clock on this runner. Every minute it checks whether free space, the
/// ceiling or the schedule calls for a reap; it starts once per process.
pub fn start_reaper(work: &Path) {
    static STARTED: std::sync::Once = std::sync::Once::new();
    let work = work.to_path_buf();
    STARTED.call_once(move || {
        crate::rt::spawn(async move {
            let mut last: Option<i64> = None;
            loop {
                crate::rt::sleep(Duration::from_secs(60)).await;
                let (w, l, layers, now) = (work.clone(), last, SERVER_POLICIES.lock().clone(), crate::util::now_unix());
                if let Ok((Some(reap::Trigger::Schedule), _)) = crate::rt::blocking(move || reap(&w, &layers, l, now, None)).await {
                    last = Some(now);
                }
            }
        });
    });
}

/// The warm workspaces this process's legs hold, by directory, each with its runner's work
/// directory.
static HELD: LazyLock<parking_lot::Mutex<HashMap<PathBuf, (PathBuf, Lease)>>> = LazyLock::new(Default::default);

/// A leg holds `lease` until its workspace is let go ([`let_go`]).
pub fn hold(work: &Path, lease: Lease) {
    HELD.lock().insert(lease.dir.clone(), (work.to_path_buf(), lease));
}

/// A held workspace that couldn't be checked out in place or cleared: never leased again, it goes
/// at the next sweep.
pub fn quarantine(dir: &Path) {
    if let Some((_, l)) = HELD.lock().get_mut(dir) {
        l.quarantine = true;
    }
}

/// The workspace at `dir` let go (its leg ended, or a kept leg's keep ran out). When it's a warm
/// one held here it goes back to its lane (a throwaway clone is removed), then the slot is
/// measured and what's over budget goes, in the background. Whether it was one.
pub async fn let_go(dir: &Path) -> bool {
    let Some((work, lease)) = HELD.lock().remove(dir) else { return false };
    let at = work.clone();
    let measured = crate::rt::blocking(move || release(&at, lease)).await.ok().flatten();
    crate::rt::spawn(async move {
        let went = crate::rt::blocking(move || settle(&work, measured, &budget(&work))).await.unwrap_or_default();
        for what in went {
            tracing::info!(slot = %what, "a warm workspace went");
        }
    });
    true
}

/// After a release (blocking): the slot measured, what's over budget dropped, and what drops left
/// in the trash removed. What went, said.
pub fn settle(work: &Path, measured: Option<(PathBuf, u32)>, budget: &Budget) -> Vec<String> {
    if let Some((lane, n)) = measured {
        let bytes = usage(&slot_dir(&lane, n));
        if let Ok(_locked) = lock_file(&lane.join(".lock"))
            && let Some(s) = read_slots(&lane).into_iter().find(|s| s.n == n)
        {
            let _ = write_slot(&lane, &Slot { bytes, shared: 0, ..s });
        }
    }
    let went = sweep(work, budget);
    empty_trash(work);
    changed();
    went
}

/// What checking out in place did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InPlace {
    /// The commit the workspace was at, if it had one.
    pub from: Option<String>,
    /// How many files differ between that commit and the leg's (all of the leg's, from none).
    pub changed: usize,
}

/// Checks `sha` out in place in the repository at `repo` (W24-R11), made if it isn't one yet.
/// `from`, a repository on this machine (this runner's mirror), is where the commit is fetched
/// from when `repo` lacks it. Git's index is refreshed first, so a clone's files (new inodes, new
/// change times) aren't taken for changed ones; `git checkout --force` then writes only the files
/// whose content differs (the rest keep their mtimes), and `git clean -ffdx` removes everything
/// else but `kept` (paths in the workspace).
pub fn check_out(repo: &Path, sha: &str, kept: &[String], from: Option<&Path>) -> anyhow::Result<InPlace> {
    anyhow::ensure!(!sha.is_empty(), "there's no commit to check out");
    if !repo.join(".git").exists() {
        std::fs::create_dir_all(repo)?;
        git(repo, &["init", "--quiet"])?;
    }
    let before = head(repo);
    if !has_commit(repo, sha) {
        let src = from.ok_or_else(|| anyhow::anyhow!("{sha} isn't in the workspace's repository, and nothing here has it"))?;
        git(repo, &["fetch", "--quiet", "--no-tags", &*src.to_string_lossy(), sha])?;
    }
    // `-q`: what really changed is left for the checkout; the rest is refreshed.
    let _ = git(repo, &["update-index", "-q", "--refresh"]);
    let changed = match before.as_deref() {
        Some(b) if b == sha => 0,
        Some(b) => git(repo, &["diff", "--name-only", "--no-renames", b, sha]).map_or(0, |o| o.lines().count()),
        None => git(repo, &["ls-tree", "-r", "--name-only", sha]).map_or(0, |o| o.lines().count()),
    };
    git(repo, &["checkout", "--quiet", "--force", "--detach", sha])?;
    let excludes: Vec<String> = kept.iter().map(|k| format!("/{}", k.trim_matches('/'))).collect();
    let mut clean = vec!["clean", "-ffdxq"];
    for e in &excludes {
        clean.extend(["-e", e.as_str()]);
    }
    git(repo, &clean)?;
    Ok(InPlace { from: before, changed })
}

/// Clears a workspace but for its kept folders (with `git`, its repository's `.git` too), for a leg
/// whose job checks nothing out (W24-R11): what a checkout in place would leave.
pub fn clear_but(ws: &Path, kept: &[String], git: bool) -> std::io::Result<()> {
    let mut keep: Vec<PathBuf> = kept.iter().map(|k| PathBuf::from(k.trim_matches('/'))).collect();
    if git {
        keep.push(PathBuf::from(".git"));
    }
    if !ws.is_dir() {
        return Ok(());
    }
    clear_in(ws, Path::new(""), &keep)
}

fn clear_in(ws: &Path, rel: &Path, keep: &[PathBuf]) -> std::io::Result<()> {
    for e in std::fs::read_dir(ws.join(rel))? {
        let e = e?;
        let r = rel.join(e.file_name());
        if keep.contains(&r) {
            continue;
        }
        if keep.iter().any(|k| k.starts_with(&r)) && e.file_type()?.is_dir() {
            clear_in(ws, &r, keep)?;
            continue;
        }
        remove_any(&e.path())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(dir: &Path, script: &str) -> anyhow::Result<String> {
        let out = Command::new("sh").arg("-c").arg(script).current_dir(dir).env("GIT_CONFIG_GLOBAL", "/dev/null").env("GIT_CONFIG_NOSYSTEM", "1").output()?;
        anyhow::ensure!(out.status.success(), "{script}: {}", String::from_utf8_lossy(&out.stderr));
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    }

    /// A repository of two commits, the second changing `b.txt` alone: their SHAs.
    fn source(dir: &Path) -> anyhow::Result<(String, String)> {
        sh(dir, "git init -q -b main && git config user.name Ada && git config user.email ada@example.com && git config commit.gpgsign false && mkdir -p src && echo a > a.txt && echo b1 > b.txt && echo 'fn main() {}' > src/main.rs && git add . && git commit -qm one")?;
        let one = sh(dir, "git rev-parse HEAD")?;
        sh(dir, "echo b2 > b.txt && git commit -qam two")?;
        Ok((one, sh(dir, "git rev-parse HEAD")?))
    }

    fn ask(sha: &str, trusted: bool) -> Ask {
        let lane = LaneOf { repo: "acme/app".into(), job: ".kiln/ci.yaml#build".into(), ..LaneOf::default() };
        Ask { repo: "acme/app".into(), lane, leg: format!("leg-{}", crate::util::new_id()), sha: sha.to_owned(), trusted, kept: vec!["target".into()], budget: None, name: "app".into() }
    }

    const ROOMY: Budget = Budget { runner: 1 << 40, floor: 0, slots: 4 };

    fn warm(l: anyhow::Result<Leased>) -> anyhow::Result<Lease> {
        match l? {
            Leased::Warm(l) => Ok(l),
            Leased::Cold(why) => anyhow::bail!("cold: {why}"),
        }
    }

    fn mtime(p: &Path) -> anyhow::Result<std::time::SystemTime> {
        Ok(std::fs::metadata(p)?.modified()?)
    }

    const LONG_AGO: Duration = Duration::from_secs(1_000_000_000);

    /// W24-R11, the runner's half of W24-AC02: a leg's slot comes back to the job's next leg with
    /// its build output; checked out in place for the new commit, the unchanged files keep their
    /// mtimes, the changed one is new, and what wasn't kept is gone.
    #[test]
    fn w24_r11_a_slot_keeps_its_build_output_and_unchanged_files_mtimes() -> anyhow::Result<()> {
        let d = tempfile::tempdir()?;
        let src = d.path().join("src");
        std::fs::create_dir_all(&src)?;
        let (one, two) = source(&src)?;
        let work = d.path().join("work");
        let first = warm(lease_with(&work, &ask(&one, true), Clones::No, &ROOMY))?;
        assert!(first.said.contains("new slot 0") && first.dir.ends_with("0"), "{}", first.said);
        let ws = first.dir.join("app");
        let did = check_out(&ws, &one, &["target".into()], Some(&src))?;
        assert_eq!(did, InPlace { from: None, changed: 3 });
        std::fs::create_dir_all(ws.join("target"))?;
        std::fs::write(ws.join("target/out"), "built at one")?;
        std::fs::write(ws.join("stray"), "not kept")?;
        std::fs::create_dir_all(first.dir.join("_temp"))?;
        let old = std::time::UNIX_EPOCH + LONG_AGO;
        for f in ["a.txt", "b.txt", "src/main.rs"] {
            File::options().write(true).open(ws.join(f))?.set_modified(old)?;
        }
        let measured = release(&work, first);
        assert!(settle(&work, measured, &ROOMY).is_empty());
        let lane = lane_dir(&work, "acme/app", &slots::lane(&ask(&one, true).lane).hex());
        let rec = read_slots(&lane);
        assert!(matches!(rec.as_slice(), [s] if s.state == State::Free && s.base.as_deref() == Some(one.as_str()) && s.bytes > 0), "{rec:?}");
        // The next leg, for the next commit, takes it back.
        let second = warm(lease_with(&work, &ask(&two, true), Clones::No, &ROOMY))?;
        assert!(second.said.contains("slot 0") && second.said.contains(one.get(..12).unwrap_or_default()), "{}", second.said);
        assert!(!second.dir.join("_temp").exists(), "what the last leg left beside its workspace goes");
        let ws = second.dir.join("app");
        assert_eq!(check_out(&ws, &two, &["target".into()], Some(&src))?, InPlace { from: Some(one.clone()), changed: 1 });
        assert_eq!(std::fs::read_to_string(ws.join("target/out"))?, "built at one");
        assert_eq!(std::fs::read_to_string(ws.join("b.txt"))?, "b2\n");
        assert!(!ws.join("stray").exists());
        assert_eq!(mtime(&ws.join("a.txt"))?, old, "an unchanged file isn't written again");
        assert_eq!(mtime(&ws.join("src/main.rs"))?, old);
        assert!(mtime(&ws.join("b.txt"))? > old, "the changed one is");
        let measured = release(&work, second);
        settle(&work, measured, &ROOMY);
        assert!(matches!(read_slots(&lane).as_slice(), [s] if s.base.as_deref() == Some(two.as_str())));
        Ok(())
    }

    /// W24-R11: an untrusted leg reads the lane's warm state from a throwaway clone, and nothing it
    /// writes comes back; where the volume can't clone, it runs cold.
    #[test]
    fn w24_r11_an_untrusted_leg_never_writes_back() -> anyhow::Result<()> {
        let d = tempfile::tempdir()?;
        let src = d.path().join("src");
        std::fs::create_dir_all(&src)?;
        let (one, two) = source(&src)?;
        let work = d.path().join("work");
        let l = warm(lease_with(&work, &ask(&one, true), Clones::No, &ROOMY))?;
        let slot = l.dir.clone();
        check_out(&slot.join("app"), &one, &["target".into()], Some(&src))?;
        std::fs::create_dir_all(slot.join("app/target"))?;
        std::fs::write(slot.join("app/target/out"), "trusted")?;
        release(&work, l);
        match lease_with(&work, &ask(&two, false), Clones::No, &ROOMY)? {
            Leased::Cold(why) => assert!(why.contains("can't clone"), "{why}"),
            Leased::Warm(l) => anyhow::bail!("an untrusted leg got {}", l.dir.display()),
        }
        if clones(d.path()) == Clones::Cow {
            let t = warm(lease_with(&work, &ask(&two, false), Clones::Cow, &ROOMY))?;
            assert!(t.dir != slot && t.said.contains("untrusted"), "{}", t.said);
            let ws = t.dir.join("app");
            assert_eq!(std::fs::read_to_string(ws.join("target/out"))?, "trusted", "reads go down");
            check_out(&ws, &two, &["target".into()], Some(&src))?;
            std::fs::write(ws.join("target/out"), "poisoned")?;
            std::fs::write(ws.join("target/planted"), "x")?;
            let gone = t.dir.clone();
            assert_eq!(release(&work, t), None);
            empty_trash(&work);
            assert!(!gone.exists(), "the throwaway clone is removed");
        }
        let back = warm(lease_with(&work, &ask(&two, true), Clones::No, &ROOMY))?;
        assert_eq!(back.dir, slot);
        assert_eq!(std::fs::read_to_string(slot.join("app/target/out"))?, "trusted");
        assert!(!slot.join("app/target/planted").exists());
        release(&work, back);
        Ok(())
    }

    /// W24-R11: a second leg of the lane, while the first runs, gets a copy-on-write clone of the
    /// workspace as the first found it, never a copy of one being written; without clones, a new
    /// slot of its own.
    #[test]
    fn w24_r11_a_second_leg_gets_a_clone_of_a_workspace_no_one_writes() -> anyhow::Result<()> {
        let d = tempfile::tempdir()?;
        let src = d.path().join("src");
        std::fs::create_dir_all(&src)?;
        let (one, two) = source(&src)?;
        let work = d.path().join("work");
        let mode = clones(d.path());
        let l = warm(lease_with(&work, &ask(&one, true), mode, &ROOMY))?;
        check_out(&l.dir.join("app"), &one, &["target".into()], Some(&src))?;
        std::fs::create_dir_all(l.dir.join("app/target"))?;
        std::fs::write(l.dir.join("app/target/out"), "one")?;
        let old = std::time::UNIX_EPOCH + LONG_AGO;
        File::options().write(true).open(l.dir.join("app/a.txt"))?.set_modified(old)?;
        release(&work, l);
        let first = warm(lease_with(&work, &ask(&two, true), mode, &ROOMY))?;
        std::fs::write(first.dir.join("app/target/new"), "being written")?;
        let second = warm(lease_with(&work, &ask(&two, true), mode, &ROOMY))?;
        assert_ne!(first.dir, second.dir);
        let ws = second.dir.join("app");
        if mode == Clones::Cow {
            assert!(first.said.contains("copy-on-write clone of it (slot 1"), "{}", first.said);
            assert!(second.said.contains("slot 1") && second.dir.ends_with("1"), "{}", second.said);
            assert_eq!(std::fs::read_to_string(ws.join("target/out"))?, "one");
            assert!(!ws.join("target/new").exists(), "the clone is of the workspace before its leg wrote");
            assert_eq!(mtime(&ws.join("a.txt"))?, old, "a clone keeps mtimes");
            assert_eq!(check_out(&ws, &two, &["target".into()], Some(&src))?.changed, 1);
            assert_eq!(mtime(&ws.join("a.txt"))?, old, "and its checkout doesn't write unchanged files again");
        } else {
            assert!(second.said.contains("new slot 1") && !ws.join("target").exists(), "{}", second.said);
        }
        release(&work, first);
        release(&work, second);
        Ok(())
    }

    /// W24-R11: a slot whose leg died (its runner stopped: the lease dropped, never released) is
    /// reset to its repository for the lane's next leg, its kept folders gone with the rest.
    #[test]
    fn w24_r11_a_slot_whose_leg_died_is_reset() -> anyhow::Result<()> {
        let d = tempfile::tempdir()?;
        let src = d.path().join("src");
        std::fs::create_dir_all(&src)?;
        let (one, two) = source(&src)?;
        let work = d.path().join("work");
        let l = warm(lease_with(&work, &ask(&one, true), Clones::No, &ROOMY))?;
        check_out(&l.dir.join("app"), &one, &["target".into()], Some(&src))?;
        release(&work, l);
        let dying = warm(lease_with(&work, &ask(&two, true), Clones::No, &ROOMY))?;
        let slot = dying.dir.clone();
        std::fs::create_dir_all(slot.join("app/target"))?;
        std::fs::write(slot.join("app/target/half"), "half written")?;
        std::fs::write(slot.join("app/.git/index.lock"), "")?;
        drop(dying);
        let next = warm(lease_with(&work, &ask(&two, true), Clones::No, &ROOMY))?;
        assert_eq!(next.dir, slot);
        assert!(next.said.contains("with no checkout"), "{}", next.said);
        assert!(!slot.join("app/target").exists() && !slot.join("app/a.txt").exists() && slot.join("app/.git").is_dir(), "only its repository is left");
        let did = check_out(&slot.join("app"), &two, &["target".into()], Some(&src))?;
        assert_eq!(did.from.as_deref(), Some(one.as_str()));
        assert_eq!(std::fs::read_to_string(slot.join("app/b.txt"))?, "b2\n");
        release(&work, next);
        // One that died before its first checkout holds nothing worth keeping: it goes.
        let lane = lane_dir(&work, "acme/other", &slots::lane(&LaneOf { repo: "acme/other".into(), ..LaneOf::default() }).hex());
        let fresh = Ask { repo: "acme/other".into(), lane: LaneOf { repo: "acme/other".into(), ..LaneOf::default() }, ..ask(&one, true) };
        drop(warm(lease_with(&work, &fresh, Clones::No, &ROOMY))?);
        let again = warm(lease_with(&work, &fresh, Clones::No, &ROOMY))?;
        assert!(again.said.contains("new slot 0") && read_slots(&lane).len() == 1, "{}", again.said);
        release(&work, again);
        Ok(())
    }

    /// W24-R15: past the runner's budget, the least recently used slot goes, never a leased one
    /// however old; below the floor nothing new is kept.
    #[test]
    fn w24_r15_over_budget_the_least_recently_used_slot_goes() -> anyhow::Result<()> {
        let d = tempfile::tempdir()?;
        let src = d.path().join("src");
        std::fs::create_dir_all(&src)?;
        let (one, _) = source(&src)?;
        let work = d.path().join("work");
        let job = |j: &str| Ask { lane: LaneOf { repo: "acme/app".into(), job: format!(".kiln/ci.yaml#{j}"), ..LaneOf::default() }, ..ask(&one, true) };
        let (mut leased, mut dirs) = (Vec::new(), Vec::new());
        // Three lanes' slots of 4 MiB and a little: let go long ago, lately, and still leased.
        for (j, used) in [("old", 100), ("new", 200), ("held", 0)] {
            let l = warm(lease_with(&work, &job(j), Clones::No, &ROOMY))?;
            check_out(&l.dir.join("app"), &one, &[], Some(&src))?;
            std::fs::write(l.dir.join("app/big"), vec![7u8; 4 << 20])?;
            dirs.push(l.dir.clone());
            let (lane, n) = (l.lane.clone(), l.n.unwrap_or_default());
            if j == "held" {
                leased.push(l);
            } else {
                release(&work, l);
            }
            let s = read_slots(&lane).into_iter().find(|s| s.n == n).unwrap_or_default();
            write_slot(&lane, &Slot { bytes: usage(&slot_dir(&lane, n)), used, ..s })?;
        }
        // A budget for two of them: the one let go first goes, and the leased one stays.
        let went = sweep(&work, &Budget { runner: 10 << 20, ..ROOMY });
        assert_eq!(went.len(), 1, "{went:?}");
        empty_trash(&work);
        let exist: Vec<bool> = dirs.iter().map(|p| p.exists()).collect();
        assert_eq!(exist, [false, true, true]);
        assert!(std::fs::read_dir(root(&work).join(".trash"))?.next().is_none(), "the trash is emptied");
        // Below the floor, a leg finding its lane's one slot leased runs cold.
        match lease_with(&work, &job("held"), Clones::Cow, &Budget { floor: u64::MAX, ..ROOMY })? {
            Leased::Cold(why) => assert!(why.contains("below the floor"), "{why}"),
            Leased::Warm(l) => anyhow::bail!("a slot past the floor: {}", l.dir.display()),
        }
        drop(leased);
        Ok(())
    }

    /// W24-R18: what the runner tells the server it holds warm: each lane a lease made, with its
    /// slots and the history its trusted legs left there, warm and cold. A run of a lane no lease
    /// made leaves nothing, and every change moves [`changes`].
    #[test]
    fn w24_r18_what_the_runner_says_it_holds_warm() -> anyhow::Result<()> {
        let d = tempfile::tempdir()?;
        let src = d.path().join("src");
        std::fs::create_dir_all(&src)?;
        let (one, _) = source(&src)?;
        let work = d.path().join("work");
        assert!(state(&work).lanes.is_empty(), "nothing yet");
        let before = changes();
        let first = warm(lease_with(&work, &ask(&one, true), Clones::No, &ROOMY))?;
        assert!(!first.warm, "a new, empty slot: the leg starts cold");
        check_out(&first.dir.join("app"), &one, &[], Some(&src))?;
        let lane = first.lane.clone();
        release(&work, first);
        ran(&lane, false, 90_000);
        ran(&lane, true, 20_000);
        ran(&d.path().join("no-lane"), true, 1);
        assert!(changes() >= before + 4, "a lease, a release and two runs");
        let s = state(&work);
        let [l] = s.lanes.as_slice() else { anyhow::bail!("one lane: {s:?}") };
        assert_eq!((l.key.clone(), l.lane.clone()), (slots::lane(&ask(&one, true).lane).hex(), ask(&one, true).lane));
        assert!(matches!(l.slots.as_slice(), [x] if x.state == State::Free && x.base.as_deref() == Some(one.as_str())), "{:?}", l.slots);
        assert_eq!((l.runs.warm_ms.clone(), l.runs.cold_ms.clone()), (vec![20_000], vec![90_000]));
        // The lane's next leg starts from that checkout: warm.
        let next = warm(lease_with(&work, &ask(&one, true), Clones::No, &ROOMY))?;
        assert!(next.warm, "{}", next.said);
        release(&work, next);
        Ok(())
    }

    /// W24-R18: a slot released is the lane's next leg's at once, though a copy of its lock's
    /// descriptor lives on, as one does in a child this process forks until its exec: in kiln's lib
    /// run on fedora (2026-10-28) the next lease found the slot leased and started cold.
    #[test]
    fn w24_r18_a_released_slot_is_free_though_its_lock_was_copied() -> anyhow::Result<()> {
        let d = tempfile::tempdir()?;
        let src = d.path().join("src");
        std::fs::create_dir_all(&src)?;
        let (one, _) = source(&src)?;
        let work = d.path().join("work");
        let first = warm(lease_with(&work, &ask(&one, true), Clones::No, &ROOMY))?;
        check_out(&first.dir.join("app"), &one, &[], Some(&src))?;
        let copy = first._lock.try_clone()?;
        release(&work, first);
        let next = warm(lease_with(&work, &ask(&one, true), Clones::No, &ROOMY))?;
        assert!(next.warm, "{}", next.said);
        drop(copy);
        release(&work, next);
        Ok(())
    }

    #[test]
    fn w24_r11_kept_folders_are_paths_in_the_workspace() {
        let given: Vec<String> = ["target", "target/", "node_modules", "/etc", "../up", "a/../b", "**/dist", ".git", "C:/x", "", "crates/x/target"].map(str::to_owned).into();
        let (ok, refused) = kept_paths(&given);
        assert_eq!(ok, ["target", "node_modules", "crates/x/target"]);
        assert_eq!(refused, ["/etc", "../up", "a/../b", "**/dist", ".git", "C:/x", ""]);
    }

    #[test]
    fn w24_r11_a_workspace_with_no_checkout_keeps_only_its_kept_folders() -> anyhow::Result<()> {
        let d = tempfile::tempdir()?;
        let ws = d.path();
        for f in ["target/a", "crates/x/target/b", "crates/x/src/c", "crates/y", "stray", ".git/HEAD"] {
            let p = ws.join(f);
            std::fs::create_dir_all(p.parent().unwrap_or(ws))?;
            std::fs::write(p, "x")?;
        }
        clear_but(ws, &["target".into(), "crates/x/target".into()], true)?;
        let left: Vec<bool> = ["target/a", "crates/x/target/b", ".git/HEAD", "crates/x/src/c", "crates/y", "stray"].iter().map(|f| ws.join(f).exists()).collect();
        assert_eq!(left, [true, true, true, false, false, false]);
        clear_but(ws, &["target".into()], false)?;
        assert!(!ws.join(".git").exists() && !ws.join("crates").exists() && ws.join("target/a").exists());
        Ok(())
    }

    #[test]
    fn w24_r11_a_legs_lane_is_its_workflow_job_matrix_and_executor() {
        let mut a = AssignmentV2 { repo: "acme/app".into(), ..AssignmentV2::default() };
        a.leg.job_id = "build".into();
        a.contexts.github = serde_json::json!({ "repository": "acme/app", "workflow_ref": "acme/app/.kiln/ci.yaml@refs/heads/feature" });
        a.contexts.matrix = serde_json::json!({ "os": "linux", "n": 2 });
        let l = lane_of(&a);
        assert_eq!((l.job.as_str(), l.toolchain.as_str()), (".kiln/ci.yaml#build", "host||"));
        assert_eq!(l.matrix, BTreeMap::from([("n".to_owned(), "2".to_owned()), ("os".to_owned(), "linux".to_owned())]));
        // Another branch's leg of the job shares the lane; another image doesn't.
        let mut main = a.clone();
        main.contexts.github = serde_json::json!({ "repository": "acme/app", "workflow_ref": "acme/app/.kiln/ci.yaml@refs/heads/main" });
        assert_eq!(slots::lane(&lane_of(&main)), slots::lane(&l));
        main.executor.kind = "docker".into();
        main.executor.image = Some("kiln/runner-ubuntu-24.04".into());
        assert_ne!(slots::lane(&lane_of(&main)), slots::lane(&l));
    }

    /// W24-AC11 (a bench): cloning the folder `KILN_W24_AC11` names copy-on-write, beside it, takes
    /// under 10 s; the clone is removed after. Skipped without it.
    #[test]
    #[ignore = "a bench: KILN_W24_AC11=<a large folder on a volume that clones>"]
    fn w24_ac11_a_large_workspace_clones_in_under_ten_seconds() -> anyhow::Result<()> {
        let Some(from) = std::env::var_os("KILN_W24_AC11").map(PathBuf::from) else { return Ok(()) };
        let to = from.with_file_name(format!(".kiln-ac11-{}", crate::util::new_id()));
        let took = clone_tree(&from, &to);
        let removing = Instant::now();
        super::super::workspaces::remove(&to);
        let took = took?;
        eprintln!("W24-AC11: {} ({}) cloned in {:.2}s; the clone removed in {:.2}s", from.display(), gib(usage(&from)), took.as_secs_f64(), removing.elapsed().as_secs_f64());
        anyhow::ensure!(took < Duration::from_secs(10), "{took:?}");
        Ok(())
    }

    /// W24-R31 on a runner: a small volume (100 GiB) filled past its floor reaps its warm
    /// workspaces until the target is free, keeping each lane's newest; each eviction says why; a
    /// scheduled reap then drops what `max_age` names.
    #[test]
    fn w24_r31_the_runner_reaps_to_its_target_and_on_schedule() -> anyhow::Result<()> {
        const GIB: u64 = 1 << 30;
        let work = tempfile::tempdir()?;
        let now = 1_800_000_000i64;
        let mut held = 0u64;
        for l in 0..3 {
            let lane = lane_dir(work.path(), "acme/app", &format!("lane{l}"));
            std::fs::create_dir_all(&lane)?;
            for n in 0..4u32 {
                std::fs::create_dir_all(slot_dir(&lane, n))?;
                write_slot(&lane, &Slot { n, used: now - 86_400 * i64::from(1 + n), bytes: 6 * GIB, ..Slot::default() })?;
                held += 6 * GIB;
            }
        }
        let others = 13 * GIB;
        let size = 100 * GIB;
        let system = reap::Policy { floor: Some(reap::Size::Bytes(20 * GIB)), target: Some(reap::Size::Bytes(30 * GIB)), ceiling: Some(reap::Size::Bytes(size)), keep_last: Some(1), keep_within: Some(0), ..reap::Policy::default() };
        let layers = vec![(reap::Scope::System, system)];
        let free = size - held - others;
        let (why, went) = reap(work.path(), &layers, Some(now), now, Some((size, free)));
        anyhow::ensure!(why == Some(reap::Trigger::Pressure), "{why:?}");
        let freed: u64 = went.iter().map(|e| e.bytes).sum();
        anyhow::ensure!(free + freed >= 30 * GIB, "free space back to the target: {} GiB", (free + freed) / GIB);
        anyhow::ensure!(went.iter().all(|e| e.reason == reap::Reason::Target && e.rule == "target" && e.scope == Some(reap::Scope::System)), "{went:?}");
        for l in 0..3 {
            let lane = lane_dir(work.path(), "acme/app", &format!("lane{l}"));
            anyhow::ensure!(read_slots(&lane).iter().any(|s| s.n == 0) && slot_dir(&lane, 0).exists(), "lane{l}'s newest remains");
        }
        // Kept for the server; other tests' reaps in this process may be kept beside them.
        let kept = take_reaped();
        anyhow::ensure!(went.iter().all(|e| kept.iter().any(|k| k.id == e.id)), "kept for the server");
        // A day later the schedule is due: a slot unused for 40 days goes, by max_age.
        let lane = lane_dir(work.path(), "acme/app", "lane2");
        let old = read_slots(&lane).into_iter().find(|s| s.n != 0).ok_or_else(|| anyhow::anyhow!("lane2 kept only its newest"))?;
        write_slot(&lane, &Slot { used: now - 40 * 86_400, ..old.clone() })?;
        let (why, went) = reap(work.path(), &layers, Some(now - 2 * 86_400), now, Some((size, 60 * GIB)));
        anyhow::ensure!(why == Some(reap::Trigger::Schedule), "{why:?}");
        anyhow::ensure!(went.len() == 1 && went.first().is_some_and(|e| e.reason == reap::Reason::MaxAge && e.id.ends_with(&format!("#{}", old.n))), "{went:?}");
        Ok(())
    }

    /// W24-AC15 on a runner: a week of legs on a 100 GiB volume, three lanes adding two 4 GiB slots
    /// a day, the system's and the runner's policies (the runner's floor the stricter). After every
    /// reap free space is at or above the floor while anything is left to evict; what max_age names
    /// goes; each lane's newest two stay unless the floor needed them, as their records say.
    #[test]
    fn w24_ac15_a_week_on_a_runner() -> anyhow::Result<()> {
        const GIB: u64 = 1 << 30;
        let work = tempfile::tempdir()?;
        let (size, others) = (100 * GIB, 40 * GIB);
        let system = reap::Policy { floor: Some(reap::Size::Bytes(20 * GIB)), target: Some(reap::Size::Bytes(25 * GIB)), keep_last: Some(2), keep_within: Some(0), max_age: Some(3 * 86_400), schedule: Some(reap::Schedule::Every(86_400)), ..reap::Policy::default() };
        let runner = reap::Policy { floor: Some(reap::Size::Bytes(22 * GIB)), ..reap::Policy::default() };
        let layers = vec![(reap::Scope::System, system), (reap::Scope::Runner, runner)];
        let start = 1_800_000_000i64;
        let mut last: Option<i64> = None;
        let mut next = [0u32; 3];
        for day in 0..7i64 {
            let now = start + day * 86_400;
            for (l, n) in next.iter_mut().enumerate() {
                let lane = lane_dir(work.path(), "acme/app", &format!("lane{l}"));
                std::fs::create_dir_all(&lane)?;
                for _ in 0..2 {
                    std::fs::create_dir_all(slot_dir(&lane, *n))?;
                    write_slot(&lane, &Slot { n: *n, used: now - i64::from(*n % 2) * 60, bytes: 4 * GIB, ..Slot::default() })?;
                    *n += 1;
                }
            }
            let held = |w: &Path| lanes(w).iter().flat_map(|l| read_slots(l)).map(|s| s.bytes).sum::<u64>();
            let free = size - others - held(work.path());
            let (why, went) = reap(work.path(), &layers, last, now, Some((size, free)));
            if why == Some(reap::Trigger::Schedule) {
                last = Some(now);
            }
            let after = size - others - held(work.path());
            let left = lanes(work.path()).iter().map(|l| read_slots(l).len()).sum::<usize>();
            anyhow::ensure!(after >= 22 * GIB || left == 0, "day {day}: {} GiB free after the reap ({why:?})", after / GIB);
            let floor_needed = went.iter().any(|e| e.reason == reap::Reason::Floor);
            for l in 0..3 {
                let lane = lane_dir(work.path(), "acme/app", &format!("lane{l}"));
                let mut slots = read_slots(&lane);
                slots.sort_by(|a, b| b.used.cmp(&a.used).then(a.n.cmp(&b.n)));
                anyhow::ensure!(floor_needed || slots.len() >= 2, "day {day}: lane{l} keeps its newest two: {slots:?}");
                for s in slots.iter().skip(2) {
                    anyhow::ensure!(now - s.used <= 3 * 86_400 || why.is_none(), "day {day}: lane{l}'s slot {} is past max_age", s.n);
                }
            }
            anyhow::ensure!(went.iter().all(|e| matches!(e.rule, "max_age" | "target" | "ceiling" | "floor")), "each eviction names its rule: {went:?}");
        }
        anyhow::ensure!(last.is_some(), "the schedule ran");
        Ok(())
    }
}
