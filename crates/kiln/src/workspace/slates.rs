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
//! What kiln keeps across an agent's turns needs nothing to keep it alive. Slates gives a term only
//! to an attachment of its command line's form, which lapses once nothing renews it (about a second
//! idle on one machine; Slates' A-384, whose `slates_client::Hold` renews one), and kiln makes none:
//! a workspace's mount is its file-system bridge's attachment, which lasts until it is unmounted
//! ([`remove`]); a microVM's stream is its guest's, which lasts until its last copy closes
//! (`crate::sandbox::served`); and every other call's client ends with the call.
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
///
/// 23 since kiln took Slates at ca98a952d (from 19). kiln uses 23: its start of the machine's
/// instance asks `StartSpec::until_served` and waits on its anchor while it lives, no wall-clock
/// deadline of its own, a start whose daemons never serve ending `StartFault::NeverServed`
/// ([`ensure_up`]; Slates' A-366). It needs none of the rest: signed landings (20: a
/// `LandingApproval` carries the statement a signer signs and the signature it carried, which kiln
/// sends none of), a takeover past its volume's tolerance (21) and a reply held for durability as a
/// typed retry (22, `RetryCause::DurabilityPending`, which kiln shows in Slates' words as before).
///
/// 36 since kiln took Slates at e418f5cd2 (from 23). kiln uses 36: an ended agent's mount that
/// something still holds once its patience is spent is let go on every Unix host
/// (`mount::unmount_busy`: detached on Linux, forced on macOS), where 23's `unmount_lazy` was
/// Linux's alone and a Mac kept the mount ([`remove`]). It shows two counts the contract added: the
/// drifted entries too long for any reply to name (26, `DriftReport::elided`, [`drift_lines`]) and
/// a written landing's degraded cells past what one reply holds (27,
/// `LandingReport::degraded_elided`, `report_json`). It needs none of the rest: the failover
/// estimate in the daemon's status (24), the chosen-path launcher (25), paged lists (26, 29:
/// `Instance::drift` reads every page itself, in path order), a rewitness's and a stale disk's
/// counts (27), a merge's replaced verdict (28), a granted landing's audit trail (30, 32, 35), the
/// async form (31: kiln's calls run on its blocking pool, `call`), a volume's move to another
/// fleet member (33) and a captured base (34); their refusals kiln shows in Slates' words. The
/// revision brings Slates' A-384 too, a term for a command line's attachment, which kiln makes
/// none of (module docs).
///
/// 40 since kiln took Slates at b2909fcf5 (from 36; kiln goal 95101ef3, K5). kiln uses what 33 and
/// 34 added since: a volume's move to another fleet member (33, `workspace::slates_move`, K4) and
/// a captured base (34: every workspace is a clone of its folder's captured base, [`make`], so it is
/// a plain volume a move takes), with Slates' A-424 behind it (a captured clone's landing judged
/// against its capture by content); and 39, its fleet manifest read by Slates' own reader before
/// its instance starts on it (`role::check_fleet`, which kiln carried as a recorded change of its
/// own until this revision). It maps 37's `MountMissing::MountManager` (Windows) to its own
/// can't-mount refusal, and needs none of the rest: a snapshot's archive exported (38) and a volume
/// imported from one (40).
///
/// 41 since kiln took Slates at 1f332da55 (from 40; Slates' A-496). kiln uses 41: a file whose
/// mode or Windows attribute bits alone changed lands as a `chmod` entry (Slates' A-458), which a
/// plan shows (`summary_json`) and counts among the files it lands (`planned_files`), and which a
/// review commits where git keeps the change, its executable bit (`super::review`). 41 changes
/// nothing else. Kiln takes Slates at e2d1bc42b since, the same contract: Slates' A-503 lands a
/// captured clone that moved against its capture, so kiln lands a workspace that moved here as any.
pub const CONTRACT: u32 = 41;
/// The word that makes kiln's executable one of Slates' processes rather than kiln: `kiln slates-role
/// --instance <name> anchor --shards <n>` is an instance's anchor, and the daemon its anchor starts
/// runs the same way. Every such process carries its instance's name in its arguments.
pub const ROLE_ARG: &str = "slates-role";
/// How long one git command over a workspace may take.
const GIT_TIMEOUT: Duration = Duration::from_secs(60);
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
/// `fusermount3`, no privilege), `nfs` (macOS, on loopback) or `winfsp` (Windows, a drive letter of
/// its own: `super::place`). Matched whole: a kind the contract adds fails the build here until kiln
/// says how it mounts it.
fn mount_word(kind: MountKind) -> Option<&'static str> {
    match kind {
        MountKind::Fuse => Some("fuse"),
        MountKind::NfsLoopback => Some("nfs"),
        MountKind::WinFsp => Some("winfsp"),
    }
}

/// How a volume is mounted here, as Slates' library finds it, when kiln mounts that kind
/// (`mount_word`).
pub fn mount_kind() -> Option<&'static str> {
    slates_api::mount::available().ok().and_then(mount_word)
}

/// What a machine says of its workspaces, in its facts.
pub type Facts = kiln_ci::facts::Workspaces;
/// What a machine lacks for workspaces, typed ([`Facts::missing`]).
pub use kiln_ci::facts::WorkspaceMissing;

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
        Err(fault) => return Facts { slates, why_not: Some(fault.to_string()), missing: Some(missing_of(&fault)), ..Facts::default() },
    };
    let Some(mount) = mount else {
        return Facts { slates, why_not: Some("Slates offers no mount here that kiln makes".to_owned()), missing: Some(WorkspaceMissing::Mount), ..Facts::default() };
    };
    if HERE.get().is_none() {
        return Facts { slates, mount: Some(mount.to_owned()), why_not: Some("this kiln keeps no workspaces".to_owned()), missing: Some(WorkspaceMissing::Workspaces), ..Facts::default() };
    }
    Facts { available: true, slates, mount: Some(mount.to_owned()), running: up().await, why_not: None, missing: None }
}

