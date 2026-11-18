//! Agents' workspaces of their own, on Slates (docs/agentic.md, W): an agent's folder is a Slates
//! volume laid over the real one, mounted where the machine's runner keeps workspaces.
//!
//! An overlay volume reads every file the agent hasn't touched from the folder itself, as it is
//! now, and keeps what the agent changes (its edits, its build's output, git's own writes) in
//! memory, in the volume alone: the folder, and every other agent's volume, never see it. What
//! leaves it goes as a change request (`super::change`) or, written back under a person's grant,
//! to the folder itself.
//!
//! The machine keeps one Slates instance (`kiln`) for all its runner processes. Its anchor is
//! started apart from the runner (its own systemd user unit on Linux, its own process group
//! elsewhere), so restarting or upgrading the runner never ends it: a volume lives in the
//! anchor's memory, and goes when it does.
//!
//! Slates as built today (c8d6adb4) can't clone a snapshot of an overlay volume
//! (`ContentUnavailable`), and a pinned base makes every pinned file part of a landing's plan, so
//! each agent gets an overlay of its own, unpinned: what it hasn't touched is the folder as it is
//! now. Files it has touched are witnessed when it first writes them, and a later change on disk
//! to one of them is reported as drift.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The Slates instance kiln keeps on a machine.
pub const INSTANCE: &str = "kiln";
/// How long one Slates verb may take.
const VERB_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a new anchor has to answer.
const UP_WAIT: Duration = Duration::from_secs(30);
/// Shards the instance runs (each is a core's worth of work when busy): agents' folders need few.
const SHARDS: u32 = 2;
/// The largest a workspace may grow to, at most (what it changes is held in memory): a quarter of
/// the machine's memory, up to this.
const MAX_VOLUME_GIB: u64 = 64;

struct Here {
    /// Where workspaces are mounted (`<state>/workspaces/<name>`).
    root: PathBuf,
    /// Where kiln keeps a Slates binary it installed (`<state>/bin`).
    bin_dir: PathBuf,
    /// The anchor's log, where it says how a person's grant reaches it (macOS, Windows).
    log: PathBuf,
}

static HERE: OnceLock<Here> = OnceLock::new();

/// Where this process keeps its machine's workspaces: under `state_dir` (a runner's, shared by
/// every runner process of the machine; the server's data directory for its own agents).
pub fn init(state_dir: &Path) {
    let _ = HERE.set(Here { root: state_dir.join("workspaces"), bin_dir: state_dir.join("bin"), log: state_dir.join("slates-anchor.log") });
}

fn here() -> anyhow::Result<&'static Here> {
    HERE.get().ok_or_else(|| anyhow::anyhow!("this process keeps no workspaces"))
}

/// What a test points this process's workspaces at instead: a Slates binary, an instance of its
/// own, its anchor started here. Set once, before anything here runs.
#[derive(Debug, Clone)]
pub struct Override {
    pub binary: PathBuf,
    pub instance: String,
}

static OVERRIDE: OnceLock<Override> = OnceLock::new();

/// Points this process's workspaces at `o` (a test's own Slates): false when something already did.
pub fn set_override(o: Override) -> bool {
    OVERRIDE.set(o).is_ok()
}

/// The instance kiln uses (`KILN_SLATES_INSTANCE` names another, for labs).
fn instance() -> String {
    if let Some(o) = OVERRIDE.get() {
        return o.instance.clone();
    }
    std::env::var("KILN_SLATES_INSTANCE").ok().filter(|i| !i.trim().is_empty()).unwrap_or_else(|| INSTANCE.to_owned())
}

/// The Slates binary: `KILN_SLATES`, else the one kiln installed, else `slates` on `PATH`.
pub fn binary() -> Option<PathBuf> {
    if let Some(o) = OVERRIDE.get() {
        return Some(o.binary.clone());
    }
    if let Some(p) = std::env::var_os("KILN_SLATES").map(PathBuf::from).filter(|p| p.is_file()) {
        return Some(p);
    }
    if let Some(h) = HERE.get() {
        let mine = h.bin_dir.join(if cfg!(windows) { "slates.exe" } else { "slates" });
        if mine.is_file() {
            return Some(mine);
        }
    }
    crate::os::which("slates")
}

