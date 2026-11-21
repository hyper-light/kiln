//! Agents' containers on their workspaces' Slates volumes (docs/agentic.md, W9; Slates' A-520): an agent whose runner
//! runs its sessions in containers works in a container of its own on its workspace's volume, which the engine itself
//! mounts from Slates' NFS export of it, never a copy, so its review, drift and landing are W3's, W4's and W6's as they
//! are.
//!
//! **The volume reaches the container as Slates' export of it, mounted by the engine** (Ada's ruling, 2026-11-20): the
//! volume exported to one client, the Unix identity that owns the agent's folder (Slates' `Instance::export`, bound to
//! that uid: A-115, A-521), and an ordinary volume of the engine's own made from the export (Docker's `local` driver,
//! `type=nfs`: the daemon's NFS port, the export's capability path), which the container runs on. It is how the
//! Kubernetes engine has kubelet mount the same export (W10). It replaced binding the machine's own mount of the
//! workspace into the container, which by use hung Docker Desktop's file share over Slates' loopback NFS mount (2026-11-20,
//! a Mac Studio: the engine's run never came back) and needed the mount shared with other users under a rootful Linux
//! engine (`user_allow_other`). One route serves every engine that mounts its own volumes: a rootful Linux engine
//! (Docker Engine, podman as root) mounts it on the machine, at its loopback; Docker Desktop in its VM, which reaches the
//! machine's loopback as `host.docker.internal`. A rootless engine can't mount a file system itself, and is said so.
//!
//! **The order is this module's**, and its unit tests hold it over stand-ins (`Steps`). The engine's profile is judged
//! first, by the caller, before the workspace is made (Slates' `container_engine::export_engine`, A-769: a remote engine is
//! refused, since an export's address and identity are the machine's). Then the export, then the engine's volume over it,
//! then the container on that volume. A start refused part way lets go of what it made, the volume, then the export. Its
//! end goes in order: the container (its engine lets go of the files it held open), then the engine's volume (its mount
//! with it), then the export (its capability reaches nothing after), then W3's mount and volume
//! ([`super::slates::remove`]).
//!
//! **Where the agent works.** The container mounts the volume at the same path the machine mounts it, and its harness
//! starts at the same place in it ([`Plan::workdir`], W3), so nothing is translated. It runs as the folder's owner
//! (`--user uid:gid`), the identity the export serves: Slates answers any other caller, the superuser included, as a
//! stranger (A-115). The engine's mount is `nosuid` and `nodev`, and so are the container's own rules
//! (`--security-opt=no-new-privileges`, `--cap-drop=MKNOD`). The export's path carries its capability, a bearer secret:
//! it reaches the engine as the volume's `device`, on the engine's command line on this machine alone, and is never
//! logged ([`Running`]'s `Debug` and every refusal leave it out).

use std::fmt;

use kiln_ci::facts::{ContainerFacts, ContainerMissing, Workspaces};
use slates_api::container_engine::{Engine, EngineFault, ExportEngine};

use super::slates::Made;

/// Format: the NFS version the engine mounts an export at (Slates serves NFSv4.2: A-71's Docker workload ran over it,
/// and the Kubernetes engine's PersistentVolume asks it).
const NFS_VERSION: &str = "4.2";
/// How many times an engine volume's removal is tried before its release says it stayed: an engine may still count a
/// container it just removed as the volume's user for a moment (Docker Desktop 29.3.1, w-k90-containers' r8).
pub(crate) const VOLUME_RM_TRIES: u32 = 10;
/// How long between an engine volume's removal's tries ([`VOLUME_RM_TRIES`]) on a machine.
pub(crate) const VOLUME_RM_PAUSE_LIVE: std::time::Duration = std::time::Duration::from_millis(500);
/// The pause taken: [`VOLUME_RM_PAUSE_LIVE`]'s, but none to speak of between a stand-in's tries.
#[cfg(not(test))]
const VOLUME_RM_PAUSE: std::time::Duration = VOLUME_RM_PAUSE_LIVE;
/// See above.
#[cfg(test)]
const VOLUME_RM_PAUSE: std::time::Duration = std::time::Duration::from_millis(1);
/// Format: the container option that holds `nosuid`: no process of the container gains a privilege by an exec.
const NO_NEW_PRIVILEGES: &str = "--security-opt=no-new-privileges";
/// Format: the container option that holds `nodev`: no device node is made through the container.
const NO_MKNOD: &str = "--cap-drop=MKNOD";
/// Format: where Docker Desktop's VM reaches this machine's loopback (its own name for the host, resolved by its engine).
const DESKTOP_HOST: &str = "host.docker.internal";
/// Format: where an engine on this machine reaches its loopback.
const LOOPBACK: &str = "127.0.0.1";

