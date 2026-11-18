//! Agents' workspaces of their own, on Slates (docs/agentic.md, W): an agent's folder is a Slates
//! volume laid over the real one, mounted where the machine's runner keeps workspaces.
//!
//! An overlay volume reads every file the agent hasn't touched from the folder itself, as it is
//! now, and keeps what the agent changes (its edits, its build's output, git's own writes) in
//! memory, in the volume alone: the folder, and every other agent's volume, never see it. What
//! leaves it goes as a change request (`super::change`) or, written back under a person's grant,
//! to the folder itself.
//!
//! kiln vendors Slates (`vendor/slates`) and drives it through its library, `slates-api`, alone,
//! never through its command line (the person, 2026-11-18; Slates' A-278). kiln carries no Slates
//! binary: its own executable is Slates' anchor and daemon (`kiln slates-role --instance <name>
//! anchor`, [`ROLE_ARG`]), handed to Slates' own main before kiln's runtime starts
//! (`entry::main`, [`role_main`]), so the daemon a runner talks to is the same code as the client
//! it links. Each call is one of the library's synchronous round trips, run on kiln's blocking
//! pool on a client of its own ([`call`]): microseconds for most verbs, longer for a mount or a
//! landing, which wait while the daemon lives.
//!
//! The machine keeps one Slates instance (`kiln`) for all its runner processes. Its anchor is
//! started apart from the runner (its own systemd user unit on Linux, its own process group
//! elsewhere), so restarting or upgrading the runner never ends it: a volume lives in the
//! anchor's memory, and goes when it does.
//!
//! A landing is granted under the anchor's issuer authority, which Slates hands the anchor's user
//! where its segment has a name (macOS, Windows: the anchor prints the handoff, [`can_take_grants`]),
//! as the library's [`slates_api::Issuer`]; on Linux the segment is a descriptor only the anchor's
//! own children hold (Slates' GAP-A9-10), so the plan says so. Under the person's rule of
//! 2026-11-18 the go-ahead becomes the launcher's: the permission mode kiln gives each agent sets
//! what a landing may do, with no proof at all, through Slates' typed landing contract (its
//! A-285 and A-278's next pieces), which this module moves onto as it lands.
//!
//! Slates as built (c8d6adb4) couldn't clone a snapshot of an overlay volume
//! (`ContentUnavailable`), and a pinned base makes every pinned file part of a landing's plan, so
//! each agent gets an overlay of its own, unpinned: what it hasn't touched is the folder as it is
//! now. Files it has touched are witnessed when it first writes them, and a later change on disk
//! to one of them is reported as drift.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use slates_api::{Approved, Filter, GrantScope, Instance, LandingOutcome, LandingPlan, LandingRequest, LandingSummary, MountKind, Planned, StartSpec, VolumeId};

/// The Slates instance kiln keeps on a machine.
pub const INSTANCE: &str = "kiln";
/// The word that makes kiln's executable one of Slates' processes rather than kiln: `kiln slates-role
/// --instance <name> anchor --shards <n>` is an instance's anchor, and the daemon its anchor starts
/// runs the same way. Every such process carries its instance's name in its arguments.
pub const ROLE_ARG: &str = "slates-role";
/// How long one git command over a workspace may take.
const GIT_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a new anchor has to answer.
const UP_WAIT: Duration = Duration::from_secs(30);
/// Shards the instance runs (each is a core's worth of work when busy): agents' folders need few.
const SHARDS: u16 = 2;
/// The largest a workspace may grow to, at most (what it changes is held in memory): a quarter of
/// the machine's memory, up to this.
const MAX_VOLUME_GIB: u64 = 64;
/// What the anchor prints of its issuer authority's handoff (Slates' `role::anchor`,
/// `announce_issuer_surface`): the variables a process of the anchor's user exports to hold it.
const ISSUER_SURFACE: &str = "issuer surface: export ";

struct Here {
    /// Where workspaces are mounted (`<state>/workspaces/<name>`).
    root: PathBuf,
    /// What the instance's anchor and daemon say as they run, when this process started them: the
    /// issuer authority's handoff among it (macOS, Windows).
    log: PathBuf,
}

static HERE: OnceLock<Here> = OnceLock::new();

/// Where this process keeps its machine's workspaces: under `state_dir` (a runner's, shared by
/// every runner process of the machine; the server's data directory for its own agents).
pub fn init(state_dir: &Path) {
    let _ = HERE.set(Here { root: state_dir.join("workspaces"), log: state_dir.join("slates-anchor.log") });
}