/// How a volume is mounted here, when it can be: `fuse` (Linux, through `fusermount3`, no
/// privilege), `nfs` (macOS, on loopback) or `winfsp` (Windows, at a drive letter: `super::place`).
pub fn mount_kind() -> Option<&'static str> {
    super::place::kind()
}

/// What a machine says of its workspaces, in its facts.
pub type Facts = kiln_ci::facts::Workspaces;

/// This machine's workspace facts, looked at now.
pub async fn facts() -> Facts {
    let Some(bin) = binary() else {
        return Facts { why_not: Some("Slates isn't installed on this machine".to_owned()), ..Facts::default() };
    };
    let slates = Some(bin.to_string_lossy().into_owned());
    let Some(mount) = mount_kind() else {
        return Facts { slates, why_not: Some(super::place::why_not().to_owned()), ..Facts::default() };
    };
    if HERE.get().is_none() {
        return Facts { slates, mount: Some(mount.to_owned()), why_not: Some("this kiln keeps no workspaces".to_owned()), ..Facts::default() };
    }
    Facts { available: true, slates, mount: Some(mount.to_owned()), running: up().await, why_not: None }
}

/// Runs `slates --instance <kiln's> <args> --json`: its JSON answer, or what it refused.
async fn verb(args: &[&str]) -> anyhow::Result<Value> {
    let bin = binary().ok_or_else(|| anyhow::anyhow!("Slates isn't installed on this machine"))?;
    let mut c = Command::new(&bin);
    c.arg("--instance").arg(instance()).args(args).arg("--json");
    let out = crate::proc::output(c, VERB_TIMEOUT).await?;
    let text = String::from_utf8_lossy(&out.stdout);
    if out.status.success() {
        return Ok(text.lines().rev().find_map(|l| serde_json::from_str(l).ok()).unwrap_or(Value::Null));
    }
    let err = String::from_utf8_lossy(&out.stderr);
    let said = err.lines().chain(text.lines()).find_map(|l| serde_json::from_str::<Value>(l).ok()).and_then(|v| v.pointer("/error/message").and_then(Value::as_str).map(str::to_owned));
    anyhow::bail!("slates {}: {}", args.first().copied().unwrap_or_default(), said.unwrap_or_else(|| err.trim().chars().take(400).collect()))
}

/// The instance answers.
async fn up() -> bool {
    let Some(bin) = binary() else { return false };
    let mut c = Command::new(bin);
    c.arg("--instance").arg(instance()).args(["status", "--json"]);
    crate::proc::output(c, Duration::from_secs(5)).await.is_ok_and(|o| o.status.success())
}

/// Starts the instance's anchor apart from this process: a systemd user unit of its own on Linux
/// where there is one (stopping or restarting the runner's unit never takes it along), its own
/// process group elsewhere.
async fn start_anchor(bin: &Path, log: &Path) -> anyhow::Result<()> {
    let shards = SHARDS.to_string();
    let args = ["--instance", &instance(), "anchor", "--shards", &shards];
    // Tests and labs start theirs here (`KILN_SLATES_DIRECT`): no unit outlives them.
    let direct = OVERRIDE.get().is_some() || std::env::var_os("KILN_SLATES_DIRECT").is_some();
    if !direct && cfg!(target_os = "linux") && std::env::var_os("XDG_RUNTIME_DIR").is_some() && crate::os::which("systemd-run").is_some() {
        let unit = format!("{}-slates", instance());
        let mut reset = Command::new("systemctl");
        reset.args(["--user", "reset-failed", &format!("{unit}.service")]);
        let _ = crate::proc::output(reset, Duration::from_secs(10)).await;
        let mut c = Command::new("systemd-run");
        c.args(["--user", "--quiet", "--collect", &format!("--unit={unit}"), "--"]).arg(bin).args(args);
        match crate::proc::output(c, Duration::from_secs(20)).await {
            Ok(o) if o.status.success() => return Ok(()),
            Ok(o) => tracing::info!(said = %String::from_utf8_lossy(&o.stderr).trim(), "Slates' anchor didn't start as a unit of its own; starting it here"),
            Err(e) => tracing::info!(error = %format!("{e:#}"), "Slates' anchor didn't start as a unit of its own; starting it here"),
        }
    }
    if let Some(dir) = log.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let out = std::fs::OpenOptions::new().create(true).append(true).open(log)?;
    let err = out.try_clone()?;
    let mut c = Command::new(bin);
    c.args(args).stdin(std::process::Stdio::null()).stdout(out).stderr(err);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        c.process_group(0);
    }
    let child = c.spawn()?;
    // Never waited on: it outlives this process. Its pid is kept, so a test stops the one anchor it started alone.
    let _ = STARTED.set(child.id());
    drop(child);
    Ok(())
}

