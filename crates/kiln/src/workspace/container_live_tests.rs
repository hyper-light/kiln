//! An agent's container started on its workspace's Slates volume, live (docs/agentic.md W9): a Slates instance of the
//! test's own run by a kiln executable in Slates' roles (`KILN_TEST_KILN`), a volume made and mounted over a scratch
//! repository (W3), the machine's own container engine found and judged by Slates, and a harness's stand-in run in a
//! container of `KILN_TEST_CONTAINER_IMAGE` through `sandbox::container`: it reads a file of the folder through the engine's
//! own NFS volume of Slates' export of the workspace, writes one, and that file is on the volume's mount on the host as it ends; the container, its engine volume and the
//! export go, the volume after them. Skipped, saying why, on a machine that can't mount a volume, has no engine Slates holds evidence for, or
//! wasn't given an executable and an image.
//!
//! The test owns the instance it starts and stops its anchor once its volume's mount is gone, as the bare-metal test
//! does (`bare_metal_tests`).

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use kiln_seams::sandbox::{Boundary, Egress, Exit, Launch, Limits, Output, Sandbox as _};

/// How long the whole test may take: an image pulled, a volume made and mounted, a container started and removed.
const WITHIN: Duration = Duration::from_secs(900);

fn git(dir: &Path, args: &[&str]) -> anyhow::Result<()> {
    let out = Command::new("git").args(args).current_dir(dir).env("GIT_CONFIG_GLOBAL", "/dev/null").env("GIT_CONFIG_NOSYSTEM", "1").env("GIT_TERMINAL_PROMPT", "0").output()?;
    anyhow::ensure!(out.status.success(), "git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr));
    Ok(())
}

/// W9, live. Do: start a shell in a container of the given image on a workspace mounted over a repository whose
/// `seed.txt` says `seeded`, which prints it and writes `from-container.txt`, then let the run end. Expect its output to
/// carry `seeded`, its end code 0, `from-container.txt` on the mount on the host, and the container gone after.
#[test]
fn w9_live_an_agents_container_works_on_its_workspace_volume_through_its_export() -> anyhow::Result<()> {
    let (Some(program), Some(image)) = (std::env::var_os("KILN_TEST_KILN"), std::env::var("KILN_TEST_CONTAINER_IMAGE").ok()) else {
        eprintln!("skipped: KILN_TEST_KILN names no kiln executable, or KILN_TEST_CONTAINER_IMAGE no image");
        return Ok(());
    };
    if let Err(fault) = slates_api::mount::available() {
        eprintln!("skipped: {fault}");
        return Ok(());
    }
    let Some(engine) = crate::runner::docker::find() else {
        eprintln!("skipped: no container engine answers here");
        return Ok(());
    };
    let judged = match slates_api::container_engine::engine_profile(&engine) {
        Ok(judged) => judged,
        Err(fault) => {
            eprintln!("skipped: Slates refused this machine's engine ({engine}): {fault}");
            return Ok(());
        }
    };
    // A rootless engine can't mount the workspace's export itself (`container::facts`).
    if judged.profile.rootless {
        eprintln!("skipped: {engine} runs rootless ({:?}, {}), and a rootless engine can't mount the workspace's export itself", judged.profile.engine, judged.profile.server_version);
        return Ok(());
    }
    let instance = format!("kilntestc{}", std::process::id());
    let o = super::slates::Override { instance, program: Some(program.into()), quick: true };
    anyhow::ensure!(super::slates::set_override(o), "another test pointed workspaces elsewhere");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(crate::rt::block_on(run(engine, judged, image)));
    });
    match rx.recv_timeout(WITHIN) {
        Ok(r) => r,
        Err(_) => anyhow::bail!("the container's run hung (its Slates instance is left running)"),
    }
}

async fn run(engine: String, judged: slates_api::container_engine::EngineProfile, image: String) -> anyhow::Result<()> {
    // Where this process keeps its workspaces, as a runner's state is (`slates::init`): under the person's home, inside
    // what a Mac's Docker Desktop shares with its VM (`/Users`), so the engine reaches the mount there.
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from).ok_or_else(|| anyhow::anyhow!("no HOME"))?;
    let cache = home.join(".cache");
    std::fs::create_dir_all(&cache)?;
    let state = tempfile::tempdir_in(&cache)?;
    super::slates::init(state.path());
    let repo = tempfile::tempdir_in(&cache)?;
    std::fs::write(repo.path().join("seed.txt"), "seeded\n")?;
    git(repo.path(), &["init", "-q", "-b", "main"])?;
    git(repo.path(), &["-c", "user.name=t", "-c", "user.email=t@example.com", "-c", "commit.gpgsign=false", "add", "."])?;
    git(repo.path(), &["-c", "user.name=t", "-c", "user.email=t@example.com", "-c", "commit.gpgsign=false", "commit", "-qm", "seed"])?;
    let made = super::slates::make(&format!("kiln-livec{}", std::process::id()), repo.path()).await?;
    let ran = drive(&engine, judged, &image, &made).await;
    let gone = super::slates::remove_after_container(&made).await;
    if let Some(pid) = super::slates::started_anchor() {
        if gone.is_ok() {
            let _ = Command::new("kill").args(["-TERM", &pid.to_string()]).output();
        } else {
            eprintln!("LEFT RUNNING: Slates' anchor (pid {pid}): its volume's mount didn't go");
        }
    }
    ran?;
    gone
}

async fn drive(engine: &str, judged: slates_api::container_engine::EngineProfile, image: &str, made: &super::slates::Made) -> anyhow::Result<()> {
    let sandbox = crate::sandbox::container::ContainerSandbox::new(engine.to_owned(), judged, made.clone());
    let launch = Launch {
        folder: Some(made.mount.clone()),
        limits: Limits::NONE,
        egress: Egress::Any,
        boundary: Boundary::Container { image: image.to_owned() },
        ..Launch::of("sh", &["-c", "cat seed.txt && echo made-here > from-container.txt"])
    };
    let run = sandbox.launch(launch).await.map_err(|refused| anyhow::anyhow!("its container: {refused}"))?;
    let id = run.id().unwrap_or_default();
    let (mut out, mut err, mut end) = (Vec::new(), Vec::new(), None);
    while let Some(said) = run.next().await {
        match said {
            Output::Stdout(b) => out.extend(b),
            Output::Stderr(b) => err.extend(b),
            Output::Exited(how) => end = Some(how),
        }
    }
    let (out, err) = (String::from_utf8_lossy(&out), String::from_utf8_lossy(&err));
    anyhow::ensure!(end == Some(Exit::Code(0)), "its run ended {end:?}: {out} {err}");
    anyhow::ensure!(out.contains("seeded"), "the folder's file wasn't read through the export: {out:?} {err:?}");
    let written = std::fs::read_to_string(Path::new(&made.mount).join("from-container.txt"))?;
    anyhow::ensure!(written.trim() == "made-here", "what the container wrote isn't on the mount: {written:?}");
    // Its end released the container: the engine has no container by its id.
    let left = Command::new(engine).args(["container", "inspect", &id]).output()?;
    anyhow::ensure!(!left.status.success(), "container {id} is still there after its run ended");
    eprintln!("live: {engine} ran {image} on {} on the engine's NFS volume of Slates' export; the container wrote to the volume, then went", made.mount);
    Ok(())
}
