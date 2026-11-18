//! kiln is its own Slates executable (docs/agentic.md, W1; kiln goal 01a1ec34599974448569750ac79093b6): kiln starts a
//! Slates instance from its own binary in Slates' roles (`kiln slates-role --instance <name> anchor`, and the daemon
//! its anchor starts the same way), with no Slates binary anywhere, and drives it through Slates' library alone, in its
//! typed contract: a volume laid over a folder, its plan for writing back, its drift, its write policy as kiln (its
//! launcher) sets it under its own secret, the launcher's approval, and its removal. Nothing is mounted, so it runs on
//! any Unix machine. The test owns the instance it starts and stops exactly its processes,
//! by the pid kiln kept of the anchor it started (its process group), never by a pattern.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use kiln::workspace::slates;

/// How long the anchor has to stop its daemon and itself once asked, before its process group is killed.
const STOP_GRACE: Duration = Duration::from_secs(10);
/// How long the instance's processes have to go once their anchor is stopped (a daemon whose anchor is gone ends on
/// its own as well).
const GONE_WITHIN: Duration = Duration::from_secs(30);
/// How often the process table is looked at while waited on.
const LOOK_EVERY: Duration = Duration::from_millis(100);
/// The most the test's volume holds: far more than its two small files.
const ROOM: u64 = 64 << 20;
/// A file's mode as the test writes it in the volume.
const MODE: u32 = 0o644;

/// This machine's processes whose arguments name `instance` (`--instance <instance>`): each one's pid, executable and
/// arguments.
fn processes_of(instance: &str) -> Vec<(u32, String, Vec<String>)> {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
    let mut sys = System::new();
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing().with_cmd(UpdateKind::Always).with_exe(UpdateKind::Always));
    sys.processes()
        .iter()
        .filter_map(|(pid, p)| {
            let args: Vec<String> = p.cmd().iter().map(|a| a.to_string_lossy().into_owned()).collect();
            let named = args.windows(2).any(|w| matches!(w, [flag, name] if flag == "--instance" && name == instance));
            named.then(|| (pid.as_u32(), p.exe().map(|e| e.to_string_lossy().into_owned()).unwrap_or_default(), args))
        })
        .collect()
}

/// Stops the instance the test started, whatever happened: its anchor's process group.
struct Started;

impl Drop for Started {
    fn drop(&mut self) {
        if let Some(anchor) = slates::started_anchor() {
            kiln::os::stop_processes(&[anchor], None, STOP_GRACE);
        }
    }
}

/// A folder of the test's own in the build's scratch space (never `/tmp`, a RAM disk on Linux machines, which a
/// landing's durability would not meet), canonical, removed at the end.
struct Folder(PathBuf);

impl Folder {
    fn new(what: &str) -> anyhow::Result<Folder> {
        let made = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("kiln-slates-roles-{}-{what}", std::process::id()));
        std::fs::create_dir_all(&made)?;
        Ok(Folder(std::fs::canonicalize(&made)?))
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn text(&self) -> String {
        self.0.to_string_lossy().into_owned()
    }
}