/// The pid of the anchor this process started apart (its own process group: [`start_anchor`]), the first one.
static STARTED: OnceLock<u32> = OnceLock::new();

/// The pid of the anchor this process started itself, when it started one apart: what a test that started its own
/// instance stops at its end, and nothing else.
pub fn started_anchor() -> Option<u32> {
    STARTED.get().copied()
}

/// One start of the anchor at a time: agents starting together find it started by the first.
fn starting() -> &'static futures::lock::Mutex<()> {
    static STARTING: OnceLock<futures::lock::Mutex<()>> = OnceLock::new();
    STARTING.get_or_init(|| futures::lock::Mutex::new(()))
}

/// The instance answers, its anchor started (and its one-node group made) when it wasn't running.
#[allow(clippy::disallowed_methods)] // (c) polls slates' start; its own word would tell
pub async fn ensure_up() -> anyhow::Result<()> {
    if up().await {
        return Ok(());
    }
    let _one = starting().lock().await;
    if up().await {
        return Ok(());
    }
    let h = here()?;
    let bin = binary().ok_or_else(|| anyhow::anyhow!("Slates isn't installed on this machine"))?;
    start_anchor(&bin, &h.log).await?;
    let until = std::time::Instant::now() + UP_WAIT;
    while !up().await {
        if std::time::Instant::now() > until {
            anyhow::bail!("Slates' anchor didn't answer within {}s (its log: {})", UP_WAIT.as_secs(), h.log.display());
        }
        crate::rt::sleep(Duration::from_millis(200)).await;
    }
    // A new instance holds no volumes until its group exists; an existing one says so and is left as it is.
    if let Err(e) = verb(&["bootstrap", "root"]).await {
        tracing::debug!(error = %format!("{e:#}"), "Slates' group was there already");
    }
    Ok(())
}

/// The most a workspace may hold, in Slates' units: a quarter of this machine's memory, at most
/// [`MAX_VOLUME_GIB`].
fn volume_max() -> String {
    let mut sys = sysinfo::System::new();
    sys.refresh_memory();
    let quarter = (sys.total_memory() / 4) >> 30;
    format!("{}GiB", quarter.clamp(1, MAX_VOLUME_GIB))
}

/// A workspace as it stands: the volume, its name and where it's mounted.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Made {
    pub volume: String,
    pub name: String,
    pub mount: String,
}

