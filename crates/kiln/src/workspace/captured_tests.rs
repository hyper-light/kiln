//! A workspace on a runner in kiln's Slates fleet is a clone of its folder's captured base (kiln goal 95101ef3, K5;
//! Slates' A-291, A-424 and A-463), against a Slates instance of the test's own run by a kiln executable in Slates'
//! roles (`KILN_TEST_KILN`, as `bare_metal_tests`); skipped, saying so, without one. It mounts nothing: the agent's
//! changes are written through Slates' own file verbs, so it runs on any machine whatever it can mount, and the
//! instance it started is stopped at its end whatever happened.
//!
//! - A new workspace is a captured clone: a plain volume (no base plane, what Slates' move takes) reading the folder as
//!   it was captured; a second over the folder unchanged shares the capture; one after the folder changed has a new
//!   one; an overlay (the control) has a base plane.
//! - Its review is the agent's changes since its capture, never an outsider's edit made after it; its drift is the
//!   folder's change since the capture, read from its base.
//! - Its landing writes those changes alone, under kiln's launcher authority, and passes the outsider's edit.
//! - A base goes with the last workspace that's a clone of it.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use serde_json::Value;
use slates_api::{Intent, VolumeId};

use super::git::Change;
use super::slates::{Made, call};

/// How long the whole test may take: an instance's start, three captures of a small folder, a review, a landing and the
/// ends, on a loaded machine.
const WITHIN: Duration = Duration::from_secs(600);
/// How long the instance this test started may take to stop once asked: its anchor stops its daemon first, ending it
/// past the anchor's own bound (Slates' `Anchor::stop`). One still answering after this is said loudly and ended
/// outright, which is safe as the test mounted nothing.
const STOPS_WITHIN: Duration = Duration::from_secs(60);
/// How often the instance is asked whether it still answers while its stop is waited on.
const LOOK_EVERY: Duration = Duration::from_millis(100);

/// Git, in `dir`, with no person's configuration: its output, or what it refused.
fn git(dir: &Path, args: &[&str]) -> anyhow::Result<String> {
    let out = Command::new("git").args(args).current_dir(dir).env("GIT_CONFIG_GLOBAL", "/dev/null").env("GIT_CONFIG_NOSYSTEM", "1").env("GIT_TERMINAL_PROMPT", "0").output()?;
    anyhow::ensure!(out.status.success(), "git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr));
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// A repository of three committed files in `work`: its top as git names it.
fn project(work: &Path) -> anyhow::Result<String> {
    git(work, &["init", "-q", "-b", "main"])?;
    for (key, value) in [("user.name", "Ada"), ("user.email", "ada@example.com"), ("commit.gpgsign", "false")] {
        git(work, &["config", key, value])?;
    }
    std::fs::create_dir_all(work.join("src"))?;
    std::fs::write(work.join("README.md"), "readme\n")?;
    std::fs::write(work.join("src").join("a.rs"), "a\n")?;
    std::fs::write(work.join("src").join("b.rs"), "b\n")?;
    git(work, &["add", "."])?;
    git(work, &["commit", "-qm", "init"])?;
    git(work, &["rev-parse", "--show-toplevel"])
}

/// K5 (kiln goal 95101ef32ffb8b099942fe076619c310; Slates' A-424, A-463): a fleet runner's workspace is a captured clone,
/// plain so a move takes it; its review and its landing are the agent's changes since its capture, and an outsider's
/// edit after the capture is passed.
#[test]
fn k5_a_fleet_workspace_is_a_captured_clone_whose_review_and_landing_are_the_agents_changes_since_its_capture() -> anyhow::Result<()> {
    // The kiln executable whose `slates-role` runs the test's instance: a unit test's own executable isn't kiln's.
    let Some(program) = std::env::var_os("KILN_TEST_KILN") else {
        eprintln!("skipped: KILN_TEST_KILN names no kiln executable (cargo build -p kiln, then KILN_TEST_KILN=<target>/debug/kiln)");
        return Ok(());
    };
    let o = super::slates::Override { instance: format!("kilntestk5{}", std::process::id()), program: Some(program.into()), quick: true };
    if !super::slates::set_override(o) {
        eprintln!("skipped: another test of this process points workspaces at its own instance: run k5_ alone");
        return Ok(());
    }
    let state = tempfile::tempdir()?;
    super::slates::init(state.path());
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(crate::rt::block_on(scenario()));
    });
    let answered = rx.recv_timeout(WITHIN);
    // It mounted nothing, so its instance goes whatever happened.
    if let Some(pid) = super::slates::started_anchor() {
        stop(pid);
    }
    answered.map_err(|_| anyhow::anyhow!("the workspaces hung"))?
}

