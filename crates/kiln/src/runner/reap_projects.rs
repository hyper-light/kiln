//! W24-R31's reaper over this machine's own projects and `~/.cache` (Ada, 2026-11-11: "The reaper
//! needs to keep things cleaned up. Using any active session's agent to intermittently cleanup
//! specific project build files and then keeping ~/.cache clean"; asked who cleans: both).
//!
//! The projects are the folders this runner's agent sessions work in (each one's repository, or
//! its folder). In them, and in `~/.cache`, the reaper recognizes build output only by its
//! `CACHEDIR.TAG` (cargo's targets and every tool that keeps that convention); nothing else is
//! ever its to delete. (Until 2026-11-14 it took each folder of `~/.cache` that wasn't a repository
//! for a cache as a whole, and on adacachy that deleted agents' patches, research, scripts and
//! scratch databases kept there.)
//!
//! It decides on measurement alone, with no floor, ceiling, target, age or schedule set for it
//! (`kiln_ci::work::reap::projects`): each look notes the free space and the time, keeps the
//! fastest rate it has seen the disk fill between two looks, and how long its last pass took. Only
//! when, at that rate, the disk would fill within the interval between its looks and the time a
//! pass takes does it delete, least recently used first and none a process names, just enough
//! that it no longer would. If its own deletions leave some of that still needed, an idle agent of
//! the project holding the most the reaper doesn't recognize is asked to clean it, one ask at a
//! time (`kiln_ci::work::reap::projects::ask`). Sources, repositories and work not committed are
//! never the reaper's.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, OnceLock};
use std::time::Duration;

use kiln_ci::work::reap::{self, projects as pr};

/// One of this runner's agent sessions, as the reaper sees it.
#[derive(Debug, Clone)]
pub struct Session {
    pub id: String,
    pub cwd: String,
    /// Between turns: a message starts one.
    pub idle: bool,
    /// The ids of the messages waiting in its queue.
    pub queued: Vec<String>,
}

type Sessions = Box<dyn Fn() -> Vec<Session> + Send + Sync>;
/// Queues kiln's message (its id, its words) for a session; whether it was taken.
type Tell = Box<dyn Fn(&str, String, String) -> bool + Send + Sync>;

static SESSIONS: OnceLock<Sessions> = OnceLock::new();
static TELL: OnceLock<Tell> = OnceLock::new();

/// This runner's agent sessions (`Host::list`), for the projects they work in.
pub fn set_sessions(f: impl Fn() -> Vec<Session> + Send + Sync + 'static) {
    let _ = SESSIONS.set(Box::new(f));
}

/// How kiln's message reaches one of this runner's sessions (`Host::command`).
pub fn set_tell(f: impl Fn(&str, String, String) -> bool + Send + Sync + 'static) {
    let _ = TELL.set(Box::new(f));
}

/// The reaper's ask still with an agent: its session, the message's id, and the project it asked
/// to be cleaned.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Asked {
    session: String,
    id: String,
    root: String,
}

static ASKED: LazyLock<parking_lot::Mutex<Option<Asked>>> = LazyLock::new(Default::default);

/// What `~/.cache` entries are grouped under.
const CACHE_GROUP: &str = "~/.cache";

/// Whether the reaper's last ask is still with its agent: waiting in its queue, or the turn it
/// started under way. An agent gone, or idle with it taken, has answered: the project it was asked
/// to clean is said then, once.
fn waiting(sessions: &[Session]) -> (bool, Option<String>) {
    let mut asked = ASKED.lock();
    let still = asked.as_ref().is_some_and(|a| sessions.iter().any(|s| s.id == a.session && (s.queued.contains(&a.id) || !s.idle)));
    if still {
        return (true, None);
    }
    (false, asked.take().map(|a| a.root))
}

/// What the project at `root` holds that the reaper doesn't recognize, measured now; none if it's no
/// project of `sessions` now.
fn unrecognized(home: &Path, sessions: &[Session], root: &Path) -> Option<u64> {
    let roots = roots(home, sessions);
    let nested: Vec<&Path> = roots.keys().map(PathBuf::as_path).filter(|p| *p != root && p.starts_with(root)).collect();
    roots.contains_key(root).then(|| walk(root, "", &nested, &|_| false, &mut Vec::new()).bytes)
}