/// What a machine lacks for workspaces, as Slates' typed answer to whether it can mount names it (`mount::available`).
fn missing_of(fault: &slates_api::MountFault) -> WorkspaceMissing {
    match fault {
        slates_api::MountFault::Unsupported { missing } => match missing {
            slates_api::MountMissing::Fusermount3 => WorkspaceMissing::Fusermount3,
            slates_api::MountMissing::FuseDevice => WorkspaceMissing::FuseDevice,
            slates_api::MountMissing::WinFsp => WorkspaceMissing::WinFsp,
            slates_api::MountMissing::MountNfs => WorkspaceMissing::MountNfs,
            slates_api::MountMissing::SharedMount | slates_api::MountMissing::WindowsSubtreeOrShared | slates_api::MountMissing::MountManager => WorkspaceMissing::Mount,
        },
        _ => WorkspaceMissing::Mount,
    }
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
/// as a node of kiln's Slates fleet where the operator chose this runner (`fleet`, its manifest
/// checked by Slates' own reader: `super::slates_fleet::join`) and an instance of its own
/// otherwise, handed `launcher`'s secret when there is one, and asked to stop if none of its
/// daemons ever serves (`StartSpec::until_served`, Slates' A-366): kiln mounts nothing on it before
/// [`ensure_up`]'s wait and first call succeed, so no mount can wait on an instance that never
/// answered, and that wait needs no deadline of its own.
fn start_spec(launcher: Option<LauncherSecret>, fleet: Option<slates_api::role::FleetSelection>) -> anyhow::Result<StartSpec> {
    let mut spec = StartSpec::new(&instance(), program()?, &[ROLE_ARG]).until_served();
    spec.shards = Some(SHARDS);
    spec.quick = OVERRIDE.get().is_some_and(|o| o.quick);
    spec.fleet = fleet;
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

/// The anchor's command, handed `launcher`'s secret, a node of `fleet` where there is one: run under
/// [`scope_wrapper`] where it starts under systemd (`scoped`), as itself elsewhere.
fn anchor_command(launcher: LauncherSecret, scoped: bool, fleet: Option<slates_api::role::FleetSelection>) -> anyhow::Result<Command> {
    let spec = start_spec(Some(launcher), fleet)?;
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
fn start_anchor(log: &Path, fleet: Option<slates_api::role::FleetSelection>) -> anyhow::Result<std::process::Child> {
    let mut anchor = anchor_command(launcher_secret()?, starts_in_a_scope(), fleet)?;
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
    // A runner the operator chose joins kiln's Slates fleet (`[slates_fleet]`, K3): its place asked of the server, its
    // node's identity, word and manifest made, and the manifest read by Slates' own reader before the instance starts on
    // it. Any other process's instance runs alone. A chosen runner that can't join starts nothing: the next start asks
    // again, rather than starting an instance outside the fleet it was chosen for.
    let joined = match super::slates_fleet::source() {
        Some(source) => super::slates_fleet::join(source).await?,
        None => None,
    };
    let anchor = start_anchor(&h.log, joined.as_ref().map(|j| j.selection.clone()))?;
    let name = instance();
    // The anchor started here is watched while it's waited for, and never after: it outlives this process. The wait
    // lasts while the anchor lives, with no deadline of kiln's: the anchor judges its starting daemon by the daemon's
    // own progress, never a second, so a slow start under load is waited for (a 30 s deadline failed one at load 28,
    // 2026-11-18), and its exit ends the wait: a scope systemd refused, or, asked `until_served`, its daemons' crash
    // loop before any served (Slates' A-366).
    let answered = crate::rt::blocking(move || {
        let mut anchor = anchor;
        Instance::wait_for(&name, Some(&mut anchor), None).map(|_| ())
    })
    .await?;
    // Another of the machine's kiln processes may have started it meanwhile, its anchor this one's
    // refused second.
    if let Err(e) = answered
        && !up().await
    {
        anyhow::bail!("{} (its log: {})", start_refused(&e), h.log.display());
    }
    // A fleet node imports its groups (Slates: never make one there), and says its member id, which a move of a volume to
    // it names (K4): its runner's word again, now with the member its daemon serves as since this start.
    if let (Some(joined), Some(source)) = (&joined, super::slates_fleet::source()) {
        match call("member", |i| i.member()).await {
            Ok(member) => {
                if let Err(e) = super::slates_fleet::report_member(source, &joined.node, member).await {
                    tracing::warn!(error = %format!("{e:#}"), "this runner's Slates node couldn't say its member id: no volume moves to it until it does");
                }
            }
            Err(e) => tracing::warn!(error = %format!("{e:#}"), "this runner's Slates node couldn't read its member id: no volume moves to it until it does"),
        }
        return Ok(());
    }
    // A new instance run alone holds no volumes until its group exists; an existing one is left as it is.
    call("bootstrap", |i| i.ensure_group()).await
}

/// The machine's instance started as kiln's Slates fleet's node now, where the server's `[slates_fleet]` lists this
/// runner (K4): a fleet node holds other runners' copies and takes moved volumes only while its daemon runs, so it isn't
/// left to start with this machine's first workspace. Nothing for a runner not listed.
pub async fn start_if_chosen() -> anyhow::Result<()> {
    let Some(source) = super::slates_fleet::source() else { return Ok(()) };
    if super::slates_fleet::ask_place(source).await?.is_none() {
        return Ok(());
    }
    ensure_up().await
}

/// What kiln says of a start of the machine's instance that ended before it answered (`e`, Slates' refusal of its wait).
fn start_refused(e: &slates_api::Error) -> String {
    match e {
        slates_api::Error::Start(slates_api::StartFault::NeverServed) => {
            "Slates' instance never answered: its daemons failed their starts in a row and none of them served, so its anchor stopped".to_owned()
        }
        other => format!("Slates' anchor ended before its instance answered: {other}"),
    }
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
    /// The captured base its volume is a clone of, on a runner in kiln's Slates fleet (K5, [`make`]); none for an
    /// overlay laid over its folder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(skip))]
    pub base: Option<Base>,
}

/// What a workspace's volume is a clone of (K5): its folder's captured base, an overlay of the folder whose whole tree
/// Slates' capture made its own (`Instance::capture_base`, A-291), and that capture's snapshot.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Base {
    /// The base overlay's volume id (32 hexadecimal digits).
    pub volume: String,
    /// The captured snapshot's id.
    pub snapshot: u64,
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

/// A new workspace `name` over `over`, mounted where its machine mounts workspaces (`super::place`): its folder under the
/// workspaces folder, or on Windows the highest drive letter free, the next one free when Slates refuses that one as
/// another volume's or another start took it meanwhile ([`another_letter_may_serve`]). On a runner in kiln's Slates fleet
/// it's a clone of its folder's captured base, a plain volume Slates moves with its agent (K5, [`provision`]); elsewhere an
/// overlay laid over the folder, which reads nothing of it until it's used.
pub async fn make(name: &str, over: &Path) -> anyhow::Result<Made> {
    ensure_up().await?;
    let (id, base) = provision(name, over, in_fleet().await).await?;
    let volume = slates_api::volume_id_text(id);
    let mut failed = anyhow::anyhow!(super::place::NO_POINT);
    for point in super::place::points(&here()?.root, name) {
        let m = Made { volume: volume.clone(), name: name.to_owned(), mount: point.to_string_lossy().into_owned(), base: base.clone() };
        // Mounted there anew, never taken as mounted already: a drive letter another start took meanwhile holds its
        // volume, not this one, and Slates refuses a second mount at it.
        match mount_typed(&m).await {
            Ok(Ok(())) => return Ok(m),
            // Refused as another volume's letter, or taken meanwhile: the next one; any other refusal ends the search.
            Ok(Err(refused)) => {
                let next = another_letter_may_serve(&refused, super::place::taken(&point));
                failed = anyhow::anyhow!("Slates' mount: {refused}");
                if !next {
                    break;
                }
            }
            // Taken meanwhile (a drive letter another start mounted first): the next one.
            Err(e) if super::place::taken(&point) => failed = e,
            Err(e) => {
                failed = e;
                break;
            }
        }
    }
    match call("volume destroy", move |i| i.destroy(id)).await {
        Ok(()) => {
            if let Some(base) = &base {
                release_base(base, id).await;
            }
        }
        Err(gone) => tracing::warn!(%volume, error = %format!("{gone:#}"), "a workspace that couldn't be mounted stayed"),
    }
    Err(failed)
}

/// A new workspace `name` laid over `over` with no mount (kiln goal 01a1ec34599974448f6b7663ddb07001): an isolated
/// session's, whose microVM Slates serves the volume itself over Slates' own virtio-fs device (`crate::sandbox::served`),
/// so nothing of it is on this machine's disk or in its mount table. Its `mount` is empty.
pub async fn make_unmounted(name: &str, over: &Path) -> anyhow::Result<Made> {
    ensure_up().await?;
    let spec = VolumeSpec::overlay(name, over, volume_max());
    let id = call("volume create", move |i| i.provision(&spec).map(|made| made.volume())).await?;
    Ok(Made { volume: slates_api::volume_id_text(id), name: name.to_owned(), mount: String::new(), base: None })
}

/// A workspace's volume `name` over folder `over`, unmounted (K5): with `captured`, a clone of the folder's captured base
/// ([`base_for`]), chosen and cloned while no other start or end of this process takes or lets go of a base
/// ([`bases_held`]), so no base is let go between its choice and its clone; else an overlay laid over the folder. Its id,
/// and the base it's a clone of.
pub(super) async fn provision(name: &str, over: &Path, captured: bool) -> anyhow::Result<(VolumeId, Option<Base>)> {
    if !captured {
        let spec = VolumeSpec::overlay(name, over, volume_max());
        let id = call("volume create", move |i| i.provision(&spec).map(|made| made.volume())).await?;
        return Ok((id, None));
    }
    let _held = bases_held().lock().await;
    let base = base_for(over).await?;
    let spec = VolumeSpec::clone_of(name, volume_id(&base.volume)?, slates_api::SnapshotId { value: base.snapshot });
    match call("volume create", move |i| i.provision(&spec).map(|made| made.volume())).await {
        Ok(id) => Ok((id, Some(base))),
        Err(e) => {
            // The base may have gone meanwhile (its last clone, moved to another runner, ended there): the next start
            // captures the folder again, and a base no clone holds goes now.
            let_go_of_base(&base).await;
            forget(&base);
            Err(e)
        }
    }
}

/// Whether this runner is in kiln's Slates fleet (the server's `[slates_fleet]` lists it, K3): where its workspaces are
/// captured clones a move takes (K5). Asked of the server at each workspace's start, as the operator's choice may change;
/// a server that can't be asked says no, and the workspace is an overlay, which a move refuses by kind (`not_plain`).
async fn in_fleet() -> bool {
    let Some(source) = super::slates_fleet::source() else { return false };
    matches!(super::slates_fleet::ask_place(source).await, Ok(Some(_)))
}

/// Each folder's captured base, kept for the next agent over it while one is a clone of it (K5): a base goes with its last
/// clone ([`release_base`]), so nothing kiln captured outlives the workspaces made from it.
fn kept() -> &'static parking_lot::Mutex<std::collections::HashMap<PathBuf, Base>> {
    static KEPT: OnceLock<parking_lot::Mutex<std::collections::HashMap<PathBuf, Base>>> = OnceLock::new();
    KEPT.get_or_init(Default::default)
}

/// `base` kept for its folder no more.
fn forget(base: &Base) {
    kept().lock().retain(|_, held| *held != *base);
}

/// One choice, clone or letting go of a captured base at a time in this process (K5): a start holds it from choosing its
/// base until its clone pins the base's snapshot, and an end from asking Slates to let the snapshot go until the base is
/// destroyed, so Slates' pin decides every time whether a base is still used.
fn bases_held() -> &'static futures::lock::Mutex<()> {
    static HELD: OnceLock<futures::lock::Mutex<()>> = OnceLock::new();
    HELD.get_or_init(|| futures::lock::Mutex::new(()))
}

/// The captured base a workspace over folder `over` is cloned from (K5): the one kept for the folder while the folder is
/// as it was captured ([`as_captured`]), so agents started over one folder share one capture; else a new capture, the one
/// before let go once no clone holds it. Its caller holds [`bases_held`].
async fn base_for(over: &Path) -> anyhow::Result<Base> {
    let held = kept().lock().get(over).cloned();
    if let Some(base) = held {
        if as_captured(&base).await {
            return Ok(base);
        }
        // The folder changed since its capture: the next agents get a new one, and this one goes once no clone holds it.
        forget(&base);
        let_go_of_base(&base).await;
    }
    let base = capture(over).await?;
    kept().lock().insert(over.to_path_buf(), base.clone());
    Ok(base)
}

/// Whether the folder under captured base `base` is as it was captured: Slates re-checks every entry the capture
/// witnessed against the disk (its base overlay's drift) and finds none drifted or unverified ([`unchanged`]).
async fn as_captured(base: &Base) -> bool {
    let Ok(id) = volume_id(&base.volume) else { return false };
    matches!(call("status", move |i| i.drift(id)).await, Ok(Some(report)) if unchanged(&report))
}

/// Whether a base overlay's drift `report` says its folder is as the volume witnessed it: nothing drifted, nothing
/// unverified, nothing past what a reply carries.
fn unchanged(report: &DriftReport) -> bool {
    report.drifted.is_empty() && report.unverified.is_empty() && report.elided == 0
}

/// How many captures of a folder are asked before kiln says it's being written: each one after the first follows a file
/// that changed while Slates read it (`RetryCause::ChangedWhileRead`), which a folder at rest never causes. Three covers
/// an editor's save landing during two captures running, as a review's [`super::review`] tries do; a folder a build or a
/// checkout is writing is refused in Slates' words, and the agent starts once it's quiet.
const CAPTURE_TRIES: u32 = 3;

/// Folder `over` captured as a base (K5): an overlay laid over it, its whole tree made the volume's own and frozen into a
/// snapshot (Slates' `capture_base`, A-291), each pause before a capture asked again Slates' own; any refusal destroys the
/// overlay. Never a copy through a disk: Slates reads the folder, and nothing is written to it.
async fn capture(over: &Path) -> anyhow::Result<Base> {
    let name = format!("kiln-base-{}", crate::util::random_token().chars().filter(char::is_ascii_alphanumeric).take(16).collect::<String>().to_ascii_lowercase());
    let spec = VolumeSpec::overlay(&name, over, volume_max());
    let id = call("base create", move |i| i.provision(&spec).map(|made| made.volume())).await?;
    let mut captured = call("base capture", move |i| Ok(i.capture_base(id))).await;
    for _ in 1..CAPTURE_TRIES {
        let Ok(Err(slates_api::Error::Retry { pause_ns, .. })) = &captured else { break };
        pause_for_move(Duration::from_nanos(*pause_ns)).await;
        captured = call("base capture", move |i| Ok(i.capture_base(id))).await;
    }
    let failed = match captured {
        Ok(Ok(snapshot)) => return Ok(Base { volume: slates_api::volume_id_text(id), snapshot: snapshot.value }),
        Ok(Err(refused)) => anyhow::anyhow!("Slates' capture of {}: {refused}", over.display()),
        Err(e) => e,
    };
    if let Err(gone) = call("base destroy", move |i| i.destroy(id)).await {
        tracing::warn!(base = %slates_api::volume_id_text(id), error = %format!("{gone:#}"), "a folder's base whose capture failed stayed");
    }
    Err(failed)
}

/// Destroyed workspace volume `clone`'s base let go, unless another workspace is a clone of it still (K5): once Slates has
/// finished the clone's destroy ([`destroyed`]), which gives the clone's pin on the base's snapshot back, the snapshot and
/// then the base are destroyed ([`let_go_of_base`]). Where the destroy can't be followed, the base stays, said in the log.
async fn release_base(base: &Base, clone: VolumeId) {
    if !destroyed(clone).await {
        tracing::warn!(base = %base.volume, volume = %slates_api::volume_id_text(clone), "a workspace's base stayed: its clone's destroy couldn't be followed to its end");
        return;
    }
    let _held = bases_held().lock().await;
    let_go_of_base(base).await;
}

/// Whether volume `clone`'s destroy is finished: Slates answers it missing. While its slices run Slates answers it
/// `Destroying` (or asks for a pause), and each pause is the one the library derives: Slates bounds the destroy itself
/// (sliced on its owner, and finished after a restart by its recovery), so no clock of kiln's ends the wait. Any other
/// answer, or none, is no.
async fn destroyed(clone: VolumeId) -> bool {
    let mut pause_ns = 0;
    loop {
        let previous = pause_ns;
        match call("status", move |i| Ok((i.status(clone).map(|_| ()), i.retry_pause_ns(previous)))).await {
            Ok((Err(slates_api::Error::NotFound(_)), _)) => return true,
            Ok((Err(slates_api::Error::Unusable(slates_api::Unusable::Destroying)), next)) => pause_ns = next,
            Ok((Err(slates_api::Error::Retry { pause_ns: asked, .. }), _)) => pause_ns = asked,
            _ => return false,
        }
        pause_for_move(Duration::from_nanos(pause_ns)).await;
    }
}

/// Captured base `base` destroyed, its snapshot first, unless Slates refuses the snapshot's destroy as a clone pins it
/// still (another workspace is a clone of it: its end lets the base go). Its caller holds [`bases_held`].
async fn let_go_of_base(base: &Base) {
    let Ok(id) = volume_id(&base.volume) else { return };
    let snapshot = slates_api::SnapshotId { value: base.snapshot };
    match call("base snapshot destroy", move |i| Ok(i.destroy_snapshot(id, snapshot))).await {
        Ok(Ok(()) | Err(slates_api::Error::NotFound(_))) => {}
        Ok(Err(slates_api::Error::Busy(_))) => return,
        Ok(Err(refused)) => {
            tracing::warn!(base = %base.volume, error = %refused, "a workspace's base stayed: Slates refused its capture's destroy");
            return;
        }
        Err(e) => {
            tracing::warn!(base = %base.volume, error = %format!("{e:#}"), "a workspace's base stayed: its capture's destroy wasn't answered");
            return;
        }
    }
    forget(base);
    match call("base destroy", move |i| Ok(i.destroy(id))).await {
        Ok(Ok(()) | Err(slates_api::Error::NotFound(_))) => {}
        Ok(Err(refused)) => tracing::warn!(base = %base.volume, error = %refused, "a workspace's base stayed: Slates refused its destroy"),
        Err(e) => tracing::warn!(base = %base.volume, error = %format!("{e:#}"), "a workspace's base stayed: its destroy wasn't answered"),
    }
}

/// Whether a volume is mounted at `dir` (`super::place::is_mount`: a folder's device, a drive's file system).
fn is_mount(dir: &Path) -> bool {
    super::place::is_mount(dir)
}

/// Workspace `m` is mounted where it says (again, after the machine's mount went away while its
/// volume stayed).
pub async fn mounted(m: &Made) -> anyhow::Result<()> {
    // One made with no mount (an isolated session's, [`make_unmounted`]) has none to make again.
    if m.mount.is_empty() {
        return Ok(());
    }
    if mounted_as_made(m).await? {
        return Ok(());
    }
    mount_at(m).await
}

/// Workspace `m` as this machine places it ([`placed_under`] its place for workspaces): a workspace that arrived here
/// with its agent (K4). A captured clone that moved lands here against its capture as on the runner it came from: its
/// move carried what Slates judges by (Slates' A-503).
pub fn placed_here(m: &Made) -> anyhow::Result<Made> {
    placed_under(m, &here()?.root)
}

/// Workspace `m` as this machine places it, its place for workspaces `root`: where its mount names another machine's
/// place for workspaces (it moved here with its agent, K4), its own place under `root` ([`super::place::points`]); as it
/// was otherwise.
fn placed_under(m: &Made, root: &Path) -> anyhow::Result<Made> {
    let mount = Path::new(&m.mount);
    if m.mount.is_empty() || mount.starts_with(root) || mount.starts_with(super::place::real(root)) {
        return Ok(m.clone());
    }
    let point = super::place::points(root, &m.name).into_iter().next().ok_or_else(|| anyhow::anyhow!(super::place::NO_POINT))?;
    Ok(Made { mount: point.to_string_lossy().into_owned(), ..m.clone() })
}

/// [`mounted`], waiting while Slates asks for a pause (a volume that just moved here is served once its new owner has
/// adopted it: until then its verbs are refused for now, `Error::Retry`), each pause the one Slates derived: Slates
/// bounds the adoption itself, so no clock of kiln's ends the wait.
pub async fn mounted_patiently(m: &Made) -> anyhow::Result<()> {
    if m.mount.is_empty() || mounted_as_made(m).await? {
        return Ok(());
    }
    loop {
        match mount_typed(m).await? {
            Ok(()) => return Ok(()),
            Err(slates_api::Error::Retry { pause_ns, .. }) => pause_for_move(Duration::from_nanos(pause_ns)).await,
            Err(refused) => anyhow::bail!("Slates' mount: {refused}"),
        }
    }
}

/// Workspace `m`'s mount let go before its volume moves (K4): its agent's harness is down, so nothing of it holds the
/// mount; one still held is refused, and the volume stays.
pub(super) async fn unmount_for_move(m: &Made) -> anyhow::Result<()> {
    if m.mount.is_empty() || !mounted_as_made(m).await? {
        return Ok(());
    }
    unmount(m).await?;
    anyhow::ensure!(!mounted_as_made(m).await?, "its mount at {} is still held", m.mount);
    Ok(())
}

/// One ask of volume `volume`'s move to fleet member `member` (Slates' `Instance::move_volume`, contract 33): its typed
/// answer, apart from a failure to reach Slates at all.
pub(super) async fn ask_move(volume: &str, member: u64) -> anyhow::Result<Result<slates_api::MoveProgress, slates_api::Error>> {
    let id = volume_id(volume)?;
    call("move", move |i| Ok(i.move_volume(id, member))).await
}

/// A pause between two asks of a move, of a mount of a volume that just moved here, of a capture, or of a destroy's end.
#[allow(clippy::disallowed_methods)] // (c) Slates' own derived pause between two asks: no notification of the end
pub(super) async fn pause_for_move(pause: Duration) {
    crate::rt::sleep(pause).await;
}

/// Whether workspace `m`'s own volume is mounted where it says: the kernel's mount table holds a mount at its folder
/// ([`mounted_now`]; Unix: no other workspace mounts at a workspace's folder); on Windows its drive letter holds a
/// Slates drive that its volume reports ([`holds_own_drive`]), since a letter is any workspace's to take.
async fn mounted_as_made(m: &Made) -> anyhow::Result<bool> {
    let point = PathBuf::from(&m.mount);
    if !cfg!(windows) {
        return Ok(mounted_now(&point).await);
    }
    let volume = volume_id(&m.volume)?;
    let own = call("status", move |i| i.status(volume)).await?.mounts;
    Ok(holds_own_drive(is_mount(&point), &own, &m.mount))
}

/// Whether drive letter `point` holds its workspace's own volume (Windows): the drive is Slates' (`drive_is_slates`)
/// and the volume's own mounts as Slates reports them (`own`) name it. A letter is any workspace's to take, so a drive
/// this volume left behind and another workspace's took since is never taken for its own: neither used as its mount at
/// a restart nor taken down at its end.
fn holds_own_drive(drive_is_slates: bool, own: &[slates_api::MountReport], point: &str) -> bool {
    drive_is_slates && attachment_at(own, point).is_some()
}

/// The attachment of `own` (a volume's mounts, as Slates reports them) that serves drive letter `point`, in either
/// case: what to detach to take that drive down, and nothing of another volume's.
fn attachment_at(own: &[slates_api::MountReport], point: &str) -> Option<u64> {
    own.iter().find(|mount| mount.path.eq_ignore_ascii_case(point)).map(|mount| mount.attachment)
}

/// Mounts workspace `m` at its point, which holds nothing of it now: a folder made for it (Unix), or a drive letter
/// the mount itself makes (Windows). [`mount_typed`], its refusal in Slates' words.
async fn mount_at(m: &Made) -> anyhow::Result<()> {
    mount_typed(m).await?.map_err(|refused| anyhow::anyhow!("Slates' mount: {refused}"))
}

/// [`mount_at`] with Slates' refusal kept typed (`Ok(Err(refused))`), apart from a failure to ask Slates at all, so
/// [`make`] can tell a drive letter Slates refuses as another volume's from a refusal every letter would meet.
async fn mount_typed(m: &Made) -> anyhow::Result<Result<(), slates_api::Error>> {
    let dir = PathBuf::from(&m.mount);
    super::place::ready(&dir)?;
    let volume = volume_id(&m.volume)?;
    let path = m.mount.clone();
    if let Err(refused) = call("mount", move |i| Ok(i.mount(volume, &path, &slates_api::MountOptions::default()))).await? {
        return Ok(Err(refused));
    }
    anyhow::ensure!(is_mount(&dir), "Slates said it mounted {} at {}, but nothing is mounted there", m.name, m.mount);
    Ok(Ok(()))
}

/// Whether a mount refused at a drive letter (`refused`, Slates' typed refusal) leaves the next free letter worth trying:
/// Slates refused it as another volume's (`DriveLetterTaken`, its A-369: the Mount Manager has given the letter to
/// another volume, or a definition local to the daemon's session hides it), which this process's own view of the drives
/// can miss; or a drive holds the letter now (`taken_now`: another start mounted there first). Any other refusal is the
/// machine's or the volume's (WinFsp's driver not allowed to make drives, the volume gone), which every letter would
/// meet.
fn another_letter_may_serve(refused: &slates_api::Error, taken_now: bool) -> bool {
    taken_now || matches!(refused, slates_api::Error::Host(slates_api::HostFault::Target(slates_api::TargetRefusal::DriveLetterTaken)))
}

/// Takes workspace `m`'s mount away: with the OS's own unmount, which needs no daemon (Unix); or by
/// ending its own volume's attachment at its drive (Windows, where the drive is the daemon's own:
/// [`attachment_at`]). Never Slates' `Instance::unmount` by the letter there, which detaches
/// whichever volume reports the letter first: another workspace's, where this one's drive went and
/// another's took the letter since.
async fn unmount(m: &Made) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        let path = m.mount.clone();
        crate::rt::blocking(move || slates_api::mount::unmount(&path)).await?.map_err(|e| anyhow::anyhow!("Slates' unmount: {e}"))
    }
    #[cfg(not(unix))]
    {
        let volume = volume_id(&m.volume)?;
        let point = m.mount.clone();
        call("unmount", move |i| match attachment_at(&i.status(volume)?.mounts, &point) {
            Some(attachment) => i.detach(attachment),
            None => Ok(()),
        })
        .await
    }
}

