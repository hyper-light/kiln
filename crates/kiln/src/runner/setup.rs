//! `kiln runner setup docker|firecracker|shards` (docs/ci.md W13-R10): the one-time privileged
//! steps an executor needs on this machine, for the person setting it up. Each step says what it
//! found and what it does; one that needs root runs `sudo` itself, after asking, never silently.
//! `--dry-run` says them all and does none.
//!
//! - `docker`: Docker or Podman answering for this user, or the `docker` group that lets it.
//! - `firecracker`: KVM (`/dev/kvm`, and the `kvm` group), then Firecracker and its jailer from
//!   their release, checked against its SHA-256, in kiln's own directory (no root). Its microVMs'
//!   networking comes with kiln's Firecracker executor (W13-R05), which isn't built yet.
//! - `shards`: shards installs as its own project says, and kiln finds it on `PATH` (W13-R06).

use std::io::{BufRead as _, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::Context as _;

#[derive(clap::Args, Debug)]
pub struct RunnerArgs {
    #[command(subcommand)]
    pub cmd: RunnerCmd,
}

#[derive(clap::Subcommand, Debug)]
pub enum RunnerCmd {
    /// The one-time privileged steps an executor needs on this machine.
    Setup(SetupArgs),
}

#[derive(clap::Args, Debug)]
pub struct SetupArgs {
    /// `docker`, `firecracker` or `shards`.
    #[arg(value_parser = ["docker", "firecracker", "shards"])]
    pub executor: String,
    /// Say each step, and do none of them.
    #[arg(long)]
    pub dry_run: bool,
    /// Take each step without asking first (`sudo` still asks for its password).
    #[arg(long, short = 'y')]
    pub yes: bool,
    /// Where Firecracker and its jailer go (`~/.kiln/bin`).
    #[arg(long)]
    pub bin_dir: Option<PathBuf>,
}

/// One step: what was found, and what's done about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Nothing to do.
    Ok(String),
    /// A command to run, as root when `root` (through `sudo`).
    Run { why: String, root: bool, program: String, args: Vec<String> },
    /// What a person does (install Docker, turn virtualization on); the rest waits on it.
    Do(String),
    /// Firecracker and its jailer, fetched into `to`.
    Fetch { to: PathBuf },
    /// A guest kernel for its microVMs, fetched to `to`.
    FetchKernel { to: PathBuf },
}

/// What this machine looks like, as the plan needs it.
#[derive(Debug, Clone, Default)]
pub struct Machine {
    pub user: String,
    pub linux: bool,
    /// Docker or Podman: its binary when there is one, and whether it answers for this user.
    pub engine: Option<(String, bool)>,
    /// `/dev/kvm`: whether this user may use it, when it's there.
    pub kvm: Option<bool>,
    pub groups: Vec<String>,
    /// Firecracker, where it is.
    pub firecracker: Option<PathBuf>,
    /// The guest kernel its microVMs boot, where it is.
    pub kernel: Option<PathBuf>,
    /// e2fsprogs' `mke2fs`, which makes the microVMs' disks.
    pub mkfs: Option<PathBuf>,
    /// shards, where it is.
    pub shards: Option<PathBuf>,
}