/// A new workspace `name` laid over `over`, mounted where its machine mounts workspaces (`super::place`): its folder
/// under the workspaces folder, or on Windows the highest drive letter free, the next one free when another start took
/// that one meanwhile.
pub async fn make(name: &str, over: &Path) -> anyhow::Result<Made> {
    ensure_up().await?;
    let over_s = over.to_string_lossy().into_owned();
    let max = volume_max();
    let made = verb(&["volume", "create", name, "--dynamic", &max, "--base", &over_s]).await?;
    let volume = made.get("id").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("Slates made no volume: {made}"))?.to_owned();
    let mut failed = anyhow::anyhow!(super::place::NO_POINT);
    for point in super::place::points(&here()?.root, name) {
        let m = Made { volume: volume.clone(), name: name.to_owned(), mount: point.to_string_lossy().into_owned() };
        // Mounted there anew, never taken as mounted already: a drive letter another start took meanwhile holds its
        // volume, not this one, and Slates refuses a second mount at it.
        match mount_at(&m).await {
            Ok(()) => return Ok(m),
            // Taken meanwhile (a drive letter another start mounted first): the next one.
            Err(e) if super::place::taken(&point) => failed = e,
            Err(e) => {
                failed = e;
                break;
            }
        }
    }
    let _ = verb(&["volume", "destroy", &volume]).await;
    Err(failed)
}

/// Whether a volume is mounted at `dir` (`super::place::is_mount`).
fn is_mount(dir: &Path) -> bool {
    super::place::is_mount(dir)
}

/// Workspace `m` is mounted where it says (again, after the machine's mount went away while its
/// volume stayed).
pub async fn mounted(m: &Made) -> anyhow::Result<()> {
    let dir = PathBuf::from(&m.mount);
    if is_mount(&dir) {
        return Ok(());
    }
    mount_at(m).await
}

/// Mounts workspace `m` at its point, which holds nothing of it now.
async fn mount_at(m: &Made) -> anyhow::Result<()> {
    let dir = PathBuf::from(&m.mount);
    super::place::ready(&dir)?;
    let mount = m.mount.clone();
    verb_plain(&["mount", &m.volume, &mount]).await?;
    anyhow::ensure!(is_mount(&dir), "Slates said it mounted {} at {mount}, but nothing is mounted there", m.name);
    Ok(())
}

/// A verb whose answer is text (`mount`, `unmount`).
async fn verb_plain(args: &[&str]) -> anyhow::Result<String> {
    let bin = binary().ok_or_else(|| anyhow::anyhow!("Slates isn't installed on this machine"))?;
    let mut c = Command::new(bin);
    c.arg("--instance").arg(instance()).args(args);
    let out = crate::proc::output(c, VERB_TIMEOUT).await?;
    let said = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    anyhow::ensure!(out.status.success(), "slates {}: {}", args.first().copied().unwrap_or_default(), said.trim());
    Ok(said)
}

/// Workspace `m` gone: unmounted, its volume destroyed and its mount point removed. What it held
/// is lost. A mount still in use (something the agent left running works there) is let go lazily
/// on Linux, and its volume destroyed all the same: nothing is kept for an agent that has ended.
pub async fn remove(m: &Made) -> anyhow::Result<()> {
    let dir = PathBuf::from(&m.mount);
    if is_mount(&dir)
        && let Err(e) = verb_plain(&["unmount", &m.mount]).await
    {
        tracing::info!(mount = %m.mount, error = %format!("{e:#}"), "an ended agent's workspace is still in use: let go lazily");
        if cfg!(target_os = "linux") {
            let mut c = Command::new("fusermount3");
            c.args(["-u", "-z"]).arg(&dir);
            let _ = crate::proc::output(c, VERB_TIMEOUT).await;
        }
    }
    verb(&["volume", "destroy", &m.volume]).await?;
    super::place::clear(&dir);
    Ok(())
}

/// The files of the folder below workspace `m` that changed on disk after the agent changed them
/// in its workspace (each as Slates says it: `/README.md (Modified)`).
pub async fn drift(m: &Made) -> anyhow::Result<Vec<String>> {
    let st = verb(&["status", &m.volume, "--drift"]).await?;
    Ok(st.get("drifted").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect())
}

/// What a landing of workspace `m` leaves out, each a prefix of its volume: what git ignores there (its build output
/// and dependencies, which kiln never writes into a person's folder), then git's own directory. `/.git` goes last: a
/// Slates whose command line keeps one `--exclude` (before 2026-11-18) still leaves git's own directory out, and
/// [`writable`] refuses the rest. A folder git doesn't know leaves out `/.git` alone.
pub async fn excludes(m: &Made) -> Vec<String> {
    let mut out = match git_ignored(m).await {
        Ok(ignored) => ignored,
        Err(e) => {
            tracing::debug!(error = %format!("{e:#}"), "what git ignores in a workspace couldn't be read: only .git is left out");
            Vec::new()
        }
    };
    out.push("/.git".to_owned());
    out
}

