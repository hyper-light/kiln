//! An isolated agent's workspace on Slates with no mount (kiln goal 01a1ec34599974448f6b7663ddb07001): kiln makes the
//! volume with nothing mounted (`workspace::slates::make_unmounted`), Slates serves it on a stream kiln hands its
//! microVM through Hex (`sandbox::served::SlatesVolumes`, Hex's `hex_api::serve::Servers`; Shards' D286), and its review
//! is built from the volume's own changes (`workspace::review`, Slates' A-287) into a commit beside the folder
//! (`workspace::git::commit_changes`), as git would commit the volume's own working tree. Driven through kiln's calls
//! against a Slates instance kiln starts from its own executable, as `slates_roles.rs` starts one; nothing is mounted,
//! so it runs on any Unix machine. The test owns the instance it starts and stops exactly its processes, by the pid
//! kiln kept of the anchor it started, never by a pattern.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use hex_api::serve::Servers as _;
use kiln::workspace::{git, review, slates};

/// How long the anchor has to stop its daemon and itself once asked, before its process group is killed.
const STOP_GRACE: Duration = Duration::from_secs(10);
/// How long the instance's processes have to go once their anchor is stopped.
const GONE_WITHIN: Duration = Duration::from_secs(30);
/// How often the process table is looked at while waited on.
const LOOK_EVERY: Duration = Duration::from_millis(100);
/// A file's mode as the test writes it in the volume: a plain file, and one the agent made executable.
const MODE: u32 = 0o644;
const EXECUTABLE: u32 = 0o755;

/// This machine's processes whose arguments name `instance` (`--instance <instance>`), by pid.
fn processes_of(instance: &str) -> Vec<u32> {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
    let mut sys = System::new();
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing().with_cmd(UpdateKind::Always));
    sys.processes()
        .iter()
        .filter(|(_, p)| {
            let args: Vec<String> = p.cmd().iter().map(|a| a.to_string_lossy().into_owned()).collect();
            args.windows(2).any(|w| matches!(w, [flag, name] if flag == "--instance" && name == instance))
        })
        .map(|(pid, _)| pid.as_u32())
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

/// A folder of the test's own in the build's scratch space (never `/tmp`), canonical, removed at the end.
struct Folder(PathBuf);

