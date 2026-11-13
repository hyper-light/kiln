//! kiln's own reads of a harness's files, as they are for a process on its machine, reading an
//! isolated session's person's volume through its layout's host view (kiln plan
//! 01a1cd8e862e7169a074db3d5fa33f3d, step 18): its transcript for history and handoffs, Codex's
//! home for its rollouts, a move's package, each from the volume and none from the machine's own
//! home, and no credential carried out of the volume.

use std::path::Path;

use super::home::{Layout, of_builtin};

fn builtin(name: &str) -> anyhow::Result<&'static crate::agentfiles::builtin::Builtin> {
    crate::agentfiles::builtin::ALL.iter().find(|b| b.name == name).ok_or_else(|| anyhow::anyhow!("no built-in {name}"))
}

fn layout_of(root: &Path, person: &str, image: &str) -> anyhow::Result<Layout> {
    let b = builtin(image)?;
    let (volumes, env) = of_builtin(b);
    Ok(Layout::new(root, person, b.harness, &volumes, &env)?)
}

/// Claude Code's transcript, written in the microVM under its home (`/home/claude/.claude`), is
/// what kiln's history and handoff read on the machine: at its volume's path here.
#[test]
fn step18_claude_codes_transcript_is_read_from_its_persons_volume() -> anyhow::Result<()> {
    let t = tempfile::tempdir()?;
    let l = layout_of(t.path(), "user:u1", "harness-claude")?;
    l.prepare()?;
    let env = l.host_env();
    let config = crate::harness::transcript::config_dir(&env);
    let in_vm = l.host_of("/home/claude/.claude").ok_or_else(|| anyhow::anyhow!("no host path"))?;
    anyhow::ensure!(config == in_vm, "{} != {}", config.display(), in_vm.display());
    // The project at its host path in the microVM (step 8): Claude Code names its folder for it.
    let project = config.join("projects").join("-Users-ada-src-app");
    std::fs::create_dir_all(&project)?;
    std::fs::write(project.join("s1.jsonl"), "{\"type\":\"user\"}\n")?;
    anyhow::ensure!(crate::harness::transcript::find(&config, "s1") == Some(project.join("s1.jsonl")));
    // Another person's volume of the same harness holds none of it.
    let other = layout_of(t.path(), "user:u2", "harness-claude")?;
    anyhow::ensure!(crate::harness::transcript::find(&crate::harness::transcript::config_dir(&other.host_env()), "s1").is_none());
    Ok(())
}

/// Codex's home, where its rollouts are, is its volume's path here.
#[test]
fn step18_codex_rollouts_are_read_from_its_persons_volume() -> anyhow::Result<()> {
    let t = tempfile::tempdir()?;
    let l = layout_of(t.path(), "owner", "harness-codex")?;
    let home = crate::harness::handoff::codex_home(&l.host_env());
    anyhow::ensure!(Some(home.clone()) == l.host_of("/home/codex/.codex"), "{}", home.display());
    anyhow::ensure!(home.starts_with(t.path().join("owner").join("codex")));
    Ok(())
}

/// A move's package of what Codex keeps for a session is read from its person's volume, and
/// carries no credential: what's a credential stays in the volume, as on its machine.
#[test]
fn step18_no_credential_leaves_its_persons_volume() -> anyhow::Result<()> {
    let t = tempfile::tempdir()?;
    let l = layout_of(t.path(), "user:u1", "harness-codex")?;
    l.prepare()?;
    let codex = l.host_of("/home/codex/.codex").ok_or_else(|| anyhow::anyhow!("no host path"))?;
    let memories = codex.join("memories");
    std::fs::create_dir_all(&memories)?;
    std::fs::write(memories.join("notes.md"), "the project builds with make\n")?;
    for secret in ["auth.json", ".credentials.json", "id_ed25519.pem", ".env"] {
        std::fs::write(memories.join(secret), "not-a-real-secret\n")?;
    }
    std::fs::write(codex.join("auth.json"), "{\"OPENAI_API_KEY\":\"not-a-real-key\"}\n")?;
    let dirs = crate::harness::homes::Dirs::here(&l.host_env());
    let home = crate::harness::homes::pack("codex", &dirs, None, "/Users/ada/src/app", &[], None);
    let paths: Vec<&str> = home.files.iter().map(|f| f.path.as_str()).collect();
    anyhow::ensure!(paths == ["notes.md"], "{paths:?}");
    Ok(())
}

/// Every harness's home kiln reads for a session in a microVM (its memories and knowledge for a
/// move, whose each changed file is) is in its person's volume, with Linux's places whatever this
/// machine is: not Windows' `APPDATA` for Goose, nor macOS's `Library/Caches`.
#[test]
fn step18_a_microvms_harness_homes_are_its_volumes_on_any_machine() -> anyhow::Result<()> {
    let t = tempfile::tempdir()?;
    for image in ["harness-goose", "harness-antigravity", "harness-gemini", "harness-claude"] {
        let l = layout_of(t.path(), "owner", image)?;
        let dirs = crate::harness::homes::Dirs::microvm(&l.host_env()).ok_or_else(|| anyhow::anyhow!("{image}: no home"))?;
        let base = t.path().join("owner");
        anyhow::ensure!(dirs.home().starts_with(&base), "{image}: {}", dirs.home().display());
        anyhow::ensure!(dirs.cache().starts_with(&base) && !dirs.cache().ends_with("Library/Caches"), "{image}: {}", dirs.cache().display());
        for h in dirs.harness_homes() {
            anyhow::ensure!(h.starts_with(&base), "{image}: {}", h.display());
        }
    }
    let goose = layout_of(t.path(), "owner", "harness-goose")?;
    let memory_home = goose.host_of("/home/goose/.local/share/goose/config").ok_or_else(|| anyhow::anyhow!("no host path"))?;
    let dirs = crate::harness::homes::Dirs::microvm(&goose.host_env()).ok_or_else(|| anyhow::anyhow!("no home"))?;
    anyhow::ensure!(dirs.harness_homes().contains(&memory_home), "{:?}", dirs.harness_homes());
    let agy = layout_of(t.path(), "owner", "harness-antigravity")?;
    let knowledge_home = agy.host_of("/home/antigravity/.gemini/antigravity-cli").ok_or_else(|| anyhow::anyhow!("no host path"))?;
    let dirs = crate::harness::homes::Dirs::microvm(&agy.host_env()).ok_or_else(|| anyhow::anyhow!("no home"))?;
    anyhow::ensure!(dirs.harness_homes().contains(&knowledge_home), "{:?}", dirs.harness_homes());
    Ok(())
}

/// An isolated session whose environment isn't its volume's view reads nothing of its harness's,
/// never this machine's own person's files instead.
#[test]
fn step18_an_isolated_session_without_its_volumes_view_reads_nothing_here() {
    let spec = crate::harness::session::Spec { kind: "goose".into(), isolation: Some(crate::harness::session::Isolation::microvm()), ..Default::default() };
    assert!(crate::harness::homes::Dirs::of(&spec).is_none());
    let roots = crate::harness::whose::Roots::of(&spec);
    assert!(roots.harness.is_empty() && roots.home.as_os_str().is_empty(), "{:?}", roots.harness);
    // A process on its machine reads this machine's, as before.
    let here = crate::harness::session::Spec { kind: "goose".into(), ..Default::default() };
    assert!(crate::harness::homes::Dirs::of(&here).is_some_and(|d| d.home() == crate::os::home_dir()));
}