/// What an agent's container is run as: the engine, the container, and whom it runs as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// The engine's command line, by its full path (as `crate::runner::docker::find` finds it).
    pub program: String,
    /// The engine its profile names (Slates' `container_engine::export_engine`).
    pub engine: Engine,
    /// The container's name, and its engine volume's.
    pub name: String,
    /// The image it runs.
    pub image: String,
    /// Its entrypoint, in place of the image's own: none keeps the image's.
    pub entrypoint: Option<String>,
    /// The command it runs: the harness's, or an idle keeper's that the harness is run beside (`sandbox::container`).
    pub command: Vec<String>,
    /// The harness's folder: the same place in the workspace inside the container as on the machine (W3).
    pub workdir: String,
    /// Whom it runs as and the export serves: the uid and gid that own the agent's folder.
    pub user: (u32, u32),
}

/// Why an agent's container wasn't started ([`start`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// Slates refused the export, in its words.
    Export(String),
    /// Slates' daemon serves no NFS here, so no engine can mount its export.
    NoNfs,
    /// A path the engine's options can't carry: relative, or holding a comma, a quote, a space or a control character.
    Path,
    /// The engine refused its volume, in its words.
    Volume(String),
    /// The engine refused the run, in its words.
    Engine(String),
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Export(why) => write!(f, "Slates refused to export the workspace to its container: {why}"),
            Self::NoNfs => f.write_str("Slates' daemon serves no NFS on this machine, so its engine can't mount the workspace's export"),
            Self::Path => f.write_str("the workspace's export or mount has a path the engine's options can't carry"),
            Self::Volume(why) => write!(f, "the container engine refused the workspace's volume: {why}"),
            Self::Engine(why) => write!(f, "the container engine refused the run: {why}"),
        }
    }
}

impl std::error::Error for Refusal {}

/// Where the engine `engine` reaches this machine's loopback, where Slates' daemon serves NFS (see the module's notes).
fn server_for(engine: Engine) -> &'static str {
    match engine {
        Engine::DockerDesktop => DESKTOP_HOST,
        Engine::DockerEngine | Engine::Podman => LOOPBACK,
    }
}

/// Whether `path` is one the engine's comma-separated options can carry as it is.
fn carried(path: &str) -> bool {
    path.starts_with('/') && !path.chars().any(|c| c == ',' || c == '"' || c.is_whitespace() || c.is_control())
}

/// The engine's arguments for its volume `name` over the export at `path`, served on `port` (see the module's notes):
/// Docker's and podman's `local` driver, `type=nfs`.
fn volume_args(plan: &Plan, port: u16, path: &str) -> Result<Vec<String>, Refusal> {
    if !carried(path) {
        return Err(Refusal::Path);
    }
    let options = format!("addr={},port={port},nfsvers={NFS_VERSION},proto=tcp,hard,nosuid,nodev", server_for(plan.engine));
    Ok(["volume", "create", "--driver", "local", "--opt", "type=nfs", "--opt", &format!("o={options}"), "--opt", &format!("device=:{path}"), &plan.name].map(str::to_owned).to_vec())
}

/// The engine's arguments for `plan`'s container on its volume, mounted at `mount` (the machine's own mount point).
fn run_args(plan: &Plan, mount: &str) -> Result<Vec<String>, Refusal> {
    if !carried(mount) {
        return Err(Refusal::Path);
    }
    let (uid, gid) = plan.user;
    let mut args = vec!["run".to_owned(), "--detach".to_owned(), "--name".to_owned(), plan.name.clone()];
    args.extend(["--mount".to_owned(), format!("type=volume,source={},target={mount},volume-nocopy", plan.name)]);
    args.extend([NO_NEW_PRIVILEGES.to_owned(), NO_MKNOD.to_owned(), "--user".to_owned(), format!("{uid}:{gid}")]);
    if let Some(entrypoint) = &plan.entrypoint {
        args.extend(["--entrypoint".to_owned(), entrypoint.clone()]);
    }
    args.extend(["--workdir".to_owned(), plan.workdir.clone(), plan.image.clone()]);
    args.extend(plan.command.iter().cloned());
    Ok(args)
}

/// An agent's container, started on its workspace ([`start`]): the engine's id for it, its engine volume, the export it
/// mounts, and the engine's command line, which ends it ([`end`]).
#[derive(Clone, PartialEq, Eq)]
pub struct Running {
    /// The engine's id for the container.
    pub container: String,
    /// The engine's volume it runs on.
    pub volume: String,
    /// The export's attachment.
    pub export: u64,
    /// The engine's command line.
    pub program: String,
}

impl fmt::Debug for Running {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Running").field("container", &self.container).field("volume", &self.volume).field("export", &self.export).field("program", &self.program).finish()
    }
}

/// The steps a container's start and end take ([`Machine`] on this machine), apart so the order is kiln's and is tested
/// over stand-ins.
pub(crate) trait Steps {
    /// Slates' export of workspace `made` to `uid`, and the port its daemon serves NFS on (none where it serves none).
    async fn export(&self, made: &Made, uid: u32) -> anyhow::Result<(slates_api::Exported, Option<u16>)>;
    /// The engine's command `args` through `program`: what it printed.
    async fn engine(&self, program: &str, args: &[String]) -> anyhow::Result<String>;
    /// Slates' end of the export `attachment`.
    async fn detach(&self, attachment: u64) -> anyhow::Result<()>;
    /// W3's end of workspace `made`: its mount, then its volume.
    async fn take_down(&self, made: &Made) -> anyhow::Result<()>;
}

