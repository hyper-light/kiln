//! Agents' containers on their workspaces' Slates volumes (docs/agentic.md, W9; Slates' A-520): an agent whose runner
//! runs its sessions in containers works in a container of its own, its workspace's volume mounted on the machine
//! through Slates' library (W3, [`super::slates::make`]) and bound into the container, never copied into it, so its
//! review, drift and landing are W3's, W4's and W6's as they are.
//!
//! **The order is this module's**, and its unit tests hold it over stand-ins (`Steps`). The engine's profile is judged
//! first, by the caller, before the workspace is made (Slates' `container_engine::engine_profile`). Then the bind, Slates'
//! own (`Instance::bind_into_container`): Slates checks in the kernel's mount table that the source is exactly the
//! workspace's mount, records the binding and answers the runtime's `mounts` entry, which kiln hands its engine word for
//! word ([`engine_mount`]), never a bind of its own. Then the source checked again just before the engine binds it
//! (`ContainerBind::check_source`), so a mount gone or replaced in between is never bound. Then the engine's run. A start
//! refused after its bind lets the bind go. Its end goes in order: the container (its engine lets go of the files it held
//! open on the mount, and its processes end with it), then the bind, then the mount and the volume (W3's
//! [`super::slates::remove`], which asks the mount to go once nothing kiln started in it runs).
//!
//! **The entry's words.** Slates answers a non-recursive `bind`, `ro` or `rw`, `private`, `nosuid` and `nodev`. The
//! engine's `--mount` says the first three (Docker's `bind-recursive=disabled`, `readonly` and `bind-propagation=private`;
//! podman's `bind-nonrecursive`, `ro=true` and `bind-propagation=private`). `--mount` is the form, never `-v`, which
//! makes a missing source on the host (Docker's bind-mount documentation): a mount gone is the engine's refusal, never an
//! agent writing into the person's disk. `nosuid` and `nodev` have no `--mount` word in either engine, so the container's
//! own rules hold them: `--security-opt=no-new-privileges` (no process of the container gains a privilege by executing a
//! setuid or setgid file, the volume's or any) and `--cap-drop=MKNOD` (no device node is made through it), the host's
//! own mount being `nosuid,nodev` already. A word kiln doesn't know, a word said twice, a `type` not `bind`, an access
//! said twice, not at all or against the entry's own `read_only`, or a path the `--mount` syntax can't carry (relative,
//! or holding a comma, a quote or a line break) is refused typed ([`Unappliable`]), never dropped.
//!
//! **Where the agent works.** The container sees the workspace at the same path the machine mounts it, and its harness
//! starts at the same place in it ([`Plan::workdir`], W3), so nothing is translated. Its identity follows the profile's
//! rule: under rootless podman (`ContainerRootAsMountingUser`) the container runs as its root (`--user 0:0`, whatever the
//! image's own user), the one id an unshared mount admits; through Docker Desktop's share (`HostUserThroughShare`) any id
//! reaches the volume as the person, so the image's own user stands; a rootful Linux engine (`ContainerIdsAsHostIds`)
//! needs the mount shared with other users, which kiln doesn't mount yet, and is refused before anything is asked
//! ([`Refusal::NeedsSharedMount`]). Where the profile's share shows a hard link's other names stale once its first is
//! removed (Docker Desktop), git in the container writes its objects by rename (`core.createObject=rename`, through git's
//! own `GIT_CONFIG_COUNT`, `GIT_CONFIG_KEY_0` and `GIT_CONFIG_VALUE_0`).

use std::fmt;

use kiln_ci::facts::{ContainerFacts, ContainerMissing, Workspaces};
use slates_api::container_engine::{Engine, EngineFault, EngineProfile, HardLinkRule, IdentityRule, TestedProfile};
use slates_api::{ContainerBind, OciBinding, SourceFault};

use super::slates::Made;

/// The engine's command line a container is run through, by the interface it speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cli {
    /// The Docker CLI: Docker Desktop's, or a Docker Engine's.
    Docker,
    /// Podman's.
    Podman,
}

impl Cli {
    /// The command line of the engine a profile names.
    pub fn of(engine: Engine) -> Cli {
        match engine {
            Engine::DockerDesktop | Engine::DockerEngine => Cli::Docker,
            Engine::Podman => Cli::Podman,
        }
    }
}

/// Slates' entry as an engine applies it ([`engine_mount`]): its `--mount` value, and the container options that hold
/// what `--mount` has no word for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineMount {
    /// The `--mount` value.
    pub mount: String,
    /// The container's own options for the entry's other words.
    pub options: Vec<String>,
}

