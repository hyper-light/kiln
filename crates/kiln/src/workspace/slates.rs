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
//! pool on a client of its own (`call`): microseconds for most verbs, longer for a mount or a
//! landing, which wait while the daemon lives.
//!
//! The machine keeps one Slates instance (`kiln`) for all its runner processes. Its anchor is
//! started apart from the runner (a systemd user scope of its own on Linux where the runner runs
//! under the user's service manager, its own process group elsewhere), so restarting or upgrading
//! the runner never ends it: a volume lives in the anchor's memory, and goes when it does.
//!
//! kiln speaks Slates' typed consumer contract (slates-api's A-299, pinned at [`CONTRACT`]): every
//! answer is a typed outcome and every failure a typed error, never text to parse. Writing a volume
//! back to its folder follows the write mode its launcher gave it (the person's rule, 2026-11-18):
//! kiln, the launcher, sets each workspace's policy from its agent's permission mode
//! ([`set_policy`]), and Slates itself writes a landing at once (bypass), holds it as an approval
//! request (ask) or refuses it (forbid) ([`land_by_policy`]). Setting a policy and approving are
//! the launcher's acts, proven by a secret of kiln's own that it keeps in its state, 0600, and
//! hands the anchor at every start (`launcher`), so kiln's authority outlives its own restarts
//! and the daemon's, on every system. An agent can neither set its policy nor approve its own
//! landing: Slates refuses both on an agent's channel, and only kiln holds the secret.
//!
//! Slates as built (c8d6adb4) couldn't clone a snapshot of an overlay volume
//! (`ContentUnavailable`), and a pinned base makes every pinned file part of a landing's plan, so
//! each agent gets an overlay of its own, unpinned: what it hasn't touched is the folder as it is
//! now. Files it has touched are witnessed when it first writes them, and a later change on disk
//! to one of them is reported as drift.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use slates_api::{DriftReport, Filter, Instance, Issuer, LandOutcome, LandingApproval, LandingConflict, LandingReport, LandingRequest, LauncherSecret, MountKind, PolicyRefusal, PolicySubject, StartSpec, Summary, VolumeId, VolumeSpec, WritePolicy};

/// The Slates instance kiln keeps on a machine.
pub const INSTANCE: &str = "kiln";
/// The version of Slates' consumer contract this module is written against (slates-api's `CONTRACT`,
/// A-299, raised with every change to a type or a verb of it): a re-vendor that moves it fails
/// `w1_kiln_speaks_the_contract_it_was_written_against` beside the matches it must extend.
///
/// 16 since kiln took Slates at 15f6b5b8e (from 8). Of what 9 to 16 added, kiln uses the
/// anchor's command under a wrapper (14: `start_anchor`'s systemd scope keeps the launcher
/// secret on the anchor's standard input) and the mounts an unmount names unreported (16,
/// Windows: `unmount`). It needs none of the rest: the `Volumes` seam and its simulator (9),
/// a volume's loss window (10), a daemon's recovered verdict (11), a Shards stream's resync
/// channel (12), signed merges (13), the volume core's typed refusals (15, which kiln shows in
/// Slates' words as it did).
///
/// 19 since kiln took Slates at e599cae8a (from 16). What 17 to 19 added are refusals typed where
/// they carried words, each shown by kiln in Slates' words as before, and none matched by kiln: the
/// daemon's database refusals (17, `Unusable::Database`), a snapshot's retention refused apart from
/// the volume's quota (18, `Exhausted::Volume`: the review read of the HVF drive was refused
/// "Quota" for it before A-342), and a target's, a mount point's and a mount's refusals (19,
/// `HostFault::Target`).
pub const CONTRACT: u32 = 19;
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

struct Here {
    /// Where workspaces are mounted (`<state>/workspaces/<name>`).
    root: PathBuf,
    /// What the instance's anchor and daemon say as they run, when this process started them.
    log: PathBuf,
    /// kiln's launcher secret for its instance (`launcher`), 0600.
    secret: PathBuf,
}

static HERE: OnceLock<Here> = OnceLock::new();

/// Where this process keeps its machine's workspaces: under `state_dir` (a runner's, shared by
/// every runner process of the machine; the server's data directory for its own agents).
pub fn init(state_dir: &Path) {
    let _ = HERE.set(Here { root: state_dir.join("workspaces"), log: state_dir.join("slates-anchor.log"), secret: state_dir.join("slates-launcher.key") });
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
        MountKind::WinFsp => None,
    }
}

