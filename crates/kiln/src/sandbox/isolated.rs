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
//! - **What it can't do yet**, refused rather than done another way: move to another machine, or
//!   switch to another harness ([`NO_MOVE`], [`NO_SWITCH`]); reach kiln's own tools (plan step 18).

use std::path::Path;

use kiln_seams::sandbox::{Boundary, Egress, Launch, Limits, Share};

use super::Microvms;
use super::image::{Images, Source};
use super::narrow::{Reach, Whole};
use crate::agentfiles::builtin;
use crate::harness::isolated::home;
use crate::harness::kinds::{Kind, Protocol};
use crate::harness::session::{Isolation, Spec};

/// Why an isolated agent doesn't move to another machine.
pub const NO_MOVE: &str = "an isolated agent doesn't move to another machine yet: its microVM and what its harness keeps there are its machine's; end it, or start it anew there";

/// Why an isolated agent doesn't switch to another harness.
pub const NO_SWITCH: &str = "An isolated agent doesn't switch to another harness yet: its microVM runs its own harness's image. End it, and start the other harness isolated.";

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
/// step 32).
pub fn launch_of(kind: &Kind, spec: &Spec, image: String) -> anyhow::Result<Launch> {
    let cwd = crate::harness::session::resolve_cwd(spec.cwd.as_deref());
    anyhow::ensure!(std::path::Path::new(&cwd).is_dir(), "{cwd} is not a directory on this machine");
    let run = kind.elsewhere(spec)?;
    let project = project_share(&cwd).map_err(anyhow::Error::msg)?;
    Ok(Launch {
        program: run.member.to_owned(),
        args: run.args,
        folder: Some(project.at.clone()),
        shares: vec![project],
        env: run.env,
        limits: Limits::NONE,
        egress: Egress::Any,
        boundary: Boundary::MicroVm { image },
        // No credential granted by reference yet (D242): its harness signs in natively until its sign-in is captured
        // outside the microVM (plan step 33's steps 6 and 8), so nothing is filled in at its egress.
        credentials: Vec::new(),
    })
}

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
/// (`isolated::env::member_env`) and kiln's MCP through its link's lane ([`offer_mcp`]).
pub async fn launch(kind: &Kind, spec: &Spec, session: &str, vms: &Microvms, homes: Option<&std::path::Path>) -> anyhow::Result<crate::proc::Duplex> {
    use crate::harness::isolated::{env, host_tools};
    let source = source_of(kind, spec).map_err(anyhow::Error::msg)?;
    let mut launch = launch_of(kind, spec, source.name())?;
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
            let (model, homes, project, env) = (spec.model.clone(), homes.map(std::path::Path::to_path_buf), launch.shares.first().cloned(), launch.env.clone());
            crate::rt::blocking(move || reach_of(b, person.as_deref(), model.as_deref(), homes.as_deref(), project.as_ref(), &env)).await?
        }
    };
    let text = match source {
        Source::Builtin(b) => b.text,
        Source::Kept { text, .. } => text,
    };
    // Where it doesn't parse, its build says so in Shards' words; its egress stays as its image's.
    launch.egress = egress_of(&reach, granted(text).ok().flatten());
    let image = vms.images.pinned(source).await.map_err(|r| anyhow::anyhow!("{}'s image, {}: {r}", kind.name, source.name()))?;
    launch.boundary = Boundary::MicroVm { image: image.clone() };
    let person = spec.isolation.as_ref().and_then(|i| i.person.as_deref());
    let (mounts, kept_in) = match (homes, person) {
        (Some(root), Some(person)) => home_of(root, person, kind, source, &vms.images, &image).await?,
        _ => (Vec::new(), None),
    };
    launch.shares.extend(mounts.into_iter().map(|m| Share { host: m.host.to_string_lossy().into_owned(), at: m.guest, writable: true }));
    for dir in plugin_folders(kind, session) {
        let at = guest_path(&dir, PLUGINS).map_err(anyhow::Error::msg)?;
        launch.args.extend(["--plugin-dir".to_owned(), at.clone()]);
        launch.shares.push(Share { host: dir, at, writable: false });
    }
    // kiln's MCP once its person's volume is made (agy's config is kept there).
    offer_mcp(kind, &source, person, homes, &mut launch.args).await;
    let name = match (&spec.claim, crate::reach::node::global()) {
        (Some(claim), Some(reach)) => reach.box_of(claim.clone()).await,
        _ => None,
    };
    let run = vms.launch(name, launch).await.map_err(|r| anyhow::anyhow!("{} couldn't start in a microVM of {image}: {r}", kind.name))?;
    // Placement reads its use from its VM's meter from now on (#73 part 7 slice 3).
    if let (Some(claim), Some(reach), Some(group)) = (&spec.claim, crate::reach::node::global(), super::group_of(vms, run.as_ref())) {
        reach.session_grouped(claim.clone(), group).await;
    }
    Ok(crate::proc::duplex_of_run(run, image, reach, kept_in))
}

