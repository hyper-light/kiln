//! kiln is its own Slates executable (docs/agentic.md, W1; kiln goal 01a1ec34599974448569750ac79093b6): kiln starts a
//! Slates instance from its own binary in Slates' roles (`kiln slates-role --instance <name> anchor`, and the daemon
//! its anchor starts the same way), with no Slates binary anywhere, and drives it through Slates' library alone: a
//! volume laid over a folder, its plan for writing back, its drift, the go-ahead's authority, and its removal. Nothing
//! is mounted, so it runs on any Unix machine. The test owns the instance it starts and stops exactly its processes,
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
    let volume = library.create(&slates_api::overlay("kilnroles-work", folder.path(), ROOM)?)?;
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

    // The go-ahead's authority: the handoff the anchor kiln started printed (macOS, Windows), which grants exactly the
    // plan's manifest and lands it; Linux's is a descriptor only the anchor's children hold (Slates' GAP-A9-10).
    if cfg!(target_os = "linux") {
        let refused = slates::can_take_grants().err().map(|e| format!("{e:#}")).unwrap_or_default();
        assert!(refused.contains("GAP-A9-10"), "{refused}");
    } else {
        slates::can_take_grants().map_err(|e| anyhow::anyhow!("{e:#}\nthe anchor said:\n{}", log_of(state.path())))?;
        let log = std::fs::read_to_string(state.path().join("slates-anchor.log"))?;
        let exports = log.lines().rev().find_map(|l| l.split_once("issuer surface: export ").map(|(_, rest)| rest.to_owned())).unwrap_or_default();
        let handoff: Vec<(String, String)> = exports.split_whitespace().filter_map(|kv| kv.split_once('=')).map(|(k, v)| (k.to_owned(), v.to_owned())).collect();
        let issuer = slates_api::Issuer::from_handoff(&handoff)?;
        let request = slates_api::LandingRequest {
            volume,
            snapshot: None,
            target: folder.text(),
            filter: slates_api::Filter { include: Vec::new(), exclude: slates::EXCLUDES.iter().map(|p| (*p).to_owned()).collect() },
        };
        let mut approved = [0u8; 32];
        for (i, byte) in approved.iter_mut().enumerate() {
            *byte = u8::from_str_radix(manifest.get(i * 2..i * 2 + 2).unwrap_or_default(), 16)?;
        }
        let landed = library.land_approved(&issuer, &request, &approved, (slates_api::GrantScope::Once, slates_api::GRANT_TERM_NS))?;
        assert!(matches!(&landed, slates_api::Approved::Landed(outcome) if outcome.state == "done"), "{landed:?}");
        assert_eq!(std::fs::read(folder.path().join("a.txt"))?, b"a2\n");
        assert_eq!(std::fs::read(folder.path().join("n.txt"))?, b"n\n");
        assert_eq!(std::fs::read(folder.path().join(".git/HEAD"))?, b"ref: refs/heads/main\n", "git's own files left out");
    }

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