fn here() -> anyhow::Result<&'static Here> {
    HERE.get().ok_or_else(|| anyhow::anyhow!("this process keeps no workspaces"))
}

/// What a test points this process's workspaces at instead: an instance of its own, its anchor
/// started here, run by `program` (a test's own executable isn't kiln's). Set once, before anything
/// here runs.
#[derive(Debug, Clone, Default)]
pub struct Override {
    pub instance: String,
    /// The kiln executable whose `slates-role` the anchor runs as; `None`: this process's own.
    pub program: Option<PathBuf>,
    /// Slates measures the machine in its quick form (seconds less at each start), as its own
    /// tests do.
    pub quick: bool,
}

static OVERRIDE: OnceLock<Override> = OnceLock::new();

/// Points this process's workspaces at `o` (a test's own instance): false when something already did.
pub fn set_override(o: Override) -> bool {
    OVERRIDE.set(o).is_ok()
}

/// The instance kiln uses (`KILN_SLATES_INSTANCE` names another, for labs): the one name every
/// call of this process attaches to, whoever makes it.
pub(crate) fn instance() -> String {
    if let Some(o) = OVERRIDE.get() {
        return o.instance.clone();
    }
    std::env::var("KILN_SLATES_INSTANCE").ok().filter(|i| !i.trim().is_empty()).unwrap_or_else(|| INSTANCE.to_owned())
}

/// The executable an instance's processes run as: kiln's own, in Slates' roles.
fn program() -> anyhow::Result<PathBuf> {
    match OVERRIDE.get().and_then(|o| o.program.clone()) {
        Some(p) => Ok(p),
        None => Ok(std::env::current_exe()?),
    }
}

/// Serves one of Slates' processes in this one (`kiln slates-role <args>`, [`ROLE_ARG`]): Slates' own
/// main for its anchor and daemon, on Slates' own runtime, never kiln's. Its exit code is the
/// process's.
///
/// Slates is built to end a process at any panic (its own release builds abort), so a daemon is
/// never left serving with one of its threads gone: its anchor sees it end and starts it again.
/// kiln builds with unwinding, for its tasks' sake, so a role's process aborts once a panic has
/// been reported.
pub fn role_main(args: &[String]) -> std::process::ExitCode {
    let report = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        report(info);
        std::process::abort();
    }));
    slates_api::role::main(&[ROLE_ARG.to_owned()], args)
}

/// What kiln calls a host mount of `kind` (its facts' `mount`): `fuse` (Linux, through
/// `fusermount3`, no privilege) or `nfs` (macOS, on loopback). None for a kind kiln doesn't mount
/// at yet: a WinFsp drive (Windows) is mounted at a drive letter, and kiln's workspaces are
/// folders.
fn mount_word(kind: MountKind) -> Option<&'static str> {
    match kind {
        MountKind::Fuse => Some("fuse"),
        MountKind::NfsLoopback => Some("nfs"),
        _ => None,
    }
}

/// How a volume is mounted here, as Slates' library finds it, when kiln mounts that kind
/// ([`mount_word`]).
pub fn mount_kind() -> Option<&'static str> {
    slates_api::mount::available().ok().and_then(mount_word)
}

/// What a machine says of its workspaces, in its facts.
pub type Facts = kiln_ci::facts::Workspaces;

/// The Slates kiln links: the revision of it vendor/MANIFEST.toml takes, which vendor/slates holds
/// (docs/vendoring.md).
fn linked() -> Option<String> {
    linked_in(include_str!("../../../../vendor/MANIFEST.toml"))
}

/// The Slates revision a vendor manifest's `[projects.slates]` names.
fn linked_in(manifest: &str) -> Option<String> {
    let manifest = toml::from_str::<toml::Table>(manifest).ok()?;
    let commit = manifest.get("projects")?.get("slates")?.get("commit")?.as_str()?;
    Some(format!("slates {commit}"))
}

/// This machine's workspace facts, looked at now: whether Slates' library can mount here, and why
/// not, as the library says.
pub async fn facts() -> Facts {
    let slates = linked();
    let mount = match slates_api::mount::available() {
        Ok(kind) => mount_word(kind),
        Err(fault) => return Facts { slates, why_not: Some(fault.to_string()), ..Facts::default() },
    };
    let Some(mount) = mount else {
        return Facts { slates, why_not: Some("Slates mounts a volume here at a drive letter (WinFsp), and kiln keeps workspaces in folders".to_owned()), ..Facts::default() };
    };
    if HERE.get().is_none() {
        return Facts { slates, mount: Some(mount.to_owned()), why_not: Some("this kiln keeps no workspaces".to_owned()), ..Facts::default() };
    }
    Facts { available: true, slates, mount: Some(mount.to_owned()), running: up().await, why_not: None }
}

