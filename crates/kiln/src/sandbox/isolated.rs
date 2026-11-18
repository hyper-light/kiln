//! Isolated agents (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 17; docs/isolation.md): a
//! session whose spec says `isolation` runs its harness in a microVM of its own, as a member of an
//! Agentfile's image, never as a process on its machine.
//!
//! - **Its image** is made from the Agentfile its start chose (`Isolation::image`): kiln's own for its
//!   harness by its name (`harness-<id>`), or one of a person's own by its id (`agentfile:<id>`), whose
//!   text kiln's server read from their Agentfiles as it started; none chosen, kiln's own for its
//!   harness ([`source_of`]). It boots pinned by its digest, built once for its content
//!   (`super::image`).
//! - **Its harness** is that image's HARNESS member, run by its image's command, which runs it as
//!   kiln's adapter drives it; what the session adds (its model, its mode, the conversation it
//!   resumes) are the arguments past it ([`Kind::elsewhere`]). Its stdio are the member's streams,
//!   which the session's adapter speaks to as to a process's pipes (`super::held`).
//! - **Its folder** is the session's own, at its own path, so what the harness says of its files
//!   names them as the machine does.
//! - **What it sees of the machine** ([`Launch::shares`]): its project at its own path (a Windows
//!   folder, which no Linux path names, under `/work` by its name: [`guest_path`]), and its
//!   harness's home, its person's volumes kept on the machine (`harness::isolated::home::Layout`), each
//!   at its path in the microVM, as kiln's Agentfile for its harness declares them, or a person's own
//!   ([`kept_volumes`], its run's environment read from its image: [`harness_env`]); and the plugin
//!   folders its harness loads on the machine, read-only at their own paths ([`plugin_folders`]).
//! - **What it reaches** past its microVM is its image's destinations narrowed to its session's own
//!   provider's or sign-in's ([`reach_of`], `super::narrow`), else its image's whole and why.
//! - **Its sign-in** (plan step 33; Shards' D242, step 8): Claude Code's and Codex's, whose CLIs Shards' injector
//!   takes sign-ins of, are granted their person's sign-in as they launch ([`sign_in_of`], `secrets::signin`): the
//!   token response taken outside the microVM and kept by kiln's broker, the CLI given placeholders, its access token
//!   filled in at its egress toward its provider's hosts alone. Where its microVMs lease no credentials, it signs in
//!   itself, said.
//! - **kiln's MCP** reaches it over its link's own `mcp` lane (Shards D254; #75 step 3), never the network: its
//!   harness's config names its microVM's bridge, and this runner serves each connection to it in its own process
//!   ([`bridge`]), where its microVMs carry the lane (`super::Microvms::mcp`); where they don't, its harness starts
//!   without it, said.
//! - **What it can't do yet**, refused rather than done another way: move to another machine
//!   ([`NO_MOVE`]), switch to another harness where its microVMs swap none in place ([`NO_SWITCH`]), or swap a
//!   component in where they lay none ([`NO_COMPONENTS`]); where they do, a switch swaps its member there
//!   ([`ready_to_swap`], [`launch`]'s `swap_from`), and a component is laid at its launch (`Launch::components`).

use std::path::Path;

use kiln_seams::sandbox::{Boundary, Egress, Launch, Limits, Refusal, Share, VolumeCause};

use super::Microvms;
use super::image::{Images, Source};
use super::narrow::{Reach, Whole};
use crate::agentfiles::builtin;
use crate::harness::isolated::home;
use crate::harness::kinds::{Kind, Protocol};
use crate::harness::session::{Isolation, Spec};
use crate::secrets::signin;

/// Why an isolated agent doesn't move to another machine.
pub const NO_MOVE: &str = "an isolated agent doesn't move to another machine yet: its microVM and what its harness keeps there are its machine's; end it, or start it anew there";

/// Why an isolated agent's component isn't swapped where its machine's microVMs lay none (kiln plan 01a1dd2e…, P6c.3).
pub const NO_COMPONENTS: &str = "An isolated agent's components are swapped only where its machine's microVMs lay them over its image's own, and these don't yet (Hex's wire calls, kiln plan 01a1dd2e…, P6c.2). Its image's own run on.";

/// Why an isolated agent doesn't switch to another harness where its machine's microVMs swap none in place.
pub const NO_SWITCH: &str = "An isolated agent switches harness only where its machine's microVMs swap one in place, and these don't yet (Hex's wire calls, kiln plan 01a1dd2e…, P6c.2). End it, and start the other harness isolated.";

/// Whether `kind`'s harness can be swapped into a microVM session of `spec` (kiln plan 01a1dd2e…, P6c.5): its image
/// (the Agentfile `spec` names for it, kiln's own where none) pinned in the machine's store, built there once where it
/// isn't, before the session's harness is handed on, so a switch that can't go leaves it as it was. Why not, in words.
pub async fn ready_to_swap(kind: &Kind, spec: &Spec, vms: &Microvms, secrets: &[crate::secrets::mine::BuildSecret]) -> Result<(), String> {
    let source = source_of(kind, spec)?;
    vms.images.pinned(source, secrets).await.map(|_| ()).map_err(|r| format!("{}'s image, {}: {r}", kind.name, source.name()))
}

/// How a start names one of a person's own Agentfiles as an isolated agent's image (the console's
/// `views/agentfiles/choose.ts`, `keptImage`).
pub const KEPT: &str = "agentfile:";

/// The Agentfile an isolated session of `kind` is made from, as its start named it: one kiln ships by
/// its name (`harness-<id>`, for its own harness alone), or one of a person's own by its id
/// (`agentfile:<id>`), its text as kiln's server read it from their Agentfiles as it started
/// (`Spec::agentfile`); none named, kiln's own for its harness. Else why not, in words.
pub fn source_of<'s>(kind: &Kind, spec: &'s Spec) -> Result<Source<'s>, String> {
    let isolation = spec.isolation.as_ref().ok_or("the session isn't isolated")?;
    if isolation.kind != Isolation::MICROVM {
        return Err(format!("isolation {:?} isn't one kiln holds agents in: \"microvm\"", isolation.kind));
    }
    let own = builtin::ALL.iter().find(|b| b.harness == kind.id);
    let Some(name) = isolation.image.as_deref().map(str::trim).filter(|n| !n.is_empty()) else {
        return own.map(Source::Builtin).ok_or_else(|| format!("kiln ships no Agentfile for {}: choose one of yours", kind.name));
    };
    if let Some(id) = name.strip_prefix(KEPT) {
        if !crate::util::is_id(id) {
            return Err(format!("{name:?} names no Agentfile of yours: `agentfile:` and its id, 32 lowercase hex digits"));
        }
        let text = spec.agentfile.as_deref().ok_or_else(|| format!("{name}'s text didn't come with its start: start it again"))?;
        return Ok(Source::Kept { id, text, beside: own });
    }
    match builtin::ALL.iter().find(|b| b.name == name) {
        Some(b) if b.harness == kind.id => Ok(Source::Builtin(b)),
        Some(b) => Err(format!("{name} is kiln's Agentfile for {}, not {}", b.harness, kind.name)),
        None => Err(format!("{name:?} is neither an Agentfile kiln ships (`harness-<harness>`) nor one of yours (`agentfile:<id>`)")),
    }
}

/// The seam's launch of `kind` for an isolated `spec`, its microVM booting `image`: its image's
/// harness member, the arguments past its image's command, in the session's folder at its own path,
/// which it sees there (its project, [`project_share`]: [`launch`] adds its harness's home), with
/// its image's grants (which [`launch`] narrows to what its session uses: [`reach_of`]) and the
/// microVM sized as its boundary sizes one (the person's sizes reach it through its placement: plan
/// step 32). A project [`kept_out`] keeps out, `kiln`'s state among it, is refused before anything starts: its
/// folder, or the one its workspace of its own lies over ([`given`]).
pub fn launch_of(kind: &Kind, spec: &Spec, image: String, kiln: &[&Path]) -> anyhow::Result<Launch> {
    let cwd = crate::harness::session::resolve_cwd(spec.cwd.as_deref());
    anyhow::ensure!(std::path::Path::new(&cwd).is_dir(), "{cwd} is not a directory on this machine");
    let given = given(spec, Path::new(&cwd));
    if let Some(why) = kept_out(&given, kiln) {
        anyhow::bail!("an isolated agent can't work in {}: {why}", given.display());
    }
    let run = kind.elsewhere(spec)?;
    let harness = kiln_seams::sandbox::Member(run.member.to_owned());
    // Its project: the folder itself, shared; or, where its workspace of its own is a Slates volume made for it with no
    // mount (`crate::workspace::prepare`), that volume, served to its microVM by Slates over Slates' own virtio-fs device
    // and given to its harness, which works in it from its first instruction ([`served_project`]).
    let (folder, shares, served) = match served_project(spec, &cwd, &harness).map_err(anyhow::Error::msg)? {
        Some((served, folder)) => (folder, Vec::new(), vec![served]),
        None => {
            let project = project_share(&cwd).map_err(anyhow::Error::msg)?;
            (project.at.clone(), vec![project], Vec::new())
        }
    };
    // Its project's top-level directory, where a folder is plugged in live ([`launch`] adds its person's volumes' and its
    // session's root).
    let roots = plug_root_of(&folder).into_iter().collect();
    let volumes = volumes_of(&spec.volumes, &harness)?;
    Ok(Launch {
        program: run.member.to_owned(),
        args: run.args,
        folder: Some(folder),
        shares,
        served,
        env: run.env,
        limits: Limits::NONE,
        egress: Egress::Any,
        boundary: Boundary::MicroVm { image },
        // No credential granted by reference (D242): kiln's store holds none of a harness's but the sign-ins it
        // captures, which [`launch`] grants as sign-ins ([`sign_in_of`]), not as credentials.
        credentials: Vec::new(),
        plug_roots: roots,
        // The most egress it may be granted live: its person's or org's policy, as kiln's server set it at its start.
        egress_ceiling: spec.egress_ceiling.clone(),
        // The components a person swapped into its session, laid over its image's own (kiln plan 01a1dd2e…, P6c.3).
        components: spec.components.clone(),
        // Its agent's volumes (P6.2), there as it starts, before its session hands it a turn.
        volumes,
    })
}

/// How a start that swapped its member into the microVM its session ran in started (the one-click harness switch,
/// P6c.5), in a start's record beside Hex's `started_from` words (`booted`, `saved`, `restored`, `replaced`, `warm`).
pub const SWAPPED: &str = "swapped";