/// Why Slates' entry can't be applied as it answered it ([`engine_mount`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unappliable {
    /// Its `type` isn't a bind.
    NotABind {
        /// The type it named.
        kind: String,
    },
    /// A word kiln doesn't know: applying the rest would drop it.
    UnknownWord {
        /// The word.
        word: String,
    },
    /// A word said twice.
    Repeated {
        /// The word.
        word: String,
    },
    /// No non-recursive `bind`: kiln never binds what lies beneath the source.
    Recursive,
    /// Its access said twice, not at all, or against its own `read_only`.
    Access,
    /// A path the `--mount` syntax can't carry: relative, or holding a comma, a quote or a line break.
    Path {
        /// The path.
        path: String,
    },
}

impl fmt::Display for Unappliable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotABind { kind } => write!(f, "Slates' entry is a {kind:?} mount, not a bind"),
            Self::UnknownWord { word } => write!(f, "Slates' entry says {word:?}, which kiln doesn't know how to apply"),
            Self::Repeated { word } => write!(f, "Slates' entry says {word:?} twice"),
            Self::Recursive => f.write_str("Slates' entry asks no non-recursive bind"),
            Self::Access => f.write_str("Slates' entry says its access twice, not at all, or against its own read_only"),
            Self::Path { path } => write!(f, "{path:?} can't be carried in an engine's --mount: relative, or with a comma, a quote or a line break"),
        }
    }
}

impl std::error::Error for Unappliable {}

/// Format: the words Slates' runtime entry carries (`crates/bridge-oci/src/binding.rs`).
const BIND: &str = "bind";
/// Format: see [`BIND`].
const READ_ONLY: &str = "ro";
/// Format: see [`BIND`].
const READ_WRITE: &str = "rw";
/// Format: see [`BIND`].
const PRIVATE: &str = "private";
/// Format: see [`BIND`].
const NOSUID: &str = "nosuid";
/// Format: see [`BIND`].
const NODEV: &str = "nodev";
/// Format: the container option that holds `nosuid`: no process of the container gains a privilege by an exec.
const NO_NEW_PRIVILEGES: &str = "--security-opt=no-new-privileges";
/// Format: the container option that holds `nodev`: no device node is made through the container.
const NO_MKNOD: &str = "--cap-drop=MKNOD";

/// Slates' entry `binding` as `cli` applies it (see the module's notes): its `--mount` value and the container options
/// for the words `--mount` can't say, or why it can't be applied as Slates answered it.
pub fn engine_mount(binding: &OciBinding, cli: Cli) -> Result<EngineMount, Unappliable> {
    if binding.mount_type != BIND {
        return Err(Unappliable::NotABind { kind: binding.mount_type.clone() });
    }
    for path in [&binding.source, &binding.destination] {
        if !path.starts_with('/') || path.contains([',', '"', '\n', '\r']) {
            return Err(Unappliable::Path { path: path.clone() });
        }
    }
    let mut fields = vec!["type=bind".to_owned(), format!("source={}", binding.source), format!("target={}", binding.destination)];
    let mut options = Vec::new();
    let mut said: Vec<&str> = Vec::new();
    let mut access = None;
    for word in &binding.options {
        let word = word.as_str();
        if said.contains(&word) {
            return Err(Unappliable::Repeated { word: word.to_owned() });
        }
        said.push(word);
        match word {
            BIND => fields.push(
                match cli {
                    Cli::Docker => "bind-recursive=disabled",
                    Cli::Podman => "bind-nonrecursive",
                }
                .to_owned(),
            ),
            READ_ONLY | READ_WRITE => {
                if access.replace(word == READ_ONLY).is_some() {
                    return Err(Unappliable::Access);
                }
            }
            PRIVATE => fields.push("bind-propagation=private".to_owned()),
            NOSUID => options.push(NO_NEW_PRIVILEGES.to_owned()),
            NODEV => options.push(NO_MKNOD.to_owned()),
            other => return Err(Unappliable::UnknownWord { word: other.to_owned() }),
        }
    }
    if !said.contains(&BIND) {
        return Err(Unappliable::Recursive);
    }
    match access {
        Some(read_only) if read_only == binding.read_only => {
            if read_only {
                fields.push(
                    match cli {
                        Cli::Docker => "readonly",
                        Cli::Podman => "ro=true",
                    }
                    .to_owned(),
                );
            }
        }
        _ => return Err(Unappliable::Access),
    }
    Ok(EngineMount { mount: fields.join(","), options })
}

/// What an agent's container is run as: the engine, its judged profile, and the container itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// The engine's command line, by its full path (as `crate::runner::docker::find` finds it).
    pub program: String,
    /// The engine its profile names (Slates' `container_engine::engine_profile`).
    pub engine: Engine,
    /// The evidence its profile holds, with its rules.
    pub tested: TestedProfile,
    /// The container's name.
    pub name: String,
    /// The image it runs.
    pub image: String,
    /// The command it runs, the harness's.
    pub command: Vec<String>,
    /// The harness's folder: the same place in the workspace inside the container as on the machine (W3).
    pub workdir: String,
}