impl Drop for Folder {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// What the anchor said, for a failure's message.
fn log_of(state: &Path) -> String {
    std::fs::read_to_string(state.join("slates-anchor.log")).unwrap_or_default().lines().rev().take(40).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n")
}

#[test]
fn w1_kiln_runs_its_slates_instance_as_its_own_executable_and_drives_it_through_the_library() -> anyhow::Result<()> {
    let kiln_exe = env!("CARGO_BIN_EXE_kiln");
    let instance = format!("kilnroles{}", std::process::id());
    let state = Folder::new("state")?;
    slates::init(state.path());
    anyhow::ensure!(slates::set_override(slates::Override { instance: instance.clone(), program: Some(kiln_exe.into()), quick: true }), "another test pointed workspaces elsewhere");
    let stop = Started;
    kiln::rt::block_on(slates::ensure_up()).map_err(|e| anyhow::anyhow!("{e:#}\nthe anchor said:\n{}", log_of(state.path())))?;
    anyhow::ensure!(slates_api::Instance::attach(&instance).is_ok(), "the instance doesn't answer\nthe anchor said:\n{}", log_of(state.path()));
    let facts = kiln::rt::block_on(async { anyhow::Ok(slates::facts().await) })?;
    assert!(facts.slates.as_deref().is_some_and(|s| s.starts_with("slates ")), "{facts:?}");

    // Its processes are kiln's own executable in Slates' roles: the anchor kiln started, and the daemon the anchor
    // started from the same executable and word. No `slates` program runs for it.
    let anchor = slates::started_anchor().ok_or_else(|| anyhow::anyhow!("kiln kept no anchor's pid"))?;
    let processes = processes_of(&instance);
    let role = |pid: Option<u32>, word: &str| processes.iter().find(|(p, _, args)| pid.is_none_or(|want| *p == want) && args.iter().any(|a| a == slates::ROLE_ARG) && args.iter().any(|a| a == word));
    let (_, exe, args) = role(Some(anchor), "anchor").ok_or_else(|| anyhow::anyhow!("the anchor {anchor} isn't kiln's in Slates' role: {processes:?}"))?;
    assert_eq!(args.get(1).map(String::as_str), Some(slates::ROLE_ARG), "{args:?}");
    let same = |exe: &str| Path::new(exe).canonicalize().ok() == Path::new(kiln_exe).canonicalize().ok();
    assert!(same(exe), "the anchor runs {exe}, not kiln ({kiln_exe})");
    let (daemon, exe, _) = role(None, "daemon").ok_or_else(|| anyhow::anyhow!("no daemon in Slates' role: {processes:?}"))?;
    assert!(same(exe), "the daemon {daemon} runs {exe}, not kiln ({kiln_exe})");
    for (pid, exe, _) in &processes {
        assert!(Path::new(exe).file_stem().is_none_or(|s| s != "slates"), "process {pid} runs a Slates binary: {exe}");
    }
    // A second start finds it up and starts nothing.
    kiln::rt::block_on(slates::ensure_up())?;
    assert_eq!(processes_of(&instance).len(), processes.len(), "a second anchor was started");

    // A volume laid over a folder (made here through the library, unmounted), one file of the folder changed in it and
    // one new: kiln's own calls see it as Slates does.
    let folder = Folder::new("folder")?;
    std::fs::write(folder.path().join("a.txt"), b"a\n")?;
    std::fs::create_dir_all(folder.path().join(".git"))?;
    std::fs::write(folder.path().join(".git/HEAD"), b"ref: refs/heads/main\n")?;
    let mut library = slates_api::Instance::attach(&instance)?;
    let volume = library.provision(&slates_api::VolumeSpec::overlay("kilnroles-work", folder.path(), ROOM))?.volume();
    let attached = library.attach_volume(volume, slates_api::Intent::Write)?;
    library.client().fs_write((volume, attached.attachment), "/a.txt", b"a2\n", MODE)?;
    library.client().fs_write((volume, attached.attachment), "/n.txt", b"n\n", MODE)?;
    library.client().fs_write((volume, attached.attachment), "/.git/HEAD", b"ref: refs/heads/agent\n", MODE)?;
    library.detach(attached.attachment)?;
    let made = slates::Made { volume: slates_api::volume_id_text(volume), name: "kilnroles-work".into(), mount: state.path().join("never-mounted").to_string_lossy().into_owned() };
    let drifted = kiln::rt::block_on({
        let made = made.clone();
        async move { slates::drift(&made).await }
    })?;
    assert_eq!(drifted, Vec::<String>::new(), "nothing changed on disk under it");
    let plan = kiln::rt::block_on({
        let (made, over) = (made.clone(), folder.text());
        async move { slates::plan_landing(&made, &over).await }
    })?;
    let at = |pointer: &str| plan.pointer(pointer).cloned().unwrap_or_default();
    assert_eq!(at("/grant_required"), serde_json::json!(true), "{plan}");
    let manifest = at("/manifest").as_str().unwrap_or_default().to_owned();
    assert_eq!(manifest.len(), 64, "{plan}");
    let by_action = at("/summary/by_action").as_array().cloned().unwrap_or_default();
    let count = |action: &str| by_action.iter().filter(|a| a.get("action").and_then(serde_json::Value::as_str) == Some(action)).filter_map(|a| a.get("count").and_then(serde_json::Value::as_u64)).sum::<u64>();
    assert_eq!((count("create"), count("replace")), (1, 1), "the new file and the changed one, .git left out: {plan}");
    assert_eq!(std::fs::read(folder.path().join("a.txt"))?, b"a\n", "the folder is as it was");

    // Unset, a volume's landing asks: Slates' typed approval request, the plan's manifest, nothing written.
    let over = folder.text();
    let by_policy = |made: &slates::Made| {
        let (made, over) = (made.clone(), over.clone());
        kiln::rt::block_on(async move { slates::land_by_policy(&made, &over).await })
    };
    let held = by_policy(&made)?;
    let slates_api::LandOutcome::ApprovalRequired(approval) = held else { anyhow::bail!("an unset volume's landing didn't ask: {held:?}") };
    assert_eq!(approval.manifest_text(), manifest);
    assert_eq!(std::fs::read(folder.path().join("a.txt"))?, b"a\n", "asking wrote nothing");

    // kiln's own authority, the launcher secret it handed the anchor it started (kept 0600 in its state), approves
    // exactly the plan's manifest; another manifest approves nothing.
    slates::can_take_grants()?;
    let approve = |manifest: String| {
        let (made, over) = (made.clone(), over.clone());
        kiln::rt::block_on(async move { slates::approve(&made, &over, &manifest).await })
    };
    let other = approve("00".repeat(32))?;
    assert!(matches!(other, slates_api::LandOutcome::ApprovalRequired(_)), "another manifest's approval: {other:?}");
    assert_eq!(std::fs::read(folder.path().join("a.txt"))?, b"a\n", "an approval of another manifest wrote nothing");
    let landed = approve(manifest)?;
    assert!(matches!(&landed, slates_api::LandOutcome::Written(report) if report.state == slates_api::LandingState::Done), "{landed:?}");
    assert_eq!(std::fs::read(folder.path().join("a.txt"))?, b"a2\n");
    assert_eq!(std::fs::read(folder.path().join("n.txt"))?, b"n\n");
    assert_eq!(std::fs::read(folder.path().join(".git/HEAD"))?, b"ref: refs/heads/main\n", "git's own files left out");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(std::fs::metadata(state.path().join("slates-launcher.key"))?.permissions().mode() & 0o777, 0o600, "kiln's secret kept private");
    }