/// Starts `kind`'s harness for isolated `spec` in the machine's microVMs `vms`: what its microVM
/// reaches of its image's destinations ([`reach_of`], read from its person's home under `homes`, the
/// machine's), its image pinned by its digest (`image::Images::pinned`, built where their store holds
/// none of its content), its person's home made and given it ([`home_of`]), then its member's run as
/// the `Duplex` its session drives (`crate::proc::duplex_of_run`), saying where its harness keeps its
/// conversations; or why not, in the images', the home's or the sandbox's own words. What its launch
/// takes of the session, and of a person's own Agentfile, is checked before anything is built. With no
/// person or no homes kept on this machine its harness has no home of theirs, and keeps nothing. The
/// plugin folders its harness loads for `session` are given it as on the machine ([`plugin_folders`]).
/// It launches in the box placement holds for it (`Holder::box_of`, else placement's numbering). A
/// project that builds with its host's own tools is refused before anything is built (step 18,
/// `host_tools`); its harness gets kiln's variables as a microVM's member does
/// (`isolated::env::member_env`) and kiln's MCP through its link's lane ([`offer_mcp`]), served by this runner for
/// `session` ([`bridge`], its asks `host`'s), where its microVMs carry the lane. A build of its image is
/// given `secrets` (`Spec::build_secrets`, P4.7). With `swap_from`, the harness a session switches from in a microVM of
/// its own (kiln plan 01a1dd2e…, P6c.5), its member is swapped for this one there (`Run::swap`): the same microVM, none
/// started, its sign-in granted as a launch's is or the swap refused (`Microvms::swap`); refused, why, and that
/// harness's run is let go of.
#[allow(clippy::too_many_arguments, reason = "a launch's own, and the run a switch swaps it into")]
pub async fn launch(kind: &Kind, spec: &Spec, session: &str, vms: &Microvms, homes: Option<&std::path::Path>, host: Option<std::sync::Arc<crate::harness::host::Host>>, secrets: &[crate::secrets::mine::BuildSecret], swap_from: Option<crate::proc::Duplex>) -> anyhow::Result<crate::proc::Duplex> {
    use crate::harness::isolated::{env, host_tools};
    let source = source_of(kind, spec).map_err(anyhow::Error::msg)?;
    // kiln's own state, which its project is, holds or lies in none of: where its microVMs are held, its people's volumes,
    // and its server's data folder (kiln.db, its secrets and tokens), as its runner keeps it.
    let server = host.as_ref().and_then(|h| h.server_data().map(Path::to_path_buf));
    let kiln: Vec<&Path> = vms.state().into_iter().chain(homes).chain(server.as_deref()).collect();
    let mut launch = launch_of(kind, spec, source.name(), &kiln)?;
    let folder = crate::harness::session::resolve_cwd(spec.cwd.as_deref());
    let files = crate::rt::blocking(move || host_tools::tracked(Path::new(&folder))).await?;
    host_tools::check(files.iter().map(String::as_str))?;
    for (k, v) in env::member_env(session, kind) {
        if !launch.env.iter().any(|(e, _)| *e == k) {
            launch.env.push((k, v));
        }
    }
    if let Source::Kept { text, .. } = source {
        kept_volumes(text, kind.id).map_err(|e| anyhow::anyhow!("{}: {e}", source.name()))?;
    }
    let reach = match source {
        Source::Kept { .. } => Reach::Whole { why: Whole::PersonsOwn },
        Source::Builtin(b) => {
            let person = spec.isolation.as_ref().and_then(|i| i.person.clone());
            // Its project's own configuration is read where the machine has it: its folder, or the folder a project Slates
            // serves lies over, which its volume reads through until its agent writes there.
            let project = match (served_over(spec), launch.served.first()) {
                (Some((_, over)), Some(served)) => Some(Share { host: over.to_owned(), at: served.at.clone(), writable: true, members: Vec::new() }),
                _ => launch.shares.first().cloned(),
            };
            let (model, homes, env) = (spec.model.clone(), homes.map(std::path::Path::to_path_buf), launch.env.clone());
            crate::rt::blocking(move || reach_of(b, person.as_deref(), model.as_deref(), homes.as_deref(), project.as_ref(), &env)).await?
        }
    };
    let text = match source {
        Source::Builtin(b) => b.text,
        Source::Kept { text, .. } => text,
    };
    // Where it doesn't parse, its build says so in Shards' words; its egress stays as its image's.
    launch.egress = egress_of(&reach, granted(text).ok().flatten());
    let super::image::Pinned { image, built } = vms.images.pinned(source, secrets).await.map_err(|r| anyhow::anyhow!("{}'s image, {}: {r}", kind.name, source.name()))?;
    launch.boundary = Boundary::MicroVm { image: image.clone() };
    let person = spec.isolation.as_ref().and_then(|i| i.person.as_deref());
    let (mounts, layout) = match (homes, person) {
        (Some(root), Some(person)) => {
            let (mounts, layout) = home_of(root, person, kind, source, &vms.images, &image).await?;
            (mounts, Some(layout))
        }
        _ => (Vec::new(), None),
    };
    let kept_in = layout.as_ref().and_then(home::Layout::kept_in);
    let sign_ins = match signin::Provider::of_harness(kind.id) {
        Some(provider) => sign_in_of(provider, person, layout.as_ref(), &launch.egress, vms).await?,
        None => Vec::new(),
    };
    launch.shares.extend(mounts.into_iter().map(|m| Share { host: m.host.to_string_lossy().into_owned(), at: m.guest, writable: true, members: Vec::new() }));
    // Where a folder a person gives it while it runs is plugged in live (Shards' `ImageSpec::plug_roots`, D271): the
    // top-level directories of its project and its person's volumes, and its session's own root, where kiln puts such a
    // folder by default (`harness::overrides`), each once.
    let mut roots: Vec<String> = Vec::new();
    for root in launch.shares.iter().filter(|s| s.writable).map(|s| s.at.as_str()).chain(launch.served.iter().filter(|s| s.writable).map(|s| s.at.as_str())).filter_map(plug_root_of).chain([VOLUMES.to_owned()]) {
        if !roots.contains(&root) {
            roots.push(root);
        }
    }
    launch.plug_roots = roots;
    for dir in plugin_folders(kind, session) {
        let at = guest_path(&dir, PLUGINS).map_err(anyhow::Error::msg)?;
        launch.args.extend(["--plugin-dir".to_owned(), at.clone()]);
        launch.shares.push(Share { host: dir, at, writable: false, members: Vec::new() });
    }
    // kiln's MCP once its person's volume is made (agy's config is kept there), where its lane is carried.
    let lanes = match vms.mcp() {
        Ok(lanes) => Some(lanes),
        Err(why) => {
            tracing::warn!(harness = kind.id, %why, "it starts without kiln's MCP");
            None
        }
    };
    if lanes.is_some() {
        offer_mcp(kind, layout.as_ref(), &mut launch.args).await;
    }
    // Every folder it starts with is its harness's own: given to that member at its start, where it sees it
    // (Shards' share member scope). A start share given to no member reaches its microVM's root alone, which
    // its harness never sees: its project, its working directory, wasn't there (kiln plan P1.4 on HVF).
    let member = kiln_seams::sandbox::Member(launch.program.clone());
    for share in &mut launch.shares {
        share.members = vec![member.clone()];
    }
    let declared = crate::sandbox::held::Declared::of(&launch);
    // Its microVM's size as its isolation gives it (plan step 32; a deployment experiment's variant, P7.1): whole vCPUs
    // and MiB, rounded up.
    let size = super::pools::size_of(spec.isolation.as_ref());
    let asked = std::time::Instant::now();
    let (run, swapped) = match swap_from {
        // A switch: its member swapped for this one in the microVM it runs in, granted its sign-in as a launch is, never
        // left to sign in itself there (`Microvms::swap`).
        Some(from) => {
            let held = from.boundary().ok_or_else(|| anyhow::anyhow!("{} has no microVM to be swapped into", kind.name))?;
            let swapped = vms.swap(&held, launch, &sign_ins).await.map_err(|r| anyhow::anyhow!("{} couldn't be swapped into its microVM ({image}): {r}", kind.name));
            drop(from);
            (swapped?, true)
        }
        None => {
            let name = match (&spec.claim, crate::reach::node::global()) {
                (Some(claim), Some(reach)) => reach.box_of(claim.clone()).await,
                _ => None,
            };
            (vms.launch(name, launch, size, &sign_ins).await.map_err(|r| anyhow::anyhow!("{} couldn't start in a microVM of {image}: {r}{}", kind.name, again(&r)))?, false)
        }
    };
    let us = u64::try_from(asked.elapsed().as_micros()).unwrap_or(u64::MAX);
    let (from, meter) = match (&vms.hex, run.id()) {
        // A swap started no microVM: its start is its member's in the one it ran in (`swapped`), never that
        // microVM's own start, which a split's strata would otherwise count it as (P7.1, P6c.5).
        (Some(hex), Some(vm)) => (if swapped { Some(SWAPPED) } else { hex.started_from(&vm).await.map(super::hexvm::said) }, hex.meter(&vm)),
        _ => (swapped.then_some(SWAPPED), None),
    };
    let built_us = built.map(|b| u64::try_from(b.as_micros()).unwrap_or(u64::MAX));
    let started = super::held::Started { us, from, meter, built_us };
    // Its microVM's warm pool on this machine, named from its starts (kiln plan 01a1dd2e…, P3.2); a swap started none.
    if !swapped {
        let warm = from == Some(super::hexvm::said(hex_api::StartedFrom::Warm));
        vms.pools.started(kiln_seams::sandbox::Pool { image: image.clone(), size }, warm).await;
    }
    if let Some(lanes) = lanes {
        let here = crate::reach::node::global().map(|h| crate::reach::local::Here { handle: h.clone(), me: h.name().to_owned(), host, session: session.to_owned() });
        let served = run.id().ok_or_else(|| std::io::Error::other("its run names no microVM")).and_then(|vm| bridge(lanes, vm, here, crate::harness::session::resolve_cwd(spec.cwd.as_deref())));
        if let Err(e) = served {
            tracing::warn!(harness = kind.id, error = %e, "kiln's MCP isn't served to it");
        }
    }
    // Placement reads its use from its VM's meter from now on (#73 part 7 slice 3).
    if let (Some(claim), Some(reach), Some(group)) = (&spec.claim, crate::reach::node::global(), super::group_of(vms, run.as_ref())) {
        reach.session_grouped(claim.clone(), group).await;
    }
    Ok(crate::proc::duplex_of_run(run, image, reach, kept_in, declared, Some(started)))
}

/// Its agent's `given` volumes as its launch gives them to its harness `member` (P6.2): each by its name in its
/// machine's store, its bound the most its definition lets it hold, read-write at its path, there as it starts, made
/// where it isn't and kept past its session (Shards' D271, D281). One its server named no bound for (a server older
/// than bounds) is refused rather than given unbounded.
pub fn volumes_of(given: &[crate::agentdefs::volumes::Given], member: &kiln_seams::sandbox::Member) -> anyhow::Result<Vec<kiln_seams::sandbox::Volume>> {
    given
        .iter()
        .map(|v| {
            let bound = std::num::NonZeroU64::new(v.bytes).ok_or_else(|| anyhow::anyhow!("its agent volume {} came with no bound (its server is older than volumes' bounds), and a volume isn't given unbounded", v.name))?;
            Ok(kiln_seams::sandbox::Volume { name: v.store.clone(), bound, at: v.at.clone(), members: vec![member.clone()], read_only: false })
        })
        .collect()
}

/// When a start its machine's store refused for one of its agent's volumes (P6.2) may be asked again, by the refusal's
/// cause: a clearing's once it has answered, a bound's once no other session of the agent there holds it. None for any
/// other refusal.
fn again(r: &Refusal) -> &'static str {
    match r {
        Refusal::Volume { cause: VolumeCause::Clearing, .. } => " (start it again once that volume's clearing has answered)",
        Refusal::Volume { cause: VolumeCause::Bound, .. } => " (its volume's bound changed: start it again once no other session of this agent on this machine holds that volume)",
        Refusal::Volume { cause: VolumeCause::InUse | VolumeCause::Unknown, .. } | Refusal::NoSuchProgram(_) | Refusal::Invalid { .. } | Refusal::Unsupported(_) | Refusal::Full { .. } | Refusal::Ended => "",
    }
}

/// Serves kiln's MCP to run `vm`'s harness in this runner's own process (Shards D254; #75 step 3): each connection
/// it makes to its microVM's bridge, taken from `lanes` as it comes, on a thread of its own
/// (`reach::mcp::serve_lane`: its calls asked here for `here`'s session, `cwd` its folder on this machine), until
/// `lanes` says its run is over. No token is given anything: the lane says whose a connection is. Where this
/// process runs no reach node (`here` none) a connection is let go of at once, said, so its harness finds kiln's MCP
/// gone rather than waiting on it.
pub fn bridge(lanes: std::sync::Arc<dyn super::Lanes>, vm: String, here: Option<crate::reach::local::Here>, cwd: String) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new().name("kiln-mcp-lane".into()).spawn(move || {
        loop {
            let conn = match lanes.next(&vm) {
                Ok(conn) => conn,
                Err(kiln_seams::sandbox::Refusal::Ended) => return,
                Err(why) => {
                    tracing::warn!(%vm, %why, "kiln's MCP takes no more of its harness's connections");
                    return;
                }
            };
            let Some(here) = here.clone() else {
                tracing::warn!(%vm, "a connection to kiln's MCP let go of: this process runs no reach node to serve it");
                continue;
            };
            let (cwd, session) = (cwd.clone(), here.session.clone());
            let served = std::thread::Builder::new().name("kiln-mcp-conn".into()).spawn(move || {
                let super::McpConnection { from, to } = conn;
                if let Err(e) = crate::reach::mcp::serve_lane(here, cwd, &mut std::io::BufReader::new(from), to) {
                    tracing::warn!(%session, error = %format!("{e:#}"), "a connection to kiln's MCP ended in error");
                }
            });
            if let Err(e) = served {
                tracing::warn!(%vm, error = %e, "a connection to kiln's MCP let go of: no thread to serve it");
            }
        }
    })
}

/// Where a microVM sees a Windows machine's project folder: beneath this, by its own name ([`guest_path`]).
pub const PROJECTS: &str = "/work";

/// Where a microVM sees a Windows machine's plugin folders: beneath this, each by its own name.
pub const PLUGINS: &str = "/plugins";

/// Where a microVM sees the folders a person gives its running session (`harness::overrides`), by default: beneath
/// this, each by its own name, one of the plug roots its launch declares ([`launch`]), so each is taken live.
pub const VOLUMES: &str = "/volumes";

/// The plug root path `at` of a microVM lies beneath (Shards' `shards_abi::run::plug::root_name`, D271): its top-level
/// directory, unless that is one its init keeps its own ([`INITS_OWN`]), which no plug root names.
pub fn plug_root_of(at: &str) -> Option<String> {
    let first = at.strip_prefix('/')?.split('/').next().filter(|f| !f.is_empty() && *f != "." && *f != ".." && !f.contains('\0'))?;
    (!INITS_OWN.contains(&first)).then(|| format!("/{first}"))
}

/// The top-level directories a microVM's init keeps its own, which no plug root names (Shards' `plug::REFUSED`, D271).
pub const INITS_OWN: &[&str] = &["proc", "sys", "dev", "tmp", "etc", "run"];

/// Where a microVM sees folder `host` of the machine, given its running session (`harness::overrides`) where none was
/// asked: beneath [`VOLUMES`] by its own name, its last part. Why not, for one with no name to go by.
pub fn volume_at(host: &str) -> Result<String, String> {
    let name = host.rsplit(['\\', '/']).find(|c| !c.is_empty()).filter(|c| *c != "." && *c != ".." && !c.ends_with(':') && *c != "?");
    let name = name.ok_or_else(|| format!("{host:?} has no folder's name to plug it in by"))?;
    Ok(format!("{VOLUMES}/{name}"))
}