/// Lets go of the mount at `path` though something still uses it (Slates' `mount::unmount_busy`, contract 36), saying
/// how: on Linux the kernel takes it out of the namespace now and finishes once its last user closes
/// ([`slates_api::LetGo::Detached`]); on macOS, which has no detaching unmount and whose loopback mount is `hard`, it is
/// unmounted now and its users' next calls on it fail ([`slates_api::LetGo::Forced`]). Refused on Windows, saying why:
/// a workspace's drive is its daemon's own, which [`unmount`] detaches.
async fn let_go(path: String) -> anyhow::Result<slates_api::LetGo> {
    #[cfg(unix)]
    {
        crate::rt::blocking(move || slates_api::mount::unmount_busy(&path)).await?.map_err(|e| anyhow::anyhow!("Slates' unmount: {e}"))
    }
    #[cfg(not(unix))]
    {
        anyhow::bail!("letting go of a mount still in use is the Unix hosts': {path} is a drive its daemon detaches")
    }
}

/// Whether workspace mount `dir` is still there as its removal asks: the kernel's mount table's word, read without
/// touching the mount (`super::place::listed`), never a stat of a hard NFS mount, which waits on its server for as long
/// as that can't answer and reads any refusal as no mount; on Windows, the drive's file system. A table that can't be
/// read counts it mounted, so its removal asks to take it down rather than leave it unasked.
async fn mounted_now(dir: &Path) -> bool {
    #[cfg(unix)]
    {
        let point = dir.to_path_buf();
        match crate::rt::blocking(move || super::place::listed(&point)).await.and_then(|listed| listed) {
            Ok(listed) => listed,
            Err(e) => {
                tracing::info!(mount = %dir.display(), error = %format!("{e:#}"), "the mount table couldn't be read: a workspace's mount is taken as there");
                true
            }
        }
    }
    #[cfg(not(unix))]
    {
        is_mount(dir)
    }
}

