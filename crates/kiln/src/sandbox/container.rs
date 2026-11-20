//! The container sandbox (docs/agentic.md W9; Slates' A-520): the seam's `Boundary::Container`, a harness run in an OCI
//! container of its own on its agent's workspace, through the machine's own container engine (`runner::docker::find`:
//! `docker`, else `podman`), the workspace's Slates volume bound into it from the machine's mount, never copied
//! ([`crate::workspace::container`]).
//!
//! **One workspace, one sandbox.** A [`ContainerSandbox`] serves the workspace it was made for (W3's [`Made`], mounted
//! already) through the engine whose profile Slates judged before the workspace was made (`container_engine::
//! engine_profile`): a launch runs in that workspace alone, its folder inside the workspace's mount, which the container
//! sees at the same path, so nothing is translated.
//!
//! **A keeper, and the harness beside it.** The container runs an idle keeper (`tail -f /dev/null` as its entrypoint, the
//! image's own set aside) started by `workspace::container::start` (Slates' bind, its source checked just before the
//! engine binds it, the run), and the harness runs in it through the engine's `exec -i`, whose stdio are this machine's
//! pipes, so its structured session is the one a process on the host speaks. Its environment reaches the container by
//! name alone (`--env NAME`, the value in the engine's command line's own environment), never on a command line another
//! user could read. The container's identity and git's settings are the profile's (`--user`, `core.createObject`), set
//! as it starts, which an `exec` inherits.
//!
//! **Its end.** As the harness ends, or is killed, the container is removed and then its bind let go
//! (`workspace::container::release`): the workspace's mount and volume stay, for W3 to take down when the workspace goes.
//!
//! **What it holds.** Nothing of the seam's limits yet: a launch asking a memory or process limit, a share other than an
//! even one, egress short of any host, a folder shared beside its own, a credential filled in at its egress, or anything
//! of a microVM's (components, volumes, a served folder) is refused `Unsupported`, typed, never taken and ignored; and no
//! signal reaches the harness (the engine's `exec` forwards none), so a signal is refused too.

use std::path::{Component, Path};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures::future::BoxFuture;
use kiln_seams::sandbox::{Boundary, Holds, Launch, Link, Output, Refusal, Run, Sandbox, Signal};
use slates_api::container_engine::EngineProfile;

use super::ProcRun;
use crate::workspace::container::{Plan, Running};
use crate::workspace::slates::Made;

/// What a container's run is held to: none of the seam's limits, and no signal (see the module's notes).
pub const HOLDS: Holds = Holds {
    memory: false,
    processes: false,
    weights: false,
    no_egress: false,
    egress_by_host: false,
    shares: false,
    signals: false,
    credentials: false,
    live: false,
    swaps: false,
    components: false,
    pools: false,
    volumes: false,
    served: false,
};

/// Format: the keeper's entrypoint and its arguments, which keep the container running, idle, while its harness runs
/// beside it (`tail`: POSIX, in every image with a shell's tools).
const KEEPER: &str = "tail";
/// Format: see [`KEEPER`].
const KEEPER_ARGS: [&str; 2] = ["-f", "/dev/null"];

/// The container sandbox of one workspace (see the module's notes).
#[derive(Debug, Clone)]
pub struct ContainerSandbox {
    /// The engine's command line, by its full path.
    program: String,
    /// Its profile, as Slates judged it.
    judged: EngineProfile,
    /// The workspace its runs work in, mounted already (W3).
    made: Made,
}

impl ContainerSandbox {
    /// The sandbox of workspace `made` through the engine `program` (its full path), whose profile Slates judged
    /// (`judged`).
    pub fn new(program: String, judged: EngineProfile, made: Made) -> Self {
        Self { program, judged, made }
    }
}

/// Whether `folder` lies in (or is) `mount`, compared by components, with no `..` to climb out by.
fn inside(folder: &str, mount: &str) -> bool {
    let folder = Path::new(folder);
    folder.is_absolute() && !folder.components().any(|c| matches!(c, Component::ParentDir)) && folder.starts_with(mount)
}

