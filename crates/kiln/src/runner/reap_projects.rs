//! W24-R31's reaper over this machine's own projects and `~/.cache` (Ada, 2026-11-11: "The reaper
//! needs to keep things cleaned up. Using any active session's agent to intermittently cleanup
//! specific project build files and then keeping ~/.cache clean"; asked who cleans: both).
//!
//! The projects are the folders this runner's agent sessions work in (each one's repository, or
//! its folder). In them, and in what `~/.cache` holds, the reaper recognizes build output by its
//! `CACHEDIR.TAG` (cargo's targets and every tool that keeps that convention). What `~/.cache`
//! holds that isn't a repository is a cache as a whole. On the policy every W24 cache keeps (its
//! floor, target, ceiling, keep and age rules: `kiln_ci::work::reap`), when free space is under the
//! floor, the caches past their ceiling, or the schedule due, it deletes what the plan names, none
//! a process names. If free space is still under the target then, an idle agent of the project
//! holding the most the reaper doesn't recognize is asked to clean it, one ask at a time
//! (`kiln_ci::work::reap::projects::ask`). Sources, repositories and work not committed are never
//! the reaper's: it deletes only what it recognized.

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

/// The reaper's ask still with an agent: its session and the message's id.
static ASKED: LazyLock<parking_lot::Mutex<Option<(String, String)>>> = LazyLock::new(Default::default);

/// What `~/.cache` entries are grouped under.
const CACHE_GROUP: &str = "~/.cache";

/// Whether the reaper's last ask is still with its agent: waiting in its queue, or the turn it
/// started under way. An agent gone, or idle with it taken, has answered.
fn waiting(sessions: &[Session]) -> bool {
    let mut asked = ASKED.lock();
    let still = asked.as_ref().is_some_and(|(session, id)| sessions.iter().any(|s| &s.id == session && (s.queued.iter().any(|q| q == id) || !s.idle)));
    if !still {
        *asked = None;
    }
    still
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
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
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
    /// When any of the rest was last written.
    used: i64,
    /// A repository is in it (a `.git` of a worktree or a clone): what it holds may not be
    /// committed.
    repository: bool,
}

/// What a folder at `root` holds: its build output, each folder marked `CACHEDIR.TAG`, as a
/// [`pr::Found`] of `group`; and the rest. The folders of `nested` (projects) are theirs.
fn walk(root: &Path, group: &str, nested: &[&Path], busy: &dyn Fn(&Path) -> bool, found: &mut Vec<pr::Found>) -> Rest {
    let mut rest = Rest::default();
    let Ok(top) = std::fs::symlink_metadata(root) else { return rest };
    let dev = device(&top);
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        if d.join("CACHEDIR.TAG").is_file() && d != root {
            let (bytes, used) = measure(&d);
            found.push(pr::Found { path: d.to_string_lossy().into_owned(), group: group.to_owned(), bytes, used, busy: busy(&d) });
            continue;
        }
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let Ok(m) = e.metadata() else { continue };
            let p = e.path();
            if e.file_name() == ".git" {
                rest.repository = true;
                continue;
            }
            rest.used = rest.used.max(mtime(&m));
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
        // A project's (walked above), or a folder holding one or a repository: only its build
        // output is the reaper's. Else it's a cache, whole.
        let inside: Vec<&Path> = paths.iter().copied().filter(|r| r.starts_with(&p)).collect();
        if inside.iter().any(|r| *r == p) {
            continue;
        }
        let Ok(m) = std::fs::symlink_metadata(&p) else { continue };
        if !m.is_dir() {
            found.push(pr::Found { path: p.to_string_lossy().into_owned(), group: CACHE_GROUP.to_owned(), bytes: disk_bytes(&m), used: mtime(&m), busy: busy(&p) });
            continue;
        }
        let mut tagged = Vec::new();
        let rest = walk(&p, CACHE_GROUP, &inside, busy, &mut tagged);
        if !inside.is_empty() || rest.repository {
            found.append(&mut tagged);
            continue;
        }
        let bytes = tagged.iter().map(|t| t.bytes).fold(rest.bytes.saturating_add(disk_bytes(&m)), u64::saturating_add);
        let used = tagged.iter().map(|t| t.used).fold(rest.used.max(mtime(&m)), i64::max);
        found.push(pr::Found { path: p.to_string_lossy().into_owned(), group: CACHE_GROUP.to_owned(), bytes, used, busy: busy(&p) });
    }
    Scan { found, projects }
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