/// The steps `executor` needs on `m`, Firecracker going to `bin`.
pub fn plan(executor: &str, m: &Machine, bin: &Path) -> Vec<Step> {
    let mut out = Vec::new();
    match executor {
        "docker" => match &m.engine {
            None => out.push(Step::Do("There's no Docker or Podman here: install Docker Engine (https://docs.docker.com/engine/install/) or Podman, then run this again.".into())),
            Some((engine, true)) => out.push(Step::Ok(format!("{engine} answers for {}.", m.user))),
            Some((engine, false)) if engine.ends_with("podman") => {
                out.push(Step::Do(format!("{engine} doesn't answer for {}: `podman info` says why (rootless Podman needs subordinate ids, `/etc/subuid` and `/etc/subgid`).", m.user)));
            }
            Some((engine, false)) if m.groups.iter().any(|g| g == "docker") => {
                out.push(Step::Do(format!(
                    "{} is in the `docker` group but {engine} doesn't answer: is its daemon running (`sudo systemctl start docker`)? A new group takes after logging in again.",
                    m.user
                )));
            }
            Some((engine, false)) => {
                out.push(Step::Run {
                    why: format!("Let {} use {engine} without sudo: the `docker` group (it takes after logging in again)", m.user),
                    root: true,
                    program: "usermod".into(),
                    args: vec!["-aG".into(), "docker".into(), m.user.clone()],
                });
            }
        },
        "firecracker" => {
            if !m.linux {
                out.push(Step::Do("Firecracker runs on Linux with KVM, and this machine isn't Linux.".into()));
                return out;
            }
            match m.kvm {
                None => {
                    out.push(Step::Do("There's no /dev/kvm here: turn virtualization on (VT-x or AMD-V) in the machine's firmware, or nested virtualization on its host, then run this again.".into()));
                    return out;
                }
                Some(true) => out.push(Step::Ok(format!("KVM is {}'s to use.", m.user))),
                Some(false) if m.groups.iter().any(|g| g == "kvm") => out.push(Step::Do(format!("{} is in the `kvm` group already: log in again for it to take.", m.user))),
                Some(false) => out.push(Step::Run {
                    why: format!("Let {} use KVM: the `kvm` group (it takes after logging in again)", m.user),
                    root: true,
                    program: "usermod".into(),
                    args: vec!["-aG".into(), "kvm".into(), m.user.clone()],
                }),
            }
            match &m.firecracker {
                Some(p) => out.push(Step::Ok(format!("Firecracker is at {}.", p.display()))),
                None => out.push(Step::Fetch { to: bin.to_path_buf() }),
            }
            match &m.kernel {
                Some(p) => out.push(Step::Ok(format!("Its microVMs boot {}.", p.display()))),
                None => out.push(Step::FetchKernel { to: kernel_path() }),
            }
            match &m.mkfs {
                Some(p) => out.push(Step::Ok(format!("mke2fs is at {}.", p.display()))),
                None => out.push(Step::Do("There's no mke2fs here, which makes the microVMs' disks: install e2fsprogs (your distribution's `e2fsprogs` package).".into())),
            }
            out.push(Step::Ok("Its microVMs have no network device: their one way out is the leg's egress proxy here, over vsock (docs/ci.md W13-R05).".into()));
        }
        _ => match &m.shards {
            Some(p) => out.push(Step::Ok(format!("shards is at {}.", p.display()))),
            None => out.push(Step::Do("There's no shards here: install it as its own project says; kiln finds it on PATH (docs/ci.md W13-R06).".into())),
        },
    }
    out
}

/// A program on `PATH`, where it is.
fn on_path(program: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?).map(|d| d.join(program)).find(|p| p.is_file())
}

fn output(program: &str, args: &[&str]) -> Option<String> {
    Command::new(program).args(args).stderr(std::process::Stdio::null()).output().ok().filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
}

/// This machine, looked at.
pub fn look(bin: &Path) -> Machine {
    let engine = ["docker", "podman"].iter().find_map(|b| on_path(b)).map(|p| {
        let s = p.to_string_lossy().into_owned();
        let answers = output(&s, &["info", "--format", "{{.Host.Arch}}{{.ServerVersion}}"]).is_some();
        (s, answers)
    });
    let kvm = Path::new("/dev/kvm").exists().then(|| std::fs::OpenOptions::new().read(true).write(true).open("/dev/kvm").is_ok());
    let firecracker = Some(bin.join("firecracker")).filter(|p| p.is_file()).or_else(|| on_path("firecracker"));
    let kernel = std::env::var_os("KILN_FIRECRACKER_KERNEL").map(PathBuf::from).or_else(|| Some(kernel_path())).filter(|p| p.is_file());
    let mkfs = on_path("mke2fs").or_else(|| ["/usr/sbin/mke2fs", "/sbin/mke2fs"].into_iter().map(PathBuf::from).find(|p| p.is_file()));
    Machine {
        user: std::env::var("USER").ok().filter(|u| !u.is_empty()).or_else(|| output("id", &["-un"])).unwrap_or_default(),
        linux: cfg!(target_os = "linux"),
        engine,
        kvm,
        groups: output("id", &["-Gn"]).map(|g| g.split_whitespace().map(str::to_owned).collect()).unwrap_or_default(),
        firecracker,
        kernel,
        mkfs,
        shards: on_path("shards"),
    }
}

/// Where the microVMs' guest kernel goes, and where the executor looks for it.
fn kernel_path() -> PathBuf {
    crate::os::home_dir().join(".kiln").join("firecracker").join("vmlinux")
}

/// The guest kernel for this machine's architecture: one of Firecracker's own CI kernels (6.1, with
/// virtio block devices, vsock, ext4 and overlayfs built in), pinned by its SHA-256.
const KERNELS: &[(&str, &str, &str)] = &[
    ("x86_64", "firecracker-ci/v1.15/x86_64/vmlinux-6.1.155", "e20e46d0c36c55c0d1014eb20576171b3f3d922260d9f792017aeff53af3d4f2"),
    ("aarch64", "firecracker-ci/v1.15/aarch64/vmlinux-6.1.155", "e3544b10603acbf3db492cb52e000d22ba202cb4b63b9add027565683e11c591"),
];