/// [`Steps`] on this machine: Slates' library on kiln's blocking pool, the engine's command line.
pub(crate) struct Machine;

impl Steps for Machine {
    async fn export(&self, made: &Made, uid: u32) -> anyhow::Result<(slates_api::Exported, Option<u16>)> {
        let exported = super::slates::export(made, uid, false).await?;
        let volume = super::slates::volume_id(&made.volume)?;
        let port = match super::slates::call("status", move |i| i.status(volume)).await {
            Ok(status) => status.nfs_port,
            Err(e) => {
                let _ = super::slates::detach(exported.attachment).await;
                return Err(e);
            }
        };
        Ok((exported, port))
    }

    async fn engine(&self, program: &str, args: &[String]) -> anyhow::Result<String> {
        let (program, args) = (program.to_owned(), args.to_vec());
        let ran = crate::rt::blocking(move || std::process::Command::new(&program).args(&args).output()).await??;
        anyhow::ensure!(ran.status.success(), "{}", String::from_utf8_lossy(&ran.stderr).trim());
        Ok(String::from_utf8_lossy(&ran.stdout).trim().to_owned())
    }

    async fn detach(&self, attachment: u64) -> anyhow::Result<()> {
        super::slates::detach(attachment).await
    }

    async fn take_down(&self, made: &Made) -> anyhow::Result<()> {
        super::slates::remove_after_container(made).await
    }
}

/// Starts `plan`'s container on workspace `made`, which is made already (W3): the export, the engine's volume over it,
/// the run (see the module's notes). A start refused part way lets go of what it made.
pub async fn start(made: &Made, plan: &Plan) -> Result<Running, Refusal> {
    start_with(&Machine, made, plan).await
}

/// Ends `running` and workspace `made` in order: the container, its engine volume, the export, then the mount and the
/// volume (W3). A container that won't go keeps the rest, never torn from under it.
pub async fn end(running: &Running, made: &Made) -> anyhow::Result<()> {
    end_with(&Machine, running, made).await
}

/// Ends `running` alone, in order: the container, its engine volume, then the export; the workspace's mount and volume
/// stay, for W3 to take down when the workspace goes (a container run inside a workspace that outlives it:
/// `sandbox::container`).
pub async fn release(running: &Running, made: &Made) -> anyhow::Result<()> {
    release_with(&Machine, running, made).await
}

/// `said` with the export's capability path `path` shown without its token, a bearer secret (an engine's refusal of
/// its volume names the volume's device, the path whole).
pub(crate) fn without_token(said: &str, path: &str) -> String {
    match path.rsplit_once('.') {
        Some((named, token)) if !token.is_empty() => said.replace(path, &format!("{named}.<token>")),
        _ => said.to_owned(),
    }
}

/// [`start`] over `steps`.
async fn start_with(steps: &impl Steps, made: &Made, plan: &Plan) -> Result<Running, Refusal> {
    let (exported, port) = steps.export(made, plan.user.0).await.map_err(|e| Refusal::Export(format!("{e:#}")))?;
    let started: Result<String, Refusal> = async {
        let port = port.ok_or(Refusal::NoNfs)?;
        let volume = volume_args(plan, port, &exported.path)?;
        let run = run_args(plan, &made.mount)?;
        steps.engine(&plan.program, &volume).await.map_err(|e| Refusal::Volume(without_token(&format!("{e:#}"), &exported.path)))?;
        match steps.engine(&plan.program, &run).await {
            Ok(id) if !id.is_empty() && id.bytes().all(|b| b.is_ascii_hexdigit()) => Ok(id),
            ran => {
                // The volume goes with the start it was made for.
                let rm = ["volume".to_owned(), "rm".to_owned(), "--force".to_owned(), plan.name.clone()];
                if let Err(e) = steps.engine(&plan.program, &rm).await {
                    tracing::warn!(volume = %plan.name, error = %format!("{e:#}"), "a refused container start's engine volume wasn't removed");
                }
                Err(Refusal::Engine(match ran {
                    Ok(id) => format!("it answered no container id: {id:?}"),
                    Err(e) => without_token(&format!("{e:#}"), &exported.path),
                }))
            }
        }
    }
    .await;
    match started {
        Ok(container) => Ok(Running { container, volume: plan.name.clone(), export: exported.attachment, program: plan.program.clone() }),
        Err(refused) => {
            // The export goes with the start it was made for; one that won't go ends with its volume anyway.
            if let Err(e) = steps.detach(exported.attachment).await {
                tracing::warn!(mount = %made.mount, error = %format!("{e:#}"), "a refused container start's export wasn't detached: it ends with its volume");
            }
            Err(refused)
        }
    }
}