/// A folder's repository (the nearest folder above it holding `.git`), else the folder itself.
fn project_root(cwd: &Path) -> PathBuf {
    cwd.ancestors().find(|a| a.join(".git").exists()).unwrap_or(cwd).to_path_buf()
}

/// The projects `sessions` work in, each with its agents: none the home folder itself, or above it
/// (a session at work in `~` gives no project: its folders are others').
fn roots(home: &Path, sessions: &[Session]) -> BTreeMap<PathBuf, Vec<pr::Agent>> {
    let mut out: BTreeMap<PathBuf, Vec<pr::Agent>> = BTreeMap::new();
    for s in sessions.iter().filter(|s| !s.cwd.is_empty()) {
        let root = project_root(Path::new(&s.cwd));
        if home.starts_with(&root) || !root.is_dir() {
            continue;
        }
        out.entry(root).or_default().push(pr::Agent { session: s.id.clone(), idle: s.idle });
    }
    out
}

/// What a file takes on disk.
fn disk_bytes(m: &std::fs::Metadata) -> u64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        m.blocks().saturating_mul(512)
    }
    #[cfg(not(unix))]
    {
        m.len()
    }
}

#[cfg(unix)]
fn device(m: &std::fs::Metadata) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    Some(m.dev())
}

#[cfg(not(unix))]
fn device(_: &std::fs::Metadata) -> Option<u64> {
    None
}

fn mtime(m: &std::fs::Metadata) -> i64 {
    m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).and_then(|d| i64::try_from(d.as_secs()).ok()).unwrap_or(0)
}

/// A folder's size on disk and when anything in it was last written, its symlinks not followed
/// and other volumes not entered. On a worklist, not by recursion.
fn measure(dir: &Path) -> (u64, i64) {
    let Ok(top) = std::fs::symlink_metadata(dir) else { return (0, 0) };
    let dev = device(&top);
    let (mut bytes, mut used) = (disk_bytes(&top), mtime(&top));
    if !top.is_dir() {
        return (bytes, used);
    }
    // A mount point under it is never stat'd: a FUSE mount whose server is gone never answers (W06-R01).
    let mounts: Vec<PathBuf> = crate::os::tree::mounts_under(dir).into_iter().map(|(p, _)| p).collect();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            if mounts.contains(&e.path()) {
                continue;
            }
            let Ok(m) = e.metadata() else { continue };
            bytes = bytes.saturating_add(disk_bytes(&m));
            used = used.max(mtime(&m));
            if m.is_dir() && device(&m) == dev {
                stack.push(e.path());
            }
        }
    }
    (bytes, used)
}

/// What a walk found besides the build output it recognized.
#[derive(Default)]
struct Rest {
    /// The bytes of the rest, `.git` left out (no one cleans that).
    bytes: u64,
}

/// What a folder at `root` holds: its build output, each folder marked `CACHEDIR.TAG`, as a
/// [`pr::Found`] of `group`; and the rest. The folders of `nested` (projects) are theirs.
fn walk(root: &Path, group: &str, nested: &[&Path], busy: &dyn Fn(&Path) -> bool, found: &mut Vec<pr::Found>) -> Rest {
    let mut rest = Rest::default();
    let Ok(top) = std::fs::symlink_metadata(root) else { return rest };
    let dev = device(&top);
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        if tagged(&d) && d != root {
            let (bytes, used) = measure(&d);
            found.push(pr::Found { path: d.to_string_lossy().into_owned(), group: group.to_owned(), bytes, used, busy: busy(&d) });
            continue;
        }
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let Ok(m) = e.metadata() else { continue };
            let p = e.path();
            if e.file_name() == ".git" {
                continue;
            }
            if m.is_dir() {
                if nested.iter().any(|n| *n == p) || device(&m) != dev {
                    continue;
                }
                stack.push(p);
            } else {
                rest.bytes = rest.bytes.saturating_add(disk_bytes(&m));
            }
        }
    }
    rest
}

/// What the reaper found on this machine: what it recognizes, and each project with what else it
/// holds.
pub struct Scan {
    pub found: Vec<pr::Found>,
    pub projects: Vec<pr::Project>,
}