fn land_args(m: &Made, over: &str, excludes: &[String]) -> Vec<String> {
    let mut args = vec!["land".to_owned(), m.volume.clone(), over.to_owned()];
    for prefix in excludes {
        args.push("--exclude".to_owned());
        args.push(prefix.clone());
    }
    args
}

/// What git ignores in workspace `m` ([`ignored_prefixes`]).
async fn git_ignored(m: &Made) -> anyhow::Result<Vec<String>> {
    let mut c = Command::new("git");
    c.arg("-C").arg(super::place::top(&m.mount)).args(["status", "--porcelain=v1", "-z", "--ignored=traditional", "--untracked-files=normal"]).env("GIT_TERMINAL_PROMPT", "0");
    let o = crate::proc::output(c, VERB_TIMEOUT).await?;
    anyhow::ensure!(o.status.success(), "git status: {}", String::from_utf8_lossy(&o.stderr).trim());
    Ok(ignored_prefixes(&String::from_utf8_lossy(&o.stdout)))
}

/// The paths `git status --porcelain=v1 -z --ignored=traditional` says it ignores (`!! target/`: a directory git
/// ignores whole is named once), each a prefix of the volume (`/target`); a rename's or a copy's source, a field of its
/// own, is no path of its own.
fn ignored_prefixes(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut fields = text.split('\0').filter(|f| !f.is_empty());
    while let Some(record) = fields.next() {
        if let Some(path) = record.strip_prefix("!! ") {
            let path = path.trim_end_matches('/');
            if !path.is_empty() {
                out.push(format!("/{path}"));
            }
        } else if names_a_source(record) {
            let _ = fields.next();
        }
    }
    out
}

/// Whether a `git status --porcelain=v1 -z` record is a rename's or a copy's, whose source follows as a field of its
/// own: its status says so in either column, the index's (`R  new`) or the worktree's (` R new`, a file added with
/// `git add -N` in place of one removed).
fn names_a_source(record: &str) -> bool {
    record.chars().take(2).any(|status| matches!(status, 'R' | 'C'))
}

/// How many paths git sees changed in workspace `m`: tracked files changed or deleted, and new
/// files git doesn't ignore, each one (`--untracked-files=all`).
async fn git_changed(m: &Made) -> anyhow::Result<usize> {
    let mut c = Command::new("git");
    c.arg("-C").arg(super::place::top(&m.mount)).args(["status", "--porcelain=v1", "-z", "--untracked-files=all"]).env("GIT_TERMINAL_PROMPT", "0");
    let o = crate::proc::output(c, VERB_TIMEOUT).await?;
    anyhow::ensure!(o.status.success(), "git status: {}", String::from_utf8_lossy(&o.stderr).trim());
    Ok(count_changed(&String::from_utf8_lossy(&o.stdout)))
}

/// The paths of `git status --porcelain=v1 -z`'s records: a rename or copy carries its source as
/// one more field, which is no path of its own.
fn count_changed(text: &str) -> usize {
    let mut n = 0usize;
    let mut fields = text.split('\0').filter(|f| !f.is_empty());
    while let Some(record) = fields.next() {
        n = n.saturating_add(1);
        if names_a_source(record) {
            let _ = fields.next();
        }
    }
    n
}

/// The files a landing's plan would write, create or delete (not counting directories).
fn planned_files(plan: &Value) -> usize {
    plan.pointer("/summary/by_action")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|a| !matches!(a.get("action").and_then(Value::as_str), Some("mkdir" | "rmdir")))
        .filter_map(|a| a.get("count").and_then(Value::as_u64))
        .map(|n| usize::try_from(n).unwrap_or(usize::MAX))
        .fold(0usize, usize::saturating_add)
}