/// Deletes what the reaper found at `path`, unless it has become what it never takes since: a
/// project, a folder holding one, a repository, `~/.cache` itself, or in use. What it freed.
fn delete(path: &Path, home: &Path, roots: &[PathBuf], busy: &dyn Fn(&Path) -> bool) -> Option<u64> {
    if path == home.join(".cache") || roots.iter().any(|r| r.starts_with(path)) || path.join(".git").exists() || busy(path) {
        return None;
    }
    let (bytes, _) = measure(path);
    let m = std::fs::symlink_metadata(path).ok()?;
    let gone = if m.is_dir() {
        // Go's module cache makes its folders read-only: they're made writable again to be emptied.
        std::fs::remove_dir_all(path).or_else(|e| if e.kind() == std::io::ErrorKind::PermissionDenied { writable(path).and_then(|()| std::fs::remove_dir_all(path)) } else { Err(e) })
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
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                stack.push(e.path());
            }
        }
    }
    Ok(())
}

/// The reaper's state between its looks: when it last reaped on schedule, what the caches held at
/// its last scan, and the free space a scan under the floor last found nothing to do at.
#[derive(Default)]
pub struct State {
    pub last: Option<i64>,
    pub held: u64,
    pub fruitless: Option<u64>,
}

/// One look of the reaper at this machine's projects (`now`: unix seconds). Blocking.
pub fn look(home: &Path, machine: &str, layers: &[(reap::Scope, reap::Policy)], state: &mut State, now: i64) {
    let Some((size, free)) = crate::os::disk_space(home) else { return };
    let env = super::warm::env_policy();
    let mut all: Vec<(reap::Scope, &reap::Policy)> = layers.iter().map(|(s, p)| (*s, p)).collect();
    all.insert(0, (reap::Scope::Runner, &env));
    let policy = reap::resolve(size, &all);
    let sessions = SESSIONS.get().map(|f| f()).unwrap_or_default();
    let waiting = waiting(&sessions);
    let Some(trigger) = reap::due(&policy, free, state.held, 0, state.last, now) else {
        state.fruitless = None;
        return;
    };
    // Under the floor, a scan that found nothing to do isn't made again until free space falls
    // further or the agent asked has answered.
    if trigger == reap::Trigger::Pressure && (waiting || state.fruitless.is_some_and(|f| free >= f)) {
        return;
    }
    let lines = processes();
    let busy = |p: &Path| in_use(&lines, p);
    let found = scan(home, &sessions, &busy);
    let (entries, held) = pr::entries(&found.found);
    let roots: Vec<PathBuf> = found.projects.iter().map(|p| PathBuf::from(&p.root)).collect();
    let mut freed = 0u64;
    let lines = processes();
    let busy = |p: &Path| in_use(&lines, p);
    for e in reap::plan(&policy, &entries, free, held, now) {
        let Some(bytes) = delete(Path::new(&e.id), home, &roots, &busy) else { continue };
        freed = freed.saturating_add(bytes);
        let reason = match e.reason {
            reap::Reason::MaxAge => "max_age",
            reap::Reason::Target => "target",
            reap::Reason::Floor => "floor",
        };
        tracing::info!(path = %e.id, bytes, reason, rule = e.rule, scope = ?e.scope, trigger = ?trigger, "the reaper deleted build output");
        crate::metrics::global().work_reaped_bytes.add(&["projects", reason], bytes);
        super::warm::keep_reaped(reap::Eviction { bytes, ..e });
    }
    state.held = held.saturating_sub(freed);
    if trigger == reap::Trigger::Schedule {
        state.last = Some(now);
    }
    let free = free.saturating_add(freed);
    let asked = pr::ask(&policy, free, waiting, &found.projects);
    state.fruitless = (trigger == reap::Trigger::Pressure && freed == 0 && asked.is_none()).then_some(free);
    let Some(a) = asked else { return };
    let id = format!("{}reap-{now}", crate::harness::core::KEEP_ID);
    let text = pr::ask_text(machine, free, policy.target, &a);
    if TELL.get().is_some_and(|tell| tell(&a.session, id.clone(), text)) {
        tracing::info!(session = %a.session, project = %a.root, bytes = a.bytes, free, "the reaper asked an agent to clean its project");
        *ASKED.lock() = Some((a.session, id));
    }
}

