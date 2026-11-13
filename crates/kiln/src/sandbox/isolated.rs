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
//! - **What it can't do yet**, refused rather than done another way: move to another machine, or
//!   switch to another harness ([`NO_MOVE`], [`NO_SWITCH`]); reach kiln's own tools (plan step 18).

use kiln_seams::sandbox::{Boundary, Egress, Launch, Limits};

use super::Microvms;
use super::image::Source;
use crate::agentfiles::builtin;
use crate::harness::kinds::Kind;
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
/// with its image's grants narrowed no further and the microVM sized as its boundary sizes one (the
/// person's sizes reach it through its placement: plan step 32).
pub fn launch_of(kind: &Kind, spec: &Spec, image: String) -> anyhow::Result<Launch> {
    let cwd = crate::harness::session::resolve_cwd(spec.cwd.as_deref());
    anyhow::ensure!(std::path::Path::new(&cwd).is_dir(), "{cwd} is not a directory on this machine");
    let run = kind.elsewhere(spec)?;
    Ok(Launch {
        program: run.member.to_owned(),
        args: run.args,
        folder: Some(cwd),
        shares: Vec::new(),
        env: run.env,
        limits: Limits::NONE,
        egress: Egress::Any,
        boundary: Boundary::MicroVm { image },
    })
}

/// Starts `kind`'s harness for isolated `spec` in the machine's microVMs `vms`: its image pinned by
/// its digest (`image::Images::pinned`, built where their store holds none of its content), then its
/// member's run as the `Duplex` its session drives (`crate::proc::duplex_of_run`); or why not, in the
/// images' or the sandbox's own words. What its launch takes of the session is checked before
/// anything is built.
pub async fn launch(kind: &Kind, spec: &Spec, vms: &Microvms) -> anyhow::Result<crate::proc::Duplex> {
    let source = source_of(kind, spec).map_err(anyhow::Error::msg)?;
    let mut launch = launch_of(kind, spec, source.name())?;
    let image = vms.images.pinned(source).await.map_err(|r| anyhow::anyhow!("{}'s image, {}: {r}", kind.name, source.name()))?;
    launch.boundary = Boundary::MicroVm { image: image.clone() };
    let run = vms.sandbox.launch(launch).await.map_err(|r| anyhow::anyhow!("{} couldn't start in a microVM of {image}: {r}", kind.name))?;
    Ok(crate::proc::duplex_of_run(run, image))
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
}