/// How a volume is mounted here, as Slates' library finds it, when kiln mounts that kind
/// (`mount_word`).
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
pub(crate) async fn call<R, F>(verb: &'static str, f: F) -> anyhow::Result<R>
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
/// a laptop's instance of its own (no fleet), handed `launcher`'s secret when there is one.
fn start_spec(launcher: Option<LauncherSecret>) -> anyhow::Result<StartSpec> {
    let mut spec = StartSpec::new(&instance(), program()?, &[ROLE_ARG]);
    spec.shards = Some(SHARDS);
    spec.quick = OVERRIDE.get().is_some_and(|o| o.quick);
    Ok(match launcher {
        Some(secret) => spec.with_launcher(secret),
        None => spec,
    })
}

/// Whether the anchor starts in a systemd user scope of its own (`scope_wrapper`): on Linux,
/// where this process runs under the user's service manager (as a runner's user unit does), unless
/// `KILN_SLATES_DIRECT` asks for its own process group in this process's cgroup (labs, and the
/// servers end-to-end tests start, which stop their instance's processes themselves). A scope ends
/// with its processes, so a test that stops the anchor it started leaves no unit behind.
pub fn starts_in_a_scope() -> bool {
    std::env::var_os("KILN_SLATES_DIRECT").is_none()
        && cfg!(target_os = "linux")
        && std::env::var_os("XDG_RUNTIME_DIR").is_some()
        && crate::os::which("systemd-run").is_some()
        && std::fs::read_to_string("/proc/self/cgroup").is_ok_and(|cgroup| in_user_manager(&cgroup))
}

/// Whether the cgroup `/proc/self/cgroup` names (`cgroup`, its v2 `0::` line) lies in the user's own
/// service manager's tree (`user@<uid>.service`): only from there can that manager move a process
/// of this one's into a scope of its own. A login session's cgroup (`session-<n>.scope`) and a
/// system service's belong to the system's manager, and the user's can't move a process out of them.
fn in_user_manager(cgroup: &str) -> bool {
    cgroup.lines().filter_map(|l| l.strip_prefix("0::")).any(|path| path.split('/').any(|part| part.starts_with("user@") && part.ends_with(".service")))
}

/// The command that runs the anchor in a systemd user scope of its own named `unit`
/// (`systemd-run --user --scope`): systemd-run moves its own process into the scope and then execs
/// the anchor, which keeps the standard input, the environment and the process group Slates gave
/// it (`StartSpec::anchor_command_under`, contract 14). The launcher's secret reaches the anchor
/// on that standard input, and stopping or restarting the runner's unit never takes the instance
/// along. A transient service (`systemd-run` with no `--scope`) is started by the user manager,
/// which hands it no standard input of ours.
fn scope_wrapper(unit: &str) -> Vec<OsString> {
    ["systemd-run", "--user", "--scope", "--quiet", "--collect", &format!("--unit={unit}"), "--"].into_iter().map(OsString::from).collect()
}

/// The anchor's command, handed `launcher`'s secret: run under [`scope_wrapper`] where it starts
/// under systemd (`scoped`), as itself elsewhere.
fn anchor_command(launcher: LauncherSecret, scoped: bool) -> anyhow::Result<Command> {
    let spec = start_spec(Some(launcher))?;
    let command = if scoped {
        let wrapper = scope_wrapper(&format!("{}-slates", instance()));
        spec.anchor_command_under(&wrapper.iter().map(OsString::as_os_str).collect::<Vec<&OsStr>>())
    } else {
        spec.anchor_command()
    };
    command.map_err(|e| anyhow::anyhow!("Slates: {e}"))
}

/// Starts the instance's anchor apart from this process, handed kiln's launcher secret: in a
/// systemd user scope of its own on Linux where this process runs under the user's service manager
/// ([`starts_in_a_scope`], [`scope_wrapper`]), in its own process group elsewhere, with what it says
/// (systemd-run's own refusal included) appended to `log`. The process started is the anchor
/// itself, which [`ensure_up`] watches while it waits for its daemon to answer.
fn start_anchor(log: &Path) -> anyhow::Result<std::process::Child> {
    let mut anchor = anchor_command(launcher_secret()?, starts_in_a_scope())?;
    if let Some(dir) = log.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let out = std::fs::OpenOptions::new().create(true).append(true).open(log)?;
    let err = out.try_clone()?;
    // Its standard input stays the one Slates gave it: the launcher's secret, then its end.
    anchor.stdout(out).stderr(err);
    let child = crate::os::supervise::spawn_detached(&mut anchor)?;
    let _ = STARTED.set(child.id());
    Ok(child)
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
    let anchor = start_anchor(&h.log)?;
    let name = instance();
    let until = Instant::now() + UP_WAIT;
    // The anchor started here is watched while it's waited for (its exit ends the wait, a scope
    // systemd refused included), and never after: it outlives this process.
    let answered = crate::rt::blocking(move || {
        let mut anchor = anchor;
        Instance::wait_for(&name, Some(&mut anchor), Some(until)).map(|_| ())
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
pub fn volume_id(text: &str) -> anyhow::Result<VolumeId> {
    Ok(VolumeId { bytes: hex_bytes(text).ok_or_else(|| anyhow::anyhow!("{text:?} isn't a Slates volume id: 32 hexadecimal digits"))? })
}

/// A manifest's hash from its text (64 hexadecimal digits, as [`LandingApproval::manifest_text`] writes
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
    let spec = VolumeSpec::overlay(name, over, volume_max());
    let id = call("volume create", move |i| i.provision(&spec).map(|made| made.volume())).await?;
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

/// A new workspace `name` laid over `over` with no mount (kiln goal 01a1ec34599974448f6b7663ddb07001): an isolated
/// session's, whose microVM Slates serves the volume itself over Slates' own virtio-fs device (`crate::sandbox::served`),
/// so nothing of it is on this machine's disk or in its mount table. Its `mount` is empty.
pub async fn make_unmounted(name: &str, over: &Path) -> anyhow::Result<Made> {
    ensure_up().await?;
    let spec = VolumeSpec::overlay(name, over, volume_max());
    let id = call("volume create", move |i| i.provision(&spec).map(|made| made.volume())).await?;
    Ok(Made { volume: slates_api::volume_id_text(id), name: name.to_owned(), mount: String::new() })
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
    // One made with no mount (an isolated session's, [`make_unmounted`]) has none to make again.
    if m.mount.is_empty() {
        return Ok(());
    }
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
/// ending its attachment (Windows, where the drive is the daemon's own). On Windows Slates finds
/// the drive among its volumes' mounts, and names each volume it couldn't ask (one its recovery
/// couldn't rebuild): those are logged, since their mounts are unknown.
async fn unmount(path: String) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        crate::rt::blocking(move || slates_api::mount::unmount(&path)).await?.map_err(|e| anyhow::anyhow!("Slates' unmount: {e}"))
    }
    #[cfg(not(unix))]
    {
        let unmounted = call("unmount", move |i| i.unmount(&path)).await?;
        if !unmounted.unreported.is_empty() {
            let unreported: Vec<String> = unmounted.unreported.iter().map(ToString::to_string).collect();
            tracing::info!(unreported = %unreported.join("; "), "Slates unmounted a workspace's drive but couldn't ask every volume for its mounts");
        }
        Ok(())
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
    if !m.mount.is_empty()
        && is_mount(&dir)
        && let Err(e) = unmount(m.mount.clone()).await
    {
        tracing::info!(mount = %m.mount, error = %format!("{e:#}"), "an ended agent's workspace is still in use: let go lazily");
        if let Err(e) = let_go(m.mount.clone()).await {
            tracing::info!(mount = %m.mount, error = %format!("{e:#}"), "an ended agent's workspace stays mounted until what uses it ends");
        }
    }
    let volume = volume_id(&m.volume)?;
    call("volume destroy", move |i| i.destroy(volume)).await?;
    if !m.mount.is_empty() {
        let _ = std::fs::remove_dir(&dir);
    }
    Ok(())
}

/// The files of the folder below workspace `m` that changed on disk after the agent changed them
/// in its workspace, each as kiln shows it (`/README.md (Modified)`, `drift_lines`).
pub async fn drift(m: &Made) -> anyhow::Result<Vec<String>> {
    let volume = volume_id(&m.volume)?;
    let report = call("status", move |i| i.drift(volume)).await?;
    Ok(report.as_ref().map(drift_lines).unwrap_or_default())
}

/// A drift report as kiln shows it: each drifted entry and what the disk did to it, then each entry
/// whose re-check the host refused, as not known to match.
fn drift_lines(report: &DriftReport) -> Vec<String> {
    let drifted = report.drifted.iter().map(|d| format!("{} ({:?})", d.path, d.kind));
    drifted.chain(report.unverified.iter().map(|path| format!("{path} (Unverified)"))).collect()
}

/// Sets workspace `m`'s write policy: what its landings do, as the launcher (kiln) gives it from its
/// agent's permission mode, and an agent can never raise. A setting another raced is read and set
/// again, once.
pub async fn set_policy(m: &Made, policy: WritePolicy) -> anyhow::Result<()> {
    let authority = launcher()?;
    let volume = volume_id(&m.volume)?;
    call("write policy", move |i| match i.set_write_policy(PolicySubject::Volume(volume), &policy, &authority) {
        Err(slates_api::Error::Stale(_)) => i.set_write_policy(PolicySubject::Volume(volume), &policy, &authority),
        set => set,
    })
    .await
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
pub async fn writable(m: &Made, over: &str, plan: &Value) -> Result<(), String> {
    let planned = planned_files(plan);
    // One with no mount has no working tree for git to look at: what git ignores among what its landing writes is read
    // from its entries and its own `.gitignore` files (kiln goal 01a1ec34…, `super::review::ignored_writes`).
    if m.mount.is_empty() {
        let ignored = super::review::ignored_writes(m, over).await.map_err(|e| format!("{e:#}"))?;
        return match ignored {
            0 => Ok(()),
            n => Err(format!("it would also write {n} file{} git ignores (build output, dependencies) into the folder: kiln leaves out only .git, so put the work up for review instead", if n == 1 { "" } else { "s" })),
        };
    }
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

/// What a landing would write, as kiln shows it (the shape Slates' own JSON had): entries per
/// action, bytes to write, entries the filter left out.
fn summary_json(s: &Summary) -> Value {
    let a = &s.actions;
    let by_action: Vec<Value> = [("create", a.create), ("replace", a.replace), ("delete", a.delete), ("rename", a.rename), ("mkdir", a.mkdir), ("rmdir", a.rmdir), ("symlink", a.symlink), ("clear", a.clear)]
        .into_iter()
        .filter(|(_, count)| *count > 0)
        .map(|(action, count)| json!({"action": action, "count": count}))
        .collect();
    json!({"by_action": by_action, "bytes": s.bytes, "filtered_out": s.filtered_out})
}

/// What reached the disk, as kiln shows it: the landing's counts, how it finished, the durability
/// it reached and each degraded cell it met.
fn report_json(r: &LandingReport) -> Value {
    json!({
        "landing": r.landing,
        "state": format!("{:?}", r.state).to_ascii_lowercase(),
        "written": r.written,
        "skipped": r.skipped,
        "conflicts": r.conflicts,
        "failed": r.failed,
        "bytes_written": r.bytes_written,
        "held": r.held,
        "durability": {
            "data_synced": r.durability.data_synced,
            "dirs_synced": r.durability.dirs_synced,
            "media": r.durability.media,
            "media_requested": r.durability.media_requested,
            "dirs": r.durability.dirs,
        },
        "degraded": r.degraded.iter().map(|d| format!("{d:?}")).collect::<Vec<_>>(),
        "barriers": r.barriers,
    })
}

/// What an approval would write, as kiln shows it: the manifest it binds (64 hexadecimal digits),
/// what it writes, and its presentation.
fn approval_json(a: &LandingApproval) -> Value {
    json!({"manifest": a.manifest_text(), "summary": summary_json(&a.summary), "presentation": a.presentation})
}

/// Each landing conflict as kiln shows it: the entry and why it can't land.
fn conflicts_json(conflicts: &[LandingConflict]) -> Value {
    Value::Array(conflicts.iter().map(|c| json!({"path": c.path, "class": format!("{:?}", c.class)})).collect())
}

/// A policy's refusal as kiln shows it: the act, whose policy, why, and the same in words.
fn refusal_json(r: &PolicyRefusal) -> Value {
    let by = match r.by {
        PolicySubject::Volume(v) => json!({"volume": slates_api::volume_id_text(v)}),
        PolicySubject::Consumer(c) => json!({"consumer": c}),
    };
    json!({"act": format!("{:?}", r.act).to_ascii_lowercase(), "by": by, "reason": format!("{:?}", r.reason), "said": r.to_string()})
}

/// A landing's outcome as kiln answers it (docs/agentic.md W6): `outcome` names Slates' typed
/// outcome, and the field of that name holds it: `written` (what reached the disk),
/// `approval_required` (the `approval` the launcher or a person approves), `conflicted` (the
/// `conflicts`; nothing written) or `refused` (the policy's `refusal`).
pub fn outcome_json(o: &LandOutcome) -> Value {
    match o {
        LandOutcome::Written(report) => json!({"outcome": "written", "written": report_json(report)}),
        LandOutcome::ApprovalRequired(approval) => json!({"outcome": "approval_required", "approval": approval_json(approval)}),
        LandOutcome::Conflicted(conflicts) => json!({"outcome": "conflicted", "conflicts": conflicts_json(conflicts)}),
        LandOutcome::Refused(refusal) => json!({"outcome": "refused", "refusal": refusal_json(refusal)}),
    }
}

/// A presentation as kiln's plan shows it (`agents.land` with no manifest): the plan an approval
/// binds (its manifest, what it writes), the conflicts that hold it back with no manifest to
/// approve, how it went when nothing needed an approval, or the policy's refusal.
fn plan_json(o: &LandOutcome) -> Value {
    match o {
        LandOutcome::ApprovalRequired(a) => json!({"grant_required": true, "landing": a.presentation, "manifest": a.manifest_text(), "summary": summary_json(&a.summary), "conflicts": []}),
        LandOutcome::Conflicted(conflicts) => json!({"grant_required": true, "conflicts": conflicts.iter().map(|c| c.path.clone()).collect::<Vec<_>>()}),
        LandOutcome::Written(report) => json!({"grant_required": false, "outcome": report_json(report)}),
        LandOutcome::Refused(refusal) => json!({"grant_required": true, "refused": refusal_json(refusal)}),
    }
}

/// The landing of workspace `m` into `over` as Slates presents it, never writing whatever the
/// volume's mode (its present-only verb): what review shows, and what an approval binds.
pub async fn plan(m: &Made, over: &str) -> anyhow::Result<LandOutcome> {
    let request = landing(m, over)?;
    call("land", move |i| i.plan_landing(&request)).await
}

/// What writing workspace `m` back to `over` would do: Slates' presentation (`landing`,
/// `manifest`, `summary`, `conflicts`, `grant_required`), or how it went if nothing needed an
/// approval.
pub async fn plan_landing(m: &Made, over: &str) -> anyhow::Result<Value> {
    Ok(plan_json(&plan(m, over).await?))
}

/// Lands workspace `m` into `over` as its write policy says ([`set_policy`]): written at once
/// (bypass), held as an approval request (ask), conflicted, or refused (forbid), Slates' typed
/// outcome either way.
pub async fn land_by_policy(m: &Made, over: &str) -> anyhow::Result<LandOutcome> {
    let request = landing(m, over)?;
    call("land", move |i| i.land(&request, None)).await
}

/// Approves the landing of workspace `m` into `over` whose manifest is `manifest` (64 hexadecimal
/// digits), as the launcher: Slates presents it again (never writing), and when its manifest is
/// still that one approves exactly it under kiln's authority and lands it
/// ([`Instance::approve_landing`], from any client: an approval is bound to its request and
/// manifest). When anything changed, nothing is written and the answer is what holds now: the new
/// approval request, the conflicts, or the policy's refusal.
pub async fn approve(m: &Made, over: &str, manifest: &str) -> anyhow::Result<LandOutcome> {
    let authority = launcher()?;
    let approved = manifest_hash(manifest)?;
    let request = landing(m, over)?;
    call("land", move |i| match i.plan_landing(&request)? {
        LandOutcome::ApprovalRequired(approval) if approval.manifest == approved => i.approve_landing(&approval, &authority),
        other => Ok(other),
    })
    .await
}

/// kiln's launcher secret for its instance, made once and kept in its state (0600): the same at
/// every start and after kiln's own restarts, so the authority it proves never changes under a
/// running instance. Two processes making it at once keep the first one written.
fn launcher_secret() -> anyhow::Result<LauncherSecret> {
    secret_at(&here()?.secret)
}

/// The launcher secret kept at `path`, made and kept there first when there is none (`launcher_secret`).
fn secret_at(path: &Path) -> anyhow::Result<LauncherSecret> {
    if let Some(kept) = read_secret(path)? {
        return Ok(kept);
    }
    let fresh = LauncherSecret::generate().map_err(|e| anyhow::anyhow!("Slates: {e}"))?;
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    crate::util::write_secret(&tmp, fresh.bytes(), 0o600)?;
    // Linked, never renamed over: a secret another process kept first stays, and this one goes.
    let kept = std::fs::hard_link(&tmp, path);
    let _ = std::fs::remove_file(&tmp);
    match kept {
        Ok(()) => Ok(fresh),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => read_secret(path)?.ok_or_else(|| anyhow::anyhow!("{} vanished as it was made", path.display())),
        Err(e) => Err(e.into()),
    }
}

/// The launcher secret kept at `path`, if one is: refused when the file holds anything but one.
fn read_secret(path: &Path) -> anyhow::Result<Option<LauncherSecret>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let bytes: [u8; slates_api::launcher::LAUNCHER_SECRET_BYTES] = bytes.try_into().map_err(|b: Vec<u8>| anyhow::anyhow!("{} holds {} bytes, not a launcher secret", path.display(), b.len()))?;
    LauncherSecret::from_bytes(bytes).map(Some).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))
}

/// kiln's authority as its instance's launcher (`launcher_secret`): what sets a workspace's write
/// policy and approves a landing its mode asks about. Every anchor kiln starts is handed the
/// secret, a scoped one under systemd included ([`start_anchor`]).
fn launcher() -> anyhow::Result<Issuer> {
    Ok(Issuer::from_launcher_secret(&launcher_secret()?))
}

/// What came of writing a workspace back: written (Slates' outcome), or not, because the plan is no
/// longer the one the person saw (the new plan, to show them).
#[derive(Debug)]
pub enum Landed {
    Written(Value),
    Changed(Value),
}

/// Writes workspace `m` back to `over` exactly as the plan whose manifest is `manifest` (the one
/// the person saw) says, when it writes no more than git sees changed ([`writable`]): approved
/// under kiln's authority ([`approve`]). When anything changed, nothing is written and the new plan
/// comes back.
pub async fn land(m: &Made, over: &str, manifest: &str) -> anyhow::Result<Landed> {
    let shown = plan_landing(m, over).await?;
    if shown.get("manifest").and_then(Value::as_str).map(str::to_ascii_lowercase) != Some(manifest.to_ascii_lowercase()) {
        return Ok(match shown.get("grant_required").and_then(Value::as_bool) {
            // Nothing waits on an approval (nothing to write, or it landed as it was).
            Some(false) => Landed::Written(shown),
            _ => Landed::Changed(shown),
        });
    }
    writable(m, over, &shown).await.map_err(anyhow::Error::msg)?;
    Ok(match approve(m, over, manifest).await? {
        LandOutcome::Written(report) => Landed::Written(json!({"grant_required": false, "outcome": report_json(&report)})),
        other => Landed::Changed(plan_json(&other)),
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

    /// An approval of `manifest` writing two files and making a directory, of a landing into `/src/kiln`.
    fn approval(manifest: [u8; 32]) -> anyhow::Result<LandingApproval> {
        let m = Made { volume: "00017f80ff10abcdef123456789abcde".into(), name: "kiln-a".into(), mount: "/w/kiln-a".into() };
        Ok(LandingApproval {
            request: landing(&m, "/src/kiln")?,
            presentation: 7,
            manifest,
            summary: Summary { actions: slates_api::ActionTally { create: 2, mkdir: 1, ..Default::default() }, bytes: 12, filtered_out: 3 },
            verification: slates_api::Verification::Unsigned,
        })
    }

    /// W1: kiln speaks the contract it was written against; a re-vendor that moves it fails here, beside the matches
    /// it must extend.
    #[test]
    fn w1_kiln_speaks_the_contract_it_was_written_against() {
        assert_eq!(slates_api::CONTRACT, CONTRACT);
    }

    /// W6: the manifest a person saw comes back to Slates as the hash it wrote; anything else is
    /// refused before Slates is asked, never approved as another plan.
    #[test]
    fn w6_the_manifest_a_person_saw_is_the_hash_slates_wrote() -> anyhow::Result<()> {
        let shown = approval([0x5a; 32])?;
        assert_eq!(manifest_hash(&shown.manifest_text())?, shown.manifest);
        assert_eq!(manifest_hash(&shown.manifest_text().to_uppercase())?, shown.manifest);
        for bad in ["", "5a", &"5a".repeat(31), &"5a".repeat(33), &format!("{}zz", "5a".repeat(31))] {
            assert!(manifest_hash(bad).is_err(), "{bad:?} taken as a manifest");
        }
        Ok(())
    }

    /// W6: a plan shows what Slates would write as kiln always showed it (the manifest an approval
    /// binds in hexadecimal, entries per action, the conflicts), weighed as before; a conflicted
    /// presentation shows its conflicts and no manifest to approve.
    #[test]
    fn w6_a_landing_plan_reads_as_kiln_showed_it() -> anyhow::Result<()> {
        let shown = plan_json(&LandOutcome::ApprovalRequired(approval([0xab; 32])?));
        let at = |pointer: &str| shown.pointer(pointer).cloned().unwrap_or_default();
        assert_eq!(at("/grant_required"), json!(true));
        assert_eq!(at("/landing"), json!(7));
        assert_eq!(at("/manifest"), json!("ab".repeat(32)));
        assert_eq!(at("/summary/by_action"), json!([{"action": "create", "count": 2}, {"action": "mkdir", "count": 1}]));
        assert_eq!(at("/summary/bytes"), json!(12));
        assert_eq!(at("/summary/filtered_out"), json!(3));
        assert_eq!(at("/conflicts"), json!([]));
        assert_eq!(planned_files(&shown), 2, "kiln weighs Slates' plan as it weighed its command line's");
        let conflict = LandingConflict { path: "/README.md".into(), class: slates_api::ConflictClass::ModifyModify, planned: None };
        let held = plan_json(&LandOutcome::Conflicted(vec![conflict]));
        assert_eq!(held.get("conflicts"), Some(&json!(["/README.md"])));
        assert!(held.get("manifest").is_none(), "nothing to approve while it conflicts: {held}");
        Ok(())
    }

    /// W6 (docs/agentic.md; kiln's slates e2e contract): a landing's outcome names Slates' typed outcome, and the
    /// field of that name holds it, the approval's manifest as the 64 hexadecimal digits a person approves.
    #[test]
    fn w6_a_landing_outcome_names_slates_typed_outcome() -> anyhow::Result<()> {
        let held = outcome_json(&LandOutcome::ApprovalRequired(approval([0xcd; 32])?));
        assert_eq!(held.get("outcome"), Some(&json!("approval_required")));
        assert_eq!(held.pointer("/approval/manifest"), Some(&json!("cd".repeat(32))));
        assert_eq!(held.pointer("/approval/summary/bytes"), Some(&json!(12)));
        let conflict = LandingConflict { path: "/a.rs".into(), class: slates_api::ConflictClass::ModifyModify, planned: None };
        let conflicted = outcome_json(&LandOutcome::Conflicted(vec![conflict]));
        assert_eq!(conflicted.get("outcome"), Some(&json!("conflicted")));
        assert_eq!(conflicted.pointer("/conflicts/0/path"), Some(&json!("/a.rs")));
        let volume = VolumeId { bytes: [7; 16] };
        let refusal = PolicyRefusal { act: slates_api::WriteAct::Land, by: PolicySubject::Volume(volume), reason: slates_api::PolicyReason::Forbidden };
        let refused = outcome_json(&LandOutcome::Refused(refusal));
        assert_eq!(refused.get("outcome"), Some(&json!("refused")));
        assert_eq!(refused.pointer("/refusal/act"), Some(&json!("land")));
        assert_eq!(refused.pointer("/refusal/by/volume"), Some(&json!(slates_api::volume_id_text(volume))));
        assert_eq!(refused.pointer("/refusal/reason"), Some(&json!("Forbidden")));
        assert!(refused.pointer("/refusal/said").and_then(Value::as_str).is_some_and(|s| s.contains("forbidden")), "{refused}");
        Ok(())
    }

    /// W4: a drifted entry shows as kiln always showed it, the disk's change named, and one the host
    /// couldn't re-check as not known to match.
    #[test]
    fn w4_drift_reads_as_kiln_showed_it() {
        let report = DriftReport { drifted: vec![slates_api::Drift { path: "/README.md".into(), kind: slates_api::DriftKind::Modified }], unverified: vec!["/b.rs".into()], watcher: slates_api::Watcher::Unavailable };
        assert_eq!(drift_lines(&report), ["/README.md (Modified)", "/b.rs (Unverified)"]);
        assert!(drift_lines(&DriftReport { drifted: Vec::new(), unverified: Vec::new(), watcher: slates_api::Watcher::Live }).is_empty());
    }

    /// W6: kiln's launcher secret is made once and kept, private, and read back the same at every
    /// start; a kept file that holds anything but a secret is refused, never replaced.
    #[test]
    fn w6_kilns_launcher_secret_is_made_once_and_kept() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("slates-launcher.key");
        let made = secret_at(&path)?;
        assert_eq!(secret_at(&path)?, made, "the same secret at every start");
        assert_eq!(std::fs::read(&path)?, made.bytes().to_vec());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(std::fs::metadata(&path)?.permissions().mode() & 0o777, 0o600, "kept private");
        }
        std::fs::write(&path, b"not a secret")?;
        assert!(secret_at(&path).is_err(), "a kept file that isn't a secret is refused");
        assert_eq!(std::fs::read(&path)?, b"not a secret", "never replaced");
        Ok(())
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
        let spec = start_spec(None)?;
        assert_eq!(spec.prefix, vec![ROLE_ARG.to_owned()]);
        assert_eq!(spec.shards, Some(SHARDS));
        assert!(spec.fleet.is_none(), "a machine's instance runs alone");
        let command = spec.anchor_command()?;
        let args: Vec<String> = command.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(args.first().map(String::as_str), Some(ROLE_ARG), "{args:?}");
        assert!(args.windows(2).any(|w| matches!(w, [flag, name] if flag == "--instance" && *name == spec.instance)), "{args:?}");
        assert!(args.iter().any(|a| a == "anchor"), "{args:?}");
        // With kiln's secret, the anchor is told its standard input carries it (and the secret is in no argument).
        let secret = LauncherSecret::generate()?;
        let handed = start_spec(Some(secret.clone()))?.anchor_command()?;
        let marked = handed.get_envs().any(|(k, v)| k == "SLATES_LAUNCHER" && v.is_some_and(|v| v == "stdin"));
        assert!(marked, "{:?}", handed.get_envs().collect::<Vec<_>>());
        let hex: String = secret.bytes().iter().map(|b| format!("{b:02x}")).collect();
        assert!(handed.get_args().all(|a| !a.to_string_lossy().contains(&hex)), "the secret rides no argument");
        Ok(())
    }

    /// W1 (Slates' contract 14): under systemd the anchor's command is systemd-run making a scope of
    /// its own, named for the instance, that execs exactly the anchor's own command; and the
    /// launcher's secret still rides its standard input (the anchor is told so), never an argument.
    #[test]
    fn w1_under_systemd_the_anchor_runs_in_a_scope_of_its_own_handed_the_secret() -> anyhow::Result<()> {
        let text = |os: &OsStr| os.to_string_lossy().into_owned();
        let secret = LauncherSecret::generate()?;
        let scoped = anchor_command(secret.clone(), true)?;
        assert_eq!(scoped.get_program(), "systemd-run");
        let args: Vec<String> = scoped.get_args().map(text).collect();
        let unit = format!("--unit={}-slates", instance());
        let wrapper = ["--user", "--scope", "--quiet", "--collect", unit.as_str(), "--"];
        assert_eq!(args.get(..wrapper.len()), Some(&wrapper.map(str::to_owned)[..]), "{args:?}");
        let own = anchor_command(secret.clone(), false)?;
        let mut execed = vec![text(own.get_program())];
        execed.extend(own.get_args().map(text));
        assert_eq!(args.get(wrapper.len()..), Some(&execed[..]), "what the scope execs is the anchor's own command");
        let marked = scoped.get_envs().any(|(k, v)| k == "SLATES_LAUNCHER" && v.is_some_and(|v| v == "stdin"));
        assert!(marked, "{:?}", scoped.get_envs().collect::<Vec<_>>());
        let hex: String = secret.bytes().iter().map(|b| format!("{b:02x}")).collect();
        assert!(args.iter().all(|a| !a.contains(&hex)), "the secret rides no argument");
        Ok(())
    }

    /// W1: only a process under the user's own service manager (a runner's user unit) starts its
    /// anchor in a scope: the user's manager can't move a process out of a login session's cgroup
    /// or a system service's. cgroup v2's line alone decides.
    #[test]
    fn w1_only_a_process_under_the_users_service_manager_starts_its_anchor_in_a_scope() {
        let runner = "0::/user.slice/user-1000.slice/user@1000.service/app.slice/kiln-agent.service/kiln\n";
        assert!(in_user_manager(runner));
        let hybrid = "12:pids:/user.slice/user-1000.slice/session-2.scope\n0::/user.slice/user-1000.slice/user@1000.service/app.slice/x.scope\n";
        assert!(in_user_manager(hybrid), "the v2 line, beside v1's");
        for outside in ["0::/user.slice/user-1000.slice/session-3.scope\n", "0::/system.slice/kiln-agent.service\n", "1:name=systemd:/user.slice/user-1000.slice/user@1000.service\n", "0::/\n", ""] {
            assert!(!in_user_manager(outside), "{outside:?}");
        }
    }
}