/// Runs `f` against the machine's instance on kiln's blocking pool, on a client of its own that
/// ends with the call: what it answered, or what refused it (`verb` names the call). A client
/// that ends abandons what it alone held, such as a landing it presented (Slates' A-46).
async fn call<R, F>(verb: &'static str, f: F) -> anyhow::Result<R>
where
    R: Send + 'static,
    F: FnOnce(&mut Instance) -> Result<R, slates_api::Error> + Send + 'static,
{
    let name = instance();
    let answer = crate::rt::blocking(move || Instance::attach(&name).and_then(|mut i| f(&mut i))).await?;
    answer.map_err(|e| anyhow::anyhow!("Slates' {verb}: {e}"))
}

/// The instance answers.
async fn up() -> bool {
    let name = instance();
    crate::rt::blocking(move || Instance::attach(&name).is_ok()).await.unwrap_or(false)
}

/// The anchor this process started apart (its own process group), the first one.
static STARTED: OnceLock<u32> = OnceLock::new();

/// The pid of the anchor this process started itself, when it started one apart: what a test that
/// started its own instance stops at its end, and nothing else.
pub fn started_anchor() -> Option<u32> {
    STARTED.get().copied()
}

/// What starts the machine's instance: kiln's own executable in Slates' roles, [`SHARDS`] shards,
/// a laptop's instance of its own (no fleet).
fn start_spec() -> anyhow::Result<StartSpec> {
    let mut spec = StartSpec::new(&instance(), program()?, &[ROLE_ARG]);
    spec.shards = Some(SHARDS);
    spec.quick = OVERRIDE.get().is_some_and(|o| o.quick);
    Ok(spec)
}

/// Starts the instance's anchor apart from this process: a systemd user unit of its own on Linux
/// where there is one (stopping or restarting the runner's unit never takes it along), its own
/// process group elsewhere, with what it says appended to `log`. The anchor started here, which
/// [`ensure_up`] watches while it waits for its daemon to answer; none for a unit's.
async fn start_anchor(log: &Path) -> anyhow::Result<Option<std::process::Child>> {
    let mut anchor = start_spec()?.anchor_command();
    // Tests and labs start theirs here (`KILN_SLATES_DIRECT`): no unit outlives them.
    let direct = OVERRIDE.get().is_some() || std::env::var_os("KILN_SLATES_DIRECT").is_some();
    if !direct && cfg!(target_os = "linux") && std::env::var_os("XDG_RUNTIME_DIR").is_some() && crate::os::which("systemd-run").is_some() {
        let unit = format!("{}-slates", instance());
        let mut reset = Command::new("systemctl");
        reset.args(["--user", "reset-failed", &format!("{unit}.service")]);
        let _ = crate::proc::output(reset, Duration::from_secs(10)).await;
        let mut c = Command::new("systemd-run");
        c.args(["--user", "--quiet", "--collect", &format!("--unit={unit}"), "--"]).arg(anchor.get_program()).args(anchor.get_args());
        match crate::proc::output(c, Duration::from_secs(20)).await {
            Ok(o) if o.status.success() => return Ok(None),
            Ok(o) => tracing::info!(said = %String::from_utf8_lossy(&o.stderr).trim(), "Slates' anchor didn't start as a unit of its own; starting it here"),
            Err(e) => tracing::info!(error = %format!("{e:#}"), "Slates' anchor didn't start as a unit of its own; starting it here"),
        }
    }
    if let Some(dir) = log.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let out = std::fs::OpenOptions::new().create(true).append(true).open(log)?;
    let err = out.try_clone()?;
    anchor.stdout(out).stderr(err);
    let child = crate::os::supervise::spawn_detached(&mut anchor)?;
    let _ = STARTED.set(child.id());
    Ok(Some(child))
}

/// One start of the anchor at a time: agents starting together find it started by the first.
fn starting() -> &'static futures::lock::Mutex<()> {
    static STARTING: OnceLock<futures::lock::Mutex<()>> = OnceLock::new();
    STARTING.get_or_init(|| futures::lock::Mutex::new(()))
}