/// Stops the anchor `pid` this test started (`SIGTERM`, as Slates' `Anchor::stop` asks; on Windows its tree ended) and
/// waits until its instance answers no more, at most [`STOPS_WITHIN`], saying how long it took; one still answering then
/// is said loudly and ended outright (`SIGKILL`: its daemon follows its parent's end).
fn stop(pid: u32) {
    let signal = |how: &str| {
        let args: Vec<String> = if cfg!(windows) { vec!["/PID".to_owned(), pid.to_string(), "/T".to_owned(), "/F".to_owned()] } else { vec![how.to_owned(), pid.to_string()] };
        if let Err(e) = Command::new(if cfg!(windows) { "taskkill" } else { "kill" }).args(&args).output() {
            eprintln!("Slates' anchor (pid {pid}) couldn't be signalled {how}: {e}");
        }
    };
    signal("-TERM");
    let began = std::time::Instant::now();
    while slates_api::Instance::attach(&super::slates::instance()).is_ok() {
        if began.elapsed() >= STOPS_WITHIN {
            eprintln!("SLOW STOP: Slates' anchor (pid {pid}) still served its instance {STOPS_WITHIN:?} after SIGTERM: ended outright");
            signal("-KILL");
            return;
        }
        std::thread::sleep(LOOK_EVERY);
    }
    eprintln!("Slates' anchor (pid {pid}) stopped its instance {:?} after it was asked", began.elapsed());
}

/// The file at `path` of `volume` written with `bytes`, or removed with none, through Slates' own file verbs under a write
/// attachment of the test's: what an agent's tools do in its mount.
async fn agent_writes(volume: VolumeId, path: &'static str, bytes: Option<&'static [u8]>) -> anyhow::Result<()> {
    let (wrote, detached) = call("write", move |i| {
        let attachment = i.attach_volume(volume, Intent::Write)?.attachment;
        let wrote = match bytes {
            Some(bytes) => i.client().fs_write((volume, attachment), path, bytes, 0o644).map(|_| ()),
            None => i.client().fs_remove((volume, attachment), path),
        };
        Ok((wrote.map_err(|e| e.to_string()), i.detach(attachment).map_err(|e| e.to_string())))
    })
    .await?;
    wrote.map_err(|e| anyhow::anyhow!("{path}: {e}"))?;
    detached.map_err(|e| anyhow::anyhow!("its detach: {e}"))
}

/// Whether Slates holds `volume` no more (gone, or its destroy's slices still running).
async fn gone(volume: &str) -> anyhow::Result<bool> {
    let id = super::slates::volume_id(volume)?;
    Ok(matches!(call("status", move |i| Ok(i.status(id))).await?, Err(slates_api::Error::NotFound(_) | slates_api::Error::Unusable(slates_api::Unusable::Destroying))))
}

/// Workspace `name` over `top` as `make` provisions it on a fleet runner, with no mount.
async fn workspace(name: &str, top: &str) -> anyhow::Result<Made> {
    let (id, base) = super::slates::provision(name, Path::new(top), true).await?;
    anyhow::ensure!(base.is_some(), "a fleet runner's workspace {name} made no captured base");
    Ok(Made { volume: slates_api::volume_id_text(id), name: name.to_owned(), mount: String::new(), base })
}

/// A review's changes as `path=bytes`, `path gone` or as Debug, in order.
fn shown(changes: &[Change]) -> Vec<String> {
    let mut out: Vec<String> = changes
        .iter()
        .map(|c| match c {
            Change::File { path, bytes, .. } => format!("{path}={}", String::from_utf8_lossy(bytes).trim()),
            Change::Gone { path } => format!("{path} gone"),
            other => format!("{other:?}"),
        })
        .collect();
    out.sort();
    out
}