/// Why an agent's container wasn't started ([`start`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The profile's container ids reach the volume as the host's own (a rootful Linux engine), which needs the workspace
    /// mounted shared with other users: kiln doesn't mount one so yet.
    NeedsSharedMount,
    /// Slates refused the bind, in its words.
    Bind(String),
    /// Slates' entry can't be applied as it answered it.
    Unappliable(Unappliable),
    /// The bind's source could not be checked, in the words of why.
    Unchecked(String),
    /// The bind's source is no longer the mount Slates verified: it is never bound.
    Source(SourceFault),
    /// The engine refused the run, in its words.
    Engine(String),
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NeedsSharedMount => f.write_str(
                "this engine's containers reach the workspace as their own ids (a rootful Linux engine), which needs the workspace mounted shared with other users, as kiln doesn't mount it yet",
            ),
            Self::Bind(why) => write!(f, "Slates refused to bind the workspace into the container: {why}"),
            Self::Unappliable(why) => write!(f, "{why}"),
            Self::Unchecked(why) => write!(f, "the workspace's mount couldn't be checked before the bind: {why}"),
            Self::Source(fault) => write!(f, "{fault}"),
            Self::Engine(why) => write!(f, "the container engine refused the run: {why}"),
        }
    }
}

impl std::error::Error for Refusal {}

/// The container options the profile's identity rule asks (see the module's notes), or the refusal of a profile kiln
/// can't run a container under yet.
fn identity_options(identity: IdentityRule) -> Result<Vec<String>, Refusal> {
    match identity {
        IdentityRule::ContainerRootAsMountingUser => Ok(vec!["--user".to_owned(), "0:0".to_owned()]),
        IdentityRule::HostUserThroughShare => Ok(Vec::new()),
        IdentityRule::ContainerIdsAsHostIds => Err(Refusal::NeedsSharedMount),
    }
}

/// The container options the profile's hard-link rule asks (see the module's notes).
fn hard_link_options(hard_links: HardLinkRule) -> Vec<String> {
    match hard_links {
        HardLinkRule::OtherNamesStaleAfterTheFirstIsRemoved => ["GIT_CONFIG_COUNT=1", "GIT_CONFIG_KEY_0=core.createObject", "GIT_CONFIG_VALUE_0=rename"]
            .into_iter()
            .flat_map(|setting| ["--env".to_owned(), setting.to_owned()])
            .collect(),
        HardLinkRule::EveryNameServedAtOnce => Vec::new(),
    }
}

/// The engine's arguments for `plan`'s container over `applied`, its identity's options already judged.
fn run_args(plan: &Plan, applied: &EngineMount, identity: Vec<String>) -> Vec<String> {
    let mut args = vec!["run".to_owned(), "--detach".to_owned(), "--name".to_owned(), plan.name.clone(), "--mount".to_owned(), applied.mount.clone()];
    args.extend(applied.options.iter().cloned());
    args.extend(identity);
    args.extend(hard_link_options(plan.tested.hard_links));
    args.extend(["--workdir".to_owned(), plan.workdir.clone(), plan.image.clone()]);
    args.extend(plan.command.iter().cloned());
    args
}

/// An agent's container, started on its workspace ([`start`]): the engine's id for it, the bind it runs on, and the
/// engine's command line, which ends it ([`end`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Running {
    /// The engine's id for the container.
    pub container: String,
    /// The bind's attachment.
    pub bind: u64,
    /// The engine's command line.
    pub program: String,
}

/// The steps a container's start and end take ([`Machine`] on this machine), apart so the order is kiln's and is tested
/// over stand-ins.
pub(crate) trait Steps {
    /// Slates' bind of workspace `made`'s mount into a container at the same path.
    async fn bind(&self, made: &Made) -> anyhow::Result<ContainerBind>;
    /// Slates' check that `bind`'s source is still the mount it verified: its verdict, or why it couldn't be asked.
    async fn check(&self, bind: &ContainerBind) -> anyhow::Result<Result<(), SourceFault>>;
    /// The engine's run of `args` through `program`: the container's id.
    async fn run(&self, program: &str, args: &[String]) -> anyhow::Result<String>;
    /// The engine's removal of `container` through `program`, stopping it first.
    async fn remove(&self, program: &str, container: &str) -> anyhow::Result<()>;
    /// Slates' end of the bind `attachment`.
    async fn detach(&self, attachment: u64) -> anyhow::Result<()>;
    /// W3's end of workspace `made`: its mount, then its volume.
    async fn take_down(&self, made: &Made) -> anyhow::Result<()>;
}