/// [`release`] over `steps`.
async fn release_with(steps: &impl Steps, running: &Running, made: &Made) -> anyhow::Result<()> {
    // The container first: its engine lets go of the files it held open. One that won't go keeps the rest.
    steps.engine(&running.program, &["rm".to_owned(), "--force".to_owned(), running.container.clone()]).await?;
    // Then its engine volume, whose mount goes with it: tried again while the engine still counts the container it just
    // removed as its user, at most VOLUME_RM_TRIES times.
    let rm = ["volume".to_owned(), "rm".to_owned(), "--force".to_owned(), running.volume.clone()];
    let mut stayed = None;
    for tried in 1..=VOLUME_RM_TRIES {
        match steps.engine(&running.program, &rm).await {
            Ok(_) => {
                stayed = None;
                break;
            }
            Err(e) => {
                tracing::debug!(volume = %running.volume, tried, error = %format!("{e:#}"), "an ended container's engine volume wasn't removed yet");
                stayed = Some(e);
                if tried < VOLUME_RM_TRIES {
                    // (c) the container engine sends no notification when it lets go of its volume: asked again at a deadline.
                    crate::rt::sleep_until(kiln_seams::clock::later(std::time::Instant::now(), VOLUME_RM_PAUSE)).await;
                }
            }
        }
    }
    // Then the export, whatever the engine said: its capability reaches nothing after, so a volume left behind mounts
    // nothing of the workspace. One that won't go ends with its volume anyway.
    if let Err(e) = steps.detach(running.export).await {
        tracing::warn!(mount = %made.mount, error = %format!("{e:#}"), "an ended container's export wasn't detached: it ends with its volume");
    }
    match stayed {
        None => Ok(()),
        Some(e) => Err(e.context(format!("the engine volume {} stayed after {VOLUME_RM_TRIES} tries", running.volume))),
    }
}

/// [`end`] over `steps`.
async fn end_with(steps: &impl Steps, running: &Running, made: &Made) -> anyhow::Result<()> {
    release_with(steps, running, made).await?;
    // Then the mount and the volume, as W3 takes a workspace down, nothing kiln started in it running now.
    steps.take_down(made).await
}

/// What a runner's facts say of agents' containers here ([`ContainerFacts`]), their image the one its administrator
/// declared (`image`, `[agent] container_image`): asked as its facts are probed, through Slates' library alone. Blocking
/// calls (the engine's command line) run on kiln's blocking pool.
pub async fn facts(workspaces: Option<&Workspaces>, image: Option<&str>) -> ContainerFacts {
    let has_workspaces = workspaces.is_some_and(|w| w.available);
    let image = image.map(str::trim).filter(|i| !i.is_empty()).map(str::to_owned);
    crate::rt::blocking(move || facts_of(has_workspaces, image, crate::runner::docker::find, slates_api::container_engine::export_engine))
        .await
        .unwrap_or_else(|e| ContainerFacts { why_not: Some(format!("the container engine couldn't be asked: {e:#}")), missing: Some(ContainerMissing::Engine), ..ContainerFacts::default() })
}

/// The name a runner's facts give the engine a profile names.
pub fn engine_word(engine: Engine) -> &'static str {
    match engine {
        Engine::DockerDesktop => "docker-desktop",
        Engine::DockerEngine => "docker-engine",
        Engine::Podman => "podman",
    }
}