/// The keeper container's plan for `launch` in workspace `made` through `program` under `judged`, named `name`, or why
/// it's refused (see the module's notes).
fn plan_of(launch: &Launch, program: &str, judged: &EngineProfile, made: &Made, name: String) -> Result<Plan, Refusal> {
    HOLDS.check(launch)?;
    let image = match &launch.boundary {
        Boundary::Container { image } => image.trim(),
        Boundary::Host => return Err(Refusal::Unsupported("a run on the host, outside a container".into())),
        Boundary::MicroVm { .. } => return Err(Refusal::Unsupported("a run in a microVM".into())),
    };
    if image.is_empty() {
        return Err(Refusal::Invalid { what: "its image", why: "it names none".into() });
    }
    if launch.program.trim().is_empty() {
        return Err(Refusal::NoSuchProgram(launch.program.clone()));
    }
    let Some(folder) = &launch.folder else {
        return Err(Refusal::Invalid { what: "its folder", why: "a run in a container works in its workspace, and it names no folder there".into() });
    };
    if !inside(folder, &made.mount) {
        return Err(Refusal::Invalid { what: "its folder", why: format!("{folder} isn't in its workspace's mount {}", made.mount) });
    }
    if let Some((key, _)) = launch.env.iter().find(|(k, _)| k.is_empty() || k.contains(['=', '\0'])) {
        return Err(Refusal::Invalid { what: "its environment", why: format!("{key:?} can't name a variable") });
    }
    Ok(Plan {
        program: program.to_owned(),
        engine: judged.profile.engine,
        tested: judged.tested.clone(),
        name,
        image: image.to_owned(),
        entrypoint: Some(KEEPER.to_owned()),
        command: KEEPER_ARGS.map(str::to_owned).to_vec(),
        workdir: folder.clone(),
    })
}

/// The engine's `exec` of `launch`'s harness in container `container` at `workdir`: its environment by name alone, the
/// values given to the engine's command line itself (see the module's notes).
fn exec_args(launch: &Launch, container: &str, workdir: &str) -> Vec<String> {
    let mut args = vec!["exec".to_owned(), "--interactive".to_owned(), "--workdir".to_owned(), workdir.to_owned()];
    for (key, _) in &launch.env {
        args.extend(["--env".to_owned(), key.clone()]);
    }
    args.push(container.to_owned());
    args.push(launch.program.clone());
    args.extend(launch.args.iter().cloned());
    args
}

/// The container `running` and its bind let go, once: the workspace's mount stays (`workspace::container::release`).
async fn release_once(released: &AtomicBool, running: &Running, made: &Made) {
    if released.swap(true, Ordering::AcqRel) {
        return;
    }
    if let Err(e) = crate::workspace::container::release(running, made).await {
        tracing::warn!(container = %running.container, mount = %made.mount, error = %format!("{e:#}"), "a container sandbox's run ended, but its container wasn't removed: its bind and the workspace's mount stay with it");
    }
}

/// A harness's run in its container: the engine's `exec`, a process of this machine's, and the container it runs in,
/// released as the run ends.
struct ContainerRun {
    exec: ProcRun,
    running: Running,
    made: Made,
    released: Arc<AtomicBool>,
}

impl Run for ContainerRun {
    fn stdin<'a>(&'a self, bytes: Vec<u8>) -> BoxFuture<'a, Result<(), Refusal>> {
        self.exec.stdin(bytes)
    }

    fn close_stdin(&self) {
        self.exec.close_stdin();
    }

    fn next(&self) -> BoxFuture<'_, Option<Output>> {
        Box::pin(async move {
            let said = self.exec.next().await;
            if matches!(said, Some(Output::Exited(_)) | None) {
                release_once(&self.released, &self.running, &self.made).await;
            }
            said
        })
    }

    fn signal(&self, signal: Signal) -> Result<(), Refusal> {
        let _ = signal;
        Err(Refusal::Unsupported("a signal to a harness in a container: the engine's exec forwards none".into()))
    }

    fn kill(&self) {
        self.exec.kill();
        // The engine's exec ended leaves the harness running in its container: the container goes now, whether or not
        // its end is read.
        let (released, running, made) = (self.released.clone(), self.running.clone(), self.made.clone());
        drop(crate::rt::spawn(async move { release_once(&released, &running, &made).await }));
    }

    fn id(&self) -> Option<String> {
        Some(self.running.container.clone())
    }

    fn link(&self) -> BoxFuture<'_, Option<Link>> {
        Box::pin(async { None })
    }
}