impl Folder {
    fn new(what: &str) -> anyhow::Result<Folder> {
        let made = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("kiln-slates-served-{}-{what}", std::process::id()));
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

/// `git` in `dir`, with no person's configuration, its answer's text.
fn git_in(dir: &Path, args: &[&str]) -> anyhow::Result<String> {
    let out = Command::new("git").args(args).current_dir(dir).env("GIT_CONFIG_GLOBAL", "/dev/null").env("GIT_CONFIG_NOSYSTEM", "1").output()?;
    anyhow::ensure!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// Kiln goal 01a1ec34599974448f6b7663ddb07001. Do make a workspace over a git project with no mount, and write in its
/// volume as an agent would (a tracked file changed, a new file, a new executable, a `.gitignore` the agent extended,
/// a file that extension ignores, build output the project's own `.gitignore` ignores). Expect: nothing mounted for it;
/// its review the commit of exactly the tracked change, the new file, the executable with its bit and the extended
/// `.gitignore`, on the folder's last commit, the folder's branch, index and files as they were; a landing refused as
/// writing the two files git ignores; Slates' stream of the volume a socket, and a volume it doesn't serve refused
/// `Invalid` naming it; and the workspace gone with its volume.
#[test]
fn g265_a_volume_with_no_mount_is_served_on_a_stream_and_reviewed_from_its_own_changes() -> anyhow::Result<()> {
    let kiln_exe = env!("CARGO_BIN_EXE_kiln");
    let instance = format!("kilnserved{}", std::process::id());
    let state = Folder::new("state")?;
    slates::init(state.path());
    anyhow::ensure!(slates::set_override(slates::Override { instance: instance.clone(), program: Some(kiln_exe.into()), quick: true }), "another test pointed workspaces elsewhere");
    let stop = Started;
    kiln::rt::block_on(slates::ensure_up()).map_err(|e| anyhow::anyhow!("{e:#}\nthe anchor said:\n{}", log_of(state.path())))?;

    // A project: one tracked file and its own .gitignore, committed.
    let project = Folder::new("project")?;
    git_in(project.path(), &["init", "-q", "-b", "main"])?;
    git_in(project.path(), &["config", "user.name", "Ada"])?;
    git_in(project.path(), &["config", "user.email", "ada@example.com"])?;
    git_in(project.path(), &["config", "commit.gpgsign", "false"])?;
    std::fs::write(project.path().join("a.txt"), b"a\n")?;
    std::fs::write(project.path().join(".gitignore"), b"target/\n")?;
    git_in(project.path(), &["add", "."])?;
    git_in(project.path(), &["commit", "-qm", "init"])?;
    let head = git_in(project.path(), &["rev-parse", "HEAD"])?;
    let over = project.text();

    let made = kiln::rt::block_on({
        let over = project.path().to_path_buf();
        async move { slates::make_unmounted(&format!("kilnserved-{}", std::process::id()), &over).await }
    })?;
    anyhow::ensure!(made.mount.is_empty(), "a workspace made with no mount names one: {made:?}");
    let volume = slates::volume_id(&made.volume)?;

    // What the agent writes in its volume, through Slates' own file verbs (its microVM's way in is the stream).
    let mut library = slates_api::Instance::attach(&instance)?;
    let attached = library.attach_volume(volume, slates_api::Intent::Write)?;
    let at = (volume, attached.attachment);
    library.client().fs_write(at, "/a.txt", b"a, the agent's\n", MODE)?;
    library.client().fs_write(at, "/n.txt", b"new\n", MODE)?;
    library.client().fs_write(at, "/run.sh", b"#!/bin/sh\necho run\n", EXECUTABLE)?;
    library.client().fs_write(at, "/.gitignore", b"target/\nsecret.env\n", MODE)?;
    library.client().fs_write(at, "/secret.env", b"KEY=1\n", MODE)?;
    library.client().fs_mkdir(at, "/target", EXECUTABLE)?;
    library.client().fs_write(at, "/target/out.o", b"\x7fELF", MODE)?;
    library.detach(attached.attachment)?;

    // Its review: exactly what git would commit of the volume's own working tree.
    let changes = kiln::rt::block_on({
        let (made, over) = (made.clone(), over.clone());
        async move { review::changes(&made, &over).await }
    })?;
    let who = git::Ident { name: "Ada".into(), email: "ada@example.com".into() };
    let commit = kiln::rt::block_on({
        let (top, head) = (project.path().to_path_buf(), head.clone());
        async move { git::commit_changes(&top, Some(&head), &changes, "The agent's work", &who, None).await }
    })?
    .ok_or_else(|| anyhow::anyhow!("its review committed nothing"))?;
    assert_eq!(commit.parent.as_deref(), Some(head.as_str()));
    let mut files = commit.files.clone();
    files.sort();
    assert_eq!(files, [".gitignore", "a.txt", "n.txt", "run.sh"], "the agent's changes, none git ignores");
    let listed = git_in(project.path(), &["ls-tree", "-r", "--format=%(objectmode) %(path)", commit.sha.as_str()])?;
    assert!(listed.lines().any(|l| l == "100755 run.sh"), "the executable keeps its bit: {listed}");
    assert_eq!(git_in(project.path(), &["show", format!("{}:a.txt", commit.sha).as_str()])?, "a, the agent's");
    assert_eq!(git_in(project.path(), &["rev-parse", "HEAD"])?, head, "the folder's branch didn't move");
    assert_eq!(git_in(project.path(), &["status", "--porcelain"])?, "", "the folder's index and files are as they were");
    assert_eq!(std::fs::read(project.path().join("a.txt"))?, b"a\n", "nothing of the volume reached the folder");

    // A landing of it would write the two files git ignores: refused, as a mounted workspace's is.
    let plan = kiln::rt::block_on({
        let (made, over) = (made.clone(), over.clone());
        async move { slates::plan_landing(&made, &over).await }
    })?;
    let refused = kiln::rt::block_on({
        let (made, over, plan) = (made.clone(), over.clone(), plan.clone());
        async move { slates::writable(&made, &over, &plan).await }
    });
    assert!(refused.as_ref().is_err_and(|why| why.contains("2 files git ignores")), "{refused:?}");

    // Slates' stream of the volume, as Hex asks it of kiln's server for a box's boot: a socket, its microVM's device's
    // to frame FUSE on; a volume Slates serves none of is refused Invalid, naming it (Hex's HEX-SERVE-3).
    let share = |source: &str| hex_api::ServedMount { source: source.to_owned(), destination: "/work/project".to_owned(), read_only: false, members: Vec::new() };
    let id = hex_api::BoxId::new("kiln-served-test").map_err(|e| anyhow::anyhow!("{e}"))?;
    let stream = kiln::sandbox::served::SlatesVolumes.stream(&id, &share(&made.volume)).map_err(|e| anyhow::anyhow!("{e}"))?;
    {
        use std::os::unix::fs::FileTypeExt as _;
        assert!(std::fs::File::from(stream).metadata()?.file_type().is_socket(), "Slates' stream is no socket");
    }
    let unknown = "00000000000000000000000000000000";
    match kiln::sandbox::served::SlatesVolumes.stream(&id, &share(unknown)) {
        Err(hex_api::Refusal::Invalid { why, .. }) => assert!(why.contains(unknown), "{why}"),
        other => anyhow::bail!("a volume Slates serves none of: {:?}", other.map(|_| "a stream")),
    }

    // Removed: its volume is gone, and nothing of it was ever mounted.
    kiln::rt::block_on({
        let made = made.clone();
        async move { slates::remove(&made).await }
    })?;
    assert!(library.list()?.iter().all(|v| v.id != volume), "the volume outlived its removal");
    drop(library);

    drop(stop);
    let until = Instant::now() + GONE_WITHIN;
    while !processes_of(&instance).is_empty() {
        anyhow::ensure!(Instant::now() < until, "the instance's processes outlived their anchor: {:?}", processes_of(&instance));
        std::thread::sleep(LOOK_EVERY);
    }
    Ok(())
}