/// What came of taking a workspace's mount down ([`unmount_patiently`]).
#[derive(Debug, PartialEq, Eq)]
enum Unmounted {
    /// It's gone.
    Gone,
    /// Still there once the patience ran out: the last refusal, in its words.
    StillHeld(String),
}

/// Asks `ask` to take a mount down while `held` says it's still there: again after a pause as long as the last ask took,
/// doubling, for as long as `patience`. What uses a workspace right after its agent ended (a process of its tree on its
/// way out, the kernel's last reference to a file it had open) goes in about the time its agent's own end took, which is
/// what its caller gives as `patience`; a refusal then isn't the agent's leftovers. The last refusal, where it's still
/// held after that.
async fn unmount_patiently<A, F, H, G>(mut ask: A, mut held: H, patience: Duration) -> Unmounted
where
    A: FnMut() -> F,
    F: std::future::Future<Output = anyhow::Result<()>>,
    H: FnMut() -> G,
    G: std::future::Future<Output = bool>,
{
    let began = Instant::now();
    let mut pause: Option<Duration> = None;
    loop {
        let asked = Instant::now();
        // Gone is the mount table's word, never the unmount's alone: one that answers done while the table still holds
        // the mount is asked again, as a refused one is.
        let refused = match ask().await {
            Ok(()) if !held().await => return Unmounted::Gone,
            Ok(()) => "the unmount answered done, but the mount table still holds it".to_owned(),
            Err(e) => format!("{e:#}"),
        };
        if !held().await {
            return Unmounted::Gone;
        }
        let took = asked.elapsed();
        let next = pause.map_or(took, |p| p.saturating_mul(2)).max(took);
        if began.elapsed().saturating_add(next) > patience {
            return Unmounted::StillHeld(refused);
        }
        crate::rt::sleep(next).await;
        pause = Some(next);
    }
}

/// What holds the mount at `mount` open, as `lsof` names it (`<pid> <command>`, each), for kiln to say of a workspace
/// that stays mounted; what kept `lsof` from saying, where it couldn't.
#[cfg(unix)]
async fn holders(mount: &str) -> String {
    let mount = mount.to_owned();
    let listed = crate::rt::blocking(move || Command::new("lsof").args(["-F", "pc", "--"]).arg(&mount).output()).await;
    match listed {
        Ok(Ok(out)) => {
            let mut held = Vec::new();
            let mut pid = String::new();
            for line in String::from_utf8_lossy(&out.stdout).lines() {
                match line.split_at_checked(1) {
                    Some(("p", p)) => p.clone_into(&mut pid),
                    Some(("c", command)) => held.push(format!("{pid} {command}")),
                    _ => {}
                }
            }
            if held.is_empty() { "nothing lsof can see".to_owned() } else { held.join(", ") }
        }
        Ok(Err(e)) => format!("lsof couldn't run: {e}"),
        Err(e) => format!("{e:#}"),
    }
}

/// See the Unix arm: Windows has no `lsof`, and a WinFsp drive is the daemon's own to detach.
#[cfg(not(unix))]
async fn holders(_mount: &str) -> String {
    "what holds a drive isn't asked on Windows".to_owned()
}

/// Workspace `m` gone: unmounted (on Windows only a drive its own volume reports, [`mounted_as_made`]), its volume
/// destroyed and its mount point removed. What it held is lost. A mount still in
/// use is asked again for as long as `patience` (its agent's own end took that long: [`unmount_patiently`]); still in
/// use then, it's let go on a Unix host, detached on Linux and forced on macOS ([`let_go`]), kiln saying what held it,
/// and a Windows drive stays mounted, kiln saying so. Its volume is destroyed once the mount has gone, and kept,
/// refused typed ([`MountStays`]), while it stays: never destroyed beneath a live mount.
pub async fn remove(m: &Made, patience: Duration) -> anyhow::Result<()> {
    take_down(&Live { made: m }, &m.mount, patience).await?;
    if !m.mount.is_empty() {
        super::place::clear(Path::new(&m.mount));
    }
    // Its base goes with the last workspace that's a clone of it (K5).
    if let Some(base) = &m.base {
        release_base(base, volume_id(&m.volume)?).await;
    }
    Ok(())
}

/// A workspace's volume kept because its mount is still in the kernel's table after kiln asked it gone, let go
/// included: destroying the volume then would leave a mount with nothing behind it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountStays {
    /// The mount point.
    pub mount: String,
    /// The last refusal of the unmount, in its words.
    pub refused: String,
    /// What the let-go said, where it refused.
    pub let_go: String,
    /// What holds the mount, as `lsof` names it.
    pub holders: String,
}

impl std::fmt::Display for MountStays {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} stays mounted, so its volume is kept: unmount refused ({}), let-go ({}), held by {}", self.mount, self.refused, self.let_go, self.holders)
    }
}

impl std::error::Error for MountStays {}

/// The steps of taking a workspace down ([`take_down`]), each this machine's word: whether its own mount is in the
/// kernel's table (on Windows, its own drive), the OS's unmount, its let-go, what holds the mount, and the volume's
/// destroy. Apart from [`remove`] so a test holds their order.
trait TakeDown {
    async fn mounted(&self) -> bool;
    async fn unmount(&self) -> anyhow::Result<()>;
    async fn let_go(&self) -> anyhow::Result<slates_api::LetGo>;
    async fn holders(&self) -> String;
    async fn destroy(&self) -> anyhow::Result<()>;
}

/// [`TakeDown`] for workspace `made` on this machine.
struct Live<'a> {
    made: &'a Made,
}

impl TakeDown for Live<'_> {
    /// A table or a volume that can't be read counts its mount there, so nothing is destroyed beneath it unseen.
    async fn mounted(&self) -> bool {
        mounted_as_made(self.made).await.unwrap_or(true)
    }

    async fn unmount(&self) -> anyhow::Result<()> {
        unmount(self.made).await
    }

    async fn let_go(&self) -> anyhow::Result<slates_api::LetGo> {
        let_go(self.made.mount.clone()).await
    }

    async fn holders(&self) -> String {
        holders(&self.made.mount).await
    }

    async fn destroy(&self) -> anyhow::Result<()> {
        let volume = volume_id(&self.made.volume)?;
        call("volume destroy", move |i| i.destroy(volume)).await
    }
}