/// Finds what the projects of `sessions` and `home`'s `.cache` hold. `busy`: whether a process
/// names a folder now. Blocking.
pub fn scan(home: &Path, sessions: &[Session], busy: &dyn Fn(&Path) -> bool) -> Scan {
    let roots = roots(home, sessions);
    let paths: Vec<&Path> = roots.keys().map(PathBuf::as_path).collect();
    let mut found = Vec::new();
    let mut projects = Vec::new();
    for (root, agents) in &roots {
        let nested: Vec<&Path> = paths.iter().copied().filter(|p| p != root && p.starts_with(root)).collect();
        let group = root.to_string_lossy().into_owned();
        let bytes = walk(root, &group, &nested, busy, &mut found).bytes;
        projects.push(pr::Project { root: group, bytes, agents: agents.clone() });
    }
    let cache = home.join(".cache");
    for e in std::fs::read_dir(&cache).into_iter().flatten().flatten() {
        let p = e.path();
        // A project's (walked above): its build output is found there.
        let inside: Vec<&Path> = paths.iter().copied().filter(|r| r.starts_with(&p)).collect();
        if inside.iter().any(|r| *r == p) {
            continue;
        }
        // W24-R31: only what's marked `CACHEDIR.TAG` is the reaper's, in `~/.cache` as in a project.
        // A file there, or a folder unmarked, is someone's (a patch, notes, a script, research):
        // never taken whole for a cache.
        let Ok(m) = std::fs::symlink_metadata(&p) else { continue };
        if !m.is_dir() {
            continue;
        }
        if tagged(&p) && inside.is_empty() {
            let (bytes, used) = measure(&p);
            found.push(pr::Found { path: p.to_string_lossy().into_owned(), group: CACHE_GROUP.to_owned(), bytes, used, busy: busy(&p) });
            continue;
        }
        walk(&p, CACHE_GROUP, &inside, busy, &mut found);
    }
    Scan { found, projects }
}

/// A folder marked as build output or a cache by the convention's `CACHEDIR.TAG`: the only thing
/// the reaper recognizes (W24-R31).
fn tagged(dir: &Path) -> bool {
    dir.join("CACHEDIR.TAG").is_file()
}

/// The command lines of the machine's processes now: a folder one names is in use.
fn processes() -> Vec<String> {
    #[cfg(unix)]
    {
        std::process::Command::new("ps").args(["-axo", "command="]).output().ok().map(|o| String::from_utf8_lossy(&o.stdout).lines().map(str::to_owned).collect()).unwrap_or_default()
    }
    #[cfg(not(unix))]
    {
        Vec::new()
    }
}

fn names(lines: &[String], p: &Path) -> bool {
    let p = p.to_string_lossy();
    lines.iter().any(|l| l.contains(p.as_ref()))
}

/// Cargo builds into `dir` now: it holds a profile's `.cargo-lock` (`<dir>/<profile>/` or, for a
/// target named, `<dir>/<triple>/<profile>/`) while it builds, which a build whose target folder
/// only its environment names shows no other way (Mantle, 2026-11-11).
fn cargo_building(dir: &Path) -> bool {
    let subdirs = |d: &Path| -> Vec<PathBuf> { std::fs::read_dir(d).into_iter().flatten().flatten().filter(|e| e.file_type().is_ok_and(|t| t.is_dir())).map(|e| e.path()).collect() };
    let held = |lock: &Path| std::fs::File::open(lock).is_ok_and(|f| matches!(f.try_lock(), Err(std::fs::TryLockError::WouldBlock)));
    subdirs(dir).iter().any(|p| held(&p.join(".cargo-lock")) || subdirs(p).iter().any(|q| held(&q.join(".cargo-lock"))))
}

/// A folder is in use: a process names it, or cargo builds into it.
fn in_use(lines: &[String], p: &Path) -> bool {
    names(lines, p) || cargo_building(p)
}