impl Sandbox for ContainerSandbox {
    fn launch<'a>(&'a self, launch: Launch) -> BoxFuture<'a, Result<Box<dyn Run>, Refusal>> {
        Box::pin(async move {
            let name = format!("{}-{}", self.made.name, crate::util::now_ms());
            let plan = plan_of(&launch, &self.program, &self.judged, &self.made, name)?;
            let running = crate::workspace::container::start(&self.made, &plan).await.map_err(|refused| Refusal::Failed(refused.to_string()))?;
            let mut cmd = std::process::Command::new(&self.program);
            cmd.args(exec_args(&launch, &running.container, &plan.workdir));
            for (key, value) in &launch.env {
                cmd.env(key, value);
            }
            let released = Arc::new(AtomicBool::new(false));
            let exec = match crate::proc::spawn_duplex(cmd) {
                Ok(exec) => exec,
                Err(e) => {
                    release_once(&released, &running, &self.made).await;
                    return Err(Refusal::Failed(format!("the engine's exec didn't start: {e:#}")));
                }
            };
            let run: Box<dyn Run> = Box::new(ContainerRun { exec: ProcRun::new(exec), running, made: self.made.clone(), released });
            Ok(run)
        })
    }

    fn boundaries(&self) -> Vec<&'static str> {
        vec!["container"]
    }

    fn holds(&self) -> Holds {
        HOLDS
    }
}

/// Kind `kind`'s harness for session `spec` as a launch in a container of `image` (docs/agentic.md W9): its program by
/// its name on the image's `PATH`, started as kiln's adapter drives it (its base arguments, then what the session asks,
/// as a microVM's member is: `Kind::elsewhere`, so none of this machine's own paths, kiln's MCP config or hooks among
/// them), with the environment a boundary is given (the session's locale, its environment's variables, what kiln fixes),
/// its environment's secrets among them (`Kind::in_container`: its sign-in, granted by name to this session), in the
/// session's folder in its workspace's mount.
pub fn launch_of(kind: &crate::harness::kinds::Kind, spec: &crate::harness::session::Spec, image: &str) -> anyhow::Result<Launch> {
    let run = kind.in_container(spec)?;
    let mut args = kind.base_args();
    args.extend(run.args);
    Ok(Launch {
        args,
        folder: Some(crate::harness::session::resolve_cwd(spec.cwd.as_deref())),
        env: run.env,
        limits: kiln_seams::sandbox::Limits::NONE,
        egress: kiln_seams::sandbox::Egress::Any,
        boundary: Boundary::Container { image: image.to_owned() },
        ..Launch::of(kind.program, &[])
    })
}

/// Starts session `spec`'s harness of kind `kind` in a container of `image` on its workspace (docs/agentic.md W9): its
/// workspace volume, mounted already, is the one the container binds; the machine's engine is found and judged by Slates
/// first; then the container's sandbox launches it ([`ContainerSandbox`]), and its run is the session's pipes. Refused
/// where its session has no workspace volume of its own, or this machine's engine is none Slates holds evidence for.
pub async fn launch(kind: &crate::harness::kinds::Kind, spec: &crate::harness::session::Spec, image: &str) -> anyhow::Result<crate::proc::Duplex> {
    let made = spec.workspace.as_ref().filter(|w| w.kind == "volume").and_then(|w| w.made.clone()).filter(|m| !m.mount.is_empty()).ok_or_else(|| {
        anyhow::anyhow!("an agent in a container works in a workspace volume of its own, mounted on its machine, and this one has none (a person chose its folder, or its machine makes no volumes)")
    })?;
    let launch = launch_of(kind, spec, image)?;
    let (program, judged) = crate::rt::blocking(|| -> anyhow::Result<(String, EngineProfile)> {
        let program = crate::runner::docker::find().ok_or_else(|| anyhow::anyhow!("no container engine answers here: neither docker nor podman, or its daemon is down"))?;
        let judged = slates_api::container_engine::engine_profile(&program).map_err(|fault| anyhow::anyhow!("Slates refused this machine's container engine: {fault}"))?;
        Ok((program, judged))
    })
    .await??;
    let declared = super::held::Declared::of(&launch);
    let run = ContainerSandbox::new(program, judged, made).launch(launch).await.map_err(|refused| anyhow::anyhow!("its container: {refused}"))?;
    Ok(crate::proc::duplex_of_run(run, image.to_owned(), super::narrow::Reach::Whole { why: super::narrow::Whole::Container }, None, declared, None))
}

#[cfg(test)]
mod tests {
    use kiln_seams::sandbox::{Limits, Share};
    use slates_api::container_engine::{Endpoint, Engine, HardLinkRule, IdentityRule, RuntimeProfile, TestedProfile};

    use super::*;

    /// Format: the workspace's mount point.
    const MOUNT: &str = "/Users/u/.local/share/kiln/agent/workspaces/kiln-a";
    /// Format: the engine's command line.
    const DOCKER: &str = "/usr/local/bin/docker";

    fn made() -> Made {
        Made { volume: "0".repeat(32), name: "kiln-a".to_owned(), mount: MOUNT.to_owned(), ..Made::default() }
    }