/// Whether `plan` writes only what git sees changed in workspace `m`: with what git ignores and `.git` left out
/// ([`excludes`]), anything more is something git ignores that this machine's Slates wrote anyway (one whose command
/// line keeps one `--exclude`), which kiln won't write into a person's folder.
pub async fn writable(m: &Made, plan: &Value) -> Result<(), String> {
    let planned = planned_files(plan);
    let changed = git_changed(m).await.map_err(|e| format!("{e:#}"))?;
    if planned <= changed {
        return Ok(());
    }
    Err(format!(
        "it would also write {} file{} git ignores (build output, dependencies) into the folder: this machine's Slates leaves out only .git (an older one: update it), so put the work up for review instead",
        planned.saturating_sub(changed),
        if planned.saturating_sub(changed) == 1 { "" } else { "s" }
    ))
}

/// What writing workspace `m` back to `over` would do: Slates' presentation (`landing`,
/// `manifest`, `summary`, `conflicts`, `grant_required`), or how it went if nothing needed a grant;
/// and what it leaves out ([`excludes`]).
pub async fn plan_landing(m: &Made, over: &str) -> anyhow::Result<(Value, Vec<String>)> {
    let excludes = excludes(m).await;
    let args = land_args(m, over, &excludes);
    let plan = verb(&args.iter().map(String::as_str).collect::<Vec<_>>()).await?;
    Ok((plan, excludes))
}

/// How a person's grant reaches this machine's Slates: the variables its anchor said to export
/// (macOS and Windows, where the anchor's segment has a name). Linux has none yet: there the
/// segment is a descriptor only the anchor's own children hold (Slates' GAP-A9-10).
fn issuer_env() -> anyhow::Result<Vec<(String, String)>> {
    if cfg!(target_os = "linux") {
        anyhow::bail!("Slates on Linux can't take a person's go-ahead from kiln yet (Slates' GAP-A9-10): put the work up for review instead, or approve it with `slates grant` in the session that started Slates");
    }
    let log = std::fs::read_to_string(&here()?.log).unwrap_or_default();
    let line = log.lines().rev().find_map(|l| l.split_once("issuer surface: export ").map(|(_, rest)| rest.to_owned())).ok_or_else(|| anyhow::anyhow!("Slates' anchor here wasn't started by kiln, so kiln can't pass on a person's approval to it"))?;
    Ok(line.split_whitespace().filter_map(|kv| kv.split_once('=')).map(|(k, v)| (k.to_owned(), v.to_owned())).collect())
}

/// Whether this machine's Slates takes a person's go-ahead from kiln, and why not.
pub fn can_take_grants() -> anyhow::Result<()> {
    issuer_env().map(|_| ())
}

/// What came of writing a workspace back: written (Slates' outcome), or not, because the plan is no
/// longer the one the person saw (the new plan, to show them, and what it leaves out).
#[derive(Debug)]
pub enum Landed {
    Written(Value),
    Changed(Value, Vec<String>),
}