/// Deletes what the reaper found at `path`, unless it isn't, or is no longer, what it recognizes (a
/// folder marked `CACHEDIR.TAG`), or has become what it never takes: a project, a folder holding
/// one, a repository, `~/.cache` itself, or in use. What it freed.
fn delete(path: &Path, home: &Path, roots: &[PathBuf], busy: &dyn Fn(&Path) -> bool) -> Option<u64> {
    if !tagged(path) || path == home.join(".cache") || roots.iter().any(|r| r.starts_with(path)) || path.join(".git").exists() || busy(path) {
        return None;
    }
    let (bytes, _) = measure(path);
    let m = std::fs::symlink_metadata(path).ok()?;
    let gone = if m.is_dir() {
        // Go's module cache makes its folders read-only: they're made writable again to be emptied.
        crate::os::tree::remove_dir_all(path).or_else(|e| if e.kind() == std::io::ErrorKind::PermissionDenied { writable(path).and_then(|()| crate::os::tree::remove_dir_all(path)) } else { Err(e) })
    } else {
        std::fs::remove_file(path)
    };
    match gone {
        Ok(()) => Some(bytes),
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "the reaper couldn't delete what it found");
            None
        }
    }
}

/// Gives its owner write on `dir` and every folder under it (its symlinks not followed).
fn writable(dir: &Path) -> std::io::Result<()> {
    let mounts: Vec<PathBuf> = crate::os::tree::mounts_under(dir).into_iter().map(|(p, _)| p).collect();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let m = std::fs::symlink_metadata(&d)?;
        if !m.is_dir() {
            continue;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = m.permissions();
            perms.set_mode(perms.mode() | 0o700);
            std::fs::set_permissions(&d, perms)?;
        }
        for e in std::fs::read_dir(&d)?.flatten() {
            if e.file_type().is_ok_and(|t| t.is_dir()) && !mounts.contains(&e.path()) {
                stack.push(e.path());
            }
        }
    }
    Ok(())
}

/// The reaper's state between its looks (W24-R31), every value of it measured, none set: the
/// previous look (unix seconds, bytes free then), the fastest the disk has filled between two of
/// its looks in this process, the interval between its last two looks (seconds), how long its last
/// pass took (seconds, rounded up), the free space a pass under pressure last found nothing to do
/// at, and what each project held that it doesn't recognize when its agent last answered an ask
/// (by root: it isn't asked again until it holds more).
#[derive(Default)]
pub struct State {
    pub previous: Option<(i64, u64)>,
    pub fastest: Option<pr::Rate>,
    pub interval: u64,
    pub pass: u64,
    pub fruitless: Option<u64>,
    pub answered: BTreeMap<String, u64>,
}

/// An ask the reaper makes, with what it measured to make it.
struct Asking {
    ask: pr::Ask,
    free: u64,
    need: u64,
    rate: pr::Rate,
    horizon: u64,
}

/// One look of the reaper at this machine's projects (`now`: unix seconds). Blocking.
pub fn look(home: &Path, machine: &str, state: &mut State, now: i64) {
    let Some((_, free)) = crate::os::disk_space(home) else { return };
    let sessions = SESSIONS.get().map(|f| f()).unwrap_or_default();
    let (waiting, answered) = waiting(&sessions);
    // What the project holds now that its agent answered: it isn't asked again until it holds more.
    if let Some(root) = answered
        && let Some(bytes) = unrecognized(home, &sessions, Path::new(&root))
    {
        tracing::info!(project = %root, bytes, "the reaper's ask was answered: it asks this project again only once it holds more");
        state.answered.insert(root, bytes);
    }
    let Some(a) = look_at(home, &sessions, waiting, free, state, now, &processes) else { return };
    let id = format!("{}reap-{now}", crate::harness::core::KEEP_ID);
    let text = pr::ask_text(machine, a.free, a.need, a.rate, a.horizon, &a.ask);
    if TELL.get().is_some_and(|tell| tell(&a.ask.session, id.clone(), text)) {
        tracing::info!(session = %a.ask.session, project = %a.ask.root, bytes = a.ask.bytes, free = a.free, need = a.need, "the reaper asked an agent to clean its project");
        *ASKED.lock() = Some(Asked { session: a.ask.session, id, root: a.ask.root });
    }
}