/// Where a microVM sees folder `host` of the machine, as Shards plugs one in (D235): a Unix path at its
/// own path, so what the harness says of its files names them as the machine does; a Windows one
/// (`C:\…`, `\\?\C:\…`, `\\server\share\…`), which no Linux path names, beneath `under` by its own
/// name, its last part. Hex takes a Windows folder as a share's source, but a share's destination and a
/// member's working folder are the guest's Linux paths, absolute and plain, and Hex maps none for kiln
/// (Hex's lead, through ROOT, 2026-11-13). Why not, for a path of neither kind, or a Windows one with
/// no name to go by (a drive's root).
pub fn guest_path(host: &str, under: &str) -> Result<String, String> {
    if host.starts_with('/') {
        return Ok(host.to_owned());
    }
    let windows = host.starts_with(r"\\") || host.get(1..3).is_some_and(|s| s == ":\\" || s == ":/");
    if !windows {
        return Err(format!("{host:?} is neither a Unix path nor a Windows one"));
    }
    let name = host.rsplit(['\\', '/']).find(|c| !c.is_empty()).filter(|c| *c != "." && *c != ".." && !c.ends_with(':') && *c != "?");
    let name = name.ok_or_else(|| format!("{host:?} has no folder's name to plug it in by"))?;
    Ok(format!("{}/{name}", under.trim_end_matches('/')))
}

/// Where an isolated session's harness sees the project kiln holds for it on this machine, where the two
/// differ: a Windows machine's folder, which its microVM sees at [`guest_path`]'s `/work/<name>` (a Unix
/// one it sees at its own path). The harness knows only its microVM's paths, so what tells it of a
/// folder (Codex's and ACP's requests) or names something after one for it (Claude Code's folder of a
/// project's conversations) says the folder as [`Seen::said`] has it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Seen {
    host: String,
    at: String,
}

impl Seen {
    /// `spec`'s, for an isolated session whose microVM sees its project elsewhere; none for a harness on
    /// its machine, for a Unix project, and for one [`guest_path`] refuses, which [`launch_of`] refuses
    /// before its harness is told anything.
    pub fn of(spec: &Spec) -> Option<Self> {
        spec.isolation.as_ref()?;
        // A project Slates serves is seen where the folder its volume lies over is ([`served_project`]).
        let host = served_over(spec).map_or_else(|| crate::harness::session::resolve_cwd(spec.cwd.as_deref()), |(_, over)| over.to_owned());
        let at = guest_path(&host, PROJECTS).ok()?;
        (at != host).then(|| Self { host: host.trim_end_matches(['\\', '/']).to_owned(), at })
    }

    /// `path`, a folder or file of this machine's, as the harness sees it: its project and what's beneath
    /// it at its place in the microVM, with `/` between its parts; anything else as it is.
    pub fn said(&self, path: &str) -> String {
        match path.strip_prefix(self.host.as_str()) {
            Some(rest) if rest.is_empty() || rest.starts_with(['\\', '/']) => {
                let rest = rest.replace('\\', "/");
                format!("{}{}", self.at, rest.trim_end_matches('/'))
            }
            _ => path.to_owned(),
        }
    }
}

/// `path` as the harness `seen` says where it sees its project ([`Seen::said`]); as it is for a harness
/// that sees the machine's own paths.
pub fn said(seen: Option<&Seen>, path: &str) -> String {
    seen.map_or_else(|| path.to_owned(), |s| s.said(path))
}

/// The folder `spec`'s harness is told it works in: its project as its microVM sees it, isolated
/// ([`Seen`]); the machine's own, on its machine.
pub fn harness_cwd(spec: &Spec) -> String {
    said(Seen::of(spec).as_ref(), &crate::harness::session::resolve_cwd(spec.cwd.as_deref()))
}

/// The project folder `cwd` as a launch shares it: writable, where [`guest_path`] puts it in the
/// microVM, which is its member's working folder too.
pub fn project_share(cwd: &str) -> Result<Share, String> {
    Ok(Share { host: cwd.to_owned(), at: guest_path(cwd, PROJECTS)?, writable: true, members: Vec::new() })
}

/// `spec`'s workspace of its own where it is a Slates volume made with no mount, which Slates serves its microVM
/// ([`served_project`]), and the folder that volume lies over; none for any other.
fn served_over(spec: &Spec) -> Option<(&crate::workspace::slates::Made, &str)> {
    let w = spec.workspace.as_ref().filter(|w| w.kind == "volume")?;
    let made = w.made.as_ref().filter(|m| m.mount.is_empty() && !m.volume.is_empty())?;
    Some((made, w.over.as_deref()?))
}

/// An isolated `spec`'s project as Slates serves it to its microVM (kiln goal 01a1ec34599974448f6b7663ddb07001), where
/// its workspace of its own is a Slates volume made with no mount (`crate::workspace::prepare`): the volume, by its id,
/// writable and given to its `harness` as it starts, where its microVM sees the folder the volume lies over
/// ([`guest_path`]); and its harness's folder, `cwd` at the same place in it. None for a session working in its folder
/// itself, or in a mounted workspace, whose folder is shared ([`project_share`]). Why not, where `cwd` lies outside the
/// folder the volume lies over.
pub fn served_project(spec: &Spec, cwd: &str, harness: &kiln_seams::sandbox::Member) -> Result<Option<(kiln_seams::sandbox::Served, String)>, String> {
    let Some((made, over)) = served_over(spec) else { return Ok(None) };
    let at = guest_path(over, PROJECTS)?;
    let within = Path::new(cwd).strip_prefix(over).map_err(|_| format!("{cwd} lies outside {over}, the folder its workspace's volume lies over"))?;
    let mut folder = at.clone();
    for part in within.components() {
        match part {
            std::path::Component::Normal(name) => {
                folder.push('/');
                folder.push_str(&name.to_string_lossy());
            }
            std::path::Component::CurDir => {}
            other => return Err(format!("{cwd}: {other:?} in its folder beneath {over}")),
        }
    }
    let served = kiln_seams::sandbox::Served { source: made.volume.clone(), at, writable: true, members: vec![harness.clone()] };
    Ok(Some((served, folder)))
}

/// Why folder `cwd` can't be an isolated agent's project, shared writable with its microVM, where it can't
/// (CP-T4.1's AC-T4.1.5, as T4's `cp/t4-isolation` held it, 6134fd63b): the home of the user kiln runs as, or a
/// folder holding it, every file of theirs among it (an agent started with no folder starts there); a folder that is,
/// holds or lies in `~/.ssh` or one of `kiln`'s own (its state: its runner's keys and records, its microVMs' store,
/// its people's volumes, its server's data folder with kiln.db, its secrets and tokens); or one holding another worktree
/// of its repository, whose files are another agent's.
/// Compared with their links resolved, where they resolve.
pub fn kept_out(cwd: &Path, kiln: &[&Path]) -> Option<String> {
    let cwd = canonical(cwd);
    let home = canonical(&crate::os::home_dir());
    if home.starts_with(&cwd) {
        return Some("it is your home folder or holds it, and nothing else of your home goes in a microVM: give it a project's folder".to_owned());
    }
    let ssh = home.join(".ssh");
    for kept in [ssh.as_path()].into_iter().chain(kiln.iter().copied()).map(canonical) {
        if cwd.starts_with(&kept) {
            return Some(format!("it is or lies in {}, which no isolated agent sees", kept.display()));
        }
        if kept.starts_with(&cwd) {
            return Some(format!("it holds {}, which no isolated agent sees: give it a project's folder", kept.display()));
        }
    }
    other_worktree(&cwd).map(|other| format!("it holds {}, another worktree of its repository, whose files are another agent's: start it in a worktree of its own", other.display()))
}

/// The folder of the machine whose files an isolated session working in `cwd` is given: `cwd`; or, where `cwd` lies in
/// its workspace of its own (a Slates volume kiln made for it and mounted beneath its state, `crate::workspace::prepare`),
/// the folder that workspace lies over, at the same place in it, whose files the volume holds.
fn given(spec: &Spec, cwd: &Path) -> std::path::PathBuf {
    // A workspace with no mount (one Slates serves its microVM, [`served_project`]) leaves its session in the folder itself.
    let own = spec.workspace.as_ref().and_then(|w| Some((w.made.as_ref().filter(|m| !m.mount.is_empty())?.mount.as_str(), w.over.as_deref()?)));
    let Some((mount, over)) = own else { return cwd.to_path_buf() };
    let real = canonical(cwd);
    match real.strip_prefix(canonical(Path::new(mount))) {
        Ok(within) => Path::new(over).join(within),
        Err(_) => cwd.to_path_buf(),
    }
}

/// A worktree of `cwd`'s repository other than `cwd`, lying within it, where `cwd` holds its repository's git
/// folder: each linked worktree's `.git` is named in `.git/worktrees/<id>/gitdir` (gitrepository-layout(5)),
/// relative to that folder where git wrote it relative. None where `cwd` holds none, or it can't be read.
fn other_worktree(cwd: &Path) -> Option<std::path::PathBuf> {
    let linked = std::fs::read_dir(cwd.join(".git").join("worktrees")).ok()?;
    linked.flatten().find_map(|entry| {
        let said = std::fs::read_to_string(entry.path().join("gitdir")).ok()?;
        let tree = canonical(entry.path().join(said.trim()).parent()?);
        (tree != cwd && tree.starts_with(cwd)).then_some(tree)
    })
}