/// The instance answers, its anchor started (and its one-node group made) when it wasn't running.
pub async fn ensure_up() -> anyhow::Result<()> {
    if up().await {
        return Ok(());
    }
    let _one = starting().lock().await;
    if up().await {
        return Ok(());
    }
    let h = here()?;
    let anchor = start_anchor(&h.log).await?;
    let name = instance();
    let until = Instant::now() + UP_WAIT;
    // The anchor started here is watched while it's waited for (its exit ends the wait), and never
    // after: it outlives this process.
    let answered = crate::rt::blocking(move || {
        let mut anchor = anchor;
        Instance::wait_for(&name, anchor.as_mut(), Some(until)).map(|_| ())
    })
    .await?;
    // Another of the machine's kiln processes may have started it meanwhile, its anchor this one's
    // refused second.
    if let Err(e) = answered
        && !up().await
    {
        anyhow::bail!("Slates' anchor didn't answer within {}s: {e} (its log: {})", UP_WAIT.as_secs(), h.log.display());
    }
    // A new instance holds no volumes until its group exists; an existing one is left as it is.
    call("bootstrap", |i| i.ensure_group()).await
}

/// The most a workspace may hold, in bytes: a quarter of this machine's memory, at least a GiB and
/// at most [`MAX_VOLUME_GIB`].
fn volume_max() -> u64 {
    let mut sys = sysinfo::System::new();
    sys.refresh_memory();
    let quarter_gib = (sys.total_memory() / 4) >> 30;
    quarter_gib.clamp(1, MAX_VOLUME_GIB) << 30
}

/// A workspace as it stands: the volume, its name and where it's mounted.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Made {
    pub volume: String,
    pub name: String,
    pub mount: String,
}

/// A volume's id from its text (Slates' 32 hexadecimal digits, as [`slates_api::volume_id_text`]
/// writes it).
fn volume_id(text: &str) -> anyhow::Result<VolumeId> {
    Ok(VolumeId { bytes: hex_bytes(text).ok_or_else(|| anyhow::anyhow!("{text:?} isn't a Slates volume id: 32 hexadecimal digits"))? })
}

/// A manifest's hash from its text (64 hexadecimal digits, as [`LandingPlan::manifest_text`] writes
/// it).
fn manifest_hash(text: &str) -> anyhow::Result<[u8; 32]> {
    hex_bytes(text).ok_or_else(|| anyhow::anyhow!("{text:?} isn't a landing's manifest: 64 hexadecimal digits"))
}

/// `N` bytes from their `2 * N` hexadecimal digits (either case), or none: digits alone, never a
/// sign, which Rust's own number parsing would take (`+0` is a zero there).
fn hex_bytes<const N: usize>(text: &str) -> Option<[u8; N]> {
    if text.len() != N.checked_mul(2)? || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut bytes = [0u8; N];
    for (i, byte) in bytes.iter_mut().enumerate() {
        let pair = text.get(i.checked_mul(2)?..i.checked_mul(2)?.checked_add(2)?)?;
        *byte = u8::from_str_radix(pair, 16).ok()?;
    }
    Some(bytes)
}

/// Where workspace `name` is mounted.
pub fn mount_of(name: &str) -> anyhow::Result<PathBuf> {
    Ok(here()?.root.join(name))
}

/// A new workspace `name` laid over `over`, mounted.
pub async fn make(name: &str, over: &Path) -> anyhow::Result<Made> {
    ensure_up().await?;
    let spec = slates_api::overlay(name, over, volume_max()).map_err(|e| anyhow::anyhow!("Slates: {e}"))?;
    let id = call("volume create", move |i| i.create(&spec)).await?;
    let mount = mount_of(name)?;
    let m = Made { volume: slates_api::volume_id_text(id), name: name.to_owned(), mount: mount.to_string_lossy().into_owned() };
    if let Err(e) = mounted(&m).await {
        if let Err(gone) = call("volume destroy", move |i| i.destroy(id)).await {
            tracing::warn!(volume = %m.volume, error = %format!("{gone:#}"), "a workspace that couldn't be mounted stayed");
        }
        return Err(e);
    }
    Ok(m)
}

/// Whether something is mounted at `dir` (its device differs from its parent's).
#[cfg(unix)]
fn is_mount(dir: &Path) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    let (Ok(d), Some(Ok(p))) = (std::fs::metadata(dir), dir.parent().map(std::fs::metadata)) else { return false };
    d.dev() != p.dev()
}

#[cfg(not(unix))]
fn is_mount(_dir: &Path) -> bool {
    false
}

/// Workspace `m` is mounted where it says (again, after the machine's mount went away while its
/// volume stayed).
pub async fn mounted(m: &Made) -> anyhow::Result<()> {
    let dir = PathBuf::from(&m.mount);
    if is_mount(&dir) {
        return Ok(());
    }
    std::fs::create_dir_all(&dir)?;
    let volume = volume_id(&m.volume)?;
    let path = m.mount.clone();
    call("mount", move |i| i.mount(volume, &path, &slates_api::MountOptions::default())).await?;
    anyhow::ensure!(is_mount(&dir), "Slates said it mounted {} at {}, but nothing is mounted there", m.name, m.mount);
    Ok(())
}