/// [`facts`] over its questions, in the order that asks least: the machine's workspaces first (a container works on its
/// agent's volume), then the image declared for its agents' containers (`image`), then the engine (`find`: its command
/// line, when its daemon answers), then Slates' judge of its profile (`profile`), then whether the engine mounts its own
/// volumes (a rootless one can't). Nothing is asked past the first lack, which is named.
fn facts_of(has_workspaces: bool, image: Option<String>, find: impl FnOnce() -> Option<String>, profile: impl FnOnce(&str) -> Result<ExportEngine, EngineFault>) -> ContainerFacts {
    if !has_workspaces {
        return ContainerFacts { why_not: Some("this machine gives agents no workspaces of their own, which their containers work on".to_owned()), missing: Some(ContainerMissing::Workspaces), ..ContainerFacts::default() };
    }
    let Some(image) = image else {
        return ContainerFacts { why_not: Some("no image is declared for its agents' containers: set `container_image` under `[agent]` in its kiln configuration".to_owned()), missing: Some(ContainerMissing::Image), ..ContainerFacts::default() };
    };
    let Some(program) = find() else {
        return ContainerFacts { image: Some(image), why_not: Some("no container engine answers here: neither docker nor podman, or its daemon is down".to_owned()), missing: Some(ContainerMissing::Engine), ..ContainerFacts::default() };
    };
    let judged = match profile(&program) {
        Ok(judged) => judged,
        Err(fault) => {
            let missing = match fault {
                EngineFault::Unsupported { .. } => ContainerMissing::Platform,
                EngineFault::Refused(_) | EngineFault::Untested { .. } | EngineFault::Unreachable(_) | EngineFault::TooLarge | EngineFault::Unreadable(_) => ContainerMissing::Profile,
            };
            // A profile Slates established but holds no evidence for still names the engine it refused.
            let (engine, endpoint, version) = match &fault {
                EngineFault::Untested { endpoint, profile, .. } => (Some(engine_word(profile.engine).to_owned()), Some(endpoint.clone()), Some(profile.server_version.clone())),
                _ => (None, None, None),
            };
            return ContainerFacts { available: false, program: Some(program), engine, endpoint, version, image: Some(image), why_not: Some(format!("Slates refused this machine's container engine: {fault}")), missing: Some(missing) };
        }
    };
    let said = ContainerFacts {
        program: Some(program),
        engine: Some(engine_word(judged.profile.engine).to_owned()),
        endpoint: Some(judged.endpoint.clone()),
        version: Some(judged.profile.server_version.clone()),
        image: Some(image),
        ..ContainerFacts::default()
    };
    ContainerFacts { available: true, ..said }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    /// Format: the workspace's mount point, as the kernel's table names it.
    const MOUNT: &str = "/home/u/.local/share/kiln/agent/workspaces/kiln-a";
    /// Format: the engine's command line.
    const DOCKER: &str = "/usr/local/bin/docker";
    /// Format: the image declared for agents' containers.
    const IMAGE: &str = "ghcr.io/org/agents:1";
    /// Format: the export's capability path as Slates answers it (`/<name>@<attachment>.<token>`).
    const EXPORT: &str = "/kiln-a@29.5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a";
    /// Format: the port Slates' daemon serves NFS on.
    const PORT: u16 = 2049;

    /// Workspace `kiln-a`, mounted at [`MOUNT`].
    fn made() -> Made {
        Made { volume: "0".repeat(32), name: "kiln-a".to_owned(), mount: MOUNT.to_owned(), ..Made::default() }
    }

    /// The plan of an agent's container through `engine`, run as uid 501, gid 20.
    fn plan(engine: Engine) -> Plan {
        Plan { program: DOCKER.to_owned(), engine, name: "kiln-a-1".to_owned(), image: "kiln/harness".to_owned(), entrypoint: None, command: vec!["claude".to_owned()], workdir: format!("{MOUNT}/crates"), user: (501, 20) }
    }

    /// The machine's steps as a stand-in takes them: Slates' export answering [`EXPORT`] on `port`, the engine answering
    /// each command by its first word from `answers` (else its empty success); every step is said in `said` as it's
    /// taken, and each engine command's arguments kept in `commands`.
    struct Stand {
        port: Option<u16>,
        /// How many `volume rm` fail before one goes (the engine still counting a container just removed as its user).
        volume_rm_fails: Mutex<u32>,
        answers: Vec<(&'static str, Result<String, String>)>,
        said: Mutex<Vec<String>>,
        commands: Mutex<Vec<Vec<String>>>,
    }

    impl Stand {
        fn new() -> Stand {
            Stand { port: Some(PORT), volume_rm_fails: Mutex::new(0), answers: vec![("run", Ok("c0ffee".to_owned()))], said: Mutex::new(Vec::new()), commands: Mutex::new(Vec::new()) }
        }

        fn answering(word: &'static str, answer: Result<String, String>) -> Stand {
            let mut stand = Stand::new();
            stand.answers.retain(|(w, _)| *w != word);
            stand.answers.push((word, answer));
            stand
        }

        fn say(&self, step: String) {
            if let Ok(mut said) = self.said.lock() {
                said.push(step);
            }
        }

        fn said(&self) -> Vec<String> {
            self.said.lock().map(|said| said.clone()).unwrap_or_default()
        }

        fn commands(&self) -> Vec<Vec<String>> {
            self.commands.lock().map(|c| c.clone()).unwrap_or_default()
        }
    }

    impl Steps for Stand {
        async fn export(&self, made: &Made, uid: u32) -> anyhow::Result<(slates_api::Exported, Option<u16>)> {
            self.say(format!("export {} to {uid}", made.name));
            Ok((slates_api::Exported { attachment: 29, path: EXPORT.to_owned() }, self.port))
        }

        async fn engine(&self, program: &str, args: &[String]) -> anyhow::Result<String> {
            assert_eq!(program, DOCKER);
            let words: Vec<&str> = args.iter().take(2).map(String::as_str).collect();
            let step = match words.as_slice() {
                ["volume", verb] => format!("volume {verb}"),
                [verb, ..] => (*verb).to_owned(),
                [] => String::new(),
            };
            self.say(step);
            if let Ok(mut kept) = self.commands.lock() {
                kept.push(args.to_vec());
            }
            let first = args.first().map(String::as_str).unwrap_or_default();
            let key = if first == "volume" { args.get(1).map(String::as_str).unwrap_or_default() } else { first };
            if first == "volume" && key == "rm"
                && let Ok(mut fails) = self.volume_rm_fails.lock()
                && *fails > 0
            {
                *fails -= 1;
                anyhow::bail!("remove kiln-a-1: volume is in use - [c0ffee]");
            }
            match self.answers.iter().find(|(w, _)| *w == key) {
                Some((_, answer)) => answer.clone().map_err(|why| anyhow::anyhow!(why)),
                None => Ok(String::new()),
            }
        }

        async fn detach(&self, attachment: u64) -> anyhow::Result<()> {
            self.say(format!("detach {attachment}"));
            Ok(())
        }

        async fn take_down(&self, made: &Made) -> anyhow::Result<()> {
            self.say(format!("take down {}", made.name));
            Ok(())
        }
    }

    /// W9 (Ada's ruling 2026-11-20: the export route). Do: start an agent's container through Docker Desktop on its
    /// workspace. Expect Slates' export to the folder's owner, then the engine's volume over it, then the run, in that
    /// order; the volume Docker's `local` driver at `type=nfs`, from Desktop's VM to this machine's loopback by Desktop's
    /// own name for it, at Slates' NFS port, NFSv4.2, `nosuid,nodev`, the export's capability path its device; the
    /// container on that volume at the machine's own mount point, never copying the image into it, run as the folder's
    /// owner under its own rules for `nosuid` and `nodev`, the harness at its place in the workspace.
    #[test]
    fn w9_a_container_starts_on_the_engines_own_volume_over_the_workspaces_export() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let stand = Stand::new();
            let running = start_with(&stand, &made(), &plan(Engine::DockerDesktop)).await?;
            assert_eq!(running, Running { container: "c0ffee".to_owned(), volume: "kiln-a-1".to_owned(), export: 29, program: DOCKER.to_owned() });
            assert_eq!(stand.said(), ["export kiln-a to 501", "volume create", "run"]);
            let device = format!("device=:{EXPORT}");
            let volume = ["volume", "create", "--driver", "local", "--opt", "type=nfs", "--opt", "o=addr=host.docker.internal,port=2049,nfsvers=4.2,proto=tcp,hard,nosuid,nodev", "--opt", device.as_str(), "kiln-a-1"];
            let mount = format!("type=volume,source=kiln-a-1,target={MOUNT},volume-nocopy");
            let workdir = format!("{MOUNT}/crates");
            let run = ["run", "--detach", "--name", "kiln-a-1", "--mount", mount.as_str(), NO_NEW_PRIVILEGES, NO_MKNOD, "--user", "501:20", "--workdir", workdir.as_str(), "kiln/harness", "claude"];
            assert_eq!(stand.commands(), [volume.map(str::to_owned).to_vec(), run.map(str::to_owned).to_vec()]);
            Ok(())
        })
    }

    /// W9. Do: start a container through a rootful Linux engine (Docker Engine, podman as root). Expect its volume served
    /// at this machine's loopback, which its engine mounts on the machine itself: no mount shared with other users asked.
    #[test]
    fn w9_a_rootful_linux_engine_mounts_the_export_at_the_machines_loopback() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            for engine in [Engine::DockerEngine, Engine::Podman] {
                let stand = Stand::new();
                start_with(&stand, &made(), &plan(engine)).await?;
                let options = stand.commands().first().and_then(|c| c.get(7).cloned()).unwrap_or_default();
                assert_eq!(options, "o=addr=127.0.0.1,port=2049,nfsvers=4.2,proto=tcp,hard,nosuid,nodev", "{engine:?}");
            }
            Ok(())
        })
    }

    /// W9. Do: start a container where Slates' daemon serves no NFS, one whose engine refuses its volume (naming the
    /// volume's device, as Docker's refusal does), and one whose engine refuses the run. Expect each refused typed, the
    /// export's token in none of their words; the first making no volume, the last removing the volume it made; and each
    /// start's export detached: a refused start leaves nothing behind.
    #[test]
    fn w9_a_refused_start_lets_go_of_what_it_made_and_never_shows_the_exports_token() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let desktop = plan(Engine::DockerDesktop);
            let stand = Stand { port: None, ..Stand::new() };
            assert_eq!(start_with(&stand, &made(), &desktop).await, Err(Refusal::NoNfs));
            assert_eq!(stand.said(), ["export kiln-a to 501", "detach 29"]);
            let stand = Stand::answering("create", Err(format!("failed to mount local volume: mount :{EXPORT}:/var/lib/docker/volumes/kiln-a-1/_data: no such file or directory")));
            let refused = start_with(&stand, &made(), &desktop).await;
            assert_eq!(refused, Err(Refusal::Volume("failed to mount local volume: mount :/kiln-a@29.<token>:/var/lib/docker/volumes/kiln-a-1/_data: no such file or directory".to_owned())));
            assert_eq!(stand.said(), ["export kiln-a to 501", "volume create", "detach 29"]);
            let stand = Stand::answering("run", Err(format!("error while mounting volume: {EXPORT}: permission denied")));
            let refused = start_with(&stand, &made(), &desktop).await;
            assert_eq!(refused, Err(Refusal::Engine("error while mounting volume: /kiln-a@29.<token>: permission denied".to_owned())));
            assert_eq!(stand.said(), ["export kiln-a to 501", "volume create", "run", "volume rm", "detach 29"]);
            assert!(!format!("{refused:?}").contains("5a5a5a"), "the token never shows");
            Ok(())
        })
    }

    /// W9. Do: start containers whose export path or mount point the engine's options can't carry (a comma, a space).
    /// Expect each refused before the engine is asked anything, its export detached.
    #[test]
    fn w9_a_path_the_engines_options_cant_carry_is_refused_before_the_engine_is_asked() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let stand = Stand::new();
            let spaced = Made { mount: "/home/u/my workspaces/kiln-a".to_owned(), ..made() };
            assert_eq!(start_with(&stand, &spaced, &plan(Engine::DockerDesktop)).await, Err(Refusal::Path));
            assert_eq!(stand.said(), ["export kiln-a to 501", "detach 29"]);
            assert_eq!(volume_args(&plan(Engine::DockerDesktop), PORT, "/kiln-a@1.x,ro"), Err(Refusal::Path));
            Ok(())
        })
    }

    /// W9. Do: end a running container, then one whose engine won't remove it. Expect the container, then its engine
    /// volume, then the export, then the workspace's mount and volume, in that order; and a container that won't go
    /// keeping everything beneath it.
    #[test]
    fn w9_a_containers_end_tears_down_the_container_then_its_volume_then_the_export_then_the_mount() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let running = Running { container: "c0ffee".to_owned(), volume: "kiln-a-1".to_owned(), export: 29, program: DOCKER.to_owned() };
            let stand = Stand::new();
            end_with(&stand, &running, &made()).await?;
            assert_eq!(stand.said(), ["rm", "volume rm", "detach 29", "take down kiln-a"]);
            assert_eq!(stand.commands(), [["rm", "--force", "c0ffee"].map(str::to_owned).to_vec(), ["volume", "rm", "--force", "kiln-a-1"].map(str::to_owned).to_vec()]);
            let stand = Stand::answering("rm", Err("container is busy".to_owned()));
            assert!(end_with(&stand, &running, &made()).await.is_err());
            assert_eq!(stand.said(), ["rm"], "a container that won't go keeps its volume, its export and the mount");
            Ok(())
        })
    }

    /// W9. Do: release a container run inside a workspace that outlives it. Expect the container, its engine volume and
    /// the export to go, and the workspace's mount and volume left for W3.
    #[test]
    fn w9_a_released_container_goes_with_its_volume_and_export_and_leaves_the_workspace() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let running = Running { container: "c0ffee".to_owned(), volume: "kiln-a-1".to_owned(), export: 29, program: DOCKER.to_owned() };
            let stand = Stand::new();
            release_with(&stand, &running, &made()).await?;
            assert_eq!(stand.said(), ["rm", "volume rm", "detach 29"]);
            Ok(())
        })
    }

    /// W9. Do: build the engine's run of a plan with an entrypoint of its own. Expect `--entrypoint` before the image, so
    /// the command after the image is the entrypoint's arguments, and none where the plan keeps the image's.
    #[test]
    fn w9_a_plans_own_entrypoint_goes_before_its_image() -> anyhow::Result<()> {
        let keeper = Plan { entrypoint: Some("tail".to_owned()), command: vec!["-f".to_owned(), "/dev/null".to_owned()], ..plan(Engine::DockerDesktop) };
        let args = run_args(&keeper, MOUNT)?;
        let tail = ["--entrypoint", "tail", "--workdir", &format!("{MOUNT}/crates"), "kiln/harness", "-f", "/dev/null"].map(str::to_owned);
        assert!(args.ends_with(&tail), "{args:?}");
        assert!(!run_args(&plan(Engine::DockerDesktop), MOUNT)?.iter().any(|a| a == "--entrypoint"));
        Ok(())
    }

    /// An engine Slates judged for the export route: `engine` at Docker's socket, mounting the export from `from`.
    fn judged(engine: Engine, from: slates_api::container_engine::MountsFrom) -> ExportEngine {
        use slates_api::container_engine::{Endpoint, ExportProfile, RuntimeProfile};
        ExportEngine {
            endpoint: "unix:///var/run/docker.sock".to_owned(),
            profile: RuntimeProfile {
                endpoint: Endpoint::UnixSocket("/var/run/docker.sock".to_owned()),
                engine,
                operating_system: "Docker Desktop".to_owned(),
                server_version: "29.3.1".to_owned(),
                rootless: false,
                user_namespace_remap: false,
                selinux_labels: false,
            },
            tested: ExportProfile { test: "a stand-in's", description: "a stand-in's", mounts_from: from },
        }
    }

    /// W9: a runner's facts say whether its agents can run in containers, asking in the order that asks least. Given a
    /// machine with no workspaces, then no image declared for its agents' containers, then no engine, then an engine Slates
    /// refuses (by its runtime, by an untested profile, on a platform where nothing is mounted into a container), then a
    /// rootless engine, then a rootful Linux engine and Docker Desktop. Expect each lack named typed and nothing asked past
    /// it; the refused untested profile still naming its engine; the rootless one said to mount nothing itself; only the
    /// last two available (the export route needs no mount shared with other users).
    #[test]
    #[allow(clippy::result_large_err, reason = "the stand-ins answer as Slates' export_engine does, with its EngineFault")]
    fn w9_a_runners_facts_say_whether_its_agents_can_run_in_containers_and_what_it_lacks() {
        use slates_api::container_engine::ProfileRefusal;
        let asked = std::cell::Cell::new(0_u32);
        let image = || Some(IMAGE.to_owned());
        let none = facts_of(
            false,
            image(),
            || {
                asked.set(asked.get() + 1);
                None
            },
            |_: &str| {
                asked.set(asked.get() + 1);
                Err(EngineFault::TooLarge)
            },
        );
        assert_eq!((none.available, none.missing, none.program, asked.get()), (false, Some(ContainerMissing::Workspaces), None, 0), "nothing is asked of a machine without workspaces");
        let no_image = facts_of(
            true,
            None,
            || {
                asked.set(asked.get() + 1);
                None
            },
            |_: &str| {
                asked.set(asked.get() + 1);
                Err(EngineFault::TooLarge)
            },
        );
        assert_eq!((no_image.available, no_image.missing, asked.get()), (false, Some(ContainerMissing::Image), 0), "no engine is looked for where no image is declared");
        assert!(no_image.why_not.as_deref().is_some_and(|w| w.contains("container_image")), "{:?}", no_image.why_not);
        let no_engine = facts_of(true, image(), || None, |_: &str| {
            asked.set(asked.get() + 1);
            Err(EngineFault::TooLarge)
        });
        assert_eq!((no_engine.available, no_engine.missing, no_engine.image.as_deref(), asked.get()), (false, Some(ContainerMissing::Engine), Some(IMAGE), 0), "no profile is asked of an engine not found");

        let refused = |fault: EngineFault| facts_of(true, image(), || Some(DOCKER.to_owned()), move |_: &str| Err(fault));
        let by_runtime = refused(EngineFault::Refused(ProfileRefusal::RuntimeUnsupported { runtime: "nerdctl".to_owned() }));
        assert_eq!((by_runtime.available, by_runtime.missing, by_runtime.program.as_deref(), by_runtime.engine), (false, Some(ContainerMissing::Profile), Some(DOCKER), None));
        assert!(by_runtime.why_not.as_deref().is_some_and(|w| w.starts_with("Slates refused this machine's container engine: ")), "{:?}", by_runtime.why_not);
        let untested = judged(Engine::DockerEngine, slates_api::container_engine::MountsFrom::HostKernel);
        let by_profile = refused(EngineFault::Untested { endpoint: untested.endpoint.clone(), profile: Box::new(untested.profile), refusal: ProfileRefusal::RootlessCannotMount });
        assert_eq!((by_profile.missing, by_profile.engine.as_deref(), by_profile.version.as_deref()), (Some(ContainerMissing::Profile), Some("docker-engine"), Some("29.3.1")));
        assert!(by_profile.why_not.as_deref().is_some_and(|w| w.contains("RootlessCannotMount")), "{:?}", by_profile.why_not);
        let windows = refused(EngineFault::Unsupported { platform: "windows" });
        assert_eq!((windows.available, windows.missing), (false, Some(ContainerMissing::Platform)));

        let with = |profile: ExportEngine| facts_of(true, image(), || Some(DOCKER.to_owned()), move |program: &str| {
            assert_eq!(program, DOCKER, "Slates judges the engine found");
            Ok(profile)
        });
        let rootful = with(judged(Engine::DockerEngine, slates_api::container_engine::MountsFrom::HostKernel));
        assert_eq!((rootful.available, rootful.missing, rootful.engine.as_deref()), (true, None, Some("docker-engine")));
        let desktop = with(judged(Engine::DockerDesktop, slates_api::container_engine::MountsFrom::DesktopVm));
        assert_eq!(
            desktop,
            ContainerFacts { available: true, program: Some(DOCKER.to_owned()), engine: Some("docker-desktop".to_owned()), endpoint: Some("unix:///var/run/docker.sock".to_owned()), version: Some("29.3.1".to_owned()), image: Some(IMAGE.to_owned()), why_not: None, missing: None }
        );
    }

    /// W9 (w-k90-containers' r8 on Docker Desktop 29.3.1: an ended session's engine volume left behind). Do: release a
    /// container whose engine still counts it as its volume's user for a moment (its first two `volume rm` refused), then
    /// one whose volume never goes. Expect the first's volume removed on a later try, then its export detached; the
    /// second's tries bounded, its export detached whatever the engine said, and its release an error naming the volume.
    #[test]
    fn w9_an_engine_volume_still_in_use_is_removed_on_a_later_try_and_its_export_detached_whatever_it_said() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let running = Running { container: "c0ffee".to_owned(), volume: "kiln-a-1".to_owned(), export: 29, program: DOCKER.to_owned() };
            let stand = Stand { volume_rm_fails: Mutex::new(2), ..Stand::new() };
            release_with(&stand, &running, &made()).await?;
            assert_eq!(stand.said(), ["rm", "volume rm", "volume rm", "volume rm", "detach 29"]);
            let stand = Stand { volume_rm_fails: Mutex::new(u32::MAX), ..Stand::new() };
            let refused = release_with(&stand, &running, &made()).await;
            assert!(refused.as_ref().is_err_and(|e| format!("{e:#}").contains("kiln-a-1")), "{refused:?}");
            let said = stand.said();
            assert_eq!(said.iter().filter(|s| *s == "volume rm").count(), usize::try_from(VOLUME_RM_TRIES).unwrap_or(0));
            assert_eq!(said.last().map(String::as_str), Some("detach 29"), "the export goes whatever the engine said: {said:?}");
            Ok(())
        })
    }
}