/// Takes a workspace with mount point `mount` down by `steps`: unmounted, asked again for as long as `patience`
/// ([`unmount_patiently`]), let go where still held, then its volume destroyed, only once the kernel's mount table no
/// longer holds the point. A mount that stays keeps its volume, refused typed ([`MountStays`]) and logged: a volume
/// destroyed beneath a live mount leaves the mount with nothing behind it (the macOS bare-metal drive, f3f618f2, kiln
/// 68eda5b1, 2026-11-18).
async fn take_down(steps: &impl TakeDown, mount: &str, patience: Duration) -> anyhow::Result<()> {
    if !mount.is_empty() {
        if steps.mounted().await {
            match unmount_patiently(|| steps.unmount(), || steps.mounted(), patience).await {
                Unmounted::Gone => tracing::info!(mount, "an ended agent's workspace is unmounted"),
                Unmounted::StillHeld(refused) => {
                    let how = steps.let_go().await;
                    if steps.mounted().await {
                        // Never destroyed beneath a mount the table still holds: the mount would stay with nothing
                        // behind it. The volume is kept, and the refusal says why.
                        let holders = steps.holders().await;
                        let let_go = how.err().map(|e| format!("{e:#}")).unwrap_or_default();
                        let stays = MountStays { mount: mount.to_owned(), refused, let_go, holders };
                        tracing::warn!(%stays, ?patience, "an ended agent's workspace stays mounted: what holds it holds it past its agent's end, and its volume is kept");
                        return Err(stays.into());
                    }
                    tracing::info!(mount, %refused, ?patience, let_go = ?how.ok(), "an ended agent's workspace was still in use past its agent's end: let go");
                }
            }
        } else {
            tracing::info!(mount, "an ended agent's workspace had nothing of its own mounted at its point");
        }
    }
    steps.destroy().await
}

/// The files of the folder below workspace `m` that changed on disk after the agent changed them
/// in its workspace, each as kiln shows it (`/README.md (Modified)`, `drift_lines`).
pub async fn drift(m: &Made) -> anyhow::Result<Vec<String>> {
    // A captured clone reads no folder: what changed on disk since its capture is its base overlay's drift (K5).
    let volume = volume_id(drift_volume(m))?;
    let report = call("status", move |i| i.drift(volume)).await?;
    Ok(report.as_ref().map(drift_lines).unwrap_or_default())
}

/// A drift report as kiln shows it: each drifted entry and what the disk did to it, then each entry
/// whose re-check the host refused, as not known to match, then how many more Slates counted but
/// could name in no reply (`DriftReport::elided`, contract 26: a path near the system's longest),
/// so a person never reads a short list as the whole of it.
fn drift_lines(report: &DriftReport) -> Vec<String> {
    let drifted = report.drifted.iter().map(|d| format!("{} ({:?})", d.path, d.kind));
    let unverified = report.unverified.iter().map(|path| format!("{path} (Unverified)"));
    let unnamed = (report.elided > 0).then(|| format!("{} more, each path too long for Slates to name (Unnamed)", report.elided));
    drifted.chain(unverified).chain(unnamed).collect()
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

/// Workspace `m`'s write policy as Slates keeps it (none set: Slates' unset policy, which asks).
#[cfg(test)]
pub(crate) async fn policy(m: &Made) -> anyhow::Result<Option<WritePolicy>> {
    let volume = volume_id(&m.volume)?;
    call("write policy", move |i| i.write_policy(PolicySubject::Volume(volume))).await
}

/// What every landing leaves out: git's own directory.
pub const EXCLUDES: &[&str] = &["/.git"];

/// What a landing of workspace `m` leaves out, each a prefix of its volume: what git ignores there (its build output
/// and dependencies, which kiln never writes into a person's folder), then [`EXCLUDES`]. A folder git doesn't know
/// (or a volume with no mount to ask git in) leaves out [`EXCLUDES`] alone.
pub async fn excludes(m: &Made, over: &str) -> Vec<String> {
    let ignored = match ignores_from(m) {
        IgnoresFrom::Tree(top) => git_ignored(&top).await,
        IgnoresFrom::Volume => super::review::ignored_paths(m, over).await,
    };
    let mut out = match ignored {
        Ok(ignored) => ignored,
        Err(e) => {
            tracing::debug!(error = %format!("{e:#}"), "what git ignores in a workspace couldn't be read: only .git is left out");
            Vec::new()
        }
    };
    out.extend(EXCLUDES.iter().map(|p| (*p).to_owned()));
    out
}

/// Where what git ignores in a workspace is read ([`excludes`]).
#[derive(Debug, PartialEq, Eq)]
enum IgnoresFrom {
    /// The working tree at its mount's top.
    Tree(PathBuf),
    /// The volume itself.
    Volume,
}

/// Where what git ignores in workspace `m` is read: the working tree at its mount, or, with none (an isolated workspace,
/// its volume served to its microVM), the volume itself (`super::review::ignored_paths`: its own `.gitignore` files,
/// the repository's excludes and the user's, over what its landing writes). Never `git -C ""`, which git takes for its
/// own process's folder, the runner's.
fn ignores_from(m: &Made) -> IgnoresFrom {
    if m.mount.is_empty() { IgnoresFrom::Volume } else { IgnoresFrom::Tree(super::place::top(&m.mount)) }
}

/// The volume whose drift says what changed on disk under workspace `m` (K5): its base overlay for a captured clone, whose
/// capture witnessed the folder whole; itself for an overlay.
fn drift_volume(m: &Made) -> &str {
    m.base.as_ref().map_or(&m.volume, |b| &b.volume)
}

/// The landing of workspace `m`'s head into `over`, each of `excludes` left out: one that moved here from another runner
/// is landed as any (Slates judges a moved captured clone against its capture, its A-503).
fn landing(m: &Made, over: &str, excludes: &[String]) -> anyhow::Result<LandingRequest> {
    Ok(LandingRequest { volume: volume_id(&m.volume)?, snapshot: None, target: over.to_owned(), filter: Filter { include: Vec::new(), exclude: excludes.to_vec() } })
}

/// What git ignores in workspace `m` ([`ignored_prefixes`]).
async fn git_ignored(top: &Path) -> anyhow::Result<Vec<String>> {
    let mut c = Command::new("git");
    c.arg("-C").arg(top).args(["status", "--porcelain=v1", "-z", "--ignored=traditional", "--untracked-files=normal"]).env("GIT_TERMINAL_PROMPT", "0");
    let o = crate::proc::output(c, GIT_TIMEOUT).await?;
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
        if names_a_source(record) {
            let _ = fields.next();
        }
    }
    n
}

/// The files a landing's plan would write, create, delete or set the mode of (not counting directories). A mode git keeps
/// no part of (any permission bit but the executable one, a Windows attribute) is counted all the same, where git's
/// count of what changed has no record of it: such a landing is held back for review rather than judged by a count that
/// can't tell it from build output git ignores (Slates' contract 41, `chmod`).
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
/// ([`excludes`]), anything more is something git ignores that was made after kiln asked git (a build still running),
/// which kiln won't write into a person's folder. One with no mount is judged by its landing's entries instead.
pub async fn writable(m: &Made, over: &str, plan: &Value, excludes: &[String]) -> Result<(), String> {
    let planned = planned_files(plan);
    // One with no mount has no working tree for git to look at: what git ignores among what its landing writes, past the
    // `excludes` it was planned with (themselves read from its volume, [`excludes`]), is read from its entries and its
    // own `.gitignore` files (kiln goal 01a1ec34…, `super::review::ignored_writes`).
    if m.mount.is_empty() {
        let ignored = super::review::ignored_writes(m, over, excludes).await.map_err(|e| format!("{e:#}"))?;
        return match ignored {
            0 => Ok(()),
            n => Err(format!("it would also write {n} file{} git ignores (build output, dependencies) into the folder, made since kiln read what git ignores in it: plan it again once the agent's build is done, or put the work up for review instead", if n == 1 { "" } else { "s" })),
        };
    }
    let changed = git_changed(m).await.map_err(|e| format!("{e:#}"))?;
    if planned <= changed {
        return Ok(());
    }
    Err(format!(
        "it would also write {} file{} git doesn't see changed into the folder: what git ignores (build output, dependencies) made since kiln asked git, or a mode git keeps no part of: plan it again once the agent's build is done, or put the work up for review instead",
        planned.saturating_sub(changed),
        if planned.saturating_sub(changed) == 1 { "" } else { "s" }
    ))
}

/// What a landing would write, as kiln shows it (the shape Slates' own JSON had): entries per
/// action, a file whose mode alone it sets as `chmod` (Slates' contract 41), bytes to write, entries
/// the filter left out.
fn summary_json(s: &Summary) -> Value {
    let a = &s.actions;
    let by_action: Vec<Value> = [("create", a.create), ("replace", a.replace), ("delete", a.delete), ("rename", a.rename), ("mkdir", a.mkdir), ("rmdir", a.rmdir), ("symlink", a.symlink), ("clear", a.clear), ("chmod", a.chmod)]
        .into_iter()
        .filter(|(_, count)| *count > 0)
        .map(|(action, count)| json!({"action": action, "count": count}))
        .collect();
    json!({"by_action": by_action, "bytes": s.bytes, "filtered_out": s.filtered_out})
}

/// What reached the disk, as kiln shows it: the landing's counts, how it finished, the durability
/// it reached, each degraded cell it met as far as one reply holds them, and how many more it met
/// (`degraded_elided`, contract 27).
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
        "degraded_elided": r.degraded_elided,
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
/// approve (their paths, which the UI lists, and each typed in `conflicted`, which the server
/// answers as the landing's outcome), how it went when nothing needed an approval, or the policy's
/// refusal.
fn plan_json(o: &LandOutcome) -> Value {
    match o {
        LandOutcome::ApprovalRequired(a) => json!({"grant_required": true, "landing": a.presentation, "manifest": a.manifest_text(), "summary": summary_json(&a.summary), "conflicts": []}),
        LandOutcome::Conflicted(conflicts) => json!({"grant_required": true, "conflicts": conflicts.iter().map(|c| c.path.clone()).collect::<Vec<_>>(), "conflicted": conflicts_json(conflicts)}),
        LandOutcome::Written(report) => json!({"grant_required": false, "outcome": report_json(report)}),
        LandOutcome::Refused(refusal) => json!({"grant_required": true, "refused": refusal_json(refusal)}),
    }
}

/// The landing of workspace `m` into `over` as Slates presents it, never writing whatever the
/// volume's mode (its present-only verb): what review shows, and what an approval binds; what git
/// ignores and git's own directory left out ([`excludes`]).
pub async fn plan(m: &Made, over: &str) -> anyhow::Result<LandOutcome> {
    let excludes = excludes(m, over).await;
    plan_with(m, over, &excludes).await
}