/// Takes the mount at `path` away: with the OS's own unmount, which needs no daemon (Unix); or by
/// ending its attachment (Windows, where the drive is the daemon's own).
async fn unmount(path: String) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        crate::rt::blocking(move || slates_api::mount::unmount(&path)).await?.map_err(|e| anyhow::anyhow!("Slates' unmount: {e}"))
    }
    #[cfg(not(unix))]
    {
        call("unmount", move |i| i.unmount(&path)).await
    }
}

/// Lets go of the mount at `path` though something still uses it: the kernel takes it out of the
/// namespace now and finishes once its last user closes (Linux's lazy unmount); refused elsewhere,
/// saying why.
async fn let_go(path: String) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        crate::rt::blocking(move || slates_api::mount::unmount_lazy(&path)).await?.map_err(|e| anyhow::anyhow!("Slates' unmount: {e}"))
    }
    #[cfg(not(unix))]
    {
        anyhow::bail!("a lazy unmount of {path} is Linux's alone")
    }
}

/// Workspace `m` gone: unmounted, its volume destroyed and its mount point removed. What it held
/// is lost. A mount still in use (something the agent left running works there) is let go lazily
/// on Linux, and its volume destroyed all the same: nothing is kept for an agent that has ended.
pub async fn remove(m: &Made) -> anyhow::Result<()> {
    let dir = PathBuf::from(&m.mount);
    if is_mount(&dir)
        && let Err(e) = unmount(m.mount.clone()).await
    {
        tracing::info!(mount = %m.mount, error = %format!("{e:#}"), "an ended agent's workspace is still in use: let go lazily");
        if let Err(e) = let_go(m.mount.clone()).await {
            tracing::info!(mount = %m.mount, error = %format!("{e:#}"), "an ended agent's workspace stays mounted until what uses it ends");
        }
    }
    let volume = volume_id(&m.volume)?;
    call("volume destroy", move |i| i.destroy(volume)).await?;
    let _ = std::fs::remove_dir(&dir);
    Ok(())
}

/// The files of the folder below workspace `m` that changed on disk after the agent changed them
/// in its workspace (each as Slates says it: `/README.md (Modified)`).
pub async fn drift(m: &Made) -> anyhow::Result<Vec<String>> {
    let volume = volume_id(&m.volume)?;
    call("status", move |i| i.drift(volume)).await
}

/// What a landing leaves out: git's own directory.
pub const EXCLUDES: &[&str] = &["/.git"];

/// The landing of workspace `m`'s head into `over`, [`EXCLUDES`] left out.
fn landing(m: &Made, over: &str) -> anyhow::Result<LandingRequest> {
    Ok(LandingRequest {
        volume: volume_id(&m.volume)?,
        snapshot: None,
        target: over.to_owned(),
        filter: Filter { include: Vec::new(), exclude: EXCLUDES.iter().map(|p| (*p).to_owned()).collect() },
    })
}