/// Writes workspace `m` back to `over` exactly as the plan whose manifest is `manifest` (the one
/// the person saw) says. Slates holds a plan only while the command that made it runs, so the plan
/// is made again here: when its manifest is the one the person saw, the grant for it is given at
/// once and the landing made under it; when anything changed, nothing is written and the new plan
/// comes back. Either way the grant binds exactly what the person said yes to.
pub async fn land(m: &Made, over: &str, manifest: &str) -> anyhow::Result<Landed> {
    let env = issuer_env()?;
    let (plan, excludes) = plan_landing(m, over).await?;
    if plan.get("grant_required").and_then(Value::as_bool) != Some(true) {
        // Nothing waits on a grant (nothing to write, or it landed as it was).
        return Ok(Landed::Written(plan));
    }
    let (Some(landing), Some(now)) = (plan.get("landing").and_then(Value::as_u64), plan.get("manifest").and_then(Value::as_str)) else {
        anyhow::bail!("Slates' plan named no landing: {plan}");
    };
    if now != manifest {
        return Ok(Landed::Changed(plan, excludes));
    }
    writable(m, &plan).await.map_err(anyhow::Error::msg)?;
    let bin = binary().ok_or_else(|| anyhow::anyhow!("Slates isn't installed on this machine"))?;
    let mut c = Command::new(&bin);
    c.arg("--instance").arg(instance()).args(["grant", &landing.to_string(), manifest, "--json"]);
    for (k, v) in &env {
        c.env(k, v);
    }
    let out = crate::proc::output(c, VERB_TIMEOUT).await?;
    let said = String::from_utf8_lossy(&out.stdout).into_owned();
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let why = err.lines().chain(said.lines()).find_map(|l| serde_json::from_str::<Value>(l).ok()).and_then(|v| v.pointer("/error/message").and_then(Value::as_str).map(str::to_owned));
        anyhow::bail!("Slates refused the approval: {}", why.unwrap_or_else(|| err.trim().to_owned()));
    }
    // `{"grant": N}` (Slates' `grant --json`).
    let grant = said
        .lines()
        .rev()
        .find_map(|l| serde_json::from_str::<Value>(l).ok())
        .and_then(|v| v.get("grant").or_else(|| v.get("id")).and_then(Value::as_u64))
        .ok_or_else(|| anyhow::anyhow!("Slates gave no grant: {said}"))?;
    // Landed under its grant exactly as planned: the same prefixes left out, so the same manifest.
    let mut args = land_args(m, over, &excludes);
    args.push("--grant".to_owned());
    args.push(grant.to_string());
    Ok(Landed::Written(verb(&args.iter().map(String::as_str).collect::<Vec<_>>()).await?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_workspace_never_holds_more_than_its_share_of_memory() {
        let max = volume_max();
        let gib: u64 = max.trim_end_matches("GiB").parse().unwrap_or(0);
        assert!((1..=MAX_VOLUME_GIB).contains(&gib), "{max}");
    }

    #[test]
    fn a_landing_is_weighed_against_what_git_sees_changed() {
        // ` M a`, `?? n`, a rename (its source a field of its own), a deletion.
        assert_eq!(count_changed(" M src/a.rs\0?? src/n.rs\0R  new.rs\0old.rs\0 D gone.rs\0"), 4);
        assert_eq!(count_changed(" R new.txt\0old.txt\0"), 1, "a rename git sees in the worktree (git add -N), its source no path of its own");
        assert_eq!(count_changed(""), 0);
        let plan = serde_json::json!({"summary": {"by_action": [{"action": "replace", "count": 2}, {"action": "create", "count": 3}, {"action": "mkdir", "count": 4}]}});
        assert_eq!(planned_files(&plan), 5, "directories don't count");
        assert_eq!(planned_files(&serde_json::json!({})), 0);
    }

    /// Kiln goal 01a1ec34599974448aed2672bafaf9a2 (W6): a landing leaves out each path git ignores, once per directory
    /// git ignores whole, a rename's source no path of its own; and `.git` last, which an older Slates keeps alone.
    #[test]
    fn a_landing_leaves_out_what_git_ignores_and_git_itself_last() {
        let said = "!! target/\0 M src/a.rs\0R  new.rs\0old.rs\0!! src/gen.o\0?? src/n.rs\0 R moved.rs\0!! was.rs\0!! node_modules/\0";
        assert_eq!(ignored_prefixes(said), ["/target", "/src/gen.o", "/node_modules"]);
        assert!(ignored_prefixes("").is_empty());
        let m = Made { volume: "v".into(), name: "kiln-a".into(), mount: "/w/kiln-a".into() };
        let args = land_args(&m, "/src/kiln", &["/target".to_owned(), "/.git".to_owned()]);
        assert_eq!(args, ["land", "v", "/src/kiln", "--exclude", "/target", "--exclude", "/.git"]);
    }
}

// The bare-metal engine's workspaces end to end, with a real Slates: `super::bare_metal_tests`.