/// [`look`] with `free` bytes free now, `waiting` whether its last ask is still with its agent,
/// and `processes` the machine's command lines when asked (a folder one names is in use). It
/// measures, deletes what the measurement needs ([`pr::need`], [`pr::plan`]), and says whom it
/// would ask to clean, if anyone.
fn look_at(home: &Path, sessions: &[Session], waiting: bool, free: u64, state: &mut State, now: i64, processes: &dyn Fn() -> Vec<String>) -> Option<Asking> {
    state.fastest = pr::fastest(state.fastest, state.previous, (now, free));
    if let Some((then, _)) = state.previous {
        state.interval = u64::try_from(now.saturating_sub(then)).unwrap_or(0);
    }
    state.previous = Some((now, free));
    let horizon = state.interval.saturating_add(state.pass);
    let need = pr::need(free, state.fastest, horizon);
    let Some(rate) = state.fastest.filter(|_| need > 0) else {
        state.fruitless = None;
        return None;
    };
    // A pass that found nothing to do isn't made again until free space falls further, or while
    // the agent it asked hasn't answered.
    if waiting || state.fruitless.is_some_and(|f| free >= f) {
        return None;
    }
    let started = std::time::Instant::now();
    let lines = processes();
    let busy = |p: &Path| in_use(&lines, p);
    let found = scan(home, sessions, &busy);
    let (mut left, _) = pr::entries(&found.found);
    let roots: Vec<PathBuf> = found.projects.iter().map(|p| PathBuf::from(&p.root)).collect();
    let lines = processes();
    let busy = |p: &Path| in_use(&lines, p);
    let mut freed = 0u64;
    let mut still = need;
    // What it chose and couldn't delete leaves the rest of the need to the next least recently used.
    while still > 0 {
        let chosen = pr::plan(&left, still);
        if chosen.is_empty() {
            break;
        }
        left.retain(|e| !chosen.iter().any(|c| c.id == e.id));
        for e in chosen {
            if still == 0 {
                break;
            }
            let Some(bytes) = delete(Path::new(&e.id), home, &roots, &busy) else { continue };
            freed = freed.saturating_add(bytes);
            still = still.saturating_sub(bytes);
            tracing::info!(path = %e.id, bytes, free, need, fill_bytes_per_s = rate.per_second(), horizon_s = horizon, "the reaper deleted build output marked CACHEDIR.TAG: at the fastest it measured the disk filling, the disk would fill before its next pass could finish (W24-R31)");
            crate::metrics::global().work_reaped_bytes.add(&["projects", "pressure"], bytes);
            super::warm::keep_reaped(reap::Eviction { id: e.id, bytes, reason: reap::Reason::Pressure, rule: "pressure", scope: None });
        }
    }
    let took = started.elapsed();
    state.pass = took.as_secs().saturating_add(u64::from(took.subsec_nanos() > 0));
    let free = free.saturating_add(freed);
    let still = pr::need(free, state.fastest, horizon);
    // A project no session works in now has no one to ask: what it held when it answered goes.
    state.answered.retain(|root, _| found.projects.iter().any(|p| p.root == *root));
    let asked = pr::ask(still, waiting, &found.projects, &state.answered);
    state.fruitless = (freed == 0 && asked.is_none()).then_some(free);
    asked.map(|ask| Asking { ask, free, need: still, rate, horizon })
}