    // The policy kiln sets as the launcher: bypass writes the next change at once, forbid refuses it, typed.
    let set = |policy: slates_api::WritePolicy| {
        let made = made.clone();
        kiln::rt::block_on(async move { slates::set_policy(&made, policy).await })
    };
    let write = |path: &str, bytes: &[u8]| -> anyhow::Result<()> {
        let mut library = slates_api::Instance::attach(&instance)?;
        let attached = library.attach_volume(volume, slates_api::Intent::Write)?;
        library.client().fs_write((volume, attached.attachment), path, bytes, MODE)?;
        library.detach(attached.attachment)?;
        Ok(())
    };
    set(slates_api::WritePolicy { land: slates_api::WriteMode::Bypass, ..slates_api::WritePolicy::UNSET })?;
    write("/b.txt", b"b\n")?;
    let written = by_policy(&made)?;
    assert!(matches!(written, slates_api::LandOutcome::Written(_)), "a bypass volume's landing asked: {written:?}");
    assert_eq!(std::fs::read(folder.path().join("b.txt"))?, b"b\n");
    set(slates_api::WritePolicy { land: slates_api::WriteMode::Forbid, ..slates_api::WritePolicy::UNSET })?;
    write("/c.txt", b"c\n")?;
    let refused = by_policy(&made)?;
    assert!(matches!(refused, slates_api::LandOutcome::Refused(slates_api::PolicyRefusal { reason: slates_api::PolicyReason::Forbidden, .. })), "{refused:?}");
    assert!(!folder.path().join("c.txt").exists(), "a forbidden landing wrote nothing");

    // Removed: its volume is gone from the instance.
    kiln::rt::block_on({
        let made = made.clone();
        async move { slates::remove(&made).await }
    })?;
    assert!(library.list()?.iter().all(|v| v.id != volume), "the volume outlived its removal");
    drop(library);

    // Stopped by its anchor's process group, every process of the instance goes.
    drop(stop);
    let until = Instant::now() + GONE_WITHIN;
    while !processes_of(&instance).is_empty() {
        anyhow::ensure!(Instant::now() < until, "the instance's processes outlived their anchor: {:?}", processes_of(&instance));
        std::thread::sleep(LOOK_EVERY);
    }
    Ok(())
}