/// `path` with its links resolved, where it can be: one not made yet (a person's volumes, made as their harness
/// launches) as the folders it lies in resolve, then its own names.
fn canonical(path: &Path) -> std::path::PathBuf {
    let mut names = Vec::new();
    let mut at = path;
    loop {
        if let Ok(real) = std::fs::canonicalize(at) {
            return names.into_iter().rev().fold(real, |p, n| p.join(n));
        }
        match (at.parent(), at.file_name()) {
            (Some(parent), Some(name)) => {
                names.push(name);
                at = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
}

/// The folders of plugins `kind`'s harness loads for `session`, as the machine's launch gives them
/// (`Kind::launch`, `--plugin-dir`): Claude Code's, kiln's own skills (`crate::mcp`) and those kiln
/// installed for the session (`skills::place::session_folder`, made if it isn't there); none for another
/// harness. An isolated launch plugs each in read-only at its own path (docs/agents.md, "Skills kiln
/// installs, and kiln's own") and names it to its harness as on the machine.
pub fn plugin_folders(kind: &Kind, session: &str) -> Vec<String> {
    if !matches!(kind.protocol, Protocol::ClaudeStreamJson) {
        return Vec::new();
    }
    let own = crate::mcp::kit().map(|k| k.plugin_dir.clone()).or_else(|| crate::mcp::plugin_dir().map(Path::to_path_buf));
    own.into_iter().chain(crate::skills::place::session_folder(session)).map(|d| d.to_string_lossy().into_owned()).collect()
}

/// `person`'s home for `kind`'s harness under `root` (the machine's `<state>/homes`), as image `pinned`
/// of `source` lays it out (`harness::isolated::home::Layout`): its volumes made on this machine, each
/// its directory here and its path in the microVM (`Layout::prepare`), and that layout, which says where
/// its harness keeps its conversations there (`Layout::kept_in`) and where its files are here
/// (`Layout::host_of`: agy's MCP config, [`offer_mcp`]). kiln's own Agentfile says its harness's volumes and run
/// environment (`home::of_builtin`); a person's own, its volumes ([`kept_volumes`]), and its image, its
/// harness's environment ([`harness_env`]). Refused, typed, where kiln can't keep it
/// (`harness::isolated::Refusal`), or in words where its image can't be read.
async fn home_of(root: &Path, person: &str, kind: &Kind, source: Source<'_>, images: &Images, pinned: &str) -> anyhow::Result<(Vec<home::Mount>, home::Layout)> {
    let (volumes, env) = match source {
        Source::Builtin(b) => home::of_builtin(b),
        Source::Kept { text, .. } => {
            let (volumes, dir) = kept_volumes(text, kind.id).map_err(anyhow::Error::msg)?;
            let (store, at) = (images.store(), pinned.to_owned());
            let env = crate::rt::blocking(move || harness_env(&store, &at, &dir)).await?.map_err(|e| anyhow::anyhow!("{pinned}'s harness: {e}"))?;
            (volumes, env)
        }
    };
    let layout = home::Layout::new(root, person, kind.id, &volumes, &env)?;
    let prepared = layout.clone();
    let mounts = crate::rt::blocking(move || prepared.prepare()).await??;
    Ok((mounts, layout))
}

/// The sign-in `provider`'s CLI is granted as it launches for `person` (plan step 33; Shards' D242, step 8; Hex's C8):
/// its CLI's own, taken outside its microVM, its access token filled in toward `egress`'s hosts of its provider's
/// alone ([`signin::Provider::grant`]), with the placeholders an earlier capture gave its CLI, as its file in
/// `layout`'s volumes on this machine holds them. None, said, where `vms` hold no credentials (they aren't Hex's, or no
/// broker leases theirs: `hexvm::NO_BROKER`), no broker answers this runner's runs (`secrets::lessor::answering`: a
/// server from before), it acts for no person, or `egress` names no destination or doesn't reach its hosts: its
/// harness then signs in itself, its tokens in its microVM, as before plan step 33. Where one could be granted, a
/// launch before this runner's server has said whether it answers (a session restored as the runner started) waits
/// for its word, as Hex grants a sign-in only as a box is made (its C8): granted then, or not, as any launch's is.
async fn sign_in_of(provider: signin::Provider, person: Option<&str>, layout: Option<&home::Layout>, egress: &Egress, vms: &Microvms) -> anyhow::Result<Vec<signin::SignIn>> {
    let leases = vms.hex.as_ref().is_some_and(|h| kiln_seams::Sandbox::holds(&**h).credentials);
    let leased = leases && person.is_some() && matches!(egress, Egress::Only(_)) && crate::secrets::lessor::answering().await;
    let (Some(person), Egress::Only(hosts), true) = (person, egress, leased) else {
        tracing::warn!(?provider, leased, person = person.is_some(), "its harness signs in itself, its tokens in its microVM: kiln takes a sign-in outside only for a person's run in microVMs that lease credentials");
        return Ok(Vec::new());
    };
    let file = layout.and_then(|l| provider.file_in(l).and_then(|guest| l.host_of(&guest)));
    let (person, hosts) = (person.to_owned(), hosts.clone());
    let granted = crate::rt::blocking(move || {
        let text = file.as_deref().and_then(cli_file);
        provider.grant(&person, &hosts, text.as_ref().and_then(|t| std::str::from_utf8(t.expose()).ok()))
    })
    .await?;
    if granted.is_none() {
        tracing::warn!(?provider, "its harness signs in itself, its tokens in its microVM: its egress doesn't reach its sign-in's hosts, or its person has no sign-in name");
    }
    Ok(granted.into_iter().collect())
}

/// What a CLI's sign-in file at `path` on this machine holds, wiped as it's let go of (it may hold tokens from before
/// kiln took its sign-ins outside): none where there's none, or where it can't be read, said. Blocking.
fn cli_file(path: &Path) -> Option<hex_api::lease::Value> {
    match std::fs::read(path) {
        Ok(bytes) => Some(hex_api::lease::Value::new(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            tracing::warn!(file = %path.display(), error = %e, "a CLI's sign-in file couldn't be read: its sign-in is granted with no earlier capture");
            None
        }
    }
}

/// Agentfile `text`'s directives, as Shards reads them (`shards_agentfile`), every stage's; why not, in
/// Shards' words, where it doesn't parse.
fn directives(text: &str) -> Result<Vec<shards_agentfile::agentfile::Directive>, String> {
    use shards_agentfile::{instructions, lint::Linter, parser};
    let words = |m: &[u8]| String::from_utf8_lossy(m).into_owned();
    let parsed = parser::parse_as(text.as_bytes(), parser::Dialect::Agentfile).map_err(|e| words(&e.message))?;
    let read = instructions::parse(&parsed, &Linter::default()).map_err(|e| words(&e.message))?;
    let directives = read.stages.into_iter().flat_map(|s| s.commands).filter_map(|c| match c.kind {
        instructions::Kind::Agentfile(d) => Some(d),
        _ => None,
    });
    Ok(directives.collect())
}

/// A person's own Agentfile `text`'s volumes for its harness `member`, as Shards reads its lines: each
/// path of each `VOLUME` its agents and harnesses are given (one with an option, a `FOR` or a name:
/// a Dockerfile's `VOLUME` is its command's alone) that names `member` (`FOR`, as a harness or by its
/// own kind) or names none (every one of them), rooted at `/` as Shards roots a relative one; and the
/// folder its image keeps that harness in (`HARNESS ... TO`, else `/harness/<member>`), whose
/// `<folder>.d/osi.json` says how it runs. Why not, in words, where it doesn't parse or declares no
/// harness `member`.
pub fn kept_volumes(text: &str, member: &str) -> Result<(Vec<String>, String), String> {
    use shards_agentfile::agentfile::{Directive, TargetKind};
    let words = |m: &[u8]| String::from_utf8_lossy(m).into_owned();
    let rooted = |p: &[u8]| format!("/{}", words(p).split('/').filter(|c| !c.is_empty() && *c != ".").collect::<Vec<_>>().join("/"));
    let (mut volumes, mut dir) = (Vec::new(), None);
    for d in directives(text)? {
        match d {
            Directive::Volume(v) if v.scope.names.is_empty() || (v.scope.kind != Some(TargetKind::Agent) && v.scope.names.iter().any(|n| n == member.as_bytes())) => {
                volumes.extend(v.paths.iter().map(|p| rooted(p)));
            }
            Directive::Harness(h) if h.name == member.as_bytes() => dir = Some(h.to.as_deref().map_or_else(|| format!("/harness/{member}"), rooted)),
            _ => {}
        }
    }
    let dir = dir.ok_or_else(|| format!("it declares no harness {member} (`HARNESS {member} FROM ...`), which kiln starts it as"))?;
    Ok((volumes, dir))
}

/// The run environment of the harness kept at `dir` in image `pinned`, which the Shards store at `store`
/// holds, as Shards' init gives a harness its own (`harness_env`, crates/init/src/domains.rs): the image's
/// environment but its `PATH` and `HOME`, which are init's, and over it its OSI config's `run.env` (its
/// `harness.json` as its build normalized it, `<dir>.d/osi.json`). Shards' build labels its image with
/// its Agentfile's grants and digest alone (`vnd.osi.agentfile.*`), none of it, so it's read from the
/// image: its root file system as its build kept it, which its microVM's init reads, else its layers,
/// from the topmost. Its launch follows its build, so the image is there. Why not, in words. Blocking.
pub fn harness_env(store: &Path, pinned: &str, dir: &str) -> Result<Vec<(String, String)>, String> {
    use shards_image::{erofs, oci, osi, reference::Digest, store::Layer, store::Store};
    let said = |e: shards_image::Error| e.to_string();
    let store = Store::open(store).map_err(|e| format!("the Shards home's store, {}: {e}", store.display()))?;
    let digest = Digest::parse(pinned.rsplit_once('@').map_or(pinned, |(_, d)| d)).map_err(|e| format!("{pinned}: {e}"))?;
    let reference = store.holding(&digest).map_err(said)?.ok_or_else(|| format!("the Shards home's store holds no {pinned}"))?;
    let manifest = store.tagged(&reference).map_err(said)?.ok_or_else(|| format!("{reference} names no manifest"))?;
    let bytes = store.content(&manifest, oci::MAX_MANIFEST).map_err(said)?.ok_or_else(|| format!("{reference}'s manifest isn't in its store"))?;
    let oci::Document::Manifest(m) = oci::parse_document(&bytes, &manifest.media_type).map_err(said)? else {
        return Err(format!("{reference} names an index where its platform's manifest was"));
    };
    let config = store.content(m.image_config(), oci::MAX_CONFIG).map_err(said)?.ok_or_else(|| format!("{reference}'s config isn't in its store"))?;
    let config = oci::parse_config(&config).map_err(said)?;
    let diff_ids = config.rootfs.diff_ids.iter().map(|d| Digest::parse(d)).collect::<Result<Vec<_>, _>>().map_err(said)?;
    let path = format!("{dir}.d/osi.json");
    let osi = match store.rootfs_kept(&diff_ids) {
        Some(rootfs) => {
            let mut image = std::fs::File::open(&rootfs).map_err(|e| format!("{}: {e}", rootfs.display()))?;
            match erofs::read_file(&mut image, path.as_bytes(), oci::MAX_CONFIG).map_err(said)? {
                erofs::Found::File(bytes) => Some(bytes),
                erofs::Found::Missing | erofs::Found::Other => None,
            }
        }
        None => {
            if m.layers.len() != diff_ids.len() {
                return Err(format!("{reference} has {} layers and {} of their digests", m.layers.len(), diff_ids.len()));
            }
            let layer = |(l, d): (&oci::Descriptor, &Digest)| -> Result<Layer, String> { Ok(Layer { blob: l.digest().map_err(said)?, media_type: l.media_type.clone(), diff_id: d.clone() }) };
            let layers = m.layers.iter().zip(&diff_ids).map(layer).collect::<Result<Vec<_>, String>>()?;
            in_layers(&store, &layers, &path, oci::MAX_CONFIG)?
        }
    };
    let osi = osi.ok_or_else(|| format!("its image holds no {path}: nothing of its says how its harness runs"))?;
    let osi = osi::Config::parse(&osi).map_err(|e| format!("{path}: {e}"))?;
    Ok(run_env(config.config.and_then(|c| c.env).unwrap_or_default(), &osi))
}

/// A harness's run environment from its image's environment (`image`, each `NAME=value`) and its OSI
/// config (`osi`, its `<dir>.d/osi.json`), as Shards' init composes it (`harness_env`,
/// crates/init/src/domains.rs): the image's but its `PATH` and `HOME`, which are init's, then the
/// config's `run.env` over it.
pub fn run_env(image: Vec<String>, osi: &shards_image::osi::Config) -> Vec<(String, String)> {
    let run = osi.run.as_ref().map(|r| r.env.clone()).unwrap_or_default();
    let image = image.into_iter().filter(|e| !matches!(e.split_once('=').map(|(k, _)| k), Some("PATH" | "HOME")));
    image.chain(run).filter_map(|e| e.split_once('=').map(|(k, v)| (k.to_owned(), v.to_owned()))).collect()
}

/// What one layer says of a file.
enum Said {
    Nothing,
    File(Vec<u8>),
    Gone,
}

/// The regular file at `path` (absolute, plain) as `layers` stack it in `store`, read from the topmost
/// that says anything of it (image-spec's layer.md, "Whiteouts", whose whiteouts hide what's below
/// their layer alone): its bytes; none where that layer puts no regular file there, removes it (a
/// whiteout of it or of a folder it's in) or holds none of it in an opaque folder it's in, or where no
/// layer has it. One longer than `most` bytes is refused.
fn in_layers(store: &shards_image::store::Store, layers: &[shards_image::store::Layer], path: &str, most: u64) -> Result<Option<Vec<u8>>, String> {
    use shards_image::tar::{Reader, Type};
    let target: Vec<&[u8]> = path.split('/').filter(|c| !c.is_empty()).map(str::as_bytes).collect();
    for layer in layers.iter().rev() {
        // Its own entry, or one that leaves no folder above it; and whether it hides what's below it.
        let read = store.with_layer_tar(layer, &shards_image::store::Limits::none(), |tar| {
            let (mut tar, mut own, mut hides) = (Reader::new(tar), None, false);
            while let Some(entry) = tar.next_entry()? {
                let parts: Vec<&[u8]> = entry.path.split(|b| *b == b'/').filter(|c| !c.is_empty()).collect();
                let Some((name, folder)) = parts.split_last() else { continue };
                if parts == target {
                    own = Some(match entry.kind {
                        Type::File if entry.size > most => return Err(shards_image::Error(format!("{path} is {} bytes, past {most}", entry.size))),
                        Type::File => {
                            let mut bytes = Vec::new();
                            tar.copy_data(&mut bytes)?;
                            Said::File(bytes)
                        }
                        _ => Said::Gone,
                    });
                } else if *name == b".wh..wh..opq" && target.starts_with(folder) {
                    hides = true;
                } else if let Some(gone) = name.strip_prefix(b".wh.") {
                    let mut removed = folder.to_vec();
                    removed.push(gone);
                    hides |= target.starts_with(&removed);
                } else if target.starts_with(&parts) && entry.kind != Type::Dir {
                    own = Some(Said::Gone);
                }
            }
            Ok(own.unwrap_or(if hides { Said::Gone } else { Said::Nothing }))
        });
        match read.map_err(|e| format!("layer {}: {e}", layer.blob))? {
            Said::File(bytes) => return Ok(Some(bytes)),
            Said::Gone => return Ok(None),
            Said::Nothing => {}
        }
    }
    Ok(None)
}

/// kiln's MCP for `kind`'s harness in its microVM, through its link's `mcp` lane's bridge (step 18,
/// `crate::harness::isolated::mcp`): its command line's form added to `args`; Antigravity's entry put
/// in its own config in its person's volume as `home` lays it out ([`home_of`]; the only place agy reads one
/// from, kept as agy keeps it: `mcp::agy_config`), from kiln's own Agentfile or a person's alike: the one fixed
/// entry, as its lane, not the entry, names its session (Shards D254); none with no home of its person's here. An
/// ACP agent's is given at its `session/new` (`kinds::acp_servers`). A config that couldn't be written, or that
/// lies in none of its volumes, starts it without kiln's MCP, said, as on its machine (`Kind::prepare`).
async fn offer_mcp(kind: &Kind, home: Option<&home::Layout>, args: &mut Vec<String>) {
    use crate::harness::isolated::mcp;
    match mcp::for_harness(kind) {
        mcp::Config::Args(more) => args.extend(more),
        mcp::Config::Shared(_) => {
            let Some(layout) = home.cloned() else { return };
            let put = crate::rt::blocking(move || -> anyhow::Result<()> {
                let guest = format!("{}/.gemini/config/mcp_config.json", layout.guest_home().trim_end_matches('/'));
                // Outside its volumes is its microVM's scratch, which kiln never makes and its harness never sees here.
                let at = layout.host_of(&guest).filter(|at| layout.mounts().iter().any(|m| at.starts_with(&m.host)));
                let at = at.ok_or_else(|| anyhow::anyhow!("{guest} is in none of its volumes, where kiln could put it"))?;
                let config = match std::fs::read(&at) {
                    Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Bool(false)),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::Value::Null,
                    Err(e) => return Err(anyhow::anyhow!("{}: {e}", at.display())),
                };
                if let Some(next) = mcp::agy_config(&config) {
                    if let Some(dir) = at.parent() {
                        std::fs::create_dir_all(dir)?;
                    }
                    std::fs::write(&at, serde_json::to_vec_pretty(&next)?)?;
                }
                Ok(())
            });
            if let Err(e) = put.await.and_then(|r| r) {
                tracing::warn!(harness = kind.id, error = %format!("{e:#}"), "kiln's MCP couldn't be put in its config: it starts without it");
            }
        }
        mcp::Config::Acp(_) | mcp::Config::Refused(_) => {}
    }
}

/// What a run of kiln's Agentfile `b` reaches of its image's destinations (`super::narrow`): its
/// session's own provider's or sign-in's, read as its harness reads them, from its run's environment
/// as the microVM has it (its image's `run.env`, then `launch_env`), the model its session names, and
/// its configuration in its person's home under `homes` (`harness::isolated::home::Layout`) or its
/// project's own (`project`, as the launch shares it: read at its folder here, where the microVM has
/// it at its share's path). Whole, saying why, where
/// its image serves one use or kiln keeps no home for it here. Reads files: off the runtime's threads.
pub fn reach_of(b: &'static builtin::Builtin, person: Option<&str>, model: Option<&str>, homes: Option<&std::path::Path>, project: Option<&Share>, launch_env: &[(String, String)]) -> Reach {
    if !builtin::destinations::BY_USE.iter().any(|t| t.harness == b.harness) {
        return Reach::Whole { why: Whole::OneUse };
    }
    let (Some(root), Some(person)) = (homes, person) else {
        return Reach::Whole { why: Whole::NoHome };
    };
    let (volumes, image_env) = crate::harness::isolated::home::of_builtin(b);
    let layout = match crate::harness::isolated::home::Layout::new(root, person, b.harness, &volumes, &image_env) {
        Ok(layout) => layout,
        Err(e) => return Reach::Whole { why: Whole::Unread { file: format!("{person}'s home for {}", b.harness), error: e.to_string() } },
    };
    let env: Vec<(String, String)> = image_env.into_iter().chain(launch_env.iter().cloned()).collect();
    let read = |guest: &str| -> Result<Option<String>, String> {
        // In its project: that folder here; else in its home's volumes, or nowhere here.
        let in_project = project.and_then(|p| std::path::Path::new(guest).strip_prefix(&p.at).ok().map(|rest| std::path::Path::new(&p.host).join(rest)));
        let at = match in_project.or_else(|| layout.host_of(guest)) {
            Some(at) => at,
            None => return Ok(None),
        };
        match std::fs::read_to_string(&at) {
            Ok(text) => Ok(Some(text)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("{}: {e}", at.display())),
        }
    };
    super::narrow::reach(b.harness, &super::narrow::Reads { home: &layout.guest_home(), project: project.map(|p| p.at.as_str()), env: &env, model, read: &read })
}

/// The destinations Agentfile `text` grants past its microVM, each `host:port`, as Shards reads them
/// (`shards_agentfile::agentfile::grants`, D236): none where it grants a port to any address or any
/// name, which no list of destinations is; why not, where it doesn't parse.
pub fn granted(text: &str) -> Result<Option<Vec<String>>, String> {
    use shards_agentfile::agentfile;
    let words = |m: &[u8]| String::from_utf8_lossy(m).into_owned();
    let g = agentfile::grants(&agentfile::resolve_targets(&directives(text)?).map_err(|e| words(&e))?);
    Ok((g.egress.is_empty() && !g.dns).then(|| g.destinations.iter().map(|d| words(d)).collect()))
}

/// The seam's egress for what a run reaches: its image's destinations narrowed to `hosts`; or, kept
/// whole, each destination its image grants (`whole`, [`granted`]), as a runtime that grants a run each
/// one by name (Hex: default deny) takes them, else its image's own grants, narrowed no further.
pub fn egress_of(reach: &Reach, whole: Option<Vec<String>>) -> Egress {
    match reach {
        Reach::Narrowed { hosts, .. } => Egress::Only(hosts.clone()),
        Reach::Whole { .. } => whole.map_or(Egress::Any, Egress::Only),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// P6.2: each of an agent's volumes is given to its harness alone as it starts, read-write, by its name in its
    /// machine's store (made there where it isn't, found again by the agent's next session), with the bound its
    /// definition gives it, beneath `/volumes`. One its server named no bound for is refused, not given unbounded.
    #[test]
    fn p6_2_an_agents_volumes_are_given_to_its_harness_by_their_store_names_as_it_starts() -> anyhow::Result<()> {
        use kiln_seams::sandbox::{Member, Volume};
        let given = crate::agentdefs::volumes::given("user:a", "d1", &[crate::agentdefs::volumes::Volume { name: "cache".into(), bytes: 1 << 30 }]);
        let store = crate::agentdefs::volumes::store_name("user:a", "d1", "cache");
        let bound = std::num::NonZeroU64::new(1 << 30).ok_or_else(|| anyhow::anyhow!("a bound of 0"))?;
        let volumes = volumes_of(&given, &Member("claude".into()))?;
        assert_eq!(volumes, [Volume { name: store, bound, at: format!("{VOLUMES}/cache"), members: vec![Member("claude".into())], read_only: false }]);
        assert!(volumes_of(&[], &Member("claude".into()))?.is_empty(), "an agent with none is given none");
        let unbounded = crate::agentdefs::volumes::Given { bytes: 0, ..given.into_iter().next().ok_or_else(|| anyhow::anyhow!("none given"))? };
        assert!(volumes_of(&[unbounded], &Member("claude".into())).is_err(), "a volume with no bound is refused");
        Ok(())
    }

    /// P6.2 (Hex's P6.2 retries): a start its machine's store refused for a volume's clearing or bound says when to
    /// start it again; any other refusal says nothing more.
    #[test]
    fn p6_2_a_start_refused_for_a_volume_says_when_to_start_it_again() {
        let refused = |cause| Refusal::Volume { cause, why: "kiln-agent-x".into() };
        assert!(again(&refused(VolumeCause::Clearing)).contains("clearing has answered"));
        assert!(again(&refused(VolumeCause::Bound)).contains("no other session"));
        assert_eq!(again(&refused(VolumeCause::InUse)), "");
        assert_eq!(again(&Refusal::Ended), "");
    }

    /// An MCP `initialize`, numbered `id`, as a harness's client sends one first.
    fn initialize(id: u64) -> serde_json::Value {
        serde_json::json!({"jsonrpc": "2.0", "id": id, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "harness", "version": "1"}}})
    }

    /// The answer numbered `id` that `said` holds, waited for up to 30 seconds.
    fn answer(said: &super::super::fake::Said, id: u64) -> anyhow::Result<serde_json::Value> {
        for _ in 0..3000 {
            if let Some(a) = said.lines().into_iter().find(|l| l.get("id") == Some(&serde_json::json!(id))) {
                return Ok(a);
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        anyhow::bail!("no answer {id}: {:?}", said.lines())
    }

    /// #75 step 3 (Shards D254): this runner serves kiln's MCP over each connection its run's harness makes to its
    /// microVM's bridge, in its own process, as the lane hands them out for its run, until the lane says the run is
    /// over: each connection's `initialize` is answered as `kiln reach mcp` answers it, naming its session, and its
    /// tools are kiln's. No token or address is said on any: the lane is whose it is.
    #[test]
    fn s35_mcp_the_runner_serves_kilns_mcp_over_each_connection_of_its_lane() -> anyhow::Result<()> {
        use super::super::fake::{FakeLanes, connection};
        let dir = tempfile::tempdir()?;
        let config = crate::reach::node::Config {
            mirrors: dir.path().join("mirrors"),
            adopt_policy: crate::harness::moving::stream::StoragePolicy::Off,
            sense: false,
            ..crate::reach::node::Config::for_runner("here", dir.path(), true)
        };
        let (handle, _to_server) = crate::reach::node::start(config)?;
        let here = crate::reach::local::Here { handle, me: "here".into(), host: None, session: "iso-s1".into() };
        let listed = serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"});
        let (one, said_one) = connection(&[initialize(1), serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"}), listed]);
        let (two, said_two) = connection(&[initialize(7)]);
        let lanes = std::sync::Arc::new(FakeLanes::of(vec![one, two]));
        let served = bridge(lanes.clone(), "vm-1".into(), Some(here), dir.path().to_string_lossy().into_owned())?;
        served.join().map_err(|_| anyhow::anyhow!("its bridge panicked"))?;
        assert_eq!(lanes.asked(), ["vm-1", "vm-1", "vm-1"], "each connection of its run's, until its lane said it was over");
        for (said, id) in [(&said_one, 1), (&said_two, 7)] {
            let init = answer(said, id)?;
            anyhow::ensure!(init.pointer("/result/serverInfo").is_some(), "{init}");
            anyhow::ensure!(init.pointer("/result/instructions").and_then(serde_json::Value::as_str).is_some_and(|i| i.contains("iso-s1")), "its session named: {init}");
        }
        let list = answer(&said_one, 2)?;
        let names: Vec<&str> = list.pointer("/result/tools").and_then(serde_json::Value::as_array).into_iter().flatten().filter_map(|t| t.get("name").and_then(serde_json::Value::as_str)).collect();
        anyhow::ensure!(names.contains(&"code_run") && names.contains(&"run"), "kiln's tools: {list}");
        for said in [&said_one, &said_two] {
            let all = String::from_utf8_lossy(&said.0.lock()).into_owned();
            anyhow::ensure!(!all.contains("Bearer") && !all.contains("KILN_SESSION") && !all.contains("/api/mcp"), "a token or an address said: {all}");
        }
        Ok(())
    }

    /// #75 step 3: where this process runs no reach node, a connection is let go of at once, so its harness finds
    /// kiln's MCP gone rather than waiting on it, and the bridge ends with its lane.
    #[test]
    fn s35_mcp_with_no_reach_node_a_connection_is_let_go_of() -> anyhow::Result<()> {
        use super::super::fake::{FakeLanes, connection};
        let (one, said) = connection(&[initialize(1)]);
        let lanes = std::sync::Arc::new(FakeLanes::of(vec![one]));
        bridge(lanes.clone(), "vm-1".into(), None, "/".into())?.join().map_err(|_| anyhow::anyhow!("its bridge panicked"))?;
        assert_eq!(lanes.asked(), ["vm-1", "vm-1"]);
        assert_eq!(std::sync::Arc::strong_count(&said.0), 1, "its connection let go of");
        assert!(said.lines().is_empty(), "nothing answered");
        Ok(())
    }

    /// Plan 01a1d2c16eb074f19cf7ae7f1025c0be step 4: two isolated Antigravity sessions of one person on one machine.
    /// Both launches put kiln's entry in agy's own `mcp_config.json`, in the person's volume they share: the lane's
    /// bridge alone, naming no session, token or address, and nothing else is written there. Each session's bridge
    /// takes only its own run's connections from the machine's one lane, and kiln answers each as that session.
    #[test]
    fn s35_mcp_two_antigravity_sessions_of_one_person_are_each_their_own() -> anyhow::Result<()> {
        use super::super::fake::{FakeLanes, connection};
        let dir = tempfile::tempdir()?;
        let homes = dir.path().join("homes");
        // The person's home as kiln's own Agentfile lays it out, as `home_of` gives it to both launches.
        let (volumes, env) = crate::harness::isolated::home::of_builtin(builtin_of("antigravity")?);
        let layout = crate::harness::isolated::home::Layout::new(&homes, "user:u1", "antigravity", &volumes, &env)?;
        let laid = layout.clone();
        crate::rt::block_on(async move {
            let agy = kind("antigravity")?;
            for _ in 0..2 {
                let mut args = Vec::new();
                offer_mcp(agy, Some(&laid), &mut args).await;
                anyhow::ensure!(args.is_empty(), "agy takes kiln's MCP from its own config alone: {args:?}");
            }
            anyhow::Ok(())
        })?;
        let written = files_in(&homes)?;
        let [kept_at] = written.as_slice() else { anyhow::bail!("written in the person's volume: {written:?}") };
        anyhow::ensure!(kept_at.ends_with(".gemini/config/mcp_config.json"), "{}", kept_at.display());
        anyhow::ensure!(layout.mounts().iter().any(|m| kept_at.starts_with(&m.host)), "not in the person's volume: {}", kept_at.display());
        let kept: serde_json::Value = serde_json::from_slice(&std::fs::read(kept_at)?)?;
        anyhow::ensure!(kept == serde_json::json!({"mcpServers": {"kiln": {"command": crate::harness::isolated::mcp::BRIDGE, "args": []}}}), "{kept}");

        let node = crate::reach::node::Config {
            mirrors: dir.path().join("mirrors"),
            adopt_policy: crate::harness::moving::stream::StoragePolicy::Off,
            sense: false,
            ..crate::reach::node::Config::for_runner("here", dir.path(), true)
        };
        let (handle, _to_server) = crate::reach::node::start(node)?;
        let here = |session: &str| crate::reach::local::Here { handle: handle.clone(), me: "here".into(), host: None, session: session.into() };
        let (a, said_a) = connection(&[initialize(1)]);
        let (b, said_b) = connection(&[initialize(2)]);
        // The machine's one lane holds b's connection first, for its own run alone.
        let lanes = std::sync::Arc::new(FakeLanes::per_run(vec![("vm-b", b), ("vm-a", a)]));
        let cwd = dir.path().to_string_lossy().into_owned();
        let served = [bridge(lanes.clone(), "vm-a".into(), Some(here("agy-a")), cwd.clone())?, bridge(lanes.clone(), "vm-b".into(), Some(here("agy-b")), cwd)?];
        for s in served {
            s.join().map_err(|_| anyhow::anyhow!("a bridge panicked"))?;
        }
        let mut asked = lanes.asked();
        asked.sort();
        assert_eq!(asked, ["vm-a", "vm-a", "vm-b", "vm-b"], "each bridge asked for its own run's connections, until it had none");
        for (said, id, own, other) in [(&said_a, 1, "agy-a", "agy-b"), (&said_b, 2, "agy-b", "agy-a")] {
            let init = answer(said, id)?;
            let told = init.pointer("/result/instructions").and_then(serde_json::Value::as_str).unwrap_or_default();
            anyhow::ensure!(told.contains(own) && !told.contains(other), "{own}'s connection answered as {told:?}");
        }
        Ok(())
    }

    /// Every file beneath `dir`, at any depth.
    fn files_in(dir: &std::path::Path) -> std::io::Result<Vec<std::path::PathBuf>> {
        let (mut files, mut dirs) = (Vec::new(), vec![dir.to_path_buf()]);
        while let Some(d) = dirs.pop() {
            for entry in std::fs::read_dir(&d)? {
                let path = entry?.path();
                if path.is_dir() {
                    dirs.push(path);
                } else {
                    files.push(path);
                }
            }
        }
        Ok(files)
    }

    /// Plan step 35 (s35-tests' P1): Antigravity started from a person's own Agentfile gets kiln's MCP as from kiln's own:
    /// the one fixed entry, the lane's bridge alone (Shards D254), in agy's own `mcp_config.json` beneath the home its
    /// image's harness names, in the volume its Agentfile gives that harness, as its launch lays them out ([`home_of`]).
    /// An image whose harness's home is in none of its volumes (its microVM's scratch, `/tmp`) gets none: nothing is
    /// written for it, and no scratch is made here.
    #[test]
    fn s35_mcp_antigravity_from_a_persons_own_agentfile_gets_the_one_fixed_entry() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let homes = dir.path().join("homes");
        let images = Images::new(dir.path().join("no-shards"), dir.path().join("shards-home"), dir.path().join("scratch"));
        let osi = |env: &str| serde_json::json!({"schemaVersion": 1, "name": "antigravity", "run": {"command": ["agy"], "env": [env]}}).to_string();
        let (home_osi, scratch_osi) = (osi("HOME=/home/me"), osi("XDG_DATA_HOME=/data/agy"));
        let (pinned_home, _) = image_in(&images.store(), "agentfile-home", &["PATH=/usr/bin"], &[&[("harness/antigravity.d/osi.json", &home_osi)]])?;
        let (pinned_scratch, _) = image_in(&images.store(), "agentfile-scratch", &["PATH=/usr/bin"], &[&[("harness/antigravity.d/osi.json", &scratch_osi)]])?;
        let root = homes.clone();
        let laid = crate::rt::block_on(async move {
            let agy = kind("antigravity")?;
            let image = format!("{KEPT}0123456789abcdef0123456789abcdef");
            let mut laid = Vec::new();
            for (volume, pinned) in [("/home/me", pinned_home), ("/data/agy", pinned_scratch)] {
                let text = format!("FROM scratch\nHARNESS antigravity FROM ./agy\nVOLUME --target-kind=harness {volume} FOR antigravity\n");
                let spec = Spec { kind: agy.id.into(), isolation: Some(microvm(Some(image.as_str()))), agentfile: Some(text), ..Spec::default() };
                let source = source_of(agy, &spec).map_err(anyhow::Error::msg)?;
                anyhow::ensure!(matches!(source, Source::Kept { .. }), "{} isn't the person's own", source.name());
                let (_, layout) = home_of(&root, "user:u1", agy, source, &images, &pinned).await?;
                let mut args = Vec::new();
                offer_mcp(agy, Some(&layout), &mut args).await;
                anyhow::ensure!(args.is_empty(), "agy takes kiln's MCP from its own config alone: {args:?}");
                laid.push(layout);
            }
            anyhow::Ok(laid)
        })?;
        let [in_volume, in_scratch] = laid.as_slice() else { anyhow::bail!("two launches: {laid:?}") };
        anyhow::ensure!(in_volume.guest_home() == "/home/me", "its home as its image names it: {}", in_volume.guest_home());
        let kept_at = in_volume.host_of("/home/me/.gemini/config/mcp_config.json").ok_or_else(|| anyhow::anyhow!("no path here for its config"))?;
        anyhow::ensure!(in_volume.mounts().iter().any(|m| kept_at.starts_with(&m.host)), "not in its volume: {}", kept_at.display());
        let kept: serde_json::Value = serde_json::from_slice(&std::fs::read(&kept_at)?)?;
        anyhow::ensure!(kept == serde_json::json!({"mcpServers": {"kiln": {"command": crate::harness::isolated::mcp::BRIDGE, "args": []}}}), "{kept}");
        let written = files_in(&homes)?;
        anyhow::ensure!(written == [kept_at], "written for either launch: {written:?}");
        anyhow::ensure!(!in_scratch.host_home().exists(), "a scratch made here: {}", in_scratch.host_home().display());
        Ok(())
    }

    fn microvm(image: Option<&str>) -> Isolation {
        Isolation { image: image.map(str::to_owned), ..Isolation::microvm() }
    }

    fn kind(id: &str) -> anyhow::Result<&'static Kind> {
        Kind::get(id).ok_or_else(|| anyhow::anyhow!("no harness {id}"))
    }

    /// Step 17: the Agentfile a start names is what the microVM is made from: kiln's own for its
    /// harness when none is named, a built-in by its name for its own harness alone, and a person's
    /// own by its id, its text the one the server read as it started, built beside kiln's for its
    /// harness; anything else is refused, saying what it takes.
    #[test]
    fn s17_an_isolated_sessions_agentfile_is_the_one_its_start_named() -> anyhow::Result<()> {
        let (claude, codex) = (kind("claude")?, kind("codex")?);
        let spec = |image: Option<&str>, agentfile: Option<&str>| Spec { isolation: Some(microvm(image)), agentfile: agentfile.map(str::to_owned), ..Spec::default() };
        let name = |kind: &Kind, spec: &Spec| source_of(kind, spec).map(|s| s.name());
        assert_eq!(name(claude, &spec(None, None)), Ok("harness-claude".to_owned()));
        assert_eq!(name(codex, &spec(Some(" harness-codex "), None)), Ok("harness-codex".to_owned()));
        let id = crate::util::new_id();
        let kept = spec(Some(&format!("agentfile:{id}")), Some("FROM scratch\n"));
        match source_of(claude, &kept) {
            Ok(Source::Kept { id: of, text, beside }) => {
                assert_eq!((of, text), (id.as_str(), "FROM scratch\n"));
                assert_eq!(beside.map(|b| b.name), Some("harness-claude"), "built beside kiln's for its harness");
            }
            other => anyhow::bail!("not a person's own: {:?}", other.map(|s| s.name())),
        }
        let textless = name(claude, &spec(Some(&format!("agentfile:{id}")), None)).err().unwrap_or_default();
        assert!(textless.contains("didn't come with its start"), "{textless}");
        let not_its_own = name(claude, &spec(Some("harness-codex"), None)).err().unwrap_or_default();
        assert!(not_its_own.contains("for codex, not Claude Code"), "{not_its_own}");
        assert!(name(claude, &spec(Some("agentfile:../etc"), Some("FROM scratch\n"))).is_err(), "an id is 32 lowercase hex digits");
        assert!(name(claude, &spec(Some("ubuntu:24.04"), None)).is_err(), "neither kiln's nor a person's Agentfile");
        let container = Spec { isolation: Some(Isolation { kind: "container".into(), ..Isolation::microvm() }), ..Spec::default() };
        assert!(name(claude, &container).is_err(), "only microVMs hold agents");
        assert!(name(claude, &Spec::default()).is_err(), "a session that isn't isolated has none");
        Ok(())
    }

    /// Step 17 part 7: an isolated harness on a Windows machine is told its project, and what's beneath it,
    /// at its place in its microVM, and anything else as it is; one on a Unix machine, and one on its
    /// machine, the machine's own paths.
    #[test]
    fn s17_an_isolated_harness_is_told_its_folder_as_its_microvm_sees_it() {
        let spec = |cwd: &str, isolated: bool| Spec { kind: "codex".into(), cwd: Some(cwd.into()), isolation: isolated.then(|| microvm(None)), ..Spec::default() };
        let seen = Seen::of(&spec(r"C:\src\app\", true));
        for (path, told) in [(r"C:\src\app", "/work/app"), (r"C:\src\app\", "/work/app"), (r"C:\src\app\crates\kiln", "/work/app/crates/kiln"), ("C:\\src\\app/docs", "/work/app/docs"), (r"C:\src\application", r"C:\src\application"), ("/work/app/sub", "/work/app/sub")] {
            assert_eq!(said(seen.as_ref(), path), told, "{path}");
        }
        assert_eq!(harness_cwd(&spec(r"\\?\D:\repos\kiln", true)), "/work/kiln");
        for (cwd, isolated) in [("/Users/ada/src/app", true), (r"C:\src\app", false), (r"C:\", true)] {
            assert_eq!(Seen::of(&spec(cwd, isolated)), None, "{cwd}");
            assert_eq!(harness_cwd(&spec(cwd, isolated)), cwd);
        }
    }

    /// Step 17: an isolated launch is its image's harness member, in the session's folder as its microVM
    /// sees it (a Unix folder at its own path, a Windows one at `/work/<its name>`), given only what its image's command doesn't: Codex none, Claude Code its session's
    /// flags; of the session's environment only its locale and time zone, and no kiln variable.
    #[test]
    fn s17_an_isolated_launch_is_the_members_with_what_the_session_adds() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let cwd = dir.path().to_string_lossy().into_owned();
        let env = vec![("PATH".to_owned(), "/host/bin".to_owned()), ("LANG".to_owned(), "fr_FR.UTF-8".to_owned()), ("TZ".to_owned(), "Europe/Paris".to_owned()), ("KILN_REACH_SOCK".to_owned(), "/host/sock".to_owned())];
        let spec = |kind: &str| Spec { kind: kind.into(), cwd: Some(cwd.clone()), env: env.clone(), isolation: Some(microvm(None)), ..Spec::default() };
        let codex = launch_of(kind("codex")?, &spec("codex"), "harness-codex@sha256:00".into(), &[])?;
        assert_eq!((codex.program.as_str(), codex.args.as_slice()), ("codex", &[][..]), "its image's command runs it as kiln's adapter drives it");
        assert_eq!(codex.boundary, Boundary::MicroVm { image: "harness-codex@sha256:00".into() });
        // Its project, shared from this machine's folder to where its microVM sees it, which is its member's
        // working folder too: a Unix folder at its own path, a Windows one at `/work/<its name>` (`guest_path`;
        // jupiter, 2026-11-13: `/work/.tmpAoo893` for `C:\Users\…\Temp\.tmpAoo893`). Its home comes with its
        // person's volume.
        let name = dir.path().file_name().map(|n| n.to_string_lossy().into_owned()).ok_or_else(|| anyhow::anyhow!("{cwd} has no name"))?;
        let at = if cfg!(windows) { format!("{PROJECTS}/{name}") } else { cwd.clone() };
        assert_eq!(codex.shares, [Share { host: cwd.clone(), at: at.clone(), writable: true, members: Vec::new() }], "its project, where its microVM sees it");
        assert_eq!(codex.folder.as_deref(), Some(at.as_str()), "its working folder, its project's place in its microVM");
        // A Windows machine's project: shared from where it is, at a Linux path of its own name, which is
        // its working folder too; Unix's at its own path.
        let windows = project_share(r"C:\Users\ada\proj").map_err(anyhow::Error::msg)?;
        assert_eq!(windows, Share { host: r"C:\Users\ada\proj".into(), at: "/work/proj".into(), writable: true, members: Vec::new() });
        for (host, at) in [(r"\\?\C:\Users\ada\proj\", "/work/proj"), ("D:/src/kiln", "/work/kiln"), (r"\\server\share\repo", "/work/repo"), ("/Users/ada/proj", "/Users/ada/proj")] {
            assert_eq!(guest_path(host, PROJECTS), Ok(at.to_owned()), "{host}");
        }
        for nameless in [r"C:\", "C:/", r"\\?\C:\", "relative\\proj"] {
            assert!(guest_path(nameless, PROJECTS).is_err(), "{nameless}");
        }
        assert_eq!(codex.env, vec![("LANG".to_owned(), "fr_FR.UTF-8".to_owned()), ("TZ".to_owned(), "Europe/Paris".to_owned())]);
        let resumed = Spec { resume: Some("conversation-1".into()), model: Some("claude-opus-5-5".into()), allow_bypass: false, ..spec("claude") };
        let claude = launch_of(kind("claude")?, &resumed, "harness-claude@sha256:00".into(), &[])?;
        assert_eq!(claude.program, "claude");
        assert_eq!(claude.args, ["--model", "claude-opus-5-5", "--resume", "conversation-1"], "its session's own flags, past its image's");
        assert!(claude.env.contains(&("CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING".to_owned(), "1".to_owned())));
        assert!(!claude.env.iter().any(|(k, _)| k == "PATH" || k.starts_with("KILN_")), "no host variable, no kiln socket: {:?}", claude.env);
        // A folder this machine hasn't is refused before anything starts.
        let gone = Spec { cwd: Some(dir.path().join("gone").to_string_lossy().into_owned()), ..spec("codex") };
        assert!(launch_of(kind("codex")?, &gone, "harness-codex@sha256:00".into(), &[]).is_err());
        Ok(())
    }

    /// CP-T4.1's AC-T4.1.5, as T4's `cp/t4-isolation` held it (6134fd63b), carried to the launch through Hex (step 17):
    /// no isolated agent works in its person's home or a folder holding it (where one started with no folder would), in
    /// `~/.ssh`, or in a folder holding another worktree of its repository; a project's own folder and a worktree of its
    /// own are its to work in. Each refused before anything starts.
    #[test]
    fn s17_t4_a_home_ssh_or_anothers_worktree_is_no_isolated_agents_project() -> anyhow::Result<()> {
        let home = crate::os::home_dir();
        let mut refused = vec![home.clone()];
        refused.extend(home.parent().map(Path::to_path_buf));
        refused.extend([home.join(".ssh")].into_iter().filter(|ssh| ssh.is_dir()));
        for folder in &refused {
            assert!(kept_out(folder, &[]).is_some(), "{} would be shared", folder.display());
        }
        let repo = tempfile::tempdir()?;
        assert_eq!(kept_out(repo.path(), &[]), None, "a project's own folder");
        let inside = repo.path().join(".claude").join("worktrees").join("w");
        let linked = repo.path().join(".git").join("worktrees").join("w");
        std::fs::create_dir_all(&inside)?;
        std::fs::create_dir_all(&linked)?;
        std::fs::write(linked.join("gitdir"), format!("{}\n", inside.join(".git").display()))?;
        let why = kept_out(repo.path(), &[]).unwrap_or_default();
        assert!(why.contains("another worktree"), "{why}");
        assert_eq!(kept_out(&inside, &[]), None, "a worktree of its own");
        let spec = Spec { kind: "codex".into(), cwd: Some(repo.path().to_string_lossy().into_owned()), isolation: Some(microvm(None)), ..Spec::default() };
        let refusal = launch_of(kind("codex")?, &spec, "harness-codex@sha256:00".into(), &[]).err().map(|e| e.to_string()).unwrap_or_default();
        assert!(refusal.contains("another worktree"), "refused before anything starts: {refusal}");
        let homeless = Spec { cwd: None, ..spec };
        assert!(launch_of(kind("codex")?, &homeless, "harness-codex@sha256:00".into(), &[]).is_err(), "one started with no folder works in its home");
        Ok(())
    }

    /// CP-T4.1's AC-T4.1.5, as T4's `cp/t4-isolation` held it (6134fd63b), carried to the launch through Hex (step 17):
    /// no isolated agent works in a folder that is, holds or lies in kiln's own state (where its microVMs are held, and
    /// its people's volumes, made or not yet), each refused before anything starts; a project beside it is its to work
    /// in. In a workspace of its own, mounted beneath kiln's state, the folder that workspace lies over is what's judged:
    /// a project's taken, the home's refused. Another agent's workspace lies in kiln's state, and is refused.
    #[test]
    fn s17_t4_kilns_state_is_no_isolated_agents_project_nor_seen_through_its_workspace() -> anyhow::Result<()> {
        let base = tempfile::tempdir()?;
        let (state, project) = (base.path().join("state"), base.path().join("project"));
        let volumes = state.join("homes").join("owner");
        let (mine, theirs) = (state.join("workspaces").join("kiln-mine"), state.join("workspaces").join("kiln-theirs"));
        for dir in [&project, &volumes, &mine, &theirs] {
            std::fs::create_dir_all(dir)?;
        }
        let kiln = [state.as_path()];
        assert_eq!(kept_out(&project, &kiln), None, "a project beside kiln's state");
        for (folder, said) in [(state.as_path(), "lies in"), (volumes.as_path(), "lies in"), (base.path(), "holds")] {
            let why = kept_out(folder, &kiln).unwrap_or_default();
            assert!(why.contains(said), "{}: {why}", folder.display());
        }
        // A person's volumes kept apart from the rest of kiln's state (as a stand-in keeps them), not made yet.
        let unmade = project.join("homes");
        assert!(kept_out(&project, &[unmade.as_path()]).is_some_and(|why| why.contains("holds")), "a project holding where its person's volumes go");
        let spec = |cwd: &Path| Spec { kind: "codex".into(), cwd: Some(cwd.to_string_lossy().into_owned()), isolation: Some(microvm(None)), ..Spec::default() };
        let launched = |spec: &Spec| -> anyhow::Result<Launch> { launch_of(kind("codex")?, spec, "harness-codex@sha256:00".into(), &kiln) };
        let refusal = |spec: &Spec| launched(spec).err().map(|e| e.to_string()).unwrap_or_default();
        assert!(refusal(&spec(base.path())).contains("holds"), "a folder holding kiln's state");
        assert!(launched(&spec(project.as_path())).is_ok(), "a project beside kiln's state");
        let in_workspace = |cwd: &Path, over: &Path| Spec {
            workspace: Some(crate::workspace::Workspace {
                kind: "volume".into(),
                over: Some(over.to_string_lossy().into_owned()),
                sub: String::new(),
                made: Some(crate::workspace::slates::Made { mount: mine.to_string_lossy().into_owned(), ..Default::default() }),
                ..Default::default()
            }),
            ..spec(cwd)
        };
        assert!(launched(&in_workspace(mine.as_path(), project.as_path())).is_ok(), "its own workspace over a project");
        assert!(refusal(&in_workspace(mine.as_path(), crate::os::home_dir().as_path())).contains("home folder"), "its own workspace over the home");
        assert!(refusal(&in_workspace(theirs.as_path(), project.as_path())).contains("lies in"), "another agent's workspace");
        Ok(())
    }

    /// Kiln goal 01a1ec34599974448f6b7663ddb07001: an isolated session whose workspace of its own is a Slates volume made
    /// with no mount is given that volume as a folder Slates serves its microVM, never a folder of the machine's. Do
    /// launch one working in a folder beneath its project's top, the volume laid over that top. Expect no share of the
    /// machine's, one served folder: the volume by its id, where its microVM sees the top, writable, given to its harness;
    /// its working folder the same place beneath it; its harness told it works there; the top's top-level directory a plug
    /// root. Do launch one whose workspace is mounted. Expect its folder shared from the machine, and nothing served.
    #[test]
    fn g265_a_workspace_volume_with_no_mount_is_served_to_its_microvm_and_no_folder_is_shared() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let top = dir.path().join("proj");
        let sub = top.join("crates").join("kiln");
        std::fs::create_dir_all(&sub)?;
        let (top_s, sub_s) = (top.to_string_lossy().into_owned(), sub.to_string_lossy().into_owned());
        let workspace = |mount: &str| crate::workspace::Workspace {
            kind: "volume".into(),
            over: Some(top_s.clone()),
            sub: "crates/kiln".into(),
            made: Some(crate::workspace::slates::Made { volume: "0123456789abcdef0123456789abcdef".into(), name: "kiln-x".into(), mount: mount.into() }),
            ..Default::default()
        };
        let spec = Spec { kind: "codex".into(), cwd: Some(sub_s.clone()), isolation: Some(microvm(None)), workspace: Some(workspace("")), ..Spec::default() };
        let launched = launch_of(kind("codex")?, &spec, "harness-codex@sha256:00".into(), &[])?;
        let at = guest_path(&top_s, PROJECTS).map_err(anyhow::Error::msg)?;
        let folder = format!("{at}/crates/kiln");
        assert!(launched.shares.is_empty(), "no folder of the machine's: {:?}", launched.shares);
        assert_eq!(
            launched.served,
            [kiln_seams::sandbox::Served { source: "0123456789abcdef0123456789abcdef".into(), at: at.clone(), writable: true, members: vec![kiln_seams::sandbox::Member("codex".into())] }],
            "its volume, served where its microVM sees the folder it lies over"
        );
        assert_eq!(launched.folder.as_deref(), Some(folder.as_str()), "its working folder, at the same place in it");
        assert_eq!(harness_cwd(&spec), folder, "its harness told it works there");
        assert_eq!(launched.plug_roots, plug_root_of(&at).into_iter().collect::<Vec<_>>());
        let mounted = Spec { workspace: Some(workspace(&top_s)), ..spec.clone() };
        let shared = launch_of(kind("codex")?, &mounted, "harness-codex@sha256:00".into(), &[])?;
        assert!(shared.served.is_empty(), "a mounted workspace is served nothing");
        assert_eq!(shared.shares.len(), 1, "its folder, shared from the machine");
        Ok(())
    }

    /// CP-T4.1's AC-T4.1.5 (step 17): the kiln server's data folder (kiln.db, its secrets and tokens; on a machine that
    /// runs both, the parent of its runner's default state) is kiln's state too, as its runner keeps it
    /// (`Host::server_data`). No isolated agent works in a folder that is, holds or lies in it, refused before anything
    /// starts; with its runner's state alone kept out, the server's folders beside that state were shared. Not made yet
    /// (its server not started), it compares as made folders do, through the folders it lies in (macOS's `/var` is
    /// `/private/var`), whichever way it and the project are named.
    #[test]
    fn s17_t4_the_kiln_servers_data_folder_is_no_isolated_agents_project() -> anyhow::Result<()> {
        let base = tempfile::tempdir()?;
        let (data, project) = (base.path().join("kiln"), base.path().join("project"));
        let (runner, secrets) = (data.join("agent"), data.join("secrets"));
        for dir in [&runner, &secrets, &project] {
            std::fs::create_dir_all(dir)?;
        }
        assert_eq!(kept_out(&secrets, &[runner.as_path()]), None, "the server's secrets, its runner's state alone kept out");
        let kiln = [runner.as_path(), data.as_path()];
        assert_eq!(kept_out(&project, &kiln), None, "a project beside kiln's data");
        for (folder, said) in [(data.as_path(), "lies in"), (secrets.as_path(), "lies in"), (base.path(), "holds")] {
            let why = kept_out(folder, &[data.as_path()]).unwrap_or_default();
            assert!(why.contains(said), "{}: {why}", folder.display());
        }
        let spec = Spec { kind: "codex".into(), cwd: Some(secrets.to_string_lossy().into_owned()), isolation: Some(microvm(None)), ..Spec::default() };
        let refusal = launch_of(kind("codex")?, &spec, "harness-codex@sha256:00".into(), &kiln).err().map(|e| e.to_string()).unwrap_or_default();
        assert!(refusal.contains("lies in"), "refused before anything starts: {refusal}");
        let real = std::fs::canonicalize(base.path())?;
        for (folder, unmade) in [(real.as_path(), base.path().join("unmade")), (base.path(), real.join("unmade"))] {
            assert!(kept_out(folder, &[unmade.as_path()]).is_some_and(|why| why.contains("holds")), "{} holds {}", folder.display(), unmade.display());
            let inside = folder.join("unmade").join("kiln.db");
            assert!(kept_out(&inside, &[unmade.as_path()]).is_some_and(|why| why.contains("lies in")), "{} lies in {}", inside.display(), unmade.display());
        }
        Ok(())
    }

    /// Step 17 part 7: a person's own Agentfile gives its harness the volumes its `VOLUME` lines give
    /// it, as Shards reads them: those for it, or for every agent and harness (`FOR` none, a name), each
    /// rooted at `/`; none of a Dockerfile's `VOLUME`, another harness's or an agent's. Its harness is
    /// kept where its `TO` says, else under `/harness`; one it doesn't declare is refused, saying so.
    #[test]
    fn s17_a_persons_own_agentfile_gives_its_harness_the_volumes_its_lines_give_it() -> anyhow::Result<()> {
        let text = "FROM scratch\nHARNESS claude FROM ./h TO /opt/claude\nHARNESS codex FROM ./c\nAGENT helper FROM ./a\n\
            VOLUME --target-kind=harness /data/claude FOR claude\nVOLUME --target-kind=harness /data/codex FOR codex\n\
            VOLUME --target-kind=agent /data/helper FOR helper\nVOLUME --chown=1:1 /shared/ FOR claude codex\n\
            VOLUME cache /var/cache/own\nVOLUME /docker-only\n";
        let (claude, at) = kept_volumes(text, "claude").map_err(anyhow::Error::msg)?;
        assert_eq!((claude, at), (vec!["/data/claude".to_owned(), "/shared".to_owned(), "/var/cache/own".to_owned()], "/opt/claude".to_owned()));
        let (codex, at) = kept_volumes(text, "codex").map_err(anyhow::Error::msg)?;
        assert_eq!((codex, at), (vec!["/data/codex".to_owned(), "/shared".to_owned(), "/var/cache/own".to_owned()], "/harness/codex".to_owned()));
        let undeclared = kept_volumes(text, "goose").err().unwrap_or_default();
        assert!(undeclared.contains("declares no harness goose"), "{undeclared}");
        assert!(kept_volumes("FROM\n", "claude").is_err(), "one that doesn't parse says why");
        Ok(())
    }

    /// A Shards store at `root` holding image `name` as Shards' build leaves one: each of `layers` (its
    /// files by path and text, the lowest first) an uncompressed tar, its config of environment `env`,
    /// its manifest, tagged. Its name pinned by its manifest's digest, and its layers.
    fn image_in(root: &std::path::Path, name: &str, env: &[&str], layers: &[&[(&str, &str)]]) -> anyhow::Result<(String, Vec<shards_image::store::Layer>)> {
        use sha2::Digest as _;
        use shards_image::reference::{Digest, Reference};
        use shards_image::store::{Layer, Store};
        std::fs::create_dir_all(root)?;
        let store = Store::open(root).map_err(|e| anyhow::anyhow!("{e}"))?;
        let put = |bytes: &[u8]| -> anyhow::Result<(Digest, u64)> {
            let digest = Digest::parse(&format!("sha256:{}", hex::encode(sha2::Sha256::digest(bytes)))).map_err(|e| anyhow::anyhow!("{e}"))?;
            let (size, mut src) = (u64::try_from(bytes.len())?, bytes);
            store.ingest(&digest, size, &mut src).map_err(|e| anyhow::anyhow!("{e}"))?;
            Ok((digest, size))
        };
        let mut stored = Vec::new();
        for files in layers {
            let mut tar = tar::Builder::new(Vec::new());
            for (path, text) in *files {
                let mut header = tar::Header::new_ustar();
                header.set_size(u64::try_from(text.len())?);
                header.set_mode(0o644);
                tar.append_data(&mut header, path, text.as_bytes())?;
            }
            let (digest, size) = put(&tar.into_inner()?)?;
            stored.push((Layer { blob: digest.clone(), media_type: "application/vnd.oci.image.layer.v1.tar".into(), diff_id: digest }, size));
        }
        let diff_ids: Vec<String> = stored.iter().map(|(l, _)| l.diff_id.to_string()).collect();
        let config = serde_json::json!({"architecture": "arm64", "os": "linux", "config": {"Env": env}, "rootfs": {"type": "layers", "diff_ids": diff_ids}});
        let (config, config_size) = put(&serde_json::to_vec(&config)?)?;
        let described: Vec<serde_json::Value> = stored.iter().map(|(l, size)| serde_json::json!({"mediaType": l.media_type, "digest": l.blob.to_string(), "size": size})).collect();
        let manifest = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {"mediaType": "application/vnd.oci.image.config.v1+json", "digest": config.to_string(), "size": config_size},
            "layers": described,
        });
        let (digest, size) = put(&serde_json::to_vec(&manifest)?)?;
        let desc = shards_image::oci::Descriptor { media_type: "application/vnd.oci.image.manifest.v1+json".into(), digest: digest.to_string(), size: i64::try_from(size)?, platform: None, annotations: Default::default() };
        let reference = Reference::parse(&format!("{name}:built")).map_err(|e| anyhow::anyhow!("{e}"))?.to_string();
        store.tag(&reference, &desc, &digest, &[]).map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok((format!("{name}@{digest}"), stored.into_iter().map(|(l, _)| l).collect()))
    }

    /// Step 17 part 7: a person's own image's harness runs with the environment Shards' init gives it,
    /// read from its image as its build left it: its image's environment but its `PATH` and `HOME`, then
    /// its harness's `run.env` (`<dir>.d/osi.json`); from the topmost of its layers that says anything
    /// of it while its store keeps no root file system of them, and from that once it does (its layers
    /// gone). A whiteout of its folder, or an opaque folder above it, in a layer above leaves it none,
    /// and so does an image its store doesn't hold: each said.
    #[test]
    fn s17_a_persons_own_images_harness_runs_with_its_images_environment() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().join("images");
        let osi = |env: &[&str]| serde_json::json!({"schemaVersion": 1, "name": "codex", "run": {"command": ["codex"], "env": env}}).to_string();
        let (old, new) = (osi(&["HOME=/old"]), osi(&["HOME=/home/codex", "CODEX_HOME=/data/codex"]));
        let image_env = ["PATH=/usr/bin", "HOME=/root", "LANG=C.UTF-8"];
        let (pinned, layers) = image_in(&root, "agentfile-0123", &image_env, &[&[("harness/codex.d/osi.json", &old), ("etc/x", "x")], &[("harness/codex.d/osi.json", &new)]])?;
        let want = [("LANG", "C.UTF-8"), ("HOME", "/home/codex"), ("CODEX_HOME", "/data/codex")].map(|(k, v)| (k.to_owned(), v.to_owned()));
        assert_eq!(harness_env(&root, &pinned, "/harness/codex").map_err(anyhow::Error::msg)?, want, "from its layers, the topmost");
        let store = shards_image::store::Store::open(&root).map_err(|e| anyhow::anyhow!("{e}"))?;
        store.rootfs(&layers, &shards_image::store::Limits::none()).map_err(|e| anyhow::anyhow!("{e}"))?;
        for l in &layers {
            std::fs::remove_file(store.blob_path(&l.blob))?;
        }
        assert_eq!(harness_env(&root, &pinned, "/harness/codex").map_err(anyhow::Error::msg)?, want, "from its root file system, its layers gone");
        for (name, hiding) in [("agentfile-wh", "harness/.wh.codex.d"), ("agentfile-opq", "harness/.wh..wh..opq")] {
            let (pinned, _) = image_in(&root, name, &image_env, &[&[("harness/codex.d/osi.json", &new)], &[(hiding, "")]])?;
            let none = harness_env(&root, &pinned, "/harness/codex").err().unwrap_or_default();
            assert!(none.contains("holds no /harness/codex.d/osi.json"), "{hiding}: {none}");
        }
        let unheld = harness_env(&root, &format!("agentfile-0123@sha256:{}", "0".repeat(64)), "/harness/codex").err().unwrap_or_default();
        assert!(unheld.contains("holds no agentfile-0123@sha256:"), "{unheld}");
        Ok(())
    }

    /// Step 17 part 7: Claude Code loads kiln's own skills and those kiln installed for its session
    /// from their folders, as on the machine; another harness takes none.
    #[test]
    fn s17_claude_codes_plugin_folders_are_kilns_skills_and_its_sessions() -> anyhow::Result<()> {
        let own = crate::mcp::plugin_dir().map(|d| d.to_string_lossy().into_owned()).ok_or_else(|| anyhow::anyhow!("no folder of kiln's skills"))?;
        let installed = crate::skills::place::session_folder("s-1").map(|d| d.to_string_lossy().into_owned()).ok_or_else(|| anyhow::anyhow!("no folder of its installed skills"))?;
        assert_eq!(plugin_folders(kind("claude")?, "s-1"), [own, installed]);
        assert!(plugin_folders(kind("codex")?, "s-1").is_empty());
        Ok(())
    }

    fn builtin_of(harness: &str) -> anyhow::Result<&'static builtin::Builtin> {
        builtin::ALL.iter().find(|b| b.harness == harness).ok_or_else(|| anyhow::anyhow!("kiln ships no Agentfile for {harness}"))
    }

    /// Step 17 part 6: kiln's own Agentfile for a harness of several providers is narrowed to its
    /// session's, read from its person's home on this machine as `harness::isolated::home` lays it out
    /// (its run's environment as its image's `run.env` sets it), or from its project at its own path;
    /// another person's home is never read; with no person or no homes kept, its image stays whole,
    /// saying why; one of one provider stays whole, no fallback.
    #[test]
    fn s17_a_builtins_run_reaches_its_sessions_own_use_read_from_its_persons_home() -> anyhow::Result<()> {
        let homes = tempfile::tempdir()?;
        let goose = builtin_of("goose")?;
        let (volumes, env) = crate::harness::isolated::home::of_builtin(goose);
        let layout = crate::harness::isolated::home::Layout::new(homes.path(), "user:u1", "goose", &volumes, &env)?;
        // kiln's goose image roots Goose's files at its harness.json's GOOSE_PATH_ROOT, its config beneath it.
        let root = env.iter().find(|(k, _)| k == "GOOSE_PATH_ROOT").map(|(_, v)| v.clone()).ok_or_else(|| anyhow::anyhow!("kiln's goose image roots no GOOSE_PATH_ROOT"))?;
        let config = layout.host_of(&format!("{root}/config/config.yaml")).ok_or_else(|| anyhow::anyhow!("no host path for goose's config"))?;
        std::fs::create_dir_all(config.parent().ok_or_else(|| anyhow::anyhow!("a config with no folder"))?)?;
        std::fs::write(&config, "GOOSE_PROVIDER: anthropic\n")?;
        let reach = reach_of(goose, Some("user:u1"), None, Some(homes.path()), None, &[]);
        let Reach::Narrowed { uses, hosts } = &reach else { anyhow::bail!("not narrowed: {reach:?}") };
        assert_eq!((uses.as_slice(), hosts.as_slice()), (&["anthropic".to_owned()][..], &["api.anthropic.com:443".to_owned()][..]));
        assert_eq!(egress_of(&reach, Some(vec!["other.example:443".to_owned()])), Egress::Only(hosts.clone()), "its egress, those alone");
        // Its run's own environment comes before its configuration.
        let groq = reach_of(goose, Some("user:u1"), None, Some(homes.path()), None, &[("GOOSE_PROVIDER".to_owned(), "groq".to_owned())]);
        assert!(matches!(&groq, Reach::Narrowed { uses, .. } if uses == &["groq"]), "{groq:?}");
        // Another person's home isn't read: theirs says nothing here.
        assert!(matches!(reach_of(goose, Some("user:u2"), None, Some(homes.path()), None, &[]), Reach::Whole { why: Whole::Untold { .. } }));
        // No person to read for, or no homes kept on this machine: whole, a fallback the session says.
        assert_eq!(reach_of(goose, None, None, Some(homes.path()), None, &[]), Reach::Whole { why: Whole::NoHome });
        assert_eq!(reach_of(goose, Some("user:u1"), None, None, None, &[]), Reach::Whole { why: Whole::NoHome });
        assert!(Whole::NoHome.is_fallback());
        // Kept whole: each destination its image grants, by name, or its image's own where it grants more than names.
        let goose_hosts = granted(goose.text).map_err(anyhow::Error::msg)?.ok_or_else(|| anyhow::anyhow!("goose's image grants more than names"))?;
        assert!(goose_hosts.iter().any(|h| h == "api.anthropic.com:443") && goose_hosts.iter().any(|h| h == "api.groq.com:443"), "{goose_hosts:?}");
        assert_eq!(egress_of(&Reach::Whole { why: Whole::NoHome }, Some(goose_hosts.clone())), Egress::Only(goose_hosts));
        assert_eq!(egress_of(&Reach::Whole { why: Whole::NoHome }, None), Egress::Any);
        assert_eq!(granted(&goose.text.replace("NETWORK --egress=", "NETWORK --egress=443/tcp --egress=")).map_err(anyhow::Error::msg)?, None, "a port to any address is no list of names");
        assert!(granted("FROM\n").is_err(), "an Agentfile that doesn't parse says why");
        // opencode's project at its own path, read where it is.
        let project = tempfile::tempdir()?;
        std::fs::write(project.path().join("opencode.json"), "{\"model\": \"openai/gpt-5\"}")?;
        let shared = project_share(&project.path().to_string_lossy()).map_err(anyhow::Error::msg)?;
        let opencode = reach_of(builtin_of("opencode")?, Some("user:u1"), None, Some(homes.path()), Some(&shared), &[]);
        assert!(matches!(&opencode, Reach::Narrowed { uses, .. } if uses == &["openai"]), "{opencode:?}");
        // One provider's image: its destinations are all its run's.
        assert_eq!(reach_of(builtin_of("claude")?, Some("user:u1"), None, Some(homes.path()), None, &[]), Reach::Whole { why: Whole::OneUse });
        Ok(())
    }
}