    /// Docker Desktop's profile, as Slates judges it.
    fn judged() -> EngineProfile {
        EngineProfile {
            endpoint: "unix:///var/run/docker.sock".to_owned(),
            profile: RuntimeProfile {
                endpoint: Endpoint::UnixSocket("/var/run/docker.sock".to_owned()),
                engine: Engine::DockerDesktop,
                operating_system: "Docker Desktop".to_owned(),
                server_version: "29.3.1".to_owned(),
                rootless: false,
                user_namespace_remap: false,
                selinux_labels: false,
            },
            tested: TestedProfile { test: "T-4.13", description: "a stand-in's", identity: IdentityRule::HostUserThroughShare, hard_links: HardLinkRule::OtherNamesStaleAfterTheFirstIsRemoved },
        }
    }

    /// A harness's launch in a container of `image`, in the workspace's `crates` folder, as the host's would be but for
    /// its boundary.
    fn launch(image: &str) -> Launch {
        Launch {
            folder: Some(format!("{MOUNT}/crates")),
            env: vec![("ANTHROPIC_API_KEY".to_owned(), "sk-not-on-a-command-line".to_owned()), ("TERM".to_owned(), "dumb".to_owned())],
            limits: Limits::NONE,
            egress: kiln_seams::sandbox::Egress::Any,
            boundary: Boundary::Container { image: image.to_owned() },
            ..Launch::of("claude", &["--print", "--input-format", "stream-json"])
        }
    }

    /// W9. Do: plan a harness's launch in a container on its workspace, then build its engine's exec. Expect the keeper
    /// container at the launch's own folder in the workspace, the image's entrypoint set aside for the keeper's, the
    /// profile's rules carried; and the harness exec'd there with its arguments, its environment named on the command line
    /// and never its values.
    #[test]
    fn w9_a_harness_runs_beside_a_keeper_in_its_container_its_environment_named_never_valued() -> anyhow::Result<()> {
        let launch = launch("ghcr.io/kiln/harness-claude:1");
        let plan = plan_of(&launch, DOCKER, &judged(), &made(), "kiln-a-1".to_owned())?;
        assert_eq!(
            plan,
            Plan {
                program: DOCKER.to_owned(),
                engine: Engine::DockerDesktop,
                tested: judged().tested,
                name: "kiln-a-1".to_owned(),
                image: "ghcr.io/kiln/harness-claude:1".to_owned(),
                entrypoint: Some("tail".to_owned()),
                command: vec!["-f".to_owned(), "/dev/null".to_owned()],
                workdir: format!("{MOUNT}/crates"),
            }
        );
        let args = exec_args(&launch, "c0ffee", &plan.workdir);
        let expected = ["exec", "--interactive", "--workdir", &format!("{MOUNT}/crates"), "--env", "ANTHROPIC_API_KEY", "--env", "TERM", "c0ffee", "claude", "--print", "--input-format", "stream-json"].map(str::to_owned);
        assert_eq!(args, expected);
        assert!(!args.iter().any(|a| a.contains("sk-not-on-a-command-line") || a.contains("dumb")), "a value reached the command line: {args:?}");
        // The workspace's own root is a folder of it too.
        assert!(plan_of(&Launch { folder: Some(MOUNT.to_owned()), ..launch }, DOCKER, &judged(), &made(), "kiln-a-2".to_owned()).is_ok());
        Ok(())
    }