/// The guest kernel, fetched to `to` and checked against its pinned SHA-256.
async fn fetch_kernel(to: &Path) -> anyhow::Result<String> {
    let arch = std::env::consts::ARCH;
    let (_, key, want) = KERNELS.iter().find(|(a, _, _)| *a == arch).with_context(|| format!("there's no guest kernel for {arch}"))?;
    let http = crate::http::client::Client::public()?;
    let url = format!("https://s3.amazonaws.com/spec.ccfc.min/{key}");
    let bytes = download(&http, &url).await?;
    let got = crate::util::sha256_hex(&bytes);
    anyhow::ensure!(got == *want, "{url}: SHA-256 {got}, where kiln expects {want}");
    let to = to.to_path_buf();
    crate::rt::blocking(move || -> anyhow::Result<()> {
        if let Some(dir) = to.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = to.with_extension("partial");
        std::fs::write(&tmp, &bytes)?;
        std::fs::rename(&tmp, &to)?;
        Ok(())
    })
    .await??;
    Ok(key.rsplit('/').next().unwrap_or(key).to_owned())
}

fn ask(question: &str) -> bool {
    print!("{question} [y/N] ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line).is_ok() && matches!(line.trim(), "y" | "Y" | "yes")
}

/// What `url` answers, whole (redirects followed).
async fn download(http: &crate::http::client::Client, url: &str) -> anyhow::Result<Vec<u8>> {
    let mut resp = crate::updates::get_following(http, url, Duration::from_secs(600)).await?;
    anyhow::ensure!(resp.is_success(), "{url}: HTTP {}", resp.status());
    let mut bytes = Vec::new();
    while let Some(c) = resp.chunk().await? {
        bytes.extend_from_slice(&c);
        anyhow::ensure!(bytes.len() < 1 << 30, "{url} is over 1 GiB");
    }
    Ok(bytes)
}

/// Firecracker and its jailer from their latest release, checked against its SHA-256, into `to`.
async fn fetch_firecracker(to: &Path) -> anyhow::Result<String> {
    let arch = match std::env::consts::ARCH {
        a @ ("x86_64" | "aarch64") => a,
        other => anyhow::bail!("Firecracker has no build for {other}"),
    };
    let http = crate::http::client::Client::public()?;
    let latest: serde_json::Value = http.get("https://api.github.com/repos/firecracker-microvm/firecracker/releases/latest").timeout(Duration::from_secs(60)).send().await?.json(4 << 20).await?;
    let tag = latest.get("tag_name").and_then(serde_json::Value::as_str).ok_or_else(|| anyhow::anyhow!("Firecracker's latest release has no tag"))?.to_owned();
    let file = format!("firecracker-{tag}-{arch}.tgz");
    let base = format!("https://github.com/firecracker-microvm/firecracker/releases/download/{tag}");
    let sums = String::from_utf8_lossy(&download(&http, &format!("{base}/{file}.sha256.txt")).await?).into_owned();
    let want = sums.split_whitespace().next().unwrap_or_default().to_ascii_lowercase();
    let tgz = download(&http, &format!("{base}/{file}")).await?;
    let got = crate::util::sha256_hex(&tgz);
    anyhow::ensure!(!want.is_empty() && got == want, "{file}: SHA-256 {got}, its release says {want}");
    let (to, tag2) = (to.to_path_buf(), tag.clone());
    crate::rt::blocking(move || -> anyhow::Result<()> {
        std::fs::create_dir_all(&to)?;
        let mut ar = tar::Archive::new(flate2::read::GzDecoder::new(tgz.as_slice()));
        let mut found = 0;
        for entry in ar.entries()? {
            let mut entry = entry?;
            let name = entry.path()?.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let Some(which) = ["firecracker", "jailer"].into_iter().find(|w| name == format!("{w}-{tag2}-{arch}")) else { continue };
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes)?;
            let tmp = to.join(format!(".{which}.partial"));
            std::fs::write(&tmp, &bytes)?;
            #[cfg(unix)]
            std::fs::set_permissions(&tmp, std::os::unix::fs::PermissionsExt::from_mode(0o755))?;
            std::fs::rename(&tmp, to.join(which))?;
            found += 1;
        }
        anyhow::ensure!(found == 2, "the release didn't hold both firecracker and jailer");
        Ok(())
    })
    .await??;
    Ok(tag)
}