/// W24-R31: the reaper of this machine's projects, on its clock. Every minute it looks: it measures
/// the free space and decides on what it has measured ([`look`]); it starts once per process.
#[allow(clippy::disallowed_methods)] // (e) the reaper looks every minute, as the warm workspaces' does
pub fn start(machine: String) {
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(move || {
        crate::rt::spawn(async move {
            let state = std::sync::Arc::new(parking_lot::Mutex::new(State::default()));
            loop {
                crate::rt::sleep(Duration::from_secs(60)).await;
                let (state, machine, now) = (std::sync::Arc::clone(&state), machine.clone(), crate::util::now_unix());
                let _ = crate::rt::blocking(move || look(&crate::os::home_dir(), &machine, &mut state.lock(), now)).await;
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(base: &Path) -> anyhow::Result<()> {
        let w = |p: &str, bytes: usize| -> anyhow::Result<()> {
            let p = base.join(p);
            if let Some(d) = p.parent() {
                std::fs::create_dir_all(d)?;
            }
            std::fs::write(p, vec![b'x'; bytes])?;
            Ok(())
        };
        // A project: its sources, its .git, a cargo target and a node build cache.
        w("Projects/app/.git/HEAD", 10)?;
        w("Projects/app/src/main.rs", 100)?;
        w("Projects/app/target/CACHEDIR.TAG", 10)?;
        w("Projects/app/target/debug/app", 50_000)?;
        w("Projects/app/web/node_modules/.cache/x/CACHEDIR.TAG", 10)?;
        w("Projects/app/web/node_modules/left-pad/index.js", 20_000)?;
        // ~/.cache: a cache marked as one, an agent's worktree beside its target, a repository;
        // and what's someone's, unmarked: a cache that keeps no tag, a patch, research.
        w(".cache/uv/CACHEDIR.TAG", 10)?;
        w(".cache/uv/archive-v0/x/wheel", 25_000)?;
        w(".cache/agent-x/wt/.git", 10)?;
        w(".cache/agent-x/wt/src/lib.rs", 100)?;
        w(".cache/agent-x/target/CACHEDIR.TAG", 10)?;
        w(".cache/agent-x/target/debug/lib.rlib", 40_000)?;
        w(".cache/clone/.git/HEAD", 10)?;
        w(".cache/clone/notes.md", 100)?;
        w(".cache/huggingface/model.bin", 30_000)?;
        w(".cache/kiln-rollback.patch", 3_000)?;
        w(".cache/hex-research/paper.md", 5_000)?;
        Ok(())
    }

    /// What a test's tree must still hold whatever the reaper did: nothing unmarked is its.
    fn never_taken(home: &Path) -> anyhow::Result<()> {
        for kept in ["Projects/app/src/main.rs", ".cache/agent-x/wt/src/lib.rs", ".cache/clone/notes.md", ".cache/huggingface/model.bin", ".cache/kiln-rollback.patch", ".cache/hex-research/paper.md"] {
            anyhow::ensure!(home.join(kept).exists(), "{kept} was deleted");
        }
        Ok(())
    }

    fn sessions(base: &Path) -> Vec<Session> {
        vec![
            Session { id: "s-app".into(), cwd: base.join("Projects/app/src").to_string_lossy().into_owned(), idle: true, queued: Vec::new() },
            Session { id: "s-wt".into(), cwd: base.join(".cache/agent-x/wt").to_string_lossy().into_owned(), idle: false, queued: Vec::new() },
            // At work in the home folder: no project.
            Session { id: "s-home".into(), cwd: base.to_string_lossy().into_owned(), idle: true, queued: Vec::new() },
        ]
    }

    #[test]
    fn w24_r31_scan_finds_only_folders_marked_cachedir_tag_never_sources_repositories_or_unmarked_cache() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let home = dir.path();
        tree(home)?;
        let s = scan(home, &sessions(home), &|_| false);
        let mut paths: Vec<String> = s.found.iter().map(|f| f.path.strip_prefix(&*home.to_string_lossy()).unwrap_or(&f.path).to_owned()).collect();
        paths.sort();
        // `~/.cache`'s unmarked folders and files (a cache without a tag, a patch, research) are never found.
        assert_eq!(paths, ["/.cache/agent-x/target", "/.cache/uv", "/Projects/app/target", "/Projects/app/web/node_modules/.cache/x"]);
        let app = s.projects.iter().find(|p| p.root.ends_with("Projects/app")).ok_or_else(|| anyhow::anyhow!("no app project: {:?}", s.projects))?;
        // What it holds that the reaper doesn't recognize: sources and node_modules, not .git or its targets.
        assert!(app.bytes >= 20_100 && app.bytes < 50_000, "{}", app.bytes);
        assert_eq!(app.agents, [pr::Agent { session: "s-app".into(), idle: true }]);
        assert!(s.projects.iter().all(|p| Path::new(&p.root) != home), "the home folder is no project");
        Ok(())
    }

    #[test]
    fn w24_r31_delete_takes_only_what_it_found_and_nothing_in_use() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let home = dir.path();
        tree(home)?;
        let roots = vec![home.join("Projects/app"), home.join(".cache/agent-x/wt")];
        let target = home.join("Projects/app/target");
        assert_eq!(delete(&target, home, &roots, &|p| p == target.as_path()), None, "in use");
        assert!(delete(&target, home, &roots, &|_| false).is_some_and(|b| b > 0));
        assert!(!target.exists() && home.join("Projects/app/src/main.rs").exists());
        // W24-R31: what isn't marked `CACHEDIR.TAG` is never deleted, even when asked to.
        for never in [
            home.join(".cache"),
            home.join(".cache/agent-x"),
            home.join(".cache/agent-x/wt"),
            home.join(".cache/clone"),
            home.join("Projects/app"),
            home.join(".cache/huggingface"),
            home.join(".cache/kiln-rollback.patch"),
            home.join(".cache/hex-research"),
        ] {
            assert_eq!(delete(&never, home, &roots, &|_| false), None, "{}", never.display());
            assert!(never.exists());
        }
        assert!(delete(&home.join(".cache/uv"), home, &roots, &|_| false).is_some());
        never_taken(home)
    }

    #[test]
    fn w24_r31_a_target_cargo_builds_into_is_in_use() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let target = dir.path().join("gate-target");
        for profile in ["debug", "aarch64-apple-darwin/release"] {
            std::fs::create_dir_all(target.join(profile))?;
            std::fs::write(target.join(profile).join(".cargo-lock"), b"")?;
        }
        assert!(!in_use(&[], &target), "no build holds its locks");
        // As cargo holds it while it builds (the target named only in its environment).
        let lock = std::fs::File::open(target.join("aarch64-apple-darwin/release/.cargo-lock"))?;
        lock.lock()?;
        assert!(in_use(&[], &target));
        lock.unlock()?;
        assert!(!in_use(&[], &target));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn w24_r31_a_read_only_module_cache_is_deleted_whole() -> anyhow::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir()?;
        let home = dir.path();
        let module = home.join(".cache/go-mod/gopkg.in/yaml.v3@v3.0.1");
        std::fs::create_dir_all(&module)?;
        std::fs::write(home.join(".cache/go-mod/CACHEDIR.TAG"), b"Signature: 8a477f597d28d172789f06886806bc55")?;
        std::fs::write(module.join("yaml.go"), b"package yaml")?;
        // As Go leaves its module cache.
        std::fs::set_permissions(&module, std::fs::Permissions::from_mode(0o555))?;
        let cache = home.join(".cache/go-mod");
        assert!(delete(&cache, home, &[], &|_| false).is_some_and(|b| b > 0));
        assert!(!cache.exists());
        Ok(())
    }

    #[test]
    fn w24_r31_an_ask_waits_while_queued_or_under_way_and_its_answer_names_the_project_once() {
        *ASKED.lock() = Some(Asked { session: "s".into(), id: "kiln-keep-reap-1".into(), root: "/p/app".into() });
        let s = |idle: bool, queued: &[&str]| vec![Session { id: "s".into(), cwd: String::new(), idle, queued: queued.iter().map(|q| (*q).to_owned()).collect() }];
        assert_eq!(waiting(&s(true, &["kiln-keep-reap-1"])), (true, None));
        assert_eq!(waiting(&s(false, &[])), (true, None));
        assert_eq!(waiting(&s(true, &[])), (false, Some("/p/app".into())));
        assert_eq!(waiting(&s(true, &[])), (false, None));
        assert!(ASKED.lock().is_none());
    }

    const GIB: u64 = 1 << 30;

    fn marked_left(home: &Path) -> (bool, bool) {
        (home.join(".cache/uv").exists(), home.join(".cache/agent-x/target").exists())
    }

    #[test]
    fn w24_r31_the_first_look_only_measures_however_little_is_free() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let home = dir.path();
        tree(home)?;
        let mut state = State::default();
        anyhow::ensure!(look_at(home, &[], false, 0, &mut state, 1_000, &Vec::<String>::new).is_none());
        anyhow::ensure!(marked_left(home) == (true, true), "nothing measured yet, nothing deleted");
        anyhow::ensure!(state.previous == Some((1_000, 0)) && state.fastest.is_none(), "it noted what it saw");
        never_taken(home)
    }

    #[test]
    fn w24_r31_deletes_marked_output_only_when_the_fastest_fill_measured_would_fill_the_disk_before_its_next_pass() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let home = dir.path();
        tree(home)?;
        let mut state = State::default();
        let none = Vec::<String>::new;
        // Filling at 1 GiB a minute with 500 GiB free: it wouldn't fill within a minute, nothing goes.
        anyhow::ensure!(look_at(home, &[], false, 501 * GIB, &mut state, 0, &none).is_none());
        anyhow::ensure!(look_at(home, &[], false, 500 * GIB, &mut state, 60, &none).is_none());
        anyhow::ensure!(marked_left(home) == (true, true), "no need measured, nothing deleted");
        anyhow::ensure!(state.fastest == pr::Rate::new(GIB, 60) && state.interval == 60, "the fill and the interval are measured");
        // A burst of 490 GiB in the next minute leaves 10 GiB: at that rate the disk fills within the horizon.
        anyhow::ensure!(look_at(home, &[], false, 10 * GIB, &mut state, 120, &none).is_none(), "no project, no one to ask");
        anyhow::ensure!(state.fastest == pr::Rate::new(490 * GIB, 60), "the fastest fill is the burst's");
        anyhow::ensure!(marked_left(home) == (false, false), "what's marked went");
        never_taken(home)
    }

    #[test]
    fn w24_r31_under_pressure_nothing_in_use_goes_and_no_pass_while_its_ask_is_unanswered() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let home = dir.path();
        tree(home)?;
        let mut state = State::default();
        let uv = home.join(".cache/uv");
        let line = vec![format!("uv pip install --cache-dir {}", uv.display())];
        let using_uv = || line.clone();
        anyhow::ensure!(look_at(home, &[], false, 100 * GIB, &mut state, 0, &using_uv).is_none());
        // Under pressure, but the agent it asked hasn't answered: no pass.
        anyhow::ensure!(look_at(home, &[], true, GIB, &mut state, 60, &using_uv).is_none());
        anyhow::ensure!(marked_left(home) == (true, true), "no pass while an ask is with its agent");
        anyhow::ensure!(look_at(home, &[], false, GIB, &mut state, 120, &using_uv).is_none());
        anyhow::ensure!(marked_left(home) == (true, false), "a process names uv's cache: it stays; the rest went");
        never_taken(home)
    }

    #[test]
    fn w24_r31_what_its_deletions_leave_needed_is_asked_of_an_idle_agent_of_the_largest_project() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let home = dir.path();
        tree(home)?;
        let mut state = State::default();
        let none = Vec::<String>::new;
        anyhow::ensure!(look_at(home, &sessions(home), false, 100 * GIB, &mut state, 0, &none).is_none());
        let a = look_at(home, &sessions(home), false, GIB, &mut state, 60, &none).ok_or_else(|| anyhow::anyhow!("no ask under pressure with an idle agent"))?;
        anyhow::ensure!(a.ask.session == "s-app" && a.ask.root.ends_with("Projects/app"), "{:?}", a.ask);
        anyhow::ensure!(a.need > 0 && a.horizon >= 60 && a.rate == pr::Rate::new(99 * GIB, 60).ok_or_else(|| anyhow::anyhow!("a rate"))?, "measured: need {} horizon {}", a.need, a.horizon);
        anyhow::ensure!(!home.join("Projects/app/target").exists() && marked_left(home) == (false, false), "its own deletions came first");
        never_taken(home)
    }

    #[test]
    fn w24_r31_a_project_whose_agent_answered_is_asked_again_only_once_it_holds_more() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let home = dir.path();
        tree(home)?;
        let mut state = State::default();
        let none = Vec::<String>::new;
        anyhow::ensure!(look_at(home, &sessions(home), false, 100 * GIB, &mut state, 0, &none).is_none());
        let a = look_at(home, &sessions(home), false, GIB, &mut state, 60, &none).ok_or_else(|| anyhow::anyhow!("no first ask"))?;
        // Its agent answered (all sources, nothing to free): what the project holds then is noted, as `look` does.
        let held = unrecognized(home, &sessions(home), Path::new(&a.ask.root)).ok_or_else(|| anyhow::anyhow!("no project at {}", a.ask.root))?;
        anyhow::ensure!(held == a.ask.bytes, "measured at the answer {held}, at the ask {}", a.ask.bytes);
        state.answered.insert(a.ask.root.clone(), held);
        // Still under pressure, and it hasn't grown: it isn't asked again.
        anyhow::ensure!(look_at(home, &sessions(home), false, GIB - 1, &mut state, 120, &none).is_none(), "asked again though it held no more than when it answered");
        // It grew past what it held then: it's asked again.
        std::fs::write(home.join("Projects/app/src/more.rs"), vec![b'x'; 10_000])?;
        let again = look_at(home, &sessions(home), false, GIB - 2, &mut state, 180, &none).ok_or_else(|| anyhow::anyhow!("not asked again after it grew"))?;
        anyhow::ensure!(again.ask.root == a.ask.root && again.ask.bytes > held, "{:?}", again.ask);
        never_taken(home)
    }
}