async fn scenario() -> anyhow::Result<()> {
    let work = tempfile::tempdir()?;
    let top = project(work.path())?;
    let folder = Path::new(&top);
    super::slates::ensure_up().await?;

    // A new workspace is a captured clone: plain, reading the folder as captured.
    let a = workspace("kiln-k5-a", &top).await?;
    let a_id = super::slates::volume_id(&a.volume)?;
    let status = call("status", move |i| i.status(a_id)).await?;
    anyhow::ensure!(status.base.is_none(), "a captured clone has a base plane, so a move refuses it not_plain: {:?}", status.base);
    let read = call("read", move |i| i.read(a_id, "/src/a.rs", slates_api::ReadAt::Head)).await?;
    anyhow::ensure!(read == b"a\n", "the clone reads {:?}, not the folder as captured", String::from_utf8_lossy(&read));
    // The control: an overlay has its base plane.
    let (overlay, none) = super::slates::provision("kiln-k5-overlay", folder, false).await?;
    anyhow::ensure!(none.is_none(), "an overlay named a base");
    let overlay_base = call("status", move |i| i.status(overlay)).await?.base;
    call("volume destroy", move |i| i.destroy(overlay)).await?;
    anyhow::ensure!(overlay_base.is_some(), "an overlay reported no base plane: the plain check above proves nothing");
    // A second agent over the folder unchanged shares its capture.
    let b = workspace("kiln-k5-b", &top).await?;
    anyhow::ensure!(b.base == a.base, "the folder unchanged, a second workspace captured it again: {:?} then {:?}", a.base, b.base);

    // The agent's changes: one file rewritten, one removed, one made; then an outsider's edit of a file it didn't touch.
    agent_writes(a_id, "/src/a.rs", Some(b"agent")).await?;
    agent_writes(a_id, "/src/b.rs", None).await?;
    agent_writes(a_id, "/src/c.rs", Some(b"new")).await?;
    std::fs::write(folder.join("README.md"), "outsider\n")?;

    // Its drift is the folder's change since the capture, read from its base.
    let drift = super::slates::drift(&a).await?;
    anyhow::ensure!(drift.iter().any(|d| d.starts_with("/README.md")), "the outsider's edit isn't the workspace's drift: {drift:?}");
    // The folder changed, so the next agent's capture is new, and reads the edit.
    let c = workspace("kiln-k5-c", &top).await?;
    anyhow::ensure!(c.base != a.base, "the folder changed, and the next workspace shares the old capture: {:?}", c.base);
    let c_id = super::slates::volume_id(&c.volume)?;
    let readme = call("read", move |i| i.read(c_id, "/README.md", slates_api::ReadAt::Head)).await?;
    anyhow::ensure!(readme == b"outsider\n", "the new capture reads {:?}", String::from_utf8_lossy(&readme));

    // Its review is the agent's changes since its capture alone.
    let changes = super::review::changes(&a, &top).await?;
    anyhow::ensure!(shown(&changes) == ["src/a.rs=agent", "src/b.rs gone", "src/c.rs=new"], "its review: {:?}", shown(&changes));

    // Its landing writes those changes alone, approved under kiln's launcher authority, and passes the outsider's edit.
    let (plan, _) = super::slates::plan_landing(&a, &top).await?;
    let manifest = plan.get("manifest").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("its landing names no manifest: {plan}"))?.to_owned();
    match super::slates::land(&a, &top, &manifest).await? {
        super::slates::Landed::Written(_) => {}
        super::slates::Landed::Changed(plan, _) => anyhow::bail!("its landing changed as it was written: {plan}"),
    }
    let on_disk = |path: &str| std::fs::read_to_string(folder.join(path)).ok();
    anyhow::ensure!(on_disk("src/a.rs").as_deref() == Some("agent"), "src/a.rs: {:?}", on_disk("src/a.rs"));
    anyhow::ensure!(on_disk("src/b.rs").is_none(), "the agent's removal wasn't written: {:?}", on_disk("src/b.rs"));
    anyhow::ensure!(on_disk("src/c.rs").as_deref() == Some("new"), "src/c.rs: {:?}", on_disk("src/c.rs"));
    anyhow::ensure!(on_disk("README.md").as_deref() == Some("outsider\n"), "the outsider's edit wasn't passed: {:?}", on_disk("README.md"));

    // A base goes with the last workspace that's a clone of it.
    let (first, newer) = (a.base.clone().map(|b| b.volume).unwrap_or_default(), c.base.clone().map(|b| b.volume).unwrap_or_default());
    super::slates::remove(&a, Duration::ZERO).await?;
    anyhow::ensure!(!gone(&first).await?, "a base went while another workspace is a clone of it");
    super::slates::remove(&b, Duration::ZERO).await?;
    anyhow::ensure!(gone(&first).await?, "a base stayed after its last clone went");
    super::slates::remove(&c, Duration::ZERO).await?;
    anyhow::ensure!(gone(&newer).await?, "the folder's newest base stayed after its last clone went");
    Ok(())
}