/// `kiln runner setup <executor>`.
pub async fn cli(a: RunnerArgs) -> anyhow::Result<()> {
    let RunnerCmd::Setup(s) = a.cmd;
    let bin = s.bin_dir.clone().unwrap_or_else(|| crate::os::home_dir().join(".kiln").join("bin"));
    let m = crate::rt::blocking({
        let bin = bin.clone();
        move || look(&bin)
    })
    .await?;
    let steps = plan(&s.executor, &m, &bin);
    let mut waiting = false;
    for step in steps {
        match step {
            Step::Ok(t) => println!("✓ {t}"),
            Step::Do(t) => {
                println!("! {t}");
                waiting = true;
            }
            Step::Run { why, root, program, args } => {
                let shown = format!("{}{program} {}", if root { "sudo " } else { "" }, args.join(" "));
                println!("→ {why}:\n    {shown}");
                if s.dry_run {
                    continue;
                }
                if !s.yes && !ask("  Run it?") {
                    println!("  Skipped.");
                    waiting = true;
                    continue;
                }
                let status = if root { Command::new("sudo").arg(&program).args(&args).status() } else { Command::new(&program).args(&args).status() }.with_context(|| format!("running {shown}"))?;
                anyhow::ensure!(status.success(), "{shown} failed ({status})");
                println!("  Done.");
            }
            Step::Fetch { to } => {
                println!("→ Firecracker and its jailer, from their latest release (checked against its SHA-256), into {}", to.display());
                if s.dry_run {
                    continue;
                }
                let tag = fetch_firecracker(&to).await?;
                println!("  Firecracker {tag} is at {}.", to.join("firecracker").display());
            }
            Step::FetchKernel { to } => {
                println!("→ A guest kernel for its microVMs, Firecracker's own (checked against kiln's SHA-256 for it), to {}", to.display());
                if s.dry_run {
                    continue;
                }
                let name = fetch_kernel(&to).await?;
                println!("  {name} is at {}.", to.display());
            }
        }
    }
    if s.dry_run {
        println!("(a dry run: nothing was done)");
    } else if waiting {
        println!("Some steps are a person's: run this again once they're done.");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w13_r10_setup_says_each_step() {
        let bin = Path::new("/home/ada/.kiln/bin");
        let base = Machine { user: "ada".into(), linux: true, ..Machine::default() };
        // Docker: none, one that answers, one this user can't reach yet (the group), and Podman's.
        assert!(matches!(plan("docker", &base, bin).as_slice(), [Step::Do(t)] if t.contains("install Docker Engine")));
        let answers = Machine { engine: Some(("/usr/bin/docker".into(), true)), ..base.clone() };
        assert_eq!(plan("docker", &answers, bin), [Step::Ok("/usr/bin/docker answers for ada.".into())]);
        let locked = Machine { engine: Some(("/usr/bin/docker".into(), false)), ..base.clone() };
        assert!(matches!(plan("docker", &locked, bin).as_slice(), [Step::Run { root: true, program, args, .. }] if program == "usermod" && args == &["-aG", "docker", "ada"]));
        let grouped = Machine { groups: vec!["docker".into()], ..locked.clone() };
        assert!(matches!(plan("docker", &grouped, bin).as_slice(), [Step::Do(t)] if t.contains("daemon running")));
        // Firecracker: no KVM stops it; KVM not ours yet is the group; then the fetches and
        // e2fsprogs, then its networking said.
        assert!(matches!(plan("firecracker", &base, bin).as_slice(), [Step::Do(t)] if t.contains("/dev/kvm")));
        let kvm = Machine { kvm: Some(false), ..base.clone() };
        let steps = plan("firecracker", &kvm, bin);
        assert!(matches!(steps.first(), Some(Step::Run { args, .. }) if args == &["-aG", "kvm", "ada"]), "{steps:?}");
        assert_eq!(steps.get(1), Some(&Step::Fetch { to: bin.to_path_buf() }));
        assert_eq!(steps.get(2), Some(&Step::FetchKernel { to: kernel_path() }));
        assert!(matches!(steps.get(3), Some(Step::Do(t)) if t.contains("e2fsprogs")), "{steps:?}");
        assert!(matches!(steps.last(), Some(Step::Ok(t)) if t.contains("W13-R05")), "{steps:?}");
        let ready = Machine { kvm: Some(true), firecracker: Some(bin.join("firecracker")), kernel: Some(bin.join("vmlinux")), mkfs: Some("/usr/bin/mke2fs".into()), ..base.clone() };
        assert!(plan("firecracker", &ready, bin).iter().all(|s| matches!(s, Step::Ok(_))));
        // Its guest kernel fetched when there's none, and e2fsprogs asked for.
        let bare = plan("firecracker", &Machine { kernel: None, mkfs: None, ..ready.clone() }, bin);
        assert!(bare.iter().any(|s| matches!(s, Step::FetchKernel { .. })), "{bare:?}");
        assert!(bare.iter().any(|s| matches!(s, Step::Do(t) if t.contains("e2fsprogs"))), "{bare:?}");
        assert!(matches!(plan("firecracker", &Machine { linux: false, ..base.clone() }, bin).as_slice(), [Step::Do(t)] if t.contains("isn't Linux")));
        // shards: found, or a person's to install.
        assert!(matches!(plan("shards", &base, bin).as_slice(), [Step::Do(t)] if t.contains("install it")));
    }
}