/// How many paths git sees changed in workspace `m`: tracked files changed or deleted, and new
/// files git doesn't ignore, each one (`--untracked-files=all`).
async fn git_changed(m: &Made) -> anyhow::Result<usize> {
    let mut c = Command::new("git");
    c.arg("-C").arg(&m.mount).args(["status", "--porcelain=v1", "-z", "--untracked-files=all"]).env("GIT_TERMINAL_PROMPT", "0");
    let o = crate::proc::output(c, GIT_TIMEOUT).await?;
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
        if record.starts_with('R') || record.starts_with('C') {
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

/// Whether `plan` writes only what git sees changed in workspace `m`: with `.git` the one path
/// left out, anything more is something git ignores (build output, dependencies), which kiln
/// won't write into a person's folder.
pub async fn writable(m: &Made, plan: &Value) -> Result<(), String> {
    let planned = planned_files(plan);
    let changed = git_changed(m).await.map_err(|e| format!("{e:#}"))?;
    if planned <= changed {
        return Ok(());
    }
    Err(format!(
        "it would also write {} file{} git ignores (build output, dependencies) into the folder: kiln leaves out only .git, so put the work up for review instead",
        planned.saturating_sub(changed),
        if planned.saturating_sub(changed) == 1 { "" } else { "s" }
    ))
}

/// A landing's summary as Slates' own JSON has it (its MCP server's and command line's): entries
/// per action, bytes to write, entries the filter left out.
fn summary_json(s: &LandingSummary) -> Value {
    json!({
        "by_action": s.by_action.iter().map(|a| json!({"action": a.action, "count": a.count})).collect::<Vec<_>>(),
        "bytes": s.bytes,
        "filtered_out": s.filtered_out,
    })
}

/// A finished landing's outcome as Slates' own JSON has it: its counts, the durability it reached,
/// and each degraded cell it met, as Slates words it.
fn outcome_json(o: &LandingOutcome) -> Value {
    json!({
        "landing": o.landing,
        "state": o.state,
        "written": o.written,
        "skipped": o.skipped,
        "conflicts": o.conflicts,
        "failed": o.failed,
        "bytes_written": o.bytes_written,
        "held": o.held,
        "durability": {
            "data_synced": o.durability.data_synced,
            "dirs_synced": o.durability.dirs_synced,
            "media": o.durability.media,
            "media_requested": o.durability.media_requested,
            "dirs": o.durability.dirs,
        },
        "degraded": o.degraded.iter().map(|d| format!("{d:?}")).collect::<Vec<_>>(),
        "barriers": o.barriers,
    })
}

/// A plan a person approves as Slates' own JSON has it: the landing, its manifest's hash (what a
/// grant binds), what it would write, and the conflicts found before any write.
fn plan_json(p: &LandingPlan) -> Value {
    json!({
        "grant_required": true,
        "landing": p.landing,
        "manifest": p.manifest_text(),
        "summary": summary_json(&p.summary),
        "conflicts": p.conflicts,
    })
}

/// A landing's presentation as kiln shows it: the plan a person approves, or, when nothing needed a
/// grant (nothing to write), how it went.
fn planned_json(p: &Planned) -> Value {
    match p {
        Planned::Plan(plan) => plan_json(plan),
        Planned::Landed(outcome) => json!({"grant_required": false, "outcome": outcome_json(outcome)}),
    }
}

/// What writing workspace `m` back to `over` would do: Slates' presentation (`landing`,
/// `manifest`, `summary`, `conflicts`, `grant_required`), or how it went if nothing needed a grant.
pub async fn plan_landing(m: &Made, over: &str) -> anyhow::Result<Value> {
    let request = landing(m, over)?;
    let planned = call("land", move |i| i.plan_landing(&request)).await?;
    Ok(planned_json(&planned))
}

/// The handoff of the anchor's issuer authority in what the anchor printed (`log`): the variables
/// of its last `issuer surface: export NAME=VALUE ...` line, the one the running anchor printed.
fn handoff_in(log: &str) -> Option<Vec<(String, String)>> {
    let line = log.lines().rev().find_map(|l| l.split_once(ISSUER_SURFACE).map(|(_, rest)| rest))?;
    Some(line.split_whitespace().filter_map(|kv| kv.split_once('=')).map(|(k, v)| (k.to_owned(), v.to_owned())).collect())
}

/// How a person's grant reaches this machine's Slates: the handoff of its anchor's issuer authority
/// (macOS and Windows, where the anchor's segment has a name and the anchor kiln started printed it
/// into its log). Linux has none: there the segment is a descriptor only the anchor's own children
/// hold (Slates' GAP-A9-10).
fn issuer_handoff() -> anyhow::Result<Vec<(String, String)>> {
    if cfg!(target_os = "linux") {
        anyhow::bail!("Slates on Linux can't take a person's go-ahead from kiln yet (Slates' GAP-A9-10): put the work up for review instead");
    }
    let log = std::fs::read_to_string(&here()?.log).unwrap_or_default();
    handoff_in(&log).ok_or_else(|| anyhow::anyhow!("Slates' anchor here wasn't started by kiln, so kiln can't pass on a person's approval to it"))
}

/// Whether this machine's Slates takes a person's go-ahead from kiln, and why not.
pub fn can_take_grants() -> anyhow::Result<()> {
    issuer_handoff().map(|_| ())
}

/// What came of writing a workspace back: written (Slates' outcome), or not, because the plan is no
/// longer the one the person saw (the new plan, to show them).
#[derive(Debug)]
pub enum Landed {
    Written(Value),
    Changed(Value),
}

/// Writes workspace `m` back to `over` exactly as the plan whose manifest is `manifest` (the one
/// the person saw) says. Slates holds a presentation only as long as the client that made it, and
/// each of kiln's calls has its own, so the plan is made again here: when its manifest is no longer
/// the one the person saw, nothing is written and the new plan comes back. When it still is, and
/// writes no more than git sees changed ([`writable`]), the library presents it once more, grants
/// exactly that manifest under the anchor's issuer authority and lands it under that grant, all on
/// one client ([`Instance::land_approved`]); a manifest that moved meanwhile writes nothing and
/// comes back as the new plan.
pub async fn land(m: &Made, over: &str, manifest: &str) -> anyhow::Result<Landed> {
    let handoff = issuer_handoff()?;
    let approved = manifest_hash(manifest)?;
    let plan = plan_landing(m, over).await?;
    if plan.get("grant_required").and_then(Value::as_bool) != Some(true) {
        // Nothing waits on a grant (nothing to write, or it landed as it was).
        return Ok(Landed::Written(plan));
    }
    if plan.get("manifest").and_then(Value::as_str).map(str::to_ascii_lowercase) != Some(manifest.to_ascii_lowercase()) {
        return Ok(Landed::Changed(plan));
    }
    writable(m, &plan).await.map_err(anyhow::Error::msg)?;
    let request = landing(m, over)?;
    let came = call("land", move |i| {
        let issuer = slates_api::Issuer::from_handoff(&handoff)?;
        i.land_approved(&issuer, &request, &approved, (GrantScope::Once, slates_api::GRANT_TERM_NS))
    })
    .await?;
    Ok(match came {
        Approved::Landed(outcome) => Landed::Written(json!({"grant_required": false, "outcome": outcome_json(&outcome)})),
        Approved::Changed(plan) => Landed::Changed(plan_json(&plan)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_workspace_never_holds_more_than_its_share_of_memory() {
        let max = volume_max();
        assert_eq!(max % (1 << 30), 0, "whole GiB: {max}");
        assert!((1..=MAX_VOLUME_GIB).contains(&(max >> 30)), "{max}");
    }

    #[test]
    fn a_landing_is_weighed_against_what_git_sees_changed() {
        // ` M a`, `?? n`, a rename (its source a field of its own), a deletion.
        assert_eq!(count_changed(" M src/a.rs\0?? src/n.rs\0R  new.rs\0old.rs\0 D gone.rs\0"), 4);
        assert_eq!(count_changed(""), 0);
        let plan = serde_json::json!({"summary": {"by_action": [{"action": "replace", "count": 2}, {"action": "create", "count": 3}, {"action": "mkdir", "count": 4}]}});
        assert_eq!(planned_files(&plan), 5, "directories don't count");
        assert_eq!(planned_files(&serde_json::json!({})), 0);
    }

    /// W3: a volume's id goes out as Slates writes it and comes back the same; anything else is
    /// refused in words, never taken as another volume.
    #[test]
    fn w3_a_volume_id_comes_back_as_slates_wrote_it() -> anyhow::Result<()> {
        let id = VolumeId { bytes: [0x00, 0x01, 0x7f, 0x80, 0xff, 0x10, 0xab, 0xcd, 0xef, 0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde] };
        let text = slates_api::volume_id_text(id);
        assert_eq!(text, "00017f80ff10abcdef123456789abcde");
        assert_eq!(volume_id(&text)?, id);
        assert_eq!(volume_id(&text.to_uppercase())?, id, "either case");
        let short = text.get(..31).unwrap_or_default().to_owned();
        for bad in ["", "00", short.as_str(), &format!("{text}0"), &text.replacen('0', "g", 1), "ü0017f80ff10abcdef123456789abcd", "+0017f80ff10abcdef123456789abcde"] {
            assert!(volume_id(bad).is_err(), "{bad:?} taken as a volume id");
        }
        Ok(())
    }

    /// W6: the manifest a person saw comes back to Slates as the hash it wrote; anything else is
    /// refused before Slates is asked, never granted as another plan.
    #[test]
    fn w6_the_manifest_a_person_saw_is_the_hash_slates_wrote() -> anyhow::Result<()> {
        let plan = LandingPlan { landing: 1, manifest: [0x5a; 32], summary: LandingSummary::default(), conflicts: Vec::new() };
        assert_eq!(manifest_hash(&plan.manifest_text())?, plan.manifest);
        assert_eq!(manifest_hash(&plan.manifest_text().to_uppercase())?, plan.manifest);
        for bad in ["", "5a", &"5a".repeat(31), &"5a".repeat(33), &format!("{}zz", "5a".repeat(31))] {
            assert!(manifest_hash(bad).is_err(), "{bad:?} taken as a manifest");
        }
        Ok(())
    }

    /// W6: a plan shows what Slates would write as Slates' own JSON has it (the manifest a grant
    /// binds in hexadecimal, entries per action, the conflicts), and a landing with nothing to write
    /// shows how it went.
    #[test]
    fn w6_a_landing_plan_reads_as_slates_writes_it() {
        let plan = LandingPlan {
            landing: 7,
            manifest: [0xab; 32],
            summary: LandingSummary {
                by_action: vec![slates_api::ActionCount { action: "create".into(), count: 2 }, slates_api::ActionCount { action: "mkdir".into(), count: 1 }],
                bytes: 12,
                filtered_out: 3,
            },
            conflicts: vec!["/README.md".into()],
        };
        let shown = planned_json(&Planned::Plan(plan));
        let at = |pointer: &str| shown.pointer(pointer).cloned().unwrap_or_default();
        assert_eq!(at("/grant_required"), json!(true));
        assert_eq!(at("/landing"), json!(7));
        assert_eq!(at("/manifest"), json!("ab".repeat(32)));
        assert_eq!(at("/summary/by_action"), json!([{"action": "create", "count": 2}, {"action": "mkdir", "count": 1}]));
        assert_eq!(at("/summary/bytes"), json!(12));
        assert_eq!(at("/summary/filtered_out"), json!(3));
        assert_eq!(at("/conflicts"), json!(["/README.md"]));
        assert_eq!(planned_files(&shown), 2, "kiln weighs Slates' plan as it weighed its command line's");
    }

    /// W6: a person's go-ahead reaches the anchor kiln started through the handoff it printed last
    /// (a restarted anchor prints its own), and a log with none says the anchor isn't kiln's.
    #[test]
    fn w6_the_go_ahead_takes_the_handoff_the_running_anchor_printed() {
        let log = "slates anchor: instance kiln up; daemon pid 10; 2 shards\n\
                   slates anchor: issuer surface: export SLATES_ANCHOR=old SLATES_ANCHOR_LEN=1\n\
                   slates anchor: stopped (daemon exit Some(0))\n\
                   slates anchor: issuer surface: export SLATES_ANCHOR=slates-kiln-2 SLATES_ANCHOR_LEN=4096\n\
                   slates daemon: serving\n";
        assert_eq!(handoff_in(log), Some(vec![("SLATES_ANCHOR".to_owned(), "slates-kiln-2".to_owned()), ("SLATES_ANCHOR_LEN".to_owned(), "4096".to_owned())]));
        assert_eq!(handoff_in("slates anchor: instance kiln up\n"), None);
        assert_eq!(handoff_in(""), None);
    }

    /// W1: the facts name the mounts kiln makes by the library's word, and leave out a kind kiln
    /// doesn't mount at yet.
    #[test]
    fn w1_the_facts_name_the_mount_the_library_offers() {
        assert_eq!(mount_word(MountKind::Fuse), Some("fuse"));
        assert_eq!(mount_word(MountKind::NfsLoopback), Some("nfs"));
        assert_eq!(mount_word(MountKind::WinFsp), None, "a drive letter, not a folder");
        if cfg!(target_os = "macos") {
            assert_eq!(mount_kind(), Some("nfs"), "macOS always mounts on loopback");
        }
    }

    /// W1: the facts name the Slates kiln links: the revision its vendor manifest takes.
    #[test]
    fn w1_the_facts_name_the_slates_kiln_links() {
        let manifest = "[projects.slates]\nkind = \"git\"\ncommit = \"0b850153fcd47c94fec129f1b417b034528d695b\"\n";
        assert_eq!(linked_in(manifest).as_deref(), Some("slates 0b850153fcd47c94fec129f1b417b034528d695b"));
        assert_eq!(linked_in("[projects.hex]\ncommit = \"x\"\n"), None);
        assert!(linked().is_some_and(|l| l.starts_with("slates ") && l.len() == "slates ".len() + 40), "{:?}", linked());
    }

    /// W1: the anchor kiln starts is kiln's own executable in Slates' role, with the instance's name,
    /// its shards and its quick profile only where a test asked.
    #[test]
    fn w1_the_anchor_is_kilns_own_executable_in_slates_role() -> anyhow::Result<()> {
        let spec = start_spec()?;
        assert_eq!(spec.prefix, vec![ROLE_ARG.to_owned()]);
        assert_eq!(spec.shards, Some(SHARDS));
        assert!(spec.fleet.is_none(), "a machine's instance runs alone");
        let command = spec.anchor_command();
        let args: Vec<String> = command.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(args.first().map(String::as_str), Some(ROLE_ARG), "{args:?}");
        assert!(args.windows(2).any(|w| matches!(w, [flag, name] if flag == "--instance" && *name == spec.instance)), "{args:?}");
        assert!(args.iter().any(|a| a == "anchor"), "{args:?}");
        Ok(())
    }
}