/// [`Steps`] on this machine: Slates' library on kiln's blocking pool, the engine's command line.
pub(crate) struct Machine;

impl Steps for Machine {
    async fn bind(&self, made: &Made) -> anyhow::Result<ContainerBind> {
        let volume = super::slates::volume_id(&made.volume)?;
        let point = made.mount.clone();
        super::slates::call("container bind", move |i| i.bind_into_container(volume, &point, &point, slates_api::Intent::Write)).await
    }

    async fn check(&self, bind: &ContainerBind) -> anyhow::Result<Result<(), SourceFault>> {
        let bind = bind.clone();
        crate::rt::blocking(move || bind.check_source()).await
    }

    async fn run(&self, program: &str, args: &[String]) -> anyhow::Result<String> {
        let (program, args) = (program.to_owned(), args.to_vec());
        let ran = crate::rt::blocking(move || std::process::Command::new(&program).args(&args).output()).await??;
        anyhow::ensure!(ran.status.success(), "{}", String::from_utf8_lossy(&ran.stderr).trim());
        let id = String::from_utf8_lossy(&ran.stdout).trim().to_owned();
        anyhow::ensure!(!id.is_empty() && id.bytes().all(|b| b.is_ascii_hexdigit()), "the engine answered no container id: {id:?}");
        Ok(id)
    }

    async fn remove(&self, program: &str, container: &str) -> anyhow::Result<()> {
        let (program, container) = (program.to_owned(), container.to_owned());
        let removed = crate::rt::blocking(move || std::process::Command::new(&program).args(["rm", "--force", &container]).output()).await??;
        anyhow::ensure!(removed.status.success(), "{}", String::from_utf8_lossy(&removed.stderr).trim());
        Ok(())
    }

    async fn detach(&self, attachment: u64) -> anyhow::Result<()> {
        super::slates::call("container unbind", move |i| i.detach(attachment)).await
    }

    async fn take_down(&self, made: &Made) -> anyhow::Result<()> {
        super::slates::remove(made).await
    }
}

/// Starts `plan`'s container on workspace `made`, which is mounted already (W3): the bind, the source checked just before
/// the engine binds it, the run (see the module's notes). A start refused after its bind lets the bind go.
pub async fn start(made: &Made, plan: &Plan) -> Result<Running, Refusal> {
    start_with(&Machine, made, plan).await
}

/// Ends `running` and workspace `made` in order: the container, then the bind, then the mount and the volume (W3). A
/// container that won't go keeps its bind and its mount, never torn from under it.
pub async fn end(running: &Running, made: &Made) -> anyhow::Result<()> {
    end_with(&Machine, running, made).await
}

/// [`start`] over `steps`.
async fn start_with(steps: &impl Steps, made: &Made, plan: &Plan) -> Result<Running, Refusal> {
    // The profile first: one kiln can't run a container under is refused before anything is asked.
    let identity = identity_options(plan.tested.identity)?;
    let bind = steps.bind(made).await.map_err(|e| Refusal::Bind(format!("{e:#}")))?;
    let started: Result<String, Refusal> = async {
        let applied = engine_mount(&bind.binding, Cli::of(plan.engine)).map_err(Refusal::Unappliable)?;
        let args = run_args(plan, &applied, identity);
        // Just before the engine binds it: the source still the mount Slates verified.
        match steps.check(&bind).await {
            Ok(Ok(())) => {}
            Ok(Err(fault)) => return Err(Refusal::Source(fault)),
            Err(e) => return Err(Refusal::Unchecked(format!("{e:#}"))),
        }
        steps.run(&plan.program, &args).await.map_err(|e| Refusal::Engine(format!("{e:#}")))
    }
    .await;
    match started {
        Ok(container) => Ok(Running { container, bind: bind.attachment, program: plan.program.clone() }),
        Err(refused) => {
            // The bind goes with the start it was made for; one that won't go ends with its mount anyway.
            if let Err(e) = steps.detach(bind.attachment).await {
                tracing::warn!(mount = %made.mount, error = %format!("{e:#}"), "a refused container start's bind wasn't let go: it ends with its mount");
            }
            Err(refused)
        }
    }
}

/// [`end`] over `steps`.
async fn end_with(steps: &impl Steps, running: &Running, made: &Made) -> anyhow::Result<()> {
    // The container first: its engine lets go of the files it held open on the mount. One that won't go keeps the rest.
    steps.remove(&running.program, &running.container).await?;
    // Then the bind; one that won't go ends with its mount anyway (Slates: a bind ends with its source mount).
    if let Err(e) = steps.detach(running.bind).await {
        tracing::warn!(mount = %made.mount, error = %format!("{e:#}"), "an ended container's bind wasn't let go: it ends with its mount");
    }
    // Then the mount and the volume, as W3 takes a workspace down, nothing kiln started in it running now.
    steps.take_down(made).await
}