/// Where a microVM sees a Windows machine's project folder: beneath this, by its own name ([`guest_path`]).
pub const PROJECTS: &str = "/work";

/// Where a microVM sees a Windows machine's plugin folders: beneath this, each by its own name.
pub const PLUGINS: &str = "/plugins";

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

/// The project folder `cwd` as a launch shares it: writable, where [`guest_path`] puts it in the
/// microVM, which is its member's working folder too.
pub fn project_share(cwd: &str) -> Result<Share, String> {
    Ok(Share { host: cwd.to_owned(), at: guest_path(cwd, PROJECTS)?, writable: true })
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
/// its directory here and its path in the microVM (`Layout::prepare`), and where its harness keeps its
/// conversations there (`Layout::kept_in`). kiln's own Agentfile says its harness's volumes and run
/// environment (`home::of_builtin`); a person's own, its volumes ([`kept_volumes`]), and its image, its
/// harness's environment ([`harness_env`]). Refused, typed, where kiln can't keep it
/// (`harness::isolated::Refusal`), or in words where its image can't be read.
async fn home_of(root: &Path, person: &str, kind: &Kind, source: Source<'_>, images: &Images, pinned: &str) -> anyhow::Result<(Vec<home::Mount>, Option<String>)> {
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
    let kept_in = layout.kept_in();
    let mounts = crate::rt::blocking(move || layout.prepare()).await??;
    Ok((mounts, kept_in))
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
    let run = osi::Config::parse(&osi).map_err(|e| format!("{path}: {e}"))?.run.map(|r| r.env).unwrap_or_default();
    let image = config.config.and_then(|c| c.env).unwrap_or_default();
    let image = image.into_iter().filter(|e| !matches!(e.split_once('=').map(|(k, _)| k), Some("PATH" | "HOME")));
    Ok(image.chain(run).filter_map(|e| e.split_once('=').map(|(k, v)| (k.to_owned(), v.to_owned()))).collect())
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
/// in its own config in its person's volume under `homes` (the only place agy reads one from, kept as
/// agy keeps it: `mcp::agy_config`), for kiln's own Agentfile, whose volumes kiln keeps; an ACP
/// agent's is given at its `session/new` (`kinds::acp_servers`). A config that couldn't be written
/// starts it without kiln's MCP, said, as on its machine (`Kind::prepare`).
async fn offer_mcp(kind: &Kind, source: &Source<'_>, person: Option<&str>, homes: Option<&Path>, args: &mut Vec<String>) {
    use crate::harness::isolated::mcp;
    match mcp::for_harness(kind) {
        mcp::Config::Args(more) => args.extend(more),
        mcp::Config::Shared(_) => {
            let (Source::Builtin(b), Some(person), Some(root)) = (source, person, homes) else { return };
            let (b, person, root) = (*b, person.to_owned(), root.to_path_buf());
            let put = crate::rt::blocking(move || -> anyhow::Result<()> {
                let (volumes, image_env) = home::of_builtin(b);
                let layout = home::Layout::new(&root, &person, b.harness, &volumes, &image_env)?;
                let guest = format!("{}/.gemini/config/mcp_config.json", layout.guest_home().trim_end_matches('/'));
                let at = layout.host_of(&guest).ok_or_else(|| anyhow::anyhow!("{guest} isn't a path in its microVM"))?;
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

    /// Step 17: an isolated launch is its image's harness member, in the session's folder at its own
    /// path, given only what its image's command doesn't: Codex none, Claude Code its session's
    /// flags; of the session's environment only its locale and time zone, and no kiln variable.
    #[test]
    fn s17_an_isolated_launch_is_the_members_with_what_the_session_adds() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let cwd = dir.path().to_string_lossy().into_owned();
        let env = vec![("PATH".to_owned(), "/host/bin".to_owned()), ("LANG".to_owned(), "fr_FR.UTF-8".to_owned()), ("TZ".to_owned(), "Europe/Paris".to_owned()), ("KILN_REACH_SOCK".to_owned(), "/host/sock".to_owned())];
        let spec = |kind: &str| Spec { kind: kind.into(), cwd: Some(cwd.clone()), env: env.clone(), isolation: Some(microvm(None)), ..Spec::default() };
        let codex = launch_of(kind("codex")?, &spec("codex"), "harness-codex@sha256:00".into())?;
        assert_eq!((codex.program.as_str(), codex.args.as_slice()), ("codex", &[][..]), "its image's command runs it as kiln's adapter drives it");
        assert_eq!(codex.boundary, Boundary::MicroVm { image: "harness-codex@sha256:00".into() });
        assert_eq!(codex.folder.as_deref(), Some(cwd.as_str()));
        assert_eq!(codex.shares, [Share::at_its_path(&cwd)], "its project, at its own path: its home comes with its person's volume");
        // A Windows machine's project: shared from where it is, at a Linux path of its own name, which is
        // its working folder too; Unix's at its own path.
        let windows = project_share(r"C:\Users\ada\proj").map_err(anyhow::Error::msg)?;
        assert_eq!(windows, Share { host: r"C:\Users\ada\proj".into(), at: "/work/proj".into(), writable: true });
        for (host, at) in [(r"\\?\C:\Users\ada\proj\", "/work/proj"), ("D:/src/kiln", "/work/kiln"), (r"\\server\share\repo", "/work/repo"), ("/Users/ada/proj", "/Users/ada/proj")] {
            assert_eq!(guest_path(host, PROJECTS), Ok(at.to_owned()), "{host}");
        }
        for nameless in [r"C:\", "C:/", r"\\?\C:\", "relative\\proj"] {
            assert!(guest_path(nameless, PROJECTS).is_err(), "{nameless}");
        }
        assert_eq!(codex.env, vec![("LANG".to_owned(), "fr_FR.UTF-8".to_owned()), ("TZ".to_owned(), "Europe/Paris".to_owned())]);
        let resumed = Spec { resume: Some("conversation-1".into()), model: Some("claude-opus-5-5".into()), allow_bypass: false, ..spec("claude") };
        let claude = launch_of(kind("claude")?, &resumed, "harness-claude@sha256:00".into())?;
        assert_eq!(claude.program, "claude");
        assert_eq!(claude.args, ["--model", "claude-opus-5-5", "--resume", "conversation-1"], "its session's own flags, past its image's");
        assert!(claude.env.contains(&("CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING".to_owned(), "1".to_owned())));
        assert!(!claude.env.iter().any(|(k, _)| k == "PATH" || k.starts_with("KILN_")), "no host variable, no kiln socket: {:?}", claude.env);
        // A folder this machine hasn't is refused before anything starts.
        let gone = Spec { cwd: Some(dir.path().join("gone").to_string_lossy().into_owned()), ..spec("codex") };
        assert!(launch_of(kind("codex")?, &gone, "harness-codex@sha256:00".into()).is_err());
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