/// W24-R31: the reaper of this machine's projects, on its clock. Every minute it checks whether
/// free space, the ceiling or the schedule calls for a reap; it starts once per process.
#[allow(clippy::disallowed_methods)] // (e) the reaper looks every minute, as the warm workspaces' does
pub fn start(machine: String) {
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(move || {
        crate::rt::spawn(async move {
            let state = std::sync::Arc::new(parking_lot::Mutex::new(State::default()));
            loop {
                crate::rt::sleep(Duration::from_secs(60)).await;
                let (state, machine, layers, now) = (std::sync::Arc::clone(&state), machine.clone(), super::warm::server_policies(), crate::util::now_unix());
                let _ = crate::rt::blocking(move || look(&crate::os::home_dir(), &machine, &layers, &mut state.lock(), now)).await;
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
        // ~/.cache: a cache, an agent's worktree beside its target, a repository.
        w(".cache/huggingface/model.bin", 30_000)?;
        w(".cache/agent-x/wt/.git", 10)?;
        w(".cache/agent-x/wt/src/lib.rs", 100)?;
        w(".cache/agent-x/target/CACHEDIR.TAG", 10)?;
        w(".cache/agent-x/target/debug/lib.rlib", 40_000)?;
        w(".cache/clone/.git/HEAD", 10)?;
        w(".cache/clone/notes.md", 100)?;
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
    fn w24_r31_scan_finds_build_output_and_caches_never_sources_or_repositories() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let home = dir.path();
        tree(home)?;
        let s = scan(home, &sessions(home), &|_| false);
        let mut paths: Vec<String> = s.found.iter().map(|f| f.path.strip_prefix(&*home.to_string_lossy()).unwrap_or(&f.path).to_owned()).collect();
        paths.sort();
        assert_eq!(paths, ["/.cache/agent-x/target", "/.cache/huggingface", "/Projects/app/target", "/Projects/app/web/node_modules/.cache/x"]);
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
        for never in [home.join(".cache"), home.join(".cache/agent-x"), home.join(".cache/agent-x/wt"), home.join(".cache/clone"), home.join("Projects/app")] {
            assert_eq!(delete(&never, home, &roots, &|_| false), None, "{}", never.display());
            assert!(never.exists());
        }
        assert!(delete(&home.join(".cache/huggingface"), home, &roots, &|_| false).is_some());
        Ok(())
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
        std::fs::write(module.join("yaml.go"), b"package yaml")?;
        // As Go leaves its module cache.
        std::fs::set_permissions(&module, std::fs::Permissions::from_mode(0o555))?;
        let cache = home.join(".cache/go-mod");
        assert!(delete(&cache, home, &[], &|_| false).is_some_and(|b| b > 0));
        assert!(!cache.exists());
        Ok(())
    }

    #[test]
    fn w24_r31_an_ask_waits_while_queued_or_under_way() {
        *ASKED.lock() = Some(("s".into(), "kiln-keep-reap-1".into()));
        let s = |idle: bool, queued: &[&str]| vec![Session { id: "s".into(), cwd: String::new(), idle, queued: queued.iter().map(|q| (*q).to_owned()).collect() }];
        assert!(waiting(&s(true, &["kiln-keep-reap-1"])));
        assert!(waiting(&s(false, &[])));
        assert!(!waiting(&s(true, &[])));
        assert!(ASKED.lock().is_none());
    }
}