/// What a runner's facts say of agents' containers here ([`ContainerFacts`]): asked as its facts are probed, through
/// Slates' library alone. Blocking calls (the engine's command line) run on kiln's blocking pool.
pub async fn facts(workspaces: Option<&Workspaces>) -> ContainerFacts {
    let has_workspaces = workspaces.is_some_and(|w| w.available);
    crate::rt::blocking(move || facts_of(has_workspaces, crate::runner::docker::find, |program| slates_api::container_engine::engine_profile(program)))
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

/// [`facts`] over its two questions, in the order that asks least: the machine's workspaces first (a container binds its
/// agent's volume), then the engine (`find`: its command line, when its daemon answers), then Slates' judge of its profile
/// (`profile`), then whether kiln runs a container under that profile's identity rule ([`identity_options`]). Nothing is
/// asked past the first lack, which is named.
fn facts_of(has_workspaces: bool, find: impl FnOnce() -> Option<String>, profile: impl FnOnce(&str) -> Result<EngineProfile, EngineFault>) -> ContainerFacts {
    if !has_workspaces {
        return ContainerFacts { why_not: Some("this machine gives agents no workspaces of their own, which their containers bind".to_owned()), missing: Some(ContainerMissing::Workspaces), ..ContainerFacts::default() };
    }
    let Some(program) = find() else {
        return ContainerFacts { why_not: Some("no container engine answers here: neither docker nor podman, or its daemon is down".to_owned()), missing: Some(ContainerMissing::Engine), ..ContainerFacts::default() };
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
            return ContainerFacts { available: false, program: Some(program), engine, endpoint, version, why_not: Some(format!("Slates refused this machine's container engine: {fault}")), missing: Some(missing) };
        }
    };
    let said = ContainerFacts { program: Some(program), engine: Some(engine_word(judged.profile.engine).to_owned()), endpoint: Some(judged.endpoint.clone()), version: Some(judged.profile.server_version.clone()), ..ContainerFacts::default() };
    match identity_options(judged.tested.identity) {
        Ok(_) => ContainerFacts { available: true, ..said },
        Err(refused) => ContainerFacts { why_not: Some(refused.to_string()), missing: Some(ContainerMissing::SharedMount), ..said },
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use slates_api::HostMountEvidence;
    use slates_api::container::MountIdentity;

    use super::*;

    /// Format: the workspace's mount point, as the kernel's table names it.
    const MOUNT: &str = "/home/u/.local/share/kiln/agent/workspaces/kiln-a";
    /// Format: the engine's command line.
    const DOCKER: &str = "/usr/local/bin/docker";

    /// Workspace `kiln-a`, mounted at [`MOUNT`].
    fn made() -> Made {
        Made { volume: "0".repeat(32), name: "kiln-a".to_owned(), mount: MOUNT.to_owned(), ..Made::default() }
    }

    /// Slates' entry for the workspace's mount bound at the same path, as it answers it (`crates/bridge-oci/src/binding.rs`).
    fn binding(read_only: bool) -> OciBinding {
        let access = if read_only { READ_ONLY } else { READ_WRITE };
        OciBinding {
            source: MOUNT.to_owned(),
            destination: MOUNT.to_owned(),
            read_only,
            mount_type: BIND.to_owned(),
            options: [BIND, access, PRIVATE, NOSUID, NODEV].map(str::to_owned).to_vec(),
            evidence: HostMountEvidence { fstype: "nfs".to_owned(), mount_source: "localhost:/kiln-a".to_owned(), names_volume: true, mount_id: 7, mount_device: 9, shared_with_other_users: false },
        }
    }

    /// A profile holding `identity` and `hard_links`.
    fn tested(identity: IdentityRule, hard_links: HardLinkRule) -> TestedProfile {
        TestedProfile { test: "T-4.13", description: "a stand-in's", identity, hard_links }
    }

    /// The plan of an agent's container through `engine`, holding `tested`.
    fn plan(engine: Engine, tested: TestedProfile) -> Plan {
        Plan { program: DOCKER.to_owned(), engine, tested, name: "kiln-a".to_owned(), image: "kiln/harness".to_owned(), command: vec!["claude".to_owned()], workdir: format!("{MOUNT}/crates") }
    }

    /// Docker Desktop's plan: its share's rules.
    fn desktop() -> Plan {
        plan(Engine::DockerDesktop, tested(IdentityRule::HostUserThroughShare, HardLinkRule::OtherNamesStaleAfterTheFirstIsRemoved))
    }

    /// The machine's steps as a stand-in takes them: Slates' bind answering [`binding`], the check answering `check`, the
    /// engine's run answering `run` and its removal `remove`; every step is said in `said` as it's taken, and the run's
    /// arguments kept in `args`.
    struct Stand {
        check: Result<(), SourceFault>,
        run: Result<String, String>,
        remove: Result<(), String>,
        said: Mutex<Vec<String>>,
        args: Mutex<Vec<String>>,
    }

    impl Stand {
        fn new() -> Stand {
            Stand { check: Ok(()), run: Ok("c0ffee".to_owned()), remove: Ok(()), said: Mutex::new(Vec::new()), args: Mutex::new(Vec::new()) }
        }

        fn say(&self, step: String) {
            if let Ok(mut said) = self.said.lock() {
                said.push(step);
            }
        }

        fn said(&self) -> Vec<String> {
            self.said.lock().map(|said| said.clone()).unwrap_or_default()
        }

        fn args(&self) -> Vec<String> {
            self.args.lock().map(|args| args.clone()).unwrap_or_default()
        }
    }

    impl Steps for Stand {
        async fn bind(&self, made: &Made) -> anyhow::Result<ContainerBind> {
            self.say(format!("bind {}", made.name));
            Ok(ContainerBind { attachment: 41, binding: binding(false) })
        }

        async fn check(&self, _bind: &ContainerBind) -> anyhow::Result<Result<(), SourceFault>> {
            self.say("check".to_owned());
            Ok(self.check.clone())
        }

        async fn run(&self, program: &str, args: &[String]) -> anyhow::Result<String> {
            self.say(format!("run {program}"));
            if let Ok(mut kept) = self.args.lock() {
                *kept = args.to_vec();
            }
            self.run.clone().map_err(|why| anyhow::anyhow!(why))
        }

        async fn remove(&self, _program: &str, container: &str) -> anyhow::Result<()> {
            self.say(format!("remove {container}"));
            self.remove.clone().map_err(|why| anyhow::anyhow!(why))
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

    /// W9 (Slates' A-520). Do: start an agent's container through Docker Desktop on its workspace. Expect Slates' bind, then
    /// its check of the source, then the engine's run, in that order; the engine handed Slates' entry word for word as
    /// Docker's `--mount` says it, the container's own rules for `nosuid` and `nodev`, git writing by rename (Desktop's
    /// share), the image's own user, and the harness's folder at its place in the workspace; and the container running on
    /// the bind.
    #[test]
    fn w9_a_container_starts_on_its_bound_volume_its_source_checked_just_before_the_engine_binds() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let stand = Stand::new();
            let running = start_with(&stand, &made(), &desktop()).await?;
            assert_eq!(running, Running { container: "c0ffee".to_owned(), bind: 41, program: DOCKER.to_owned() });
            let ran = format!("run {DOCKER}");
            assert_eq!(stand.said(), ["bind kiln-a", "check", ran.as_str()]);
            let mount = format!("type=bind,source={MOUNT},target={MOUNT},bind-recursive=disabled,bind-propagation=private");
            let workdir = format!("{MOUNT}/crates");
            let expected = [
                "run", "--detach", "--name", "kiln-a", "--mount", mount.as_str(), NO_NEW_PRIVILEGES, NO_MKNOD, "--env", "GIT_CONFIG_COUNT=1", "--env",
                "GIT_CONFIG_KEY_0=core.createObject", "--env", "GIT_CONFIG_VALUE_0=rename", "--workdir", workdir.as_str(), "kiln/harness", "claude",
            ];
            assert_eq!(stand.args(), expected);
            Ok(())
        })
    }

    /// W9 (Slates' A-520's third piece). Do: start an agent's container through rootless podman. Expect podman's own words
    /// for the bind (`bind-nonrecursive`), the container run as its root, the one id an unshared mount admits, and no git
    /// setting: every name of a hard-linked file is served at once.
    #[test]
    fn w9_rootless_podman_runs_the_container_as_its_root_in_podmans_words() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let stand = Stand::new();
            let podman = plan(Engine::Podman, tested(IdentityRule::ContainerRootAsMountingUser, HardLinkRule::EveryNameServedAtOnce));
            start_with(&stand, &made(), &podman).await?;
            let mount = format!("type=bind,source={MOUNT},target={MOUNT},bind-nonrecursive,bind-propagation=private");
            let workdir = format!("{MOUNT}/crates");
            let expected =
                ["run", "--detach", "--name", "kiln-a", "--mount", mount.as_str(), NO_NEW_PRIVILEGES, NO_MKNOD, "--user", "0:0", "--workdir", workdir.as_str(), "kiln/harness", "claude"];
            assert_eq!(stand.args(), expected);
            Ok(())
        })
    }

    /// W9 (AUD-29-66). Do: start a container whose source Slates finds replaced at the check, and one whose engine refuses
    /// the run. Expect the first never run, refused naming the replacement, the second refused in the engine's words, and
    /// each start's bind let go: a refused start leaves no bind behind.
    #[test]
    fn w9_a_source_no_longer_the_mount_verified_is_never_bound_and_a_refused_start_lets_its_bind_go() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let replaced = SourceFault::Replaced { source: MOUNT.to_owned(), verified: MountIdentity { mount: 7, device: 9 }, found: MountIdentity { mount: 8, device: 9 } };
            let stand = Stand { check: Err(replaced.clone()), ..Stand::new() };
            assert_eq!(start_with(&stand, &made(), &desktop()).await, Err(Refusal::Source(replaced)));
            assert_eq!(stand.said(), ["bind kiln-a", "check", "detach 41"]);
            let stand = Stand { run: Err("pull access denied".to_owned()), ..Stand::new() };
            assert_eq!(start_with(&stand, &made(), &desktop()).await, Err(Refusal::Engine("pull access denied".to_owned())));
            let ran = format!("run {DOCKER}");
            assert_eq!(stand.said(), ["bind kiln-a", "check", ran.as_str(), "detach 41"]);
            Ok(())
        })
    }

    /// W9. Do: start a container under a rootful Linux engine's profile (its ids reaching the volume as the host's own).
    /// Expect it refused before anything is asked: its mount would have to be shared with other users, as kiln doesn't
    /// mount it yet.
    #[test]
    fn w9_a_profile_that_needs_a_shared_mount_is_refused_before_anything_is_asked() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let stand = Stand::new();
            let rootful = plan(Engine::DockerEngine, tested(IdentityRule::ContainerIdsAsHostIds, HardLinkRule::EveryNameServedAtOnce));
            assert_eq!(start_with(&stand, &made(), &rootful).await, Err(Refusal::NeedsSharedMount));
            assert!(stand.said().is_empty(), "nothing is asked");
            Ok(())
        })
    }

    /// W9. Do: end a running container, then one whose engine won't remove it. Expect the container, then the bind, then
    /// the workspace's mount and volume, in that order; and a container that won't go keeping its bind and its mount.
    #[test]
    fn w9_a_containers_end_tears_down_the_container_then_the_bind_then_the_mount() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let running = Running { container: "c0ffee".to_owned(), bind: 41, program: DOCKER.to_owned() };
            let stand = Stand::new();
            end_with(&stand, &running, &made()).await?;
            assert_eq!(stand.said(), ["remove c0ffee", "detach 41", "take down kiln-a"]);
            let stand = Stand { remove: Err("container is busy".to_owned()), ..Stand::new() };
            assert!(end_with(&stand, &running, &made()).await.is_err());
            assert_eq!(stand.said(), ["remove c0ffee"], "a container that won't go keeps its bind and its mount");
            Ok(())
        })
    }

    /// W9. Do: apply Slates' read-only entry through each engine, then entries kiln can't apply as answered: another type, a
    /// word it doesn't know, a word said twice, no non-recursive bind, both accesses, an access against its `read_only`, and
    /// a path with a comma. Expect each engine's own read-only word, and each other refused typed: never a word dropped.
    #[test]
    fn w9_slates_entry_is_applied_word_for_word_or_refused() -> anyhow::Result<()> {
        let read_only = binding(true);
        assert_eq!(
            engine_mount(&read_only, Cli::Docker)?.mount,
            format!("type=bind,source={MOUNT},target={MOUNT},bind-recursive=disabled,bind-propagation=private,readonly")
        );
        assert_eq!(
            engine_mount(&read_only, Cli::Podman)?,
            EngineMount {
                mount: format!("type=bind,source={MOUNT},target={MOUNT},bind-nonrecursive,bind-propagation=private,ro=true"),
                options: vec![NO_NEW_PRIVILEGES.to_owned(), NO_MKNOD.to_owned()],
            }
        );
        let with = |change: &dyn Fn(&mut OciBinding)| {
            let mut entry = binding(false);
            change(&mut entry);
            engine_mount(&entry, Cli::Docker)
        };
        assert_eq!(with(&|e| e.mount_type = "volume".to_owned()), Err(Unappliable::NotABind { kind: "volume".to_owned() }));
        assert_eq!(with(&|e| e.options.push("noexec".to_owned())), Err(Unappliable::UnknownWord { word: "noexec".to_owned() }));
        assert_eq!(with(&|e| e.options.push(PRIVATE.to_owned())), Err(Unappliable::Repeated { word: PRIVATE.to_owned() }));
        assert_eq!(with(&|e| e.options.retain(|word| word != BIND)), Err(Unappliable::Recursive));
        assert_eq!(with(&|e| e.options.push(READ_ONLY.to_owned())), Err(Unappliable::Access));
        assert_eq!(with(&|e| e.read_only = true), Err(Unappliable::Access));
        assert_eq!(with(&|e| e.source = format!("{MOUNT},x")), Err(Unappliable::Path { path: format!("{MOUNT},x") }));
        Ok(())
    }

    /// A profile Slates judged: `engine` at Docker's socket, its rules `identity` and `hard_links`.
    fn judged(engine: Engine, identity: IdentityRule, hard_links: HardLinkRule) -> EngineProfile {
        use slates_api::container_engine::{Endpoint, RuntimeProfile};
        EngineProfile {
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
            tested: tested(identity, hard_links),
        }
    }

    /// W9: a runner's facts say whether its agents can run in containers, asking in the order that asks least. Given a
    /// machine with no workspaces, then no engine, then an engine Slates refuses (by its runtime, by an untested profile,
    /// on a platform where nothing is bound into a container), then a rootful Linux engine, then Docker Desktop and
    /// rootless podman. Expect each lack named typed and nothing asked past it; the refused untested profile still naming
    /// its engine; only the last two available.
    #[test]
    fn w9_a_runners_facts_say_whether_its_agents_can_run_in_containers_and_what_it_lacks() {
        use slates_api::container_engine::ProfileRefusal;
        let asked = std::cell::Cell::new(0_u32);
        let none = facts_of(
            false,
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
        let no_engine = facts_of(true, || None, |_: &str| {
            asked.set(asked.get() + 1);
            Err(EngineFault::TooLarge)
        });
        assert_eq!((no_engine.available, no_engine.missing, asked.get()), (false, Some(ContainerMissing::Engine), 0), "no profile is asked of an engine not found");

        let refused = |fault: EngineFault| facts_of(true, || Some(DOCKER.to_owned()), move |_: &str| Err(fault));
        let by_runtime = refused(EngineFault::Refused(ProfileRefusal::RuntimeUnsupported { runtime: "nerdctl".to_owned() }));
        assert_eq!((by_runtime.available, by_runtime.missing, by_runtime.program.as_deref(), by_runtime.engine), (false, Some(ContainerMissing::Profile), Some(DOCKER), None));
        assert!(by_runtime.why_not.as_deref().is_some_and(|w| w.starts_with("Slates refused this machine's container engine: ")), "{:?}", by_runtime.why_not);
        let untested = judged(Engine::DockerEngine, IdentityRule::ContainerIdsAsHostIds, HardLinkRule::EveryNameServedAtOnce);
        let by_profile = refused(EngineFault::Untested { endpoint: untested.endpoint.clone(), profile: untested.profile, refusal: ProfileRefusal::UserNamespaceUntested });
        assert_eq!((by_profile.missing, by_profile.engine.as_deref(), by_profile.version.as_deref()), (Some(ContainerMissing::Profile), Some("docker-engine"), Some("29.3.1")));
        let windows = refused(EngineFault::Unsupported { platform: "windows" });
        assert_eq!((windows.available, windows.missing), (false, Some(ContainerMissing::Platform)));

        let with = |profile: EngineProfile| facts_of(true, || Some(DOCKER.to_owned()), move |program: &str| {
            assert_eq!(program, DOCKER, "Slates judges the engine found");
            Ok(profile)
        });
        let rootful = with(judged(Engine::DockerEngine, IdentityRule::ContainerIdsAsHostIds, HardLinkRule::EveryNameServedAtOnce));
        assert_eq!((rootful.available, rootful.missing, rootful.engine.as_deref()), (false, Some(ContainerMissing::SharedMount), Some("docker-engine")));
        assert_eq!(rootful.why_not, Some(Refusal::NeedsSharedMount.to_string()));
        let desktop = with(judged(Engine::DockerDesktop, IdentityRule::HostUserThroughShare, HardLinkRule::OtherNamesStaleAfterTheFirstIsRemoved));
        assert_eq!(
            desktop,
            ContainerFacts { available: true, program: Some(DOCKER.to_owned()), engine: Some("docker-desktop".to_owned()), endpoint: Some("unix:///var/run/docker.sock".to_owned()), version: Some("29.3.1".to_owned()), why_not: None, missing: None }
        );
        let podman = with(judged(Engine::Podman, IdentityRule::ContainerRootAsMountingUser, HardLinkRule::EveryNameServedAtOnce));
        assert_eq!((podman.available, podman.engine.as_deref()), (true, Some("podman")));
    }
}