/// [`plan`] with `excludes` left out.
async fn plan_with(m: &Made, over: &str, excludes: &[String]) -> anyhow::Result<LandOutcome> {
    let request = landing(m, over, excludes)?;
    call("land", move |i| i.plan_landing(&request)).await
}

/// What writing workspace `m` back to `over` would do: Slates' presentation (`landing`,
/// `manifest`, `summary`, `conflicts`, `grant_required`), or how it went if nothing needed an
/// approval; and what it leaves out ([`excludes`]).
pub async fn plan_landing(m: &Made, over: &str) -> anyhow::Result<(Value, Vec<String>)> {
    let excludes = excludes(m, over).await;
    let shown = plan_with(m, over, &excludes).await?;
    Ok((plan_json(&shown), excludes))
}

/// Lands workspace `m` into `over` as its write policy says ([`set_policy`]): written at once
/// (bypass), held as an approval request (ask), conflicted, or refused (forbid), Slates' typed
/// outcome either way; what git ignores and git's own directory left out ([`excludes`]).
pub async fn land_by_policy(m: &Made, over: &str) -> anyhow::Result<LandOutcome> {
    let excludes = excludes(m, over).await;
    let request = landing(m, over, &excludes)?;
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
    // The plan the person saw left out what git ignored then; asked again, a new ignored path moves the manifest, and
    // nothing is approved.
    let excludes = excludes(m, over).await;
    let request = landing(m, over, &excludes)?;
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
/// longer the one the person saw (the new plan, to show them, and what it leaves out).
#[derive(Debug)]
pub enum Landed {
    Written(Value),
    Changed(Value, Vec<String>),
}

/// Writes workspace `m` back to `over` exactly as the plan whose manifest is `manifest` (the one
/// the person saw) says, when it writes no more than git sees changed ([`writable`]): approved
/// under kiln's authority ([`approve`]). When anything changed, nothing is written and the new plan
/// comes back.
pub async fn land(m: &Made, over: &str, manifest: &str) -> anyhow::Result<Landed> {
    let (shown, excludes) = plan_landing(m, over).await?;
    if shown.get("manifest").and_then(Value::as_str).map(str::to_ascii_lowercase) != Some(manifest.to_ascii_lowercase()) {
        return Ok(match shown.get("grant_required").and_then(Value::as_bool) {
            // Nothing waits on an approval (nothing to write, or it landed as it was).
            Some(false) => Landed::Written(shown),
            _ => Landed::Changed(shown, excludes),
        });
    }
    writable(m, over, &shown, &excludes).await.map_err(anyhow::Error::msg)?;
    Ok(match approve(m, over, manifest).await? {
        LandOutcome::Written(report) => Landed::Written(json!({"grant_required": false, "outcome": report_json(&report)})),
        other => Landed::Changed(plan_json(&other), excludes),
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

    /// W6: a plan that met conflicts carries each conflict typed (`conflicted`: its path and class, as `outcome_json`
    /// names them), beside the paths the UI reads (`conflicts`), and no manifest: the server answers it `conflicted`
    /// from these (`harness::ops::typed_landing`), never as an approval nobody could give.
    #[test]
    fn w6_a_plan_that_met_conflicts_carries_each_typed_and_no_manifest() {
        let met = LandOutcome::Conflicted(vec![LandingConflict { path: "/README.md".into(), class: slates_api::ConflictClass::ModifyModify, planned: None }]);
        let plan = plan_json(&met);
        assert_eq!(plan.get("conflicts"), Some(&json!(["/README.md"])), "{plan}");
        assert_eq!(plan.get("conflicted"), Some(&json!([{"path": "/README.md", "class": "ModifyModify"}])), "{plan}");
        assert_eq!(plan.get("conflicted"), outcome_json(&met).get("conflicts"), "typed as a landing's own outcome types them");
        assert!(plan.get("manifest").is_none(), "{plan}");
    }

    /// W3 (kiln goal 01a1ec34599974448aed2672bafaf9a2): a workspace's volume is never destroyed while the kernel's mount
    /// table still holds its point: a mount kiln's unmount and let-go both leave (they refused, or one answered done and
    /// the table says otherwise) keeps its volume and is refused typed (`MountStays`); one that goes, or none there, takes
    /// its volume with it. The macOS bare-metal drive met it: the Ask agent's volume destroyed, its NFS mount left in the
    /// table (f3f618f2, kiln 68eda5b1, 2026-11-18).
    #[test]
    fn w3_a_volume_is_never_destroyed_while_its_mount_is_in_the_table() -> anyhow::Result<()> {
        use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::Relaxed};
        #[derive(Default)]
        struct Fake {
            mounted: AtomicBool,
            refuses: bool,
            lies: bool,
            unmounts: AtomicU32,
            destroyed: AtomicU32,
        }
        impl TakeDown for Fake {
            async fn mounted(&self) -> bool {
                self.mounted.load(Relaxed)
            }
            async fn unmount(&self) -> anyhow::Result<()> {
                self.unmounts.fetch_add(1, Relaxed);
                match (self.refuses, self.lies) {
                    (true, _) => anyhow::bail!("Slates' unmount: umount couldn't run: No such file or directory (os error 2)"),
                    (false, true) => Ok(()),
                    (false, false) => {
                        self.mounted.store(false, Relaxed);
                        Ok(())
                    }
                }
            }
            async fn let_go(&self) -> anyhow::Result<slates_api::LetGo> {
                if self.refuses || self.lies { anyhow::bail!("Slates' unmount: umount -f couldn't run") } else { Ok(slates_api::LetGo::Forced) }
            }
            async fn holders(&self) -> String {
                "nothing lsof can see".to_owned()
            }
            async fn destroy(&self) -> anyhow::Result<()> {
                self.destroyed.fetch_add(1, Relaxed);
                Ok(())
            }
        }
        crate::rt::block_on(async {
            for (refuses, lies) in [(true, false), (false, true)] {
                let stuck = Fake { mounted: AtomicBool::new(true), refuses, lies, ..Fake::default() };
                let kept = take_down(&stuck, "/state/workspaces/kiln-a", Duration::ZERO).await;
                anyhow::ensure!(stuck.destroyed.load(Relaxed) == 0, "refuses {refuses}, lies {lies}: destroyed under a mount the table holds");
                let stays = kept.as_ref().err().and_then(|e| e.downcast_ref::<MountStays>()).cloned();
                anyhow::ensure!(stays.as_ref().is_some_and(|s| s.mount == "/state/workspaces/kiln-a"), "refuses {refuses}, lies {lies}: {kept:?}");
            }
            let goes = Fake { mounted: AtomicBool::new(true), ..Fake::default() };
            take_down(&goes, "/state/workspaces/kiln-b", Duration::ZERO).await?;
            anyhow::ensure!((goes.unmounts.load(Relaxed), goes.destroyed.load(Relaxed)) == (1, 1), "unmounted once, then destroyed");
            let none = Fake::default();
            take_down(&none, "/state/workspaces/kiln-c", Duration::ZERO).await?;
            anyhow::ensure!((none.unmounts.load(Relaxed), none.destroyed.load(Relaxed)) == (0, 1), "nothing mounted: destroyed, nothing asked");
            Ok(())
        })
    }

    /// W8 (kiln goal 01a1ec34599974448aed2672bafaf9a2): what git ignores in a workspace with no mount (an isolated agent's,
    /// its volume served to its microVM) is read from its volume, never from the tree at its mount's top: with no mount
    /// that's `git -C ""`, which git takes for its own process's folder, the runner's, whose ignores are another
    /// repository's or none (g267, 2026-11-18). A mounted workspace's are read from its working tree.
    #[test]
    fn w8_an_unmounted_workspaces_ignores_are_read_from_its_volume_never_the_runners_folder() {
        let unmounted = Made { volume: "00017f80ff10abcdef123456789abcde".into(), name: "kiln-iso".into(), mount: String::new(), ..Default::default() };
        assert_eq!(ignores_from(&unmounted), IgnoresFrom::Volume);
        let mounted = Made { mount: "/state/workspaces/kiln-a".into(), ..unmounted.clone() };
        assert_eq!(ignores_from(&mounted), IgnoresFrom::Tree(PathBuf::from("/state/workspaces/kiln-a")));
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
        let m = Made { volume: "00017f80ff10abcdef123456789abcde".into(), name: "kiln-a".into(), mount: "/w/kiln-a".into(), ..Default::default() };
        Ok(LandingApproval {
            request: landing(&m, "/src/kiln", &EXCLUDES.iter().map(|p| (*p).to_owned()).collect::<Vec<_>>())?,
            presentation: 7,
            manifest,
            summary: Summary { actions: slates_api::ActionTally { create: 2, mkdir: 1, ..Default::default() }, bytes: 12, filtered_out: 3 },
            verification: slates_api::Verification::Unsigned,
            statement: Vec::new(),
            signature: None,
        })
    }

    /// W3 (the coordinator's macOS proof, 2026-11-18: an ended agent's volume stayed mounted, its first unmount refused
    /// busy and none after it): an ended agent's mount is asked to go again while what used it is on its way out, for as
    /// long as its agent's own end took, each pause as long as the last ask took and doubling; gone at the first ask that
    /// takes it down, or once it's no longer mounted; still held after that, the last refusal comes back for kiln to say
    /// with what holds it; asked once where nothing ran in it.
    #[test]
    fn w3_an_ended_agents_mount_is_asked_again_while_what_used_it_goes() -> anyhow::Result<()> {
        use std::sync::atomic::{AtomicU32, Ordering};
        crate::rt::block_on(async {
            let asks = AtomicU32::new(0);
            let busy_until = |free_at: u32| {
                let n = asks.fetch_add(1, Ordering::Relaxed);
                async move { if n < free_at { anyhow::bail!("Resource busy") } else { Ok(()) } }
            };
            // Mounted until an ask has taken it down: the table's word follows the asks.
            let up_until = |free_at: u32| asks.load(Ordering::Relaxed) <= free_at;
            let gone = unmount_patiently(|| busy_until(2), || async { up_until(2) }, Duration::from_secs(60)).await;
            anyhow::ensure!(gone == Unmounted::Gone && asks.load(Ordering::Relaxed) == 3, "taken down at its third ask: {gone:?} after {asks:?}");
            asks.store(0, Ordering::Relaxed);
            // An unmount that answers done while the table still holds the mount isn't believed: asked again, then held.
            let told = unmount_patiently(|| busy_until(0), || async { true }, Duration::from_millis(50)).await;
            anyhow::ensure!(told == Unmounted::StillHeld("the unmount answered done, but the mount table still holds it".into()) && asks.load(Ordering::Relaxed) > 1, "an unmount the table belies: {told:?} after {asks:?}");
            asks.store(0, Ordering::Relaxed);
            let once = unmount_patiently(|| busy_until(u32::MAX), || async { true }, Duration::ZERO).await;
            anyhow::ensure!(once == Unmounted::StillHeld("Resource busy".into()) && asks.load(Ordering::Relaxed) == 1, "no patience: asked once: {once:?} after {asks:?}");
            asks.store(0, Ordering::Relaxed);
            let went = unmount_patiently(|| busy_until(u32::MAX), || async { false }, Duration::from_secs(60)).await;
            anyhow::ensure!(went == Unmounted::Gone && asks.load(Ordering::Relaxed) == 1, "no longer mounted: gone, asked no more: {went:?} after {asks:?}");
            asks.store(0, Ordering::Relaxed);
            let held = unmount_patiently(|| busy_until(u32::MAX), || async { true }, Duration::from_millis(50)).await;
            anyhow::ensure!(held == Unmounted::StillHeld("Resource busy".into()) && asks.load(Ordering::Relaxed) > 1, "asked again, then held: {held:?} after {asks:?}");
            Ok(())
        })
    }

    /// W1 (kiln goal 01a1ec34599974448aed2672bafaf9a2, Windows; Slates' A-369): a drive letter Slates refuses as another
    /// volume's makes a workspace try the next free letter, though no drive holds it in this process's view (the Mount
    /// Manager has given it to a volume that isn't here, or a definition local to the daemon's session hides it); one a
    /// drive took meanwhile does too, whatever Slates said; a refusal every letter would meet ends the search there.
    #[test]
    fn w1_a_drive_letter_slates_refuses_as_another_volumes_moves_on_to_the_next() {
        use slates_api::{Error, HostFault, TargetRefusal};
        let taken = Error::Host(HostFault::Target(TargetRefusal::DriveLetterTaken));
        assert!(another_letter_may_serve(&taken, false), "refused as another volume's, though this process sees the letter free");
        assert!(another_letter_may_serve(&taken, true));
        let not_allowed = Error::Host(HostFault::Target(TargetRefusal::MountManagerNotAllowed));
        assert!(!another_letter_may_serve(&not_allowed, false), "the machine's refusal: every letter meets it");
        assert!(another_letter_may_serve(&not_allowed, true), "a drive took the letter meanwhile: the next one");
        let refused = Error::Host(HostFault::Target(TargetRefusal::MountRefused { errno: Some(5) }));
        assert!(!another_letter_may_serve(&refused, false));
    }

    /// W1 (kiln goal 01a1ec34599974448aed2672bafaf9a2, Windows): a drive letter is any workspace's to take, so a
    /// workspace's drive is its own only when its own volume reports a mount there. A Slates drive at its letter that its
    /// volume doesn't report (its drive went, and another workspace's took the letter since) is neither used as its
    /// mount at a restart nor taken down at its end; the attachment its end detaches is its volume's own at that letter,
    /// in either case. Before, a Slates drive at the letter was taken as the workspace's whatever volume it served.
    #[test]
    fn w1_a_windows_workspaces_drive_is_its_own_only_when_its_volume_reports_it() {
        let at = |attachment, path: &str| slates_api::MountReport { attachment, path: path.to_owned() };
        let own = [at(7, "J:"), at(9, "K:")];
        assert!(holds_own_drive(true, &own, "K:"));
        assert!(holds_own_drive(true, &own, "k:"), "a drive letter in either case");
        assert!(!holds_own_drive(true, &[at(7, "J:")], "K:"), "a Slates drive its volume doesn't report: another workspace's");
        assert!(!holds_own_drive(true, &[], "K:"));
        assert!(!holds_own_drive(false, &own, "K:"), "its volume's record of a drive that isn't there now");
        assert_eq!(attachment_at(&own, "k:"), Some(9), "its own attachment at the letter");
        assert_eq!(attachment_at(&own, "L:"), None, "none of its own there: nothing to detach");
    }

    /// W2: what a machine lacks for workspaces is typed from Slates' own typed answer (`mount::available`), so a start
    /// there is refused naming it; every missing piece Slates names has its kiln name.
    #[test]
    fn w2_what_a_machine_lacks_for_workspaces_is_typed_from_slates_answer() {
        use slates_api::{MountFault, MountMissing};
        let of = |missing| missing_of(&MountFault::Unsupported { missing });
        assert_eq!(of(MountMissing::Fusermount3), WorkspaceMissing::Fusermount3);
        assert_eq!(of(MountMissing::FuseDevice), WorkspaceMissing::FuseDevice);
        assert_eq!(of(MountMissing::WinFsp), WorkspaceMissing::WinFsp);
        assert_eq!(of(MountMissing::MountNfs), WorkspaceMissing::MountNfs);
        assert_eq!(missing_of(&MountFault::NoNfsListener), WorkspaceMissing::Mount, "any other refusal: a mount kiln makes");
    }

    /// W1: kiln speaks the contract it was written against; a re-vendor that moves it fails here, beside the matches
    /// it must extend.
    #[test]
    fn w1_kiln_speaks_the_contract_it_was_written_against() {
        assert_eq!(slates_api::CONTRACT, CONTRACT);
    }

    /// W1 (Slates' A-366): kiln's start of its machine's instance asks the anchor to stop if none of its daemons ever
    /// serves, so its wait on the anchor needs no deadline of its own; a start that ends so says so in its own words,
    /// apart from an anchor that ended for another reason.
    #[test]
    fn w1_kiln_waits_on_its_anchor_while_it_lives_and_hears_a_start_that_never_served() -> anyhow::Result<()> {
        assert!(start_spec(None, None)?.until_served, "kiln asks until_served");
        let never = start_refused(&slates_api::Error::Start(slates_api::StartFault::NeverServed));
        assert!(never.contains("never answered") && never.contains("none of them served"), "{never}");
        let ended = start_refused(&slates_api::Error::Start(slates_api::StartFault::AnchorExited { code: Some(4) }));
        assert!(ended.contains("anchor ended") && ended.contains("code 4"), "{ended}");
        Ok(())
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

    /// W6 (Slates' contract 41, A-458): a file whose mode alone changes shows in a plan as `chmod`, after the actions
    /// kiln always showed, and counts among the files the plan lands, so a person sees every entry they approve, and a
    /// mode git keeps no part of holds the landing back for review rather than passing as no change.
    #[test]
    fn w6_a_plan_shows_a_file_whose_mode_alone_changes_and_counts_it() -> anyhow::Result<()> {
        let mut shown = approval([0x3c; 32])?;
        shown.summary.actions.chmod = 2;
        let plan = plan_json(&LandOutcome::ApprovalRequired(shown));
        assert_eq!(plan.pointer("/summary/by_action"), Some(&json!([{"action": "create", "count": 2}, {"action": "mkdir", "count": 1}, {"action": "chmod", "count": 2}])));
        assert_eq!(planned_files(&plan), 4, "two files written and two whose mode alone it sets; the directory not counted");
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
        let report = DriftReport { drifted: vec![slates_api::Drift { path: "/README.md".into(), kind: slates_api::DriftKind::Modified }], unverified: vec!["/b.rs".into()], watcher: slates_api::Watcher::Unavailable, elided: 0 };
        assert_eq!(drift_lines(&report), ["/README.md (Modified)", "/b.rs (Unverified)"]);
        assert!(drift_lines(&DriftReport { drifted: Vec::new(), unverified: Vec::new(), watcher: slates_api::Watcher::Live, elided: 0 }).is_empty());
    }

    /// W4 (Slates' contract 26): drifted entries Slates counted but could name in no reply (a path near the system's
    /// longest) show as one line counting them after the named ones, so a short list never passes for the whole; a
    /// report with none counted shows no such line.
    #[test]
    fn w4_drift_counts_the_entries_slates_could_not_name() {
        let report = DriftReport { drifted: vec![slates_api::Drift { path: "/a.rs".into(), kind: slates_api::DriftKind::Deleted }], unverified: Vec::new(), watcher: slates_api::Watcher::Live, elided: 2 };
        assert_eq!(drift_lines(&report), ["/a.rs (Deleted)", "2 more, each path too long for Slates to name (Unnamed)"]);
        let only = DriftReport { drifted: Vec::new(), unverified: Vec::new(), watcher: slates_api::Watcher::Live, elided: 1 };
        assert_eq!(drift_lines(&only), ["1 more, each path too long for Slates to name (Unnamed)"], "drift even when none could be named");
    }

    /// W6 (Slates' contract 27): a written landing shows each degraded cell Slates named, and counts those past what one
    /// reply holds, so a landing that met more than its report lists never reads as having met only those.
    #[test]
    fn w6_a_written_landing_shows_its_degraded_cells_and_counts_the_rest() {
        let durability = slates_api::LandingDurability { data_synced: true, dirs_synced: false, media: false, media_requested: false, dirs: 3 };
        let report = LandingReport {
            landing: 9,
            state: slates_api::LandingState::Done,
            written: 4,
            skipped: 1,
            conflicts: 0,
            failed: 0,
            bytes_written: 53,
            held: 2,
            durability,
            degraded: vec![slates_api::LandingDegradation::BarriersOnly],
            degraded_elided: 5,
            barriers: 1,
            verification: slates_api::Verification::Unsigned,
        };
        let shown = outcome_json(&LandOutcome::Written(report));
        assert_eq!(shown.get("outcome"), Some(&json!("written")));
        assert_eq!(shown.pointer("/written/degraded"), Some(&json!(["BarriersOnly"])));
        assert_eq!(shown.pointer("/written/degraded_elided"), Some(&json!(5)));
        assert_eq!(shown.pointer("/written/written"), Some(&json!(4)));
        assert_eq!(shown.pointer("/written/held"), Some(&json!(2)));
        assert_eq!(shown.pointer("/written/durability/dirs"), Some(&json!(3)));
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

    /// W1: the facts name the mounts kiln makes by the library's word, a WinFsp drive among them.
    #[test]
    fn w1_the_facts_name_the_mount_the_library_offers() {
        assert_eq!(mount_word(MountKind::Fuse), Some("fuse"));
        assert_eq!(mount_word(MountKind::NfsLoopback), Some("nfs"));
        assert_eq!(mount_word(MountKind::WinFsp), Some("winfsp"), "at a drive letter of its own (super::place)");
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
        let spec = start_spec(None, None)?;
        assert_eq!(spec.prefix, vec![ROLE_ARG.to_owned()]);
        assert_eq!(spec.shards, Some(SHARDS));
        assert!(spec.fleet.is_none(), "a machine the operator didn't choose runs its instance alone");
        let command = spec.anchor_command()?;
        let args: Vec<String> = command.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(args.first().map(String::as_str), Some(ROLE_ARG), "{args:?}");
        assert!(args.windows(2).any(|w| matches!(w, [flag, name] if flag == "--instance" && *name == spec.instance)), "{args:?}");
        assert!(args.iter().any(|a| a == "anchor"), "{args:?}");
        // With kiln's secret, the anchor is told its standard input carries it (and the secret is in no argument).
        let secret = LauncherSecret::generate()?;
        let handed = start_spec(Some(secret.clone()), None)?.anchor_command()?;
        let marked = handed.get_envs().any(|(k, v)| k == "SLATES_LAUNCHER" && v.is_some_and(|v| v == "stdin"));
        assert!(marked, "{:?}", handed.get_envs().collect::<Vec<_>>());
        let hex: String = secret.bytes().iter().map(|b| format!("{b:02x}")).collect();
        assert!(handed.get_args().all(|a| !a.to_string_lossy().contains(&hex)), "the secret rides no argument");
        Ok(())
    }

    /// K4 (kiln goal 95101ef32ffb8b099942fe076619c310): a workspace that moved here with its agent names the other
    /// machine's place for workspaces; it's placed under this machine's own, by its name, its volume the same; one placed
    /// here already, and one with no mount (an isolated session's), stay as they are.
    #[cfg(unix)]
    #[test]
    fn k4_a_workspace_that_moved_here_is_placed_under_this_machines_own_place() -> anyhow::Result<()> {
        let scratch = tempfile::tempdir()?;
        let root = scratch.path().join("workspaces");
        let moved = Made { volume: "00017f80ff10abcdef123456789abcde".into(), name: "kiln-a".into(), mount: "/another/machines/state/kiln/workspaces/kiln-a".into(), ..Default::default() };
        let placed = placed_under(&moved, &root)?;
        let here = super::super::place::real(&root).join("kiln-a");
        assert_eq!(placed, Made { mount: here.to_string_lossy().into_owned(), ..moved.clone() });
        assert_eq!(placed_under(&placed, &root)?, placed, "placed here already");
        let unmounted = Made { mount: String::new(), ..moved };
        assert_eq!(placed_under(&unmounted, &root)?, unmounted);
        Ok(())
    }

    /// A captured clone's record, as a fleet runner makes one (K5).
    fn captured() -> Made {
        let base = Some(Base { volume: "00017f80ff10abcdef1234567890ffff".into(), snapshot: 7 });
        Made { volume: "00017f80ff10abcdef123456789abcde".into(), name: "kiln-a".into(), mount: "/another/machines/state/kiln/workspaces/kiln-a".into(), base }
    }

    /// A-503 (kiln goal 95101ef32ffb8b099942fe076619c310): a captured clone that moved here with its agent is placed under
    /// this machine's own place and written back as any workspace is, into its folder with what git ignores left out:
    /// Slates judges it against its capture, which its move carried. Until A-503 kiln refused it.
    #[cfg(unix)]
    #[test]
    fn a503_a_workspace_that_moved_here_is_written_back_as_any_workspace_is() -> anyhow::Result<()> {
        let scratch = tempfile::tempdir()?;
        let root = scratch.path().join("workspaces");
        let came = captured();
        let here = placed_under(&came, &root)?;
        assert_eq!(here, Made { mount: super::super::place::real(&root).join("kiln-a").to_string_lossy().into_owned(), ..came });
        let request = landing(&here, "/src/kiln", &["/.git".to_owned()])?;
        assert_eq!((request.target.as_str(), request.filter.exclude.as_slice()), ("/src/kiln", ["/.git".to_owned()].as_slice()));
        assert_eq!(request.volume, volume_id(&here.volume)?);
        Ok(())
    }

    /// K5: what changed on disk under a captured clone is read from its base, whose capture witnessed the folder whole (the
    /// clone reads no folder); an overlay's from itself.
    #[test]
    fn k5_a_captured_clones_drift_is_read_from_its_base() {
        let clone = captured();
        assert_eq!(drift_volume(&clone), "00017f80ff10abcdef1234567890ffff");
        let overlay = Made { base: None, ..clone };
        assert_eq!(drift_volume(&overlay), "00017f80ff10abcdef123456789abcde");
    }

    /// K5: a folder's capture is shared by the next agent only while Slates finds the folder as it was captured: nothing
    /// drifted, nothing unverified and nothing past what its reply carries; any of them, and the next agent's capture is
    /// new.
    #[test]
    fn k5_a_capture_is_shared_only_while_its_folder_is_as_captured() {
        let quiet = DriftReport { drifted: Vec::new(), unverified: Vec::new(), watcher: slates_api::Watcher::Live, elided: 0 };
        assert!(unchanged(&quiet));
        assert!(unchanged(&DriftReport { watcher: slates_api::Watcher::Unavailable, ..quiet.clone() }), "the watcher's hints are no fact: Slates re-checks every entry");
        let edited = DriftReport { drifted: vec![slates_api::Drift { path: "/README.md".into(), kind: slates_api::DriftKind::Modified }], ..quiet.clone() };
        let unread = DriftReport { unverified: vec!["/src/a.rs".into()], ..quiet.clone() };
        let elided = DriftReport { elided: 1, ..quiet };
        for report in [edited, unread, elided] {
            assert!(!unchanged(&report), "{report:?}");
        }
    }

    /// K5: a workspace's record from before (an overlay's: no base) reads as it did and is written as it was, so a runner
    /// or server of either age reads the other's; a captured clone's carries its base. A record K5's runners wrote with
    /// `moved` (A-503 made the field go) reads as the same workspace.
    #[test]
    fn k5_a_workspace_record_from_before_reads_and_writes_as_it_did() -> anyhow::Result<()> {
        let before = json!({"volume": "00017f80ff10abcdef123456789abcde", "name": "kiln-a", "mount": "/w/kiln-a"});
        let read: Made = serde_json::from_value(before.clone())?;
        assert_eq!(read, Made { volume: "00017f80ff10abcdef123456789abcde".into(), name: "kiln-a".into(), mount: "/w/kiln-a".into(), base: None });
        assert_eq!(serde_json::to_value(&read)?, before);
        let clone = captured();
        let mut written = serde_json::to_value(&clone)?;
        assert_eq!(written.pointer("/base/snapshot"), Some(&json!(7)));
        assert_eq!(serde_json::from_value::<Made>(written.clone())?, clone);
        if let Some(fields) = written.as_object_mut() {
            fields.insert("moved".to_owned(), json!(true));
        }
        assert_eq!(serde_json::from_value::<Made>(written)?, clone, "a K5 runner's moved record");
        Ok(())
    }

    /// K3 (kiln goal 95101ef32ffb8b099942fe076619c310): a runner the operator chose starts its instance as the node its
    /// manifest names: the anchor's command carries the manifest and the machine's name (`--fleet PATH --node NAME`),
    /// as Slates' anchor reads them.
    #[test]
    fn k3_a_chosen_runners_anchor_is_started_as_the_fleets_node() -> anyhow::Result<()> {
        let selection = slates_api::role::FleetSelection { manifest: "/state/slates-fleet/fleet.json".to_owned(), node: "bench-a".to_owned() };
        let spec = start_spec(None, Some(selection.clone()))?;
        assert_eq!(spec.fleet, Some(selection));
        let args: Vec<String> = spec.anchor_command()?.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert!(args.windows(2).any(|w| matches!(w, [flag, path] if flag == "--fleet" && path == "/state/slates-fleet/fleet.json")), "{args:?}");
        assert!(args.windows(2).any(|w| matches!(w, [flag, node] if flag == "--node" && node == "bench-a")), "{args:?}");
        Ok(())
    }

    /// Kiln goal 01a1ec34599974448aed2672bafaf9a2 (W6): a landing leaves out each path git ignores, once per directory
    /// git ignores whole, a rename's source (in either status column) no path of its own; and `.git`; the library's
    /// request carries every one of them.
    #[test]
    fn w6_a_landing_leaves_out_what_git_ignores_and_git_itself() -> anyhow::Result<()> {
        let said = "!! target/\0 M src/a.rs\0R  new.rs\0old.rs\0!! src/gen.o\0?? src/n.rs\0 R moved.rs\0!! was.rs\0!! node_modules/\0";
        assert_eq!(ignored_prefixes(said), ["/target", "/src/gen.o", "/node_modules"]);
        assert!(ignored_prefixes("").is_empty());
        let m = Made { volume: "00017f80ff10abcdef123456789abcde".into(), name: "kiln-a".into(), mount: "/w/kiln-a".into(), ..Default::default() };
        let request = landing(&m, "/src/kiln", &["/target".to_owned(), "/.git".to_owned()])?;
        assert_eq!(request.filter.exclude, ["/target", "/.git"]);
        assert!(request.filter.include.is_empty() && request.snapshot.is_none() && request.target == "/src/kiln");
        Ok(())
    }

    /// W1 (Slates' contract 14): under systemd the anchor's command is systemd-run making a scope of
    /// its own, named for the instance, that execs exactly the anchor's own command; and the
    /// launcher's secret still rides its standard input (the anchor is told so), never an argument.
    #[test]
    fn w1_under_systemd_the_anchor_runs_in_a_scope_of_its_own_handed_the_secret() -> anyhow::Result<()> {
        let text = |os: &OsStr| os.to_string_lossy().into_owned();
        let secret = LauncherSecret::generate()?;
        let scoped = anchor_command(secret.clone(), true, None)?;
        assert_eq!(scoped.get_program(), "systemd-run");
        let args: Vec<String> = scoped.get_args().map(text).collect();
        let unit = format!("--unit={}-slates", instance());
        let wrapper = ["--user", "--scope", "--quiet", "--collect", unit.as_str(), "--"];
        assert_eq!(args.get(..wrapper.len()), Some(&wrapper.map(str::to_owned)[..]), "{args:?}");
        let own = anchor_command(secret.clone(), false, None)?;
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