    /// W9. Do: launch through a workspace's container sandbox what it can't hold: another boundary, no image, no folder, a
    /// folder outside the workspace (a sibling whose name starts as the mount's, one climbing out by `..`), a variable that
    /// can't be named, and each of the seam's limits it holds none of. Expect each refused, typed, before any engine is asked.
    #[test]
    fn w9_a_container_sandbox_refuses_what_it_cant_hold_before_anything_is_asked() {
        let refused = |l: Launch| plan_of(&l, DOCKER, &judged(), &made(), "kiln-a-1".to_owned()).err();
        let unsupported = |r: Option<Refusal>| matches!(r, Some(Refusal::Unsupported(_)));
        let invalid = |r: Option<Refusal>, of: &str| matches!(r, Some(Refusal::Invalid { what, .. }) if what == of);
        assert!(unsupported(refused(Launch { boundary: Boundary::Host, ..launch("i") })));
        assert!(unsupported(refused(Launch { boundary: Boundary::MicroVm { image: "i".to_owned() }, ..launch("i") })));
        assert!(invalid(refused(launch("  ")), "its image"));
        assert!(matches!(refused(Launch { program: " ".to_owned(), ..launch("i") }), Some(Refusal::NoSuchProgram(_))));
        assert!(invalid(refused(Launch { folder: None, ..launch("i") }), "its folder"));
        for outside in [format!("{MOUNT}b/crates"), format!("{MOUNT}/../kiln-b"), "/Users/u/src/kiln".to_owned(), "crates".to_owned()] {
            assert!(invalid(refused(Launch { folder: Some(outside.clone()), ..launch("i") }), "its folder"), "{outside} was taken");
        }
        assert!(invalid(refused(Launch { env: vec![("A=B".to_owned(), "c".to_owned())], ..launch("i") }), "its environment"));
        assert!(unsupported(refused(Launch { limits: Limits::default(), ..launch("i") })));
        assert!(unsupported(refused(Launch { egress: kiln_seams::sandbox::Egress::Only(Vec::new()), ..launch("i") })));
        let share = Share { host: "/Users/u/.ssh".to_owned(), at: "/Users/u/.ssh".to_owned(), writable: false, members: Vec::new() };
        assert!(unsupported(refused(Launch { shares: vec![share], ..launch("i") })));
    }

    /// W9. Do: make the container launch of a Claude session resumed with a model, in its workspace's folder, with host
    /// variables and kiln's socket in its environment. Expect the harness by its name on the image's `PATH`, its stream
    /// flags then the session's own, its folder the session's, the image's container its boundary, and none of the host's
    /// variables nor kiln's socket handed in: only what a boundary's run is given.
    #[test]
    fn w9_a_sessions_container_launch_is_its_harness_by_name_with_what_a_boundary_gives_it() -> anyhow::Result<()> {
        use crate::harness::kinds::Kind;
        use crate::harness::session::Spec;
        let claude = Kind::get("claude").ok_or_else(|| anyhow::anyhow!("no harness claude"))?;
        let env = vec![("PATH".to_owned(), "/host/bin".to_owned()), ("LANG".to_owned(), "fr_FR.UTF-8".to_owned()), ("KILN_REACH_SOCK".to_owned(), "/host/sock".to_owned())];
        let spec = Spec { kind: "claude".into(), cwd: Some(format!("{MOUNT}/crates")), env, resume: Some("conversation-1".into()), model: Some("claude-opus-5-5".into()), allow_bypass: false, ..Spec::default() };
        let launch = launch_of(claude, &spec, "ghcr.io/org/agents:1")?;
        assert_eq!(launch.program, "claude");
        let base = claude.base_args();
        assert_eq!(launch.args.get(..base.len()), Some(base.as_slice()), "its stream flags first: {:?}", launch.args);
        assert_eq!(launch.args.get(base.len()..), Some(["--model", "claude-opus-5-5", "--resume", "conversation-1"].map(str::to_owned).as_slice()));
        assert_eq!((launch.folder.as_deref(), &launch.boundary), (Some(format!("{MOUNT}/crates").as_str()), &Boundary::Container { image: "ghcr.io/org/agents:1".to_owned() }));
        assert!(launch.env.contains(&("LANG".to_owned(), "fr_FR.UTF-8".to_owned())));
        assert!(!launch.env.iter().any(|(k, _)| k == "PATH" || k.starts_with("KILN_")), "no host variable, no kiln socket: {:?}", launch.env);
        // A secret its environment grants it by name reaches the container's run as a variable named on the engine's
        // command line, its value never there (`exec_args`).
        use crate::harness::agentenv::{Base, Resolved, Set};
        let granted = Spec { agentenv: Some(Resolved { id: "e".into(), name: "sign-in".into(), version: 1, base: Base::User, vars: vec![("ANTHROPIC_API_KEY".into(), Set::Secret("not-a-real-key".into()))] }), ..spec.clone() };
        let signed = launch_of(claude, &granted, "ghcr.io/org/agents:1")?;
        assert!(signed.env.contains(&("ANTHROPIC_API_KEY".to_owned(), "not-a-real-key".to_owned())));
        let args = exec_args(&signed, "c0ffee", MOUNT);
        assert!(args.windows(2).any(|w| w == ["--env", "ANTHROPIC_API_KEY"]) && !args.iter().any(|a| a.contains("not-a-real-key")), "{args:?}");
        // What it is, a container's sandbox holds: no limit or narrowed egress it would refuse.
        assert!(plan_of(&launch, DOCKER, &judged(), &made(), "kiln-a-1".to_owned()).is_ok());
        Ok(())
    }
}
